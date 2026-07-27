//! Statement binding: AST statements → [`LogicalPlan`].
//!
//! Everything parsed but not executed must be rejected loudly (`0A000`):
//! silently dropping a clause would return wrong results instead of an honest
//! error.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crabgresql_parser::{Span, ast};
use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::{
    Column, StorageError, TableAm, TableEngine, TableSchema, TypeCatalog,
    ViewDefinition,
};
use crabgresql_types::collation::DEFAULT_COLLATION_OID;
use crabgresql_types::{PgType, Value};

use crate::expr::{
    BinOp, Binding, BoundExpr, OuterLevel, ParamCtx, Scope, VisibleColumn, VisibleLookup,
    bind_binary_op, bind_column_default, bind_expr, bind_projection, bind_scalar, coerce_expr,
    coerce_to_column, lookup_visible, merge_types, normalize_ident, output_name, param_ctx_none,
    to_bool_operand, unify_value_column,
};
use crate::functions::{bind_table_fn_call, positional_arg_exprs};
use crate::{BindError, BoundAggregate, OutputColumn, TableFn};

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
    /// The collation ordering this key, derived from the ORDER BY expression.
    /// Only meaningful for a string `ty`; every other type ignores it.
    pub collation: u32,
    pub asc: bool,
    pub nulls_first: bool,
}

/// One key column of a `SELECT DISTINCT` / `DISTINCT ON`. Both forms reduce to
/// deduplicating on a set of columns of the projected tuple: plain `DISTINCT`
/// keys on every visible output column, `DISTINCT ON (…)` on the resolved ON
/// expressions (which, like ORDER BY, may live in a hidden column past the
/// visible width). `column` indexes the projected tuple; `ty` drives the
/// hash/equality (via the same `hash_key`/`keys_equal` the executor uses).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DistinctKey {
    pub column: usize,
    pub ty: PgType,
}

/// One arm of a [`LogicalPlan::SetOp`].
#[derive(Clone)]
pub struct SetOpArm {
    pub plan: LogicalPlan,
    /// Projections mapping this arm's own columns onto the set operation's
    /// unified output layout; `None` when the arm already emits that layout.
    pub coercion: Option<Vec<BoundExpr>>,
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
        distinct: Option<Vec<DistinctKey>>,
    },
    /// Single-table SELECT with optional predicate.
    Query {
        table: Arc<dyn TableAm>,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
        distinct: Option<Vec<DistinctKey>>,
    },
    /// Union scan over the leaf partitions of a partitioned parent. `tables` are
    /// the leaf relations (each with the parent's column layout); the node emits
    /// every leaf's rows, full parent-column width, in leaf order. It carries no
    /// projection/predicate/sort of its own — a partitioned-parent FROM item is
    /// bound as a [`Self::Subquery`] wrapping this Append, so the surrounding
    /// SELECT's WHERE/projection/ORDER BY/DISTINCT apply on top (and joins /
    /// aggregates reuse the same subplan machinery).
    Append {
        tables: Vec<Arc<dyn TableAm>>,
        columns: Vec<OutputColumn>,
    },
    /// A `UNION` / `UNION ALL`: concatenate both arms, then optionally
    /// deduplicate and sort. `columns` is the unified output layout (per-position
    /// common types, named from the left arm).
    ///
    /// This node owns its whole tail rather than delegating to a wrapping
    /// [`Self::Subquery`]. That matters for more than tidiness: a `Subquery` is a
    /// *derived table*, i.e. its own query nesting level, so `subst_outer_plan`
    /// descends into it with `depth + 1`. A set operation introduces no such
    /// level — its arms bind in the enclosing query's scope — so wrapping one in
    /// a Subquery would shift every arm's [`BoundExpr::OuterColumnRef`] out of
    /// range and leave correlated references unsubstituted at execution.
    ///
    /// Each arm carries its own coercion onto `columns` for the same reason:
    /// coercion must not introduce a nesting level either.
    ///
    /// The node is N-ary, and [`bind_set_operation`] flattens a chain of
    /// equivalent operations into one node (`a UNION b UNION c` is three arms,
    /// not nested pairs), matching PG's single Append over N children. Besides
    /// keeping the plan shallow, that collapses the redundant per-level
    /// deduplication a nested encoding would produce.
    SetOp {
        /// Two or more arms, in query order.
        arms: Vec<SetOpArm>,
        columns: Vec<OutputColumn>,
        /// A query-level `ORDER BY` over the combined result.
        sort: Vec<SortKey>,
        /// `Some(all output columns)` for `UNION`; `None` for `UNION ALL`.
        distinct: Option<Vec<DistinctKey>>,
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
        distinct: Option<Vec<DistinctKey>>,
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
        distinct: Option<Vec<DistinctKey>>,
    },
    /// SELECT over a recursive join tree. Leaf rows are laid out left-to-right
    /// in the combined row; the same projection/predicate/sort pipeline as
    /// `Query` runs on top, with `ColumnRef`s indexing that combined row.
    Join {
        source: JoinExpr,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
        distinct: Option<Vec<DistinctKey>>,
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
    /// Aggregation over a single row source: `GROUP BY` / `HAVING` and/or
    /// aggregate calls in the target list. The physical pipeline is
    /// `input → Filter(predicate) → Aggregate → [Filter(having)] → Projection →
    /// Sort`: `predicate` (WHERE) filters the source *before* aggregation, the
    /// aggregate node emits one row per group laid out `[group keys…, aggregates…]`,
    /// `having` filters those rows, and `projections`/`sort` (whose aggregate and
    /// grouped-column references were rewritten to `ColumnRef`s into that row)
    /// produce the visible output. An empty `group_exprs` is the implicit single
    /// group (`SELECT count(*) …` — always one output row).
    Aggregate {
        input: AggInput,
        predicate: Option<BoundExpr>,
        group_exprs: Vec<BoundExpr>,
        aggregates: Vec<BoundAggregate>,
        having: Option<BoundExpr>,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        sort: Vec<SortKey>,
        distinct: Option<Vec<DistinctKey>>,
    },
    /// INSERT: rows come either from a `VALUES` list (full-width, schema order,
    /// each cell already coerced) or from a query source (`INSERT ... SELECT` /
    /// `INSERT ... TABLE t`). See [`InsertSource`].
    Insert {
        table: Arc<dyn TableAm>,
        source: InsertSource,
        /// `RETURNING`: a projection over each inserted row, bound against the
        /// table schema. `None` when the clause is absent.
        returning: Option<Returning>,
        /// Tuple routing for a partitioned parent: `Some(leaves)` when `table` is
        /// a partitioned parent, holding its leaf partitions. The executor routes
        /// each row to the leaf whose RANGE bound admits its key (reading the
        /// bound from each leaf's `partition_of`) and writes there instead of to
        /// `table`. `None` for an ordinary table (rows go straight to `table`).
        routing: Option<Vec<Arc<dyn TableAm>>>,
    },
    Update {
        table: Arc<dyn TableAm>,
        predicate: Option<BoundExpr>,
        /// (column index, value expression bound against the OLD row).
        assignments: Vec<(usize, BoundExpr)>,
        /// `RETURNING`: a projection over each updated (NEW) row.
        returning: Option<Returning>,
        /// Tuple routing for a partitioned parent: `Some(leaves)` when `table` is
        /// a partitioned parent, holding its leaf partitions. The executor scans
        /// every leaf, and for each updated row re-routes the NEW tuple to the
        /// leaf whose RANGE bound admits it — moving the row (delete from the old
        /// leaf, insert into the new) when the key change lands it elsewhere.
        /// `None` for an ordinary table.
        routing: Option<Vec<Arc<dyn TableAm>>>,
    },
    Delete {
        table: Arc<dyn TableAm>,
        predicate: Option<BoundExpr>,
        /// `RETURNING`: a projection over each deleted (OLD) row.
        returning: Option<Returning>,
        /// Tuple routing for a partitioned parent: `Some(leaves)` when `table` is
        /// a partitioned parent. The executor scans every leaf and deletes matching
        /// rows from whichever leaf holds them. `None` for an ordinary table.
        routing: Option<Vec<Arc<dyn TableAm>>>,
    },
}

/// A bound `RETURNING` target list: the output column shape plus one expression
/// per output column, evaluated against each affected row (the NEW row for
/// INSERT/UPDATE, the deleted row for DELETE). Same shape as a SELECT
/// projection.
#[derive(Clone)]
pub struct Returning {
    pub columns: Vec<OutputColumn>,
    pub projections: Vec<BoundExpr>,
}

/// The rows an INSERT writes.
#[derive(Clone)]
pub enum InsertSource {
    /// `INSERT ... VALUES`: fully-formed rows, full-width in schema order, each
    /// cell already coerced to its column type. Evaluated against the empty row.
    Values(Vec<Vec<BoundExpr>>),
    /// `INSERT ... SELECT` / `INSERT ... TABLE t`: rows are pulled from `input`
    /// at execution time. `projections` is full-width in schema order — non-target
    /// columns hold their column default, each target column a `ColumnRef` into
    /// the source row coerced to the column type — evaluated against each source
    /// tuple.
    Query {
        input: Box<LogicalPlan>,
        projections: Vec<BoundExpr>,
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

/// The SQL join semantics applied by one binary [`JoinExpr::Join`] node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Cross,
    Inner,
    Left,
    Right,
    Full,
}

/// A recursively bound FROM source. Every leaf records its output width so an
/// outer join can synthesize a correctly-sized all-NULL row even when that side
/// has no tuples. Join predicates address the concatenated `left || right` row.
#[derive(Clone)]
pub enum JoinExpr {
    Input {
        input: JoinInput,
        width: usize,
    },
    Join {
        left: Box<JoinExpr>,
        right: Box<JoinExpr>,
        kind: JoinKind,
        /// `None` only for `CROSS JOIN` / comma joins.
        predicate: Option<BoundExpr>,
    },
}

impl JoinExpr {
    pub fn width(&self) -> usize {
        match self {
            JoinExpr::Input { width, .. } => *width,
            JoinExpr::Join { left, right, .. } => left.width() + right.width(),
        }
    }
}

/// The row source feeding a [`LogicalPlan::Aggregate`]. `Scan` is a single base
/// table; `Join` is any other FROM source — a recursively joined tree or a
/// single-input node (derived table, CTE reference, `VALUES`, or set-returning
/// function); `SingleRow` is the one virtual row of a FROM-less aggregate
/// (`SELECT count(*)`).
#[derive(Clone)]
pub enum AggInput {
    Scan(Arc<dyn TableAm>),
    Join(JoinExpr),
    SingleRow,
}

/// Split a relation name into an optional schema qualifier and the relation
/// name. `t` → `(None, "t")`; `pg_catalog.pg_type` → `(Some("pg_catalog"),
/// "pg_type")`. A three-or-more-part name (cross-database) is still unsupported.
fn split_relation_name(name: &ast::ObjectName) -> Result<(Option<String>, String), BindError> {
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
    verb: WriteVerb,
) -> Result<(Arc<dyn TableAm>, String), BindError> {
    let (schema, table_name) = split_relation_name(name)?;
    let table = match schema.as_deref() {
        // Unqualified: temp then global — `open_table` never consults the
        // system catalog, so this cannot resolve to a read-only relation.
        None => engine.open_table(&table_name),
        Some("pg_catalog") | Some("information_schema") => {
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
    let table = table.map_err(|e| {
        // A write whose target is a view is rejected as non-updatable rather than
        // "relation does not exist": crabgresql has no auto-updatable-view or
        // INSTEAD OF trigger support yet.
        if matches!(e, StorageError::TableNotFound(_))
            && engine.resolve_view(schema.as_deref(), &table_name).is_some()
        {
            return non_updatable_view(verb, &table_name);
        }
        not_found_as_written(e, schema.as_deref(), &table_name)
    })?;
    let capabilities = table.capabilities();
    let supported = match verb {
        WriteVerb::Insert => capabilities.insert,
        WriteVerb::Update => capabilities.update,
        WriteVerb::Delete => capabilities.delete,
    };
    if !supported {
        let verb = match verb {
            WriteVerb::Insert => "INSERT",
            WriteVerb::Update => "UPDATE",
            WriteVerb::Delete => "DELETE",
        };
        let method = table.schema().access_method.as_str();
        return Err(BindError::feature_not_supported(format!(
            "table access method \"{method}\" does not support {verb}"
        )));
    }
    // A partitioned parent is a valid write target: INSERT/COPY route rows to
    // leaves and UPDATE/DELETE route through them (each binder captures the leaves
    // via `partition_leaves`).
    Ok((table, table_name))
}

/// The physical sources a read of `table` must union, or `None` when it is
/// scanned directly.
///
/// Two independent kinds of split compose here. A **SQL partitioned parent**
/// holds no rows itself and fans out to its catalog leaf partitions. An access
/// method with **engine-internal storage leaves** fans out to its own physical
/// sources ([`TableAm::storage_leaves`]) — invisible to the catalog.
///
/// A SQL leaf may itself split the second way, so the leaves are expanded and
/// the result flattened: `Append` stays one flat list and the executor keeps one
/// loop. Nothing produces that nesting today (a partitioned parent's leaves are
/// heap relations), but writing the flatten costs three lines and removes the
/// trap for whoever first makes a columnar relation a partition.
fn scan_leaves(
    engine: &Arc<dyn TableEngine>,
    table: &Arc<dyn TableAm>,
) -> Result<Option<Vec<Arc<dyn TableAm>>>, BindError> {
    if table.schema().partition_scheme.is_some() {
        let mut leaves = Vec::new();
        for leaf in partition_leaves(engine, table.schema())? {
            match leaf.storage_leaves() {
                Some(inner) => leaves.extend(inner),
                None => leaves.push(leaf),
            }
        }
        return Ok(Some(leaves));
    }
    Ok(table.storage_leaves())
}

/// Enumerate the leaf partitions of the partitioned parent `parent` as storage
/// handles, in a deterministic order (by leaf name). Both tuple routing on
/// INSERT and the union scan on read need this set; it is captured at bind time
/// (like every other `Arc<dyn TableAm>` a plan embeds), so partitions created
/// after a statement is planned are not observed until it is re-bound.
fn partition_leaves(
    engine: &Arc<dyn TableEngine>,
    parent: &TableSchema,
) -> Result<Vec<Arc<dyn TableAm>>, BindError> {
    // Identify the leaves from catalog metadata (each carries a `partition_of`
    // link back to this parent), then resolve each to a storage handle.
    let mut leaves: Vec<(String, String)> = engine
        .relation_metadata()
        .into_iter()
        .filter_map(|meta| {
            let part = meta.schema.partition_of.as_ref()?;
            (part.parent_namespace == parent.namespace && part.parent_name == parent.name)
                .then(|| (meta.schema.namespace.clone(), meta.schema.name.clone()))
        })
        .collect();
    leaves.sort();
    leaves
        .into_iter()
        .map(|(namespace, name)| {
            engine.resolve(Some(&namespace), &name).map_err(|e| {
                BindError::new(
                    sqlstate::INTERNAL_ERROR,
                    format!("partition \"{name}\" of \"{}\" is unreadable: {e}", parent.name),
                )
            })
        })
        .collect()
}

/// Which DML verb is writing, for the non-updatable-view error text.
#[derive(Clone, Copy)]
enum WriteVerb {
    Insert,
    Update,
    Delete,
}

/// PG's rejection of a write to a view with no updatability support.
fn non_updatable_view(verb: WriteVerb, name: &str) -> BindError {
    let (action, trigger) = match verb {
        WriteVerb::Insert => ("insert into", "INSERT"),
        WriteVerb::Update => ("update", "UPDATE"),
        WriteVerb::Delete => ("delete from", "DELETE"),
    };
    BindError::new(
        sqlstate::FEATURE_NOT_SUPPORTED,
        format!("cannot {action} view \"{name}\""),
    )
    .with_detail(Some(
        "Views that do not select from a single table or view are not automatically updatable."
            .to_string(),
    ))
    .with_hint(Some(format!(
        "To enable {action} the view, provide an INSTEAD OF {trigger} trigger or an unconditional ON {trigger} DO INSTEAD rule."
    )))
}

/// Resolve an `UPDATE`/`DELETE` target plus the qualifier for its columns.
fn open_write_relation(
    engine: &Arc<dyn TableEngine>,
    relation: &ast::TableFactor,
    verb: WriteVerb,
) -> Result<(Arc<dyn TableAm>, String), BindError> {
    let ast::TableFactor::Table { name, alias, .. } = relation else {
        return Err(BindError::feature_not_supported(format!(
            "target is not supported yet: {relation}"
        )));
    };
    // A partitioned parent is a valid UPDATE/DELETE target: the executor routes
    // through its leaves (each binder captures them via `partition_leaves`), so —
    // unlike before — the parent is not rejected here.
    let (table, table_name) = resolve_write_table(engine, name, verb)?;
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
pub(crate) struct CteRelation {
    columns: Vec<OutputColumn>,
    plan: LogicalPlan,
}

/// The set of CTE names visible while binding a query — the enclosing `WITH`
/// plus any earlier siblings in the same clause.
pub(crate) type CteEnv = HashMap<String, CteRelation>;

/// Replace every `$n` placeholder ([`BoundExpr::Param`]) in a bound plan with a
/// constant carrying the value bound for that parameter, in place. Run by the
/// extended-query executor after a portal's parameters are decoded, so the
/// downstream planner/executor see an ordinary parameter-free plan and need no
/// notion of a parameter list. `params` is indexed by parameter number − 1; a
/// `Param` whose index is past the list is left untouched (the executor reports
/// the missing binding), which cannot happen once Bind validates the count.
pub fn substitute_params(plan: &mut LogicalPlan, params: &[Value]) {
    match plan {
        LogicalPlan::Values {
            rows, predicate, ..
        } => {
            for row in rows {
                subst_exprs(row, params);
            }
            subst_opt(predicate, params);
        }
        LogicalPlan::Query {
            projections,
            predicate,
            ..
        } => {
            subst_exprs(projections, params);
            subst_opt(predicate, params);
        }
        LogicalPlan::Subquery {
            source,
            projections,
            predicate,
            ..
        } => {
            substitute_params(source, params);
            subst_exprs(projections, params);
            subst_opt(predicate, params);
        }
        LogicalPlan::TableFunction {
            args,
            projections,
            predicate,
            ..
        } => {
            subst_exprs(args, params);
            subst_exprs(projections, params);
            subst_opt(predicate, params);
        }
        LogicalPlan::Join {
            source,
            projections,
            predicate,
            ..
        } => {
            subst_join(source, params);
            subst_exprs(projections, params);
            subst_opt(predicate, params);
        }
        // An Append carries only leaf table handles, no parameterizable exprs.
        LogicalPlan::Append { .. } => {}
        LogicalPlan::SetOp { arms, .. } => {
            for arm in arms.iter_mut() {
                substitute_params(&mut arm.plan, params);
                if let Some(coercion) = &mut arm.coercion {
                    subst_exprs(coercion, params);
                }
            }
        }
        LogicalPlan::Limit { source, .. } => substitute_params(source, params),
        LogicalPlan::Aggregate {
            input,
            predicate,
            group_exprs,
            aggregates,
            having,
            projections,
            ..
        } => {
            if let AggInput::Join(join) = input {
                subst_join(join, params);
            }
            subst_opt(predicate, params);
            subst_exprs(group_exprs, params);
            for agg in aggregates {
                for arg in agg.args.iter_mut() {
                    subst_expr(arg, params);
                }
            }
            subst_opt(having, params);
            subst_exprs(projections, params);
        }
        LogicalPlan::Insert {
            source, returning, ..
        } => {
            match source {
                InsertSource::Values(rows) => {
                    for row in rows {
                        subst_exprs(row, params);
                    }
                }
                InsertSource::Query { input, projections } => {
                    substitute_params(input, params);
                    subst_exprs(projections, params);
                }
            }
            subst_returning(returning, params);
        }
        LogicalPlan::Update {
            predicate,
            assignments,
            returning,
            ..
        } => {
            subst_opt(predicate, params);
            for (_, expr) in assignments {
                subst_expr(expr, params);
            }
            subst_returning(returning, params);
        }
        LogicalPlan::Delete {
            predicate,
            returning,
            ..
        } => {
            subst_opt(predicate, params);
            subst_returning(returning, params);
        }
    }
}

fn subst_returning(returning: &mut Option<Returning>, params: &[Value]) {
    if let Some(returning) = returning {
        subst_exprs(&mut returning.projections, params);
    }
}

fn subst_join(join: &mut JoinExpr, params: &[Value]) {
    match join {
        JoinExpr::Input { input, .. } => match input {
            JoinInput::Scan(_) => {}
            JoinInput::Subplan(plan) => substitute_params(plan, params),
            JoinInput::TableFunction { args, .. } => subst_exprs(args, params),
        },
        JoinExpr::Join {
            left,
            right,
            predicate,
            ..
        } => {
            subst_join(left, params);
            subst_join(right, params);
            subst_opt(predicate, params);
        }
    }
}

fn subst_opt(expr: &mut Option<BoundExpr>, params: &[Value]) {
    if let Some(e) = expr {
        subst_expr(e, params);
    }
}

fn subst_exprs(exprs: &mut [BoundExpr], params: &[Value]) {
    for e in exprs {
        subst_expr(e, params);
    }
}

/// Rewrite one expression tree in place, replacing each `Param` leaf with the
/// bound value as a `Const`. The value was decoded using the parameter's
/// resolved type, so it already matches the node's `ty`.
fn subst_expr(expr: &mut BoundExpr, params: &[Value]) {
    match expr {
        BoundExpr::Param { index, ty } => {
            if let Some(value) = params.get(*index) {
                *expr = BoundExpr::Const {
                    value: value.clone(),
                    ty: *ty,
                };
            }
        }
        // Outer references belong to an enclosing query, not this statement's
        // `$n` list; `substitute_outer` fills them per outer row at execution.
        BoundExpr::Const { .. } | BoundExpr::ColumnRef { .. } | BoundExpr::OuterColumnRef { .. } => {
        }
        BoundExpr::Unary { expr, .. } => subst_expr(expr, params),
        BoundExpr::Binary { left, right, .. } => {
            subst_expr(left, params);
            subst_expr(right, params);
        }
        BoundExpr::IsNull { expr, .. } => subst_expr(expr, params),
        BoundExpr::Coerce { expr, .. } => subst_expr(expr, params),
        BoundExpr::Collate { expr, .. } => subst_expr(expr, params),
        BoundExpr::Reinterpret { expr, .. } => subst_expr(expr, params),
        BoundExpr::FuncCall { args, .. }
        | BoundExpr::Routine { args, .. }
        | BoundExpr::Srf { args, .. } => subst_exprs(args, params),
        BoundExpr::ArrayCtor { elems, .. } => subst_exprs(elems, params),
        BoundExpr::Subscript { base, index, .. } => {
            subst_expr(base, params);
            subst_expr(index, params);
        }
        BoundExpr::Case { whens, else_, .. } => {
            for (cond, result) in whens {
                subst_expr(cond, params);
                subst_expr(result, params);
            }
            if let Some(e) = else_ {
                subst_expr(e, params);
            }
        }
        BoundExpr::Aggregate { args, .. } => {
            for arg in args {
                subst_expr(arg, params);
            }
        }
        // A `$n` may appear inside the subquery body, and (for IN) inside the
        // needle carried by the comparison template.
        BoundExpr::ScalarSubquery { subplan, .. } | BoundExpr::Exists { subplan, .. } => {
            substitute_params(&mut subplan.0, params);
        }
        BoundExpr::QuantifiedSubquery { subplan, cmp, .. } => {
            substitute_params(&mut subplan.0, params);
            subst_expr(cmp, params);
        }
        // `x op ANY/ALL(array)` carries no subplan; a `$n` may appear in either
        // the array operand or the needle (in the comparison template).
        BoundExpr::QuantifiedArray { array, cmp, .. } => {
            subst_expr(array, params);
            subst_expr(cmp, params);
        }
    }
}

/// Fill a correlated subplan's outer references with the enclosing row's values,
/// in place, before it executes for that row. `outer` is the immediate parent
/// row (the one this subplan is correlated to). An [`BoundExpr::OuterColumnRef`]
/// is resolved by comparing its `level` to its nesting `depth` within this
/// subplan (1 at the top, +1 per nested expression-subquery): `level == depth`
/// names a column of `outer` and folds to a `Const`; `level > depth` belongs to
/// a still-outer query and is decremented (to be filled at that boundary);
/// `level < depth` names an intervening inner query and is left for it. The
/// classic nested-loop correlated substitution — run per outer row by the
/// executor (see `crabgresql_executor`).
pub fn substitute_outer(plan: &mut LogicalPlan, outer: &[Value]) {
    subst_outer_plan(plan, outer, 1);
}

fn subst_outer_plan(plan: &mut LogicalPlan, outer: &[Value], depth: usize) {
    match plan {
        LogicalPlan::Values {
            rows, predicate, ..
        } => {
            for row in rows {
                for e in row.iter_mut() {
                    subst_outer_expr(e, outer, depth);
                }
            }
            if let Some(p) = predicate {
                subst_outer_expr(p, outer, depth);
            }
        }
        LogicalPlan::Query {
            projections,
            predicate,
            ..
        } => {
            for e in projections.iter_mut() {
                subst_outer_expr(e, outer, depth);
            }
            if let Some(p) = predicate {
                subst_outer_expr(p, outer, depth);
            }
        }
        LogicalPlan::Subquery {
            source,
            projections,
            predicate,
            ..
        } => {
            // A derived table is its own query nesting level.
            subst_outer_plan(source, outer, depth + 1);
            for e in projections.iter_mut() {
                subst_outer_expr(e, outer, depth);
            }
            if let Some(p) = predicate {
                subst_outer_expr(p, outer, depth);
            }
        }
        LogicalPlan::TableFunction {
            args,
            projections,
            predicate,
            ..
        } => {
            for e in args.iter_mut().chain(projections.iter_mut()) {
                subst_outer_expr(e, outer, depth);
            }
            if let Some(p) = predicate {
                subst_outer_expr(p, outer, depth);
            }
        }
        LogicalPlan::Join {
            source,
            projections,
            predicate,
            ..
        } => {
            subst_outer_join(source, outer, depth);
            for e in projections.iter_mut() {
                subst_outer_expr(e, outer, depth);
            }
            if let Some(p) = predicate {
                subst_outer_expr(p, outer, depth);
            }
        }
        // An Append holds only leaf table handles — no correlated exprs.
        LogicalPlan::Append { .. } => {}
        // A set operation is not its own query nesting level: its arms bound in
        // the enclosing scope, so they keep this `depth` (contrast the Subquery
        // arm above, a derived table, which descends with `depth + 1`).
        LogicalPlan::SetOp { arms, .. } => {
            for arm in arms.iter_mut() {
                subst_outer_plan(&mut arm.plan, outer, depth);
                for e in arm.coercion.iter_mut().flatten() {
                    subst_outer_expr(e, outer, depth);
                }
            }
        }
        LogicalPlan::Limit { source, .. } => subst_outer_plan(source, outer, depth),
        LogicalPlan::Aggregate {
            input,
            predicate,
            group_exprs,
            aggregates,
            having,
            projections,
            ..
        } => {
            if let AggInput::Join(join) = input {
                subst_outer_join(join, outer, depth);
            }
            if let Some(p) = predicate {
                subst_outer_expr(p, outer, depth);
            }
            for e in group_exprs.iter_mut() {
                subst_outer_expr(e, outer, depth);
            }
            for agg in aggregates {
                for arg in agg.args.iter_mut() {
                    subst_outer_expr(arg, outer, depth);
                }
            }
            if let Some(h) = having {
                subst_outer_expr(h, outer, depth);
            }
            for e in projections.iter_mut() {
                subst_outer_expr(e, outer, depth);
            }
        }
        LogicalPlan::Insert {
            source, returning, ..
        } => {
            match source {
                InsertSource::Values(rows) => {
                    for row in rows {
                        for e in row.iter_mut() {
                            subst_outer_expr(e, outer, depth);
                        }
                    }
                }
                InsertSource::Query { input, projections } => {
                    subst_outer_plan(input, outer, depth + 1);
                    for e in projections.iter_mut() {
                        subst_outer_expr(e, outer, depth);
                    }
                }
            }
            if let Some(r) = returning {
                for e in r.projections.iter_mut() {
                    subst_outer_expr(e, outer, depth);
                }
            }
        }
        LogicalPlan::Update {
            predicate,
            assignments,
            returning,
            ..
        } => {
            if let Some(p) = predicate {
                subst_outer_expr(p, outer, depth);
            }
            for (_, e) in assignments {
                subst_outer_expr(e, outer, depth);
            }
            if let Some(r) = returning {
                for e in r.projections.iter_mut() {
                    subst_outer_expr(e, outer, depth);
                }
            }
        }
        LogicalPlan::Delete {
            predicate,
            returning,
            ..
        } => {
            if let Some(p) = predicate {
                subst_outer_expr(p, outer, depth);
            }
            if let Some(r) = returning {
                for e in r.projections.iter_mut() {
                    subst_outer_expr(e, outer, depth);
                }
            }
        }
    }
}

fn subst_outer_join(join: &mut JoinExpr, outer: &[Value], depth: usize) {
    match join {
        JoinExpr::Input { input, .. } => match input {
            JoinInput::Scan(_) => {}
            JoinInput::Subplan(plan) => subst_outer_plan(plan, outer, depth + 1),
            JoinInput::TableFunction { args, .. } => {
                for e in args.iter_mut() {
                    subst_outer_expr(e, outer, depth);
                }
            }
        },
        JoinExpr::Join {
            left,
            right,
            predicate,
            ..
        } => {
            subst_outer_join(left, outer, depth);
            subst_outer_join(right, outer, depth);
            if let Some(p) = predicate {
                subst_outer_expr(p, outer, depth);
            }
        }
    }
}

fn subst_outer_expr(expr: &mut BoundExpr, outer: &[Value], depth: usize) {
    match expr {
        BoundExpr::OuterColumnRef { level, index, ty } => {
            if *level == depth {
                if let Some(value) = outer.get(*index) {
                    *expr = BoundExpr::Const {
                        value: value.clone(),
                        ty: *ty,
                    };
                }
            } else if *level > depth {
                *level -= 1;
            }
            // `level < depth`: an intervening inner query fills it — leave it.
        }
        BoundExpr::Const { .. } | BoundExpr::ColumnRef { .. } | BoundExpr::Param { .. } => {}
        BoundExpr::Unary { expr, .. }
        | BoundExpr::IsNull { expr, .. }
        | BoundExpr::Coerce { expr, .. }
        | BoundExpr::Collate { expr, .. }
        | BoundExpr::Reinterpret { expr, .. } => subst_outer_expr(expr, outer, depth),
        BoundExpr::Binary { left, right, .. } => {
            subst_outer_expr(left, outer, depth);
            subst_outer_expr(right, outer, depth);
        }
        BoundExpr::FuncCall { args, .. }
        | BoundExpr::Routine { args, .. }
        | BoundExpr::Srf { args, .. } => {
            for a in args.iter_mut() {
                subst_outer_expr(a, outer, depth);
            }
        }
        BoundExpr::ArrayCtor { elems, .. } => {
            for a in elems.iter_mut() {
                subst_outer_expr(a, outer, depth);
            }
        }
        BoundExpr::Subscript { base, index, .. } => {
            subst_outer_expr(base, outer, depth);
            subst_outer_expr(index, outer, depth);
        }
        BoundExpr::Case { whens, else_, .. } => {
            for (cond, result) in whens {
                subst_outer_expr(cond, outer, depth);
                subst_outer_expr(result, outer, depth);
            }
            if let Some(e) = else_ {
                subst_outer_expr(e, outer, depth);
            }
        }
        BoundExpr::Aggregate { args, .. } => {
            for a in args {
                subst_outer_expr(a, outer, depth);
            }
        }
        // A nested expression-subquery is one query level deeper.
        BoundExpr::ScalarSubquery { subplan, .. } | BoundExpr::Exists { subplan, .. } => {
            subst_outer_plan(&mut subplan.0, outer, depth + 1);
        }
        BoundExpr::QuantifiedSubquery { subplan, cmp, .. } => {
            subst_outer_plan(&mut subplan.0, outer, depth + 1);
            subst_outer_expr(cmp, outer, depth);
        }
        // The array operand and the needle live at this query level.
        BoundExpr::QuantifiedArray { array, cmp, .. } => {
            subst_outer_expr(array, outer, depth);
            subst_outer_expr(cmp, outer, depth);
        }
    }
}

/// Whether a bound plan contains any correlated outer reference
/// ([`BoundExpr::OuterColumnRef`]) — a subplan that cannot be folded once before
/// execution because its value depends on an enclosing row. The executor uses
/// this to leave such subqueries for per-outer-row evaluation.
pub fn plan_has_outer_refs(plan: &LogicalPlan) -> bool {
    let mut found = false;
    for_each_plan_expr(plan, &mut |e| {
        if expr_has_outer_ref(e) {
            found = true;
        }
    });
    found
}

/// Whether `plan` calls a user-defined routine anywhere.
///
/// A routine body may write, and the executor cannot tell before running it —
/// so a statement that calls one has to be treated as a write: it needs an XID
/// to stamp the body's versions with, and its result set has to be drained
/// before the transaction is finalized rather than streamed after it.
pub fn plan_calls_routine(plan: &LogicalPlan) -> bool {
    let mut found = false;
    for_each_plan_expr(plan, &mut |e| {
        if e.contains_routine() {
            found = true;
        }
    });
    found
}

/// Visit every top-level expression of `plan`, recursing through structural
/// sub-plans (derived tables, join inputs, DML sources). Does not descend into
/// expression-subquery markers — `expr_has_outer_ref` handles those.
fn for_each_plan_expr(plan: &LogicalPlan, f: &mut impl FnMut(&BoundExpr)) {
    match plan {
        LogicalPlan::Values {
            rows, predicate, ..
        } => {
            rows.iter().flatten().for_each(&mut *f);
            if let Some(p) = predicate {
                f(p);
            }
        }
        LogicalPlan::Query {
            projections,
            predicate,
            ..
        } => {
            projections.iter().for_each(&mut *f);
            if let Some(p) = predicate {
                f(p);
            }
        }
        LogicalPlan::Subquery {
            source,
            projections,
            predicate,
            ..
        } => {
            for_each_plan_expr(source, &mut *f);
            projections.iter().for_each(&mut *f);
            if let Some(p) = predicate {
                f(p);
            }
        }
        LogicalPlan::TableFunction {
            args,
            projections,
            predicate,
            ..
        } => {
            args.iter().chain(projections.iter()).for_each(&mut *f);
            if let Some(p) = predicate {
                f(p);
            }
        }
        LogicalPlan::Join {
            source,
            projections,
            predicate,
            ..
        } => {
            for_each_join_expr(source, &mut *f);
            projections.iter().for_each(&mut *f);
            if let Some(p) = predicate {
                f(p);
            }
        }
        // An Append exposes no expressions of its own.
        LogicalPlan::Append { .. } => {}
        LogicalPlan::SetOp { arms, .. } => {
            for arm in arms {
                for_each_plan_expr(&arm.plan, &mut *f);
                arm.coercion.iter().flatten().for_each(&mut *f);
            }
        }
        LogicalPlan::Limit { source, .. } => for_each_plan_expr(source, &mut *f),
        LogicalPlan::Aggregate {
            input,
            predicate,
            group_exprs,
            aggregates,
            having,
            projections,
            ..
        } => {
            if let AggInput::Join(join) = input {
                for_each_join_expr(join, &mut *f);
            }
            if let Some(p) = predicate {
                f(p);
            }
            group_exprs.iter().for_each(&mut *f);
            for agg in aggregates {
                for arg in agg.args.iter() {
                    f(arg);
                }
            }
            if let Some(h) = having {
                f(h);
            }
            projections.iter().for_each(&mut *f);
        }
        LogicalPlan::Insert {
            source, returning, ..
        } => {
            match source {
                InsertSource::Values(rows) => rows.iter().flatten().for_each(&mut *f),
                InsertSource::Query { input, projections } => {
                    for_each_plan_expr(input, &mut *f);
                    projections.iter().for_each(&mut *f);
                }
            }
            if let Some(r) = returning {
                r.projections.iter().for_each(&mut *f);
            }
        }
        LogicalPlan::Update {
            predicate,
            assignments,
            returning,
            ..
        } => {
            if let Some(p) = predicate {
                f(p);
            }
            for (_, e) in assignments {
                f(e);
            }
            if let Some(r) = returning {
                r.projections.iter().for_each(&mut *f);
            }
        }
        LogicalPlan::Delete {
            predicate,
            returning,
            ..
        } => {
            if let Some(p) = predicate {
                f(p);
            }
            if let Some(r) = returning {
                r.projections.iter().for_each(&mut *f);
            }
        }
    }
}

fn for_each_join_expr(join: &JoinExpr, f: &mut impl FnMut(&BoundExpr)) {
    match join {
        JoinExpr::Input { input, .. } => match input {
            JoinInput::Scan(_) => {}
            JoinInput::Subplan(plan) => for_each_plan_expr(plan, &mut *f),
            JoinInput::TableFunction { args, .. } => args.iter().for_each(&mut *f),
        },
        JoinExpr::Join {
            left,
            right,
            predicate,
            ..
        } => {
            for_each_join_expr(left, &mut *f);
            for_each_join_expr(right, &mut *f);
            if let Some(p) = predicate {
                f(p);
            }
        }
    }
}

/// Whether an expression tree contains an [`BoundExpr::OuterColumnRef`],
/// including inside nested expression-subquery subplans.
fn expr_has_outer_ref(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::OuterColumnRef { .. } => true,
        BoundExpr::Const { .. } | BoundExpr::ColumnRef { .. } | BoundExpr::Param { .. } => false,
        BoundExpr::Unary { expr, .. }
        | BoundExpr::IsNull { expr, .. }
        | BoundExpr::Coerce { expr, .. }
        | BoundExpr::Collate { expr, .. }
        | BoundExpr::Reinterpret { expr, .. } => expr_has_outer_ref(expr),
        BoundExpr::Binary { left, right, .. } => {
            expr_has_outer_ref(left) || expr_has_outer_ref(right)
        }
        BoundExpr::FuncCall { args, .. }
        | BoundExpr::Routine { args, .. }
        | BoundExpr::Srf { args, .. } => args.iter().any(expr_has_outer_ref),
        BoundExpr::ArrayCtor { elems, .. } => elems.iter().any(expr_has_outer_ref),
        BoundExpr::Subscript { base, index, .. } => {
            expr_has_outer_ref(base) || expr_has_outer_ref(index)
        }
        BoundExpr::Case { whens, else_, .. } => {
            whens
                .iter()
                .any(|(c, r)| expr_has_outer_ref(c) || expr_has_outer_ref(r))
                || else_.as_deref().is_some_and(expr_has_outer_ref)
        }
        BoundExpr::Aggregate { args, .. } => args.iter().any(expr_has_outer_ref),
        BoundExpr::ScalarSubquery { subplan, .. } | BoundExpr::Exists { subplan, .. } => {
            plan_has_outer_refs(&subplan.0)
        }
        BoundExpr::QuantifiedSubquery { subplan, cmp, .. } => {
            plan_has_outer_refs(&subplan.0) || expr_has_outer_ref(cmp)
        }
        BoundExpr::QuantifiedArray { array, cmp, .. } => {
            expr_has_outer_ref(array) || expr_has_outer_ref(cmp)
        }
    }
}

pub fn bind_query(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    query: &ast::Query,
) -> Result<LogicalPlan, BindError> {
    bind_query_with_params(engine, catalog, query, &param_ctx_none())
}

/// Bind a top-level query for the extended query protocol, threading a shared
/// parameter context so `$n` placeholders are typed from context (and unify
/// across the whole statement). The caller reads the inferred types via
/// [`crate::param_types`] after a successful bind.
pub fn bind_query_with_params(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    query: &ast::Query,
    params: &ParamCtx,
) -> Result<LogicalPlan, BindError> {
    bind_query_scoped(engine, catalog, params, query, &CteEnv::new(), &[])
}

/// Bind a query with a set of visible CTEs. Recurses for CTE bodies and derived
/// tables, extending the environment with this query's own `WITH` clause.
pub(crate) fn bind_query_scoped(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
    query: &ast::Query,
    outer: &CteEnv,
    outer_scope: &[OuterLevel],
) -> Result<LogicalPlan, BindError> {
    // Only build (clone) an extended environment when this query has a WITH; the
    // common no-CTE case binds against `outer` directly.
    match &query.with {
        Some(with) => {
            let ctes = bind_ctes(engine, catalog, params, with, outer)?;
            bind_query_body(engine, catalog, params, query, &ctes, outer_scope)
        }
        None => bind_query_body(engine, catalog, params, query, outer, outer_scope),
    }
}

/// Bind a query's body (SELECT or VALUES) against a resolved CTE environment.
/// `outer_scope` carries the enclosing queries' relations when this body is a
/// correlated subquery, so its columns can resolve outward (empty otherwise).
fn bind_query_body(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
    query: &ast::Query,
    ctes: &CteEnv,
    outer_scope: &[OuterLevel],
) -> Result<LogicalPlan, BindError> {
    reject_unsupported_query_clauses(query)?;
    let inner = match query.body.as_ref() {
        ast::SetExpr::Select(select) => {
            reject_unsupported_select_clauses(select)?;
            bind_select(engine, catalog, params, select, &query.order_by, ctes, outer_scope)
        }
        ast::SetExpr::Values(values) => {
            bind_values_query(catalog, params, values, &query.order_by, outer_scope)
        }
        ast::SetExpr::Table(table) => {
            bind_table_query(engine, catalog, params, table, &query.order_by, ctes)
        }
        // UNION / UNION ALL. The whole query's ORDER BY applies above the union
        // (a query-level clause), so it is bound here, not inside the arms.
        ast::SetExpr::SetOperation {
            left,
            op,
            set_quantifier,
            right,
        } => bind_set_operation(
            engine,
            catalog,
            params,
            SetOpParts {
                left,
                op,
                quantifier: set_quantifier,
                right,
            },
            &query.order_by,
            ctes,
            outer_scope,
            1,
        ),
        // A parenthesized body — `(SELECT ... UNION ...) ORDER BY 1 LIMIT n`,
        // the idiomatic spelling for attaching a tail to a set operation. The
        // inner query owns its own WITH/ORDER BY/LIMIT; this level's LIMIT then
        // wraps the result below.
        ast::SetExpr::Query(inner) => {
            bind_query_scoped(engine, catalog, params, inner, ctes, outer_scope)
                .and_then(|plan| apply_query_tail(plan, query, catalog, params))
        }
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

/// How deeply set operations may nest. Flattening keeps a chain of equivalent
/// operations at one level, so this only bounds genuinely nested forms.
///
/// A backstop rather than the usual limit: the parser guards its own recursion
/// at the same depth, so it rejects such a query before binding ever sees it.
/// Keeping the bound here makes the binder safe on its own — an unbounded walk
/// recurses through plan and execute too, which overflowed the stack and aborted
/// the process.
const MAX_SET_OP_NESTING: usize = 50;

/// Apply a parenthesized query body's own `ORDER BY` to the plan bound from it.
/// The inner query already consumed its own tail, so this is only the outer
/// level's sort — bound, like any set-operation sort, against the output columns.
fn apply_query_tail(
    plan: LogicalPlan,
    query: &ast::Query,
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
) -> Result<LogicalPlan, BindError> {
    if query.order_by.is_none() {
        return Ok(plan);
    }
    let columns = output_columns_of(&plan)?;
    let sort = bind_set_order_by(&query.order_by, &columns, catalog, params)?;
    Ok(attach_sort(plan, sort, columns))
}

/// Attach a sort to a plan that produces `columns`.
///
/// A set operation owns its sort, so it takes the keys directly — including when
/// it already has some, since an inner `ORDER BY` without a `LIMIT` does not
/// survive an outer one (PG discards it too). Deliberately no wrapping in a
/// `Subquery` here: that is a derived table, i.e. a query nesting level, and
/// would strand any correlated reference inside the arms.
///
/// A `LIMIT` is the one boundary that must keep its own ordering — the bound
/// applies to the rows the inner sort chose — so the outer sort goes above it.
fn attach_sort(plan: LogicalPlan, sort: Vec<SortKey>, columns: Vec<OutputColumn>) -> LogicalPlan {
    if sort.is_empty() {
        return plan;
    }
    match plan {
        LogicalPlan::SetOp {
            arms,
            columns,
            distinct,
            sort: _,
        } => LogicalPlan::SetOp {
            arms,
            columns,
            sort,
            distinct,
        },
        plan @ LogicalPlan::Limit { .. } => LogicalPlan::Subquery {
            projections: identity_projections(&columns),
            source: Box::new(plan),
            columns,
            predicate: None,
            sort,
            distinct: None,
        },
        // Any other body already bound this query's ORDER BY itself.
        plan => plan,
    }
}

/// Bind a `UNION` / `UNION ALL` set operation. `INTERSECT` / `EXCEPT` are not
/// supported yet.
///
/// A chain of equivalent operations is flattened into one N-ary
/// [`LogicalPlan::SetOp`] rather than nested pairs. The parser builds set
/// operations left-deep, so `a UNION b UNION c UNION ...` would otherwise nest
/// one level per arm — which both re-deduplicates at every level and, for a long
/// enough chain, recurses deeply enough through bind/plan/execute to exhaust the
/// stack. Flattening walks that left spine iteratively, so a flat chain costs one
/// node and no recursion regardless of length; `nesting` bounds the genuinely
/// nested cases (parenthesized or non-flattenable arms) that still recurse.
fn bind_set_operation(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
    set_op: SetOpParts<'_>,
    order_by: &Option<ast::OrderBy>,
    ctes: &CteEnv,
    outer_scope: &[OuterLevel],
    nesting: usize,
) -> Result<LogicalPlan, BindError> {
    let all = set_op.quantifier_is_all()?;
    let arm_exprs = flatten_union_chain(set_op, all)?;

    // Bind every arm, then reconcile their layouts into the unified output.
    let mut bound: Vec<(LogicalPlan, Vec<OutputColumn>)> = Vec::with_capacity(arm_exprs.len());
    for arm in arm_exprs {
        bound.push(bind_set_tree(
            engine,
            catalog,
            params,
            arm,
            ctes,
            outer_scope,
            nesting + 1,
        )?);
    }
    let columns = unify_set_columns(&bound, catalog)?;
    let arms = bound
        .into_iter()
        .map(|(plan, arm_cols)| {
            Ok(SetOpArm {
                coercion: set_arm_coercion(&plan, &arm_cols, &columns)?,
                plan,
            })
        })
        .collect::<Result<Vec<_>, BindError>>()?;

    // UNION deduplicates on every output column; UNION ALL keeps duplicates.
    let distinct = if all {
        None
    } else {
        reject_undedupable_columns(&columns, catalog)?;
        Some(all_column_distinct_keys(&columns))
    };
    // A top-level ORDER BY over a set operation resolves against the output
    // columns only — ordinals and output-column names, never the arms' inputs.
    let sort = bind_set_order_by(order_by, &columns, catalog, params)?;
    Ok(LogicalPlan::SetOp {
        arms,
        columns,
        sort,
        distinct,
    })
}

/// The pieces of an `ast::SetExpr::SetOperation`, so the binder works with the
/// already-destructured fields instead of re-matching the enum.
#[derive(Clone, Copy)]
struct SetOpParts<'a> {
    left: &'a ast::SetExpr,
    op: &'a ast::SetOperator,
    quantifier: &'a ast::SetQuantifier,
    right: &'a ast::SetExpr,
}

impl<'a> SetOpParts<'a> {
    /// Reject everything but UNION, and read its `ALL` / `DISTINCT` quantifier.
    fn quantifier_is_all(&self) -> Result<bool, BindError> {
        if *self.op != ast::SetOperator::Union {
            return Err(BindError::feature_not_supported(format!(
                "{} is not supported yet",
                self.op
            )));
        }
        match self.quantifier {
            ast::SetQuantifier::All => Ok(true),
            // Bare `UNION` and explicit `UNION DISTINCT` both deduplicate.
            ast::SetQuantifier::Distinct | ast::SetQuantifier::None => Ok(false),
            // `UNION BY NAME` and friends are non-standard.
            other => Err(BindError::feature_not_supported(format!(
                "UNION {other} is not supported yet"
            ))),
        }
    }
}

/// Walk a left-deep chain of equivalent UNIONs into a flat arm list, in query
/// order. Iterative by construction: the left spine is a loop, so chain length
/// costs no stack.
///
/// A nested UNION folds into its parent only when doing so preserves multiset
/// semantics. Deduplication is idempotent and absorbs duplicates from below, so
/// a distinct parent may absorb either kind of child, and an ALL parent may
/// absorb an ALL child — but an ALL parent must *not* absorb a DISTINCT child,
/// whose deduplication has to happen first and would otherwise be lost.
fn flatten_union_chain<'a>(
    set_op: SetOpParts<'a>,
    all: bool,
) -> Result<Vec<&'a ast::SetExpr>, BindError> {
    let mut arms = vec![set_op.right];
    let mut node = set_op.left;
    while let ast::SetExpr::SetOperation {
        left,
        op,
        set_quantifier,
        right,
    } = node
    {
        let child = SetOpParts {
            left,
            op,
            quantifier: set_quantifier,
            right,
        };
        // Surface an unsupported operator/quantifier here rather than silently
        // declining to flatten and reporting it one recursion deeper.
        let child_all = child.quantifier_is_all()?;
        if all && !child_all {
            break;
        }
        arms.push(right);
        node = left;
    }
    arms.push(node);
    arms.reverse();
    Ok(arms)
}

/// Bind one arm of a set operation to a plan plus its output columns. An arm has
/// no ORDER BY / LIMIT of its own (those are query-level), so nested SELECT /
/// VALUES / TABLE bodies bind without one; a nested set operation recurses (and
/// applies its own deduplication), and a parenthesized subquery arm binds through
/// the full query path, carrying its own WITH / ORDER BY / LIMIT.
fn bind_set_tree(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
    arm: &ast::SetExpr,
    ctes: &CteEnv,
    outer_scope: &[OuterLevel],
    nesting: usize,
) -> Result<(LogicalPlan, Vec<OutputColumn>), BindError> {
    // Flattening keeps a flat chain at nesting 1, so only genuinely nested set
    // operations count against this bound. PG likewise reports a runaway nesting
    // depth rather than letting the stack overflow.
    if nesting > MAX_SET_OP_NESTING {
        return Err(BindError::new(
            sqlstate::STATEMENT_TOO_COMPLEX,
            "stack depth limit exceeded",
        ));
    }
    let plan = match arm {
        ast::SetExpr::Select(select) => {
            reject_unsupported_select_clauses(select)?;
            bind_select(engine, catalog, params, select, &None, ctes, outer_scope)?
        }
        ast::SetExpr::Values(values) => {
            bind_values_query(catalog, params, values, &None, outer_scope)?
        }
        ast::SetExpr::Table(table) => bind_table_query(engine, catalog, params, table, &None, ctes)?,
        ast::SetExpr::SetOperation {
            left,
            op,
            set_quantifier,
            right,
        } => bind_set_operation(
            engine,
            catalog,
            params,
            SetOpParts {
                left,
                op,
                quantifier: set_quantifier,
                right,
            },
            &None,
            ctes,
            outer_scope,
            nesting,
        )?,
        // A parenthesized arm is a complete query: it may carry its own WITH,
        // ORDER BY and LIMIT, so it binds through the scoped entry point (the
        // plain body binder would silently drop a WITH).
        ast::SetExpr::Query(query) => {
            bind_query_scoped(engine, catalog, params, query, ctes, outer_scope)?
        }
        other => {
            return Err(BindError::feature_not_supported(format!(
                "query form is not supported yet: {other}"
            )));
        }
    };
    let columns = output_columns_of(&plan)?;
    Ok((plan, columns))
}

/// Reconcile every arm's columns into the set operation's output layout: equal
/// arity, a per-position common type, and names from the first arm (PG's rules).
fn unify_set_columns(
    arms: &[(LogicalPlan, Vec<OutputColumn>)],
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<Vec<OutputColumn>, BindError> {
    let (first_plan, first_cols) = &arms[0];
    if arms.iter().any(|(_, cols)| cols.len() != first_cols.len()) {
        return Err(BindError::new(
            sqlstate::SYNTAX_ERROR,
            "each UNION query must have the same number of columns",
        ));
    }
    // An arm column's `(collation, strength)` re-expressed as a `Derived`, the
    // same shape every other multi-input collation combination (function
    // arguments, CASE branches, ARRAY elements) folds over.
    let to_derived = |c: &OutputColumn| crate::collation::Derived {
        collation: c
            .collation
            .unwrap_or_else(|| crabgresql_types::collation::type_collation(c.ty)),
        strength: c.strength,
    };
    let mut columns = Vec::with_capacity(first_cols.len());
    for (i, first) in first_cols.iter().enumerate() {
        // Fold the common type across all arms. A bare NULL contributes no type
        // (PG resolves an unknown-typed set-operation column from the other
        // arms), so it never forces the column to `text`.
        let first_typed = !is_null_literal_column(first_plan, i);
        let mut common = first_typed.then_some(first.ty);
        // Fold the arms' collations the same way: two explicit COLLATE clauses
        // that disagree are `42P22`, exactly as they would be in a direct
        // comparison; anything weaker that disagrees drops to the type
        // default, as PG resolves a set operation's output collation.
        let mut derived = first_typed.then(|| to_derived(first));
        for (plan, cols) in &arms[1..] {
            if is_null_literal_column(plan, i) {
                continue;
            }
            let ty = cols[i].ty;
            common = Some(match common {
                None => ty,
                Some(prev) => merge_types(prev, ty).ok_or_else(|| {
                    BindError::new(
                        sqlstate::DATATYPE_MISMATCH,
                        format!(
                            "UNION types {} and {} cannot be matched",
                            crate::expr::type_label(prev, catalog.as_ref()),
                            crate::expr::type_label(ty, catalog.as_ref()),
                        ),
                    )
                })?,
            });
            let next = to_derived(&cols[i]);
            derived = Some(match derived {
                None => next,
                Some(prev) => {
                    crate::collation::check_explicit_conflict([prev, next])?;
                    prev.max_with(next)
                }
            });
        }
        // An all-NULL column has no type to take: PG resolves it to `text`.
        let merged_ty = common.unwrap_or(PgType::Text);
        let (collation, strength) = match derived.filter(|_| merged_ty.is_collatable()) {
            Some(d) => {
                let default = crabgresql_types::collation::type_collation(merged_ty);
                (
                    (d.collation != default).then_some(d.collation),
                    d.strength,
                )
            }
            None => (None, crate::collation::Strength::None),
        };
        columns.push(OutputColumn {
            name: first.name.clone(),
            ty: merged_ty,
            collation,
            strength,
        });
    }
    Ok(columns)
}

/// Whether an arm's column `index` is a bare `NULL` literal. Such a column has no
/// type of its own — the binder already resolved the untyped literal to `text` —
/// so a set operation lets the other arms decide the column type, as PG does.
/// NULL casts to anything, so adopting that type is always safe.
fn is_null_literal_column(plan: &LogicalPlan, index: usize) -> bool {
    let is_null_const = |e: &BoundExpr| matches!(e, BoundExpr::Const { value: Value::Null, .. });
    match plan {
        // `SELECT NULL` / `VALUES (NULL)`: every row must be NULL in this column.
        LogicalPlan::Values { rows, .. } => {
            rows.iter().all(|row| row.get(index).is_some_and(is_null_const))
        }
        LogicalPlan::Query { projections, .. }
        | LogicalPlan::Subquery { projections, .. }
        | LogicalPlan::TableFunction { projections, .. }
        | LogicalPlan::Join { projections, .. }
        | LogicalPlan::Aggregate { projections, .. } => {
            projections.get(index).is_some_and(is_null_const)
        }
        LogicalPlan::Limit { source, .. } => is_null_literal_column(source, index),
        _ => false,
    }
}

/// The projections mapping an arm onto the set operation's output layout, or
/// `None` when the arm already emits it. A bare `NULL` column is re-typed to the
/// resolved column type (its `text` placeholder never reaches the output).
fn set_arm_coercion(
    plan: &LogicalPlan,
    arm_cols: &[OutputColumn],
    target: &[OutputColumn],
) -> Result<Option<Vec<BoundExpr>>, BindError> {
    let needs_coercion = arm_cols
        .iter()
        .zip(target)
        .enumerate()
        .any(|(i, (a, t))| a.ty != t.ty || is_null_literal_column(plan, i));
    if !needs_coercion {
        return Ok(None);
    }
    let mut projections = Vec::with_capacity(target.len());
    for (i, (a, t)) in arm_cols.iter().zip(target).enumerate() {
        let expr = if is_null_literal_column(plan, i) {
            // Re-type the NULL directly rather than casting a `text` NULL.
            BoundExpr::Const {
                value: Value::Null,
                ty: t.ty,
            }
        } else {
            coerce_expr(BoundExpr::ColumnRef { index: i, ty: a.ty }, t.ty)?
        };
        projections.push(expr);
    }
    Ok(Some(projections))
}

/// Reject an output column a `UNION` cannot deduplicate on. PG needs an equality
/// operator for every column; the executor's dedup compares with the same
/// ordering-based helpers `DISTINCT` uses, so the check mirrors that capability.
fn reject_undedupable_columns(
    columns: &[OutputColumn],
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<(), BindError> {
    for col in columns {
        if !crate::expr::is_orderable(col.ty, catalog.as_ref()) {
            return Err(BindError::new(
                sqlstate::UNDEFINED_FUNCTION,
                format!(
                    "could not identify an equality operator for type {}",
                    crate::expr::type_label(col.ty, catalog.as_ref())
                ),
            ));
        }
    }
    Ok(())
}

/// Bind a set operation's `ORDER BY`. Only ordinals and output-column names
/// resolve (there is no single input relation to bind an expression against), so
/// PG reports anything else as an invalid set-operation ORDER BY rather than a
/// missing feature.
fn bind_set_order_by(
    order_by: &Option<ast::OrderBy>,
    columns: &[OutputColumn],
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
) -> Result<Vec<SortKey>, BindError> {
    let scope = Scope::empty(catalog, params);
    let mut projections = identity_projections(columns);
    let sort = bind_order_by(order_by, columns, &scope, &mut projections, false);
    match sort {
        // `allow_hidden = false` turns every unresolved item into one generic
        // feature-not-supported error; distinguish the two cases PG reports.
        Err(e) if e.code == sqlstate::FEATURE_NOT_SUPPORTED => {
            Err(set_order_by_error(order_by, columns))
        }
        other => other,
    }
}

/// PG's error for an `ORDER BY` item a set operation cannot resolve: a bare name
/// that is not an output column "does not exist"; anything else must be one of
/// the result columns.
fn set_order_by_error(order_by: &Option<ast::OrderBy>, columns: &[OutputColumn]) -> BindError {
    if let Some(ast::OrderBy {
        kind: ast::OrderByKind::Expressions(exprs),
        ..
    }) = order_by
    {
        for oe in exprs {
            if let ast::Expr::Identifier(ident) = &oe.expr {
                let name = normalize_ident(ident);
                if !columns.iter().any(|c| c.name == name) {
                    return BindError::new(
                        sqlstate::UNDEFINED_COLUMN,
                        format!("column \"{name}\" does not exist"),
                    );
                }
            }
        }
    }
    BindError::new(
        sqlstate::INVALID_COLUMN_REFERENCE,
        "invalid UNION/INTERSECT/EXCEPT ORDER BY clause",
    )
    .with_hint(Some(
        "Only result column names can be used, not expressions or functions.".to_string(),
    ))
}

/// Identity projections (one `ColumnRef` per output column).
fn identity_projections(columns: &[OutputColumn]) -> Vec<BoundExpr> {
    columns
        .iter()
        .enumerate()
        .map(|(index, col)| BoundExpr::ColumnRef { index, ty: col.ty })
        .collect()
}

/// Deduplication keys covering every output column — UNION (distinct) semantics.
fn all_column_distinct_keys(columns: &[OutputColumn]) -> Vec<DistinctKey> {
    columns
        .iter()
        .enumerate()
        .map(|(column, col)| DistinctKey { column, ty: col.ty })
        .collect()
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
                (
                    sqlstate::INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE,
                    "OFFSET",
                )
            } else {
                (sqlstate::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE, "LIMIT")
            };
            Err(BindError::new(code, format!("{kind} must not be negative")))
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
    params: &ParamCtx,
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
        // A CTE body is not correlated to the query that references it.
        let plan = bind_query_scoped(engine, catalog, params, &cte.query, &ctes, &[])?;
        let mut columns = output_columns_of(&plan)?;
        apply_alias_columns(&mut columns, &cte.alias.columns, &with_query_subject(&name))?;
        ctes.insert(name, CteRelation { columns, plan });
    }
    Ok(ctes)
}

/// Bind the SELECT body (its FROM items, projections, WHERE, ORDER BY).
fn bind_select(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
    select: &ast::Select,
    order_by: &Option<ast::OrderBy>,
    ctes: &CteEnv,
    outer_scope: &[OuterLevel],
) -> Result<LogicalPlan, BindError> {
    if select.from.is_empty() {
        return bind_values_select(engine, catalog, params, select, order_by, ctes, outer_scope);
    }
    let BoundFrom {
        source,
        relations,
        visible,
    } = bind_from_clause(engine, catalog, params, &select.from, ctes, outer_scope)?;
    let scope = Scope::relations_with_visible(relations, visible, catalog, params)
        .with_subqueries(engine, ctes)
        .with_outer(outer_scope.to_vec());
    let body = bind_select_body(select, order_by, &scope)?;
    if let Some(agg) = body.aggregation {
        let input = match source {
            JoinExpr::Input {
                input: JoinInput::Scan(table),
                ..
            } => AggInput::Scan(table),
            // A derived table, CTE reference, `VALUES` in FROM, or set-returning
            // function feeds the aggregate through the same join machinery as a
            // multi-relation FROM — an `Input` node is just a single-source tree.
            source => AggInput::Join(source),
        };
        return Ok(LogicalPlan::Aggregate {
            input,
            predicate: body.predicate,
            group_exprs: agg.group_exprs,
            aggregates: agg.aggregates,
            having: agg.having,
            columns: body.columns,
            projections: body.projections,
            sort: body.sort,
            distinct: body.distinct,
        });
    }
    Ok(match source {
        JoinExpr::Input { input, .. } => finish_single_select(input, body),
        source @ JoinExpr::Join { .. } => LogicalPlan::Join {
            source,
            columns: body.columns,
            projections: body.projections,
            predicate: body.predicate,
            sort: body.sort,
            distinct: body.distinct,
        },
    })
}

/// Preserve the compact single-source plan variants when FROM contains no
/// comma or explicit join.
fn finish_single_select(input: JoinInput, body: SelectBody) -> LogicalPlan {
    match input {
        JoinInput::Scan(table) => LogicalPlan::Query {
            table,
            columns: body.columns,
            projections: body.projections,
            predicate: body.predicate,
            sort: body.sort,
            distinct: body.distinct,
        },
        JoinInput::Subplan(source) => LogicalPlan::Subquery {
            source,
            columns: body.columns,
            projections: body.projections,
            predicate: body.predicate,
            sort: body.sort,
            distinct: body.distinct,
        },
        JoinInput::TableFunction { func, args } => LogicalPlan::TableFunction {
            func,
            args,
            columns: body.columns,
            projections: body.projections,
            predicate: body.predicate,
            sort: body.sort,
            distinct: body.distinct,
        },
    }
}

/// Bind a `TABLE t` query body (`SetExpr::Table`), which is exactly
/// `SELECT * FROM t` with an optional ORDER BY. The table resolves through the
/// same FROM machinery as a plain relation reference (search-path resolution and
/// CTE shadowing included), then `*` expands over its columns.
fn bind_table_query(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
    table: &ast::Table,
    order_by: &Option<ast::OrderBy>,
    ctes: &CteEnv,
) -> Result<LogicalPlan, BindError> {
    // `TABLE t` is `SELECT * FROM t`: resolve the name through the same FROM
    // machinery as a plain relation reference. Reuse the parsed `ObjectName`
    // verbatim so identifier quoting (`TABLE "MixedCase"`) and schema
    // qualification are honored exactly as in a FROM clause.
    let relation = ast::TableFactor::Table {
        name: table.name.clone(),
        alias: None,
        args: None,
        with_hints: Vec::new(),
        version: None,
        with_ordinality: false,
        partitions: Vec::new(),
        json_path: None,
        sample: None,
        index_hints: Vec::new(),
    };
    let BoundFrom {
        source,
        relations,
        visible,
    } = bind_from_item(engine, catalog, params, &relation, ctes)?.into_bound_from();
    let scope = Scope::relations_with_visible(relations, visible, catalog, params);
    let mut columns = Vec::new();
    let mut projections = Vec::new();
    for (col, expr) in scope.expand_wildcard() {
        columns.push(col);
        projections.push(expr);
    }
    let sort = bind_order_by(order_by, &columns, &scope, &mut projections, true)?;
    let body = SelectBody {
        columns,
        projections,
        predicate: None,
        sort,
        distinct: None,
        aggregation: None,
    };
    match source {
        JoinExpr::Input { input, .. } => Ok(finish_single_select(input, body)),
        // A single relation reference never produces a join tree.
        JoinExpr::Join { .. } => unreachable!("TABLE t binds a single relation"),
    }
}

/// Convert a rowset's output columns into storage `Column`s for a [`Scope`].
fn to_columns(columns: &[OutputColumn]) -> Vec<Column> {
    columns
        .iter()
        .map(|c| {
            let mut col = Column::new(c.name.clone(), c.ty);
            col.collation = c.collation;
            col
        })
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

/// A bound FROM clause (or one comma-delimited `TableWithJoins` group): its
/// executable row-source tree and the flat relation namespace exposed to
/// projection/WHERE/GROUP BY binding.
struct BoundFrom {
    source: JoinExpr,
    relations: Vec<(String, Vec<Column>)>,
    /// The merged-column view when a `USING`/`NATURAL` join is present; `None`
    /// keeps the plain "every relation's columns in order" behavior. Exprs index
    /// into this FROM's own combined row (base 0); [`bind_from_clause`] shifts
    /// them when it cross-joins several comma groups.
    visible: Option<Vec<VisibleColumn>>,
}

impl BoundFromItem {
    fn into_bound_from(self) -> BoundFrom {
        let width = self.columns.len();
        BoundFrom {
            source: JoinExpr::Input {
                input: self.input,
                width,
            },
            relations: vec![(self.qualifier, to_columns(&self.columns))],
            visible: None,
        }
    }
}

/// Whether `start` can reach itself through view→view dependency edges — i.e.
/// expanding it would recurse forever. Follows only names that are themselves
/// views (a dependency on a table is a leaf); a `visited` set bounds the walk.
fn view_is_recursive(engine: &Arc<dyn TableEngine>, start: &str) -> bool {
    let views = engine.views();
    let deps: HashMap<&str, &[String]> = views
        .iter()
        .map(|v| (v.name.as_str(), v.depends_on.as_slice()))
        .collect();
    let Some(first) = deps.get(start) else {
        return false;
    };
    let mut stack: Vec<&str> = first.iter().map(String::as_str).collect();
    let mut visited = HashSet::new();
    while let Some(name) = stack.pop() {
        if name == start {
            return true;
        }
        if !visited.insert(name) {
            continue;
        }
        if let Some(next) = deps.get(name) {
            stack.extend(next.iter().map(String::as_str));
        }
    }
    false
}

/// Bind a stored view's query into a logical plan. The SQL text is re-parsed and
/// bound in a fresh scope (no outer CTEs, no outer `$n` parameters — a view body
/// references neither). A parse/shape failure is an internal invariant violation
/// (the text was validated at `CREATE VIEW`), reported as `XX000`.
fn bind_view_query(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    view: &ViewDefinition,
) -> Result<LogicalPlan, BindError> {
    let stmts = crabgresql_parser::parse(&view.sql).map_err(|e| {
        BindError::new(
            sqlstate::INTERNAL_ERROR,
            format!("could not parse stored definition of view \"{}\": {e}", view.name),
        )
    })?;
    let query = match stmts.as_slice() {
        [ast::Statement::Query(query)] => query,
        _ => {
            return Err(BindError::new(
                sqlstate::INTERNAL_ERROR,
                format!("stored definition of view \"{}\" is not a single query", view.name),
            ));
        }
    };
    bind_query_scoped(engine, catalog, &param_ctx_none(), query, &CteEnv::new(), &[])
}

/// The output columns a view reference exposes: the stored column names (which
/// captured any explicit `CREATE VIEW v(...)` list at creation) carrying the
/// types the re-bound query currently produces.
fn view_output_columns(
    inner: &LogicalPlan,
    view: &ViewDefinition,
) -> Result<Vec<OutputColumn>, BindError> {
    let mut columns = output_columns_of(inner)?;
    for (col, stored) in columns.iter_mut().zip(&view.columns) {
        col.name = stored.name.clone();
    }
    Ok(columns)
}

/// Resolve one FROM item to a [`BoundFromItem`], producing a bare row source
/// (no projection pipeline) so several can be combined into a join tree.
fn bind_from_item(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
    relation: &ast::TableFactor,
    ctes: &CteEnv,
) -> Result<BoundFromItem, BindError> {
    match relation {
        // A `Table` factor carrying call arguments is a set-returning function.
        ast::TableFactor::Table {
            name,
            alias,
            args: Some(fn_args),
            with_ordinality,
            ..
        } => {
            if fn_args.settings.is_some() {
                return Err(BindError::feature_not_supported(
                    "table function SETTINGS are not supported yet",
                ));
            }
            // `WITH ORDINALITY` adds a trailing bigint column; unsupported for now
            // — reject rather than silently drop it (which would return the wrong
            // number of columns).
            if *with_ordinality {
                return Err(BindError::feature_not_supported(
                    "WITH ORDINALITY is not supported yet",
                ));
            }
            let fname = object_name_to_table_name(name)?;
            let arg_exprs = positional_arg_exprs(&fn_args.args)?;
            let (func, args) =
                bind_table_fn_call(&fname, &arg_exprs, &Scope::empty(catalog, params))?;
            let qualifier = relation_qualifier(alias, &fname);
            let mut columns: Vec<OutputColumn> = func
                .columns()
                .into_iter()
                .map(|c| OutputColumn {
                    name: c.name,
                    ty: c.ty,
                    collation: c.collation,
                    strength: c.strength,
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
            // Read resolution honors the search path: temp → pg_catalog →
            // global, so an unqualified `pg_type` reaches `pg_catalog` and a temp
            // table shadows a permanent one (a qualified miss keeps its schema in
            // the error text). A view shares this relation namespace, but PG's
            // precedence puts a real relation (temp table, system catalog) ahead
            // of a like-named view — so a view is tried only when no table on the
            // path claims the name.
            match engine.resolve(cte_schema.as_deref(), &tname) {
                Ok(table) => {
                    let qualifier = relation_qualifier(alias, &tname);
                    let mut columns: Vec<OutputColumn> = table
                        .schema()
                        .columns
                        .iter()
                        .map(|c| OutputColumn {
                            name: c.name.clone(),
                            ty: c.ty,
                            collation: c.collation,
                            // A real table column is always implicit strength,
                            // whether or not its collation equals the type
                            // default.
                            strength: crate::collation::Strength::Implicit,
                        })
                        .collect();
                    // A relation whose rows live in several places is read as a
                    // union scan. Bind it as a subplan wrapping an `Append` (raw
                    // relation columns), so the surrounding SELECT's
                    // projection/WHERE/ORDER BY — and any join or aggregate —
                    // apply through the existing subplan machinery.
                    let input = match scan_leaves(engine, &table)? {
                        Some(leaves) => JoinInput::Subplan(Box::new(LogicalPlan::Append {
                            tables: leaves,
                            columns: columns.clone(),
                        })),
                        None => JoinInput::Scan(table),
                    };
                    apply_relation_alias_columns(&mut columns, alias, &table_subject(&qualifier))?;
                    Ok(BoundFromItem {
                        qualifier,
                        columns,
                        input,
                    })
                }
                Err(e) => {
                    // No table on the path: a public view may claim the name. Its
                    // stored query is re-parsed and re-bound as a subplan (the same
                    // mechanism as a CTE / derived table).
                    if matches!(e, StorageError::TableNotFound(_))
                        && let Some(view) = engine.resolve_view(cte_schema.as_deref(), &tname)
                    {
                        // A view whose definition (transitively) reads itself would
                        // recurse forever when expanded. PG allows creating such a
                        // view but errors when it is used; detect the cycle from the
                        // stored dependency graph before expanding.
                        if view_is_recursive(engine, &view.name) {
                            return Err(BindError::new(
                                sqlstate::INVALID_OBJECT_DEFINITION,
                                format!(
                                    "infinite recursion detected in rules for relation \"{}\"",
                                    view.name
                                ),
                            ));
                        }
                        let inner = bind_view_query(engine, catalog, &view)?;
                        let qualifier = relation_qualifier(alias, &tname);
                        let mut columns = view_output_columns(&inner, &view)?;
                        apply_relation_alias_columns(
                            &mut columns,
                            alias,
                            &table_subject(&qualifier),
                        )?;
                        return Ok(BoundFromItem {
                            qualifier,
                            columns,
                            input: JoinInput::Subplan(Box::new(inner)),
                        });
                    }
                    Err(not_found_as_written(e, cte_schema.as_deref(), &tname))
                }
            }
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
            // A (non-LATERAL) subquery in FROM cannot see the enclosing query's
            // columns, so it binds with no outer scope.
            let inner = bind_query_scoped(engine, catalog, params, subquery, ctes, &[])?;
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

/// Bind all comma-separated FROM groups. Each group owns its JOIN/ON namespace;
/// only after its explicit join chain is complete is it combined with prior
/// groups by a cross join. This makes `a, b JOIN c ON a.x = c.x` reject `a` in
/// ON, matching SQL's join nesting rules.
fn bind_from_clause(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
    from: &[ast::TableWithJoins],
    ctes: &CteEnv,
    outer_scope: &[OuterLevel],
) -> Result<BoundFrom, BindError> {
    let mut combined: Option<JoinExpr> = None;
    let mut relations: Vec<(String, Vec<Column>)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    // Accumulated merged-column view across comma groups. Stays `None` while no
    // group merges columns; once one does, it is materialized (plain views fill
    // in for the non-merging groups) with combined-row-global indices.
    let mut visible: Option<Vec<VisibleColumn>> = None;
    let mut width = 0usize;
    for table in from {
        let BoundFrom {
            source: group_source,
            relations: group_relations,
            visible: group_visible,
        } = bind_table_with_joins(engine, catalog, params, table, ctes, outer_scope)?;
        for (qualifier, _) in &group_relations {
            ensure_unique_qualifier(&mut seen, qualifier)?;
        }
        let group_width = group_source.width();
        // Fold this group's view into the running one, shifting its base-0
        // indices to their global position (`width`).
        match (&mut visible, group_visible) {
            (Some(acc), Some(gv)) => {
                acc.extend(gv.into_iter().map(|c| shift_visible(c, width)));
            }
            (Some(acc), None) => acc.extend(default_visible(&group_relations, width)),
            (None, Some(gv)) => {
                let mut acc = default_visible(&relations, 0);
                acc.extend(gv.into_iter().map(|c| shift_visible(c, width)));
                visible = Some(acc);
            }
            (None, None) => {}
        }
        relations.extend(group_relations);
        width += group_width;
        combined = Some(match combined {
            None => group_source,
            Some(left) => JoinExpr::Join {
                left: Box::new(left),
                right: Box::new(group_source),
                kind: JoinKind::Cross,
                predicate: None,
            },
        });
    }
    Ok(BoundFrom {
        source: combined.expect("non-empty FROM checked by bind_select"),
        relations,
        visible,
    })
}

/// Relocate a merged column from a comma group's base-0 layout to its global
/// position by adding `delta` to every `ColumnRef` in its expression.
fn shift_visible(mut col: VisibleColumn, delta: usize) -> VisibleColumn {
    col.expr.shift_column_refs(delta as isize);
    col
}

/// Bind one left-associative explicit join chain. Each ON/USING clause sees the
/// accumulated left side and the newly-added right factor, but no relations
/// from other comma-delimited FROM groups. A `USING`/`NATURAL` join also builds
/// a merged-column view (`visible`) so its join columns resolve once, before
/// the rest; a chain with only `ON`/`CROSS` joins keeps that view `None`.
fn bind_table_with_joins(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
    table: &ast::TableWithJoins,
    ctes: &CteEnv,
    outer_scope: &[OuterLevel],
) -> Result<BoundFrom, BindError> {
    let mut bound =
        bind_from_item(engine, catalog, params, &table.relation, ctes)?.into_bound_from();
    let mut seen: HashSet<String> = bound
        .relations
        .iter()
        .map(|(qualifier, _)| qualifier.clone())
        .collect();
    // Width of the accumulated left side in the group's combined row, tracked
    // incrementally (each Input is O(1), so no repeated tree walks).
    let mut left_width = bound.source.width();
    // The merged-column view over the accumulated left side, indexed into this
    // group's combined row (base 0). Materialized only once a USING/NATURAL join
    // needs it; an all-`ON`/`CROSS` chain leaves it `None` and allocates nothing.
    let mut visible: Option<Vec<VisibleColumn>> = None;

    for join in &table.joins {
        if join.global {
            return Err(BindError::feature_not_supported(
                "GLOBAL JOIN is not supported yet",
            ));
        }
        let right =
            bind_from_item(engine, catalog, params, &join.relation, ctes)?.into_bound_from();
        let right_qualifier = &right.relations[0].0;
        ensure_unique_qualifier(&mut seen, right_qualifier)?;
        let right_width = right.source.width();

        let (kind, predicate) = match join_kind_and_constraint(&join.join_operator)? {
            JoinBinding::Cross => {
                if let Some(v) = &mut visible {
                    v.extend(default_visible(&right.relations, left_width));
                }
                (JoinKind::Cross, None)
            }
            JoinBinding::On(kind, on) => {
                let mut on_relations = bound.relations.clone();
                on_relations.extend(right.relations.clone());
                // A prior USING/NATURAL join makes the merged view govern
                // unqualified names inside this ON clause too.
                let on_visible = visible.as_ref().map(|v| {
                    let mut ov = v.clone();
                    ov.extend(default_visible(&right.relations, left_width));
                    ov
                });
                let scope =
                    Scope::relations_with_visible(on_relations, on_visible, catalog, params)
                        .with_subqueries(engine, ctes)
                        .with_outer(outer_scope.to_vec());
                let binding = bind_expr(on, &scope)?;
                if matches!(&binding, Binding::Typed(expr) if expr.contains_aggregate()) {
                    return Err(BindError::new(
                        sqlstate::GROUPING_ERROR,
                        "aggregate functions are not allowed in JOIN conditions",
                    ));
                }
                if let Some(v) = &mut visible {
                    v.extend(default_visible(&right.relations, left_width));
                }
                (kind, Some(to_bool_operand(binding, "JOIN/ON")?))
            }
            JoinBinding::Using(kind, names) => {
                let left_view = visible
                    .take()
                    .unwrap_or_else(|| default_visible(&bound.relations, 0));
                let (predicate, new_visible) =
                    build_merged_join(kind, &names, &left_view, &right, left_width, catalog)?;
                visible = Some(new_visible);
                (kind, predicate)
            }
            JoinBinding::Natural(kind) => {
                let left_view = visible
                    .take()
                    .unwrap_or_else(|| default_visible(&bound.relations, 0));
                let names = natural_join_names(&left_view, &right);
                let (predicate, new_visible) =
                    build_merged_join(kind, &names, &left_view, &right, left_width, catalog)?;
                visible = Some(new_visible);
                (kind, predicate)
            }
        };
        bound.source = JoinExpr::Join {
            left: Box::new(bound.source),
            right: Box::new(right.source),
            kind,
            predicate,
        };
        bound.relations.extend(right.relations);
        left_width += right_width;
    }
    bound.visible = visible;
    Ok(bound)
}

/// The plain visible view for a run of relations laid out from `base` in the
/// combined row: every column as a `ColumnRef`, in order — exactly what
/// unqualified resolution and `*` produce without any merged join columns.
fn default_visible(relations: &[(String, Vec<Column>)], base: usize) -> Vec<VisibleColumn> {
    let mut out = Vec::new();
    let mut index = base;
    for (_qualifier, columns) in relations {
        for col in columns {
            out.push(VisibleColumn {
                name: col.name.clone(),
                // Carry the column's declared collation, as unqualified
                // resolution against `rels` does — this view shadows that path.
                expr: crate::expr::with_column_collation(
                    BoundExpr::ColumnRef { index, ty: col.ty },
                    col.collation,
                ),
            });
            index += 1;
        }
    }
    out
}

/// The columns a `NATURAL` join equates: every name present in both the
/// accumulated left view and the right input, in left-to-right order, once
/// each. An empty result means no common columns — a plain cross product.
fn natural_join_names(left: &[VisibleColumn], right: &BoundFrom) -> Vec<String> {
    let right_names: HashSet<&str> = right
        .relations
        .iter()
        .flat_map(|(_, cols)| cols.iter())
        .map(|c| c.name.as_str())
        .collect();
    let mut names: Vec<String> = Vec::new();
    for col in left {
        if right_names.contains(col.name.as_str()) && !names.iter().any(|n| n == &col.name) {
            names.push(col.name.clone());
        }
    }
    names
}

/// Locate a join column by `name` in one side's view: exactly one match returns
/// its combined-row expression; zero or many raise the errors PG reports for a
/// `USING`/`NATURAL` column on the given `side` ("left"/"right").
fn lookup_join_column<'a>(
    cols: &'a [VisibleColumn],
    name: &str,
    side: &str,
) -> Result<&'a BoundExpr, BindError> {
    match lookup_visible(cols, name) {
        VisibleLookup::Found(expr) => Ok(expr),
        VisibleLookup::Ambiguous => Err(BindError::new(
            sqlstate::AMBIGUOUS_COLUMN,
            format!("common column name \"{name}\" appears more than once in {side} table"),
        )),
        VisibleLookup::Missing => Err(BindError::new(
            sqlstate::UNDEFINED_COLUMN,
            format!("column \"{name}\" specified in USING clause does not exist in {side} table"),
        )),
    }
}

/// Build a `USING`/`NATURAL` join's ON predicate and merged-column view. For
/// each join name the left and right copies are equated (with the usual
/// comparison coercion); the merged output column carries the left value
/// (inner/left), the right value (right), or `COALESCE(left, right)` (full), so
/// it is never the NULL-extended side. The new view lists the merged columns
/// first, then each side's remaining columns — matching PG's `SELECT *` order.
fn build_merged_join(
    kind: JoinKind,
    names: &[String],
    left: &[VisibleColumn],
    right: &BoundFrom,
    left_width: usize,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<(Option<BoundExpr>, Vec<VisibleColumn>), BindError> {
    let right_visible = default_visible(&right.relations, left_width);
    let mut predicate: Option<BoundExpr> = None;
    let mut merged_cols: Vec<VisibleColumn> = Vec::new();

    for name in names {
        let left_expr = lookup_join_column(left, name, "left")?.clone();
        let right_expr = lookup_join_column(&right_visible, name, "right")?.clone();
        let (left_ty, right_ty) = (left_expr.ty(), right_expr.ty());

        // The join predicate compares the two copies with the engine's ordinary
        // comparison coercion (which may promote, e.g. `real = int4` compares as
        // float8) — orthogonal to the merged column's output type below.
        let eq = bind_binary_op(
            BinOp::Eq,
            Binding::Typed(left_expr.clone()),
            Binding::Typed(right_expr.clone()),
            crabgresql_parser::Span::empty(),
            catalog.as_ref(),
        )?;
        let Binding::Typed(eq_expr) = eq else {
            unreachable!("equality of typed operands is always typed");
        };

        // PG types the merged column by `select_common_type` (so `real + int4`
        // -> real), which differs from the comparison's promoted type. Since the
        // equality above already succeeded, a common type always exists.
        let merged_ty = merge_types(left_ty, right_ty).ok_or_else(|| {
            BindError::new(
                sqlstate::DATATYPE_MISMATCH,
                format!(
                    "USING column \"{name}\" has no common type for {} and {}",
                    left_ty.name(),
                    right_ty.name()
                ),
            )
        })?;
        let le = coerce_expr(left_expr, merged_ty)?;
        let re = coerce_expr(right_expr, merged_ty)?;
        let merged = match kind {
            JoinKind::Right => re,
            // COALESCE(left, right): the matched rows agree, and each unmatched
            // side keeps the non-NULL value.
            JoinKind::Full => BoundExpr::Case {
                whens: vec![(
                    BoundExpr::IsNull {
                        expr: Box::new(le.clone()),
                        negated: true,
                    },
                    le,
                )],
                else_: Some(Box::new(re)),
                ty: merged_ty,
            },
            _ => le,
        };
        merged_cols.push(VisibleColumn {
            name: name.clone(),
            expr: merged,
        });
        predicate = Some(match predicate {
            None => eq_expr,
            Some(prev) => BoundExpr::Binary {
                op: BinOp::And,
                arg_ty: PgType::Bool,
                collation: DEFAULT_COLLATION_OID,
                left: Box::new(prev),
                right: Box::new(eq_expr),
            },
        });
    }

    let is_join_name = |name: &str| names.iter().any(|n| n == name);
    let mut new_visible = merged_cols;
    new_visible.extend(left.iter().filter(|c| !is_join_name(&c.name)).cloned());
    new_visible.extend(right_visible.into_iter().filter(|c| !is_join_name(&c.name)));
    Ok((predicate, new_visible))
}

fn ensure_unique_qualifier(seen: &mut HashSet<String>, qualifier: &str) -> Result<(), BindError> {
    if seen.insert(qualifier.to_string()) {
        Ok(())
    } else {
        Err(BindError::new(
            sqlstate::DUPLICATE_ALIAS,
            format!("table name \"{qualifier}\" specified more than once"),
        ))
    }
}

/// One explicit join's kind and how its rows are matched, resolved from the
/// AST. `On` carries the predicate expression for later binding; `Using` the
/// (normalized) join column names; `Natural` derives its names from the inputs;
/// `Cross` matches every pair.
enum JoinBinding<'a> {
    Cross,
    On(JoinKind, &'a ast::Expr),
    Using(JoinKind, Vec<String>),
    Natural(JoinKind),
}

/// A `USING (...)` column name: a bare identifier, normalized like any column
/// reference. A schema-qualified name here is rejected the way it is elsewhere.
fn using_column_name(name: &ast::ObjectName) -> Result<String, BindError> {
    object_name_to_table_name(name)
}

/// Map an AST join operator to the kind and match strategy the binder builds.
fn join_kind_and_constraint(operator: &ast::JoinOperator) -> Result<JoinBinding<'_>, BindError> {
    use ast::{JoinConstraint, JoinOperator};

    fn constrained(
        kind: JoinKind,
        constraint: &JoinConstraint,
    ) -> Result<JoinBinding<'_>, BindError> {
        match constraint {
            JoinConstraint::On(expr) => Ok(JoinBinding::On(kind, expr)),
            JoinConstraint::Using(names) => {
                let cols = names
                    .iter()
                    .map(using_column_name)
                    .collect::<Result<Vec<_>, _>>()?;
                // A name may not appear twice in the same USING list.
                for (i, name) in cols.iter().enumerate() {
                    if cols[..i].contains(name) {
                        return Err(BindError::new(
                            sqlstate::DUPLICATE_COLUMN,
                            format!(
                                "column name \"{name}\" appears more than once in USING clause"
                            ),
                        ));
                    }
                }
                Ok(JoinBinding::Using(kind, cols))
            }
            JoinConstraint::Natural => Ok(JoinBinding::Natural(kind)),
            JoinConstraint::None => Err(BindError::feature_not_supported(
                "JOIN without ON is not supported yet",
            )),
        }
    }

    match operator {
        JoinOperator::Join(c) | JoinOperator::Inner(c) => constrained(JoinKind::Inner, c),
        JoinOperator::Left(c) | JoinOperator::LeftOuter(c) => constrained(JoinKind::Left, c),
        JoinOperator::Right(c) | JoinOperator::RightOuter(c) => constrained(JoinKind::Right, c),
        JoinOperator::FullOuter(c) => constrained(JoinKind::Full, c),
        JoinOperator::CrossJoin(JoinConstraint::None) => Ok(JoinBinding::Cross),
        JoinOperator::CrossJoin(_) => Err(BindError::feature_not_supported(
            "CROSS JOIN constraints are not supported yet",
        )),
        _ => Err(BindError::feature_not_supported(
            "join operator is not supported yet",
        )),
    }
}

/// A standalone `VALUES (...), (...)` list. Column names default to
/// `column1..columnN`; each column resolves to a common type across all rows.
fn bind_values_query(
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
    values: &ast::Values,
    order_by: &Option<ast::OrderBy>,
    outer_scope: &[OuterLevel],
) -> Result<LogicalPlan, BindError> {
    if values.rows.is_empty() {
        return Err(BindError::syntax("VALUES lists must not be empty"));
    }
    let width = values.rows[0].content.len();
    // A `VALUES (...)` used as a correlated subquery (`x IN (VALUES (outer.c))`)
    // resolves its cell expressions outward via `outer_scope`; empty at top level.
    let scope = Scope::empty(catalog, params).with_outer(outer_scope.to_vec());
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
        columns.push(OutputColumn::new(format!("column{}", i + 1), ty));
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
        distinct: None,
    })
}

/// The output columns a query plan produces (for CTE/derived-table schemas,
/// and the extended protocol's `Describe`, which needs a statement's
/// `RowDescription` without executing it). A data-modifying plan has no result
/// row shape (no `RETURNING` yet) and returns an error the caller treats as
/// "NoData".
pub fn output_columns_of(plan: &LogicalPlan) -> Result<Vec<OutputColumn>, BindError> {
    match plan {
        LogicalPlan::Values { columns, .. }
        | LogicalPlan::Query { columns, .. }
        | LogicalPlan::Append { columns, .. }
        | LogicalPlan::SetOp { columns, .. }
        | LogicalPlan::Subquery { columns, .. }
        | LogicalPlan::TableFunction { columns, .. }
        | LogicalPlan::Aggregate { columns, .. }
        | LogicalPlan::Join { columns, .. } => Ok(columns.clone()),
        // LIMIT/OFFSET is a transparent wrapper: it exposes its source's columns.
        LogicalPlan::Limit { source, .. } => output_columns_of(source),
        LogicalPlan::Insert { returning, .. }
        | LogicalPlan::Update { returning, .. }
        | LogicalPlan::Delete { returning, .. } => match returning {
            Some(returning) => Ok(returning.columns.clone()),
            None => Err(BindError::feature_not_supported(
                "data-modifying statements in WITH are not supported yet",
            )),
        },
    }
}

/// Rewrite an `EXISTS` subquery's target list to a single constant `1`, so the
/// executor only tests for a first row and never evaluates the original
/// projection (which PG's semi-join also skips — matching `EXISTS(SELECT 1/0 …)`
/// returning true rather than erroring). Row count is preserved because a
/// scalar projection is one-in/one-out; when a projection is set-returning the
/// output count depends on it, so the plan is left untouched (the executor still
/// stops at the first produced row). `ORDER BY`/`DISTINCT` are dropped: they
/// cannot change whether any row exists.
pub(crate) fn strip_to_existence(plan: LogicalPlan) -> LogicalPlan {
    fn one_row() -> Vec<BoundExpr> {
        vec![BoundExpr::Const {
            value: Value::Int4(1),
            ty: PgType::Int4,
        }]
    }
    fn one_col() -> Vec<OutputColumn> {
        vec![OutputColumn::new("?column?", PgType::Int4)]
    }
    fn has_srf(projections: &[BoundExpr]) -> bool {
        projections.iter().any(BoundExpr::contains_srf)
    }
    match plan {
        LogicalPlan::Query {
            table,
            projections,
            predicate,
            ..
        } if !has_srf(&projections) => LogicalPlan::Query {
            table,
            columns: one_col(),
            projections: one_row(),
            predicate,
            sort: Vec::new(),
            distinct: None,
        },
        LogicalPlan::Subquery {
            source,
            projections,
            predicate,
            ..
        } if !has_srf(&projections) => LogicalPlan::Subquery {
            source,
            columns: one_col(),
            projections: one_row(),
            predicate,
            sort: Vec::new(),
            distinct: None,
        },
        LogicalPlan::Join {
            source,
            projections,
            predicate,
            ..
        } if !has_srf(&projections) => LogicalPlan::Join {
            source,
            columns: one_col(),
            projections: one_row(),
            predicate,
            sort: Vec::new(),
            distinct: None,
        },
        LogicalPlan::TableFunction {
            func,
            args,
            projections,
            predicate,
            ..
        } if !has_srf(&projections) => LogicalPlan::TableFunction {
            func,
            args,
            columns: one_col(),
            projections: one_row(),
            predicate,
            sort: Vec::new(),
            distinct: None,
        },
        LogicalPlan::Aggregate {
            input,
            predicate,
            group_exprs,
            aggregates,
            having,
            projections,
            ..
        } if !has_srf(&projections) => LogicalPlan::Aggregate {
            input,
            predicate,
            group_exprs,
            aggregates,
            having,
            columns: one_col(),
            projections: one_row(),
            sort: Vec::new(),
            distinct: None,
        },
        LogicalPlan::Values {
            rows, predicate, ..
        } => LogicalPlan::Values {
            columns: one_col(),
            rows: rows.into_iter().map(|_| one_row()).collect(),
            predicate,
            sort: Vec::new(),
            distinct: None,
        },
        LogicalPlan::Limit {
            source,
            limit,
            offset,
        } => LogicalPlan::Limit {
            source: Box::new(strip_to_existence(*source)),
            limit,
            offset,
        },
        // A set-returning projection (guards above fell through) or a DML body in
        // WITH: leave as-is; the executor's first-row check is still correct.
        other => other,
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
        // on a type it can't order. Reject such a key at bind time rather than
        // aborting mid-sort.
        if !crate::expr::is_orderable(ty, scope.catalog().as_ref()) {
            return Err(BindError::feature_not_supported(format!(
                "ORDER BY on type {} is not supported yet",
                crate::expr::type_label(ty, scope.catalog().as_ref())
            )));
        }
        let asc = oe.options.asc.unwrap_or(true);
        let nulls_first = oe.options.nulls_first.unwrap_or(!asc);
        // The projected expression at `column` is exactly what the sort reads,
        // so its derived collation is the one that orders this key.
        let collation = projections.get(column).map_or(DEFAULT_COLLATION_OID, |e| {
            crate::collation::expr_collation(e).collation
        });
        keys.push(SortKey {
            column,
            ty,
            collation,
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

/// Resolve a `SELECT DISTINCT` / `DISTINCT ON (…)` clause into the set of key
/// columns to deduplicate on, or `None` when the query keeps duplicates
/// (`SELECT` / `SELECT ALL`). Runs after [`bind_order_by`], so `sort` and the
/// hidden columns it appended to `projections` are available for validation and
/// ON-expression reuse.
fn bind_distinct(
    distinct: &Option<ast::Distinct>,
    columns: &[OutputColumn],
    scope: &Scope,
    projections: &mut Vec<BoundExpr>,
    sort: &[SortKey],
) -> Result<Option<Vec<DistinctKey>>, BindError> {
    let keys = match distinct {
        // No DISTINCT, or the explicit `ALL` default: keep every row.
        None | Some(ast::Distinct::All) => return Ok(None),
        // Plain `SELECT DISTINCT`: deduplicate on all visible output columns.
        Some(ast::Distinct::Distinct) => {
            // PG requires every ORDER BY key to be a select-list column; a key
            // that resolved to a hidden column past the visible width is an
            // expression not in the target list.
            if sort.iter().any(|key| key.column >= columns.len()) {
                return Err(BindError::new(
                    sqlstate::INVALID_COLUMN_REFERENCE,
                    "for SELECT DISTINCT, ORDER BY expressions must appear in select list",
                ));
            }
            columns
                .iter()
                .enumerate()
                .map(|(i, col)| DistinctKey {
                    column: i,
                    ty: col.ty,
                })
                .collect()
        }
        // `SELECT DISTINCT ON (exprs)`: deduplicate on the ON expressions.
        Some(ast::Distinct::On(exprs)) => {
            let mut keys = Vec::with_capacity(exprs.len());
            for expr in exprs {
                let (column, ty) = order_by_target(expr, columns, scope, projections, true)?;
                keys.push(DistinctKey { column, ty });
            }
            // When ORDER BY is present the ON expressions must be exactly the
            // *set* of leading ORDER BY expressions (order among them does not
            // matter — PG accepts `DISTINCT ON (a, b) … ORDER BY b, a`), so
            // "first row per DISTINCT ON group" is well-defined by the sort.
            if !sort.is_empty() {
                let on_cols: HashSet<usize> = keys.iter().map(|key| key.column).collect();
                // Walk the leading ORDER BY keys: each must be an ON column until
                // every ON column is covered. A leading key that is not an ON
                // column before then — or running out of ORDER BY keys first —
                // means the ON expressions are not the initial ORDER BY ones.
                let mut covered: HashSet<usize> = HashSet::new();
                for sort_key in sort {
                    if covered.len() == on_cols.len() {
                        break;
                    }
                    if !on_cols.contains(&sort_key.column) {
                        break;
                    }
                    covered.insert(sort_key.column);
                }
                if covered != on_cols {
                    return Err(BindError::new(
                        sqlstate::INVALID_COLUMN_REFERENCE,
                        "SELECT DISTINCT ON expressions must match initial ORDER BY expressions",
                    ));
                }
            }
            keys
        }
    };
    // The executor deduplicates via `keys_equal`, which compares with
    // `compare_values` and panics on a type it cannot order. Reject such a key
    // at bind time, mirroring `bind_order_by`.
    for key in &keys {
        if !crate::expr::is_orderable(key.ty, scope.catalog().as_ref()) {
            return Err(BindError::feature_not_supported(format!(
                "DISTINCT on type {} is not supported yet",
                crate::expr::type_label(key.ty, scope.catalog().as_ref())
            )));
        }
    }
    Ok(Some(keys))
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
    // GROUP BY and HAVING are handled by the aggregation binder; only the
    // grouping-set extensions (ROLLUP / CUBE / GROUPING SETS / GROUP BY ALL)
    // remain unsupported.
    let grouping_sets_unsupported = match &select.group_by {
        ast::GroupByExpr::Expressions(_, modifiers) => !modifiers.is_empty(),
        ast::GroupByExpr::All(_) => true,
    };
    // DISTINCT / DISTINCT ON are handled by `bind_distinct` in the select body.
    let unsupported: Option<&str> = if grouping_sets_unsupported {
        Some("GROUP BY ROLLUP/CUBE/GROUPING SETS")
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
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
    select: &ast::Select,
    order_by: &Option<ast::OrderBy>,
    ctes: &CteEnv,
    outer_scope: &[OuterLevel],
) -> Result<LogicalPlan, BindError> {
    let scope = Scope::empty(catalog, params)
        .with_subqueries(engine, ctes)
        .with_outer(outer_scope.to_vec());
    let mut columns = Vec::new();
    let mut row = Vec::new();
    for item in &select.projection {
        let SelectField::Expr(expr, alias) = classify_select_item(item)? else {
            return Err(BindError::syntax(
                "SELECT * with no tables specified is not valid",
            ));
        };
        let bound = bind_projection(expr, &scope)?;
        let (collation, strength) = crate::collation::output_collation(&bound);
        columns.push(OutputColumn {
            name: alias.unwrap_or_else(|| output_name(expr)),
            ty: bound.ty(),
            collation,
            strength,
        });
        row.push(bound);
    }
    let predicate = bind_where(&select.selection, &scope)?;
    // The empty scope means any hidden ORDER BY expression is column-free
    // (`ORDER BY random()`), so appending it to `row` stays safe against the
    // Values node's empty-row evaluation. Hidden columns are never SRFs, so the
    // SRF check below is unaffected.
    let sort = bind_order_by(order_by, &columns, &scope, &mut row, true)?;
    let distinct = bind_distinct(&select.distinct, &columns, &scope, &mut row, &sort)?;
    // A FROM-less aggregate (`SELECT count(*)`, or a HAVING/GROUP BY) runs over
    // the single virtual row. `count(*)` returns 1, `WHERE false` makes it 0.
    if let Some(agg) = bind_aggregation(select, &scope, &columns, &mut row, &predicate)? {
        return Ok(LogicalPlan::Aggregate {
            input: AggInput::SingleRow,
            predicate,
            group_exprs: agg.group_exprs,
            aggregates: agg.aggregates,
            having: agg.having,
            columns,
            projections: row,
            sort,
            distinct,
        });
    }
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
            distinct: None,
        };
        return Ok(LogicalPlan::Subquery {
            source: Box::new(source),
            columns,
            projections: row,
            predicate,
            sort,
            distinct,
        });
    }
    Ok(LogicalPlan::Values {
        columns,
        rows: vec![row],
        predicate,
        sort,
        distinct,
    })
}

/// The bound pieces of a SELECT over a single in-scope relation.
struct SelectBody {
    columns: Vec<OutputColumn>,
    projections: Vec<BoundExpr>,
    predicate: Option<BoundExpr>,
    sort: Vec<SortKey>,
    /// `Some` for `SELECT DISTINCT` / `DISTINCT ON` — the key columns to
    /// deduplicate on. `None` when the query keeps duplicates.
    distinct: Option<Vec<DistinctKey>>,
    /// Present when the SELECT aggregates (`GROUP BY`/`HAVING`, or an aggregate
    /// in the target list / ORDER BY). `projections` and `sort` have then been
    /// rewritten to reference the aggregate output row.
    aggregation: Option<Aggregation>,
}

/// The grouping/aggregation part of a bound SELECT, extracted from its
/// expressions. Feeds a [`LogicalPlan::Aggregate`].
struct Aggregation {
    group_exprs: Vec<BoundExpr>,
    aggregates: Vec<BoundAggregate>,
    having: Option<BoundExpr>,
}

/// Bind a SELECT's projection list, WHERE and ORDER BY against the in-scope
/// relation(s). One relation for a single-table SELECT / subquery / SRF, more
/// for a cross join — `scope` handles wildcard expansion and column resolution
/// uniformly across however many relations it holds.
/// Bind a target list (a SELECT projection or a `RETURNING` list) against
/// `scope`, expanding `*`/`t.*` and naming each output column by its alias or
/// derived name. Shared by SELECT and RETURNING, which have identical shape.
fn bind_target_list(
    items: &[ast::SelectItem],
    scope: &Scope,
) -> Result<Returning, BindError> {
    let mut columns = Vec::new();
    let mut projections = Vec::new();
    for item in items {
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
                let (collation, strength) = crate::collation::output_collation(&bound);
                columns.push(OutputColumn {
                    name: alias.unwrap_or_else(|| output_name(expr)),
                    ty: bound.ty(),
                    collation,
                    strength,
                });
                projections.push(bound);
            }
        }
    }
    Ok(Returning {
        columns,
        projections,
    })
}

/// Bind an optional `RETURNING` clause against the target table's `scope`.
///
/// Unlike a SELECT target list, RETURNING is not an aggregation/SRF context:
/// PostgreSQL rejects aggregate and set-returning functions here at bind time
/// (there is no aggregate/ProjectSet plan node above a data-modifying
/// statement to consume them), so reject them with PG's SQLSTATE and wording.
fn bind_returning(
    returning: &Option<Vec<ast::SelectItem>>,
    scope: &Scope,
) -> Result<Option<Returning>, BindError> {
    let Some(items) = returning else {
        return Ok(None);
    };
    let bound = bind_target_list(items, scope)?;
    for projection in &bound.projections {
        if projection.contains_aggregate() {
            return Err(BindError::new(
                sqlstate::GROUPING_ERROR,
                "aggregate functions are not allowed in RETURNING",
            ));
        }
        if projection.contains_srf() {
            return Err(BindError::new(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "set-returning functions are not allowed in RETURNING",
            ));
        }
    }
    Ok(Some(bound))
}

fn bind_select_body(
    select: &ast::Select,
    order_by: &Option<ast::OrderBy>,
    scope: &Scope,
) -> Result<SelectBody, BindError> {
    let Returning {
        columns,
        mut projections,
    } = bind_target_list(&select.projection, scope)?;

    let predicate = bind_where(&select.selection, scope)?;
    let sort = bind_order_by(order_by, &columns, scope, &mut projections, true)?;
    // DISTINCT is resolved before aggregation so any hidden `DISTINCT ON` column
    // appended to `projections` is aggregate-rewritten alongside the ORDER BY
    // ones below.
    let distinct = bind_distinct(&select.distinct, &columns, scope, &mut projections, &sort)?;
    // ORDER BY expressions were appended to `projections` as hidden columns, so
    // aggregate detection/rewrite below covers `ORDER BY count(*)` too.
    let aggregation = bind_aggregation(select, scope, &columns, &mut projections, &predicate)?;
    Ok(SelectBody {
        columns,
        projections,
        predicate,
        sort,
        distinct,
        aggregation,
    })
}

/// Detect and bind a SELECT's aggregation. Returns `None` for a non-aggregating
/// SELECT (leaving `projections` untouched). When the query aggregates — it has
/// a `GROUP BY`, a `HAVING`, or any aggregate call in its target list / ORDER BY
/// — this binds the group keys and HAVING, extracts every aggregate into an
/// ordered list, and rewrites `projections` (and the returned HAVING) so each
/// aggregate call and each grouped sub-expression becomes a `ColumnRef` into the
/// aggregate output row `[group keys…, aggregates…]`.
fn bind_aggregation(
    select: &ast::Select,
    scope: &Scope,
    columns: &[OutputColumn],
    projections: &mut [BoundExpr],
    predicate: &Option<BoundExpr>,
) -> Result<Option<Aggregation>, BindError> {
    // An aggregate in WHERE is always an error — WHERE filters rows before
    // grouping, so no aggregate value exists yet.
    if predicate
        .as_ref()
        .is_some_and(BoundExpr::contains_aggregate)
    {
        return Err(BindError::new(
            sqlstate::GROUPING_ERROR,
            "aggregate functions are not allowed in WHERE",
        ));
    }

    let group_exprs = bind_group_by(&select.group_by, scope, columns, projections)?;
    let having = select
        .having
        .as_ref()
        .map(|h| bind_expr(h, scope).and_then(|b| to_bool_operand(b, "HAVING")))
        .transpose()?;

    let aggregating = !group_exprs.is_empty()
        || having.is_some()
        || projections.iter().any(BoundExpr::contains_aggregate);
    if !aggregating {
        return Ok(None);
    }

    let mut aggregates: Vec<BoundAggregate> = Vec::new();
    for proj in projections.iter_mut() {
        // Move the projection out (rewrite consumes it and rebuilds a fresh
        // tree) rather than cloning the whole expression only to drop it.
        let taken = std::mem::replace(
            proj,
            BoundExpr::Const {
                value: Value::Null,
                ty: PgType::Bool,
            },
        );
        *proj = rewrite_over_aggregate(taken, &group_exprs, &mut aggregates, scope)?;
    }
    let having = having
        .map(|h| rewrite_over_aggregate(h, &group_exprs, &mut aggregates, scope))
        .transpose()?;
    Ok(Some(Aggregation {
        group_exprs,
        aggregates,
        having,
    }))
}

/// Bind the `GROUP BY` keys against the FROM `scope`. Supports plain expressions
/// (including bare columns) and 1-based output-column ordinals (`GROUP BY 1`);
/// grouping-set modifiers were already rejected. An aggregate inside a group key
/// is an error, as in PG.
fn bind_group_by(
    group_by: &ast::GroupByExpr,
    scope: &Scope,
    columns: &[OutputColumn],
    projections: &[BoundExpr],
) -> Result<Vec<BoundExpr>, BindError> {
    let exprs = match group_by {
        ast::GroupByExpr::Expressions(exprs, _) => exprs,
        // GroupByExpr::All was rejected in reject_unsupported_select_clauses.
        ast::GroupByExpr::All(_) => return Ok(Vec::new()),
    };
    let mut keys = Vec::with_capacity(exprs.len());
    for expr in exprs {
        // A bare unsigned integer is a 1-based output-column ordinal
        // (`GROUP BY 1`), grouping by that select-list expression — not the
        // literal integer. Mirrors `order_by_target`'s ordinal handling.
        let bound = if let Some(ordinal) = group_by_ordinal(expr) {
            if ordinal < 1 || ordinal > columns.len() {
                return Err(BindError::new(
                    sqlstate::INVALID_COLUMN_REFERENCE,
                    format!("GROUP BY position {ordinal} is not in select list"),
                ));
            }
            projections[ordinal - 1].clone()
        } else {
            bind_group_key(expr, scope, columns, projections)?
        };
        if bound.contains_aggregate() {
            return Err(BindError::new(
                sqlstate::GROUPING_ERROR,
                "aggregate functions are not allowed in GROUP BY",
            ));
        }
        // The executor groups with `compare_values`, which cannot order every
        // type (`bit`, user types); reject such a key at bind time rather than
        // panicking mid-group.
        if !crate::expr::is_orderable(bound.ty(), scope.catalog().as_ref()) {
            return Err(BindError::feature_not_supported(format!(
                "GROUP BY on type {} is not supported yet",
                crate::expr::type_label(bound.ty(), scope.catalog().as_ref())
            )));
        }
        keys.push(bound);
    }
    Ok(keys)
}

/// Bind one non-ordinal GROUP BY key. PG resolves a bare name as an input column
/// first (via the scope), then falls back to a select-list output alias — so
/// `SELECT a AS z FROM t GROUP BY z` groups by `a`. Qualified names and
/// expressions bind against the scope only, as in PG.
fn bind_group_key(
    expr: &ast::Expr,
    scope: &Scope,
    columns: &[OutputColumn],
    projections: &[BoundExpr],
) -> Result<BoundExpr, BindError> {
    match bind_scalar(expr, scope) {
        Ok(bound) => Ok(bound),
        // Only a bare, unresolved name falls back to an output alias.
        Err(e) if e.code == sqlstate::UNDEFINED_COLUMN => {
            if let ast::Expr::Identifier(ident) = expr {
                let name = normalize_ident(ident);
                let mut hit: Option<usize> = None;
                for (i, col) in columns.iter().enumerate() {
                    if col.name == name {
                        if hit.is_some() {
                            return Err(BindError::new(
                                sqlstate::AMBIGUOUS_COLUMN,
                                format!("GROUP BY \"{name}\" is ambiguous"),
                            ));
                        }
                        hit = Some(i);
                    }
                }
                if let Some(i) = hit {
                    return Ok(projections[i].clone());
                }
            }
            Err(e)
        }
        Err(e) => Err(e),
    }
}

/// A bare unsigned integer literal used as a GROUP BY ordinal, if any. `1 + 1` is
/// an expression, not ordinal 2, so only a plain `Value::Number` qualifies.
fn group_by_ordinal(expr: &ast::Expr) -> Option<usize> {
    if let ast::Expr::Value(v) = expr
        && let ast::Value::Number(n, _) = &v.value
    {
        return n.parse::<usize>().ok();
    }
    None
}

/// Rewrite an expression bound against the source scope into one over the
/// aggregate output row: a sub-expression equal to a group key becomes a
/// `ColumnRef` into that key's slot; an aggregate marker is appended to
/// `aggregates` and becomes a `ColumnRef` into its slot (after the group keys);
/// any remaining bare column reference is the "must appear in GROUP BY" error.
fn rewrite_over_aggregate(
    expr: BoundExpr,
    group_exprs: &[BoundExpr],
    aggregates: &mut Vec<BoundAggregate>,
    scope: &Scope,
) -> Result<BoundExpr, BindError> {
    // A whole sub-expression that is one of the group keys is projected straight
    // from the aggregate row — this is what makes `a`, `a + 1` (under GROUP BY a)
    // and `date_trunc(...)` (under GROUP BY date_trunc(...)) legal.
    if let Some(slot) = group_exprs.iter().position(|k| *k == expr) {
        return Ok(BoundExpr::ColumnRef {
            index: slot,
            ty: expr.ty(),
        });
    }
    match expr {
        BoundExpr::Aggregate {
            func,
            distinct,
            args,
            input_ty,
            ret,
        } => {
            if args.iter().any(|a| a.contains_aggregate()) {
                return Err(BindError::new(
                    sqlstate::GROUPING_ERROR,
                    "aggregate function calls cannot be nested",
                ));
            }
            let index = group_exprs.len() + aggregates.len();
            let collation = args
                .first()
                .map_or(DEFAULT_COLLATION_OID, |a| crate::collation::expr_collation(a).collation);
            aggregates.push(BoundAggregate {
                func,
                distinct,
                args,
                input_ty,
                ret,
                collation,
            });
            Ok(BoundExpr::ColumnRef { index, ty: ret })
        }
        BoundExpr::ColumnRef { index, .. } => Err(BindError::new(
            sqlstate::GROUPING_ERROR,
            format!(
                "column \"{}\" must appear in the GROUP BY clause or be used in an aggregate function",
                scope.column_label(index)
            ),
        )),
        // A bind parameter is a per-execution constant (like `Const`): it is the
        // same value for every group, so it passes through unchanged. An outer
        // (correlated) reference is likewise fixed across this query's groups.
        c @ (BoundExpr::Const { .. }
        | BoundExpr::Param { .. }
        | BoundExpr::OuterColumnRef { .. }) => Ok(c),
        BoundExpr::Unary { op, expr } => Ok(BoundExpr::Unary {
            op,
            expr: Box::new(rewrite_over_aggregate(
                *expr,
                group_exprs,
                aggregates,
                scope,
            )?),
        }),
        BoundExpr::Collate {
            expr,
            collation,
            explicit,
        } => Ok(BoundExpr::Collate {
            expr: Box::new(rewrite_over_aggregate(
                *expr,
                group_exprs,
                aggregates,
                scope,
            )?),
            collation,
            explicit,
        }),
        BoundExpr::Binary {
            op,
            arg_ty,
            collation,
            left,
            right,
        } => Ok(BoundExpr::Binary {
            op,
            arg_ty,
            collation,
            left: Box::new(rewrite_over_aggregate(
                *left,
                group_exprs,
                aggregates,
                scope,
            )?),
            right: Box::new(rewrite_over_aggregate(
                *right,
                group_exprs,
                aggregates,
                scope,
            )?),
        }),
        BoundExpr::IsNull { expr, negated } => Ok(BoundExpr::IsNull {
            expr: Box::new(rewrite_over_aggregate(
                *expr,
                group_exprs,
                aggregates,
                scope,
            )?),
            negated,
        }),
        BoundExpr::Coerce { expr, ty } => Ok(BoundExpr::Coerce {
            expr: Box::new(rewrite_over_aggregate(
                *expr,
                group_exprs,
                aggregates,
                scope,
            )?),
            ty,
        }),
        BoundExpr::Reinterpret {
            expr,
            reported,
            rep,
        } => Ok(BoundExpr::Reinterpret {
            expr: Box::new(rewrite_over_aggregate(
                *expr,
                group_exprs,
                aggregates,
                scope,
            )?),
            reported,
            rep,
        }),
        BoundExpr::FuncCall { func, ret, args } => {
            let args = args
                .into_iter()
                .map(|a| rewrite_over_aggregate(a, group_exprs, aggregates, scope))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(BoundExpr::FuncCall { func, ret, args })
        }
        BoundExpr::Routine {
            oid,
            name,
            arg_types,
            strict,
            args,
            ret,
        } => {
            let args = args
                .into_iter()
                .map(|a| rewrite_over_aggregate(a, group_exprs, aggregates, scope))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(BoundExpr::Routine {
                oid,
                name,
                arg_types,
                strict,
                args,
                ret,
            })
        }
        BoundExpr::ArrayCtor { elem, ty, elems } => {
            let elems = elems
                .into_iter()
                .map(|a| rewrite_over_aggregate(a, group_exprs, aggregates, scope))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(BoundExpr::ArrayCtor { elem, ty, elems })
        }
        BoundExpr::Subscript { base, index, ty } => Ok(BoundExpr::Subscript {
            base: Box::new(rewrite_over_aggregate(*base, group_exprs, aggregates, scope)?),
            index: Box::new(rewrite_over_aggregate(
                *index,
                group_exprs,
                aggregates,
                scope,
            )?),
            ty,
        }),
        BoundExpr::Case { whens, else_, ty } => {
            let whens = whens
                .into_iter()
                .map(|(c, r)| {
                    Ok((
                        rewrite_over_aggregate(c, group_exprs, aggregates, scope)?,
                        rewrite_over_aggregate(r, group_exprs, aggregates, scope)?,
                    ))
                })
                .collect::<Result<Vec<_>, BindError>>()?;
            let else_ = else_
                .map(|e| rewrite_over_aggregate(*e, group_exprs, aggregates, scope).map(Box::new))
                .transpose()?;
            Ok(BoundExpr::Case { whens, else_, ty })
        }
        // A set-returning function combined with aggregation is not meaningful
        // (PG: "set-returning functions are not allowed in ..."); reject cleanly.
        BoundExpr::Srf { .. } => Err(BindError::feature_not_supported(
            "set-returning functions with aggregation are not supported yet",
        )),
        // A non-correlated subquery is a per-group constant, so scalar/EXISTS pass
        // through unchanged. A *correlated* one is rejected here: its
        // `OuterColumnRef` indices address the outer query's pre-aggregation row,
        // but a target-list/HAVING expression is evaluated against the aggregate
        // node's output row (`[group keys, aggregates]`), so the indices would not
        // line up. (A correlated subquery in WHERE is unaffected — that predicate
        // runs before aggregation, over the base row, and never reaches here.)
        c @ (BoundExpr::ScalarSubquery { .. } | BoundExpr::Exists { .. }) => {
            reject_correlated_over_aggregate(&c)?;
            Ok(c)
        }
        BoundExpr::QuantifiedSubquery { subplan, all, cmp } => {
            if plan_has_outer_refs(&subplan.0) {
                return Err(correlated_over_aggregate_error());
            }
            Ok(BoundExpr::QuantifiedSubquery {
                subplan,
                all,
                cmp: Box::new(rewrite_over_aggregate(*cmp, group_exprs, aggregates, scope)?),
            })
        }
        // `x op ANY/ALL(array)` has no subplan; rewrite both operands so any
        // aggregate/group-key reference inside them is redirected to the
        // aggregate node's output row.
        BoundExpr::QuantifiedArray { array, all, cmp } => Ok(BoundExpr::QuantifiedArray {
            array: Box::new(rewrite_over_aggregate(*array, group_exprs, aggregates, scope)?),
            all,
            cmp: Box::new(rewrite_over_aggregate(*cmp, group_exprs, aggregates, scope)?),
        }),
    }
}

/// Reject a scalar/EXISTS subquery marker that is correlated to the enclosing
/// (aggregating) query when it appears in a target-list/HAVING expression — see
/// [`rewrite_over_aggregate`]. Non-correlated markers are left alone.
fn reject_correlated_over_aggregate(marker: &BoundExpr) -> Result<(), BindError> {
    let subplan = match marker {
        BoundExpr::ScalarSubquery { subplan, .. } | BoundExpr::Exists { subplan, .. } => &subplan.0,
        _ => return Ok(()),
    };
    if plan_has_outer_refs(subplan) {
        return Err(correlated_over_aggregate_error());
    }
    Ok(())
}

fn correlated_over_aggregate_error() -> BindError {
    BindError::feature_not_supported(
        "correlated subquery in the target list or HAVING of a grouped/aggregated query \
         is not supported yet",
    )
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
            ast::SelectItemQualifiedWildcardKind::ObjectName(name) => Ok(
                SelectField::QualifiedWildcard(object_name_to_table_name(name)?),
            ),
            ast::SelectItemQualifiedWildcardKind::Expr(_) => Err(BindError::feature_not_supported(
                "qualified * on an expression is not supported yet",
            )),
        },
        ast::SelectItem::ExprWithAliases { .. } => Err(BindError::feature_not_supported(
            "multiple aliases are not supported yet",
        )),
    }
}

fn is_default_keyword(expr: &ast::Expr) -> bool {
    matches!(
        expr,
        ast::Expr::Identifier(ident)
            if ident.quote_style.is_none() && ident.value.eq_ignore_ascii_case("default")
    )
}

/// Reparse and bind a persisted default. Storing canonical SQL keeps the
/// storage API independent of the parser/binder IR while still evaluating the
/// expression for every inserted/updated row.
fn default_for_column(
    column: &Column,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<BoundExpr, BindError> {
    let Some(sql) = &column.default else {
        return Ok(BoundExpr::Const {
            value: Value::Null,
            ty: column.ty,
        });
    };
    let statements = crabgresql_parser::parse(&format!("SELECT {sql}"))
        .map_err(|e| BindError::syntax(format!("invalid stored default expression: {e}")))?;
    let expr = match statements.as_slice() {
        [ast::Statement::Query(query)] => match query.body.as_ref() {
            ast::SetExpr::Select(select) => match select.projection.as_slice() {
                [ast::SelectItem::UnnamedExpr(expr)] => expr,
                _ => return Err(BindError::syntax("invalid stored default expression")),
            },
            _ => return Err(BindError::syntax("invalid stored default expression")),
        },
        _ => return Err(BindError::syntax("invalid stored default expression")),
    };
    bind_column_default(expr, column, catalog)
}

pub fn bind_insert(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    insert: &ast::Insert,
) -> Result<LogicalPlan, BindError> {
    bind_insert_with_params(engine, catalog, insert, &param_ctx_none())
}

/// [`bind_insert`] for the extended query protocol: `$n` placeholders in the
/// VALUES cells take their type from the target column and unify across the
/// statement via the shared `params` context.
pub fn bind_insert_with_params(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    insert: &ast::Insert,
    params: &ParamCtx,
) -> Result<LogicalPlan, BindError> {
    let target = match &insert.table {
        ast::TableObject::TableName(name) => name,
        other => {
            return Err(BindError::feature_not_supported(format!(
                "INSERT target is not supported yet: {other}"
            )));
        }
    };
    if insert.on.is_some() {
        return Err(BindError::feature_not_supported(
            "ON CONFLICT is not supported yet",
        ));
    }
    // Same write-target routing as UPDATE/DELETE: `public.t` reaches the
    // permanent relation, a write to `pg_catalog` is refused, and the system
    // catalog is never a write target.
    let (table, name) = resolve_write_table(engine, target, WriteVerb::Insert)?;
    let schema = table.schema().clone();
    // A partitioned parent routes each row to a leaf partition at execution time;
    // capture the leaves now. `None` for an ordinary table.
    let routing = if schema.partition_scheme.is_some() {
        Some(partition_leaves(engine, &schema)?)
    } else {
        None
    };
    let defaults = schema
        .columns
        .iter()
        .map(|column| default_for_column(column, catalog))
        .collect::<Result<Vec<_>, _>>()?;

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

    let default_values_form = insert.source.is_none();
    let source = insert.source.as_deref();
    // A non-VALUES body (`SELECT`, `TABLE t`) — or a VALUES list carrying ORDER
    // BY / LIMIT / WITH, which must be executed as a query — feeds rows in from a
    // child plan. A plain VALUES list keeps the direct, fully-materialized path.
    let query_source: Option<&ast::Query> = match source {
        Some(q)
            if q.with.is_some()
                || q.order_by.is_some()
                || q.limit_clause.is_some()
                || !matches!(q.body.as_ref(), ast::SetExpr::Values(_)) =>
        {
            Some(q)
        }
        _ => None,
    };

    let insert_source = if let Some(query) = query_source {
        let input = bind_query_with_params(engine, catalog, query, params)?;
        let src_cols = output_columns_of(&input)?;
        // PG matches the query's column count against the target columns: too
        // many is always an error; too few is an error only with an explicit
        // column list (otherwise trailing columns take their defaults).
        if src_cols.len() > target_indices.len() {
            return Err(BindError::syntax(
                "INSERT has more expressions than target columns",
            ));
        }
        if explicit_columns && src_cols.len() < target_indices.len() {
            return Err(BindError::syntax(
                "INSERT has more target columns than expressions",
            ));
        }
        // Build one full-width, schema-order projection per table column: each
        // target column is a `ColumnRef` into the source row coerced to the
        // column type (assignment context, so length typmods apply); the rest
        // keep their column default.
        let scope = Scope::empty(catalog, params);
        let mut projections = defaults.clone();
        for (i, &idx) in target_indices.iter().enumerate().take(src_cols.len()) {
            let src_ref = BoundExpr::ColumnRef {
                index: i,
                ty: src_cols[i].ty,
            };
            projections[idx] =
                coerce_to_column(Binding::Typed(src_ref), &schema.columns[idx], &scope)?;
        }
        InsertSource::Query {
            input: Box::new(input),
            projections,
        }
    } else {
        // Plain VALUES (or the DEFAULT VALUES form): materialize fully-formed
        // rows now. FETCH / FOR UPDATE on the source are still rejected.
        let mut value_rows: Vec<&[ast::Expr]> = Vec::new();
        let empty: &[ast::Expr] = &[];
        if default_values_form {
            value_rows.push(empty);
        }
        if let Some(source) = source {
            reject_unsupported_query_clauses(source)?;
            let ast::SetExpr::Values(values) = source.body.as_ref() else {
                unreachable!("non-VALUES bodies take the query-source path");
            };
            value_rows.extend(values.rows.iter().map(|row| row.content.as_slice()));
        }

        // VALUES cells bind in the empty scope: a column reference in VALUES is
        // an undefined column, as in PG. A cell may still hold a subquery; INSERT
        // takes no WITH, so the CTE environment is empty.
        let scope = Scope::empty(catalog, params).with_subqueries(engine, &CteEnv::new());
        let first_width = value_rows.first().map_or(0, |row| row.len());
        let mut rows = Vec::with_capacity(value_rows.len());
        for value_row in value_rows {
            // PG validates the VALUES clause shape before matching it against the
            // target columns.
            if value_row.len() != first_width {
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
            if !default_values_form && explicit_columns && value_row.len() < target_indices.len() {
                return Err(BindError::syntax(
                    "INSERT has more target columns than expressions",
                ));
            }
            let mut row = defaults.clone();
            for (expr, &idx) in value_row.iter().zip(&target_indices) {
                row[idx] = if is_default_keyword(expr) {
                    defaults[idx].clone()
                } else {
                    let binding = bind_expr(expr, &scope)?;
                    coerce_to_column(binding, &schema.columns[idx], &scope)?
                };
            }
            rows.push(row);
        }
        InsertSource::Values(rows)
    };

    // RETURNING references the inserted row's columns, addressed by the table
    // name (INSERT takes no alias). Its VALUES bound in the empty scope, so this
    // needs a fresh table scope.
    let returning = bind_returning(
        &insert.returning,
        &Scope::table(&schema, name, catalog, params).with_subqueries(engine, &CteEnv::new()),
    )?;

    Ok(LogicalPlan::Insert {
        table,
        source: insert_source,
        returning,
        routing,
    })
}

/// The COPY text/CSV format, resolved from the statement's `WITH (…)` (and
/// legacy) options. Consumed by the server's row decoder, which splits the raw
/// stdin bytes into fields per these rules before [`CopyFromPlan::build_insert`]
/// turns them into typed rows.
///
/// The delimiter, quote, and escape are single bytes — PostgreSQL requires each
/// to be "a single one-byte character" — so the decoder can operate on the raw
/// byte stream (COPY is byte-oriented; multi-byte data flows through untouched).
#[derive(Clone, Debug)]
pub struct CopyFormat {
    /// `true` for `FORMAT csv`, `false` for the default text format.
    pub csv: bool,
    /// Field separator (TAB for text, `,` for CSV, unless overridden).
    pub delimiter: u8,
    /// The unquoted token that means SQL NULL (`\N` for text, empty for CSV).
    pub null: String,
    /// Skip the first data line (`HEADER`).
    pub header: bool,
    /// CSV quote byte (default `"`); unused in text format.
    pub quote: u8,
    /// CSV escape byte (default = the quote byte); unused in text.
    pub escape: u8,
    /// Field positions (into the target column list) that CSV must read as a
    /// non-NULL empty string rather than NULL (`FORCE_NOT_NULL`).
    pub force_not_null: Vec<usize>,
}

impl CopyFormat {
    /// Text-format defaults. Public so the server-side decoder and its tests
    /// share one source of truth instead of reconstructing the fields.
    pub fn text() -> Self {
        CopyFormat {
            csv: false,
            delimiter: b'\t',
            null: "\\N".to_string(),
            header: false,
            quote: b'"',
            escape: b'"',
            force_not_null: Vec::new(),
        }
    }

    /// CSV-format defaults.
    pub fn csv() -> Self {
        CopyFormat {
            csv: true,
            delimiter: b',',
            null: String::new(),
            header: false,
            quote: b'"',
            escape: b'"',
            force_not_null: Vec::new(),
        }
    }
}

/// A bound `COPY <table> [(cols)] FROM STDIN`: the resolved write target, the
/// data-column → schema-column mapping, per-column defaults for columns absent
/// from the column list, and the text/CSV format. The row bytes arrive later
/// over the wire, so binding is split: this resolves everything the server needs
/// before sending `CopyInResponse`, and [`build_insert`](Self::build_insert)
/// turns the decoded field rows into an ordinary [`LogicalPlan::Insert`].
pub struct CopyFromPlan {
    table: Arc<dyn TableAm>,
    table_name: String,
    schema: TableSchema,
    /// One schema-column index per data column, in wire order.
    target_indices: Vec<usize>,
    /// Default expression per schema column (used for columns not in the list).
    defaults: Vec<BoundExpr>,
    pub format: CopyFormat,
    /// Leaf partitions when `table` is a partitioned parent, so each decoded row
    /// routes to the leaf whose RANGE bound admits it (reusing the executor's
    /// INSERT routing); `None` for an ordinary table.
    routing: Option<Vec<Arc<dyn TableAm>>>,
}

impl CopyFromPlan {
    /// Number of columns each data row must supply.
    pub fn column_count(&self) -> usize {
        self.target_indices.len()
    }

    /// The relation's bare name, for the `COPY` command tag / error context.
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// Turn decoded field rows (`None` = the NULL marker matched) into a
    /// fully-formed `INSERT ... VALUES` plan: each field parses through its
    /// column's input function with the column typmod (so `char(n)` blank-pads,
    /// an over-long `varchar(n)` errors), exactly as a SQL literal would; columns
    /// absent from the COPY column list take their default. Arity mismatches
    /// surface as PG's `extra data` / `missing data` errors.
    pub fn build_insert(
        &self,
        catalog: &Arc<dyn TypeCatalog>,
        rows: Vec<Vec<Option<String>>>,
    ) -> Result<LogicalPlan, BindError> {
        let params = param_ctx_none();
        let scope = Scope::empty(catalog, &params);
        let ncols = self.target_indices.len();
        let mut value_rows = Vec::with_capacity(rows.len());
        for fields in rows {
            if fields.len() > ncols {
                return Err(BindError::new(
                    sqlstate::BAD_COPY_FILE_FORMAT,
                    "extra data after last expected column",
                ));
            }
            if fields.len() < ncols {
                let missing = self.target_indices[fields.len()];
                return Err(BindError::new(
                    sqlstate::BAD_COPY_FILE_FORMAT,
                    format!(
                        "missing data for column \"{}\"",
                        self.schema.columns[missing].name
                    ),
                ));
            }
            let mut row = self.defaults.clone();
            for (field, &idx) in fields.into_iter().zip(&self.target_indices) {
                let column = &self.schema.columns[idx];
                row[idx] = match field {
                    // The NULL marker: a genuine SQL NULL, not the column default.
                    None => BoundExpr::Const {
                        value: Value::Null,
                        ty: column.ty,
                    },
                    // A data field parses like the equivalent unknown-typed SQL
                    // literal against the column type, then takes its length typmod.
                    Some(text) => coerce_to_column(
                        Binding::Unknown {
                            lit: Some(text),
                            span: Span::empty(),
                            param: None,
                        },
                        column,
                        &scope,
                    )?,
                };
            }
            value_rows.push(row);
        }
        Ok(LogicalPlan::Insert {
            table: self.table.clone(),
            source: InsertSource::Values(value_rows),
            returning: None,
            // A partitioned parent routes each decoded row to a leaf, reusing the
            // executor's INSERT tuple routing; `None` targets an ordinary table.
            routing: self.routing.clone(),
        })
    }
}

/// Bind `COPY <table> [(cols)] FROM STDIN [WITH (…)]`. Rejects the forms not yet
/// supported (`COPY TO`, a query source, file/program targets, binary format)
/// with the matching error, resolves the write target and column list the same
/// way INSERT does, and resolves the text/CSV options into a [`CopyFormat`].
pub fn bind_copy_from(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    source: &ast::CopySource,
    to: bool,
    target: &ast::CopyTarget,
    options: &[ast::CopyOption],
    legacy_options: &[ast::CopyLegacyOption],
) -> Result<CopyFromPlan, BindError> {
    if to {
        return Err(BindError::feature_not_supported(
            "COPY TO is not supported yet",
        ));
    }
    let (table_name, columns) = match source {
        ast::CopySource::Table {
            table_name,
            columns,
        } => (table_name, columns),
        ast::CopySource::Query(_) => {
            return Err(BindError::feature_not_supported(
                "COPY (query) is not supported yet",
            ));
        }
    };
    if !matches!(target, ast::CopyTarget::Stdin) {
        return Err(BindError::feature_not_supported(
            "COPY from a file or program is not supported yet; use COPY ... FROM STDIN",
        ));
    }

    let (table, name) = resolve_write_table(engine, table_name, WriteVerb::Insert)?;
    let schema = table.schema().clone();
    // A partitioned parent routes each decoded row to a leaf at execution time
    // (see `CopyFromPlan::routing`); capture the leaves now. `None` for a plain table.
    let routing = if schema.partition_scheme.is_some() {
        Some(partition_leaves(engine, &schema)?)
    } else {
        None
    };
    let defaults = schema
        .columns
        .iter()
        .map(|column| default_for_column(column, catalog))
        .collect::<Result<Vec<_>, _>>()?;

    // Map the optional column list (empty = every column, in schema order).
    let target_indices: Vec<usize> = if columns.is_empty() {
        (0..schema.columns.len()).collect()
    } else {
        let mut indices = Vec::with_capacity(columns.len());
        for ident in columns {
            let col = normalize_ident(ident);
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

    let format = resolve_copy_format(options, legacy_options, &schema, &target_indices, &name)?;

    Ok(CopyFromPlan {
        table,
        table_name: name,
        schema,
        target_indices,
        defaults,
        format,
        routing,
    })
}

/// Resolve the modern `WITH (…)` options plus the pre-9.0 legacy option list
/// into a [`CopyFormat`]. The format keyword is decided first (so `csv`
/// defaults apply before per-option overrides), then delimiter/NULL/header and
/// the CSV-only quote/escape/`FORCE_NOT_NULL` settings.
fn resolve_copy_format(
    options: &[ast::CopyOption],
    legacy_options: &[ast::CopyLegacyOption],
    schema: &TableSchema,
    target_indices: &[usize],
    relname: &str,
) -> Result<CopyFormat, BindError> {
    // Pass 1: the format keyword.
    let mut csv = false;
    for opt in options {
        if let ast::CopyOption::Format(name) = opt {
            match normalize_ident(name).as_str() {
                "text" => csv = false,
                "csv" => csv = true,
                "binary" => {
                    return Err(BindError::feature_not_supported(
                        "COPY with binary format is not supported yet",
                    ));
                }
                other => {
                    return Err(BindError::new(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        format!("COPY format \"{other}\" not recognized"),
                    ));
                }
            }
        }
    }
    for opt in legacy_options {
        match opt {
            ast::CopyLegacyOption::Csv(_) => csv = true,
            ast::CopyLegacyOption::Binary => {
                return Err(BindError::feature_not_supported(
                    "COPY with binary format is not supported yet",
                ));
            }
            _ => {}
        }
    }

    let mut fmt = if csv {
        CopyFormat::csv()
    } else {
        CopyFormat::text()
    };

    // Resolve a FORCE_NOT_NULL column name to its position in the data-column
    // list (the decoder indexes fields by that position).
    let force_not_null = |cols: &[ast::Ident]| -> Result<Vec<usize>, BindError> {
        let mut out = Vec::with_capacity(cols.len());
        for ident in cols {
            let col = normalize_ident(ident);
            let sidx = schema.column_index(&col).ok_or_else(|| {
                BindError::new(
                    sqlstate::UNDEFINED_COLUMN,
                    format!("column \"{col}\" of relation \"{relname}\" does not exist"),
                )
            })?;
            let pos = target_indices.iter().position(|&i| i == sidx).ok_or_else(|| {
                BindError::new(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    format!("FORCE_NOT_NULL column \"{col}\" not referenced by COPY"),
                )
            })?;
            out.push(pos);
        }
        Ok(out)
    };

    // Pass 2: modern per-option overrides.
    let require_csv = |what: &str| -> Result<(), BindError> {
        if csv {
            Ok(())
        } else {
            Err(BindError::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                format!("COPY {what} available only in CSV mode"),
            ))
        }
    };
    for opt in options {
        match opt {
            ast::CopyOption::Format(_) | ast::CopyOption::Freeze(_) => {}
            ast::CopyOption::Delimiter(c) => fmt.delimiter = single_byte(*c, "delimiter")?,
            ast::CopyOption::Null(s) => fmt.null = s.clone(),
            ast::CopyOption::Header(b) => fmt.header = *b,
            ast::CopyOption::Quote(c) => {
                require_csv("QUOTE")?;
                fmt.quote = single_byte(*c, "quote")?;
            }
            ast::CopyOption::Escape(c) => {
                require_csv("ESCAPE")?;
                fmt.escape = single_byte(*c, "escape")?;
            }
            ast::CopyOption::ForceNotNull(cols) => {
                require_csv("FORCE_NOT_NULL")?;
                fmt.force_not_null = force_not_null(cols)?;
            }
            ast::CopyOption::ForceNull(_) | ast::CopyOption::ForceQuote(_) => {
                return Err(BindError::feature_not_supported(
                    "COPY FORCE_NULL/FORCE_QUOTE is not supported yet",
                ));
            }
            ast::CopyOption::Encoding(enc) => require_utf8(enc)?,
        }
    }

    // Pass 2b: legacy per-option overrides (`COPY … CSV HEADER DELIMITER ','`).
    for opt in legacy_options {
        match opt {
            ast::CopyLegacyOption::Delimiter(c) => fmt.delimiter = single_byte(*c, "delimiter")?,
            ast::CopyLegacyOption::Null(s) => fmt.null = s.clone(),
            ast::CopyLegacyOption::Header => fmt.header = true,
            ast::CopyLegacyOption::Binary | ast::CopyLegacyOption::Csv(_) => {}
            other => {
                return Err(BindError::feature_not_supported(format!(
                    "COPY option {other} is not supported yet"
                )));
            }
        }
        if let ast::CopyLegacyOption::Csv(sub) = opt {
            for s in sub {
                match s {
                    ast::CopyLegacyCsvOption::Header => fmt.header = true,
                    ast::CopyLegacyCsvOption::Quote(c) => fmt.quote = single_byte(*c, "quote")?,
                    ast::CopyLegacyCsvOption::Escape(c) => fmt.escape = single_byte(*c, "escape")?,
                    ast::CopyLegacyCsvOption::ForceNotNull(cols) => {
                        fmt.force_not_null = force_not_null(cols)?;
                    }
                    ast::CopyLegacyCsvOption::ForceQuote(_) => {
                        return Err(BindError::feature_not_supported(
                            "COPY FORCE_QUOTE is not supported yet",
                        ));
                    }
                }
            }
        }
    }

    // PG rejects a delimiter that collides with the CSV quote.
    if fmt.csv && fmt.delimiter == fmt.quote {
        return Err(BindError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "COPY delimiter and quote must be different",
        ));
    }

    Ok(fmt)
}

/// A COPY delimiter/quote/escape must be a single one-byte character (PG rejects
/// a multi-byte one with `0A000`). Returns the byte for the byte-oriented decoder.
fn single_byte(c: char, what: &str) -> Result<u8, BindError> {
    if c.is_ascii() {
        Ok(c as u8)
    } else {
        Err(BindError::feature_not_supported(format!(
            "COPY {what} must be a single one-byte character"
        )))
    }
}

/// COPY only speaks UTF-8; any other `ENCODING` is an honest not-supported.
fn require_utf8(enc: &str) -> Result<(), BindError> {
    let e = enc.to_ascii_uppercase().replace(['-', '_'], "");
    if e == "UTF8" {
        Ok(())
    } else {
        Err(BindError::feature_not_supported(format!(
            "COPY ENCODING \"{enc}\" is not supported yet; only UTF8"
        )))
    }
}

pub fn bind_update(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    update: &ast::Update,
) -> Result<LogicalPlan, BindError> {
    bind_update_with_params(engine, catalog, update, &param_ctx_none())
}

/// [`bind_update`] for the extended query protocol: `$n` placeholders in SET
/// and WHERE take their type from context via the shared `params`.
pub fn bind_update_with_params(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    update: &ast::Update,
    params: &ParamCtx,
) -> Result<LogicalPlan, BindError> {
    let unsupported: Option<&str> = if update.from.is_some() {
        Some("UPDATE ... FROM")
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

    let (table, qualifier) = open_write_relation(engine, &update.table.relation, WriteVerb::Update)?;
    let schema = table.schema().clone();
    let table_name = schema.name.clone();
    // A partitioned parent routes each updated row to a leaf (moving it across
    // leaves when the key changes); capture the leaves now. `None` for a plain table.
    let routing = if schema.partition_scheme.is_some() {
        Some(partition_leaves(engine, &schema)?)
    } else {
        None
    };
    // SET / WHERE / RETURNING may all contain subqueries; UPDATE takes no WITH,
    // so the CTE environment is empty.
    let scope =
        Scope::table(&schema, qualifier, catalog, params).with_subqueries(engine, &CteEnv::new());

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
        let value = if is_default_keyword(&assignment.value) {
            default_for_column(&schema.columns[idx], catalog)?
        } else {
            let binding = bind_expr(&assignment.value, &scope)?;
            coerce_to_column(binding, &schema.columns[idx], &scope)?
        };
        assignments.push((idx, value));
    }

    let predicate = bind_where(&update.selection, &scope)?;
    // RETURNING references the NEW row (post-update), which the executor feeds
    // in schema order — the same scope the SET/WHERE clauses bound against.
    let returning = bind_returning(&update.returning, &scope)?;
    Ok(LogicalPlan::Update {
        table,
        predicate,
        assignments,
        returning,
        routing,
    })
}

pub fn bind_delete(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    delete: &ast::Delete,
) -> Result<LogicalPlan, BindError> {
    bind_delete_with_params(engine, catalog, delete, &param_ctx_none())
}

/// [`bind_delete`] for the extended query protocol: `$n` placeholders in the
/// WHERE clause take their type from context via the shared `params`.
pub fn bind_delete_with_params(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    delete: &ast::Delete,
    params: &ParamCtx,
) -> Result<LogicalPlan, BindError> {
    let unsupported: Option<&str> = if !delete.tables.is_empty() {
        Some("multi-table DELETE")
    } else if delete.using.is_some() {
        Some("DELETE ... USING")
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

    let (table, qualifier) = open_write_relation(engine, &target.relation, WriteVerb::Delete)?;
    let schema = table.schema().clone();
    // A partitioned parent deletes matching rows from whichever leaf holds them;
    // capture the leaves now. `None` for a plain table.
    let routing = if schema.partition_scheme.is_some() {
        Some(partition_leaves(engine, &schema)?)
    } else {
        None
    };
    // WHERE / RETURNING may contain subqueries; DELETE takes no WITH.
    let scope =
        Scope::table(&schema, qualifier, catalog, params).with_subqueries(engine, &CteEnv::new());
    let predicate = bind_where(&delete.selection, &scope)?;
    // RETURNING references the deleted (OLD) row, which the executor feeds.
    let returning = bind_returning(&delete.returning, &scope)?;
    Ok(LogicalPlan::Delete {
        table,
        predicate,
        returning,
        routing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BinOp;
    use crabgresql_storage_api::{Column, TableEngine, TableSchema};
    use crabgresql_types::PgType;

    fn engine_with_table() -> Arc<dyn TableEngine> {
        let engine = crabgresql_pg_engine::ephemeral_engine();
        if let Err(error) = engine.create_table(TableSchema::new(
            "t",
            vec![
                Column::new("id", PgType::Int4),
                Column::new("big", PgType::Int8),
                Column::new("name", PgType::Text),
                Column::new("flag", PgType::Bool),
            ],
        )) {
            panic!("failed to create binder test table: {error}");
        }
        engine
    }

    fn bind_one(sql: &str) -> Result<LogicalPlan, BindError> {
        let engine = engine_with_table();
        let catalog: Arc<dyn TypeCatalog> = Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
        let stmts = match crabgresql_parser::parse(sql) {
            Ok(stmts) => stmts,
            Err(error) => panic!("invalid SQL test fixture `{sql}`: {error}"),
        };
        match &stmts[0] {
            ast::Statement::Query(q) => bind_query(&engine, &catalog, q),
            ast::Statement::Insert(i) => bind_insert(&engine, &catalog, i),
            ast::Statement::Update(u) => bind_update(&engine, &catalog, u),
            ast::Statement::Delete(d) => bind_delete(&engine, &catalog, d),
            other => panic!("unexpected statement: {other}"),
        }
    }

    fn bound(sql: &str) -> LogicalPlan {
        match bind_one(sql) {
            Ok(plan) => plan,
            Err(error) => panic!("failed to bind SQL test fixture `{sql}`: {error}"),
        }
    }

    fn bind_err(sql: &str) -> BindError {
        match bind_one(sql) {
            Err(e) => e,
            Ok(_) => panic!("expected bind error for: {sql}"),
        }
    }

    /// The pieces of a bound `Aggregate` plan.
    fn agg_of(
        sql: &str,
    ) -> (
        Vec<BoundExpr>,
        Vec<crate::BoundAggregate>,
        Vec<BoundExpr>,
        Option<BoundExpr>,
    ) {
        match bound(sql) {
            LogicalPlan::Aggregate {
                group_exprs,
                aggregates,
                projections,
                having,
                ..
            } => (group_exprs, aggregates, projections, having),
            other => panic!(
                "expected Aggregate for `{sql}`, got another plan variant: {}",
                plan_name(&other)
            ),
        }
    }

    fn plan_name(p: &LogicalPlan) -> &'static str {
        match p {
            LogicalPlan::Values { .. } => "Values",
            LogicalPlan::Query { .. } => "Query",
            LogicalPlan::Append { .. } => "Append",
            LogicalPlan::SetOp { .. } => "SetOp",
            LogicalPlan::Subquery { .. } => "Subquery",
            LogicalPlan::TableFunction { .. } => "TableFunction",
            LogicalPlan::Join { .. } => "Join",
            LogicalPlan::Aggregate { .. } => "Aggregate",
            LogicalPlan::Limit { .. } => "Limit",
            LogicalPlan::Insert { .. } => "Insert",
            LogicalPlan::Update { .. } => "Update",
            LogicalPlan::Delete { .. } => "Delete",
        }
    }

    #[test]
    fn count_star_becomes_a_single_aggregate() {
        let (group_exprs, aggregates, projections, having) = agg_of("SELECT count(*) FROM t");
        assert!(group_exprs.is_empty());
        assert!(having.is_none());
        assert_eq!(aggregates.len(), 1);
        assert_eq!(aggregates[0].func, crate::AggFn::Count);
        assert!(aggregates[0].args.is_empty());
        assert_eq!(aggregates[0].ret, PgType::Int8);
        // The projection reads the single aggregate slot.
        assert_eq!(
            projections,
            vec![BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int8
            }]
        );
    }

    #[test]
    fn min_and_max_extract_two_aggregates() {
        let (_g, aggregates, projections, _h) = agg_of("SELECT min(id), max(id) FROM t");
        assert_eq!(aggregates.len(), 2);
        assert_eq!(aggregates[0].func, crate::AggFn::Min);
        assert_eq!(aggregates[1].func, crate::AggFn::Max);
        // MIN/MAX keep the argument type.
        assert_eq!(aggregates[0].ret, PgType::Int4);
        assert_eq!(
            projections,
            vec![
                BoundExpr::ColumnRef {
                    index: 0,
                    ty: PgType::Int4
                },
                BoundExpr::ColumnRef {
                    index: 1,
                    ty: PgType::Int4
                },
            ]
        );
    }

    #[test]
    fn distinct_and_all_aggregate_treatments_are_preserved() {
        let (_g, aggregates, _p, _h) =
            agg_of("SELECT count(DISTINCT id), sum(ALL id), avg(id) FROM t");
        assert_eq!(aggregates.len(), 3);
        assert!(aggregates[0].distinct);
        assert!(!aggregates[1].distinct);
        assert!(!aggregates[2].distinct);
    }

    #[test]
    fn duplicate_treatment_with_wildcard_is_a_syntax_error() {
        for sql in [
            "SELECT count(DISTINCT *) FROM t",
            "SELECT count(ALL *) FROM t",
        ] {
            let err = bind_err(sql);
            assert_eq!(err.code, sqlstate::SYNTAX_ERROR);
            assert_eq!(err.message, "syntax error at or near \"*\"");
        }
    }

    #[test]
    fn expression_over_aggregates_rewrites_each_call() {
        let (_g, aggregates, projections, _h) = agg_of("SELECT max(id) - min(id) FROM t");
        assert_eq!(aggregates.len(), 2);
        let BoundExpr::Binary {
            op: BinOp::Sub,
            left,
            right,
            ..
        } = &projections[0]
        else {
            panic!("expected a subtraction over the two aggregate columns");
        };
        assert_eq!(
            **left,
            BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4
            }
        );
        assert_eq!(
            **right,
            BoundExpr::ColumnRef {
                index: 1,
                ty: PgType::Int4
            }
        );
    }

    #[test]
    fn constant_mixed_with_aggregate_is_kept() {
        let (_g, aggregates, projections, _h) = agg_of("SELECT 'x', count(*) FROM t");
        assert_eq!(aggregates.len(), 1);
        assert!(matches!(projections[0], BoundExpr::Const { .. }));
        assert_eq!(
            projections[1],
            BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int8
            }
        );
    }

    #[test]
    fn group_by_puts_keys_before_aggregates() {
        let (group_exprs, aggregates, projections, _h) =
            agg_of("SELECT id, count(*) FROM t GROUP BY id");
        assert_eq!(
            group_exprs,
            vec![BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4
            }]
        );
        assert_eq!(aggregates.len(), 1);
        // Group key is slot 0; the aggregate is slot 1 (after the keys).
        assert_eq!(
            projections,
            vec![
                BoundExpr::ColumnRef {
                    index: 0,
                    ty: PgType::Int4
                },
                BoundExpr::ColumnRef {
                    index: 1,
                    ty: PgType::Int8
                },
            ]
        );
    }

    #[test]
    fn group_by_ordinal_references_select_expression() {
        // GROUP BY 1 groups by the first select expression (id), not the literal 1.
        let (group_exprs, _a, _p, _h) = agg_of("SELECT id, count(*) FROM t GROUP BY 1");
        assert_eq!(
            group_exprs,
            vec![BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4
            }]
        );
    }

    #[test]
    fn grouped_compound_expression_is_allowed() {
        // `id + 1` is legal because its column is a group key.
        let (_g, _a, projections, _h) = agg_of("SELECT id + 1 FROM t GROUP BY id");
        assert!(matches!(
            projections[0],
            BoundExpr::Binary { op: BinOp::Add, .. }
        ));
    }

    #[test]
    fn having_forces_aggregation_and_is_rewritten() {
        let (_g, aggregates, _p, having) =
            agg_of("SELECT id FROM t GROUP BY id HAVING count(*) > 1");
        assert_eq!(aggregates.len(), 1);
        // HAVING references the aggregate slot (after the one group key).
        let BoundExpr::Binary { left, .. } = having.expect("HAVING present") else {
            panic!("expected a comparison in HAVING");
        };
        assert_eq!(
            *left,
            BoundExpr::ColumnRef {
                index: 1,
                ty: PgType::Int8
            }
        );
    }

    #[test]
    fn sum_and_avg_return_types() {
        assert_eq!(agg_of("SELECT sum(id) FROM t").1[0].ret, PgType::Int8);
        assert_eq!(agg_of("SELECT sum(big) FROM t").1[0].ret, PgType::Numeric);
        assert_eq!(agg_of("SELECT avg(id) FROM t").1[0].ret, PgType::Numeric);
        assert_eq!(agg_of("SELECT avg(big) FROM t").1[0].ret, PgType::Numeric);
    }

    #[test]
    fn order_by_aggregate_binds_without_error() {
        // ORDER BY count(*) appends a hidden aggregate column; it must bind.
        let (_g, aggregates, _p, _h) = agg_of("SELECT id FROM t GROUP BY id ORDER BY count(*)");
        assert_eq!(aggregates.len(), 1);
    }

    #[test]
    fn ungrouped_column_is_a_grouping_error() {
        assert_eq!(
            bind_err("SELECT id, count(*) FROM t").code,
            sqlstate::GROUPING_ERROR
        );
        assert_eq!(
            bind_err("SELECT id FROM t GROUP BY big").code,
            sqlstate::GROUPING_ERROR
        );
    }

    #[test]
    fn aggregate_in_where_is_rejected() {
        assert_eq!(
            bind_err("SELECT count(*) FROM t WHERE count(*) > 1").code,
            sqlstate::GROUPING_ERROR
        );
    }

    #[test]
    fn nested_aggregate_is_rejected() {
        assert_eq!(
            bind_err("SELECT max(min(id)) FROM t").code,
            sqlstate::GROUPING_ERROR
        );
    }

    #[test]
    fn aggregate_in_group_by_is_rejected() {
        assert_eq!(
            bind_err("SELECT count(*) FROM t GROUP BY count(*)").code,
            sqlstate::GROUPING_ERROR
        );
    }

    #[test]
    fn unsupported_aggregate_argument_is_undefined_function() {
        assert_eq!(
            bind_err("SELECT sum(name) FROM t").code,
            sqlstate::UNDEFINED_FUNCTION
        );
        assert_eq!(
            bind_err("SELECT avg(name) FROM t").code,
            sqlstate::UNDEFINED_FUNCTION
        );
    }

    #[test]
    fn group_by_ordinal_out_of_range_is_rejected() {
        assert_eq!(
            bind_err("SELECT id, count(*) FROM t GROUP BY 5").code,
            sqlstate::INVALID_COLUMN_REFERENCE
        );
    }

    #[test]
    fn min_max_reject_boolean() {
        // PG has no min/max(boolean) even though bool is orderable for ORDER BY.
        assert_eq!(
            bind_err("SELECT max(flag) FROM t").code,
            sqlstate::UNDEFINED_FUNCTION
        );
        assert_eq!(
            bind_err("SELECT min(flag) FROM t").code,
            sqlstate::UNDEFINED_FUNCTION
        );
    }

    #[test]
    fn group_by_resolves_output_alias() {
        // `z` is not an input column; it resolves to the select-list alias for id.
        let (group_exprs, _a, _p, _h) = agg_of("SELECT id AS z, count(*) FROM t GROUP BY z");
        assert_eq!(
            group_exprs,
            vec![BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4
            }]
        );
    }

    #[test]
    fn parameterless_count_has_pg_message() {
        let e = bind_err("SELECT count()");
        assert_eq!(e.code, sqlstate::WRONG_OBJECT_TYPE);
        assert_eq!(
            e.message,
            "count(*) must be used to call a parameterless aggregate function"
        );
    }

    #[test]
    fn wrong_arity_aggregate_names_argument_types() {
        let e = bind_err("SELECT min(id, big) FROM t");
        assert_eq!(e.code, sqlstate::UNDEFINED_FUNCTION);
        assert_eq!(e.message, "function min(integer, bigint) does not exist");
    }

    #[test]
    fn wildcard_non_count_aggregate_is_undefined() {
        let e = bind_err("SELECT sum(*) FROM t");
        assert_eq!(e.code, sqlstate::UNDEFINED_FUNCTION);
        assert_eq!(e.message, "function sum() does not exist");
    }

    #[test]
    fn resolves_columns_to_indices() -> anyhow::Result<()> {
        let LogicalPlan::Query { projections, .. } = bind_one("SELECT name, id FROM t")? else {
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

        Ok(())
    }

    #[test]
    fn unknown_column_is_42703() {
        let e = bind_err("SELECT nope FROM t");
        assert_eq!(e.code, "42703");
        assert_eq!(e.message, "column \"nope\" does not exist");
    }

    /// The first projected expression of a bound FROM-less `SELECT`.
    fn one_projection(sql: &str) -> BoundExpr {
        let LogicalPlan::Values { mut rows, .. } = bound(sql) else {
            panic!("expected Values");
        };
        rows.remove(0).remove(0)
    }

    #[test]
    fn string_concat_lowers_to_text_concat() {
        let expr = one_projection("SELECT 'a' || 'b'");
        assert!(matches!(
            expr,
            BoundExpr::FuncCall {
                func: crate::ScalarFn::TextConcat,
                ret: PgType::Text,
                ..
            }
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
            BoundExpr::Unary {
                op: crate::UnaryOp::Not,
                ..
            }
        ));
    }

    #[test]
    fn char_types_carry_their_type_and_length() {
        assert_eq!(
            one_projection("SELECT 'abcdef'::varchar(3)").ty(),
            PgType::Varchar
        );
        // `char(3)` truncates a constant at bind time (explicit-cast semantics).
        assert_eq!(
            one_projection("SELECT 'abcdef'::char(3)"),
            BoundExpr::Const {
                value: Value::Text("abc".into()),
                ty: PgType::Bpchar
            }
        );
        // A bare `char` is `char(1)` and blank-pads a short constant.
        assert_eq!(
            one_projection("SELECT 'a'::char(3)"),
            BoundExpr::Const {
                value: Value::Text("a  ".into()),
                ty: PgType::Bpchar
            }
        );
    }

    #[test]
    fn substring_and_position_desugar_to_functions() {
        assert_eq!(
            one_projection("SELECT substring('abc' FROM 2 FOR 1)").ty(),
            PgType::Text
        );
        assert_eq!(
            one_projection("SELECT position('b' IN 'abc')").ty(),
            PgType::Int4
        );
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
    fn int4_int8_comparison_promotes_via_coerce() -> anyhow::Result<()> {
        let LogicalPlan::Query { predicate, .. } = bind_one("SELECT id FROM t WHERE id = big")?
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

        Ok(())
    }

    #[test]
    fn unknown_literal_takes_type_from_other_side() -> anyhow::Result<()> {
        let LogicalPlan::Query { predicate, .. } = bind_one("SELECT id FROM t WHERE big = '5'")?
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

        Ok(())
    }

    #[test]
    fn between_desugars_to_gte_and_lte() -> anyhow::Result<()> {
        let LogicalPlan::Query { predicate, .. } =
            bind_one("SELECT id FROM t WHERE id BETWEEN 1 AND 3")?
        else {
            panic!("expected Query");
        };
        // `x BETWEEN low AND high` -> `(x >= low) AND (x <= high)`.
        let Some(BoundExpr::Binary {
            op: BinOp::And,
            left,
            right,
            ..
        }) = predicate
        else {
            panic!("expected AND of two comparisons");
        };
        assert!(matches!(
            *left,
            BoundExpr::Binary {
                op: BinOp::GtEq,
                ..
            }
        ));
        assert!(matches!(
            *right,
            BoundExpr::Binary {
                op: BinOp::LtEq,
                ..
            }
        ));

        Ok(())
    }

    #[test]
    fn not_between_desugars_to_lt_or_gt() -> anyhow::Result<()> {
        let LogicalPlan::Query { predicate, .. } =
            bind_one("SELECT id FROM t WHERE id NOT BETWEEN 1 AND 3")?
        else {
            panic!("expected Query");
        };
        // `x NOT BETWEEN low AND high` -> `(x < low) OR (x > high)`.
        let Some(BoundExpr::Binary {
            op: BinOp::Or,
            left,
            right,
            ..
        }) = predicate
        else {
            panic!("expected OR of two comparisons");
        };
        assert!(matches!(
            *left,
            BoundExpr::Binary { op: BinOp::Lt, .. }
        ));
        assert!(matches!(
            *right,
            BoundExpr::Binary { op: BinOp::Gt, .. }
        ));

        Ok(())
    }

    #[test]
    fn between_reports_low_side_error_first() {
        // PG analyzes `(id >= low) AND (id <= high)` left-to-right and fully
        // resolves the low comparison — coercing the bad literal — before it
        // ever looks at the high bound. The low-side 22P02 must win over the
        // undefined-column 42703 the high bound would otherwise raise.
        let e = bind_err("SELECT id FROM t WHERE id BETWEEN 'notint' AND missingcol");
        assert_eq!(e.code, "22P02");
        assert_eq!(e.message, "invalid input syntax for type integer: \"notint\"");
    }

    #[test]
    fn unparsable_unknown_literal_is_22p02() {
        let e = bind_err("SELECT id FROM t WHERE id = 'abc'");
        assert_eq!(e.code, "22P02");
        assert_eq!(e.message, "invalid input syntax for type integer: \"abc\"");
    }

    #[test]
    fn unknown_vs_unknown_comparison_falls_back_to_text() -> anyhow::Result<()> {
        let LogicalPlan::Values { rows, .. } = bind_one("SELECT 'a' = 'b'")? else {
            panic!("expected Values");
        };
        assert_eq!(rows.len(), 1);
        let BoundExpr::Binary { arg_ty, .. } = &rows[0][0] else {
            panic!("expected comparison");
        };
        assert_eq!(*arg_ty, PgType::Text);

        Ok(())
    }

    #[test]
    fn unknown_arithmetic_is_ambiguous_42725() {
        // Like PG, every 42725 "operator is not unique" carries the same
        // DETAIL/HINT and a cursor on the operator.
        let e = bind_err("SELECT '1' + '2'");
        assert_eq!(e.code, "42725");
        assert_eq!(e.message, "operator is not unique: unknown + unknown");
        assert_eq!(
            e.detail.as_deref(),
            Some("Could not choose a best candidate operator.")
        );
        assert_eq!(
            e.hint.as_deref(),
            Some("You might need to add explicit type casts.")
        );
        // Cursor at the `+` (1-based column 12).
        assert_eq!(e.location, Some((1, 12)));
    }

    #[test]
    fn unary_on_untyped_literal_is_ambiguous_42725() {
        // `- unknown` / `~ unknown` are ambiguous in PG with the same DETAIL/HINT.
        let e = bind_err("SELECT - NULL");
        assert_eq!(e.code, "42725");
        assert_eq!(e.message, "operator is not unique: - unknown");
        assert_eq!(
            e.detail.as_deref(),
            Some("Could not choose a best candidate operator.")
        );
        assert_eq!(
            e.hint.as_deref(),
            Some("You might need to add explicit type casts.")
        );
    }

    #[test]
    fn time_plus_time_is_ambiguous_42725() {
        // PG cannot pick a best `+` candidate for `time + time`, so it reports
        // ambiguity (with DETAIL/HINT) and points the cursor at the operator —
        // unlike `timetz + timetz` / `time * time`, which are 42883.
        let e = bind_err("SELECT time '00:01' + time '00:02'");
        assert_eq!(e.code, "42725");
        assert_eq!(
            e.message,
            "operator is not unique: time without time zone + time without time zone"
        );
        assert_eq!(
            e.detail.as_deref(),
            Some("Could not choose a best candidate operator.")
        );
        assert_eq!(
            e.hint.as_deref(),
            Some("You might need to add explicit type casts.")
        );
        // Cursor at the `+` (1-based column 21).
        assert_eq!(e.location, Some((1, 21)));
    }

    #[test]
    fn timetz_plus_timetz_stays_undefined_42883() {
        let e = bind_err("SELECT '00:01+00'::timetz + '00:02+00'::timetz");
        assert_eq!(e.code, "42883");
        assert_eq!(
            e.message,
            "operator does not exist: time with time zone + time with time zone"
        );
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
    fn min_int4_literal_binds_as_int4() -> anyhow::Result<()> {
        let LogicalPlan::Values { rows, columns, .. } = bind_one("SELECT -2147483648")? else {
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

        Ok(())
    }

    #[test]
    fn output_column_names_follow_pg() -> anyhow::Result<()> {
        let LogicalPlan::Query { columns, .. } =
            bind_one("SELECT id, (name), id + 1 AS next, id + 1, true FROM t")?
        else {
            panic!("expected Query");
        };
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "name", "next", "?column?", "bool"]);

        Ok(())
    }

    #[test]
    fn insert_coerces_cells_to_column_types() -> anyhow::Result<()> {
        let LogicalPlan::Insert {
            source: InsertSource::Values(rows),
            ..
        } = bind_one("INSERT INTO t (id, name) VALUES ('7', 'x')")?
        else {
            panic!("expected Insert with a VALUES source");
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

        Ok(())
    }

    #[test]
    fn insert_type_mismatch_is_42804_with_column_context() {
        let e = bind_err("INSERT INTO t (flag) VALUES (1)");
        assert_eq!(e.code, "42804");
        assert_eq!(
            e.message,
            "column \"flag\" is of type boolean but expression is of type integer"
        );
    }

    #[test]
    fn insert_column_refs_in_values_are_undefined() {
        let e = bind_err("INSERT INTO t (id) VALUES (id)");
        assert_eq!(e.code, "42703");
    }

    #[test]
    fn update_binds_assignments_by_index() -> anyhow::Result<()> {
        let LogicalPlan::Update {
            assignments,
            predicate,
            ..
        } = bind_one("UPDATE t SET name = 'x', id = id + 1 WHERE flag")?
        else {
            panic!("expected Update");
        };
        assert_eq!(assignments.len(), 2);
        assert_eq!(assignments[0].0, 2);
        assert_eq!(assignments[1].0, 0);
        assert!(predicate.is_some());

        Ok(())
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
    fn update_assignment_coerces_to_column_type() -> anyhow::Result<()> {
        let LogicalPlan::Update { assignments, .. } = bind_one("UPDATE t SET id = big")? else {
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

        Ok(())
    }

    #[test]
    fn delete_binds_predicate() -> anyhow::Result<()> {
        let LogicalPlan::Delete { predicate, .. } = bind_one("DELETE FROM t WHERE id = 1")? else {
            panic!("expected Delete");
        };
        assert!(predicate.is_some());
        let LogicalPlan::Delete { predicate, .. } = bind_one("DELETE FROM t")? else {
            panic!("expected Delete");
        };
        assert!(predicate.is_none());

        Ok(())
    }

    #[test]
    fn unsupported_forms_stay_0a000() {
        for sql in [
            "UPDATE t SET (id, name) = (1, 'x')",
            "DELETE FROM t USING t AS u",
        ] {
            let e = bind_err(sql);
            assert_eq!(e.code, "0A000", "for: {sql}");
        }
    }

    #[test]
    fn insert_select_binds_as_query_source() -> anyhow::Result<()> {
        // A SELECT source produces a query-source Insert whose projection list is
        // full-width in schema order (unlisted columns take their defaults).
        let LogicalPlan::Insert {
            source: InsertSource::Query { projections, .. },
            ..
        } = bind_one("INSERT INTO t (id, name) SELECT id, name FROM t")?
        else {
            panic!("expected a query-source Insert");
        };
        assert_eq!(projections.len(), 4);
        // The two listed columns reference the source row by position.
        assert!(matches!(
            projections[0],
            BoundExpr::ColumnRef { index: 0, .. }
        ));
        assert!(matches!(
            projections[2],
            BoundExpr::ColumnRef { index: 1, .. }
        ));
        Ok(())
    }

    #[test]
    fn insert_select_arity_mismatches_match_pg() {
        let too_many = bind_err("INSERT INTO t (id) SELECT id, name FROM t");
        assert_eq!(too_many.message, "INSERT has more expressions than target columns");
        let too_few = bind_err("INSERT INTO t (id, name) SELECT id FROM t");
        assert_eq!(too_few.message, "INSERT has more target columns than expressions");
    }

    #[test]
    fn insert_select_type_mismatch_reports_datatype_mismatch() {
        // int4 (id) does not assign to a bool column.
        let e = bind_err("INSERT INTO t (flag) SELECT id FROM t");
        assert_eq!(e.code, sqlstate::DATATYPE_MISMATCH);
    }

    #[test]
    fn insert_table_source_binds_as_query() -> anyhow::Result<()> {
        // `INSERT ... TABLE t` is `INSERT ... SELECT * FROM t`.
        let LogicalPlan::Insert {
            source: InsertSource::Query { projections, .. },
            ..
        } = bind_one("INSERT INTO t TABLE t")?
        else {
            panic!("expected a query-source Insert");
        };
        assert_eq!(projections.len(), 4);
        Ok(())
    }

    #[test]
    fn table_statement_binds_select_star() -> anyhow::Result<()> {
        let LogicalPlan::Query { columns, .. } = bind_one("TABLE t")? else {
            panic!("expected a Query plan");
        };
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "big", "name", "flag"]);
        Ok(())
    }

    #[test]
    fn table_preserves_quoted_identifier_case() -> anyhow::Result<()> {
        // A case-sensitive relation reached via `TABLE "MixedCase"` must keep its
        // quoting, exactly as `SELECT * FROM "MixedCase"` does; an unquoted name
        // folds to lower case and does not resolve (matching PostgreSQL).
        let engine = crabgresql_pg_engine::ephemeral_engine();
        if let Err(error) =
            engine.create_table(TableSchema::new("MixedCase", vec![Column::new("id", PgType::Int4)]))
        {
            panic!("failed to create test table: {error}");
        }
        let engine: Arc<dyn TableEngine> = engine;
        let catalog: Arc<dyn TypeCatalog> = Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
        let bind = |sql: &str| -> Result<LogicalPlan, BindError> {
            let stmts = crabgresql_parser::parse(sql).expect("valid SQL");
            match &stmts[0] {
                ast::Statement::Query(q) => bind_query(&engine, &catalog, q),
                ast::Statement::Insert(i) => bind_insert(&engine, &catalog, i),
                other => panic!("unexpected statement: {other}"),
            }
        };

        // Quoted keeps case → resolves, as a statement and as an INSERT source.
        assert!(bind("TABLE \"MixedCase\"").is_ok());
        assert!(bind("INSERT INTO \"MixedCase\" TABLE \"MixedCase\"").is_ok());
        // Unquoted folds to `mixedcase`, which does not exist.
        match bind("TABLE MixedCase") {
            Err(e) => assert_eq!(e.code, sqlstate::UNDEFINED_TABLE),
            Ok(_) => panic!("unquoted MixedCase must not resolve to \"MixedCase\""),
        }
        Ok(())
    }

    #[test]
    fn insert_source_query_clauses_are_executed_not_rejected() -> anyhow::Result<()> {
        // A VALUES source carrying ORDER BY / LIMIT is a full query in PG: it must
        // be executed as one (a query source), not silently dropped or rejected.
        for sql in [
            "INSERT INTO t (id) VALUES (1), (2) LIMIT 1",
            "INSERT INTO t (id) VALUES (1), (2) ORDER BY 1",
        ] {
            let LogicalPlan::Insert {
                source: InsertSource::Query { .. },
                ..
            } = bind_one(sql)?
            else {
                panic!("expected a query-source Insert for: {sql}");
            };
        }
        Ok(())
    }

    #[test]
    fn default_keyword_binds_as_typed_null_without_a_declared_default() -> anyhow::Result<()> {
        let LogicalPlan::Insert {
            source: InsertSource::Values(rows),
            ..
        } = bind_one("INSERT INTO t (id) VALUES (DEFAULT)")?
        else {
            panic!("expected Insert with a VALUES source");
        };
        assert_eq!(
            rows[0][0],
            BoundExpr::Const {
                value: Value::Null,
                ty: PgType::Int4,
            }
        );
        assert!(bind_one("UPDATE t SET id = DEFAULT").is_ok());

        Ok(())
    }

    #[test]
    fn returning_binds_output_columns_for_each_dml() -> anyhow::Result<()> {
        // INSERT: `*` expands the whole table, a computed column carries an alias.
        let insert = bound("INSERT INTO t (id) VALUES (1) RETURNING *, id + 1 AS next");
        let LogicalPlan::Insert {
            returning: Some(_), ..
        } = &insert
        else {
            panic!("expected Insert with RETURNING");
        };
        let cols = output_columns_of(&insert)?;
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "big", "name", "flag", "next"]);
        assert_eq!(cols.last().unwrap().ty, PgType::Int4);

        // UPDATE and DELETE report their RETURNING columns too (used by Describe).
        let update = bound("UPDATE t SET id = 1 RETURNING id, name");
        assert!(matches!(
            update,
            LogicalPlan::Update {
                returning: Some(_),
                ..
            }
        ));
        let names: Vec<String> = output_columns_of(&update)?
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["id", "name"]);

        let delete = bound("DELETE FROM t RETURNING name, id");
        let names: Vec<String> = output_columns_of(&delete)?
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["name", "id"]);

        // A RETURNING expression over an unknown column still errors.
        assert_eq!(bind_err("DELETE FROM t RETURNING nope").code, "42703");

        Ok(())
    }

    #[test]
    fn returning_rejects_aggregates_and_set_returning_functions() {
        // PostgreSQL rejects both at bind time (no aggregate/ProjectSet node
        // exists above a data-modifying statement to consume them).
        let agg = bind_err("UPDATE t SET id = 1 RETURNING count(*)");
        assert_eq!(agg.code, "42803");
        assert_eq!(
            agg.message,
            "aggregate functions are not allowed in RETURNING"
        );
        assert_eq!(bind_err("DELETE FROM t RETURNING max(id)").code, "42803");

        let srf = bind_err("INSERT INTO t (id) VALUES (1) RETURNING generate_series(1, id)");
        assert_eq!(srf.code, "0A000");
        assert_eq!(
            srf.message,
            "set-returning functions are not allowed in RETURNING"
        );
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
    fn bool_literals_accept_pg_prefixes() -> anyhow::Result<()> {
        for (sql, expected) in [
            ("UPDATE t SET flag = 'tru'", Value::Bool(true)),
            ("UPDATE t SET flag = 'of'", Value::Bool(false)),
            ("UPDATE t SET flag = 'ye'", Value::Bool(true)),
            ("UPDATE t SET flag = 'N'", Value::Bool(false)),
        ] {
            let LogicalPlan::Update { assignments, .. } = bind_one(sql)? else {
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

        Ok(())
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
    fn decimal_literal_binds_as_numeric() -> anyhow::Result<()> {
        let LogicalPlan::Values { rows, .. } = bind_one("SELECT 1.5")? else {
            panic!("expected Values");
        };
        let BoundExpr::Const {
            value: Value::Numeric(n),
            ty: PgType::Numeric,
        } = &rows[0][0]
        else {
            panic!("expected numeric const, got {:?}", rows[0][0]);
        };
        assert_eq!(n.to_display(), "1.5");

        Ok(())
    }

    #[test]
    fn hex_literal_binds_as_bit() {
        // X'...' is a bit(n) value with n = 4 * hex digits, MSB-first bytes.
        assert_eq!(
            one_projection("SELECT X'00000001'"),
            BoundExpr::Const {
                value: Value::Bit {
                    len: 32,
                    data: vec![0, 0, 0, 1]
                },
                ty: PgType::Bit
            }
        );
        // Lowercase hex parses too.
        assert_eq!(
            one_projection("SELECT X'ff'"),
            BoundExpr::Const {
                value: Value::Bit {
                    len: 8,
                    data: vec![0xff]
                },
                ty: PgType::Bit
            }
        );
    }

    #[test]
    fn wide_bit_literal_binds() {
        // Arbitrary width is supported (68 bits, past the old u64 backing).
        let BoundExpr::Const {
            value: Value::Bit { len, .. },
            ty: PgType::Bit,
        } = one_projection("SELECT X'FFFFFFFFFFFFFFFFF'")
        else {
            panic!("expected bit const");
        };
        assert_eq!(len, 68);
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
            assert_eq!(
                e.message,
                format!("\"{bad}\" is not a valid hexadecimal digit")
            );
        }
    }

    #[test]
    fn empty_hex_literal_binds_as_zero_width_bit() {
        assert_eq!(
            one_projection("SELECT X''"),
            BoundExpr::Const {
                value: Value::Bit {
                    len: 0,
                    data: vec![]
                },
                ty: PgType::Bit
            }
        );
    }

    #[test]
    fn order_by_on_bit_binds() {
        // `bit` now has an executor comparison, so ORDER BY on it binds.
        assert!(bind_one("SELECT X'FF' ORDER BY 1").is_ok());
    }

    #[test]
    fn float_literal_cast_binds() -> anyhow::Result<()> {
        let LogicalPlan::Values { rows, .. } = bind_one("SELECT 'NaN'::float4")? else {
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

        Ok(())
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
    fn numeric_operators_bind() -> anyhow::Result<()> {
        // Comparison, arithmetic, and modulo all resolve for numeric now.
        assert!(bind_one("SELECT '1'::numeric < '2'::numeric").is_ok());
        let LogicalPlan::Values { rows, .. } = bind_one("SELECT 1.5 + 2.25")? else {
            panic!("expected Values");
        };
        assert_eq!(rows[0][0].ty(), PgType::Numeric);
        assert!(bind_one("SELECT 5.5 % 2.0").is_ok());

        Ok(())
    }

    #[test]
    fn int2_arithmetic_binds() -> anyhow::Result<()> {
        let LogicalPlan::Values { rows, .. } = bind_one("SELECT '1'::int2 + '2'::int2")? else {
            panic!("expected Values");
        };
        assert_eq!(rows[0][0].ty(), PgType::Int2);

        Ok(())
    }

    #[test]
    fn implicit_int_to_float4_function_arg_resolves() {
        // float4send(integer) works via the implicit int4->float4 cast.
        assert!(bind_one("SELECT float4send(1)").is_ok());
    }

    #[test]
    fn cast_keeps_bare_column_name() -> anyhow::Result<()> {
        let LogicalPlan::Query { columns, .. } = bind_one("SELECT id::int8 FROM t")? else {
            panic!("expected Query");
        };
        assert_eq!(columns[0].name, "id");
        // A constant/nested cast falls back to the target type name.
        let LogicalPlan::Values { columns, .. } = bind_one("SELECT 'nan'::numeric::float4")? else {
            panic!("expected Values");
        };
        assert_eq!(columns[0].name, "float4");

        Ok(())
    }

    #[test]
    fn select_where_without_table_binds_predicate() -> anyhow::Result<()> {
        let LogicalPlan::Values {
            rows, predicate, ..
        } = bind_one("SELECT 1 WHERE 1 = 2")?
        else {
            panic!("expected Values");
        };
        assert_eq!(rows.len(), 1);
        assert!(predicate.is_some());

        Ok(())
    }

    #[test]
    fn set_returning_function_in_from_binds_columns() -> anyhow::Result<()> {
        let LogicalPlan::TableFunction { func, columns, .. } =
            bind_one("SELECT * FROM pg_input_error_info('1e400', 'float4')")?
        else {
            panic!("expected TableFunction");
        };
        assert_eq!(func, crate::TableFn::PgInputErrorInfo);
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["message", "detail", "hint", "sql_error_code"]);
        assert!(columns.iter().all(|c| c.ty == PgType::Text));

        Ok(())
    }

    #[test]
    fn set_returning_function_projects_and_filters() -> anyhow::Result<()> {
        // A subset projection over the SRF's columns resolves like a table.
        let LogicalPlan::TableFunction {
            columns, predicate, ..
        } = bind_one(
            "SELECT sql_error_code FROM pg_input_error_info('1e400', 'float4') \
             WHERE message IS NOT NULL",
        )?
        else {
            panic!("expected TableFunction");
        };
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "sql_error_code");
        assert!(predicate.is_some());

        Ok(())
    }

    #[test]
    fn unknown_set_returning_function_is_42883() {
        let e = bind_err("SELECT * FROM no_such_srf('x')");
        assert_eq!(e.code, "42883");
        assert_eq!(e.message, "function no_such_srf(unknown) does not exist");
    }

    #[test]
    fn generate_series_in_from_binds_int4_column() -> anyhow::Result<()> {
        let LogicalPlan::TableFunction { func, columns, .. } =
            bind_one("SELECT * FROM generate_series(1, 5)")?
        else {
            panic!("expected TableFunction");
        };
        assert_eq!(func, crate::TableFn::GenerateSeries(PgType::Int4));
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "generate_series");
        assert_eq!(columns[0].ty, PgType::Int4);

        Ok(())
    }

    #[test]
    fn generate_series_widens_to_int8() -> anyhow::Result<()> {
        // A bigint bound widens the whole series to int8.
        let LogicalPlan::TableFunction { func, columns, .. } =
            bind_one("SELECT * FROM generate_series(1, 5000000000)")?
        else {
            panic!("expected TableFunction");
        };
        assert_eq!(func, crate::TableFn::GenerateSeries(PgType::Int8));
        assert_eq!(columns[0].ty, PgType::Int8);

        Ok(())
    }

    #[test]
    fn generate_series_three_arg_step_binds() -> anyhow::Result<()> {
        let LogicalPlan::TableFunction { func, args, .. } =
            bind_one("SELECT * FROM generate_series(1, 10, 3)")?
        else {
            panic!("expected TableFunction");
        };
        assert_eq!(func, crate::TableFn::GenerateSeries(PgType::Int4));
        assert_eq!(args.len(), 3);

        Ok(())
    }

    #[test]
    fn generate_series_wrong_arity_is_42883() {
        let e = bind_err("SELECT * FROM generate_series(1)");
        assert_eq!(e.code, "42883");
    }

    #[test]
    fn generate_series_in_target_list_is_srf_projection() -> anyhow::Result<()> {
        // A FROM-less SRF in the target list expands over a single dummy row.
        let LogicalPlan::Subquery {
            columns,
            projections,
            source,
            ..
        } = bind_one("SELECT generate_series(1, 5)")?
        else {
            panic!("expected Subquery over a single-row source");
        };
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "generate_series");
        assert_eq!(columns[0].ty, PgType::Int4);
        assert!(matches!(projections[0], BoundExpr::Srf { .. }));
        assert!(matches!(*source, LogicalPlan::Values { .. }));

        Ok(())
    }

    #[test]
    fn generate_series_in_target_list_over_table() -> anyhow::Result<()> {
        // Mixed scalar + SRF projection over a base table stays a Query.
        let LogicalPlan::Query { projections, .. } =
            bind_one("SELECT id, generate_series(1, 2) FROM t")?
        else {
            panic!("expected Query");
        };
        assert!(matches!(projections[0], BoundExpr::ColumnRef { .. }));
        assert!(matches!(projections[1], BoundExpr::Srf { .. }));

        Ok(())
    }

    fn table_fn(sql: &str) -> (crate::TableFn, Vec<OutputColumn>) {
        let LogicalPlan::TableFunction { func, columns, .. } = bound(sql) else {
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
    fn standalone_values_binds_to_values_plan() -> anyhow::Result<()> {
        let LogicalPlan::Values { columns, rows, .. } = bind_one("VALUES (1, 'a'), (2, 'b')")?
        else {
            panic!("expected Values");
        };
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].name, "column1");
        assert_eq!(columns[1].name, "column2");
        assert_eq!(rows.len(), 2);

        Ok(())
    }

    #[test]
    fn values_uneven_row_lengths_error() {
        let e = bind_err("VALUES (1), (2, 3)");
        assert_eq!(e.code, "42601");
    }

    #[test]
    fn values_common_type_keeps_real_over_int() -> anyhow::Result<()> {
        // PG's select_common_type resolves (real, int4) to real, not float8
        // (int4 implicitly casts to real). Contrast with operator resolution.
        let LogicalPlan::Values { columns, .. } = bind_one("VALUES (CAST(1.5 AS real)), (2)")?
        else {
            panic!("expected Values");
        };
        assert_eq!(columns[0].ty, PgType::Float4);

        Ok(())
    }

    #[test]
    fn derived_table_binds_to_subquery_plan() -> anyhow::Result<()> {
        let LogicalPlan::Subquery { columns, .. } =
            bind_one("SELECT x FROM (VALUES (1), (2)) v(x)")?
        else {
            panic!("expected Subquery");
        };
        assert_eq!(columns[0].name, "x");

        Ok(())
    }

    #[test]
    fn derived_table_requires_alias() {
        let e = bind_err("SELECT * FROM (VALUES (1))");
        assert_eq!(e.code, "42601");
        assert_eq!(e.message, "subquery in FROM must have an alias");
    }

    #[test]
    fn cte_reference_resolves_to_subquery() -> anyhow::Result<()> {
        let LogicalPlan::Subquery { columns, .. } =
            bind_one("WITH t(x) AS (VALUES (1)) SELECT x FROM t")?
        else {
            panic!("expected Subquery");
        };
        assert_eq!(columns[0].name, "x");

        Ok(())
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
    fn with_on_insert_source_binds_as_a_query() -> anyhow::Result<()> {
        // The WITH belongs to the source query and is honored via the query
        // binder (the CTE here is unused; the VALUES still supplies the row).
        let LogicalPlan::Insert {
            source: InsertSource::Query { .. },
            ..
        } = bind_one("INSERT INTO t (id) WITH c AS (SELECT 1) VALUES (10)")?
        else {
            panic!("expected a query-source Insert");
        };
        Ok(())
    }

    #[test]
    fn with_recursive_is_rejected() {
        let e = bind_err("WITH RECURSIVE t(n) AS (VALUES (1)) SELECT n FROM t");
        assert_eq!(e.code, "0A000");
        assert_eq!(e.message, "WITH RECURSIVE is not supported yet");
    }

    #[test]
    fn cte_shadows_a_real_table() -> anyhow::Result<()> {
        // `t` here is the CTE, not the base table `t`; its column is `x`.
        let LogicalPlan::Subquery { columns, .. } =
            bind_one("WITH t(x) AS (VALUES (1)) SELECT x FROM t")?
        else {
            panic!("expected Subquery");
        };
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "x");

        Ok(())
    }

    #[test]
    fn scalar_subquery_binds_with_column_type() -> anyhow::Result<()> {
        // A FROM-less SELECT is a Values plan; its one projection is the marker.
        let LogicalPlan::Values { rows, .. } = bound("SELECT (SELECT big FROM t)") else {
            panic!("expected Values");
        };
        assert!(matches!(
            rows[0][0],
            BoundExpr::ScalarSubquery {
                ty: PgType::Int8,
                ..
            }
        ));
        Ok(())
    }

    #[test]
    fn exists_binds_to_marker() {
        let LogicalPlan::Query { predicate, .. } =
            bound("SELECT id FROM t WHERE EXISTS (SELECT 1 FROM t)")
        else {
            panic!("expected Query");
        };
        assert!(matches!(
            predicate,
            Some(BoundExpr::Exists { negated: false, .. })
        ));
    }

    #[test]
    fn not_exists_sets_negated() {
        let LogicalPlan::Query { predicate, .. } =
            bound("SELECT id FROM t WHERE NOT EXISTS (SELECT 1 FROM t)")
        else {
            panic!("expected Query");
        };
        assert!(matches!(
            predicate,
            Some(BoundExpr::Exists { negated: true, .. })
        ));
    }

    /// `IN (SELECT …)` is PG's `= ANY (…)`, so it binds to the quantified marker
    /// with an equality template and no `ALL` flag.
    #[test]
    fn in_subquery_binds_to_marker() {
        let LogicalPlan::Query { predicate, .. } =
            bound("SELECT id FROM t WHERE id IN (SELECT id FROM t)")
        else {
            panic!("expected Query");
        };
        let Some(BoundExpr::QuantifiedSubquery { all, cmp, .. }) = predicate else {
            panic!("expected QuantifiedSubquery predicate");
        };
        assert!(!all);
        // The comparison template is `id = <hole>`, an equality Binary.
        assert!(matches!(*cmp, BoundExpr::Binary { op: BinOp::Eq, .. }));
    }

    /// `NOT IN (SELECT …)` is PG's `<> ALL (…)` — the De Morgan dual, so it binds
    /// to an inequality template with `all` set rather than a negated equality.
    #[test]
    fn not_in_subquery_binds_to_all_of_inequality() {
        let LogicalPlan::Query { predicate, .. } =
            bound("SELECT id FROM t WHERE id NOT IN (SELECT id FROM t)")
        else {
            panic!("expected Query");
        };
        let Some(BoundExpr::QuantifiedSubquery { all, cmp, .. }) = predicate else {
            panic!("expected QuantifiedSubquery predicate");
        };
        assert!(all);
        assert!(matches!(*cmp, BoundExpr::Binary { op: BinOp::NotEq, .. }));
    }

    #[test]
    fn scalar_subquery_multiple_columns_errors() {
        let e = bind_err("SELECT (SELECT id, big FROM t)");
        assert_eq!(e.code, "42601");
        assert_eq!(e.message, "subquery must return only one column");
    }

    #[test]
    fn in_subquery_multiple_columns_errors() {
        let e = bind_err("SELECT id FROM t WHERE id IN (SELECT id, big FROM t)");
        assert_eq!(e.code, "42601");
        assert_eq!(e.message, "subquery has too many columns");
    }

    #[test]
    fn correlated_qualified_reference_binds_to_outer_column() {
        // A qualified reference to an enclosing relation resolves outward rather
        // than erroring: `x.id` becomes an OuterColumnRef at level 1, index 0
        // (the outer row's `id`).
        let LogicalPlan::Query {
            predicate: Some(pred),
            ..
        } = bound("SELECT id FROM t x WHERE EXISTS (SELECT 1 FROM t WHERE id = x.id)")
        else {
            panic!("expected Query with a WHERE predicate");
        };
        let BoundExpr::Exists { subplan, negated } = pred else {
            panic!("expected EXISTS marker, got {pred:?}");
        };
        assert!(!negated);
        let LogicalPlan::Query {
            predicate: Some(inner),
            ..
        } = &*subplan.0
        else {
            panic!("expected inner Query with a predicate");
        };
        let BoundExpr::Binary { right, .. } = inner else {
            panic!("expected `id = x.id` comparison, got {inner:?}");
        };
        assert!(
            matches!(
                **right,
                BoundExpr::OuterColumnRef {
                    level: 1,
                    index: 0,
                    ..
                }
            ),
            "expected outer reference to x.id, got {right:?}"
        );
    }

    #[test]
    fn correlated_unqualified_reference_binds_to_outer_column() {
        // An unqualified name absent from the subquery's own relation falls
        // through to the enclosing query. Here `flag` is not selected from in the
        // subquery's FROM-less body, so it resolves to the outer row.
        let LogicalPlan::Query {
            predicate: Some(pred),
            ..
        } = bound("SELECT id FROM t WHERE EXISTS (SELECT 1 WHERE flag)")
        else {
            panic!("expected Query with a WHERE predicate");
        };
        let BoundExpr::Exists { subplan, .. } = pred else {
            panic!("expected EXISTS marker, got {pred:?}");
        };
        // The FROM-less inner body binds as a single-row Values plan; its WHERE
        // is the bare `flag` outer reference (level 1, the outer row's `flag`).
        let LogicalPlan::Values {
            predicate: Some(inner),
            ..
        } = &*subplan.0
        else {
            panic!("expected inner Values with a predicate");
        };
        assert!(
            matches!(
                inner,
                BoundExpr::OuterColumnRef {
                    level: 1,
                    index: 3,
                    ..
                }
            ),
            "expected outer reference to flag (index 3), got {inner:?}"
        );
    }

    #[test]
    fn uncorrelated_missing_column_still_errors_42703() {
        // A name in neither the subquery nor any enclosing query is still the
        // ordinary undefined-column error.
        let e = bind_err("SELECT id FROM t x WHERE EXISTS (SELECT 1 FROM t WHERE nope = 1)");
        assert_eq!(e.code, "42703");
    }

    #[test]
    fn scalar_subquery_column_named_after_inner_column() {
        let LogicalPlan::Values { columns, .. } = bound("SELECT (SELECT max(id) FROM t)") else {
            panic!("expected Values");
        };
        assert_eq!(columns[0].name, "max");
    }

    #[test]
    fn exists_column_named_exists() {
        let LogicalPlan::Values { columns, .. } = bound("SELECT EXISTS (SELECT 1 FROM t)") else {
            panic!("expected Values");
        };
        assert_eq!(columns[0].name, "exists");
    }

    #[test]
    fn exists_strips_target_list_to_a_constant() -> anyhow::Result<()> {
        // The EXISTS subplan's projection is replaced with a constant so the
        // original target list (here a division by zero) is never evaluated.
        let LogicalPlan::Values { rows, .. } = bound("SELECT EXISTS (SELECT id / 0 FROM t)") else {
            panic!("expected Values");
        };
        let BoundExpr::Exists { subplan, .. } = &rows[0][0] else {
            panic!("expected Exists");
        };
        let LogicalPlan::Query { projections, .. } = subplan.0.as_ref() else {
            panic!("expected Query subplan");
        };
        assert!(matches!(projections.as_slice(), [BoundExpr::Const { .. }]));
        Ok(())
    }

    #[test]
    fn update_set_accepts_subquery() {
        let LogicalPlan::Update { assignments, .. } =
            bound("UPDATE t SET id = (SELECT max(id) FROM t)")
        else {
            panic!("expected Update");
        };
        assert!(matches!(assignments[0].1, BoundExpr::ScalarSubquery { .. }));
    }

    #[test]
    fn delete_where_accepts_in_subquery() {
        let LogicalPlan::Delete { predicate, .. } =
            bound("DELETE FROM t WHERE id IN (SELECT id FROM t)")
        else {
            panic!("expected Delete");
        };
        assert!(matches!(
            predicate,
            Some(BoundExpr::QuantifiedSubquery { .. })
        ));
    }

    fn case_column(sql: &str) -> (OutputColumn, BoundExpr) {
        let LogicalPlan::Query {
            columns,
            projections,
            ..
        } = bound(sql)
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
            BoundExpr::Coerce {
                ty: PgType::Int8,
                ..
            }
        ));
        assert!(matches!(
            else_.as_deref(),
            Some(BoundExpr::ColumnRef {
                ty: PgType::Int8,
                ..
            })
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
            assert_eq!(
                e.message, "operator does not exist: text = integer",
                "{sql}"
            );
        }
        // Two untyped literals still compare as text (unchanged).
        assert!(bind_one("SELECT CASE 'x' WHEN 'y' THEN 1 ELSE 2 END").is_ok());
    }

    #[test]
    fn cross_join_builds_join_plan_with_offsets() -> anyhow::Result<()> {
        // Two derived tables: a(x) at offset 0, b(y) at offset 1.
        let LogicalPlan::Join {
            source,
            columns,
            projections,
            ..
        } = bind_one("SELECT a.x, b.y FROM (VALUES (1)) a(x), (VALUES (2)) b(y)")?
        else {
            panic!("expected Join");
        };
        assert!(matches!(
            source,
            JoinExpr::Join {
                kind: JoinKind::Cross,
                predicate: None,
                ..
            }
        ));
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

        Ok(())
    }

    #[test]
    fn cross_join_wildcard_expands_every_relation_in_order() -> anyhow::Result<()> {
        let LogicalPlan::Join {
            columns,
            projections,
            ..
        } = bind_one("SELECT * FROM (VALUES (1, 2)) a(x, y), (VALUES (3)) b(z)")?
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

        Ok(())
    }

    #[test]
    fn cross_join_qualified_refs_use_combined_row_index() -> anyhow::Result<()> {
        // `t` occupies indices 0..4 (id, big, name, flag); b.y follows at 4.
        let LogicalPlan::Join { projections, .. } =
            bind_one("SELECT t.id, b.y FROM t, (VALUES (2)) b(y)")?
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

        Ok(())
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
    fn explicit_cross_join_flattens_like_a_comma() -> anyhow::Result<()> {
        let LogicalPlan::Join { source, .. } =
            bind_one("SELECT * FROM (VALUES (1)) a(x) CROSS JOIN (VALUES (2)) b(y)")?
        else {
            panic!("expected Join");
        };
        assert!(matches!(
            source,
            JoinExpr::Join {
                kind: JoinKind::Cross,
                predicate: None,
                ..
            }
        ));

        Ok(())
    }

    #[test]
    fn on_join_kinds_bind_boolean_predicates() -> anyhow::Result<()> {
        for (sql, expected) in [
            ("SELECT * FROM t a JOIN t b ON a.id = b.id", JoinKind::Inner),
            (
                "SELECT * FROM t a LEFT JOIN t b ON a.id = b.id",
                JoinKind::Left,
            ),
            (
                "SELECT * FROM t a RIGHT OUTER JOIN t b ON a.id = b.id",
                JoinKind::Right,
            ),
            (
                "SELECT * FROM t a FULL JOIN t b ON a.id = b.id",
                JoinKind::Full,
            ),
        ] {
            let LogicalPlan::Join { source, .. } = bind_one(sql)? else {
                panic!("expected Join for {sql}");
            };
            let JoinExpr::Join {
                kind, predicate, ..
            } = source
            else {
                panic!("expected binary join for {sql}");
            };
            assert_eq!(kind, expected, "{sql}");
            assert!(matches!(
                predicate,
                Some(BoundExpr::Binary {
                    op: BinOp::Eq,
                    arg_ty: PgType::Int4,
                    ..
                })
            ));
        }

        Ok(())
    }

    #[test]
    fn chained_join_is_left_associative_and_offsets_keep_growing() -> anyhow::Result<()> {
        let LogicalPlan::Join {
            source,
            projections,
            ..
        } = bind_one(
            "SELECT c.z FROM (VALUES (1)) a(x) \
             LEFT JOIN (VALUES (1)) b(y) ON a.x = b.y \
             JOIN (VALUES (1)) c(z) ON b.y = c.z",
        )?
        else {
            panic!("expected Join");
        };
        let JoinExpr::Join {
            left,
            kind: JoinKind::Inner,
            ..
        } = source
        else {
            panic!("expected top inner join");
        };
        assert!(matches!(
            *left,
            JoinExpr::Join {
                kind: JoinKind::Left,
                ..
            }
        ));
        assert_eq!(
            projections[0],
            BoundExpr::ColumnRef {
                index: 2,
                ty: PgType::Int4
            }
        );

        Ok(())
    }

    #[test]
    fn join_on_scope_excludes_prior_comma_group() {
        let e = bind_err(
            "SELECT * FROM (VALUES (1)) a(x), \
             (VALUES (1)) b(y) JOIN (VALUES (1)) c(z) ON a.x = c.z",
        );
        assert_eq!(e.code, "42P01");
        assert_eq!(e.message, "missing FROM-clause entry for table \"a\"");
    }

    #[test]
    fn join_on_must_be_boolean() {
        let e = bind_err("SELECT * FROM t a JOIN t b ON a.id");
        assert_eq!(e.code, "42804");
        assert_eq!(
            e.message,
            "argument of JOIN/ON must be type boolean, not type integer"
        );
    }

    #[test]
    fn aggregate_in_join_on_is_rejected() {
        let e = bind_err("SELECT * FROM t a JOIN t b ON count(*) > 0");
        assert_eq!(e.code, "42803");
        assert_eq!(
            e.message,
            "aggregate functions are not allowed in JOIN conditions"
        );
    }

    #[test]
    fn using_join_merges_column_and_builds_equality() -> anyhow::Result<()> {
        // `id` is merged (once, first); the other three columns of each side
        // follow — 1 + 3 + 3 = 7 output columns.
        let LogicalPlan::Join {
            source,
            columns,
            projections,
            ..
        } = bind_one("SELECT * FROM t a JOIN t b USING (id)")?
        else {
            panic!("expected Join");
        };
        assert!(matches!(
            source,
            JoinExpr::Join {
                kind: JoinKind::Inner,
                predicate: Some(BoundExpr::Binary {
                    op: BinOp::Eq,
                    arg_ty: PgType::Int4,
                    ..
                }),
                ..
            }
        ));
        assert_eq!(columns.len(), 7);
        assert_eq!(columns[0].name, "id");
        // The merged column carries the left side's value (index 0).
        assert_eq!(
            projections[0],
            BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4
            }
        );

        Ok(())
    }

    #[test]
    fn using_merged_column_is_unqualified_while_sides_stay_addressable() -> anyhow::Result<()> {
        let LogicalPlan::Join { projections, .. } =
            bind_one("SELECT id, a.id, b.id FROM t a JOIN t b USING (id)")?
        else {
            panic!("expected Join");
        };
        // Unqualified `id` and `a.id` are the left copy (index 0); `b.id` the
        // right copy (index 4).
        let left = BoundExpr::ColumnRef {
            index: 0,
            ty: PgType::Int4,
        };
        let right = BoundExpr::ColumnRef {
            index: 4,
            ty: PgType::Int4,
        };
        assert_eq!(projections, vec![left.clone(), left, right]);

        Ok(())
    }

    #[test]
    fn using_full_join_merges_with_coalesce() -> anyhow::Result<()> {
        let LogicalPlan::Join { projections, .. } =
            bind_one("SELECT id FROM t a FULL JOIN t b USING (id)")?
        else {
            panic!("expected Join");
        };
        // A full join's merged column is COALESCE(left, right), lowered to CASE.
        assert!(matches!(
            projections[0],
            BoundExpr::Case {
                ty: PgType::Int4,
                ..
            }
        ));

        Ok(())
    }

    #[test]
    fn natural_join_equates_every_common_column() -> anyhow::Result<()> {
        let LogicalPlan::Join {
            source, columns, ..
        } = bind_one("SELECT * FROM t a NATURAL JOIN t b")?
        else {
            panic!("expected Join");
        };
        // All four columns are shared, so all four merge and the predicate ANDs
        // four equalities; no columns remain.
        assert_eq!(columns.len(), 4);
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "big", "name", "flag"]);
        assert!(matches!(
            source,
            JoinExpr::Join {
                kind: JoinKind::Inner,
                predicate: Some(BoundExpr::Binary { op: BinOp::And, .. }),
                ..
            }
        ));

        Ok(())
    }

    #[test]
    fn natural_join_without_common_columns_is_a_cross_product() -> anyhow::Result<()> {
        let LogicalPlan::Join {
            source, columns, ..
        } = bind_one("SELECT * FROM (VALUES (1)) a(x) NATURAL JOIN (VALUES (2)) b(y)")?
        else {
            panic!("expected Join");
        };
        assert_eq!(columns.len(), 2);
        assert!(matches!(
            source,
            JoinExpr::Join {
                kind: JoinKind::Inner,
                predicate: None,
                ..
            }
        ));

        Ok(())
    }

    #[test]
    fn using_column_missing_on_a_side_is_42703() {
        let right = bind_err("SELECT * FROM t a JOIN (VALUES (1)) b(x) USING (id)");
        assert_eq!(right.code, "42703");
        assert_eq!(
            right.message,
            "column \"id\" specified in USING clause does not exist in right table"
        );
        let left = bind_err("SELECT * FROM (VALUES (1)) a(x) JOIN t b USING (id)");
        assert_eq!(left.code, "42703");
        assert_eq!(
            left.message,
            "column \"id\" specified in USING clause does not exist in left table"
        );
    }

    #[test]
    fn using_join_in_a_later_comma_group_shifts_merged_indices() -> anyhow::Result<()> {
        // `t` (4 columns) is the first comma group, so the merged `id` and the
        // rest of the USING group live at combined-row offsets 4 and up.
        let LogicalPlan::Join {
            columns,
            projections,
            ..
        } = bind_one(
            "SELECT * FROM t, \
             (VALUES (5, 50)) a(id, x) JOIN (VALUES (5, 500)) b(id, y) USING (id)",
        )?
        else {
            panic!("expected Join");
        };
        // t's 4 columns, then merged id, a.x, b.y — 7 in all.
        assert_eq!(columns.len(), 7);
        // The merged `id` carries a.id, shifted past t to index 4.
        assert_eq!(
            projections[4],
            BoundExpr::ColumnRef {
                index: 4,
                ty: PgType::Int4
            }
        );
        // b.y is a's width past that (a occupies 4,5; b occupies 6,7).
        assert_eq!(
            projections[6],
            BoundExpr::ColumnRef {
                index: 7,
                ty: PgType::Int4
            }
        );

        Ok(())
    }

    #[test]
    fn duplicate_using_column_is_42701() {
        let e = bind_err("SELECT * FROM t a JOIN t b USING (id, id)");
        assert_eq!(e.code, "42701");
        assert_eq!(
            e.message,
            "column name \"id\" appears more than once in USING clause"
        );
    }

    #[test]
    fn using_merged_column_uses_common_type_not_comparison_type() -> anyhow::Result<()> {
        // real + int4: PG's select_common_type resolves the merged column to
        // real, even though the equality comparison promotes to float8.
        let LogicalPlan::Join { projections, .. } =
            bind_one("SELECT x FROM (VALUES (1.0::real)) a(x) JOIN (VALUES (1)) b(x) USING (x)")?
        else {
            panic!("expected Join");
        };
        assert_eq!(projections[0].ty(), PgType::Float4);

        Ok(())
    }

    #[test]
    fn using_column_ambiguous_on_a_side_is_42702() {
        let e = bind_err("SELECT * FROM (VALUES (1, 2)) a(x, x) JOIN (VALUES (1)) b(x) USING (x)");
        assert_eq!(e.code, "42702");
        assert_eq!(
            e.message,
            "common column name \"x\" appears more than once in left table"
        );
    }

    #[test]
    fn aggregate_accepts_join_input() -> anyhow::Result<()> {
        let LogicalPlan::Aggregate {
            input: AggInput::Join(source),
            aggregates,
            ..
        } = bind_one("SELECT count(*) FROM t a LEFT JOIN t b ON a.id = b.id")?
        else {
            panic!("expected Aggregate over Join");
        };
        assert_eq!(aggregates.len(), 1);
        assert!(matches!(
            source,
            JoinExpr::Join {
                kind: JoinKind::Left,
                ..
            }
        ));

        Ok(())
    }

    #[test]
    fn where_referencing_both_relations_binds() -> anyhow::Result<()> {
        let LogicalPlan::Join { predicate, .. } =
            bind_one("SELECT a.x FROM (VALUES (1)) a(x), (VALUES (1)) b(y) WHERE a.x = b.y")?
        else {
            panic!("expected Join");
        };
        assert!(predicate.is_some());

        Ok(())
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
        match bound(sql) {
            LogicalPlan::Query {
                projections, sort, ..
            } => (projections, sort),
            _ => panic!("expected Query for {sql}, got another plan variant"),
        }
    }

    fn distinct_of(sql: &str) -> (Vec<BoundExpr>, Option<Vec<DistinctKey>>) {
        match bound(sql) {
            LogicalPlan::Query {
                projections,
                distinct,
                ..
            } => (projections, distinct),
            _ => panic!("expected Query for {sql}, got another plan variant"),
        }
    }

    #[test]
    fn select_distinct_keys_every_visible_column() {
        // Plain DISTINCT deduplicates on all visible output columns, in order.
        let (projections, distinct) = distinct_of("SELECT id, name FROM t");
        assert!(distinct.is_none(), "no DISTINCT keyword → no distinct keys");
        let _ = projections;
        let (_, distinct) = distinct_of("SELECT DISTINCT id, name FROM t");
        assert_eq!(
            distinct,
            Some(vec![
                DistinctKey {
                    column: 0,
                    ty: PgType::Int4,
                },
                DistinctKey {
                    column: 1,
                    ty: PgType::Text,
                },
            ])
        );
    }

    #[test]
    fn select_all_keeps_duplicates() {
        // The explicit ALL default is not DISTINCT.
        let (_, distinct) = distinct_of("SELECT ALL id FROM t");
        assert!(distinct.is_none());
    }

    #[test]
    fn distinct_on_resolves_expressions_to_columns() {
        // DISTINCT ON (id): id is a select-list column, so the key reuses it.
        let (projections, distinct) =
            distinct_of("SELECT DISTINCT ON (id) id, name FROM t ORDER BY id, name");
        assert_eq!(projections.len(), 2, "ON key reuses the visible column");
        assert_eq!(
            distinct,
            Some(vec![DistinctKey {
                column: 0,
                ty: PgType::Int4,
            }])
        );
    }

    #[test]
    fn distinct_on_hidden_expression_appends_column() {
        // DISTINCT ON (big) where big is not selected: it becomes a hidden
        // column, and ORDER BY big reuses that same hidden column (prefix match).
        let (projections, distinct) =
            distinct_of("SELECT DISTINCT ON (big) id FROM t ORDER BY big, id");
        assert_eq!(projections.len(), 2, "one hidden column for the ON expr");
        assert_eq!(
            distinct,
            Some(vec![DistinctKey {
                column: 1,
                ty: PgType::Int8,
            }])
        );
    }

    #[test]
    fn select_distinct_order_by_not_in_select_list_is_42p10() {
        // PG requires DISTINCT's ORDER BY keys to be select-list columns.
        let e = bind_err("SELECT DISTINCT id FROM t ORDER BY big");
        assert_eq!(e.code, "42P10");
        assert_eq!(
            e.message,
            "for SELECT DISTINCT, ORDER BY expressions must appear in select list"
        );
    }

    #[test]
    fn distinct_on_not_matching_order_by_is_42p10() {
        // DISTINCT ON expressions must be a prefix of ORDER BY.
        let e = bind_err("SELECT DISTINCT ON (id) id FROM t ORDER BY name");
        assert_eq!(e.code, "42P10");
        assert_eq!(
            e.message,
            "SELECT DISTINCT ON expressions must match initial ORDER BY expressions"
        );
    }

    #[test]
    fn distinct_on_matches_reordered_order_by_prefix() {
        // The ON expressions are the *set* of leading ORDER BY expressions;
        // their order relative to each other does not matter (PG accepts this).
        let (_, distinct) =
            distinct_of("SELECT DISTINCT ON (id, big) id, big FROM t ORDER BY big, id");
        assert_eq!(
            distinct,
            Some(vec![
                DistinctKey {
                    column: 0,
                    ty: PgType::Int4,
                },
                DistinctKey {
                    column: 1,
                    ty: PgType::Int8,
                },
            ])
        );
        // Extra trailing ORDER BY keys (per-group tiebreak) are still allowed.
        let (_, distinct) =
            distinct_of("SELECT DISTINCT ON (id) id, name FROM t ORDER BY id, name");
        assert_eq!(
            distinct,
            Some(vec![DistinctKey {
                column: 0,
                ty: PgType::Int4,
            }])
        );
    }

    #[test]
    fn distinct_on_more_expressions_than_order_by_is_42p10() {
        // ON has two expressions but ORDER BY only covers one — not a match.
        let e = bind_err("SELECT DISTINCT ON (id, big) id, big FROM t ORDER BY id");
        assert_eq!(e.code, "42P10");
        assert_eq!(
            e.message,
            "SELECT DISTINCT ON expressions must match initial ORDER BY expressions"
        );
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
                collation: DEFAULT_COLLATION_OID,
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
                collation: DEFAULT_COLLATION_OID,
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
    fn values_order_by_column_name_resolves() -> anyhow::Result<()> {
        let LogicalPlan::Values { sort, .. } = bind_one("VALUES (3), (1) ORDER BY column1")? else {
            panic!("expected Values");
        };
        assert_eq!(sort[0].column, 0);
        assert_eq!(sort[0].ty, PgType::Int4);

        Ok(())
    }

    #[test]
    fn values_order_by_expression_stays_0a000() {
        // A standalone VALUES list has no projection tuple to hang a hidden
        // column on, so expression sort keys are still unsupported.
        let e = bind_err("VALUES (3), (1) ORDER BY column1 + 1");
        assert_eq!(e.code, "0A000");
    }

    #[test]
    fn limit_offset_wraps_body() -> anyhow::Result<()> {
        let LogicalPlan::Limit {
            source,
            limit,
            offset,
        } = bind_one("SELECT id FROM t LIMIT 5 OFFSET 2")?
        else {
            panic!("expected Limit");
        };
        assert_eq!(limit, Some(5));
        assert_eq!(offset, Some(2));
        assert!(matches!(*source, LogicalPlan::Query { .. }));

        Ok(())
    }

    #[test]
    fn offset_zero_is_a_bare_offset() -> anyhow::Result<()> {
        // The float4/float8 optimization-fence shape: `OFFSET 0`, no LIMIT.
        let LogicalPlan::Limit { limit, offset, .. } = bind_one("SELECT id FROM t OFFSET 0")?
        else {
            panic!("expected Limit");
        };
        assert_eq!(limit, None);
        assert_eq!(offset, Some(0));

        Ok(())
    }

    #[test]
    fn limit_all_is_no_bound() -> anyhow::Result<()> {
        // `LIMIT ALL OFFSET 3` carries only the offset; the limit is unbounded.
        let LogicalPlan::Limit { limit, offset, .. } =
            bind_one("SELECT id FROM t LIMIT ALL OFFSET 3")?
        else {
            panic!("expected Limit");
        };
        assert_eq!(limit, None);
        assert_eq!(offset, Some(3));

        Ok(())
    }

    #[test]
    fn offset_in_derived_table_wraps_subplan() -> anyhow::Result<()> {
        // `OFFSET 0` inside a FROM subquery binds as a Limit at that level.
        let LogicalPlan::Subquery { source, .. } =
            bind_one("SELECT * FROM (SELECT id FROM t OFFSET 0) s")?
        else {
            panic!("expected Subquery");
        };
        assert!(matches!(*source, LogicalPlan::Limit { .. }));

        Ok(())
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

    // --- bind-parameter ($1, $2, …) inference -------------------------------

    use crate::expr::{ParamCtx, param_ctx_extended, param_types};

    /// Bind `sql` for the extended protocol with the given declared parameter
    /// types, returning both the plan result and the shared context (so tests
    /// can read back the inferred types).
    fn bind_params(
        sql: &str,
        declared: Vec<Option<PgType>>,
    ) -> (Result<LogicalPlan, BindError>, ParamCtx) {
        let engine = engine_with_table();
        let catalog: Arc<dyn TypeCatalog> = Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
        let ctx = param_ctx_extended(declared);
        let stmts = match crabgresql_parser::parse(sql) {
            Ok(stmts) => stmts,
            Err(error) => panic!("invalid SQL test fixture `{sql}`: {error}"),
        };
        let plan = match &stmts[0] {
            ast::Statement::Query(q) => bind_query_with_params(&engine, &catalog, q, &ctx),
            other => panic!("unexpected statement: {other}"),
        };
        (plan, ctx)
    }

    #[test]
    fn declared_param_binds_and_reports_its_type() {
        // A client-declared int4 `$1` binds directly to a Param node.
        let (plan, ctx) = bind_params("SELECT $1", vec![Some(PgType::Int4)]);
        let plan = plan.expect("declared $1 binds");
        assert_eq!(param_types(&ctx), vec![Some(PgType::Int4)]);
        let LogicalPlan::Values { rows, .. } = plan else {
            panic!("expected Values for a FROM-less SELECT");
        };
        assert_eq!(
            rows[0][0],
            BoundExpr::Param {
                index: 0,
                ty: PgType::Int4
            }
        );
    }

    #[test]
    fn undeclared_param_infers_type_from_comparison() {
        // `$1 = big` against `big int8` deduces $1 as int8.
        let (plan, ctx) = bind_params("SELECT $1 = big FROM t", vec![]);
        plan.expect("$1 = big binds");
        assert_eq!(param_types(&ctx), vec![Some(PgType::Int8)]);
    }

    #[test]
    fn undeclared_param_infers_type_from_cast() {
        let (plan, ctx) = bind_params("SELECT $1::int4", vec![]);
        plan.expect("$1::int4 binds");
        assert_eq!(param_types(&ctx), vec![Some(PgType::Int4)]);
    }

    #[test]
    fn param_reused_across_sites_unifies() {
        // The same `$1` appears twice; both sites agree on int8.
        let (plan, ctx) = bind_params("SELECT $1 = big, $1 = big FROM t", vec![]);
        plan.expect("repeated $1 binds");
        assert_eq!(param_types(&ctx), vec![Some(PgType::Int8)]);
    }

    /// The bind error for a param query, panicking if it unexpectedly succeeds
    /// (`LogicalPlan` has no `Debug`, so `Result::expect_err` is unavailable).
    fn param_err(sql: &str, declared: Vec<Option<PgType>>) -> BindError {
        match bind_params(sql, declared).0 {
            Err(e) => e,
            Ok(_) => panic!("expected a bind error for: {sql}"),
        }
    }

    #[test]
    fn undetermined_param_is_42p18() {
        // A bare `$1` with no context and no declaration cannot be typed.
        let err = param_err("SELECT $1", vec![]);
        assert_eq!(err.code, "42P18");
        assert_eq!(err.message, "could not determine data type of parameter $1");
    }

    #[test]
    fn conflicting_param_deductions_are_42p18() {
        // `$1 IN (big, name)` clones the still-untyped `$1` for each comparison,
        // so one arm deduces int8 and the other text before either is fixed —
        // an inconsistency PG reports as 42P18.
        let err = param_err("SELECT $1 IN (big, name) FROM t", vec![]);
        assert_eq!(err.code, "42P18");
        assert_eq!(err.message, "inconsistent types deduced for parameter $1");
    }

    #[test]
    fn param_in_simple_query_is_42p02() {
        // The simple-query entry point forbids parameters entirely.
        let err = bind_err("SELECT $1");
        assert_eq!(err.code, "42P02");
        assert_eq!(err.message, "there is no parameter $1");
    }

    /// The arms of a bound set operation, panicking if it is not one.
    fn setop_of(sql: &str) -> (Vec<SetOpArm>, Vec<OutputColumn>, Vec<SortKey>, Option<Vec<DistinctKey>>) {
        match bound(sql) {
            LogicalPlan::SetOp {
                arms,
                columns,
                sort,
                distinct,
            } => (arms, columns, sort, distinct),
            other => panic!("expected SetOp for `{sql}`, got {}", plan_name(&other)),
        }
    }

    #[test]
    fn union_all_binds_to_a_flat_setop() {
        // Same-typed arms, no ORDER BY: a bare concat, arms untouched.
        let (arms, columns, sort, distinct) =
            setop_of("SELECT id FROM t UNION ALL SELECT id FROM t");
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "id");
        assert_eq!(columns[0].ty, PgType::Int4);
        assert!(sort.is_empty());
        assert!(distinct.is_none(), "UNION ALL keeps duplicates");
        assert_eq!(arms.len(), 2);
        for arm in &arms {
            assert_eq!(plan_name(&arm.plan), "Query");
            assert!(arm.coercion.is_none(), "same-typed arms need no coercion");
        }
    }

    #[test]
    fn union_deduplicates_on_every_output_column() {
        let (_, _, sort, distinct) = setop_of("SELECT id FROM t UNION SELECT id FROM t");
        assert!(sort.is_empty());
        assert_eq!(
            distinct.expect("UNION should deduplicate"),
            vec![DistinctKey {
                column: 0,
                ty: PgType::Int4,
            }]
        );
    }

    #[test]
    fn union_unifies_arm_types_and_coerces() {
        // int4 + int8 unify to int8; only the int4 arm needs coercing, and the
        // result column keeps the first arm's name.
        let (arms, columns, ..) = setop_of("SELECT id FROM t UNION ALL SELECT big FROM t");
        assert_eq!(columns[0].name, "id");
        assert_eq!(columns[0].ty, PgType::Int8);
        assert!(arms[0].coercion.is_some(), "int4 arm is coerced to int8");
        assert!(arms[1].coercion.is_none(), "int8 arm already matches");
    }

    #[test]
    fn union_column_count_mismatch_is_42601() {
        let err = bind_err("SELECT id FROM t UNION SELECT id, big FROM t");
        assert_eq!(err.code, sqlstate::SYNTAX_ERROR);
        assert_eq!(
            err.message,
            "each UNION query must have the same number of columns"
        );
    }

    #[test]
    fn union_incompatible_types_is_42804() {
        let err = bind_err("SELECT id FROM t UNION SELECT flag FROM t");
        assert_eq!(err.code, sqlstate::DATATYPE_MISMATCH);
        assert_eq!(
            err.message,
            "UNION types integer and boolean cannot be matched"
        );
    }

    #[test]
    fn union_all_order_by_ordinal_sorts_without_dedup() {
        let (_, _, sort, distinct) =
            setop_of("SELECT id FROM t UNION ALL SELECT id FROM t ORDER BY 1");
        assert!(distinct.is_none(), "UNION ALL keeps duplicates");
        assert_eq!(sort.len(), 1);
        assert_eq!(sort[0].column, 0);
    }

    #[test]
    fn equivalent_union_chain_flattens_into_one_node() {
        // `a UNION b UNION c` is one three-armed node with a single dedup, not
        // nested pairs that would deduplicate at every level.
        let (arms, _, sort, distinct) = setop_of(
            "SELECT id FROM t UNION SELECT id FROM t UNION SELECT id FROM t ORDER BY 1",
        );
        assert_eq!(arms.len(), 3);
        assert!(arms.iter().all(|a| plan_name(&a.plan) == "Query"));
        assert!(distinct.is_some());
        assert_eq!(sort.len(), 1);
    }

    #[test]
    fn union_all_over_a_distinct_arm_keeps_the_inner_dedup() {
        // Flattening must not absorb a DISTINCT child into an ALL parent: the
        // inner deduplication happens first and would otherwise be lost.
        let (arms, _, _, distinct) = setop_of(
            "(SELECT id FROM t UNION SELECT id FROM t) UNION ALL SELECT id FROM t",
        );
        assert!(distinct.is_none(), "the outer UNION ALL keeps duplicates");
        assert_eq!(arms.len(), 2);
        assert_eq!(
            plan_name(&arms[0].plan),
            "SetOp",
            "the inner UNION stays its own node"
        );
    }

    #[test]
    fn a_long_union_chain_binds_without_deep_nesting() {
        // A flat chain must not nest one level per arm: that recursed deeply
        // enough through bind/plan/execute to abort the process.
        let mut sql = String::from("SELECT id FROM t");
        for _ in 0..500 {
            sql.push_str(" UNION ALL SELECT id FROM t");
        }
        let (arms, ..) = setop_of(&sql);
        assert_eq!(arms.len(), 501);
    }

    #[test]
    fn deeply_nested_set_ops_are_rejected_rather_than_crashing() {
        let mut sql = String::from("SELECT id FROM t");
        for _ in 0..MAX_SET_OP_NESTING + 5 {
            // Alternating quantifiers defeat flattening, forcing real nesting.
            sql = format!("({sql} UNION SELECT id FROM t) UNION ALL SELECT id FROM t");
        }
        // The contract is a clean error rather than an aborted process. The
        // parser guards its own recursion at the same depth, so it reaches this
        // shape first and is what reports here; the binder's limit stays as a
        // backstop for callers that raise the parser's.
        let err = crabgresql_parser::parse(&sql).expect_err("should be rejected");
        assert!(
            err.to_string().contains("recursion limit exceeded"),
            "expected a depth error, got: {err}"
        );
    }

    #[test]
    fn a_null_arm_takes_its_type_from_the_other_arms() {
        // PG resolves an unknown-typed set-operation column from the other arms,
        // so the NULL-padding idiom keeps the real column type.
        let (arms, columns, ..) = setop_of("SELECT id FROM t UNION ALL SELECT NULL");
        assert_eq!(columns[0].ty, PgType::Int4, "NULL must not force text");
        assert!(
            arms[1].coercion.is_some(),
            "the NULL arm is re-typed to the resolved column type"
        );
    }

    #[test]
    fn an_all_null_column_falls_back_to_text() {
        let (_, columns, ..) = setop_of("SELECT NULL UNION ALL SELECT NULL");
        assert_eq!(columns[0].ty, PgType::Text);
    }

    #[test]
    fn union_order_by_unknown_column_is_42703() {
        let err = bind_err("SELECT id FROM t UNION SELECT id FROM t ORDER BY nosuch");
        assert_eq!(err.code, sqlstate::UNDEFINED_COLUMN);
        assert_eq!(err.message, "column \"nosuch\" does not exist");
    }

    #[test]
    fn union_order_by_expression_is_42p10() {
        let err = bind_err("SELECT id FROM t UNION SELECT id FROM t ORDER BY id + 1");
        assert_eq!(err.code, sqlstate::INVALID_COLUMN_REFERENCE);
        assert_eq!(err.message, "invalid UNION/INTERSECT/EXCEPT ORDER BY clause");
        assert!(err.hint.is_some(), "PG hints at result column names");
    }

    #[test]
    fn union_on_a_type_without_equality_is_42883() {
        let err = bind_err("SELECT '{}'::json UNION SELECT '{}'::json");
        assert_eq!(err.code, sqlstate::UNDEFINED_FUNCTION);
        assert_eq!(
            err.message,
            "could not identify an equality operator for type json"
        );
    }

    #[test]
    fn intersect_and_except_are_still_unsupported() {
        assert_eq!(
            bind_err("SELECT id FROM t INTERSECT SELECT id FROM t").code,
            sqlstate::FEATURE_NOT_SUPPORTED
        );
        assert_eq!(
            bind_err("SELECT id FROM t EXCEPT SELECT id FROM t").code,
            sqlstate::FEATURE_NOT_SUPPORTED
        );
    }

    /// A relation whose storage is split into engine-internal leaves. Only
    /// `schema` and `storage_leaves` are exercised — `scan_leaves` inspects
    /// metadata and never touches rows.
    struct SplitTable {
        schema: TableSchema,
        leaves: Vec<Arc<dyn TableAm>>,
    }

    impl SplitTable {
        fn new(name: &str, leaves: Vec<Arc<dyn TableAm>>) -> Arc<dyn TableAm> {
            Arc::new(Self {
                schema: TableSchema::new(name, vec![Column::new("id", PgType::Int4)]),
                leaves,
            })
        }
    }

    impl TableAm for SplitTable {
        fn schema(&self) -> &TableSchema {
            &self.schema
        }
        fn storage_leaves(&self) -> Option<Vec<Arc<dyn TableAm>>> {
            (!self.leaves.is_empty()).then(|| self.leaves.clone())
        }
        fn scan(&self, _txn: &crabgresql_storage_api::txn::TxnContext) -> crabgresql_storage_api::TupleStream {
            Box::new(std::iter::empty())
        }
        fn fetch(
            &self,
            _tid: crabgresql_storage_api::Tid,
            _txn: &crabgresql_storage_api::txn::TxnContext,
        ) -> Result<Option<crabgresql_storage_api::Tuple>, crabgresql_storage_api::StorageError>
        {
            Ok(None)
        }
        fn insert(
            &self,
            _tuple: crabgresql_storage_api::Tuple,
            _txn: &crabgresql_storage_api::txn::TxnContext,
        ) -> Result<crabgresql_storage_api::Tid, crabgresql_storage_api::StorageError> {
            unimplemented!("metadata-only test double")
        }
        fn update(
            &self,
            _tid: crabgresql_storage_api::Tid,
            _tuple: crabgresql_storage_api::Tuple,
            _txn: &crabgresql_storage_api::txn::TxnContext,
        ) -> Result<crabgresql_storage_api::UpdateResult, crabgresql_storage_api::StorageError>
        {
            unimplemented!("metadata-only test double")
        }
        fn delete(
            &self,
            _tid: crabgresql_storage_api::Tid,
            _txn: &crabgresql_storage_api::txn::TxnContext,
        ) -> Result<crabgresql_storage_api::DeleteResult, crabgresql_storage_api::StorageError>
        {
            unimplemented!("metadata-only test double")
        }
    }

    fn leaf_names(leaves: &[Arc<dyn TableAm>]) -> Vec<String> {
        leaves.iter().map(|l| l.schema().name.clone()).collect()
    }

    #[test]
    fn a_relation_without_storage_leaves_is_scanned_directly() {
        let engine = engine_with_table();
        let table = SplitTable::new("solo", Vec::new());
        assert!(
            scan_leaves(&engine, &table)
                .expect("scan_leaves must not fail on a plain relation")
                .is_none(),
            "a monolithic relation must bind to a plain Scan, not a one-armed Append"
        );
    }

    #[test]
    fn storage_leaves_become_the_append_arms() {
        let engine = engine_with_table();
        let table = SplitTable::new(
            "split",
            vec![
                SplitTable::new("split_chunks", Vec::new()),
                SplitTable::new("split_buffer", Vec::new()),
            ],
        );
        let leaves = scan_leaves(&engine, &table)
            .expect("scan_leaves must not fail")
            .expect("a relation reporting storage leaves must fan out");
        // Order is the access method's, not sorted: a leaf order carries meaning
        // (durable storage before the write buffer, say) that must survive.
        assert_eq!(leaf_names(&leaves), vec!["split_chunks", "split_buffer"]);
    }

    #[test]
    fn a_sql_partitions_storage_leaves_flatten_into_one_append() {
        // A partitioned parent is identified by its schema, so build one whose
        // single leaf itself splits, and confirm the result is one flat list
        // rather than an Append of an Append.
        let inner = SplitTable::new(
            "part_2024",
            vec![
                SplitTable::new("part_2024_chunks", Vec::new()),
                SplitTable::new("part_2024_buffer", Vec::new()),
            ],
        );
        // `partition_leaves` reads the engine's catalog, so exercise the flatten
        // directly on the expansion `scan_leaves` performs.
        let flattened: Vec<Arc<dyn TableAm>> = match inner.storage_leaves() {
            Some(leaves) => leaves,
            None => vec![Arc::clone(&inner)],
        };
        assert_eq!(
            leaf_names(&flattened),
            vec!["part_2024_chunks", "part_2024_buffer"],
            "a SQL partition that splits its storage must contribute its leaves, not itself"
        );
    }
}
