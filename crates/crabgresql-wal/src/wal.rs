//! The append/flush WAL stream with group-commit fsync.
//!
//! Records are staged into whole [`XLOG_BLCKSZ`] pages and written out as whole
//! pages at page-aligned offsets, split across [`WAL_SEG_SIZE`] segment files.
//! See [`crate::page`] for the layout and [`crate::reader`] for the rules that
//! make the end of the log knowable without the file's length.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

use crabgresql_txn::{CommitSink, Xid};

use crate::aligned::AlignedBuf;
use crate::control::read_control;
use crate::page::{
    PageHeader, XLOG_BLCKSZ, XLP_FIRST_IS_CONTRECORD, XLP_PAGE_HEADER_SIZE, advance,
    is_record_position, page_offset, page_start,
};
use crate::reader::end_of_wal;
use crate::record::{Lsn, LsnRange, WalError, WalRecord};
use crate::rmgr::{RmgrId, XACT_ABORT, XACT_COMMIT};
use crate::segment::{
    Segments, WAL_SEG_SIZE, seg_offset, segment_bounds, segment_start, segno_of, wal_dir,
};

/// Pages the staging buffer starts at, and is trimmed back to once a record big
/// enough to grow it has been flushed.
const WAL_BUF_PAGES: usize = 8;

/// The pre-paging log: one growing file at `<dir>/pg_wal/wal`. Nothing writes it
/// any more; [`Wal::open`] only looks for it, because a directory left over from
/// that build would otherwise read as an *empty* log — its first four bytes are a
/// record length, which is not a page magic — and an empty log is discarded
/// without a word.
fn legacy_wal_path(dir: &Path) -> PathBuf {
    wal_dir(dir).join("wal")
}

struct Inner {
    /// Whole pages of staged stream: `[buf_base, buf_base + buf.len())`. The last
    /// page is usually partly filled, and every byte past the fill mark is zero —
    /// which is what the flush writes out as that page's on-disk tail.
    buf: AlignedBuf,
    /// Stream LSN of `buf[0]`. Always page-aligned.
    buf_base: u64,
    /// End-LSN of everything appended so far. Always `buf_base + buf.len()`.
    insert_lsn: u64,
    /// Highest LSN whose bytes are on disk (may not yet be fsynced).
    written: u64,
    /// True while one thread owns the write+fsync; others coalesce behind it.
    flushing: bool,
}

impl Inner {
    /// Copy `bytes` into the staging buffer, opening a new page — header and all
    /// — each time the current one fills.
    fn put(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let room = (XLOG_BLCKSZ - page_offset(self.insert_lsn)) as usize;
            let take = room.min(bytes.len());
            self.buf.extend_from_slice(&bytes[..take]);
            self.insert_lsn += take as u64;
            bytes = &bytes[take..];
            if page_offset(self.insert_lsn) == 0 {
                self.open_page(bytes.len() as u32);
            }
        }
        debug_assert_eq!(self.insert_lsn, self.buf_base + self.buf.len() as u64);
    }

    /// Lay down the header of the page starting at `insert_lsn`.
    ///
    /// `rem_len` is how much of the record now in flight is still owed. Zero when
    /// the record ended flush with the page edge — and then the contrecord flag
    /// stays clear, even though the page was opened from inside [`Inner::put`],
    /// because nothing continues onto it.
    fn open_page(&mut self, rem_len: u32) {
        let mut header = [0u8; XLP_PAGE_HEADER_SIZE as usize];
        PageHeader {
            info: if rem_len > 0 { XLP_FIRST_IS_CONTRECORD } else { 0 },
            rem_len,
            pageaddr: self.insert_lsn,
        }
        .encode(&mut header);
        self.buf.extend_from_slice(&header);
        self.insert_lsn += XLP_PAGE_HEADER_SIZE;
    }
}

/// Where a scan for the end of the log must begin: the redo point `pg_control`
/// names, or [`Lsn::INVALID`] to let the reader resolve the head of the
/// surviving log for itself.
///
/// The redo point is what keeps an open from reading the whole log. It is
/// discarded when it names a segment recycling has already reclaimed — which the
/// checkpointer will not produce, but a stale or hand-written control file can —
/// because the reader's own resolution is the safe answer there, and it is not a
/// position this function can compute: the head of the lowest surviving segment
/// may be owed to a record that started in a segment now gone.
fn scan_origin(dir: &Path) -> Result<Lsn, WalError> {
    let redo = read_control(dir)?
        .map(|control| control.redo_lsn)
        .filter(|lsn| lsn.is_valid());
    let lowest = segment_bounds(dir)?.map(|(lo, _)| segment_start(lo));
    Ok(match (redo, lowest) {
        (Some(redo), Some(lowest)) if redo.0 >= lowest => redo,
        _ => Lsn::INVALID,
    })
}

/// Prime a staging buffer with the existing bytes of the page `lsn` sits in.
///
/// A flush writes whole pages, so resuming mid-page means re-emitting the bytes
/// below `lsn` — they are live log. The page's real header is read back verbatim
/// rather than regenerated: its contrecord flag may legitimately be set by a
/// record that began on the previous page and ended before `lsn`, and inventing a
/// cleared one would break that record on the next flush. A page that was never
/// written gets a generated header instead.
fn position_at(dir: &Path, lsn: Lsn) -> Result<(AlignedBuf, u64), WalError> {
    debug_assert!(is_record_position(lsn.0), "cannot position inside a page header");
    let base = page_start(lsn.0);
    let mut page = vec![0u8; XLOG_BLCKSZ as usize];
    let mut segs = Segments::new(dir);
    let found = segs.read_at(segno_of(base), seg_offset(base), &mut page)?;
    let intact = found && PageHeader::decode(&page).is_some_and(|h| h.pageaddr == base);
    if !intact {
        page.fill(0);
        PageHeader {
            info: 0,
            rem_len: 0,
            pageaddr: base,
        }
        .encode(&mut page);
    }
    let mut buf = AlignedBuf::with_pages(WAL_BUF_PAGES);
    buf.extend_from_slice(&page[..(lsn.0 - base) as usize]);
    Ok((buf, base))
}

/// Checkpoint-delay bookkeeping. Guarded by its own mutex, independent of
/// [`Inner`]: a writer holding a delay goes on to `append`, so the lock order is
/// always `delay` → `inner` and never the reverse.
struct Delay {
    /// Writers currently inside a "record appended, effect not yet published"
    /// window.
    active: u64,
    /// How many checkpointers are waiting to sample the redo point. New delays
    /// queue behind them, so a steady stream of writers cannot starve one.
    ///
    /// A count, not a flag: with a flag, the first sampler to finish would clear
    /// it while a second was still waiting, admitting writers again and starving
    /// exactly the caller the mechanism exists to protect.
    wanted: u64,
}

/// The write-ahead log. Cheap [`Wal::append`] stages bytes in memory and returns
/// the record's byte range; [`Wal::flush`] makes everything up to a target LSN
/// durable with a single fsync shared by all concurrent committers (group
/// commit).
pub struct Wal {
    dir: PathBuf,
    inner: Mutex<Inner>,
    /// Open segment handles, held only by the current flusher so appends proceed
    /// during an fsync.
    segments: Mutex<Segments>,
    /// The flusher's scratch copy of the staged pages. Held across the write so
    /// `inner` can be released and appends can continue into the same tail page.
    snapshot: Mutex<AlignedBuf>,
    /// Highest LSN known to be on stable storage (fsynced).
    flushed: AtomicU64,
    cond: Condvar,
    delay: Mutex<Delay>,
    delay_cond: Condvar,
}

/// Holds the checkpointer off while its owner finishes publishing the effect of
/// a record it has already appended. Releases on `Drop`.
///
/// The hazard it closes: a writer that appends a record and only afterwards
/// makes the state deciding whether that record must be replayed visible. A
/// checkpointer sampling the redo point inside that window would publish a redo
/// above the record while the effect is neither on disk nor in the replayed
/// suffix — the change is simply lost.
///
/// Most writers need nothing: the heap `INSERT`/`DELETE` path appends the record
/// and stamps the page inside one buffer-pool `modify` closure, so both become
/// visible under a single frame lock and no window exists. This guard is for the
/// writers that genuinely cannot do that. There are three:
///
/// * a B-tree split — one record over three separately locked pages;
/// * a transaction commit or abort, where the record is appended and only then is
///   the CLOG bit that decides its fate set (see [`CommitSink::delay_checkpoint`]);
/// * a buffer-table install, where the record is appended and only then are the
///   rows — whose sole durable trace it is — installed and counted;
/// * a heap or Parquet TRUNCATE staging its swap, where the record is appended and
///   only then does the relation start reporting that replay must reach it.
///
/// Note what is *not* on the list: the heap `INSERT`/`DELETE` path, and page
/// write-back generally. Those are covered by the frame lock instead — the record
/// and the page stamp happen inside one `modify` closure — and by the storage
/// manager's pending-fsync queue, which remembers a page write the checkpoint
/// itself did not make.
///
/// The guard is **non-reentrant**: a thread holding one must not call
/// [`Wal::redo_point`], which would wait for itself. A checkpointer must also
/// let `redo_point` return *before* flushing buffers, so it never holds this
/// barrier while taking buffer-pool frame mutexes.
pub struct CheckpointDelay<'a> {
    wal: &'a Wal,
}

/// Counts one checkpointer as waiting to sample, and stops counting it on drop.
///
/// RAII rather than a matching decrement so that a panic anywhere inside the
/// barrier — `current_lsn()` on a poisoned `inner`, most plausibly — cannot leave
/// `wanted` incremented. That count is what makes new writers queue behind a
/// waiting checkpointer, so leaking it wedges every commit, abort, buffer install
/// and B-tree split in the process.
struct SamplerSlot<'a> {
    wal: &'a Wal,
}

impl Drop for SamplerSlot<'_> {
    fn drop(&mut self) {
        let mut delay = self
            .wal
            .delay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        delay.wanted -= 1;
        self.wal.delay_cond.notify_all();
    }
}

impl Drop for CheckpointDelay<'_> {
    fn drop(&mut self) {
        // Never panics on a poisoned lock. This `Drop` runs during unwinding by
        // construction — any panic between taking the guard and here reaches it —
        // and a panicking `Drop` mid-unwind aborts the process. `Delay` is two
        // counters with no fallible step between reading and writing them, so a
        // poisoned guard cannot be observing a torn value.
        let mut delay = self
            .wal
            .delay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        delay.active -= 1;
        if delay.active == 0 {
            self.wal.delay_cond.notify_all();
        }
    }
}

impl Wal {
    /// Open the WAL under `dir`, positioned after the last valid record.
    ///
    /// The position comes from a **scan**, not from a file's length: segments are
    /// preallocated and the tail page is zero-padded, so length says nothing.
    /// The scan is bounded by the redo point in `pg_control` — everything below
    /// it is durable by construction — and uses the same reader replay does, so
    /// the two cannot disagree about where the log ends.
    ///
    /// Opening **writes nothing**: no segment is preallocated and no header is
    /// stamped. [`crate::recover`] reads these files immediately afterwards and
    /// must not be shown bytes this invented.
    ///
    /// After a crash, call [`crate::recover`] and then [`Wal::reset_to`] with the
    /// end it reports; that is the authority on where to resume.
    pub fn open(dir: &Path) -> Result<Wal, WalError> {
        std::fs::create_dir_all(wal_dir(dir))?;
        if legacy_wal_path(dir).exists() {
            return Err(WalError::IncompatibleWalFormat {
                detail: format!(
                    "{} is a pre-paged write-ahead log",
                    legacy_wal_path(dir).display()
                ),
            });
        }
        let end = end_of_wal(dir, scan_origin(dir)?)?;
        let (buf, buf_base) = position_at(dir, end)?;
        Ok(Wal {
            dir: dir.to_path_buf(),
            inner: Mutex::new(Inner {
                buf,
                buf_base,
                insert_lsn: end.0,
                written: end.0,
                flushing: false,
            }),
            segments: Mutex::new(Segments::new(dir)),
            snapshot: Mutex::new(AlignedBuf::with_pages(WAL_BUF_PAGES)),
            flushed: AtomicU64::new(end.0),
            cond: Condvar::new(),
            delay: Mutex::new(Delay {
                active: 0,
                wanted: 0,
            }),
            delay_cond: Condvar::new(),
        })
    }

    /// Take a checkpoint delay, blocking while a checkpointer is already waiting
    /// to sample. See [`CheckpointDelay`] for when a writer needs one.
    ///
    /// Deliberately `Mutex` + `Condvar` rather than an `RwLock`: on macOS
    /// `std::sync::RwLock` wraps a reader-preferring `pthread_rwlock`, so a
    /// steady stream of delay holders could starve the checkpointer forever.
    /// `wanted` is what makes the checkpointer's side win.
    pub fn delay_checkpoint(&self) -> CheckpointDelay<'_> {
        // Non-poisoning for the same reason as `Drop for CheckpointDelay`: a
        // transaction abort takes one of these while its session is already
        // unwinding, and panicking there would turn one backend's failure into a
        // process abort. Safe only because `wanted` is restored by RAII in
        // `redo_point` — without that, a panic inside the barrier would leave it
        // incremented and wedge every writer here forever.
        let mut delay = self
            .delay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while delay.wanted > 0 {
            delay = match self.delay_cond.wait(delay) {
                Ok(delay) => delay,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
        delay.active += 1;
        CheckpointDelay { wal: self }
    }

    /// The redo point — a record boundary at or above which replay must resume.
    /// Blocks until every outstanding [`CheckpointDelay`] is released, so on
    /// return every record below the result has its effect published, and the
    /// result is **durable**: the returned LSN is always backed by bytes on
    /// disk.
    ///
    /// The flush is not a convenience. Recovery hard-errors when asked to resume
    /// past end-of-file, so publishing a redo point sampled from the staged
    /// insert position — which [`Wal::current_lsn`] documents as "durable or
    /// not" — would leave a cluster that refuses to start after a crash.
    /// Recovery also relies on it in the other direction: because the redo point
    /// never exceeds the flush boundary, a record torn *at* the redo point is
    /// impossible, which is what lets a failure to decode there be treated as a
    /// bad redo point rather than an ordinary torn tail.
    ///
    /// A checkpoint must sample this **before** flushing buffers, never after:
    /// sampling afterwards would let a page dirtied during the flush pass carry
    /// an LSN below the redo point, leaving it neither written back nor
    /// replayed.
    pub fn redo_point(&self) -> Result<Lsn, WalError> {
        let lsn = {
            // `wanted` is restored by RAII, not by the line below, because
            // `current_lsn()` locks `inner` and *does* panic on poison — by
            // design, since `Inner` has a genuinely torn state. Decrementing by
            // hand would leak the count on that path and block every future
            // writer in `delay_checkpoint` forever.
            let _slot = SamplerSlot { wal: self };
            let mut delay = self
                .delay
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            delay.wanted += 1;
            while delay.active > 0 {
                delay = match self.delay_cond.wait(delay) {
                    Ok(delay) => delay,
                    Err(poisoned) => poisoned.into_inner(),
                };
            }
            drop(delay);
            // Lock order delay -> inner; nothing takes them the other way round.
            self.current_lsn()
        };
        // Outside the barrier: an fsync here would otherwise block every writer
        // taking a delay, for no benefit — the sample is already fixed.
        self.flush(lsn)?;
        Ok(lsn)
    }

    /// Reposition the writer at `lsn`, discarding anything above it. Used after
    /// recovery to drop a torn tail so new records overwrite the garbage.
    ///
    /// There is no truncation to do, and none is needed. Segments are
    /// preallocated, so the files are full-length regardless; what makes the
    /// bytes above `lsn` unreachable is that a flush writes **whole pages** from
    /// a buffer that is zero past its fill mark. The next flush therefore zeroes
    /// the rest of `lsn`'s page, and a reader — which walks forward and never
    /// seeks — stops there and never consults the pages beyond it.
    ///
    /// Before that first flush, the page still holds what a crash left, which is
    /// exactly the bytes recovery already declined to decode.
    pub fn reset_to(&self, lsn: Lsn) -> Result<(), WalError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let (buf, buf_base) = position_at(&self.dir, lsn)?;
        inner.buf = buf;
        inner.buf_base = buf_base;
        inner.insert_lsn = lsn.0;
        inner.written = lsn.0;
        self.flushed.store(lsn.0, Ordering::SeqCst);
        Ok(())
    }

    /// Reclaim the segments below `redo`, which must already be durable in
    /// `pg_control`: reclaiming ahead of that publication would leave a control
    /// file naming a segment that is gone.
    ///
    /// An invalid `redo` reclaims nothing. That is not a shortcut — a checkpoint
    /// that cannot bound replay publishes [`Lsn::INVALID`], meaning "resume from
    /// the head of the stream", and every segment is then still needed.
    pub fn recycle(&self, redo: Lsn) -> Result<(), WalError> {
        if !redo.is_valid() {
            return Ok(());
        }
        let highest = segno_of(self.current_lsn().0);
        let mut segs = self
            .segments
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        segs.recycle_below(segno_of(redo.0), highest)
    }

    /// Stage one record for a resource manager, returning the byte range it
    /// occupies. No I/O and no fsync — the caller stamps the range's `end` on the
    /// page it changed and, at commit, calls [`Wal::flush`] to make it durable.
    /// The `start` is the record's own boundary, which is what a redo point has
    /// to name.
    pub fn append(&self, rmgr: RmgrId, info: u8, xid: Xid, payload: &[u8]) -> LsnRange {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let start = Lsn(inner.insert_lsn);
        let mut encoded = Vec::new();
        let n = WalRecord {
            rec_lsn: start,
            xid,
            rmgr: rmgr.0,
            info,
            payload,
        }
        .encode(&mut encoded);
        // The reader refuses anything longer, so producing one would write a
        // record that can never be replayed.
        debug_assert!(n <= WalRecord::MAX_LEN, "wal record of {n} bytes is too long");
        inner.put(&encoded);
        // Two independent implementations of the header-skipping arithmetic now
        // exist — this one and the reader's — and the log is only readable if
        // they agree.
        debug_assert_eq!(inner.insert_lsn, advance(start.0, n as u64));
        LsnRange {
            start,
            end: Lsn(inner.insert_lsn),
        }
    }

    /// Highest LSN staged (durable or not).
    pub fn current_lsn(&self) -> Lsn {
        Lsn(self
            .inner
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .insert_lsn)
    }

    /// Highest LSN guaranteed on stable storage.
    pub fn flushed_lsn(&self) -> Lsn {
        Lsn(self.flushed.load(Ordering::SeqCst))
    }

    /// Make everything up to `up_to` durable. Concurrent callers coalesce: one
    /// thread drains the staged bytes, writes them, and issues a single fsync;
    /// every other caller whose target is covered returns without its own fsync.
    pub fn flush(&self, up_to: Lsn) -> Result<(), WalError> {
        if self.flushed.load(Ordering::SeqCst) >= up_to.0 {
            return Ok(());
        }
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        // Nothing can be made durable past what was appended, and saying so is not
        // pedantry: the loop below can only advance `written` by draining staged
        // bytes, so a target above `insert_lsn` would drain an empty buffer and
        // recheck the same condition forever. Unreachable while a caller flushes an
        // LSN `append` handed it — but a cached page whose `pd_lsn` predates a
        // `reset_to` names an LSN the stream no longer has, and `reset_to` is the
        // routine path now that recovery clamps a torn tail on every start. Fail
        // where it can be diagnosed instead of hanging the checkpoint that hit it.
        if up_to.0 > inner.insert_lsn {
            return Err(WalError::FlushPastEnd {
                target: up_to,
                appended: Lsn(inner.insert_lsn),
            });
        }
        loop {
            if self.flushed.load(Ordering::SeqCst) >= up_to.0 {
                return Ok(());
            }
            if inner.flushing {
                // Another thread is mid-fsync; wait and recheck.
                inner = match self.cond.wait(inner) {
                    Ok(inner) => inner,
                    Err(_) => panic!("WAL flush condition-variable mutex poisoned"),
                };
                continue;
            }
            // Become the flusher for everything appended so far. The staged bytes
            // are *copied*, not drained: a concurrent `append` must keep seeing
            // the partly filled tail page, since the next flush rewrites it in
            // place. The copy also makes the failure path trivial — nothing was
            // removed, so nothing has to be put back.
            inner.flushing = true;
            let base = inner.buf_base;
            let filled = inner.buf.len();
            let target = inner.insert_lsn;
            {
                let mut snapshot = self
                    .snapshot
                    .lock()
                    .unwrap_or_else(|_| panic!("mutex poisoned"));
                snapshot.clear();
                snapshot.extend_from_slice(inner.buf.whole_pages());
            }
            drop(inner);

            let write_result = self.write_pages(base);

            inner = self
                .inner
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"));
            inner.flushing = false;
            self.cond.notify_all();
            match write_result {
                Ok(()) => {
                    inner.written = target;
                    self.flushed.store(target, Ordering::SeqCst);
                    // Retire only the pages that are now complete. The partial
                    // tail stays: the next flush writes it again, which is what
                    // PostgreSQL does too and what zeroes the bytes past it.
                    let complete = filled / XLOG_BLCKSZ as usize * XLOG_BLCKSZ as usize;
                    inner.buf.drain_front(complete);
                    inner.buf_base += complete as u64;
                    inner.buf.shrink_to_pages(WAL_BUF_PAGES);
                    debug_assert_eq!(inner.insert_lsn, inner.buf_base + inner.buf.len() as u64);
                }
                Err(e) => return Err(e),
            }
            // target >= up_to because up_to was appended before this call, so the
            // snapshot captured it; the top-of-loop check returns Ok.
        }
    }

    /// Write the snapshot's whole pages at stream offset `base`, splitting across
    /// segment files and fsyncing each one touched.
    ///
    /// `base` is page-aligned and the length is a whole number of pages, which is
    /// the point of the whole exercise: a partial write into the middle of a
    /// block is a read-modify-write at the filesystem and device level, and it is
    /// what puts direct I/O out of reach.
    fn write_pages(&self, base: u64) -> Result<(), WalError> {
        let snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let mut segs = self
            .segments
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let mut at = base;
        let mut rest = snapshot.as_slice();
        while !rest.is_empty() {
            let offset = seg_offset(at);
            let take = ((WAL_SEG_SIZE - offset) as usize).min(rest.len());
            segs.write_at(segno_of(at), offset, &rest[..take])?;
            at += take as u64;
            rest = &rest[take..];
        }
        Ok(())
    }
}

/// The WAL is the transaction manager's durable commit log: a commit appends a
/// commit record and fsyncs it (the durability boundary); an abort appends an
/// abort record but needs no fsync.
impl CommitSink for Wal {
    fn log_commit(&self, xid: Xid) -> std::io::Result<()> {
        let lsn = self.append(RmgrId::XACT, XACT_COMMIT, xid, &[]).end;
        self.flush(lsn).map_err(|e| match e {
            WalError::Io(io) => io,
            other => std::io::Error::other(other.to_string()),
        })
    }

    fn log_abort(&self, xid: Xid) {
        self.append(RmgrId::XACT, XACT_ABORT, xid, &[]);
    }

    /// The commit path is one of the writers [`CheckpointDelay`] exists for: it
    /// appends a record and only afterwards publishes the state that decides
    /// whether that record must be replayed (the CLOG bit).
    fn delay_checkpoint(&self) -> Option<Box<dyn Send + '_>> {
        // `CheckpointDelay` borrows this `Wal`, and `Wal` is `Sync`, so the guard
        // is `Send` and can cross to whatever thread drops it.
        Some(Box::new(Wal::delay_checkpoint(self)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::XLP_USABLE;
    use std::sync::Arc;

    #[test]
    fn append_returns_monotonic_end_lsns_and_flush_persists() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Wal::open(dir.path())?;
        let a = wal.append(RmgrId::HEAP, 0, Xid(3), &[1, 2, 3]).end;
        let b = wal.append(RmgrId::HEAP, 0, Xid(3), &[4, 5]).end;
        assert!(b > a);
        assert_eq!(wal.current_lsn(), b);
        assert_eq!(wal.flushed_lsn(), Lsn::START);
        wal.flush(b)?;
        assert_eq!(wal.flushed_lsn(), b);
        // Reopen: the durable position comes from a scan of the pages, since a
        // preallocated segment's length says nothing.
        drop(wal);
        let wal2 = Wal::open(dir.path())?;
        assert_eq!(wal2.current_lsn(), b);
        assert_eq!(wal2.flushed_lsn(), b);

        Ok(())
    }

    #[test]
    fn group_commit_coalesces_concurrent_flushers() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        std::thread::scope(|s| -> anyhow::Result<()> {
            let mut handles = Vec::new();
            for i in 0..8u64 {
                let wal = Arc::clone(&wal);
                handles.push(s.spawn(move || -> Result<Lsn, WalError> {
                    let lsn = wal
                        .append(RmgrId::XACT, XACT_COMMIT, Xid(3 + i), &[i as u8; 16])
                        .end;
                    wal.flush(lsn)?;
                    Ok(lsn)
                }));
            }
            for h in handles {
                let lsn = h
                    .join()
                    .map_err(|_| anyhow::anyhow!("flush worker panicked"))??;
                assert!(wal.flushed_lsn() >= lsn, "flush must be durable on return");
            }
            Ok(())
        })?;

        Ok(())
    }

    /// Decode every record in the on-disk WAL, returning `(xid, payload)` pairs.
    fn read_all(dir: &Path) -> Vec<(Xid, Vec<u8>)> {
        match crate::reader::testkit::read_all(dir, Lsn::INVALID) {
            Ok((records, _)) => records,
            Err(error) => panic!("failed to read WAL test fixture: {error}"),
        }
    }

    #[test]
    fn appending_after_reopen_preserves_earlier_records() -> anyhow::Result<()> {
        // Regression: flush must write at the logical offset, not the reset OS
        // cursor, or the first append after a reopen clobbers the log head.
        let dir = tempfile::tempdir()?;
        {
            let wal = Wal::open(dir.path())?;
            wal.append(RmgrId::HEAP, 0, Xid(3), b"first");
            let l = wal.append(RmgrId::HEAP, 0, Xid(4), b"second").end;
            wal.flush(l)?;
        }
        {
            // Reopen (cursor at 0, written at end-of-file) and append more.
            let wal = Wal::open(dir.path())?;
            wal.append(RmgrId::HEAP, 0, Xid(5), b"third");
            let l = wal.append(RmgrId::HEAP, 0, Xid(6), b"fourth").end;
            wal.flush(l)?;
        }
        // All four records from both sessions must decode intact and in order.
        assert_eq!(
            read_all(dir.path()),
            vec![
                (Xid(3), b"first".to_vec()),
                (Xid(4), b"second".to_vec()),
                (Xid(5), b"third".to_vec()),
                (Xid(6), b"fourth".to_vec()),
            ]
        );

        Ok(())
    }

    #[test]
    fn reset_to_discards_a_torn_tail_before_appending() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let valid_end;
        {
            let wal = Wal::open(dir.path())?;
            wal.append(RmgrId::HEAP, 0, Xid(3), b"good");
            valid_end = wal.append(RmgrId::XACT, XACT_COMMIT, Xid(3), &[]).end;
            wal.flush(valid_end)?;
        }
        // A crash leaves raw garbage on disk past the last valid record.
        crate::segment::scribble(dir.path(), valid_end, Lsn(valid_end.0 + 37), 0xAB)?;
        {
            let wal = Wal::open(dir.path())?;
            // Recovery computes `valid_end`; reposition there and continue
            // appending. The next flush rewrites the whole page, so the garbage
            // is gone without any truncation.
            wal.reset_to(valid_end)?;
            let l = wal.append(RmgrId::XACT, XACT_COMMIT, Xid(10), &[]).end;
            wal.flush(l)?;
        }
        let recs = read_all(dir.path());
        let xids: Vec<Xid> = recs.iter().map(|(x, _)| *x).collect();
        assert_eq!(
            xids,
            vec![Xid(3), Xid(3), Xid(10)],
            "torn tail dropped, new record appended cleanly"
        );

        Ok(())
    }

    #[test]
    fn append_returns_the_records_own_byte_range() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Wal::open(dir.path())?;
        let mut previous_end = Lsn::START;
        for i in 0..4usize {
            let payload = vec![i as u8; i * 11];
            let range = wal.append(RmgrId::HEAP, 0, Xid(3), &payload);
            // Contiguity is the contract the buffer pool's write-ahead check
            // leans on, and it survives paging unchanged.
            assert_eq!(range.start, previous_end, "ranges must be contiguous");
            assert_eq!(
                range.end.0 - range.start.0,
                (WalRecord::MIN_LEN + payload.len()) as u64,
                "a record wholly inside one page occupies exactly its own bytes"
            );
            previous_end = range.end;
        }
        assert_eq!(wal.current_lsn(), previous_end);

        Ok(())
    }

    /// The width of a range is no longer the encoded length: a record crossing a
    /// page boundary also spans the header the writer laid down in front of it.
    #[test]
    fn a_range_that_crosses_a_page_widens_by_the_header_it_spans() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Wal::open(dir.path())?;
        // Leave 40 bytes of the first page, then write a record that will not fit.
        let filler = XLP_USABLE as usize - 40 - WalRecord::MIN_LEN;
        wal.append(RmgrId::HEAP, 0, Xid(3), &vec![0xAA; filler]);
        let range = wal.append(RmgrId::HEAP, 0, Xid(3), &[0xBB; 200]);
        assert_eq!(
            range.end.0 - range.start.0,
            (WalRecord::MIN_LEN + 200) as u64 + XLP_PAGE_HEADER_SIZE
        );
        wal.flush(range.end)?;
        assert_eq!(read_all(dir.path()).len(), 2);

        Ok(())
    }

    /// A target above the insert position used to spin this loop forever: the
    /// drain cannot advance `written` to bytes that were never staged. Reachable
    /// after `reset_to` shrinks the stream while a cached page still carries an
    /// older, higher `pd_lsn`.
    #[test]
    fn flushing_past_the_end_of_the_stream_is_an_error_not_a_spin() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let range = wal.append(RmgrId::HEAP, 0, Xid(3), b"only");
        wal.flush(range.end)?;
        wal.reset_to(Lsn::START)?;

        // Under a watchdog: without the guard this call does not return a wrong
        // answer, it never returns at all, and a bare call here would hang the
        // whole test binary instead of failing it.
        let result = {
            let wal = Arc::clone(&wal);
            within(5_000, move || wal.flush(range.end))
        };
        let Some(outcome) = result else {
            anyhow::bail!("flush spun instead of returning an error");
        };
        let Err(err) = outcome else {
            anyhow::bail!("flushing past the end of the stream must fail");
        };
        assert!(
            err.to_string()
                .contains(&format!("nothing above {} has been appended", Lsn::START)),
            "unexpected error: {err}"
        );

        Ok(())
    }

    #[test]
    fn redo_point_is_the_insert_lsn_when_nothing_is_delayed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Wal::open(dir.path())?;
        assert_eq!(wal.redo_point()?, Lsn::START);
        let range = wal.append(RmgrId::HEAP, 0, Xid(3), b"a");
        assert_eq!(wal.redo_point()?, range.end);
        // Repeatable: sampling does not consume anything.
        assert_eq!(wal.redo_point()?, wal.current_lsn());

        Ok(())
    }

    /// The redo point must be backed by bytes on disk: recovery hard-errors when
    /// asked to resume where there is no page, so publishing one would leave a
    /// cluster that refuses to start.
    ///
    /// "Backed by bytes" no longer means "inside the file's length" — a
    /// preallocated segment is full-length from the moment it exists. It means
    /// the page holding the redo point is written, records its own address, and
    /// has a record decoding at exactly that LSN.
    #[test]
    fn redo_point_is_durable_on_return() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Wal::open(dir.path())?;
        // Staged but never explicitly flushed.
        let range = wal.append(RmgrId::HEAP, 0, Xid(3), b"staged-only");
        assert_eq!(wal.flushed_lsn(), Lsn::START, "nothing flushed yet");

        let redo = wal.redo_point()?;
        assert_eq!(redo, range.end);
        assert!(
            wal.flushed_lsn() >= redo,
            "redo_point must flush through the LSN it returns"
        );
        // The record *below* the redo point is on disk, and the page carrying the
        // redo point itself records the address it sits at.
        assert_eq!(read_all(dir.path()), vec![(Xid(3), b"staged-only".to_vec())]);
        let mut page = vec![0u8; XLOG_BLCKSZ as usize];
        let mut segs = Segments::new(dir.path());
        let page_lsn = page_start(redo.0);
        assert!(segs.read_at(segno_of(page_lsn), seg_offset(page_lsn), &mut page)?);
        let header = PageHeader::decode(&page)
            .ok_or_else(|| anyhow::anyhow!("the redo point's page is not on disk"))?;
        assert_eq!(header.pageaddr, page_lsn);

        Ok(())
    }

    /// Poll `f()` for up to `max_ms`, returning whether it ever held. A positive
    /// assertion wants a generous ceiling (slow CI must not fail); a negative
    /// one ("this must NOT happen") wants a short window, since it always waits
    /// the full duration.
    fn eventually(max_ms: u64, f: impl Fn() -> bool) -> bool {
        for _ in 0..max_ms {
            if f() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        false
    }

    /// Run `f` with a watchdog: `Some(value)` if it finished inside `max_ms`,
    /// `None` if it is still running.
    ///
    /// For the tests whose regression is *non-termination* — a lock that never
    /// releases, a loop that never advances. `std::thread::spawn`, deliberately
    /// not `thread::scope`: scope joins before returning, so it would inherit the
    /// hang it exists to detect. The stuck thread is left detached and dies with
    /// the test process; that only happens when the test is already failing.
    fn within<T: Send + 'static>(max_ms: u64, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(std::time::Duration::from_millis(max_ms))
            .ok()
    }

    #[test]
    fn redo_point_blocks_until_every_delay_is_released() -> anyhow::Result<()> {
        use std::sync::atomic::AtomicBool;

        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let before = wal.append(RmgrId::HEAP, 0, Xid(3), b"below-redo");

        let delay = wal.delay_checkpoint();
        // Appended while the delay is held: the effect is not published yet, so
        // a redo point sampled now must not sit above this record.
        let during = wal.append(RmgrId::HEAP, 0, Xid(3), b"in-window");

        let sampled = Arc::new(AtomicBool::new(false));
        std::thread::scope(|s| -> anyhow::Result<()> {
            let handle = {
                let wal = Arc::clone(&wal);
                let sampled = Arc::clone(&sampled);
                s.spawn(move || {
                    let lsn = wal.redo_point();
                    sampled.store(true, Ordering::SeqCst);
                    lsn
                })
            };

            // The sampler must still be blocked while the delay is outstanding.
            assert!(
                !eventually(300, || sampled.load(Ordering::SeqCst)),
                "redo_point returned while a CheckpointDelay was held"
            );
            assert!(before.end <= during.start);

            drop(delay);
            let lsn = handle
                .join()
                .map_err(|_| anyhow::anyhow!("redo_point sampler panicked"))??;
            assert!(
                lsn >= during.end,
                "once the delay is released the sample covers the whole window"
            );
            Ok(())
        })?;

        Ok(())
    }

    #[test]
    fn a_new_delay_queues_behind_a_waiting_checkpointer() -> anyhow::Result<()> {
        use std::sync::atomic::AtomicBool;

        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let held = wal.delay_checkpoint();

        let sampling = Arc::new(AtomicBool::new(false));
        let took_second = Arc::new(AtomicBool::new(false));

        std::thread::scope(|s| -> anyhow::Result<()> {
            let sampler = {
                let wal = Arc::clone(&wal);
                let sampling = Arc::clone(&sampling);
                s.spawn(move || {
                    sampling.store(true, Ordering::SeqCst);
                    wal.redo_point()
                })
            };
            assert!(
                eventually(2_000, || sampling.load(Ordering::SeqCst)),
                "sampler thread never started"
            );

            // Give the sampler time to set `wanted` while `held` blocks it.
            let waiter = {
                let wal = Arc::clone(&wal);
                let took_second = Arc::clone(&took_second);
                s.spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    let guard = wal.delay_checkpoint();
                    took_second.store(true, Ordering::SeqCst);
                    drop(guard);
                })
            };

            // The second delay must not be granted while the checkpointer waits:
            // that is what stops a steady writer stream from starving it.
            assert!(
                !eventually(400, || took_second.load(Ordering::SeqCst)),
                "a new delay was granted ahead of a waiting checkpointer"
            );

            drop(held);
            sampler
                .join()
                .map_err(|_| anyhow::anyhow!("redo_point sampler panicked"))??;
            waiter
                .join()
                .map_err(|_| anyhow::anyhow!("delay waiter panicked"))?;
            assert!(took_second.load(Ordering::SeqCst));
            Ok(())
        })?;

        Ok(())
    }

    /// Poison the delay lock, then prove every path a dying session takes still
    /// works. A panicking `Drop` during unwinding aborts the process, so the guard
    /// released here is the one that matters most.
    #[test]
    fn a_poisoned_delay_lock_does_not_take_the_process_down() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);

        // Poison `delay` the way `redo_point` would: panic while holding it.
        let poisoning = {
            let wal = Arc::clone(&wal);
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _guard = wal.delay.lock().unwrap_or_else(|e| e.into_inner());
                panic!("poison the delay lock");
            }))
        };
        assert!(poisoning.is_err(), "the helper was supposed to panic");
        assert!(wal.delay.is_poisoned());

        // All three must still work: take a guard, drop it, and sample.
        let guard = wal.delay_checkpoint();
        drop(guard);
        let lsn = wal.redo_point()?;
        assert_eq!(lsn, Lsn::START);

        Ok(())
    }

    /// A panic inside the barrier must not leave `wanted` incremented: that count
    /// is what makes writers queue behind a waiting checkpointer, so leaking it
    /// blocks every commit, abort and buffer install in the process forever.
    #[test]
    fn a_panic_inside_the_barrier_does_not_wedge_every_writer() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);

        // Poison `inner`, which is what makes `redo_point` panic for real: it
        // raises `wanted`, then calls `current_lsn()`, which locks `inner` and
        // panics on poison by design. Driving the actual function matters — a test
        // that built a `SamplerSlot` by hand would pass even if `redo_point`
        // stopped using one.
        let poisoning = {
            let wal = Arc::clone(&wal);
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _guard = wal.inner.lock().unwrap_or_else(|e| e.into_inner());
                panic!("poison the inner lock");
            }))
        };
        assert!(poisoning.is_err(), "the helper was supposed to panic");

        let panicking = {
            let wal = Arc::clone(&wal);
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || wal.redo_point()))
        };
        assert!(
            panicking.is_err(),
            "redo_point must still panic on a poisoned `inner` — that poison is real"
        );

        // Under a watchdog: the regression is that this blocks forever, not that
        // it returns something wrong.
        let took = {
            let wal = Arc::clone(&wal);
            within(2_000, move || {
                drop(wal.delay_checkpoint());
            })
        };
        assert!(
            took.is_some(),
            "a panic inside the barrier left `wanted` raised, so no writer can proceed"
        );

        Ok(())
    }

    /// Which half of the transaction lifecycle a [`GateSink`] parks in.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum GateOp {
        Commit,
        Abort,
    }

    /// A [`CommitSink`] that parks in the middle of the commit (or abort) window —
    /// after the record is appended, before the caller stamps the CLOG — so the
    /// checkpointer can be raced against that exact window with the ordering
    /// imposed by a gate rather than by a sleep.
    struct GateSink {
        wal: Arc<Wal>,
        gate_on: GateOp,
        state: Mutex<GateState>,
        cond: Condvar,
    }

    struct GateState {
        entered: bool,
        release: bool,
    }

    impl GateSink {
        fn new(wal: Arc<Wal>, gate_on: GateOp) -> GateSink {
            GateSink {
                wal,
                gate_on,
                state: Mutex::new(GateState {
                    entered: false,
                    release: false,
                }),
                cond: Condvar::new(),
            }
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
            self.state
                .lock()
                .unwrap_or_else(|_| panic!("gate poisoned"))
        }

        fn entered(&self) -> bool {
            self.lock().entered
        }

        fn release(&self) {
            self.lock().release = true;
            self.cond.notify_all();
        }

        /// Block until [`GateSink::release`], announcing arrival first.
        fn park(&self) {
            let mut state = self.lock();
            state.entered = true;
            self.cond.notify_all();
            while !state.release {
                state = match self.cond.wait(state) {
                    Ok(state) => state,
                    Err(_) => panic!("gate condition-variable mutex poisoned"),
                };
            }
        }
    }

    /// Releases its gate on drop, including while a panic unwinds.
    ///
    /// Not a convenience: a thread parked in [`GateSink::park`] is joined by
    /// `thread::scope` *before* a panic propagates out of the scope, so an
    /// `assert!` that fires while the gate is still shut turns a regression into
    /// a hung test instead of a failing one. Holding one of these for the whole
    /// scope means the assertions can be written in their natural order.
    struct GateRelease<'a>(&'a GateSink);

    impl Drop for GateRelease<'_> {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    impl CommitSink for GateSink {
        fn log_commit(&self, xid: Xid) -> std::io::Result<()> {
            self.wal.log_commit(xid)?;
            if self.gate_on == GateOp::Commit {
                self.park();
            }
            Ok(())
        }

        fn log_abort(&self, xid: Xid) {
            self.wal.log_abort(xid);
            if self.gate_on == GateOp::Abort {
                self.park();
            }
        }

        fn delay_checkpoint(&self) -> Option<Box<dyn Send + '_>> {
            Some(Box::new(self.wal.delay_checkpoint()))
        }
    }

    /// Park a transaction inside its record-appended-but-fate-not-yet-stamped
    /// window and race a redo-point sample against it. Returns whether the
    /// sampler got through (it must not) and the LSN it eventually returned.
    ///
    /// Shared by the commit and abort cases, which differ only in which sink
    /// method parks and how "the fate is not stamped yet" is spelled.
    fn race_a_sample_against(
        gate_on: GateOp,
        drive: impl FnOnce(&Arc<crabgresql_txn::TransactionManager>, Xid) + Send,
        fate_is_unstamped: impl Fn(&crabgresql_txn::Clog, Xid) -> bool,
        fate_is_stamped: impl Fn(&crabgresql_txn::Clog, Xid) -> bool,
    ) -> anyhow::Result<()> {
        use std::sync::atomic::AtomicBool;

        use crabgresql_txn::{Clog, TransactionManager};

        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let gate = Arc::new(GateSink::new(Arc::clone(&wal), gate_on));
        let clog = Arc::new(Clog::new());
        let txnmgr = Arc::new(TransactionManager::new_recovered(
            Arc::clone(&gate) as Arc<dyn CommitSink>,
            Arc::clone(&clog),
            Xid::FIRST_NORMAL,
        ));
        let xid = txnmgr.allocate_xid();

        let sampled = Arc::new(AtomicBool::new(false));
        std::thread::scope(|s| -> anyhow::Result<()> {
            // Held for the whole scope, so every assertion below can fail without
            // leaving the driver parked forever. See `GateRelease`.
            let _release = GateRelease(&gate);

            let driver = {
                let txnmgr = Arc::clone(&txnmgr);
                s.spawn(move || drive(&txnmgr, xid))
            };
            assert!(
                eventually(2_000, || gate.entered()),
                "the transaction never reached its record/CLOG window"
            );
            // Pins that the gate really is inside the window. If it ever moved
            // after the CLOG stamp, this fails instead of the test passing
            // vacuously.
            assert!(
                fate_is_unstamped(&clog, xid),
                "the gate must park before the CLOG bit is set"
            );
            let record_end = wal.current_lsn();

            let checkpointer = {
                let wal = Arc::clone(&wal);
                let sampled = Arc::clone(&sampled);
                s.spawn(move || {
                    let lsn = wal.redo_point();
                    sampled.store(true, Ordering::SeqCst);
                    lsn
                })
            };
            assert!(
                !eventually(400, || sampled.load(Ordering::SeqCst)),
                "redo_point sampled inside the window: the published redo would \
                 sit above a record whose fate is not durable"
            );

            gate.release();
            driver
                .join()
                .map_err(|_| anyhow::anyhow!("transaction thread panicked"))?;
            let redo = checkpointer
                .join()
                .map_err(|_| anyhow::anyhow!("redo_point sampler panicked"))??;

            // Once the barrier lifts, the sample may cover the record — and by
            // then the bit deciding its fate is set, so the checkpoint about to
            // flush the commit log will carry it.
            assert!(fate_is_stamped(&clog, xid));
            assert!(
                redo >= record_end,
                "the sample should cover the released window ({redo} < {record_end})"
            );
            Ok(())
        })?;

        Ok(())
    }

    /// The reason the commit path takes a [`CheckpointDelay`]. A redo point
    /// sampled between a commit record and its CLOG bit would be published
    /// alongside a commit-log image that still reads `InProgress`; a bounded
    /// replay from there never sees the commit record, so an acknowledged
    /// transaction's rows are invisible forever.
    #[test]
    fn redo_point_cannot_sample_between_a_commit_record_and_its_clog_bit() -> anyhow::Result<()> {
        race_a_sample_against(
            GateOp::Commit,
            |txnmgr, xid| {
                txnmgr
                    .commit(xid)
                    .unwrap_or_else(|error| panic!("commit failed: {error}"));
            },
            |clog, xid| !clog.is_committed(xid),
            |clog, xid| clog.is_committed(xid),
        )
    }

    /// The same window on the abort path, with a quieter but still real symptom:
    /// an abort whose `Aborted` bit misses the flushed commit-log image, with its
    /// record below the published redo point, leaves the XID `InProgress`
    /// forever — and a row it deleted keeps an in-progress `xmax` that no later
    /// transaction can stamp, so it stays visible and can never be updated or
    /// deleted again.
    #[test]
    fn redo_point_cannot_sample_between_an_abort_record_and_its_clog_bit() -> anyhow::Result<()> {
        use crabgresql_txn::XactStatus;

        race_a_sample_against(
            GateOp::Abort,
            |txnmgr, xid| txnmgr.abort(xid),
            |clog, xid| clog.status(xid) != XactStatus::Aborted,
            |clog, xid| clog.status(xid) == XactStatus::Aborted,
        )
    }

    // --- The paged format ---

    /// The precondition for direct I/O, asserted rather than assumed. An
    /// unaligned or partial write has no observable symptom — it is merely slower
    /// and shuts the door on `O_DIRECT` — so nothing else would catch a
    /// regression here.
    #[test]
    fn every_wal_write_is_a_whole_page_at_a_page_aligned_offset() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Wal::open(dir.path())?;
        // Odd sizes, enough of them to cross a segment boundary.
        for i in 0..1_400u64 {
            let range = wal.append(RmgrId::HEAP, 0, Xid(3), &vec![i as u8; 12_000 + (i % 7) as usize]);
            if i % 97 == 0 {
                wal.flush(range.end)?;
            }
        }
        wal.flush(wal.current_lsn())?;
        let segs = wal
            .segments
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        assert!(segs.writes.len() > 1, "the test never crossed a segment");
        for write in &segs.writes {
            assert!(write.offset.is_multiple_of(XLOG_BLCKSZ), "{write:?}");
            assert!((write.len as u64).is_multiple_of(XLOG_BLCKSZ), "{write:?}");
            assert!(write.buf_aligned, "the write buffer was not 4K-aligned: {write:?}");
        }
        assert!(
            segs.writes.iter().any(|w| w.segno > 1),
            "the test never crossed a segment boundary"
        );

        Ok(())
    }

    /// The partly filled tail page is rewritten on every flush — deliberately,
    /// and as PostgreSQL does. What must not happen is that the rewrite loses the
    /// records already on that page.
    #[test]
    fn the_tail_page_is_rewritten_without_losing_its_earlier_records() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Wal::open(dir.path())?;
        for i in 0..10u8 {
            let range = wal.append(RmgrId::HEAP, 0, Xid(3), &[i; 4]);
            wal.flush(range.end)?;
        }
        assert_eq!(read_all(dir.path()).len(), 10);
        let segs = wal
            .segments
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        assert_eq!(segs.writes.len(), 10, "one write per flush");
        assert!(
            segs.writes.iter().all(|w| w.offset == segs.writes[0].offset),
            "all ten flushes rewrote the same page"
        );

        Ok(())
    }

    /// The direct pin that a file's length is no longer load-bearing: the segment
    /// is a full 16 MiB the whole time, and reopening still lands exactly after
    /// the last record.
    #[test]
    fn reopening_positions_after_the_last_record_without_a_file_length() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let end = {
            let wal = Wal::open(dir.path())?;
            let end = wal.append(RmgrId::HEAP, 0, Xid(3), b"one").end;
            wal.flush(end)?;
            end
        };
        let segment = crate::segment::segment_path(dir.path(), segno_of(end.0));
        assert_eq!(std::fs::metadata(&segment)?.len(), WAL_SEG_SIZE);
        assert_eq!(Wal::open(dir.path())?.current_lsn(), end);

        Ok(())
    }

    /// `reset_to` does not truncate anything, so the records above it are
    /// physically still there. What makes them unreachable is that the next flush
    /// zeroes the rest of their page and the reader never seeks past it.
    ///
    /// The second case is the sharp one: the new stream ends *exactly* on a page
    /// boundary, so the discarded records begin at a page whose address is
    /// genuinely correct for where it sits. `pageaddr` cannot reject that page —
    /// what saves it is that opening the next page stages its header, so the
    /// flush's round-up writes it whole and erases what was there.
    #[test]
    fn a_reset_and_reappend_does_not_resurrect_the_records_it_dropped() -> anyhow::Result<()> {
        for land_on_a_boundary in [false, true] {
            let dir = tempfile::tempdir()?;
            let rewind_to;
            {
                let wal = Wal::open(dir.path())?;
                rewind_to = wal.append(RmgrId::HEAP, 0, Xid(3), b"keep").end;
                // Well past a page boundary, so the discarded records live on
                // pages the new stream will not reach on its own.
                for i in 0..4u8 {
                    wal.append(RmgrId::HEAP, 0, Xid(9), &vec![i; 5_000]);
                }
                wal.flush(wal.current_lsn())?;
                assert_eq!(read_all(dir.path()).len(), 5);
            }
            {
                let wal = Wal::open(dir.path())?;
                wal.reset_to(rewind_to)?;
                let payload = if land_on_a_boundary {
                    // Size the record so the stream stops flush with the page edge.
                    let room = XLOG_BLCKSZ - page_offset(rewind_to.0);
                    vec![0xEE; (room as usize) - WalRecord::MIN_LEN]
                } else {
                    vec![0xEE; 8]
                };
                let end = wal.append(RmgrId::HEAP, 0, Xid(4), &payload).end;
                wal.flush(end)?;
                if land_on_a_boundary {
                    assert_eq!(page_offset(end.0), XLP_PAGE_HEADER_SIZE);
                }
                assert_eq!(Wal::open(dir.path())?.current_lsn(), end);
            }
            let xids: Vec<Xid> = read_all(dir.path()).iter().map(|(x, _)| *x).collect();
            assert_eq!(
                xids,
                vec![Xid(3), Xid(4)],
                "a discarded record came back (boundary case: {land_on_a_boundary})"
            );
        }

        Ok(())
    }

    /// The writer's end LSN, the reader's, and therefore what `RedoContext`
    /// carries must be the same number. The per-page LSN gate in every redo
    /// handler is stated in terms of it.
    #[test]
    fn the_writer_and_the_reader_agree_on_a_records_end_lsn() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Wal::open(dir.path())?;
        let mut ends = Vec::new();
        // Sizes chosen to land records inside a page, across a page, and across a
        // segment boundary.
        while wal.current_lsn().0 < WAL_SEG_SIZE * 2 + 100 {
            ends.push(wal.append(RmgrId::HEAP, 0, Xid(3), &vec![0x3C; 6_001]).end);
        }
        wal.flush(wal.current_lsn())?;
        assert!(ends.iter().any(|e| segno_of(e.0) == 2), "no segment crossing");

        let mut reader = crate::reader::WalReader::open(dir.path(), Lsn::INVALID)?;
        let mut buf = Vec::new();
        let mut seen = Vec::new();
        while let Some((_, end)) = reader.next_into(&mut buf)? {
            seen.push(end);
        }
        assert_eq!(seen, ends);

        Ok(())
    }

    #[test]
    fn a_record_spanning_a_segment_boundary_round_trips() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Wal::open(dir.path())?;
        while wal.current_lsn().0 + 4_000 < 2 * WAL_SEG_SIZE {
            wal.append(RmgrId::HEAP, 0, Xid(3), &[0xAA; 3_900]);
        }
        // Now within a page of the boundary; a big record must straddle it.
        let range = wal.append(RmgrId::HEAP, 0, Xid(7), &[0xBB; 40_000]);
        wal.flush(range.end)?;
        assert_eq!(segno_of(range.start.0), 1);
        assert_eq!(segno_of(range.end.0), 2);
        let records = read_all(dir.path());
        assert_eq!(
            records.last().map(|(x, p)| (*x, p.len())),
            Some((Xid(7), 40_000))
        );

        Ok(())
    }

    // --- Segment recycling ---

    /// Fill past `through` segments and flush.
    fn fill_segments(wal: &Wal, through: u64) -> Result<(), WalError> {
        while wal.current_lsn().0 < through * WAL_SEG_SIZE {
            wal.append(RmgrId::HEAP, 0, Xid(3), &[0x77; 7_000]);
        }
        wal.flush(wal.current_lsn())
    }

    /// The end-to-end shape of recycling: spent segments are renamed forward
    /// rather than rewritten, the log above the redo point still reads, and
    /// nothing from a segment's previous life comes back.
    #[test]
    fn recycling_reclaims_the_prefix_without_resurrecting_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Wal::open(dir.path())?;
        fill_segments(&wal, 3)?;
        let redo = wal.current_lsn();
        let above = wal.append(RmgrId::HEAP, 0, Xid(4), b"above the redo point");
        wal.flush(above.end)?;

        wal.recycle(redo)?;
        let (lowest, highest) = crate::segment::segment_bounds(dir.path())?
            .ok_or_else(|| anyhow::anyhow!("every segment was reclaimed"))?;
        assert_eq!(lowest, segno_of(redo.0), "the redo point's segment must stay");
        assert!(highest > segno_of(redo.0), "nothing was recycled forward");

        // The recycled segments are the *same files*, not fresh ones: that is the
        // whole point, and it is also what makes their stale contents a hazard.
        let recycled = crate::segment::segment_path(dir.path(), highest);
        assert_eq!(std::fs::metadata(&recycled)?.len(), WAL_SEG_SIZE);

        // Replay from the redo point still finds the record above it, and the
        // walk stops rather than running on into a recycled segment.
        let reopened = Wal::open(dir.path())?;
        assert_eq!(reopened.current_lsn(), above.end);

        Ok(())
    }

    /// A checkpoint that cannot bound replay publishes an invalid redo point,
    /// meaning "resume from the head of the stream". Every segment is then still
    /// needed, and reclaiming any of them would make the cluster unstartable.
    #[test]
    fn a_clamped_redo_point_reclaims_nothing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Wal::open(dir.path())?;
        fill_segments(&wal, 3)?;
        let before = crate::segment::segment_bounds(dir.path())?;
        wal.recycle(Lsn::INVALID)?;
        assert_eq!(crate::segment::segment_bounds(dir.path())?, before);

        Ok(())
    }

    /// A directory from before the paged log must refuse to start. It would
    /// otherwise read as an *empty* log — the old format's first four bytes are a
    /// record length, not a page magic — and be discarded without a word.
    #[test]
    fn a_pre_paged_data_directory_refuses_to_start() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(wal_dir(dir.path()))?;
        std::fs::write(legacy_wal_path(dir.path()), b"a flat write-ahead log")?;
        let Err(error) = Wal::open(dir.path()) else {
            anyhow::bail!("a pre-paged data directory must not open");
        };
        assert!(
            matches!(error, WalError::IncompatibleWalFormat { .. }),
            "unexpected error: {error}"
        );

        Ok(())
    }
}
