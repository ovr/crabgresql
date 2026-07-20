//! `pg_control`: a small, atomically-rewritten file recording the last known
//! transaction high-water mark and whether the server shut down cleanly.
//!
//! Recovery does not depend on it for correctness (it replays the whole WAL and
//! derives the next XID from the records it sees); the control file is a floor
//! for the XID counter and a clean-shutdown marker. Segment recycling and a
//! checkpoint redo-LSN that would let recovery *skip* early WAL are follow-ups
//! (`docs/ARCHITECTURE.md §3`), and both need the durable CLOG that this cut
//! intentionally defers.

use std::io::Write;
use std::path::{Path, PathBuf};

use crabgresql_txn::Xid;

use crate::record::WalError;

const CONTROL_SUBDIR: &str = "global";
const CONTROL_FILE: &str = "pg_control";
const MAGIC: u32 = 0xCA6D_0001;
const VERSION: u32 = 1;

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
    pub next_xid: Xid,
    pub clean_shutdown: bool,
}

/// Read `pg_control`, or `None` when it is absent (a freshly created cluster).
/// A corrupt/truncated control file is treated as absent — recovery rebuilds all
/// state from the WAL regardless.
pub fn read_control(dir: &Path) -> Result<Option<ControlFile>, WalError> {
    let path = control_path(dir);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if bytes.len() < 24 {
        return Ok(None);
    }
    let Some(magic) = read_u32(&bytes, 0) else {
        return Ok(None);
    };
    let Some(version) = read_u32(&bytes, 4) else {
        return Ok(None);
    };
    if magic != MAGIC || version != VERSION {
        return Ok(None);
    }
    let Some(crc_stored) = read_u32(&bytes, 20) else {
        return Ok(None);
    };
    if crc32c::crc32c(&bytes[0..20]) != crc_stored {
        return Ok(None);
    }
    let Some(next_xid) = read_u64(&bytes, 8).map(Xid) else {
        return Ok(None);
    };
    let clean_shutdown = bytes[16] != 0;
    Ok(Some(ControlFile {
        next_xid,
        clean_shutdown,
    }))
}

/// Atomically replace `pg_control`: write a temp file, fsync it, rename over the
/// target, then fsync the directory so the rename is durable.
pub fn write_control(dir: &Path, ctl: &ControlFile) -> Result<(), WalError> {
    let subdir = dir.join(CONTROL_SUBDIR);
    std::fs::create_dir_all(&subdir)?;

    let mut bytes = Vec::with_capacity(24);
    bytes.extend_from_slice(&MAGIC.to_le_bytes());
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&ctl.next_xid.0.to_le_bytes());
    bytes.push(ctl.clean_shutdown as u8);
    bytes.extend_from_slice(&[0u8; 3]); // pad to 20
    let crc = crc32c::crc32c(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());

    let tmp = subdir.join("pg_control.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_data()?;
    }
    std::fs::rename(&tmp, control_path(dir))?;
    // fsync the directory so the rename survives a crash.
    if let Ok(d) = std::fs::File::open(&subdir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_absent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(read_control(dir.path())?, None);
        let ctl = ControlFile {
            next_xid: Xid(1234),
            clean_shutdown: true,
        };
        write_control(dir.path(), &ctl)?;
        assert_eq!(read_control(dir.path())?, Some(ctl));

        Ok(())
    }
}
