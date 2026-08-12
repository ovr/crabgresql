use crabgresql_planner::PhysicalAppendArm;
use crabgresql_storage_api::{StorageError, Tuple};
use crabgresql_txn::TxnContext;
use crabgresql_types::Value;

use crate::{ExecContext, ExecError, ExecNode, resolve_tableoid};

/// Union scan over the relations one FROM item named: concatenates each arm's
/// snapshot scan into one row stream, in arm order (see
/// [`PhysicalPlan::Append`](crabgresql_planner::PhysicalPlan::Append)). Each arm
/// captures its own MVCC snapshot up front, exactly as [`SeqScan`](crate::SeqScan) does.
pub struct Append {
    iter: Box<dyn Iterator<Item = Result<Tuple, StorageError>> + Send>,
}

impl Append {
    pub fn new(
        arms: &[PhysicalAppendArm],
        ctx: &ExecContext,
        txn: &TxnContext,
    ) -> Result<Self, ExecError> {
        let mut scans: Vec<Box<dyn Iterator<Item = Result<Tuple, StorageError>> + Send>> =
            Vec::with_capacity(arms.len());
        for arm in arms {
            let scan = arm
                .relation
                .table
                .scan(txn, &arm.projection)
                .map(|row| row.map(|(_, tuple)| tuple));
            let scan: Box<dyn Iterator<Item = Result<Tuple, StorageError>> + Send> = match &arm
                .relation
                .map
            {
                // The arm already carries the named relation's layout, so
                // its tuples pass through untouched.
                None => Box::new(scan),
                // A wider arm reads through the same `view` the write paths
                // use. The scan's projection was translated through this
                // same map, so a slot the projection skipped is a
                // placeholder in both spaces and stays one here.
                Some(_) => {
                    let relation = arm.relation.clone();
                    Box::new(
                        scan.map(move |row| row.map(|tuple| relation.view(&tuple).into_owned())),
                    )
                }
            };
            scans.push(match &arm.relation.tableoid {
                None => scan,
                // Resolved once here rather than per row: within one statement
                // the catalog snapshot is fixed, so the OID cannot change under
                // the scan — and leaving it to execution rather than to binding
                // is what keeps a prepared statement honest when a relation is
                // created or dropped ahead of this one.
                //
                // Appended *after* `view`, so `map` stays a pure gather and the
                // write-back paths never meet a column with nowhere to write.
                Some(ident) => {
                    let oid = resolve_tableoid(ident, ctx)?;
                    Box::new(scan.map(move |row| {
                        row.map(|mut tuple| {
                            tuple.push(Value::Oid(oid));
                            tuple
                        })
                    }))
                }
            });
        }
        Ok(Self {
            iter: Box::new(scans.into_iter().flatten()),
        })
    }
}

impl ExecNode for Append {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        self.iter.next().transpose().map_err(ExecError::from)
    }
}
