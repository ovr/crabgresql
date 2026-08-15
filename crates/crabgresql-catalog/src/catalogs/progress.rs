//! The `pg_stat_progress_*` views: what a long-running maintenance command is
//! doing while it runs.
//!
//! All six are empty here, and empty is the same answer PostgreSQL gives: a
//! progress view holds one row per backend *currently executing* the command it
//! reports on, so on an idle server every one of them is empty there too. A
//! monitoring client that polls these gets the same "nothing running" from both
//! servers rather than "relation does not exist".
//!
//! That the commands themselves are missing (`CLUSTER`, `VACUUM`'s progress
//! reporting, base backups) only widens the window in which the row would be
//! absent; it does not change what the view says.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::PgType;

use crate::cols::*;

pub(crate) fn pg_stat_progress_analyze_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stat_progress_analyze",
        "pg_catalog",
        vec![
            col("pid", PgType::Int4),
            col("datid", PgType::Oid),
            col("datname", PgType::Name),
            col("relid", PgType::Oid),
            col("phase", PgType::Text),
            col("sample_blks_total", PgType::Int8),
            col("sample_blks_scanned", PgType::Int8),
            col("ext_stats_total", PgType::Int8),
            col("ext_stats_computed", PgType::Int8),
            col("child_tables_total", PgType::Int8),
            col("child_tables_done", PgType::Int8),
            col("current_child_table_relid", PgType::Oid),
            col("delay_time", PgType::Float8),
        ],
    )
}

pub(crate) fn pg_stat_progress_basebackup_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stat_progress_basebackup",
        "pg_catalog",
        vec![
            col("pid", PgType::Int4),
            col("phase", PgType::Text),
            col("backup_total", PgType::Int8),
            col("backup_streamed", PgType::Int8),
            col("tablespaces_total", PgType::Int8),
            col("tablespaces_streamed", PgType::Int8),
        ],
    )
}

pub(crate) fn pg_stat_progress_cluster_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stat_progress_cluster",
        "pg_catalog",
        vec![
            col("pid", PgType::Int4),
            col("datid", PgType::Oid),
            col("datname", PgType::Name),
            col("relid", PgType::Oid),
            col("command", PgType::Text),
            col("phase", PgType::Text),
            col("cluster_index_relid", PgType::Oid),
            col("heap_tuples_scanned", PgType::Int8),
            col("heap_tuples_written", PgType::Int8),
            col("heap_blks_total", PgType::Int8),
            col("heap_blks_scanned", PgType::Int8),
            col("index_rebuild_count", PgType::Int8),
        ],
    )
}

pub(crate) fn pg_stat_progress_copy_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stat_progress_copy",
        "pg_catalog",
        vec![
            col("pid", PgType::Int4),
            col("datid", PgType::Oid),
            col("datname", PgType::Name),
            col("relid", PgType::Oid),
            col("command", PgType::Text),
            col("type", PgType::Text),
            col("bytes_processed", PgType::Int8),
            col("bytes_total", PgType::Int8),
            col("tuples_processed", PgType::Int8),
            col("tuples_excluded", PgType::Int8),
            col("tuples_skipped", PgType::Int8),
        ],
    )
}

pub(crate) fn pg_stat_progress_create_index_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stat_progress_create_index",
        "pg_catalog",
        vec![
            col("pid", PgType::Int4),
            col("datid", PgType::Oid),
            col("datname", PgType::Name),
            col("relid", PgType::Oid),
            col("index_relid", PgType::Oid),
            col("command", PgType::Text),
            col("phase", PgType::Text),
            col("lockers_total", PgType::Int8),
            col("lockers_done", PgType::Int8),
            // int8, unlike the `pid` above: PostgreSQL reads this one out of a
            // generic int8 progress slot and never narrows it.
            col("current_locker_pid", PgType::Int8),
            col("blocks_total", PgType::Int8),
            col("blocks_done", PgType::Int8),
            col("tuples_total", PgType::Int8),
            col("tuples_done", PgType::Int8),
            col("partitions_total", PgType::Int8),
            col("partitions_done", PgType::Int8),
        ],
    )
}

pub(crate) fn pg_stat_progress_vacuum_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stat_progress_vacuum",
        "pg_catalog",
        vec![
            col("pid", PgType::Int4),
            col("datid", PgType::Oid),
            col("datname", PgType::Name),
            col("relid", PgType::Oid),
            col("phase", PgType::Text),
            col("heap_blks_total", PgType::Int8),
            col("heap_blks_scanned", PgType::Int8),
            col("heap_blks_vacuumed", PgType::Int8),
            col("index_vacuum_count", PgType::Int8),
            col("max_dead_tuple_bytes", PgType::Int8),
            col("dead_tuple_bytes", PgType::Int8),
            col("num_dead_item_ids", PgType::Int8),
            col("indexes_total", PgType::Int8),
            col("indexes_processed", PgType::Int8),
            col("delay_time", PgType::Float8),
        ],
    )
}
