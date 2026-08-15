//! A checkpoint retires the WAL segments its redo point made dead, and the
//! cluster still comes back up afterwards.
//!
//! Its own test binary for the same reason as `max_wal_size_env`: driving a
//! checkpoint from WAL volume needs `CRABGRESQL_MAX_WAL_SIZE` set, and setting a
//! variable is `unsafe` under the 2024 edition because it races any other thread
//! reading the environment. A binary with a single test has no other thread.
//!
//! The reopen at the end is the point of the test, not a flourish. Removing one
//! segment too many is invisible while the server is running — nothing reads the
//! retired log — and shows up only as a cluster that will not start, because
//! recovery resumes at a segment that is gone.

use std::sync::Arc;

use crabgresql_pg_engine::{BufferPoolPolicy, PgEngine};
use crabgresql_storage_api::{Column, ColumnProjection, TableEngine, TableSchema};
use crabgresql_txn::{CommandId, CommitSink, TransactionManager, TxnContext, TxnFinalize, Xid};
use crabgresql_types::{PgType, Value};
use crabgresql_wal::{Lsn, SEGMENT_SIZE, Wal, read_control, segment_numbers, segment_of};

/// Rows wide enough that a few thousand of them log more than one segment.
const WIDE: usize = 4096;
/// Rows per transaction: every commit is an fsync, so one row per transaction
/// would make this test disk-bound for a minute.
const PER_TXN: i32 = 64;

#[test]
fn a_checkpoint_retires_the_segments_below_its_redo_point() -> anyhow::Result<()> {
    // SAFETY: this binary holds exactly one test, so no other thread is reading
    // the environment while this is set.
    unsafe {
        std::env::set_var(crabgresql_config::MAX_WAL_SIZE.name, "32MB");
    }
    let threshold = crabgresql_config::MAX_WAL_SIZE.min as u64;
    assert_eq!(
        threshold, SEGMENT_SIZE,
        "the test assumes a checkpoint every segment"
    );

    let dir = tempfile::tempdir()?;
    let rows = {
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

        // Past the second segment boundary, so the checkpoint the volume triggers
        // publishes a redo point above segment 0 and there is something to retire.
        let payload = "x".repeat(WIDE);
        let mut rows = 0;
        while wal.current_lsn().0 < 2 * SEGMENT_SIZE + threshold / 2 {
            let xid = tm.allocate_xid();
            let txn = tm.context(xid, CommandId::FIRST);
            for _ in 0..PER_TXN {
                table.insert(vec![Value::Int4(rows), Value::Text(payload.clone())], &txn)?;
                rows += 1;
            }
            tm.commit(xid)?;
        }

        let control = read_control(dir.path())?.expect("a control file");
        assert_ne!(
            control.redo_lsn,
            Lsn::INVALID,
            "this cluster has nothing that clamps the redo point"
        );
        let redo_segment = segment_of(control.redo_lsn);
        assert!(
            redo_segment > 0,
            "writing {} bytes must have moved the redo point past segment 0, but it \
             is at {}",
            wal.current_lsn().0,
            control.redo_lsn
        );
        // Nothing below the redo point survives — `wal_keep_size` defaults to zero —
        // and the segment holding it does, because a redo point is a byte inside a
        // segment that replay must read in full.
        let segments = segment_numbers(dir.path())?;
        assert_eq!(
            segments.first().copied(),
            Some(redo_segment),
            "the oldest segment left must be the one holding the redo point {}; \
             segments: {segments:?}",
            control.redo_lsn
        );

        TableEngine::shutdown(engine.as_ref());
        rows
    };

    // The cluster comes back up from a `pg_wal` missing its retired prefix, with
    // every committed row still there.
    let wal = Arc::new(Wal::open(dir.path()).map_err(std::io::Error::other)?);
    let (engine, clog, next_xid) = PgEngine::open_recovered_with_pool(
        dir.path(),
        Arc::clone(&wal),
        BufferPoolPolicy::minimal(),
    )?;
    let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
    let tm = TransactionManager::new_recovered(sink, clog, next_xid);
    let table = engine.open_table("t")?;
    let read: TxnContext = tm.context(Xid::INVALID, CommandId::FIRST);
    let recovered = table.scan(&read, &ColumnProjection::All).count();
    assert_eq!(
        recovered, rows as usize,
        "every committed row must survive the retirement"
    );

    // And now the combination that has nothing to do with corruption: a
    // checkpoint that cannot bound replay publishes `Lsn::INVALID` over a log
    // whose oldest segments are already gone. Read as "resume at segment 0" that
    // is a data directory nobody can start — the whole stream means the whole
    // stream still on disk.
    let clamped = {
        let mut registry = crabgresql_wal::RmgrRegistry::new();
        // No commit log, so `redo_clamp` fires for the simplest of its reasons.
        // A resident write buffer reaches the same published value by the same
        // path.
        let engine = PgEngine::new_with_pool(
            dir.path(),
            Arc::clone(&wal),
            &mut registry,
            BufferPoolPolicy::minimal(),
        )?;
        engine.checkpoint(Xid::FIRST_NORMAL)?;
        read_control(dir.path())?.expect("a control file").redo_lsn
    };
    assert_eq!(clamped, Lsn::INVALID, "the checkpoint must have clamped");
    assert!(
        !segment_numbers(dir.path())?.contains(&0),
        "and segment 0 is still gone"
    );

    drop(engine);
    let wal = Arc::new(Wal::open(dir.path()).map_err(std::io::Error::other)?);
    let (engine, clog, next_xid) = PgEngine::open_recovered_with_pool(
        dir.path(),
        Arc::clone(&wal),
        BufferPoolPolicy::minimal(),
    )?;
    let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
    let tm = TransactionManager::new_recovered(sink, clog, next_xid);
    let table = engine.open_table("t")?;
    let read: TxnContext = tm.context(Xid::INVALID, CommandId::FIRST);
    assert_eq!(
        table.scan(&read, &ColumnProjection::All).count(),
        rows as usize,
        "a whole-stream replay of a retired log must still find every row"
    );

    Ok(())
}
