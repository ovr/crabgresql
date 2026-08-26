//! `pg_namespace`: the built-in schemas plus every `CREATE SCHEMA`.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;
use crate::oids::*;

/// `pg_catalog.pg_namespace`.
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

/// The reserved schemas this build publishes, as `(oid, nspname)`. Three of the
/// OIDs match PostgreSQL's stable `.dat` assignments (`pg_catalog` = 11,
/// `pg_toast` = 99, `public` = 2200); `information_schema` comes from `initdb`
/// instead — see [`INFORMATION_SCHEMA_NAMESPACE_OID`].
///
/// A list rather than a literal inside [`pg_namespace_rows`] because two other
/// readers need the same set: [`crate::SystemCatalog::namespace_oids`] resolves
/// names against it, and [`crate::catalogs::description`] filters
/// `pg_namespace.dat`'s descriptions against it — that file also describes the
/// subscription conflict-log schema, which this build does not have (and says
/// nothing about `information_schema`, which PostgreSQL leaves uncommented).
pub(crate) const BUILTIN_NAMESPACES: &[(u32, &str)] = &[
    (PG_CATALOG_NAMESPACE_OID, "pg_catalog"),
    (TOAST_NAMESPACE_OID, crate::TOAST_NAMESPACE),
    (PUBLIC_NAMESPACE_OID, "public"),
    (INFORMATION_SCHEMA_NAMESPACE_OID, "information_schema"),
];

/// The reserved schemas, then every schema `CREATE SCHEMA` made. Owners are the
/// bootstrap superuser — see `BOOTSTRAP_ROLE_OID` for why there is only the one.
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
    let mut rows: Vec<Vec<Value>> = BUILTIN_NAMESPACES
        .iter()
        .map(|(oid, name)| row(*oid, name))
        .collect();
    for (name, oid) in user_schemas {
        rows.push(row(*oid, name));
    }
    rows
}
