//! Statement binding: AST statements → [`LogicalPlan`].
//!
//! Everything parsed but not executed must be rejected loudly (`0A000`):
//! silently dropping a clause would return wrong results instead of an honest
//! error.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crabgresql_parser::ast;
use crabgresql_protocol::sqlstate;
use crabgresql_storage_api::{Column, TableAm, TableEngine, TableSchema};
use crabgresql_types::Value;

use crate::expr::{
    BoundExpr, Scope, bind_expr, bind_scalar, coerce_to_column, normalize_ident, output_name,
    to_bool_operand, unify_value_column,
};
use crate::functions::{bind_table_fn_call, positional_arg_exprs};
use crate::{BindError, OutputColumn, TableFn};

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

#[derive(Clone)]
pub enum LogicalPlan {
    /// FROM-less SELECT (`SELECT 1`) or a standalone `VALUES` list: one or more
    /// constant rows. A predicate (`SELECT 1 WHERE false`) contains no column
    /// references — it bound in the empty scope.
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
    /// SELECT over a subquery source in FROM: a derived table (`(SELECT ...) s`)
    /// or a CTE reference. `source` produces the input rows; the same
    /// projection/predicate/sort pipeline as `Query` runs on top.
    Subquery {
        source: Box<LogicalPlan>,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
    },
    /// SELECT over a set-returning function in FROM position. The source rows
    /// come from evaluating `func` with `args`; the same projection/predicate/
    /// sort pipeline as `Query` runs on top.
    TableFunction {
        func: TableFn,
        args: Vec<BoundExpr>,
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

/// A resolved CTE: its output columns and the plan that produces its rows.
/// Cloned on each reference (single-FROM keeps this cheap in practice).
#[derive(Clone)]
struct CteRelation {
    columns: Vec<OutputColumn>,
    plan: LogicalPlan,
}

/// The set of CTE names visible while binding a query — the enclosing `WITH`
/// plus any earlier siblings in the same clause.
type CteEnv = HashMap<String, CteRelation>;

pub fn bind_query(
    engine: &Arc<dyn TableEngine>,
    query: &ast::Query,
) -> Result<LogicalPlan, BindError> {
    bind_query_scoped(engine, query, &CteEnv::new())
}

/// Bind a query with a set of visible CTEs. Recurses for CTE bodies and derived
/// tables, extending the environment with this query's own `WITH` clause.
fn bind_query_scoped(
    engine: &Arc<dyn TableEngine>,
    query: &ast::Query,
    outer: &CteEnv,
) -> Result<LogicalPlan, BindError> {
    // Only build (clone) an extended environment when this query has a WITH; the
    // common no-CTE case binds against `outer` directly.
    match &query.with {
        Some(with) => {
            let ctes = bind_ctes(engine, with, outer)?;
            bind_query_body(engine, query, &ctes)
        }
        None => bind_query_body(engine, query, outer),
    }
}

/// Bind a query's body (SELECT or VALUES) against a resolved CTE environment.
fn bind_query_body(
    engine: &Arc<dyn TableEngine>,
    query: &ast::Query,
    ctes: &CteEnv,
) -> Result<LogicalPlan, BindError> {
    reject_unsupported_query_clauses(query)?;
    match query.body.as_ref() {
        ast::SetExpr::Select(select) => {
            reject_unsupported_select_clauses(select)?;
            bind_select(engine, select, &query.order_by, ctes)
        }
        ast::SetExpr::Values(values) => bind_values_query(values, &query.order_by),
        other => Err(BindError::feature_not_supported(format!(
            "query form is not supported yet: {other}"
        ))),
    }
}

/// Bind a `WITH` clause into a fresh environment layered on `outer`. CTEs bind in
/// order, each seeing earlier siblings; recursion and `WITH` on data-modifying
/// bodies are not yet supported. Duplicate names within one clause are rejected.
fn bind_ctes(
    engine: &Arc<dyn TableEngine>,
    with: &ast::With,
    outer: &CteEnv,
) -> Result<CteEnv, BindError> {
    if with.recursive {
        return Err(BindError::feature_not_supported(
            "WITH RECURSIVE is not supported yet",
        ));
    }
    let mut ctes = outer.clone();
    let mut defined = HashSet::new();
    for cte in &with.cte_tables {
        let name = normalize_ident(&cte.alias.name);
        // A name may shadow an outer CTE, but not repeat within this clause.
        if !defined.insert(name.clone()) {
            return Err(BindError::new(
                sqlstate::DUPLICATE_ALIAS,
                format!("WITH query name \"{name}\" specified more than once"),
            ));
        }
        let plan = bind_query_scoped(engine, &cte.query, &ctes)?;
        let mut columns = output_columns_of(&plan)?;
        apply_alias_columns(&mut columns, &cte.alias.columns, &with_query_subject(&name))?;
        ctes.insert(name, CteRelation { columns, plan });
    }
    Ok(ctes)
}

/// Bind the SELECT body (its FROM item, projections, WHERE, ORDER BY).
fn bind_select(
    engine: &Arc<dyn TableEngine>,
    select: &ast::Select,
    order_by: &Option<ast::OrderBy>,
    ctes: &CteEnv,
) -> Result<LogicalPlan, BindError> {
    match select.from.as_slice() {
        [] => bind_values_select(select, order_by),
        [table] => {
            if !table.joins.is_empty() {
                return Err(BindError::feature_not_supported(
                    "JOIN is not supported yet",
                ));
            }
            bind_from_select(engine, &table.relation, select, order_by, ctes)
        }
        _ => Err(BindError::feature_not_supported(
            "multiple FROM items are not supported yet",
        )),
    }
}

/// Dispatch a single FROM item to the right binder: a table function, a CTE
/// reference, a derived table, or a base table.
fn bind_from_select(
    engine: &Arc<dyn TableEngine>,
    relation: &ast::TableFactor,
    select: &ast::Select,
    order_by: &Option<ast::OrderBy>,
    ctes: &CteEnv,
) -> Result<LogicalPlan, BindError> {
    match relation {
        // A `Table` factor carrying call arguments is a set-returning function.
        ast::TableFactor::Table {
            name,
            alias,
            args: Some(fn_args),
            ..
        } => bind_table_function_select(name, fn_args, alias, select, order_by),
        // A bare name may resolve to a CTE (which shadows a real table).
        ast::TableFactor::Table {
            name,
            alias,
            args: None,
            ..
        } => {
            let tname = object_name_to_table_name(name)?;
            if let Some(cte) = ctes.get(&tname) {
                let qualifier = relation_qualifier(alias, &tname);
                let mut columns = cte.columns.clone();
                apply_relation_alias_columns(&mut columns, alias, &table_subject(&qualifier))?;
                return bind_subquery_select(cte.plan.clone(), columns, qualifier, select, order_by);
            }
            bind_table_select(engine, relation, select, order_by)
        }
        ast::TableFactor::Derived {
            subquery, alias, ..
        } => {
            let Some(alias) = alias else {
                return Err(BindError::new(
                    sqlstate::SYNTAX_ERROR,
                    "subquery in FROM must have an alias",
                ));
            };
            let qualifier = normalize_ident(&alias.name);
            let inner = bind_query_scoped(engine, subquery, ctes)?;
            let mut columns = output_columns_of(&inner)?;
            apply_alias_columns(&mut columns, &alias.columns, &table_subject(&qualifier))?;
            bind_subquery_select(inner, columns, qualifier, select, order_by)
        }
        other => Err(BindError::feature_not_supported(format!(
            "FROM item is not supported yet: {other}"
        ))),
    }
}

/// Bind a SELECT whose source is a subquery (a derived table or CTE reference).
/// The source's output columns form a synthetic relation so the outer SELECT
/// binds exactly as it would over a base table.
fn bind_subquery_select(
    source: LogicalPlan,
    source_columns: Vec<OutputColumn>,
    qualifier: String,
    select: &ast::Select,
    order_by: &Option<ast::OrderBy>,
) -> Result<LogicalPlan, BindError> {
    let schema = synthetic_schema(&qualifier, source_columns);
    let body = bind_select_body(select, order_by, &schema, &qualifier)?;
    Ok(LogicalPlan::Subquery {
        source: Box::new(source),
        columns: body.columns,
        projections: body.projections,
        predicate: body.predicate,
        sort: body.sort,
    })
}

/// A standalone `VALUES (...), (...)` list. Column names default to
/// `column1..columnN`; each column resolves to a common type across all rows.
fn bind_values_query(
    values: &ast::Values,
    order_by: &Option<ast::OrderBy>,
) -> Result<LogicalPlan, BindError> {
    if values.rows.is_empty() {
        return Err(BindError::syntax("VALUES lists must not be empty"));
    }
    let width = values.rows[0].content.len();
    let scope = Scope::empty();
    // Bind every cell, grouping bindings by column for type unification.
    let mut columns_of_bindings: Vec<Vec<crate::Binding>> = vec![Vec::new(); width];
    for row in &values.rows {
        if row.content.len() != width {
            return Err(BindError::new(
                sqlstate::SYNTAX_ERROR,
                "VALUES lists must all be the same length",
            ));
        }
        for (col, expr) in row.content.iter().enumerate() {
            columns_of_bindings[col].push(bind_expr(expr, &scope)?);
        }
    }

    let mut columns = Vec::with_capacity(width);
    let mut column_cells: Vec<Vec<BoundExpr>> = Vec::with_capacity(width);
    for (i, bindings) in columns_of_bindings.into_iter().enumerate() {
        let (ty, cells) = unify_value_column(bindings)?;
        columns.push(OutputColumn {
            name: format!("column{}", i + 1),
            ty,
        });
        column_cells.push(cells);
    }
    // Transpose column-major cells back into rows, moving each cell exactly once.
    let nrows = values.rows.len();
    let mut column_iters: Vec<_> = column_cells.into_iter().map(Vec::into_iter).collect();
    let rows: Vec<Vec<BoundExpr>> = (0..nrows)
        .map(|_| {
            column_iters
                .iter_mut()
                .map(|cells| cells.next().expect("each column has one cell per row"))
                .collect()
        })
        .collect();

    let sort = bind_order_by(order_by, &columns)?;
    Ok(LogicalPlan::Values {
        columns,
        rows,
        predicate: None,
        sort,
    })
}

/// The output columns a query plan produces (for CTE/derived-table schemas).
fn output_columns_of(plan: &LogicalPlan) -> Result<Vec<OutputColumn>, BindError> {
    match plan {
        LogicalPlan::Values { columns, .. }
        | LogicalPlan::Query { columns, .. }
        | LogicalPlan::Subquery { columns, .. }
        | LogicalPlan::TableFunction { columns, .. } => Ok(columns.clone()),
        LogicalPlan::Insert { .. } | LogicalPlan::Update { .. } | LogicalPlan::Delete { .. } => {
            Err(BindError::feature_not_supported(
                "data-modifying statements in WITH are not supported yet",
            ))
        }
    }
}

/// The qualifier a FROM item's columns are addressed by: its alias, else its
/// name.
fn relation_qualifier(alias: &Option<ast::TableAlias>, default: &str) -> String {
    match alias {
        None => default.to_string(),
        Some(alias) => normalize_ident(&alias.name),
    }
}

/// Build the single-relation schema a subquery/SRF source exposes for name
/// resolution, from its output columns under `qualifier`.
fn synthetic_schema(qualifier: &str, columns: Vec<OutputColumn>) -> TableSchema {
    TableSchema {
        name: qualifier.to_string(),
        columns: columns
            .into_iter()
            .map(|c| Column {
                name: c.name,
                ty: c.ty,
            })
            .collect(),
    }
}

/// PG's subject phrasing for a column-count-mismatch error: `table "v"` for a
/// derived table / relation reference, `WITH query "t"` for a CTE definition.
fn table_subject(name: &str) -> String {
    format!("table \"{name}\"")
}
fn with_query_subject(name: &str) -> String {
    format!("WITH query \"{name}\"")
}

/// Apply an optional relation alias's column list to a rowset's columns.
fn apply_relation_alias_columns(
    columns: &mut [OutputColumn],
    alias: &Option<ast::TableAlias>,
    subject: &str,
) -> Result<(), BindError> {
    match alias {
        None => Ok(()),
        Some(alias) => apply_alias_columns(columns, &alias.columns, subject),
    }
}

/// Rename a rowset's columns from an alias column list (`t(a, b, c)`); the count
/// must match, and per-column type annotations are not supported. `subject` is
/// the relation phrasing PG uses in the column-count error (see [`table_subject`]
/// / [`with_query_subject`]).
fn apply_alias_columns(
    columns: &mut [OutputColumn],
    alias_columns: &[ast::TableAliasColumnDef],
    subject: &str,
) -> Result<(), BindError> {
    if alias_columns.is_empty() {
        return Ok(());
    }
    if alias_columns.iter().any(|c| c.data_type.is_some()) {
        return Err(BindError::feature_not_supported(
            "column type annotations in a table alias are not supported yet",
        ));
    }
    if alias_columns.len() != columns.len() {
        // Matches PG's ERRCODE_INVALID_COLUMN_REFERENCE (42P10) and wording, e.g.
        // `table "v" has 1 columns available but 2 columns specified`.
        return Err(BindError::new(
            sqlstate::INVALID_COLUMN_REFERENCE,
            format!(
                "{subject} has {} columns available but {} columns specified",
                columns.len(),
                alias_columns.len()
            ),
        ));
    }
    for (col, def) in columns.iter_mut().zip(alias_columns) {
        col.name = normalize_ident(&def.name);
    }
    Ok(())
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
                sqlstate::INVALID_COLUMN_REFERENCE,
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
    // WITH is handled by the caller (bind_ctes); it is not rejected here.
    let unsupported: Option<&str> = if query.limit_clause.is_some() {
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
    let body = bind_select_body(select, order_by, &schema, &qualifier)?;
    Ok(LogicalPlan::Query {
        table,
        columns: body.columns,
        projections: body.projections,
        predicate: body.predicate,
        sort: body.sort,
    })
}

/// Bind a set-returning function in FROM position. Its fixed output columns
/// form a synthetic single-relation schema, so the SELECT list, WHERE and
/// ORDER BY bind exactly as they would over a base table.
fn bind_table_function_select(
    name: &ast::ObjectName,
    fn_args: &ast::TableFunctionArgs,
    alias: &Option<ast::TableAlias>,
    select: &ast::Select,
    order_by: &Option<ast::OrderBy>,
) -> Result<LogicalPlan, BindError> {
    if fn_args.settings.is_some() {
        return Err(BindError::feature_not_supported(
            "table function SETTINGS are not supported yet",
        ));
    }
    let fname = object_name_to_table_name(name)?;
    let arg_exprs = positional_arg_exprs(&fn_args.args)?;
    let (func, args) = bind_table_fn_call(&fname, &arg_exprs, &Scope::empty())?;

    let qualifier = match alias {
        None => fname,
        Some(alias) => {
            if !alias.columns.is_empty() {
                return Err(BindError::feature_not_supported(
                    "column aliases in FROM are not supported yet",
                ));
            }
            normalize_ident(&alias.name)
        }
    };
    let schema = TableSchema {
        name: qualifier.clone(),
        columns: func
            .columns()
            .into_iter()
            .map(|c| Column {
                name: c.name,
                ty: c.ty,
            })
            .collect(),
    };
    let body = bind_select_body(select, order_by, &schema, &qualifier)?;
    Ok(LogicalPlan::TableFunction {
        func,
        args,
        columns: body.columns,
        projections: body.projections,
        predicate: body.predicate,
        sort: body.sort,
    })
}

/// The bound pieces of a SELECT over a single in-scope relation.
struct SelectBody {
    columns: Vec<OutputColumn>,
    projections: Vec<BoundExpr>,
    predicate: Option<BoundExpr>,
    sort: Vec<SortKey>,
}

/// Bind a SELECT's projection list, WHERE and ORDER BY against a single
/// in-scope relation (`schema` addressed by `qualifier`).
fn bind_select_body(
    select: &ast::Select,
    order_by: &Option<ast::OrderBy>,
    schema: &TableSchema,
    qualifier: &str,
) -> Result<SelectBody, BindError> {
    let scope = Scope::table(schema, qualifier.to_string());

    let mut columns = Vec::new();
    let mut projections = Vec::new();
    for item in &select.projection {
        match classify_select_item(item)? {
            SelectField::Wildcard => {
                expand_all(schema, &mut columns, &mut projections);
            }
            SelectField::QualifiedWildcard(q) => {
                // `f.*` is only valid for the relation's qualifier.
                if q != qualifier {
                    return Err(BindError::new(
                        sqlstate::UNDEFINED_TABLE,
                        format!("missing FROM-clause entry for table \"{q}\""),
                    ));
                }
                expand_all(schema, &mut columns, &mut projections);
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
    Ok(SelectBody {
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
    // A WITH on the INSERT source (`INSERT ... WITH c AS (...) VALUES ...`) is not
    // executed here; reject it rather than silently dropping the CTE. (Top-level
    // WITH is handled by bind_ctes, but the INSERT path never reaches it.)
    if source.with.is_some() {
        return Err(BindError::feature_not_supported(
            "WITH on INSERT is not supported yet",
        ));
    }
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
    fn float_and_numeric_reject_unsupported_operators() {
        // `%` is integer-only; float/numeric modulo does not exist in PG.
        let e = bind_err("SELECT 1.5 % 2.0");
        assert_eq!(e.code, "42883");
        assert_eq!(
            e.message,
            "operator does not exist: double precision % double precision"
        );
        // numeric has no operators in this stub.
        let e = bind_err("SELECT '1'::numeric < '2'::numeric");
        assert_eq!(e.code, "42883");
        assert_eq!(e.message, "operator does not exist: numeric < numeric");
    }

    #[test]
    fn int2_arithmetic_binds() {
        let LogicalPlan::Values { rows, .. } = bind_one("SELECT '1'::int2 + '2'::int2").unwrap()
        else {
            panic!("expected Values");
        };
        assert_eq!(rows[0][0].ty(), PgType::Int2);
    }

    #[test]
    fn implicit_int_to_float4_function_arg_resolves() {
        // float4send(integer) works via the implicit int4->float4 cast.
        assert!(bind_one("SELECT float4send(1)").is_ok());
    }

    #[test]
    fn cast_keeps_bare_column_name() {
        let LogicalPlan::Query { columns, .. } = bind_one("SELECT id::int8 FROM t").unwrap() else {
            panic!("expected Query");
        };
        assert_eq!(columns[0].name, "id");
        // A constant/nested cast falls back to the target type name.
        let LogicalPlan::Values { columns, .. } = bind_one("SELECT 'nan'::numeric::float4").unwrap()
        else {
            panic!("expected Values");
        };
        assert_eq!(columns[0].name, "float4");
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

    #[test]
    fn set_returning_function_in_from_binds_columns() {
        let LogicalPlan::TableFunction { func, columns, .. } =
            bind_one("SELECT * FROM pg_input_error_info('1e400', 'float4')").unwrap()
        else {
            panic!("expected TableFunction");
        };
        assert_eq!(func, crate::TableFn::PgInputErrorInfo);
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["message", "detail", "hint", "sql_error_code"]);
        assert!(columns.iter().all(|c| c.ty == PgType::Text));
    }

    #[test]
    fn set_returning_function_projects_and_filters() {
        // A subset projection over the SRF's columns resolves like a table.
        let LogicalPlan::TableFunction {
            columns, predicate, ..
        } = bind_one(
            "SELECT sql_error_code FROM pg_input_error_info('1e400', 'float4') \
             WHERE message IS NOT NULL",
        )
        .unwrap()
        else {
            panic!("expected TableFunction");
        };
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "sql_error_code");
        assert!(predicate.is_some());
    }

    #[test]
    fn unknown_set_returning_function_is_42883() {
        let e = bind_err("SELECT * FROM no_such_srf('x')");
        assert_eq!(e.code, "42883");
        assert_eq!(e.message, "function no_such_srf(unknown) does not exist");
    }

    #[test]
    fn standalone_values_binds_to_values_plan() {
        let LogicalPlan::Values { columns, rows, .. } =
            bind_one("VALUES (1, 'a'), (2, 'b')").unwrap()
        else {
            panic!("expected Values");
        };
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].name, "column1");
        assert_eq!(columns[1].name, "column2");
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn values_uneven_row_lengths_error() {
        let e = bind_err("VALUES (1), (2, 3)");
        assert_eq!(e.code, "42601");
    }

    #[test]
    fn values_common_type_keeps_real_over_int() {
        // PG's select_common_type resolves (real, int4) to real, not float8
        // (int4 implicitly casts to real). Contrast with operator resolution.
        let LogicalPlan::Values { columns, .. } =
            bind_one("VALUES (CAST(1.5 AS real)), (2)").unwrap()
        else {
            panic!("expected Values");
        };
        assert_eq!(columns[0].ty, PgType::Float4);
    }

    #[test]
    fn derived_table_binds_to_subquery_plan() {
        let LogicalPlan::Subquery { columns, .. } =
            bind_one("SELECT x FROM (VALUES (1), (2)) v(x)").unwrap()
        else {
            panic!("expected Subquery");
        };
        assert_eq!(columns[0].name, "x");
    }

    #[test]
    fn derived_table_requires_alias() {
        let e = bind_err("SELECT * FROM (VALUES (1))");
        assert_eq!(e.code, "42601");
        assert_eq!(e.message, "subquery in FROM must have an alias");
    }

    #[test]
    fn cte_reference_resolves_to_subquery() {
        let LogicalPlan::Subquery { columns, .. } =
            bind_one("WITH t(x) AS (VALUES (1)) SELECT x FROM t").unwrap()
        else {
            panic!("expected Subquery");
        };
        assert_eq!(columns[0].name, "x");
    }

    #[test]
    fn cte_column_count_mismatch_errors() {
        let e = bind_err("WITH t(a, b) AS (VALUES (1)) SELECT * FROM t");
        assert_eq!(e.code, "42P10");
        assert_eq!(
            e.message,
            "WITH query \"t\" has 1 columns available but 2 columns specified"
        );
    }

    #[test]
    fn derived_table_column_count_mismatch_errors() {
        let e = bind_err("SELECT * FROM (VALUES (1)) v(a, b)");
        assert_eq!(e.code, "42P10");
        assert_eq!(
            e.message,
            "table \"v\" has 1 columns available but 2 columns specified"
        );
    }

    #[test]
    fn duplicate_cte_name_is_42712() {
        let e = bind_err("WITH t AS (VALUES (1)), t AS (VALUES (2)) SELECT * FROM t");
        assert_eq!(e.code, "42712");
        assert_eq!(e.message, "WITH query name \"t\" specified more than once");
    }

    #[test]
    fn with_on_insert_source_is_rejected() {
        // The WITH must not be silently dropped: reject rather than insert (10).
        let e = bind_err("INSERT INTO t (id) WITH c AS (SELECT 1) VALUES (10)");
        assert_eq!(e.code, "0A000");
        assert_eq!(e.message, "WITH on INSERT is not supported yet");
    }

    #[test]
    fn with_recursive_is_rejected() {
        let e = bind_err("WITH RECURSIVE t(n) AS (VALUES (1)) SELECT n FROM t");
        assert_eq!(e.code, "0A000");
        assert_eq!(e.message, "WITH RECURSIVE is not supported yet");
    }

    #[test]
    fn cte_shadows_a_real_table() {
        // `t` here is the CTE, not the base table `t`; its column is `x`.
        let LogicalPlan::Subquery { columns, .. } =
            bind_one("WITH t(x) AS (VALUES (1)) SELECT x FROM t").unwrap()
        else {
            panic!("expected Subquery");
        };
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "x");
    }
}
