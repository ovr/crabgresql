//! Crash recovery: bring the engine up on a temp dir, write a mix of committed,
//! uncommitted and aborted work, "crash" by dropping everything without a
//! checkpoint, then reopen and prove that redo-only recovery restores exactly
//! the committed state.

use std::sync::Arc;

use crabgresql_pg_engine::{PgEngine, RelFileNode};
use crabgresql_storage_api::{Column, TableAm, TableEngine, TableSchema, Tid};
use crabgresql_txn::{Clog, CommandId, CommitSink, TransactionManager, TxnContext, Xid};
use crabgresql_types::{PgType, Value};
use crabgresql_wal::{RmgrRegistry, Wal, recover};

mod common;
use common::{corrupt_page_byte, open, try_open};

fn schema() -> TableSchema {
    TableSchema {
        name: "t".to_string(),
        columns: vec![
            Column::new("id", PgType::Int4),
            Column::new("name", PgType::Text),
        ],
    }
}

fn read(tm: &TransactionManager) -> TxnContext {
    tm.context(Xid::INVALID, CommandId::FIRST)
}

fn visible_ids(tm: &TransactionManager, table: &dyn TableAm) -> Vec<i32> {
    let mut v: Vec<i32> = table
        .scan(&read(tm))
        .map(|(_, t)| match t[0] {
            Value::Int4(x) => x,
            _ => unreachable!(),
        })
        .collect();
    v.sort();
    v
}

fn tid_of(tm: &TransactionManager, table: &dyn TableAm, id: i32) -> Tid {
    table
        .scan(&read(tm))
        .find(|(_, t)| t[0] == Value::Int4(id))
        .map(|(tid, _)| tid)
        .unwrap_or_else(|| panic!("expected visible tuple with id {id}"))
}

fn insert(table: &dyn TableAm, txn: &TxnContext, id: i32, name: &str) -> Tid {
    table.insert(vec![Value::Int4(id), Value::Text(name.into())], txn)
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
        );
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
fn truncate_committed_then_crash_recovers_empty() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (engine, tm) = open(dir.path()).unwrap();
        let table = seed_three(&engine, &tm);
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST));
        tm.commit(tx).unwrap();
        assert_eq!(visible_ids(&tm, &*table), Vec::<i32>::new());
        // Crash: drop without checkpoint.
    }
    let (engine, tm) = open(dir.path()).unwrap();
    let table = engine.open_table("t").unwrap();
    assert_eq!(visible_ids(&tm, &*table), Vec::<i32>::new());
}

#[test]
fn truncate_uncommitted_then_crash_restores_rows() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (engine, tm) = open(dir.path()).unwrap();
        let table = seed_three(&engine, &tm);
        let tx = tm.allocate_xid();
        let ctx = tm.context(tx, CommandId::FIRST);
        table.truncate(&ctx);
        // Read-your-own-truncate: the truncater sees its own now-empty table
        // (reading under its OWN xid; a concurrent reader would block on the
        // AccessExclusive lock until this transaction ends).
        let own: Vec<i32> = table.scan(&ctx).map(|(_, t)| match t[0] {
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
}

#[test]
fn truncate_rolled_back_restores_rows_in_place_and_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, tm) = open(dir.path()).unwrap();
    let table = seed_three(&engine, &tm);
    let tx = tm.allocate_xid();
    table.truncate(&tm.context(tx, CommandId::FIRST));
    tm.abort(tx);
    // Explicit abort restores the rows immediately (the old file was untouched).
    assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3]);
    drop((engine, tm));
    let (engine, tm) = open(dir.path()).unwrap();
    let table = engine.open_table("t").unwrap();
    assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3]);
}

#[test]
fn truncate_then_insert_then_commit_crash_keeps_only_new_rows() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (engine, tm) = open(dir.path()).unwrap();
        let table = seed_three(&engine, &tm);
        let tx = tm.allocate_xid();
        let ctx = tm.context(tx, CommandId::FIRST);
        table.truncate(&ctx);
        // Post-truncate inserts land in the new file (read-your-own-truncate).
        insert(&*table, &ctx, 10, "x");
        insert(&*table, &ctx, 11, "y");
        tm.commit(tx).unwrap();
        assert_eq!(visible_ids(&tm, &*table), vec![10, 11]);
    }
    let (engine, tm) = open(dir.path()).unwrap();
    let table = engine.open_table("t").unwrap();
    assert_eq!(visible_ids(&tm, &*table), vec![10, 11]);
}

#[test]
fn truncate_crash_then_truncate_again_commit_is_consistent() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (engine, tm) = open(dir.path()).unwrap();
        let table = seed_three(&engine, &tm);
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST));
        // Crash before commit.
    }
    {
        // Rows are back; truncate again and commit this time.
        let (engine, tm) = open(dir.path()).unwrap();
        let table = engine.open_table("t").unwrap();
        assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3]);
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST));
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
fn interleaved_committed_and_in_flight_txns_recover_committed_only() {
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
        table.update(base_tid, vec![Value::Int4(10), Value::Text("a-upd".into())], &ca);
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
}
