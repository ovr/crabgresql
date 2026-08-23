//! Planner: logical plan → physical plan.
//!
//! Every choice made here is structural rather than cost-based: an equality
//! index probe when one index's key columns are all pinned by the `WHERE`, a
//! hash join when a cross-side equality can be extracted, predicate and
//! column-projection pushdown, and sinking a filter's correlated-subquery
//! conjunct behind the cheap conjuncts it may legally cross.
//!
//! TODO: join ordering and cost-based path selection, both of which need the
//! table statistics this layer has no source for (docs/ARCHITECTURE.md §3).

pub mod cost;
mod projection;
mod pushdown;
mod qualorder;
pub mod vectorize;

use std::sync::Arc;
use std::time::Duration;

use crabgresql_binder::{
    AggInput, AggregatePlan, AppendPlan, BinOp, BoundAggregate, BoundExpr, BoundWindowFunc,
    BoundWindowSpec, DeletePlan, DistinctKey, InsertPlan, InsertSource, JoinExpr, JoinInput,
    JoinKind, JoinPlan, LimitPlan, LogicalPlan, MappedRelation, OutputColumn, QueryPlan,
    RelationIdent, Returning, SetOpPlan, SortKey, SubqueryPlan, SysCol, SystemEmit, TableFn,
    TableFunctionPlan, UpdatePlan, ValuesPlan, WindowPlan,
};
use crabgresql_storage_api::{
    ColumnProjection, IndexConstraint, IndexMetadata, RelStats, TableAm, TableSchema, Tuple,
};
use crabgresql_types::collation::DEFAULT_COLLATION_OID;
use crabgresql_types::{PgType, Value};

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
        /// The columns this scan's own expressions read, for engines that can
        /// skip the rest (see [`projection`]). Rows stay full width regardless.
        projection: ColumnProjection,
        /// The system columns this scan appends past the relation's declared
        /// ones; see [`JoinInput::Scan`](crabgresql_binder::JoinInput::Scan).
        system: Option<SystemEmit>,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
        distinct: Option<Vec<DistinctKey>>,
    },
    /// A single-table read served by a probe on `index_name`: the executor
    /// evaluates each `key` value once and asks the engine for the matching
    /// rows. The planner only emits this when the engine reports
    /// [`TableAm::supports_index_scan`], but the executor still scan-fallbacks
    /// defensively. `predicate` is the residual WHERE the index did not consume,
    /// applied as a `Filter`; the standard Projection → Sort tail follows,
    /// exactly as for [`Self::Select`].
    IndexScan {
        table: Arc<dyn TableAm>,
        /// As for [`Self::Select`], and additionally always covering every
        /// `key` column: the executor's scan fallback re-checks the key per row.
        projection: ColumnProjection,
        /// As for [`Self::Select`].
        system: Option<SystemEmit>,
        index_name: String,
        /// What to search for. The value expressions are row-constant and
        /// evaluated once.
        key: IndexProbeSpec,
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
        ordinality: bool,
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
    /// Union scan over the several relations one FROM item names. Mirrors
    /// [`LogicalPlan::Append`]: the executor concatenates each arm's scan into
    /// one row stream. Such a FROM item is planned as a [`Self::Subquery`]
    /// wrapping this, so the standard projection/predicate/sort tail runs on top.
    Append {
        arms: Vec<PhysicalAppendArm>,
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
    /// One step of window-function evaluation. Mirrors [`LogicalPlan::Window`]:
    /// the executor materializes `source`, sorts it by `spec`'s partition keys
    /// then its ORDER BY keys, and fills each of `funcs` into the slot it names.
    /// A window query is planned as a [`Self::Subquery`] wrapping the chain, so
    /// the standard projection/sort tail runs on top.
    Window {
        source: Box<PhysicalPlan>,
        spec: BoundWindowSpec,
        funcs: Vec<BoundWindowFunc>,
        input_width: usize,
        output_width: usize,
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
        /// `COPY … FREEZE` (see [`LogicalPlan::Insert`]): the executor freezes
        /// this target's write and nothing else.
        freeze: bool,
        /// The system columns each inserted row carries as trailing slots for
        /// RETURNING to read (see [`LogicalPlan::Insert`]). The executor fills
        /// a `tableoid` with the leaf the row was routed to.
        system: Arc<[SysCol]>,
    },
    Update {
        /// The system columns each row carries as trailing slots for WHERE, SET
        /// or RETURNING to read (see [`LogicalPlan::Insert`]).
        system: Arc<[SysCol]>,
        table: Arc<dyn TableAm>,
        predicate: Option<BoundExpr>,
        assignments: Vec<(usize, BoundExpr)>,
        returning: Option<Returning>,
        /// Leaf partitions for tuple routing when `table` is a partitioned parent
        /// (see [`LogicalPlan::Update`]); `None` for an ordinary table.
        routing: Option<Vec<DmlTarget>>,
        /// `table` and its inheritance descendants, each with its column map
        /// (see [`LogicalPlan::Update`]); empty for a table with no children.
        inherited: Vec<DmlTarget>,
        /// The row source for `table` itself, used when it is neither partitioned
        /// nor inherited (the other two arms carry their own).
        probe: Option<DmlIndexProbe>,
    },
    Delete {
        /// The system columns each row carries as trailing slots for WHERE or
        /// RETURNING to read (see [`LogicalPlan::Insert`]).
        system: Arc<[SysCol]>,
        table: Arc<dyn TableAm>,
        predicate: Option<BoundExpr>,
        returning: Option<Returning>,
        /// Leaf partitions for tuple routing when `table` is a partitioned parent
        /// (see [`LogicalPlan::Delete`]); `None` for an ordinary table.
        routing: Option<Vec<DmlTarget>>,
        /// `table` and its inheritance descendants, each with its column map
        /// (see [`LogicalPlan::Delete`]); empty for a table with no children.
        inherited: Vec<DmlTarget>,
        /// The row source for `table` itself, used when it is neither partitioned
        /// nor inherited (the other two arms carry their own).
        probe: Option<DmlIndexProbe>,
    },
}

/// One relation an `UPDATE`/`DELETE` reads rows from, with the row source chosen
/// for it.
///
/// The probe travels *with* its relation rather than in a vector alongside one,
/// so the two cannot fall out of step — the same reason [`PhysicalAppendArm`]
/// embeds its [`MappedRelation`] instead of running parallel to it. A positional
/// pairing would survive partition pruning skipping a leaf, and read leaf B
/// through leaf A's index.
pub struct DmlTarget {
    pub relation: MappedRelation,
    /// `None` scans the whole relation.
    pub probe: Option<DmlIndexProbe>,
}

/// One end of an index range: the column it bounds, the value, and whether the
/// bound itself matches. Mirrors [`crabgresql_storage_api::IndexBound`], whose
/// value the executor produces by evaluating `value` once.
#[derive(Clone)]
pub struct IndexBoundExpr {
    pub column: usize,
    pub value: BoundExpr,
    pub inclusive: bool,
}

/// What an index probe searches for: leading key columns pinned by equality,
/// plus optional bounds on the one column after them.
///
/// The shape follows what a B-tree can serve and what
/// [`crabgresql_storage_api::IndexProbeKey`] accepts — `eq` need not cover every
/// key column (an index on `(a, b)` probed by `a` alone narrows the scan just
/// fine), and it may be empty when only the first key column is bounded.
#[derive(Clone)]
pub struct IndexProbeSpec {
    /// One `(key column, equality value)` pair per pinned key column, in key
    /// order, covering a *prefix* of the index's keys.
    pub eq: Vec<(usize, BoundExpr)>,
    /// Boxed because most probes have no bounds at all, and a `BoundExpr` held
    /// inline twice over would widen every `PhysicalPlan` in the tree.
    pub lower: Option<Box<IndexBoundExpr>>,
    pub upper: Option<Box<IndexBoundExpr>>,
}

impl IndexProbeSpec {
    /// The leading key columns pinned by equality and nothing else — the shape
    /// every probe had before ranges existed. Mirrors
    /// [`crabgresql_storage_api::IndexProbeKey::equality`], which the executor
    /// turns this into.
    pub fn equality(eq: Vec<(usize, BoundExpr)>) -> Self {
        IndexProbeSpec {
            eq,
            lower: None,
            upper: None,
        }
    }

    /// Every column the probe reads, in key order — what a scan fallback has to
    /// re-check per row, and so what the projection pass must keep.
    pub fn columns(&self) -> impl Iterator<Item = usize> + '_ {
        self.eq
            .iter()
            .map(|(column, _)| *column)
            .chain(self.lower.iter().chain(&self.upper).map(|b| b.column))
    }

    /// Every value expression the probe evaluates, in the same order.
    pub fn exprs(&self) -> impl Iterator<Item = &BoundExpr> + '_ {
        self.eq
            .iter()
            .map(|(_, value)| value)
            .chain(self.lower.iter().chain(&self.upper).map(|b| &b.value))
    }

    pub fn exprs_mut(&mut self) -> impl Iterator<Item = &mut BoundExpr> + '_ {
        self.eq.iter_mut().map(|(_, value)| value).chain(
            self.lower
                .iter_mut()
                .chain(&mut self.upper)
                .map(|b| &mut b.value),
        )
    }
}

/// An index probe standing in for one DML target's sequential scan.
///
/// Unlike [`PhysicalPlan::IndexScan`], a probe here does *not* consume the
/// conjuncts it matched: the plan's `predicate` stays the whole `WHERE` and is
/// re-checked per row. The probe only narrows the row source, so a target that
/// cannot serve one falls back to a scan without any predicate rewriting — which
/// is what lets each inheritance descendant and each leaf partition decide
/// independently, in its own column space.
pub struct DmlIndexProbe {
    pub index_name: String,
    /// The columns are ordinals in the *target's* own schema, already
    /// translated through [`MappedRelation::map`] where one applies.
    pub key: IndexProbeSpec,
    /// The conjuncts the key did *not* cover, for `EXPLAIN` only.
    ///
    /// The executor re-checks the whole `WHERE`, so this never drives execution;
    /// it exists so a plan can show the same `Index Cond` / `Filter` split PG
    /// shows. Re-checking a conjunct the index already satisfied is not
    /// observable, which is what lets display and execution differ here.
    pub residual: Option<BoundExpr>,
}

/// One arm of a [`PhysicalPlan::Append`]: a [`MappedRelation`] plus the columns
/// its scan must materialize.
///
/// The relation is embedded rather than flattened so the permutation has one
/// definition — the executor reads a row through [`MappedRelation::view`]
/// instead of open-coding the same indexing a second time.
///
/// The projection is per-arm rather than shared: with a remap in play, an
/// ordinal in one arm's schema names a different column in another's, so a
/// single [`ColumnProjection`] could not be right for both.
pub struct PhysicalAppendArm {
    pub relation: MappedRelation,
    /// Which of this arm's own columns the scan must materialize. Supplied by
    /// the wrapping [`PhysicalPlan::Subquery`], which owns the expressions that
    /// read these rows, translated through the map into this arm's ordinals.
    pub projection: ColumnProjection,
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
    /// Rows whose cells are already values, mirroring [`InsertSource::Tuples`].
    /// `defaults` names the columns whose `DEFAULT` still needs evaluating once
    /// per row, and `notnull_verified` the columns the builder already proved
    /// non-NULL in every row.
    Tuples {
        rows: Vec<Tuple>,
        defaults: Vec<(usize, BoundExpr)>,
        notnull_verified: Vec<u32>,
    },
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
    Scan {
        table: Arc<dyn TableAm>,
        /// The columns this leaf's own row supplies to the join tree above it,
        /// in the leaf's base-0 space (see [`projection`]).
        projection: ColumnProjection,
        /// The system columns the leaf appends past the relation's declared
        /// ones. They are not in `projection`, which addresses stored columns.
        system: Option<SystemEmit>,
    },
    Subplan(Box<PhysicalPlan>),
    TableFunction {
        func: TableFn,
        args: Vec<BoundExpr>,
        ordinality: bool,
    },
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
        /// Single-relation conjuncts the planner sank to this leaf, applied
        /// above the source before any join sees the row. Indexes the leaf's own
        /// row, so a right child's conjuncts are rebased on the way down.
        predicate: Option<BoundExpr>,
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
    /// A join whose right side is a `LATERAL` FROM item: it reads the left row,
    /// so it produces *different rows for every one of them*.
    ///
    /// That is why `right` is still a [`JoinInput`] and not a planned node —
    /// its expressions hold `OuterColumnRef { level: 1 }` slots that only exist
    /// once a left row does, so the executor fills them and plans it per row.
    /// Keeping the shape distinct also puts the two consequences beyond reach
    /// of a missed condition: there is nowhere to hang hash keys, and nothing
    /// materializes the right side once.
    Lateral {
        left: Box<PhysicalJoinExpr>,
        right: JoinInput,
        /// The same item planned once, with its `OuterColumnRef`s left standing.
        ///
        /// **Never executed**, and not the plan any row is produced by: the
        /// executor builds that from `right` per left row. This copy exists for
        /// the passes that *inspect* a plan and have no row to substitute —
        /// [`scan_projections`], which feeds `pg_depend`'s per-column view
        /// dependencies, and `EXPLAIN`. Without it a relation read only inside a
        /// lateral item is invisible to both.
        ///
        /// `None` for a table function, which scans no relation and has no
        /// subtree to show.
        right_shape: Option<Box<PhysicalPlan>>,
        right_width: usize,
        /// `Cross`, `Inner` or `Left` — PostgreSQL refuses a lateral reference
        /// across a `RIGHT`/`FULL` join, and the binder does too.
        kind: JoinKind,
        predicate: Option<BoundExpr>,
    },
}

impl PhysicalJoinExpr {
    /// The width of the row this subtree *emits*, mirroring [`JoinExpr::width`].
    pub fn width(&self) -> usize {
        match self {
            PhysicalJoinExpr::Input { width, .. } => *width,
            PhysicalJoinExpr::Join {
                left, right, kind, ..
            } if kind.emits_pairs() => left.width() + right.width(),
            PhysicalJoinExpr::Join { left, .. } => left.width(),
            PhysicalJoinExpr::Lateral {
                left, right_width, ..
            } => left.width() + right_width,
        }
    }

    /// The executor and EXPLAIN must use the same physical-algorithm decision.
    /// Keeping it here prevents the renderer from drifting away if the
    /// selection rule grows beyond the presence of extracted hash keys.
    pub fn uses_hash_join(&self) -> bool {
        matches!(
            self,
            PhysicalJoinExpr::Join { hash_keys, .. } if !hash_keys.is_empty()
        )
    }
}

/// The row source of a [`PhysicalPlan::Aggregate`], mirroring [`AggInput`].
pub enum PhysicalAggInput {
    Scan {
        table: Arc<dyn TableAm>,
        /// The columns the grouping keys, aggregate arguments and WHERE read.
        projection: ColumnProjection,
    },
    Join(PhysicalJoinExpr),
    SingleRow,
}

fn plan_join_input(input: JoinInput, costs: cost::CostSettings) -> PhysicalJoinInput {
    match input {
        JoinInput::Scan { table, system } => PhysicalJoinInput::Scan {
            table,
            projection: ColumnProjection::All,
            system,
        },
        JoinInput::Subplan(source) => PhysicalJoinInput::Subplan(Box::new(lower(*source, costs))),
        JoinInput::TableFunction {
            func,
            args,
            ordinality,
        } => PhysicalJoinInput::TableFunction {
            func,
            args,
            ordinality,
        },
    }
}

fn plan_join_expr(source: JoinExpr, costs: cost::CostSettings) -> PhysicalJoinExpr {
    match source {
        JoinExpr::Input {
            input,
            width,
            lateral,
        } => {
            // A lateral leaf is always the right child of the join that feeds it
            // its left row. The binder guarantees it by construction rather than
            // by care: `Preceding::pushes_a_level` is the one gate that sets the
            // flag, and it is handed an empty `reachable` at every position
            // where the leaf would land somewhere else — so this assert
            // documents the invariant, it is not what enforces it.
            debug_assert!(
                !lateral,
                "a lateral leaf reached the planner outside the right child of a join, \
                 so nothing can substitute its level-1 references"
            );
            PhysicalJoinExpr::Input {
                input: plan_join_input(input, costs),
                width,
                predicate: None,
            }
        }
        // Neither hash keys nor sunk leaf filters apply to a row source that is
        // rebuilt per left row.
        JoinExpr::Join {
            left,
            right,
            kind,
            predicate,
        } if matches!(*right, JoinExpr::Input { lateral: true, .. }) => {
            let JoinExpr::Input { input, width, .. } = *right else {
                unreachable!("matched by the guard");
            };
            // Planning the body with its outer references still standing is
            // sound: an `OuterColumnRef` is not a column of this body's own row,
            // so it contributes nothing to the column demand, and every pass
            // already walks past one.
            let right_shape = match &input {
                JoinInput::Subplan(body) => Some(Box::new(plan((**body).clone(), costs))),
                JoinInput::Scan { .. } | JoinInput::TableFunction { .. } => None,
            };
            PhysicalJoinExpr::Lateral {
                left: Box::new(plan_join_expr(*left, costs)),
                right: input,
                right_shape,
                right_width: width,
                kind,
                predicate,
            }
        }
        JoinExpr::Join {
            left,
            right,
            kind,
            predicate,
        } => {
            let left_width = left.width();
            let (hash_keys, predicate) = extract_hash_keys(predicate, left_width);
            debug_assert!(
                kind != JoinKind::Cross || (predicate.is_none() && hash_keys.is_empty()),
                "a cross join carries no condition; pushdown flips the kind to Inner \
                 when it attaches one"
            );
            let mut left = Box::new(plan_join_expr(*left, costs));
            let mut right = Box::new(plan_join_expr(*right, costs));
            let predicate = sink_leaf_filters(predicate, &mut left, &mut right, kind, left_width);
            PhysicalJoinExpr::Join {
                left,
                right,
                kind,
                predicate,
                hash_keys,
            }
        }
    }
}

/// Sink single-relation conjuncts of a join's residual into the leaf they
/// restrict, so a scan is filtered before it is materialized into a hash table
/// rather than after the join has already paired its rows.
///
/// Restricted to inner joins, and deliberately so. For an *outer* join the two
/// predicate origins that meet in this residual pull in opposite directions: a
/// conjunct that came from the query's `WHERE` may only sink into the preserved
/// side, while one that came from the `ON` clause may only sink into the
/// null-supplying side. Nothing here records which is which, and an inner join
/// null-extends neither side, so the distinction cannot arise.
///
/// A semi/anti join is excluded for the same reason: its residual decides
/// *whether a left row has a match*, so sinking a left-only conjunct out of it
/// changes which rows count as matched — under `Anti` that flips the row's fate
/// rather than filtering the output.
fn sink_leaf_filters(
    predicate: Option<BoundExpr>,
    left: &mut PhysicalJoinExpr,
    right: &mut PhysicalJoinExpr,
    kind: JoinKind,
    left_width: usize,
) -> Option<BoundExpr> {
    if !matches!(kind, JoinKind::Cross | JoinKind::Inner) {
        return predicate;
    }
    let mut conjuncts = Vec::new();
    flatten_and(predicate?, &mut conjuncts);

    let mut kept = Vec::new();
    for mut conjunct in conjuncts {
        if !pushdown::is_relocatable(&conjunct) {
            kept.push(conjunct);
            continue;
        }
        // A deeper join node would already have taken this conjunct during
        // pushdown, so the only home left below is a leaf.
        let (target, base) = match ref_side(&conjunct, left_width) {
            Some(Side::Left) => (&mut *left, 0),
            Some(Side::Right) => (&mut *right, left_width),
            None => {
                kept.push(conjunct);
                continue;
            }
        };
        let PhysicalJoinExpr::Input {
            predicate: leaf, ..
        } = target
        else {
            kept.push(conjunct);
            continue;
        };
        conjunct.shift_column_refs(-(base as isize));
        *leaf = rebuild_and(leaf.take().into_iter().chain([conjunct]).collect());
    }
    rebuild_and(kept)
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
pub(crate) fn flatten_and(expr: BoundExpr, out: &mut Vec<BoundExpr>) {
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
pub(crate) fn rebuild_and(mut conjuncts: Vec<BoundExpr>) -> Option<BoundExpr> {
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

/// Recursively split a top-level `OR` tree into its arms, preserving order.
/// The `AND` mirror of this is [`flatten_and`].
pub(crate) fn flatten_or(expr: BoundExpr, out: &mut Vec<BoundExpr>) {
    match expr {
        BoundExpr::Binary {
            op: BinOp::Or,
            left,
            right,
            ..
        } => {
            flatten_or(*left, out);
            flatten_or(*right, out);
        }
        other => out.push(other),
    }
}

/// Re-combine arms with `OR`, yielding `None` for an empty list.
pub(crate) fn rebuild_or(mut arms: Vec<BoundExpr>) -> Option<BoundExpr> {
    let mut acc = arms.pop()?;
    while let Some(next) = arms.pop() {
        acc = BoundExpr::Binary {
            op: BinOp::Or,
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

pub fn plan(logical: LogicalPlan, costs: cost::CostSettings) -> PhysicalPlan {
    let mut physical = lower(logical, costs);
    // After the sinking passes, so each conjunct is ordered against the ones it
    // shares a node with rather than the ones it was written next to.
    qualorder::reorder_quals(&mut physical);
    // Last, so every predicate has already been sunk to the leaf that will
    // evaluate it and each scan's demand is analyzed where it actually applies.
    projection::push_column_projections(&mut physical);
    physical
}

/// Every relation `plan` scans, with the columns the projection pass proved
/// that scan reads.
///
/// Written for `pg_depend`, which records a view's dependency on the columns
/// its query actually reads: planning the view's query and reading the stamped
/// projections is what recovers that set, and doing it here keeps the match
/// over [`PhysicalPlan`]'s arms next to the enum rather than in a caller that
/// would silently miss a new one.
///
/// A relation scanned more than once appears once per scan; the caller decides
/// how to merge them. [`ColumnProjection::All`] is the pass's fail-safe answer
/// and means "assume every column".
pub fn scan_projections(plan: &PhysicalPlan) -> Vec<(Arc<TableSchema>, ColumnProjection)> {
    let mut out = Vec::new();
    collect_scans(plan, &mut out);
    out
}

fn collect_scans(plan: &PhysicalPlan, out: &mut Vec<(Arc<TableSchema>, ColumnProjection)>) {
    match plan {
        PhysicalPlan::Select {
            table, projection, ..
        }
        | PhysicalPlan::IndexScan {
            table, projection, ..
        } => out.push((table.schema(), projection.clone())),
        PhysicalPlan::Subquery { source, .. } => collect_scans(source, out),
        PhysicalPlan::Window { source, .. } => collect_scans(source, out),
        PhysicalPlan::Limit { source, .. } => collect_scans(source, out),
        PhysicalPlan::Join { source, .. } => collect_join_scans(source, out),
        PhysicalPlan::Aggregate { input, .. } => match input {
            PhysicalAggInput::Scan { table, projection } => {
                out.push((table.schema(), projection.clone()))
            }
            PhysicalAggInput::Join(join) => collect_join_scans(join, out),
            PhysicalAggInput::SingleRow => {}
        },
        PhysicalPlan::Append { arms, .. } => {
            for arm in arms {
                out.push((arm.relation.table.schema(), arm.projection.clone()));
            }
        }
        PhysicalPlan::SetOp { arms, .. } => {
            for arm in arms {
                collect_scans(&arm.plan, out);
            }
        }
        PhysicalPlan::Insert { source, .. } => {
            if let PhysicalInsertSource::Query { input, .. } = source {
                collect_scans(input, out);
            }
        }
        // A table function reads no relation, and the three remaining DML nodes
        // name their target rather than scanning a source plan.
        PhysicalPlan::Values { .. }
        | PhysicalPlan::TableFunction { .. }
        | PhysicalPlan::Update { .. }
        | PhysicalPlan::Delete { .. } => {}
    }
}

fn collect_join_scans(
    join: &PhysicalJoinExpr,
    out: &mut Vec<(Arc<TableSchema>, ColumnProjection)>,
) {
    match join {
        PhysicalJoinExpr::Input { input, .. } => match input {
            PhysicalJoinInput::Scan {
                table, projection, ..
            } => out.push((table.schema(), projection.clone())),
            PhysicalJoinInput::Subplan(plan) => collect_scans(plan, out),
            PhysicalJoinInput::TableFunction { .. } => {}
        },
        PhysicalJoinExpr::Join { left, right, .. } => {
            collect_join_scans(left, out);
            collect_join_scans(right, out);
        }
        // Through `right_shape`, not `right`: the executed plan is built per left
        // row and does not exist yet, but a relation read only inside a lateral
        // item is still read by the statement — leaving it out under-reports
        // `pg_depend`, which is the one direction this must not err in.
        PhysicalJoinExpr::Lateral {
            left, right_shape, ..
        } => {
            collect_join_scans(left, out);
            if let Some(shape) = right_shape {
                collect_scans(shape, out);
            }
        }
    }
}

/// Lower one logical node, recursing into subplans. Split from [`plan`] so the
/// projection pass runs exactly once, over the finished tree.
fn lower(logical: LogicalPlan, costs: cost::CostSettings) -> PhysicalPlan {
    match logical {
        LogicalPlan::Values(ValuesPlan {
            columns,
            rows,
            predicate,
            sort,
            distinct,
        }) => PhysicalPlan::Values {
            columns,
            rows,
            predicate,
            sort,
            distinct,
        },
        // The predicate is factored before the access path is chosen, so a
        // `WHERE` written as one OR still offers `choose_access` the conjuncts
        // every arm repeats — without it, `(k = 5 AND a = 1) OR (k = 5 AND a =
        // 2)` probes an index on `k` when the query joins a second relation but
        // scans the whole table when it does not.
        LogicalPlan::Query(QueryPlan {
            table,
            system,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        }) => match choose_access(
            &table,
            predicate.map(pushdown::factor_common_or_conjuncts),
            costs,
        ) {
            AccessPath::Index {
                index_name,
                key,
                residual,
            } => PhysicalPlan::IndexScan {
                table,
                projection: ColumnProjection::All,
                system,
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
                projection: ColumnProjection::All,
                system,
                columns,
                projections,
                predicate,
                sort,
                distinct,
            },
        },
        LogicalPlan::Append(AppendPlan { arms, columns }) => PhysicalPlan::Append {
            arms: arms
                .into_iter()
                .map(|relation| PhysicalAppendArm {
                    relation,
                    // Narrowed by the projection-pushdown pass, once the
                    // wrapping Subquery's demand is known.
                    projection: ColumnProjection::All,
                })
                .collect(),
            columns,
        },
        LogicalPlan::SetOp(SetOpPlan {
            arms,
            columns,
            sort,
            distinct,
        }) => PhysicalPlan::SetOp {
            arms: arms
                .into_iter()
                .map(|arm| PhysicalSetOpArm {
                    plan: lower(arm.plan, costs),
                    coercion: arm.coercion,
                })
                .collect(),
            columns,
            sort,
            distinct,
        },
        LogicalPlan::Subquery(SubqueryPlan {
            source,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        }) => PhysicalPlan::Subquery {
            source: Box::new(lower(*source, costs)),
            columns,
            projections,
            predicate,
            sort,
            distinct,
        },
        LogicalPlan::Window(WindowPlan {
            source,
            spec,
            funcs,
            input_width,
            output_width,
        }) => PhysicalPlan::Window {
            source: Box::new(lower(*source, costs)),
            spec,
            funcs,
            input_width,
            output_width,
        },
        LogicalPlan::TableFunction(TableFunctionPlan {
            func,
            args,
            ordinality,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        }) => PhysicalPlan::TableFunction {
            func,
            args,
            ordinality,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        },
        LogicalPlan::Join(JoinPlan {
            mut source,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        }) => {
            let predicate = pushdown::push_where_into_joins(&mut source, predicate);
            PhysicalPlan::Join {
                source: plan_join_expr(source, costs),
                columns,
                projections,
                predicate,
                sort,
                distinct,
            }
        }
        LogicalPlan::Aggregate(AggregatePlan {
            input,
            predicate,
            group_exprs,
            aggregates,
            having,
            columns,
            projections,
            sort,
            distinct,
        }) => {
            // A grouped query keeps its `WHERE` here — the same join-row predicate
            // an ungrouped one carries — so the extraction has to run on this path
            // too, or every aggregating join (most of TPC-H) misses it.
            let (input, predicate) = match input {
                AggInput::Scan(table) => (
                    PhysicalAggInput::Scan {
                        table,
                        projection: ColumnProjection::All,
                    },
                    predicate,
                ),
                AggInput::Join(mut source) => {
                    let predicate = pushdown::push_where_into_joins(&mut source, predicate);
                    (
                        PhysicalAggInput::Join(plan_join_expr(source, costs)),
                        predicate,
                    )
                }
                AggInput::SingleRow => (PhysicalAggInput::SingleRow, predicate),
            };
            PhysicalPlan::Aggregate {
                input,
                predicate,
                group_exprs,
                aggregates,
                having,
                columns,
                projections,
                sort,
                distinct,
            }
        }
        LogicalPlan::Limit(LimitPlan {
            source,
            limit,
            offset,
        }) => PhysicalPlan::Limit {
            source: Box::new(lower(*source, costs)),
            limit,
            offset,
        },
        LogicalPlan::Insert(InsertPlan {
            table,
            source,
            returning,
            routing,
            freeze,
            system,
        }) => PhysicalPlan::Insert {
            system,
            table,
            source: match source {
                InsertSource::Values(rows) => PhysicalInsertSource::Values(rows),
                InsertSource::Tuples {
                    rows,
                    defaults,
                    notnull_verified,
                } => PhysicalInsertSource::Tuples {
                    rows,
                    defaults,
                    notnull_verified,
                },
                InsertSource::Query { input, projections } => PhysicalInsertSource::Query {
                    input: Box::new(lower(*input, costs)),
                    projections,
                },
            },
            returning,
            routing,
            freeze,
        },
        LogicalPlan::Update(UpdatePlan {
            table,
            predicate,
            assignments,
            returning,
            routing,
            inherited,
            system,
        }) => {
            // A target whose UNIQUE check needs the whole-relation snapshot has
            // to be scanned: see `update_needs_unique_snapshot`. Row movement is
            // possible only through a partitioned parent.
            let row_movement = routing.is_some();
            let keep = |target: &Arc<dyn TableAm>, map: &Option<Arc<[usize]>>| {
                let indexes = target.indexes();
                match map_assigned_columns(&assignments, map) {
                    Some(assigned) => {
                        !update_needs_unique_snapshot(&indexes, &assigned, row_movement)
                    }
                    // Untranslatable assignment: assume the worst and scan.
                    None => false,
                }
            };
            let (routing, inherited, probe) = dml_targets(
                &table,
                routing,
                inherited,
                &predicate,
                Some(&keep),
                &system,
                costs,
            );
            PhysicalPlan::Update {
                system,
                table,
                predicate,
                assignments,
                returning,
                routing,
                inherited,
                probe,
            }
        }
        LogicalPlan::Delete(DeletePlan {
            table,
            predicate,
            returning,
            routing,
            inherited,
            system,
        }) => {
            // A DELETE removes rows outright, so no target needs a snapshot.
            let (routing, inherited, probe) =
                dml_targets(&table, routing, inherited, &predicate, None, &system, costs);
            PhysicalPlan::Delete {
                system,
                table,
                predicate,
                returning,
                routing,
                inherited,
                probe,
            }
        }
    }
}

/// The access path chosen for a single-table read.
enum AccessPath {
    /// An index probe (see [`PhysicalPlan::IndexScan`]).
    Index {
        index_name: String,
        key: IndexProbeSpec,
        residual: Option<BoundExpr>,
    },
    /// A full scan carrying the whole (unconsumed) predicate.
    Scan { predicate: Option<BoundExpr> },
}

/// Choose an access path for a `WHERE` predicate: an index probe when some
/// index's leading key columns are pinned by `col = <constant>` conjuncts —
/// optionally with `<`/`>` bounds on the column after them — and probing it is
/// estimated to cost less than reading the relation, else a full scan. See
/// [`pick_index`] for how the two are compared.
fn choose_access(
    table: &Arc<dyn TableAm>,
    predicate: Option<BoundExpr>,
    costs: cost::CostSettings,
) -> AccessPath {
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
    let quals: Vec<Option<KeyQual>> = conjuncts.iter().map(as_key_qual).collect();

    let Some((probe, consumed)) = pick_index(table, &indexes, &quals, costs) else {
        return AccessPath::Scan {
            predicate: rebuild_and(conjuncts),
        };
    };
    let residual = conjuncts
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !consumed[*i])
        .map(|(_, conjunct)| conjunct)
        .collect();
    AccessPath::Index {
        index_name: probe.index_name,
        key: probe.key,
        residual: rebuild_and(residual),
    }
}

/// Pick the index to probe for a set of already-classified conjuncts, returning
/// the probe and a per-conjunct flag saying which conjuncts the key consumed.
///
/// Every index [`cover_index`] can narrow, and which the engine can physically
/// scan, is costed against a sequential scan of the relation ([`cost`]); the
/// cheapest wins, and `None` means the scan did. That is PostgreSQL's rule, and
/// it is what keeps a probe of a two-page table — where four random pages cost
/// more than reading everything — from being chosen just because an index
/// exists. Ties break structurally, PRIMARY KEY then UNIQUE then the rest, so
/// the choice is deterministic when the estimates cannot separate two indexes.
///
/// Shared by the read path ([`choose_access`], which turns the unconsumed
/// conjuncts into a residual filter the scan applies) and the DML path
/// ([`dml_targets`], which keeps the whole predicate and uses the residual for
/// `EXPLAIN` alone).
///
/// `indexes` is passed in rather than fetched because `TableAm::indexes` deep
/// clones the metadata under a lock, and every caller already needs the list for
/// something else.
fn pick_index(
    table: &Arc<dyn TableAm>,
    indexes: &[IndexMetadata],
    quals: &[Option<KeyQual>],
    costs: cost::CostSettings,
) -> Option<(DmlIndexProbe, Vec<bool>)> {
    // Which indexes are even in the running, in tie-break order. Settled before
    // anything is measured: asking for statistics reads the relation's file
    // length, and every planned statement — including one against a relation
    // with no index at all — would otherwise pay for it.
    //
    // The schema is read here rather than below because `cover_index` needs it;
    // it is an `Arc` snapshot, unlike the statistics.
    let schema = table.schema();
    let candidates: Vec<(&IndexMetadata, IndexCover)> = [
        Some(IndexConstraint::PrimaryKey),
        Some(IndexConstraint::Unique),
        None,
    ]
    .into_iter()
    .flat_map(|pref| indexes.iter().filter(move |i| i.constraint == pref))
    .filter_map(|index| Some((index, cover_index(index, &schema, quals)?)))
    // Only route to an index scan the engine can physically serve; otherwise
    // `EXPLAIN` would advertise an index scan that silently degrades to a
    // sequential scan at execution time.
    .filter(|(index, _)| table.supports_index_scan(&index.name))
    .collect();
    if candidates.is_empty() {
        return None;
    }

    let stats = table.statistics();
    let size = cost::estimate_rel_size(&stats, &schema);
    // Conjuncts the scan would have to evaluate per row. The index path's
    // residual is never larger, and the difference is one CPU term either way;
    // charging both sides the same keeps the comparison about the I/O.
    let nquals = quals.len();
    let mut best: Option<(f64, DmlIndexProbe, Vec<bool>)> = None;
    for (index, chosen) in candidates {
        let mut consumed = vec![false; quals.len()];
        let mut key = IndexProbeSpec::equality(Vec::with_capacity(chosen.eq.len()));
        // Independent per-column selectivities multiply, as in PG's
        // `clauselist_selectivity`.
        let mut selectivity = 1.0;
        for (column, conjunct) in chosen.eq {
            // `cover_index` only returns conjuncts it classified as this role,
            // so the match always binds.
            if let Some(KeyQual::Eq { value, .. }) = &quals[conjunct] {
                selectivity *= key_selectivity(&schema, &stats, column, value, size.rows);
                key.eq.push((column, value.clone()));
                consumed[conjunct] = true;
            }
        }
        if let Some(bounded) = chosen.bounded {
            let mut end = |conjunct: Option<usize>| {
                let conjunct = conjunct?;
                // `cover_index` only returns conjuncts it classified as this
                // role, so the match always binds.
                let KeyQual::Bound {
                    value, inclusive, ..
                } = quals[conjunct].as_ref()?
                else {
                    return None;
                };
                consumed[conjunct] = true;
                Some(Box::new(IndexBoundExpr {
                    column: bounded.column,
                    value: value.clone(),
                    inclusive: *inclusive,
                }))
            };
            let (lower, upper) = (end(bounded.lower), end(bounded.upper));
            selectivity *= bound_selectivity(&schema, &stats, bounded.column, &lower, &upper);
            key.lower = lower;
            key.upper = upper;
        }
        let total = cost::index_scan_cost(
            costs,
            size,
            index_pages(table, &index.name, size),
            selectivity,
            correlation_of(&stats, index.keys.first().map(|k| k.column)),
            nquals,
        );
        if best
            .as_ref()
            .is_none_or(|(cheapest, _, _)| total < *cheapest)
        {
            best = Some((
                total,
                DmlIndexProbe {
                    index_name: index.name.clone(),
                    key,
                    residual: None,
                },
                consumed,
            ));
        }
    }
    let (total, probe, consumed) = best?;
    (total < cost::seq_scan_cost(costs, size, nquals)).then_some((probe, consumed))
}

/// The fraction of the relation the bounded key column keeps.
///
/// Feeds [`cost::range_selectivity`] the same literal-or-nothing the equality
/// side uses ([`const_of`]): a bound whose value is only known at execution time
/// gets the estimator's default, which is what PostgreSQL does too.
fn bound_selectivity(
    schema: &TableSchema,
    stats: &RelStats,
    column: usize,
    lower: &Option<Box<IndexBoundExpr>>,
    upper: &Option<Box<IndexBoundExpr>>,
) -> f64 {
    let Some(meta) = schema.columns.get(column) else {
        return 1.0;
    };
    // A bound whose value is not a literal still *is* a bound — dropping it
    // would estimate an unbounded side and make the index look worse than it is.
    // It is passed as a NULL instead, which `cost::describes` rejects for every
    // analyzed column, so the estimator falls back to its default for that side.
    // That is an in-band signal: teaching the estimator to read NULLs (a
    // null_frac-aware selectivity) has to give "bounded, value unknown" a name
    // of its own first.
    fn end(bound: &Option<Box<IndexBoundExpr>>) -> Option<(&Value, bool)> {
        bound
            .as_ref()
            .map(|b| (const_of(&b.value).unwrap_or(&Value::Null), b.inclusive))
    }
    cost::range_selectivity(
        stats.columns.get(column),
        meta.ty,
        meta.collation.unwrap_or(DEFAULT_COLLATION_OID),
        end(lower),
        end(upper),
    )
}

/// The fraction of the relation one `col = <value>` key column keeps.
fn key_selectivity(
    schema: &TableSchema,
    stats: &RelStats,
    column: usize,
    value: &BoundExpr,
    rows: f64,
) -> f64 {
    let Some(meta) = schema.columns.get(column) else {
        return 1.0;
    };
    cost::eq_selectivity(
        stats.columns.get(column),
        meta.ty,
        meta.collation.unwrap_or(DEFAULT_COLLATION_OID),
        const_of(value),
        rows,
    )
}

/// The literal a key expression is, when it is one. A key that is not a literal
/// — a parameter, or an expression left for execution — is estimated without
/// consulting the distribution, exactly as PostgreSQL's `var_eq_non_const` does:
/// the planner cannot tell which value it will land on.
fn const_of(expr: &BoundExpr) -> Option<&Value> {
    match strip_collate(expr) {
        BoundExpr::Const { value, .. } => Some(value),
        _ => None,
    }
}

/// How strongly the relation's physical order follows this index's leading key
/// column — what decides whether its heap fetches are sequential or random.
fn correlation_of(stats: &RelStats, leading: Option<usize>) -> f32 {
    leading
        .and_then(|column| stats.columns.get(column))
        .map_or(0.0, |column| column.correlation)
}

/// The index's size in pages, or — for an engine that does not report one — a
/// flat assumption of [`ASSUMED_INDEX_ENTRIES_PER_PAGE`] entries per page, one
/// entry per row.
///
/// The fallback is deliberately crude: it only has to be the right order of
/// magnitude against the table's own page count, and the engine that actually
/// serves index scans reports the real number. Sizing it from the key columns'
/// `avg_width` would look more precise without being more useful — the estimate
/// it feeds is multiplied by a selectivity that is itself an approximation.
fn index_pages(table: &Arc<dyn TableAm>, index_name: &str, size: cost::RelSize) -> f64 {
    if let Some(stats) = table.index_statistics(index_name)
        && stats.relpages > 0
    {
        return f64::from(stats.relpages);
    }
    (size.rows / ASSUMED_INDEX_ENTRIES_PER_PAGE).max(1.0)
}

/// Index entries assumed to fit on a page when the engine cannot say. Roughly a
/// 40-byte entry in an 8 KB page.
const ASSUMED_INDEX_ENTRIES_PER_PAGE: f64 = 200.0;

/// Whether one DML target may keep the probe an index offers it, given the
/// target and its [`MappedRelation::map`]. `UPDATE` uses this to veto a probe for
/// a relation it must read in full; see [`update_needs_unique_snapshot`].
type KeepProbe<'a> = &'a dyn Fn(&Arc<dyn TableAm>, &Option<Arc<[usize]>>) -> bool;

/// Attach a row source to every relation an `UPDATE`/`DELETE` writes through,
/// returning the arms in the shape [`PhysicalPlan::Update`]/[`Delete`] carries
/// them: leaf partitions, inheritance descendants, or the plain table's own probe.
///
/// Exactly one arm is ever populated, mirroring the executor's dispatch —
/// routing wins over inheritance, and a plain table is its own single target.
/// Leaf partitions carry no map: a leaf is a verbatim clone of the parent's
/// layout.
///
/// Each target is decided independently against its own indexes, so a mixed plan
/// — one descendant probed, another scanned — is normal. `keep` lets `UPDATE`
/// veto a probe for a target it must read in full; `DELETE` passes `None`.
///
/// [`Delete`]: PhysicalPlan::Delete
fn dml_targets(
    table: &Arc<dyn TableAm>,
    routing: Option<Vec<Arc<dyn TableAm>>>,
    inherited: Vec<MappedRelation>,
    predicate: &Option<BoundExpr>,
    keep: Option<KeepProbe<'_>>,
    // The system columns the statement reads. Each routed leaf must name itself
    // for a `tableoid` among them; inherited targets already carry their own
    // name from the binder.
    system: &Arc<[SysCol]>,
    costs: cost::CostSettings,
) -> (
    Option<Vec<DmlTarget>>,
    Vec<DmlTarget>,
    Option<DmlIndexProbe>,
) {
    let quals = predicate.as_ref().map(|predicate| {
        let mut conjuncts = Vec::new();
        flatten_and(predicate.clone(), &mut conjuncts);
        let quals: Vec<Option<KeyQual>> = conjuncts.iter().map(as_key_qual).collect();
        (conjuncts, quals)
    });
    let probe_for = |target: &Arc<dyn TableAm>, map: &Option<Arc<[usize]>>| {
        let (conjuncts, quals) = quals.as_ref()?;
        if keep.is_some_and(|keep| !keep(target, map)) {
            return None;
        }
        // The predicate stays in the named relation's column space — that is the
        // invariant the executor's `view` rests on — so only the key columns are
        // translated into the target's own. A conjunct whose column the map does
        // not cover simply stops being an index candidate.
        let translated: Vec<Option<KeyQual>> = match map {
            None => quals.to_vec(),
            Some(map) => quals
                .iter()
                .map(|qual| qual.as_ref()?.remapped(|column| map.get(column).copied()))
                .collect(),
        };
        let (mut probe, consumed) = pick_index(target, &target.indexes(), &translated, costs)?;
        probe.residual = rebuild_and(
            conjuncts
                .iter()
                .enumerate()
                .filter(|(i, _)| !consumed[*i])
                .map(|(_, conjunct)| conjunct.clone())
                .collect(),
        );
        Some(probe)
    };

    if let Some(leaves) = routing {
        let arms = leaves
            .into_iter()
            .map(|leaf| DmlTarget {
                probe: probe_for(&leaf, &None),
                relation: MappedRelation {
                    system: (!system.is_empty()).then(|| SystemEmit {
                        cols: Arc::clone(system),
                        ident: RelationIdent::of(&leaf.schema()),
                    }),
                    table: leaf,
                    map: None,
                },
            })
            .collect();
        return (Some(arms), Vec::new(), None);
    }
    if !inherited.is_empty() {
        let arms = inherited
            .into_iter()
            .map(|relation| DmlTarget {
                probe: probe_for(&relation.table, &relation.map),
                relation,
            })
            .collect();
        return (None, arms, None);
    }
    (None, Vec::new(), probe_for(table, &None))
}

/// Whether an `UPDATE` must snapshot all of a relation's rows to check `UNIQUE`.
///
/// It must whenever the statement can introduce a conflict. Writing a unique key
/// is the obvious way. The other is row movement: an `UPDATE` through a
/// partitioned parent can relocate a row into a leaf where its key — untouched
/// though it is — already exists, so for a routed target every unique index
/// counts, not just the written ones.
///
/// This governs the index probe too, and in the same direction: a probe returns
/// only the matching rows, which is too little to build that snapshot from, so
/// the planner withholds one from exactly the targets that answer `true` here.
///
/// `assigned` holds the written columns as ordinals in the schema `indexes`
/// belongs to, so a caller writing through an inheritance parent must translate
/// them first (see [`map_assigned_columns`]).
pub fn update_needs_unique_snapshot(
    indexes: &[IndexMetadata],
    assigned: &[usize],
    row_movement: bool,
) -> bool {
    indexes.iter().any(|index| {
        index.unique
            && (row_movement || index.keys.iter().any(|key| assigned.contains(&key.column)))
    })
}

/// Translate assignment target columns from the named relation's ordinals into
/// one target's own, through that target's [`MappedRelation::map`], or `None` if
/// the map does not cover them all.
///
/// A gap cannot happen today — every parent column exists in a descendant by
/// name — but the answer must stay safe if one ever does, and no fallback ordinal
/// is safe: passing the parent's through unchanged would test the *wrong* column
/// against the target's unique keys, which can flip
/// [`update_needs_unique_snapshot`] to `false` and admit a duplicate key. Callers
/// treat `None` as "snapshot required".
pub fn map_assigned_columns(
    assignments: &[(usize, BoundExpr)],
    map: &Option<Arc<[usize]>>,
) -> Option<Vec<usize>> {
    assignments
        .iter()
        .map(|(column, _)| match map {
            None => Some(*column),
            Some(map) => map.get(*column).copied(),
        })
        .collect()
}

/// Which conjuncts an index's keys can consume: the equality-pinned leading key
/// columns, then at most one bounded column after them.
struct IndexCover {
    /// `(key column, conjunct index)` per pinned column, in key order.
    eq: Vec<(usize, usize)>,
    bounded: Option<BoundedColumn>,
}

/// The one key column a probe may bound, and the conjuncts bounding it.
struct BoundedColumn {
    column: usize,
    lower: Option<usize>,
    upper: Option<usize>,
}

/// Match `index`'s key columns against the classified conjuncts, left to right:
/// equalities for as long as they last, then bounds on the first key column no
/// equality pinned. `None` when nothing matched — such an index would scan the
/// whole key space and narrow nothing.
///
/// Stopping at the first unpinned column is not a simplification, it is what an
/// index *is*: keys are ordered left to right, so a predicate on `b` alone says
/// nothing about where in an `(a, b)` index its rows are. That is also why only
/// one column can be bounded — `a > 1 AND b > 2` selects a contiguous stretch
/// only in `a`, and the `b` test stays a residual filter.
///
/// A cover that leaves a **nullable** key column unconstrained is refused, and
/// that is a property of the storage rather than of the predicate: an engine
/// whose index omits rows with a NULL key column cannot answer such a probe (see
/// [`TableAm::index_lookup`]), so it declines and the executor scans. Refusing
/// here as well is what keeps `EXPLAIN` from advertising an index scan that
/// silently degrades. A probe pinning every key column is unaffected, which is
/// every probe that existed before ranges did.
fn cover_index(
    index: &IndexMetadata,
    schema: &TableSchema,
    quals: &[Option<KeyQual>],
) -> Option<IndexCover> {
    let mut used = vec![false; quals.len()];
    let mut cover = IndexCover {
        eq: Vec::with_capacity(index.keys.len()),
        bounded: None,
    };
    for key in &index.keys {
        let equality = quals.iter().enumerate().position(|(i, qual)| {
            !used[i] && matches!(qual, Some(KeyQual::Eq { column, .. }) if *column == key.column)
        });
        if let Some(conjunct) = equality {
            used[conjunct] = true;
            cover.eq.push((key.column, conjunct));
            continue;
        }
        // No equality on this column: it may still be bounded, and then the key
        // stops there either way.
        let end = |want: BoundSide| {
            quals.iter().position(|qual| {
                matches!(qual, Some(KeyQual::Bound { column, side, .. })
                    if *column == key.column && *side == want)
            })
        };
        let (lower, upper) = (end(BoundSide::Lower), end(BoundSide::Upper));
        if lower.is_some() || upper.is_some() {
            cover.bounded = Some(BoundedColumn {
                column: key.column,
                lower,
                upper,
            });
        }
        break;
    }
    if cover.eq.is_empty() && cover.bounded.is_none() {
        return None;
    }
    // The key columns past the cover: the equality prefix, then the bounded one.
    let constrained = cover.eq.len() + usize::from(cover.bounded.is_some());
    let servable = index.keys[constrained..].iter().all(|key| {
        schema
            .columns
            .get(key.column)
            .is_some_and(|column| !column.nullable)
    });
    servable.then_some(cover)
}

/// Which end of a range a bound is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BoundSide {
    Lower,
    Upper,
}

/// A conjunct an index key can consume: one column compared against a
/// row-constant value.
#[derive(Clone)]
enum KeyQual {
    Eq {
        column: usize,
        value: BoundExpr,
    },
    Bound {
        column: usize,
        side: BoundSide,
        inclusive: bool,
        value: BoundExpr,
    },
}

impl KeyQual {
    /// The same qualification about a different column ordinal — the
    /// translation one DML target needs to read a predicate written in the named
    /// relation's column space. `None` from `map` (a column the target does not
    /// have) drops the qualification rather than guessing an ordinal.
    fn remapped(&self, map: impl Fn(usize) -> Option<usize>) -> Option<KeyQual> {
        let mut out = self.clone();
        match &mut out {
            KeyQual::Eq { column, .. } | KeyQual::Bound { column, .. } => *column = map(*column)?,
        }
        Some(out)
    }
}

/// Classify a conjunct of the form `col <op> <constant>` (either operand order).
///
/// The column side must be a bare [`BoundExpr::ColumnRef`] — a `Coerce` around
/// it means the comparison runs at a different type than the index key, so it is
/// not an index match.
///
/// An ordering comparison additionally has to agree with the order the index is
/// built in, which is byte order: under an ICU collation `'a' < 'B'` while the
/// bytes say otherwise, so a text range under one would select the wrong stretch
/// of the index. Equality is exempt — every supported collation is
/// deterministic, so two strings are equal under it exactly when their bytes
/// are — and that exemption is the reason [`strip_collate`] applies to `=` only.
fn as_key_qual(conjunct: &BoundExpr) -> Option<KeyQual> {
    let BoundExpr::Binary {
        op,
        collation,
        left,
        right,
        ..
    } = conjunct
    else {
        return None;
    };
    if *op == BinOp::Eq {
        return match (strip_collate(left), strip_collate(right)) {
            (BoundExpr::ColumnRef { index, .. }, value) if is_row_constant(value) => {
                Some(KeyQual::Eq {
                    column: *index,
                    value: value.clone(),
                })
            }
            (value, BoundExpr::ColumnRef { index, .. }) if is_row_constant(value) => {
                Some(KeyQual::Eq {
                    column: *index,
                    value: value.clone(),
                })
            }
            _ => None,
        };
    }
    // `collation` is the collation the binder resolved for this comparison, so
    // one check covers both an explicit `COLLATE` and the column's own.
    if !crabgresql_types::collation::is_byte_order(*collation) {
        return None;
    }
    // Reading the comparison with the column on the left: flipping the operands
    // flips which end of the range the constant bounds.
    let (column, value, op) = match (&**left, &**right) {
        (BoundExpr::ColumnRef { index, .. }, value) if is_row_constant(value) => {
            (*index, value, *op)
        }
        (value, BoundExpr::ColumnRef { index, .. }) if is_row_constant(value) => {
            (*index, value, flip_comparison(*op))
        }
        _ => return None,
    };
    let (side, inclusive) = match op {
        BinOp::Gt => (BoundSide::Lower, false),
        BinOp::GtEq => (BoundSide::Lower, true),
        BinOp::Lt => (BoundSide::Upper, false),
        BinOp::LtEq => (BoundSide::Upper, true),
        _ => return None,
    };
    Some(KeyQual::Bound {
        column,
        side,
        inclusive,
        value: value.clone(),
    })
}

/// The comparison that means the same with its operands swapped.
fn flip_comparison(op: BinOp) -> BinOp {
    match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::LtEq => BinOp::GtEq,
        BinOp::Gt => BinOp::Lt,
        BinOp::GtEq => BinOp::LtEq,
        other => other,
    }
}

/// See through a [`BoundExpr::Collate`] to the expression it labels.
///
/// Safe for an *equality* index probe specifically: every supported collation is
/// deterministic, so two values are equal under a collation exactly when their
/// bytes are equal — which is the order the index is built in. A range probe
/// must not use this; see the collation check in [`as_key_qual`].
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
        | BoundExpr::BoolTest { expr, .. }
        | BoundExpr::Coerce { expr, .. }
        | BoundExpr::Collate { expr, .. }
        | BoundExpr::Reinterpret { expr, .. } => is_row_constant(expr),
        BoundExpr::Binary { left, right, .. } => is_row_constant(left) && is_row_constant(right),
        BoundExpr::ArrayCtor { elems, .. } => elems.iter().all(is_row_constant),
        BoundExpr::Subscript { base, index, .. } => is_row_constant(base) && is_row_constant(index),
        BoundExpr::Case { whens, else_, .. } => {
            whens
                .iter()
                .all(|(when, then)| is_row_constant(when) && is_row_constant(then))
                && else_.as_ref().map_or(true, |e| is_row_constant(e))
        }
        BoundExpr::Coalesce { args, .. } | BoundExpr::MinMax { args, .. } => {
            args.iter().all(is_row_constant)
        }
        // ColumnRef/Param reference per-row/per-execution state; FuncCall/Srf and
        // a user routine may be volatile; Aggregate and WindowFunc never appear in a
        // bindable WHERE key. A subquery
        // is still an unresolved subplan at plan time, so never hoist it as a key.
        // An outer (correlated) reference is only a `Const` after `substitute_outer`
        // rewrites it per outer row; unresolved here it must not be hoisted as a
        // once-evaluated index key.
        BoundExpr::ColumnRef { .. }
        | BoundExpr::Param { .. }
        | BoundExpr::OuterColumnRef { .. }
        | BoundExpr::FuncCall { .. }
        | BoundExpr::Routine { .. }
        | BoundExpr::Srf { .. }
        | BoundExpr::Aggregate { .. }
        | BoundExpr::WindowFunc { .. }
        | BoundExpr::ScalarSubquery { .. }
        | BoundExpr::ArraySubquery { .. }
        | BoundExpr::Exists { .. }
        | BoundExpr::QuantifiedSubquery { .. }
        | BoundExpr::QuantifiedArray { .. } => false,
    }
}

/// Render a physical plan as PostgreSQL-style `EXPLAIN` text — one string per
/// output line. It reproduces PG's node headers (`Seq Scan on t`, `Index Scan
/// using t_pkey on t`, `Index Cond`, `Filter`) so the chosen access path is
/// observable. Shapes with no access path worth showing collapse to one
/// summary line (`Values Scan`, `Function Scan`, an aggregate over a non-join
/// input); every other node renders its children and properties.
///
/// TODO: print the `(cost=… rows=… width=…)` estimate PG puts on every node,
/// which needs the cost model this planner does not have.
///
/// `EXPLAIN ANALYZE` renders the same lines and appends [`explain_summary`].
/// TODO: print the per-node `(actual time=… rows=… loops=…)` suffix and the
/// `Buffers:` block; without them a node line is a *prefix* of PG's, not a copy
/// of it, so a consumer that parses `actual rows=` out of ANALYZE output will
/// not find it here. Only the two footers are byte-identical to PG's.
pub fn explain(plan: &PhysicalPlan) -> Vec<String> {
    match plan {
        PhysicalPlan::Select {
            table, predicate, ..
        } => {
            let schema = table.schema();
            let names = schema_names(&schema);
            let mut lines = vec![format!(
                "Seq Scan on {}{}",
                schema.name,
                plan.vectorization().suffix().unwrap_or_default()
            )];
            if let Some(predicate) = predicate {
                lines.push(format!("  Filter: ({})", explain_expr(predicate, &names)));
            }
            lines
        }
        PhysicalPlan::IndexScan {
            table,
            index_name,
            key,
            predicate,
            ..
        } => index_scan_lines(
            table,
            index_name,
            key,
            predicate.as_ref(),
            &schema_names(&table.schema()),
        ),
        PhysicalPlan::Values { .. } => vec!["Values Scan".to_string()],
        PhysicalPlan::Append { arms, .. } => {
            // PG's Append: one child scan per arm, in scan order. A WHERE
            // predicate lives on the wrapping Subquery in this pipeline, so it is
            // not re-rendered per child (reduced EXPLAIN).
            //
            // Each arm names its own line: a SQL partition or inheritance child
            // renders as the usual `Seq Scan on <rel>`, while an engine-internal
            // storage leaf can distinguish itself, so an Append over one
            // relation's leaves is not the same line repeated. The remap an
            // inheritance arm carries is deliberately invisible — PG prints no
            // such annotation either.
            // Not annotated here: an engine-managed relation always renders
            // through the `Subquery` that owns its tail, and that wrapper adds
            // the annotation to this very line. Doing it in both places would
            // print the suffix twice.
            let mut lines = vec!["Append".to_string()];
            for arm in arms {
                lines.push(format!("  ->  {}", arm.relation.table.scan_label()));
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
                push_child(&mut lines, explain(&arm.plan));
            }
            if distinct.is_some() {
                lines = nest_under("HashAggregate", lines);
            }
            if !sort.is_empty() {
                lines = nest_under("Sort", lines);
            }
            lines
        }
        PhysicalPlan::Subquery {
            source, predicate, ..
        } => {
            let mut lines = explain(source);
            // The tail on this wrapper is what vectorizes, but the node the user
            // sees is the child's root line — an engine-managed relation renders
            // as `Append`, with no line of its own for the Subquery. So the
            // annotation is attached there, describing the work as one node.
            if let Some(suffix) = plan.vectorization().suffix()
                && let Some(first) = lines.first_mut()
            {
                first.push_str(&suffix);
            }
            // A subplan's WHERE lives here, not on the child, so rendering only
            // the child would drop the predicate from the plan entirely. Names
            // come from the SOURCE row (what the predicate indexes into), not the
            // subquery's projected output.
            if let Some(predicate) = predicate {
                let names = source_column_names(source);
                push_root_property(
                    &mut lines,
                    format!("Filter: ({})", explain_expr(predicate, &names)),
                );
            }
            lines
        }
        PhysicalPlan::TableFunction { .. } => vec!["Function Scan".to_string()],
        PhysicalPlan::Join {
            source, predicate, ..
        } => explain_join(source, predicate.as_ref()),
        PhysicalPlan::Aggregate {
            input, predicate, ..
        } => match input {
            // Only the join input is rendered in detail: it is the shape whose
            // access strategy is worth observing, and the one pushdown changes.
            PhysicalAggInput::Join(source) => {
                nest_under("Aggregate", explain_join(source, predicate.as_ref()))
            }
            _ => vec!["Aggregate".to_string()],
        },
        // PG renders one `WindowAgg` per spec with a `Window: wN AS (…)`
        // property, numbering the specs in *evaluation* order — so the bottom
        // node of a chain is `w1`. Numbering here is by depth for the same
        // reason: `window_number` counts the `Window` nodes below this one.
        PhysicalPlan::Window { source, spec, .. } => {
            let mut lines = nest_under("WindowAgg", explain(source));
            let names = source_column_names(source);
            push_root_property(
                &mut lines,
                format!(
                    "Window: w{} AS ({})",
                    window_number(plan),
                    explain_window_spec(spec, &names)
                ),
            );
            lines
        }
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
        PhysicalPlan::Update {
            table,
            predicate,
            routing,
            inherited,
            probe,
            system,
            ..
        } => {
            let mut lines = vec![format!("Update on {}", table.schema().name)];
            lines.extend(dml_child_lines(
                table,
                routing,
                inherited,
                probe,
                predicate.as_ref(),
                system,
            ));
            lines
        }
        PhysicalPlan::Delete {
            table,
            predicate,
            routing,
            inherited,
            probe,
            system,
            ..
        } => {
            let mut lines = vec![format!("Delete on {}", table.schema().name)];
            lines.extend(dml_child_lines(
                table,
                routing,
                inherited,
                probe,
                predicate.as_ref(),
                system,
            ));
            lines
        }
    }
}

/// The `Index Scan` node for a read, and — indented — for one probed DML target.
///
/// `predicate` is the residual filter: the conjuncts the index key did not cover.
///
/// The two halves are spelled in different column spaces, which is why `names` is
/// a parameter. `Index Cond` names key columns, which are ordinals in `table`'s
/// own schema; the residual belongs to whichever relation the predicate was
/// written against, and for an inheritance descendant that is the parent.
fn index_scan_lines(
    table: &Arc<dyn TableAm>,
    index_name: &str,
    key: &IndexProbeSpec,
    predicate: Option<&BoundExpr>,
    names: &[Option<String>],
) -> Vec<String> {
    let schema = table.schema();

    let mut lines = vec![format!("Index Scan using {index_name} on {}", schema.name)];
    let column = |column: usize| schema.columns[column].name.clone();
    let bound = |b: &IndexBoundExpr, op: &str, eq: &str| {
        let op = if b.inclusive { eq } else { op };
        format!(
            "{} {op} {}",
            column(b.column),
            explain_expr(&b.value, names)
        )
    };
    let conds: Vec<String> = key
        .eq
        .iter()
        .map(|(c, value)| format!("{} = {}", column(*c), explain_expr(value, names)))
        // PG prints the bounds after the equality keys, in that order, which is
        // also index-key order: the bounded column follows the pinned ones, and
        // its lower bound precedes its upper.
        .chain(key.lower.iter().map(|b| bound(b, ">", ">=")))
        .chain(key.upper.iter().map(|b| bound(b, "<", "<=")))
        .collect();
    // One condition prints bare inside its own parentheses, several are
    // parenthesized individually *and* as a whole — `((a = 1) AND (b > 5))`,
    // matching what PG 18.4 prints for the same index and predicate.
    let cond = conds.join(") AND (");
    lines.push(match conds.len() {
        1 => format!("  Index Cond: ({cond})"),
        _ => format!("  Index Cond: (({cond}))"),
    });
    if let Some(predicate) = predicate {
        lines.push(format!("  Filter: ({})", explain_expr(predicate, names)));
    }
    lines
}

/// The `Seq Scan` node for a read, and — indented — for one scanned DML target.
///
/// `names` is supplied rather than read off `table` because a DML child renders a
/// predicate written in the *named relation's* column space; an inheritance
/// descendant's own schema would spell those ordinals as different columns.
fn seq_scan_lines(
    table: &Arc<dyn TableAm>,
    names: &[Option<String>],
    predicate: Option<&BoundExpr>,
) -> Vec<String> {
    let mut lines = vec![format!("Seq Scan on {}", table.schema().name)];
    if let Some(predicate) = predicate {
        lines.push(format!("  Filter: ({})", explain_expr(predicate, names)));
    }
    lines
}

/// The child scan nodes under an `Update on`/`Delete on`, one per target.
///
/// Every target is rendered, probed or not, because the set of relations a
/// statement reads is exactly what the plan is being asked. Showing only the
/// probed ones made an inheritance plan name the indexed descendant and stay
/// silent about the parent whose rows it also modifies.
///
/// A probed target splits its predicate the way PG does — the covered conjuncts
/// as `Index Cond`, the rest as `Filter` — even though the executor re-checks the
/// whole `WHERE`; see [`DmlIndexProbe::residual`].
fn dml_child_lines(
    table: &Arc<dyn TableAm>,
    routing: &Option<Vec<DmlTarget>>,
    inherited: &[DmlTarget],
    probe: &Option<DmlIndexProbe>,
    predicate: Option<&BoundExpr>,
    system: &[SysCol],
) -> Vec<String> {
    // The predicate is in the named relation's column space, whichever target
    // ends up reading the rows — widened by the system slots, which live past
    // that space and are what the predicate may also read.
    let names = dml_names(&table.schema(), system);
    let mut lines = Vec::new();
    let mut push = |target: &Arc<dyn TableAm>, probe: &Option<DmlIndexProbe>| {
        let child = match probe {
            Some(probe) => index_scan_lines(
                target,
                &probe.index_name,
                &probe.key,
                probe.residual.as_ref(),
                &names,
            ),
            None => seq_scan_lines(target, &names, predicate),
        };
        push_child(&mut lines, child);
    };
    match (routing, inherited.is_empty()) {
        (Some(leaves), _) => {
            for leaf in leaves {
                push(&leaf.relation.table, &leaf.probe);
            }
        }
        (None, false) => {
            for target in inherited {
                push(&target.relation.table, &target.probe);
            }
        }
        (None, true) => push(table, probe),
    }
    lines
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

/// Append a child plan under a `->` lead, indenting its continuation lines to
/// line up beneath it.
fn push_child(lines: &mut Vec<String>, child: Vec<String>) {
    let mut child = child.into_iter();
    if let Some(first) = child.next() {
        lines.push(format!("  ->  {first}"));
        lines.extend(child.map(|l| format!("      {l}")));
    }
}

/// Put `child` under a new parent node, indenting it beneath a `->` lead the way
/// PG renders a single-child plan node.
fn nest_under(parent: &str, child: Vec<String>) -> Vec<String> {
    let mut lines = vec![parent.to_string()];
    push_child(&mut lines, child);
    lines
}

/// Add a property to the root node of an already-rendered plan. Root properties
/// belong before its first child; appending would make a filter executed above
/// a multi-node subplan look like it belonged to the subplan's final leaf.
fn push_root_property(lines: &mut Vec<String>, property: String) {
    let child = lines
        .iter()
        .position(|line| line.starts_with("  ->  "))
        .unwrap_or(lines.len());
    lines.insert(child, format!("  {property}"));
}

/// Minimal expression rendering for `EXPLAIN` conditions: enough to show a
/// constant key or a residual comparison. Column references render by name
/// through `names`.
///
/// TODO: deparse the expression kinds that fall through to the `…` placeholder
/// (function calls, `CASE`, subqueries, array constructors); PG prints the
/// whole condition.
fn explain_expr(expr: &BoundExpr, names: &[Option<String>]) -> String {
    match expr {
        // Divergence: rendered in UTC at the default `extra_float_digits` and
        // the default `bytea_output`, because `EXPLAIN` output is built without
        // a session context — so a `float8` constant ignores the session's
        // precision setting, and a `bytea` one prints hex however the session is
        // set. PG 18.4 under `SET bytea_output=escape` prints
        // `Filter: (b = '\000a'::bytea)` where this prints `'\x0061'::bytea`.
        //
        // A `timestamptz` literal is further off: it is still an unevaluated
        // `Coerce` of its source text at this point (see the binder's
        // `resolve_unknown`), and the `Coerce` arm below recurses to the inner
        // `Const`, so `EXPLAIN … WHERE ts > '2024-01-01'` prints the bare
        // `2024-01-01` rather than a rendered timestamp.
        BoundExpr::Const { value, .. } => value
            .encode_text_utc()
            .unwrap_or_else(|| "NULL".to_string()),
        BoundExpr::ColumnRef { index, .. } => names
            .get(*index)
            .and_then(Option::as_deref)
            .map_or_else(|| format!("${index}"), str::to_string),
        BoundExpr::Param { index, .. } => format!("${}", index + 1),
        BoundExpr::Coerce { expr, .. } | BoundExpr::Reinterpret { expr, .. } => {
            explain_expr(expr, names)
        }
        BoundExpr::IsNull { expr, negated } => format!(
            "{} IS {}NULL",
            explain_operand(expr, names),
            if *negated { "NOT " } else { "" }
        ),
        BoundExpr::BoolTest {
            expr,
            value,
            negated,
        } => format!(
            "{} {}",
            explain_operand(expr, names),
            crabgresql_binder::bool_test_clause(*value, *negated)
        ),
        BoundExpr::Binary {
            op, left, right, ..
        } => format!(
            "{} {} {}",
            explain_operand(left, names),
            op.sql_symbol(),
            explain_operand(right, names)
        ),
        _ => "…".to_string(),
    }
}

/// Render `expr` where a larger expression uses it as an operand. PG's
/// `EXPLAIN` output parenthesizes anything that is not a bare column, constant,
/// or parameter, so `x IS NULL` under an `IS TRUE` prints as
/// `(x IS NULL) IS TRUE`.
fn explain_operand(expr: &BoundExpr, names: &[Option<String>]) -> String {
    // A cast is invisible in this output, so its operand decides.
    let bare = match expr {
        BoundExpr::Coerce { expr, .. } | BoundExpr::Reinterpret { expr, .. } => expr,
        other => other,
    };
    match bare {
        BoundExpr::IsNull { .. } | BoundExpr::BoolTest { .. } | BoundExpr::Binary { .. } => {
            format!("({})", explain_expr(expr, names))
        }
        // Leaves, and the `…` placeholder, read better unwrapped.
        _ => explain_expr(expr, names),
    }
}

/// The column names of a single table's row, for [`explain_expr`].
fn schema_names(schema: &TableSchema) -> Vec<Option<String>> {
    schema
        .columns
        .iter()
        .map(|c| Some(c.name.clone()))
        .collect()
}

/// [`schema_names`] plus the system-column slots the statement appended, in row
/// order.
///
/// Without them a predicate reading one renders as `$N` — the ordinal falls past
/// the declared columns, and a plan that prints `Filter: ($2 = 0)` says nothing
/// about which column was tested. PostgreSQL names them, so this does too.
fn dml_names(schema: &TableSchema, system: &[SysCol]) -> Vec<Option<String>> {
    let mut names = schema_names(schema);
    names.extend(system.iter().map(|c| Some(c.name().to_string())));
    names
}

/// This window step's 1-based position in its chain, counting from the bottom —
/// the `N` in PG's `Window: wN AS (…)`, which numbers specs in evaluation order.
fn window_number(plan: &PhysicalPlan) -> usize {
    let mut node = plan;
    let mut depth = 1;
    while let PhysicalPlan::Window { source, .. } = node {
        node = source;
        depth += 1;
    }
    // `node` is now the first non-window plan, and `depth` counted this node
    // plus every window below it, one too many.
    depth - 1
}

/// Render an `OVER (…)` clause the way PG's `Window:` property does, omitting a
/// clause that is absent. The frame is never printed: the binder accepts only
/// the default frame, and PG omits that one too.
///
/// TODO: print the `ROWS`/`RANGE` frame clause for an explicit (non-default)
/// window frame.
fn explain_window_spec(spec: &BoundWindowSpec, names: &[Option<String>]) -> String {
    let mut parts = Vec::new();
    if !spec.partition_by.is_empty() {
        parts.push(format!(
            "PARTITION BY {}",
            spec.partition_by
                .iter()
                .map(|e| explain_expr(e, names))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !spec.order_by.is_empty() {
        parts.push(format!(
            "ORDER BY {}",
            spec.order_by
                .iter()
                .map(|key| {
                    let mut rendered = explain_expr(&key.expr, names);
                    if !key.asc {
                        rendered.push_str(" DESC");
                    }
                    // PG prints the NULLS clause only when it is not the default
                    // for the direction — last for ASC, first for DESC — which
                    // is exactly when the two flags agree.
                    if key.nulls_first == key.asc {
                        rendered.push_str(if key.nulls_first {
                            " NULLS FIRST"
                        } else {
                            " NULLS LAST"
                        });
                    }
                    rendered
                })
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    parts.join(" ")
}

/// The column names of `plan`'s output row, for rendering an expression that
/// indexes into it. Empty when the shape has no names to offer, which
/// [`explain_expr`] renders as `$index`.
fn source_column_names(plan: &PhysicalPlan) -> Vec<Option<String>> {
    match plan {
        PhysicalPlan::Append { columns, .. } => {
            columns.iter().map(|c| Some(c.name.clone())).collect()
        }
        PhysicalPlan::Select { table, .. } | PhysicalPlan::IndexScan { table, .. } => {
            schema_names(&table.schema())
        }
        // A window step reproduces its input row and appends slots, so the
        // names below it still apply — and every spec in a chain reads that
        // same row, so seeing through is what lets the upper `Window:` lines
        // name their columns instead of falling back to `$n`.
        PhysicalPlan::Window { source, .. } => source_column_names(source),
        _ => Vec::new(),
    }
}

/// The column names of a join subtree's row, in layout order. A subplan or
/// table-function leaf contributes `None` per column — its output columns have
/// no schema name to show, so an expression over them renders as `$index`.
fn join_column_names(join: &PhysicalJoinExpr) -> Vec<Option<String>> {
    match join {
        PhysicalJoinExpr::Input { input, width, .. } => match input {
            PhysicalJoinInput::Scan { table, .. } => schema_names(&table.schema()),
            // An `Append` here is one relation read from several physical
            // sources, not a genuine subquery: its columns are the relation's, so
            // an expression over them must still render by name. Without this a
            // join or filter touching such a relation prints `$0`.
            PhysicalJoinInput::Subplan(plan) => match plan.as_ref() {
                PhysicalPlan::Append { columns, .. } if columns.len() == *width => {
                    columns.iter().map(|c| Some(c.name.clone())).collect()
                }
                _ => vec![None; *width],
            },
            _ => vec![None; *width],
        },
        PhysicalJoinExpr::Lateral {
            left, right_width, ..
        } => {
            let mut names = emitted_column_names(left);
            names.extend(std::iter::repeat_n(None, *right_width));
            names
        }
        PhysicalJoinExpr::Join { left, right, .. } => {
            let mut names = emitted_column_names(left);
            names.extend(emitted_column_names(right));
            names
        }
    }
}

/// [`join_column_names`] for a subtree seen from *above*, where only the row it
/// emits is addressable. The two differ under a semi/anti join, and taking its
/// full name list there would shift every name of the sibling to its right.
fn emitted_column_names(join: &PhysicalJoinExpr) -> Vec<Option<String>> {
    let mut names = join_column_names(join);
    names.resize(join.width(), None);
    names
}

/// The node label EXPLAIN prints for one binary join, following PG's spelling:
/// `Hash Left Join`, `Nested Loop Left Join`, `Hash Full Join`, `Hash Anti Join`
/// (`vendor/postgres/regress/expected/generated_virtual.out:1680`,
/// `create_index.out:2256`, `equivclass.out:528`, `eager_aggregate.out:484`).
///
/// `Cross` prints as an inner join: by EXPLAIN time PostgreSQL no longer
/// distinguishes the two, and the absent condition shows as a missing
/// `Join Filter` instead.
fn join_node_label(kind: JoinKind, hashed: bool) -> String {
    let algorithm = if hashed { "Hash" } else { "Nested Loop" };
    let kind = match kind {
        JoinKind::Cross | JoinKind::Inner => {
            return if hashed { "Hash Join" } else { "Nested Loop" }.to_string();
        }
        JoinKind::Left => "Left",
        JoinKind::Right => "Right",
        JoinKind::Full => "Full",
        JoinKind::Semi => "Semi",
        JoinKind::Anti => "Anti",
    };
    format!("{algorithm} {kind} Join")
}

/// Render a join tree. `filter` is the plan-level `WHERE` residual that pushdown
/// could not relocate; it belongs to the root node's row, so only the root call
/// passes it.
///
/// A node's own predicate and hash keys index *its* subtree's row, so names are
/// recomputed per node rather than threaded down.
fn explain_join(join: &PhysicalJoinExpr, filter: Option<&BoundExpr>) -> Vec<String> {
    let names = join_column_names(join);
    let hashed = join.uses_hash_join();
    match join {
        PhysicalJoinExpr::Input {
            input, predicate, ..
        } => {
            let mut lines = match input {
                PhysicalJoinInput::Scan { table, .. } => {
                    vec![format!("Seq Scan on {}", table.schema().name)]
                }
                PhysicalJoinInput::TableFunction { .. } => vec!["Function Scan".to_string()],
                PhysicalJoinInput::Subplan(source) => explain(source),
            };
            for predicate in predicate.iter().chain(filter) {
                push_root_property(
                    &mut lines,
                    format!("Filter: ({})", explain_expr(predicate, &names)),
                );
            }
            lines
        }
        PhysicalJoinExpr::Join {
            left,
            right,
            kind,
            predicate,
            hash_keys,
        } => {
            let mut lines = vec![join_node_label(*kind, hashed)];
            if hashed {
                let cond = hash_keys
                    .iter()
                    .map(|k| {
                        format!(
                            "{} = {}",
                            explain_expr(&k.left, &names),
                            explain_expr(&k.right, &names)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(") AND (");
                lines.push(format!("  Hash Cond: ({cond})"));
            }
            if let Some(predicate) = predicate {
                lines.push(format!(
                    "  Join Filter: ({})",
                    explain_expr(predicate, &names)
                ));
            }
            if let Some(filter) = filter {
                lines.push(format!("  Filter: ({})", explain_expr(filter, &names)));
            }
            push_child(&mut lines, explain_join(left, None));
            // The right input is the build side, so a hash join shows it under a
            // Hash node the way PG does.
            let right = explain_join(right, None);
            push_child(
                &mut lines,
                if hashed {
                    nest_under("Hash", right)
                } else {
                    right
                },
            );
            lines
        }
        PhysicalJoinExpr::Lateral {
            left,
            right_shape,
            kind,
            predicate,
            ..
        } => {
            let mut lines = vec![join_node_label(*kind, false)];
            if let Some(predicate) = predicate {
                lines.push(format!(
                    "  Join Filter: ({})",
                    explain_expr(predicate, &names)
                ));
            }
            if let Some(filter) = filter {
                lines.push(format!("  Filter: ({})", explain_expr(filter, &names)));
            }
            push_child(&mut lines, explain_join(left, None));
            // `right_shape`, the copy planned once for exactly this: the plan
            // a row is actually produced by is built per left row and cannot be
            // shown, but the two differ only in the constants substituted in.
            push_child(
                &mut lines,
                match right_shape {
                    Some(shape) => explain(shape),
                    None => vec!["Function Scan".to_string()],
                },
            );
            lines
        }
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
        ColStats, Column, DeleteResult, EmptyTypeCatalog, IndexKey, IndexMethod, StorageError,
        TableEngine, TableSchema, Tid, Tuple, TupleStream, TypeCatalog, UpdateResult,
    };
    use crabgresql_txn::TxnContext;
    use crabgresql_types::{PgType, Value};

    /// An `Append` arm carrying the named relation's layout verbatim — what a
    /// partition or a storage leaf produces.
    pub(super) fn identity_arm(table: Arc<dyn TableAm>) -> PhysicalAppendArm {
        PhysicalAppendArm {
            relation: MappedRelation {
                table,
                map: None,
                system: None,
            },
            projection: ColumnProjection::All,
        }
    }

    /// A metadata-only engine for planner tests: it holds table schemas and their
    /// index metadata and reports `supports_index_scan = true`, so the planner's
    /// index-selection path is exercised without a real storage engine. It never
    /// stores rows — the planner only reads schema/index metadata, so every
    /// row-touching method is `unimplemented!()`.
    #[derive(Default)]
    pub(super) struct MetaEngine {
        tables: Mutex<HashMap<String, Arc<MetaTable>>>,
    }

    struct MetaTable {
        schema: Arc<TableSchema>,
        indexes: Mutex<Vec<IndexMetadata>>,
        /// What `ANALYZE` would have measured. Left at "nothing known" unless a
        /// test sets it: the cost model's answer for an unmeasured relation is
        /// itself worth asserting, and every plan choice that does not depend on
        /// size should be made without one.
        stats: Mutex<Option<RelStats>>,
    }

    impl TableAm for MetaTable {
        fn schema(&self) -> Arc<TableSchema> {
            Arc::clone(&self.schema)
        }
        fn indexes(&self) -> Vec<IndexMetadata> {
            self.indexes.lock().expect("mutex").clone()
        }
        fn supports_index_scan(&self, _index_name: &str) -> bool {
            true
        }
        fn statistics(&self) -> RelStats {
            match self.stats.lock().expect("mutex").clone() {
                Some(stats) => stats,
                None => RelStats::unknown(&self.schema),
            }
        }
        fn scan(&self, _txn: &TxnContext, _projection: &ColumnProjection) -> TupleStream {
            unimplemented!("planner tests never scan")
        }
        fn fetch(&self, _tid: Tid, _txn: &TxnContext) -> Result<Option<Tuple>, StorageError> {
            unimplemented!("planner tests never fetch")
        }
        fn insert(&self, _tuple: Tuple, _txn: &TxnContext) -> Result<Tid, StorageError> {
            unimplemented!("planner tests never insert")
        }
        fn update(
            &self,
            _tid: Tid,
            _tuple: Tuple,
            _txn: &TxnContext,
        ) -> Result<UpdateResult, StorageError> {
            unimplemented!("planner tests never update")
        }
        fn delete(&self, _tid: Tid, _txn: &TxnContext) -> Result<DeleteResult, StorageError> {
            unimplemented!("planner tests never delete")
        }
    }

    impl TableEngine for MetaEngine {
        fn create_table(&self, schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError> {
            let table = Arc::new(MetaTable {
                schema: Arc::new(schema.clone()),
                indexes: Mutex::new(Vec::new()),
                stats: Mutex::new(None),
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

    pub(super) fn plan_sql(sql: &str) -> PhysicalPlan {
        plan_sql_indexed(sql, None)
    }

    /// Plan `sql` against table `t(id int4, big int8, name text)`, optionally
    /// registering an index (name + `IndexMetadata`) first.
    fn plan_sql_indexed(sql: &str, index: Option<IndexMetadata>) -> PhysicalPlan {
        plan_sql_analyzed(sql, index, None)
    }

    /// As [`plan_sql_indexed`], with `t`'s statistics as `ANALYZE` would have
    /// left them. Needed wherever the choice is a cost comparison rather than a
    /// structural one: an unmeasured relation is assumed small, and reading a
    /// third of a small table is cheaper through a sequential scan.
    fn plan_sql_analyzed(
        sql: &str,
        index: Option<IndexMetadata>,
        stats: Option<RelStats>,
    ) -> PhysicalPlan {
        plan_sql_analyzed_with(sql, index, stats, true)
    }

    /// As [`plan_sql_analyzed`], choosing whether `big` accepts NULL.
    fn plan_sql_analyzed_with(
        sql: &str,
        index: Option<IndexMetadata>,
        stats: Option<RelStats>,
        big_nullable: bool,
    ) -> PhysicalPlan {
        plan(
            bind_sql_full(sql, index, stats, big_nullable),
            cost::CostSettings::default(),
        )
    }

    /// [`plan_sql_indexed`] stopping at the bound plan, for a test that wants to
    /// rewrite it before planning.
    pub(super) fn bind_sql_indexed(sql: &str, index: Option<IndexMetadata>) -> LogicalPlan {
        bind_sql_full(sql, index, None, true)
    }

    /// The bound plan, with `t`'s statistics set. The plan holds the table by
    /// `Arc`, so statistics recorded here are the ones the planner reads.
    /// The whole fixture, with `big`'s nullability spelled out: it is what
    /// decides whether a cover stopping before it is servable at all.
    fn bind_sql_full(
        sql: &str,
        index: Option<IndexMetadata>,
        stats: Option<RelStats>,
        big_nullable: bool,
    ) -> LogicalPlan {
        let meta = Arc::new(MetaEngine::default());
        let engine: Arc<dyn TableEngine> = Arc::clone(&meta) as Arc<dyn TableEngine>;
        let catalog: Arc<dyn TypeCatalog> = Arc::new(EmptyTypeCatalog);
        let mut big = Column::new("big", PgType::Int8);
        big.nullable = big_nullable;
        if let Err(error) = engine.create_table(TableSchema::in_namespace(
            "t",
            "public",
            vec![
                Column::new("id", PgType::Int4),
                big,
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
        if let Some(stats) = stats {
            let tables = meta.tables.lock().expect("mutex");
            let table = tables.get("t").expect("the test table exists");
            *table.stats.lock().expect("mutex") = Some(stats);
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
        logical
    }

    /// What constant folding buys the planner. The access path is unchanged —
    /// `is_row_constant` already accepts an arithmetic key, and the executor
    /// evaluates it once per scan either way — but a *folded* key is a literal,
    /// and only a literal reaches `const_of`, which is what lets
    /// `key_selectivity` consult the column's distribution instead of falling
    /// back to PostgreSQL's `var_eq_non_const` guess. The `Index Cond` line is
    /// the observable half of that difference.
    #[test]
    fn a_folded_key_becomes_a_literal_index_cond() {
        let sql = "SELECT id FROM t WHERE id = 2 + 3";
        let mut logical = bind_sql_indexed(sql, Some(pk_on_id()));
        assert_eq!(
            explain(&plan(logical.clone(), cost::CostSettings::default())),
            vec!["Index Scan using t_pkey on t", "  Index Cond: (id = 2 + 3)"],
            "unoptimized, the key is an expression the estimator cannot read"
        );
        crabgresql_optimizer::optimize(
            &mut logical,
            &crabgresql_optimizer::OptimizerContext::new(crabgresql_types::FmtCtx::utc_default()),
        );
        assert_eq!(
            explain(&plan(logical, cost::CostSettings::default())),
            vec!["Index Scan using t_pkey on t", "  Index Cond: (id = 5)"]
        );
    }

    /// A qual that folds to `TRUE` filters nothing, and disappears from the scan
    /// rather than being evaluated per row.
    #[test]
    fn a_constantly_true_qual_leaves_no_filter() {
        let mut logical = bind_sql_indexed("SELECT id FROM t WHERE 1 = 1", None);
        assert_eq!(
            explain(&plan(logical.clone(), cost::CostSettings::default())),
            vec!["Seq Scan on t", "  Filter: (1 = 1)"]
        );
        crabgresql_optimizer::optimize(
            &mut logical,
            &crabgresql_optimizer::OptimizerContext::new(crabgresql_types::FmtCtx::utc_default()),
        );
        assert_eq!(
            explain(&plan(logical, cost::CostSettings::default())),
            vec!["Seq Scan on t"]
        );
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
    fn a_lateral_right_side_stays_logical_and_is_never_hashed() {
        // Even with an equality across the join — the shape that would otherwise
        // become a hash key — the right side has to stay a per-left-row build:
        // there is no one rowset to hash.
        let PhysicalPlan::Join { source, .. } =
            plan_sql("SELECT * FROM t, LATERAL (SELECT t.id AS z) s WHERE t.id = s.z")
        else {
            panic!("expected Join");
        };
        let PhysicalJoinExpr::Lateral {
            right, right_width, ..
        } = &source
        else {
            panic!("expected a Lateral join node, not a hashable Join");
        };
        assert_eq!(*right_width, 1);
        assert!(matches!(right, JoinInput::Subplan(_)));
        assert!(!source.uses_hash_join());
        assert_eq!(explain_join(&source, None)[0], "Nested Loop");
    }

    #[test]
    fn a_lateral_join_keeps_the_left_columns_its_right_side_reads() {
        // `name` is the only column the target list asks for, but the lateral
        // body reads `id`. Pruning the scan to `name` alone would feed the
        // lateral side a NULL and change the answer.
        let PhysicalPlan::Join { source, .. } =
            plan_sql("SELECT s.z FROM t, LATERAL (SELECT t.id AS z) s")
        else {
            panic!("expected Join");
        };
        let PhysicalJoinExpr::Lateral { left, .. } = &source else {
            panic!("expected a Lateral join node");
        };
        let PhysicalJoinExpr::Input {
            input: PhysicalJoinInput::Scan { projection, .. },
            ..
        } = left.as_ref()
        else {
            panic!("expected a scan on the left");
        };
        let reads_id = match projection {
            ColumnProjection::All => true,
            ColumnProjection::Some(cols) => cols.contains(&0),
        };
        assert!(
            reads_id,
            "column 0 (`id`) is read by the lateral body, got {projection:?}"
        );
    }

    #[test]
    fn a_relation_read_only_inside_a_lateral_item_is_still_reported() {
        // `scan_projections` feeds `pg_depend`'s per-column view dependencies,
        // and the lateral side is the one part of the plan that is not planned
        // with the statement — so without `right_shape` this relation, and every
        // column edge on it, would be invisible.
        let plan = plan_sql("SELECT s.z FROM t a, LATERAL (SELECT b.big AS z FROM t b) s");
        let scanned: Vec<String> = scan_projections(&plan)
            .iter()
            .map(|(schema, _)| schema.name.clone())
            .collect();
        assert_eq!(
            scanned,
            vec!["t".to_string(), "t".to_string()],
            "both the left scan and the one inside the lateral body"
        );

        // And EXPLAIN shows the lateral side as the subtree it is, rather than a
        // one-line placeholder.
        let PhysicalPlan::Join { source, .. } = &plan else {
            panic!("expected Join");
        };
        let rendered = explain_join(source, None);
        assert_eq!(rendered[0], "Nested Loop");
        assert!(
            rendered
                .iter()
                .filter(|l| l.contains("Seq Scan on t"))
                .count()
                == 2,
            "the lateral body's scan is rendered too, got {rendered:?}"
        );
    }

    /// The root join node of a `PhysicalPlan::Join`, plus whatever predicate was
    /// left behind above it.
    fn join_root(plan: PhysicalPlan) -> (PhysicalJoinExpr, Option<BoundExpr>) {
        let PhysicalPlan::Join {
            source, predicate, ..
        } = plan
        else {
            panic!("expected Join");
        };
        (source, predicate)
    }

    fn join_parts(source: PhysicalJoinExpr) -> (JoinKind, Option<BoundExpr>, Vec<HashKey>) {
        let PhysicalJoinExpr::Join {
            kind,
            predicate,
            hash_keys,
            ..
        } = source
        else {
            panic!("expected a Join node, not a leaf");
        };
        (kind, predicate, hash_keys)
    }

    /// The kind is overwritten rather than written in SQL because no syntax
    /// binds to a semi/anti join (see [`JoinKind::Semi`]).
    fn with_kind(sql: &str, kind: JoinKind) -> PhysicalPlan {
        let mut plan = plan_sql(sql);
        let PhysicalPlan::Join { source, .. } = &mut plan else {
            panic!("expected Join");
        };
        let PhysicalJoinExpr::Join { kind: slot, .. } = source else {
            panic!("expected a Join node, not a leaf");
        };
        *slot = kind;
        plan
    }

    #[test]
    fn a_semi_or_anti_join_emits_only_its_left_row() {
        let plan = with_kind("SELECT * FROM t a, t b WHERE a.id = b.id", JoinKind::Semi);
        let PhysicalPlan::Join { source, .. } = &plan else {
            panic!("expected Join");
        };
        let PhysicalJoinExpr::Join { left, right, .. } = source else {
            panic!("expected a Join node, not a leaf");
        };
        assert!(right.width() > 0, "the right side has columns of its own");
        assert_eq!(
            source.width(),
            left.width(),
            "a semi join's output is the left row alone"
        );
    }

    #[test]
    fn explain_names_the_join_kind() {
        let hashed = "SELECT * FROM t a, t b WHERE a.id = b.id";
        let looped = "SELECT * FROM t a, t b WHERE a.big < b.big";
        // An inner join is the one kind PG leaves unnamed.
        assert_eq!(explain(&plan_sql(hashed))[0], "Hash Join");
        assert_eq!(explain(&plan_sql(looped))[0], "Nested Loop");

        for (kind, hash_label, loop_label) in [
            (JoinKind::Left, "Hash Left Join", "Nested Loop Left Join"),
            (JoinKind::Right, "Hash Right Join", "Nested Loop Right Join"),
            (JoinKind::Full, "Hash Full Join", "Nested Loop Full Join"),
            (JoinKind::Semi, "Hash Semi Join", "Nested Loop Semi Join"),
            (JoinKind::Anti, "Hash Anti Join", "Nested Loop Anti Join"),
        ] {
            assert_eq!(explain(&with_kind(hashed, kind))[0], hash_label);
            assert_eq!(explain(&with_kind(looped, kind))[0], loop_label);
        }
    }

    #[test]
    fn column_names_of_a_tree_follow_the_emitted_widths() {
        // A misaligned list makes EXPLAIN print a plausible wrong name: the `$n`
        // fallback only catches an index past the end.
        let mut plan = plan_sql("SELECT * FROM t a, t b, t c WHERE a.id = b.id AND b.big = c.big");
        let PhysicalPlan::Join { source, .. } = &mut plan else {
            panic!("expected Join");
        };
        let PhysicalJoinExpr::Join { left, .. } = source else {
            panic!("expected a Join node, not a leaf");
        };
        let PhysicalJoinExpr::Join { kind, .. } = left.as_mut() else {
            panic!("expected a nested Join node");
        };
        *kind = JoinKind::Semi;

        let names = join_column_names(source);
        assert_eq!(names.len(), source.width());
        // `t` twice, not three times: the semi join's own right side is gone.
        let one = ["id", "big", "name"].map(|n| Some(n.to_string()));
        assert_eq!(names, [one.clone(), one].concat());
    }

    #[test]
    fn comma_join_where_equality_becomes_a_hash_join() {
        // The headline property: a comma join whose WHERE carries the join
        // condition must reach the same physical shape as the explicit ON form.
        let (comma_source, comma_filter) =
            join_root(plan_sql("SELECT * FROM t a, t b WHERE a.id = b.id"));
        let (on_source, on_filter) =
            join_root(plan_sql("SELECT * FROM t a JOIN t b ON a.id = b.id"));

        let (comma_kind, comma_pred, comma_keys) = join_parts(comma_source);
        let (on_kind, on_pred, on_keys) = join_parts(on_source);

        assert_eq!(comma_kind, on_kind, "kind must match the explicit-ON form");
        assert_eq!(
            comma_kind,
            JoinKind::Inner,
            "Cross must flip once conditioned"
        );
        assert_eq!(comma_keys, on_keys, "hash keys must match");
        assert_eq!(comma_keys.len(), 1);
        assert_eq!(comma_pred, on_pred, "residual must match");
        // The conjunct moved: nothing is left to re-check above the join.
        assert!(comma_filter.is_none());
        assert!(on_filter.is_none());
    }

    #[test]
    fn aggregate_over_comma_join_pushes_its_where() {
        // A grouped query keeps its WHERE on the Aggregate node, so extraction has
        // to run on that path too — this is the shape most of TPC-H has.
        let PhysicalPlan::Aggregate {
            input: PhysicalAggInput::Join(source),
            predicate,
            ..
        } = plan_sql("SELECT a.name, count(*) FROM t a, t b WHERE a.id = b.id GROUP BY a.name")
        else {
            panic!("expected Aggregate over Join");
        };
        let (kind, residual, hash_keys) = join_parts(source);
        assert_eq!(kind, JoinKind::Inner);
        assert_eq!(hash_keys.len(), 1);
        assert!(residual.is_none());
        assert!(predicate.is_none());
    }

    #[test]
    fn or_of_arms_sharing_an_equality_still_hashes() {
        // The TPC-H Q19 shape: the whole WHERE is one OR, but every arm repeats
        // the join equality and one single-relation restriction. Factoring those
        // out is what keeps this from planning as a Cartesian product.
        let PhysicalPlan::Aggregate {
            input: PhysicalAggInput::Join(source),
            predicate,
            ..
        } = plan_sql(
            "SELECT count(*) FROM t a, t b \
             WHERE (a.id = b.id AND a.name = 'x' AND b.big = 1) \
                OR (a.id = b.id AND a.name = 'x' AND b.big = 2)",
        )
        else {
            panic!("expected Aggregate over Join");
        };
        let PhysicalJoinExpr::Join {
            left,
            kind,
            right,
            predicate: residual,
            hash_keys,
        } = source
        else {
            panic!("expected a Join node");
        };
        assert_eq!(kind, JoinKind::Inner, "Cross must flip once conditioned");
        assert_eq!(hash_keys.len(), 1, "the factored equality is the hash key");
        // `a.name = 'x'` is common to both arms too, so it reaches the left leaf.
        let PhysicalJoinExpr::Input {
            predicate: Some(_), ..
        } = *left
        else {
            panic!("the common single-relation conjunct must sink to the leaf");
        };
        // What is left of the OR (`b.big = 1 OR b.big = 2`) touches only `b`, so
        // it sinks to the right leaf rather than filtering joined rows.
        let PhysicalJoinExpr::Input {
            predicate: Some(_), ..
        } = *right
        else {
            panic!("the residual OR must sink to the right leaf");
        };
        assert!(
            residual.is_none(),
            "nothing is left to re-check at the join"
        );
        assert!(predicate.is_none(), "nothing is left above the join");
    }

    #[test]
    fn three_way_comma_join_hashes_at_every_level() {
        // The TPC-H Q3 shape. The binder builds Cross(Cross(a, b), c); `a.id = b.id`
        // sinks to the inner node and `b.id = c.id` straddles the root, so both
        // levels get a key and no reordering is needed.
        let (source, filter) = join_root(plan_sql(
            "SELECT * FROM t a, t b, t c WHERE a.id = b.id AND b.id = c.id",
        ));
        let PhysicalJoinExpr::Join {
            left,
            kind,
            hash_keys,
            ..
        } = source
        else {
            panic!("expected Join node");
        };
        assert_eq!(kind, JoinKind::Inner);
        assert_eq!(hash_keys.len(), 1, "root joins (a,b) to c");

        let (inner_kind, inner_residual, inner_keys) = join_parts(*left);
        assert_eq!(inner_kind, JoinKind::Inner);
        assert_eq!(inner_keys.len(), 1, "inner node joins a to b");
        assert!(inner_residual.is_none());
        assert!(filter.is_none());
    }

    #[test]
    fn bushy_right_subtree_predicate_is_rebased() {
        // `t a, t b JOIN t c ON ...` is the one shape that nests a join under a
        // right child, and a right child's predicate is base-0 relative to its own
        // subtree while the WHERE is global. `b.big = c.big` is at global indices
        // 4 and 7 and must land at local 1 and 4 — the subtree starts at 3.
        let (source, filter) = join_root(plan_sql(
            "SELECT * FROM t a, t b JOIN t c ON b.id = c.id WHERE b.big = c.big",
        ));
        let PhysicalJoinExpr::Join {
            right,
            kind,
            hash_keys,
            ..
        } = source
        else {
            panic!("expected Join node");
        };
        // Nothing landed at the root: the conjunct belongs to the right subtree.
        assert_eq!(kind, JoinKind::Cross);
        assert!(hash_keys.is_empty());
        assert!(filter.is_none());

        let (sub_kind, sub_residual, sub_keys) = join_parts(*right);
        assert_eq!(sub_kind, JoinKind::Inner);
        assert!(sub_residual.is_none());
        assert_eq!(sub_keys.len(), 2, "the ON key plus the pushed one");
        let pushed = &sub_keys[1];
        assert_eq!(
            (&pushed.left, &pushed.right),
            (
                &BoundExpr::ColumnRef {
                    index: 1,
                    ty: PgType::Int8
                },
                &BoundExpr::ColumnRef {
                    index: 4,
                    ty: PgType::Int8
                }
            ),
            "rebased into the subtree's own index space, not left global"
        );
    }

    #[test]
    fn anti_join_predicate_is_not_pushed_below_a_left_join() {
        // The classic anti-join idiom. `b.big IS NULL` reads the null-supplying
        // side, so it must stay above the join: moving it into the ON (or below it)
        // would resurrect rows the WHERE drops.
        let (source, filter) = join_root(plan_sql(
            "SELECT * FROM t a LEFT JOIN t b ON a.id = b.id WHERE b.big IS NULL",
        ));
        let (kind, residual, hash_keys) = join_parts(source);
        assert_eq!(kind, JoinKind::Left);
        assert_eq!(hash_keys.len(), 1, "the ON equality is still a key");
        assert!(residual.is_none(), "nothing was attached to the ON");
        assert!(
            matches!(filter, Some(BoundExpr::IsNull { .. })),
            "the IS NULL stays above the join"
        );
    }

    #[test]
    fn full_join_takes_no_pushed_predicate() {
        // Both sides of a FULL join are null-supplying, so neither attaching nor
        // descending is sound — even for a conjunct over just one side.
        let (source, filter) = join_root(plan_sql(
            "SELECT * FROM t a FULL JOIN t b ON a.id = b.id WHERE a.big = 1",
        ));
        let (kind, residual, _) = join_parts(source);
        assert_eq!(kind, JoinKind::Full);
        assert!(residual.is_none());
        assert!(filter.is_some());
    }

    #[test]
    fn volatile_conjunct_is_not_pushed() {
        // Relocating a volatile conjunct changes how many times it runs: sunk to a
        // leaf it fires once per scanned row, where above the join it fired once
        // per joined row. `nextval` would advance a different number of times.
        let (source, filter) = join_root(plan_sql(
            "SELECT * FROM t a, t b WHERE a.id = b.id AND a.big > nextval('s')",
        ));
        let (kind, residual, hash_keys) = join_parts(source);
        assert_eq!(kind, JoinKind::Inner);
        assert_eq!(hash_keys.len(), 1, "the plain equality still pushes");
        assert!(residual.is_none(), "nothing sank to the join or its leaves");
        assert!(
            matches!(filter, Some(BoundExpr::Binary { op: BinOp::Gt, .. })),
            "the volatile comparison stays above the join"
        );
    }

    #[test]
    fn correlated_conjunct_is_not_pushed() {
        // A correlated EXISTS reports no column bounds — its dependency on this row
        // lives inside the subplan as an outer reference — so moving it to a
        // narrower row would silently read a different column. It stays put.
        let (source, filter) = join_root(plan_sql(
            "SELECT * FROM t a, t b WHERE a.id = b.id \
             AND EXISTS (SELECT 1 FROM t c WHERE c.id = a.id)",
        ));
        let (kind, residual, hash_keys) = join_parts(source);
        assert_eq!(kind, JoinKind::Inner);
        assert_eq!(hash_keys.len(), 1, "the plain equality still pushes");
        assert!(residual.is_none());
        assert!(
            matches!(filter, Some(BoundExpr::Exists { .. })),
            "the correlated EXISTS stays above the join"
        );
    }

    #[test]
    fn correlated_on_conjunct_is_not_sunk_to_a_leaf() {
        // Unlike the WHERE case above, this expression starts on the explicit
        // join node and reaches leaf sinking only after its equality became a
        // hash key. The scalar subplan still needs the full joined row when its
        // OuterColumnRef is substituted, so the residual must remain here.
        let (source, filter) = join_root(plan_sql(
            "SELECT * FROM t a JOIN t b ON a.id = b.id \
             AND b.big = (SELECT max(c.big) FROM t c WHERE c.id = a.big)",
        ));
        let PhysicalJoinExpr::Join {
            right,
            predicate,
            hash_keys,
            ..
        } = source
        else {
            panic!("expected Join node");
        };
        assert_eq!(hash_keys.len(), 1);
        assert!(
            matches!(predicate, Some(BoundExpr::Binary { op: BinOp::Eq, .. })),
            "the correlated ON residual stays on the join"
        );
        assert!(filter.is_none());
        let PhysicalJoinExpr::Input {
            predicate: right_leaf,
            ..
        } = *right
        else {
            panic!("expected a leaf on the right");
        };
        assert!(right_leaf.is_none());
    }

    #[test]
    fn volatile_on_conjunct_is_not_sunk_to_a_leaf() {
        // An explicit ON residual bypasses WHERE pushdown, but must receive the
        // same volatility screen before leaf sinking. Otherwise nextval runs
        // once per scanned right row instead of once per candidate join row.
        let (source, filter) = join_root(plan_sql(
            "SELECT * FROM t a JOIN t b \
             ON a.id = b.id AND b.big > nextval('s')",
        ));
        let PhysicalJoinExpr::Join {
            right,
            predicate,
            hash_keys,
            ..
        } = source
        else {
            panic!("expected Join node");
        };
        assert_eq!(hash_keys.len(), 1);
        assert!(
            matches!(predicate, Some(BoundExpr::Binary { op: BinOp::Gt, .. })),
            "the volatile ON residual stays on the join"
        );
        assert!(filter.is_none());
        let PhysicalJoinExpr::Input {
            predicate: right_leaf,
            ..
        } = *right
        else {
            panic!("expected a leaf on the right");
        };
        assert!(right_leaf.is_none());
    }

    #[test]
    #[should_panic(expected = "shifted out of range")]
    fn shifting_a_column_below_zero_panics_in_all_builds() {
        let mut expr = BoundExpr::ColumnRef {
            index: 0,
            ty: PgType::Int4,
        };
        expr.shift_column_refs(-1);
    }

    #[test]
    fn non_equi_comma_join_condition_lands_on_an_inner_node() {
        // A non-equi condition yields no hash key, so it rides the nested loop as
        // the node's predicate. The kind must flip to Inner: `Cross` is the
        // planner's marker for "unconditional", and leaving it would misdescribe
        // a node that now filters.
        let (source, filter) = join_root(plan_sql("SELECT * FROM t a, t b WHERE a.big < b.big"));
        let (kind, residual, hash_keys) = join_parts(source);
        assert_eq!(kind, JoinKind::Inner);
        assert!(hash_keys.is_empty());
        assert!(matches!(
            residual,
            Some(BoundExpr::Binary { op: BinOp::Lt, .. })
        ));
        assert!(filter.is_none());
    }

    #[test]
    fn single_relation_conjunct_sinks_to_its_leaf() {
        // `a.big = 7` restricts one relation, so it is not a join key — it sinks
        // all the way to a's scan, which is then filtered before the join pairs
        // anything (and, on the build side, before the hash table is populated).
        let (source, filter) = join_root(plan_sql(
            "SELECT * FROM t a, t b WHERE a.id = b.id AND a.big = 7",
        ));
        let PhysicalJoinExpr::Join {
            left,
            right,
            kind,
            predicate,
            hash_keys,
        } = source
        else {
            panic!("expected Join node");
        };
        assert_eq!(kind, JoinKind::Inner);
        assert_eq!(hash_keys.len(), 1);
        assert!(predicate.is_none(), "nothing left on the join node");
        assert!(filter.is_none(), "nothing left above the join");

        let PhysicalJoinExpr::Input {
            predicate: left_leaf,
            ..
        } = *left
        else {
            panic!("expected a leaf on the left");
        };
        assert!(matches!(
            left_leaf,
            Some(BoundExpr::Binary { op: BinOp::Eq, .. })
        ));
        let PhysicalJoinExpr::Input {
            predicate: right_leaf,
            ..
        } = *right
        else {
            panic!("expected a leaf on the right");
        };
        assert!(right_leaf.is_none(), "b is unrestricted");
    }

    #[test]
    fn right_leaf_filter_is_rebased_into_the_leaf_row() {
        // `b.big = 7` is at index 4 of the join's combined row; b's own row starts
        // at 3, so the sunk filter has to address index 1.
        let (source, _) = join_root(plan_sql(
            "SELECT * FROM t a, t b WHERE a.id = b.id AND b.big = 7",
        ));
        let PhysicalJoinExpr::Join { right, .. } = source else {
            panic!("expected Join node");
        };
        let PhysicalJoinExpr::Input {
            predicate: Some(BoundExpr::Binary { left, .. }),
            ..
        } = *right
        else {
            panic!("expected a filtered leaf on the right");
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
    fn outer_join_residual_is_not_sunk_to_a_leaf() {
        // The residual of an outer join mixes conjuncts whose safe direction is
        // opposite (a WHERE conjunct sinks into the preserved side, an ON conjunct
        // into the null-supplying one) and nothing records which is which, so
        // nothing sinks at all.
        let (source, _) = join_root(plan_sql(
            "SELECT * FROM t a LEFT JOIN t b ON a.id = b.id AND b.big > 5",
        ));
        let PhysicalJoinExpr::Join {
            right,
            kind,
            predicate,
            ..
        } = source
        else {
            panic!("expected Join node");
        };
        assert_eq!(kind, JoinKind::Left);
        assert!(
            matches!(predicate, Some(BoundExpr::Binary { op: BinOp::Gt, .. })),
            "the ON residual stays on the join node"
        );
        let PhysicalJoinExpr::Input {
            predicate: right_leaf,
            ..
        } = *right
        else {
            panic!("expected a leaf on the right");
        };
        assert!(right_leaf.is_none());
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
        assert_eq!(eq_key(&key), vec![(0, Value::Int4(1))]);
        assert!(key.lower.is_none() && key.upper.is_none());
        // The equality conjunct is fully consumed by the index.
        assert!(predicate.is_none());
    }

    /// A probe's equality keys as `(column, literal)`, for tests that care about
    /// which conjunct the index consumed rather than how it is spelled.
    fn eq_key(key: &IndexProbeSpec) -> Vec<(usize, Value)> {
        key.eq
            .iter()
            .map(|(column, value)| match value {
                BoundExpr::Const { value, .. } => (*column, value.clone()),
                other => panic!("expected a literal key value, got {other:?}"),
            })
            .collect()
    }

    /// A probe's bounds as `(column, inclusive, literal)`, lower then upper.
    fn bounds(key: &IndexProbeSpec) -> Vec<(usize, bool, Value)> {
        key.lower
            .iter()
            .chain(&key.upper)
            .map(|b| match &b.value {
                BoundExpr::Const { value, .. } => (b.column, b.inclusive, value.clone()),
                other => panic!("expected a literal bound value, got {other:?}"),
            })
            .collect()
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
        assert_eq!(eq_key(&key), vec![(0, Value::Int4(1))]);
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
    fn or_arms_sharing_an_equality_still_probe_the_index() {
        // The single-table counterpart of the Q19 shape: the key equality is
        // written once per arm, so it only becomes a probe once the common
        // conjunct has been factored out of the OR.
        let plan = plan_sql_indexed(
            "SELECT * FROM t WHERE (id = 1 AND name = 'x') OR (id = 1 AND name = 'y')",
            Some(pk_on_id()),
        );
        let PhysicalPlan::IndexScan { key, predicate, .. } = plan else {
            panic!("expected IndexScan");
        };
        assert_eq!(eq_key(&key), vec![(0, Value::Int4(1))]);
        // What is left of the OR is not an index key, so it filters at runtime.
        assert!(
            matches!(predicate, Some(BoundExpr::Binary { op: BinOp::Or, .. })),
            "the residual OR must survive as a filter"
        );
    }

    #[test]
    fn an_equality_in_only_one_or_arm_does_not_probe() {
        // Nothing is common to both arms, so the OR stays whole and a row with
        // `id <> 1` is still reachable — an index probe would drop it.
        let plan = plan_sql_indexed(
            "SELECT * FROM t WHERE (id = 1 AND name = 'x') OR name = 'y'",
            Some(pk_on_id()),
        );
        assert!(matches!(plan, PhysicalPlan::Select { .. }));
    }

    #[test]
    fn equality_on_unindexed_column_stays_seq_scan() {
        let plan = plan_sql_indexed("SELECT * FROM t WHERE name = 'x'", Some(pk_on_id()));
        assert!(
            matches!(plan, PhysicalPlan::Select { .. }),
            "unindexed column must not use an index scan"
        );
    }

    /// `t` as `ANALYZE` would leave it after 100k rows of `id` running 1..=100000
    /// in physical order: big enough, and correlated enough, for a narrow range
    /// through the index to beat reading everything.
    fn analyzed_ids() -> RelStats {
        let histogram = (0..=10)
            .map(|bucket| Value::Int4(bucket * 10_000))
            .collect();
        let id = ColStats {
            avg_width: 4,
            n_distinct: -1.0,
            histogram,
            correlation: 1.0,
            ..ColStats::default()
        };
        let blank = ColStats {
            avg_width: 8,
            ..ColStats::default()
        };
        RelStats {
            relpages: 1_000,
            reltuples: 100_000.0,
            analyzed: true,
            curpages: Some(1_000),
            columns: Arc::from([id, blank.clone(), blank]),
        }
    }

    /// A partial cover is only planned when the key columns it leaves
    /// unconstrained are `NOT NULL`. The engine cannot serve it otherwise — a
    /// row with a NULL there satisfies the probe and is not in the index — so
    /// planning one would advertise an index scan that degrades to a sequential
    /// scan, and quietly return too few rows if it did not.
    #[test]
    fn a_partial_cover_needs_its_unconstrained_key_columns_not_null() {
        let index = IndexMetadata {
            name: "t_id_big_idx".into(),
            keys: vec![
                IndexKey {
                    column: 0,
                    descending: false,
                    nulls_first: false,
                },
                IndexKey {
                    column: 1,
                    descending: false,
                    nulls_first: false,
                },
            ],
            unique: false,
            constraint: None,
            ..pk_on_id()
        };
        // `id = 1` covers only the first of the two key columns.
        let plan = |big_nullable: bool| {
            let mut stats = analyzed_ids();
            // Narrow enough that cost is not what decides this.
            stats.relpages = 10_000;
            plan_sql_analyzed_with(
                "SELECT * FROM t WHERE id = 1",
                Some(index.clone()),
                Some(stats),
                big_nullable,
            )
        };
        assert!(
            matches!(plan(true), PhysicalPlan::Select { .. }),
            "a nullable `big` leaves rows out of the index, so no probe"
        );
        let PhysicalPlan::IndexScan { key, .. } = plan(false) else {
            panic!("a NOT NULL `big` is servable and should be probed");
        };
        assert_eq!(eq_key(&key), vec![(0, Value::Int4(1))]);
    }

    #[test]
    fn a_narrow_range_on_the_pk_becomes_an_index_scan() {
        for (sql, want) in [
            (
                "SELECT * FROM t WHERE id > 99000",
                vec![(0, false, Value::Int4(99000))],
            ),
            (
                "SELECT * FROM t WHERE id >= 99000",
                vec![(0, true, Value::Int4(99000))],
            ),
            (
                "SELECT * FROM t WHERE id BETWEEN 1 AND 9",
                vec![(0, true, Value::Int4(1)), (0, true, Value::Int4(9))],
            ),
        ] {
            let plan = plan_sql_analyzed(sql, Some(pk_on_id()), Some(analyzed_ids()));
            let PhysicalPlan::IndexScan { key, predicate, .. } = plan else {
                panic!("expected IndexScan for {sql}");
            };
            assert!(key.eq.is_empty(), "{sql} pins no column by equality");
            assert_eq!(bounds(&key), want, "{sql}");
            assert!(predicate.is_none(), "{sql} leaves no residual");
        }
    }

    /// An index on `name`, the text column — where a range has to answer to the
    /// collation.
    fn index_on_name() -> IndexMetadata {
        IndexMetadata {
            name: "t_name_idx".into(),
            keys: vec![IndexKey {
                column: 2,
                descending: false,
                nulls_first: false,
            }],
            unique: false,
            constraint: None,
            ..pk_on_id()
        }
    }

    /// `t` analyzed with `name` running `'a'..'z'`, so a narrow text range would
    /// be worth an index scan if the collation allowed one.
    fn analyzed_names() -> RelStats {
        let mut stats = analyzed_ids();
        let name = ColStats {
            avg_width: 4,
            n_distinct: -1.0,
            histogram: ["a", "f", "k", "p", "u", "z"]
                .iter()
                .map(|s| Value::Text((*s).to_string()))
                .collect(),
            correlation: 1.0,
            ..ColStats::default()
        };
        let columns: Vec<ColStats> = stats
            .columns
            .iter()
            .take(2)
            .cloned()
            .chain([name])
            .collect();
        stats.columns = Arc::from(columns);
        stats
    }

    #[test]
    fn a_text_range_is_index_served_only_under_a_byte_order_collation() {
        // The index stores raw bytes, so its order *is* byte order. Under `C`
        // that is also the comparison's order and the range is exact; under an
        // ICU collation the two disagree (`'a' < 'B'` by the locale, not by the
        // bytes), so the stretch of the index between the bounds is not the set
        // of rows the predicate selects. Equality is unaffected either way —
        // every supported collation is deterministic.
        let served = |sql: &str| {
            matches!(
                plan_sql_analyzed(sql, Some(index_on_name()), Some(analyzed_names())),
                PhysicalPlan::IndexScan { .. }
            )
        };
        assert!(served("SELECT * FROM t WHERE name > 'y'"));
        assert!(served("SELECT * FROM t WHERE name > 'y' COLLATE \"C\""));
        assert!(
            !served("SELECT * FROM t WHERE name > 'y' COLLATE \"unicode\""),
            "an ICU-collated range must not be served from a byte-ordered index"
        );
        // …while the same collation on an equality is still probed.
        assert!(served(
            "SELECT * FROM t WHERE name = 'y' COLLATE \"unicode\""
        ));
    }

    /// The `Index Cond` spellings, as PostgreSQL 18.4 prints them for the same
    /// index and predicates: one condition bare inside its parentheses, several
    /// parenthesized individually and as a whole, bounds after the equality keys
    /// with the lower one first.
    #[test]
    fn explain_renders_range_index_conds_the_way_pg_does() {
        let cond = |sql: &str| {
            let plan = plan_sql_analyzed(sql, Some(pk_on_id()), Some(analyzed_ids()));
            explain(&plan)
                .into_iter()
                .find(|line| line.starts_with("  Index Cond:"))
                .unwrap_or_else(|| panic!("no Index Cond for {sql}"))
        };
        assert_eq!(
            cond("SELECT * FROM t WHERE id = 1"),
            "  Index Cond: (id = 1)"
        );
        assert_eq!(
            cond("SELECT * FROM t WHERE id > 99000"),
            "  Index Cond: (id > 99000)"
        );
        assert_eq!(
            cond("SELECT * FROM t WHERE id >= 99000"),
            "  Index Cond: (id >= 99000)"
        );
        assert_eq!(
            cond("SELECT * FROM t WHERE id BETWEEN 1 AND 9"),
            "  Index Cond: ((id >= 1) AND (id <= 9))"
        );
    }

    #[test]
    fn a_wide_range_stays_a_seq_scan() {
        // Two thirds of the relation: reading it all sequentially beats fetching
        // that many pages at random, index or no index. The cost model decides
        // this, not the shape of the predicate — which is the whole reason a
        // range path had to be costed rather than always taken.
        let plan = plan_sql_analyzed(
            "SELECT * FROM t WHERE id > 33000",
            Some(pk_on_id()),
            Some(analyzed_ids()),
        );
        assert!(matches!(plan, PhysicalPlan::Select { .. }));
    }

    #[test]
    fn a_reversed_range_bounds_the_same_end() {
        // `99000 < id` is `id > 99000`: the constant on the left flips which end
        // of the range it bounds, and reading it the other way would select
        // everything below the bound instead of above it.
        let plan = plan_sql_analyzed(
            "SELECT * FROM t WHERE 99000 < id",
            Some(pk_on_id()),
            Some(analyzed_ids()),
        );
        let PhysicalPlan::IndexScan { key, .. } = plan else {
            panic!("expected IndexScan");
        };
        assert!(key.upper.is_none(), "`99000 < id` is a lower bound");
        assert_eq!(bounds(&key), vec![(0, false, Value::Int4(99000))]);
    }

    #[test]
    fn equality_without_index_stays_seq_scan() {
        let plan = plan_sql_indexed("SELECT * FROM t WHERE id = 1", None);
        assert!(matches!(plan, PhysicalPlan::Select { .. }));
    }

    /// The probe a single-table DML plan chose for its own relation.
    fn direct_probe(plan: &PhysicalPlan) -> &Option<DmlIndexProbe> {
        match plan {
            PhysicalPlan::Update { probe, .. } | PhysicalPlan::Delete { probe, .. } => probe,
            _ => panic!("expected a DML plan"),
        }
    }

    #[test]
    fn dml_on_pk_equality_probes_the_index() {
        for sql in [
            "UPDATE t SET name = 'x' WHERE id = 1",
            "DELETE FROM t WHERE id = 1",
        ] {
            let plan = plan_sql_indexed(sql, Some(pk_on_id()));
            let Some(probe) = direct_probe(&plan) else {
                panic!("expected a probe for {sql}");
            };
            assert_eq!(probe.index_name, "t_pkey");
            assert_eq!(eq_key(&probe.key), vec![(0, Value::Int4(1))]);
        }
    }

    #[test]
    fn a_dml_probe_leaves_the_whole_predicate_in_place() {
        // Unlike a read, the probe consumes no conjunct: the modify node still
        // re-checks `id = 1` along with `name = 'y'`. The residual it carries is
        // for EXPLAIN only.
        let plan = plan_sql_indexed(
            "UPDATE t SET big = 1 WHERE id = 1 AND name = 'y'",
            Some(pk_on_id()),
        );
        let Some(probe) = direct_probe(&plan) else {
            panic!("expected a probe");
        };
        let Some(BoundExpr::Binary {
            op: BinOp::Eq,
            arg_ty: PgType::Text,
            ..
        }) = &probe.residual
        else {
            panic!("expected the uncovered text equality as the residual");
        };
        let PhysicalPlan::Update { predicate, .. } = &plan else {
            panic!("expected Update");
        };
        let Some(BoundExpr::Binary { op: BinOp::And, .. }) = predicate else {
            panic!("expected the full AND predicate to survive");
        };
    }

    #[test]
    fn update_writing_the_unique_key_stays_seq_scan() {
        // Writing `id` means the UNIQUE check needs every row, which a probe
        // cannot supply — see `update_needs_unique_snapshot`.
        let plan = plan_sql_indexed("UPDATE t SET id = 2 WHERE id = 1", Some(pk_on_id()));
        assert!(direct_probe(&plan).is_none(), "expected no probe");
    }

    #[test]
    fn dml_without_a_usable_index_stays_seq_scan() {
        for (sql, index) in [
            ("UPDATE t SET big = 1 WHERE id = 1", None),
            ("DELETE FROM t WHERE name = 'x'", Some(pk_on_id())),
            ("DELETE FROM t", Some(pk_on_id())),
        ] {
            let plan = plan_sql_indexed(sql, index);
            assert!(
                direct_probe(&plan).is_none(),
                "{sql} must not probe an index"
            );
        }
    }

    #[test]
    fn explain_renders_a_child_per_dml_target() {
        // Probed: the covered conjunct as Index Cond, the rest as Filter — the
        // split PG prints.
        let lines = explain(&plan_sql_indexed(
            "DELETE FROM t WHERE id = 1 AND name = 'y'",
            Some(pk_on_id()),
        ));
        assert_eq!(
            lines,
            vec![
                "Delete on t",
                "  ->  Index Scan using t_pkey on t",
                "        Index Cond: (id = 1)",
                "        Filter: (name = y)",
            ]
        );

        // Unprobed: the child is still rendered, carrying the whole predicate.
        let lines = explain(&plan_sql_indexed("DELETE FROM t WHERE id = 1", None));
        assert_eq!(
            lines,
            vec![
                "Delete on t",
                "  ->  Seq Scan on t",
                "        Filter: (id = 1)"
            ]
        );

        // An unfiltered DML still names the relation it reads.
        let lines = explain(&plan_sql("DELETE FROM t"));
        assert_eq!(lines, vec!["Delete on t", "  ->  Seq Scan on t"]);
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
    fn explain_renders_a_comma_join_as_a_hash_join() {
        // The whole point of the extraction is observable here: a comma join with
        // its condition in the WHERE now reports a Hash Join with a Hash Cond, and
        // the single-relation restriction shows as a Filter on the scan it sank to.
        let lines = explain(&plan_sql(
            "SELECT * FROM t a, t b WHERE a.id = b.id AND b.big = 7",
        ));
        assert_eq!(
            lines,
            vec![
                "Hash Join",
                "  Hash Cond: (id = id)",
                "  ->  Seq Scan on t",
                "  ->  Hash",
                "        ->  Seq Scan on t",
                "              Filter: (big = 7)",
            ]
        );
    }

    #[test]
    fn explain_attaches_a_subplan_leaf_filter_to_the_subplan_root() {
        let lines = explain(&plan_sql(
            "SELECT * FROM t a \
             JOIN (SELECT x.id, x.big FROM t x, t y WHERE x.id = y.id) b \
               ON a.id = b.id AND b.big = 7",
        ));
        let filter = lines
            .iter()
            .position(|line| line.contains("Filter:"))
            .unwrap_or_else(|| panic!("expected the sunk filter: {lines:?}"));
        let subplan_child = lines
            .iter()
            .enumerate()
            .skip(filter + 1)
            .find(|(_, line)| line.contains("->  Seq Scan on t"))
            .map(|(index, _)| index)
            .expect("expected a child of the subplan root");
        assert!(
            filter < subplan_child,
            "the filter must be a property of the subplan root, not its final leaf: {lines:?}"
        );
    }

    #[test]
    fn explain_renders_a_non_equi_join_as_a_nested_loop() {
        let lines = explain(&plan_sql("SELECT * FROM t a, t b WHERE a.big < b.big"));
        assert_eq!(
            lines,
            vec![
                "Nested Loop",
                "  Join Filter: (big < big)",
                "  ->  Seq Scan on t",
                "  ->  Seq Scan on t",
            ]
        );
    }

    #[test]
    fn explain_renders_an_unpushable_where_above_the_join() {
        // The anti-join idiom: `b.big IS NULL` may not move, so it stays a Filter
        // on the join node itself rather than becoming part of the ON.
        let lines = explain(&plan_sql(
            "SELECT * FROM t a LEFT JOIN t b ON a.id = b.id WHERE b.big IS NULL",
        ));
        assert_eq!(lines[0], "Hash Left Join");
        assert_eq!(lines[1], "  Hash Cond: (id = id)");
        assert_eq!(lines[2], "  Filter: (big IS NULL)");
    }

    /// The `Filter:` line of the plan for `sql`.
    fn filter_line(sql: &str) -> String {
        explain(&plan_sql(sql))
            .into_iter()
            .find(|l| l.contains("Filter:"))
            .expect("no Filter line")
    }

    #[test]
    fn explain_parenthesizes_a_composite_operand() {
        // PG's `EXPLAIN` output wraps anything that is not a bare column,
        // constant or parameter, so a test over a test nests rather than
        // running together.
        // `big IS NULL IS TRUE` would be a different (and unreadable) tree.
        assert_eq!(
            filter_line("SELECT * FROM t WHERE (big IS NULL) IS TRUE"),
            "  Filter: ((big IS NULL) IS TRUE)"
        );
        assert_eq!(
            filter_line("SELECT * FROM t WHERE (id = 1) IS NOT FALSE"),
            "  Filter: ((id = 1) IS NOT FALSE)"
        );
        // A leaf operand stays bare.
        assert_eq!(
            filter_line("SELECT * FROM t WHERE name IS NULL"),
            "  Filter: (name IS NULL)"
        );
    }

    #[test]
    fn explain_spells_every_boolean_test() {
        for spelling in [
            "IS TRUE",
            "IS NOT TRUE",
            "IS FALSE",
            "IS NOT FALSE",
            "IS UNKNOWN",
            "IS NOT UNKNOWN",
        ] {
            assert_eq!(
                filter_line(&format!("SELECT * FROM t WHERE (id = 1) {spelling}")),
                format!("  Filter: ((id = 1) {spelling})")
            );
        }
    }

    #[test]
    fn explain_nests_a_join_under_an_aggregate() {
        // Without this the plan shape is invisible for grouped queries — which is
        // most of TPC-H.
        let lines = explain(&plan_sql("SELECT count(*) FROM t a, t b WHERE a.id = b.id"));
        assert_eq!(
            lines,
            vec![
                "Aggregate",
                "  ->  Hash Join",
                "        Hash Cond: (id = id)",
                "        ->  Seq Scan on t",
                "        ->  Hash",
                "              ->  Seq Scan on t",
            ]
        );
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
            arms: [leaf("sales_2023"), leaf("sales_2024")]
                .into_iter()
                .map(identity_arm)
                .collect(),
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
        assert_eq!(
            explain(&setop_plan(Vec::new(), distinct.clone()))[0..2],
            ["HashAggregate".to_string(), "  ->  Append".to_string()]
        );
        assert_eq!(
            explain(&setop_plan(sort, distinct))[0..3],
            [
                "Sort".to_string(),
                "  ->  HashAggregate".to_string(),
                "        ->  Append".to_string()
            ]
        );
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

#[cfg(test)]
mod projection_tests {
    //! The column-projection pass: SQL in, the projection stamped on each scan
    //! leaf out. The fixture table is `t(id int4, big int8, name text)`.

    use super::tests::{MetaEngine, identity_arm, plan_sql};
    use super::*;
    use crabgresql_storage_api::{Column, TableEngine};

    /// The stamped projection, as a plain `Vec` (`None` = every column).
    fn cols(projection: &ColumnProjection) -> Option<Vec<usize>> {
        match projection {
            ColumnProjection::All => None,
            ColumnProjection::Some(cols) => Some(cols.to_vec()),
        }
    }

    fn select_projection(sql: &str) -> Option<Vec<usize>> {
        match plan_sql(sql) {
            PhysicalPlan::Select { projection, .. } => cols(&projection),
            other => panic!("expected Select for `{sql}`, got {}", explain(&other)[0]),
        }
    }

    #[test]
    fn a_scan_reads_only_the_columns_its_projections_and_where_name() {
        assert_eq!(
            select_projection("SELECT id FROM t WHERE name = 'x'"),
            Some(vec![0, 2])
        );
    }

    #[test]
    fn select_star_reads_every_column() {
        assert_eq!(select_projection("SELECT * FROM t"), None);
        // A set covering the width normalizes to `All`, not `Some([0,1,2])`.
        assert_eq!(select_projection("SELECT id, big, name FROM t"), None);
    }

    /// A hidden ORDER BY column arrives as an extra projection, so it is
    /// counted — `sort` itself indexes the projected tuple and must be ignored.
    #[test]
    fn an_order_by_column_outside_the_select_list_is_still_read() {
        assert_eq!(
            select_projection("SELECT id FROM t ORDER BY name"),
            Some(vec![0, 2])
        );
    }

    /// A correlated subquery hides its dependency on this row inside its body,
    /// so the pass must fall back to reading everything.
    #[test]
    fn a_correlated_subquery_forces_every_column() {
        assert_eq!(
            select_projection(
                "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM t x WHERE x.big = t.big)"
            ),
            None
        );
    }

    fn agg_projection(sql: &str) -> Option<Vec<usize>> {
        match plan_sql(sql) {
            PhysicalPlan::Aggregate {
                input: PhysicalAggInput::Scan { projection, .. },
                ..
            } => cols(&projection),
            PhysicalPlan::Aggregate {
                input: PhysicalAggInput::Join(source),
                ..
            } => match source {
                PhysicalJoinExpr::Input {
                    input: PhysicalJoinInput::Scan { projection, .. },
                    ..
                } => cols(&projection),
                other => panic!("expected a single scan leaf, got {:?}", other.width()),
            },
            other => panic!("expected Aggregate for `{sql}`, got {}", explain(&other)[0]),
        }
    }

    /// `count(*)` reads no column at all. Rather than a zero-column read, the
    /// projection normalizes to the single narrowest one — `id`, the only
    /// 4-byte fixed-width column — so the scan still pays for just one.
    #[test]
    fn count_star_reads_one_narrow_column() {
        assert_eq!(agg_projection("SELECT count(*) FROM t"), Some(vec![0]));
    }

    #[test]
    fn an_aggregate_reads_its_where_group_keys_and_arguments() {
        assert_eq!(
            agg_projection("SELECT count(*) FROM t WHERE big = 1"),
            Some(vec![1])
        );
        assert_eq!(
            agg_projection("SELECT name, sum(big) FROM t GROUP BY name"),
            Some(vec![1, 2])
        );
    }

    /// The join split: demand is expressed over the concatenated `left || right`
    /// row, so the right side's indices have to be rebased by `left.width()`
    /// before they reach the right subtree.
    #[test]
    fn a_join_splits_its_demand_across_the_width_boundary() {
        // a.id=0 a.big=1 a.name=2 | b.id=3 b.big=4 b.name=5
        // read: a.id (0), b.name (5); join key: a.big (1) = b.big (4)
        let PhysicalPlan::Join { source, .. } =
            plan_sql("SELECT a.id, b.name FROM t a JOIN t b ON a.big = b.big")
        else {
            panic!("expected Join");
        };
        let PhysicalJoinExpr::Join { left, right, .. } = source else {
            panic!("expected a binary join");
        };
        let leaf = |node: PhysicalJoinExpr| match node {
            PhysicalJoinExpr::Input {
                input: PhysicalJoinInput::Scan { projection, .. },
                ..
            } => cols(&projection),
            _ => panic!("expected a scan leaf"),
        };
        assert_eq!(leaf(*left), Some(vec![0, 1]), "left keeps its own indices");
        assert_eq!(
            leaf(*right),
            Some(vec![1, 2]),
            "right rebases by left width"
        );
    }

    /// Demand threads down through a derived table: the inner `SELECT *`
    /// projects every column, but only the one the outer query reads is
    /// actually needed from the table.
    ///
    /// This is the shape a view expands to, and the one an earlier version of
    /// this pass could not see through — it matched a literal `Append` child and
    /// so read all three columns here.
    #[test]
    fn demand_threads_through_a_derived_table() {
        let plan = plan_sql("SELECT id FROM (SELECT * FROM t) s");
        let PhysicalPlan::Subquery { source, .. } = plan else {
            panic!("expected a Subquery over the derived table");
        };
        let PhysicalPlan::Select { projection, .. } = *source else {
            panic!("expected the derived table to lower to a Select");
        };
        assert_eq!(cols(&projection), Some(vec![0]));
    }

    /// Two levels of nesting, and a predicate at the inner level that reads a
    /// column the outer query never sees.
    #[test]
    fn demand_threads_through_nested_derived_tables() {
        let plan = plan_sql("SELECT id FROM (SELECT * FROM (SELECT * FROM t WHERE big > 1) a) b");
        let mut node = &plan;
        loop {
            match node {
                PhysicalPlan::Subquery { source, .. } => node = source,
                PhysicalPlan::Select { projection, .. } => {
                    // `id` for the output, `big` for the inner WHERE.
                    assert_eq!(cols(projection), Some(vec![0, 1]));
                    return;
                }
                other => panic!("unexpected node {}", explain(other)[0]),
            }
        }
    }

    /// `LIMIT` has no projections of its own — its output row is its source
    /// row — so demand passes straight through it.
    #[test]
    fn demand_threads_through_a_limit() {
        let plan = plan_sql("SELECT id FROM (SELECT * FROM t LIMIT 5) s");
        let mut node = &plan;
        loop {
            match node {
                PhysicalPlan::Subquery { source, .. } => node = source,
                PhysicalPlan::Limit { source, .. } => node = source,
                PhysicalPlan::Select { projection, .. } => {
                    assert_eq!(cols(projection), Some(vec![0]));
                    return;
                }
                other => panic!("unexpected node {}", explain(other)[0]),
            }
        }
    }

    /// The window arm of the projection pass has to split the parent's demand at
    /// `input_width`: a demanded *slot* is computed by the window node, not read
    /// from below, so forwarding it would land past the source's own projection
    /// width and trip `through_tail`'s fail-safe — silently reverting every
    /// window query to reading all columns.
    #[test]
    fn a_window_query_still_prunes_the_columns_it_does_not_read() {
        let plan = plan_sql("SELECT id, rank() OVER (ORDER BY name) FROM t");
        let PhysicalPlan::Subquery { source, .. } = plan else {
            panic!("expected a Subquery wrapping the window chain");
        };
        let PhysicalPlan::Window { source, .. } = *source else {
            panic!("expected a Window");
        };
        let PhysicalPlan::Select { projection, .. } = *source else {
            panic!("expected a Select");
        };
        assert_eq!(
            cols(&projection),
            Some(vec![0, 2]),
            "`big` is read by neither the target list nor the OVER clause"
        );
    }

    /// A partition key that is never projected still decides the partitions, so
    /// it must survive pruning — the same rule the `Aggregate` arm applies to a
    /// group key.
    #[test]
    fn a_window_partition_key_is_read_even_when_it_is_not_projected() {
        let plan = plan_sql("SELECT rank() OVER (PARTITION BY big ORDER BY name) FROM t");
        let PhysicalPlan::Subquery { source, .. } = plan else {
            panic!("expected a Subquery wrapping the window chain");
        };
        let PhysicalPlan::Window { source, .. } = *source else {
            panic!("expected a Window");
        };
        let PhysicalPlan::Select { projection, .. } = *source else {
            panic!("expected a Select");
        };
        assert_eq!(cols(&projection), Some(vec![1, 2]));
    }

    /// PG evaluates the spec with the most keys first and the fewest last, and
    /// numbers them `w1`, `w2`, … in that order — so `w1` is the bottom of the
    /// chain. The order is observable: the last spec's sort is what a window
    /// query with no ORDER BY of its own returns rows in.
    #[test]
    fn explain_nests_one_window_agg_per_spec_in_evaluation_order() {
        let plan = plan_sql(
            "SELECT rank() OVER (ORDER BY name), \
             sum(big) OVER (PARTITION BY id ORDER BY name) FROM t",
        );
        assert_eq!(
            explain(&plan),
            [
                "WindowAgg".to_string(),
                "  Window: w2 AS (ORDER BY name)".to_string(),
                "  ->  WindowAgg".to_string(),
                "        Window: w1 AS (PARTITION BY id ORDER BY name)".to_string(),
                "        ->  Seq Scan on t".to_string(),
            ]
        );
    }

    /// A direction and NULL placement are printed only when they are not the
    /// default for that direction, as PG does.
    #[test]
    fn explain_prints_only_non_default_window_sort_options() {
        let plan = plan_sql("SELECT rank() OVER (ORDER BY name DESC, id NULLS FIRST) FROM t");
        assert_eq!(
            explain(&plan)[1],
            "  Window: w1 AS (ORDER BY name DESC, id NULLS FIRST)"
        );
    }

    /// A hidden ORDER BY column lives in the inner node's `projections` past the
    /// visible width. The outer demand does not name it, so it survives only
    /// because `through_tail` folds the sort keys into the node's own demand.
    #[test]
    fn a_hidden_order_by_column_survives_threading() {
        let plan = plan_sql("SELECT id FROM (SELECT * FROM t) s ORDER BY name");
        let PhysicalPlan::Subquery { source, .. } = plan else {
            panic!("expected a Subquery");
        };
        let PhysicalPlan::Select { projection, .. } = *source else {
            panic!("expected a Select");
        };
        assert_eq!(
            cols(&projection),
            Some(vec![0, 2]),
            "`name` is read for the sort even though it is not in the select list"
        );
    }

    /// A set-operation arm still prunes from its **own** tail — it just does not
    /// receive the consumer's demand, because a non-`ALL` `UNION` deduplicates
    /// on every output column and an arm's coercion may hold a NULL constant
    /// that references no source column.
    #[test]
    fn a_set_operation_arm_prunes_from_its_own_tail_only() {
        let PhysicalPlan::SetOp { arms, .. } =
            plan_sql("SELECT id FROM t UNION ALL SELECT big FROM t")
        else {
            panic!("expected a SetOp");
        };
        let projection = |arm: &PhysicalSetOpArm| match &arm.plan {
            PhysicalPlan::Select { projection, .. } => cols(projection),
            other => panic!(
                "expected each arm to be a Select, got {}",
                explain(other)[0]
            ),
        };
        assert_eq!(projection(&arms[0]), Some(vec![0]));
        assert_eq!(projection(&arms[1]), Some(vec![1]));

        // But an arm that selects everything is not narrowed by what the
        // consumer of the set operation happens to read.
        let PhysicalPlan::Subquery { source, .. } =
            plan_sql("SELECT id FROM (SELECT * FROM t UNION ALL SELECT * FROM t) s")
        else {
            panic!("expected a Subquery over the set operation");
        };
        let PhysicalPlan::SetOp { arms, .. } = *source else {
            panic!("expected a SetOp under the subquery");
        };
        assert_eq!(projection(&arms[0]), None);
    }

    /// An `Append` fans one projection out to every leaf, so leaves that do not
    /// share a width must disable pruning rather than hand a leaf an ordinal
    /// past its own end — which would panic inside the storage engine.
    #[test]
    fn append_leaves_of_differing_width_disable_pruning() {
        let engine: Arc<dyn TableEngine> = Arc::new(MetaEngine::default());
        let leaf = |name: &str, types: &[PgType]| {
            engine
                .create_table(TableSchema::in_namespace(
                    name,
                    "public",
                    types
                        .iter()
                        .enumerate()
                        .map(|(i, ty)| Column::new(format!("c{i}"), *ty))
                        .collect(),
                ))
                .expect("create leaf")
        };
        let wide = leaf("wide", &[PgType::Int4, PgType::Int4, PgType::Int4]);
        let narrow = leaf("narrow", &[PgType::Int4, PgType::Int4]);

        let mut plan = PhysicalPlan::Append {
            arms: [wide, narrow].into_iter().map(identity_arm).collect(),
            columns: vec![
                OutputColumn::new("c0", PgType::Int4),
                OutputColumn::new("c1", PgType::Int4),
                OutputColumn::new("c2", PgType::Int4),
            ],
        };
        projection::push_column_projections(&mut plan);
        let PhysicalPlan::Append { arms, .. } = &plan else {
            unreachable!()
        };
        for arm in arms {
            assert_eq!(cols(&arm.projection), None);
        }
    }

    /// DML reads whole rows: the row is rebuilt by ordinal and RETURNING may
    /// name any column.
    #[test]
    fn dml_plans_read_every_column() {
        for sql in [
            "UPDATE t SET id = 1 WHERE big = 2",
            "DELETE FROM t WHERE big = 2",
        ] {
            // Neither carries a scan-leaf projection field at all; planning them
            // simply must not panic, and their scans stay unprojected by
            // construction (the executor passes `All` explicitly).
            let _ = plan_sql(sql);
        }
    }
}

#[cfg(test)]
mod decorrelate_tests {
    //! What `crabgresql_optimizer::DecorrelateSubqueries` leaves for the planner
    //! to plan. The fixture table is `t(id int4, big int8, name text)`, joined
    //! against itself: a correlated subquery needs two relations and one is
    //! enough to alias twice.

    use super::tests::bind_sql_indexed;
    use super::*;

    /// Bind, optimize, plan, explain — the whole path a statement takes.
    fn explain_optimized(sql: &str) -> Vec<String> {
        let mut logical = bind_sql_indexed(sql, None);
        crabgresql_optimizer::optimize(
            &mut logical,
            &crabgresql_optimizer::OptimizerContext::new(crabgresql_types::FmtCtx::utc_default()),
        );
        explain(&plan(logical, cost::CostSettings::default()))
    }

    /// The shape ① produces: `EXISTS` is a membership test, so it becomes a
    /// semi join whose right side projects the stripped correlation key. The
    /// residual (`b.big > 3`) stays inside that arm, where the planner sinks it
    /// to the scan.
    #[test]
    fn exists_becomes_a_semi_join() {
        assert_eq!(
            explain_optimized(
                "SELECT a.id FROM t a WHERE EXISTS (SELECT 1 FROM t b WHERE b.id = a.id AND b.big > 3)"
            ),
            vec![
                "Hash Semi Join",
                "  Hash Cond: (id = $3)",
                "  ->  Seq Scan on t",
                "  ->  Hash",
                "        ->  Seq Scan on t",
                "              Filter: (big > 3)",
            ]
        );
    }

    /// `NOT EXISTS` is the same rewrite with the complementary kind — and no
    /// `IS NULL` idiom to go with it, since an anti join emits a left row whose
    /// key is NULL by itself.
    #[test]
    fn not_exists_becomes_an_anti_join() {
        assert_eq!(
            explain_optimized(
                "SELECT a.id FROM t a WHERE NOT EXISTS (SELECT 1 FROM t b WHERE b.id = a.id)"
            )[0],
            "Hash Anti Join"
        );
    }

    /// `IN` is `= ANY`, and the needle joins the correlation keys as one more
    /// condition — here `a.id = b.id` beside the correlation `a.big = b.big`.
    /// Both become hash keys, which is the whole point of doing this before the
    /// planner rather than inside the executor.
    #[test]
    fn a_correlated_in_becomes_a_semi_join_on_both() {
        assert_eq!(
            explain_optimized(
                "SELECT a.id FROM t a WHERE a.id IN (SELECT b.id FROM t b WHERE b.big = a.big)"
            ),
            vec![
                "Hash Semi Join",
                "  Hash Cond: (big = $3) AND (id = $4)",
                "  ->  Seq Scan on t",
                "  ->  Hash",
                "        ->  Seq Scan on t",
            ]
        );
    }

    /// An *uncorrelated* `IN` is worth the same rewrite: the executor would
    /// otherwise fold it to a list of candidates and scan that list once per
    /// outer row.
    #[test]
    fn an_uncorrelated_in_becomes_a_semi_join_too() {
        assert_eq!(
            explain_optimized("SELECT a.id FROM t a WHERE a.id IN (SELECT b.id FROM t b)")[0],
            "Hash Semi Join"
        );
    }

    /// A correlated conjunct that is *not* an equality — TPC-H Q21's
    /// `l2.l_suppkey <> l1.l_suppkey` — cannot be a hash key, but it can still be
    /// part of the match test. It rides into the `ON` clause as a residual, and
    /// the arm projects the column it reads so that it is there to be read.
    #[test]
    fn a_correlated_residual_rides_into_the_join_condition() {
        assert_eq!(
            explain_optimized(
                "SELECT a.id FROM t a \
                 WHERE EXISTS (SELECT 1 FROM t b WHERE b.id = a.id AND b.big <> a.big)"
            ),
            vec![
                "Hash Semi Join",
                "  Hash Cond: (id = $3)",
                "  Join Filter: ($4 <> big)",
                "  ->  Seq Scan on t",
                "  ->  Hash",
                "        ->  Seq Scan on t",
            ]
        );
    }

    /// `NOT IN` binds as `<> ALL`, whose NULL semantics are not an anti join's.
    /// It stays a per-row subquery — visible here as a plan with no join in it
    /// at all.
    #[test]
    fn not_in_is_left_alone() {
        assert_eq!(
            explain_optimized("SELECT a.id FROM t a WHERE a.id NOT IN (SELECT b.id FROM t b)")[0],
            "Seq Scan on t"
        );
    }

    /// An `EXISTS` over an aggregate is *always* true — an implicit group emits
    /// its row whether or not anything fell into it — so the semi join on the
    /// correlation keys would wrongly drop the outer rows with no match.
    #[test]
    fn exists_over_an_aggregate_is_left_alone() {
        assert_eq!(
            explain_optimized(
                "SELECT a.id FROM t a WHERE EXISTS (SELECT count(*) FROM t b WHERE b.id = a.id)"
            )[0],
            "Seq Scan on t"
        );
    }

    /// A marker under an `OR` is not a filter the join may apply: it decides one
    /// operand of a boolean, not whether the row survives.
    #[test]
    fn a_marker_under_an_or_is_left_alone() {
        assert_eq!(
            explain_optimized(
                "SELECT a.id FROM t a WHERE a.big = 1 \
                 OR EXISTS (SELECT 1 FROM t b WHERE b.id = a.id)"
            )[0],
            "Seq Scan on t"
        );
    }

    /// The shape ② produces: the subquery becomes a grouped arm, joined on the
    /// correlation and read as a column. The comparison that consumed it stays
    /// above the join as an ordinary filter.
    #[test]
    fn a_scalar_aggregate_becomes_a_grouped_left_join() {
        assert_eq!(
            explain_optimized(
                "SELECT a.id FROM t a WHERE a.big < (SELECT avg(b.big) FROM t b WHERE b.id = a.id)"
            ),
            vec![
                "Hash Left Join",
                "  Hash Cond: (id = $3)",
                "  Filter: (big < $4)",
                "  ->  Seq Scan on t",
                "  ->  Hash",
                "        ->  Aggregate",
            ]
        );
    }

    /// A left join answers a missing group with NULL, which is what every
    /// aggregate but `count` returns for an empty input. `count` returns 0, so
    /// the substituted column is wrapped — without which an outer row with no
    /// match would compare against NULL instead of 0.
    #[test]
    fn a_correlated_count_keeps_its_zero() {
        let lines = explain_optimized(
            "SELECT a.id FROM t a WHERE a.big < (SELECT count(*) FROM t b WHERE b.id = a.id)",
        );
        assert_eq!(lines[0], "Hash Left Join");
        assert_eq!(
            lines[2], "  Filter: (big < …)",
            "the filter reads a COALESCE, which EXPLAIN abbreviates"
        );
    }

    /// A scalar subquery that is *not* an aggregate keeps the per-row path: the
    /// executor raises `21000` when such a subquery returns two rows for one
    /// outer row, and a join would silently return both.
    #[test]
    fn a_non_aggregate_scalar_subquery_is_left_alone() {
        assert_eq!(
            explain_optimized(
                "SELECT a.id FROM t a WHERE a.big < (SELECT b.big FROM t b WHERE b.id = a.id)"
            )[0],
            "Seq Scan on t"
        );
    }

    /// A correlation key with a subquery inside it is not a key this may lift:
    /// the rebase that moves a conjunct into the join condition cannot reach into
    /// a subquery's body, so the body's references would be left counting levels
    /// from a query that is no longer there.
    #[test]
    fn a_key_holding_a_subquery_leaves_the_plan_alone() {
        assert_eq!(
            explain_optimized(
                "SELECT a.id FROM t a WHERE EXISTS ( \
                   SELECT 1 FROM t b \
                   WHERE (SELECT c.id FROM t c WHERE c.big = b.big LIMIT 1) = a.id)"
            )[0],
            "Seq Scan on t"
        );
    }

    /// The rewrite happens below an aggregate as readily as below a scan: the
    /// arm is added to the aggregate's own input, where its `WHERE` and grouping
    /// keys keep addressing the same columns.
    #[test]
    fn a_semi_join_lands_under_an_aggregate() {
        assert_eq!(
            explain_optimized(
                "SELECT count(*) FROM t a WHERE EXISTS (SELECT 1 FROM t b WHERE b.id = a.id)"
            ),
            vec![
                "Aggregate",
                "  ->  Hash Semi Join",
                "        Hash Cond: (id = $3)",
                "        ->  Seq Scan on t",
                "        ->  Hash",
                "              ->  Seq Scan on t",
            ]
        );
    }

    /// Two markers, two arms. The second one's columns land past the first's,
    /// which is what the rule's one-rewrite-at-a-time loop is for.
    #[test]
    fn two_markers_become_two_arms() {
        let lines = explain_optimized(
            "SELECT a.id FROM t a \
             WHERE EXISTS (SELECT 1 FROM t b WHERE b.id = a.id) \
               AND EXISTS (SELECT 1 FROM t c WHERE c.big = a.big)",
        );
        assert_eq!(lines[0], "Hash Semi Join");
        assert!(
            lines.iter().filter(|l| l.contains("Semi Join")).count() == 2,
            "both markers became arms: {lines:?}"
        );
    }

    /// Turning the rule off leaves the plan exactly as the binder built it —
    /// the switch a differential test drives both paths with.
    #[test]
    fn the_rule_can_be_turned_off() {
        let sql = "SELECT a.id FROM t a WHERE EXISTS (SELECT 1 FROM t b WHERE b.id = a.id)";
        let mut logical = bind_sql_indexed(sql, None);
        let mut ctx =
            crabgresql_optimizer::OptimizerContext::new(crabgresql_types::FmtCtx::utc_default());
        ctx.decorrelate = false;
        crabgresql_optimizer::optimize(&mut logical, &ctx);
        assert_eq!(
            explain(&plan(logical, cost::CostSettings::default()))[0],
            "Seq Scan on t"
        );
    }
}
