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

use std::path::Path;

use crabgresql_txn::{Clog, Xid};

use crate::control::read_control;
use crate::page::{XLOG_BLCKSZ, is_record_position, page_start};
use crate::reader::{StopReason, WalReader};
use crate::record::{Lsn, WalError};
use crate::rmgr::{RedoContext, RmgrId, RmgrRegistry, XACT_ABORT, XACT_COMMIT};
use crate::segment::{segment_bounds, segment_start, segno_of};

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

/// The LSN of the first record **above** `from` that decodes, passes its CRC, and
/// agrees with where it was found.
///
/// Steps a page at a time rather than a byte at a time. That is the honest
/// granularity now: a record only ever begins at a page's first usable byte,
/// immediately after another record, or immediately after a continuation's tail,
/// so after a break the only positions worth probing are the ones the page
/// headers themselves name. The byte scan this replaces was hunting for a CRC-32C
/// collision that also happened to carry a matching eight-byte offset.
///
/// The `rec_lsn == candidate` self-check is kept verbatim, and is now doubled by
/// the page's own address check — which is what makes a hit evidence rather than
/// a guess.
fn first_valid_record_after(dir: &Path, from: Lsn) -> Result<Option<Lsn>, WalError> {
    let Some((_, highest)) = segment_bounds(dir)? else {
        return Ok(None);
    };
    let ceiling = segment_start(highest + 1);
    let mut page = page_start(from.0) + XLOG_BLCKSZ;
    let mut buf = Vec::new();
    while page < ceiling {
        // The page header names where its first record starts, which is past any
        // tail owed to a record that began earlier.
        let mut reader = WalReader::open_at_page(dir, Lsn(page))?;
        let candidate = reader.position();
        if let Some((rec, _)) = reader.next_into(&mut buf)?
            && rec.rec_lsn == candidate
        {
            return Ok(Some(candidate));
        }
        page += XLOG_BLCKSZ;
    }
    Ok(None)
}

/// Refuse to report `end_of_wal == start` unless there is provably nothing above
/// it to lose.
///
/// The caller feeds that value to [`crate::Wal::reset_to`], which repositions the
/// writer there — so getting it wrong overwrites every record above it while
/// reporting a clean start.
///
/// Two situations reach here, and only one may reposition:
///
/// * We were given a redo point and nothing decodes at it. Refuse, always. The
///   bytes below `start` were never read, so a record we are sitting inside is
///   invisible from here — "nothing decodes ahead" does not mean there is nothing
///   to lose. Unlike the flat log, this now also refuses when the redo point
///   lands on *padding*: preallocation makes "the redo point is the end of the
///   log" and "the redo point is past the end of the log" physically identical,
///   and production never produces either — a checkpoint samples its redo point
///   before appending the record that goes at or above it.
/// * We are replaying the whole stream and its head does not decode. Refuse only
///   if a valid record still sits further on, since repositioning would destroy
///   it. Otherwise there is genuinely nothing behind or ahead: a crash during the
///   very first flush leaves exactly this, and nothing in it was acknowledged
///   because that flush never returned. Repositioning is *required* there, or an
///   ordinary crash would leave a cluster that cannot start at all.
fn guard_the_reposition(dir: &Path, start: Lsn, stop: Option<StopReason>) -> Result<(), WalError> {
    let detail = match stop {
        Some(stop) => stop.to_string(),
        None => "the log is empty".to_string(),
    };
    // A record that decodes but names a different position, or a page that
    // validates but does not continue the record running onto it, is positive
    // evidence of log belonging somewhere else — not of log being absent.
    // Repositioning on top of it would be repositioning on top of something.
    // This is the paged form of the old "the redo point is not a record boundary"
    // check, which the reader now catches before a record reaches replay.
    if let Some(StopReason::Misplaced { .. } | StopReason::BrokenContrecord { .. }) = stop {
        return Err(WalError::Redo(format!(
            "recovery resumed at {start} but {detail} — that is not a record \
             boundary; refusing to reposition the log there"
        )));
    }
    if start.is_valid() {
        return Err(WalError::Redo(format!(
            "recovery resumed at {start} but no record decodes there ({detail}); \
             refusing to reposition the log to a point it cannot vouch for"
        )));
    }
    match first_valid_record_after(dir, start)? {
        Some(at) => Err(WalError::Redo(format!(
            "the log head does not decode ({detail}), but a valid record starts \
             at {at}; refusing to truncate the log past it"
        ))),
        None => Ok(()),
    }
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

    // A redo point inside a page header can never be a record boundary, and
    // saying so here names the fault instead of reporting a mysterious decode
    // failure at a plausible-looking LSN.
    if start.is_valid() && !is_record_position(start.0) {
        return Err(WalError::Redo(format!(
            "recovery resumed at {start}, which lands inside the header of the \
             wal page at {}",
            Lsn(page_start(start.0))
        )));
    }
    if start.is_valid() && segment_bounds(dir)?.is_none_or(|(_, hi)| segno_of(start.0) > hi) {
        return Err(WalError::Redo(format!(
            "recovery must resume at {start} but wal segment {:016X} does not exist",
            segno_of(start.0)
        )));
    }

    let mut reader = WalReader::open(dir, start)?;
    // The reader normalizes `Lsn::INVALID` to the lowest surviving segment, which
    // is where a whole-stream replay actually begins now that recycling removes
    // the log's prefix.
    let origin = reader.position();
    let mut replayed = 0usize;
    let mut buf = Vec::new();
    while let Some((rec, end_lsn)) = reader.next_into(&mut buf)? {
        // The first record must begin exactly where we were told to resume, or
        // the redo point is not a record boundary.
        if replayed == 0 && rec.rec_lsn != origin {
            return Err(WalError::Redo(format!(
                "recovery resumed at {origin} but the record there claims to start \
                 at {} — the redo point is not a record boundary",
                rec.rec_lsn
            )));
        }
        replayed += 1;
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
    }

    if replayed == 0 {
        guard_the_reposition(dir, start, reader.stop_reason())?;
    }

    Ok(RecoveryResult {
        end_of_wal: reader.position(),
        next_xid: Xid(next_xid),
        replayed_from: start,
    })
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::WAL_SEG_SIZE;
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
        // Raw garbage past the valid records (a torn write).
        crate::segment::scribble(dir.path(), good_end, Lsn(good_end.0 + 40), 0xAB)?;
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
        // Well past the end: a segment that does not exist at all. Nearer
        // overshoots land on padding, which is refused by the guard instead.
        let Err(err) = recover(dir.path(), &reg, &clog, Lsn(end.0 + WAL_SEG_SIZE)) else {
            anyhow::bail!("recovery past the end of the log should fail");
        };
        assert!(
            err.to_string().contains("segment"),
            "error should name the missing segment: {err}"
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
        let mut builder = crate::reader::testkit::Builder::at(Lsn::START);
        let mut bytes = Vec::new();
        crate::record::WalRecord {
            rec_lsn: Lsn(Lsn::START.0 + 4096),
            xid: Xid(3),
            rmgr: RmgrId::HEAP.0,
            info: 0,
            payload: b"misplaced",
        }
        .encode(&mut bytes);
        builder.raw(&bytes);
        builder.finish(dir.path())?;

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
            err.to_string().contains("refusing to reposition"),
            "unexpected error: {err}"
        );

        Ok(())
    }

    /// A redo point with no record at it is refused, even when it names exactly
    /// the end of the log.
    ///
    /// The flat log accepted that case, distinguishing it from "past the end" by
    /// the file's length. Preallocation makes the two physically identical — both
    /// are padding on a valid page — and accepting them would let `reset_to` park
    /// the writer above the real end, leaving a hole of zeros that stops every
    /// future replay short of everything written after it.
    ///
    /// Production never produces such a redo point: `redo_point()` is sampled
    /// *before* the checkpoint record is appended, so the published LSN always
    /// names the start of a record that exists.
    #[test]
    fn a_redo_point_with_no_record_at_it_is_refused() -> anyhow::Result<()> {
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
        let Err(err) = recover(dir.path(), &reg, &clog, end) else {
            anyhow::bail!("a redo point with no record at it must be refused");
        };
        assert!(
            err.to_string().contains("refusing to reposition"),
            "unexpected error: {err}"
        );

        Ok(())
    }

    /// The redo point a checkpoint really publishes: the start of a record. It
    /// replays that record and nothing below it.
    #[test]
    fn a_redo_point_on_the_last_record_replays_just_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let last;
        {
            let wal = Wal::open(dir.path())?;
            wal.append(RmgrId::HEAP, 7, Xid(3), b"below");
            last = wal.append(RmgrId::HEAP, 7, Xid(4), b"at-the-redo-point");
            wal.flush(last.end)?;
        }
        write_floor(dir.path(), 100)?;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut reg = RmgrRegistry::new();
        reg.register(RmgrId::HEAP, Arc::new(Collector(Arc::clone(&seen))));
        let clog = Clog::new();
        let res = recover(dir.path(), &reg, &clog, last.start)?;
        assert_eq!(res.end_of_wal, last.end);
        assert_eq!(
            seen.lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"))
                .clone(),
            vec![b"at-the-redo-point".to_vec()]
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
            // Big enough to fill the first page, so the survivor lands on a page
            // of its own: the probe for a record above the damage steps a page at
            // a time, and two records sharing a page would leave it nothing to
            // find — the test would then pass by truncating, for the wrong reason.
            let head = vec![0xCD; crate::page::XLP_USABLE as usize - crate::record::WalRecord::MIN_LEN];
            wal.append(RmgrId::HEAP, 7, Xid(3), &head);
            second = wal.append(RmgrId::HEAP, 7, Xid(4), b"still-good");
            wal.flush(second.end)?;
        }
        // Corrupt the first record's payload, leaving its length intact so the
        // second record stays where it is.
        let payload_at = Lsn(Lsn::START.0 + crate::record::WalRecord::HEADER_LEN as u64);
        crate::segment::scribble(dir.path(), payload_at, Lsn(payload_at.0 + 4), 0xAB)?;

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
        // The tail of the write never landed: zeros from twelve bytes in. The
        // page header itself survives, which is what a partial page write looks
        // like — the header goes down first.
        let from = Lsn(Lsn::START.0 + 12);
        crate::segment::scribble(dir.path(), from, Lsn(Lsn::START.0 + 4096), 0x00)?;

        let reg = RmgrRegistry::new();
        let clog = Clog::new();
        let res = recover(dir.path(), &reg, &clog, Lsn::INVALID)?;
        assert_eq!(
            res.end_of_wal,
            Lsn::START,
            "a log with nothing recoverable in it repositions to the head"
        );

        Ok(())
    }

    /// A fresh cluster has no segments at all. Segments are created on the first
    /// *write*, not on open — an opening scan that preallocated one would be
    /// manufacturing the very zeros it then reads as the end of the log.
    #[test]
    fn a_fresh_cluster_has_an_empty_log_and_recovers_cleanly() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        drop(Wal::open(dir.path())?);
        assert_eq!(crate::segment::segment_bounds(dir.path())?, None);

        let reg = RmgrRegistry::new();
        let clog = Clog::new();
        let res = recover(dir.path(), &reg, &clog, Lsn::INVALID)?;
        assert_eq!(res.end_of_wal, Lsn::START);

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
