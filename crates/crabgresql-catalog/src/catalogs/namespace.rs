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

/// Shared by row publication and name/OID resolution so neither can expose a
/// schema the other does not recognize.
///
/// `information_schema` gets its OID from `initdb`; the other three have stable
/// `.dat` assignments.
pub(crate) const BUILTIN_NAMESPACES: &[(u32, &str)] = &[
    (PG_CATALOG_NAMESPACE_OID, "pg_catalog"),
    (TOAST_NAMESPACE_OID, crate::TOAST_NAMESPACE),
    (PUBLIC_NAMESPACE_OID, "public"),
    (INFORMATION_SCHEMA_NAMESPACE_OID, "information_schema"),
];

/// The schemas this build publishes without anyone having run `CREATE SCHEMA` —
/// the set a duplicate-name check has to consider beyond the engine's own.
pub fn is_builtin_namespace(name: &str) -> bool {
    BUILTIN_NAMESPACES.iter().any(|(_, n)| *n == name)
}

/// A schema the database system requires. `public` is excluded because
/// PostgreSQL lets its owner drop it; the rest it refuses with `cannot drop
/// schema … because it is required by the database system`.
pub fn is_system_namespace(name: &str) -> bool {
    is_builtin_namespace(name) && name != "public"
}

/// All schemas report the bootstrap owner because this build exposes a single
/// owning role.
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
