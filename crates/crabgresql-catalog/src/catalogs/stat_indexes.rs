//! The three per-index statistics views: `pg_stat_{all,sys,user}_indexes`.
//!
//! Split by schema exactly as the per-table views are, and the `sys` variant is
//! empty for the same reason — see [`crate::catalogs::stat_tables`].

use crabgresql_storage_api::TableSchema;
use crabgresql_storage_api::pgstat::IndexStatSnapshot;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;

/// The 9 columns of `pg_stat_all_indexes`, under `name`.
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
            col("idx_scan", PgType::Int8),
            col("last_idx_scan", PgType::TimestampTz),
            col("idx_tup_read", PgType::Int8),
            col("idx_tup_fetch", PgType::Int8),
        ],
    )
}

pub(crate) fn pg_stat_all_indexes_schema() -> TableSchema {
    all_indexes_schema("pg_stat_all_indexes")
}

pub(crate) fn pg_stat_sys_indexes_schema() -> TableSchema {
    all_indexes_schema("pg_stat_sys_indexes")
}

pub(crate) fn pg_stat_user_indexes_schema() -> TableSchema {
    all_indexes_schema("pg_stat_user_indexes")
}

fn index_rows(cat: &SystemCatalog, want_system: Option<bool>) -> Vec<Vec<Value>> {
    cat.index_stats()
        .iter()
        .filter(|stats| {
            want_system
                .is_none_or(|sys| sys == super::stat_tables::is_system_namespace(&stats.namespace))
        })
        .filter_map(|stats| index_row(cat, stats))
        .collect()
}

/// One row per index with counters. Dropped when either OID is missing: an
/// index this snapshot cannot number is one that has been dropped, and its
/// statistics go with it.
fn index_row(cat: &SystemCatalog, stats: &IndexStatSnapshot) -> Option<Vec<Value>> {
    let relid = cat.relation_oid_in(&stats.namespace, &stats.relation)?;
    let indexrelid = cat.index_oid_in(&stats.namespace, &stats.relation, &stats.index)?;
    Some(vec![
        Value::Oid(relid),
        Value::Oid(indexrelid),
        Value::Text(stats.namespace.clone()),
        Value::Text(stats.relation.clone()),
        Value::Text(stats.index.clone()),
        counter(stats.idx_scan),
        stamp_or_null(stats.last_idx_scan),
        counter(stats.idx_tup_read),
        counter(stats.idx_tup_fetch),
    ])
}

pub(crate) fn pg_stat_all_indexes_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    index_rows(cat, None)
}

pub(crate) fn pg_stat_sys_indexes_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    index_rows(cat, Some(true))
}

pub(crate) fn pg_stat_user_indexes_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    index_rows(cat, Some(false))
}
