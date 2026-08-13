//! `relpages`/`reltuples`: measuring them, and caching an ANALYZE result under
//! the transaction that took it.

use std::sync::Arc;

use crabgresql_storage_api::{ColumnProjection, RelStats, StorageError};
use crabgresql_txn::{TxnContext, Xid};

use crate::fragment::{relpages_in, relpages_of};
use crate::table::ParquetTable;

impl ParquetTable {
    /// Measure the relation's current on-disk size in 8 KB pages, ignoring any
    /// cached ANALYZE result. `statistics()` deliberately prefers the cached
    /// value; ANALYZE itself must re-measure, or `relpages` would freeze at
    /// whatever the first ANALYZE recorded and never track the table's growth.
    ///
    /// Deliberately lock-free: `TableAm::statistics` has no `TxnContext` (so no
    /// lock owner) and is called while planning, where blocking behind a TRUNCATE's
    /// transaction would stall unrelated queries. The cost is a race with a
    /// committing TRUNCATE, which publishes the new relfilenode before removing the
    /// old directory — so a listing that lost the race is simply retried against the
    /// directory that is live by then.
    pub fn measure_relpages(&self) -> Result<u32, StorageError> {
        let rel = self.relfilenode();
        match relpages_in(&self.dir_of(rel)) {
            Ok(relpages) => Ok(relpages),
            Err(error) => {
                let now = self.relfilenode();
                if now == rel {
                    return Err(error);
                }
                relpages_in(&self.dir_of(now))
            }
        }
    }

    /// Size and row count for ANALYZE, taken under one shared hold from ONE listing
    /// of the fragment directory — the scan below reuses the very fragments that
    /// were measured, rather than re-listing. Two listings would let a fragment
    /// promoted (`.pending` renamed away) in between be stat'd at a path that no
    /// longer exists — silently contributing 0 bytes while its rows still count —
    /// and would let an ANALYZE inside an uncommitted TRUNCATE pair one directory's
    /// rows with another's pages.
    pub fn measure(&self, txn: &TxnContext) -> Result<(u32, f64), StorageError> {
        let guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        let visible = self.visible_fragments(rel, txn)?;
        let relpages = relpages_of(&visible);
        let mut rows = 0u64;
        // Only the rows are being counted, so read the narrowest column and skip
        // the rest. The count is unchanged — a mask prunes columns, never rows.
        let projection = ColumnProjection::of([], &self.schema);
        for row in self.scan_over(rel, visible, guard, &projection) {
            row?;
            rows += 1;
        }
        Ok((relpages, rows as f64))
    }

    /// Drop the cached ANALYZE result, returning the relation to never-analyzed —
    /// which is what PostgreSQL reports after a TRUNCATE (`relpages = 0`,
    /// `reltuples = -1`), not a measured zero. Called when the fragments the
    /// measurement described stop existing.
    pub(crate) fn forget_analyzed(&self) {
        *self
            .analyzed
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned")) = None;
    }

    /// Drop the cached ANALYZE result only if `xid` is the transaction that took
    /// it. Used when a rollback unlinks storage that transaction had staged: its
    /// own measurement covered those fragments, while a measurement taken before
    /// the transaction started still describes what survives.
    pub(crate) fn forget_analyzed_by(&self, xid: Xid) {
        let mut analyzed = self
            .analyzed
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        if analyzed.is_some_and(|(measured, _, _)| measured == xid) {
            *analyzed = None;
        }
    }

    /// Record a result measured outside any transaction — the catalog's persisted
    /// statistics, replayed into the handle at startup.
    pub fn set_analyzed(&self, relpages: u32, reltuples: f64) {
        self.set_analyzed_by(Xid::INVALID, relpages, reltuples);
    }

    /// Record the result of an ANALYZE run by `xid` (see the `analyzed` field).
    pub fn set_analyzed_by(&self, xid: Xid, relpages: u32, reltuples: f64) {
        *self
            .analyzed
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned")) = Some((xid, relpages, reltuples));
    }

    /// The body of [`TableAm::statistics`].
    pub(super) fn stats_snapshot(&self) -> RelStats {
        if let Some((_, relpages, reltuples)) = *self
            .analyzed
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
        {
            return RelStats {
                relpages,
                reltuples,
                analyzed: true,
                // No live page count: sizing this relation means walking its
                // fragment directory, which is more than the planner's
                // per-statement budget. The measured figure stands alone, so a
                // plan here does not rescale by growth the way a heap's does.
                curpages: None,
                columns: Arc::from([]),
            };
        }
        let Ok(relpages) = self.measure_relpages() else {
            return RelStats::unknown(&self.schema);
        };
        RelStats::from_pages(relpages, &self.schema)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crabgresql_storage_api::TableAm;
    use crabgresql_txn::{CommandId, Xid};
    use crabgresql_types::{PgType, Value};
    use crabgresql_wal::Wal;

    use crate::test_support::{finish, manager, open_table, schema};

    /// `statistics()` intentionally returns the last ANALYZE's cached numbers,
    /// so ANALYZE itself must re-measure — otherwise `relpages` freezes at the
    /// first value recorded and never tracks the table's growth.
    #[test]
    fn measure_relpages_ignores_the_cached_analyze_result() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("stats", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        finish(&table, xid, true)?;

        let measured = table.measure_relpages()?;
        table.set_analyzed(9_999, 1.0);
        assert_eq!(
            table.statistics().relpages,
            9_999,
            "cache serves statistics"
        );
        assert_eq!(
            table.measure_relpages()?,
            measured,
            "ANALYZE re-measures instead of reading its own cached value back"
        );
        Ok(())
    }

    #[test]
    fn truncate_resets_the_analyze_cache_on_commit_only() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;
        table.set_analyzed(7, 1.0);

        let rolled_back = tm.allocate_xid();
        table.truncate(&tm.context(rolled_back, CommandId::FIRST))?;
        assert_eq!(table.statistics().relpages, 7, "still cached while staged");
        tm.abort(rolled_back);
        finish(&table, rolled_back, false)?;
        assert_eq!(table.statistics().relpages, 7, "an abort changes nothing");

        let committed = tm.allocate_xid();
        table.truncate(&tm.context(committed, CommandId::FIRST))?;
        tm.commit(committed)?;
        finish(&table, committed, true)?;
        let stats = table.statistics();
        assert!(
            !stats.analyzed,
            "back to never-analyzed, as PostgreSQL reports"
        );
        assert_eq!(stats.relpages, 0);
        Ok(())
    }

    /// A measurement taken inside a transaction covers that transaction's own
    /// not-yet-committed fragments (the same rows it counts). If the transaction
    /// rolls back, those fragments are unlinked, so the cached result describes
    /// bytes that no longer exist and must be dropped.
    #[test]
    fn a_rollback_discards_statistics_measured_over_its_own_fragments() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;

        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(xid, CommandId::FIRST))?;
        let (relpages, reltuples) = table.measure(&tm.context(xid, CommandId(1)))?;
        assert!(relpages > 0, "its own pending fragment is measured");
        assert_eq!(reltuples, 1.0);
        table.set_analyzed_by(xid, relpages, reltuples);

        tm.abort(xid);
        finish(&table, xid, false)?;
        let stats = table.statistics();
        assert!(
            !stats.analyzed,
            "the rolled-back measurement must not survive: {stats:?}"
        );
        assert_eq!(stats.relpages, 0);
        Ok(())
    }

    /// The same rule for a rolled-back TRUNCATE: a measurement the transaction took
    /// of its staged (empty) directory goes with it, while one taken before the
    /// TRUNCATE still describes the directory that survives.
    #[test]
    fn a_rolled_back_truncate_discards_only_its_own_measurement() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        // Measured before any TRUNCATE: still valid after one rolls back.
        let (relpages, reltuples) = table.measure(&tm.context(Xid::INVALID, CommandId::FIRST))?;
        table.set_analyzed(relpages, reltuples);
        let rolled_back = tm.allocate_xid();
        table.truncate(&tm.context(rolled_back, CommandId::FIRST))?;
        tm.abort(rolled_back);
        finish(&table, rolled_back, false)?;
        let stats = table.statistics();
        assert!(stats.analyzed, "an older measurement survives an abort");
        assert_eq!((stats.relpages, stats.reltuples), (relpages, reltuples));

        // Measured by the truncating transaction, against the staged empty
        // directory: discarded when that directory is.
        let second = tm.allocate_xid();
        table.truncate(&tm.context(second, CommandId::FIRST))?;
        let (staged_pages, staged_rows) = table.measure(&tm.context(second, CommandId(1)))?;
        assert_eq!((staged_pages, staged_rows), (0, 0.0));
        table.set_analyzed_by(second, staged_pages, staged_rows);
        tm.abort(second);
        finish(&table, second, false)?;
        assert!(
            !table.statistics().analyzed,
            "a measurement of the discarded directory must not outlive it"
        );
        Ok(())
    }

    /// ANALYZE inside an uncommitted TRUNCATE must measure ONE directory: pairing
    /// the staged directory's row count with the old one's page count would persist
    /// statistics describing a relation that never existed.
    #[test]
    fn measure_inside_an_uncommitted_truncate_sees_only_the_staged_directory() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;
        let (loaded_pages, loaded_rows) =
            table.measure(&tm.context(Xid::INVALID, CommandId::FIRST))?;
        assert!(loaded_pages > 0);
        assert_eq!(loaded_rows, 1.0);

        let xid = tm.allocate_xid();
        table.truncate(&tm.context(xid, CommandId::FIRST))?;
        assert_eq!(table.measure(&tm.context(xid, CommandId(1)))?, (0, 0.0));
        // And once the TRUNCATE rolls back, the old measurement is what everyone
        // (including another session, which may now take the lock) sees again.
        tm.abort(xid);
        finish(&table, xid, false)?;
        assert_eq!(
            table.measure(&tm.context(Xid::INVALID, CommandId::FIRST))?,
            (loaded_pages, loaded_rows)
        );
        Ok(())
    }
}
