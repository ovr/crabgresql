use std::sync::Arc;

use arrow_array::{ArrayRef, RecordBatch, RecordBatchOptions, UInt32Array};
use arrow_schema::{DataType, Field, Schema};
use crabgresql_planner::PhysicalAppendArm;
use crabgresql_txn::TxnContext;

use super::{BatchNode, BatchScan};
use crate::{ExecContext, ExecError, resolve_tableoid};

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
    /// `None` — stay on the row path — if any arm cannot hand up batches, if any
    /// arm carries a column remap, or if any arm must append a system column
    /// other than `tableoid`. A batch is in its own relation's column order and
    /// there is nowhere here to permute one, so a remapped arm would concatenate
    /// mis-ordered columns rather than fail loudly.
    ///
    /// The remap branch is unreachable today: DDL refuses an engine-managed
    /// relation on either side of an inheritance link, so no remapped arm can be
    /// batch-capable. The planner asks the *same* predicate
    /// ([`SystemEmit::is_batchable`](crabgresql_binder::SystemEmit::is_batchable))
    /// for its `EXPLAIN` annotation, so neither a remapped arm nor a system slot
    /// can make the annotation disagree with what runs.
    ///
    /// **`tableoid` is the only system column reachable here**, and not by
    /// choice: the access methods that hand up batches are exactly the columnar
    /// ones, and the binder refuses every other system column on those (they
    /// keep no row versions). A [`BatchStream`] also carries no
    /// [`Tid`](crabgresql_storage_api::Tid) — deliberately, see its docs — so
    /// even `ctid` would have nothing to read. `tableoid` is a fact about the
    /// relation, so it appends as a constant column and costs the batch path
    /// nothing.
    ///
    /// [`arms_batch`]: crabgresql_planner::PhysicalPlan::vectorization
    /// [`BatchStream`]: crabgresql_storage_api::BatchStream
    pub fn open(arms: &[PhysicalAppendArm], txn: &TxnContext, ctx: &ExecContext) -> Option<Self> {
        let children = arms
            .iter()
            .map(|arm| {
                // A remapped arm changes the row shape the batch layout
                // describes; a system slot this cannot synthesize does too.
                if arm.relation.map.is_some()
                    || arm
                        .relation
                        .system
                        .as_ref()
                        .is_some_and(|e| !e.is_batchable())
                {
                    return None;
                }
                let scan = BatchScan::open(&arm.relation.table, txn, &arm.projection, ctx)?;
                match &arm.relation.system {
                    None => Some(Box::new(scan) as Box<dyn BatchNode>),
                    // Resolved once per arm, exactly as the row path does it:
                    // the catalog snapshot is fixed within one statement, so a
                    // prepared statement never freezes a positional OID.
                    Some(emit) => {
                        let oid = resolve_tableoid(&emit.ident, ctx).ok()?;
                        Some(Box::new(WithTableOid { scan, oid }) as Box<dyn BatchNode>)
                    }
                }
            })
            .collect::<Option<Vec<_>>>()?;
        Some(BatchAppend {
            children,
            cursor: 0,
        })
    }
}

/// A batch source with a constant `tableoid` column appended — the columnar
/// twin of the row path's `push_system`.
struct WithTableOid<S> {
    scan: S,
    oid: u32,
}

impl<S: BatchNode> BatchNode for WithTableOid<S> {
    fn next_batch(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        let Some(batch) = self.scan.next_batch()? else {
            return Ok(None);
        };
        let rows = batch.num_rows();
        let mut fields: Vec<Field> = batch
            .schema()
            .fields()
            .iter()
            .map(|f| (**f).clone())
            .collect();
        fields.push(Field::new("tableoid", DataType::UInt32, false));
        let mut columns = batch.columns().to_vec();
        columns.push(Arc::new(UInt32Array::from(vec![self.oid; rows])) as ArrayRef);
        // `try_new_with_options` rather than `try_new`: a batch of zero rows
        // carries its width in the schema alone, and the plain constructor
        // cannot infer a row count from it.
        RecordBatch::try_new_with_options(
            Arc::new(Schema::new(fields)),
            columns,
            &RecordBatchOptions::new().with_row_count(Some(rows)),
        )
        .map(Some)
        .map_err(|error| {
            ExecError::new(
                crabgresql_pg_wire::sqlstate::INTERNAL_ERROR,
                format!("appending a tableoid column to a batch: {error}"),
            )
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
