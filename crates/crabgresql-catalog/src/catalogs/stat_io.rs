//! `pg_stat_io`: block I/O broken down by backend type, object and context.
//!
//! Empty, deliberately. The view is a `backend_type × object × context` grid,
//! and this build runs one backend type, no background writer and no context
//! distinction — so all but one cell of it names a worker that does not exist.
//!
//! That one cell cannot be filled either: the buffer pool's counters are
//! cluster-wide totals with no backend attribution (the limit
//! [`crate::catalogs::statio`] documents), and publishing them as a row of this
//! grid would claim a breakdown nobody computed.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::PgType;

use crate::cols::*;

pub(crate) fn pg_stat_io_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stat_io",
        "pg_catalog",
        vec![
            col("backend_type", PgType::Text),
            col("object", PgType::Text),
            col("context", PgType::Text),
            col("reads", PgType::Int8),
            // `numeric`, not `bigint`: PostgreSQL widens a byte count rather
            // than risk the block-count multiplication overflowing.
            col("read_bytes", PgType::Numeric),
            col("read_time", PgType::Float8),
            col("writes", PgType::Int8),
            col("write_bytes", PgType::Numeric),
            col("write_time", PgType::Float8),
            col("writebacks", PgType::Int8),
            col("writeback_time", PgType::Float8),
            col("extends", PgType::Int8),
            col("extend_bytes", PgType::Numeric),
            col("extend_time", PgType::Float8),
            col("hits", PgType::Int8),
            col("evictions", PgType::Int8),
            col("reuses", PgType::Int8),
            col("fsyncs", PgType::Int8),
            col("fsync_time", PgType::Float8),
            col("stats_reset", PgType::TimestampTz),
        ],
    )
}
