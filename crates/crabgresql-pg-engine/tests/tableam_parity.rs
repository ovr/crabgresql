//! Drives the durable heap engine through the full `TableAm` surface — visibility,
//! rollback, batch DML, truncate, vacuum, and concurrent inserts — over permanent
//! (on-disk, WAL-logged) tables. The RAM-backed memory-table variant of the same
//! contract lives in `memory_table.rs`.

use std::sync::Arc;

use crabgresql_pg_engine::PgEngine;
use crabgresql_storage_api::{
    Column, DeleteResult, IndexConstraint, IndexKey, IndexMetadata, IndexMethod, StorageError,
    TableAm, TableEngine, TableSchema, Tid, Tuple, UpdateResult,
};
use crabgresql_txn::{
    Clog, CommandId, CommitSink, LockOwner, TransactionManager, TxnContext, TxnFinalize, Xid,
};
use crabgresql_types::{PgType, Value};
use crabgresql_wal::{RmgrRegistry, Wal};

struct H {
    _dir: tempfile::TempDir,
    engine: Arc<PgEngine>,
    tm: TransactionManager,
}

fn setup() -> H {
    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(error) => panic!("failed to create test data directory: {error}"),
    };
    let wal = Arc::new(match Wal::open(dir.path()) {
        Ok(wal) => wal,
        Err(error) => panic!("failed to open test WAL: {error}"),
    });
    let mut reg = RmgrRegistry::new();
    let engine = match PgEngine::new(dir.path(), Arc::clone(&wal), &mut reg) {
        Ok(engine) => Arc::new(engine),
        Err(error) => panic!("failed to open test engine: {error}"),
    };
    let clog = Arc::new(Clog::new());
    let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
    let mut tm = TransactionManager::new_recovered(sink, clog, Xid::FIRST_NORMAL);
    // Wire the finalize hook so a committed TRUNCATE applies its relfilenode swap.
    tm.set_finalize(Arc::clone(&engine) as Arc<dyn TxnFinalize>);
    H {
        _dir: dir,
        engine,
        tm,
    }
}

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            Column::new("id", PgType::Int4),
            Column::new("name", PgType::Text),
        ],
    )
}

fn insert_committed(tm: &TransactionManager, table: &dyn TableAm, tuple: Tuple) -> Tid {
    let xid = tm.allocate_xid();
    let txn = tm.context(xid, CommandId::FIRST);
    let tid = table.insert(tuple, &txn);
    if let Err(error) = tm.commit(xid) {
        panic!("failed to commit table-access test transaction: {error}");
    }
    tid.unwrap_or_else(|error| panic!("failed to insert table-access test tuple: {error}"))
}

fn read(tm: &TransactionManager) -> TxnContext {
    tm.context(Xid::INVALID, CommandId::FIRST)
}

fn ids(tm: &TransactionManager, table: &dyn TableAm) -> Vec<Value> {
    scan_rows(table, &read(tm))
        .into_iter()
        .map(|(_, t)| t[0].clone())
        .collect()
}

fn scan_rows(table: &dyn TableAm, txn: &TxnContext) -> Vec<(Tid, Tuple)> {
    table
        .scan(txn)
        .collect::<Result<Vec<_>, StorageError>>()
        .unwrap_or_else(|error| panic!("table scan failed: {error}"))
}

#[test]
fn insert_then_scan() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    insert_committed(
        &h.tm,
        &*table,
        vec![Value::Int4(1), Value::Text("one".into())],
    );
    insert_committed(&h.tm, &*table, vec![Value::Int4(2), Value::Null]);
    let rows = scan_rows(&*table, &read(&h.tm));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1, vec![Value::Int4(1), Value::Text("one".into())]);
    assert_eq!(rows[1].1, vec![Value::Int4(2), Value::Null]);

    Ok(())
}

#[test]
fn insert_returns_distinct_ascending_tids() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let a = insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    let b = insert_committed(&h.tm, &*table, vec![Value::Int4(2), Value::Null]);
    // Sequential inserts fill a block in order, so tids ascend by (block, offset).
    assert!(b > a);
    let tids: Vec<Tid> = scan_rows(&*table, &read(&h.tm))
        .into_iter()
        .map(|(tid, _)| tid)
        .collect();
    assert_eq!(tids, vec![a, b]);

    Ok(())
}

#[test]
fn uncommitted_insert_is_invisible_until_commit() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    table.insert(vec![Value::Int4(1), Value::Null], &txn)?;
    assert_eq!(table.scan(&read(&h.tm)).count(), 0);
    let self_read = h.tm.context(xid, CommandId(1));
    assert_eq!(table.scan(&self_read).count(), 1);
    h.tm.commit(xid)?;
    assert_eq!(table.scan(&read(&h.tm)).count(), 1);

    Ok(())
}

#[test]
fn aborted_insert_is_never_visible() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    table.insert(vec![Value::Int4(1), Value::Null], &txn)?;
    h.tm.abort(xid);
    assert_eq!(table.scan(&read(&h.tm)).count(), 0);

    Ok(())
}

#[test]
fn update_makes_new_version_visible_old_dead() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let tid = insert_committed(
        &h.tm,
        &*table,
        vec![Value::Int4(1), Value::Text("one".into())],
    );
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    assert_eq!(
        table.update(tid, vec![Value::Int4(1), Value::Text("uno".into())], &txn)?,
        UpdateResult::Updated
    );
    h.tm.commit(xid)?;
    let rows: Vec<_> = scan_rows(&*table, &read(&h.tm))
        .into_iter()
        .map(|(_, t)| t)
        .collect();
    assert_eq!(rows, vec![vec![Value::Int4(1), Value::Text("uno".into())]]);

    Ok(())
}

#[test]
fn rolled_back_update_restores_old_version() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let tid = insert_committed(
        &h.tm,
        &*table,
        vec![Value::Int4(1), Value::Text("one".into())],
    );
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    table.update(tid, vec![Value::Int4(1), Value::Text("uno".into())], &txn)?;
    h.tm.abort(xid);
    let rows: Vec<_> = scan_rows(&*table, &read(&h.tm))
        .into_iter()
        .map(|(_, t)| t)
        .collect();
    assert_eq!(rows, vec![vec![Value::Int4(1), Value::Text("one".into())]]);

    Ok(())
}

#[test]
fn delete_leaves_other_tids_untouched() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    let b = insert_committed(&h.tm, &*table, vec![Value::Int4(2), Value::Null]);
    insert_committed(&h.tm, &*table, vec![Value::Int4(3), Value::Null]);
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    assert_eq!(table.delete(b, &txn)?, DeleteResult::Deleted);
    h.tm.commit(xid)?;
    assert_eq!(ids(&h.tm, &*table), vec![Value::Int4(1), Value::Int4(3)]);

    Ok(())
}

#[test]
fn rolled_back_delete_keeps_the_row() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let a = insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    table.delete(a, &txn)?;
    h.tm.abort(xid);
    assert_eq!(ids(&h.tm, &*table), vec![Value::Int4(1)]);

    Ok(())
}

#[test]
fn vacuum_keeps_live_row_whose_deleter_aborted() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let a = insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    let x = h.tm.allocate_xid();
    table.delete(a, &h.tm.context(x, CommandId::FIRST))?;
    h.tm.abort(x);
    let horizon = h.tm.allocate_xid();
    table.vacuum(horizon, h.tm.clog());
    assert_eq!(ids(&h.tm, &*table), vec![Value::Int4(1)]);

    Ok(())
}

#[test]
fn vacuum_reclaims_committed_dead_row() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let a = insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    insert_committed(&h.tm, &*table, vec![Value::Int4(2), Value::Null]);
    let x = h.tm.allocate_xid();
    table.delete(a, &h.tm.context(x, CommandId::FIRST))?;
    h.tm.commit(x)?;
    let horizon = h.tm.allocate_xid();
    table.vacuum(horizon, h.tm.clog());
    // The dead row is gone; the survivor remains readable.
    assert_eq!(ids(&h.tm, &*table), vec![Value::Int4(2)]);

    Ok(())
}

#[test]
fn update_and_delete_of_missing_tid_report_not_found() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let tid = insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    assert_eq!(table.delete(tid, &txn)?, DeleteResult::Deleted);
    h.tm.commit(xid)?;
    let x2 = h.tm.allocate_xid();
    let t2 = h.tm.context(x2, CommandId::FIRST);
    assert_eq!(table.delete(tid, &t2)?, DeleteResult::NotFound);
    assert_eq!(
        table.update(tid, vec![Value::Int4(2), Value::Null], &t2)?,
        UpdateResult::NotFound
    );
    assert_eq!(
        table.delete(
            Tid {
                block: 0,
                offset: 999
            },
            &t2
        )?,
        DeleteResult::NotFound
    );

    Ok(())
}

#[test]
fn duplicate_create_fails() -> anyhow::Result<()> {
    let h = setup();
    h.engine.create_table(schema("t"))?;
    assert!(matches!(
        h.engine.create_table(schema("t")),
        Err(crabgresql_storage_api::StorageError::TableAlreadyExists(_))
    ));

    Ok(())
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
fn update_many_applies_batch_and_skips_missing() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let a = insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    let b = insert_committed(&h.tm, &*table, vec![Value::Int4(2), Value::Null]);
    let dx = h.tm.allocate_xid();
    table.delete(b, &h.tm.context(dx, CommandId::FIRST))?;
    h.tm.commit(dx)?;
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    let applied = table.update_many(
        vec![
            (a, vec![Value::Int4(10), Value::Null]),
            (b, vec![Value::Int4(20), Value::Null]),
        ],
        &txn,
    )?;
    h.tm.commit(xid)?;
    assert_eq!(applied, 1);
    assert_eq!(ids(&h.tm, &*table), vec![Value::Int4(10)]);

    Ok(())
}

#[test]
fn delete_many_removes_batch_in_one_pass() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let a = insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    let b = insert_committed(&h.tm, &*table, vec![Value::Int4(2), Value::Null]);
    let c = insert_committed(&h.tm, &*table, vec![Value::Int4(3), Value::Null]);
    let dx = h.tm.allocate_xid();
    table.delete(b, &h.tm.context(dx, CommandId::FIRST))?;
    h.tm.commit(dx)?;
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    assert_eq!(table.delete_many(vec![a, b, c], &txn)?, 2);
    h.tm.commit(xid)?;
    assert_eq!(table.scan(&read(&h.tm)).count(), 0);

    Ok(())
}

#[test]
fn truncate_empties_table() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    insert_committed(&h.tm, &*table, vec![Value::Int4(2), Value::Null]);
    let tx = h.tm.allocate_xid();
    table.truncate(&h.tm.context(tx, CommandId::FIRST))?;
    h.tm.commit(tx)?;
    assert_eq!(table.scan(&read(&h.tm)).count(), 0);
    insert_committed(&h.tm, &*table, vec![Value::Int4(3), Value::Null]);
    assert_eq!(ids(&h.tm, &*table), vec![Value::Int4(3)]);

    Ok(())
}

#[test]
fn statistics_track_the_relations_physical_size() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;

    // A brand-new relation occupies no pages and so estimates no rows.
    let empty = table.statistics();
    assert_eq!(empty.relpages, 0);
    assert_eq!(empty.reltuples, 0.0);
    assert!(
        !empty.analyzed,
        "a size-derived estimate must not claim to be analyzed"
    );

    // Enough rows to span several 8 KB pages, so the page count is meaningful.
    for i in 0..1000 {
        insert_committed(
            &h.tm,
            &*table,
            vec![Value::Int4(i), Value::Text("x".repeat(40))],
        );
    }
    let full = table.statistics();
    assert!(full.relpages > 1, "expected several pages, got {full:?}");
    // The estimate divides page space by an assumed row width, so it is only
    // ever a rough figure — assert the order of magnitude, not the number.
    assert!(
        (250.0..=4000.0).contains(&full.reltuples),
        "1000 rows should estimate within a factor of 4: {full:?}"
    );

    // TRUNCATE swaps in a fresh, empty relfilenode, so the estimate must follow
    // it back down — and only once the swap has committed.
    let tx = h.tm.allocate_xid();
    table.truncate(&h.tm.context(tx, CommandId::FIRST))?;
    h.tm.commit(tx)?;
    assert_eq!(table.statistics().relpages, 0);

    Ok(())
}

#[test]
fn analyze_counts_visible_rows_exactly() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    for i in 0..250 {
        insert_committed(&h.tm, &*table, vec![Value::Int4(i), Value::Null]);
    }
    // An uncommitted insert and a committed delete: ANALYZE measures what a
    // reader can see, so neither should be counted.
    let pending = h.tm.allocate_xid();
    table.insert(
        vec![Value::Int4(9998), Value::Null],
        &h.tm.context(pending, CommandId::FIRST),
    )?;
    let gone = table
        .scan(&read(&h.tm))
        .next()
        .transpose()?
        .map(|(tid, _)| tid)
        .expect("seeded rows");
    let del = h.tm.allocate_xid();
    table.delete(gone, &h.tm.context(del, CommandId::FIRST))?;
    h.tm.commit(del)?;

    let xid = h.tm.allocate_xid();
    h.engine.analyze("public", "t", &h.tm.context(xid, CommandId::FIRST))?;
    h.tm.commit(xid)?;

    let stats = table.statistics();
    assert!(stats.analyzed);
    assert_eq!(stats.reltuples, 249.0, "{stats:?}");
    assert!(stats.relpages > 0, "{stats:?}");

    Ok(())
}

#[test]
fn truncate_discards_the_analyze_result() -> anyhow::Result<()> {
    // Regression: statistics describe the file TRUNCATE swaps away, so keeping
    // them would report the pre-truncate row count for an empty relation
    // forever. PostgreSQL returns the relation to never-analyzed.
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    for i in 0..200 {
        insert_committed(&h.tm, &*table, vec![Value::Int4(i), Value::Null]);
    }
    let xid = h.tm.allocate_xid();
    h.engine
        .analyze("public", "t", &h.tm.context(xid, CommandId::FIRST))?;
    h.tm.commit(xid)?;
    assert_eq!(table.statistics().reltuples, 200.0);

    let tx = h.tm.allocate_xid();
    table.truncate(&h.tm.context(tx, CommandId::FIRST))?;
    h.tm.commit(tx)?;

    let stats = table.statistics();
    assert!(
        !stats.analyzed,
        "TRUNCATE must return the relation to never-analyzed, got {stats:?}"
    );
    assert_eq!(stats.relpages, 0);
    assert_eq!(stats.reltuples, 0.0);

    Ok(())
}

#[test]
fn analyze_inside_an_uncommitted_truncate_measures_one_file() -> anyhow::Result<()> {
    // Regression: the scan reads the transaction's staged (empty) file while a
    // bare nblocks() reads the committed one, which paired a zero row count
    // with the old file's page count — and survived the rollback, because
    // statistics are not transactional.
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    for i in 0..500 {
        insert_committed(
            &h.tm,
            &*table,
            vec![Value::Int4(i), Value::Text("x".repeat(40))],
        );
    }
    assert!(table.statistics().relpages > 1);

    // One transaction: stage a TRUNCATE, then analyze inside it.
    let tx = h.tm.allocate_xid();
    let ctx = h.tm.context(tx, CommandId::FIRST);
    table.truncate(&ctx)?;
    h.engine.analyze("public", "t", &ctx)?;
    let staged = table.statistics();
    assert_eq!(
        (staged.relpages, staged.reltuples),
        (0, 0.0),
        "the staged empty file must be measured as a whole, got {staged:?}"
    );
    h.tm.abort(tx);

    Ok(())
}

#[test]
fn analyze_of_a_missing_relation_reports_it_as_absent() {
    let h = setup();
    let xid = h.tm.allocate_xid();
    let error = h
        .engine
        .analyze("public", "nosuch", &h.tm.context(xid, CommandId::FIRST))
        .expect_err("analyzing a missing relation must fail");
    assert!(matches!(error, StorageError::TableNotFound(name) if name == "nosuch"));
}

#[test]
fn many_inserts_span_multiple_pages() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    // Enough rows to force several 8 KB pages.
    for i in 0..1000 {
        insert_committed(
            &h.tm,
            &*table,
            vec![Value::Int4(i), Value::Text("x".repeat(40))],
        );
    }
    let got: Vec<i32> = scan_rows(&*table, &read(&h.tm))
        .into_iter()
        .map(|(_, t)| match t[0] {
            Value::Int4(v) => v,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(got.len(), 1000);
    assert_eq!(got, (0..1000).collect::<Vec<_>>());

    Ok(())
}

#[test]
fn concurrent_inserts_are_all_visible_with_distinct_tids() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    std::thread::scope(|s| -> anyhow::Result<()> {
        let mut handles = Vec::new();
        for _ in 0..4 {
            handles.push(s.spawn(|| -> anyhow::Result<()> {
                for i in 0..250 {
                    let xid = h.tm.allocate_xid();
                    let txn = h.tm.context(xid, CommandId::FIRST);
                    table.insert(vec![Value::Int4(i), Value::Null], &txn)?;
                    h.tm.commit(xid)?;
                }
                Ok(())
            }));
        }
        for handle in handles {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("insert worker panicked"))??;
        }
        Ok(())
    })?;
    let tids: Vec<Tid> = scan_rows(&*table, &read(&h.tm))
        .into_iter()
        .map(|(tid, _)| tid)
        .collect();
    assert_eq!(tids.len(), 1000);
    let mut sorted = tids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 1000, "tids must be distinct");

    Ok(())
}

#[test]
fn concurrent_updates_of_one_row_never_duplicate_it() -> anyhow::Result<()> {
    // Regression: update stamps the old version atomically before placing the new
    // one, so two racing updaters can't both leave a live successor. Each trial
    // uses a fresh single-row table so the live count is unambiguous.
    let h = setup();
    for trial in 0..40 {
        let table = h.engine.create_table(schema(&format!("t{trial}")))?;
        let tid = insert_committed(
            &h.tm,
            &*table,
            vec![Value::Int4(0), Value::Text("v0".into())],
        );
        std::thread::scope(|s| -> anyhow::Result<()> {
            let table = &table;
            let tm = &h.tm;
            let mut handles = Vec::new();
            for w in 0..2i32 {
                handles.push(s.spawn(move || -> anyhow::Result<()> {
                    let xid = tm.allocate_xid();
                    let txn = tm.context(xid, CommandId::FIRST);
                    table.update(
                        tid,
                        vec![Value::Int4(1000 + w), Value::Text("vN".into())],
                        &txn,
                    )?;
                    tm.commit(xid)?;
                    Ok(())
                }));
            }
            for handle in handles {
                handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("update worker panicked"))??;
            }
            Ok(())
        })?;
        let rows: Vec<Value> = scan_rows(&*table, &read(&h.tm))
            .into_iter()
            .map(|(_, t)| t[0].clone())
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "trial {trial}: exactly one live version, got {rows:?}"
        );
        assert!(matches!(rows[0], Value::Int4(1000) | Value::Int4(1001)));
    }

    Ok(())
}

#[test]
fn scan_is_stable_against_concurrent_writes() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let a = insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    let scan = table.scan(&read(&h.tm));
    insert_committed(&h.tm, &*table, vec![Value::Int4(2), Value::Null]);
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    table.update(a, vec![Value::Int4(99), Value::Null], &txn)?;
    h.tm.commit(xid)?;
    let rows: Vec<_> = scan
        .collect::<Result<Vec<_>, StorageError>>()?;
    assert_eq!(rows, vec![(a, vec![Value::Int4(1), Value::Null])]);

    Ok(())
}

#[test]
fn drop_table_unlinks_file_and_frees_name() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    insert_committed(
        &h.tm,
        &*table,
        vec![Value::Int4(1), Value::Text("one".into())],
    );
    // The first relation is relfilenode 1; its heap file exists after the insert.
    let base = h._dir.path().join("base");
    assert!(base.join("1").exists());
    drop(table);

    h.engine.drop_table("public", "t")?;
    // The heap file is unlinked and the name is gone.
    assert!(!base.join("1").exists());
    assert!(matches!(
        h.engine.open_table("t"),
        Err(crabgresql_storage_api::StorageError::TableNotFound(_))
    ));
    // Dropping again reports the missing table.
    assert!(matches!(
        h.engine.drop_table("public", "t"),
        Err(crabgresql_storage_api::StorageError::TableNotFound(_))
    ));

    // Re-creating the name succeeds and gets a fresh relfilenode (2), never
    // reusing the dropped one — the catalog's counter stays monotonic.
    let t2 = h.engine.create_table(schema("t"))?;
    insert_committed(&h.tm, &*t2, vec![Value::Int4(2), Value::Null]);
    assert!(base.join("2").exists());
    assert!(!base.join("1").exists());
    let rows: Vec<Value> = scan_rows(&*t2, &read(&h.tm))
        .into_iter()
        .map(|(_, t)| t[0].clone())
        .collect();
    assert_eq!(rows, vec![Value::Int4(2)]);

    Ok(())
}

/// A PRIMARY KEY index on `id` (column 0) named `t_pkey`.
fn pk_on_id() -> IndexMetadata {
    IndexMetadata {
        name: "t_pkey".into(),
        method: IndexMethod::BTree,
        keys: vec![IndexKey {
            column: 0,
            descending: false,
            nulls_first: false,
        }],
        unique: true,
        nulls_distinct: true,
        constraint: Some(IndexConstraint::PrimaryKey),
    }
}

#[test]
fn heap_index_lookup_uses_btree() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Text("a".into())]);
    insert_committed(&h.tm, &*table, vec![Value::Int4(2), Value::Text("b".into())]);
    h.engine.create_index("public", "t", pk_on_id())?;

    // The durable heap engine now builds a physical B-tree: it reports index-scan
    // support (the planner plans an Index Scan) and `index_lookup` serves the
    // probe directly.
    assert!(table.supports_index_scan("t_pkey"));
    let hits: Vec<Tuple> = table
        .index_lookup("t_pkey", &[Value::Int4(2)], &read(&h.tm))
        .expect("the index serves the probe")
        .map(|(_, t)| t)
        .collect();
    assert_eq!(hits, vec![vec![Value::Int4(2), Value::Text("b".into())]]);

    // The index probe agrees bit-for-bit with a seq scan + key filter.
    let scan: Vec<Tuple> = scan_rows(&*table, &read(&h.tm))
        .into_iter()
        .filter(|(_, t)| t[0] == Value::Int4(2))
        .map(|(_, t)| t)
        .collect();
    assert_eq!(hits, scan);

    // A key with no matching row is served as an empty result, not a fallback.
    let miss: Vec<Tuple> = table
        .index_lookup("t_pkey", &[Value::Int4(999)], &read(&h.tm))
        .expect("the index serves an absent key too")
        .map(|(_, t)| t)
        .collect();
    assert!(miss.is_empty());

    // An unknown index name falls back (None), keeping the scan path correct.
    assert!(
        table
            .index_lookup("nope", &[Value::Int4(2)], &read(&h.tm))
            .is_none()
    );
    Ok(())
}

#[test]
fn truncate_upgrades_over_the_same_owners_open_scan() -> anyhow::Result<()> {
    // Review finding #3: a session that holds an open cursor (a live scan guard)
    // and then TRUNCATEs the same table must not self-deadlock. Run it on a worker
    // thread and fail fast via a timeout if the lock upgrade regresses to a hang.
    use std::sync::mpsc;
    use std::time::Duration;

    let (done_tx, done_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || -> anyhow::Result<()> {
        let h = setup();
        let table = h.engine.create_table(schema("t"))?;
        insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);

        // Open a scan under owner 42 and keep it paused mid-stream (a suspended
        // cursor holds the scan's shared guard exactly like this).
        let mut scan_ctx = h.tm.context(Xid::INVALID, CommandId::FIRST);
        scan_ctx.lock_owner = LockOwner(42);
        let mut scan = table.scan(&scan_ctx);
        let _first = scan.next();

        // TRUNCATE under the SAME owner: must upgrade over its own shared hold.
        let tx = h.tm.allocate_xid();
        let mut trunc_ctx = h.tm.context(tx, CommandId::FIRST);
        trunc_ctx.lock_owner = LockOwner(42);
        table.truncate(&trunc_ctx)?;
        h.tm.commit(tx)?;
        drop(scan);
        done_tx.send(())?;
        Ok(())
    });

    done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("TRUNCATE deadlocked against the same session's open scan");
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("TRUNCATE worker panicked"))??;
    Ok(())
}

#[test]
fn drop_table_reclaims_a_pending_truncate_file() -> anyhow::Result<()> {
    // Review finding #4: a staged (uncommitted) TRUNCATE's new file lives on the
    // handle, not the catalog; dropping the table must reclaim it, not leak it.
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);

    // Stage an uncommitted TRUNCATE: the first table is relfilenode 1, so the
    // staged empty file is relfilenode 2.
    let tx = h.tm.allocate_xid();
    table.truncate(&h.tm.context(tx, CommandId::FIRST))?;
    let staged = h._dir.path().join("base").join("2");
    assert!(staged.exists(), "staged TRUNCATE file should exist before the drop");

    h.engine.drop_table("public", "t")?;
    assert!(
        !staged.exists(),
        "drop_table must unlink the staged TRUNCATE file (no leak)"
    );
    Ok(())
}
