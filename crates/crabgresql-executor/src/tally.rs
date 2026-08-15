//! What a scan reports to the cumulative statistics counters.
//!
//! One [`ScanTally`] per scan node. It counts rows in a plain `u64` field and
//! touches the shared counters exactly once, when the node is dropped — the
//! whole reason `pg_stat_user_tables` can be served without putting an atomic
//! on the per-row path. Everything it needs (the relation's counters, the
//! index's) is resolved when the scan opens, so the flush is a handful of
//! relaxed adds against `Arc`s it already holds.
//!
//! `None` — a context with no counters behind it, which is every unit test and
//! every `EXPLAIN` — costs one `Option` check per scan and nothing per row.

use std::sync::Arc;

use crabgresql_storage_api::TableAm;
use crabgresql_storage_api::pgstat::{IndexCounters, PgStatCounters, RelCounters, WriteKind};

use crate::ExecContext;

/// Count `rows` rows written to `table`, for `n_tup_ins`/`n_tup_upd`/`n_tup_del`
/// and the database-wide `tup_inserted`/`tup_updated`/`tup_deleted`.
///
/// Counted at the write itself rather than from the statement's command tag,
/// which is what makes a partitioned write land on the *leaf* that received the
/// row (as PostgreSQL counts it), a `RETURNING` statement count at all, and a
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
    /// The index this scan went through, or `None` for a sequential scan. What
    /// splits `seq_scan` from `idx_scan` at flush time.
    index: Option<Arc<IndexCounters>>,
    rows: u64,
    /// The instant the scan is stamped with, for `last_seq_scan` /
    /// `last_idx_scan`. The statement's start rather than a fresh reading: a
    /// scan is part of its statement, the clock is already in the context, and
    /// reading the wall clock per scan would cost more than the column is worth.
    at: i64,
}

impl ScanTally {
    /// The tally for a sequential scan of `table`, or `None` in a context with
    /// no counters (tests, `EXPLAIN`) or with no clock to stamp with.
    pub(crate) fn seq(ctx: &ExecContext, table: &Arc<dyn TableAm>) -> Option<Self> {
        Self::open(ctx, table, None)
    }

    /// The tally for a scan of `table` through the index named `index`.
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

    /// Note `rows` more rows read by this scan.
    pub(crate) fn saw(&mut self, rows: u64) {
        self.rows += rows;
    }
}

impl Drop for ScanTally {
    /// Report the scan once, however it ended.
    ///
    /// On drop rather than at end-of-stream because a scan is very often
    /// abandoned early — `LIMIT`, an existence check, a client that stops
    /// fetching — and PostgreSQL counts those scans too. A flush hung off the
    /// iterator's exhaustion would silently miss exactly the queries a reader
    /// most wants to see.
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
