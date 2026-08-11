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
    // Nothing here loads a server-side COPY file; the data dir is enough.
    let copy_files = crabgresql_server::CopyFileAccess::confined_to(dir);
    let handle = tokio::spawn(crabgresql_server::serve_with(
        listener, engine, txnmgr, copy_files,
    ));
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

/// A committed `INSERT` into a Parquet relation is durable through the WAL alone:
/// the row reads back immediately and survives a restart, while no Parquet file
/// exists for it yet. That is the point of the write buffer — a stream of small
/// inserts must not become a directory of tiny fragments.
#[tokio::test]
async fn a_committed_parquet_insert_is_durable_before_any_file_exists() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let parquet_files = || -> Vec<String> {
        let mut names = Vec::new();
        let root = dir.path().join("parquet");
        let Ok(entries) = std::fs::read_dir(&root) else {
            return names;
        };
        for entry in entries.flatten() {
            let Ok(inner) = std::fs::read_dir(entry.path()) else {
                continue;
            };
            for file in inner.flatten() {
                names.push(file.file_name().to_string_lossy().into_owned());
            }
        }
        names
    };

    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client
            .simple_query("CREATE TABLE events (id int4 PRIMARY KEY, label text) USING parquet")
            .await?;
        for id in 1..=5 {
            client
                .simple_query(&format!("INSERT INTO events VALUES ({id}, 'row {id}')"))
                .await?;
        }
        let messages = client.simple_query("SELECT id FROM events").await?;
        assert_eq!(rows(&messages).len(), 5);
        assert!(
            parquet_files().is_empty(),
            "five separate inserts must not have produced five fragments: {:?}",
            parquet_files()
        );

        // VACUUM is the explicit flush: five buffered rows become ONE chunk, and
        // the rows read back identically across the move.
        client.simple_query("VACUUM events").await?;
        assert_eq!(
            parquet_files().len(),
            1,
            "a flush must consolidate the batch into one fragment: {:?}",
            parquet_files()
        );
        let messages = client
            .simple_query("SELECT id FROM events ORDER BY id")
            .await?;
        let result = rows(&messages);
        assert_eq!(
            result.iter().map(|r| r.get(0)).collect::<Vec<_>>(),
            vec![Some("1"), Some("2"), Some("3"), Some("4"), Some("5")],
            "a flush must not add, drop, or duplicate a row"
        );

        // Flushing again has nothing left to move.
        client.simple_query("VACUUM events").await?;
        assert_eq!(
            parquet_files().len(),
            1,
            "an empty flush must write no file"
        );
        shutdown(client, handle).await;
    }

    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        let messages = client
            .simple_query("SELECT id FROM events ORDER BY id")
            .await?;
        let result = rows(&messages);
        assert_eq!(
            result.iter().map(|r| r.get(0)).collect::<Vec<_>>(),
            vec![Some("1"), Some("2"), Some("3"), Some("4"), Some("5")],
            "every acknowledged row must come back from the WAL"
        );
        shutdown(client, handle).await;
    }
    Ok(())
}

/// A buffer table holds its rows only in RAM, so the WAL is its *entire*
/// durability story: a committed row must come back after a restart even though
/// no file ever held it, a rolled-back one must not, and recovery must resume
/// row ids above every id the log mentions.
#[tokio::test]
async fn buffer_table_rows_survive_a_restart_through_the_wal_alone() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;

    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client
            .simple_query("CREATE TABLE staging (id int4, label text) USING buffer ORDER BY (id)")
            .await?;
        client
            .simple_query("INSERT INTO staging VALUES (1, 'committed'), (2, 'also committed')")
            .await?;
        // A committed delete must stay deleted across the restart.
        client
            .simple_query("DELETE FROM staging WHERE id = 2")
            .await?;
        client.simple_query("BEGIN").await?;
        client
            .simple_query("INSERT INTO staging VALUES (3, 'rolled back')")
            .await?;
        client.simple_query("ROLLBACK").await?;
        shutdown(client, handle).await;
    }

    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        let messages = client
            .simple_query("SELECT id, label FROM staging ORDER BY id")
            .await?;
        let result = rows(&messages);
        assert_eq!(
            result.len(),
            1,
            "only the committed, undeleted row may return"
        );
        assert_eq!(
            (result[0].get(0), result[0].get(1)),
            (Some("1"), Some("committed"))
        );
        // Writing after recovery must not collide with a recovered row id.
        client
            .simple_query("INSERT INTO staging VALUES (4, 'after restart')")
            .await?;
        let messages = client
            .simple_query("SELECT id FROM staging ORDER BY id")
            .await?;
        assert_eq!(rows(&messages).len(), 2);
        shutdown(client, handle).await;
    }

    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        let messages = client
            .simple_query("SELECT id FROM staging ORDER BY id")
            .await?;
        let result = rows(&messages);
        assert_eq!(
            result.iter().map(|r| r.get(0)).collect::<Vec<_>>(),
            vec![Some("1"), Some("4")],
            "a second restart must replay to the same state, not accumulate"
        );
        shutdown(client, handle).await;
    }
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
        let values: String = (1..=500)
            .map(|i| format!("({i},'r{i}')"))
            .collect::<Vec<_>>()
            .join(",");
        client
            .simple_query(&format!("INSERT INTO t VALUES {values}"))
            .await?;
        client
            .simple_query("CREATE INDEX t_id_idx ON t(id)")
            .await?;
        // Without statistics the planner has to assume `id = 250` keeps PG's
        // default 0.5% of the rows, and on a table this small a probe for two or
        // three rows costs more in random pages than reading all four — so it
        // would choose a Seq Scan, exactly as PostgreSQL does for an unanalyzed
        // table. ANALYZE is what tells it the column is unique.
        client.simple_query("ANALYZE t").await?;

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
        assert!(
            lines.iter().any(|l| l.contains("Index Cond: (id = 250)")),
            "plan: {lines:?}"
        );

        // The index scan returns the correct row.
        let msgs = client
            .simple_query("SELECT label FROM t WHERE id = 250")
            .await?;
        assert_eq!(rows(&msgs)[0].get(0), Some("r250"));
        shutdown(client, handle).await;
    }

    // Restart: the index is rebuilt from the WAL and still serves probes. The
    // plan is also unchanged, which takes both halves — the index *and* the
    // statistics that justify choosing it — surviving the restart.
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

#[tokio::test]
async fn parquet_commit_rollback_and_restart_are_durable() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;

    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client
            .simple_query("CREATE TABLE events (id int4 PRIMARY KEY, label text) USING parquet")
            .await?;
        client
            .simple_query("INSERT INTO events VALUES (1, 'committed')")
            .await?;
        client.simple_query("BEGIN").await?;
        client
            .simple_query("INSERT INTO events VALUES (2, 'rolled back')")
            .await?;
        client.simple_query("ROLLBACK").await?;
        shutdown(client, handle).await;
    }

    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        let messages = client
            .simple_query("SELECT id, label FROM events ORDER BY id")
            .await?;
        let result = rows(&messages);
        assert_eq!(result.len(), 1);
        assert_eq!(
            (result[0].get(0), result[0].get(1)),
            (Some("1"), Some("committed"))
        );
        client
            .simple_query("INSERT INTO events VALUES (3, 'after restart')")
            .await?;
        shutdown(client, handle).await;
    }

    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        let messages = client
            .simple_query("SELECT id FROM events ORDER BY id")
            .await?;
        let ids: Vec<Option<&str>> = rows(&messages).iter().map(|row| row.get(0)).collect();
        assert_eq!(ids, vec![Some("1"), Some("3")]);
        client.simple_query("DROP TABLE events").await?;
        shutdown(client, handle).await;
    }
    assert!(
        !dir.path().join("parquet").join("1").exists(),
        "DROP TABLE must remove its managed Parquet directory"
    );
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
async fn truncate_rolled_back_across_a_restart_keeps_rows() -> anyhow::Result<()> {
    // Transactional TRUNCATE end-to-end: a rolled-back (and an abandoned) TRUNCATE
    // must not survive; the rows come back and persist across a restart.
    let dir = tempfile::tempdir()?;
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client.simple_query("CREATE TABLE t (id int)").await?;
        client
            .simple_query("INSERT INTO t VALUES (1), (2), (3)")
            .await?;
        // Explicit block that truncates then rolls back.
        client.simple_query("BEGIN").await?;
        client.simple_query("TRUNCATE t").await?;
        // Inside the block the truncater sees its own empty table.
        let msgs = client.simple_query("SELECT id FROM t").await?;
        assert_eq!(rows(&msgs).len(), 0);
        client.simple_query("ROLLBACK").await?;
        // After rollback the rows are back.
        let msgs = client.simple_query("SELECT id FROM t ORDER BY id").await?;
        assert_eq!(rows(&msgs).len(), 3);
        shutdown(client, handle).await;
    }
    // Restart: the rolled-back TRUNCATE left nothing behind.
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        let msgs = client.simple_query("SELECT id FROM t ORDER BY id").await?;
        let got: Vec<Option<&str>> = rows(&msgs).iter().map(|r| r.get(0)).collect();
        assert_eq!(got, vec![Some("1"), Some("2"), Some("3")]);
        shutdown(client, handle).await;
    }
    Ok(())
}

#[tokio::test]
async fn truncate_committed_across_a_restart_stays_empty() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client.simple_query("CREATE TABLE t (id int)").await?;
        client.simple_query("INSERT INTO t VALUES (1), (2)").await?;
        client.simple_query("BEGIN").await?;
        client.simple_query("TRUNCATE t").await?;
        client.simple_query("COMMIT").await?;
        client.simple_query("INSERT INTO t VALUES (9)").await?;
        shutdown(client, handle).await;
    }
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        let msgs = client.simple_query("SELECT id FROM t ORDER BY id").await?;
        let got: Vec<Option<&str>> = rows(&msgs).iter().map(|r| r.get(0)).collect();
        assert_eq!(got, vec![Some("9")]);
        shutdown(client, handle).await;
    }
    Ok(())
}

/// An indexed table's TRUNCATE survives a restart on both halves: the catalog
/// must come back naming the post-truncate B-tree, or the reopened index would
/// answer a truncated-away key with a row the new heap file has since placed at
/// the tid the stale entry names.
#[tokio::test]
async fn truncate_of_an_indexed_table_is_durable_across_a_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client.simple_query("CREATE TABLE t (id int)").await?;
        client.simple_query("CREATE INDEX t_idx ON t (id)").await?;
        client
            .simple_query("INSERT INTO t VALUES (1), (2), (3)")
            .await?;
        client.simple_query("TRUNCATE t").await?;
        client.simple_query("INSERT INTO t VALUES (7)").await?;
        shutdown(client, handle).await;
    }
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        let msgs = client.simple_query("SELECT id FROM t WHERE id = 1").await?;
        assert!(rows(&msgs).is_empty(), "a truncated-away key came back");
        let msgs = client.simple_query("SELECT id FROM t WHERE id = 7").await?;
        let got: Vec<Option<&str>> = rows(&msgs).iter().map(|r| r.get(0)).collect();
        assert_eq!(got, vec![Some("7")]);
        shutdown(client, handle).await;
    }
    Ok(())
}

/// TRUNCATE on a Parquet table end-to-end across restarts: the committed swap
/// stays committed, a rolled-back one leaves the rows in place, and exactly one
/// fragment directory survives each restart (the superseded and staged ones are
/// reclaimed).
#[tokio::test]
async fn parquet_truncate_is_durable_across_a_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let parquet_dirs = || -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir.path().join("parquet"))
            .expect("parquet root exists")
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .collect();
        names.sort();
        names
    };

    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client
            .simple_query("CREATE TABLE events (id int4) USING parquet ORDER BY (id)")
            .await?;
        client
            .simple_query("INSERT INTO events VALUES (1), (2)")
            .await?;
        // Rolled back: nothing changes.
        client.simple_query("BEGIN").await?;
        client.simple_query("TRUNCATE events").await?;
        client.simple_query("ROLLBACK").await?;
        // Committed, then reloaded in the same boot.
        client.simple_query("TRUNCATE events").await?;
        client.simple_query("INSERT INTO events VALUES (9)").await?;
        shutdown(client, handle).await;
    }
    assert_eq!(
        parquet_dirs().len(),
        1,
        "the superseded and rolled-back directories are gone: {:?}",
        parquet_dirs()
    );

    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        let msgs = client
            .simple_query("SELECT id FROM events ORDER BY id")
            .await?;
        let got: Vec<Option<&str>> = rows(&msgs).iter().map(|r| r.get(0)).collect();
        assert_eq!(got, vec![Some("9")]);
        // A TRUNCATE that never commits before the shutdown leaves the rows alone.
        client.simple_query("BEGIN").await?;
        client.simple_query("TRUNCATE events").await?;
        shutdown(client, handle).await;
    }

    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        let msgs = client
            .simple_query("SELECT id FROM events ORDER BY id")
            .await?;
        let got: Vec<Option<&str>> = rows(&msgs).iter().map(|r| r.get(0)).collect();
        assert_eq!(got, vec![Some("9")]);
        assert_eq!(parquet_dirs().len(), 1, "{:?}", parquet_dirs());
        client.simple_query("DROP TABLE events").await?;
        shutdown(client, handle).await;
    }
    assert!(
        parquet_dirs().is_empty(),
        "DROP TABLE must remove every directory the relation owned: {:?}",
        parquet_dirs()
    );
    Ok(())
}

#[tokio::test]
async fn dropped_table_stays_dropped_across_a_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client.simple_query("CREATE TABLE t (id int)").await?;
        client.simple_query("INSERT INTO t VALUES (1), (2)").await?;
        client.simple_query("DROP TABLE t").await?;
        shutdown(client, handle).await;
    }
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        // The table is gone (42P01 undefined_table)...
        let err = client
            .simple_query("SELECT id FROM t")
            .await
            .expect_err("the dropped table must not resolve");
        let db = err.as_db_error().expect("expected a database error");
        assert_eq!(db.code(), &tokio_postgres::error::SqlState::UNDEFINED_TABLE);
        assert!(
            db.message().contains("does not exist"),
            "got: {}",
            db.message()
        );
        // ...and a new table gets a fresh relfilenode with no data-file collision.
        client.simple_query("CREATE TABLE t2 (id int)").await?;
        client.simple_query("INSERT INTO t2 VALUES (7)").await?;
        let msgs = client.simple_query("SELECT id FROM t2").await?;
        let got: Vec<Option<&str>> = rows(&msgs).iter().map(|r| r.get(0)).collect();
        assert_eq!(got, vec![Some("7")]);
        shutdown(client, handle).await;
    }
    Ok(())
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
        let oid = rows(
            &client
                .simple_query("SELECT oid FROM pg_namespace WHERE nspname = 'app'")
                .await?,
        )[0]
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

/// A primary key added by `ALTER TABLE` survives a restart — both halves of it.
///
/// The index half rides the relation catalog like any other. The NOT NULL half
/// is the one worth pinning: it is a bit inside the persisted column record, and
/// nothing about the in-memory flip that the same statement performs would show
/// up here. If the catalog write were ever dropped, the reflection below would
/// still look right on the *original* process and only this test would notice.
#[tokio::test]
async fn alter_table_primary_key_survives_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;

    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client.simple_query("CREATE TABLE r (a int, b int)").await?;
        client.simple_query("INSERT INTO r VALUES (1, 1)").await?;
        client
            .simple_query("ALTER TABLE r ADD PRIMARY KEY (a)")
            .await?;
        shutdown(client, handle).await;
    }

    let (port, handle) = spawn_pg(dir.path()).await;
    let client = connect(port).await;
    let msgs = client
        .simple_query(
            "SELECT attname, attnotnull FROM pg_attribute \
             WHERE attrelid = 'r'::regclass AND attnum > 0 ORDER BY attnum",
        )
        .await?;
    let notnull: Vec<(Option<&str>, Option<&str>)> =
        rows(&msgs).iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(
        notnull,
        vec![(Some("a"), Some("t")), (Some("b"), Some("f"))]
    );
    let msgs = client
        .simple_query("SELECT conname, contype FROM pg_constraint WHERE conrelid = 'r'::regclass")
        .await?;
    assert_eq!(rows(&msgs)[0].get(0), Some("r_pkey"));
    assert_eq!(rows(&msgs)[0].get(1), Some("p"));

    // Reflection is a claim; these are the enforcement it is claiming.
    let e = client
        .simple_query("INSERT INTO r VALUES (NULL, 2)")
        .await
        .expect_err("a NULL in a NOT NULL column must be rejected");
    assert_eq!(
        e.as_db_error().map(|e| e.code().code().to_string()),
        Some("23502".to_string())
    );
    let e = client
        .simple_query("INSERT INTO r VALUES (1, 3)")
        .await
        .expect_err("a duplicate primary key must be rejected");
    assert_eq!(
        e.as_db_error().map(|e| e.code().code().to_string()),
        Some("23505".to_string())
    );
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
            .expect_err("an out-of-range partition key must be rejected");
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
    const SIZE: &str = "SELECT reltuples::int FROM pg_class WHERE relname = 'meas'";

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

#[tokio::test]
async fn out_of_line_values_survive_a_restart() -> anyhow::Result<()> {
    // The engine-level recovery suite proves replay of the chunk records; this
    // proves the whole stack agrees afterwards — including that the chunk
    // relfilenode comes back through the durable catalog rather than being
    // unlinked by the startup orphan sweep.
    let dir = tempfile::tempdir()?;
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client
            .simple_query("CREATE TABLE docs (id int, body text)")
            .await?;
        client
            .simple_query("INSERT INTO docs VALUES (1, repeat('abcdefghij', 20000))")
            .await?;
        client
            .simple_query("INSERT INTO docs VALUES (2, 'short')")
            .await?;
        shutdown(client, handle).await;
    }
    // Second boot: the value is intact — an md5 alongside the length, so a
    // chain reassembled out of order and not just short would show.
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        let msgs = client
            .simple_query(
                "SELECT length(body), md5(body) = md5(repeat('abcdefghij', 20000)) \
                 FROM docs WHERE id = 1",
            )
            .await?;
        assert_eq!(rows(&msgs)[0].get(0), Some("200000"));
        assert_eq!(rows(&msgs)[0].get(1), Some("t"));
        // reltoastrelid survived the restart and still resolves to a real row.
        let msgs = client
            .simple_query(
                "SELECT t.relkind FROM pg_class c JOIN pg_class t ON t.oid = c.reltoastrelid \
                 WHERE c.relname = 'docs'",
            )
            .await?;
        assert_eq!(rows(&msgs)[0].get(0), Some("t"));
        // And the table still takes new wide values on the recovered chunk store.
        client
            .simple_query("INSERT INTO docs VALUES (3, repeat('z', 90000))")
            .await?;
        let msgs = client
            .simple_query("SELECT length(body) FROM docs WHERE id = 3")
            .await?;
        assert_eq!(rows(&msgs)[0].get(0), Some("90000"));
        shutdown(client, handle).await;
    }
    Ok(())
}

#[tokio::test]
async fn inheritance_hierarchy_survives_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;

    // First boot: a three-level hierarchy whose bottom relation has two parents,
    // so both the *set* of links and their *order* have something to lose.
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;
        client
            .simple_query("CREATE TABLE person (name text, age int4)")
            .await?;
        client
            .simple_query("CREATE TABLE emp (salary int4) INHERITS (person)")
            .await?;
        client
            .simple_query("CREATE TABLE student (gpa float8) INHERITS (person)")
            .await?;
        client
            .simple_query("CREATE TABLE stud_emp (percent int4) INHERITS (emp, student)")
            .await?;
        client
            .simple_query(
                "INSERT INTO person VALUES ('pp', 10); \
                 INSERT INTO stud_emp VALUES ('se', 40, 200, 4.0, 50)",
            )
            .await?;
        shutdown(client, handle).await;
    }

    // Second boot: the merged layout, the parent links with their `inhseqno`, and
    // the read fan-out all come back.
    {
        let (port, handle) = spawn_pg(dir.path()).await;
        let client = connect(port).await;

        let msgs = client
            .simple_query(
                "SELECT a.attname FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid \
                 WHERE c.relname = 'stud_emp' AND a.attnum > 0 ORDER BY a.attnum",
            )
            .await?;
        let layout: Vec<_> = rows(&msgs).iter().map(|r| r.get(0)).collect();
        assert_eq!(
            layout,
            vec![
                Some("name"),
                Some("age"),
                Some("salary"),
                Some("gpa"),
                Some("percent")
            ]
        );

        let msgs = client
            .simple_query(
                "SELECT c.relname, p.relname, i.inhseqno FROM pg_inherits i \
                 JOIN pg_class c ON c.oid = i.inhrelid \
                 JOIN pg_class p ON p.oid = i.inhparent \
                 ORDER BY c.relname, i.inhseqno",
            )
            .await?;
        let links: Vec<_> = rows(&msgs)
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2)))
            .collect();
        assert_eq!(
            links,
            vec![
                (Some("emp"), Some("person"), Some("1")),
                (Some("stud_emp"), Some("emp"), Some("1")),
                (Some("stud_emp"), Some("student"), Some("2")),
                (Some("student"), Some("person"), Some("1")),
            ]
        );

        // The fan-out is rebuilt from those links, including the remap that reads
        // `stud_emp`'s `gpa` (ordinal 4) as `student`'s (ordinal 3).
        let msgs = client
            .simple_query("SELECT name FROM person ORDER BY name")
            .await?;
        let names: Vec<_> = rows(&msgs).iter().map(|r| r.get(0)).collect();
        assert_eq!(names, vec![Some("pp"), Some("se")]);
        let msgs = client.simple_query("SELECT name, gpa FROM student").await?;
        assert_eq!(
            (rows(&msgs)[0].get(0), rows(&msgs)[0].get(1)),
            (Some("se"), Some("4"))
        );

        shutdown(client, handle).await;
    }

    Ok(())
}
