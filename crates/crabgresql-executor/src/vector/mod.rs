//! Columnar execution nodes, sitting beside the row nodes rather than
//! replacing them.
//!
//! `docs/ARCHITECTURE.md` §1.2 puts it as "Volcano first, vectorization later":
//! PostgreSQL's semantics are tied to row-at-a-time execution (volatile call
//! order, cursors, per-row triggers), so the row executor stays the default and
//! the whole of it stays correct on its own. A columnar node is only ever an
//! *alternative* the planner may pick when it can prove the choice invisible.
//!
//! # Shape of a vectorized plan
//!
//! A columnar **segment** starts at a scan whose engine can hand up Arrow
//! batches and continues as far up the pipeline as every operator has a
//! vectorized form. Wherever it stops, [`Shred`] turns batches back into tuples
//! and the ordinary row nodes carry on. Nothing above the shred can tell the
//! difference, which is what makes the choice safe to make per-node:
//!
//! ```text
//!   BatchScan ──▶ FilterBatch ──▶ Shred ──▶ Projection ──▶ Sort   (row nodes)
//!   └──────────── columnar ───────────┘
//! ```
//!
//! # What a batch means here
//!
//! Batches carry `Value` semantics, not Arrow's — see the invariant on
//! [`crabgresql_storage_api::arrow`]. They are also **full width** in table
//! schema order, so a `BoundExpr::ColumnRef { index }` indexes a batch column
//! exactly as it indexes a row.

mod batch_append;
mod batch_scan;
pub mod expr;
mod filter_batch;
mod projection;
mod shred;
mod sort;
#[cfg(test)]
mod testutil;

pub use batch_append::BatchAppend;
pub use batch_scan::BatchScan;
pub use filter_batch::FilterBatch;
pub use projection::{ProjectBatch, Take};
pub use shred::Shred;
pub use sort::SortBatch;

use std::sync::Arc;

use arrow_array::RecordBatch;
use crabgresql_storage_api::{Column, TableSchema};

use crate::ExecError;

/// A columnar execution node: `next_batch()` pulls many rows at a time.
///
/// The columnar twin of [`ExecNode`](crate::ExecNode), and deliberately the
/// same shape — pull based, `Send`, no lifetime — so a node can be suspended
/// inside a portal and resumed across `Execute` round trips just as a row node
/// can.
pub trait BatchNode: Send {
    fn next_batch(&mut self) -> Result<Option<RecordBatch>, ExecError>;
}

/// The columns a batch carries, in batch order.
///
/// Used to decode a batch back into tuples. For a scan this is just the table's
/// columns; it is a layout rather than a `TableSchema` because an operator may
/// hand up a batch whose columns are no longer a table's.
pub type BatchLayout = Arc<[Column]>;

/// The layout of a batch straight from a scan of `schema`.
pub fn layout_of(schema: &TableSchema) -> BatchLayout {
    Arc::from(schema.columns.clone())
}

/// A columnar node that met a state its compile step promised to rule out — a
/// kernel that failed, a column that is not there, an operand of the wrong
/// type. The nodes decline anything they cannot run, so reaching one of these
/// is a defect in that gate rather than anything the user wrote.
fn internal(message: &str) -> ExecError {
    ExecError::new("XX000", message)
}
