//! `pg_control`: a small, atomically-rewritten file recording where recovery
//! must resume, the last known transaction high-water mark, and whether the
//! server shut down cleanly.
//!
//! The redo LSN is the one field recovery cannot derive for itself: replay has to
//! know where to *begin* reading, so it must come from a file at a fixed path
//! rather than from a record inside the stream being read. Everything else here
//! is a floor — replay raises the XID counter from the records it sees, but only
//! those at or above the redo point, so a bounded replay depends on
//! [`ControlFile::next_xid`] being current (see [`crate::recover`]).
//!
//! A control file that is absent, truncated, of an unknown version, or fails its
//! CRC reads as `None`, which the caller turns into a whole-stream replay. That
//! is the fail-safe direction: replaying more than necessary is always correct,
//! because redo is idempotent under the per-page LSN gate. Segment recycling —
//! the other thing a durable redo point unlocks — is still a follow-up
//! (`docs/ARCHITECTURE.md §3`).

use std::io::Write;
use std::path::{Path, PathBuf};

use crabgresql_txn::Xid;

use crate::record::{Lsn, WalError};

const CONTROL_SUBDIR: &str = "global";
const CONTROL_FILE: &str = "pg_control";
const MAGIC: u32 = 0xCA6D_0001;
/// Version 3 has the same fields at the same offsets as version 2. The bump is
/// not about the layout: it is the marker that the *write-ahead log* underneath
/// changed to a paged, segmented format that a version-2 directory's WAL cannot
/// be read as. See [`WalError::IncompatibleWalFormat`].
const VERSION: u32 = 3;
/// Encoded length of a version-2 image, and the offset of its trailing CRC —
/// which is also the number of leading bytes that CRC covers. The reader and the
/// writer both take the covered range from `CRC_OFFSET`: a range that disagreed
/// between them would make every control file read as corrupt, and because
/// corruption is deliberately indistinguishable from absence that would not
/// fail — it would silently degrade every start to a whole-stream replay.
const LEN: usize = 32;
const CRC_OFFSET: usize = 28;

/// Version 1 predates the redo point: 24 bytes with `clean_shutdown` at 16 and
/// the CRC at 20. Version 2 has version 3's layout but a pre-paging WAL beside
/// it. Both are recognized only in order to be **refused**.
///
/// An on-disk format is a compatibility boundary (`AGENTS.md`), and refusing is
/// how this one is honored. The alternative is not "read the old control file" —
/// its fields still decode fine — but "read the old control file and then read
/// the old WAL", and the old WAL parses as an empty log rather than as an error.
/// Loading these would therefore start a cluster that silently threw its log
/// away. Corrupt and unknown-version images still read as absent, so the
/// fall-back to a whole-stream replay is unchanged.
const LEGACY_VERSIONS: [(u32, usize, usize); 2] = [(1, 24, 20), (2, 32, 28)];

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

pub fn control_path(dir: &Path) -> PathBuf {
    dir.join(CONTROL_SUBDIR).join(CONTROL_FILE)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlFile {
    /// The XID the allocator must start at or above. A bounded replay never sees
    /// the XIDs below its redo point, so this is their only remaining floor.
    pub next_xid: Xid,
    /// The record boundary replay resumes from. [`Lsn::INVALID`] means "replay
    /// the whole stream" — a fresh cluster, or a checkpoint that could not bound
    /// itself (see `PgEngine::redo_floor`).
    pub redo_lsn: Lsn,
    pub clean_shutdown: bool,
}

/// Read `pg_control`, or `None` when it is absent (a freshly created cluster) or
/// unreadable. A corrupt/truncated/unknown-version control file is treated as
/// absent: recovery then replays the whole stream and rebuilds every floor from
/// it, which is correct, just slower.
pub fn read_control(dir: &Path) -> Result<Option<ControlFile>, WalError> {
    let bytes = match std::fs::read(control_path(dir)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if let Some(version) = legacy_version(&bytes) {
        return Err(WalError::IncompatibleWalFormat {
            detail: format!("pg_control is version {version}"),
        });
    }
    Ok(decode(&bytes))
}

/// The version of a control file this build recognizes but will not load, or
/// `None`. A legacy image must pass its own CRC to be reported: an unreadable
/// file is deliberately indistinguishable from an absent one, and turning noise
/// into a refusal to start would be the wrong trade.
fn legacy_version(bytes: &[u8]) -> Option<u32> {
    if read_u32(bytes, 0)? != MAGIC {
        return None;
    }
    let version = read_u32(bytes, 4)?;
    let (_, len, crc_offset) = LEGACY_VERSIONS.iter().find(|(v, ..)| *v == version)?;
    if bytes.len() < *len {
        return None;
    }
    (crc32c::crc32c(&bytes[0..*crc_offset]) == read_u32(bytes, *crc_offset)?).then_some(version)
}

/// Parse a control-file image. `None` for anything we cannot vouch for.
fn decode(bytes: &[u8]) -> Option<ControlFile> {
    if read_u32(bytes, 0)? != MAGIC || read_u32(bytes, 4)? != VERSION {
        return None;
    }
    if bytes.len() < LEN {
        return None;
    }
    if crc32c::crc32c(&bytes[0..CRC_OFFSET]) != read_u32(bytes, CRC_OFFSET)? {
        return None;
    }
    Some(ControlFile {
        next_xid: Xid(read_u64(bytes, 8)?),
        redo_lsn: Lsn(read_u64(bytes, 16)?),
        clean_shutdown: *bytes.get(24)? != 0,
    })
}

/// Serialize a version-2 image.
fn encode(ctl: &ControlFile) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(LEN);
    bytes.extend_from_slice(&MAGIC.to_le_bytes());
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&ctl.next_xid.0.to_le_bytes());
    bytes.extend_from_slice(&ctl.redo_lsn.0.to_le_bytes());
    bytes.push(ctl.clean_shutdown as u8);
    bytes.extend_from_slice(&[0u8; 3]); // pad up to the CRC
    // Ties the layout above to the constant the verifier reads, so a field added
    // without moving `CRC_OFFSET` fails here instead of leaving the new bytes
    // outside the checksum.
    debug_assert_eq!(bytes.len(), CRC_OFFSET);
    let crc = crc32c::crc32c(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    debug_assert_eq!(bytes.len(), LEN);
    bytes
}

/// Atomically replace `pg_control`: write a temp file, fsync it, rename over the
/// target, then fsync the directory so the rename is durable.
pub fn write_control(dir: &Path, ctl: &ControlFile) -> Result<(), WalError> {
    let subdir = dir.join(CONTROL_SUBDIR);
    std::fs::create_dir_all(&subdir)?;

    let bytes = encode(ctl);
    let tmp = subdir.join("pg_control.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_data()?;
    }
    std::fs::rename(&tmp, control_path(dir))?;
    // fsync the directory so the rename survives a crash. Real errors propagate —
    // this file names the LSN recovery resumes from, so reporting success for a
    // rename a crash could still undo would leave the caller believing a
    // checkpoint is published when it is not — but a filesystem that simply
    // cannot fsync a directory is tolerated rather than fatal, or the server
    // would refuse to start on it. See [`crate::sync_dir`].
    crate::fsutil::sync_dir(&subdir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_raw(dir: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let subdir = dir.join(CONTROL_SUBDIR);
        std::fs::create_dir_all(&subdir)?;
        std::fs::write(control_path(dir), bytes)
    }

    /// A legacy image, as the build that wrote `version` would have produced it.
    /// Version 1 predates the redo point (24 bytes); version 2 has version 3's
    /// layout but a pre-paged WAL beside it.
    fn legacy_image(version: u32, next_xid: Xid, clean_shutdown: bool) -> Vec<u8> {
        let Some(&(_, len, crc_offset)) = LEGACY_VERSIONS.iter().find(|(v, ..)| *v == version)
        else {
            panic!("{version} is not a legacy control-file version");
        };
        let mut bytes = Vec::with_capacity(len);
        bytes.extend_from_slice(&MAGIC.to_le_bytes());
        bytes.extend_from_slice(&version.to_le_bytes());
        bytes.extend_from_slice(&next_xid.0.to_le_bytes());
        if version >= 2 {
            bytes.extend_from_slice(&Lsn(8192).0.to_le_bytes());
        }
        bytes.push(clean_shutdown as u8);
        bytes.extend_from_slice(&[0u8; 3]);
        assert_eq!(bytes.len(), crc_offset);
        let crc = crc32c::crc32c(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        bytes
    }

    #[test]
    fn roundtrip_and_absent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(read_control(dir.path())?, None);
        let ctl = ControlFile {
            next_xid: Xid(1234),
            redo_lsn: Lsn(4096),
            clean_shutdown: true,
        };
        write_control(dir.path(), &ctl)?;
        assert_eq!(read_control(dir.path())?, Some(ctl));

        Ok(())
    }

    /// The CRC-range pin. Every byte of the image — including the padding and the
    /// checksum itself — must be covered, or a `redo_lsn` could be corrupted into
    /// a plausible value and send replay to the wrong offset. A reader still
    /// checksumming the v1 range would leave bytes 20..28 unprotected and fail
    /// here at `offset == 20`.
    #[test]
    fn flipping_any_byte_makes_the_control_file_read_as_absent() -> anyhow::Result<()> {
        let ctl = ControlFile {
            next_xid: Xid(7),
            redo_lsn: Lsn(8192),
            clean_shutdown: true,
        };
        let good = encode(&ctl);
        assert_eq!(good.len(), LEN);
        for offset in 0..LEN {
            let mut bad = good.clone();
            bad[offset] ^= 0xFF;
            let dir = tempfile::tempdir()?;
            write_raw(dir.path(), &bad)?;
            assert_eq!(
                read_control(dir.path())?,
                None,
                "a flipped byte at offset {offset} was not detected"
            );
        }

        Ok(())
    }

    /// A directory from before the paged WAL is refused, not loaded.
    ///
    /// Loading it would succeed — these fields still decode — and then the WAL
    /// beside it would read as an *empty* log, because its first four bytes are a
    /// record length rather than a page magic. The cluster would start, silently
    /// having thrown its log away. An on-disk format is a compatibility boundary
    /// (`AGENTS.md`), and this is how this one is honored: loudly.
    #[test]
    fn a_pre_paged_control_file_refuses_to_load() -> anyhow::Result<()> {
        for version in [1u32, 2] {
            let dir = tempfile::tempdir()?;
            write_raw(dir.path(), &legacy_image(version, Xid(99), true))?;
            let Err(error) = read_control(dir.path()) else {
                anyhow::bail!("a version-{version} control file was accepted");
            };
            assert!(
                matches!(error, WalError::IncompatibleWalFormat { .. }),
                "unexpected error for version {version}: {error}"
            );
            assert!(error.to_string().contains(&format!("version {version}")));
        }

        Ok(())
    }

    /// Corruption stays indistinguishable from absence even at a legacy version:
    /// turning noise into a refusal to start would be the wrong trade.
    #[test]
    fn a_legacy_image_that_fails_its_crc_reads_as_absent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut image = legacy_image(2, Xid(99), true);
        image[8] ^= 0xFF;
        write_raw(dir.path(), &image)?;
        assert_eq!(read_control(dir.path())?, None);

        Ok(())
    }

    #[test]
    fn a_short_or_unknown_version_image_reads_as_absent() -> anyhow::Result<()> {
        let ctl = ControlFile {
            next_xid: Xid(5),
            redo_lsn: Lsn(64),
            clean_shutdown: false,
        };
        let good = encode(&ctl);
        for truncated in [0, 1, 24, LEN - 1] {
            let dir = tempfile::tempdir()?;
            write_raw(dir.path(), &good[..truncated])?;
            assert_eq!(
                read_control(dir.path())?,
                None,
                "a {truncated}-byte image was accepted"
            );
        }
        // A version this build has never written, with a CRC that would otherwise
        // check out: refused, because its tail layout is unknown.
        let mut future = good.clone();
        future[4..8].copy_from_slice(&99u32.to_le_bytes());
        let crc = crc32c::crc32c(&future[0..CRC_OFFSET]);
        future[CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
        let dir = tempfile::tempdir()?;
        write_raw(dir.path(), &future)?;
        assert_eq!(read_control(dir.path())?, None);

        Ok(())
    }

    /// Pins the wire layout itself, so a field reordered or resized shows up here
    /// rather than as an unreadable data directory after an upgrade.
    #[test]
    fn the_encoded_layout_is_stable() {
        let bytes = encode(&ControlFile {
            next_xid: Xid(0x0102_0304_0506_0708),
            redo_lsn: Lsn(0x1112_1314_1516_1718),
            clean_shutdown: true,
        });
        assert_eq!(&bytes[0..4], &MAGIC.to_le_bytes());
        assert_eq!(&bytes[4..8], &3u32.to_le_bytes());
        assert_eq!(&bytes[8..16], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(&bytes[16..24], &0x1112_1314_1516_1718u64.to_le_bytes());
        assert_eq!(bytes[24], 1);
        assert_eq!(&bytes[25..28], &[0, 0, 0]);
        assert_eq!(bytes.len(), 32);
    }
}
