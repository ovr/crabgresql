//! `pg_language`: the four languages a routine can be written in.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;
use crate::oids::*;

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

/// Every language this build publishes, as `(oid, lanname, lanispl,
/// lanpltrusted)`.
///
/// 12/13/14 are PostgreSQL's bootstrap OIDs and are stable across versions.
/// `plpgsql`'s is not: PostgreSQL assigns it through `CREATE EXTENSION` at
/// initdb time, so it varies by build and there is nothing to reproduce —
/// clients match on `lanname`.
///
/// A list rather than a literal inside [`pg_language_rows`] because
/// [`crate::catalogs::description`] filters `pg_language.dat`'s descriptions
/// against it.
pub(crate) const BUILTIN_LANGUAGES: &[(u32, &str, bool, bool)] = &[
    (12, "internal", false, false),
    (13, "c", false, false),
    (14, "sql", false, true),
    (PLPGSQL_LANG_OID, "plpgsql", true, true),
];

/// The fixed `pg_language` rows.
///
/// TODO: `lanplcallfoid`/`laninline`/`lanvalidator` are 0. `pg_proc` publishes
/// only the functions the other catalogs reference and no language handler is
/// among them, so pointing these at a row means emitting it first — until then
/// a non-zero value here would dangle.
pub(crate) fn pg_language_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    BUILTIN_LANGUAGES
        .iter()
        .map(|(oid, name, ispl, trusted)| {
            vec![
                Value::Oid(*oid),
                Value::Text((*name).to_string()),
                Value::Oid(BOOTSTRAP_ROLE_OID),
                Value::Bool(*ispl),
                Value::Bool(*trusted),
                Value::Oid(0),
                Value::Oid(0),
                Value::Oid(0),
                Value::Null,
            ]
        })
        .collect()
}
