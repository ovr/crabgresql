//! End-to-end tests: a real driver (tokio-postgres) against an in-process
//! server on an ephemeral port, plus raw-socket checks of the startup phase.

use anyhow::Context as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_postgres::{NoTls, SimpleQueryMessage};

/// The OIDs of an `oidvector` column, decoded from the binary wire payload.
///
/// `tokio-postgres` has no built-in `FromSql` for `oidvector`, so without this
/// every test read of a vector column would have to go through a server-side
/// `::text` cast — which exercises the *text* output function and leaves
/// `Value::encode_binary` unverified over a real socket. This decodes the array
/// binary layout PostgreSQL uses for these types (`ndim`, has-nulls, element
/// OID, then per dimension a length and a lower bound, then each element as a
/// 4-byte length plus payload), asserting the parts that must be fixed.
#[derive(Debug)]
struct OidVectorBinary(Vec<u32>);

impl<'a> tokio_postgres::types::FromSql<'a> for OidVectorBinary {
    fn from_sql(
        _ty: &tokio_postgres::types::Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let be = |s: &[u8]| i32::from_be_bytes([s[0], s[1], s[2], s[3]]);
        if raw.len() < 20 {
            return Err("oidvector payload shorter than its header".into());
        }
        assert_eq!(be(&raw[0..4]), 1, "ndim");
        assert_eq!(be(&raw[4..8]), 0, "has nulls");
        assert_eq!(be(&raw[8..12]), 26, "element type is oid");
        // A vector's lower bound is 0, which is what makes it 0-subscripted.
        assert_eq!(be(&raw[16..20]), 0, "lower bound");
        let count = be(&raw[12..16]) as usize;
        assert_eq!(raw.len(), 20 + count * 8, "payload length");
        Ok(OidVectorBinary(
            (0..count)
                .map(|i| {
                    let at = 20 + i * 8;
                    assert_eq!(be(&raw[at..at + 4]), 4, "element length");
                    be(&raw[at + 4..at + 8]) as u32
                })
                .collect(),
        ))
    }

    fn accepts(ty: &tokio_postgres::types::Type) -> bool {
        ty.oid() == 30
    }
}

async fn spawn_server() -> u16 {
    spawn_server_reading(&[]).await
}

/// A server that may also read COPY source files under `copy_roots`, on top of
/// its own data directory. Server-side `COPY … FROM '<file>'` is confined by
/// path, and test fixtures live outside the data directory.
async fn spawn_server_reading(copy_roots: &[&std::path::Path]) -> u16 {
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
    let (engine, txnmgr) = crabgresql_server::open_pg_engine(dir.path()).expect("open test engine");
    let copy_files = copy_roots.iter().fold(
        crabgresql_server::CopyFileAccess::confined_to(dir.path()),
        |access, root| access.allowing(root),
    );
    std::mem::forget(dir);
    tokio::spawn(crabgresql_server::serve_with(
        listener, engine, txnmgr, copy_files,
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
        .expect_err("a column of a shell type must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_OBJECT
    );
    assert_eq!(
        err.as_db_error().expect("database error").message(),
        "type \"shell_only\" is only a shell"
    );

    // CREATE names the namespace to create in rather than resolving through the
    // search path, so a built-in's name is free: PostgreSQL puts the new type in
    // `public` beside `pg_catalog.int4`. What must not change is which one an
    // unqualified reference finds — pg_catalog precedes public, so `int4` keeps
    // meaning the built-in, and the shadow type is reachable only by OID here.
    client
        .simple_query("CREATE TYPE int4 AS ENUM ('shadow')")
        .await?;
    let still_builtin = client.simple_query("SELECT 1::int4 AS v").await?;
    assert_eq!(rows(&still_builtin)[0].get(0), Some("1"));
    let both = client
        .simple_query("SELECT count(*) FROM pg_type WHERE typname = 'int4'")
        .await?;
    assert_eq!(rows(&both)[0].get(0), Some("2"));
    // A built-in cannot be dropped, and an unqualified DROP names it.
    let err = client
        .simple_query("DROP TYPE int4")
        .await
        .expect_err("an unqualified DROP resolves to the built-in, which is not droppable");
    let err = err.as_db_error().expect("database error");
    assert_eq!(err.code(), &SqlState::DEPENDENT_OBJECTS_STILL_EXIST);
    assert_eq!(
        err.message(),
        "cannot drop type integer because it is required by the database system"
    );
    // A second CREATE in the same namespace is still a duplicate.
    let err = client
        .simple_query("CREATE TYPE int4 AS ENUM ('again')")
        .await
        .expect_err("two user types of one name in public must collide");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::DUPLICATE_OBJECT
    );

    // A built-in type the catalog knows about but this build does not model is
    // `0A000`, not the `42704` a nonexistent type would get. (`xml` is the stand-in
    // here; it used to be `box`, which is now a real type.)
    let err = client
        .simple_query("CREATE TABLE unsupported (value xml)")
        .await
        .expect_err("a column of a catalog type this build does not model must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::FEATURE_NOT_SUPPORTED
    );
    let err = client
        .simple_query("CREATE TABLE unsupported (value _nosuchtype)")
        .await
        .expect_err("a column of a type no catalog knows must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_OBJECT
    );

    // `_int4` is not in that group: it is PostgreSQL's own name for integer[],
    // and it declares exactly the same column the bracket spelling does.
    client
        .simple_query("CREATE TABLE arrspelling (a _int4, b integer[])")
        .await?;
    let spelled = client
        .simple_query(
            "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod) AS ty \
             FROM pg_attribute a \
             WHERE a.attrelid = (SELECT oid FROM pg_class WHERE relname = 'arrspelling') \
               AND a.attnum > 0 ORDER BY a.attnum",
        )
        .await?;
    let spelled = rows(&spelled);
    assert_eq!(spelled[0].get(1), Some("integer[]"));
    assert_eq!(spelled[1].get(1), Some("integer[]"));
    // …and it resolves as a type reference, not just as DDL syntax.
    let regt = client.simple_query("SELECT '_int4'::regtype").await?;
    assert_eq!(rows(&regt)[0].get(0), Some("integer[]"));

    // There is no array-of-array type, so the composition stays an error rather
    // than minting an Array whose own OID would be 0.
    let err = client
        .simple_query("CREATE TABLE nested (a _int4[])")
        .await
        .expect_err("an array of an array must be rejected");
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

    // An array column's atttypid is the array type's own OID, so the join every
    // client driver builds its type map with has to land on a real pg_type row.
    client
        .simple_query("CREATE TABLE arrcols (tags int[], notes text[])")
        .await?;
    let arr = client
        .simple_query(
            "SELECT a.attname, t.oid, t.typname, t.typelem, t.typcategory \
             FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid \
             WHERE a.attrelid = (SELECT oid FROM pg_class WHERE relname = 'arrcols') \
               AND a.attnum > 0 ORDER BY a.attnum",
        )
        .await?;
    let arr = rows(&arr);
    assert_eq!(
        arr.len(),
        2,
        "both array columns must join to a pg_type row"
    );
    assert_eq!(arr[0].get(1), Some("1007"));
    assert_eq!(arr[0].get(2), Some("_int4"));
    assert_eq!(arr[0].get(3), Some("23"));
    assert_eq!(arr[0].get(4), Some("A"));
    assert_eq!(arr[1].get(2), Some("_text"));
    assert_eq!(arr[1].get(3), Some("25"));

    for target in ["varchar", "name", "bpchar"] {
        let sql = format!("SELECT 'red'::rainbow::{target}");
        let err = client
            .simple_query(&sql)
            .await
            .expect_err("an enum must not coerce to a string type other than text");
        assert_eq!(
            err.as_db_error().expect("database error").code(),
            &SqlState::CANNOT_COERCE
        );
    }
    client.simple_query("SELECT 'red'::rainbow::text").await?;

    let err = client
        .simple_query("SELECT 'red'::rainbow > 1")
        .await
        .expect_err("comparing an enum with an integer must find no operator");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_FUNCTION
    );
    assert_eq!(
        err.as_db_error().expect("database error").message(),
        "operator does not exist: rainbow > integer"
    );

    client
        .simple_query("CREATE TYPE zeta AS ENUM ('z')")
        .await?;
    client
        .simple_query("CREATE TYPE alpha AS ENUM ('a')")
        .await?;
    let ordered = client
        .simple_query("SELECT typname FROM pg_type WHERE typname = 'zeta' OR typname = 'alpha'")
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
        .expect_err("a user type with no comparison operator must not be orderable");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_FUNCTION
    );
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
        .expect_err("an unqualified information_schema view name must not resolve");
    assert_eq!(
        err.code().expect("database error has SQLSTATE").code(),
        "42P01"
    );
    let err = client
        .simple_query("INSERT INTO information_schema.tables VALUES (1)")
        .await
        .expect_err("writing to an information_schema view must be refused");
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
    let mut deleted: Vec<(Option<&str>, Option<&str>)> = rows(&messages)
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
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
    assert_eq!(
        err.code(),
        Some(&tokio_postgres::error::SqlState::DIVISION_BY_ZERO)
    );
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

/// Assert an error is PG's `42P17` for a view that (transitively) reads itself,
/// naming `relation`.
fn assert_view_recursion(err: tokio_postgres::Error, relation: &str) {
    let db = err.as_db_error().expect("db error");
    assert_eq!(
        db.code(),
        &tokio_postgres::error::SqlState::INVALID_OBJECT_DEFINITION
    );
    assert_eq!(
        db.message(),
        format!("infinite recursion detected in rules for relation \"{relation}\"")
    );
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
        let err = client
            .simple_query(ddl)
            .await
            .expect_err("an out-of-range type modifier must be rejected");
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
        let err = client
            .simple_query(ddl)
            .await
            .expect_err("regclass in a table definition must be rejected");
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
        .simple_query(
            "SELECT schema_name FROM information_schema.schemata WHERE schema_name = 'app'",
        )
        .await?;
    assert_eq!(rows(&schemata)[0].get(0), Some("app"));

    // Duplicate without IF NOT EXISTS → 42P06; with it → success (a NOTICE).
    let err = client
        .simple_query("CREATE SCHEMA app")
        .await
        .expect_err("a duplicate CREATE SCHEMA without IF NOT EXISTS must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::from_code("42P06")
    );
    client
        .simple_query("CREATE SCHEMA IF NOT EXISTS app")
        .await?;

    // A `pg_`-prefixed name is reserved (42939).
    let err = client
        .simple_query("CREATE SCHEMA pg_evil")
        .await
        .expect_err("a pg_-prefixed schema name is reserved and must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::from_code("42939")
    );

    // A schema-qualified table coexists with a same-named public table, and its
    // pg_class.relnamespace resolves to the schema's pg_namespace.oid.
    client
        .simple_query("CREATE TABLE app.item (id int, label text)")
        .await?;
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
    let nv = client
        .simple_query("SELECT nextval('app.counter_id_seq')")
        .await?;
    assert_eq!(rows(&nv)[0].get(0), Some("3"));

    // CREATE TABLE in a missing schema → 3F000.
    let err = client
        .simple_query("CREATE TABLE nope.t (id int)")
        .await
        .expect_err("creating a table in a schema that does not exist must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::from_code("3F000")
    );

    // DROP SCHEMA RESTRICT on a non-empty schema → 2BP01.
    let err = client
        .simple_query("DROP SCHEMA app")
        .await
        .expect_err("DROP SCHEMA RESTRICT on a non-empty schema must be refused");
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
        .expect_err("a relation in a dropped schema must no longer resolve");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_TABLE
    );
    // The like-named public table survives.
    client.simple_query("SELECT id FROM item").await?;

    // DROP SCHEMA of a missing schema → 3F000; IF EXISTS → success.
    let err = client
        .simple_query("DROP SCHEMA nope")
        .await
        .expect_err("dropping a schema that does not exist without IF EXISTS must be rejected");
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
        .expect_err("a scalar subquery returning more than one row must be rejected");
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
    client
        .simple_query("CREATE TABLE t1 (a int, b int)")
        .await?;
    client
        .simple_query("INSERT INTO t1 VALUES (1, 10), (2, 20), (3, 30)")
        .await?;
    client
        .simple_query("CREATE TABLE t2 (a int, c int)")
        .await?;
    client
        .simple_query("INSERT INTO t2 VALUES (1, 100), (1, 200), (2, 20), (2, 50), (4, 400)")
        .await?;

    // Correlated EXISTS (Q4-shape): keep outer rows with a matching t2 row.
    let msgs = client
        .simple_query(
            "SELECT a FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.a = t1.a) ORDER BY a",
        )
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
        .simple_query("SELECT a FROM t1 WHERE b IN (SELECT c FROM t2 WHERE t2.a = t1.a) ORDER BY a")
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
        .expect_err("a scalar subquery correlated to a grouped outer query must be rejected");
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
        .simple_query(
            "SELECT id FROM sq WHERE val = ANY(SELECT val FROM sq WHERE val <> 20) ORDER BY id",
        )
        .await?;
    assert_eq!(ids(&msgs), vec!["1", "3"]);

    let msgs = client
        .simple_query(
            "SELECT id FROM sq WHERE val <> ALL(SELECT val FROM sq WHERE val = 20) ORDER BY id",
        )
        .await?;
    assert_eq!(ids(&msgs), vec!["1", "3"]);

    let msgs = client
        .simple_query(
            "SELECT id FROM sq WHERE val > ALL(SELECT val FROM sq WHERE val < 30) ORDER BY id",
        )
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
    let err = client
        .simple_query("SELECT 1 = ANY(2)")
        .await
        .expect_err("ANY with a non-array right side must be rejected");
    let db = err.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::WRONG_OBJECT_TYPE);
    assert_eq!(
        db.message(),
        "op ANY/ALL (array) requires array on right side"
    );
    assert_eq!(
        db.position(),
        Some(&tokio_postgres::error::ErrorPosition::Original(10))
    );

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

/// `uuid` over the binary wire format, in both directions: `tokio-postgres`
/// sends a `uuid` parameter as the 16 raw bytes and asks for every result
/// column in binary, so this is what exercises `wire::decode_binary` /
/// `Value::encode_binary` for the type end to end.
#[tokio::test]
async fn uuid_round_trips_in_binary_over_the_extended_protocol() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    let sent = uuid::uuid!("5b35380a-7143-4912-9b55-f322699c6770");

    // Parameter in, value back out — neither side goes through the text form.
    let row = client.query_one("SELECT $1::uuid AS g", &[&sent]).await?;
    assert_eq!(row.get::<_, uuid::Uuid>("g"), sent);

    // A generated value decodes too, and carries the version it claims.
    let row = client
        .query_one(
            "SELECT uuidv7() AS g, uuid_extract_version(uuidv7()) AS v",
            &[],
        )
        .await?;
    assert_eq!(row.get::<_, uuid::Uuid>("g").get_version_num(), 7);
    assert_eq!(row.get::<_, i16>("v"), 7);

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
async fn substring_pattern_extraction() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;

    // `substring(text, text)` extracts with a POSIX regex: the first
    // subexpression when there is one, otherwise the whole match.
    let messages = client
        .simple_query(
            "SELECT substring('Thomas' from '...$') AS tail, \
             substring('foobar' from 'o(.)b(a)') AS grp, \
             substring('abc' from '(x)?b') AS unmatched_grp, \
             substring('ABC' from 'b') AS case_sensitive",
        )
        .await?;
    let row = rows(&messages)[0];
    assert_eq!(row.get(0), Some("mas"));
    assert_eq!(row.get(1), Some("o"));
    assert_eq!(row.get(2), None);
    assert_eq!(row.get(3), None);

    // The three-argument SQL-regex form, in both of its spellings.
    let messages = client
        .simple_query(
            "SELECT substring('Thomas' similar '%#\"o_a#\"_' escape '#') AS sim, \
             substring('Thomas' from '%#\"o_a#\"_' for '#') AS from_for, \
             substring('Thomas' similar '%o_a_' escape '#') AS whole, \
             substring('XY' similar 'X#\"Y' escape '#') AS open_ended",
        )
        .await?;
    let row = rows(&messages)[0];
    assert_eq!(row.get(0), Some("oma"));
    assert_eq!(row.get(1), Some("oma"));
    assert_eq!(row.get(2), Some("Thomas"));
    assert_eq!(row.get(3), Some("Y"));

    // Overload resolution splits the two names: an untyped literal is a pattern
    // for `substring` but an offset for `substr`, which has no regex forms.
    let messages = client
        .simple_query(
            "SELECT substring('abcdef' from '2') AS pattern, \
             substr('abcdef', '2') AS offset_, \
             substring('abcdef' from 2 for 3) AS positional",
        )
        .await?;
    let row = rows(&messages)[0];
    assert_eq!(row.get(0), None);
    assert_eq!(row.get(1), Some("bcdef"));
    assert_eq!(row.get(2), Some("bcd"));

    // At most two separators, and the escape must be a single character.
    let err = client
        .simple_query("SELECT substring('XYZ' similar 'X#\"Y#\"Z#\"' escape '#')")
        .await
        .expect_err("more than two escape-double-quote separators must be rejected");
    let db = err.as_db_error().expect("database error");
    assert_eq!(db.code().code(), "2200C");
    assert_eq!(
        db.message(),
        "SQL regular expression may not contain more than two escape-double-quote separators"
    );

    // The escape-length complaint is a HINT in PG, not a DETAIL, and every
    // operator that takes an ESCAPE reports it the same way.
    for query in [
        "SELECT substring('Thomas' similar 'o' escape '##')",
        "SELECT 'x' SIMILAR TO 'x' ESCAPE '##'",
        "SELECT 'x' LIKE 'x' ESCAPE '##'",
    ] {
        let err = client
            .simple_query(query)
            .await
            .expect_err("a multi-character escape string must be rejected");
        let db = err.as_db_error().expect("database error");
        assert_eq!(db.code(), &SqlState::INVALID_ESCAPE_SEQUENCE, "{query}");
        assert_eq!(db.message(), "invalid escape string", "{query}");
        assert_eq!(
            db.hint(),
            Some("Escape string must be empty or one character."),
            "{query}"
        );
        assert_eq!(db.detail(), None, "{query}");
    }

    Ok(())
}

#[tokio::test]
async fn substring_similar_is_rejected_under_the_substr_spelling() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;

    // PG's grammar gives the SIMILAR spelling to SUBSTRING only. Accepting it
    // under SUBSTR would bind against `substr(text, int4, int4)` and read the
    // pattern and the escape as offsets, so `substr('abcdef' SIMILAR '2' ESCAPE
    // '3')` would quietly return 'bcd' instead of being rejected.
    for query in [
        "SELECT substr('abcdef' SIMILAR '2' ESCAPE '3')",
        "SELECT substr('abcdef' SIMILAR '%#\"cd#\"%' ESCAPE '#')",
    ] {
        let err = client
            .simple_query(query)
            .await
            .expect_err("the substr spelling must not accept the SIMILAR form");
        let db = err.as_db_error().expect("database error");
        assert_eq!(db.code(), &SqlState::SYNTAX_ERROR, "{query}");
    }

    // The SUBSTRING spelling still works, and each name labels its own column.
    let messages = client
        .simple_query(
            "SELECT substring('abcdef' SIMILAR '%#\"cd#\"%' ESCAPE '#'), \
             substr('abcdef', 2, 3)",
        )
        .await?;
    let row = rows(&messages)[0];
    let names: Vec<_> = row.columns().iter().map(|c| c.name()).collect();
    assert_eq!(names, ["substring", "substr"]);
    assert_eq!(row.get(0), Some("cd"));
    assert_eq!(row.get(1), Some("bcd"));

    Ok(())
}

#[tokio::test]
async fn substring_similar_extracts_from_the_earliest_position() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    // The segment before the first separator prefers the shortest match, so the
    // canonical `%#"..."#%` idiom extracts from the earliest position rather
    // than letting a greedy prefix eat the capture.
    let messages = client
        .simple_query(
            "SELECT substring('abc' similar '%#\"%#\"%' escape '#') AS whole, \
             substring('aaa' similar '%#\"a%#\"%' escape '#') AS runs, \
             substring('foobar' similar '%#\"o+#\"%' escape '#') AS quantified, \
             substring('ab' similar '#\"a#\"|b' escape '#') AS alternation",
        )
        .await?;
    let row = rows(&messages)[0];
    assert_eq!(row.get(0), Some("abc"));
    assert_eq!(row.get(1), Some("aaa"));
    assert_eq!(row.get(2), Some("oo"));
    assert_eq!(row.get(3), Some("a"));

    Ok(())
}

/// A failing binary operator whose error carries no cursor position of its own
/// must not make binding cost double per nesting level.
///
/// `bind_binary` stamps the operator token onto its own 42883 so PG's caret
/// lands there. An earlier shape decided *whose* error it was by re-binding both
/// operands on the error path, which re-bound the whole subtree once per
/// enclosing operator — 24 chained `+` took over 30 seconds to reject, from a
/// query short enough to paste. A typed marker on the error answers the same
/// question in constant time.
#[tokio::test]
async fn a_deeply_nested_operator_error_binds_in_linear_time() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    // The `IN` fallback raises an unlocated 42883, so nothing short-circuits on
    // an already-set position.
    let query = format!("SELECT (1 IN (1, 'x'::text)){}", " + 1".repeat(24));
    let started = std::time::Instant::now();
    let err = client
        .simple_query(&query)
        .await
        .expect_err("comparing an integer with text must find no operator");
    let elapsed = started.elapsed();

    let db = err
        .as_db_error()
        .ok_or_else(|| anyhow::anyhow!("expected a db error"))?;
    assert_eq!(db.message(), "operator does not exist: integer = text");
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "binding took {elapsed:?}; an exponential re-bind has regressed"
    );

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
        .expect_err("an operator symbol that names no operator must be rejected");
    let db = err.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::UNDEFINED_FUNCTION);
    assert!(
        db.message().starts_with("operator does not exist:")
            && db.message().contains("pg_catalog.###")
            && !db.message().contains("OPERATOR("),
        "unexpected message: {}",
        db.message()
    );
    // An unrecognized symbol names no operator at all, so PG reports that rather
    // than the type-mismatch DETAIL, and offers no cast HINT.
    assert_eq!(db.detail(), Some("There is no operator of that name."));
    assert_eq!(db.hint(), None);

    // An operand error surfaces first, as PG analyzes operands before resolving
    // the operator — an undefined column is 42703, not masked as 42883.
    let err = client
        .simple_query("SELECT missing_col OPERATOR(pg_catalog.###) 1")
        .await
        .expect_err("an undefined operand column must be reported before the operator");
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
        .expect_err("an operator qualified by a schema other than pg_catalog must be rejected");
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
        .expect_err("a relation that does not exist must be rejected");
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
    let err = client
        .simple_query("SELEC 1")
        .await
        .expect_err("a misspelled keyword must be rejected");
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
        .expect_err("an integer literal beyond the column type's range must be rejected");
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
        let err = client
            .simple_query(sql)
            .await
            .expect_err("an unsupported GROUP BY extension must be rejected");
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
    assert_eq!(
        (deduped[0].get(0), deduped[0].get(1)),
        (Some("1"), Some("10"))
    );
    assert_eq!(
        (deduped[1].get(0), deduped[1].get(1)),
        (Some("2"), Some("20"))
    );
    assert_eq!(
        (deduped[2].get(0), deduped[2].get(1)),
        (Some("2"), Some("30"))
    );

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

/// A correlated reference below a window chain resolves against the right outer
/// row. The chain is wrapped in a synthetic `Subquery`, which is *not* a query
/// nesting level — treating it as one stranded the reference. Same bug family as
/// `correlated_reference_inside_a_union_arm_resolves`.
///
/// The two-level case is the one that mattered most: it failed *silently*,
/// returning the first inner row's value for every outer row, so this asserts
/// values rather than merely the absence of an error.
#[tokio::test]
async fn correlated_reference_below_a_window_chain_resolves() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE wide (a int, b int)")
        .await?;
    client
        .simple_query("INSERT INTO wide VALUES (1,10),(2,20),(3,30),(4,40)")
        .await?;

    // sum(b) is 100 over the four rows, so each outer row adds 4 * p.a.
    let one = client
        .query(
            "SELECT p.a, (SELECT sum(w.b + p.a) OVER () FROM wide w LIMIT 1) FROM wide p ORDER BY 1",
            &[],
        )
        .await?;
    let got: Vec<(i32, i64)> = one.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, vec![(1, 104), (2, 108), (3, 112), (4, 116)]);

    // Two markers deep: the inner reference is level 2 and must be decremented
    // at the outer boundary, not skipped.
    let two = client
        .query(
            "SELECT p.a, (SELECT (SELECT sum(w.b + p.a) OVER () FROM wide w LIMIT 1) \
             FROM wide m LIMIT 1) FROM wide p ORDER BY 1",
            &[],
        )
        .await?;
    let got: Vec<(i32, i64)> = two.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(
        got,
        vec![(1, 104), (2, 108), (3, 112), (4, 116)],
        "each outer row must see its own `a`, not the first one"
    );

    let exists = client
        .query(
            "SELECT p.a FROM wide p \
             WHERE EXISTS (SELECT rank() OVER (ORDER BY w.b) FROM wide w WHERE w.a = p.a) \
             ORDER BY 1",
            &[],
        )
        .await?;
    assert_eq!(exists.len(), 4);

    // The same synthetic-wrapper bug without any window: `attach_sort` wraps a
    // sorted LIMIT the same way.
    let sorted_limit = client
        .query(
            "SELECT p.a FROM wide p \
             WHERE EXISTS ( (SELECT 1 FROM wide w WHERE w.a = p.a LIMIT 1) ORDER BY 1 ) \
             ORDER BY 1",
            &[],
        )
        .await?;
    assert_eq!(sorted_limit.len(), 4);

    Ok(())
}

/// A user-defined function may share a window function's name: PG resolves by
/// name *and* argument types, so `rank(int)` is the user's while the bare
/// `rank()` is still the builtin. Needs the catalog, so it lives here.
#[tokio::test]
async fn a_user_function_may_shadow_a_window_function_name() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id int)").await?;
    client.simple_query("INSERT INTO t VALUES (1), (2)").await?;
    client
        .simple_query("CREATE FUNCTION rank(x int) RETURNS int LANGUAGE SQL AS 'SELECT $1 + 1'")
        .await?;

    let rows = client.query("SELECT rank(41)", &[]).await?;
    assert_eq!(
        rows[0].get::<_, i32>(0),
        42,
        "the user's function is called"
    );

    let err = client
        .simple_query("SELECT rank() FROM t")
        .await
        .expect_err("a window function without an OVER clause must be rejected");
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::WRONG_OBJECT_TYPE);
    assert_eq!(db.message(), "window function rank requires an OVER clause");

    let windowed = client.query("SELECT rank() OVER () FROM t", &[]).await?;
    assert_eq!(windowed.len(), 2);

    Ok(())
}

/// A window call in a `LANGUAGE SQL` body is refused. PG accepts it and runs the
/// body as its own query (every call returns 1); this engine *inlines* bodies, so
/// the marker would join the caller's chain and number the caller's rows instead.
/// Refusing is the honest answer until bodies stop being inlined.
#[tokio::test]
async fn a_window_call_in_a_sql_function_body_is_rejected() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    let err = client
        .simple_query(
            "CREATE FUNCTION wr() RETURNS bigint LANGUAGE SQL AS 'SELECT row_number() OVER ()'",
        )
        .await
        .expect_err("a window function in a SQL function body must be rejected");
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::FEATURE_NOT_SUPPORTED);
    assert_eq!(
        db.message(),
        "window functions in a SQL function body are not supported yet"
    );

    Ok(())
}

/// A window call in a column DEFAULT is refused at DDL time. Accepting it left a
/// table whose every default-taking INSERT failed.
#[tokio::test]
async fn a_window_call_in_a_column_default_is_rejected() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    let err = client
        .simple_query("CREATE TABLE d (a int DEFAULT rank() OVER (), b int)")
        .await
        .expect_err("a window function in a DEFAULT expression must be rejected");
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::WINDOWING_ERROR);
    assert_eq!(
        db.message(),
        "window functions are not allowed in DEFAULT expressions"
    );

    Ok(())
}

/// Window functions over the wire: the values, and the types they are described
/// as. `rank()` is `int8` and `sum(int4)` widens to `int8`, both of which a
/// binary-format client decodes by the advertised OID — so getting one wrong is
/// a protocol bug, not just a display one.
#[tokio::test]
async fn window_functions_over_a_table() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE w (dep text, sal integer)")
        .await?;
    client
        .simple_query(
            "INSERT INTO w VALUES ('a', 100), ('a', 200), ('a', 200), ('b', 300), ('b', 400)",
        )
        .await?;

    // Ranking over ties: rank skips, dense_rank does not, row_number never ties.
    let rows = client
        .query(
            "SELECT rank() OVER w, dense_rank() OVER w, row_number() OVER w \
             FROM w WINDOW w AS (PARTITION BY dep ORDER BY sal) ORDER BY 1, 2, 3",
            &[],
        )
        .await?;
    let ranks: Vec<(i64, i64, i64)> = rows
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    assert_eq!(
        ranks,
        vec![(1, 1, 1), (1, 1, 1), (2, 2, 2), (2, 2, 2), (2, 2, 3)]
    );

    // The default frame runs through the current row's last peer, so the two
    // rows tied at 200 share a running total of 500 rather than 300 and 500.
    let rows = client
        .query(
            "SELECT sal, sum(sal) OVER (PARTITION BY dep ORDER BY sal) \
             FROM w WHERE dep = 'a' ORDER BY sal",
            &[],
        )
        .await?;
    let running: Vec<(i32, i64)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(running, vec![(100, 100), (200, 500), (200, 500)]);

    // The advertised types: both counting and summing an int4 yield int8.
    let rows = client
        .query(
            "SELECT rank() OVER (ORDER BY sal), sum(sal) OVER () FROM w",
            &[],
        )
        .await?;
    assert_eq!(
        rows[0].columns()[0].type_(),
        &tokio_postgres::types::Type::INT8
    );
    assert_eq!(
        rows[0].columns()[1].type_(),
        &tokio_postgres::types::Type::INT8
    );

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
        .expect_err("an ungrouped column outside an aggregate must be rejected");
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
        .expect_err("an out-of-range value in any row must fail the whole INSERT");
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
        .expect_err("a column named twice in the INSERT column list must be rejected");
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
        .expect_err("fewer values than named columns must be rejected");
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
        .expect_err("a non-numeric literal for an integer column must be rejected");
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
        .expect_err("a duplicate CREATE TABLE without IF NOT EXISTS must be rejected");
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
        let err = client
            .simple_query(sql)
            .await
            .expect_err("an unsupported ON COMMIT action must be rejected");
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
        .expect_err("CREATE TABLE AS over an existing relation must be rejected");
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

    let update_duplicate = client
        .simple_query("UPDATE c SET u = 9")
        .await
        .expect_err("an UPDATE collapsing a unique column onto one value must be rejected");
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
        .expect_err("two rows of one INSERT sharing a unique key must be rejected");
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
        .expect_err("a NULL in a NOT NULL column must be rejected");
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
        .expect_err("a second NULL under UNIQUE NULLS NOT DISTINCT must be rejected");
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
        .expect_err("a unique index build over duplicate rows must be rejected");
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
    // `indkey`/`indoption` are real `int2vector`s (PG's type OID 22), not text.
    let indkey_row = client
        .query_one(
            "SELECT indkey, indoption FROM pg_index ORDER BY indexrelid LIMIT 1",
            &[],
        )
        .await?;
    assert_eq!(
        indkey_row.columns()[0].type_().oid(),
        22,
        "indkey is int2vector"
    );
    assert_eq!(
        indkey_row.columns()[1].type_().oid(),
        22,
        "indoption is int2vector"
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
        .expect_err("SET TRANSACTION ISOLATION LEVEL after a query in the block must be rejected");
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
        .expect_err("a write under SET TRANSACTION READ ONLY must be refused");
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
        .expect_err("SET TRANSACTION ISOLATION LEVEL after DDL in the block must be rejected");
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
        .expect_err("a write in a BEGIN READ ONLY block must be refused");
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
        .expect_err("DDL in a BEGIN READ ONLY block must be refused");
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
    let err = client
        .simple_query("DROP TABLE nope")
        .await
        .expect_err("a DROP of a missing table in a read-only block must be refused as read-only");
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
        .expect_err("a write under default_transaction_read_only must be refused");
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
        .expect_err("a row whose UPDATE expression divides by zero must fail the statement");
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
        .expect_err("the failing SELECT must abort the rest of the simple-query batch");
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
        let err = client
            .simple_query(sql)
            .await
            .expect_err("each listed expression must be rejected");
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
        .expect_err("UPDATE arithmetic overflowing the column type must be rejected");
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
        .expect_err("a VALUES list with rows of differing width must be rejected");
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
        .expect_err("overflow while evaluating the source query must abort the INSERT");
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
        .expect_err("a constant beyond the column type's range must be rejected even with no rows");
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
        .expect_err("a comparison literal beyond the column type's range must be rejected");
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
    let err = client
        .simple_query("TRUNCATE nope")
        .await
        .expect_err("truncating a table that does not exist must be rejected");
    assert_eq!(
        err.as_db_error()
            .context("database error details are missing")?
            .code(),
        &tokio_postgres::error::SqlState::UNDEFINED_TABLE
    );

    Ok(())
}

/// TRUNCATE swaps the table's indexes along with its heap file. Only at this
/// level does the executor's Index Scan run, and on the physical path it
/// re-checks nothing — so a carried-over index would answer `WHERE id = 1` with
/// a row the post-truncate INSERT placed at the tid the stale entry names.
#[tokio::test]
async fn truncate_resets_the_index_so_stale_keys_find_nothing() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (id integer)").await?;
    client.simple_query("CREATE INDEX t_idx ON t (id)").await?;
    client
        .simple_query("INSERT INTO t VALUES (1), (2), (3)")
        .await?;
    client.simple_query("TRUNCATE t").await?;
    client.simple_query("INSERT INTO t VALUES (7)").await?;

    // Guard the guard: if the planner stopped choosing the index this test would
    // silently pass on a sequential scan, which re-checks the key itself.
    let plan = rows(
        &client
            .simple_query("EXPLAIN SELECT * FROM t WHERE id = 1")
            .await?,
    )
    .iter()
    .filter_map(|r| r.get(0).map(str::to_string))
    .collect::<Vec<_>>()
    .join("\n");
    assert!(
        plan.contains("Index Scan"),
        "expected an index scan:\n{plan}"
    );

    assert_eq!(
        rows(&client.simple_query("SELECT * FROM t WHERE id = 1").await?).len(),
        0,
        "a truncated-away key must find nothing"
    );
    let msgs = client.simple_query("SELECT * FROM t WHERE id = 7").await?;
    let after = rows(&msgs);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].get(0), Some("7"));

    // And a rolled-back TRUNCATE leaves the index serving the original rows.
    client
        .simple_query("BEGIN; TRUNCATE t; INSERT INTO t VALUES (42); ROLLBACK")
        .await?;
    assert_eq!(
        rows(&client.simple_query("SELECT * FROM t WHERE id = 7").await?).len(),
        1
    );
    assert_eq!(
        rows(&client.simple_query("SELECT * FROM t WHERE id = 42").await?).len(),
        0
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
        .expect_err("a TRUNCATE list naming a missing table must fail as a whole");
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
        let err = client
            .simple_query(sql)
            .await
            .expect_err("an unsupported TRUNCATE option must be rejected");
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

#[tokio::test]
async fn dml_on_pk_equality_probes_the_index() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id int PRIMARY KEY, label text)")
        .await?;
    client
        .simple_query("INSERT INTO t VALUES (1, 'one'), (2, 'two'), (3, 'three')")
        .await?;

    // The modify node now reports the child scan it reads through.
    let lines = explain_lines(&client, "EXPLAIN UPDATE t SET label = 'x' WHERE id = 2").await?;
    assert_eq!(
        lines,
        vec![
            "Update on t",
            "  ->  Index Scan using t_pkey on t",
            "        Index Cond: (id = 2)",
        ],
        "plan was {lines:?}"
    );

    // Writing the PK itself needs every row for the UNIQUE check, so it scans —
    // and the child says so, as PG's does.
    let lines = explain_lines(&client, "EXPLAIN UPDATE t SET id = 9 WHERE id = 2").await?;
    assert_eq!(
        lines,
        vec![
            "Update on t",
            "  ->  Seq Scan on t",
            "        Filter: (id = 2)",
        ],
        "plan was {lines:?}"
    );

    let lines = explain_lines(&client, "EXPLAIN DELETE FROM t WHERE id = 2").await?;
    assert_eq!(
        lines,
        vec![
            "Delete on t",
            "  ->  Index Scan using t_pkey on t",
            "        Index Cond: (id = 2)",
        ],
        "plan was {lines:?}"
    );

    // A conjunct the key does not cover stays visible as the child's Filter.
    let lines = explain_lines(
        &client,
        "EXPLAIN DELETE FROM t WHERE id = 2 AND label = 'x'",
    )
    .await?;
    assert_eq!(
        lines,
        vec![
            "Delete on t",
            "  ->  Index Scan using t_pkey on t",
            "        Index Cond: (id = 2)",
            "        Filter: (label = x)",
        ],
        "plan was {lines:?}"
    );

    // ...and the probed statements touch exactly the rows the predicate names.
    let result = client
        .simple_query("UPDATE t SET label = 'hit' WHERE id = 2 RETURNING id, label")
        .await?;
    let returned = rows(&result);
    assert_eq!(returned.len(), 1);
    assert_eq!(returned[0].get(1), Some("hit"));

    client.simple_query("DELETE FROM t WHERE id = 1").await?;
    let result = client
        .simple_query("SELECT id, label FROM t ORDER BY id")
        .await?;
    let left = rows(&result);
    assert_eq!(left.len(), 2);
    assert_eq!(left[0].get(0), Some("2"));
    assert_eq!(left[0].get(1), Some("hit"));
    assert_eq!(left[1].get(0), Some("3"));

    Ok(())
}

/// A probe must never hand the same row to DML twice.
///
/// `TRUNCATE` swaps the heap's relfilenode without resetting the index, so a
/// stale `key -> tid` entry survives and the next insert reuses the very slot it
/// names. The probe then reports that tid twice — once for the stale entry, once
/// for the new one — and an unguarded UPDATE would write the row twice and report
/// `UPDATE 2` for a one-row table. The engine-side repair is tracked separately;
/// this pins the executor's guard.
#[tokio::test]
async fn a_probe_never_reports_the_same_row_twice() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (id int PRIMARY KEY, v text)")
        .await?;
    client.simple_query("INSERT INTO t VALUES (1, 'a')").await?;
    client.simple_query("TRUNCATE t").await?;
    client.simple_query("INSERT INTO t VALUES (1, 'b')").await?;

    let result = client
        .simple_query("UPDATE t SET v = 'c' WHERE id = 1 RETURNING id, v")
        .await?;
    assert_eq!(
        rows(&result).len(),
        1,
        "UPDATE ... RETURNING duplicated a row"
    );
    assert!(
        result
            .iter()
            .any(|m| matches!(m, tokio_postgres::SimpleQueryMessage::CommandComplete(1))),
        "expected the tag to count one row, got {result:?}"
    );

    let result = client.simple_query("SELECT id, v FROM t").await?;
    let left = rows(&result);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].get(1), Some("c"));

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
    assert!(
        lines[1].starts_with("Planning Time: "),
        "plan was {lines:?}"
    );
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
    assert!(
        lines[1].starts_with("Planning Time: "),
        "plan was {lines:?}"
    );
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
        vec![
            "Delete on t".to_string(),
            "  ->  Seq Scan on t".to_string(),
            "        Filter: (id = 1)".to_string(),
        ]
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
        .query(
            "EXPLAIN (FORMAT JSON) SELECT * FROM t WHERE id = $1",
            &[&2i32],
        )
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
    assert_eq!(
        rows(&listed).len(),
        1,
        "pg_namespace should list the temp schema"
    );

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
        .simple_query(
            "SELECT table_schema FROM information_schema.tables WHERE table_name = 'secret'",
        )
        .await?;
    let a_temp_schema = rows(&ns)[0]
        .get("table_schema")
        .expect("table_schema is present")
        .to_string();
    assert!(a_temp_schema.starts_with("pg_temp_"));

    // A second session must not read, write, or drop A's temp table by qualifier.
    let b = connect(port).await;
    for stmt in [
        format!("SELECT * FROM {a_temp_schema}.secret"),
        format!("INSERT INTO {a_temp_schema}.secret VALUES (99)"),
        format!("DROP TABLE {a_temp_schema}.secret"),
    ] {
        let err = b
            .simple_query(&stmt)
            .await
            .expect_err("another session's temp schema must not be reachable");
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
    client
        .simple_query("INSERT INTO src VALUES (1), (2)")
        .await?;
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
    let seen = other
        .simple_query("SELECT label FROM u ORDER BY id")
        .await?;
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
        .expect_err("adding two time values must find no unique operator");
    let db = err.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::AMBIGUOUS_FUNCTION);
    assert_eq!(
        db.message(),
        "operator is not unique: time without time zone + time without time zone"
    );
    assert_eq!(
        db.detail(),
        Some("Could not choose a best candidate operator.")
    );
    assert_eq!(
        db.hint(),
        Some("You might need to add explicit type casts.")
    );
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
        .simple_query("SELECT table_type FROM information_schema.tables WHERE table_name = 'v'")
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
        .expect_err("a plain CREATE VIEW over an existing view must be rejected");
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
        .expect_err("a table cannot take a name a view already holds");
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
        .expect_err("CREATE OR REPLACE VIEW dropping a column must be rejected");
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
    let err = client
        .simple_query("DROP TABLE v")
        .await
        .expect_err("DROP TABLE naming a view must be rejected");
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::WRONG_OBJECT_TYPE);
    assert_eq!(db.message(), "\"v\" is not a table");
    assert_eq!(db.hint(), Some("Use DROP VIEW to remove a view."));

    let err = client
        .simple_query("DROP VIEW t")
        .await
        .expect_err("DROP VIEW naming a table must be rejected");
    assert_eq!(
        err.as_db_error().expect("db error").message(),
        "\"t\" is not a view"
    );

    // RESTRICT: the table refuses to drop while the view depends on it.
    let err = client
        .simple_query("DROP TABLE t")
        .await
        .expect_err("dropping a table a view depends on must be refused under RESTRICT");
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::DEPENDENT_OBJECTS_STILL_EXIST);
    assert_eq!(
        db.message(),
        "cannot drop table t because other objects depend on it"
    );
    assert_eq!(db.detail(), Some("view v depends on table t"));

    // CASCADE drops the table and its dependent view.
    client.simple_query("DROP TABLE t CASCADE").await?;
    let err = client
        .simple_query("SELECT id FROM v")
        .await
        .expect_err("a view dropped by CASCADE must no longer resolve");
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
        .expect_err("inserting into a view must be refused");
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::FEATURE_NOT_SUPPORTED);
    assert_eq!(db.message(), "cannot insert into view \"v\"");

    let err = client
        .simple_query("UPDATE v SET id = 2")
        .await
        .expect_err("updating a view must be refused");
    assert_eq!(
        err.as_db_error().expect("db error").message(),
        "cannot update view \"v\""
    );

    let err = client
        .simple_query("DELETE FROM v")
        .await
        .expect_err("deleting from a view must be refused");
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
    assert_view_recursion(
        client
            .simple_query("SELECT a FROM v2")
            .await
            .expect_err("a view cycle must be detected rather than overflow the stack"),
        "v2",
    );

    // The shortest cycle — a view that reads itself — is detected at the same
    // point, and the error names that one view.
    client
        .simple_query("CREATE VIEW v AS SELECT a FROM t")
        .await?;
    client
        .simple_query("CREATE OR REPLACE VIEW v AS SELECT a FROM v")
        .await?;
    assert_view_recursion(
        client
            .simple_query("SELECT a FROM v")
            .await
            .expect_err("a view that reads itself must be detected as recursive"),
        "v",
    );
    Ok(())
}

/// A cycle closed through a subquery in an *expression* — a scalar subquery in
/// the target list, or an `IN (SELECT …)` in WHERE — rather than through FROM.
/// These are invisible to the stored dependency graph, which records only
/// FROM-position relations, but the expansion guard sees every reference.
#[tokio::test]
async fn a_view_cycle_through_an_expression_subquery_is_detected() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (a int4)").await?;
    client
        .simple_query("CREATE VIEW v2 AS SELECT a FROM t")
        .await?;
    client
        .simple_query("CREATE VIEW v3 AS SELECT a FROM v2")
        .await?;

    // Scalar subquery in the target list: v2 -> v3 -> v2.
    client
        .simple_query("CREATE OR REPLACE VIEW v2 AS SELECT (SELECT a FROM v3 LIMIT 1) AS a")
        .await?;
    assert_view_recursion(
        client
            .simple_query("SELECT a FROM v2")
            .await
            .expect_err("a cycle closed through a scalar subquery must be detected"),
        "v2",
    );

    // Same cycle, closed through an IN (SELECT …) in WHERE instead. v2 has to go
    // back to reading the table first: redefining it while it is recursive would
    // itself fail to bind (see `creating_a_view_over_a_recursive_view_errors_unlike_pg`).
    client
        .simple_query("CREATE OR REPLACE VIEW v2 AS SELECT a FROM t")
        .await?;
    client
        .simple_query("CREATE OR REPLACE VIEW v2 AS SELECT a FROM t WHERE a IN (SELECT a FROM v3)")
        .await?;
    assert_view_recursion(
        client
            .simple_query("SELECT a FROM v2")
            .await
            .expect_err("a cycle closed through an IN subquery must be detected"),
        "v2",
    );
    Ok(())
}

/// Two same-named views in different schemas are distinct nodes: a cycle that
/// runs through both is detected, and (crucially) a chain that merely mentions
/// the name twice is not mistaken for one.
#[tokio::test]
async fn view_recursion_is_detected_across_schemas() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (a int4)").await?;
    client.simple_query("CREATE SCHEMA s").await?;
    client
        .simple_query("CREATE VIEW s.v AS SELECT a FROM t")
        .await?;
    client
        .simple_query("CREATE VIEW public.v AS SELECT a FROM s.v")
        .await?;
    // public.v -> s.v is fine while s.v still reads the table.
    let rows = client.query("SELECT a FROM public.v", &[]).await?;
    assert!(rows.is_empty());

    // Close the loop: s.v -> public.v -> s.v.
    client
        .simple_query("CREATE OR REPLACE VIEW s.v AS SELECT a FROM public.v")
        .await?;
    assert_view_recursion(
        client.simple_query("SELECT a FROM s.v").await.expect_err(
            "a cycle across two same-named views in different schemas must be detected",
        ),
        "v",
    );
    Ok(())
}

/// Referencing the same view more than once is not recursion: a diamond, and a
/// self-join of one view, both expand fine. This is what makes the expansion
/// state a stack rather than a set of everything ever seen.
#[tokio::test]
async fn repeated_view_references_are_not_recursion() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .batch_execute("CREATE TABLE t (a int4); INSERT INTO t VALUES (1), (2)")
        .await?;
    client
        .simple_query("CREATE VIEW leaf AS SELECT a FROM t")
        .await?;
    // A diamond: mid reads leaf twice, top reads mid.
    client
        .simple_query("CREATE VIEW mid AS SELECT l1.a FROM leaf l1 JOIN leaf l2 ON l1.a = l2.a")
        .await?;
    client
        .simple_query("CREATE VIEW top AS SELECT a FROM mid")
        .await?;

    let rows = client.query("SELECT a FROM top ORDER BY a", &[]).await?;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<_, i32>(0), 1);

    // Two references to one view in a single query, as siblings.
    let rows = client
        .query("SELECT x.a FROM leaf, leaf x WHERE x.a = 1", &[])
        .await?;
    assert_eq!(rows.len(), 2);
    Ok(())
}

/// A cycle detected several levels down reports the *repeating* view, not the
/// one the statement named, and reports it identically every time.
///
/// This does not pin the guard's `Drop` — expansion state lives on the
/// statement's bind context, so it cannot outlive a failed statement even if the
/// pop were removed. `repeated_view_references_are_not_recursion` is what pins
/// the pop, via two sibling references within one statement.
#[tokio::test]
async fn a_recursion_error_does_not_poison_later_statements() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .batch_execute("CREATE TABLE t (a int4); INSERT INTO t VALUES (7)")
        .await?;
    client
        .simple_query("CREATE VIEW ok AS SELECT a FROM t")
        .await?;
    // outer_v -> mid -> bad, with bad self-referential: the cycle is three levels
    // below the view the statement names.
    client
        .simple_query("CREATE VIEW bad AS SELECT a FROM t")
        .await?;
    client
        .simple_query("CREATE VIEW mid AS SELECT a FROM bad")
        .await?;
    client
        .simple_query("CREATE VIEW outer_v AS SELECT a FROM mid")
        .await?;
    client
        .simple_query("CREATE OR REPLACE VIEW bad AS SELECT a FROM bad")
        .await?;

    assert_view_recursion(
        client
            .simple_query("SELECT a FROM outer_v")
            .await
            .expect_err("a view over a recursive view must be detected as recursive"),
        "bad",
    );
    // Same failure again: if the first attempt leaked, the second would report a
    // different relation (whichever stale key it hit first).
    assert_view_recursion(
        client
            .simple_query("SELECT a FROM outer_v")
            .await
            .expect_err("the repeated attempt must report the same relation, not a stale one"),
        "bad",
    );
    // And an unrelated view still binds against a clean stack.
    let row = client.query_one("SELECT a FROM ok", &[]).await?;
    assert_eq!(row.get::<_, i32>(0), 7);
    Ok(())
}

/// A view chain deep enough to exhaust the native stack is rejected with PG's
/// `54001` instead of aborting the process. Cycle detection alone does not cover
/// this: there is no cycle here, just depth.
#[tokio::test]
async fn a_view_chain_deeper_than_the_cap_is_rejected() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (a int4)").await?;
    client
        .simple_query("CREATE VIEW v0 AS SELECT a FROM t")
        .await?;
    // Build past the cap. The CREATE that first exceeds it fails the same way a
    // SELECT would, since CREATE VIEW binds its defining query.
    let mut depth = 0;
    let err = loop {
        depth += 1;
        assert!(depth < 200, "expected the depth cap to fire");
        let sql = format!("CREATE VIEW v{depth} AS SELECT a FROM v{}", depth - 1);
        if let Err(e) = client.simple_query(&sql).await {
            break e;
        }
    };
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::STATEMENT_TOO_COMPLEX);
    assert_eq!(db.message(), "views nested too deeply");
    Ok(())
}

/// A view that references another twice doubles the expansions per level, so a
/// short chain of them is 2^n re-parses of the same bodies. The per-statement
/// expansion budget stops that at bind time rather than hanging the server.
#[tokio::test]
async fn exponential_view_reference_growth_is_bounded() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (a int4)").await?;
    client
        .simple_query("CREATE VIEW w0 AS SELECT a FROM t")
        .await?;
    let mut level = 0;
    let err = loop {
        level += 1;
        assert!(level < 40, "expected the expansion budget to fire");
        let prev = level - 1;
        let sql = format!(
            "CREATE VIEW w{level} AS SELECT x.a FROM w{prev} x JOIN w{prev} y ON x.a = y.a"
        );
        if let Err(e) = client.simple_query(&sql).await {
            break e;
        }
    };
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::STATEMENT_TOO_COMPLEX);
    assert_eq!(db.message(), "too many view expansions in one statement");
    Ok(())
}

/// Documented divergence: we bind a view's defining query at CREATE VIEW to
/// derive its columns, so defining a view *over* an already-recursive one is an
/// error here. PG accepts it (rules are only expanded at rewrite time, and the
/// column list comes from the catalog) — see vendor/postgres/regress/sql/lock.sql,
/// which creates `lock_view7` over the self-referential `lock_view2`.
#[tokio::test]
async fn creating_a_view_over_a_recursive_view_errors_unlike_pg() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (a int4)").await?;
    client
        .simple_query("CREATE VIEW v2 AS SELECT a FROM t")
        .await?;
    client
        .simple_query("CREATE VIEW v3 AS SELECT a FROM v2")
        .await?;
    client
        .simple_query("CREATE OR REPLACE VIEW v2 AS SELECT a FROM v3")
        .await?;

    assert_view_recursion(
        client
            .simple_query("CREATE VIEW v7 AS SELECT a FROM v2")
            .await
            .expect_err("CREATE VIEW over a recursive view must be detected as recursive"),
        "v2",
    );
    Ok(())
}

/// PG accepts a CREATE VIEW column list shorter than the query's output (the
/// trailing columns keep their derived names); only a longer list is an error.
#[tokio::test]
async fn create_view_accepts_fewer_column_names() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE t (a int4, b int4)")
        .await?;
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
        .expect_err("more column names than the query outputs must be rejected");
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::SYNTAX_ERROR);
    assert_eq!(
        db.message(),
        "CREATE VIEW specifies more column names than columns"
    );
    Ok(())
}

/// A relation resolved through the search path (a temp table) shadows a
/// same-named permanent view, matching PG's `pg_temp`-before-`public` precedence.
#[tokio::test]
async fn temp_table_shadows_same_named_view() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE VIEW x AS SELECT 1 AS a")
        .await?;
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
    client
        .simple_query("INSERT INTO t (name) VALUES ('c')")
        .await?;
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
    let err = client
        .simple_query("SELECT currval('s')")
        .await
        .expect_err("currval before nextval in the session must be rejected");
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
    let n = client
        .query_one("SELECT nextval('a') AS v", &[])
        .await?
        .get::<_, i64>("v");
    client.query_one("SELECT setval('b', 7) AS v", &[]).await?; // must not touch lastval
    assert_eq!(
        client
            .query_one("SELECT lastval() AS v", &[])
            .await?
            .get::<_, i64>("v"),
        n,
        "lastval must reflect the nextval on a, not the setval on b"
    );
    // ...but setval DOES define currval for its sequence.
    assert_eq!(
        client
            .query_one("SELECT currval('b') AS v", &[])
            .await?
            .get::<_, i64>("v"),
        7
    );

    // setval out of the sequence's own [min,max] is 22003.
    let e = client
        .query_one("SELECT setval('b', 999)", &[])
        .await
        .expect_err("setval outside the sequence's own bounds must be rejected");
    assert_eq!(
        e.as_db_error().expect("a database error").code(),
        &SqlState::NUMERIC_VALUE_OUT_OF_RANGE
    );

    // setval with a NULL third argument is a NULL no-op (no side effect).
    let is_null: bool = client
        .query_one("SELECT setval('b', 3, NULL) IS NULL AS n", &[])
        .await?
        .get("n");
    assert!(is_null);
    // currval still 7 (the NULL setval did nothing).
    assert_eq!(
        client
            .query_one("SELECT currval('b') AS v", &[])
            .await?
            .get::<_, i64>("v"),
        7
    );

    // CREATE SEQUENCE with a bound outside the declared type is 22023.
    let e = client
        .batch_execute("CREATE SEQUENCE toobig AS smallint MAXVALUE 100000")
        .await
        .expect_err("a sequence bound outside its declared type must be rejected");
    assert_eq!(
        e.as_db_error().expect("a database error").code(),
        &SqlState::INVALID_PARAMETER_VALUE
    );

    // nextval on a table (existing non-sequence relation) is 42809, not 42P01.
    client.batch_execute("CREATE TABLE tab (id int)").await?;
    let e = client
        .query_one("SELECT nextval('tab')", &[])
        .await
        .expect_err("nextval on a table must be rejected as the wrong object type");
    assert_eq!(
        e.as_db_error().expect("a database error").code(),
        &SqlState::WRONG_OBJECT_TYPE
    );

    // currval after DROP errors 42P01 (no stale cached value).
    client.batch_execute("CREATE SEQUENCE gone; ").await?;
    client.query_one("SELECT nextval('gone') AS v", &[]).await?;
    client.batch_execute("DROP SEQUENCE gone").await?;
    let e = client
        .query_one("SELECT currval('gone')", &[])
        .await
        .expect_err("currval after the sequence is dropped must not return a cached value");
    assert_eq!(
        e.as_db_error().expect("a database error").code(),
        &SqlState::UNDEFINED_TABLE
    );

    // An index cannot take a sequence's name (shared relation namespace).
    let e = client
        .batch_execute("CREATE INDEX a ON tab (id)")
        .await
        .expect_err("an index cannot take a name a sequence already holds");
    assert_eq!(
        e.as_db_error().expect("a database error").code(),
        &SqlState::DUPLICATE_TABLE
    );

    // DROP SEQUENCE of a serial-owned sequence is blocked under RESTRICT (2BP01).
    client.batch_execute("CREATE TABLE ser (id serial)").await?;
    let e = client
        .batch_execute("DROP SEQUENCE ser_id_seq")
        .await
        .expect_err("dropping a serial-owned sequence under RESTRICT must be refused");
    assert_eq!(
        e.as_db_error().expect("a database error").code(),
        &SqlState::DEPENDENT_OBJECTS_STILL_EXIST
    );
    // CASCADE drops it.
    client
        .batch_execute("DROP SEQUENCE ser_id_seq CASCADE")
        .await?;
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
    let e = client
        .batch_execute("DROP INDEX t")
        .await
        .expect_err("DROP INDEX naming a table must be rejected");
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::WRONG_OBJECT_TYPE
    );

    // A constraint-backing index cannot be dropped directly (2BP01).
    let e = client
        .batch_execute("DROP INDEX t_pkey")
        .await
        .expect_err("dropping a constraint-backing index directly must be refused");
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::DEPENDENT_OBJECTS_STILL_EXIST
    );

    // A missing index is 42704 (UNDEFINED_OBJECT) — PG uses that for indexes,
    // not the 42P01 it uses for tables. IF EXISTS turns it into a skip NOTICE.
    let e = client
        .batch_execute("DROP INDEX nope")
        .await
        .expect_err("dropping an index that does not exist must be rejected");
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
        .expect_err("one index named twice in a DROP INDEX must be rejected");
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
    let e = client
        .batch_execute("DROP FUNCTION f_in")
        .await
        .expect_err("a bare name with two overloads must be rejected as ambiguous");
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::AMBIGUOUS_FUNCTION
    );

    // The same function named twice (a bare name and its signature) is rejected
    // as specified-more-than-once (42710), not silently accepted.
    let e = client
        .batch_execute("DROP FUNCTION f_in(int8), f_in(int8)")
        .await
        .expect_err("the same function named twice in one DROP must be rejected");
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
        .expect_err("dropping a signature that no longer exists must be rejected");
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
        .expect_err("dropping a function whose name does not exist must be rejected");
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
        .batch_execute(
            "CREATE FUNCTION f_out(int8, OUT int4) RETURNS int4 AS 'int8out' LANGUAGE internal",
        )
        .await?;
    client.batch_execute("DROP FUNCTION f_out(int8)").await?;
    Ok(())
}

/// CREATE INDEX without a name derives one from the table and its key columns
/// (`t_a_b_idx`), bumping the label on every kind of relation-name collision.
#[tokio::test]
async fn create_index_generates_name() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    let index_names = async |table: &str| -> anyhow::Result<Vec<String>> {
        let msgs = client
            .simple_query(&format!(
                "SELECT i.relname FROM pg_class i \
                 JOIN pg_index x ON x.indexrelid = i.oid \
                 JOIN pg_class t ON t.oid = x.indrelid \
                 WHERE t.relname = '{table}' ORDER BY 1"
            ))
            .await?;
        Ok(rows(&msgs)
            .iter()
            .map(|r| r.get(0).unwrap_or_default().to_string())
            .collect())
    };

    client
        .batch_execute("CREATE TABLE t (a int, b int)")
        .await?;
    client.batch_execute("CREATE INDEX ON t (a)").await?;
    assert_eq!(index_names("t").await?, ["t_a_idx"]);

    // Every key column joins the name, in index order.
    client.batch_execute("CREATE INDEX ON t (a, b)").await?;
    assert_eq!(index_names("t").await?, ["t_a_b_idx", "t_a_idx"]);

    // Repeats collide with the index just made, so the label is bumped. PG allows
    // the duplicate index itself — only the *name* has to be fresh.
    client.batch_execute("CREATE INDEX ON t (a)").await?;
    client.batch_execute("CREATE INDEX ON t (a)").await?;
    assert_eq!(
        index_names("t").await?,
        ["t_a_b_idx", "t_a_idx", "t_a_idx1", "t_a_idx2"]
    );

    // Indexes share the relation namespace, so a table (or view, or sequence) of
    // the generated name pushes the index off it too.
    client
        .batch_execute("CREATE TABLE u (a int); CREATE TABLE u_a_idx (x int)")
        .await?;
    client.batch_execute("CREATE INDEX ON u (a)").await?;
    assert_eq!(index_names("u").await?, ["u_a_idx1"]);

    // UNIQUE uses the same label as a plain index, and still enforces the key.
    client
        .batch_execute("CREATE TABLE q (a int); CREATE UNIQUE INDEX ON q (a)")
        .await?;
    assert_eq!(index_names("q").await?, ["q_a_idx"]);
    client.batch_execute("INSERT INTO q VALUES (1)").await?;
    let e = client
        .batch_execute("INSERT INTO q VALUES (1)")
        .await
        .expect_err("a duplicate key under the implicitly named unique index must be rejected");
    assert_eq!(
        e.as_db_error().context("missing error details")?.code(),
        &tokio_postgres::error::SqlState::UNIQUE_VIOLATION
    );

    // A temp table's generated names must dodge the indexes in this session's
    // temp schema, not the ones in `public`.
    client
        .batch_execute("CREATE TEMP TABLE tmp_t (a int)")
        .await?;
    client.batch_execute("CREATE INDEX ON tmp_t (a)").await?;
    client.batch_execute("CREATE INDEX ON tmp_t (a)").await?;
    let msgs = client
        .simple_query(
            "SELECT relname FROM pg_class WHERE relkind = 'i' \
             AND relname LIKE 'tmp\\_t%' ORDER BY relname",
        )
        .await?;
    let temp: Vec<_> = rows(&msgs).iter().map(|r| r.get(0)).collect();
    assert_eq!(temp, [Some("tmp_t_a_idx"), Some("tmp_t_a_idx1")]);

    // The name is derived after the key columns resolve, so a bad column is
    // reported as such rather than as a missing index name.
    let e = client
        .batch_execute("CREATE INDEX ON t (nope)")
        .await
        .expect_err("a key column that does not exist must be reported as such");
    assert_eq!(
        e.as_db_error().context("missing error details")?.code(),
        &tokio_postgres::error::SqlState::UNDEFINED_COLUMN
    );
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

/// `ALTER TABLE ... ADD PRIMARY KEY` on a populated table: it creates `t_pkey`,
/// reflects everywhere PostgreSQL reflects a primary key, and — the part that
/// needs the schema to actually change — makes the key columns NOT NULL.
#[tokio::test]
async fn alter_table_add_primary_key() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;
    client
        .batch_execute("CREATE TABLE t (a int, b int); INSERT INTO t VALUES (1, 10), (2, 20)")
        .await?;
    client
        .batch_execute("ALTER TABLE t ADD PRIMARY KEY (a)")
        .await?;

    let msgs = client
        .simple_query(
            "SELECT conname, contype FROM pg_constraint \
             WHERE conrelid = 't'::regclass ORDER BY 1",
        )
        .await?;
    let constraints: Vec<_> = rows(&msgs).iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(constraints, [(Some("t_pkey"), Some("p"))]);

    let msgs = client
        .simple_query(
            "SELECT x.indisprimary FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid \
             WHERE i.relname = 't_pkey'",
        )
        .await?;
    assert_eq!(rows(&msgs)[0].get(0), Some("t"));

    // The key column is NOT NULL now; the other one is untouched. Both spellings
    // of the question, since they read the schema through different row builders.
    let msgs = client
        .simple_query(
            "SELECT attname, attnotnull FROM pg_attribute \
             WHERE attrelid = 't'::regclass AND attnum > 0 ORDER BY attnum",
        )
        .await?;
    let notnull: Vec<_> = rows(&msgs).iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(notnull, [(Some("a"), Some("t")), (Some("b"), Some("f"))]);
    let msgs = client
        .simple_query(
            "SELECT column_name, is_nullable FROM information_schema.columns \
             WHERE table_name = 't' ORDER BY ordinal_position",
        )
        .await?;
    let nullable: Vec<_> = rows(&msgs).iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(
        nullable,
        [(Some("a"), Some("NO")), (Some("b"), Some("YES"))]
    );

    // And it is enforced, which is what the reflection above is claiming.
    let e = client
        .batch_execute("INSERT INTO t VALUES (NULL, 30)")
        .await
        .expect_err("a NULL in the NOT NULL column must be rejected");
    let e = e.as_db_error().context("missing error details")?;
    assert_eq!(e.code(), &SqlState::NOT_NULL_VIOLATION);
    assert_eq!(
        e.message(),
        "null value in column \"a\" of relation \"t\" violates not-null constraint"
    );
    let e = client
        .batch_execute("INSERT INTO t VALUES (1, 40)")
        .await
        .expect_err("a duplicate key in the unique column must be rejected");
    assert_eq!(
        e.as_db_error().context("missing error details")?.code(),
        &SqlState::UNIQUE_VIOLATION
    );
    Ok(())
}

/// The rows already in the table have to satisfy the constraint being added, and
/// a failure must leave the relation exactly as it was.
#[tokio::test]
async fn alter_table_add_constraint_validates_existing_rows() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;
    let index_count = async |name: &str| -> anyhow::Result<Option<String>> {
        let msgs = client
            .simple_query(&format!(
                "SELECT count(*) FROM pg_class WHERE relname = '{name}'"
            ))
            .await?;
        Ok(rows(&msgs)[0].get(0).map(str::to_string))
    };

    client
        .batch_execute("CREATE TABLE n (a int, b int); INSERT INTO n VALUES (1, 1), (NULL, 2)")
        .await?;
    let e = client
        .batch_execute("ALTER TABLE n ADD PRIMARY KEY (a)")
        .await
        .expect_err("a primary key over a column holding NULLs must be rejected");
    let e = e.as_db_error().context("missing error details")?;
    assert_eq!(e.code(), &SqlState::NOT_NULL_VIOLATION);
    assert_eq!(
        e.message(),
        "column \"a\" of relation \"n\" contains null values"
    );
    // Nothing was applied: no index, and the column still takes NULL.
    assert_eq!(index_count("n_pkey").await?.as_deref(), Some("0"));
    client
        .batch_execute("INSERT INTO n VALUES (NULL, 3)")
        .await?;

    // PostgreSQL builds the index before it verifies not-null, so a table with
    // both a duplicate and a NULL reports the duplicate — the reverse of the
    // order the same two constraints are reported in on INSERT.
    client
        .batch_execute("CREATE TABLE d (a int); INSERT INTO d VALUES (1), (1), (NULL)")
        .await?;
    let e = client
        .batch_execute("ALTER TABLE d ADD PRIMARY KEY (a)")
        .await
        .expect_err("a primary key over duplicate values must be rejected");
    let e = e.as_db_error().context("missing error details")?;
    assert_eq!(e.code(), &SqlState::UNIQUE_VIOLATION);
    assert_eq!(e.message(), "could not create unique index \"d_pkey\"");
    assert_eq!(e.detail(), Some("Key (a)=(1) is duplicated."));

    // Within an offending row PostgreSQL names the lowest-numbered column, not
    // the first in key order — `PRIMARY KEY (b, a)` over `(1, NULL)` says "b".
    client
        .batch_execute("CREATE TABLE o (a int, b int); INSERT INTO o VALUES (1, NULL)")
        .await?;
    let e = client
        .batch_execute("ALTER TABLE o ADD PRIMARY KEY (b, a)")
        .await
        .expect_err("the lowest-numbered NULL key column must be the one reported");
    assert_eq!(
        e.as_db_error().context("missing error details")?.message(),
        "column \"b\" of relation \"o\" contains null values"
    );
    Ok(())
}

/// Constraint index naming, and the errors that come out of resolving a name or
/// a second primary key.
#[tokio::test]
async fn alter_table_add_constraint_names_and_conflicts() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;
    let index_names = async |like: &str| -> anyhow::Result<Vec<String>> {
        let msgs = client
            .simple_query(&format!(
                "SELECT relname FROM pg_class WHERE relkind = 'i' \
                 AND relname LIKE '{like}' ORDER BY 1"
            ))
            .await?;
        Ok(rows(&msgs)
            .iter()
            .map(|r| r.get(0).unwrap_or_default().to_string())
            .collect())
    };

    client
        .batch_execute("CREATE TABLE u (a int, b int, c int)")
        .await?;
    // PRIMARY KEY collapses to `t_pkey`; UNIQUE is named after every key column.
    client
        .batch_execute("ALTER TABLE u ADD PRIMARY KEY (c)")
        .await?;
    client.batch_execute("ALTER TABLE u ADD UNIQUE (a)").await?;
    client
        .batch_execute("ALTER TABLE u ADD UNIQUE (a, b)")
        .await?;
    assert_eq!(index_names("u%").await?, ["u_a_b_key", "u_a_key", "u_pkey"]);

    // A repeat is allowed — PostgreSQL only requires the *name* to be fresh.
    client.batch_execute("ALTER TABLE u ADD UNIQUE (a)").await?;
    assert_eq!(index_names("u\\_a\\_key%").await?, ["u_a_key", "u_a_key1"]);

    // Two constraints in ONE statement collide with each other, and neither is
    // visible to the engine yet when the second name is picked.
    client.batch_execute("CREATE TABLE v (b int)").await?;
    client
        .batch_execute("ALTER TABLE v ADD UNIQUE (b), ADD UNIQUE (b)")
        .await?;
    assert_eq!(index_names("v%").await?, ["v_b_key", "v_b_key1"]);

    // An explicit name that is taken is a *relation* collision (42P07), not the
    // 42710 that CREATE TABLE's constraint-name check raises.
    client
        .batch_execute("ALTER TABLE u ADD CONSTRAINT c1 UNIQUE (b)")
        .await?;
    let e = client
        .batch_execute("ALTER TABLE u ADD CONSTRAINT c1 UNIQUE (c)")
        .await
        .expect_err("a constraint name already in use must be rejected");
    let e = e.as_db_error().context("missing error details")?;
    assert_eq!(e.code(), &SqlState::DUPLICATE_TABLE);
    assert_eq!(e.message(), "relation \"c1\" already exists");

    // A second primary key, whether against the stored one or within a statement.
    let e = client
        .batch_execute("ALTER TABLE u ADD PRIMARY KEY (a)")
        .await
        .expect_err("a second primary key on the table must be rejected");
    let e = e.as_db_error().context("missing error details")?;
    assert_eq!(e.code(), &SqlState::INVALID_TABLE_DEFINITION);
    assert_eq!(
        e.message(),
        "multiple primary keys for table \"u\" are not allowed"
    );
    client
        .batch_execute("CREATE TABLE w (a int, b int)")
        .await?;
    let e = client
        .batch_execute("ALTER TABLE w ADD PRIMARY KEY (a), ADD PRIMARY KEY (b)")
        .await
        .expect_err("two primary keys in one statement must be rejected");
    assert_eq!(
        e.as_db_error().context("missing error details")?.code(),
        &SqlState::INVALID_TABLE_DEFINITION
    );
    assert_eq!(index_names("w%").await?, Vec::<String>::new());
    Ok(())
}

/// A multi-action `ALTER TABLE` is all-or-nothing: validating every action
/// before applying any is what keeps the first one from landing when the second
/// is the one that fails.
#[tokio::test]
async fn alter_table_multi_action_is_atomic() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;
    client
        .batch_execute("CREATE TABLE m (a int, b int); INSERT INTO m VALUES (1, 1), (2, 1)")
        .await?;
    let e = client
        .batch_execute("ALTER TABLE m ADD UNIQUE (a), ADD UNIQUE (b)")
        .await
        .expect_err("a UNIQUE clause over duplicate values must fail the whole statement");
    assert_eq!(
        e.as_db_error().context("missing error details")?.code(),
        &SqlState::UNIQUE_VIOLATION
    );
    let msgs = client
        .simple_query("SELECT count(*) FROM pg_class WHERE relkind = 'i' AND relname LIKE 'm\\_%'")
        .await?;
    assert_eq!(
        rows(&msgs)[0].get(0),
        Some("0"),
        "the first action must not survive the second action's failure"
    );
    Ok(())
}

/// Name resolution and the forms that are refused.
#[tokio::test]
async fn alter_table_rejected_forms() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;
    let code = async |sql: &str| -> anyhow::Result<SqlState> {
        let e = client
            .batch_execute(sql)
            .await
            .expect_err("the statement must be rejected");
        Ok(e.as_db_error()
            .context("missing error details")?
            .code()
            .clone())
    };

    client
        .batch_execute("CREATE TABLE t (a int, b int); CREATE VIEW v AS SELECT * FROM t")
        .await?;

    assert_eq!(
        code("ALTER TABLE nosuch ADD PRIMARY KEY (a)").await?,
        SqlState::UNDEFINED_TABLE
    );
    // IF EXISTS turns that into a notice and a successful command.
    client
        .batch_execute("ALTER TABLE IF EXISTS nosuch ADD PRIMARY KEY (a)")
        .await?;

    // 25006 is raised before name resolution, so it wins over 42P01.
    client.batch_execute("BEGIN TRANSACTION READ ONLY").await?;
    assert_eq!(
        code("ALTER TABLE nosuch ADD PRIMARY KEY (a)").await?,
        SqlState::READ_ONLY_SQL_TRANSACTION
    );
    client.batch_execute("ROLLBACK").await?;

    assert_eq!(
        code("ALTER TABLE v ADD PRIMARY KEY (a)").await?,
        SqlState::WRONG_OBJECT_TYPE
    );
    assert_eq!(
        code("ALTER TABLE t ADD PRIMARY KEY (zz)").await?,
        SqlState::UNDEFINED_COLUMN
    );
    assert_eq!(
        code("ALTER TABLE t ADD PRIMARY KEY (a, a)").await?,
        SqlState::DUPLICATE_COLUMN
    );
    for sql in [
        "ALTER TABLE t ADD COLUMN z int",
        "ALTER TABLE t DROP CONSTRAINT nope",
        "ALTER TABLE t ALTER COLUMN a SET NOT NULL",
        "ALTER TABLE t RENAME TO t2",
        "ALTER TABLE t ADD FOREIGN KEY (a) REFERENCES t (a)",
        "ALTER TABLE t ADD PRIMARY KEY (a) NOT VALID",
        // A CHECK *is* supported now; only these two spellings of it are not.
        "ALTER TABLE t ADD CHECK (a > 0) NOT VALID",
        "ALTER TABLE t ADD CHECK (a > 0) NOT ENFORCED",
        "ALTER TABLE t ADD UNIQUE (b) DEFERRABLE",
        "ALTER TABLE public.t ADD UNIQUE (b)",
    ] {
        assert_eq!(
            code(sql).await?,
            SqlState::FEATURE_NOT_SUPPORTED,
            "expected {sql} to be rejected as unsupported"
        );
    }
    Ok(())
}

/// The pre-flight scan of `ALTER TABLE ... ADD CHECK` runs under the same
/// fully-wired runtime a statement gets, so a predicate calling a routine, a
/// sequence or a catalog function validates instead of failing internally. It
/// used to run under a bare formatting context, which made the DDL succeed on an
/// empty table and fail with XX000 on a populated one.
#[tokio::test]
async fn alter_table_add_check_evaluates_a_routine_over_existing_rows() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;
    client
        .batch_execute(
            "CREATE FUNCTION positive(int) RETURNS bool LANGUAGE sql AS 'SELECT $1 > 0';
             CREATE TABLE t (x int);
             INSERT INTO t VALUES (5)",
        )
        .await?;
    // The row satisfies it, so the constraint lands.
    client
        .batch_execute("ALTER TABLE t ADD CONSTRAINT c1 CHECK (positive(x))")
        .await?;
    // And enforces from then on — the same predicate, the same runtime.
    let e = client
        .batch_execute("INSERT INTO t VALUES (-1)")
        .await
        .expect_err("-1 fails positive()");
    assert_eq!(
        e.as_db_error().context("missing error details")?.code(),
        &SqlState::CHECK_VIOLATION
    );
    // A row that violates it is reported as a constraint violation, not as an
    // internal error from an unwired context.
    client.batch_execute("CREATE TABLE u (x int)").await?;
    client.batch_execute("INSERT INTO u VALUES (-3)").await?;
    let e = client
        .batch_execute("ALTER TABLE u ADD CONSTRAINT c2 CHECK (positive(x))")
        .await
        .expect_err("the existing row fails");
    let db = e.as_db_error().context("missing error details")?;
    assert_eq!(db.code(), &SqlState::CHECK_VIOLATION);
    assert_eq!(
        db.message(),
        "check constraint \"c2\" of relation \"u\" is violated by some row"
    );
    Ok(())
}

/// `DROP FUNCTION` refuses to strand a stored expression that calls it, the way
/// PostgreSQL's `pg_depend` does — for a CHECK predicate and a column DEFAULT
/// alike. Without this the drop succeeded and left the relation unwritable.
#[tokio::test]
async fn drop_function_reports_stored_expression_dependents() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;
    client
        .batch_execute(
            "CREATE FUNCTION g(int) RETURNS bool LANGUAGE sql AS 'SELECT $1 > 0';
             CREATE TABLE gt (x int, CHECK (g(x)));
             CREATE FUNCTION h() RETURNS int LANGUAGE sql AS 'SELECT 7';
             CREATE TABLE dt (y int DEFAULT h())",
        )
        .await?;

    let e = client
        .batch_execute("DROP FUNCTION g(int)")
        .await
        .expect_err("a CHECK depends on it");
    let db = e.as_db_error().context("missing error details")?;
    assert_eq!(db.code(), &SqlState::DEPENDENT_OBJECTS_STILL_EXIST);
    assert_eq!(
        db.message(),
        "cannot drop function g(integer) because other objects depend on it"
    );
    assert_eq!(
        db.detail(),
        Some("constraint gt_x_check on table gt depends on function g(integer)")
    );
    assert_eq!(
        db.hint(),
        Some("Use DROP ... CASCADE to drop the dependent objects too.")
    );

    let e = client
        .batch_execute("DROP FUNCTION h()")
        .await
        .expect_err("a DEFAULT depends on it");
    assert_eq!(
        e.as_db_error().context("missing error details")?.detail(),
        Some("default value for column y of table dt depends on function h()")
    );

    // The refusal is total: the function is still callable and the relation is
    // still writable.
    client.batch_execute("INSERT INTO gt VALUES (1)").await?;

    // A different overload is a different object.
    client
        .batch_execute(
            "CREATE FUNCTION g(text) RETURNS bool LANGUAGE sql AS 'SELECT true';
             DROP FUNCTION g(text)",
        )
        .await?;

    // CASCADE would have to drop the constraint, and nothing here can — so it is
    // refused rather than reported as done.
    let e = client
        .batch_execute("DROP FUNCTION g(int) CASCADE")
        .await
        .expect_err("CASCADE cannot drop the dependent constraint");
    assert_eq!(
        e.as_db_error().context("missing error details")?.code(),
        &SqlState::FEATURE_NOT_SUPPORTED
    );

    // Once the dependent is gone, so is the objection.
    client
        .batch_execute("DROP TABLE gt; DROP FUNCTION g(int)")
        .await?;
    Ok(())
}

/// A dependency is *direct*, as in PostgreSQL: a routine reached only through an
/// inlined SQL body is not one, so dropping it succeeds — while the routine the
/// expression names itself stays protected, even after that inner drop has made
/// the body unbindable.
#[tokio::test]
async fn drop_function_dependency_is_direct_only() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;
    client
        .batch_execute(
            "CREATE FUNCTION base() RETURNS int LANGUAGE sql AS 'SELECT 1';
             CREATE FUNCTION wrap() RETURNS int LANGUAGE sql AS 'SELECT base()';
             CREATE TABLE wt (x int DEFAULT wrap())",
        )
        .await?;
    // Reached only through `wrap`'s inlined body — not a dependency.
    client.batch_execute("DROP FUNCTION base()").await?;
    // Named by the default itself — still a dependency, even though the body no
    // longer binds.
    let e = client
        .batch_execute("DROP FUNCTION wrap()")
        .await
        .expect_err("the default names wrap() directly");
    assert_eq!(
        e.as_db_error().context("missing error details")?.code(),
        &SqlState::DEPENDENT_OBJECTS_STILL_EXIST
    );
    Ok(())
}

/// A subquery in a CHECK predicate is refused with PostgreSQL's own wording and
/// SQLSTATE — `0A000`, not the `42P17` the constraint-violation family might
/// suggest. Lives here rather than in the smoke suite because PostgreSQL puts
/// the error cursor on the subquery's opening paren and this parser's span for a
/// parenthesised subquery starts at `SELECT`, so the `LINE n: … ^` line cannot
/// match byte-for-byte.
#[tokio::test]
async fn check_constraint_rejects_a_subquery() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;
    let e = client
        .batch_execute("CREATE TABLE t (a int CHECK ((SELECT true)))")
        .await
        .expect_err("a subquery in a CHECK is refused");
    let db = e.as_db_error().context("missing error details")?;
    assert_eq!(db.code(), &SqlState::FEATURE_NOT_SUPPORTED);
    assert_eq!(db.message(), "cannot use subquery in check constraint");
    Ok(())
}

/// `ALTER TABLE ... ADD CONSTRAINT ... CHECK`: the existing rows are scanned
/// before it lands, and the constraint enforces from then on.
#[tokio::test]
async fn alter_table_add_check_constraint() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;
    client
        .batch_execute("CREATE TABLE t (a int); INSERT INTO t VALUES (1), (5)")
        .await?;

    // A row already in the table fails the predicate: rejected, and — unlike a
    // DML-time violation — with no DETAIL naming the row.
    let e = client
        .batch_execute("ALTER TABLE t ADD CONSTRAINT vc CHECK (a > 3)")
        .await
        .expect_err("row a = 1 violates a > 3");
    let db = e.as_db_error().context("missing error details")?;
    assert_eq!(db.code(), &SqlState::CHECK_VIOLATION);
    assert_eq!(
        db.message(),
        "check constraint \"vc\" of relation \"t\" is violated by some row"
    );
    assert_eq!(db.detail(), None);
    // The refusal is total: nothing was recorded.
    let msgs = client
        .simple_query("SELECT count(*) FROM pg_constraint WHERE contype = 'c'")
        .await?;
    assert_eq!(rows(&msgs)[0].get(0), Some("0"));

    // A predicate every row satisfies lands, and enforces from then on.
    client
        .batch_execute("ALTER TABLE t ADD CONSTRAINT vc CHECK (a > 0)")
        .await?;
    let e = client
        .batch_execute("INSERT INTO t VALUES (-1)")
        .await
        .expect_err("-1 violates the constraint just added");
    assert_eq!(
        e.as_db_error().context("missing error details")?.code(),
        &SqlState::CHECK_VIOLATION
    );

    // The name is taken now. A collision in the *constraint* namespace is 42710;
    // 42P07 is what a collision in the *relation* namespace raises (a plain
    // index, a table), which a CHECK never enters.
    let e = client
        .batch_execute("ALTER TABLE t ADD CONSTRAINT vc CHECK (a < 9)")
        .await
        .expect_err("the name is taken");
    let db = e.as_db_error().context("missing error details")?;
    assert_eq!(db.code(), &SqlState::DUPLICATE_OBJECT);
    assert_eq!(
        db.message(),
        "constraint \"vc\" for relation \"t\" already exists"
    );

    // An unnamed one is named from the predicate, not from where it was written:
    // one referenced column gives `{table}_{column}_check`.
    client
        .batch_execute("ALTER TABLE t ADD CHECK (a < 1000)")
        .await?;
    let msgs = client
        .simple_query(
            "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE contype = 'c' ORDER BY conname",
        )
        .await?;
    let found = rows(&msgs);
    assert_eq!(found[0].get(0), Some("t_a_check"));
    assert_eq!(found[0].get(1), Some("CHECK ((a < 1000))"));
    assert_eq!(found[1].get(0), Some("vc"));
    Ok(())
}

/// A PRIMARY KEY makes its key columns NOT NULL, and PostgreSQL pushes that down
/// the inheritance tree. We have no fan-out for it, so a parent is refused —
/// while UNIQUE, which PostgreSQL never inherits, is allowed on one.
#[tokio::test]
async fn alter_table_add_constraint_and_inheritance() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;
    client
        .batch_execute("CREATE TABLE par (a int, b int); CREATE TABLE chi (c int) INHERITS (par)")
        .await?;

    let e = client
        .batch_execute("ALTER TABLE par ADD PRIMARY KEY (b)")
        .await
        .expect_err("a primary key on a table with inheritance children must be rejected");
    assert_eq!(
        e.as_db_error().context("missing error details")?.code(),
        &SqlState::FEATURE_NOT_SUPPORTED
    );

    // UNIQUE on the parent lands on the parent alone, as in PostgreSQL.
    client
        .batch_execute("ALTER TABLE par ADD UNIQUE (a)")
        .await?;
    let msgs = client
        .simple_query(
            "SELECT relname FROM pg_class WHERE relkind = 'i' \
             AND (relname LIKE 'par%' OR relname LIKE 'chi%') ORDER BY 1",
        )
        .await?;
    let names: Vec<_> = rows(&msgs).iter().map(|r| r.get(0)).collect();
    assert_eq!(names, [Some("par_a_key")]);

    // A leaf child has nothing below it to recurse into, so it takes a key.
    client
        .batch_execute("ALTER TABLE chi ADD PRIMARY KEY (c)")
        .await?;
    let msgs = client
        .simple_query(
            "SELECT attname, attnotnull FROM pg_attribute \
             WHERE attrelid = 'chi'::regclass AND attnum > 0 ORDER BY attnum",
        )
        .await?;
    let notnull: Vec<_> = rows(&msgs).iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(
        notnull,
        [
            (Some("a"), Some("f")),
            (Some("b"), Some("f")),
            (Some("c"), Some("t"))
        ]
    );

    // Descent is transitive: a grandchild keeps the grandparent refused, even
    // though no link names the two directly.
    client
        .batch_execute("CREATE TABLE g1 (a int); CREATE TABLE g2 () INHERITS (g1)")
        .await?;
    client
        .batch_execute("CREATE TABLE g3 () INHERITS (g2)")
        .await?;
    let e = client
        .batch_execute("ALTER TABLE g1 ADD PRIMARY KEY (a)")
        .await
        .expect_err("a primary key on a table with a grandchild must be rejected");
    assert_eq!(
        e.as_db_error().context("missing error details")?.code(),
        &SqlState::FEATURE_NOT_SUPPORTED
    );

    // …and it is namespace-exact. A temp table shadowing a parent has no
    // children of its own, so the refusal must not follow the *name*: matching
    // bare names would let `public.par` veto a key on `pg_temp_N.par`.
    // PostgreSQL 18.4 accepts this and creates `pg_temp_N.par_pkey`.
    client
        .batch_execute("CREATE TEMP TABLE par (a int, b int)")
        .await?;
    client
        .batch_execute("ALTER TABLE par ADD PRIMARY KEY (a)")
        .await?;
    let msgs = client
        .simple_query(
            "SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relname = 'par_pkey'",
        )
        .await?;
    let schemas: Vec<_> = rows(&msgs).iter().map(|r| r.get(0)).collect();
    assert_eq!(schemas.len(), 1);
    assert!(
        schemas[0].unwrap_or_default().starts_with("pg_temp_"),
        "the key must land on the temp table, got {schemas:?}"
    );
    Ok(())
}

/// Only the heap can record a column as NOT NULL, so ADD PRIMARY KEY on another
/// access method is refused — as *unsupported*, and before anything is written.
///
/// Both halves are the regression. The refusal used to come from the engine in
/// the apply pass as a `TableNotFound`, so the client was told `relation "p"
/// does not exist` about a relation it had just queried; and because the engine
/// call is skipped when there is nothing to flip, the same statement succeeded
/// on the same table whenever the key column was already NOT NULL.
#[tokio::test]
async fn alter_table_add_primary_key_rejects_non_heap() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;
    client
        .batch_execute(
            "CREATE TABLE p (a int, b int) USING parquet ORDER BY (b); \
             INSERT INTO p VALUES (1, 2)",
        )
        .await?;

    let e = client
        .batch_execute("ALTER TABLE p ADD PRIMARY KEY (a)")
        .await
        .expect_err("a primary key on a parquet table must be rejected");
    let e = e.as_db_error().context("missing error details")?;
    assert_eq!(e.code(), &SqlState::FEATURE_NOT_SUPPORTED);
    assert_eq!(
        e.message(),
        "PRIMARY KEY on a table using access method \"parquet\" is not supported yet"
    );

    // Already NOT NULL: nothing to flip, so the old gate let this through.
    client
        .batch_execute("CREATE TABLE q (a int NOT NULL, b int) USING parquet ORDER BY (b)")
        .await?;
    assert_eq!(
        client
            .batch_execute("ALTER TABLE q ADD PRIMARY KEY (a)")
            .await
            .expect_err("a primary key on an ordered parquet table must be rejected")
            .as_db_error()
            .context("missing error details")?
            .code(),
        &SqlState::FEATURE_NOT_SUPPORTED
    );

    // UNIQUE changes no column, so it stays available — as `CREATE UNIQUE INDEX`
    // already is on these methods.
    client.batch_execute("ALTER TABLE p ADD UNIQUE (a)").await?;
    let msgs = client
        .simple_query("SELECT conname FROM pg_constraint WHERE conrelid = 'p'::regclass")
        .await?;
    assert_eq!(rows(&msgs)[0].get(0), Some("p_a_key"));
    Ok(())
}

/// Both halves of ADD PRIMARY KEY — the index and the NOT NULL flip — have to
/// reach the session's temp table, not a same-named permanent one.
#[tokio::test]
async fn alter_table_add_primary_key_temp_shadowing() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .batch_execute("CREATE TABLE sh (a int, b int); CREATE TEMP TABLE sh (a int, b int)")
        .await?;
    client
        .batch_execute("ALTER TABLE sh ADD PRIMARY KEY (a)")
        .await?;

    let msgs = client
        .simple_query(
            "SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relname = 'sh_pkey'",
        )
        .await?;
    let schemas: Vec<_> = rows(&msgs).iter().map(|r| r.get(0)).collect();
    assert_eq!(schemas.len(), 1);
    assert!(
        schemas[0].unwrap_or_default().starts_with("pg_temp_"),
        "the index must land in the temp schema, got {schemas:?}"
    );

    // The permanent table must be untouched — including its nullability, which
    // travels through a different engine call than the index does.
    let msgs = client
        .simple_query(
            "SELECT a.attnotnull FROM pg_attribute a \
             JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relname = 'sh' AND n.nspname = 'public' AND a.attnum > 0 ORDER BY a.attnum",
        )
        .await?;
    let notnull: Vec<_> = rows(&msgs).iter().map(|r| r.get(0)).collect();
    assert_eq!(notnull, [Some("f"), Some("f")]);
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
    let e = client
        .query_one("SELECT nextval('s')", &[])
        .await
        .expect_err("nextval in a read-only transaction must be refused");
    assert_eq!(
        e.as_db_error().expect("a database error").code(),
        &SqlState::READ_ONLY_SQL_TRANSACTION
    );
    client.batch_execute("ROLLBACK").await?;
    // The counter did not advance despite the rejected nextval.
    assert_eq!(
        client
            .query_one("SELECT nextval('s') AS v", &[])
            .await?
            .get::<_, i64>("v"),
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

#[tokio::test]
async fn parquet_tables_support_append_workflows_and_reject_mutation() -> anyhow::Result<()> {
    use bytes::Bytes;
    use futures_util::SinkExt;

    let client = connect(spawn_server().await).await;
    client
        .simple_query(
            "CREATE TABLE p (id int4 PRIMARY KEY, label text, payload bytea) USING parquet",
        )
        .await?;
    client
        .simple_query("INSERT INTO p VALUES (1, 'one', '\\x0102'), (2, NULL, NULL)")
        .await?;

    let messages = client
        .simple_query("SELECT id, label, encode(payload, 'hex') FROM p ORDER BY id")
        .await?;
    let result = rows(&messages);
    assert_eq!(result.len(), 2);
    assert_eq!(
        (result[0].get(0), result[0].get(1), result[0].get(2)),
        (Some("1"), Some("one"), Some("0102"))
    );
    assert_eq!(
        (result[1].get(0), result[1].get(1), result[1].get(2)),
        (Some("2"), None, None)
    );

    let catalog = client
        .simple_query(
            "SELECT a.oid, a.amname FROM pg_class c JOIN pg_am a ON a.oid = c.relam \
             WHERE c.relname = 'p'",
        )
        .await?;
    assert_eq!(
        (rows(&catalog)[0].get(0), rows(&catalog)[0].get(1)),
        (Some("16000"), Some("parquet"))
    );

    let sink = client.copy_in("COPY p (id, label) FROM STDIN").await?;
    futures_util::pin_mut!(sink);
    sink.send(Bytes::from_static(b"3\tthree\n4\tfour\n"))
        .await?;
    assert_eq!(sink.finish().await?, 2);

    client
        // CTAS declares no constraints, so there is no PRIMARY KEY to default
        // from and the key must be spelled out.
        .simple_query("CREATE TABLE p_copy USING parquet ORDER BY (id) AS SELECT id, label FROM p")
        .await?;
    let copied = client.simple_query("SELECT count(*) FROM p_copy").await?;
    assert_eq!(rows(&copied)[0].get(0), Some("4"));

    let duplicate = client
        .simple_query("INSERT INTO p (id) VALUES (1)")
        .await
        .expect_err("PRIMARY KEY remains semantically enforced");
    assert_eq!(
        duplicate
            .as_db_error()
            .expect("database error")
            .code()
            .code(),
        "23505"
    );

    for (sql, verb) in [
        ("UPDATE p SET label = 'x'", "UPDATE"),
        ("DELETE FROM p", "DELETE"),
    ] {
        let error = client
            .simple_query(sql)
            .await
            .expect_err("append-only mutation must fail");
        let error = error.as_db_error().expect("database error");
        assert_eq!(error.code().code(), "0A000");
        assert_eq!(
            error.message(),
            format!("table access method \"parquet\" does not support {verb}")
        );
    }

    let unknown = client
        .simple_query("CREATE TABLE unknown_am (id int) USING imaginary")
        .await
        .expect_err("unknown table access method must fail");
    let unknown = unknown.as_db_error().expect("database error");
    assert_eq!(unknown.code().code(), "42704");
    // Wording matches PostgreSQL's `get_am_type_oid`, which says "access method",
    // not "table access method".
    assert_eq!(
        unknown.message(),
        "access method \"imaginary\" does not exist"
    );
    // Each carries a sort key so the form under test is what fails: without one
    // the `42P17` "requires ORDER BY or PRIMARY KEY" would mask it.
    for sql in [
        "CREATE TEMP TABLE temp_p (id int) USING parquet ORDER BY (id)",
        "CREATE UNLOGGED TABLE unlogged_p (id int) USING parquet ORDER BY (id)",
        "CREATE TABLE partitioned_p (id int) USING parquet PARTITION BY RANGE (id) ORDER BY (id)",
        "CREATE TABLE unsupported_p (value jsonb) USING parquet ORDER BY (value)",
        "CREATE TABLE located_p (id int) USING parquet LOCATION '/tmp/data' ORDER BY (id)",
    ] {
        let error = client
            .simple_query(sql)
            .await
            .expect_err("unsupported Parquet table form must fail");
        assert_eq!(
            error.as_db_error().expect("database error").code().code(),
            "0A000",
            "{sql}"
        );
    }
    Ok(())
}

/// An engine-managed relation must declare the order it stores rows in: an
/// explicit `ORDER BY (...)`, or a `PRIMARY KEY` to default from. This is
/// ClickHouse MergeTree's rule, not PostgreSQL's — PostgreSQL has no such
/// clause — and refusing the keyless table is the whole point of adopting it.
#[tokio::test]
async fn an_engine_managed_table_must_declare_its_sort_key() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    // The accepted shapes: an explicit key (single and composite), a PRIMARY KEY
    // to default from, and the same for the buffer method.
    for sql in [
        "CREATE TABLE k1 (id int4, at timestamp) USING parquet ORDER BY (id)",
        "CREATE TABLE k2 (id int4, at timestamp) USING parquet ORDER BY (at, id)",
        "CREATE TABLE k3 (id int4 PRIMARY KEY, at timestamp) USING parquet",
        "CREATE TABLE k4 (a int4, b int4, PRIMARY KEY (a, b)) USING parquet",
        "CREATE TABLE k5 (id int4) USING buffer ORDER BY (id)",
        "CREATE TABLE k6 (id int4 PRIMARY KEY) USING buffer",
        // A single column needs no parentheses. Two do — see the parse-error
        // case below; the paren-free form does not generalize.
        "CREATE TABLE k7 (id int4) USING parquet ORDER BY id",
    ] {
        client
            .simple_query(sql)
            .await
            .with_context(|| sql.to_string())?;
    }

    let fails = async |sql: &str| -> anyhow::Result<(String, String, Option<String>)> {
        let error = client
            .simple_query(sql)
            .await
            .expect_err("invalid sort key must fail");
        let error = error
            .as_db_error()
            .context("database error details are missing")?;
        Ok((
            error.code().code().to_string(),
            error.message().to_string(),
            error.hint().map(str::to_string),
        ))
    };

    // Neither clause: refused. The HINT leads with ORDER BY and prices the
    // PRIMARY KEY alternative, because a unique index on an engine-managed table
    // makes every insert scan the relation — the two are not interchangeable.
    let (code, message, hint) = fails("CREATE TABLE n1 (id int4) USING parquet").await?;
    assert_eq!(code, "42P17");
    assert_eq!(
        message,
        "table access method \"parquet\" requires ORDER BY or PRIMARY KEY"
    );
    assert_eq!(
        hint.as_deref(),
        Some(
            "Add ORDER BY (columns) to the CREATE TABLE. A PRIMARY KEY supplies \
             one too, at the cost of enforcing uniqueness on every insert."
        )
    );
    // The buffer method names itself rather than borrowing parquet's wording.
    let (code, message, _) = fails("CREATE TABLE n2 (id int4) USING buffer").await?;
    assert_eq!(code, "42P17");
    assert_eq!(
        message,
        "table access method \"buffer\" requires ORDER BY or PRIMARY KEY"
    );

    // `ORDER BY ()` is ClickHouse's opt-out (`ORDER BY tuple()`). We have none:
    // an unordered column store gives up pruning, compression locality, and
    // merge-friendly compaction. It gets its own message — the user did declare
    // something, it was just empty.
    let (code, message, _) = fails("CREATE TABLE n3 (id int4) USING parquet ORDER BY ()").await?;
    assert_eq!(code, "42P17");
    assert_eq!(message, "ORDER BY must name at least one column");

    let (code, message, _) =
        fails("CREATE TABLE n4 (id int4) USING parquet ORDER BY (nope)").await?;
    assert_eq!(code, "42703");
    assert_eq!(message, "column \"nope\" named in sort key does not exist");

    let (code, message, _) =
        fails("CREATE TABLE n5 (id int4) USING parquet ORDER BY (id, id)").await?;
    assert_eq!(code, "42P17");
    assert_eq!(message, "sort key column \"id\" appears more than once");

    let (code, message, _) =
        fails("CREATE TABLE n6 (id int4) USING parquet ORDER BY (id + 1)").await?;
    assert_eq!(code, "0A000");
    assert_eq!(
        message,
        "only simple column references are supported in ORDER BY"
    );

    // A key the storage layer cannot order is refused rather than recorded and
    // ignored: `numeric` is stored as text in a fragment, so Arrow's order over
    // it is a string order, and `timetz` is a struct no kernel orders at all.
    let (code, message, hint) =
        fails("CREATE TABLE n10 (n numeric) USING parquet ORDER BY (n)").await?;
    assert_eq!(code, "42P17");
    assert_eq!(
        message,
        "column \"n\" of type numeric cannot be used in a sort key"
    );
    assert_eq!(
        hint.as_deref(),
        Some("Name a column the storage layer can order.")
    );
    let (code, message, _) =
        fails("CREATE TABLE n11 (t timetz, id int4) USING parquet ORDER BY (t)").await?;
    assert_eq!(code, "42P17");
    assert_eq!(
        message,
        "column \"t\" of type time with time zone cannot be used in a sort key"
    );
    // Inherited from the PRIMARY KEY, so the remedy is a different clause
    // rather than a different column in the one they wrote.
    let (code, message, hint) =
        fails("CREATE TABLE n12 (n numeric PRIMARY KEY) USING parquet").await?;
    assert_eq!(code, "42P17");
    assert_eq!(
        message,
        "column \"n\" of type numeric cannot be used in a sort key"
    );
    assert_eq!(
        hint.as_deref(),
        Some(
            "The PRIMARY KEY supplies the sort key. Add an explicit ORDER BY \
             (columns) naming a column the storage layer can order."
        )
    );

    // The rule is asked of the method, not of every engine-managed one. A
    // standalone `USING buffer` relation stores nothing in key order to begin
    // with, so refusing one of its key columns would guard a promise it never
    // makes — and would break DDL that worked before the rule existed.
    for sql in [
        "CREATE TABLE k8 (n numeric) USING buffer ORDER BY (n)",
        "CREATE TABLE k9 (n numeric PRIMARY KEY) USING buffer",
        // `"char"` is stored as `UInt8` precisely so Arrow's order is its own
        // unsigned one, so parquet can honor it and must not refuse it.
        "CREATE TABLE k10 (c \"char\") USING parquet ORDER BY (c)",
        "CREATE TABLE k11 (c \"char\" PRIMARY KEY) USING parquet",
    ] {
        client
            .simple_query(sql)
            .await
            .with_context(|| sql.to_string())?;
    }

    // A heap has no layout order to declare, so the clause is refused rather
    // than recorded and never honored.
    let (code, message, _) = fails("CREATE TABLE n7 (id int4) ORDER BY (id)").await?;
    assert_eq!(code, "0A000");
    assert_eq!(
        message,
        "table access method \"heap\" does not support ORDER BY"
    );

    // Redshift's `SORTKEY` parses too, and would be a second spelling of the
    // same thing; it is refused so only one spelling can ever be recorded.
    let (code, message, _) = fails("CREATE TABLE n8 (id int4) USING parquet SORTKEY (id)").await?;
    assert_eq!(code, "0A000");
    assert_eq!(
        message,
        "CREATE TABLE ... SORTKEY is not supported; use ORDER BY (columns)"
    );

    // The paren-free form takes exactly one column; two is a parse error, so the
    // single-column spelling accepted above must not be read as general.
    assert!(
        client
            .simple_query("CREATE TABLE n9 (a int4, b int4) USING parquet ORDER BY a, b")
            .await
            .is_err()
    );

    // CTAS carries no constraints, so it can never default from a PRIMARY KEY.
    client
        .simple_query("CREATE TABLE src (id int4, label text)")
        .await?;
    let (code, _, _) = fails("CREATE TABLE c1 USING parquet AS SELECT id FROM src").await?;
    assert_eq!(code, "42P17");
    client
        .simple_query("CREATE TABLE c2 USING parquet ORDER BY (id) AS SELECT id FROM src")
        .await?;

    // A trailing ORDER BY belongs to the query, not the table. Saying only
    // "requires ORDER BY" to someone who just wrote one is useless, so the hint
    // names the position.
    let (code, _, hint) =
        fails("CREATE TABLE c3 USING parquet AS SELECT id FROM src ORDER BY id").await?;
    assert_eq!(code, "42P17");
    assert_eq!(
        hint.as_deref(),
        Some(
            "The trailing ORDER BY orders the query, not the table. Write \
             ORDER BY (columns) before AS to declare the table's sort key."
        )
    );

    // SORTKEY is refused on the CTAS path too — it reaches a different function,
    // which is how it went on being silently dropped after the plain path closed.
    let (code, message, _) = fails(
        "CREATE TABLE c4 USING parquet ORDER BY (id) SORTKEY (label) AS SELECT id, label FROM src",
    )
    .await?;
    assert_eq!(code, "0A000");
    assert_eq!(
        message,
        "this CREATE TABLE ... AS form is not supported yet"
    );

    Ok(())
}

/// The sort-key rule must not fire on statements it has no business failing: an
/// `IF NOT EXISTS` re-run, and a leaf partition that quietly swallowed the
/// clause the plain heap path rejects.
#[tokio::test]
async fn the_sort_key_rule_does_not_reach_past_its_own_statements() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    // An idempotent bootstrap script re-runs its DDL. A database written before
    // sort keys existed is full of keyless engine-managed relations, so the
    // second run must skip rather than demand a key for a table already there.
    client
        .simple_query("CREATE TABLE t (id int4) USING parquet ORDER BY (id)")
        .await?;
    client
        .simple_query("CREATE TABLE IF NOT EXISTS t (id int4) USING parquet")
        .await?;
    client
        .simple_query("CREATE TABLE IF NOT EXISTS t USING parquet AS SELECT 1 AS id")
        .await?;

    // A leaf partition is a heap and declares no order; the clause is refused,
    // not dropped, exactly as on a plain heap.
    client
        .simple_query("CREATE TABLE par (id int4) PARTITION BY RANGE (id)")
        .await?;
    let error = client
        .simple_query(
            "CREATE TABLE par_1 PARTITION OF par FOR VALUES FROM (1) TO (10) ORDER BY (id)",
        )
        .await
        .expect_err("ORDER BY on a leaf partition must fail");
    let error = error
        .as_db_error()
        .context("database error details are missing")?;
    assert_eq!(error.code().code(), "0A000");
    assert_eq!(
        error.message(),
        "table access method \"heap\" does not support ORDER BY"
    );

    // A PRIMARY KEY supplies the sort key verbatim, so anything it can express
    // becomes a stored key. Both of these are PostgreSQL errors in their own
    // right, and both would otherwise smuggle in a key `ORDER BY` cannot write.
    for (sql, code, message) in [
        (
            "CREATE TABLE d1 (a int4, b int4, PRIMARY KEY (a, a)) USING parquet",
            "42701",
            "column \"a\" appears twice in primary key constraint",
        ),
        (
            "CREATE TABLE d2 (a int4, PRIMARY KEY (a DESC)) USING parquet",
            "42601",
            "syntax error at or near \"DESC\"",
        ),
        (
            "CREATE TABLE d3 (a int4, b int4, UNIQUE (b, b)) USING parquet ORDER BY (a)",
            "42701",
            "column \"b\" appears twice in unique constraint",
        ),
    ] {
        let error = client
            .simple_query(sql)
            .await
            .expect_err("an invalid constraint key must fail");
        let error = error
            .as_db_error()
            .context("database error details are missing")?;
        assert_eq!(error.code().code(), code, "{sql}");
        assert_eq!(error.message(), message, "{sql}");
    }

    // The access method's own type whitelist wins over the sort key's B-tree
    // rule, so the user is told the truth instead of being sent round a loop.
    let error = client
        .simple_query("CREATE TABLE j (v json) USING parquet ORDER BY (v)")
        .await
        .expect_err("an unsupported column type must fail");
    let error = error
        .as_db_error()
        .context("database error details are missing")?;
    assert_eq!(error.code().code(), "0A000");
    assert_eq!(
        error.message(),
        "data type json is not supported by table access method \"parquet\""
    );

    Ok(())
}

/// A Parquet relation is physically two stores — the immutable chunks and its RAM
/// A columnar plan must answer exactly what the row plan answers.
///
/// Each case here is a bug the columnar path shipped with: a predicate with no
/// column reference produced a mask describing one value, and Arrow's filter
/// truncates the batch to the mask rather than rejecting it, so `WHERE 1=1`
/// returned one row per batch; a constant of a type Arrow cannot encode was
/// accepted by the projection compiler and only failed once a batch arrived.
/// Both were invisible to the unit tests, which drive the operators directly.
#[tokio::test]
async fn a_columnar_plan_answers_what_the_row_plan_answers() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE p (id int4, label text) USING parquet ORDER BY (id)")
        .await?;
    client
        .simple_query("CREATE TABLE h (id int4, label text)")
        .await?;
    for table in ["p", "h"] {
        client
            .simple_query(&format!(
                "INSERT INTO {table} VALUES (1,'a'),(2,'b'),(3,'c'),(4,NULL),(5,'e')"
            ))
            .await?;
    }

    let ids = |messages: &Vec<tokio_postgres::SimpleQueryMessage>| {
        rows(messages)
            .iter()
            .filter_map(|r| r.get(0).map(str::to_owned))
            .collect::<Vec<_>>()
    };
    for tail in [
        // Constant-only predicates: every row, or none, never one per batch.
        "WHERE 1=1 ORDER BY id",
        "WHERE true ORDER BY id",
        "WHERE NULL IS NULL ORDER BY id",
        "WHERE 1=2 ORDER BY id",
        // A constant beside a column: the operands have different lengths, and
        // Arrow's Kleene kernels reject that outright.
        "WHERE id = 1 AND true ORDER BY id",
        "WHERE id > 3 OR false ORDER BY id",
        // A predicate the columnar filter declines, combined with a sort it
        // accepts: the row Filter must still run, and run first.
        "WHERE label LIKE 'a%' ORDER BY id",
    ] {
        let columnar = client
            .simple_query(&format!("SELECT id FROM p {tail}"))
            .await?;
        let row = client
            .simple_query(&format!("SELECT id FROM h {tail}"))
            .await?;
        assert_eq!(ids(&columnar), ids(&row), "disagreement on: {tail}");
    }

    // A constant whose type has no Arrow encoding is legal in a target list
    // even on a relation that could never store one.
    for literal in ["'{}'::json", "'{}'::jsonb", "'1.2.3.4'::inet", "42"] {
        let columnar = client
            .simple_query(&format!("SELECT {literal} AS c, id FROM p ORDER BY id"))
            .await?;
        let row = client
            .simple_query(&format!("SELECT {literal} AS c, id FROM h ORDER BY id"))
            .await?;
        assert_eq!(ids(&columnar), ids(&row), "disagreement on: {literal}");
    }
    Ok(())
}

/// A standalone `USING buffer` relation reaches the columnar path too.
///
/// It only does so because `ManagedTable` forwards the batch-scan methods; both
/// have trait defaults, so a wrapper that drops them compiles cleanly and
/// silently reports "no batch path" for a leaf that has one.
#[tokio::test]
async fn a_buffer_relation_vectorizes() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE b (id int4) USING buffer ORDER BY (id)")
        .await?;
    client
        .simple_query("INSERT INTO b VALUES (3),(1),(2)")
        .await?;

    let lines = explain_lines(&client, "EXPLAIN SELECT id FROM b WHERE id > 1 ORDER BY id").await?;
    assert_eq!(
        lines.first().map(String::as_str),
        Some("Seq Scan on b (columnar: scan, filter, sort)"),
        "{lines:?}"
    );
    Ok(())
}

/// write buffer — so it plans as an `Append` over both. The leaves are not
/// catalog relations, so each labels itself and neither appears in `pg_class`.
#[tokio::test]
async fn a_parquet_relation_plans_as_an_append_over_its_storage_leaves() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE p (id int4, label text) USING parquet ORDER BY (id)")
        .await?;
    client
        .simple_query("INSERT INTO p VALUES (1, 'one')")
        .await?;

    let lines = explain_lines(&client, "EXPLAIN SELECT * FROM p").await?;
    assert_eq!(
        lines,
        vec![
            // Both leaves hand up Arrow batches, so the scan runs columnar and
            // says so. Nothing else vectorizes here — there is no WHERE and no
            // ORDER BY to vectorize.
            "Append (columnar: scan)".to_string(),
            "  ->  Seq Scan on p".to_string(),
            "  ->  Buffer Scan on p".to_string(),
        ],
        "the two leaves must be distinguishable, not the same line twice"
    );

    // Splitting the relation across leaves must not cost the plan its predicate
    // or its column names — both live on nodes above the Append.
    client
        .simple_query("CREATE TABLE h (id int4, label text)")
        .await?;
    let lines = explain_lines(&client, "EXPLAIN SELECT * FROM p WHERE id = 1").await?;
    assert_eq!(
        lines,
        vec![
            // `id = 1` is an integer equality against a constant, so it compiles
            // to an Arrow filter and runs below the row boundary.
            "Append (columnar: scan, filter)".to_string(),
            "  Filter: (id = 1)".to_string(),
            "  ->  Seq Scan on p".to_string(),
            "  ->  Buffer Scan on p".to_string(),
        ],
        "a WHERE on a split relation must still be rendered, and by column name"
    );
    // The annotation must be earned, not assumed. A `LIKE` has no Arrow kernel
    // here and `id + 1` is a computed sort key, so each declines its own step
    // while the scan stays columnar — if either over-claimed, EXPLAIN would be
    // reporting work that never happens.
    for (query, expected) in [
        (
            "EXPLAIN SELECT id FROM p ORDER BY id",
            "Append (columnar: scan, sort)",
        ),
        (
            "EXPLAIN SELECT id FROM p WHERE label LIKE 'a%'",
            "Append (columnar: scan)",
        ),
        (
            "EXPLAIN SELECT id FROM p ORDER BY id + 1",
            "Append (columnar: scan)",
        ),
    ] {
        let lines = explain_lines(&client, query).await?;
        assert_eq!(lines.first().map(String::as_str), Some(expected), "{query}");
    }
    // A heap relation has no batch path at all, so its plan is untouched — the
    // divergence from PostgreSQL's EXPLAIN is confined to plans that vectorize.
    client.simple_query("CREATE TABLE hh (id int4)").await?;
    client.simple_query("INSERT INTO hh VALUES (1)").await?;
    let lines = explain_lines(&client, "EXPLAIN SELECT id FROM hh WHERE id = 1").await?;
    assert_eq!(
        lines,
        vec![
            "Seq Scan on hh".to_string(),
            "  Filter: (id = 1)".to_string()
        ],
        "a row-path plan must render exactly as it did before"
    );

    let lines = explain_lines(&client, "EXPLAIN SELECT * FROM p JOIN h ON p.id = h.id").await?;
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Hash Cond: (id = id)")),
        "join keys over a split relation must render by name, not as $n: {lines:?}"
    );

    // The leaves are engine-internal: nothing new is addressable from SQL.
    let messages = client
        .simple_query("SELECT relname FROM pg_catalog.pg_class WHERE relname LIKE 'p%'")
        .await?;
    assert_eq!(
        rows(&messages).iter().map(|r| r.get(0)).collect::<Vec<_>>(),
        vec![Some("p")],
        "a storage leaf must not gain a pg_class row"
    );
    Ok(())
}

/// A committed TRUNCATE empties the relation outright, even when the truncating
/// transaction holds a snapshot that predates rows another session committed —
/// the durable and buffered halves must not disagree about what "all rows" means.
#[tokio::test]
async fn truncate_under_repeatable_read_leaves_no_rows_behind() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let truncater = connect(port).await;
    let other = connect(port).await;
    truncater
        .simple_query("CREATE TABLE p (id int4) USING parquet ORDER BY (id)")
        .await?;
    truncater.simple_query("INSERT INTO p VALUES (1)").await?;

    // Pin the truncater's snapshot before the other session's row exists.
    truncater
        .simple_query("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await?;
    let messages = truncater.simple_query("SELECT id FROM p").await?;
    assert_eq!(rows(&messages).len(), 1);

    other.simple_query("INSERT INTO p VALUES (999)").await?;

    truncater.simple_query("TRUNCATE p").await?;
    truncater.simple_query("COMMIT").await?;

    let messages = other.simple_query("SELECT id FROM p ORDER BY id").await?;
    assert!(
        rows(&messages).is_empty(),
        "a committed TRUNCATE must leave no row behind, including one it could not see"
    );
    Ok(())
}

/// `VACUUM` may not reclaim a version a live reader is still entitled to see.
///
/// The reader is read-only, so it never allocates an XID and is invisible to the
/// running-transaction floor; the snapshot it registered for the block is the
/// only thing standing between it and the vacuumer. Choosing the wrong floor here
/// deletes the row out from under a `REPEATABLE READ` session mid-transaction.
#[tokio::test]
async fn vacuum_does_not_reclaim_below_a_read_only_repeatable_read_reader() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let reader = connect(port).await;
    let vacuumer = connect(port).await;
    vacuumer.simple_query("CREATE TABLE t (id int4)").await?;
    vacuumer
        .simple_query("INSERT INTO t VALUES (1), (2), (3)")
        .await?;

    // The first read freezes and registers the block's snapshot. The block writes
    // nothing, so it holds no XID from here on.
    reader
        .simple_query("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await?;
    let messages = reader.simple_query("SELECT id FROM t ORDER BY id").await?;
    assert_eq!(
        rows(&messages).iter().map(|r| r.get(0)).collect::<Vec<_>>(),
        vec![Some("1"), Some("2"), Some("3")]
    );

    // A committed delete, then a vacuum, both while the reader sits idle.
    vacuumer.simple_query("DELETE FROM t WHERE id = 2").await?;
    vacuumer.simple_query("VACUUM t").await?;

    let messages = reader.simple_query("SELECT id FROM t ORDER BY id").await?;
    assert_eq!(
        rows(&messages).iter().map(|r| r.get(0)).collect::<Vec<_>>(),
        vec![Some("1"), Some("2"), Some("3")],
        "VACUUM must not reclaim a version the open snapshot can still see"
    );

    // Holding the horizon back only delays reclamation: once the reader is done,
    // the delete is final, and a vacuum with nothing to hold it back must still
    // leave the live rows alone.
    reader.simple_query("COMMIT").await?;
    vacuumer.simple_query("VACUUM t").await?;
    let messages = reader.simple_query("SELECT id FROM t ORDER BY id").await?;
    assert_eq!(
        rows(&messages).iter().map(|r| r.get(0)).collect::<Vec<_>>(),
        vec![Some("1"), Some("3")],
        "the delete is final once the reader is gone, and live rows survive"
    );
    Ok(())
}

/// The same guarantee when the deleter was still IN FLIGHT as the reader froze
/// its snapshot, which is the case that separates a correct reclamation floor
/// from one that only looks correct.
///
/// A reader can still see rows deleted by any XID in its `xip` list, and the
/// smallest of those is its `xmin`. A floor taken from the snapshot's `xmax`
/// instead leaves every `xip` member above the horizon, so the vacuum reclaims
/// precisely the versions the reader is entitled to keep reading. The two agree
/// whenever `xip` is empty, so only a concurrent writer tells them apart.
#[tokio::test]
async fn vacuum_respects_a_reader_that_captured_around_an_in_flight_deleter() -> anyhow::Result<()>
{
    let port = spawn_server().await;
    let reader = connect(port).await;
    let deleter = connect(port).await;
    let vacuumer = connect(port).await;
    deleter.simple_query("CREATE TABLE t (id int4)").await?;
    deleter
        .simple_query("INSERT INTO t VALUES (1), (2), (3)")
        .await?;

    // Open the delete but do NOT commit: its XID is in flight, so it lands in the
    // reader's `xip` and its delete does not apply to the reader.
    deleter.simple_query("BEGIN").await?;
    deleter.simple_query("DELETE FROM t WHERE id = 2").await?;

    reader
        .simple_query("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await?;
    let messages = reader.simple_query("SELECT id FROM t ORDER BY id").await?;
    assert_eq!(
        rows(&messages).iter().map(|r| r.get(0)).collect::<Vec<_>>(),
        vec![Some("1"), Some("2"), Some("3")],
        "an uncommitted delete is invisible to the reader"
    );

    deleter.simple_query("COMMIT").await?;
    vacuumer.simple_query("VACUUM t").await?;

    let messages = reader.simple_query("SELECT id FROM t ORDER BY id").await?;
    assert_eq!(
        rows(&messages).iter().map(|r| r.get(0)).collect::<Vec<_>>(),
        vec![Some("1"), Some("2"), Some("3")],
        "the horizon must stay below a deleter the reader still has in flight"
    );
    Ok(())
}

/// `VACUUM` is the explicit flush hook: it moves a Parquet relation's buffered
/// rows into durable storage without changing what any reader sees, works on a
/// heap table and bare, and refuses the forms it cannot honor.
#[tokio::test]
async fn vacuum_flushes_buffered_rows_without_changing_what_readers_see() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE p (id int4, label text) USING parquet ORDER BY (id)")
        .await?;
    client.simple_query("CREATE TABLE h (id int4)").await?;
    client
        .simple_query("INSERT INTO p VALUES (1, 'one'), (2, 'two'), (3, NULL)")
        .await?;
    client.simple_query("INSERT INTO h VALUES (1)").await?;

    let messages = client
        .simple_query("SELECT id, label FROM p ORDER BY id")
        .await?;
    let before = rows(&messages)
        .iter()
        .map(|r| (r.get(0).map(str::to_string), r.get(1).map(str::to_string)))
        .collect::<Vec<_>>();

    client.simple_query("VACUUM p").await?;

    let messages = client
        .simple_query("SELECT id, label FROM p ORDER BY id")
        .await?;
    let after = rows(&messages)
        .iter()
        .map(|r| (r.get(0).map(str::to_string), r.get(1).map(str::to_string)))
        .collect::<Vec<_>>();
    assert_eq!(
        before, after,
        "moving rows from the buffer into a chunk must be invisible to a reader"
    );

    // A second flush has nothing to move, and further inserts still land and read
    // back alongside the already-flushed rows.
    client.simple_query("VACUUM p").await?;
    client
        .simple_query("INSERT INTO p VALUES (4, 'four')")
        .await?;
    let messages = client.simple_query("SELECT id FROM p ORDER BY id").await?;
    assert_eq!(
        rows(&messages).iter().map(|r| r.get(0)).collect::<Vec<_>>(),
        vec![Some("1"), Some("2"), Some("3"), Some("4")],
        "a chunk and the buffer must read as one relation"
    );

    // What the sort key buys, stated end to end: the flush wrote the rows in key
    // order, so an unordered SELECT — which reads the fragment in storage order —
    // comes back ascending. The last row is still buffered and reads after them,
    // which is also storage order.
    client
        .simple_query("CREATE TABLE s (id int4) USING parquet ORDER BY (id)")
        .await?;
    client
        .simple_query("INSERT INTO s VALUES (5), (1), (4), (2), (3)")
        .await?;
    client.simple_query("VACUUM s").await?;
    client.simple_query("INSERT INTO s VALUES (0)").await?;
    let messages = client.simple_query("SELECT id FROM s").await?;
    assert_eq!(
        rows(&messages).iter().map(|r| r.get(0)).collect::<Vec<_>>(),
        vec![
            Some("1"),
            Some("2"),
            Some("3"),
            Some("4"),
            Some("5"),
            Some("0")
        ],
        "a flushed fragment must hold its rows in sort key order"
    );

    // Heap relations and the bare form are accepted; neither has anything to flush.
    client.simple_query("VACUUM h").await?;
    client.simple_query("VACUUM").await?;

    // A flush is its own transaction, so it cannot run inside a block.
    client.simple_query("BEGIN").await?;
    let in_block = client
        .simple_query("VACUUM p")
        .await
        .expect_err("VACUUM inside a transaction block must fail");
    assert_eq!(
        in_block
            .as_db_error()
            .expect("database error")
            .code()
            .code(),
        "25001"
    );
    client.simple_query("ROLLBACK").await?;

    // PostgreSQL's own option spellings must be understood as options. Parsed as
    // a table name instead, `VACUUM ANALYZE` reports 42P01 on a relation the user
    // never wrote.
    for sql in [
        "VACUUM ANALYZE",
        "VACUUM VERBOSE",
        "VACUUM ANALYZE p",
        "VACUUM (ANALYZE) p",
        "VACUUM (VERBOSE, ANALYZE) p",
    ] {
        client
            .simple_query(sql)
            .await
            .unwrap_or_else(|error| panic!("`{sql}` must be accepted: {error}"));
    }

    // Inside a block, the transaction-block check must win over the unsupported
    // -modifier check, as it does in PostgreSQL.
    client.simple_query("BEGIN").await?;
    let full_in_block = client
        .simple_query("VACUUM FULL p")
        .await
        .expect_err("VACUUM inside a transaction block must fail");
    assert_eq!(
        full_in_block
            .as_db_error()
            .expect("database error")
            .code()
            .code(),
        "25001",
        "a transaction block is rejected before any option is inspected"
    );
    client.simple_query("ROLLBACK").await?;

    // Modifiers that would change what VACUUM does must be stated gaps, not
    // silently downgraded to a plain vacuum.
    let full = client
        .simple_query("VACUUM FULL p")
        .await
        .expect_err("VACUUM FULL must be reported as unsupported");
    assert_eq!(
        full.as_db_error().expect("database error").code().code(),
        "0A000"
    );

    // A partitioned parent holds no rows of its own, matching ANALYZE.
    client
        .simple_query("CREATE TABLE part (id int4) PARTITION BY RANGE (id)")
        .await?;
    let parent = client
        .simple_query("VACUUM part")
        .await
        .expect_err("VACUUM of a partitioned parent must fail");
    assert_eq!(
        parent.as_db_error().expect("database error").code().code(),
        "0A000"
    );
    Ok(())
}

/// `USING buffer` is a first-class access method: a WAL-logged, RAM-resident
/// table that supports the full mutable surface, reflects itself in `pg_am` and
/// `pg_class.relam`, and rejects the forms that contradict "permanent, engine
/// managed".
#[tokio::test]
async fn buffer_tables_are_fully_mutable_and_reflect_their_access_method() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE b (id int4 PRIMARY KEY, label text) USING buffer")
        .await?;
    client
        .simple_query("INSERT INTO b VALUES (1, 'one'), (2, 'two'), (3, NULL)")
        .await?;

    let messages = client
        .simple_query("SELECT id, label FROM b ORDER BY id")
        .await?;
    let result = rows(&messages);
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].get("label"), Some("one"));
    assert_eq!(result[2].get("label"), None);

    // Unlike Parquet, a buffer table is mutable.
    client
        .simple_query("UPDATE b SET label = 'ONE' WHERE id = 1")
        .await?;
    client.simple_query("DELETE FROM b WHERE id = 3").await?;
    let messages = client
        .simple_query("SELECT id, label FROM b ORDER BY id")
        .await?;
    let result = rows(&messages);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].get("label"), Some("ONE"));

    // The PRIMARY KEY is enforced by scanning the visible rows, as on Parquet.
    let duplicate = client
        .simple_query("INSERT INTO b VALUES (2, 'dup')")
        .await
        .expect_err("a duplicate key must be rejected");
    assert_eq!(
        duplicate
            .as_db_error()
            .expect("database error")
            .code()
            .code(),
        "23505"
    );

    // A rollback must undo everything, since the rows are MVCC versions and not
    // an opaque RAM cache.
    client.simple_query("BEGIN").await?;
    client.simple_query("DELETE FROM b").await?;
    assert!(rows(&client.simple_query("SELECT id FROM b").await?).is_empty());
    client.simple_query("ROLLBACK").await?;
    let messages = client.simple_query("SELECT id FROM b").await?;
    assert_eq!(rows(&messages).len(), 2);

    // A rolled-back DELETE leaves `xmax` set on every row; counting those as dead
    // would report the relation permanently empty in pg_class.
    client.simple_query("BEGIN").await?;
    client.simple_query("DELETE FROM b").await?;
    client.simple_query("ROLLBACK").await?;
    client.simple_query("ANALYZE b").await?;
    let messages = client
        .simple_query(
            "SELECT count(*)::int8 AS live, \
             (SELECT reltuples::int8 FROM pg_catalog.pg_class WHERE relname = 'b') AS est FROM b",
        )
        .await?;
    let counted = rows(&messages);
    assert_eq!(
        counted[0].get("est"),
        counted[0].get("live"),
        "reltuples must match the rows a scan returns after an aborted DELETE"
    );

    client.simple_query("TRUNCATE b").await?;
    assert!(rows(&client.simple_query("SELECT id FROM b").await?).is_empty());

    let messages = client
        .simple_query(
            "SELECT c.relam, a.amname FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_am a ON a.oid = c.relam WHERE c.relname = 'b'",
        )
        .await?;
    let reflected = rows(&messages);
    assert_eq!(reflected.len(), 1);
    assert_eq!(reflected[0].get("relam"), Some("16001"));
    assert_eq!(reflected[0].get("amname"), Some("buffer"));

    // A buffer table is WAL-logged and permanent by definition, so the forms that
    // ask for the opposite must be refused rather than quietly downgraded.
    // Each carries a sort key so the form under test is what fails, not the
    // missing-order rule.
    for sql in [
        "CREATE TEMP TABLE temp_b (id int) USING buffer ORDER BY (id)",
        "CREATE UNLOGGED TABLE unlogged_b (id int) USING buffer ORDER BY (id)",
        "CREATE TABLE partitioned_b (id int) USING buffer PARTITION BY RANGE (id) ORDER BY (id)",
        "CREATE TABLE unsupported_b (value jsonb) USING buffer ORDER BY (value)",
    ] {
        let error = client
            .simple_query(sql)
            .await
            .expect_err("unsupported buffer table form must fail");
        assert_eq!(
            error.as_db_error().expect("database error").code().code(),
            "0A000",
            "{sql}"
        );
    }
    Ok(())
}

/// TRUNCATE on a Parquet table is a transactional directory swap: it empties the
/// table, a rollback brings every row back, reloading after it works, and it
/// composes with heap tables in one multi-table statement.
#[tokio::test]
async fn parquet_truncate_is_transactional_and_resets_statistics() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE p (id int4, label text) USING parquet ORDER BY (id)")
        .await?;
    client.simple_query("CREATE TABLE h (id int4)").await?;
    client
        .simple_query("INSERT INTO p VALUES (1, 'one'), (2, 'two')")
        .await?;
    client.simple_query("INSERT INTO h VALUES (1)").await?;

    let count = |messages: &[tokio_postgres::SimpleQueryMessage]| {
        rows(messages)[0].get(0).map(str::to_string)
    };

    // ANALYZE first, so the reset back to never-analyzed is observable.
    client.simple_query("ANALYZE p").await?;
    let analyzed = client
        .simple_query("SELECT reltuples FROM pg_class WHERE relname = 'p'")
        .await?;
    assert_eq!(count(&analyzed).as_deref(), Some("2"));

    // Rolled back: the rows are back, in place and afterwards.
    client.simple_query("BEGIN").await?;
    client.simple_query("TRUNCATE p").await?;
    let inside = client.simple_query("SELECT count(*) FROM p").await?;
    assert_eq!(count(&inside).as_deref(), Some("0"));
    client.simple_query("ROLLBACK").await?;
    let after_rollback = client.simple_query("SELECT count(*) FROM p").await?;
    assert_eq!(count(&after_rollback).as_deref(), Some("2"));

    // Committed, in one statement with a heap table (which also exercises the
    // name-ordered lock acquisition), then reloaded.
    client.simple_query("TRUNCATE p, h").await?;
    let emptied = client.simple_query("SELECT count(*) FROM p").await?;
    assert_eq!(count(&emptied).as_deref(), Some("0"));
    let heap_emptied = client.simple_query("SELECT count(*) FROM h").await?;
    assert_eq!(count(&heap_emptied).as_deref(), Some("0"));

    // PostgreSQL reports a truncated relation as never-analyzed, not as a measured
    // zero: `relpages = 0`, `reltuples = -1`.
    let reset = client
        .simple_query("SELECT relpages, reltuples FROM pg_class WHERE relname = 'p'")
        .await?;
    assert_eq!(
        (rows(&reset)[0].get(0), rows(&reset)[0].get(1)),
        (Some("0"), Some("-1"))
    );

    client
        .simple_query("INSERT INTO p VALUES (3, 'three')")
        .await?;
    let reloaded = client
        .simple_query("SELECT id, label FROM p ORDER BY id")
        .await?;
    assert_eq!(rows(&reloaded).len(), 1);
    assert_eq!(
        (rows(&reloaded)[0].get(0), rows(&reloaded)[0].get(1)),
        (Some("3"), Some("three"))
    );

    // TRUNCATE twice in one transaction, then insert: the rows of the second
    // truncate's directory are the only ones that survive.
    client.simple_query("BEGIN").await?;
    client.simple_query("TRUNCATE p").await?;
    client
        .simple_query("INSERT INTO p VALUES (4, 'four')")
        .await?;
    client.simple_query("TRUNCATE p").await?;
    client
        .simple_query("INSERT INTO p VALUES (5, 'five')")
        .await?;
    client.simple_query("COMMIT").await?;
    let doubled = client.simple_query("SELECT id FROM p ORDER BY id").await?;
    assert_eq!(rows(&doubled).len(), 1);
    assert_eq!(rows(&doubled)[0].get(0), Some("5"));

    // The unsupported spellings stay unsupported.
    for sql in ["TRUNCATE p CASCADE", "TRUNCATE p RESTART IDENTITY"] {
        let error = client
            .simple_query(sql)
            .await
            .expect_err("unsupported TRUNCATE form must fail");
        assert_eq!(
            error.as_db_error().expect("database error").code().code(),
            "0A000",
            "{sql}"
        );
    }
    Ok(())
}

/// CSV format over the extended protocol: quoted fields with `""` doubling and
/// embedded delimiters, an unquoted empty field as NULL, and HEADER skipping.
#[tokio::test]
async fn copy_in_csv_with_header_and_quotes() -> anyhow::Result<()> {
    use bytes::Bytes;
    use futures_util::SinkExt;

    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE c (a int4, b text)")
        .await?;

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
    let err = sink
        .finish()
        .await
        .expect_err("a row whose value is not a valid integer must fail the COPY");
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
    let err = sink
        .finish()
        .await
        .expect_err("a byte sequence that is not valid UTF-8 must fail the COPY");
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
    client
        .simple_query("CREATE TABLE c (a int4, b text)")
        .await?;
    let sink = client
        .copy_in("COPY c FROM STDIN WITH (FORMAT csv)")
        .await?;
    futures_util::pin_mut!(sink);
    // `1, "two"` -> b = ' two' (space + quoted run), as PG concatenates.
    sink.send(Bytes::from_static(b"1, \"two\"\n")).await?;
    assert_eq!(sink.finish().await?, 1);

    let messages = client
        .simple_query("SELECT '['||b||']' AS b FROM c")
        .await?;
    assert_eq!(rows(&messages)[0].get("b"), Some("[ two]"));
    Ok(())
}

#[tokio::test]
async fn copy_multibyte_delimiter_rejected() -> anyhow::Result<()> {
    // A multi-byte DELIMITER is rejected (the parser guards the single-char slot,
    // and the binder's single-byte check backs it up) rather than silently
    // splitting on a multi-byte character, matching PG.
    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE c (a int4, b text)")
        .await?;
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
        .expect_err("COPY in an aborted transaction block must be refused");
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

// ---------------------------------------------------------------------------
// COPY ... FREEZE
// ---------------------------------------------------------------------------

/// Stream `data` into `table` with the given COPY options, returning the row
/// count or the server's error.
async fn copy_in_rows(
    client: &tokio_postgres::Client,
    statement: &str,
    data: &'static [u8],
) -> Result<u64, tokio_postgres::Error> {
    use futures_util::SinkExt;

    let sink = client.copy_in(statement).await?;
    futures_util::pin_mut!(sink);
    sink.send(bytes::Bytes::from_static(data)).await?;
    sink.finish().await
}

/// FREEZE is authorized per relation, so it must reach only that relation's
/// write. A column `DEFAULT` that calls a routine is the path that proves it: the
/// routine's own INSERT targets a table nobody truncated, and freezing *those*
/// rows would leave them visible after a ROLLBACK with no XID whose abort could
/// hide them — permanently committed rows written by a rolled-back transaction.
#[tokio::test]
async fn copy_freeze_does_not_reach_a_routine_s_own_inserts() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let writer = connect(port).await;
    let reader = connect(port).await;
    writer.simple_query("CREATE TABLE audit (msg text)").await?;
    writer
        .simple_query(
            "CREATE FUNCTION note() RETURNS text LANGUAGE plpgsql AS $$ \
             BEGIN INSERT INTO audit VALUES ('row'); RETURN 'x'; END $$",
        )
        .await?;
    writer
        .simple_query("CREATE TABLE t (a text, b text DEFAULT note())")
        .await?;

    writer.simple_query("BEGIN").await?;
    writer.simple_query("TRUNCATE t").await?;
    assert_eq!(
        copy_in_rows(
            &writer,
            "COPY t (a) FROM STDIN WITH (FORMAT csv, FREEZE)",
            b"p\nq\n",
        )
        .await?,
        2
    );
    writer.simple_query("ROLLBACK").await?;

    // `audit` was never truncated, so nothing discarded its storage: the only
    // thing that can hide the routine's rows is their own transaction's abort.
    assert_eq!(row_count(&reader, "audit").await, 0);
    // And the frozen half is gone too, with the staged file it was written into.
    assert_eq!(row_count(&reader, "t").await, 0);
    Ok(())
}

/// What FREEZE actually changes, expressed as something a client can see: a
/// REPEATABLE READ snapshot taken *before* the load still sees the rows.
///
/// The reader's snapshot predates the loading transaction entirely, so ordinary
/// rows would be invisible to it no matter how long it waited — that is the
/// control case below. Frozen rows carry `Xid::FROZEN`, which every snapshot
/// treats as long-committed, so they show up.
///
/// The reader has to run *after* the writer commits rather than concurrently:
/// TRUNCATE holds AccessExclusive until the transaction ends, so a concurrent
/// reader would block on the lock rather than demonstrate anything about
/// visibility.
#[tokio::test]
async fn copy_freeze_rows_are_visible_to_an_older_snapshot() -> anyhow::Result<()> {
    for (options, expected) in [("FREEZE ON", 2), ("FREEZE OFF", 0)] {
        let port = spawn_server().await;
        let writer = connect(port).await;
        let reader = connect(port).await;
        writer.simple_query("CREATE TABLE vistest (a text)").await?;

        // Pin a snapshot that predates everything the writer is about to do.
        reader
            .simple_query("BEGIN ISOLATION LEVEL REPEATABLE READ")
            .await?;
        assert_eq!(row_count(&reader, "vistest").await, 0);

        writer.simple_query("BEGIN").await?;
        writer.simple_query("TRUNCATE vistest").await?;
        let loaded = copy_in_rows(
            &writer,
            &format!("COPY vistest FROM STDIN WITH (FORMAT csv, {options})"),
            b"a2\nb\n",
        )
        .await?;
        assert_eq!(loaded, 2, "{options}");
        writer.simple_query("COMMIT").await?;

        assert_eq!(
            row_count(&reader, "vistest").await,
            expected,
            "old snapshot with {options}"
        );
        reader.simple_query("COMMIT").await?;
        // A fresh snapshot sees them either way.
        assert_eq!(row_count(&reader, "vistest").await, 2, "{options}");
    }
    Ok(())
}

/// Rolling back a frozen load must still lose the rows. Nothing about the rows
/// themselves can hide them — they name no live transaction — so this is really
/// a test that the staged TRUNCATE file is what gets discarded.
#[tokio::test]
async fn copy_freeze_rollback_discards_the_rows() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let writer = connect(port).await;
    let reader = connect(port).await;
    writer.simple_query("CREATE TABLE vistest (a text)").await?;
    writer
        .simple_query("INSERT INTO vistest VALUES ('old')")
        .await?;

    writer.simple_query("BEGIN").await?;
    writer.simple_query("TRUNCATE vistest").await?;
    assert_eq!(
        copy_in_rows(
            &writer,
            "COPY vistest FROM STDIN WITH (FORMAT csv, FREEZE)",
            b"x\ny\n",
        )
        .await?,
        2
    );
    writer.simple_query("ROLLBACK").await?;

    // Both the TRUNCATE and the frozen rows are gone.
    let messages = reader.simple_query("SELECT a FROM vistest").await?;
    let rows = rows(&messages);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("a"), Some("old"));
    Ok(())
}

/// Without a TRUNCATE in the same transaction there is no discardable storage,
/// so PostgreSQL refuses rather than write rows a rollback could not take back.
/// Autocommit is the plain case: the TRUNCATE committed in its own transaction.
#[tokio::test]
async fn copy_freeze_requires_a_truncate_in_the_same_transaction() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE vistest (a text)").await?;
    client.simple_query("TRUNCATE vistest").await?;

    for statement in [
        "COPY vistest FROM STDIN WITH (FORMAT csv, FREEZE)",
        // A block that has not truncated is refused just the same.
        "COPY vistest FROM STDIN CSV FREEZE",
    ] {
        let err = copy_in_rows(&client, statement, b"p\ng\n")
            .await
            .expect_err("FREEZE into a block another transaction can see is refused");
        let db = err.as_db_error().expect("db error");
        assert_eq!(
            db.code(),
            &SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            "{statement}"
        );
        assert_eq!(
            db.message(),
            "cannot perform COPY FREEZE because the table was not created or \
             truncated in the current subtransaction",
            "{statement}"
        );
    }

    // The refusal loaded nothing and left the session usable.
    assert_eq!(row_count(&client, "vistest").await, 0);
    Ok(())
}

/// This engine's DDL is not transactional, so a table created in the current
/// transaction is *not* the discardable storage PostgreSQL's rule assumes — a
/// rollback would leave the relation behind with its frozen rows inside. We
/// refuse, using PostgreSQL's wording; a documented divergence, pinned here so
/// it cannot regress into silent data retention.
#[tokio::test]
async fn copy_freeze_refuses_a_table_created_in_this_transaction() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client.simple_query("BEGIN").await?;
    client.simple_query("CREATE TABLE fresh (a text)").await?;
    let err = copy_in_rows(
        &client,
        "COPY fresh FROM STDIN WITH (FORMAT csv, FREEZE)",
        b"d\ne\n",
    )
    .await
    .expect_err("FREEZE into a table this transaction did not create is refused");
    assert_eq!(
        err.as_db_error().expect("db error").code(),
        &SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE
    );
    Ok(())
}

/// `FREEZE OFF` is an ordinary load: no precondition, no frozen rows.
#[tokio::test]
async fn copy_freeze_off_is_an_ordinary_load() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let writer = connect(port).await;
    let reader = connect(port).await;
    writer.simple_query("CREATE TABLE t (a text)").await?;

    writer.simple_query("BEGIN").await?;
    assert_eq!(
        copy_in_rows(
            &writer,
            "COPY t FROM STDIN WITH (FORMAT csv, FREEZE OFF)",
            b"a\nb\n",
        )
        .await?,
        2
    );
    // Unfrozen, so still invisible elsewhere until the commit.
    assert_eq!(row_count(&reader, "t").await, 0);
    writer.simple_query("COMMIT").await?;
    assert_eq!(row_count(&reader, "t").await, 2);
    Ok(())
}

/// Relations with no storage of their own to discard: a partitioned parent, and
/// a `buffer` table whose rows live in a RAM list no rollback empties.
#[tokio::test]
async fn copy_freeze_rejects_relations_it_cannot_discard() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let port = spawn_server().await;
    // A connection per case on purpose: both errors are raised *before* the
    // server enters copy mode, and a second `copy_in` on a connection that has
    // already taken one of those desyncs the driver. That is a pre-existing
    // protocol defect unrelated to FREEZE — `COPY nosuch FROM STDIN` twice on one
    // connection reproduces it — so it is not smuggled into this test.
    for (ddl, statement, message) in [
        (
            "CREATE TABLE parted (a int4) PARTITION BY RANGE (a)",
            "COPY parted FROM STDIN WITH (FREEZE)",
            "cannot perform COPY FREEZE on a partitioned table",
        ),
        (
            "CREATE TABLE buffered (a int4) USING buffer ORDER BY (a)",
            "COPY buffered FROM STDIN WITH (FREEZE)",
            "cannot perform COPY FREEZE on a buffer table",
        ),
    ] {
        let client = connect(port).await;
        client.simple_query(ddl).await?;
        let err = copy_in_rows(&client, statement, b"1\n")
            .await
            .expect_err("an access method that cannot freeze must refuse the option");
        let db = err.as_db_error().expect("db error");
        assert_eq!(db.code(), &SqlState::FEATURE_NOT_SUPPORTED, "{statement}");
        assert_eq!(db.message(), message, "{statement}");
    }
    Ok(())
}

/// A `parquet` relation routes a frozen load straight to a fragment instead of
/// its RAM write buffer, because the buffer is one flat list that no rollback
/// discards — a frozen row left there would outlive its transaction forever.
///
/// The assertions have to distinguish the two paths, which "rows land, rollback
/// loses them" does not: buffered rows are stamped with the real XID and hidden
/// by its abort record, so an ordinary buffered load passes that pair too. What
/// only a fragment can do is answer an *older* snapshot, since a frozen fragment
/// reports `Xid::FROZEN` while a buffered row reports its writer. Deleting the
/// bypass makes the first assertion below fail.
#[tokio::test]
async fn copy_freeze_into_parquet_writes_a_discardable_fragment() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let writer = connect(port).await;
    let reader = connect(port).await;
    writer
        .simple_query("CREATE TABLE p (a int4) USING parquet ORDER BY (a)")
        .await?;

    // Pin a snapshot that predates the load, as the heap test does.
    reader
        .simple_query("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await?;
    assert_eq!(row_count(&reader, "p").await, 0);

    writer.simple_query("BEGIN").await?;
    writer.simple_query("TRUNCATE p").await?;
    assert_eq!(
        copy_in_rows(&writer, "COPY p FROM STDIN WITH (FREEZE)", b"1\n2\n3\n").await?,
        3
    );
    // The loading transaction sees its own rows before committing: a frozen
    // fragment is on disk and visible, not parked in an accumulator.
    assert_eq!(row_count(&writer, "p").await, 3);
    // The reader is only consulted after COMMIT: TRUNCATE holds AccessExclusive
    // until then, so a concurrent read would block on the lock rather than show
    // anything about visibility.
    writer.simple_query("COMMIT").await?;
    assert_eq!(
        row_count(&reader, "p").await,
        3,
        "a frozen fragment must be visible to a snapshot that predates it"
    );
    reader.simple_query("COMMIT").await?;

    // And a rolled-back frozen load leaves nothing behind — the `.pending`
    // fragment is unlinked, keyed on the writer XID the filename preserves.
    writer.simple_query("BEGIN").await?;
    writer.simple_query("TRUNCATE p").await?;
    assert_eq!(
        copy_in_rows(&writer, "COPY p FROM STDIN WITH (FREEZE)", b"7\n8\n").await?,
        2
    );
    writer.simple_query("ROLLBACK").await?;
    assert_eq!(row_count(&reader, "p").await, 3);
    Ok(())
}

// ---------------------------------------------------------------------------
// COPY ... FROM '<file>' — the server reads the file itself
// ---------------------------------------------------------------------------

/// Write `contents` to a file inside `dir` and hand back its absolute path,
/// which is the only form `COPY … FROM` accepts.
fn fixture_file(dir: &tempfile::TempDir, name: &str, contents: &[u8]) -> anyhow::Result<String> {
    let path = dir.path().join(name);
    std::fs::write(&path, contents)?;
    Ok(path.to_string_lossy().into_owned())
}

/// The upstream fixture pattern: an absolute path in the statement, text format,
/// loaded by the server without a copy-in sub-protocol.
#[tokio::test]
async fn copy_from_file_loads_text_format() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let client = connect(spawn_server_reading(&[dir.path()]).await).await;
    let path = fixture_file(&dir, "t.data", b"1\talice\n2\t\\N\n3\tbob\n")?;

    client
        .simple_query("CREATE TABLE t (a int4, b text)")
        .await?;
    let messages = client
        .simple_query(&format!("COPY t FROM '{path}'"))
        .await?;
    assert!(
        matches!(&messages[0], SimpleQueryMessage::CommandComplete(n) if *n == 3),
        "COPY should report 3 rows"
    );

    let messages = client.simple_query("SELECT a, b FROM t ORDER BY a").await?;
    let rows = rows(&messages);
    assert_eq!(rows[0].get("b"), Some("alice"));
    assert_eq!(rows[1].get("b"), None);
    assert_eq!(rows[2].get("b"), Some("bob"));
    Ok(())
}

/// The `WITH (…)` options resolve the same way for a file as for stdin, and a
/// quoted field spanning a newline survives the chunked read.
#[tokio::test]
async fn copy_from_file_loads_csv_with_options() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let client = connect(spawn_server_reading(&[dir.path()]).await).await;
    let path = fixture_file(&dir, "c.csv", b"a,b\n1,\"line1\nline2\"\n2,\"x\"\"y\"\n")?;

    client
        .simple_query("CREATE TABLE c (a int4, b text)")
        .await?;
    let messages = client
        .simple_query(&format!("COPY c FROM '{path}' WITH (FORMAT csv, HEADER)"))
        .await?;
    assert!(
        matches!(&messages[0], SimpleQueryMessage::CommandComplete(n) if *n == 2),
        "the header line must not be loaded as a row"
    );

    let messages = client.simple_query("SELECT b FROM c ORDER BY a").await?;
    let rows = rows(&messages);
    assert_eq!(rows[0].get("b"), Some("line1\nline2"));
    assert_eq!(rows[1].get("b"), Some("x\"y"));
    Ok(())
}

/// More rows than one insert batch, so the whole file passes through the
/// batching loop rather than a single pass.
#[tokio::test]
async fn copy_from_file_loads_more_rows_than_one_batch() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let client = connect(spawn_server_reading(&[dir.path()]).await).await;
    let data: String = (1..=2500).map(|i| format!("{i}\tv{i}\n")).collect();
    let path = fixture_file(&dir, "big.data", data.as_bytes())?;

    client
        .simple_query("CREATE TABLE b (a int4, v text)")
        .await?;
    let messages = client
        .simple_query(&format!("COPY b FROM '{path}'"))
        .await?;
    assert!(matches!(&messages[0], SimpleQueryMessage::CommandComplete(n) if *n == 2500));

    let messages = client
        .simple_query("SELECT count(*) AS n, max(a) AS m FROM b")
        .await?;
    assert_eq!(rows(&messages)[0].get("n"), Some("2500"));
    assert_eq!(rows(&messages)[0].get("m"), Some("2500"));
    Ok(())
}

/// A UNIQUE index must be enforced across the whole file, not per insert batch.
///
/// Each batch runs at its own command id so it can see the rows the earlier
/// batches wrote; sharing one command id made the duplicate check blind past a
/// batch boundary and silently admitted a key that already existed.
#[tokio::test]
async fn copy_from_file_enforces_unique_across_batches() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let dir = tempfile::tempdir()?;
    let client = connect(spawn_server_reading(&[dir.path()]).await).await;
    // The duplicate of line 1 sits well past the first batch boundary.
    let data: String = (1..=2500)
        .map(|i| {
            if i == 2000 {
                "1\n".to_string()
            } else {
                format!("{i}\n")
            }
        })
        .collect();
    let path = fixture_file(&dir, "dup.data", data.as_bytes())?;

    client
        .simple_query("CREATE TABLE u (a int4 PRIMARY KEY)")
        .await?;
    let err = client
        .simple_query(&format!("COPY u FROM '{path}'"))
        .await
        .expect_err("a duplicate key must abort the COPY");
    assert_eq!(
        err.as_db_error().expect("db error").code(),
        &SqlState::UNIQUE_VIOLATION
    );

    let messages = client.simple_query("SELECT count(*) AS n FROM u").await?;
    assert_eq!(rows(&messages)[0].get("n"), Some("0"));
    Ok(())
}

/// The control for [`copy_from_file_enforces_unique_across_batches`]: a
/// duplicate *within* one batch, and the same data over STDIN, were already
/// rejected — so a future regression localizes to the batch boundary.
#[tokio::test]
async fn copy_unique_violation_within_one_batch_and_over_stdin() -> anyhow::Result<()> {
    use bytes::Bytes;
    use futures_util::SinkExt;
    use tokio_postgres::error::SqlState;

    let dir = tempfile::tempdir()?;
    let client = connect(spawn_server_reading(&[dir.path()]).await).await;
    // Fewer rows than one batch, duplicate in the middle.
    let data: String = (1..=899)
        .map(|i| {
            if i == 500 {
                "1\n".to_string()
            } else {
                format!("{i}\n")
            }
        })
        .collect();
    let path = fixture_file(&dir, "dup_small.data", data.as_bytes())?;

    client
        .simple_query("CREATE TABLE u (a int4 PRIMARY KEY)")
        .await?;
    let err = client
        .simple_query(&format!("COPY u FROM '{path}'"))
        .await
        .expect_err("a duplicate within one batch must abort the COPY");
    assert_eq!(
        err.as_db_error().expect("db error").code(),
        &SqlState::UNIQUE_VIOLATION
    );

    client
        .simple_query("CREATE TABLE s (a int4 PRIMARY KEY)")
        .await?;
    let sink = client.copy_in("COPY s FROM STDIN").await?;
    futures_util::pin_mut!(sink);
    sink.send(Bytes::from(data)).await?;
    let err = sink
        .finish()
        .await
        .expect_err("a duplicate over stdin must abort the COPY");
    assert_eq!(
        err.as_db_error().expect("db error").code(),
        &SqlState::UNIQUE_VIOLATION
    );

    let messages = client
        .simple_query("SELECT (SELECT count(*) FROM u) + (SELECT count(*) FROM s) AS n")
        .await?;
    assert_eq!(rows(&messages)[0].get("n"), Some("0"));
    Ok(())
}

/// A bad row in the file's tail must leave none of its head behind: every batch
/// runs in one transaction, so the whole COPY rolls back.
#[tokio::test]
async fn copy_from_file_bad_value_late_in_file_aborts_whole_load() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let dir = tempfile::tempdir()?;
    let client = connect(spawn_server_reading(&[dir.path()]).await).await;
    // The failing row sits past the first insert batch.
    let mut data: String = (1..=2000).map(|i| format!("{i}\n")).collect();
    data.push_str("not_an_int\n");
    let path = fixture_file(&dir, "bad.data", data.as_bytes())?;

    client.simple_query("CREATE TABLE t (a int4)").await?;
    let err = client
        .simple_query(&format!("COPY t FROM '{path}'"))
        .await
        .expect_err("a malformed value must abort the COPY");
    assert_eq!(
        err.as_db_error().expect("db error").code(),
        &SqlState::INVALID_TEXT_REPRESENTATION
    );

    let messages = client.simple_query("SELECT count(*) AS n FROM t").await?;
    assert_eq!(rows(&messages)[0].get("n"), Some("0"));
    Ok(())
}

/// A file missing from a directory the server *may* read reports PG's 58P01
/// wording, quoting the path as the statement wrote it.
#[tokio::test]
async fn copy_from_missing_file_errors() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let client = connect(spawn_server_reading(&[dir.path()]).await).await;
    client.simple_query("CREATE TABLE t (a int4)").await?;

    let path = dir.path().join("absent.data");
    let path = path.to_string_lossy();
    let err = client
        .simple_query(&format!("COPY t FROM '{path}'"))
        .await
        .expect_err("a missing file must error");
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code().code(), "58P01");
    assert_eq!(
        db.message(),
        format!("could not open file \"{path}\" for reading: No such file or directory")
    );
    Ok(())
}

/// A relative path resolves against the data directory, as it does in PG, where
/// the backend's working directory is PGDATA. It is not an error in itself — it
/// simply names a file that is not there.
#[tokio::test]
async fn copy_from_relative_path_resolves_against_the_data_dir() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (a int4)").await?;

    let err = client
        .simple_query("COPY t FROM 'relative.data'")
        .await
        .expect_err("the file does not exist under the data directory");
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code().code(), "58P01");
    assert_eq!(
        db.message(),
        "could not open file \"relative.data\" for reading: No such file or directory"
    );
    Ok(())
}

/// The server reads with its own privileges, so a path outside the directories
/// it was configured for is refused — and refused identically whether or not the
/// file exists, so the error cannot be used to probe the filesystem.
#[tokio::test]
async fn copy_from_outside_the_allowed_roots_is_denied() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let outside = tempfile::tempdir()?;
    let present = outside.path().join("present.data");
    std::fs::write(&present, b"1\n")?;
    let absent = outside.path().join("absent.data");

    // Note: the server is NOT told about `outside`.
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (a int4)").await?;

    let mut seen = Vec::new();
    for path in [present.to_string_lossy(), absent.to_string_lossy()] {
        let err = client
            .simple_query(&format!("COPY t FROM '{path}'"))
            .await
            .expect_err("a path outside the allowed roots must be refused");
        let db = err.as_db_error().expect("db error");
        assert_eq!(db.code(), &SqlState::INSUFFICIENT_PRIVILEGE);
        seen.push(db.message().to_string());
    }
    assert_eq!(
        seen[0], seen[1],
        "an existing and a missing out-of-bounds file must be indistinguishable"
    );
    assert_eq!(seen[0], "absolute path not allowed");

    // A `..` cannot walk out of the data directory either.
    let err = client
        .simple_query("COPY t FROM '../../etc/passwd'")
        .await
        .expect_err("`..` must not escape the data directory");
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::INSUFFICIENT_PRIVILEGE);
    assert_eq!(
        db.message(),
        "path must be in or below the current directory"
    );
    Ok(())
}

/// PG rejects a directory with the wrong-object-type class before it reads;
/// anything else that is not a regular file we refuse too, because a read that
/// never returns (a FIFO with no writer) cannot be cancelled here.
#[tokio::test]
async fn copy_from_a_directory_or_fifo_errors_rather_than_hanging() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let client = connect(spawn_server_reading(&[dir.path()]).await).await;
    client.simple_query("CREATE TABLE t (a int4)").await?;

    let path = dir.path().to_string_lossy().into_owned();
    let err = client
        .simple_query(&format!("COPY t FROM '{path}'"))
        .await
        .expect_err("a directory is not a COPY source");
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code().code(), "42809");
    assert_eq!(db.message(), format!("\"{path}\" is a directory"));

    #[cfg(unix)]
    {
        // No writer will ever open this, so opening it to read would block for
        // good; the check happens before the open.
        let fifo = dir.path().join("pipe");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status()?;
        assert!(made.success(), "mkfifo failed: {made}");

        let fifo = fifo.to_string_lossy().into_owned();
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.simple_query(&format!("COPY t FROM '{fifo}'")),
        )
        .await
        .expect("COPY from a FIFO must not block the server")
        .expect_err("a FIFO is not a regular file");
        let db = err.as_db_error().expect("db error");
        assert_eq!(db.code().code(), "42809");
        assert_eq!(db.message(), format!("\"{fifo}\" is not a regular file"));
    }
    Ok(())
}

/// A bad path aborts an open block, as any statement error does in PG, and the
/// session recovers on ROLLBACK. (Internally the statement is rejected before it
/// takes a transaction, so it burns no XID — not observable from here, but it is
/// why the resolve/open happen outside `run_copy_rows`.)
#[tokio::test]
async fn copy_from_bad_path_aborts_the_block_and_recovers() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (a int4)").await?;

    client.simple_query("BEGIN").await?;
    client
        .simple_query("COPY t FROM '/etc/passwd'")
        .await
        .expect_err("outside the allowed roots");
    client
        .simple_query("SELECT 1")
        .await
        .expect_err("PG aborts the whole block on any statement error");
    client.simple_query("ROLLBACK").await?;

    let messages = client.simple_query("SELECT 1 AS n").await?;
    assert_eq!(rows(&messages)[0].get("n"), Some("1"));
    Ok(())
}

/// PG's 58P01 for an unopenable COPY source carries a HINT pointing at psql's
/// client-side `\copy`, which is usually what the user actually wanted.
#[tokio::test]
async fn copy_from_missing_file_carries_pgs_hint() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (a int4)").await?;

    let err = client
        .simple_query("COPY t FROM 'absent.data'")
        .await
        .expect_err("a missing file must error");
    let db = err.as_db_error().expect("db error");
    assert_eq!(
        db.hint(),
        Some(
            "COPY FROM instructs the PostgreSQL server process to read a file. \
             You may want a client-side facility such as psql's \\copy."
        )
    );
    Ok(())
}

/// `COPY … FROM PROGRAM` is still unimplemented, and keeps its own 0A000.
#[tokio::test]
async fn copy_from_program_is_unsupported() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client.simple_query("CREATE TABLE t (a int4)").await?;

    let err = client
        .simple_query("COPY t FROM PROGRAM 'echo 1'")
        .await
        .expect_err("PROGRAM is not supported");
    let db = err.as_db_error().expect("db error");
    assert_eq!(db.code(), &SqlState::FEATURE_NOT_SUPPORTED);
    assert_eq!(db.message(), "COPY from a program is not supported yet");
    Ok(())
}

#[tokio::test]
async fn create_function_language_sql_evaluates_and_composes() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    // The `AS $$ SELECT ... $$` body form and a direct call.
    client
        .simple_query(
            "CREATE FUNCTION add(int, int) RETURNS int LANGUAGE SQL AS $$ SELECT $1 + $2 $$",
        )
        .await?;
    let out = client.simple_query("SELECT add(1, 2)").await?;
    assert_eq!(rows(&out)[0].get(0), Some("3"));

    // The extended protocol: the outer statement's `$1`/`$2` are the call
    // arguments, distinct from the body's own (now inlined) parameters.
    let row = client
        .query_one("SELECT add($1, $2)", &[&5i32, &7i32])
        .await?;
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
        .simple_query(
            "CREATE FUNCTION double_inc(int) RETURNS int LANGUAGE SQL AS $$ SELECT inc(inc($1)) $$",
        )
        .await?;
    let out = client
        .simple_query("SELECT double_inc(40), add(inc(1), 5)")
        .await?;
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
        .simple_query(
            "CREATE FUNCTION same(text) RETURNS text LANGUAGE SQL AS $$ SELECT $1 || '!' $$",
        )
        .await?;
    let out = client.simple_query("SELECT same(4), same('hi')").await?;
    assert_eq!(rows(&out)[0].get(0), Some("40"));
    assert_eq!(rows(&out)[0].get(1), Some("hi!"));

    // A function used per row over a table.
    client.simple_query("CREATE TABLE t (a int, b int)").await?;
    client
        .simple_query("INSERT INTO t VALUES (1, 2), (3, 4), (10, 20)")
        .await?;
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
async fn create_function_language_sql_body_resolves_argument_names() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;

    // A declared argument name refers to that argument, exactly as `$n` does.
    client
        .simple_query(
            "CREATE FUNCTION f(value int4, seed int8) RETURNS int8 LANGUAGE SQL \
             AS $$ SELECT value + seed $$",
        )
        .await?;
    let out = client.simple_query("SELECT f(2, 40)").await?;
    assert_eq!(rows(&out)[0].get(0), Some("42"));

    // The routine's own name qualifies its parameters.
    client
        .simple_query(
            "CREATE FUNCTION g(value int4, seed int8) RETURNS int8 LANGUAGE SQL \
             AS $$ SELECT g.value + g.seed $$",
        )
        .await?;
    let out = client.simple_query("SELECT g(2, 40)").await?;
    assert_eq!(rows(&out)[0].get(0), Some("42"));

    // Both spellings may be mixed in one body.
    client
        .simple_query(
            "CREATE FUNCTION h(value int4, seed int8) RETURNS int8 LANGUAGE SQL \
             AS $$ SELECT value + $2 $$",
        )
        .await?;
    let out = client.simple_query("SELECT h(2, 40)").await?;
    assert_eq!(rows(&out)[0].get(0), Some("42"));

    // A quoted argument name keeps its case, and only that spelling matches.
    client
        .simple_query(
            "CREATE FUNCTION q(\"Value\" int4) RETURNS int4 LANGUAGE SQL \
             AS $$ SELECT \"Value\" * 2 $$",
        )
        .await?;
    let out = client.simple_query("SELECT q(21)").await?;
    assert_eq!(rows(&out)[0].get(0), Some("42"));
    let err = client
        .simple_query(
            "CREATE FUNCTION q2(\"Value\" int4) RETURNS int4 LANGUAGE SQL \
             AS $$ SELECT value * 2 $$",
        )
        .await
        .expect_err("a body naming a quoted argument in the wrong case must be rejected");
    let dberr = err.as_db_error().expect("database error");
    assert_eq!(dberr.code(), &SqlState::UNDEFINED_COLUMN);
    assert_eq!(dberr.message(), "column \"value\" does not exist");

    // An argument declared without a name stays reachable only as `$n`.
    client
        .simple_query(
            "CREATE FUNCTION unnamed(int4) RETURNS int4 LANGUAGE SQL AS $$ SELECT $1 + 1 $$",
        )
        .await?;
    let out = client.simple_query("SELECT unnamed(41)").await?;
    assert_eq!(rows(&out)[0].get(0), Some("42"));

    // A name matching no argument is still an undefined column, and a member of
    // the routine's qualifier that is not an argument is a missing FROM entry —
    // both as PG reports them.
    let err = client
        .simple_query("CREATE FUNCTION bad(value int) RETURNS int LANGUAGE SQL AS $$ SELECT zz $$")
        .await
        .expect_err("a body naming no declared argument must be rejected");
    let dberr = err.as_db_error().expect("database error");
    assert_eq!(dberr.code(), &SqlState::UNDEFINED_COLUMN);
    assert_eq!(dberr.message(), "column \"zz\" does not exist");

    let err = client
        .simple_query(
            "CREATE FUNCTION bad2(value int) RETURNS int LANGUAGE SQL AS $$ SELECT bad2.nope $$",
        )
        .await
        .expect_err("a routine-qualified name that is not an argument must be rejected");
    let dberr = err.as_db_error().expect("database error");
    assert_eq!(dberr.code(), &SqlState::UNDEFINED_TABLE);
    assert_eq!(
        dberr.message(),
        "missing FROM-clause entry for table \"bad2\""
    );

    // A body may not refer to two arguments by one name.
    let err = client
        .simple_query(
            "CREATE FUNCTION dupname(a int, a int) RETURNS int LANGUAGE SQL AS $$ SELECT $1 $$",
        )
        .await
        .expect_err("two arguments sharing one name must be rejected");
    let dberr = err.as_db_error().expect("database error");
    assert_eq!(dberr.code(), &SqlState::INVALID_FUNCTION_DEFINITION);
    assert_eq!(dberr.message(), "parameter name \"a\" used more than once");

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
        .expect_err("a $n past the declared argument list must be rejected");
    let dberr = err.as_db_error().expect("database error");
    assert_eq!(dberr.code(), &SqlState::UNDEFINED_PARAMETER);
    assert_eq!(dberr.message(), "there is no parameter $9");

    // A body whose type is not assignable to the declared return type.
    let err = client
        .simple_query("CREATE FUNCTION badret(int) RETURNS bool LANGUAGE SQL AS $$ SELECT $1 $$")
        .await
        .expect_err(
            "a body whose type is not assignable to the declared return type must be rejected",
        );
    let dberr = err.as_db_error().expect("database error");
    assert_eq!(dberr.code(), &SqlState::INVALID_FUNCTION_DEFINITION);
    assert_eq!(
        dberr.message(),
        "return type mismatch in function declared to return boolean"
    );
    assert_eq!(dberr.detail(), Some("Actual return type is integer."));

    // An unknown function referenced in a body is rejected at CREATE time.
    let err = client
        .simple_query(
            "CREATE FUNCTION nested(int) RETURNS int LANGUAGE SQL AS $$ SELECT nope($1) $$",
        )
        .await
        .expect_err("a body calling a function that does not exist must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_FUNCTION
    );

    // Only scalar, FROM-less bodies are supported for now.
    client.simple_query("CREATE TABLE t (a int)").await?;
    let err = client
        .simple_query("CREATE FUNCTION scan() RETURNS int LANGUAGE SQL AS $$ SELECT a FROM t $$")
        .await
        .expect_err("a body reading from a table must be rejected");
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
        .expect_err("a duplicate name and signature must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::DUPLICATE_FUNCTION
    );

    // Calling with a wrong argument count finds no matching overload.
    let err = client
        .simple_query("SELECT dup(1, 2)")
        .await
        .expect_err("a call with an argument count no overload matches must be rejected");
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
        .expect_err("a volatile argument referenced more than once must be rejected");
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
    let err = client
        .simple_query("SELECT g(1::int)")
        .await
        .expect_err("a call matching two overloads equally well must be rejected as ambiguous");
    let dberr = err.as_db_error().expect("database error");
    assert_eq!(dberr.code(), &SqlState::AMBIGUOUS_FUNCTION);
    assert_eq!(dberr.message(), "function g(integer) is not unique");
    // PG puts the whole sentence in the HINT for an ambiguous *function*, where
    // the operator form splits it across DETAIL and HINT.
    assert_eq!(
        dberr.hint(),
        Some(
            "Could not choose a best candidate function. You might need to add explicit type casts."
        )
    );

    // An aggregate body is a scalar-inlining limitation, reported as unsupported
    // (PostgreSQL accepts `SELECT sum(1)`), not a grouping error.
    let err = client
        .simple_query("CREATE FUNCTION agg() RETURNS bigint LANGUAGE SQL AS $$ SELECT sum(1) $$")
        .await
        .expect_err("an aggregate in a SQL function body must be rejected");
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
        .simple_query("SELECT relkind, relispartition FROM pg_class WHERE relname = 'm'")
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
    let msgs = client
        .simple_query("SELECT id FROM m_2024 ORDER BY id")
        .await?;
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
        .expect_err("a key admitted by no partition must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::CHECK_VIOLATION
    );
    assert_eq!(
        rows(&client.simple_query("SELECT count(*) FROM m").await?)[0].get(0),
        Some("2")
    );

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
        .expect_err("LIST partitioning must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::FEATURE_NOT_SUPPORTED
    );
    let err = client
        .simple_query(
            "CREATE TABLE m_ov PARTITION OF m FOR VALUES FROM ('2024-06-01') TO ('2024-07-01')",
        )
        .await
        .expect_err("a partition bound overlapping an existing partition must be rejected");
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
        .expect_err("PARTITION OF a table that is not partitioned must be rejected");
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
        .expect_err("a NULL partition bound must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::from_code("42P17")
    );
    assert_eq!(
        rows(&client.simple_query("SELECT 1").await?)[0].get(0),
        Some("1")
    );

    // A non-orderable RANGE key (json) is rejected at parent create (42704), not a crash.
    let err = client
        .simple_query("CREATE TABLE jm (j json) PARTITION BY RANGE (j)")
        .await
        .expect_err("a RANGE key of a non-orderable type must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_OBJECT
    );
    assert_eq!(
        rows(&client.simple_query("SELECT 2").await?)[0].get(0),
        Some("2")
    );

    // A duplicate partition name is 'relation already exists' (42P07), not a self-overlap;
    // IF NOT EXISTS is a no-op.
    client
        .simple_query(
            "CREATE TABLE m_2024 PARTITION OF m FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')",
        )
        .await?;
    let err = client
        .simple_query(
            "CREATE TABLE m_2024 PARTITION OF m FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')",
        )
        .await
        .expect_err("a partition name already in use must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::DUPLICATE_TABLE
    );
    client
        .simple_query("CREATE TABLE IF NOT EXISTS m_2024 PARTITION OF m FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')")
        .await?;

    // TRUNCATE / CREATE INDEX on the parent are rejected (0A000), not applied to
    // the empty parent relation.
    let err = client
        .simple_query("TRUNCATE m")
        .await
        .expect_err("TRUNCATE on the partitioned parent must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::FEATURE_NOT_SUPPORTED
    );
    let err = client
        .simple_query("CREATE INDEX m_idx ON m (d)")
        .await
        .expect_err("CREATE INDEX on the partitioned parent must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::FEATURE_NOT_SUPPORTED
    );

    // PARTITION OF a view reports wrong-object-type (42809), not 'does not exist'.
    client
        .simple_query("CREATE VIEW vv AS SELECT 1 AS x")
        .await?;
    let err = client
        .simple_query("CREATE TABLE cv PARTITION OF vv FOR VALUES FROM (1) TO (2)")
        .await
        .expect_err("PARTITION OF a view must be rejected");
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
        .expect_err("a key below the leaf's range must be rejected");
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
        .expect_err("a key equal to the exclusive upper bound must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::CHECK_VIOLATION
    );

    // A NULL partition key has no place in any range partition (23514).
    let err = client
        .simple_query("INSERT INTO m_2024 VALUES (5, NULL)")
        .await
        .expect_err("a NULL partition key must be rejected");
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
        .expect_err("an UPDATE moving the key out of the leaf's range must be rejected");
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
        .expect_err("a key above an open-ended leaf's upper bound must be rejected");
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
        .expect_err("a key admitted by no partition must be rejected");
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
        .expect_err("a duplicate key in the destination leaf must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::UNIQUE_VIOLATION
    );

    // Two rows in one statement that route to the same leaf with a duplicate key
    // are caught against each other, not just against pre-existing rows.
    let err = client
        .simple_query("INSERT INTO m VALUES (2, '2023-02-01', 1), (2, '2023-05-01', 2)")
        .await
        .expect_err(
            "two rows of one INSERT routing to the same leaf with one key must be rejected",
        );
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
        .expect_err(
            "the earlier row's not-null violation must be reported, not the later routing failure",
        );
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
    let err = sink
        .finish()
        .await
        .expect_err("a copied row admitted by no partition must fail the COPY");
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
        rows(
            &client
                .simple_query("SELECT d FROM m_2023 WHERE id = 1")
                .await?
        )[0]
        .get(0),
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
    let msgs = client
        .simple_query("SELECT id FROM m_2024 ORDER BY id")
        .await?;
    let moved: Vec<_> = rows(&msgs).iter().map(|r| r.get(0)).collect();
    assert_eq!(moved, vec![Some("1"), Some("2")]);

    // A key-changing UPDATE that fits no partition fails (23514) and nothing moves.
    let err = client
        .simple_query("UPDATE m SET d = '2019-05-01' WHERE id = 2")
        .await
        .expect_err("a key-changing UPDATE that fits no partition must be rejected");
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
        .expect_err("a moved row colliding in the destination leaf must be rejected");
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
    let msgs = client
        .simple_query("SELECT id FROM m_2024 ORDER BY id")
        .await?;
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
    assert_eq!(
        pairs,
        vec![
            ("1".to_string(), "1".to_string()),
            ("2".to_string(), "2".to_string())
        ]
    );
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
    client
        .simple_query("CREATE TABLE regwire (a integer)")
        .await?;

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
        .simple_query("SELECT 'regwire'::regclass::oid = 'REGWIRE'::regclass::oid AS eq")
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
    client
        .simple_query("INSERT INTO a1 VALUES (1), (2)")
        .await?;
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

    let e = client
        .query_one("SELECT boom(7)", &[])
        .await
        .expect_err("the body's RAISE must surface as an error");
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
        .expect_err("unbounded recursion must be stopped rather than exhaust the stack");
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
            "CREATE FUNCTION bad() RETURNS int LANGUAGE plpgsql AS $$\nBEGIN\n  x := 1;\nEND $$",
        )
        .await
        .expect_err("a body assigning to an undeclared variable must be rejected");
    let db = e.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::SYNTAX_ERROR);
    assert_eq!(db.message(), "\"x\" is not a known variable");
    // PostgreSQL also prints a `LINE n:` excerpt with a caret into the body,
    // which needs a mapping from a body offset back into the statement text;
    // the CONTEXT line names the line instead.
    assert_eq!(
        db.where_(),
        Some("compilation of PL/pgSQL function \"bad\" near line 3")
    );

    // A RAISE whose format string and argument list disagree is caught when
    // the routine is defined, not when the RAISE is reached.
    let e = client
        .batch_execute(
            "CREATE FUNCTION few() RETURNS int LANGUAGE plpgsql AS $$\n\
             BEGIN RAISE NOTICE 'a % b %', 1; RETURN 1; END $$",
        )
        .await
        .expect_err("a RAISE with fewer arguments than format placeholders must be rejected");
    let db = e.as_db_error().expect("database error");
    // PostgreSQL reports this from `check_raise_parameters` as a syntax error.
    assert_eq!(db.code().code(), "42601");
    assert_eq!(db.message(), "too few parameters specified for RAISE");

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
        .expect_err("DO in a language that cannot run inline code must be rejected");
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::FEATURE_NOT_SUPPORTED
    );
    let e = client
        .batch_execute("DO LANGUAGE nope $$ BEGIN END $$")
        .await
        .expect_err("DO in a language that does not exist must be rejected");
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
    let e = client
        .query_one("SELECT add_n(1)", &[])
        .await
        .expect_err("a procedure called as a function must be rejected");
    let db = e.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::WRONG_OBJECT_TYPE);
    assert_eq!(db.message(), "add_n(integer) is a procedure");
    assert_eq!(db.hint(), Some("To call a procedure, use CALL."));

    // ...nor a function callable with CALL.
    client
        // A LANGUAGE SQL body refers to its arguments only as `$n`.
        .batch_execute("CREATE FUNCTION fn_n(v int) RETURNS int LANGUAGE sql AS 'SELECT $1'")
        .await?;
    let e = client
        .batch_execute("CALL fn_n(1)")
        .await
        .expect_err("a function invoked with CALL must be rejected");
    let db = e.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::WRONG_OBJECT_TYPE);
    assert_eq!(db.hint(), Some("To call a function, use SELECT."));

    // DROP refuses to cross the two kinds, then drops the right one.
    let e = client
        .batch_execute("DROP FUNCTION add_n(int)")
        .await
        .expect_err("DROP FUNCTION naming a procedure must be rejected");
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::WRONG_OBJECT_TYPE
    );
    client.batch_execute("DROP PROCEDURE add_n(int)").await?;
    let e = client
        .batch_execute("CALL add_n(1)")
        .await
        .expect_err("calling the dropped procedure must be rejected");
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_FUNCTION
    );

    Ok(())
}

/// `pg_language` and `pg_proc`: a routine is visible to introspection, with the
/// metadata its declaration actually gave.
#[tokio::test]
async fn routines_are_visible_in_pg_proc() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .batch_execute(
            "CREATE FUNCTION shown(a int, b text) RETURNS bigint \
             LANGUAGE plpgsql IMMUTABLE STRICT AS $$ BEGIN RETURN 1; END $$",
        )
        .await?;

    let row = client
        .query_one(
            "SELECT p.proname, l.lanname, p.prokind, p.provolatile, p.proisstrict, \
                    p.pronargs, p.proargtypes, p.proargtypes::text AS argtypes_text, \
                    (p.proargtypes)[0] AS argtype0, p.prosrc, n.nspname, \
                    array_to_string(p.proargnames, ',') AS argnames \
             FROM pg_catalog.pg_proc p \
             JOIN pg_catalog.pg_language l ON l.oid = p.prolang \
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             WHERE p.proname = 'shown'",
            &[],
        )
        .await?;
    assert_eq!(row.get::<_, &str>("proname"), "shown");
    assert_eq!(row.get::<_, &str>("lanname"), "plpgsql");
    // `prokind`/`provolatile` are `"char"`, so the client decodes a single byte
    // rather than text — the same thing `tokio-postgres` does against PG.
    assert_eq!(row.get::<_, i8>("prokind"), b'f' as i8);
    // The attributes CREATE FUNCTION used to parse and silently drop.
    assert_eq!(row.get::<_, i8>("provolatile"), b'i' as i8);
    assert!(row.get::<_, bool>("proisstrict"));
    assert_eq!(row.get::<_, i16>("pronargs"), 2);
    // `proargtypes` is a real `oidvector`, not text: it must advertise PG's
    // type OID 30, render as `oidvectorout` does (the OIDs, space-separated),
    // and subscript 0-based.
    assert_eq!(row.columns()[6].type_().oid(), 30);
    assert_eq!(row.get::<_, &str>("argtypes_text"), "23 25");
    // Decoded from the *binary* payload the server sent, not from the `::text`
    // rendering above — `tokio-postgres` requests binary for every column, so
    // this is the only assertion that exercises `Value::encode_binary` for a
    // vector end to end. See `OidVectorBinary`.
    assert_eq!(row.get::<_, OidVectorBinary>("proargtypes").0, vec![23, 25]);
    assert_eq!(row.get::<_, u32>("argtype0"), 23);
    // Read through array_to_string: arrays have no binary wire format yet.
    assert_eq!(row.get::<_, &str>("argnames"), "a,b");
    assert!(row.get::<_, &str>("prosrc").contains("RETURN 1;"));
    assert_eq!(row.get::<_, &str>("nspname"), "public");

    // A procedure is prokind 'p'.
    client
        .batch_execute("CREATE PROCEDURE proc_shown() LANGUAGE plpgsql AS $$ BEGIN NULL; END $$")
        .await?;
    assert_eq!(
        client
            .query_one(
                "SELECT prokind FROM pg_catalog.pg_proc WHERE proname = 'proc_shown'",
                &[]
            )
            .await?
            .get::<_, i8>(0),
        b'p' as i8
    );

    // The four languages this build knows, and only those.
    let rows = client
        .query(
            "SELECT lanname FROM pg_catalog.pg_language ORDER BY oid",
            &[],
        )
        .await?;
    let names: Vec<&str> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(names, ["internal", "c", "sql", "plpgsql"]);

    Ok(())
}

/// A READ ONLY transaction rejects DML no matter what its expressions call.
/// The routine itself is only *treated* as a write so it has an XID to stamp
/// with — that exemption must not leak onto the outer statement, which writes
/// whatever the routine does.
#[tokio::test]
async fn read_only_rejects_dml_that_calls_a_routine() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;
    client.batch_execute("CREATE TABLE t (n int)").await?;
    client
        .batch_execute(
            "CREATE FUNCTION pure(n int) RETURNS int LANGUAGE plpgsql AS $$
             BEGIN RETURN n; END $$",
        )
        .await?;

    for sql in [
        "INSERT INTO t VALUES (pure(1))",
        "UPDATE t SET n = pure(1)",
        "DELETE FROM t WHERE n = pure(1)",
    ] {
        client.batch_execute("BEGIN READ ONLY").await?;
        let e = client
            .batch_execute(sql)
            .await
            .expect_err("a write in a read-only transaction must be refused");
        assert_eq!(
            e.as_db_error().expect("database error").code(),
            &SqlState::READ_ONLY_SQL_TRANSACTION,
            "{sql}"
        );
        client.batch_execute("ROLLBACK").await?;
    }

    // A bare SELECT of the same routine is still allowed: nothing writes.
    client.batch_execute("BEGIN READ ONLY").await?;
    assert_eq!(
        client
            .query_one("SELECT pure(7)", &[])
            .await?
            .get::<_, i32>(0),
        7
    );
    client.batch_execute("ROLLBACK").await?;

    assert_eq!(
        client
            .query_one("SELECT count(*) FROM t", &[])
            .await?
            .get::<_, i64>(0),
        0
    );
    Ok(())
}

/// A routine that raises mid-drain must abort its statement's transaction. If
/// the XID leaks in-flight it pins the snapshot horizon and the rows it touched
/// can never be modified again.
#[tokio::test]
async fn routine_error_while_draining_rolls_its_writes_back() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.batch_execute("CREATE TABLE t (n int)").await?;
    client
        .batch_execute(
            "CREATE FUNCTION boom() RETURNS int LANGUAGE plpgsql AS $$
             BEGIN INSERT INTO t VALUES (1); RAISE EXCEPTION 'boom'; END $$",
        )
        .await?;

    let e = client
        .query_one("SELECT boom()", &[])
        .await
        .expect_err("the body's RAISE EXCEPTION must surface as an error");
    assert_eq!(e.as_db_error().expect("database error").message(), "boom");

    // The body's insert is rolled back, and the connection is still usable —
    // an XID left in flight would also block this row from being written.
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM t", &[])
            .await?
            .get::<_, i64>(0),
        0
    );
    client.batch_execute("INSERT INTO t VALUES (2)").await?;
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM t", &[])
            .await?
            .get::<_, i64>(0),
        1
    );
    Ok(())
}

/// `CALL p(f(1))` — a routine call inside a CALL argument needs the same
/// runtime the body gets, or it fails with an internal error.
#[tokio::test]
async fn call_arguments_may_themselves_call_routines() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.batch_execute("CREATE TABLE t (n int)").await?;
    client
        .batch_execute(
            "CREATE FUNCTION inc(n int) RETURNS int LANGUAGE plpgsql AS $$
             BEGIN RETURN n + 1; END $$",
        )
        .await?;
    client
        .batch_execute(
            "CREATE PROCEDURE keep(v int) LANGUAGE plpgsql AS $$
             BEGIN INSERT INTO t VALUES (v); END $$",
        )
        .await?;

    client.batch_execute("CALL keep(inc(1))").await?;
    assert_eq!(
        client
            .query_one("SELECT n FROM t", &[])
            .await?
            .get::<_, i32>(0),
        2
    );
    Ok(())
}

/// A `FETCH` driven over the extended protocol. `Describe` has to answer with
/// the cursor's column shape rather than `NoData` — a client that is told there
/// are no columns and then handed DataRows treats it as a protocol violation,
/// which is exactly what `tokio_postgres::query` does here.
#[tokio::test]
async fn fetch_reports_its_columns_over_the_extended_protocol() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .batch_execute("CREATE TABLE t (id integer, label text)")
        .await?;
    client
        .batch_execute("INSERT INTO t VALUES (1, 'one'), (2, 'two'), (3, 'three')")
        .await?;
    // The cursor has to outlive the statement that declares it, and each
    // extended-protocol statement here is its own implicit transaction — so it
    // must be holdable.
    client
        .batch_execute("DECLARE c SCROLL CURSOR WITH HOLD FOR SELECT id, label FROM t ORDER BY id")
        .await?;

    let first = client.query("FETCH 2 FROM c", &[]).await?;
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].get::<_, i32>("id"), 1);
    assert_eq!(first[1].get::<_, &str>("label"), "two");

    // The cursor keeps its position across statements.
    let next = client.query("FETCH BACKWARD ALL FROM c", &[]).await?;
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].get::<_, i32>("id"), 1);

    // An exhausted fetch still describes its columns, so a zero-row result is a
    // result and not an error.
    client.batch_execute("MOVE ALL c").await?;
    assert!(client.query("FETCH ALL c", &[]).await?.is_empty());

    client.batch_execute("CLOSE c").await?;
    let err = client.query("FETCH 1 c", &[]).await.expect_err("closed");
    assert_eq!(err.code().map(|c| c.code()), Some("34000"));
    Ok(())
}

/// A `FETCH` describes the cursor it names *now*, not the one that existed when
/// it was parsed. A prepared FETCH may be parsed before its cursor is declared,
/// and it has to start reporting the right shape once it is.
///
/// Only the shape resolved at Describe time is a server-side guarantee: a client
/// that caches the description it got from Parse keeps decoding against that
/// one, and PostgreSQL behaves the same way (its statement-level Describe of a
/// FETCH also reads the live cursor). The smoke suite covers the redeclared-with-
/// different-columns case, where psql re-describes each time.
#[tokio::test]
async fn prepared_fetch_follows_the_cursor_it_names() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.batch_execute("CREATE TABLE t (id integer)").await?;
    client
        .batch_execute("INSERT INTO t VALUES (1), (2)")
        .await?;

    // Parsed before the cursor exists: the 34000 belongs to Execute, not Parse,
    // so the statement itself must prepare cleanly.
    let err = client
        .query("FETCH ALL FROM c", &[])
        .await
        .expect_err("no cursor yet");
    assert_eq!(err.code().map(|c| c.code()), Some("34000"));

    // Declaring it afterwards makes the same text work, with its columns.
    client
        .batch_execute("DECLARE c CURSOR WITH HOLD FOR SELECT id FROM t ORDER BY id")
        .await?;
    let rows = client.query("FETCH ALL FROM c", &[]).await?;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<_, i32>("id"), 1);
    Ok(())
}

/// A cursor body may carry `$n`, and the value bound to the `DECLARE` has to
/// reach it — the cursor opens over the rows the client asked for.
#[tokio::test]
async fn declare_cursor_binds_its_parameters() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.batch_execute("CREATE TABLE t (g integer)").await?;
    client
        .batch_execute("INSERT INTO t VALUES (1), (2), (3), (4), (5)")
        .await?;
    client
        .execute(
            "DECLARE c CURSOR WITH HOLD FOR SELECT g FROM t WHERE g > $1 ORDER BY g",
            &[&3i32],
        )
        .await?;
    let rows = client.query("FETCH ALL c", &[]).await?;
    assert_eq!(
        rows.iter()
            .map(|r| r.get::<_, i32>("g"))
            .collect::<Vec<_>>(),
        [4, 5]
    );
    Ok(())
}

/// Cursor names fold like every other unquoted identifier, so the case a client
/// happens to type never changes which cursor it means.
#[tokio::test]
async fn cursor_names_fold_to_lowercase() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .batch_execute("DECLARE Foo CURSOR WITH HOLD FOR SELECT 1 AS n")
        .await?;
    assert_eq!(
        client
            .query_one("SELECT name FROM pg_cursors", &[])
            .await?
            .get::<_, &str>("name"),
        "foo"
    );
    // The same cursor under any spelling, and a second DECLARE is a duplicate.
    assert_eq!(
        client.query("FETCH 1 FOO", &[]).await?[0].get::<_, i32>("n"),
        1
    );
    let err = client
        .batch_execute("DECLARE fOo CURSOR WITH HOLD FOR SELECT 2")
        .await
        .expect_err("duplicate");
    assert_eq!(err.code().map(|c| c.code()), Some("42P03"));
    // A quoted name is a different cursor, as in PG.
    client
        .batch_execute("DECLARE \"Foo\" CURSOR WITH HOLD FOR SELECT 2 AS n")
        .await?;
    client.batch_execute("CLOSE \"Foo\"").await?;
    client.batch_execute("CLOSE fOO").await?;
    assert_eq!(
        client
            .query_one("SELECT count(*) AS n FROM pg_cursors", &[])
            .await?
            .get::<_, i64>("n"),
        0
    );
    Ok(())
}

#[tokio::test]
async fn a_large_value_round_trips_in_binary_over_the_extended_protocol() {
    // The smoke suite drives the simple protocol in text; this covers the other
    // half — a wide value both bound as a parameter and returned in binary.
    let port = spawn_server().await;
    let client = connect(port).await;
    client
        .simple_query("CREATE TABLE toast_wire (id int, b bytea, t text)")
        .await
        .expect("create");

    let blob: Vec<u8> = (0..1_000_000).map(|i| (i % 251) as u8).collect();
    let text: String = "abcdefghij".repeat(20_000);
    client
        .execute(
            "INSERT INTO toast_wire VALUES ($1, $2, $3)",
            &[&1i32, &blob, &text],
        )
        .await
        .expect("insert a value far larger than a page through Bind");

    let row = client
        .query_one("SELECT b, t FROM toast_wire WHERE id = $1", &[&1i32])
        .await
        .expect("select");
    let got_blob: Vec<u8> = row.get(0);
    let got_text: String = row.get(1);
    assert_eq!(got_blob, blob, "the bytea must come back byte for byte");
    assert_eq!(got_text, text);

    // And through RETURNING, which builds its rows on a different path.
    let returned = client
        .query_one(
            "INSERT INTO toast_wire VALUES ($1, $2, $3) RETURNING t",
            &[&2i32, &blob, &text],
        )
        .await
        .expect("insert returning");
    assert_eq!(returned.get::<_, String>(0), text);
}

#[tokio::test]
async fn a_row_that_cannot_be_shrunk_reports_program_limit_exceeded() {
    // Every column is fixed-width, so no amount of out-of-line storage helps.
    // PostgreSQL raises 54000 here; the point of the test is that the session
    // stays usable afterwards rather than the connection dying.
    let port = spawn_server().await;
    let client = connect(port).await;
    let columns: Vec<String> = (0..140).map(|i| format!("c{i} name")).collect();
    client
        .simple_query(&format!(
            "CREATE TABLE toast_fixed ({})",
            columns.join(", ")
        ))
        .await
        .expect("create");

    let values: Vec<String> = (0..140).map(|_| "repeat('x', 63)".to_string()).collect();
    let err = client
        .simple_query(&format!(
            "INSERT INTO toast_fixed SELECT {}",
            values.join(", ")
        ))
        .await
        .expect_err("a row of fixed-width columns cannot be made to fit");
    let db_error = err.as_db_error().expect("database error");
    assert_eq!(
        db_error.code(),
        &tokio_postgres::error::SqlState::PROGRAM_LIMIT_EXCEEDED
    );
    assert!(
        db_error.message().starts_with("row is too big: size "),
        "unexpected message: {}",
        db_error.message()
    );
    assert!(
        db_error.message().ends_with(", maximum size 8160"),
        "unexpected message: {}",
        db_error.message()
    );

    // The connection survives: the failure is an ordinary SQL error, not a
    // panic that unwinds the connection task.
    let row = client
        .query_one("SELECT count(*) FROM toast_fixed", &[])
        .await
        .expect("the session must still be usable");
    assert_eq!(row.get::<_, i64>(0), 0);
}

// --- session TimeZone GUC -------------------------------------------------
//
// Every expected value below is pinned against PostgreSQL 18.4.

/// The single value of a one-row, one-column simple query.
async fn scalar(client: &tokio_postgres::Client, sql: &str) -> String {
    client
        .simple_query(sql)
        .await
        .unwrap_or_else(|e| panic!("`{sql}` should succeed: {e}"))
        .iter()
        .find_map(|m| match m {
            SimpleQueryMessage::Row(row) => row.get(0).map(str::to_string),
            _ => None,
        })
        .unwrap_or_else(|| panic!("`{sql}` should return a row"))
}

#[tokio::test]
async fn timezone_guc_drives_timestamptz_input_and_output() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    // The boot value is UTC (PG's is the host zone; ours keeps test output
    // stable), and `SHOW TIME ZONE` is the same parameter under a two-word
    // spelling.
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "UTC");
    assert_eq!(scalar(&client, "SHOW TIME ZONE").await, "UTC");
    assert_eq!(scalar(&client, "SHOW timezone").await, "UTC");

    client
        .simple_query("SET TIME ZONE 'America/New_York'")
        .await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "America/New_York");

    // A zone-less literal is read in the session zone; one carrying a zone
    // token keeps its instant and is merely rendered in the session zone.
    assert_eq!(
        scalar(&client, "SELECT '2024-06-01 12:00:00'::timestamptz").await,
        "2024-06-01 12:00:00-04"
    );
    assert_eq!(
        scalar(&client, "SELECT '2024-01-15 12:00:00'::timestamptz").await,
        "2024-01-15 12:00:00-05"
    );
    assert_eq!(
        scalar(&client, "SELECT '2024-06-01 12:00:00+00'::timestamptz").await,
        "2024-06-01 08:00:00-04"
    );

    // The other spelling reaches the same parameter, and a sub-hour zone
    // widens the printed offset.
    client
        .simple_query("SET timezone TO 'Asia/Kolkata'")
        .await?;
    assert_eq!(
        scalar(&client, "SELECT '2024-06-01 12:00:00'::timestamptz").await,
        "2024-06-01 12:00:00+05:30"
    );

    client.simple_query("RESET TimeZone").await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "UTC");
    assert_eq!(
        scalar(&client, "SELECT '2024-06-01 12:00:00'::timestamptz").await,
        "2024-06-01 12:00:00+00"
    );
    Ok(())
}

/// The conversions that were an identity only because the zone was UTC.
#[tokio::test]
async fn timezone_guc_drives_casts_and_field_functions() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("SET TIME ZONE 'America/New_York'")
        .await?;

    let cases = [
        // A wall clock read in the session zone, and an instant shown as one.
        (
            "SELECT '2024-06-01 12:00:00'::timestamp::timestamptz",
            "2024-06-01 12:00:00-04",
        ),
        (
            "SELECT '2024-06-01 12:00:00+00'::timestamptz::timestamp",
            "2024-06-01 08:00:00",
        ),
        // A date widens to *local* midnight, and back to the local date.
        (
            "SELECT '2024-06-01'::date::timestamptz",
            "2024-06-01 00:00:00-04",
        ),
        (
            "SELECT '2024-06-01 02:00:00+00'::timestamptz::date",
            "2024-05-31",
        ),
        (
            "SELECT make_timestamptz(2024, 6, 1, 12, 0, 0)",
            "2024-06-01 12:00:00-04",
        ),
        // `date_trunc` re-resolves the offset from the truncated wall clock, so
        // it lands on real local midnight on a spring-forward day.
        (
            "SELECT date_trunc('day', '2024-03-10 15:00:00-04'::timestamptz)",
            "2024-03-10 00:00:00-05",
        ),
        // Offset fields report the session zone rather than a constant 0.
        (
            "SELECT extract(timezone from '2024-01-15 12:00:00-05'::timestamptz)",
            "-18000",
        ),
        (
            "SELECT extract(timezone_hour from '2024-01-15 12:00:00-05'::timestamptz)",
            "-5",
        ),
        // Ordinary fields read the local clock — 02:00 UTC on the 1st is still
        // the 31st in New York — but `epoch` names the instant, so it does not.
        (
            "SELECT extract(day from '2024-01-01 02:00:00+00'::timestamptz)",
            "31",
        ),
        (
            "SELECT extract(epoch from '2024-01-01 02:00:00+00'::timestamptz)",
            "1704074400.000000",
        ),
        // `to_char`'s TZ/OF report the zone, not a hardcoded UTC.
        (
            "SELECT to_char('2024-01-15 12:00:00-05'::timestamptz, 'HH24:MI TZ OF')",
            "12:00 EST -05",
        ),
        // TZH/TZM split that same offset — the sign on the hours, the bare
        // magnitude on the minutes.
        (
            "SELECT to_char('2024-01-15 12:00:00-05'::timestamptz, 'TZH:TZM')",
            "-05:00",
        ),
        // The time-of-day casts rotate into the session zone first, and
        // `timetz` keeps the offset in effect at that instant.
        (
            "SELECT '2024-06-15 03:30:45.5+00'::timestamptz::time",
            "23:30:45.5",
        ),
        (
            "SELECT '2024-06-15 03:30:45.5+00'::timestamptz::timetz",
            "23:30:45.5-04",
        ),
        (
            "SELECT '2024-06-15 03:30:45.5'::timestamp::time",
            "03:30:45.5",
        ),
    ];
    for (sql, want) in cases {
        assert_eq!(scalar(&client, sql).await, want, "for `{sql}`");
    }

    // A `timetz` with no zone of its own takes the offset the session zone is
    // at *today*, as PG's `time_timetz` does — so New York gives `-04` in a
    // summer transaction and `-05` in a winter one. That makes a literal
    // expectation a test that fails every March, so these assert the two
    // properties that hold year-round instead: the offset is the one `now()`
    // reports, and every route to a zone-less `timetz` agrees on it. (The
    // second matters on its own: when one route disagreed, the same value
    // compared unequal to itself.)
    let relative = [
        "SELECT date_part('timezone', '03:30'::timetz) = date_part('timezone', now())",
        "SELECT '03:30:45.5'::timetz = '03:30:45.5'::time::timetz",
        "SELECT '03:30:45.5'::text::timetz = '03:30:45.5'::timetz",
        "SELECT ('{03:30}'::time[]::timetz[])[1] = '03:30'::timetz",
        "SELECT (ARRAY['03:30'::time]::timetz[])[1] = '03:30'::timetz",
    ];
    for sql in relative {
        assert_eq!(scalar(&client, sql).await, "t", "for `{sql}`");
    }
    Ok(())
}

/// The three-argument `date_trunc` truncates in the zone it is handed, leaving
/// the session zone to decide only how the result prints. Values pinned against
/// PG 18.4; the unit-error *wording* is PG 14's, which is the baseline the
/// suites target — 18.4 phrases it `unit "bogus" not recognized for type …`.
#[tokio::test]
async fn date_trunc_takes_an_explicit_zone() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client
        .simple_query("SET TIME ZONE 'America/New_York'")
        .await?;

    let cases = [
        // Local midnight in UTC is the previous evening in New York, where the
        // two-argument form lands on local midnight instead.
        (
            "SELECT date_trunc('day', '2024-03-10 15:00:00-04'::timestamptz, 'UTC')",
            "2024-03-09 19:00:00-05",
        ),
        (
            "SELECT date_trunc('day', '2024-03-10 15:00:00-04'::timestamptz)",
            "2024-03-10 00:00:00-05",
        ),
        // A bare numeric zone argument is POSIX-signed: `+05:30` is UTC-5:30, so
        // local midnight there is 05:30 UTC — 00:30 EST.
        (
            "SELECT date_trunc('day', '2001-02-16 20:38:40+00'::timestamptz, '+05:30')",
            "2001-02-16 00:30:00-05",
        ),
        // An abbreviation the `TimeZone` GUC would refuse is legal as an argument.
        (
            "SELECT date_trunc('day', '2001-02-16 20:38:40+00'::timestamptz, 'VET')",
            "2001-02-15 23:00:00-05",
        ),
    ];
    for (sql, want) in cases {
        assert_eq!(scalar(&client, sql).await, want, "for `{sql}`");
    }

    // The zone is resolved before the unit is looked at, and the connection
    // survives both errors.
    for (sql, want) in [
        (
            "SELECT date_trunc('bogus', now(), 'Nowhere/Nozone')",
            "time zone \"Nowhere/Nozone\" not recognized",
        ),
        (
            // PG 14 wording; 18.4 says `unit "bogus" not recognized for type …`.
            "SELECT date_trunc('bogus', now(), 'UTC')",
            "timestamp with time zone units \"bogus\" not recognized",
        ),
    ] {
        let err = client.simple_query(sql).await.expect_err("should fail");
        let db = err.as_db_error().expect("database error");
        assert_eq!(db.code(), &SqlState::INVALID_PARAMETER_VALUE, "for `{sql}`");
        assert_eq!(db.message(), want, "for `{sql}`");
    }
    assert_eq!(scalar(&client, "SELECT 1").await, "1");
    Ok(())
}

/// **Known divergence from PostgreSQL**, pinned here so it is visible rather
/// than silent.
///
/// PG resolves a `timestamptz` literal during *parse analysis*, freezing the
/// instant with the parsing session's zone; only the rendering follows a later
/// `SET`. Probed against PG 18.4 over the wire (Parse once, Execute twice):
///
/// ```text
/// SET TIME ZONE 'UTC';  Parse "SELECT ('2024-06-01 12:00:00'::timestamptz)::text"
///   Execute                            -> 2024-06-01 12:00:00+00
/// SET TIME ZONE 'America/New_York';
///   Execute                            -> 2024-06-01 08:00:00-04   (same instant)
/// ```
///
/// We defer the literal to a runtime `Coerce`, because the binder holds no
/// session zone to fold with — so the instant is recomputed per execution and
/// the second row reads `12:00:00-04`, a *different* instant. The same applies
/// to `'now'`, which PG likewise freezes at PREPARE.
///
/// Closing it takes a **plan cache**, not a session-aware binder: this server
/// re-binds the statement on every `Execute`, so folding at bind time would
/// simply re-fold with the new session state. Where the divergence would
/// actually cost data — a column default — the DDL path resolves the literal
/// itself, with the session in hand (`zoned_literal_default`), so
/// `DEFAULT 'now'` freezes at `CREATE TABLE` as PG's does.
#[tokio::test]
async fn prepared_statement_diverges_from_pg_on_a_later_set_timezone() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("SET TIME ZONE 'UTC'").await?;
    // `prepare` uses the extended protocol, so the statement is parsed once.
    // Rendered as text because `timestamptz` has no binary output routine yet,
    // which tokio-postgres would otherwise request.
    let stmt = client
        .prepare("SELECT ('2024-06-01 12:00:00'::timestamptz)::text")
        .await?;

    let before: String = client.query_one(&stmt, &[]).await?.get(0);
    assert_eq!(before, "2024-06-01 12:00:00+00");

    client
        .simple_query("SET TIME ZONE 'America/New_York'")
        .await?;
    let after: String = client.query_one(&stmt, &[]).await?.get(0);
    // PG would answer "2024-06-01 08:00:00-04" here.
    assert_eq!(after, "2024-06-01 12:00:00-04");
    Ok(())
}

/// A text bind parameter and an array literal must read their elements in the
/// session zone too — both go through input functions the scalar literal shares.
#[tokio::test]
async fn parameters_and_arrays_use_the_session_zone() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("SET TIME ZONE 'America/New_York'")
        .await?;

    let stmt = client
        .prepare("SELECT ($1::text)::timestamptz::text")
        .await?;
    let value: String = client
        .query_one(&stmt, &[&"2024-06-01 12:00:00"])
        .await?
        .get(0);
    assert_eq!(value, "2024-06-01 12:00:00-04");

    assert_eq!(
        scalar(&client, "SELECT '{2024-06-01 12:00:00}'::timestamptz[]").await,
        "{\"2024-06-01 12:00:00-04\"}"
    );
    Ok(())
}

/// Configuration parameters are transactional in PG. A plain `SET` inside a
/// block survives COMMIT but is undone by ROLLBACK; `SET LOCAL` is undone by
/// either.
#[tokio::test]
async fn set_local_and_set_are_transactional() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    client.simple_query("BEGIN").await?;
    client
        .simple_query("SET LOCAL TimeZone = 'Asia/Tokyo'")
        .await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "Asia/Tokyo");
    client.simple_query("COMMIT").await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "UTC");

    client.simple_query("BEGIN").await?;
    client
        .simple_query("SET LOCAL TimeZone = 'Asia/Tokyo'")
        .await?;
    client.simple_query("ROLLBACK").await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "UTC");

    // A plain SET is kept by COMMIT ...
    client.simple_query("BEGIN").await?;
    client.simple_query("SET TimeZone = 'Asia/Tokyo'").await?;
    client.simple_query("COMMIT").await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "Asia/Tokyo");

    // ... and undone by ROLLBACK.
    client.simple_query("BEGIN").await?;
    client.simple_query("SET TimeZone = 'Europe/Paris'").await?;
    client.simple_query("ROLLBACK").await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "Asia/Tokyo");
    Ok(())
}

/// A block may issue both spellings on one parameter, in either order, and
/// PostgreSQL keeps two levels to answer that: COMMIT unmasks the *session*
/// value a plain SET established rather than reverting to the pre-block one.
/// One save slot could only ever restore the latter, which was wrong both ways
/// round.
#[tokio::test]
async fn a_plain_set_survives_the_set_local_masking_it() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    // SET LOCAL then SET: the plain SET wins outright, during and after.
    client.simple_query("BEGIN").await?;
    client
        .simple_query("SET LOCAL TimeZone = 'Asia/Tokyo'")
        .await?;
    client.simple_query("SET TimeZone = 'Europe/Paris'").await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "Europe/Paris");
    client.simple_query("COMMIT").await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "Europe/Paris");

    // SET then SET LOCAL: the local masks it until COMMIT unmasks.
    client.simple_query("BEGIN").await?;
    client
        .simple_query("SET TimeZone = 'Europe/Berlin'")
        .await?;
    client
        .simple_query("SET LOCAL TimeZone = 'Asia/Tokyo'")
        .await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "Asia/Tokyo");
    client.simple_query("COMMIT").await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "Europe/Berlin");

    // ROLLBACK still discards both levels.
    client.simple_query("BEGIN").await?;
    client
        .simple_query("SET TimeZone = 'America/Denver'")
        .await?;
    client
        .simple_query("SET LOCAL TimeZone = 'Asia/Tokyo'")
        .await?;
    client.simple_query("ROLLBACK").await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "Europe/Berlin");
    Ok(())
}

/// `SET x = DEFAULT` is `RESET x`, so it returns `pg_settings.source` to
/// `default` — it used to report `session`, disagreeing with the RESET spelling
/// of the same operation.
#[tokio::test]
async fn set_to_default_reports_source_default() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    let source = |name: &str| format!("SELECT source FROM pg_settings WHERE name = '{name}'");

    client.simple_query("SET extra_float_digits = 2").await?;
    assert_eq!(
        scalar(&client, &source("extra_float_digits")).await,
        "session"
    );
    client
        .simple_query("SET extra_float_digits = DEFAULT")
        .await?;
    assert_eq!(
        scalar(&client, &source("extra_float_digits")).await,
        "default"
    );

    // `SET TIME ZONE DEFAULT` and `SET TIME ZONE LOCAL` reduce to the same
    // value, so they answer the same way.
    for reset in ["DEFAULT", "LOCAL"] {
        client.simple_query("SET TimeZone = 'Asia/Tokyo'").await?;
        assert_eq!(scalar(&client, &source("TimeZone")).await, "session");
        client
            .simple_query(&format!("SET TIME ZONE {reset}"))
            .await?;
        assert_eq!(scalar(&client, &source("TimeZone")).await, "default");
        assert_eq!(scalar(&client, "SHOW TimeZone").await, "UTC");
    }
    Ok(())
}

/// `SET SESSION CHARACTERISTICS` writes only the modes it names, so only those
/// become `source = session`. It used to mark both parameters whichever was
/// given.
#[tokio::test]
async fn session_characteristics_marks_only_named_parameters() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    let iso = "SELECT source FROM pg_settings WHERE name = 'default_transaction_isolation'";
    let ro = "SELECT source FROM pg_settings WHERE name = 'default_transaction_read_only'";

    client
        .simple_query("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY")
        .await?;
    assert_eq!(scalar(&client, iso).await, "default");
    assert_eq!(scalar(&client, ro).await, "session");

    client
        .simple_query("SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .await?;
    assert_eq!(scalar(&client, iso).await, "session");
    assert_eq!(scalar(&client, ro).await, "session");

    // ...and both are transactional, like a plain SET of either.
    client.simple_query("RESET ALL").await?;
    client.simple_query("BEGIN").await?;
    client
        .simple_query("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY")
        .await?;
    client.simple_query("ROLLBACK").await?;
    assert_eq!(scalar(&client, iso).await, "default");
    assert_eq!(scalar(&client, ro).await, "default");
    Ok(())
}

/// Outside a block there is nothing for a `SET LOCAL` to be local to, so PG
/// warns and does nothing.
#[tokio::test]
async fn set_local_outside_a_transaction_warns() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client
        .simple_query("SET LOCAL TimeZone = 'Asia/Tokyo'")
        .await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "UTC");
    Ok(())
}

#[tokio::test]
async fn guc_errors_match_pg() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;

    let err = client
        .simple_query("SET TimeZone = 'Nowhere/Nozone'")
        .await
        .expect_err("an unknown time zone name must be rejected");
    let db = err.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::INVALID_PARAMETER_VALUE);
    assert_eq!(
        db.message(),
        "invalid value for parameter \"TimeZone\": \"Nowhere/Nozone\""
    );
    // The failed SET left the old value in place.
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "UTC");

    let err = client
        .simple_query("SHOW bogus_param")
        .await
        .expect_err("an unrecognized configuration parameter must be rejected");
    let db = err.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::UNDEFINED_OBJECT);
    assert_eq!(
        db.message(),
        "unrecognized configuration parameter \"bogus_param\""
    );

    let err = client
        .simple_query("SET server_version = '1'")
        .await
        .expect_err("setting a read-only parameter must be rejected");
    let db = err.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::CANT_CHANGE_RUNTIME_PARAM);
    assert_eq!(
        db.message(),
        "parameter \"server_version\" cannot be changed"
    );

    // Divergence, on purpose: an unrecognized *name* is accepted by SET (PG
    // raises 42704). Drivers set parameters we do not model.
    client.simple_query("SET application_name = 'x'").await?;
    Ok(())
}

/// The numeric `SET TIME ZONE` forms count *east*, while a bare numeric string
/// is POSIX and counts west — opposite conventions for the same digits.
#[tokio::test]
async fn timezone_numeric_forms_follow_pg_sign_conventions() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    client.simple_query("SET TIME ZONE 7").await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "<+07>-07");
    assert_eq!(
        scalar(&client, "SELECT '2024-06-01 12:00:00+00'::timestamptz").await,
        "2024-06-01 19:00:00+07"
    );

    client.simple_query("SET TIME ZONE -5").await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "<-05>+05");

    // The string form is POSIX: `'+05:30'` puts the session at UTC-5:30.
    client.simple_query("SET TimeZone = '+05:30'").await?;
    assert_eq!(
        scalar(&client, "SELECT '2024-06-01 12:00:00+00'::timestamptz").await,
        "2024-06-01 06:30:00-05:30"
    );

    // Both `LOCAL` and `DEFAULT` restore the boot value.
    client.simple_query("SET TIME ZONE LOCAL").await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "UTC");

    // Names are canonicalized to the tzdb spelling.
    client
        .simple_query("SET TimeZone = 'america/new_york'")
        .await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "America/New_York");
    Ok(())
}

/// `TimeZone` is `GUC_REPORT`: the server echoes a `ParameterStatus` whenever
/// the value actually changes, including when a transaction reverts it.
/// tokio-postgres hides these, so this drives a raw socket.
#[tokio::test]
async fn timezone_changes_emit_parameter_status() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let mut socket = raw_session(port).await;

    /// Run one simple query, returning the `ParameterStatus` pairs it emitted.
    async fn query(
        socket: &mut tokio::net::TcpStream,
        sql: &str,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let mut body = sql.as_bytes().to_vec();
        body.push(0);
        socket.write_all(&frontend_message(b'Q', &body)).await?;
        Ok(read_until_ready(socket)
            .await
            .into_iter()
            .filter(|(tag, _)| *tag == b'S')
            .map(|(_, body)| {
                let mut parts = body.split(|b| *b == 0);
                let name = String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
                let value = String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
                (name, value)
            })
            .collect())
    }

    assert_eq!(
        query(&mut socket, "SET TimeZone = 'Asia/Tokyo'").await?,
        vec![("TimeZone".to_string(), "Asia/Tokyo".to_string())]
    );
    // Setting the same value again reports nothing: PG echoes changes, not SETs.
    assert_eq!(
        query(&mut socket, "SET TimeZone = 'Asia/Tokyo'").await?,
        vec![]
    );
    // A parameter that is not GUC_REPORT is never echoed.
    assert_eq!(
        query(&mut socket, "SET extra_float_digits = 0").await?,
        vec![]
    );
    assert_eq!(
        query(&mut socket, "RESET TimeZone").await?,
        vec![("TimeZone".to_string(), "UTC".to_string())]
    );

    // The revert at the end of a block is a change too, and must be reported —
    // the case a design where each SET announces itself would miss.
    query(&mut socket, "BEGIN").await?;
    assert_eq!(
        query(&mut socket, "SET LOCAL TimeZone = 'Europe/Paris'").await?,
        vec![("TimeZone".to_string(), "Europe/Paris".to_string())]
    );
    assert_eq!(
        query(&mut socket, "COMMIT").await?,
        vec![("TimeZone".to_string(), "UTC".to_string())]
    );

    // Unmasking at commit is a change too, and it lands on the *session* value a
    // plain SET established rather than on the pre-block one — which is the
    // cheapest wire-level guard on the two-level save stack, since a one-slot
    // one could only ever announce `UTC` here.
    query(&mut socket, "SET TimeZone = 'Europe/Paris'").await?;
    query(&mut socket, "BEGIN").await?;
    query(&mut socket, "SET LOCAL TimeZone = 'Asia/Tokyo'").await?;
    assert_eq!(
        query(&mut socket, "COMMIT").await?,
        vec![("TimeZone".to_string(), "Europe/Paris".to_string())]
    );
    query(&mut socket, "RESET TimeZone").await?;

    // An ordinary statement carries none.
    assert_eq!(query(&mut socket, "SELECT 1").await?, vec![]);
    Ok(())
}

/// `IntervalStyle` is GUC_REPORT in PostgreSQL — it rides in the startup burst
/// and every change is echoed — and it is transactional like every other GUC.
#[tokio::test]
async fn interval_style_changes_emit_parameter_status() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let mut socket = raw_session(port).await;

    async fn query(
        socket: &mut tokio::net::TcpStream,
        sql: &str,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let mut body = sql.as_bytes().to_vec();
        body.push(0);
        socket.write_all(&frontend_message(b'Q', &body)).await?;
        Ok(read_until_ready(socket)
            .await
            .into_iter()
            .filter(|(tag, _)| *tag == b'S')
            .map(|(_, body)| {
                let mut parts = body.split(|b| *b == 0);
                let name = String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
                let value = String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
                (name, value)
            })
            .collect())
    }
    let reported = |value: &str| vec![("IntervalStyle".to_string(), value.to_string())];

    assert_eq!(
        query(&mut socket, "SET IntervalStyle TO iso_8601").await?,
        reported("iso_8601")
    );
    // Same value again is not a change.
    assert_eq!(
        query(&mut socket, "SET IntervalStyle TO iso_8601").await?,
        vec![]
    );
    assert_eq!(
        query(&mut socket, "RESET IntervalStyle").await?,
        reported("postgres")
    );

    // The revert at the end of a block is a change, and must be reported.
    query(&mut socket, "BEGIN").await?;
    assert_eq!(
        query(&mut socket, "SET LOCAL IntervalStyle TO sql_standard").await?,
        reported("sql_standard")
    );
    assert_eq!(query(&mut socket, "COMMIT").await?, reported("postgres"));

    // So is a rollback of a plain SET inside a block.
    query(&mut socket, "BEGIN").await?;
    query(&mut socket, "SET IntervalStyle TO postgres_verbose").await?;
    assert_eq!(query(&mut socket, "ROLLBACK").await?, reported("postgres"));
    Ok(())
}

/// The GUC picks the rendering, and a rejected value carries PG's HINT.
/// The per-style expectations themselves are pinned in `interval.rs`'s unit
/// tests; this only proves the session state reaches `interval_out`.
#[tokio::test]
async fn interval_style_selects_the_output_form() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    assert_eq!(scalar(&client, "SHOW IntervalStyle").await, "postgres");
    for (style, want) in [
        ("postgres", "1 day -01:00:00"),
        ("postgres_verbose", "@ 1 day -1 hours"),
        ("sql_standard", "+0-0 +1 -1:00:00"),
        ("iso_8601", "P1DT-1H"),
    ] {
        client
            .simple_query(&format!("SET IntervalStyle TO {style}"))
            .await?;
        assert_eq!(scalar(&client, "SHOW IntervalStyle").await, style);
        assert_eq!(
            scalar(&client, "SELECT interval '1 day -1 hour'").await,
            want,
            "style {style}"
        );
    }
    // The name is matched case-insensitively, quoted or not.
    client
        .simple_query("SET intervalstyle TO 'SQL_STANDARD'")
        .await?;
    assert_eq!(scalar(&client, "SHOW IntervalStyle").await, "sql_standard");

    // Case-insensitively and nothing more: padding is part of the value, so a
    // padded name is rejected like any other unrecognized one.
    for value in ["bogus", "' postgres '"] {
        let err = client
            .simple_query(&format!("SET IntervalStyle TO {value}"))
            .await
            .expect_err("an unrecognized IntervalStyle must be rejected");
        let db = err.as_db_error().expect("database error");
        assert_eq!(db.code().code(), "22023", "{value}");
        assert_eq!(
            db.message(),
            format!(
                "invalid value for parameter \"IntervalStyle\": \"{}\"",
                value.trim_matches('\'')
            ),
            "{value}"
        );
        assert_eq!(
            db.hint(),
            Some("Available values: postgres, postgres_verbose, sql_standard, iso_8601."),
            "{value}"
        );
    }
    Ok(())
}

/// PostgreSQL's one-argument `age` spans two type categories — the two datetime
/// forms and `age(xid)` — so an untyped argument has no best candidate and the
/// call is ambiguous rather than quietly resolving to a datetime overload.
#[tokio::test]
async fn one_argument_age_is_ambiguous_for_an_untyped_argument() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    for sql in ["SELECT age('2001-01-01')", "SELECT age(NULL)"] {
        let err = client
            .simple_query(sql)
            .await
            .expect_err("an untyped one-argument age must be ambiguous");
        let db = err.as_db_error().expect("database error");
        assert_eq!(db.code().code(), "42725", "{sql}");
        assert_eq!(db.message(), "function age(unknown) is not unique", "{sql}");
        assert_eq!(
            db.hint(),
            Some(
                "Could not choose a best candidate function. You might need to add explicit type casts."
            ),
            "{sql}"
        );
    }

    // The load-bearing half: the ambiguity must fire *only* for the untyped
    // case. Nothing can reach the xid overload but an xid, so every typed call
    // resolves exactly as it did before it existed.
    for (sql, want) in [
        ("SELECT pg_typeof(age(DATE '2001-01-01'))", "interval"),
        ("SELECT pg_typeof(age(TIMESTAMP '2001-01-01'))", "interval"),
        (
            "SELECT pg_typeof(age(TIMESTAMPTZ '2001-01-01+00'))",
            "interval",
        ),
        ("SELECT age(DATE '2001-01-01', DATE '2000-01-01')", "1 year"),
        (
            "SELECT age(TIMESTAMP '2001-01-01', TIMESTAMP '2000-01-01')",
            "1 year",
        ),
    ] {
        assert_eq!(scalar(&client, sql).await, want, "{sql}");
    }

    // `age(xid)` itself: how many transactions have started since that one.
    // Pinned relatively, because the absolute counter is not reproducible.
    assert_eq!(
        scalar(&client, "SELECT age('3'::xid) - age('4'::xid)").await,
        "1"
    );
    assert_eq!(
        scalar(&client, "SELECT age('100'::xid) - age('1100'::xid)").await,
        "1000"
    );
    // A 32-bit wrapping difference, so an xid past the counter reads as one
    // more than the lowest normal one rather than as a huge number.
    assert_eq!(
        scalar(&client, "SELECT age('4294967295'::xid) - age('3'::xid)").await,
        "4"
    );
    // XIDs below the first normal one are permanent, and report as infinitely
    // old rather than as a difference.
    assert_eq!(
        scalar(
            &client,
            "SELECT age('0'::xid), age('1'::xid), age('2'::xid)"
        )
        .await,
        "2147483647"
    );
    assert_eq!(scalar(&client, "SELECT age(NULL::xid) IS NULL").await, "t");
    // Read-only transactions never allocate an XID, so the answer cannot come
    // from `TxnContext::xid`.
    client.simple_query("BEGIN READ ONLY").await?;
    assert_eq!(
        scalar(&client, "SELECT age('3'::xid) - age('4'::xid)").await,
        "1"
    );
    client.simple_query("COMMIT").await?;
    Ok(())
}

#[tokio::test]
async fn show_all_lists_the_known_parameters() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    let rows = client.simple_query("SHOW ALL").await?;
    let names: Vec<String> = rows
        .iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(row) => row.get("name").map(str::to_string),
            _ => None,
        })
        .collect();
    assert!(names.contains(&"TimeZone".to_string()), "got {names:?}");
    assert!(
        names.contains(&"server_version".to_string()),
        "got {names:?}"
    );
    // Three columns: name, setting, description.
    let first = rows
        .iter()
        .find_map(|m| match m {
            SimpleQueryMessage::Row(row) => Some(row.len()),
            _ => None,
        })
        .expect("at least one row");
    assert_eq!(first, 3);
    Ok(())
}

/// The statements every `pg_dump` file opens with. These name real
/// `PGC_USERSET` parameters we do not model; PG accepts them, and so must we —
/// rejecting them broke restores.
#[tokio::test]
async fn pg_dump_preamble_parameters_are_accepted() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    for sql in [
        "SET client_encoding = 'UTF8'",
        "SET standard_conforming_strings = on",
        "SET DATESTYLE = 'ISO'",
        "set datestyle to postgres, dmy",
        "SET client_encoding TO 'LATIN1'",
    ] {
        client
            .simple_query(sql)
            .await
            .unwrap_or_else(|e| panic!("`{sql}` must be accepted: {e}"));
    }
    // Accepted, but a no-op: we implement exactly one value for each.
    assert_eq!(scalar(&client, "SHOW client_encoding").await, "UTF8");
    assert_eq!(scalar(&client, "SHOW DateStyle").await, "ISO, MDY");
    Ok(())
}

/// PG's `boolin` takes any unambiguous prefix, not just the full words.
#[tokio::test]
async fn boolean_parameters_accept_pg_prefixes() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    for (value, want) in [("tr", "on"), ("fal", "off"), ("ye", "on"), ("of", "off")] {
        client
            .simple_query(&format!("SET default_transaction_read_only = {value}"))
            .await?;
        assert_eq!(
            scalar(&client, "SHOW default_transaction_read_only").await,
            want,
            "for `{value}`"
        );
    }
    Ok(())
}

/// A bare number is east-signed in *both* statement spellings — only a quoted
/// numeric string is POSIX. Getting this wrong put the two spellings 10 hours
/// apart.
#[tokio::test]
async fn numeric_timezone_is_east_signed_in_both_spellings() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    for sql in [
        "SET TIME ZONE -5",
        "SET timezone TO -5",
        "SET timezone = -5",
    ] {
        client.simple_query("SET TimeZone = 'UTC'").await?;
        client.simple_query(sql).await?;
        assert_eq!(
            scalar(&client, "SHOW TimeZone").await,
            "<-05>+05",
            "for `{sql}`"
        );
        assert_eq!(
            scalar(&client, "SELECT '2024-06-01 12:00:00+00'::timestamptz").await,
            "2024-06-01 07:00:00-05",
            "for `{sql}`"
        );
    }

    // The documented spelling from PG's manual.
    client
        .simple_query("SET TIME ZONE INTERVAL '-08:00' HOUR TO MINUTE")
        .await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "<-08>+08");

    // The GUC forms allow a far wider range than a zone token in a value does:
    // PG takes up to ±167 hours and rejects 168.
    client.simple_query("SET TIME ZONE 167").await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "<+167>-167");
    let err = client
        .simple_query("SET TIME ZONE 168")
        .await
        .expect_err("a UTC offset beyond the accepted range must be rejected");
    let db = err.as_db_error().expect("database error");
    assert_eq!(
        db.message(),
        "invalid value for parameter \"TimeZone\": \"168\""
    );
    Ok(())
}

/// `RESET <name>` on a read-only parameter errors, while `RESET ALL` skips it.
#[tokio::test]
async fn reset_distinguishes_named_from_all() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;

    let err = client
        .simple_query("RESET server_version")
        .await
        .expect_err("resetting a read-only parameter must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::CANT_CHANGE_RUNTIME_PARAM
    );

    client.simple_query("SET TimeZone = 'Asia/Tokyo'").await?;
    client.simple_query("RESET ALL").await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "UTC");
    Ok(())
}

/// `SET SESSION CHARACTERISTICS` writes the same parameters a plain `SET` does,
/// so it has to be equally transactional.
#[tokio::test]
async fn set_session_characteristics_is_transactional() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("BEGIN").await?;
    client
        .simple_query("SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .await?;
    client.simple_query("ROLLBACK").await?;
    assert_eq!(
        scalar(&client, "SHOW default_transaction_isolation").await,
        "read committed"
    );
    Ok(())
}

/// The transactional restore has to survive a zone whose `SHOW` form is a POSIX
/// spec — `<+07>-07` is a *display* string that no setter can parse back, so
/// round-tripping the rendered value silently dropped the zone.
#[tokio::test]
async fn rollback_restores_a_posix_spec_zone() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("SET TIME ZONE 7").await?;
    client.simple_query("BEGIN").await?;
    client.simple_query("SET TimeZone = 'UTC'").await?;
    client.simple_query("ROLLBACK").await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "<+07>-07");

    client.simple_query("BEGIN").await?;
    client.simple_query("SET LOCAL TimeZone = 'UTC'").await?;
    client.simple_query("COMMIT").await?;
    assert_eq!(scalar(&client, "SHOW TimeZone").await, "<+07>-07");
    Ok(())
}

/// A partition bound is a wall clock read in the *defining* session's zone,
/// exactly as the INSERT that routes a row reads its value. Folding the bound
/// under UTC while routing under the session zone put rows in the wrong leaf.
#[tokio::test]
async fn partition_bounds_use_the_session_zone() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;
    let client = connect(spawn_server().await).await;
    client
        .simple_query("SET TimeZone = 'America/New_York'")
        .await?;
    client
        .simple_query("CREATE TABLE p (ts timestamptz) PARTITION BY RANGE (ts)")
        .await?;
    client
        .simple_query(
            "CREATE TABLE p1 PARTITION OF p \
             FOR VALUES FROM ('2024-01-01 00:00:00') TO ('2024-02-01 00:00:00')",
        )
        .await?;

    // 20:00 local on Dec 31 is 01:00 UTC on Jan 1 — inside the range only if the
    // bound was (wrongly) read as UTC.
    let err = client
        .simple_query("INSERT INTO p VALUES ('2023-12-31 20:00:00')")
        .await
        .expect_err("a timestamp outside the partition's local-time range must be rejected");
    assert_eq!(
        err.as_db_error().expect("database error").code(),
        &SqlState::CHECK_VIOLATION
    );

    client
        .simple_query("INSERT INTO p VALUES ('2024-01-15 12:00:00')")
        .await?;
    assert_eq!(scalar(&client, "SELECT count(*) FROM p1").await, "1");
    Ok(())
}

/// A bound may name the session identity, and PostgreSQL folds the *value* into
/// the stored bound at CREATE time. The fold used to run with a context that
/// carried the session's GUCs but no catalog handle, so `current_setting()` in a
/// bound worked while `current_user` raised XX000.
#[tokio::test]
async fn partition_bounds_fold_session_identity() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    let user = scalar(&client, "SELECT current_user").await;
    client
        .simple_query("CREATE TABLE ident (a text) PARTITION BY RANGE (a)")
        .await?;
    client
        .simple_query(
            "CREATE TABLE ident1 PARTITION OF ident FOR VALUES FROM (current_user) TO ('zzz')",
        )
        .await?;

    // The leaf carries the value, not the call.
    let bound = scalar(
        &client,
        "SELECT pg_get_expr(relpartbound, oid) FROM pg_class WHERE relname = 'ident1'",
    )
    .await;
    assert_eq!(bound, format!("FOR VALUES FROM ('{user}') TO ('zzz')"));

    // ...and that stored value is what routes a row.
    client
        .simple_query("INSERT INTO ident VALUES (current_user)")
        .await?;
    assert_eq!(scalar(&client, "SELECT count(*) FROM ident1").await, "1");
    Ok(())
}

/// `SHOW` returns rows, so Describe must answer with a RowDescription. The
/// utility catch-all reported NoData and Execute then streamed DataRows the
/// client had been told not to expect.
#[tokio::test]
async fn show_describes_its_columns_in_the_extended_protocol() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let mut socket = raw_session(port).await;

    for (sql, want_columns) in [("SHOW TimeZone", 1u16), ("SHOW ALL", 3)] {
        let name = format!("s_{want_columns}");
        let mut parse = name.clone().into_bytes();
        parse.push(0);
        parse.extend_from_slice(sql.as_bytes());
        parse.push(0);
        parse.extend_from_slice(&0i16.to_be_bytes());
        let mut describe = vec![b'S'];
        describe.extend_from_slice(name.as_bytes());
        describe.push(0);

        let mut batch = frontend_message(b'P', &parse);
        batch.extend_from_slice(&frontend_message(b'D', &describe));
        batch.extend_from_slice(&frontend_message(b'S', b""));
        socket.write_all(&batch).await?;

        let messages = read_until_ready(&mut socket).await;
        let tags: Vec<u8> = messages.iter().map(|(tag, _)| *tag).collect();
        assert!(
            !tags.contains(&b'n'),
            "`{sql}` Describe answered NoData: {:?}",
            tags.iter().map(|t| *t as char).collect::<Vec<_>>()
        );
        let row_description = messages
            .iter()
            .find(|(tag, _)| *tag == b'T')
            .map(|(_, body)| u16::from_be_bytes([body[0], body[1]]))
            .unwrap_or_else(|| panic!("`{sql}` sent no RowDescription"));
        assert_eq!(row_description, want_columns, "for `{sql}`");
    }
    Ok(())
}

/// `CREATE TABLE ... INHERITS (...)`: the merged column layout, the notices PG
/// raises while merging, and the `pg_inherits`/`pg_class` reflection.
///
/// The hierarchy is `test_setup.sql`'s, because its `stud_emp` is the shape that
/// makes the merge non-trivial: `student`'s layout is *not* a prefix of
/// `stud_emp`'s, so nothing can get away with assuming inherited columns stay
/// contiguous.
#[tokio::test]
async fn table_inheritance_merges_columns_and_reflects_in_the_catalog() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    client
        .simple_query("CREATE TABLE person (name text, age int4)")
        .await?;
    client
        .simple_query("CREATE TABLE emp (salary int4, manager name) INHERITS (person)")
        .await?;
    client
        .simple_query("CREATE TABLE student (gpa float8) INHERITS (person)")
        .await?;
    client
        .simple_query("CREATE TABLE stud_emp (percent int4) INHERITS (emp, student)")
        .await?;
    // An empty column list is legal and yields a verbatim copy of the parent.
    client
        .simple_query("CREATE TABLE clone () INHERITS (person)")
        .await?;

    let layout = |table: &str| {
        let sql = format!(
            "SELECT a.attname FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid \
             WHERE c.relname = '{table}' AND a.attnum > 0 ORDER BY a.attnum"
        );
        let client = &client;
        async move {
            let msgs = client.simple_query(&sql).await?;
            Ok::<_, anyhow::Error>(
                rows(&msgs)
                    .iter()
                    .filter_map(|r| r.get(0).map(str::to_string))
                    .collect::<Vec<_>>(),
            )
        }
    };
    assert_eq!(layout("emp").await?, ["name", "age", "salary", "manager"]);
    assert_eq!(layout("student").await?, ["name", "age", "gpa"]);
    assert_eq!(layout("clone").await?, ["name", "age"]);
    // Parents left to right, each contributing only names not already merged,
    // then the child's own. `gpa` lands at position 5 rather than 3, which is
    // exactly what a naive prefix assumption would get wrong.
    assert_eq!(
        layout("stud_emp").await?,
        ["name", "age", "salary", "manager", "gpa", "percent"]
    );

    // One `pg_inherits` row per parent link, numbered in declaration order.
    let msgs = client
        .simple_query(
            "SELECT p.relname, i.inhseqno FROM pg_inherits i \
             JOIN pg_class c ON c.oid = i.inhrelid JOIN pg_class p ON p.oid = i.inhparent \
             WHERE c.relname = 'stud_emp' ORDER BY i.inhseqno",
        )
        .await?;
    let links: Vec<_> = rows(&msgs).iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(
        links,
        vec![(Some("emp"), Some("1")), (Some("student"), Some("2"))]
    );

    // Neither end of an inheritance link is a partition: both stay `relkind='r'`.
    let msgs = client
        .simple_query(
            "SELECT relkind, relispartition FROM pg_class \
             WHERE relname IN ('person', 'stud_emp') ORDER BY relname",
        )
        .await?;
    for row in rows(&msgs) {
        assert_eq!((row.get(0), row.get(1)), (Some("r"), Some("f")));
    }

    Ok(())
}

/// The NOTICEs the merge raises, and the errors that stop it. PG reports a clash
/// between two parents differently from a clash between the child's own
/// declaration and what it inherited, so both spellings are pinned.
#[tokio::test]
async fn table_inheritance_reports_merges_and_conflicts_like_pg() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let port = spawn_server().await;
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

    // NOTICEs are read off a raw socket: the pooled client discards them.
    let mut socket = raw_session(port).await;
    let notices = |msgs: &[(u8, Vec<u8>)]| {
        msgs.iter()
            .filter(|(tag, _)| *tag == b'N')
            .map(|m| fields(m).message().to_string())
            .collect::<Vec<_>>()
    };
    // Merging two parents that share a common ancestor reports each shared
    // column once, in the order the merge reaches them.
    let msgs = simple_query_raw(
        &mut socket,
        "CREATE TABLE stud_emp (percent int4) INHERITS (emp, student)",
    )
    .await;
    assert_eq!(
        notices(&msgs),
        [
            "merging multiple inherited definitions of column \"name\"",
            "merging multiple inherited definitions of column \"age\"",
        ]
    );
    // A child redeclaring an inherited column refines it; different wording.
    let msgs = simple_query_raw(
        &mut socket,
        "CREATE TABLE tightened (name text NOT NULL) INHERITS (person)",
    )
    .await;
    assert_eq!(
        notices(&msgs),
        ["merging column \"name\" with inherited definition"]
    );

    // PG raises the merge NOTICE and *then* the conflict that stopped it. The
    // notices go to the session sink for exactly that reason: a `Result`'s `Ok`
    // half cannot carry them past the error, and `PgError` has nowhere to put
    // them. Asserting the *order* of the wire messages, not just their presence.
    let msgs = simple_query_raw(
        &mut socket,
        "CREATE TABLE conflicted (name int4) INHERITS (person)",
    )
    .await;
    let kinds: Vec<u8> = msgs
        .iter()
        .map(|(tag, _)| *tag)
        .filter(|tag| *tag == b'N' || *tag == b'E')
        .collect();
    assert_eq!(kinds, [b'N', b'E'], "the NOTICE must precede the ERROR");
    assert_eq!(
        notices(&msgs),
        ["merging column \"name\" with inherited definition"]
    );
    let e = fields(msgs.iter().find(|(tag, _)| *tag == b'E').expect("an ERROR"));
    assert_eq!(e.message(), "column \"name\" has a type conflict");
    assert_eq!(e.get(b'D'), Some("text versus integer"));

    // A child that declares an inherited column somewhere other than its
    // inherited position is told the column moved — PG's other wording.
    let msgs = simple_query_raw(
        &mut socket,
        "CREATE TABLE moved (age int4, extra int4) INHERITS (person)",
    )
    .await;
    let notice = fields(
        msgs.iter()
            .find(|(tag, _)| *tag == b'N')
            .expect("a NOTICE is expected"),
    );
    assert_eq!(
        notice.message(),
        "moving and merging column \"age\" with inherited definition"
    );
    assert_eq!(
        notice.get(b'D'),
        Some("User-specified column moved to the position of the inherited column.")
    );

    // A redeclared column merges into its inherited position rather than
    // appearing twice.
    let msgs = client
        .simple_query(
            "SELECT count(*) FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid \
             WHERE c.relname = 'tightened' AND a.attname = 'name'",
        )
        .await?;
    assert_eq!(rows(&msgs)[0].get(0), Some("1"));

    let err = |sql: &'static str| {
        let client = &client;
        async move {
            client
                .simple_query(sql)
                .await
                .expect_err("the statement must fail")
        }
    };

    // The child's own declaration versus what it inherited.
    let e = err("CREATE TABLE bad (age text) INHERITS (person)").await;
    let db = e.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::DATATYPE_MISMATCH);
    assert_eq!(db.message(), "column \"age\" has a type conflict");
    assert_eq!(db.detail(), Some("integer versus text"));

    // A conflict the merge can only see because the column records its type
    // *modifier*, not just its type. This is what the inheritance check needs
    // from `Column::typmod`, and the encoding it goes through is not the one
    // stored (`bpchar` has no header, `char(5)` does), so both spellings are
    // pinned.
    for (parent, child, detail) in [
        (
            "numeric(10,4)",
            "numeric(10,7)",
            "numeric(10,4) versus numeric(10,7)",
        ),
        (
            "timestamp(2)",
            "timestamp(4)",
            "timestamp(2) without time zone versus timestamp(4) without time zone",
        ),
        ("bpchar", "char(5)", "bpchar versus character(5)"),
    ] {
        client
            .simple_query(&format!("CREATE TABLE tp_{} (v {parent})", detail.len()))
            .await?;
        let e = client
            .simple_query(&format!(
                "CREATE TABLE tc_{} (v {child}) INHERITS (tp_{})",
                detail.len(),
                detail.len()
            ))
            .await
            .expect_err("a differing type modifier is a conflict");
        let db = e.as_db_error().expect("database error");
        assert_eq!(db.code(), &SqlState::DATATYPE_MISMATCH);
        assert_eq!(db.detail(), Some(detail), "for {parent} vs {child}");
    }

    // Two parents disagreeing — a different message, same SQLSTATE.
    client
        .simple_query("CREATE TABLE other (age float8)")
        .await?;
    let e = err("CREATE TABLE bad2 () INHERITS (person, other)").await;
    let db = e.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::DATATYPE_MISMATCH);
    assert_eq!(db.message(), "inherited column \"age\" has a type conflict");
    assert_eq!(db.detail(), Some("integer versus double precision"));

    // Conflicting inherited defaults are unresolvable; PG names the fix.
    client
        .simple_query("CREATE TABLE d1 (a int4 DEFAULT 1)")
        .await?;
    client
        .simple_query("CREATE TABLE d2 (a int4 DEFAULT 2)")
        .await?;
    let e = err("CREATE TABLE bad3 () INHERITS (d1, d2)").await;
    let db = e.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::from_code("42611"));
    assert_eq!(
        db.message(),
        "column \"a\" inherits conflicting default values"
    );
    assert_eq!(
        db.hint(),
        Some("To resolve the conflict, specify a default explicitly.")
    );

    let e = err("CREATE TABLE bad4 () INHERITS (person, person)").await;
    assert_eq!(
        e.as_db_error().expect("database error").message(),
        "relation \"person\" would be inherited from more than once"
    );

    // A temporary relation anywhere in a hierarchy is refused, and the message
    // says so rather than borrowing the permanent-child wording. Naming a temp
    // parent unqualified must find it: DDL runs against the raw engine, which
    // resolves `public` only, so before the temp probe this reported that a
    // relation plainly present "does not exist".
    client.simple_query("CREATE TEMP TABLE tp (a int4)").await?;
    for sql in [
        "CREATE TEMP TABLE tc1 () INHERITS (tp)",
        "CREATE TEMP TABLE tc2 () INHERITS (person)",
    ] {
        let e = err(sql).await;
        let db = e.as_db_error().expect("database error");
        assert_eq!(db.code(), &SqlState::FEATURE_NOT_SUPPORTED);
        assert_eq!(
            db.message(),
            "temporary tables in an inheritance hierarchy are not supported yet"
        );
    }
    // A permanent child of a temp parent keeps PG's own wording, verbatim.
    let e = err("CREATE TABLE pc () INHERITS (tp)").await;
    let db = e.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::WRONG_OBJECT_TYPE);
    assert_eq!(
        db.message(),
        "cannot inherit from temporary relation \"tp\""
    );

    // An engine-managed relation is refused on *either* side of the link. The
    // child side was already guarded; a parquet parent was not, and it would
    // have taken every read of the parent off the batch path.
    client
        .simple_query("CREATE TABLE pq (a int4) USING parquet ORDER BY (a)")
        .await?;
    let e = err("CREATE TABLE pqc () INHERITS (pq)").await;
    let db = e.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::FEATURE_NOT_SUPPORTED);
    assert_eq!(
        db.message(),
        "table access method \"parquet\" does not support inheritance"
    );

    client
        .simple_query("CREATE VIEW v AS SELECT 1 AS x")
        .await?;
    let e = err("CREATE TABLE bad5 () INHERITS (v)").await;
    let db = e.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::WRONG_OBJECT_TYPE);
    assert_eq!(
        db.message(),
        "inherited relation \"v\" is not a table or foreign table"
    );

    let e = err("CREATE TABLE bad6 () INHERITS (nope)").await;
    assert_eq!(
        e.as_db_error().expect("database error").code(),
        &SqlState::UNDEFINED_TABLE
    );

    Ok(())
}

/// Reading and writing through an inheritance parent: a scan unions the parent
/// with its descendants, `ONLY` suppresses that, and UPDATE/DELETE/TRUNCATE fan
/// out the same way — updating each row where it lies, never moving one.
#[tokio::test]
async fn table_inheritance_reads_and_writes_fan_out_to_descendants() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

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
             INSERT INTO emp VALUES ('ee', 20, 100); \
             INSERT INTO student VALUES ('ss', 30, 3.5); \
             INSERT INTO stud_emp VALUES ('se', 40, 200, 4.0, 50)",
        )
        .await?;

    let names = |sql: &'static str| {
        let client = &client;
        async move {
            let msgs = client.simple_query(sql).await?;
            Ok::<_, anyhow::Error>(
                rows(&msgs)
                    .iter()
                    .filter_map(|r| r.get(0).map(str::to_string))
                    .collect::<Vec<_>>(),
            )
        }
    };

    // The parent sees itself plus every descendant, transitively.
    assert_eq!(
        names("SELECT name FROM person ORDER BY name").await?,
        ["ee", "pp", "se", "ss"]
    );
    // An INSERT aimed at the parent stays in the parent — the difference from a
    // partitioned parent, which would route the row away.
    assert_eq!(names("SELECT name FROM ONLY person").await?, ["pp"]);
    // Reading `stud_emp` as a `student` must find `gpa` at ordinal 5, not 3.
    let msgs = client
        .simple_query("SELECT name, gpa FROM student ORDER BY name")
        .await?;
    let read: Vec<_> = rows(&msgs).iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(
        read,
        vec![(Some("se"), Some("4")), (Some("ss"), Some("3.5"))]
    );

    // EXPLAIN renders one scan per relation, parent first.
    let plan = client.simple_query("EXPLAIN SELECT * FROM person").await?;
    let lines: Vec<&str> = rows(&plan).iter().filter_map(|r| r.get(0)).collect();
    assert_eq!(lines[0], "Append");
    assert_eq!(lines[1], "  ->  Seq Scan on person");
    assert_eq!(lines.len(), 5);
    let plan = client
        .simple_query("EXPLAIN SELECT * FROM ONLY person")
        .await?;
    assert_eq!(rows(&plan)[0].get(0), Some("Seq Scan on person"));

    // UPDATE through the parent touches every descendant in place.
    client
        .simple_query("UPDATE person SET age = age + 1")
        .await?;
    let msgs = client
        .simple_query("SELECT age FROM person ORDER BY age")
        .await?;
    let ages: Vec<_> = rows(&msgs).iter().map(|r| r.get(0)).collect();
    assert_eq!(ages, vec![Some("11"), Some("21"), Some("31"), Some("41")]);
    // A wider child's own columns are untouched by an update through the parent.
    let msgs = client
        .simple_query("SELECT salary, gpa, percent FROM stud_emp")
        .await?;
    let extra: Vec<_> = rows(&msgs)[0..1]
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    assert_eq!(extra, vec![(Some("200"), Some("4"), Some("50"))]);

    // `ONLY` confines the write to the parent's own rows.
    client
        .simple_query("UPDATE ONLY person SET age = 99")
        .await?;
    assert_eq!(
        names("SELECT name FROM person WHERE age = 99").await?,
        ["pp"]
    );

    // RETURNING through the parent is in the parent's shape, not a child's.
    let msgs = client
        .simple_query("UPDATE person SET name = name || '!' WHERE age > 30 RETURNING name")
        .await?;
    let mut returned: Vec<_> = rows(&msgs)
        .iter()
        .filter_map(|r| r.get(0).map(str::to_string))
        .collect();
    returned.sort();
    assert_eq!(returned, ["pp!", "se!", "ss!"]);

    // DELETE removes each row from whichever descendant holds it.
    client
        .simple_query("DELETE FROM person WHERE age > 30")
        .await?;
    assert_eq!(names("SELECT name FROM person").await?, ["ee"]);

    // TRUNCATE recurses; `ONLY` does not.
    client
        .simple_query("INSERT INTO person VALUES ('p2', 1)")
        .await?;
    client.simple_query("TRUNCATE ONLY person").await?;
    assert_eq!(names("SELECT name FROM person").await?, ["ee"]);
    client.simple_query("TRUNCATE person").await?;
    let msgs = client.simple_query("SELECT count(*) FROM person").await?;
    assert_eq!(rows(&msgs)[0].get(0), Some("0"));

    Ok(())
}

/// An inheritance child is a RESTRICT-blocking dependent of its parent, and a
/// CASCADE target — unlike a *partition*, which PG drops with its parent
/// silently and without CASCADE.
#[tokio::test]
async fn dropping_an_inheritance_parent_needs_cascade() -> anyhow::Result<()> {
    use tokio_postgres::error::SqlState;

    let client = connect(spawn_server().await).await;
    client
        .simple_query("CREATE TABLE person (name text)")
        .await?;
    client
        .simple_query("CREATE TABLE emp () INHERITS (person)")
        .await?;

    let err = client
        .simple_query("DROP TABLE person")
        .await
        .expect_err("RESTRICT must refuse a parent with children");
    let db = err.as_db_error().expect("database error");
    assert_eq!(db.code(), &SqlState::DEPENDENT_OBJECTS_STILL_EXIST);
    assert_eq!(
        db.message(),
        "cannot drop table person because other objects depend on it"
    );
    assert_eq!(db.detail(), Some("table emp depends on table person"));

    // Dropping the child alone needs nothing: the link lives only on the child.
    client.simple_query("DROP TABLE emp").await?;
    client.simple_query("DROP TABLE person").await?;

    // CASCADE takes the children with it, transitively.
    client.simple_query("CREATE TABLE a (x int4)").await?;
    client
        .simple_query("CREATE TABLE b () INHERITS (a)")
        .await?;
    client
        .simple_query("CREATE TABLE c () INHERITS (b)")
        .await?;
    client.simple_query("DROP TABLE a CASCADE").await?;
    let msgs = client
        .simple_query("SELECT count(*) FROM pg_class WHERE relname IN ('a', 'b', 'c')")
        .await?;
    assert_eq!(rows(&msgs)[0].get(0), Some("0"));

    Ok(())
}

/// Every row a simple query returned, first column only. Unlike [`scalar`],
/// this keeps the results of *every* statement in a multi-statement message,
/// which is exactly what the stamping tests need to compare.
async fn column(client: &tokio_postgres::Client, sql: &str) -> Vec<String> {
    client
        .simple_query(sql)
        .await
        .unwrap_or_else(|e| panic!("`{sql}` should succeed: {e}"))
        .iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(row) => row.get(0).map(str::to_string),
            _ => None,
        })
        .collect()
}

/// `now()` is fixed for the whole transaction, `statement_timestamp()` for the
/// whole message, and `clock_timestamp()` for nothing.
///
/// The three are asserted by their ordering rather than by elapsed time: the
/// engine has no `pg_sleep`, and a test that raced the clock would be flaky in
/// whichever direction the machine was slow.
#[tokio::test]
async fn the_clock_functions_are_stable_at_three_different_scopes() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("SET TIME ZONE 'UTC'").await?;

    client.simple_query("BEGIN").await?;
    let first = scalar(&client, "SELECT now()").await;
    // Two intervening round trips, so a per-message or per-statement stamp
    // would have moved by now.
    client.simple_query("SELECT 1").await?;
    client.simple_query("SELECT 1").await?;
    assert_eq!(scalar(&client, "SELECT now()").await, first);
    assert_eq!(
        scalar(&client, "SELECT now() = transaction_timestamp()").await,
        "t"
    );
    // Inside a block the statement is later than the transaction, and the wall
    // clock is later than both.
    assert_eq!(
        scalar(&client, "SELECT now() < statement_timestamp()").await,
        "t"
    );
    assert_eq!(
        scalar(
            &client,
            "SELECT statement_timestamp() <= clock_timestamp()
                AND clock_timestamp() <= clock_timestamp()"
        )
        .await,
        "t"
    );
    client.simple_query("COMMIT").await?;

    // Under autocommit each statement is its own transaction, so the two
    // coincide — and successive statements move.
    assert_eq!(
        scalar(&client, "SELECT now() = statement_timestamp()").await,
        "t"
    );
    let a = scalar(&client, "SELECT now()").await;
    let b = scalar(&client, "SELECT now()").await;
    assert_ne!(a, b, "autocommit statements should not share a transaction");
    Ok(())
}

/// `pg_postmaster_start_time()` is fixed for the life of the process — a
/// stronger scope than any of the three clock functions above — and it precedes
/// everything a session can observe.
#[tokio::test]
async fn the_postmaster_start_time_is_fixed_and_precedes_the_session() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("SET TIME ZONE 'UTC'").await?;

    let first = scalar(&client, "SELECT pg_postmaster_start_time()").await;
    // Unlike `now()`, a second autocommit statement — a new transaction — must
    // still report the same instant.
    assert_eq!(
        scalar(&client, "SELECT pg_postmaster_start_time()").await,
        first
    );
    assert_eq!(
        scalar(
            &client,
            "SELECT pg_postmaster_start_time() <= clock_timestamp()"
        )
        .await,
        "t"
    );
    // It is a timestamptz, so it renders in the session zone like one.
    client.simple_query("SET TIME ZONE 'Asia/Tokyo'").await?;
    assert_ne!(
        scalar(&client, "SELECT pg_postmaster_start_time()").await,
        first,
        "a timestamptz must re-render in the new session zone"
    );
    Ok(())
}

/// `version()` and the two version GUCs are one fact with three spellings, and
/// clients cross-check them: a driver reads `server_version` from the startup
/// packet, psql branches on `server_version_num`, and `version()` is what a user
/// pastes into a bug report. They must agree.
#[tokio::test]
async fn the_version_surfaces_agree() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    let version = scalar(&client, "SELECT version()").await;
    let server_version = scalar(&client, "SHOW server_version").await;
    assert!(
        version.starts_with(&format!("PostgreSQL {server_version} on ")),
        "version() must carry server_version ({server_version}) verbatim: got {version}"
    );
    assert!(version.ends_with("-bit"), "got {version}");

    // `server_version_num` encodes the same version as an integer.
    assert_eq!(scalar(&client, "SHOW server_version_num").await, "190000");
    assert_eq!(
        scalar(&client, "SELECT current_setting('server_version_num')").await,
        "190000"
    );
    assert!(
        server_version.starts_with("19.0"),
        "server_version_num 190000 must match {server_version}"
    );

    // Both are read-only, as in PG.
    let err = client
        .simple_query("SET server_version_num = '1'")
        .await
        .expect_err("server_version_num is read-only");
    let db = err.as_db_error().expect("database error");
    assert_eq!(
        db.code(),
        &tokio_postgres::error::SqlState::CANT_CHANGE_RUNTIME_PARAM
    );
    assert_eq!(
        db.message(),
        "parameter \"server_version_num\" cannot be changed"
    );
    Ok(())
}

/// A statement timestamp belongs to the *message*, not the statement: PG stamps
/// it once per protocol message, so every statement of a multi-statement simple
/// query reports the same one — and, since the batch is one implicit
/// transaction, the same `now()` too.
#[tokio::test]
async fn a_multi_statement_query_shares_one_stamp() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("SET TIME ZONE 'UTC'").await?;

    let stamps = column(
        &client,
        "SELECT statement_timestamp(); SELECT statement_timestamp(); SELECT statement_timestamp()",
    )
    .await;
    assert_eq!(stamps.len(), 3);
    assert!(
        stamps.windows(2).all(|w| w[0] == w[1]),
        "one message should be one statement timestamp: {stamps:?}"
    );

    let nows = column(&client, "SELECT now(); SELECT now()").await;
    assert_eq!(nows[0], nows[1], "one message is one implicit transaction");
    Ok(())
}

/// The extended protocol restamps per `Execute`, so a prepared statement run
/// twice inside a block sees one `now()` and two statement timestamps.
#[tokio::test]
async fn the_extended_protocol_restamps_each_execute() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("SET TIME ZONE 'UTC'").await?;
    // `timestamptz` has no binary output routine yet, so ask for text.
    let stmt = client
        .prepare("SELECT now()::text, statement_timestamp()::text")
        .await?;

    client.simple_query("BEGIN").await?;
    let first = client.query_one(&stmt, &[]).await?;
    let second = client.query_one(&stmt, &[]).await?;
    client.simple_query("COMMIT").await?;

    let (now1, stmt1): (String, String) = (first.get(0), first.get(1));
    let (now2, stmt2): (String, String) = (second.get(0), second.get(1));
    assert_eq!(now1, now2, "the block's transaction timestamp is frozen");
    assert_ne!(stmt1, stmt2, "each Execute is its own statement");
    Ok(())
}

/// `'now'` is the *transaction* timestamp, to the microsecond — not the
/// statement's, and not a fresh reading. Everything relative is measured from
/// the same instant, so the date specials line up with it.
#[tokio::test]
async fn the_relative_literals_read_the_transaction_clock() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("SET TIME ZONE 'UTC'").await?;

    client.simple_query("BEGIN").await?;
    // An intervening statement, so the statement stamp has moved past the
    // transaction's and the two can be told apart.
    client.simple_query("SELECT 1").await?;
    for (sql, want) in [
        ("SELECT 'now'::timestamptz = now()", "t"),
        ("SELECT 'now'::timestamptz <> statement_timestamp()", "t"),
        ("SELECT 'now'::timestamp = now()::timestamp", "t"),
        ("SELECT 'now'::date = current_date", "t"),
        ("SELECT 'now'::time = localtime", "t"),
        ("SELECT 'now'::timetz = current_time", "t"),
        // The date tokens are fields, so they combine with a time.
        ("SELECT 'tomorrow'::date - 'today'::date", "1"),
        ("SELECT 'today'::date - 'yesterday'::date", "1"),
        (
            "SELECT 'today 10:00'::timestamp - 'today'::timestamp",
            "10:00:00",
        ),
        (
            "SELECT '10:00 today'::timestamp = 'today 10:00'::timestamp",
            "t",
        ),
        ("SELECT 'today'::timestamp = current_date::timestamp", "t"),
        // `allballs` is midnight, and at a literal `+00` whatever the session
        // zone — unlike `now`, which takes the session's offset.
        ("SELECT 'allballs'::time", "00:00:00"),
        ("SELECT 'allballs'::timetz", "00:00:00+00"),
    ] {
        assert_eq!(scalar(&client, sql).await, want, "for `{sql}`");
    }
    client.simple_query("COMMIT").await?;
    Ok(())
}

/// The specials each type does *not* accept, and the two shapes `now` refuses.
#[tokio::test]
async fn the_relative_literals_reject_what_pg_rejects() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    for (sql, want) in [
        (
            "SELECT 'today'::time",
            "invalid input syntax for type time: \"today\"",
        ),
        (
            "SELECT 'epoch'::time",
            "invalid input syntax for type time: \"epoch\"",
        ),
        (
            "SELECT 'allballs'::date",
            "invalid input syntax for type date: \"allballs\"",
        ),
        (
            "SELECT 'now 10:00'::timestamp",
            "invalid input syntax for type timestamp: \"now 10:00\"",
        ),
        (
            "SELECT 'today today'::timestamp",
            "invalid input syntax for type timestamp: \"today today\"",
        ),
        (
            "SELECT '2020-01-01 today'::timestamp",
            "invalid input syntax for type timestamp: \"2020-01-01 today\"",
        ),
    ] {
        let e = client
            .simple_query(sql)
            .await
            .expect_err(sql)
            .as_db_error()
            .expect("a database error")
            .message()
            .to_string();
        assert_eq!(e, want, "for `{sql}`");
    }
    Ok(())
}

/// The `CURRENT_TIMESTAMP` family is exactly a set of casts of `now()`, and its
/// `(p)` is a grammar slot: an unsigned integer literal or nothing. Both halves
/// are pinned against PostgreSQL 18.4.
#[tokio::test]
async fn the_keyword_datetime_forms_are_casts_of_now() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    for zone in ["UTC", "America/New_York", "Asia/Kolkata"] {
        client
            .simple_query(&format!("SET TIME ZONE '{zone}'"))
            .await?;
        for sql in [
            "SELECT current_timestamp = now()",
            "SELECT current_date = now()::date",
            "SELECT localtimestamp = now()::timestamp",
            "SELECT current_time = now()::timetz",
            "SELECT localtime = now()::time",
            "SELECT current_timestamp(2) = now()::timestamptz(2)",
        ] {
            assert_eq!(scalar(&client, sql).await, "t", "for `{sql}` in {zone}");
        }
    }

    client.simple_query("SET TIME ZONE 'UTC'").await?;
    // A modifier above 6 clamps, silently — the divergence
    // `expr::datetime_precision` documents (PG also warns).
    assert_eq!(
        scalar(&client, "SELECT current_timestamp(7) = current_timestamp").await,
        "t"
    );
    // Everything the grammar rejects, reported at the token PG blames.
    for (sql, want) in [
        ("SELECT current_date(0)", "syntax error at or near \"(\""),
        (
            "SELECT current_timestamp(-1)",
            "syntax error at or near \"-\"",
        ),
        (
            "SELECT current_timestamp(1+1)",
            "syntax error at or near \"+\"",
        ),
        (
            "SELECT current_time(3::int)",
            "syntax error at or near \"::\"",
        ),
        // `now` *is* a function, so a wrong-arity call is an overload failure.
        ("SELECT now(0)", "function now(integer) does not exist"),
    ] {
        let e = client
            .simple_query(sql)
            .await
            .expect_err(sql)
            .as_db_error()
            .expect("a database error")
            .message()
            .to_string();
        assert_eq!(e, want, "for `{sql}`");
    }
    Ok(())
}

/// PostgreSQL evaluates a *literal* default when the DDL runs and stores the
/// value, so `DEFAULT 'now'` is the moment of `CREATE TABLE`; `DEFAULT now()`
/// stays a live call. The manual draws exactly this distinction, and it is the
/// difference between a column that records when the table was made and one
/// that records when the row was.
#[tokio::test]
async fn relative_column_defaults_freeze_but_function_defaults_do_not() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("SET TIME ZONE 'UTC'").await?;
    client
        .simple_query(
            "CREATE TABLE d (
                 frozen_ts    timestamp   DEFAULT 'now',
                 frozen_date  date        DEFAULT 'tomorrow',
                 live         timestamptz DEFAULT now(),
                 keyword      timestamptz DEFAULT current_timestamp(3),
                 keyword_date date        DEFAULT current_date)",
        )
        .await?;

    // What was stored: a resolved literal for the relative ones, the call
    // itself for the rest — and the keyword forms print back as keywords.
    let stored = column(
        &client,
        "SELECT pg_get_expr(ad.adbin, ad.adrelid)
           FROM pg_attrdef ad JOIN pg_attribute a
             ON a.attrelid = ad.adrelid AND a.attnum = ad.adnum
          WHERE ad.adrelid = 'd'::regclass
          ORDER BY a.attnum",
    )
    .await;
    assert!(
        stored[0].ends_with("::timestamp without time zone") && !stored[0].contains("now"),
        "DEFAULT 'now' should have frozen into a literal, got {:?}",
        stored[0]
    );
    assert!(
        stored[1].ends_with("::date") && !stored[1].contains("tomorrow"),
        "DEFAULT 'tomorrow' should have frozen into a literal, got {:?}",
        stored[1]
    );
    assert_eq!(stored[2], "now()");
    assert_eq!(stored[3], "CURRENT_TIMESTAMP(3)");
    assert_eq!(stored[4], "CURRENT_DATE");

    // And what they do: the frozen one repeats, the live one does not.
    client.simple_query("INSERT INTO d DEFAULT VALUES").await?;
    client.simple_query("INSERT INTO d DEFAULT VALUES").await?;
    assert_eq!(
        scalar(&client, "SELECT count(DISTINCT frozen_ts) FROM d").await,
        "1"
    );
    assert_eq!(
        scalar(&client, "SELECT count(DISTINCT live) FROM d").await,
        "2"
    );
    assert_eq!(
        scalar(
            &client,
            "SELECT frozen_date = current_date + 1 FROM d LIMIT 1"
        )
        .await,
        "t"
    );
    Ok(())
}

/// `current_date` and the date specials are read in the session zone, so a
/// `SET TIME ZONE` across the date line moves them together with `now()::date`.
#[tokio::test]
async fn the_session_zone_decides_which_day_today_is() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    // Two zones 18.5 hours apart: at some instant every day they disagree about
    // the date, and at every instant they agree with their own `now()`.
    for zone in ["Pacific/Midway", "Pacific/Kiritimati"] {
        client
            .simple_query(&format!("SET TIME ZONE '{zone}'"))
            .await?;
        for sql in [
            "SELECT current_date = now()::date",
            "SELECT 'today'::date = current_date",
            "SELECT 'today'::timestamptz = current_date::timestamptz",
        ] {
            assert_eq!(scalar(&client, sql).await, "t", "for `{sql}` in {zone}");
        }
    }
    Ok(())
}

/// **Known divergence from PostgreSQL**, the `'now'` half of the one above.
///
/// PG resolves `'now'` during parse analysis, so a prepared statement freezes
/// it and every `EXECUTE` — in whatever later transaction — returns the same
/// instant. Probed against PG 18.4: two executes 0.3 s apart gave one frozen
/// `'now'` and two different `now()`s. Here both move, because the plan is
/// re-bound per Execute. See `fold_needs_session` for why closing this needs a
/// plan cache rather than a session-aware binder.
#[tokio::test]
async fn a_prepared_now_literal_is_not_frozen_as_pg_freezes_it() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("SET TIME ZONE 'UTC'").await?;
    let stmt = client
        .prepare("SELECT ('now'::timestamptz)::text, now()::text")
        .await?;

    let first = client.query_one(&stmt, &[]).await?;
    let second = client.query_one(&stmt, &[]).await?;
    let (lit1, now1): (String, String) = (first.get(0), first.get(1));
    let (lit2, now2): (String, String) = (second.get(0), second.get(1));

    // Each execute is its own autocommit transaction, so `now()` moves — as it
    // does in PG.
    assert_ne!(now1, now2);
    // PG would hold `lit1 == lit2` here. We track `now()` instead.
    assert_eq!(lit1, now1);
    assert_eq!(lit2, now2);
    assert_ne!(lit1, lit2);
    Ok(())
}

/// A relative literal reaches the clock through *every* route to a date/time
/// type, not just the unknown-literal one.
///
/// `'now'::text::timestamp` arrives at `coerce_expr` as an already-typed `text`
/// constant, which the binder folds at bind time — where there is no session.
/// The fold has to defer instead, the way `resolve_unknown` does for a bare
/// literal; before it did, this reported the internal "no transaction clock"
/// error to the client.
#[tokio::test]
async fn a_relative_literal_resolves_through_an_explicit_text_cast() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("SET TIME ZONE 'UTC'").await?;

    for sql in [
        "SELECT ('now'::text::timestamp) = now()::timestamp",
        "SELECT ('today'::text::date) = current_date",
        "SELECT ('now'::varchar::time) = localtime",
        "SELECT ('now'::text::timestamptz) = now()",
        // The array element route reaches the same input functions.
        "SELECT ('{now}'::timestamp[])[1] = now()::timestamp",
    ] {
        assert_eq!(scalar(&client, sql).await, "t", "for `{sql}`");
    }

    // Deferring must not swallow a real diagnostic: an unparseable literal still
    // reports at bind time, with its own message.
    let e = client
        .simple_query("SELECT 'garbage'::text::timestamp")
        .await
        .expect_err("garbage should not parse")
        .as_db_error()
        .expect("a database error")
        .message()
        .to_string();
    assert_eq!(e, "invalid input syntax for type timestamp: \"garbage\"");
    Ok(())
}

/// The `CURRENT_TIMESTAMP` family is a grammar production, so only the bare
/// unquoted spelling is one.
///
/// The binder sees a lowercased, unquoted name and cannot tell `current_date`
/// from `"current_date"` — so it has to consult the unnormalized name. Before
/// it did, every function whose bare name matched was intercepted: a user's
/// `"localtime"(int)` was unreachable and its argument was reinterpreted as a
/// fractional-second precision.
#[tokio::test]
async fn only_the_keyword_spelling_binds_to_the_clock() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;

    client
        .simple_query(
            "CREATE FUNCTION \"localtime\"(int) RETURNS int LANGUAGE SQL AS 'SELECT $1 + 1'",
        )
        .await?;
    assert_eq!(scalar(&client, "SELECT \"localtime\"(1)").await, "2");

    // A quoted zero-arg call names no function, exactly as in PostgreSQL.
    let e = client
        .simple_query("SELECT \"current_timestamp\"()")
        .await
        .expect_err("quoted spelling is not the keyword")
        .as_db_error()
        .expect("a database error")
        .message()
        .to_string();
    assert_eq!(e, "function current_timestamp() does not exist");

    // ...and the keyword spelling is untouched.
    for sql in [
        "SELECT current_date = now()::date",
        "SELECT current_timestamp(2) = now()::timestamptz(2)",
        "SELECT localtime = now()::time",
    ] {
        assert_eq!(scalar(&client, sql).await, "t", "for `{sql}`");
    }
    Ok(())
}

/// Freezing a literal default follows the *bound* shape, not the written
/// syntax, so every spelling of the same constant behaves alike.
///
/// `DEFAULT 'now'::timestamp` — the spelling PostgreSQL's own manual uses to
/// contrast with `DEFAULT now()` — used to escape the DDL-time resolution
/// because it is a `Cast` node rather than a bare `Value`, and re-read the clock
/// on every insert. An array element escaped for the same reason.
#[tokio::test]
async fn every_spelling_of_a_literal_default_freezes_alike() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("SET TIME ZONE 'UTC'").await?;
    client
        .simple_query(
            "CREATE TABLE f (
                 bare    timestamp     DEFAULT 'now',
                 cast_ts timestamp     DEFAULT 'now'::timestamp,
                 cast_tz timestamptz   DEFAULT 'now'::timestamptz,
                 cast_d  date          DEFAULT 'today'::date,
                 arr     timestamptz[] DEFAULT '{now}',
                 live    timestamptz   DEFAULT now())",
        )
        .await?;

    let stored = column(
        &client,
        "SELECT pg_get_expr(ad.adbin, ad.adrelid)
           FROM pg_attrdef ad JOIN pg_attribute a
             ON a.attrelid = ad.adrelid AND a.attnum = ad.adnum
          WHERE ad.adrelid = 'f'::regclass
          ORDER BY a.attnum",
    )
    .await;
    for (i, want_suffix) in [
        (0, "::timestamp without time zone"),
        (1, "::timestamp without time zone"),
        (2, "::timestamp with time zone"),
        (3, "::date"),
        (4, "::timestamp with time zone[]"),
    ] {
        assert!(
            stored[i].ends_with(want_suffix) && !stored[i].contains("now"),
            "column {i} should have frozen into a literal, got {:?}",
            stored[i]
        );
    }
    assert_eq!(stored[5], "now()");

    // Two inserts a moment apart: everything frozen repeats, `now()` does not.
    client.simple_query("INSERT INTO f DEFAULT VALUES").await?;
    client.simple_query("INSERT INTO f DEFAULT VALUES").await?;
    for col in ["bare", "cast_ts", "cast_tz", "cast_d", "arr"] {
        assert_eq!(
            scalar(&client, &format!("SELECT count(DISTINCT {col}) FROM f")).await,
            "1",
            "`{col}` should be frozen"
        );
    }
    assert_eq!(
        scalar(&client, "SELECT count(DISTINCT live) FROM f").await,
        "2"
    );
    Ok(())
}

/// An array element is read by the element type's own input function, so it
/// needs the same session the scalar would get.
///
/// `parse_unknown` threaded the session context into every date/time arm except
/// the array one, which built a fresh clockless UTC context — so
/// `pg_input_is_valid('{now}','timestamp[]')` answered `f`, and
/// `pg_input_error_info` handed the client the internal `XX000` wiring message
/// as though it were an input error.
#[tokio::test]
async fn soft_input_reads_array_elements_with_the_session() -> anyhow::Result<()> {
    let client = connect(spawn_server().await).await;
    client.simple_query("SET TIME ZONE 'UTC'").await?;

    for sql in [
        "SELECT pg_input_is_valid('{now}','timestamp[]')",
        "SELECT pg_input_is_valid('{today}','date[]')",
        "SELECT pg_input_is_valid('{2024-01-01 12:00}','timestamptz[]')",
    ] {
        assert_eq!(scalar(&client, sql).await, "t", "for `{sql}`");
    }

    // A genuinely bad element still reports as a *value* error, not an internal
    // one — the distinction the soft-input surface exists to make.
    assert_eq!(
        scalar(&client, "SELECT pg_input_is_valid('{bogus}','date[]')").await,
        "f"
    );
    assert_eq!(
        scalar(
            &client,
            "SELECT sql_error_code FROM pg_input_error_info('{bogus}','date[]')"
        )
        .await,
        "22007"
    );
    Ok(())
}

/// Every `DataRow`'s first column, as text, in arrival order.
fn data_row_values(msgs: &[(u8, Vec<u8>)]) -> Vec<String> {
    msgs.iter()
        .filter(|(t, _)| *t == b'D')
        .map(|(_, body)| {
            // int16 column count, then int32 length + bytes per column.
            let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]) as usize;
            String::from_utf8_lossy(&body[6..6 + len]).into_owned()
        })
        .collect()
}

/// An extended-query batch is one implicit transaction block, so every message
/// up to `Sync` shares a transaction timestamp.
///
/// PostgreSQL starts the block at the first Parse/Bind/Execute and ends it at
/// `Sync`; we re-stamped per message, so a pipelining client that stamped a
/// parent row and its children in one round trip saw two different instants.
/// Reachable only over a raw socket: tokio-postgres Syncs once per call.
///
/// This is the *clock* half of an implicit block. The batch is still not atomic
/// — each autocommit statement commits at its own boundary — which is separate
/// work.
#[tokio::test]
async fn an_extended_query_batch_shares_one_transaction_timestamp() -> anyhow::Result<()> {
    let port = spawn_server().await;
    let mut socket = raw_session(port).await;

    // Two Parse/Bind/Execute triples, one Sync. `::text` because `timestamptz`
    // has no binary output routine yet. Statement names are per batch: a second
    // `Parse` over a live named statement is an error.
    let batch = |round: u8| {
        let mut out = Vec::new();
        for n in 0..2u8 {
            let (stmt, portal) = (format!("s{round}{n}"), format!("p{round}{n}"));
            let mut parse = format!("{stmt}\0").into_bytes();
            parse.extend_from_slice(b"SELECT now()::text\0\x00\x00");
            out.extend(frontend_message(b'P', &parse));
            let mut bind = format!("{portal}\0{stmt}\0").into_bytes();
            bind.extend_from_slice(b"\x00\x00\x00\x00\x00\x00");
            out.extend(frontend_message(b'B', &bind));
            let mut exec = format!("{portal}\0").into_bytes();
            exec.extend_from_slice(b"\x00\x00\x00\x00");
            out.extend(frontend_message(b'E', &exec));
        }
        out.extend(frontend_message(b'S', b""));
        out
    };

    socket.write_all(&batch(1)).await?;
    let values = data_row_values(&read_until_ready(&mut socket).await);
    assert_eq!(values.len(), 2, "expected one row per Execute: {values:?}");
    assert_eq!(
        values[0], values[1],
        "one batch is one implicit transaction: {values:?}"
    );

    // The next batch is a new transaction, so it moves.
    socket.write_all(&batch(2)).await?;
    let next = data_row_values(&read_until_ready(&mut socket).await);
    assert_eq!(next.len(), 2);
    assert_ne!(next[0], values[0], "a new batch starts a new transaction");
    Ok(())
}
