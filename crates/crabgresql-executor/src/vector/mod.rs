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

pub mod expr;
pub mod sort;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use arrow_array::RecordBatch;
use arrow_select::filter::filter_record_batch;
use crabgresql_planner::PhysicalAppendArm;
use crabgresql_storage_api::arrow::decode_columns;
use crabgresql_storage_api::{BatchStream, Column, ColumnProjection, TableAm, TableSchema};
use crabgresql_txn::TxnContext;

use crate::{ExecError, ExecNode, Tuple};

/// A columnar execution node: `next_batch()` pulls many rows at a time.
///
/// The columnar twin of [`ExecNode`], and deliberately the same shape — pull
/// based, `Send`, no lifetime — so a node can be suspended inside a portal and
/// resumed across `Execute` round trips just as a row node can.
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

/// A full table scan that yields batches — [`crate::SeqScan`]'s columnar twin.
///
/// [`BatchScan::open`] returns `None` when the engine has no batch path, so a
/// caller always has the row scan to fall back on.
pub struct BatchScan {
    iter: BatchStream,
}

impl BatchScan {
    pub fn open(
        table: &Arc<dyn TableAm>,
        txn: &TxnContext,
        projection: &ColumnProjection,
    ) -> Option<Self> {
        table
            .scan_batches(txn, projection)
            .map(|iter| BatchScan { iter })
    }
}

impl BatchNode for BatchScan {
    fn next_batch(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        self.iter.next().transpose().map_err(ExecError::from)
    }
}

/// Concatenates several batch sources — the columnar [`crate::Append`].
///
/// A Parquet relation is always read through this: it plans as an `Append` over
/// its chunk store and its RAM write buffer. Without a columnar Append the
/// batches would be shredded at the leaves and nothing above could vectorize,
/// so this is load-bearing rather than an extra.
///
/// [`BatchAppend::open`] is all-or-nothing: one row-only arm puts the whole
/// relation back on the row path, because the arms' outputs are concatenated
/// and must share one representation.
pub struct BatchAppend {
    children: Vec<Box<dyn BatchNode>>,
    cursor: usize,
}

impl BatchAppend {
    /// `None` — stay on the row path — if any arm cannot hand up batches, or if
    /// any arm carries a column remap. A batch is in its own relation's column
    /// order and there is nowhere here to permute one, so a remapped arm would
    /// concatenate mis-ordered columns rather than fail loudly. No arm that
    /// remaps can produce batches today (an inheritance child is a heap
    /// relation), so the check is a guard for the day one can; the planner's
    /// `arms_batch` makes the same call so `EXPLAIN` agrees.
    pub fn open(arms: &[PhysicalAppendArm], txn: &TxnContext) -> Option<Self> {
        let children = arms
            .iter()
            .map(|arm| {
                if arm.relation.map.is_some() {
                    return None;
                }
                BatchScan::open(&arm.relation.table, txn, &arm.projection)
                    .map(|scan| Box::new(scan) as Box<dyn BatchNode>)
            })
            .collect::<Option<Vec<_>>>()?;
        Some(BatchAppend {
            children,
            cursor: 0,
        })
    }
}

impl BatchNode for BatchAppend {
    fn next_batch(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        while self.cursor < self.children.len() {
            if let Some(batch) = self.children[self.cursor].next_batch()? {
                return Ok(Some(batch));
            }
            self.cursor += 1;
        }
        Ok(None)
    }
}

/// Drops the rows of each batch that fail a predicate — the columnar
/// [`crate::Filter`].
///
/// This is the operator that makes vectorizing pay. [`Shred`] costs one tuple
/// build per surviving row, so filtering *below* the shred is what turns a
/// selective `WHERE` into work the row executor never does at all.
///
/// A batch that loses every row is passed on empty rather than skipped; `Shred`
/// treats an empty batch as "nothing here", not "nothing left".
pub struct FilterBatch {
    child: Box<dyn BatchNode>,
    predicate: expr::VectorPredicate,
}

impl FilterBatch {
    pub fn new(child: Box<dyn BatchNode>, predicate: expr::VectorPredicate) -> Self {
        FilterBatch { child, predicate }
    }
}

impl BatchNode for FilterBatch {
    fn next_batch(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        let Some(batch) = self.child.next_batch()? else {
            return Ok(None);
        };
        let mask = self.predicate.evaluate(&batch)?;
        // `filter_record_batch` keeps only `true`; `false` and NULL both drop,
        // which is SQL's rule for a `WHERE` and matches `predicate_holds`.
        filter_record_batch(&batch, &mask)
            .map(Some)
            .map_err(|error| ExecError::new("XX000", format!("vectorized filter failed: {error}")))
    }
}

/// Turns a batch stream back into a tuple stream — the boundary where a
/// columnar segment ends and the row executor resumes.
///
/// Every vectorized plan has exactly one of these per segment. It is pure cost
/// (the work the columnar nodes below it saved has to be paid back for the rows
/// that survive), which is the whole argument for pushing selective operators
/// like a filter *below* it: fewer surviving rows, less to shred.
///
/// A batch with no rows is skipped rather than ending the stream — an empty
/// batch means "nothing here", not "nothing left", and a filter that rejects
/// everything in one batch produces exactly that.
pub struct Shred {
    child: Box<dyn BatchNode>,
    /// The batch's column types, in the shape [`decode_columns`] takes. Only
    /// its `columns` are read; the relation name is never used.
    schema: TableSchema,
    /// Which batch columns actually carry values.
    ///
    /// A scan's batch is full width, but the columns outside its
    /// [`ColumnProjection`] are all-NULL padding that only exists so a schema
    /// ordinal is a batch ordinal. Decoding those would make the per-row cost
    /// scale with the table's width instead of with the query's — on a
    /// hundred-column relation read for two columns, fifty times the work the
    /// row scan does. `decode_columns` leaves the slots it is not given as `Null`,
    /// which is exactly the row scan's contract for an unprojected column.
    positions: Vec<usize>,
    batch: Option<RecordBatch>,
    row: usize,
}

impl Shred {
    pub fn new(child: Box<dyn BatchNode>, layout: BatchLayout, positions: Vec<usize>) -> Self {
        Shred {
            child,
            schema: TableSchema::new("", layout.to_vec()),
            positions,
            batch: None,
            row: 0,
        }
    }

    /// Every column of the batch carries a value — the shape an operator that
    /// builds its own output columns (a projection, a sort) hands up.
    pub fn dense(child: Box<dyn BatchNode>, layout: BatchLayout) -> Self {
        let positions = (0..layout.len()).collect();
        Shred::new(child, layout, positions)
    }
}

impl ExecNode for Shred {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        loop {
            if let Some(batch) = &self.batch
                && self.row < batch.num_rows()
            {
                let row = self.row;
                self.row += 1;
                return decode_columns(&self.schema, &self.positions, batch, row)
                    .map(Some)
                    .map_err(ExecError::from);
            }
            match self.child.next_batch()? {
                Some(batch) => {
                    self.batch = Some(batch);
                    self.row = 0;
                }
                None => return Ok(None),
            }
        }
    }
}
