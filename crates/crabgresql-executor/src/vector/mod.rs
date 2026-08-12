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
//! vectorized form. Wherever it stops, [`shred`] turns batches back into row
//! chunks and the ordinary row nodes carry on. Nothing above the shred can tell
//! the difference, which is what makes the choice safe to make per-node:
//!
//! ```text
//!   batch_scan ──▶ filter_batches ──▶ shred ──▶ projection ──▶ sort  (row nodes)
//!   └──────────────── columnar ────────────┘
//! ```
//!
//! # What a batch means here
//!
//! Batches carry `Value` semantics, not Arrow's — see the invariant on
//! [`crabgresql_storage_api::arrow`]. They are also **full width** in table
//! schema order, so a `BoundExpr::ColumnRef { index }` indexes a batch column
//! exactly as it indexes a row.

pub mod expr;
pub mod sort;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use arrow_select::filter::filter_record_batch;
use async_stream::try_stream;
use futures_util::StreamExt;

use crabgresql_planner::PhysicalAppendArm;
use crabgresql_storage_api::arrow::decode_columns;
use crabgresql_storage_api::{Column, ColumnProjection, TableAm, TableSchema};
use crabgresql_txn::TxnContext;

use crate::ExecError;
use crate::stream::{BatchStream, ROW_CHUNK, RowChunk, RowStream, blocking_batches};

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

/// A full table scan that yields batches — [`crate::seq_scan`]'s columnar twin.
///
/// `None` when the engine has no batch path, so a caller always has the row scan
/// to fall back on. The engine's scan is a synchronous iterator, so the pulls go
/// through [`blocking_batches`] rather than holding a worker per batch.
pub fn batch_scan(
    table: &Arc<dyn TableAm>,
    txn: &TxnContext,
    projection: &ColumnProjection,
) -> Option<BatchStream> {
    table
        .scan_batches(txn, projection)
        .map(|iter| blocking_batches(iter))
}

/// Concatenates several batch sources — the columnar [`crate::append_scan`].
///
/// A Parquet relation is always read through this: it plans as an `Append` over
/// its chunk store and its RAM write buffer. Without a columnar Append the
/// batches would be shredded at the leaves and nothing above could vectorize,
/// so this is load-bearing rather than an extra.
///
/// All-or-nothing: one row-only arm puts the whole relation back on the row
/// path, because the arms' outputs are concatenated and must share one
/// representation. `None` — stay on the row path — if any arm cannot hand up
/// batches, if any arm carries a column remap, or if any arm must append a
/// `tableoid`. A batch is in its own relation's column order and there is
/// nowhere here to permute one, so a remapped arm would concatenate mis-ordered
/// columns rather than fail loudly.
///
/// The remap branch is unreachable today: DDL refuses an engine-managed
/// relation on either side of an inheritance link, so no remapped arm can be
/// batch-capable. The planner's `arms_batch` applies that same remap rule, so a
/// remapped arm never makes `EXPLAIN` disagree with what runs.
///
/// TODO: hand up batches for an arm that must append a `tableoid` slot.
/// `arms_batch` does not test for that slot, so until then `EXPLAIN` calls
/// such an `Append` columnar while it runs on rows.
pub fn batch_append(arms: &[PhysicalAppendArm], txn: &TxnContext) -> Option<BatchStream> {
    let children = arms
        .iter()
        .map(|arm| {
            // A remapped arm, or one that must append a `tableoid`, changes
            // the row shape the batch layout describes.
            if arm.relation.map.is_some() || arm.relation.tableoid.is_some() {
                return None;
            }
            batch_scan(&arm.relation.table, txn, &arm.projection)
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Box::pin(try_stream! {
        for child in children {
            let mut child = child;
            while let Some(batch) = child.next().await {
                yield batch?;
            }
        }
    }))
}

/// Drops the rows of each batch that fail a predicate — the columnar
/// [`crate::filter`].
///
/// This is the operator that makes vectorizing pay. [`shred`] costs one tuple
/// build per surviving row, so filtering *below* the shred is what turns a
/// selective `WHERE` into work the row executor never does at all.
///
/// A batch that loses every row is passed on empty rather than skipped; `shred`
/// treats an empty batch as "nothing here", not "nothing left".
pub fn filter_batches(child: BatchStream, predicate: expr::VectorPredicate) -> BatchStream {
    Box::pin(try_stream! {
        let mut child = child;
        while let Some(batch) = child.next().await {
            let batch = batch?;
            let mask = predicate.evaluate(&batch)?;
            // `filter_record_batch` keeps only `true`; `false` and NULL both
            // drop, which is SQL's rule for a `WHERE` and matches
            // `predicate_holds`.
            yield filter_record_batch(&batch, &mask).map_err(|error| {
                ExecError::new("XX000", format!("vectorized filter failed: {error}"))
            })?;
        }
    })
}

/// Turns a batch stream back into a row stream — the boundary where a columnar
/// segment ends and the row executor resumes.
///
/// Every vectorized plan has exactly one of these per segment. It is pure cost
/// (the work the columnar nodes below it saved has to be paid back for the rows
/// that survive), which is the whole argument for pushing selective operators
/// like a filter *below* it: fewer surviving rows, less to shred.
///
/// A batch becomes whole [`RowChunk`]s, which is why the row path counts in
/// chunks at all: the boundary between the two representations is a regroup, not
/// a per-row handover. A batch wider than [`ROW_CHUNK`] rows is split rather
/// than shredded whole — a columnar sort hands its entire output up as one
/// batch, and building every tuple of it before releasing any would hold the
/// result set twice over, once as Arrow and once as tuples.
///
/// A batch with no rows yields no chunk rather than ending the stream — an empty
/// batch means "nothing here", not "nothing left", and a filter that rejects
/// everything in one batch produces exactly that.
///
/// `positions` names which batch columns actually carry values. A scan's batch
/// is full width, but the columns outside its [`ColumnProjection`] are all-NULL
/// padding that only exists so a schema ordinal is a batch ordinal. Decoding
/// those would make the per-row cost scale with the table's width instead of
/// with the query's — on a hundred-column relation read for two columns, fifty
/// times the work the row scan does. `decode_columns` leaves the slots it is not
/// given as `Null`, which is exactly the row scan's contract for an unprojected
/// column.
pub fn shred(child: BatchStream, layout: BatchLayout, positions: Vec<usize>) -> RowStream {
    // Only its `columns` are read; the relation name is never used.
    let schema = TableSchema::new("", layout.to_vec());
    Box::pin(try_stream! {
        let mut child = child;
        while let Some(batch) = child.next().await {
            let batch = batch?;
            let mut start = 0;
            while start < batch.num_rows() {
                let end = (start + ROW_CHUNK).min(batch.num_rows());
                let mut chunk: RowChunk = Vec::with_capacity(end - start);
                for row in start..end {
                    chunk.push(decode_columns(&schema, &positions, &batch, row)?);
                }
                yield chunk;
                start = end;
            }
        }
    })
}

/// [`shred`] for a batch every column of which carries a value — the shape an
/// operator that builds its own output columns (a projection, a sort) hands up.
pub fn shred_dense(child: BatchStream, layout: BatchLayout) -> RowStream {
    let positions = (0..layout.len()).collect();
    shred(child, layout, positions)
}
