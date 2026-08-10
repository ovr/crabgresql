//! Top-N fusion: turn `ORDER BY … LIMIT n` into a bounded sort.
//!
//! A sort under a limit does not need to order the whole input — only to find
//! the `limit + offset` smallest rows. The executor's [`Sort`] materializes
//! every row and orders all of them, which is `O(n log n)` time and `O(n)`
//! memory for an answer that is usually ten rows wide.
//!
//! This pass spots the shape and hands the keys to the `Limit` node, which runs
//! them as a heap-bounded Top-N instead (`TopN` in the executor). It is the same
//! rewrite DataFusion performs by giving `SortExec` a `fetch`, and the same
//! bound ClickHouse's `PartialSortingTransform` keeps as its threshold block.
//!
//! # Why the keys move *up* rather than the limit moving down
//!
//! There is no `Sort` node in a [`PhysicalPlan`]: `sort` is a field on every
//! row-producing node, applied by that node's own tail. `Limit` is the only real
//! wrapper. So "push the limit into the sort" is spelled here as taking the keys
//! out of the child and leaving them on the `Limit` — one field on one variant
//! rather than a `fetch` on eight.
//!
//! # Barriers
//!
//! Fusing is wrong whenever the row that ends up `n`-th is not the row the
//! bounded sort would keep:
//!
//! - **`DISTINCT`** runs *above* the sort and removes rows, so the limit counts
//!   deduplicated rows. A Top-N over the raw input could throw away the only
//!   surviving member of a group and return too few rows. `UNION` (a `SetOp`
//!   carrying `distinct`) is the same case.
//! - **No `LIMIT`** (`LIMIT ALL`, or `OFFSET` alone) leaves the bound unknown.
//!
//! And one barrier that is about speed rather than correctness: a sort the
//! executor would run on Arrow batches is left alone, because lifting the keys
//! also breaks the columnar *projection* below it (the two are one decision in
//! `Source::project_and_sort`) and would trade a vectorized sort for a row-wise
//! Top-N plus a shred.
//!
//! TODO(perf): a columnar Top-N node, so a vectorized sort can be bounded too
//! rather than excluded.

use std::mem;

use crabgresql_binder::{DistinctKey, SortKey};

use crate::{
    PhysicalAggInput, PhysicalInsertSource, PhysicalJoinExpr, PhysicalJoinInput, PhysicalPlan,
    PhysicalSetOpArm,
};

/// The largest `limit + offset` worth fusing. Beyond this the heap holds as much
/// as the full sort would have and the bound stops buying anything, so the plain
/// sort — one `sort_by` over a flat `Vec` — is the better shape.
const MAX_FETCH: u64 = 1 << 20;

/// Fuse every `ORDER BY … LIMIT` in `plan` that qualifies.
///
/// Runs after `projection::push_column_projections`: that pass keeps a scan from
/// pruning the columns a node's `sort` keys address, and this one takes those
/// keys away. Reversing the order would let a scan drop a column the Top-N still
/// reads.
pub(crate) fn fuse_limit_into_sort(plan: &mut PhysicalPlan) {
    // Children first, so a nested `LIMIT` inside a subquery or an insert source
    // is fused too.
    for child in subplans(plan) {
        fuse_limit_into_sort(child);
    }
    let PhysicalPlan::Limit {
        source,
        limit,
        offset,
        sort,
    } = plan
    else {
        return;
    };
    if fetch(*limit, *offset).is_none() {
        return;
    }
    // Asked before the mutable borrow below, and about the child as it stands —
    // with its keys still in place, which is what `vectorization` reads.
    if source.vectorization().sort {
        return;
    }
    let Some((keys, distinct)) = sort_tail(source) else {
        return;
    };
    if keys.is_empty() || distinct.is_some() {
        return;
    }
    *sort = mem::take(keys);
}

/// How many rows a `Limit` needs its input to produce, or `None` when that is
/// unbounded or not worth bounding. Negative counts are rejected at bind time;
/// clamping here only keeps the arithmetic honest.
pub fn fetch(limit: Option<i64>, offset: Option<i64>) -> Option<u64> {
    let limit = limit?.max(0) as u64;
    let fetch = limit.saturating_add(offset.unwrap_or(0).max(0) as u64);
    // `LIMIT 0` returns nothing at all; there is no Top-N to run.
    (fetch > 0 && fetch <= MAX_FETCH).then_some(fetch)
}

/// The `(sort, distinct)` tail of a node that has one.
///
/// The variants without a pair are the ones that carry no tail of their own:
/// `Append` and `Window` are always read through a wrapping `Subquery`, `Limit`
/// applies its source's order unchanged, and the DML nodes have no ORDER BY.
fn sort_tail(plan: &mut PhysicalPlan) -> Option<(&mut Vec<SortKey>, &Option<Vec<DistinctKey>>)> {
    match plan {
        PhysicalPlan::Values { sort, distinct, .. }
        | PhysicalPlan::Select { sort, distinct, .. }
        | PhysicalPlan::IndexScan { sort, distinct, .. }
        | PhysicalPlan::Subquery { sort, distinct, .. }
        | PhysicalPlan::TableFunction { sort, distinct, .. }
        | PhysicalPlan::Join { sort, distinct, .. }
        | PhysicalPlan::Aggregate { sort, distinct, .. }
        | PhysicalPlan::SetOp { sort, distinct, .. } => Some((sort, distinct)),
        _ => None,
    }
}

/// Every nested [`PhysicalPlan`] one node holds, in no particular order.
fn subplans(plan: &mut PhysicalPlan) -> Vec<&mut PhysicalPlan> {
    match plan {
        PhysicalPlan::Subquery { source, .. }
        | PhysicalPlan::Window { source, .. }
        | PhysicalPlan::Limit { source, .. } => vec![source],
        PhysicalPlan::SetOp { arms, .. } => arms
            .iter_mut()
            .map(|PhysicalSetOpArm { plan, .. }| plan)
            .collect(),
        PhysicalPlan::Join { source, .. } => join_subplans(source),
        PhysicalPlan::Aggregate { input, .. } => match input {
            PhysicalAggInput::Join(source) => join_subplans(source),
            PhysicalAggInput::Scan { .. } | PhysicalAggInput::SingleRow => Vec::new(),
        },
        PhysicalPlan::Insert { source, .. } => match source {
            PhysicalInsertSource::Query { input, .. } => vec![input],
            PhysicalInsertSource::Values(_) | PhysicalInsertSource::Tuples { .. } => Vec::new(),
        },
        PhysicalPlan::Values { .. }
        | PhysicalPlan::Select { .. }
        | PhysicalPlan::IndexScan { .. }
        | PhysicalPlan::TableFunction { .. }
        | PhysicalPlan::Append { .. }
        | PhysicalPlan::Update { .. }
        | PhysicalPlan::Delete { .. } => Vec::new(),
    }
}

/// The subplans hanging off a join tree's leaves.
fn join_subplans(node: &mut PhysicalJoinExpr) -> Vec<&mut PhysicalPlan> {
    match node {
        PhysicalJoinExpr::Input { input, .. } => match input {
            PhysicalJoinInput::Subplan(source) => vec![source],
            PhysicalJoinInput::Scan { .. } | PhysicalJoinInput::TableFunction { .. } => Vec::new(),
        },
        PhysicalJoinExpr::Join { left, right, .. } => {
            let mut plans = join_subplans(left);
            plans.extend(join_subplans(right));
            plans
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explain;
    use crate::tests::plan_sql;

    /// The ORDER BY keys of `sql`'s top-level `LIMIT`, paired with the ones its
    /// source kept. A fused plan has the keys on the `Limit` and none below.
    fn limit_keys(sql: &str) -> (usize, usize) {
        let plan = plan_sql(sql);
        let PhysicalPlan::Limit { source, sort, .. } = &plan else {
            panic!("expected a Limit, got {}", explain(&plan)[0]);
        };
        let below = match source.as_ref() {
            PhysicalPlan::Values { sort, .. }
            | PhysicalPlan::Select { sort, .. }
            | PhysicalPlan::IndexScan { sort, .. }
            | PhysicalPlan::Subquery { sort, .. }
            | PhysicalPlan::TableFunction { sort, .. }
            | PhysicalPlan::Join { sort, .. }
            | PhysicalPlan::Aggregate { sort, .. }
            | PhysicalPlan::SetOp { sort, .. } => sort.len(),
            other => panic!("unexpected source {}", explain(other)[0]),
        };
        (sort.len(), below)
    }

    /// The fusion this pass exists for: the sort moves onto the `Limit`, which
    /// runs it bounded, and the source stops sorting altogether.
    #[test]
    fn order_by_under_a_limit_becomes_a_top_n() {
        assert_eq!(limit_keys("SELECT id FROM t ORDER BY id LIMIT 5"), (1, 0));
        // Several keys, and one that is only a sort key (hidden past the visible
        // output), move together.
        assert_eq!(
            limit_keys("SELECT id FROM t ORDER BY name DESC, big NULLS FIRST LIMIT 5 OFFSET 3"),
            (2, 0)
        );
        // Grouped queries are the shape this was written for: ClickBench's
        // `GROUP BY … ORDER BY count DESC LIMIT 10`.
        assert_eq!(
            limit_keys("SELECT name, count(*) FROM t GROUP BY name ORDER BY 2 DESC LIMIT 10"),
            (1, 0)
        );
    }

    /// `DISTINCT` sits *above* the sort and removes rows, so the limit counts
    /// deduplicated rows and a bound on the raw input would be wrong.
    #[test]
    fn distinct_blocks_the_top_n_fusion() {
        assert_eq!(
            limit_keys("SELECT DISTINCT id FROM t ORDER BY id LIMIT 5"),
            (0, 1)
        );
        // `UNION` deduplicates for the same reason; `UNION ALL` does not.
        assert_eq!(
            limit_keys("SELECT id FROM t UNION SELECT id FROM t ORDER BY 1 LIMIT 5"),
            (0, 1)
        );
        assert_eq!(
            limit_keys("SELECT id FROM t UNION ALL SELECT id FROM t ORDER BY 1 LIMIT 5"),
            (1, 0)
        );
    }

    /// Without a row count there is nothing to bound the heap by, and `LIMIT 0`
    /// returns nothing at all.
    #[test]
    fn an_unbounded_or_empty_limit_blocks_the_fusion() {
        assert_eq!(
            limit_keys("SELECT id FROM t ORDER BY id LIMIT ALL OFFSET 2"),
            (0, 1)
        );
        assert_eq!(limit_keys("SELECT id FROM t ORDER BY id OFFSET 2"), (0, 1));
        assert_eq!(limit_keys("SELECT id FROM t ORDER BY id LIMIT 0"), (0, 1));
    }

    /// A lifted sort is the same sort, so the plan the user sees must not move.
    /// `SetOp` is the one node that renders a `Sort` line for keys this pass
    /// takes away, so the `Limit` renderer has to put it back.
    #[test]
    fn a_lifted_sort_still_renders() {
        assert_eq!(
            explain(&plan_sql(
                "SELECT id FROM t UNION ALL SELECT id FROM t ORDER BY 1 LIMIT 5"
            )),
            explain(&{
                let mut plan =
                    plan_sql("SELECT id FROM t UNION ALL SELECT id FROM t ORDER BY 1 LIMIT 5");
                // The same plan with the fusion undone: keys back on the source.
                let PhysicalPlan::Limit { source, sort, .. } = &mut plan else {
                    panic!("expected a Limit");
                };
                let PhysicalPlan::SetOp { sort: below, .. } = source.as_mut() else {
                    panic!("expected a SetOp");
                };
                *below = std::mem::take(sort);
                plan
            })
        );
    }
}
