//! End-to-end tests: a real driver (tokio-postgres) against an in-process
//! server on an ephemeral port, plus raw-socket checks of the startup phase.

use std::sync::Arc;

use crabgresql_memory_storage::MemoryEngine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_postgres::{NoTls, SimpleQueryMessage};

async fn spawn_server() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(crabgresql_server::serve(
        listener,
        Arc::new(MemoryEngine::new()),
    ));
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

#[tokio::test]
async fn select_one() {
    let client = connect(spawn_server().await).await;
    let messages = client.simple_query("SELECT 1").await.unwrap();
    let rows = rows(&messages);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].columns()[0].name(), "?column?");
    assert_eq!(rows[0].get(0), Some("1"));
}

#[tokio::test]
async fn select_literals_with_aliases() {
    let client = connect(spawn_server().await).await;
    let messages = client
        .simple_query("SELECT 1 AS one, 'hi' AS greeting, true AS ok, NULL AS nothing")
        .await
        .unwrap();
    let rows = rows(&messages);
    let row = rows[0];
    let names: Vec<_> = row.columns().iter().map(|c| c.name()).collect();
    assert_eq!(names, ["one", "greeting", "ok", "nothing"]);
    assert_eq!(row.get(0), Some("1"));
    assert_eq!(row.get(1), Some("hi"));
    assert_eq!(row.get(2), Some("t"));
    assert_eq!(row.get(3), None);
}

#[tokio::test]
async fn create_insert_select_on_memory_engine() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE crabs (id integer, name text, big boolean)")
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO crabs VALUES (1, 'ferris', true), (2, 'hermit', false)")
        .await
        .unwrap();

    let messages = client
        .simple_query("SELECT name, id FROM crabs")
        .await
        .unwrap();
    let rows = rows(&messages);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get(0), Some("ferris"));
    assert_eq!(rows[0].get(1), Some("1"));
    assert_eq!(rows[1].get(0), Some("hermit"));

    // Command tag row count
    let count = messages.iter().find_map(|m| match m {
        SimpleQueryMessage::CommandComplete(n) => Some(*n),
        _ => None,
    });
    assert_eq!(count, Some(2));
}

#[tokio::test]
async fn multiple_statements_in_one_query() {
    let client = connect(spawn_server().await).await;
    let messages = client.simple_query("SELECT 1; SELECT 2").await.unwrap();
    let rows = rows(&messages);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get(0), Some("1"));
    assert_eq!(rows[1].get(0), Some("2"));
}

#[tokio::test]
async fn undefined_table_reports_sqlstate_42p01() {
    let client = connect(spawn_server().await).await;
    let err = client
        .simple_query("SELECT * FROM missing")
        .await
        .unwrap_err();
    let db_err = err.as_db_error().expect("should be a server error");
    assert_eq!(
        db_err.code(),
        &tokio_postgres::error::SqlState::UNDEFINED_TABLE
    );
    assert_eq!(db_err.message(), "relation \"missing\" does not exist");

    // The session must stay usable after an error.
    let messages = client.simple_query("SELECT 1").await.unwrap();
    assert_eq!(rows(&messages).len(), 1);
}

#[tokio::test]
async fn syntax_error_reports_sqlstate_42601() {
    let client = connect(spawn_server().await).await;
    let err = client.simple_query("SELEC 1").await.unwrap_err();
    let db_err = err.as_db_error().unwrap();
    assert_eq!(
        db_err.code(),
        &tokio_postgres::error::SqlState::SYNTAX_ERROR
    );
}

#[tokio::test]
async fn integer_out_of_range_on_insert() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer)")
        .await
        .unwrap();
    let err = client
        .simple_query("INSERT INTO t VALUES (5000000000)")
        .await
        .unwrap_err();
    let db_err = err.as_db_error().unwrap();
    assert_eq!(
        db_err.code(),
        &tokio_postgres::error::SqlState::NUMERIC_VALUE_OUT_OF_RANGE
    );
}

#[tokio::test]
async fn unsupported_clauses_error_instead_of_silently_dropping() {
    let client = connect(spawn_server().await).await;
    for sql in [
        "SELECT 1 ORDER BY 1",
        "SELECT 1 LIMIT 1",
        "SELECT 1 GROUP BY 1",
        "SELECT 1 HAVING true",
    ] {
        let err = client.simple_query(sql).await.unwrap_err();
        let db_err = err
            .as_db_error()
            .unwrap_or_else(|| panic!("{sql} should error"));
        assert_eq!(
            db_err.code(),
            &tokio_postgres::error::SqlState::FEATURE_NOT_SUPPORTED,
            "{sql}"
        );
    }
}

#[tokio::test]
async fn multi_row_insert_is_atomic() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer)")
        .await
        .unwrap();
    let err = client
        .simple_query("INSERT INTO t VALUES (1), (2), (5000000000)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().unwrap().code(),
        &tokio_postgres::error::SqlState::NUMERIC_VALUE_OUT_OF_RANGE
    );

    // The failing statement must not leave rows 1 and 2 behind.
    let messages = client.simple_query("SELECT * FROM t").await.unwrap();
    assert_eq!(rows(&messages).len(), 0);
}

#[tokio::test]
async fn duplicate_insert_column_is_rejected() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (a integer, b integer)")
        .await
        .unwrap();
    let err = client
        .simple_query("INSERT INTO t (a, a) VALUES (1, 2)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().unwrap().code(),
        &tokio_postgres::error::SqlState::DUPLICATE_COLUMN
    );
}

#[tokio::test]
async fn insert_without_column_list_pads_missing_columns_with_null() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (a integer, b text)")
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO t VALUES (1)")
        .await
        .unwrap();

    let messages = client.simple_query("SELECT * FROM t").await.unwrap();
    let rows = rows(&messages);
    assert_eq!(rows[0].get(0), Some("1"));
    assert_eq!(rows[0].get(1), None);

    // With an explicit column list PG requires an exact match.
    let err = client
        .simple_query("INSERT INTO t (a, b) VALUES (2)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().unwrap().code(),
        &tokio_postgres::error::SqlState::SYNTAX_ERROR
    );
}

#[tokio::test]
async fn quoted_literals_coerce_to_column_types() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer, big bigint, ok boolean)")
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO t VALUES ('42', '9000000000', 'yes')")
        .await
        .unwrap();

    let messages = client.simple_query("SELECT * FROM t").await.unwrap();
    let rows = rows(&messages);
    assert_eq!(rows[0].get(0), Some("42"));
    assert_eq!(rows[0].get(1), Some("9000000000"));
    assert_eq!(rows[0].get(2), Some("t"));

    let err = client
        .simple_query("INSERT INTO t (id) VALUES ('abc')")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().unwrap().code(),
        &tokio_postgres::error::SqlState::INVALID_TEXT_REPRESENTATION
    );
}

#[tokio::test]
async fn create_table_if_not_exists_is_idempotent() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer)")
        .await
        .unwrap();
    client
        .simple_query("CREATE TABLE IF NOT EXISTS t (id integer)")
        .await
        .unwrap();

    // Without IF NOT EXISTS the duplicate still errors.
    let err = client
        .simple_query("CREATE TABLE t (id integer)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().unwrap().code(),
        &tokio_postgres::error::SqlState::DUPLICATE_TABLE
    );
}

#[tokio::test]
async fn unenforceable_ddl_is_rejected() {
    let client = connect(spawn_server().await).await;
    for sql in [
        "CREATE TABLE c (id integer PRIMARY KEY)",
        "CREATE TABLE c (id integer NOT NULL)",
        "CREATE TABLE c (s varchar(10))",
    ] {
        let err = client.simple_query(sql).await.unwrap_err();
        assert_eq!(
            err.as_db_error().unwrap().code(),
            &tokio_postgres::error::SqlState::FEATURE_NOT_SUPPORTED,
            "{sql}"
        );
    }
}

async fn read_backend_message(socket: &mut tokio::net::TcpStream) -> (u8, Vec<u8>) {
    let tag = socket.read_u8().await.unwrap();
    let len = socket.read_i32().await.unwrap() as usize;
    let mut body = vec![0u8; len - 4];
    socket.read_exact(&mut body).await.unwrap();
    (tag, body)
}

/// Cleartext startup on a raw socket, draining until ReadyForQuery.
async fn raw_session(port: u16) -> tokio::net::TcpStream {
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let mut body = Vec::new();
    body.extend_from_slice(&196608i32.to_be_bytes());
    body.extend_from_slice(b"user\0postgres\0\0");
    socket
        .write_all(&((body.len() + 4) as i32).to_be_bytes())
        .await
        .unwrap();
    socket.write_all(&body).await.unwrap();
    loop {
        let (tag, _) = read_backend_message(&mut socket).await;
        if tag == b'Z' {
            return socket;
        }
    }
}

fn frontend_message(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut msg = vec![tag];
    msg.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    msg.extend_from_slice(body);
    msg
}

/// A failed extended-protocol batch must produce exactly one ErrorResponse
/// and one ReadyForQuery (at Sync) — per-message replies desync drivers.
#[tokio::test]
async fn extended_protocol_errors_once_and_recovers_at_sync() {
    let port = spawn_server().await;
    let mut socket = raw_session(port).await;

    let mut batch = Vec::new();
    batch.extend(frontend_message(b'P', b"\0SELECT 1\0\x00\x00")); // Parse
    batch.extend(frontend_message(b'B', b"\0\0\x00\x00\x00\x00\x00\x00")); // Bind
    batch.extend(frontend_message(b'D', b"P\0")); // Describe portal
    batch.extend(frontend_message(b'E', b"\0\x00\x00\x00\x00")); // Execute
    batch.extend(frontend_message(b'S', b"")); // Sync
    socket.write_all(&batch).await.unwrap();

    let (tag, _) = read_backend_message(&mut socket).await;
    assert_eq!(tag, b'E', "first reply must be a single ErrorResponse");
    let (tag, body) = read_backend_message(&mut socket).await;
    assert_eq!(
        tag, b'Z',
        "Bind/Describe/Execute must be skipped until Sync"
    );
    assert_eq!(body, [b'I']);

    // The session must remain usable for simple queries afterwards.
    socket
        .write_all(&frontend_message(b'Q', b"SELECT 1\0"))
        .await
        .unwrap();
    let mut tags = Vec::new();
    loop {
        let (tag, _) = read_backend_message(&mut socket).await;
        tags.push(tag);
        if tag == b'Z' {
            break;
        }
    }
    assert_eq!(tags, [b'T', b'D', b'C', b'Z']);
}

/// psql and libpq open with SSLRequest; the server must answer `N` and then
/// complete a cleartext handshake on the same connection.
#[tokio::test]
async fn ssl_request_is_refused_then_startup_proceeds() {
    let port = spawn_server().await;
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();

    socket
        .write_all(&[0, 0, 0, 8, 4, 210, 22, 47])
        .await
        .unwrap(); // SSLRequest
    assert_eq!(socket.read_u8().await.unwrap(), b'N');

    let mut body = Vec::new();
    body.extend_from_slice(&196608i32.to_be_bytes());
    body.extend_from_slice(b"user\0postgres\0\0");
    socket
        .write_all(&((body.len() + 4) as i32).to_be_bytes())
        .await
        .unwrap();
    socket.write_all(&body).await.unwrap();

    // Read backend messages until ReadyForQuery('I'); first must be AuthenticationOk.
    let mut first_tag = None;
    loop {
        let tag = socket.read_u8().await.unwrap();
        let len = socket.read_i32().await.unwrap() as usize;
        let mut msg = vec![0u8; len - 4];
        socket.read_exact(&mut msg).await.unwrap();
        if first_tag.is_none() {
            first_tag = Some(tag);
            assert_eq!(tag, b'R', "first backend message must be Authentication");
            assert_eq!(msg, 0i32.to_be_bytes(), "must be AuthenticationOk");
        }
        if tag == b'Z' {
            assert_eq!(msg, [b'I']);
            break;
        }
    }
}
