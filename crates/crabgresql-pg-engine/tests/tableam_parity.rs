//! Drives the durable heap engine through the same `TableAm` scenarios the
//! in-memory reference engine covers, proving the two agree on visibility,
//! rollback, batch DML, truncate, vacuum, and concurrent inserts.

use std::sync::Arc;

use crabgresql_pg_engine::PgEngine;
use crabgresql_storage_api::{
    Column, DeleteResult, TableAm, TableEngine, TableSchema, Tid, Tuple, UpdateResult,
};
use crabgresql_txn::{Clog, CommandId, CommitSink, TransactionManager, TxnContext, Xid};
use crabgresql_types::{PgType, Value};
use crabgresql_wal::{RmgrRegistry, Wal};

struct H {
    _dir: tempfile::TempDir,
    engine: PgEngine,
    tm: TransactionManager,
}

fn setup() -> H {
    let dir = tempfile::tempdir().unwrap();
    let wal = Arc::new(Wal::open(dir.path()).unwrap());
    let mut reg = RmgrRegistry::new();
    let engine = PgEngine::new(dir.path(), Arc::clone(&wal), &mut reg).unwrap();
    let clog = Arc::new(Clog::new());
    let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
    let tm = TransactionManager::new_recovered(sink, clog, Xid::FIRST_NORMAL);
    H { _dir: dir, engine, tm }
}

fn schema(name: &str) -> TableSchema {
    TableSchema {
        name: name.to_string(),
        columns: vec![Column::new("id", PgType::Int4), Column::new("name", PgType::Text)],
    }
}

fn insert_committed(tm: &TransactionManager, table: &dyn TableAm, tuple: Tuple) -> Tid {
    let xid = tm.allocate_xid();
    let txn = tm.context(xid, CommandId::FIRST);
    let tid = table.insert(tuple, &txn);
    tm.commit(xid).unwrap();
    tid
}

fn read(tm: &TransactionManager) -> TxnContext {
    tm.context(Xid::INVALID, CommandId::FIRST)
}

fn ids(tm: &TransactionManager, table: &dyn TableAm) -> Vec<Value> {
    table.scan(&read(tm)).map(|(_, t)| t[0].clone()).collect()
}

#[test]
fn insert_then_scan() {
    let h = setup();
    let table = h.engine.create_table(schema("t")).unwrap();
    insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Text("one".into())]);
    insert_committed(&h.tm, &*table, vec![Value::Int4(2), Value::Null]);
    let rows: Vec<_> = table.scan(&read(&h.tm)).collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1, vec![Value::Int4(1), Value::Text("one".into())]);
    assert_eq!(rows[1].1, vec![Value::Int4(2), Value::Null]);
}

#[test]
fn insert_returns_distinct_ascending_tids() {
    let h = setup();
    let table = h.engine.create_table(schema("t")).unwrap();
    let a = insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    let b = insert_committed(&h.tm, &*table, vec![Value::Int4(2), Value::Null]);
    // Sequential inserts fill a block in order, so tids ascend by (block, offset).
    assert!(b > a);
    let tids: Vec<Tid> = table.scan(&read(&h.tm)).map(|(tid, _)| tid).collect();
    assert_eq!(tids, vec![a, b]);
}

#[test]
fn uncommitted_insert_is_invisible_until_commit() {
    let h = setup();
    let table = h.engine.create_table(schema("t")).unwrap();
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    table.insert(vec![Value::Int4(1), Value::Null], &txn);
    assert_eq!(table.scan(&read(&h.tm)).count(), 0);
    let self_read = h.tm.context(xid, CommandId(1));
    assert_eq!(table.scan(&self_read).count(), 1);
    h.tm.commit(xid).unwrap();
    assert_eq!(table.scan(&read(&h.tm)).count(), 1);
}

#[test]
fn aborted_insert_is_never_visible() {
    let h = setup();
    let table = h.engine.create_table(schema("t")).unwrap();
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    table.insert(vec![Value::Int4(1), Value::Null], &txn);
    h.tm.abort(xid);
    assert_eq!(table.scan(&read(&h.tm)).count(), 0);
}

#[test]
fn update_makes_new_version_visible_old_dead() {
    let h = setup();
    let table = h.engine.create_table(schema("t")).unwrap();
    let tid = insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Text("one".into())]);
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    assert_eq!(
        table.update(tid, vec![Value::Int4(1), Value::Text("uno".into())], &txn),
        UpdateResult::Updated
    );
    h.tm.commit(xid).unwrap();
    let rows: Vec<_> = table.scan(&read(&h.tm)).map(|(_, t)| t).collect();
    assert_eq!(rows, vec![vec![Value::Int4(1), Value::Text("uno".into())]]);
}

#[test]
fn rolled_back_update_restores_old_version() {
    let h = setup();
    let table = h.engine.create_table(schema("t")).unwrap();
    let tid = insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Text("one".into())]);
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    table.update(tid, vec![Value::Int4(1), Value::Text("uno".into())], &txn);
    h.tm.abort(xid);
    let rows: Vec<_> = table.scan(&read(&h.tm)).map(|(_, t)| t).collect();
    assert_eq!(rows, vec![vec![Value::Int4(1), Value::Text("one".into())]]);
}

#[test]
fn delete_leaves_other_tids_untouched() {
    let h = setup();
    let table = h.engine.create_table(schema("t")).unwrap();
    insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    let b = insert_committed(&h.tm, &*table, vec![Value::Int4(2), Value::Null]);
    insert_committed(&h.tm, &*table, vec![Value::Int4(3), Value::Null]);
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    assert_eq!(table.delete(b, &txn), DeleteResult::Deleted);
    h.tm.commit(xid).unwrap();
    assert_eq!(ids(&h.tm, &*table), vec![Value::Int4(1), Value::Int4(3)]);
}

#[test]
fn rolled_back_delete_keeps_the_row() {
    let h = setup();
    let table = h.engine.create_table(schema("t")).unwrap();
    let a = insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    table.delete(a, &txn);
    h.tm.abort(xid);
    assert_eq!(ids(&h.tm, &*table), vec![Value::Int4(1)]);
}

#[test]
fn vacuum_keeps_live_row_whose_deleter_aborted() {
    let h = setup();
    let table = h.engine.create_table(schema("t")).unwrap();
    let a = insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    let x = h.tm.allocate_xid();
    table.delete(a, &h.tm.context(x, CommandId::FIRST));
    h.tm.abort(x);
    let horizon = h.tm.allocate_xid();
    table.vacuum(horizon, h.tm.clog());
    assert_eq!(ids(&h.tm, &*table), vec![Value::Int4(1)]);
}

#[test]
fn vacuum_reclaims_committed_dead_row() {
    let h = setup();
    let table = h.engine.create_table(schema("t")).unwrap();
    let a = insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    insert_committed(&h.tm, &*table, vec![Value::Int4(2), Value::Null]);
    let x = h.tm.allocate_xid();
    table.delete(a, &h.tm.context(x, CommandId::FIRST));
    h.tm.commit(x).unwrap();
    let horizon = h.tm.allocate_xid();
    table.vacuum(horizon, h.tm.clog());
    // The dead row is gone; the survivor remains readable.
    assert_eq!(ids(&h.tm, &*table), vec![Value::Int4(2)]);
}

#[test]
fn update_and_delete_of_missing_tid_report_not_found() {
    let h = setup();
    let table = h.engine.create_table(schema("t")).unwrap();
    let tid = insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    assert_eq!(table.delete(tid, &txn), DeleteResult::Deleted);
    h.tm.commit(xid).unwrap();
    let x2 = h.tm.allocate_xid();
    let t2 = h.tm.context(x2, CommandId::FIRST);
    assert_eq!(table.delete(tid, &t2), DeleteResult::NotFound);
    assert_eq!(
        table.update(tid, vec![Value::Int4(2), Value::Null], &t2),
        UpdateResult::NotFound
    );
    assert_eq!(table.delete(Tid { block: 0, offset: 999 }, &t2), DeleteResult::NotFound);
}

#[test]
fn duplicate_create_fails() {
    let h = setup();
    h.engine.create_table(schema("t")).unwrap();
    assert!(matches!(
        h.engine.create_table(schema("t")),
        Err(crabgresql_storage_api::StorageError::TableAlreadyExists(_))
    ));
}

#[test]
fn open_missing_table_fails() {
    let h = setup();
    assert!(matches!(
        h.engine.open_table("nope"),
        Err(crabgresql_storage_api::StorageError::TableNotFound(_))
    ));
}

#[test]
fn update_many_applies_batch_and_skips_missing() {
    let h = setup();
    let table = h.engine.create_table(schema("t")).unwrap();
    let a = insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    let b = insert_committed(&h.tm, &*table, vec![Value::Int4(2), Value::Null]);
    let dx = h.tm.allocate_xid();
    table.delete(b, &h.tm.context(dx, CommandId::FIRST));
    h.tm.commit(dx).unwrap();
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    let applied = table.update_many(
        vec![(a, vec![Value::Int4(10), Value::Null]), (b, vec![Value::Int4(20), Value::Null])],
        &txn,
    );
    h.tm.commit(xid).unwrap();
    assert_eq!(applied, 1);
    assert_eq!(ids(&h.tm, &*table), vec![Value::Int4(10)]);
}

#[test]
fn delete_many_removes_batch_in_one_pass() {
    let h = setup();
    let table = h.engine.create_table(schema("t")).unwrap();
    let a = insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    let b = insert_committed(&h.tm, &*table, vec![Value::Int4(2), Value::Null]);
    let c = insert_committed(&h.tm, &*table, vec![Value::Int4(3), Value::Null]);
    let dx = h.tm.allocate_xid();
    table.delete(b, &h.tm.context(dx, CommandId::FIRST));
    h.tm.commit(dx).unwrap();
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    assert_eq!(table.delete_many(vec![a, b, c], &txn), 2);
    h.tm.commit(xid).unwrap();
    assert_eq!(table.scan(&read(&h.tm)).count(), 0);
}

#[test]
fn truncate_empties_table() {
    let h = setup();
    let table = h.engine.create_table(schema("t")).unwrap();
    insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    insert_committed(&h.tm, &*table, vec![Value::Int4(2), Value::Null]);
    let tx = h.tm.allocate_xid();
    table.truncate(&h.tm.context(tx, CommandId::FIRST));
    h.tm.commit(tx).unwrap();
    assert_eq!(table.scan(&read(&h.tm)).count(), 0);
    insert_committed(&h.tm, &*table, vec![Value::Int4(3), Value::Null]);
    assert_eq!(ids(&h.tm, &*table), vec![Value::Int4(3)]);
}

#[test]
fn many_inserts_span_multiple_pages() {
    let h = setup();
    let table = h.engine.create_table(schema("t")).unwrap();
    // Enough rows to force several 8 KB pages.
    for i in 0..1000 {
        insert_committed(&h.tm, &*table, vec![Value::Int4(i), Value::Text("x".repeat(40))]);
    }
    let got: Vec<i32> = table
        .scan(&read(&h.tm))
        .map(|(_, t)| match t[0] {
            Value::Int4(v) => v,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(got.len(), 1000);
    assert_eq!(got, (0..1000).collect::<Vec<_>>());
}

#[test]
fn concurrent_inserts_are_all_visible_with_distinct_tids() {
    let h = setup();
    let table = h.engine.create_table(schema("t")).unwrap();
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                for i in 0..250 {
                    let xid = h.tm.allocate_xid();
                    let txn = h.tm.context(xid, CommandId::FIRST);
                    table.insert(vec![Value::Int4(i), Value::Null], &txn);
                    h.tm.commit(xid).unwrap();
                }
            });
        }
    });
    let tids: Vec<Tid> = table.scan(&read(&h.tm)).map(|(tid, _)| tid).collect();
    assert_eq!(tids.len(), 1000);
    let mut sorted = tids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 1000, "tids must be distinct");
}

#[test]
fn concurrent_updates_of_one_row_never_duplicate_it() {
    // Regression: update stamps the old version atomically before placing the new
    // one, so two racing updaters can't both leave a live successor. Each trial
    // uses a fresh single-row table so the live count is unambiguous.
    let h = setup();
    for trial in 0..40 {
        let table = h.engine.create_table(schema(&format!("t{trial}"))).unwrap();
        let tid = insert_committed(&h.tm, &*table, vec![Value::Int4(0), Value::Text("v0".into())]);
        std::thread::scope(|s| {
            let table = &table;
            let tm = &h.tm;
            for w in 0..2i32 {
                s.spawn(move || {
                    let xid = tm.allocate_xid();
                    let txn = tm.context(xid, CommandId::FIRST);
                    table.update(tid, vec![Value::Int4(1000 + w), Value::Text("vN".into())], &txn);
                    tm.commit(xid).unwrap();
                });
            }
        });
        let rows: Vec<Value> = table.scan(&read(&h.tm)).map(|(_, t)| t[0].clone()).collect();
        assert_eq!(rows.len(), 1, "trial {trial}: exactly one live version, got {rows:?}");
        assert!(matches!(rows[0], Value::Int4(1000) | Value::Int4(1001)));
    }
}

#[test]
fn scan_is_stable_against_concurrent_writes() {
    let h = setup();
    let table = h.engine.create_table(schema("t")).unwrap();
    let a = insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    let scan = table.scan(&read(&h.tm));
    insert_committed(&h.tm, &*table, vec![Value::Int4(2), Value::Null]);
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    table.update(a, vec![Value::Int4(99), Value::Null], &txn);
    h.tm.commit(xid).unwrap();
    let rows: Vec<_> = scan.collect();
    assert_eq!(rows, vec![(a, vec![Value::Int4(1), Value::Null])]);
}
