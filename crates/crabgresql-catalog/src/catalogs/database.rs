//! `pg_database` and `pg_tablespace`: the cluster-wide singletons.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;
use crate::oids::*;

/// `pg_catalog.pg_database` — one row, for the database this session is
/// connected to. PostgreSQL lists every database in the cluster; a crabgresql
/// server serves exactly one, so the connected database *is* the relation.
///
/// `datacl` is NULL: no `GRANT` exists to populate it, which is also what
/// PostgreSQL reports for a database whose privileges were never changed from
/// the owner's defaults. See [`crate::cols::ACLITEM_ARRAY`] for the type.
pub(crate) fn pg_database_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_database",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("datname", PgType::Name),
            col("datdba", PgType::Oid),
            col("encoding", PgType::Int4),
            col("datlocprovider", CHARLIKE),
            col("datistemplate", PgType::Bool),
            col("datallowconn", PgType::Bool),
            col("dathasloginevt", PgType::Bool),
            col("datconnlimit", PgType::Int4),
            col("datfrozenxid", PgType::Xid),
            col("datminmxid", PgType::Xid),
            col("dattablespace", PgType::Oid),
            col("datcollate", PgType::Text),
            col("datctype", PgType::Text),
            col("datlocale", PgType::Text),
            col("daticurules", PgType::Text),
            col("datcollversion", PgType::Text),
            // aclitem[]; represented as text and always NULL (default ACL) here.
            col("datacl", ACLITEM_ARRAY),
        ],
    )
}

/// The single `pg_database` row.
///
/// `encoding` is 6 (`UTF8`) because that is the only encoding the server
/// advertises (`server_encoding`), and the locale columns report `C`: the
/// default collation compares bytewise, and `datcollate`/`datctype` must name
/// the collation a `CREATE TABLE` with no `COLLATE` clause actually gets.
/// `datfrozenxid`/`datminmxid` report 1, PostgreSQL's `BootstrapTransactionId`
/// and below every XID this build hands out — it never advances a per-database
/// freeze horizon.
pub(crate) fn pg_database_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let database = cat.database();
    vec![vec![
        Value::Oid(DATABASE_OID),
        Value::Text(database.to_string()),
        Value::Oid(BOOTSTRAP_ROLE_OID),
        Value::Int4(6),
        // 'c' — the libc locale provider, which is what a bytewise default is.
        chr('c'),
        Value::Bool(false),
        Value::Bool(true),
        Value::Bool(false),
        Value::Int4(-1),
        Value::Xid(1),
        Value::Xid(1),
        Value::Oid(DEFAULT_TABLESPACE_OID),
        Value::Text("C".to_string()),
        Value::Text("C".to_string()),
        Value::Null,
        Value::Null,
        Value::Null,
        // datacl
        Value::Null,
    ]]
}

/// `pg_catalog.pg_tablespace` — the two bootstrap tablespaces, as in
/// PostgreSQL. `spcacl` is NULL for the reason given on [`pg_database_schema`].
pub(crate) fn pg_tablespace_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_tablespace",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("spcname", PgType::Name),
            col("spcowner", PgType::Oid),
            // aclitem[]; represented as text and always NULL (default ACL) here.
            col("spcacl", ACLITEM_ARRAY),
            col("spcoptions", PgType::Array(crabgresql_types::oid::TEXT)),
        ],
    )
}

pub(crate) fn pg_tablespace_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let row = |oid: u32, name: &str| {
        vec![
            Value::Oid(oid),
            Value::Text(name.to_string()),
            Value::Oid(BOOTSTRAP_ROLE_OID),
            // spcacl / spcoptions
            Value::Null,
            Value::Null,
        ]
    };
    vec![
        row(DEFAULT_TABLESPACE_OID, "pg_default"),
        row(GLOBAL_TABLESPACE_OID, "pg_global"),
    ]
}
