//! Columnar projection and sort.
//!
//! These two go together. `project_pipeline` builds `Filter → Projection →
//! Sort`, and a [`SortKey`] indexes the *projected* tuple, so a columnar sort is
//! only reachable if the projection above the filter also stays columnar.
//! [`ProjectBatch`] exists to bridge that gap and nothing more: it reorders and
//! duplicates columns, it does not evaluate expressions.

use std::sync::Arc;

use arrow_array::{
    Array, ArrayRef, Float32Array, Float64Array, RecordBatch, RecordBatchOptions, UInt64Array,
};
use arrow_ord::sort::{SortColumn, SortOptions, lexsort_to_indices};
use arrow_schema::{Field, Schema};
use arrow_select::concat::concat_batches;
use arrow_select::take::take as take_kernel;
use crabgresql_binder::{BoundExpr, SortKey};
use crabgresql_planner::vectorize;
use crabgresql_storage_api::Column;
use crabgresql_storage_api::arrow::{build_array, scan_schema};

use super::{BatchLayout, BatchNode};
use crate::ExecError;

/// How one output column of a projection is produced.
pub enum Take {
    /// Column `n` of the input batch, unchanged.
    Column(usize),
    /// A length-1 array, broadcast to the batch's height.
    ///
    /// Built once at compile time rather than per batch. That is not only
    /// cheaper — it is what makes an unrepresentable constant *decline* instead
    /// of failing mid-scan: roughly half of `PgType` (json, inet, arrays, …)
    /// has no Arrow encoding, and a `Const` of such a type is perfectly legal
    /// in a target list even on a relation that could never store one.
    Const(ArrayRef),
}

/// Reorders, duplicates and drops columns — the take-only subset of
/// [`crate::Projection`].
///
/// Deliberately not an expression evaluator. Every projection that is anything
/// more than a column reference or a constant ends the columnar segment, so the
/// row `Projection` keeps sole responsibility for evaluating expressions and
/// there is no second implementation of any operator's semantics.
pub struct ProjectBatch {
    child: Box<dyn BatchNode>,
    takes: Vec<Take>,
    schema: Arc<Schema>,
}

impl ProjectBatch {
    /// Compile `projections` to a take list, or `None` if any of them computes
    /// or names a constant Arrow cannot hold.
    ///
    /// Gated on the planner's [`vectorize::vectorizable_projection`] first, so
    /// this can only ever accept a subset of what `EXPLAIN` advertises.
    pub fn compile(projections: &[BoundExpr], layout: &BatchLayout) -> Option<Vec<Take>> {
        if !vectorize::vectorizable_projection(projections, layout.len()) {
            return None;
        }
        projections
            .iter()
            .map(|expr| match unwrap_collate(expr) {
                BoundExpr::ColumnRef { index, .. } => {
                    (*index < layout.len()).then_some(Take::Column(*index))
                }
                // Built here, not per batch: `ok()?` turns a type with no Arrow
                // encoding into a declined projection rather than a query that
                // dies on its first batch.
                BoundExpr::Const { value, ty } => {
                    let column = Column::new("const", *ty);
                    let row = [vec![value.clone()]];
                    build_array(&column, &row, 0).ok().map(Take::Const)
                }
                _ => None,
            })
            .collect()
    }

    pub fn new(child: Box<dyn BatchNode>, takes: Vec<Take>, layout: &BatchLayout) -> Self {
        ProjectBatch {
            schema: scan_schema(&crabgresql_storage_api::TableSchema::new(
                "",
                layout.to_vec(),
            )),
            child,
            takes,
        }
    }

    /// The layout a projection produces, for the node above.
    pub fn layout(projections: &[BoundExpr]) -> BatchLayout {
        Arc::from(
            projections
                .iter()
                .enumerate()
                .map(|(index, expr)| Column::new(format!("c{index}"), expr.ty()))
                .collect::<Vec<_>>(),
        )
    }
}

impl BatchNode for ProjectBatch {
    fn next_batch(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        let Some(batch) = self.child.next_batch()? else {
            return Ok(None);
        };
        let rows = batch.num_rows();
        // Index 0 repeated: `take` broadcasts the length-1 constant array to the
        // batch's height in one allocation, the same trick `expr.rs` uses for a
        // predicate literal.
        let broadcast: UInt64Array = std::iter::repeat_n(0u64, rows).collect();
        let mut columns: Vec<ArrayRef> = Vec::with_capacity(self.takes.len());
        for take in &self.takes {
            columns.push(match take {
                Take::Column(index) => batch
                    .columns()
                    .get(*index)
                    .map(Arc::clone)
                    .ok_or_else(|| internal("projection names a missing column"))?,
                Take::Const(array) => take_kernel(array.as_ref(), &broadcast, None)
                    .map_err(|error| internal(&format!("constant broadcast failed: {error}")))?,
            });
        }
        let options = RecordBatchOptions::new().with_row_count(Some(rows));
        RecordBatch::try_new_with_options(Arc::clone(&self.schema), columns, &options)
            .map(Some)
            .map_err(|error| internal(&format!("projection failed: {error}")))
    }
}

/// Make a float column sort the way PostgreSQL does.
///
/// Two divergences, both real and both silent:
///
/// - PostgreSQL treats `-0.0` and `0.0` as **equal** (`float8_cmp`), while
///   Arrow's total order ranks `-0.0` below `0.0`.
/// - PostgreSQL treats all NaNs as one value, greater than everything. Arrow's
///   total order also puts NaN last, but orders distinct NaN *bit patterns*
///   against each other, so two NaNs that PostgreSQL calls equal would get a
///   defined relative order — and a stable sort would report it.
///
/// Mapping `-0.0` to `0.0` and every NaN to one canonical NaN makes Arrow's
/// total order coincide with PostgreSQL's exactly. Only the sort *key* is
/// rewritten; the value the query returns is taken from the untouched column.
fn canonicalize(array: &ArrayRef) -> ArrayRef {
    if let Some(values) = array.as_any().downcast_ref::<Float64Array>() {
        let fixed: Float64Array = values.unary(|v: f64| {
            if v.is_nan() {
                f64::NAN
            } else if v == 0.0 {
                0.0
            } else {
                v
            }
        });
        return Arc::new(fixed);
    }
    if let Some(values) = array.as_any().downcast_ref::<Float32Array>() {
        let fixed: Float32Array = values.unary(|v: f32| {
            if v.is_nan() {
                f32::NAN
            } else if v == 0.0 {
                0.0
            } else {
                v
            }
        });
        return Arc::new(fixed);
    }
    Arc::clone(array)
}

/// Materializing sort — the columnar [`crate::Sort`].
///
/// Same memory model as the row node: everything is buffered before the first
/// row comes out, so this changes the representation, not the contract.
///
/// **Stability.** The row `Sort` is a stable sort, which PostgreSQL's is too for
/// keys with no tiebreak. `lexsort_to_indices` is not stable, so a final
/// `UInt64` position key is appended: ties then resolve by input position, which
/// *is* stability, expressed as a comparison.
pub struct SortBatch {
    rows: Option<RecordBatch>,
    emitted: bool,
}

impl SortBatch {
    /// Whether every key can be sorted by Arrow with PostgreSQL's ordering.
    pub fn compilable(keys: &[SortKey], layout: &BatchLayout) -> bool {
        !keys.is_empty()
            && keys
                .iter()
                .all(|key| key.column < layout.len() && vectorize::sortable_key(key))
    }

    /// Drain `child`, sort it, and keep the result for [`BatchNode::next_batch`].
    ///
    /// `visible_width` drops the hidden ORDER BY columns the planner appended
    /// past the output, exactly as the row node's truncation does.
    pub fn new(
        mut child: Box<dyn BatchNode>,
        keys: &[SortKey],
        layout: &BatchLayout,
        visible_width: usize,
    ) -> Result<Self, ExecError> {
        let schema = scan_schema(&crabgresql_storage_api::TableSchema::new(
            "",
            layout.to_vec(),
        ));
        let mut batches = Vec::new();
        while let Some(batch) = child.next_batch()? {
            batches.push(batch);
        }
        let all = concat_batches(&schema, &batches)
            .map_err(|error| internal(&format!("sort concat failed: {error}")))?;
        // `concat_batches` copies, so the inputs are now a second full copy of
        // the relation. Release them before the take allocates a third — this
        // node already holds everything in memory, and holding it three times
        // over turns a sort that fit into one that does not.
        drop(batches);

        let mut columns: Vec<SortColumn> = keys
            .iter()
            .map(|key| SortColumn {
                values: canonicalize(all.column(key.column)),
                options: Some(SortOptions {
                    descending: !key.asc,
                    // PostgreSQL's NULLS FIRST/LAST is independent of ASC/DESC;
                    // Arrow's `nulls_first` is too, so it maps straight across.
                    nulls_first: key.nulls_first,
                }),
            })
            .collect();
        // The stability tiebreak. Ascending with no nulls, so it only ever
        // decides between rows every real key called equal.
        let positions: UInt64Array = (0..all.num_rows() as u64).collect::<Vec<_>>().into();
        columns.push(SortColumn {
            values: Arc::new(positions),
            options: Some(SortOptions {
                descending: false,
                nulls_first: false,
            }),
        });

        let indices = lexsort_to_indices(&columns, None)
            .map_err(|error| internal(&format!("sort failed: {error}")))?;
        let sorted: Vec<ArrayRef> = all
            .columns()
            .iter()
            .take(visible_width)
            .map(|column| take_kernel(column.as_ref(), &indices, None))
            .collect::<Result<_, _>>()
            .map_err(|error| internal(&format!("sort take failed: {error}")))?;

        let fields: Vec<Field> = schema
            .fields()
            .iter()
            .take(visible_width)
            .map(|field| field.as_ref().clone())
            .collect();
        let height = all.num_rows();
        // The sorted copy is complete; the unsorted one is dead weight now.
        drop(all);
        let options = RecordBatchOptions::new().with_row_count(Some(height));
        let rows =
            RecordBatch::try_new_with_options(Arc::new(Schema::new(fields)), sorted, &options)
                .map_err(|error| internal(&format!("sort rebuild failed: {error}")))?;
        Ok(SortBatch {
            rows: Some(rows),
            emitted: false,
        })
    }
}

impl BatchNode for SortBatch {
    fn next_batch(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        if self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        Ok(self.rows.take())
    }
}

fn unwrap_collate(expr: &BoundExpr) -> &BoundExpr {
    match expr {
        BoundExpr::Collate { expr, .. } => unwrap_collate(expr),
        other => other,
    }
}

fn internal(message: &str) -> ExecError {
    ExecError::new("XX000", message)
}
