//! Redo-only crash recovery.
//!
//! There is no undo pass: uncommitted heap versions are simply invisible under
//! MVCC and are reclaimed later by VACUUM, so recovery only needs to *reapply*
//! logged changes (ARIES redo) and rebuild the transaction state.
//!
//! [`recover`] takes the LSN to resume from. Because an [`Lsn`] is literally a
//! byte offset into the single stream file, that is a positioned read rather
//! than a scan. Every production caller still passes [`Lsn::INVALID`] (`0`, the
//! start of the stream): honouring a real redo point additionally needs a
//! durable CLOG, to recover the fate of transactions that committed *before* the
//! checkpoint, and a checkpoint record to read the redo point from — both
//! deliberate follow-ups, see `docs/ARCHITECTURE.md §3`. The parameter exists
//! now so that writers whose correctness depends on a bounded replay can be
//! tested against one.

use std::os::unix::fs::FileExt;
use std::path::Path;

use crabgresql_txn::{Clog, Xid};

use crate::control::read_control;
use crate::record::{Lsn, WalError, WalRecord};
use crate::rmgr::{RedoContext, RmgrId, RmgrRegistry, XACT_ABORT, XACT_COMMIT};
use crate::wal::wal_path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryResult {
    /// LSN just past the last valid record — where fresh appends must resume.
    pub end_of_wal: Lsn,
    /// The XID the allocator must start from: above every transaction recovered.
    pub next_xid: Xid,
    /// The LSN replay actually resumed from — echoed back so a caller can assert
    /// that the log below it was never read.
    pub replayed_from: Lsn,
}

/// Read `[start, end-of-file)` of the WAL under `dir`. A positioned read, not a
/// whole-file slurp: buffering the prefix would defeat the point of bounding
/// replay at all.
fn read_from(dir: &Path, start: Lsn) -> Result<Vec<u8>, WalError> {
    let path = wal_path(dir);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && start == Lsn::INVALID => {
            return Ok(Vec::new());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(WalError::Redo(format!(
                "recovery must resume at {start} but {} does not exist",
                path.display()
            )));
        }
        Err(e) => return Err(e.into()),
    };
    let len = file.metadata()?.len();
    if start.0 > len {
        return Err(WalError::Redo(format!(
            "recovery must resume at {start} (byte {}) but {} is only {len} bytes",
            start.0,
            path.display()
        )));
    }
    let mut bytes = vec![0u8; (len - start.0) as usize];
    // A short read is possible on a large file; loop until the buffer is full or
    // the file ends (a concurrent truncation would be the only way to see EOF).
    let mut filled = 0usize;
    while filled < bytes.len() {
        match file.read_at(&mut bytes[filled..], start.0 + filled as u64)? {
            0 => {
                bytes.truncate(filled);
                break;
            }
            n => filled += n,
        }
    }
    Ok(bytes)
}

/// Replay the WAL under `dir` from `start`, rebuilding `clog` from commit/abort
/// records and dispatching every other record to its registered redo handler.
/// Returns the end of the valid log and the next XID to hand out.
///
/// `start` must name a record boundary; pass [`Lsn::INVALID`] to replay the whole
/// stream.
pub fn recover(
    dir: &Path,
    registry: &RmgrRegistry,
    clog: &Clog,
    start: Lsn,
) -> Result<RecoveryResult, WalError> {
    let control = read_control(dir)?;
    let mut next_xid = control.map(|c| c.next_xid.0).unwrap_or(Xid::FIRST_NORMAL.0);

    let bytes = read_from(dir, start)?;

    let mut pos = 0usize;
    let mut first = true;
    while let Some((rec, len)) = WalRecord::decode(&bytes[pos..]) {
        // The first record must begin exactly where we were told to resume. A
        // redo point landing mid-record would otherwise be decoded as garbage
        // and silently read as a clean end-of-log. (Only detectable when the
        // bytes happen to decode — a mid-record offset usually fails CRC, which
        // is indistinguishable from a torn tail, so this is a boundary
        // assertion, not a search.)
        if first {
            first = false;
            if rec.rec_lsn != start {
                return Err(WalError::Redo(format!(
                    "recovery resumed at {start} but the record there claims to start \
                     at {} — the redo point is not a record boundary",
                    rec.rec_lsn
                )));
            }
        }
        let end_lsn = Lsn(start.0 + (pos + len) as u64);
        // The allocator must sit above any XID that ever appeared in the log.
        if rec.xid.0 >= next_xid {
            next_xid = rec.xid.0 + 1;
        }
        match RmgrId(rec.rmgr) {
            RmgrId::XACT => match rec.info {
                XACT_COMMIT => clog.set_committed(rec.xid),
                XACT_ABORT => clog.set_aborted(rec.xid),
                other => {
                    return Err(WalError::Redo(format!("unknown xact info byte {other:#x}")));
                }
            },
            RmgrId::CHECKPOINT => { /* metadata only; nothing to redo into a page */ }
            other => {
                let handler = registry
                    .get(other.0)
                    .ok_or(WalError::UnknownRmgr(other.0))?;
                handler.redo(&RedoContext {
                    lsn: end_lsn,
                    xid: rec.xid,
                    info: rec.info,
                    payload: rec.payload,
                })?;
            }
        }
        pos += len;
    }

    Ok(RecoveryResult {
        end_of_wal: Lsn(start.0 + pos as u64),
        next_xid: Xid(next_xid),
        replayed_from: start,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::Wal;
    use std::sync::{Arc, Mutex};

    use crate::rmgr::RmgrRedo;

    /// A redo handler that just records the payloads it was asked to reapply.
    struct Collector(Arc<Mutex<Vec<Vec<u8>>>>);
    impl RmgrRedo for Collector {
        fn redo(&self, ctx: &RedoContext) -> Result<(), WalError> {
            self.0
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"))
                .push(ctx.payload.to_vec());
            Ok(())
        }
    }

    #[test]
    fn replays_committed_records_and_recovers_next_xid() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        {
            let wal = Wal::open(dir.path())?;
            wal.append(RmgrId::HEAP, 7, Xid(3), b"insert-a");
            let c = wal.append(RmgrId::XACT, XACT_COMMIT, Xid(3), &[]).end;
            wal.append(RmgrId::HEAP, 7, Xid(4), b"insert-b"); // never committed
            wal.flush(c)?;
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut reg = RmgrRegistry::new();
        reg.register(RmgrId::HEAP, Arc::new(Collector(Arc::clone(&seen))));
        let clog = Clog::new();
        let res = recover(dir.path(), &reg, &clog, Lsn::INVALID)?;

        assert!(clog.is_committed(Xid(3)));
        assert!(!clog.is_committed(Xid(4)));
        assert_eq!(res.next_xid, Xid(5)); // above the highest XID seen (4)
        // Both heap records are redone (redo is oblivious to commit; MVCC hides
        // the uncommitted one at read time).
        assert_eq!(
            seen.lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"))
                .len(),
            2
        );

        Ok(())
    }

    #[test]
    fn stops_at_a_torn_tail() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let good_end;
        {
            let wal = Wal::open(dir.path())?;
            wal.append(RmgrId::HEAP, 0, Xid(3), b"ok");
            good_end = wal.append(RmgrId::XACT, XACT_COMMIT, Xid(3), &[]).end;
            wal.flush(good_end)?;
        }
        // Append raw garbage past the valid records (a torn write).
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(wal_path(dir.path()))?;
            f.write_all(&[0xAB; 40])?;
        }
        let mut reg = RmgrRegistry::new();
        reg.register(
            RmgrId::HEAP,
            Arc::new(Collector(Arc::new(Mutex::new(Vec::new())))),
        );
        let clog = Clog::new();
        let res = recover(dir.path(), &reg, &clog, Lsn::INVALID)?;
        assert_eq!(
            res.end_of_wal, good_end,
            "recovery ends at the last valid record"
        );

        Ok(())
    }

    #[test]
    fn replay_starts_at_the_given_lsn() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let second;
        {
            let wal = Wal::open(dir.path())?;
            wal.append(RmgrId::HEAP, 7, Xid(3), b"below-redo");
            second = wal.append(RmgrId::HEAP, 7, Xid(4), b"above-redo");
            wal.flush(second.end)?;
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut reg = RmgrRegistry::new();
        reg.register(RmgrId::HEAP, Arc::new(Collector(Arc::clone(&seen))));
        let clog = Clog::new();
        let res = recover(dir.path(), &reg, &clog, second.start)?;

        assert_eq!(res.replayed_from, second.start);
        assert_eq!(res.end_of_wal, second.end);
        let seen = seen
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .clone();
        assert_eq!(
            seen,
            vec![b"above-redo".to_vec()],
            "only records at or above the start LSN are replayed"
        );

        Ok(())
    }

    #[test]
    fn a_start_lsn_past_the_end_of_the_log_is_an_error() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let end;
        {
            let wal = Wal::open(dir.path())?;
            end = wal.append(RmgrId::HEAP, 0, Xid(3), b"only").end;
            wal.flush(end)?;
        }
        let reg = RmgrRegistry::new();
        let clog = Clog::new();
        // Must be a clean error, not a panic on an out-of-range slice.
        let Err(err) = recover(dir.path(), &reg, &clog, Lsn(end.0 + 1)) else {
            anyhow::bail!("recovery past the end of the log should fail");
        };
        assert!(
            err.to_string().contains("bytes"),
            "error should name the file length: {err}"
        );

        Ok(())
    }

    #[test]
    fn a_first_record_whose_rec_lsn_disagrees_with_the_start_is_rejected() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        drop(Wal::open(dir.path())?); // creates <dir>/pg_wal/
        // Hand-encode a CRC-valid record that lies about where it starts. Seeking
        // to a genuine mid-record offset would usually just fail CRC and read as
        // a clean end-of-log, so the boundary check is pinned directly.
        let mut bytes = Vec::new();
        WalRecord {
            rec_lsn: Lsn(4096),
            xid: Xid(3),
            rmgr: RmgrId::HEAP.0,
            info: 0,
            payload: b"misplaced",
        }
        .encode(&mut bytes);
        std::fs::write(wal_path(dir.path()), &bytes)?;

        let mut reg = RmgrRegistry::new();
        reg.register(
            RmgrId::HEAP,
            Arc::new(Collector(Arc::new(Mutex::new(Vec::new())))),
        );
        let clog = Clog::new();
        let Err(err) = recover(dir.path(), &reg, &clog, Lsn::INVALID) else {
            anyhow::bail!("a record that lies about its start LSN should fail recovery");
        };
        assert!(
            err.to_string().contains("not a record boundary"),
            "unexpected error: {err}"
        );

        Ok(())
    }
}
