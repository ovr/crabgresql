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
        .map(
            |row| match row.unwrap_or_else(|error| panic!("scan failed: {error}")).1[0] {
                Value::Int4(x) => x,
                _ => unreachable!(),
            },
        )
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
    let engine2 = PgEngine::new_with_pool(
        dir.path(),
        Arc::clone(&wal),
        &mut reg,
        crabgresql_pg_engine::BufferPoolPolicy::minimal(),
    )?;
    let clog = Arc::new(Clog::new());
    let res = recover(dir.path(), &reg, &clog, crabgresql_wal::Lsn::INVALID)?;
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
fn seed_three(engine: &PgEngine, tm: &TransactionManager) -> anyhow::Result<Arc<dyn TableAm>> {
    let table = engine.create_table(schema())?;
    let x = tm.allocate_xid();
    let ctx = tm.context(x, CommandId::FIRST);
    insert(&*table, &ctx, 1, "a");
    insert(&*table, &ctx, 2, "b");
    insert(&*table, &ctx, 3, "c");
    tm.commit(x)?;
    Ok(table)
}

#[test]
fn truncate_committed_then_crash_recovers_empty() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_three(&engine, &tm)?;
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST))?;
        tm.commit(tx)?;
        assert_eq!(visible_ids(&tm, &*table), Vec::<i32>::new());
        // Crash: drop without checkpoint.
    }
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    assert_eq!(visible_ids(&tm, &*table), Vec::<i32>::new());
    Ok(())
}

#[test]
fn truncate_uncommitted_then_crash_restores_rows() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_three(&engine, &tm)?;
        let tx = tm.allocate_xid();
        let ctx = tm.context(tx, CommandId::FIRST);
        table.truncate(&ctx)?;
        // Read-your-own-truncate: the truncater sees its own now-empty table
        // (reading under its OWN xid; a concurrent reader would block on the
        // AccessExclusive lock until this transaction ends).
        let own: Vec<i32> = table
            .scan(&ctx, &ColumnProjection::All)
            .map(
                |row| match row.unwrap_or_else(|error| panic!("scan failed: {error}")).1[0] {
                    Value::Int4(x) => x,
                    _ => unreachable!(),
                },
            )
            .collect();
        assert_eq!(own, Vec::<i32>::new());
        // Never commits: crash (drop) with the swap still pending.
    }
    // Recovery keeps the original file because the swap's XID never committed.
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3]);
    Ok(())
}

#[test]
fn truncate_rolled_back_restores_rows_in_place_and_after_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = seed_three(&engine, &tm)?;
    let tx = tm.allocate_xid();
    table.truncate(&tm.context(tx, CommandId::FIRST))?;
    tm.abort(tx);
    // Explicit abort restores the rows immediately (the old file was untouched).
    assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3]);
    drop((engine, tm));
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3]);
    Ok(())
}

#[test]
fn truncate_then_insert_then_commit_crash_keeps_only_new_rows() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_three(&engine, &tm)?;
        let tx = tm.allocate_xid();
        let ctx = tm.context(tx, CommandId::FIRST);
        table.truncate(&ctx)?;
        // Post-truncate inserts land in the new file (read-your-own-truncate).
        insert(&*table, &ctx, 10, "x");
        insert(&*table, &ctx, 11, "y");
        tm.commit(tx)?;
        assert_eq!(visible_ids(&tm, &*table), vec![10, 11]);
    }
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    assert_eq!(visible_ids(&tm, &*table), vec![10, 11]);
    Ok(())
}

#[test]
fn truncate_crash_then_truncate_again_commit_is_consistent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_three(&engine, &tm)?;
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST))?;
        // Crash before commit.
    }
    {
        // Rows are back; truncate again and commit this time.
        let (engine, tm) = open(dir.path())?;
        let table = engine.open_table("t")?;
        assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3]);
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST))?;
        tm.commit(tx)?;
        assert_eq!(visible_ids(&tm, &*table), Vec::<i32>::new());
    }
    // Final restart: empty, and the recovered engine still accepts writes.
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    assert_eq!(visible_ids(&tm, &*table), Vec::<i32>::new());
    let x = tm.allocate_xid();
    insert(&*table, &tm.context(x, CommandId::FIRST), 42, "z");
    tm.commit(x)?;
    assert_eq!(visible_ids(&tm, &*table), vec![42]);
    Ok(())
}

// --- Corruption & checkpoint interactions ---

/// A replay that reads a page must reject a corrupt one — and *when* that happens
/// moved once replay became bounded.
///
/// Before, startup replayed the whole stream, pinned the page to check its LSN
/// gate, and refused to start. Now a recovery resuming at the last checkpoint's
/// redo point has nothing to replay into that page, so it never pins it and
/// startup succeeds; `StorageManager::read` rejects the page at the first actual
/// read instead. That later rejection is not asserted here because the heap scan
/// path surfaces an I/O error as a panic rather than a `Result` (`heap::io`) —
/// pre-existing, and a separate thing to fix.
#[test]
fn a_corrupt_data_page_is_rejected_by_any_replay_that_reads_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_three(&engine, &tm)?;
        let _ = table;
        // Flush the rows to disk so the page carries a checksum we can break.
        engine.checkpoint(Xid::FIRST_NORMAL)?;
    }
    // The first (and only) user table is relfilenode 1.
    corrupt_page_byte(dir.path(), RelFileNode(1), 0)?;

    // A replay that does reach the records touching the page pins it and fails at
    // startup, exactly as it always has.
    let Err(err) = common::open_from(dir.path(), crabgresql_wal::Lsn::INVALID) else {
        anyhow::bail!("a whole-stream replay must reject a corrupt page");
    };
    assert!(
        err.to_string().contains("checksum"),
        "expected a checksum error at startup, got: {err}"
    );

    // Production resumes at the redo point, replays nothing, and so never touches
    // the page: startup is clean. Pinned deliberately — it is the behaviour change,
    // and a future reader comparing this against the assertion above should see
    // that the difference is which records replay reads, not whether corruption is
    // detected.
    let (engine, _tm) = try_open(dir.path())?;
    assert!(
        engine.open_table("t").is_ok(),
        "a bounded replay does not read the page, so startup must succeed"
    );

    Ok(())
}

#[test]
fn checkpoint_then_more_writes_then_crash_recovers_all() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.create_table(schema())?;
        let x = tm.allocate_xid();
        let ctx = tm.context(x, CommandId::FIRST);
        insert(&*table, &ctx, 1, "a");
        insert(&*table, &ctx, 2, "b");
        tm.commit(x)?;
        // Make rows 1,2 durable on their pages.
        engine.checkpoint(Xid::FIRST_NORMAL)?;
        // Post-checkpoint committed writes live only in the WAL until replay.
        let y = tm.allocate_xid();
        let ctx = tm.context(y, CommandId::FIRST);
        insert(&*table, &ctx, 3, "c");
        insert(&*table, &ctx, 4, "d");
        tm.commit(y)?;
        // Crash.
    }
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3, 4]);
    Ok(())
}

#[test]
fn interleaved_committed_and_in_flight_txns_recover_committed_only() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.create_table(schema())?;
        // Seed a committed base row.
        let base = tm.allocate_xid();
        let base_tid = insert(&*table, &tm.context(base, CommandId::FIRST), 1, "base");
        tm.commit(base)?;
        // Two overlapping transactions: allocate both up front, interleave.
        let xa = tm.allocate_xid();
        let xb = tm.allocate_xid();
        let ca = tm.context(xa, CommandId::FIRST);
        let cb = tm.context(xb, CommandId::FIRST);
        insert(&*table, &ca, 2, "a-ins");
        insert(&*table, &cb, 3, "b-ins");
        // xa updates the base row (delete old + insert new); xb does not touch it.
        table.update(
            base_tid,
            vec![Value::Int4(10), Value::Text("a-upd".into())],
            &ca,
        )?;
        // Commit xa; leave xb in flight, then commit an unrelated row to force
        // xb's records durable, so recovery must reason about them explicitly.
        tm.commit(xa)?;
        let xf = tm.allocate_xid();
        insert(&*table, &tm.context(xf, CommandId::FIRST), 99, "f");
        tm.commit(xf)?;
        // Crash with xb still uncommitted.
    }
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    // xa's insert (2) and update (1->10) survive; xb's insert (3) vanishes; 99 too.
    assert_eq!(visible_ids(&tm, &*table), vec![2, 10, 99]);
    Ok(())
}

#[test]
fn committed_truncate_then_uncommitted_truncate_crash_keeps_committed_rows() -> anyhow::Result<()> {
    // Regression: a committed TRUNCATE followed by an uncommitted one must NOT be
    // judged by the last record's fate. The committed truncate's file is live and
    // must survive; only the uncommitted truncate is discarded.
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_three(&engine, &tm)?;
        // Txn A: truncate away the seed rows and insert 10, 11, then COMMIT.
        let a = tm.allocate_xid();
        let ca = tm.context(a, CommandId::FIRST);
        table.truncate(&ca)?;
        insert(&*table, &ca, 10, "x");
        insert(&*table, &ca, 11, "y");
        tm.commit(a)?;
        assert_eq!(visible_ids(&tm, &*table), vec![10, 11]);
        // Txn B: truncate again but never commit; crash with the swap pending.
        let b = tm.allocate_xid();
        table.truncate(&tm.context(b, CommandId::FIRST))?;
    }
    // Recovery must keep A's committed rows (10, 11), not delete A's live file
    // because B's later truncate was uncommitted.
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    assert_eq!(visible_ids(&tm, &*table), vec![10, 11]);
    Ok(())
}

// --- TRUNCATE across a crash, with a physical index ---
//
// The index relfilenodes swap in the same WAL record and by the same verdict as
// the heap file. Recovery must apply or discard both together: a committed heap
// swap left beside the pre-truncate tree would answer probes with rows the new
// file has since placed at the tids the old entries name.

/// A non-unique B-tree index on `id` for the recovery tests.
fn idx_on_id() -> crabgresql_storage_api::IndexMetadata {
    crabgresql_storage_api::IndexMetadata {
        name: "t_id_idx".into(),
        method: crabgresql_storage_api::IndexMethod::BTree,
        keys: vec![crabgresql_storage_api::IndexKey {
            column: 0,
            descending: false,
            nulls_first: false,
        }],
        unique: false,
        nulls_distinct: true,
        constraint: None,
    }
}

/// Seed 1,2,3 and index them.
fn seed_three_indexed(
    engine: &PgEngine,
    tm: &TransactionManager,
) -> anyhow::Result<Arc<dyn TableAm>> {
    let table = seed_three(engine, tm)?;
    engine.create_index("public", "t", idx_on_id())?;
    Ok(table)
}

/// The visible `id`s an index probe for `key` returns, sorted.
fn probe_ids(tm: &TransactionManager, table: &dyn TableAm, key: i32) -> Vec<i32> {
    let mut v: Vec<i32> = table
        .index_lookup("t_id_idx", &[Value::Int4(key)], &read(tm))
        .expect("index serves the probe")
        .map(|row| match row.expect("index probe failed").1[0] {
            Value::Int4(x) => x,
            _ => unreachable!(),
        })
        .collect();
    v.sort();
    v
}

#[test]
fn indexed_truncate_committed_then_crash_recovers_an_empty_index() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_three_indexed(&engine, &tm)?;
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST))?;
        tm.commit(tx)?;
    }
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    for k in [1, 2, 3] {
        assert!(probe_ids(&tm, &*table, k).is_empty(), "key {k} survived");
    }
    Ok(())
}

#[test]
fn indexed_truncate_uncommitted_then_crash_restores_index_entries() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_three_indexed(&engine, &tm)?;
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST))?;
        // Never commits: crash with the swap pending.
    }
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    for k in [1, 2, 3] {
        assert_eq!(probe_ids(&tm, &*table, k), vec![k]);
    }
    Ok(())
}

#[test]
fn indexed_truncate_then_insert_then_commit_crash_probes_only_new_rows() -> anyhow::Result<()> {
    // The sharpest case: the post-truncate rows occupy the tids the pre-truncate
    // index entries name, so recovery applying only the heap swap would surface
    // them under the old keys.
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_three_indexed(&engine, &tm)?;
        let tx = tm.allocate_xid();
        let ctx = tm.context(tx, CommandId::FIRST);
        table.truncate(&ctx)?;
        insert(&*table, &ctx, 10, "x");
        insert(&*table, &ctx, 11, "y");
        tm.commit(tx)?;
    }
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    assert_eq!(visible_ids(&tm, &*table), vec![10, 11]);
    for k in [1, 2, 3] {
        assert!(probe_ids(&tm, &*table, k).is_empty(), "key {k} survived");
    }
    for k in [10, 11] {
        assert_eq!(probe_ids(&tm, &*table, k), vec![k]);
    }
    Ok(())
}

#[test]
fn indexed_committed_then_uncommitted_truncate_crash_keeps_the_committed_index()
-> anyhow::Result<()> {
    // Per-record verdicts over the index chain, mirroring
    // `committed_truncate_then_uncommitted_truncate_crash_keeps_committed_rows`.
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_three_indexed(&engine, &tm)?;
        let a = tm.allocate_xid();
        let ca = tm.context(a, CommandId::FIRST);
        table.truncate(&ca)?;
        insert(&*table, &ca, 10, "x");
        tm.commit(a)?;
        let b = tm.allocate_xid();
        table.truncate(&tm.context(b, CommandId::FIRST))?;
    }
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    assert_eq!(visible_ids(&tm, &*table), vec![10]);
    assert_eq!(probe_ids(&tm, &*table, 10), vec![10]);
    assert!(probe_ids(&tm, &*table, 1).is_empty());
    Ok(())
}

/// A committed TRUNCATE whose catalog write never happened: recovery must repair
/// the index relfilenodes from the WAL too, not just the heap's. Without the
/// index half the reopened catalog would name the new heap beside the old tree.
#[test]
fn an_indexed_committed_truncate_with_a_stale_catalog_is_repaired_at_recovery() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = common::open_without_finalize(dir.path())?;
        let table = seed_three_indexed(&engine, &tm)?;
        let tx = tm.allocate_xid();
        let ctx = tm.context(tx, CommandId::FIRST);
        table.truncate(&ctx)?;
        insert(&*table, &ctx, 10, "x");
        // Commits in the WAL, but the finalize hook never runs, so the catalog
        // still names the pre-truncate files.
        tm.commit(tx)?;
    }
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    assert_eq!(visible_ids(&tm, &*table), vec![10]);
    assert_eq!(probe_ids(&tm, &*table, 10), vec![10]);
    for k in [1, 2, 3] {
        assert!(probe_ids(&tm, &*table, k).is_empty(), "key {k} survived");
    }
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
    // An engine-managed relation must declare the order it stores rows in; the
    // engine refuses one that does not.
    schema.sort_key = vec![crabgresql_storage_api::IndexKey {
        column: 0,
        descending: false,
        nulls_first: false,
    }];
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

/// The layout sort key is catalog state, so it has to come back on the next
/// boot — nothing recomputes it, and a relation that silently reopened with no
/// order would claim one in `CREATE TABLE` and lose it on restart.
#[test]
fn a_sort_key_survives_a_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let key = vec![
        crabgresql_storage_api::IndexKey {
            column: 1,
            descending: false,
            nulls_first: false,
        },
        crabgresql_storage_api::IndexKey {
            column: 0,
            descending: false,
            nulls_first: false,
        },
    ];
    {
        let (engine, _tm) = open(dir.path())?;
        let mut schema = TableSchema::new(
            "events",
            vec![
                Column::new("id", PgType::Int4),
                Column::new("n", PgType::Int4),
            ],
        );
        schema.access_method = crabgresql_storage_api::TableAccessMethod::Parquet;
        schema.sort_key = key.clone();
        engine.create_table(schema)?;
        // A heap neighbour, so a decoder that zipped the tail onto the wrong
        // relation would be caught rather than reading its own key back.
        engine.create_table(TableSchema::new(
            "plain",
            vec![Column::new("id", PgType::Int4)],
        ))?;
    }

    let (engine, _tm) = open(dir.path())?;
    assert_eq!(engine.open_table("events")?.schema().sort_key, key);
    assert!(engine.open_table("plain")?.schema().sort_key.is_empty());
    Ok(())
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
    assert_eq!(
        engine
            .open_table("events")?
            .scan(&read(&tm), &ColumnProjection::All)
            .count(),
        1
    );
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
fn seed_parquet(engine: &PgEngine, tm: &TransactionManager) -> anyhow::Result<Arc<dyn TableAm>> {
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
        let table = seed_three(&engine, &tm)?;
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

#[test]
fn a_commit_below_the_redo_point_survives_a_bounded_replay() -> anyhow::Result<()> {
    // The reason the durable CLOG exists. Recovery used to rebuild every
    // transaction's fate solely from the XACT_COMMIT/XACT_ABORT records it
    // replayed, so a transaction that committed *below* the redo point came back
    // InProgress and every row it wrote turned invisible — indistinguishable from
    // data loss. With the commit log on disk, replay no longer has to see the
    // commit record to know the fate.
    let dir = tempfile::tempdir()?;
    let redo;
    {
        let (engine, tm, wal) =
            common::open_from_with_wal(dir.path(), crabgresql_wal::Lsn::INVALID)?;
        let table = engine.create_table(schema())?;

        let committed = tm.allocate_xid();
        insert(
            &*table,
            &tm.context(committed, CommandId::FIRST),
            1,
            "below",
        );
        insert(
            &*table,
            &tm.context(committed, CommandId::FIRST),
            2,
            "below",
        );
        tm.commit(committed)?;

        // An aborted neighbour, to prove the CLOG carries the *fate* across the
        // boundary rather than just making everything below it visible.
        let aborted = tm.allocate_xid();
        insert(&*table, &tm.context(aborted, CommandId::FIRST), 3, "gone");
        tm.abort(aborted);

        // Sample the redo point AFTER both transactions finished, then check
        // point: the commit and abort records now sit below redo, and the
        // checkpoint is what makes their CLOG bits durable.
        redo = wal.redo_point()?;
        engine.checkpoint(Xid(aborted.0 + 1))?;
    }

    // Destroy the WAL prefix below redo, so a replay that tried to read those
    // commit records would fail rather than quietly succeed. Only the durable
    // CLOG can answer now.
    assert!(redo.is_valid(), "redo_point() never advanced");
    common::scribble(&common::wal_file_path(dir.path()), 0, redo.0, 0xAB)?;

    let (engine, tm) = common::open_from(dir.path(), redo)?;
    let table = engine.open_table("t")?;
    assert_eq!(
        visible_ids(&tm, &*table),
        vec![1, 2],
        "committed rows below the redo point must survive, and aborted ones must not"
    );
    Ok(())
}

// --- Bounded replay in production ---

/// The whole point of the change: production startup resumes at the redo point the
/// last checkpoint published, so the WAL prefix below it is never read.
///
/// Note what the row assertion alone would *not* prove. The rows are already on
/// disk — the checkpoint flushed them — so they survive even a replay that reads
/// the scribbled prefix, decodes nothing, and reports an empty log. What gives that
/// away is the log itself: `end_of_wal` would come back as `0`, `reset_to` would
/// truncate the whole stream, and the file would be left shorter than the redo point
/// it was supposed to resume from. So both are asserted.
#[test]
fn production_startup_resumes_from_the_recorded_redo_point() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_three(&engine, &tm)?;
        let _ = table;
        // A clean shutdown checkpoints, which is where the redo point comes from.
        TableEngine::shutdown(engine.as_ref());
    }

    let control = crabgresql_wal::read_control(dir.path())?.expect("a control file");
    assert!(
        control.redo_lsn.is_valid(),
        "a heap-only cluster must publish a bounded redo point"
    );
    common::scribble(
        &common::wal_file_path(dir.path()),
        0,
        control.redo_lsn.0,
        0xAB,
    )?;

    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3]);
    let wal_len = std::fs::metadata(common::wal_file_path(dir.path()))?.len();
    assert!(
        wal_len > control.redo_lsn.0,
        "the log was truncated to {wal_len}, below the redo point {} it should have \
         resumed at — recovery read the prefix instead of skipping it",
        control.redo_lsn
    );

    Ok(())
}

/// A crash after a bounded recovery must still recover. This is the case where a
/// redo point published too high would show up: the second startup depends on the
/// first one's checkpoint having flushed everything below its own redo point.
#[test]
fn a_crash_after_a_bounded_recovery_still_recovers_everything() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_three(&engine, &tm)?;
        let _ = table;
        TableEngine::shutdown(engine.as_ref());
    }
    let first = crabgresql_wal::read_control(dir.path())?.expect("a control file");
    common::scribble(
        &common::wal_file_path(dir.path()),
        0,
        first.redo_lsn.0,
        0xAB,
    )?;
    {
        // Bounded startup, then more work, then a crash: dropped without a
        // checkpoint, so only replay can bring the new rows back.
        let (engine, tm) = open(dir.path())?;
        let table = engine.open_table("t")?;
        let x = tm.allocate_xid();
        insert(&*table, &tm.context(x, CommandId::FIRST), 4, "d");
        tm.commit(x)?;
    }

    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    assert_eq!(
        visible_ids(&tm, &*table),
        vec![1, 2, 3, 4],
        "rows from before and after the bounded recovery must all survive"
    );

    Ok(())
}

/// A committed TRUNCATE whose catalog write never landed is repaired from its WAL
/// record — so a checkpoint must not bound replay above that record while the
/// repair is still outstanding. `open_without_finalize` is exactly that window:
/// the commit fsyncs and the CLOG is stamped, but the swap is never applied.
#[test]
fn a_checkpoint_does_not_bound_replay_over_an_unresolved_truncate() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_three(&engine, &tm)?;
        let _ = table;
        TableEngine::shutdown(engine.as_ref());
    }
    {
        let (engine, tm) = common::open_without_finalize(dir.path())?;
        let table = engine.open_table("t")?;
        let x = tm.allocate_xid();
        table.truncate(&tm.context(x, CommandId::FIRST))?;
        tm.commit(x)?;
        engine.checkpoint(tm.snapshot().xmax)?;

        let control = crabgresql_wal::read_control(dir.path())?.expect("a control file");
        assert_eq!(
            control.redo_lsn,
            crabgresql_wal::Lsn::INVALID,
            "a TRUNCATE whose swap is not in the catalog must keep replay unbounded"
        );
    }

    // And the repair still happens, because the record is still replayed.
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    assert_eq!(
        visible_ids(&tm, &*table),
        Vec::<i32>::new(),
        "the committed TRUNCATE must be reapplied from the WAL"
    );

    Ok(())
}

/// An unreadable control file costs a whole-stream replay, never data: recovery
/// treats it as absent and rebuilds every floor from the log.
#[test]
fn an_unreadable_control_file_falls_back_to_a_whole_stream_replay() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_three(&engine, &tm)?;
        let _ = table;
        TableEngine::shutdown(engine.as_ref());
    }
    common::flip_byte(&crabgresql_wal::control_path(dir.path()), 8)?;
    assert_eq!(crabgresql_wal::read_control(dir.path())?, None);

    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    assert_eq!(visible_ids(&tm, &*table), vec![1, 2, 3]);

    Ok(())
}

/// A redo point past the end of the log must be refused loudly. Recovery would
/// otherwise hand back `end_of_wal == start`, which the caller feeds to `reset_to`
/// — truncating away every record above it while reporting a clean start.
#[test]
fn a_redo_point_past_the_end_of_the_log_refuses_to_start() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = seed_three(&engine, &tm)?;
        let _ = table;
        TableEngine::shutdown(engine.as_ref());
    }
    let wal_len = std::fs::metadata(common::wal_file_path(dir.path()))?.len();
    let control = crabgresql_wal::read_control(dir.path())?.expect("a control file");
    crabgresql_wal::write_control(
        dir.path(),
        &crabgresql_wal::ControlFile {
            redo_lsn: crabgresql_wal::Lsn(wal_len + 1),
            ..control
        },
    )?;

    let Err(err) = try_open(dir.path()) else {
        anyhow::bail!("a redo point past the end of the log must not start");
    };
    assert!(
        err.to_string().contains("bytes"),
        "the error should name the log length: {err}"
    );

    Ok(())
}

/// Recovery discards an uncommitted TRUNCATE's staged file, and now sweeps the
/// TOAST chains its tuples named on the way — the same reclamation the ROLLBACK
/// path does, since a crash is the other way that file is thrown away.
///
/// What this pins is the hazard that sweep introduces: it must free the doomed
/// generation's chains and *only* those. The pre-truncate row's wide value lives
/// in the same chunk store (a TRUNCATE never swaps it), so freeing one chunk too
/// many would silently truncate or corrupt a committed value.
#[test]
fn truncate_uncommitted_then_crash_keeps_the_surviving_wide_value() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let big = big_text(7_000);
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.create_table(schema())?;
        let tx = tm.allocate_xid();
        table.insert(
            vec![Value::Int4(1), big.clone()],
            &tm.context(tx, CommandId::FIRST),
        )?;
        tm.commit(tx)?;
    }
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.open_table("t")?;
        let tx = tm.allocate_xid();
        let ctx = tm.context(tx, CommandId::FIRST);
        table.truncate(&ctx)?;
        // A wide row into the staged file, then a crash with the swap pending.
        table.insert(vec![Value::Int4(2), big.clone()], &ctx)?;
    }

    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    assert_eq!(visible_ids(&tm, &*table), vec![1]);
    assert_eq!(
        big_of(&tm, &*table, 1),
        big,
        "the committed value must survive"
    );
    // And the store still works for new wide values afterwards.
    let tx = tm.allocate_xid();
    table.insert(
        vec![Value::Int4(3), big.clone()],
        &tm.context(tx, CommandId::FIRST),
    )?;
    tm.commit(tx)?;
    assert_eq!(big_of(&tm, &*table, 3), big);
    assert_eq!(big_of(&tm, &*table, 1), big);
    Ok(())
}

/// A text value of `n` bytes with position-dependent content, so a chain
/// reassembled out of order or short would not compare equal.
fn big_text(n: usize) -> Value {
    Value::Text(
        (0..n)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect::<String>(),
    )
}

fn big_of(tm: &TransactionManager, table: &dyn TableAm, id: i32) -> Value {
    table
        .scan(&read(tm), &ColumnProjection::All)
        .map(|row| row.unwrap_or_else(|error| panic!("scan failed: {error}")))
        .find(|(_, t)| t[0] == Value::Int4(id))
        .map(|(_, t)| t[1].clone())
        .unwrap_or_else(|| panic!("expected visible tuple with id {id}"))
}

#[test]
fn out_of_line_attributes_survive_a_crash() -> anyhow::Result<()> {
    // The chunks are logged before the tuple that points at them, so the WAL's
    // total order alone guarantees replay never sees a pointer whose target is
    // missing — no extra fsync involved.
    let dir = tempfile::tempdir()?;
    let big = big_text(120_000);
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.create_table(schema())?;
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        table.insert(vec![Value::Int4(1), big.clone()], &txn)?;
        tm.commit(xid)?;

        // An uncommitted big row must leave nothing visible after recovery, even
        // though its chunks did reach the log.
        let doomed = tm.allocate_xid();
        let txn = tm.context(doomed, CommandId::FIRST);
        table.insert(vec![Value::Int4(2), big_text(90_000)], &txn)?;
        // Dropped without commit — the crash.
    }
    let (engine, tm) = open(dir.path())?;
    let table = engine.open_table("t")?;
    assert_eq!(visible_ids(&tm, &*table), vec![1]);
    assert_eq!(
        big_of(&tm, &*table, 1),
        big,
        "the value must survive intact"
    );
    Ok(())
}

#[test]
fn replaying_an_out_of_line_insert_twice_is_idempotent() -> anyhow::Result<()> {
    // Chunk pages are ordinary heap pages, so the page-LSN gate that makes heap
    // redo repeatable covers them unchanged. Forcing a whole-stream replay proves
    // it rather than assuming it.
    let dir = tempfile::tempdir()?;
    let big = big_text(60_000);
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.create_table(schema())?;
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        table.insert(vec![Value::Int4(1), big.clone()], &txn)?;
        tm.commit(xid)?;
    }
    for _ in 0..2 {
        let (engine, tm) = common::open_from(dir.path(), crabgresql_wal::Lsn::INVALID)?;
        let table = engine.open_table("t")?;
        assert_eq!(visible_ids(&tm, &*table), vec![1]);
        assert_eq!(big_of(&tm, &*table, 1), big);
    }
    Ok(())
}

#[test]
fn the_chunk_relation_survives_the_startup_orphan_sweep() -> anyhow::Result<()> {
    // `gc_orphan_relfiles` unlinks every file in `base/` the catalog does not
    // name. If the chunk relfilenode were missing from `live_relfilenodes`, this
    // restart would delete the file and leave every pointer dangling — so this
    // test fails with a read error rather than a wrong value.
    let dir = tempfile::tempdir()?;
    let big = big_text(200_000);
    let toast_file;
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.create_table(schema())?;
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        table.insert(vec![Value::Int4(1), big.clone()], &txn)?;
        tm.commit(xid)?;
        // The heap is relfilenode 1, so the chunk store — allocated next — is 2.
        toast_file = dir.path().join("base").join("2");
        assert!(
            toast_file.exists(),
            "the chunk store should have been created"
        );
    }
    // Two restarts: the first proves the sweep spares it, the second proves the
    // catalog tail that names it round-trips through a rewrite.
    for _ in 0..2 {
        let (engine, tm) = open(dir.path())?;
        assert!(
            toast_file.exists(),
            "the startup orphan sweep must not unlink the chunk store"
        );
        let table = engine.open_table("t")?;
        assert_eq!(big_of(&tm, &*table, 1), big);
    }
    Ok(())
}

#[test]
fn a_corrupt_chunk_page_is_rejected_rather_than_silently_short() -> anyhow::Result<()> {
    // Chunk pages are ordinary heap pages and carry the same checksum, so both
    // paths that read one must reject it — a value assembled from a corrupt page
    // would be silently wrong, which is the failure mode worth ruling out.
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.create_table(schema())?;
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        table.insert(vec![Value::Int4(1), big_text(50_000)], &txn)?;
        tm.commit(xid)?;
        // Flush so the chunk page carries a checksum we can break.
        engine.checkpoint(Xid::FIRST_NORMAL)?;
    }
    // The heap is relfilenode 1, so the chunk store is 2.
    corrupt_page_byte(dir.path(), RelFileNode(2), 0)?;

    // A whole-stream replay reaches the chunk records, pins the page and fails at
    // startup.
    let Err(err) = common::open_from(dir.path(), crabgresql_wal::Lsn::INVALID) else {
        anyhow::bail!("a whole-stream replay must reject a corrupt chunk page");
    };
    assert!(
        err.to_string().contains("checksum"),
        "expected a checksum error at startup, got: {err}"
    );

    // A bounded replay never reads the page, so startup is clean — but the scan
    // that detoasts the row must still refuse rather than hand back a value
    // assembled from a corrupt page.
    let (engine, tm) = try_open(dir.path())?;
    let table = engine.open_table("t")?;
    let scanned = table
        .scan(&read(&tm), &ColumnProjection::All)
        .collect::<Result<Vec<_>, _>>();
    assert!(
        scanned.is_err(),
        "a corrupt chunk page must fail the read, never yield a truncated value"
    );
    Ok(())
}

/// Build a table with one toasted row, checkpoint it, then corrupt its chunk
/// store — the shape every "a bad chunk page must not take the process down"
/// test needs.
fn table_with_a_corrupt_chunk_page(
    dir: &std::path::Path,
) -> anyhow::Result<(Arc<PgEngine>, TransactionManager)> {
    {
        let (engine, tm) = open(dir)?;
        let table = engine.create_table(schema())?;
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        table.insert(vec![Value::Int4(1), big_text(50_000)], &txn)?;
        tm.commit(xid)?;
        // Flush so the chunk page carries a checksum, and so the bounded replay
        // below never reads it.
        engine.checkpoint(Xid::FIRST_NORMAL)?;
    }
    // The heap is relfilenode 1, so the chunk store is 2.
    corrupt_page_byte(dir, RelFileNode(2), 0)?;
    Ok(try_open(dir)?)
}

#[test]
fn vacuum_reports_an_unreadable_chunk_store_instead_of_aborting() -> anyhow::Result<()> {
    // VACUUM used to `panic!` here. Connection tasks have no `catch_unwind`, and
    // this panic fired inside the buffer pool's frame guard, so a single bad page
    // took the process down rather than one statement.
    let dir = tempfile::tempdir()?;
    let (engine, tm) = table_with_a_corrupt_chunk_page(dir.path())?;
    let table = engine.open_table("t")?;
    // Must return, not unwind. Vacuum reports nothing (the trait is infallible),
    // so the assertion is that we get here at all — and that the engine is still
    // usable afterwards.
    table.vacuum(tm.allocate_xid(), &Clog::new());
    let scanned = table
        .scan(&read(&tm), &ColumnProjection::All)
        .collect::<Result<Vec<_>, _>>();
    assert!(
        scanned.is_err(),
        "the corrupt chain is still reported to readers"
    );
    Ok(())
}

#[test]
fn create_index_reports_an_unreadable_chunk_store_instead_of_aborting() -> anyhow::Result<()> {
    // Same panic, on the CREATE INDEX path: an index key over a toasted column
    // has to be the value, so the build detoasts — and an unreadable store must
    // fail the statement rather than the process.
    let dir = tempfile::tempdir()?;
    let (engine, _tm) = table_with_a_corrupt_chunk_page(dir.path())?;
    let error = engine
        .create_index(
            "public",
            "t",
            crabgresql_storage_api::IndexMetadata {
                name: "t_name_idx".to_string(),
                method: crabgresql_storage_api::IndexMethod::BTree,
                keys: vec![crabgresql_storage_api::IndexKey {
                    column: 1,
                    descending: false,
                    nulls_first: false,
                }],
                unique: false,
                nulls_distinct: true,
                constraint: None,
            },
        )
        .expect_err("a chunk store that cannot be read must fail CREATE INDEX");
    // Either shape is right and both are recoverable: an unreadable page is
    // `Io`, a chain that does not add up is `CorruptData`. What matters is that
    // the statement fails rather than the process.
    assert!(
        matches!(
            error,
            crabgresql_storage_api::StorageError::Io(_)
                | crabgresql_storage_api::StorageError::CorruptData(_)
        ),
        "expected a recoverable storage error, got {error:?}"
    );
    Ok(())
}

#[test]
fn an_index_probe_surfaces_an_unreadable_chunk_store() -> anyhow::Result<()> {
    // `index_lookup` used to drop a failed fetch on the floor, so an index scan
    // silently returned fewer rows than a sequential scan of the same table —
    // the same query answering differently depending on the chosen plan.
    let dir = tempfile::tempdir()?;
    {
        let (engine, tm) = open(dir.path())?;
        let table = engine.create_table(schema())?;
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        table.insert(vec![Value::Int4(1), big_text(50_000)], &txn)?;
        tm.commit(xid)?;
        engine.create_index(
            "public",
            "t",
            crabgresql_storage_api::IndexMetadata {
                name: "t_id_idx".to_string(),
                method: crabgresql_storage_api::IndexMethod::BTree,
                keys: vec![crabgresql_storage_api::IndexKey {
                    column: 0,
                    descending: false,
                    nulls_first: false,
                }],
                unique: false,
                nulls_distinct: true,
                constraint: None,
            },
        )?;
        engine.checkpoint(Xid::FIRST_NORMAL)?;
    }
    // The heap is 1 and the index is 2, so the chunk store — created before the
    // index — is 2 and the index is 3. Corrupt the chunk store.
    corrupt_page_byte(dir.path(), RelFileNode(2), 0)?;
    let (engine, tm) = try_open(dir.path())?;
    let table = engine.open_table("t")?;

    let probe: Vec<_> = table
        .index_lookup("t_id_idx", &[Value::Int4(1)], &read(&tm))
        .expect("the index serves the probe")
        .collect();
    assert!(
        probe.iter().any(|row| row.is_err()),
        "the probe must report the unreadable value, not omit the row"
    );
    // And it agrees with the seq scan, which also errors.
    let scanned = table
        .scan(&read(&tm), &ColumnProjection::All)
        .collect::<Result<Vec<_>, _>>();
    assert!(scanned.is_err(), "a seq scan errors on the same table");
    Ok(())
}
