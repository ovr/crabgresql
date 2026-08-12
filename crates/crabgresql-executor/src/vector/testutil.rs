//! Fixtures for the columnar nodes' equivalence tests: a columnar node must
//! produce exactly what the row node it stands in for produces, for the same
//! input.
//!
//! That is the only property that matters here. A columnar node is chosen
//! automatically and is invisible in the result, so any divergence is a
//! wrong-answer bug that nothing else in the system would catch. The two
//! `assert_same*` helpers are how every such test states it.

use std::sync::Arc;

use arrow_array::RecordBatch;
use crabgresql_binder::{BinOp, BoundExpr, SortKey};
use crabgresql_storage_api::arrow::build_scan_batch;
use crabgresql_storage_api::{Column, TableSchema, Tuple};
use crabgresql_types::collation::DEFAULT_COLLATION_OID;
use crabgresql_types::{PgType, Value};

use super::{BatchLayout, BatchNode, FilterBatch, ProjectBatch, Shred, SortBatch, expr, layout_of};
use crate::{ExecContext, ExecError, ExecNode, MaterializedRows, Sort as RowSort, predicate_holds};

/// A batch source over a fixed list of batches.
pub(super) struct Batches(pub(super) std::vec::IntoIter<RecordBatch>);

impl BatchNode for Batches {
    fn next_batch(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        Ok(self.0.next())
    }
}

pub(super) fn schema_of(types: &[PgType]) -> TableSchema {
    TableSchema::new(
        "t",
        types
            .iter()
            .enumerate()
            .map(|(index, ty)| Column::new(format!("c{index}"), *ty))
            .collect(),
    )
}

/// Run `rows` through the columnar filter, split across two batches so a
/// multi-batch stream is always exercised.
pub(super) fn columnar_filter(
    schema: &TableSchema,
    rows: &[Tuple],
    predicate: &BoundExpr,
) -> Result<Vec<Tuple>, ExecError> {
    let layout = layout_of(schema);
    let compiled = expr::compile_predicate(predicate, &layout)
        .ok_or_else(|| ExecError::new("XX000", "predicate did not compile"))?;

    let split = rows.len() / 2;
    let batches = vec![
        build_scan_batch(schema, &rows[..split])?,
        build_scan_batch(schema, &rows[split..])?,
    ];
    let filtered = FilterBatch::new(Box::new(Batches(batches.into_iter())), compiled);

    let mut node = Shred::dense(Box::new(filtered), layout);
    let mut out = Vec::new();
    while let Some(row) = node.next()? {
        out.push(row);
    }
    Ok(out)
}

/// Run the same rows through the row `Filter`'s own truth test.
fn row_filter(rows: &[Tuple], predicate: &BoundExpr) -> Result<Vec<Tuple>, ExecError> {
    let ctx = ExecContext::default();
    let mut out = Vec::new();
    for row in rows {
        if predicate_holds(&Some(predicate.clone()), row, &ctx)? {
            out.push(row.clone());
        }
    }
    Ok(out)
}

/// The assertion the filter tests make: both paths, same answer.
pub(super) fn assert_same(schema: &TableSchema, rows: &[Tuple], predicate: &BoundExpr) {
    let columnar = columnar_filter(schema, rows, predicate).expect("columnar filter");
    let row = row_filter(rows, predicate).expect("row filter");
    assert_eq!(columnar, row, "columnar and row filters disagree");
}

pub(super) fn column(index: usize, ty: PgType) -> BoundExpr {
    BoundExpr::ColumnRef { index, ty }
}

pub(super) fn constant(value: Value, ty: PgType) -> BoundExpr {
    BoundExpr::Const { value, ty }
}

pub(super) fn compare(op: BinOp, ty: PgType, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr::Binary {
        op,
        arg_ty: ty,
        collation: DEFAULT_COLLATION_OID,
        left: Box::new(left),
        right: Box::new(right),
    }
}

pub(super) fn logic(op: BinOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr::Binary {
        op,
        arg_ty: PgType::Bool,
        collation: DEFAULT_COLLATION_OID,
        left: Box::new(left),
        right: Box::new(right),
    }
}

pub(super) fn int_rows() -> Vec<Tuple> {
    [
        Some(-3),
        Some(0),
        Some(1),
        None,
        Some(7),
        Some(42),
        None,
        Some(1),
    ]
    .into_iter()
    .map(|v| vec![v.map_or(Value::Null, Value::Int4)])
    .collect()
}

pub(super) fn sort_key(column: usize, ty: PgType, asc: bool, nulls_first: bool) -> SortKey {
    SortKey {
        column,
        ty,
        collation: DEFAULT_COLLATION_OID,
        asc,
        nulls_first,
    }
}

/// Sort `rows` columnar-side: an identity projection, then `SortBatch`, then a
/// shred. Split into two batches so a multi-batch input is always exercised.
pub(super) fn columnar_sort(
    schema: &TableSchema,
    rows: &[Tuple],
    keys: &[SortKey],
    visible_width: usize,
) -> Result<Vec<Tuple>, ExecError> {
    let layout = layout_of(schema);
    let identity: Vec<BoundExpr> = schema
        .columns
        .iter()
        .enumerate()
        .map(|(index, c)| column(index, c.ty))
        .collect();
    let takes = ProjectBatch::compile(&identity, &layout).expect("identity projection compiles");
    let projected = ProjectBatch::layout(&identity);

    let split = rows.len() / 2;
    let batches = vec![
        build_scan_batch(schema, &rows[..split])?,
        build_scan_batch(schema, &rows[split..])?,
    ];
    let project = ProjectBatch::new(Box::new(Batches(batches.into_iter())), takes, &projected);
    let sorted = SortBatch::new(Box::new(project), keys, &projected, visible_width)?;

    let visible: BatchLayout = Arc::from(&projected[..visible_width]);
    let mut node = Shred::dense(Box::new(sorted), visible);
    let mut out = Vec::new();
    while let Some(row) = node.next()? {
        out.push(row);
    }
    Ok(out)
}

/// Sort the same rows with the row `Sort`, the node this stands in for.
fn row_sort(
    rows: &[Tuple],
    keys: &[SortKey],
    visible_width: usize,
) -> Result<Vec<Tuple>, ExecError> {
    let source = MaterializedRows::new(rows.to_vec());
    let mut node = RowSort::new(Box::new(source), keys.to_vec(), visible_width)?;
    let mut out = Vec::new();
    while let Some(row) = node.next()? {
        out.push(row);
    }
    Ok(out)
}

pub(super) fn assert_same_order(
    schema: &TableSchema,
    rows: &[Tuple],
    keys: &[SortKey],
    width: usize,
) {
    let columnar = columnar_sort(schema, rows, keys, width).expect("columnar sort");
    let row = row_sort(rows, keys, width).expect("row sort");
    // Compared as rendered text, not with `==`. `Value`'s derived `PartialEq`
    // is IEEE for floats, so `Float8(NaN) == Float8(NaN)` is false and two
    // identical NaN-bearing results would compare unequal — the executor uses
    // `compare_values` for equality precisely because of this. Rendering
    // sidesteps it while still comparing every value in order.
    assert_eq!(
        format!("{columnar:?}"),
        format!("{row:?}"),
        "columnar and row sorts disagree"
    );
}
