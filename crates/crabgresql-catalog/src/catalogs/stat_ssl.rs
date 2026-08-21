//! `pg_stat_ssl`: the TLS parameters of each backend's connection.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;

/// `pg_catalog.pg_stat_ssl` — one row per backend, keyed by `pid`, so it joins
/// to `pg_stat_activity` on that column. It shows the same set of backends,
/// which here is the session reading it; see
/// [`crate::source::CatalogSource::backends`] for why that is one row.
///
/// **`ssl` is always false.** Nothing in this build speaks TLS: the server
/// refuses the `SSLRequest` packet and every connection is cleartext, so the
/// seven columns PostgreSQL fills from the handshake have nothing to report.
/// PostgreSQL leaves all of them NULL for a non-TLS backend as well, so a
/// monitoring client that asks "is this connection encrypted" reads `f` from
/// both servers rather than failing on an unknown relation.
pub(crate) fn pg_stat_ssl_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stat_ssl",
        "pg_catalog",
        vec![
            col("pid", PgType::Int4),
            col("ssl", PgType::Bool),
            col("version", PgType::Text),
            col("cipher", PgType::Text),
            col("bits", PgType::Int4),
            col("client_dn", PgType::Text),
            col("client_serial", PgType::Numeric),
            col("issuer_dn", PgType::Text),
        ],
    )
}

pub(crate) fn pg_stat_ssl_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    cat.backends()
        .iter()
        .map(|backend| {
            vec![
                Value::Int4(backend.pid),
                Value::Bool(false),
                Value::Null, // version
                Value::Null, // cipher
                Value::Null, // bits
                Value::Null, // client_dn
                Value::Null, // client_serial
                Value::Null, // issuer_dn
            ]
        })
        .collect()
}
