//! Crash recovery: bring the engine up on a temp dir, write a mix of committed,
//! uncommitted and aborted work, "crash" by dropping everything without a
//! checkpoint, then reopen and prove that redo-only recovery restores exactly
//! the committed state.

use std::sync::Arc;

use crabgresql_pg_engine::{PgEngine, RelFileNode};
use crabgresql_storage_api::{Column, ColumnProjection, TableAm, TableEngine, TableSchema, Tid};
use crabgresql_txn::{Clog, CommandId, CommitSink, TransactionManager, TxnContext, Xid};
use crabgresql_types::{PgType, Value};
use crabgresql_wal::{RmgrRegistry, Wal, recover};

mod common;
use common::{corrupt_page_byte, open, try_open};

fn schema() -> TableSchema {
    TableSchema::new(
        "t",
        vec![
            Column::new("id", PgType::Int4),
            Column::new("name", PgType::Text),
        ],
    )
}

fn read(tm: &TransactionManager) -> TxnContext {
    tm.context(Xid::INVALID, CommandId::FIRST)
}

fn visible_ids(tm: &TransactionManager, table: &dyn TableAm) -> Vec<i32> {
    let mut v: Vec<i32> = table
        .scan(&read(tm), &ColumnProjection::All)
        .map(|row| match row.unwrap_or_else(|error| panic!("scan failed: {error}")).1[0] {
            Value::Int4(x) => x,
            _ => unreachable!(),
        })
        .collect();
    v.sort();
    v
}

fn tid_of(tm: &TransactionManager, table: &dyn TableAm, id: i32) -> Tid {
    table
        .scan(&read(tm), &ColumnProjection::All)
        .map(|row| row.unwrap_or_else(|error| panic!("scan failed: {error}")))
        .find(|(_, t)| t[0] == Value::Int4(id))
        .map(|(tid, _)| tid)
        .unwrap_or_else(|| panic!("expected visible tuple with id {id}"))
}

fn insert(table: &dyn TableAm, txn: &TxnContext, id: i32, name: &str) -> Tid {
    table
        .insert(vec![Value::Int4(id), Value::Text(name.into())], txn)
        .unwrap_or_else(|error| panic!("insert failed: {error}"))
}

#[test]
fn committed_survives_uncommitted_and_aborted_vanish() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;

    // --- lifetime 1: do work, then "crash" (drop without a checkpoint). ---
    let (tid_two, xid_high);
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.create_table(schema())?;

        // A: committed inserts.
        let xa = tm.allocate_xid();
        insert(&*table, &tm.context(xa, CommandId::FIRST), 1, "a1");
        insert(&*table, &tm.context(xa, CommandId::FIRST), 2, "a2");
        tm.commit(xa)?;

        tid_two = tid_of(&tm, &*table, 2);

        // B: an insert left uncommitted (in flight at crash).
        let xb = tm.allocate_xid();
        insert(&*table, &tm.context(xb, CommandId::FIRST), 3, "b");

        // C: an insert that aborts.
        let xc = tm.allocate_xid();
        insert(&*table, &tm.context(xc, CommandId::FIRST), 4, "c");
        tm.abort(xc);

        // G: a committed UPDATE of row id=2 -> id=20 (exercises redo of the
        // new-version insert + the old-version stamp).
        let xg = tm.allocate_xid();
        table.update(
            tid_two,
            vec![Value::Int4(20), Value::Text("a2u".into())],
            &tm.context(xg, CommandId::FIRST),
        )?;
        tm.commit(xg)?;

        // F: a final committed insert. Its commit fsync also makes B's and C's
        // records durable, so recovery must reason about them explicitly.
        let xf = tm.allocate_xid();
        insert(&*table, &tm.context(xf, CommandId::FIRST), 7, "f");
        tm.commit(xf)?;
        xid_high = xf;

        // Sanity within lifetime 1.
        assert_eq!(visible_ids(&tm, &*table), vec![1, 7, 20]);
        // Drop engine + tm here: buffers vanish, only fsynced WAL remains.
    }

    // --- lifetime 2: recover and verify. ---
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.open_table("t")?;
        // Committed A (1), the committed update (20), and F (7) survive; the
        // uncommitted B (3) and aborted C (4) are gone.
        assert_eq!(visible_ids(&tm, &*table), vec![1, 7, 20]);
        // The XID allocator resumes above every recovered transaction.
        let next = tm.allocate_xid();
        assert!(
            next > xid_high,
            "next XID {next:?} must exceed recovered {xid_high:?}"
        );
        // The recovered engine is fully usable: a new committed insert appears.
        let xn = tm.allocate_xid();
        insert(&*table, &tm.context(xn, CommandId::FIRST), 99, "new");
        tm.commit(xn)?;
        assert_eq!(visible_ids(&tm, &*table), vec![1, 7, 20, 99]);
    }

    Ok(())
}

#[test]
fn replaying_the_same_wal_twice_is_idempotent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.create_table(schema())?;
        for i in 0..50 {
            let x = tm.allocate_xid();
            insert(&*table, &tm.context(x, CommandId::FIRST), i, "row");
            tm.commit(x)?;
        }
    }
    // First recovery, then force pages to disk (advancing their pd_lsn), then a
    // second recovery over the same WAL: the LSN gate must make it a no-op.
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    assert_eq!(visible_ids(&tm, &*table).len(), 50);
    engine.checkpoint(Xid::FIRST_NORMAL)?;

    let mut reg = RmgrRegistry::new();
    let wal = Arc::new(Wal::open(dir.path())?);
    let engine2 = PgEngine::new(dir.path(), Arc::clone(&wal), &mut reg)?;
    let clog = Arc::new(Clog::new());
    let res = recover(dir.path(), &reg, &clog)?;
    let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
    let tm2 = TransactionManager::new_recovered(sink, clog, res.next_xid);
    let table2 = engine2.open_table("t")?;
    // Still exactly 50 rows — no duplication from the second replay.
    assert_eq!(visible_ids(&tm2, &*table2).len(), 50);

    Ok(())
}

#[test]
fn writes_survive_across_multiple_restarts() -> anyhow::Result<()> {
    // Regression for the WAL-append-after-reopen corruption: every boot writes
    // and commits, so the WAL is appended to (not just read) after each reopen.
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.create_table(schema())?;
        let x = tm.allocate_xid();
        insert(&*table, &tm.context(x, CommandId::FIRST), 1, "boot1");
        tm.commit(x)?;
    }
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.open_table("t")?;
        assert_eq!(visible_ids(&tm, &*table), vec![1]);
        let x = tm.allocate_xid();
        insert(&*table, &tm.context(x, CommandId::FIRST), 2, "boot2");
        tm.commit(x)?;
    }
    {
        // Third boot: recovery must replay a WAL that was appended to after a
        // reopen — both boots' rows must be present.
        let (engine, tm) = open(dir.path())?;
        let table = engine.open_table("t")?;
        assert_eq!(visible_ids(&tm, &*table), vec![1, 2]);
    }

    Ok(())
}

// --- Transactional TRUNCATE across crash (relfilenode-swap) ---

/// Seed rows 1,2,3 in their own committed transaction on a fresh table.
fn seed_three(engine: &PgEngine, tm: &TransactionManager) -> Arc<dyn TableAm> {
    let table = engine.create_table(schema()).unwrap();
    let x = tm.allocate_xid();
    let ctx = tm.context(x, CommandId::FIRST);
    insert(&*table, &ctx, 1, "a");
    insert(&*table, &ctx, 2, "b");
    insert(&*table, &ctx, 3, "c");
    tm.commit(x).unwrap();
    table
}

#[test]
fn truncate_committed_then_crash_recovers_empty() -> anyhow::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    {
        let (engine, tm) = open(dir.path()).unwrap();
        let table = seed_three(&engine, &tm);
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST))?;
        tm.commit(tx).unwrap();
        assert_eq!(visible_ids(&tm, &*table), Vec::<i32>::new());
        // Crash: drop without checkpoint.
    }
    let (engine, tm) = open(dir.path()).unwrap();
    let table = engine.open_table("t").unwrap();
    assert_eq!(visible_ids(&tm, &*table), Vec::<i32>::new());
    Ok(())
}

#[test]
fn truncate_uncommitted_then_crash_restores_rows() -> anyhow::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    {
        let (engine, tm) = open(dir.path()).unwrap();
        let table = seed_three(&engine, &tm);
        let tx = tm.allocate_xid();
        let ctx = tm.context(tx, CommandId::FIRST);
        table.truncate(&ctx)?;
        // Read-your-own-truncate: the truncater sees its own now-empty table
        // (reading under its OWN xid; a concurrent reader would block on the
        // AccessExclusive lock until this transaction ends).
        let own: Vec<i32> = table.scan(&ctx, &ColumnProjection::All).map(|row| match row
            .unwrap_or_else(|error| panic!("scan failed: {error}")).1[0] {
            Value::Int4(x) => x,
            _ => unreachable!(),
        }).collect();
        assert_eq!(own, Vec::<i32>::new());
        // Never commits: crash (drop) with the swap still pending.
    }
    // Recovery keeps the original file because the swap's XID never committed.
    let (engine, tm) = open(dir.path()).unwrap();
    let table = engine.open_table("t").unwrap();
    assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3]);
    Ok(())
}

#[test]
fn truncate_rolled_back_restores_rows_in_place_and_after_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let (engine, tm) = open(dir.path()).unwrap();
    let table = seed_three(&engine, &tm);
    let tx = tm.allocate_xid();
    table.truncate(&tm.context(tx, CommandId::FIRST))?;
    tm.abort(tx);
    // Explicit abort restores the rows immediately (the old file was untouched).
    assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3]);
    drop((engine, tm));
    let (engine, tm) = open(dir.path()).unwrap();
    let table = engine.open_table("t").unwrap();
    assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3]);
    Ok(())
}

#[test]
fn truncate_then_insert_then_commit_crash_keeps_only_new_rows() -> anyhow::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    {
        let (engine, tm) = open(dir.path()).unwrap();
        let table = seed_three(&engine, &tm);
        let tx = tm.allocate_xid();
        let ctx = tm.context(tx, CommandId::FIRST);
        table.truncate(&ctx)?;
        // Post-truncate inserts land in the new file (read-your-own-truncate).
        insert(&*table, &ctx, 10, "x");
        insert(&*table, &ctx, 11, "y");
        tm.commit(tx).unwrap();
        assert_eq!(visible_ids(&tm, &*table), vec![10, 11]);
    }
    let (engine, tm) = open(dir.path()).unwrap();
    let table = engine.open_table("t").unwrap();
    assert_eq!(visible_ids(&tm, &*table), vec![10, 11]);
    Ok(())
}

#[test]
fn truncate_crash_then_truncate_again_commit_is_consistent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    {
        let (engine, tm) = open(dir.path()).unwrap();
        let table = seed_three(&engine, &tm);
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST))?;
        // Crash before commit.
    }
    {
        // Rows are back; truncate again and commit this time.
        let (engine, tm) = open(dir.path()).unwrap();
        let table = engine.open_table("t").unwrap();
        assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3]);
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST))?;
        tm.commit(tx).unwrap();
        assert_eq!(visible_ids(&tm, &*table), Vec::<i32>::new());
    }
    // Final restart: empty, and the recovered engine still accepts writes.
    let (engine, tm) = open(dir.path()).unwrap();
    let table = engine.open_table("t").unwrap();
    assert_eq!(visible_ids(&tm, &*table), Vec::<i32>::new());
    let x = tm.allocate_xid();
    insert(&*table, &tm.context(x, CommandId::FIRST), 42, "z");
    tm.commit(x).unwrap();
    assert_eq!(visible_ids(&tm, &*table), vec![42]);
    Ok(())
}

// --- Corruption & checkpoint interactions ---

#[test]
fn corrupted_data_page_fails_recovery_loudly() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (engine, tm) = open(dir.path()).unwrap();
        let table = seed_three(&engine, &tm);
        let _ = table;
        // Flush the rows to disk so the page carries a checksum we can break.
        engine.checkpoint(Xid::FIRST_NORMAL).unwrap();
    }
    // The first (and only) user table is relfilenode 1.
    corrupt_page_byte(dir.path(), RelFileNode(1), 0);
    let err = match try_open(dir.path()) {
        Ok(_) => panic!("recovery must reject a corrupt page, not succeed"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("checksum"),
        "expected a checksum error, got: {err}"
    );
}

#[test]
fn checkpoint_then_more_writes_then_crash_recovers_all() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (engine, tm) = open(dir.path()).unwrap();
        let table = engine.create_table(schema()).unwrap();
        let x = tm.allocate_xid();
        let ctx = tm.context(x, CommandId::FIRST);
        insert(&*table, &ctx, 1, "a");
        insert(&*table, &ctx, 2, "b");
        tm.commit(x).unwrap();
        // Make rows 1,2 durable on their pages.
        engine.checkpoint(Xid::FIRST_NORMAL).unwrap();
        // Post-checkpoint committed writes live only in the WAL until replay.
        let y = tm.allocate_xid();
        let ctx = tm.context(y, CommandId::FIRST);
        insert(&*table, &ctx, 3, "c");
        insert(&*table, &ctx, 4, "d");
        tm.commit(y).unwrap();
        // Crash.
    }
    let (engine, tm) = open(dir.path()).unwrap();
    let table = engine.open_table("t").unwrap();
    assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3, 4]);
}

#[test]
fn interleaved_committed_and_in_flight_txns_recover_committed_only() -> anyhow::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    {
        let (engine, tm) = open(dir.path()).unwrap();
        let table = engine.create_table(schema()).unwrap();
        // Seed a committed base row.
        let base = tm.allocate_xid();
        let base_tid = insert(&*table, &tm.context(base, CommandId::FIRST), 1, "base");
        tm.commit(base).unwrap();
        // Two overlapping transactions: allocate both up front, interleave.
        let xa = tm.allocate_xid();
        let xb = tm.allocate_xid();
        let ca = tm.context(xa, CommandId::FIRST);
        let cb = tm.context(xb, CommandId::FIRST);
        insert(&*table, &ca, 2, "a-ins");
        insert(&*table, &cb, 3, "b-ins");
        // xa updates the base row (delete old + insert new); xb does not touch it.
        table.update(base_tid, vec![Value::Int4(10), Value::Text("a-upd".into())], &ca)?;
        // Commit xa; leave xb in flight, then commit an unrelated row to force
        // xb's records durable, so recovery must reason about them explicitly.
        tm.commit(xa).unwrap();
        let xf = tm.allocate_xid();
        insert(&*table, &tm.context(xf, CommandId::FIRST), 99, "f");
        tm.commit(xf).unwrap();
        // Crash with xb still uncommitted.
    }
    let (engine, tm) = open(dir.path()).unwrap();
    let table = engine.open_table("t").unwrap();
    // xa's insert (2) and update (1->10) survive; xb's insert (3) vanishes; 99 too.
    assert_eq!(visible_ids(&tm, &*table), vec![2, 10, 99]);
    Ok(())
}

#[test]
fn committed_truncate_then_uncommitted_truncate_crash_keeps_committed_rows() -> anyhow::Result<()> {
    // Regression: a committed TRUNCATE followed by an uncommitted one must NOT be
    // judged by the last record's fate. The committed truncate's file is live and
    // must survive; only the uncommitted truncate is discarded.
    let dir = tempfile::tempdir().unwrap();
    {
        let (engine, tm) = open(dir.path()).unwrap();
        let table = seed_three(&engine, &tm);
        // Txn A: truncate away the seed rows and insert 10, 11, then COMMIT.
        let a = tm.allocate_xid();
        let ca = tm.context(a, CommandId::FIRST);
        table.truncate(&ca)?;
        insert(&*table, &ca, 10, "x");
        insert(&*table, &ca, 11, "y");
        tm.commit(a).unwrap();
        assert_eq!(visible_ids(&tm, &*table), vec![10, 11]);
        // Txn B: truncate again but never commit; crash with the swap pending.
        let b = tm.allocate_xid();
        table.truncate(&tm.context(b, CommandId::FIRST))?;
    }
    // Recovery must keep A's committed rows (10, 11), not delete A's live file
    // because B's later truncate was uncommitted.
    let (engine, tm) = open(dir.path()).unwrap();
    let table = engine.open_table("t").unwrap();
    assert_eq!(visible_ids(&tm, &*table), vec![10, 11]);
    Ok(())
}

#[test]
fn size_derived_statistics_survive_a_crash() -> anyhow::Result<()> {
    // Statistics are not WAL-logged, but the size-derived estimate is read back
    // from the relation file itself — so it must come back after a crash without
    // anything having replayed it.
    let dir = tempfile::tempdir()?;
    let before;
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.create_table(schema())?;
        for i in 0..500 {
            let xid = tm.allocate_xid();
            insert(&*table, &tm.context(xid, CommandId::FIRST), i, "padding");
            tm.commit(xid)?;
        }
        before = table.statistics();
        assert!(before.relpages > 0, "{before:?}");
        // Drop without a checkpoint: only the fsynced WAL survives.
    }

    let (engine, _tm) = open(dir.path())?;
    assert_eq!(engine.open_table("t")?.statistics(), before);

    Ok(())
}

#[test]
fn analyze_results_survive_a_crash_without_being_wal_logged() -> anyhow::Result<()> {
    // Statistics live in the relation catalog, which is fsynced directly rather
    // than replayed from the WAL. This proves the tail alone carries them across
    // a crash — nothing redoes an ANALYZE.
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.create_table(schema())?;
        for i in 0..40 {
            let xid = tm.allocate_xid();
            insert(&*table, &tm.context(xid, CommandId::FIRST), i, "row");
            tm.commit(xid)?;
        }
        let xid = tm.allocate_xid();
        engine.analyze("public", "t", &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        let stats = table.statistics();
        assert!(stats.analyzed);
        assert_eq!(stats.reltuples, 40.0);
        // Drop without a checkpoint.
    }

    let (engine, _tm) = open(dir.path())?;
    let stats = engine.open_table("t")?.statistics();
    assert!(
        stats.analyzed,
        "the reopened relation must still know it was analyzed: {stats:?}"
    );
    assert_eq!(stats.reltuples, 40.0);

    Ok(())
}

fn parquet_schema(name: &str) -> TableSchema {
    let mut schema = TableSchema::new(name, vec![Column::new("id", PgType::Int4)]);
    schema.access_method = crabgresql_storage_api::TableAccessMethod::Parquet;
    schema
}

/// The single `parquet/<relfilenode>` directory in `dir`.
fn parquet_table_dir(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::read_dir(dir.join("parquet"))
        .expect("parquet root exists")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .next()
        .expect("the parquet table directory exists")
}

#[test]
fn an_unopenable_parquet_relation_does_not_block_startup() -> anyhow::Result<()> {
    // A Parquet relation whose directory the engine cannot make sense of must
    // degrade to "this one table is unavailable", not "the cluster will not
    // boot" — otherwise every heap table in the same data directory becomes
    // unreachable and the offender could never be dropped.
    let dir = tempfile::tempdir()?;
    let table_dir;
    {
        let (engine, tm) = open(dir.path())?;
        let heap = engine.create_table(schema())?;
        let xid = tm.allocate_xid();
        insert(&*heap, &tm.context(xid, CommandId::FIRST), 1, "kept");
        tm.commit(xid)?;

        let events = engine.create_table(parquet_schema("events"))?;
        let xid = tm.allocate_xid();
        events.insert(vec![Value::Int4(9)], &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        table_dir = parquet_table_dir(dir.path());
    }
    // Leave behind a file the fragment-name parser rejects.
    std::fs::write(table_dir.join("not-a-fragment.parquet"), b"garbage")?;

    let (engine, tm) = open(dir.path())?;
    // The heap table is untouched and still readable.
    let heap = engine.open_table("t")?;
    assert_eq!(visible_ids(&tm, &*heap), vec![1]);
    // The broken relation reports as nonexistent rather than taking the engine
    // down, and can still be dropped so the catalog entry goes away.
    assert!(engine.open_table("events").is_err());
    engine.drop_table("public", "events")?;
    drop(tm);
    drop(engine);

    // The next boot reclaims its now-orphaned fragment directory.
    let (_engine, _tm) = open(dir.path())?;
    assert!(
        !table_dir.exists(),
        "orphaned Parquet directory should have been reclaimed: {table_dir:?}"
    );
    Ok(())
}

#[test]
fn an_orphaned_parquet_directory_is_reclaimed_at_startup() -> anyhow::Result<()> {
    // `drop_table` removes the catalog entry before the fragment directory, so a
    // crash or IO error in that window leaves a directory no relation owns. The
    // startup sweep is the Parquet counterpart of `gc_orphan_relfiles`; without
    // it the bytes are stranded forever.
    let dir = tempfile::tempdir()?;
    let live_dir;
    let orphan_dir = dir.path().join("parquet").join("999999");
    {
        let (engine, tm) = open(dir.path())?;
        let events = engine.create_table(parquet_schema("events"))?;
        let xid = tm.allocate_xid();
        events.insert(vec![Value::Int4(1)], &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        live_dir = parquet_table_dir(dir.path());
        // A directory whose relation no longer exists in the catalog.
        std::fs::create_dir_all(&orphan_dir)?;
        std::fs::write(orphan_dir.join("00000001-3-0.parquet"), b"stale")?;
    }

    let (engine, tm) = open(dir.path())?;
    assert!(
        !orphan_dir.exists(),
        "startup should reclaim the orphaned Parquet directory"
    );
    // The live relation is left alone.
    assert!(live_dir.exists());
    assert_eq!(engine.open_table("events")?.scan(&read(&tm), &ColumnProjection::All).count(), 1);
    Ok(())
}

// --- Transactional TRUNCATE across crash (Parquet directory swap) ---

/// Every `parquet/<n>` directory on disk, as relfilenodes.
fn parquet_dirs(dir: &std::path::Path) -> Vec<u32> {
    let mut dirs: Vec<u32> = std::fs::read_dir(dir.join("parquet"))
        .expect("parquet root exists")
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().and_then(|n| n.parse().ok()))
        .collect();
    dirs.sort_unstable();
    dirs
}

/// Seed rows 1,2,3 into a fresh Parquet table in one committed transaction.
fn seed_parquet(
    engine: &PgEngine,
    tm: &TransactionManager,
) -> anyhow::Result<Arc<dyn TableAm>> {
    let table = engine.create_table(parquet_schema("events"))?;
    let xid = tm.allocate_xid();
    let ctx = tm.context(xid, CommandId::FIRST);
    for id in 1..=3 {
        table.insert(vec![Value::Int4(id)], &ctx)?;
    }
    tm.commit(xid)?;
    Ok(table)
}

#[test]
fn parquet_truncate_committed_then_crash_recovers_empty() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_parquet(&engine, &tm)?;
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST))?;
        tm.commit(tx)?;
        assert_eq!(visible_ids(&tm, &*table), Vec::<i32>::new());
        // Crash: drop without checkpoint.
    }
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("events")?;
    assert_eq!(visible_ids(&tm, &*table), Vec::<i32>::new());
    assert_eq!(
        parquet_dirs(dir.path()).len(),
        1,
        "the pre-truncate directory is gone and no staging leftovers remain"
    );
    Ok(())
}

#[test]
fn parquet_truncate_uncommitted_then_crash_restores_rows() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_parquet(&engine, &tm)?;
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST))?;
        // Never commits: crash with the swap staged.
    }
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("events")?;
    assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3]);
    assert_eq!(
        parquet_dirs(dir.path()).len(),
        1,
        "the staged directory of an unresolved transaction is reclaimed at startup"
    );
    Ok(())
}

#[test]
fn parquet_truncate_rolled_back_restores_rows_in_place_and_after_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = seed_parquet(&engine, &tm)?;
    let tx = tm.allocate_xid();
    table.truncate(&tm.context(tx, CommandId::FIRST))?;
    tm.abort(tx);
    // The old directory was untouched, so the rows are back immediately.
    assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3]);
    assert_eq!(parquet_dirs(dir.path()).len(), 1);
    drop((engine, tm));
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("events")?;
    assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3]);
    Ok(())
}

#[test]
fn parquet_truncate_then_insert_then_commit_crash_keeps_only_new_rows() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_parquet(&engine, &tm)?;
        let tx = tm.allocate_xid();
        let ctx = tm.context(tx, CommandId::FIRST);
        table.truncate(&ctx)?;
        table.insert(vec![Value::Int4(10)], &tm.context(tx, CommandId(1)))?;
        table.insert(vec![Value::Int4(11)], &tm.context(tx, CommandId(2)))?;
        tm.commit(tx)?;
        assert_eq!(visible_ids(&tm, &*table), vec![10, 11]);
    }
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("events")?;
    assert_eq!(visible_ids(&tm, &*table), vec![10, 11]);
    Ok(())
}

#[test]
fn parquet_truncate_crash_then_truncate_again_commit_is_consistent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_parquet(&engine, &tm)?;
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST))?;
        // Crash before commit.
    }
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.open_table("events")?;
        assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3]);
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST))?;
        tm.commit(tx)?;
        assert_eq!(visible_ids(&tm, &*table), Vec::<i32>::new());
    }
    // Final restart: empty, and the recovered relation still accepts writes into
    // the swapped-in directory.
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("events")?;
    assert_eq!(visible_ids(&tm, &*table), Vec::<i32>::new());
    let xid = tm.allocate_xid();
    table.insert(vec![Value::Int4(42)], &tm.context(xid, CommandId::FIRST))?;
    tm.commit(xid)?;
    assert_eq!(visible_ids(&tm, &*table), vec![42]);
    Ok(())
}

#[test]
fn parquet_committed_truncate_then_uncommitted_truncate_crash_keeps_committed_rows()
-> anyhow::Result<()> {
    // A committed swap followed by an uncommitted one must NOT be judged by the
    // last record's fate: the committed truncate's directory is live and must
    // survive, only the uncommitted one is discarded.
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_parquet(&engine, &tm)?;
        let a = tm.allocate_xid();
        table.truncate(&tm.context(a, CommandId::FIRST))?;
        table.insert(vec![Value::Int4(10)], &tm.context(a, CommandId(1)))?;
        table.insert(vec![Value::Int4(11)], &tm.context(a, CommandId(2)))?;
        tm.commit(a)?;
        assert_eq!(visible_ids(&tm, &*table), vec![10, 11]);
        let b = tm.allocate_xid();
        table.truncate(&tm.context(b, CommandId::FIRST))?;
    }
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("events")?;
    assert_eq!(visible_ids(&tm, &*table), vec![10, 11]);
    assert_eq!(parquet_dirs(dir.path()).len(), 1);
    Ok(())
}

#[test]
fn parquet_committed_truncate_with_a_stale_catalog_is_repaired_at_recovery() -> anyhow::Result<()> {
    // The one window the WAL record exists for: the transaction's commit is
    // durable, but the catalog persist that follows it never happened (a crash
    // between the two). Recovery must re-apply the swap from the WAL, or the
    // truncated rows would come back.
    let dir = tempfile::tempdir()?;
    let staged;
    {
        // No finalize hook: the commit is durable, but nothing applies the swap or
        // persists it to the catalog.
        let (engine, tm) = common::open_without_finalize(dir.path())?;
        let table = seed_parquet(&engine, &tm)?;
        let before = parquet_dirs(dir.path());
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST))?;
        staged = parquet_dirs(dir.path())
            .into_iter()
            .find(|rel| !before.contains(rel))
            .expect("the TRUNCATE staged a directory");
        tm.commit(tx)?;
    }
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("events")?;
    assert_eq!(visible_ids(&tm, &*table), Vec::<i32>::new());
    assert_eq!(
        parquet_dirs(dir.path()),
        vec![staged],
        "recovery must repoint the relation at the staged directory and reclaim the old one"
    );
    // And the repaired relation still writes into the right directory.
    let xid = tm.allocate_xid();
    table.insert(vec![Value::Int4(5)], &tm.context(xid, CommandId::FIRST))?;
    tm.commit(xid)?;
    assert_eq!(visible_ids(&tm, &*table), vec![5]);
    Ok(())
}

#[test]
fn drop_table_reclaims_a_pending_parquet_truncate_directory() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = seed_parquet(&engine, &tm)?;
    let tx = tm.allocate_xid();
    table.truncate(&tm.context(tx, CommandId::FIRST))?;
    assert_eq!(parquet_dirs(dir.path()).len(), 2, "live plus staged");
    engine.drop_table("public", "events")?;
    assert!(
        parquet_dirs(dir.path()).is_empty(),
        "DROP TABLE must reclaim the staged directory too — the catalog never named it"
    );
    Ok(())
}

#[test]
fn a_replayed_truncate_does_not_repoint_a_recreated_relation() -> anyhow::Result<()> {
    // The WAL is replayed from the beginning on every boot and DDL is not logged,
    // so a committed TRUNCATE record can name a relation that was since dropped and
    // re-created under the same name. Applying it would repoint the NEW relation at
    // the old one's dead storage, and the orphan sweep would then delete the live
    // storage — silently emptying a table nobody truncated.
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_parquet(&engine, &tm)?;
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST))?;
        tm.commit(tx)?;
        engine.drop_table("public", "events")?;

        // Same name, brand-new relation and directory.
        let recreated = engine.create_table(parquet_schema("events"))?;
        let xid = tm.allocate_xid();
        recreated.insert(vec![Value::Int4(42)], &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        assert_eq!(visible_ids(&tm, &*recreated), vec![42]);
        // Crash without a checkpoint, so recovery replays the stale truncate.
    }
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("events")?;
    assert_eq!(visible_ids(&tm, &*table), vec![42]);
    Ok(())
}

/// The same hazard for the heap: a committed relfilenode-swap TRUNCATE replayed
/// against a relation that was dropped and re-created under the same name.
#[test]
fn a_replayed_heap_truncate_does_not_repoint_a_recreated_relation() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_three(&engine, &tm);
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST))?;
        tm.commit(tx)?;
        engine.drop_table("public", "t")?;

        let recreated = engine.create_table(schema())?;
        let xid = tm.allocate_xid();
        insert(&*recreated, &tm.context(xid, CommandId::FIRST), 42, "z");
        tm.commit(xid)?;
        assert_eq!(visible_ids(&tm, &*recreated), vec![42]);
    }
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    assert_eq!(visible_ids(&tm, &*table), vec![42]);
    Ok(())
}

#[test]
fn a_committed_truncate_in_a_non_public_schema_is_repaired_at_recovery() -> anyhow::Result<()> {
    // The heap's TRUNCATE record carries the relation's schema, so recovery
    // resolves `app.t` against `app.t`. Assuming `public` would look up a
    // different relation (or none), and the swap would be silently skipped —
    // resurrecting rows a committed TRUNCATE removed.
    let dir = tempfile::tempdir()?;
    {
        // No finalize hook: the commit is durable, but nothing applies the swap or
        // persists it to the catalog — the window the WAL record exists for.
        let (engine, tm) = common::open_without_finalize(dir.path())?;
        engine.create_schema("app")?;
        let mut app_schema = schema();
        app_schema.namespace = "app".to_string();
        let table = engine.create_table(app_schema)?;
        let xid = tm.allocate_xid();
        insert(&*table, &tm.context(xid, CommandId::FIRST), 1, "a");
        insert(&*table, &tm.context(xid, CommandId::FIRST), 2, "b");
        tm.commit(xid)?;

        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST))?;
        tm.commit(tx)?;
    }
    let (engine, tm) = open(dir.path())?;
    let table = engine.resolve(Some("app"), "t")?;
    assert_eq!(visible_ids(&tm, &*table), Vec::<i32>::new());
    Ok(())
}

#[test]
fn recovery_keeps_the_analyze_result_of_a_previously_truncated_relation() -> anyhow::Result<()> {
    // The WAL is replayed from the beginning on every boot, so a relation's old
    // TRUNCATE record is seen again at every restart. Rebinding a handle that is
    // already on the right relation is not free — it drops the ANALYZE result the
    // engine just seeded from the catalog — so recovery must skip it.
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_parquet(&engine, &tm)?;
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST))?;
        tm.commit(tx)?;
        let xid = tm.allocate_xid();
        for id in 1..=4 {
            table.insert(vec![Value::Int4(id)], &tm.context(xid, CommandId::FIRST))?;
        }
        tm.commit(xid)?;
        let xid = tm.allocate_xid();
        engine.analyze("public", "events", &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        assert_eq!(table.statistics().reltuples, 4.0);
    }
    for boot in 1..=2 {
        let (engine, _tm) = open(dir.path())?;
        let stats = engine.open_table("events")?.statistics();
        assert!(
            stats.analyzed,
            "boot {boot}: the replayed TRUNCATE must not discard the statistics: {stats:?}"
        );
        assert_eq!(stats.reltuples, 4.0, "boot {boot}");
    }
    Ok(())
}

#[test]
fn a_parquet_relation_that_cannot_be_rebound_reports_as_absent() -> anyhow::Result<()> {
    // Recovery repoints the relation at the swapped-in directory. If that fails,
    // the catalog names the new directory while the handle still holds the old one
    // — which `gc_orphan_parquet_dirs` deletes moments later. Serving that handle
    // would fail on every access, so the relation must be unregistered instead.
    let dir = tempfile::tempdir()?;
    let staged;
    {
        let (engine, tm) = common::open_without_finalize(dir.path())?;
        let table = seed_parquet(&engine, &tm)?;
        let before = parquet_dirs(dir.path());
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST))?;
        staged = parquet_dirs(dir.path())
            .into_iter()
            .find(|rel| !before.contains(rel))
            .expect("the TRUNCATE staged a directory");
        tm.commit(tx)?;
    }
    // A file the fragment-name parser rejects makes the rebind's `next_block_in`
    // fail for the swapped-in directory.
    std::fs::write(
        dir.path()
            .join("parquet")
            .join(staged.to_string())
            .join("not-a-fragment.parquet"),
        b"garbage",
    )?;

    let (engine, _tm) = open(dir.path())?;
    assert!(
        engine.open_table("events").is_err(),
        "a relation that could not be rebound must report as nonexistent, \
         not serve a directory the startup sweep deleted"
    );
    // And it is still droppable, so the catalog entry can be cleared.
    engine.drop_table("public", "events")?;
    Ok(())
}
