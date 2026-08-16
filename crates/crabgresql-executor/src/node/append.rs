use std::sync::Arc;

use crabgresql_binder::SysCol;
use crabgresql_planner::PhysicalAppendArm;
use crabgresql_storage_api::{StorageError, Tuple};
use crabgresql_txn::TxnContext;

use crate::{ExecContext, ExecError, ExecNode, SystemRow, push_system, resolve_tableoid};

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
            // Three shapes of source, decided by what the arm has to emit:
            //
            //  - nothing system: the plain scan, tid dropped as before;
            //  - `ctid` (and/or `tableoid`) only: the plain scan again, but the
            //    tid it already yields is kept instead of discarded;
            //  - anything reading the MVCC header: `scan_with_system`, which
            //    the binder has already checked this access method provides.
            let emit = arm.relation.system.as_ref();
            let cols: &[SysCol] = emit.map_or(&[], |e| &e.cols);
            let oid = match emit.filter(|_| cols.contains(&SysCol::TableOid)) {
                // Resolved once here rather than per row: within one statement
                // the catalog snapshot is fixed, so the OID cannot change under
                // the scan — and leaving it to execution rather than to binding
                // is what keeps a prepared statement honest when a relation is
                // created or dropped ahead of this one.
                Some(emit) => Some(resolve_tableoid(&emit.ident, ctx)?),
                None => None,
            };
            let rows: Box<dyn Iterator<Item = Result<SystemRow, StorageError>> + Send> =
                match cols.iter().any(|c| c.needs_header()) {
                    false => Box::new(
                        arm.relation
                            .table
                            .scan(txn, &arm.projection)
                            .map(|row| row.map(|(tid, tuple)| (tid, None, tuple))),
                    ),
                    true => Box::new(
                        arm.relation
                            .table
                            .scan_with_system(txn, &arm.projection)
                            .expect(
                                "the binder rejects a system column the access method declines, \
                                 so an arm reaching here can produce a header",
                            )
                            .map(|row| row.map(|(tid, hdr, tuple)| (tid, Some(hdr), tuple))),
                    ),
                };
            let map = arm.relation.map.is_some().then(|| arm.relation.clone());
            let cols: Arc<[SysCol]> = Arc::from(cols);
            scans.push(Box::new(rows.map(move |row| {
                row.map(|(tid, hdr, tuple)| {
                    // A wider arm reads through the same `view` the write paths
                    // use. The scan's projection was translated through this
                    // same map, so a slot the projection skipped is a
                    // placeholder in both spaces and stays one here.
                    let mut tuple = match &map {
                        None => tuple,
                        Some(relation) => relation.view(&tuple).into_owned(),
                    };
                    // Appended *after* `view`, so `map` stays a pure gather and
                    // the write-back paths never meet a column with nowhere to
                    // write.
                    push_system(&mut tuple, &cols, oid, tid, hdr.as_ref());
                    tuple
                })
            })));
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
