//! `pg_timezone_names` and `pg_timezone_abbrevs`.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;

/// `pg_catalog.pg_timezone_names` — every IANA zone the bundled tz database
/// knows, with its offset and DST flag **at the given instant** (PostgreSQL
/// reports these as of `now()`, so a zone's row changes with the season).
pub(crate) fn pg_timezone_names_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_timezone_names",
        "pg_catalog",
        vec![
            col("name", PgType::Text),
            col("abbrev", PgType::Text),
            col("utc_offset", PgType::Interval),
            col("is_dst", PgType::Bool),
        ],
    )
}

pub(crate) fn pg_timezone_names_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let at_micros = cat.now();
    crabgresql_types::tz::timezone_names(at_micros)
        .into_iter()
        .map(|z| {
            vec![
                Value::Text(z.name),
                Value::Text(z.abbrev),
                offset_interval(z.utc_offset_secs),
                Value::Bool(z.is_dst),
            ]
        })
        .collect()
}

/// `pg_catalog.pg_timezone_abbrevs` — the abbreviations a datetime literal
/// accepts.
///
/// Divergence, deliberate: PostgreSQL 18.4 loads 198 abbreviations from the file
/// its `timezone_abbreviations` names, and this server has a curated 15 (see
/// `crabgresql_types::tz::timezone_abbrevs` for why growing that table is a
/// change to value parsing, not to a view). Consequences a reader should
/// expect: `count(*)` is 15, the offsets span 9 distinct values, and upstream's
/// `sysviews` check `count(distinct utc_offset) >= 24` reports false.
/// PostgreSQL 18's second half of this view — the abbreviations from the
/// *session zone's* own history, which is where its `LMT` rows come from — is
/// not implemented.
pub(crate) fn pg_timezone_abbrevs_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_timezone_abbrevs",
        "pg_catalog",
        vec![
            col("abbrev", PgType::Text),
            col("utc_offset", PgType::Interval),
            col("is_dst", PgType::Bool),
        ],
    )
}

pub(crate) fn pg_timezone_abbrevs_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let at_micros = cat.now();
    crabgresql_types::tz::timezone_abbrevs(at_micros)
        .into_iter()
        .map(|a| {
            vec![
                Value::Text(a.abbrev.to_string()),
                offset_interval(a.utc_offset_secs),
                Value::Bool(a.is_dst),
            ]
        })
        .collect()
}

/// A UTC offset as the `interval` both timezone views report it. Whole seconds
/// only: no tz database entry carries a sub-second offset.
fn offset_interval(secs: i32) -> Value {
    Value::Interval(crabgresql_types::interval::Interval {
        months: 0,
        days: 0,
        usec: i64::from(secs) * 1_000_000,
    })
}
