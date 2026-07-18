//! Statement binding: AST statements → [`LogicalPlan`].
//!
//! Everything parsed but not executed must be rejected loudly (`0A000`):
//! silently dropping a clause would return wrong results instead of an honest
//! error.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crabgresql_parser::ast;
use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::{Column, StorageError, TableAm, TableEngine, TypeCatalog};
use crabgresql_types::{PgType, Value};

use crate::expr::{
    BoundExpr, Scope, bind_expr, bind_projection, bind_scalar, coerce_to_column, normalize_ident,
    output_name, to_bool_operand, unify_value_column,
};
use crate::functions::{bind_table_fn_call, positional_arg_exprs};
use crate::{BindError, OutputColumn, TableFn};

/// A bound statement: names resolved, expressions typed, clauses vetted.
/// Carries the opened `TableAm` so later stages never re-resolve the name.
/// One ORDER BY key: an index into the projected tuple, the type its values
/// compare as, and its direction. `column` may address a hidden ("resjunk")
/// column appended past the visible output width when ORDER BY references an
/// expression not in the select list. NULLs order last for ASC, first for DESC
/// (PG defaults).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortKey {
    pub column: usize,
    pub ty: PgType,
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
    /// SELECT over the cartesian product of two or more FROM items (`FROM a, b`
    /// or `a CROSS JOIN b`). `inputs` produce the per-relation rows, laid out
    /// left-to-right in the combined row; the same projection/predicate/sort
    /// pipeline as `Query` runs on top, with `ColumnRef`s indexing the combined
    /// row.
    Join {
        inputs: Vec<JoinInput>,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
    },
    /// LIMIT/OFFSET applied above a SELECT body — after its ORDER BY, since PG
    /// evaluates the count clauses on the ordered result. `source` produces the
    /// (ordered) rows; this node skips `offset` of them and stops after `limit`.
    /// A wrapper rather than a field on every SELECT variant: LIMIT/OFFSET is a
    /// query-level construct that sits above the whole select, mirroring PG's
    /// Limit plan node above the sort.
    Limit {
        source: Box<LogicalPlan>,
        /// `None` = no limit (`LIMIT ALL` or clause absent).
        limit: Option<i64>,
        /// `None` = `OFFSET 0` (clause absent).
        offset: Option<i64>,
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

/// One row source feeding a [`LogicalPlan::Join`]: a base table scan, a
/// subplan (derived table, CTE reference, or `VALUES` in FROM), or a
/// set-returning function.
#[derive(Clone)]
pub enum JoinInput {
    Scan(Arc<dyn TableAm>),
    Subplan(Box<LogicalPlan>),
    TableFunction { func: TableFn, args: Vec<BoundExpr> },
}

/// Split a relation name into an optional schema qualifier and the relation
/// name. `t` → `(None, "t")`; `pg_catalog.pg_type` → `(Some("pg_catalog"),
/// "pg_type")`. A three-or-more-part name (cross-database) is still unsupported.
fn split_relation_name(
    name: &ast::ObjectName,
) -> Result<(Option<String>, String), BindError> {
    let idents: Vec<&ast::Ident> = name.0.iter().filter_map(|p| p.as_ident()).collect();
    if idents.len() != name.0.len() {
        return Err(BindError::syntax(format!("invalid relation name: {name}")));
    }
    match idents.as_slice() {
        [rel] => Ok((None, normalize_ident(rel))),
        [schema, rel] => Ok((Some(normalize_ident(schema)), normalize_ident(rel))),
        _ => Err(BindError::feature_not_supported(format!(
            "cross-database relation names are not supported yet: {name}"
        ))),
    }
}

/// A single-part relation name (no schema qualifier allowed) — for contexts
/// like CTE names and qualified wildcards where a schema makes no sense.
fn object_name_to_table_name(name: &ast::ObjectName) -> Result<String, BindError> {
    match split_relation_name(name)? {
        (None, rel) => Ok(rel),
        (Some(_), _) => Err(BindError::feature_not_supported(format!(
            "qualified name is not supported here: {name}"
        ))),
    }
}

/// The relation name for a user-facing error, schema-qualified exactly as the
/// user wrote it — so a miss reports `relation "pg_catalog.pg_type" does not
/// exist`, matching PG rather than dropping the schema.
fn display_relation_name(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    }
}

/// Rewrite a `TableNotFound` so its relation name carries the schema qualifier
/// the user typed (other storage errors pass through unchanged).
fn not_found_as_written(e: StorageError, schema: Option<&str>, name: &str) -> BindError {
    match e {
        StorageError::TableNotFound(_) => {
            StorageError::TableNotFound(display_relation_name(schema, name)).into()
        }
        other => other.into(),
    }
}

/// Resolve a write target (INSERT/UPDATE/DELETE) and its bare relation name.
/// A write never reaches the read-only system catalog: an unqualified name
/// searches temp then global (never `pg_catalog`), `public.` targets the
/// permanent relation only, `pg_temp.` the session temp store, and a write to
/// `pg_catalog` (or any other schema) is refused.
fn resolve_write_table(
    engine: &Arc<dyn TableEngine>,
    name: &ast::ObjectName,
) -> Result<(Arc<dyn TableAm>, String), BindError> {
    let (schema, table_name) = split_relation_name(name)?;
    let table = match schema.as_deref() {
        // Unqualified: temp then global — `open_table` never consults the
        // system catalog, so this cannot resolve to a read-only relation.
        None => engine.open_table(&table_name),
        Some("pg_catalog") => {
            // Matches PG's observable error for a non-superuser writing a system
            // catalog (crabgresql has no roles, so this is the default posture).
            return Err(BindError::new(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for table {table_name}"),
            ));
        }
        // `public.` -> global only, `pg_temp.` -> temp only, other -> not found.
        Some(_) => engine.resolve(schema.as_deref(), &table_name),
    };
    let table = table.map_err(|e| not_found_as_written(e, schema.as_deref(), &table_name))?;
    Ok((table, table_name))
}

/// Resolve an `UPDATE`/`DELETE` target plus the qualifier for its columns.
fn open_write_relation(
    engine: &Arc<dyn TableEngine>,
    relation: &ast::TableFactor,
) -> Result<(Arc<dyn TableAm>, String), BindError> {
    let ast::TableFactor::Table { name, alias, .. } = relation else {
        return Err(BindError::feature_not_supported(format!(
            "target is not supported yet: {relation}"
        )));
    };
    let (table, table_name) = resolve_write_table(engine, name)?;
    let qualifier = aliased_qualifier(alias, table_name)?;
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
    catalog: &Arc<dyn TypeCatalog>,
    query: &ast::Query,
) -> Result<LogicalPlan, BindError> {
    bind_query_scoped(engine, catalog, query, &CteEnv::new())
}

/// Bind a query with a set of visible CTEs. Recurses for CTE bodies and derived
/// tables, extending the environment with this query's own `WITH` clause.
fn bind_query_scoped(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    query: &ast::Query,
    outer: &CteEnv,
) -> Result<LogicalPlan, BindError> {
    // Only build (clone) an extended environment when this query has a WITH; the
    // common no-CTE case binds against `outer` directly.
    match &query.with {
        Some(with) => {
            let ctes = bind_ctes(engine, catalog, with, outer)?;
            bind_query_body(engine, catalog, query, &ctes)
        }
        None => bind_query_body(engine, catalog, query, outer),
    }
}

/// Bind a query's body (SELECT or VALUES) against a resolved CTE environment.
fn bind_query_body(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    query: &ast::Query,
    ctes: &CteEnv,
) -> Result<LogicalPlan, BindError> {
    reject_unsupported_query_clauses(query)?;
    let inner = match query.body.as_ref() {
        ast::SetExpr::Select(select) => {
            reject_unsupported_select_clauses(select)?;
            bind_select(engine, catalog, select, &query.order_by, ctes)
        }
        ast::SetExpr::Values(values) => bind_values_query(catalog, values, &query.order_by),
        other => Err(BindError::feature_not_supported(format!(
            "query form is not supported yet: {other}"
        ))),
    }?;
    // LIMIT/OFFSET wrap the bound body so they apply after its ORDER BY.
    match &query.limit_clause {
        Some(clause) => {
            let (limit, offset) = bind_limit_offset(clause)?;
            Ok(LogicalPlan::Limit {
                source: Box::new(inner),
                limit,
                offset,
            })
        }
        None => Ok(inner),
    }
}

/// Fold a `LIMIT`/`OFFSET` clause into constant row counts. PG evaluates these as
/// `bigint` expressions; we support constant integers (the only form the tests
/// and typical queries need). `LIMIT ALL`, a `NULL` count, or an absent clause
/// all mean "no bound" (`None`). Negative counts are rejected with PG's wording.
fn bind_limit_offset(clause: &ast::LimitClause) -> Result<(Option<i64>, Option<i64>), BindError> {
    let ast::LimitClause::LimitOffset {
        limit,
        offset,
        limit_by,
    } = clause
    else {
        // MySQL `LIMIT <offset>, <limit>` — not PG syntax.
        return Err(BindError::feature_not_supported(
            "LIMIT <offset>, <limit> is not supported yet",
        ));
    };
    if !limit_by.is_empty() {
        return Err(BindError::feature_not_supported(
            "LIMIT ... BY is not supported yet",
        ));
    }
    let limit = limit
        .as_ref()
        .map(|e| bind_count_expr(e, "LIMIT"))
        .transpose()?
        .flatten();
    let offset = offset
        .as_ref()
        .map(|o| bind_count_expr(&o.value, "OFFSET"))
        .transpose()?
        .flatten();
    Ok((limit, offset))
}

/// Evaluate a single LIMIT/OFFSET count expression to a non-negative `i64`.
/// Returns `None` for a `NULL` literal (PG: no limit / offset 0). Non-constant
/// expressions are rejected as unsupported; negatives with PG's SQLSTATE.
fn bind_count_expr(expr: &ast::Expr, clause: &str) -> Result<Option<i64>, BindError> {
    match const_i64(expr) {
        Some(Some(n)) if n < 0 => {
            let (code, kind) = if clause == "OFFSET" {
                (sqlstate::INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE, "OFFSET")
            } else {
                (sqlstate::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE, "LIMIT")
            };
            Err(BindError::new(
                code,
                format!("{kind} must not be negative"),
            ))
        }
        Some(value) => Ok(value),
        None => Err(BindError::feature_not_supported(format!(
            "non-constant {clause} is not supported yet"
        ))),
    }
}

/// Constant-fold `expr` to an integer count. `Some(None)` is a recognized `NULL`
/// literal; `Some(Some(n))` a constant integer; `None` means not a constant we
/// evaluate. Handles integer literals and nested unary `+`/`-`.
fn const_i64(expr: &ast::Expr) -> Option<Option<i64>> {
    match expr {
        ast::Expr::Value(v) => match &v.value {
            ast::Value::Number(n, _) => n.parse().ok().map(Some),
            ast::Value::Null => Some(None),
            _ => None,
        },
        ast::Expr::UnaryOp {
            op: ast::UnaryOperator::Minus,
            expr,
        } => const_i64(expr).map(|v| v.map(|n| -n)),
        ast::Expr::UnaryOp {
            op: ast::UnaryOperator::Plus,
            expr,
        } => const_i64(expr),
        _ => None,
    }
}

/// Bind a `WITH` clause into a fresh environment layered on `outer`. CTEs bind in
/// order, each seeing earlier siblings; recursion and `WITH` on data-modifying
/// bodies are not yet supported. Duplicate names within one clause are rejected.
fn bind_ctes(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
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
        let plan = bind_query_scoped(engine, catalog, &cte.query, &ctes)?;
        let mut columns = output_columns_of(&plan)?;
        apply_alias_columns(&mut columns, &cte.alias.columns, &with_query_subject(&name))?;
        ctes.insert(name, CteRelation { columns, plan });
    }
    Ok(ctes)
}

/// Bind the SELECT body (its FROM items, projections, WHERE, ORDER BY).
///
/// The FROM clause is first flattened into a list of relations: comma-separated
/// items and `CROSS JOIN`s both contribute a cartesian-product input. Any other
/// join (`INNER`/`LEFT`/... with `ON`/`USING`/`NATURAL`) is still rejected.
fn bind_select(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    select: &ast::Select,
    order_by: &Option<ast::OrderBy>,
    ctes: &CteEnv,
) -> Result<LogicalPlan, BindError> {
    let relations = flatten_from(&select.from)?;
    match relations.as_slice() {
        [] => bind_values_select(catalog, select, order_by),
        [relation] => bind_from_select(engine, catalog, relation, select, order_by, ctes),
        _ => bind_cross_join(engine, catalog, &relations, select, order_by, ctes),
    }
}

/// Flatten `FROM` into a list of relations, treating comma-separated items and
/// `CROSS JOIN`s alike (both are cartesian products). Any join carrying a
/// condition — `INNER`/`LEFT`/`RIGHT`/`FULL` with `ON`/`USING`, or `NATURAL` —
/// is not supported yet.
fn flatten_from(from: &[ast::TableWithJoins]) -> Result<Vec<&ast::TableFactor>, BindError> {
    let mut relations = Vec::new();
    for item in from {
        relations.push(&item.relation);
        for join in &item.joins {
            match &join.join_operator {
                ast::JoinOperator::CrossJoin(ast::JoinConstraint::None) => {
                    relations.push(&join.relation);
                }
                _ => {
                    return Err(BindError::feature_not_supported(
                        "JOIN is not supported yet",
                    ));
                }
            }
        }
    }
    Ok(relations)
}

/// Bind a SELECT over a single FROM item. Resolves the item to a row source
/// with [`bind_from_item`], binds the projection/WHERE/ORDER BY against its
/// one-relation scope, then wraps the source in the matching plan variant. The
/// single-item plan shapes (`Query`/`Subquery`/`TableFunction`) are preserved;
/// only the dispatch is shared with the cross-join path.
fn bind_from_select(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    relation: &ast::TableFactor,
    select: &ast::Select,
    order_by: &Option<ast::OrderBy>,
    ctes: &CteEnv,
) -> Result<LogicalPlan, BindError> {
    let item = bind_from_item(engine, catalog, relation, ctes)?;
    let scope = Scope::relations(vec![(item.qualifier, to_columns(&item.columns))], catalog);
    let body = bind_select_body(select, order_by, &scope)?;
    Ok(match item.input {
        JoinInput::Scan(table) => LogicalPlan::Query {
            table,
            columns: body.columns,
            projections: body.projections,
            predicate: body.predicate,
            sort: body.sort,
        },
        JoinInput::Subplan(source) => LogicalPlan::Subquery {
            source,
            columns: body.columns,
            projections: body.projections,
            predicate: body.predicate,
            sort: body.sort,
        },
        JoinInput::TableFunction { func, args } => LogicalPlan::TableFunction {
            func,
            args,
            columns: body.columns,
            projections: body.projections,
            predicate: body.predicate,
            sort: body.sort,
        },
    })
}

/// Convert a rowset's output columns into storage `Column`s for a [`Scope`].
fn to_columns(columns: &[OutputColumn]) -> Vec<Column> {
    columns
        .iter()
        .map(|c| Column::new(c.name.clone(), c.ty))
        .collect()
}

/// A single FROM item resolved as a join input: its qualifier (alias, else
/// name), the columns it exposes for name resolution, and the row source that
/// produces its tuples.
struct BoundFromItem {
    qualifier: String,
    columns: Vec<OutputColumn>,
    input: JoinInput,
}

/// Resolve one FROM item to a [`BoundFromItem`] — the same dispatch as
/// [`bind_from_select`], but producing a bare row source (no projection
/// pipeline) so several can be combined into a cross join.
fn bind_from_item(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    relation: &ast::TableFactor,
    ctes: &CteEnv,
) -> Result<BoundFromItem, BindError> {
    match relation {
        // A `Table` factor carrying call arguments is a set-returning function.
        ast::TableFactor::Table {
            name,
            alias,
            args: Some(fn_args),
            ..
        } => {
            if fn_args.settings.is_some() {
                return Err(BindError::feature_not_supported(
                    "table function SETTINGS are not supported yet",
                ));
            }
            let fname = object_name_to_table_name(name)?;
            let arg_exprs = positional_arg_exprs(&fn_args.args)?;
            let (func, args) = bind_table_fn_call(&fname, &arg_exprs, &Scope::empty(catalog))?;
            let qualifier = relation_qualifier(alias, &fname);
            let mut columns: Vec<OutputColumn> = func
                .columns()
                .into_iter()
                .map(|c| OutputColumn {
                    name: c.name,
                    ty: c.ty,
                })
                .collect();
            apply_relation_alias_columns(&mut columns, alias, &table_subject(&qualifier))?;
            Ok(BoundFromItem {
                qualifier,
                columns,
                input: JoinInput::TableFunction { func, args },
            })
        }
        // A bare name may resolve to a CTE (which shadows a real table).
        ast::TableFactor::Table {
            name,
            alias,
            args: None,
            ..
        } => {
            // A CTE reference is always a bare (unqualified) name; a schema
            // qualifier means it is a real relation, so skip the CTE lookup.
            let (cte_schema, tname) = split_relation_name(name)?;
            if cte_schema.is_none()
                && let Some(cte) = ctes.get(&tname)
            {
                let qualifier = relation_qualifier(alias, &tname);
                let mut columns = cte.columns.clone();
                apply_relation_alias_columns(&mut columns, alias, &table_subject(&qualifier))?;
                return Ok(BoundFromItem {
                    qualifier,
                    columns,
                    input: JoinInput::Subplan(Box::new(cte.plan.clone())),
                });
            }
            // Read resolution honors the search path: an unqualified `pg_type`
            // reaches `pg_catalog`, and `pg_catalog.pg_type` routes there
            // directly (a qualified miss keeps its schema in the error text).
            let table = engine
                .resolve(cte_schema.as_deref(), &tname)
                .map_err(|e| not_found_as_written(e, cte_schema.as_deref(), &tname))?;
            let qualifier = relation_qualifier(alias, &tname);
            let mut columns: Vec<OutputColumn> = table
                .schema()
                .columns
                .iter()
                .map(|c| OutputColumn {
                    name: c.name.clone(),
                    ty: c.ty,
                })
                .collect();
            apply_relation_alias_columns(&mut columns, alias, &table_subject(&qualifier))?;
            Ok(BoundFromItem {
                qualifier,
                columns,
                input: JoinInput::Scan(table),
            })
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
            let inner = bind_query_scoped(engine, catalog, subquery, ctes)?;
            let mut columns = output_columns_of(&inner)?;
            apply_alias_columns(&mut columns, &alias.columns, &table_subject(&qualifier))?;
            Ok(BoundFromItem {
                qualifier,
                columns,
                input: JoinInput::Subplan(Box::new(inner)),
            })
        }
        other => Err(BindError::feature_not_supported(format!(
            "FROM item is not supported yet: {other}"
        ))),
    }
}

/// The qualifier for an `UPDATE`/`DELETE` target relation: its alias when
/// present, else `default`. A column-list alias (`f(a, b)`) is not valid on a
/// modify target (only on `FROM` items), so it is rejected here.
fn aliased_qualifier(
    alias: &Option<ast::TableAlias>,
    default: String,
) -> Result<String, BindError> {
    match alias {
        None => Ok(default),
        Some(alias) => {
            if !alias.columns.is_empty() {
                return Err(BindError::syntax(
                    "column aliases are not allowed on an UPDATE/DELETE target",
                ));
            }
            Ok(normalize_ident(&alias.name))
        }
    }
}

/// Bind a SELECT over the cartesian product of two or more FROM items. Each
/// item becomes a join input and a relation in a combined [`Scope`]; the
/// projection/WHERE/ORDER BY then bind across all of them at once, with
/// `ColumnRef`s indexing the concatenated row. Two items with the same
/// qualifier are rejected (`42712`), as in PG.
fn bind_cross_join(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    relations: &[&ast::TableFactor],
    select: &ast::Select,
    order_by: &Option<ast::OrderBy>,
    ctes: &CteEnv,
) -> Result<LogicalPlan, BindError> {
    let mut inputs = Vec::with_capacity(relations.len());
    let mut scope_rels: Vec<(String, Vec<Column>)> = Vec::with_capacity(relations.len());
    let mut seen: HashSet<String> = HashSet::new();
    for relation in relations {
        let item = bind_from_item(engine, catalog, relation, ctes)?;
        if !seen.insert(item.qualifier.clone()) {
            return Err(BindError::new(
                sqlstate::DUPLICATE_ALIAS,
                format!(
                    "table name \"{}\" specified more than once",
                    item.qualifier
                ),
            ));
        }
        scope_rels.push((item.qualifier, to_columns(&item.columns)));
        inputs.push(item.input);
    }
    let scope = Scope::relations(scope_rels, catalog);
    let body = bind_select_body(select, order_by, &scope)?;
    Ok(LogicalPlan::Join {
        inputs,
        columns: body.columns,
        projections: body.projections,
        predicate: body.predicate,
        sort: body.sort,
    })
}

/// A standalone `VALUES (...), (...)` list. Column names default to
/// `column1..columnN`; each column resolves to a common type across all rows.
fn bind_values_query(
    catalog: &Arc<dyn TypeCatalog>,
    values: &ast::Values,
    order_by: &Option<ast::OrderBy>,
) -> Result<LogicalPlan, BindError> {
    if values.rows.is_empty() {
        return Err(BindError::syntax("VALUES lists must not be empty"));
    }
    let width = values.rows[0].content.len();
    let scope = Scope::empty(catalog);
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
        let (ty, cells) = unify_value_column(bindings, "VALUES")?;
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

    // A standalone VALUES list has no projection tuple to append hidden sort
    // columns to (cells evaluate against an empty row), so only ordinals and
    // `columnN` names resolve; expressions stay `0A000`.
    let sort = bind_order_by(order_by, &columns, &scope, &mut Vec::new(), false)?;
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
        | LogicalPlan::TableFunction { columns, .. }
        | LogicalPlan::Join { columns, .. } => Ok(columns.clone()),
        // LIMIT/OFFSET is a transparent wrapper: it exposes its source's columns.
        LogicalPlan::Limit { source, .. } => output_columns_of(source),
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

/// Rename a rowset's columns from an alias column list (`t(a, b, c)`). As in PG,
/// the list may be shorter than the rowset — the leading columns are renamed and
/// the rest keep their names — but a longer list is an error. Per-column type
/// annotations are not supported. `subject` is the relation phrasing PG uses in
/// the column-count error (see [`table_subject`] / [`with_query_subject`]).
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
    // PG allows fewer aliases than columns — the leading columns are renamed and
    // the rest keep their original names — but rejects an alias list that is
    // *longer* than the rowset.
    if alias_columns.len() > columns.len() {
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

/// Bind an ORDER BY clause into sort keys indexing the projected tuple.
///
/// Each item resolves in PG's SQL92/SQL99 precedence:
/// 1. a bare unsigned integer → an output-column ordinal (`ORDER BY 1`);
/// 2. a bare, unqualified identifier matching an output column name → that
///    column (`ORDER BY total`); ambiguous names error `42702`;
/// 3. otherwise the whole expression binds against the FROM `scope` — output
///    aliases are deliberately *not* visible inside an expression, so
///    `SELECT 1 AS a ORDER BY a+1` errors like PG. A bound expression already in
///    `projections` is reused; anything else is appended as a hidden
///    ("resjunk") column that the executor sorts on and then trims.
///
/// `allow_hidden` gates case 3's expression binding: a standalone `VALUES` list
/// has no projection tuple to append to (its cells evaluate against an empty
/// row), so it only permits cases 1–2 and rejects expressions with `0A000`.
fn bind_order_by(
    order_by: &Option<ast::OrderBy>,
    columns: &[OutputColumn],
    scope: &Scope,
    projections: &mut Vec<BoundExpr>,
    allow_hidden: bool,
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
        // Resolve the item to (projected-tuple index, comparison type).
        let (column, ty) = order_by_target(&oe.expr, columns, scope, projections, allow_hidden)?;
        // The executor's sort compares keys with `compare_values`, which panics
        // on a type it can't order (e.g. `bit`). Reject such a key at bind time
        // rather than aborting mid-sort.
        if !crate::expr::is_orderable(ty) {
            return Err(BindError::feature_not_supported(format!(
                "ORDER BY on type {} is not supported yet",
                ty.name()
            )));
        }
        let asc = oe.options.asc.unwrap_or(true);
        let nulls_first = oe.options.nulls_first.unwrap_or(!asc);
        keys.push(SortKey {
            column,
            ty,
            asc,
            nulls_first,
        });
    }
    Ok(keys)
}

/// Resolve one ORDER BY item to `(column index into the projected tuple, type)`,
/// appending a hidden projection when the item is an expression not already
/// present. See [`bind_order_by`] for the precedence.
fn order_by_target(
    expr: &ast::Expr,
    columns: &[OutputColumn],
    scope: &Scope,
    projections: &mut Vec<BoundExpr>,
    allow_hidden: bool,
) -> Result<(usize, PgType), BindError> {
    // (1) A bare unsigned integer literal is an output-column ordinal. `1+1` is
    // an expression, not ordinal 2, so only a plain `Value::Number` qualifies.
    if let ast::Expr::Value(v) = expr
        && let ast::Value::Number(n, _) = &v.value
        && let Ok(ordinal) = n.parse::<usize>()
    {
        if ordinal < 1 || ordinal > columns.len() {
            return Err(BindError::new(
                sqlstate::INVALID_COLUMN_REFERENCE,
                format!("ORDER BY position {ordinal} is not in select list"),
            ));
        }
        return Ok((ordinal - 1, columns[ordinal - 1].ty));
    }

    // (2) A bare, unqualified identifier matches an output column name first.
    // Qualified names (`t.x`) skip this and bind as expressions, as in PG.
    if let ast::Expr::Identifier(ident) = expr {
        let name = normalize_ident(ident);
        let mut hit: Option<usize> = None;
        for (i, col) in columns.iter().enumerate() {
            if col.name == name {
                if hit.is_some() {
                    return Err(BindError::new(
                        sqlstate::AMBIGUOUS_COLUMN,
                        format!("ORDER BY \"{name}\" is ambiguous"),
                    ));
                }
                hit = Some(i);
            }
        }
        if let Some(i) = hit {
            return Ok((i, columns[i].ty));
        }
        // Fall through: a name not in the select list may still be an input
        // column resolvable against the scope (`SELECT a FROM t ORDER BY b`).
    }

    // (3) Bind the expression against the FROM scope.
    if !allow_hidden {
        return Err(BindError::feature_not_supported(
            "ORDER BY expressions are not supported yet (only column ordinals)",
        ));
    }
    let bound = bind_scalar(expr, scope)?;
    if bound.is_srf() {
        // PG: "set-returning functions are not allowed in ORDER BY".
        return Err(BindError::feature_not_supported(
            "set-returning functions are not allowed in ORDER BY",
        ));
    }
    let ty = bound.ty();
    // Reuse an equal projection (PG's target-entry reuse) — scan the growing
    // list so a later key can reuse an earlier key's hidden column too.
    if let Some(i) = projections.iter().position(|p| *p == bound) {
        return Ok((i, ty));
    }
    let index = projections.len();
    projections.push(bound);
    Ok((index, ty))
}

fn reject_unsupported_query_clauses(query: &ast::Query) -> Result<(), BindError> {
    // WITH is handled by the caller (bind_ctes) and LIMIT/OFFSET by the caller
    // (bind_limit_offset); neither is rejected here.
    let unsupported: Option<&str> = if query.fetch.is_some() {
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
    catalog: &Arc<dyn TypeCatalog>,
    select: &ast::Select,
    order_by: &Option<ast::OrderBy>,
) -> Result<LogicalPlan, BindError> {
    let scope = Scope::empty(catalog);
    let mut columns = Vec::new();
    let mut row = Vec::new();
    for item in &select.projection {
        let SelectField::Expr(expr, alias) = classify_select_item(item)? else {
            return Err(BindError::syntax(
                "SELECT * with no tables specified is not valid",
            ));
        };
        let bound = bind_projection(expr, &scope)?;
        columns.push(OutputColumn {
            name: alias.unwrap_or_else(|| output_name(expr)),
            ty: bound.ty(),
        });
        row.push(bound);
    }
    let predicate = bind_where(&select.selection, &scope)?;
    // The empty scope means any hidden ORDER BY expression is column-free
    // (`ORDER BY random()`), so appending it to `row` stays safe against the
    // Values node's empty-row evaluation. Hidden columns are never SRFs, so the
    // SRF check below is unaffected.
    let sort = bind_order_by(order_by, &columns, &scope, &mut row, true)?;
    // A FROM-less SELECT with a set-returning function in the target list
    // (`SELECT generate_series(1, 5)`) expands into rows, so it cannot be a
    // constant `Values`. Run the projection pipeline over a single dummy input
    // row: `ProjectSet` then expands each SRF. Mirrors PG's Result + ProjectSet.
    if row.iter().any(BoundExpr::is_srf) {
        let source = LogicalPlan::Values {
            columns: Vec::new(),
            rows: vec![vec![]],
            predicate: None,
            sort: Vec::new(),
        };
        return Ok(LogicalPlan::Subquery {
            source: Box::new(source),
            columns,
            projections: row,
            predicate,
            sort,
        });
    }
    Ok(LogicalPlan::Values {
        columns,
        rows: vec![row],
        predicate,
        sort,
    })
}

/// The bound pieces of a SELECT over a single in-scope relation.
struct SelectBody {
    columns: Vec<OutputColumn>,
    projections: Vec<BoundExpr>,
    predicate: Option<BoundExpr>,
    sort: Vec<SortKey>,
}

/// Bind a SELECT's projection list, WHERE and ORDER BY against the in-scope
/// relation(s). One relation for a single-table SELECT / subquery / SRF, more
/// for a cross join — `scope` handles wildcard expansion and column resolution
/// uniformly across however many relations it holds.
fn bind_select_body(
    select: &ast::Select,
    order_by: &Option<ast::OrderBy>,
    scope: &Scope,
) -> Result<SelectBody, BindError> {
    let mut columns = Vec::new();
    let mut projections = Vec::new();
    for item in &select.projection {
        match classify_select_item(item)? {
            SelectField::Wildcard => {
                for (col, expr) in scope.expand_wildcard() {
                    columns.push(col);
                    projections.push(expr);
                }
            }
            SelectField::QualifiedWildcard(q) => {
                // `f.*` expands the columns of the relation named `f`.
                for (col, expr) in scope.expand_qualified(&q)? {
                    columns.push(col);
                    projections.push(expr);
                }
            }
            SelectField::Expr(expr, alias) => {
                let bound = bind_projection(expr, scope)?;
                columns.push(OutputColumn {
                    name: alias.unwrap_or_else(|| output_name(expr)),
                    ty: bound.ty(),
                });
                projections.push(bound);
            }
        }
    }

    let predicate = bind_where(&select.selection, scope)?;
    let sort = bind_order_by(order_by, &columns, scope, &mut projections, true)?;
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

pub fn bind_insert(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    insert: &ast::Insert,
) -> Result<LogicalPlan, BindError> {
    let target = match &insert.table {
        ast::TableObject::TableName(name) => name,
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
    // Same write-target routing as UPDATE/DELETE: `public.t` reaches the
    // permanent relation, a write to `pg_catalog` is refused, and the system
    // catalog is never a write target.
    let (table, name) = resolve_write_table(engine, target)?;
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
    // `VALUES (1),(2) LIMIT 1` is a legal INSERT source in PG that inserts one
    // row; this path does not execute the limit, so reject it rather than
    // silently insert the wrong rows.
    if source.limit_clause.is_some() {
        return Err(BindError::feature_not_supported(
            "LIMIT/OFFSET on an INSERT source is not supported yet",
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
    let scope = Scope::empty(catalog);
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
    catalog: &Arc<dyn TypeCatalog>,
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

    let (table, qualifier) = open_write_relation(engine, &update.table.relation)?;
    let schema = table.schema().clone();
    let table_name = schema.name.clone();
    let scope = Scope::table(&schema, qualifier, catalog);

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
    catalog: &Arc<dyn TypeCatalog>,
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

    let (table, qualifier) = open_write_relation(engine, &target.relation)?;
    let schema = table.schema().clone();
    let scope = Scope::table(&schema, qualifier, catalog);
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
                    Column::new("id", PgType::Int4),
                    Column::new("big", PgType::Int8),
                    Column::new("name", PgType::Text),
                    Column::new("flag", PgType::Bool),
                ],
            })
            .unwrap();
        Arc::new(engine)
    }

    fn bind_one(sql: &str) -> Result<LogicalPlan, BindError> {
        let engine = engine_with_table();
        let catalog: Arc<dyn TypeCatalog> =
            Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
        let stmts = crabgresql_parser::parse(sql).unwrap();
        match &stmts[0] {
            ast::Statement::Query(q) => bind_query(&engine, &catalog, q),
            ast::Statement::Insert(i) => bind_insert(&engine, &catalog, i),
            ast::Statement::Update(u) => bind_update(&engine, &catalog, u),
            ast::Statement::Delete(d) => bind_delete(&engine, &catalog, d),
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

    /// The first projected expression of a bound FROM-less `SELECT`.
    fn one_projection(sql: &str) -> BoundExpr {
        let LogicalPlan::Values { mut rows, .. } = bind_one(sql).unwrap() else {
            panic!("expected Values");
        };
        rows.remove(0).remove(0)
    }

    #[test]
    fn string_concat_lowers_to_text_concat() {
        let expr = one_projection("SELECT 'a' || 'b'");
        assert!(matches!(
            expr,
            BoundExpr::FuncCall { func: crate::ScalarFn::TextConcat, ret: PgType::Text, .. }
        ));
    }

    #[test]
    fn concat_of_two_non_text_is_undefined_operator() {
        let e = bind_err("SELECT 1 || 2");
        assert_eq!(e.code, "42883");
        assert_eq!(e.message, "operator does not exist: integer || integer");
    }

    #[test]
    fn like_binds_to_bool_and_not_wraps() {
        assert_eq!(one_projection("SELECT 'a' LIKE 'a%'").ty(), PgType::Bool);
        assert!(matches!(
            one_projection("SELECT 'a' NOT LIKE 'b%'"),
            BoundExpr::Unary { op: crate::UnaryOp::Not, .. }
        ));
    }

    #[test]
    fn char_types_carry_their_type_and_length() {
        assert_eq!(one_projection("SELECT 'abcdef'::varchar(3)").ty(), PgType::Varchar);
        // `char(3)` truncates a constant at bind time (explicit-cast semantics).
        assert_eq!(
            one_projection("SELECT 'abcdef'::char(3)"),
            BoundExpr::Const { value: Value::Text("abc".into()), ty: PgType::Bpchar }
        );
        // A bare `char` is `char(1)` and blank-pads a short constant.
        assert_eq!(
            one_projection("SELECT 'a'::char(3)"),
            BoundExpr::Const { value: Value::Text("a  ".into()), ty: PgType::Bpchar }
        );
    }

    #[test]
    fn substring_and_position_desugar_to_functions() {
        assert_eq!(one_projection("SELECT substring('abc' FROM 2 FOR 1)").ty(), PgType::Text);
        assert_eq!(one_projection("SELECT position('b' IN 'abc')").ty(), PgType::Int4);
        assert_eq!(one_projection("SELECT length('abc')").ty(), PgType::Int4);
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
    fn decimal_literal_binds_as_numeric() {
        let LogicalPlan::Values { rows, .. } = bind_one("SELECT 1.5").unwrap() else {
            panic!("expected Values");
        };
        let BoundExpr::Const { value: Value::Numeric(n), ty: PgType::Numeric } = &rows[0][0] else {
            panic!("expected numeric const, got {:?}", rows[0][0]);
        };
        assert_eq!(n.to_display(), "1.5");
    }

    #[test]
    fn hex_literal_binds_as_bit() {
        // X'...' is a bit(n) value with n = 4 * hex digits.
        assert_eq!(
            one_projection("SELECT X'00000001'"),
            BoundExpr::Const { value: Value::Bit { len: 32, bits: 1 }, ty: PgType::Bit }
        );
        // Lowercase hex parses too.
        assert_eq!(
            one_projection("SELECT X'ff'"),
            BoundExpr::Const { value: Value::Bit { len: 8, bits: 0xff }, ty: PgType::Bit }
        );
    }

    #[test]
    fn hex_literal_too_wide_is_rejected() {
        // 17 hex digits = 68 bits, past the u64 backing of Value::Bit.
        let e = bind_err("SELECT X'FFFFFFFFFFFFFFFFF'");
        assert_eq!(e.code, "0A000");
    }

    #[test]
    fn hex_literal_with_bad_digit_is_data_exception() {
        // PG's bit_in reports 22P02 naming the first non-hex character; a leading
        // sign (which u64::from_str_radix would accept) is rejected the same way.
        for (sql, bad) in [
            ("SELECT X'GG'", "G"),
            ("SELECT X'+1'", "+"),
            ("SELECT X'-1'", "-"),
            ("SELECT X'1 2'", " "),
        ] {
            let e = bind_err(sql);
            assert_eq!(e.code, "22P02", "{sql}");
            assert_eq!(e.message, format!("\"{bad}\" is not a valid hexadecimal digit"));
        }
    }

    #[test]
    fn empty_hex_literal_binds_as_zero_width_bit() {
        assert_eq!(
            one_projection("SELECT X''"),
            BoundExpr::Const { value: Value::Bit { len: 0, bits: 0 }, ty: PgType::Bit }
        );
    }

    #[test]
    fn order_by_on_bit_is_rejected_not_panicking() {
        // `bit` has no executor comparison, so ORDER BY on it must fail at bind
        // time rather than reaching the Sort node's `unreachable!`.
        let e = bind_err("SELECT X'FF' ORDER BY 1");
        assert_eq!(e.code, "0A000");
        assert_eq!(e.message, "ORDER BY on type bit is not supported yet");
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
    fn float_modulo_is_rejected() {
        // `%` exists for the integer types and numeric, but not float.
        let e = bind_err("SELECT '1.5'::float8 % '2.0'::float8");
        assert_eq!(e.code, "42883");
        assert_eq!(
            e.message,
            "operator does not exist: double precision % double precision"
        );
    }

    #[test]
    fn numeric_operators_bind() {
        // Comparison, arithmetic, and modulo all resolve for numeric now.
        assert!(bind_one("SELECT '1'::numeric < '2'::numeric").is_ok());
        let LogicalPlan::Values { rows, .. } = bind_one("SELECT 1.5 + 2.25").unwrap() else {
            panic!("expected Values");
        };
        assert_eq!(rows[0][0].ty(), PgType::Numeric);
        assert!(bind_one("SELECT 5.5 % 2.0").is_ok());
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
    fn generate_series_in_from_binds_int4_column() {
        let LogicalPlan::TableFunction { func, columns, .. } =
            bind_one("SELECT * FROM generate_series(1, 5)").unwrap()
        else {
            panic!("expected TableFunction");
        };
        assert_eq!(func, crate::TableFn::GenerateSeries(PgType::Int4));
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "generate_series");
        assert_eq!(columns[0].ty, PgType::Int4);
    }

    #[test]
    fn generate_series_widens_to_int8() {
        // A bigint bound widens the whole series to int8.
        let LogicalPlan::TableFunction { func, columns, .. } =
            bind_one("SELECT * FROM generate_series(1, 5000000000)").unwrap()
        else {
            panic!("expected TableFunction");
        };
        assert_eq!(func, crate::TableFn::GenerateSeries(PgType::Int8));
        assert_eq!(columns[0].ty, PgType::Int8);
    }

    #[test]
    fn generate_series_three_arg_step_binds() {
        let LogicalPlan::TableFunction { func, args, .. } =
            bind_one("SELECT * FROM generate_series(1, 10, 3)").unwrap()
        else {
            panic!("expected TableFunction");
        };
        assert_eq!(func, crate::TableFn::GenerateSeries(PgType::Int4));
        assert_eq!(args.len(), 3);
    }

    #[test]
    fn generate_series_wrong_arity_is_42883() {
        let e = bind_err("SELECT * FROM generate_series(1)");
        assert_eq!(e.code, "42883");
    }

    #[test]
    fn generate_series_in_target_list_is_srf_projection() {
        // A FROM-less SRF in the target list expands over a single dummy row.
        let LogicalPlan::Subquery {
            columns,
            projections,
            source,
            ..
        } = bind_one("SELECT generate_series(1, 5)").unwrap()
        else {
            panic!("expected Subquery over a single-row source");
        };
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "generate_series");
        assert_eq!(columns[0].ty, PgType::Int4);
        assert!(matches!(projections[0], BoundExpr::Srf { .. }));
        assert!(matches!(*source, LogicalPlan::Values { .. }));
    }

    #[test]
    fn generate_series_in_target_list_over_table() {
        // Mixed scalar + SRF projection over a base table stays a Query.
        let LogicalPlan::Query { projections, .. } =
            bind_one("SELECT id, generate_series(1, 2) FROM t").unwrap()
        else {
            panic!("expected Query");
        };
        assert!(matches!(projections[0], BoundExpr::ColumnRef { .. }));
        assert!(matches!(projections[1], BoundExpr::Srf { .. }));
    }

    fn table_fn(sql: &str) -> (crate::TableFn, Vec<OutputColumn>) {
        let LogicalPlan::TableFunction { func, columns, .. } = bind_one(sql).unwrap() else {
            panic!("expected TableFunction");
        };
        (func, columns)
    }

    #[test]
    fn generate_series_numeric_overload_binds() {
        // A decimal argument (typed numeric) selects the numeric overload.
        let (func, columns) = table_fn("SELECT * FROM generate_series(1, 3, 0.5)");
        assert_eq!(func, crate::TableFn::GenerateSeries(PgType::Numeric));
        assert_eq!(columns[0].name, "generate_series");
        assert_eq!(columns[0].ty, PgType::Numeric);
    }

    #[test]
    fn generate_series_timestamp_overload_binds() {
        let (func, columns) = table_fn(
            "SELECT * FROM generate_series(timestamp '2020-01-01', \
             timestamp '2020-01-05', interval '1 day')",
        );
        assert_eq!(func, crate::TableFn::GenerateSeries(PgType::Timestamp));
        assert_eq!(columns[0].ty, PgType::Timestamp);
    }

    #[test]
    fn generate_series_timestamptz_overload_binds() {
        let (func, _columns) = table_fn(
            "SELECT * FROM generate_series(timestamptz '2020-01-01+00', \
             timestamptz '2020-01-05+00', interval '1 day')",
        );
        assert_eq!(func, crate::TableFn::GenerateSeries(PgType::TimestampTz));
    }

    #[test]
    fn generate_series_timestamp_requires_three_args() {
        // The timestamp overload has no 2-arg form: PG rejects it as 42883.
        let e = bind_err(
            "SELECT * FROM generate_series(timestamp '2020-01-01', timestamp '2020-01-05')",
        );
        assert_eq!(e.code, "42883");
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

    fn case_column(sql: &str) -> (OutputColumn, BoundExpr) {
        let LogicalPlan::Query {
            columns,
            projections,
            ..
        } = bind_one(sql).unwrap()
        else {
            panic!("expected Query");
        };
        (columns[0].clone(), projections[0].clone())
    }

    #[test]
    fn case_default_column_name_is_case() {
        let (col, expr) = case_column("SELECT CASE WHEN flag THEN id END FROM t");
        assert_eq!(col.name, "case");
        assert!(matches!(expr, BoundExpr::Case { .. }));
    }

    #[test]
    fn case_result_branches_promote_to_common_type() {
        // int4 THEN, int8 ELSE -> int8, with a Coerce inserted on the int4 arm.
        let (col, expr) = case_column("SELECT CASE WHEN flag THEN id ELSE big END FROM t");
        assert_eq!(col.ty, PgType::Int8);
        let BoundExpr::Case { whens, else_, ty } = expr else {
            panic!("expected Case");
        };
        assert_eq!(ty, PgType::Int8);
        assert!(matches!(
            &whens[0].1,
            BoundExpr::Coerce { ty: PgType::Int8, .. }
        ));
        assert!(matches!(
            else_.as_deref(),
            Some(BoundExpr::ColumnRef { ty: PgType::Int8, .. })
        ));
    }

    #[test]
    fn all_untyped_case_branches_resolve_to_text() {
        let (col, _) = case_column("SELECT CASE WHEN flag THEN NULL ELSE NULL END FROM t");
        assert_eq!(col.ty, PgType::Text);
    }

    #[test]
    fn simple_case_desugars_operand_to_equality() {
        // CASE id WHEN 1 THEN ... becomes a boolean `id = 1` condition.
        let (_, expr) = case_column("SELECT CASE id WHEN 1 THEN 'a' END FROM t");
        let BoundExpr::Case { whens, .. } = expr else {
            panic!("expected Case");
        };
        assert!(matches!(
            &whens[0].0,
            BoundExpr::Binary {
                op: BinOp::Eq,
                arg_ty: PgType::Int4,
                ..
            }
        ));
    }

    #[test]
    fn non_boolean_when_condition_is_42804() {
        let e = bind_err("SELECT CASE WHEN id THEN 1 END FROM t");
        assert_eq!(e.code, "42804");
        assert_eq!(
            e.message,
            "argument of CASE/WHEN must be type boolean, not type integer"
        );
    }

    #[test]
    fn incompatible_case_results_are_42804() {
        // ELSE participates first in unification, matching PG's type order.
        let e = bind_err("SELECT CASE WHEN flag THEN id ELSE name END FROM t");
        assert_eq!(e.code, "42804");
        assert_eq!(e.message, "CASE types text and integer cannot be matched");
    }

    #[test]
    fn simple_case_untyped_operand_resolves_to_text() {
        // PG gives an untyped-literal operand its own type (text) before
        // comparing, so a NULL or string operand against an integer WHEN value
        // is `text = integer` (operator does not exist), not a read of the
        // operand as integer.
        for sql in [
            "SELECT CASE NULL WHEN 1 THEN 'a' ELSE 'b' END",
            "SELECT CASE 'x' WHEN 1 THEN 'a' END",
        ] {
            let e = bind_err(sql);
            assert_eq!(e.code, "42883", "{sql}");
            assert_eq!(e.message, "operator does not exist: text = integer", "{sql}");
        }
        // Two untyped literals still compare as text (unchanged).
        assert!(bind_one("SELECT CASE 'x' WHEN 'y' THEN 1 ELSE 2 END").is_ok());
    }

    #[test]
    fn cross_join_builds_join_plan_with_offsets() {
        // Two derived tables: a(x) at offset 0, b(y) at offset 1.
        let LogicalPlan::Join {
            inputs,
            columns,
            projections,
            ..
        } = bind_one("SELECT a.x, b.y FROM (VALUES (1)) a(x), (VALUES (2)) b(y)").unwrap()
        else {
            panic!("expected Join");
        };
        assert_eq!(inputs.len(), 2);
        assert_eq!(
            projections[0],
            BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4
            }
        );
        assert_eq!(
            projections[1],
            BoundExpr::ColumnRef {
                index: 1,
                ty: PgType::Int4
            }
        );
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["x", "y"]);
    }

    #[test]
    fn cross_join_wildcard_expands_every_relation_in_order() {
        let LogicalPlan::Join {
            columns,
            projections,
            ..
        } = bind_one("SELECT * FROM (VALUES (1, 2)) a(x, y), (VALUES (3)) b(z)").unwrap()
        else {
            panic!("expected Join");
        };
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["x", "y", "z"]);
        // b.z sits after a's two columns.
        assert_eq!(
            projections[2],
            BoundExpr::ColumnRef {
                index: 2,
                ty: PgType::Int4
            }
        );
    }

    #[test]
    fn cross_join_qualified_refs_use_combined_row_index() {
        // `t` occupies indices 0..4 (id, big, name, flag); b.y follows at 4.
        let LogicalPlan::Join { projections, .. } =
            bind_one("SELECT t.id, b.y FROM t, (VALUES (2)) b(y)").unwrap()
        else {
            panic!("expected Join");
        };
        assert_eq!(
            projections[0],
            BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4
            }
        );
        assert_eq!(
            projections[1],
            BoundExpr::ColumnRef {
                index: 4,
                ty: PgType::Int4
            }
        );
    }

    #[test]
    fn ambiguous_unqualified_column_is_42702() {
        let e = bind_err("SELECT x FROM (VALUES (1)) a(x), (VALUES (2)) b(x)");
        assert_eq!(e.code, "42702");
        assert_eq!(e.message, "column reference \"x\" is ambiguous");
    }

    #[test]
    fn duplicate_from_qualifier_is_42712() {
        let e = bind_err("SELECT * FROM t, t");
        assert_eq!(e.code, "42712");
        assert_eq!(e.message, "table name \"t\" specified more than once");
    }

    #[test]
    fn explicit_cross_join_flattens_like_a_comma() {
        let LogicalPlan::Join { inputs, .. } =
            bind_one("SELECT * FROM (VALUES (1)) a(x) CROSS JOIN (VALUES (2)) b(y)").unwrap()
        else {
            panic!("expected Join");
        };
        assert_eq!(inputs.len(), 2);
    }

    #[test]
    fn inner_join_with_condition_stays_0a000() {
        let e = bind_err("SELECT * FROM t a JOIN t b ON a.id = b.id");
        assert_eq!(e.code, "0A000");
        assert_eq!(e.message, "JOIN is not supported yet");
    }

    #[test]
    fn where_referencing_both_relations_binds() {
        let LogicalPlan::Join { predicate, .. } =
            bind_one("SELECT a.x FROM (VALUES (1)) a(x), (VALUES (1)) b(y) WHERE a.x = b.y")
                .unwrap()
        else {
            panic!("expected Join");
        };
        assert!(predicate.is_some());
    }

    #[test]
    fn duplicate_column_within_relation_is_ambiguous_42702() {
        // A duplicate column alias makes a reference ambiguous, as in PG —
        // whether unqualified or qualified into that relation.
        let e = bind_err("SELECT x FROM (VALUES (1, 2)) a(x, x)");
        assert_eq!(e.code, "42702");
        assert_eq!(e.message, "column reference \"x\" is ambiguous");
        let e = bind_err("SELECT a.x FROM (VALUES (1, 2)) a(x, x)");
        assert_eq!(e.code, "42702");
        assert_eq!(e.message, "column reference \"x\" is ambiguous");
    }

    #[test]
    fn qualified_missing_column_names_the_qualifier() {
        // PG prints `column q.c does not exist` for a qualified reference,
        // unquoted and qualifier-prefixed (contrast the unqualified form).
        let e = bind_err("SELECT x.nope FROM t x");
        assert_eq!(e.code, "42703");
        assert_eq!(e.message, "column x.nope does not exist");
    }

    /// A single-table SELECT's projections and sort keys.
    fn query_parts(sql: &str) -> (Vec<BoundExpr>, Vec<SortKey>) {
        match bind_one(sql).unwrap() {
            LogicalPlan::Query {
                projections, sort, ..
            } => (projections, sort),
            _ => panic!("expected Query for {sql}, got another plan variant"),
        }
    }

    #[test]
    fn order_by_ordinal_carries_type_and_direction() {
        // `ORDER BY 2 DESC` → second output column (id, int4), descending, and
        // the PG default NULLS FIRST for a descending sort.
        let (projections, sort) = query_parts("SELECT name, id FROM t ORDER BY 2 DESC");
        assert_eq!(projections.len(), 2, "no hidden column for an ordinal");
        assert_eq!(
            sort,
            vec![SortKey {
                column: 1,
                ty: PgType::Int4,
                asc: false,
                nulls_first: true,
            }]
        );
    }

    #[test]
    fn order_by_output_name_resolves_to_visible_column() {
        // A bare name matches a select-list output name first (SQL92).
        let (projections, sort) = query_parts("SELECT name, id FROM t ORDER BY name");
        assert_eq!(projections.len(), 2);
        assert_eq!(
            sort,
            vec![SortKey {
                column: 0,
                ty: PgType::Text,
                asc: true,
                nulls_first: false,
            }]
        );
    }

    #[test]
    fn order_by_alias_resolves_to_its_column() {
        let (projections, sort) = query_parts("SELECT id + big AS s FROM t ORDER BY s");
        assert_eq!(projections.len(), 1);
        assert_eq!(sort[0].column, 0);
        assert_eq!(sort[0].ty, PgType::Int8);
    }

    #[test]
    fn order_by_nonselected_column_appends_hidden() {
        // `big` is not in the select list, so it becomes a hidden column past
        // the single visible output. Its type drives comparison.
        let (projections, sort) = query_parts("SELECT id FROM t ORDER BY big");
        assert_eq!(projections.len(), 2, "one hidden column appended");
        assert_eq!(
            projections[1],
            BoundExpr::ColumnRef {
                index: 1,
                ty: PgType::Int8
            }
        );
        assert_eq!(sort[0].column, 1);
        assert_eq!(sort[0].ty, PgType::Int8);
    }

    #[test]
    fn order_by_expression_reuses_equal_projection() {
        // `ORDER BY id + big` equals the sole projection `id + big`, so it is
        // reused rather than appended (PG's target-entry reuse).
        let (projections, sort) = query_parts("SELECT id + big AS s FROM t ORDER BY id + big");
        assert_eq!(projections.len(), 1, "reused, not appended");
        assert_eq!(sort[0].column, 0);
    }

    #[test]
    fn order_by_qualified_name_binds_as_expression() {
        // A qualified name skips the output-name match and binds against the
        // FROM scope, appending a hidden column when not selected.
        let (projections, sort) = query_parts("SELECT id FROM t ORDER BY t.name");
        assert_eq!(projections.len(), 2);
        assert_eq!(sort[0].column, 1);
        assert_eq!(sort[0].ty, PgType::Text);
    }

    #[test]
    fn order_by_ambiguous_output_name_is_42702() {
        let e = bind_err("SELECT id AS c, big AS c FROM t ORDER BY c");
        assert_eq!(e.code, "42702");
        assert_eq!(e.message, "ORDER BY \"c\" is ambiguous");
    }

    #[test]
    fn order_by_alias_not_visible_inside_expression() {
        // A top-level bare alias resolves, but inside an expression the alias is
        // invisible — PG reports the underlying column as undefined.
        let e = bind_err("SELECT 1 AS a ORDER BY a + 1");
        assert_eq!(e.code, "42703");
    }

    #[test]
    fn order_by_upper_of_column_binds() {
        let (projections, sort) = query_parts("SELECT id FROM t ORDER BY upper(name)");
        assert_eq!(projections.len(), 2);
        assert_eq!(sort[0].column, 1);
        assert_eq!(sort[0].ty, PgType::Text);
    }

    #[test]
    fn values_order_by_column_name_resolves() {
        let LogicalPlan::Values { sort, .. } = bind_one("VALUES (3), (1) ORDER BY column1").unwrap()
        else {
            panic!("expected Values");
        };
        assert_eq!(sort[0].column, 0);
        assert_eq!(sort[0].ty, PgType::Int4);
    }

    #[test]
    fn values_order_by_expression_stays_0a000() {
        // A standalone VALUES list has no projection tuple to hang a hidden
        // column on, so expression sort keys are still unsupported.
        let e = bind_err("VALUES (3), (1) ORDER BY column1 + 1");
        assert_eq!(e.code, "0A000");
    }

    #[test]
    fn limit_offset_wraps_body() {
        let LogicalPlan::Limit {
            source,
            limit,
            offset,
        } = bind_one("SELECT id FROM t LIMIT 5 OFFSET 2").unwrap()
        else {
            panic!("expected Limit");
        };
        assert_eq!(limit, Some(5));
        assert_eq!(offset, Some(2));
        assert!(matches!(*source, LogicalPlan::Query { .. }));
    }

    #[test]
    fn offset_zero_is_a_bare_offset() {
        // The float4/float8 optimization-fence shape: `OFFSET 0`, no LIMIT.
        let LogicalPlan::Limit { limit, offset, .. } =
            bind_one("SELECT id FROM t OFFSET 0").unwrap()
        else {
            panic!("expected Limit");
        };
        assert_eq!(limit, None);
        assert_eq!(offset, Some(0));
    }

    #[test]
    fn limit_all_is_no_bound() {
        // `LIMIT ALL OFFSET 3` carries only the offset; the limit is unbounded.
        let LogicalPlan::Limit { limit, offset, .. } =
            bind_one("SELECT id FROM t LIMIT ALL OFFSET 3").unwrap()
        else {
            panic!("expected Limit");
        };
        assert_eq!(limit, None);
        assert_eq!(offset, Some(3));
    }

    #[test]
    fn offset_in_derived_table_wraps_subplan() {
        // `OFFSET 0` inside a FROM subquery binds as a Limit at that level.
        let LogicalPlan::Subquery { source, .. } =
            bind_one("SELECT * FROM (SELECT id FROM t OFFSET 0) s").unwrap()
        else {
            panic!("expected Subquery");
        };
        assert!(matches!(*source, LogicalPlan::Limit { .. }));
    }

    #[test]
    fn negative_limit_and_offset_rejected() {
        assert_eq!(bind_err("SELECT id FROM t LIMIT -1").code, "2201W");
        assert_eq!(bind_err("SELECT id FROM t OFFSET -1").code, "2201X");
    }

    #[test]
    fn non_constant_limit_stays_0a000() {
        let e = bind_err("SELECT id FROM t LIMIT id");
        assert_eq!(e.code, "0A000");
    }
}
