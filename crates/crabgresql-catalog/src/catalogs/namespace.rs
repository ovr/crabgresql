//! `pg_namespace`: the built-in schemas plus every `CREATE SCHEMA`.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;
use crate::oids::*;

/// `pg_catalog.pg_namespace` — the schemas visible on a fresh cluster.
pub(crate) fn pg_namespace_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_namespace",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("nspname", PgType::Name),
            col("nspowner", PgType::Oid),
            // aclitem[]; represented as text and always NULL (default ACL) here.
            col("nspacl", PgType::Text),
        ],
    )
}

/// Fixed `pg_namespace` rows: the reserved catalog/toast schemas plus `public`.
/// OIDs match PostgreSQL's stable assignments (`pg_catalog` = 11, `pg_toast` =
/// 99, `public` = 2200). `information_schema` has an initdb-assigned OID, so
/// it remains absent here; its named discovery surface lives in
/// `information_schema.schemata`. Owners are the bootstrap superuser — see
/// `BOOTSTRAP_ROLE_OID` for why there is only the one.
pub(crate) fn pg_namespace_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let user_schemas = cat.user_schemas();
    let row = |oid: u32, name: &str| {
        vec![
            Value::Oid(oid),
            Value::Text(name.to_string()),
            Value::Oid(BOOTSTRAP_ROLE_OID),
            Value::Null,
        ]
    };
    let mut rows = vec![
        row(11, "pg_catalog"),
        row(99, "pg_toast"),
        row(2200, "public"),
    ];
    for (name, oid) in user_schemas {
        rows.push(row(*oid, name));
    }
    rows
}
