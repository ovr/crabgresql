//! `pg_stat_gssapi`: the GSSAPI parameters of each backend's connection.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;

/// `pg_catalog.pg_stat_gssapi` — one row per backend, keyed by `pid`, so it
/// joins to `pg_stat_activity` and to [`crate::catalogs::stat_ssl`] on that
/// column. Same set of backends, which here is the session reading it; see
/// [`crate::source::CatalogSource::backends`] for why that is one row.
///
/// **Every flag is false.** There is no GSSAPI authentication and no GSSAPI
/// encryption in this build — the server asks for no credentials at all and
/// answers `GSSENCRequest` with `N`, exactly as it does `SSLRequest` — so the
/// three booleans are false and `principal` is NULL, which is what PostgreSQL
/// reports for a backend that authenticated any other way. The distinction the
/// columns exist to draw (authenticated vs. encrypted vs. delegated) is one
/// this build cannot give three different answers to.
pub(crate) fn pg_stat_gssapi_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stat_gssapi",
        "pg_catalog",
        vec![
            col("pid", PgType::Int4),
            col("gss_authenticated", PgType::Bool),
            col("principal", PgType::Text),
            col("encrypted", PgType::Bool),
            col("credentials_delegated", PgType::Bool),
        ],
    )
}

pub(crate) fn pg_stat_gssapi_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    cat.backends()
        .iter()
        .map(|backend| {
            vec![
                Value::Int4(backend.pid),
                Value::Bool(false), // gss_authenticated
                Value::Null,        // principal
                Value::Bool(false), // encrypted
                Value::Bool(false), // credentials_delegated
            ]
        })
        .collect()
}
