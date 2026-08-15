//! `pg_stat_io`: block I/O broken down by backend type, object and context.
//!
//! Empty, deliberately. PostgreSQL's grid is `backend_type × object × context`
//! — a checkpointer, a background writer, an autovacuum worker and a client
//! backend, each against permanent or temporary relations, in normal, vacuum,
//! bulkread or bulkwrite context. This build has one backend type (the client
//! session), no background writer and no such context distinction, so all but a
//! single cell of that grid names a worker that does not exist here.
//!
//! The one cell that *would* be real — a client backend reading permanent
//! relations in normal context — cannot be filled either: the counters the
//! buffer pool keeps are cluster-wide totals with no backend attribution, which
//! is the same limit [`crate::catalogs::statio`] documents. Publishing them as
//! one row of that grid would claim a breakdown that was never computed, so the
//! relation is served with its shape and no rows: a client that groups by
//! `backend_type` reads "nothing measured" rather than a total mislabelled as a
//! category.

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
            // The `*_bytes` columns are `numeric`, not `bigint`: a byte count is
            // a block count times the block size, and PostgreSQL widens it
            // rather than risk the multiplication overflowing.
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
