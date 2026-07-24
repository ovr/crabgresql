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

mod btkey;
mod btpage;
mod btrec;
mod btredo;
mod bufpool;
mod catalog;
mod datum;
mod heap;
mod lock;
mod nbtree;
mod page;
mod rec;
mod redo;
mod smgr;
mod tuple;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crabgresql_storage_api::{
    IndexMetadata, RelationMetadata, SequenceAdvance, SequenceDefinition, StorageError, TableAm,
    TableEngine, TableSchema, ViewDefinition,
};
use crabgresql_txn::{Clog, TxnFinalize, Xid};
use crabgresql_wal::{ControlFile, RmgrId, RmgrRegistry, Wal, read_control, recover, write_control};

use crate::btredo::BtreeRedo;
use crate::nbtree::BTree;
use crate::bufpool::BufferPool;
use crate::catalog::RelCatalog;
use crate::heap::HeapTable;
use crate::redo::HeapRedo;
use crate::smgr::StorageManager;

pub use crate::smgr::RelFileNode;

/// A TRUNCATE swap replayed from the WAL during recovery, awaiting a verdict:
/// its swap is applied to the catalog only if `xid` committed. Collected by the
/// redo handler and drained by [`PgEngine::apply_recovered_truncates`].
pub(crate) struct RecoveredTruncate {
    pub xid: Xid,
    pub table: String,
    pub old: RelFileNode,
    pub new: RelFileNode,
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
    pub pending_truncates: Mutex<HashMap<Xid, Vec<(String, String)>>>,
    /// TRUNCATE swaps replayed from the WAL during recovery, applied after the
    /// CLOG is rebuilt (see [`PgEngine::apply_recovered_truncates`]).
    pub recovered_truncates: Mutex<Vec<RecoveredTruncate>>,
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
    /// Open table handles keyed by `(namespace, name)`. Unqualified tables live
    /// in `public`; the relfilenode-swap TRUNCATE machinery only ever touches
    /// `public` tables (it threads bare names through the WAL).
    tables: RwLock<HashMap<(String, String), Arc<HeapTable>>>,
    /// The `next_xid` recorded by the last checkpoint, reused as the control-file
    /// floor at a clean shutdown (recovery recomputes the exact value from the WAL,
    /// so this only needs to be a valid lower bound).
    last_next_xid: AtomicU64,
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
            wal,
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

        let mut tables = HashMap::new();
        for (name, rel, schema, indexes) in catalog.schemas() {
            let namespace = schema.namespace.clone();
            tables.insert(
                (namespace, name),
                Arc::new(HeapTable::new(Arc::clone(&inner), rel, schema, indexes)),
            );
        }
        Ok(PgEngine {
            inner,
            data_dir: data_dir.to_path_buf(),
            catalog,
            tables: RwLock::new(tables),
            last_next_xid: AtomicU64::new(0),
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
    pub fn open_recovered(
        data_dir: &Path,
        wal: Arc<Wal>,
    ) -> std::io::Result<(Arc<PgEngine>, Arc<Clog>, Xid)> {
        // Read the pre-recovery control file: `clean_shutdown == false` (or absent)
        // means the last run crashed, so unlogged relations must be reset. Read it
        // BEFORE the startup checkpoint below overwrites it with a running marker.
        let was_clean = read_control(data_dir)
            .map_err(std::io::Error::other)?
            .map(|c| c.clean_shutdown)
            .unwrap_or(false);
        let mut registry = RmgrRegistry::new();
        let engine = Arc::new(PgEngine::new(data_dir, Arc::clone(&wal), &mut registry)?);
        let clog = Arc::new(Clog::new());
        let res = recover(data_dir, &registry, &clog).map_err(std::io::Error::other)?;
        // Clamp the WAL to the last valid record before any new append, discarding
        // a torn tail left by a crash.
        wal.reset_to(res.end_of_wal).map_err(std::io::Error::other)?;
        // Reconcile swap TRUNCATEs replayed from the WAL (apply committed, discard
        // the rest), reclaim orphaned staging files.
        engine.apply_recovered_truncates(&clog);
        engine.gc_orphan_relfiles()?;
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

    /// Flush all dirty pages to their relation files (obeying the write-ahead
    /// rule) and record a **running** (not-cleanly-shut-down) control file, so a
    /// crash after this leaves `clean_shutdown = false` and the next startup resets
    /// unlogged relations. A clean exit calls [`TableEngine::shutdown`], which marks
    /// it clean instead.
    pub fn checkpoint(&self, next_xid: crabgresql_txn::Xid) -> std::io::Result<()> {
        self.write_control_file(next_xid, false)
    }

    fn write_control_file(&self, next_xid: Xid, clean_shutdown: bool) -> std::io::Result<()> {
        self.inner.bufpool.flush_all()?;
        self.last_next_xid.store(next_xid.0, Ordering::Relaxed);
        write_control(
            &self.data_dir,
            &ControlFile {
                next_xid,
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

    /// Resolve relfilenode-swap TRUNCATEs replayed from the WAL, now that the
    /// CLOG has been rebuilt. Called once by `open_pg_engine` after `recover` and
    /// before `checkpoint`. For each truncated table, in WAL order:
    ///
    /// * every old/new relfilenode is fed to the catalog so a freshly issued id
    ///   can never alias a file already on disk;
    /// * if the truncating transaction **committed**, the table is rebound to its
    ///   final new file (persisting the catalog if it lagged) and every superseded
    ///   file is unlinked;
    /// * otherwise the truncate never happened: the staged new file(s) are
    ///   discarded and the table keeps its original file (rows intact).
    ///
    /// Idempotent across repeated recoveries: a re-applied swap sees the catalog
    /// already pointing at the new file and only re-cleans the (already gone) old
    /// file, which tolerates a missing file.
    pub fn apply_recovered_truncates(&self, clog: &Clog) {
        let recovered = std::mem::take(
            &mut *self
                .inner
                .recovered_truncates
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned")),
        );
        if recovered.is_empty() {
            return;
        }
        // Group by table, preserving WAL order within each group.
        let mut order: Vec<String> = Vec::new();
        let mut by_table: HashMap<String, Vec<RecoveredTruncate>> = HashMap::new();
        for rt in recovered {
            self.catalog.observe_relfilenode(rt.old);
            self.catalog.observe_relfilenode(rt.new);
            if !by_table.contains_key(&rt.table) {
                order.push(rt.table.clone());
            }
            by_table.entry(rt.table.clone()).or_default().push(rt);
        }

        for table in order {
            // Resolve each swap by ITS OWN transaction's fate, in WAL order,
            // threading the table's live relfilenode. A chain can span several
            // transactions with independent fates (each TRUNCATE holds the table
            // lock only to its own commit), so the chain must NOT be judged by a
            // single verdict: a committed swap earlier in the chain leaves its new
            // file live even if a later (uncommitted) swap is discarded.
            let chain = &by_table[&table];
            let mut live = self.catalog.current_relfilenode(&table);
            for rt in chain {
                if clog.is_committed(rt.xid) {
                    // Swap took effect: the old file is dead, the new one is live.
                    self.inner.discard_relfile(rt.old);
                    live = Some(rt.new);
                } else {
                    // Swap never committed: the staged new file is an orphan.
                    self.inner.discard_relfile(rt.new);
                }
            }
            if let Some(live) = live {
                // Persist the catalog only if it lagged the WAL, and repoint the
                // in-memory table handle at the final live file.
                if self.catalog.current_relfilenode(&table) != Some(live) {
                    // Recovery only reconciles permanent, WAL-logged (public) tables;
                    // memory tables never reach the WAL.
                    self.catalog
                        .swap_relfilenode("public", &table, live)
                        .unwrap_or_else(|e| panic!("relation catalog write failed: {e}"));
                }
                if let Some(t) = self
                    .tables
                    .read()
                    .unwrap_or_else(|_| panic!("rwlock poisoned"))
                    .get(&("public".to_string(), table.clone()))
                {
                    t.rebind(live);
                }
            }
        }
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
        let Some(tables) = self
            .inner
            .pending_truncates
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .remove(&xid)
        else {
            return; // hot path: nothing to finalize for this transaction
        };
        let handles = self.tables.read().unwrap_or_else(|_| panic!("rwlock poisoned"));
        for (namespace, name) in tables {
            if let Some(t) = handles.get(&(namespace.clone(), name.clone())) {
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
                    }
                    self.inner.discard_relfile(old);
                    t.release_truncate_lock(owner);
                }
            }
        }
    }

    /// Discard the transaction's staged TRUNCATE files (the table keeps its
    /// original file) and release the exclusive table lock.
    fn on_abort(&self, xid: Xid) {
        let Some(tables) = self
            .inner
            .pending_truncates
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .remove(&xid)
        else {
            return;
        };
        let handles = self.tables.read().unwrap_or_else(|_| panic!("rwlock poisoned"));
        for (namespace, name) in tables {
            if let Some(t) = handles.get(&(namespace.clone(), name.clone())) {
                if let Some((new, owner)) = t.abort_truncate(xid) {
                    self.inner.discard_relfile(new);
                    t.release_truncate_lock(owner);
                }
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
        tables: &HashMap<(String, String), Arc<HeapTable>>,
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
    fn create_table(&self, schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError> {
        let namespace = schema.namespace.clone();
        let mut tables = self
            .tables
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        if self.relation_name_taken(&tables, &namespace, &schema.name) {
            return Err(StorageError::TableAlreadyExists(schema.name));
        }
        let rel = self
            .catalog
            .create(&schema)
            .expect("relation catalog write failed");
        // A `Temporary` table is RAM-backed: register its relfilenode with the
        // storage manager so every page op routes to memory, never a file. An
        // `Unlogged` table is on-disk (WAL-skipped but file-backed), so it is not
        // registered here — it uses a real `base/<relfilenode>` file.
        if schema.persistence.is_ram_backed() {
            self.inner.bufpool.smgr().register_memory(rel);
        }
        let table = Arc::new(HeapTable::new(
            Arc::clone(&self.inner),
            rel,
            schema.clone(),
            Vec::new(),
        ));
        tables.insert((namespace, schema.name), Arc::clone(&table));
        Ok(table as Arc<dyn TableAm>)
    }

    fn open_table(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        self.resolve(Some("public"), name)
    }

    fn shutdown(&self) {
        // Flush everything and mark the control file clean, so the next startup
        // keeps unlogged relations' data. Reuse the last checkpoint's next_xid — a
        // valid floor; recovery recomputes the exact value from the WAL.
        let next_xid = Xid(self.last_next_xid.load(Ordering::Relaxed));
        if let Err(e) = self.write_control_file(next_xid, true) {
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
        let (rel, staged) = {
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
            let staged = tables.get(&key).and_then(|t| t.staged_relfilenode());
            tables.remove(&key);
            (rel, staged)
        };
        // Physical cleanup runs after the tables lock is released, so an IO error
        // unlinking the file panics only this statement rather than poisoning the
        // lock and disabling every other table operation. Evict the relation's
        // buffered pages first so a later checkpoint can't write them back to the
        // file we are about to unlink.
        self.inner.bufpool.forget_relation(rel);
        self.inner
            .bufpool
            .smgr()
            .unlink(rel)
            .expect("relation file unlink failed");
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
        let index_rel = if target.can_index(&index) && !target.schema().persistence.is_ram_backed() {
            self.catalog.alloc_relfilenode()
        } else {
            RelFileNode(0)
        };
        // Build the B-tree and make its WAL durable FIRST, then commit the catalog
        // record. Ordering matters for crash safety: if we persisted the catalog
        // first, a crash before the build's WAL flush would leave a durable index
        // record pointing at a B-tree that was never made durable, and the first
        // probe would fault on its missing meta page. With this order, a crash
        // before the catalog write leaves only an orphan file, which the startup
        // GC reclaims (it is not yet in the catalog's live set).
        target.build_index(index.clone(), index_rel);
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
        let _guard = target.begin_index_ddl();
        let rel = self
            .catalog
            .remove_index_in(namespace, table, index_name)
            .expect("relation catalog write failed");
        target.remove_index(index_name);
        if let Some(rel) = rel
            && rel.0 != 0
        {
            self.inner.discard_relfile(rel);
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
            PgEngine::open_recovered(dir.path(), wal).expect("open engine");
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
            PgEngine::open_recovered(dir.path(), Arc::clone(&wal)).expect("open engine");
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
