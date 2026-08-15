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
use std::path::{Path, PathBuf};

use crate::fsutil::{FileExt, sync_dir};
use crate::record::{Lsn, WalError};

/// Bytes per segment file. Fixed, not configurable: it is baked into every
/// LSN→file mapping on disk, so a cluster written with one value is unreadable
/// with another, and a knob that silently corrupts a data directory is worse
/// than no knob.
pub const SEGMENT_SIZE: u64 = 32 << 20;

const NAME_LEN: usize = 24;

const WAL_SUBDIR: &str = "pg_wal";

pub fn wal_dir(dir: &Path) -> PathBuf {
    dir.join(WAL_SUBDIR)
}

pub fn wal_segment_path(dir: &Path, seg: u64) -> PathBuf {
    wal_dir(dir).join(format!("{seg:0NAME_LEN$X}"))
}

/// Segment zero's path. Named, rather than spelled out at each call site, so a
/// layout change breaks compilation instead of leaving a test scribbling on a
/// path nothing reads.
#[cfg(test)]
pub(crate) fn wal_segment_path_0(dir: &Path) -> PathBuf {
    wal_segment_path(dir, 0)
}

pub const fn segment_of(lsn: Lsn) -> u64 {
    lsn.0 / SEGMENT_SIZE
}

pub const fn segment_offset(lsn: Lsn) -> u64 {
    lsn.0 % SEGMENT_SIZE
}

/// `None` when the name is not one of ours.
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

/// The end of the stream as it exists on disk, `0` when there are no segments.
/// The segmented replacement for "the length of the WAL file" — which is what a
/// caller comparing against an [`Lsn`] means.
pub fn wal_stream_len(dir: &Path) -> std::io::Result<u64> {
    let Some(last) = segment_numbers(dir)?.pop() else {
        return Ok(0);
    };
    let len = std::fs::metadata(wal_segment_path(dir, last))?.len();
    Ok(last * SEGMENT_SIZE + len)
}

/// Unlink every segment lying wholly below `redo`, holding `keep_bytes` of them
/// back, and return how many were removed.
///
/// This is what bounds `pg_wal`: [`crate::recover`] reads the stream from the
/// redo point upwards, so a segment entirely below it is never opened again, and
/// without this the log grows for the life of the cluster. `keep_bytes`
/// (`CRABGRESQL_WAL_KEEP_SIZE`) holds a tail of that dead history back for
/// forensics; nothing in the server reads it.
///
/// Three things about *when* and *in what order* this runs are load-bearing:
///
/// * The caller must have made `redo` durable in `pg_control` **first**. Removing
///   a segment while the control file still names a lower redo point leaves a
///   cluster that cannot start: recovery would resume at a segment that is gone.
///   That is why this takes an [`Lsn`] rather than reading the control file
///   itself — the ordering belongs to the checkpoint, and a function that could
///   read the redo point for itself invites being called before the publish.
/// * [`Lsn::INVALID`] removes nothing. It means "replay the whole stream" — a
///   checkpoint that could not bound itself — and every segment is still needed.
///   This is also what keeps a buffer table's rows safe, since their only durable
///   trace is a WAL record and their presence is exactly what clamps the redo
///   point to zero.
/// * Ascending order, so an interrupted pass leaves a contiguous suffix rather
///   than a hole. A crash between an unlink and the directory fsync can resurrect
///   a segment below the redo point, which is harmless — `read_from` only
///   requires contiguity from the segment it starts in upwards.
///
/// Takes no lock and needs none: the writer only ever creates segments at or
/// above the one it is filling, and `segment_of(redo)` can never exceed that.
pub fn remove_segments_below(dir: &Path, redo: Lsn, keep_bytes: u64) -> std::io::Result<usize> {
    if !redo.is_valid() {
        return Ok(0);
    }
    // The segment holding the redo point is still needed in full, so the floor is
    // below it, never at it.
    let floor = segment_of(redo).saturating_sub(keep_bytes.div_ceil(SEGMENT_SIZE));
    let mut removed = 0;
    // `segment_numbers` is sorted, so this walks them ascending.
    for seg in segment_numbers(dir)?.into_iter().take_while(|s| *s < floor) {
        match std::fs::remove_file(wal_segment_path(dir, seg)) {
            Ok(()) => removed += 1,
            // Already gone: an earlier pass that lost its directory fsync, or a
            // second process. Nothing to undo.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    if removed > 0 {
        sync_dir(&wal_dir(dir))?;
    }
    Ok(removed)
}

/// The writer's end of the layout, moving to the next segment as the stream
/// crosses a boundary.
///
/// Only ever used behind the WAL's writer mutex, so the create-then-fsync
/// sequences below cannot race.
pub struct SegmentWriter {
    dir: PathBuf,
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

    pub fn len(&self) -> Result<u64, WalError> {
        Ok(self.file.metadata()?.len())
    }

    /// Move to `seg`, leaving the segment being abandoned fsynced first.
    ///
    /// That order is a durability guarantee recovery reads back: a segment is
    /// made durable *before* the next is created, so a crash can never leave
    /// `N+1` on disk with `N` still short — which is what lets a short non-final
    /// segment be treated as corruption.
    fn switch_to(&mut self, seg: u64) -> Result<(), WalError> {
        self.file.sync_data()?;
        self.file = open_segment(&self.dir, seg)?;
        self.seg = seg;
        Ok(())
    }

    /// Write `bytes`, which start at stream position `start`, into the segments
    /// they belong to and fsync every file touched.
    ///
    /// `durable` is advanced to the number of leading bytes on stable storage,
    /// so on error it names exactly the prefix the caller may count as flushed:
    /// a multi-segment write is several fsyncs, and "all or nothing" would
    /// either lose the durable part or claim bytes that never landed.
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
                // The segment just left behind is fsynced, so everything written
                // before this point is durable.
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

    /// Lay down segments `0..=last` as full files, so a test can ask which of
    /// them a removal pass kept.
    fn lay_out_segments(dir: &Path, last: u64) -> std::io::Result<()> {
        std::fs::create_dir_all(wal_dir(dir))?;
        for seg in 0..=last {
            std::fs::write(wal_segment_path(dir, seg), [seg as u8])?;
        }
        Ok(())
    }

    #[test]
    fn removal_keeps_the_redo_segment_and_everything_above_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        lay_out_segments(dir.path(), 3)?;

        // A redo point in the middle of segment 2 still needs segment 2 whole.
        let redo = Lsn(2 * SEGMENT_SIZE + 17);
        assert_eq!(remove_segments_below(dir.path(), redo, 0)?, 2);
        assert_eq!(segment_numbers(dir.path())?, vec![2, 3]);
        // Idempotent: nothing left below the floor to remove.
        assert_eq!(remove_segments_below(dir.path(), redo, 0)?, 0);
        assert_eq!(segment_numbers(dir.path())?, vec![2, 3]);

        Ok(())
    }

    /// The retained tail is measured in bytes and rounds *up* to whole segments:
    /// keeping a fraction of a segment means keeping the file.
    #[test]
    fn keep_bytes_holds_back_whole_segments() -> anyhow::Result<()> {
        let redo = Lsn(3 * SEGMENT_SIZE);
        for (keep, left) in [
            (0, vec![3]),
            (1, vec![2, 3]),
            (SEGMENT_SIZE, vec![2, 3]),
            (SEGMENT_SIZE + 1, vec![1, 2, 3]),
            (2 * SEGMENT_SIZE, vec![1, 2, 3]),
            (u64::MAX, vec![0, 1, 2, 3]),
        ] {
            let dir = tempfile::tempdir()?;
            lay_out_segments(dir.path(), 3)?;
            remove_segments_below(dir.path(), redo, keep)?;
            assert_eq!(
                segment_numbers(dir.path())?,
                left,
                "keeping {keep} bytes below {redo}"
            );
        }

        Ok(())
    }

    /// A checkpoint that could not bound replay publishes `Lsn::INVALID`, and
    /// every segment is then still needed. Removing on that value would delete the
    /// whole log — including the only durable copy of a buffer table's rows.
    #[test]
    fn an_unbounded_redo_point_removes_nothing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        lay_out_segments(dir.path(), 3)?;
        assert_eq!(remove_segments_below(dir.path(), Lsn::INVALID, 0)?, 0);
        assert_eq!(segment_numbers(dir.path())?, vec![0, 1, 2, 3]);

        Ok(())
    }

    /// A redo point inside segment 0 has nothing below it, and an absent `pg_wal`
    /// is not an error — both are what a freshly started cluster looks like.
    #[test]
    fn removal_tolerates_nothing_to_remove() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(remove_segments_below(dir.path(), Lsn(64), 0)?, 0);
        lay_out_segments(dir.path(), 0)?;
        assert_eq!(remove_segments_below(dir.path(), Lsn(64), 0)?, 0);
        assert_eq!(segment_numbers(dir.path())?, vec![0]);

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
