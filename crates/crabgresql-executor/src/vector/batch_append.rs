use arrow_array::RecordBatch;
use crabgresql_planner::PhysicalAppendArm;
use crabgresql_txn::TxnContext;

use super::{BatchNode, BatchScan};
use crate::{ExecContext, ExecError};

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
    /// `None` — stay on the row path — if any arm cannot hand up batches, if
    /// any arm carries a column remap, or if any arm must append a `tableoid`.
    /// A batch is in its own relation's column order and there is nowhere here
    /// to permute one, so a remapped arm would concatenate mis-ordered columns
    /// rather than fail loudly.
    ///
    /// The remap branch is unreachable today: DDL refuses an engine-managed
    /// relation on either side of an inheritance link, so no remapped arm can be
    /// batch-capable. The planner's `arms_batch` applies that same remap rule,
    /// so a remapped arm never makes `EXPLAIN` disagree with what runs.
    ///
    /// TODO: hand up batches for an arm that must append a `tableoid` slot.
    /// `arms_batch` does not test for that slot, so until then `EXPLAIN` calls
    /// such an `Append` columnar while it runs on rows.
    pub fn open(arms: &[PhysicalAppendArm], txn: &TxnContext, ctx: &ExecContext) -> Option<Self> {
        let children = arms
            .iter()
            .map(|arm| {
                // A remapped arm, or one that must append a `tableoid`, changes
                // the row shape the batch layout describes.
                if arm.relation.map.is_some() || arm.relation.tableoid.is_some() {
                    return None;
                }
                BatchScan::open(&arm.relation.table, txn, &arm.projection, ctx)
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
