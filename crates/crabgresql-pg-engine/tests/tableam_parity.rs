//! Drives the durable heap engine through the full `TableAm` surface — visibility,
//! rollback, batch DML, truncate, vacuum, and concurrent inserts — over permanent
//! (on-disk, WAL-logged) tables. The RAM-backed memory-table variant of the same
//! contract lives in `memory_table.rs`.

use std::sync::Arc;

use crabgresql_pg_engine::PgEngine;
use crabgresql_storage_api::{
    Column, ColumnProjection, DeleteResult, IndexConstraint, IndexKey, IndexMetadata, IndexMethod,
    IndexProbeKey, StorageError, TableAm, TableEngine, TableSchema, Tid, Tuple, UpdateResult,
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
    let engine = match PgEngine::new_with_pool(
        dir.path(),
        Arc::clone(&wal),
        &mut reg,
        crabgresql_pg_engine::BufferPoolPolicy::minimal(),
    ) {
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
        .scan(txn, &ColumnProjection::All)
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

/// The scan buffers a whole block and then hands its rows out one at a time by
/// moving them out of that buffer. Nothing may be yielded twice, nothing may
/// arrive emptied, and the order must stay ascending by tid — across a row count
/// large enough to span several pages, so the buffer is refilled repeatedly.
///
/// Asserting the *contents* is the point: a slot read after it was emptied comes
/// back as a zero-length tuple, which a `count()`-only check would sail past.
#[test]
fn a_multi_block_scan_yields_every_row_intact_and_in_order() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    // Wide enough that a few hundred rows cannot fit on one 8 KB page, so the
    // scan crosses block boundaries several times.
    const ROWS: i32 = 400;
    for i in 0..ROWS {
        insert_committed(
            &h.tm,
            &*table,
            vec![
                Value::Int4(i),
                Value::Text(format!("{i}-{}", "x".repeat(60))),
            ],
        );
    }
    let rows = scan_rows(&*table, &read(&h.tm));
    assert_eq!(rows.len() as i32, ROWS);
    for (i, (_, tuple)) in rows.iter().enumerate() {
        let i = i as i32;
        assert_eq!(
            tuple,
            &vec![
                Value::Int4(i),
                Value::Text(format!("{i}-{}", "x".repeat(60)))
            ],
            "row {i} came back wrong"
        );
    }
    let tids: Vec<Tid> = rows.iter().map(|(tid, _)| *tid).collect();
    assert!(
        tids.windows(2)
            .all(|w| (w[0].block, w[0].offset) < (w[1].block, w[1].offset)),
        "scan order must be ascending by tid"
    );

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
    assert_eq!(table.scan(&read(&h.tm), &ColumnProjection::All).count(), 0);
    let self_read = h.tm.context(xid, CommandId(1));
    assert_eq!(table.scan(&self_read, &ColumnProjection::All).count(), 1);
    h.tm.commit(xid)?;
    assert_eq!(table.scan(&read(&h.tm), &ColumnProjection::All).count(), 1);

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
    assert_eq!(table.scan(&read(&h.tm), &ColumnProjection::All).count(), 0);

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
    assert_eq!(table.scan(&read(&h.tm), &ColumnProjection::All).count(), 0);

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
    assert_eq!(table.scan(&read(&h.tm), &ColumnProjection::All).count(), 0);
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
        .scan(&read(&h.tm), &ColumnProjection::All)
        .next()
        .transpose()?
        .map(|(tid, _)| tid)
        .expect("seeded rows");
    let del = h.tm.allocate_xid();
    table.delete(gone, &h.tm.context(del, CommandId::FIRST))?;
    h.tm.commit(del)?;

    let xid = h.tm.allocate_xid();
    h.engine
        .analyze("public", "t", &h.tm.context(xid, CommandId::FIRST))?;
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
fn a_batch_spanning_pages_places_every_row_and_reports_its_tids() -> anyhow::Result<()> {
    // The batch that matters is the one that outgrows a page: `insert_many`
    // reports its tids from two different runs, and both have to name the rows
    // actually stored.
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let rows: Vec<Tuple> = (0..500)
        .map(|i| vec![Value::Int4(i), Value::Text("y".repeat(300))])
        .collect();
    let xid = h.tm.allocate_xid();
    let tids = table.insert_many(rows.clone(), &h.tm.context(xid, CommandId::FIRST))?;
    h.tm.commit(xid)?;

    assert_eq!(tids.len(), rows.len());
    let blocks: std::collections::BTreeSet<u32> = tids.iter().map(|t| t.block).collect();
    assert!(
        blocks.len() > 1,
        "the batch was meant to cross a page boundary, got {} block(s)",
        blocks.len()
    );
    let scanned = scan_rows(&*table, &read(&h.tm));
    assert_eq!(scanned.len(), rows.len());
    // Scan order is physical, and so is placement order, so the two lists line
    // up element for element.
    for (i, (tid, tuple)) in scanned.iter().enumerate() {
        assert_eq!(*tid, tids[i], "row {i} was reported at the wrong tid");
        assert_eq!(*tuple, rows[i], "row {i} did not survive the batch");
    }
    // A reported tid is also usable on its own, not just in scan order.
    for (i, tid) in tids.iter().enumerate() {
        assert_eq!(table.fetch(*tid, &read(&h.tm))?, Some(rows[i].clone()));
    }

    Ok(())
}

#[test]
fn a_batch_mixing_toasted_and_inline_rows_round_trips() -> anyhow::Result<()> {
    // A toasted row's chunks are written before the batch touches its heap page
    // (a page lock may not be held across another pin), so the interesting batch
    // is the one where wide and narrow rows alternate.
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let rows: Vec<Tuple> = (0..6)
        .map(|i| {
            let text = match i % 2 {
                0 => Value::Text("small".into()),
                _ => big_text(40_000),
            };
            vec![Value::Int4(i), text]
        })
        .collect();
    let xid = h.tm.allocate_xid();
    let tids = table.insert_many(rows.clone(), &h.tm.context(xid, CommandId::FIRST))?;
    h.tm.commit(xid)?;

    assert_eq!(tids.len(), rows.len());
    let scanned = scan_rows(&*table, &read(&h.tm));
    assert_eq!(
        scanned.into_iter().map(|(_, t)| t).collect::<Vec<_>>(),
        rows,
        "a toasted value did not survive the batch"
    );

    Ok(())
}

#[test]
fn a_batch_maintains_the_unique_index() -> anyhow::Result<()> {
    // Index maintenance runs after placement now rather than interleaved with
    // it, so every row of the batch must still reach the tree under the tid it
    // was placed at.
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    h.engine.create_index("public", "t", pk_on_id())?;
    let rows: Vec<Tuple> = (0..200)
        .map(|i| vec![Value::Int4(i), Value::Text("z".repeat(200))])
        .collect();
    let xid = h.tm.allocate_xid();
    let tids = table.insert_many(rows.clone(), &h.tm.context(xid, CommandId::FIRST))?;
    h.tm.commit(xid)?;

    for (i, tid) in tids.iter().enumerate() {
        let key = [Value::Int4(i as i32)];
        let hits: Vec<(Tid, Tuple)> = table
            .index_lookup("t_pkey", &IndexProbeKey::equality(&key), &read(&h.tm))
            .unwrap_or_else(|| panic!("the index declined to serve id {i}"))
            .collect::<Result<Vec<_>, StorageError>>()?;
        assert_eq!(
            hits,
            vec![(*tid, rows[i].clone())],
            "id {i} is missing from the index or points elsewhere"
        );
    }

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
    let scan = table.scan(&read(&h.tm), &ColumnProjection::All);
    insert_committed(&h.tm, &*table, vec![Value::Int4(2), Value::Null]);
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    table.update(a, vec![Value::Int4(99), Value::Null], &txn)?;
    h.tm.commit(xid)?;
    let rows: Vec<_> = scan.collect::<Result<Vec<_>, StorageError>>()?;
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
    insert_committed(
        &h.tm,
        &*table,
        vec![Value::Int4(1), Value::Text("a".into())],
    );
    insert_committed(
        &h.tm,
        &*table,
        vec![Value::Int4(2), Value::Text("b".into())],
    );
    h.engine.create_index("public", "t", pk_on_id())?;

    // The durable heap engine now builds a physical B-tree: it reports index-scan
    // support (the planner plans an Index Scan) and `index_lookup` serves the
    // probe directly.
    assert!(table.supports_index_scan("t_pkey"));
    let hits: Vec<Tuple> = table
        .index_lookup(
            "t_pkey",
            &IndexProbeKey::equality(&[Value::Int4(2)]),
            &read(&h.tm),
        )
        .expect("the index serves the probe")
        .map(|row| row.expect("index probe failed").1)
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
        .index_lookup(
            "t_pkey",
            &IndexProbeKey::equality(&[Value::Int4(999)]),
            &read(&h.tm),
        )
        .expect("the index serves an absent key too")
        .map(|row| row.expect("index probe failed").1)
        .collect();
    assert!(miss.is_empty());

    // An unknown index name falls back (None), keeping the scan path correct.
    assert!(
        table
            .index_lookup(
                "nope",
                &IndexProbeKey::equality(&[Value::Int4(2)]),
                &read(&h.tm)
            )
            .is_none()
    );
    Ok(())
}

#[test]
fn truncate_upgrades_over_the_same_owners_open_scan() -> anyhow::Result<()> {
    // Regression: a session that holds an open cursor (a live scan guard) and
    // then TRUNCATEs the same table must not self-deadlock. Run it on a worker
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
        let mut scan = table.scan(&scan_ctx, &ColumnProjection::All);
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
    // Regression: a staged (uncommitted) TRUNCATE's new file lives on the
    // handle, not the catalog; dropping the table must reclaim it, not leak it.
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);

    // Stage an uncommitted TRUNCATE: the first table is relfilenode 1, so the
    // staged empty file is relfilenode 2.
    let tx = h.tm.allocate_xid();
    table.truncate(&h.tm.context(tx, CommandId::FIRST))?;
    let staged = h._dir.path().join("base").join("2");
    assert!(
        staged.exists(),
        "staged TRUNCATE file should exist before the drop"
    );

    h.engine.drop_table("public", "t")?;
    assert!(
        !staged.exists(),
        "drop_table must unlink the staged TRUNCATE file (no leak)"
    );
    Ok(())
}

#[test]
fn drop_table_reclaims_a_pending_truncates_index_files_too() -> anyhow::Result<()> {
    // A TRUNCATE stages a fresh file per physical index alongside the heap's;
    // none of them is in the catalog until commit, so DROP TABLE must unlink
    // every one.
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    insert_committed(&h.tm, &*table, vec![Value::Int4(1), Value::Null]);
    h.engine.create_index("public", "t", pk_on_id())?;
    let before: Vec<_> = std::fs::read_dir(h._dir.path().join("base"))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();

    let tx = h.tm.allocate_xid();
    table.truncate(&h.tm.context(tx, CommandId::FIRST))?;
    let staged: Vec<_> = std::fs::read_dir(h._dir.path().join("base"))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| !before.contains(p))
        .collect();
    assert_eq!(
        staged.len(),
        2,
        "one staged file for the heap and one for the index: {staged:?}"
    );

    h.engine.drop_table("public", "t")?;
    for path in staged {
        assert!(!path.exists(), "drop_table leaked the staged file {path:?}");
    }
    Ok(())
}

/// A row of `plain`-storage columns only: nothing is a candidate for out-of-line
/// storage, so this is the shape TOAST cannot rescue — it stays rejected.
fn untoastable_schema(name: &str, ncols: usize) -> TableSchema {
    TableSchema::new(
        name,
        (0..ncols)
            .map(|i| Column::new(format!("c{i}"), PgType::Uuid))
            .collect(),
    )
}

#[test]
fn a_row_of_untoastable_columns_reports_row_is_too_big() -> anyhow::Result<()> {
    // PostgreSQL raises 54000 program_limit_exceeded with
    // "row is too big: size N, maximum size 8160" for a row that cannot be made
    // to fit a page. Every column here is fixed-width, so no amount of
    // out-of-line storage can shrink the row — the error is permanent, not a
    // stand-in for TOAST.
    let h = setup();
    let table = h.engine.create_table(untoastable_schema("t", 500))?;
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    let row: Tuple = (0..500).map(|_| Value::Uuid([0u8; 16])).collect();

    let error = table
        .insert(row, &txn)
        .expect_err("a row of fixed-width columns cannot be made to fit");
    match error {
        StorageError::RowTooBig { size, max } => {
            assert_eq!(max, 8160);
            assert!(size > max, "size {size} should exceed the maximum {max}");
            assert_eq!(
                error.to_string(),
                format!("row is too big: size {size}, maximum size 8160")
            );
        }
        other => panic!("expected RowTooBig, got {other}"),
    }
    // The failed insert stored nothing.
    h.tm.commit(xid)?;
    assert_eq!(scan_rows(&*table, &read(&h.tm)).len(), 0);
    Ok(())
}

#[test]
fn an_update_rejected_for_size_leaves_the_old_row_visible() -> anyhow::Result<()> {
    // `update` stamps the old version deleted before placing the new one, and the
    // stamp is not undoable within the statement. A new version that cannot be
    // stored must therefore be rejected BEFORE the stamp, or the row is deleted
    // by a statement that reported an error — and vanishes on commit.
    let h = setup();
    let table = h.engine.create_table(untoastable_schema("t", 500))?;
    let small: Tuple = std::iter::once(Value::Uuid([7u8; 16]))
        .chain((1..500).map(|_| Value::Null))
        .collect();
    let tid = insert_committed(&h.tm, &*table, small.clone());

    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    let oversized: Tuple = (0..500).map(|_| Value::Uuid([1u8; 16])).collect();
    assert!(matches!(
        table
            .update(tid, oversized, &txn)
            .expect_err("the new version cannot be made to fit"),
        StorageError::RowTooBig { .. }
    ));
    h.tm.commit(xid)?;

    let rows = scan_rows(&*table, &read(&h.tm));
    assert_eq!(
        rows.len(),
        1,
        "the pre-image must survive a rejected update"
    );
    assert_eq!(rows[0].1, small);
    Ok(())
}

/// A text value of `n` bytes with position-dependent content, so a test that
/// reassembled chunks in the wrong order would notice.
fn big_text(n: usize) -> Value {
    Value::Text(
        (0..n)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect::<String>(),
    )
}

#[test]
fn a_large_value_round_trips_through_scan_and_fetch() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let big = big_text(100_000);
    let tid = insert_committed(&h.tm, &*table, vec![Value::Int4(1), big.clone()]);

    let rows = scan_rows(&*table, &read(&h.tm));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, vec![Value::Int4(1), big.clone()]);

    let fetched = table.fetch(tid, &read(&h.tm))?;
    assert_eq!(fetched, Some(vec![Value::Int4(1), big]));
    Ok(())
}

/// A projected scan must agree with a full scan on every column it was asked
/// for, for **every** subset of the columns — including one that skips a toasted
/// value, which is the case the heap's decoder walks past rather than
/// reassembles.
///
/// Exhaustive over the subsets on purpose. A skip that mis-measures one kind
/// shifts every attribute behind it, so it shows up only for the particular
/// pairing that straddles the mistake; a handful of hand-picked projections
/// would miss it.
#[test]
fn every_projection_agrees_with_a_full_scan() -> anyhow::Result<()> {
    let h = setup();
    let columns = vec![
        Column::new("i", PgType::Int4),
        Column::new("t", PgType::Text),
        Column::new("big", PgType::Text),
        Column::new("b", PgType::Bool),
        Column::new("n", PgType::Numeric),
        Column::new("y", PgType::Bytea),
    ];
    let width = columns.len();
    let table = h
        .engine
        .create_table(TableSchema::new("t", columns.clone()))?;
    let schema = table.schema();
    let rows = vec![
        vec![
            Value::Int4(1),
            Value::Text("short".into()),
            // Comfortably over the inline limit, so it is stored out of line and
            // an unprojected scan must not walk its chunks.
            big_text(60_000),
            Value::Bool(true),
            Value::Numeric(crabgresql_types::Numeric::parse("12.3400")?),
            Value::Bytea(vec![1, 2, 3]),
        ],
        // A row of NULLs beside the values, so the bitmap and the skip walk are
        // exercised together.
        vec![
            Value::Int4(2),
            Value::Null,
            big_text(3_000),
            Value::Null,
            Value::Numeric(crabgresql_types::Numeric::parse("-0.5")?),
            Value::Null,
        ],
    ];
    for row in &rows {
        insert_committed(&h.tm, &*table, row.clone());
    }

    let full = scan_rows(&*table, &read(&h.tm));
    assert_eq!(full.len(), rows.len());
    for mask in 0..(1u32 << width) {
        let wanted: Vec<usize> = (0..width).filter(|i| mask >> i & 1 == 1).collect();
        let projection = ColumnProjection::of(wanted.iter().copied(), &schema);
        let got: Vec<(Tid, Tuple)> = table
            .scan(&read(&h.tm), &projection)
            .collect::<Result<_, StorageError>>()?;
        assert_eq!(got.len(), full.len(), "projection {wanted:?} lost rows");
        for (want, have) in full.iter().zip(&got) {
            assert_eq!(want.0, have.0, "projection {wanted:?} moved a tid");
            assert_eq!(
                have.1.len(),
                width,
                "projection {wanted:?} narrowed the row"
            );
            for &column in &wanted {
                assert_eq!(
                    have.1[column], want.1[column],
                    "column {column} under projection {wanted:?}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn values_spanning_one_two_and_many_chunks_round_trip() -> anyhow::Result<()> {
    // The chunk payload is ~2 KB, so these straddle every boundary that matters:
    // under one chunk, exactly at it, just over, and hundreds of them.
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let sizes = [2_001, 2_002, 2_003, 4_004, 4_005, 500_000];
    for (i, n) in sizes.iter().enumerate() {
        insert_committed(&h.tm, &*table, vec![Value::Int4(i as i32), big_text(*n)]);
    }
    let rows = scan_rows(&*table, &read(&h.tm));
    assert_eq!(rows.len(), sizes.len());
    for (i, n) in sizes.iter().enumerate() {
        assert_eq!(
            rows[i].1,
            vec![Value::Int4(i as i32), big_text(*n)],
            "value of {n} bytes did not survive the round trip"
        );
    }
    Ok(())
}

#[test]
fn a_bytea_and_a_jsonb_are_toasted_by_the_same_path() -> anyhow::Result<()> {
    // The chunks hold exactly what the value codec would have written inline, so
    // out-of-line storage is type-agnostic — no per-type toast logic exists.
    let h = setup();
    let table = h.engine.create_table(TableSchema::new(
        "t",
        vec![
            Column::new("b", PgType::Bytea),
            Column::new("j", PgType::Jsonb),
        ],
    ))?;
    let blob = Value::Bytea((0..60_000).map(|i| i as u8).collect());
    let json =
        crabgresql_types::Value::Jsonb(crabgresql_types::json::Jsonb::String("x".repeat(40_000)));
    insert_committed(&h.tm, &*table, vec![blob.clone(), json.clone()]);
    let rows = scan_rows(&*table, &read(&h.tm));
    assert_eq!(rows[0].1, vec![blob, json]);
    Ok(())
}

#[test]
fn a_toasted_row_survives_an_update_of_another_column() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let big = big_text(50_000);
    let tid = insert_committed(&h.tm, &*table, vec![Value::Int4(1), big.clone()]);

    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    assert_eq!(
        table.update(tid, vec![Value::Int4(2), big.clone()], &txn)?,
        UpdateResult::Updated
    );
    h.tm.commit(xid)?;

    let rows = scan_rows(&*table, &read(&h.tm));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, vec![Value::Int4(2), big]);
    Ok(())
}

#[test]
fn a_deleted_toasted_row_stays_readable_to_an_older_snapshot() -> anyhow::Result<()> {
    // Chunks must outlive the DELETE that stamps the row: a snapshot older than
    // the deleter still sees the row and must still be able to read its value.
    // This is why nothing is freed eagerly — only VACUUM reclaims chunks.
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let big = big_text(30_000);
    let tid = insert_committed(&h.tm, &*table, vec![Value::Int4(1), big.clone()]);

    let reader = h.tm.allocate_xid();
    let snapshot = h.tm.context(reader, CommandId::FIRST);

    let deleter = h.tm.allocate_xid();
    let txn = h.tm.context(deleter, CommandId::FIRST);
    assert_eq!(table.delete(tid, &txn)?, DeleteResult::Deleted);
    h.tm.commit(deleter)?;

    let rows = scan_rows(&*table, &snapshot);
    assert_eq!(rows.len(), 1, "the older snapshot must still see the row");
    assert_eq!(rows[0].1, vec![Value::Int4(1), big]);
    // And it is gone for a snapshot taken after the delete.
    assert_eq!(scan_rows(&*table, &read(&h.tm)).len(), 0);
    Ok(())
}

/// Size of the relfilenode-`n` file under this harness's data directory.
fn relfile_len(h: &H, n: u32) -> u64 {
    std::fs::metadata(h._dir.path().join("base").join(n.to_string()))
        .map(|m| m.len())
        .unwrap_or(0)
}

#[test]
fn vacuum_reclaims_the_chunks_of_a_dead_row() -> anyhow::Result<()> {
    // Reclamation is observable as space reuse: `page::add_item` refills
    // LP_UNUSED slots and `page::compact` repacks the page, so a chain freed by
    // VACUUM is available to the next one. Without it this cycle would extend
    // the chunk store by a whole value every time.
    //
    // A single-page value (7000 bytes is four chunks, which is exactly one page)
    // keeps this a test of reclamation rather than of block selection: like the
    // heap, chunk writes follow a one-block insert hint and there is no free
    // space map, so space freed in a block the hint has already moved past is not
    // found again. That gap is pre-existing and shared with the heap; it is not
    // what this test is about.
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let big = big_text(7_000);

    let tid = insert_committed(&h.tm, &*table, vec![Value::Int4(1), big.clone()]);
    // The heap is relfilenode 1, so the chunk store is 2.
    let after_first = relfile_len(&h, 2);
    assert_eq!(
        after_first, 8192,
        "four chunks should occupy exactly one page"
    );

    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    assert_eq!(table.delete(tid, &txn)?, DeleteResult::Deleted);
    h.tm.commit(xid)?;

    table.vacuum(h.tm.allocate_xid(), h.tm.clog());
    assert_eq!(scan_rows(&*table, &read(&h.tm)).len(), 0);

    insert_committed(&h.tm, &*table, vec![Value::Int4(2), big.clone()]);
    assert_eq!(
        relfile_len(&h, 2),
        after_first,
        "the second value should reuse the reclaimed chunks, not extend the store"
    );

    // And the reused chunks hold the right bytes.
    let rows = scan_rows(&*table, &read(&h.tm));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, vec![Value::Int4(2), big]);
    Ok(())
}

#[test]
fn a_committed_truncate_empties_the_chunk_store_in_place() -> anyhow::Result<()> {
    // The chunk store keeps its relfilenode across a TRUNCATE and is emptied
    // instead. Swapping it would need a second relfilenode that no WAL record
    // names and that the catalog only learns about at commit — so a crash in that
    // window would leave a committed row pointing into a file the startup sweep
    // unlinks. Emptying in place has no such window.
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    insert_committed(&h.tm, &*table, vec![Value::Int4(1), big_text(80_000)]);
    let toast = h._dir.path().join("base").join("2");
    assert!(toast.exists(), "the chunk store should have been created");
    assert!(relfile_len(&h, 2) > 0);

    let xid = h.tm.allocate_xid();
    table.truncate(&h.tm.context(xid, CommandId::FIRST))?;
    h.tm.commit(xid)?;

    assert_eq!(scan_rows(&*table, &read(&h.tm)).len(), 0);
    assert!(toast.exists(), "the chunk store keeps its relfilenode");
    assert_eq!(relfile_len(&h, 2), 0, "but its space is reclaimed");

    // The table works afterwards, on the same chunk store.
    let big = big_text(90_000);
    insert_committed(&h.tm, &*table, vec![Value::Int4(2), big.clone()]);
    let rows = scan_rows(&*table, &read(&h.tm));
    assert_eq!(rows[0].1, vec![Value::Int4(2), big]);
    Ok(())
}

#[test]
fn a_rolled_back_truncate_keeps_the_chunk_store_and_its_values() -> anyhow::Result<()> {
    // The rollback restores the pre-truncate heap file, whose tuples point into
    // the chunk store — so that store and everything in it must survive intact.
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let big = big_text(70_000);
    insert_committed(&h.tm, &*table, vec![Value::Int4(1), big.clone()]);
    let toast = h._dir.path().join("base").join("2");

    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    table.truncate(&txn)?;
    // A big row written inside the doomed transaction.
    table.insert(vec![Value::Int4(2), big_text(60_000)], &txn)?;
    h.tm.abort(xid);

    assert!(toast.exists(), "the chunk store must survive a rollback");
    let rows = scan_rows(&*table, &read(&h.tm));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, vec![Value::Int4(1), big]);
    Ok(())
}

#[test]
fn a_frozen_write_requires_this_transaction_to_have_truncated() -> anyhow::Result<()> {
    // A frozen tuple is visible at once and names no transaction whose abort
    // could retract it, so it is only sound in storage a rollback discards. The
    // server checks that before authorizing the freeze; the heap checks it again
    // where the header is actually stamped, so widening the freeze fails loudly
    // instead of writing unretractable rows into a live relfilenode.
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST).with_freeze();

    let error = table
        .insert(vec![Value::Int4(1), Value::Null], &txn)
        .expect_err("a frozen write with no staged truncate must be refused");
    assert!(
        error.to_string().contains("has not truncated it"),
        "{error}"
    );
    assert_eq!(scan_rows(&*table, &read(&h.tm)).len(), 0);

    // With this transaction's own TRUNCATE staged, the same write goes through.
    table.truncate(&txn)?;
    table.insert(vec![Value::Int4(1), Value::Null], &txn)?;
    h.tm.commit(xid)?;
    assert_eq!(ids(&h.tm, &*table), vec![Value::Int4(1)]);
    Ok(())
}

#[test]
fn a_rolled_back_truncate_reclaims_the_chunks_it_wrote() -> anyhow::Result<()> {
    // The other half of the rollback story. A TRUNCATE stages a fresh heap file
    // and discards it on abort, which takes away every tuple that named a chain
    // written into it — but the chunk store is deliberately not swapped, and
    // VACUUM reaches a chain only through a heap tuple. So without an explicit
    // sweep those chunks are unreachable *and* unreclaimable, and repeating this
    // cycle grows the store without bound.
    //
    // Same single-page value as `vacuum_reclaims_the_chunks_of_a_dead_row`, and
    // for the same reason: chunk writes follow a one-block hint with no free space
    // map, so this stays a test of reclamation rather than of block selection.
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let big = big_text(7_000);

    insert_committed(&h.tm, &*table, vec![Value::Int4(1), big.clone()]);
    let after_first = relfile_len(&h, 2);

    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    table.truncate(&txn)?;
    table.insert(vec![Value::Int4(2), big.clone()], &txn)?;
    let while_staged = relfile_len(&h, 2);
    assert!(
        while_staged > after_first,
        "the doomed row's chunks should have extended the store: \
         {while_staged} vs {after_first}"
    );
    h.tm.abort(xid);

    // The surviving row is untouched, and the doomed row's space is available
    // again — so the next value reuses it instead of extending the store.
    insert_committed(&h.tm, &*table, vec![Value::Int4(3), big.clone()]);
    assert_eq!(
        relfile_len(&h, 2),
        while_staged,
        "the rolled-back load's chunks should have been reclaimed and reused"
    );
    let rows = scan_rows(&*table, &read(&h.tm));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1, vec![Value::Int4(1), big.clone()]);
    assert_eq!(rows[1].1, vec![Value::Int4(3), big]);
    Ok(())
}

#[test]
fn vacuum_reclaims_the_chunks_of_a_rolled_back_insert() -> anyhow::Result<()> {
    // A rolled-back wide INSERT leaves a tuple whose *inserter* aborted. Keying
    // VACUUM's victim rule on `xmax` alone would never select it, so its whole
    // out-of-line value would leak with no path to reclaim it — bounded before
    // TOAST at one page-sized tuple, unbounded after.
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    // 7000 bytes is four chunks, exactly one page, so reuse is observable
    // without depending on block selection (there is no free space map).
    let big = big_text(7_000);

    let xid = h.tm.allocate_xid();
    let txn = h.tm.context(xid, CommandId::FIRST);
    table.insert(vec![Value::Int4(1), big.clone()], &txn)?;
    h.tm.abort(xid);
    assert_eq!(scan_rows(&*table, &read(&h.tm)).len(), 0);
    let after_abort = relfile_len(&h, 2);
    assert_eq!(after_abort, 8192, "the aborted value still occupies a page");

    table.vacuum(h.tm.allocate_xid(), h.tm.clog());

    // The reclaimed chunks are reused rather than the store being extended.
    insert_committed(&h.tm, &*table, vec![Value::Int4(2), big.clone()]);
    assert_eq!(
        relfile_len(&h, 2),
        after_abort,
        "the aborted insert's chunks must be reclaimed, not leaked"
    );
    let rows = scan_rows(&*table, &read(&h.tm));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, vec![Value::Int4(2), big]);
    Ok(())
}

#[test]
fn drop_table_unlinks_the_chunk_store() -> anyhow::Result<()> {
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    insert_committed(&h.tm, &*table, vec![Value::Int4(1), big_text(40_000)]);
    let toast = h._dir.path().join("base").join("2");
    assert!(toast.exists());
    drop(table);

    h.engine.drop_table("public", "t")?;
    assert!(
        !toast.exists(),
        "drop_table must unlink the chunk store too"
    );
    Ok(())
}

#[test]
fn concurrent_first_wide_inserts_create_exactly_one_chunk_store() -> anyhow::Result<()> {
    // Inserts hold only a shared table lock, so several can be inside
    // `ensure_toast_rel` at once. Each store it creates is published to the
    // catalog, which keeps one — so a second store would hold chunks the next
    // startup's orphan sweep unlinks, permanently destroying the rows that
    // toasted into it. Racing the *first* wide inserts into a fresh table is the
    // window; a parallel bulk load hits it immediately.
    let h = setup();
    let table = h.engine.create_table(schema("t"))?;
    let barrier = std::sync::Barrier::new(4);
    std::thread::scope(|s| -> anyhow::Result<()> {
        let mut handles = Vec::new();
        for i in 0..4 {
            let barrier = &barrier;
            let table = &table;
            let h = &h;
            handles.push(s.spawn(move || -> anyhow::Result<()> {
                let xid = h.tm.allocate_xid();
                let txn = h.tm.context(xid, CommandId::FIRST);
                // Line every writer up on the pre-creation read.
                barrier.wait();
                table.insert(vec![Value::Int4(i), big_text(50_000)], &txn)?;
                h.tm.commit(xid)?;
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

    // The heap is relfilenode 1; exactly one chunk store means exactly one more.
    let mut files: Vec<String> = std::fs::read_dir(h._dir.path().join("base"))?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    files.sort();
    assert_eq!(
        files.len(),
        2,
        "every writer must share one chunk store, got {files:?}"
    );
    // And every value is intact and readable.
    let rows = scan_rows(&*table, &read(&h.tm));
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().all(|(_, t)| t[1] == big_text(50_000)));
    Ok(())
}
