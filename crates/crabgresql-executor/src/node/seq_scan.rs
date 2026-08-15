use std::sync::Arc;

use crabgresql_storage_api::{ColumnProjection, StorageError, TableAm, Tuple};
use crabgresql_txn::TxnContext;

use crate::tally::ScanTally;
use crate::{ExecContext, ExecError, ExecNode};

/// Full table scan through the storage API.
pub struct SeqScan {
    iter: Box<dyn Iterator<Item = Result<Tuple, StorageError>> + Send>,
    /// What this scan reports to `pg_stat_all_tables` when it is dropped. See
    /// [`ScanTally`] for why counting here costs nothing per row.
    tally: Option<ScanTally>,
}

impl SeqScan {
    pub fn new(
        table: &Arc<dyn TableAm>,
        txn: &TxnContext,
        projection: &ColumnProjection,
        ctx: &ExecContext,
    ) -> Self {
        Self {
            iter: Box::new(
                table
                    .scan(txn, projection)
                    .map(|row| row.map(|(_, tuple)| tuple)),
            ),
            tally: ScanTally::seq(ctx, table),
        }
    }
}

impl ExecNode for SeqScan {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        let row = self.iter.next().transpose().map_err(ExecError::from)?;
        if row.is_some()
            && let Some(tally) = &mut self.tally
        {
            tally.saw(1);
        }
        Ok(row)
    }
}
