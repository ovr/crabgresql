//! Managed append-only Parquet table access method.
//!
//! A statement batch is written as one or more immutable fragments. Files remain
//! `.pending` until the transaction commits; the engine's finalize hook promotes
//! them to `.parquet` or removes them on abort. MVCC identity lives in file
//! metadata, leaving the physical Parquet schema composed solely of user columns.
//!
//! Fragments are immutable, so the only supported mutation besides INSERT is
//! TRUNCATE, implemented as the directory-level twin of the heap's
//! relfilenode-swap: the truncating transaction stages a fresh, empty
//! `parquet/<new>/` and reads and writes there, while the old directory stays
//! untouched until commit (it is removed on commit, and the staged one on abort).
//! A `.pending`-style rename cannot express "all rows are gone", and a tombstone
//! that merely hides fragments would neither free the space nor reset `relpages`
//! until a vacuum, which is not what TRUNCATE promises.
//!
//! Known divergence, shared with the heap and inherited from
//! [`crabgresql_txn::TableLock`]'s scope: a reader's/writer's shared hold covers one
//! *operation*, not its whole transaction (PostgreSQL holds `RowExclusiveLock` to
//! end-of-transaction). So a TRUNCATE can commit between two statements of another
//! open transaction and discard fragments that transaction had already staged.
//!
//! TODO: scope the table lock to the whole transaction instead of to one operation
//! (in the engine, not as per-AM bookkeeping), which is what closes this hole.

mod buffered;
mod epoch;
mod error;
mod fragment;
mod scan;
mod schema;
mod table;
mod wal;

#[cfg(test)]
mod test_support;

pub use buffered::BufferedParquetTable;
pub use schema::{supports_type, validate_schema};
pub use table::ParquetTable;
pub use wal::{
    PARQUET_TRUNCATE, PARQUET_XID_OBSERVED, ParquetRedo, ParquetSwap, RMGR_PARQUET,
    RecoveredParquetTruncate,
};
