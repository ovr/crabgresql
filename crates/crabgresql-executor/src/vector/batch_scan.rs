use std::sync::Arc;

use arrow_array::RecordBatch;
use crabgresql_storage_api::{BatchStream, ColumnProjection, TableAm};
use crabgresql_txn::TxnContext;

use super::BatchNode;
use crate::ExecError;

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
