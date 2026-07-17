//! Statement binding: AST statements → [`LogicalPlan`].
//!
//! Everything parsed but not executed must be rejected loudly (`0A000`):
//! silently dropping a clause would return wrong results instead of an honest
//! error.

use std::sync::Arc;

use crabgresql_parser::ast;
use crabgresql_protocol::sqlstate;
use crabgresql_storage_api::{TableAm, TableEngine};
use crabgresql_types::Value;

use crate::expr::{
    BoundExpr, Scope, bind_expr, bind_scalar, coerce_to_column, normalize_ident, output_name,
    to_bool_operand,
};
use crate::{BindError, OutputColumn};

/// A bound statement: names resolved, expressions typed, clauses vetted.
/// Carries the opened `TableAm` so later stages never re-resolve the name.
/// One ORDER BY key: an output-column index and its direction. NULLs order
/// last for ASC, first for DESC (PG defaults).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortKey {
    pub column: usize,
    pub asc: bool,
    pub nulls_first: bool,
}

pub enum LogicalPlan {
    /// FROM-less SELECT: one constant row. The predicate (`SELECT 1 WHERE
    /// false`) contains no column references — it bound in the empty scope.
    Values {
        columns: Vec<OutputColumn>,
        rows: Vec<Vec<BoundExpr>>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
    },
    /// Single-table SELECT with optional predicate.
    Query {
        table: Arc<dyn TableAm>,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
    },
    /// INSERT ... VALUES: rows are full-width in schema order, each cell
    /// already coerced to its column type.
    Insert {
        table: Arc<dyn TableAm>,
        rows: Vec<Vec<BoundExpr>>,
    },
    Update {
        table: Arc<dyn TableAm>,
        predicate: Option<BoundExpr>,
        /// (column index, value expression bound against the OLD row).
        assignments: Vec<(usize, BoundExpr)>,
    },
    Delete {
        table: Arc<dyn TableAm>,
        predicate: Option<BoundExpr>,
    },
}

fn object_name_to_table_name(name: &ast::ObjectName) -> Result<String, BindError> {
    if name.0.len() != 1 {
        return Err(BindError::feature_not_supported(format!(
            "qualified relation names are not supported yet: {name}"
        )));
    }
    match name.0[0].as_ident() {
        Some(ident) => Ok(normalize_ident(ident)),
        None => Err(BindError::syntax(format!("invalid relation name: {name}"))),
    }
}

/// Resolve a `FROM`/`UPDATE`/`DELETE` target: the table plus the qualifier
/// its columns may be addressed by (the alias when present, as in PG).
fn open_relation(
    engine: &Arc<dyn TableEngine>,
    relation: &ast::TableFactor,
) -> Result<(Arc<dyn TableAm>, String), BindError> {
    let ast::TableFactor::Table { name, alias, .. } = relation else {
        return Err(BindError::feature_not_supported(format!(
            "FROM item is not supported yet: {relation}"
        )));
    };
    let table_name = object_name_to_table_name(name)?;
    let qualifier = match alias {
        None => table_name.clone(),
        Some(alias) => {
            if !alias.columns.is_empty() {
                return Err(BindError::feature_not_supported(
                    "column aliases in FROM are not supported yet",
                ));
            }
            normalize_ident(&alias.name)
        }
    };
    let table = engine.open_table(&table_name)?;
    Ok((table, qualifier))
}

fn bind_where(
    selection: &Option<ast::Expr>,
    scope: &Scope,
) -> Result<Option<BoundExpr>, BindError> {
    selection
        .as_ref()
        .map(|expr| to_bool_operand(bind_expr(expr, scope)?, "WHERE"))
        .transpose()
}

pub fn bind_query(
    engine: &Arc<dyn TableEngine>,
    query: &ast::Query,
) -> Result<LogicalPlan, BindError> {
    reject_unsupported_query_clauses(query)?;
    let select = match query.body.as_ref() {
        ast::SetExpr::Select(select) => select,
        other => {
            return Err(BindError::feature_not_supported(format!(
                "query form is not supported yet: {other}"
            )));
        }
    };
    reject_unsupported_select_clauses(select)?;

    match select.from.as_slice() {
        [] => bind_values_select(select, &query.order_by),
        [table] => {
            if !table.joins.is_empty() {
                return Err(BindError::feature_not_supported(
                    "JOIN is not supported yet",
                ));
            }
            bind_table_select(engine, &table.relation, select, &query.order_by)
        }
        _ => Err(BindError::feature_not_supported(
            "multiple FROM items are not supported yet",
        )),
    }
}

/// Bind an ORDER BY clause against the already-built output columns. Only
/// integer ordinals are supported (`ORDER BY 1`); other forms stay `0A000`.
fn bind_order_by(
    order_by: &Option<ast::OrderBy>,
    columns: &[OutputColumn],
) -> Result<Vec<SortKey>, BindError> {
    let Some(order_by) = order_by else {
        return Ok(Vec::new());
    };
    let exprs = match &order_by.kind {
        ast::OrderByKind::Expressions(exprs) => exprs,
        ast::OrderByKind::All(_) => {
            return Err(BindError::feature_not_supported(
                "ORDER BY ALL is not supported yet",
            ));
        }
    };
    let mut keys = Vec::with_capacity(exprs.len());
    for oe in exprs {
        let ordinal = match &oe.expr {
            ast::Expr::Value(v) => match &v.value {
                ast::Value::Number(n, _) => n.parse::<usize>().ok(),
                _ => None,
            },
            _ => None,
        };
        let Some(ordinal) = ordinal else {
            return Err(BindError::feature_not_supported(
                "ORDER BY expressions are not supported yet (only column ordinals)",
            ));
        };
        if ordinal < 1 || ordinal > columns.len() {
            return Err(BindError::new(
                "42P10",
                format!("ORDER BY position {ordinal} is not in select list"),
            ));
        }
        let asc = oe.options.asc.unwrap_or(true);
        let nulls_first = oe.options.nulls_first.unwrap_or(!asc);
        keys.push(SortKey {
            column: ordinal - 1,
            asc,
            nulls_first,
        });
    }
    Ok(keys)
}

fn reject_unsupported_query_clauses(query: &ast::Query) -> Result<(), BindError> {
    let unsupported: Option<&str> = if query.with.is_some() {
        Some("WITH")
    } else if query.limit_clause.is_some() {
        Some("LIMIT/OFFSET")
    } else if query.fetch.is_some() {
        Some("FETCH")
    } else if !query.locks.is_empty() {
        Some("FOR UPDATE/SHARE")
    } else {
        None
    };
    match unsupported {
        Some(clause) => Err(BindError::feature_not_supported(format!(
            "{clause} is not supported yet"
        ))),
        None => Ok(()),
    }
}

fn reject_unsupported_select_clauses(select: &ast::Select) -> Result<(), BindError> {
    let group_by_present = match &select.group_by {
        ast::GroupByExpr::Expressions(exprs, modifiers) => {
            !exprs.is_empty() || !modifiers.is_empty()
        }
        ast::GroupByExpr::All(_) => true,
    };
    let unsupported: Option<&str> = if select.distinct.is_some() {
        Some("DISTINCT")
    } else if group_by_present {
        Some("GROUP BY")
    } else if select.having.is_some() {
        Some("HAVING")
    } else if !select.named_window.is_empty() {
        Some("WINDOW")
    } else if select.qualify.is_some() {
        Some("QUALIFY")
    } else if select.into.is_some() {
        Some("SELECT INTO")
    } else {
        None
    };
    match unsupported {
        Some(clause) => Err(BindError::feature_not_supported(format!(
            "{clause} is not supported yet"
        ))),
        None => Ok(()),
    }
}

/// FROM-less SELECT: one row of constant expressions. A WHERE is still legal
/// (`SELECT 1 WHERE false`) and binds in the empty scope.
fn bind_values_select(
    select: &ast::Select,
    order_by: &Option<ast::OrderBy>,
) -> Result<LogicalPlan, BindError> {
    let scope = Scope::empty();
    let mut columns = Vec::new();
    let mut row = Vec::new();
    for item in &select.projection {
        let SelectField::Expr(expr, alias) = classify_select_item(item)? else {
            return Err(BindError::syntax(
                "SELECT * with no tables specified is not valid",
            ));
        };
        let bound = bind_scalar(expr, &scope)?;
        columns.push(OutputColumn {
            name: alias.unwrap_or_else(|| output_name(expr)),
            ty: bound.ty(),
        });
        row.push(bound);
    }
    let predicate = bind_where(&select.selection, &scope)?;
    let sort = bind_order_by(order_by, &columns)?;
    Ok(LogicalPlan::Values {
        columns,
        rows: vec![row],
        predicate,
        sort,
    })
}

fn bind_table_select(
    engine: &Arc<dyn TableEngine>,
    relation: &ast::TableFactor,
    select: &ast::Select,
    order_by: &Option<ast::OrderBy>,
) -> Result<LogicalPlan, BindError> {
    let (table, qualifier) = open_relation(engine, relation)?;
    let schema = table.schema().clone();
    let scope = Scope::table(&schema, qualifier.clone());

    let mut columns = Vec::new();
    let mut projections = Vec::new();
    for item in &select.projection {
        match classify_select_item(item)? {
            SelectField::Wildcard => {
                expand_all(&schema, &mut columns, &mut projections);
            }
            SelectField::QualifiedWildcard(q) => {
                // `f.*` is only valid for the table's qualifier.
                if q != qualifier {
                    return Err(BindError::new(
                        sqlstate::UNDEFINED_TABLE,
                        format!("missing FROM-clause entry for table \"{q}\""),
                    ));
                }
                expand_all(&schema, &mut columns, &mut projections);
            }
            SelectField::Expr(expr, alias) => {
                let bound = bind_scalar(expr, &scope)?;
                columns.push(OutputColumn {
                    name: alias.unwrap_or_else(|| output_name(expr)),
                    ty: bound.ty(),
                });
                projections.push(bound);
            }
        }
    }

    let predicate = bind_where(&select.selection, &scope)?;
    let sort = bind_order_by(order_by, &columns)?;
    Ok(LogicalPlan::Query {
        table,
        columns,
        projections,
        predicate,
        sort,
    })
}

/// A projection list item after classification.
enum SelectField<'a> {
    Wildcard,
    QualifiedWildcard(String),
    Expr(&'a ast::Expr, Option<String>),
}

fn classify_select_item(item: &ast::SelectItem) -> Result<SelectField<'_>, BindError> {
    match item {
        ast::SelectItem::Wildcard(_) => Ok(SelectField::Wildcard),
        ast::SelectItem::UnnamedExpr(expr) => Ok(SelectField::Expr(expr, None)),
        ast::SelectItem::ExprWithAlias { expr, alias } => {
            Ok(SelectField::Expr(expr, Some(normalize_ident(alias))))
        }
        ast::SelectItem::QualifiedWildcard(kind, _) => match kind {
            ast::SelectItemQualifiedWildcardKind::ObjectName(name) => {
                Ok(SelectField::QualifiedWildcard(object_name_to_table_name(
                    name,
                )?))
            }
            ast::SelectItemQualifiedWildcardKind::Expr(_) => Err(
                BindError::feature_not_supported("qualified * on an expression is not supported yet"),
            ),
        },
        ast::SelectItem::ExprWithAliases { .. } => Err(BindError::feature_not_supported(
            "multiple aliases are not supported yet",
        )),
    }
}

/// Append every column of `schema` as a `*` / `f.*` expansion.
fn expand_all(
    schema: &crabgresql_storage_api::TableSchema,
    columns: &mut Vec<OutputColumn>,
    projections: &mut Vec<BoundExpr>,
) {
    for (index, col) in schema.columns.iter().enumerate() {
        columns.push(OutputColumn {
            name: col.name.clone(),
            ty: col.ty,
        });
        projections.push(BoundExpr::ColumnRef { index, ty: col.ty });
    }
}

pub fn bind_insert(
    engine: &Arc<dyn TableEngine>,
    insert: &ast::Insert,
) -> Result<LogicalPlan, BindError> {
    let name = match &insert.table {
        ast::TableObject::TableName(name) => object_name_to_table_name(name)?,
        other => {
            return Err(BindError::feature_not_supported(format!(
                "INSERT target is not supported yet: {other}"
            )));
        }
    };
    if insert.returning.is_some() {
        return Err(BindError::feature_not_supported(
            "RETURNING is not supported yet",
        ));
    }
    if insert.on.is_some() {
        return Err(BindError::feature_not_supported(
            "ON CONFLICT is not supported yet",
        ));
    }
    let table = engine.open_table(&name)?;
    let schema = table.schema().clone();

    // Map the column list (or its absence) to positions in the table schema.
    let explicit_columns = !insert.columns.is_empty();
    let target_indices: Vec<usize> = if !explicit_columns {
        (0..schema.columns.len()).collect()
    } else {
        let mut indices = Vec::with_capacity(insert.columns.len());
        for column_name in &insert.columns {
            let col = object_name_to_table_name(column_name)?;
            let idx = schema.column_index(&col).ok_or_else(|| {
                BindError::new(
                    sqlstate::UNDEFINED_COLUMN,
                    format!("column \"{col}\" of relation \"{name}\" does not exist"),
                )
            })?;
            if indices.contains(&idx) {
                return Err(BindError::new(
                    sqlstate::DUPLICATE_COLUMN,
                    format!("column \"{col}\" specified more than once"),
                ));
            }
            indices.push(idx);
        }
        indices
    };

    let source = insert.source.as_deref().ok_or_else(|| {
        BindError::feature_not_supported("INSERT without VALUES is not supported yet")
    })?;
    // The INSERT source is a full query in PG: `VALUES (1),(2) LIMIT 1` is
    // legal and inserts one row. Ignoring these clauses would silently insert
    // the wrong rows, so reject them like any other unexecuted clause. ORDER BY
    // on an INSERT source is not executed here either.
    reject_unsupported_query_clauses(source)?;
    if source.order_by.is_some() {
        return Err(BindError::feature_not_supported(
            "ORDER BY on an INSERT source is not supported yet",
        ));
    }
    let values = match source.body.as_ref() {
        ast::SetExpr::Values(values) => &values.rows,
        other => {
            return Err(BindError::feature_not_supported(format!(
                "INSERT source is not supported yet: {other}"
            )));
        }
    };

    // VALUES cells bind in the empty scope: a column reference in VALUES is
    // an undefined column, as in PG.
    let scope = Scope::empty();
    let mut rows = Vec::with_capacity(values.len());
    for value_row in values {
        // PG validates the VALUES clause shape before matching it against the
        // target columns.
        if value_row.len() != values[0].len() {
            return Err(BindError::syntax(
                "VALUES lists must all be the same length",
            ));
        }
        if value_row.len() > target_indices.len() {
            return Err(BindError::syntax(
                "INSERT has more expressions than target columns",
            ));
        }
        // With an explicit column list PG requires an exact match; without
        // one, missing trailing columns default to NULL.
        if explicit_columns && value_row.len() < target_indices.len() {
            return Err(BindError::syntax(
                "INSERT has more target columns than expressions",
            ));
        }
        let mut row: Vec<BoundExpr> = schema
            .columns
            .iter()
            .map(|col| BoundExpr::Const {
                value: Value::Null,
                ty: col.ty,
            })
            .collect();
        for (expr, &idx) in value_row.iter().zip(&target_indices) {
            let binding = bind_expr(expr, &scope)?;
            row[idx] = coerce_to_column(binding, &schema.columns[idx])?;
        }
        rows.push(row);
    }

    Ok(LogicalPlan::Insert { table, rows })
}

pub fn bind_update(
    engine: &Arc<dyn TableEngine>,
    update: &ast::Update,
) -> Result<LogicalPlan, BindError> {
    let unsupported: Option<&str> = if update.from.is_some() {
        Some("UPDATE ... FROM")
    } else if update.returning.is_some() {
        Some("RETURNING")
    } else if update.or.is_some()
        || update.output.is_some()
        || !update.order_by.is_empty()
        || update.limit.is_some()
    {
        Some("this UPDATE form")
    } else if !update.table.joins.is_empty() {
        Some("JOIN in UPDATE")
    } else {
        None
    };
    if let Some(what) = unsupported {
        return Err(BindError::feature_not_supported(format!(
            "{what} is not supported yet"
        )));
    }

    let (table, qualifier) = open_relation(engine, &update.table.relation)?;
    let schema = table.schema().clone();
    let table_name = schema.name.clone();
    let scope = Scope::table(&schema, qualifier);

    // SET expressions bind against the table schema: they all see the OLD
    // row, so `SET a = b, b = a` swaps.
    let mut assignments: Vec<(usize, BoundExpr)> = Vec::with_capacity(update.assignments.len());
    for assignment in &update.assignments {
        let target = match &assignment.target {
            ast::AssignmentTarget::ColumnName(name) => name,
            ast::AssignmentTarget::Tuple(_) => {
                return Err(BindError::feature_not_supported(
                    "multi-column UPDATE SET is not supported yet",
                ));
            }
        };
        let column = object_name_to_table_name(target)?;
        let idx = schema.column_index(&column).ok_or_else(|| {
            BindError::new(
                sqlstate::UNDEFINED_COLUMN,
                format!("column \"{column}\" of relation \"{table_name}\" does not exist"),
            )
        })?;
        if assignments.iter().any(|(i, _)| *i == idx) {
            return Err(BindError::syntax(format!(
                "multiple assignments to same column \"{column}\""
            )));
        }
        let binding = bind_expr(&assignment.value, &scope)?;
        assignments.push((idx, coerce_to_column(binding, &schema.columns[idx])?));
    }

    let predicate = bind_where(&update.selection, &scope)?;
    Ok(LogicalPlan::Update {
        table,
        predicate,
        assignments,
    })
}

pub fn bind_delete(
    engine: &Arc<dyn TableEngine>,
    delete: &ast::Delete,
) -> Result<LogicalPlan, BindError> {
    let unsupported: Option<&str> = if !delete.tables.is_empty() {
        Some("multi-table DELETE")
    } else if delete.using.is_some() {
        Some("DELETE ... USING")
    } else if delete.returning.is_some() {
        Some("RETURNING")
    } else if delete.output.is_some() || !delete.order_by.is_empty() || delete.limit.is_some() {
        Some("this DELETE form")
    } else {
        None
    };
    if let Some(what) = unsupported {
        return Err(BindError::feature_not_supported(format!(
            "{what} is not supported yet"
        )));
    }

    let (ast::FromTable::WithFromKeyword(from) | ast::FromTable::WithoutKeyword(from)) =
        &delete.from;
    let [target] = from.as_slice() else {
        return Err(BindError::feature_not_supported(
            "multi-table DELETE is not supported yet",
        ));
    };
    if !target.joins.is_empty() {
        return Err(BindError::feature_not_supported(
            "JOIN in DELETE is not supported yet",
        ));
    }

    let (table, qualifier) = open_relation(engine, &target.relation)?;
    let schema = table.schema().clone();
    let scope = Scope::table(&schema, qualifier);
    let predicate = bind_where(&delete.selection, &scope)?;
    Ok(LogicalPlan::Delete { table, predicate })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BinOp;
    use crabgresql_memory_storage::MemoryEngine;
    use crabgresql_storage_api::{Column, TableEngine, TableSchema};
    use crabgresql_types::PgType;

    fn engine_with_table() -> Arc<dyn TableEngine> {
        let engine = MemoryEngine::new();
        engine
            .create_table(TableSchema {
                name: "t".into(),
                columns: vec![
                    Column {
                        name: "id".into(),
                        ty: PgType::Int4,
                    },
                    Column {
                        name: "big".into(),
                        ty: PgType::Int8,
                    },
                    Column {
                        name: "name".into(),
                        ty: PgType::Text,
                    },
                    Column {
                        name: "flag".into(),
                        ty: PgType::Bool,
                    },
                ],
            })
            .unwrap();
        Arc::new(engine)
    }

    fn bind_one(sql: &str) -> Result<LogicalPlan, BindError> {
        let engine = engine_with_table();
        let stmts = crabgresql_parser::parse(sql).unwrap();
        match &stmts[0] {
            ast::Statement::Query(q) => bind_query(&engine, q),
            ast::Statement::Insert(i) => bind_insert(&engine, i),
            ast::Statement::Update(u) => bind_update(&engine, u),
            ast::Statement::Delete(d) => bind_delete(&engine, d),
            other => panic!("unexpected statement: {other}"),
        }
    }

    fn bind_err(sql: &str) -> BindError {
        match bind_one(sql) {
            Err(e) => e,
            Ok(_) => panic!("expected bind error for: {sql}"),
        }
    }

    #[test]
    fn resolves_columns_to_indices() {
        let LogicalPlan::Query { projections, .. } = bind_one("SELECT name, id FROM t").unwrap()
        else {
            panic!("expected Query");
        };
        assert_eq!(
            projections,
            vec![
                BoundExpr::ColumnRef {
                    index: 2,
                    ty: PgType::Text
                },
                BoundExpr::ColumnRef {
                    index: 0,
                    ty: PgType::Int4
                },
            ]
        );
    }

    #[test]
    fn unknown_column_is_42703() {
        let e = bind_err("SELECT nope FROM t");
        assert_eq!(e.code, "42703");
        assert_eq!(e.message, "column \"nope\" does not exist");
    }

    #[test]
    fn qualified_column_uses_table_name_or_alias() {
        assert!(bind_one("SELECT t.id FROM t").is_ok());
        assert!(bind_one("SELECT x.id FROM t AS x").is_ok());
        // With an alias the bare table name is no longer a valid qualifier.
        let e = bind_err("SELECT t.id FROM t AS x");
        assert_eq!(e.code, "42P01");
        assert_eq!(e.message, "missing FROM-clause entry for table \"t\"");
    }

    #[test]
    fn where_must_be_boolean() {
        let e = bind_err("SELECT id FROM t WHERE 1");
        assert_eq!(e.code, "42804");
        assert_eq!(
            e.message,
            "argument of WHERE must be type boolean, not type integer"
        );
    }

    #[test]
    fn int4_int8_comparison_promotes_via_coerce() {
        let LogicalPlan::Query { predicate, .. } =
            bind_one("SELECT id FROM t WHERE id = big").unwrap()
        else {
            panic!("expected Query");
        };
        let Some(BoundExpr::Binary {
            op: BinOp::Eq,
            arg_ty: PgType::Int8,
            left,
            ..
        }) = predicate
        else {
            panic!("expected int8 equality");
        };
        assert_eq!(
            *left,
            BoundExpr::Coerce {
                expr: Box::new(BoundExpr::ColumnRef {
                    index: 0,
                    ty: PgType::Int4
                }),
                ty: PgType::Int8,
            }
        );
    }

    #[test]
    fn unknown_literal_takes_type_from_other_side() {
        let LogicalPlan::Query { predicate, .. } =
            bind_one("SELECT id FROM t WHERE big = '5'").unwrap()
        else {
            panic!("expected Query");
        };
        let Some(BoundExpr::Binary { arg_ty, right, .. }) = predicate else {
            panic!("expected comparison");
        };
        assert_eq!(arg_ty, PgType::Int8);
        assert_eq!(
            *right,
            BoundExpr::Const {
                value: Value::Int8(5),
                ty: PgType::Int8
            }
        );
    }

    #[test]
    fn unparsable_unknown_literal_is_22p02() {
        let e = bind_err("SELECT id FROM t WHERE id = 'abc'");
        assert_eq!(e.code, "22P02");
        assert_eq!(e.message, "invalid input syntax for type integer: \"abc\"");
    }

    #[test]
    fn unknown_vs_unknown_comparison_falls_back_to_text() {
        let LogicalPlan::Values { rows, .. } = bind_one("SELECT 'a' = 'b'").unwrap() else {
            panic!("expected Values");
        };
        assert_eq!(rows.len(), 1);
        let BoundExpr::Binary { arg_ty, .. } = &rows[0][0] else {
            panic!("expected comparison");
        };
        assert_eq!(*arg_ty, PgType::Text);
    }

    #[test]
    fn unknown_arithmetic_is_ambiguous_42725() {
        let e = bind_err("SELECT '1' + '2'");
        assert_eq!(e.code, "42725");
        assert_eq!(e.message, "operator is not unique: unknown + unknown");
    }

    #[test]
    fn mismatched_operator_is_42883() {
        let e = bind_err("SELECT id FROM t WHERE name = id");
        assert_eq!(e.code, "42883");
        assert_eq!(e.message, "operator does not exist: text = integer");

        let e = bind_err("SELECT name + name FROM t");
        assert_eq!(e.code, "42883");
        assert_eq!(e.message, "operator does not exist: text + text");
    }

    #[test]
    fn logic_operands_must_be_boolean() {
        let e = bind_err("SELECT flag AND id FROM t");
        assert_eq!(e.code, "42804");
        assert_eq!(
            e.message,
            "argument of AND must be type boolean, not type integer"
        );
    }

    #[test]
    fn min_int4_literal_binds_as_int4() {
        let LogicalPlan::Values { rows, columns, .. } = bind_one("SELECT -2147483648").unwrap()
        else {
            panic!("expected Values");
        };
        assert_eq!(
            rows[0][0],
            BoundExpr::Const {
                value: Value::Int4(i32::MIN),
                ty: PgType::Int4
            }
        );
        assert_eq!(columns[0].ty, PgType::Int4);
    }

    #[test]
    fn output_column_names_follow_pg() {
        let LogicalPlan::Query { columns, .. } =
            bind_one("SELECT id, (name), id + 1 AS next, id + 1, true FROM t").unwrap()
        else {
            panic!("expected Query");
        };
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "name", "next", "?column?", "bool"]);
    }

    #[test]
    fn insert_coerces_cells_to_column_types() {
        let LogicalPlan::Insert { rows, .. } =
            bind_one("INSERT INTO t (id, name) VALUES ('7', 'x')").unwrap()
        else {
            panic!("expected Insert");
        };
        // Full-width row in schema order, missing columns padded with NULL.
        assert_eq!(rows[0].len(), 4);
        assert_eq!(
            rows[0][0],
            BoundExpr::Const {
                value: Value::Int4(7),
                ty: PgType::Int4
            }
        );
        assert_eq!(
            rows[0][1],
            BoundExpr::Const {
                value: Value::Null,
                ty: PgType::Int8
            }
        );
    }

    #[test]
    fn insert_type_mismatch_is_42804_with_column_context() {
        let e = bind_err("INSERT INTO t (name) VALUES (1)");
        assert_eq!(e.code, "42804");
        assert_eq!(
            e.message,
            "column \"name\" is of type text but expression is of type integer"
        );
    }

    #[test]
    fn insert_column_refs_in_values_are_undefined() {
        let e = bind_err("INSERT INTO t (id) VALUES (id)");
        assert_eq!(e.code, "42703");
    }

    #[test]
    fn update_binds_assignments_by_index() {
        let LogicalPlan::Update {
            assignments,
            predicate,
            ..
        } = bind_one("UPDATE t SET name = 'x', id = id + 1 WHERE flag").unwrap()
        else {
            panic!("expected Update");
        };
        assert_eq!(assignments.len(), 2);
        assert_eq!(assignments[0].0, 2);
        assert_eq!(assignments[1].0, 0);
        assert!(predicate.is_some());
    }

    #[test]
    fn update_duplicate_assignment_is_42601() {
        let e = bind_err("UPDATE t SET id = 1, id = 2");
        assert_eq!(e.code, "42601");
        assert_eq!(e.message, "multiple assignments to same column \"id\"");
    }

    #[test]
    fn update_unknown_column_names_the_relation() {
        let e = bind_err("UPDATE t SET nope = 1");
        assert_eq!(e.code, "42703");
        assert_eq!(
            e.message,
            "column \"nope\" of relation \"t\" does not exist"
        );
    }

    #[test]
    fn update_assignment_coerces_to_column_type() {
        let LogicalPlan::Update { assignments, .. } = bind_one("UPDATE t SET id = big").unwrap()
        else {
            panic!("expected Update");
        };
        assert_eq!(
            assignments[0].1,
            BoundExpr::Coerce {
                expr: Box::new(BoundExpr::ColumnRef {
                    index: 1,
                    ty: PgType::Int8
                }),
                ty: PgType::Int4,
            }
        );
    }

    #[test]
    fn delete_binds_predicate() {
        let LogicalPlan::Delete { predicate, .. } = bind_one("DELETE FROM t WHERE id = 1").unwrap()
        else {
            panic!("expected Delete");
        };
        assert!(predicate.is_some());
        let LogicalPlan::Delete { predicate, .. } = bind_one("DELETE FROM t").unwrap() else {
            panic!("expected Delete");
        };
        assert!(predicate.is_none());
    }

    #[test]
    fn unsupported_forms_stay_0a000() {
        for sql in [
            "SELECT id FROM t ORDER BY id",
            "SELECT id FROM t LIMIT 1",
            "UPDATE t SET (id, name) = (1, 'x')",
            "UPDATE t SET id = 1 RETURNING id",
            "DELETE FROM t USING t AS u",
            "DELETE FROM t RETURNING id",
            "INSERT INTO t (id) VALUES (1) RETURNING id",
            // The INSERT source is a full query in PG: silently dropping its
            // clauses would insert the wrong rows.
            "INSERT INTO t (id) VALUES (1), (2) LIMIT 1",
            "INSERT INTO t (id) VALUES (1), (2) ORDER BY 1",
            // DEFAULT parses as an identifier; it must not bind as a column.
            "INSERT INTO t (id) VALUES (DEFAULT)",
            "UPDATE t SET id = DEFAULT",
        ] {
            let e = bind_err(sql);
            assert_eq!(e.code, "0A000", "for: {sql}");
        }
    }

    #[test]
    fn ragged_values_lists_are_42601() {
        let e = bind_err("INSERT INTO t VALUES (1, 2), (3)");
        assert_eq!(e.code, "42601");
        assert_eq!(e.message, "VALUES lists must all be the same length");
    }

    #[test]
    fn out_of_range_literal_is_22003_not_22p02() {
        let e = bind_err("SELECT id FROM t WHERE id = '3000000000'");
        assert_eq!(e.code, "22003");
        assert_eq!(
            e.message,
            "value \"3000000000\" is out of range for type integer"
        );
        // Malformed input keeps 22P02.
        let e = bind_err("SELECT id FROM t WHERE id = '30x'");
        assert_eq!(e.code, "22P02");
    }

    #[test]
    fn constant_assignment_range_checks_at_bind_time() {
        // PG const-folds the cast during planning: the error fires even when
        // no row would match.
        let e = bind_err("UPDATE t SET id = 2147483648");
        assert_eq!(e.code, "22003");
        assert_eq!(e.message, "integer out of range");
    }

    #[test]
    fn bool_literals_accept_pg_prefixes() {
        for (sql, expected) in [
            ("UPDATE t SET flag = 'tru'", Value::Bool(true)),
            ("UPDATE t SET flag = 'of'", Value::Bool(false)),
            ("UPDATE t SET flag = 'ye'", Value::Bool(true)),
            ("UPDATE t SET flag = 'N'", Value::Bool(false)),
        ] {
            let LogicalPlan::Update { assignments, .. } = bind_one(sql).unwrap() else {
                panic!("expected Update for: {sql}");
            };
            assert_eq!(
                assignments[0].1,
                BoundExpr::Const {
                    value: expected,
                    ty: PgType::Bool
                },
                "{sql}"
            );
        }
        // A bare "o" is ambiguous between on and off.
        let e = bind_err("UPDATE t SET flag = 'o'");
        assert_eq!(e.code, "22P02");
    }

    #[test]
    fn arithmetic_on_non_numeric_with_unknown_is_42883() {
        let e = bind_err("SELECT flag + 'x' FROM t");
        assert_eq!(e.code, "42883");
        assert_eq!(e.message, "operator does not exist: boolean + unknown");
        let e = bind_err("SELECT 'x' + name FROM t");
        assert_eq!(e.code, "42883");
        assert_eq!(e.message, "operator does not exist: unknown + text");
    }

    #[test]
    fn decimal_literal_binds_as_float8() {
        let LogicalPlan::Values { rows, .. } = bind_one("SELECT 1.5").unwrap() else {
            panic!("expected Values");
        };
        assert_eq!(
            rows[0][0],
            BoundExpr::Const {
                value: Value::Float8(1.5),
                ty: PgType::Float8
            }
        );
    }

    #[test]
    fn float_literal_cast_binds() {
        let LogicalPlan::Values { rows, .. } = bind_one("SELECT 'NaN'::float4").unwrap() else {
            panic!("expected Values");
        };
        let BoundExpr::Const {
            value: Value::Float4(v),
            ty: PgType::Float4,
        } = &rows[0][0]
        else {
            panic!("expected float4 const, got {:?}", rows[0][0]);
        };
        assert!(v.is_nan());
    }

    #[test]
    fn bad_float_literal_carries_position() {
        let e = bind_err("SELECT 'xyz'::float4");
        assert_eq!(e.code, "22P02");
        assert_eq!(e.message, "invalid input syntax for type real: \"xyz\"");
        assert!(e.location.is_some());
    }

    #[test]
    fn float_to_int_cast_overflow_is_22003_without_position() {
        let e = bind_err("SELECT '32767.6'::float4::int2");
        assert_eq!(e.code, "22003");
        assert_eq!(e.message, "smallint out of range");
        assert!(e.location.is_none());
    }

    #[test]
    fn float_out_of_range_literal_has_position() {
        let e = bind_err("SELECT '10e70'::float4");
        assert_eq!(e.code, "22003");
        assert_eq!(e.message, "\"10e70\" is out of range for type real");
        assert!(e.location.is_some());
    }

    #[test]
    fn select_where_without_table_binds_predicate() {
        let LogicalPlan::Values {
            rows, predicate, ..
        } = bind_one("SELECT 1 WHERE 1 = 2").unwrap()
        else {
            panic!("expected Values");
        };
        assert_eq!(rows.len(), 1);
        assert!(predicate.is_some());
    }
}
