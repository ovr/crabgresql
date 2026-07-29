//! Transaction core: XIDs, commit log (CLOG), snapshots and the single MVCC
//! visibility rule shared by every storage engine.
//!
//! This crate is the *core* side of the storage contract described in
//! `docs/ARCHITECTURE.md §1.3`: **snapshots and XIDs are the core's job; storing
//! versions and answering visibility is the engine's job.** An engine keeps a
//! [`TupleHeader`] per version and asks [`satisfies_mvcc`] whether a version is
//! visible to a given [`Snapshot`] — so the visibility semantics live in exactly
//! one place and `crabgresql-pg-engine` reuses them for both durable heap tables
//! and RAM-backed memory tables.
//!
//! The rule reproduces PostgreSQL's observable `HeapTupleSatisfiesMVCC`
//! behaviour (a version is visible when its inserter is committed-and-in-snapshot
//! and its deleter is not); it is written independently from PG's C source per
//! the clean-room policy in `AGENTS.md`.

pub mod clog;
mod lock;

pub use lock::{ExclusiveGuard, SharedGuard, TableLock};

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A transaction identifier. Unlike PostgreSQL's 32-bit XIDs this is 64-bit, a
/// deliberate deviation (see `docs/ARCHITECTURE.md §5`): no wraparound, no
/// freeze, no anti-wraparound VACUUM.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Xid(pub u64);

impl Xid {
    /// The invalid/unset XID (`InvalidTransactionId`). A live tuple that has
    /// never been deleted carries this in `xmax`.
    pub const INVALID: Xid = Xid(0);
    /// A frozen inserter: always treated as committed and visible to every
    /// snapshot. Reserved for a future VACUUM FREEZE; unused by the in-memory
    /// engine, but the visibility rule honours it.
    pub const FROZEN: Xid = Xid(2);
    /// The first XID handed out to a real transaction. Values below this are
    /// reserved (`INVALID`, `FROZEN`), mirroring PG's `FirstNormalTransactionId`.
    pub const FIRST_NORMAL: Xid = Xid(3);

    pub fn is_valid(self) -> bool {
        self != Xid::INVALID
    }
}

/// A command counter within one transaction: statement N of a transaction sees
/// rows written by commands `< cid` but not by itself or later commands, which
/// is how a single `UPDATE` avoids re-processing rows it just wrote (the
/// "Halloween problem").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CommandId(pub u32);

impl CommandId {
    pub const FIRST: CommandId = CommandId(0);
}

/// Commit state of a transaction, as recorded in the [`Clog`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XactStatus {
    /// Still running, or never recorded (the default for an unknown XID).
    InProgress,
    Committed,
    Aborted,
    /// A subtransaction that committed to its parent but whose top-level
    /// transaction has not yet committed. Reserved for savepoints (P6).
    SubCommitted,
}

impl XactStatus {
    /// The on-disk encoding, two bits wide.
    ///
    /// `InProgress == 0` is load-bearing: a file hole, a never-written page and a
    /// missing segment all read as zeros, and every one of them must mean
    /// "unknown, so assume still running".
    pub const fn to_bits(self) -> u8 {
        match self {
            XactStatus::InProgress => 0b00,
            XactStatus::Committed => 0b01,
            XactStatus::Aborted => 0b10,
            XactStatus::SubCommitted => 0b11,
        }
    }

    /// The inverse of [`XactStatus::to_bits`]. Total by construction: two bits
    /// have four values and the enum has four variants, so there is no error path
    /// and nothing to unwrap. Bits above the low two are ignored.
    pub const fn from_bits(bits: u8) -> Self {
        match bits & 0b11 {
            0b00 => XactStatus::InProgress,
            0b01 => XactStatus::Committed,
            0b10 => XactStatus::Aborted,
            _ => XactStatus::SubCommitted,
        }
    }
}

/// Hint bits cached on a version so visibility need not re-consult the CLOG once
/// a transaction's fate is known. Correctness never depends on them — they are a
/// cache the engine may set lazily — so the in-memory engine leaves them clear
/// and [`satisfies_mvcc`] falls back to the CLOG.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Infomask(pub u16);

impl Infomask {
    pub const XMIN_COMMITTED: u16 = 1 << 0;
    pub const XMIN_INVALID: u16 = 1 << 1;
    pub const XMAX_COMMITTED: u16 = 1 << 2;
    pub const XMAX_INVALID: u16 = 1 << 3;

    pub fn contains(self, bit: u16) -> bool {
        self.0 & bit != 0
    }
}

/// The MVCC bookkeeping an engine stores alongside each row version. In the heap
/// engine these fields live in the on-page tuple header; in the memory engine
/// they sit beside the tuple in a `Vec`.
#[derive(Clone, Copy, Debug)]
pub struct TupleHeader {
    /// Inserting transaction.
    pub xmin: Xid,
    /// Deleting transaction, or [`Xid::INVALID`] while the version is live.
    pub xmax: Xid,
    /// Command that created the version (visibility of own inserts).
    pub cmin: CommandId,
    /// Command that deleted the version (visibility of own deletes); only
    /// meaningful once `xmax` is set.
    pub cmax: CommandId,
    pub infomask: Infomask,
}

impl TupleHeader {
    /// A freshly inserted, live version stamped by `(xid, cid)`.
    pub fn inserted(xid: Xid, cid: CommandId) -> Self {
        TupleHeader {
            xmin: xid,
            xmax: Xid::INVALID,
            cmin: cid,
            cmax: CommandId::FIRST,
            infomask: Infomask::default(),
        }
    }
}

/// The resident page set, plus what still needs writing.
#[derive(Debug)]
struct ClogCache {
    /// Resident pages, keyed by page number.
    ///
    /// There is no eviction. A page is 8 KiB per 32768 transactions — 256 KiB per
    /// million — and evicting would mean writing a page outside a checkpoint,
    /// which is precisely what the write-back model exists to avoid. Truncation
    /// is the only thing that drops pages.
    pages: std::collections::HashMap<u64, clog::ClogPage>,
    /// Pages stamped since the last flush.
    dirty: BTreeSet<u64>,
    /// Every XID below this has been frozen out of every relation; its segment
    /// may be gone. Zero until a freeze sweep advances it.
    floor: Xid,
    /// A read error with nowhere to go: [`Clog::status`] returns a bare
    /// [`XactStatus`]. Latched here and surfaced by [`Clog::flush`], which does
    /// have somewhere to put it. Never silently dropped.
    failure: Option<std::io::Error>,
}

impl Default for ClogCache {
    fn default() -> Self {
        ClogCache {
            pages: std::collections::HashMap::new(),
            dirty: BTreeSet::new(),
            // Xid has no Default, and shouldn't: INVALID is the meaningful zero
            // here, not an arbitrary one.
            floor: Xid::INVALID,
            failure: None,
        }
    }
}

/// The commit log: the authoritative fate of every transaction.
///
/// Pages are cached in RAM and written back at checkpoint. Deliberately *not*
/// written or fsynced per commit: [`TransactionManager::commit`] only calls
/// [`Clog::set_committed`] after [`CommitSink::log_commit`] has fsynced the WAL
/// commit record, so a crash between the two is repaired by replay. A CLOG fsync
/// per commit would buy nothing and cost a synchronous write on the hottest path.
///
/// An XID with no bits recorded reads as [`XactStatus::InProgress`].
#[derive(Debug, Default)]
pub struct Clog {
    /// `None` for an in-memory-only commit log — the memory engine and tests,
    /// where nothing is ever written and the cache is simply the whole log.
    dir: Option<std::path::PathBuf>,
    cache: Mutex<ClogCache>,
}

impl Clog {
    /// An in-memory commit log, lost on restart. Used by the memory engine and by
    /// tests that have no data directory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Open the durable commit log under `<data_dir>/pg_xact`, creating it if
    /// this is a fresh cluster.
    ///
    /// Pages are read lazily, so this only reads `meta`.
    pub fn open(data_dir: &std::path::Path) -> std::io::Result<Self> {
        let dir = data_dir.join(clog::CLOG_SUBDIR);
        std::fs::create_dir_all(&dir)?;
        // Absent meta is a fresh cluster, not an error; a meta we cannot parse is.
        let floor = match clog::read_meta(&dir)? {
            Some(meta) => meta.floor,
            None => {
                // Stamp the marker now rather than at the first floor advance.
                // Its job is to let a future build recognise a layout it cannot
                // address, and a cluster that never truncates would otherwise
                // carry segments with nothing identifying their geometry.
                let fresh = clog::ClogMeta {
                    floor: Xid::INVALID,
                };
                clog::write_meta(&dir, &fresh)?;
                fresh.floor
            }
        };
        Ok(Clog {
            dir: Some(dir),
            cache: Mutex::new(ClogCache {
                floor,
                ..ClogCache::default()
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ClogCache> {
        self.cache
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
    }

    /// Borrow the page holding `xid`, reading it in on a miss.
    ///
    /// A read failure latches the error and yields a zero page, so the caller
    /// sees `InProgress` — the answer that renders a row invisible rather than
    /// resurrecting it. The latch turns the next [`Clog::flush`] into an error.
    fn with_page<R>(
        cache: &mut ClogCache,
        dir: Option<&std::path::Path>,
        xid: Xid,
        f: impl FnOnce(&mut clog::ClogPage) -> R,
    ) -> R {
        let pageno = clog::page_of(xid);
        let page = match cache.pages.entry(pageno) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let page = match dir {
                    Some(dir) => clog::read_page(dir, pageno).unwrap_or_else(|error| {
                        cache.failure.get_or_insert(error);
                        clog::ZERO_PAGE
                    }),
                    None => clog::ZERO_PAGE,
                };
                entry.insert(page)
            }
        };
        f(page)
    }

    pub fn status(&self, xid: Xid) -> XactStatus {
        if xid == Xid::FROZEN {
            return XactStatus::Committed;
        }
        let mut cache = self.lock();
        if xid < cache.floor {
            // Unreachable by construction: the floor only advances once every
            // relation has been swept, so no surviving tuple names an XID below
            // it. InProgress is the fail-safe answer if that ever breaks — it
            // hides a row rather than resurrecting a deleted one.
            debug_assert!(
                !xid.is_valid(),
                "status({xid:?}) below the CLOG floor {:?}",
                cache.floor
            );
            return XactStatus::InProgress;
        }
        let dir = self.dir.as_deref();
        Self::with_page(&mut cache, dir, xid, |page| clog::page_status(page, xid))
    }

    pub fn is_committed(&self, xid: Xid) -> bool {
        self.status(xid) == XactStatus::Committed
    }

    pub fn set_committed(&self, xid: Xid) {
        self.set_status(xid, XactStatus::Committed);
    }

    pub fn set_aborted(&self, xid: Xid) {
        self.set_status(xid, XactStatus::Aborted);
    }

    fn set_status(&self, xid: Xid, status: XactStatus) {
        let mut cache = self.lock();
        let dir = self.dir.as_deref();
        Self::with_page(&mut cache, dir, xid, |page| {
            clog::set_page_status(page, xid, status);
        });
        cache.dirty.insert(clog::page_of(xid));
    }

    /// Write every dirty page and fsync the segments they landed in.
    ///
    /// Called from the checkpoint, which is the only place CLOG pages reach disk.
    /// A no-op for an in-memory commit log.
    pub fn flush(&self) -> std::io::Result<()> {
        let mut cache = self.lock();
        if let Some(error) = cache.failure.take() {
            return Err(error);
        }
        let Some(dir) = self.dir.as_deref() else {
            cache.dirty.clear();
            return Ok(());
        };
        // Take the dirty set up front: on failure the pages stay dirty and the
        // next checkpoint retries them.
        let dirty = std::mem::take(&mut cache.dirty);
        let mut segments = BTreeSet::new();
        for pageno in &dirty {
            let Some(page) = cache.pages.get(pageno) else {
                continue;
            };
            if let Err(error) = clog::write_page(dir, *pageno, page) {
                cache.dirty.extend(dirty.iter().copied());
                return Err(error);
            }
            segments.insert(clog::segment_of_page(*pageno));
        }
        for segno in segments {
            if let Err(error) = clog::sync_segment(dir, segno) {
                cache.dirty.extend(dirty.iter().copied());
                return Err(error);
            }
        }
        Ok(())
    }
}

/// Allocates XIDs and tracks which are still running, so a [`Snapshot`] can
/// record the in-flight set (PG's `ProcArray`).
pub struct XidManager {
    next: AtomicU64,
    active: Mutex<BTreeSet<Xid>>,
}

impl Default for XidManager {
    fn default() -> Self {
        XidManager {
            next: AtomicU64::new(Xid::FIRST_NORMAL.0),
            active: Mutex::new(BTreeSet::new()),
        }
    }
}

impl XidManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Assign a fresh XID and mark it running. Called lazily on a transaction's
    /// first write — read-only transactions never consume an XID.
    ///
    /// The counter bump and the `active` insert happen under one lock so they are
    /// atomic with respect to [`XidManager::take_snapshot`] (which also reads
    /// `next` while holding `active`). Otherwise a snapshot could observe `next`
    /// already advanced past an XID that is not yet published in `active` and
    /// wrongly treat that still-running transaction as finished.
    pub fn allocate(&self) -> Xid {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let xid = Xid(self.next.fetch_add(1, Ordering::SeqCst));
        active.insert(xid);
        xid
    }

    /// Mark an XID no longer running (after its CLOG status is set). Snapshots
    /// taken afterwards no longer list it as in-flight.
    pub fn complete(&self, xid: Xid) {
        self.active
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .remove(&xid);
    }

    /// Build an allocator whose next XID is `next`, used after crash recovery so
    /// the first freshly issued XID sits above every transaction recovered from
    /// the WAL (otherwise a reissued XID would alias a recovered one). No XIDs
    /// are marked running — recovery has already retired every transaction it
    /// replayed.
    pub fn with_next(next: Xid) -> Self {
        XidManager {
            next: AtomicU64::new(next.0.max(Xid::FIRST_NORMAL.0)),
            active: Mutex::new(BTreeSet::new()),
        }
    }

    /// Capture the set of transactions in flight right now. Everything `>= xmax`
    /// had not started; everything `< xmin` had already finished.
    pub fn take_snapshot(&self) -> Snapshot {
        let active = self
            .active
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let xmax = Xid(self.next.load(Ordering::SeqCst));
        let xmin = active.iter().next().copied().unwrap_or(xmax);
        Snapshot {
            xmin,
            xmax,
            xip: active.iter().copied().collect(),
        }
    }
}

/// A point-in-time view of transaction visibility: `[xmin, xmax)` with the
/// still-running XIDs (`xip`) punched out. Mirrors the `(xmin, xmax, xip_list)`
/// of PG's snapshots.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Smallest XID still running when the snapshot was taken; anything below is
    /// guaranteed complete.
    pub xmin: Xid,
    /// First XID not yet assigned; anything `>=` this started after the snapshot.
    pub xmax: Xid,
    /// XIDs that were in flight in `[xmin, xmax)` at snapshot time.
    pub xip: Vec<Xid>,
}

impl Snapshot {
    /// Whether `xid` was still in flight relative to this snapshot — i.e. its
    /// effects must be treated as invisible even if the CLOG now says committed
    /// (it may have committed *after* the snapshot).
    pub fn in_progress(&self, xid: Xid) -> bool {
        if xid >= self.xmax {
            return true;
        }
        if xid < self.xmin {
            return false;
        }
        self.xip.binary_search(&xid).is_ok()
    }
}

/// SQL transaction isolation. `READ UNCOMMITTED` is an alias for `READ COMMITTED`
/// (PG never permits dirty reads); `SERIALIZABLE` reuses `RepeatableRead`
/// visibility until SSI lands in M3.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IsolationLevel {
    #[default]
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

/// Identifies who holds a table-level lock. Stable across a session's statements
/// (both reads, which may carry [`Xid::INVALID`], and writes), so a table AM can
/// let a transaction upgrade its own `AccessShare` hold to `AccessExclusive`
/// (e.g. `TRUNCATE` a table the same session has an open cursor on) without
/// self-deadlocking, exactly as PostgreSQL's lock manager does — while still
/// blocking on *other* owners' holds. The server assigns one per connection via
/// [`TransactionManager::new_lock_owner`]; contexts built without a session
/// default to the transaction's XID, and [`LockOwner::INTERNAL`] is reserved for
/// engine-internal callers such as VACUUM.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LockOwner(pub u64);

impl LockOwner {
    /// Reserved owner for engine-internal work (VACUUM) with no session.
    pub const INTERNAL: LockOwner = LockOwner(0);
    /// Reserved owner for engine-internal DDL (CREATE/DROP INDEX) with no session.
    /// Distinct from [`LockOwner::INTERNAL`] so a DDL's exclusive hold also excludes
    /// a concurrent VACUUM (which runs as `INTERNAL`), and from any session owner
    /// (allocated from a counter seeded at 1 or from an XID `>= 3`), which this
    /// top-of-range sentinel never collides with.
    pub const DDL: LockOwner = LockOwner(u64::MAX);
}

/// What flows into an engine on every scan and write: who is asking (`xid`,
/// `cid`), the snapshot their visibility is judged against, and a handle to the
/// commit log so the engine can resolve other transactions' fates without a
/// second parameter. `xid` is [`Xid::INVALID`] until the transaction's first
/// write allocates one.
#[derive(Clone, Debug)]
pub struct TxnContext {
    pub xid: Xid,
    pub cid: CommandId,
    pub snapshot: Snapshot,
    pub iso: IsolationLevel,
    /// Keeps [`TxnContext::snapshot`] counted as in-use for as long as this
    /// context (or any clone of it) lives, so storage cannot reclaim a version
    /// this reader can still see. `None` only for contexts built by hand in
    /// tests, which hold nothing back.
    pub reservation: Option<Arc<SnapshotGuard>>,
    pub clog: Arc<Clog>,
    /// The session-stable table-lock owner (see [`LockOwner`]). Defaults to the
    /// transaction's XID for contexts built without a session; the server stamps
    /// the connection's owner over it in `build_txn`.
    pub lock_owner: LockOwner,
}

/// The one MVCC visibility rule. A version described by `hdr` is visible to a
/// reader identified by `(my_xid, my_cid)` under snapshot `snap` (consulting
/// `clog` for other transactions' fates) exactly when its inserter counts as
/// committed-and-visible and its deleter does not.
///
/// Reproduces PostgreSQL's observable `HeapTupleSatisfiesMVCC` outcome; written
/// clean-room from the documented semantics.
pub fn satisfies_mvcc(
    hdr: &TupleHeader,
    snap: &Snapshot,
    clog: &Clog,
    my_xid: Xid,
    my_cid: CommandId,
) -> bool {
    // --- Is the inserting transaction's effect visible? ---
    if hdr.xmin == Xid::FROZEN {
        // Frozen inserts are visible to everyone.
    } else if my_xid.is_valid() && hdr.xmin == my_xid {
        // Our own insert: visible only if an earlier command in this
        // transaction created it (not this command or a later one).
        if hdr.cmin >= my_cid {
            return false;
        }
    } else {
        // Another transaction's insert: it must be committed *and* have been
        // complete as of our snapshot.
        if !clog.is_committed(hdr.xmin) || snap.in_progress(hdr.xmin) {
            return false;
        }
    }

    // The insert is visible. --- Has a visible delete removed it? ---
    if !hdr.xmax.is_valid() {
        return true; // never deleted
    }

    if my_xid.is_valid() && hdr.xmax == my_xid {
        // Our own delete hides the row only from the deleting command onward.
        return hdr.cmax >= my_cid;
    }

    // Another transaction's delete removes the row only if that transaction is
    // committed and visible in our snapshot; otherwise the row is still live.
    if clog.is_committed(hdr.xmax) && !snap.in_progress(hdr.xmax) {
        return false;
    }
    true
}

/// Where the transaction manager records the durable fate of a transaction.
///
/// This is the seam that lets durability live in a separate crate without a
/// dependency cycle: `crabgresql-txn` defines the trait, `crabgresql-wal`
/// implements it over its WAL (append a commit/abort record, fsync at commit).
/// When no sink is attached (the in-memory engine, existing tests) commit/abort
/// are purely in-memory, exactly as before.
///
/// `log_commit` is the durability boundary: it must not return `Ok` until the
/// commit record is on stable storage (append + fsync). `log_abort` needs no
/// fsync — a transaction whose commit never reached disk is simply not committed
/// at recovery, and redo-only recovery needs no undo.
pub trait CommitSink: Send + Sync {
    fn log_commit(&self, xid: Xid) -> std::io::Result<()>;
    fn log_abort(&self, xid: Xid);
}

/// A callback the engine registers so it can apply deferred physical work when a
/// transaction's fate is decided — after the [`Clog`] records it. The durable
/// heap engine uses it to commit or discard a relfilenode-swap TRUNCATE (swap in
/// the new file and unlink the old on commit; unlink the new file on abort) and
/// to release the table lock the TRUNCATE held to transaction end.
///
/// This is invoked from [`TransactionManager::commit`]/[`TransactionManager::abort`]
/// *after* the CLOG update, so every finalize path — autocommit statements, an
/// explicit `COMMIT`/`ROLLBACK`, and a dropped session with an open block — fires
/// it uniformly, with no engine handle threaded through the query layer. The
/// commit's WAL fsync (in [`CommitSink::log_commit`]) has already completed by the
/// time `on_commit` runs, so a crash between the two is repaired at recovery. When
/// no hook is attached (the in-memory engine, unit tests) commit/abort behave
/// exactly as before.
pub trait TxnFinalize: Send + Sync {
    fn on_commit(&self, xid: Xid);
    fn on_abort(&self, xid: Xid);
}

/// The core's transaction service, shared by the whole server: it hands out
/// XIDs, records commit/abort in the [`Clog`], and mints [`TxnContext`]s with a
/// fresh snapshot. One instance is shared across all connections (XIDs and the
/// commit log are process-global).
#[derive(Default)]
pub struct TransactionManager {
    xids: XidManager,
    clog: Arc<Clog>,
    /// Durable commit log, when running on a durable engine; `None` keeps the
    /// pre-durability in-memory behavior for the memory engine and unit tests.
    sink: Option<Arc<dyn CommitSink>>,
    /// Engine finalize hook (relfilenode-swap TRUNCATE apply/discard + lock
    /// release); `None` for the memory engine and unit tests.
    finalize: Option<Arc<dyn TxnFinalize>>,
    /// Vends session-stable [`LockOwner`]s. Starts at 1 so no session ever gets
    /// [`LockOwner::INTERNAL`] (0).
    next_lock_owner: AtomicU64,
    /// Snapshots still in use, as a multiset of their reclamation floor keyed by
    /// count.
    ///
    /// A read-only transaction never allocates an XID, so it contributes nothing
    /// to [`Snapshot::xmin`] — yet it can still be reading rows that a later
    /// transaction has deleted. Any storage that reclaims deleted versions needs
    /// to know about those readers, hence this registry.
    live_snapshots: Arc<Mutex<BTreeMap<Xid, usize>>>,
}

/// Keeps a snapshot counted in [`TransactionManager::live_snapshots`] until it is
/// dropped, so reclamation cannot run past a reader that is still using it.
pub struct SnapshotGuard {
    live: Arc<Mutex<BTreeMap<Xid, usize>>>,
    /// The snapshot's [`Snapshot::xmin`], which is the floor it pins.
    ///
    /// Deliberately not `xmax`: a snapshot can still see rows deleted by any XID
    /// in its `xip` list, and the smallest of those is `xmin` by definition.
    /// Pinning `xmax` would leave every `xip` member above the horizon, so a
    /// concurrent vacuum would reclaim exactly the versions this reader is
    /// entitled to keep reading.
    floor: Xid,
}

impl std::fmt::Debug for SnapshotGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotGuard")
            .field("floor", &self.floor)
            .finish_non_exhaustive()
    }
}

impl Drop for SnapshotGuard {
    fn drop(&mut self) {
        let mut live = self
            .live
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        if let Some(count) = live.get_mut(&self.floor) {
            *count -= 1;
            if *count == 0 {
                live.remove(&self.floor);
            }
        }
    }
}

impl TransactionManager {
    pub fn new() -> Self {
        TransactionManager {
            next_lock_owner: AtomicU64::new(1),
            ..Default::default()
        }
    }

    /// Build a durable manager after crash recovery: attach the WAL-backed
    /// [`CommitSink`], reuse the CLOG recovery rebuilt, and seed the XID
    /// allocator above every recovered transaction.
    pub fn new_recovered(sink: Arc<dyn CommitSink>, clog: Arc<Clog>, next_xid: Xid) -> Self {
        TransactionManager {
            xids: XidManager::with_next(next_xid),
            clog,
            sink: Some(sink),
            finalize: None,
            next_lock_owner: AtomicU64::new(1),
            live_snapshots: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Assign a fresh, unique [`LockOwner`] for a connection. The server calls
    /// this once per session; every statement of that session then shares the
    /// owner, so a transaction can upgrade its own `AccessShare` hold on a table
    /// to `AccessExclusive` without self-deadlocking.
    pub fn new_lock_owner(&self) -> LockOwner {
        LockOwner(self.next_lock_owner.fetch_add(1, Ordering::Relaxed))
    }

    /// Attach the engine's [`TxnFinalize`] hook. Called once at startup, after
    /// the engine and manager are built, before any connection is served.
    pub fn set_finalize(&mut self, finalize: Arc<dyn TxnFinalize>) {
        self.finalize = Some(finalize);
    }

    /// The shared commit log (engines consult it through [`TxnContext::clog`]).
    pub fn clog(&self) -> &Arc<Clog> {
        &self.clog
    }

    /// Assign a fresh XID and mark it running. Called on a transaction's first
    /// write.
    pub fn allocate_xid(&self) -> Xid {
        self.xids.allocate()
    }

    /// Register `snapshot` as in use until the returned guard drops, pinning
    /// [`TransactionManager::reclaim_horizon`] at its [`Snapshot::xmin`].
    ///
    /// The server holds one per open transaction and one per statement, so a
    /// read-only `REPEATABLE READ` session — which allocates no XID and so does
    /// not appear in any snapshot's in-flight set — still holds back the horizon.
    ///
    /// Prefer [`Self::freeze_snapshot`] when capturing a fresh snapshot: this
    /// entry point exists for a snapshot that already exists, and leaves the
    /// caller responsible for the gap between capture and registration.
    pub fn register_snapshot(&self, snapshot: &Snapshot) -> SnapshotGuard {
        let mut live = self
            .live_snapshots
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        Self::register_locked(&mut live, Arc::clone(&self.live_snapshots), snapshot)
    }

    /// Take a snapshot and register it in one step.
    ///
    /// The two-step form has a window: between `snapshot()` and
    /// `register_snapshot`, the new reader is in neither the in-flight set (it
    /// holds no XID) nor the registry, so a concurrent [`Self::reclaim_horizon`]
    /// can step over it and reclaim versions the snapshot was just handed the
    /// right to read. Holding the registry lock across the capture closes it.
    pub fn freeze_snapshot(&self) -> (Snapshot, SnapshotGuard) {
        let mut live = self
            .live_snapshots
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        // Lock order is live_snapshots -> the XID set, and nothing takes them the
        // other way round; `reclaim_horizon` releases the registry before it reads
        // the in-flight set.
        let snapshot = self.xids.take_snapshot();
        let guard = Self::register_locked(&mut live, Arc::clone(&self.live_snapshots), &snapshot);
        (snapshot, guard)
    }

    /// Count `snapshot` into an already-locked registry.
    fn register_locked(
        live: &mut BTreeMap<Xid, usize>,
        handle: Arc<Mutex<BTreeMap<Xid, usize>>>,
        snapshot: &Snapshot,
    ) -> SnapshotGuard {
        *live.entry(snapshot.xmin).or_insert(0) += 1;
        SnapshotGuard {
            live: handle,
            floor: snapshot.xmin,
        }
    }

    /// The XID below which a deleted row version is dead to every reader that
    /// exists or can still be created.
    ///
    /// This is the floor for reclaiming versions, and it is deliberately NOT just
    /// `snapshot().xmin`: that only accounts for transactions holding an XID, and
    /// a read-only transaction holds a snapshot without one. Reclaiming above a
    /// live snapshot's floor would delete rows out from under a `REPEATABLE READ`
    /// reader mid-transaction.
    pub fn reclaim_horizon(&self) -> Xid {
        let oldest_reader = self
            .live_snapshots
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .keys()
            .next()
            .copied();
        let running = self.snapshot().xmin;
        match oldest_reader {
            Some(reader) => running.min(reader),
            None => running,
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.xids.take_snapshot()
    }

    /// Record `xid` committed and retire it from the in-flight set. On a durable
    /// manager the commit record is appended and fsynced *before* the in-memory
    /// CLOG is updated, so a crash between the two is repaired by replaying the
    /// commit record; the fsync is the point after which the transaction is
    /// durable. Returns the I/O error if the WAL flush fails.
    pub fn commit(&self, xid: Xid) -> std::io::Result<()> {
        if xid.is_valid() {
            if let Some(sink) = &self.sink
                && let Err(e) = sink.log_commit(xid)
            {
                // A commit whose WAL never reached disk did not happen: abort the
                // transaction so its XID is retired (otherwise it stays in the
                // in-flight set forever, pinning the snapshot xmin horizon) and
                // its versions become dead. Then surface the I/O error.
                self.abort(xid);
                return Err(e);
            }
            self.clog.set_committed(xid);
            self.xids.complete(xid);
            // Fire the engine hook only after the fate is durable (the WAL commit
            // fsynced above) and recorded in the CLOG, so it can apply the
            // relfilenode swap and unlink the old file safely.
            if let Some(finalize) = &self.finalize {
                finalize.on_commit(xid);
            }
        }
        Ok(())
    }

    /// Record `xid` aborted and retire it — its versions become dead with no
    /// undo (the MVCC advantage: an uncommitted version is simply invisible). No
    /// fsync is needed: an abort that never reaches disk is indistinguishable
    /// from a crash before commit, and both leave the versions invisible.
    pub fn abort(&self, xid: Xid) {
        if xid.is_valid() {
            if let Some(sink) = &self.sink {
                sink.log_abort(xid);
            }
            self.clog.set_aborted(xid);
            self.xids.complete(xid);
            // Discard the transaction's uncommitted physical work (a pending
            // TRUNCATE's new file) and release any table lock it held.
            if let Some(finalize) = &self.finalize {
                finalize.on_abort(xid);
            }
        }
    }

    /// Abort `xid` WITHOUT running the engine [`TxnFinalize`] hook. Used only by
    /// `Session::drop` when the thread is already unwinding from a panic: the
    /// hook takes engine locks that a prior panic may have poisoned, and
    /// re-entering them mid-unwind would be a fatal double-panic. Any physical
    /// work the aborted transaction staged (a pending TRUNCATE's new file) is
    /// reclaimed by the engine's orphan GC at the next startup.
    pub fn abort_without_finalize(&self, xid: Xid) {
        if xid.is_valid() {
            if let Some(sink) = &self.sink {
                sink.log_abort(xid);
            }
            self.clog.set_aborted(xid);
            self.xids.complete(xid);
        }
    }

    /// Build a context taking a fresh snapshot now (READ COMMITTED default).
    pub fn context(&self, xid: Xid, cid: CommandId) -> TxnContext {
        // Capture and register together, so no vacuum can slip between the two.
        let (snapshot, guard) = self.freeze_snapshot();
        self.context_from(xid, cid, snapshot, guard, IsolationLevel::ReadCommitted)
    }

    /// Build a context from an explicit snapshot and isolation level — for
    /// REPEATABLE READ, which reuses one snapshot for the whole transaction.
    pub fn context_with(
        &self,
        xid: Xid,
        cid: CommandId,
        snapshot: Snapshot,
        iso: IsolationLevel,
    ) -> TxnContext {
        let guard = self.register_snapshot(&snapshot);
        self.context_from(xid, cid, snapshot, guard, iso)
    }

    fn context_from(
        &self,
        xid: Xid,
        cid: CommandId,
        snapshot: Snapshot,
        guard: SnapshotGuard,
        iso: IsolationLevel,
    ) -> TxnContext {
        let reservation = Arc::new(guard);
        TxnContext {
            xid,
            cid,
            snapshot,
            iso,
            reservation: Some(reservation),
            clog: Arc::clone(&self.clog),
            // Default the lock owner to the transaction's XID; the server
            // overrides it with the connection's session-stable owner in
            // `build_txn`. This default keeps engine-level tests (which build
            // contexts directly) correctly keyed per transaction.
            lock_owner: LockOwner(xid.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Is `hdr` visible to reader `(xid, cid)` under `snap`?
    fn visible(hdr: &TupleHeader, snap: &Snapshot, clog: &Clog, xid: Xid, cid: u32) -> bool {
        satisfies_mvcc(hdr, snap, clog, xid, CommandId(cid))
    }

    #[test]
    fn allocate_is_monotonic_and_tracks_active() {
        let xm = XidManager::new();
        let a = xm.allocate();
        let b = xm.allocate();
        assert!(b > a);
        let snap = xm.take_snapshot();
        assert!(snap.in_progress(a));
        assert!(snap.in_progress(b));
        xm.complete(a);
        xm.complete(b);
        // A fresh snapshot no longer lists them.
        let snap2 = xm.take_snapshot();
        assert!(!snap2.in_progress(a));
        assert!(!snap2.in_progress(b));
    }

    /// The property every reclaiming caller depends on: a read-only reader is
    /// invisible to `Snapshot::xmin` — it holds no XID — so only the registry
    /// keeps `reclaim_horizon` from running past it.
    #[test]
    fn a_registered_read_only_snapshot_holds_back_the_reclaim_horizon() {
        let tm = TransactionManager::new();
        let (reader, guard) = tm.freeze_snapshot();

        // Write traffic that starts and finishes entirely after the reader. With
        // nothing left in flight the running xmin is free to run past it.
        let writer = tm.allocate_xid();
        tm.commit(writer)
            .expect("a manager with no sink cannot fail to commit");
        assert!(
            tm.snapshot().xmin > reader.xmax,
            "the premise: xmin must have passed the reader, which holds no XID"
        );

        assert_eq!(
            tm.reclaim_horizon(),
            reader.xmin,
            "the live reader must pin the horizon at its own snapshot"
        );

        // Once the reader is gone, nothing holds the horizon down.
        drop(guard);
        assert_eq!(
            tm.reclaim_horizon(),
            tm.snapshot().xmin,
            "a released snapshot must stop holding back reclamation"
        );
    }

    /// A reader pins its `xmin`, not its `xmax`. The two coincide only when the
    /// reader's `xip` list is empty, so a writer already in flight at capture time
    /// is what separates a correct floor from one that reclaims rows the reader is
    /// still entitled to see.
    #[test]
    fn a_reader_pins_below_a_writer_that_was_in_flight_when_it_captured() {
        let tm = TransactionManager::new();

        // The deleter is ALREADY running when the reader captures its snapshot, so
        // it lands in `xip` and its delete does not apply to this reader.
        let deleter = tm.allocate_xid();
        let (reader, _guard) = tm.freeze_snapshot();
        assert!(
            reader.in_progress(deleter),
            "the premise: the reader must still see rows this deleter removed"
        );

        tm.commit(deleter)
            .expect("a manager with no sink cannot fail to commit");

        // Storage reclaims a version when `xmax < horizon && committed`, so a
        // horizon above `deleter` would free rows the reader can still read.
        let horizon = tm.reclaim_horizon();
        assert!(
            deleter >= horizon,
            "horizon {horizon:?} ran past deleter {deleter:?}, which the reader still sees"
        );
    }

    #[test]
    fn committed_insert_is_visible_deleted_row_is_not() {
        let clog = Clog::new();
        let inserter = Xid(3);
        clog.set_committed(inserter);
        // Snapshot taken now: inserter already complete.
        let snap = Snapshot {
            xmin: Xid(4),
            xmax: Xid(4),
            xip: vec![],
        };

        let live = TupleHeader::inserted(inserter, CommandId(0));
        assert!(visible(&live, &snap, &clog, Xid::INVALID, 0));

        // Delete it with a committed transaction that is also complete.
        let deleter = Xid(4);
        clog.set_committed(deleter);
        let snap2 = Snapshot {
            xmin: Xid(5),
            xmax: Xid(5),
            xip: vec![],
        };
        let mut dead = live;
        dead.xmax = deleter;
        assert!(!visible(&dead, &snap2, &clog, Xid::INVALID, 0));
    }

    #[test]
    fn in_progress_insert_is_invisible_to_others() {
        let clog = Clog::new();
        let other = Xid(3); // never committed -> InProgress
        let snap = Snapshot {
            xmin: Xid(3),
            xmax: Xid(4),
            xip: vec![Xid(3)],
        };
        let hdr = TupleHeader::inserted(other, CommandId(0));
        assert!(!visible(&hdr, &snap, &clog, Xid(7), 0));
    }

    #[test]
    fn aborted_insert_is_invisible_even_if_snapshot_would_allow() {
        let clog = Clog::new();
        let other = Xid(3);
        clog.set_aborted(other);
        let snap = Snapshot {
            xmin: Xid(4),
            xmax: Xid(4),
            xip: vec![],
        };
        let hdr = TupleHeader::inserted(other, CommandId(0));
        assert!(!visible(&hdr, &snap, &clog, Xid::INVALID, 0));
    }

    #[test]
    fn own_insert_visible_only_from_a_later_command() {
        let clog = Clog::new();
        let me = Xid(3);
        let snap = Snapshot {
            xmin: Xid(3),
            xmax: Xid(4),
            xip: vec![Xid(3)],
        };
        // Inserted by command 0.
        let hdr = TupleHeader::inserted(me, CommandId(0));
        // The inserting command itself does not see the row...
        assert!(!visible(&hdr, &snap, &clog, me, 0));
        // ...but a later command in the same transaction does.
        assert!(visible(&hdr, &snap, &clog, me, 1));
    }

    #[test]
    fn own_delete_hides_row_only_from_later_commands() {
        let clog = Clog::new();
        let me = Xid(3);
        let snap = Snapshot {
            xmin: Xid(3),
            xmax: Xid(4),
            xip: vec![Xid(3)],
        };
        // Inserted by command 0, deleted by command 1, same transaction.
        let mut hdr = TupleHeader::inserted(me, CommandId(0));
        hdr.xmax = me;
        hdr.cmax = CommandId(1);
        // The deleting command's own scan still sees the row it deletes
        // (curcid == cmax) — this is what avoids the Halloween problem.
        assert!(visible(&hdr, &snap, &clog, me, 1));
        // A later command in the same transaction no longer sees it.
        assert!(!visible(&hdr, &snap, &clog, me, 2));
    }

    #[test]
    fn delete_by_in_progress_txn_leaves_row_visible() {
        let clog = Clog::new();
        let inserter = Xid(3);
        clog.set_committed(inserter);
        let deleter = Xid(4); // in progress
        let snap = Snapshot {
            xmin: Xid(4),
            xmax: Xid(5),
            xip: vec![Xid(4)],
        };
        let mut hdr = TupleHeader::inserted(inserter, CommandId(0));
        hdr.xmax = deleter;
        // Deleter not committed -> row still visible.
        assert!(visible(&hdr, &snap, &clog, Xid(9), 0));
    }

    #[test]
    fn commit_after_snapshot_is_not_visible() {
        let clog = Clog::new();
        let inserter = Xid(5);
        // Snapshot taken while inserter still in flight.
        let snap = Snapshot {
            xmin: Xid(5),
            xmax: Xid(6),
            xip: vec![Xid(5)],
        };
        let hdr = TupleHeader::inserted(inserter, CommandId(0));
        // Even though it commits now, the snapshot still hides it.
        clog.set_committed(inserter);
        assert!(!visible(&hdr, &snap, &clog, Xid::INVALID, 0));
    }

    #[test]
    fn transaction_manager_commit_makes_writes_visible() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let xid = tm.allocate_xid();
        let hdr = TupleHeader::inserted(xid, CommandId(0));
        // Before commit, another reader's fresh snapshot cannot see it.
        let before = tm.context(Xid::INVALID, CommandId::FIRST);
        assert!(!satisfies_mvcc(
            &hdr,
            &before.snapshot,
            &before.clog,
            before.xid,
            before.cid
        ));
        // After commit, a newly taken snapshot can.
        tm.commit(xid)?;
        let after = tm.context(Xid::INVALID, CommandId::FIRST);
        assert!(satisfies_mvcc(
            &hdr,
            &after.snapshot,
            &after.clog,
            after.xid,
            after.cid
        ));

        Ok(())
    }

    struct FailingSink;
    impl CommitSink for FailingSink {
        fn log_commit(&self, _xid: Xid) -> std::io::Result<()> {
            Err(std::io::Error::other("wal down"))
        }
        fn log_abort(&self, _xid: Xid) {}
    }

    #[test]
    fn failed_commit_retires_the_xid() {
        // A commit whose WAL flush fails must not leak the XID in the in-flight
        // set (which would pin the snapshot horizon forever).
        let sink: Arc<dyn CommitSink> = Arc::new(FailingSink);
        let tm = TransactionManager::new_recovered(sink, Arc::new(Clog::new()), Xid::FIRST_NORMAL);
        let xid = tm.allocate_xid();
        assert!(tm.commit(xid).is_err());
        assert_eq!(tm.clog().status(xid), XactStatus::Aborted);
        assert!(
            !tm.snapshot().in_progress(xid),
            "XID must be retired from the in-flight set"
        );
    }

    #[test]
    fn transaction_manager_abort_keeps_writes_invisible() {
        let tm = TransactionManager::new();
        let xid = tm.allocate_xid();
        let hdr = TupleHeader::inserted(xid, CommandId(0));
        tm.abort(xid);
        let after = tm.context(Xid::INVALID, CommandId::FIRST);
        assert!(!satisfies_mvcc(
            &hdr,
            &after.snapshot,
            &after.clog,
            after.xid,
            after.cid
        ));
    }
}

#[cfg(test)]
mod durable_clog_tests {
    use super::*;

    #[test]
    fn statuses_survive_a_reopen() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // XIDs spread across two segments and several pages, so the reopen has to
        // find them through real addressing rather than one cached page.
        let committed = [Xid(3), Xid(9_000), Xid(clog::XACTS_PER_SEGMENT + 17)];
        let aborted = [Xid(4), Xid(clog::XACTS_PER_PAGE + 1)];
        {
            let log = Clog::open(dir.path())?;
            for xid in committed {
                log.set_committed(xid);
            }
            for xid in aborted {
                log.set_aborted(xid);
            }
            // Before the flush nothing is on disk; the checkpoint is what publishes.
            log.flush()?;
        }
        let log = Clog::open(dir.path())?;
        for xid in committed {
            assert_eq!(log.status(xid), XactStatus::Committed, "{xid:?}");
        }
        for xid in aborted {
            assert_eq!(log.status(xid), XactStatus::Aborted, "{xid:?}");
        }
        // An XID nobody ever reported on is still running, not committed.
        assert_eq!(log.status(Xid(12_345)), XactStatus::InProgress);
        Ok(())
    }

    #[test]
    fn unflushed_statuses_are_lost_but_the_wal_is_the_authority() -> anyhow::Result<()> {
        // Dropping without a flush loses the bits — which is safe precisely
        // because `commit` fsyncs the WAL commit record first, so replay puts
        // them back. This pins the write-back contract: nothing reaches disk
        // outside `flush`.
        let dir = tempfile::tempdir()?;
        {
            let log = Clog::open(dir.path())?;
            log.set_committed(Xid(7));
        }
        assert_eq!(
            Clog::open(dir.path())?.status(Xid(7)),
            XactStatus::InProgress
        );
        Ok(())
    }

    #[test]
    fn opening_a_fresh_cluster_stamps_the_version_marker() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let subdir = dir.path().join(clog::CLOG_SUBDIR);
        assert_eq!(clog::read_meta(&subdir)?, None);
        let _ = Clog::open(dir.path())?;
        assert_eq!(
            clog::read_meta(&subdir)?,
            Some(clog::ClogMeta {
                floor: Xid::INVALID
            })
        );
        Ok(())
    }

    #[test]
    fn an_unreadable_meta_refuses_to_open() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let subdir = dir.path().join(clog::CLOG_SUBDIR);
        std::fs::create_dir_all(&subdir)?;
        std::fs::write(subdir.join("meta"), b"not a clog meta at all")?;
        // Guessing the geometry would silently misread every commit status.
        assert!(Clog::open(dir.path()).is_err());
        Ok(())
    }

    #[test]
    fn an_in_memory_clog_never_touches_the_filesystem() -> anyhow::Result<()> {
        let log = Clog::new();
        log.set_committed(Xid(5));
        assert_eq!(log.status(Xid(5)), XactStatus::Committed);
        log.flush()?; // a no-op, not an error
        Ok(())
    }
}

/// Throughput of [`Clog`] lookups and stamps at 1, 2, 4 and 8 threads.
///
/// [`Clog::status`] is the hot path of the whole system — [`satisfies_mvcc`] calls
/// it up to twice per tuple per scan — so how it behaves under concurrency is a
/// design constraint, not a detail. This measures exactly that and nothing else:
/// the commit log in isolation, no parse, no plan, no storage.
///
/// `#[ignore]`d because it is a *measurement*, not an assertion. It asserts nothing
/// about timing — a timing assertion is the classic CI flake — so it never runs in
/// CI, and its numbers are machine-dependent. Run it explicitly:
///
/// ```text
/// cargo test --release -p crabgresql-txn --lib clog_contention_bench \
///     -- --ignored --nocapture --test-threads=1
/// ```
///
/// `--release` is mandatory (a debug build measures lock bookkeeping and
/// unoptimised atomics, not the design) and `--test-threads=1` stops the harness
/// from running two thread counts concurrently and poisoning both.
#[cfg(test)]
mod clog_contention_bench {
    use super::*;
    use std::hint::black_box;
    use std::sync::Barrier;
    use std::time::{Duration, Instant};

    /// The XID window every thread walks. All of page 0, so the page fault is out
    /// of the measurement and all threads contend on the same 8 KiB.
    const LO: u64 = 3;
    const HI: u64 = 32_000;
    const SPAN: u64 = HI - LO;
    /// Coprime with `SPAN` (= 7² · 653), so the walk sweeps the whole page instead
    /// of cycling one cache line.
    const STRIDE: u64 = 7_919;
    const ITERS: u64 = 2_000_000;
    const WARMUP: u64 = 100_000;

    #[derive(Clone, Copy)]
    enum Mix {
        Read,
        Write,
        /// What a real workload looks like: nine visibility checks per stamp.
        Mixed,
    }

    fn work(log: &Clog, mix: Mix, iters: u64, seed: u64) -> u64 {
        let mut idx = seed % SPAN;
        let mut acc = 0u64;
        for i in 0..iters {
            idx = (idx + STRIDE) % SPAN;
            let xid = Xid(LO + idx);
            let write = match mix {
                Mix::Read => false,
                Mix::Write => true,
                Mix::Mixed => i % 10 == 0,
            };
            if write {
                log.set_committed(xid);
                acc += 1;
            } else {
                acc += u64::from(log.is_committed(xid));
            }
        }
        acc
    }

    fn run(threads: usize, mix: Mix) -> Duration {
        let log = Arc::new(Clog::new());
        // Pre-stamp the window so every lookup is a hit and page 0 is resident.
        for xid in LO..HI {
            log.set_committed(Xid(xid));
        }
        let barrier = Arc::new(Barrier::new(threads + 1));
        let mut handles = Vec::with_capacity(threads);
        for t in 0..threads {
            let log = Arc::clone(&log);
            let barrier = Arc::clone(&barrier);
            let seed = t as u64 * 977;
            handles.push(std::thread::spawn(move || {
                // Warm up before the barrier, so first-touch and branch-predictor
                // effects sit outside the timed window.
                black_box(work(&log, mix, WARMUP, seed));
                barrier.wait();
                let acc = work(&log, mix, ITERS, seed);
                // Without this the optimiser is free to delete the whole loop.
                assert!(black_box(acc) > 0, "the measured loop did no work");
            }));
        }
        barrier.wait();
        let started = Instant::now();
        for handle in handles {
            assert!(handle.join().is_ok(), "a bench worker panicked");
        }
        started.elapsed()
    }

    #[test]
    #[ignore = "measurement, not an assertion: prints throughput, asserts nothing about timing"]
    fn clog_contention() {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0);
        eprintln!(
            "clog contention: {ITERS} ops/thread over XIDs [{LO}, {HI}), available_parallelism={cores}"
        );
        eprintln!(
            "{:>8}  {:>10}  {:>10}  {:>10}   (Mops/s, aggregate)",
            "threads", "read", "write", "90/10"
        );
        for threads in [1usize, 2, 4, 8] {
            let mut mops = [0f64; 3];
            for (slot, mix) in mops.iter_mut().zip([Mix::Read, Mix::Write, Mix::Mixed]) {
                let elapsed = run(threads, mix);
                *slot = (ITERS * threads as u64) as f64 / elapsed.as_secs_f64() / 1e6;
            }
            eprintln!(
                "{threads:>8}  {:>10.1}  {:>10.1}  {:>10.1}",
                mops[0], mops[1], mops[2]
            );
        }
    }
}
