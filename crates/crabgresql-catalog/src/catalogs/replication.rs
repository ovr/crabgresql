//! The replication family: origins, slots, the walsender/walreceiver views, and
//! the subscription side of logical replication.
//!
//! crabgresql is a single server. There is no `CREATE SUBSCRIPTION`, no
//! replication slot to hand out, and nothing connects on the replication
//! protocol — so all ten are empty, which is also what PostgreSQL answers on a
//! standalone instance nobody has replicated from or to. A monitoring client
//! that polls `pg_stat_replication` for lag reads "no standbys" from both
//! servers instead of failing on an unknown relation.
//!
//! The WAL these views would describe does exist (see `crabgresql-wal`), but
//! it is written for crash recovery only: no LSN here is being streamed
//! anywhere, so reporting one would be an invention, not a measurement.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, oid};

use crate::cols::*;

/// `pg_replication_origin` — the catalog of origins a subscriber tracks
/// progress against. `roident` is an `oid` column in the catalog even though
/// PostgreSQL calls the value a "replication origin id".
pub(crate) fn pg_replication_origin_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_replication_origin",
        "pg_catalog",
        vec![col("roident", PgType::Oid), col("roname", PgType::Text)],
    )
}

pub(crate) fn pg_replication_origin_status_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_replication_origin_status",
        "pg_catalog",
        vec![
            col("local_id", PgType::Oid),
            col("external_id", PgType::Text),
            col("remote_lsn", PgType::PgLsn),
            col("local_lsn", PgType::PgLsn),
        ],
    )
}

pub(crate) fn pg_replication_slots_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_replication_slots",
        "pg_catalog",
        vec![
            col("slot_name", PgType::Name),
            col("plugin", PgType::Name),
            col("slot_type", PgType::Text),
            col("datoid", PgType::Oid),
            col("database", PgType::Name),
            col("temporary", PgType::Bool),
            col("active", PgType::Bool),
            col("active_pid", PgType::Int4),
            col("xmin", PgType::Xid),
            col("catalog_xmin", PgType::Xid),
            col("restart_lsn", PgType::PgLsn),
            col("confirmed_flush_lsn", PgType::PgLsn),
            col("wal_status", PgType::Text),
            col("safe_wal_size", PgType::Int8),
            col("two_phase", PgType::Bool),
            col("two_phase_at", PgType::PgLsn),
            col("inactive_since", PgType::TimestampTz),
            col("conflicting", PgType::Bool),
            col("invalidation_reason", PgType::Text),
            col("failover", PgType::Bool),
            col("synced", PgType::Bool),
        ],
    )
}

pub(crate) fn pg_stat_replication_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stat_replication",
        "pg_catalog",
        vec![
            col("pid", PgType::Int4),
            col("usesysid", PgType::Oid),
            col("usename", PgType::Name),
            col("application_name", PgType::Text),
            col("client_addr", PgType::Inet),
            col("client_hostname", PgType::Text),
            col("client_port", PgType::Int4),
            col("backend_start", PgType::TimestampTz),
            col("backend_xmin", PgType::Xid),
            col("state", PgType::Text),
            col("sent_lsn", PgType::PgLsn),
            col("write_lsn", PgType::PgLsn),
            col("flush_lsn", PgType::PgLsn),
            col("replay_lsn", PgType::PgLsn),
            col("write_lag", PgType::Interval),
            col("flush_lag", PgType::Interval),
            col("replay_lag", PgType::Interval),
            col("sync_priority", PgType::Int4),
            col("sync_state", PgType::Text),
            col("reply_time", PgType::TimestampTz),
        ],
    )
}

pub(crate) fn pg_stat_replication_slots_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stat_replication_slots",
        "pg_catalog",
        vec![
            // text, not the `name` that pg_replication_slots.slot_name is: the
            // statistics view goes through a set-returning function that spells
            // it that way.
            col("slot_name", PgType::Text),
            col("spill_txns", PgType::Int8),
            col("spill_count", PgType::Int8),
            col("spill_bytes", PgType::Int8),
            col("stream_txns", PgType::Int8),
            col("stream_count", PgType::Int8),
            col("stream_bytes", PgType::Int8),
            col("total_txns", PgType::Int8),
            col("total_bytes", PgType::Int8),
            col("stats_reset", PgType::TimestampTz),
        ],
    )
}

pub(crate) fn pg_stat_subscription_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stat_subscription",
        "pg_catalog",
        vec![
            col("subid", PgType::Oid),
            col("subname", PgType::Name),
            col("worker_type", PgType::Text),
            col("pid", PgType::Int4),
            col("leader_pid", PgType::Int4),
            col("relid", PgType::Oid),
            col("received_lsn", PgType::PgLsn),
            col("last_msg_send_time", PgType::TimestampTz),
            col("last_msg_receipt_time", PgType::TimestampTz),
            col("latest_end_lsn", PgType::PgLsn),
            col("latest_end_time", PgType::TimestampTz),
        ],
    )
}

pub(crate) fn pg_stat_subscription_stats_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stat_subscription_stats",
        "pg_catalog",
        vec![
            col("subid", PgType::Oid),
            col("subname", PgType::Name),
            col("apply_error_count", PgType::Int8),
            col("sync_error_count", PgType::Int8),
            col("confl_insert_exists", PgType::Int8),
            col("confl_update_origin_differs", PgType::Int8),
            col("confl_update_exists", PgType::Int8),
            col("confl_update_missing", PgType::Int8),
            col("confl_delete_origin_differs", PgType::Int8),
            col("confl_delete_missing", PgType::Int8),
            col("confl_multiple_unique_conflicts", PgType::Int8),
            col("stats_reset", PgType::TimestampTz),
        ],
    )
}

pub(crate) fn pg_stat_wal_receiver_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stat_wal_receiver",
        "pg_catalog",
        vec![
            col("pid", PgType::Int4),
            col("status", PgType::Text),
            col("receive_start_lsn", PgType::PgLsn),
            col("receive_start_tli", PgType::Int4),
            col("written_lsn", PgType::PgLsn),
            col("flushed_lsn", PgType::PgLsn),
            col("received_tli", PgType::Int4),
            col("last_msg_send_time", PgType::TimestampTz),
            col("last_msg_receipt_time", PgType::TimestampTz),
            col("latest_end_lsn", PgType::PgLsn),
            col("latest_end_time", PgType::TimestampTz),
            col("slot_name", PgType::Text),
            col("sender_host", PgType::Text),
            col("sender_port", PgType::Int4),
            col("conninfo", PgType::Text),
        ],
    )
}

/// `pg_subscription`. The column order is PostgreSQL's own and not alphabetical
/// or grouped: `subskiplsn` sits third, before `subname`, because the catalog
/// puts the fixed-width columns first.
pub(crate) fn pg_subscription_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_subscription",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("subdbid", PgType::Oid),
            col("subskiplsn", PgType::PgLsn),
            col("subname", PgType::Name),
            col("subowner", PgType::Oid),
            col("subenabled", PgType::Bool),
            col("subbinary", PgType::Bool),
            col("substream", CHARLIKE),
            col("subtwophasestate", CHARLIKE),
            col("subdisableonerr", PgType::Bool),
            col("subpasswordrequired", PgType::Bool),
            col("subrunasowner", PgType::Bool),
            col("subfailover", PgType::Bool),
            col("subconninfo", PgType::Text),
            col("subslotname", PgType::Name),
            col("subsynccommit", PgType::Text),
            col("subpublications", PgType::Array(oid::TEXT)),
            col("suborigin", PgType::Text),
        ],
    )
}

pub(crate) fn pg_subscription_rel_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_subscription_rel",
        "pg_catalog",
        vec![
            col("srsubid", PgType::Oid),
            col("srrelid", PgType::Oid),
            col("srsubstate", CHARLIKE),
            col("srsublsn", PgType::PgLsn),
        ],
    )
}
