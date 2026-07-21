//! End-to-end tests for the `parquet` table access method: a real driver
//! (tokio-postgres) against an in-process server whose global engine composes
//! the in-memory engine with a Parquet engine rooted in a temp dir.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;

use crabgresql_memory_storage::MemoryEngine;
use crabgresql_storage_api::TableEngine;
use tokio::net::TcpListener;
use tokio_postgres::error::SqlState;
use tokio_postgres::{NoTls, SimpleQueryMessage};

/// Spawn a server whose global engine routes `USING parquet` tables to a Parquet
/// engine rooted at `parquet_dir`.
async fn spawn_server(parquet_dir: &Path) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base: Arc<dyn TableEngine> = Arc::new(MemoryEngine::new());
    let engine = crabgresql_server::with_parquet_engine(base, parquet_dir).unwrap();
    tokio::spawn(crabgresql_server::serve(listener, engine));
    port
}

async fn connect(port: u16) -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(port)
        .user("postgres")
        .dbname("postgres")
        .connect(NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    client
}

/// Collect the `(col -> value)` rows a SELECT returns, as strings.
fn rows(messages: &[SimpleQueryMessage], cols: &[&str]) -> Vec<Vec<Option<String>>> {
    messages
        .iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(row) => Some(
                cols.iter()
                    .enumerate()
                    .map(|(i, _)| row.get(i).map(str::to_string))
                    .collect(),
            ),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn create_insert_select_and_reject_mutations() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let client = connect(spawn_server(dir.path()).await).await;

    // CREATE ... USING parquet succeeds and the backing file appears on disk.
    client
        .simple_query("CREATE TABLE p (id int4, name text) USING parquet")
        .await?;
    let file = dir.path().join("public__p.parquet");
    assert!(file.exists(), "parquet file should exist at {file:?}");

    // INSERT then SELECT round-trips through the in-memory rows.
    client
        .simple_query("INSERT INTO p VALUES (1, 'a'), (2, 'b')")
        .await?;
    let got = client.simple_query("SELECT id, name FROM p ORDER BY id").await?;
    assert_eq!(
        rows(&got, &["id", "name"]),
        vec![
            vec![Some("1".to_string()), Some("a".to_string())],
            vec![Some("2".to_string()), Some("b".to_string())],
        ]
    );

    // UPDATE / DELETE / TRUNCATE are rejected as feature-not-supported (0A000).
    for sql in [
        "UPDATE p SET name = 'x'",
        "DELETE FROM p",
        "TRUNCATE p",
    ] {
        let err = client.simple_query(sql).await.unwrap_err();
        let db = err.as_db_error().expect("db error");
        assert_eq!(
            db.code(),
            &SqlState::FEATURE_NOT_SUPPORTED,
            "expected 0A000 for `{sql}`, got {}",
            db.code().code()
        );
    }
    Ok(())
}

#[tokio::test]
async fn unknown_access_method_errors_42704() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let client = connect(spawn_server(dir.path()).await).await;

    let err = client
        .simple_query("CREATE TABLE q (id int4) USING bogus")
        .await
        .unwrap_err();
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::UNDEFINED_OBJECT);
    assert_eq!(db.message(), "access method \"bogus\" does not exist");
    Ok(())
}

#[tokio::test]
async fn using_heap_behaves_like_a_normal_table() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let client = connect(spawn_server(dir.path()).await).await;

    // Explicit `USING heap` is the default engine; UPDATE works, no parquet file.
    client
        .simple_query("CREATE TABLE h (id int4) USING heap")
        .await?;
    client.simple_query("INSERT INTO h VALUES (1)").await?;
    client.simple_query("UPDATE h SET id = 2").await?;
    let got = client.simple_query("SELECT id FROM h").await?;
    assert_eq!(rows(&got, &["id"]), vec![vec![Some("2".to_string())]]);
    assert!(!dir.path().join("public__h.parquet").exists());
    Ok(())
}

#[tokio::test]
async fn tables_recover_from_parquet_files_on_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;

    // First server: create and populate a parquet table, then stop using it.
    {
        let client = connect(spawn_server(dir.path()).await).await;
        client
            .simple_query("CREATE TABLE r (id int4, name text) USING parquet")
            .await?;
        client
            .simple_query("INSERT INTO r VALUES (1, 'a'), (2, 'b')")
            .await?;
    }

    // Second server over the same dir recovers the table from its file alone.
    let client = connect(spawn_server(dir.path()).await).await;
    let got = client.simple_query("SELECT id, name FROM r ORDER BY id").await?;
    assert_eq!(
        rows(&got, &["id", "name"]),
        vec![
            vec![Some("1".to_string()), Some("a".to_string())],
            vec![Some("2".to_string()), Some("b".to_string())],
        ]
    );
    Ok(())
}
