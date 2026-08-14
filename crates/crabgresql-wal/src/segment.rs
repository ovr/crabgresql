//! The WAL's on-disk layout: one logical byte stream cut into fixed-size
//! segment files.
//!
//! An [`Lsn`] is still a position in the *stream* — that invariant is what the
//! whole crate is built on, and nothing here weakens it. What segmentation gives
//! up is the weaker property that an LSN is also an offset in a single file:
//! the byte at LSN `l` now lives in segment [`segment_of`]`(l)` at offset
//! [`segment_offset`]`(l)`.
//!
//! Segments are named after their number in PostgreSQL's style — 24 uppercase
//! hex digits — so `pg_wal` looks familiar and a stray file (an editor backup, a
//! WAL from before segmentation) is excluded by the name alone rather than by a
//! deny-list.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::fsutil::sync_dir;
use crate::record::{Lsn, WalError};

/// Bytes per segment file. Fixed, not configurable: it is baked into every
/// LSN→file mapping on disk, so a cluster written with one value is unreadable
/// with another, and a knob that silently corrupts a data directory is worse
/// than no knob.
pub const SEGMENT_SIZE: u64 = 32 << 20;

/// Segment file names are exactly this many hex digits, matching PostgreSQL's
/// WAL file names.
const NAME_LEN: usize = 24;

const WAL_SUBDIR: &str = "pg_wal";

/// The directory holding the segment files: `<dir>/pg_wal`.
pub fn wal_dir(dir: &Path) -> PathBuf {
    dir.join(WAL_SUBDIR)
}

/// The path of one segment file.
pub fn wal_segment_path(dir: &Path, seg: u64) -> PathBuf {
    wal_dir(dir).join(format!("{seg:0NAME_LEN$X}"))
}

/// The first segment's path — where a test that writes a short log directly, or
/// corrupts one, has to reach. Named rather than spelled out at each call site so
/// that a layout change breaks compilation instead of turning a test into a
/// silent no-op on a path nothing reads.
#[cfg(test)]
pub(crate) fn wal_segment_path_0(dir: &Path) -> PathBuf {
    wal_segment_path(dir, 0)
}

/// Which segment holds the byte at `lsn`.
pub const fn segment_of(lsn: Lsn) -> u64 {
    lsn.0 / SEGMENT_SIZE
}

/// Where inside its segment the byte at `lsn` sits.
pub const fn segment_offset(lsn: Lsn) -> u64 {
    lsn.0 % SEGMENT_SIZE
}

/// Parse a segment number out of a file name, or `None` if the name is not one
/// of ours.
fn segment_number(name: &str) -> Option<u64> {
    if name.len() != NAME_LEN || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // 24 hex digits can name far more segments than a u64 can count; a value
    // that does not fit is not a segment we ever wrote.
    u64::from_str_radix(name, 16).ok()
}

/// Every segment number present under `dir`, ascending. An absent `pg_wal`
/// reads as no segments rather than an error — that is a cluster whose WAL has
/// not been created yet.
pub fn segment_numbers(dir: &Path) -> std::io::Result<Vec<u64>> {
    let entries = match std::fs::read_dir(wal_dir(dir)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut segments = Vec::new();
    for entry in entries {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str()
            && let Some(seg) = segment_number(name)
        {
            segments.push(seg);
        }
    }
    segments.sort_unstable();
    Ok(segments)
}

/// The end of the stream as it exists on disk: the last segment's number times
/// [`SEGMENT_SIZE`], plus that segment's length. `0` when there are no segments.
///
/// The segmented replacement for "the length of the WAL file", which is what
/// callers actually mean when they compare against an [`Lsn`].
pub fn wal_stream_len(dir: &Path) -> std::io::Result<u64> {
    let Some(last) = segment_numbers(dir)?.pop() else {
        return Ok(0);
    };
    let len = std::fs::metadata(wal_segment_path(dir, last))?.len();
    Ok(last * SEGMENT_SIZE + len)
}

/// The writer's end of the layout: holds the segment currently being appended
/// to and moves to the next one as the stream crosses a boundary.
///
/// Only ever used behind the WAL's writer mutex, so "the current segment" is
/// single-threaded state and the create-then-fsync sequences below cannot race.
pub struct SegmentWriter {
    dir: PathBuf,
    /// The segment `file` is open on.
    seg: u64,
    file: File,
}

impl SegmentWriter {
    /// Open (creating if absent) the writer positioned on segment `seg`.
    pub fn open(dir: &Path, seg: u64) -> Result<SegmentWriter, WalError> {
        std::fs::create_dir_all(wal_dir(dir))?;
        let file = open_segment(dir, seg)?;
        Ok(SegmentWriter {
            dir: dir.to_path_buf(),
            seg,
            file,
        })
    }

    /// The length of the segment currently open.
    pub fn len(&self) -> Result<u64, WalError> {
        Ok(self.file.metadata()?.len())
    }

    /// Move to `seg`, leaving the segment being abandoned fsynced first.
    ///
    /// That order is a durability guarantee recovery reads back: because a
    /// segment is made durable *before* the next one is created, a crash can
    /// never leave segment `N+1` on disk with `N` still short. Recovery can
    /// therefore treat a short non-final segment as corruption rather than as an
    /// ordinary crash artifact it would have to guess about.
    fn switch_to(&mut self, seg: u64) -> Result<(), WalError> {
        self.file.sync_data()?;
        self.file = open_segment(&self.dir, seg)?;
        self.seg = seg;
        Ok(())
    }

    /// Write `bytes`, which start at stream position `start`, into the segments
    /// they belong to and fsync every file touched.
    ///
    /// `durable` is advanced to the number of leading bytes that are on stable
    /// storage. On error it therefore names exactly the prefix the caller may
    /// count as flushed — a multi-segment write is several fsyncs, so "all or
    /// nothing" is no longer available and pretending otherwise would either
    /// lose the durable prefix or claim bytes that never landed.
    pub fn write_at(
        &mut self,
        bytes: &[u8],
        start: u64,
        durable: &mut usize,
    ) -> Result<(), WalError> {
        *durable = 0;
        let mut done = 0usize;
        while done < bytes.len() {
            let pos = start + done as u64;
            let seg = pos / SEGMENT_SIZE;
            let off = pos % SEGMENT_SIZE;
            if seg != self.seg {
                self.switch_to(seg)?;
                // Everything before this point lives in segments that are now
                // fsynced.
                *durable = done;
            }
            let room = (SEGMENT_SIZE - off) as usize;
            let n = room.min(bytes.len() - done);
            // Positioned write at the segment-relative offset, never the OS file
            // cursor: after a reopen the cursor is 0 but the segment continues
            // where the stream left off, and on a partial-write retry the cursor
            // is desynced — both would otherwise corrupt the stream.
            self.file.write_all_at(&bytes[done..done + n], off)?;
            done += n;
        }
        self.file.sync_data()?;
        *durable = done;
        Ok(())
    }

    /// Discard everything at or above `lsn`: truncate the segment holding it and
    /// unlink every segment above.
    ///
    /// Unlinking is not tidiness. `Wal::open` positions the stream after the
    /// highest segment present, so a leftover segment above the truncation point
    /// would put the insert position back above the discarded region, and the
    /// gap between them would be read on the next recovery as a hole in the log.
    pub fn reset_to(&mut self, lsn: Lsn) -> Result<(), WalError> {
        let seg = segment_of(lsn);
        for above in segment_numbers(&self.dir)?.into_iter().filter(|s| *s > seg) {
            match std::fs::remove_file(wal_segment_path(&self.dir, above)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        if seg != self.seg {
            self.file = open_segment(&self.dir, seg)?;
            self.seg = seg;
        }
        self.file.set_len(segment_offset(lsn))?;
        self.file.sync_data()?;
        // The unlinks above are directory changes, and a crash that lost one
        // would resurrect a segment past the truncation point.
        sync_dir(&wal_dir(&self.dir))?;
        Ok(())
    }
}

/// Open segment `seg`, creating it if absent. A newly created file is published
/// with a directory fsync, so a crash cannot leave the segment's contents
/// durable while its name is not.
fn open_segment(dir: &Path, seg: u64) -> Result<File, WalError> {
    let path = wal_segment_path(dir, seg);
    let existed = path.exists();
    // truncate(false): never discard an existing segment — we append to it.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    if !existed {
        sync_dir(&wal_dir(dir))?;
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_pg_style_and_round_trip() {
        let dir = Path::new("/tmp/pgdata");
        assert_eq!(
            wal_segment_path(dir, 0),
            Path::new("/tmp/pgdata/pg_wal/000000000000000000000000")
        );
        assert_eq!(
            wal_segment_path(dir, 0x2A),
            Path::new("/tmp/pgdata/pg_wal/00000000000000000000002A")
        );
        for seg in [0u64, 1, 0x2A, u64::MAX] {
            let name = format!("{seg:024X}");
            assert_eq!(segment_number(&name), Some(seg));
        }
    }

    /// The name filter is the only thing keeping a foreign file out of the
    /// stream — including `pg_wal/wal`, the single-file layout this replaced.
    #[test]
    fn foreign_names_are_not_segments() {
        for name in [
            "wal",
            "",
            "00000000000000000000002G",
            "0000000000000000000002A",
            "0000000000000000000000002A",
            "000000000000000000000000.tmp",
        ] {
            assert_eq!(segment_number(name), None, "{name} must not parse");
        }
    }

    #[test]
    fn lsn_maps_to_a_segment_and_an_offset() {
        assert_eq!(segment_of(Lsn(0)), 0);
        assert_eq!(segment_offset(Lsn(0)), 0);
        assert_eq!(segment_of(Lsn(SEGMENT_SIZE - 1)), 0);
        assert_eq!(segment_offset(Lsn(SEGMENT_SIZE - 1)), SEGMENT_SIZE - 1);
        // A boundary LSN names the first byte of the next segment, never the
        // one-past-the-end of the previous one.
        assert_eq!(segment_of(Lsn(SEGMENT_SIZE)), 1);
        assert_eq!(segment_offset(Lsn(SEGMENT_SIZE)), 0);
        assert_eq!(segment_of(Lsn(SEGMENT_SIZE + 7)), 1);
        assert_eq!(segment_offset(Lsn(SEGMENT_SIZE + 7)), 7);
    }

    #[test]
    fn stream_length_spans_every_segment() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(wal_stream_len(dir.path())?, 0, "no pg_wal yet");

        let mut writer = SegmentWriter::open(dir.path(), 0)?;
        assert_eq!(wal_stream_len(dir.path())?, 0, "an empty segment 0");

        // A write that lands astride the boundary fills segment 0 and starts 1.
        let mut durable = 0usize;
        let bytes = vec![7u8; 100];
        writer.write_at(&bytes, SEGMENT_SIZE - 40, &mut durable)?;
        assert_eq!(durable, bytes.len());
        assert_eq!(wal_stream_len(dir.path())?, SEGMENT_SIZE + 60);
        assert_eq!(
            std::fs::metadata(wal_segment_path(dir.path(), 0))?.len(),
            SEGMENT_SIZE,
            "the segment left behind must be full"
        );

        Ok(())
    }

    #[test]
    fn reset_unlinks_the_segments_above() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut writer = SegmentWriter::open(dir.path(), 0)?;
        let mut durable = 0usize;
        writer.write_at(&[7u8; 100], SEGMENT_SIZE - 40, &mut durable)?;
        assert_eq!(segment_numbers(dir.path())?, vec![0, 1]);

        writer.reset_to(Lsn(SEGMENT_SIZE - 40))?;
        assert_eq!(segment_numbers(dir.path())?, vec![0]);
        assert_eq!(wal_stream_len(dir.path())?, SEGMENT_SIZE - 40);

        Ok(())
    }
}
