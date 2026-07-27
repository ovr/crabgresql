//! A `USING parquet` relation as the rest of the system sees it.
//!
//! One SQL relation, but two physical stores: an immutable chunk store
//! ([`ParquetTable`]) and a WAL-logged RAM write buffer
//! ([`BufferTable`](crabgresql_buffer_engine::BufferTable)). Foreground writes
//! land in the buffer at WAL speed; a later flush turns many buffered rows into
//! one immutable Parquet file, which is what keeps a stream of small `INSERT`s
//! from producing a directory full of tiny fragments.
//!
//! The split is engine-internal. This type owns the relation's identity — its
//! [`TableSchema`], its capabilities, and its semantic indexes — so every
//! user-facing answer (the `0A000` text for `UPDATE`, a `NOT NULL` violation,
//! `pg_class`) names the relation and its `parquet` access method, never a leaf.
//! The leaves have no `pg_class` row, no OID, and no name a user can write.
//!
//! Reads fan out: [`TableAm::storage_leaves`] hands the binder both stores and
//! the plan becomes an `Append`. That is safe without any pinned generation or
//! covered-LSN watermark because **a flush is a real transaction**: it stamps
//! the new chunk `xmin = X_f` and the copied buffer rows `xmax = X_f`, and
//! `Snapshot::in_progress` fixes a reader's verdict on `X_f` when its snapshot is
//! taken. So for one snapshot, a flushed row is either still in the buffer and
//! not yet in a chunk, or in the chunk and gone from the buffer — never both and
//! never neither, no matter when the two leaf scans open relative to the flush.

use std::sync::{Arc, RwLock};

use crabgresql_buffer_engine::BufferTable;
use crabgresql_storage_api::{
    DeleteResult, IndexMetadata, RelStats, StorageError, TableAm, TableCapabilities, TableSchema,
    Tid, Tuple, TupleStream, UpdateResult,
};
use crabgresql_txn::{Clog, LockOwner, TransactionManager, TxnContext, Xid};

use crate::{ParquetSwap, ParquetTable};

/// The chunk store and its write buffer under one relation identity.
pub struct BufferedParquetTable {
    /// The user-visible relation schema; its `access_method` is `Parquet`.
    schema: TableSchema,
    /// Leaf 0: the durable, immutable fragment store.
    chunks: Arc<ParquetTable>,
    /// Leaf 1: the WAL-logged RAM buffer.
    buffer: Arc<BufferTable>,
    /// Semantic indexes are relation-level, not leaf-level: the executor enforces
    /// uniqueness by reading `table.indexes()` and then `table.scan()`, and those
    /// two must describe the same set of rows. Holding them here is what keeps a
    /// `PRIMARY KEY` checked against buffered *and* flushed rows.
    indexes: RwLock<Vec<IndexMetadata>>,
}

impl BufferedParquetTable {
    pub fn open(chunks: ParquetTable, buffer: BufferTable, indexes: Vec<IndexMetadata>) -> Self {
        BufferedParquetTable {
            schema: chunks.schema().clone(),
            chunks: Arc::new(chunks),
            buffer: Arc::new(buffer),
            indexes: RwLock::new(indexes),
        }
    }

    /// The durable chunk store.
    pub fn chunks(&self) -> &Arc<ParquetTable> {
        &self.chunks
    }

    /// The RAM write buffer.
    pub fn buffer(&self) -> &Arc<BufferTable> {
        &self.buffer
    }

    pub fn relfilenode(&self) -> u32 {
        self.chunks.relfilenode()
    }

    pub fn add_index(&self, index: IndexMetadata) {
        self.indexes
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .push(index);
    }

    pub fn remove_index(&self, name: &str) {
        self.indexes
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .retain(|index| index.name != name);
    }

    pub fn set_analyzed(&self, relpages: u32, reltuples: f64) {
        self.chunks.set_analyzed(relpages, reltuples);
    }

    pub fn set_analyzed_by(&self, xid: Xid, relpages: u32, reltuples: f64) {
        self.chunks.set_analyzed_by(xid, relpages, reltuples);
    }

    /// Measure the **durable** half for `ANALYZE`: pages and rows from one
    /// directory listing under one shared hold.
    ///
    /// The buffer is deliberately not included. Its rows are counted live by
    /// [`TableAm::statistics`], because a buffered row can appear or vanish
    /// between two `ANALYZE`s — a flush moves it into a chunk — so freezing a
    /// count of them would be wrong in both directions.
    pub fn measure(&self, txn: &TxnContext) -> Result<(u32, f64), StorageError> {
        self.chunks.measure(txn)
    }

    /// Resolve this relation's part of a transaction ending.
    ///
    /// A returned [`ParquetSwap`] means a TRUNCATE committed and the chunk store
    /// moved to a fresh relfilenode. The buffer follows immediately, so rows
    /// written after the TRUNCATE are logged under the id the catalog now names
    /// and are found again at the next restart. Rows logged under the superseded
    /// id are then correctly unreachable — a committed TRUNCATE is the only thing
    /// that supersedes an id, and those rows did not survive it.
    pub fn finish_transaction(
        &self,
        xid: Xid,
        committed: bool,
    ) -> Result<Option<ParquetSwap>, StorageError> {
        let swap = self.chunks.finish_transaction(xid, committed)?;
        if let Some(swap) = &swap {
            self.buffer.rebind(swap.new_rel);
        }
        Ok(swap)
    }

    pub fn release_truncate_lock(&self, owner: LockOwner) {
        self.chunks.release_truncate_lock(owner);
    }

    /// Point the relation at a new relfilenode after recovery resolves a
    /// TRUNCATE. Both leaves follow: the chunk store re-derives its directory,
    /// and the buffer files future WAL records under the id the catalog now
    /// names, so the next restart looks them up where they were written.
    pub fn rebind(&self, new: u32) -> Result<(), StorageError> {
        self.chunks.rebind(new)?;
        self.buffer.rebind(new);
        Ok(())
    }

    pub fn recover(&self, clog: &Clog) -> Result<(), StorageError> {
        self.chunks.recover(clog)
    }

    /// Move every committed buffered row into one durable chunk, as an
    /// independent transaction. Returns how many rows were flushed.
    ///
    /// The transaction is what makes this safe to run while queries are reading.
    /// The new fragment is stamped `xmin = X_f` and the copied buffer rows
    /// `xmax = X_f`, and one CLOG entry decides both. A reader's verdict on `X_f`
    /// is fixed when its snapshot is taken — `Snapshot::in_progress` is `true` for
    /// any XID at or above the snapshot's `xmax` — so a snapshot either predates
    /// the flush and reads the rows from the buffer, or postdates it and reads
    /// them from the chunk. Never both, never neither, however the two leaf scans
    /// interleave with the flush.
    ///
    /// The rows keep their contents but are restamped with the flush's XID rather
    /// than their original inserters'. That is sound because the buffer copy
    /// survives until [`TableAm::vacuum`] proves no snapshot still needs it, so a
    /// reader that predates the flush never loses sight of the row.
    pub fn flush(
        &self,
        txnmgr: &TransactionManager,
        oldest: Xid,
    ) -> Result<u64, StorageError> {
        // Allocate first, then take the shared hold, then the snapshot. The hold
        // must precede the snapshot (invariant P1 on `ParquetTable::scan_in`): a
        // concurrent TRUNCATE can otherwise swap the directory out from under a
        // flush that already decided which rows to move, resurrecting them in a
        // generation that was supposed to be empty.
        let xid = txnmgr.allocate_xid();
        let _guard = self.chunks.lock.acquire_shared(LockOwner(xid.0));
        let txn = txnmgr.context(xid, crabgresql_txn::CommandId::FIRST);

        let rows = self.buffer.snapshot_rows(&txn);
        if rows.is_empty() {
            txnmgr.abort(xid);
            // Nothing to flush is still a good moment to reclaim rows an earlier
            // flush superseded, now that no snapshot can need them.
            self.buffer.vacuum(oldest, &txn.clog);
            return Ok(0);
        }
        let flushed = rows.len() as u64;
        let (tids, values): (Vec<Tid>, Vec<Tuple>) = rows.into_iter().unzip();
        let rel = self.chunks.effective_rel(xid);

        // One `insert_many` for the whole batch, so the flush produces one
        // fragment rather than one per original transaction — which is the entire
        // reason the buffer exists.
        if let Err(error) = self
            .chunks
            .insert_many(values, &txn)
            .and_then(|_| self.buffer.delete_many_in(rel, tids, &txn))
        {
            // Abort undoes both halves at once: the fragment is still `.pending`
            // and the finalize hook unlinks it, and an `xmax` belonging to an
            // aborted transaction means "not deleted".
            txnmgr.abort(xid);
            return Err(error);
        }
        txnmgr
            .commit(xid)
            .map_err(|error| StorageError::Io(error.to_string()))?;
        self.buffer.vacuum(oldest, &txn.clog);
        Ok(flushed)
    }

    pub fn drop_storage(&self) -> Result<(), StorageError> {
        self.chunks.drop_storage()
    }
}

impl TableAm for BufferedParquetTable {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    /// Identical to the chunk store's: append-only plus TRUNCATE. The buffer leaf
    /// is mutable, but it is not reachable as a write target — only
    /// `storage_leaves` produces it, and that feeds reads — so `UPDATE` and
    /// `DELETE` on a Parquet relation still raise `0A000`.
    fn capabilities(&self) -> TableCapabilities {
        TableCapabilities {
            truncate: true,
            ..TableCapabilities::APPEND_ONLY
        }
    }

    fn indexes(&self) -> Vec<IndexMetadata> {
        self.indexes
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .clone()
    }

    /// Durable chunks first, then the buffer. The order is the one a flush moves
    /// rows in, so a merged scan is also `Tid`-ascending (logical tids sort above
    /// physical ones).
    fn storage_leaves(&self) -> Option<Vec<Arc<dyn TableAm>>> {
        Some(vec![
            Arc::clone(&self.chunks) as Arc<dyn TableAm>,
            Arc::clone(&self.buffer) as Arc<dyn TableAm>,
        ])
    }

    /// The chunk half is a cached `ANALYZE` result or a size-derived estimate;
    /// the buffer half is always counted live.
    ///
    /// Keeping the buffer out of the cached half is what stops a restart from
    /// double-counting: `PgEngine` re-seeds the chunk cache from the catalog at
    /// every boot, so a persisted figure that already included buffered rows
    /// would be added to them a second time.
    fn statistics(&self) -> RelStats {
        let chunks = self.chunks.statistics();
        let buffered = self.buffer.statistics();
        RelStats {
            relpages: chunks.relpages,
            reltuples: chunks.reltuples + buffered.reltuples,
            analyzed: chunks.analyzed,
            columns: chunks.columns,
        }
    }

    fn scan(&self, txn: &TxnContext) -> TupleStream {
        // Both leaves under one `TxnContext`, hence one snapshot — the condition
        // that makes the flush transaction's stamping yield every row exactly
        // once. `Append` gives the same guarantee for the planned path.
        Box::new(self.chunks.scan(txn).chain(self.buffer.scan(txn)))
    }

    /// Route by the tid's own bit: a logical id belongs to the buffer, a physical
    /// `(block, offset)` to a fragment. No side table, and it stays correct when a
    /// row moves between them, because a flushed row keeps its logical id.
    fn fetch(&self, tid: Tid, txn: &TxnContext) -> Result<Option<Tuple>, StorageError> {
        if tid.is_logical() {
            self.buffer.fetch(tid, txn)
        } else {
            self.chunks.fetch(tid, txn)
        }
    }

    /// Foreground writes go to the buffer, never straight to a fragment.
    ///
    /// Forwarding here rather than redirecting the binder to the buffer leaf is
    /// what keeps one handle answering for the whole relation: the executor's
    /// UNIQUE pre-scan reads `indexes()` then `scan()` on this same object, so a
    /// `PRIMARY KEY` is still checked against flushed rows, and constraint errors
    /// still quote the relation's schema.
    fn insert(&self, tuple: Tuple, txn: &TxnContext) -> Result<Tid, StorageError> {
        let tids = self.insert_many(vec![tuple], txn)?;
        tids.into_iter().next().ok_or_else(|| {
            StorageError::CorruptData("buffered insert produced no tid".to_string())
        })
    }

    fn insert_many(&self, tuples: Vec<Tuple>, txn: &TxnContext) -> Result<Vec<Tid>, StorageError> {
        // The *effective* generation, not the live one: a transaction that has
        // staged a TRUNCATE reads and writes in the staged directory, so its
        // buffered rows must be logged there too. Otherwise recovery would look
        // for them under the committed id and find nothing — losing rows the
        // transaction had already acknowledged.
        self.buffer
            .append_in(self.chunks.effective_rel(txn.xid), tuples, txn)
    }

    /// Unreachable: `capabilities()` reports no UPDATE, so the binder rejects the
    /// statement with `0A000` long before a plan exists. Kept as the invariant's
    /// second line of defense rather than a silent success on the buffer half.
    fn update(
        &self,
        tid: Tid,
        tuple: Tuple,
        txn: &TxnContext,
    ) -> Result<UpdateResult, StorageError> {
        self.chunks.update(tid, tuple, txn)
    }

    fn delete(&self, tid: Tid, txn: &TxnContext) -> Result<DeleteResult, StorageError> {
        self.chunks.delete(tid, txn)
    }

    /// Empty both halves under one transaction.
    ///
    /// The chunk store goes first: it is the half that takes `AccessExclusive` and
    /// can block, and its WAL record is what recovery reconciles — so a truncate
    /// that fails to stage leaves no tombstones behind. Both halves are then
    /// stamped by the same XID, so one CLOG entry decides them together and a
    /// rollback needs no undo on either side (an `xmax` belonging to an aborted
    /// transaction means "not deleted").
    fn truncate(&self, txn: &TxnContext) -> Result<(), StorageError> {
        self.chunks.truncate(txn)?;
        let visible: Vec<Tid> = self
            .buffer
            .scan(txn)
            .filter_map(|row| row.ok().map(|(tid, _)| tid))
            .collect();
        // Tombstones belong to the generation the rows are in — the one being
        // superseded — not to the directory this TRUNCATE just staged.
        self.buffer
            .delete_many_in(self.chunks.relfilenode(), visible, txn)?;
        Ok(())
    }

    fn vacuum(&self, oldest: Xid, clog: &Clog) {
        self.buffer.vacuum(oldest, clog);
        self.chunks.vacuum(oldest, clog);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::AtomicU32;

    use crabgresql_storage_api::{Column, RelfilenodeAllocator, TableAccessMethod};
    use crabgresql_txn::{Clog, CommandId, CommitSink, TransactionManager};
    use crabgresql_types::{PgType, Value};
    use crabgresql_wal::Wal;

    use super::*;

    struct Counter(AtomicU32);

    impl RelfilenodeAllocator for Counter {
        fn alloc_relfilenode(&self) -> u32 {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        }
    }

    fn schema() -> TableSchema {
        let mut schema = TableSchema::new("p", vec![Column::new("id", PgType::Int4)]);
        schema.access_method = TableAccessMethod::Parquet;
        schema
    }

    fn open_table(root: &Path, wal: Arc<Wal>) -> Result<BufferedParquetTable, StorageError> {
        let chunks = ParquetTable::open(
            root,
            1,
            schema(),
            Vec::new(),
            Arc::clone(&wal),
            Arc::new(Counter(AtomicU32::new(1_000))),
        )?;
        let buffer = BufferTable::open(1, schema(), Vec::new(), wal).as_write_buffer_of("p");
        Ok(BufferedParquetTable::open(chunks, buffer, Vec::new()))
    }

    /// The engine's commit/abort hook, as `PgEngine` wires it: promote or unlink
    /// this transaction's pending fragments and release a TRUNCATE's hold.
    ///
    /// A flush *is* a transaction, so it relies on this hook to publish the chunk
    /// it just wrote. Wiring the real thing here rather than calling
    /// `finish_transaction` by hand keeps the test on the same path production
    /// takes — without it, a flush would leave its fragment `.pending` forever.
    struct Finalize(Arc<BufferedParquetTable>);

    impl Finalize {
        fn resolve(&self, xid: Xid, committed: bool) {
            match self.0.finish_transaction(xid, committed) {
                Ok(Some(swap)) => self.0.release_truncate_lock(swap.owner),
                Ok(None) => {}
                Err(error) => panic!("finalize failed: {error}"),
            }
        }
    }

    impl crabgresql_txn::TxnFinalize for Finalize {
        fn on_commit(&self, xid: Xid) {
            self.resolve(xid, true);
        }
        fn on_abort(&self, xid: Xid) {
            self.resolve(xid, false);
        }
    }

    /// A table plus a transaction manager already wired to its finalize hook.
    fn open_wired(dir: &Path) -> anyhow::Result<(Arc<BufferedParquetTable>, TransactionManager)> {
        let wal = Arc::new(Wal::open(dir)?);
        let table = Arc::new(open_table(dir, Arc::clone(&wal))?);
        let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
        let mut tm =
            TransactionManager::new_recovered(sink, Arc::new(Clog::new()), Xid::FIRST_NORMAL);
        tm.set_finalize(Arc::new(Finalize(Arc::clone(&table))));
        Ok((table, tm))
    }

    fn ids_of(rows: Vec<(Tid, Tuple)>) -> Vec<i32> {
        let mut ids: Vec<i32> = rows
            .into_iter()
            .map(|(_, values)| match values[0] {
                Value::Int4(id) => id,
                ref other => panic!("unexpected id value {other:?}"),
            })
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Read the relation the way a planned `Append` does — one scan per leaf,
    /// both under the same `TxnContext` — with `between` run in the window
    /// separating the two.
    ///
    /// `Append` opens its leaf scans one after another, so a flush can land
    /// between them. Running the whole flush there deterministically is a
    /// stronger test than racing threads: it reproduces the exact interleaving
    /// that a pinned-generation design exists to prevent, on every run.
    fn append_scan_with(
        table: &BufferedParquetTable,
        txn: &TxnContext,
        first_buffer: bool,
        between: impl FnOnce(),
    ) -> Vec<i32> {
        let leaves = table.storage_leaves().expect("a Parquet relation has leaves");
        let (a, b) = if first_buffer {
            (&leaves[1], &leaves[0])
        } else {
            (&leaves[0], &leaves[1])
        };
        let mut rows: Vec<(Tid, Tuple)> = a
            .scan(txn)
            .map(|row| row.expect("scan must not fail"))
            .collect();
        between();
        rows.extend(b.scan(txn).map(|row| row.expect("scan must not fail")));
        ids_of(rows)
    }

    fn seed(table: &BufferedParquetTable, tm: &TransactionManager, ids: &[i32]) {
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        table
            .insert_many(ids.iter().map(|id| vec![Value::Int4(*id)]).collect(), &txn)
            .expect("insert must succeed");
        tm.commit(xid).expect("commit must succeed");
    }

    #[test]
    fn a_flush_between_the_two_leaf_scans_yields_every_row_exactly_once()
    -> anyhow::Result<()> {
        // Both orderings: a flush landing between the leaf scans must not
        // duplicate a row (chunk read after publication, buffer read before) nor
        // drop one (the reverse).
        for first_buffer in [false, true] {
            let dir = tempfile::tempdir()?;
            let (table, tm) = open_wired(dir.path())?;
            seed(&table, &tm, &[1, 2, 3, 4, 5]);

            let reader = tm.context(tm.allocate_xid(), CommandId::FIRST);
            let ids = append_scan_with(&table, &reader, first_buffer, || {
                let flushed = table
                    .flush(&tm, tm.snapshot().xmin)
                    .expect("flush must succeed");
                assert_eq!(flushed, 5);
            });
            assert_eq!(
                ids,
                vec![1, 2, 3, 4, 5],
                "first_buffer={first_buffer}: a concurrent flush must not add or drop a row"
            );
        }
        Ok(())
    }

    #[test]
    fn a_snapshot_taken_before_a_flush_still_reads_every_row() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (table, tm) = open_wired(dir.path())?;
        seed(&table, &tm, &[1, 2, 3]);

        // A reader that started before the flush keeps reading the buffer copy,
        // which is why the flush may not reclaim it eagerly.
        let older = tm.context(tm.allocate_xid(), CommandId::FIRST);
        assert_eq!(table.flush(&tm, older.snapshot.xmin)?, 3);
        assert_eq!(
            ids_of(table.scan(&older).map(|r| r.expect("scan")).collect()),
            vec![1, 2, 3],
            "a snapshot older than the flush must still see every row exactly once"
        );

        let newer = tm.context(tm.allocate_xid(), CommandId::FIRST);
        assert_eq!(
            ids_of(table.scan(&newer).map(|r| r.expect("scan")).collect()),
            vec![1, 2, 3],
            "a snapshot newer than the flush must read them from the chunk"
        );
        Ok(())
    }

    #[test]
    fn a_flush_leaves_in_progress_rows_buffered() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (table, tm) = open_wired(dir.path())?;
        seed(&table, &tm, &[1, 2]);

        // A writer still in flight when the flush runs.
        let open_xid = tm.allocate_xid();
        let open_txn = tm.context(open_xid, CommandId::FIRST);
        table.insert_many(vec![vec![Value::Int4(9)]], &open_txn)?;

        assert_eq!(
            table.flush(&tm, tm.snapshot().xmin)?,
            2,
            "only committed rows may be flushed"
        );
        tm.commit(open_xid)?;

        let reader = tm.context(tm.allocate_xid(), CommandId::FIRST);
        assert_eq!(
            ids_of(table.scan(&reader).map(|r| r.expect("scan")).collect()),
            vec![1, 2, 9],
            "the row that was in flight during the flush must still be readable"
        );
        Ok(())
    }

    #[test]
    fn many_small_inserts_flush_into_one_fragment() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (table, tm) = open_wired(dir.path())?;
        // Ten separate transactions — the shape that used to produce ten files.
        for id in 0..10 {
            seed(&table, &tm, &[id]);
        }
        let fragments = || -> usize {
            std::fs::read_dir(dir.path().join("parquet").join("1"))
                .map(|entries| {
                    entries
                        .flatten()
                        .filter(|e| {
                            e.path().extension().and_then(|x| x.to_str()) == Some("parquet")
                        })
                        .count()
                })
                .unwrap_or(0)
        };
        assert_eq!(fragments(), 0, "foreground inserts must write no fragment");

        assert_eq!(table.flush(&tm, tm.snapshot().xmin)?, 10);
        assert_eq!(
            fragments(),
            1,
            "ten transactions' rows must consolidate into one fragment"
        );
        let reader = tm.context(tm.allocate_xid(), CommandId::FIRST);
        assert_eq!(
            ids_of(table.scan(&reader).map(|r| r.expect("scan")).collect()),
            (0..10).collect::<Vec<i32>>()
        );
        Ok(())
    }
}
