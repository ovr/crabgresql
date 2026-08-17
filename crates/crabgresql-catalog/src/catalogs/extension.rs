//! `pg_extension` and the two `pg_available_extension*` views: what `CREATE
//! EXTENSION` has installed, and what it could install.
//!
//! Exactly one extension, and it is genuinely there: `plpgsql`, whose language
//! this build implements (see [`crate::catalogs::language`]). PostgreSQL ships
//! the same row in every fresh database, created by `initdb` rather than by a
//! user — so `\dx` printing one line here is the same answer PostgreSQL gives,
//! not a stand-in for one.
//!
//! The "available" views are shorter than PostgreSQL's for a real reason rather
//! than a missing feature: those list what is on disk in `SHAREDIR/extension`,
//! and there is no such directory to read. Nothing else can be installed, so
//! nothing else is offered — a client that reads these to decide whether
//! `CREATE EXTENSION postgis` would work gets the correct "no".

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value, oid};

use crate::SystemCatalog;
use crate::cols::*;
use crate::oids::*;

/// The one extension name and description, shared by all three relations so
/// they cannot disagree about it.
const NAME: &str = "plpgsql";
const COMMENT: &str = "PL/pgSQL procedural language";

/// One installable extension version: what `pg_available_extension_versions`
/// publishes about it, minus the `installed` flag that view computes.
///
/// The flags are the ones PostgreSQL 18.4 reports for `plpgsql` 1.0.
pub struct AvailableExtension {
    pub name: &'static str,
    pub version: &'static str,
    pub superuser: bool,
    pub trusted: bool,
    pub relocatable: bool,
    pub schema: &'static str,
    pub comment: &'static str,
}

/// Every installable extension version, for the four readers that must not
/// disagree about them: the two views here, and — across a crate boundary — the
/// set-returning `pg_available_extensions()` and
/// `pg_available_extension_versions()` in the executor, which psql's `\dx` calls
/// instead of the views of those names.
///
/// One row per `(extension, version)`. The one extension here offers exactly one
/// version, so that row is also its *default* version — which is what lets
/// `pg_available_extensions.default_version` read this same list.
pub fn available_extensions() -> &'static [AvailableExtension] {
    &[AvailableExtension {
        name: NAME,
        version: "1.0",
        superuser: true,
        trusted: true,
        relocatable: false,
        schema: "pg_catalog",
        comment: COMMENT,
    }]
}

pub(crate) fn pg_extension_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_extension",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("extname", PgType::Name),
            col("extowner", PgType::Oid),
            col("extnamespace", PgType::Oid),
            col("extrelocatable", PgType::Bool),
            col("extversion", PgType::Text),
            col("extconfig", PgType::Array(oid::OID)),
            col("extcondition", PgType::Array(oid::TEXT)),
        ],
    )
}

/// The `plpgsql` row, column for column as PostgreSQL 18.4 reports it.
pub(crate) fn pg_extension_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    available_extensions()
        .iter()
        .map(|ext| {
            vec![
                Value::Oid(PLPGSQL_EXTENSION_OID),
                Value::Text(ext.name.to_string()),
                Value::Oid(BOOTSTRAP_ROLE_OID),
                Value::Oid(PG_CATALOG_NAMESPACE_OID),
                Value::Bool(ext.relocatable),
                Value::Text(ext.version.to_string()),
                Value::Null,
                Value::Null,
            ]
        })
        .collect()
}

pub(crate) fn pg_available_extensions_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_available_extensions",
        "pg_catalog",
        vec![
            col("name", PgType::Name),
            col("default_version", PgType::Text),
            col("installed_version", PgType::Text),
            col("comment", PgType::Text),
        ],
    )
}

/// `installed_version` equals `default_version` because the one extension is
/// always installed: no version of it is offered and not present.
pub(crate) fn pg_available_extensions_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    available_extensions()
        .iter()
        .map(|ext| {
            vec![
                Value::Text(ext.name.to_string()),
                Value::Text(ext.version.to_string()),
                Value::Text(ext.version.to_string()),
                Value::Text(ext.comment.to_string()),
            ]
        })
        .collect()
}

pub(crate) fn pg_available_extension_versions_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_available_extension_versions",
        "pg_catalog",
        vec![
            col("name", PgType::Name),
            col("version", PgType::Text),
            col("installed", PgType::Bool),
            col("superuser", PgType::Bool),
            col("trusted", PgType::Bool),
            col("relocatable", PgType::Bool),
            col("schema", PgType::Name),
            col("requires", PgType::Array(oid::NAME)),
            col("comment", PgType::Text),
        ],
    )
}

/// `installed` is `true` for every row: the one extension is always installed,
/// so no version of it is offered and not present. `requires` is NULL, as
/// PostgreSQL reports for an extension that depends on nothing.
pub(crate) fn pg_available_extension_versions_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    available_extensions()
        .iter()
        .map(|ext| {
            vec![
                Value::Text(ext.name.to_string()),
                Value::Text(ext.version.to_string()),
                Value::Bool(true),
                Value::Bool(ext.superuser),
                Value::Bool(ext.trusted),
                Value::Bool(ext.relocatable),
                Value::Text(ext.schema.to_string()),
                Value::Null,
                Value::Text(ext.comment.to_string()),
            ]
        })
        .collect()
}
