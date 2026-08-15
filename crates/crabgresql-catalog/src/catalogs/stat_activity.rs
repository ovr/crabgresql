//! `pg_stat_activity`: what the backends are doing right now.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::cols::*;
use crate::oids::{BOOTSTRAP_ROLE_OID, DATABASE_OID};
use crate::{SystemCatalog, source::CatalogBackend};

/// `pg_catalog.pg_stat_activity` — one row per backend.
///
/// **One row: the session reading it.** PostgreSQL answers this from shared
/// memory and shows every backend in the cluster; there is no registry of live
/// connections here (the gap [`crate::source::CatalogSource::backends`]
/// describes, and the same one that leaves a `CancelRequest` with no session to
/// find), so a session can only describe itself. A monitoring query still binds
/// and still returns a truthful row — it just sees one backend where PostgreSQL
/// would list the others too.
///
/// That reading session is by definition running a query — the one that opened
/// this relation — so `state` is always `active` and `query` is that statement.
/// The states PostgreSQL reports for *other* backends (`idle`,
/// `idle in transaction`) are unreachable here for the same reason the other
/// rows are.
///
/// Constant columns, and why:
///
/// * `leader_pid` — parallel query's leader. There is no parallel query.
/// * `client_addr`/`client_hostname`/`client_port` — NULL, as PostgreSQL
///   reports for a connection whose address it does not track. The socket's
///   peer is known at accept time but is not carried on the session.
/// * `wait_event_type`/`wait_event` — NULL, which PostgreSQL also uses for a
///   backend that is running rather than waiting. True of this row by
///   construction: it is executing the query that reads the view.
/// * `query_id` — NULL unless `compute_query_id` is on in PostgreSQL; nothing
///   hashes a plan here.
/// * `backend_type` — always `client backend`. Every other value names a
///   background process this build does not run.
pub(crate) fn pg_stat_activity_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stat_activity",
        "pg_catalog",
        vec![
            col("datid", PgType::Oid),
            col("datname", PgType::Name),
            col("pid", PgType::Int4),
            col("leader_pid", PgType::Int4),
            col("usesysid", PgType::Oid),
            col("usename", PgType::Name),
            col("application_name", PgType::Text),
            col("client_addr", PgType::Inet),
            col("client_hostname", PgType::Text),
            col("client_port", PgType::Int4),
            col("backend_start", PgType::TimestampTz),
            col("xact_start", PgType::TimestampTz),
            col("query_start", PgType::TimestampTz),
            col("state_change", PgType::TimestampTz),
            col("wait_event_type", PgType::Text),
            col("wait_event", PgType::Text),
            col("state", PgType::Text),
            col("backend_xid", PgType::Xid),
            col("backend_xmin", PgType::Xid),
            col("query_id", PgType::Int8),
            col("query", PgType::Text),
            col("backend_type", PgType::Text),
        ],
    )
}

pub(crate) fn pg_stat_activity_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    cat.backends()
        .iter()
        .map(|backend| backend_row(cat, backend))
        .collect()
}

fn backend_row(cat: &SystemCatalog, backend: &CatalogBackend) -> Vec<Value> {
    vec![
        Value::Oid(DATABASE_OID),
        Value::Text(cat.database().to_string()),
        Value::Int4(backend.pid),
        Value::Null, // leader_pid
        Value::Oid(BOOTSTRAP_ROLE_OID),
        Value::Text(cat.owner().to_string()),
        Value::Text(backend.application_name.clone()),
        Value::Null, // client_addr
        Value::Null, // client_hostname
        Value::Null, // client_port
        Value::TimestampTz(backend.backend_start),
        stamp_or_null(backend.xact_start),
        Value::TimestampTz(backend.query_start),
        Value::TimestampTz(backend.state_change),
        Value::Null, // wait_event_type
        Value::Null, // wait_event
        Value::Text(backend.state.to_string()),
        match backend.backend_xid {
            Some(xid) => Value::Xid(xid),
            None => Value::Null,
        },
        match backend.backend_xmin {
            Some(xid) => Value::Xid(xid),
            None => Value::Null,
        },
        Value::Null, // query_id
        Value::Text(backend.query.clone()),
        Value::Text("client backend".to_string()),
    ]
}
