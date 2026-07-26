//! End-to-end tests against the **durable** heap engine: a real driver drives an
//! in-process server backed by `crabgresql-pg-engine` over a temp data
//! directory, and data is shown to survive a full server restart (crash
//! recovery replaying the WAL).

use std::path::Path;

use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_postgres::{NoTls, SimpleQueryMessage};

/// Bring up a server on an ephemeral port backed by the durable engine over
/// `dir` (running recovery on the way up). Returns the port and the serve task.
async fn spawn_pg(dir: &Path) -> (u16, JoinHandle<std::io::Result<()>>) {
    let (engine, txnmgr) = match crabgresql_server::open_pg_engine(dir) {
        Ok(state) => state,
        Err(error) => panic!("failed to open durable test engine: {error}"),
    };
    let listener = match TcpListener::bind(("127.0.0.1", 0)).await {
        Ok(listener) => listener,
        Err(error) => panic!("failed to bind durable test server: {error}"),
    };
    let port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(error) => panic!("failed to read durable test server address: {error}"),
    };
    let handle = tokio::spawn(crabgresql_server::serve_with(listener, engine, txnmgr));
    (port, handle)
}

async fn connect(port: u16) -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(port)
        .user("postgres")
        .dbname("postgres")
        .connect(NoTls)
        .await
        .expect("handshake should succeed");
    tokio::spawn(conn);
    client
}

fn rows(messages: &[SimpleQueryMessage]) -> Vec<&tokio_postgres::SimpleQueryRow> {
    messages
        .iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(row) => Some(row),
            _ => None,
        })
        .collect()
}

/// Shut a server down: disconnect the client, let the connection task drain,
/// then abort and await the serve loop so its engine (and file handles) drop
/// before the directory is reopened.
async fn shutdown(client: tokio_postgres::Client, handle: JoinHandle<std::io::Result<()>>) {
    drop(client);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
async fn crud_over_the_wire() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (port, handle) = spawn_pg(dir.path()).await;
    let client = connect(port).await;

    client
        .simple_query("CREATE TABLE t (id int, name text)")
        .await?;
    client
        .simple_query("INSERT INTO t VALUES (1, 'alice'), (2, 'bob'), (3, 'carol')")
        .await?;
    client
        .simple_query("UPDATE t SET name = 'ALICE' WHERE id = 1")
        .await?;
    client.simple_query("DELETE FROM t WHERE id = 3").await?;

    let msgs = client
        .simple_query("SELECT id, name FROM t ORDER BY id")
        .await?;
    let r = rows(&msgs);
    assert_eq!(r.len(), 2);
    assert_eq!((r[0].get(0), r[0].get(1)), (Some("1"), Some("ALICE")));
    assert_eq!((r[1].get(0), r[1].get(1)), (Some("2"), Some("bob")));

    shutdown(client, handle).await;

    Ok(())
}

/// The durable engine's physical B-tree serves equality index scans end to end:
/// `EXPLAIN` plans an Index Scan, the query returns the right row, and both the
/// index and its correctness survive a restart (rebuilt from replayed WAL).
#[tokio::test]
async fn index_scan_over_the_wire_and_survives_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client
            .simple_query("CREATE TABLE t (id int, label text)")
            .await?;
        // Enough rows to exercise a multi-page tree after CREATE INDEX.
        let values: String = (1..=500).map(|i| format!("({i},'r{i}')")).collect::<Vec<_>>().join(",");
        client
            .simple_query(&format!("INSERT INTO t VALUES {values}"))
            .await?;
        client.simple_query("CREATE INDEX t_id_idx ON t(id)").await?;

        // EXPLAIN now plans an Index Scan on the durable engine (it did a Seq Scan
        // before physical B-trees).
        let msgs = client
            .simple_query("EXPLAIN SELECT * FROM t WHERE id = 250")
            .await?;
        let lines: Vec<String> = rows(&msgs)
            .iter()
            .filter_map(|r| r.get(0).map(str::to_string))
            .collect();
        assert_eq!(lines[0], "Index Scan using t_id_idx on t");
        assert!(lines.iter().any(|l| l.contains("Index Cond: (id = 250)")), "plan: {lines:?}");

        // The index scan returns the correct row.
        let msgs = client
            .simple_query("SELECT label FROM t WHERE id = 250")
            .await?;
        assert_eq!(rows(&msgs)[0].get(0), Some("r250"));
        shutdown(client, handle).await;
    }

    // Restart: the index is rebuilt from the WAL and still serves probes.
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        let msgs = client
            .simple_query("EXPLAIN SELECT * FROM t WHERE id = 250")
            .await?;
        let lines: Vec<String> = rows(&msgs)
            .iter()
            .filter_map(|r| r.get(0).map(str::to_string))
            .collect();
        assert_eq!(lines[0], "Index Scan using t_id_idx on t");
        let msgs = client
            .simple_query("SELECT label FROM t WHERE id = 250")
            .await?;
        assert_eq!(rows(&msgs)[0].get(0), Some("r250"));
        // A never-inserted key is empty.
        let msgs = client
            .simple_query("SELECT label FROM t WHERE id = 9999")
            .await?;
        assert!(rows(&msgs).is_empty());
        shutdown(client, handle).await;
    }
    Ok(())
}

#[tokio::test]
async fn committed_data_survives_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;

    // First boot: create and populate, then shut the server down.
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client
            .simple_query("CREATE TABLE accounts (id int, owner text)")
            .await?;
        client
            .simple_query("INSERT INTO accounts VALUES (1, 'alice'), (2, 'bob')")
            .await?;
        // An explicit transaction block that commits.
        client.simple_query("BEGIN").await?;
        client
            .simple_query("INSERT INTO accounts VALUES (3, 'carol')")
            .await?;
        client.simple_query("COMMIT").await?;
        // And one that rolls back — must NOT survive.
        client.simple_query("BEGIN").await?;
        client
            .simple_query("INSERT INTO accounts VALUES (4, 'mallory')")
            .await?;
        client.simple_query("ROLLBACK").await?;
        shutdown(client, handle).await;
    }

    // Second boot over the same directory: recovery must restore the committed
    // rows (1, 2, 3) and drop the rolled-back one (4).
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        let msgs = client
            .simple_query("SELECT id, owner FROM accounts ORDER BY id")
            .await?;
        let r = rows(&msgs);
        let got: Vec<(Option<&str>, Option<&str>)> =
            r.iter().map(|row| (row.get(0), row.get(1))).collect();
        assert_eq!(
            got,
            vec![
                (Some("1"), Some("alice")),
                (Some("2"), Some("bob")),
                (Some("3"), Some("carol")),
            ]
        );
        shutdown(client, handle).await;
    }

    Ok(())
}

/// A sequence's counter is non-transactional (a ROLLBACK does not rewind it) and
/// durable (its advanced position survives a full server restart).
#[tokio::test]
async fn sequence_counter_is_nontransactional_and_survives_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;

    // Boot 1: create, advance, and prove ROLLBACK does not rewind the counter.
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client.simple_query("CREATE SEQUENCE s").await?;
        let one = rows(&client.simple_query("SELECT nextval('s')").await?)[0]
            .get(0)
            .map(str::to_string);
        assert_eq!(one.as_deref(), Some("1"));
        client.simple_query("BEGIN").await?;
        let two = rows(&client.simple_query("SELECT nextval('s')").await?)[0]
            .get(0)
            .map(str::to_string);
        assert_eq!(two.as_deref(), Some("2"));
        client.simple_query("ROLLBACK").await?;
        // The rolled-back advance is NOT undone: the next value is 3, not 2.
        let three = rows(&client.simple_query("SELECT nextval('s')").await?)[0]
            .get(0)
            .map(str::to_string);
        assert_eq!(three.as_deref(), Some("3"));
        shutdown(client, handle).await;
    }

    // Boot 2 over the same directory: the counter resumes past its persisted
    // value (4), not from the start again.
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        let four = rows(&client.simple_query("SELECT nextval('s')").await?)[0]
            .get(0)
            .map(str::to_string);
        assert_eq!(four.as_deref(), Some("4"));
        shutdown(client, handle).await;
    }

    Ok(())
}

#[tokio::test]
async fn writes_after_a_restart_survive_the_next_restart() -> anyhow::Result<()> {
    // Regression for the WAL-append-after-reopen corruption: the second boot must
    // append+commit to an already-populated WAL, and the third boot must still
    // recover both boots' rows.
    let dir = tempfile::tempdir()?;

    // Boot 1: create + insert.
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client.simple_query("CREATE TABLE t (id int)").await?;
        client.simple_query("INSERT INTO t VALUES (1)").await?;
        shutdown(client, handle).await;
    }
    // Boot 2: this append+commit goes into a non-empty (reopened) WAL.
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client.simple_query("INSERT INTO t VALUES (2)").await?;
        shutdown(client, handle).await;
    }
    // Boot 3: recovery replays a WAL that was appended to after a reopen.
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        let msgs = client.simple_query("SELECT id FROM t ORDER BY id").await?;
        let got: Vec<Option<&str>> = rows(&msgs).iter().map(|r| r.get(0)).collect();
        assert_eq!(got, vec![Some("1"), Some("2")]);
        shutdown(client, handle).await;
    }

    Ok(())
}

#[tokio::test]
async fn truncate_rolled_back_across_a_restart_keeps_rows() {
    // Transactional TRUNCATE end-to-end: a rolled-back (and an abandoned) TRUNCATE
    // must not survive; the rows come back and persist across a restart.
    let dir = tempfile::tempdir().unwrap();
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client.simple_query("CREATE TABLE t (id int)").await.unwrap();
        client
            .simple_query("INSERT INTO t VALUES (1), (2), (3)")
            .await
            .unwrap();
        // Explicit block that truncates then rolls back.
        client.simple_query("BEGIN").await.unwrap();
        client.simple_query("TRUNCATE t").await.unwrap();
        // Inside the block the truncater sees its own empty table.
        let msgs = client.simple_query("SELECT id FROM t").await.unwrap();
        assert_eq!(rows(&msgs).len(), 0);
        client.simple_query("ROLLBACK").await.unwrap();
        // After rollback the rows are back.
        let msgs = client.simple_query("SELECT id FROM t ORDER BY id").await.unwrap();
        assert_eq!(rows(&msgs).len(), 3);
        shutdown(client, handle).await;
    }
    // Restart: the rolled-back TRUNCATE left nothing behind.
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        let msgs = client.simple_query("SELECT id FROM t ORDER BY id").await.unwrap();
        let got: Vec<Option<&str>> = rows(&msgs).iter().map(|r| r.get(0)).collect();
        assert_eq!(got, vec![Some("1"), Some("2"), Some("3")]);
        shutdown(client, handle).await;
    }
}

#[tokio::test]
async fn truncate_committed_across_a_restart_stays_empty() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client.simple_query("CREATE TABLE t (id int)").await.unwrap();
        client.simple_query("INSERT INTO t VALUES (1), (2)").await.unwrap();
        client.simple_query("BEGIN").await.unwrap();
        client.simple_query("TRUNCATE t").await.unwrap();
        client.simple_query("COMMIT").await.unwrap();
        client.simple_query("INSERT INTO t VALUES (9)").await.unwrap();
        shutdown(client, handle).await;
    }
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        let msgs = client.simple_query("SELECT id FROM t ORDER BY id").await.unwrap();
        let got: Vec<Option<&str>> = rows(&msgs).iter().map(|r| r.get(0)).collect();
        assert_eq!(got, vec![Some("9")]);
        shutdown(client, handle).await;
    }
}

#[tokio::test]
async fn dropped_table_stays_dropped_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client.simple_query("CREATE TABLE t (id int)").await.unwrap();
        client.simple_query("INSERT INTO t VALUES (1), (2)").await.unwrap();
        client.simple_query("DROP TABLE t").await.unwrap();
        shutdown(client, handle).await;
    }
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        // The table is gone (42P01 undefined_table)...
        let err = client.simple_query("SELECT id FROM t").await.unwrap_err();
        let db = err.as_db_error().expect("expected a database error");
        assert_eq!(db.code(), &tokio_postgres::error::SqlState::UNDEFINED_TABLE);
        assert!(db.message().contains("does not exist"), "got: {}", db.message());
        // ...and a new table gets a fresh relfilenode with no data-file collision.
        client.simple_query("CREATE TABLE t2 (id int)").await.unwrap();
        client.simple_query("INSERT INTO t2 VALUES (7)").await.unwrap();
        let msgs = client.simple_query("SELECT id FROM t2").await.unwrap();
        let got: Vec<Option<&str>> = rows(&msgs).iter().map(|r| r.get(0)).collect();
        assert_eq!(got, vec![Some("7")]);
        shutdown(client, handle).await;
    }
}

/// A user schema and its schema-qualified table survive a full restart: the
/// relation catalog persists the namespace registry (NSP1) and each relation's
/// namespace, so the schema, its OID, and the table's rows all come back.
#[tokio::test]
async fn schema_and_qualified_table_survive_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;

    let oid = {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client.simple_query("CREATE SCHEMA app").await?;
        client
            .simple_query("CREATE TABLE app.item (id int, label text)")
            .await?;
        client
            .simple_query("INSERT INTO app.item VALUES (1, 'a'), (2, 'b')")
            .await?;
        let oid = rows(&client.simple_query("SELECT oid FROM pg_namespace WHERE nspname = 'app'").await?)
            [0]
        .get(0)
        .map(str::to_string);
        shutdown(client, handle).await;
        oid
    };

    // Restart: the schema (with the same OID), its table, and its rows persist.
    let (port, handle) = spawn_pg(dir.path()).await;
    let client = connect(port).await;
    let ns = client
        .simple_query("SELECT oid FROM pg_namespace WHERE nspname = 'app'")
        .await?;
    assert_eq!(rows(&ns)[0].get(0).map(str::to_string), oid);
    let msgs = client
        .simple_query("SELECT id, label FROM app.item ORDER BY id")
        .await?;
    let got: Vec<(Option<&str>, Option<&str>)> =
        rows(&msgs).iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, vec![(Some("1"), Some("a")), (Some("2"), Some("b"))]);
    // `pg_class.relnamespace` still points at the persisted schema OID.
    let joined = client
        .simple_query(
            "SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relname = 'item'",
        )
        .await?;
    assert_eq!(rows(&joined)[0].get(0), Some("app"));
    shutdown(client, handle).await;

    Ok(())
}

/// A view — a catalog-only object with no heap — survives a full restart: the
/// relation catalog persists its definition, so `SELECT` through it still works.
#[tokio::test]
async fn view_survives_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (port, handle) = spawn_pg(dir.path()).await;
    let client = connect(port).await;

    client
        .simple_query("CREATE TABLE t (id int4, name text)")
        .await?;
    client
        .simple_query("INSERT INTO t VALUES (1, 'a'), (2, 'b')")
        .await?;
    client
        .simple_query("CREATE VIEW v (label) AS SELECT name FROM t WHERE id = 2")
        .await?;
    shutdown(client, handle).await;

    // Restart: the view definition and its column alias come back.
    let (port, handle) = spawn_pg(dir.path()).await;
    let client = connect(port).await;
    let messages = client.simple_query("SELECT label FROM v").await?;
    let result = rows(&messages);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].get("label"), Some("b"));

    // It still reflects as a view after recovery.
    let messages = client
        .simple_query("SELECT relkind FROM pg_class WHERE relname = 'v'")
        .await?;
    assert_eq!(rows(&messages)[0].get("relkind"), Some("v"));
    shutdown(client, handle).await;

    Ok(())
}

/// DROP INDEX removes the index from the durable relation catalog, and the
/// removal persists across a restart (the index does not reappear on recovery).
#[tokio::test]
async fn dropped_index_stays_dropped_across_a_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (port, handle) = spawn_pg(dir.path()).await;
    let client = connect(port).await;

    client
        .simple_query("CREATE TABLE t (id int4, a int4)")
        .await?;
    client.simple_query("CREATE INDEX t_a_idx ON t(a)").await?;
    // The index reflects as relkind='i' before the drop.
    let messages = client
        .simple_query("SELECT count(*) FROM pg_class WHERE relkind = 'i'")
        .await?;
    assert_eq!(rows(&messages)[0].get(0), Some("1"));
    client.simple_query("DROP INDEX t_a_idx").await?;
    let messages = client
        .simple_query("SELECT count(*) FROM pg_class WHERE relkind = 'i'")
        .await?;
    assert_eq!(rows(&messages)[0].get(0), Some("0"));
    shutdown(client, handle).await;

    // Restart: the dropped index does not come back, and its name is free again.
    let (port, handle) = spawn_pg(dir.path()).await;
    let client = connect(port).await;
    let messages = client
        .simple_query("SELECT count(*) FROM pg_class WHERE relkind = 'i'")
        .await?;
    assert_eq!(rows(&messages)[0].get(0), Some("0"));
    client.simple_query("CREATE INDEX t_a_idx ON t(a)").await?;
    shutdown(client, handle).await;

    Ok(())
}

#[tokio::test]
async fn partition_metadata_survives_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;

    // First boot: a RANGE-partitioned parent with one leaf partition, plus a row
    // inserted directly into the partition.
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client
            .simple_query("CREATE TABLE m (id int, d date) PARTITION BY RANGE (d)")
            .await?;
        client
            .simple_query(
                "CREATE TABLE m_2024 PARTITION OF m FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')",
            )
            .await?;
        client
            .simple_query("INSERT INTO m_2024 VALUES (1, '2024-06-01')")
            .await?;
        shutdown(client, handle).await;
    }

    // Second boot: the parent is still relkind='p', the partition still
    // relispartition='t', the pg_inherits link and the partition's row survive.
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;

        let msgs = client
            .simple_query(
                "SELECT relname, relkind, relispartition FROM pg_class \
                 WHERE relname IN ('m', 'm_2024') ORDER BY relname",
            )
            .await?;
        let r = rows(&msgs);
        assert_eq!(
            (r[0].get(0), r[0].get(1), r[0].get(2)),
            (Some("m"), Some("p"), Some("f"))
        );
        assert_eq!(
            (r[1].get(0), r[1].get(1), r[1].get(2)),
            (Some("m_2024"), Some("r"), Some("t"))
        );

        let msgs = client
            .simple_query(
                "SELECT c.relname, p.relname FROM pg_inherits i \
                 JOIN pg_class c ON c.oid = i.inhrelid \
                 JOIN pg_class p ON p.oid = i.inhparent",
            )
            .await?;
        let r = rows(&msgs);
        assert_eq!((r[0].get(0), r[0].get(1)), (Some("m_2024"), Some("m")));

        let msgs = client
            .simple_query("SELECT partstrat, partnatts, partattrs FROM pg_partitioned_table")
            .await?;
        let r = rows(&msgs);
        assert_eq!(
            (r[0].get(0), r[0].get(1), r[0].get(2)),
            (Some("r"), Some("1"), Some("2"))
        );

        let msgs = client.simple_query("SELECT id FROM m_2024").await?;
        assert_eq!(rows(&msgs)[0].get(0), Some("1"));

        // The reloaded (typed) bound still enforces: an out-of-range direct
        // INSERT into the leaf is rejected (23514), same as before the restart.
        let err = client
            .simple_query("INSERT INTO m_2024 VALUES (2, '2023-03-01')")
            .await
            .unwrap_err();
        assert_eq!(
            err.as_db_error().expect("database error").code(),
            &tokio_postgres::error::SqlState::CHECK_VIOLATION
        );

        shutdown(client, handle).await;
    }

    Ok(())
}

/// `ANALYZE` is non-transactional and durable: its result survives both a
/// `ROLLBACK` and a full server restart, matching PostgreSQL.
#[tokio::test]
async fn analyze_statistics_are_nontransactional_and_survive_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let reltuples = |messages: &[SimpleQueryMessage]| -> Option<String> {
        rows(messages).first()?.get(0).map(str::to_string)
    };
    const SIZE: &str =
        "SELECT reltuples::int FROM pg_class WHERE relname = 'meas'";

    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client.simple_query("CREATE TABLE meas (id int)").await?;
        client
            .simple_query("INSERT INTO meas VALUES (1), (2), (3)")
            .await?;
        // Until ANALYZE runs, pg_class reports the never-analyzed sentinel
        // rather than the real size — as PostgreSQL does.
        assert_eq!(
            reltuples(&client.simple_query(SIZE).await?).as_deref(),
            Some("-1")
        );

        client.simple_query("ANALYZE meas").await?;
        assert_eq!(
            reltuples(&client.simple_query(SIZE).await?).as_deref(),
            Some("3")
        );

        // An ANALYZE inside a transaction that then rolls back still stands:
        // statistics are not transactional.
        client.simple_query("BEGIN").await?;
        client.simple_query("INSERT INTO meas VALUES (4)").await?;
        client.simple_query("ANALYZE meas").await?;
        client.simple_query("ROLLBACK").await?;
        assert_eq!(
            reltuples(&client.simple_query(SIZE).await?).as_deref(),
            Some("4"),
            "a rolled-back ANALYZE result must not be rewound"
        );
        shutdown(client, handle).await;
    }

    // Boot 2 over the same directory: the measurement is still there, and the
    // row the rollback removed is still gone.
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        assert_eq!(
            reltuples(&client.simple_query(SIZE).await?).as_deref(),
            Some("4")
        );
        // Re-measuring after the restart sees the truth again.
        client.simple_query("ANALYZE meas").await?;
        assert_eq!(
            reltuples(&client.simple_query(SIZE).await?).as_deref(),
            Some("3")
        );
        shutdown(client, handle).await;
    }

    Ok(())
}
