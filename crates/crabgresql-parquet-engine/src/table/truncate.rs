//! TRUNCATE as a directory swap: staging it, and resolving it at commit or
//! abort.

use std::path::Path;
use std::sync::atomic::Ordering;

use crabgresql_storage_api::StorageError;
use crabgresql_txn::{LockOwner, TxnContext, Xid};

use crate::error::io_error;
use crate::fragment::{next_block_in, remove_dir_all_ok, sync_dir};
use crate::table::ParquetTable;
use crate::wal::{PARQUET_TRUNCATE, RMGR_PARQUET, encode_truncate};

/// An uncommitted directory-swap TRUNCATE staged by one transaction. Because a
/// TRUNCATE holds the table exclusively until it commits, at most one can exist on
/// a table at a time — hence a single `Option`, not a map.
pub(super) struct PendingTruncate {
    pub(super) xid: Xid,
    pub(super) new_rel: u32,
    /// The lock owner holding the table exclusively — needed to release the hold
    /// from the commit/abort path, which only receives the XID.
    pub(super) owner: LockOwner,
    /// `next_block` as of the *first* TRUNCATE in this transaction, restored on
    /// abort so a later insert into the surviving directory cannot re-issue a
    /// block number an existing fragment already owns (invariant P2).
    pub(super) saved_next_block: u32,
}

impl ParquetTable {
    /// Whether a TRUNCATE record of this relation still needs replay to reach it.
    /// Read by the checkpoint; see the field's documentation.
    pub fn truncate_unreconciled(&self) -> bool {
        self.truncate_unreconciled.load(Ordering::Acquire)
    }

    /// The swap is now named by the durable catalog, so replay need not reach the
    /// record any more.
    pub fn truncate_reconciled(&self) {
        self.truncate_unreconciled.store(false, Ordering::Release);
    }

    /// The relfilenode `xid` should read and write: the directory staged by its own
    /// TRUNCATE, else the committed one.
    pub fn effective_rel(&self, xid: Xid) -> u32 {
        if self.has_pending.load(Ordering::Acquire)
            && let Some(p) = self
                .pending
                .read()
                .unwrap_or_else(|_| panic!("rwlock poisoned"))
                .as_ref()
            && p.xid == xid
        {
            return p.new_rel;
        }
        self.relfilenode()
    }

    /// The `(new_rel, owner)` of a TRUNCATE staged by `xid`, if any.
    pub(crate) fn staged_truncate(&self, xid: Xid) -> Option<(u32, LockOwner)> {
        self.pending
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .as_ref()
            .filter(|p| p.xid == xid)
            .map(|p| (p.new_rel, p.owner))
    }

    /// Release the exclusive hold a committed TRUNCATE kept (keyed by its lock
    /// owner). Call it after persisting the swap [`ParquetTable::finish_transaction`]
    /// returned, whether or not that persist succeeded.
    pub fn release_truncate_lock(&self, owner: LockOwner) {
        self.lock.release_exclusive(owner);
    }

    /// Apply a committed TRUNCATE: the staged directory becomes the live one.
    /// Returns the superseded relfilenode, or `None` if nothing was staged by `xid`.
    ///
    /// Deliberately leaves `next_block` alone (invariant P2): the truncating
    /// transaction may already have filled blocks in the new directory, and
    /// restarting the counter would hand out a block number a fragment there
    /// already owns — duplicate TIDs in a scan and a wrong row from `fetch`.
    pub(crate) fn commit_truncate(&self, xid: Xid) -> Option<u32> {
        let mut pending = self
            .pending
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let p = pending.take_if(|p| p.xid == xid)?;
        let old = self.live_rel.swap(p.new_rel, Ordering::Relaxed);
        self.has_pending.store(false, Ordering::Release);
        // `truncate_unreconciled` is deliberately NOT cleared here — see its
        // documentation. The caller still has directory work and a catalog write
        // ahead of it, and the record is the swap's only durable trace until that
        // write lands.
        // The measurement described the directory that just went away.
        self.forget_analyzed();
        Some(old)
    }

    /// Discard a staged TRUNCATE on abort: the live directory keeps its rows, and
    /// the block counter returns to where the first TRUNCATE found it.
    pub(crate) fn abort_truncate(&self, xid: Xid) {
        let mut pending = self
            .pending
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let Some(p) = pending.take_if(|p| p.xid == xid) else {
            return;
        };
        // The swap never happened, so no replay is needed to reconcile it.
        self.truncate_reconciled();
        self.has_pending.store(false, Ordering::Release);
        *self
            .next_block
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned")) = p.saved_next_block;
    }

    /// Point the table at `new` after recovery applied a committed swap (the
    /// on-disk catalog lagged the WAL). Clears any stale pending state.
    ///
    /// Re-derives `next_block` from the new directory (invariant P4): the handle
    /// was opened against the *old* one, so a carried-over counter can sit below
    /// the highest block the new directory already holds and collide with it.
    pub fn rebind(&self, new: u32) -> Result<(), StorageError> {
        // Both fallible steps run BEFORE `live_rel` is published: a caller that
        // logs the error and carries on (startup must not abort over one relation)
        // would otherwise leave the table pointing at the new directory while the
        // counter still describes the old one — the block collision P4 exists to
        // prevent.
        let dir = self.dir_of(new);
        std::fs::create_dir_all(&dir)
            .map_err(|error| io_error("create Parquet table directory", error))?;
        let next_block = next_block_in(&dir)?;
        {
            // `pending` and `has_pending` are cleared under one write guard, so no
            // reader can observe the gate set with nothing behind it.
            let mut pending = self
                .pending
                .write()
                .unwrap_or_else(|_| panic!("rwlock poisoned"));
            *pending = None;
            self.has_pending.store(false, Ordering::Release);
        }
        // Recovery applied the swap and persisted the catalog: what the pin waited
        // for.
        self.truncate_reconciled();
        *self
            .next_block
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned")) = next_block;
        self.live_rel.store(new, Ordering::Relaxed);
        // The seeded ANALYZE result came from the catalog, which described the
        // pre-swap directory; as on the commit path, go back to never-analyzed
        // rather than let it describe the relation we just swapped in.
        self.forget_analyzed();
        Ok(())
    }

    /// Create the staged directory, WAL-log the swap and record it in memory.
    /// Split out of [`ParquetTable::truncate_in`] so every failure before the state
    /// transition takes the same cleanup path.
    pub(crate) fn stage_truncate(
        &self,
        old: u32,
        new: u32,
        new_dir: &Path,
        txn: &TxnContext,
    ) -> Result<(), StorageError> {
        std::fs::create_dir_all(new_dir)
            .map_err(|error| io_error("create Parquet table directory", error))?;
        sync_dir(&self.root.join("parquet"))?;
        // WAL-log the swap intent {old, new, relation} and flush it. Recovery
        // applies the swap only for a committed XID, so the record is safe to write
        // now; and because it carries the XID, a transaction that only TRUNCATEs is
        // still observed by recovery's XID allocator without a separate
        // `PARQUET_XID_OBSERVED` record.
        // Held from the append through every piece of state a checkpoint reads, so
        // a redo point cannot be sampled above a record whose relation still looks
        // unpinned. A block expression, so it ends before the `remove_dir_all_ok`
        // below rather than covering that file I/O too.
        let superseded = {
            let _delay = self.wal.delay_checkpoint();
            let lsn = self.wal.append(
                RMGR_PARQUET,
                PARQUET_TRUNCATE,
                txn.xid,
                &encode_truncate(&self.schema.namespace, &self.schema.name, old, new),
            );
            self.wal
                .flush(lsn.end)
                .map_err(|error| io_error("flush Parquet TRUNCATE WAL record", error))?;
            // Only once the record is durable: a failed flush must leave nothing
            // pinned.
            self.truncate_unreconciled.store(true, Ordering::Release);
            self.staged_xids
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"))
                .insert(txn.xid);
            let mut pending = self
                .pending
                .write()
                .unwrap_or_else(|_| panic!("rwlock poisoned"));
            let mut next = self
                .next_block
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"));
            // A second TRUNCATE in one transaction keeps the FIRST saved counter: abort
            // must restore what the transaction found, not what its own first TRUNCATE
            // left behind (invariant P2).
            let saved = pending
                .as_ref()
                .filter(|p| p.xid == txn.xid)
                .map_or(*next, |p| p.saved_next_block);
            let superseded = pending.replace(PendingTruncate {
                xid: txn.xid,
                new_rel: new,
                owner: txn.lock_owner,
                saved_next_block: saved,
            });
            self.has_pending.store(true, Ordering::Release);
            // The staged directory is empty, so its fragments start from block 1 again.
            *next = 1;
            drop(next);
            drop(pending);
            superseded
        };
        if let Some(superseded) = superseded {
            // Used only by this uncommitted transaction; reclaim it now.
            let _ = remove_dir_all_ok(&self.dir_of(superseded.new_rel));
        }
        Ok(())
    }

    /// Transactional TRUNCATE via a fragment-directory swap — the Parquet twin of
    /// the heap's relfilenode swap. Stages a fresh, empty `parquet/<new>/` and holds
    /// the table exclusively until the transaction ends; the swap is applied on
    /// commit and discarded on abort by [`ParquetTable::finish_transaction`]. The
    /// old directory stays intact until commit, so a rollback or a
    /// crash-before-commit restores every row.
    pub(super) fn truncate_in(&self, txn: &TxnContext) -> Result<(), StorageError> {
        // AccessExclusiveLock: block concurrent readers/writers of this table until
        // we commit, so no one reads the directory we are about to remove or writes
        // fragments the swap would drop. Held until txn end.
        self.lock.acquire_exclusive(txn.lock_owner);
        let old = self.effective_rel(txn.xid);
        // A fresh, never-reused relfilenode for the empty post-truncate directory.
        let new = self.relfilenodes.alloc_relfilenode();
        let new_dir = self.dir_of(new);
        match self.stage_truncate(old, new, &new_dir, txn) {
            Ok(()) => Ok(()),
            Err(error) => {
                // Nothing was staged, so nothing will ever release this hold on our
                // behalf — unless a previous TRUNCATE in the same transaction owns
                // it, in which case its commit/abort still does. The empty directory
                // is not named by the catalog: `gc_orphan_parquet_dirs` reclaims it.
                let _ = remove_dir_all_ok(&new_dir);
                if self.staged_truncate(txn.xid).is_none() {
                    self.lock.release_exclusive(txn.lock_owner);
                }
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crabgresql_storage_api::{StorageError, TableAm};
    use crabgresql_txn::{CommandId, Xid};
    use crabgresql_types::{PgType, Value};
    use crabgresql_wal::Wal;

    use crate::test_support::{finish, manager, open_table, parquet_files, schema};
    use std::fs::File;

    use crabgresql_storage_api::{ColumnProjection, Tid, Tuple};

    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    use crate::test_support::{fragment_dirs, scan_values};

    #[test]
    fn truncate_commit_empties_the_table_and_swaps_the_directory() -> anyhow::Result<()> {
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

        let truncater = tm.allocate_xid();
        table.truncate(&tm.context(truncater, CommandId::FIRST))?;
        // Read-your-own-truncate: the truncating transaction sees an empty table.
        // (Another session cannot look at all while the TRUNCATE is staged — its
        // AccessShare hold waits for the AccessExclusive one, as in PostgreSQL.)
        assert!(scan_values(&table, &tm.context(truncater, CommandId::FIRST)).is_empty());
        tm.commit(truncater)?;
        let swapped = finish(&table, truncater, true)?.ok_or_else(|| {
            anyhow::anyhow!("a committed TRUNCATE must report its new relfilenode")
        })?;

        assert_eq!(table.relfilenode(), swapped);
        assert_eq!(
            fragment_dirs(dir.path())?,
            vec![swapped],
            "old directory gone"
        );
        assert!(scan_values(&table, &tm.context(Xid::INVALID, CommandId::FIRST)).is_empty());
        Ok(())
    }

    #[test]
    fn truncate_abort_restores_every_row_and_removes_the_staged_directory() -> anyhow::Result<()> {
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
        table.insert_many(
            vec![vec![Value::Int4(1)], vec![Value::Int4(2)]],
            &tm.context(loader, CommandId::FIRST),
        )?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        let truncater = tm.allocate_xid();
        table.truncate(&tm.context(truncater, CommandId::FIRST))?;
        tm.abort(truncater);
        assert_eq!(finish(&table, truncater, false)?, None);

        assert_eq!(table.relfilenode(), 1);
        assert_eq!(fragment_dirs(dir.path())?, vec![1], "staged directory gone");
        assert_eq!(
            scan_values(&table, &tm.context(Xid::INVALID, CommandId::FIRST)),
            vec![1, 2]
        );
        Ok(())
    }

    /// Invariant P1: a post-TRUNCATE insert must stamp the *staged* directory's
    /// relfilenode into its footer, or reading it back reports valid bytes as
    /// corrupt — including through a freshly opened handle after a restart.
    #[test]
    fn post_truncate_fragments_carry_the_staged_relfilenode() -> anyhow::Result<()> {
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

        let xid = tm.allocate_xid();
        table.truncate(&tm.context(xid, CommandId::FIRST))?;
        table.insert(vec![Value::Int4(7)], &tm.context(xid, CommandId(1)))?;
        // Visible to its own transaction from the staged directory, before commit.
        assert_eq!(scan_values(&table, &tm.context(xid, CommandId(2))), vec![7]);
        tm.commit(xid)?;
        let swapped = finish(&table, xid, true)?.ok_or_else(|| anyhow::anyhow!("missing swap"))?;

        let file = parquet_files(dir.path(), swapped)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing promoted fragment"))?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(&file)?)?;
        let stamped = reader
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .and_then(|kv| {
                kv.iter()
                    .find(|item| item.key == crate::fragment::META_REL)
                    .and_then(|item| item.value.clone())
            })
            .ok_or_else(|| anyhow::anyhow!("fragment has no relfilenode metadata"))?;
        assert_eq!(stamped, swapped.to_string());

        // A fresh handle over the swapped directory reads the same row back.
        let reopened = open_table(dir.path(), swapped, schema("t", &[PgType::Int4]), wal)?;
        assert_eq!(
            scan_values(&reopened, &tm.context(Xid::INVALID, CommandId::FIRST)),
            vec![7]
        );
        Ok(())
    }

    /// Invariant P2, abort half: the block counter must return to what the
    /// transaction found, or a later insert re-issues a block an existing fragment
    /// in the surviving directory already owns — duplicate TIDs.
    #[test]
    fn aborted_truncate_restores_the_block_counter() -> anyhow::Result<()> {
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
        table.insert(vec![Value::Int4(2)], &tm.context(loader, CommandId(1)))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        let truncater = tm.allocate_xid();
        table.truncate(&tm.context(truncater, CommandId::FIRST))?;
        // Two TRUNCATEs in one transaction must still restore the FIRST counter.
        table.truncate(&tm.context(truncater, CommandId(1)))?;
        tm.abort(truncater);
        finish(&table, truncater, false)?;

        let writer = tm.allocate_xid();
        let tids = table.insert(vec![Value::Int4(3)], &tm.context(writer, CommandId::FIRST))?;
        assert_eq!(tids, Tid::new(3, 1), "block 1 and 2 are still occupied");
        tm.commit(writer)?;
        finish(&table, writer, true)?;
        assert_eq!(
            scan_values(&table, &tm.context(Xid::INVALID, CommandId::FIRST)),
            vec![1, 2, 3]
        );
        assert_eq!(fragment_dirs(dir.path())?, vec![1]);
        Ok(())
    }

    /// Invariant P2, commit half: the counter must NOT restart, or the next insert
    /// collides with the fragments the truncating transaction itself wrote.
    #[test]
    fn committed_truncate_keeps_the_counter_its_own_inserts_advanced() -> anyhow::Result<()> {
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
        table.truncate(&tm.context(xid, CommandId::FIRST))?;
        assert_eq!(
            table.insert(vec![Value::Int4(1)], &tm.context(xid, CommandId(1)))?,
            Tid::new(1, 1)
        );
        tm.commit(xid)?;
        finish(&table, xid, true)?;

        let writer = tm.allocate_xid();
        assert_eq!(
            table.insert(vec![Value::Int4(2)], &tm.context(writer, CommandId::FIRST))?,
            Tid::new(2, 1),
            "the post-truncate insert already owns block 1"
        );
        tm.commit(writer)?;
        finish(&table, writer, true)?;
        assert_eq!(
            scan_values(&table, &tm.context(Xid::INVALID, CommandId::FIRST)),
            vec![1, 2]
        );
        Ok(())
    }

    /// `INSERT; TRUNCATE; ROLLBACK`: the pending fragments the transaction staged
    /// *before* its TRUNCATE live in the surviving directory and must be unlinked
    /// there, not promoted.
    #[test]
    fn aborted_insert_then_truncate_unlinks_the_pre_truncate_fragments() -> anyhow::Result<()> {
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

        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(2)], &tm.context(xid, CommandId::FIRST))?;
        table.truncate(&tm.context(xid, CommandId(1)))?;
        tm.abort(xid);
        finish(&table, xid, false)?;

        assert_eq!(fragment_dirs(dir.path())?, vec![1]);
        assert_eq!(
            parquet_files(dir.path(), 1)?.len(),
            1,
            "only the committed loader's fragment remains"
        );
        assert_eq!(
            std::fs::read_dir(dir.path().join("parquet").join("1"))?.count(),
            1,
            "the aborted transaction's .pending fragment was unlinked"
        );
        Ok(())
    }

    /// A superseded staged directory is reclaimed as soon as the next TRUNCATE in
    /// the same transaction replaces it.
    #[test]
    fn double_truncate_in_one_transaction_reclaims_the_superseded_directory() -> anyhow::Result<()>
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
        let xid = tm.allocate_xid();
        table.truncate(&tm.context(xid, CommandId::FIRST))?;
        let first_staged = fragment_dirs(dir.path())?;
        assert_eq!(first_staged.len(), 2, "live plus staged");
        table.truncate(&tm.context(xid, CommandId(1)))?;
        let second_staged = fragment_dirs(dir.path())?;
        assert_eq!(second_staged.len(), 2, "the superseded directory is gone");
        assert_ne!(first_staged, second_staged);
        tm.commit(xid)?;
        let swapped = finish(&table, xid, true)?.ok_or_else(|| anyhow::anyhow!("missing swap"))?;
        assert_eq!(fragment_dirs(dir.path())?, vec![swapped]);
        Ok(())
    }

    #[test]
    fn drop_storage_removes_the_staged_directory_too() -> anyhow::Result<()> {
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
        table.truncate(&tm.context(xid, CommandId::FIRST))?;
        assert_eq!(fragment_dirs(dir.path())?.len(), 2);
        table.drop_storage()?;
        assert!(fragment_dirs(dir.path())?.is_empty());
        Ok(())
    }

    /// The same owner may TRUNCATE a table it is already scanning (lock upgrade),
    /// while another owner's in-flight scan blocks the TRUNCATE until it finishes.
    #[test]
    fn truncate_upgrades_over_its_own_scan_and_waits_for_a_foreign_one() -> anyhow::Result<()> {
        use crabgresql_txn::LockOwner;
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = Arc::new(open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?);
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        // Same owner: an open scan does not self-deadlock the TRUNCATE.
        let own = tm.allocate_xid();
        let mut own_ctx = tm.context(own, CommandId::FIRST);
        own_ctx.lock_owner = LockOwner(42);
        let cursor = table.scan(&own_ctx, &ColumnProjection::All);
        table.truncate(&own_ctx)?;
        drop(cursor);
        tm.abort(own);
        finish(&table, own, false)?;

        // Foreign owner: the TRUNCATE must wait for the scan to be dropped.
        let mut reader_ctx = tm.context(Xid::INVALID, CommandId::FIRST);
        reader_ctx.lock_owner = LockOwner(7);
        let cursor = table.scan(&reader_ctx, &ColumnProjection::All);
        let truncater = tm.allocate_xid();
        let mut truncate_ctx = tm.context(truncater, CommandId::FIRST);
        truncate_ctx.lock_owner = LockOwner(8);
        let (tx, rx) = mpsc::channel();
        let worker = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || {
                let outcome = table.truncate(&truncate_ctx);
                tx.send(()).expect("send");
                outcome
            })
        };
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "TRUNCATE must wait for a foreign owner's open scan"
        );
        drop(cursor);
        rx.recv_timeout(Duration::from_secs(10))
            .expect("TRUNCATE must proceed once the scan is dropped");
        worker.join().expect("worker panicked")?;
        Ok(())
    }

    /// Invariant P1 under contention: a reader that parks in `acquire_shared` while
    /// a TRUNCATE holds the table must read the directory that exists when it is
    /// finally granted the hold — not the one it saw before waiting. Resolving the
    /// relfilenode before the lock made a plain scan of healthy data fail with
    /// `CorruptData`, because the footer stamp belongs to the new generation.
    #[test]
    fn a_scan_that_waits_for_a_truncate_reads_the_swapped_in_directory() -> anyhow::Result<()> {
        use crabgresql_txn::LockOwner;
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = Arc::new(open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?);
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        // The truncater commits (so the CLOG already says committed and its rows are
        // MVCC-visible) but has not been finalized yet, so it still holds the table.
        let truncater = tm.allocate_xid();
        let mut truncate_ctx = tm.context(truncater, CommandId::FIRST);
        truncate_ctx.lock_owner = LockOwner(8);
        table.truncate(&truncate_ctx)?;
        table.insert(vec![Value::Int4(7)], &{
            let mut ctx = tm.context(truncater, CommandId(1));
            ctx.lock_owner = LockOwner(8);
            ctx
        })?;
        tm.commit(truncater)?;

        // A foreign reader whose snapshot sees the committed truncater, parked on the
        // exclusive hold.
        let mut reader_ctx = tm.context(Xid::INVALID, CommandId::FIRST);
        reader_ctx.lock_owner = LockOwner(7);
        let (tx, rx) = mpsc::channel();
        let reader = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || {
                let rows: Vec<Result<(Tid, Tuple), StorageError>> =
                    table.scan(&reader_ctx, &ColumnProjection::All).collect();
                tx.send(()).expect("send");
                rows
            })
        };
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "the reader must wait for the TRUNCATE's exclusive hold"
        );
        // Finalize: the directory the reader looked at before waiting is now gone.
        finish(&table, truncater, true)?;
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the reader must proceed once the hold is released");
        let rows = reader.join().expect("reader panicked");
        let values: Vec<i32> = rows
            .into_iter()
            .map(
                |row| match row.expect("a waiting scan must not report corruption").1[0] {
                    Value::Int4(value) => value,
                    ref other => panic!("unexpected value {other:?}"),
                },
            )
            .collect();
        assert_eq!(values, vec![7]);
        Ok(())
    }

    /// A TRUNCATE that fails before staging anything must not keep the exclusive
    /// hold: nothing would ever release it (the transaction has no staged swap for
    /// the finalize path to find), and the table would be unusable for the process
    /// lifetime.
    #[test]
    fn a_truncate_that_fails_to_stage_releases_the_exclusive_hold() -> anyhow::Result<()> {
        use crabgresql_txn::LockOwner;

        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        // Occupy the path the allocator will hand out with a regular file, so
        // `create_dir_all` for the staged directory fails.
        std::fs::write(dir.path().join("parquet").join("1000"), b"not a directory")?;

        let truncater = tm.allocate_xid();
        let mut truncate_ctx = tm.context(truncater, CommandId::FIRST);
        truncate_ctx.lock_owner = LockOwner(8);
        let error = table
            .truncate(&truncate_ctx)
            .expect_err("staging must fail when the directory cannot be created");
        assert!(matches!(error, StorageError::Io(_)), "{error:?}");
        tm.abort(truncater);
        finish(&table, truncater, false)?;

        // Another owner can still read and write: the hold was released.
        let mut reader_ctx = tm.context(Xid::INVALID, CommandId::FIRST);
        reader_ctx.lock_owner = LockOwner(7);
        assert!(scan_values(&table, &reader_ctx).is_empty());
        let writer = tm.allocate_xid();
        let mut write_ctx = tm.context(writer, CommandId::FIRST);
        write_ctx.lock_owner = LockOwner(7);
        table.insert(vec![Value::Int4(3)], &write_ctx)?;
        tm.commit(writer)?;
        finish(&table, writer, true)?;
        let mut reader_ctx = tm.context(Xid::INVALID, CommandId::FIRST);
        reader_ctx.lock_owner = LockOwner(7);
        assert_eq!(scan_values(&table, &reader_ctx), vec![3]);
        Ok(())
    }

    /// `rebind`'s fallible steps run before it publishes the new relfilenode: the
    /// caller logs a failure and keeps serving the table, so a half-applied rebind
    /// (new directory, old block counter) would hand out colliding block numbers.
    #[test]
    fn a_failed_rebind_leaves_the_table_on_its_old_directory() -> anyhow::Result<()> {
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

        // A regular file where the rebind target directory should be.
        std::fs::write(dir.path().join("parquet").join("77"), b"not a directory")?;
        table
            .rebind(77)
            .expect_err("rebind must fail when the directory cannot be created");
        assert_eq!(
            table.relfilenode(),
            1,
            "the table stays on its old directory"
        );

        // And the block counter still describes that directory: the next insert does
        // not collide with the existing fragment.
        let writer = tm.allocate_xid();
        let tid = table.insert(vec![Value::Int4(2)], &tm.context(writer, CommandId::FIRST))?;
        assert_eq!(tid, Tid::new(2, 1));
        tm.commit(writer)?;
        finish(&table, writer, true)?;
        assert_eq!(
            scan_values(&table, &tm.context(Xid::INVALID, CommandId::FIRST)),
            vec![1, 2]
        );
        Ok(())
    }
}
