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

mod lock;

pub use lock::{ExclusiveGuard, SharedGuard, TableLock};

use std::collections::BTreeSet;
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

/// The commit log: the authoritative fate of every transaction. This first
/// implementation is in-memory (lost on restart, like the memory engine);
/// durability lands with WAL in P4. An XID absent from the map is treated as
/// [`XactStatus::InProgress`].
#[derive(Debug, Default)]
pub struct Clog {
    // Sparse; a durable SLRU-style bitmap replaces this in P4.
    status: Mutex<std::collections::HashMap<Xid, XactStatus>>,
}

impl Clog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn status(&self, xid: Xid) -> XactStatus {
        if xid == Xid::FROZEN {
            return XactStatus::Committed;
        }
        self.status
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .get(&xid)
            .copied()
            .unwrap_or(XactStatus::InProgress)
    }

    pub fn is_committed(&self, xid: Xid) -> bool {
        self.status(xid) == XactStatus::Committed
    }

    pub fn set_committed(&self, xid: Xid) {
        self.status
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .insert(xid, XactStatus::Committed);
    }

    pub fn set_aborted(&self, xid: Xid) {
        self.status
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .insert(xid, XactStatus::Aborted);
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

    /// Capture the current in-flight set.
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
        self.context_with(xid, cid, self.snapshot(), IsolationLevel::ReadCommitted)
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
        TxnContext {
            xid,
            cid,
            snapshot,
            iso,
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
