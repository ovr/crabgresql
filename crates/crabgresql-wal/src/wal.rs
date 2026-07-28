//! The append/flush WAL stream with group-commit fsync.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

use crabgresql_txn::{CommitSink, Xid};

use crate::record::{Lsn, LsnRange, WalError, WalRecord};
use crate::rmgr::{RmgrId, XACT_ABORT, XACT_COMMIT};

/// The single WAL file lives at `<dir>/pg_wal/wal`. Segment rotation is a
/// follow-up (`docs/ARCHITECTURE.md §3`); a single growing file is enough for a
/// correct first cut and keeps LSN==byte-offset trivially true.
const WAL_SUBDIR: &str = "pg_wal";
const WAL_FILE: &str = "wal";

pub fn wal_path(dir: &Path) -> PathBuf {
    dir.join(WAL_SUBDIR).join(WAL_FILE)
}

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

/// Checkpoint-delay bookkeeping. Guarded by its own mutex, independent of
/// [`Inner`]: a writer holding a delay goes on to `append`, so the lock order is
/// always `delay` → `inner` and never the reverse.
struct Delay {
    /// Writers currently inside a "record appended, effect not yet published"
    /// window.
    active: u64,
    /// A checkpointer is waiting to sample the redo point. New delays queue
    /// behind it, so a steady stream of writers cannot starve it.
    wanted: bool,
}

/// The write-ahead log. Cheap [`Wal::append`] stages bytes in memory and returns
/// the record's byte range; [`Wal::flush`] makes everything up to a target LSN
/// durable with a single fsync shared by all concurrent committers (group
/// commit).
pub struct Wal {
    inner: Mutex<Inner>,
    /// Held only by the current flusher, so appends proceed during an fsync.
    file: Mutex<File>,
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
/// writers that genuinely cannot do that — currently only a B-tree split, which
/// is one record over three separately locked pages. A transaction commit and a
/// buffer-table install will join it as the durable-CLOG and checkpoint work
/// lands.
///
/// The guard is **non-reentrant**: a thread holding one must not call
/// [`Wal::redo_point`], which would wait for itself. A checkpointer must also
/// let `redo_point` return *before* flushing buffers, so it never holds this
/// barrier while taking buffer-pool frame mutexes.
pub struct CheckpointDelay<'a> {
    wal: &'a Wal,
}

impl Drop for CheckpointDelay<'_> {
    fn drop(&mut self) {
        let mut delay = self
            .wal
            .delay
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        delay.active -= 1;
        if delay.active == 0 {
            self.wal.delay_cond.notify_all();
        }
    }
}

impl Wal {
    /// Open (creating if absent) the WAL under `dir`, positioned to append after
    /// the existing file contents. Everything already in the file is treated as
    /// durable. After a crash, call [`crate::recover`] first and then
    /// [`Wal::reset_to`] to discard any torn tail past the last valid record.
    pub fn open(dir: &Path) -> Result<Wal, WalError> {
        std::fs::create_dir_all(dir.join(WAL_SUBDIR))?;
        let path = wal_path(dir);
        // truncate(false): never discard an existing WAL — we append to it.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let len = file.metadata()?.len();
        Ok(Wal {
            inner: Mutex::new(Inner {
                unwritten: Vec::new(),
                insert_lsn: len,
                written: len,
                flushing: false,
            }),
            file: Mutex::new(file),
            flushed: AtomicU64::new(len),
            cond: Condvar::new(),
            delay: Mutex::new(Delay {
                active: 0,
                wanted: false,
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
        let mut delay = self
            .delay
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        while delay.wanted {
            delay = match self.delay_cond.wait(delay) {
                Ok(delay) => delay,
                Err(_) => panic!("WAL checkpoint-delay condition-variable mutex poisoned"),
            };
        }
        delay.active += 1;
        CheckpointDelay { wal: self }
    }

    /// The redo point — a record boundary at or above which replay must resume.
    /// Blocks until every outstanding [`CheckpointDelay`] is released, so on
    /// return every record below the result has its effect published.
    ///
    /// A checkpoint must sample this **before** flushing buffers, never after:
    /// sampling afterwards would let a page dirtied during the flush pass carry
    /// an LSN below the redo point, leaving it neither written back nor
    /// replayed.
    pub fn redo_point(&self) -> Lsn {
        let mut delay = self
            .delay
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        delay.wanted = true;
        while delay.active > 0 {
            delay = match self.delay_cond.wait(delay) {
                Ok(delay) => delay,
                Err(_) => panic!("WAL checkpoint-delay condition-variable mutex poisoned"),
            };
        }
        // Lock order delay -> inner; nothing takes them the other way round.
        let lsn = self.current_lsn();
        delay.wanted = false;
        self.delay_cond.notify_all();
        lsn
    }

    /// Truncate the stream back to `lsn`, discarding anything after it. Used
    /// after recovery to drop a torn tail so new records overwrite the garbage.
    pub fn reset_to(&self, lsn: Lsn) -> Result<(), WalError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let file = self
            .file
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        file.set_len(lsn.0)?;
        file.sync_data()?;
        inner.unwritten.clear();
        inner.insert_lsn = lsn.0;
        inner.written = lsn.0;
        self.flushed.store(lsn.0, Ordering::SeqCst);
        Ok(())
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

            // Positioned write at the logical offset `start`, never the OS file
            // cursor: after a reopen the cursor is 0 but the log continues at
            // `written`, and on a partial-write retry the cursor is desynced —
            // both would otherwise corrupt the stream.
            let write_result = (|| -> Result<(), WalError> {
                let file = self
                    .file
                    .lock()
                    .unwrap_or_else(|_| panic!("mutex poisoned"));
                file.write_all_at(&bytes, start)?;
                file.sync_data()?;
                Ok(())
            })();

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
                }
                Err(e) => {
                    // Put the drained bytes back so a retry can flush them, and
                    // wake any waiters to observe the failure on their own retry.
                    let mut returned = bytes;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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
                    let lsn = wal.append(RmgrId::XACT, XACT_COMMIT, Xid(3 + i), &[i as u8; 16]).end;
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
        let bytes = match std::fs::read(wal_path(dir)) {
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
            let mut f = OpenOptions::new().append(true).open(wal_path(dir.path()))?;
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

    #[test]
    fn redo_point_is_the_insert_lsn_when_nothing_is_delayed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Wal::open(dir.path())?;
        assert_eq!(wal.redo_point(), Lsn::INVALID);
        let range = wal.append(RmgrId::HEAP, 0, Xid(3), b"a");
        assert_eq!(wal.redo_point(), range.end);
        // Repeatable: sampling does not consume anything.
        assert_eq!(wal.redo_point(), wal.current_lsn());

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
                .map_err(|_| anyhow::anyhow!("redo_point sampler panicked"))?;
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
}
