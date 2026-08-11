//! `ANALYZE`: measure a relation and hand back the statistics the planner and
//! `pg_class` report.
//!
//! # Sampling
//!
//! PostgreSQL samples: it reads `300 × default_statistics_target` rows drawn
//! from randomly chosen blocks and extrapolates. This implementation **reads
//! every row instead**, so [`SampleTarget::target_rows`] is recorded but not
//! honored. Reading everything buys two things:
//!
//! - the result is exact, so `reltuples` is a count rather than an estimate;
//! - it is deterministic, which is what makes the statistics testable at all —
//!   a sampled `n_distinct` cannot be asserted, a counted one can.
//!
//! TODO(perf): draw `target_rows` rows from randomly chosen blocks instead of
//! reading every row — `ANALYZE` on a large relation is O(rows) rather than
//! O(sample). The two-stage sampler is a change to [`analyze_heap`]'s body
//! alone: build it on [`crate::heap::HeapTable`]'s block-at-a-time scan (which
//! already captures the block count up front and holds the relation against a
//! concurrent TRUNCATE), not on `TableAm::scan`, which yields tuples and so
//! loses the block boundary the extrapolation needs.

use crabgresql_storage_api::{RelStats, TableAm};
use crabgresql_txn::TxnContext;

use crate::heap::HeapTable;

/// PostgreSQL's `default_statistics_target`: the histogram/MCV size, and the
/// multiplier on the sample row count.
pub const DEFAULT_STATISTICS_TARGET: usize = 100;

/// How large a sample `ANALYZE` should draw.
#[derive(Clone, Copy, Debug)]
pub struct SampleTarget {
    /// Rows to sample. TODO: honor this — [`analyze_heap`] reads every row
    /// instead of sampling (see the module docs).
    pub target_rows: usize,
}

impl Default for SampleTarget {
    /// PostgreSQL's default: `300 × default_statistics_target` rows.
    fn default() -> Self {
        SampleTarget {
            target_rows: 300 * DEFAULT_STATISTICS_TARGET,
        }
    }
}

/// Measure `table` under `txn`'s snapshot.
///
/// Counts only rows visible to `txn`, as PostgreSQL does — statistics describe
/// what a query can actually see, so in-flight and dead versions do not count.
pub fn analyze_heap(table: &HeapTable, txn: &TxnContext, target: SampleTarget) -> RelStats {
    tracing::debug!(
        relation = %table.schema().name,
        target_rows = target.target_rows,
        "ANALYZE: reading every row (sampling not implemented)"
    );
    // One call, so the row count and the page count describe the same file even
    // when this transaction has a staged TRUNCATE or another commits one
    // mid-measurement — see [`HeapTable::measure`].
    let (relpages, reltuples) = table.measure(txn);
    RelStats {
        relpages,
        reltuples,
        analyzed: true,
        columns: Vec::new(),
    }
}
