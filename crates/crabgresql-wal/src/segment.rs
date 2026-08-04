//! WAL segment files: fixed-size, preallocated, and recycled rather than deleted.
//!
//! The log is cut into [`WAL_SEG_SIZE`] pieces named by their segment number, so
//! an [`crate::Lsn`] resolves to a file and an offset by division. Segments are
//! **preallocated with real zeros** — not `ftruncate`d — for two reasons: a later
//! WAL write cannot then fail with `ENOSPC` halfway through a flush, and no
//! metadata update rides along with the `sync_data` that flush issues.
//!
//! [`WAL_SEG_SIZE`] is a multiple of [`XLOG_BLCKSZ`], so a page never straddles
//! two files. That is what keeps every write simultaneously page-aligned within
//! its file and a whole number of pages long.
//!
//! ## Why recycling is safe
//!
//! A segment below the published redo point is renamed *forward* to a future
//! segment number with its contents untouched — that is the entire point, since
//! rewriting 16 MiB of zeros is what recycling exists to avoid. Every page inside
//! it still passes its own header CRC, and every record inside it still passes
//! its own record CRC. What stops recovery reading them is
//! [`crate::PageHeader::pageaddr`]: a page carries the LSN of the position it was
//! *written at*, which after a forward rename is strictly below the position it
//! now occupies. The reader checks that on every page, so the first page it
//! touches in a recycled file ends the log.
//!
//! The invariant that argument rests on is that a rename never *lowers* a segment
//! number. [`Segments::recycle_below`] asserts it.

// Lands ahead of its consumer, like `aligned`: the writer and the reader are
// converted in later commits.
#![allow(dead_code)]

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::aligned::AlignedBuf;
use crate::page::XLOG_BLCKSZ;
use crate::record::WalError;

/// Bytes per segment file. PostgreSQL's default, and 2048 pages.
pub const WAL_SEG_SIZE: u64 = 16 * 1024 * 1024;

const WAL_SUBDIR: &str = "pg_wal";

/// How many unused segments ahead of the write position to keep preallocated by
/// recycling into them. Anything beyond this is unlinked instead: holding more
/// costs disk for a write rate that is not happening.
const WAL_RECYCLE_TARGET: u64 = 4;

/// Chunk the zero-fill of a new segment is written in. One `AlignedBuf` of this
/// size is built per creation and reused across its writes.
const PREALLOC_CHUNK: u64 = 1024 * 1024;

/// How many segment files to keep open. Small: the writer touches one, or two
/// across a boundary, and the reader walks forward.
const OPEN_CACHE: usize = 4;

const _: () = assert!(WAL_SEG_SIZE % XLOG_BLCKSZ == 0);

pub fn wal_dir(dir: &Path) -> PathBuf {
    dir.join(WAL_SUBDIR)
}

/// The segment a byte position falls in.
pub fn segno_of(lsn: u64) -> u64 {
    lsn / WAL_SEG_SIZE
}

/// How far into its segment file a byte position sits.
pub fn seg_offset(lsn: u64) -> u64 {
    lsn % WAL_SEG_SIZE
}

/// The LSN of a segment's first byte.
pub fn segment_start(segno: u64) -> u64 {
    segno * WAL_SEG_SIZE
}

/// Fixed-width upper-case hex, so a lexicographic directory listing is also
/// numeric order.
pub fn segment_name(segno: u64) -> String {
    format!("{segno:016X}")
}

pub fn segment_path(dir: &Path, segno: u64) -> PathBuf {
    wal_dir(dir).join(segment_name(segno))
}

/// The segment number a file name denotes, or `None` for anything that is not
/// one — a stray temp file, the old single-file log, a `.` entry.
pub fn parse_segment_name(name: &str) -> Option<u64> {
    if name.len() != 16 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(name, 16).ok()
}

/// The lowest and highest segment numbers present, or `None` when there are
/// none. The low end matters because recycling removes the prefix: LSN 0 is not
/// generally reachable, so a whole-stream replay starts at the lowest survivor.
pub fn segment_bounds(dir: &Path) -> Result<Option<(u64, u64)>, WalError> {
    let entries = match std::fs::read_dir(wal_dir(dir)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut bounds: Option<(u64, u64)> = None;
    for entry in entries {
        let entry = entry?;
        let Some(segno) = entry.file_name().to_str().and_then(parse_segment_name) else {
            continue;
        };
        bounds = Some(match bounds {
            None => (segno, segno),
            Some((lo, hi)) => (lo.min(segno), hi.max(segno)),
        });
    }
    Ok(bounds)
}

/// A record of every positioned write, kept only under `cfg(test)`.
///
/// The alignment guarantee has no observable symptom — an unaligned write is
/// merely slower and closes the door on direct I/O — so it is asserted here or
/// nowhere.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteRecord {
    pub segno: u64,
    pub offset: u64,
    pub len: usize,
    pub buf_aligned: bool,
}

/// Open segment handles plus the creation and recycling policy.
pub struct Segments {
    dir: PathBuf,
    /// MRU-first, capped at [`OPEN_CACHE`].
    open: Vec<(u64, File)>,
    #[cfg(test)]
    pub writes: Vec<WriteRecord>,
    #[cfg(test)]
    pub dir_syncs: u64,
}

impl Segments {
    pub fn new(dir: &Path) -> Segments {
        Segments {
            dir: dir.to_path_buf(),
            open: Vec::new(),
            #[cfg(test)]
            writes: Vec::new(),
            #[cfg(test)]
            dir_syncs: 0,
        }
    }

    fn cached(&mut self, segno: u64) -> Option<&File> {
        let at = self.open.iter().position(|(n, _)| *n == segno)?;
        if at != 0 {
            self.open[..=at].rotate_right(1);
        }
        Some(&self.open[0].1)
    }

    fn admit(&mut self, segno: u64, file: File) -> &File {
        self.open.insert(0, (segno, file));
        self.open.truncate(OPEN_CACHE);
        &self.open[0].1
    }

    fn forget(&mut self, segno: u64) {
        self.open.retain(|(n, _)| *n != segno);
    }

    /// An existing segment, or `None`. **Never creates**: a reader that created a
    /// segment would manufacture exactly the zeros it is about to interpret as
    /// the end of the log.
    fn open_existing(&mut self, segno: u64) -> Result<Option<&File>, WalError> {
        if self.open.iter().any(|(n, _)| *n == segno) {
            return Ok(self.cached(segno));
        }
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(segment_path(&self.dir, segno))
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(Some(self.admit(segno, file)))
    }

    /// An existing segment, or a freshly preallocated one.
    fn open_for_write(&mut self, segno: u64) -> Result<&File, WalError> {
        if self.open_existing(segno)?.is_none() {
            self.create(segno)?;
        }
        match self.open_existing(segno)? {
            Some(file) => Ok(file),
            // `create` renamed a full-size file into place and returned Ok, so
            // this is a concurrent removal of the WAL directory under a running
            // server, not a condition to paper over.
            None => Err(WalError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("wal segment {} vanished after creation", segment_name(segno)),
            ))),
        }
    }

    /// Preallocate a segment and publish it under its real name.
    ///
    /// The zeros go to a temp name first and are renamed into place, so "the file
    /// exists" and "the file is a full [`WAL_SEG_SIZE`] of known content" are the
    /// same statement — a crash partway through preallocation leaves no file a
    /// reader would trust.
    fn create(&mut self, segno: u64) -> Result<(), WalError> {
        let dir = wal_dir(&self.dir);
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!("xlogtemp.{}", segment_name(segno)));
        {
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            let mut zeros = AlignedBuf::with_pages((PREALLOC_CHUNK / XLOG_BLCKSZ) as usize);
            zeros.extend_from_slice(&vec![0u8; PREALLOC_CHUNK as usize]);
            let mut at = 0u64;
            while at < WAL_SEG_SIZE {
                let take = PREALLOC_CHUNK.min(WAL_SEG_SIZE - at) as usize;
                file.write_all_at(&zeros.as_slice()[..take], at)?;
                at += take as u64;
            }
            // `sync_all`, not `sync_data`: the file is new, so its size and its
            // existence have to be durable too, not just its contents.
            file.sync_all()?;
        }
        std::fs::rename(&tmp, segment_path(&self.dir, segno))?;
        self.sync_dir()
    }

    /// Make the directory's own entries durable.
    ///
    /// Without this a crash can leave a segment's blocks allocated but the name
    /// binding them absent, while `pg_control` already published a redo point
    /// inside it — and recovery hard-errors on a missing segment below the redo
    /// point, so a cluster with nothing actually wrong with it refuses to start.
    fn sync_dir(&mut self) -> Result<(), WalError> {
        #[cfg(test)]
        {
            self.dir_syncs += 1;
        }
        Ok(crate::fsutil::sync_dir(&wal_dir(&self.dir))?)
    }

    /// Write whole pages at a page-aligned offset within `segno`, and make them
    /// durable.
    pub fn write_at(&mut self, segno: u64, offset: u64, bytes: &[u8]) -> Result<(), WalError> {
        debug_assert_eq!(offset % XLOG_BLCKSZ, 0, "unaligned WAL write offset");
        debug_assert_eq!(
            bytes.len() as u64 % XLOG_BLCKSZ,
            0,
            "WAL write is not a whole number of pages"
        );
        debug_assert!(offset + bytes.len() as u64 <= WAL_SEG_SIZE);
        #[cfg(test)]
        self.writes.push(WriteRecord {
            segno,
            offset,
            len: bytes.len(),
            buf_aligned: bytes.as_ptr() as usize % crate::aligned::ALIGN == 0,
        });
        let file = self.open_for_write(segno)?;
        file.write_all_at(bytes, offset)?;
        file.sync_data()?;
        Ok(())
    }

    /// Fill `buf` from `offset` within `segno`. `Ok(false)` when the segment does
    /// not exist or is too short — both of which the reader treats as "no page
    /// here", i.e. the end of the log.
    pub fn read_at(&mut self, segno: u64, offset: u64, buf: &mut [u8]) -> Result<bool, WalError> {
        let Some(file) = self.open_existing(segno)? else {
            return Ok(false);
        };
        match file.read_exact_at(buf, offset) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Reclaim the segments strictly below `keep_from`, renaming the first few
    /// forward past `highest` so rotation does not pay a 16 MiB zero-fill.
    ///
    /// Only ever called with a `keep_from` derived from a redo point that is
    /// already durable in `pg_control`; reclaiming ahead of that publication
    /// would leave a control file naming a segment that is gone.
    pub fn recycle_below(&mut self, keep_from: u64, highest: u64) -> Result<(), WalError> {
        let Some((lo, _)) = segment_bounds(&self.dir)? else {
            return Ok(());
        };
        let mut next_free = highest + 1;
        let mut spare = 0u64;
        let mut touched = false;
        for segno in lo..keep_from {
            let from = segment_path(&self.dir, segno);
            if !from.exists() {
                continue;
            }
            self.forget(segno);
            if spare < WAL_RECYCLE_TARGET {
                // The invariant the whole scheme rests on: a page's recorded
                // address must stay strictly below the position it occupies, or
                // stale records would validate where they lie.
                assert!(
                    next_free > segno,
                    "recycling a WAL segment backwards ({segno} -> {next_free}) would make \
                     its stale pages validate at their new address"
                );
                self.forget(next_free);
                std::fs::rename(&from, segment_path(&self.dir, next_free))?;
                next_free += 1;
                spare += 1;
            } else {
                std::fs::remove_file(&from)?;
            }
            touched = true;
        }
        if touched {
            self.sync_dir()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_names_round_trip_and_sort_numerically() -> anyhow::Result<()> {
        for segno in [0u64, 1, 0xFF, 0x1234_5678, u64::MAX] {
            let name = segment_name(segno);
            assert_eq!(name.len(), 16);
            assert_eq!(parse_segment_name(&name), Some(segno));
        }
        let mut names = [1u64, 16, 2, 255].map(segment_name);
        names.sort();
        assert_eq!(
            names.map(|n| parse_segment_name(&n)),
            [Some(1), Some(2), Some(16), Some(255)],
            "lexicographic order must also be numeric order"
        );

        Ok(())
    }

    /// Anything that is not sixteen hex digits is not a segment — in particular
    /// the temp name preallocation uses and the pre-paging single-file log.
    #[test]
    fn non_segment_names_are_rejected() {
        for name in [
            "",
            "wal",
            "0000000000000001.tmp",
            "xlogtemp.0000000000000001",
            "000000000000001",
            "00000000000000010",
            "00000000000000GG",
        ] {
            assert_eq!(parse_segment_name(name), None, "{name} parsed as a segment");
        }
    }

    #[test]
    fn lsn_resolves_to_a_segment_and_an_offset() {
        assert_eq!(segno_of(0), 0);
        assert_eq!(segno_of(WAL_SEG_SIZE - 1), 0);
        assert_eq!(segno_of(WAL_SEG_SIZE), 1);
        assert_eq!(seg_offset(WAL_SEG_SIZE + 24), 24);
        assert_eq!(segment_start(3), 3 * WAL_SEG_SIZE);
    }

    #[test]
    fn a_created_segment_is_full_size_and_entirely_zero() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut segs = Segments::new(dir.path());
        segs.write_at(2, 0, &vec![0u8; XLOG_BLCKSZ as usize])?;
        let meta = std::fs::metadata(segment_path(dir.path(), 2))?;
        assert_eq!(meta.len(), WAL_SEG_SIZE);
        let bytes = std::fs::read(segment_path(dir.path(), 2))?;
        assert!(bytes.iter().all(|&b| b == 0));
        // The temp file is renamed, never left behind.
        assert_eq!(segment_bounds(dir.path())?, Some((2, 2)));

        Ok(())
    }

    #[test]
    fn writes_and_reads_round_trip_within_a_segment() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut segs = Segments::new(dir.path());
        let page = vec![0xA5u8; XLOG_BLCKSZ as usize];
        segs.write_at(0, 3 * XLOG_BLCKSZ, &page)?;
        let mut back = vec![0u8; XLOG_BLCKSZ as usize];
        assert!(segs.read_at(0, 3 * XLOG_BLCKSZ, &mut back)?);
        assert_eq!(back, page);
        // The page before it is still the preallocated zeros.
        assert!(segs.read_at(0, 2 * XLOG_BLCKSZ, &mut back)?);
        assert!(back.iter().all(|&b| b == 0));

        Ok(())
    }

    /// A reader must never bring a segment into existence: the zeros it would
    /// create are exactly what it reads as the end of the log.
    #[test]
    fn reading_a_missing_segment_creates_nothing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(wal_dir(dir.path()))?;
        let mut segs = Segments::new(dir.path());
        let mut buf = vec![0u8; XLOG_BLCKSZ as usize];
        assert!(!segs.read_at(7, 0, &mut buf)?);
        assert_eq!(segment_bounds(dir.path())?, None);

        Ok(())
    }

    #[test]
    fn every_write_is_recorded_aligned_and_page_granular() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut segs = Segments::new(dir.path());
        let mut buf = AlignedBuf::with_pages(2);
        buf.extend_from_slice(&[1u8; 100]);
        segs.write_at(0, 0, buf.whole_pages())?;
        segs.write_at(0, XLOG_BLCKSZ, buf.whole_pages())?;
        assert_eq!(segs.writes.len(), 2);
        for write in &segs.writes {
            assert_eq!(write.offset % XLOG_BLCKSZ, 0);
            assert_eq!(write.len as u64 % XLOG_BLCKSZ, 0);
            assert!(write.buf_aligned, "the write buffer was not 4K-aligned");
        }

        Ok(())
    }

    #[test]
    fn creating_a_segment_fsyncs_the_directory() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut segs = Segments::new(dir.path());
        let page = vec![0u8; XLOG_BLCKSZ as usize];
        segs.write_at(0, 0, &page)?;
        assert_eq!(segs.dir_syncs, 1);
        // A second write into the same segment creates nothing, so it syncs
        // nothing: the cost is per segment, not per flush.
        segs.write_at(0, XLOG_BLCKSZ, &page)?;
        assert_eq!(segs.dir_syncs, 1);
        segs.write_at(1, 0, &page)?;
        assert_eq!(segs.dir_syncs, 2);

        Ok(())
    }

    #[test]
    fn recycling_renames_forward_and_then_unlinks() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut segs = Segments::new(dir.path());
        let page = vec![0u8; XLOG_BLCKSZ as usize];
        for segno in 0..8 {
            segs.write_at(segno, 0, &page)?;
        }
        let syncs_before = segs.dir_syncs;
        // Keep 6 and 7; the four eligible below become 8..11, the rest go.
        segs.recycle_below(6, 7)?;
        assert_eq!(segment_bounds(dir.path())?, Some((6, 11)));
        for segno in 0..6 {
            assert!(!segment_path(dir.path(), segno).exists(), "{segno} survived");
        }
        assert_eq!(segs.dir_syncs, syncs_before + 1, "one directory fsync, not one per file");

        Ok(())
    }

    /// Renaming a segment to a *lower* number would leave its pages recording an
    /// address at or above where they now sit, so stale records would validate.
    /// Nothing in the caller can produce that today; the assertion is what keeps
    /// it that way.
    #[test]
    #[should_panic(expected = "backwards")]
    fn recycling_never_lowers_a_segment_number() {
        let dir = match tempfile::tempdir() {
            Ok(dir) => dir,
            Err(error) => panic!("tempdir: {error}"),
        };
        let mut segs = Segments::new(dir.path());
        let page = vec![0u8; XLOG_BLCKSZ as usize];
        for segno in 5..8 {
            if let Err(error) = segs.write_at(segno, 0, &page) {
                panic!("write: {error}");
            }
        }
        // `highest` below the segments being reclaimed is the caller bug this
        // catches.
        let _ = segs.recycle_below(7, 3);
    }

    #[test]
    fn recycling_an_empty_directory_is_a_no_op() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut segs = Segments::new(dir.path());
        segs.recycle_below(10, 20)?;
        assert_eq!(segs.dir_syncs, 0);

        Ok(())
    }

    /// The MRU cache must not hand back a stale handle for a segment that was
    /// renamed away underneath it.
    #[test]
    fn recycling_drops_the_cached_handles() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut segs = Segments::new(dir.path());
        let mut page = vec![0u8; XLOG_BLCKSZ as usize];
        page[0] = 0xEE;
        segs.write_at(0, 0, &page)?;
        segs.write_at(1, 0, &page)?;
        segs.recycle_below(1, 1)?;
        // Segment 0 became segment 2; reading 0 must now report "no segment"
        // rather than serving the handle still open on the renamed inode.
        let mut back = vec![0u8; XLOG_BLCKSZ as usize];
        assert!(!segs.read_at(0, 0, &mut back)?);
        assert!(segs.read_at(2, 0, &mut back)?);
        assert_eq!(back[0], 0xEE);

        Ok(())
    }
}
