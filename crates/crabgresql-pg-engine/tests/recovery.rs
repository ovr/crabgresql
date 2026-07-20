//! Crash recovery: bring the engine up on a temp dir, write a mix of committed,
//! uncommitted and aborted work, "crash" by dropping everything without a
//! checkpoint, then reopen and prove that redo-only recovery restores exactly
//! the committed state.

use std::path::Path;
use std::sync::Arc;

use crabgresql_pg_engine::PgEngine;
use crabgresql_storage_api::{Column, TableAm, TableEngine, TableSchema, Tid};
use crabgresql_txn::{Clog, CommandId, CommitSink, TransactionManager, TxnContext, Xid};
use crabgresql_types::{PgType, Value};
use crabgresql_wal::{RmgrRegistry, Wal, recover};

fn schema() -> TableSchema {
    TableSchema {
        name: "t".to_string(),
        columns: vec![
            Column::new("id", PgType::Int4),
            Column::new("name", PgType::Text),
        ],
    }
}

/// Open the engine fresh over `dir`, replaying any existing WAL. Returns the
/// engine and a transaction manager sharing its WAL + recovered CLOG.
fn open(dir: &Path) -> anyhow::Result<(PgEngine, TransactionManager)> {
    let wal = Arc::new(Wal::open(dir)?);
    let mut reg = RmgrRegistry::new();
    let engine = PgEngine::new(dir, Arc::clone(&wal), &mut reg)?;
    let clog = Arc::new(Clog::new());
    let res = recover(dir, &reg, &clog)?;
    // Mirror the production startup (open_pg_engine): clamp the WAL to the last
    // valid record before appending, so multi-restart cycles stay consistent.
    wal.reset_to(res.end_of_wal)?;
    let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
    let tm = TransactionManager::new_recovered(sink, clog, res.next_xid);
    Ok((engine, tm))
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
