//! The nine `pg_statio_*` views: block-level I/O per table, per index and per
//! sequence, each split three ways by schema.
//!
//! **Every block count here is zero, and that is a "not measured", not a
//! "measured zero".** The engine's buffer pool does count the pins it serves
//! (hits, misses and extends — see `crabgresql-pg-engine`'s `BufferPool`), and
//! `pg_stat_database.blks_hit`/`blks_read` publish those totals, which is sound
//! because this build serves exactly one database. What the pool cannot do is
//! say *which relation* a pin belonged to: a pin names a `RelFileNode`, the
//! physical file, and nothing maps that back to the catalog relation a
//! statistics view is keyed by. Attributing the totals to relations would take
//! a per-relfilenode table on the pin path — real work, and a real cost on the
//! hottest path in the engine.
//!
//! So the relations are served with their full PostgreSQL shape and rows of
//! zeros: a monitoring query that joins them binds and runs, and a client that
//! reads a hit ratio out of them gets 0/0 rather than a number that was made
//! up. The `heap_blks_*` columns are the ones to fill first if this is ever
//! attributed.
//!
//! The `sys` variants are empty for the reason
//! [`crate::catalogs::stat_tables`] gives.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::catalogs::stat_tables::is_system_namespace;
use crate::cols::*;
use crate::source::RelKind;

/// The 11 columns of `pg_statio_all_tables`, under `name`.
fn all_tables_schema(name: &str) -> TableSchema {
    TableSchema::in_namespace(
        name,
        "pg_catalog",
        vec![
            col("relid", PgType::Oid),
            col("schemaname", PgType::Name),
            col("relname", PgType::Name),
            col("heap_blks_read", PgType::Int8),
            col("heap_blks_hit", PgType::Int8),
            col("idx_blks_read", PgType::Int8),
            col("idx_blks_hit", PgType::Int8),
            col("toast_blks_read", PgType::Int8),
            col("toast_blks_hit", PgType::Int8),
            col("tidx_blks_read", PgType::Int8),
            col("tidx_blks_hit", PgType::Int8),
        ],
    )
}

/// The 7 columns of `pg_statio_all_indexes`, under `name`.
fn all_indexes_schema(name: &str) -> TableSchema {
    TableSchema::in_namespace(
        name,
        "pg_catalog",
        vec![
            col("relid", PgType::Oid),
            col("indexrelid", PgType::Oid),
            col("schemaname", PgType::Name),
            col("relname", PgType::Name),
            col("indexrelname", PgType::Name),
            col("idx_blks_read", PgType::Int8),
            col("idx_blks_hit", PgType::Int8),
        ],
    )
}

/// The 5 columns of `pg_statio_all_sequences`, under `name`.
fn all_sequences_schema(name: &str) -> TableSchema {
    TableSchema::in_namespace(
        name,
        "pg_catalog",
        vec![
            col("relid", PgType::Oid),
            col("schemaname", PgType::Name),
            col("relname", PgType::Name),
            col("blks_read", PgType::Int8),
            col("blks_hit", PgType::Int8),
        ],
    )
}

pub(crate) fn pg_statio_all_tables_schema() -> TableSchema {
    all_tables_schema("pg_statio_all_tables")
}

pub(crate) fn pg_statio_sys_tables_schema() -> TableSchema {
    all_tables_schema("pg_statio_sys_tables")
}

pub(crate) fn pg_statio_user_tables_schema() -> TableSchema {
    all_tables_schema("pg_statio_user_tables")
}

pub(crate) fn pg_statio_all_indexes_schema() -> TableSchema {
    all_indexes_schema("pg_statio_all_indexes")
}

pub(crate) fn pg_statio_sys_indexes_schema() -> TableSchema {
    all_indexes_schema("pg_statio_sys_indexes")
}

pub(crate) fn pg_statio_user_indexes_schema() -> TableSchema {
    all_indexes_schema("pg_statio_user_indexes")
}

pub(crate) fn pg_statio_all_sequences_schema() -> TableSchema {
    all_sequences_schema("pg_statio_all_sequences")
}

pub(crate) fn pg_statio_sys_sequences_schema() -> TableSchema {
    all_sequences_schema("pg_statio_sys_sequences")
}

pub(crate) fn pg_statio_user_sequences_schema() -> TableSchema {
    all_sequences_schema("pg_statio_user_sequences")
}

/// The relations a `pg_statio_*_tables` view lists: everything with heap
/// storage, which is PostgreSQL's rule (`relkind` in `r`, `t`, `m`, `p`) minus
/// the kinds this build has none of. One row each, all counts zero.
fn table_rows(cat: &SystemCatalog, want_system: Option<bool>) -> Vec<Vec<Value>> {
    storage_relations(cat, want_system, RelKind::Sequence, false)
        .map(|(oid, namespace, name)| {
            vec![
                Value::Oid(oid),
                Value::Text(namespace),
                Value::Text(name),
                Value::Int8(0),
                Value::Int8(0),
                Value::Int8(0),
                Value::Int8(0),
                Value::Int8(0),
                Value::Int8(0),
                Value::Int8(0),
                Value::Int8(0),
            ]
        })
        .collect()
}

/// The sequences a `pg_statio_*_sequences` view lists.
fn sequence_rows(cat: &SystemCatalog, want_system: Option<bool>) -> Vec<Vec<Value>> {
    storage_relations(cat, want_system, RelKind::Sequence, true)
        .map(|(oid, namespace, name)| {
            vec![
                Value::Oid(oid),
                Value::Text(namespace),
                Value::Text(name),
                Value::Int8(0),
                Value::Int8(0),
            ]
        })
        .collect()
}

/// Every relation of this snapshot whose kind either is or is not `kind`
/// (`matching` says which), filtered by the schema split, as
/// `(oid, namespace, name)`.
///
/// A view has no storage, so it appears in neither list — the one rule shared
/// by all nine views, and the reason this is one helper rather than two.
fn storage_relations<'a>(
    cat: &'a SystemCatalog,
    want_system: Option<bool>,
    kind: RelKind,
    matching: bool,
) -> impl Iterator<Item = (u32, String, String)> + 'a {
    cat.relation_oids()
        .iter()
        .zip(cat.relation_kinds())
        .filter(move |(_, relkind)| **relkind != RelKind::View)
        .filter(move |(_, relkind)| (**relkind == kind) == matching)
        .filter(move |((_, schema), _)| {
            want_system.is_none_or(|sys| sys == is_system_namespace(&schema.namespace))
        })
        .map(|((oid, schema), _)| (*oid, schema.namespace.clone(), schema.name.clone()))
}

/// The indexes a `pg_statio_*_indexes` view lists: every index of every
/// relation, all counts zero.
fn index_rows(cat: &SystemCatalog, want_system: Option<bool>) -> Vec<Vec<Value>> {
    cat.index_oids()
        .iter()
        .filter(|index| {
            want_system.is_none_or(|sys| sys == is_system_namespace(&index.table_schema.namespace))
        })
        .map(|index| {
            vec![
                Value::Oid(index.table_oid),
                Value::Oid(index.oid),
                Value::Text(index.table_schema.namespace.clone()),
                Value::Text(index.table_schema.name.clone()),
                Value::Text(index.metadata.name.clone()),
                Value::Int8(0),
                Value::Int8(0),
            ]
        })
        .collect()
}

pub(crate) fn pg_statio_all_tables_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    table_rows(cat, None)
}

pub(crate) fn pg_statio_sys_tables_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    table_rows(cat, Some(true))
}

pub(crate) fn pg_statio_user_tables_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    table_rows(cat, Some(false))
}

pub(crate) fn pg_statio_all_indexes_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    index_rows(cat, None)
}

pub(crate) fn pg_statio_sys_indexes_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    index_rows(cat, Some(true))
}

pub(crate) fn pg_statio_user_indexes_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    index_rows(cat, Some(false))
}

pub(crate) fn pg_statio_all_sequences_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    sequence_rows(cat, None)
}

pub(crate) fn pg_statio_sys_sequences_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    sequence_rows(cat, Some(true))
}

pub(crate) fn pg_statio_user_sequences_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    sequence_rows(cat, Some(false))
}
