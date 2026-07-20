//! Shared helpers for the durable-engine integration tests: open/reopen an
//! engine over a data directory (the "crash = drop without checkpoint, then
//! reopen" idiom), and mutate on-disk files to simulate torn/corrupt media.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crabgresql_pg_engine::{PgEngine, RelFileNode};
use crabgresql_txn::{Clog, CommitSink, TransactionManager, TxnFinalize};
use crabgresql_wal::{RmgrRegistry, Wal, recover};

/// Open the engine over `dir`, replaying any existing WAL, applying recovered
/// TRUNCATE swaps, and wiring the finalize hook so commits/aborts drive the
/// relfilenode swap and lock release — the same sequence as the production
/// `open_pg_engine`. An alias of [`try_open`]; corruption tests that must
/// inspect the failure can call either.
pub fn open(dir: &Path) -> std::io::Result<(Arc<PgEngine>, TransactionManager)> {
    try_open(dir)
}

/// Open the engine, surfacing any recovery error (for corruption tests that must
/// assert recovery fails loudly).
pub fn try_open(dir: &Path) -> std::io::Result<(Arc<PgEngine>, TransactionManager)> {
    let wal = Arc::new(Wal::open(dir).map_err(std::io::Error::other)?);
    let mut reg = RmgrRegistry::new();
    let engine = Arc::new(PgEngine::new(dir, Arc::clone(&wal), &mut reg)?);
    let clog = Arc::new(Clog::new());
    let res = recover(dir, &reg, &clog).map_err(std::io::Error::other)?;
    // Mirror the production startup (open_pg_engine): clamp a torn tail, resolve
    // recovered truncate swaps, then make the manager finalize-aware.
    wal.reset_to(res.end_of_wal).map_err(std::io::Error::other)?;
    engine.apply_recovered_truncates(&clog);
    let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
    let mut tm = TransactionManager::new_recovered(sink, clog, res.next_xid);
    tm.set_finalize(Arc::clone(&engine) as Arc<dyn TxnFinalize>);
    Ok((engine, tm))
}

/// The on-disk path of a relation's data file: `<dir>/base/<relfilenode>`.
pub fn relfile_path(dir: &Path, rel: RelFileNode) -> PathBuf {
    dir.join("base").join(rel.0.to_string())
}

/// Flip one byte at `offset` in the file at `path` (read-modify-write) to
/// simulate a torn/corrupt write.
pub fn flip_byte(path: &Path, offset: u64) {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    f.seek(SeekFrom::Start(offset)).unwrap();
    let mut b = [0u8; 1];
    f.read_exact(&mut b).unwrap();
    b[0] ^= 0xFF;
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.write_all(&b).unwrap();
    f.sync_all().unwrap();
}

/// Corrupt a data page by flipping a byte well inside its written region (past
/// the page header, before the checksum field), deterministically breaking its
/// CRC so `StorageManager::read` must reject it.
pub fn corrupt_page_byte(dir: &Path, rel: RelFileNode, block: u32) {
    const BLCKSZ: u64 = 8192;
    flip_byte(&relfile_path(dir, rel), block as u64 * BLCKSZ + 100);
}
