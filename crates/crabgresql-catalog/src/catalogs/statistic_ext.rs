//! The `CREATE STATISTICS` family: multi-column ("extended") statistics.
//!
//! Distinct from [`crate::catalogs::statistic`], which is per-column and really
//! is populated by `ANALYZE`. There is no `CREATE STATISTICS` here, so nothing
//! defines an extended statistics object and all four relations are empty —
//! PostgreSQL's answer too until someone creates one. psql's `\d <table>`
//! joins `pg_statistic_ext` to print the "Statistics objects:" footer.
//!
//! **Deviation.** The three statistics types (`pg_ndistinct`,
//! `pg_dependencies`, `pg_mcv_list`) and `anyarray` have no counterpart here, so
//! their columns are declared `text`. That is the same choice
//! [`crate::catalogs::statistic`] documents for `pg_statistic`'s value arrays,
//! and it costs nothing while the relations hold no rows to render.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, oid};

use crate::cols::*;

/// A `pg_ndistinct`/`pg_dependencies`/`pg_mcv_list`/`pg_statistic[]`/`anyarray`
/// column — see the module's deviation note.
const STATS_BLOB: PgType = PgType::Text;

pub(crate) fn pg_statistic_ext_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_statistic_ext",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("stxrelid", PgType::Oid),
            col("stxname", PgType::Name),
            col("stxnamespace", PgType::Oid),
            col("stxowner", PgType::Oid),
            col("stxkeys", INT2VECTOR),
            col("stxstattarget", PgType::Int2),
            col("stxkind", PgType::Array(oid::CHAR)),
            col("stxexprs", NODE_TREE),
        ],
    )
}

pub(crate) fn pg_statistic_ext_data_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_statistic_ext_data",
        "pg_catalog",
        vec![
            col("stxoid", PgType::Oid),
            col("stxdinherit", PgType::Bool),
            col("stxdndistinct", STATS_BLOB),
            col("stxddependencies", STATS_BLOB),
            col("stxdmcv", STATS_BLOB),
            col("stxdexpr", STATS_BLOB),
        ],
    )
}

pub(crate) fn pg_stats_ext_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stats_ext",
        "pg_catalog",
        vec![
            col("schemaname", PgType::Name),
            col("tablename", PgType::Name),
            col("statistics_schemaname", PgType::Name),
            col("statistics_name", PgType::Name),
            col("statistics_owner", PgType::Name),
            col("attnames", PgType::Array(oid::NAME)),
            col("exprs", PgType::Array(oid::TEXT)),
            col("kinds", PgType::Array(oid::CHAR)),
            col("inherited", PgType::Bool),
            col("n_distinct", STATS_BLOB),
            col("dependencies", STATS_BLOB),
            col("most_common_vals", PgType::Array(oid::TEXT)),
            col("most_common_val_nulls", PgType::Array(oid::BOOL)),
            col("most_common_freqs", PgType::Array(oid::FLOAT8)),
            col("most_common_base_freqs", PgType::Array(oid::FLOAT8)),
        ],
    )
}

pub(crate) fn pg_stats_ext_exprs_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stats_ext_exprs",
        "pg_catalog",
        vec![
            col("schemaname", PgType::Name),
            col("tablename", PgType::Name),
            col("statistics_schemaname", PgType::Name),
            col("statistics_name", PgType::Name),
            col("statistics_owner", PgType::Name),
            col("expr", PgType::Text),
            col("inherited", PgType::Bool),
            col("null_frac", PgType::Float4),
            col("avg_width", PgType::Int4),
            col("n_distinct", PgType::Float4),
            col("most_common_vals", STATS_BLOB),
            col("most_common_freqs", PgType::Array(oid::FLOAT4)),
            col("histogram_bounds", STATS_BLOB),
            col("correlation", PgType::Float4),
            col("most_common_elems", STATS_BLOB),
            col("most_common_elem_freqs", PgType::Array(oid::FLOAT4)),
            col("elem_count_histogram", PgType::Array(oid::FLOAT4)),
        ],
    )
}
