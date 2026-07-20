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
