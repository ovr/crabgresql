//! End-to-end tests: a real driver (tokio-postgres) against an in-process
//! server on an ephemeral port, plus raw-socket checks of the startup phase.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use anyhow::Context as _;
use crabgresql_memory_storage::MemoryEngine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_postgres::{NoTls, SimpleQueryMessage};

async fn spawn_server() -> u16 {
    let listener = match TcpListener::bind(("127.0.0.1", 0)).await {
        Ok(listener) => listener,
        Err(error) => panic!("failed to bind test server: {error}"),
    };
    let port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(error) => panic!("failed to read test server address: {error}"),
    };
    tokio::spawn(crabgresql_server::serve(
        listener,
        Arc::new(MemoryEngine::new()),
    ));
    port
}

async fn connect(port: u16) -> tokio_postgres::Client {
    connect_as(port, "postgres", "postgres").await
}

async fn connect_as(port: u16, user: &str, database: &str) -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(port)
        .user(user)
        .dbname(database)
        .connect(NoTls)
        .await
        .expect("handshake should succeed");
    tokio::spawn(conn);
    client
}

#[tokio::test]
async fn enum_catalog_and_type_boundaries_match_pg() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;

    client.simple_query("CREATE TYPE shell_only").await?;
    let err = client
        .simple_query("CREATE TABLE shell_table (value shell_only)")
        .await
        .unwrap_err();
    assert_eq!(err.as_db_error().expect("database error").code(), &SqlState::UNDEFINED_OBJECT);
    assert_eq!(
        err.as_db_error().expect("database error").message(),
        "type \"shell_only\" is only a shell"
    );

    let err = client
        .simple_query("CREATE TYPE int4 AS ENUM ('shadow')")
        .await
        .unwrap_err();
    assert_eq!(err.as_db_error().expect("database error").code(), &SqlState::DUPLICATE_OBJECT);

    let err = client
        .simple_query("CREATE TABLE unsupported (value box)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::FEATURE_NOT_SUPPORTED
    );

    client
        .simple_query("CREATE TYPE rainbow AS ENUM ('red', 'green')")
        .await?;
    client
        .simple_query("CREATE TABLE enumtest (value rainbow)")
        .await?;

    let oid_overlap = client
        .simple_query(
            "SELECT count(*) FROM pg_type t JOIN pg_class c ON t.oid = c.oid \
             WHERE t.typname = 'rainbow' AND c.relname = 'enumtest'",
        )
        .await?;
    assert_eq!(rows(&oid_overlap)[0].get(0), Some("0"));

    for target in ["varchar", "name", "bpchar"] {
        let sql = format!("SELECT 'red'::rainbow::{target}");
        let err = client.simple_query(&sql).await.unwrap_err();
        assert_eq!(err.as_db_error().expect("database error").code(), &SqlState::CANNOT_COERCE);
    }
    client
        .simple_query("SELECT 'red'::rainbow::text")
        .await?;

    let err = client
        .simple_query("SELECT 'red'::rainbow > 1")
        .await
        .unwrap_err();
    assert_eq!(err.as_db_error().expect("database error").code(), &SqlState::UNDEFINED_FUNCTION);
    assert_eq!(
        err.as_db_error().expect("database error").message(),
        "operator does not exist: rainbow > integer"
    );

    client.simple_query("CREATE TYPE zeta AS ENUM ('z')").await?;
    client.simple_query("CREATE TYPE alpha AS ENUM ('a')").await?;
    let ordered = client
        .simple_query(
            "SELECT typname FROM pg_type WHERE typname = 'zeta' OR typname = 'alpha'",
        )
        .await?;
    let ordered = rows(&ordered);
    assert_eq!(ordered[0].get(0), Some("zeta"));
    assert_eq!(ordered[1].get(0), Some("alpha"));

    client.simple_query("CREATE TYPE xbase").await?;
    client
        .simple_query(
            "CREATE FUNCTION xbase_in(cstring) RETURNS xbase AS 'int8in' LANGUAGE internal; \
             CREATE FUNCTION xbase_out(xbase) RETURNS cstring AS 'int8out' LANGUAGE internal; \
             CREATE TYPE xbase (input = xbase_in, output = xbase_out, like = int8); \
             CREATE CAST (int8 AS xbase) WITHOUT FUNCTION",
        )
        .await?;
    let err = client
        .simple_query("SELECT 1::int8::xbase > 0::int8::xbase")
        .await
        .unwrap_err();
    assert_eq!(err.as_db_error().expect("database error").code(), &SqlState::UNDEFINED_FUNCTION);
    assert_eq!(
        err.as_db_error().expect("database error").message(),
        "operator does not exist: xbase > xbase"
    );

    Ok(())
}

#[tokio::test]
async fn information_schema_reflects_live_relations_and_session_identity() -> anyhow::Result<()> {
    let client = connect_as(spawn_server().await, "catalog_user", "catalog_db").await;
    client
        .simple_query("CREATE TABLE inventory (id int4, code varchar(8))")
        .await?;

    let table_messages = client
        .simple_query(
            "SELECT table_catalog, table_schema, table_name, table_type \
             FROM information_schema.tables WHERE table_name = 'inventory'",
        )
        .await?;
    let table_rows = rows(&table_messages);
    assert_eq!(table_rows.len(), 1);
    assert_eq!(table_rows[0].get(0), Some("catalog_db"));
    assert_eq!(table_rows[0].get(1), Some("public"));
    assert_eq!(table_rows[0].get(2), Some("inventory"));
    assert_eq!(table_rows[0].get(3), Some("BASE TABLE"));

    let column_messages = client
        .simple_query(
            "SELECT column_name, ordinal_position, data_type, character_maximum_length, \
                    udt_catalog, udt_schema, udt_name, is_generated \
             FROM information_schema.columns \
             WHERE table_name = 'inventory' ORDER BY ordinal_position",
        )
        .await?;
    let columns = rows(&column_messages);
    assert_eq!(columns.len(), 2);
    assert_eq!(columns[0].get(0), Some("id"));
    assert_eq!(columns[0].get(1), Some("1"));
    assert_eq!(columns[0].get(2), Some("integer"));
    assert_eq!(columns[1].get(0), Some("code"));
    assert_eq!(columns[1].get(3), Some("8"));
    assert_eq!(columns[1].get(4), Some("catalog_db"));
    assert_eq!(columns[1].get(5), Some("pg_catalog"));
    assert_eq!(columns[1].get(6), Some("varchar"));
    assert_eq!(columns[1].get(7), Some("NEVER"));

    client
        .simple_query("CREATE TEMP TABLE scratch (v int4)")
        .await?;
    let temp_messages = client
        .simple_query(
            "SELECT table_schema, table_type FROM information_schema.tables \
             WHERE table_name = 'scratch'",
        )
        .await?;
    let temp = rows(&temp_messages);
    assert_eq!(temp.len(), 1);
    assert!(
        temp[0]
            .get(0)
            .context("temporary table schema is missing")?
            .starts_with("pg_temp_")
    );
    assert_eq!(temp[0].get(1), Some("LOCAL TEMPORARY"));

    let err = client
        .simple_query("SELECT * FROM tables")
        .await
        .unwrap_err();
    assert_eq!(
        err.code().expect("database error has SQLSTATE").code(),
        "42P01"
    );
    let err = client
        .simple_query("INSERT INTO information_schema.tables VALUES (1)")
        .await
        .unwrap_err();
    assert_eq!(
        err.code().expect("database error has SQLSTATE").code(),
        "42501"
    );

    Ok(())
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

/// tokio-postgres drives the extended protocol: it sends Parse with no declared
/// parameter types and relies on the server to infer them and return binary
/// results. A parameterized arithmetic query must round-trip through
/// inference + binary decode.
#[tokio::test]
async fn extended_query_infers_params_and_returns_binary() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    let row = client
        .query_one("SELECT $1::int4 + $2::int4 AS sum", &[&1i32, &2i32])
        .await?;
    let sum: i32 = row.get("sum");
    assert_eq!(sum, 3);

    // A bigint result exercises 8-byte binary output.
    let row = client.query_one("SELECT $1::int8 AS v", &[&42i64]).await?;
    assert_eq!(row.get::<_, i64>("v"), 42);

    // Text and bool parameters + results.
    let row = client
        .query_one("SELECT $1::text AS t, $2::bool AS b", &[&"hi", &true])
        .await?;
    assert_eq!(row.get::<_, &str>("t"), "hi");
    assert!(row.get::<_, bool>("b"));

    Ok(())
}

/// A parameter typed only by its use against a table column: the server infers
/// `$1` from the compared column, with no cast in the SQL.
#[tokio::test]
async fn extended_query_infers_param_from_column_context() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE nums (id int4)").await?;
    client
        .simple_query("INSERT INTO nums VALUES (5), (7), (9)")
        .await?;

    let rows = client
        .query("SELECT id FROM nums WHERE id = $1", &[&7i32])
        .await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, i32>("id"), 7);

    Ok(())
}

/// A prepared statement is reused across executions with different values, and a
/// NULL parameter round-trips.
#[tokio::test]
async fn prepared_statement_reused_and_null_param() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    let stmt = client.prepare("SELECT $1::int8 AS v").await?;
    assert_eq!(
        client.query_one(&stmt, &[&10i64]).await?.get::<_, i64>("v"),
        10
    );
    assert_eq!(
        client.query_one(&stmt, &[&20i64]).await?.get::<_, i64>("v"),
        20
    );

    let row = client
        .query_one("SELECT $1::int4 AS v", &[&Option::<i32>::None])
        .await?;
    assert_eq!(row.get::<_, Option<i32>>("v"), None);

    Ok(())
}

/// A parameter whose type cannot be determined is reported (42P18), and the
/// connection stays usable.
#[tokio::test]
async fn undeterminable_param_type_errors() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    let err = client
        .query("SELECT $1", &[&1i32])
        .await
        .expect_err("bare $1 has no type context");
    assert_eq!(
        err.code().expect("has SQLSTATE").code(),
        "42P18",
        "could not determine data type of parameter"
    );
    // Still usable.
    let row = client.query_one("SELECT $1::int4 AS v", &[&5i32]).await?;
    assert_eq!(row.get::<_, i32>("v"), 5);

    Ok(())
}

#[tokio::test]
async fn select_one() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    let messages = client.simple_query("SELECT 1").await?;
    let rows = rows(&messages);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].columns()[0].name(), "?column?");
    assert_eq!(rows[0].get(0), Some("1"));

    Ok(())
}

#[tokio::test]
async fn select_literals_with_aliases() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    let messages = client
        .simple_query("SELECT 1 AS one, 'hi' AS greeting, true AS ok, NULL AS nothing")
        .await?;
    let rows = rows(&messages);
    let row = rows[0];
    let names: Vec<_> = row.columns().iter().map(|c| c.name()).collect();
    assert_eq!(names, ["one", "greeting", "ok", "nothing"]);
    assert_eq!(row.get(0), Some("1"));
    assert_eq!(row.get(1), Some("hi"));
    assert_eq!(row.get(2), Some("t"));
    assert_eq!(row.get(3), None);

    Ok(())
}

#[tokio::test]
async fn regex_and_similar_to_operators() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    let messages = client
        .simple_query(
            "SELECT 'abc' ~ '^a' AS m, 'ABC' ~* 'abc' AS ci, 'abc' !~ 'z' AS nm, \
             'abc' SIMILAR TO '(b|a)%' AS sim, 'abc' NOT SIMILAR TO 'x%' AS nsim",
        )
        .await?;
    let rows = rows(&messages);
    let row = rows[0];
    let names: Vec<_> = row.columns().iter().map(|c| c.name()).collect();
    assert_eq!(names, ["m", "ci", "nm", "sim", "nsim"]);
    for i in 0..5 {
        assert_eq!(row.get(i), Some("t"));
    }

    Ok(())
}

#[tokio::test]
async fn hex_string_literals_bind_display_and_cast() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    let messages = client
        .simple_query("SELECT X'00000001', X'FF', X'00000001'::int4, X'FFFFFFFF'::int4, X''")
        .await?;
    let rows = rows(&messages);
    let row = rows[0];
    // X'...' is bit(n), displayed as a zero-padded binary string.
    assert_eq!(row.get(0), Some("00000000000000000000000000000001"));
    assert_eq!(row.get(1), Some("11111111"));
    // bit -> int4 reinterprets the bits as two's-complement.
    assert_eq!(row.get(2), Some("1"));
    assert_eq!(row.get(3), Some("-1"));
    // A zero-length bit string prints as the empty string, as in PG.
    assert_eq!(row.get(4), Some(""));

    Ok(())
}

#[tokio::test]
async fn create_insert_select_on_memory_engine() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE crabs (id integer, name text, big boolean)")
        .await?;
    client
        .simple_query("INSERT INTO crabs VALUES (1, 'ferris', true), (2, 'hermit', false)")
        .await?;

    let messages = client.simple_query("SELECT name, id FROM crabs").await?;
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

    Ok(())
}

#[tokio::test]
async fn order_by_name_expression_and_alias() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer, a integer, b integer)")
        .await?;
    client
        .simple_query("INSERT INTO t VALUES (1, 3, 10), (2, 1, 40), (3, 2, 20)")
        .await?;

    // ORDER BY a column name: sorted by `a` ascending → id 2 (a=1), id 3 (a=2),
    // id 1 (a=3).
    let messages = client.simple_query("SELECT id FROM t ORDER BY a").await?;
    let ids: Vec<&str> = rows(&messages)
        .iter()
        .map(|r| r.get(0).context("id column is missing"))
        .collect::<anyhow::Result<_>>()?;
    assert_eq!(ids, vec!["2", "3", "1"]);

    // ORDER BY an expression over a non-selected column (a + b): a+b is 13, 41,
    // 22 for ids 1,2,3 → ascending order ids 1,3,2.
    let messages = client
        .simple_query("SELECT id FROM t ORDER BY a + b")
        .await?;
    let ordered = rows(&messages);
    let ids: Vec<&str> = ordered
        .iter()
        .map(|r| r.get(0).context("id column is missing"))
        .collect::<anyhow::Result<_>>()?;
    assert_eq!(ids, vec!["1", "3", "2"]);
    // Only the single visible column is returned (the sort column is hidden).
    assert_eq!(ordered[0].len(), 1);

    // ORDER BY an output alias, descending: total = a+b, largest first → 2,3,1.
    let messages = client
        .simple_query("SELECT id, a + b AS total FROM t ORDER BY total DESC")
        .await?;
    let ids: Vec<&str> = rows(&messages)
        .iter()
        .map(|r| r.get(0).context("id column is missing"))
        .collect::<anyhow::Result<_>>()?;
    assert_eq!(ids, vec!["2", "3", "1"]);

    Ok(())
}

#[tokio::test]
async fn multiple_statements_in_one_query() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    let messages = client.simple_query("SELECT 1; SELECT 2").await?;
    let rows = rows(&messages);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get(0), Some("1"));
    assert_eq!(rows[1].get(0), Some("2"));

    Ok(())
}

#[tokio::test]
async fn undefined_table_reports_sqlstate_42p01() -> anyhow::Result<()> {
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
    let messages = client.simple_query("SELECT 1").await?;
    assert_eq!(rows(&messages).len(), 1);

    Ok(())
}

#[tokio::test]
async fn drop_table_lifecycle() {
    let mut socket = raw_session(spawn_server().await).await;

    // A successful drop returns the bare `DROP TABLE` command tag.
    simple_query_raw(&mut socket, "CREATE TABLE t (a int)").await;
    let msgs = simple_query_raw(&mut socket, "DROP TABLE t").await;
    assert_eq!(command_tags(&msgs), ["DROP TABLE"]);
    assert_eq!(ready_status(&msgs), b'I');

    // The relation is really gone.
    let msgs = simple_query_raw(&mut socket, "SELECT * FROM t").await;
    let err = msgs
        .iter()
        .find(|(tag, _)| *tag == b'E')
        .expect("must report an error");
    assert_eq!(fields(err).code(), "42P01");

    // Dropping a missing table without IF EXISTS errors 42P01. PG uses the noun
    // "table" here (not "relation").
    let msgs = simple_query_raw(&mut socket, "DROP TABLE t").await;
    let err = msgs
        .iter()
        .find(|(tag, _)| *tag == b'E')
        .expect("must report an error");
    let err = fields(err);
    assert_eq!(err.code(), "42P01");
    assert_eq!(err.message(), "table \"t\" does not exist");

    // DROP TABLE IF EXISTS of a missing table warns and still succeeds.
    let msgs = simple_query_raw(&mut socket, "DROP TABLE IF EXISTS t").await;
    let notice = fields(
        msgs.iter()
            .find(|(tag, _)| *tag == b'N')
            .expect("a NOTICE is expected"),
    );
    assert_eq!(notice.severity(), "NOTICE");
    assert_eq!(notice.message(), "table \"t\" does not exist, skipping");
    assert_eq!(command_tags(&msgs), ["DROP TABLE"]);
    assert_eq!(ready_status(&msgs), b'I');

    // The name is free to reuse after a drop.
    let msgs = simple_query_raw(&mut socket, "CREATE TABLE t (a int)").await;
    assert_eq!(command_tags(&msgs), ["CREATE TABLE"]);
}

#[tokio::test]
async fn drop_table_rejects_duplicate_names() {
    let mut socket = raw_session(spawn_server().await).await;
    simple_query_raw(&mut socket, "CREATE TABLE t (a int)").await;

    // A target named twice is rejected before anything is dropped, matching PG.
    let msgs = simple_query_raw(&mut socket, "DROP TABLE t, t").await;
    let err = fields(
        msgs.iter()
            .find(|(tag, _)| *tag == b'E')
            .expect("must report an error"),
    );
    assert_eq!(err.code(), "42710");
    assert_eq!(err.message(), "table \"t\" specified more than once");

    // The table is untouched: the rejected DROP dropped nothing.
    let msgs = simple_query_raw(&mut socket, "DROP TABLE t").await;
    assert_eq!(command_tags(&msgs), ["DROP TABLE"]);
}

#[tokio::test]
async fn drop_table_resolves_temp_first() {
    let mut socket = raw_session(spawn_server().await).await;

    // A temp table shadows a same-named permanent one; DROP resolves temp-first,
    // so it removes the temp table and leaves the permanent one intact.
    simple_query_raw(&mut socket, "CREATE TABLE t (a int)").await;
    simple_query_raw(&mut socket, "INSERT INTO t VALUES (1)").await;
    simple_query_raw(&mut socket, "CREATE TEMP TABLE t (a int)").await;
    let msgs = simple_query_raw(&mut socket, "DROP TABLE t").await;
    assert_eq!(command_tags(&msgs), ["DROP TABLE"]);

    // The permanent table is still there with its row.
    let msgs = simple_query_raw(&mut socket, "SELECT a FROM t").await;
    assert!(
        msgs.iter().any(|(tag, _)| *tag == b'D'),
        "permanent row remains"
    );

    // Dropping again now removes the permanent table.
    let msgs = simple_query_raw(&mut socket, "DROP TABLE t").await;
    assert_eq!(command_tags(&msgs), ["DROP TABLE"]);
    let msgs = simple_query_raw(&mut socket, "SELECT a FROM t").await;
    let err = msgs
        .iter()
        .find(|(tag, _)| *tag == b'E')
        .expect("relation is gone");
    assert_eq!(fields(err).code(), "42P01");
}

#[tokio::test]
async fn syntax_error_reports_sqlstate_42601() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    let err = client.simple_query("SELEC 1").await.unwrap_err();
    let db_err = err
        .as_db_error()
        .context("database error details are missing")?;
    assert_eq!(
        db_err.code(),
        &tokio_postgres::error::SqlState::SYNTAX_ERROR
    );

    Ok(())
}

#[tokio::test]
async fn integer_out_of_range_on_insert() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id integer)").await?;
    let err = client
        .simple_query("INSERT INTO t VALUES (5000000000)")
        .await
        .unwrap_err();
    let db_err = err
        .as_db_error()
        .context("database error details are missing")?;
    assert_eq!(
        db_err.code(),
        &tokio_postgres::error::SqlState::NUMERIC_VALUE_OUT_OF_RANGE
    );

    Ok(())
}

#[tokio::test]
async fn unsupported_clauses_error_instead_of_silently_dropping() {
    let client = connect(spawn_server().await).await;
    // GROUP BY / HAVING are supported now (see aggregate tests); the rest still
    // error rather than being silently dropped.
    for sql in ["SELECT 1 FETCH FIRST 1 ROW ONLY", "SELECT DISTINCT 1"] {
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
async fn aggregates_over_a_table() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (a integer, b integer)")
        .await?;
    client
        .simple_query("INSERT INTO t VALUES (1, 10), (1, 20), (2, 5), (2, NULL)")
        .await?;

    // Whole-table aggregates: one row, count/min/max/sum over all rows.
    let messages = client
        .simple_query("SELECT count(*), min(b), max(b), sum(b) FROM t")
        .await?;
    let whole = rows(&messages);
    assert_eq!(whole.len(), 1);
    assert_eq!(whole[0].get(0), Some("4")); // count(*) counts every row
    assert_eq!(whole[0].get(1), Some("5")); // min ignores NULL
    assert_eq!(whole[0].get(2), Some("20"));
    assert_eq!(whole[0].get(3), Some("35"));

    // count(expr) skips NULLs where count(*) does not.
    let messages = client
        .simple_query("SELECT count(b), count(*) FROM t")
        .await?;
    let counts = rows(&messages);
    assert_eq!(counts[0].get(0), Some("3"));
    assert_eq!(counts[0].get(1), Some("4"));

    // GROUP BY + HAVING + ORDER BY.
    let messages = client
        .simple_query("SELECT a, count(*), sum(b) FROM t GROUP BY a HAVING count(*) > 1 ORDER BY a")
        .await?;
    let grouped = rows(&messages);
    assert_eq!(grouped.len(), 2);
    assert_eq!(
        (grouped[0].get(0), grouped[0].get(1), grouped[0].get(2)),
        (Some("1"), Some("2"), Some("30"))
    );
    assert_eq!(
        (grouped[1].get(0), grouped[1].get(1), grouped[1].get(2)),
        (Some("2"), Some("2"), Some("5"))
    );

    // An empty group: sum is NULL, count is 0.
    let messages = client
        .simple_query("SELECT count(*), sum(b) FROM t WHERE a > 100")
        .await?;
    let empty = rows(&messages);
    assert_eq!(empty[0].get(0), Some("0"));
    assert_eq!(empty[0].get(1), None); // NULL

    // Ungrouped column outside an aggregate is an error.
    let err = client
        .simple_query("SELECT a, count(*) FROM t")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::GROUPING_ERROR,
    );

    Ok(())
}

#[tokio::test]
async fn limit_and_offset_slice_ordered_rows() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id integer)").await?;
    client
        .simple_query("INSERT INTO t VALUES (3), (1), (4), (1), (5), (9)")
        .await?;

    // LIMIT/OFFSET apply after ORDER BY: sorted ids are 1,1,3,4,5,9.
    let messages = client
        .simple_query("SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 1")
        .await?;
    let got: Vec<_> = rows(&messages)
        .iter()
        .map(|r| r.get(0).context("id column is missing").map(str::to_string))
        .collect::<anyhow::Result<Vec<_>>>()?;
    assert_eq!(got, ["1", "3"]);

    // OFFSET 0 is a no-op fence (the float4/float8 pattern): all rows, in order.
    let messages = client
        .simple_query("SELECT id FROM t ORDER BY id OFFSET 0")
        .await?;
    assert_eq!(rows(&messages).len(), 6);

    Ok(())
}

#[tokio::test]
async fn multi_row_insert_is_atomic() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id integer)").await?;
    let err = client
        .simple_query("INSERT INTO t VALUES (1), (2), (5000000000)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::NUMERIC_VALUE_OUT_OF_RANGE
    );

    // The failing statement must not leave rows 1 and 2 behind.
    let messages = client.simple_query("SELECT * FROM t").await?;
    assert_eq!(rows(&messages).len(), 0);

    Ok(())
}

#[tokio::test]
async fn duplicate_insert_column_is_rejected() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (a integer, b integer)")
        .await?;
    let err = client
        .simple_query("INSERT INTO t (a, a) VALUES (1, 2)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::DUPLICATE_COLUMN
    );

    Ok(())
}

#[tokio::test]
async fn insert_without_column_list_pads_missing_columns_with_null() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (a integer, b text)")
        .await?;
    client.simple_query("INSERT INTO t VALUES (1)").await?;

    let messages = client.simple_query("SELECT * FROM t").await?;
    let rows = rows(&messages);
    assert_eq!(rows[0].get(0), Some("1"));
    assert_eq!(rows[0].get(1), None);

    // With an explicit column list PG requires an exact match.
    let err = client
        .simple_query("INSERT INTO t (a, b) VALUES (2)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::SYNTAX_ERROR
    );

    Ok(())
}

#[tokio::test]
async fn quoted_literals_coerce_to_column_types() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer, big bigint, ok boolean)")
        .await?;
    client
        .simple_query("INSERT INTO t VALUES ('42', '9000000000', 'yes')")
        .await?;

    let messages = client.simple_query("SELECT * FROM t").await?;
    let rows = rows(&messages);
    assert_eq!(rows[0].get(0), Some("42"));
    assert_eq!(rows[0].get(1), Some("9000000000"));
    assert_eq!(rows[0].get(2), Some("t"));

    let err = client
        .simple_query("INSERT INTO t (id) VALUES ('abc')")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::INVALID_TEXT_REPRESENTATION
    );

    Ok(())
}

#[tokio::test]
async fn create_table_if_not_exists_is_idempotent() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id integer)").await?;
    client
        .simple_query("CREATE TABLE IF NOT EXISTS t (id integer)")
        .await?;

    // Without IF NOT EXISTS the duplicate still errors.
    let err = client
        .simple_query("CREATE TABLE t (id integer)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::DUPLICATE_TABLE
    );

    Ok(())
}

#[tokio::test]
async fn temp_table_shadows_permanent_within_the_session_only() -> anyhow::Result<()> {
    let port = spawn_server().await;

    // A permanent table lives in the shared engine.
    let a = connect(port).await;
    a.simple_query("CREATE TABLE t (v integer)").await?;
    a.simple_query("INSERT INTO t VALUES (1)").await?;

    // A same-named TEMP table shadows it for this session — no 42P07, and all
    // DML now resolves to the temp table.
    a.simple_query("CREATE TEMP TABLE t (v integer)").await?;
    a.simple_query("INSERT INTO t VALUES (2), (3)").await?;

    let temp_msgs = a.simple_query("SELECT v FROM t").await?;
    let temp_rows = rows(&temp_msgs);
    assert_eq!(temp_rows.len(), 2, "SELECT hits the temp table");
    assert_eq!(temp_rows[0].get(0), Some("2"));
    assert_eq!(temp_rows[1].get(0), Some("3"));

    // UPDATE and TRUNCATE hit the temp table too, never the shadowed permanent one.
    let msgs = a.simple_query("UPDATE t SET v = v * -1").await?;
    assert_eq!(command_count(&msgs), Some(2), "UPDATE hits the 2 temp rows");
    a.simple_query("TRUNCATE t").await?;
    assert_eq!(
        rows(&a.simple_query("SELECT v FROM t").await?).len(),
        0,
        "TRUNCATE emptied the temp table"
    );

    // A second, fresh session has no temp store: it sees only the permanent
    // table (still holding its original row), proving the temp table is
    // session-scoped and left the permanent one untouched.
    let b = connect(port).await;
    let perm_msgs = b.simple_query("SELECT v FROM t").await?;
    let perm_rows = rows(&perm_msgs);
    assert_eq!(perm_rows.len(), 1);
    assert_eq!(
        perm_rows[0].get(0),
        Some("1"),
        "the permanent table was never shadowed for this session"
    );

    Ok(())
}

#[tokio::test]
async fn unenforceable_ddl_is_rejected() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    for sql in [
        // Clauses we can't honor must be rejected, not silently dropped: CTAS
        // would discard the SELECT, and ON COMMIT needs the M2 txn engine.
        "CREATE TABLE c AS SELECT 1 AS x",
        "CREATE TEMP TABLE c AS SELECT 1 AS x",
        "CREATE TEMP TABLE c (x int) ON COMMIT DROP",
        "CREATE TEMP TABLE c (x int) ON COMMIT DELETE ROWS",
    ] {
        let err = client.simple_query(sql).await.unwrap_err();
        assert_eq!(
            err.as_db_error()
                .context("database error details are missing")?
                .code(),
            &tokio_postgres::error::SqlState::FEATURE_NOT_SUPPORTED,
            "{sql}"
        );
    }

    Ok(())
}

#[tokio::test]
async fn defaults_constraints_and_semantic_indexes() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query(
            "CREATE TABLE c (\
                id integer PRIMARY KEY, \
                u integer UNIQUE, \
                n integer CONSTRAINT c_n_nn NOT NULL, \
                d integer DEFAULT (1 + 2))",
        )
        .await?;

    client
        .simple_query("INSERT INTO c (id, n) VALUES (1, 10)")
        .await?;
    client
        .simple_query(
            "INSERT INTO c (id, u, n, d) VALUES \
             (2, NULL, 20, DEFAULT), (3, NULL, 30, 7)",
        )
        .await?;
    client
        .simple_query("UPDATE c SET d = DEFAULT WHERE id = 3")
        .await?;
    let value_messages = client
        .simple_query("SELECT id, d FROM c ORDER BY id")
        .await?;
    let values = rows(&value_messages);
    assert_eq!(values.len(), 3);
    assert!(values.iter().all(|row| row.get(1) == Some("3")));

    let update_duplicate = client.simple_query("UPDATE c SET u = 9").await.unwrap_err();
    assert_eq!(
        update_duplicate
            .as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::UNIQUE_VIOLATION
    );
    let unchanged_messages = client
        .simple_query("SELECT count(*) FROM c WHERE u IS NULL")
        .await?;
    assert_eq!(rows(&unchanged_messages)[0].get(0), Some("3"));

    let duplicate = client
        .simple_query("INSERT INTO c (id, u, n) VALUES (4, 9, 40), (5, 9, 50)")
        .await
        .unwrap_err();
    assert_eq!(
        duplicate
            .as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::UNIQUE_VIOLATION
    );
    assert_eq!(
        rows(&client.simple_query("SELECT id FROM c").await?).len(),
        3,
        "failed multi-row INSERT is atomic"
    );

    let not_null = client
        .simple_query("INSERT INTO c (id, n) VALUES (4, NULL)")
        .await
        .unwrap_err();
    assert_eq!(
        not_null
            .as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::NOT_NULL_VIOLATION
    );

    client
        .simple_query(
            "CREATE TABLE defaults_only (a integer DEFAULT (2 * 3), b text DEFAULT upper('x'))",
        )
        .await?;
    client
        .simple_query("INSERT INTO defaults_only DEFAULT VALUES")
        .await?;
    let default_messages = client
        .simple_query("SELECT a, b FROM defaults_only")
        .await?;
    let default_rows = rows(&default_messages);
    assert_eq!(default_rows[0].get(0), Some("6"));
    assert_eq!(default_rows[0].get(1), Some("X"));

    client
        .simple_query("CREATE TABLE null_equal (a integer, UNIQUE NULLS NOT DISTINCT (a))")
        .await?;
    client
        .simple_query("INSERT INTO null_equal VALUES (NULL)")
        .await?;
    let null_duplicate = client
        .simple_query("INSERT INTO null_equal VALUES (NULL)")
        .await
        .unwrap_err();
    assert_eq!(
        null_duplicate
            .as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::UNIQUE_VIOLATION
    );

    client
        .simple_query("CREATE TABLE ix (a integer, b text)")
        .await?;
    client
        .simple_query("INSERT INTO ix VALUES (1, 'x'), (1, 'y')")
        .await?;
    let build = client
        .simple_query("CREATE UNIQUE INDEX ix_a_idx ON ix(a)")
        .await
        .unwrap_err();
    assert_eq!(
        build
            .as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::UNIQUE_VIOLATION
    );
    client
        .simple_query("CREATE INDEX ix_b_idx ON ix(b)")
        .await?;
    client
        .simple_query("CREATE INDEX IF NOT EXISTS ix_b_idx ON ix(b)")
        .await?;

    let column_messages = client
        .simple_query(
            "SELECT column_name, column_default, is_nullable \
             FROM information_schema.columns \
             WHERE table_name = 'c' ORDER BY column_name",
        )
        .await?;
    let columns = rows(&column_messages);
    let d = columns
        .iter()
        .find(|row| row.get(0) == Some("d"))
        .context("column d is missing")?;
    assert_eq!(d.get(1), Some("(1 + 2)"));
    let id = columns
        .iter()
        .find(|row| row.get(0) == Some("id"))
        .context("column id is missing")?;
    assert_eq!(id.get(2), Some("NO"));

    let index_messages = client
        .simple_query("SELECT indexrelid, indisunique FROM pg_index ORDER BY indexrelid")
        .await?;
    let indexes = rows(&index_messages);
    assert_eq!(
        indexes.len(),
        4,
        "PK, UNIQUE constraints, and explicit index are reflected"
    );
    let constraint_messages = client
        .simple_query("SELECT count(*) FROM pg_constraint")
        .await?;
    assert_eq!(rows(&constraint_messages)[0].get(0), Some("4"));
    let default_messages = client
        .simple_query("SELECT count(*) FROM pg_attrdef")
        .await?;
    assert_eq!(rows(&default_messages)[0].get(0), Some("3"));
    let class_messages = client
        .simple_query("SELECT count(*) FROM pg_class WHERE relkind = 'i'")
        .await?;
    assert_eq!(rows(&class_messages)[0].get(0), Some("4"));

    Ok(())
}

/// The numeric suffix of the first CommandComplete tag (`UPDATE 2` → 2).
fn command_count(messages: &[SimpleQueryMessage]) -> Option<u64> {
    messages.iter().find_map(|m| match m {
        SimpleQueryMessage::CommandComplete(n) => Some(*n),
        _ => None,
    })
}

#[tokio::test]
async fn full_crud_cycle_with_where() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE crabs (id integer, name text)")
        .await?;
    client
        .simple_query("INSERT INTO crabs VALUES (1, 'ferris'), (2, 'hermit'), (3, 'king')")
        .await?;

    let messages = client
        .simple_query("SELECT name FROM crabs WHERE id > 1")
        .await?;
    let selected = rows(&messages);
    assert_eq!(selected.len(), 2);
    assert_eq!(selected[0].get(0), Some("hermit"));

    let messages = client
        .simple_query("UPDATE crabs SET name = 'crab' WHERE id > 1")
        .await?;
    assert_eq!(command_count(&messages), Some(2), "tag must be UPDATE 2");

    let messages = client
        .simple_query("DELETE FROM crabs WHERE name = 'crab'")
        .await?;
    assert_eq!(command_count(&messages), Some(2), "tag must be DELETE 2");

    let messages = client.simple_query("SELECT * FROM crabs").await?;
    let remaining = rows(&messages);
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].get(1), Some("ferris"));

    Ok(())
}

/// The count of visible rows in `t`.
async fn row_count(client: &tokio_postgres::Client, table: &str) -> usize {
    let messages = match client.simple_query(&format!("SELECT * FROM {table}")).await {
        Ok(messages) => messages,
        Err(error) => panic!("failed to count rows in test table `{table}`: {error}"),
    };
    rows(&messages).len()
}

#[tokio::test]
async fn rollback_undoes_inserts() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id integer)").await?;
    client.simple_query("BEGIN").await?;
    client.simple_query("INSERT INTO t VALUES (1), (2)").await?;
    // The transaction sees its own uncommitted inserts.
    assert_eq!(row_count(&client, "t").await, 2);
    client.simple_query("ROLLBACK").await?;
    // After rollback the rows are gone — real MVCC undo, not just control flow.
    assert_eq!(row_count(&client, "t").await, 0);

    Ok(())
}

#[tokio::test]
async fn rollback_restores_deleted_and_updated_rows() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer, label text)")
        .await?;
    client
        .simple_query("INSERT INTO t VALUES (1, 'a'), (2, 'b')")
        .await?;

    client.simple_query("BEGIN").await?;
    client.simple_query("DELETE FROM t WHERE id = 1").await?;
    client
        .simple_query("UPDATE t SET label = 'B' WHERE id = 2")
        .await?;
    // Inside the block the changes are visible: id=1 gone, id=2 now 'B'.
    let msgs = client
        .simple_query("SELECT label FROM t ORDER BY 1")
        .await?;
    let seen: Vec<_> = rows(&msgs)
        .iter()
        .map(|r| {
            r.get(0)
                .context("label column is missing")
                .map(str::to_string)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    assert_eq!(seen, ["B"]);

    client.simple_query("ROLLBACK").await?;
    // Both the delete and the update are undone.
    let msgs = client
        .simple_query("SELECT label FROM t ORDER BY 1")
        .await?;
    let restored: Vec<_> = rows(&msgs)
        .iter()
        .map(|r| {
            r.get(0)
                .context("label column is missing")
                .map(str::to_string)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    assert_eq!(restored, ["a", "b"]);

    Ok(())
}

#[tokio::test]
async fn commit_persists_changes() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id integer)").await?;
    client.simple_query("BEGIN").await?;
    client.simple_query("INSERT INTO t VALUES (7)").await?;
    client.simple_query("COMMIT").await?;
    let msgs = client.simple_query("SELECT id FROM t").await?;
    let r = rows(&msgs);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].get(0), Some("7"));

    Ok(())
}

#[tokio::test]
async fn uncommitted_changes_are_invisible_to_other_sessions() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let writer = connect(port).await;
    let reader = connect(port).await;
    writer.simple_query("CREATE TABLE t (id integer)").await?;
    writer.simple_query("BEGIN").await?;
    writer.simple_query("INSERT INTO t VALUES (1)").await?;
    // The writer sees its own row; a concurrent session does not.
    assert_eq!(row_count(&writer, "t").await, 1);
    assert_eq!(row_count(&reader, "t").await, 0);
    writer.simple_query("COMMIT").await?;
    // Once committed, the other session sees it.
    assert_eq!(row_count(&reader, "t").await, 1);

    Ok(())
}

#[tokio::test]
async fn disconnect_mid_block_aborts_and_frees_the_row() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let a = connect(port).await;
    a.simple_query("CREATE TABLE t (id integer)").await?;
    a.simple_query("INSERT INTO t VALUES (1)").await?;

    // B opens a block and updates the row (allocating an XID and stamping the
    // old version's xmax), then disconnects without COMMIT/ROLLBACK.
    let b = connect(port).await;
    b.simple_query("BEGIN").await?;
    assert_eq!(
        command_count(&b.simple_query("UPDATE t SET id = 2").await?),
        Some(1)
    );
    drop(b);
    // Give the server a moment to observe the disconnect and abort B's block.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // C can still update the row: B's abort-on-drop made the original version
    // live again. Without the fix, B's XID stays in-flight, the row is not
    // is_live, and this reports UPDATE 0.
    let c = connect(port).await;
    let msg = c.simple_query("UPDATE t SET id = 3").await?;
    assert_eq!(
        command_count(&msg),
        Some(1),
        "row must be updatable after B's abandoned block aborts on disconnect"
    );
    let sel = c.simple_query("SELECT id FROM t").await?;
    assert_eq!(rows(&sel)[0].get(0), Some("3"));

    Ok(())
}

#[tokio::test]
async fn repeatable_read_freezes_the_snapshot() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let a = connect(port).await;
    let b = connect(port).await;
    a.simple_query("CREATE TABLE t (id integer)").await?;
    a.simple_query("INSERT INTO t VALUES (1)").await?;

    a.simple_query("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await?;
    // The first read freezes the block's snapshot (before B's insert).
    assert_eq!(row_count(&a, "t").await, 1);
    b.simple_query("INSERT INTO t VALUES (2)").await?;
    // RR reuses that snapshot: B's later commit stays invisible to A.
    assert_eq!(row_count(&a, "t").await, 1);
    a.simple_query("COMMIT").await?;
    // A fresh block sees B's committed row.
    assert_eq!(row_count(&a, "t").await, 2);

    Ok(())
}

#[tokio::test]
async fn read_committed_sees_concurrent_commits() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let a = connect(port).await;
    let b = connect(port).await;
    a.simple_query("CREATE TABLE t (id integer)").await?;
    a.simple_query("INSERT INTO t VALUES (1)").await?;

    a.simple_query("BEGIN ISOLATION LEVEL READ COMMITTED")
        .await?;
    assert_eq!(row_count(&a, "t").await, 1);
    b.simple_query("INSERT INTO t VALUES (2)").await?;
    // RC takes a fresh snapshot per statement, so A now sees B's committed row.
    assert_eq!(row_count(&a, "t").await, 2);
    a.simple_query("COMMIT").await?;

    Ok(())
}

#[tokio::test]
async fn set_transaction_sets_isolation_before_any_query() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let a = connect(port).await;
    let b = connect(port).await;
    a.simple_query("CREATE TABLE t (id integer)").await?;
    a.simple_query("INSERT INTO t VALUES (1)").await?;

    a.simple_query("BEGIN").await?;
    a.simple_query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .await?;
    assert_eq!(row_count(&a, "t").await, 1); // freezes the snapshot
    b.simple_query("INSERT INTO t VALUES (2)").await?;
    assert_eq!(row_count(&a, "t").await, 1); // RR: still frozen
    a.simple_query("COMMIT").await?;

    Ok(())
}

#[tokio::test]
async fn set_transaction_after_a_query_errors_25001() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id integer)").await?;
    client.simple_query("BEGIN").await?;
    client.simple_query("SELECT * FROM t").await?;
    let err = client
        .simple_query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .await
        .unwrap_err();
    let db = err.as_db_error().expect("should be a server error");
    assert_eq!(
        db.code(),
        &tokio_postgres::error::SqlState::ACTIVE_SQL_TRANSACTION
    );
    assert_eq!(
        db.message(),
        "SET TRANSACTION ISOLATION LEVEL must be called before any query"
    );
    client.simple_query("ROLLBACK").await?;

    Ok(())
}

#[tokio::test]
async fn set_transaction_outside_a_block_warns_but_succeeds() {
    // PG warns and still completes with tag SET (no error, session stays idle) —
    // it does not raise 25P01. Checked over the raw wire so the NOTICE frame is
    // visible.
    let mut socket = raw_session(spawn_server().await).await;
    let msgs = simple_query_raw(&mut socket, "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE").await;
    let notice = msgs
        .iter()
        .find(|(tag, _)| *tag == b'N')
        .expect("a warning is expected");
    assert_eq!(fields(notice).code(), "25P01");
    assert!(!msgs.iter().any(|(tag, _)| *tag == b'E'), "must not error");
    assert_eq!(command_tags(&msgs), ["SET"]);
    assert_eq!(ready_status(&msgs), b'I', "session stays idle");
}

#[tokio::test]
async fn set_transaction_read_only_after_a_query_is_allowed() -> anyhow::Result<()> {
    // Only ISOLATION LEVEL is snapshot-gated; READ ONLY can be set any time.
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id integer)").await?;
    client.simple_query("BEGIN").await?;
    client.simple_query("SELECT * FROM t").await?;
    client
        .simple_query("SET TRANSACTION READ ONLY")
        .await
        .expect("SET TRANSACTION READ ONLY is allowed after a query");
    // It took effect: a write is now rejected.
    let err = client
        .simple_query("INSERT INTO t VALUES (1)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("server error").code(),
        &tokio_postgres::error::SqlState::READ_ONLY_SQL_TRANSACTION
    );
    client.simple_query("ROLLBACK").await?;

    Ok(())
}

#[tokio::test]
async fn set_transaction_isolation_after_ddl_errors_25001() -> anyhow::Result<()> {
    // A DDL statement takes a snapshot, so a later ISOLATION LEVEL change is
    // rejected just as it would be after a SELECT.
    let client = connect(spawn_server().await).await;
    client.simple_query("BEGIN").await?;
    client.simple_query("CREATE TABLE t (id integer)").await?;
    let err = client
        .simple_query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("server error").code(),
        &tokio_postgres::error::SqlState::ACTIVE_SQL_TRANSACTION
    );
    client.simple_query("ROLLBACK").await?;

    Ok(())
}

#[tokio::test]
async fn set_guc_to_default_resets_it() {
    let client = connect(spawn_server().await).await;
    // = DEFAULT resets rather than erroring on the "DEFAULT" token.
    client
        .simple_query("SET default_transaction_isolation = DEFAULT")
        .await
        .expect("= DEFAULT resets default_transaction_isolation");
    client
        .simple_query("SET default_transaction_read_only = DEFAULT")
        .await
        .expect("= DEFAULT resets default_transaction_read_only");
    client
        .simple_query("SET extra_float_digits = DEFAULT")
        .await
        .expect("= DEFAULT resets extra_float_digits");
}

#[tokio::test]
async fn read_only_transaction_rejects_writes_25006() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id integer)").await?;
    client.simple_query("BEGIN READ ONLY").await?;
    let err = client
        .simple_query("INSERT INTO t VALUES (1)")
        .await
        .unwrap_err();
    let db = err.as_db_error().expect("should be a server error");
    assert_eq!(
        db.code(),
        &tokio_postgres::error::SqlState::READ_ONLY_SQL_TRANSACTION
    );
    assert_eq!(
        db.message(),
        "cannot execute INSERT in a read-only transaction"
    );
    client.simple_query("ROLLBACK").await?;
    // Reads are still allowed in a read-only block.
    client.simple_query("BEGIN READ ONLY").await?;
    assert_eq!(row_count(&client, "t").await, 0);
    client.simple_query("COMMIT").await?;

    Ok(())
}

#[tokio::test]
async fn read_only_transaction_rejects_ddl_before_resolution() -> anyhow::Result<()> {
    // DDL is rejected up front (25006) — even for a missing target, the
    // read-only error precedes the undefined-object error, as in PG.
    let client = connect(spawn_server().await).await;
    client.simple_query("BEGIN READ ONLY").await?;
    let err = client
        .simple_query("CREATE TABLE t (id integer)")
        .await
        .unwrap_err();
    let db = err.as_db_error().expect("server error");
    assert_eq!(
        db.code(),
        &tokio_postgres::error::SqlState::READ_ONLY_SQL_TRANSACTION
    );
    assert_eq!(
        db.message(),
        "cannot execute CREATE TABLE in a read-only transaction"
    );
    client.simple_query("ROLLBACK").await?;

    // DROP of a missing table also reports 25006, not 42P01.
    client.simple_query("BEGIN READ ONLY").await?;
    let err = client.simple_query("DROP TABLE nope").await.unwrap_err();
    assert_eq!(
        err.as_db_error().expect("server error").code(),
        &tokio_postgres::error::SqlState::READ_ONLY_SQL_TRANSACTION
    );
    client.simple_query("ROLLBACK").await?;

    Ok(())
}

#[tokio::test]
async fn session_default_isolation_applies_to_new_blocks() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let a = connect(port).await;
    let b = connect(port).await;
    a.simple_query("CREATE TABLE t (id integer)").await?;
    a.simple_query("INSERT INTO t VALUES (1)").await?;

    // SET SESSION CHARACTERISTICS makes a subsequent bare BEGIN block RR.
    a.simple_query("SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .await?;
    a.simple_query("BEGIN").await?;
    assert_eq!(row_count(&a, "t").await, 1); // freeze
    b.simple_query("INSERT INTO t VALUES (2)").await?;
    assert_eq!(row_count(&a, "t").await, 1); // RR freeze from the session default
    a.simple_query("COMMIT").await?;

    // The plain GUC spelling switches the default back to READ COMMITTED.
    a.simple_query("SET default_transaction_isolation = 'read committed'")
        .await?;
    a.simple_query("BEGIN").await?;
    assert_eq!(row_count(&a, "t").await, 2);
    b.simple_query("INSERT INTO t VALUES (3)").await?;
    assert_eq!(row_count(&a, "t").await, 3); // RC sees the concurrent commit
    a.simple_query("COMMIT").await?;

    Ok(())
}

#[tokio::test]
async fn default_read_only_guc_blocks_autocommit_writes() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id integer)").await?;
    client
        .simple_query("SET default_transaction_read_only = on")
        .await?;
    let err = client
        .simple_query("INSERT INTO t VALUES (1)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("server error").code(),
        &tokio_postgres::error::SqlState::READ_ONLY_SQL_TRANSACTION
    );
    // Turning it back off restores writes.
    client
        .simple_query("SET default_transaction_read_only = off")
        .await?;
    client.simple_query("INSERT INTO t VALUES (1)").await?;
    assert_eq!(row_count(&client, "t").await, 1);

    Ok(())
}

#[tokio::test]
async fn update_and_delete_without_where_hit_all_rows() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id integer)").await?;
    client
        .simple_query("INSERT INTO t VALUES (1), (2), (3)")
        .await?;

    let messages = client.simple_query("UPDATE t SET id = 0").await?;
    assert_eq!(command_count(&messages), Some(3));

    let messages = client.simple_query("DELETE FROM t").await?;
    assert_eq!(command_count(&messages), Some(3));
    let messages = client.simple_query("SELECT * FROM t").await?;
    assert_eq!(rows(&messages).len(), 0);

    Ok(())
}

#[tokio::test]
async fn null_rows_do_not_match_comparisons() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer, v integer)")
        .await?;
    client
        .simple_query("INSERT INTO t VALUES (1, 10), (2, NULL)")
        .await?;

    // Neither = nor <> matches a NULL: only IS NULL does.
    for (sql, expected) in [
        ("SELECT id FROM t WHERE v = 10", 1),
        ("SELECT id FROM t WHERE v <> 10", 0),
        ("SELECT id FROM t WHERE v IS NULL", 1),
        ("SELECT id FROM t WHERE v IS NOT NULL", 1),
    ] {
        let messages = client.simple_query(sql).await?;
        assert_eq!(rows(&messages).len(), expected, "{sql}");
    }

    Ok(())
}

#[tokio::test]
async fn expressions_in_select_list() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id integer)").await?;
    client.simple_query("INSERT INTO t VALUES (41)").await?;

    let messages = client
        .simple_query("SELECT id + 1, id * 2 AS double FROM t")
        .await?;
    let rows = rows(&messages);
    assert_eq!(rows[0].columns()[0].name(), "?column?");
    assert_eq!(rows[0].columns()[1].name(), "double");
    assert_eq!(rows[0].get(0), Some("42"));
    assert_eq!(rows[0].get(1), Some("82"));

    Ok(())
}

#[tokio::test]
async fn update_set_expressions_see_the_old_row() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (a integer, b integer)")
        .await?;
    client.simple_query("INSERT INTO t VALUES (1, 2)").await?;

    // Both SET expressions evaluate against the OLD row: this swaps.
    client.simple_query("UPDATE t SET a = b, b = a").await?;
    let messages = client.simple_query("SELECT a, b FROM t").await?;
    let rows = rows(&messages);
    assert_eq!(rows[0].get(0), Some("2"));
    assert_eq!(rows[0].get(1), Some("1"));

    Ok(())
}

#[tokio::test]
async fn failing_update_leaves_no_rows_modified() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer, v integer)")
        .await?;
    client
        .simple_query("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)")
        .await?;

    // Fails on the id=2 row after id=1 evaluated fine.
    let err = client
        .simple_query("UPDATE t SET v = v / (id - 2)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::DIVISION_BY_ZERO
    );

    let messages = client.simple_query("SELECT v FROM t WHERE id = 1").await?;
    assert_eq!(
        rows(&messages)[0].get(0),
        Some("10"),
        "statement must be atomic"
    );

    Ok(())
}

#[tokio::test]
async fn mid_stream_error_aborts_remaining_statements() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id integer)").await?;
    client.simple_query("INSERT INTO t VALUES (2), (0)").await?;

    // The error surfaces mid-stream, after RowDescription (and possibly the
    // first row) went out; the trailing INSERT must not run.
    let err = client
        .simple_query("SELECT 10 / id FROM t; INSERT INTO t VALUES (7)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::DIVISION_BY_ZERO
    );

    let messages = client.simple_query("SELECT * FROM t").await?;
    assert_eq!(rows(&messages).len(), 2, "aborted INSERT must not run");

    Ok(())
}

#[tokio::test]
async fn expression_type_errors_report_pg_sqlstates() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer, name text)")
        .await?;
    client
        .simple_query("INSERT INTO t VALUES (2147483647, 'x')")
        .await?;

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
        let db_err = err
            .as_db_error()
            .context("database error details are missing")?;
        assert_eq!(db_err.code(), &code, "{sql}");
        assert_eq!(db_err.message(), message, "{sql}");
    }

    // Runtime overflow through UPDATE arithmetic.
    let err = client
        .simple_query("UPDATE t SET id = id + 1")
        .await
        .unwrap_err();
    let db_err = err
        .as_db_error()
        .context("database error details are missing")?;
    assert_eq!(
        db_err.code(),
        &tokio_postgres::error::SqlState::NUMERIC_VALUE_OUT_OF_RANGE
    );
    assert_eq!(db_err.message(), "integer out of range");

    Ok(())
}

#[tokio::test]
async fn insert_source_clauses_and_ragged_values_are_rejected() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (a integer, b integer)")
        .await?;

    // The INSERT source is a full query in PG (`VALUES ... LIMIT 1` inserts
    // one row); until that executes, it must be rejected, not ignored.
    for sql in [
        "INSERT INTO t (a) VALUES (1), (2) LIMIT 1",
        "INSERT INTO t (a) VALUES (1), (2) ORDER BY 1",
    ] {
        let err = client.simple_query(sql).await.unwrap_err();
        assert_eq!(
            err.as_db_error()
                .context("database error details are missing")?
                .code(),
            &tokio_postgres::error::SqlState::FEATURE_NOT_SUPPORTED,
            "{sql}"
        );
    }

    let err = client
        .simple_query("INSERT INTO t VALUES (1, 2), (3)")
        .await
        .unwrap_err();
    let db_err = err
        .as_db_error()
        .context("database error details are missing")?;
    assert_eq!(
        db_err.code(),
        &tokio_postgres::error::SqlState::SYNTAX_ERROR
    );
    assert_eq!(db_err.message(), "VALUES lists must all be the same length");

    let messages = client.simple_query("SELECT * FROM t").await?;
    assert_eq!(
        rows(&messages).len(),
        0,
        "no rejected INSERT may leave rows"
    );

    Ok(())
}

#[tokio::test]
async fn constant_update_overflow_errors_even_with_no_matching_rows() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id integer)").await?;

    // PG const-folds the cast at plan time: the empty table must not turn
    // the error into `UPDATE 0`.
    let err = client
        .simple_query("UPDATE t SET id = 2147483648")
        .await
        .unwrap_err();
    let db_err = err
        .as_db_error()
        .context("database error details are missing")?;
    assert_eq!(
        db_err.code(),
        &tokio_postgres::error::SqlState::NUMERIC_VALUE_OUT_OF_RANGE
    );
    assert_eq!(db_err.message(), "integer out of range");

    Ok(())
}

#[tokio::test]
async fn integer_literals_distinguish_out_of_range_from_malformed() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer, ok boolean)")
        .await?;
    client
        .simple_query("INSERT INTO t VALUES (1, 'tru'), (2, 'of')")
        .await?;

    // PG bool input accepts unambiguous prefixes.
    let messages = client.simple_query("SELECT id FROM t WHERE ok").await?;
    let matched = rows(&messages);
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].get(0), Some("1"));

    let err = client
        .simple_query("SELECT id FROM t WHERE id = '3000000000'")
        .await
        .unwrap_err();
    let db_err = err
        .as_db_error()
        .context("database error details are missing")?;
    assert_eq!(
        db_err.code(),
        &tokio_postgres::error::SqlState::NUMERIC_VALUE_OUT_OF_RANGE
    );
    assert_eq!(
        db_err.message(),
        "value \"3000000000\" is out of range for type integer"
    );

    Ok(())
}

async fn read_backend_message(socket: &mut tokio::net::TcpStream) -> (u8, Vec<u8>) {
    let tag = match socket.read_u8().await {
        Ok(tag) => tag,
        Err(error) => panic!("failed to read backend message tag: {error}"),
    };
    let len = match socket.read_i32().await {
        Ok(len) => len as usize,
        Err(error) => panic!("failed to read backend message length: {error}"),
    };
    let mut body = vec![0u8; len - 4];
    if let Err(error) = socket.read_exact(&mut body).await {
        panic!("failed to read backend message body: {error}");
    }
    (tag, body)
}

/// Cleartext startup on a raw socket, draining until ReadyForQuery.
async fn raw_session(port: u16) -> tokio::net::TcpStream {
    let mut socket = match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
        Ok(socket) => socket,
        Err(error) => panic!("failed to connect raw test session: {error}"),
    };
    let mut body = Vec::new();
    body.extend_from_slice(&196608i32.to_be_bytes());
    body.extend_from_slice(b"user\0postgres\0\0");
    if let Err(error) = socket
        .write_all(&((body.len() + 4) as i32).to_be_bytes())
        .await
    {
        panic!("failed to write startup message length: {error}");
    }
    if let Err(error) = socket.write_all(&body).await {
        panic!("failed to write startup message body: {error}");
    }
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

/// Collect every backend `(tag, body)` up to and including the terminating
/// ReadyForQuery, after the caller has written an extended-query batch.
async fn read_until_ready(socket: &mut tokio::net::TcpStream) -> Vec<(u8, Vec<u8>)> {
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

/// A valid Parse/Bind/Describe/Execute/Sync batch runs end to end: ParseComplete,
/// BindComplete, RowDescription (from Describe portal), the row, CommandComplete,
/// and one ReadyForQuery.
#[tokio::test]
async fn extended_protocol_runs_a_full_batch() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let mut socket = raw_session(port).await;

    let mut batch = Vec::new();
    batch.extend(frontend_message(b'P', b"\0SELECT 1\0\x00\x00")); // Parse (no params)
    batch.extend(frontend_message(b'B', b"\0\0\x00\x00\x00\x00\x00\x00")); // Bind
    batch.extend(frontend_message(b'D', b"P\0")); // Describe portal
    batch.extend(frontend_message(b'E', b"\0\x00\x00\x00\x00")); // Execute (unlimited)
    batch.extend(frontend_message(b'S', b"")); // Sync
    socket.write_all(&batch).await?;

    let msgs = read_until_ready(&mut socket).await;
    let tags: Vec<u8> = msgs.iter().map(|(t, _)| *t).collect();
    assert_eq!(tags, [b'1', b'2', b'T', b'D', b'C', b'Z']);
    // The DataRow carries the single text column "1".
    let data = &msgs
        .iter()
        .find(|(t, _)| *t == b'D')
        .context("DataRow is missing")?
        .1;
    assert_eq!(&data[..], &[0, 1, 0, 0, 0, 1, b'1']); // 1 col, len 1, "1"
    assert_eq!(msgs.last().context("ReadyForQuery is missing")?.1, [b'I']);

    Ok(())
}

/// A Bind whose result-format count is neither 0, 1, nor the column count must
/// be rejected (08P01) instead of panicking on an out-of-bounds format index.
#[tokio::test]
async fn bind_rejects_mismatched_format_count() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let mut socket = raw_session(port).await;

    let mut batch = Vec::new();
    batch.extend(frontend_message(b'P', b"\0SELECT 1\0\x00\x00")); // Parse: 1 column
    // Bind portal "" stmt "": 0 param formats, 0 params, 2 result formats.
    batch.extend(frontend_message(
        b'B',
        b"\0\0\x00\x00\x00\x00\x00\x02\x00\x00\x00\x00",
    ));
    batch.extend(frontend_message(b'S', b""));
    socket.write_all(&batch).await?;

    let msgs = read_until_ready(&mut socket).await;
    let tags: Vec<u8> = msgs.iter().map(|(t, _)| *t).collect();
    assert_eq!(tags, [b'1', b'E', b'Z'], "ParseComplete, error, RFQ");
    // 08P01 protocol_violation, not a crashed connection.
    let err = &msgs
        .iter()
        .find(|(t, _)| *t == b'E')
        .context("ErrorResponse is missing")?
        .1;
    assert!(err.windows(6).any(|w| w == b"08P01\0"));

    Ok(())
}

/// Re-Parsing an existing *named* prepared statement is 42P05; the unnamed
/// statement is silently replaced.
#[tokio::test]
async fn reparse_named_statement_errors_42p05() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let mut socket = raw_session(port).await;

    let mut batch = Vec::new();
    batch.extend(frontend_message(b'P', b"s\0SELECT 1\0\x00\x00")); // Parse "s"
    batch.extend(frontend_message(b'P', b"s\0SELECT 2\0\x00\x00")); // Parse "s" again
    batch.extend(frontend_message(b'S', b""));
    socket.write_all(&batch).await?;

    let msgs = read_until_ready(&mut socket).await;
    let tags: Vec<u8> = msgs.iter().map(|(t, _)| *t).collect();
    assert_eq!(tags, [b'1', b'E', b'Z']);
    let err = &msgs
        .iter()
        .find(|(t, _)| *t == b'E')
        .context("ErrorResponse is missing")?
        .1;
    assert!(err.windows(6).any(|w| w == b"42P05\0"));

    Ok(())
}

/// An out-of-range parameter number must be rejected, not resized into a
/// multi-gigabyte allocation.
#[tokio::test]
async fn huge_parameter_number_is_rejected_not_allocated() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    let err = client
        .prepare("SELECT $2000000000::int4")
        .await
        .expect_err("parameter number is out of range");
    assert_eq!(err.code().expect("has SQLSTATE").code(), "54000");
    // Connection still usable.
    let row = client.query_one("SELECT $1::int4 AS v", &[&7i32]).await?;
    assert_eq!(row.get::<_, i32>("v"), 7);

    Ok(())
}

/// A failed extended-protocol batch must produce exactly one ErrorResponse and
/// one ReadyForQuery (at Sync) — per-message replies desync drivers. Here Bind
/// names a prepared statement that was never created.
#[tokio::test]
async fn extended_protocol_errors_once_and_recovers_at_sync() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let mut socket = raw_session(port).await;

    let mut batch = Vec::new();
    batch.extend(frontend_message(b'B', b"\0nope\0\x00\x00\x00\x00\x00\x00")); // Bind to "nope"
    batch.extend(frontend_message(b'D', b"P\0")); // Describe portal (skipped)
    batch.extend(frontend_message(b'E', b"\0\x00\x00\x00\x00")); // Execute (skipped)
    batch.extend(frontend_message(b'S', b"")); // Sync
    socket.write_all(&batch).await?;

    let (tag, body) = read_backend_message(&mut socket).await;
    assert_eq!(tag, b'E', "first reply must be a single ErrorResponse");
    // SQLSTATE 26000 (invalid_sql_statement_name).
    assert!(body.windows(6).any(|w| w == b"26000\0"));
    let (tag, body) = read_backend_message(&mut socket).await;
    assert_eq!(tag, b'Z', "Describe/Execute must be skipped until Sync");
    assert_eq!(body, [b'I']);

    // The session must remain usable for simple queries afterwards.
    socket
        .write_all(&frontend_message(b'Q', b"SELECT 1\0"))
        .await?;
    let tags: Vec<u8> = read_until_ready(&mut socket)
        .await
        .iter()
        .map(|(t, _)| *t)
        .collect();
    assert_eq!(tags, [b'T', b'D', b'C', b'Z']);

    Ok(())
}

/// Describe on a prepared statement reports its parameter types then the result
/// columns; Close then drops it so a later Describe errors (and recovers).
#[tokio::test]
async fn describe_statement_reports_params_then_close_drops_it() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let mut socket = raw_session(port).await;

    // Parse `SELECT $1::int4` as statement "s" (no declared types — the `::int4`
    // cast forces inference); Describe the statement; Sync.
    let mut batch = Vec::new();
    batch.extend(frontend_message(b'P', b"s\0SELECT $1::int4\0\x00\x00"));
    batch.extend(frontend_message(b'D', b"Ss\0")); // Describe statement "s"
    batch.extend(frontend_message(b'S', b""));
    socket.write_all(&batch).await?;

    let msgs = read_until_ready(&mut socket).await;
    let tags: Vec<u8> = msgs.iter().map(|(t, _)| *t).collect();
    // ParseComplete, ParameterDescription, RowDescription, ReadyForQuery.
    assert_eq!(tags, [b'1', b't', b'T', b'Z']);
    // ParameterDescription: one parameter, OID 23 (int4).
    let params = &msgs
        .iter()
        .find(|(t, _)| *t == b't')
        .context("ParameterDescription is missing")?
        .1;
    assert_eq!(&params[..], &[0, 1, 0, 0, 0, 23]);

    // Close statement "s", then Describe it again → 26000, recover at Sync.
    let mut batch = Vec::new();
    batch.extend(frontend_message(b'C', b"Ss\0")); // Close statement "s"
    batch.extend(frontend_message(b'D', b"Ss\0")); // Describe closed statement → error
    batch.extend(frontend_message(b'S', b""));
    socket.write_all(&batch).await?;

    let msgs = read_until_ready(&mut socket).await;
    let tags: Vec<u8> = msgs.iter().map(|(t, _)| *t).collect();
    // CloseComplete, ErrorResponse, ReadyForQuery.
    assert_eq!(tags, [b'3', b'E', b'Z']);

    Ok(())
}

/// psql and libpq open with SSLRequest; the server must answer `N` and then
/// complete a cleartext handshake on the same connection.
#[tokio::test]
async fn ssl_request_is_refused_then_startup_proceeds() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;

    socket.write_all(&[0, 0, 0, 8, 4, 210, 22, 47]).await?; // SSLRequest
    assert_eq!(socket.read_u8().await?, b'N');

    let mut body = Vec::new();
    body.extend_from_slice(&196608i32.to_be_bytes());
    body.extend_from_slice(b"user\0postgres\0\0");
    socket
        .write_all(&((body.len() + 4) as i32).to_be_bytes())
        .await?;
    socket.write_all(&body).await?;

    // Read backend messages until ReadyForQuery('I'); first must be AuthenticationOk.
    let mut first_tag = None;
    loop {
        let tag = socket.read_u8().await?;
        let len = socket.read_i32().await? as usize;
        let mut msg = vec![0u8; len - 4];
        socket.read_exact(&mut msg).await?;
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

    Ok(())
}

// --- Transactions: raw-socket helpers (tokio-postgres only exposes the numeric
// command count, not the tag text or the ReadyForQuery status byte) ---

/// Send a simple `Query` and collect every backend `(tag, body)` up to and
/// including the terminating ReadyForQuery.
async fn simple_query_raw(socket: &mut tokio::net::TcpStream, sql: &str) -> Vec<(u8, Vec<u8>)> {
    let mut q = sql.as_bytes().to_vec();
    q.push(0);
    if let Err(error) = socket.write_all(&frontend_message(b'Q', &q)).await {
        panic!("failed to write simple-query test message: {error}");
    }
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
    let decoded = match crabgresql_pg_wire::BackendMessage::decode(msg.0, &msg.1) {
        Ok(decoded) => decoded,
        Err(error) => panic!("failed to decode backend test message: {error}"),
    };
    match decoded {
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
async fn truncate_empties_tables() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id integer)").await?;
    client
        .simple_query("INSERT INTO t VALUES (1), (2), (3)")
        .await?;

    client.simple_query("TRUNCATE t").await?;
    assert_eq!(
        rows(&client.simple_query("SELECT * FROM t").await?).len(),
        0
    );

    // The `TRUNCATE TABLE` keyword form works too.
    client.simple_query("INSERT INTO t VALUES (9)").await?;
    client.simple_query("TRUNCATE TABLE t").await?;
    assert_eq!(
        rows(&client.simple_query("SELECT * FROM t").await?).len(),
        0
    );

    // A missing table fails the statement with 42P01.
    let err = client.simple_query("TRUNCATE nope").await.unwrap_err();
    assert_eq!(
        err.as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::UNDEFINED_TABLE
    );

    Ok(())
}

#[tokio::test]
async fn truncate_resolves_every_table_before_emptying() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE a (id integer)").await?;
    client.simple_query("INSERT INTO a VALUES (1)").await?;

    // The second name is missing: the whole statement fails and `a` is untouched.
    let err = client
        .simple_query("TRUNCATE a, missing")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::UNDEFINED_TABLE
    );
    assert_eq!(
        rows(&client.simple_query("SELECT * FROM a").await?).len(),
        1,
        "no table may be emptied when any named table is missing"
    );

    Ok(())
}

#[tokio::test]
async fn unsupported_transaction_forms_are_rejected() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id integer)").await?;
    for sql in [
        // ISOLATION LEVEL / READ ONLY are now honored; SNAPSHOT isolation and
        // SET TRANSACTION SNAPSHOT remain unsupported modes.
        "BEGIN ISOLATION LEVEL SNAPSHOT",
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

    Ok(())
}

#[tokio::test]
async fn explain_shows_index_scan_for_pk_equality() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id int PRIMARY KEY, label text)")
        .await?;
    client
        .simple_query("INSERT INTO t VALUES (1, 'one'), (2, 'two'), (3, 'three')")
        .await?;

    // An equality on the PRIMARY KEY chooses an index scan.
    let plan = client
        .simple_query("EXPLAIN SELECT * FROM t WHERE id = 2")
        .await?;
    let lines: Vec<&str> = rows(&plan).iter().filter_map(|r| r.get(0)).collect();
    assert_eq!(lines[0], "Index Scan using t_pkey on t");
    assert!(
        lines.iter().any(|l| l.contains("Index Cond: (id = 2)")),
        "plan was {lines:?}"
    );

    // ...and it still returns the right row.
    let result = client
        .simple_query("SELECT label FROM t WHERE id = 2")
        .await?;
    assert_eq!(rows(&result)[0].get(0), Some("two"));

    // A filter on a non-indexed column stays a sequential scan.
    let plan = client
        .simple_query("EXPLAIN SELECT * FROM t WHERE label = 'two'")
        .await?;
    let lines: Vec<&str> = rows(&plan).iter().filter_map(|r| r.get(0)).collect();
    assert_eq!(lines[0], "Seq Scan on t");

    // EXPLAIN ANALYZE is rejected (it would execute the statement).
    let err = client
        .simple_query("EXPLAIN ANALYZE SELECT * FROM t WHERE id = 2")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::FEATURE_NOT_SUPPORTED
    );

    Ok(())
}
