//! On-disk commit log: a page-addressed, two-bits-per-transaction bitmap under
//! `<data_dir>/pg_xact/`.
//!
//! This module is pure storage — addressing, encoding, and segment I/O. The
//! caching policy (which pages are resident, when dirty pages are written) lives
//! in [`crate::Clog`], which sits on top of these functions.
//!
//! Keeping it here costs the crate nothing: `std::fs` is all it needs, so
//! `crabgresql-txn` stays dependency-free and remains the leaf that
//! `crabgresql-wal` can depend on.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::{XactStatus, Xid};

/// The subdirectory of the data directory holding the commit log.
pub const CLOG_SUBDIR: &str = "pg_xact";

/// Bits of status stored per transaction.
pub const BITS_PER_XACT: u64 = 2;
/// Bytes per CLOG page. Matches the heap's `BLCKSZ`, though nothing requires it
/// to — the two are addressed independently.
pub const CLOG_PAGE_SIZE: usize = 8192;
/// Transactions addressed by one page: `8192 bytes * 4 xacts/byte` = 2^15.
pub const XACTS_PER_PAGE: u64 = 1 << 15;
/// Pages per segment file.
pub const PAGES_PER_SEGMENT: u64 = 32;
/// Transactions per segment file: 2^20, so a segment is 256 KiB.
pub const XACTS_PER_SEGMENT: u64 = XACTS_PER_PAGE * PAGES_PER_SEGMENT;

// Every constant above is a power of two, so all addressing below is shifts and
// masks. This is also why there is no per-page header: reserving header bytes
// would make XACTS_PER_PAGE a non-power-of-two and put a 64-bit divide in every
// visibility check, which is the hottest path in the system.
const PAGE_SHIFT: u32 = XACTS_PER_PAGE.trailing_zeros();
const SEGMENT_SHIFT: u32 = XACTS_PER_SEGMENT.trailing_zeros();
const BYTE_IN_PAGE_MASK: u64 = CLOG_PAGE_SIZE as u64 - 1;
const XACTS_PER_BYTE_MASK: u64 = (8 / BITS_PER_XACT) - 1;
const STATUS_MASK: u8 = (1 << BITS_PER_XACT) - 1;

/// One CLOG page, held by value: 8 KiB is cheap to copy and this keeps the cache
/// free of lifetimes.
pub type ClogPage = [u8; CLOG_PAGE_SIZE];

/// An all-zero page. Zero is [`XactStatus::InProgress`], which is what a hole, a
/// never-written page, and a missing segment must all read as.
pub const ZERO_PAGE: ClogPage = [0u8; CLOG_PAGE_SIZE];

/// The page holding `xid`.
pub const fn page_of(xid: Xid) -> u64 {
    xid.0 >> PAGE_SHIFT
}

/// The segment file holding `xid`.
pub const fn segment_of(xid: Xid) -> u64 {
    xid.0 >> SEGMENT_SHIFT
}

/// The segment file holding page `pageno`.
pub const fn segment_of_page(pageno: u64) -> u64 {
    pageno / PAGES_PER_SEGMENT
}

/// Where `pageno` sits within its segment file.
const fn page_in_segment(pageno: u64) -> u64 {
    pageno % PAGES_PER_SEGMENT
}

const fn byte_in_page(xid: Xid) -> usize {
    ((xid.0 >> BITS_PER_XACT) & BYTE_IN_PAGE_MASK) as usize
}

const fn shift_in_byte(xid: Xid) -> u32 {
    (BITS_PER_XACT * (xid.0 & XACTS_PER_BYTE_MASK)) as u32
}

/// Read `xid`'s status out of the page that holds it.
pub fn page_status(page: &ClogPage, xid: Xid) -> XactStatus {
    XactStatus::from_bits((page[byte_in_page(xid)] >> shift_in_byte(xid)) & STATUS_MASK)
}

/// Stamp `xid`'s status into the page that holds it.
pub fn set_page_status(page: &mut ClogPage, xid: Xid, status: XactStatus) {
    let byte = &mut page[byte_in_page(xid)];
    let shift = shift_in_byte(xid);
    *byte = (*byte & !(STATUS_MASK << shift)) | (status.to_bits() << shift);
}

/// `<dir>/<16 hex digits>`. The full u64 segment number is zero-padded so
/// lexicographic order equals numeric order — which is what lets truncation walk
/// the directory and stop at the first segment at or above the floor.
pub fn segment_path(dir: &Path, segno: u64) -> PathBuf {
    dir.join(format!("{segno:016X}"))
}

/// Read one page. A missing segment, or a page past the end of its segment,
/// reads as all zeros.
///
/// This deliberately does **not** use `read_exact`, diverging from the heap's
/// `smgr::read`. There, a short read means a truncated relation file and must
/// hard-fail, because the heap page is the only copy of that data. Here, a short
/// read past EOF is the normal case: segments are written sparsely, so a page
/// that was never stamped simply is not there yet, and zero-fill is the correct
/// answer rather than an error.
pub fn read_page(dir: &Path, pageno: u64) -> std::io::Result<ClogPage> {
    let mut page = ZERO_PAGE;
    let path = segment_path(dir, segment_of_page(pageno));
    let mut file = match File::open(&path) {
        Ok(file) => file,
        // A segment that does not exist is a run of transactions none of which
        // has reported a fate yet: all InProgress.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(page),
        Err(error) => return Err(error),
    };
    file.seek(SeekFrom::Start(
        page_in_segment(pageno) * CLOG_PAGE_SIZE as u64,
    ))?;
    let mut filled = 0;
    while filled < CLOG_PAGE_SIZE {
        match file.read(&mut page[filled..]) {
            Ok(0) => break, // EOF: the rest of the page stays zero.
            Ok(n) => filled += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(page)
}

/// Write one page into its segment, creating the segment if needed.
///
/// Segments are sparse: seeking past the current end and writing leaves a hole,
/// and holes read back as zeros — which is exactly [`XactStatus::InProgress`].
/// So a cluster that has only ever committed a high XID gets a segment with one
/// real page in it, not a fully materialised 256 KiB file.
pub fn write_page(dir: &Path, pageno: u64, page: &ClogPage) -> std::io::Result<()> {
    let path = segment_path(dir, segment_of_page(pageno));
    // truncate(false): the segment's other pages must survive.
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    file.seek(SeekFrom::Start(
        page_in_segment(pageno) * CLOG_PAGE_SIZE as u64,
    ))?;
    file.write_all(page)?;
    Ok(())
}

/// Flush one segment file to stable storage.
pub fn sync_segment(dir: &Path, segno: u64) -> std::io::Result<()> {
    File::open(segment_path(dir, segno))?.sync_data()
}

/// fsync a directory so a rename or unlink within it is durable.
pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

// ---------------------------------------------------------------------------
// meta
// ---------------------------------------------------------------------------

const META_FILE: &str = "meta";
const META_TMP: &str = "meta.tmp";
const META_MAGIC: u32 = 0xCA6D_C106;
const META_VERSION: u32 = 1;
const META_LEN: usize = 32;

/// The out-of-band version marker for the commit log.
///
/// It cannot live in `pg_control`: `ControlFile` belongs to `crabgresql-wal`,
/// which depends on this crate, so putting it there would invert the dependency.
///
/// There is no CRC, unlike `pg_control`. This record is a single sector replaced
/// by `rename`, so a partial write is unreachable, and the magic, version and
/// geometry fields already reject garbage. (`pg_control` can afford a CRC because
/// `crabgresql-wal` already depends on `crc32c`; this crate depends on nothing.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClogMeta {
    /// Every XID below this has been frozen out of every relation, so its status
    /// is no longer reachable and its segment may be removed.
    pub floor: Xid,
}

fn read_u32(bytes: &[u8], start: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(start..start + 4)?.try_into().ok()?,
    ))
}

fn read_u64(bytes: &[u8], start: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(start..start + 8)?.try_into().ok()?,
    ))
}

fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}

/// Read `<dir>/meta`.
///
/// `Ok(None)` means the file is absent — a freshly created cluster, not an error.
/// Anything present but unreadable is a hard error: unlike `pg_control`, which
/// recovery can afford to treat as absent because it rebuilds from the WAL
/// anyway, a `meta` we cannot parse means we do not know how to address the
/// segments sitting next to it, and guessing would silently misread commit
/// status.
pub fn read_meta(dir: &Path) -> std::io::Result<Option<ClogMeta>> {
    let bytes = match std::fs::read(dir.join(META_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if bytes.len() < META_LEN {
        return Err(invalid(format!(
            "pg_xact/meta is {} bytes, expected at least {META_LEN}",
            bytes.len()
        )));
    }
    let field = |start: usize| read_u32(&bytes, start).ok_or_else(|| invalid("pg_xact/meta"));
    let magic = field(0)?;
    if magic != META_MAGIC {
        return Err(invalid(format!(
            "pg_xact/meta has magic {magic:#010x}, expected {META_MAGIC:#010x}"
        )));
    }
    let version = field(4)?;
    if version != META_VERSION {
        return Err(invalid(format!(
            "pg_xact/meta has version {version}, expected {META_VERSION}"
        )));
    }
    // The geometry is compiled into the addressing above, so a disagreement means
    // every offset we would compute is wrong. Refuse rather than misread.
    let geometry = [
        ("bits per xact", field(8)? as u64, BITS_PER_XACT),
        ("page size", field(12)? as u64, CLOG_PAGE_SIZE as u64),
        ("xacts per page", field(16)? as u64, XACTS_PER_PAGE),
        ("pages per segment", field(20)? as u64, PAGES_PER_SEGMENT),
    ];
    for (name, found, expected) in geometry {
        if found != expected {
            return Err(invalid(format!(
                "pg_xact/meta has {name} {found}, but this build uses {expected}"
            )));
        }
    }
    let floor = read_u64(&bytes, 24)
        .map(Xid)
        .ok_or_else(|| invalid("pg_xact/meta is missing the floor"))?;
    Ok(Some(ClogMeta { floor }))
}

/// Atomically replace `<dir>/meta`: write a temp file, fsync it, rename over the
/// target, then fsync the directory so the rename is durable. The same idiom as
/// `crabgresql-wal`'s `write_control`.
pub fn write_meta(dir: &Path, meta: &ClogMeta) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;

    let mut bytes = Vec::with_capacity(META_LEN);
    bytes.extend_from_slice(&META_MAGIC.to_le_bytes());
    bytes.extend_from_slice(&META_VERSION.to_le_bytes());
    bytes.extend_from_slice(&(BITS_PER_XACT as u32).to_le_bytes());
    bytes.extend_from_slice(&(CLOG_PAGE_SIZE as u32).to_le_bytes());
    bytes.extend_from_slice(&(XACTS_PER_PAGE as u32).to_le_bytes());
    bytes.extend_from_slice(&(PAGES_PER_SEGMENT as u32).to_le_bytes());
    bytes.extend_from_slice(&meta.floor.0.to_le_bytes());

    let tmp = dir.join(META_TMP);
    {
        let mut file = File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_data()?;
    }
    std::fs::rename(&tmp, dir.join(META_FILE))?;
    sync_dir(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addressing_is_dense_and_distinct() {
        // Four transactions share a byte, and the fifth moves on.
        assert_eq!(byte_in_page(Xid(0)), 0);
        assert_eq!(byte_in_page(Xid(3)), 0);
        assert_eq!(byte_in_page(Xid(4)), 1);
        assert_eq!(shift_in_byte(Xid(0)), 0);
        assert_eq!(shift_in_byte(Xid(3)), 6);

        // Page and segment boundaries land where the shifts say they do.
        assert_eq!(page_of(Xid(XACTS_PER_PAGE - 1)), 0);
        assert_eq!(page_of(Xid(XACTS_PER_PAGE)), 1);
        assert_eq!(segment_of(Xid(XACTS_PER_SEGMENT - 1)), 0);
        assert_eq!(segment_of(Xid(XACTS_PER_SEGMENT)), 1);
        assert_eq!(segment_of_page(PAGES_PER_SEGMENT - 1), 0);
        assert_eq!(segment_of_page(PAGES_PER_SEGMENT), 1);
    }

    #[test]
    fn every_status_survives_a_bit_roundtrip() {
        for status in [
            XactStatus::InProgress,
            XactStatus::Committed,
            XactStatus::Aborted,
            XactStatus::SubCommitted,
        ] {
            assert_eq!(XactStatus::from_bits(status.to_bits()), status);
        }
    }

    #[test]
    fn neighbours_in_one_byte_do_not_disturb_each_other() {
        let mut page = ZERO_PAGE;
        // Four XIDs sharing byte 0, each with a different status.
        set_page_status(&mut page, Xid(0), XactStatus::Committed);
        set_page_status(&mut page, Xid(1), XactStatus::Aborted);
        set_page_status(&mut page, Xid(2), XactStatus::SubCommitted);
        set_page_status(&mut page, Xid(3), XactStatus::Committed);
        assert_eq!(page_status(&page, Xid(0)), XactStatus::Committed);
        assert_eq!(page_status(&page, Xid(1)), XactStatus::Aborted);
        assert_eq!(page_status(&page, Xid(2)), XactStatus::SubCommitted);
        assert_eq!(page_status(&page, Xid(3)), XactStatus::Committed);

        // Overwriting one leaves its neighbours alone.
        set_page_status(&mut page, Xid(1), XactStatus::Committed);
        assert_eq!(page_status(&page, Xid(0)), XactStatus::Committed);
        assert_eq!(page_status(&page, Xid(1)), XactStatus::Committed);
        assert_eq!(page_status(&page, Xid(2)), XactStatus::SubCommitted);
    }

    #[test]
    fn a_page_survives_a_write_read_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut page = ZERO_PAGE;
        set_page_status(&mut page, Xid(7), XactStatus::Committed);
        write_page(dir.path(), 0, &page)?;
        assert_eq!(
            page_status(&read_page(dir.path(), 0)?, Xid(7)),
            XactStatus::Committed
        );
        Ok(())
    }

    #[test]
    fn absent_and_sparse_pages_read_as_in_progress() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // No segment at all.
        assert_eq!(read_page(dir.path(), 0)?, ZERO_PAGE);

        // Write only the last page of segment 0, leaving a hole beneath it. The
        // short read past EOF that `read_exact` would reject is the normal case.
        let last = PAGES_PER_SEGMENT - 1;
        let mut page = ZERO_PAGE;
        set_page_status(&mut page, Xid(1), XactStatus::Committed);
        write_page(dir.path(), last, &page)?;
        assert_eq!(read_page(dir.path(), 0)?, ZERO_PAGE);
        assert_eq!(read_page(dir.path(), last)?, page);
        // A page beyond everything written is still just zeros.
        assert_eq!(read_page(dir.path(), last + 1)?, ZERO_PAGE);
        Ok(())
    }

    #[test]
    fn a_high_xid_lands_in_its_own_segment() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let xid = Xid(XACTS_PER_SEGMENT + 5);
        let pageno = page_of(xid);
        let mut page = ZERO_PAGE;
        set_page_status(&mut page, xid, XactStatus::Aborted);
        write_page(dir.path(), pageno, &page)?;

        assert!(segment_path(dir.path(), 1).exists());
        assert!(!segment_path(dir.path(), 0).exists());
        assert_eq!(
            page_status(&read_page(dir.path(), pageno)?, xid),
            XactStatus::Aborted
        );
        Ok(())
    }

    #[test]
    fn meta_is_absent_then_roundtrips() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(read_meta(dir.path())?, None);
        let meta = ClogMeta { floor: Xid(4242) };
        write_meta(dir.path(), &meta)?;
        assert_eq!(read_meta(dir.path())?, Some(meta));
        Ok(())
    }

    #[test]
    fn a_corrupt_meta_is_a_hard_error() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        write_meta(dir.path(), &ClogMeta { floor: Xid(1) })?;
        let path = dir.path().join(META_FILE);

        // Bad magic.
        let mut bytes = std::fs::read(&path)?;
        bytes[0] ^= 0xFF;
        std::fs::write(&path, &bytes)?;
        assert!(read_meta(dir.path()).is_err());

        // Disagreeing geometry: every offset we would compute would be wrong.
        let mut bytes = std::fs::read(&path)?;
        bytes[0] ^= 0xFF; // restore the magic
        bytes[16..20].copy_from_slice(&1024u32.to_le_bytes());
        std::fs::write(&path, &bytes)?;
        assert!(read_meta(dir.path()).is_err());

        // Truncated.
        std::fs::write(&path, [0u8; 4])?;
        assert!(read_meta(dir.path()).is_err());
        Ok(())
    }
}
