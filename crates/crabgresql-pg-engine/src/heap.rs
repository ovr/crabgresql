//! The durable heap access method: `TableAm` over slotted pages + buffer pool +
//! WAL. Visibility is the shared [`satisfies_mvcc`] rule applied to the on-page
//! [`TupleHeader`]. A memory table (UNLOGGED/TEMP) uses this same access method
//! over RAM-backed pages with the WAL skipped; only where the versions live differs.
//!
//! Every mutator follows the same write-ahead sequence inside the page's write
//! lock: change the page, append the WAL record, stamp `pd_lsn` with the record
//! LSN, mark the page dirty — so the page can never reach disk ahead of its log.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use crabgresql_storage_api::{
    DeleteResult, IndexMetadata, TableAm, TableSchema, Tid, Tuple, UpdateResult,
};
use crabgresql_txn::{Clog, LockOwner, TupleHeader, TxnContext, XactStatus, Xid, satisfies_mvcc};
use crabgresql_types::Value;
use crabgresql_wal::{Lsn, RmgrId};

use crate::EngineInner;
use crate::btkey;
use crate::lock::{SharedGuard, TableLock};
use crate::nbtree::BTree;
use crate::page::{self, PAGE_HEADER_LEN};
use crate::rec;
use crate::smgr::RelFileNode;
use crate::tuple::{self, TUPLE_HEADER_LEN};

/// Largest tuple that fits on an otherwise-empty page (no TOAST yet).
const MAX_TUPLE: usize = crate::page::BLCKSZ - PAGE_HEADER_LEN - 4;

/// An uncommitted relfilenode-swap TRUNCATE staged by one transaction. Because a
/// TRUNCATE holds the table exclusively until it commits, at most one can exist
/// on a table at a time — hence a single `Option`, not a map.
struct PendingTruncate {
    xid: Xid,
    new_rel: u32,
    /// The lock owner that holds the table's exclusive lock — needed to release
    /// it from the commit/abort hook, which only receives the XID.
    owner: LockOwner,
}

pub struct HeapTable {
    schema: TableSchema,
    engine: Arc<EngineInner>,
    /// The committed relfilenode — what every transaction sees, except the one
    /// with a pending TRUNCATE (which sees `pending.new_rel`).
    live_rel: AtomicU32,
    /// A staged, not-yet-committed TRUNCATE, if any. `pending` and `has_pending`
    /// are the single source of truth for an in-flight swap and are mutated ONLY
    /// together, through this type's methods (`truncate`/`commit_truncate`/
    /// `abort_truncate`/`rebind`), so they never drift (review finding #10).
    pending: RwLock<Option<PendingTruncate>>,
    /// Cheap gate that lets the read/write hot path skip the `pending` RwLock read
    /// entirely while no TRUNCATE is in flight — kept in sync with `pending`.
    has_pending: AtomicBool,
    /// Serializes TRUNCATE (exclusive) against readers/writers (shared).
    lock: Arc<TableLock>,
    /// Last block we inserted into — where the next insert tries first.
    insert_hint: AtomicU32,
    indexes: RwLock<Vec<IndexEntry>>,
    /// A memory table (`relpersistence` `'u'`/`'t'`): its pages live in RAM in the
    /// storage manager and every mutation skips the WAL. Set from
    /// `schema.persistence` at construction.
    is_memory: bool,
}

/// One index attached to a heap table: its semantic metadata, its physical
/// B-tree relfilenode (`RelFileNode(0)` = metadata-only, no physical scan), and
/// the coarse latch shared by every [`BTree`] handle for that index so its
/// operations serialize.
struct IndexEntry {
    meta: IndexMetadata,
    rel: RelFileNode,
    latch: Arc<RwLock<()>>,
}

impl IndexEntry {
    /// Whether this index has a physical B-tree the engine can scan.
    fn is_physical(&self, schema: &TableSchema) -> bool {
        self.rel.0 != 0 && btkey::keys_indexable(schema, &self.meta.keys)
    }

impl HeapTable {
    pub fn new(
        engine: Arc<EngineInner>,
        rel: RelFileNode,
        schema: TableSchema,
        indexes: Vec<(IndexMetadata, RelFileNode)>,
    ) -> HeapTable {
        let indexes = indexes
            .into_iter()
            .map(|(meta, rel)| IndexEntry {
                meta,
                rel,
                latch: Arc::new(RwLock::new(())),
            })
            .collect();
        let is_memory = schema.persistence.is_memory();
        HeapTable {
            schema,
            engine,
            live_rel: AtomicU32::new(rel.0),
            pending: RwLock::new(None),
            has_pending: AtomicBool::new(false),
            lock: Arc::new(TableLock::new()),
            insert_hint: AtomicU32::new(0),
            indexes: RwLock::new(indexes),
            is_memory,
        }
    }

    /// Log a heap change, or skip the WAL entirely for a memory table (returning
    /// LSN 0, which the page carries as its `pd_lsn`). LSN 0 makes the buffer
    /// pool's write-ahead flush a no-op, so a memory table's dirty pages evict
    /// straight to their RAM backing.
    fn log(&self, info: u8, xid: Xid, payload: &[u8]) -> Lsn {
        if self.is_memory {
            Lsn(0)
        } else {
            self.engine.wal.append(RmgrId::HEAP, info, xid, payload)
        }
    }

    pub fn add_index(&self, index: IndexMetadata, rel: RelFileNode) {
        self.indexes
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .push(IndexEntry {
                meta: index,
                rel,
                latch: Arc::new(RwLock::new(())),
            });
    }

    pub fn remove_index(&self, index_name: &str) {
        self.indexes
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .retain(|i| i.meta.name != index_name);
    }

    /// Take the table's exclusive lock for an index DDL operation (DROP INDEX),
    /// returning an RAII guard. Excludes concurrent readers/writers and VACUUM
    /// (which runs as `INTERNAL`) while the index is unpublished and its file
    /// unlinked, so no maintenance/vacuum still holding a handle writes to the
    /// doomed relfilenode.
    pub fn begin_index_ddl(&self) -> crate::lock::ExclusiveGuard {
        self.lock.acquire_exclusive_guard(LockOwner::DDL)
    }

    /// Open the [`BTree`] for an index entry (shares the entry's latch).
    fn btree(&self, entry: &IndexEntry) -> BTree {
        BTree::open(Arc::clone(&self.engine), entry.rel, Arc::clone(&entry.latch))
    }

    /// Build the physical B-tree for a new index and publish it. Runs under the
    /// table's exclusive lock so no concurrent writer slips between the build scan
    /// and publication; the index becomes probe-visible only once fully built.
    /// Every physical version is indexed (dead ones included) — probes re-check
    /// visibility, exactly as the memory engine's build does. An index whose key
    /// columns are not physically indexable is published metadata-only (no file,
    /// no build), so `supports_index_scan` stays false and probes fall back.
    /// Whether an index over `index`'s key columns can be served by a physical
    /// B-tree on this table (all key types order-preserving-encodable). When
    /// false, the index is registered metadata-only (relfilenode 0).
    pub fn can_index(&self, index: &IndexMetadata) -> bool {
        btkey::keys_indexable(&self.schema, &index.keys)
    }

    pub fn build_index(&self, meta: IndexMetadata, rel: RelFileNode) {
        // A metadata-only index (relfilenode 0) has no physical B-tree to build;
        // just publish it. `rel == 0` is the single canonical encoding of
        // metadata-only (create_index allocates a relfilenode only when physical).
        if rel.0 == 0 {
            self.add_index(meta, rel);
            return;
        }
        // Exclude sessions AND vacuum (which runs as INTERNAL) during the build,
        // via an RAII guard so a panic mid-build cannot leak the exclusive hold.
        let _guard = self.lock.acquire_exclusive_guard(LockOwner::DDL);
        let latch = Arc::new(RwLock::new(()));
        let btree = BTree::open(Arc::clone(&self.engine), rel, Arc::clone(&latch));
        Self::io(self.engine.bufpool.smgr().create_if_missing(rel));
        btree.create();
        let cols = btkey::key_columns(&meta.keys);
        let heap_rel = RelFileNode(self.live_rel.load(Ordering::Relaxed));
        let smgr = self.engine.bufpool.smgr();
        let nblocks = Self::io(smgr.nblocks(heap_rel));
        for block in 0..nblocks {
            let page = Self::io(self.engine.bufpool.pin(heap_rel, block));
            let rows: Vec<(Tid, Tuple)> = page.read(|pg| {
                let mut out = Vec::new();
                for off in 1..=page::max_offset(pg) {
                    if let Some(bytes) = page::get_item(pg, off) {
                        out.push((Tid { block, offset: off }, tuple::decode_tuple(bytes).1));
                    }
                }
                out
            });
            for (tid, tuple) in rows {
                if let Some(key) = btkey::encode_row(&self.schema, &cols, &tuple) {
                    btree.insert(&key, tid);
                }
            }
        }
        // Make the build durable now, so the index survives a crash even with no
        // subsequent commit — CREATE INDEX is a durable DDL, as in PostgreSQL.
        let lsn = self.engine.wal.current_lsn();
        Self::io(self.engine.wal.flush(lsn).map_err(std::io::Error::other));
        // Publish with the same latch the build used, so no maintenance window
        // opens between the last build insert and the entry becoming visible.
        self.indexes
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .push(IndexEntry { meta, rel, latch });
        // `_guard` releases the exclusive hold here (or on an unwinding panic).
    }

    /// Add a `key -> tid` entry to every physical index for a newly placed
    /// version. A row whose key column is NULL or an un-indexable value is simply
    /// not indexed (its key never satisfies equality), matching the memory engine.
    fn maintain_insert(&self, tuple: &Tuple, tid: Tid) {
        let indexes = self.indexes.read().unwrap_or_else(|_| panic!("rwlock poisoned"));
        for entry in indexes.iter() {
            if !entry.is_physical(&self.schema) {
                continue;
            }
            let cols = btkey::key_columns(&entry.meta.keys);
            if let Some(key) = btkey::encode_row(&self.schema, &cols, tuple) {
                self.btree(entry).insert(&key, tid);
            }
        }
    }

    #[allow(dead_code)] // used by tooling/tests and future index AM wiring
    pub fn relfilenode(&self) -> RelFileNode {
        RelFileNode(self.live_rel.load(Ordering::Relaxed))
    }

    /// The relfilenode `xid` should read and write: the staged TRUNCATE file if
    /// `xid` is the truncating transaction, else the committed file.
    fn effective_rel(&self, xid: Xid) -> RelFileNode {
        if self.has_pending.load(Ordering::Acquire)
            && let Some(p) = self
                .pending
                .read()
                .unwrap_or_else(|_| panic!("rwlock poisoned"))
                .as_ref()
            && p.xid == xid
        {
            return RelFileNode(p.new_rel);
        }
        RelFileNode(self.live_rel.load(Ordering::Relaxed))
    }

    /// Commit a staged TRUNCATE: the new file becomes the committed one. Returns
    /// the old relfilenode to unlink and the lock owner to release, or `None` if
    /// nothing was pending for `xid`.
    pub(crate) fn commit_truncate(&self, xid: Xid) -> Option<(RelFileNode, LockOwner)> {
        let mut pending = self
            .pending
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let p = pending.take_if(|p| p.xid == xid)?;
        let old = self.live_rel.swap(p.new_rel, Ordering::Relaxed);
        self.has_pending.store(false, Ordering::Release);
        self.insert_hint.store(0, Ordering::Relaxed);
        Some((RelFileNode(old), p.owner))
    }

    /// Discard a staged TRUNCATE on abort: the new file is dropped, the committed
    /// one stays. Returns the new relfilenode to unlink and the lock owner to
    /// release, or `None`.
    pub(crate) fn abort_truncate(&self, xid: Xid) -> Option<(RelFileNode, LockOwner)> {
        let mut pending = self
            .pending
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let p = pending.take_if(|p| p.xid == xid)?;
        self.has_pending.store(false, Ordering::Release);
        Some((RelFileNode(p.new_rel), p.owner))
    }

    /// The relfilenode staged by an uncommitted TRUNCATE, if any. `drop_table`
    /// reads this so it can reclaim a staged file the catalog doesn't know about.
    pub(crate) fn staged_relfilenode(&self) -> Option<RelFileNode> {
        self.pending
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .as_ref()
            .map(|p| RelFileNode(p.new_rel))
    }

    /// Release the exclusive lock a TRUNCATE held (keyed by its lock owner).
    pub(crate) fn release_truncate_lock(&self, owner: LockOwner) {
        self.lock.release_exclusive(owner);
    }

    /// Point the table at `new` after recovery applied a committed TRUNCATE swap
    /// (the on-disk catalog lagged the WAL). Clears any stale pending state.
    pub(crate) fn rebind(&self, new: RelFileNode) {
        *self
            .pending
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned")) = None;
        self.has_pending.store(false, Ordering::Release);
        self.live_rel.store(new.0, Ordering::Relaxed);
        self.insert_hint.store(0, Ordering::Relaxed);
    }

    fn io<T>(r: std::io::Result<T>) -> T {
        // The storage-api trait is infallible; a disk error here is unrecoverable
        // for this backend, matching PostgreSQL's PANIC-on-I/O behavior.
        r.expect("heap engine I/O error")
    }

    /// Place `tuple_bytes` (a full on-page tuple with a placeholder ctid) onto a
    /// page of `rel`, patch its self-ctid, log a HEAP_INSERT, and return its tid.
    fn place(&self, rel: RelFileNode, xid: Xid, tuple_bytes: &[u8]) -> Tid {
        assert!(
            tuple_bytes.len() <= MAX_TUPLE,
            "tuple of {} bytes exceeds page capacity (TOAST not implemented)",
            tuple_bytes.len()
        );
        let smgr = self.engine.bufpool.smgr();
        loop {
            let nblocks = Self::io(smgr.nblocks(rel));
            let target = if nblocks == 0 {
                Self::io(smgr.extend(rel))
            } else {
                self.insert_hint.load(Ordering::Relaxed).min(nblocks - 1)
            };
            let page = Self::io(self.engine.bufpool.pin(rel, target));
            let placed = page.modify(|pg| {
                let off = page::add_item(pg, tuple_bytes)?;
                let tid = Tid {
                    block: target,
                    offset: off,
                };
                let Some(item) = page::get_item_mut(pg, off) else {
                    panic!("newly inserted tuple is missing from its page");
                };
                tuple::set_ctid(item, tid);
                let Some(item) = page::get_item(pg, off) else {
                    panic!("newly inserted tuple is missing from its page");
                };
                let final_bytes = item.to_vec();
                let lsn = self.log(
                    rec::HEAP_INSERT,
                    xid,
                    &rec::insert(rel, target, off, &final_bytes),
                );
                page::set_lsn(pg, lsn.0);
                Some(tid)
            });
            if let Some(tid) = placed {
                self.insert_hint.store(target, Ordering::Relaxed);
                return tid;
            }
            // Page full: extend a fresh block and retry there.
            let fresh = Self::io(smgr.extend(rel));
            self.insert_hint.store(fresh, Ordering::Relaxed);
        }
    }

    /// Atomically mark the version at `tid` deleted by `txn`, under the page's
    /// write lock, logging a HEAP_DELETE. Returns `true` if it was live and got
    /// stamped, `false` if the tid is gone or already deleted. This single
    /// critical section is the serialization point shared by `delete` and
    /// `update`, so concurrent modifications of the same row cannot both succeed.
    fn stamp_deleted(&self, rel: RelFileNode, tid: Tid, txn: &TxnContext) -> bool {
        if tid.block >= Self::io(self.engine.bufpool.smgr().nblocks(rel)) {
            return false;
        }
        let page = Self::io(self.engine.bufpool.pin(rel, tid.block));
        page.modify(|pg| {
            let Some(item) = page::get_item_mut(pg, tid.offset) else {
                return false;
            };
            if !is_live(&tuple::decode_header(item).hdr, &txn.clog) {
                return false;
            }
            tuple::stamp_xmax(item, txn.xid, txn.cid);
            let lsn = self.log(
                rec::HEAP_DELETE,
                txn.xid,
                &rec::delete(rel, tid.block, tid.offset, txn.xid, txn.cid),
            );
            page::set_lsn(pg, lsn.0);
            true
        })
    }
}

/// A version is still updatable/deletable unless a committed transaction deleted
/// it (an aborted or in-flight deleter leaves it live).
fn is_live(hdr: &TupleHeader, clog: &Clog) -> bool {
    !hdr.xmax.is_valid() || clog.status(hdr.xmax) == XactStatus::Aborted
}

impl TableAm for HeapTable {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    fn indexes(&self) -> Vec<IndexMetadata> {
        self.indexes
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .iter()
            .map(|e| e.meta.clone())
            .collect()
    }

    fn supports_index_scan(&self, index_name: &str) -> bool {
        self.indexes
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .iter()
            .any(|e| e.meta.name == index_name && e.is_physical(&self.schema))
    }

    /// Probe the physical B-tree `index_name` for versions whose key equals
    /// `key`, returning those visible to `txn`. `None` (caller falls back to a
    /// scan) when the index is absent or has no physical B-tree; `Some(empty)`
    /// when it is served but nothing matches (including a NULL probe key, which
    /// never satisfies equality). Visibility is decided by re-fetching each heap
    /// tuple through [`TableAm::fetch`] — the index entry is never trusted for
    /// visibility, exactly like a PostgreSQL secondary index.
    fn index_lookup(
        &self,
        index_name: &str,
        key: &[Value],
        txn: &TxnContext,
    ) -> Option<Box<dyn Iterator<Item = (Tid, Tuple)> + Send>> {
        let (rel, latch, cols) = {
            let indexes = self.indexes.read().unwrap_or_else(|_| panic!("rwlock poisoned"));
            let entry = indexes.iter().find(|e| e.meta.name == index_name)?;
            if !entry.is_physical(&self.schema) {
                return None;
            }
            (
                entry.rel,
                Arc::clone(&entry.latch),
                btkey::key_columns(&entry.meta.keys),
            )
        };
        let Some(kb) = btkey::encode_values(&self.schema, &cols, key) else {
            // A NULL (or otherwise un-encodable) probe key: served, no match.
            return Some(Box::new(std::iter::empty()));
        };
        let tids = BTree::open(Arc::clone(&self.engine), rel, latch).search_equal(&kb);
        let mut out = Vec::new();
        for tid in tids {
            if let Some(tuple) = self.fetch(tid, txn) {
                out.push((tid, tuple));
            }
        }
        Some(Box::new(out.into_iter()))
    }

    fn scan(&self, txn: &TxnContext) -> Box<dyn Iterator<Item = (Tid, Tuple)> + Send> {
        // Hold a shared lock for the whole iterator life so a concurrent TRUNCATE
        // cannot unlink the file this scan is reading.
        let guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        let nblocks = Self::io(self.engine.bufpool.smgr().nblocks(rel));
        Box::new(HeapScan {
            engine: Arc::clone(&self.engine),
            rel,
            txn: txn.clone(),
            nblocks,
            cur_block: 0,
            buffer: Vec::new(),
            buf_idx: 0,
            _guard: guard,
        })
    }

    fn fetch(&self, tid: Tid, txn: &TxnContext) -> Option<Tuple> {
        let _guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        let smgr = self.engine.bufpool.smgr();
        if tid.block >= Self::io(smgr.nblocks(rel)) {
            return None;
        }
        let page = Self::io(self.engine.bufpool.pin(rel, tid.block));
        page.read(|pg| {
            let bytes = page::get_item(pg, tid.offset)?;
            let head = tuple::decode_header(bytes);
            if satisfies_mvcc(&head.hdr, &txn.snapshot, &txn.clog, txn.xid, txn.cid) {
                Some(tuple::decode_tuple(bytes).1)
            } else {
                None
            }
        })
    }

    fn insert(&self, tuple: Tuple, txn: &TxnContext) -> Tid {
        let _guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        let hdr = TupleHeader::inserted(txn.xid, txn.cid);
        let bytes = tuple::encode_tuple(
            &tuple,
            &hdr,
            Tid {
                block: 0,
                offset: 0,
            },
        );
        let tid = self.place(rel, txn.xid, &bytes);
        self.maintain_insert(&tuple, tid);
        tid
    }

    fn update(&self, tid: Tid, tuple: Tuple, txn: &TxnContext) -> UpdateResult {
        let _guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        // Stamp the old version deleted-by-us FIRST, atomically under its page
        // lock (`stamp_deleted` is the serialization point). Two concurrent
        // updaters of the same row therefore serialize: the loser sees xmax
        // already set, gets `false`, and inserts no new version — so the row
        // never ends up with two live successors. Only after winning that race do
        // we place the new version.
        if !self.stamp_deleted(rel, tid, txn) {
            return UpdateResult::NotFound;
        }
        // The old tuple's forward ctid is left pointing at itself; the
        // update-chain link is only consumed by EvalPlanQual, which is deferred
        // (P6).
        let hdr = TupleHeader::inserted(txn.xid, txn.cid);
        let new_bytes = tuple::encode_tuple(
            &tuple,
            &hdr,
            Tid {
                block: 0,
                offset: 0,
            },
        );
        // Index only the new version's key; the old version's entry stays and is
        // filtered by MVCC at probe time (reclaimed by vacuum), matching the
        // memory engine's append-only maintenance.
        let new_tid = self.place(rel, txn.xid, &new_bytes);
        self.maintain_insert(&tuple, new_tid);
        UpdateResult::Updated
    }

    fn delete(&self, tid: Tid, txn: &TxnContext) -> DeleteResult {
        let _guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        if self.stamp_deleted(rel, tid, txn) {
            DeleteResult::Deleted
        } else {
            DeleteResult::NotFound
        }
    }

    /// Transactional TRUNCATE via relfilenode swap (PostgreSQL's mechanism).
    /// Stages a fresh empty file and holds the table exclusively until the
    /// transaction ends; the swap is applied on commit and discarded on abort by
    /// the [`crabgresql_txn::TxnFinalize`] hook (`PgEngine::on_commit`/`on_abort`).
    /// The old file stays intact until commit, so a rollback or crash-before-commit
    /// restores every row.
    fn truncate(&self, txn: &TxnContext) {
        // AccessExclusiveLock: block concurrent readers/writers of this table
        // until we commit, so no one reads the old file we are about to unlink or
        // writes rows that the swap would drop. Held until txn end.
        self.lock.acquire_exclusive(txn.lock_owner);
        let old = self.effective_rel(txn.xid);
        // A fresh, never-reused relfilenode for the empty post-truncate file. A
        // memory table's replacement must also be RAM-backed.
        let new = self.engine.catalog.alloc_relfilenode();
        if self.is_memory {
            self.engine.bufpool.smgr().register_memory(new);
        }
        Self::io(self.engine.bufpool.smgr().create_if_missing(new));
        // WAL-log the swap intent {old, new, table} and flush it. Recovery applies
        // the swap only for a committed XID, so the record is safe to write now. A
        // memory table skips the WAL (it never recovers), but the swap still runs
        // through the same commit/abort finalize path in RAM.
        if !self.is_memory {
            let lsn = self.engine.wal.append(
                RmgrId::HEAP,
                rec::HEAP_TRUNCATE,
                txn.xid,
                &rec::truncate(&self.schema.name, old, new),
            );
            Self::io(self.engine.wal.flush(lsn).map_err(std::io::Error::other));
        }
        // Double TRUNCATE in one transaction: the previously staged file is now
        // superseded and, being used only by this uncommitted txn, is discarded.
        let prev = self
            .pending
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .replace(PendingTruncate {
                xid: txn.xid,
                new_rel: new.0,
                owner: txn.lock_owner,
            });
        self.has_pending.store(true, Ordering::Release);
        self.insert_hint.store(0, Ordering::Relaxed);
        match prev {
            Some(prev) => {
                // The superseded staged file was used only by this uncommitted
                // transaction; reclaim it now.
                self.engine.discard_relfile(RelFileNode(prev.new_rel));
                // Already registered with the engine on the first TRUNCATE.
            }
            None => {
                // First TRUNCATE of this table in this txn: register it so the
                // commit/abort hook visits this table once.
                self.engine
                    .pending_truncates
                    .lock()
                    .unwrap_or_else(|_| panic!("mutex poisoned"))
                    .entry(txn.xid)
                    .or_default()
                    .push((self.schema.namespace.clone(), self.schema.name.clone()));
            }
        }
    }

    fn vacuum(&self, oldest: Xid, clog: &Clog) {
        // Vacuum reclaims committed-dead versions from the committed file.
        let _guard = self.lock.acquire_shared(LockOwner::INTERNAL);
        let rel = RelFileNode(self.live_rel.load(Ordering::Relaxed));
        // Snapshot the physical indexes so each reclaimed version's index entry
        // can be removed too — otherwise a stale `key -> tid` would point at a
        // heap slot that a later insert reuses, yielding wrong rows.
        let phys: Vec<(RelFileNode, Arc<RwLock<()>>, Vec<usize>)> = self
            .indexes
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .iter()
            .filter(|e| e.is_physical(&self.schema))
            .map(|e| {
                (
                    e.rel,
                    Arc::clone(&e.latch),
                    btkey::key_columns(&e.meta.keys),
                )
            })
            .collect();
        let smgr = self.engine.bufpool.smgr();
        let nblocks = Self::io(smgr.nblocks(rel));
        for block in 0..nblocks {
            let page = Self::io(self.engine.bufpool.pin(rel, block));
            // Collect the offsets to free and, when indexes exist, the version's
            // values so its index entries can be deleted.
            let (freed, victims): (Vec<u16>, Vec<(Tid, Tuple)>) = page.read(|pg| {
                let mut offs = Vec::new();
                let mut victims = Vec::new();
                for off in 1..=page::max_offset(pg) {
                    if let Some(bytes) = page::get_item(pg, off) {
                        let xmax = tuple::decode_header(bytes).hdr.xmax;
                        if xmax.is_valid() && xmax < oldest && clog.is_committed(xmax) {
                            offs.push(off);
                            if !phys.is_empty() {
                                victims.push((Tid { block, offset: off }, tuple::decode_tuple(bytes).1));
                            }
                        }
                    }
                }
                (offs, victims)
            });
            if freed.is_empty() {
                continue;
            }
            // Remove each reclaimed version's index entries BEFORE freeing the heap
            // slots (PostgreSQL's two-pass order). If the slot were freed first, a
            // concurrent insert could reuse the offset before its stale `key -> tid`
            // entry is gone, and a probe would then return the reused row for the
            // old key. Deleting entries first closes that window.
            for (tid, tuple) in &victims {
                for (irel, latch, cols) in &phys {
                    if let Some(key) = btkey::encode_row(&self.schema, cols, tuple) {
                        BTree::open(Arc::clone(&self.engine), *irel, Arc::clone(latch))
                            .delete(&key, *tid);
                    }
                }
            }
            page.modify(|pg| {
                for &off in &freed {
                    page::set_flags(pg, off, page::LP_UNUSED);
                }
                page::compact(pg);
                let lsn = self.log(rec::HEAP_VACUUM, Xid::INVALID, &rec::vacuum(rel, block, &freed));
                page::set_lsn(pg, lsn.0);
            });
        }
    }
}

/// A snapshot-stable scan: it captures the block count up front and, per block,
/// pins the page, buffers the visible rows, then drops the pin before yielding
/// them — so no frame lock is ever held across an iterator step.
struct HeapScan {
    engine: Arc<EngineInner>,
    rel: RelFileNode,
    txn: TxnContext,
    nblocks: u32,
    cur_block: u32,
    buffer: Vec<(Tid, Tuple)>,
    buf_idx: usize,
    /// Shared table-lock hold kept for the iterator's whole life, so a concurrent
    /// TRUNCATE cannot unlink `rel` mid-scan.
    _guard: SharedGuard,
}

impl Iterator for HeapScan {
    type Item = (Tid, Tuple);

    fn next(&mut self) -> Option<(Tid, Tuple)> {
        loop {
            if self.buf_idx < self.buffer.len() {
                let row = self.buffer[self.buf_idx].clone();
                self.buf_idx += 1;
                return Some(row);
            }
            if self.cur_block >= self.nblocks {
                return None;
            }
            let block = self.cur_block;
            self.cur_block += 1;
            self.buffer.clear();
            self.buf_idx = 0;
            let page = HeapTable::io(self.engine.bufpool.pin(self.rel, block));
            page.read(|pg| {
                for off in 1..=page::max_offset(pg) {
                    if let Some(bytes) = page::get_item(pg, off) {
                        // A visible tuple must at least be a full header long.
                        debug_assert!(bytes.len() >= TUPLE_HEADER_LEN);
                        let head = tuple::decode_header(bytes);
                        if satisfies_mvcc(
                            &head.hdr,
                            &self.txn.snapshot,
                            &self.txn.clog,
                            self.txn.xid,
                            self.txn.cid,
                        ) {
                            let (_, vals) = tuple::decode_tuple(bytes);
                            self.buffer.push((Tid { block, offset: off }, vals));
                        }
                    }
                }
            });
        }
    }
}
