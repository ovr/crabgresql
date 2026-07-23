//! Per-table memory tables (`relpersistence` `'u'`/`'t'`): their pages live in
//! RAM in the storage manager and their mutations skip the WAL, so they write no
//! file under `base/` and are gone after a restart — the UNLOGGED/TEMP contract —
//! while permanent tables in the same engine stay durable.

use std::sync::Arc;

use crabgresql_pg_engine::PgEngine;
use crabgresql_storage_api::{
    Column, RelPersistence, TableAm, TableEngine, TableSchema, Tid, Tuple,
};
use crabgresql_txn::{CommandId, CommitSink, TransactionManager, TxnContext, TxnFinalize, Xid};
use crabgresql_types::{PgType, Value};
use crabgresql_wal::Wal;

struct H {
    dir: tempfile::TempDir,
    engine: Arc<PgEngine>,
    tm: TransactionManager,
}

fn open(dir: tempfile::TempDir) -> H {
    let wal = Arc::new(Wal::open(dir.path()).expect("open wal"));
    let (engine, clog, next_xid) =
        PgEngine::open_recovered(dir.path(), Arc::clone(&wal)).expect("open engine");
    let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
    let mut tm = TransactionManager::new_recovered(sink, clog, next_xid);
    tm.set_finalize(Arc::clone(&engine) as Arc<dyn TxnFinalize>);
    H { dir, engine, tm }
}

fn setup() -> H {
    open(tempfile::tempdir().expect("create temp data dir"))
}

fn schema(name: &str, persistence: RelPersistence) -> TableSchema {
    let mut s = TableSchema::new(
        name,
        vec![
            Column::new("id", PgType::Int4),
            Column::new("name", PgType::Text),
        ],
    );
    s.persistence = persistence;
    s
}

fn insert_committed(tm: &TransactionManager, table: &dyn TableAm, tuple: Tuple) -> Tid {
    let xid = tm.allocate_xid();
    let txn = tm.context(xid, CommandId::FIRST);
    let tid = table.insert(tuple, &txn);
    tm.commit(xid).expect("commit insert");
    tid
}

fn read(tm: &TransactionManager) -> TxnContext {
    tm.context(Xid::INVALID, CommandId::FIRST)
}

#[test]
fn memory_table_writes_no_file_but_scans_back() -> anyhow::Result<()> {
    let h = setup();
    // The memory table is created first, so it is relfilenode 1; the permanent one
    // is relfilenode 2.
    let mem = h.engine.create_table(schema("m", RelPersistence::Unlogged))?;
    let perm = h.engine.create_table(schema("p", RelPersistence::Permanent))?;

    // Enough rows to span several 8 KB pages, forcing extends (into RAM for the
    // memory table, into a file for the permanent one).
    for i in 0..500 {
        insert_committed(&h.tm, &*mem, vec![Value::Int4(i), Value::Text("x".repeat(40))]);
        insert_committed(&h.tm, &*perm, vec![Value::Int4(i), Value::Text("y".repeat(40))]);
    }

    // The memory table's rows read back intact from RAM.
    let got: Vec<i32> = mem
        .scan(&read(&h.tm))
        .map(|(_, t)| match t[0] {
            Value::Int4(v) => v,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(got, (0..500).collect::<Vec<_>>());

    // No heap file was ever created for the memory table; the permanent table has
    // one.
    let base = h.dir.path().join("base");
    assert!(!base.join("1").exists(), "memory table must not touch disk");
    assert!(base.join("2").exists(), "permanent table must have a heap file");

    Ok(())
}

#[test]
fn memory_table_is_gone_after_restart_permanent_survives() -> anyhow::Result<()> {
    let h = setup();
    let mem = h.engine.create_table(schema("m", RelPersistence::Unlogged))?;
    let perm = h.engine.create_table(schema("p", RelPersistence::Permanent))?;
    insert_committed(&h.tm, &*mem, vec![Value::Int4(1), Value::Text("mem".into())]);
    insert_committed(&h.tm, &*perm, vec![Value::Int4(2), Value::Text("perm".into())]);
    // Make the permanent table's pages durable, then simulate a restart by
    // reopening a brand-new engine over the same directory.
    h.engine.checkpoint(h.tm.allocate_xid())?;
    drop(mem);
    drop(perm);
    let dir = h.dir;
    drop(h.engine);
    drop(h.tm);

    let h2 = open(dir);
    // The memory table never persisted its catalog entry: it is simply absent.
    assert!(matches!(
        h2.engine.open_table("m"),
        Err(crabgresql_storage_api::StorageError::TableNotFound(_))
    ));
    // The permanent table and its row survive the restart.
    let perm = h2.engine.open_table("p")?;
    let rows: Vec<Value> = perm.scan(&read(&h2.tm)).map(|(_, t)| t[1].clone()).collect();
    assert_eq!(rows, vec![Value::Text("perm".into())]);

    Ok(())
}

#[test]
fn memory_table_truncate_swaps_in_ram() -> anyhow::Result<()> {
    let h = setup();
    let mem = h.engine.create_table(schema("m", RelPersistence::Unlogged))?;
    insert_committed(&h.tm, &*mem, vec![Value::Int4(1), Value::Null]);
    insert_committed(&h.tm, &*mem, vec![Value::Int4(2), Value::Null]);
    let tx = h.tm.allocate_xid();
    mem.truncate(&h.tm.context(tx, CommandId::FIRST));
    h.tm.commit(tx)?;
    assert_eq!(mem.scan(&read(&h.tm)).count(), 0);
    // Reinserts land in the fresh (RAM-backed) relfilenode, and still no disk file.
    insert_committed(&h.tm, &*mem, vec![Value::Int4(3), Value::Null]);
    let rows: Vec<Value> = mem.scan(&read(&h.tm)).map(|(_, t)| t[0].clone()).collect();
    assert_eq!(rows, vec![Value::Int4(3)]);
    let base = h.dir.path().join("base");
    assert!(!base.join("1").exists());
    assert!(!base.join("2").exists(), "the post-truncate rel is RAM-backed too");

    Ok(())
}
