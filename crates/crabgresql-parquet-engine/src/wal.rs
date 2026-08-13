//! This engine's WAL records: the XID-observed marker and the directory-swap
//! TRUNCATE, plus the redo handler that replays them.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crabgresql_txn::{LockOwner, Xid};
use crabgresql_wal::{RedoContext, RmgrId, RmgrRedo, WalError};

pub const RMGR_PARQUET: RmgrId = RmgrId(12);
pub const PARQUET_XID_OBSERVED: u8 = 1;
/// A directory-swap TRUNCATE: see `encode_truncate` for the payload.
pub const PARQUET_TRUNCATE: u8 = 2;

/// A committed TRUNCATE's applied directory swap, handed to the engine so it can
/// persist the new relfilenode and then release the table's exclusive hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParquetSwap {
    pub new_rel: u32,
    pub owner: LockOwner,
}

/// A directory-swap TRUNCATE replayed from the WAL, awaiting the CLOG's verdict on
/// its transaction. Collected by [`ParquetRedo`] and drained by the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredParquetTruncate {
    pub xid: Xid,
    pub namespace: String,
    pub name: String,
    pub old: u32,
    pub new: u32,
}

/// Encode a [`PARQUET_TRUNCATE`] payload: the relation's old (still-live) and new
/// (staged, empty) fragment directories, plus the relation's schema-qualified name
/// so recovery can rebind the catalog once it knows the transaction's fate. Layout
/// `[old:u32][new:u32][ns_len:u32][ns][name_len:u32][name]`, little-endian.
pub(crate) fn encode_truncate(namespace: &str, name: &str, old: u32, new: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&old.to_le_bytes());
    out.extend_from_slice(&new.to_le_bytes());
    for text in [namespace, name] {
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
    }
    out
}

fn decode_truncate(xid: Xid, payload: &[u8]) -> Result<RecoveredParquetTruncate, WalError> {
    let bad = || WalError::Redo("parquet truncate record: truncated payload".to_string());
    let u32_at = |offset: usize| -> Result<u32, WalError> {
        payload
            .get(offset..offset + 4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().expect("4 bytes")))
            .ok_or_else(bad)
    };
    let old = u32_at(0)?;
    let new = u32_at(4)?;
    let mut at = 8;
    let mut text = || -> Result<String, WalError> {
        let len = u32_at(at)? as usize;
        at += 4;
        let bytes = payload.get(at..at + len).ok_or_else(bad)?;
        at += len;
        String::from_utf8(bytes.to_vec())
            .map_err(|e| WalError::Redo(format!("parquet truncate record: bad name: {e}")))
    };
    let namespace = text()?;
    let name = text()?;
    Ok(RecoveredParquetTruncate {
        xid,
        namespace,
        name,
        old,
        new,
    })
}

/// Replays Parquet WAL records. A [`PARQUET_XID_OBSERVED`] record exists only so
/// the XID allocator observes the transaction — fragment bytes were fsynced before
/// commit and pending-file promotion is reconciled separately. A
/// [`PARQUET_TRUNCATE`] record additionally materializes the staged directory (so
/// the same transaction's later inserts have somewhere to have landed) and records
/// the swap for the engine to resolve once the CLOG is rebuilt.
pub struct ParquetRedo {
    root: PathBuf,
    recovered: Mutex<Vec<RecoveredParquetTruncate>>,
}

impl ParquetRedo {
    pub fn new(data_dir: &Path) -> ParquetRedo {
        ParquetRedo {
            root: data_dir.to_path_buf(),
            recovered: Mutex::new(Vec::new()),
        }
    }

    /// Drain the swaps seen during replay, in WAL order.
    pub fn take_recovered(&self) -> Vec<RecoveredParquetTruncate> {
        std::mem::take(
            &mut *self
                .recovered
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned")),
        )
    }
}

impl RmgrRedo for ParquetRedo {
    fn redo(&self, ctx: &RedoContext) -> Result<(), WalError> {
        match ctx.info {
            PARQUET_XID_OBSERVED if ctx.payload.is_empty() => Ok(()),
            PARQUET_TRUNCATE => {
                let record = decode_truncate(ctx.xid, ctx.payload)?;
                let dir = self.root.join("parquet").join(record.new.to_string());
                std::fs::create_dir_all(&dir).map_err(|error| {
                    WalError::Redo(format!(
                        "create Parquet directory {}: {error}",
                        dir.display()
                    ))
                })?;
                self.recovered
                    .lock()
                    .unwrap_or_else(|_| panic!("mutex poisoned"))
                    .push(record);
                Ok(())
            }
            other => Err(WalError::Redo(format!(
                "unknown parquet WAL record info byte {other:#x}"
            ))),
        }
    }
}
