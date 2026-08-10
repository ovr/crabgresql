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
//! # Demand flows down
//!
//! The pass threads a *demand set* — the columns a node's consumers read, in
//! that node's own output-row index space — from the top of the plan to each
//! scan leaf. A node that projects maps its parent's demand through its own
//! `projections` ([`through_tail`]), so only the source columns feeding a
//! demanded output column survive.
//!
//! Threading is what makes the pass see through the shapes a relation is
//! normally read through. `SELECT k FROM v` over a view plans as
//! `Subquery { source: Subquery { source: Append } }` and
//! `SELECT count(k) FROM v` as `Aggregate { Join { Subplan(Subquery { Append }) } }`;
//! an earlier version of this pass matched a literal `Append` child and so
//! collapsed both to "read everything", turning the optimization off for every
//! view and derived table.
//!
//! Every path is fail-safe: an expression whose dependencies cannot be proven,
//! or a plan shape this pass does not model, yields [`ColumnProjection::All`] —
//! which is exactly the behavior before this pass existed.

use std::collections::BTreeSet;

use crabgresql_binder::{BoundAggregate, BoundExpr, BoundWindowFunc, DistinctKey, SortKey};
use crabgresql_storage_api::{ColumnProjection, TableSchema};

use crate::{
    PhysicalAggInput, PhysicalAppendArm, PhysicalJoinExpr, PhysicalJoinInput, PhysicalPlan,
    PhysicalSetOpArm,
};

/// The columns a node's consumers read, in that node's own base-0 index space.
/// `None` means "could not be determined — assume all of them".
type Demand = Option<BTreeSet<usize>>;

/// Every column: the fail-safe answer.
const ALL: Demand = None;

/// Stamp a projection on every scan leaf of `plan` that this pass can prove one
/// for. The root's own output is fully consumed by the client, so it starts at
/// [`ALL`].
pub(crate) fn push_column_projections(plan: &mut PhysicalPlan) {
    push(plan, ALL);
}

/// Push `demand` — the columns of **this node's output row** that its consumer
/// reads — down to the scan leaves beneath it.
fn push(plan: &mut PhysicalPlan, demand: Demand) {
    match plan {
        // The tail-bearing single-table nodes: their own `projections` and
        // `predicate` are the only things that read the scanned row.
        PhysicalPlan::Select {
            table,
            projection,
            projections,
            predicate,
            sort,
            distinct,
            ..
        } => {
            let demand = through_tail(demand, projections, predicate.as_ref(), sort, distinct);
            *projection = resolve(demand, &table.schema());
        }
        PhysicalPlan::IndexScan {
            table,
            projection,
            key,
            projections,
            predicate,
            sort,
            distinct,
            ..
        } => {
            let mut demand = through_tail(demand, projections, predicate.as_ref(), sort, distinct);
            // The executor's index-scan fallback (`index_probe_rows`) re-checks
            // every key column per row whenever the engine declines the probe —
            // which it may do even though `pick_index` only plans an `IndexScan`
            // over an index the engine advertised, since a concurrent
            // `DROP INDEX` can remove it mid-statement. Pruning a key column
            // would make that re-check read NULL and drop every row.
            if let Some(demand) = &mut demand {
                demand.extend(key.iter().map(|(column, _)| *column));
            }
            // Key *values* are row-constant, but folding them in costs nothing
            // and keeps the set correct if that ever loosens.
            let demand = add_exprs(demand, key.iter().map(|(_, value)| value));
            *projection = resolve(demand, &table.schema());
        }
        PhysicalPlan::Subquery {
            source,
            projections,
            predicate,
            sort,
            distinct,
            ..
        } => {
            let demand = through_tail(demand, projections, predicate.as_ref(), sort, distinct);
            push(source, demand);
        }
        // Transparent: output row is the source row, no expressions of its own.
        PhysicalPlan::Limit { source, .. } => push(source, demand),
        PhysicalPlan::Window {
            source,
            spec,
            funcs,
            input_width,
            ..
        } => {
            // This node's output row is `[input row…, window slots…]`, so the
            // parent's demand spans two index spaces. A demanded *slot* is
            // computed here, not read from below, and must be dropped rather
            // than forwarded: a source whose row is only `input_width` wide
            // would see an index past its own width and trip `through_tail`'s
            // fail-safe, turning pruning off for every window query.
            let demand = demand.map(|wanted| {
                wanted
                    .into_iter()
                    .filter(|index| *index < *input_width)
                    .collect()
            });
            // The spec's own reads are added unconditionally, exactly as the
            // `Aggregate` arm adds its group keys: a partition key that is never
            // projected still decides the partitions.
            let demand = add_exprs(demand, spec.exprs());
            let demand = funcs.iter().fold(demand, |demand, func: &BoundWindowFunc| {
                add_exprs(demand, func.kind.args().iter())
            });
            push(source, demand);
        }
        // Also transparent, and the node that actually reaches storage. The
        // demand arrives in the *named* relation's index space, which an
        // identity arm shares and a remapped one does not.
        PhysicalPlan::Append { arms, columns } => {
            prune_append(arms, demand, columns.len());
        }
        PhysicalPlan::Join {
            source,
            projections,
            predicate,
            sort,
            distinct,
            ..
        } => {
            // `projections`/`predicate` index the whole concatenated join row,
            // which is exactly `source`'s own base-0 space.
            let demand = through_tail(demand, projections, predicate.as_ref(), sort, distinct);
            prune_join(source, demand);
        }
        PhysicalPlan::Aggregate {
            input,
            predicate,
            group_exprs,
            aggregates,
            ..
        } => {
            // The parent's demand is deliberately ignored. Only WHERE, the
            // grouping keys and the aggregate arguments read the *source* row —
            // `projections`, `having`, `sort` and `distinct` all index the
            // post-grouping row — and none of the three may be dropped just
            // because its output column is unread: a group key determines the
            // row count, and the executor accumulates every aggregate.
            let demand = add_exprs(Some(BTreeSet::new()), predicate.as_ref());
            let demand = add_exprs(demand, group_exprs.iter());
            let demand = aggregates
                .iter()
                .fold(demand, |demand, agg: &BoundAggregate| {
                    add_exprs(demand, agg.args.iter())
                });
            match input {
                PhysicalAggInput::Scan { table, projection } => {
                    *projection = resolve(demand, &table.schema());
                }
                PhysicalAggInput::Join(source) => prune_join(source, demand),
                PhysicalAggInput::SingleRow => {}
            }
        }

        // Arms line up positionally with the set operation's output, but demand
        // is not threaded through them: a non-ALL `UNION` deduplicates on every
        // output column anyway, and an arm's `coercion` may hold a NULL constant
        // that references no source column at all.
        PhysicalPlan::SetOp { arms, .. } => {
            for PhysicalSetOpArm { plan, .. } in arms {
                push(plan, ALL);
            }
        }
        PhysicalPlan::Insert { source, .. } => {
            if let crate::PhysicalInsertSource::Query { input, .. } = source {
                push(input, ALL);
            }
        }
        // Leaves that hold no scan, plus the DML nodes — those rebuild rows by
        // ordinal and their RETURNING may name any column, so the executor
        // passes `ColumnProjection::All` on every DML scan explicitly.
        PhysicalPlan::Values { .. }
        | PhysicalPlan::TableFunction { .. }
        | PhysicalPlan::Update { .. }
        | PhysicalPlan::Delete { .. } => {}
    }
}

/// Map a parent's demand on a node's **output** row down to a demand on the row
/// its source produces.
///
/// Only the projections the parent actually reads contribute their column
/// references; the predicate always does, because `project_pipeline` evaluates
/// it as a `Filter` *below* the `Projection`, against the source row.
fn through_tail(
    demand: Demand,
    projections: &[BoundExpr],
    predicate: Option<&BoundExpr>,
    sort: &[SortKey],
    distinct: &Option<Vec<DistinctKey>>,
) -> Demand {
    // `sort` and `distinct` index the *projected* tuple — including hidden
    // ORDER BY / DISTINCT ON columns the binder appended past the visible output
    // width — so they are this node's own demand on its own projections. They
    // contribute nothing directly to the source row, but dropping a projection
    // they key on would leave the sort reading a NULL.
    let wanted = demand.map(|mut wanted| {
        wanted.extend(sort.iter().map(|key| key.column));
        if let Some(distinct) = distinct {
            wanted.extend(distinct.iter().map(|key| key.column));
        }
        wanted
    });

    let mut out = Some(BTreeSet::new());
    match wanted {
        None => out = add_exprs(out, projections.iter()),
        Some(wanted) => {
            for index in wanted {
                match projections.get(index) {
                    Some(expr) => out = add_exprs(out, Some(expr)),
                    // A demand past our own output width: the caller's index
                    // space is not ours, so nothing here can be proven.
                    None => return ALL,
                }
            }
        }
    }
    add_exprs(out, predicate)
}

/// Stamp a projection on each arm of an [`PhysicalPlan::Append`], translating
/// the node's demand — which is in the *named* relation's index space — into
/// each arm's own ordinals.
///
/// An **identity arm** — a partition, or a storage leaf of the named relation —
/// carries the named layout verbatim, so the demand passes through untranslated.
/// That invariant spans the binder, the engines and DDL, so it is checked rather
/// than assumed, and checked across *all* identity arms at once: an ordinal
/// valid for one leaf but past the end of another would panic inside
/// `ProjectionMask::roots` or `BufferTable::visible` rather than merely reading
/// the wrong column, so one mismatched leaf disarms the whole optimization
/// rather than only its own arm.
///
/// A **remapped arm** — an inheritance descendant — is wider, and `map` is
/// exactly the translation, so it is unaffected by that check. `map` is total
/// over the named relation's columns by construction, and an ordinal it somehow
/// did not cover is forwarded out of range so [`ColumnProjection::of`]'s own
/// fail-safe answers `All` — a broken map costs a wide read, never a column the
/// query asked for and did not get.
fn prune_append(arms: &mut [PhysicalAppendArm], demand: Demand, columns: usize) {
    let Some(demand) = demand else {
        for arm in arms.iter_mut() {
            arm.projection = ColumnProjection::All;
        }
        return;
    };
    let identity_layouts_agree = arms
        .iter()
        .filter(|arm| arm.relation.map.is_none())
        .all(|arm| arm.relation.table.schema().columns.len() == columns);
    for arm in arms.iter_mut() {
        let schema = arm.relation.table.schema();
        arm.projection = match &arm.relation.map {
            None if !identity_layouts_agree => ColumnProjection::All,
            None => ColumnProjection::of(demand.iter().copied(), &schema),
            // A demand ordinal the map does not cover is forwarded out of range
            // rather than dropped, so `of`'s own fail-safe turns it into `All`.
            // Dropping it would instead narrow the scan past what the query
            // reads, and the missing column would come back as a placeholder —
            // a wrong answer where this pass promises only a wide read.
            Some(map) => ColumnProjection::of(
                demand
                    .iter()
                    .map(|i| map.get(*i).copied().unwrap_or(usize::MAX)),
                &schema,
            ),
        };
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
            // A leaf's own sunk conjuncts were rebased into the leaf's row by
            // `sink_leaf_filters` (which applies `shift_column_refs(-base)` on
            // the way down), so they share this space and need no shift here.
            let demand = add_exprs(demand, predicate.as_ref());
            let width = *width;
            // An index past the leaf's own width means the demand was derived in
            // the wrong space. Fail safe rather than dropping it: a dropped
            // column reads back NULL and silently returns wrong rows.
            let demand = match demand {
                Some(demand) if demand.iter().any(|index| *index >= width) => ALL,
                other => other,
            };
            match input {
                PhysicalJoinInput::Scan { table, projection } => {
                    *projection = resolve(demand, &table.schema());
                }
                PhysicalJoinInput::Subplan(source) => push(source, demand),
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
            // indices rebase into the right subtree's own base-0 space. This is
            // a partition of the concatenated row, not a clamp — every index
            // lands on exactly one side.
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
fn resolve(demand: Demand, schema: &TableSchema) -> ColumnProjection {
    match demand {
        None => ColumnProjection::All,
        Some(demand) => ColumnProjection::of(demand, schema),
    }
}
