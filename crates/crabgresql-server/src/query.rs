//! Simple-query execution: AST → plan → Volcano executor.
//!
//! M0 supports FROM-less SELECT over literals, single-table SELECT without
//! predicates, CREATE TABLE, INSERT ... VALUES and a no-op SET. The real
//! binder/planner crates take over from M1.

use std::sync::Arc;

use crabgresql_executor::{ExecNode, OutputColumn, Project, SeqScan, Values};
use crabgresql_parser::ast;
use crabgresql_protocol::sqlstate;
use crabgresql_storage_api::{Column, StorageError, TableAm, TableEngine, TableSchema, Tuple};
use crabgresql_types::{PgType, Value};

use crate::error::PgError;

pub enum QueryResult {
    /// A result set, streamed: the caller pulls tuples from the node and
    /// derives the `SELECT n` tag from the row count.
    Rows {
        columns: Vec<OutputColumn>,
        node: Box<dyn ExecNode>,
    },
    Command {
        tag: String,
    },
}

pub fn execute_statement(
    engine: &Arc<dyn TableEngine>,
    stmt: &ast::Statement,
) -> Result<QueryResult, PgError> {
    match stmt {
        ast::Statement::Query(query) => execute_query(engine, query),
        ast::Statement::CreateTable(create) => execute_create_table(engine, create),
        ast::Statement::Insert(insert) => execute_insert(engine, insert),
        // Accepted and ignored for driver compatibility (no GUC store yet).
        ast::Statement::Set(_) => Ok(QueryResult::Command { tag: "SET".into() }),
        other => Err(PgError::feature_not_supported(format!(
            "statement is not supported yet: {}",
            statement_kind(other)
        ))),
    }
}

fn statement_kind(stmt: &ast::Statement) -> String {
    // First word of the SQL rendering, e.g. "DROP" or "UPDATE".
    stmt.to_string()
        .split_whitespace()
        .next()
        .unwrap_or("?")
        .to_string()
}

/// Unquoted identifiers fold to lowercase, as in PG.
fn normalize_ident(ident: &ast::Ident) -> String {
    match ident.quote_style {
        Some(_) => ident.value.clone(),
        None => ident.value.to_lowercase(),
    }
}

fn object_name_to_table_name(name: &ast::ObjectName) -> Result<String, PgError> {
    if name.0.len() != 1 {
        return Err(PgError::feature_not_supported(format!(
            "qualified relation names are not supported yet: {name}"
        )));
    }
    match name.0[0].as_ident() {
        Some(ident) => Ok(normalize_ident(ident)),
        None => Err(PgError::syntax(format!("invalid relation name: {name}"))),
    }
}

fn execute_query(
    engine: &Arc<dyn TableEngine>,
    query: &ast::Query,
) -> Result<QueryResult, PgError> {
    // Everything parsed but not executed must be rejected loudly: silently
    // dropping a clause would return wrong results instead of an honest error.
    reject_unsupported_query_clauses(query)?;
    let select = match query.body.as_ref() {
        ast::SetExpr::Select(select) => select,
        other => {
            return Err(PgError::feature_not_supported(format!(
                "query form is not supported yet: {other}"
            )));
        }
    };
    reject_unsupported_select_clauses(select)?;

    let (columns, node) = match select.from.as_slice() {
        [] => plan_values(&select.projection)?,
        [table] => {
            if !table.joins.is_empty() {
                return Err(PgError::feature_not_supported("JOIN is not supported yet"));
            }
            plan_scan(engine, table, &select.projection)?
        }
        _ => {
            return Err(PgError::feature_not_supported(
                "multiple FROM items are not supported yet",
            ));
        }
    };
    Ok(QueryResult::Rows { columns, node })
}

fn reject_unsupported_query_clauses(query: &ast::Query) -> Result<(), PgError> {
    let unsupported: Option<&str> = if query.with.is_some() {
        Some("WITH")
    } else if query.order_by.is_some() {
        Some("ORDER BY")
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
        Some(clause) => Err(PgError::feature_not_supported(format!(
            "{clause} is not supported yet"
        ))),
        None => Ok(()),
    }
}

fn reject_unsupported_select_clauses(select: &ast::Select) -> Result<(), PgError> {
    let group_by_present = match &select.group_by {
        ast::GroupByExpr::Expressions(exprs, modifiers) => {
            !exprs.is_empty() || !modifiers.is_empty()
        }
        ast::GroupByExpr::All(_) => true,
    };
    let unsupported: Option<&str> = if select.selection.is_some() {
        Some("WHERE")
    } else if select.distinct.is_some() {
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
        Some(clause) => Err(PgError::feature_not_supported(format!(
            "{clause} is not supported yet"
        ))),
        None => Ok(()),
    }
}

/// FROM-less SELECT: every projection item must be a literal expression.
fn plan_values(
    projection: &[ast::SelectItem],
) -> Result<(Vec<OutputColumn>, Box<dyn ExecNode>), PgError> {
    let mut columns = Vec::new();
    let mut row = Vec::new();
    for item in projection {
        let (expr, alias) = match item {
            ast::SelectItem::UnnamedExpr(expr) => (expr, None),
            ast::SelectItem::ExprWithAlias { expr, alias } => (expr, Some(normalize_ident(alias))),
            ast::SelectItem::Wildcard(_) | ast::SelectItem::QualifiedWildcard(..) => {
                return Err(PgError::syntax(
                    "SELECT * with no tables specified is not valid",
                ));
            }
            ast::SelectItem::ExprWithAliases { .. } => {
                return Err(PgError::feature_not_supported(
                    "multiple aliases are not supported yet",
                ));
            }
        };
        let (value, ty) = eval_literal(expr)?;
        columns.push(OutputColumn {
            name: alias.unwrap_or_else(|| "?column?".into()),
            ty,
        });
        row.push(value);
    }
    Ok((columns, Box::new(Values::new(vec![row]))))
}

/// Single-table scan with projection by column name or `*`.
fn plan_scan(
    engine: &Arc<dyn TableEngine>,
    table: &ast::TableWithJoins,
    projection: &[ast::SelectItem],
) -> Result<(Vec<OutputColumn>, Box<dyn ExecNode>), PgError> {
    let name = match &table.relation {
        ast::TableFactor::Table { name, .. } => object_name_to_table_name(name)?,
        other => {
            return Err(PgError::feature_not_supported(format!(
                "FROM item is not supported yet: {other}"
            )));
        }
    };
    let table: Arc<dyn TableAm> = engine.open_table(&name)?;
    let schema = table.schema().clone();

    let mut columns = Vec::new();
    let mut indices = Vec::new();
    for item in projection {
        match item {
            ast::SelectItem::Wildcard(_) => {
                for (i, col) in schema.columns.iter().enumerate() {
                    columns.push(OutputColumn {
                        name: col.name.clone(),
                        ty: col.ty,
                    });
                    indices.push(i);
                }
            }
            ast::SelectItem::UnnamedExpr(expr) | ast::SelectItem::ExprWithAlias { expr, .. } => {
                let column_name = match expr {
                    ast::Expr::Identifier(ident) => normalize_ident(ident),
                    other => {
                        return Err(PgError::feature_not_supported(format!(
                            "expression is not supported yet: {other}"
                        )));
                    }
                };
                let idx = schema.column_index(&column_name).ok_or_else(|| {
                    PgError::new(
                        sqlstate::UNDEFINED_COLUMN,
                        format!("column \"{column_name}\" does not exist"),
                    )
                })?;
                let name = match item {
                    ast::SelectItem::ExprWithAlias { alias, .. } => normalize_ident(alias),
                    _ => column_name,
                };
                columns.push(OutputColumn {
                    name,
                    ty: schema.columns[idx].ty,
                });
                indices.push(idx);
            }
            ast::SelectItem::QualifiedWildcard(..) => {
                return Err(PgError::feature_not_supported(
                    "qualified * is not supported yet",
                ));
            }
            ast::SelectItem::ExprWithAliases { .. } => {
                return Err(PgError::feature_not_supported(
                    "multiple aliases are not supported yet",
                ));
            }
        }
    }

    let scan = Box::new(SeqScan::new(&table));
    Ok((columns, Box::new(Project::new(scan, indices))))
}

fn execute_create_table(
    engine: &Arc<dyn TableEngine>,
    create: &ast::CreateTable,
) -> Result<QueryResult, PgError> {
    let name = object_name_to_table_name(&create.name)?;
    if let Some(constraint) = create.constraints.first() {
        return Err(PgError::feature_not_supported(format!(
            "table constraints are not supported yet: {constraint}"
        )));
    }
    let mut columns = Vec::new();
    for col in &create.columns {
        // Constraints we can't enforce must not be accepted: a silently
        // dropped NOT NULL / PRIMARY KEY would let invalid data in.
        if let Some(option) = col.options.first() {
            return Err(PgError::feature_not_supported(format!(
                "column constraints are not supported yet: {}",
                option.option
            )));
        }
        columns.push(Column {
            name: normalize_ident(&col.name),
            ty: map_data_type(&col.data_type)?,
        });
    }
    match engine.create_table(TableSchema { name, columns }) {
        Ok(_) => {}
        // PG succeeds with a notice; NoticeResponse itself is still todo.
        Err(StorageError::TableAlreadyExists(_)) if create.if_not_exists => {}
        Err(e) => return Err(e.into()),
    }
    Ok(QueryResult::Command {
        tag: "CREATE TABLE".into(),
    })
}

fn map_data_type(dt: &ast::DataType) -> Result<PgType, PgError> {
    use ast::DataType;
    Ok(match dt {
        DataType::Bool | DataType::Boolean => PgType::Bool,
        DataType::Int(_) | DataType::Integer(_) | DataType::Int4(_) => PgType::Int4,
        DataType::BigInt(_) | DataType::Int8(_) => PgType::Int8,
        DataType::Text | DataType::Varchar(None) | DataType::CharacterVarying(None) => PgType::Text,
        // Accepting varchar(n) without enforcing the length (22001) would
        // silently store over-long values; reject until typmod exists.
        DataType::Varchar(Some(_)) | DataType::CharacterVarying(Some(_)) => {
            return Err(PgError::feature_not_supported(
                "varchar length limits are not supported yet",
            ));
        }
        other => {
            return Err(PgError::feature_not_supported(format!(
                "type \"{other}\" is not supported yet"
            )));
        }
    })
}

fn execute_insert(
    engine: &Arc<dyn TableEngine>,
    insert: &ast::Insert,
) -> Result<QueryResult, PgError> {
    let name = match &insert.table {
        ast::TableObject::TableName(name) => object_name_to_table_name(name)?,
        other => {
            return Err(PgError::feature_not_supported(format!(
                "INSERT target is not supported yet: {other}"
            )));
        }
    };
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
                PgError::new(
                    sqlstate::UNDEFINED_COLUMN,
                    format!("column \"{col}\" of relation \"{name}\" does not exist"),
                )
            })?;
            if indices.contains(&idx) {
                return Err(PgError::new(
                    sqlstate::DUPLICATE_COLUMN,
                    format!("column \"{col}\" specified more than once"),
                ));
            }
            indices.push(idx);
        }
        indices
    };

    let source = insert.source.as_deref().ok_or_else(|| {
        PgError::feature_not_supported("INSERT without VALUES is not supported yet")
    })?;
    let rows = match source.body.as_ref() {
        ast::SetExpr::Values(values) => &values.rows,
        other => {
            return Err(PgError::feature_not_supported(format!(
                "INSERT source is not supported yet: {other}"
            )));
        }
    };

    // Evaluate and coerce every row before touching storage: a failure in a
    // later row must not leave earlier rows behind (statement atomicity).
    let mut tuples: Vec<Tuple> = Vec::with_capacity(rows.len());
    for row in rows {
        if row.len() > target_indices.len() {
            return Err(PgError::syntax(
                "INSERT has more expressions than target columns",
            ));
        }
        // With an explicit column list PG requires an exact match; without
        // one, missing trailing columns default to NULL.
        if explicit_columns && row.len() < target_indices.len() {
            return Err(PgError::syntax(
                "INSERT has more target columns than expressions",
            ));
        }
        let mut tuple: Tuple = vec![Value::Null; schema.columns.len()];
        for (expr, &idx) in row.iter().zip(&target_indices) {
            let (value, _) = eval_literal(expr)?;
            tuple[idx] = coerce(value, schema.columns[idx].ty, &schema.columns[idx].name)?;
        }
        tuples.push(tuple);
    }

    let inserted = tuples.len();
    for tuple in tuples {
        table.insert(tuple);
    }
    Ok(QueryResult::Command {
        tag: format!("INSERT 0 {inserted}"),
    })
}

/// Evaluate a literal expression to a value and its result type. NULL reports
/// text, matching how PG resolves an untyped NULL in a bare SELECT.
fn eval_literal(expr: &ast::Expr) -> Result<(Value, PgType), PgError> {
    match expr {
        ast::Expr::Value(v) => eval_ast_value(&v.value),
        // PG folds `-` into the numeric literal before choosing int4 vs int8.
        ast::Expr::UnaryOp {
            op: ast::UnaryOperator::Minus,
            expr,
        } => match expr.as_ref() {
            ast::Expr::Value(v) => match &v.value {
                ast::Value::Number(n, _) => parse_number(&format!("-{n}")),
                _ => Err(unsupported_expr(expr)),
            },
            _ => Err(unsupported_expr(expr)),
        },
        ast::Expr::UnaryOp {
            op: ast::UnaryOperator::Plus,
            expr,
        } => eval_literal(expr),
        ast::Expr::Nested(inner) => eval_literal(inner),
        _ => Err(unsupported_expr(expr)),
    }
}

fn unsupported_expr(expr: &ast::Expr) -> PgError {
    PgError::feature_not_supported(format!("expression is not supported yet: {expr}"))
}

fn eval_ast_value(value: &ast::Value) -> Result<(Value, PgType), PgError> {
    match value {
        ast::Value::Number(n, _) => parse_number(n),
        ast::Value::SingleQuotedString(s)
        | ast::Value::DollarQuotedString(ast::DollarQuotedString { value: s, .. }) => {
            Ok((Value::Text(s.clone()), PgType::Text))
        }
        ast::Value::Boolean(b) => Ok((Value::Bool(*b), PgType::Bool)),
        ast::Value::Null => Ok((Value::Null, PgType::Text)),
        other => Err(PgError::feature_not_supported(format!(
            "literal is not supported yet: {other}"
        ))),
    }
}

/// Integer literals become int4 when they fit, int8 otherwise — PG semantics.
/// Decimals need `numeric`, which is an M1 type.
fn parse_number(n: &str) -> Result<(Value, PgType), PgError> {
    if let Ok(v) = n.parse::<i32>() {
        return Ok((Value::Int4(v), PgType::Int4));
    }
    if let Ok(v) = n.parse::<i64>() {
        return Ok((Value::Int8(v), PgType::Int8));
    }
    Err(PgError::feature_not_supported(format!(
        "numeric literal \"{n}\" is not supported yet (numeric lands in M1)"
    )))
}

fn coerce(value: Value, target: PgType, column: &str) -> Result<Value, PgError> {
    let mismatch = |found: PgType| {
        PgError::new(
            sqlstate::DATATYPE_MISMATCH,
            format!(
                "column \"{column}\" is of type {} but expression is of type {}",
                target.name(),
                found.name()
            ),
        )
    };
    match (value, target) {
        (Value::Null, _) => Ok(Value::Null),
        (Value::Int4(v), PgType::Int4) => Ok(Value::Int4(v)),
        (Value::Int4(v), PgType::Int8) => Ok(Value::Int8(v as i64)),
        (Value::Int8(v), PgType::Int4) => match i32::try_from(v) {
            Ok(v) => Ok(Value::Int4(v)),
            Err(_) => Err(PgError::new(
                sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
                "integer out of range",
            )),
        },
        (Value::Int8(v), PgType::Int8) => Ok(Value::Int8(v)),
        (Value::Bool(v), PgType::Bool) => Ok(Value::Bool(v)),
        (Value::Text(v), PgType::Text) => Ok(Value::Text(v)),
        // Quoted literals are untyped constants in PG and coerce to the
        // column type by parsing their text representation.
        (Value::Text(v), PgType::Int4) => match v.trim().parse::<i32>() {
            Ok(v) => Ok(Value::Int4(v)),
            Err(_) => Err(invalid_text(&v, target)),
        },
        (Value::Text(v), PgType::Int8) => match v.trim().parse::<i64>() {
            Ok(v) => Ok(Value::Int8(v)),
            Err(_) => Err(invalid_text(&v, target)),
        },
        (Value::Text(v), PgType::Bool) => match parse_bool_text(&v) {
            Some(b) => Ok(Value::Bool(b)),
            None => Err(invalid_text(&v, target)),
        },
        (v, _) => Err(mismatch(v.pg_type().expect("null handled above"))),
    }
}

fn invalid_text(input: &str, target: PgType) -> PgError {
    PgError::new(
        sqlstate::INVALID_TEXT_REPRESENTATION,
        format!(
            "invalid input syntax for type {}: \"{input}\"",
            target.name()
        ),
    )
}

/// The spellings `boolin` accepts, case-insensitively and trimmed.
fn parse_bool_text(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "t" | "true" | "yes" | "y" | "on" | "1" => Some(true),
        "f" | "false" | "no" | "n" | "off" | "0" => Some(false),
        _ => None,
    }
}
