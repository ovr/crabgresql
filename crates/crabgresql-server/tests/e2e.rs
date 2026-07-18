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
async fn order_by_name_expression_and_alias() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer, a integer, b integer)")
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO t VALUES (1, 3, 10), (2, 1, 40), (3, 2, 20)")
        .await
        .unwrap();

    // ORDER BY a column name: sorted by `a` ascending → id 2 (a=1), id 3 (a=2),
    // id 1 (a=3).
    let messages = client.simple_query("SELECT id FROM t ORDER BY a").await.unwrap();
    let ids: Vec<&str> = rows(&messages).iter().map(|r| r.get(0).unwrap()).collect();
    assert_eq!(ids, vec!["2", "3", "1"]);

    // ORDER BY an expression over a non-selected column (a + b): a+b is 13, 41,
    // 22 for ids 1,2,3 → ascending order ids 1,3,2.
    let messages = client
        .simple_query("SELECT id FROM t ORDER BY a + b")
        .await
        .unwrap();
    let ordered = rows(&messages);
    let ids: Vec<&str> = ordered.iter().map(|r| r.get(0).unwrap()).collect();
    assert_eq!(ids, vec!["1", "3", "2"]);
    // Only the single visible column is returned (the sort column is hidden).
    assert_eq!(ordered[0].len(), 1);

    // ORDER BY an output alias, descending: total = a+b, largest first → 2,3,1.
    let messages = client
        .simple_query("SELECT id, a + b AS total FROM t ORDER BY total DESC")
        .await
        .unwrap();
    let ids: Vec<&str> = rows(&messages).iter().map(|r| r.get(0).unwrap()).collect();
    assert_eq!(ids, vec!["2", "3", "1"]);
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
        "SELECT 1 LIMIT 1",
        "SELECT 1 GROUP BY 1",
        "SELECT 1 HAVING true",
        "SELECT DISTINCT 1",
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
async fn temp_table_shadows_permanent_within_the_session_only() {
    let port = spawn_server().await;

    // A permanent table lives in the shared engine.
    let a = connect(port).await;
    a.simple_query("CREATE TABLE t (v integer)").await.unwrap();
    a.simple_query("INSERT INTO t VALUES (1)").await.unwrap();

    // A same-named TEMP table shadows it for this session — no 42P07, and all
    // DML now resolves to the temp table.
    a.simple_query("CREATE TEMP TABLE t (v integer)")
        .await
        .unwrap();
    a.simple_query("INSERT INTO t VALUES (2), (3)")
        .await
        .unwrap();

    let temp_msgs = a.simple_query("SELECT v FROM t").await.unwrap();
    let temp_rows = rows(&temp_msgs);
    assert_eq!(temp_rows.len(), 2, "SELECT hits the temp table");
    assert_eq!(temp_rows[0].get(0), Some("2"));
    assert_eq!(temp_rows[1].get(0), Some("3"));

    // UPDATE and TRUNCATE hit the temp table too, never the shadowed permanent one.
    let msgs = a.simple_query("UPDATE t SET v = v * -1").await.unwrap();
    assert_eq!(command_count(&msgs), Some(2), "UPDATE hits the 2 temp rows");
    a.simple_query("TRUNCATE t").await.unwrap();
    assert_eq!(
        rows(&a.simple_query("SELECT v FROM t").await.unwrap()).len(),
        0,
        "TRUNCATE emptied the temp table"
    );

    // A second, fresh session has no temp store: it sees only the permanent
    // table (still holding its original row), proving the temp table is
    // session-scoped and left the permanent one untouched.
    let b = connect(port).await;
    let perm_msgs = b.simple_query("SELECT v FROM t").await.unwrap();
    let perm_rows = rows(&perm_msgs);
    assert_eq!(perm_rows.len(), 1);
    assert_eq!(
        perm_rows[0].get(0),
        Some("1"),
        "the permanent table was never shadowed for this session"
    );
}

#[tokio::test]
async fn unenforceable_ddl_is_rejected() {
    let client = connect(spawn_server().await).await;
    for sql in [
        "CREATE TABLE c (id integer PRIMARY KEY)",
        "CREATE TABLE c (id integer NOT NULL)",
        // Clauses we can't honor must be rejected, not silently dropped: CTAS
        // would discard the SELECT, and ON COMMIT needs the M2 txn engine.
        "CREATE TABLE c AS SELECT 1 AS x",
        "CREATE TEMP TABLE c AS SELECT 1 AS x",
        "CREATE TEMP TABLE c (x int) ON COMMIT DROP",
        "CREATE TEMP TABLE c (x int) ON COMMIT DELETE ROWS",
    ] {
        let err = client.simple_query(sql).await.unwrap_err();
        assert_eq!(
            err.as_db_error().unwrap().code(),
            &tokio_postgres::error::SqlState::FEATURE_NOT_SUPPORTED,
            "{sql}"
        );
    }
}

/// The numeric suffix of the first CommandComplete tag (`UPDATE 2` → 2).
fn command_count(messages: &[SimpleQueryMessage]) -> Option<u64> {
    messages.iter().find_map(|m| match m {
        SimpleQueryMessage::CommandComplete(n) => Some(*n),
        _ => None,
    })
}

#[tokio::test]
async fn full_crud_cycle_with_where() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE crabs (id integer, name text)")
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO crabs VALUES (1, 'ferris'), (2, 'hermit'), (3, 'king')")
        .await
        .unwrap();

    let messages = client
        .simple_query("SELECT name FROM crabs WHERE id > 1")
        .await
        .unwrap();
    let selected = rows(&messages);
    assert_eq!(selected.len(), 2);
    assert_eq!(selected[0].get(0), Some("hermit"));

    let messages = client
        .simple_query("UPDATE crabs SET name = 'crab' WHERE id > 1")
        .await
        .unwrap();
    assert_eq!(command_count(&messages), Some(2), "tag must be UPDATE 2");

    let messages = client
        .simple_query("DELETE FROM crabs WHERE name = 'crab'")
        .await
        .unwrap();
    assert_eq!(command_count(&messages), Some(2), "tag must be DELETE 2");

    let messages = client.simple_query("SELECT * FROM crabs").await.unwrap();
    let remaining = rows(&messages);
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].get(1), Some("ferris"));
}

/// The count of visible rows in `t`.
async fn row_count(client: &tokio_postgres::Client, table: &str) -> usize {
    let messages = client
        .simple_query(&format!("SELECT * FROM {table}"))
        .await
        .unwrap();
    rows(&messages).len()
}

#[tokio::test]
async fn rollback_undoes_inserts() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer)")
        .await
        .unwrap();
    client.simple_query("BEGIN").await.unwrap();
    client.simple_query("INSERT INTO t VALUES (1), (2)").await.unwrap();
    // The transaction sees its own uncommitted inserts.
    assert_eq!(row_count(&client, "t").await, 2);
    client.simple_query("ROLLBACK").await.unwrap();
    // After rollback the rows are gone — real MVCC undo, not just control flow.
    assert_eq!(row_count(&client, "t").await, 0);
}

#[tokio::test]
async fn rollback_restores_deleted_and_updated_rows() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer, label text)")
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO t VALUES (1, 'a'), (2, 'b')")
        .await
        .unwrap();

    client.simple_query("BEGIN").await.unwrap();
    client.simple_query("DELETE FROM t WHERE id = 1").await.unwrap();
    client
        .simple_query("UPDATE t SET label = 'B' WHERE id = 2")
        .await
        .unwrap();
    // Inside the block the changes are visible: id=1 gone, id=2 now 'B'.
    let msgs = client
        .simple_query("SELECT label FROM t ORDER BY 1")
        .await
        .unwrap();
    let seen: Vec<_> = rows(&msgs).iter().map(|r| r.get(0).unwrap().to_string()).collect();
    assert_eq!(seen, ["B"]);

    client.simple_query("ROLLBACK").await.unwrap();
    // Both the delete and the update are undone.
    let msgs = client
        .simple_query("SELECT label FROM t ORDER BY 1")
        .await
        .unwrap();
    let restored: Vec<_> = rows(&msgs).iter().map(|r| r.get(0).unwrap().to_string()).collect();
    assert_eq!(restored, ["a", "b"]);
}

#[tokio::test]
async fn commit_persists_changes() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer)")
        .await
        .unwrap();
    client.simple_query("BEGIN").await.unwrap();
    client.simple_query("INSERT INTO t VALUES (7)").await.unwrap();
    client.simple_query("COMMIT").await.unwrap();
    let msgs = client.simple_query("SELECT id FROM t").await.unwrap();
    let r = rows(&msgs);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].get(0), Some("7"));
}

#[tokio::test]
async fn uncommitted_changes_are_invisible_to_other_sessions() {
    let port = spawn_server().await;
    let writer = connect(port).await;
    let reader = connect(port).await;
    writer
        .simple_query("CREATE TABLE t (id integer)")
        .await
        .unwrap();
    writer.simple_query("BEGIN").await.unwrap();
    writer.simple_query("INSERT INTO t VALUES (1)").await.unwrap();
    // The writer sees its own row; a concurrent session does not.
    assert_eq!(row_count(&writer, "t").await, 1);
    assert_eq!(row_count(&reader, "t").await, 0);
    writer.simple_query("COMMIT").await.unwrap();
    // Once committed, the other session sees it.
    assert_eq!(row_count(&reader, "t").await, 1);
}

#[tokio::test]
async fn disconnect_mid_block_aborts_and_frees_the_row() {
    let port = spawn_server().await;
    let a = connect(port).await;
    a.simple_query("CREATE TABLE t (id integer)").await.unwrap();
    a.simple_query("INSERT INTO t VALUES (1)").await.unwrap();

    // B opens a block and updates the row (allocating an XID and stamping the
    // old version's xmax), then disconnects without COMMIT/ROLLBACK.
    let b = connect(port).await;
    b.simple_query("BEGIN").await.unwrap();
    assert_eq!(command_count(&b.simple_query("UPDATE t SET id = 2").await.unwrap()), Some(1));
    drop(b);
    // Give the server a moment to observe the disconnect and abort B's block.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // C can still update the row: B's abort-on-drop made the original version
    // live again. Without the fix, B's XID stays in-flight, the row is not
    // is_live, and this reports UPDATE 0.
    let c = connect(port).await;
    let msg = c.simple_query("UPDATE t SET id = 3").await.unwrap();
    assert_eq!(
        command_count(&msg),
        Some(1),
        "row must be updatable after B's abandoned block aborts on disconnect"
    );
    let sel = c.simple_query("SELECT id FROM t").await.unwrap();
    assert_eq!(rows(&sel)[0].get(0), Some("3"));
}

#[tokio::test]
async fn update_and_delete_without_where_hit_all_rows() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer)")
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO t VALUES (1), (2), (3)")
        .await
        .unwrap();

    let messages = client.simple_query("UPDATE t SET id = 0").await.unwrap();
    assert_eq!(command_count(&messages), Some(3));

    let messages = client.simple_query("DELETE FROM t").await.unwrap();
    assert_eq!(command_count(&messages), Some(3));
    let messages = client.simple_query("SELECT * FROM t").await.unwrap();
    assert_eq!(rows(&messages).len(), 0);
}

#[tokio::test]
async fn null_rows_do_not_match_comparisons() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer, v integer)")
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO t VALUES (1, 10), (2, NULL)")
        .await
        .unwrap();

    // Neither = nor <> matches a NULL: only IS NULL does.
    for (sql, expected) in [
        ("SELECT id FROM t WHERE v = 10", 1),
        ("SELECT id FROM t WHERE v <> 10", 0),
        ("SELECT id FROM t WHERE v IS NULL", 1),
        ("SELECT id FROM t WHERE v IS NOT NULL", 1),
    ] {
        let messages = client.simple_query(sql).await.unwrap();
        assert_eq!(rows(&messages).len(), expected, "{sql}");
    }
}

#[tokio::test]
async fn expressions_in_select_list() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer)")
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO t VALUES (41)")
        .await
        .unwrap();

    let messages = client
        .simple_query("SELECT id + 1, id * 2 AS double FROM t")
        .await
        .unwrap();
    let rows = rows(&messages);
    assert_eq!(rows[0].columns()[0].name(), "?column?");
    assert_eq!(rows[0].columns()[1].name(), "double");
    assert_eq!(rows[0].get(0), Some("42"));
    assert_eq!(rows[0].get(1), Some("82"));
}

#[tokio::test]
async fn update_set_expressions_see_the_old_row() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (a integer, b integer)")
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO t VALUES (1, 2)")
        .await
        .unwrap();

    // Both SET expressions evaluate against the OLD row: this swaps.
    client
        .simple_query("UPDATE t SET a = b, b = a")
        .await
        .unwrap();
    let messages = client.simple_query("SELECT a, b FROM t").await.unwrap();
    let rows = rows(&messages);
    assert_eq!(rows[0].get(0), Some("2"));
    assert_eq!(rows[0].get(1), Some("1"));
}

#[tokio::test]
async fn failing_update_leaves_no_rows_modified() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer, v integer)")
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)")
        .await
        .unwrap();

    // Fails on the id=2 row after id=1 evaluated fine.
    let err = client
        .simple_query("UPDATE t SET v = v / (id - 2)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().unwrap().code(),
        &tokio_postgres::error::SqlState::DIVISION_BY_ZERO
    );

    let messages = client
        .simple_query("SELECT v FROM t WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(
        rows(&messages)[0].get(0),
        Some("10"),
        "statement must be atomic"
    );
}

#[tokio::test]
async fn mid_stream_error_aborts_remaining_statements() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer)")
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO t VALUES (2), (0)")
        .await
        .unwrap();

    // The error surfaces mid-stream, after RowDescription (and possibly the
    // first row) went out; the trailing INSERT must not run.
    let err = client
        .simple_query("SELECT 10 / id FROM t; INSERT INTO t VALUES (7)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().unwrap().code(),
        &tokio_postgres::error::SqlState::DIVISION_BY_ZERO
    );

    let messages = client.simple_query("SELECT * FROM t").await.unwrap();
    assert_eq!(rows(&messages).len(), 2, "aborted INSERT must not run");
}

#[tokio::test]
async fn expression_type_errors_report_pg_sqlstates() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer, name text)")
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO t VALUES (2147483647, 'x')")
        .await
        .unwrap();

    for (sql, code, message) in [
        (
            "SELECT id FROM t WHERE 1",
            tokio_postgres::error::SqlState::DATATYPE_MISMATCH,
            "argument of WHERE must be type boolean, not type integer",
        ),
        (
            "SELECT id FROM t WHERE name = id",
            tokio_postgres::error::SqlState::UNDEFINED_FUNCTION,
            "operator does not exist: text = integer",
        ),
        (
            "SELECT '1' + '2'",
            tokio_postgres::error::SqlState::AMBIGUOUS_FUNCTION,
            "operator is not unique: unknown + unknown",
        ),
    ] {
        let err = client.simple_query(sql).await.unwrap_err();
        let db_err = err.as_db_error().unwrap();
        assert_eq!(db_err.code(), &code, "{sql}");
        assert_eq!(db_err.message(), message, "{sql}");
    }

    // Runtime overflow through UPDATE arithmetic.
    let err = client
        .simple_query("UPDATE t SET id = id + 1")
        .await
        .unwrap_err();
    let db_err = err.as_db_error().unwrap();
    assert_eq!(
        db_err.code(),
        &tokio_postgres::error::SqlState::NUMERIC_VALUE_OUT_OF_RANGE
    );
    assert_eq!(db_err.message(), "integer out of range");
}

#[tokio::test]
async fn insert_source_clauses_and_ragged_values_are_rejected() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (a integer, b integer)")
        .await
        .unwrap();

    // The INSERT source is a full query in PG (`VALUES ... LIMIT 1` inserts
    // one row); until that executes, it must be rejected, not ignored.
    for sql in [
        "INSERT INTO t (a) VALUES (1), (2) LIMIT 1",
        "INSERT INTO t (a) VALUES (1), (2) ORDER BY 1",
        "INSERT INTO t (a) VALUES (DEFAULT)",
        "UPDATE t SET a = DEFAULT",
    ] {
        let err = client.simple_query(sql).await.unwrap_err();
        assert_eq!(
            err.as_db_error().unwrap().code(),
            &tokio_postgres::error::SqlState::FEATURE_NOT_SUPPORTED,
            "{sql}"
        );
    }

    let err = client
        .simple_query("INSERT INTO t VALUES (1, 2), (3)")
        .await
        .unwrap_err();
    let db_err = err.as_db_error().unwrap();
    assert_eq!(
        db_err.code(),
        &tokio_postgres::error::SqlState::SYNTAX_ERROR
    );
    assert_eq!(db_err.message(), "VALUES lists must all be the same length");

    let messages = client.simple_query("SELECT * FROM t").await.unwrap();
    assert_eq!(
        rows(&messages).len(),
        0,
        "no rejected INSERT may leave rows"
    );
}

#[tokio::test]
async fn constant_update_overflow_errors_even_with_no_matching_rows() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer)")
        .await
        .unwrap();

    // PG const-folds the cast at plan time: the empty table must not turn
    // the error into `UPDATE 0`.
    let err = client
        .simple_query("UPDATE t SET id = 2147483648")
        .await
        .unwrap_err();
    let db_err = err.as_db_error().unwrap();
    assert_eq!(
        db_err.code(),
        &tokio_postgres::error::SqlState::NUMERIC_VALUE_OUT_OF_RANGE
    );
    assert_eq!(db_err.message(), "integer out of range");
}

#[tokio::test]
async fn integer_literals_distinguish_out_of_range_from_malformed() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer, ok boolean)")
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO t VALUES (1, 'tru'), (2, 'of')")
        .await
        .unwrap();

    // PG bool input accepts unambiguous prefixes.
    let messages = client
        .simple_query("SELECT id FROM t WHERE ok")
        .await
        .unwrap();
    let matched = rows(&messages);
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].get(0), Some("1"));

    let err = client
        .simple_query("SELECT id FROM t WHERE id = '3000000000'")
        .await
        .unwrap_err();
    let db_err = err.as_db_error().unwrap();
    assert_eq!(
        db_err.code(),
        &tokio_postgres::error::SqlState::NUMERIC_VALUE_OUT_OF_RANGE
    );
    assert_eq!(
        db_err.message(),
        "value \"3000000000\" is out of range for type integer"
    );
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

// --- Transactions: raw-socket helpers (tokio-postgres only exposes the numeric
// command count, not the tag text or the ReadyForQuery status byte) ---

/// Send a simple `Query` and collect every backend `(tag, body)` up to and
/// including the terminating ReadyForQuery.
async fn simple_query_raw(socket: &mut tokio::net::TcpStream, sql: &str) -> Vec<(u8, Vec<u8>)> {
    let mut q = sql.as_bytes().to_vec();
    q.push(0);
    socket.write_all(&frontend_message(b'Q', &q)).await.unwrap();
    let mut out = Vec::new();
    loop {
        let (tag, body) = read_backend_message(socket).await;
        let done = tag == b'Z';
        out.push((tag, body));
        if done {
            return out;
        }
    }
}

/// CommandComplete (`C`) tag strings, in order (NUL terminator stripped).
fn command_tags(msgs: &[(u8, Vec<u8>)]) -> Vec<String> {
    msgs.iter()
        .filter(|(tag, _)| *tag == b'C')
        .map(|(_, body)| String::from_utf8_lossy(body.strip_suffix(&[0]).unwrap_or(body)).into())
        .collect()
}

/// The status byte of the terminating ReadyForQuery (`I`/`T`/`E`).
fn ready_status(msgs: &[(u8, Vec<u8>)]) -> u8 {
    msgs.iter()
        .rev()
        .find(|(tag, _)| *tag == b'Z')
        .map(|(_, body)| body[0])
        .expect("a ReadyForQuery must terminate the batch")
}

/// Decode an ErrorResponse / NoticeResponse `(tag, body)` into its fields using
/// the wire codec, so the tests read errors the same way a client does.
fn fields(msg: &(u8, Vec<u8>)) -> crabgresql_pg_wire::ErrorFields {
    match crabgresql_pg_wire::BackendMessage::decode(msg.0, &msg.1).unwrap() {
        crabgresql_pg_wire::BackendMessage::ErrorResponse(f)
        | crabgresql_pg_wire::BackendMessage::NoticeResponse(f) => f,
        other => panic!("expected an ErrorResponse/NoticeResponse, got {other:?}"),
    }
}

#[tokio::test]
async fn transaction_status_and_tags_track_the_block() {
    let mut socket = raw_session(spawn_server().await).await;

    let msgs = simple_query_raw(&mut socket, "BEGIN").await;
    assert_eq!(command_tags(&msgs), ["BEGIN"]);
    assert_eq!(ready_status(&msgs), b'T');

    let msgs = simple_query_raw(&mut socket, "SELECT 1").await;
    assert_eq!(ready_status(&msgs), b'T', "still inside the block");

    // `END` is an alias for COMMIT and completes with the COMMIT tag.
    let msgs = simple_query_raw(&mut socket, "END").await;
    assert_eq!(command_tags(&msgs), ["COMMIT"]);
    assert_eq!(ready_status(&msgs), b'I');

    // START TRANSACTION enters the block but keeps its own distinct tag.
    let msgs = simple_query_raw(&mut socket, "START TRANSACTION").await;
    assert_eq!(command_tags(&msgs), ["START TRANSACTION"]);
    assert_eq!(ready_status(&msgs), b'T');

    let msgs = simple_query_raw(&mut socket, "ROLLBACK").await;
    assert_eq!(command_tags(&msgs), ["ROLLBACK"]);
    assert_eq!(ready_status(&msgs), b'I');
}

#[tokio::test]
async fn aborted_block_rejects_until_it_ends() {
    let mut socket = raw_session(spawn_server().await).await;
    simple_query_raw(&mut socket, "BEGIN").await;

    // An error inside the block moves it to the failed state ('E').
    let msgs = simple_query_raw(&mut socket, "SELECT * FROM missing").await;
    assert_eq!(ready_status(&msgs), b'E');

    // Everything but COMMIT/ROLLBACK is now rejected with 25P02.
    let msgs = simple_query_raw(&mut socket, "SELECT 1").await;
    let err = msgs
        .iter()
        .find(|(tag, _)| *tag == b'E')
        .expect("must report an error");
    assert_eq!(fields(err).code(), "25P02");
    assert!(
        !msgs.iter().any(|(tag, _)| *tag == b'D'),
        "no rows in an aborted block"
    );
    assert_eq!(ready_status(&msgs), b'E');

    // ROLLBACK clears the block and the session is usable again.
    let msgs = simple_query_raw(&mut socket, "ROLLBACK").await;
    assert_eq!(command_tags(&msgs), ["ROLLBACK"]);
    assert_eq!(ready_status(&msgs), b'I');
    let msgs = simple_query_raw(&mut socket, "SELECT 1").await;
    assert!(msgs.iter().any(|(tag, _)| *tag == b'D'));
    assert_eq!(ready_status(&msgs), b'I');
}

#[tokio::test]
async fn commit_of_a_failed_block_reports_rollback() {
    let mut socket = raw_session(spawn_server().await).await;
    simple_query_raw(&mut socket, "BEGIN").await;
    simple_query_raw(&mut socket, "SELECT * FROM missing").await; // aborts the block

    let msgs = simple_query_raw(&mut socket, "COMMIT").await;
    assert_eq!(
        command_tags(&msgs),
        ["ROLLBACK"],
        "COMMIT of a failed block is a rollback"
    );
    assert_eq!(ready_status(&msgs), b'I');
}

#[tokio::test]
async fn redundant_transaction_commands_warn() {
    let mut socket = raw_session(spawn_server().await).await;

    // COMMIT with no block open warns (25P01, severity WARNING) but succeeds.
    let msgs = simple_query_raw(&mut socket, "COMMIT").await;
    let notice = msgs
        .iter()
        .find(|(tag, _)| *tag == b'N')
        .expect("a warning is expected");
    let notice = fields(notice);
    assert_eq!(notice.code(), "25P01");
    assert_eq!(notice.severity(), "WARNING");
    assert_eq!(notice.message(), "there is no transaction in progress");
    assert_eq!(command_tags(&msgs), ["COMMIT"]);
    assert_eq!(ready_status(&msgs), b'I');

    // A nested BEGIN warns (25001) but stays in the block.
    simple_query_raw(&mut socket, "BEGIN").await;
    let msgs = simple_query_raw(&mut socket, "BEGIN").await;
    let notice = msgs
        .iter()
        .find(|(tag, _)| *tag == b'N')
        .expect("a warning is expected");
    let notice = fields(notice);
    assert_eq!(notice.code(), "25001");
    assert_eq!(
        notice.message(),
        "there is already a transaction in progress"
    );
    assert_eq!(command_tags(&msgs), ["BEGIN"]);
    assert_eq!(ready_status(&msgs), b'T');
}

#[tokio::test]
async fn syntax_error_inside_block_aborts_it() {
    let mut socket = raw_session(spawn_server().await).await;
    simple_query_raw(&mut socket, "BEGIN").await;

    // A parse error inside the block aborts it, just like an execution error.
    let msgs = simple_query_raw(&mut socket, "SELCT 1").await;
    let err = msgs
        .iter()
        .find(|(tag, _)| *tag == b'E')
        .expect("must error");
    assert_eq!(fields(err).code(), "42601");
    assert_eq!(ready_status(&msgs), b'E');

    // The next statement is then rejected until the block ends.
    let msgs = simple_query_raw(&mut socket, "SELECT 1").await;
    let err = msgs
        .iter()
        .find(|(tag, _)| *tag == b'E')
        .expect("must error");
    assert_eq!(fields(err).code(), "25P02");
    assert_eq!(ready_status(&msgs), b'E');

    simple_query_raw(&mut socket, "ROLLBACK").await;
    let msgs = simple_query_raw(&mut socket, "SELECT 1").await;
    assert_eq!(ready_status(&msgs), b'I');
}

#[tokio::test]
async fn unsupported_begin_mode_inside_block_warns_without_aborting() {
    let mut socket = raw_session(spawn_server().await).await;
    simple_query_raw(&mut socket, "BEGIN").await;

    // Inside a block PG ignores a nested BEGIN's arguments and only warns — an
    // unsupported mode must not turn into an error that aborts the block.
    let msgs = simple_query_raw(&mut socket, "BEGIN ISOLATION LEVEL SERIALIZABLE").await;
    let notice = msgs
        .iter()
        .find(|(tag, _)| *tag == b'N')
        .expect("a warning is expected");
    assert_eq!(fields(notice).code(), "25001");
    assert!(!msgs.iter().any(|(tag, _)| *tag == b'E'), "must not error");
    assert_eq!(command_tags(&msgs), ["BEGIN"]);
    assert_eq!(ready_status(&msgs), b'T', "the block stays open");

    // The block is still usable.
    let msgs = simple_query_raw(&mut socket, "SELECT 1").await;
    assert!(msgs.iter().any(|(tag, _)| *tag == b'D'));
    assert_eq!(ready_status(&msgs), b'T');
    simple_query_raw(&mut socket, "COMMIT").await;
}

#[tokio::test]
async fn truncate_empties_tables() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer)")
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO t VALUES (1), (2), (3)")
        .await
        .unwrap();

    client.simple_query("TRUNCATE t").await.unwrap();
    assert_eq!(
        rows(&client.simple_query("SELECT * FROM t").await.unwrap()).len(),
        0
    );

    // The `TRUNCATE TABLE` keyword form works too.
    client
        .simple_query("INSERT INTO t VALUES (9)")
        .await
        .unwrap();
    client.simple_query("TRUNCATE TABLE t").await.unwrap();
    assert_eq!(
        rows(&client.simple_query("SELECT * FROM t").await.unwrap()).len(),
        0
    );

    // A missing table fails the statement with 42P01.
    let err = client.simple_query("TRUNCATE nope").await.unwrap_err();
    assert_eq!(
        err.as_db_error().unwrap().code(),
        &tokio_postgres::error::SqlState::UNDEFINED_TABLE
    );
}

#[tokio::test]
async fn truncate_resolves_every_table_before_emptying() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE a (id integer)")
        .await
        .unwrap();
    client
        .simple_query("INSERT INTO a VALUES (1)")
        .await
        .unwrap();

    // The second name is missing: the whole statement fails and `a` is untouched.
    let err = client
        .simple_query("TRUNCATE a, missing")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().unwrap().code(),
        &tokio_postgres::error::SqlState::UNDEFINED_TABLE
    );
    assert_eq!(
        rows(&client.simple_query("SELECT * FROM a").await.unwrap()).len(),
        1,
        "no table may be emptied when any named table is missing"
    );
}

#[tokio::test]
async fn unsupported_transaction_forms_are_rejected() {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer)")
        .await
        .unwrap();
    for sql in [
        "BEGIN ISOLATION LEVEL SERIALIZABLE",
        "BEGIN TRANSACTION READ ONLY",
        "SAVEPOINT s",
        "ROLLBACK TO SAVEPOINT s",
        "TRUNCATE t CASCADE",
        "TRUNCATE t RESTART IDENTITY",
    ] {
        let err = client.simple_query(sql).await.unwrap_err();
        assert_eq!(
            err.as_db_error()
                .unwrap_or_else(|| panic!("{sql} should error"))
                .code(),
            &tokio_postgres::error::SqlState::FEATURE_NOT_SUPPORTED,
            "{sql}"
        );
    }
}
