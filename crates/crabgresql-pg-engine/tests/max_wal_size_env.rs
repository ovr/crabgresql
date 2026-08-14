//! That WAL volume actually triggers a checkpoint, and at the documented knob.
//!
//! Its own test binary on purpose, like `shared_buffers_env`: setting a variable
//! is `unsafe` under the 2024 edition because it races any other thread reading
//! the environment, and a binary with a single test has no other thread to race.
//!
//! What this covers is the wiring, not the parsing (`crabgresql-config` has unit
//! tests for that): that a long-running process re-checkpoints on its own, which
//! is the only thing keeping crash recovery bounded between a startup and a clean
//! shutdown.

use std::sync::Arc;

use crabgresql_pg_engine::{BufferPoolPolicy, PgEngine};
use crabgresql_storage_api::{Column, TableEngine, TableSchema};
use crabgresql_txn::{CommandId, CommitSink, TransactionManager, TxnFinalize};
use crabgresql_types::{PgType, Value};
use crabgresql_wal::{Lsn, Wal, read_control};

/// Rows wide enough that a few thousand of them log more than one segment, so
/// the test does not spend its time on tuple overhead.
const WIDE: usize = 4096;
/// Rows per transaction. The trigger runs from the commit path either way, and
/// each commit is an fsync — one row per transaction makes this test spend a
/// minute waiting on the disk instead of a second.
const PER_TXN: i32 = 64;

#[test]
fn wal_volume_triggers_a_checkpoint() -> anyhow::Result<()> {
    // SAFETY: this binary holds exactly one test, so no other thread is reading
    // the environment while this is set.
    unsafe {
        std::env::set_var(crabgresql_config::MAX_WAL_SIZE.name, "32MB");
    }
    let threshold = crabgresql_config::MAX_WAL_SIZE.min as u64;

    let dir = tempfile::tempdir()?;
    let wal = Arc::new(Wal::open(dir.path()).map_err(std::io::Error::other)?);
    let (engine, clog, next_xid) = PgEngine::open_recovered_with_pool(
        dir.path(),
        Arc::clone(&wal),
        BufferPoolPolicy::minimal(),
    )?;
    let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
    let mut tm = TransactionManager::new_recovered(sink, clog, next_xid);
    tm.set_finalize(Arc::clone(&engine) as Arc<dyn TxnFinalize>);

    let table = engine.create_table(TableSchema::new(
        "t",
        vec![
            Column::new("id", PgType::Int4),
            Column::new("payload", PgType::Text),
        ],
    ))?;
    let startup = read_control(dir.path())?
        .expect("the startup checkpoint publishes a control file")
        .redo_lsn;

    // Many small transactions rather than one enormous one, so the trigger is
    // exercised from the commit path — which is where it has to work.
    let payload = "x".repeat(WIDE);
    let mut rows = 0;
    while wal.current_lsn().0 < threshold + threshold / 2 {
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        for _ in 0..PER_TXN {
            table.insert(vec![Value::Int4(rows), Value::Text(payload.clone())], &txn)?;
            rows += 1;
        }
        tm.commit(xid)?;
    }

    let control = read_control(dir.path())?.expect("a control file");
    assert!(
        control.redo_lsn > startup,
        "writing {} bytes of WAL past the startup checkpoint at {startup} must have \
         triggered another one, but the redo point is still {}",
        wal.current_lsn().0,
        control.redo_lsn
    );
    // The trigger is a level, not a cap: the redo point trails the insert position
    // by less than the threshold, which is the bound on how much a crash here would
    // have to replay.
    assert!(
        wal.current_lsn().0 - control.redo_lsn.0 <= threshold,
        "the checkpoint left {} bytes to replay, more than the {threshold} asked for",
        wal.current_lsn().0 - control.redo_lsn.0
    );
    assert_ne!(control.redo_lsn, Lsn::INVALID, "and it is a bounded one");

    TableEngine::shutdown(engine.as_ref());

    Ok(())
}
