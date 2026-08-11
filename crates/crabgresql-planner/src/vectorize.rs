//! Which parts of a plan can run on Arrow batches.
//!
//! These rules live here, in the planner, for the reason
//! [`PhysicalJoinExpr::uses_hash_join`](crate::PhysicalJoinExpr::uses_hash_join)
//! does: **the executor and `EXPLAIN` must make the same decision.** `EXPLAIN`
//! annotates a node as columnar, and an annotation the executor disagrees with
//! is worse than none — it reports work that never happened.
//!
//! So this module is the single source of truth for *whether*, and
//! `crabgresql_executor::vector` only decides *how*. The executor gates its
//! compilers on these functions and may decline further (an operand shape it has
//! not implemented yet), but it must never vectorize something rejected here.
//!
//! Everything is a pure function of the plan, so no Arrow dependency is needed
//! at this layer.

use crabgresql_binder::{BinOp, BoundAggregate, BoundExpr, DistinctKey, SortKey, UnaryOp};
use crabgresql_types::{PgType, collation};

use crate::{
    PhysicalAggInput, PhysicalAppendArm, PhysicalJoinExpr, PhysicalJoinInput, PhysicalPlan,
};

/// Which parts of one plan node run columnar. Rendered by
/// [`explain`](crate::explain) and consulted by the executor as it builds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vectorization {
    /// The scan hands up Arrow batches instead of tuples.
    pub scan: bool,
    /// The `WHERE` runs as an Arrow filter, below the row boundary.
    pub filter: bool,
    /// The `ORDER BY` (with its projection) runs as an Arrow sort.
    pub sort: bool,
    /// The `GROUP BY` consumes batches directly, instead of shredding each input
    /// row into a tuple first.
    pub agg: bool,
}

impl Vectorization {
    pub fn any(self) -> bool {
        self.scan || self.filter || self.sort || self.agg
    }

    /// The `EXPLAIN` suffix for a node, or `None` when nothing vectorized.
    ///
    /// A divergence from PostgreSQL's output, taken deliberately: a plan that
    /// runs on a different engine than it appears to is not worth the
    /// compatibility. Only plans that actually vectorize are affected, so every
    /// row-path plan still renders exactly as before.
    pub fn suffix(self) -> Option<String> {
        if !self.any() {
            return None;
        }
        let parts = [
            (self.scan, "scan"),
            (self.filter, "filter"),
            (self.sort, "sort"),
            (self.agg, "aggregate"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect::<Vec<_>>()
        .join(", ");
        Some(format!(" (columnar: {parts})"))
    }
}

/// Whether values of `ty` compare in Arrow exactly as PostgreSQL compares them,
/// under `op` and `collation`.
///
/// Each exclusion is a case where Arrow's answer would be *wrong*, not merely
/// different:
///
/// - `numeric` has no Arrow type (arbitrary precision), so it is stored as text
///   and an Arrow comparison would compare text: `'9' > '10'`.
/// - `float4`/`float8` — Arrow's comparison kernels are not IEEE `==` but
///   bitwise, i.e. IEEE's totalOrder predicate. So `-0.0 = 0.0` is false where
///   PostgreSQL says true, and two NaNs of different bit patterns compare
///   unequal where PostgreSQL calls every NaN one value. (Floats *can* be sort
///   keys — see [`sortable_key`].)
/// - `bpchar` compares with trailing blanks trimmed.
/// - `timetz`/`interval` are structs whose PostgreSQL order is not
///   lexicographic over their fields.
/// - text *ordering* follows the collation, and an ICU collation is not byte
///   order. Equality is allowed under any collation, because every supported
///   collation is deterministic and so equality is bytewise regardless.
pub fn comparable(ty: PgType, op: BinOp, collation: u32) -> bool {
    match ty {
        PgType::Bool
        | PgType::Int2
        | PgType::Int4
        | PgType::Int8
        | PgType::Date
        | PgType::Time
        | PgType::Timestamp
        | PgType::TimestampTz
        | PgType::Bytea
        | PgType::Uuid => true,
        PgType::Text | PgType::Varchar | PgType::Name => {
            matches!(op, BinOp::Eq | BinOp::NotEq) || collation::is_byte_order(collation)
        }
        _ => false,
    }
}

/// Whether Arrow's total order over a sort key is PostgreSQL's order.
///
/// The float types qualify although [`comparable`] excludes them: the sort
/// canonicalizes `-0.0` to `0.0` and every NaN to one NaN before ordering,
/// which makes the two orders coincide. The same rewrite would repair equality
/// too, but a sort owns its key column and can rewrite it once, whereas a
/// filter would have to rewrite both operands of every comparison it evaluates.
///
/// The set itself lives with the sort that relies on it
/// ([`crabgresql_storage_api::sort::sortable`]), because the columnar engines'
/// write path asks the same question of a stored column and the two answers
/// must be one answer.
pub fn sortable_key(key: &SortKey) -> bool {
    crabgresql_storage_api::sort::sortable(key.ty, key.collation)
}

/// Whether `predicate` can run as an Arrow filter over a batch `width` columns
/// wide.
///
/// Operands are columns and constants only. Anything computed — arithmetic, a
/// function call, a cast, a bind parameter, a correlated reference — stays on
/// the row path, which is where PostgreSQL's evaluation semantics and side
/// effects live.
pub fn vectorizable_predicate(predicate: &BoundExpr, width: usize) -> bool {
    match predicate {
        // Value-transparent; a comparison reads its collation from its own node.
        BoundExpr::Collate { expr, .. } => vectorizable_predicate(expr, width),
        BoundExpr::Binary {
            op: BinOp::And | BinOp::Or,
            left,
            right,
            ..
        } => vectorizable_predicate(left, width) && vectorizable_predicate(right, width),
        BoundExpr::Unary {
            op: UnaryOp::Not,
            expr,
        } => vectorizable_predicate(expr, width),
        BoundExpr::IsNull { expr, .. } => vectorizable_operand(expr, width),
        BoundExpr::Binary {
            op,
            arg_ty,
            collation,
            left,
            right,
        } if is_comparison(*op) => {
            comparable(*arg_ty, *op, *collation)
                && vectorizable_operand(left, width)
                && vectorizable_operand(right, width)
        }
        // A bare boolean column or constant is a legal WHERE on its own.
        BoundExpr::ColumnRef {
            ty: PgType::Bool, ..
        }
        | BoundExpr::Const {
            ty: PgType::Bool, ..
        } => vectorizable_operand(predicate, width),
        _ => false,
    }
}

fn is_comparison(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq
    )
}

/// Whether a `Coerce` from `from` to `to` is a *widening* integer cast.
///
/// The distinction is the whole reason a cast can be admitted at all. A widening
/// cast is **total** over its source type — every `int2` is an `int4` — so it has
/// no failure mode to reproduce and no error message to match. A narrowing one
/// raises `22003 integer out of range` on a value the source type holds
/// perfectly well, and reproducing *which* row raises first is exactly the kind
/// of semantics `vectorize` refuses to re-derive.
///
/// Admitting this is load-bearing rather than a nicety. `smallint_col <> 0`
/// resolves in `int4`, so the binder wraps the **column** in a `Coerce` and
/// leaves the constant alone; on an analytic relation where much of the schema is
/// `smallint` — every flag column in ClickBench's `hits`, and the flag columns are
/// what the selective predicates test — declining this declines the filter on
/// most real `WHERE`s, and with it everything the filter was going to save.
pub fn widening_int_cast(from: PgType, to: PgType) -> bool {
    matches!(
        (from, to),
        (PgType::Int2, PgType::Int4 | PgType::Int8) | (PgType::Int4, PgType::Int8)
    )
}

fn vectorizable_operand(expr: &BoundExpr, width: usize) -> bool {
    match expr {
        BoundExpr::Collate { expr, .. } => vectorizable_operand(expr, width),
        // See [`widening_int_cast`]. Nesting is fine: each step is checked, so
        // `int2 -> int4 -> int8` widens twice and `int8 -> int4` never starts.
        BoundExpr::Coerce { expr, ty } => {
            widening_int_cast(expr.ty(), *ty) && vectorizable_operand(expr, width)
        }
        // Batches are full width in schema order, so a schema ordinal is a batch
        // ordinal — but only if it is actually in range.
        BoundExpr::ColumnRef { index, .. } => *index < width,
        // A constant is only usable if Arrow can hold its type. Roughly half of
        // `PgType` cannot be encoded (json, inet, arrays, …), and such a
        // constant is perfectly legal in a target list or a `WHERE` even on a
        // relation that could never store one — `SELECT id, '{}'::json FROM p
        // ORDER BY id`. Without this check the planner advertises a columnar
        // plan the executor cannot build.
        BoundExpr::Const { ty, .. } => crabgresql_storage_api::arrow::supports_type(*ty),
        _ => false,
    }
}

/// Whether one grouping key or aggregate argument can be read straight out of a
/// batch.
///
/// The same subset as a comparison operand, and for the same reason: there is no
/// general vectorized expression evaluator, so anything computed —
/// `GROUP BY a + b`, `sum(x + 1)`, `count(f(x))` — stays on the row path.
///
/// Note what is *not* required here, and is the point worth understanding about
/// this gate. A grouping key needs no [`comparable`] check. Group identity is
/// decided by `agg::hash_key` and `agg::keys_equal` over decoded
/// [`Value`](crabgresql_types::Value)s — the row engine's own equality — so
/// `numeric`, `float8` and `bpchar` keys are all admissible here even though a
/// columnar *filter* must reject them. The columnar part of an aggregate is
/// getting the values out of the batch, not comparing them.
pub fn vectorizable_agg_cell(expr: &BoundExpr, width: usize) -> bool {
    match strip_collate(expr) {
        // A widening cast is admissible in a filter because Arrow does the
        // comparing; here the value is decoded and handed to the row engine, so a
        // cast would have to be *applied*, which is the computation this subset
        // excludes. Hence `ColumnRef`/`Const` only, not `vectorizable_operand`.
        BoundExpr::ColumnRef { index, .. } => *index < width,
        BoundExpr::Const { ty, .. } => crabgresql_storage_api::arrow::supports_type(*ty),
        _ => false,
    }
}

fn strip_collate(expr: &BoundExpr) -> &BoundExpr {
    match expr {
        BoundExpr::Collate { expr, .. } => strip_collate(expr),
        other => other,
    }
}

/// Whether every grouping key and aggregate argument reads straight out of a
/// batch, so the aggregate can consume batches instead of shredded tuples.
///
/// The aggregate functions themselves are unrestricted — `DISTINCT`, `string_agg`
/// and all — because the accumulators are the row engine's own. Only the *inputs*
/// are gated.
pub fn vectorizable_aggregate(
    group_exprs: &[BoundExpr],
    aggregates: &[BoundAggregate],
    width: usize,
) -> bool {
    let cell = |expr| vectorizable_agg_cell(expr, width);
    group_exprs.iter().all(cell)
        && aggregates
            .iter()
            .all(|aggregate| aggregate.args.iter().all(cell))
}

/// Whether a projection is a pure column take — a reorder or a constant, never
/// a computation. Only such a projection keeps the columnar segment alive as far
/// as a sort, because a [`SortKey`] indexes the *projected* tuple.
pub fn vectorizable_projection(projections: &[BoundExpr], width: usize) -> bool {
    projections
        .iter()
        .all(|expr| vectorizable_operand(expr, width))
}

/// The tail a scan-bearing plan node carries, in the order the executor applies
/// it. Grouped so [`tail_vectorization`] states the rules once for every node
/// shape that has one.
struct Tail<'a> {
    predicate: Option<&'a BoundExpr>,
    projections: &'a [BoundExpr],
    sort: &'a [SortKey],
    distinct: Option<&'a Vec<DistinctKey>>,
    /// The client-visible output width; the sort drops anything past it.
    visible: usize,
}

/// The decision for one node, given whether its scan can produce batches and how
/// wide its input rows are.
///
/// The conditions mirror the executor's pipeline exactly, and in the same order,
/// because they describe the same pipeline:
///
/// - the filter needs a batch source;
/// - the sort needs the filter to have *consumed* the predicate, since a
///   predicate left for the row `Filter` must run before the projection;
/// - the sort needs no `DISTINCT`, which is a row node that wants the hidden
///   ORDER BY columns the sort would drop;
/// - the sort needs its projection to be a pure take, since a [`SortKey`]
///   indexes the projected tuple.
fn tail_vectorization(scan: bool, width: usize, tail: Tail<'_>) -> Vectorization {
    let filter = scan
        && tail
            .predicate
            .is_some_and(|predicate| vectorizable_predicate(predicate, width));
    let predicate_gone = tail.predicate.is_none() || filter;
    let sort = scan
        && predicate_gone
        && tail.distinct.is_none()
        && !tail.sort.is_empty()
        && tail.visible <= tail.projections.len()
        && vectorizable_projection(tail.projections, width)
        && tail
            .sort
            .iter()
            .all(|key| key.column < tail.projections.len() && sortable_key(key));
    // A scan-bearing node has no grouping of its own; `PhysicalPlan::Aggregate`
    // is the only shape that sets `agg`.
    Vectorization {
        scan,
        filter,
        sort,
        agg: false,
    }
}

/// Whether every arm of an `Append` can hand up batches. All or none: their
/// outputs are concatenated, so they must share one representation.
///
/// A remapped arm disqualifies the whole node — including the arms that could
/// have handed up batches, since the outputs are concatenated and must share one
/// representation. Batches carry the arm's own column order and the batch path
/// has nowhere to apply a permutation, so it would concatenate mis-ordered
/// columns rather than fail.
///
/// Nothing pays for the *remap* rule today because DDL refuses an engine-managed
/// relation on *either* side of an inheritance link, so no hierarchy can contain a
/// batch-capable relation at all. That is what makes the all-or-none rule free
/// rather than a silent de-optimization of the parent.
///
/// An arm that must emit a `tableoid` is a different matter and is reachable:
/// `SELECT tableoid, id FROM p` on a Parquet relation stamps every arm with its
/// own relation identity and widens the arm's column list by one. That extra
/// column is not in any batch — it is appended by the row `Append` — so
/// `crabgresql_executor::vector::BatchAppend::open` declines it, and this must
/// decline with it or `EXPLAIN` claims a columnar scan that never runs.
/// The width of the batches an aggregate's input can hand up, or `None` when it
/// cannot hand up batches at all.
///
/// Consulted by both [`PhysicalPlan::vectorization`] and the executor's
/// `agg_source`, so the annotation and the pipeline cannot disagree about which
/// inputs qualify.
///
/// The `Join` arm is the load-bearing one, not a generalisation. A relation with
/// storage leaves — which is *every* `USING parquet` relation, since its rows live
/// in a durable chunk store and a RAM write buffer — binds as
/// `JoinInput::Subplan(Append)`, so `build_query_block`'s `JoinInput::Scan`
/// pattern misses and the aggregate's input is a one-input `Join`.
/// `PhysicalAggInput::Scan` is reached only by a monolithic relation: a heap table
/// (no batch path) or a standalone `USING buffer` table.
pub fn agg_input_width(input: &PhysicalAggInput) -> Option<usize> {
    let of_table = |table: &std::sync::Arc<dyn crabgresql_storage_api::TableAm>| {
        table
            .supports_batch_scan()
            .then(|| table.schema().columns.len())
    };
    match input {
        PhysicalAggInput::Scan { table, .. } => of_table(table),
        PhysicalAggInput::Join(PhysicalJoinExpr::Input {
            input, predicate, ..
        }) => {
            // A single-input `Input` carries no predicate of its own: only the
            // `Join` arm runs `sink_leaf_filters`. Checked rather than assumed,
            // because a sunk filter this classification skipped would be a
            // dropped `WHERE` — a wrong answer, not a slow one.
            if predicate.is_some() {
                debug_assert!(false, "a single-input join expression carries no predicate");
                return None;
            }
            match input {
                PhysicalJoinInput::Scan { table, .. } => of_table(table),
                PhysicalJoinInput::Subplan(source) => match source.as_ref() {
                    // The width is the *named* relation's, which an arm's own
                    // width equals only when it does not remap — and `arms_batch`
                    // refuses a remapped arm.
                    PhysicalPlan::Append { arms, columns } => {
                        arms_batch(arms).then_some(columns.len())
                    }
                    _ => None,
                },
                PhysicalJoinInput::TableFunction { .. } => None,
            }
        }
        PhysicalAggInput::Join(_) | PhysicalAggInput::SingleRow => None,
    }
}

fn arms_batch(arms: &[PhysicalAppendArm]) -> bool {
    !arms.is_empty()
        && arms.iter().all(|arm| {
            arm.relation.map.is_none()
                && arm.relation.tableoid.is_none()
                && arm.relation.table.supports_batch_scan()
        })
}

impl PhysicalPlan {
    /// What this node runs on Arrow batches.
    ///
    /// Consulted by [`explain`](crate::explain) so the plan it prints is the
    /// plan that runs.
    pub fn vectorization(&self) -> Vectorization {
        match self {
            PhysicalPlan::Select {
                table,
                columns,
                projections,
                predicate,
                sort,
                distinct,
                ..
            } => tail_vectorization(
                table.supports_batch_scan(),
                table.schema().columns.len(),
                Tail {
                    predicate: predicate.as_ref(),
                    projections,
                    sort,
                    distinct: distinct.as_ref(),
                    visible: columns.len(),
                },
            ),
            // An engine-managed relation reads as an `Append` over its storage
            // leaves wrapped in a `Subquery`, and the tail lives on the wrapper.
            // Recognising that shape here is what lets such a relation report a
            // columnar filter at all.
            PhysicalPlan::Subquery {
                source,
                columns,
                projections,
                predicate,
                sort,
                distinct,
            } => {
                let PhysicalPlan::Append {
                    arms,
                    columns: append_columns,
                } = source.as_ref()
                else {
                    return Vectorization::default();
                };
                tail_vectorization(
                    arms_batch(arms),
                    // The Append's output width is the *named* relation's, which
                    // an arm's own width equals only when it does not remap.
                    append_columns.len(),
                    Tail {
                        predicate: predicate.as_ref(),
                        projections,
                        sort,
                        distinct: distinct.as_ref(),
                        visible: columns.len(),
                    },
                )
            }
            PhysicalPlan::Append { arms, .. } => Vectorization {
                scan: arms_batch(arms),
                ..Vectorization::default()
            },
            // A grouped aggregate consumes batches directly instead of shredding
            // each input row into a tuple — see
            // `crabgresql_executor::vector::agg`.
            //
            // The whole suffix lands on *this* line, `scan` included, because under
            // an aggregate nothing below it renders one. An `Append` gets its
            // annotation from the `Subquery` that wraps it in a plain `SELECT`
            // (see that arm), and an aggregate's input is a bare join input with no
            // such wrapper — `PhysicalPlan::Append`'s own arm attaches nothing. So
            // reporting `scan` here is the only way the columnar scan is visible at
            // all; it would only double up if that arm ever gained a suffix of its
            // own.
            //
            // `sort` stays `false` for a different reason: this node's
            // `projections`, `sort` and `distinct` index the *post-grouping* row,
            // not the input, so `tail_vectorization`'s rules do not apply to them.
            PhysicalPlan::Aggregate {
                input,
                predicate,
                group_exprs,
                aggregates,
                ..
            } => {
                let Some(width) = agg_input_width(input) else {
                    return Vectorization::default();
                };
                let filter = predicate
                    .as_ref()
                    .is_some_and(|predicate| vectorizable_predicate(predicate, width));
                // A predicate the columnar filter declined is still waiting for the
                // row `Filter`, which has to run *before* grouping — and a row
                // filter cannot be interposed in a batch stream. So the grouping
                // goes back to rows, exactly as a leftover predicate blocks the
                // columnar sort. The scan below it stays columnar either way.
                let predicate_gone = predicate.is_none() || filter;
                let agg = predicate_gone && vectorizable_aggregate(group_exprs, aggregates, width);
                Vectorization {
                    scan: true,
                    filter,
                    sort: false,
                    agg,
                }
            }
            _ => Vectorization::default(),
        }
    }
}
