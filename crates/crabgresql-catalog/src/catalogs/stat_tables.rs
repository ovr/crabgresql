//! The six per-table statistics views: `pg_stat_{all,sys,user}_tables` and the
//! `pg_stat_xact_*` trio beside them.
//!
//! PostgreSQL builds all six from one function over the same counters and
//! splits them by schema; so does this. Two things differ, and both are
//! properties of this build rather than of these views:
//!
//! * The `sys` pair is **always empty**. Its rows are the catalog relations,
//!   and catalog relations here are served from Rust rather than reflected into
//!   `pg_class` — they have no storage to scan, no OID assigned by a snapshot,
//!   and nothing counts a read of one. So `all` and `user` hold the same rows,
//!   which is what PostgreSQL itself shows for a database whose only activity
//!   is in `public`.
//! * The `xact` trio reports what the *current transaction* has done, from
//!   counters that are not kept per transaction here — a write is counted once,
//!   when the statement finishes, and never unwound. They are served with their
//!   full shape and no rows rather than with the cumulative numbers under a
//!   name that promises transaction scope: a client that reads
//!   `pg_stat_xact_user_tables` is asking "what has *this* transaction touched",
//!   and answering with the database's lifetime totals would be a wrong answer,
//!   where no rows is a true "nothing is being tracked at that scope".

use crabgresql_storage_api::TableSchema;
use crabgresql_storage_api::pgstat::RelStatSnapshot;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;

/// Whether a namespace is one of PostgreSQL's system schemas, which is what
/// splits `pg_stat_sys_tables` from `pg_stat_user_tables`. PostgreSQL's own
/// rule, spelled the same way its view definitions do.
pub(crate) fn is_system_namespace(namespace: &str) -> bool {
    namespace == "pg_catalog" || namespace == "information_schema" || namespace == "pg_toast"
}

/// The 30 columns of `pg_stat_all_tables`, under `name`.
fn all_tables_schema(name: &str) -> TableSchema {
    TableSchema::in_namespace(
        name,
        "pg_catalog",
        vec![
            col("relid", PgType::Oid),
            col("schemaname", PgType::Name),
            col("relname", PgType::Name),
            col("seq_scan", PgType::Int8),
            col("last_seq_scan", PgType::TimestampTz),
            col("seq_tup_read", PgType::Int8),
            col("idx_scan", PgType::Int8),
            col("last_idx_scan", PgType::TimestampTz),
            col("idx_tup_fetch", PgType::Int8),
            col("n_tup_ins", PgType::Int8),
            col("n_tup_upd", PgType::Int8),
            col("n_tup_del", PgType::Int8),
            col("n_tup_hot_upd", PgType::Int8),
            col("n_tup_newpage_upd", PgType::Int8),
            col("n_live_tup", PgType::Int8),
            col("n_dead_tup", PgType::Int8),
            col("n_mod_since_analyze", PgType::Int8),
            col("n_ins_since_vacuum", PgType::Int8),
            col("last_vacuum", PgType::TimestampTz),
            col("last_autovacuum", PgType::TimestampTz),
            col("last_analyze", PgType::TimestampTz),
            col("last_autoanalyze", PgType::TimestampTz),
            col("vacuum_count", PgType::Int8),
            col("autovacuum_count", PgType::Int8),
            col("analyze_count", PgType::Int8),
            col("autoanalyze_count", PgType::Int8),
            col("total_vacuum_time", PgType::Float8),
            col("total_autovacuum_time", PgType::Float8),
            col("total_analyze_time", PgType::Float8),
            col("total_autoanalyze_time", PgType::Float8),
        ],
    )
}

/// The 12 columns of `pg_stat_xact_all_tables`, under `name`. A strict subset
/// of [`all_tables_schema`]'s: the transaction-scoped view carries no
/// timestamps and no vacuum history.
fn xact_tables_schema(name: &str) -> TableSchema {
    TableSchema::in_namespace(
        name,
        "pg_catalog",
        vec![
            col("relid", PgType::Oid),
            col("schemaname", PgType::Name),
            col("relname", PgType::Name),
            col("seq_scan", PgType::Int8),
            col("seq_tup_read", PgType::Int8),
            col("idx_scan", PgType::Int8),
            col("idx_tup_fetch", PgType::Int8),
            col("n_tup_ins", PgType::Int8),
            col("n_tup_upd", PgType::Int8),
            col("n_tup_del", PgType::Int8),
            col("n_tup_hot_upd", PgType::Int8),
            col("n_tup_newpage_upd", PgType::Int8),
        ],
    )
}

pub(crate) fn pg_stat_all_tables_schema() -> TableSchema {
    all_tables_schema("pg_stat_all_tables")
}

pub(crate) fn pg_stat_sys_tables_schema() -> TableSchema {
    all_tables_schema("pg_stat_sys_tables")
}

pub(crate) fn pg_stat_user_tables_schema() -> TableSchema {
    all_tables_schema("pg_stat_user_tables")
}

pub(crate) fn pg_stat_xact_all_tables_schema() -> TableSchema {
    xact_tables_schema("pg_stat_xact_all_tables")
}

pub(crate) fn pg_stat_xact_sys_tables_schema() -> TableSchema {
    xact_tables_schema("pg_stat_xact_sys_tables")
}

pub(crate) fn pg_stat_xact_user_tables_schema() -> TableSchema {
    xact_tables_schema("pg_stat_xact_user_tables")
}

/// One row per relation with counters, dropping any the snapshot cannot number
/// — a relation dropped since the counters were last touched has no `pg_class`
/// OID, and PostgreSQL drops its statistics at the same moment.
fn table_rows(cat: &SystemCatalog, want_system: Option<bool>) -> Vec<Vec<Value>> {
    cat.table_stats()
        .iter()
        .filter(|stats| want_system.is_none_or(|sys| sys == is_system_namespace(&stats.namespace)))
        .filter_map(|stats| table_row(cat, stats))
        .collect()
}

fn table_row(cat: &SystemCatalog, stats: &RelStatSnapshot) -> Option<Vec<Value>> {
    let oid = cat.relation_oid_in(&stats.namespace, &stats.name)?;
    Some(vec![
        Value::Oid(oid),
        Value::Text(stats.namespace.clone()),
        Value::Text(stats.name.clone()),
        counter(stats.seq_scan),
        stamp_or_null(stats.last_seq_scan),
        counter(stats.seq_tup_read),
        counter(stats.idx_scan),
        stamp_or_null(stats.last_idx_scan),
        counter(stats.idx_tup_fetch),
        counter(stats.n_tup_ins),
        counter(stats.n_tup_upd),
        counter(stats.n_tup_del),
        // HOT and new-page updates are properties of PostgreSQL's page layout
        // decisions during an update. This build's update path makes no such
        // distinction, so neither number exists to report.
        Value::Int8(0), // n_tup_hot_upd
        Value::Int8(0), // n_tup_newpage_upd
        counter(stats.n_tup_ins.saturating_sub(stats.n_tup_del)),
        // Dead rows are counted by PostgreSQL's collector as updates plus
        // deletes since the last vacuum. Reclaiming them here is the engine's
        // business and it reports no per-relation number for what it found, so
        // there is nothing to publish — and a count derived from the write
        // counters alone would keep rising through a `VACUUM` that had already
        // removed them.
        Value::Int8(0), // n_dead_tup
        counter(stats.n_mod_since_analyze),
        counter(stats.n_ins_since_vacuum),
        stamp_or_null(stats.last_vacuum),
        Value::Null, // last_autovacuum
        stamp_or_null(stats.last_analyze),
        Value::Null, // last_autoanalyze
        counter(stats.vacuum_count),
        Value::Int8(0), // autovacuum_count
        counter(stats.analyze_count),
        Value::Int8(0), // autoanalyze_count
        // The four `total_*_time` columns need each command timed. Nothing
        // times them, so they stay at PostgreSQL's own never-ran value.
        Value::Float8(0.0),
        Value::Float8(0.0),
        Value::Float8(0.0),
        Value::Float8(0.0),
    ])
}

pub(crate) fn pg_stat_all_tables_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    table_rows(cat, None)
}

pub(crate) fn pg_stat_sys_tables_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    table_rows(cat, Some(true))
}

pub(crate) fn pg_stat_user_tables_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    table_rows(cat, Some(false))
}
