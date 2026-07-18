//! Redo-only crash recovery.
//!
//! There is no undo pass: uncommitted heap versions are simply invisible under
//! MVCC and are reclaimed later by VACUUM, so recovery only needs to *reapply*
//! logged changes (ARIES redo) and rebuild the transaction state. This cut
//! replays the entire WAL from the start of the stream; bounding replay to the
//! last checkpoint's redo LSN needs a durable CLOG (to recover the fate of
//! transactions that committed *before* the checkpoint), which is a deliberate
//! follow-up — see `docs/ARCHITECTURE.md §3`.

use std::path::Path;

use crabgresql_txn::{Clog, Xid};

use crate::control::read_control;
use crate::record::{Lsn, WalError, WalRecord};
use crate::rmgr::{RedoContext, RmgrId, RmgrRegistry, XACT_ABORT, XACT_COMMIT};
use crate::wal::wal_path;

pub struct RecoveryResult {
    /// LSN just past the last valid record — where fresh appends must resume.
    pub end_of_wal: Lsn,
    /// The XID the allocator must start from: above every transaction recovered.
    pub next_xid: Xid,
}

/// Replay the WAL under `dir`, rebuilding `clog` from commit/abort records and
/// dispatching every other record to its registered redo handler. Returns the
/// end of the valid log and the next XID to hand out.
pub fn recover(
    dir: &Path,
    registry: &RmgrRegistry,
    clog: &Clog,
) -> Result<RecoveryResult, WalError> {
    let control = read_control(dir)?;
    let mut next_xid = control.map(|c| c.next_xid.0).unwrap_or(Xid::FIRST_NORMAL.0);

    let bytes = match std::fs::read(wal_path(dir)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };

    let mut pos = 0usize;
    while let Some((rec, len)) = WalRecord::decode(&bytes[pos..]) {
        let end_lsn = Lsn((pos + len) as u64);
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
                let handler = registry.get(other.0).ok_or(WalError::UnknownRmgr(other.0))?;
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

    Ok(RecoveryResult { end_of_wal: Lsn(pos as u64), next_xid: Xid(next_xid) })
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
            self.0.lock().unwrap().push(ctx.payload.to_vec());
            Ok(())
        }
    }

    #[test]
    fn replays_committed_records_and_recovers_next_xid() {
        let dir = tempfile::tempdir().unwrap();
        {
            let wal = Wal::open(dir.path()).unwrap();
            wal.append(RmgrId::HEAP, 7, Xid(3), b"insert-a");
            let c = wal.append(RmgrId::XACT, XACT_COMMIT, Xid(3), &[]);
            wal.append(RmgrId::HEAP, 7, Xid(4), b"insert-b"); // never committed
            wal.flush(c).unwrap();
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut reg = RmgrRegistry::new();
        reg.register(RmgrId::HEAP, Arc::new(Collector(Arc::clone(&seen))));
        let clog = Clog::new();
        let res = recover(dir.path(), &reg, &clog).unwrap();

        assert!(clog.is_committed(Xid(3)));
        assert!(!clog.is_committed(Xid(4)));
        assert_eq!(res.next_xid, Xid(5)); // above the highest XID seen (4)
        // Both heap records are redone (redo is oblivious to commit; MVCC hides
        // the uncommitted one at read time).
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn stops_at_a_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let good_end;
        {
            let wal = Wal::open(dir.path()).unwrap();
            wal.append(RmgrId::HEAP, 0, Xid(3), b"ok");
            good_end = wal.append(RmgrId::XACT, XACT_COMMIT, Xid(3), &[]);
            wal.flush(good_end).unwrap();
        }
        // Append raw garbage past the valid records (a torn write).
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(wal_path(dir.path())).unwrap();
            f.write_all(&[0xAB; 40]).unwrap();
        }
        let mut reg = RmgrRegistry::new();
        reg.register(RmgrId::HEAP, Arc::new(Collector(Arc::new(Mutex::new(Vec::new())))));
        let clog = Clog::new();
        let res = recover(dir.path(), &reg, &clog).unwrap();
        assert_eq!(res.end_of_wal, good_end, "recovery ends at the last valid record");
    }
}
