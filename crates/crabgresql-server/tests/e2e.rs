//! End-to-end tests: a real driver (tokio-postgres) against an in-process
//! server on an ephemeral port, plus raw-socket checks of the startup phase.

#![allow(clippy::unwrap_used)]

use anyhow::Context as _;
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
    // Each test server gets its own throwaway data directory, so runs are
    // isolated. The dir is leaked to keep it alive for the spawned server's whole
    // lifetime (the OS reclaims it after the test process exits).
    let dir = tempfile::tempdir().expect("create temp data dir");
    let (engine, txnmgr) =
        crabgresql_server::open_pg_engine(dir.path()).expect("open test engine");
    std::mem::forget(dir);
    tokio::spawn(crabgresql_server::serve_with(listener, engine, txnmgr));
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

#[tokio::test]
async fn dml_returning_streams_affected_rows() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE crabs (id integer, name text)")
        .await?;

    // INSERT ... RETURNING streams the inserted rows (with a computed column)
    // and still reports the INSERT row count.
    let messages = client
        .simple_query(
            "INSERT INTO crabs VALUES (1, 'ferris'), (2, 'hermit') RETURNING id, name, id + 10 AS bumped",
        )
        .await?;
    let inserted = rows(&messages);
    assert_eq!(inserted.len(), 2);
    assert_eq!(inserted[0].get(0), Some("1"));
    assert_eq!(inserted[0].get(1), Some("ferris"));
    assert_eq!(inserted[0].get(2), Some("11"));
    assert_eq!(inserted[1].get(2), Some("12"));
    let count = messages.iter().find_map(|m| match m {
        SimpleQueryMessage::CommandComplete(n) => Some(*n),
        _ => None,
    });
    assert_eq!(count, Some(2));

    // UPDATE ... RETURNING returns the NEW rows.
    let messages = client
        .simple_query("UPDATE crabs SET name = 'crab' WHERE id = 1 RETURNING id, name")
        .await?;
    let updated = rows(&messages);
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].get(0), Some("1"));
    assert_eq!(updated[0].get(1), Some("crab"));

    // DELETE ... RETURNING returns the deleted (OLD) rows, columns reordered
    // (name, id). Storage scan order is unspecified, so compare as a set.
    let messages = client
        .simple_query("DELETE FROM crabs RETURNING name, id")
        .await?;
    let mut deleted: Vec<(Option<&str>, Option<&str>)> =
        rows(&messages).iter().map(|r| (r.get(0), r.get(1))).collect();
    deleted.sort();
    assert_eq!(
        deleted,
        vec![(Some("crab"), Some("1")), (Some("hermit"), Some("2"))]
    );
    let count = messages.iter().find_map(|m| match m {
        SimpleQueryMessage::CommandComplete(n) => Some(*n),
        _ => None,
    });
    assert_eq!(count, Some(2));

    // The table is now empty.
    assert!(rows(&client.simple_query("SELECT id FROM crabs").await?).is_empty());

    Ok(())
}

/// A RETURNING expression that faults at runtime must abort the whole statement,
/// leaving the mutation rolled back — the projection runs before the commit.
#[tokio::test]
async fn dml_returning_faulting_expression_rolls_back_the_mutation() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id integer)").await?;
    client.simple_query("INSERT INTO t VALUES (5)").await?;

    // INSERT: division by zero in RETURNING must insert nothing.
    let err = client
        .simple_query("INSERT INTO t (id) VALUES (0) RETURNING 100/id")
        .await
        .expect_err("expected a division-by-zero error");
    assert_eq!(err.code(), Some(&tokio_postgres::error::SqlState::DIVISION_BY_ZERO));
    let after_insert = client.simple_query("SELECT id FROM t").await?;
    assert_eq!(
        rows(&after_insert).len(),
        1,
        "the failed INSERT must not persist a row"
    );

    // DELETE: same faulting RETURNING must delete nothing.
    client
        .simple_query("DELETE FROM t RETURNING 100/(id-5)")
        .await
        .expect_err("expected a division-by-zero error");
    let after_delete = client.simple_query("SELECT id FROM t").await?;
    assert_eq!(
        rows(&after_delete).len(),
        1,
        "the failed DELETE must not remove the row"
    );

    Ok(())
}

/// The extended protocol: a prepared `INSERT ... RETURNING` reports its result
/// column shape at Describe and streams rows at Execute via `query`.
#[tokio::test]
async fn dml_returning_extended_protocol() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id integer, label text)")
        .await?;

    let returned = client
        .query(
            "INSERT INTO t VALUES ($1, $2) RETURNING id, label",
            &[&7i32, &"seven"],
        )
        .await?;
    assert_eq!(returned.len(), 1);
    assert_eq!(returned[0].get::<_, i32>("id"), 7);
    assert_eq!(returned[0].get::<_, &str>("label"), "seven");

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

/// The five listing queries psql sends for `\d`, `\dt`, `\dv`, `\ds`, and `\di`,
/// replayed verbatim (captured from psql 18.4, which is the shape we get because
/// we advertise `server_version = 19.0`). The regress runner cannot execute
/// backslash metacommands, so this is the gate that the whole query binds and
/// runs — including the owner column, which the smoke suite cannot assert
/// because the reference server's role differs.
#[tokio::test]
async fn psql_describe_listings_match_pg() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    client
        .simple_query("CREATE TABLE t1 (id int PRIMARY KEY, label text)")
        .await?;
    client
        .simple_query("CREATE VIEW v1 AS SELECT id FROM t1")
        .await?;
    client.simple_query("CREATE SEQUENCE s1").await?;
    client
        .simple_query("CREATE TABLE parted (id int, k int) PARTITION BY RANGE (id)")
        .await?;
    client
        .simple_query("CREATE TABLE parted_lo PARTITION OF parted FOR VALUES FROM (1) TO (10)")
        .await?;
    // A relation only a qualified name reaches must not appear in any listing.
    client.simple_query("CREATE SCHEMA app").await?;
    client
        .simple_query("CREATE TABLE app.hidden (x int)")
        .await?;

    // psql builds the four table-ish listings from one query, varying only the
    // relkind set; `\dv` and `\ds` omit the pg_am join.
    let listing = |relkinds: &str, with_am: bool| {
        format!(
            "SELECT n.nspname as \"Schema\",\n  \
               c.relname as \"Name\",\n  \
               CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' \
               WHEN 'm' THEN 'materialized view' WHEN 'i' THEN 'index' \
               WHEN 'S' THEN 'sequence' WHEN 't' THEN 'TOAST table' \
               WHEN 'f' THEN 'foreign table' WHEN 'p' THEN 'partitioned table' \
               WHEN 'I' THEN 'partitioned index' END as \"Type\",\n  \
               pg_catalog.pg_get_userbyid(c.relowner) as \"Owner\"\n\
             FROM pg_catalog.pg_class c\n     \
             LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace\n{}\
             WHERE c.relkind IN ({relkinds})\n      \
             AND n.nspname <> 'pg_catalog'\n      \
             AND n.nspname !~ '^pg_toast'\n      \
             AND n.nspname <> 'information_schema'\n  \
             AND pg_catalog.pg_table_is_visible(c.oid)\n\
             ORDER BY 1,2;",
            if with_am {
                "     LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam\n"
            } else {
                ""
            }
        )
    };
    let listed = |messages: &[SimpleQueryMessage]| {
        rows(messages)
            .iter()
            .map(|r| {
                format!(
                    "{}|{}|{}|{}",
                    r.get(0).unwrap_or("?"),
                    r.get(1).unwrap_or("?"),
                    r.get(2).unwrap_or("?"),
                    r.get(3).unwrap_or("?"),
                )
            })
            .collect::<Vec<_>>()
    };

    // `\d` — every relkind a user can see. `app.hidden` is absent (not visible),
    // and the owner is the connected role, as PG reports it.
    let d = client
        .simple_query(&listing("'r','p','v','m','S','f',''", true))
        .await?;
    assert_eq!(
        listed(&d),
        vec![
            "public|parted|partitioned table|postgres",
            "public|parted_lo|table|postgres",
            "public|s1|sequence|postgres",
            "public|t1|table|postgres",
            "public|v1|view|postgres",
        ]
    );

    // `\dt` — tables and partitioned parents only.
    let dt = client.simple_query(&listing("'r','p',''", true)).await?;
    assert_eq!(
        listed(&dt),
        vec![
            "public|parted|partitioned table|postgres",
            "public|parted_lo|table|postgres",
            "public|t1|table|postgres",
        ]
    );

    // `\dv` and `\ds` drop the pg_am join entirely.
    let dv = client.simple_query(&listing("'v',''", false)).await?;
    assert_eq!(listed(&dv), vec!["public|v1|view|postgres"]);
    let ds = client.simple_query(&listing("'S',''", false)).await?;
    assert_eq!(listed(&ds), vec!["public|s1|sequence|postgres"]);

    // `\di` adds pg_index and a second pg_class alias for the indexed table, so
    // the same catalog relation is scanned twice in one statement — the OIDs
    // must agree across both scans and across pg_table_is_visible.
    let di = client
        .simple_query(
            "SELECT n.nspname as \"Schema\",\n  \
               c.relname as \"Name\",\n  \
               CASE c.relkind WHEN 'i' THEN 'index' WHEN 'I' THEN 'partitioned index' END as \"Type\",\n  \
               pg_catalog.pg_get_userbyid(c.relowner) as \"Owner\",\n  \
               c2.relname as \"Table\"\n\
             FROM pg_catalog.pg_class c\n     \
             LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace\n     \
             LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam\n     \
             LEFT JOIN pg_catalog.pg_index i ON i.indexrelid = c.oid\n     \
             LEFT JOIN pg_catalog.pg_class c2 ON i.indrelid = c2.oid\n\
             WHERE c.relkind IN ('i','I','')\n      \
             AND n.nspname <> 'pg_catalog'\n      \
             AND n.nspname !~ '^pg_toast'\n      \
             AND n.nspname <> 'information_schema'\n  \
             AND pg_catalog.pg_table_is_visible(c.oid)\n\
             ORDER BY 1,2;",
        )
        .await?;
    let index_rows = rows(&di);
    assert_eq!(index_rows.len(), 1);
    assert_eq!(index_rows[0].get(1), Some("t1_pkey"));
    assert_eq!(index_rows[0].get(2), Some("index"));
    assert_eq!(index_rows[0].get(3), Some("postgres"));
    assert_eq!(index_rows[0].get(4), Some("t1"));

    // A temp relation is this session's, so it joins the listing under its own
    // namespace — the one visibility rule that is not simply "public".
    client
        .simple_query("CREATE TEMP TABLE tmp1 (y int)")
        .await?;
    let with_temp = client.simple_query(&listing("'r','p',''", true)).await?;
    let temp_row = rows(&with_temp)
        .into_iter()
        .find(|r| r.get(1) == Some("tmp1"))
        .context("temp table should be listed")?;
    assert!(
        temp_row.get(0).unwrap_or("").starts_with("pg_temp_"),
        "temp relation should list under its pg_temp_N namespace, got {:?}",
        temp_row.get(0)
    );

    // The index's relam resolves through pg_am, and a view's `relam = 0` finds
    // no row (which is why psql uses a LEFT JOIN).
    let ams = client
        .simple_query(
            "SELECT c.relname, a.amname FROM pg_catalog.pg_class c \
             LEFT JOIN pg_catalog.pg_am a ON a.oid = c.relam \
             WHERE c.relname IN ('t1', 't1_pkey', 'v1') ORDER BY 1",
        )
        .await?;
    let am_rows = rows(&ams);
    assert_eq!(am_rows[0].get(1), Some("heap")); // t1
    assert_eq!(am_rows[1].get(1), Some("btree")); // t1_pkey
    assert_eq!(am_rows[2].get(1), None); // v1

    Ok(())
}

/// A declared length beyond PostgreSQL's limit is rejected at DDL time, with
/// PG's message and SQLSTATE. It must not be stored: `pg_attribute` encodes a
/// character length as `n + 4`, so an unchecked one would overflow there and
/// take down every later reader of the catalog — permanently, since the table
/// outlives the session.
#[tokio::test]
async fn out_of_range_declared_length_is_rejected() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;

    for ddl in [
        "CREATE TABLE t (a varchar(2147483647))",
        "CREATE TABLE t (a varchar(10485761))",
        "CREATE TABLE t (a char(10485761))",
        "CREATE TABLE t (a bit(83886081))",
        "CREATE TABLE t (a varchar(0))",
    ] {
        let err = client.simple_query(ddl).await.unwrap_err();
        assert_eq!(
            err.code(),
            Some(&SqlState::INVALID_PARAMETER_VALUE),
            "{ddl} should be rejected as out of range"
        );
    }

    // The catalog is still readable, and the session still usable.
    client
        .simple_query("CREATE TABLE t (a varchar(20), b char(4))")
        .await?;
    let attrs = client
        .simple_query(
            "SELECT a.attname, a.atttypmod, pg_catalog.format_type(a.atttypid, a.atttypmod) \
             FROM pg_catalog.pg_attribute a, pg_catalog.pg_class c \
             WHERE c.relname = 't' AND a.attrelid = c.oid AND a.attnum > 0 ORDER BY a.attnum",
        )
        .await?;
    let rows = rows(&attrs);
    assert_eq!(
        (rows[0].get(1), rows[0].get(2)),
        (Some("24"), Some("character varying(20)"))
    );
    assert_eq!(
        (rows[1].get(1), rows[1].get(2)),
        (Some("8"), Some("character(4)"))
    );

    Ok(())
}

/// A `reg*` cannot be a stored column type. Its whole contract is that the OID
/// keeps naming the same object, and crabgresql numbers relations positionally
/// per catalog snapshot — so a stored value would silently come to name a
/// different relation after unrelated DDL. Expression use is unaffected.
#[tokio::test]
async fn reg_types_are_rejected_as_stored_columns() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE zz (a int)").await?;

    for ddl in [
        "CREATE TABLE hold (r regclass)",
        "CREATE TABLE hold (r regtype)",
        "CREATE TABLE hold (r regclass[])",
        "CREATE TABLE hold AS SELECT 'zz'::regclass AS r",
    ] {
        let err = client.simple_query(ddl).await.unwrap_err();
        assert_eq!(
            err.code(),
            Some(&SqlState::FEATURE_NOT_SUPPORTED),
            "{ddl} should be rejected"
        );
    }

    // ... while a reg* in an expression still resolves, in both directions.
    let q = client
        .simple_query(
            "SELECT 'zz'::regclass::text, 'pg_class'::regclass::text, \
             1259::regclass::text, 'pg_class'::regclass = 1259::regclass",
        )
        .await?;
    let row = &rows(&q)[0];
    assert_eq!(row.get(0), Some("zz"));
    // A catalog relation resolves by name and by its fixed OID, and renders
    // unqualified because `pg_catalog` is always reachable.
    assert_eq!(row.get(1), Some("pg_class"));
    assert_eq!(row.get(2), Some("pg_class"));
    assert_eq!(row.get(3), Some("t"));

    // `oid = regclass` resolves through the implicit reg* -> oid cast, the
    // comparison shape psql's `\d` uses to find a relation.
    let cmp = client
        .simple_query("SELECT relname FROM pg_catalog.pg_class WHERE oid = 'zz'::regclass")
        .await?;
    assert_eq!(rows(&cmp)[0].get(0), Some("zz"));

    Ok(())
}

/// The per-column query psql sends third for `\d <table>` / `\d <view>`,
/// replayed verbatim but for the OID psql substitutes from its first query.
/// This is the gate that the whole statement binds and runs: `format_type` over
/// the stored `atttypmod`, `pg_get_expr` over a column default in a correlated
/// subquery, the collation subquery that must self-filter to NULL, and the
/// `attidentity`/`attgenerated` projections.
#[tokio::test]
async fn psql_describe_columns_match_pg() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    client
        .simple_query(
            "CREATE TABLE t1 (id int PRIMARY KEY, code varchar(20), \
             tag char(4), mask bit(5), flag bool DEFAULT true, note text)",
        )
        .await?;

    // Verbatim psql, with the relation located by name instead of by OID.
    let columns = client
        .simple_query(
            "SELECT a.attname,\n  \
               pg_catalog.format_type(a.atttypid, a.atttypmod),\n  \
               (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true)\n   \
                FROM pg_catalog.pg_attrdef d\n   \
                WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum AND a.atthasdef),\n  \
               a.attnotnull,\n  \
               (SELECT c.collname FROM pg_catalog.pg_collation c, pg_catalog.pg_type t\n   \
                WHERE c.oid = a.attcollation AND t.oid = a.atttypid \
                AND a.attcollation <> t.typcollation) AS attcollation,\n  \
               a.attidentity,\n  \
               a.attgenerated\n\
             FROM pg_catalog.pg_attribute a, pg_catalog.pg_class c\n\
             WHERE c.relname = 't1' AND a.attrelid = c.oid AND a.attnum > 0 \
             AND NOT a.attisdropped\n\
             ORDER BY a.attnum;",
        )
        .await?;
    let described = rows(&columns)
        .iter()
        .map(|r| {
            format!(
                "{}|{}|{}|{}|{}|{}|{}",
                r.get(0).unwrap_or("?"),
                r.get(1).unwrap_or("?"),
                // The three columns psql renders as blank when absent.
                r.get(2).unwrap_or(""),
                r.get(3).unwrap_or("?"),
                r.get(4).unwrap_or(""),
                r.get(5).unwrap_or(""),
                r.get(6).unwrap_or(""),
            )
        })
        .collect::<Vec<_>>();
    // Exactly what PostgreSQL 18.4 returns for this table: the declared type
    // modifiers survive into `format_type`, the default deparses back to its SQL
    // text, and no column is collated, identity, or generated.
    assert_eq!(
        described,
        vec![
            "id|integer||t|||".to_string(),
            "code|character varying(20)||f|||".to_string(),
            "tag|character(4)||f|||".to_string(),
            "mask|bit(5)||f|||".to_string(),
            "flag|boolean|true|f|||".to_string(),
            "note|text||f|||".to_string(),
        ]
    );

    Ok(())
}

#[tokio::test]
async fn create_drop_schema_and_qualified_relations_match_pg() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;

    // CREATE SCHEMA registers a namespace visible in pg_namespace and
    // information_schema.schemata.
    client.simple_query("CREATE SCHEMA app").await?;
    let ns = client
        .simple_query("SELECT nspname FROM pg_namespace WHERE nspname = 'app'")
        .await?;
    assert_eq!(rows(&ns)[0].get(0), Some("app"));
    let schemata = client
        .simple_query("SELECT schema_name FROM information_schema.schemata WHERE schema_name = 'app'")
        .await?;
    assert_eq!(rows(&schemata)[0].get(0), Some("app"));

    // Duplicate without IF NOT EXISTS → 42P06; with it → success (a NOTICE).
    let err = client.simple_query("CREATE SCHEMA app").await.unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::from_code("42P06")
    );
    client.simple_query("CREATE SCHEMA IF NOT EXISTS app").await?;

    // A `pg_`-prefixed name is reserved (42939).
    let err = client.simple_query("CREATE SCHEMA pg_evil").await.unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::from_code("42939")
    );

    // A schema-qualified table coexists with a same-named public table, and its
    // pg_class.relnamespace resolves to the schema's pg_namespace.oid.
    client.simple_query("CREATE TABLE app.item (id int, label text)").await?;
    client.simple_query("CREATE TABLE item (id int)").await?;
    client
        .simple_query("INSERT INTO app.item VALUES (1, 'a'), (2, 'b')")
        .await?;
    let selected = client
        .simple_query("SELECT label FROM app.item ORDER BY id")
        .await?;
    let selected = rows(&selected);
    assert_eq!(selected[0].get(0), Some("a"));
    assert_eq!(selected[1].get(0), Some("b"));

    let joined = client
        .simple_query(
            "SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relname = 'item' ORDER BY n.nspname",
        )
        .await?;
    let joined = rows(&joined);
    // One `item` in `app`, one in `public`.
    assert_eq!(joined[0].get(0), Some("app"));
    assert_eq!(joined[1].get(0), Some("public"));

    // A serial column and an explicit sequence both resolve within the schema
    // (regression: qualified sequences used to resolve only in public).
    client
        .simple_query("CREATE TABLE app.counter (id serial, note text)")
        .await?;
    client
        .simple_query("INSERT INTO app.counter (note) VALUES ('x'), ('y')")
        .await?;
    let ids = client
        .simple_query("SELECT id FROM app.counter ORDER BY id")
        .await?;
    let ids = rows(&ids);
    assert_eq!(ids[0].get(0), Some("1"));
    assert_eq!(ids[1].get(0), Some("2"));
    let nv = client.simple_query("SELECT nextval('app.counter_id_seq')").await?;
    assert_eq!(rows(&nv)[0].get(0), Some("3"));

    // CREATE TABLE in a missing schema → 3F000.
    let err = client
        .simple_query("CREATE TABLE nope.t (id int)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::from_code("3F000")
    );

    // DROP SCHEMA RESTRICT on a non-empty schema → 2BP01.
    let err = client.simple_query("DROP SCHEMA app").await.unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::DEPENDENT_OBJECTS_STILL_EXIST
    );

    // DROP SCHEMA CASCADE removes the schema and its contents; the public `item`
    // is untouched.
    client.simple_query("DROP SCHEMA app CASCADE").await?;
    let gone = client
        .simple_query("SELECT nspname FROM pg_namespace WHERE nspname = 'app'")
        .await?;
    assert!(rows(&gone).is_empty());
    // Reading a relation in the now-dropped schema fails (resolution finds no
    // such relation — 42P01, as the read path does not distinguish a missing
    // schema from a missing table).
    let err = client
        .simple_query("SELECT * FROM app.item")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_TABLE
    );
    // The like-named public table survives.
    client.simple_query("SELECT id FROM item").await?;

    // DROP SCHEMA of a missing schema → 3F000; IF EXISTS → success.
    let err = client.simple_query("DROP SCHEMA nope").await.unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::from_code("3F000")
    );
    client.simple_query("DROP SCHEMA IF EXISTS nope").await?;

    Ok(())
}

#[tokio::test]
async fn subqueries_in_expressions_match_pg() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE sq (id int, val int)")
        .await?;
    client
        .simple_query("INSERT INTO sq VALUES (1, 10), (2, 20), (3, 30)")
        .await?;

    // Scalar subquery in the target list.
    let msgs = client
        .simple_query("SELECT (SELECT max(val) FROM sq) AS m")
        .await?;
    assert_eq!(rows(&msgs)[0].get("m"), Some("30"));

    // A scalar subquery with no rows is NULL.
    let msgs = client
        .simple_query("SELECT (SELECT val FROM sq WHERE id = 99) AS m")
        .await?;
    assert_eq!(rows(&msgs)[0].get("m"), None);

    // EXISTS / NOT EXISTS.
    let msgs = client
        .simple_query(
            "SELECT id FROM sq WHERE EXISTS (SELECT 1 FROM sq WHERE val = 20) ORDER BY id",
        )
        .await?;
    assert_eq!(rows(&msgs).len(), 3);

    // IN (SELECT ...) as a WHERE predicate.
    let msgs = client
        .simple_query(
            "SELECT id FROM sq WHERE val IN (SELECT val FROM sq WHERE val <> 20) ORDER BY id",
        )
        .await?;
    let got: Vec<_> = rows(&msgs).iter().map(|r| r.get("id")).collect();
    assert_eq!(got, vec![Some("1"), Some("3")]);

    // NOT IN (SELECT ...).
    let msgs = client
        .simple_query(
            "SELECT id FROM sq WHERE val NOT IN (SELECT val FROM sq WHERE val = 20) ORDER BY id",
        )
        .await?;
    let got: Vec<_> = rows(&msgs).iter().map(|r| r.get("id")).collect();
    assert_eq!(got, vec![Some("1"), Some("3")]);

    // A scalar subquery returning more than one row is a cardinality violation.
    let err = client
        .simple_query("SELECT (SELECT val FROM sq)")
        .await
        .unwrap_err();
    let db = err.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::CARDINALITY_VIOLATION);
    assert_eq!(
        db.message(),
        "more than one row returned by a subquery used as an expression"
    );

    Ok(())
}

/// Regression coverage for the subquery review fixes: a large `IN (SELECT …)`
/// must not overflow the stack, `text IN (SELECT char(n))` must compare across
/// string types, `EXISTS` must ignore its target list, and subqueries must work
/// in `UPDATE`/`DELETE`.
#[tokio::test]
async fn subquery_review_fixes() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    // Large IN subquery: folds to a balanced OR tree, so no stack overflow.
    let msgs = client
        .simple_query(
            "SELECT i FROM generate_series(1, 3) g(i) \
             WHERE i IN (SELECT gs FROM generate_series(1, 50000) x(gs)) ORDER BY i",
        )
        .await?;
    assert_eq!(rows(&msgs).len(), 3);

    // text IN (SELECT char(n)): the candidate coercion must not drop the value.
    client.simple_query("CREATE TABLE txt (t text)").await?;
    client.simple_query("CREATE TABLE chr (c char(3))").await?;
    client
        .simple_query("INSERT INTO txt VALUES ('foo')")
        .await?;
    client
        .simple_query("INSERT INTO chr VALUES ('foo')")
        .await?;
    let msgs = client
        .simple_query("SELECT t IN (SELECT c FROM chr) AS matched FROM txt")
        .await?;
    assert_eq!(rows(&msgs)[0].get("matched"), Some("t"));

    // EXISTS ignores the subquery target list (a 1/0 there must not error).
    let msgs = client
        .simple_query("SELECT EXISTS (SELECT 1 / 0 FROM txt) AS e")
        .await?;
    assert_eq!(rows(&msgs)[0].get("e"), Some("t"));

    // Subqueries in DML predicates/assignments.
    client
        .simple_query("CREATE TABLE dm (id int, val int)")
        .await?;
    client
        .simple_query("INSERT INTO dm VALUES (1, 10), (2, 20), (3, 30)")
        .await?;
    client
        .simple_query("DELETE FROM dm WHERE val IN (SELECT val FROM dm WHERE val > 25)")
        .await?;
    client
        .simple_query("UPDATE dm SET val = (SELECT max(val) FROM dm WHERE val < 25) WHERE id = 1")
        .await?;
    let msgs = client
        .simple_query("SELECT id, val FROM dm ORDER BY id")
        .await?;
    let got: Vec<_> = rows(&msgs)
        .iter()
        .map(|r| (r.get("id"), r.get("val")))
        .collect();
    assert_eq!(got, vec![(Some("1"), Some("20")), (Some("2"), Some("20"))]);

    Ok(())
}

/// Correlated subqueries: a subquery that references a column of the enclosing
/// query is re-evaluated per outer row (the outer references filled from that
/// row) — the shapes TPC-H Q2/Q4/Q17/Q20/Q21/Q22 rely on. Covers correlated
/// `EXISTS`/`NOT EXISTS`, a correlated scalar (with empty → NULL), a correlated
/// scalar-aggregate comparison in WHERE, a correlated `IN`, and two-level
/// correlation.
#[tokio::test]
async fn correlated_subqueries_match_pg() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t1 (a int, b int)").await?;
    client
        .simple_query("INSERT INTO t1 VALUES (1, 10), (2, 20), (3, 30)")
        .await?;
    client.simple_query("CREATE TABLE t2 (a int, c int)").await?;
    client
        .simple_query("INSERT INTO t2 VALUES (1, 100), (1, 200), (2, 20), (2, 50), (4, 400)")
        .await?;

    // Correlated EXISTS (Q4-shape): keep outer rows with a matching t2 row.
    let msgs = client
        .simple_query("SELECT a FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.a = t1.a) ORDER BY a")
        .await?;
    let got: Vec<_> = rows(&msgs).iter().map(|r| r.get("a")).collect();
    assert_eq!(got, vec![Some("1"), Some("2")]);

    // Correlated NOT EXISTS (Q22-shape): the anti-join complement.
    let msgs = client
        .simple_query(
            "SELECT a FROM t1 WHERE NOT EXISTS (SELECT 1 FROM t2 WHERE t2.a = t1.a) ORDER BY a",
        )
        .await?;
    let got: Vec<_> = rows(&msgs).iter().map(|r| r.get("a")).collect();
    assert_eq!(got, vec![Some("3")]);

    // Correlated scalar subquery in the target list; no matching row → NULL.
    let msgs = client
        .simple_query(
            "SELECT a, (SELECT max(c) FROM t2 WHERE t2.a = t1.a) AS mc FROM t1 ORDER BY a",
        )
        .await?;
    let got: Vec<_> = rows(&msgs)
        .iter()
        .map(|r| (r.get("a"), r.get("mc")))
        .collect();
    assert_eq!(
        got,
        vec![
            (Some("1"), Some("200")),
            (Some("2"), Some("50")),
            (Some("3"), None),
        ]
    );

    // Correlated scalar-aggregate comparison in WHERE (Q17-shape). For a=3 the
    // subquery is empty → NULL → the row is dropped (three-valued logic).
    let msgs = client
        .simple_query(
            "SELECT a FROM t1 WHERE b < (SELECT max(c) FROM t2 WHERE t2.a = t1.a) ORDER BY a",
        )
        .await?;
    let got: Vec<_> = rows(&msgs).iter().map(|r| r.get("a")).collect();
    assert_eq!(got, vec![Some("1"), Some("2")]);

    // Correlated IN: the candidate set depends on the outer row. Only a=2 has a
    // t2.c (20) equal to its b (20).
    let msgs = client
        .simple_query(
            "SELECT a FROM t1 WHERE b IN (SELECT c FROM t2 WHERE t2.a = t1.a) ORDER BY a",
        )
        .await?;
    let got: Vec<_> = rows(&msgs).iter().map(|r| r.get("a")).collect();
    assert_eq!(got, vec![Some("2")]);

    // Two-level correlation: the innermost EXISTS references both the middle
    // (y.c, level 1) and the outermost (x.a, level 2) query. Only a=2 qualifies
    // (t2 row (2,20) and t1 row (2,20) with z.a = x.a and z.b = y.c).
    let msgs = client
        .simple_query(
            "SELECT a FROM t1 x WHERE EXISTS (\
               SELECT 1 FROM t2 y WHERE y.a = x.a AND EXISTS (\
                 SELECT 1 FROM t1 z WHERE z.a = x.a AND z.b = y.c)) ORDER BY a",
        )
        .await?;
    let got: Vec<_> = rows(&msgs).iter().map(|r| r.get("a")).collect();
    assert_eq!(got, vec![Some("2")]);

    // Correlation from a join outer (Q17-shape): the outer reference indexes into
    // the joined (t1 || t2) row, so it must line up with that combined layout.
    let msgs = client
        .simple_query(
            "SELECT t1.a FROM t1 JOIN t2 ON t1.a = t2.a \
             WHERE t1.b < (SELECT max(c) FROM t2 z WHERE z.a = t1.a) ORDER BY t1.a",
        )
        .await?;
    let got: Vec<_> = rows(&msgs).iter().map(|r| r.get("a")).collect();
    assert_eq!(got, vec![Some("1"), Some("1"), Some("2"), Some("2")]);

    // A correlated subquery in the target list of a GROUP BY (aggregating) query
    // is rejected cleanly: its OuterColumnRef indices address the pre-aggregation
    // row, which would not line up with the aggregate node's output row. We
    // return 0A000 rather than silently returning a wrong value (PG evaluates it;
    // this is a documented not-yet-supported position). WHERE-clause correlation
    // over an aggregating query (the join-outer case above) is unaffected.
    let err = client
        .simple_query("SELECT a, (SELECT max(c) FROM t2 WHERE t2.a = t1.a) FROM t1 GROUP BY a")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().map(|e| e.code()),
        Some(&SqlState::FEATURE_NOT_SUPPORTED)
    );

    Ok(())
}

/// `ANY`/`SOME`/`ALL` quantified comparisons over both an array expression and a
/// subquery, checked against PostgreSQL: the six comparison operators, empty/NULL
/// three-valued semantics, `= ANY` ≡ `IN` / `<> ALL` ≡ `NOT IN`, a correlated
/// subquery form, single evaluation of a side-effecting needle, and the
/// non-array right-side error. (`= ANY($1)` with an array parameter is not
/// covered: binary array parameters are still undecodable — see `types::wire`.)
#[tokio::test]
async fn any_all_quantified_comparisons_match_pg() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;

    // --- Array form: the six operators and SOME synonym. ---
    let msgs = client
        .simple_query(
            "SELECT 2 = ANY(ARRAY[1,2,3]) AS a, 2 = ALL(ARRAY[1,2]) AS b, \
                    3 > ALL(ARRAY[1,2]) AS c, 20 = SOME(ARRAY[10,20]) AS d",
        )
        .await?;
    let r = &rows(&msgs)[0];
    assert_eq!(r.get("a"), Some("t"));
    assert_eq!(r.get("b"), Some("f"));
    assert_eq!(r.get("c"), Some("t"));
    assert_eq!(r.get("d"), Some("t"));

    // --- Empty / NULL three-valued semantics. ---
    let msgs = client
        .simple_query(
            "SELECT 1 = ANY(ARRAY[2,NULL]) AS null_elem, \
                    1 = ANY(ARRAY[]::int[]) AS empty_any, \
                    1 <> ALL(ARRAY[]::int[]) AS empty_all, \
                    1 = ANY(NULL::int[]) AS null_arr",
        )
        .await?;
    let r = &rows(&msgs)[0];
    assert_eq!(r.get("null_elem"), None); // no match but a NULL ⇒ NULL
    assert_eq!(r.get("empty_any"), Some("f"));
    assert_eq!(r.get("empty_all"), Some("t"));
    assert_eq!(r.get("null_arr"), None);

    // A text-literal array coerces to the needle's type; column is `?column?`.
    let msgs = client.simple_query("SELECT 2 = ANY('{1,2,3}')").await?;
    assert_eq!(rows(&msgs)[0].get("?column?"), Some("t"));

    // --- Subquery form: `= ANY` ≡ IN, `<> ALL` ≡ NOT IN, plus `> ALL`/`> ANY`. ---
    client
        .simple_query("CREATE TABLE sq (id int, val int)")
        .await?;
    client
        .simple_query("INSERT INTO sq VALUES (1,10),(2,20),(3,30)")
        .await?;
    let ids = |msgs: &[SimpleQueryMessage]| -> Vec<String> {
        rows(msgs)
            .iter()
            .filter_map(|r| r.get("id").map(str::to_string))
            .collect()
    };

    let msgs = client
        .simple_query("SELECT id FROM sq WHERE val = ANY(SELECT val FROM sq WHERE val <> 20) ORDER BY id")
        .await?;
    assert_eq!(ids(&msgs), vec!["1", "3"]);

    let msgs = client
        .simple_query("SELECT id FROM sq WHERE val <> ALL(SELECT val FROM sq WHERE val = 20) ORDER BY id")
        .await?;
    assert_eq!(ids(&msgs), vec!["1", "3"]);

    let msgs = client
        .simple_query("SELECT id FROM sq WHERE val > ALL(SELECT val FROM sq WHERE val < 30) ORDER BY id")
        .await?;
    assert_eq!(ids(&msgs), vec!["3"]);

    let msgs = client
        .simple_query("SELECT id FROM sq WHERE val > ANY(SELECT val FROM sq) ORDER BY id")
        .await?;
    assert_eq!(ids(&msgs), vec!["2", "3"]);

    // Correlated subquery: the candidate set depends on the outer row.
    let msgs = client
        .simple_query("SELECT id FROM sq x WHERE val = ANY(SELECT val FROM sq y WHERE y.id = x.id) ORDER BY id")
        .await?;
    assert_eq!(ids(&msgs), vec!["1", "2", "3"]);

    // --- The needle is evaluated exactly once, as PG's ScalarArrayOpExpr does. ---
    // A per-candidate needle would advance the sequence once per element (and,
    // for the subquery form, compare a *different* value against each candidate).
    client.simple_query("CREATE SEQUENCE sq1").await?;
    let msgs = client
        .simple_query("SELECT nextval('sq1') = ANY(ARRAY[99,98,97]) AS r")
        .await?;
    assert_eq!(rows(&msgs)[0].get("r"), Some("f"));
    let msgs = client.simple_query("SELECT currval('sq1') AS v").await?;
    assert_eq!(rows(&msgs)[0].get("v"), Some("1"), "needle evaluated once");

    // Subquery form: with the needle drawn once (3), 3 is in {1,3} → true. A
    // re-drawn needle would compare 3 vs 1 then 4 vs 3 and wrongly yield false.
    client.simple_query("CREATE SEQUENCE sq2").await?;
    client.simple_query("SELECT setval('sq2', 2)").await?;
    let msgs = client
        .simple_query("SELECT nextval('sq2') = ANY(SELECT id FROM sq WHERE id <> 2) AS r")
        .await?;
    assert_eq!(rows(&msgs)[0].get("r"), Some("t"));

    // --- A non-array right side is PG's `op ANY/ALL (array) requires ...` error,
    // with the cursor on the operator as PG places it. ---
    let err = client.simple_query("SELECT 1 = ANY(2)").await.unwrap_err();
    let db = err.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::WRONG_OBJECT_TYPE);
    assert_eq!(db.message(), "op ANY/ALL (array) requires array on right side");
    assert_eq!(db.position(), Some(&tokio_postgres::error::ErrorPosition::Original(10)));

    Ok(())
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
async fn explicit_schema_operator_spelling() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;

    // `OPERATOR(pg_catalog.op)` and the bare `OPERATOR(op)` form resolve to the
    // same built-in operator as the plain spelling: regex, comparison,
    // arithmetic, exponent, array containment, and inet containment/shift all
    // route identically.
    let messages = client
        .simple_query(
            "SELECT 'foo' OPERATOR(pg_catalog.~) 'f.o' AS rx, \
             'foo' OPERATOR(~) 'f.o' AS rx_bare, \
             1 OPERATOR(pg_catalog.=) 1 AS eq, \
             (1 OPERATOR(pg_catalog.+) 2) AS sum, \
             (2 OPERATOR(pg_catalog.^) 3) AS pow, \
             '{1,2}'::int[] OPERATOR(pg_catalog.@>) '{1}'::int[] AS contains, \
             inet '10.0.0.0/8' OPERATOR(pg_catalog.>>) inet '10.1.2.3' AS net_gt, \
             inet '10.1.2.3' OPERATOR(pg_catalog.<<) inet '10.0.0.0/8' AS net_lt",
        )
        .await?;
    let rows = rows(&messages);
    let row = rows[0];
    assert_eq!(row.get(0), Some("t")); // rx
    assert_eq!(row.get(1), Some("t")); // rx_bare
    assert_eq!(row.get(2), Some("t")); // eq
    assert_eq!(row.get(3), Some("3")); // sum
    assert_eq!(row.get(4), Some("8")); // pow
    assert_eq!(row.get(5), Some("t")); // contains
    assert_eq!(row.get(6), Some("t")); // net_gt (>> handled by resolve_network_op)
    assert_eq!(row.get(7), Some("t")); // net_lt (<< handled by resolve_network_op)

    // An unrecognized operator symbol is 42883, and the message names the
    // operator schema-qualified (`pg_catalog.###`) like PG — never wrapped in
    // `OPERATOR(...)` — with the standard "add explicit type casts" hint.
    let err = client
        .simple_query("SELECT 1 OPERATOR(pg_catalog.###) 2")
        .await
        .unwrap_err();
    let db = err.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::UNDEFINED_FUNCTION);
    assert!(
        db.message().starts_with("operator does not exist:")
            && db.message().contains("pg_catalog.###")
            && !db.message().contains("OPERATOR("),
        "unexpected message: {}",
        db.message()
    );
    assert_eq!(
        db.hint(),
        Some(
            "No operator matches the given name and argument types. \
             You might need to add explicit type casts."
        )
    );

    // An operand error surfaces first, as PG analyzes operands before resolving
    // the operator — an undefined column is 42703, not masked as 42883.
    let err = client
        .simple_query("SELECT missing_col OPERATOR(pg_catalog.###) 1")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_COLUMN
    );

    // A non-`pg_catalog` schema qualification never names a built-in operator, so
    // it is reported as 42883. (Real PG additionally reports 3F000 when the schema
    // itself does not exist; the schema catalog is not reachable at bind time.)
    let err = client
        .simple_query("SELECT 1 OPERATOR(myschema.=) 2")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_FUNCTION
    );

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
    // GROUP BY / HAVING and DISTINCT are supported now (see the aggregate and
    // distinct tests); the rest still error rather than being silently dropped.
    for sql in [
        "SELECT 1 FETCH FIRST 1 ROW ONLY",
        "SELECT 1 GROUP BY ROLLUP (1)",
        "SELECT 1 GROUP BY GROUPING SETS ((1))",
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
async fn select_distinct_deduplicates_rows() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE d (a integer, b integer)")
        .await?;
    client
        .simple_query("INSERT INTO d VALUES (1, 10), (1, 10), (2, 20), (2, 30)")
        .await?;

    // Plain DISTINCT collapses the duplicate (1, 10) row.
    let messages = client
        .simple_query("SELECT DISTINCT a, b FROM d ORDER BY a, b")
        .await?;
    let deduped = rows(&messages);
    assert_eq!(deduped.len(), 3);
    assert_eq!((deduped[0].get(0), deduped[0].get(1)), (Some("1"), Some("10")));
    assert_eq!((deduped[1].get(0), deduped[1].get(1)), (Some("2"), Some("20")));
    assert_eq!((deduped[2].get(0), deduped[2].get(1)), (Some("2"), Some("30")));

    // DISTINCT ON (a) keeps the first row per group in ORDER BY order.
    let messages = client
        .simple_query("SELECT DISTINCT ON (a) a, b FROM d ORDER BY a, b DESC")
        .await?;
    let on = rows(&messages);
    assert_eq!(on.len(), 2);
    assert_eq!((on[0].get(0), on[0].get(1)), (Some("1"), Some("10")));
    assert_eq!((on[1].get(0), on[1].get(1)), (Some("2"), Some("30")));

    Ok(())
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
        // Clauses we can't honor must be rejected, not silently dropped:
        // ON COMMIT DROP/DELETE ROWS needs the M2 txn engine.
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
async fn create_table_as_select() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    // Basic CTAS: column shape derives from the query, rows are populated, and the
    // completion tag is `SELECT <n>` (tokio-postgres surfaces the trailing count).
    let messages = client
        .simple_query("CREATE TABLE t AS SELECT 1 AS a, 'x'::text AS b")
        .await?;
    let count = messages.iter().find_map(|m| match m {
        SimpleQueryMessage::CommandComplete(n) => Some(*n),
        _ => None,
    });
    assert_eq!(count, Some(1));
    let msgs = client.simple_query("SELECT a, b FROM t").await?;
    let out = rows(&msgs);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].get(0), Some("1"));
    assert_eq!(out[0].get(1), Some("x"));

    // A new relation reflects as an ordinary table (relkind 'r').
    let msgs = client
        .simple_query("SELECT relkind FROM pg_class WHERE relname = 't'")
        .await?;
    let out = rows(&msgs);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].get(0), Some("r"));

    // An empty source populates nothing and reports `SELECT 0`.
    let messages = client
        .simple_query("CREATE TABLE empty AS SELECT a FROM t WHERE false")
        .await?;
    let count = messages.iter().find_map(|m| match m {
        SimpleQueryMessage::CommandComplete(n) => Some(*n),
        _ => None,
    });
    assert_eq!(count, Some(0));
    let msgs = client.simple_query("SELECT a FROM empty").await?;
    assert!(rows(&msgs).is_empty());

    // Re-creating an existing relation errors 42P07; IF NOT EXISTS skips instead.
    let err = client
        .simple_query("CREATE TABLE t AS SELECT 9 AS a")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::DUPLICATE_TABLE,
    );
    client
        .simple_query("CREATE TABLE IF NOT EXISTS t AS SELECT 9 AS a")
        .await?;
    // The original single row is untouched by the skipped CTAS.
    let msgs = client.simple_query("SELECT a FROM t").await?;
    let out = rows(&msgs);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].get(0), Some("1"));

    // TEMP CTAS is session-local and queryable.
    client
        .simple_query("CREATE TEMP TABLE tmp AS SELECT * FROM t")
        .await?;
    let msgs = client.simple_query("SELECT a, b FROM tmp").await?;
    let out = rows(&msgs);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].get(0), Some("1"));

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
async fn insert_source_query_clauses_execute_and_ragged_values_are_rejected() -> anyhow::Result<()>
{
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (a integer, b integer)")
        .await?;

    // The INSERT source is a full query in PG: ORDER BY / LIMIT execute, and a
    // SELECT / TABLE source is inserted row by row. `VALUES (1),(2) ORDER BY 1`
    // inserts both rows; the SELECT below then copies the smallest one.
    client
        .simple_query("INSERT INTO t (a) VALUES (1), (2) ORDER BY 1")
        .await?;
    client
        .simple_query("INSERT INTO t (a, b) SELECT a, a + 10 FROM t ORDER BY a LIMIT 1")
        .await?;

    // A ragged VALUES list is still a bind-time error.
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

    // Two rows from the VALUES insert, one from the SELECT insert.
    let messages = client.simple_query("SELECT a, b FROM t").await?;
    let t_rows = rows(&messages);
    assert_eq!(t_rows.len(), 3);
    // Exactly one row carries the computed `b = a + 10` from the SELECT source.
    let with_b = t_rows.iter().filter(|r| r.get("b") == Some("11")).count();
    assert_eq!(with_b, 1);

    // A failed INSERT ... SELECT leaves the target untouched: an integer
    // overflow while evaluating the source aborts the whole statement, so `u`
    // stays empty rather than being partially filled.
    client.simple_query("CREATE TABLE u (a integer)").await?;
    let err = client
        .simple_query("INSERT INTO u (a) SELECT a + 2147483647 FROM t")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::NUMERIC_VALUE_OUT_OF_RANGE
    );
    let messages = client.simple_query("SELECT a FROM u").await?;
    assert_eq!(
        rows(&messages).len(),
        0,
        "a failed INSERT ... SELECT must leave no rows"
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

/// The `CommandComplete` tag of the last completed command in a batch.
fn command_tag(msgs: &[(u8, Vec<u8>)]) -> anyhow::Result<String> {
    let body = &msgs
        .iter()
        .rev()
        .find(|(t, _)| *t == b'C')
        .context("CommandComplete is missing")?
        .1;
    Ok(String::from_utf8_lossy(body.split_last().map_or(&body[..], |(_, s)| s)).into_owned())
}

/// A portal executes at most once. A second `Execute` of a portal that already ran
/// to completion must be answered from the portal's recorded state, never by
/// running the statement again — which for a data-modifying statement would apply
/// its writes twice. Named portals inside an explicit block survive Sync, so this
/// is reachable by any client that re-Executes a portal it believes is done.
#[tokio::test]
async fn a_completed_portal_is_not_run_again() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let client = connect(port).await;
    client.simple_query("CREATE TABLE p (id int)").await?;

    // Parse/Bind a named portal, Execute it to completion, then Execute again.
    // `sql` runs inside a block so the portal outlives the first Sync.
    async fn execute_twice(port: u16, sql: &str) -> anyhow::Result<(String, Vec<(u8, Vec<u8>)>)> {
        let mut socket = raw_session(port).await;
        socket
            .write_all(&frontend_message(b'Q', b"BEGIN\0"))
            .await?;
        read_until_ready(&mut socket).await;

        let mut parse = b"st\0".to_vec();
        parse.extend_from_slice(sql.as_bytes());
        parse.extend_from_slice(b"\0\x00\x00");
        let mut batch = Vec::new();
        batch.extend(frontend_message(b'P', &parse));
        // Bind portal "po" from statement "st": no formats, no params.
        batch.extend(frontend_message(b'B', b"po\0st\0\x00\x00\x00\x00\x00\x00"));
        batch.extend(frontend_message(b'E', b"po\0\x00\x00\x00\x00"));
        batch.extend(frontend_message(b'S', b""));
        socket.write_all(&batch).await?;
        let first = command_tag(&read_until_ready(&mut socket).await)?;

        let mut again = Vec::new();
        again.extend(frontend_message(b'E', b"po\0\x00\x00\x00\x00"));
        again.extend(frontend_message(b'S', b""));
        socket.write_all(&again).await?;
        Ok((first, read_until_ready(&mut socket).await))
    }

    // A statement with no result set cannot be re-run at all: PG answers 55000.
    let (first, second) = execute_twice(port, "INSERT INTO p VALUES (1)").await?;
    assert_eq!(first, "INSERT 0 1");
    let err = &second
        .iter()
        .find(|(t, _)| *t == b'E')
        .context("expected an ErrorResponse for the second Execute")?
        .1;
    assert!(
        err.windows(6).any(|w| w == b"55000\0"),
        "expected 55000, got {}",
        String::from_utf8_lossy(err)
    );

    // An exhausted result set re-reports as an empty one, with a zero count.
    let (first, second) = execute_twice(port, "SELECT * FROM p").await?;
    assert_eq!(first, "SELECT 0");
    assert_eq!(command_tag(&second)?, "SELECT 0");

    // EXPLAIN ANALYZE is the case that makes this a data bug: it returns a result
    // set *and* writes, so a re-run would double the write while the client sees
    // nothing but plan text both times.
    client.simple_query("CREATE TABLE q (id int)").await?;
    let (first, second) = execute_twice(port, "EXPLAIN ANALYZE INSERT INTO q VALUES (1)").await?;
    assert_eq!(first, "EXPLAIN");
    assert_eq!(command_tag(&second)?, "EXPLAIN");
    // The block the raw session opened was never committed, so the row is gone —
    // what matters is that a second connection never sees two of them.
    assert_eq!(row_count(&client, "q").await, 0);

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

    // An equality on the PRIMARY KEY chooses an index scan (the durable heap engine
    // builds a physical B-tree for the PK).
    let lines = explain_lines(&client, "EXPLAIN SELECT * FROM t WHERE id = 2").await?;
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
    let lines = explain_lines(&client, "EXPLAIN SELECT * FROM t WHERE label = 'two'").await?;
    assert_eq!(lines[0], "Seq Scan on t");

    Ok(())
}

/// The `QUERY PLAN` lines of an EXPLAIN, as the client sees them.
async fn explain_lines(client: &tokio_postgres::Client, sql: &str) -> anyhow::Result<Vec<String>> {
    let plan = client.simple_query(sql).await?;
    Ok(rows(&plan)
        .iter()
        .filter_map(|r| r.get(0).map(str::to_string))
        .collect())
}

/// The SQLSTATE a failing statement reports.
async fn sqlstate(client: &tokio_postgres::Client, sql: &str) -> anyhow::Result<String> {
    let err = client
        .simple_query(sql)
        .await
        .expect_err("statement should be rejected");
    Ok(err
        .as_db_error()
        .context("database error details are missing")?
        .code()
        .code()
        .to_string())
}

#[tokio::test]
async fn explain_analyze_reports_planning_and_execution_time() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id int PRIMARY KEY, label text)")
        .await?;
    client
        .simple_query("INSERT INTO t VALUES (1, 'one'), (2, 'two'), (3, 'three')")
        .await?;

    // ANALYZE keeps the plan lines a plain EXPLAIN prints and appends PG's two
    // footers — millisecond times to three decimals.
    let lines = explain_lines(&client, "EXPLAIN ANALYZE SELECT * FROM t WHERE id = 2").await?;
    assert_eq!(lines[0], "Index Scan using t_pkey on t");
    assert_eq!(lines[1], "  Index Cond: (id = 2)");
    let footers = &lines[2..];
    assert!(
        footers[0].starts_with("Planning Time: ") && footers[0].ends_with(" ms"),
        "plan was {lines:?}"
    );
    assert!(
        footers[1].starts_with("Execution Time: ") && footers[1].ends_with(" ms"),
        "plan was {lines:?}"
    );
    for footer in footers {
        let time = footer
            .split(": ")
            .nth(1)
            .and_then(|t| t.strip_suffix(" ms"))
            .context("a footer should carry a time")?;
        assert_eq!(
            time.split('.').nth(1).map(str::len),
            Some(3),
            "expected three decimals in {footer:?}"
        );
        assert!(time.parse::<f64>().is_ok(), "{footer:?} is not a number");
    }

    // SUMMARY OFF drops the footers, leaving exactly the plain EXPLAIN output.
    assert_eq!(
        explain_lines(
            &client,
            "EXPLAIN (ANALYZE, SUMMARY OFF) SELECT * FROM t WHERE id = 2"
        )
        .await?,
        explain_lines(&client, "EXPLAIN SELECT * FROM t WHERE id = 2").await?
    );

    // Without ANALYZE there is no execution to time, so PG reports planning alone.
    let lines = explain_lines(&client, "EXPLAIN (SUMMARY ON) SELECT * FROM t").await?;
    assert_eq!(lines[0], "Seq Scan on t");
    assert!(lines[1].starts_with("Planning Time: "), "plan was {lines:?}");
    assert_eq!(lines.len(), 2, "plan was {lines:?}");

    Ok(())
}

#[tokio::test]
async fn explain_analyze_runs_dml_and_plain_explain_does_not() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id int)").await?;

    // A plain EXPLAIN only plans: the row is not written.
    let lines = explain_lines(&client, "EXPLAIN INSERT INTO t VALUES (1)").await?;
    assert_eq!(lines, vec!["Insert on t".to_string()]);
    assert_eq!(row_count(&client, "t").await, 0);

    // ANALYZE executes it for real — and still answers with the plan, not
    // `INSERT 0 1`.
    let lines = explain_lines(&client, "EXPLAIN ANALYZE INSERT INTO t VALUES (1)").await?;
    assert_eq!(lines[0], "Insert on t");
    assert!(lines[1].starts_with("Planning Time: "), "plan was {lines:?}");
    assert_eq!(row_count(&client, "t").await, 1);

    // UPDATE and DELETE apply too.
    explain_lines(&client, "EXPLAIN ANALYZE UPDATE t SET id = 2").await?;
    assert_eq!(row_count(&client, "t WHERE id = 2").await, 1);
    explain_lines(&client, "EXPLAIN ANALYZE DELETE FROM t").await?;
    assert_eq!(row_count(&client, "t").await, 0);

    Ok(())
}

#[tokio::test]
async fn explain_analyze_write_is_visible_to_another_session() -> anyhow::Result<()> {
    // Under autocommit the write must be committed by the time the statement
    // finishes, not left dangling in an unfinalized transaction.
    let port = spawn_server().await;
    let client = connect(port).await;
    client.simple_query("CREATE TABLE t (id int)").await?;
    client
        .simple_query("EXPLAIN ANALYZE INSERT INTO t VALUES (7)")
        .await?;

    let other = connect(port).await;
    assert_eq!(row_count(&other, "t").await, 1);

    Ok(())
}

#[tokio::test]
async fn explain_analyze_aborts_the_statement_when_the_run_faults() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id int)").await?;
    client
        .simple_query("INSERT INTO t VALUES (1), (2), (3)")
        .await?;

    // The third source row divides by zero. The fault surfaces as the statement's
    // error and nothing it had already written survives — proving the run is
    // drained before the commit, not after.
    assert_eq!(
        sqlstate(
            &client,
            "EXPLAIN ANALYZE INSERT INTO t SELECT 100 / (id - 3) FROM t"
        )
        .await?,
        "22012"
    );
    assert_eq!(row_count(&client, "t").await, 3);

    Ok(())
}

#[tokio::test]
async fn explain_analyze_rolls_back_with_its_transaction_block() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id int)").await?;

    client.simple_query("BEGIN").await?;
    client
        .simple_query("EXPLAIN ANALYZE INSERT INTO t VALUES (1)")
        .await?;
    // The write is visible inside the block...
    assert_eq!(row_count(&client, "t").await, 1);
    client.simple_query("ROLLBACK").await?;
    // ...and gone after the rollback: ANALYZE's write belongs to the block, and is
    // not committed at the statement boundary.
    assert_eq!(row_count(&client, "t").await, 0);

    Ok(())
}

#[tokio::test]
async fn explain_analyze_honors_the_read_only_transaction_check() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id int)").await?;
    client.simple_query("INSERT INTO t VALUES (1)").await?;

    client.simple_query("BEGIN TRANSACTION READ ONLY").await?;
    // A plain EXPLAIN of a write is accepted in a read-only transaction — it never
    // executes, so there is nothing to reject. ANALYZE does execute, so it is.
    assert_eq!(
        explain_lines(&client, "EXPLAIN DELETE FROM t WHERE id = 1").await?,
        vec!["Delete on t".to_string()]
    );
    assert_eq!(
        sqlstate(&client, "EXPLAIN ANALYZE DELETE FROM t WHERE id = 1").await?,
        "25006"
    );
    client.simple_query("ROLLBACK").await?;
    assert_eq!(row_count(&client, "t").await, 1);

    Ok(())
}

#[tokio::test]
async fn explain_of_a_utility_statement_never_runs_it() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    // Wrapping a utility in EXPLAIN must report the gap, not fall through to the
    // handler and create the table for real — with or without ANALYZE.
    //
    // The invariant under test is "never runs it". The SQLSTATE is a known
    // divergence, not a contract: PG's grammar accepts only explainable
    // statements, so `EXPLAIN CREATE TABLE …` is a 42601 syntax error there, and
    // `EXPLAIN <other utility>` prints one `Utility Statement` row. Closing that
    // gap belongs with the statements themselves (`EXPLAIN CREATE TABLE … AS
    // SELECT` is fully explainable in PG), and may change this assertion.
    for sql in [
        "EXPLAIN CREATE TABLE untouched (x int)",
        "EXPLAIN ANALYZE CREATE TABLE untouched (x int)",
    ] {
        assert_eq!(sqlstate(&client, sql).await?, "0A000", "{sql}");
    }
    assert_eq!(
        row_count(&client, "pg_class WHERE relname = 'untouched'").await,
        0
    );

    Ok(())
}

#[tokio::test]
async fn explain_options_report_pgs_sqlstates() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    for (sql, expected) in [
        // An unknown option name is a syntax error in PG, not an invalid value.
        ("EXPLAIN (BOGUS) SELECT 1", "42601"),
        ("EXPLAIN (FORMAT bogus) SELECT 1", "22023"),
        ("EXPLAIN (TIMING ON) SELECT 1", "22023"),
        // Options PG supports and crabgresql cannot yet produce.
        ("EXPLAIN (FORMAT JSON) SELECT 1", "0A000"),
        ("EXPLAIN VERBOSE SELECT 1", "0A000"),
        ("EXPLAIN (SETTINGS) SELECT 1", "0A000"),
    ] {
        assert_eq!(sqlstate(&client, sql).await?, expected, "{sql}");
    }

    // ...and the ones we accept and ignore stay accepted.
    for sql in [
        "EXPLAIN (COSTS OFF) SELECT 1",
        "EXPLAIN (ANALYZE, BUFFERS, TIMING OFF) SELECT 1",
        "EXPLAIN (FORMAT TEXT) SELECT 1",
    ] {
        client.simple_query(sql).await?;
    }

    // Another dialect's spelling that the shared parser accepts is not PG's, and
    // PG's grammar rejects it before it resolves any name.
    for sql in [
        "EXPLAIN QUERY PLAN SELECT 1",
        "EXPLAIN ESTIMATE SELECT 1",
        "EXPLAIN FORMAT TEXT SELECT 1",
        "EXPLAIN FORMAT JSON SELECT 1",
    ] {
        assert_eq!(sqlstate(&client, sql).await?, "42601", "{sql}");
    }

    Ok(())
}

#[tokio::test]
async fn explain_resolves_names_before_reading_its_options() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    // PG parse-analyzes the inner statement before it reads the option list, so a
    // missing relation outranks a bad option — a client fixing errors in the order
    // reported sees the same sequence it would from PG.
    for sql in [
        "EXPLAIN (BOGUS) SELECT * FROM nonexistent",
        "EXPLAIN (FORMAT JSON) SELECT * FROM nonexistent",
        "EXPLAIN (TIMING ON) SELECT * FROM nonexistent",
        "EXPLAIN (ANALYZE -1) SELECT * FROM nonexistent",
    ] {
        assert_eq!(sqlstate(&client, sql).await?, "42P01", "{sql}");
    }

    // A grammar error is the exception: PG raises it before name resolution.
    assert_eq!(
        sqlstate(&client, "EXPLAIN QUERY PLAN SELECT * FROM nonexistent").await?,
        "42601"
    );

    // With the relation present, the option error is what surfaces.
    client.simple_query("CREATE TABLE present (id int)").await?;
    assert_eq!(
        sqlstate(&client, "EXPLAIN (BOGUS) SELECT * FROM present").await?,
        "42601"
    );

    Ok(())
}

#[tokio::test]
async fn an_unreadable_explain_option_value_does_not_run_the_statement() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id int)").await?;
    client
        .simple_query("INSERT INTO t VALUES (1), (2), (3)")
        .await?;

    // A value the option parser cannot read must be an error, never a silent
    // "TRUE": read as TRUE, ANALYZE would turn on and the DELETE would run.
    for sql in [
        "EXPLAIN (ANALYZE -1) DELETE FROM t",
        "EXPLAIN (ANALYZE NULL) DELETE FROM t",
        "EXPLAIN (ANALYZE yes) DELETE FROM t",
    ] {
        assert_eq!(sqlstate(&client, sql).await?, "42601", "{sql}");
        assert_eq!(row_count(&client, "t").await, 3, "{sql} deleted rows");
    }

    Ok(())
}

#[tokio::test]
async fn plain_explain_pins_the_repeatable_read_snapshot() -> anyhow::Result<()> {
    // EXPLAIN reads the catalog, so it takes a snapshot like any other reading
    // statement: in a REPEATABLE READ block it must freeze the view the rest of
    // the block sees, even though it executes nothing.
    let port = spawn_server().await;
    let client = connect(port).await;
    client.simple_query("CREATE TABLE t (id int)").await?;
    client.simple_query("INSERT INTO t VALUES (1)").await?;

    client
        .simple_query("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await?;
    client.simple_query("EXPLAIN SELECT * FROM t").await?;

    let other = connect(port).await;
    other.simple_query("INSERT INTO t VALUES (99)").await?;

    // The snapshot was taken by the EXPLAIN, so the concurrent insert is invisible.
    assert_eq!(row_count(&client, "t").await, 1);
    client.simple_query("ROLLBACK").await?;
    assert_eq!(row_count(&client, "t").await, 2);

    Ok(())
}

#[tokio::test]
async fn explain_reports_the_explain_command_tag() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id int)").await?;

    // PG's CommandComplete for EXPLAIN is the bare tag `EXPLAIN`, with no count —
    // the plan's line count is not a row count, and `execute` returns the tag's
    // trailing integer, so a count here would be reported as affected rows.
    assert_eq!(client.execute("EXPLAIN SELECT * FROM t", &[]).await?, 0);
    // Most visibly for a DML ANALYZE, which really does write.
    assert_eq!(
        client
            .execute("EXPLAIN ANALYZE INSERT INTO t VALUES (1)", &[])
            .await?,
        0
    );
    assert_eq!(row_count(&client, "t").await, 1);

    Ok(())
}

#[tokio::test]
async fn explain_analyze_resolves_bind_parameters_in_extended_protocol() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id int PRIMARY KEY)")
        .await?;
    client.simple_query("INSERT INTO t VALUES (1), (2)").await?;

    // A prepared EXPLAIN ANALYZE infers `$1` from the inner statement at Describe,
    // then runs with the bound value at Execute.
    let rows = client
        .query("EXPLAIN ANALYZE SELECT * FROM t WHERE id = $1", &[&2i32])
        .await?;
    let lines: Vec<&str> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(lines[0], "Index Scan using t_pkey on t");
    assert_eq!(lines[1], "  Index Cond: (id = 2)");
    assert!(
        lines.iter().any(|l| l.starts_with("Execution Time: ")),
        "plan was {lines:?}"
    );

    // An option we cannot honor is rejected when the portal executes, which is
    // where PG raises its own option errors — Parse and Describe stay clean, so a
    // driver that only prepares the statement does not abort its transaction block.
    let err = client
        .query("EXPLAIN (FORMAT JSON) SELECT * FROM t WHERE id = $1", &[&2i32])
        .await
        .expect_err("FORMAT JSON should be rejected");
    assert_eq!(
        err.as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::FEATURE_NOT_SUPPORTED
    );

    Ok(())
}

/// A temp table reflects its own `pg_temp_N` namespace: `pg_class.relnamespace`
/// joins to a `pg_namespace` row named `pg_temp_N` (not `public`/2200), even though
/// the temp schema is never persisted.
#[tokio::test]
async fn temp_table_reflects_its_own_namespace() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TEMP TABLE t (id int)").await?;

    let joined = client
        .simple_query(
            "SELECT n.nspname FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relname = 't'",
        )
        .await?;
    let nspname = rows(&joined)[0].get("nspname").expect("nspname");
    assert!(
        nspname.starts_with("pg_temp_"),
        "temp table relnamespace should resolve to pg_temp_N, got {nspname:?}"
    );

    let listed = client
        .simple_query("SELECT nspname FROM pg_namespace WHERE nspname LIKE 'pg_temp_%'")
        .await?;
    assert_eq!(rows(&listed).len(), 1, "pg_namespace should list the temp schema");

    Ok(())
}

/// A session cannot reach another session's temp tables by qualifying with the
/// other backend's `pg_temp_N` namespace — temp tables all live in the one shared
/// engine now, so this guards the isolation the old per-session engine gave for
/// free.
#[tokio::test]
async fn temp_tables_are_not_reachable_across_sessions() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let a = connect(port).await;
    a.simple_query("CREATE TEMP TABLE secret (x int)").await?;
    a.simple_query("INSERT INTO secret VALUES (42)").await?;
    // Learn A's temp namespace (pg_temp_N) from information_schema.
    let ns = a
        .simple_query("SELECT table_schema FROM information_schema.tables WHERE table_name = 'secret'")
        .await?;
    let a_temp_schema = rows(&ns)[0].get("table_schema").unwrap().to_string();
    assert!(a_temp_schema.starts_with("pg_temp_"));

    // A second session must not read, write, or drop A's temp table by qualifier.
    let b = connect(port).await;
    for stmt in [
        format!("SELECT * FROM {a_temp_schema}.secret"),
        format!("INSERT INTO {a_temp_schema}.secret VALUES (99)"),
        format!("DROP TABLE {a_temp_schema}.secret"),
    ] {
        let err = b.simple_query(&stmt).await.unwrap_err();
        assert_eq!(
            err.as_db_error().expect("db error").code(),
            &tokio_postgres::error::SqlState::UNDEFINED_TABLE,
            "cross-session access should be rejected: {stmt}"
        );
    }
    // A's data is intact.
    let still = a.simple_query("SELECT x FROM secret").await?;
    assert_eq!(rows(&still)[0].get("x"), Some("42"));

    Ok(())
}

/// `CREATE UNLOGGED TABLE ... AS SELECT` must also produce an UNLOGGED table
/// (`relpersistence = 'u'`), not silently fall back to a Permanent heap.
#[tokio::test]
async fn unlogged_create_table_as_is_unlogged() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE src (id int4)").await?;
    client.simple_query("INSERT INTO src VALUES (1), (2)").await?;
    client
        .simple_query("CREATE UNLOGGED TABLE u AS SELECT * FROM src")
        .await?;
    let reflected = client
        .simple_query("SELECT relpersistence FROM pg_class WHERE relname = 'u'")
        .await?;
    assert_eq!(rows(&reflected)[0].get("relpersistence"), Some("u"));
    Ok(())
}

/// `CREATE UNLOGGED TABLE` reads and writes like any table, reflects
/// `relpersistence = 'u'` in `pg_class`, and — unlike a TEMP table — is a shared
/// relation visible to other sessions (it is on-disk and WAL-skipped, not
/// session-local).
#[tokio::test]
async fn unlogged_table_crud_reflection_and_cross_session() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let client = connect(port).await;
    client
        .simple_query("CREATE UNLOGGED TABLE u (id int4, label text)")
        .await?;
    client
        .simple_query("INSERT INTO u VALUES (1, 'one'), (2, 'two')")
        .await?;
    let selected = client
        .simple_query("SELECT label FROM u ORDER BY id")
        .await?;
    let labels = rows(&selected);
    assert_eq!(labels[0].get("label"), Some("one"));
    assert_eq!(labels[1].get("label"), Some("two"));

    // Reflected as an unlogged relation, distinct from a permanent one.
    client.simple_query("CREATE TABLE p (id int4)").await?;
    let unlogged = client
        .simple_query("SELECT relpersistence FROM pg_class WHERE relname = 'u'")
        .await?;
    assert_eq!(rows(&unlogged)[0].get("relpersistence"), Some("u"));
    let permanent = client
        .simple_query("SELECT relpersistence FROM pg_class WHERE relname = 'p'")
        .await?;
    assert_eq!(rows(&permanent)[0].get("relpersistence"), Some("p"));

    // An UNLOGGED table is shared: a second session sees it and its rows (a TEMP
    // table would be invisible cross-session).
    let other = connect(port).await;
    let seen = other.simple_query("SELECT label FROM u ORDER BY id").await?;
    assert_eq!(rows(&seen)[0].get("label"), Some("one"));

    Ok(())
}

#[tokio::test]
async fn explain_resolves_bind_parameters_in_extended_protocol() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id int PRIMARY KEY, label text)")
        .await?;
    client
        .simple_query("INSERT INTO t VALUES (1, 'one'), (2, 'two')")
        .await?;

    // The extended-protocol path (a query with $1) must resolve the inner
    // statement's parameter at Describe and not error at Execute.
    let rows = client
        .query("EXPLAIN SELECT * FROM t WHERE id = $1", &[&2i32])
        .await?;
    let plan: String = rows[0].get(0);
    assert_eq!(plan, "Index Scan using t_pkey on t");

    Ok(())
}

#[tokio::test]
async fn time_plus_time_reports_ambiguous_operator_over_the_wire() -> anyhow::Result<()> {
    use tokio_postgres::error::{ErrorPosition, SqlState};

    let client = connect(spawn_server().await).await;
    let err = client
        .simple_query("SELECT time '00:01' + time '00:02'")
        .await
        .unwrap_err();
    let db = err.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::AMBIGUOUS_FUNCTION);
    assert_eq!(
        db.message(),
        "operator is not unique: time without time zone + time without time zone"
    );
    assert_eq!(db.detail(), Some("Could not choose a best candidate operator."));
    assert_eq!(db.hint(), Some("You might need to add explicit type casts."));
    // Cursor points at the `+` (1-based character 21).
    assert!(matches!(db.position(), Some(ErrorPosition::Original(21))));

    Ok(())
}

/// A view is a stored query: `SELECT` expands it, an explicit column list renames
/// its output, and it reflects into `pg_class` (`relkind='v'`) and
/// `information_schema.tables` (`VIEW`).
#[tokio::test]
async fn views_expand_and_reflect_into_catalog() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id int4, name text)")
        .await?;
    client
        .simple_query("INSERT INTO t VALUES (1, 'a'), (2, 'b')")
        .await?;
    client
        .simple_query("CREATE VIEW v AS SELECT id, name FROM t WHERE id = 1")
        .await?;

    let messages = client.simple_query("SELECT id, name FROM v").await?;
    let result = rows(&messages);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].get("id"), Some("1"));
    assert_eq!(result[0].get("name"), Some("a"));

    // Explicit column list renames the outputs.
    client
        .simple_query("CREATE VIEW v2 (label) AS SELECT name FROM t")
        .await?;
    let messages = client
        .simple_query("SELECT label FROM v2 ORDER BY label")
        .await?;
    let result = rows(&messages);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].get("label"), Some("a"));

    // A view over a view expands transitively.
    client
        .simple_query("CREATE VIEW v3 AS SELECT id FROM v")
        .await?;
    let messages = client.simple_query("SELECT id FROM v3").await?;
    assert_eq!(rows(&messages).len(), 1);

    // Catalog reflection: relkind='v' and information_schema table_type='VIEW'.
    let messages = client
        .simple_query("SELECT relkind FROM pg_class WHERE relname = 'v'")
        .await?;
    assert_eq!(rows(&messages)[0].get("relkind"), Some("v"));
    let messages = client
        .simple_query(
            "SELECT table_type FROM information_schema.tables WHERE table_name = 'v'",
        )
        .await?;
    assert_eq!(rows(&messages)[0].get("table_type"), Some("VIEW"));

    Ok(())
}

/// `CREATE OR REPLACE VIEW` swaps the definition (and may only add trailing
/// columns); `IF NOT EXISTS` and a plain re-create resolve name collisions.
#[tokio::test]
async fn create_view_replace_and_collisions() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id int4)").await?;
    client.simple_query("INSERT INTO t VALUES (1), (2)").await?;
    client
        .simple_query("CREATE VIEW v AS SELECT id FROM t WHERE id = 1")
        .await?;

    // OR REPLACE swaps the body; the row set changes.
    client
        .simple_query("CREATE OR REPLACE VIEW v AS SELECT id FROM t")
        .await?;
    let messages = client.simple_query("SELECT id FROM v").await?;
    assert_eq!(rows(&messages).len(), 2);

    // A plain re-create collides.
    let err = client
        .simple_query("CREATE VIEW v AS SELECT 1")
        .await
        .unwrap_err();
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::DUPLICATE_TABLE);
    assert_eq!(db.message(), "relation \"v\" already exists");

    // IF NOT EXISTS is a no-op (skips) when the view exists.
    client
        .simple_query("CREATE VIEW IF NOT EXISTS v AS SELECT 1")
        .await?;

    // A table cannot collide with a view name either.
    let err = client
        .simple_query("CREATE TABLE v (x int4)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("db error").code(),
        &SqlState::DUPLICATE_TABLE
    );

    // OR REPLACE that drops a column is rejected.
    client
        .simple_query("CREATE VIEW two AS SELECT id, id AS id2 FROM t")
        .await?;
    let err = client
        .simple_query("CREATE OR REPLACE VIEW two AS SELECT id FROM t")
        .await
        .unwrap_err();
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.message(), "cannot drop columns from view");

    Ok(())
}

/// DROP VIEW honors object type, IF EXISTS, and dependency tracking; a table with
/// a dependent view refuses RESTRICT and cascades under CASCADE.
#[tokio::test]
async fn drop_view_object_type_and_dependencies() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id int4)").await?;
    client
        .simple_query("CREATE VIEW v AS SELECT id FROM t")
        .await?;

    // Wrong object type both ways.
    let err = client.simple_query("DROP TABLE v").await.unwrap_err();
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::WRONG_OBJECT_TYPE);
    assert_eq!(db.message(), "\"v\" is not a table");
    assert_eq!(db.hint(), Some("Use DROP VIEW to remove a view."));

    let err = client.simple_query("DROP VIEW t").await.unwrap_err();
    assert_eq!(
        err.as_db_error().expect("db error").message(),
        "\"t\" is not a view"
    );

    // RESTRICT: the table refuses to drop while the view depends on it.
    let err = client.simple_query("DROP TABLE t").await.unwrap_err();
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::DEPENDENT_OBJECTS_STILL_EXIST);
    assert_eq!(
        db.message(),
        "cannot drop table t because other objects depend on it"
    );
    assert_eq!(db.detail(), Some("view v depends on table t"));

    // CASCADE drops the table and its dependent view.
    client.simple_query("DROP TABLE t CASCADE").await?;
    let err = client.simple_query("SELECT id FROM v").await.unwrap_err();
    assert_eq!(
        err.as_db_error().expect("db error").code(),
        &SqlState::UNDEFINED_TABLE
    );

    // DROP VIEW IF EXISTS on a missing view succeeds (skips).
    client.simple_query("DROP VIEW IF EXISTS v").await?;
    Ok(())
}

/// A view is not automatically updatable: writes through it are rejected.
#[tokio::test]
async fn views_are_not_updatable() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id int4)").await?;
    client
        .simple_query("CREATE VIEW v AS SELECT id FROM t")
        .await?;

    let err = client
        .simple_query("INSERT INTO v VALUES (1)")
        .await
        .unwrap_err();
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::FEATURE_NOT_SUPPORTED);
    assert_eq!(db.message(), "cannot insert into view \"v\"");

    let err = client
        .simple_query("UPDATE v SET id = 2")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("db error").message(),
        "cannot update view \"v\""
    );

    let err = client.simple_query("DELETE FROM v").await.unwrap_err();
    assert_eq!(
        err.as_db_error().expect("db error").message(),
        "cannot delete from view \"v\""
    );

    Ok(())
}

/// A view may (transitively) reference itself: PG permits creating it and errors
/// only when it is used. Expansion detects the cycle instead of recursing.
#[tokio::test]
async fn recursive_view_definition_errors_on_use_not_creation() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (a int4)").await?;
    client
        .simple_query("CREATE VIEW v2 AS SELECT a FROM t")
        .await?;
    client
        .simple_query("CREATE VIEW v3 AS SELECT a FROM v2")
        .await?;
    // Close the loop: v2 -> v3 -> v2. Creating it succeeds (as in PG).
    client
        .simple_query("CREATE OR REPLACE VIEW v2 AS SELECT a FROM v3")
        .await?;
    // Using it detects the cycle rather than overflowing the stack.
    let err = client.simple_query("SELECT a FROM v2").await.unwrap_err();
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::INVALID_OBJECT_DEFINITION);
    assert_eq!(
        db.message(),
        "infinite recursion detected in rules for relation \"v2\""
    );
    Ok(())
}

/// PG accepts a CREATE VIEW column list shorter than the query's output (the
/// trailing columns keep their derived names); only a longer list is an error.
#[tokio::test]
async fn create_view_accepts_fewer_column_names() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (a int4, b int4)").await?;
    client.simple_query("INSERT INTO t VALUES (1, 2)").await?;
    // One name for a two-column query: `a` is renamed to `x`, `b` keeps its name.
    client
        .simple_query("CREATE VIEW v (x) AS SELECT a, b FROM t")
        .await?;
    let messages = client.simple_query("SELECT x, b FROM v").await?;
    let result = rows(&messages);
    assert_eq!(result[0].get("x"), Some("1"));
    assert_eq!(result[0].get("b"), Some("2"));

    // More names than columns is still an error.
    let err = client
        .simple_query("CREATE VIEW w (p, q, r) AS SELECT a, b FROM t")
        .await
        .unwrap_err();
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::SYNTAX_ERROR);
    assert_eq!(db.message(), "CREATE VIEW specifies more column names than columns");
    Ok(())
}

/// A relation resolved through the search path (a temp table) shadows a
/// same-named permanent view, matching PG's `pg_temp`-before-`public` precedence.
#[tokio::test]
async fn temp_table_shadows_same_named_view() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE VIEW x AS SELECT 1 AS a").await?;
    client.simple_query("CREATE TEMP TABLE x (b int4)").await?;
    client.simple_query("INSERT INTO x VALUES (5)").await?;
    // The temp table wins; the row (and column name `b`) come from it, not the view.
    let messages = client.simple_query("SELECT b FROM x").await?;
    assert_eq!(rows(&messages)[0].get("b"), Some("5"));
    Ok(())
}

/// A serial column auto-assigns from an owned sequence, and the sequence reflects
/// into the catalogs as relkind 'S' / pg_sequence and is auto-dropped with the
/// table. Covers the headline `serial PRIMARY KEY` case end to end.
#[tokio::test]
async fn serial_column_and_sequence_reflection() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;

    client
        .simple_query("CREATE TABLE t (id serial PRIMARY KEY, name text)")
        .await?;
    client
        .simple_query("INSERT INTO t (name) VALUES ('a'), ('b')")
        .await?;
    client.simple_query("INSERT INTO t (name) VALUES ('c')").await?;
    let id_msgs = client.simple_query("SELECT id FROM t ORDER BY id").await?;
    let ids: Vec<Option<&str>> = rows(&id_msgs).iter().map(|r| r.get("id")).collect();
    assert_eq!(ids, vec![Some("1"), Some("2"), Some("3")]);

    // The owned sequence reflects as relkind 'S'.
    let msgs = client
        .simple_query("SELECT relkind FROM pg_class WHERE relname = 't_id_seq'")
        .await?;
    assert_eq!(rows(&msgs)[0].get("relkind"), Some("S"));

    // DROP TABLE auto-drops the owned sequence.
    client.simple_query("DROP TABLE t").await?;
    let msgs = client
        .simple_query("SELECT count(*) AS c FROM pg_class WHERE relname = 't_id_seq'")
        .await?;
    assert_eq!(rows(&msgs)[0].get("c"), Some("0"));

    // currval before nextval in a session is 55000.
    client.simple_query("CREATE SEQUENCE s").await?;
    let err = client.simple_query("SELECT currval('s')").await.unwrap_err();
    assert_eq!(
        err.as_db_error().expect("db error").code(),
        &SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE
    );
    Ok(())
}

/// Regression coverage for the sequence-semantics fixes: setval/lastval,
/// setval bounds + NULL strictness, CREATE SEQUENCE type-range validation,
/// read-only rejection, wrong-object-type, currval-after-drop, namespace
/// collisions, and DROP SEQUENCE dependency blocking. Asserts SQLSTATEs so it is
/// stable across PostgreSQL versions.
#[tokio::test]
async fn sequence_semantics_edge_cases() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;

    // setval does NOT define lastval; lastval reflects only the last nextval.
    client
        .batch_execute("CREATE SEQUENCE a; CREATE SEQUENCE b MINVALUE 1 MAXVALUE 10")
        .await?;
    let n = client.query_one("SELECT nextval('a') AS v", &[]).await?.get::<_, i64>("v");
    client.query_one("SELECT setval('b', 7) AS v", &[]).await?; // must not touch lastval
    assert_eq!(
        client.query_one("SELECT lastval() AS v", &[]).await?.get::<_, i64>("v"),
        n,
        "lastval must reflect the nextval on a, not the setval on b"
    );
    // ...but setval DOES define currval for its sequence.
    assert_eq!(
        client.query_one("SELECT currval('b') AS v", &[]).await?.get::<_, i64>("v"),
        7
    );

    // setval out of the sequence's own [min,max] is 22003.
    let e = client.query_one("SELECT setval('b', 999)", &[]).await.unwrap_err();
    assert_eq!(e.as_db_error().unwrap().code(), &SqlState::NUMERIC_VALUE_OUT_OF_RANGE);

    // setval with a NULL third argument is a NULL no-op (no side effect).
    let is_null: bool = client
        .query_one("SELECT setval('b', 3, NULL) IS NULL AS n", &[])
        .await?
        .get("n");
    assert!(is_null);
    // currval still 7 (the NULL setval did nothing).
    assert_eq!(
        client.query_one("SELECT currval('b') AS v", &[]).await?.get::<_, i64>("v"),
        7
    );

    // CREATE SEQUENCE with a bound outside the declared type is 22023.
    let e = client
        .batch_execute("CREATE SEQUENCE toobig AS smallint MAXVALUE 100000")
        .await
        .unwrap_err();
    assert_eq!(e.as_db_error().unwrap().code(), &SqlState::INVALID_PARAMETER_VALUE);

    // nextval on a table (existing non-sequence relation) is 42809, not 42P01.
    client.batch_execute("CREATE TABLE tab (id int)").await?;
    let e = client.query_one("SELECT nextval('tab')", &[]).await.unwrap_err();
    assert_eq!(e.as_db_error().unwrap().code(), &SqlState::WRONG_OBJECT_TYPE);

    // currval after DROP errors 42P01 (no stale cached value).
    client.batch_execute("CREATE SEQUENCE gone; ").await?;
    client.query_one("SELECT nextval('gone') AS v", &[]).await?;
    client.batch_execute("DROP SEQUENCE gone").await?;
    let e = client.query_one("SELECT currval('gone')", &[]).await.unwrap_err();
    assert_eq!(e.as_db_error().unwrap().code(), &SqlState::UNDEFINED_TABLE);

    // An index cannot take a sequence's name (shared relation namespace).
    let e = client
        .batch_execute("CREATE INDEX a ON tab (id)")
        .await
        .unwrap_err();
    assert_eq!(e.as_db_error().unwrap().code(), &SqlState::DUPLICATE_TABLE);

    // DROP SEQUENCE of a serial-owned sequence is blocked under RESTRICT (2BP01).
    client.batch_execute("CREATE TABLE ser (id serial)").await?;
    let e = client.batch_execute("DROP SEQUENCE ser_id_seq").await.unwrap_err();
    assert_eq!(
        e.as_db_error().unwrap().code(),
        &SqlState::DEPENDENT_OBJECTS_STILL_EXIST
    );
    // CASCADE drops it.
    client.batch_execute("DROP SEQUENCE ser_id_seq CASCADE").await?;
    Ok(())
}

/// DROP INDEX: reflection round-trip, IF EXISTS skip, wrong-object-type, the
/// constraint-backing-index block, and multi-name atomic dedup. SQLSTATEs are
/// asserted so the test is stable across PostgreSQL versions.
#[tokio::test]
async fn drop_index_semantics() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;

    client
        .batch_execute("CREATE TABLE t (id int PRIMARY KEY, a int); CREATE INDEX t_a_idx ON t(a)")
        .await?;

    // The PK and the explicit index are both reflected as relkind='i'.
    let count = |msgs: &[tokio_postgres::SimpleQueryMessage]| -> String {
        rows(msgs)[0].get(0).unwrap_or_default().to_string()
    };
    let msgs = client
        .simple_query("SELECT count(*) FROM pg_class WHERE relkind = 'i'")
        .await?;
    assert_eq!(count(&msgs), "2");

    // A table name is not an index (42809).
    let e = client.batch_execute("DROP INDEX t").await.unwrap_err();
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::WRONG_OBJECT_TYPE
    );

    // A constraint-backing index cannot be dropped directly (2BP01).
    let e = client.batch_execute("DROP INDEX t_pkey").await.unwrap_err();
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::DEPENDENT_OBJECTS_STILL_EXIST
    );

    // A missing index is 42704 (UNDEFINED_OBJECT) — PG uses that for indexes,
    // not the 42P01 it uses for tables. IF EXISTS turns it into a skip NOTICE.
    let e = client.batch_execute("DROP INDEX nope").await.unwrap_err();
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_OBJECT
    );
    client.batch_execute("DROP INDEX IF EXISTS nope").await?;

    // The real drop removes it from the catalog and frees the name for reuse.
    client.batch_execute("DROP INDEX t_a_idx").await?;
    let msgs = client
        .simple_query("SELECT count(*) FROM pg_class WHERE relkind = 'i'")
        .await?;
    assert_eq!(count(&msgs), "1");
    client.batch_execute("CREATE INDEX t_a_idx ON t(a)").await?;

    // A name listed twice in one statement is rejected up front (42710).
    let e = client
        .batch_execute("DROP INDEX t_a_idx, t_a_idx")
        .await
        .unwrap_err();
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::DUPLICATE_OBJECT
    );
    Ok(())
}

/// DROP FUNCTION: resolves an overload by signature, the bare-name unambiguous
/// and ambiguous (42725) forms, missing (42883) with IF EXISTS skip, and that a
/// dropped signature frees its catalog slot for re-creation.
#[tokio::test]
async fn drop_function_semantics() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;

    // Two overloads of the same name (LANGUAGE internal is the only supported
    // form; the declared signature need not match the builtin it wraps).
    client
        .batch_execute(
            "CREATE FUNCTION f_in(cstring) RETURNS int8 AS 'int8in' LANGUAGE internal; \
             CREATE FUNCTION f_in(int8) RETURNS cstring AS 'int8out' LANGUAGE internal",
        )
        .await?;

    // A bare name with two overloads is ambiguous (42725).
    let e = client.batch_execute("DROP FUNCTION f_in").await.unwrap_err();
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::AMBIGUOUS_FUNCTION
    );

    // The same function named twice (a bare name and its signature) is rejected
    // as specified-more-than-once (42710), not silently accepted.
    let e = client
        .batch_execute("DROP FUNCTION f_in(int8), f_in(int8)")
        .await
        .unwrap_err();
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::DUPLICATE_OBJECT
    );

    // Dropping one overload by its argument list leaves the other; the bare name
    // is then unambiguous and drops it.
    client.batch_execute("DROP FUNCTION f_in(int8)").await?;
    client.batch_execute("DROP FUNCTION f_in").await?;

    // Both are gone now: dropping a missing signature is 42883.
    let e = client
        .batch_execute("DROP FUNCTION f_in(cstring)")
        .await
        .unwrap_err();
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_FUNCTION
    );
    // IF EXISTS turns the miss into a skip NOTICE.
    client
        .batch_execute("DROP FUNCTION IF EXISTS f_in(cstring)")
        .await?;
    let e = client
        .batch_execute("DROP FUNCTION nosuch(int4)")
        .await
        .unwrap_err();
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_FUNCTION
    );

    // The dropped signature's catalog slot is free: re-creating it succeeds.
    client
        .batch_execute("CREATE FUNCTION f_in(cstring) RETURNS int8 AS 'int8in' LANGUAGE internal")
        .await?;

    // A function declared with an OUT parameter is droppable by its input
    // signature: OUT params are not part of a function's identity (CREATE and
    // DROP agree on excluding them).
    client
        .batch_execute("CREATE FUNCTION f_out(int8, OUT int4) RETURNS int4 AS 'int8out' LANGUAGE internal")
        .await?;
    client.batch_execute("DROP FUNCTION f_out(int8)").await?;
    Ok(())
}

/// DROP INDEX honors the session temp store: an index on a TEMP table can be
/// dropped, and a same-named temp table shadowing a permanent one does not cause
/// the drop of the permanent index to be misrouted (and silently lost).
#[tokio::test]
async fn drop_index_temp_and_shadowing() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    let present = |msgs: &[tokio_postgres::SimpleQueryMessage], name: &str| -> bool {
        rows(msgs).iter().any(|r| r.get(0) == Some(name))
    };

    // An index on a TEMP table reflects in pg_class and can be dropped.
    client
        .batch_execute("CREATE TEMP TABLE tt (a int); CREATE INDEX tt_idx ON tt(a)")
        .await?;
    let msgs = client
        .simple_query("SELECT relname FROM pg_class WHERE relkind = 'i'")
        .await?;
    assert!(present(&msgs, "tt_idx"), "temp index reflects in pg_class");
    client.batch_execute("DROP INDEX tt_idx").await?;
    let msgs = client
        .simple_query("SELECT relname FROM pg_class WHERE relkind = 'i'")
        .await?;
    assert!(!present(&msgs, "tt_idx"), "temp index is actually dropped");

    // Shadowing: a permanent table's index must still be dropped when a same-named
    // temp table shadows it — the drop must not be routed to the temp table.
    client
        .batch_execute(
            "CREATE TABLE sh (a int); CREATE INDEX sh_idx ON sh(a); CREATE TEMP TABLE sh (b int)",
        )
        .await?;
    client.batch_execute("DROP INDEX sh_idx").await?;
    let msgs = client
        .simple_query("SELECT count(*) FROM pg_class WHERE relname = 'sh_idx'")
        .await?;
    assert_eq!(
        rows(&msgs)[0].get(0),
        Some("0"),
        "the permanent index sh_idx must actually be dropped, not silently kept"
    );
    Ok(())
}

/// nextval() / setval() are rejected in a read-only transaction (25006), even
/// though a bare SELECT is not a DML write.
#[tokio::test]
async fn sequence_write_rejected_read_only() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;
    client.batch_execute("CREATE SEQUENCE s").await?;
    client.batch_execute("BEGIN TRANSACTION READ ONLY").await?;
    let e = client.query_one("SELECT nextval('s')", &[]).await.unwrap_err();
    assert_eq!(
        e.as_db_error().unwrap().code(),
        &SqlState::READ_ONLY_SQL_TRANSACTION
    );
    client.batch_execute("ROLLBACK").await?;
    // The counter did not advance despite the rejected nextval.
    assert_eq!(
        client.query_one("SELECT nextval('s') AS v", &[]).await?.get::<_, i64>("v"),
        1
    );
    Ok(())
}

// --------------------------------------------------------------------------
// COPY ... FROM STDIN
// --------------------------------------------------------------------------

/// The extended-protocol COPY path (tokio-postgres `copy_in` prepares the
/// statement, then streams CopyData / CopyDone): rows land with the right types,
/// NULL (`\N`) and defaults are honored, and the sink reports the row count.
#[tokio::test]
async fn copy_in_extended_loads_rows() -> anyhow::Result<()> {
    use bytes::Bytes;
    use futures_util::SinkExt;

    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (a int4, b text, c int4 DEFAULT 7)")
        .await?;

    // Column list omits `c`, which takes its default; `\N` is a SQL NULL.
    let sink = client
        .copy_in("COPY t (a, b) FROM STDIN")
        .await
        .context("copy_in should enter copy mode")?;
    futures_util::pin_mut!(sink);
    sink.send(Bytes::from_static(b"1\thello\n2\t\\N\n")).await?;
    let count = sink.finish().await?;
    assert_eq!(count, 2);

    let messages = client
        .simple_query("SELECT a, b, c FROM t ORDER BY a")
        .await?;
    let rows = rows(&messages);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get("a"), Some("1"));
    assert_eq!(rows[0].get("b"), Some("hello"));
    assert_eq!(rows[0].get("c"), Some("7"));
    assert_eq!(rows[1].get("a"), Some("2"));
    assert_eq!(rows[1].get("b"), None); // NULL
    assert_eq!(rows[1].get("c"), Some("7"));
    Ok(())
}

/// CSV format over the extended protocol: quoted fields with `""` doubling and
/// embedded delimiters, an unquoted empty field as NULL, and HEADER skipping.
#[tokio::test]
async fn copy_in_csv_with_header_and_quotes() -> anyhow::Result<()> {
    use bytes::Bytes;
    use futures_util::SinkExt;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE c (a int4, b text)").await?;

    let sink = client
        .copy_in("COPY c FROM STDIN WITH (FORMAT csv, HEADER)")
        .await?;
    futures_util::pin_mut!(sink);
    sink.send(Bytes::from_static(
        b"col_a,col_b\n1,\"a,b\"\n2,\"she \"\"said\"\"\"\n",
    ))
    .await?;
    let count = sink.finish().await?;
    assert_eq!(count, 2);

    let messages = client.simple_query("SELECT a, b FROM c ORDER BY a").await?;
    let rows = rows(&messages);
    assert_eq!(rows[0].get("b"), Some("a,b"));
    assert_eq!(rows[1].get("b"), Some("she \"said\""));
    Ok(())
}

/// A data-type error mid-COPY aborts the whole load (autocommit rollback): the
/// error surfaces with SQLSTATE 22P02 and no rows are left behind.
#[tokio::test]
async fn copy_in_bad_value_aborts_load() -> anyhow::Result<()> {
    use bytes::Bytes;
    use futures_util::SinkExt;
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (a int4)").await?;

    let sink = client.copy_in("COPY t FROM STDIN").await?;
    futures_util::pin_mut!(sink);
    sink.send(Bytes::from_static(b"1\nnot_an_int\n3\n")).await?;
    let err = sink.finish().await.unwrap_err();
    assert_eq!(
        err.as_db_error().expect("db error").code(),
        &SqlState::INVALID_TEXT_REPRESENTATION
    );

    // Nothing was committed.
    let messages = client.simple_query("SELECT count(*) AS n FROM t").await?;
    assert_eq!(rows(&messages)[0].get("n"), Some("0"));
    Ok(())
}

/// COPY into a missing relation errors before entering copy mode (no
/// CopyInResponse), so the driver surfaces `undefined_table` from `copy_in`.
#[tokio::test]
async fn copy_in_missing_table_errors_before_copy_mode() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    let result: Result<tokio_postgres::CopyInSink<bytes::Bytes>, _> =
        client.copy_in("COPY nope FROM STDIN").await;
    let err = result
        .err()
        .expect("copy_in into a missing table should error");
    assert_eq!(
        err.as_db_error().expect("db error").code(),
        &SqlState::UNDEFINED_TABLE
    );
    Ok(())
}

/// Regression tests for the code-review fixes: byte-oriented escape decoding,
/// CSV quote concatenation, single-byte options, and the aborted-transaction
/// guard — all matching PostgreSQL's observable behavior.
#[tokio::test]
async fn copy_in_octal_escapes_form_multibyte_utf8() -> anyhow::Result<()> {
    use bytes::Bytes;
    use futures_util::SinkExt;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE m (v text)").await?;
    let sink = client.copy_in("COPY m FROM STDIN").await?;
    futures_util::pin_mut!(sink);
    // The three UTF-8 bytes of 日 as octal escapes must round-trip to one char.
    sink.send(Bytes::from_static(b"\\346\\227\\245\n")).await?;
    sink.finish().await?;

    let messages = client
        .simple_query("SELECT v, octet_length(v) AS len FROM m")
        .await?;
    let rows = rows(&messages);
    assert_eq!(rows[0].get("v"), Some("日"));
    assert_eq!(rows[0].get("len"), Some("3"));
    Ok(())
}

#[tokio::test]
async fn copy_in_invalid_utf8_byte_errors() -> anyhow::Result<()> {
    use bytes::Bytes;
    use futures_util::SinkExt;
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE m (v text)").await?;
    let sink = client.copy_in("COPY m FROM STDIN").await?;
    futures_util::pin_mut!(sink);
    sink.send(Bytes::from_static(b"\\351\n")).await?; // 0xe9 alone: invalid UTF-8
    let err = sink.finish().await.unwrap_err();
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::CHARACTER_NOT_IN_REPERTOIRE);
    assert!(db.message().contains("0xe9"), "{}", db.message());
    Ok(())
}

#[tokio::test]
async fn copy_in_csv_concatenates_quote_after_content() -> anyhow::Result<()> {
    use bytes::Bytes;
    use futures_util::SinkExt;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE c (a int4, b text)").await?;
    let sink = client.copy_in("COPY c FROM STDIN WITH (FORMAT csv)").await?;
    futures_util::pin_mut!(sink);
    // `1, "two"` -> b = ' two' (space + quoted run), as PG concatenates.
    sink.send(Bytes::from_static(b"1, \"two\"\n")).await?;
    assert_eq!(sink.finish().await?, 1);

    let messages = client.simple_query("SELECT '['||b||']' AS b FROM c").await?;
    assert_eq!(rows(&messages)[0].get("b"), Some("[ two]"));
    Ok(())
}

#[tokio::test]
async fn copy_multibyte_delimiter_rejected() -> anyhow::Result<()> {
    // A multi-byte DELIMITER is rejected (the parser guards the single-char slot,
    // and the binder's single-byte check backs it up) rather than silently
    // splitting on a multi-byte character, matching PG.
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE c (a int4, b text)").await?;
    let result: Result<tokio_postgres::CopyInSink<bytes::Bytes>, _> = client
        .copy_in("COPY c FROM STDIN WITH (FORMAT csv, DELIMITER 'é')")
        .await;
    assert!(
        result.is_err(),
        "a multi-byte COPY delimiter must be rejected"
    );
    Ok(())
}

#[tokio::test]
async fn copy_in_rejected_in_aborted_transaction() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE z (a int4)").await?;
    client.batch_execute("BEGIN").await?;
    // Force the block into the failed state.
    let _ = client.simple_query("SELECT 1/0").await;
    // COPY must be refused before entering copy mode (no CopyInResponse), like any
    // other statement — so a plain simple query surfaces the error without hanging.
    let err = client
        .simple_query("COPY z FROM STDIN")
        .await
        .err()
        .expect("COPY in an aborted txn should error");
    assert_eq!(
        err.as_db_error().expect("db error").code(),
        &SqlState::IN_FAILED_SQL_TRANSACTION
    );
    client.batch_execute("ROLLBACK").await?;
    // Connection is still usable after rollback, and no row was loaded.
    let messages = client.simple_query("SELECT count(*) AS n FROM z").await?;
    assert_eq!(rows(&messages)[0].get("n"), Some("0"));
    Ok(())
}

/// COPY into a table with a `serial` column advances the owned sequence for the
/// omitted column (its `nextval()` default runs in the copy's exec context),
/// matching how INSERT fills a serial default.
#[tokio::test]
async fn copy_in_fills_serial_default_from_sequence() -> anyhow::Result<()> {
    use bytes::Bytes;
    use futures_util::SinkExt;

    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE s (id serial PRIMARY KEY, name text)")
        .await?;
    let sink = client.copy_in("COPY s (name) FROM STDIN").await?;
    futures_util::pin_mut!(sink);
    sink.send(Bytes::from_static(b"alice\nbob\n")).await?;
    assert_eq!(sink.finish().await?, 2);

    let messages = client
        .simple_query("SELECT id, name FROM s ORDER BY id")
        .await?;
    let rows = rows(&messages);
    assert_eq!(rows[0].get("id"), Some("1"));
    assert_eq!(rows[0].get("name"), Some("alice"));
    assert_eq!(rows[1].get("id"), Some("2"));
    assert_eq!(rows[1].get("name"), Some("bob"));
    Ok(())
}

#[tokio::test]
async fn create_function_language_sql_evaluates_and_composes() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    // The `AS $$ SELECT ... $$` body form and a direct call.
    client
        .simple_query("CREATE FUNCTION add(int, int) RETURNS int LANGUAGE SQL AS $$ SELECT $1 + $2 $$")
        .await?;
    let out = client.simple_query("SELECT add(1, 2)").await?;
    assert_eq!(rows(&out)[0].get(0), Some("3"));

    // The extended protocol: the outer statement's `$1`/`$2` are the call
    // arguments, distinct from the body's own (now inlined) parameters.
    let row = client.query_one("SELECT add($1, $2)", &[&5i32, &7i32]).await?;
    assert_eq!(row.get::<_, i32>(0), 12);

    // The `RETURN <expr>` body form.
    client
        .simple_query("CREATE FUNCTION inc(int) RETURNS int LANGUAGE SQL RETURN $1 + 1")
        .await?;
    let out = client.simple_query("SELECT inc(41)").await?;
    assert_eq!(rows(&out)[0].get(0), Some("42"));

    // Functions compose: a body may call another SQL function, and arguments are
    // arbitrary expressions evaluated in the caller.
    client
        .simple_query("CREATE FUNCTION double_inc(int) RETURNS int LANGUAGE SQL AS $$ SELECT inc(inc($1)) $$")
        .await?;
    let out = client.simple_query("SELECT double_inc(40), add(inc(1), 5)").await?;
    assert_eq!(rows(&out)[0].get(0), Some("42"));
    assert_eq!(rows(&out)[0].get(1), Some("7"));

    // Return-type coercion: an int body widens to the declared bigint return.
    client
        .simple_query("CREATE FUNCTION widen(int) RETURNS bigint LANGUAGE SQL AS $$ SELECT $1 $$")
        .await?;
    let out = client.simple_query("SELECT widen(5)").await?;
    assert_eq!(rows(&out)[0].get(0), Some("5"));

    // Overloading by argument type: same name, different signature.
    client
        .simple_query("CREATE FUNCTION same(int) RETURNS int LANGUAGE SQL AS $$ SELECT $1 * 10 $$")
        .await?;
    client
        .simple_query("CREATE FUNCTION same(text) RETURNS text LANGUAGE SQL AS $$ SELECT $1 || '!' $$")
        .await?;
    let out = client.simple_query("SELECT same(4), same('hi')").await?;
    assert_eq!(rows(&out)[0].get(0), Some("40"));
    assert_eq!(rows(&out)[0].get(1), Some("hi!"));

    // A function used per row over a table.
    client.simple_query("CREATE TABLE t (a int, b int)").await?;
    client.simple_query("INSERT INTO t VALUES (1, 2), (3, 4), (10, 20)").await?;
    let out = client
        .simple_query("SELECT add(a, b) AS s FROM t ORDER BY a")
        .await?;
    let out = rows(&out);
    assert_eq!(out[0].get("s"), Some("3"));
    assert_eq!(out[1].get("s"), Some("7"));
    assert_eq!(out[2].get("s"), Some("30"));

    Ok(())
}

#[tokio::test]
async fn create_function_language_sql_reports_errors_like_pg() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;

    // A `$n` past the declared argument list has no value to bind, and the error
    // names the parameter actually referenced (not argcount+1).
    let err = client
        .simple_query("CREATE FUNCTION bad(int) RETURNS int LANGUAGE SQL AS $$ SELECT $1 + $9 $$")
        .await
        .unwrap_err();
    let dberr = err.as_db_error().expect("database error");
    assert_eq!(dberr.code(), &SqlState::UNDEFINED_PARAMETER);
    assert_eq!(dberr.message(), "there is no parameter $9");

    // A body whose type is not assignable to the declared return type.
    let err = client
        .simple_query("CREATE FUNCTION badret(int) RETURNS bool LANGUAGE SQL AS $$ SELECT $1 $$")
        .await
        .unwrap_err();
    let dberr = err.as_db_error().expect("database error");
    assert_eq!(dberr.code(), &SqlState::INVALID_FUNCTION_DEFINITION);
    assert_eq!(
        dberr.message(),
        "return type mismatch in function declared to return boolean"
    );
    assert_eq!(dberr.detail(), Some("Actual return type is integer."));

    // An unknown function referenced in a body is rejected at CREATE time.
    let err = client
        .simple_query("CREATE FUNCTION nested(int) RETURNS int LANGUAGE SQL AS $$ SELECT nope($1) $$")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_FUNCTION
    );

    // Only scalar, FROM-less bodies are supported for now.
    client.simple_query("CREATE TABLE t (a int)").await?;
    let err = client
        .simple_query("CREATE FUNCTION scan() RETURNS int LANGUAGE SQL AS $$ SELECT a FROM t $$")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::FEATURE_NOT_SUPPORTED
    );

    // A duplicate name+signature is rejected.
    client
        .simple_query("CREATE FUNCTION dup(int) RETURNS int LANGUAGE SQL AS $$ SELECT $1 $$")
        .await?;
    let err = client
        .simple_query("CREATE FUNCTION dup(int) RETURNS int LANGUAGE SQL AS $$ SELECT $1 + 1 $$")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::DUPLICATE_FUNCTION
    );

    // Calling with a wrong argument count finds no matching overload.
    let err = client.simple_query("SELECT dup(1, 2)").await.unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_FUNCTION
    );

    Ok(())
}

#[tokio::test]
async fn create_function_language_sql_resolution_and_volatility_match_pg() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE SEQUENCE s").await?;

    // A volatile argument (nextval) referenced more than once in the body would be
    // evaluated once per occurrence if inlined; refuse it rather than diverge from
    // PostgreSQL (which evaluates each argument once).
    client
        .simple_query(
            "CREATE FUNCTION twice(bigint) RETURNS bigint LANGUAGE SQL AS $$ SELECT $1 + $1 $$",
        )
        .await?;
    let err = client
        .simple_query("SELECT twice(nextval('s'))")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::FEATURE_NOT_SUPPORTED
    );
    // A non-volatile argument used twice is fine (double work, same result), and a
    // volatile argument referenced once is fine (evaluated once).
    let out = client.simple_query("SELECT twice(21)").await?;
    assert_eq!(rows(&out)[0].get(0), Some("42"));
    client
        .simple_query(
            "CREATE FUNCTION once(bigint) RETURNS bigint LANGUAGE SQL AS $$ SELECT $1 + 1 $$",
        )
        .await?;
    let out = client.simple_query("SELECT once(nextval('s'))").await?;
    assert_eq!(rows(&out)[0].get(0), Some("2")); // nextval->1, +1 = 2 (advanced once)

    // Overloading: an exact match resolves; two equally-coercible overloads are
    // ambiguous (42725), as in PostgreSQL.
    client
        .simple_query("CREATE FUNCTION g(bigint) RETURNS int LANGUAGE SQL AS $$ SELECT 1 $$")
        .await?;
    client
        .simple_query("CREATE FUNCTION g(numeric) RETURNS int LANGUAGE SQL AS $$ SELECT 2 $$")
        .await?;
    let out = client.simple_query("SELECT g(1::bigint)").await?;
    assert_eq!(rows(&out)[0].get(0), Some("1")); // exact bigint overload
    let err = client.simple_query("SELECT g(1::int)").await.unwrap_err();
    let dberr = err.as_db_error().expect("database error");
    assert_eq!(dberr.code(), &SqlState::AMBIGUOUS_FUNCTION);
    assert_eq!(dberr.message(), "function g(integer) is not unique");
    assert_eq!(
        dberr.hint(),
        Some("You might need to add explicit type casts.")
    );

    // An aggregate body is a scalar-inlining limitation, reported as unsupported
    // (PostgreSQL accepts `SELECT sum(1)`), not a grouping error.
    let err = client
        .simple_query("CREATE FUNCTION agg() RETURNS bigint LANGUAGE SQL AS $$ SELECT sum(1) $$")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::FEATURE_NOT_SUPPORTED
    );

    Ok(())
}

#[tokio::test]
async fn range_partitioning_ddl_and_catalog_reflection() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;

    client
        .simple_query("CREATE TABLE m (id int, d date) PARTITION BY RANGE (d)")
        .await?;
    client
        .simple_query(
            "CREATE TABLE m_2024 PARTITION OF m FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')",
        )
        .await?;

    // Reflection: parent is relkind='p', partition is relispartition='t'.
    let msgs = client
        .simple_query(
            "SELECT relkind, relispartition FROM pg_class WHERE relname = 'm'",
        )
        .await?;
    assert_eq!(
        (rows(&msgs)[0].get(0), rows(&msgs)[0].get(1)),
        (Some("p"), Some("f"))
    );
    let msgs = client
        .simple_query("SELECT relispartition FROM pg_class WHERE relname = 'm_2024'")
        .await?;
    assert_eq!(rows(&msgs)[0].get(0), Some("t"));

    // A row goes into the partition directly and reads back.
    client
        .simple_query("INSERT INTO m_2024 VALUES (1, '2024-06-01')")
        .await?;
    let msgs = client.simple_query("SELECT id FROM m_2024").await?;
    assert_eq!(rows(&msgs)[0].get(0), Some("1"));

    // An INSERT through the parent routes each row to the leaf whose range admits
    // its key: '2024-07-01' lands in m_2024, so the parent now holds two rows.
    client
        .simple_query("INSERT INTO m VALUES (2, '2024-07-01')")
        .await?;
    let msgs = client.simple_query("SELECT id FROM m_2024 ORDER BY id").await?;
    let leaf_ids: Vec<_> = rows(&msgs).iter().map(|r| r.get(0)).collect();
    assert_eq!(leaf_ids, vec![Some("1"), Some("2")]);

    // A SELECT from the parent unions its partitions.
    let msgs = client.simple_query("SELECT id FROM m ORDER BY id").await?;
    let parent_ids: Vec<_> = rows(&msgs).iter().map(|r| r.get(0)).collect();
    assert_eq!(parent_ids, vec![Some("1"), Some("2")]);

    // EXPLAIN of the parent shows an Append with one Seq Scan per partition.
    let plan = client.simple_query("EXPLAIN SELECT * FROM m").await?;
    let lines: Vec<&str> = rows(&plan).iter().filter_map(|r| r.get(0)).collect();
    assert_eq!(lines[0], "Append");
    assert!(
        lines.iter().any(|l| l.contains("Seq Scan on m_2024")),
        "plan was {lines:?}"
    );

    // A key admitted by no partition is rejected (23514), and nothing is written.
    let err = client
        .simple_query("INSERT INTO m VALUES (3, '2020-01-01')")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::CHECK_VIOLATION
    );
    assert_eq!(rows(&client.simple_query("SELECT count(*) FROM m").await?)[0].get(0), Some("2"));

    // DELETE through the parent removes the matching row from whichever leaf
    // holds it (id = 1 lived in m_2024), leaving id = 2 behind.
    client.simple_query("DELETE FROM m WHERE id = 1").await?;
    let msgs = client.simple_query("SELECT id FROM m ORDER BY id").await?;
    let remaining: Vec<_> = rows(&msgs).iter().map(|r| r.get(0)).collect();
    assert_eq!(remaining, vec![Some("2")]);

    // Unsupported strategies and bad bounds report the right SQLSTATEs.
    let err = client
        .simple_query("CREATE TABLE l (id int) PARTITION BY LIST (id)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::FEATURE_NOT_SUPPORTED
    );
    let err = client
        .simple_query(
            "CREATE TABLE m_ov PARTITION OF m FOR VALUES FROM ('2024-06-01') TO ('2024-07-01')",
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::from_code("42P17")
    );
    let err = client
        .simple_query("CREATE TABLE plain (id int)")
        .await
        .and(
            client
                .simple_query("CREATE TABLE p2 PARTITION OF plain FOR VALUES FROM (1) TO (2)")
                .await,
        )
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::WRONG_OBJECT_TYPE
    );

    Ok(())
}

#[tokio::test]
async fn range_partitioning_error_paths_and_cascade() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE m (id int, d date) PARTITION BY RANGE (d)")
        .await?;

    // A NULL bound is rejected (42P17), not a crash. The connection stays usable.
    let err = client
        .simple_query("CREATE TABLE mn PARTITION OF m FOR VALUES FROM (NULL) TO ('2024-01-01')")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::from_code("42P17")
    );
    assert_eq!(rows(&client.simple_query("SELECT 1").await?)[0].get(0), Some("1"));

    // A non-orderable RANGE key (json) is rejected at parent create (42704), not a crash.
    let err = client
        .simple_query("CREATE TABLE jm (j json) PARTITION BY RANGE (j)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_OBJECT
    );
    assert_eq!(rows(&client.simple_query("SELECT 2").await?)[0].get(0), Some("2"));

    // A duplicate partition name is 'relation already exists' (42P07), not a self-overlap;
    // IF NOT EXISTS is a no-op.
    client
        .simple_query("CREATE TABLE m_2024 PARTITION OF m FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')")
        .await?;
    let err = client
        .simple_query("CREATE TABLE m_2024 PARTITION OF m FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::DUPLICATE_TABLE
    );
    client
        .simple_query("CREATE TABLE IF NOT EXISTS m_2024 PARTITION OF m FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')")
        .await?;

    // TRUNCATE / CREATE INDEX on the parent are rejected (0A000), not applied to
    // the empty parent relation.
    let err = client.simple_query("TRUNCATE m").await.unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::FEATURE_NOT_SUPPORTED
    );
    let err = client
        .simple_query("CREATE INDEX m_idx ON m (d)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::FEATURE_NOT_SUPPORTED
    );

    // PARTITION OF a view reports wrong-object-type (42809), not 'does not exist'.
    client.simple_query("CREATE VIEW vv AS SELECT 1 AS x").await?;
    let err = client
        .simple_query("CREATE TABLE cv PARTITION OF vv FOR VALUES FROM (1) TO (2)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::WRONG_OBJECT_TYPE
    );

    // DROP TABLE on the parent cascades to its partitions (no CASCADE needed).
    client.simple_query("DROP TABLE m").await?;
    let msgs = client
        .simple_query("SELECT count(*) FROM pg_class WHERE relname IN ('m', 'm_2024')")
        .await?;
    assert_eq!(rows(&msgs)[0].get(0), Some("0"));

    Ok(())
}

#[tokio::test]
async fn range_partitioning_enforces_leaf_bounds() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE m (id int, d date) PARTITION BY RANGE (d)")
        .await?;
    client
        .simple_query(
            "CREATE TABLE m_2024 PARTITION OF m FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')",
        )
        .await?;

    // A key inside [from, to) inserts directly into the leaf; the lower bound is
    // inclusive, so exactly '2024-01-01' is admitted.
    client
        .simple_query("INSERT INTO m_2024 VALUES (1, '2024-06-01')")
        .await?;
    client
        .simple_query("INSERT INTO m_2024 VALUES (2, '2024-01-01')")
        .await?;

    // A key below the leaf's range is rejected (23514), with PG's message and
    // the failing row (all columns, in schema order) in the DETAIL line.
    let err = client
        .simple_query("INSERT INTO m_2024 VALUES (3, '2023-03-01')")
        .await
        .unwrap_err();
    let db = err.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::CHECK_VIOLATION);
    assert_eq!(
        db.message(),
        "new row for relation \"m_2024\" violates partition constraint"
    );
    assert_eq!(db.detail(), Some("Failing row contains (3, 2023-03-01)."));

    // The upper bound is exclusive: exactly '2025-01-01' is rejected.
    let err = client
        .simple_query("INSERT INTO m_2024 VALUES (4, '2025-01-01')")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::CHECK_VIOLATION
    );

    // A NULL partition key has no place in any range partition (23514).
    let err = client
        .simple_query("INSERT INTO m_2024 VALUES (5, NULL)")
        .await
        .unwrap_err();
    let db = err.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::CHECK_VIOLATION);
    assert_eq!(db.detail(), Some("Failing row contains (5, null)."));

    // Only the two admitted rows landed; the rejected inserts wrote nothing.
    let msgs = client.simple_query("SELECT count(*) FROM m_2024").await?;
    assert_eq!(rows(&msgs)[0].get(0), Some("2"));

    // An UPDATE that moves a row's key out of the leaf's range is rejected too
    // (the check is shared with INSERT).
    let err = client
        .simple_query("UPDATE m_2024 SET d = '2030-01-01' WHERE id = 1")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::CHECK_VIOLATION
    );

    // An unbounded (MINVALUE) leaf admits its open end but still rejects the
    // other side of its bound.
    client
        .simple_query(
            "CREATE TABLE m_early PARTITION OF m FOR VALUES FROM (MINVALUE) TO ('2024-01-01')",
        )
        .await?;
    client
        .simple_query("INSERT INTO m_early VALUES (6, '1900-01-01')")
        .await?;
    let err = client
        .simple_query("INSERT INTO m_early VALUES (7, '2024-06-01')")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::CHECK_VIOLATION
    );

    Ok(())
}

#[tokio::test]
async fn range_partitioning_detail_clips_long_field() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE tp (k text) PARTITION BY RANGE (k)")
        .await?;
    client
        .simple_query("CREATE TABLE tp_ab PARTITION OF tp FOR VALUES FROM ('a') TO ('b')")
        .await?;

    // A failing-row field longer than 64 bytes is clipped to 64 characters with
    // `...` appended in the DETAIL, matching PostgreSQL's per-column field limit.
    let long = "z".repeat(70);
    let err = client
        .simple_query(&format!("INSERT INTO tp_ab VALUES ('{long}')"))
        .await
        .unwrap_err();
    let db = err.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::CHECK_VIOLATION);
    assert_eq!(
        db.detail(),
        Some(format!("Failing row contains ({}...).", "z".repeat(64)).as_str())
    );

    Ok(())
}

#[tokio::test]
async fn range_partitioning_routed_insert_enforces_unique_and_error_order() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client
        .simple_query(
            "CREATE TABLE m (id int, sold date, amount int NOT NULL) PARTITION BY RANGE (sold)",
        )
        .await?;
    client
        .simple_query(
            "CREATE TABLE m_2023 PARTITION OF m FOR VALUES FROM ('2023-01-01') TO ('2024-01-01')",
        )
        .await?;
    // A leaf is an ordinary heap and may carry its own UNIQUE index.
    client
        .simple_query("CREATE UNIQUE INDEX m_2023_id_idx ON m_2023 (id)")
        .await?;

    // A row inserted through the parent that duplicates an existing leaf row must
    // be rejected by the leaf's unique index (23505) — routing does not bypass it.
    client
        .simple_query("INSERT INTO m VALUES (1, '2023-03-01', 10)")
        .await?;
    let err = client
        .simple_query("INSERT INTO m VALUES (1, '2023-06-01', 20)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::UNIQUE_VIOLATION
    );

    // Two rows in one statement that route to the same leaf with a duplicate key
    // are caught against each other, not just against pre-existing rows.
    let err = client
        .simple_query("INSERT INTO m VALUES (2, '2023-02-01', 1), (2, '2023-05-01', 2)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::UNIQUE_VIOLATION
    );
    // Nothing from the failed statements was written: only the first row remains.
    let msgs = client.simple_query("SELECT count(*) FROM m").await?;
    assert_eq!(rows(&msgs)[0].get(0), Some("1"));

    // Rows are processed in order: an earlier row's constraint violation is
    // reported before a later row's routing failure, matching PostgreSQL's
    // row-by-row processing. Row 1 routes fine but is NULL in a NOT NULL column
    // (23502); row 2 would fail routing (23514) — the row-1 error must win.
    let err = client
        .simple_query("INSERT INTO m VALUES (3, '2023-04-01', NULL), (4, '1990-01-01', 1)")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::NOT_NULL_VIOLATION
    );

    Ok(())
}

/// UPDATE/DELETE/COPY through a partitioned parent: in-place update, cross-leaf
/// row movement on a key change, RETURNING over routed writes, COPY routing, and
/// the "no partition found" (23514) failure when a key change fits no leaf.
#[tokio::test]
async fn range_partitioning_routed_update_delete_copy() -> anyhow::Result<()> {
    use bytes::Bytes;
    use futures_util::SinkExt;
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE m (id int, d date) PARTITION BY RANGE (d)")
        .await?;
    client
        .simple_query(
            "CREATE TABLE m_2023 PARTITION OF m FOR VALUES FROM ('2023-01-01') TO ('2024-01-01')",
        )
        .await?;
    client
        .simple_query(
            "CREATE TABLE m_2024 PARTITION OF m FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')",
        )
        .await?;

    // COPY through the parent routes each decoded row to the leaf whose range
    // admits its key: id 1 → m_2023, id 2 → m_2024.
    let sink = client
        .copy_in("COPY m FROM STDIN")
        .await
        .context("copy_in should enter copy mode")?;
    futures_util::pin_mut!(sink);
    sink.send(Bytes::from_static(b"1\t2023-06-01\n2\t2024-06-01\n"))
        .await?;
    assert_eq!(sink.finish().await?, 2);
    assert_eq!(
        rows(&client.simple_query("SELECT id FROM m_2023").await?)[0].get(0),
        Some("1")
    );
    assert_eq!(
        rows(&client.simple_query("SELECT id FROM m_2024").await?)[0].get(0),
        Some("2")
    );

    // A COPY row whose key fits no partition fails routing (23514) and the whole
    // load is rolled back.
    let sink = client.copy_in("COPY m FROM STDIN").await?;
    futures_util::pin_mut!(sink);
    sink.send(Bytes::from_static(b"9\t2019-01-01\n")).await?;
    let err = sink.finish().await.unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::CHECK_VIOLATION
    );
    assert_eq!(
        rows(&client.simple_query("SELECT count(*) FROM m").await?)[0].get(0),
        Some("2")
    );

    // An in-place UPDATE through the parent (key stays in the same leaf) rewrites
    // the row and RETURNING streams the NEW value.
    let msgs = client
        .simple_query("UPDATE m SET d = '2023-09-01' WHERE id = 1 RETURNING id, d")
        .await?;
    assert_eq!(
        (rows(&msgs)[0].get("id"), rows(&msgs)[0].get("d")),
        (Some("1"), Some("2023-09-01"))
    );
    // The row stayed in m_2023.
    assert_eq!(
        rows(&client.simple_query("SELECT d FROM m_2023 WHERE id = 1").await?)[0].get(0),
        Some("2023-09-01")
    );

    // A key-changing UPDATE moves the row across leaves: id 1 leaves m_2023 and
    // lands in m_2024 (delete-from-old + insert-into-new).
    client
        .simple_query("UPDATE m SET d = '2024-03-01' WHERE id = 1")
        .await?;
    assert!(
        rows(&client.simple_query("SELECT id FROM m_2023").await?).is_empty(),
        "m_2023 should be empty after the row moved out"
    );
    let msgs = client.simple_query("SELECT id FROM m_2024 ORDER BY id").await?;
    let moved: Vec<_> = rows(&msgs).iter().map(|r| r.get(0)).collect();
    assert_eq!(moved, vec![Some("1"), Some("2")]);

    // A key-changing UPDATE that fits no partition fails (23514) and nothing moves.
    let err = client
        .simple_query("UPDATE m SET d = '2019-05-01' WHERE id = 2")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::CHECK_VIOLATION
    );
    assert_eq!(
        rows(&client.simple_query("SELECT count(*) FROM m_2024").await?)[0].get(0),
        Some("2")
    );

    // DELETE through the parent with RETURNING removes from whichever leaf holds
    // the row and streams the deleted (OLD) row.
    let msgs = client
        .simple_query("DELETE FROM m WHERE id = 1 RETURNING id")
        .await?;
    assert_eq!(rows(&msgs)[0].get("id"), Some("1"));
    let msgs = client.simple_query("SELECT id FROM m ORDER BY id").await?;
    let remaining: Vec<_> = rows(&msgs).iter().map(|r| r.get(0)).collect();
    assert_eq!(remaining, vec![Some("2")]);

    Ok(())
}

/// Row movement respects the destination leaf's own UNIQUE index: an UPDATE that
/// relocates a row into a partition where its key already exists is rejected
/// (23505), and routing does not bypass the leaf's constraint.
#[tokio::test]
async fn range_partitioning_row_movement_respects_leaf_unique() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE m (id int, d date) PARTITION BY RANGE (d)")
        .await?;
    client
        .simple_query(
            "CREATE TABLE m_2023 PARTITION OF m FOR VALUES FROM ('2023-01-01') TO ('2024-01-01')",
        )
        .await?;
    client
        .simple_query(
            "CREATE TABLE m_2024 PARTITION OF m FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')",
        )
        .await?;
    // Only the destination leaf carries a UNIQUE index (leaves are ordinary heaps).
    client
        .simple_query("CREATE UNIQUE INDEX m_2024_id_idx ON m_2024 (id)")
        .await?;
    // id 1 lives in both leaves — legal, since the unique index is per-leaf.
    client
        .simple_query("INSERT INTO m VALUES (1, '2023-06-01'), (1, '2024-06-01')")
        .await?;

    // Moving the m_2023 row into m_2024 collides with the existing id 1 there, so
    // the destination leaf's unique index rejects it (23505) — nothing moves.
    let err = client
        .simple_query("UPDATE m SET d = '2024-03-01' WHERE id = 1 AND d = '2023-06-01'")
        .await
        .unwrap_err();
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::UNIQUE_VIOLATION
    );
    // The row stayed in m_2023; m_2024 still holds exactly its original row.
    assert_eq!(
        rows(&client.simple_query("SELECT count(*) FROM m_2023").await?)[0].get(0),
        Some("1")
    );
    assert_eq!(
        rows(&client.simple_query("SELECT count(*) FROM m_2024").await?)[0].get(0),
        Some("1")
    );

    // A move into m_2024 with a non-colliding key succeeds.
    client
        .simple_query("UPDATE m SET id = 2, d = '2024-03-01' WHERE id = 1 AND d = '2023-06-01'")
        .await?;
    let msgs = client.simple_query("SELECT id FROM m_2024 ORDER BY id").await?;
    let moved: Vec<_> = rows(&msgs).iter().map(|r| r.get(0)).collect();
    assert_eq!(moved, vec![Some("1"), Some("2")]);
    assert!(
        rows(&client.simple_query("SELECT id FROM m_2023").await?).is_empty(),
        "m_2023 should be empty after the row moved out"
    );

    Ok(())
}

#[tokio::test]
async fn long_union_all_chain_survives_the_server() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    // A flat chain binds to one N-ary node, so it costs no recursion. Nesting
    // one level per arm previously overflowed the stack and aborted the whole
    // process, taking every other session with it.
    let mut sql = String::from("SELECT 0");
    for i in 1..=1000 {
        sql.push_str(&format!(" UNION ALL SELECT {i}"));
    }
    let rows = client
        .simple_query(&format!("SELECT count(*) AS n FROM ({sql}) x"))
        .await?;
    let count = rows
        .iter()
        .find_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(r) => r.get("n").map(str::to_string),
            _ => None,
        })
        .expect("count row");
    assert_eq!(count, "1001");
    // The session — and the server — are still usable afterwards.
    client.simple_query("SELECT 1").await?;
    Ok(())
}

#[tokio::test]
async fn correlated_reference_inside_a_union_arm_resolves() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE c (a int4)").await?;
    client.simple_query("INSERT INTO c VALUES (1), (2)").await?;
    // The deduplicating form wraps the arms in the set-operation node; when that
    // wrapper was a derived table it counted as a query nesting level, leaving
    // `o.a` unsubstituted and surfacing an internal error.
    let rows = client
        .simple_query("SELECT a, (SELECT o.a UNION SELECT o.a) AS same FROM c o ORDER BY 1")
        .await?;
    let pairs: Vec<(String, String)> = rows
        .iter()
        .filter_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(r) => {
                Some((r.get("a")?.to_string(), r.get("same")?.to_string()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(pairs, vec![
        ("1".to_string(), "1".to_string()),
        ("2".to_string(), "2".to_string())
    ]);
    Ok(())
}

/// A `reg*` column must be advertised on the wire as the real PG type OID
/// (regclass = 2205), not as the `text`/`oid` it is represented by internally —
/// a client that reads `RowDescription` decodes by that OID. `\d` itself never
/// looks, but `\gdesc` and every typed driver do.
#[tokio::test]
async fn reg_columns_advertise_their_postgresql_type_oids() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let client = connect(port).await;
    client.simple_query("CREATE TABLE regwire (a integer)").await?;

    let typed = client
        .query(
            "SELECT 'regwire'::regclass AS c, 23::regtype AS t, 2200::regnamespace AS n",
            &[],
        )
        .await?;
    let columns = typed[0].columns();
    assert_eq!(columns[0].type_().oid(), 2205, "regclass");
    assert_eq!(columns[1].type_().oid(), 2206, "regtype");
    assert_eq!(columns[2].type_().oid(), 4089, "regnamespace");

    // The value on the wire is the rendered name, and the OID underneath still
    // round-trips through `::oid`.
    let rendered = client
        .simple_query("SELECT 'regwire'::regclass::text AS name, 23::regtype::text AS ty")
        .await?;
    let row = rows(&rendered);
    assert_eq!(row[0].get("name"), Some("regwire"));
    assert_eq!(row[0].get("ty"), Some("integer"));

    let same = client
        .simple_query(
            "SELECT 'regwire'::regclass::oid = 'REGWIRE'::regclass::oid AS eq",
        )
        .await?;
    assert_eq!(rows(&same)[0].get("eq"), Some("t"));
    Ok(())
}

#[tokio::test]
async fn analyze_measures_relations_and_reports_its_limits() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let port = spawn_server().await;
    let client = connect(port).await;
    client.simple_query("CREATE TABLE a1 (id int)").await?;
    client.simple_query("INSERT INTO a1 VALUES (1), (2)").await?;
    client.simple_query("CREATE TEMP TABLE a2 (id int)").await?;
    client.simple_query("INSERT INTO a2 VALUES (1)").await?;

    let reltuples = |messages: &[SimpleQueryMessage], want: &str| {
        let got = messages
            .iter()
            .find_map(|m| match m {
                SimpleQueryMessage::Row(row) => row.get(0).map(str::to_string),
                _ => None,
            })
            .expect("one row");
        assert_eq!(got, want);
    };
    let size = |name: &str| format!("SELECT reltuples::int FROM pg_class WHERE relname = '{name}'");

    // A bare ANALYZE covers every reachable relation, permanent and temp alike.
    client.simple_query("ANALYZE").await?;
    reltuples(&client.simple_query(&size("a1")).await?, "2");
    reltuples(&client.simple_query(&size("a2")).await?, "1");

    // A named ANALYZE re-measures just that relation.
    client.simple_query("INSERT INTO a1 VALUES (3)").await?;
    client.simple_query("ANALYZE a1").await?;
    reltuples(&client.simple_query(&size("a1")).await?, "3");

    // PostgreSQL 18.4 accepts ANALYZE inside a READ ONLY transaction and still
    // updates the statistics, so this must not be rejected as a write.
    client.simple_query("BEGIN TRANSACTION READ ONLY").await?;
    client.simple_query("ANALYZE a1").await?;
    client.simple_query("COMMIT").await?;

    let err = client
        .simple_query("ANALYZE nosuchtable")
        .await
        .expect_err("analyzing a missing relation must fail");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_TABLE
    );

    // Column statistics are not collected yet, so a column list is refused
    // rather than silently ignored.
    let err = client
        .simple_query("ANALYZE a1 (id)")
        .await
        .expect_err("a column list is not supported yet");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::FEATURE_NOT_SUPPORTED
    );

    Ok(())
}

/// `CREATE FUNCTION ... LANGUAGE plpgsql` and calling one from SQL: the whole
/// path from CREATE through overload resolution, the interpreter, and back out
/// as a value.
#[tokio::test]
async fn plpgsql_functions_run_from_sql() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    client
        .batch_execute(
            "CREATE FUNCTION fib(n int) RETURNS bigint LANGUAGE plpgsql AS $$
             DECLARE a bigint := 0; b bigint := 1; t bigint;
             BEGIN
               IF n < 1 THEN RETURN 0; END IF;
               FOR i IN 2..n LOOP
                 t := a + b; a := b; b := t;
               END LOOP;
               RETURN b;
             END $$",
        )
        .await?;

    let row = client.query_one("SELECT fib(10)", &[]).await?;
    assert_eq!(row.get::<_, i64>(0), 55);

    // Called per row of a real scan, and usable in a WHERE clause.
    client
        .batch_execute("CREATE TABLE t (n int); INSERT INTO t VALUES (1), (5), (10)")
        .await?;
    let rows = client
        .query("SELECT n, fib(n) FROM t WHERE fib(n) > 3 ORDER BY n", &[])
        .await?;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<_, i32>(0), 5);
    assert_eq!(rows[0].get::<_, i64>(1), 5);
    assert_eq!(rows[1].get::<_, i64>(1), 55);

    Ok(())
}

/// A PL/pgSQL routine writes inside the caller's transaction, and later
/// statements in the body see what earlier ones wrote.
#[tokio::test]
async fn a_plpgsql_function_can_write_and_read_back() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.batch_execute("CREATE TABLE log (msg text)").await?;
    client
        .batch_execute(
            "CREATE FUNCTION note(m text) RETURNS bigint LANGUAGE plpgsql AS $$
             DECLARE c bigint;
             BEGIN
               INSERT INTO log (msg) VALUES (m);
               SELECT count(*) INTO c FROM log;
               RETURN c;
             END $$",
        )
        .await?;

    assert_eq!(
        client
            .query_one("SELECT note('a')", &[])
            .await?
            .get::<_, i64>(0),
        1
    );
    assert_eq!(
        client
            .query_one("SELECT note('b')", &[])
            .await?
            .get::<_, i64>(0),
        2
    );
    // The writes are committed and visible to a plain query afterwards.
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM log", &[])
            .await?
            .get::<_, i64>(0),
        2
    );

    Ok(())
}

/// `RAISE EXCEPTION` reaches the client with its SQLSTATE and a CONTEXT
/// traceback naming the routine and the line.
#[tokio::test]
async fn plpgsql_raise_reaches_the_client_with_context() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .batch_execute(
            "CREATE FUNCTION boom(n int) RETURNS int LANGUAGE plpgsql AS $$\n\
             BEGIN\n\
               RAISE EXCEPTION 'bad value %', n USING ERRCODE = '22023', HINT = 'try 1';\n\
             END $$",
        )
        .await?;

    let e = client.query_one("SELECT boom(7)", &[]).await.unwrap_err();
    let db = e.as_db_error().expect("database error");
    assert_eq!(db.code().code(), "22023");
    assert_eq!(db.message(), "bad value 7");
    assert_eq!(db.hint(), Some("try 1"));
    assert_eq!(
        db.where_(),
        Some("PL/pgSQL function boom(integer) line 3 at RAISE")
    );

    Ok(())
}

/// A recursive routine terminates on its own, and an unbounded one reports
/// PostgreSQL's stack-depth error rather than aborting the process.
#[tokio::test]
async fn plpgsql_recursion_works_and_is_bounded() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;

    // A body may call itself: nothing binds the body at CREATE time, so the
    // routine is already registered by the time the call is resolved.
    client
        .batch_execute(
            "CREATE FUNCTION fact(n int) RETURNS bigint LANGUAGE plpgsql AS $$
             BEGIN
               IF n <= 1 THEN RETURN 1; END IF;
               RETURN n * fact(n - 1);
             END $$",
        )
        .await?;
    assert_eq!(
        client
            .query_one("SELECT fact(10)", &[])
            .await?
            .get::<_, i64>(0),
        3_628_800
    );

    client
        .batch_execute(
            "CREATE FUNCTION forever(n int) RETURNS int LANGUAGE plpgsql AS $$
             BEGIN RETURN forever(n + 1); END $$",
        )
        .await?;
    let e = client
        .query_one("SELECT forever(1)", &[])
        .await
        .unwrap_err();
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::from_code("54001")
    );

    Ok(())
}

/// A body that is not valid PL/pgSQL is rejected at CREATE time, as
/// PostgreSQL's validator does — but only for syntax, so a body referring to a
/// table that does not exist yet still creates.
#[tokio::test]
async fn plpgsql_bodies_are_syntax_checked_at_create_time() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;

    let e = client
        .batch_execute(
            "CREATE FUNCTION bad() RETURNS int LANGUAGE plpgsql AS $$ BEGIN x := 1; END $$",
        )
        .await
        .unwrap_err();
    let db = e.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::SYNTAX_ERROR);
    assert_eq!(db.message(), "\"x\" is not a known variable");

    // Forward references are fine: the SQL inside a body is only bound at call
    // time, so this creates and then works once the table exists.
    client
        .batch_execute(
            "CREATE FUNCTION later() RETURNS bigint LANGUAGE plpgsql AS $$
             DECLARE c bigint;
             BEGIN SELECT count(*) INTO c FROM not_yet; RETURN c; END $$",
        )
        .await?;
    client.batch_execute("CREATE TABLE not_yet (n int)").await?;
    assert_eq!(
        client
            .query_one("SELECT later()", &[])
            .await?
            .get::<_, i64>(0),
        0
    );

    Ok(())
}

/// `DO $$ ... $$` runs an anonymous block, including its NOTICEs and its DML.
#[tokio::test]
async fn do_blocks_run_and_emit_notices() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;
    client.batch_execute("CREATE TABLE t (n int)").await?;

    client
        .batch_execute(
            "DO $$ BEGIN FOR i IN 1..3 LOOP INSERT INTO t (n) VALUES (i); END LOOP; END $$",
        )
        .await?;
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM t", &[])
            .await?
            .get::<_, i64>(0),
        3
    );

    // LANGUAGE may be given explicitly, on either side of the code.
    client
        .batch_execute("DO LANGUAGE plpgsql $$ BEGIN NULL; END $$")
        .await?;

    // A language that cannot run inline code, and one that does not exist.
    let e = client
        .batch_execute("DO LANGUAGE sql $$ SELECT 1 $$")
        .await
        .unwrap_err();
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::FEATURE_NOT_SUPPORTED
    );
    let e = client
        .batch_execute("DO LANGUAGE nope $$ BEGIN END $$")
        .await
        .unwrap_err();
    let db = e.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::UNDEFINED_OBJECT);
    assert_eq!(db.message(), "language \"nope\" does not exist");

    Ok(())
}

/// `CREATE PROCEDURE` and `CALL`, and the two 42809s that keep procedures and
/// functions from being confused for one another.
#[tokio::test]
async fn procedures_are_created_called_and_dropped() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;
    client.batch_execute("CREATE TABLE t (n int)").await?;

    client
        .batch_execute(
            "CREATE PROCEDURE add_n(v int) LANGUAGE plpgsql AS $$
             BEGIN INSERT INTO t (n) VALUES (v); END $$",
        )
        .await?;
    client.batch_execute("CALL add_n(4)").await?;
    client.batch_execute("CALL add_n(5)").await?;
    assert_eq!(
        client
            .query_one("SELECT sum(n) FROM t", &[])
            .await?
            .get::<_, i64>(0),
        9
    );

    // A procedure is not callable as a function...
    let e = client.query_one("SELECT add_n(1)", &[]).await.unwrap_err();
    let db = e.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::WRONG_OBJECT_TYPE);
    assert_eq!(db.message(), "add_n(integer) is a procedure");
    assert_eq!(db.hint(), Some("To call a procedure, use CALL."));

    // ...nor a function callable with CALL.
    client
        // A LANGUAGE SQL body refers to its arguments only as `$n`.
        .batch_execute("CREATE FUNCTION fn_n(v int) RETURNS int LANGUAGE sql AS 'SELECT $1'")
        .await?;
    let e = client.batch_execute("CALL fn_n(1)").await.unwrap_err();
    let db = e.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::WRONG_OBJECT_TYPE);
    assert_eq!(db.hint(), Some("To call a function, use SELECT."));

    // DROP refuses to cross the two kinds, then drops the right one.
    let e = client
        .batch_execute("DROP FUNCTION add_n(int)")
        .await
        .unwrap_err();
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::WRONG_OBJECT_TYPE
    );
    client.batch_execute("DROP PROCEDURE add_n(int)").await?;
    let e = client.batch_execute("CALL add_n(1)").await.unwrap_err();
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_FUNCTION
    );

    Ok(())
}
