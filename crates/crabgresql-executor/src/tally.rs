//! What a scan reports to the cumulative statistics counters.
//!
//! One [`ScanTally`] per scan node, counting rows in a plain `u64` and touching
//! the shared counters exactly once, on drop — which is what keeps an atomic
//! off the per-row path. Everything it needs is resolved when the scan opens,
//! so the flush is a few relaxed adds against `Arc`s it already holds.

use std::sync::Arc;

use crabgresql_storage_api::TableAm;
use crabgresql_storage_api::pgstat::{IndexCounters, PgStatCounters, RelCounters, WriteKind};

use crate::ExecContext;

/// Counted at the write itself rather than from the statement's command tag,
/// which is what makes a partitioned write land on the *leaf* that took the row
/// (as PostgreSQL counts it), a `RETURNING` statement count at all, and a
/// routine body's DML count like any other.
pub(crate) fn count_write(ctx: &ExecContext, table: &Arc<dyn TableAm>, kind: WriteKind, rows: u64) {
    if rows == 0 {
        return;
    }
    let Some(stats) = &ctx.stats else {
        return;
    };
    let schema = table.schema();
    stats.tuples_written(&schema.namespace, &schema.name, kind, rows);
}

/// The counters one scan will report to when it finishes.
pub(crate) struct ScanTally {
    database: Arc<PgStatCounters>,
    relation: Arc<RelCounters>,
    /// `None` for a sequential scan; what splits `seq_scan` from `idx_scan` at
    /// flush time.
    index: Option<Arc<IndexCounters>>,
    rows: u64,
    /// The statement's start rather than a fresh clock reading: a scan is part
    /// of its statement, and `last_seq_scan` is not worth a syscall per scan.
    at: i64,
}

impl ScanTally {
    /// `None` in a context with no counters (tests, `EXPLAIN`) or no clock.
    pub(crate) fn seq(ctx: &ExecContext, table: &Arc<dyn TableAm>) -> Option<Self> {
        Self::open(ctx, table, None)
    }

    pub(crate) fn index(ctx: &ExecContext, table: &Arc<dyn TableAm>, index: &str) -> Option<Self> {
        Self::open(ctx, table, Some(index))
    }

    fn open(ctx: &ExecContext, table: &Arc<dyn TableAm>, index: Option<&str>) -> Option<Self> {
        let database = ctx.stats.clone()?;
        let at = ctx.fmt.stmt_start().ok()?;
        let schema = table.schema();
        let relation = database.relation(&schema.namespace, &schema.name);
        let index = index.map(|name| relation.index(name));
        Some(Self {
            database,
            relation,
            index,
            rows: 0,
            at,
        })
    }

    pub(crate) fn saw(&mut self, rows: u64) {
        self.rows += rows;
    }
}

impl Drop for ScanTally {
    /// On drop rather than at end-of-stream: a scan is often abandoned early
    /// (`LIMIT`, an existence check, a client that stops fetching) and
    /// PostgreSQL counts those too, so a flush hung off the iterator's
    /// exhaustion would miss exactly the queries a reader cares about.
    fn drop(&mut self) {
        match &self.index {
            Some(index) => {
                index.scan_finished(self.rows, self.at);
                self.relation.idx_scan_finished(self.rows, self.at);
                self.database.tup_fetched(self.rows);
            }
            None => {
                self.relation.seq_scan_finished(self.rows, self.at);
                self.database.tup_returned(self.rows);
            }
        }
    }
}
