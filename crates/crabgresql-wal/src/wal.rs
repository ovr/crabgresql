//! The append/flush WAL stream with group-commit fsync.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

use crabgresql_txn::{CommitSink, Xid};

use crate::record::{Lsn, WalError, WalRecord};
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

/// The write-ahead log. Cheap [`Wal::append`] stages bytes in memory and returns
/// the record's end-LSN; [`Wal::flush`] makes everything up to a target LSN
/// durable with a single fsync shared by all concurrent committers (group
/// commit).
pub struct Wal {
    inner: Mutex<Inner>,
    /// Held only by the current flusher, so appends proceed during an fsync.
    file: Mutex<File>,
    /// Highest LSN known to be on stable storage (fsynced).
    flushed: AtomicU64,
    cond: Condvar,
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
        })
    }

    /// Truncate the stream back to `lsn`, discarding anything after it. Used
    /// after recovery to drop a torn tail so new records overwrite the garbage.
    pub fn reset_to(&self, lsn: Lsn) -> Result<(), WalError> {
        let mut inner = self.inner.lock().unwrap();
        let file = self.file.lock().unwrap();
        file.set_len(lsn.0)?;
        file.sync_data()?;
        inner.unwritten.clear();
        inner.insert_lsn = lsn.0;
        inner.written = lsn.0;
        self.flushed.store(lsn.0, Ordering::SeqCst);
        Ok(())
    }

    /// Stage one record for a resource manager, returning its end-LSN. No I/O and
    /// no fsync — the caller stamps the returned LSN on the page it changed and,
    /// at commit, calls [`Wal::flush`] to make it durable.
    pub fn append(&self, rmgr: RmgrId, info: u8, xid: Xid, payload: &[u8]) -> Lsn {
        let mut inner = self.inner.lock().unwrap();
        let start = Lsn(inner.insert_lsn);
        let rec = WalRecord { prev_lsn: start, xid, rmgr: rmgr.0, info, payload };
        let mut scratch = std::mem::take(&mut inner.unwritten);
        let n = rec.encode(start, &mut scratch);
        inner.unwritten = scratch;
        inner.insert_lsn += n as u64;
        Lsn(inner.insert_lsn)
    }

    /// Highest LSN staged (durable or not).
    pub fn current_lsn(&self) -> Lsn {
        Lsn(self.inner.lock().unwrap().insert_lsn)
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
        let mut inner = self.inner.lock().unwrap();
        loop {
            if self.flushed.load(Ordering::SeqCst) >= up_to.0 {
                return Ok(());
            }
            if inner.flushing {
                // Another thread is mid-fsync; wait and recheck.
                inner = self.cond.wait(inner).unwrap();
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
                let file = self.file.lock().unwrap();
                file.write_all_at(&bytes, start)?;
                file.sync_data()?;
                Ok(())
            })();

            inner = self.inner.lock().unwrap();
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
        let lsn = self.append(RmgrId::XACT, XACT_COMMIT, xid, &[]);
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
    fn append_returns_monotonic_end_lsns_and_flush_persists() {
        let dir = tempfile::tempdir().unwrap();
        let wal = Wal::open(dir.path()).unwrap();
        let a = wal.append(RmgrId::HEAP, 0, Xid(3), &[1, 2, 3]);
        let b = wal.append(RmgrId::HEAP, 0, Xid(3), &[4, 5]);
        assert!(b > a);
        assert_eq!(wal.current_lsn(), b);
        assert_eq!(wal.flushed_lsn(), Lsn::INVALID);
        wal.flush(b).unwrap();
        assert_eq!(wal.flushed_lsn(), b);
        // Reopen: the durable position is restored from the file length.
        drop(wal);
        let wal2 = Wal::open(dir.path()).unwrap();
        assert_eq!(wal2.current_lsn(), b);
        assert_eq!(wal2.flushed_lsn(), b);
    }

    #[test]
    fn group_commit_coalesces_concurrent_flushers() {
        let dir = tempfile::tempdir().unwrap();
        let wal = Arc::new(Wal::open(dir.path()).unwrap());
        std::thread::scope(|s| {
            let mut handles = Vec::new();
            for i in 0..8u64 {
                let wal = Arc::clone(&wal);
                handles.push(s.spawn(move || {
                    let lsn = wal.append(RmgrId::XACT, XACT_COMMIT, Xid(3 + i), &[i as u8; 16]);
                    wal.flush(lsn).unwrap();
                    lsn
                }));
            }
            for h in handles {
                let lsn = h.join().unwrap();
                assert!(wal.flushed_lsn() >= lsn, "flush must be durable on return");
            }
        });
    }

    /// Decode every record in the on-disk WAL, returning `(xid, payload)` pairs.
    fn read_all(dir: &Path) -> Vec<(Xid, Vec<u8>)> {
        let bytes = std::fs::read(wal_path(dir)).unwrap();
        let mut out = Vec::new();
        let mut pos = 0;
        while let Some((rec, len)) = WalRecord::decode(&bytes[pos..]) {
            out.push((rec.xid, rec.payload.to_vec()));
            pos += len;
        }
        out
    }

    #[test]
    fn appending_after_reopen_preserves_earlier_records() {
        // Regression: flush must write at the logical offset, not the reset OS
        // cursor, or the first append after a reopen clobbers the log head.
        let dir = tempfile::tempdir().unwrap();
        {
            let wal = Wal::open(dir.path()).unwrap();
            wal.append(RmgrId::HEAP, 0, Xid(3), b"first");
            let l = wal.append(RmgrId::HEAP, 0, Xid(4), b"second");
            wal.flush(l).unwrap();
        }
        {
            // Reopen (cursor at 0, written at end-of-file) and append more.
            let wal = Wal::open(dir.path()).unwrap();
            wal.append(RmgrId::HEAP, 0, Xid(5), b"third");
            let l = wal.append(RmgrId::HEAP, 0, Xid(6), b"fourth");
            wal.flush(l).unwrap();
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
    }

    #[test]
    fn reset_to_discards_a_torn_tail_before_appending() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let valid_end;
        {
            let wal = Wal::open(dir.path()).unwrap();
            wal.append(RmgrId::HEAP, 0, Xid(3), b"good");
            valid_end = wal.append(RmgrId::XACT, XACT_COMMIT, Xid(3), &[]);
            wal.flush(valid_end).unwrap();
        }
        // A crash leaves raw garbage on disk past the last valid record.
        {
            let mut f = OpenOptions::new().append(true).open(wal_path(dir.path())).unwrap();
            f.write_all(&[0xAB; 37]).unwrap();
        }
        {
            let wal = Wal::open(dir.path()).unwrap();
            // Recovery computes `valid_end`; clamp to it (truncating the garbage),
            // then continue appending cleanly.
            wal.reset_to(valid_end).unwrap();
            let l = wal.append(RmgrId::XACT, XACT_COMMIT, Xid(10), &[]);
            wal.flush(l).unwrap();
        }
        let recs = read_all(dir.path());
        let xids: Vec<Xid> = recs.iter().map(|(x, _)| *x).collect();
        assert_eq!(xids, vec![Xid(3), Xid(3), Xid(10)], "torn tail dropped, new record appended cleanly");
    }
}
