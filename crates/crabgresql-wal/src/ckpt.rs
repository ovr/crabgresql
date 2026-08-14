//! The [`crate::RmgrId::CHECKPOINT`] record payload.
//!
//! A checkpoint's redo point lives in `pg_control`, not here — replay has to know
//! where to begin before it can read anything, so it cannot come from a record
//! inside the stream being read. This record is the in-stream witness of the same
//! event, and it earns its place three ways:
//!
//! * It carries an XID floor nothing else can supply. A transaction that only
//!   touched `UNLOGGED`/`TEMP` relations never writes a WAL record at all, so
//!   `max(record xid) + 1` is not a sufficient floor; the checkpoint's
//!   `next_xid` is. That matters exactly when `pg_control` is unreadable and
//!   replay has fallen back to the whole stream.
//! * It is the anchor for anything that has to reason about checkpoints in
//!   *stream order* rather than about the latest one, which is all a control file
//!   describes (`docs/ARCHITECTURE.md §1.3`). Retiring spent segments does not
//!   need that — [`crate::remove_segments_below`] works off the one redo point
//!   `pg_control` publishes — but reusing a segment under a future name will.
//! * It lets a reader verify that the LSN `pg_control` published really names a
//!   checkpoint boundary.
//!
//! The record is appended *after* the pages and the commit log it covers reach
//! disk, so a durable CHECKPOINT record implies that checkpoint's work completed.

use crabgresql_txn::Xid;

use crate::record::{Lsn, WalError};

/// `info` byte values for a [`crate::RmgrId::CHECKPOINT`] record.
pub const CHECKPOINT_ONLINE: u8 = 0x01;
/// A checkpoint taken as the server shuts down cleanly.
pub const CHECKPOINT_SHUTDOWN: u8 = 0x02;

const MAGIC: u32 = 0x504B_4350;
const VERSION: u32 = 1;
const LEN: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// The redo point this checkpoint published — the record boundary a recovery
    /// resuming from it must start at.
    pub redo_lsn: Lsn,
    /// The XID floor at checkpoint time.
    pub next_xid: Xid,
}

impl Checkpoint {
    /// Serialize the payload: `magic u32 | version u32 | redo_lsn u64 |
    /// next_xid u64`, little-endian like every other record body.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(LEN);
        bytes.extend_from_slice(&MAGIC.to_le_bytes());
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&self.redo_lsn.0.to_le_bytes());
        bytes.extend_from_slice(&self.next_xid.0.to_le_bytes());
        debug_assert_eq!(bytes.len(), LEN);
        bytes
    }

    /// Parse a payload, or `None` when it is short, not ours, or of a version
    /// this build cannot address.
    ///
    /// Deliberately not built on the heap record reader in `rec.rs`: that one
    /// panics on a short buffer, and a malformed payload here has to come back as
    /// a recovery *error*, never as an aborted process.
    pub fn decode(bytes: &[u8]) -> Option<Checkpoint> {
        if bytes.len() < LEN {
            return None;
        }
        let field = |start: usize| -> u32 {
            u32::from_le_bytes([
                bytes[start],
                bytes[start + 1],
                bytes[start + 2],
                bytes[start + 3],
            ])
        };
        if field(0) != MAGIC || field(4) != VERSION {
            return None;
        }
        let word = |start: usize| -> u64 {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes[start..start + 8]);
            u64::from_le_bytes(buf)
        };
        Some(Checkpoint {
            redo_lsn: Lsn(word(8)),
            next_xid: Xid(word(16)),
        })
    }
}

/// Decode the payload of a checkpoint record found at `rec_lsn` during replay,
/// rejecting anything inconsistent.
///
/// Strict on purpose, matching how the `XACT` arm treats an unrecognized info
/// byte: the payload carries an XID floor, and silently skipping a payload we
/// cannot read would let the allocator reissue an XID already stamped on
/// committed tuples. A loud refusal is recoverable; that is not.
pub fn replay(info: u8, payload: &[u8], rec_lsn: Lsn) -> Result<Checkpoint, WalError> {
    if info != CHECKPOINT_ONLINE && info != CHECKPOINT_SHUTDOWN {
        return Err(WalError::Redo(format!(
            "unknown checkpoint info byte {info:#x}"
        )));
    }
    let Some(ckpt) = Checkpoint::decode(payload) else {
        return Err(WalError::Redo(format!(
            "checkpoint record at {rec_lsn} has an unreadable {}-byte payload",
            payload.len()
        )));
    };
    // A checkpoint samples its redo point before appending its own record, so the
    // redo can never sit above it. A payload claiming otherwise is corrupt, and
    // trusting it would publish a floor derived from a record we cannot believe.
    if ckpt.redo_lsn > rec_lsn {
        return Err(WalError::Redo(format!(
            "checkpoint record at {rec_lsn} claims a redo point of {} above itself",
            ckpt.redo_lsn
        )));
    }
    Ok(ckpt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_roundtrip() {
        for ckpt in [
            Checkpoint {
                redo_lsn: Lsn::INVALID,
                next_xid: Xid::FIRST_NORMAL,
            },
            Checkpoint {
                redo_lsn: Lsn(8192),
                next_xid: Xid(u64::MAX),
            },
        ] {
            assert_eq!(Checkpoint::decode(&ckpt.encode()), Some(ckpt));
        }
    }

    #[test]
    fn a_truncated_or_foreign_payload_decodes_to_none() {
        let good = Checkpoint {
            redo_lsn: Lsn(64),
            next_xid: Xid(9),
        }
        .encode();
        for len in 0..good.len() {
            assert_eq!(
                Checkpoint::decode(&good[..len]),
                None,
                "a {len}-byte payload decoded"
            );
        }
        let mut wrong_magic = good.clone();
        wrong_magic[0] ^= 0xFF;
        assert_eq!(Checkpoint::decode(&wrong_magic), None);
        let mut future_version = good.clone();
        future_version[4..8].copy_from_slice(&99u32.to_le_bytes());
        assert_eq!(Checkpoint::decode(&future_version), None);
    }

    #[test]
    fn replay_rejects_what_it_cannot_believe() -> anyhow::Result<()> {
        let ckpt = Checkpoint {
            redo_lsn: Lsn(64),
            next_xid: Xid(9),
        };
        let payload = ckpt.encode();
        assert_eq!(replay(CHECKPOINT_ONLINE, &payload, Lsn(4096))?, ckpt);
        assert_eq!(replay(CHECKPOINT_SHUTDOWN, &payload, Lsn(64))?, ckpt);

        let Err(err) = replay(0x7F, &payload, Lsn(4096)) else {
            anyhow::bail!("an unknown info byte should fail");
        };
        assert!(err.to_string().contains("info byte"), "{err}");

        let Err(err) = replay(CHECKPOINT_ONLINE, &payload[..8], Lsn(4096)) else {
            anyhow::bail!("a truncated payload should fail");
        };
        assert!(err.to_string().contains("unreadable"), "{err}");

        // The redo point may equal the record's own start (a checkpoint on a
        // quiet system samples exactly where its record lands) but never exceed
        // it.
        let Err(err) = replay(CHECKPOINT_ONLINE, &payload, Lsn(63)) else {
            anyhow::bail!("a redo point above the record should fail");
        };
        assert!(err.to_string().contains("above itself"), "{err}");

        Ok(())
    }
}
