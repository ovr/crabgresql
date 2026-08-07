//! Redo-only crash recovery.
//!
//! There is no undo pass: uncommitted heap versions are simply invisible under
//! MVCC and are reclaimed later by VACUUM, so recovery only needs to *reapply*
//! logged changes (ARIES redo) and rebuild the transaction state.
//!
//! [`recover`] takes the LSN to resume from. Because an [`Lsn`] is literally a
//! byte offset into the single stream file, that is a positioned read rather
//! than a scan.
//!
//! Production passes the redo point the last checkpoint recorded in `pg_control`.
//! Two things had to exist first, and now do: a durable CLOG, to recover the fate
//! of transactions that committed *below* the redo point, and something that
//! writes the redo point down. [`Lsn::INVALID`] (`0`) still means the whole
//! stream, and is what a fresh cluster — or one whose control file is unreadable —
//! gets. Replaying more than necessary is always safe, because every redo handler
//! is gated on the target page's LSN.

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
    // `read_exact_at` handles short reads and, unlike a hand-rolled fill loop,
    // surfaces an unexpected EOF as an error instead of silently shortening the
    // buffer — which would stop the decode early and hand the caller a lower
    // `end_of_wal` to truncate to.
    file.read_exact_at(&mut bytes, start.0)?;
    Ok(bytes)
}

/// The offset of the first record in `bytes` that both passes its CRC and agrees
/// with where it was found — `rec_lsn == start + offset`. Offset 0 is skipped; the
/// caller has already established that nothing decodes there.
///
/// That second condition is what makes this evidence rather than a guess: a false
/// positive would need a CRC-32C collision *and* an eight-byte field that happens
/// to equal the offset it was found at. `recover` leans on the same self-check
/// when it validates the first record against `start`.
fn first_valid_record_offset(bytes: &[u8], start: Lsn) -> Option<usize> {
    (1..bytes.len()).find(|&offset| {
        WalRecord::decode(&bytes[offset..])
            .is_some_and(|(rec, _)| rec.rec_lsn == Lsn(start.0 + offset as u64))
    })
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
    // Replay raises this floor from the records it reads, but only those at or
    // above `start`. Whole-stream replay therefore sees every XID that ever
    // appeared in the log; a bounded replay does not, and the control file is
    // the only remaining floor. Starting without one would let the allocator
    // reissue an XID already stamped on committed tuples in the skipped prefix —
    // the reused XID is `InProgress` in the CLOG, so the moment it commits, the
    // old tuples become visible. Refuse instead.
    if start.is_valid() && control.is_none() {
        return Err(WalError::Redo(format!(
            "recovery from {start} needs a control file for the next-XID floor, \
             but none is readable; replay below the redo point would be skipped \
             and XIDs could be reissued"
        )));
    }
    let mut next_xid = control.map(|c| c.next_xid.0).unwrap_or(Xid::FIRST_NORMAL.0);

    let bytes = read_from(dir, start)?;

    let mut pos = 0usize;
    while let Some((rec, len)) = WalRecord::decode(&bytes[pos..]) {
        // The first record must begin exactly where we were told to resume, or
        // the redo point is not a record boundary. `pos == 0` is true on exactly
        // the first iteration, since `len` is always positive.
        if pos == 0 && rec.rec_lsn != start {
            return Err(WalError::Redo(format!(
                "recovery resumed at {start} but the record there claims to start \
                 at {} — the redo point is not a record boundary",
                rec.rec_lsn
            )));
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
            // Nothing to redo into a page, but the payload carries an XID floor
            // that no other record can supply: a transaction touching only
            // `UNLOGGED`/`TEMP` relations never appears in the log, so the
            // envelope scan above cannot see it. Raising the floor is a `max`,
            // not a `+ 1` — unlike `rec.xid`, which names a *used* XID, this
            // field is already the next one to hand out.
            RmgrId::CHECKPOINT => {
                let ckpt = crate::ckpt::replay(rec.info, rec.payload, rec.rec_lsn)?;
                next_xid = next_xid.max(ckpt.next_xid.0);
            }
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

    // Nothing decoded where we were told to resume, yet there are bytes there.
    // Returning `end_of_wal == start` sends the caller into `Wal::reset_to`, which
    // `set_len`s the stream to that point — so getting this wrong destroys every
    // record above it while reporting a clean start.
    //
    // Two situations reach it, and only one may truncate:
    //
    // * We were given a redo point and it is not a record boundary. Refuse. The
    //   bytes below `start` were never read, so a record we are sitting in the
    //   middle of is invisible from here — "nothing decodes ahead" does not mean
    //   there is nothing to lose. (A redo point never exceeds the flush boundary,
    //   see `Wal::redo_point`, so a record torn *at* one cannot happen either.)
    // * We are replaying the whole stream and its head is damaged. Refuse only if
    //   a valid record still decodes further on, since truncating would destroy
    //   it. Otherwise there is genuinely nothing behind or ahead: a crash during
    //   the very first `flush` leaves exactly this — `write_all_at` is a `pwrite`
    //   loop, so a partial first record on disk is ordinary — and nothing in it
    //   was ever acknowledged, because that flush never returned. Truncating is
    //   *required* there, or an ordinary crash would leave a cluster that cannot
    //   start at all.
    //
    // The second case is why this is no longer gated on `start.is_valid()` alone:
    // whole-stream replay is a routine path now — any control file we cannot read
    // falls back to it — and it is precisely the path on which a damaged head
    // would otherwise erase every record behind it. An empty region is fine either
    // way: that is a checkpoint with no activity after it.
    if pos == 0 && !bytes.is_empty() {
        if start.is_valid() {
            return Err(WalError::Redo(format!(
                "recovery resumed at {start} but no record decodes there \
                 ({} bytes follow); refusing to truncate the log to that point",
                bytes.len()
            )));
        }
        if let Some(offset) = first_valid_record_offset(&bytes, start) {
            return Err(WalError::Redo(format!(
                "the log head does not decode, but a valid record starts at {} \
                 ({} bytes follow); refusing to truncate the log past it",
                Lsn(offset as u64),
                bytes.len()
            )));
        }
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

    /// Bounded replay requires a control file for the next-XID floor, so every
    /// test that passes a non-zero start needs one on disk.
    fn write_floor(dir: &Path, next_xid: u64) -> anyhow::Result<()> {
        crate::control::write_control(
            dir,
            &crate::control::ControlFile {
                next_xid: Xid(next_xid),
                // Irrelevant here: these tests pass the start LSN to `recover`
                // directly rather than going through the control file.
                redo_lsn: crate::record::Lsn::INVALID,
                clean_shutdown: false,
            },
        )?;
        Ok(())
    }

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
        write_floor(dir.path(), 100)?;
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
        write_floor(dir.path(), 100)?;
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

    /// The destructive case: a redo point at which nothing decodes must NOT come
    /// back as a clean empty log. `end_of_wal` would equal `start`, and the
    /// caller feeds that to `Wal::reset_to` — truncating away every committed
    /// record above the checkpoint.
    #[test]
    fn a_redo_point_where_nothing_decodes_is_rejected_not_truncated() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let second;
        {
            let wal = Wal::open(dir.path())?;
            wal.append(RmgrId::HEAP, 7, Xid(3), b"first");
            second = wal.append(RmgrId::HEAP, 7, Xid(4), b"second");
            wal.flush(second.end)?;
        }
        // A redo point one byte inside the second record: the bytes there are a
        // record's interior, so `decode` fails CRC and yields nothing.
        let bad = Lsn(second.start.0 + 1);
        let mut reg = RmgrRegistry::new();
        reg.register(
            RmgrId::HEAP,
            Arc::new(Collector(Arc::new(Mutex::new(Vec::new())))),
        );
        // The control file must exist, or the next-XID guard fires first and we
        // would not be testing what we think we are.
        write_floor(dir.path(), 100)?;
        let clog = Clog::new();
        let Err(err) = recover(dir.path(), &reg, &clog, bad) else {
            anyhow::bail!("a redo point where nothing decodes must not report a clean log");
        };
        assert!(
            err.to_string().contains("refusing to truncate"),
            "unexpected error: {err}"
        );

        Ok(())
    }

    /// A redo point exactly at end-of-file is legitimate — a checkpoint with no
    /// activity after it — and must not be confused with the case above.
    #[test]
    fn a_redo_point_at_end_of_log_replays_nothing_and_succeeds() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let end;
        {
            let wal = Wal::open(dir.path())?;
            end = wal.append(RmgrId::HEAP, 7, Xid(3), b"only").end;
            wal.flush(end)?;
        }
        write_floor(dir.path(), 100)?;
        let reg = RmgrRegistry::new();
        let clog = Clog::new();
        let res = recover(dir.path(), &reg, &clog, end)?;
        assert_eq!(res.end_of_wal, end);
        assert_eq!(
            res.next_xid,
            Xid(100),
            "the control file supplies the floor"
        );

        Ok(())
    }

    /// Bounded replay cannot see XIDs below the redo point, so without a control
    /// file there is no next-XID floor at all and the allocator would reissue
    /// XIDs already stamped on committed tuples.
    #[test]
    fn a_non_zero_start_without_a_control_file_is_rejected() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let second;
        {
            let wal = Wal::open(dir.path())?;
            wal.append(RmgrId::HEAP, 7, Xid(3), b"below");
            second = wal.append(RmgrId::HEAP, 7, Xid(4), b"above");
            wal.flush(second.end)?;
        }
        let reg = RmgrRegistry::new();
        let clog = Clog::new();
        // No control file was ever written.
        let Err(err) = recover(dir.path(), &reg, &clog, second.start) else {
            anyhow::bail!("bounded replay without a next-XID floor must be refused");
        };
        assert!(
            err.to_string().contains("next-XID floor"),
            "unexpected error: {err}"
        );

        Ok(())
    }

    /// A whole-stream replay whose *head* is damaged must not report a clean log:
    /// the caller feeds `end_of_wal` to `Wal::reset_to`, so returning zero would
    /// erase every record behind the damage. This is reachable in production now —
    /// an unreadable control file falls back to a whole-stream replay, and bounded
    /// replay is what lets damage accumulate in a prefix nothing reads.
    #[test]
    fn a_damaged_log_head_does_not_truncate_the_records_behind_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let second;
        {
            let wal = Wal::open(dir.path())?;
            wal.append(RmgrId::HEAP, 7, Xid(3), b"damaged-head");
            second = wal.append(RmgrId::HEAP, 7, Xid(4), b"still-good");
            wal.flush(second.end)?;
        }
        // Corrupt the first record's payload, leaving its length intact so the
        // second record stays where it is.
        {
            use std::os::unix::fs::FileExt;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(wal_path(dir.path()))?;
            file.write_all_at(&[0xAB; 4], WalRecord::HEADER_LEN as u64)?;
            file.sync_all()?;
        }

        let reg = RmgrRegistry::new();
        let clog = Clog::new();
        let Err(err) = recover(dir.path(), &reg, &clog, Lsn::INVALID) else {
            anyhow::bail!("a damaged head with valid records behind it must be refused");
        };
        assert!(
            err.to_string().contains("refusing to truncate"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains(&second.start.to_string()),
            "the error should name the record that survived: {err}"
        );

        Ok(())
    }

    /// The counterweight to the test above, and the reason the refusal cannot be
    /// unconditional: a crash during the very first `flush` leaves a partial record
    /// at offset 0 and nothing else. None of it was ever acknowledged — that flush
    /// never returned — so recovery must truncate and start, not refuse.
    #[test]
    fn a_torn_first_record_on_a_fresh_cluster_still_starts_clean() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        {
            let wal = Wal::open(dir.path())?;
            let range = wal.append(RmgrId::HEAP, 7, Xid(3), b"never-acknowledged");
            wal.flush(range.end)?;
        }
        // Keep only the first few bytes: the tail of the write never landed.
        {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(wal_path(dir.path()))?;
            file.set_len(12)?;
            file.sync_all()?;
        }

        let reg = RmgrRegistry::new();
        let clog = Clog::new();
        let res = recover(dir.path(), &reg, &clog, Lsn::INVALID)?;
        assert_eq!(
            res.end_of_wal,
            Lsn::INVALID,
            "a log with nothing recoverable in it truncates to empty"
        );

        Ok(())
    }

    /// A fresh cluster has a zero-length WAL, which is the exemption the guard
    /// above leans on (`!bytes.is_empty()`).
    #[test]
    fn a_fresh_cluster_has_an_empty_log_and_recovers_cleanly() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        drop(Wal::open(dir.path())?);
        assert_eq!(std::fs::metadata(wal_path(dir.path()))?.len(), 0);

        let reg = RmgrRegistry::new();
        let clog = Clog::new();
        let res = recover(dir.path(), &reg, &clog, Lsn::INVALID)?;
        assert_eq!(res.end_of_wal, Lsn::INVALID);

        Ok(())
    }

    /// The floor a checkpoint contributes is the payload's `next_xid` verbatim.
    /// A transaction that only touched `UNLOGGED`/`TEMP` relations writes no
    /// record at all, so scanning envelope XIDs cannot find it — which is why
    /// this record exists — and the value is already a *next* XID, so raising the
    /// floor past it would leak one XID per checkpoint.
    #[test]
    fn a_checkpoint_record_raises_the_next_xid_floor() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        {
            let wal = Wal::open(dir.path())?;
            wal.append(RmgrId::HEAP, 7, Xid(3), b"row");
            let payload = crate::ckpt::Checkpoint {
                redo_lsn: Lsn::INVALID,
                next_xid: Xid(500),
            }
            .encode();
            let end = wal
                .append(
                    RmgrId::CHECKPOINT,
                    crate::ckpt::CHECKPOINT_ONLINE,
                    Xid::INVALID,
                    &payload,
                )
                .end;
            wal.flush(end)?;
        }
        let mut reg = RmgrRegistry::new();
        reg.register(
            RmgrId::HEAP,
            Arc::new(Collector(Arc::new(Mutex::new(Vec::new())))),
        );
        let clog = Clog::new();
        let res = recover(dir.path(), &reg, &clog, Lsn::INVALID)?;
        assert_eq!(
            res.next_xid,
            Xid(500),
            "the checkpoint's next_xid is the floor, exactly"
        );

        Ok(())
    }

    /// A checkpoint payload we cannot read must stop recovery, not be skipped:
    /// it carries an XID floor, and continuing without it lets the allocator
    /// reissue an XID already stamped on committed tuples.
    #[test]
    fn an_unreadable_checkpoint_record_fails_recovery() -> anyhow::Result<()> {
        for (info, payload) in [
            (crate::ckpt::CHECKPOINT_ONLINE, b"too short".to_vec()),
            (
                0x7F,
                crate::ckpt::Checkpoint {
                    redo_lsn: Lsn::INVALID,
                    next_xid: Xid(9),
                }
                .encode(),
            ),
        ] {
            let dir = tempfile::tempdir()?;
            {
                let wal = Wal::open(dir.path())?;
                let end = wal
                    .append(RmgrId::CHECKPOINT, info, Xid::INVALID, &payload)
                    .end;
                wal.flush(end)?;
            }
            let reg = RmgrRegistry::new();
            let clog = Clog::new();
            let Err(err) = recover(dir.path(), &reg, &clog, Lsn::INVALID) else {
                anyhow::bail!("a checkpoint record with info {info:#x} should fail recovery");
            };
            assert!(
                err.to_string().contains("checkpoint"),
                "unexpected error: {err}"
            );
        }

        Ok(())
    }

    /// A redo point is always a record boundary, and a checkpoint record is one —
    /// so resuming exactly at a checkpoint is the ordinary case, not an edge one.
    #[test]
    fn replay_can_resume_at_a_checkpoint_record() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let ckpt_at;
        {
            let wal = Wal::open(dir.path())?;
            wal.append(RmgrId::HEAP, 7, Xid(3), b"below-redo");
            let payload = crate::ckpt::Checkpoint {
                redo_lsn: Lsn::INVALID,
                next_xid: Xid(7),
            }
            .encode();
            ckpt_at = wal.append(
                RmgrId::CHECKPOINT,
                crate::ckpt::CHECKPOINT_ONLINE,
                Xid::INVALID,
                &payload,
            );
            wal.flush(ckpt_at.end)?;
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut reg = RmgrRegistry::new();
        reg.register(RmgrId::HEAP, Arc::new(Collector(Arc::clone(&seen))));
        write_floor(dir.path(), 5)?;
        let clog = Clog::new();
        let res = recover(dir.path(), &reg, &clog, ckpt_at.start)?;

        assert_eq!(res.replayed_from, ckpt_at.start);
        assert_eq!(res.end_of_wal, ckpt_at.end);
        assert_eq!(
            res.next_xid,
            Xid(7),
            "the checkpoint's floor outranks the control file's"
        );
        assert!(
            seen.lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"))
                .is_empty(),
            "the heap record below the redo point must not be replayed"
        );

        Ok(())
    }
}
