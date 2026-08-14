//! The append/flush WAL stream with group-commit fsync.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

use crabgresql_txn::{CommitSink, Xid};

use crate::record::{Lsn, LsnRange, WalError, WalRecord};
use crate::rmgr::{RmgrId, XACT_ABORT, XACT_COMMIT, XLOG_PAD};
use crate::segment::{SEGMENT_SIZE, SegmentWriter, segment_numbers, segment_offset};

struct Inner {
    /// Bytes appended but not yet handed to a writer.
    unwritten: Vec<u8>,
    /// End-LSN of everything appended so far (monotonic; == bytes accounted).
    insert_lsn: u64,
    /// Bytes already written to the file (may not yet be fsynced).
    written: u64,
    /// True while one thread owns the write+fsync; others coalesce behind it.
    flushing: bool,
}

impl Inner {
    /// Carry the insert position to the next segment boundary if a record of
    /// `len` bytes would otherwise straddle it. No-op in the overwhelmingly
    /// common case where the record fits.
    ///
    /// Two fillers, because one cannot cover both cases:
    ///
    /// * a tail of at least [`WalRecord::MIN_LEN`] bytes gets a **padding
    ///   record** — a real, CRC-covered record whose payload is zeros. Recovery
    ///   decodes and ignores it, so the scan needs no rule about where segments
    ///   end and no way to confuse padding with a torn write;
    /// * a shorter tail gets raw zeros, because no record fits in it. That is
    ///   unambiguous for a different reason: a record can never *start* there,
    ///   so recovery can skip such a tail on arithmetic alone.
    ///
    /// A record longer than a whole segment is exempt (see [`Wal::append`]): at
    /// offset 0 there is nothing to pad to, and padding a whole empty segment
    /// would not help it fit.
    fn pad_to_boundary(&mut self, len: u64) {
        let off = segment_offset(Lsn(self.insert_lsn));
        if off == 0 {
            return;
        }
        let remaining = SEGMENT_SIZE - off;
        if remaining < WalRecord::MIN_LEN as u64 {
            self.unwritten
                .resize(self.unwritten.len() + remaining as usize, 0);
            self.insert_lsn += remaining;
            return;
        }
        if len <= remaining {
            return;
        }
        let filler = vec![0u8; (remaining - WalRecord::MIN_LEN as u64) as usize];
        let pad = WalRecord {
            rec_lsn: Lsn(self.insert_lsn),
            xid: Xid::INVALID,
            rmgr: RmgrId::XLOG.0,
            info: XLOG_PAD,
            payload: &filler,
        };
        let mut scratch = std::mem::take(&mut self.unwritten);
        let n = pad.encode(&mut scratch);
        self.unwritten = scratch;
        debug_assert_eq!(
            n as u64, remaining,
            "padding must land exactly on the boundary"
        );
        self.insert_lsn += n as u64;
    }
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
    inner: Mutex<Inner>,
    /// Held only by the current flusher, so appends proceed during an fsync.
    writer: Mutex<SegmentWriter>,
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
/// writers that genuinely cannot do that. There are four:
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
    /// Open (creating if absent) the WAL under `dir`, positioned to append after
    /// the existing segments. Everything already on disk is treated as durable.
    /// After a crash, call [`crate::recover`] first and then [`Wal::reset_to`]
    /// to discard any torn tail past the last valid record.
    ///
    /// The insert position comes from the *highest-numbered* segment, not from
    /// summing them: a segment is only created once the one before it is full
    /// and fsynced, so the last one is the only partial file and its length is
    /// where the stream ends.
    pub fn open(dir: &Path) -> Result<Wal, WalError> {
        let last = segment_numbers(dir)?.pop().unwrap_or(0);
        let writer = SegmentWriter::open(dir, last)?;
        let len = last * SEGMENT_SIZE + writer.len()?;
        tracing::trace!(dir = %dir.display(), segment = last, len, "opened WAL");
        Ok(Wal {
            inner: Mutex::new(Inner {
                unwritten: Vec::new(),
                insert_lsn: len,
                written: len,
                flushing: false,
            }),
            writer: Mutex::new(writer),
            flushed: AtomicU64::new(len),
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

    /// Truncate the stream back to `lsn`, discarding anything after it —
    /// including whole segments above it. Used after recovery to drop a torn
    /// tail so new records overwrite the garbage.
    pub fn reset_to(&self, lsn: Lsn) -> Result<(), WalError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        writer.reset_to(lsn)?;
        inner.unwritten.clear();
        inner.insert_lsn = lsn.0;
        inner.written = lsn.0;
        self.flushed.store(lsn.0, Ordering::SeqCst);
        tracing::trace!(lsn = %lsn, "reset WAL to LSN, discarding torn tail");
        Ok(())
    }

    /// Stage one record for a resource manager, returning the byte range it
    /// occupies. No I/O and no fsync — the caller stamps the range's `end` on the
    /// page it changed and, at commit, calls [`Wal::flush`] to make it durable.
    /// The `start` is the record's own boundary, which is what a redo point has
    /// to name.
    ///
    /// A record never straddles a segment boundary: when one would, the tail of
    /// the current segment is filled first (see [`Inner::pad_to_boundary`]) and
    /// the record starts the next segment. The single exception is a record
    /// larger than a whole segment, which cannot be placed any other way; it
    /// begins at a segment boundary and spans as many files as it needs. Keeping
    /// that case working is why `append` stays infallible — the alternative is
    /// turning one oversized row into a startup-time error at every call site.
    pub fn append(&self, rmgr: RmgrId, info: u8, xid: Xid, payload: &[u8]) -> LsnRange {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        inner.pad_to_boundary((WalRecord::HEADER_LEN + payload.len() + 4) as u64);
        let start = Lsn(inner.insert_lsn);
        let rec = WalRecord {
            rec_lsn: start,
            xid,
            rmgr: rmgr.0,
            info,
            payload,
        };
        let mut scratch = std::mem::take(&mut inner.unwritten);
        let n = rec.encode(&mut scratch);
        inner.unwritten = scratch;
        inner.insert_lsn += n as u64;
        let range = LsnRange {
            start,
            end: Lsn(inner.insert_lsn),
        };
        tracing::trace!(
            rmgr = rmgr.0,
            info,
            xid = xid.0,
            start = %range.start,
            end = %range.end,
            "appended WAL record"
        );
        range
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
        tracing::trace!(up_to = %up_to, "flush requested");
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
                appended: inner.insert_lsn,
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
            // Become the flusher for everything appended so far.
            inner.flushing = true;
            let bytes = std::mem::take(&mut inner.unwritten);
            let start = inner.written;
            let target = start + bytes.len() as u64;
            drop(inner);

            // The writer splits `bytes` across whatever segments they span and
            // fsyncs each file it touches, reporting how much of the buffer is
            // durable — which on the error path is not necessarily nothing.
            let mut durable = 0usize;
            // A block, so the writer lock is released before `inner` is taken
            // again below: appends must keep running during the fsync, which is
            // the whole point of holding the two separately.
            let write_result = {
                let mut writer = self
                    .writer
                    .lock()
                    .unwrap_or_else(|_| panic!("mutex poisoned"));
                writer.write_at(&bytes, start, &mut durable)
            };

            inner = self
                .inner
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"));
            match write_result {
                Ok(()) => {
                    inner.written = target;
                    self.flushed.store(target, Ordering::SeqCst);
                    inner.flushing = false;
                    self.cond.notify_all();
                    tracing::trace!(start, target, "flushed WAL bytes to disk");
                }
                Err(e) => {
                    // Keep the prefix that reached stable storage and put only
                    // the rest back, so a retry re-writes exactly what is
                    // missing. Claiming the whole drain failed would be a lie in
                    // the other direction now: a multi-segment write is several
                    // fsyncs, and the earlier ones already succeeded.
                    inner.written = start + durable as u64;
                    self.flushed.store(inner.written, Ordering::SeqCst);
                    let mut returned = bytes;
                    returned.drain(..durable);
                    returned.extend_from_slice(&inner.unwritten);
                    inner.unwritten = returned;
                    inner.flushing = false;
                    self.cond.notify_all();
                    return Err(e);
                }
            }
            // target >= up_to because up_to was appended before this call, so the
            // drain captured it; the top-of-loop check returns Ok.
        }
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
    use std::fs::OpenOptions;
    use std::sync::Arc;

    use crate::segment::{wal_segment_path, wal_segment_path_0, wal_stream_len};

    #[test]
    fn append_returns_monotonic_end_lsns_and_flush_persists() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Wal::open(dir.path())?;
        let a = wal.append(RmgrId::HEAP, 0, Xid(3), &[1, 2, 3]).end;
        let b = wal.append(RmgrId::HEAP, 0, Xid(3), &[4, 5]).end;
        assert!(b > a);
        assert_eq!(wal.current_lsn(), b);
        assert_eq!(wal.flushed_lsn(), Lsn::INVALID);
        wal.flush(b)?;
        assert_eq!(wal.flushed_lsn(), b);
        // Reopen: the durable position is restored from the file length.
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

    /// Append one record that ends exactly `remaining` bytes short of the first
    /// segment boundary, and flush it.
    ///
    /// The boundary tests below all need the insert position parked at a precise
    /// distance from the boundary; doing it with one big record rather than a
    /// loop of small ones keeps a 32 MB test to a single append and a single
    /// fsync. Only valid as the first append into a fresh WAL.
    fn fill_to_within(wal: &Wal, remaining: u64) -> anyhow::Result<LsnRange> {
        let payload = vec![0x5A; (SEGMENT_SIZE - remaining) as usize - WalRecord::MIN_LEN];
        let range = wal.append(RmgrId::HEAP, 7, Xid(3), &payload);
        assert_eq!(
            range.end,
            Lsn(SEGMENT_SIZE - remaining),
            "the filler must land where the test expects it"
        );
        wal.flush(range.end)?;

        Ok(range)
    }

    /// A record that does not fit in what is left of a segment must not straddle
    /// the boundary: the tail is filled with a padding record and the record
    /// itself starts the next segment.
    #[test]
    fn a_record_that_does_not_fit_is_padded_to_the_next_segment() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Wal::open(dir.path())?;
        // 100 bytes left: room for a padding record (>= MIN_LEN), not for the
        // 228-byte record that follows.
        fill_to_within(&wal, 100)?;
        let range = wal.append(RmgrId::HEAP, 7, Xid(4), &[0xC3; 200]);
        assert_eq!(
            range.start,
            Lsn(SEGMENT_SIZE),
            "the record must start the next segment, not straddle the boundary"
        );
        wal.flush(range.end)?;

        assert_eq!(
            std::fs::metadata(wal_segment_path_0(dir.path()))?.len(),
            SEGMENT_SIZE,
            "the segment left behind must be full to the byte"
        );
        assert_eq!(
            std::fs::metadata(wal_segment_path(dir.path(), 1))?.len(),
            range.end.0 - SEGMENT_SIZE
        );
        assert_eq!(wal_stream_len(dir.path())?, range.end.0);

        // The filler is a real record: it decodes, and it says what it is.
        let seg0 = std::fs::read(wal_segment_path_0(dir.path()))?;
        let (pad, len) = WalRecord::decode(&seg0[(SEGMENT_SIZE - 100) as usize..])
            .ok_or_else(|| anyhow::anyhow!("the padding did not decode as a record"))?;
        assert_eq!(len, 100, "padding must reach the boundary exactly");
        assert_eq!(pad.rmgr, RmgrId::XLOG.0);
        assert_eq!(pad.info, XLOG_PAD);
        assert_eq!(pad.rec_lsn, Lsn(SEGMENT_SIZE - 100));

        Ok(())
    }

    /// A tail too short for even an empty record cannot be padded with one. It is
    /// zero-filled instead, and recovery skips it on arithmetic alone — which is
    /// the only case where the reader needs to know segments exist at all.
    #[test]
    fn a_tail_too_small_for_a_record_is_zero_filled() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Wal::open(dir.path())?;
        fill_to_within(&wal, WalRecord::MIN_LEN as u64 - 1)?;
        let range = wal.append(RmgrId::HEAP, 7, Xid(4), b"x");
        assert_eq!(range.start, Lsn(SEGMENT_SIZE));
        wal.flush(range.end)?;

        let seg0 = std::fs::read(wal_segment_path_0(dir.path()))?;
        assert_eq!(seg0.len() as u64, SEGMENT_SIZE);
        assert!(
            seg0[SEGMENT_SIZE as usize - (WalRecord::MIN_LEN - 1)..]
                .iter()
                .all(|b| *b == 0),
            "an unusable tail must be zeros, not stale bytes"
        );

        Ok(())
    }

    /// Crossing a boundary and reopening must resume after the *last* segment,
    /// not after the first: the insert position comes from the highest-numbered
    /// segment plus its length, never from segment zero.
    #[test]
    fn reopening_positions_after_the_last_segment() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let end;
        {
            let wal = Wal::open(dir.path())?;
            fill_to_within(&wal, 100)?;
            end = wal.append(RmgrId::HEAP, 7, Xid(4), &[0xC3; 200]).end;
            wal.flush(end)?;
        }
        let wal = Wal::open(dir.path())?;
        assert_eq!(wal.current_lsn(), end);
        assert_eq!(wal.flushed_lsn(), end);
        // And the next append continues the second segment rather than
        // overwriting it.
        let range = wal.append(RmgrId::HEAP, 7, Xid(5), b"after-reopen");
        assert_eq!(range.start, end);
        wal.flush(range.end)?;
        assert_eq!(wal_stream_len(dir.path())?, range.end.0);

        Ok(())
    }

    /// `reset_to` below a boundary must unlink the segments above it. A leftover
    /// segment would put the next `open` back above the truncation point, with an
    /// unwritten hole in between that recovery would then read as the log.
    #[test]
    fn reset_to_below_a_boundary_deletes_the_segments_above() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Wal::open(dir.path())?;
        let filler = fill_to_within(&wal, 100)?;
        let above = wal.append(RmgrId::HEAP, 7, Xid(4), &[0xC3; 200]);
        wal.flush(above.end)?;
        assert_eq!(crate::segment::segment_numbers(dir.path())?, vec![0, 1]);

        wal.reset_to(filler.end)?;
        assert_eq!(
            crate::segment::segment_numbers(dir.path())?,
            vec![0],
            "the segment above the truncation point must be gone"
        );
        assert_eq!(wal_stream_len(dir.path())?, filler.end.0);
        assert_eq!(wal.current_lsn(), filler.end);

        // Appending again re-pads and re-creates the second segment cleanly.
        let again = wal.append(RmgrId::HEAP, 7, Xid(5), &[0xC3; 200]);
        assert_eq!(again.start, Lsn(SEGMENT_SIZE));
        wal.flush(again.end)?;
        assert_eq!(wal_stream_len(dir.path())?, again.end.0);

        Ok(())
    }

    /// Decode every record in the on-disk WAL, returning `(xid, payload)` pairs.
    fn read_all(dir: &Path) -> Vec<(Xid, Vec<u8>)> {
        let bytes = match std::fs::read(wal_segment_path_0(dir)) {
            Ok(bytes) => bytes,
            Err(error) => panic!("failed to read WAL test fixture: {error}"),
        };
        let mut out = Vec::new();
        let mut pos = 0;
        while let Some((rec, len)) = WalRecord::decode(&bytes[pos..]) {
            out.push((rec.xid, rec.payload.to_vec()));
            pos += len;
        }
        out
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
        use std::io::Write;
        let dir = tempfile::tempdir()?;
        let valid_end;
        {
            let wal = Wal::open(dir.path())?;
            wal.append(RmgrId::HEAP, 0, Xid(3), b"good");
            valid_end = wal.append(RmgrId::XACT, XACT_COMMIT, Xid(3), &[]).end;
            wal.flush(valid_end)?;
        }
        // A crash leaves raw garbage on disk past the last valid record.
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(wal_segment_path_0(dir.path()))?;
            f.write_all(&[0xAB; 37])?;
        }
        {
            let wal = Wal::open(dir.path())?;
            // Recovery computes `valid_end`; clamp to it (truncating the garbage),
            // then continue appending cleanly.
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
        let mut previous_end = Lsn(0);
        for i in 0..4usize {
            let payload = vec![i as u8; i * 11];
            let range = wal.append(RmgrId::HEAP, 0, Xid(3), &payload);
            assert_eq!(range.start, previous_end, "ranges must be contiguous");
            assert_eq!(
                range.end.0 - range.start.0,
                (WalRecord::HEADER_LEN + payload.len() + 4) as u64,
                "range width must be the encoded record length"
            );
            previous_end = range.end;
        }
        assert_eq!(wal.current_lsn(), previous_end);

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
        wal.reset_to(Lsn::INVALID)?;

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
            err.to_string().contains("only 0 bytes have been appended"),
            "unexpected error: {err}"
        );

        Ok(())
    }

    #[test]
    fn redo_point_is_the_insert_lsn_when_nothing_is_delayed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Wal::open(dir.path())?;
        assert_eq!(wal.redo_point()?, Lsn::INVALID);
        let range = wal.append(RmgrId::HEAP, 0, Xid(3), b"a");
        assert_eq!(wal.redo_point()?, range.end);
        // Repeatable: sampling does not consume anything.
        assert_eq!(wal.redo_point()?, wal.current_lsn());

        Ok(())
    }

    /// The redo point must never name a byte past the end of the on-disk log:
    /// recovery hard-errors on that, so publishing one would leave a cluster
    /// that refuses to start.
    #[test]
    fn redo_point_is_durable_on_return() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Wal::open(dir.path())?;
        // Staged but never explicitly flushed.
        let range = wal.append(RmgrId::HEAP, 0, Xid(3), b"staged-only");
        assert_eq!(wal.flushed_lsn(), Lsn::INVALID, "nothing flushed yet");

        let redo = wal.redo_point()?;
        assert_eq!(redo, range.end);
        assert!(
            wal.flushed_lsn() >= redo,
            "redo_point must flush through the LSN it returns"
        );
        let on_disk = std::fs::metadata(wal_segment_path_0(dir.path()))?.len();
        assert!(
            on_disk >= redo.0,
            "the file must be at least as long as the redo point ({on_disk} < {redo})"
        );

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
                .map_err(|_| anyhow::anyhow!("redo_point sampler panicked"))?;
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
        assert_eq!(lsn, Lsn::INVALID);

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
}
