//! The three relation persistence classes:
//! * `Temporary` — RAM-backed (pages in the storage manager's memory store),
//!   WAL-skipped, catalog row not persisted → gone entirely on restart.
//! * `Unlogged` — on-disk (a real `base/<relfilenode>` file), WAL-skipped, catalog
//!   row persisted → definition + data survive a CLEAN restart, data reset to empty
//!   after a CRASH (like PostgreSQL).
//! * `Permanent` — on-disk, WAL-logged, recovered from the WAL after a crash.

use std::sync::Arc;

use crabgresql_pg_engine::PgEngine;
use crabgresql_storage_api::{ColumnProjection, 
    Column, IndexConstraint, IndexKey, IndexMetadata, IndexMethod, RelPersistence, StorageError,
    TableAm, TableEngine, TableSchema, Tid, Tuple,
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

/// Reopen the same data directory. `clean` chooses whether the previous run shut
/// down cleanly (Unlogged data kept) or crashed (Unlogged data reset).
fn reopen(h: H, clean: bool) -> H {
    if clean {
        h.engine.shutdown();
    }
    let dir = h.dir;
    drop(h.engine);
    drop(h.tm);
    open(dir)
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
    tid.unwrap_or_else(|error| panic!("insert failed: {error}"))
}

fn read(tm: &TransactionManager) -> TxnContext {
    tm.context(Xid::INVALID, CommandId::FIRST)
}

fn ids(tm: &TransactionManager, table: &dyn TableAm) -> Vec<i32> {
    table
        .scan(&read(tm), &ColumnProjection::All)
        .map(|row| match row.unwrap_or_else(|error| panic!("scan failed: {error}")).1[0] {
            Value::Int4(v) => v,
            _ => unreachable!(),
        })
        .collect()
}

/// A UNIQUE index named `name` on column 0 (`id`).
fn idx(name: &str) -> IndexMetadata {
    IndexMetadata {
        name: name.into(),
        method: IndexMethod::BTree,
        keys: vec![IndexKey {
            column: 0,
            descending: false,
            nulls_first: false,
        }],
        unique: true,
        nulls_distinct: true,
        constraint: Some(IndexConstraint::Unique),
    }
}

/// Number of relation files under `base/` (heap + index files).
fn base_file_count(h: &H) -> usize {
    std::fs::read_dir(h.dir.path().join("base"))
        .map(|e| e.count())
        .unwrap_or(0)
}

// ---- Temporary (RAM-backed) -------------------------------------------------

#[test]
fn temporary_table_is_ram_backed_no_file() -> anyhow::Result<()> {
    let h = setup();
    let t = h.engine.create_table(schema("t", RelPersistence::Temporary))?;
    // Enough rows to span several 8 KB pages (forcing extends into RAM).
    for i in 0..500 {
        insert_committed(&h.tm, &*t, vec![Value::Int4(i), Value::Text("x".repeat(40))]);
    }
    assert_eq!(ids(&h.tm, &*t), (0..500).collect::<Vec<_>>());
    assert_eq!(base_file_count(&h), 0, "a Temporary table must not touch disk");
    Ok(())
}

#[test]
fn temporary_table_gone_after_restart() -> anyhow::Result<()> {
    let h = setup();
    let t = h.engine.create_table(schema("t", RelPersistence::Temporary))?;
    let p = h.engine.create_table(schema("p", RelPersistence::Permanent))?;
    insert_committed(&h.tm, &*t, vec![Value::Int4(1), Value::Text("temp".into())]);
    insert_committed(&h.tm, &*p, vec![Value::Int4(2), Value::Text("perm".into())]);
    drop(t);
    drop(p);

    let h2 = reopen(h, true);
    // The temp table never persisted its catalog entry: it is simply absent.
    assert!(matches!(
        h2.engine.open_table("t"),
        Err(StorageError::TableNotFound(_))
    ));
    let p = h2.engine.open_table("p")?;
    assert_eq!(ids(&h2.tm, &*p), vec![2]);
    Ok(())
}

#[test]
fn temporary_table_truncate_swaps_in_ram() -> anyhow::Result<()> {
    let h = setup();
    let t = h.engine.create_table(schema("t", RelPersistence::Temporary))?;
    insert_committed(&h.tm, &*t, vec![Value::Int4(1), Value::Null]);
    insert_committed(&h.tm, &*t, vec![Value::Int4(2), Value::Null]);
    let tx = h.tm.allocate_xid();
    t.truncate(&h.tm.context(tx, CommandId::FIRST))?;
    h.tm.commit(tx)?;
    assert_eq!(t.scan(&read(&h.tm), &ColumnProjection::All).count(), 0);
    insert_committed(&h.tm, &*t, vec![Value::Int4(3), Value::Null]);
    assert_eq!(ids(&h.tm, &*t), vec![3]);
    assert_eq!(base_file_count(&h), 0, "post-truncate rel is RAM-backed too");
    Ok(())
}

#[test]
fn temporary_index_is_metadata_only() -> anyhow::Result<()> {
    let h = setup();
    let t = h.engine.create_table(schema("t", RelPersistence::Temporary))?;
    insert_committed(&h.tm, &*t, vec![Value::Int4(1), Value::Text("one".into())]);
    h.engine.create_index("public", "t", idx("t_idx"))?;
    // No physical B-tree: probe returns None (executor falls back to a scan).
    assert!(!t.supports_index_scan("t_idx"));
    assert!(t.index_lookup("t_idx", &[Value::Int4(1)], &read(&h.tm)).is_none());
    assert_eq!(base_file_count(&h), 0, "a Temporary index must not write a file");
    Ok(())
}

/// Regression: a `Temporary` table is excluded from the persisted catalog, so
/// EVERY catalog section (including the IXR1 index-relfilenode tail) must skip it,
/// or the positional zip desyncs and a later permanent table's durable index is
/// silently downgraded to metadata-only (and its B-tree file GC'd).
#[test]
fn temp_before_indexed_permanent_does_not_desync_index_tail() -> anyhow::Result<()> {
    let h = setup();
    // The temp table is created FIRST — it is the one dropped from `encode`.
    let _t = h.engine.create_table(schema("t", RelPersistence::Temporary))?;
    let p = h.engine.create_table(schema("p", RelPersistence::Permanent))?;
    insert_committed(&h.tm, &*p, vec![Value::Int4(1), Value::Text("one".into())]);
    insert_committed(&h.tm, &*p, vec![Value::Int4(2), Value::Text("two".into())]);
    h.engine.create_index("public", "p", idx("p_idx"))?;
    assert!(p.supports_index_scan("p_idx"));
    drop(_t);
    drop(p);

    let h2 = reopen(h, true);
    let p2 = h2.engine.open_table("p")?;
    assert!(
        p2.supports_index_scan("p_idx"),
        "permanent index silently downgraded by an IXR1 desync"
    );
    let rows: Vec<Value> = p2
        .index_lookup("p_idx", &[Value::Int4(2)], &read(&h2.tm))
        .expect("physical index should serve the probe")
        .map(|(_, t)| t[1].clone())
        .collect();
    assert_eq!(rows, vec![Value::Text("two".into())]);
    Ok(())
}

// ---- Unlogged (on-disk, WAL-skipped, crash-truncated) -----------------------

#[test]
fn unlogged_table_is_on_disk() -> anyhow::Result<()> {
    let h = setup();
    let u = h.engine.create_table(schema("u", RelPersistence::Unlogged))?;
    insert_committed(&h.tm, &*u, vec![Value::Int4(1), Value::Text("one".into())]);
    // Unlogged is file-backed (unlike Temporary): a heap file exists under base/.
    assert!(base_file_count(&h) >= 1, "an Unlogged table must be on disk");
    Ok(())
}

#[test]
fn unlogged_survives_clean_restart() -> anyhow::Result<()> {
    let h = setup();
    let u = h.engine.create_table(schema("u", RelPersistence::Unlogged))?;
    let p = h.engine.create_table(schema("p", RelPersistence::Permanent))?;
    insert_committed(&h.tm, &*u, vec![Value::Int4(1), Value::Null]);
    insert_committed(&h.tm, &*u, vec![Value::Int4(2), Value::Null]);
    insert_committed(&h.tm, &*p, vec![Value::Int4(9), Value::Null]);
    drop(u);
    drop(p);

    let h2 = reopen(h, true); // clean shutdown
    // The Unlogged table's definition AND data survive a clean restart.
    let u = h2.engine.open_table("u")?;
    assert_eq!(ids(&h2.tm, &*u), vec![1, 2]);
    let p = h2.engine.open_table("p")?;
    assert_eq!(ids(&h2.tm, &*p), vec![9]);
    Ok(())
}

#[test]
fn unlogged_truncated_after_crash() -> anyhow::Result<()> {
    let h = setup();
    let u = h.engine.create_table(schema("u", RelPersistence::Unlogged))?;
    let p = h.engine.create_table(schema("p", RelPersistence::Permanent))?;
    insert_committed(&h.tm, &*u, vec![Value::Int4(1), Value::Null]);
    insert_committed(&h.tm, &*p, vec![Value::Int4(9), Value::Null]);
    // Flush the Unlogged rows to their file (a running checkpoint keeps the control
    // file dirty), so the reset genuinely has torn-looking data to discard — without
    // it this test would pass even if the reset never ran.
    h.engine.checkpoint(h.tm.allocate_xid())?;
    drop(u);
    drop(p);

    let h2 = reopen(h, false); // crash: no clean shutdown
    // The Unlogged table's DEFINITION survives, but its data is reset to empty.
    let u = h2.engine.open_table("u")?;
    assert_eq!(ids(&h2.tm, &*u), Vec::<i32>::new(), "Unlogged data reset on crash");
    // The permanent table's committed row is recovered from the WAL.
    let p = h2.engine.open_table("p")?;
    assert_eq!(ids(&h2.tm, &*p), vec![9]);
    Ok(())
}

#[test]
fn unlogged_index_is_physical_survives_clean_and_resets_on_crash() -> anyhow::Result<()> {
    let h = setup();
    let u = h.engine.create_table(schema("u", RelPersistence::Unlogged))?;
    insert_committed(&h.tm, &*u, vec![Value::Int4(1), Value::Text("one".into())]);
    insert_committed(&h.tm, &*u, vec![Value::Int4(2), Value::Text("two".into())]);
    h.engine.create_index("public", "u", idx("u_idx"))?;
    // An Unlogged table gets a real physical B-tree (unlike Temporary).
    assert!(u.supports_index_scan("u_idx"), "Unlogged index should be physical");
    // Flush heap + index to disk so both the clean-restart and crash-reset paths
    // operate on real on-disk state.
    h.engine.checkpoint(h.tm.allocate_xid())?;
    drop(u);

    // Clean restart: the index still probes and returns the row.
    let h2 = reopen(h, true);
    let u = h2.engine.open_table("u")?;
    assert!(u.supports_index_scan("u_idx"));
    let hit: Vec<Value> = u
        .index_lookup("u_idx", &[Value::Int4(2)], &read(&h2.tm))
        .expect("physical index after clean restart")
        .map(|(_, t)| t[1].clone())
        .collect();
    assert_eq!(hit, vec![Value::Text("two".into())]);
    drop(u);

    // Crash restart: the table is reset empty and the index is re-laid as a VALID
    // empty B-tree (a probe returns nothing rather than faulting on a torn meta page).
    let h3 = reopen(h2, false);
    let u = h3.engine.open_table("u")?;
    assert_eq!(ids(&h3.tm, &*u), Vec::<i32>::new());
    assert!(u.supports_index_scan("u_idx"));
    let miss = u
        .index_lookup("u_idx", &[Value::Int4(2)], &read(&h3.tm))
        .expect("empty index still serves probes")
        .count();
    assert_eq!(miss, 0);
    Ok(())
}
