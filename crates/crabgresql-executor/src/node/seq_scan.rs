use std::sync::Arc;

use crabgresql_storage_api::{ColumnProjection, StorageError, TableAm, Tuple};
use crabgresql_txn::TxnContext;

use crate::{ExecError, ExecNode};

/// Full table scan through the storage API.
pub struct SeqScan {
    iter: Box<dyn Iterator<Item = Result<Tuple, StorageError>> + Send>,
}

impl SeqScan {
    pub fn new(table: &Arc<dyn TableAm>, txn: &TxnContext, projection: &ColumnProjection) -> Self {
        Self {
            iter: Box::new(
                table
                    .scan(txn, projection)
                    .map(|row| row.map(|(_, tuple)| tuple)),
            ),
        }
    }
}

impl ExecNode for SeqScan {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        self.iter.next().transpose().map_err(ExecError::from)
    }
}
