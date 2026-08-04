//! Walking the paged log: page validation, record reassembly, and the one place
//! that decides where the log ends.
//!
//! Both [`crate::Wal::open`] and [`crate::recover`] read through this, so the
//! writer's idea of where to resume and replay's idea of what to replay are the
//! same function of the same bytes rather than two implementations that have to
//! be kept in agreement.
//!
//! ## Why the end of the log is knowable without the file's length
//!
//! Segments are preallocated and the tail page is zero-padded, so a file's length
//! says nothing. Four rules replace it, and the walk stops at the first position
//! that breaks one:
//!
//! 1. **A page is log only if its header validates** — magic, header CRC, and
//!    `pageaddr` equal to the page's own position. A preallocated page is zeros;
//!    a recycled segment's page records the address of its previous life, which a
//!    forward rename leaves strictly below where it now sits.
//! 2. **A record is log only if** it lies entirely on validating pages, each
//!    continuation page claims it with a `rem_len` equal to what is still owed,
//!    its CRC checks out, and its `rec_lsn` equals the position it was found at.
//! 3. **The walk never seeks.** It starts at a known-good LSN and moves forward
//!    one record at a time, so bytes beyond a stopping point are unreachable by
//!    construction — the only route to them is a contiguous chain of records that
//!    validate, which is the definition of log.
//! 4. **It never resynchronizes.** After a recycle a perfectly valid-looking page
//!    can sit above a break; skipping ahead to it would replay a previous cycle's
//!    records. The first failure is the end, full stop.

use std::path::Path;

use crate::aligned::AlignedBuf;
use crate::page::{
    PageHeader, XLOG_BLCKSZ, XLP_PAGE_HEADER_SIZE, advance, first_usable, page_offset, page_start,
};
use crate::record::{Lsn, WalError, WalRecord};
use crate::segment::{Segments, seg_offset, segment_bounds, segment_start, segno_of};

/// Why a walk ended.
///
/// The distinction is the whole end-of-log design: [`StopReason::NoPage`] is the
/// ordinary end, everything else is evidence that something wrote bytes there
/// that we cannot vouch for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// No page at that address: the segment is absent, or the page is zeros,
    /// fails its magic or header CRC, or records an address other than its own.
    NoPage { at: Lsn },
    /// The page is fine and the rest of it, from here on, is zeros — the padding
    /// a flush leaves after the last record. The ordinary end of a live log, and
    /// the reason the file's length is not needed to find it.
    Padding { at: Lsn },
    /// The page is fine but the record on it is incomplete, over-long, or fails
    /// its CRC. The ordinary torn tail.
    TornRecord { at: Lsn },
    /// A record decoded, but it claims to begin somewhere else. Not a torn tail —
    /// a record found where it does not belong.
    Misplaced { at: Lsn, claims: Lsn },
    /// A record ran onto a page that is itself valid but does not continue it:
    /// the contrecord flag is clear, or `rem_len` disagrees with what is owed.
    BrokenContrecord { at: Lsn, page: Lsn },
}

impl StopReason {
    pub fn at(&self) -> Lsn {
        match self {
            StopReason::NoPage { at }
            | StopReason::Padding { at }
            | StopReason::TornRecord { at }
            | StopReason::Misplaced { at, .. }
            | StopReason::BrokenContrecord { at, .. } => *at,
        }
    }
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StopReason::NoPage { at } => write!(f, "no valid wal page at {at}"),
            StopReason::Padding { at } => write!(f, "the log ends at {at}"),
            StopReason::TornRecord { at } => write!(f, "a torn record at {at}"),
            StopReason::Misplaced { at, claims } => {
                write!(f, "the record at {at} claims to start at {claims}")
            }
            StopReason::BrokenContrecord { at, page } => write!(
                f,
                "the record at {at} runs onto {page}, which does not continue it"
            ),
        }
    }
}

/// A forward cursor over the log.
pub struct WalReader {
    segs: Segments,
    /// LSN of the next record to return; never advances past a failure.
    next: u64,
    page: AlignedBuf,
    /// Which page `self.page` holds, and its validated header.
    loaded: Option<(u64, PageHeader)>,
    stop: Option<StopReason>,
}

impl WalReader {
    /// Position at `from`. [`Lsn::INVALID`] means the start of the lowest segment
    /// on disk — not byte zero, since recycling removes the log's prefix — or
    /// [`Lsn::START`] when there are no segments at all.
    pub fn open(dir: &Path, from: Lsn) -> Result<WalReader, WalError> {
        let next = if from.is_valid() {
            from.0
        } else {
            match segment_bounds(dir)? {
                Some((lo, _)) => first_usable(segment_start(lo)),
                None => Lsn::START.0,
            }
        };
        let mut page = AlignedBuf::with_pages(1);
        page.extend_from_slice(&vec![0u8; XLOG_BLCKSZ as usize]);
        Ok(WalReader {
            segs: Segments::new(dir),
            next,
            page,
            loaded: None,
            stop: None,
        })
    }

    /// Where the walk stands: the start of the record that has not been returned.
    /// After exhaustion this is the end of the log.
    pub fn position(&self) -> Lsn {
        Lsn(self.next)
    }

    pub fn stop_reason(&self) -> Option<StopReason> {
        self.stop
    }

    /// The next record, reassembled into `buf`, together with its end LSN — the
    /// same value `Wal::append` handed the writer, which is what a data page
    /// carries as its `pd_lsn`.
    pub fn next_into<'a>(
        &mut self,
        buf: &'a mut Vec<u8>,
    ) -> Result<Option<(WalRecord<'a>, Lsn)>, WalError> {
        if self.stop.is_some() {
            return Ok(None);
        }
        let at = self.next;
        if !self.load_page(page_start(at))? {
            return Ok(None);
        }
        // Zeros to the end of the page mean the flush that wrote it stopped here.
        // Distinguishing that from garbage is what the old flat log could not do:
        // it could only ask whether any bytes followed, so thirty-seven bytes of
        // 0xAB and thirty-seven of 0x00 were the same answer.
        if self.page.as_slice()[page_offset(at) as usize..]
            .iter()
            .all(|&b| b == 0)
        {
            self.stop = Some(StopReason::Padding { at: Lsn(at) });
            return Ok(None);
        }
        // `total_len` first, so the gather below knows how far to run. Page
        // validity is checked here; the continuation rules are checked by the
        // gather, which by then knows how many bytes are owed at each crossing.
        let mut len_bytes = Vec::new();
        if !self.gather(at, 4, false, &mut len_bytes)? {
            return Ok(None);
        }
        let total_len = u32::from_le_bytes(match len_bytes[..4].try_into() {
            Ok(bytes) => bytes,
            // `gather` returned four bytes or it returned false.
            Err(_) => unreachable!("gather returned fewer than four bytes"),
        }) as usize;
        if !(WalRecord::MIN_LEN..=WalRecord::MAX_LEN).contains(&total_len) {
            self.stop = Some(StopReason::TornRecord { at: Lsn(at) });
            return Ok(None);
        }
        if !self.gather(at, total_len, true, buf)? {
            return Ok(None);
        }
        let Some((rec, decoded)) = WalRecord::decode(buf) else {
            self.stop = Some(StopReason::TornRecord { at: Lsn(at) });
            return Ok(None);
        };
        debug_assert_eq!(decoded, total_len);
        // The same self-check the flat log had, and the reason a hit is evidence
        // rather than a guess: a false positive needs a CRC-32C collision *and*
        // an eight-byte field that happens to equal the offset it was found at.
        if rec.rec_lsn != Lsn(at) {
            self.stop = Some(StopReason::Misplaced {
                at: Lsn(at),
                claims: rec.rec_lsn,
            });
            return Ok(None);
        }
        let end = advance(at, total_len as u64);
        self.next = end;
        Ok(Some((rec, Lsn(end))))
    }

    /// Load and validate the page starting at `page_lsn`. `false` records
    /// [`StopReason::NoPage`] and means the log ends at [`WalReader::next`].
    fn load_page(&mut self, page_lsn: u64) -> Result<bool, WalError> {
        if matches!(self.loaded, Some((at, _)) if at == page_lsn) {
            return Ok(true);
        }
        self.loaded = None;
        let ok = self.segs.read_at(
            segno_of(page_lsn),
            seg_offset(page_lsn),
            self.page.as_mut_slice(),
        )?;
        let header = if ok {
            PageHeader::decode(self.page.as_slice())
        } else {
            None
        };
        // `pageaddr` is checked here and nowhere else, unconditionally, on every
        // page: it is what makes a recycled segment's leftovers — whose every
        // record still passes its own CRC — stop the walk.
        // `rem_len` is the whole remainder of a record, so it can exceed a page;
        // the only bound worth checking here is the one that stops a garbage
        // value from being believed at all.
        let valid = header
            .filter(|h| h.pageaddr == page_lsn && h.rem_len as usize <= WalRecord::MAX_LEN);
        match valid {
            Some(header) => {
                self.loaded = Some((page_lsn, header));
                Ok(true)
            }
            None => {
                self.stop = Some(StopReason::NoPage { at: Lsn(page_lsn) });
                Ok(false)
            }
        }
    }

    /// Copy `n` bytes of the stream from `at` into `out`, hopping page headers.
    ///
    /// `check_continuation` is off for the four-byte length probe, which cannot
    /// yet know how many bytes will be owed at a crossing; the full gather that
    /// follows re-reads the same bytes with it on.
    fn gather(
        &mut self,
        at: u64,
        n: usize,
        check_continuation: bool,
        out: &mut Vec<u8>,
    ) -> Result<bool, WalError> {
        out.clear();
        let mut pos = at;
        let mut left = n;
        let mut crossed = false;
        while left > 0 {
            let page_lsn = page_start(pos);
            if !self.load_page(page_lsn)? {
                return Ok(false);
            }
            if crossed && check_continuation {
                let owed = u32::try_from(left).unwrap_or(u32::MAX);
                let continues = matches!(
                    self.loaded,
                    Some((_, header)) if header.is_contrecord() && header.rem_len == owed
                );
                if !continues {
                    self.stop = Some(StopReason::BrokenContrecord {
                        at: Lsn(at),
                        page: Lsn(page_lsn),
                    });
                    return Ok(false);
                }
            }
            let off = page_offset(pos) as usize;
            let take = (XLOG_BLCKSZ as usize - off).min(left);
            out.extend_from_slice(&self.page.as_slice()[off..off + take]);
            left -= take;
            pos += take as u64;
            if page_offset(pos) == 0 {
                pos += XLP_PAGE_HEADER_SIZE;
            }
            crossed = true;
        }
        debug_assert_eq!(pos, advance(at, n as u64));
        Ok(true)
    }
}

/// Walk from `from` to the end of the log and report where it ends.
///
/// What [`crate::Wal::open`] positions itself with. Records are decoded and
/// discarded: this is a physical scan, with no resource-manager dispatch and no
/// registry.
pub fn end_of_wal(dir: &Path, from: Lsn) -> Result<Lsn, WalError> {
    let mut reader = WalReader::open(dir, from)?;
    let mut buf = Vec::new();
    while reader.next_into(&mut buf)?.is_some() {}
    Ok(reader.position())
}

#[cfg(test)]
pub(crate) mod testkit {
    //! Lays records into segment files directly, so the reader can be exercised
    //! — including on pages no correct writer would produce — before the writer
    //! is converted.

    use super::*;
    use crate::page::XLP_FIRST_IS_CONTRECORD;
    use crabgresql_txn::Xid;

    /// A whole-page image of the log, held in memory and spilled to segment files
    /// on [`Builder::finish`].
    pub struct Builder {
        /// Page-aligned LSN of `bytes[0]`.
        base: u64,
        bytes: Vec<u8>,
        at: u64,
    }

    impl Builder {
        /// Start laying records at `at`, which must be a record position.
        pub fn at(at: Lsn) -> Builder {
            assert_eq!(page_offset(at.0), XLP_PAGE_HEADER_SIZE, "start on a fresh page");
            let base = page_start(at.0);
            let mut builder = Builder {
                base,
                bytes: Vec::new(),
                at: base,
            };
            builder.open_page(0, 0);
            builder
        }

        fn open_page(&mut self, rem_len: u32, info: u16) {
            let mut header = [0u8; XLP_PAGE_HEADER_SIZE as usize];
            PageHeader {
                info,
                rem_len,
                pageaddr: self.at,
            }
            .encode(&mut header);
            self.bytes.extend_from_slice(&header);
            self.at += XLP_PAGE_HEADER_SIZE;
        }

        /// Append raw stream bytes, opening pages as they fill.
        pub fn raw(&mut self, mut bytes: &[u8]) -> &mut Builder {
            while !bytes.is_empty() {
                let room = (XLOG_BLCKSZ - page_offset(self.at)) as usize;
                let take = room.min(bytes.len());
                self.bytes.extend_from_slice(&bytes[..take]);
                self.at += take as u64;
                bytes = &bytes[take..];
                if page_offset(self.at) == 0 {
                    let rem = bytes.len() as u32;
                    let info = if rem > 0 { XLP_FIRST_IS_CONTRECORD } else { 0 };
                    self.open_page(rem, info);
                }
            }
            self
        }

        /// Append one encoded record, returning its `(start, end)`.
        pub fn record(&mut self, xid: Xid, payload: &[u8]) -> (Lsn, Lsn) {
            let start = self.at;
            let mut encoded = Vec::new();
            WalRecord {
                rec_lsn: Lsn(start),
                xid,
                rmgr: 10,
                info: 0,
                payload,
            }
            .encode(&mut encoded);
            self.raw(&encoded);
            (Lsn(start), Lsn(self.at))
        }

        pub fn position(&self) -> Lsn {
            Lsn(self.at)
        }

        /// The page starting at `page_lsn`, mutably, for tests that need to break
        /// one.
        pub fn page_mut(&mut self, page_lsn: Lsn) -> &mut [u8] {
            let off = (page_lsn.0 - self.base) as usize;
            let end = off + XLOG_BLCKSZ as usize;
            if self.bytes.len() < end {
                self.bytes.resize(end, 0);
            }
            &mut self.bytes[off..end]
        }

        /// Write every page out, creating the segments it spans.
        pub fn finish(&self, dir: &Path) -> Result<(), WalError> {
            let mut segs = Segments::new(dir);
            let pages = self.bytes.len().div_ceil(XLOG_BLCKSZ as usize);
            let mut padded = self.bytes.clone();
            padded.resize(pages * XLOG_BLCKSZ as usize, 0);
            for page in 0..pages {
                let lsn = self.base + page as u64 * XLOG_BLCKSZ;
                let from = page * XLOG_BLCKSZ as usize;
                segs.write_at(
                    segno_of(lsn),
                    seg_offset(lsn),
                    &padded[from..from + XLOG_BLCKSZ as usize],
                )?;
            }
            Ok(())
        }
    }

    /// Every record the reader hands back, plus the exhausted reader so a test
    /// can ask *why* the walk ended.
    pub type ReadAll = (Vec<(Xid, Vec<u8>)>, WalReader);

    /// Decode everything the reader will hand back from `from`.
    pub fn read_all(dir: &Path, from: Lsn) -> Result<ReadAll, WalError> {
        let mut reader = WalReader::open(dir, from)?;
        let mut out = Vec::new();
        let mut buf = Vec::new();
        while let Some((rec, _)) = reader.next_into(&mut buf)? {
            out.push((rec.xid, rec.payload.to_vec()));
        }
        Ok((out, reader))
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::{Builder, read_all};
    use super::*;
    use crate::page::{XLP_FIRST_IS_CONTRECORD, XLP_USABLE};
    use crate::segment::{WAL_SEG_SIZE, segment_path};
    use crabgresql_txn::Xid;

    #[test]
    fn records_on_one_page_round_trip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut b = Builder::at(Lsn::START);
        b.record(Xid(3), b"first");
        b.record(Xid(4), b"second");
        let end = b.position();
        b.finish(dir.path())?;

        let (records, reader) = read_all(dir.path(), Lsn::START)?;
        assert_eq!(
            records,
            vec![(Xid(3), b"first".to_vec()), (Xid(4), b"second".to_vec())]
        );
        assert_eq!(reader.position(), end);
        assert_eq!(reader.stop_reason(), Some(StopReason::Padding { at: end }));

        Ok(())
    }

    /// The padding a flush leaves after the last record is not damage: it is how
    /// the log ends now that the file's length cannot say so.
    #[test]
    fn padding_after_the_last_record_ends_the_log_cleanly() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut b = Builder::at(Lsn::START);
        let (_, end) = b.record(Xid(3), b"only");
        b.finish(dir.path())?;
        // The page is written in full, so everything past `end` is zeros.
        let (records, reader) = read_all(dir.path(), Lsn::START)?;
        assert_eq!(records.len(), 1);
        assert_eq!(reader.position(), end);
        assert_eq!(reader.stop_reason(), Some(StopReason::Padding { at: end }));

        Ok(())
    }

    #[test]
    fn a_record_spanning_a_page_boundary_round_trips() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut b = Builder::at(Lsn::START);
        // Land the next record 40 bytes short of the page edge.
        let filler = XLP_USABLE as usize - 40 - WalRecord::MIN_LEN;
        b.record(Xid(1), &vec![0xAA; filler]);
        let (start, end) = b.record(Xid(2), &[0xBB; 200]);
        b.finish(dir.path())?;

        assert_eq!(
            end.0 - start.0,
            (WalRecord::MIN_LEN + 200) as u64 + XLP_PAGE_HEADER_SIZE,
            "the range must widen by exactly the header it crossed"
        );
        let (records, _) = read_all(dir.path(), Lsn::START)?;
        assert_eq!(records[1], (Xid(2), vec![0xBB; 200]));

        Ok(())
    }

    /// The case most easily got wrong: the next page must claim no continuation.
    #[test]
    fn a_record_ending_at_the_page_edge_leaves_the_next_page_uncontinued() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut b = Builder::at(Lsn::START);
        let payload = XLP_USABLE as usize - WalRecord::MIN_LEN;
        let (_, end) = b.record(Xid(1), &vec![0xCC; payload]);
        b.record(Xid(2), b"next");
        let page = Lsn(page_start(Lsn::START.0) + XLOG_BLCKSZ);
        assert_eq!(end, Lsn(first_usable(page.0)));
        let header = PageHeader::decode(b.page_mut(page))
            .ok_or_else(|| anyhow::anyhow!("page header did not decode"))?;
        assert_eq!(header.rem_len, 0);
        assert!(!header.is_contrecord());
        b.finish(dir.path())?;

        let (records, _) = read_all(dir.path(), Lsn::START)?;
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].0, Xid(2));

        Ok(())
    }

    #[test]
    fn a_record_larger_than_a_page_round_trips() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut b = Builder::at(Lsn::START);
        let payload = vec![0x5A; (3 * XLP_USABLE + XLP_USABLE / 2) as usize];
        let (start, end) = b.record(Xid(9), &payload);
        b.finish(dir.path())?;
        assert_eq!(
            end.0 - start.0,
            (WalRecord::MIN_LEN + payload.len()) as u64 + 3 * XLP_PAGE_HEADER_SIZE
        );

        // A strictly decreasing chain of `rem_len` across the continuation pages.
        let mut previous = u32::MAX;
        for page in 1..=3u64 {
            let lsn = Lsn(page_start(start.0) + page * XLOG_BLCKSZ);
            let header = PageHeader::decode(b.page_mut(lsn))
                .ok_or_else(|| anyhow::anyhow!("page {page} header did not decode"))?;
            assert!(header.is_contrecord(), "page {page} must claim the record");
            assert!(header.rem_len < previous, "rem_len must decrease");
            previous = header.rem_len;
        }

        let (records, _) = read_all(dir.path(), Lsn::START)?;
        assert_eq!(records, vec![(Xid(9), payload)]);

        Ok(())
    }

    #[test]
    fn a_record_spanning_a_segment_boundary_round_trips() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // Start on the last page of segment 1.
        let last_page = 2 * WAL_SEG_SIZE - XLOG_BLCKSZ;
        let mut b = Builder::at(Lsn(first_usable(last_page)));
        let filler = XLP_USABLE as usize - 40 - WalRecord::MIN_LEN;
        b.record(Xid(1), &vec![0xAA; filler]);
        let (_, end) = b.record(Xid(2), &[0xBB; 100]);
        b.finish(dir.path())?;

        assert!(segment_path(dir.path(), 1).exists());
        assert!(segment_path(dir.path(), 2).exists());
        assert_eq!(segno_of(end.0), 2, "the record must land in the next segment");
        let (records, _) = read_all(dir.path(), Lsn(first_usable(last_page)))?;
        assert_eq!(records[1], (Xid(2), vec![0xBB; 100]));

        Ok(())
    }

    /// The marquee rule. Every page and every record in a segment renamed forward
    /// still passes its own checksum; only `pageaddr` disagrees.
    #[test]
    fn a_recycled_segment_is_not_mistaken_for_live_log() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut b = Builder::at(Lsn::START);
        // Fill segment 1 and spill into segment 2.
        while b.position().0 < 2 * WAL_SEG_SIZE {
            b.record(Xid(7), &vec![0xEE; 4000]);
        }
        b.finish(dir.path())?;
        let (before, _) = read_all(dir.path(), Lsn::START)?;

        // Recycle segment 1 forward to 5, contents untouched — exactly what
        // `Segments::recycle_below` does.
        std::fs::rename(segment_path(dir.path(), 1), segment_path(dir.path(), 5))?;
        let (after, reader) = read_all(dir.path(), Lsn(first_usable(segment_start(2))))?;

        // The walk over segment 2 must not run on into segment 5's stale pages.
        assert!(reader.position().0 < segment_start(5));
        assert!(after.len() < before.len());
        assert!(matches!(
            reader.stop_reason(),
            Some(StopReason::NoPage { .. }) | Some(StopReason::TornRecord { .. })
        ));

        // Name the field doing the work, so this cannot pass for an incidental
        // reason: page 0 of segment 5 records segment 1's address.
        let mut page = vec![0u8; XLOG_BLCKSZ as usize];
        let mut segs = Segments::new(dir.path());
        assert!(segs.read_at(5, 0, &mut page)?);
        let header = PageHeader::decode(&page)
            .ok_or_else(|| anyhow::anyhow!("the recycled page header is intact, as intended"))?;
        assert_eq!(header.pageaddr, segment_start(1));
        assert_ne!(header.pageaddr, segment_start(5));

        Ok(())
    }

    /// Reading directly at a recycled segment — which a redo point inside it
    /// would do — must report no page rather than replay a previous cycle.
    #[test]
    fn a_recycled_page_is_refused_at_its_new_address() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut b = Builder::at(Lsn::START);
        b.record(Xid(7), b"stale");
        b.finish(dir.path())?;
        std::fs::rename(segment_path(dir.path(), 1), segment_path(dir.path(), 9))?;

        let at = Lsn(first_usable(segment_start(9)));
        let (records, reader) = read_all(dir.path(), at)?;
        assert!(records.is_empty());
        assert_eq!(
            reader.stop_reason(),
            Some(StopReason::NoPage {
                at: Lsn(segment_start(9))
            })
        );

        Ok(())
    }

    #[test]
    fn a_page_with_a_bad_magic_or_crc_ends_the_log() -> anyhow::Result<()> {
        for (label, byte) in [("magic", 0usize), ("crc", 20usize)] {
            let dir = tempfile::tempdir()?;
            let mut b = Builder::at(Lsn::START);
            b.record(Xid(1), &vec![0xAA; XLP_USABLE as usize - WalRecord::MIN_LEN]);
            let (second, _) = b.record(Xid(2), b"on page one");
            let page = Lsn(page_start(second.0));
            b.page_mut(page)[byte] ^= 0xFF;
            b.finish(dir.path())?;

            let (records, reader) = read_all(dir.path(), Lsn::START)?;
            assert_eq!(records.len(), 1, "{label}: the first record still decodes");
            assert_eq!(reader.stop_reason(), Some(StopReason::NoPage { at: page }));
        }

        Ok(())
    }

    #[test]
    fn a_page_that_does_not_claim_a_continuation_ends_the_log() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut b = Builder::at(Lsn::START);
        let filler = XLP_USABLE as usize - 40 - WalRecord::MIN_LEN;
        b.record(Xid(1), &vec![0xAA; filler]);
        let (start, _) = b.record(Xid(2), &[0xBB; 200]);
        let page = Lsn(page_start(Lsn::START.0) + XLOG_BLCKSZ);
        // Clear the flag and re-checksum, so the page is otherwise impeccable.
        let mut header = PageHeader::decode(b.page_mut(page))
            .ok_or_else(|| anyhow::anyhow!("header did not decode"))?;
        header.info &= !XLP_FIRST_IS_CONTRECORD;
        header.encode(b.page_mut(page));
        b.finish(dir.path())?;

        let (records, reader) = read_all(dir.path(), Lsn::START)?;
        assert_eq!(records.len(), 1);
        assert_eq!(
            reader.stop_reason(),
            Some(StopReason::BrokenContrecord { at: start, page })
        );

        Ok(())
    }

    #[test]
    fn a_page_whose_rem_len_lies_ends_the_log() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut b = Builder::at(Lsn::START);
        let filler = XLP_USABLE as usize - 40 - WalRecord::MIN_LEN;
        b.record(Xid(1), &vec![0xAA; filler]);
        let (start, _) = b.record(Xid(2), &[0xBB; 200]);
        let page = Lsn(page_start(Lsn::START.0) + XLOG_BLCKSZ);
        let mut header = PageHeader::decode(b.page_mut(page))
            .ok_or_else(|| anyhow::anyhow!("header did not decode"))?;
        header.rem_len += 1;
        header.encode(b.page_mut(page));
        b.finish(dir.path())?;

        let (records, reader) = read_all(dir.path(), Lsn::START)?;
        assert_eq!(records.len(), 1);
        assert_eq!(
            reader.stop_reason(),
            Some(StopReason::BrokenContrecord { at: start, page })
        );

        Ok(())
    }

    #[test]
    fn a_torn_record_on_a_valid_page_ends_the_log() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut b = Builder::at(Lsn::START);
        b.record(Xid(1), b"good");
        let (start, _) = b.record(Xid(2), b"about to be corrupted");
        let page = Lsn(page_start(start.0));
        let off = page_offset(start.0) as usize;
        b.page_mut(page)[off + WalRecord::HEADER_LEN] ^= 0xFF;
        b.finish(dir.path())?;

        let (records, reader) = read_all(dir.path(), Lsn::START)?;
        assert_eq!(records.len(), 1);
        assert_eq!(reader.stop_reason(), Some(StopReason::TornRecord { at: start }));

        Ok(())
    }

    #[test]
    fn a_record_found_where_it_does_not_belong_ends_the_log() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut b = Builder::at(Lsn::START);
        // A record whose `rec_lsn` names somewhere else entirely, but which is
        // otherwise perfectly encoded.
        let mut encoded = Vec::new();
        WalRecord {
            rec_lsn: Lsn(Lsn::START.0 + 4096),
            xid: Xid(3),
            rmgr: 10,
            info: 0,
            payload: b"misplaced",
        }
        .encode(&mut encoded);
        b.raw(&encoded);
        b.finish(dir.path())?;

        let (records, reader) = read_all(dir.path(), Lsn::START)?;
        assert!(records.is_empty());
        assert_eq!(
            reader.stop_reason(),
            Some(StopReason::Misplaced {
                at: Lsn::START,
                claims: Lsn(Lsn::START.0 + 4096)
            })
        );

        Ok(())
    }

    /// After a break the walk must not skip ahead, even to a page that would
    /// validate: that is exactly how a recycled segment gets replayed.
    #[test]
    fn the_walk_stops_at_the_first_break_even_with_valid_pages_above() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut b = Builder::at(Lsn::START);
        let whole_page = XLP_USABLE as usize - WalRecord::MIN_LEN;
        b.record(Xid(1), &vec![0xAA; whole_page]);
        b.record(Xid(2), &vec![0xBB; whole_page]);
        b.record(Xid(3), b"third page");
        // Break the middle page only; the third is untouched and impeccable.
        let broken = Lsn(page_start(Lsn::START.0) + XLOG_BLCKSZ);
        b.page_mut(broken)[8] ^= 0xFF;
        b.finish(dir.path())?;

        let (records, reader) = read_all(dir.path(), Lsn::START)?;
        assert_eq!(records.len(), 1, "the walk resynchronized past a break");
        assert_eq!(reader.stop_reason(), Some(StopReason::NoPage { at: broken }));

        Ok(())
    }

    #[test]
    fn an_empty_directory_reads_as_a_log_that_starts_at_the_beginning() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(end_of_wal(dir.path(), Lsn::INVALID)?, Lsn::START);
        // And nothing was created by looking.
        assert_eq!(segment_bounds(dir.path())?, None);

        Ok(())
    }

    /// A whole-stream replay starts at the lowest surviving segment, not at byte
    /// zero: recycling removes the prefix.
    #[test]
    fn an_invalid_start_resolves_to_the_lowest_segment() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let at = Lsn(first_usable(segment_start(4)));
        let mut b = Builder::at(at);
        let (_, end) = b.record(Xid(3), b"only");
        b.finish(dir.path())?;
        assert_eq!(end_of_wal(dir.path(), Lsn::INVALID)?, end);

        Ok(())
    }

    #[test]
    fn a_missing_segment_ends_the_log() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut b = Builder::at(Lsn::START);
        b.record(Xid(1), b"one");
        b.finish(dir.path())?;
        let at = Lsn(first_usable(segment_start(3)));
        let (records, reader) = read_all(dir.path(), at)?;
        assert!(records.is_empty());
        assert_eq!(
            reader.stop_reason(),
            Some(StopReason::NoPage {
                at: Lsn(segment_start(3))
            })
        );

        Ok(())
    }
}
