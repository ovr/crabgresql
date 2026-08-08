//! Grouped aggregation over a batch stream.
//!
//! # What is and is not vectorized
//!
//! The scan, the decode and the `WHERE` are columnar. **Accumulation and grouping
//! are not**: they run one value at a time through the row engine's own
//! [`Accumulator`], [`agg::feed`] and [`GroupIndex`]. That is the deliberate
//! centre of the design, not a stage left unfinished.
//!
//! Aggregation is where PostgreSQL's arithmetic lives. `sum(int8)` promotes to
//! `numeric` on overflow; `avg` of an exact type divides at a scale
//! [`agg::avg_quotient`] derives from both operands' `dscale`; `min`/`max` over
//! text compare under a collation, and over `oidvector` in an order that is not
//! btree order; float summation is order-dependent, so the *sequence* of
//! additions is observable; `bpchar` grouping trims trailing blanks; `-0.0` and
//! every NaN bit pattern are one group but the group reports its **first-seen**
//! representative. An Arrow kernel reproduces none of that. Routing the values
//! through the row engine's accumulators makes every one of those properties
//! inherited by construction, rather than a second implementation that has to be
//! kept in step with the first.
//!
//! So what this node actually deletes is [`super::Shred`]: the full-width
//! `Tuple` built per input row, of which a grouped aggregate reads one or two
//! slots. On a hundred-column relation read for two columns that is the dominant
//! per-row cost, and pulling the `WHERE` below it means the rows a selective
//! predicate rejects are never decoded at all.
//!
//! # Why this is an `ExecNode`
//!
//! Its output is one row per group, which is small, so `HAVING`, the projection
//! list, `ORDER BY` and `LIMIT` keep running on the row nodes that already
//! implement them. A batch-producing form would have to *build* Arrow arrays out
//! of the `Value`s [`Accumulator::finalize`] returns — pure added cost — and
//! would need a layout for `[keys…, aggregates…]` whose column types come from
//! accumulators rather than from any table.
//!
//! It is therefore the **second** way a columnar segment can end, alongside
//! `Shred`. A plan has exactly one terminator, never both.

use arrow_array::{Array, RecordBatch};
use crabgresql_binder::{BoundAggregate, BoundExpr};
use crabgresql_planner::vectorize;
use crabgresql_storage_api::arrow::decode_value;
use crabgresql_types::{PgType, Value};

use super::{BatchLayout, BatchNode};
use crate::keyindex::{GroupIndex, Slot};
use crate::{ExecError, ExecNode, Tuple, agg};

/// Where one grouping key or aggregate argument reads its value.
///
/// The two cases `vectorize::vectorizable_agg_cell` admits. There is no general
/// vectorized expression evaluator — `super::expr` compiles predicates only — so
/// `GROUP BY a + b` and `sum(x + 1)` stay on the row path entirely.
pub(super) enum Cell {
    /// A batch column, by schema ordinal.
    Column(usize),
    /// A constant, cloned per row. Nothing is built for it: `BoundExpr::Const`
    /// already holds the `Value` the row path would produce.
    Const(Value),
}

impl Cell {
    fn value(
        &self,
        batch: &RecordBatch,
        layout: &BatchLayout,
        row: usize,
    ) -> Result<Value, ExecError> {
        match self {
            Cell::Column(index) => {
                let column = layout
                    .get(*index)
                    .ok_or_else(|| internal("aggregate cell names a column outside the layout"))?;
                let array = batch
                    .columns()
                    .get(*index)
                    .ok_or_else(|| internal("aggregate cell names a column outside the batch"))?;
                decode_value(column, array.as_ref(), row).map_err(ExecError::from)
            }
            Cell::Const(value) => Ok(value.clone()),
        }
    }
}

fn internal(message: &str) -> ExecError {
    ExecError::new("XX000", message)
}

/// Compile `exprs` into cells, or `None` if any of them falls outside the subset.
///
/// Gated on the planner first, as every compiler in `super` is, so this can only
/// ever accept a subset of what `EXPLAIN` advertised.
///
/// `positions` names the batch columns a scan actually filled. A column outside
/// it is `null_array` padding, which `decode_value` reads back as `Value::Null`
/// **silently** — so a key compiled against one would collapse every row into a
/// single NULL group rather than fail. The planner's projection pass makes the
/// aggregate's projection exactly `predicate ∪ group_exprs ∪ ⋃args`, so this
/// cannot happen today; it is checked because the failure is a wrong answer, not
/// an error, and nothing else would notice.
pub(super) fn cells(
    exprs: &[BoundExpr],
    layout: &BatchLayout,
    positions: &[usize],
) -> Option<Vec<Cell>> {
    exprs
        .iter()
        .map(|expr| {
            if !vectorize::vectorizable_agg_cell(expr, layout.len()) {
                return None;
            }
            match strip(expr) {
                BoundExpr::ColumnRef { index, .. } => {
                    positions.contains(index).then_some(Cell::Column(*index))
                }
                BoundExpr::Const { value, .. } => Some(Cell::Const(value.clone())),
                // Unreachable: the gate admits nothing else.
                _ => None,
            }
        })
        .collect()
}

/// See through the value-transparent wrappers the gate also sees through.
fn strip(expr: &BoundExpr) -> &BoundExpr {
    match expr {
        BoundExpr::Collate { expr, .. } => strip(expr),
        other => other,
    }
}

impl AggPlan {
    fn compile(
        group_exprs: &[BoundExpr],
        aggregates: &[BoundAggregate],
        layout: &BatchLayout,
        positions: &[usize],
    ) -> Option<AggPlan> {
        let keys = cells(group_exprs, layout, positions)?;
        let args = aggregates
            .iter()
            .map(|aggregate| cells(&aggregate.args, layout, positions))
            .collect::<Option<Vec<_>>>()?;
        Some(AggPlan {
            counts_only: keys.is_empty() && aggregates.iter().all(is_plain_count),
            keys,
            key_tys: group_exprs.iter().map(BoundExpr::ty).collect(),
            aggregates: aggregates.to_vec(),
            args,
        })
    }

    /// A fresh group, with one accumulator per aggregate. Both the seeded single
    /// group and every keyed group come through here, so the two cannot disagree
    /// about what a new group holds.
    fn new_group(&self, key: Vec<Value>, any_distinct: bool) -> Group {
        Group {
            key,
            accumulators: self.aggregates.iter().map(agg::Accumulator::new).collect(),
            distinct_values: if any_distinct {
                self.aggregates
                    .iter()
                    .map(|aggregate| {
                        aggregate
                            .distinct
                            .then(|| agg::DistinctValues::new(aggregate.input_ty))
                    })
                    .collect()
            } else {
                Vec::new()
            },
        }
    }

    /// Fold a whole batch into the single group without decoding a value.
    ///
    /// Exact rather than approximate. [`agg::feed`] on an argument-less aggregate
    /// does nothing but count the row, and on a one-argument one it skips a NULL
    /// first argument and otherwise counts — so `count(*)` is the batch height and
    /// `count(col)` is the height less that column's nulls, both of which Arrow
    /// already knows. This is where a `count`-only query stops touching values at
    /// all, which is the largest single win in the node.
    ///
    /// It reads the batch it is *given*, and that is the filtered one — the
    /// columnar filter sits below this node — so no predicate is skipped.
    fn fold_counts(&self, batch: &RecordBatch, group: &mut Group) -> Result<(), ExecError> {
        let rows = batch.num_rows();
        for (i, acc) in group.accumulators.iter_mut().enumerate() {
            let counted = match self.args[i].first() {
                None => rows,
                Some(Cell::Column(index)) => {
                    let array = batch
                        .columns()
                        .get(*index)
                        .ok_or_else(|| internal("count names a column outside the batch"))?;
                    rows - array.null_count()
                }
                // `count(NULL)` counts nothing; `count(1)` counts every row.
                Some(Cell::Const(Value::Null)) => 0,
                Some(Cell::Const(_)) => rows,
            };
            acc.count_rows(counted as i64);
        }
        Ok(())
    }
}

/// One accumulating group. The row node's `AggGroup`, field for field, because
/// the two must hold the same thing for the same reasons.
struct Group {
    key: Vec<Value>,
    accumulators: Vec<agg::Accumulator>,
    /// Empty unless *some* aggregate is DISTINCT, so a million-group query does
    /// not allocate a vector of `None` per group. Readers index rather than zip
    /// it — a third `zip` over the empty form would feed no aggregate at all.
    distinct_values: Vec<Option<agg::DistinctValues>>,
}

/// Everything the node needs that does not depend on the child, so eligibility is
/// decided before the child is consumed.
pub struct AggPlan {
    keys: Vec<Cell>,
    key_tys: Vec<PgType>,
    aggregates: Vec<BoundAggregate>,
    args: Vec<Vec<Cell>>,
    /// Every aggregate is a plain `count` over the whole input, so a batch folds
    /// without decoding anything. See [`AggregateBatch::fold_counts`].
    counts_only: bool,
}

pub struct AggregateBatch {
    child: Box<dyn BatchNode>,
    layout: BatchLayout,
    plan: AggPlan,
    output: Option<std::vec::IntoIter<Tuple>>,
}

impl AggregateBatch {
    /// Build the node, or hand `child` back if any key or argument declines.
    ///
    /// Deciding before constructing follows [`super::sort::ProjectBatch::compile`]
    /// and is not stylistic: this takes ownership of `child`, so a decline
    /// discovered afterwards would leave the caller's row fallback with no source.
    pub fn compile(
        child: Box<dyn BatchNode>,
        layout: BatchLayout,
        group_exprs: &[BoundExpr],
        aggregates: &[BoundAggregate],
        positions: &[usize],
    ) -> Result<AggregateBatch, Box<dyn BatchNode>> {
        let Some(plan) = AggPlan::compile(group_exprs, aggregates, &layout, positions) else {
            return Err(child);
        };
        Ok(AggregateBatch {
            child,
            layout,
            plan,
            output: None,
        })
    }

    /// Drain the child, accumulate per group, and materialize the output rows.
    fn build(&mut self) -> Result<std::vec::IntoIter<Tuple>, ExecError> {
        let plan = &self.plan;
        let any_distinct = plan.aggregates.iter().any(|aggregate| aggregate.distinct);
        let mut groups: Vec<Group> = Vec::new();
        let mut lookup = GroupIndex::new(&plan.key_tys);
        // An unkeyed aggregate is one group even over no input, so `SELECT
        // count(*) FROM empty` returns `0` rather than no row at all. Seeded
        // before the first pull, not on the first row, or an empty stream would
        // produce nothing.
        if plan.keys.is_empty() {
            groups.push(plan.new_group(Vec::new(), any_distinct));
        }

        // Reused across rows: the row path's `Vec<Value>` key and its stack
        // buffer sized to the largest aggregate arity.
        let mut key = vec![Value::Null; plan.keys.len()];
        while let Some(batch) = self.child.next_batch()? {
            // An empty batch means "nothing here", not "nothing left" — a filter
            // that rejects a whole batch produces exactly that — so the loop
            // continues rather than ending. Both paths below no-op on it.
            if plan.counts_only {
                plan.fold_counts(&batch, &mut groups[0])?;
                continue;
            }
            for row in 0..batch.num_rows() {
                let index = if plan.keys.is_empty() {
                    0
                } else {
                    for (cell, slot) in plan.keys.iter().zip(key.iter_mut()) {
                        *slot = cell.value(&batch, &self.layout, row)?;
                    }
                    let next = groups.len();
                    match lookup.find_or_insert(&key, next, |i| {
                        agg::keys_equal(&plan.key_tys, &groups[i].key, &key)
                    }) {
                        Slot::Existing(i) => i,
                        Slot::Vacant => {
                            // The group keeps its *first-seen* key, which is
                            // observable: `GROUP BY x` over `(-0.0, 0.0)` reports
                            // `-0`, not the canonical form the hash used.
                            groups.push(plan.new_group(key.clone(), any_distinct));
                            next
                        }
                    }
                };
                let Group {
                    accumulators,
                    distinct_values,
                    ..
                } = &mut groups[index];
                for (i, (aggregate, acc)) in plan
                    .aggregates
                    .iter()
                    .zip(accumulators.iter_mut())
                    .enumerate()
                {
                    let seen = distinct_values.get_mut(i).and_then(Option::as_mut);
                    debug_assert!(
                        aggregate.args.len() <= 2,
                        "widen the argument buffer for a >2-arg aggregate"
                    );
                    let mut buf = [Value::Null, Value::Null];
                    for (slot, cell) in buf.iter_mut().zip(plan.args[i].iter()) {
                        *slot = cell.value(&batch, &self.layout, row)?;
                    }
                    agg::feed(acc, aggregate, &buf[..aggregate.args.len()], seen)?;
                }
            }
        }

        let mut out = Vec::with_capacity(groups.len());
        for group in groups {
            let mut tuple = group.key;
            for acc in &group.accumulators {
                tuple.push(acc.finalize()?);
            }
            out.push(tuple);
        }
        Ok(out.into_iter())
    }
}

/// Whether `aggregate` is a `count` this node can fold a batch at a time.
///
/// DISTINCT is excluded outright: `count(DISTINCT col)` is not the batch height
/// less its nulls, it is the size of a set. A second argument is excluded too —
/// no `count` has one today, and `feed`'s rule is stated over the *first*
/// argument only.
fn is_plain_count(aggregate: &BoundAggregate) -> bool {
    matches!(aggregate.func, crabgresql_binder::AggFn::Count)
        && !aggregate.distinct
        && aggregate.args.len() <= 1
}

impl ExecNode for AggregateBatch {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        if self.output.is_none() {
            self.output = Some(self.build()?);
        }
        Ok(self.output.as_mut().and_then(Iterator::next))
    }
}
