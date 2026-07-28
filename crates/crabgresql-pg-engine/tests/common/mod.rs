//! Shared helpers for the durable-engine integration tests: open/reopen an
//! engine over a data directory (the "crash = drop without checkpoint, then
//! reopen" idiom), and mutate on-disk files to simulate torn/corrupt media.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crabgresql_pg_engine::{PgEngine, RelFileNode};
use crabgresql_txn::{CommitSink, TransactionManager, TxnFinalize};
use crabgresql_wal::{Lsn, Wal};

/// Open the engine over `dir`, replaying any existing WAL, applying recovered
/// TRUNCATE swaps, and wiring the finalize hook so commits/aborts drive the
/// relfilenode swap and lock release — the exact production startup sequence.
/// An alias of [`try_open`]; corruption tests that must inspect the failure can
/// call either.
pub fn open(dir: &Path) -> std::io::Result<(Arc<PgEngine>, TransactionManager)> {
    try_open(dir)
}

/// Open the engine, surfacing any recovery error (for corruption tests that must
/// assert recovery fails loudly). Drives the SAME `PgEngine::open_recovered`
/// sequence the server's `open_pg_engine` uses (recover → clamp tail → reconcile
/// truncates → GC → checkpoint), then attaches the finalize hook.
pub fn try_open(dir: &Path) -> std::io::Result<(Arc<PgEngine>, TransactionManager)> {
    let (engine, mut tm) = open_without_finalize(dir)?;
    tm.set_finalize(Arc::clone(&engine) as Arc<dyn TxnFinalize>);
    Ok((engine, tm))
}

/// Open the engine WITHOUT the finalize hook, so a `commit` makes the commit
/// record durable and marks the CLOG but never applies the transaction's
/// relfilenode swap in memory or persists it to the catalog. That is exactly the
/// crash window between the commit fsync and the catalog write, which recovery has
/// to repair from the WAL.
pub fn open_without_finalize(dir: &Path) -> std::io::Result<(Arc<PgEngine>, TransactionManager)> {
    open_from_without_finalize(dir, Lsn::INVALID)
}

/// [`open_without_finalize`], but resuming replay at `redo` instead of the start
/// of the stream — the bounded recovery a real checkpoint will publish. Used to
/// prove a writer's changes survive when the WAL prefix below `redo` is never
/// read.
pub fn open_from_without_finalize(
    dir: &Path,
    redo: Lsn,
) -> std::io::Result<(Arc<PgEngine>, TransactionManager)> {
    let wal = Arc::new(Wal::open(dir).map_err(std::io::Error::other)?);
    let (engine, clog, next_xid) = PgEngine::open_recovered(dir, Arc::clone(&wal), redo)?;
    let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
    Ok((engine, TransactionManager::new_recovered(sink, clog, next_xid)))
}

/// [`open`], but resuming replay at `redo`.
pub fn open_from(dir: &Path, redo: Lsn) -> std::io::Result<(Arc<PgEngine>, TransactionManager)> {
    let (engine, tm, _wal) = open_from_with_wal(dir, redo)?;
    Ok((engine, tm))
}

/// [`open_from`], also handing back the [`Wal`] so a test can drive the
/// checkpoint sequence itself — sample [`Wal::redo_point`], flush, *then*
/// checkpoint — which is the ordering a real checkpointer must use.
pub fn open_from_with_wal(
    dir: &Path,
    redo: Lsn,
) -> std::io::Result<(Arc<PgEngine>, TransactionManager, Arc<Wal>)> {
    let wal = Arc::new(Wal::open(dir).map_err(std::io::Error::other)?);
    let (engine, clog, next_xid) = PgEngine::open_recovered(dir, Arc::clone(&wal), redo)?;
    let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
    let mut tm = TransactionManager::new_recovered(sink, clog, next_xid);
    tm.set_finalize(Arc::clone(&engine) as Arc<dyn TxnFinalize>);
    Ok((engine, tm, wal))
}

/// The WAL stream file, for tests that corrupt or truncate it directly.
pub fn wal_file_path(dir: &Path) -> PathBuf {
    dir.join("pg_wal").join("wal")
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

/// Overwrite `[from, to)` of the file at `path` with `byte`.
///
/// The adversarial half of a bounded-replay test: scribbling the WAL prefix
/// below a redo point proves recovery never read it. If it does read one byte
/// below, the first `decode` fails, the log reads as empty, and everything below
/// the redo point vanishes — so such a test cannot pass by accident.
///
/// Returns `Result` and is propagated with `?` per AGENTS.md; `flip_byte` above
/// predates that rule, so do not copy its `unwrap` style.
pub fn scribble(path: &Path, from: u64, to: u64, byte: u8) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.seek(SeekFrom::Start(from))?;
    let len = to.saturating_sub(from) as usize;
    f.write_all(&vec![byte; len])?;
    f.sync_all()
}

/// Corrupt a data page by flipping a byte well inside its written region (past
/// the page header, before the checksum field), deterministically breaking its
/// CRC so `StorageManager::read` must reject it.
pub fn corrupt_page_byte(dir: &Path, rel: RelFileNode, block: u32) {
    const BLCKSZ: u64 = 8192;
    flip_byte(&relfile_path(dir, rel), block as u64 * BLCKSZ + 100);
}
