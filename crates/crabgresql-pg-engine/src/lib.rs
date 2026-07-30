//! `crabgresql-pg-engine`: the durable, PostgreSQL-faithful heap engine.
//!
//! Implements the [`TableEngine`]/[`TableAm`] contract over 8 KB slotted pages
//! (`page`), a clock-sweep buffer pool (`bufpool`), on-page tuple headers with
//! genuine `ctid = (block, offset)` (`tuple`), and physiological WAL logging via
//! the core [`crabgresql_wal`] service — with redo-only crash recovery. MVCC is
//! the shared [`satisfies_mvcc`](crabgresql_txn::satisfies_mvcc) rule applied to
//! the on-page header.
//!
//! Three persistence classes share this one heap access method (see
//! [`crabgresql_storage_api::RelPersistence`]): `Permanent` (on-disk, WAL-logged),
//! `Unlogged` (on-disk but WAL-skipped, its data reset to empty on crash), and
//! `Temporary` (RAM-backed, WAL-skipped, gone on restart). Only the WAL and the
//! backing store differ; visibility is identical.
//!
//! Deliberately deferred to keep this first cut tractable (all documented in
//! `docs/ARCHITECTURE.md §3`): TOAST (a tuple must fit one page), a durable SLRU
//! CLOG and checkpoint-bounded recovery (recovery replays the whole WAL),
//! full-page writes / torn-page protection beyond page checksums, WAL segment
//! recycling, and a transactional relation catalog.

mod analyze;
mod btkey;
mod btpage;
mod btrec;
mod btredo;
mod bufpool;
mod catalog;
mod flush;
mod heap;
mod nbtree;
mod page;
mod rec;
mod redo;
mod smgr;
mod tuple;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crabgresql_buffer_engine::{BufferRedo, BufferTable, RMGR_BUFFER};
use crabgresql_parquet_engine::{
    BufferedParquetTable, ParquetRedo, ParquetTable, RMGR_PARQUET, validate_schema,
};
use crabgresql_storage_api::{
    ColumnProjection, DeleteResult, IndexMetadata, RelStats, RelationMetadata,
    RelfilenodeAllocator, SequenceAdvance, SequenceDefinition, StorageError, TableAccessMethod,
    TableAm, TableCapabilities, TableEngine, TableSchema, Tid, Tuple, TupleStream, UpdateResult,
    ViewDefinition,
};
use crabgresql_txn::{Clog, TransactionManager, TxnContext, TxnFinalize, Xid};
use crabgresql_types::Value;
use crabgresql_wal::{
    CHECKPOINT_ONLINE, CHECKPOINT_SHUTDOWN, Checkpoint, ControlFile, Lsn, RmgrId, RmgrRedo,
    RmgrRegistry, Wal, read_control, recover, write_control,
};

use crate::btredo::BtreeRedo;
use crate::nbtree::BTree;
use crate::bufpool::BufferPool;
use crate::catalog::RelCatalog;
use crate::heap::HeapTable;
use crate::redo::HeapRedo;
use crate::smgr::StorageManager;

pub use crate::flush::{BufferFlushPolicy, BufferedRelation};
pub use crate::smgr::RelFileNode;

/// A TRUNCATE swap replayed from the WAL during recovery, awaiting a verdict:
/// its swap is applied to the catalog only if `xid` committed. Collected by the
/// redo handler and drained by [`PgEngine::apply_recovered_truncates`].
pub(crate) struct RecoveredTruncate {
    pub xid: Xid,
    /// The relation's schema. Heap records carry bare names through the WAL and so
    /// are always `public`; a Parquet record carries the namespace explicitly.
    pub namespace: String,
    pub table: String,
    pub old: RelFileNode,
    pub new: RelFileNode,
    /// Whether the swapped relation is a Parquet fragment directory rather than a
    /// heap file — the physical reclaim differs (`gc_orphan_parquet_dirs` sweeps
    /// directories; `discard_relfile` only knows about `base/<n>`).
    pub parquet: bool,
}

/// Number of buffer-pool frames (8 MB). Must comfortably exceed the number of
/// pages pinned concurrently; see `bufpool` docs.
const DEFAULT_FRAMES: usize = 1024;

/// Shared engine state that both the table AM and the redo handler reach into.
pub(crate) struct EngineInner {
    pub bufpool: BufferPool,
    pub wal: Arc<Wal>,
    /// The durable relation catalog, shared so a heap table's TRUNCATE can
    /// allocate a fresh relfilenode.
    pub catalog: Arc<RelCatalog>,
    /// Uncommitted relfilenode-swap TRUNCATEs, keyed by the truncating XID:
    /// which tables it truncated, as `(namespace, name)` so temp tables (in a
    /// `pg_temp_N` namespace) resolve correctly, not just `public` ones. This is an
    /// O(1) commit-time index (review finding #10) — the alternative, scanning
    /// every table for a matching pending XID on every commit, is O(tables). The
    /// commit/abort hook drains an XID's entry to apply or discard its swaps and
    /// release the table locks.
    ///
    /// Emphatically NOT a durability signal, though it looks like one: it is
    /// drained at the *top* of the commit hook, before the swap it describes is
    /// durable, and a `wal_skipped` relation registers here with no record for
    /// replay to reach. Whether a checkpoint may bound replay is each table's own
    /// `truncate_unreconciled`.
    pub pending_truncates: Mutex<HashMap<Xid, Vec<(String, String)>>>,
    /// TRUNCATE swaps replayed from the WAL during recovery, applied after the
    /// CLOG is rebuilt (see [`PgEngine::apply_recovered_truncates`]).
    pub recovered_truncates: Mutex<Vec<RecoveredTruncate>>,
}

/// Why a checkpoint may not bound crash recovery. Each variant names state whose
/// only durable trace is a WAL record below the redo point a checkpoint would
/// otherwise publish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RedoClamp {
    /// No commit log to make durable, so no transaction's fate could be recovered.
    /// Only an engine built by hand in a test reaches this.
    NoCommitLog,
    /// A TRUNCATE whose relfilenode swap the durable catalog does not name yet.
    UnreconciledTruncate,
    /// Rows in a RAM write buffer, which exist nowhere else until a flush writes
    /// them into a fsynced fragment.
    BufferedRows,
}

/// Exposes the relation catalog's relfilenode counter to an out-of-crate access
/// method (Parquet), whose TRUNCATE stages a fresh `parquet/<n>/` directory and
/// must draw its id from the same sequence as every heap file.
struct CatalogAllocator(Arc<RelCatalog>);

impl RelfilenodeAllocator for CatalogAllocator {
    fn alloc_relfilenode(&self) -> u32 {
        self.0.alloc_relfilenode().0
    }
}

/// One open relation, dispatched by its persisted table access method.
enum ManagedTable {
    Heap(Arc<HeapTable>),
    Parquet(Arc<BufferedParquetTable>),
    Buffer(Arc<BufferTable>),
}

impl ManagedTable {
    /// The concrete access method behind this relation.
    ///
    /// Every `TableAm` method forwards through here rather than matching per
    /// method: the enum adds dispatch, never behavior, so one match is both the
    /// whole implementation and the only place a newly added method has to be
    /// taught about.
    fn as_am(&self) -> &dyn TableAm {
        match self {
            ManagedTable::Heap(table) => table.as_ref(),
            ManagedTable::Parquet(table) => table.as_ref(),
            ManagedTable::Buffer(table) => table.as_ref(),
        }
    }

    fn as_heap(&self) -> Option<&HeapTable> {
        match self {
            ManagedTable::Heap(table) => Some(table),
            _ => None,
        }
    }

    fn as_parquet(&self) -> Option<&BufferedParquetTable> {
        match self {
            ManagedTable::Parquet(table) => Some(table),
            _ => None,
        }
    }
}

impl TableAm for ManagedTable {
    fn schema(&self) -> &TableSchema {
        self.as_am().schema()
    }

    fn capabilities(&self) -> TableCapabilities {
        self.as_am().capabilities()
    }

    fn indexes(&self) -> Vec<IndexMetadata> {
        self.as_am().indexes()
    }

    fn statistics(&self) -> RelStats {
        self.as_am().statistics()
    }

    fn storage_leaves(&self) -> Option<Vec<Arc<dyn TableAm>>> {
        self.as_am().storage_leaves()
    }

    fn scan_label(&self) -> String {
        self.as_am().scan_label()
    }

    fn scan(&self, txn: &TxnContext, projection: &ColumnProjection) -> TupleStream {
        self.as_am().scan(txn, projection)
    }

    fn fetch(&self, tid: Tid, txn: &TxnContext) -> Result<Option<Tuple>, StorageError> {
        self.as_am().fetch(tid, txn)
    }

    fn supports_index_scan(&self, index_name: &str) -> bool {
        self.as_am().supports_index_scan(index_name)
    }

    fn index_lookup(
        &self,
        index_name: &str,
        key: &[Value],
        txn: &TxnContext,
    ) -> Option<Box<dyn Iterator<Item = (Tid, Tuple)> + Send>> {
        self.as_am().index_lookup(index_name, key, txn)
    }

    fn insert(&self, tuple: Tuple, txn: &TxnContext) -> Result<Tid, StorageError> {
        self.as_am().insert(tuple, txn)
    }

    fn insert_many(
        &self,
        tuples: Vec<Tuple>,
        txn: &TxnContext,
    ) -> Result<Vec<Tid>, StorageError> {
        self.as_am().insert_many(tuples, txn)
    }

    fn update(
        &self,
        tid: Tid,
        tuple: Tuple,
        txn: &TxnContext,
    ) -> Result<UpdateResult, StorageError> {
        self.as_am().update(tid, tuple, txn)
    }

    fn delete(&self, tid: Tid, txn: &TxnContext) -> Result<DeleteResult, StorageError> {
        self.as_am().delete(tid, txn)
    }

    fn update_many(
        &self,
        updates: Vec<(Tid, Tuple)>,
        txn: &TxnContext,
    ) -> Result<u64, StorageError> {
        self.as_am().update_many(updates, txn)
    }

    fn delete_many(&self, tids: Vec<Tid>, txn: &TxnContext) -> Result<u64, StorageError> {
        self.as_am().delete_many(tids, txn)
    }

    fn truncate(&self, txn: &TxnContext) -> Result<(), StorageError> {
        self.as_am().truncate(txn)
    }

    fn vacuum(&self, oldest: Xid, clog: &Clog) {
        self.as_am().vacuum(oldest, clog)
    }
}

impl EngineInner {
    /// Reclaim a relation's on-disk file: evict its buffered pages (without
    /// writing them back) then unlink the file. A missing file is not an error —
    /// recovery and the finalize hook call this on files that may already be gone.
    fn discard_relfile(&self, rel: RelFileNode) {
        self.bufpool.forget_relation(rel);
        let _ = self.bufpool.smgr().unlink(rel);
    }
}

/// The durable heap engine: a [`TableEngine`] over a data directory.
pub struct PgEngine {
    inner: Arc<EngineInner>,
    data_dir: PathBuf,
    catalog: Arc<RelCatalog>,
    /// The catalog's relfilenode counter, handed to every Parquet table so its
    /// TRUNCATE can stage a fresh directory (see [`CatalogAllocator`]).
    relfilenodes: Arc<dyn RelfilenodeAllocator>,
    /// The Parquet redo handler, kept so [`PgEngine::apply_recovered_truncates`]
    /// can drain the directory swaps replay collected.
    parquet_redo: Arc<ParquetRedo>,
    /// The buffer-table redo handler, kept so [`PgEngine::restore_buffers`] can
    /// drain the rows replay rebuilt.
    buffer_redo: Arc<BufferRedo>,
    /// Open table handles keyed by `(namespace, name)`. Unqualified tables live
    /// in `public`; a heap TRUNCATE threads a bare name through the WAL and so is
    /// only ever reconciled for `public` (a Parquet record carries its namespace).
    tables: RwLock<HashMap<(String, String), Arc<ManagedTable>>>,
    /// The `next_xid` recorded by the last checkpoint, reused as a *lower bound* for
    /// the control-file floor at a clean shutdown.
    ///
    /// It is only a lower bound, and that used to be enough because recovery
    /// recomputed the exact value by scanning every XID in the log. A bounded replay
    /// does not see the XIDs below its redo point, so the recorded floor is the only
    /// one left — `write_control_file` raises this to the allocator's live high-water
    /// mark (`Clog::next_xid_floor`) before writing it.
    last_next_xid: AtomicU64,
    /// The transaction service, attached after construction.
    ///
    /// Flushing a RAM write buffer into durable storage is a real transaction —
    /// it allocates an XID, stamps both halves with it, and commits — so the
    /// engine needs the manager. But the manager is built *from* this engine's
    /// recovered CLOG, so it cannot be a constructor argument; see
    /// [`PgEngine::attach_txn_manager`].
    ///
    /// Held **weakly**: the manager already owns this engine through its finalize
    /// hook, so a strong handle here would close a reference cycle and neither
    /// would ever be dropped — leaking the data directory's file handles and
    /// leaving the flush worker running against a directory nobody is using.
    txnmgr: std::sync::OnceLock<std::sync::Weak<TransactionManager>>,
    /// The commit log, so the checkpoint can write its dirty pages back.
    ///
    /// Held **strongly**, unlike `txnmgr`: a [`Clog`] owns nothing and so closes
    /// no cycle, and the checkpoint that matters most is the one at a clean
    /// shutdown — by which point the transaction manager may already be gone,
    /// taking the last chance to make the commit log durable with it.
    ///
    /// Absent for an engine built by hand in a test, which never checkpoints a
    /// commit log it does not have.
    clog: std::sync::OnceLock<Arc<Clog>>,
    /// Whether the last checkpoint had to record a whole-stream redo point, so the
    /// operator hears about the transition rather than every checkpoint.
    clamped: AtomicBool,
    /// The background flush thread, present once a transaction manager is
    /// attached. Tests that build an engine by hand never attach one, so they get
    /// no thread and stay deterministic.
    flush_worker: Mutex<Option<crate::flush::FlushWorker>>,
}

impl PgEngine {
    /// Open the engine over `data_dir`, registering its redo handler into
    /// `registry` (which recovery will consult) and loading the relation
    /// catalog. Call [`crabgresql_wal::recover`] afterwards to replay the WAL,
    /// then [`PgEngine::checkpoint`] to make recovered pages durable.
    pub fn new(
        data_dir: &Path,
        wal: Arc<Wal>,
        registry: &mut RmgrRegistry,
    ) -> std::io::Result<PgEngine> {
        let smgr = Arc::new(StorageManager::open(data_dir)?);
        let bufpool = BufferPool::new(DEFAULT_FRAMES, smgr, Arc::clone(&wal));
        let catalog = Arc::new(RelCatalog::load(data_dir)?);
        let inner = Arc::new(EngineInner {
            bufpool,
            wal: Arc::clone(&wal),
            catalog: Arc::clone(&catalog),
            pending_truncates: Mutex::new(HashMap::new()),
            recovered_truncates: Mutex::new(Vec::new()),
        });
        registry.register(
            RmgrId::HEAP,
            Arc::new(HeapRedo {
                engine: Arc::clone(&inner),
            }),
        );
        registry.register(
            crate::btrec::RMGR_BTREE,
            Arc::new(BtreeRedo {
                engine: Arc::clone(&inner),
            }),
        );
        let parquet_redo = Arc::new(ParquetRedo::new(data_dir));
        registry.register(RMGR_PARQUET, Arc::clone(&parquet_redo) as Arc<dyn RmgrRedo>);
        // Registered before `recover` runs, and kept so `restore_buffers` can
        // install each relation's replayed rows once the CLOG is rebuilt. A
        // buffer table holds nothing on disk, so the log is its only source.
        let buffer_redo = Arc::new(BufferRedo::new());
        registry.register(RMGR_BUFFER, Arc::clone(&buffer_redo) as Arc<dyn RmgrRedo>);
        let relfilenodes: Arc<dyn RelfilenodeAllocator> =
            Arc::new(CatalogAllocator(Arc::clone(&catalog)));

        let mut tables = HashMap::new();
        for (name, rel, schema, indexes) in catalog.schemas() {
            let namespace = schema.namespace.clone();
            let analyzed = catalog.stats_in(&namespace, &name);
            let table = match schema.access_method {
                TableAccessMethod::Heap => {
                    let table = Arc::new(HeapTable::new(
                        Arc::clone(&inner),
                        rel,
                        schema,
                        indexes,
                    ));
                    if let Some((relpages, reltuples)) = analyzed {
                        table.set_analyzed(relpages, reltuples);
                    }
                    ManagedTable::Heap(table)
                }
                TableAccessMethod::Buffer => {
                    // Opens empty: every row comes back from the WAL in
                    // `restore_buffers`, after recovery rebuilds the CLOG.
                    let indexes = indexes.into_iter().map(|(index, _)| index).collect();
                    ManagedTable::Buffer(Arc::new(BufferTable::open(
                        rel.0,
                        schema,
                        indexes,
                        Arc::clone(&wal),
                    )))
                }
                TableAccessMethod::Parquet => {
                    let indexes = indexes.into_iter().map(|(index, _)| index).collect();
                    // A relation that cannot be opened must not take the whole
                    // cluster down with it: one unparseable filename under
                    // `parquet/<rel>/` would otherwise abort startup, making every
                    // heap table unreachable and leaving no way to DROP the
                    // offender. Log it and leave the relation unregistered — it
                    // then reports 42P01 on access, and DROP TABLE still clears
                    // the catalog entry (`gc_orphan_parquet_dirs` reclaims the
                    // directory at the next boot).
                    match ParquetTable::open(
                        data_dir,
                        rel.0,
                        schema.clone(),
                        Vec::new(),
                        Arc::clone(&wal),
                        Arc::clone(&relfilenodes),
                    ) {
                        Ok(chunks) => {
                            // The buffer opens empty and is filled from the WAL by
                            // `restore_buffers`, below in the startup sequence.
                            let buffer = BufferTable::open(
                                rel.0,
                                schema.clone(),
                                Vec::new(),
                                Arc::clone(&wal),
                            )
                            .as_write_buffer_of(&schema.name);
                            let table =
                                Arc::new(BufferedParquetTable::open(chunks, buffer, indexes));
                            if let Some((relpages, reltuples)) = analyzed {
                                table.set_analyzed(relpages, reltuples);
                            }
                            ManagedTable::Parquet(table)
                        }
                        Err(error) => {
                            tracing::error!(
                                table = %name,
                                error = %error,
                                "Parquet relation could not be opened; \
                                 it will report as nonexistent until dropped"
                            );
                            continue;
                        }
                    }
                }
            };
            tables.insert((namespace, name), Arc::new(table));
        }
        Ok(PgEngine {
            inner,
            data_dir: data_dir.to_path_buf(),
            catalog,
            relfilenodes,
            parquet_redo,
            buffer_redo,
            txnmgr: std::sync::OnceLock::new(),
            clog: std::sync::OnceLock::new(),
            flush_worker: Mutex::new(None),
            tables: RwLock::new(tables),
            last_next_xid: AtomicU64::new(0),
            clamped: AtomicBool::new(false),
        })
    }

    /// The full engine-side open + crash-recovery sequence over `data_dir`,
    /// returning the engine, the rebuilt commit log, and the next XID to hand out.
    /// The single source of truth shared by the server's `open_pg_engine` and the
    /// recovery tests, so both exercise the same steps (recover → clamp a torn WAL
    /// tail → reconcile relfilenode-swap TRUNCATEs → reclaim orphans → checkpoint).
    /// The caller builds the [`crabgresql_txn::TransactionManager`] from the
    /// returned `clog`/`next_xid` and wires the [`crabgresql_txn::TxnFinalize`]
    /// hook (this engine as `Arc<dyn TxnFinalize>`).
    ///
    /// Replay resumes where the last checkpoint published, read from `pg_control`.
    pub fn open_recovered(
        data_dir: &Path,
        wal: Arc<Wal>,
    ) -> std::io::Result<(Arc<PgEngine>, Arc<Clog>, Xid)> {
        Self::open_recovered_at(data_dir, wal, None)
    }

    /// [`PgEngine::open_recovered`], but resuming at an explicit `redo` instead of
    /// the one `pg_control` names.
    ///
    /// For tests that pin a bounded replay to an LSN they sampled themselves, and
    /// for those that want a deterministic whole-stream replay ([`Lsn::INVALID`])
    /// regardless of what the last checkpoint managed to bound. Production uses
    /// [`PgEngine::open_recovered`], which is also why that one takes no LSN: a
    /// caller cannot accidentally pass a redo point the checkpoint never published.
    pub fn open_recovered_from(
        data_dir: &Path,
        wal: Arc<Wal>,
        redo: Lsn,
    ) -> std::io::Result<(Arc<PgEngine>, Arc<Clog>, Xid)> {
        Self::open_recovered_at(data_dir, wal, Some(redo))
    }

    fn open_recovered_at(
        data_dir: &Path,
        wal: Arc<Wal>,
        redo: Option<Lsn>,
    ) -> std::io::Result<(Arc<PgEngine>, Arc<Clog>, Xid)> {
        // Read the pre-recovery control file: `clean_shutdown == false` (or absent)
        // means the last run crashed, so unlogged relations must be reset. Read it
        // BEFORE the startup checkpoint below overwrites it with a running marker.
        // It also carries the redo point, so this is one read, not two.
        let control = read_control(data_dir).map_err(std::io::Error::other)?;
        let was_clean = control.map(|c| c.clean_shutdown).unwrap_or(false);
        // No control file — a fresh cluster, or one whose control file we cannot
        // vouch for — means the whole stream. That also keeps `recover`'s next-XID
        // guard satisfied by construction: a redo point can only be non-zero here if
        // it came from a control file that is readable.
        let redo = redo.unwrap_or_else(|| control.map_or(Lsn::INVALID, |c| c.redo_lsn));
        let mut registry = RmgrRegistry::new();
        let engine = Arc::new(PgEngine::new(data_dir, Arc::clone(&wal), &mut registry)?);
        // Load the durable commit log, then replay over it: the WAL is the
        // authority, so a status it carries simply overwrites what was on disk.
        // What the CLOG adds is the fates of transactions whose commit records
        // sit *below* `redo` and are therefore never replayed.
        let clog = Arc::new(Clog::open(data_dir)?);
        engine.clog.get_or_init(|| Arc::clone(&clog));
        let res = recover(data_dir, &registry, &clog, redo).map_err(std::io::Error::other)?;
        // Clamp the WAL to the last valid record before any new append, discarding
        // a torn tail left by a crash.
        if let Ok(meta) = std::fs::metadata(crabgresql_wal::wal_path(data_dir))
            && meta.len() > res.end_of_wal.0
        {
            // Dropping a torn tail is routine; dropping a large one is the visible
            // symptom of a redo point or a decode that went wrong, and it used to
            // happen in complete silence.
            tracing::warn!(
                discarded = meta.len() - res.end_of_wal.0,
                end_of_wal = %res.end_of_wal,
                replayed_from = %res.replayed_from,
                "discarding the tail of the write-ahead log"
            );
        }
        wal.reset_to(res.end_of_wal).map_err(std::io::Error::other)?;
        // Reconcile swap TRUNCATEs replayed from the WAL (apply committed, discard
        // the rest), reclaim orphaned staging files.
        engine.apply_recovered_truncates(&clog);
        engine.recover_parquet_fragments(&clog);
        // A buffer table has no file to reconcile: its rows exist only in the WAL
        // until now. Runs after the truncate resolution so a relation whose
        // storage was swapped is already on its final generation.
        engine.restore_buffers(&clog);
        engine.gc_orphan_relfiles()?;
        engine.gc_orphan_parquet_dirs()?;
        // After a crash, an unlogged relation's WAL-skipped pages may be torn — the
        // write-ahead rule never guarded them — so empty each unlogged heap and
        // re-lay its indexes as empty B-trees (PostgreSQL's ResetUnloggedRelations).
        if !was_clean {
            engine.reset_unlogged_relations()?;
        }
        // Make the recovered catalog and pages durable and mark the DB running.
        engine.checkpoint(res.next_xid)?;
        Ok((engine, clog, res.next_xid))
    }

    /// Install the buffer-table rows replay rebuilt.
    ///
    /// The whole WAL is replayed before this runs, so the CLOG is complete and
    /// each relation can keep exactly the rows a future snapshot could see. A
    /// relation the log never mentions simply stays empty.
    fn restore_buffers(&self, clog: &Clog) {
        for table in self
            .tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .values()
        {
            // Both a standalone `USING buffer` relation and the write buffer of a
            // Parquet relation are rebuilt here — they are the same component, and
            // missing the second would silently drop every committed row that had
            // not yet been flushed to a chunk.
            let buffer = match table.as_ref() {
                ManagedTable::Buffer(buffer) => buffer.as_ref(),
                ManagedTable::Parquet(parquet) => parquet.buffer().as_ref(),
                ManagedTable::Heap(_) => continue,
            };
            if let Some(restored) = self.buffer_redo.take(buffer.relfilenode()) {
                buffer.restore(restored, clog);
            }
        }
        // Anything still held belongs to a relfilenode no open relation names — a
        // superseded TRUNCATE generation or a dropped relation. Freeing it here is
        // the only chance: `take` is keyed by relfilenode and nothing else ever
        // visits these entries.
        self.buffer_redo.discard_unclaimed();
    }

    /// Flush all dirty pages to their relation files (obeying the write-ahead
    /// rule) and record a **running** (not-cleanly-shut-down) control file, so a
    /// crash after this leaves `clean_shutdown = false` and the next startup resets
    /// unlogged relations. A clean exit calls [`TableEngine::shutdown`], which marks
    /// it clean instead.
    /// Hand the engine the transaction service its storage-maintenance work
    /// needs, once.
    ///
    /// `VACUUM` on a relation with a RAM write buffer flushes it by running an
    /// independent transaction: allocate an XID, write the chunk and tombstone
    /// the buffered rows under it, commit. That needs the manager — and the
    /// manager is built from the CLOG this engine recovered, so the dependency
    /// only closes after both exist.
    ///
    /// Must be called after the manager's finalize hook is wired: a flush that
    /// committed first would never promote its own `.pending` fragment. Calling
    /// it twice is a no-op, and an engine that never gets a manager simply
    /// reports `VACUUM` as unavailable rather than misbehaving.
    pub fn attach_txn_manager(self: &Arc<Self>, txnmgr: Arc<TransactionManager>) {
        if self.txnmgr.set(Arc::downgrade(&txnmgr)).is_err() {
            return;
        }
        // Normally already set by `open_recovered`; this covers an engine wired up
        // by another route, so the checkpoint always has a commit log to flush.
        self.clog.get_or_init(|| Arc::clone(txnmgr.clog()));
        // Give every buffer the CLOG, so `statistics()` — which gets no
        // `TxnContext` — can tell a row deleted by a committed transaction from
        // one whose deleter aborted. `create_table` does the same for relations
        // created later.
        for table in self
            .tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .values()
        {
            Self::attach_clog_to(table, txnmgr.clog());
        }
        let worker = crate::flush::FlushWorker::spawn(
            Arc::downgrade(self),
            BufferFlushPolicy::from_env(),
        );
        *self
            .flush_worker
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned")) = Some(worker);
    }

    /// Hand `table`'s RAM buffer the CLOG, if it has one.
    fn attach_clog_to(table: &ManagedTable, clog: &Arc<Clog>) {
        match table {
            ManagedTable::Parquet(parquet) => parquet.buffer().attach_clog(Arc::clone(clog)),
            ManagedTable::Buffer(buffer) => buffer.attach_clog(Arc::clone(clog)),
            ManagedTable::Heap(_) => {}
        }
    }

    /// Every relation currently holding rows in a RAM write buffer.
    ///
    /// The flush worker reads this instead of keeping its own registry: the
    /// engine's table map already *is* the registry, and a second one would only
    /// be something to keep in sync.
    pub fn buffered_relations(&self) -> Vec<BufferedRelation> {
        self.tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .iter()
            .filter_map(|((namespace, name), table)| {
                // Both a Parquet relation's write buffer and a standalone
                // `USING buffer` relation belong here. The latter has nowhere to
                // flush to, but `vacuum_table` still reclaims its dead versions —
                // and without that its memory only ever grows, which is the exact
                // failure this worker exists to prevent.
                let (rel, bytes) = match table.as_ref() {
                    ManagedTable::Parquet(parquet) => {
                        (parquet.relfilenode(), parquet.buffer().resident_bytes())
                    }
                    ManagedTable::Buffer(buffer) => {
                        (buffer.relfilenode(), buffer.resident_bytes())
                    }
                    ManagedTable::Heap(_) => return None,
                };
                (bytes > 0).then(|| BufferedRelation {
                    namespace: namespace.clone(),
                    name: name.clone(),
                    rel,
                    bytes,
                })
            })
            .collect()
    }

    /// Flush one relation's buffer, choosing the reclamation horizon itself.
    /// The worker's entry point; `VACUUM` reaches the same work through
    /// [`TableEngine::vacuum_table`].
    pub fn flush_buffer(&self, namespace: &str, name: &str) -> Result<u64, StorageError> {
        let Some(txnmgr) = self.txnmgr.get().and_then(std::sync::Weak::upgrade) else {
            return Err(StorageError::UnsupportedOperation(
                "no transaction service is attached".to_string(),
            ));
        };
        self.vacuum_table(namespace, name, txnmgr.reclaim_horizon())
    }

    pub fn checkpoint(&self, next_xid: crabgresql_txn::Xid) -> std::io::Result<()> {
        self.write_control_file(next_xid, CHECKPOINT_ONLINE, false)
    }

    /// Why this checkpoint may not bound crash recovery, or `None` if it may.
    ///
    /// Every reason here is state whose only durable trace is a WAL record, so a
    /// redo point above it would skip the very record that rebuilds it. Each one
    /// currently costs a whole-stream replay rather than a clamp to the record's
    /// own LSN: tracking a real per-item LSN is the refinement
    /// (`docs/ARCHITECTURE.md`, the per-buffer min/max LSN paragraph), and it is a
    /// change to this function's body and nothing else.
    ///
    /// Must be called only *after* [`Wal::redo_point`] has returned. It takes the
    /// table map, and a checkpointer holding the WAL's delay barrier while reaching
    /// for other locks is exactly what [`crabgresql_wal::CheckpointDelay`] forbids.
    /// The ordering is also what makes the buffer check sound: a `BUFFER_INSERT`
    /// below the sample necessarily had its rows installed and counted before the
    /// sample was taken, because its writer held the barrier across both.
    fn redo_clamp(&self) -> Option<RedoClamp> {
        // No commit log to make durable means no way to record any transaction's
        // fate, so a bounded replay would come back with every one of them
        // `InProgress`. Only an engine built by hand in a test gets here.
        if self.clog.get().is_none() {
            return Some(RedoClamp::NoCommitLog);
        }
        for table in self
            .tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .values()
        {
            // Rows in a RAM write buffer exist nowhere else until a flush writes
            // them into a fsynced Parquet fragment: `restore_buffers` rebuilds them
            // from replay alone. A Parquet relation's staged TRUNCATE is the same
            // story as the heap's, tracked on the table rather than in the engine.
            let reason = match table.as_ref() {
                ManagedTable::Parquet(parquet) => {
                    if parquet.chunks().truncate_unreconciled() {
                        Some(RedoClamp::UnreconciledTruncate)
                    } else if parquet.buffer().resident_bytes() > 0 {
                        Some(RedoClamp::BufferedRows)
                    } else {
                        None
                    }
                }
                ManagedTable::Buffer(buffer) => {
                    (buffer.resident_bytes() > 0).then_some(RedoClamp::BufferedRows)
                }
                ManagedTable::Heap(heap) => heap
                    .truncate_unreconciled()
                    .then_some(RedoClamp::UnreconciledTruncate),
            };
            if let Some(reason) = reason {
                return Some(reason);
            }
        }
        None
    }

    /// The buffer pool, for white-box tests that assert checkpoint ordering.
    #[cfg(test)]
    pub(crate) fn bufpool(&self) -> &crate::bufpool::BufferPool {
        &self.inner.bufpool
    }

    /// Take a checkpoint and publish it.
    ///
    /// The statement order is the substance of this function, and each position is
    /// forced:
    ///
    /// 1. Sample the redo point **first**, and let its barrier go before touching a
    ///    page. Sampling after the flush pass would let a page dirtied *during* the
    ///    pass carry an LSN below the redo point, leaving it neither written back
    ///    nor replayed. The sample is also durable on return, which matters because
    ///    recovery hard-errors on a start past end-of-file.
    /// 2. Clamp it to whatever [`PgEngine::redo_floor`] still needs replayed —
    ///    after the sample, never while holding the barrier.
    /// 3. and 4. Pages, then the commit log: this is what turns "below the redo
    ///    point" into "on disk".
    /// 5. Append the record only now, so that a durable CHECKPOINT record implies
    ///    that checkpoint's work completed — what a future WAL recycler needs.
    /// 6. Write the control file last. Its rename is the atomic publish.
    ///
    /// Nothing can end up both un-written and un-replayed. A change whose record
    /// ends at or below the redo point had its effect published before the sample
    /// (writers whose append and publish are not atomic hold a
    /// [`crabgresql_wal::CheckpointDelay`] across that window, and the sample waits
    /// for those), so steps 3 and 4 saw it. A change above the redo point is in
    /// `[redo, EOF)` and is replayed — records never straddle the redo point,
    /// because it samples an insert position that only advances by whole records.
    /// Every crash window here can only make the next replay *longer*: a crash
    /// between 5 and 6 leaves the previous, lower redo point published, and redo is
    /// idempotent under the per-page LSN gate.
    fn write_control_file(
        &self,
        next_xid: Xid,
        info: u8,
        clean_shutdown: bool,
    ) -> std::io::Result<()> {
        let sampled = self
            .inner
            .wal
            .redo_point()
            .map_err(std::io::Error::other)?;
        // Said out loud on purpose: a silently clamped redo point looks exactly
        // like a working bounded replay from the outside, and the cost — every
        // restart re-reading the whole stream — only shows up as slow startups.
        // Logged on the transitions rather than every time, because a cluster with
        // a resident buffer table clamps at every single checkpoint and a line per
        // checkpoint would be noise nobody reads.
        let clamp = self.redo_clamp();
        let redo = match clamp {
            Some(_) => Lsn::INVALID,
            None => sampled,
        };
        match (clamp, self.clamped.swap(clamp.is_some(), Ordering::Relaxed)) {
            (Some(reason), false) => tracing::warn!(
                %sampled,
                ?reason,
                "checkpoint cannot bound crash recovery; recording a whole-stream \
                 redo point, so every restart will replay the entire WAL"
            ),
            (None, true) => tracing::info!("checkpoint can bound crash recovery again"),
            _ => {}
        }
        self.inner.bufpool.flush_all()?;
        // Make the commit log durable before the control file advertises this
        // checkpoint. The ordering is the whole point: a control file naming a
        // checkpoint whose CLOG had not reached disk would let a later bounded
        // replay start above the commit records it still needs, and every
        // transaction below that point would come back InProgress — its rows
        // silently invisible.
        if let Some(clog) = self.clog.get() {
            clog.flush()?;
        }
        // The caller's value is only a lower bound — `shutdown` reuses the last
        // checkpoint's. The commit log carries the allocator's own high-water mark,
        // and that is the one a bounded replay depends on: replay never sees the
        // XIDs below its redo point, so too low a floor here means reissuing an XID
        // already stamped on committed tuples.
        let next_xid = match self.clog.get() {
            Some(clog) => Xid(next_xid.0.max(clog.next_xid_floor().0)),
            None => next_xid,
        };
        let ckpt = Checkpoint {
            redo_lsn: redo,
            next_xid,
        };
        let end = self
            .inner
            .wal
            .append(RmgrId::CHECKPOINT, info, Xid::INVALID, &ckpt.encode())
            .end;
        self.inner
            .wal
            .flush(end)
            .map_err(std::io::Error::other)?;
        self.last_next_xid.store(next_xid.0, Ordering::Relaxed);
        write_control(
            &self.data_dir,
            &ControlFile {
                next_xid,
                redo_lsn: redo,
                clean_shutdown,
            },
        )
        .map_err(std::io::Error::other)?;
        Ok(())
    }

    /// Empty every `Unlogged` relation's on-disk files after an unclean shutdown.
    /// The heap file is truncated to zero blocks; each physical index file is
    /// truncated and re-initialized to a valid empty B-tree (an empty file would
    /// fault on the missing meta page). The heap is now empty, so no index rebuild
    /// inserts are needed. All WAL-silent — this is a startup-time bulk reset.
    pub fn reset_unlogged_relations(&self) -> std::io::Result<()> {
        let smgr = self.inner.bufpool.smgr();
        for (heap_rel, index_rels) in self.catalog.unlogged_relfilenodes() {
            self.inner.bufpool.forget_relation(heap_rel);
            smgr.truncate(heap_rel)?;
            for irel in index_rels {
                self.inner.bufpool.forget_relation(irel);
                smgr.truncate(irel)?;
                BTree::open(Arc::clone(&self.inner), irel, Arc::new(RwLock::new(())), true).create();
            }
        }
        Ok(())
    }

    /// Resolve relfilenode-swap TRUNCATEs replayed from the WAL — heap files and
    /// Parquet fragment directories alike — now that the CLOG has been rebuilt.
    /// Called once by `open_pg_engine` after `recover` and before `checkpoint`. For
    /// each truncated table, in WAL order:
    ///
    /// * every old/new relfilenode is fed to the catalog so a freshly issued id
    ///   can never alias a file or directory already on disk;
    /// * if the truncating transaction **committed**, the table is rebound to its
    ///   final new relation (persisting the catalog if it lagged) and every
    ///   superseded one is reclaimed;
    /// * otherwise the truncate never happened: the staged new relation is
    ///   discarded and the table keeps its original one (rows intact).
    ///
    /// Idempotent across repeated recoveries: a re-applied swap sees the catalog
    /// already pointing at the new relation and only re-cleans the (already gone)
    /// old one, which tolerates a missing file or directory.
    pub fn apply_recovered_truncates(&self, clog: &Clog) {
        // Parquet swaps are collected by their own redo handler; fold them in so
        // both access methods go through this one resolution.
        let mut recovered = std::mem::take(
            &mut *self
                .inner
                .recovered_truncates
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned")),
        );
        recovered.extend(self.parquet_redo.take_recovered().into_iter().map(|rt| {
            RecoveredTruncate {
                xid: rt.xid,
                namespace: rt.namespace,
                table: rt.name,
                old: RelFileNode(rt.old),
                new: RelFileNode(rt.new),
                parquet: true,
            }
        }));
        if recovered.is_empty() {
            return;
        }
        // Group by relation, preserving WAL order within each group.
        let mut order: Vec<(String, String)> = Vec::new();
        let mut by_table: HashMap<(String, String), Vec<RecoveredTruncate>> = HashMap::new();
        for rt in recovered {
            self.catalog.observe_relfilenode(rt.old);
            self.catalog.observe_relfilenode(rt.new);
            let key = (rt.namespace.clone(), rt.table.clone());
            if !by_table.contains_key(&key) {
                order.push(key.clone());
            }
            by_table.entry(key).or_default().push(rt);
        }

        for key in order {
            // Resolve each swap by ITS OWN transaction's fate, in WAL order,
            // threading the table's live relfilenode. A chain can span several
            // transactions with independent fates (each TRUNCATE holds the table
            // lock only to its own commit), so the chain must NOT be judged by a
            // single verdict: a committed swap earlier in the chain leaves its new
            // relation live even if a later (uncommitted) swap is discarded.
            let (namespace, table) = &key;
            let chain = &by_table[&key];
            let mut live = self.catalog.current_relfilenode(namespace, table);
            for rt in chain {
                // A swap only applies to the relation it was recorded against. The
                // WAL is replayed from the beginning on every boot and DDL is not
                // logged, so a record can name a relation that was since dropped and
                // re-created under the same name — applying it would repoint the new
                // relation at the old one's dead file and let the orphan sweep delete
                // the live one. Requiring `old` to be what the relation currently
                // points at also makes re-applying an already-applied swap a no-op.
                if live != Some(rt.old) {
                    continue;
                }
                // A Parquet relation's storage is a directory, not a `base/<n>`
                // file: the dead and the staged directories are both reclaimed by
                // `gc_orphan_parquet_dirs`, which runs right after this and deletes
                // every directory the catalog no longer names.
                if clog.is_committed(rt.xid) {
                    // Swap took effect: the old relation is dead, the new one live.
                    if !rt.parquet {
                        self.inner.discard_relfile(rt.old);
                    }
                    live = Some(rt.new);
                } else if !rt.parquet {
                    // Swap never committed: the staged new file is an orphan.
                    self.inner.discard_relfile(rt.new);
                }
            }
            if let Some(live) = live {
                // Persist the catalog only if it lagged the WAL, and repoint the
                // in-memory table handle at the final live relation.
                if self.catalog.current_relfilenode(namespace, table) != Some(live) {
                    // Recovery only reconciles permanent, WAL-logged tables; memory
                    // tables never reach the WAL.
                    self.catalog
                        .swap_relfilenode(namespace, table, live)
                        .unwrap_or_else(|e| panic!("relation catalog write failed: {e}"));
                }
                let handle = self
                    .tables
                    .read()
                    .unwrap_or_else(|_| panic!("rwlock poisoned"))
                    .get(&key)
                    .cloned();
                let Some(handle) = handle else {
                    continue;
                };
                // Repoint the open handle only if it is not already on `live`.
                // The WAL is replayed from the beginning on every boot, so this loop
                // revisits every TRUNCATE the relation ever ran; rebinding a handle
                // that needs nothing is not free — it resets per-relation state (a
                // Parquet table drops the ANALYZE result `PgEngine::new` just seeded
                // from the catalog), so the relation would silently lose its
                // statistics at every restart.
                let stale = match handle.as_ref() {
                    ManagedTable::Heap(t) => t.relfilenode() != live,
                    ManagedTable::Parquet(t) => t.relfilenode() != live.0,
                    // A buffer table owns no physical relation to swap: TRUNCATE
                    // is MVCC tombstones, so there is nothing to repoint.
                    ManagedTable::Buffer(_) => false,
                };
                if !stale {
                    continue;
                }
                match handle.as_ref() {
                    ManagedTable::Buffer(_) => unreachable!("a buffer table is never stale"),
                    ManagedTable::Heap(t) => t.rebind(live),
                    ManagedTable::Parquet(t) => {
                        if let Err(error) = t.rebind(live.0) {
                            // One relation we cannot repoint must not stop the
                            // cluster from booting — but it must not keep serving
                            // either: the catalog names `live`, and
                            // `gc_orphan_parquet_dirs` (next in the startup sequence)
                            // deletes every directory the catalog does not name,
                            // including the one this handle is still pointing at.
                            // Unregister it, exactly as `PgEngine::new` does for a
                            // relation it cannot open: it then reports 42P01 until
                            // the next boot rebinds it, and DROP TABLE still clears
                            // the catalog entry.
                            tracing::error!(
                                table = %table,
                                error = %error,
                                "Parquet relation could not be rebound after a \
                                 recovered TRUNCATE; it will report as nonexistent \
                                 until the next restart"
                            );
                            self.tables
                                .write()
                                .unwrap_or_else(|_| panic!("rwlock poisoned"))
                                .remove(&key);
                        }
                    }
                }
            }
        }
    }

    fn recover_parquet_fragments(&self, clog: &Clog) {
        for table in self
            .tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .values()
        {
            // As in `PgEngine::new`: one relation whose fragments cannot be
            // reconciled must not stop the cluster from starting.
            if let Some(parquet) = table.as_parquet()
                && let Err(error) = parquet.recover(clog)
            {
                tracing::error!(
                    table = %parquet.schema().name,
                    error = %error,
                    "Parquet fragment recovery failed; pending fragments remain unreconciled"
                );
            }
        }
    }

    /// Delete `parquet/<n>` directories not referenced by any live catalog
    /// relation — the Parquet counterpart of [`PgEngine::gc_orphan_relfiles`].
    /// Relfilenodes are never reused, so a directory the catalog does not name is
    /// genuinely orphaned. It reclaims three kinds of leftovers:
    ///
    /// * `drop_table` removes the catalog entry before the fragment directory, so a
    ///   crash or IO error in that window would strand the whole directory;
    /// * a TRUNCATE's staged directory whose transaction never resolved (a crash
    ///   before commit, or a failure between staging and the WAL flush);
    /// * the pre-TRUNCATE directory whose removal at commit time failed.
    ///
    /// Must run after [`PgEngine::apply_recovered_truncates`], which is what decides
    /// which side of a replayed swap the catalog names.
    pub fn gc_orphan_parquet_dirs(&self) -> std::io::Result<()> {
        let live: std::collections::HashSet<u32> =
            self.catalog.live_relfilenodes().into_iter().collect();
        let root = self.data_dir.join("parquet");
        let entries = match std::fs::read_dir(&root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            if let Some(n) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
                && !live.contains(&n)
            {
                // Best-effort, like the heap sweep: a directory we fail to remove
                // is retried at the next boot rather than blocking startup.
                if let Err(error) = std::fs::remove_dir_all(entry.path()) {
                    tracing::error!(
                        relfilenode = n,
                        error = %error,
                        "orphaned Parquet directory could not be reclaimed"
                    );
                }
            }
        }
        Ok(())
    }

    /// Delete `base/<n>` files not referenced by any live catalog relation.
    /// Reclaims staged TRUNCATE files whose transaction neither committed nor was
    /// otherwise resolved (e.g. a crash before the transaction ended). Safe only
    /// after [`PgEngine::apply_recovered_truncates`] has run — relfilenodes are
    /// never reused, so a file with no catalog entry is genuinely orphaned.
    pub fn gc_orphan_relfiles(&self) -> std::io::Result<()> {
        let live: std::collections::HashSet<u32> =
            self.catalog.live_relfilenodes().into_iter().collect();
        let base = self.data_dir.join("base");
        let entries = match std::fs::read_dir(&base) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            if let Some(n) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
                && !live.contains(&n)
            {
                self.inner.discard_relfile(RelFileNode(n));
            }
        }
        Ok(())
    }
}

impl TxnFinalize for PgEngine {
    /// Apply the transaction's committed TRUNCATE swaps: rebind each table to its
    /// new file, unlink the old file, and release the exclusive table lock. The
    /// commit's WAL record is already fsynced, so a crash before or during this
    /// is repaired by `apply_recovered_truncates` at the next boot.
    fn on_commit(&self, xid: Xid) {
        let truncate_tables = self
            .inner
            .pending_truncates
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .remove(&xid);
        let handles = self.tables.read().unwrap_or_else(|_| panic!("rwlock poisoned"));
        for (namespace, name) in truncate_tables.into_iter().flatten() {
            if let Some(t) = handles
                .get(&(namespace.clone(), name.clone()))
                .and_then(|table| table.as_heap())
            {
                // Apply the in-memory swap, then persist + reclaim, then release the
                // lock — all without panicking, so the exclusive lock is ALWAYS
                // released even if the catalog persist fails (which would otherwise
                // strand the lock and wedge the table for the process lifetime).
                // The lock is held across the persist so a concurrent TRUNCATE that
                // commits can't have its catalog write clobbered by a stale one.
                if let Some((old, owner)) = t.commit_truncate(xid) {
                    // The commit is already durable in the WAL, so a catalog persist
                    // failure is not fatal — recovery re-applies the swap from the
                    // WAL at the next boot; log and continue rather than panic. (A
                    // memory/temp table writes no WAL and its catalog row is not
                    // persisted, so the swap only updates in-memory state.)
                    if let Err(e) = self
                        .catalog
                        .swap_relfilenode(&namespace, &name, t.relfilenode())
                    {
                        tracing::error!(
                            table = %name,
                            error = %e,
                            "TRUNCATE commit: catalog persist failed; \
                             will be reconciled from the WAL at next recovery"
                        );
                        // That promise depends on replay still reaching the record,
                        // so leave the relation pinned: no checkpoint may bound
                        // replay above it until a later persist succeeds.
                    } else {
                        // Only now is the swap durable.
                        t.truncate_reconciled();
                    }
                    self.inner.discard_relfile(old);
                    t.release_truncate_lock(owner);
                }
            }
        }
        for ((namespace, name), table) in handles.iter() {
            let Some(parquet) = table.as_parquet() else {
                continue;
            };
            match parquet.finish_transaction(xid, true) {
                // A committed TRUNCATE swapped the fragment directory: persist it,
                // then release the hold — in that order, exactly as the heap arm
                // does, so a concurrent TRUNCATE (which cannot stage until the hold
                // is gone) can never have its catalog write clobbered by this one.
                // A catalog write failure is logged rather than fatal (the WAL record
                // repairs it at the next recovery), but the hold is released either
                // way or the table would be wedged for the process lifetime.
                Ok(Some(swap)) => {
                    if let Err(error) =
                        self.catalog
                            .swap_relfilenode(namespace, name, RelFileNode(swap.new_rel))
                    {
                        tracing::error!(
                            table = %name,
                            error = %error,
                            "Parquet TRUNCATE commit: catalog persist failed; \
                             will be reconciled from the WAL at next recovery"
                        );
                        // As in the heap arm: leave it pinned.
                    } else {
                        parquet.chunks().truncate_reconciled();
                    }
                    parquet.release_truncate_lock(swap.owner);
                }
                Ok(None) => {}
                Err(error) => tracing::error!(
                    table = %name,
                    error = %error,
                    "Parquet commit finalization failed; recovery will reconcile it"
                ),
            }
        }
    }

    /// Discard the transaction's staged TRUNCATE files (the table keeps its
    /// original file) and release the exclusive table lock.
    fn on_abort(&self, xid: Xid) {
        let truncate_tables = self
            .inner
            .pending_truncates
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .remove(&xid);
        let handles = self.tables.read().unwrap_or_else(|_| panic!("rwlock poisoned"));
        for (namespace, name) in truncate_tables.into_iter().flatten() {
            if let Some(t) = handles
                .get(&(namespace.clone(), name.clone()))
                .and_then(|table| table.as_heap())
                && let Some((new, owner)) = t.abort_truncate(xid)
            {
                self.inner.discard_relfile(new);
                t.release_truncate_lock(owner);
            }
        }
        for table in handles.values() {
            if let Some(parquet) = table.as_parquet()
                && let Err(error) = parquet.finish_transaction(xid, false)
            {
                tracing::error!(
                    table = %parquet.schema().name,
                    error = %error,
                    "Parquet abort cleanup failed; recovery will reconcile it"
                );
            }
        }
    }
}

impl PgEngine {
    /// Whether `name` already names a table, index, view or sequence in
    /// `namespace` — the shared PostgreSQL relation namespace, scoped per schema.
    /// The caller passes the already locked `tables` map (the create paths hold
    /// that lock across check+write), so this must not take the `tables` lock
    /// itself.
    fn relation_name_taken(
        &self,
        tables: &HashMap<(String, String), Arc<ManagedTable>>,
        namespace: &str,
        name: &str,
    ) -> bool {
        tables.contains_key(&(namespace.to_string(), name.to_string()))
            || self.catalog.contains_in(namespace, name)
            || self.catalog.contains_view_in(namespace, name)
            || self.catalog.contains_sequence_in(namespace, name)
            || tables
                .iter()
                .filter(|((ns, _), _)| ns == namespace)
                .any(|(_, t)| t.indexes().iter().any(|i| i.name == name))
    }
}

impl TableEngine for PgEngine {
    fn validate_schema(&self, schema: &TableSchema) -> Result<(), StorageError> {
        if schema.access_method.is_engine_managed() {
            validate_schema(schema)?;
        }
        Ok(())
    }

    fn create_table(&self, schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError> {
        // The server rejects these at bind time for the message; re-asserting them
        // here makes them invariants of the engine rather than of one caller.
        if schema.access_method.is_engine_managed() {
            let method = schema.access_method.as_str();
            if schema.persistence != crabgresql_storage_api::RelPersistence::Permanent {
                return Err(StorageError::UnsupportedOperation(format!(
                    "table access method \"{method}\" only supports permanent tables"
                )));
            }
            if schema.partition_scheme.is_some() || schema.partition_of.is_some() {
                return Err(StorageError::UnsupportedOperation(format!(
                    "table access method \"{method}\" does not support partitioning"
                )));
            }
            // The load-bearing half of the sort-key rule. Without it the rule is
            // an invariant of `execute_create_table` alone, and any other caller
            // could mint the keyless relation the eventual sorted flush would
            // have no order for.
            if schema.sort_key.is_empty() {
                return Err(StorageError::UnsupportedOperation(format!(
                    "table access method \"{method}\" requires a sort key"
                )));
            }
            // A key indexes into `columns`, and it outlives the statement that
            // built it: an out-of-range entry would be persisted verbatim and
            // only panic later, in whatever finally consumes the key.
            if let Some(key) = schema
                .sort_key
                .iter()
                .find(|key| key.column >= schema.columns.len())
            {
                return Err(StorageError::UnsupportedOperation(format!(
                    "sort key column {} is out of range for a {}-column relation",
                    key.column,
                    schema.columns.len()
                )));
            }
            validate_schema(&schema)?;
        } else if !schema.sort_key.is_empty() {
            // Only an engine-managed method has a layout to order. A key on a
            // heap relation would be recorded and never honored by anything.
            return Err(StorageError::UnsupportedOperation(format!(
                "table access method \"{}\" does not support ORDER BY",
                schema.access_method.as_str()
            )));
        }
        let namespace = schema.namespace.clone();
        let name = schema.name.clone();
        let mut tables = self
            .tables
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        if self.relation_name_taken(&tables, &namespace, &name) {
            return Err(StorageError::TableAlreadyExists(name));
        }
        let rel = self
            .catalog
            .create(&schema)
            .expect("relation catalog write failed");
        // A `Temporary` table is RAM-backed: register its relfilenode with the
        // storage manager so every page op routes to memory, never a file. An
        // `Unlogged` table is on-disk (WAL-skipped but file-backed), so it is not
        // registered here — it uses a real `base/<relfilenode>` file.
        if schema.access_method == TableAccessMethod::Heap && schema.persistence.is_ram_backed() {
            self.inner.bufpool.smgr().register_memory(rel);
        }
        let table = match schema.access_method {
            TableAccessMethod::Heap => ManagedTable::Heap(Arc::new(HeapTable::new(
                Arc::clone(&self.inner),
                rel,
                schema,
                Vec::new(),
            ))),
            TableAccessMethod::Buffer => ManagedTable::Buffer(Arc::new(BufferTable::open(
                rel.0,
                schema,
                Vec::new(),
                Arc::clone(&self.inner.wal),
            ))),
            TableAccessMethod::Parquet => {
                match ParquetTable::open(
                    &self.data_dir,
                    rel.0,
                    schema.clone(),
                    Vec::new(),
                    Arc::clone(&self.inner.wal),
                    Arc::clone(&self.relfilenodes),
                ) {
                    Ok(chunks) => {
                        let buffer = BufferTable::open(
                            rel.0,
                            schema.clone(),
                            Vec::new(),
                            Arc::clone(&self.inner.wal),
                        )
                        .as_write_buffer_of(&schema.name);
                        ManagedTable::Parquet(Arc::new(BufferedParquetTable::open(
                            chunks,
                            buffer,
                            Vec::new(),
                        )))
                    }
                    Err(error) => {
                        let _ = self.catalog.remove_in(&namespace, &name);
                        return Err(error);
                    }
                }
            }
        };
        let table = Arc::new(table);
        // A relation created after the transaction service was attached must get
        // the CLOG too, or `statistics()` — which has no `TxnContext` — cannot tell
        // a committed delete from a rolled-back one and reports the relation empty.
        if let Some(txnmgr) = self.txnmgr.get().and_then(std::sync::Weak::upgrade) {
            Self::attach_clog_to(&table, txnmgr.clog());
        }
        tables.insert((namespace, name), Arc::clone(&table));
        Ok(table as Arc<dyn TableAm>)
    }

    fn open_table(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        self.resolve(Some("public"), name)
    }

    fn analyze(&self, namespace: &str, name: &str, txn: &TxnContext) -> Result<(), StorageError> {
        let table = {
            let tables = self
                .tables
                .read()
                .unwrap_or_else(|_| panic!("rwlock poisoned"));
            tables
                .get(&(namespace.to_string(), name.to_string()))
                .cloned()
                .ok_or_else(|| StorageError::TableNotFound(name.to_string()))?
        };
        // Scan outside the tables lock: measuring a large relation must not block
        // concurrent DDL on unrelated tables.
        let stats = match table.as_ref() {
            ManagedTable::Heap(heap) => {
                let stats =
                    crate::analyze::analyze_heap(heap, txn, analyze::SampleTarget::default());
                heap.set_analyzed(stats.relpages, stats.reltuples);
                stats
            }
            ManagedTable::Buffer(buffer) => {
                // Rows are in memory, so the "estimate" is an exact count and
                // there is nothing to measure or cache.
                buffer.statistics()
            }
            ManagedTable::Parquet(parquet) => {
                // Size and rows come from one directory under one shared hold, so an
                // ANALYZE inside an uncommitted TRUNCATE cannot pair the staged
                // directory's row count with the old one's page count. Measuring
                // (rather than reading `statistics()`) is what keeps relpages from
                // being pinned to the first ANALYZE's value forever.
                //
                // Only the durable half is measured, cached, and persisted:
                // `statistics()` adds the buffer's live row count on top, and
                // `PgEngine::new` re-seeds this cache from the catalog at every
                // boot — so a persisted figure that already counted buffered rows
                // would be added to them a second time after a restart.
                let (relpages, reltuples) = parquet.measure(txn)?;
                // Tagged with the measuring transaction: if it rolls back, the
                // fragments it sized are unlinked and the result goes with them.
                parquet.set_analyzed_by(txn.xid, relpages, reltuples);
                RelStats {
                    relpages,
                    reltuples,
                    analyzed: true,
                    columns: Vec::new(),
                }
            }
        };
        self.catalog
            .set_stats(namespace, name, stats.relpages, stats.reltuples)
            .expect("relation catalog write failed");
        Ok(())
    }

    fn vacuum_table(
        &self,
        namespace: &str,
        name: &str,
        oldest: Xid,
    ) -> Result<u64, StorageError> {
        let Some(txnmgr) = self.txnmgr.get().and_then(std::sync::Weak::upgrade) else {
            return Err(StorageError::UnsupportedOperation(
                "VACUUM is unavailable: this engine has no transaction service".to_string(),
            ));
        };
        // Resolve and release the map lock before doing any work: a flush commits,
        // and the commit hook walks every open table, so holding a read guard
        // across it would nest two locks for no reason.
        let table = {
            let tables = self
                .tables
                .read()
                .unwrap_or_else(|_| panic!("rwlock poisoned"));
            tables
                .get(&(namespace.to_string(), name.to_string()))
                .cloned()
                .ok_or_else(|| StorageError::TableNotFound(name.to_string()))?
        };
        match table.as_ref() {
            // A relation whose rows are buffered in RAM becomes durable-as-a-file
            // here; this is the only path that turns many small writes into one
            // chunk on demand.
            ManagedTable::Parquet(parquet) => parquet.flush(&txnmgr),
            ManagedTable::Heap(heap) => {
                heap.vacuum(oldest, txnmgr.clog());
                Ok(0)
            }
            ManagedTable::Buffer(buffer) => {
                // A standalone buffer table has nowhere to flush to, so vacuuming
                // it means only reclaiming versions no snapshot can still see.
                buffer.vacuum(oldest, txnmgr.clog());
                Ok(0)
            }
        }
    }

    fn shutdown(&self) {
        // Stop the background worker first, so no flush is mid-write while the
        // control file is being marked clean.
        if let Some(worker) = self
            .flush_worker
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .take()
        {
            worker.stop_and_join();
        }
        // Flush everything and mark the control file clean, so the next startup
        // keeps unlogged relations' data. The last checkpoint's next_xid is only a
        // floor; `write_control_file` raises it to the allocator's own high-water
        // mark, which is what a bounded replay depends on.
        //
        // On failure the *previous* control file is left in place rather than a
        // patched-up one being written. That is the fail-safe direction: it names an
        // older, lower redo point, so the next replay covers a superset, and redo is
        // idempotent. Claiming a clean shutdown after a failed page flush would be
        // the opposite — it suppresses the unlogged-relation reset for a run whose
        // pages may be torn.
        let next_xid = Xid(self.last_next_xid.load(Ordering::Relaxed));
        if let Err(e) = self.write_control_file(next_xid, CHECKPOINT_SHUTDOWN, true) {
            tracing::error!(error = %e, "clean-shutdown flush failed");
        }
    }

    fn resolve(&self, schema: Option<&str>, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        let namespace = schema.unwrap_or("public");
        self.tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .get(&(namespace.to_string(), name.to_string()))
            .cloned()
            .map(|t| t as Arc<dyn TableAm>)
            .ok_or_else(|| StorageError::TableNotFound(name.to_string()))
    }

    fn drop_table(&self, namespace: &str, name: &str) -> Result<(), StorageError> {
        // Remove the durable catalog entry and the in-memory handle together
        // under the tables lock, so `open_table` never observes a half-dropped
        // relation. The persistent catalog is the source of truth for existence:
        // a missing entry there is the 42P01 case.
        let key = (namespace.to_string(), name.to_string());
        let (rel, dropped, staged) = {
            let mut tables = self
                .tables
                .write()
                .unwrap_or_else(|_| panic!("rwlock poisoned"));
            let rel = self
                .catalog
                .remove_in(namespace, name)
                .expect("relation catalog write failed");
            let Some(rel) = rel else {
                return Err(StorageError::TableNotFound(name.to_string()));
            };
            // Also reclaim any file staged by an in-flight TRUNCATE on this table
            // (its new relfilenode lives on the handle, not the catalog), so
            // dropping the table doesn't leak it. Concurrent DROP vs another
            // session's uncommitted TRUNCATE is not otherwise synchronized — full
            // serialization needs transactional DDL, which is deferred; the losing
            // side's staged file is caught here or by `gc_orphan_relfiles`.
            let dropped = tables.get(&key).cloned();
            let staged = dropped
                .as_deref()
                .and_then(ManagedTable::as_heap)
                .and_then(HeapTable::staged_relfilenode);
            tables.remove(&key);
            (rel, dropped, staged)
        };
        // Physical cleanup runs after the tables lock is released, so an IO error
        // unlinking the file panics only this statement rather than poisoning the
        // lock and disabling every other table operation. Evict the relation's
        // buffered pages first so a later checkpoint can't write them back to the
        // file we are about to unlink.
        match dropped.as_deref() {
            Some(ManagedTable::Parquet(table)) => table.drop_storage()?,
            _ => {
                self.inner.bufpool.forget_relation(rel);
                self.inner
                    .bufpool
                    .smgr()
                    .unlink(rel)
                    .expect("relation file unlink failed");
            }
        }
        if let Some(staged) = staged {
            self.inner.discard_relfile(staged);
        }
        Ok(())
    }

    fn create_index(
        &self,
        namespace: &str,
        table: &str,
        index: IndexMetadata,
    ) -> Result<(), StorageError> {
        let tables = self
            .tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        if self.relation_name_taken(&tables, namespace, &index.name) {
            return Err(StorageError::RelationAlreadyExists(index.name));
        }
        let target = tables
            .get(&(namespace.to_string(), table.to_string()))
            .ok_or_else(|| StorageError::IndexTableNotFound(table.to_string()))?;
        // Index storage follows the table's persistence:
        // * Permanent / Unlogged → a physical on-disk B-tree (its own relfilenode).
        //   An Unlogged index is WAL-silent but file-backed (`build_index` derives
        //   that from the schema) and is reset with its table on crash.
        // * Temporary → metadata-only (relfilenode 0): a RAM table's index would
        //   need RAM/WAL handling for little benefit, so uniqueness stays on the
        //   executor's visible-row scan and equality lookups fall back to a heap scan.
        let index_rel = match target.as_ref() {
            ManagedTable::Heap(heap)
                if heap.can_index(&index) && !heap.schema().persistence.is_ram_backed() =>
            {
                self.catalog.alloc_relfilenode()
            }
            _ => RelFileNode(0),
        };
        // Build the B-tree and make its WAL durable FIRST, then commit the catalog
        // record. Ordering matters for crash safety: if we persisted the catalog
        // first, a crash before the build's WAL flush would leave a durable index
        // record pointing at a B-tree that was never made durable, and the first
        // probe would fault on its missing meta page. With this order, a crash
        // before the catalog write leaves only an orphan file, which the startup
        // GC reclaims (it is not yet in the catalog's live set).
        match target.as_ref() {
            ManagedTable::Heap(heap) => heap.build_index(index.clone(), index_rel),
            ManagedTable::Parquet(parquet) => parquet.add_index(index.clone()),
            ManagedTable::Buffer(buffer) => buffer.add_index(index.clone()),
        }
        self.catalog
            .add_index_in(namespace, table, index, index_rel)
            .expect("relation catalog write failed");
        Ok(())
    }

    fn drop_index(
        &self,
        namespace: &str,
        table: &str,
        index_name: &str,
    ) -> Result<(), StorageError> {
        // Resolve the table handle, then unpublish + unlink under the table's
        // exclusive lock (via begin_index_ddl) so a concurrent VACUUM or
        // in-flight maintenance cannot still be writing the index's relfilenode
        // while we forget/unlink it. The exclusive hold is dropped at end of scope.
        let target = {
            let tables = self
                .tables
                .read()
                .unwrap_or_else(|_| panic!("rwlock poisoned"));
            tables
                .get(&(namespace.to_string(), table.to_string()))
                .cloned()
        }
        .ok_or_else(|| StorageError::IndexTableNotFound(table.to_string()))?;
        match target.as_ref() {
            ManagedTable::Heap(heap) => {
                let _guard = heap.begin_index_ddl();
                let rel = self
                    .catalog
                    .remove_index_in(namespace, table, index_name)
                    .expect("relation catalog write failed");
                heap.remove_index(index_name);
                if let Some(rel) = rel
                    && rel.0 != 0
                {
                    self.inner.discard_relfile(rel);
                }
            }
            ManagedTable::Buffer(buffer) => {
                self.catalog
                    .remove_index_in(namespace, table, index_name)
                    .expect("relation catalog write failed");
                // No physical index relation to reclaim: a buffer index is
                // metadata only, so `index_rel` was never allocated.
                buffer.remove_index(index_name);
            }
            ManagedTable::Parquet(parquet) => {
                let rel = self
                    .catalog
                    .remove_index_in(namespace, table, index_name)
                    .expect("relation catalog write failed");
                parquet.remove_index(index_name);
                if let Some(rel) = rel
                    && rel.0 != 0
                {
                    self.inner.discard_relfile(rel);
                }
            }
        }
        Ok(())
    }

    fn create_schema(&self, name: &str) -> Result<u32, StorageError> {
        self.catalog
            .create_schema(name)
            .expect("relation catalog write failed")
            .ok_or_else(|| StorageError::SchemaAlreadyExists(name.to_string()))
    }

    fn drop_schema(&self, name: &str) -> Result<(), StorageError> {
        let removed = self
            .catalog
            .remove_schema(name)
            .expect("relation catalog write failed");
        if removed {
            Ok(())
        } else {
            Err(StorageError::SchemaNotFound(name.to_string()))
        }
    }

    fn schemas(&self) -> Vec<(String, u32)> {
        self.catalog.schema_list()
    }

    fn relations(&self) -> Vec<TableSchema> {
        self.tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .values()
            .map(|t| t.schema().clone())
            .collect()
    }

    fn relation_names_in(&self, namespace: &str) -> Vec<String> {
        // Read the table map's keys under the lock and clone only the names — no
        // schema deep-clone — so a session's disconnect teardown is O(its temp
        // tables), not O(all relations).
        self.tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .keys()
            .filter(|(ns, _)| ns == namespace)
            .map(|(_, name)| name.clone())
            .collect()
    }

    fn relation_metadata(&self) -> Vec<RelationMetadata> {
        self.tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .values()
            .map(|t| RelationMetadata {
                schema: t.schema().clone(),
                indexes: t.indexes(),
                stats: t.statistics(),
            })
            .collect()
    }

    fn create_view(&self, def: ViewDefinition) -> Result<(), StorageError> {
        // A view shares the relation namespace with tables and indexes (per
        // schema). Hold the tables lock across the collision check and the
        // durable write so a concurrent CREATE TABLE of the same name can't slip
        // between them.
        let namespace = def.namespace.clone();
        let tables = self
            .tables
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        if self.relation_name_taken(&tables, &namespace, &def.name) {
            return Err(StorageError::TableAlreadyExists(def.name));
        }
        let created = self
            .catalog
            .create_view(&def)
            .expect("relation catalog write failed");
        if !created {
            return Err(StorageError::TableAlreadyExists(def.name));
        }
        Ok(())
    }

    fn resolve_view(&self, schema: Option<&str>, name: &str) -> Option<ViewDefinition> {
        self.catalog.view_in(schema.unwrap_or("public"), name)
    }

    fn drop_view(&self, namespace: &str, name: &str) -> Result<(), StorageError> {
        let removed = self
            .catalog
            .remove_view_in(namespace, name)
            .expect("relation catalog write failed");
        if removed {
            Ok(())
        } else {
            Err(StorageError::TableNotFound(name.to_string()))
        }
    }

    fn views(&self) -> Vec<ViewDefinition> {
        self.catalog.views()
    }

    fn create_sequence(&self, def: SequenceDefinition) -> Result<(), StorageError> {
        // Sequences share the relation namespace. Hold the tables lock across the
        // collision check and the durable write, matching `create_view`.
        let namespace = def.namespace.clone();
        let tables = self
            .tables
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        if self.relation_name_taken(&tables, &namespace, &def.name) {
            return Err(StorageError::TableAlreadyExists(def.name));
        }
        let created = self
            .catalog
            .create_sequence(&def)
            .expect("relation catalog write failed");
        if !created {
            return Err(StorageError::TableAlreadyExists(def.name));
        }
        Ok(())
    }

    fn drop_sequence(&self, namespace: &str, name: &str) -> Result<(), StorageError> {
        let removed = self
            .catalog
            .remove_sequence_in(namespace, name)
            .expect("relation catalog write failed");
        if removed {
            Ok(())
        } else {
            Err(StorageError::TableNotFound(name.to_string()))
        }
    }

    fn sequence(&self, namespace: &str, name: &str) -> Option<SequenceDefinition> {
        self.catalog.sequence_in(namespace, name)
    }

    fn sequences(&self) -> Vec<SequenceDefinition> {
        self.catalog.sequences()
    }

    fn sequence_nextval(&self, namespace: &str, name: &str) -> SequenceAdvance {
        self.catalog
            .advance_sequence_in(namespace, name)
            .expect("relation catalog write failed")
    }

    fn sequence_setval(
        &self,
        namespace: &str,
        name: &str,
        value: i64,
        is_called: bool,
    ) -> SequenceAdvance {
        self.catalog
            .set_sequence_in(namespace, name, value, is_called)
            .expect("relation catalog write failed")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crabgresql_storage_api::{Column, TableEngine, TableSchema};
    use crabgresql_txn::{CommandId, CommitSink, TransactionManager, TxnFinalize, Xid};
    use crabgresql_types::{PgType, Value};
    use crabgresql_wal::{Wal, read_control};

    use crate::{PgEngine, RedoClamp};

    /// One test covering three things that must agree: the control file carries a
    /// real redo point, that LSN names a record boundary, and the record there is
    /// this checkpoint's own — with a payload matching what the control file says.
    ///
    /// The boundary property is not incidental. A checkpoint samples the insert
    /// position and then appends its record, so on a quiet system the record starts
    /// exactly at the redo point; that is what makes the published LSN safe to feed
    /// to `recover`, which rejects a start that is not a record boundary.
    ///
    /// It also catches the failure mode that would otherwise be silent: get the
    /// control file's CRC range wrong and every read returns `None`, which degrades
    /// to a whole-stream replay — correct, so nothing else would notice.
    #[test]
    fn a_checkpoint_publishes_a_redo_point_naming_its_own_record() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let (engine, clog, next_xid) =
            PgEngine::open_recovered(dir.path(), Arc::clone(&wal))?;
        let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
        let mut tm = TransactionManager::new_recovered(sink, clog, next_xid);
        tm.set_finalize(Arc::clone(&engine) as Arc<dyn TxnFinalize>);

        let table = engine.create_table(TableSchema::new(
            "t",
            vec![Column::new("id", PgType::Int4)],
        ))?;
        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;

        engine.checkpoint(tm.snapshot().xmax)?;

        let control = read_control(dir.path())?.expect("a checkpoint publishes a control file");
        assert!(
            control.redo_lsn.is_valid(),
            "a heap-only cluster has nothing to clamp for, so replay must be bounded"
        );
        let bytes = std::fs::read(crabgresql_wal::wal_path(dir.path()))?;
        assert!(
            control.redo_lsn.0 < bytes.len() as u64,
            "the redo point must be backed by bytes on disk"
        );
        let (rec, _) = crabgresql_wal::WalRecord::decode(&bytes[control.redo_lsn.0 as usize..])
            .ok_or_else(|| anyhow::anyhow!("no record decodes at the published redo point"))?;
        assert_eq!(rec.rmgr, crabgresql_wal::RmgrId::CHECKPOINT.0);
        assert_eq!(rec.info, crabgresql_wal::CHECKPOINT_ONLINE);
        assert_eq!(rec.xid, Xid::INVALID, "a checkpoint owns no transaction");
        let ckpt = crabgresql_wal::Checkpoint::decode(rec.payload)
            .ok_or_else(|| anyhow::anyhow!("the checkpoint payload did not decode"))?;
        assert_eq!(ckpt.redo_lsn, control.redo_lsn);
        assert_eq!(ckpt.next_xid, control.next_xid);

        Ok(())
    }

    fn wired(
        dir: &std::path::Path,
    ) -> anyhow::Result<(Arc<PgEngine>, TransactionManager, Arc<Wal>)> {
        let wal = Arc::new(Wal::open(dir)?);
        let (engine, clog, next_xid) = PgEngine::open_recovered(dir, Arc::clone(&wal))?;
        let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
        let mut tm = TransactionManager::new_recovered(sink, clog, next_xid);
        tm.set_finalize(Arc::clone(&engine) as Arc<dyn TxnFinalize>);
        Ok((engine, tm, wal))
    }

    fn one_column(name: &str) -> TableSchema {
        TableSchema::new(name, vec![Column::new("id", PgType::Int4)])
    }

    /// A committed TRUNCATE is repaired from its WAL record when the catalog write
    /// has not landed, so replay must still be able to reach that record. The pin
    /// therefore has to outlive `commit_truncate`, which only applies the swap in
    /// memory — clearing it there would drop the pin exactly while the swap is
    /// least durable.
    #[test]
    fn the_truncate_pin_holds_until_the_catalog_names_the_swap() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // No finalize hook: `commit` makes the record and the CLOG bit durable but
        // never runs the swap, so this test drives the hook's steps by hand and can
        // look between them.
        let wal = Arc::new(Wal::open(dir.path())?);
        let (engine, clog, next_xid) = PgEngine::open_recovered(dir.path(), Arc::clone(&wal))?;
        let tm = TransactionManager::new_recovered(
            Arc::clone(&wal) as Arc<dyn CommitSink>,
            clog,
            next_xid,
        );
        let table = engine.create_table(one_column("t"))?;
        let xid = tm.allocate_xid();
        table.truncate(&tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;

        assert_eq!(
            engine.redo_clamp(),
            Some(RedoClamp::UnreconciledTruncate),
            "a staged TRUNCATE must pin replay"
        );

        let swapped_to = {
            let tables = engine
                .tables
                .read()
                .unwrap_or_else(|_| panic!("rwlock poisoned"));
            let heap = tables
                .get(&("public".to_string(), "t".to_string()))
                .and_then(|t| t.as_heap())
                .ok_or_else(|| anyhow::anyhow!("t is not a heap table"))?;
            heap.commit_truncate(xid)
                .ok_or_else(|| anyhow::anyhow!("nothing was staged"))?;
            heap.relfilenode()
        };
        assert_eq!(
            engine.redo_clamp(),
            Some(RedoClamp::UnreconciledTruncate),
            "the swap is still only in memory: replay must stay unbounded"
        );

        engine.catalog.swap_relfilenode("public", "t", swapped_to)?;
        {
            let tables = engine
                .tables
                .read()
                .unwrap_or_else(|_| panic!("rwlock poisoned"));
            let heap = tables
                .get(&("public".to_string(), "t".to_string()))
                .and_then(|t| t.as_heap())
                .ok_or_else(|| anyhow::anyhow!("t is not a heap table"))?;
            heap.truncate_reconciled();
        }
        assert_eq!(
            engine.redo_clamp(),
            None,
            "once the catalog names the swap, replay may be bounded again"
        );

        Ok(())
    }

    /// An `UNLOGGED` TRUNCATE writes no record, so there is nothing for replay to
    /// reach and pinning the redo point for it would clamp the whole cluster to a
    /// whole-stream replay for free.
    #[test]
    fn a_wal_skipped_truncate_does_not_clamp_the_cluster() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (engine, tm, _wal) = wired(dir.path())?;
        let mut unlogged = one_column("u");
        unlogged.persistence = crabgresql_storage_api::RelPersistence::Unlogged;
        let unlogged = engine.create_table(unlogged)?;
        let logged = engine.create_table(one_column("t"))?;

        let xid = tm.allocate_xid();
        unlogged.truncate(&tm.context(xid, CommandId::FIRST))?;
        assert_eq!(
            engine.redo_clamp(),
            None,
            "an unlogged TRUNCATE has no record to replay, so it must not clamp"
        );

        // The positive control: a logged one does clamp, so the assertion above
        // cannot be passing merely because the clamp is broken outright.
        logged.truncate(&tm.context(xid, CommandId::FIRST))?;
        assert_eq!(
            engine.redo_clamp(),
            Some(RedoClamp::UnreconciledTruncate)
        );

        Ok(())
    }

    /// Every table a transaction truncated stays pinned until its *own* catalog
    /// write lands. The pin used to be taken per table inside the commit hook,
    /// after one drain had already cleared the state for all of them, so the tables
    /// later in the list were covered by nothing.
    #[test]
    fn every_table_in_a_multi_table_truncate_stays_pinned() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (engine, tm, _wal) = wired(dir.path())?;
        let a = engine.create_table(one_column("a"))?;
        let b = engine.create_table(one_column("b"))?;

        let xid = tm.allocate_xid();
        a.truncate(&tm.context(xid, CommandId::FIRST))?;
        b.truncate(&tm.context(xid, CommandId::FIRST))?;
        assert_eq!(
            engine.redo_clamp(),
            Some(RedoClamp::UnreconciledTruncate)
        );

        // Reconciling one of them must not lift the clamp for the other.
        let tables = engine
            .tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let heap_a = tables
            .get(&("public".to_string(), "a".to_string()))
            .and_then(|t| t.as_heap())
            .ok_or_else(|| anyhow::anyhow!("a is not a heap table"))?;
        heap_a.truncate_reconciled();
        drop(tables);
        assert_eq!(
            engine.redo_clamp(),
            Some(RedoClamp::UnreconciledTruncate),
            "b is still unreconciled, so replay must stay unbounded"
        );

        Ok(())
    }

    /// The floor recorded for the XID allocator must be the *live* one. A stale
    /// floor is invisible while replay covers the whole stream — it rederives the
    /// value from the log — but once replay is bounded it is the only floor left,
    /// and too low a one reissues an XID already stamped on committed tuples.
    #[test]
    fn a_checkpoint_records_the_live_xid_floor() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let (engine, clog, next_xid) =
            PgEngine::open_recovered(dir.path(), Arc::clone(&wal))?;
        let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
        let mut tm = TransactionManager::new_recovered(sink, clog, next_xid);
        tm.set_finalize(Arc::clone(&engine) as Arc<dyn TxnFinalize>);

        // Startup already checkpointed. Burn XIDs *without* checkpointing again, so
        // the only way the floor below can be right is by reading the allocator
        // rather than replaying the argument the last checkpoint was given.
        let mut highest = Xid::INVALID;
        for _ in 0..5 {
            highest = tm.allocate_xid();
        }

        // `shutdown` deliberately passes the *last* checkpoint's floor, which is the
        // stale value; the checkpoint has to raise it for itself.
        TableEngine::shutdown(engine.as_ref());
        let control = read_control(dir.path())?.expect("shutdown publishes a control file");
        assert!(
            control.next_xid.0 > highest.0,
            "recorded floor {} must sit above every XID allocated ({highest:?})",
            control.next_xid.0
        );

        Ok(())
    }
}

#[cfg(feature = "test-support")]
mod test_support {
    use std::sync::Arc;

    use crabgresql_storage_api::TableEngine;
    use crabgresql_txn::{CommitSink, TransactionManager, TxnFinalize};
    use crabgresql_wal::Wal;
    use tempfile::TempDir;

    use crate::PgEngine;

    /// A fully-wired durable [`PgEngine`] over a throwaway temp directory, plus its
    /// WAL-backed [`TransactionManager`]. The directory is deleted when this is
    /// dropped, so each test that builds one is isolated. Replaces the old
    /// in-memory reference engine in downstream tests.
    pub struct Ephemeral {
        engine: Arc<PgEngine>,
        txnmgr: Arc<TransactionManager>,
        _dir: TempDir,
    }

    impl Ephemeral {
        /// The engine as a trait object, for tests that only need a `TableEngine`.
        pub fn engine(&self) -> Arc<dyn TableEngine> {
            Arc::clone(&self.engine) as Arc<dyn TableEngine>
        }

        /// The concrete engine handle (for engine-specific calls like `checkpoint`).
        pub fn pg_engine(&self) -> Arc<PgEngine> {
            Arc::clone(&self.engine)
        }

        /// The WAL-backed transaction manager wired to this engine's finalize hook.
        pub fn txnmgr(&self) -> Arc<TransactionManager> {
            Arc::clone(&self.txnmgr)
        }
    }

    /// A fresh engine over a new temp directory, returned as a bare handle. The
    /// directory is **leaked** (never cleaned) so the engine keeps working for the
    /// whole test process without a guard to hold — convenient for the many
    /// metadata/execution tests in downstream crates that build their own
    /// transaction manager. Each call gets its own directory, so runs stay
    /// isolated. Use [`ephemeral`] instead when you want the directory reclaimed.
    pub fn ephemeral_engine() -> Arc<PgEngine> {
        let dir = TempDir::new().expect("create temp data dir");
        let wal = Arc::new(Wal::open(dir.path()).expect("open wal"));
        let (engine, _clog, _next_xid) =
            PgEngine::open_recovered_from(dir.path(), wal, crabgresql_wal::Lsn::INVALID)
                .expect("open engine");
        // Keep the data directory alive for the process lifetime; the OS reclaims
        // it when the (short-lived) test process exits.
        std::mem::forget(dir);
        engine
    }

    /// Open a fresh durable engine over a new temp directory, mirroring the
    /// server's `open_pg_engine` wiring (WAL + recovery + finalize hook).
    pub fn ephemeral() -> Ephemeral {
        let dir = TempDir::new().expect("create temp data dir");
        let wal = Arc::new(Wal::open(dir.path()).expect("open wal"));
        let (engine, clog, next_xid) =
            PgEngine::open_recovered_from(
            dir.path(),
            Arc::clone(&wal),
            crabgresql_wal::Lsn::INVALID,
        )
                .expect("open engine");
        let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
        let mut txnmgr = TransactionManager::new_recovered(sink, clog, next_xid);
        txnmgr.set_finalize(Arc::clone(&engine) as Arc<dyn TxnFinalize>);
        Ephemeral {
            engine,
            txnmgr: Arc::new(txnmgr),
            _dir: dir,
        }
    }
}

#[cfg(feature = "test-support")]
pub use test_support::{Ephemeral, ephemeral, ephemeral_engine};
