//! `pg_language`: the four languages a routine can be written in.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;
use crate::oids::*;

/// `pg_catalog.pg_language`.
pub(crate) fn pg_language_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_language",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("lanname", PgType::Name),
            col("lanowner", PgType::Oid),
            col("lanispl", PgType::Bool),
            col("lanpltrusted", PgType::Bool),
            col("lanplcallfoid", PgType::Oid),
            col("laninline", PgType::Oid),
            col("lanvalidator", PgType::Oid),
            col("lanacl", PgType::Text),
        ],
    )
}

/// The fixed `pg_language` rows.
///
/// 12/13/14 are PostgreSQL's bootstrap OIDs and are stable across versions.
/// `plpgsql`'s is not: PostgreSQL assigns it through `CREATE EXTENSION` at
/// initdb time, so it varies by build and there is nothing to reproduce —
/// clients match on `lanname`. The handler OIDs stay 0 until `pg_proc` carries
/// built-in rows for them to point at.
pub(crate) fn pg_language_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let row = |oid: u32, name: &str, ispl: bool, trusted: bool| {
        vec![
            Value::Oid(oid),
            Value::Text(name.to_string()),
            Value::Oid(BOOTSTRAP_ROLE_OID),
            Value::Bool(ispl),
            Value::Bool(trusted),
            Value::Oid(0),
            Value::Oid(0),
            Value::Oid(0),
            Value::Null,
        ]
    };
    vec![
        row(12, "internal", false, false),
        row(13, "c", false, false),
        row(14, "sql", false, true),
        row(PLPGSQL_LANG_OID, "plpgsql", true, true),
    ]
}
