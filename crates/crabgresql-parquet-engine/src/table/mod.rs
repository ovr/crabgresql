//! The fragment store as a table access method.

mod read;
mod stats;
mod truncate;
mod write;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crabgresql_storage_api::{
    BatchStream, ColumnProjection, DeleteResult, IndexMetadata, RelStats, RelfilenodeAllocator,
    StorageError, TableAm, TableCapabilities, TableSchema, Tid, Tuple, TupleStream, UpdateResult,
};
use crabgresql_txn::{TableLock, TxnContext, Xid};
use crabgresql_wal::Wal;

use crate::error::{corrupt, io_error, unsupported};
use crate::fragment::{
    Fragment, fragments, next_block_in, parse_fragment, remove_dir_all_ok, sync_dir,
};
use crate::schema::validate_schema;
use crate::table::truncate::PendingTruncate;
use crate::wal::ParquetSwap;

pub struct ParquetTable {
    schema: Arc<TableSchema>,
    /// The data directory. The table's fragments live in `root/parquet/<rel>`,
    /// which TRUNCATE swaps — so the path is derived, never cached.
    root: PathBuf,
    /// The committed relfilenode: the directory every transaction reads, except
    /// the one holding a pending TRUNCATE (which reads `pending.new_rel`).
    live_rel: AtomicU32,
    /// A staged, not-yet-committed TRUNCATE, if any. `pending` and `has_pending`
    /// are the single source of truth for an in-flight swap and are mutated ONLY
    /// together, through this type's methods, so they never drift.
    pending: RwLock<Option<PendingTruncate>>,
    /// Cheap gate letting the read/write hot path skip the `pending` RwLock read
    /// entirely while no TRUNCATE is in flight — kept in sync with `pending`.
    has_pending: AtomicBool,
    /// A `PARQUET_TRUNCATE` record is in the log whose swap the catalog does not
    /// name yet, so replay must still be able to reach it.
    ///
    /// Deliberately NOT tied to `has_pending`, which `commit_truncate` clears
    /// before this type has even finished its own directory work — and well before
    /// the engine's `swap_relfilenode` makes the swap durable. A checkpoint
    /// sampling in that window would publish a redo point above the record, and a
    /// crash would leave the catalog naming a directory `remove_dir_all_ok` has
    /// already deleted.
    truncate_unreconciled: AtomicBool,
    /// Serializes TRUNCATE (exclusive) against readers/writers (shared).
    pub(crate) lock: Arc<TableLock>,
    wal: Arc<Wal>,
    /// The engine's relfilenode counter, used to name the directory a TRUNCATE
    /// stages. Shared with every other relation so an id can never alias one.
    relfilenodes: Arc<dyn RelfilenodeAllocator>,
    indexes: RwLock<Vec<IndexMetadata>>,
    /// The cached ANALYZE result, tagged with the transaction that measured it —
    /// [`Xid::INVALID`] for a result seeded from the catalog at startup. The tag is
    /// what lets an abort tell a measurement of storage it just destroyed (its own)
    /// from one taken before it, which stays valid.
    analyzed: RwLock<Option<(Xid, u32, f64)>>,
    next_block: Mutex<u32>,
    /// Transactions that have staged `.pending` fragments or a TRUNCATE in this
    /// table and not yet been finalized. The engine's commit/abort hook runs over
    /// every open table, so this lets [`ParquetTable::finish_transaction`] answer
    /// "nothing of mine" from memory instead of paying a directory scan and an
    /// fsync on every transaction end. Empty after a restart, which is correct:
    /// [`ParquetTable::recover`] reconciles leftover pending files directly and
    /// the WAL carries the swap.
    staged_xids: Mutex<HashSet<Xid>>,
}

impl ParquetTable {
    pub fn open(
        root: &Path,
        rel: u32,
        schema: TableSchema,
        indexes: Vec<IndexMetadata>,
        wal: Arc<Wal>,
        relfilenodes: Arc<dyn RelfilenodeAllocator>,
    ) -> Result<Self, StorageError> {
        validate_schema(&schema)?;
        let dir = root.join("parquet").join(rel.to_string());
        std::fs::create_dir_all(&dir)
            .map_err(|error| io_error("create Parquet table directory", error))?;
        Ok(Self {
            schema: Arc::new(schema),
            root: root.to_path_buf(),
            live_rel: AtomicU32::new(rel),
            pending: RwLock::new(None),
            has_pending: AtomicBool::new(false),
            truncate_unreconciled: AtomicBool::new(false),
            lock: Arc::new(TableLock::new()),
            wal,
            relfilenodes,
            indexes: RwLock::new(indexes),
            analyzed: RwLock::new(None),
            next_block: Mutex::new(next_block_in(&dir)?),
            staged_xids: Mutex::new(HashSet::new()),
        })
    }

    pub(crate) fn dir_of(&self, rel: u32) -> PathBuf {
        self.root.join("parquet").join(rel.to_string())
    }

    /// The committed relfilenode — the one the catalog names.
    pub fn relfilenode(&self) -> u32 {
        self.live_rel.load(Ordering::Relaxed)
    }

    pub(crate) fn live_dir(&self) -> PathBuf {
        self.dir_of(self.relfilenode())
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

    /// Promote (on commit) or unlink (on abort) an already-listed set of pending
    /// fragments. Does not scan the directory or fsync it — the caller owns both,
    /// so a batch of transactions costs one listing and one fsync rather than one
    /// of each per transaction.
    pub(crate) fn reconcile(
        &self,
        pending: &[Fragment],
        committed: bool,
    ) -> Result<(), StorageError> {
        for fragment in pending {
            if committed {
                let promoted = fragment
                    .promoted_path()
                    .ok_or_else(|| corrupt("pending Parquet filename is invalid"))?;
                std::fs::rename(&fragment.path, &promoted)
                    .map_err(|error| io_error("promote Parquet fragment", error))?;
            } else {
                std::fs::remove_file(&fragment.path)
                    .map_err(|error| io_error("remove aborted Parquet fragment", error))?;
            }
        }
        Ok(())
    }

    /// Promote or unlink the `.pending` fragments `xid` staged in `dir`, scanning
    /// and fsyncing that one directory. A transaction that staged nothing there
    /// costs the listing and no writes.
    pub(crate) fn reconcile_pending_in(
        &self,
        dir: &Path,
        xid: Xid,
        committed: bool,
    ) -> Result<(), StorageError> {
        let pending: Vec<Fragment> = fragments(dir)?
            .into_iter()
            .filter(|fragment| fragment.pending && fragment.xid == xid)
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        self.reconcile(&pending, committed)?;
        sync_dir(dir)
    }

    /// Reconcile everything `xid` staged in this table: its `.pending` fragments
    /// and, if it ran a TRUNCATE, the directory swap.
    ///
    /// Returns the applied swap when a committed TRUNCATE replaced the directory.
    /// The caller must then persist it with `swap_relfilenode` and release the hold
    /// with [`ParquetTable::release_truncate_lock`] — in that order, and on every
    /// path including a failed persist. Handing the hold back rather than dropping
    /// it here is what keeps a stale catalog write from clobbering a newer
    /// TRUNCATE's: a second TRUNCATE cannot even stage until the hold is released.
    ///
    /// The in-memory state transition happens before any error is returned, so an
    /// error here means "the swap took effect in memory but some file work did
    /// not" — the caller logs it, and the WAL record repairs the catalog at the
    /// next recovery. On every path that returns `Ok(None)`/`Err` the hold is
    /// already released, so no caller can leak it.
    pub fn finish_transaction(
        &self,
        xid: Xid,
        committed: bool,
    ) -> Result<Option<ParquetSwap>, StorageError> {
        // The engine's finalize hook calls this for every open Parquet table on
        // every transaction end. Tables the transaction never wrote to answer
        // from memory here, without touching the filesystem at all.
        let staged = self
            .staged_xids
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .remove(&xid);
        if !staged {
            return Ok(None);
        }
        let Some((new_rel, owner)) = self.staged_truncate(xid) else {
            let outcome = self.reconcile_pending_in(&self.live_dir(), xid, committed);
            if !committed {
                // The fragments just unlinked may have been measured: an ANALYZE run
                // inside this transaction sized its own not-yet-committed fragments
                // (the same rows it counted). Those bytes are gone now, so its cached
                // measurement describes a relation that never existed. A measurement
                // taken before this transaction is untouched — it still describes the
                // fragments that survive.
                self.forget_analyzed_by(xid);
            }
            return outcome.map(|()| None);
        };
        if !committed {
            // The TRUNCATE never happened. Its staged directory goes wholesale, so
            // the fragments inside it need no per-file pass — but fragments this
            // transaction staged *before* the TRUNCATE live in the surviving
            // directory and must be unlinked there. Nothing to persist, so the hold
            // is released here.
            self.abort_truncate(xid);
            let cleaned = self.reconcile_pending_in(&self.live_dir(), xid, false);
            let reclaimed = remove_dir_all_ok(&self.dir_of(new_rel));
            // As above: an ANALYZE run by this transaction measured the staged
            // directory, which is now gone. An older measurement described the
            // surviving directory and stays.
            self.forget_analyzed_by(xid);
            self.lock.release_exclusive(owner);
            return cleaned.and(reclaimed).map(|()| None);
        }
        // Swap first, so `live_dir()` already names the new directory and a failure
        // past this point cannot leave the table reading the directory this commit
        // is about to remove.
        let old = self.commit_truncate(xid);
        let promoted = self.reconcile_pending_in(&self.dir_of(new_rel), xid, true);
        // The old directory's rows are gone as of this commit. Failing to remove it
        // only leaks disk: it is no longer named by the catalog, so
        // `gc_orphan_parquet_dirs` reclaims it at the next boot.
        let reclaimed = old.map_or(Ok(()), |old| remove_dir_all_ok(&self.dir_of(old)));
        match promoted.and(reclaimed) {
            Ok(()) => Ok(Some(ParquetSwap { new_rel, owner })),
            Err(error) => {
                // The caller gets no swap to persist, so it cannot release for us.
                self.lock.release_exclusive(owner);
                Err(error)
            }
        }
    }

    pub fn recover(&self, clog: &crabgresql_txn::Clog) -> Result<(), StorageError> {
        let dir = self.live_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|error| io_error("create Parquet table directory", error))?;
        // Collect the whole directory before touching it: the reconciliation
        // below renames and unlinks entries, and mutating a directory while a
        // `read_dir` stream over it is still open can silently skip entries,
        // stranding another transaction's fragments as `.pending` forever.
        let entries = std::fs::read_dir(&dir)
            .map_err(|error| io_error("recover Parquet table", error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| io_error("recover Parquet entry", error))?;
        let mut pending: HashMap<Xid, Vec<Fragment>> = HashMap::new();
        let mut dirty = false;
        for entry in entries {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("tmp") {
                std::fs::remove_file(path)
                    .map_err(|error| io_error("remove temporary Parquet fragment", error))?;
                dirty = true;
                continue;
            }
            if let Some(fragment) = parse_fragment(path)?
                && fragment.pending
            {
                pending.entry(fragment.xid).or_default().push(fragment);
            }
        }
        // One reconcile pass per distinct transaction, not per file — and a
        // single directory fsync covering all of them.
        for (xid, fragments) in &pending {
            self.reconcile(fragments, clog.is_committed(*xid))?;
            dirty = true;
        }
        if dirty {
            sync_dir(&dir)?;
        }
        // A carried-over counter would be wrong if recovery promoted fragments
        // this handle never saw (it is seeded at `open`, before replay).
        *self
            .next_block
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned")) = next_block_in(&dir)?;
        Ok(())
    }

    /// Remove the table's storage: the live directory and, if a TRUNCATE is still
    /// staged, the directory it staged (the catalog never named it, so nothing else
    /// would ever reclaim it in this process).
    ///
    /// Deliberately does NOT take the table lock, matching the heap's DROP path: an
    /// exclusive acquire here would wait for an *uncommitted* TRUNCATE's hold, which
    /// is kept to that transaction's end, so a DROP could block for an unbounded
    /// time on a reactor thread with no timeout. The consequence is the same as the
    /// heap's — a concurrent scan can have its storage removed mid-iteration and
    /// report an I/O error — and closing it needs transactional DDL, not a lock here.
    pub fn drop_storage(&self) -> Result<(), StorageError> {
        let staged = self
            .pending
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .as_ref()
            .map(|p| p.new_rel);
        remove_dir_all_ok(&self.live_dir())?;
        match staged {
            Some(staged) => remove_dir_all_ok(&self.dir_of(staged)),
            None => Ok(()),
        }
    }
}

impl TableAm for ParquetTable {
    fn schema(&self) -> Arc<TableSchema> {
        Arc::clone(&self.schema)
    }

    fn capabilities(&self) -> TableCapabilities {
        // Append-only per row — fragments are immutable, so there is no UPDATE and
        // no DELETE — but the whole relation can still be replaced wholesale, which
        // is what TRUNCATE does (a fresh fragment directory swapped in on commit).
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

    fn statistics(&self) -> RelStats {
        self.stats_snapshot()
    }

    fn scan(&self, txn: &TxnContext, projection: &ColumnProjection) -> TupleStream {
        match self.scan_in(txn, projection) {
            Ok(scan) => Box::new(scan),
            Err(error) => Box::new(std::iter::once(Err(error))),
        }
    }

    fn supports_batch_scan(&self) -> bool {
        true
    }

    fn scan_batches(&self, txn: &TxnContext, projection: &ColumnProjection) -> Option<BatchStream> {
        Some(match self.batch_scan_in(txn, projection) {
            Ok(scan) => Box::new(scan),
            Err(error) => Box::new(std::iter::once(Err(error))),
        })
    }

    fn fetch(&self, tid: Tid, txn: &TxnContext) -> Result<Option<Tuple>, StorageError> {
        self.fetch_in(tid, txn)
    }

    fn insert(&self, tuple: Tuple, txn: &TxnContext) -> Result<Tid, StorageError> {
        let mut tids = self.insert_rows(vec![tuple], txn)?;
        tids.pop()
            .ok_or_else(|| corrupt("Parquet insert produced no tuple identifier"))
    }

    fn insert_many(&self, tuples: Vec<Tuple>, txn: &TxnContext) -> Result<Vec<Tid>, StorageError> {
        self.insert_rows(tuples, txn)
    }

    fn update(
        &self,
        _tid: Tid,
        _tuple: Tuple,
        _txn: &TxnContext,
    ) -> Result<UpdateResult, StorageError> {
        Err(unsupported(
            "table access method \"parquet\" does not support UPDATE",
        ))
    }

    fn delete(&self, _tid: Tid, _txn: &TxnContext) -> Result<DeleteResult, StorageError> {
        Err(unsupported(
            "table access method \"parquet\" does not support DELETE",
        ))
    }

    fn truncate(&self, txn: &TxnContext) -> Result<(), StorageError> {
        self.truncate_in(txn)
    }

    /// A staged directory swap by `xid` means this transaction's fragments land in
    /// a directory an abort removes wholesale — the discardable storage
    /// `COPY … FREEZE` needs.
    fn truncated_by(&self, xid: Xid) -> bool {
        self.staged_truncate(xid).is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crabgresql_storage_api::TableAm;
    use crabgresql_txn::{CommandId, Xid};
    use crabgresql_types::{PgType, Value};
    use crabgresql_wal::Wal;

    use crate::test_support::{finish, manager, open_table, parquet_files, schema};
    use std::fs::File;

    use crabgresql_storage_api::{ColumnProjection, Tid, Tuple};
    use crabgresql_txn::Clog;
    use crabgresql_types::numeric::Numeric;
    use crabgresql_types::{Interval, TimeTz};
    use crabgresql_wal::{RmgrRegistry, recover};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use parquet::basic::Compression;

    #[test]
    fn supported_values_round_trip_and_file_has_only_user_columns() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = schema(
            "types",
            &[
                PgType::Bool,
                PgType::Int2,
                PgType::Int4,
                PgType::Int8,
                PgType::Float4,
                PgType::Float8,
                PgType::Numeric,
                PgType::Text,
                PgType::Varchar,
                PgType::Bpchar,
                PgType::Char,
                PgType::Name,
                PgType::Bytea,
                PgType::Uuid,
                PgType::Date,
                PgType::Time,
                PgType::TimeTz,
                PgType::Timestamp,
                PgType::TimestampTz,
                PgType::Interval,
            ],
        );
        let table = open_table(dir.path(), 1, schema, Arc::clone(&wal))?;
        let row = vec![
            Value::Bool(true),
            Value::Int2(-2),
            Value::Int4(42),
            Value::Int8(9_000_000_000),
            Value::Float4(1.25),
            Value::Float8(-2.5),
            Value::Numeric(Numeric::parse("1234567890.012300")?),
            Value::Text("hello".to_string()),
            Value::Text("varchar".to_string()),
            Value::Text("bpchar".to_string()),
            // High-bit: proves the raw byte survives a real Parquet file rather
            // than being smuggled through a UTF-8 column.
            Value::Char(0xFF),
            Value::Text("name".to_string()),
            Value::Bytea(vec![0, 1, 255]),
            Value::Uuid([0x42; 16]),
            Value::Date(9_000),
            Value::Time(12_345_678),
            Value::TimeTz(TimeTz {
                usec: 45_000_000,
                zone: 3_600,
            }),
            Value::Timestamp(123_456_789),
            Value::TimestampTz(-987_654_321),
            Value::Interval(Interval {
                months: 14,
                days: -3,
                usec: 777,
            }),
        ];
        let expected_column_count = row.len();
        let nulls = vec![Value::Null; row.len()];
        let xid = tm.allocate_xid();
        table.insert_many(
            vec![row.clone(), nulls.clone()],
            &tm.context(xid, CommandId::FIRST),
        )?;

        assert_eq!(
            table
                .scan(&tm.context(xid, CommandId::FIRST), &ColumnProjection::All)
                .count(),
            0,
            "a statement cannot see its own inserts before the command counter advances"
        );
        let own_rows: Vec<Tuple> = table
            .scan(&tm.context(xid, CommandId(1)), &ColumnProjection::All)
            .map(|result| result.map(|(_, tuple)| tuple))
            .collect::<Result<_, _>>()?;
        assert_eq!(own_rows, vec![row.clone(), nulls.clone()]);
        assert_eq!(
            table
                .scan(
                    &tm.context(Xid::INVALID, CommandId::FIRST),
                    &ColumnProjection::All
                )
                .count(),
            0
        );

        tm.commit(xid)?;
        finish(&table, xid, true)?;
        let rows: Vec<Tuple> = table
            .scan(
                &tm.context(Xid::INVALID, CommandId::FIRST),
                &ColumnProjection::All,
            )
            .map(|result| result.map(|(_, tuple)| tuple))
            .collect::<Result<_, _>>()?;
        assert_eq!(rows, vec![row, nulls]);

        let files = parquet_files(dir.path(), 1)?;
        assert_eq!(files.len(), 1);
        let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(&files[0])?)?;
        assert_eq!(builder.schema().fields().len(), expected_column_count);
        assert!(
            builder
                .schema()
                .fields()
                .iter()
                .enumerate()
                .all(|(index, field)| field.name() == &format!("c{index}"))
        );
        assert_eq!(
            builder.metadata().row_group(0).column(0).compression(),
            Compression::SNAPPY
        );
        let metadata = builder
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .ok_or_else(|| anyhow::anyhow!("missing Parquet footer metadata"))?;
        assert!(metadata.iter().any(
            |item| item.key == crate::fragment::META_XMIN && item.value.as_deref() == Some("3")
        ));
        Ok(())
    }

    #[test]
    fn inserts_split_at_fragment_limit_and_tids_fetch_stably() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("many", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let xid = tm.allocate_xid();
        let tuples = (0..=u16::MAX as i32)
            .map(|value| vec![Value::Int4(value)])
            .collect();
        let tids = table.insert_many(tuples, &tm.context(xid, CommandId::FIRST))?;
        assert_eq!(tids.len(), u16::MAX as usize + 1);
        assert_eq!(tids[0], Tid::new(1, 1));
        assert_eq!(tids[u16::MAX as usize - 1], Tid::new(1, u16::MAX));
        assert_eq!(tids[u16::MAX as usize], Tid::new(2, 1));
        tm.commit(xid)?;
        finish(&table, xid, true)?;
        assert_eq!(parquet_files(dir.path(), 1)?.len(), 2);
        assert_eq!(
            table.fetch(Tid::new(2, 1), &tm.context(Xid::INVALID, CommandId::FIRST))?,
            Some(vec![Value::Int4(u16::MAX as i32)])
        );
        Ok(())
    }

    #[test]
    fn aborted_pending_fragments_are_removed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("aborted", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(xid, CommandId::FIRST))?;
        tm.abort(xid);
        finish(&table, xid, false)?;
        assert!(parquet_files(dir.path(), 1)?.is_empty());
        assert_eq!(
            table
                .scan(
                    &tm.context(Xid::INVALID, CommandId::FIRST),
                    &ColumnProjection::All
                )
                .count(),
            0
        );
        Ok(())
    }

    #[test]
    fn recovery_reconciles_pending_fragments_and_observes_xids() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let committed = open_table(
            dir.path(),
            1,
            schema("committed", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let committed_xid = tm.allocate_xid();
        committed.insert(
            vec![Value::Int4(1)],
            &tm.context(committed_xid, CommandId::FIRST),
        )?;
        tm.commit(committed_xid)?;

        let interrupted = open_table(
            dir.path(),
            2,
            schema("interrupted", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let interrupted_xid = tm.allocate_xid();
        interrupted.insert(
            vec![Value::Int4(2)],
            &tm.context(interrupted_xid, CommandId::FIRST),
        )?;
        drop(committed);
        drop(interrupted);
        drop(tm);
        drop(wal);

        let recovered_wal = Arc::new(Wal::open(dir.path())?);
        let mut registry = RmgrRegistry::new();
        registry.register(
            crate::wal::RMGR_PARQUET,
            Arc::new(crate::wal::ParquetRedo::new(dir.path())),
        );
        let clog = Arc::new(Clog::new());
        let result = recover(dir.path(), &registry, &clog, crabgresql_wal::Lsn::INVALID)?;
        assert!(result.next_xid > interrupted_xid);

        let committed = open_table(
            dir.path(),
            1,
            schema("committed", &[PgType::Int4]),
            Arc::clone(&recovered_wal),
        )?;
        committed.recover(&clog)?;
        let interrupted = open_table(
            dir.path(),
            2,
            schema("interrupted", &[PgType::Int4]),
            recovered_wal,
        )?;
        interrupted.recover(&clog)?;
        assert_eq!(parquet_files(dir.path(), 1)?.len(), 1);
        assert!(parquet_files(dir.path(), 2)?.is_empty());
        Ok(())
    }

    /// A scan lists fragments up front but opens them lazily, and a fragment
    /// becomes MVCC-visible the moment its transaction is marked committed —
    /// before the finalize hook renames `.pending` away. The reader must follow
    /// the promotion rather than failing the query with a spurious ENOENT.
    #[test]
    fn scan_follows_a_fragment_promoted_after_it_was_listed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("promoted", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(7)], &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;

        // Snapshot the fragment list (still `.pending`) before the rename lands,
        // exactly as a concurrent session's scan would.
        let scan = table.scan(
            &tm.context(Xid::INVALID, CommandId::FIRST),
            &ColumnProjection::All,
        );
        finish(&table, xid, true)?;
        let rows = scan.collect::<Result<Vec<_>, _>>()?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, vec![Value::Int4(7)]);
        Ok(())
    }

    /// `recover` must reconcile *every* pending transaction it finds, including
    /// when several interleave in the same directory. Reconciling from a live
    /// `read_dir` stream while renaming/unlinking entries could skip some.
    #[test]
    fn recover_reconciles_every_pending_transaction() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("interleaved", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        // Interleave several fragments from two transactions, leaving all of
        // them `.pending` as an interrupted run would.
        let (first, second) = (tm.allocate_xid(), tm.allocate_xid());
        for value in 0..8 {
            let xid = if value % 2 == 0 { first } else { second };
            table.insert(vec![Value::Int4(value)], &tm.context(xid, CommandId::FIRST))?;
        }
        assert!(parquet_files(dir.path(), 1)?.is_empty());

        let clog = Clog::new();
        clog.set_committed(first);
        clog.set_aborted(second);
        table.recover(&clog)?;

        // The committed transaction's four fragments were promoted; the aborted
        // transaction's four were unlinked. Neither was left half-reconciled.
        assert_eq!(parquet_files(dir.path(), 1)?.len(), 4);
        let table_dir = dir.path().join("parquet").join("1");
        let pending = std::fs::read_dir(&table_dir)?
            .filter(|entry| {
                entry
                    .as_ref()
                    .is_ok_and(|entry| entry.file_name().to_string_lossy().ends_with(".pending"))
            })
            .count();
        assert_eq!(pending, 0);
        Ok(())
    }

    /// A frozen fragment is visible as soon as it is fsynced — `header` reports
    /// `Xid::FROZEN` and `visible_fragments` ignores the `.pending` suffix — so the
    /// only thing standing between it and a dirty read is that it lands in a
    /// staged TRUNCATE directory nobody else lists. This asserts the invariant
    /// where it is relied upon, rather than trusting the server two crates away to
    /// have checked it.
    #[test]
    fn a_frozen_write_requires_this_transaction_to_have_truncated() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("frozen", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;

        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST).with_freeze();
        let error = table
            .insert(vec![Value::Int4(1)], &txn)
            .expect_err("a frozen write with no staged truncate must be refused");
        assert!(
            error.to_string().contains("has not truncated it"),
            "{error}"
        );
        // Nothing reached the directory, so no reader could have seen anything.
        assert!(parquet_files(dir.path(), 1)?.is_empty());

        // With the truncate staged by this same transaction it goes through.
        table.truncate(&txn)?;
        table.insert(vec![Value::Int4(1)], &txn)?;
        Ok(())
    }

    /// The finalize hook runs over every open Parquet table on every transaction
    /// end, so a table the transaction never wrote to must answer from memory —
    /// no directory scan, no fsync. Deleting the directory makes any filesystem
    /// access observable as an error.
    #[test]
    fn finish_transaction_skips_tables_the_xid_never_wrote() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("untouched", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        std::fs::remove_dir_all(dir.path().join("parquet").join("1"))?;
        let xid = tm.allocate_xid();
        finish(&table, xid, true)?;
        finish(&table, xid, false)?;
        Ok(())
    }
}
