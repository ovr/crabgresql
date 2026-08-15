//! `pg_stat_database` and `pg_stat_database_conflicts`: the database-wide
//! cumulative counters.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;
use crate::oids::DATABASE_OID;

/// `pg_catalog.pg_stat_database` — the work done in this database since the
/// counters were last reset.
///
/// One row: one database. PostgreSQL also emits a `datid = 0` row for the work
/// that belongs to no database, and there is no such accounting here — a zero
/// row for it would suggest one.
///
/// The columns held at zero or NULL count something that does not happen here,
/// rather than something that happened zero times: recovery `conflicts` (no
/// standby), `temp_files`/`temp_bytes` (no sort or hash spills to disk),
/// `deadlocks` (no detector), `checksum_failures` (checksums are not verified),
/// the `blk_*_time` and per-state session timings (nothing is instrumented),
/// how a session ended, and the parallel-worker counts.
pub(crate) fn pg_stat_database_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stat_database",
        "pg_catalog",
        vec![
            col("datid", PgType::Oid),
            col("datname", PgType::Name),
            col("numbackends", PgType::Int4),
            col("xact_commit", PgType::Int8),
            col("xact_rollback", PgType::Int8),
            col("blks_read", PgType::Int8),
            col("blks_hit", PgType::Int8),
            col("tup_returned", PgType::Int8),
            col("tup_fetched", PgType::Int8),
            col("tup_inserted", PgType::Int8),
            col("tup_updated", PgType::Int8),
            col("tup_deleted", PgType::Int8),
            col("conflicts", PgType::Int8),
            col("temp_files", PgType::Int8),
            col("temp_bytes", PgType::Int8),
            col("deadlocks", PgType::Int8),
            col("checksum_failures", PgType::Int8),
            col("checksum_last_failure", PgType::TimestampTz),
            col("blk_read_time", PgType::Float8),
            col("blk_write_time", PgType::Float8),
            col("session_time", PgType::Float8),
            col("active_time", PgType::Float8),
            col("idle_in_transaction_time", PgType::Float8),
            col("sessions", PgType::Int8),
            col("sessions_abandoned", PgType::Int8),
            col("sessions_fatal", PgType::Int8),
            col("sessions_killed", PgType::Int8),
            col("parallel_workers_to_launch", PgType::Int8),
            col("parallel_workers_launched", PgType::Int8),
            col("stats_reset", PgType::TimestampTz),
        ],
    )
}

pub(crate) fn pg_stat_database_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let stats = cat.database_stats();
    vec![vec![
        Value::Oid(DATABASE_OID),
        Value::Text(cat.database().to_string()),
        Value::Int4(stats.numbackends),
        counter(stats.xact_commit),
        counter(stats.xact_rollback),
        counter(stats.blks_read),
        counter(stats.blks_hit),
        counter(stats.tup_returned),
        counter(stats.tup_fetched),
        counter(stats.tup_inserted),
        counter(stats.tup_updated),
        counter(stats.tup_deleted),
        Value::Int8(0), // conflicts
        Value::Int8(0), // temp_files
        Value::Int8(0), // temp_bytes
        Value::Int8(0), // deadlocks
        Value::Int8(0), // checksum_failures
        Value::Null,    // checksum_last_failure
        Value::Float8(0.0),
        Value::Float8(0.0),
        Value::Float8(0.0),
        Value::Float8(0.0),
        Value::Float8(0.0),
        counter(stats.sessions),
        Value::Int8(0), // sessions_abandoned
        Value::Int8(0), // sessions_fatal
        Value::Int8(0), // sessions_killed
        Value::Int8(0), // parallel_workers_to_launch
        Value::Int8(0), // parallel_workers_launched
        stamp_or_null((stats.stats_reset != 0).then_some(stats.stats_reset)),
    ]]
}

/// `pg_catalog.pg_stat_database_conflicts` — queries a standby cancelled
/// because replay needed something they were holding open.
///
/// One row, all zeros. Every conflict this counts can only happen on a hot
/// standby, and there is no replication here at all (see
/// [`crate::catalogs::replication`]) — which is also what PostgreSQL reports on
/// a primary, where the columns exist and stay at zero forever.
pub(crate) fn pg_stat_database_conflicts_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stat_database_conflicts",
        "pg_catalog",
        vec![
            col("datid", PgType::Oid),
            col("datname", PgType::Name),
            col("confl_tablespace", PgType::Int8),
            col("confl_lock", PgType::Int8),
            col("confl_snapshot", PgType::Int8),
            col("confl_bufferpin", PgType::Int8),
            col("confl_deadlock", PgType::Int8),
            col("confl_active_logicalslot", PgType::Int8),
        ],
    )
}

pub(crate) fn pg_stat_database_conflicts_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    vec![vec![
        Value::Oid(DATABASE_OID),
        Value::Text(cat.database().to_string()),
        Value::Int8(0),
        Value::Int8(0),
        Value::Int8(0),
        Value::Int8(0),
        Value::Int8(0),
        Value::Int8(0),
    ]]
}
