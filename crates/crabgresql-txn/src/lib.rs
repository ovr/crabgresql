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

/// One slot of the resident-page index.
///
/// Installed at most once and never replaced, which is what makes a lookup
/// lock-free: the address of an installed page is stable for the life of the commit
/// log, so a reader can follow it with a plain load. `OnceLock` also gives the
/// fault-in race the right shape for free — concurrent first touches of the *same*
/// page read it once and share the result, while readers of every other page are
/// undisturbed.
type PageSlot = std::sync::OnceLock<Box<clog::ClogPage>>;

/// A block of [`PageSlot`]s, allocated on the first touch anywhere in its range.
type Chunk = Box<[PageSlot]>;

/// Page slots per [`Chunk`]: 4096 slots index 2^27 transactions and cost 64 KiB,
/// paid the first time any XID in that range is looked up.
const PAGES_PER_CHUNK: usize = 4096;

/// [`Chunk`] slots in the index spine. The spine is allocated eagerly, so this is
/// what an empty commit log costs: 2048 slots at 24 bytes each, 48 KiB.
const INDEX_CHUNKS: usize = 2048;

/// The highest page number the resident index can address, hence the ceiling on
/// XIDs the commit log will answer for: 2^23 pages, so XIDs below 2^38 (2.7×10^11).
///
/// This is a guard, not a budget. `pageno = xid >> 15`, so without a ceiling a
/// garbage XID near `u64::MAX` would index terabytes off the end of the spine; heap
/// pages carry only a 16-bit checksum, so a corrupt `xmin`/`xmax` reaching a
/// visibility check is plausible. Rejecting it answers [`XactStatus::InProgress`],
/// which hides a row rather than resurrecting a deleted one.
///
/// This is not comfortable headroom, and the numbers are worth stating plainly:
/// 2^23 pages of 8 KiB is 64 GiB of resident pages, which is reachable on a large
/// machine, and 2^38 XIDs is about a month at a sustained 100k commits/s. Resident
/// page memory binds first, and bounding it is the eviction follow-up named in this
/// design's notes — but a cluster that outruns either ceiling needs the truncation
/// work (Rung D), not a bigger constant. Crossing it is at least not silent: a
/// *stamp* above the ceiling latches an error that fails the next checkpoint (see
/// [`Clog::set_status`]), rather than being dropped.
const MAX_PAGENO: u64 = (INDEX_CHUNKS * PAGES_PER_CHUNK) as u64;

/// Where an XID sits relative to the addressable range of the commit log.
enum Reach {
    /// Addressable: this is its page number.
    Page(u64),
    /// Frozen out of every relation and its segment possibly gone, so there are no
    /// bits to read and none worth writing.
    BelowFloor,
    /// Past [`MAX_PAGENO`] — a garbage XID, not a real one.
    AboveCeiling,
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
///
/// # Why the index is lock-free rather than an `RwLock`
///
/// [`Clog::status`] is the hot path of the whole system — [`satisfies_mvcc`]
/// consults it up to twice per tuple per scan — so it must not serialise concurrent
/// scanners. A shared lock does *not* achieve that: acquiring one still means a
/// read-modify-write on a single shared word, so every lookup bounces the same cache
/// line between cores. Measured on this crate's contention bench, an
/// `RwLock<Vec<..>>` index was *slower than the single `Mutex` it was meant to
/// replace* at every thread count: 3.8 Mops/s at eight threads against the mutex's
/// 20.4, the figure the bench records on the parent commit.
///
/// So the index takes no lock at all. A lookup is two dependent acquire loads
/// (spine slot, then page slot) and then one relaxed load of the status byte — no
/// atomic RMW on any shared word, so the lines involved stay shared and reads scale
/// with cores. Stamping a status is the same walk plus one masked RMW on the page's
/// own byte ([`clog::ClogPage::set_status`]).
///
/// Nothing is ever evicted or moved, which is what makes that sound. A page is
/// 8 KiB per 32768 transactions — 256 KiB per million — and evicting would mean
/// writing a page outside a checkpoint, precisely what the write-back model exists
/// to avoid. Truncation is the only thing that will ever drop pages, and it has to
/// take the spine apart deliberately (Rung D).
pub struct Clog {
    /// `None` for an in-memory-only commit log — the memory engine and tests,
    /// where nothing is ever written and the cache is simply the whole log.
    dir: Option<std::path::PathBuf>,
    /// Every XID below this has been frozen out of every relation; its segment
    /// may be gone. Zero until a freeze sweep advances it.
    floor: AtomicU64,
    /// The next XID the allocator would hand out, as last observed.
    ///
    /// It lives here rather than being read back from the [`TransactionManager`]
    /// for the same reason the engine holds this commit log *strongly*: the
    /// checkpoint that most needs a current floor is the one at a clean shutdown,
    /// by which point the manager may already be gone. A checkpoint that recorded
    /// a stale floor would be worse than useless once replay is bounded — a
    /// bounded replay never sees the XIDs below its redo point, so this value is
    /// their only remaining floor, and too low a floor means reissuing an XID
    /// already stamped on committed tuples.
    next_xid: AtomicU64,
    /// The resident-page index: a fixed spine of lazily allocated [`Chunk`]s.
    ///
    /// Two levels rather than one flat array so that an empty commit log costs
    /// 48 KiB instead of the 128 MiB a flat [`MAX_PAGENO`]-sized index would, and
    /// two levels rather than a growable `Vec` so that no lookup needs a lock.
    spine: Box<[std::sync::OnceLock<Chunk>]>,
    /// A read error with nowhere to go: [`Clog::status`] returns a bare
    /// [`XactStatus`]. Latched here and surfaced by [`Clog::flush`], which does
    /// have somewhere to put it. Never silently dropped.
    failure: Mutex<Option<std::io::Error>>,
    /// Serialises [`Clog::flush`] with itself.
    ///
    /// Two concurrent checkpoints would otherwise interleave: the second would find
    /// every page the first had claimed but not yet written already clean, and
    /// report a durable commit log while those writes were still in flight — so its
    /// caller would publish a control file naming a checkpoint whose CLOG is not on
    /// disk. The single mutex this design replaced gave that mutual exclusion for
    /// free; this keeps it.
    flushing: Mutex<()>,
}

impl Default for Clog {
    /// Hand-written, not derived: `Box<[T]>::default()` is an *empty* slice, so a
    /// derive would leave the spine zero-length and every lookup out of range.
    fn default() -> Self {
        Clog {
            dir: None,
            // Xid has no Default, and shouldn't: INVALID is the meaningful zero
            // here, not an arbitrary one.
            floor: AtomicU64::new(Xid::INVALID.0),
            next_xid: AtomicU64::new(Xid::FIRST_NORMAL.0),
            spine: (0..INDEX_CHUNKS)
                .map(|_| std::sync::OnceLock::new())
                .collect(),
            failure: Mutex::new(None),
            flushing: Mutex::new(()),
        }
    }
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
            floor: AtomicU64::new(floor.0),
            ..Clog::default()
        })
    }

    /// Raise the recorded XID floor to `next`. Monotonic, so a late-arriving lower
    /// observation cannot walk it back.
    pub fn observe_next_xid(&self, next: Xid) {
        self.next_xid.fetch_max(next.0, Ordering::Relaxed);
    }

    /// The next XID to hand out, for a checkpoint to record as its floor.
    pub fn next_xid_floor(&self) -> Xid {
        Xid(self.next_xid.load(Ordering::Relaxed))
    }

    /// Record an I/O error the caller has no way to return. The first one wins:
    /// the earliest failure is the one that explains the rest.
    fn latch(&self, error: std::io::Error) {
        self.failure
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .get_or_insert(error);
    }

    /// How far `xid` is from the addressable range. The two rejections are *not*
    /// interchangeable, which is why this is an enum rather than an `Option`: they
    /// mean different things, and the read and write paths must treat them
    /// differently.
    fn reach(&self, xid: Xid) -> Reach {
        // Relaxed: nothing is ever stored to `floor` after construction, so there is
        // no release for an acquire to pair with, and this runs once or twice per
        // tuple per scan. When a freeze sweep starts advancing it (Rung D) the
        // ordering it needs is against dropping pages, not against this word, so
        // this line will have to be revisited there rather than merely strengthened.
        if xid.0 < self.floor.load(Ordering::Relaxed) {
            return Reach::BelowFloor;
        }
        let pageno = clog::page_of(xid);
        if pageno >= MAX_PAGENO {
            return Reach::AboveCeiling;
        }
        Reach::Page(pageno)
    }

    /// The resident page holding `pageno`, faulting it in on its first touch.
    ///
    /// On the hit path this is two dependent acquire loads and nothing else — no
    /// lock, and no read-modify-write on any shared word, which is the entire point
    /// (see the type's documentation).
    ///
    /// `pageno` must have come from [`Clog::reach`], which is what bounds both
    /// indexes below.
    fn page(&self, pageno: u64) -> &clog::ClogPage {
        let index = pageno as usize;
        let chunk = self.spine[index / PAGES_PER_CHUNK]
            .get_or_init(|| (0..PAGES_PER_CHUNK).map(|_| PageSlot::new()).collect());
        chunk[index % PAGES_PER_CHUNK].get_or_init(|| {
            // The read happens inside this slot's one-time initialisation, so
            // concurrent first touches of this page read it once and share the
            // result, and no other page's readers are held up.
            //
            // A read failure latches the error and installs a *poisoned* page: the
            // caller sees `InProgress` — the answer that hides a row rather than
            // resurrecting it — and later stamps still land and are still visible,
            // but the page is never written back, because its bytes are a zero-fill
            // rather than the segment's real contents and writing them would erase
            // every other status in the page.
            let (bytes, poisoned) = match self.dir.as_deref() {
                Some(dir) => match clog::read_page(dir, pageno) {
                    Ok(bytes) => (bytes, false),
                    // Deliberately not latched into `self.failure`: the poison flag
                    // *is* the record of this failure, and [`Clog::flush`] re-derives
                    // a current error from its own re-read attempt. Latching here as
                    // well would leave a stale error behind that fails the very
                    // checkpoint that healed the page.
                    Err(_) => (clog::ZERO_BYTES, true),
                },
                None => (clog::ZERO_BYTES, false),
            };
            Box::new(clog::ClogPage::new(&bytes, poisoned))
        })
    }

    pub fn status(&self, xid: Xid) -> XactStatus {
        if xid == Xid::FROZEN {
            return XactStatus::Committed;
        }
        match self.reach(xid) {
            Reach::Page(pageno) => self.page(pageno).status(xid),
            Reach::BelowFloor => {
                // Unreachable by construction: the floor only advances once every
                // relation has been swept, so no surviving tuple names an XID below
                // it. Asserting on the *read* path only — a stamp below the floor is
                // a different situation, handled in `set_status`.
                debug_assert!(
                    !xid.is_valid(),
                    "status({xid:?}) is below the CLOG floor {:?}",
                    Xid(self.floor.load(Ordering::Relaxed))
                );
                XactStatus::InProgress
            }
            // A garbage XID out of a corrupt tuple header, which is what the ceiling
            // exists for; deliberately not an assertion, since bad data must not
            // crash a debug build. `InProgress` is the fail-safe answer for an
            // *inserter* — it hides the row. Note it is not fail-safe for a deleter:
            // there it makes a committed delete invisible, so the row reappears.
            Reach::AboveCeiling => XactStatus::InProgress,
        }
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
        match self.reach(xid) {
            Reach::Page(pageno) => self.page(pageno).set_status(xid, status),
            // Not the fail-safe the read path gets: by the time a status is stamped
            // the WAL has already fsynced this transaction's fate, so dropping it
            // silently would leave a committed transaction's rows invisible forever
            // with nothing to say why — and `TransactionManager::commit` has already
            // returned `Ok` and run the engine's finalize hook. Latch it so the next
            // checkpoint fails instead of publishing a control file over a status
            // that was never recorded.
            Reach::BelowFloor | Reach::AboveCeiling => self.latch(std::io::Error::other(format!(
                "{xid:?} is outside the addressable commit-log range, so its \
                 {status:?} status was not recorded"
            ))),
        }
    }

    /// Write every dirty page and fsync the segments they landed in.
    ///
    /// Called from the checkpoint, which is the only place CLOG pages reach disk.
    /// A no-op for an in-memory commit log.
    ///
    /// The image this produces is not a consistent cut of the commit log: a status
    /// stamped while the walk is in progress may or may not appear in it. The
    /// invariant is the weaker one that a stamp is *either* in this image *or* still
    /// flagged for the next checkpoint (see [`clog::ClogPage::claim_for_writeback`]).
    /// That is sufficient for the same reason the single lock this replaced was: neither
    /// orders anything against [`CommitSink::log_commit`] returning, so a commit can
    /// always land in the commit log just after the checkpoint has read that byte,
    /// and replay is what covers it either way.
    ///
    /// "Replay covers it" is what [`CommitSink::delay_checkpoint`] keeps true now
    /// that replay is bounded. A commit is either stamped before the checkpoint
    /// sampled its redo point — so its bit is in this image — or its record sits
    /// above that redo point and is replayed. The barrier is what excludes the third
    /// case: sampled above the record, stamped after this walk read the byte.
    pub fn flush(&self) -> std::io::Result<()> {
        // Held for the whole call. See the field's documentation.
        let _flushing = self
            .flushing
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));

        let Some(dir) = self.dir.as_deref() else {
            return Ok(());
        };

        let mut claimed: Vec<&clog::ClogPage> = Vec::new();
        let mut segments = BTreeSet::new();
        let mut heal_error: Option<std::io::Error> = None;
        let mut result = Ok(());

        // Walking the index needs no lock and holds no guard across the writes
        // below, which are syscalls. Installed pages never move and are never
        // dropped while this commit log lives, so a plain `&ClogPage` stays valid
        // for as long as it is needed here.
        'walk: for (chunk_index, chunk_slot) in self.spine.iter().enumerate() {
            let Some(chunk) = chunk_slot.get() else {
                continue;
            };
            for (slot_index, page_slot) in chunk.iter().enumerate() {
                let Some(page) = page_slot.get() else {
                    continue;
                };
                // Cheap pre-check, before the claim's read-modify-write. Skipping it
                // for a clean page is sound where skipping it in a *writer* would not
                // be (see `ClogPage::mark_dirty`): reading `false` is indistinguishable
                // from claiming immediately before a stamp lands, which the protocol
                // already handles by leaving the page flagged for the next checkpoint.
                // Without this, a checkpoint takes an exclusive cache line on every
                // resident page — clean ones included — and invalidates it on every
                // core currently committing into that page.
                if !page.is_dirty() && !page.is_poisoned() {
                    continue;
                }
                let pageno = (chunk_index * PAGES_PER_CHUNK + slot_index) as u64;
                if page.is_poisoned() {
                    // Try to heal it. The zero-fill standing in for the unreadable
                    // page must never be written back — it would erase every other
                    // transaction's status in the page — but a read that now succeeds
                    // reconstructs the page exactly, because a zero slot is precisely
                    // one nothing has stamped since the failure.
                    match clog::read_page(dir, pageno) {
                        Ok(disk) => page.absorb_unstamped(&disk),
                        Err(error) => {
                            // Still unreadable, so it still must not be written.
                            // Deliberately kept out of `result` so the healthy pages
                            // below are still written and fsynced — one bad page does
                            // not hold the rest of the commit log hostage.
                            if heal_error.is_none() {
                                heal_error = Some(error);
                            }
                            continue;
                        }
                    }
                }
                let Some(image) = page.claim_for_writeback() else {
                    continue;
                };
                claimed.push(page);
                if let Err(error) = clog::write_page(dir, pageno, &image) {
                    result = Err(error);
                    break 'walk;
                }
                segments.insert(clog::segment_of_page(pageno));
            }
        }

        if result.is_ok() {
            for segno in &segments {
                if let Err(error) = clog::sync_segment(dir, *segno) {
                    result = Err(error);
                    break;
                }
            }
        }
        // A page written into a segment that did not exist before is not durable
        // until the directory entry naming it is. `write_page` creates segments, so
        // without this a crash just after a checkpoint that opened a new segment
        // range could lose the whole segment rather than merely its contents.
        if result.is_ok() && !segments.is_empty() {
            result = clog::sync_dir(dir);
        }

        if let Err(error) = result {
            // Leave everything this pass claimed dirty so the next checkpoint
            // retries it — including pages already written, whose segment may not
            // have been fsynced.
            for page in claimed {
                page.mark_dirty();
            }
            return Err(error);
        }

        // A page still poisoned after the heal attempt fails the checkpoint, whether
        // or not it is carrying stamps: it is answering `InProgress` for up to 32768
        // XIDs it has no information about, so the commit log is not in a state worth
        // certifying as durable. This is not the permanent wedge it would once have
        // been — the next checkpoint re-reads it, and the first read that succeeds
        // heals it outright.
        if let Some(error) = heal_error {
            return Err(error);
        }

        // Surface a latched error last, so a checkpoint stopped by a write failure
        // reports that failure rather than an older, unrelated one.
        let latched = self
            .failure
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .take();
        match latched {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl std::fmt::Debug for Clog {
    /// Hand-written for the same reason [`SnapshotGuard`]'s is, and one more:
    /// [`TxnContext`] derives `Debug` and holds an `Arc<Clog>`, so a single `{:?}`
    /// on a transaction context would otherwise walk the whole index and dump 8192
    /// atomic loads per resident page.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clog")
            .field("dir", &self.dir)
            .field("floor", &Xid(self.floor.load(Ordering::Relaxed)))
            .finish_non_exhaustive()
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
        //
        // The snapshot goes first deliberately. Both operands are pure, so the
        // order is a free choice, and `Snapshot::in_progress` searches a sorted
        // `Vec` this reader already owns, touching no shared memory at all, while
        // `Clog::is_committed` walks the shared page index and may fault a page in
        // off disk. Vetoing on the snapshot first means an XID the reader could not
        // have seen anyway never touches the commit log.
        if snap.in_progress(hdr.xmin) || !clog.is_committed(hdr.xmin) {
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
    // committed and visible in our snapshot; otherwise the row is still live. The
    // snapshot is tested first for the same reason as above.
    if !snap.in_progress(hdr.xmax) && clog.is_committed(hdr.xmax) {
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

    /// Hold off anything that would sample a redo point until the returned token
    /// is dropped, so a transaction's record and the [`Clog`] bit deciding its
    /// fate become visible to that sample *together*.
    ///
    /// Without it, a checkpointer can sample a redo point above a commit record
    /// whose CLOG bit has not been set yet, write a commit-log image that still
    /// reads `InProgress`, and publish that pair. A bounded replay then starts
    /// above the commit record and never learns the fate: an acknowledged
    /// commit's rows are invisible forever.
    ///
    /// The token is opaque because the guard it wraps lives in `crabgresql-wal`,
    /// which depends on this crate — the seam only has to carry "released on
    /// drop", not the guard's identity. `None` when the sink has no checkpointer
    /// to hold off, which is why this defaults: an in-memory sink needs nothing.
    fn delay_checkpoint(&self) -> Option<Box<dyn Send + '_>> {
        None
    }
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
        // Seed the checkpoint's floor with the recovered one, so a cluster that
        // starts and stops without allocating anything still records a floor no
        // lower than the one it recovered.
        clog.observe_next_xid(next_xid);
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
        let xid = self.xids.allocate();
        // Publish the floor where a checkpoint can still reach it. At *allocation*,
        // not at commit: an XID stamps tuples the moment its transaction writes, so
        // one that crashed in flight must never be reissued either — the reissued
        // XID would make the old transaction's rows visible the moment it commits.
        self.clog.observe_next_xid(Xid(xid.0 + 1));
        xid
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
            // Taken BEFORE the append, not merely around the CLOG update: a redo
            // point is sampled from the *staged* insert position, so a sampler can
            // already be above a record that has not been flushed. Released once
            // the CLOG carries the fate, below.
            let delay = self.sink.as_ref().and_then(|sink| sink.delay_checkpoint());
            if let Some(sink) = &self.sink
                && let Err(e) = sink.log_commit(xid)
            {
                // A commit whose WAL never reached disk did not happen: abort the
                // transaction so its XID is retired (otherwise it stays in the
                // in-flight set forever, pinning the snapshot xmin horizon) and
                // its versions become dead. Then surface the I/O error.
                drop(delay);
                self.abort(xid);
                return Err(e);
            }
            self.clog.set_committed(xid);
            // The record and its fate are now inseparable to any sampler. Dropped
            // before the steps below, which take engine locks and do file I/O — a
            // checkpoint must not wait on those.
            drop(delay);
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
            // Same window as `commit`, milder symptom: an abort record below a
            // published redo point whose `Aborted` bit missed the commit-log image
            // leaves the XID `InProgress` forever. Rows it *wrote* are invisible
            // either way, but a row it *deleted* keeps an in-progress `xmax`, which
            // no later transaction can stamp — visible, and permanently immune to
            // `UPDATE` and `DELETE`.
            let delay = self.sink.as_ref().and_then(|sink| sink.delay_checkpoint());
            if let Some(sink) = &self.sink {
                sink.log_abort(xid);
            }
            self.clog.set_aborted(xid);
            drop(delay);
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
            // Skip the sink entirely while unwinding, barrier included. Not just
            // the barrier: `log_abort` reaches the WAL's `inner` lock, which
            // panics on poison *by design* — `append` takes the staged buffer out
            // and puts it back, so a poisoned `Inner` really can be torn and that
            // must stay fatal. Panicking here, in a `Drop` that a panic already
            // started, aborts the process — the outcome this whole function exists
            // to avoid.
            //
            // The cost is one missing abort record: if the process then dies
            // before the next checkpoint flushes the commit log, the XID comes
            // back `InProgress` rather than `Aborted`. Every reader treats those
            // the same, and an abort record is not fsynced anyway, so this is the
            // cheaper half of the trade.
            if !std::thread::panicking() {
                let delay = self.sink.as_ref().and_then(|sink| sink.delay_checkpoint());
                if let Some(sink) = &self.sink {
                    sink.log_abort(xid);
                }
                self.clog.set_aborted(xid);
                drop(delay);
            } else {
                self.clog.set_aborted(xid);
            }
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

    /// The xmax case where the *snapshot* is the only thing keeping the row alive.
    /// Its mirror on the xmin side is `commit_after_snapshot_is_not_visible`; without
    /// it, dropping `snap.in_progress(hdr.xmax)` from the visibility rule turns every
    /// concurrent committed delete into a non-repeatable read with a green suite.
    #[test]
    fn a_committed_delete_still_in_flight_at_snapshot_time_leaves_the_row_visible() {
        let clog = Clog::new();
        let inserter = Xid(3);
        clog.set_committed(inserter);
        // Committed in the commit log...
        let deleter = Xid(4);
        clog.set_committed(deleter);
        // ...but still in flight as of this snapshot, so its delete is not ours to
        // see: the row has to stay visible for the whole of this transaction.
        let snap = Snapshot {
            xmin: Xid(4),
            xmax: Xid(5),
            xip: vec![Xid(4)],
        };
        assert!(snap.in_progress(deleter), "the premise of this test");
        assert!(clog.is_committed(deleter), "and the other half of it");
        let mut hdr = TupleHeader::inserted(inserter, CommandId(0));
        hdr.xmax = deleter;
        assert!(visible(&hdr, &snap, &clog, Xid(9), 0));
    }

    /// The xmax case where the CLOG is the *only* thing keeping the row alive:
    /// every other test here has the deleter either still in the snapshot's
    /// in-flight set or committed, so a visibility rule that consulted only the
    /// snapshot would pass them all and reclaim this row.
    #[test]
    fn an_aborted_delete_leaves_the_row_visible_once_the_deleter_leaves_the_snapshot() {
        let clog = Clog::new();
        let inserter = Xid(3);
        clog.set_committed(inserter);
        let deleter = Xid(4);
        clog.set_aborted(deleter);
        // Both are complete as of this snapshot, so `in_progress` vetoes neither.
        let snap = Snapshot {
            xmin: Xid(5),
            xmax: Xid(5),
            xip: vec![],
        };
        assert!(!snap.in_progress(deleter), "the premise of this test");
        let mut hdr = TupleHeader::inserted(inserter, CommandId(0));
        hdr.xmax = deleter;
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

    #[test]
    fn a_flushed_segment_matches_the_shipped_byte_layout() -> anyhow::Result<()> {
        // `pg_xact` is a persisted format, so its bit layout is a compatibility
        // boundary: files written by an older build must stay loadable. These are
        // deliberately hard-coded literals rather than values recomputed from
        // `byte_in_page`/`shift_in_byte` — those are the code under test, so
        // deriving the expectation from them would assert nothing at all.
        let dir = tempfile::tempdir()?;
        let log = Clog::open(dir.path())?;
        log.set_committed(Xid(3));
        log.set_aborted(Xid(4));
        log.set_committed(Xid(5));
        log.set_aborted(Xid(9));
        log.set_committed(Xid(clog::XACTS_PER_PAGE));
        log.set_committed(Xid(clog::XACTS_PER_SEGMENT + 1));
        log.flush()?;

        let subdir = dir.path().join(clog::CLOG_SUBDIR);
        let segment0 = std::fs::read(subdir.join("0000000000000000"))?;
        assert_eq!(segment0.len(), 2 * clog::CLOG_PAGE_SIZE, "pages 0 and 1");
        // Xid 3 => byte 0, shift 6. `0x40` and not `0x01` is what pins the low
        // slot at the low bits of the byte rather than the high ones.
        assert_eq!(segment0[0x0000], 0x40, "Xid(3) committed");
        // Xid 4 (Aborted, shift 0) and Xid 5 (Committed, shift 2) share byte 1.
        assert_eq!(segment0[0x0001], 0x06, "Xid(4) aborted + Xid(5) committed");
        // Xid 9 => byte 2, shift 2, Aborted.
        assert_eq!(segment0[0x0002], 0x08, "Xid(9) aborted");
        assert!(
            segment0[0x0003..clog::CLOG_PAGE_SIZE]
                .iter()
                .all(|b| *b == 0),
            "nothing else in page 0 was touched"
        );
        // The first XID of page 1 lands at the start of the second page.
        assert_eq!(segment0[0x2000], 0x01, "Xid(XACTS_PER_PAGE) committed");
        assert!(
            segment0[0x2001..].iter().all(|b| *b == 0),
            "nothing else in page 1 was touched"
        );

        // A new segment file, its page at offset 0, Committed at shift 2.
        let segment1 = std::fs::read(subdir.join("0000000000000001"))?;
        assert_eq!(
            segment1[0x0000], 0x04,
            "Xid(XACTS_PER_SEGMENT + 1) committed"
        );
        Ok(())
    }

    #[test]
    fn a_high_xid_does_not_materialise_the_segments_below_it() -> anyhow::Result<()> {
        // The resident index is addressed by absolute page number, so reaching a
        // high page must not fill in everything beneath it — on disk or in RAM.
        let dir = tempfile::tempdir()?;
        let xid = Xid(clog::XACTS_PER_SEGMENT * 4 + 7);
        {
            let log = Clog::open(dir.path())?;
            log.set_committed(xid);
            log.flush()?;
        }
        let subdir = dir.path().join(clog::CLOG_SUBDIR);
        for segno in 0..4 {
            assert!(
                !clog::segment_path(&subdir, segno).exists(),
                "segment {segno} should not exist"
            );
        }
        assert!(clog::segment_path(&subdir, 4).exists());
        assert_eq!(Clog::open(dir.path())?.status(xid), XactStatus::Committed);
        Ok(())
    }

    #[test]
    fn a_page_above_the_first_index_chunk_round_trips_through_its_own_segment() -> anyhow::Result<()>
    {
        // `flush` reconstructs each page number from its (chunk, slot) position in
        // the index. Every other durable test lives in chunk 0, where slot index and
        // page number coincide and a wrong multiplier is invisible; this one does not.
        let dir = tempfile::tempdir()?;
        let pageno = PAGES_PER_CHUNK as u64 + 1; // chunk 1, slot 1
        let xid = Xid(pageno * clog::XACTS_PER_PAGE + 5);
        {
            let log = Clog::open(dir.path())?;
            log.set_committed(xid);
            log.flush()?;
        }
        let subdir = dir.path().join(clog::CLOG_SUBDIR);
        assert!(
            clog::segment_path(&subdir, clog::segment_of_page(pageno)).exists(),
            "the page must land in the segment its page number names"
        );
        assert_eq!(Clog::open(dir.path())?.status(xid), XactStatus::Committed);
        Ok(())
    }

    #[test]
    fn a_failed_write_leaves_the_page_dirty_for_the_next_checkpoint() -> anyhow::Result<()> {
        // A checkpoint claims a page's dirty flag before writing it, so a write that
        // then fails has to put the flag back or the stamp is lost: the next
        // checkpoint would find nothing dirty and report success over it.
        let dir = tempfile::tempdir()?;
        let subdir = dir.path().join(clog::CLOG_SUBDIR);
        let log = Clog::open(dir.path())?;
        log.set_committed(Xid(3));
        log.flush()?;

        // Make the segment unwritable — but still readable, so this exercises the
        // write path rather than the fault-in path — and stamp again.
        let segment = clog::segment_path(&subdir, 0);
        let readonly = |on: bool| -> std::io::Result<()> {
            let mut perms = std::fs::metadata(&segment)?.permissions();
            perms.set_readonly(on);
            std::fs::set_permissions(&segment, perms)
        };
        readonly(true)?;
        log.set_committed(Xid(9));
        assert!(
            log.flush().is_err(),
            "an unwritable segment must fail the checkpoint"
        );

        // The stamp is still flagged, so the next checkpoint publishes it.
        readonly(false)?;
        log.flush()?;
        drop(log);

        let reopened = Clog::open(dir.path())?;
        assert_eq!(reopened.status(Xid(3)), XactStatus::Committed);
        assert_eq!(
            reopened.status(Xid(9)),
            XactStatus::Committed,
            "a stamp whose first write failed must survive to the next checkpoint"
        );
        Ok(())
    }

    #[test]
    fn a_page_that_could_not_be_read_heals_once_the_read_succeeds() -> anyhow::Result<()> {
        // A transient read failure must not be terminal. While the segment is
        // unreadable the page serves stamps out of RAM and refuses to be written
        // back; once the segment can be read again the checkpoint reconstructs the
        // page from it and publishes the whole thing, without erasing the statuses
        // that were already on disk.
        let dir = tempfile::tempdir()?;
        let subdir = dir.path().join(clog::CLOG_SUBDIR);
        let already = Xid(3);
        {
            let log = Clog::open(dir.path())?;
            log.set_committed(already);
            log.flush()?;
        }

        // Hide the real segment behind a directory of the same name, so the read
        // fails where `File::open` still succeeds.
        let segment = clog::segment_path(&subdir, 0);
        let stash = dir.path().join("stashed-segment");
        std::fs::rename(&segment, &stash)?;
        std::fs::create_dir(&segment)?;

        let log = Clog::open(dir.path())?;
        let during = Xid(9);
        log.set_committed(during);
        assert_eq!(
            log.status(during),
            XactStatus::Committed,
            "RAM still answers for what was stamped after the failure"
        );
        assert_eq!(
            log.status(already),
            XactStatus::InProgress,
            "and what it could not read is unknown, not committed"
        );
        assert!(
            log.flush().is_err(),
            "a page it cannot read must not be written back"
        );

        // The disk recovers.
        std::fs::remove_dir(&segment)?;
        std::fs::rename(&stash, &segment)?;
        log.flush()?;
        assert_eq!(
            log.status(already),
            XactStatus::Committed,
            "the checkpoint healed the page from disk"
        );
        drop(log);

        let reopened = Clog::open(dir.path())?;
        assert_eq!(
            reopened.status(already),
            XactStatus::Committed,
            "the status already on disk was not erased"
        );
        assert_eq!(
            reopened.status(during),
            XactStatus::Committed,
            "and the stamp taken while unreadable reached disk"
        );
        Ok(())
    }

    #[test]
    fn a_page_that_could_not_be_read_is_never_written_back() -> anyhow::Result<()> {
        // A read failure used to install a zero page that later checkpoints would
        // happily write back over the real segment, erasing up to 32767 other
        // transactions' statuses. Such a page must stay readable and stampable in
        // RAM but never reach disk, and the checkpoint must keep failing while it
        // holds statuses that are nowhere on disk.
        //
        // A directory where segment 0 belongs is the portable way to make one page
        // unreadable: `File::open` succeeds and the `read` then fails.
        let dir = tempfile::tempdir()?;
        let subdir = dir.path().join(clog::CLOG_SUBDIR);
        std::fs::create_dir_all(&subdir)?;
        std::fs::create_dir(clog::segment_path(&subdir, 0))?;

        let log = Clog::open(dir.path())?;
        let broken = Xid(3);
        let healthy = Xid(clog::XACTS_PER_SEGMENT + 3);
        log.set_committed(broken);
        log.set_committed(healthy);

        // In RAM the stamp is still authoritative.
        assert_eq!(log.status(broken), XactStatus::Committed);

        // The checkpoint fails — and keeps failing, rather than reporting success
        // once the latched read error has been taken.
        assert!(
            log.flush().is_err(),
            "first checkpoint must report the read"
        );
        assert!(log.flush().is_err(), "and must not later claim success");

        // A page in a healthy segment still reached disk: one bad page does not
        // hold the rest of the commit log hostage.
        drop(log);
        assert_eq!(
            Clog::open(dir.path())?.status(healthy),
            XactStatus::Committed
        );
        Ok(())
    }
}

/// The concurrent commit log: the write path, the page fault, and the interaction
/// between stamping and a checkpoint that is running at the same time.
#[cfg(test)]
mod concurrent_clog_tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    #[test]
    fn a_status_overwrite_replaces_the_old_bits_rather_than_or_ing_them() {
        // The obvious implementation of a lock-free stamp is `fetch_or`, and it is
        // wrong. `Wal::log_commit` appends the commit record *then* fsyncs, so on
        // fsync failure `TransactionManager::commit` aborts and appends an abort
        // record after the commit one; replay applies both. `Committed | Aborted`
        // is the SubCommitted encoding — neither committed nor aborted — and
        // `HeapTable`'s `is_live` would then read a row deleted by that XID as
        // permanently un-live. A stamp has to *replace* its two bits.
        let log = Clog::new();

        // Neighbours in the same byte, stamped first so an implementation that
        // writes the whole byte instead of the slot is caught too.
        log.set_committed(Xid(4));
        log.set_aborted(Xid(5));
        log.set_committed(Xid(6));

        log.set_committed(Xid(7));
        assert_eq!(log.status(Xid(7)), XactStatus::Committed);
        log.set_aborted(Xid(7));
        assert_eq!(
            log.status(Xid(7)),
            XactStatus::Aborted,
            "an abort after a commit must overwrite, not OR"
        );

        // And the other direction, on a fresh slot.
        log.set_aborted(Xid(11));
        log.set_committed(Xid(11));
        assert_eq!(log.status(Xid(11)), XactStatus::Committed);

        assert_eq!(log.status(Xid(4)), XactStatus::Committed);
        assert_eq!(log.status(Xid(5)), XactStatus::Aborted);
        assert_eq!(log.status(Xid(6)), XactStatus::Committed);
    }

    #[test]
    fn touching_a_low_page_does_not_drop_a_high_one() {
        // The resident index is addressed by absolute page number, so it has to
        // grow to reach a high page. Growing must never disturb what is already
        // there: a `Vec` + `resize_with` gets this wrong, because `resize_with`
        // *truncates* when the new length is shorter, and faulting a low page after
        // a high one is the steady state — XIDs are allocated upward while readers
        // keep touching old XIDs on old heap pages.
        let log = Clog::new();
        let high = Xid(100 * clog::XACTS_PER_PAGE + 7);
        let low = Xid(5 * clog::XACTS_PER_PAGE + 9);

        log.set_committed(high);
        log.set_aborted(low);

        assert_eq!(
            log.status(high),
            XactStatus::Committed,
            "high page survived"
        );
        assert_eq!(log.status(low), XactStatus::Aborted);
    }

    #[test]
    fn four_xids_sharing_a_byte_get_independent_statuses_under_concurrency() {
        // XIDs 100..104 occupy the four slots of one byte. If the read-modify-write
        // is not atomic, or the mask is wrong, a lost update leaves a slot at 0b00
        // — InProgress — and the assertion names which.
        let log = Clog::new();
        let want = [
            (Xid(100), XactStatus::Committed),
            (Xid(101), XactStatus::Aborted),
            (Xid(102), XactStatus::Aborted),
            (Xid(103), XactStatus::Committed),
        ];
        std::thread::scope(|scope| {
            for (xid, status) in want {
                let log = &log;
                scope.spawn(move || {
                    for _ in 0..10_000 {
                        log.set_status(xid, status);
                    }
                });
            }
        });
        for (xid, status) in want {
            assert_eq!(log.status(xid), status, "{xid:?}");
        }
    }

    #[test]
    fn every_xid_in_one_page_survives_concurrent_stamping() {
        // Eight threads over disjoint XIDs covering a whole page, so every byte has
        // four different owners and the sweep is repeated enough to make the
        // interleaving real.
        const THREADS: u64 = 8;
        let log = Clog::new();
        let status = |xid: Xid| {
            if xid.0.is_multiple_of(2) {
                XactStatus::Committed
            } else {
                XactStatus::Aborted
            }
        };
        std::thread::scope(|scope| {
            for t in 0..THREADS {
                let log = &log;
                scope.spawn(move || {
                    for _ in 0..20 {
                        for xid in (3..clog::XACTS_PER_PAGE).filter(|x| x % THREADS == t) {
                            log.set_status(Xid(xid), status(Xid(xid)));
                        }
                    }
                });
            }
        });
        for xid in 3..clog::XACTS_PER_PAGE {
            assert_eq!(log.status(Xid(xid)), status(Xid(xid)), "Xid({xid})");
        }
    }

    #[test]
    fn statuses_do_not_bleed_across_page_chunk_or_segment_boundaries() {
        // Every boundary the addressing crosses: the page, the index chunk (which
        // only the resident index knows about), and the segment file.
        let log = Clog::new();
        let boundaries = [
            clog::XACTS_PER_PAGE,
            PAGES_PER_CHUNK as u64 * clog::XACTS_PER_PAGE,
            clog::XACTS_PER_SEGMENT,
        ];
        for edge in boundaries {
            let below = Xid(edge - 1);
            let above = Xid(edge);
            log.set_committed(below);
            log.set_aborted(above);
            assert_eq!(log.status(below), XactStatus::Committed, "below {edge}");
            assert_eq!(log.status(above), XactStatus::Aborted, "above {edge}");
            // Their neighbours on the far side are untouched.
            assert_eq!(log.status(Xid(edge - 2)), XactStatus::InProgress);
            assert_eq!(log.status(Xid(edge + 1)), XactStatus::InProgress);
        }
    }

    #[test]
    fn the_reserved_xids_keep_their_meanings() {
        // FROZEN short-circuits ahead of the index entirely. `satisfies_mvcc` has its
        // own FROZEN branch for xmin, so losing this one would go unnoticed there —
        // but `HeapTable`'s `is_live` and the buffer engine read `Clog::status`
        // directly, and a frozen xmax reading InProgress flips a dead row live.
        //
        // INVALID is not short-circuited: it goes through the index like any other
        // XID and reads slot 0, which is InProgress because nothing has stamped it.
        // Both call sites above guard on `xmax.is_valid()` before consulting the
        // commit log, so that is sufficient — but it is not the same mechanism, and
        // this test's name used to claim otherwise.
        let log = Clog::new();
        assert_eq!(log.status(Xid::FROZEN), XactStatus::Committed);
        assert!(log.is_committed(Xid::FROZEN));
        assert_eq!(log.status(Xid::INVALID), XactStatus::InProgress);
        assert!(!log.is_committed(Xid::INVALID));
    }

    #[test]
    fn a_pathological_xid_is_refused_rather_than_allocated() {
        // `pageno = xid >> 15`, so without a ceiling a garbage XID out of a corrupt
        // tuple header would index terabytes off the end of the index. InProgress
        // is the fail-safe answer: it hides a row rather than resurrecting one.
        let log = Clog::new();
        for xid in [Xid(u64::MAX), Xid(MAX_PAGENO * clog::XACTS_PER_PAGE)] {
            assert_eq!(log.status(xid), XactStatus::InProgress, "{xid:?}");
            // And a stamp is dropped rather than growing the index to reach it.
            log.set_committed(xid);
            assert_eq!(log.status(xid), XactStatus::InProgress, "{xid:?}");
        }
        // The last addressable page still works.
        let highest = Xid(MAX_PAGENO * clog::XACTS_PER_PAGE - 1);
        log.set_committed(highest);
        assert_eq!(log.status(highest), XactStatus::Committed);
    }

    #[test]
    fn a_page_faulted_in_from_two_threads_keeps_both_stamps() -> anyhow::Result<()> {
        // Two threads racing to fault the same page in must end up sharing one
        // resident page. If each installs its own, the loser's stamp is discarded.
        // `A` is only on disk, so it also proves the read itself is not lost.
        let dir = tempfile::tempdir()?;
        let (a, b, c) = (Xid(3), Xid(4), Xid(5)); // one page, and b/c share a byte
        {
            let log = Clog::open(dir.path())?;
            log.set_committed(a);
            log.flush()?;
        }
        for _ in 0..200 {
            let log = Clog::open(dir.path())?;
            let gate = Barrier::new(2);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    gate.wait();
                    log.set_aborted(b);
                });
                scope.spawn(|| {
                    gate.wait();
                    log.set_committed(c);
                });
            });
            assert_eq!(log.status(a), XactStatus::Committed, "read from disk lost");
            assert_eq!(log.status(b), XactStatus::Aborted);
            assert_eq!(log.status(c), XactStatus::Committed);
        }
        Ok(())
    }

    #[test]
    fn no_stamp_is_lost_to_a_concurrent_flush() -> anyhow::Result<()> {
        // Stamping and checkpointing at the same time, over pages that are also
        // faulting in while the checkpoint walks the index. What this catches is a
        // page dropped from the index and a stamp that never reaches disk at all.
        //
        // Be precise about what it does *not* catch, because two plausible-sounding
        // claims are both false. It does not pin the claim-before-image ordering:
        // that window is a few instructions wide and reversing it still passes here
        // — and `ClogPage::claim_for_writeback`'s own test does not catch it either,
        // being single-threaded, so nothing in this crate does. Nor does it cover
        // `flush`'s re-flag-on-failure path: no write ever fails here, so that is
        // `a_failed_write_leaves_the_page_dirty_for_the_next_checkpoint`'s job.
        const WRITERS: u64 = 4;
        const PER_WRITER: u64 = 20_000;
        /// Checkpoints the writers must have overlapped before they may stop. The
        /// overlap *is* the thing under test, so without this the run could finish
        /// before the checkpoint thread was ever scheduled and prove nothing.
        const MIN_OVERLAPPED_FLUSHES: usize = 4;
        /// Ceiling on the wait for those checkpoints. Without it, a `flush` that
        /// fails every time — a full or read-only TMPDIR, an fsync error — would
        /// leave every thread spinning forever, turning a diagnosable failure into a
        /// wedged CI runner.
        const MAX_WAIT_SPINS: usize = 2_000_000;
        let dir = tempfile::tempdir()?;
        let log = Clog::open(dir.path())?;
        let done = AtomicBool::new(false);
        let flushes = AtomicUsize::new(0);
        // Bases half a page apart, so the writers between them dirty two pages and
        // the second page faults in while the checkpoint is already walking.
        let xid = |writer: u64, i: u64| Xid(3 + writer * clog::XACTS_PER_PAGE / 2 + i);

        let failures = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let flusher = scope.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    match log.flush() {
                        Ok(()) => flushes.fetch_add(1, Ordering::Relaxed),
                        Err(_) => failures.fetch_add(1, Ordering::Relaxed),
                    };
                    std::thread::yield_now();
                }
            });
            let writers: Vec<_> = (0..WRITERS)
                .map(|writer| {
                    let log = &log;
                    let flushes = &flushes;
                    scope.spawn(move || {
                        for i in 0..PER_WRITER {
                            log.set_committed(xid(writer, i));
                            // Hand the checkpoint thread a real chance to land
                            // between two stamps.
                            if i % 128 == 0 {
                                std::thread::yield_now();
                            }
                        }
                        // Keep the page dirty-and-being-stamped until enough
                        // checkpoints have gone by to have raced us — but bounded, so
                        // a persistently failing flush ends the test instead of
                        // hanging it.
                        let mut spins = 0;
                        while flushes.load(Ordering::Relaxed) < MIN_OVERLAPPED_FLUSHES
                            && spins < MAX_WAIT_SPINS
                        {
                            log.set_committed(xid(writer, PER_WRITER - 1));
                            std::thread::yield_now();
                            spins += 1;
                        }
                    })
                })
                .collect();
            // Join every writer, then stop the flusher, and only then assert. If a
            // writer panics its `join` returns `Err` rather than unwinding here, so
            // `done` is always set and `scope` can never block joining a flusher
            // that would otherwise loop forever.
            let writer_results: Vec<_> = writers.into_iter().map(|w| w.join()).collect();
            done.store(true, Ordering::Relaxed);
            let flusher_result = flusher.join();
            for result in writer_results {
                assert!(result.is_ok(), "a writer panicked");
            }
            assert!(flusher_result.is_ok(), "the checkpoint thread panicked");
        });

        let overlapped = flushes.load(Ordering::Relaxed);
        assert!(
            overlapped >= MIN_OVERLAPPED_FLUSHES,
            "only {overlapped} checkpoints succeeded ({} failed), so this proved nothing",
            failures.load(Ordering::Relaxed)
        );
        // The final checkpoint is the one that has to publish everything still
        // flagged from the concurrent ones.
        log.flush()?;
        drop(log);

        let reopened = Clog::open(dir.path())?;
        for writer in 0..WRITERS {
            for i in 0..PER_WRITER {
                assert_eq!(
                    reopened.status(xid(writer, i)),
                    XactStatus::Committed,
                    "{:?} was stamped but never reached disk",
                    xid(writer, i)
                );
            }
        }
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
