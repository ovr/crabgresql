//! Column-projection pushdown: tell each scan which columns its query reads.
//!
//! A columnar engine that knows only two of forty columns are wanted reads only
//! those two off disk. This pass computes that set per scan leaf and stamps it
//! on the plan; [`ColumnProjection`] carries it to the storage engine.
//!
//! **Rows stay full width.** The pass never narrows a tuple, only the work spent
//! filling one. That is what makes it a local change: every `ColumnRef` above a
//! scan indexes the row positionally, joins concatenate `left || right` and rely
//! on `left.width()`, and `UPDATE` rebuilds rows by ordinal — all of which keep
//! working untouched because the row shape never moves. Unselected slots hold
//! unspecified values (see [`TableAm::scan`]), which is sound precisely because
//! nothing above the scan reads them.
//!
//! Runs last in [`plan`](crate::plan), after `pushdown::push_where_into_joins`,
//! so a predicate is analyzed at the leaf where it will actually be evaluated.
//!
//! Every path is fail-safe: an expression whose dependencies cannot be proven,
//! or a plan shape this pass does not model, yields [`ColumnProjection::All`] —
//! which is exactly the behavior before this pass existed.

use std::collections::BTreeSet;

use crabgresql_binder::{BoundAggregate, BoundExpr};
use crabgresql_storage_api::ColumnProjection;

use crate::{
    PhysicalAggInput, PhysicalJoinExpr, PhysicalJoinInput, PhysicalPlan, PhysicalSetOpArm,
};

/// The columns a node's consumers read, in that node's own base-0 index space.
/// `None` means "could not be determined — assume all of them".
type Demand = Option<BTreeSet<usize>>;

/// Every column: the fail-safe answer.
const ALL: Demand = None;

/// Stamp a projection on every scan leaf of `plan` that this pass can prove one
/// for.
pub(crate) fn push_column_projections(plan: &mut PhysicalPlan) {
    match plan {
        // The tail-bearing single-table nodes: their own `projections` and
        // `predicate` are the only things that read the scanned row.
        //
        // `sort` and `distinct` are deliberately absent. Their `column` fields
        // index the *projected* tuple, not the base row (see the doc comments on
        // `SortKey` and `DistinctKey` in the binder's `plan.rs`), and a hidden
        // ORDER BY / DISTINCT ON column is already appended to `projections`.
        PhysicalPlan::Select {
            table,
            projection,
            projections,
            predicate,
            ..
        } => {
            *projection = resolve(tail_demand(projections, predicate.as_ref()), table.schema());
        }
        PhysicalPlan::IndexScan {
            table,
            projection,
            key,
            projections,
            predicate,
            ..
        } => {
            let mut demand = tail_demand(projections, predicate.as_ref());
            // The executor's index-scan fallback (`IndexScan::new`) re-checks
            // every key column per row whenever the engine has no physical index
            // to probe — which is every engine but the in-memory one. Pruning a
            // key column would make that re-check read NULL and drop every row.
            if let Some(demand) = &mut demand {
                demand.extend(key.iter().map(|(column, _)| *column));
            }
            // Key *values* are row-constant, but folding them in costs nothing
            // and keeps the set correct if that ever loosens.
            let demand = add_exprs(demand, key.iter().map(|(_, value)| value));
            *projection = resolve(demand, table.schema());
        }
        PhysicalPlan::Subquery {
            source,
            projections,
            predicate,
            ..
        } => {
            // An `Append` has no expressions of its own, so its columns are
            // demanded entirely by this node. Push only into a literal `Append`:
            // that is the partitioned-parent / buffered-Parquet shape, and the
            // one child whose output row is provably the base relation's row
            // (a single FROM item binds at scope offset 0). Any other source
            // computes its own demand from its own tail.
            if let PhysicalPlan::Append {
                projection,
                columns,
                tables,
            } = source.as_mut()
            {
                let demand = tail_demand(projections, predicate.as_ref());
                // Every leaf shares the parent's layout, so one projection
                // serves them all; take the width from the first leaf's schema.
                if let Some(table) = tables.first() {
                    debug_assert_eq!(table.schema().columns.len(), columns.len());
                    *projection = resolve(demand, table.schema());
                }
            } else {
                push_column_projections(source);
            }
        }
        PhysicalPlan::Join {
            source,
            projections,
            predicate,
            ..
        } => {
            // `projections`/`predicate` here index the whole concatenated join
            // row, which is exactly `source`'s own base-0 space.
            prune_join(source, tail_demand(projections, predicate.as_ref()));
        }
        PhysicalPlan::Aggregate {
            input,
            predicate,
            group_exprs,
            aggregates,
            ..
        } => {
            // Only WHERE, the grouping keys and the aggregate arguments read the
            // *source* row. `projections`, `having`, `sort` and `distinct` all
            // index the post-grouping row, and `BoundAggregate::args` is the
            // aggregate's only field over source columns (`count(*)` has none).
            let demand = add_exprs(Some(BTreeSet::new()), predicate.as_ref());
            let demand = add_exprs(demand, group_exprs.iter());
            let demand = aggregates
                .iter()
                .fold(demand, |demand, agg: &BoundAggregate| {
                    add_exprs(demand, agg.args.iter())
                });
            match input {
                PhysicalAggInput::Scan { table, projection } => {
                    *projection = resolve(demand, table.schema());
                }
                PhysicalAggInput::Join(source) => prune_join(source, demand),
                PhysicalAggInput::SingleRow => {}
            }
        }

        // Shapes this pass does not model: recurse so nested scans still get
        // their own analysis, but demand nothing across the boundary.
        PhysicalPlan::Limit { source, .. } => push_column_projections(source),
        PhysicalPlan::SetOp { arms, .. } => {
            for PhysicalSetOpArm { plan, .. } in arms {
                push_column_projections(plan);
            }
        }
        PhysicalPlan::Insert { source, .. } => {
            if let crate::PhysicalInsertSource::Query { input, .. } = source {
                push_column_projections(input);
            }
        }
        // A bare `Append` reached without a wrapping `Subquery`, plus the leaves
        // that hold no scan and the DML nodes, which always read whole rows.
        PhysicalPlan::Append { .. }
        | PhysicalPlan::Values { .. }
        | PhysicalPlan::TableFunction { .. }
        | PhysicalPlan::Update { .. }
        | PhysicalPlan::Delete { .. } => {}
    }
}

/// Push `demand` — expressed in `node`'s own base-0 index space — down to the
/// scan leaves beneath it.
fn prune_join(node: &mut PhysicalJoinExpr, demand: Demand) {
    match node {
        PhysicalJoinExpr::Input {
            input,
            width,
            predicate,
        } => {
            // A leaf's own sunk conjuncts are already rebased to the leaf's row
            // by `pushdown::place`, so they share this space and need no shift.
            let demand = add_exprs(demand, predicate.as_ref());
            let width = *width;
            let demand =
                demand.map(|demand| demand.into_iter().filter(|index| *index < width).collect());
            match input {
                PhysicalJoinInput::Scan { table, projection } => {
                    *projection = resolve(demand, table.schema());
                }
                PhysicalJoinInput::Subplan(source) => {
                    // Same reasoning as the `Subquery` arm above: an `Append`
                    // is transparent, anything else owns its own tail.
                    if let PhysicalPlan::Append {
                        projection, tables, ..
                    } = source.as_mut()
                    {
                        if let Some(table) = tables.first() {
                            *projection = resolve(demand, table.schema());
                        }
                    } else {
                        push_column_projections(source);
                    }
                }
                PhysicalJoinInput::TableFunction { .. } => {}
            }
        }
        PhysicalJoinExpr::Join {
            left,
            right,
            predicate,
            hash_keys,
            ..
        } => {
            // This node's own ON condition and hash keys index its concatenated
            // `left || right` row — the same space `demand` arrives in.
            let demand = add_exprs(demand, predicate.as_ref());
            let demand = hash_keys.iter().fold(demand, |demand, key| {
                add_exprs(add_exprs(demand, Some(&key.left)), Some(&key.right))
            });

            // Split at the boundary: left indices pass through unchanged, right
            // indices rebase into the right subtree's own base-0 space.
            let split = left.width();
            let (left_demand, right_demand) = match demand {
                None => (ALL, ALL),
                Some(demand) => (
                    Some(demand.iter().copied().filter(|i| *i < split).collect()),
                    Some(
                        demand
                            .iter()
                            .filter(|i| **i >= split)
                            .map(|i| i - split)
                            .collect(),
                    ),
                ),
            };
            prune_join(left, left_demand);
            prune_join(right, right_demand);
        }
    }
}

/// The columns a standard projection/predicate tail reads from its source row.
fn tail_demand(projections: &[BoundExpr], predicate: Option<&BoundExpr>) -> Demand {
    let demand = add_exprs(Some(BTreeSet::new()), projections.iter());
    add_exprs(demand, predicate)
}

/// Fold more expressions into `demand`, collapsing to [`ALL`] as soon as any of
/// them — or `demand` itself — cannot be pinned down.
fn add_exprs<'a>(demand: Demand, exprs: impl IntoIterator<Item = &'a BoundExpr>) -> Demand {
    let mut demand = demand?;
    for expr in exprs {
        if !expr.collect_column_refs(&mut demand) {
            return ALL;
        }
    }
    Some(demand)
}

/// Turn a computed demand into the projection to stamp on a leaf.
fn resolve(demand: Demand, schema: &crabgresql_storage_api::TableSchema) -> ColumnProjection {
    match demand {
        None => ColumnProjection::All,
        Some(demand) => ColumnProjection::of(demand, schema),
    }
}
