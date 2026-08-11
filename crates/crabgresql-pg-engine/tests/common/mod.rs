//! Shared helpers for the durable-engine integration tests: open/reopen an
//! engine over a data directory (the "crash = drop without checkpoint, then
//! reopen" idiom), and mutate on-disk files to simulate torn/corrupt media.
//!
//! This module is compiled into each integration-test binary separately, so any
//! helper a given binary does not call looks unused there.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crabgresql_pg_engine::{BufferPoolPolicy, PgEngine, RelFileNode};
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
    let wal = Arc::new(Wal::open(dir).map_err(std::io::Error::other)?);
    // Deliberately the no-LSN entry point, so these helpers keep matching what the
    // server does: they resume from whatever redo point the last checkpoint
    // published, rather than always replaying the whole stream.
    let (engine, clog, next_xid) =
        PgEngine::open_recovered_with_pool(dir, Arc::clone(&wal), BufferPoolPolicy::minimal())?;
    let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
    Ok((
        engine,
        TransactionManager::new_recovered(sink, clog, next_xid),
    ))
}

/// [`open_without_finalize`], but resuming replay at an explicit `redo` instead of
/// the one `pg_control` names. Used to prove a writer's changes survive when the
/// WAL prefix below `redo` is never read, and — with [`Lsn::INVALID`] — to force a
/// whole-stream replay regardless of what the last checkpoint managed to bound.
pub fn open_from_without_finalize(
    dir: &Path,
    redo: Lsn,
) -> std::io::Result<(Arc<PgEngine>, TransactionManager)> {
    let wal = Arc::new(Wal::open(dir).map_err(std::io::Error::other)?);
    let (engine, clog, next_xid) = PgEngine::open_recovered_from_with_pool(
        dir,
        Arc::clone(&wal),
        redo,
        BufferPoolPolicy::minimal(),
    )?;
    let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
    Ok((
        engine,
        TransactionManager::new_recovered(sink, clog, next_xid),
    ))
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
    let (engine, clog, next_xid) = PgEngine::open_recovered_from_with_pool(
        dir,
        Arc::clone(&wal),
        redo,
        BufferPoolPolicy::minimal(),
    )?;
    let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
    let mut tm = TransactionManager::new_recovered(sink, clog, next_xid);
    tm.set_finalize(Arc::clone(&engine) as Arc<dyn TxnFinalize>);
    Ok((engine, tm, wal))
}

/// Destroy every WAL page below the one `redo` sits on, so a replay that read
/// the prefix would fail rather than quietly succeed.
///
/// Works in *stream* positions and defers to the WAL crate for the mapping onto
/// segment files: a test that scribbled a stale path would silently become a
/// no-op and still pass.
///
/// It stops at `redo`'s own page rather than at `redo`, and that boundary is
/// load-bearing. Destroying that page's header would make recovery refuse to
/// start on a *page* fault — which is a real error, but not the property these
/// tests exist to prove. What they prove is that the bytes below the redo point
/// are never read.
pub fn scribble_wal_below(dir: &Path, redo: Lsn) -> std::io::Result<()> {
    let stop = Lsn(crabgresql_wal::page_start(redo.0));
    crabgresql_wal::scribble(dir, Lsn::INVALID, stop, 0xAB).map_err(std::io::Error::other)
}

/// The on-disk path of a relation's data file: `<dir>/base/<relfilenode>`.
pub fn relfile_path(dir: &Path, rel: RelFileNode) -> PathBuf {
    dir.join("base").join(rel.0.to_string())
}

/// Flip one byte at `offset` in the file at `path` (read-modify-write) to
/// simulate a torn/corrupt write.
pub fn flip_byte(path: &Path, offset: u64) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut b = [0u8; 1];
    f.read_exact(&mut b)?;
    b[0] ^= 0xFF;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(&b)?;
    f.sync_all()
}

/// Overwrite `[from, to)` of the file at `path` with `byte`.
///
/// The adversarial half of a bounded-replay test: scribbling the WAL prefix
/// below a redo point proves recovery never read it. If it does read one byte
/// below, the first `decode` fails, the log reads as empty, and everything below
/// the redo point vanishes — so such a test cannot pass by accident.
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
pub fn corrupt_page_byte(dir: &Path, rel: RelFileNode, block: u32) -> std::io::Result<()> {
    const BLCKSZ: u64 = 8192;
    flip_byte(&relfile_path(dir, rel), block as u64 * BLCKSZ + 100)
}
