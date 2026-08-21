//! `pg_stat_gssapi`: the GSSAPI parameters of each backend's connection.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;

/// `pg_catalog.pg_stat_gssapi` — one row per backend, keyed by `pid`, so it
/// joins to `pg_stat_activity` and to [`super::stat_ssl`] on that column; see
/// [`crate::source::CatalogSource::backends`] for why that is one row.
///
/// **Every flag is false.** The server asks for no credentials at all and
/// answers `GSSENCRequest` with `N`, so there is neither GSSAPI authentication
/// nor GSSAPI encryption to report — which is what PostgreSQL reports for a
/// backend that authenticated any other way.
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
