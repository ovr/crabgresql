//! Columnar projection: the take-only subset of the row [`crate::Projection`].
//!
//! It exists for one reason — a [`SortKey`](crabgresql_binder::SortKey) indexes
//! the *projected* tuple, so `project_pipeline` can only keep a sort columnar if
//! the projection beneath it stays columnar too.

use std::sync::Arc;

use arrow_array::{ArrayRef, RecordBatch, RecordBatchOptions, UInt64Array};
use arrow_schema::Schema;
use arrow_select::take::take as take_kernel;
use crabgresql_binder::BoundExpr;
use crabgresql_planner::vectorize;
use crabgresql_storage_api::Column;
use crabgresql_storage_api::arrow::{build_array, scan_schema};

use super::{BatchLayout, BatchNode, internal};
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

fn unwrap_collate(expr: &BoundExpr) -> &BoundExpr {
    match expr {
        BoundExpr::Collate { expr, .. } => unwrap_collate(expr),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use crabgresql_types::{PgType, Value};

    use super::ProjectBatch;
    use crate::vector::layout_of;
    use crate::vector::testutil::{column, constant, schema_of};

    /// A constant whose type has no Arrow encoding must be *declined* at compile
    /// time, not discovered on the first batch. `SELECT id, '{}'::json FROM p ORDER
    /// BY id` is legal on a relation that could never store a json column, and it
    /// used to fail with a storage error where the heap answered normally.
    #[test]
    fn a_projection_constant_arrow_cannot_hold_is_declined() {
        let schema = schema_of(&[PgType::Int4]);
        let layout = layout_of(&schema);
        let id = column(0, PgType::Int4);

        for (value, ty) in [
            (Value::Json("{}".into()), PgType::Json),
            (Value::Oid(1), PgType::Oid),
            (Value::Money(1), PgType::Money),
        ] {
            let projections = vec![id.clone(), constant(value, ty)];
            assert!(
                ProjectBatch::compile(&projections, &layout).is_none(),
                "{ty:?} has no Arrow encoding and must not compile"
            );
        }
        // A representable constant still compiles, so the guard is not a blanket ban.
        let ok = vec![id, constant(Value::Int4(7), PgType::Int4)];
        assert!(ProjectBatch::compile(&ok, &layout).is_some());
    }
}
