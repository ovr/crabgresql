//! The durable heap access method: `TableAm` over slotted pages + buffer pool +
//! WAL. Visibility is the shared [`satisfies_mvcc`] rule applied to the on-page
//! [`TupleHeader`]. `Unlogged` and `Temporary` tables use this same access method
//! with the WAL skipped (`Unlogged` still on-disk, `Temporary` in RAM); only the
//! WAL and the backing store differ.
//!
//! Every mutator follows the same write-ahead sequence inside the page's write
//! lock: change the page, append the WAL record, stamp `pd_lsn` with the record
//! LSN, mark the page dirty — so the page can never reach disk ahead of its log.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use crabgresql_storage_api::{
    ColumnProjection, DeleteResult, IndexMetadata, IndexProbe, RelPersistence, RelStats,
    StorageError, TableAm, TableSchema, Tid, Tuple, TupleStream, UpdateResult,
};
use crabgresql_txn::{
    Clog, LockOwner, SharedGuard, TableLock, TupleHeader, TxnContext, XactStatus, Xid,
    satisfies_mvcc,
};
use crabgresql_types::Value;
use crabgresql_wal::{Lsn, RmgrId};

use crate::EngineInner;
use crate::btkey;
use crate::nbtree::BTree;
use crate::page::{self, PAGE_HEADER_LEN};
use crate::rec;
use crate::smgr::RelFileNode;
use crate::toast;
use crate::tuple::{self, TUPLE_HEADER_LEN};

/// Largest tuple that fits on an otherwise-empty page: the page minus its header
/// and one line pointer. PostgreSQL reserves 8 bytes for the line pointer where
/// our [`page::ItemId`] needs only 4, and we match its figure rather than our
/// own — 8160 is the number it prints in `row is too big: size N, maximum size
/// 8160`, and a limit that disagreed with the message would accept rows PG
/// rejects.
const MAX_TUPLE: usize = crate::page::BLCKSZ - PAGE_HEADER_LEN - 8;

/// Largest encoded datum that may be stored out of line. PostgreSQL caps a
/// varlena at 1 GB and reports `invalid memory alloc request size`; the cap here
/// exists for the same reason plus one of our own — a
/// [`toast::ToastPointer`]'s `rawsize` is a `u32` and is what terminates the
/// chain walk, so a value past 4 GiB would wrap and read back silently short.
const MAX_TOASTED_VALUE: usize = 1024 * 1024 * 1024;

/// The self-ctid a tuple carries until [`HeapTable::place_item`] patches it to
/// the tid it actually landed on.
const PLACEHOLDER_TID: Tid = Tid {
    block: 0,
    offset: 0,
};

/// An uncommitted relfilenode-swap TRUNCATE staged by one transaction. Because a
/// TRUNCATE holds the table exclusively until it commits, at most one can exist
/// on a table at a time — hence a single `Option`, not a map.
struct PendingTruncate {
    xid: Xid,
    new_rel: u32,
    /// The empty replacement B-tree staged for each physical index, by index
    /// name. Indexes are swapped in lockstep with the heap: the new file reuses
    /// the tids the old index entries name, so an index carried across the swap
    /// would answer probes with rows whose key does not match.
    new_indexes: Vec<(String, RelFileNode)>,
    /// The lock owner that holds the table's exclusive lock — needed to release
    /// it from the commit/abort hook, which only receives the XID.
    owner: LockOwner,
}

/// What a committed TRUNCATE hands its caller: the files to unlink and the new
/// ones the durable catalog must name, plus the lock owner to release.
pub(crate) struct TruncateCommit {
    pub old_heap: RelFileNode,
    /// The superseded index files, to unlink.
    pub old_indexes: Vec<RelFileNode>,
    /// The new index relfilenodes, by index name, for the catalog write.
    pub new_indexes: Vec<(String, RelFileNode)>,
    pub owner: LockOwner,
}

/// What [`HeapTable::plan_tuple`] decided, before anything is written.
///
/// Splitting the decision from the writing is what lets `update` do both of the
/// things it needs: reject an oversized row *before* stamping the old version
/// deleted, and write chunks only *after* winning the race for it.
enum Planned {
    /// Fits inline; these are the final bytes.
    Inline(Vec<u8>),
    /// Needs out-of-line storage for the listed attributes, whose encoded datums
    /// are carried so they are not encoded a third time.
    Toast {
        chosen: Vec<usize>,
        encoded: Vec<Vec<u8>>,
    },
}

pub struct HeapTable {
    /// The relation's shape. Behind a lock because `ALTER TABLE` republishes it
    /// while other sessions hold this table — see `TableAm::schema`. Only
    /// `columns` ever differs between versions; name, namespace and persistence
    /// are fixed at creation.
    schema: RwLock<Arc<TableSchema>>,
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
    /// A `HEAP_TRUNCATE` record is in the log whose swap the catalog does not name
    /// yet, so replay must still be able to reach it.
    ///
    /// Deliberately NOT tied to `has_pending`, which is cleared the moment the
    /// in-memory swap is applied — several statements before `swap_relfilenode`
    /// makes it durable. A checkpoint sampling in between would publish a redo
    /// point above the record whose replay is the only thing that repairs the
    /// swap, and the committed TRUNCATE would silently revert.
    ///
    /// Set only when a record was actually appended: a `wal_skipped` relation has
    /// nothing for replay to reach, so pinning the redo point for it would clamp
    /// the whole cluster to a whole-stream replay for no reason.
    ///
    /// There is no `Drop` to clear it, on purpose: a panic between the append and
    /// the catalog write leaves it set, which is the safe direction.
    truncate_unreconciled: AtomicBool,
    /// Serializes TRUNCATE (exclusive) against readers/writers (shared).
    lock: Arc<TableLock>,
    /// Last block we inserted into — where the next insert tries first.
    insert_hint: AtomicU32,
    /// The relation holding this table's out-of-line attribute chunks, or
    /// `RelFileNode(0)` if nothing has needed one yet. Created lazily by
    /// [`HeapTable::ensure_toast_rel`] on the first row that must be toasted, so
    /// a table of narrow columns never pays for a second file — the same
    /// behavior PostgreSQL exposes through a zero `pg_class.reltoastrelid`.
    ///
    /// The sentinel is safe because relfilenodes start at 1.
    toast_rel: AtomicU32,
    /// Insert hint for `toast_rel`, kept apart from `insert_hint` so chunk writes
    /// and heap writes do not knock each other back to a full block.
    toast_hint: AtomicU32,
    /// Serializes chunk-store creation. Inserts hold only a *shared* table lock,
    /// so without this two concurrent first-time toasting writers each allocate a
    /// relfilenode and each persist it: the catalog keeps one, and the next
    /// startup's orphan sweep unlinks the other — permanently destroying the rows
    /// that toasted into it. Held across the whole allocate → create → persist →
    /// publish sequence, which is short and runs at most once per table.
    toast_create: std::sync::Mutex<()>,
    indexes: RwLock<Vec<IndexEntry>>,
    /// This relation's storage class, cached from `schema.persistence` at
    /// construction.
    ///
    /// Not a second source of truth: only `columns` ever differs between schema
    /// versions (see `schema`), so persistence is fixed for the relation's life.
    /// Caching it keeps the WAL and backing-store predicates off the schema lock
    /// on the per-row write path, where they are asked once per index per row.
    ///
    /// The axes are distinct and neither implies the other: `is_wal_skipped()`
    /// is `Unlogged | Temporary`, while `is_unlogged()` is `Unlogged` alone — an
    /// `Unlogged` table skips the WAL but is still on disk.
    persistence: RelPersistence,
    /// What `ANALYZE` last measured, or `None` for a never-analyzed relation.
    /// Loaded from the durable catalog at open and rewritten by `ANALYZE`;
    /// non-transactional, matching PostgreSQL (see `RelCatalog::set_stats`).
    analyzed: RwLock<Option<(u32, f64)>>,
}

/// One index attached to a heap table: its semantic metadata, its physical
/// B-tree relfilenode (`RelFileNode(0)` = metadata-only, no physical scan), and
/// the coarse latch shared by every [`BTree`] handle for that index so its
/// operations serialize.
struct IndexEntry {
    meta: IndexMetadata,
    rel: RelFileNode,
    /// The empty replacement tree staged by an uncommitted TRUNCATE, which only
    /// the truncating transaction reads and writes (see
    /// [`HeapTable::effective_index_rel`]). Set and cleared together with the
    /// table's `pending`.
    staged: Option<RelFileNode>,
    latch: Arc<RwLock<()>>,
}

impl IndexEntry {
    /// Whether this index has a physical B-tree the engine can scan.
    ///
    /// Judged on the committed `rel`, never on `staged`: a staged index is still
    /// an index, and the two are always both zero or both non-zero anyway
    /// (`truncate` stages a file exactly for the entries this returns true for).
    fn is_physical(&self, schema: &TableSchema) -> bool {
        self.rel.0 != 0 && btkey::keys_indexable(schema, &self.meta.keys)
    }
}

impl HeapTable {
    /// `toast` is the relation's existing chunk relfilenode, or `RelFileNode(0)`
    /// for one that has never toasted an attribute.
    pub fn new(
        engine: Arc<EngineInner>,
        rel: RelFileNode,
        toast: RelFileNode,
        schema: TableSchema,
        indexes: Vec<(IndexMetadata, RelFileNode)>,
    ) -> HeapTable {
        let indexes = indexes
            .into_iter()
            .map(|(meta, rel)| IndexEntry {
                meta,
                rel,
                staged: None,
                latch: Arc::new(RwLock::new(())),
            })
            .collect();
        let persistence = schema.persistence;
        HeapTable {
            schema: RwLock::new(Arc::new(schema)),
            engine,
            live_rel: AtomicU32::new(rel.0),
            pending: RwLock::new(None),
            has_pending: AtomicBool::new(false),
            truncate_unreconciled: AtomicBool::new(false),
            lock: Arc::new(TableLock::new()),
            insert_hint: AtomicU32::new(0),
            toast_rel: AtomicU32::new(toast.0),
            toast_hint: AtomicU32::new(0),
            toast_create: std::sync::Mutex::new(()),
            indexes: RwLock::new(indexes),
            persistence,
            analyzed: RwLock::new(None),
        }
    }

    /// The relation's size in 8 KB pages, straight from the storage manager
    /// (a file-length division, so O(1)). `Err` only on an I/O failure.
    ///
    /// Reports the **committed** relfilenode; a caller that must agree with a
    /// scan should use [`HeapTable::measure`] instead.
    pub fn nblocks(&self) -> std::io::Result<u32> {
        self.engine.bufpool.smgr().nblocks(self.relfilenode())
    }

    /// Count the rows visible to `txn` and size the relation, as one consistent
    /// observation: `(relpages, reltuples)`.
    ///
    /// Both halves must describe the *same* file, which needs two things a
    /// naive `scan().count()` then `nblocks()` does not give:
    ///
    /// - **One relfilenode.** A scan reads [`HeapTable::effective_rel`] — the
    ///   staged file when `txn` itself has an uncommitted TRUNCATE — while
    ///   [`HeapTable::nblocks`] reads the committed one. Inside
    ///   `BEGIN; TRUNCATE t; ANALYZE t;` those differ, pairing the new file's
    ///   row count with the old file's size.
    /// - **One lock hold.** The guard `scan` takes lives only as long as its
    ///   iterator, so a `scan(..).count()` releases it before any later call;
    ///   another session's TRUNCATE could commit in the gap and swap the file
    ///   between the two reads. Holding a shared hold across both closes that
    ///   window — nested shared holds by the same owner are refcounted, so the
    ///   one `scan` takes underneath is harmless.
    pub fn measure(&self, txn: &TxnContext) -> (u32, f64) {
        let _guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        let relpages = self
            .engine
            .bufpool
            .smgr()
            .nblocks(rel)
            // Statistics are advisory: a size that cannot be read pairs a zero
            // with the real count rather than failing the statement.
            .unwrap_or(0);
        let reltuples = self.scan(txn, &ColumnProjection::All).count() as f64;
        (relpages, reltuples)
    }

    /// Install the `ANALYZE` result the durable catalog holds for this relation,
    /// at open, or the one a fresh `ANALYZE` just produced.
    pub fn set_analyzed(&self, relpages: u32, reltuples: f64) {
        *self
            .analyzed
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned")) = Some((relpages, reltuples));
    }

    /// Log a heap change, or skip the WAL entirely for a WAL-skipped table
    /// (`Unlogged`/`Temporary`), returning LSN 0 which the page carries as its
    /// `pd_lsn`. LSN 0 makes the buffer pool's write-ahead flush a no-op, so the
    /// dirty page evicts straight to its backing store (RAM for `Temporary`, a
    /// file for `Unlogged`).
    fn log(&self, info: u8, xid: Xid, payload: &[u8]) -> Lsn {
        if self.persistence.is_wal_skipped() {
            Lsn(0)
        } else {
            self.engine.wal.append(RmgrId::HEAP, info, xid, payload).end
        }
    }

    /// This table's current shape.
    ///
    /// Call it **once per operation** and pass `&schema` down. Never inside a
    /// per-row or per-index loop, and never twice in one expression: two calls
    /// can return two different versions, so a pair like "is this index
    /// physical?" and "encode this row for it" could disagree about `columns`
    /// and silently leave a row out of a live index.
    ///
    /// One snapshot is stable for a whole operation by construction, not by
    /// luck: every caller holds a `TableLock` hold, and the only mutator
    /// (`set_columns_not_null`) takes that lock exclusively.
    ///
    /// Fields that cannot change are cached on the struct (`persistence`) and
    /// must not be read through here.
    fn snap(&self) -> Arc<TableSchema> {
        Arc::clone(
            &self
                .schema
                .read()
                .unwrap_or_else(|_| panic!("rwlock poisoned")),
        )
    }

    pub fn add_index(&self, index: IndexMetadata, rel: RelFileNode) {
        self.indexes
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .push(IndexEntry {
                meta: index,
                rel,
                staged: None,
                latch: Arc::new(RwLock::new(())),
            });
    }

    /// Publish a schema with `columns` marked NOT NULL, after the durable
    /// catalog has already recorded it.
    ///
    /// Builds a whole new [`TableSchema`] and swaps it in rather than editing
    /// the live one: a session that already took a snapshot keeps reading the
    /// version it started with, and the next one sees the new shape entire.
    /// That is what makes the swap indivisible across a multi-column key — no
    /// reader can observe half of it — and it is the same move a future
    /// `ADD COLUMN` will make, which editing a field in place could not be.
    pub fn set_columns_not_null(&self, columns: &[usize]) {
        let mut guard = self
            .schema
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let mut next = (**guard).clone();
        for &c in columns {
            next.columns[c].nullable = false;
        }
        *guard = Arc::new(next);
    }

    /// Unpublish an index, returning its committed relfilenode and any file a
    /// staged TRUNCATE allocated for it, so the caller can unlink both.
    ///
    /// The staged half is unreachable in practice — DROP INDEX takes the table
    /// exclusively as [`LockOwner::DDL`], which cannot be granted while a
    /// TRUNCATE holds it under the session's owner — but returning it keeps the
    /// invariant explicit instead of implicit.
    pub fn remove_index(&self, index_name: &str) -> Option<(RelFileNode, Option<RelFileNode>)> {
        let mut indexes = self
            .indexes
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let pos = indexes.iter().position(|i| i.meta.name == index_name)?;
        let entry = indexes.remove(pos);
        Some((entry.rel, entry.staged))
    }

    /// Take the table's exclusive lock for an index DDL operation (DROP INDEX),
    /// returning an RAII guard. Excludes concurrent readers/writers and VACUUM
    /// (which runs as `INTERNAL`) while the index is unpublished and its file
    /// unlinked, so no maintenance/vacuum still holding a handle writes to the
    /// doomed relfilenode.
    pub fn begin_index_ddl(&self) -> crabgresql_txn::ExclusiveGuard {
        self.lock.acquire_exclusive_guard(LockOwner::DDL)
    }

    /// Open the [`BTree`] `xid` should read and write for an index entry (shares
    /// the entry's latch): the staged tree if `xid` is the truncating
    /// transaction, else the committed one.
    fn btree(&self, entry: &IndexEntry, xid: Xid) -> BTree {
        BTree::open(
            Arc::clone(&self.engine),
            self.effective_index_rel(entry, xid),
            Arc::clone(&entry.latch),
            self.persistence.is_unlogged(),
        )
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
        btkey::keys_indexable(&self.snap(), &index.keys)
    }

    /// `Err` when a row's out-of-line value cannot be read: CREATE INDEX is an
    /// ordinary user command, so an unreadable chunk store must fail the statement
    /// rather than the process.
    pub fn build_index(&self, meta: IndexMetadata, rel: RelFileNode) -> Result<(), StorageError> {
        // A metadata-only index (relfilenode 0) has no physical B-tree to build;
        // just publish it. `rel == 0` is the single canonical encoding of
        // metadata-only (create_index allocates a relfilenode only when physical).
        if rel.0 == 0 {
            self.add_index(meta, rel);
            return Ok(());
        }
        // Exclude sessions AND vacuum (which runs as INTERNAL) during the build,
        // via an RAII guard so a panic mid-build cannot leak the exclusive hold.
        let _guard = self.lock.acquire_exclusive_guard(LockOwner::DDL);
        // Pinned by the hold above: the only republisher takes the same
        // exclusive lock, so one snapshot covers the whole build.
        let schema = self.snap();
        let latch = Arc::new(RwLock::new(()));
        let btree = BTree::open(
            Arc::clone(&self.engine),
            rel,
            Arc::clone(&latch),
            self.persistence.is_unlogged(),
        );
        Self::io(self.engine.bufpool.smgr().create_if_missing(rel));
        btree.create();
        let cols = btkey::key_columns(&meta.keys);
        let heap_rel = RelFileNode(self.live_rel.load(Ordering::Relaxed));
        let smgr = self.engine.bufpool.smgr();
        let nblocks = Self::io(smgr.nblocks(heap_rel));
        for block in 0..nblocks {
            let page = Self::io(self.engine.bufpool.pin(heap_rel, block));
            let rows: Vec<(Tid, Result<tuple::RawTuple, StorageError>)> = page.read(|pg| {
                let mut out = Vec::new();
                for off in 1..=page::max_offset(pg) {
                    if let Some(bytes) = page::get_item(pg, off) {
                        out.push((Tid { block, offset: off }, tuple::decode_raw(bytes)));
                    }
                }
                out
            });
            for (tid, raw) in rows {
                // An index key over a toasted column has to be the value itself,
                // never the pointer standing in for it — so detoast, outside the
                // page's frame lock. A chunk store that cannot be read fails the
                // statement; it must not take the process down, since CREATE INDEX
                // is an ordinary user command.
                let tuple = raw.and_then(|raw| raw.resolve(|p| detoast(&self.engine, p)))?;
                if let Some(key) = btkey::encode_row(&schema, &cols, &tuple) {
                    btree.insert(&key, tid);
                }
            }
        }
        // Make the build durable now, so the index survives a crash even with no
        // subsequent commit — CREATE INDEX is a durable DDL, as in PostgreSQL. An
        // Unlogged index writes no WAL (it is rebuilt on crash), so nothing to flush.
        if !self.persistence.is_unlogged() {
            let lsn = self.engine.wal.current_lsn();
            Self::io(self.engine.wal.flush(lsn).map_err(std::io::Error::other));
        }
        // Publish with the same latch the build used, so no maintenance window
        // opens between the last build insert and the entry becoming visible.
        self.indexes
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .push(IndexEntry {
                meta,
                rel,
                staged: None,
                latch,
            });
        // `_guard` releases the exclusive hold here (or on an unwinding panic).
        Ok(())
    }

    /// Add a `key -> tid` entry to every physical index for a newly placed
    /// version. A row whose key column is NULL or an un-indexable value is simply
    /// not indexed (its key never satisfies equality), matching the memory engine.
    fn maintain_insert(&self, tuple: &Tuple, tid: Tid, xid: Xid) {
        let indexes = self
            .indexes
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        // An unindexed table has nothing to maintain, so its rows never touch
        // the schema lock at all — that is the bulk-load shape.
        if indexes.is_empty() {
            return;
        }
        // One snapshot for the whole row: every index is judged and encoded
        // against the same `columns`, and the row costs one schema-lock
        // acquisition rather than two per index.
        let schema = self.snap();
        for entry in indexes.iter() {
            if !entry.is_physical(&schema) {
                continue;
            }
            let cols = btkey::key_columns(&entry.meta.keys);
            if let Some(key) = btkey::encode_row(&schema, &cols, tuple) {
                self.btree(entry, xid).insert(&key, tid);
            }
        }
    }

    pub fn relfilenode(&self) -> RelFileNode {
        RelFileNode(self.live_rel.load(Ordering::Relaxed))
    }

    /// This table's physical indexes as `(name, committed relfilenode)`, in
    /// publication order — the shape `RelCatalog::index_relfilenodes` returns, so
    /// recovery can compare a handle against the catalog directly.
    pub(crate) fn index_relfilenodes(&self) -> Vec<(String, RelFileNode)> {
        self.indexes
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .iter()
            .filter(|e| e.rel.0 != 0)
            .map(|e| (e.meta.name.clone(), e.rel))
            .collect()
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

    /// The index relfilenode `xid` should read and write, mirroring
    /// [`HeapTable::effective_rel`] for the heap file.
    ///
    /// Load-bearing: without it the truncating transaction's own inserts would
    /// maintain the *committed* tree, keyed to the staged heap's tids, and a
    /// rollback could not undo them.
    fn effective_index_rel(&self, entry: &IndexEntry, xid: Xid) -> RelFileNode {
        if self.has_pending.load(Ordering::Acquire)
            && let Some(staged) = entry.staged
            && let Some(p) = self
                .pending
                .read()
                .unwrap_or_else(|_| panic!("rwlock poisoned"))
                .as_ref()
            && p.xid == xid
        {
            return staged;
        }
        entry.rel
    }

    /// Commit a staged TRUNCATE: the new file becomes the committed one. Returns
    /// the old heap relfilenode to unlink and the lock owner to release, or
    /// `None` if nothing was pending for `xid`.
    ///
    /// The chunk store keeps its relfilenode across a TRUNCATE — see
    /// [`HeapTable::reclaim_toast_after_truncate`] for why the space is reclaimed
    /// by emptying that file rather than by swapping it.
    pub(crate) fn commit_truncate(&self, xid: Xid) -> Option<TruncateCommit> {
        // `indexes` before `pending`, the order every reader takes them in
        // (`index_lookup` -> `effective_index_rel`). Holding both makes the heap
        // and index swap indivisible to a concurrent probe.
        let mut indexes = self
            .indexes
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let mut pending = self
            .pending
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let p = pending.take_if(|p| p.xid == xid)?;
        let old = self.live_rel.swap(p.new_rel, Ordering::Relaxed);
        let mut old_indexes = Vec::new();
        for entry in indexes.iter_mut() {
            if let Some(staged) = entry.staged.take() {
                old_indexes.push(std::mem::replace(&mut entry.rel, staged));
            }
        }
        self.has_pending.store(false, Ordering::Release);
        // `truncate_unreconciled` is deliberately NOT cleared here. The swap is
        // only in memory at this point; it becomes durable when the caller's
        // `swap_relfilenode` returns, and `truncate_reconciled` is what says so.
        // Clearing it next to `has_pending` is the mistake this comment exists to
        // stop — it would drop the pin exactly while the swap is least durable.
        self.insert_hint.store(0, Ordering::Relaxed);
        // The measurement described the file that just went away, so drop it
        // rather than let it describe the empty one. Back to never-analyzed —
        // which is what PostgreSQL reports after a TRUNCATE (`relpages = 0`,
        // `reltuples = -1`), not a measured zero.
        *self
            .analyzed
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned")) = None;
        Some(TruncateCommit {
            old_heap: RelFileNode(old),
            old_indexes,
            new_indexes: p.new_indexes,
            owner: p.owner,
        })
    }

    /// Empty the chunk store after a committed TRUNCATE, if it is safe to.
    ///
    /// The store keeps its relfilenode rather than being swapped like the heap
    /// file. Swapping it would need a second relfilenode that no WAL record names
    /// and that the durable catalog only learns about at commit — so a crash in
    /// that window would leave a committed row pointing into a file the startup
    /// orphan sweep unlinks. Emptying in place needs neither: the relfilenode
    /// never changes, so the catalog is always right.
    ///
    /// Only safe when the post-truncate heap file is empty, which is the ordinary
    /// `TRUNCATE t;` case. A transaction that truncates and then inserts in the
    /// same breath has live tuples pointing into this store, so its space is
    /// carried over instead — a bounded leak that the next TRUNCATE reclaims.
    ///
    /// Runs after commit, so nothing can still reference the old chunks: TRUNCATE
    /// held the table exclusively until this moment.
    pub(crate) fn reclaim_toast_after_truncate(&self) {
        let Some(toast) = self.toast_relfilenode() else {
            return;
        };
        let smgr = self.engine.bufpool.smgr();
        let heap_empty = matches!(smgr.nblocks(self.relfilenode()), Ok(0));
        if !heap_empty {
            return;
        }
        self.engine.bufpool.forget_relation(toast);
        if let Err(error) = smgr.truncate(toast) {
            // Advisory: failing to reclaim leaves unreferenced chunks, never
            // unreadable ones, so it must not fail the commit.
            tracing::warn!(
                table = %self.snap().name,
                %error,
                "TRUNCATE: could not empty the chunk store; its space is retained"
            );
            return;
        }
        self.toast_hint.store(0, Ordering::Relaxed);
    }

    /// Discard a staged TRUNCATE on abort: the new files are dropped, the
    /// committed ones stay. Returns the staged relfilenodes to unlink — the
    /// heap's first, then each index's — and the lock owner to release, or
    /// `None`. The chunk store is untouched by a TRUNCATE until it commits, so
    /// there is nothing to undo there.
    pub(crate) fn abort_truncate(&self, xid: Xid) -> Option<(Vec<RelFileNode>, LockOwner)> {
        // `indexes` before `pending` — see `commit_truncate`.
        let mut indexes = self
            .indexes
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let mut pending = self
            .pending
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let p = pending.take_if(|p| p.xid == xid)?;
        let mut discard = vec![RelFileNode(p.new_rel)];
        for entry in indexes.iter_mut() {
            discard.extend(entry.staged.take());
        }
        self.has_pending.store(false, Ordering::Release);
        // The swap never happened, so no replay is needed to reconcile it.
        self.truncate_reconciled();
        Some((discard, p.owner))
    }

    /// The relfilenodes staged by an uncommitted TRUNCATE — the heap's and every
    /// index's. `drop_table` reads this so it can reclaim staged files the
    /// catalog doesn't know about.
    pub(crate) fn staged_relfilenodes(&self) -> Vec<RelFileNode> {
        self.pending
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .as_ref()
            .map(|p| {
                std::iter::once(RelFileNode(p.new_rel))
                    .chain(p.new_indexes.iter().map(|(_, irel)| *irel))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Release the exclusive lock a TRUNCATE held (keyed by its lock owner).
    pub(crate) fn release_truncate_lock(&self, owner: LockOwner) {
        self.lock.release_exclusive(owner);
    }

    /// Point the table at `new`, and each named index at its post-swap file,
    /// after recovery applied a committed TRUNCATE swap (the on-disk catalog
    /// lagged the WAL). Clears any stale pending state.
    pub(crate) fn rebind(&self, new: RelFileNode, index_rels: &[(String, RelFileNode)]) {
        // `indexes` before `pending` — see `commit_truncate`.
        let mut indexes = self
            .indexes
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        for entry in indexes.iter_mut() {
            entry.staged = None;
            if let Some((_, irel)) = index_rels.iter().find(|(n, _)| *n == entry.meta.name) {
                entry.rel = *irel;
            }
        }
        *self
            .pending
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned")) = None;
        self.has_pending.store(false, Ordering::Release);
        // Recovery just applied the swap and persisted the catalog, which is
        // exactly what the pin was waiting for.
        self.truncate_reconciled();
        self.live_rel.store(new.0, Ordering::Relaxed);
        self.insert_hint.store(0, Ordering::Relaxed);
    }

    /// This table's out-of-line chunk store, or `None` if it never needed one.
    pub fn toast_relfilenode(&self) -> Option<RelFileNode> {
        match self.toast_rel.load(Ordering::Acquire) {
            0 => None,
            rel => Some(RelFileNode(rel)),
        }
    }

    /// Whether a TRUNCATE record of this relation still needs replay to reach it.
    /// Read by the checkpoint; see the field's documentation.
    pub(crate) fn truncate_unreconciled(&self) -> bool {
        self.truncate_unreconciled.load(Ordering::Acquire)
    }

    /// The swap is now named by the durable catalog, so replay need not reach the
    /// record any more.
    pub(crate) fn truncate_reconciled(&self) {
        self.truncate_unreconciled.store(false, Ordering::Release);
    }

    fn io<T>(r: std::io::Result<T>) -> T {
        // The storage-api trait is infallible; a disk error here is unrecoverable
        // for this backend, matching PostgreSQL's PANIC-on-I/O behavior.
        r.expect("heap engine I/O error")
    }

    /// This table's chunk relation, creating it on first use.
    ///
    /// The catalog write is the durability commit point: the relfilenode must be
    /// in [`RelCatalog::live_relfilenodes`] before any chunk can reach the log,
    /// or a crash would leave chunks in a file the next startup's orphan sweep
    /// unlinks. Crashing before the write leaves an empty orphan file instead,
    /// which that same sweep reclaims.
    ///
    /// The whole sequence runs under `toast_create` because inserts hold only a
    /// shared table lock: two writers racing here would each publish a store and
    /// the catalog would keep only one, silently orphaning the other's chunks.
    fn ensure_toast_rel(&self) -> Result<RelFileNode, StorageError> {
        // Fast path: already created, no lock.
        let existing = self.toast_rel.load(Ordering::Acquire);
        if existing != 0 {
            return Ok(RelFileNode(existing));
        }
        let _create = self
            .toast_create
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        // Re-check: another writer may have created it while we waited.
        let existing = self.toast_rel.load(Ordering::Acquire);
        if existing != 0 {
            return Ok(RelFileNode(existing));
        }
        let rel = self.engine.catalog.alloc_relfilenode();
        // A temporary table's chunks must live wherever its rows do.
        if self.persistence.is_ram_backed() {
            self.engine.bufpool.smgr().register_memory(rel);
        }
        Self::io(self.engine.bufpool.smgr().create_if_missing(rel));
        if self.persistence.persists_catalog() {
            // One snapshot: namespace and name must name the same version.
            let schema = self.snap();
            self.engine
                .catalog
                .set_toast_rel(&schema.namespace, &schema.name, rel)
                .map_err(|e| StorageError::Io(e.to_string()))?;
        }
        self.toast_rel.store(rel.0, Ordering::Release);
        Ok(rel)
    }

    /// Write `bytes` to `toast_rel` as a chain of chunks and return the first
    /// chunk's tid.
    ///
    /// Chunks go out **last first** so each one knows its successor's tid before
    /// it is placed; writing forwards would need a second pass to patch every
    /// link. The last chunk points at itself, which terminates the walk.
    fn write_chain(&self, toast_rel: RelFileNode, bytes: &[u8], txn: &TxnContext) -> Tid {
        let hdr = TupleHeader::inserted(txn.xid, txn.cid);
        let mut next: Option<Tid> = None;
        for payload in toast::chunks_last_first(bytes) {
            // A placeholder for the last chunk: `place_item` patches it to the
            // chunk's own tid, which is how the walk knows to stop.
            let link = next.unwrap_or(Tid {
                block: 0,
                offset: 0,
            });
            let chunk = tuple::encode_chunk(payload, &hdr, link);
            next =
                Some(self.place_item(toast_rel, txn.xid, &chunk, &self.toast_hint, next.is_none()));
        }
        next.unwrap_or_else(|| panic!("a toasted attribute always has at least one chunk"))
    }

    /// Decide how `tuple` will be stored, without touching a page.
    ///
    /// Pure: it allocates nothing durable and writes nothing, so a caller may run
    /// it before taking any lock or making any change, and discard the result.
    /// [`StorageError::RowTooBig`] for a row that cannot be made to fit at all.
    fn plan_tuple(&self, tuple: &Tuple, hdr: &TupleHeader) -> Result<Planned, StorageError> {
        let bytes = tuple::encode_inline(tuple, hdr, PLACEHOLDER_TID);
        if bytes.len() <= toast::TOAST_TUPLE_TARGET {
            return Ok(Planned::Inline(bytes));
        }
        // Only the toasting path reads the shape (`typlen` per column), so the
        // common narrow row — every row of an untoasted bulk load — never takes
        // the schema lock.
        let schema = self.snap();

        // Re-derive each attribute's encoded width rather than measuring the
        // whole tuple: the planner needs to know what moving one out would save.
        let mut widths = Vec::with_capacity(tuple.len());
        let mut toastable = Vec::with_capacity(tuple.len());
        let mut encoded = Vec::with_capacity(tuple.len());
        for (i, v) in tuple.iter().enumerate() {
            let mut buf = Vec::new();
            if !matches!(v, Value::Null) {
                crabgresql_types::datum::encode_datum(v, &mut buf);
            }
            // An out-of-line datum's length has to fit the pointer's `rawsize`, and
            // that field is the walk's termination condition — a wrapped length
            // would silently read back a truncated value. PostgreSQL caps a varlena
            // at 1 GB; this is the same guard one step later.
            if buf.len() > MAX_TOASTED_VALUE {
                return Err(StorageError::ValueTooBig {
                    size: buf.len(),
                    max: MAX_TOASTED_VALUE,
                });
            }
            widths.push(buf.len());
            // Only a variable-length type can be wide enough to be worth moving;
            // `typlen == -1` is exactly PostgreSQL's varlena predicate.
            toastable.push(schema.columns.get(i).is_some_and(|c| c.ty.typlen() == -1));
            encoded.push(buf);
        }
        let base = bytes.len() - widths.iter().sum::<usize>();
        let chosen = toast::plan(
            &widths,
            &toastable,
            base,
            toast::TOAST_TUPLE_TARGET,
            MAX_TUPLE,
        )?;
        if chosen.is_empty() {
            // Nothing was worth moving and the row still fits: keep it inline.
            return Ok(Planned::Inline(bytes));
        }
        Ok(Planned::Toast { chosen, encoded })
    }

    /// Carry out a [`Planned`]: write any chunk chains and return the bytes
    /// `place` will store.
    ///
    /// This is the side-effecting half. `update` calls it only after it has won
    /// the right to replace the row, so a lost race writes no chunks — chunks with
    /// no heap tuple naming them are unreachable by VACUUM and would leak forever.
    fn write_planned(
        &self,
        tuple: &Tuple,
        hdr: &TupleHeader,
        planned: Planned,
        txn: &TxnContext,
    ) -> Result<Vec<u8>, StorageError> {
        let (chosen, encoded) = match planned {
            Planned::Inline(bytes) => return Ok(bytes),
            Planned::Toast { chosen, encoded } => (chosen, encoded),
        };
        let toast_rel = self.ensure_toast_rel()?;
        let mut attrs: Vec<tuple::Attr<'_>> = tuple.iter().map(tuple::Attr::Inline).collect();
        for i in chosen {
            let first = self.write_chain(toast_rel, &encoded[i], txn);
            attrs[i] = tuple::Attr::External(toast::ToastPointer {
                rel: toast_rel,
                first,
                // Bounded by `MAX_TOASTED_VALUE` in `plan_tuple`, so the narrowing
                // cannot wrap.
                rawsize: encoded[i].len() as u32,
            });
        }
        let bytes = tuple::encode_tuple(&attrs, hdr, PLACEHOLDER_TID);
        // `plan` computed this from the same widths, so a miss means the two
        // disagree — a bug here, not a user error.
        debug_assert!(
            bytes.len() <= MAX_TUPLE,
            "toasting did not shrink the tuple"
        );
        if bytes.len() > MAX_TUPLE {
            return Err(StorageError::RowTooBig {
                size: bytes.len(),
                max: MAX_TUPLE,
            });
        }
        Ok(bytes)
    }

    /// Place `bytes` onto a page of `rel`, log a HEAP_INSERT, and return its tid.
    /// With `self_link`, the item's `ctid` field is patched to its own tid once
    /// placed — what a heap tuple wants, and what the last chunk of a toast chain
    /// wants to terminate the walk.
    ///
    /// The caller must have sized `bytes` at or below `MAX_TUPLE`: anything
    /// larger fits no page, so the extend-and-retry loop would never terminate.
    fn place_item(
        &self,
        rel: RelFileNode,
        xid: Xid,
        bytes: &[u8],
        hint: &AtomicU32,
        self_link: bool,
    ) -> Tid {
        debug_assert!(bytes.len() <= MAX_TUPLE, "item was not sized");
        let smgr = self.engine.bufpool.smgr();
        loop {
            let nblocks = Self::io(smgr.nblocks(rel));
            let target = if nblocks == 0 {
                Self::io(smgr.extend(rel))
            } else {
                hint.load(Ordering::Relaxed).min(nblocks - 1)
            };
            let page = Self::io(self.engine.bufpool.pin(rel, target));
            let placed = page.modify(|pg| {
                let off = page::add_item(pg, bytes)?;
                let tid = Tid {
                    block: target,
                    offset: off,
                };
                if self_link {
                    let Some(item) = page::get_item_mut(pg, off) else {
                        panic!("newly inserted tuple is missing from its page");
                    };
                    tuple::set_ctid(item, tid);
                }
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
                hint.store(target, Ordering::Relaxed);
                return tid;
            }
            // Page full: extend a fresh block and retry there.
            let fresh = Self::io(smgr.extend(rel));
            hint.store(fresh, Ordering::Relaxed);
        }
    }

    /// Place a heap tuple: [`HeapTable::place_item`] with the self-ctid patch.
    fn place(&self, rel: RelFileNode, xid: Xid, tuple_bytes: &[u8]) -> Tid {
        self.place_item(rel, xid, tuple_bytes, &self.insert_hint, true)
    }

    /// Free every chunk of each chain, block by block, logging a HEAP_VACUUM per
    /// page touched. Called only from [`TableAm::vacuum`], once the rows that
    /// named these chains are gone.
    ///
    /// The walk reads each chunk's link before freeing it, so it collects the
    /// whole chain first and frees per block afterwards — freeing as it went
    /// would destroy the link it still needs.
    fn free_chains(&self, chains: &[toast::ToastPointer]) {
        if chains.is_empty() {
            return;
        }
        let mut per_block: std::collections::HashMap<(u32, u32), Vec<u16>> =
            std::collections::HashMap::new();
        let smgr = self.engine.bufpool.smgr();
        for p in chains {
            // Same bounds check as `detoast`, and for the same reason: pinning an
            // out-of-range block extends the relation to reach it.
            let Ok(nblocks) = smgr.nblocks(p.rel) else {
                continue;
            };
            let mut at = p.first;
            let mut seen = 0usize;
            loop {
                if at.block >= nblocks {
                    break;
                }
                let Ok(page) = self.engine.bufpool.pin(p.rel, at.block) else {
                    break;
                };
                let Some(next) = page.read(|pg| {
                    page::get_item(pg, at.offset)
                        .and_then(|bytes| tuple::decode_chunk(bytes).map(|(next, _)| next))
                }) else {
                    break;
                };
                per_block
                    .entry((p.rel.0, at.block))
                    .or_default()
                    .push(at.offset);
                seen += 1;
                // Self-link terminates the chain. The count is a backstop against
                // a corrupt link cycling forever: no value has more chunks than
                // its own byte count.
                if next == at || seen > p.rawsize as usize {
                    break;
                }
                at = next;
            }
        }
        for ((rel, block), mut offs) in per_block {
            offs.sort_unstable();
            offs.dedup();
            let rel = RelFileNode(rel);
            let Ok(page) = self.engine.bufpool.pin(rel, block) else {
                continue;
            };
            page.modify(|pg| {
                for &off in &offs {
                    page::set_flags(pg, off, page::LP_UNUSED);
                }
                page::compact(pg);
                let lsn = self.log(
                    rec::HEAP_VACUUM,
                    Xid::INVALID,
                    &rec::vacuum(rel, block, &offs),
                );
                page::set_lsn(pg, lsn.0);
            });
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

/// Whether a version is dead to every possible snapshot, so VACUUM may reclaim
/// it and anything it owns out of line.
///
/// Two ways to be dead. A version a committed transaction *deleted* below the
/// horizon is the ordinary case. A version whose *inserter* aborted below the
/// horizon is the other, and it matters much more now than it did: before
/// out-of-line storage an aborted insert wasted at most one page-sized tuple,
/// whereas a rolled-back wide INSERT strands its whole value — so keying only on
/// `xmax`, as this did, leaked unboundedly with no path to reclaim it.
fn is_reclaimable(hdr: &TupleHeader, oldest: Xid, clog: &Clog) -> bool {
    let deleted = hdr.xmax.is_valid() && hdr.xmax < oldest && clog.is_committed(hdr.xmax);
    let never_born =
        hdr.xmin.is_valid() && hdr.xmin < oldest && clog.status(hdr.xmin) == XactStatus::Aborted;
    deleted || never_born
}

/// A version is still updatable/deletable unless a committed transaction deleted
/// it (an aborted or in-flight deleter leaves it live).
fn is_live(hdr: &TupleHeader, clog: &Clog) -> bool {
    !hdr.xmax.is_valid() || clog.status(hdr.xmax) == XactStatus::Aborted
}

/// Walk a toast chain and return the bytes it holds.
///
/// Chunk visibility is deliberately **not** checked. The heap tuple that names
/// this chain has already been found visible, and its chunks must stay readable
/// for exactly as long as it does — including after a DELETE stamps it, for
/// snapshots older than the deleter. Reclamation is VACUUM's job, and it only
/// runs once no snapshot can reach the owning tuple.
///
/// A chain is bounded by `rawsize`, so a corrupt link cannot spin forever: the
/// walk stops as soon as it has collected the bytes it was promised, and
/// disagreeing with that promise is an error rather than a short value.
fn detoast(engine: &EngineInner, p: &toast::ToastPointer) -> Result<Vec<u8>, StorageError> {
    let smgr = engine.bufpool.smgr();
    let nblocks = smgr
        .nblocks(p.rel)
        .map_err(|e| StorageError::Io(e.to_string()))?;
    let mut out: Vec<u8> = Vec::with_capacity(p.rawsize as usize);
    let mut at = p.first;
    // A chunk carries at least one payload byte, so a chain can never be longer
    // than the value it holds. Bounding the hops as well as the bytes stops a
    // cycle of empty chunks — which advances neither `out.len()` nor the walk —
    // from spinning forever.
    let mut hops = 0usize;
    loop {
        // Bounds-check before pinning: `BufferPool::pin` *extends the relation*
        // to cover an out-of-range block, so a garbage link would otherwise turn
        // one read into an attempt to grow the file to that block.
        if at.block >= nblocks {
            return Err(toast::corrupt_chain(p, out.len()));
        }
        let page = engine
            .bufpool
            .pin(p.rel, at.block)
            .map_err(|e| StorageError::Io(e.to_string()))?;
        let step = page.read(|pg| {
            page::get_item(pg, at.offset)
                .and_then(tuple::decode_chunk)
                .map(|(next, payload)| (next, payload.to_vec()))
        });
        let Some((next, payload)) = step else {
            return Err(toast::corrupt_chain(p, out.len()));
        };
        out.extend_from_slice(&payload);
        hops += 1;
        // The last chunk links to itself.
        if next == at || out.len() >= p.rawsize as usize {
            break;
        }
        if hops > p.rawsize as usize {
            return Err(toast::corrupt_chain(p, out.len()));
        }
        at = next;
    }
    if out.len() != p.rawsize as usize {
        return Err(toast::corrupt_chain(p, out.len()));
    }
    Ok(out)
}

impl TableAm for HeapTable {
    fn schema(&self) -> Arc<TableSchema> {
        self.snap()
    }

    fn indexes(&self) -> Vec<IndexMetadata> {
        self.indexes
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .iter()
            .map(|e| e.meta.clone())
            .collect()
    }

    /// Size estimates from the storage manager: `nblocks` is the file length
    /// divided by the page size, so this stays O(1) and needs no scan.
    ///
    /// Reports the **committed** relfilenode, not the one a transaction with a
    /// staged TRUNCATE would read: statistics are relation-wide metadata, and
    /// an uncommitted truncation has not changed the relation's size for anyone
    /// else yet. An I/O error yields "nothing known" rather than a panic —
    /// statistics are advisory, and no query result depends on them.
    fn statistics(&self) -> RelStats {
        // An ANALYZE result supersedes the size-derived guess: it counted the
        // rows rather than inferring them from an assumed row width.
        if let Some((relpages, reltuples)) = *self
            .analyzed
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
        {
            return RelStats {
                relpages,
                reltuples,
                analyzed: true,
                columns: Vec::new(),
            };
        }
        let schema = self.snap();
        match self.nblocks() {
            Ok(nblocks) => RelStats::from_pages(nblocks, &schema),
            Err(_) => RelStats::unknown(&schema),
        }
    }

    /// The chunk store's size, or `None` until a row has needed one. Only the
    /// page count is meaningful: chunks are not rows, so reporting a tuple count
    /// would invite it to be read as one.
    fn toast_statistics(&self) -> Option<RelStats> {
        let rel = self.toast_relfilenode()?;
        let relpages = self.engine.bufpool.smgr().nblocks(rel).unwrap_or(0);
        Some(RelStats {
            relpages,
            reltuples: 0.0,
            analyzed: false,
            columns: Vec::new(),
        })
    }

    fn supports_index_scan(&self, index_name: &str) -> bool {
        let schema = self.snap();
        self.indexes
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .iter()
            .any(|e| e.meta.name == index_name && e.is_physical(&schema))
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
    ) -> Option<IndexProbe> {
        // Bound before the index guard: the filter and the key encoding below
        // must agree on `columns`.
        let schema = self.snap();
        let (rel, latch, cols) = {
            let indexes = self
                .indexes
                .read()
                .unwrap_or_else(|_| panic!("rwlock poisoned"));
            let entry = indexes.iter().find(|e| e.meta.name == index_name)?;
            if !entry.is_physical(&schema) {
                return None;
            }
            (
                // The staged tree when `txn` is the truncating transaction, so a
                // probe agrees with a scan of the file it reads.
                self.effective_index_rel(entry, txn.xid),
                Arc::clone(&entry.latch),
                btkey::key_columns(&entry.meta.keys),
            )
        };
        let Some(kb) = btkey::encode_values(&schema, &cols, key) else {
            // A NULL (or otherwise un-encodable) probe key: served, no match.
            return Some(Box::new(std::iter::empty()));
        };
        let tids = BTree::open(
            Arc::clone(&self.engine),
            rel,
            latch,
            self.persistence.is_unlogged(),
        )
        .search_equal(&kb);
        let mut out = Vec::new();
        for tid in tids {
            // `fetch` can fail now that a value may live out of line, and the
            // failure must reach the caller: swallowing it here would make an
            // index scan quietly return fewer rows than a sequential scan of the
            // same table — the same query answering differently by plan.
            match self.fetch(tid, txn) {
                Ok(Some(tuple)) => out.push(Ok((tid, tuple))),
                // Not visible to this snapshot: not an error.
                Ok(None) => {}
                Err(error) => out.push(Err(error)),
            }
        }
        Some(Box::new(out.into_iter()))
    }

    /// The heap stores whole tuples per page, so reading fewer columns saves no
    /// I/O: the projection is ignored, which the scan contract permits.
    fn scan(&self, txn: &TxnContext, _projection: &ColumnProjection) -> TupleStream {
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

    fn fetch(&self, tid: Tid, txn: &TxnContext) -> Result<Option<Tuple>, StorageError> {
        let _guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        let smgr = self.engine.bufpool.smgr();
        if tid.block >= Self::io(smgr.nblocks(rel)) {
            return Ok(None);
        }
        let page = Self::io(self.engine.bufpool.pin(rel, tid.block));
        // Decode under the page's frame lock, but reassemble any out-of-line
        // attribute after it drops: detoasting pins pages of another relation,
        // which must never happen while this one is held.
        let raw = page.read(|pg| {
            let bytes = page::get_item(pg, tid.offset)?;
            let head = tuple::decode_header(bytes);
            satisfies_mvcc(&head.hdr, &txn.snapshot, &txn.clog, txn.xid, txn.cid)
                .then(|| tuple::decode_raw(bytes))
        });
        match raw {
            Some(raw) => Ok(Some(raw?.resolve(|p| detoast(&self.engine, p))?)),
            None => Ok(None),
        }
    }

    fn insert(&self, tuple: Tuple, txn: &TxnContext) -> Result<Tid, StorageError> {
        let _guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        let hdr = TupleHeader::inserted(txn.xid, txn.cid);
        let planned = self.plan_tuple(&tuple, &hdr)?;
        let bytes = self.write_planned(&tuple, &hdr, planned, txn)?;
        let tid = self.place(rel, txn.xid, &bytes);
        self.maintain_insert(&tuple, tid, txn.xid);
        Ok(tid)
    }

    fn update(
        &self,
        tid: Tid,
        tuple: Tuple,
        txn: &TxnContext,
    ) -> Result<UpdateResult, StorageError> {
        let _guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        // DECIDE the new version's layout before stamping the old one deleted, but
        // WRITE it only after. Both halves matter and they pull opposite ways:
        //
        //  - The stamp is not undoable within the statement, so a row whose new
        //    version is too big must fail with the old version still live —
        //    otherwise the update would delete the row and report an error, losing
        //    it on commit. Hence the size check runs first.
        //  - Chunks written before the stamp are orphaned when the stamp loses the
        //    race: no heap tuple names them, and VACUUM only reaches chains through
        //    a heap tuple, so they would leak permanently. Hence the write runs
        //    after.
        //
        // `plan_tuple` is pure, so running it first costs nothing if we then lose.
        let hdr = TupleHeader::inserted(txn.xid, txn.cid);
        let planned = self.plan_tuple(&tuple, &hdr)?;
        // Stamp the old version deleted-by-us, atomically under its page lock
        // (`stamp_deleted` is the serialization point). Two concurrent updaters of
        // the same row therefore serialize: the loser sees xmax already set, gets
        // `false`, and inserts no new version — so the row never ends up with two
        // live successors. Only after winning that race do we place the new
        // version.
        if !self.stamp_deleted(rel, tid, txn) {
            return Ok(UpdateResult::NotFound);
        }
        // The old tuple's forward ctid is left pointing at itself; the
        // update-chain link is only consumed by EvalPlanQual, which is deferred
        // (P6).
        //
        // Index only the new version's key; the old version's entry stays and is
        // filtered by MVCC at probe time (reclaimed by vacuum), matching the
        // memory engine's append-only maintenance.
        let new_bytes = self.write_planned(&tuple, &hdr, planned, txn)?;
        let new_tid = self.place(rel, txn.xid, &new_bytes);
        self.maintain_insert(&tuple, new_tid, txn.xid);
        Ok(UpdateResult::Updated)
    }

    fn delete(&self, tid: Tid, txn: &TxnContext) -> Result<DeleteResult, StorageError> {
        let _guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        if self.stamp_deleted(rel, tid, txn) {
            Ok(DeleteResult::Deleted)
        } else {
            Ok(DeleteResult::NotFound)
        }
    }

    /// Transactional TRUNCATE via relfilenode swap (PostgreSQL's mechanism).
    /// Stages a fresh empty file and holds the table exclusively until the
    /// transaction ends; the swap is applied on commit and discarded on abort by
    /// the [`crabgresql_txn::TxnFinalize`] hook (`PgEngine::on_commit`/`on_abort`).
    /// The old file stays intact until commit, so a rollback or crash-before-commit
    /// restores every row.
    ///
    /// Every physical index is swapped the same way, in the same record and by
    /// the same commit verdict. Carrying an index across the swap would corrupt
    /// it rather than merely age it: the new heap file starts at block 0 with the
    /// insert hint reset, so rows inserted after the TRUNCATE occupy exactly the
    /// tids the old index entries name, and a probe would return live, visible
    /// rows whose key does not match — which the executor's index scan does not
    /// re-check.
    fn truncate(&self, txn: &TxnContext) -> Result<(), StorageError> {
        // AccessExclusiveLock: block concurrent readers/writers of this table
        // until we commit, so no one reads the old file we are about to unlink or
        // writes rows that the swap would drop. Held until txn end.
        self.lock.acquire_exclusive(txn.lock_owner);
        // One snapshot for the statement: the WAL record and the finalize-hook
        // registration below must name the same version of this relation.
        let schema = self.snap();
        let old = self.effective_rel(txn.xid);
        // A fresh, never-reused relfilenode for the empty post-truncate file. A
        // RAM-backed (`Temporary`) table's replacement must also be RAM-backed.
        let new = self.engine.catalog.alloc_relfilenode();
        if self.persistence.is_ram_backed() {
            self.engine.bufpool.smgr().register_memory(new);
        }
        Self::io(self.engine.bufpool.smgr().create_if_missing(new));
        // An empty replacement tree per physical index, staged the same way.
        //
        // Deliberately *outside* the `CheckpointDelay` block below: `BTree::create`
        // takes a barrier of its own, and nesting the two deadlocks against a
        // checkpointer that raises `wanted` in between. Creating first is safe for
        // the same reason `create_if_missing(new)` above is — a crash before the
        // record is appended leaves files nothing names, which the startup orphan
        // sweep reclaims.
        let staged_indexes: Vec<(String, RelFileNode, RelFileNode)> = {
            let mut indexes = self
                .indexes
                .write()
                .unwrap_or_else(|_| panic!("rwlock poisoned"));
            let mut staged = Vec::new();
            for entry in indexes.iter_mut() {
                if !entry.is_physical(&schema) {
                    continue;
                }
                let irel = self.engine.catalog.alloc_relfilenode();
                if self.persistence.is_ram_backed() {
                    self.engine.bufpool.smgr().register_memory(irel);
                }
                Self::io(self.engine.bufpool.smgr().create_if_missing(irel));
                BTree::open(
                    Arc::clone(&self.engine),
                    irel,
                    Arc::clone(&entry.latch),
                    self.persistence.is_unlogged(),
                )
                .create();
                staged.push((entry.meta.name.clone(), entry.rel, irel));
                entry.staged = Some(irel);
            }
            staged
        };
        // Everything from the append to the last piece of state a checkpoint reads
        // runs under one barrier: this is a writer that logs a record and only
        // afterwards publishes what decides whether the record must be replayed,
        // which is precisely what `CheckpointDelay` exists for. Without it a
        // checkpoint can sample a redo point above the record while
        // `truncate_unreconciled` still reads false, and a crash after the commit
        // then loses the swap.
        //
        // A block expression, not a function-scope `let _delay`: at function scope
        // the barrier would also cover `discard_relfile` below, which does file
        // I/O. `let _ = ...` would be worse still — it drops the guard immediately
        // and silently disarms this.
        //
        // Deliberately started *after* `acquire_exclusive`, which can block until
        // every foreign reader drains, and after the relfilenode allocation and
        // `create_if_missing`: no record exists yet, and holding the barrier across
        // an unbounded wait would stall the checkpointer for no benefit.
        let prev = {
            // WAL-log the swap intent {old, new, table} and flush it. Recovery
            // applies the swap only for a committed XID, so the record is safe to
            // write now. A WAL-skipped table (`Unlogged`/`Temporary`) writes no
            // such record (an Unlogged table's data is reset on crash instead) and
            // so needs neither the barrier nor the pin — the same `wal_skipped`
            // gate `nbtree` uses.
            let _delay =
                (!self.persistence.is_wal_skipped()).then(|| self.engine.wal.delay_checkpoint());
            if !self.persistence.is_wal_skipped() {
                let lsn = self.engine.wal.append(
                    RmgrId::HEAP,
                    rec::HEAP_TRUNCATE,
                    txn.xid,
                    &rec::truncate(&schema.namespace, &schema.name, old, new, &staged_indexes),
                );
                Self::io(
                    self.engine
                        .wal
                        .flush(lsn.end)
                        .map_err(std::io::Error::other),
                );
                // Only now that the record is durable: a failed flush must leave
                // nothing pinned.
                self.truncate_unreconciled.store(true, Ordering::Release);
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
                    new_indexes: staged_indexes
                        .iter()
                        .map(|(name, _, irel)| (name.clone(), *irel))
                        .collect(),
                    owner: txn.lock_owner,
                });
            self.has_pending.store(true, Ordering::Release);
            prev
        };
        self.insert_hint.store(0, Ordering::Relaxed);
        match prev {
            Some(prev) => {
                // The superseded staged files were used only by this uncommitted
                // transaction; reclaim them now — the heap's and every index's.
                self.engine.discard_relfile(RelFileNode(prev.new_rel));
                for (_, irel) in prev.new_indexes {
                    self.engine.discard_relfile(irel);
                }
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
                    .push((schema.namespace.clone(), schema.name.clone()));
            }
        }
        Ok(())
    }

    fn vacuum(&self, oldest: Xid, clog: &Clog) {
        // Vacuum reclaims committed-dead versions from the committed file.
        let _guard = self.lock.acquire_shared(LockOwner::INTERNAL);
        // One snapshot for the whole pass: the index set is filtered and every
        // victim's key encoded against the same `columns`, and the
        // victims x indexes loop below takes no schema lock at all.
        let schema = self.snap();
        let rel = RelFileNode(self.live_rel.load(Ordering::Relaxed));
        // Snapshot the physical indexes so each reclaimed version's index entry
        // can be removed too — otherwise a stale `key -> tid` would point at a
        // heap slot that a later insert reuses, yielding wrong rows.
        let phys: Vec<(RelFileNode, Arc<RwLock<()>>, Vec<usize>)> = self
            .indexes
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .iter()
            .filter(|e| e.is_physical(&schema))
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
            type Victim = (Tid, Result<tuple::RawTuple, StorageError>);
            let (freed, victims): (Vec<u16>, Vec<Victim>) = page.read(|pg| {
                let mut offs = Vec::new();
                let mut victims = Vec::new();
                for off in 1..=page::max_offset(pg) {
                    if let Some(bytes) = page::get_item(pg, off) {
                        let head = tuple::decode_header(bytes);
                        if is_reclaimable(&head.hdr, oldest, clog) {
                            offs.push(off);
                            // Decode when there are index entries to remove, or
                            // when the row owns chunks to reclaim. `has_external`
                            // keeps the common case free.
                            if !phys.is_empty() || head.has_external {
                                victims
                                    .push((Tid { block, offset: off }, tuple::decode_raw(bytes)));
                            }
                        }
                    }
                }
                (offs, victims)
            });
            if freed.is_empty() {
                continue;
            }
            // The chains these dead rows own, collected before the tuples go away.
            // Reading the pointers needs no chunk I/O at all.
            let dead_chains: Vec<toast::ToastPointer> = victims
                .iter()
                .filter_map(|(_, raw)| raw.as_ref().ok())
                .flat_map(|raw: &tuple::RawTuple| raw.external().iter().map(|(_, p)| *p))
                .collect();
            // Reassemble values ONLY to delete index entries. With no physical
            // index there is nothing to delete, so a table of wide rows is
            // vacuumed without reading a single chunk back.
            let victims: Vec<(Tid, Tuple)> = if phys.is_empty() {
                Vec::new()
            } else {
                victims
                    .into_iter()
                    .filter_map(|(tid, raw)| {
                        match raw.and_then(|raw| raw.resolve(|p| detoast(&self.engine, p))) {
                            Ok(vals) => Some((tid, vals)),
                            // Vacuum is advisory maintenance and cannot report an
                            // error to anyone, but it must not abort the process
                            // either: skip this row's index cleanup and leave its
                            // slot for a later pass.
                            Err(error) => {
                                tracing::warn!(
                                    table = %schema.name,
                                    %error,
                                    "VACUUM: could not read a dead row; \
                                     leaving its index entries for a later pass"
                                );
                                None
                            }
                        }
                    })
                    .collect()
            };
            // Remove each reclaimed version's index entries BEFORE freeing the heap
            // slots (PostgreSQL's two-pass order). If the slot were freed first, a
            // concurrent insert could reuse the offset before its stale `key -> tid`
            // entry is gone, and a probe would then return the reused row for the
            // old key. Deleting entries first closes that window.
            for (tid, tuple) in &victims {
                for (irel, latch, cols) in &phys {
                    if let Some(key) = btkey::encode_row(&schema, cols, tuple) {
                        BTree::open(
                            Arc::clone(&self.engine),
                            *irel,
                            Arc::clone(latch),
                            self.persistence.is_unlogged(),
                        )
                        .delete(&key, *tid);
                    }
                }
            }
            page.modify(|pg| {
                for &off in &freed {
                    page::set_flags(pg, off, page::LP_UNUSED);
                }
                page::compact(pg);
                let lsn = self.log(
                    rec::HEAP_VACUUM,
                    Xid::INVALID,
                    &rec::vacuum(rel, block, &freed),
                );
                page::set_lsn(pg, lsn.0);
            });
            // Reclaim the chunks those rows owned — strictly AFTER the heap slots
            // are freed. The other order loses data: a crash between freeing the
            // chunks and freeing the tuples would leave the dead tuples still
            // selectable as victims, and the next vacuum would walk their pointers
            // into chunk slots a later toast write had since reused, freeing live
            // chunks. This way a crash in the gap leaks chunks nothing references,
            // which the next full vacuum cannot even see — a leak, never
            // corruption.
            self.free_chains(&dead_chains);
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
    type Item = Result<(Tid, Tuple), StorageError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.buf_idx < self.buffer.len() {
                let row = self.buffer[self.buf_idx].clone();
                self.buf_idx += 1;
                return Some(Ok(row));
            }
            if self.cur_block >= self.nblocks {
                return None;
            }
            let block = self.cur_block;
            self.cur_block += 1;
            self.buffer.clear();
            self.buf_idx = 0;
            let page = HeapTable::io(self.engine.bufpool.pin(self.rel, block));
            let raw: Vec<(Tid, Result<tuple::RawTuple, StorageError>)> = page.read(|pg| {
                let mut out = Vec::new();
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
                            out.push((Tid { block, offset: off }, tuple::decode_raw(bytes)));
                        }
                    }
                }
                out
            });
            // Detoast only after the frame lock is released — the scan already
            // buffers a whole block before yielding, so this costs no extra pass.
            for (tid, t) in raw {
                match t.and_then(|t| t.resolve(|p| detoast(&self.engine, p))) {
                    Ok(vals) => self.buffer.push((tid, vals)),
                    // Stop the scan rather than skipping the rest of the block: a
                    // consumer that keeps polling must not silently receive a
                    // partial relation.
                    Err(e) => {
                        self.cur_block = self.nblocks;
                        self.buffer.clear();
                        self.buf_idx = 0;
                        return Some(Err(e));
                    }
                }
            }
        }
    }
}
