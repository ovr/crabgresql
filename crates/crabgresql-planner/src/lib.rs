//! Planner: logical plan → physical plan.
//!
//! With one access path (a full scan) the mapping is 1:1 — this crate exists
//! to hold the layer boundary where index selection, join ordering and
//! cost-based choices land later (docs/ARCHITECTURE.md §2).

use std::sync::Arc;
use std::time::Duration;

use crabgresql_binder::{
    AggInput, BinOp, BoundAggregate, BoundExpr, DistinctKey, InsertSource, JoinExpr, JoinInput,
    JoinKind, LogicalPlan, OutputColumn, Returning, SortKey, TableFn,
};
use crabgresql_storage_api::{IndexConstraint, IndexMetadata, TableAm, TableSchema};
use crabgresql_types::PgType;
use crabgresql_types::collation::DEFAULT_COLLATION_OID;

/// An executable plan. `Select` describes the SeqScan → Filter → Projection →
/// Sort pipeline the executor builds.
pub enum PhysicalPlan {
    Values {
        columns: Vec<OutputColumn>,
        rows: Vec<Vec<BoundExpr>>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
        distinct: Option<Vec<DistinctKey>>,
    },
    Select {
        table: Arc<dyn TableAm>,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
        distinct: Option<Vec<DistinctKey>>,
    },
    /// A single-table read served by an equality probe on `index_name`: the
    /// executor evaluates each `key` value once and asks the engine for the
    /// matching rows. The planner only emits this when the engine reports
    /// [`TableAm::supports_index_scan`], but the executor still scan-fallbacks
    /// defensively. `predicate` is the residual WHERE the index did not consume,
    /// applied as a `Filter`; the standard Projection → Sort tail follows,
    /// exactly as for [`Self::Select`].
    IndexScan {
        table: Arc<dyn TableAm>,
        index_name: String,
        /// One `(key column, equality value)` pair per index key column, in key
        /// order. The value expressions are row-constant and evaluated once.
        key: Vec<(usize, BoundExpr)>,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
        distinct: Option<Vec<DistinctKey>>,
    },
    Subquery {
        source: Box<PhysicalPlan>,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
        distinct: Option<Vec<DistinctKey>>,
    },
    TableFunction {
        func: TableFn,
        args: Vec<BoundExpr>,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
        distinct: Option<Vec<DistinctKey>>,
    },
    /// Recursive joined row source, then the standard Filter → Projection →
    /// Sort tail. Mirrors [`LogicalPlan::Join`].
    Join {
        source: PhysicalJoinExpr,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
        distinct: Option<Vec<DistinctKey>>,
    },
    /// Grouped aggregation. Mirrors [`LogicalPlan::Aggregate`]: the executor
    /// filters `input` by `predicate`, groups by `group_exprs`, accumulates the
    /// `aggregates`, filters groups by `having`, then runs the standard
    /// projection/sort tail.
    Aggregate {
        input: PhysicalAggInput,
        predicate: Option<BoundExpr>,
        group_exprs: Vec<BoundExpr>,
        aggregates: Vec<BoundAggregate>,
        having: Option<BoundExpr>,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        sort: Vec<SortKey>,
        distinct: Option<Vec<DistinctKey>>,
    },
    /// Union scan over the leaf partitions of a partitioned parent. Mirrors
    /// [`LogicalPlan::Append`]: the executor concatenates each leaf's scan into
    /// one row stream. A partitioned-parent FROM item is planned as a
    /// [`Self::Subquery`] wrapping this, so the standard projection/predicate/sort
    /// tail runs on top.
    Append {
        tables: Vec<Arc<dyn TableAm>>,
        columns: Vec<OutputColumn>,
    },
    /// A `UNION` / `UNION ALL`. Mirrors [`LogicalPlan::SetOp`]: the executor
    /// drains each arm into one row stream, coercing arms that need it, then
    /// applies this node's own deduplication and sort.
    SetOp {
        arms: Vec<PhysicalSetOpArm>,
        columns: Vec<OutputColumn>,
        sort: Vec<SortKey>,
        distinct: Option<Vec<DistinctKey>>,
    },
    /// LIMIT/OFFSET above a source plan (after its sort). Mirrors
    /// [`LogicalPlan::Limit`].
    Limit {
        source: Box<PhysicalPlan>,
        limit: Option<i64>,
        offset: Option<i64>,
    },
    Insert {
        table: Arc<dyn TableAm>,
        source: PhysicalInsertSource,
        returning: Option<Returning>,
        /// Leaf partitions for tuple routing when `table` is a partitioned parent
        /// (see [`LogicalPlan::Insert`]); `None` for an ordinary table.
        routing: Option<Vec<Arc<dyn TableAm>>>,
    },
    Update {
        table: Arc<dyn TableAm>,
        predicate: Option<BoundExpr>,
        assignments: Vec<(usize, BoundExpr)>,
        returning: Option<Returning>,
        /// Leaf partitions for tuple routing when `table` is a partitioned parent
        /// (see [`LogicalPlan::Update`]); `None` for an ordinary table.
        routing: Option<Vec<Arc<dyn TableAm>>>,
    },
    Delete {
        table: Arc<dyn TableAm>,
        predicate: Option<BoundExpr>,
        returning: Option<Returning>,
        /// Leaf partitions for tuple routing when `table` is a partitioned parent
        /// (see [`LogicalPlan::Delete`]); `None` for an ordinary table.
        routing: Option<Vec<Arc<dyn TableAm>>>,
    },
}

/// One arm of a [`PhysicalPlan::SetOp`], mirroring [`SetOpArm`].
pub struct PhysicalSetOpArm {
    pub plan: PhysicalPlan,
    /// Projections mapping this arm onto the set operation's output layout;
    /// `None` when it already emits that layout.
    pub coercion: Option<Vec<BoundExpr>>,
}

/// The rows an INSERT writes, mirroring [`InsertSource`] with the query source's
/// subplan already lowered to a [`PhysicalPlan`].
pub enum PhysicalInsertSource {
    /// Fully-formed rows, full-width in schema order, evaluated against the empty
    /// row.
    Values(Vec<Vec<BoundExpr>>),
    /// Rows pulled from `input`, each mapped through `projections` (full-width,
    /// schema order) evaluated against the source tuple.
    Query {
        input: Box<PhysicalPlan>,
        projections: Vec<BoundExpr>,
    },
}

/// A join input, mirroring [`JoinInput`] but with the subplan already lowered
/// to a [`PhysicalPlan`].
pub enum PhysicalJoinInput {
    Scan(Arc<dyn TableAm>),
    Subplan(Box<PhysicalPlan>),
    TableFunction { func: TableFn, args: Vec<BoundExpr> },
}

/// One equi-join key of a hash join: a pair of expressions, one addressing the
/// left input and one the right, that must compare equal. Both evaluate to `ty`
/// (the operands' promoted comparison type), so the hash/equality of the two
/// sides is computed under a single type. Extracted from `col = col` conjuncts
/// of the ON predicate (see [`extract_hash_keys`]).
///
/// The operand expressions still index into the *concatenated* `left || right`
/// row (the form the binder produced): `left` references `[0, left_width)` and
/// `right` references `[left_width, ..)`. The executor evaluates each against a
/// padded row so the indices stay valid.
#[derive(Clone, Debug, PartialEq)]
pub struct HashKey {
    pub left: BoundExpr,
    pub right: BoundExpr,
    pub ty: PgType,
}

/// A [`JoinExpr`] whose leaf subplans have already been physically planned.
pub enum PhysicalJoinExpr {
    Input {
        input: PhysicalJoinInput,
        width: usize,
    },
    /// A binary join. When `hash_keys` is non-empty the executor runs a hash
    /// join keyed on them and `predicate` holds only the residual (non-equi)
    /// conjuncts of the ON clause; when empty it runs a nested-loop join and
    /// `predicate` is the whole ON condition (`None` for CROSS/comma joins).
    Join {
        left: Box<PhysicalJoinExpr>,
        right: Box<PhysicalJoinExpr>,
        kind: JoinKind,
        predicate: Option<BoundExpr>,
        hash_keys: Vec<HashKey>,
    },
}

impl PhysicalJoinExpr {
    pub fn width(&self) -> usize {
        match self {
            PhysicalJoinExpr::Input { width, .. } => *width,
            PhysicalJoinExpr::Join { left, right, .. } => left.width() + right.width(),
        }
    }
}

/// The row source of a [`PhysicalPlan::Aggregate`], mirroring [`AggInput`].
pub enum PhysicalAggInput {
    Scan(Arc<dyn TableAm>),
    Join(PhysicalJoinExpr),
    SingleRow,
}

fn plan_join_input(input: JoinInput) -> PhysicalJoinInput {
    match input {
        JoinInput::Scan(table) => PhysicalJoinInput::Scan(table),
        JoinInput::Subplan(source) => PhysicalJoinInput::Subplan(Box::new(plan(*source))),
        JoinInput::TableFunction { func, args } => PhysicalJoinInput::TableFunction { func, args },
    }
}

fn plan_join_expr(source: JoinExpr) -> PhysicalJoinExpr {
    match source {
        JoinExpr::Input { input, width } => PhysicalJoinExpr::Input {
            input: plan_join_input(input),
            width,
        },
        JoinExpr::Join {
            left,
            right,
            kind,
            predicate,
        } => {
            let left_width = left.width();
            let (hash_keys, predicate) = extract_hash_keys(predicate, left_width);
            PhysicalJoinExpr::Join {
                left: Box::new(plan_join_expr(*left)),
                right: Box::new(plan_join_expr(*right)),
                kind,
                predicate,
                hash_keys,
            }
        }
    }
}

/// Split an ON predicate into hash-join keys plus a residual filter.
///
/// The predicate is a boolean over the concatenated `left || right` row, where
/// the left input occupies column indices `[0, left_width)`. Any top-level
/// (AND-connected) conjunct of the form `<left-only expr> = <right-only expr>`
/// (in either operand order) becomes a [`HashKey`]; every other conjunct — a
/// non-equality, a same-side equality (`a.x = a.z`), or one comparing against a
/// constant (`a.x = 5`) — stays in the residual predicate, re-AND-ed together.
///
/// With no extractable key the residual equals the original predicate, so the
/// executor falls back to a nested-loop join with identical semantics.
fn extract_hash_keys(
    predicate: Option<BoundExpr>,
    left_width: usize,
) -> (Vec<HashKey>, Option<BoundExpr>) {
    let Some(predicate) = predicate else {
        return (Vec::new(), None);
    };
    let mut conjuncts = Vec::new();
    flatten_and(predicate, &mut conjuncts);

    let mut hash_keys = Vec::new();
    let mut residual = Vec::new();
    for conjunct in conjuncts {
        match as_equi_key(conjunct, left_width) {
            Ok(key) => hash_keys.push(key),
            Err(other) => residual.push(other),
        }
    }
    (hash_keys, rebuild_and(residual))
}

/// Recursively split a top-level `AND` tree into its conjuncts, preserving order.
fn flatten_and(expr: BoundExpr, out: &mut Vec<BoundExpr>) {
    match expr {
        BoundExpr::Binary {
            op: BinOp::And,
            left,
            right,
            ..
        } => {
            flatten_and(*left, out);
            flatten_and(*right, out);
        }
        other => out.push(other),
    }
}

/// Re-combine conjuncts with `AND`, yielding `None` for an empty list.
fn rebuild_and(mut conjuncts: Vec<BoundExpr>) -> Option<BoundExpr> {
    let mut acc = conjuncts.pop()?;
    while let Some(next) = conjuncts.pop() {
        acc = BoundExpr::Binary {
            op: BinOp::And,
            arg_ty: PgType::Bool,
            collation: DEFAULT_COLLATION_OID,
            left: Box::new(next),
            right: Box::new(acc),
        };
    }
    Some(acc)
}

/// If `conjunct` is a cross-side equality, return its [`HashKey`] (oriented so
/// `left` addresses the left input); otherwise hand the expression back as a
/// residual conjunct.
fn as_equi_key(conjunct: BoundExpr, left_width: usize) -> Result<HashKey, BoundExpr> {
    // Classify on a borrow so the non-key paths can return `conjunct` untouched
    // (no field-by-field rebuild). Only a cross-side equality on a type that
    // hashes distinctly qualifies; a poorly-hashed type (interval/inet/…) would
    // collapse the whole build side into one bucket, so keep it as a residual and
    // let the executor use a nested loop instead.
    // The collation is irrelevant here: every supported collation is
    // deterministic, so equality — and therefore hashing — is bytewise
    // regardless of which one the comparison carries.
    let BoundExpr::Binary {
        op: BinOp::Eq,
        arg_ty,
        left,
        right,
        ..
    } = &conjunct
    else {
        return Err(conjunct);
    };
    if !arg_ty.hashes_distinctly() {
        return Err(conjunct);
    }
    let swap = match (ref_side(left, left_width), ref_side(right, left_width)) {
        (Some(Side::Left), Some(Side::Right)) => false,
        (Some(Side::Right), Some(Side::Left)) => true,
        _ => return Err(conjunct),
    };
    let ty = *arg_ty;
    // Confirmed a cross-side hashable equality — now consume and orient so
    // `left` addresses the left input.
    let BoundExpr::Binary { left, right, .. } = conjunct else {
        unreachable!("matched as an Eq binary above");
    };
    Ok(if swap {
        HashKey {
            left: *right,
            right: *left,
            ty,
        }
    } else {
        HashKey {
            left: *left,
            right: *right,
            ty,
        }
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Left,
    Right,
}

/// Which input a key operand references, or `None` if it spans both inputs or
/// references no column at all (a constant/param — not a join key). Column
/// indices below `left_width` are on the left, the rest on the right; the
/// operand's whole [`BoundExpr::column_ref_bounds`] range must fall on one side.
fn ref_side(expr: &BoundExpr, left_width: usize) -> Option<Side> {
    let (lo, hi) = expr.column_ref_bounds()?;
    if hi < left_width {
        Some(Side::Left)
    } else if lo >= left_width {
        Some(Side::Right)
    } else {
        None
    }
}

pub fn plan(logical: LogicalPlan) -> PhysicalPlan {
    match logical {
        LogicalPlan::Values {
            columns,
            rows,
            predicate,
            sort,
            distinct,
        } => PhysicalPlan::Values {
            columns,
            rows,
            predicate,
            sort,
            distinct,
        },
        LogicalPlan::Query {
            table,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        } => match choose_access(&table, predicate) {
            AccessPath::Index {
                index_name,
                key,
                residual,
            } => PhysicalPlan::IndexScan {
                table,
                index_name,
                key,
                columns,
                projections,
                predicate: residual,
                sort,
                distinct,
            },
            AccessPath::Scan { predicate } => PhysicalPlan::Select {
                table,
                columns,
                projections,
                predicate,
                sort,
                distinct,
            },
        },
        LogicalPlan::Append { tables, columns } => PhysicalPlan::Append { tables, columns },
        LogicalPlan::SetOp {
            arms,
            columns,
            sort,
            distinct,
        } => PhysicalPlan::SetOp {
            arms: arms
                .into_iter()
                .map(|arm| PhysicalSetOpArm {
                    plan: plan(arm.plan),
                    coercion: arm.coercion,
                })
                .collect(),
            columns,
            sort,
            distinct,
        },
        LogicalPlan::Subquery {
            source,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        } => PhysicalPlan::Subquery {
            source: Box::new(plan(*source)),
            columns,
            projections,
            predicate,
            sort,
            distinct,
        },
        LogicalPlan::TableFunction {
            func,
            args,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        } => PhysicalPlan::TableFunction {
            func,
            args,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        },
        LogicalPlan::Join {
            source,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        } => PhysicalPlan::Join {
            source: plan_join_expr(source),
            columns,
            projections,
            predicate,
            sort,
            distinct,
        },
        LogicalPlan::Aggregate {
            input,
            predicate,
            group_exprs,
            aggregates,
            having,
            columns,
            projections,
            sort,
            distinct,
        } => PhysicalPlan::Aggregate {
            input: match input {
                AggInput::Scan(table) => PhysicalAggInput::Scan(table),
                AggInput::Join(source) => PhysicalAggInput::Join(plan_join_expr(source)),
                AggInput::SingleRow => PhysicalAggInput::SingleRow,
            },
            predicate,
            group_exprs,
            aggregates,
            having,
            columns,
            projections,
            sort,
            distinct,
        },
        LogicalPlan::Limit {
            source,
            limit,
            offset,
        } => PhysicalPlan::Limit {
            source: Box::new(plan(*source)),
            limit,
            offset,
        },
        LogicalPlan::Insert {
            table,
            source,
            returning,
            routing,
        } => PhysicalPlan::Insert {
            table,
            source: match source {
                InsertSource::Values(rows) => PhysicalInsertSource::Values(rows),
                InsertSource::Query { input, projections } => PhysicalInsertSource::Query {
                    input: Box::new(plan(*input)),
                    projections,
                },
            },
            returning,
            routing,
        },
        LogicalPlan::Update {
            table,
            predicate,
            assignments,
            returning,
            routing,
        } => PhysicalPlan::Update {
            table,
            predicate,
            assignments,
            returning,
            routing,
        },
        LogicalPlan::Delete {
            table,
            predicate,
            returning,
            routing,
        } => PhysicalPlan::Delete {
            table,
            predicate,
            returning,
            routing,
        },
    }
}

/// The access path chosen for a single-table read.
enum AccessPath {
    /// An equality index probe (see [`PhysicalPlan::IndexScan`]).
    Index {
        index_name: String,
        /// One `(key column, equality value)` pair per index key column, in key
        /// order.
        key: Vec<(usize, BoundExpr)>,
        residual: Option<BoundExpr>,
    },
    /// A full scan carrying the whole (unconsumed) predicate.
    Scan { predicate: Option<BoundExpr> },
}

/// Choose an access path for a `WHERE` predicate: an equality index probe when
/// some index's every key column is pinned by an `col = <constant>` conjunct,
/// else a full scan. PostgreSQL makes this choice by cost; with one real access
/// path here the rule is structural — an equality-covered PK/UNIQUE (or plain)
/// index always beats a sequential scan for a point lookup.
fn choose_access(table: &Arc<dyn TableAm>, predicate: Option<BoundExpr>) -> AccessPath {
    let Some(predicate) = predicate else {
        return AccessPath::Scan { predicate: None };
    };
    let indexes = table.indexes();
    if indexes.is_empty() {
        return AccessPath::Scan {
            predicate: Some(predicate),
        };
    }

    // Flatten the AND-tree and classify each conjunct once.
    let mut conjuncts = Vec::new();
    flatten_and(predicate, &mut conjuncts);
    let eqs: Vec<Option<(usize, BoundExpr)>> = conjuncts.iter().map(as_eq_key).collect();

    // Prefer a PRIMARY KEY, then a UNIQUE index, then any other index.
    for pref in [
        Some(IndexConstraint::PrimaryKey),
        Some(IndexConstraint::Unique),
        None,
    ] {
        for index in indexes.iter().filter(|i| i.constraint == pref) {
            let Some(chosen) = cover_index(index, &eqs) else {
                continue;
            };
            // Only route to an index scan the engine can physically serve;
            // otherwise `EXPLAIN` would advertise an index scan that silently
            // degrades to a sequential scan at execution time.
            if !table.supports_index_scan(&index.name) {
                continue;
            }
            let mut key = Vec::with_capacity(chosen.len());
            let mut consumed = vec![false; conjuncts.len()];
            for (column, conjunct) in chosen {
                // `cover_index` only ever returns conjuncts classified as an
                // equality key, so `eqs[conjunct]` is always `Some`.
                if let Some((_, value)) = &eqs[conjunct] {
                    key.push((column, value.clone()));
                    consumed[conjunct] = true;
                }
            }
            let residual = conjuncts
                .into_iter()
                .enumerate()
                .filter(|(i, _)| !consumed[*i])
                .map(|(_, conjunct)| conjunct)
                .collect();
            return AccessPath::Index {
                index_name: index.name.clone(),
                key,
                residual: rebuild_and(residual),
            };
        }
    }

    AccessPath::Scan {
        predicate: rebuild_and(conjuncts),
    }
}

/// Match each of `index`'s key columns to a distinct equality conjunct, or
/// `None` if any key column has no `= <constant>` conjunct. Returns the
/// `(key column, conjunct index)` pairs in key order.
fn cover_index(
    index: &IndexMetadata,
    eqs: &[Option<(usize, BoundExpr)>],
) -> Option<Vec<(usize, usize)>> {
    let mut used = vec![false; eqs.len()];
    let mut chosen = Vec::with_capacity(index.keys.len());
    for key in &index.keys {
        let conjunct = eqs.iter().enumerate().position(|(i, eq)| {
            !used[i] && matches!(eq, Some((column, _)) if *column == key.column)
        })?;
        used[conjunct] = true;
        chosen.push((key.column, conjunct));
    }
    Some(chosen)
}

/// A conjunct of the form `col = <constant>` (either operand order), returning
/// the column index and the constant value expression. The column side must be
/// a bare [`BoundExpr::ColumnRef`] — a `Coerce` around it means the comparison
/// runs at a different type than the index key, so it is not an index match.
fn as_eq_key(conjunct: &BoundExpr) -> Option<(usize, BoundExpr)> {
    let BoundExpr::Binary {
        op: BinOp::Eq,
        left,
        right,
        ..
    } = conjunct
    else {
        return None;
    };
    match (strip_collate(left), strip_collate(right)) {
        (BoundExpr::ColumnRef { index, .. }, value) if is_row_constant(value) => {
            Some((*index, value.clone()))
        }
        (value, BoundExpr::ColumnRef { index, .. }) if is_row_constant(value) => {
            Some((*index, value.clone()))
        }
        _ => None,
    }
}

/// See through a [`BoundExpr::Collate`] to the expression it labels.
///
/// Safe for an *equality* index probe specifically: every supported collation is
/// deterministic, so two values are equal under a collation exactly when their
/// bytes are equal — which is the order the index is built in. A collation would
/// matter for a range probe, but those are not index-served.
fn strip_collate(expr: &BoundExpr) -> &BoundExpr {
    match expr {
        BoundExpr::Collate { expr, .. } => strip_collate(expr),
        other => other,
    }
}

/// Whether an expression can be hoisted to a **single** evaluation for the whole
/// scan to form the index key. That requires it to reference no column and no
/// bind parameter *and* to be stable — the same value on every row. Function
/// calls are rejected conservatively: the executor evaluates the key exactly
/// once, so hoisting a volatile function (`random()`, `nextval()`) would diverge
/// from the per-row evaluation a `Filter` performs, and we have no purity
/// classification here to tell pure from volatile. A rejected key simply falls
/// back to a sequential scan + filter — correct, just not indexed.
fn is_row_constant(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::Const { .. } => true,
        BoundExpr::Unary { expr, .. }
        | BoundExpr::IsNull { expr, .. }
        | BoundExpr::Coerce { expr, .. }
        | BoundExpr::Collate { expr, .. }
        | BoundExpr::Reinterpret { expr, .. } => is_row_constant(expr),
        BoundExpr::Binary { left, right, .. } => is_row_constant(left) && is_row_constant(right),
        BoundExpr::ArrayCtor { elems, .. } => elems.iter().all(is_row_constant),
        BoundExpr::Subscript { base, index, .. } => {
            is_row_constant(base) && is_row_constant(index)
        }
        BoundExpr::Case { whens, else_, .. } => {
            whens
                .iter()
                .all(|(when, then)| is_row_constant(when) && is_row_constant(then))
                && else_.as_ref().map_or(true, |e| is_row_constant(e))
        }
        // ColumnRef/Param reference per-row/per-execution state; FuncCall/Srf may
        // be volatile; Aggregate never appears in a bindable WHERE key. A subquery
        // is still an unresolved subplan at plan time, so never hoist it as a key.
        // An outer (correlated) reference is only a `Const` after `substitute_outer`
        // rewrites it per outer row; unresolved here it must not be hoisted as a
        // once-evaluated index key.
        BoundExpr::ColumnRef { .. }
        | BoundExpr::Param { .. }
        | BoundExpr::OuterColumnRef { .. }
        | BoundExpr::FuncCall { .. }
        | BoundExpr::Srf { .. }
        | BoundExpr::Aggregate { .. }
        | BoundExpr::ScalarSubquery { .. }
        | BoundExpr::Exists { .. }
        | BoundExpr::QuantifiedSubquery { .. }
        | BoundExpr::QuantifiedArray { .. } => false,
    }
}

/// Render a physical plan as PostgreSQL-style `EXPLAIN` text — one string per
/// output line. This is a **reduced** form: it reproduces PG's node headers
/// (`Seq Scan on t`, `Index Scan using t_pkey on t`, `Index Cond`, `Filter`) so
/// the chosen access path is observable, but omits the cost/row/width estimates
/// PG prints (crabgresql has no cost model). Only the scan paths are rendered in
/// detail; other plan shapes get a single summary line.
///
/// `EXPLAIN ANALYZE` renders the same lines and appends [`explain_summary`]. It
/// adds no per-node `(actual time=… rows=… loops=…)` suffix and no `Buffers:`
/// block, so a node line is a *prefix* of PG's, not a copy of it: a consumer that
/// parses `actual rows=` out of ANALYZE output will not find it here. Only the two
/// footers are byte-identical to PG's.
pub fn explain(plan: &PhysicalPlan) -> Vec<String> {
    match plan {
        PhysicalPlan::Select {
            table, predicate, ..
        } => {
            let schema = table.schema();
            let mut lines = vec![format!("Seq Scan on {}", schema.name)];
            if let Some(predicate) = predicate {
                lines.push(format!("  Filter: ({})", explain_expr(predicate, schema)));
            }
            lines
        }
        PhysicalPlan::IndexScan {
            table,
            index_name,
            key,
            predicate,
            ..
        } => {
            let schema = table.schema();
            let mut lines = vec![format!("Index Scan using {index_name} on {}", schema.name)];
            let cond = key
                .iter()
                .map(|(column, value)| {
                    format!("{} = {}", schema.columns[*column].name, explain_expr(value, schema))
                })
                .collect::<Vec<_>>()
                .join(") AND (");
            lines.push(format!("  Index Cond: ({cond})"));
            if let Some(predicate) = predicate {
                lines.push(format!("  Filter: ({})", explain_expr(predicate, schema)));
            }
            lines
        }
        PhysicalPlan::Values { .. } => vec!["Values Scan".to_string()],
        PhysicalPlan::Append { tables, .. } => {
            // PG's Append over the partitions: one child Seq Scan per leaf, in
            // scan order. A WHERE predicate lives on the wrapping Subquery in this
            // pipeline, so it is not re-rendered per child (reduced EXPLAIN).
            let mut lines = vec!["Append".to_string()];
            for table in tables {
                lines.push(format!("  ->  Seq Scan on {}", table.schema().name));
            }
            lines
        }
        PhysicalPlan::SetOp {
            arms,
            sort,
            distinct,
            ..
        } => {
            // As in PG: an Append over the arms, with the deduplication and the
            // sort shown above it — so a `UNION` is distinguishable from a
            // `UNION ALL`, and the cost of each step is visible.
            let mut lines = vec!["Append".to_string()];
            for arm in arms {
                let mut child_lines = explain(&arm.plan).into_iter();
                if let Some(first) = child_lines.next() {
                    lines.push(format!("  ->  {first}"));
                    lines.extend(child_lines.map(|l| format!("      {l}")));
                }
            }
            if distinct.is_some() {
                lines = nest_under("HashAggregate", lines);
            }
            if !sort.is_empty() {
                lines = nest_under("Sort", lines);
            }
            lines
        }
        PhysicalPlan::Subquery { source, .. } => explain(source),
        PhysicalPlan::TableFunction { .. } => vec!["Function Scan".to_string()],
        PhysicalPlan::Join { .. } => vec!["Nested Loop".to_string()],
        PhysicalPlan::Aggregate { .. } => vec!["Aggregate".to_string()],
        PhysicalPlan::Limit { source, .. } => {
            let mut lines = vec!["Limit".to_string()];
            lines.extend(explain(source).into_iter().map(|l| format!("  {l}")));
            lines
        }
        PhysicalPlan::Insert { table, source, .. } => {
            let mut lines = vec![format!("Insert on {}", table.schema().name)];
            // A query source (`INSERT ... SELECT` / `TABLE t`) has a child plan;
            // render it indented under the Insert, as Limit/Subquery do.
            if let PhysicalInsertSource::Query { input, .. } = source {
                lines.extend(explain(input).into_iter().map(|l| format!("  {l}")));
            }
            lines
        }
        PhysicalPlan::Update { table, .. } => vec![format!("Update on {}", table.schema().name)],
        PhysicalPlan::Delete { table, .. } => vec![format!("Delete on {}", table.schema().name)],
    }
}

/// The trailing summary `EXPLAIN` prints below the plan when `SUMMARY` is on
/// (which `ANALYZE` implies): how long planning took, and — only when the
/// statement actually ran — how long it took to run to completion. A plain
/// `EXPLAIN (SUMMARY ON)` reports planning alone, as PG does. Times are
/// milliseconds to three decimals, as PG prints them.
pub fn explain_summary(planning: Duration, execution: Option<Duration>) -> Vec<String> {
    let mut lines = vec![format!("Planning Time: {} ms", ms(planning))];
    if let Some(execution) = execution {
        lines.push(format!("Execution Time: {} ms", ms(execution)));
    }
    lines
}

/// A duration as PG prints an EXPLAIN time: milliseconds with three decimals.
fn ms(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64() * 1000.0)
}

/// Put `child` under a new parent node, indenting it beneath a `->` lead the way
/// PG renders a single-child plan node.
fn nest_under(parent: &str, child: Vec<String>) -> Vec<String> {
    let mut lines = vec![parent.to_string()];
    let mut child = child.into_iter();
    if let Some(first) = child.next() {
        lines.push(format!("  ->  {first}"));
        lines.extend(child.map(|l| format!("      {l}")));
    }
    lines
}

/// Minimal expression rendering for `EXPLAIN` conditions: enough to show a
/// constant key or a residual comparison, not a faithful SQL deparser. Column
/// references render by name against `schema`.
fn explain_expr(expr: &BoundExpr, schema: &TableSchema) -> String {
    match expr {
        BoundExpr::Const { value, .. } => value
            .encode_text_with(1)
            .unwrap_or_else(|| "NULL".to_string()),
        BoundExpr::ColumnRef { index, .. } => schema
            .columns
            .get(*index)
            .map_or_else(|| format!("${index}"), |c| c.name.clone()),
        BoundExpr::Param { index, .. } => format!("${}", index + 1),
        BoundExpr::Coerce { expr, .. } | BoundExpr::Reinterpret { expr, .. } => {
            explain_expr(expr, schema)
        }
        BoundExpr::Binary {
            op, left, right, ..
        } => format!(
            "{} {} {}",
            explain_expr(left, schema),
            op.sql_symbol(),
            explain_expr(right, schema)
        ),
        _ => "…".to_string(),
    }
}

#[cfg(test)]
mod tests {
    //! SQL in, physical plan out: parse → bind (against a memory table) →
    //! plan, asserting on the plan's structure.

    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use crabgresql_binder::{BinOp, bind_delete, bind_insert, bind_query, bind_update};
    use crabgresql_parser::ast;
    use crabgresql_storage_api::{
        Column, DeleteResult, EmptyTypeCatalog, IndexKey, IndexMethod, StorageError, TableEngine,
        TableSchema, Tid, Tuple, TypeCatalog, UpdateResult,
    };
    use crabgresql_txn::TxnContext;
    use crabgresql_types::{PgType, Value};

    /// A metadata-only engine for planner tests: it holds table schemas and their
    /// index metadata and reports `supports_index_scan = true`, so the planner's
    /// index-selection path is exercised without a real storage engine. It never
    /// stores rows — the planner only reads schema/index metadata, so every
    /// row-touching method is `unimplemented!()`.
    #[derive(Default)]
    struct MetaEngine {
        tables: Mutex<HashMap<String, Arc<MetaTable>>>,
    }

    struct MetaTable {
        schema: TableSchema,
        indexes: Mutex<Vec<IndexMetadata>>,
    }

    impl TableAm for MetaTable {
        fn schema(&self) -> &TableSchema {
            &self.schema
        }
        fn indexes(&self) -> Vec<IndexMetadata> {
            self.indexes.lock().expect("mutex").clone()
        }
        fn supports_index_scan(&self, _index_name: &str) -> bool {
            true
        }
        fn scan(&self, _txn: &TxnContext) -> Box<dyn Iterator<Item = (Tid, Tuple)> + Send> {
            unimplemented!("planner tests never scan")
        }
        fn fetch(&self, _tid: Tid, _txn: &TxnContext) -> Option<Tuple> {
            unimplemented!("planner tests never fetch")
        }
        fn insert(&self, _tuple: Tuple, _txn: &TxnContext) -> Tid {
            unimplemented!("planner tests never insert")
        }
        fn update(&self, _tid: Tid, _tuple: Tuple, _txn: &TxnContext) -> UpdateResult {
            unimplemented!("planner tests never update")
        }
        fn delete(&self, _tid: Tid, _txn: &TxnContext) -> DeleteResult {
            unimplemented!("planner tests never delete")
        }
    }

    impl TableEngine for MetaEngine {
        fn create_table(&self, schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError> {
            let table = Arc::new(MetaTable {
                schema: schema.clone(),
                indexes: Mutex::new(Vec::new()),
            });
            self.tables
                .lock()
                .expect("mutex")
                .insert(schema.name.clone(), Arc::clone(&table));
            Ok(table as Arc<dyn TableAm>)
        }
        fn open_table(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
            self.tables
                .lock()
                .expect("mutex")
                .get(name)
                .cloned()
                .map(|t| t as Arc<dyn TableAm>)
                .ok_or_else(|| StorageError::TableNotFound(name.to_string()))
        }
        fn drop_table(&self, _namespace: &str, name: &str) -> Result<(), StorageError> {
            self.tables
                .lock()
                .expect("mutex")
                .remove(name)
                .map(|_| ())
                .ok_or_else(|| StorageError::TableNotFound(name.to_string()))
        }
        fn create_index(
            &self,
            _namespace: &str,
            table: &str,
            index: IndexMetadata,
        ) -> Result<(), StorageError> {
            let tables = self.tables.lock().expect("mutex");
            let t = tables
                .get(table)
                .ok_or_else(|| StorageError::IndexTableNotFound(table.to_string()))?;
            t.indexes.lock().expect("mutex").push(index);
            Ok(())
        }
    }

    fn plan_sql(sql: &str) -> PhysicalPlan {
        plan_sql_indexed(sql, None)
    }

    /// Plan `sql` against table `t(id int4, big int8, name text)`, optionally
    /// registering an index (name + `IndexMetadata`) first.
    fn plan_sql_indexed(sql: &str, index: Option<IndexMetadata>) -> PhysicalPlan {
        let engine: Arc<dyn TableEngine> = Arc::new(MetaEngine::default());
        let catalog: Arc<dyn TypeCatalog> = Arc::new(EmptyTypeCatalog);
        if let Err(error) = engine.create_table(TableSchema::in_namespace(
            "t",
            "public",
            vec![
                Column::new("id", PgType::Int4),
                Column::new("big", PgType::Int8),
                Column::new("name", PgType::Text),
            ],
        )) {
            panic!("failed to create planner test table: {error}");
        }
        if let Some(index) = index
            && let Err(error) = engine.create_index("public", "t", index)
        {
            panic!("failed to create planner test index: {error}");
        }
        let stmts = match crabgresql_parser::parse(sql) {
            Ok(stmts) => stmts,
            Err(error) => panic!("invalid SQL test fixture `{sql}`: {error}"),
        };
        let logical = match match &stmts[0] {
            ast::Statement::Query(q) => bind_query(&engine, &catalog, q),
            ast::Statement::Insert(i) => bind_insert(&engine, &catalog, i),
            ast::Statement::Update(u) => bind_update(&engine, &catalog, u),
            ast::Statement::Delete(d) => bind_delete(&engine, &catalog, d),
            other => panic!("unexpected statement: {other}"),
        } {
            Ok(plan) => plan,
            Err(error) => panic!("failed to bind planner test SQL `{sql}`: {error}"),
        };
        plan(logical)
    }

    #[test]
    fn select_one_becomes_values() {
        let PhysicalPlan::Values {
            columns,
            rows,
            predicate,
            ..
        } = plan_sql("SELECT 1")
        else {
            panic!("expected Values");
        };
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "?column?");
        assert_eq!(columns[0].ty, PgType::Int4);
        assert_eq!(
            rows,
            vec![vec![BoundExpr::Const {
                value: Value::Int4(1),
                ty: PgType::Int4
            }]]
        );
        assert!(predicate.is_none());
    }

    #[test]
    fn filtered_scan_becomes_select_with_predicate() {
        let PhysicalPlan::Select {
            columns,
            projections,
            predicate,
            ..
        } = plan_sql("SELECT * FROM t WHERE id = 1")
        else {
            panic!("expected Select");
        };
        // `*` expands to one ColumnRef per schema column.
        assert_eq!(columns.len(), 3);
        assert_eq!(
            projections[0],
            BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4
            }
        );
        let Some(BoundExpr::Binary {
            op: BinOp::Eq,
            arg_ty: PgType::Int4,
            ..
        }) = predicate
        else {
            panic!("expected int4 equality predicate");
        };
    }

    #[test]
    fn expression_projection_is_named_column() {
        let PhysicalPlan::Select {
            columns,
            projections,
            ..
        } = plan_sql("SELECT id + 1 FROM t")
        else {
            panic!("expected Select");
        };
        assert_eq!(columns[0].name, "?column?");
        assert_eq!(columns[0].ty, PgType::Int4);
        let BoundExpr::Binary {
            op: BinOp::Add,
            arg_ty: PgType::Int4,
            ..
        } = &projections[0]
        else {
            panic!("expected addition projection");
        };
    }

    #[test]
    fn mixed_width_comparison_coerces_int4_side() {
        let PhysicalPlan::Select { predicate, .. } = plan_sql("SELECT id FROM t WHERE id < big")
        else {
            panic!("expected Select");
        };
        let Some(BoundExpr::Binary {
            op: BinOp::Lt,
            arg_ty: PgType::Int8,
            left,
            ..
        }) = predicate
        else {
            panic!("expected int8 comparison");
        };
        let BoundExpr::Coerce {
            ty: PgType::Int8, ..
        } = *left
        else {
            panic!("expected Coerce around the int4 side");
        };
    }

    #[test]
    fn update_plan_carries_indexed_assignments() {
        let PhysicalPlan::Update {
            assignments,
            predicate,
            ..
        } = plan_sql("UPDATE t SET name = 'x' WHERE id = 2")
        else {
            panic!("expected Update");
        };
        assert_eq!(
            assignments,
            vec![(
                2,
                BoundExpr::Const {
                    value: Value::Text("x".into()),
                    ty: PgType::Text
                }
            )]
        );
        assert!(predicate.is_some());
    }

    #[test]
    fn unfiltered_delete_has_no_predicate() {
        let PhysicalPlan::Delete { predicate, .. } = plan_sql("DELETE FROM t") else {
            panic!("expected Delete");
        };
        assert!(predicate.is_none());
    }

    #[test]
    fn cross_join_maps_to_physical_join() {
        let PhysicalPlan::Join {
            source, columns, ..
        } = plan_sql("SELECT * FROM t, (VALUES (1)) v(x)")
        else {
            panic!("expected Join");
        };
        assert!(matches!(
            source,
            PhysicalJoinExpr::Join {
                kind: JoinKind::Cross,
                predicate: None,
                ..
            }
        ));
        // t's three columns plus v's one.
        assert_eq!(columns.len(), 4);
    }

    #[test]
    fn outer_join_and_aggregate_input_map_recursively() {
        let PhysicalPlan::Aggregate {
            input: PhysicalAggInput::Join(source),
            ..
        } = plan_sql("SELECT count(*) FROM t a FULL JOIN t b ON a.id = b.id")
        else {
            panic!("expected Aggregate over Join");
        };
        // `a.id = b.id` is a cross-side equality, so it lowers to a hash join
        // with one key and no residual predicate.
        let PhysicalJoinExpr::Join {
            kind,
            predicate,
            hash_keys,
            ..
        } = source
        else {
            panic!("expected Join");
        };
        assert_eq!(kind, JoinKind::Full);
        assert!(predicate.is_none());
        assert_eq!(hash_keys.len(), 1);
    }

    #[test]
    fn equi_join_extracts_hash_key_and_leaves_residual() {
        // `a.id = b.id` is the sole key; `a.big > b.big` is a non-equi residual.
        let PhysicalPlan::Join { source, .. } =
            plan_sql("SELECT * FROM t a INNER JOIN t b ON a.id = b.id AND a.big > b.big")
        else {
            panic!("expected Join");
        };
        let PhysicalJoinExpr::Join {
            kind,
            predicate,
            hash_keys,
            ..
        } = source
        else {
            panic!("expected Join node");
        };
        assert_eq!(kind, JoinKind::Inner);
        assert_eq!(hash_keys.len(), 1);
        assert_eq!(hash_keys[0].ty, PgType::Int4);
        // The residual keeps the `>` comparison as a single conjunct.
        assert!(matches!(
            predicate,
            Some(BoundExpr::Binary { op: BinOp::Gt, .. })
        ));
    }

    #[test]
    fn non_equi_and_constant_equalities_stay_nested_loop() {
        // `a.id = 1` compares a column to a constant (a filter, not a join key),
        // and `a.big < b.big` is non-equi: neither yields a hash key.
        let PhysicalPlan::Join { source, .. } =
            plan_sql("SELECT * FROM t a INNER JOIN t b ON a.id = 1 AND a.big < b.big")
        else {
            panic!("expected Join");
        };
        let PhysicalJoinExpr::Join {
            predicate,
            hash_keys,
            ..
        } = source
        else {
            panic!("expected Join node");
        };
        assert!(hash_keys.is_empty());
        assert!(predicate.is_some());
    }

    #[test]
    fn equi_join_on_poorly_hashed_type_stays_nested_loop() {
        // `interval` equality is orderable (so the join binds) but agg::hash_key
        // can't distinguish interval values — they'd all share one bucket — so the
        // planner must keep it as a nested-loop predicate, not a hash key.
        let PhysicalPlan::Join { source, .. } = plan_sql(
            "SELECT * FROM (VALUES ('1 day'::interval)) a(x) \
             JOIN (VALUES ('1 day'::interval)) b(y) ON a.x = b.y",
        ) else {
            panic!("expected Join");
        };
        let PhysicalJoinExpr::Join {
            predicate,
            hash_keys,
            ..
        } = source
        else {
            panic!("expected Join node");
        };
        assert!(hash_keys.is_empty(), "interval key must not be hashed");
        assert!(predicate.is_some());
    }

    #[test]
    fn equi_join_on_hashable_nonint_type_extracts_key() {
        // `money` compares by its raw i64 and is now hashed distinctly, so an
        // equality on it is a hash key.
        let PhysicalPlan::Join { source, .. } = plan_sql(
            "SELECT * FROM (VALUES ('$1'::money)) a(x) \
             JOIN (VALUES ('$1'::money)) b(y) ON a.x = b.y",
        ) else {
            panic!("expected Join");
        };
        let PhysicalJoinExpr::Join { hash_keys, .. } = source else {
            panic!("expected Join node");
        };
        assert_eq!(hash_keys.len(), 1);
        assert_eq!(hash_keys[0].ty, PgType::Money);
    }

    #[test]
    fn aggregate_query_maps_to_physical_aggregate() {
        let PhysicalPlan::Aggregate {
            group_exprs,
            aggregates,
            projections,
            ..
        } = plan_sql("SELECT count(*) FROM t")
        else {
            panic!("expected Aggregate");
        };
        assert!(group_exprs.is_empty());
        assert_eq!(aggregates.len(), 1);
        assert_eq!(
            projections,
            vec![BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int8
            }]
        );
    }

    #[test]
    fn grouped_aggregate_maps_to_physical_aggregate() {
        let PhysicalPlan::Aggregate {
            group_exprs,
            aggregates,
            ..
        } = plan_sql("SELECT id, count(*) FROM t GROUP BY id")
        else {
            panic!("expected Aggregate");
        };
        assert_eq!(group_exprs.len(), 1);
        assert_eq!(aggregates.len(), 1);
    }

    #[test]
    fn insert_rows_are_prebound_constants() {
        let PhysicalPlan::Insert { source, .. } = plan_sql("INSERT INTO t (id) VALUES (1), (2)")
        else {
            panic!("expected Insert");
        };
        let PhysicalInsertSource::Values(rows) = source else {
            panic!("expected a VALUES source");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[1][0],
            BoundExpr::Const {
                value: Value::Int4(2),
                ty: PgType::Int4
            }
        );
    }

    /// A PRIMARY KEY index on `t.id` (column 0).
    fn pk_on_id() -> IndexMetadata {
        IndexMetadata {
            name: "t_pkey".into(),
            method: IndexMethod::BTree,
            keys: vec![IndexKey {
                column: 0,
                descending: false,
                nulls_first: false,
            }],
            unique: true,
            nulls_distinct: true,
            constraint: Some(IndexConstraint::PrimaryKey),
        }
    }

    #[test]
    fn equality_on_pk_becomes_index_scan() {
        let plan = plan_sql_indexed("SELECT * FROM t WHERE id = 1", Some(pk_on_id()));
        let PhysicalPlan::IndexScan {
            index_name,
            key,
            predicate,
            ..
        } = plan
        else {
            panic!("expected IndexScan");
        };
        assert_eq!(index_name, "t_pkey");
        assert_eq!(
            key,
            vec![(
                0,
                BoundExpr::Const {
                    value: Value::Int4(1),
                    ty: PgType::Int4
                }
            )]
        );
        // The equality conjunct is fully consumed by the index.
        assert!(predicate.is_none());
    }

    #[test]
    fn extra_conjunct_stays_as_residual_filter() {
        let plan = plan_sql_indexed(
            "SELECT * FROM t WHERE id = 1 AND name = 'x'",
            Some(pk_on_id()),
        );
        let PhysicalPlan::IndexScan { key, predicate, .. } = plan else {
            panic!("expected IndexScan");
        };
        assert_eq!(key.len(), 1);
        assert_eq!(key[0].0, 0);
        // `name = 'x'` is not on the index key, so it remains a runtime filter.
        let Some(BoundExpr::Binary {
            op: BinOp::Eq,
            arg_ty: PgType::Text,
            ..
        }) = predicate
        else {
            panic!("expected a residual text-equality filter");
        };
    }

    #[test]
    fn equality_on_unindexed_column_stays_seq_scan() {
        let plan = plan_sql_indexed("SELECT * FROM t WHERE name = 'x'", Some(pk_on_id()));
        assert!(
            matches!(plan, PhysicalPlan::Select { .. }),
            "unindexed column must not use an index scan"
        );
    }

    #[test]
    fn range_on_pk_stays_seq_scan() {
        // Equality only: a range predicate cannot be served by the hash index.
        let plan = plan_sql_indexed("SELECT * FROM t WHERE id > 1", Some(pk_on_id()));
        assert!(matches!(plan, PhysicalPlan::Select { .. }));
    }

    #[test]
    fn equality_without_index_stays_seq_scan() {
        let plan = plan_sql_indexed("SELECT * FROM t WHERE id = 1", None);
        assert!(matches!(plan, PhysicalPlan::Select { .. }));
    }

    #[test]
    fn explain_renders_index_scan_and_seq_scan() {
        let index_plan = plan_sql_indexed("SELECT * FROM t WHERE id = 1", Some(pk_on_id()));
        let lines = explain(&index_plan);
        assert_eq!(lines[0], "Index Scan using t_pkey on t");
        assert_eq!(lines[1], "  Index Cond: (id = 1)");

        let seq_plan = plan_sql_indexed("SELECT * FROM t WHERE name = 'x'", Some(pk_on_id()));
        let lines = explain(&seq_plan);
        assert_eq!(lines[0], "Seq Scan on t");
        assert_eq!(lines[1], "  Filter: (name = x)");
    }

    #[test]
    fn explain_append_lists_a_seq_scan_per_partition() {
        // A partitioned-parent union scan renders as an `Append` with one child
        // `Seq Scan` per leaf, in leaf (scan) order.
        let engine: Arc<dyn TableEngine> = Arc::new(MetaEngine::default());
        let leaf = |name: &str| {
            engine
                .create_table(TableSchema::in_namespace(
                    name,
                    "public",
                    vec![Column::new("id", PgType::Int4)],
                ))
                .expect("create leaf")
        };
        let plan = PhysicalPlan::Append {
            tables: vec![leaf("sales_2023"), leaf("sales_2024")],
            columns: vec![OutputColumn::new("id", PgType::Int4)],
        };
        assert_eq!(
            explain(&plan),
            vec![
                "Append".to_string(),
                "  ->  Seq Scan on sales_2023".to_string(),
                "  ->  Seq Scan on sales_2024".to_string(),
            ]
        );
    }

    /// A three-armed set operation with the given tail, for EXPLAIN tests.
    fn setop_plan(sort: Vec<SortKey>, distinct: Option<Vec<DistinctKey>>) -> PhysicalPlan {
        let columns = vec![OutputColumn::new("id", PgType::Int4)];
        PhysicalPlan::SetOp {
            arms: (0..3)
                .map(|_| PhysicalSetOpArm {
                    plan: plan_sql("SELECT * FROM t"),
                    coercion: None,
                })
                .collect(),
            columns,
            sort,
            distinct,
        }
    }

    #[test]
    fn explain_union_all_renders_as_append_over_every_arm() {
        let lines = explain(&setop_plan(Vec::new(), None));
        assert_eq!(lines[0], "Append");
        assert_eq!(
            lines.iter().filter(|l| l.starts_with("  ->  ")).count(),
            3,
            "expected one child lead per arm, got: {lines:?}"
        );
    }

    #[test]
    fn explain_shows_the_dedup_and_sort_above_the_append() {
        // A UNION must be distinguishable from a UNION ALL, and the cost of
        // deduplicating and sorting visible — as in PG's Sort/HashAggregate/Append.
        let distinct = Some(vec![DistinctKey {
            column: 0,
            ty: PgType::Int4,
        }]);
        let sort = vec![SortKey {
            column: 0,
            ty: PgType::Int4,
            collation: DEFAULT_COLLATION_OID,
            asc: true,
            nulls_first: false,
        }];
        assert_eq!(explain(&setop_plan(Vec::new(), distinct.clone()))[0..2], [
            "HashAggregate".to_string(),
            "  ->  Append".to_string()
        ]);
        assert_eq!(explain(&setop_plan(sort, distinct))[0..3], [
            "Sort".to_string(),
            "  ->  HashAggregate".to_string(),
            "        ->  Append".to_string()
        ]);
    }

    #[test]
    fn explain_insert_select_shows_source_subtree() {
        // A query-source INSERT renders its child plan indented under the Insert
        // node, rather than hiding it behind a bare `Insert on t`.
        let plan = plan_sql("INSERT INTO t SELECT * FROM t");
        let lines = explain(&plan);
        assert_eq!(lines[0], "Insert on t");
        assert!(
            lines.iter().skip(1).any(|l| l.contains("Seq Scan on t")),
            "expected the source scan under Insert, got: {lines:?}"
        );
    }

    #[test]
    fn explain_summary_renders_times_in_milliseconds() {
        // PG's ANALYZE footers: milliseconds to three decimals, planning first.
        assert_eq!(
            explain_summary(
                Duration::from_nanos(87_000),
                Some(Duration::from_nanos(93_400))
            ),
            vec![
                "Planning Time: 0.087 ms".to_string(),
                "Execution Time: 0.093 ms".to_string()
            ]
        );
    }

    #[test]
    fn explain_summary_omits_execution_time_when_the_statement_did_not_run() {
        // A plain `EXPLAIN (SUMMARY ON)` reports planning alone — there is no
        // execution to time.
        assert_eq!(
            explain_summary(Duration::from_nanos(59_000), None),
            vec!["Planning Time: 0.059 ms".to_string()]
        );
    }

    #[test]
    fn explain_summary_rounds_to_three_decimals() {
        // Just under a millisecond still rounds up to `1.000`, and a sub-nanosecond
        // measurement prints `0.000` rather than an empty or exponent form.
        let lines = explain_summary(Duration::from_nanos(999_600), Some(Duration::ZERO));
        assert_eq!(lines[0], "Planning Time: 1.000 ms");
        assert_eq!(lines[1], "Execution Time: 0.000 ms");
    }
}
