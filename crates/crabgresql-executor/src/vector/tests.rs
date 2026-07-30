//! Equivalence tests: a columnar node must produce exactly what the row node
//! it stands in for produces, for the same input.
//!
//! That is the only property that matters here. A columnar node is chosen
//! automatically and is invisible in the result, so any divergence is a
//! wrong-answer bug that nothing else in the system would catch.

use std::sync::Arc;

use arrow_array::RecordBatch;
use crabgresql_binder::{BinOp, BoundExpr, UnaryOp};
use crabgresql_storage_api::arrow::build_scan_batch;
use crabgresql_storage_api::{Column, TableSchema, Tuple};
use crabgresql_types::collation::{C_COLLATION_OID, DEFAULT_COLLATION_OID};
use crabgresql_types::{PgType, Value};

use super::{BatchLayout, BatchNode, FilterBatch, Shred, expr};
use crate::{ExecContext, ExecError, ExecNode, predicate_holds};

/// A batch source over a fixed list of batches.
struct Batches(std::vec::IntoIter<RecordBatch>);

impl BatchNode for Batches {
    fn next_batch(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        Ok(self.0.next())
    }
}

fn schema_of(types: &[PgType]) -> TableSchema {
    TableSchema::new(
        "t",
        types
            .iter()
            .enumerate()
            .map(|(index, ty)| Column::new(format!("c{index}"), *ty))
            .collect(),
    )
}

fn layout_of(schema: &TableSchema) -> BatchLayout {
    Arc::from(schema.columns.clone())
}

/// Run `rows` through the columnar filter, split across two batches so a
/// multi-batch stream is always exercised.
fn columnar_filter(
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

    let mut node = Shred::new(Box::new(filtered), layout);
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

/// The assertion every test here makes: both paths, same answer.
fn assert_same(schema: &TableSchema, rows: &[Tuple], predicate: &BoundExpr) {
    let columnar = columnar_filter(schema, rows, predicate).expect("columnar filter");
    let row = row_filter(rows, predicate).expect("row filter");
    assert_eq!(columnar, row, "columnar and row filters disagree");
}

fn column(index: usize, ty: PgType) -> BoundExpr {
    BoundExpr::ColumnRef { index, ty }
}

fn constant(value: Value, ty: PgType) -> BoundExpr {
    BoundExpr::Const { value, ty }
}

fn compare(op: BinOp, ty: PgType, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr::Binary {
        op,
        arg_ty: ty,
        collation: DEFAULT_COLLATION_OID,
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn logic(op: BinOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr::Binary {
        op,
        arg_ty: PgType::Bool,
        collation: DEFAULT_COLLATION_OID,
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn int_rows() -> Vec<Tuple> {
    [Some(-3), Some(0), Some(1), None, Some(7), Some(42), None, Some(1)]
        .into_iter()
        .map(|v| vec![v.map_or(Value::Null, Value::Int4)])
        .collect()
}

#[test]
fn every_comparison_operator_agrees_with_the_row_filter() {
    let schema = schema_of(&[PgType::Int4]);
    let rows = int_rows();
    for op in [
        BinOp::Eq,
        BinOp::NotEq,
        BinOp::Lt,
        BinOp::LtEq,
        BinOp::Gt,
        BinOp::GtEq,
    ] {
        assert_same(
            &schema,
            &rows,
            &compare(
                op,
                PgType::Int4,
                column(0, PgType::Int4),
                constant(Value::Int4(1), PgType::Int4),
            ),
        );
    }
}

/// The Kleene case. `false AND NULL` is `false` (drop) and `true OR NULL` is
/// `true` (keep); Arrow's plain `and`/`or` would return NULL for both, so a
/// row that should survive an `OR` would be silently dropped.
#[test]
fn and_or_follow_three_valued_logic() {
    let schema = schema_of(&[PgType::Int4, PgType::Bool]);
    let rows: Vec<Tuple> = [
        (Some(1), Some(true)),
        (Some(1), Some(false)),
        (Some(1), None),
        (Some(9), Some(true)),
        (Some(9), Some(false)),
        (Some(9), None),
        (None, Some(true)),
        (None, None),
    ]
    .into_iter()
    .map(|(i, b)| {
        vec![
            i.map_or(Value::Null, Value::Int4),
            b.map_or(Value::Null, Value::Bool),
        ]
    })
    .collect();

    // `c0 = 1` is false for 9 and NULL for a NULL c0, so this covers
    // false AND NULL, NULL AND true, and every other combination.
    let eq_one = compare(
        BinOp::Eq,
        PgType::Int4,
        column(0, PgType::Int4),
        constant(Value::Int4(1), PgType::Int4),
    );
    for op in [BinOp::And, BinOp::Or] {
        assert_same(
            &schema,
            &rows,
            &logic(op, eq_one.clone(), column(1, PgType::Bool)),
        );
    }
    // NOT NULL is NULL, which drops — not "keep everything that was not true".
    assert_same(
        &schema,
        &rows,
        &BoundExpr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(column(1, PgType::Bool)),
        },
    );
}

#[test]
fn is_null_and_is_not_null_agree() {
    let schema = schema_of(&[PgType::Int4]);
    let rows = int_rows();
    for negated in [false, true] {
        assert_same(
            &schema,
            &rows,
            &BoundExpr::IsNull {
                expr: Box::new(column(0, PgType::Int4)),
                negated,
            },
        );
    }
}

/// A bare boolean column is a legal `WHERE`, and its NULLs must drop.
#[test]
fn a_bare_boolean_column_filters() {
    let schema = schema_of(&[PgType::Bool]);
    let rows: Vec<Tuple> = [Some(true), Some(false), None, Some(true)]
        .into_iter()
        .map(|b| vec![b.map_or(Value::Null, Value::Bool)])
        .collect();
    assert_same(&schema, &rows, &column(0, PgType::Bool));
}

/// Comparing against a NULL constant yields NULL for every row, so everything
/// drops — `x = NULL` is never true in SQL.
#[test]
fn a_null_constant_drops_every_row() {
    let schema = schema_of(&[PgType::Int4]);
    let rows = int_rows();
    let predicate = compare(
        BinOp::Eq,
        PgType::Int4,
        column(0, PgType::Int4),
        constant(Value::Null, PgType::Int4),
    );
    assert_same(&schema, &rows, &predicate);
    assert!(columnar_filter(&schema, &rows, &predicate).expect("filter").is_empty());
}

/// Every type on the comparison whitelist really is comparable by Arrow. A type
/// that reached a kernel it has no implementation for would fail the query, so
/// this pins the list to what actually works rather than to what looks right.
#[test]
fn whitelisted_types_all_compile_and_run() {
    let cases: Vec<(PgType, Value)> = vec![
        (PgType::Bool, Value::Bool(true)),
        (PgType::Int2, Value::Int2(1)),
        (PgType::Int4, Value::Int4(1)),
        (PgType::Int8, Value::Int8(1)),
        (PgType::Date, Value::Date(1)),
        (PgType::Time, Value::Time(1)),
        (PgType::Timestamp, Value::Timestamp(1)),
        (PgType::TimestampTz, Value::TimestampTz(1)),
        (PgType::Bytea, Value::Bytea(vec![1, 2])),
        (PgType::Uuid, Value::Uuid([1; 16])),
        (PgType::Text, Value::Text("b".into())),
        (PgType::Varchar, Value::Text("b".into())),
        (PgType::Name, Value::Text("b".into())),
    ];
    for (ty, value) in cases {
        let schema = schema_of(&[ty]);
        let rows: Vec<Tuple> = vec![vec![value.clone()], vec![Value::Null]];
        for op in [BinOp::Eq, BinOp::NotEq, BinOp::Lt, BinOp::Gt] {
            let predicate = compare(op, ty, column(0, ty), constant(value.clone(), ty));
            assert!(
                expr::compile_predicate(&predicate, &layout_of(&schema)).is_some(),
                "{ty:?} {op:?} should be vectorizable"
            );
            assert_same(&schema, &rows, &predicate);
        }
    }
}

/// The exclusions, each for a reason recorded in the module docs. If one of
/// these ever starts compiling, it needs a matching argument that Arrow now
/// reproduces PostgreSQL's answer — not just that the kernel exists.
#[test]
fn types_whose_arrow_semantics_differ_are_refused() {
    let refused = [
        // Stored as Utf8, so Arrow would compare text: '9' > '10'.
        PgType::Numeric,
        // PG: NaN = NaN. IEEE and Arrow: it is not.
        PgType::Float4,
        PgType::Float8,
        // Compares with trailing blanks trimmed.
        PgType::Bpchar,
    ];
    for ty in refused {
        let schema = schema_of(&[ty]);
        let predicate = compare(
            BinOp::Eq,
            ty,
            column(0, ty),
            constant(Value::Null, PgType::Int4),
        );
        assert!(
            expr::compile_predicate(&predicate, &layout_of(&schema)).is_none(),
            "{ty:?} must not vectorize"
        );
    }
}

/// Text equality is bytewise under every supported collation, so it vectorizes
/// regardless. Ordering is not: an ICU collation orders by locale rules that no
/// Arrow kernel reproduces, so `<` is refused there and allowed under `C`.
#[test]
fn text_ordering_respects_the_collation() {
    let schema = schema_of(&[PgType::Text]);
    let layout = layout_of(&schema);
    let icu = 0xC000_0000; // the seeded `<locale>-x-icu` OID base
    let text_compare = |op, collation| BoundExpr::Binary {
        op,
        arg_ty: PgType::Text,
        collation,
        left: Box::new(column(0, PgType::Text)),
        right: Box::new(constant(Value::Text("m".into()), PgType::Text)),
    };

    for collation in [DEFAULT_COLLATION_OID, C_COLLATION_OID, icu] {
        assert!(
            expr::compile_predicate(&text_compare(BinOp::Eq, collation), &layout).is_some(),
            "equality is bytewise under every deterministic collation"
        );
    }
    assert!(expr::compile_predicate(&text_compare(BinOp::Lt, C_COLLATION_OID), &layout).is_some());
    assert!(
        expr::compile_predicate(&text_compare(BinOp::Lt, icu), &layout).is_none(),
        "an ICU ordering is not byte order and must not vectorize"
    );
}

/// Expressions the compiler must decline rather than approximate.
#[test]
fn uncompilable_expressions_decline() {
    let schema = schema_of(&[PgType::Int4]);
    let layout = layout_of(&schema);

    // Arithmetic: no vectorized operand evaluation yet.
    let arithmetic = compare(
        BinOp::Eq,
        PgType::Int4,
        BoundExpr::Binary {
            op: BinOp::Add,
            arg_ty: PgType::Int4,
            collation: DEFAULT_COLLATION_OID,
            left: Box::new(column(0, PgType::Int4)),
            right: Box::new(constant(Value::Int4(1), PgType::Int4)),
        },
        constant(Value::Int4(2), PgType::Int4),
    );
    assert!(expr::compile_predicate(&arithmetic, &layout).is_none());

    // A bind parameter is a per-execution value the compiler never sees.
    let param = compare(
        BinOp::Eq,
        PgType::Int4,
        column(0, PgType::Int4),
        BoundExpr::Param { index: 0, ty: PgType::Int4 },
    );
    assert!(expr::compile_predicate(&param, &layout).is_none());

    // A column past the batch's width would be a runtime error mid-scan.
    let out_of_range = compare(
        BinOp::Eq,
        PgType::Int4,
        column(9, PgType::Int4),
        constant(Value::Int4(1), PgType::Int4),
    );
    assert!(expr::compile_predicate(&out_of_range, &layout).is_none());
}

// ---------------------------------------------------------------- sort

use super::sort::{ProjectBatch, SortBatch};
use crabgresql_binder::SortKey;
use crate::Sort as RowSort;

fn sort_key(column: usize, ty: PgType, asc: bool, nulls_first: bool) -> SortKey {
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
fn columnar_sort(
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
        .map(|(index, column)| column_ref(index, column.ty))
        .collect();
    let takes = ProjectBatch::compile(&identity, &layout).expect("identity projection compiles");
    let projected = ProjectBatch::layout(&identity);

    let split = rows.len() / 2;
    let batches = vec![
        build_scan_batch(schema, &rows[..split])?,
        build_scan_batch(schema, &rows[split..])?,
    ];
    let project = ProjectBatch::new(
        Box::new(Batches(batches.into_iter())),
        takes,
        &projected,
    );
    let sorted = SortBatch::new(Box::new(project), keys, &projected, visible_width)?;

    let visible: BatchLayout = Arc::from(&projected[..visible_width]);
    let mut node = Shred::new(Box::new(sorted), visible);
    let mut out = Vec::new();
    while let Some(row) = node.next()? {
        out.push(row);
    }
    Ok(out)
}

/// Sort the same rows with the row `Sort`, the node this stands in for.
fn row_sort(rows: &[Tuple], keys: &[SortKey], visible_width: usize) -> Result<Vec<Tuple>, ExecError> {
    let source = crate::MaterializedRows::new(rows.to_vec());
    let mut node = RowSort::new(Box::new(source), keys.to_vec(), visible_width)?;
    let mut out = Vec::new();
    while let Some(row) = node.next()? {
        out.push(row);
    }
    Ok(out)
}

fn column_ref(index: usize, ty: PgType) -> BoundExpr {
    BoundExpr::ColumnRef { index, ty }
}

fn assert_same_order(schema: &TableSchema, rows: &[Tuple], keys: &[SortKey], width: usize) {
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

/// All four ASC/DESC × NULLS FIRST/LAST combinations. PostgreSQL keeps NULL
/// placement independent of the direction, so a DESC sort does not flip it —
/// assumed nowhere, checked here against the row node.
#[test]
fn every_direction_and_null_placement_agrees() {
    let schema = schema_of(&[PgType::Int4]);
    let rows = int_rows();
    for asc in [true, false] {
        for nulls_first in [true, false] {
            assert_same_order(
                &schema,
                &rows,
                &[sort_key(0, PgType::Int4, asc, nulls_first)],
                1,
            );
        }
    }
}

/// Equal keys must come out in input order. `lexsort_to_indices` is not a
/// stable sort, so this passes only because of the appended position key.
#[test]
fn ties_keep_input_order() {
    let schema = schema_of(&[PgType::Int4, PgType::Text]);
    // One key value, many payloads: every row is a tie.
    let rows: Vec<Tuple> = ["a", "b", "c", "d", "e", "f"]
        .into_iter()
        .map(|s| vec![Value::Int4(1), Value::Text(s.into())])
        .collect();
    let sorted = columnar_sort(&schema, &rows, &[sort_key(0, PgType::Int4, true, false)], 2)
        .expect("columnar sort");
    assert_eq!(sorted, rows, "a tie must preserve input order");
    assert_same_order(&schema, &rows, &[sort_key(0, PgType::Int4, true, false)], 2);
}

/// The float repair. PostgreSQL calls `-0.0` and `0.0` equal and every NaN
/// equal to every other; Arrow's total order does neither. Without
/// canonicalization the two paths would order these rows differently.
#[test]
fn float_zero_and_nan_sort_as_postgresql_does() {
    let schema = schema_of(&[PgType::Float8, PgType::Int4]);
    let rows: Vec<Tuple> = [
        (0.0_f64, 1),
        (-0.0, 2),
        (f64::NAN, 3),
        (1.5, 4),
        (-f64::NAN, 5),
        (-1.5, 6),
        (0.0, 7),
    ]
    .into_iter()
    .map(|(f, i)| vec![Value::Float8(f), Value::Int4(i)])
    .collect();
    for asc in [true, false] {
        assert_same_order(&schema, &rows, &[sort_key(0, PgType::Float8, asc, false)], 2);
    }
}

/// A multi-key sort, where the second key only decides rows the first ties.
#[test]
fn multi_key_sorts_agree() {
    let schema = schema_of(&[PgType::Int4, PgType::Text]);
    let rows: Vec<Tuple> = [
        (Some(2), Some("b")),
        (Some(1), Some("z")),
        (Some(2), Some("a")),
        (None, Some("m")),
        (Some(1), None),
        (Some(1), Some("a")),
    ]
    .into_iter()
    .map(|(i, s)| {
        vec![
            i.map_or(Value::Null, Value::Int4),
            s.map_or(Value::Null, |s| Value::Text(s.into())),
        ]
    })
    .collect();
    assert_same_order(
        &schema,
        &rows,
        &[
            sort_key(0, PgType::Int4, true, false),
            sort_key(1, PgType::Text, false, true),
        ],
        2,
    );
}

/// A hidden ORDER BY column — one the planner appended past the visible output —
/// orders the rows and is then dropped, leaving the client width.
#[test]
fn hidden_sort_columns_are_dropped_after_ordering() {
    let schema = schema_of(&[PgType::Text, PgType::Int4]);
    let rows: Vec<Tuple> = [("a", 3), ("b", 1), ("c", 2)]
        .into_iter()
        .map(|(s, i)| vec![Value::Text(s.into()), Value::Int4(i)])
        .collect();
    // Order by column 1, emit only column 0.
    let keys = [sort_key(1, PgType::Int4, true, false)];
    let sorted = columnar_sort(&schema, &rows, &keys, 1).expect("columnar sort");
    assert_eq!(
        sorted,
        vec![
            vec![Value::Text("b".into())],
            vec![Value::Text("c".into())],
            vec![Value::Text("a".into())],
        ]
    );
    assert_same_order(&schema, &rows, &keys, 1);
}

/// Sort keys whose Arrow order is not PostgreSQL's are refused, so the row
/// `Sort` keeps them. `numeric` is the dangerous one: stored as text, it would
/// sort `'10'` before `'9'` without any error.
#[test]
fn unsortable_key_types_are_refused() {
    for ty in [PgType::Numeric, PgType::Bpchar, PgType::Interval, PgType::TimeTz] {
        let schema = schema_of(&[ty]);
        assert!(
            !SortBatch::compilable(&[sort_key(0, ty, true, false)], &layout_of(&schema)),
            "{ty:?} must not sort columnar"
        );
    }
    // An ICU collation orders text by locale rules, not bytes.
    let schema = schema_of(&[PgType::Text]);
    let mut key = sort_key(0, PgType::Text, true, false);
    key.collation = 0xC000_0000;
    assert!(!SortBatch::compilable(&[key], &layout_of(&schema)));
}

/// An empty input sorts to an empty result rather than failing on the
/// zero-batch concat.
#[test]
fn an_empty_input_sorts_to_nothing() {
    let schema = schema_of(&[PgType::Int4]);
    let sorted = columnar_sort(&schema, &[], &[sort_key(0, PgType::Int4, true, false)], 1)
        .expect("columnar sort");
    assert!(sorted.is_empty());
}

/// A filter that rejects everything in a batch yields an empty batch, and the
/// shred above must read that as "nothing here", not "end of stream" — the
/// rows in the *next* batch still have to come out.
#[test]
fn an_entirely_rejected_batch_does_not_end_the_stream() {
    let schema = schema_of(&[PgType::Int4]);
    // First batch all rejected, second batch all kept.
    let rows: Vec<Tuple> = vec![
        vec![Value::Int4(0)],
        vec![Value::Int4(0)],
        vec![Value::Int4(5)],
        vec![Value::Int4(5)],
    ];
    let kept = columnar_filter(
        &schema,
        &rows,
        &compare(
            BinOp::Eq,
            PgType::Int4,
            column(0, PgType::Int4),
            constant(Value::Int4(5), PgType::Int4),
        ),
    )
    .expect("filter");
    assert_eq!(kept, vec![vec![Value::Int4(5)], vec![Value::Int4(5)]]);
}
