use std::sync::Arc;

use arrow_array::RecordBatch;
use crabgresql_storage_api::{BatchStream, ColumnProjection, TableAm};
use crabgresql_txn::TxnContext;

use super::BatchNode;
use crate::tally::ScanTally;
use crate::{ExecContext, ExecError};

/// A full table scan that yields batches — [`crate::SeqScan`]'s columnar twin.
///
/// [`BatchScan::open`] returns `None` when the engine has no batch path, so a
/// caller always has the row scan to fall back on.
pub struct BatchScan {
    iter: BatchStream,
    /// What this scan reports to `pg_stat_all_tables` when it is dropped. The
    /// columnar path has to carry one too, or a relation read through it would
    /// show `seq_scan = 0` while an identical query against a row-store
    /// relation counted — and which path a relation takes is invisible above
    /// the shred.
    tally: Option<ScanTally>,
}

impl BatchScan {
    pub fn open(
        table: &Arc<dyn TableAm>,
        txn: &TxnContext,
        projection: &ColumnProjection,
        ctx: &ExecContext,
    ) -> Option<Self> {
        table.scan_batches(txn, projection).map(|iter| BatchScan {
            iter,
            tally: ScanTally::seq(ctx, table),
        })
    }
}

impl BatchNode for BatchScan {
    fn next_batch(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        let batch = self.iter.next().transpose().map_err(ExecError::from)?;
        if let (Some(batch), Some(tally)) = (&batch, &mut self.tally) {
            tally.saw(batch.num_rows() as u64);
        }
        Ok(batch)
    }
}
