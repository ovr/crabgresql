//! Columnar projection and sort.
//!
//! These two go together. `project_pipeline` builds `Filter → Projection →
//! Sort`, and a [`SortKey`] indexes the *projected* tuple, so a columnar sort is
//! only reachable if the projection above the filter also stays columnar.
//! [`ProjectBatch`] exists to bridge that gap and nothing more: it reorders and
//! duplicates columns, it does not evaluate expressions.

use std::sync::Arc;

use arrow_array::{ArrayRef, RecordBatch, RecordBatchOptions, UInt64Array};
use arrow_schema::{Field, Schema};
use arrow_select::concat::concat_batches;
use arrow_select::take::take as take_kernel;
use async_stream::try_stream;
use crabgresql_binder::{BoundExpr, SortKey};
use crabgresql_planner::vectorize;
use crabgresql_storage_api::arrow::{build_array, scan_schema};
use crabgresql_storage_api::{Column, IndexKey, sort};
use futures_util::StreamExt;

use super::BatchLayout;
use crate::ExecError;
use crate::stream::BatchStream;

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
/// [`crate::projection`].
///
/// Deliberately not an expression evaluator. Every projection that is anything
/// more than a column reference or a constant ends the columnar segment, so the
/// row projection keeps sole responsibility for evaluating expressions and
/// there is no second implementation of any operator's semantics.
pub struct ProjectBatch;

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

/// Apply a compiled take list to every batch. `layout` is the layout the
/// projection *produces* — [`ProjectBatch::layout`] — since that is the schema
/// the rebuilt batches carry.
pub fn project_batches(child: BatchStream, takes: Vec<Take>, layout: &BatchLayout) -> BatchStream {
    let schema = scan_schema(&crabgresql_storage_api::TableSchema::new(
        "",
        layout.to_vec(),
    ));
    Box::pin(try_stream! {
        let mut child = child;
        while let Some(batch) = child.next().await {
            let batch = batch?;
            let rows = batch.num_rows();
            // Index 0 repeated: `take` broadcasts the length-1 constant array to
            // the batch's height in one allocation, the same trick `expr.rs`
            // uses for a predicate literal.
            let broadcast: UInt64Array = std::iter::repeat_n(0u64, rows).collect();
            let mut columns: Vec<ArrayRef> = Vec::with_capacity(takes.len());
            for take in &takes {
                columns.push(match take {
                    Take::Column(index) => batch
                        .columns()
                        .get(*index)
                        .map(Arc::clone)
                        .ok_or_else(|| internal("projection names a missing column"))?,
                    Take::Const(array) => {
                        take_kernel(array.as_ref(), &broadcast, None).map_err(|error| {
                            internal(&format!("constant broadcast failed: {error}"))
                        })?
                    }
                });
            }
            let options = RecordBatchOptions::new().with_row_count(Some(rows));
            yield RecordBatch::try_new_with_options(Arc::clone(&schema), columns, &options)
                .map_err(|error| internal(&format!("projection failed: {error}")))?;
        }
    })
}

/// Whether every key can be sorted by Arrow with PostgreSQL's ordering.
pub fn sortable(keys: &[SortKey], layout: &BatchLayout) -> bool {
    !keys.is_empty()
        && keys
            .iter()
            .all(|key| key.column < layout.len() && vectorize::sortable_key(key))
}

/// Materializing sort — the columnar [`crate::sort`].
///
/// Same memory model as the row node: everything is buffered before the first
/// row comes out, so this changes the representation, not the contract. The
/// buffering happens on the first poll rather than when the plan is built, which
/// is what makes it a stream at all — an unpulled result set does no work.
///
/// The ordering itself belongs to [`crabgresql_storage_api::sort`], which the
/// columnar write path shares: PostgreSQL's `-0.0`/NaN key rewrite and the
/// stability tiebreak are stated once, there.
///
/// `visible_width` drops the hidden ORDER BY columns the planner appended past
/// the output, exactly as the row node's truncation does.
pub fn sort_batches(
    child: BatchStream,
    keys: &[SortKey],
    layout: &BatchLayout,
    visible_width: usize,
) -> BatchStream {
    let schema = scan_schema(&crabgresql_storage_api::TableSchema::new(
        "",
        layout.to_vec(),
    ));
    // A `SortKey` is an `IndexKey` with the direction spelled the other way
    // round; everything else about the two is the same column-and-flags triple
    // the sort kernel wants.
    let index_keys: Vec<IndexKey> = keys
        .iter()
        .map(|key| IndexKey {
            column: key.column,
            descending: !key.asc,
            nulls_first: key.nulls_first,
        })
        .collect();
    Box::pin(try_stream! {
        let mut child = child;
        let mut batches = Vec::new();
        while let Some(batch) = child.next().await {
            batches.push(batch?);
        }
        let all = concat_batches(&schema, &batches)
            .map_err(|error| internal(&format!("sort concat failed: {error}")))?;
        // `concat_batches` copies, so the inputs are now a second full copy of
        // the relation. Release them before the take allocates a third — this
        // node already holds everything in memory, and holding it three times
        // over turns a sort that fit into one that does not.
        drop(batches);

        let indices = sort::sort_permutation(&all, &index_keys)?;
        let sorted = sort::take_columns(&all, &indices, visible_width)?;

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
        yield RecordBatch::try_new_with_options(Arc::new(Schema::new(fields)), sorted, &options)
            .map_err(|error| internal(&format!("sort rebuild failed: {error}")))?;
    })
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
