//! `pg_settings`: the session's configuration parameters.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;

/// `pg_catalog.pg_settings` — the configuration parameters.
///
/// A view over `pg_show_all_settings()` in PostgreSQL; served here as a
/// relation whose rows the session supplies, which is indistinguishable to a
/// client reading it. Rows come in `SHOW ALL`'s order (by name,
/// case-insensitively), and a parameter PostgreSQL flags `GUC_NO_SHOW_ALL` —
/// `is_superuser` — is absent from both, as it is upstream.
///
/// `sourcefile`/`sourceline` are always NULL and `pending_restart` always
/// false: this server reads no configuration file and has nothing that a
/// restart would change.
pub(crate) fn pg_settings_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_settings",
        "pg_catalog",
        vec![
            col("name", PgType::Text),
            col("setting", PgType::Text),
            col("unit", PgType::Text),
            col("category", PgType::Text),
            col("short_desc", PgType::Text),
            col("extra_desc", PgType::Text),
            col("context", PgType::Text),
            col("vartype", PgType::Text),
            col("source", PgType::Text),
            col("min_val", PgType::Text),
            col("max_val", PgType::Text),
            col("enumvals", PgType::Array(crabgresql_types::oid::TEXT)),
            col("boot_val", PgType::Text),
            col("reset_val", PgType::Text),
            col("sourcefile", PgType::Text),
            col("sourceline", PgType::Int4),
            col("pending_restart", PgType::Bool),
        ],
    )
}

pub(crate) fn pg_settings_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let settings = cat.settings();
    let text = |s: Option<&str>| s.map_or(Value::Null, |s| Value::Text(s.to_string()));
    settings
        .iter()
        .map(|s| {
            vec![
                Value::Text(s.name.to_string()),
                Value::Text(s.setting.clone()),
                text(s.unit),
                Value::Text(s.category.to_string()),
                Value::Text(s.short_desc.to_string()),
                text(s.extra_desc),
                Value::Text(s.context.to_string()),
                Value::Text(s.vartype.to_string()),
                Value::Text(s.source.to_string()),
                text(s.min_val),
                text(s.max_val),
                s.enumvals.map_or(Value::Null, |vals| {
                    Value::array_1d(
                        PgType::Text,
                        vals.iter().map(|v| Value::Text(v.to_string())).collect(),
                    )
                }),
                Value::Text(s.boot_val.to_string()),
                Value::Text(s.reset_val.clone()),
                Value::Null,
                Value::Null,
                Value::Bool(false),
            ]
        })
        .collect()
}
