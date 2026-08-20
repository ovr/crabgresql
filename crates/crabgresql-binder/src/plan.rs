//! Statement binding: AST statements → [`LogicalPlan`].
//!
//! Everything parsed but not executed must be rejected loudly (`0A000`):
//! silently dropping a clause would return wrong results instead of an honest
//! error.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crabgresql_parser::ast::Spanned;
use crabgresql_parser::{Span, ast};
use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::{
    Column, EnumInfo, StorageError, TableAccessMethod, TableAm, TableEngine, TableSchema,
    TypeCatalog, ViewDefinition,
};
use crabgresql_types::collation::DEFAULT_COLLATION_OID;
use crabgresql_types::{FmtCtx, PgType, Value};

use crate::copy_rows::RowBatch;
use crate::expr::{
    BinOp, Binding, BoundExpr, BoundWindowFunc, BoundWindowSpec, DeclinedSystem, ExprSortKey,
    NamedWindows, OuterLevel, ParamCtx, Scope, ScopeItem, ViewExpansion, VisibleColumn,
    VisibleLookup, WindowKind, apply_column_typmod, bind_binary_op, bind_column_default, bind_expr,
    bind_projection, bind_scalar, coerce_expr, coerce_to_column, enum_value, lookup_visible,
    merge_types, normalize_ident, output_name, param_ctx_none, param_ctx_view_body, parse_unknown,
    reject_agg_or_window, reject_window, text_column_value, to_bool_operand, unify_value_column,
    view_expansion,
};
use crate::functions::{ScalarFn, bind_table_fn_call, positional_arg_exprs};
use crate::logical_plan::{
    AggInput, AggregatePlan, AppendPlan, DeletePlan, DistinctKey, ExprVisitor, InsertPlan,
    InsertSource, JoinExpr, JoinInput, JoinKind, JoinPlan, LimitPlan, LogicalPlan, MappedRelation,
    QueryPlan, RelationIdent, Returning, SetOpArm, SetOpPlan, SortKey, SubqueryPlan, SysCol,
    SystemEmit, TableFunctionPlan, UpdatePlan, ValuesPlan, WindowPlan, walk_exprs_mut,
};
use crate::{BindError, BoundAggregate, OutputColumn, TableFn};

/// Split a relation name into an optional schema qualifier and the relation
/// name. `t` → `(None, "t")`; `pg_catalog.pg_type` → `(Some("pg_catalog"),
/// "pg_type")`.
///
/// TODO: accept a three-part `db.schema.rel` name whose first part names the
/// database the session is connected to — PG resolves that spelling and rejects
/// only a reference to a *different* database.
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
        // "relation does not exist".
        // TODO: auto-updatable views and INSTEAD OF triggers, which are what
        // would let a write to a view reach a base relation instead.
        if matches!(e, StorageError::TableNotFound(_))
            && engine
                .resolve_view(schema.as_deref(), &table_name)
                .is_some()
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
/// Three independent kinds of split compose here. A **SQL partitioned parent**
/// holds no rows itself and fans out to its catalog leaf partitions. An
/// **inheritance parent** holds rows *and* fans out, to itself followed by its
/// descendants — suppressed by `ONLY`. An access method with **engine-internal
/// storage leaves** fans out to its own physical sources
/// ([`TableAm::storage_leaves`]) — invisible to the catalog.
///
/// A relation reached by either of the first two may itself split the third way,
/// so every arm is expanded and the result flattened: `Append` stays one flat
/// list and the executor keeps one loop. Nothing produces that nesting today (a
/// partition and an inheritance child are both heap relations), but writing the
/// flatten costs three lines and removes the trap for whoever first makes a
/// columnar relation a partition or a child.
///
/// `ONLY` on a *partitioned* parent is a deliberate divergence: PostgreSQL scans
/// the parent's own (always empty) storage and returns nothing, whereas this
/// still expands to the leaves. A silently empty result is the worse surprise of
/// the two, and nothing in the corpus asks for it; revisit if `DETACH PARTITION`
/// ever lands, which is what makes PG's answer meaningful.
/// System columns — the ones the query asked for — are stamped on each arm as
/// the arm's *own* relation, which is what makes a partitioned or inherited read
/// report the child a row came from. An access method's storage leaves are the
/// exception handled by [`push_storage_leaves`]: they are pieces of one
/// relation, not relations, so they all carry the name of the relation that owns
/// them.
pub(crate) fn scan_arms(
    engine: &Arc<dyn TableEngine>,
    table: &Arc<dyn TableAm>,
    only: bool,
    system: &Arc<[SysCol]>,
) -> Result<Option<Vec<MappedRelation>>, BindError> {
    let schema = table.schema();
    let ident = |schema: &TableSchema| system_emit(system, schema);
    if schema.partition_scheme.is_some() {
        let mut arms = Vec::new();
        for leaf in partition_leaves(engine, &schema)? {
            // A leaf is a verbatim clone of the parent's layout, so identity.
            let leaf_ident = ident(&leaf.schema());
            push_storage_leaves(&mut arms, leaf, None, leaf_ident);
        }
        reject_declining_arms(&arms)?;
        return Ok(Some(arms));
    }
    if !only {
        let descendants = inheritance_descendants(engine, &schema)?;
        if !descendants.is_empty() {
            // The parent owns rows of its own and reads first, in its own layout.
            let mut arms = Vec::new();
            push_storage_leaves(&mut arms, table.clone(), None, ident(&schema));
            for child in descendants {
                let child_schema = child.schema();
                let map = inherit_map(&schema, &child_schema)?;
                let child_ident = ident(&child_schema);
                push_storage_leaves(&mut arms, child, map, child_ident);
            }
            reject_declining_arms(&arms)?;
            return Ok(Some(arms));
        }
    }
    let leaves = table.storage_leaves().map(|leaves| {
        leaves
            .into_iter()
            .map(|t| arm(t, None, ident(&schema)))
            .collect::<Vec<_>>()
    });
    match leaves {
        None => Ok(None),
        Some(arms) => {
            reject_declining_arms(&arms)?;
            Ok(Some(arms))
        }
    }
}

/// Refuse a fan-out whose *leaf* cannot produce the system columns the named
/// relation promised.
///
/// The capability was checked against the relation the statement named, but the
/// arms are its partition leaves, inheritance children and storage leaves — and
/// nothing makes a leaf's access method agree with its parent's. Today DDL
/// happens to forbid the mismatch, in a different crate and for unrelated
/// reasons; without this the executor's `scan_with_system` would meet a `None`
/// and panic the session rather than raise.
fn reject_declining_arms(arms: &[MappedRelation]) -> Result<(), BindError> {
    for arm in arms {
        let Some(emit) = &arm.system else { continue };
        if emit.cols.iter().all(|c| !c.needs_storage_support())
            || arm.table.supports_system_columns()
        {
            continue;
        }
        let schema = arm.table.schema();
        let col = emit
            .cols
            .iter()
            .find(|c| c.needs_storage_support())
            .expect("the all() above returned false, so one column needs support");
        return Err(BindError::feature_not_supported(format!(
            "access method \"{}\" does not support system column \"{}\" on relation \"{}\"",
            schema.access_method.as_str(),
            col.name(),
            schema.name,
        )));
    }
    Ok(())
}

fn arm(
    table: Arc<dyn TableAm>,
    map: Option<Arc<[usize]>>,
    system: Option<SystemEmit>,
) -> MappedRelation {
    MappedRelation { table, map, system }
}

/// The [`SystemEmit`] for an arm naming `schema`, or `None` when the statement
/// asked for no system column at all — which is nearly every statement.
fn system_emit(cols: &Arc<[SysCol]>, schema: &TableSchema) -> Option<SystemEmit> {
    (!cols.is_empty()).then(|| SystemEmit {
        cols: Arc::clone(cols),
        ident: RelationIdent::of(schema),
    })
}

/// The relations an `UPDATE`/`DELETE` naming `table` must touch, or empty when
/// it is just `table` itself.
///
/// Unlike [`scan_arms`] this returns the parent as the *first* entry when it
/// returns anything at all, because the executor's fan-out treats every entry
/// alike — there is no "and also the named table" step, since nothing is routed.
/// Storage leaves are not expanded: a write goes to the relation, and the access
/// method decides where inside itself it lands.
fn write_targets(
    engine: &Arc<dyn TableEngine>,
    table: &Arc<dyn TableAm>,
    only: bool,
    system: &Arc<[SysCol]>,
) -> Result<Vec<MappedRelation>, BindError> {
    if only {
        return Ok(Vec::new());
    }
    let schema = table.schema();
    let descendants = inheritance_descendants(engine, &schema)?;
    if descendants.is_empty() {
        return Ok(Vec::new());
    }
    let ident = |schema: &TableSchema| system_emit(system, schema);
    // Each target names itself, so a RETURNING or WHERE that reads `tableoid`
    // sees the child the row actually lives in — not the relation the statement
    // named.
    let mut targets = vec![arm(table.clone(), None, ident(&schema))];
    for child in descendants {
        let child_schema = child.schema();
        let map = inherit_map(&schema, &child_schema)?;
        let child_ident = ident(&child_schema);
        targets.push(arm(child, map, child_ident));
    }
    reject_declining_arms(&targets)?;
    Ok(targets)
}

/// Append `table` to `arms`, expanded into its engine-internal storage leaves if
/// it has any. Every leaf of one relation shares that relation's layout, so they
/// all carry the same `map`.
fn push_storage_leaves(
    arms: &mut Vec<MappedRelation>,
    table: Arc<dyn TableAm>,
    map: Option<Arc<[usize]>>,
    system: Option<SystemEmit>,
) {
    match table.storage_leaves() {
        // Every leaf carries the *owning* relation's name: an access method
        // that keeps its rows in several places (durable chunks plus a write
        // buffer) is still one relation, and `tableoid` must not report its
        // internals.
        Some(inner) => arms.extend(
            inner
                .into_iter()
                .map(|t| arm(t, map.clone(), system.clone())),
        ),
        None => arms.push(arm(table, map, system)),
    }
}

/// The permutation reading a `child` row as a `parent` row: for each of the
/// parent's columns, the ordinal of the child column of that name.
///
/// `None` means the identity *over the same width* — the child is a verbatim
/// clone of the parent (`CREATE TABLE c () INHERITS (p)`, and every leaf
/// partition), so the executor and the projection pass skip the remap entirely.
/// A child that only appends columns still gets a map, because narrowing its
/// wider tuple to the parent's width is the same operation as permuting it.
///
/// The lookup is by name, and is total: `merge_inherited_columns` gives a child a
/// column for every name each of its parents contributes, and this server has no
/// `ALTER TABLE` to rename or drop one afterwards. A miss is therefore a bug in
/// that invariant, not a user error — reported as such rather than panicking on
/// the index.
pub(crate) fn inherit_map(
    parent: &TableSchema,
    child: &TableSchema,
) -> Result<Option<Arc<[usize]>>, BindError> {
    let mut map = Vec::with_capacity(parent.columns.len());
    for col in &parent.columns {
        let Some(pos) = child.columns.iter().position(|c| c.name == col.name) else {
            return Err(BindError::new(
                sqlstate::INTERNAL_ERROR,
                format!(
                    "child \"{}\" of \"{}\" has no column \"{}\"",
                    child.name, parent.name, col.name
                ),
            ));
        };
        map.push(pos);
    }
    let identity =
        map.len() == child.columns.len() && map.iter().enumerate().all(|(i, &pos)| i == pos);
    Ok((!identity).then(|| map.into()))
}

/// Every table that inherits from `parent`, transitively, in breadth-first order
/// with each level sorted by name. The parent itself is **not** included: unlike
/// a partitioned parent it owns rows, so the caller puts it first and this
/// function answers only "what else".
///
/// Like [`partition_leaves`], the set is captured at bind time, so a child
/// created after a statement is planned is not observed until it is re-bound.
///
/// The `visited` set is what makes this terminate. A cycle cannot be built
/// through `CREATE TABLE` — a parent must already exist to be inherited from —
/// but the links come off a catalog file on disk, and `ALTER TABLE ... INHERIT`
/// would be able to close one. Without the set that is a hang, not a wrong
/// answer.
pub fn inheritance_descendants(
    engine: &Arc<dyn TableEngine>,
    parent: &TableSchema,
) -> Result<Vec<Arc<dyn TableAm>>, BindError> {
    // `inheritance_links` rather than `relation_metadata`: this runs for every
    // base relation of every statement, so it must not clone the catalog (let
    // alone stat every relation's files) to discover the usual answer, which is
    // that nothing inherits from anything.
    let links = engine.inheritance_links();
    if links.is_empty() {
        return Ok(Vec::new());
    }
    // Index child-by-parent once: the walk is O(V+E) rather than one sweep per
    // level.
    let mut children: HashMap<(&str, &str), Vec<(&str, &str)>> = HashMap::new();
    for (child, parent) in &links {
        children
            .entry((parent.0.as_str(), parent.1.as_str()))
            .or_default()
            .push((child.0.as_str(), child.1.as_str()));
    }
    let mut visited: HashSet<(&str, &str)> =
        HashSet::from([(parent.namespace.as_str(), parent.name.as_str())]);
    let mut frontier = vec![(parent.namespace.as_str(), parent.name.as_str())];
    let mut order: Vec<(String, String)> = Vec::new();
    while !frontier.is_empty() {
        let mut next: Vec<(&str, &str)> = Vec::new();
        for key in frontier {
            for child in children.get(&key).into_iter().flatten() {
                if visited.insert(*child) {
                    next.push(*child);
                }
            }
        }
        next.sort_unstable();
        order.extend(
            next.iter()
                .map(|(ns, name)| (ns.to_string(), name.to_string())),
        );
        frontier = next;
    }
    resolve_all(engine, &order, "child", &parent.name)
}

/// Resolve a catalog-derived list of `(namespace, name)` relations to storage
/// handles.
///
/// Every name came out of the catalog moments earlier, so a miss is a torn
/// catalog rather than anything the user did — reported as an internal error
/// naming `noun` (`"child"`, `"partition"`) and the relation it hangs off.
/// Shared by the two callers so that framing, and the SQLSTATE, cannot drift
/// apart between them.
fn resolve_all(
    engine: &Arc<dyn TableEngine>,
    relations: &[(String, String)],
    noun: &str,
    parent_name: &str,
) -> Result<Vec<Arc<dyn TableAm>>, BindError> {
    relations
        .iter()
        .map(|(namespace, name)| {
            engine.resolve(Some(namespace), name).map_err(|e| {
                BindError::new(
                    sqlstate::INTERNAL_ERROR,
                    format!("{noun} \"{name}\" of \"{parent_name}\" is unreadable: {e}"),
                )
            })
        })
        .collect()
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
    resolve_all(engine, &leaves, "partition", &parent.name)
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
) -> Result<(Arc<dyn TableAm>, String, bool), BindError> {
    let ast::TableFactor::Table {
        name, only, alias, ..
    } = relation
    else {
        return Err(BindError::feature_not_supported(format!(
            "target is not supported yet: {relation}"
        )));
    };
    // A partitioned parent is a valid UPDATE/DELETE target, so it is not
    // rejected here: the executor routes through its leaves, which each binder
    // captures via `partition_leaves`.
    let (table, table_name) = resolve_write_table(engine, name, verb)?;
    let qualifier = aliased_qualifier(alias, table_name)?;
    Ok((table, qualifier, *only))
}

fn bind_where(
    selection: &Option<ast::Expr>,
    scope: &Scope,
) -> Result<Option<BoundExpr>, BindError> {
    selection
        .as_ref()
        .map(|expr| to_bool_operand(bind_expr(expr, scope)?, "WHERE", expr.span()))
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
    walk_exprs_mut(plan, &mut ParamSubstitution { params });
}

/// The [`ExprVisitor`] behind [`substitute_params`]: the plan walk is shared
/// (see [`crate::logical_plan::walk_exprs_mut`]), only the leaf rewrite is ours.
struct ParamSubstitution<'a> {
    params: &'a [Value],
}

impl ExprVisitor for ParamSubstitution<'_> {
    fn expr(&mut self, expr: &mut BoundExpr) {
        subst_expr(expr, self.params);
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
        BoundExpr::Const { .. }
        | BoundExpr::ColumnRef { .. }
        | BoundExpr::OuterColumnRef { .. } => {}
        BoundExpr::Unary { expr, .. } => subst_expr(expr, params),
        BoundExpr::Binary { left, right, .. } => {
            subst_expr(left, params);
            subst_expr(right, params);
        }
        BoundExpr::IsNull { expr, .. } | BoundExpr::BoolTest { expr, .. } => {
            subst_expr(expr, params)
        }
        BoundExpr::Coerce { expr, .. } => subst_expr(expr, params),
        BoundExpr::Collate { expr, .. } => subst_expr(expr, params),
        BoundExpr::Reinterpret { expr, .. } => subst_expr(expr, params),
        BoundExpr::FuncCall { args, .. }
        | BoundExpr::Routine { args, .. }
        | BoundExpr::Srf { args, .. }
        | BoundExpr::Coalesce { args, .. }
        | BoundExpr::MinMax { args, .. } => subst_exprs(args, params),
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
        // A `$n` can appear in an argument or in the aggregate's own ORDER BY
        // (`array_agg(x ORDER BY $1)`), so both are substituted.
        BoundExpr::Aggregate {
            agg_args, order_by, ..
        } => {
            for arg in agg_args
                .iter_mut()
                .chain(order_by.iter_mut().map(|k| &mut k.expr))
            {
                subst_expr(arg, params);
            }
        }
        // A `$n` can appear in an argument (`lag(x, $1)`) or in the OVER clause
        // itself (`PARTITION BY $1`), so both are substituted.
        BoundExpr::WindowFunc { kind, spec, .. } => {
            for arg in kind.args_mut().iter_mut().chain(spec.exprs_mut()) {
                subst_expr(arg, params);
            }
        }
        // A `$n` may appear inside the subquery body, and (for IN) inside the
        // needle carried by the comparison template.
        BoundExpr::ScalarSubquery { subplan, .. }
        | BoundExpr::ArraySubquery { subplan, .. }
        | BoundExpr::Exists { subplan, .. } => {
            substitute_params(&mut subplan.plan, params);
        }
        BoundExpr::QuantifiedSubquery { subplan, cmp, .. } => {
            substitute_params(&mut subplan.plan, params);
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
/// subplan.
///
/// **`depth` increments exactly where the binder pushes a correlation level**,
/// which is one place: [`crate::Scope::as_outer_levels`], reached only when
/// binding an expression-subquery marker (`(SELECT …)`, `EXISTS`, `IN (SELECT
/// …)`, `ANY`/`ALL`). Nothing else is a level — not `Subquery`, which the binder
/// also synthesizes as a same-level projection wrapper, and not `Limit`,
/// `Window` or `SetOp`. Incrementing anywhere else strands a correlated
/// reference in the `level < depth` branch below, where it is silently left
/// alone and later filled from the wrong row (or reaches `eval` unsubstituted).
///
/// Comparing `level` to `depth`: `level == depth`
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
        LogicalPlan::Values(ValuesPlan {
            rows, predicate, ..
        }) => {
            for row in rows {
                for e in row.iter_mut() {
                    subst_outer_expr(e, outer, depth);
                }
            }
            if let Some(p) = predicate {
                subst_outer_expr(p, outer, depth);
            }
        }
        LogicalPlan::Query(QueryPlan {
            projections,
            predicate,
            ..
        }) => {
            for e in projections.iter_mut() {
                subst_outer_expr(e, outer, depth);
            }
            if let Some(p) = predicate {
                subst_outer_expr(p, outer, depth);
            }
        }
        LogicalPlan::Subquery(SubqueryPlan {
            source,
            projections,
            predicate,
            ..
        }) => {
            // Same depth: a `Subquery` is not necessarily a derived table. The
            // binder also synthesizes one as the projection wrapper over a
            // window chain (`finish_windowed_select`), over a sorted `Limit`
            // (`attach_sort`) and over a FROM-less SRF — none of which is a query
            // level. A real derived table's source binds with an empty outer
            // scope, so it holds no `OuterColumnRef` for the distinction to
            // matter to; see `substitute_outer`.
            subst_outer_plan(source, outer, depth);
            for e in projections.iter_mut() {
                subst_outer_expr(e, outer, depth);
            }
            if let Some(p) = predicate {
                subst_outer_expr(p, outer, depth);
            }
        }
        // A window step is part of its query, not a nesting level of its own, so
        // `source` recurses at the *same* depth — unlike a derived table above.
        // Getting this wrong would shift every correlated reference in a window
        // argument one level out of range.
        LogicalPlan::Window(WindowPlan {
            source,
            spec,
            funcs,
            ..
        }) => {
            subst_outer_plan(source, outer, depth);
            for expr in spec.exprs_mut() {
                subst_outer_expr(expr, outer, depth);
            }
            for func in funcs {
                for arg in func.kind.args_mut() {
                    subst_outer_expr(arg, outer, depth);
                }
            }
        }
        LogicalPlan::TableFunction(TableFunctionPlan {
            args,
            projections,
            predicate,
            ..
        }) => {
            for e in args.iter_mut().chain(projections.iter_mut()) {
                subst_outer_expr(e, outer, depth);
            }
            if let Some(p) = predicate {
                subst_outer_expr(p, outer, depth);
            }
        }
        LogicalPlan::Join(JoinPlan {
            source,
            projections,
            predicate,
            ..
        }) => {
            subst_outer_join(source, outer, depth);
            for e in projections.iter_mut() {
                subst_outer_expr(e, outer, depth);
            }
            if let Some(p) = predicate {
                subst_outer_expr(p, outer, depth);
            }
        }
        // An Append holds only leaf table handles — no correlated exprs.
        LogicalPlan::Append(AppendPlan { .. }) => {}
        // A set operation is not its own query nesting level: its arms bound in
        // the enclosing scope, so they keep this `depth`.
        LogicalPlan::SetOp(SetOpPlan { arms, .. }) => {
            for arm in arms.iter_mut() {
                subst_outer_plan(&mut arm.plan, outer, depth);
                for e in arm.coercion.iter_mut().flatten() {
                    subst_outer_expr(e, outer, depth);
                }
            }
        }
        LogicalPlan::Limit(LimitPlan { source, .. }) => subst_outer_plan(source, outer, depth),
        LogicalPlan::Aggregate(AggregatePlan {
            input,
            predicate,
            group_exprs,
            aggregates,
            having,
            projections,
            ..
        }) => {
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
                for arg in agg.exprs_mut() {
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
        LogicalPlan::Insert(InsertPlan {
            source, returning, ..
        }) => {
            match source {
                InsertSource::Values(rows) => {
                    for row in rows {
                        for e in row.iter_mut() {
                            subst_outer_expr(e, outer, depth);
                        }
                    }
                }
                InsertSource::Tuples { defaults, .. } => {
                    for (_, default) in defaults.iter_mut() {
                        subst_outer_expr(default, outer, depth);
                    }
                }
                InsertSource::Query { input, projections } => {
                    // Same depth, per `substitute_outer`'s rule. Unreachable in
                    // practice: `substitute_outer` only ever runs on an
                    // expression-subquery body, which is always a SELECT.
                    subst_outer_plan(input, outer, depth);
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
        LogicalPlan::Update(UpdatePlan {
            predicate,
            assignments,
            returning,
            ..
        }) => {
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
        LogicalPlan::Delete(DeletePlan {
            predicate,
            returning,
            ..
        }) => {
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
            JoinInput::Scan { .. } => {}
            // Same depth. A FROM subquery binds with an empty outer scope, so
            // no `OuterColumnRef` inside it can *escape* it — see the
            // `debug_assert!` in `bind_from_item`'s Derived arm. (It may well
            // hold references of its own, correlating one of its nested
            // subqueries to its own relations; those sit below a marker, so
            // they are visited at a deeper `depth` than their `level` and left
            // alone.)
            // TODO: LATERAL — it binds a FROM subquery against the enclosing
            // scope, so it needs the `depth + 1` back here.
            JoinInput::Subplan(plan) => subst_outer_plan(plan, outer, depth),
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
        | BoundExpr::BoolTest { expr, .. }
        | BoundExpr::Coerce { expr, .. }
        | BoundExpr::Collate { expr, .. }
        | BoundExpr::Reinterpret { expr, .. } => subst_outer_expr(expr, outer, depth),
        BoundExpr::Binary { left, right, .. } => {
            subst_outer_expr(left, outer, depth);
            subst_outer_expr(right, outer, depth);
        }
        BoundExpr::FuncCall { args, .. }
        | BoundExpr::Routine { args, .. }
        | BoundExpr::Srf { args, .. }
        | BoundExpr::Coalesce { args, .. }
        | BoundExpr::MinMax { args, .. } => {
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
        BoundExpr::Aggregate {
            agg_args, order_by, ..
        } => {
            for a in agg_args
                .iter_mut()
                .chain(order_by.iter_mut().map(|k| &mut k.expr))
            {
                subst_outer_expr(a, outer, depth);
            }
        }
        // An OVER clause is part of the enclosing query, not a query of its
        // own, so its arguments and keys stay at `depth`.
        BoundExpr::WindowFunc { kind, spec, .. } => {
            for a in kind.args_mut().iter_mut().chain(spec.exprs_mut()) {
                subst_outer_expr(a, outer, depth);
            }
        }
        // A nested expression-subquery is one query level deeper.
        BoundExpr::ScalarSubquery { subplan, .. }
        | BoundExpr::ArraySubquery { subplan, .. }
        | BoundExpr::Exists { subplan, .. } => {
            subst_nested(subplan, outer, depth + 1);
        }
        BoundExpr::QuantifiedSubquery { subplan, cmp, .. } => {
            subst_nested(subplan, outer, depth + 1);
            subst_outer_expr(cmp, outer, depth);
        }
        // The array operand and the needle live at this query level.
        BoundExpr::QuantifiedArray { array, cmp, .. } => {
            subst_outer_expr(array, outer, depth);
            subst_outer_expr(cmp, outer, depth);
        }
    }
}

/// Substitute into a nested subplan, and tell it when that actually changed it.
///
/// A subplan whose own body this pass rewrote no longer matches the template its
/// [`SubplanId`](crate::SubplanId) names — the executor would otherwise cache one
/// outer row's answer and serve it to the next. `plan_has_outer_refs_at` is the
/// exact test for "substitution at this depth or beyond will touch something",
/// which is why it is asked *before* the rewrite rather than after.
fn subst_nested(subplan: &mut crate::expr::Subplan, outer: &[Value], depth: usize) {
    let rebound = plan_has_outer_refs_at(&subplan.plan, depth);
    subst_outer_plan(&mut subplan.plan, outer, depth);
    if rebound {
        subplan.mark_rebound();
    }
}

/// Whether a bound plan holds a correlated outer reference
/// ([`BoundExpr::OuterColumnRef`]) that *escapes* it — one bound to a query
/// enclosing `plan`, which therefore has to be filled by [`substitute_outer`] at
/// a boundary above. Such a plan cannot be folded once before execution because
/// its value depends on an enclosing row; the executor uses this to leave those
/// subqueries for per-outer-row evaluation.
///
/// "Escapes" is the whole point, and the reason this walk carries a depth. A
/// plan routinely contains outer references that are entirely its own business:
/// `(SELECT a, (SELECT max(b) FROM u WHERE u.k = s.a) FROM s)` holds an
/// `OuterColumnRef { level: 1 }` naming `s`, but `s` is inside the plan, so
/// nothing above it has a row to fill that reference from. The depth arithmetic
/// here mirrors [`subst_outer_plan`] exactly — same base of 1, incremented at
/// the same single boundary — so that a reference counts as escaping precisely
/// when `substitute_outer` would act on it: `level >= depth`.
pub fn plan_has_outer_refs(plan: &LogicalPlan) -> bool {
    plan_has_outer_refs_at(plan, 1)
}

/// [`plan_has_outer_refs`] for a plan reached at correlation `depth` rather than
/// at the top of the walk.
fn plan_has_outer_refs_at(plan: &LogicalPlan, depth: usize) -> bool {
    let mut found = false;
    for_each_plan_expr(plan, depth, &mut |e, depth| {
        if expr_has_outer_ref(e, depth) {
            found = true;
        }
    });
    found
}

/// Whether any expression node anywhere in `plan` satisfies `pred`. See
/// [`BoundExpr::any_node`] for why the walk takes the predicate rather than
/// naming what it looks for.
pub(crate) fn plan_any_expr_node(plan: &LogicalPlan, pred: &dyn Fn(&BoundExpr) -> bool) -> bool {
    let mut found = false;
    // A match anywhere answers the question, however deeply nested, so this
    // visitor ignores the depth.
    for_each_plan_expr(plan, 1, &mut |e, _| {
        found = found || e.any_node(pred);
    });
    found
}

/// Whether `plan` must run with a transaction id assigned to it. Two kinds of
/// node say so, for two different reasons.
///
/// A routine body may write, and the executor cannot tell before running it, so
/// a statement that calls one is treated as a write: it needs an XID to stamp
/// the body's versions with.
///
/// `txid_current()` / `pg_current_xact_id()` need one because the id *is* their
/// answer, and PostgreSQL assigns one when the transaction has none. Their
/// `_if_assigned` forms deliberately do not count: reporting NULL when there is
/// no XID is their whole meaning.
///
/// One predicate and not two, because both reasons carry the same second
/// requirement — the body runs, and the id is read, only while the transaction
/// is still open, so such a result set must be drained before it is finalized
/// rather than streamed after.
pub fn plan_needs_xid(plan: &LogicalPlan) -> bool {
    plan_any_expr_node(plan, &|e| {
        matches!(
            e,
            BoundExpr::Routine { .. }
                | BoundExpr::FuncCall {
                    func: ScalarFn::CurrentXactId {
                        if_assigned: false,
                        ..
                    },
                    ..
                }
        )
    })
}

/// Visit every top-level expression of `plan`, recursing through structural
/// sub-plans (derived tables, join inputs, DML sources) and passing each
/// expression the correlation `depth` it sits at. Does not descend into
/// expression-subquery markers — the per-expression walk handles those, and it
/// is the only place `depth` increments.
///
/// Depth moves exactly as it does in [`subst_outer_plan`]; see that function's
/// comment for why none of the structural nodes below is a query nesting level.
fn for_each_plan_expr(plan: &LogicalPlan, depth: usize, f: &mut impl FnMut(&BoundExpr, usize)) {
    match plan {
        LogicalPlan::Values(ValuesPlan {
            rows, predicate, ..
        }) => {
            rows.iter().flatten().for_each(|e| f(e, depth));
            if let Some(p) = predicate {
                f(p, depth);
            }
        }
        LogicalPlan::Query(QueryPlan {
            projections,
            predicate,
            ..
        }) => {
            projections.iter().for_each(|e| f(e, depth));
            if let Some(p) = predicate {
                f(p, depth);
            }
        }
        LogicalPlan::Subquery(SubqueryPlan {
            source,
            projections,
            predicate,
            ..
        }) => {
            for_each_plan_expr(source, depth, &mut *f);
            projections.iter().for_each(|e| f(e, depth));
            if let Some(p) = predicate {
                f(p, depth);
            }
        }
        LogicalPlan::Window(WindowPlan {
            source,
            spec,
            funcs,
            ..
        }) => {
            for_each_plan_expr(source, depth, &mut *f);
            spec.exprs().for_each(|e| f(e, depth));
            funcs
                .iter()
                .flat_map(|w| w.kind.args())
                .for_each(|e| f(e, depth));
        }
        LogicalPlan::TableFunction(TableFunctionPlan {
            args,
            projections,
            predicate,
            ..
        }) => {
            args.iter()
                .chain(projections.iter())
                .for_each(|e| f(e, depth));
            if let Some(p) = predicate {
                f(p, depth);
            }
        }
        LogicalPlan::Join(JoinPlan {
            source,
            projections,
            predicate,
            ..
        }) => {
            for_each_join_expr(source, depth, &mut *f);
            projections.iter().for_each(|e| f(e, depth));
            if let Some(p) = predicate {
                f(p, depth);
            }
        }
        // An Append exposes no expressions of its own.
        LogicalPlan::Append(AppendPlan { .. }) => {}
        LogicalPlan::SetOp(SetOpPlan { arms, .. }) => {
            for arm in arms {
                for_each_plan_expr(&arm.plan, depth, &mut *f);
                arm.coercion.iter().flatten().for_each(|e| f(e, depth));
            }
        }
        LogicalPlan::Limit(LimitPlan { source, .. }) => for_each_plan_expr(source, depth, &mut *f),
        LogicalPlan::Aggregate(AggregatePlan {
            input,
            predicate,
            group_exprs,
            aggregates,
            having,
            projections,
            ..
        }) => {
            if let AggInput::Join(join) = input {
                for_each_join_expr(join, depth, &mut *f);
            }
            if let Some(p) = predicate {
                f(p, depth);
            }
            group_exprs.iter().for_each(|e| f(e, depth));
            for agg in aggregates {
                for arg in agg.exprs() {
                    f(arg, depth);
                }
            }
            if let Some(h) = having {
                f(h, depth);
            }
            projections.iter().for_each(|e| f(e, depth));
        }
        LogicalPlan::Insert(InsertPlan {
            source, returning, ..
        }) => {
            match source {
                InsertSource::Values(rows) => rows.iter().flatten().for_each(|e| f(e, depth)),
                InsertSource::Tuples { defaults, .. } => {
                    defaults.iter().for_each(|(_, e)| f(e, depth));
                }
                InsertSource::Query { input, projections } => {
                    for_each_plan_expr(input, depth, &mut *f);
                    projections.iter().for_each(|e| f(e, depth));
                }
            }
            if let Some(r) = returning {
                r.projections.iter().for_each(|e| f(e, depth));
            }
        }
        LogicalPlan::Update(UpdatePlan {
            predicate,
            assignments,
            returning,
            ..
        }) => {
            if let Some(p) = predicate {
                f(p, depth);
            }
            for (_, e) in assignments {
                f(e, depth);
            }
            if let Some(r) = returning {
                r.projections.iter().for_each(|e| f(e, depth));
            }
        }
        LogicalPlan::Delete(DeletePlan {
            predicate,
            returning,
            ..
        }) => {
            if let Some(p) = predicate {
                f(p, depth);
            }
            if let Some(r) = returning {
                r.projections.iter().for_each(|e| f(e, depth));
            }
        }
    }
}

fn for_each_join_expr(join: &JoinExpr, depth: usize, f: &mut impl FnMut(&BoundExpr, usize)) {
    match join {
        JoinExpr::Input { input, .. } => match input {
            JoinInput::Scan { .. } => {}
            JoinInput::Subplan(plan) => for_each_plan_expr(plan, depth, &mut *f),
            JoinInput::TableFunction { args, .. } => args.iter().for_each(|e| f(e, depth)),
        },
        JoinExpr::Join {
            left,
            right,
            predicate,
            ..
        } => {
            for_each_join_expr(left, depth, &mut *f);
            for_each_join_expr(right, depth, &mut *f);
            if let Some(p) = predicate {
                f(p, depth);
            }
        }
    }
}

/// Whether an expression tree holds an [`BoundExpr::OuterColumnRef`] that
/// escapes the plan this walk started at, given the correlation `depth` this
/// expression sits at.
fn expr_has_outer_ref(expr: &BoundExpr, depth: usize) -> bool {
    let mut found = false;
    for_each_subexpr(expr, depth, &mut |expr, depth| {
        // Mirrors `subst_outer_expr`: `level == depth` names the immediate
        // parent row and `level > depth` a still-outer one — both are filled
        // from above, so both escape. `level < depth` names an intervening
        // inner query and stays entirely within this plan.
        if let BoundExpr::OuterColumnRef { level, .. } = expr
            && *level >= depth
        {
            found = true;
        }
    });
    found
}

/// Call `f(node, depth)` for every node of `expr`, with the correlation depth
/// that node sits at.
///
/// Descends into nested expression-subquery subplans, which is the one boundary
/// that pushes a level — see [`plan_has_outer_refs`]. Callers decide what to do
/// with `depth` themselves, since "escapes this plan" and "names *this* row" are
/// different questions asked of the same walk.
pub(crate) fn for_each_subexpr(
    expr: &BoundExpr,
    depth: usize,
    f: &mut dyn FnMut(&BoundExpr, usize),
) {
    f(expr, depth);
    match expr {
        BoundExpr::OuterColumnRef { .. }
        | BoundExpr::Const { .. }
        | BoundExpr::ColumnRef { .. }
        | BoundExpr::Param { .. } => {}
        BoundExpr::Unary { expr, .. }
        | BoundExpr::IsNull { expr, .. }
        | BoundExpr::BoolTest { expr, .. }
        | BoundExpr::Coerce { expr, .. }
        | BoundExpr::Collate { expr, .. }
        | BoundExpr::Reinterpret { expr, .. } => for_each_subexpr(expr, depth, f),
        BoundExpr::Binary { left, right, .. } => {
            for_each_subexpr(left, depth, f);
            for_each_subexpr(right, depth, f);
        }
        BoundExpr::FuncCall { args, .. }
        | BoundExpr::Routine { args, .. }
        | BoundExpr::Srf { args, .. }
        | BoundExpr::Coalesce { args, .. }
        | BoundExpr::MinMax { args, .. } => {
            args.iter().for_each(|e| for_each_subexpr(e, depth, f));
        }
        BoundExpr::Aggregate {
            agg_args, order_by, ..
        } => {
            agg_args
                .iter()
                .chain(order_by.iter().map(|k| &k.expr))
                .for_each(|e| for_each_subexpr(e, depth, f));
        }
        BoundExpr::ArrayCtor { elems, .. } => {
            elems.iter().for_each(|e| for_each_subexpr(e, depth, f));
        }
        BoundExpr::Subscript { base, index, .. } => {
            for_each_subexpr(base, depth, f);
            for_each_subexpr(index, depth, f);
        }
        BoundExpr::Case { whens, else_, .. } => {
            for (condition, result) in whens {
                for_each_subexpr(condition, depth, f);
                for_each_subexpr(result, depth, f);
            }
            if let Some(e) = else_ {
                for_each_subexpr(e, depth, f);
            }
        }
        BoundExpr::WindowFunc { kind, spec, .. } => {
            kind.args()
                .iter()
                .chain(spec.exprs())
                .for_each(|e| for_each_subexpr(e, depth, f));
        }
        // A nested expression-subquery is one query level deeper — the same
        // `depth + 1` `subst_outer_expr` applies.
        BoundExpr::ScalarSubquery { subplan, .. }
        | BoundExpr::ArraySubquery { subplan, .. }
        | BoundExpr::Exists { subplan, .. } => {
            plan_for_each_subexpr(&subplan.plan, depth + 1, f);
        }
        BoundExpr::QuantifiedSubquery { subplan, cmp, .. } => {
            plan_for_each_subexpr(&subplan.plan, depth + 1, f);
            for_each_subexpr(cmp, depth, f);
        }
        BoundExpr::QuantifiedArray { array, cmp, .. } => {
            for_each_subexpr(array, depth, f);
            for_each_subexpr(cmp, depth, f);
        }
    }
}

/// [`for_each_subexpr`] over every expression of a plan reached at `depth`.
fn plan_for_each_subexpr(plan: &LogicalPlan, depth: usize, f: &mut dyn FnMut(&BoundExpr, usize)) {
    for_each_plan_expr(plan, depth, &mut |expr, depth| {
        for_each_subexpr(expr, depth, f);
    });
}

/// The slots of the *immediately enclosing* row that `plan` reads, with the type
/// each is read as, deduplicated and in slot order — everything
/// [`substitute_outer`] would fill in from that row.
///
/// `None` when the plan reads a row further out than the enclosing one, which
/// [`substitute_outer`] leaves for a boundary above: the value then is not a
/// function of the enclosing row alone, so nothing may be keyed on it.
///
/// The executor uses this to memoize a correlated subplan: two outer rows
/// agreeing on these slots must produce the same answer, since the plan run for
/// them is the same plan.
pub fn plan_outer_ref_slots(plan: &LogicalPlan) -> Option<Vec<(usize, PgType)>> {
    let mut slots = std::collections::BTreeMap::new();
    let mut escapes_further = false;
    plan_for_each_subexpr(plan, 1, &mut |expr, depth| {
        if let BoundExpr::OuterColumnRef { level, index, ty } = expr {
            if *level == depth {
                slots.insert(*index, *ty);
            } else if *level > depth {
                escapes_further = true;
            }
        }
    });
    (!escapes_further).then(|| slots.into_iter().collect())
}

/// Whether `plan` calls a volatile function (or a routine, which PostgreSQL
/// defaults to volatile) anywhere.
///
/// Running such a plan once and reusing the answer would change how many times
/// it fires, which is observable — a sequence advances by a different amount, a
/// routine's writes happen a different number of times.
/// Unlike [`BoundExpr::contains_volatile_fn`], which stops at a subquery marker
/// because a subquery's body is a plan of its own, this descends into those
/// bodies: they all run under the plan being asked about.
pub fn plan_contains_volatile_fn(plan: &LogicalPlan) -> bool {
    let mut found = false;
    // The node-level test, not the subtree one: the walk already reaches every
    // descendant, and it crosses the subquery markers `contains_volatile_fn`
    // stops at.
    plan_for_each_subexpr(plan, 1, &mut |expr, _| {
        if expr.is_volatile_call() {
            found = true;
        }
    });
    found
}

/// Whether `expr` holds a subquery marker whose subplan is *correlated* — one
/// the executor cannot fold to a constant before the scan starts.
///
/// The distinction is what separates a marker that costs a subplan execution per
/// row from one that costs a `Const` clone: `resolve_subqueries` folds every
/// marker this returns `false` for, once, before any row is read, using this
/// very predicate ([`plan_has_outer_refs`]).
///
/// Only the expression's own markers are asked. A marker nested inside another
/// marker's subplan does not matter: if the enclosing one folds, the whole
/// subtree folds with it, and if it does not, the enclosing one already answers
/// `true`.
pub fn expr_contains_correlated_subquery(expr: &BoundExpr) -> bool {
    let mut found = false;
    for_each_subexpr(expr, 1, &mut |expr, depth| {
        // `depth > 1` is a node inside some marker's subplan; the walk pushes a
        // level at exactly that boundary.
        if depth != 1 {
            return;
        }
        let subplan = match expr {
            BoundExpr::ScalarSubquery { subplan, .. }
            | BoundExpr::ArraySubquery { subplan, .. }
            | BoundExpr::Exists { subplan, .. }
            | BoundExpr::QuantifiedSubquery { subplan, .. } => subplan,
            _ => return,
        };
        if plan_has_outer_refs(&subplan.plan) {
            found = true;
        }
    });
    found
}

/// Whether an expression calls a volatile function anywhere, crossing into the
/// body of a subquery marker.
///
/// Unlike [`BoundExpr::contains_volatile_fn`] this descends into the body of a
/// subquery marker. That matters wherever the decision being made is about the
/// marker itself: a conjunct holding `EXISTS (SELECT … nextval('s') …)` is
/// volatile in every sense that counts, since moving it changes how many rows
/// reach it and so how many times the sequence advances — but the shallow
/// predicate reports `false`, because a subquery's body is a plan of its own.
pub fn expr_contains_volatile_fn(expr: &BoundExpr) -> bool {
    let mut found = false;
    for_each_subexpr(expr, 1, &mut |expr, _| {
        if expr.is_volatile_call() {
            found = true;
        }
    });
    found
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
            bind_select(
                engine,
                catalog,
                params,
                select,
                &query.order_by,
                ctes,
                outer_scope,
            )
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
            let (limit, offset) = bind_limit_offset(clause, catalog, params)?;
            Ok(LogicalPlan::Limit(LimitPlan {
                source: Box::new(inner),
                limit,
                offset,
            }))
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
pub(crate) const MAX_SET_OP_NESTING: usize = 50;

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
/// `Subquery` here: the arms' projections and coercions index the arms' own
/// rows, which a wrapper would have to re-derive.
///
/// A `LIMIT` is the one boundary that must keep its own ordering — the bound
/// applies to the rows the inner sort chose — so the outer sort goes above it.
fn attach_sort(plan: LogicalPlan, sort: Vec<SortKey>, columns: Vec<OutputColumn>) -> LogicalPlan {
    if sort.is_empty() {
        return plan;
    }
    match plan {
        LogicalPlan::SetOp(SetOpPlan {
            arms,
            columns,
            distinct,
            sort: _,
        }) => LogicalPlan::SetOp(SetOpPlan {
            arms,
            columns,
            sort,
            distinct,
        }),
        plan @ LogicalPlan::Limit(LimitPlan { .. }) => LogicalPlan::Subquery(SubqueryPlan {
            projections: identity_projections(&columns),
            source: Box::new(plan),
            columns,
            predicate: None,
            sort,
            distinct: None,
        }),
        // Any other body already bound this query's ORDER BY itself.
        plan => plan,
    }
}

/// Bind a `UNION` / `UNION ALL` set operation.
///
/// TODO: bind `INTERSECT` and `EXCEPT`; both are rejected as unsupported.
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
    Ok(LogicalPlan::SetOp(SetOpPlan {
        arms,
        columns,
        sort,
        distinct,
    }))
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
        ast::SetExpr::Table(table) => {
            bind_table_query(engine, catalog, params, table, &None, ctes)?
        }
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
                ((d.collation != default).then_some(d.collation), d.strength)
            }
            None => (None, crate::collation::Strength::None),
        };
        // A modifier survives a set operation only when every contributing arm
        // declares the same one on the same type, which is how PG resolves a
        // set operation's common modifier: `varchar(20) UNION varchar(20)`
        // stays `character varying(20)`, `varchar(20) UNION char(5)` goes bare.
        let merged_typmod = crate::expr::common_typmod(
            arms.iter()
                .filter(|(plan, _)| !is_null_literal_column(plan, i))
                .map(|(_, cols)| {
                    if cols[i].ty == merged_ty {
                        cols[i].typmod
                    } else {
                        -1
                    }
                }),
        );
        columns.push(OutputColumn {
            name: first.name.clone(),
            ty: merged_ty,
            collation,
            strength,
            typmod: merged_typmod,
            generated: None,
        });
    }
    Ok(columns)
}

/// Whether an arm's column `index` is a bare `NULL` literal. Such a column has no
/// type of its own — the binder already resolved the untyped literal to `text` —
/// so a set operation lets the other arms decide the column type, as PG does.
/// NULL casts to anything, so adopting that type is always safe.
fn is_null_literal_column(plan: &LogicalPlan, index: usize) -> bool {
    let is_null_const = |e: &BoundExpr| {
        matches!(
            e,
            BoundExpr::Const {
                value: Value::Null,
                ..
            }
        )
    };
    match plan {
        // `SELECT NULL` / `VALUES (NULL)`: every row must be NULL in this column.
        LogicalPlan::Values(ValuesPlan { rows, .. }) => rows
            .iter()
            .all(|row| row.get(index).is_some_and(is_null_const)),
        LogicalPlan::Query(QueryPlan { projections, .. })
        | LogicalPlan::Subquery(SubqueryPlan { projections, .. })
        | LogicalPlan::TableFunction(TableFunctionPlan { projections, .. })
        | LogicalPlan::Join(JoinPlan { projections, .. })
        | LogicalPlan::Aggregate(AggregatePlan { projections, .. }) => {
            projections.get(index).is_some_and(is_null_const)
        }
        LogicalPlan::Limit(LimitPlan { source, .. }) => is_null_literal_column(source, index),
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
/// operator for every column, and so does the executor's dedup — which reaches
/// equality through `compare_values`. That is exactly `has_equality`, so a type
/// with a hash opclass but no btree one (`xid`) deduplicates fine here even
/// though it cannot be sorted.
fn reject_undedupable_columns(
    columns: &[OutputColumn],
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<(), BindError> {
    for col in columns {
        if !crate::expr::has_equality(col.ty, catalog.as_ref()) {
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

/// Fold a `LIMIT`/`OFFSET` clause into constant row counts. `LIMIT ALL`, a
/// `NULL` count, or an absent clause all mean "no bound" (`None`). Negative
/// counts are rejected with PG's wording.
///
/// TODO: evaluate a non-constant count; PG binds `LIMIT`/`OFFSET` as an
/// ordinary `bigint` expression, while only a constant integer folds here.
fn bind_limit_offset(
    clause: &ast::LimitClause,
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
) -> Result<(Option<i64>, Option<i64>), BindError> {
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
        .map(|e| bind_count_expr(e, "LIMIT", catalog, params))
        .transpose()?
        .flatten();
    let offset = offset
        .as_ref()
        .map(|o| bind_count_expr(&o.value, "OFFSET", catalog, params))
        .transpose()?
        .flatten();
    Ok((limit, offset))
}

/// Evaluate a single LIMIT/OFFSET count expression to a non-negative `i64`.
/// Returns `None` for a `NULL` literal (PG: no limit / offset 0). Non-constant
/// expressions are rejected as unsupported; negatives with PG's SQLSTATE.
fn bind_count_expr(
    expr: &ast::Expr,
    clause: &str,
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
) -> Result<Option<i64>, BindError> {
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
        None => {
            // PG analyzes LIMIT/OFFSET as an ordinary bigint expression, so a
            // misplaced aggregate or window call there is rejected by
            // *placement* before anything asks whether the value is constant.
            // Bind in the empty scope purely to classify: any other bind failure
            // is not the error to report, because this engine's real limitation
            // is the constant-only count that the fallthrough describes.
            if let Ok(bound) = bind_scalar(expr, &Scope::empty(catalog, params)) {
                reject_agg_or_window(&bound, clause)?;
            }
            Err(BindError::feature_not_supported(format!(
                "non-constant {clause} is not supported yet"
            )))
        }
    }
}

/// Constant-fold `expr` to an integer count. `Some(None)` is a recognized `NULL`
/// literal; `Some(Some(n))` a constant integer; `None` means not a constant we
/// evaluate. Handles integer literals and nested unary `+`/`-`.
fn const_i64(expr: &ast::Expr) -> Option<Option<i64>> {
    match expr {
        ast::Expr::Value(v) => match &v.value {
            ast::Value::Number(n, _) => crate::expr::literal_int(n)
                .and_then(|v| i64::try_from(v).ok())
                .map(Some),
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
/// order, each seeing earlier siblings. Duplicate names within one clause are
/// rejected.
///
/// TODO: `WITH RECURSIVE`, and a data-modifying CTE body (`INSERT`/`UPDATE`/
/// `DELETE` inside `WITH`).
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
    // Built before FROM so a join's `ON` can resolve `OVER w` too — it is the
    // only scope inside a SELECT other than the body's that can see one.
    let windows = named_windows(select)?;
    let BoundFrom {
        source,
        relations,
        visible,
    } = bind_from_clause(
        engine,
        catalog,
        params,
        &select.from,
        ctes,
        outer_scope,
        &windows,
        &SystemDemand::of_select(select, order_by),
    )?;
    let scope = Scope::relations_with_visible(relations, visible, catalog, params)
        .with_subqueries(engine, ctes)
        .with_outer(outer_scope.to_vec())
        .with_named_windows(&windows);
    let body = bind_select_body(select, order_by, &scope)?;
    if body.windows.is_empty() {
        return Ok(build_query_block(
            source,
            body.aggregation,
            body.columns,
            body.projections,
            body.predicate,
            body.sort,
            body.distinct,
        ));
    }
    // With windows in play the query block below the chain must reproduce its
    // input row unchanged: the chain's specs, its arguments and the projections
    // above it all index that row positionally. So the tail (projection, ORDER
    // BY, DISTINCT) moves up onto the wrapping `Subquery`, and the block keeps
    // only WHERE, grouping, and an identity projection.
    let identity = match &body.aggregation {
        Some(agg) => aggregate_identity_projection(agg),
        None => scope.identity_projection(),
    };
    let block = build_query_block(
        source,
        body.aggregation,
        internal_columns(&identity),
        identity,
        body.predicate,
        Vec::new(),
        None,
    );
    Ok(finish_windowed_select(
        block,
        body.windows,
        body.input_width,
        body.columns,
        body.projections,
        body.sort,
        body.distinct,
    ))
}

/// Assemble one SELECT's query block: an [`LogicalPlan::Aggregate`] when the
/// query groups, else the compact single-source variant or a
/// [`LogicalPlan::Join`]. The projection tail passed in is the SELECT's own,
/// unless a window chain sits above — then it is an identity projection and the
/// real tail rides the wrapping `Subquery`.
fn build_query_block(
    source: JoinExpr,
    aggregation: Option<Aggregation>,
    columns: Vec<OutputColumn>,
    projections: Vec<BoundExpr>,
    predicate: Option<BoundExpr>,
    sort: Vec<SortKey>,
    distinct: Option<Vec<DistinctKey>>,
) -> LogicalPlan {
    if let Some(agg) = aggregation {
        let input = match source {
            // A scan that appends system slots keeps the general join leaf:
            // `AggInput::Scan` describes a relation by its schema alone, and the
            // slots live past it.
            JoinExpr::Input {
                input:
                    JoinInput::Scan {
                        table,
                        system: None,
                    },
                ..
            } => AggInput::Scan(table),
            // A derived table, CTE reference, `VALUES` in FROM, or set-returning
            // function feeds the aggregate through the same join machinery as a
            // multi-relation FROM — an `Input` node is just a single-source tree.
            source => AggInput::Join(source),
        };
        return LogicalPlan::Aggregate(AggregatePlan {
            input,
            predicate,
            group_exprs: agg.group_exprs,
            aggregates: agg.aggregates,
            having: agg.having,
            columns,
            projections,
            sort,
            distinct,
        });
    }
    match source {
        JoinExpr::Input { input, .. } => {
            finish_single_select(input, columns, projections, predicate, sort, distinct)
        }
        source @ JoinExpr::Join { .. } => LogicalPlan::Join(JoinPlan {
            source,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        }),
    }
}

/// The identity projection over an aggregate node's output row,
/// `[group keys…, aggregates…]`.
fn aggregate_identity_projection(agg: &Aggregation) -> Vec<BoundExpr> {
    agg.group_exprs
        .iter()
        .map(BoundExpr::ty)
        .chain(agg.aggregates.iter().map(|a| a.ret))
        .enumerate()
        .map(|(index, ty)| BoundExpr::ColumnRef { index, ty })
        .collect()
}

/// The SELECT's `WINDOW w AS (…)` definitions, by normalized name, each already
/// merged with the base it names.
///
/// PG resolves the clause left to right, so a definition may name only an
/// *earlier* one; expanding here means a self reference (`WINDOW w AS (w)`) and a
/// forward reference both fall out as "window … does not exist", and nothing
/// downstream has to chase a base. It also means the clause's own copy errors
/// fire whether or not the definition is ever referenced, as in PG.
///
/// The bodies are still bound lazily, so an *unreferenced* definition's
/// expressions are not column-checked, where PG raises `42703` for one naming a
/// missing column. Binding eagerly instead would reject the explicit frames PG
/// accepts and this build does not implement — the worse error. See the window
/// notes in the smoke suite.
///
/// TODO: column-check an unreferenced `WINDOW` definition, which needs explicit
/// frames to bind rather than error first.
fn named_windows(select: &ast::Select) -> Result<NamedWindows, BindError> {
    let mut map = std::collections::HashMap::new();
    for ast::NamedWindowDefinition(name, definition) in &select.named_window {
        let key = normalize_ident(name);
        let spec = match definition {
            ast::NamedWindowExpr::WindowSpec(spec) => spec.clone(),
            // `WINDOW w AS other` is not PG syntax; our parser only accepts it
            // for dialects that allow it, so this is unreachable here.
            ast::NamedWindowExpr::NamedWindow(other) => {
                return Err(BindError::feature_not_supported(format!(
                    "WINDOW {key} AS {other} is not supported yet"
                )));
            }
        };
        // The duplicate check precedes expansion, as in PG: `WINDOW w AS (…),
        // w AS (nosuchwin)` reports the duplicate, not the missing base.
        if map.contains_key(&key) {
            return Err(BindError::new(
                sqlstate::WINDOWING_ERROR,
                format!("window \"{key}\" is already defined"),
            ));
        }
        let spec = crate::functions::expand_window_base(
            spec,
            |base| map.get(base),
            crate::functions::WindowCopyOrigin::Definition,
        )?;
        map.insert(key, spec);
    }
    Ok(std::rc::Rc::new(map))
}

/// Preserve the compact single-source plan variants when FROM contains no
/// comma or explicit join.
fn finish_single_select(
    input: JoinInput,
    columns: Vec<OutputColumn>,
    projections: Vec<BoundExpr>,
    predicate: Option<BoundExpr>,
    sort: Vec<SortKey>,
    distinct: Option<Vec<DistinctKey>>,
) -> LogicalPlan {
    match input {
        JoinInput::Scan { table, system } => LogicalPlan::Query(QueryPlan {
            table,
            system,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        }),
        JoinInput::Subplan(source) => LogicalPlan::Subquery(SubqueryPlan {
            source,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        }),
        JoinInput::TableFunction {
            func,
            args,
            ordinality,
        } => LogicalPlan::TableFunction(TableFunctionPlan {
            func,
            args,
            ordinality,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        }),
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
        only: false,
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
    // `TABLE t` has no projection list to name a system column; only its ORDER
    // BY can (`TABLE t ORDER BY tableoid`).
    let demand = SystemDemand::of_order_by(order_by);
    let BoundFrom {
        source,
        relations,
        visible,
    } = bind_from_item(engine, catalog, params, &relation, ctes, &[], &[], &demand)?
        .into_bound_from();
    let scope = Scope::relations_with_visible(relations, visible, catalog, params);
    let mut columns = Vec::new();
    let mut projections = Vec::new();
    for (col, expr) in scope.expand_wildcard()? {
        columns.push(col);
        projections.push(expr);
    }
    let sort = bind_order_by(order_by, &columns, &scope, &mut projections, true)?;
    // `TABLE t ORDER BY rank() OVER (…)` is legal — `TABLE t` is `SELECT * FROM
    // t`, and its ORDER BY is bound with hidden columns just like a SELECT's, so
    // it can hold a window call. Without extraction the marker would survive
    // into the plan and fail at evaluation.
    let input_width = scope.width();
    let windows = extract_windows(&mut projections, input_width)?;
    let JoinExpr::Input { input, .. } = source else {
        // A single relation reference never produces a join tree.
        unreachable!("TABLE t binds a single relation");
    };
    if windows.is_empty() {
        return Ok(finish_single_select(
            input,
            columns,
            projections,
            None,
            sort,
            None,
        ));
    }
    // With a chain above, the block below it reproduces its input row unchanged
    // and the tail rides the wrapping `Subquery` — as in `bind_select`. `TABLE t`
    // has no WHERE, GROUP BY or DISTINCT, so nothing else interleaves.
    let identity = scope.identity_projection();
    let block = finish_single_select(
        input,
        internal_columns(&identity),
        identity,
        None,
        Vec::new(),
        None,
    );
    Ok(finish_windowed_select(
        block,
        windows,
        input_width,
        columns,
        projections,
        sort,
        None,
    ))
}

/// Convert a rowset's output columns into storage `Column`s for a [`Scope`].
fn to_columns(columns: &[OutputColumn]) -> Vec<Column> {
    columns
        .iter()
        .map(|c| {
            let mut col = Column::with_typmod(c.name.clone(), c.ty, c.typmod);
            col.collation = c.collation;
            col.generated = c.generated.clone();
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
    /// Where this item's system columns start in its own row, when the query
    /// asked for any. Everything from that index on is a system slot, in
    /// [`SysCol::ALL`] order. `None` for everything that is not a relation scan
    /// — a subquery, CTE, view or table function exposes no system columns, as
    /// upstream.
    system_base: Option<usize>,
    /// See [`ScopeItem::declined_system`].
    declined_system: Option<DeclinedSystem>,
}

/// A bound FROM clause (or one comma-delimited `TableWithJoins` group): its
/// executable row-source tree and the flat relation namespace exposed to
/// projection/WHERE/GROUP BY binding.
struct BoundFrom {
    source: JoinExpr,
    relations: Vec<ScopeItem>,
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
            relations: vec![ScopeItem {
                qualifier: self.qualifier,
                columns: to_columns(&self.columns),
                system_base: self.system_base,
                declined_system: self.declined_system,
            }],
            visible: None,
        }
    }
}

/// Maximum depth of nested view expansion. A cycle is caught by identity before
/// this trips, so the cap exists for the acyclic case: each level costs a full
/// `bind_view_query` → `bind_from_item` frame chain, and the native stack runs
/// out around 45 levels — an abort that takes the whole server with it.
const MAX_VIEW_EXPANSION_DEPTH: usize = 32;

/// Maximum number of view expansions in one statement bind. Depth alone does not
/// bound the *work*: a view referencing another twice doubles the expansions per
/// level, so 30 shallow levels is 2^30 re-parses of the same bodies. Legitimate
/// queries expand a handful of views; this only fires on the pathological shape.
/// The count is per statement because the context carrying it is: each statement
/// binds against a fresh [`ParamCtx`], and only a view body shares its parent's.
const MAX_VIEW_EXPANSIONS: usize = 1000;

/// Marks one view as "currently being expanded" for as long as it is alive.
///
/// [`Self::enter`] is where recursion is detected: a view already on the stack is
/// one whose body we are still binding, so expanding it again would never
/// terminate. Popping happens in `Drop` so a `?` anywhere inside the body — not
/// just the happy path — leaves the state as it found it. That matters within a
/// *single* statement: `FROM v, v x` expands `v` twice as siblings, and the
/// second must not see the first's entry.
///
/// The state travels on the statement's [`ParamCtx`], which the binder already
/// threads through every nested bind — including expression subqueries, which is
/// what a dependency graph over FROM-position relations cannot see.
struct ViewExpansionGuard {
    state: ViewExpansion,
}

impl ViewExpansionGuard {
    fn enter(params: &ParamCtx, view: &ViewDefinition) -> Result<Self, BindError> {
        let state = view_expansion(params);
        let key = (view.namespace.clone(), view.name.clone());
        {
            // Scoped so the borrow ends before this returns: the caller binds the
            // view body while the guard is alive, and that re-enters here.
            let (stack, expansions) = &mut *state.borrow_mut();
            if stack.contains(&key) {
                return Err(BindError::new(
                    sqlstate::INVALID_OBJECT_DEFINITION,
                    format!(
                        "infinite recursion detected in rules for relation \"{}\"",
                        view.name
                    ),
                ));
            }
            // PG's ERRCODE_STATEMENT_TOO_COMPLEX ("stack depth limit exceeded"),
            // as raised for over-nested set operations and SQL-function inlining.
            if stack.len() >= MAX_VIEW_EXPANSION_DEPTH {
                return Err(BindError::new(
                    sqlstate::STATEMENT_TOO_COMPLEX,
                    "views nested too deeply",
                ));
            }
            if *expansions >= MAX_VIEW_EXPANSIONS {
                return Err(BindError::new(
                    sqlstate::STATEMENT_TOO_COMPLEX,
                    "too many view expansions in one statement",
                ));
            }
            stack.push(key);
            *expansions += 1;
        }
        Ok(Self { state })
    }
}

impl Drop for ViewExpansionGuard {
    fn drop(&mut self) {
        self.state.borrow_mut().0.pop();
    }
}

/// Bind a stored view's query into a logical plan. The SQL text is re-parsed and
/// bound in a fresh scope (no outer CTEs, no outer `$n` parameters — a view body
/// references neither). A parse/shape failure is an internal invariant violation
/// (the text was validated at `CREATE VIEW`), reported as `XX000`.
///
/// Entering the recursion guard is part of expanding a view, so it happens here
/// rather than at the call site: a second expansion site added later cannot
/// forget it and reintroduce the unbounded recursion.
fn bind_view_query(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
    view: &ViewDefinition,
) -> Result<LogicalPlan, BindError> {
    let _guard = ViewExpansionGuard::enter(params, view)?;
    let stmts = crabgresql_parser::parse(&view.sql).map_err(|e| {
        BindError::new(
            sqlstate::INTERNAL_ERROR,
            format!(
                "could not parse stored definition of view \"{}\": {e}",
                view.name
            ),
        )
    })?;
    let query = match stmts.as_slice() {
        [ast::Statement::Query(query)] => query,
        _ => {
            return Err(BindError::new(
                sqlstate::INTERNAL_ERROR,
                format!(
                    "stored definition of view \"{}\" is not a single query",
                    view.name
                ),
            ));
        }
    };
    bind_query_scoped(
        engine,
        catalog,
        &param_ctx_view_body(params),
        query,
        &CteEnv::new(),
        &[],
    )
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
/// `siblings` are the FROM items already bound to this item's left, used only to
/// tell an implicitly-lateral table-function argument from a genuinely unknown
/// column (see [`bind_table_fn_args`]); it is empty for the leftmost item.
/// `outer_scope` carries the enclosing queries' relations, which a table
/// function's arguments — and only they, among FROM items — may reference.
#[allow(clippy::too_many_arguments)]
fn bind_from_item(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
    relation: &ast::TableFactor,
    ctes: &CteEnv,
    siblings: &[ScopeItem],
    outer_scope: &[OuterLevel],
    demand: &SystemDemand,
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
            bind_function_from_item(
                name,
                &fn_args.args,
                alias,
                *with_ordinality,
                catalog,
                params,
                siblings,
                outer_scope,
            )
        }
        // `unnest(array)` in FROM. Under the PostgreSQL dialect the parser gives
        // this its own factor rather than a `Table` with call arguments, but it
        // binds to the same `TableFn::Unnest` rowset.
        ast::TableFactor::UNNEST {
            alias,
            array_exprs,
            with_offset,
            with_offset_alias: _,
            with_ordinality,
        } => {
            // `WITH OFFSET` is BigQuery syntax, not PostgreSQL's. The parser only
            // fills `with_offset_alias` when the flag is set, so the flag alone
            // decides.
            if *with_offset {
                return Err(BindError::feature_not_supported(
                    "WITH OFFSET is not supported yet",
                ));
            }
            // TODO: unnest several arrays side by side, padding the short ones
            // to the longest as PG does — `resolve_unnest` binds only the
            // single-array form. The parser requires at least one argument, so
            // this is the `> 1` case.
            if array_exprs.len() > 1 {
                return Err(BindError::feature_not_supported(
                    "unnest with multiple arrays is not supported yet",
                ));
            }
            let (func, args) = bind_table_fn_args(
                "unnest",
                array_exprs,
                catalog,
                params,
                siblings,
                outer_scope,
            )?;
            bound_table_fn_item(func, args, "unnest", alias, *with_ordinality)
        }
        // `LATERAL f(…)` gets its own factor, and binds exactly like the `f(…)`
        // above: a function FROM item is implicitly LATERAL in PostgreSQL, so the
        // keyword adds nothing there and cannot decide anything here either.
        ast::TableFactor::Function {
            name,
            args,
            with_ordinality,
            alias,
            ..
        } => bind_function_from_item(
            name,
            args,
            alias,
            *with_ordinality,
            catalog,
            params,
            siblings,
            outer_scope,
        ),
        // A bare name may resolve to a CTE (which shadows a real table).
        ast::TableFactor::Table {
            name,
            only,
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
                    system_base: None,
                    declined_system: None,
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
                            typmod: c.typmod,
                            // The one place this is set: a base relation's own
                            // column, so a scope built over the FROM item can
                            // substitute a virtual one's expression.
                            generated: c.generated.clone(),
                        })
                        .collect();
                    // `information_schema` is the one namespace served as
                    // relations here that PostgreSQL serves as views, and a view
                    // exposes no system columns — so a reference there is the
                    // same 42703 it is upstream, not an OID no relation answers
                    // to.
                    let (wanted, declined_system) = match table.schema().namespace
                        == "information_schema"
                    {
                        true => (Arc::from(&[][..]), None),
                        false => supported_system_columns(
                            without_declared(demand.wants(&qualifier), &table.schema()).to_vec(),
                            &table,
                        ),
                    };
                    // A relation whose rows live in several places is read as a
                    // union scan. Bind it as a subplan wrapping an `Append` (raw
                    // relation columns), so the surrounding SELECT's
                    // projection/WHERE/ORDER BY — and any join or aggregate —
                    // apply through the existing subplan machinery.
                    let mut arm_columns = columns.clone();
                    arm_columns.extend(wanted.iter().map(|c| system_column(*c)));
                    // A monolithic relation is scanned directly and carries its
                    // own slots. Wrapping it in a one-armed `Append` for the
                    // sake of the emit would cost it the index path: an `Append`
                    // arm has no probe, so `WHERE pk = …` would read the whole
                    // relation.
                    let input = match scan_arms(engine, &table, *only, &wanted)? {
                        Some(arms) => {
                            JoinInput::Subplan(Box::new(LogicalPlan::Append(AppendPlan {
                                arms,
                                columns: arm_columns,
                            })))
                        }
                        None => JoinInput::Scan {
                            table: Arc::clone(&table),
                            system: system_emit(&wanted, &table.schema()),
                        },
                    };
                    apply_relation_alias_columns(&mut columns, alias, &table_subject(&qualifier))?;
                    // Appended *after* the alias list is applied: `t(a, b)` counts
                    // the relation's own columns, and PG's "table t has 2 columns
                    // available but 3 columns specified" must not learn about a
                    // system column.
                    let system_base = (!wanted.is_empty()).then(|| {
                        let base = columns.len();
                        columns.extend(wanted.iter().map(|c| system_column(*c)));
                        base
                    });
                    Ok(BoundFromItem {
                        qualifier,
                        columns,
                        input,
                        system_base,
                        declined_system,
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
                        // view but errors when it is used. `bind_view_query` marks
                        // the view as in-progress and errors if it already was —
                        // exact by construction, so a cycle closed through an
                        // expression subquery is caught too.
                        let inner = bind_view_query(engine, catalog, params, &view)?;
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
                            // A view is expanded into its body, and PostgreSQL
                            // exposes no system columns on one either.
                            system_base: None,
                            declined_system: None,
                        });
                    }
                    Err(not_found_as_written(e, cte_schema.as_deref(), &tname))
                }
            }
        }
        ast::TableFactor::Derived {
            lateral,
            subquery,
            alias,
            ..
        } => {
            // Everything below binds the body with an empty outer scope, which
            // is exactly what LATERAL is not. Left to fall through, `LATERAL
            // (SELECT o.x …)` would fail with a misleading `42703 column "o.x"
            // does not exist`; say what is actually missing instead.
            if *lateral {
                return Err(BindError::feature_not_supported(
                    "LATERAL is not supported yet",
                ));
            }
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
            // That empty scope is load-bearing: `subst_outer_join` descends into
            // this subplan at the *same* depth, which is only sound because no
            // `OuterColumnRef` can *escape* it — an escaping one needs a
            // non-empty `Scope::outer` at this bind. (References that stay
            // inside, correlating a nested subquery of the body to the body's
            // own relations, are both legal and common; `plan_has_outer_refs`
            // is depth-aware precisely so they do not read as escaping.)
            // Implementing LATERAL means binding this against the enclosing scope,
            // and then the `depth + 1` has to come back — this assert is what will
            // say so.
            debug_assert!(
                !plan_has_outer_refs(&inner),
                "no outer reference can escape a FROM subquery bound with an empty outer \
                 scope; supporting LATERAL means restoring the `depth + 1` in \
                 `subst_outer_join`"
            );
            let mut columns = output_columns_of(&inner)?;
            apply_alias_columns(&mut columns, &alias.columns, &table_subject(&qualifier))?;
            Ok(BoundFromItem {
                qualifier,
                columns,
                input: JoinInput::Subplan(Box::new(inner)),
                system_base: None,
                declined_system: None,
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

/// Drop from `wanted` every system column `schema` declares under that name.
///
/// The declared column is what the name resolves to, and adding a slot beside
/// it would make the reference ambiguous. PostgreSQL keeps the two apart by
/// refusing to create such a column at all; this build's own catalog relations
/// have several (`pg_replication_slots.xmin`), because upstream serves them as
/// views — which expose no system columns either.
fn without_declared(wanted: Vec<SysCol>, schema: &TableSchema) -> Arc<[SysCol]> {
    Arc::from(
        wanted
            .into_iter()
            .filter(|c| schema.column_index(c.name()).is_none())
            .collect::<Vec<_>>(),
    )
}

/// The system columns of `wanted` that `table`'s access method can actually
/// produce, plus what a reference to one of the others has to be refused with.
///
/// The columnar engines store column chunks, not row versions: there is no tid
/// that addresses a row and no MVCC header to read `xmin` off. Answering with a
/// placeholder would be worse than refusing — a client that identifies rows by
/// `ctid` would corrupt data with it.
///
/// But the refusal cannot be driven from `wanted`. That set comes from
/// [`SystemDemand`], a deliberate over-approximation over *rendered SQL*, so a
/// string literal or an `AS` alias spelling one of six short words lands in it —
/// and turning that into an error rejects `SELECT * FROM q WHERE label = 'xmin'`,
/// which PostgreSQL answers. So the unsupported slots are dropped here, leaving
/// the over-approximation harmless again, and the `0A000` is raised only where a
/// reference actually resolves to one.
///
/// `tableoid` is never dropped: every relation has an identity, whatever it
/// stores.
fn supported_system_columns(
    wanted: Vec<SysCol>,
    table: &Arc<dyn TableAm>,
) -> (Arc<[SysCol]>, Option<DeclinedSystem>) {
    if table.supports_system_columns() {
        return (Arc::from(wanted), None);
    }
    let schema = table.schema();
    (
        Arc::from(
            wanted
                .into_iter()
                .filter(|c| !c.needs_storage_support())
                .collect::<Vec<_>>(),
        ),
        Some(DeclinedSystem {
            access_method: schema.access_method.as_str(),
            relation: schema.name.clone(),
        }),
    )
}

/// A system column as a column of the row that carries it. The slots sit past
/// every declared column of the relation, so `*` — which expands the declared
/// ones — never reaches them, exactly as a negative `attnum` is skipped
/// upstream.
fn system_column(col: SysCol) -> OutputColumn {
    OutputColumn::new(col.name(), col.ty())
}

/// Which system columns each FROM item of one query block must carry.
///
/// A system column is a value *in* the row rather than an expression over it —
/// that is what makes an outer join's null-extended side report NULL and each
/// `Append` arm report itself — so every FROM item's width has to be settled
/// before any expression binds, while a reference to one is only resolved
/// afterwards. This walks the block's expressions first and records the
/// qualifiers that name each column.
///
/// Deliberately an over-approximation. A slot nobody references costs one
/// `Value` per row and is dropped by the projection above it, whereas a
/// *missed* reference is a 42703 on a query PostgreSQL answers.
#[derive(Default)]
pub(crate) struct SystemDemand {
    /// Per [`SysCol`] (indexed by its position in [`SysCol::ALL`]): whether it
    /// was named unqualified, in which case any relation in scope might be the
    /// one that owns the reference.
    bare: [bool; SysCol::ALL.len()],
    /// Per [`SysCol`]: the qualifiers naming it explicitly (`t.ctid`),
    /// normalized the way [`relation_qualifier`] normalizes the names they are
    /// matched against.
    qualified: [HashSet<String>; SysCol::ALL.len()],
}

impl SystemDemand {
    /// Whether `col` was named without a qualifier, so every relation in scope
    /// might be the one that owns the reference.
    ///
    /// Only the tests read this directly: binding goes through [`Self::wants`],
    /// which folds the bare and qualified cases together.
    #[cfg(test)]
    pub(crate) fn is_bare(&self, col: SysCol) -> bool {
        self.bare[SysCol::ALL.iter().position(|c| *c == col).expect("in ALL")]
    }

    /// The system columns the FROM item addressed by `qualifier` must carry, in
    /// row order.
    pub(crate) fn wants(&self, qualifier: &str) -> Vec<SysCol> {
        SysCol::ALL
            .iter()
            .enumerate()
            .filter(|(i, _)| self.bare[*i] || self.qualified[*i].contains(qualifier))
            .map(|(_, col)| *col)
            .collect()
    }

    /// Every system column named anywhere in the block, whatever the qualifier
    /// — what a write statement needs, since there the slot is appended per
    /// *target* rather than per named relation.
    fn all_named(&self) -> Vec<SysCol> {
        SysCol::ALL
            .iter()
            .enumerate()
            .filter(|(i, _)| self.bare[*i] || !self.qualified[*i].is_empty())
            .map(|(_, col)| *col)
            .collect()
    }

    /// The demand of one query block: everything its SELECT and ORDER BY name.
    ///
    /// The block is scanned as *rendered SQL* rather than by walking the
    /// parser's `Expr` tree. A reference can sit in any expression position —
    /// projection, WHERE, GROUP BY, HAVING, a join's ON, ORDER BY, or nested
    /// arbitrarily deep inside a correlated subquery — and a walk that forgot
    /// one of them would cost a 42703 on a query PostgreSQL answers. Rendering
    /// cannot miss a position. It costs one `to_string` per bound block against
    /// a bind that already walks the same tree many times, and its only failure
    /// mode is the harmless one: a string literal spelling one of the names buys
    /// an unreferenced slot.
    ///
    /// That harmlessness is a standing obligation on every consumer of this set.
    /// Nothing may *refuse* a statement because a column is in it — see
    /// [`supported_system_columns`], which drops what an access method cannot
    /// serve and leaves the error to whoever resolves a real reference.
    pub(crate) fn of_select(select: &ast::Select, order_by: &Option<ast::OrderBy>) -> Self {
        let mut demand = Self::of_order_by(order_by);
        demand.scan_rendered(&select.to_string());
        demand
    }

    /// The demand of a bare ORDER BY — all a `TABLE t` body can carry.
    fn of_order_by(order_by: &Option<ast::OrderBy>) -> Self {
        let mut demand = Self::default();
        if let Some(order_by) = order_by {
            demand.scan_rendered(&order_by.to_string());
        }
        demand
    }

    /// The system columns a write statement names anywhere it could read one:
    /// its WHERE, its RETURNING list, and (for UPDATE) its SET expressions.
    ///
    /// A write answers from the target the row actually lives in, so the
    /// qualifier is not consulted here; *which* relation a `tableoid` names is
    /// settled per target at execution.
    fn in_write(
        selection: &Option<ast::Expr>,
        returning: &Option<Vec<ast::SelectItem>>,
        assignments: &[ast::Assignment],
    ) -> Vec<SysCol> {
        let mut demand = Self::default();
        if let Some(selection) = selection {
            demand.scan_rendered(&selection.to_string());
        }
        for item in returning.iter().flatten() {
            demand.scan_rendered(&item.to_string());
        }
        for assignment in assignments {
            demand.scan_rendered(&assignment.to_string());
        }
        demand.all_named()
    }

    /// Find each system column's name in rendered SQL, taking the qualifier
    /// before it when it has one. Quoting is honored on both halves: the parser
    /// re-quotes an identifier that needs it, and `"ctid"` names the system
    /// column just as the bare spelling does.
    fn scan_rendered(&mut self, text: &str) {
        let lower = text.to_ascii_lowercase();
        let bytes = lower.as_bytes();
        let part_char = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
        for (i, col) in SysCol::ALL.iter().enumerate() {
            let name = col.name();
            for (at, _) in lower.match_indices(name) {
                let end = at + name.len();
                // A longer identifier that merely contains the word is not it.
                if bytes.get(end).is_some_and(|b| part_char(*b))
                    || at.checked_sub(1).is_some_and(|i| part_char(bytes[i]))
                {
                    continue;
                }
                match qualifier_before(text, &lower, at) {
                    Some(qualifier) => {
                        self.qualified[i].insert(qualifier);
                    }
                    None => self.bare[i] = true,
                }
            }
        }
    }
}

/// The `q` of a rendered `q.ctid`, or `None` when the reference is bare.
///
/// `at` is the offset of the column name in `lower`, possibly preceded by its
/// own quote. `text` is the same string with its original case: only an ASCII
/// case fold separates the two, so the offsets address the same bytes in both,
/// and a *quoted* qualifier must be read from `text` because quoting is what
/// preserves its case — the same rule [`normalize_ident`] applies.
///
/// A schema-qualified `s.t.tableoid` renders with both parts; the relation is
/// addressed by `t`, the part nearest the column.
fn qualifier_before(text: &str, lower: &str, at: usize) -> Option<String> {
    let bytes = lower.as_bytes();
    // Step back over the column's opening quote, then the dot that joins them.
    let mut end = at;
    if end > 0 && bytes[end - 1] == b'"' {
        end -= 1;
    }
    if end == 0 || bytes[end - 1] != b'.' {
        return None;
    }
    end -= 1;
    if end > 0 && bytes[end - 1] == b'"' {
        let close = end - 1;
        let open = lower[..close].rfind('"')?;
        return Some(text[open + 1..close].to_string());
    }
    let start = lower[..end]
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
        .map_or(0, |i| i + 1);
    (start < end).then(|| lower[start..end].to_string())
}

/// Bind all comma-separated FROM groups. Each group owns its JOIN/ON namespace;
/// only after its explicit join chain is complete is it combined with prior
/// groups by a cross join. This makes `a, b JOIN c ON a.x = c.x` reject `a` in
/// ON, matching SQL's join nesting rules.
#[allow(clippy::too_many_arguments)]
fn bind_from_clause(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
    from: &[ast::TableWithJoins],
    ctes: &CteEnv,
    outer_scope: &[OuterLevel],
    windows: &NamedWindows,
    demand: &SystemDemand,
) -> Result<BoundFrom, BindError> {
    let mut combined: Option<JoinExpr> = None;
    let mut relations: Vec<ScopeItem> = Vec::new();
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
        } = bind_table_with_joins(
            engine,
            catalog,
            params,
            table,
            ctes,
            outer_scope,
            windows,
            &relations,
            demand,
        )?;
        for rel in &group_relations {
            ensure_unique_qualifier(&mut seen, &rel.qualifier)?;
        }
        let group_width = group_source.width();
        // Fold this group's view into the running one, shifting its base-0
        // indices to their global position (`width`).
        match (&mut visible, group_visible) {
            (Some(acc), Some(gv)) => {
                acc.extend(gv.into_iter().map(|c| shift_visible(c, width)));
            }
            (Some(acc), None) => acc.extend(default_visible(&group_relations, width, catalog)?),
            (None, Some(gv)) => {
                let mut acc = default_visible(&relations, 0, catalog)?;
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
#[allow(clippy::too_many_arguments)]
fn bind_table_with_joins(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
    table: &ast::TableWithJoins,
    ctes: &CteEnv,
    outer_scope: &[OuterLevel],
    windows: &NamedWindows,
    siblings: &[ScopeItem],
    demand: &SystemDemand,
) -> Result<BoundFrom, BindError> {
    let mut bound = bind_from_item(
        engine,
        catalog,
        params,
        &table.relation,
        ctes,
        siblings,
        outer_scope,
        demand,
    )?
    .into_bound_from();
    let mut seen: HashSet<String> = bound
        .relations
        .iter()
        .map(|rel| rel.qualifier.clone())
        .collect();
    // Width of the accumulated left side in the group's combined row, tracked
    // incrementally (each Input is O(1), so no repeated tree walks).
    let mut left_width = bound.source.width();
    // The merged-column view over the accumulated left side, indexed into this
    // group's combined row (base 0). Materialized only once a USING/NATURAL join
    // needs it; an all-`ON`/`CROSS` chain leaves it `None` and allocates nothing.
    let mut visible: Option<Vec<VisibleColumn>> = None;
    // Everything already joined to this item's left — what an implicitly-lateral
    // table-function argument could legally reference. Grown in place as the
    // chain is bound, so an N-way join does not re-clone the accumulated columns
    // N times.
    let mut left_relations: Vec<ScopeItem> = Vec::new();
    if !table.joins.is_empty() {
        left_relations.extend(siblings.iter().cloned());
        left_relations.extend(bound.relations.iter().cloned());
    }

    for join in &table.joins {
        if join.global {
            return Err(BindError::feature_not_supported(
                "GLOBAL JOIN is not supported yet",
            ));
        }
        let right = bind_from_item(
            engine,
            catalog,
            params,
            &join.relation,
            ctes,
            &left_relations,
            outer_scope,
            demand,
        )?
        .into_bound_from();
        left_relations.extend(right.relations.iter().cloned());
        let right_qualifier = &right.relations[0].qualifier;
        ensure_unique_qualifier(&mut seen, right_qualifier)?;
        let right_width = right.source.width();

        let (kind, predicate) = match join_kind_and_constraint(&join.join_operator)? {
            JoinBinding::Cross => {
                if let Some(v) = &mut visible {
                    v.extend(default_visible(&right.relations, left_width, catalog)?);
                }
                (JoinKind::Cross, None)
            }
            JoinBinding::On(kind, on) => {
                let mut on_relations = bound.relations.clone();
                on_relations.extend(right.relations.clone());
                // A prior USING/NATURAL join makes the merged view govern
                // unqualified names inside this ON clause too.
                let on_visible = match visible.as_ref() {
                    Some(v) => {
                        let mut ov = v.clone();
                        ov.extend(default_visible(&right.relations, left_width, catalog)?);
                        Some(ov)
                    }
                    None => None,
                };
                let scope =
                    Scope::relations_with_visible(on_relations, on_visible, catalog, params)
                        .with_subqueries(engine, ctes)
                        .with_outer(outer_scope.to_vec())
                        .with_named_windows(windows);
                let binding = bind_expr(on, &scope)?;
                if let Binding::Typed(expr) = &binding {
                    reject_agg_or_window(expr, "JOIN conditions")?;
                }
                if let Some(v) = &mut visible {
                    v.extend(default_visible(&right.relations, left_width, catalog)?);
                }
                (kind, Some(to_bool_operand(binding, "JOIN/ON", on.span())?))
            }
            JoinBinding::Using(kind, names) => {
                let left_view = match visible.take() {
                    Some(view) => view,
                    None => default_visible(&bound.relations, 0, catalog)?,
                };
                let (predicate, new_visible) =
                    build_merged_join(kind, &names, &left_view, &right, left_width, catalog)?;
                visible = Some(new_visible);
                (kind, predicate)
            }
            JoinBinding::Natural(kind) => {
                let left_view = match visible.take() {
                    Some(view) => view,
                    None => default_visible(&bound.relations, 0, catalog)?,
                };
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
pub(crate) fn default_visible(
    relations: &[ScopeItem],
    base: usize,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<Vec<VisibleColumn>, BindError> {
    let mut out = Vec::new();
    let mut offset = base;
    for rel in relations {
        // The merged view drives unqualified resolution and `*`, and a system
        // column belongs to neither: it never takes part in `USING`, and `*`
        // does not expand it.
        for (local, col) in rel.declared().iter().enumerate() {
            out.push(VisibleColumn {
                name: col.name.clone(),
                // Through the same helper name resolution uses, so a virtual
                // generated column substitutes its expression here too — this
                // view *shadows* that path, and a `ColumnRef` would read the
                // slot nothing is stored in. It also carries the column's
                // declared collation, as the shadowed path does.
                expr: crate::expr::column_value(
                    &rel.columns,
                    local,
                    offset,
                    &rel.qualifier,
                    catalog,
                )?,
            });
        }
        offset += rel.columns.len();
    }
    Ok(out)
}

/// The columns a `NATURAL` join equates: every name present in both the
/// accumulated left view and the right input, in left-to-right order, once
/// each. An empty result means no common columns — a plain cross product.
fn natural_join_names(left: &[VisibleColumn], right: &BoundFrom) -> Vec<String> {
    let right_names: HashSet<&str> = right
        .relations
        .iter()
        .flat_map(|rel| rel.declared().iter())
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
    let right_visible = default_visible(&right.relations, left_width, catalog)?;
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
            (Span::empty(), Span::empty()),
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
            let binding = bind_expr(expr, &scope)?;
            if let Binding::Typed(expr) = &binding {
                reject_agg_or_window(expr, "VALUES")?;
            }
            columns_of_bindings[col].push(binding);
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

    // TODO: ORDER BY expressions over a standalone VALUES list, which PG answers
    // (`VALUES (1),(2) ORDER BY column1 * 2`). There is no projection tuple to
    // append hidden sort columns to here — the cells evaluate against an empty
    // row — so only ordinals and `columnN` names resolve and an expression is
    // `0A000`.
    let sort = bind_order_by(order_by, &columns, &scope, &mut Vec::new(), false)?;
    Ok(LogicalPlan::Values(ValuesPlan {
        columns,
        rows,
        predicate: None,
        sort,
        distinct: None,
    }))
}

/// The output columns a query plan produces (for CTE/derived-table schemas,
/// and the extended protocol's `Describe`, which needs a statement's
/// `RowDescription` without executing it). A data-modifying plan without a
/// `RETURNING` clause has no result row shape and returns an error the caller
/// treats as "NoData".
pub fn output_columns_of(plan: &LogicalPlan) -> Result<Vec<OutputColumn>, BindError> {
    match plan {
        LogicalPlan::Values(ValuesPlan { columns, .. })
        | LogicalPlan::Query(QueryPlan { columns, .. })
        | LogicalPlan::Append(AppendPlan { columns, .. })
        | LogicalPlan::SetOp(SetOpPlan { columns, .. })
        | LogicalPlan::Subquery(SubqueryPlan { columns, .. })
        | LogicalPlan::TableFunction(TableFunctionPlan { columns, .. })
        | LogicalPlan::Aggregate(AggregatePlan { columns, .. })
        | LogicalPlan::Join(JoinPlan { columns, .. }) => Ok(columns.clone()),
        // LIMIT/OFFSET is a transparent wrapper: it exposes its source's columns.
        LogicalPlan::Limit(LimitPlan { source, .. }) => output_columns_of(source),
        // A window step is always wrapped in the `Subquery` that carries the
        // query's real target list, so this is only ever reached through one.
        // Its own row has no user-visible names; report the source's.
        LogicalPlan::Window(WindowPlan { source, .. }) => output_columns_of(source),
        LogicalPlan::Insert(InsertPlan { returning, .. })
        | LogicalPlan::Update(UpdatePlan { returning, .. })
        | LogicalPlan::Delete(DeletePlan { returning, .. }) => match returning {
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
        LogicalPlan::Query(QueryPlan {
            table,
            system,
            projections,
            predicate,
            ..
        }) if !has_srf(&projections) => LogicalPlan::Query(QueryPlan {
            table,
            // The projection becomes a constant, but the predicate is kept — and
            // it may read a system slot, whose `ColumnRef` indexes past the
            // relation's declared columns. Dropping the slots here would make it
            // index past the end of the row.
            system,
            columns: one_col(),
            projections: one_row(),
            predicate,
            sort: Vec::new(),
            distinct: None,
        }),
        LogicalPlan::Subquery(SubqueryPlan {
            source,
            projections,
            predicate,
            ..
        }) if !has_srf(&projections) => LogicalPlan::Subquery(SubqueryPlan {
            source,
            columns: one_col(),
            projections: one_row(),
            predicate,
            sort: Vec::new(),
            distinct: None,
        }),
        LogicalPlan::Join(JoinPlan {
            source,
            projections,
            predicate,
            ..
        }) if !has_srf(&projections) => LogicalPlan::Join(JoinPlan {
            source,
            columns: one_col(),
            projections: one_row(),
            predicate,
            sort: Vec::new(),
            distinct: None,
        }),
        LogicalPlan::TableFunction(TableFunctionPlan {
            func,
            args,
            ordinality,
            projections,
            predicate,
            ..
        }) if !has_srf(&projections) => LogicalPlan::TableFunction(TableFunctionPlan {
            func,
            args,
            ordinality,
            columns: one_col(),
            projections: one_row(),
            predicate,
            sort: Vec::new(),
            distinct: None,
        }),
        LogicalPlan::Aggregate(AggregatePlan {
            input,
            predicate,
            group_exprs,
            aggregates,
            having,
            projections,
            ..
        }) if !has_srf(&projections) => LogicalPlan::Aggregate(AggregatePlan {
            input,
            predicate,
            group_exprs,
            aggregates,
            having,
            columns: one_col(),
            projections: one_row(),
            sort: Vec::new(),
            distinct: None,
        }),
        LogicalPlan::Values(ValuesPlan {
            rows, predicate, ..
        }) => LogicalPlan::Values(ValuesPlan {
            columns: one_col(),
            rows: rows.into_iter().map(|_| one_row()).collect(),
            predicate,
            sort: Vec::new(),
            distinct: None,
        }),
        LogicalPlan::Limit(LimitPlan {
            source,
            limit,
            offset,
        }) => LogicalPlan::Limit(LimitPlan {
            source: Box::new(strip_to_existence(*source)),
            limit,
            offset,
        }),
        // A set-returning projection (guards above fell through) or a DML body in
        // WITH: leave as-is; the executor's first-row check is still correct.
        other => other,
    }
}

/// Bind a set-returning function FROM item, whichever factor the parser gave it:
/// `f(…)`, `LATERAL f(…)`, and (via its caller) `unnest(…)` all end up here.
#[allow(clippy::too_many_arguments)]
fn bind_function_from_item(
    name: &ast::ObjectName,
    args: &[ast::FunctionArg],
    alias: &Option<ast::TableAlias>,
    with_ordinality: bool,
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
    siblings: &[ScopeItem],
    outer_scope: &[OuterLevel],
) -> Result<BoundFromItem, BindError> {
    // A *function* name, not a relation name: qualifiers are resolved by the
    // same rule a scalar call uses, not rejected as they are for a relation.
    let fname = crate::functions::function_name(name)
        .ok_or_else(|| BindError::syntax(format!("invalid function name: {name}")))?;
    let arg_exprs = positional_arg_exprs(args)?;
    let (func, bound_args) =
        bind_table_fn_args(&fname, &arg_exprs, catalog, params, siblings, outer_scope)?;
    bound_table_fn_item(func, bound_args, &fname, alias, with_ordinality)
}

/// Bind a table function's arguments for a FROM item.
///
/// Two scopes can answer them, and PostgreSQL consults them in this order:
///
/// 1. The FROM items already bound to this one's left. A function FROM item is
///    implicitly LATERAL there, so `FROM t, unnest(t.arr)` resolves `t` — and
///    that is the one thing this build cannot do. It is reported as the missing
///    feature it is rather than resolved somewhere else: binding such a name
///    outward instead would silently answer a *different* query.
/// 2. The enclosing queries. `SELECT (SELECT … FROM unnest(t.arr)) FROM t` is an
///    ordinary correlated reference, legal in PostgreSQL without LATERAL, and it
///    binds to an [`BoundExpr::OuterColumnRef`] filled per outer row by
///    [`substitute_outer`].
///
/// The sibling probe leads with "does this argument need more than constants?"
/// because a constant binds in *every* scope, siblings included — asking the
/// siblings alone would report `FROM t, generate_series(1, 5)` as a lateral
/// reference.
///
/// The probe asks whether the *arguments* resolve against the siblings, not
/// whether the whole call does, so the two gaps stay distinct: `unnest(t.arr)`
/// resolves completely and the only thing missing is LATERAL itself, while
/// `unnest(t.name)` on a text column reports the `42883` PostgreSQL reports. A
/// name that resolves in neither scope keeps its own 42703/42P01, also matching
/// PostgreSQL.
fn bind_table_fn_args(
    name: &str,
    arg_exprs: &[ast::Expr],
    catalog: &Arc<dyn TypeCatalog>,
    params: &ParamCtx,
    siblings: &[ScopeItem],
    outer_scope: &[OuterLevel],
) -> Result<(TableFn, Vec<BoundExpr>), BindError> {
    if !siblings.is_empty() {
        let bare = Scope::empty(catalog, params);
        let lateral = Scope::relations(siblings.to_vec(), catalog, params);
        let needs_a_sibling = arg_exprs
            .iter()
            .any(|e| bind_expr(e, &bare).is_err() && bind_expr(e, &lateral).is_ok());
        if needs_a_sibling {
            return Err(match bind_table_fn_call(name, arg_exprs, &lateral) {
                Ok(_) => BindError::feature_not_supported("LATERAL is not supported yet"),
                Err(call_error) => call_error,
            });
        }
    }
    let scope = Scope::empty(catalog, params).with_outer(outer_scope.to_vec());
    bind_table_fn_call(name, arg_exprs, &scope)
}

/// Assemble the FROM item for an already-resolved table function. `default_name`
/// is the function's own name, used as the qualifier when there is no alias.
///
/// PG names a *scalar* function's single output column after a bare alias, so
/// `generate_series(1, 10) i` exposes a column `i` and not `generate_series`. An
/// explicit alias column list (`s(g)`) still wins over the bare alias, and a
/// composite-returning function keeps its row type's column names.
///
/// The `WITH ORDINALITY` column joins that rowset, so an alias column list
/// renames it like any other (`a(relid, depth)`) while a bare alias still names
/// the function's own column alone: PG's `generate_series(1, 3) WITH ORDINALITY
/// t` exposes `t` and `ordinality`.
fn bound_table_fn_item(
    func: TableFn,
    args: Vec<BoundExpr>,
    default_name: &str,
    alias: &Option<ast::TableAlias>,
    ordinality: bool,
) -> Result<BoundFromItem, BindError> {
    let qualifier = relation_qualifier(alias, default_name);
    let mut columns = func.columns();
    if ordinality {
        columns.push(OutputColumn::new("ordinality", PgType::Int8));
    }
    if func.returns_scalar() && alias.is_some() {
        debug_assert_eq!(
            columns.len(),
            1 + usize::from(ordinality),
            "a scalar table function must expose its one column, plus the ordinal"
        );
        if let Some(col) = columns.first_mut() {
            // Same normalization `relation_qualifier` just applied, so the
            // column and its qualifier can never disagree on spelling.
            col.name.clone_from(&qualifier);
        }
    }
    apply_relation_alias_columns(&mut columns, alias, &table_subject(&qualifier))?;
    Ok(BoundFromItem {
        qualifier,
        columns,
        input: JoinInput::TableFunction {
            func,
            args,
            ordinality,
        },
        system_base: None,
        declined_system: None,
    })
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
/// the rest keep their names — but a longer list is an error. `subject` is the
/// relation phrasing PG uses in the column-count error (see [`table_subject`] /
/// [`with_query_subject`]).
///
/// TODO: an alias column-definition list carrying types (`f(a int, b text)`),
/// which PG requires to give a record-returning function its row shape.
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
            let name = crate::expr::type_label(ty, scope.catalog().as_ref());
            // A type that has equality but no ordering is not a gap in this
            // build — PostgreSQL has no ordering operator for it either (`xid`
            // is the only one), so report what PG reports rather than claiming
            // the feature is merely missing.
            if crate::expr::has_equality(ty, scope.catalog().as_ref()) {
                return Err(BindError::new(
                    sqlstate::UNDEFINED_FUNCTION,
                    format!("could not identify an ordering operator for type {name}"),
                )
                .with_hint(Some(
                    "Use an explicit ordering operator or modify the query.".to_string(),
                )));
            }
            return Err(BindError::feature_not_supported(format!(
                "ORDER BY on type {name} is not supported yet"
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
        && let Some(ordinal) = crate::expr::literal_int(n).and_then(|v| usize::try_from(v).ok())
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
    // at bind time. Unlike `bind_order_by` this needs only equality, so `xid`
    // is admitted here and rejected there.
    for key in &keys {
        if !crate::expr::has_equality(key.ty, scope.catalog().as_ref()) {
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
    // GROUP BY and HAVING are handled by the aggregation binder, so only the
    // grouping-set extensions (ROLLUP / CUBE / GROUPING SETS / GROUP BY ALL)
    // reach this check.
    //
    // TODO: GROUP BY ROLLUP, CUBE and GROUPING SETS.
    let grouping_sets_unsupported = match &select.group_by {
        ast::GroupByExpr::Expressions(_, modifiers) => !modifiers.is_empty(),
        ast::GroupByExpr::All(_) => true,
    };
    // DISTINCT / DISTINCT ON are handled by `bind_distinct` in the select body.
    let unsupported: Option<&str> = if grouping_sets_unsupported {
        Some("GROUP BY ROLLUP/CUBE/GROUPING SETS")
    // `WINDOW w AS (…)` is handled by `named_windows`, which the scope carries so
    // `OVER w` resolves. QUALIFY is not PostgreSQL syntax at all.
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
        .with_outer(outer_scope.to_vec())
        .with_named_windows(&named_windows(select)?);
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
            typmod: crate::expr::projection_typmod(expr, &bound, &scope),
            generated: None,
        });
        row.push(bound);
    }
    let predicate = bind_where(&select.selection, &scope)?;
    if let Some(predicate) = &predicate {
        reject_agg_or_window(predicate, "WHERE")?;
    }
    // The empty scope means any hidden ORDER BY expression is column-free
    // (`ORDER BY random()`), so appending it to `row` stays safe against the
    // Values node's empty-row evaluation. Hidden columns are never SRFs, so the
    // SRF check below is unaffected.
    let mut sort = bind_order_by(order_by, &columns, &scope, &mut row, true)?;
    let mut distinct = bind_distinct(&select.distinct, &columns, &scope, &mut row, &sort)?;
    // A FROM-less aggregate (`SELECT count(*)`, or a HAVING/GROUP BY) runs over
    // the single virtual row. `count(*)` returns 1, `WHERE false` makes it 0.
    let aggregation = bind_aggregation(select, &scope, &columns, &mut row, &predicate)?;
    // The pre-window row is the aggregate's output row, or — with no FROM and no
    // aggregation — no row at all, so `SELECT row_number() OVER ()` reads nothing
    // and the chain's slots are the whole row.
    let input_width = match &aggregation {
        Some(agg) => agg.group_exprs.len() + agg.aggregates.len(),
        None => 0,
    };
    let windows = extract_windows(&mut row, input_width)?;
    if let Some(agg) = aggregation {
        // As in `bind_select`: with a window chain above, the aggregate keeps
        // only an identity projection over its `[keys…, aggregates…]` row and
        // the SELECT's own tail rides the wrapping `Subquery`.
        let identity = (!windows.is_empty()).then(|| aggregate_identity_projection(&agg));
        let block = LogicalPlan::Aggregate(AggregatePlan {
            input: AggInput::SingleRow,
            predicate,
            group_exprs: agg.group_exprs,
            aggregates: agg.aggregates,
            having: agg.having,
            columns: match &identity {
                Some(identity) => internal_columns(identity),
                None => columns.clone(),
            },
            projections: identity.unwrap_or_else(|| std::mem::take(&mut row)),
            sort: if windows.is_empty() {
                std::mem::take(&mut sort)
            } else {
                Vec::new()
            },
            distinct: if windows.is_empty() {
                distinct.take()
            } else {
                None
            },
        });
        return Ok(if windows.is_empty() {
            block
        } else {
            finish_windowed_select(block, windows, input_width, columns, row, sort, distinct)
        });
    }
    // A FROM-less window (`SELECT row_number() OVER ()`) runs over the same
    // single virtual row, through an empty-width `Values` the chain widens.
    if !windows.is_empty() {
        let source = LogicalPlan::Values(ValuesPlan {
            columns: Vec::new(),
            rows: vec![vec![]],
            predicate,
            sort: Vec::new(),
            distinct: None,
        });
        return Ok(finish_windowed_select(
            source,
            windows,
            input_width,
            columns,
            row,
            sort,
            distinct,
        ));
    }
    // A FROM-less SELECT with a set-returning function in the target list
    // (`SELECT generate_series(1, 5)`) expands into rows, so it cannot be a
    // constant `Values`. Run the projection pipeline over a single dummy input
    // row: `ProjectSet` then expands each SRF. Mirrors PG's Result + ProjectSet.
    if row.iter().any(BoundExpr::is_srf) {
        let source = LogicalPlan::Values(ValuesPlan {
            columns: Vec::new(),
            rows: vec![vec![]],
            predicate: None,
            sort: Vec::new(),
            distinct: None,
        });
        return Ok(LogicalPlan::Subquery(SubqueryPlan {
            source: Box::new(source),
            columns,
            projections: row,
            predicate,
            sort,
            distinct,
        }));
    }
    Ok(LogicalPlan::Values(ValuesPlan {
        columns,
        rows: vec![row],
        predicate,
        sort,
        distinct,
    }))
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
    /// One entry per distinct `OVER` clause, in order of first appearance; empty
    /// for a query with no window calls. `projections` have been rewritten to
    /// reference the window chain's output row.
    windows: Vec<WindowGroup>,
    /// The width of the row the window chain sits on — the aggregate's output
    /// row when the query aggregates, else the raw FROM row. Meaningless (and
    /// zero) when `windows` is empty.
    input_width: usize,
}

/// The grouping/aggregation part of a bound SELECT, extracted from its
/// expressions. Feeds a [`LogicalPlan::Aggregate`].
struct Aggregation {
    group_exprs: Vec<BoundExpr>,
    aggregates: Vec<BoundAggregate>,
    having: Option<BoundExpr>,
}

/// Bind a target list (a SELECT projection or a `RETURNING` list) against
/// `scope`, expanding `*`/`t.*` and naming each output column by its alias or
/// derived name. Shared by SELECT and RETURNING, which have identical shape.
fn bind_target_list(items: &[ast::SelectItem], scope: &Scope) -> Result<Returning, BindError> {
    let mut columns = Vec::new();
    let mut projections = Vec::new();
    for item in items {
        match classify_select_item(item)? {
            SelectField::Wildcard => {
                for (col, expr) in scope.expand_wildcard()? {
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
                    typmod: crate::expr::projection_typmod(expr, &bound, scope),
                    generated: None,
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
        reject_agg_or_window(projection, "RETURNING")?;
        if projection.contains_srf() {
            return Err(BindError::new(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "set-returning functions are not allowed in RETURNING",
            ));
        }
    }
    Ok(Some(bound))
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
    let Returning {
        columns,
        mut projections,
    } = bind_target_list(&select.projection, scope)?;

    let predicate = bind_where(&select.selection, scope)?;
    if let Some(predicate) = &predicate {
        reject_agg_or_window(predicate, "WHERE")?;
    }
    let sort = bind_order_by(order_by, &columns, scope, &mut projections, true)?;
    // DISTINCT is resolved before aggregation so any hidden `DISTINCT ON` column
    // appended to `projections` is aggregate-rewritten alongside the ORDER BY
    // ones below.
    let distinct = bind_distinct(&select.distinct, &columns, scope, &mut projections, &sort)?;
    // ORDER BY expressions were appended to `projections` as hidden columns, so
    // aggregate detection/rewrite below covers `ORDER BY count(*)` too.
    let aggregation = bind_aggregation(select, scope, &columns, &mut projections, &predicate)?;
    // Windows are extracted last, after aggregation has rebased every projection
    // onto the aggregate's output row. That ordering is what makes
    // `sum(sum(x)) OVER (PARTITION BY y) … GROUP BY y` bind, and it also sweeps
    // up a window in a hidden ORDER BY / DISTINCT ON column for free.
    let input_width = match &aggregation {
        Some(agg) => agg.group_exprs.len() + agg.aggregates.len(),
        None => scope.width(),
    };
    let windows = extract_windows(&mut projections, input_width)?;
    Ok(SelectBody {
        columns,
        projections,
        predicate,
        sort,
        distinct,
        aggregation,
        windows,
        input_width,
    })
}

/// Wrap `source` — the query block the window chain sits on — in one
/// [`LogicalPlan::Window`] per spec, then in the [`LogicalPlan::Subquery`] that
/// carries the SELECT's own projection, ORDER BY and DISTINCT.
///
/// `source` must already have been built with identity projections and no
/// sort/distinct, so the chain reads the raw pre-window row.
fn finish_windowed_select(
    source: LogicalPlan,
    windows: Vec<WindowGroup>,
    input_width: usize,
    columns: Vec<OutputColumn>,
    projections: Vec<BoundExpr>,
    sort: Vec<SortKey>,
    distinct: Option<Vec<DistinctKey>>,
) -> LogicalPlan {
    let output_width = input_width + windows.iter().map(|g| g.funcs.len()).sum::<usize>();
    let order = chain_order(&windows);
    let mut groups: Vec<Option<WindowGroup>> = windows.into_iter().map(Some).collect();
    let mut node = source;
    for index in order {
        let group = groups[index].take().expect("each group used once");
        node = LogicalPlan::Window(WindowPlan {
            source: Box::new(node),
            spec: group.spec,
            funcs: group.funcs,
            input_width,
            output_width,
        });
    }
    LogicalPlan::Subquery(SubqueryPlan {
        source: Box::new(node),
        columns,
        projections,
        // WHERE was applied below the chain, on `source`.
        predicate: None,
        sort,
        distinct,
    })
}

/// The synthetic output columns of a plan built only to feed a window chain.
/// Never surfaced: `output_columns_of` is asked about the top node, which is the
/// wrapping `Subquery`.
fn internal_columns(projections: &[BoundExpr]) -> Vec<OutputColumn> {
    projections
        .iter()
        .map(|e| OutputColumn::new("?column?".to_string(), e.ty()))
        .collect()
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
    // An aggregate or window in WHERE was already rejected by the caller, which
    // guards the predicate as soon as it is bound so the leftmost offender is
    // the one reported.
    debug_assert!(
        !predicate
            .as_ref()
            .is_some_and(|p| p.contains_aggregate() || p.contains_window()),
        "WHERE must be guarded by `reject_agg_or_window` before aggregation binds"
    );
    let _ = predicate;

    let group_exprs = bind_group_by(&select.group_by, scope, columns, projections)?;
    let having = select
        .having
        .as_ref()
        .map(|h| bind_expr(h, scope).and_then(|b| to_bool_operand(b, "HAVING", h.span())))
        .transpose()?;
    // HAVING filters the grouped rows, which windows are only computed *over*,
    // so a window call here has no value yet — as in WHERE and GROUP BY.
    if let Some(having) = &having {
        reject_window(having, "HAVING")?;
    }

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

/// One link of a query's window chain: a spec and the calls computed under it.
/// Produced by [`extract_windows`] in the order the specs first appear in the
/// query; [`chain_order`] decides the order they are evaluated in.
struct WindowGroup {
    spec: BoundWindowSpec,
    funcs: Vec<BoundWindowFunc>,
}

/// Extract every window call in `projections` into a per-spec group, rewriting
/// each marker to a `ColumnRef` into the window chain's output row.
///
/// Runs *after* aggregate extraction, so by now `projections` (including the
/// hidden ORDER BY / DISTINCT ON columns appended earlier) index whatever row
/// the window chain will sit on: the raw FROM row, or the aggregate's
/// `[group keys…, aggregates…]` row. `input_width` is that row's width.
///
/// Returns an empty vector — leaving `projections` untouched — for a query with
/// no window calls.
fn extract_windows(
    projections: &mut [BoundExpr],
    input_width: usize,
) -> Result<Vec<WindowGroup>, BindError> {
    if !projections.iter().any(BoundExpr::contains_window) {
        return Ok(Vec::new());
    }
    let mut groups: Vec<WindowGroup> = Vec::new();
    for proj in projections.iter_mut() {
        // Move the projection out (the rewrite consumes it and rebuilds a fresh
        // tree) rather than cloning the whole expression only to drop it.
        let taken = std::mem::replace(
            proj,
            BoundExpr::Const {
                value: Value::Null,
                ty: PgType::Bool,
            },
        );
        *proj = rewrite_over_window(taken, input_width, &mut groups)?;
    }
    Ok(groups)
}

/// One key of a spec's sort key list: the expression, plus the direction and
/// NULL placement that decide whether two specs can share a sort.
#[derive(PartialEq)]
struct ChainKey<'a> {
    expr: &'a BoundExpr,
    asc: bool,
    nulls_first: bool,
}

/// The keys a spec's step sorts by: its partition keys, then its `ORDER BY`
/// keys. Partition keys sort ascending with NULLs last, which is what
/// `WindowAgg` does — so this is exactly the sort the executor performs.
fn sort_key_list(spec: &BoundWindowSpec) -> Vec<ChainKey<'_>> {
    spec.partition_by
        .iter()
        .map(|expr| ChainKey {
            expr,
            asc: true,
            nulls_first: false,
        })
        .chain(spec.order_by.iter().map(|key| ChainKey {
            expr: &key.expr,
            asc: key.asc,
            nulls_first: key.nulls_first,
        }))
        .collect()
}

/// Whether `short` is a *proper* prefix of `long`, i.e. a step sorted by `long`
/// leaves the rows already ordered for `short`.
fn is_proper_prefix(short: &[ChainKey<'_>], long: &[ChainKey<'_>]) -> bool {
    short.len() < long.len() && short.iter().zip(long).all(|(a, b)| a == b)
}

/// The order the chain evaluates `groups` in, as indices into it, deepest first.
///
/// PG orders the chain so consecutive steps can **share a sort**: a step sorted
/// by `(a, b, c)` leaves the rows already ordered for `(a, b)`, so the shorter
/// spec follows the longer one and needs no re-sort. The order is observable,
/// not just a plan shape — the last step evaluated leaves its sort in place, and
/// that is the order a window query returns rows in when it has no `ORDER BY` of
/// its own.
///
/// Fitted from `EXPLAIN (costs off)` against PG 18.4, whose `Window: wN`
/// numbering states the evaluation order directly (w1 is the bottom): every
/// permutation of three specs with one prefix pair, of three independent
/// singletons, and of a prefix *tree* (`(a)`, `(a,b)`, `(a,c)`), plus two
/// independent prefix chains, the non-comparable case (`(a,b)` vs `(a,c)`, which
/// do *not* share a sort) and the direction-mismatch case (`(a)` vs
/// `(a DESC, b)`, likewise).
///
/// The rule those 24 observations agree on:
///
/// 1. Take the specs in reverse order of first appearance.
/// 2. Build chains longest-first; a spec joins the chain whose current shortest
///    member it is a proper prefix of, preferring the chain that sits latest in
///    the reversed order when several would do.
/// 3. Emit the chains ordered by their latest member's position in the reversed
///    order, each chain longest key list first.
///
/// A spec whose `OVER` clause holds a subquery never chains, because `Subplan`
/// compares unequal even to itself — conservative, since the only cost of not
/// chaining is a sort that could have been reused.
fn chain_order(groups: &[WindowGroup]) -> Vec<usize> {
    let n = groups.len();
    let keys: Vec<Vec<ChainKey<'_>>> = groups.iter().map(|g| sort_key_list(&g.spec)).collect();
    // Position in reverse order of appearance: the last-written spec is 0.
    let rev_pos: Vec<usize> = (0..n).map(|g| n - 1 - g).collect();

    // Longest first, and among equals the one latest in the reversed order —
    // that is the chain a shorter spec attaches to when several would fit.
    let mut by_length: Vec<usize> = (0..n).collect();
    by_length.sort_by(|&a, &b| {
        keys[b]
            .len()
            .cmp(&keys[a].len())
            .then(rev_pos[b].cmp(&rev_pos[a]))
    });

    let mut chains: Vec<Vec<usize>> = Vec::new();
    for &group in &by_length {
        // Each chain is built longest-first, so its shortest member is its last.
        let attach = chains
            .iter()
            .enumerate()
            .filter(|(_, chain)| {
                let tail = *chain.last().expect("a chain is never empty");
                is_proper_prefix(&keys[group], &keys[tail])
            })
            .max_by_key(|(_, chain)| rev_pos[*chain.last().expect("a chain is never empty")])
            .map(|(index, _)| index);
        match attach {
            Some(index) => chains[index].push(group),
            None => chains.push(vec![group]),
        }
    }

    chains.sort_by_key(|chain| chain.iter().map(|&g| rev_pos[g]).max().unwrap_or(0));
    chains.into_iter().flatten().collect()
}

/// Rewrite one expression tree, replacing each [`BoundExpr::WindowFunc`] marker
/// with a `ColumnRef` into the window chain's output row and recording the call
/// against its spec's group.
///
/// Slots are handed out in encounter order across all groups, so a group's slots
/// need not be contiguous — which is what lets the chain evaluate the groups in
/// a different order than the query mentions them.
fn rewrite_over_window(
    expr: BoundExpr,
    input_width: usize,
    groups: &mut Vec<WindowGroup>,
) -> Result<BoundExpr, BindError> {
    match expr {
        BoundExpr::WindowFunc { kind, spec, ret } => {
            if kind.args().iter().any(BoundExpr::contains_window)
                || spec.exprs().any(BoundExpr::contains_window)
            {
                return Err(BindError::new(
                    sqlstate::WINDOWING_ERROR,
                    "window function calls cannot be nested",
                ));
            }
            let slot = input_width + groups.iter().map(|g| g.funcs.len()).sum::<usize>();
            let spec = *spec;
            let group = match groups.iter_mut().position(|g| g.spec == spec) {
                Some(index) => &mut groups[index],
                None => {
                    groups.push(WindowGroup {
                        spec,
                        funcs: Vec::new(),
                    });
                    groups.last_mut().expect("just pushed")
                }
            };
            group.funcs.push(BoundWindowFunc { kind, ret, slot });
            Ok(BoundExpr::ColumnRef {
                index: slot,
                ty: ret,
            })
        }
        // Leaves, and the subplan markers: a window inside a subquery body
        // belongs to that query level and is extracted when it is bound.
        leaf @ (BoundExpr::Const { .. }
        | BoundExpr::ColumnRef { .. }
        | BoundExpr::Param { .. }
        | BoundExpr::OuterColumnRef { .. }
        | BoundExpr::Aggregate { .. }
        | BoundExpr::ScalarSubquery { .. }
        | BoundExpr::ArraySubquery { .. }
        | BoundExpr::Exists { .. }) => Ok(leaf),
        BoundExpr::Unary { op, expr } => Ok(BoundExpr::Unary {
            op,
            expr: Box::new(rewrite_over_window(*expr, input_width, groups)?),
        }),
        BoundExpr::Collate {
            expr,
            collation,
            explicit,
        } => Ok(BoundExpr::Collate {
            expr: Box::new(rewrite_over_window(*expr, input_width, groups)?),
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
            left: Box::new(rewrite_over_window(*left, input_width, groups)?),
            right: Box::new(rewrite_over_window(*right, input_width, groups)?),
        }),
        BoundExpr::IsNull { expr, negated } => Ok(BoundExpr::IsNull {
            expr: Box::new(rewrite_over_window(*expr, input_width, groups)?),
            negated,
        }),
        BoundExpr::BoolTest {
            expr,
            value,
            negated,
        } => Ok(BoundExpr::BoolTest {
            expr: Box::new(rewrite_over_window(*expr, input_width, groups)?),
            value,
            negated,
        }),
        BoundExpr::Coerce { expr, ty } => Ok(BoundExpr::Coerce {
            expr: Box::new(rewrite_over_window(*expr, input_width, groups)?),
            ty,
        }),
        BoundExpr::Reinterpret {
            expr,
            reported,
            rep,
        } => Ok(BoundExpr::Reinterpret {
            expr: Box::new(rewrite_over_window(*expr, input_width, groups)?),
            reported,
            rep,
        }),
        BoundExpr::FuncCall { func, ret, args } => Ok(BoundExpr::FuncCall {
            func,
            ret,
            args: rewrite_all_over_window(args, input_width, groups)?,
        }),
        BoundExpr::Routine {
            oid,
            name,
            arg_types,
            strict,
            args,
            ret,
        } => Ok(BoundExpr::Routine {
            oid,
            name,
            arg_types,
            strict,
            args: rewrite_all_over_window(args, input_width, groups)?,
            ret,
        }),
        BoundExpr::Srf { func, ret, args } => Ok(BoundExpr::Srf {
            func,
            ret,
            args: rewrite_all_over_window(args, input_width, groups)?,
        }),
        BoundExpr::ArrayCtor { elem, ty, elems } => Ok(BoundExpr::ArrayCtor {
            elem,
            ty,
            elems: rewrite_all_over_window(elems, input_width, groups)?,
        }),
        BoundExpr::Coalesce { args, ty } => Ok(BoundExpr::Coalesce {
            args: rewrite_all_over_window(args, input_width, groups)?,
            ty,
        }),
        BoundExpr::MinMax {
            kind,
            args,
            ty,
            collation,
        } => Ok(BoundExpr::MinMax {
            kind,
            args: rewrite_all_over_window(args, input_width, groups)?,
            ty,
            collation,
        }),
        BoundExpr::Subscript { base, index, ty } => Ok(BoundExpr::Subscript {
            base: Box::new(rewrite_over_window(*base, input_width, groups)?),
            index: Box::new(rewrite_over_window(*index, input_width, groups)?),
            ty,
        }),
        BoundExpr::Case { whens, else_, ty } => Ok(BoundExpr::Case {
            whens: whens
                .into_iter()
                .map(|(cond, result)| {
                    Ok((
                        rewrite_over_window(cond, input_width, groups)?,
                        rewrite_over_window(result, input_width, groups)?,
                    ))
                })
                .collect::<Result<Vec<_>, BindError>>()?,
            else_: match else_ {
                Some(e) => Some(Box::new(rewrite_over_window(*e, input_width, groups)?)),
                None => None,
            },
            ty,
        }),
        BoundExpr::QuantifiedSubquery { subplan, all, cmp } => Ok(BoundExpr::QuantifiedSubquery {
            subplan,
            all,
            cmp: Box::new(rewrite_over_window(*cmp, input_width, groups)?),
        }),
        BoundExpr::QuantifiedArray { array, all, cmp } => Ok(BoundExpr::QuantifiedArray {
            array: Box::new(rewrite_over_window(*array, input_width, groups)?),
            all,
            cmp: Box::new(rewrite_over_window(*cmp, input_width, groups)?),
        }),
    }
}

/// [`rewrite_over_window`] over an argument list.
fn rewrite_all_over_window(
    exprs: Vec<BoundExpr>,
    input_width: usize,
    groups: &mut Vec<WindowGroup>,
) -> Result<Vec<BoundExpr>, BindError> {
    exprs
        .into_iter()
        .map(|e| rewrite_over_window(e, input_width, groups))
        .collect()
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
        reject_agg_or_window(&bound, "GROUP BY")?;
        // The executor groups with `compare_values`, which cannot order every
        // type (`bit`, user types); reject such a key at bind time rather than
        // panicking mid-group. Grouping needs only equality, so this admits
        // `xid` -- which has a hash opclass but no btree one.
        if !crate::expr::has_equality(bound.ty(), scope.catalog().as_ref()) {
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
        return crate::expr::literal_int(n).and_then(|v| usize::try_from(v).ok());
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
            agg_args: args,
            order_by,
            input_ty,
            ret,
        } => {
            if args.iter().any(|a| a.contains_aggregate()) {
                return Err(BindError::new(
                    sqlstate::GROUPING_ERROR,
                    "aggregate function calls cannot be nested",
                ));
            }
            // Windows are evaluated *after* grouping, so an aggregate cannot
            // consume one. This has to be caught here: by the time the window
            // pass runs, every aggregate has already become a `ColumnRef` and
            // the containment is no longer visible.
            if args.iter().any(BoundExpr::contains_window) {
                return Err(BindError::new(
                    sqlstate::GROUPING_ERROR,
                    "aggregate function calls cannot contain window function calls",
                ));
            }
            let index = group_exprs.len() + aggregates.len();
            let collation = args.first().map_or(DEFAULT_COLLATION_OID, |a| {
                crate::collation::expr_collation(a).collation
            });
            // `args` and `order_by` both index the *source* row, so neither is
            // rebased here — they travel into the aggregate node unchanged,
            // while the marker itself becomes a `ColumnRef` into its output.
            aggregates.push(BoundAggregate {
                func,
                distinct,
                args,
                order_by,
                input_ty,
                ret,
                collation,
            });
            Ok(BoundExpr::ColumnRef { index, ty: ret })
        }
        // A window is evaluated over the *aggregate's* output rows, so its
        // arguments and OVER clause are rebased here alongside the projections.
        // This is what makes `sum(sum(x)) OVER (PARTITION BY y) … GROUP BY y`
        // bind: the inner `sum(x)` becomes a `ColumnRef` into the aggregate row,
        // and `y` matches a group key by the whole-subexpression rule above.
        BoundExpr::WindowFunc { kind, spec, ret } => {
            let kind = match kind {
                WindowKind::Builtin { func, args } => WindowKind::Builtin {
                    func,
                    args: args
                        .into_iter()
                        .map(|a| rewrite_over_aggregate(a, group_exprs, aggregates, scope))
                        .collect::<Result<Vec<_>, _>>()?,
                },
                WindowKind::Aggregate(agg) => WindowKind::Aggregate(BoundAggregate {
                    args: agg
                        .args
                        .into_iter()
                        .map(|a| rewrite_over_aggregate(a, group_exprs, aggregates, scope))
                        .collect::<Result<Vec<_>, _>>()?,
                    ..agg
                }),
            };
            let spec = *spec;
            let spec = BoundWindowSpec {
                partition_by: spec
                    .partition_by
                    .into_iter()
                    .map(|a| rewrite_over_aggregate(a, group_exprs, aggregates, scope))
                    .collect::<Result<Vec<_>, _>>()?,
                order_by: spec
                    .order_by
                    .into_iter()
                    .map(|key| {
                        Ok(ExprSortKey {
                            expr: rewrite_over_aggregate(key.expr, group_exprs, aggregates, scope)?,
                            ..key
                        })
                    })
                    .collect::<Result<Vec<_>, BindError>>()?,
            };
            Ok(BoundExpr::WindowFunc {
                kind,
                spec: Box::new(spec),
                ret,
            })
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
        BoundExpr::BoolTest {
            expr,
            value,
            negated,
        } => Ok(BoundExpr::BoolTest {
            expr: Box::new(rewrite_over_aggregate(
                *expr,
                group_exprs,
                aggregates,
                scope,
            )?),
            value,
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
        BoundExpr::Coalesce { args, ty } => {
            let args = args
                .into_iter()
                .map(|a| rewrite_over_aggregate(a, group_exprs, aggregates, scope))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(BoundExpr::Coalesce { args, ty })
        }
        BoundExpr::MinMax {
            kind,
            args,
            ty,
            collation,
        } => {
            let args = args
                .into_iter()
                .map(|a| rewrite_over_aggregate(a, group_exprs, aggregates, scope))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(BoundExpr::MinMax {
                kind,
                args,
                ty,
                collation,
            })
        }
        BoundExpr::Subscript { base, index, ty } => Ok(BoundExpr::Subscript {
            base: Box::new(rewrite_over_aggregate(
                *base,
                group_exprs,
                aggregates,
                scope,
            )?),
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
        // TODO: expand a set-returning function in the target list of a grouped
        // query. PG computes SRFs *after* aggregation, so
        // `SELECT a, count(*), unnest('{1,1,3}'::int[]) ... GROUP BY a` returns
        // one row per generated element (upstream regression test `tsrf`).
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
        //
        // TODO: rebase an `OuterColumnRef` onto the aggregate's output row so a
        // correlated subquery can appear in the target list or HAVING of a
        // grouped query, which PG answers.
        c @ (BoundExpr::ScalarSubquery { .. }
        | BoundExpr::ArraySubquery { .. }
        | BoundExpr::Exists { .. }) => {
            reject_correlated_over_aggregate(&c)?;
            Ok(c)
        }
        BoundExpr::QuantifiedSubquery { subplan, all, cmp } => {
            if plan_has_outer_refs(&subplan.plan) {
                return Err(correlated_over_aggregate_error());
            }
            Ok(BoundExpr::QuantifiedSubquery {
                subplan,
                all,
                cmp: Box::new(rewrite_over_aggregate(
                    *cmp,
                    group_exprs,
                    aggregates,
                    scope,
                )?),
            })
        }
        // `x op ANY/ALL(array)` has no subplan; rewrite both operands so any
        // aggregate/group-key reference inside them is redirected to the
        // aggregate node's output row.
        BoundExpr::QuantifiedArray { array, all, cmp } => Ok(BoundExpr::QuantifiedArray {
            array: Box::new(rewrite_over_aggregate(
                *array,
                group_exprs,
                aggregates,
                scope,
            )?),
            all,
            cmp: Box::new(rewrite_over_aggregate(
                *cmp,
                group_exprs,
                aggregates,
                scope,
            )?),
        }),
    }
}

/// Reject a scalar/EXISTS subquery marker that is correlated to the enclosing
/// (aggregating) query when it appears in a target-list/HAVING expression — see
/// [`rewrite_over_aggregate`]. Non-correlated markers are left alone.
fn reject_correlated_over_aggregate(marker: &BoundExpr) -> Result<(), BindError> {
    let subplan = match marker {
        BoundExpr::ScalarSubquery { subplan, .. }
        | BoundExpr::ArraySubquery { subplan, .. }
        | BoundExpr::Exists { subplan, .. } => &subplan.plan,
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

/// PostgreSQL's refusal for a statement writing a value into a generated
/// column. `428C9`, with the same DETAIL for every verb; the message names what
/// the statement could have written instead. Probed against 18.4.
fn generated_column_write_error(column: &Column, verb: WriteVerb) -> BindError {
    let message = match verb {
        WriteVerb::Update => format!("column \"{}\" can only be updated to DEFAULT", column.name),
        _ => format!(
            "cannot insert a non-DEFAULT value into column \"{}\"",
            column.name
        ),
    };
    BindError::new("428C9", message).with_detail(Some(format!(
        "Column \"{}\" is a generated column.",
        column.name
    )))
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
    let expr = crate::parse_stored_expr(sql, "default expression")?;
    bind_column_default(&expr, column, catalog)
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
            // A query source has no way to spell DEFAULT, so any column it
            // covers is a value the statement supplies.
            if schema.columns[idx].generated.is_some() {
                return Err(generated_column_write_error(
                    &schema.columns[idx],
                    WriteVerb::Insert,
                ));
            }
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
            // one, missing trailing columns take their column defaults.
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
                    // `DEFAULT` is the one thing a generated column accepts —
                    // and it means "compute it", which the executor does.
                    if schema.columns[idx].generated.is_some() {
                        return Err(generated_column_write_error(
                            &schema.columns[idx],
                            WriteVerb::Insert,
                        ));
                    }
                    let binding = bind_expr(expr, &scope)?;
                    if let Binding::Typed(expr) = &binding {
                        reject_agg_or_window(expr, "VALUES")?;
                    }
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
    // Only RETURNING can name a system column on an INSERT — there is no WHERE,
    // and the VALUES bound in the empty scope.
    let (system, declined_system) = supported_system_columns(
        without_declared(
            SystemDemand::in_write(&None, &insert.returning, &[]),
            &schema,
        )
        .to_vec(),
        &table,
    );
    let returning = bind_returning(
        &insert.returning,
        &Scope::table(&schema, name, catalog, params, &system, declined_system)
            .with_subqueries(engine, &CteEnv::new()),
    )?;

    Ok(LogicalPlan::Insert(InsertPlan {
        table,
        source: insert_source,
        returning,
        routing,
        // Only COPY can freeze; there is no `INSERT … FREEZE` in PostgreSQL.
        freeze: false,
        system,
    }))
}

/// What `COPY … HEADER` does with the first data line, resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyHeader {
    /// No header line; the first line is data.
    Off,
    /// Discard the first line unread.
    On,
    /// Discard it, but first check its field names against the columns the COPY
    /// names — carried here already resolved, in statement order, because the
    /// decoder has the header line and nothing else.
    Match(Vec<String>),
}

impl CopyHeader {
    /// Whether a first line is to be consumed as a header at all.
    pub fn present(&self) -> bool {
        !matches!(self, CopyHeader::Off)
    }
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
    /// What to do with the first data line (`HEADER`).
    pub header: CopyHeader,
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
            header: CopyHeader::Off,
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
            header: CopyHeader::Off,
            quote: b'"',
            escape: b'"',
            force_not_null: Vec::new(),
        }
    }
}

/// Where a bound COPY reads its row bytes from.
///
/// TODO: restrict the file form to superusers (or `pg_read_server_files`) the
/// way PostgreSQL does — there is no role system to check against, so the read
/// is unconditional. Relative paths, which PG resolves against the data
/// directory, are rejected rather than resolved (see the server's file reader).
#[derive(Clone, Debug)]
pub enum CopyFromSource {
    /// `FROM STDIN`: the bytes stream in over the wire's copy-in sub-protocol.
    Stdin,
    /// `FROM '<file>'`: the server reads the file itself. Holds the path exactly
    /// as written in the statement, because that is what PG's error text quotes.
    File(String),
}

/// A bound `COPY <table> [(cols)] FROM {STDIN | '<file>'}`: the resolved write
/// target, the data-column → schema-column mapping, per-column defaults for
/// columns absent from the column list, and the text/CSV format. The row bytes
/// arrive later, so binding is split: this resolves everything the server needs
/// before sending `CopyInResponse` (or opening the file), and
/// [`build_insert`](Self::build_insert) parses the decoded field rows straight
/// into their columns' [`Value`]s, yielding a [`LogicalPlan::Insert`] over
/// [`InsertSource::Tuples`].
///
/// There is deliberately only one route. An earlier version kept the binder's
/// ordinary assignment coercion as a fallback for columns the direct route
/// could not read, but the two disagreed on which error a bad row reported —
/// the direct route folds a field where the expression route deferred it past
/// the column defaults — and nothing could reach the fallback to notice: the
/// only type needing it was `interval[]`, since `CREATE TABLE` refuses every
/// `reg*` column. With the `interval[]` gap closed in `resolve_unknown`, the
/// second route had no reason left to exist.
pub struct CopyFromPlan {
    table: Arc<dyn TableAm>,
    table_name: String,
    /// The shape COPY was planned against, pinned for the statement's life.
    schema: Arc<TableSchema>,
    /// One schema-column index per data column, in wire order.
    target_indices: Vec<usize>,
    pub format: CopyFormat,
    /// `FREEZE`: stamp the loaded rows visible-to-everyone rather than
    /// visible-once-this-transaction-commits. Binding only records the request
    /// and rejects the relations that can never honor it; the precondition that
    /// makes freezing safe needs a transaction, so the server checks it (and
    /// sets [`crabgresql_txn::TxnContext::freeze_inserts`]) once it has one.
    pub freeze: bool,
    /// Where the row bytes come from: the wire, or a server-side file.
    pub source: CopyFromSource,
    /// Leaf partitions when `table` is a partitioned parent, so each decoded row
    /// routes to the leaf whose RANGE bound admits it (reusing the executor's
    /// INSERT routing); `None` for an ordinary table.
    routing: Option<Vec<Arc<dyn TableAm>>>,
    /// The row shape, worked out once at bind time so a load pays for it once
    /// rather than once per row.
    rows: CopyRowPlan,
}

/// Where one column of a row comes from. Together these say how to build a
/// tuple in one left-to-right pass, with nothing written twice.
enum CopySlot {
    /// The decoded field at this position in the data row (an index into
    /// [`CopyFromPlan::target_indices`], i.e. wire order).
    Field(usize),
    /// A `DEFAULT` that folded to a constant. The one value a row still clones,
    /// and only for the columns COPY does not carry.
    Const(Value),
    /// No value here: a column with no `DEFAULT`, or one whose `DEFAULT` the
    /// executor evaluates per row (see `defaults`).
    Null,
}

/// How to build one row: where each column's value comes from, and what the
/// executor still owes it.
struct CopyRowPlan {
    /// One slot per schema column, in schema order.
    slots: Vec<CopySlot>,
    /// Whether the target columns ascend, i.e. whether walking `slots` left to
    /// right reads the data fields in wire order. True for every COPY without a
    /// column list, and for any list written in schema order; false only for a
    /// reordered list like `COPY t (b, a)`, where the fields must still be
    /// *parsed* in wire order so a row with two bad fields reports the error
    /// PostgreSQL reports — see `build_insert`.
    ordered: bool,
    /// Columns whose `DEFAULT` did not fold, ascending. Handed to the executor,
    /// which evaluates them once per row.
    ///
    /// TODO: a `timestamptz`/`timetz` column whose `DEFAULT` is a literal binds
    /// to `Coerce{Const(Text)}` rather than a constant, so it lands here and is
    /// re-parsed for every row. `build_insert` holds the `FmtCtx` that could
    /// fold it once per batch, the way `session_literal_default` does at DDL
    /// time. `DEFAULT now()` is a `FuncCall` and must stay per-row regardless.
    defaults: Vec<(usize, BoundExpr)>,
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

    /// The write target, so the server can ask whether it satisfies `FREEZE`'s
    /// precondition once it holds a transaction.
    pub fn target(&self) -> &Arc<dyn TableAm> {
        &self.table
    }

    /// Turn decoded field rows (`None` = the NULL marker matched) into a
    /// [`LogicalPlan::Insert`]: each field parses through its column's input
    /// function with the column typmod (so `char(n)` blank-pads, an over-long
    /// `varchar(n)` errors), exactly as a SQL literal would; columns absent from
    /// the COPY column list take their default. Arity mismatches surface as PG's
    /// `extra data` / `missing data` errors.
    ///
    /// Values are built directly, with no expression tree in between: the rows
    /// are already parsed, so wrapping each cell in a [`BoundExpr::Const`] only
    /// for the executor to clone it back out doubles the work of the statement.
    /// What makes that possible is the `FmtCtx` — a load holds the session the
    /// binder normally lacks, so a `timestamptz` field, a `'now'`, or a
    /// style-sensitive `interval` folds here against exactly the zone and
    /// transaction clock a deferred runtime coercion would have used.
    pub fn build_insert(
        &self,
        catalog: &Arc<dyn TypeCatalog>,
        fmt: &FmtCtx,
        rows: &RowBatch,
    ) -> Result<LogicalPlan, BindError> {
        // An enum label is resolved against the catalog, which `parse_unknown`
        // has no access to — same order as `resolve_unknown_ctx`, whose
        // `PgType::User` arm would otherwise reject every label. Looked up once
        // per batch rather than once per cell: `enum_info` takes a read lock,
        // scans the type catalog for the OID and clones every label, which is
        // not a thing to do a million times. Per batch and not once per load, so
        // a concurrent `ALTER TYPE ... ADD VALUE` still becomes visible.
        let enums: Vec<Option<EnumInfo>> = self
            .target_indices
            .iter()
            .map(|&idx| match self.schema.columns[idx].ty {
                PgType::User(oid) => catalog.enum_info(oid),
                _ => None,
            })
            .collect();

        // A reordered column list parses into this first, so the fields are read
        // in wire order while the tuple is still assembled in schema order.
        // Allocated once for the batch — and not at all for the ordered case,
        // where the slot walk is already the wire order.
        let mut scattered: Vec<Value> = Vec::new();

        // The check rides along with the fill below, where the value is already
        // in hand — but it does not raise there: the row is half-built, and a
        // violation's DETAIL renders the whole row. What ships instead is the
        // negative result, for the columns it held for in every row.
        let mut null_seen = vec![false; self.target_indices.len()];

        let mut tuples = Vec::with_capacity(rows.len());
        for fields in rows.iter() {
            self.check_arity(fields.len())?;
            if !self.rows.ordered {
                scattered.clear();
                for ((field, &idx), enum_info) in
                    fields.iter().zip(&self.target_indices).zip(&enums)
                {
                    scattered.push(self.field_value(field, idx, enum_info.as_ref(), fmt)?);
                }
            }
            let mut tuple = Vec::with_capacity(self.rows.slots.len());
            for slot in &self.rows.slots {
                tuple.push(match slot {
                    CopySlot::Null => Value::Null,
                    CopySlot::Const(value) => value.clone(),
                    CopySlot::Field(k) => {
                        let value = match self.rows.ordered {
                            true => self.field_value(
                                fields.get(*k),
                                self.target_indices[*k],
                                enums[*k].as_ref(),
                                fmt,
                            )?,
                            // Already parsed above, in wire order; move it out
                            // rather than clone it.
                            false => std::mem::replace(&mut scattered[*k], Value::Null),
                        };
                        null_seen[*k] |= matches!(value, Value::Null);
                        value
                    }
                });
            }
            tuples.push(tuple);
        }
        // Sorted because `target_indices` need not be — a column list may name
        // the columns in any order, and the executor merges this against the
        // schema in one pass.
        //
        // Nothing to collect for a routed load: the list indexes the parent's
        // shape, and a routed row is checked against the leaf it lands in.
        let mut notnull_verified: Vec<u32> = match self.routing {
            Some(_) => Vec::new(),
            None => self
                .target_indices
                .iter()
                .zip(&null_seen)
                .filter(|&(_, &seen)| !seen)
                .map(|(&idx, _)| idx as u32)
                .collect(),
        };
        notnull_verified.sort_unstable();
        Ok(LogicalPlan::Insert(InsertPlan {
            table: self.table.clone(),
            source: InsertSource::Tuples {
                rows: tuples,
                defaults: self.rows.defaults.clone(),
                notnull_verified,
            },
            returning: None,
            // A partitioned parent routes each decoded row to a leaf, reusing the
            // executor's INSERT tuple routing; `None` targets an ordinary table.
            routing: self.routing.clone(),
            // The server has already verified the precondition against this very
            // target (it needs a transaction, which binding has no access to).
            freeze: self.freeze,
            // COPY has no RETURNING, so nothing can read a system column.
            system: Arc::from(&[][..]),
        }))
    }

    /// One decoded field as its column's [`Value`]: `None` is the NULL marker,
    /// anything else parses through the column's input function and then its
    /// length rule. `enum_info` is the column's labels when it is an enum,
    /// resolved once per batch by the caller.
    fn field_value(
        &self,
        field: Option<&str>,
        idx: usize,
        enum_info: Option<&EnumInfo>,
        fmt: &FmtCtx,
    ) -> Result<Value, BindError> {
        // The NULL marker: a genuine SQL NULL, not the column default, and it
        // takes no typmod.
        let Some(text) = field else {
            return Ok(Value::Null);
        };
        let column = &self.schema.columns[idx];
        if let Some(info) = enum_info {
            let PgType::User(oid) = column.ty else {
                unreachable!("only a user type resolves an enum")
            };
            return apply_column_typmod(enum_value(oid, info, text)?, column);
        }
        match column.ty {
            // The text family goes straight from the batch's arena to the
            // finished value: parsing it into a `String` first, only for the
            // length rule to read that `String` and build another, is the one
            // allocation a load can drop outright.
            PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => {
                text_column_value(text, column)
            }
            // Anything else, including a user type that is not an enum: the
            // input dispatch handles it, naming the type `user-defined` the way
            // the expression path does. Reachable, because dropping a type a
            // column still uses is allowed here.
            _ => apply_column_typmod(parse_unknown(text, column.ty, fmt)?, column),
        }
    }

    /// A data row must supply exactly the columns the statement named.
    fn check_arity(&self, got: usize) -> Result<(), BindError> {
        // Indexing `target_indices` below is in bounds only because `want` is
        // its length, so it is read here rather than passed in.
        let want = self.target_indices.len();
        if got > want {
            return Err(BindError::new(
                sqlstate::BAD_COPY_FILE_FORMAT,
                "extra data after last expected column",
            ));
        }
        if got < want {
            let missing = self.target_indices[got];
            return Err(BindError::new(
                sqlstate::BAD_COPY_FILE_FORMAT,
                format!(
                    "missing data for column \"{}\"",
                    self.schema.columns[missing].name
                ),
            ));
        }
        Ok(())
    }
}

/// Bind `COPY <table> [(cols)] FROM {STDIN | '<file>'} [WITH (…)]`. Resolves the
/// write target and column list the same way INSERT does, and resolves the
/// text/CSV options into a [`CopyFormat`].
///
/// TODO: `COPY TO`, a query source (`COPY (SELECT …)`), `FROM PROGRAM` and the
/// binary format; each is rejected here with its own error.
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
    let copy_source = match target {
        ast::CopyTarget::Stdin => CopyFromSource::Stdin,
        ast::CopyTarget::File { filename } => CopyFromSource::File(filename.clone()),
        ast::CopyTarget::Program { .. } => {
            return Err(BindError::feature_not_supported(
                "COPY from a program is not supported yet",
            ));
        }
        // `COPY … FROM STDOUT` is not accepted by the parser's FROM branch.
        ast::CopyTarget::Stdout => {
            return Err(BindError::feature_not_supported(
                "COPY from STDOUT is not supported",
            ));
        }
    };

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

    // Map the optional column list. An absent one is every column a COPY may
    // carry, which excludes the generated ones: their value is not something the
    // data stream provides, and upstream leaves them out of the implicit list in
    // both directions.
    let target_indices: Vec<usize> = if columns.is_empty() {
        copy_all_columns(&schema)
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
            reject_generated_in_copy(&schema.columns[idx])?;
            indices.push(idx);
        }
        indices
    };

    let format = resolve_copy_format(options, legacy_options, &schema, &target_indices, &name)?;
    let freeze = resolve_copy_freeze(options, legacy_options);
    if freeze {
        // A partitioned parent has no storage of its own, so there is no
        // relfilenode for a rollback to discard and nothing to freeze into.
        if schema.partition_scheme.is_some() {
            return Err(BindError::feature_not_supported(
                "cannot perform COPY FREEZE on a partitioned table",
            ));
        }
        // A `buffer` relation keeps its rows in one flat RAM list that no rollback
        // discards, so a frozen row written there would outlive its transaction.
        // Stated rather than silently downgraded to an unfrozen load.
        if schema.access_method == TableAccessMethod::Buffer {
            return Err(BindError::feature_not_supported(
                "cannot perform COPY FREEZE on a buffer table",
            ));
        }
    }

    let rows = build_row_plan(&schema, &target_indices, defaults);

    Ok(CopyFromPlan {
        table,
        table_name: name,
        schema,
        target_indices,
        format,
        freeze,
        source: copy_source,
        routing,
        rows,
    })
}

/// The columns an unqualified `COPY` carries: every column but the generated
/// ones, whose values are not part of the data stream in either direction.
fn copy_all_columns(schema: &TableSchema) -> Vec<usize> {
    (0..schema.columns.len())
        .filter(|&i| schema.columns[i].generated.is_none())
        .collect()
}

/// PostgreSQL's refusal for a generated column named in a `COPY` column list,
/// `42P10` in both directions. Probed against 18.4.
fn reject_generated_in_copy(column: &Column) -> Result<(), BindError> {
    if column.generated.is_none() {
        return Ok(());
    }
    Err(BindError::new(
        "42P10",
        format!("column \"{}\" is a generated column", column.name),
    )
    .with_detail(Some(
        "Generated columns cannot be used in COPY.".to_string(),
    )))
}

/// Split the column defaults into what every row starts from and what the
/// executor must still evaluate for each one.
fn build_row_plan(
    schema: &TableSchema,
    target_indices: &[usize],
    defaults: Vec<BoundExpr>,
) -> CopyRowPlan {
    // Which data field fills each column, by schema index. A lookup table
    // rather than a scan of `target_indices` per column, so a wide relation does
    // not pay O(columns x targets) to answer a question with one number in it.
    let mut from_field = vec![None; schema.columns.len()];
    for (wire, &index) in target_indices.iter().enumerate() {
        from_field[index] = Some(wire);
    }
    let mut slots = Vec::with_capacity(schema.columns.len());
    let mut deferred = Vec::new();
    for (index, default) in defaults.into_iter().enumerate() {
        // A target column's default is dead: the decoded field is the value.
        if let Some(wire) = from_field[index] {
            slots.push(CopySlot::Field(wire));
            continue;
        }
        match default {
            BoundExpr::Const { value, .. } => slots.push(CopySlot::Const(value)),
            // `nextval` on a `serial`, `now()`, a routine — must run per row.
            other => {
                slots.push(CopySlot::Null);
                deferred.push((index, other));
            }
        }
    }
    CopyRowPlan {
        slots,
        ordered: target_indices.windows(2).all(|pair| pair[0] < pair[1]),
        defaults: deferred,
    }
}

/// Whether `FREEZE` is on. The pre-9.0 bare keyword (`COPY … CSV FREEZE`) means
/// the same thing as `WITH (FREEZE)`, and carries no argument.
///
/// Exactly one of the two lists can be non-empty and neither can repeat an
/// option: the parser rejects a repeat with PostgreSQL's `conflicting or
/// redundant options`, and the mixed form with a syntax error, because upstream's
/// grammar makes the spellings alternatives. So there is no precedence rule here
/// to get wrong — which is the whole reason this reads as a search rather than as
/// a fold with a mutable accumulator.
fn resolve_copy_freeze(
    options: &[ast::CopyOption],
    legacy_options: &[ast::CopyLegacyOption],
) -> bool {
    options
        .iter()
        .any(|opt| matches!(opt, ast::CopyOption::Freeze(true)))
        || legacy_options.contains(&ast::CopyLegacyOption::Freeze)
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
            let pos = target_indices
                .iter()
                .position(|&i| i == sidx)
                .ok_or_else(|| {
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
            // `MATCH` needs the columns the statement named, in its order — the
            // decoder sees only the header line, and PostgreSQL compares against
            // the COPY column list rather than the table's own order.
            ast::CopyOption::Header(mode) => {
                fmt.header = match mode {
                    ast::CopyHeaderMode::Off => CopyHeader::Off,
                    ast::CopyHeaderMode::On => CopyHeader::On,
                    ast::CopyHeaderMode::Match => CopyHeader::Match(
                        target_indices
                            .iter()
                            .map(|&i| schema.columns[i].name.clone())
                            .collect(),
                    ),
                }
            }
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
            ast::CopyLegacyOption::Header => fmt.header = CopyHeader::On,
            // Freeze is not a decoding rule; `resolve_copy_freeze` reads it.
            ast::CopyLegacyOption::Binary
            | ast::CopyLegacyOption::Csv(_)
            | ast::CopyLegacyOption::Freeze => {}
            other => {
                return Err(BindError::feature_not_supported(format!(
                    "COPY option {other} is not supported yet"
                )));
            }
        }
        if let ast::CopyLegacyOption::Csv(sub) = opt {
            for s in sub {
                match s {
                    ast::CopyLegacyCsvOption::Header => fmt.header = CopyHeader::On,
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

/// TODO: decode COPY input in an `ENCODING` other than UTF8, which needs
/// server-side encoding conversion. Rejected rather than mis-decoded silently.
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

    let (table, qualifier, only) =
        open_write_relation(engine, &update.table.relation, WriteVerb::Update)?;
    let schema = table.schema().clone();
    let table_name = schema.name.clone();
    // A partitioned parent routes each updated row to a leaf (moving it across
    // leaves when the key changes); capture the leaves now. `None` for a plain table.
    let routing = if schema.partition_scheme.is_some() {
        Some(partition_leaves(engine, &schema)?)
    } else {
        None
    };
    let (system, declined_system) = supported_system_columns(
        without_declared(
            SystemDemand::in_write(&update.selection, &update.returning, &update.assignments),
            &schema,
        )
        .to_vec(),
        &table,
    );
    // An inheritance parent instead updates every descendant in place.
    let inherited = write_targets(engine, &table, only, &system)?;
    // SET / WHERE / RETURNING may all contain subqueries; UPDATE takes no WITH,
    // so the CTE environment is empty.
    let scope = Scope::table(
        &schema,
        qualifier,
        catalog,
        params,
        &system,
        declined_system,
    )
    .with_subqueries(engine, &CteEnv::new());

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
        // A system column is not a target: PostgreSQL says so with its own
        // message rather than "does not exist", because the name *does* resolve
        // — it is the assignment that has nowhere to go. (An INSERT column list
        // is different, and does report 42703: there the name is looked up
        // among the declared columns only.)
        if let Some(col) = SysCol::ALL.iter().find(|c| c.name() == column) {
            return Err(BindError::feature_not_supported(format!(
                "cannot assign to system column \"{}\"",
                col.name()
            )));
        }
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
            // `SET b = DEFAULT` is accepted above and means "recompute it"; any
            // other value is the statement writing a generated column.
            if schema.columns[idx].generated.is_some() {
                return Err(generated_column_write_error(
                    &schema.columns[idx],
                    WriteVerb::Update,
                ));
            }
            let binding = bind_expr(&assignment.value, &scope)?;
            if let Binding::Typed(expr) = &binding {
                reject_agg_or_window(expr, "UPDATE")?;
            }
            coerce_to_column(binding, &schema.columns[idx], &scope)?
        };
        assignments.push((idx, value));
    }

    let predicate = bind_where(&update.selection, &scope)?;
    if let Some(predicate) = &predicate {
        reject_agg_or_window(predicate, "WHERE")?;
    }
    // RETURNING references the NEW row (post-update), which the executor feeds
    // in schema order — the same scope the SET/WHERE clauses bound against.
    let returning = bind_returning(&update.returning, &scope)?;
    Ok(LogicalPlan::Update(UpdatePlan {
        system,
        table,
        predicate,
        assignments,
        returning,
        routing,
        inherited,
    }))
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

    let (table, qualifier, only) =
        open_write_relation(engine, &target.relation, WriteVerb::Delete)?;
    let schema = table.schema().clone();
    // A partitioned parent deletes matching rows from whichever leaf holds them;
    // capture the leaves now. `None` for a plain table.
    let routing = if schema.partition_scheme.is_some() {
        Some(partition_leaves(engine, &schema)?)
    } else {
        None
    };
    // A write answers `tableoid` from the target the row actually lives in, so
    // WHERE reads it too — `DELETE FROM p WHERE tableoid = 'p1'::regclass` must
    // match exactly the rows of that partition.
    let (system, declined_system) = supported_system_columns(
        without_declared(
            SystemDemand::in_write(&delete.selection, &delete.returning, &[]),
            &schema,
        )
        .to_vec(),
        &table,
    );
    // An inheritance parent instead deletes from every descendant in place.
    let inherited = write_targets(engine, &table, only, &system)?;
    // WHERE / RETURNING may contain subqueries; DELETE takes no WITH.
    let scope = Scope::table(
        &schema,
        qualifier,
        catalog,
        params,
        &system,
        declined_system,
    )
    .with_subqueries(engine, &CteEnv::new());
    let predicate = bind_where(&delete.selection, &scope)?;
    if let Some(predicate) = &predicate {
        reject_agg_or_window(predicate, "WHERE")?;
    }
    // RETURNING references the deleted (OLD) row, which the executor feeds.
    let returning = bind_returning(&delete.returning, &scope)?;
    Ok(LogicalPlan::Delete(DeletePlan {
        system,
        table,
        predicate,
        returning,
        routing,
        inherited,
    }))
}
