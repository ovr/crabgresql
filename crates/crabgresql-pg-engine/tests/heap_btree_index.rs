//! End-to-end coverage of the durable heap engine's physical B-tree index:
//! build, probe, maintenance across insert/update/delete, MVCC-correct
//! visibility at probe time, NULL and un-indexable keys, page splits over many
//! rows and duplicates, vacuum reclaiming index entries (so a reused heap slot
//! is never reachable by a stale key), crash recovery, and file lifecycle.

use crabgresql_storage_api::{
    Column, ColumnProjection, IndexConstraint, IndexKey, IndexMetadata, IndexMethod, StorageError,
    TableAm, TableEngine, TableSchema, Tid,
};
use crabgresql_txn::{CommandId, TransactionManager, TxnContext, Xid};
use crabgresql_types::{PgType, Value};

mod common;
use common::open;

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            Column::new("id", PgType::Int4),
            Column::new("name", PgType::Text),
        ],
    )
}

/// A non-unique B-tree index named `t_id_idx` on column 0 (`id`).
fn idx_on_id() -> IndexMetadata {
    IndexMetadata {
        name: "t_id_idx".into(),
        method: IndexMethod::BTree,
        keys: vec![IndexKey {
            column: 0,
            descending: false,
            nulls_first: false,
        }],
        unique: false,
        nulls_distinct: true,
        constraint: Some(IndexConstraint::Unique),
    }
}

/// A second non-unique index, on column 1 (`name`), for the multi-index cases.
fn idx_on_name() -> IndexMetadata {
    IndexMetadata {
        name: "t_name_idx".into(),
        keys: vec![IndexKey {
            column: 1,
            descending: false,
            nulls_first: false,
        }],
        constraint: None,
        ..idx_on_id()
    }
}

fn read(tm: &TransactionManager) -> TxnContext {
    tm.context(Xid::INVALID, CommandId::FIRST)
}

fn insert_committed(tm: &TransactionManager, table: &dyn TableAm, id: i32, name: &str) -> Tid {
    let x = tm.allocate_xid();
    let tid = table.insert(
        vec![Value::Int4(id), Value::Text(name.into())],
        &tm.context(x, CommandId::FIRST),
    );
    tm.commit(x).expect("commit");
    tid.unwrap_or_else(|error| panic!("insert failed: {error}"))
}

/// Probe `key` and return the visible `id`s, sorted.
fn probe_ids(table: &dyn TableAm, txn: &TxnContext, key: i32) -> Vec<i32> {
    let mut v: Vec<i32> = table
        .index_lookup("t_id_idx", &[Value::Int4(key)], txn)
        .expect("index serves the probe")
        .map(|row| match &row.expect("index probe failed").1[0] {
            Value::Int4(x) => *x,
            other => panic!("unexpected id value {other:?}"),
        })
        .collect();
    v.sort();
    v
}

/// The rows a seq scan + key filter would return, sorted by id — the oracle the
/// index probe must agree with.
fn scan_ids(table: &dyn TableAm, txn: &TxnContext, key: i32) -> Vec<i32> {
    let mut v: Vec<i32> = table
        .scan(txn, &ColumnProjection::All)
        .map(|row| row.unwrap_or_else(|error| panic!("scan failed: {error}")))
        .filter(|(_, t)| t[0] == Value::Int4(key))
        .map(|(_, t)| match &t[0] {
            Value::Int4(x) => *x,
            other => panic!("unexpected id value {other:?}"),
        })
        .collect();
    v.sort();
    v
}

#[test]
fn builds_and_probes_and_maintains_on_later_insert() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(schema("t"))?;
    // Rows present before CREATE INDEX are picked up by the build scan.
    insert_committed(&tm, &*table, 1, "a");
    insert_committed(&tm, &*table, 2, "b");
    engine.create_index("public", "t", idx_on_id())?;
    assert!(table.supports_index_scan("t_id_idx"));

    // A row inserted after CREATE INDEX is maintained into the tree.
    insert_committed(&tm, &*table, 3, "c");

    for k in [1, 2, 3] {
        assert_eq!(probe_ids(&*table, &read(&tm), k), vec![k]);
    }
    // A missing key is served as empty, and the probe agrees with a scan.
    assert!(probe_ids(&*table, &read(&tm), 99).is_empty());
    for k in [1, 2, 3, 99] {
        assert_eq!(
            probe_ids(&*table, &read(&tm), k),
            scan_ids(&*table, &read(&tm), k)
        );
    }
    Ok(())
}

#[test]
fn many_rows_force_splits_and_every_key_is_findable() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(schema("t"))?;
    engine.create_index("public", "t", idx_on_id())?;

    // Enough rows to split leaves several times and grow at least one internal
    // level. Insert in a shuffled order so descent/splits are exercised, not just
    // right-edge appends.
    const N: i32 = 4000;
    let x = tm.allocate_xid();
    let ctx = tm.context(x, CommandId::FIRST);
    let mut id = 1i32;
    for _ in 0..N {
        table.insert(vec![Value::Int4(id), Value::Text("v".into())], &ctx)?;
        // A simple full-period LCG over [0, N) reordered by stepping a coprime.
        id = (id + 1237) % N;
    }
    tm.commit(x)?;

    // Every key resolves to exactly its row, across the whole range and at the
    // edges; a key past the end is empty.
    for k in (0..N).step_by(97) {
        assert_eq!(probe_ids(&*table, &read(&tm), k), vec![k], "probe {k}");
    }
    assert_eq!(probe_ids(&*table, &read(&tm), 0), vec![0]);
    assert_eq!(probe_ids(&*table, &read(&tm), N - 1), vec![N - 1]);
    assert!(probe_ids(&*table, &read(&tm), N).is_empty());
    Ok(())
}

#[test]
fn duplicate_keys_return_every_matching_row_across_split_leaves() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(schema("t"))?;
    engine.create_index("public", "t", idx_on_id())?;

    // Many rows with the SAME key (7): the run spills across leaf pages, so the
    // probe must follow right-links to gather them all. Distinguish them by name
    // length via distinct payloads and count them.
    let x = tm.allocate_xid();
    let ctx = tm.context(x, CommandId::FIRST);
    const DUPES: usize = 2000;
    for i in 0..DUPES {
        table.insert(vec![Value::Int4(7), Value::Text(format!("d{i}"))], &ctx)?;
    }
    // A couple of other keys so the duplicate run has neighbors.
    table.insert(vec![Value::Int4(1), Value::Text("one".into())], &ctx)?;
    table.insert(vec![Value::Int4(9), Value::Text("nine".into())], &ctx)?;
    tm.commit(x)?;

    let hits = table
        .index_lookup("t_id_idx", &[Value::Int4(7)], &read(&tm))
        .expect("served")
        .count();
    assert_eq!(hits, DUPES, "every duplicate of key 7 is returned");
    assert_eq!(probe_ids(&*table, &read(&tm), 1), vec![1]);
    assert_eq!(probe_ids(&*table, &read(&tm), 9), vec![9]);
    Ok(())
}

#[test]
fn update_moves_key_and_old_snapshot_still_sees_old_version() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(schema("t"))?;
    engine.create_index("public", "t", idx_on_id())?;
    let tid = insert_committed(&tm, &*table, 1, "a");

    // Snapshot taken BEFORE the update.
    let before = read(&tm);

    // Update the key column 1 -> 2, committed.
    let xu = tm.allocate_xid();
    table.update(
        tid,
        vec![Value::Int4(2), Value::Text("a2".into())],
        &tm.context(xu, CommandId::FIRST),
    )?;
    tm.commit(xu)?;

    // Under the pre-update snapshot: key 1 still finds the old version, key 2 is
    // not yet visible (the index entry re-checks visibility via the heap).
    assert_eq!(probe_ids(&*table, &before, 1), vec![1]);
    assert!(probe_ids(&*table, &before, 2).is_empty());

    // Under a fresh snapshot: key 1 is gone, key 2 is the current row.
    let after = read(&tm);
    assert!(probe_ids(&*table, &after, 1).is_empty());
    assert_eq!(probe_ids(&*table, &after, 2), vec![2]);
    Ok(())
}

#[test]
fn delete_hides_the_row_from_the_probe() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(schema("t"))?;
    engine.create_index("public", "t", idx_on_id())?;
    let tid = insert_committed(&tm, &*table, 5, "five");
    let before = read(&tm);

    let xd = tm.allocate_xid();
    table.delete(tid, &tm.context(xd, CommandId::FIRST))?;
    tm.commit(xd)?;

    // The committer's later snapshot sees nothing; the pre-delete snapshot still
    // sees the row (MVCC re-fetch through the index entry).
    assert!(probe_ids(&*table, &read(&tm), 5).is_empty());
    assert_eq!(probe_ids(&*table, &before, 5), vec![5]);
    Ok(())
}

#[test]
fn null_key_is_not_indexed_and_probing_null_is_empty() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(schema("t"))?;
    engine.create_index("public", "t", idx_on_id())?;
    // Row whose key column is NULL: not indexed (NULL never satisfies equality).
    let x = tm.allocate_xid();
    table.insert(
        vec![Value::Null, Value::Text("null-key".into())],
        &tm.context(x, CommandId::FIRST),
    )?;
    table.insert(
        vec![Value::Int4(1), Value::Text("one".into())],
        &tm.context(x, CommandId::FIRST),
    )?;
    tm.commit(x)?;

    // A NULL probe is served (the index is physical) but matches nothing.
    let null_hits = table
        .index_lookup("t_id_idx", &[Value::Null], &read(&tm))
        .expect("served")
        .count();
    assert_eq!(null_hits, 0);
    assert_eq!(probe_ids(&*table, &read(&tm), 1), vec![1]);
    Ok(())
}

#[test]
fn un_indexable_key_type_falls_back_to_scan() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    // float8 has no order-preserving equality-canonical encoding in this cut, so
    // an index on it is metadata-only: no physical scan, probe falls back.
    let s = TableSchema::new("t", vec![Column::new("f", PgType::Float8)]);
    let table = engine.create_table(s)?;
    let x = tm.allocate_xid();
    table.insert(vec![Value::Float8(1.5)], &tm.context(x, CommandId::FIRST))?;
    tm.commit(x)?;
    engine.create_index(
        "public",
        "t",
        IndexMetadata {
            name: "t_id_idx".into(),
            method: IndexMethod::BTree,
            keys: vec![IndexKey {
                column: 0,
                descending: false,
                nulls_first: false,
            }],
            unique: false,
            nulls_distinct: true,
            constraint: None,
        },
    )?;
    assert!(!table.supports_index_scan("t_id_idx"));
    assert!(
        table
            .index_lookup("t_id_idx", &[Value::Float8(1.5)], &read(&tm))
            .is_none(),
        "an un-indexable key type falls back to a scan"
    );
    Ok(())
}

#[test]
fn vacuum_removes_the_index_entry_so_a_reused_slot_is_not_found() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(schema("t"))?;
    engine.create_index("public", "t", idx_on_id())?;
    let tid = insert_committed(&tm, &*table, 1, "a");

    // Delete row id=1 and vacuum past its deleter, reclaiming both the heap slot
    // and its index entry.
    let xd = tm.allocate_xid();
    table.delete(tid, &tm.context(xd, CommandId::FIRST))?;
    tm.commit(xd)?;
    let horizon = tm.allocate_xid();
    table.vacuum(horizon, tm.clog());

    // Insert a new row id=2; it reuses the freed heap slot (the same tid).
    let reused = insert_committed(&tm, &*table, 2, "b");
    assert_eq!(reused, tid, "the freed slot is reused, so the tids collide");

    // Probing the OLD key must not surface the new row: vacuum removed the stale
    // (key 1 -> tid) entry. Probing the new key finds the new row.
    assert!(
        probe_ids(&*table, &read(&tm), 1).is_empty(),
        "stale key is gone"
    );
    assert_eq!(probe_ids(&*table, &read(&tm), 2), vec![2]);
    Ok(())
}

/// A TRUNCATE swaps in a fresh, empty heap file and resets the insert hint, so
/// the rows inserted after it reuse the very tids the pre-truncate index entries
/// name. The index must therefore be swapped in lockstep with the heap — a probe
/// on an old key that surfaced a post-truncate row would be a wrong answer, not
/// a stale-but-invisible one, because the row is genuinely live and visible.
#[test]
fn truncate_then_insert_does_not_return_stale_index_rows() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(schema("t"))?;
    engine.create_index("public", "t", idx_on_id())?;
    for id in 1..=5 {
        insert_committed(&tm, &*table, id, "before");
    }

    let xt = tm.allocate_xid();
    table.truncate(&tm.context(xt, CommandId::FIRST))?;
    tm.commit(xt)?;

    insert_committed(&tm, &*table, 99, "after");

    for k in 1..=5 {
        assert!(
            probe_ids(&*table, &read(&tm), k).is_empty(),
            "key {k} was truncated away, but the probe still finds it"
        );
    }
    assert_eq!(probe_ids(&*table, &read(&tm), 99), vec![99]);
    Ok(())
}

/// The invariant the executor's `IndexScan` relies on: on the physical path it
/// performs no key re-check, so a probe must agree with a filtered scan exactly.
#[test]
fn truncate_then_insert_probe_agrees_with_scan() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(schema("t"))?;
    engine.create_index("public", "t", idx_on_id())?;
    for id in 1..=5 {
        insert_committed(&tm, &*table, id, "before");
    }
    let xt = tm.allocate_xid();
    table.truncate(&tm.context(xt, CommandId::FIRST))?;
    tm.commit(xt)?;
    for id in [3, 7] {
        insert_committed(&tm, &*table, id, "after");
    }

    for k in [1, 2, 3, 4, 5, 7, 99] {
        assert_eq!(
            probe_ids(&*table, &read(&tm), k),
            scan_ids(&*table, &read(&tm), k),
            "probe and scan disagree on key {k}"
        );
    }
    Ok(())
}

/// `TRUNCATE t; INSERT ...` inside ONE transaction: the truncating transaction
/// must probe its own new rows and none of the old ones. This is the case an
/// in-place index reset at commit time could not serve, since the rows exist
/// before the commit hook runs.
#[test]
fn truncate_and_insert_in_one_txn_then_commit() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(schema("t"))?;
    engine.create_index("public", "t", idx_on_id())?;
    for id in 1..=3 {
        insert_committed(&tm, &*table, id, "before");
    }

    let x = tm.allocate_xid();
    let txn = tm.context(x, CommandId::FIRST);
    table.truncate(&txn)?;
    table.insert(vec![Value::Int4(42), Value::Text("after".into())], &txn)?;
    // Inside the transaction: the staged index serves the new row only. Read at
    // the NEXT command id — a statement does not see its own writes.
    let later = tm.context(x, CommandId(1));
    assert!(probe_ids(&*table, &later, 1).is_empty());
    assert_eq!(probe_ids(&*table, &later, 42), vec![42]);
    tm.commit(x)?;

    assert!(probe_ids(&*table, &read(&tm), 1).is_empty());
    assert_eq!(probe_ids(&*table, &read(&tm), 42), vec![42]);
    Ok(())
}

/// A rolled-back TRUNCATE leaves the committed index exactly as it was, and the
/// rows the aborted transaction inserted are not reachable through it.
#[test]
fn rolled_back_truncate_leaves_the_index_intact() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(schema("t"))?;
    engine.create_index("public", "t", idx_on_id())?;
    for id in 1..=3 {
        insert_committed(&tm, &*table, id, "before");
    }

    let x = tm.allocate_xid();
    let txn = tm.context(x, CommandId::FIRST);
    table.truncate(&txn)?;
    table.insert(vec![Value::Int4(42), Value::Text("gone".into())], &txn)?;
    tm.abort(x);

    for k in 1..=3 {
        assert_eq!(probe_ids(&*table, &read(&tm), k), vec![k]);
    }
    assert!(probe_ids(&*table, &read(&tm), 42).is_empty());
    Ok(())
}

/// The swap replaces every index file and leaks none: the pre-truncate trees are
/// unlinked, the post-truncate ones exist, and the file count is unchanged.
#[test]
fn truncate_swaps_every_index_relfilenode_and_leaks_none() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(schema("t"))?;
    engine.create_index("public", "t", idx_on_id())?;
    engine.create_index("public", "t", idx_on_name())?;
    insert_committed(&tm, &*table, 1, "a");
    let before = base_files(dir.path());

    let xt = tm.allocate_xid();
    table.truncate(&tm.context(xt, CommandId::FIRST))?;
    tm.commit(xt)?;

    let after = base_files(dir.path());
    assert_eq!(
        after.len(),
        before.len(),
        "one file per relation before and after: {before:?} -> {after:?}"
    );
    assert!(
        after.iter().all(|f| !before.contains(f)),
        "every file is a fresh relfilenode: {before:?} -> {after:?}"
    );
    Ok(())
}

/// Two TRUNCATEs in one transaction: the first transaction's superseded staged
/// files — the heap's AND each index's — are reclaimed, not leaked.
#[test]
fn double_truncate_in_one_txn_leaks_no_index_file() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(schema("t"))?;
    engine.create_index("public", "t", idx_on_id())?;
    insert_committed(&tm, &*table, 1, "a");
    let before = base_files(dir.path()).len();

    let x = tm.allocate_xid();
    let txn = tm.context(x, CommandId::FIRST);
    table.truncate(&txn)?;
    table.truncate(&txn)?;
    tm.commit(x)?;

    assert_eq!(base_files(dir.path()).len(), before);
    assert!(probe_ids(&*table, &read(&tm), 1).is_empty());
    Ok(())
}

/// An index created after a TRUNCATE keeps its own relfilenode and indexes only
/// the post-truncate rows — the swap must not repoint it.
#[test]
fn truncate_then_create_index_indexes_only_the_new_rows() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(schema("t"))?;
    for id in 1..=3 {
        insert_committed(&tm, &*table, id, "before");
    }
    let xt = tm.allocate_xid();
    table.truncate(&tm.context(xt, CommandId::FIRST))?;
    tm.commit(xt)?;
    insert_committed(&tm, &*table, 7, "after");

    engine.create_index("public", "t", idx_on_id())?;
    assert_eq!(probe_ids(&*table, &read(&tm), 7), vec![7]);
    for k in [1, 2, 3] {
        assert!(probe_ids(&*table, &read(&tm), k).is_empty());
    }
    Ok(())
}

/// Replay bounded at a checkpoint's redo point keeps a heavily split B-tree
/// intact — and provably never reads the log below that point.
///
/// The prefix below `redo` is scribbled over before reopening. If recovery
/// touched one byte of it the first `decode` would fail, the log would read as
/// empty, and every row would vanish — so this cannot pass by accident
/// (verified: forcing `recover` back to offset 0 fails it).
///
/// Everything is inserted by ONE transaction that commits *above* the redo
/// point, deliberately. The CLOG is still a RAM `HashMap` rebuilt from replay,
/// so a transaction whose commit record sat below redo would come back
/// `InProgress` and all its rows would be invisible — nothing to do with
/// splits. Making commit status survive a bounded replay is the durable-CLOG
/// work; this test isolates the page-level behaviour from it.
///
/// The *timing* hazard in `split_page` is guarded separately, by
/// `nbtree::tests::no_page_stays_dirty_at_or_below_a_sampled_redo_point` and by
/// the `CheckpointDelay` tests in `crabgresql-wal`.
#[test]
fn a_bounded_replay_after_a_checkpoint_keeps_every_split_reachable() -> anyhow::Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    const ROWS: i32 = 4_000;
    let dir = tempfile::tempdir()?;
    let redo = Arc::new(AtomicU64::new(0));

    // --- lifetime 1: split under a concurrently looping checkpointer. ---
    {
        let (engine, tm, wal) =
            common::open_from_with_wal(dir.path(), crabgresql_wal::Lsn::INVALID)?;
        let table = engine.create_table(schema("t"))?;
        engine.create_index("public", "t", idx_on_id())?;

        let done = Arc::new(AtomicBool::new(false));
        let x = tm.allocate_xid();
        std::thread::scope(|s| -> anyhow::Result<()> {
            let checkpointer = {
                let (engine, wal, done, redo) = (
                    Arc::clone(&engine),
                    Arc::clone(&wal),
                    Arc::clone(&done),
                    Arc::clone(&redo),
                );
                let next_xid = Xid(x.0 + 1);
                s.spawn(move || -> anyhow::Result<()> {
                    while !done.load(Ordering::SeqCst) {
                        // The ordering a real checkpointer must use: sample redo
                        // BEFORE flushing buffers. `redo_point` makes the point
                        // itself durable, so nothing here can name a byte past
                        // the end of the on-disk log.
                        let point = wal.redo_point()?;
                        engine.checkpoint(next_xid)?;
                        redo.fetch_max(point.0, Ordering::SeqCst);
                    }
                    Ok(())
                })
            };

            // Shuffled insert order so descent and splits are exercised, not just
            // right-edge appends.
            let ctx = tm.context(x, CommandId::FIRST);
            let mut id = 1i32;
            for _ in 0..ROWS {
                table.insert(vec![Value::Int4(id), Value::Text("v".into())], &ctx)?;
                id = (id + 1237) % ROWS;
            }
            done.store(true, Ordering::SeqCst);
            checkpointer
                .join()
                .map_err(|_| anyhow::anyhow!("checkpointer thread panicked"))?
        })?;

        // Commit only after the last redo sample, so the commit record is above
        // redo and the rebuilt CLOG still learns this transaction's fate.
        tm.commit(x)?;
        wal.flush(wal.current_lsn())?;
    }

    // --- lifetime 2: destroy the prefix below redo, then replay from redo. ---
    let redo = crabgresql_wal::Lsn(redo.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        redo.is_valid(),
        "the checkpointer never sampled a redo point"
    );
    common::scribble_wal_below(dir.path(), redo)?;
    {
        let (engine, tm) = common::open_from(dir.path(), redo)?;
        let table = engine.open_table("t")?;
        assert!(table.supports_index_scan("t_id_idx"));
        // Every key must still be index-reachable, and the index must agree with
        // a sequential scan — a lost split shows up as a missing key or as a
        // descent into an empty page.
        for id in (0..ROWS).step_by(89) {
            let txn = read(&tm);
            assert_eq!(
                probe_ids(&*table, &txn, id),
                scan_ids(&*table, &txn, id),
                "index and heap disagree on key {id}"
            );
            assert_eq!(
                probe_ids(&*table, &txn, id),
                vec![id],
                "key {id} unreachable after replay from {redo}"
            );
        }
    }
    Ok(())
}

#[test]
fn index_survives_a_crash_and_serves_probes_after_recovery() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // --- lifetime 1: build an index, then "crash" (drop without checkpoint). ---
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.create_table(schema("t"))?;
        insert_committed(&tm, &*table, 1, "a");
        insert_committed(&tm, &*table, 2, "b");
        engine.create_index("public", "t", idx_on_id())?;
        // Enough post-index inserts to split at least one leaf, all committed so
        // both the build and maintenance WAL are fsynced.
        let x = tm.allocate_xid();
        let ctx = tm.context(x, CommandId::FIRST);
        for id in 3..800 {
            table.insert(vec![Value::Int4(id), Value::Text("v".into())], &ctx)?;
        }
        tm.commit(x)?;
    }
    // --- lifetime 2: recover and probe. ---
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.open_table("t")?;
        // The physical index survived (its file was not GC'd as an orphan) and
        // serves probes reconstructed purely from replayed WAL.
        assert!(table.supports_index_scan("t_id_idx"));
        for k in [1, 2, 3, 400, 799] {
            assert_eq!(
                probe_ids(&*table, &read(&tm), k),
                vec![k],
                "post-recovery probe {k}"
            );
        }
        assert!(probe_ids(&*table, &read(&tm), 5000).is_empty());
        // The recovered index keeps working for new rows.
        insert_committed(&tm, &*table, 5000, "new");
        assert_eq!(probe_ids(&*table, &read(&tm), 5000), vec![5000]);
    }
    Ok(())
}

#[test]
fn drop_index_unlinks_its_file_and_relfilenodes_stay_monotonic() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(schema("t"))?;
    insert_committed(&tm, &*table, 1, "a");
    let heap_only = base_files(dir.path());

    // CREATE INDEX adds exactly one new base file (the index's B-tree).
    engine.create_index("public", "t", idx_on_id())?;
    let after_create = base_files(dir.path());
    let first_idx = *after_create
        .iter()
        .find(|f| !heap_only.contains(f))
        .expect("create_index added an index file");

    // DROP INDEX unlinks that file and unpublishes the index.
    engine.drop_index("public", "t", "t_id_idx")?;
    assert!(!table.supports_index_scan("t_id_idx"));
    assert!(
        !base_files(dir.path()).contains(&first_idx),
        "drop_index unlinks the physical file"
    );

    // Re-create: the new index gets a higher relfilenode (never reused).
    engine.create_index("public", "t", idx_on_id())?;
    let second_idx = *base_files(dir.path())
        .iter()
        .filter(|f| !heap_only.contains(f))
        .max()
        .expect("re-create added an index file");
    assert!(second_idx > first_idx, "relfilenode counter is monotonic");
    Ok(())
}

/// A B-tree index named `t_s_idx` on a single text column (column 0).
fn idx_on_text() -> IndexMetadata {
    IndexMetadata {
        name: "t_s_idx".into(),
        method: IndexMethod::BTree,
        keys: vec![IndexKey {
            column: 0,
            descending: false,
            nulls_first: false,
        }],
        unique: false,
        nulls_distinct: true,
        constraint: None,
    }
}

#[test]
fn large_and_mixed_size_keys_split_without_panic_or_corruption() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(TableSchema::new("t", vec![Column::new("s", PgType::Text)]))?;
    engine.create_index("public", "t", idx_on_text())?;

    // Keys near the B-tree item cap (~1990 bytes), mixed with tiny keys, inserted
    // so the byte-based split point repeatedly lands next to the sizing bound.
    // Before the sizing fix this could panic in choose_split ("no feasible split
    // point") or overflow a page in put_item_at.
    let x = tm.allocate_xid();
    let ctx = tm.context(x, CommandId::FIRST);
    let mut keys: Vec<String> = Vec::new();
    for i in 0..300u32 {
        // Padding sizes chosen so the encoded item stays under the ~2000-byte cap
        // (item = 16 + padding), while still forcing near-boundary split points.
        let len = match i % 3 {
            0 => 3,
            1 => 1000,
            _ => 1980,
        };
        // Unique, order-varied keys: a 6-char index prefix then padding.
        let key = format!("{i:06}{}", "x".repeat(len));
        keys.push(key.clone());
        table.insert(vec![Value::Text(key)], &ctx)?;
    }
    tm.commit(x)?;

    // Every key resolves to exactly one row.
    for key in &keys {
        let hits = table
            .index_lookup("t_s_idx", &[Value::Text(key.clone())], &read(&tm))
            .expect("served")
            .count();
        assert_eq!(hits, 1, "probe for a {}-byte key", key.len());
    }
    // A never-inserted key is empty.
    assert!(
        table
            .index_lookup("t_s_idx", &[Value::Text("nope".into())], &read(&tm))
            .expect("served")
            .count()
            == 0
    );
    Ok(())
}

#[test]
fn metadata_only_index_allocates_no_file_and_no_relfilenode() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    // float8 is not order-preserving-encodable → metadata-only index.
    let table = engine.create_table(TableSchema::new(
        "t",
        vec![Column::new("f", PgType::Float8)],
    ))?;
    let x = tm.allocate_xid();
    table.insert(vec![Value::Float8(1.5)], &tm.context(x, CommandId::FIRST))?;
    tm.commit(x)?;
    let before = base_files(dir.path());

    engine.create_index(
        "public",
        "t",
        IndexMetadata {
            name: "t_f_idx".into(),
            method: IndexMethod::BTree,
            keys: vec![IndexKey {
                column: 0,
                descending: false,
                nulls_first: false,
            }],
            unique: false,
            nulls_distinct: true,
            constraint: None,
        },
    )?;

    // No physical file was created for a metadata-only index (relfilenode 0).
    assert_eq!(
        base_files(dir.path()),
        before,
        "metadata-only index creates no file"
    );
    assert!(!table.supports_index_scan("t_f_idx"));
    Ok(())
}

#[test]
fn oversized_key_fails_create_index_without_freezing_the_table() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(TableSchema::new("t", vec![Column::new("s", PgType::Text)]))?;
    // A value whose encoded key exceeds the B-tree item cap.
    let x = tm.allocate_xid();
    table.insert(
        vec![Value::Text("z".repeat(4000))],
        &tm.context(x, CommandId::FIRST),
    )?;
    tm.commit(x)?;

    // The build reports the key, naming the index and the row it came from.
    match engine.create_index("public", "t", idx_on_text()) {
        Err(StorageError::IndexRowTooBig {
            size, max, index, ..
        }) => {
            assert!(size > max, "{size} should exceed {max}");
            assert_eq!(index, "t_s_idx");
        }
        other => panic!("expected IndexRowTooBig, got {other:?}"),
    }

    // The table's exclusive lock was released by the RAII guard, so the table is
    // still usable (this would hang/deadlock if the lock leaked).
    let x = tm.allocate_xid();
    table.insert(
        vec![Value::Text("small".into())],
        &tm.context(x, CommandId::FIRST),
    )?;
    tm.commit(x)?;
    assert_eq!(table.scan(&read(&tm), &ColumnProjection::All).count(), 2);
    // The failed index was never published.
    assert!(!table.supports_index_scan("t_s_idx"));
    Ok(())
}

#[test]
fn oversized_key_fails_the_insert_that_carries_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(TableSchema::new("t", vec![Column::new("s", PgType::Text)]))?;
    engine.create_index("public", "t", idx_on_text())?;

    let x = tm.allocate_xid();
    let txn = tm.context(x, CommandId::FIRST);
    match table.insert(vec![Value::Text("z".repeat(4000))], &txn) {
        Err(error @ StorageError::IndexRowTooBig { .. }) => {
            // PostgreSQL's DETAIL and HINT, which the row-too-big siblings have
            // no equivalent of.
            assert!(
                error
                    .detail()
                    .is_some_and(|d| d.contains("in relation \"t\"")),
                "{error:?}"
            );
            assert!(error.hint().is_some_and(|h| h.contains("1/3 of a buffer")));
        }
        other => panic!("expected IndexRowTooBig, got {other:?}"),
    }
    // A key that fits still goes in on the same table.
    table.insert(vec![Value::Text("small".into())], &txn)?;
    tm.commit(x)?;
    let found = table
        .index_lookup("t_s_idx", &[Value::Text("small".into())], &read(&tm))
        .expect("the index serves probes")
        .count();
    assert_eq!(found, 1, "the fitting key is indexed");
    Ok(())
}

/// The relfilenode numbers of the files under `<dir>/base`.
fn base_files(dir: &std::path::Path) -> Vec<u32> {
    let mut v: Vec<u32> = std::fs::read_dir(dir.join("base"))
        .expect("base dir exists")
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()))
        .collect();
    v.sort();
    v
}
