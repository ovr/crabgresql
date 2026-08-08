//! Equivalence tests: a columnar node must produce exactly what the row node
//! it stands in for produces, for the same input.
//!
//! That is the only property that matters here. A columnar node is chosen
//! automatically and is invisible in the result, so any divergence is a
//! wrong-answer bug that nothing else in the system would catch.

use std::sync::Arc;

use arrow_array::RecordBatch;
use crabgresql_binder::{AggFn, BinOp, BoundAggregate, BoundExpr, UnaryOp};
use crabgresql_storage_api::arrow::build_scan_batch;
use crabgresql_storage_api::{Column, TableSchema, Tuple};
use crabgresql_types::collation::{C_COLLATION_OID, DEFAULT_COLLATION_OID};
use crabgresql_types::{PgType, Value};

use super::{BatchLayout, BatchNode, FilterBatch, Shred, agg, expr};
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
    assert!(
        columnar_filter(&schema, &rows, &predicate)
            .expect("filter")
            .is_empty()
    );
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
        // Arrow's `eq` is bitwise: `-0.0 = 0.0` is false, PG says true.
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
        BoundExpr::Param {
            index: 0,
            ty: PgType::Int4,
        },
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
use crate::Sort as RowSort;
use crabgresql_binder::SortKey;

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
        assert_same_order(
            &schema,
            &rows,
            &[sort_key(0, PgType::Float8, asc, false)],
            2,
        );
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

/// `"char"` sorts columnar, and by its *unsigned* byte. It is stored as
/// `UInt8` for exactly that reason — under a signed encoding `0xFF` would sort
/// below `0x00` and contradict the type — so the columnar node must agree with
/// the row node on a high-bit byte.
#[test]
fn a_char_column_sorts_columnar_by_its_unsigned_byte() {
    let schema = schema_of(&[PgType::Char]);
    let rows: Vec<Tuple> = [0xFF, 0x41, 0x00, 0x80]
        .into_iter()
        .map(|byte| vec![Value::Char(byte)])
        .collect();
    let keys = [sort_key(0, PgType::Char, true, false)];
    assert!(SortBatch::compilable(&keys, &layout_of(&schema)));
    let sorted = columnar_sort(&schema, &rows, &keys, 1).expect("columnar sort");
    assert_eq!(
        sorted,
        [0x00, 0x41, 0x80, 0xFF]
            .into_iter()
            .map(|byte| vec![Value::Char(byte)])
            .collect::<Vec<_>>()
    );
    assert_same_order(&schema, &rows, &keys, 1);
}

/// Sort keys whose Arrow order is not PostgreSQL's are refused, so the row
/// `Sort` keeps them. `numeric` is the dangerous one: stored as text, it would
/// sort `'10'` before `'9'` without any error.
#[test]
fn unsortable_key_types_are_refused() {
    for ty in [
        PgType::Numeric,
        PgType::Bpchar,
        PgType::Interval,
        PgType::TimeTz,
    ] {
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

/// A predicate with no column reference evaluates against scalars, so it
/// produces a mask describing ONE value. Every consumer assumes a mask is as
/// tall as the batch, and both fail quietly if it is not: `filter_record_batch`
/// truncates the batch to the mask's length, so `WHERE 1=1` returned one row
/// per batch instead of every row.
///
/// `WHERE 1=1` is what ORMs and query builders emit for a dynamically-empty
/// predicate, so this was reachable from ordinary SQL.
#[test]
fn a_constant_only_predicate_keeps_every_row() {
    let schema = schema_of(&[PgType::Int4]);
    let rows = int_rows();
    let one = || constant(Value::Int4(1), PgType::Int4);

    // `1 = 1` — true for every row.
    let always = compare(BinOp::Eq, PgType::Int4, one(), one());
    assert_same(&schema, &rows, &always);
    assert_eq!(
        columnar_filter(&schema, &rows, &always)
            .expect("filter")
            .len(),
        rows.len(),
        "a constant-true predicate must keep every row, not one per batch"
    );

    // `1 = 2` — false for every row.
    let never = compare(
        BinOp::Eq,
        PgType::Int4,
        one(),
        constant(Value::Int4(2), PgType::Int4),
    );
    assert_same(&schema, &rows, &never);
    assert!(
        columnar_filter(&schema, &rows, &never)
            .expect("filter")
            .is_empty()
    );

    // A bare `true`, and `NULL IS NULL` — the other two shapes with no column.
    assert_same(&schema, &rows, &constant(Value::Bool(true), PgType::Bool));
    assert_same(
        &schema,
        &rows,
        &BoundExpr::IsNull {
            expr: Box::new(constant(Value::Null, PgType::Int4)),
            negated: false,
        },
    );
}

/// The other half of the same defect: a constant beside a column gives
/// `and_kleene` a length-N and a length-1 operand, which it rejects outright —
/// so `WHERE id = 1 AND true` failed the query rather than answering it.
#[test]
fn a_constant_beside_a_column_still_compares() {
    let schema = schema_of(&[PgType::Int4]);
    let rows = int_rows();
    let eq_one = compare(
        BinOp::Eq,
        PgType::Int4,
        column(0, PgType::Int4),
        constant(Value::Int4(1), PgType::Int4),
    );
    for op in [BinOp::And, BinOp::Or] {
        for literal in [Value::Bool(true), Value::Bool(false), Value::Null] {
            assert_same(
                &schema,
                &rows,
                &logic(op, eq_one.clone(), constant(literal.clone(), PgType::Bool)),
            );
        }
    }
}

/// A zero-row batch is where a length-1 mask is *longer* than the data, which
/// `filter_record_batch` does reject — so the empty relation errored where the
/// row path returned nothing.
#[test]
fn a_constant_predicate_over_no_rows_yields_no_rows() {
    let schema = schema_of(&[PgType::Int4]);
    let always = compare(
        BinOp::Eq,
        PgType::Int4,
        constant(Value::Int4(1), PgType::Int4),
        constant(Value::Int4(1), PgType::Int4),
    );
    assert!(
        columnar_filter(&schema, &[], &always)
            .expect("filter")
            .is_empty()
    );
}

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

// ---------------------------------------------------------------------------
// Grouped aggregation
// ---------------------------------------------------------------------------

fn aggregate(func: AggFn, args: Vec<BoundExpr>, input_ty: PgType, ret: PgType) -> BoundAggregate {
    BoundAggregate {
        func,
        distinct: false,
        args,
        input_ty,
        ret,
        collation: DEFAULT_COLLATION_OID,
    }
}

fn count_star() -> BoundAggregate {
    aggregate(AggFn::Count, Vec::new(), PgType::Int8, PgType::Int8)
}

/// Run `rows` through the columnar aggregate, split across two batches so a
/// multi-batch stream is always exercised — that is what catches a group index or
/// group vector rebuilt per batch.
fn columnar_aggregate(
    schema: &TableSchema,
    rows: &[Tuple],
    group_exprs: &[BoundExpr],
    aggregates: &[BoundAggregate],
) -> Result<Vec<Tuple>, ExecError> {
    let layout = layout_of(schema);
    let split = rows.len() / 2;
    let batches = vec![
        build_scan_batch(schema, &rows[..split])?,
        build_scan_batch(schema, &rows[split..])?,
    ];
    let positions: Vec<usize> = (0..schema.columns.len()).collect();
    let mut node = agg::AggregateBatch::compile(
        Box::new(Batches(batches.into_iter())),
        layout,
        group_exprs,
        aggregates,
        &positions,
    )
    .map_err(|_| ExecError::new("XX000", "aggregate did not compile"))?;
    let mut out = Vec::new();
    while let Some(row) = node.next()? {
        out.push(row);
    }
    Ok(out)
}

/// Run the same rows through the row `Aggregate` — the real node, not a second
/// implementation of it.
fn row_aggregate(
    rows: &[Tuple],
    group_exprs: &[BoundExpr],
    aggregates: &[BoundAggregate],
) -> Result<Vec<Tuple>, ExecError> {
    let mut node = crate::Aggregate::new(
        Box::new(crate::MaterializedRows::new(rows.to_vec())),
        group_exprs.to_vec(),
        aggregates.to_vec(),
        ExecContext::default(),
    );
    let mut out = Vec::new();
    while let Some(row) = node.next()? {
        out.push(row);
    }
    Ok(out)
}

/// Both paths, same groups, same order, same values.
///
/// Order is part of the assertion, not incidental: an unordered `GROUP BY` reports
/// rows in first-seen order, and each group reports its *first-seen* key — so
/// `GROUP BY x` over `(-0.0, 0.0)` must emit `-0` rather than the canonical form
/// the hash used. Compared as rendered text because `Value`'s derived `PartialEq`
/// is IEEE, so a NaN-bearing result would never equal itself.
fn assert_same_groups(
    schema: &TableSchema,
    rows: &[Tuple],
    group_exprs: &[BoundExpr],
    aggregates: &[BoundAggregate],
) {
    let columnar =
        columnar_aggregate(schema, rows, group_exprs, aggregates).expect("columnar aggregate");
    let row = row_aggregate(rows, group_exprs, aggregates).expect("row aggregate");
    assert_eq!(
        format!("{columnar:?}"),
        format!("{row:?}"),
        "columnar and row aggregates disagree"
    );
}

/// Every grouping-key type the node admits, including the three
/// `agg::key_encoding` classes and the types a columnar *filter* must refuse.
///
/// `numeric`, `float8` and `bpchar` are the point of this test. The filter gate
/// rejects them because an Arrow comparison would be wrong — `'9' > '10'` on
/// text-encoded numeric, `-0.0 <> 0.0` under IEEE, no blank trimming. None of
/// that applies here: the values are decoded and grouped by the row engine's own
/// `hash_key`/`keys_equal`, so the answer is the row engine's by construction.
#[test]
fn grouping_keys_of_every_encoding_agree() {
    // Scalar encoding.
    assert_same_groups(
        &schema_of(&[PgType::Int4]),
        &int_rows(),
        &[column(0, PgType::Int4)],
        &[count_star()],
    );
    // Text encoding, and `bpchar`'s trailing-blank trimming: 'a' and 'a  ' are
    // one group, and it reports whichever arrived first.
    let bpchar = schema_of(&[PgType::Bpchar]);
    let bpchar_rows: Vec<Tuple> = ["a  ", "a", "b", "a ", "", " "]
        .iter()
        .map(|s| vec![Value::Text((*s).into())])
        .collect();
    assert_same_groups(
        &bpchar,
        &bpchar_rows,
        &[column(0, PgType::Bpchar)],
        &[count_star()],
    );
    // Generic encoding: `numeric` has no injective `u64` code because `1.0` and
    // `1.00` are equal but differently scaled.
    let numeric = schema_of(&[PgType::Numeric]);
    let numeric_rows: Vec<Tuple> = ["1.0", "1.00", "2", "0.10", "1"]
        .iter()
        .map(|s| {
            vec![Value::Numeric(
                crabgresql_types::numeric::Numeric::parse(s).expect("numeric"),
            )]
        })
        .collect();
    assert_same_groups(
        &numeric,
        &numeric_rows,
        &[column(0, PgType::Numeric)],
        &[count_star()],
    );
    // `float8`: both zeros are one group and every NaN is one value, but the
    // group keeps the representative it first saw.
    let float = schema_of(&[PgType::Float8]);
    let float_rows: Vec<Tuple> = [
        Some(-0.0f64),
        Some(0.0),
        Some(f64::NAN),
        Some(-f64::NAN),
        Some(1.0),
        None,
    ]
    .into_iter()
    .map(|v| vec![v.map_or(Value::Null, Value::Float8)])
    .collect();
    assert_same_groups(
        &float,
        &float_rows,
        &[column(0, PgType::Float8)],
        &[count_star()],
    );
    // `date` carries days since PostgreSQL's 2000 epoch, not Arrow's. A missed
    // rebase shifts every group by ~30 years, which no other test here notices.
    let date = schema_of(&[PgType::Date]);
    let date_rows: Vec<Tuple> = [4930i32, 4931, 4930, i32::MIN, i32::MAX, 0]
        .iter()
        .map(|d| vec![Value::Date(*d)])
        .collect();
    assert_same_groups(
        &date,
        &date_rows,
        &[column(0, PgType::Date)],
        &[count_star()],
    );
    // A multi-column key is always `Generic`, whatever its parts encode as.
    let pair = schema_of(&[PgType::Int4, PgType::Text]);
    let pair_rows: Vec<Tuple> = [
        (Some(1), Some("a")),
        (Some(1), Some("b")),
        (Some(1), Some("a")),
        (None, Some("a")),
        (Some(1), None),
        (None, None),
    ]
    .iter()
    .map(|(i, s)| {
        vec![
            i.map_or(Value::Null, Value::Int4),
            s.map_or(Value::Null, |s| Value::Text(s.into())),
        ]
    })
    .collect();
    assert_same_groups(
        &pair,
        &pair_rows,
        &[column(0, PgType::Int4), column(1, PgType::Text)],
        &[count_star()],
    );
    // A constant key is one group over every row.
    assert_same_groups(
        &schema_of(&[PgType::Int4]),
        &int_rows(),
        &[constant(Value::Int4(7), PgType::Int4)],
        &[count_star()],
    );
}

/// Every aggregate function, including the ones with state the batch path must
/// not shortcut: `sum(int8)`'s promotion to `numeric`, `avg`'s divide scale,
/// `min`/`max`'s collation, `string_agg`'s per-row delimiter, and DISTINCT.
#[test]
fn every_aggregate_function_agrees() {
    let schema = schema_of(&[PgType::Int4, PgType::Text, PgType::Int8]);
    let rows: Vec<Tuple> = [
        (Some(1), Some("a"), Some(i64::MAX)),
        (Some(2), Some("B"), Some(i64::MAX)),
        (Some(1), None, Some(-1)),
        (None, Some("a"), None),
        (Some(1), Some("a"), Some(3)),
    ]
    .iter()
    .map(|(i, s, b)| {
        vec![
            i.map_or(Value::Null, Value::Int4),
            s.map_or(Value::Null, |s| Value::Text(s.into())),
            b.map_or(Value::Null, Value::Int8),
        ]
    })
    .collect();
    let int = || column(0, PgType::Int4);
    let text = || column(1, PgType::Text);
    let big = || column(2, PgType::Int8);

    let mut every: Vec<BoundAggregate> = vec![
        count_star(),
        aggregate(AggFn::Count, vec![int()], PgType::Int4, PgType::Int8),
        aggregate(AggFn::Sum, vec![int()], PgType::Int4, PgType::Int8),
        // Two `i64::MAX`es overflow the register state into `numeric`.
        aggregate(AggFn::Sum, vec![big()], PgType::Int8, PgType::Numeric),
        aggregate(AggFn::Avg, vec![int()], PgType::Int4, PgType::Numeric),
        aggregate(AggFn::Min, vec![int()], PgType::Int4, PgType::Int4),
        aggregate(AggFn::Max, vec![int()], PgType::Int4, PgType::Int4),
        aggregate(
            AggFn::StringAgg,
            vec![text(), constant(Value::Text(",".into()), PgType::Text)],
            PgType::Text,
            PgType::Text,
        ),
    ];
    // `min`/`max` over text compare under a collation, and both are exercised
    // because ICU order and byte order disagree on case.
    for collation in [DEFAULT_COLLATION_OID, C_COLLATION_OID] {
        for func in [AggFn::Min, AggFn::Max] {
            every.push(BoundAggregate {
                collation,
                ..aggregate(func, vec![text()], PgType::Text, PgType::Text)
            });
        }
    }
    // One at a time, so a disagreement names the aggregate...
    for one in &every {
        assert_same_groups(&schema, &rows, &[], std::slice::from_ref(one));
        assert_same_groups(&schema, &rows, &[int()], std::slice::from_ref(one));
    }
    // ...and all together, which is also the only shape that exercises the
    // per-aggregate argument buffers side by side.
    assert_same_groups(&schema, &rows, &[int()], &every);

    // DISTINCT: per group and per call, and the mixed case where only one
    // aggregate is DISTINCT — `distinct_values` is empty unless some aggregate is,
    // and it is indexed rather than zipped for exactly that reason.
    let distinct = |func, arg: BoundExpr, ty, ret| BoundAggregate {
        distinct: true,
        ..aggregate(func, vec![arg], ty, ret)
    };
    for keys in [Vec::new(), vec![int()]] {
        assert_same_groups(
            &schema,
            &rows,
            &keys,
            &[distinct(AggFn::Count, text(), PgType::Text, PgType::Int8)],
        );
        assert_same_groups(
            &schema,
            &rows,
            &keys,
            &[
                count_star(),
                distinct(AggFn::Count, text(), PgType::Text, PgType::Int8),
                aggregate(AggFn::Sum, vec![int()], PgType::Int4, PgType::Int8),
            ],
        );
    }
}

/// An unkeyed aggregate is one group even over no rows, so `SELECT count(*)`
/// returns `0` rather than nothing — and a keyed one over no rows returns nothing
/// rather than a spurious group. The batch path seeds that group before the first
/// pull; seeding it on the first row instead would silently swap the two answers.
#[test]
fn an_empty_stream_produces_the_row_paths_groups() {
    let schema = schema_of(&[PgType::Int4]);
    let aggregates = [
        count_star(),
        aggregate(
            AggFn::Sum,
            vec![column(0, PgType::Int4)],
            PgType::Int4,
            PgType::Int8,
        ),
        aggregate(
            AggFn::Avg,
            vec![column(0, PgType::Int4)],
            PgType::Int4,
            PgType::Numeric,
        ),
        aggregate(
            AggFn::Min,
            vec![column(0, PgType::Int4)],
            PgType::Int4,
            PgType::Int4,
        ),
    ];
    assert_same_groups(&schema, &[], &[], &aggregates);
    assert_same_groups(&schema, &[], &[column(0, PgType::Int4)], &aggregates);
    // A group whose every value is NULL is not an empty group: `count(*)` counts
    // the rows, the rest stay NULL.
    let nulls: Vec<Tuple> = vec![vec![Value::Null], vec![Value::Null]];
    assert_same_groups(&schema, &nulls, &[], &aggregates);
    assert_same_groups(&schema, &nulls, &[column(0, PgType::Int4)], &aggregates);
}

/// The `count` fast path folds a batch by its height instead of decoding it, so it
/// has to read the height of the batch it is *given*. A filter below it empties
/// one batch and thins another; counting the pre-filter rows, or treating the
/// empty batch as end-of-stream, both show up here and nowhere else.
#[test]
fn the_count_fast_path_counts_what_the_filter_left() {
    let schema = schema_of(&[PgType::Int4, PgType::Text]);
    let rows: Vec<Tuple> = [(0, "a"), (0, "b"), (5, "c"), (0, "d"), (5, "e"), (5, "f")]
        .iter()
        .map(|(i, s)| vec![Value::Int4(*i), Value::Text((*s).into())])
        .collect();
    let keep_five = expr::compile_predicate(
        &compare(
            BinOp::Eq,
            PgType::Int4,
            column(0, PgType::Int4),
            constant(Value::Int4(5), PgType::Int4),
        ),
        &layout_of(&schema),
    )
    .expect("predicate");

    // First batch: one of three survives. Second: two of three. The row path sees
    // the same rows, so the two must agree on 3.
    let split = 3;
    let batches = vec![
        build_scan_batch(&schema, &rows[..split]).expect("batch"),
        build_scan_batch(&schema, &rows[split..]).expect("batch"),
    ];
    let filtered = FilterBatch::new(Box::new(Batches(batches.into_iter())), keep_five);
    let aggregates = [
        count_star(),
        // `count(col)` folds by null count, so a column with nulls in the
        // surviving rows is the case that separates it from the batch height.
        aggregate(
            AggFn::Count,
            vec![column(1, PgType::Text)],
            PgType::Text,
            PgType::Int8,
        ),
    ];
    let mut node = agg::AggregateBatch::compile(
        Box::new(filtered),
        layout_of(&schema),
        &[],
        &aggregates,
        &[0, 1],
    )
    .map_err(|_| "aggregate did not compile")
    .expect("compile");
    let mut out = Vec::new();
    while let Some(row) = node.next().expect("aggregate") {
        out.push(row);
    }
    let survivors: Vec<Tuple> = rows
        .iter()
        .filter(|r| r[0] == Value::Int4(5))
        .cloned()
        .collect();
    assert_eq!(
        format!("{out:?}"),
        format!(
            "{:?}",
            row_aggregate(&survivors, &[], &aggregates).expect("row")
        ),
    );

    // With a NULL in the counted column the two counts must part company.
    let with_null: Vec<Tuple> = vec![
        vec![Value::Int4(5), Value::Null],
        vec![Value::Int4(5), Value::Text("x".into())],
    ];
    assert_same_groups(&schema, &with_null, &[], &aggregates);
    // `count(NULL)` counts nothing and `count(1)` counts every row — both go
    // through the fast path's constant arm.
    for (value, ty) in [(Value::Null, PgType::Int4), (Value::Int4(1), PgType::Int4)] {
        assert_same_groups(
            &schema,
            &with_null,
            &[],
            &[aggregate(
                AggFn::Count,
                vec![constant(value, ty)],
                ty,
                PgType::Int8,
            )],
        );
    }
}

/// A key naming a column the scan never filled would read `null_array` padding
/// and collapse every row into one NULL group — a wrong answer, not an error, so
/// the compile has to refuse it. The planner's projection pass makes this
/// unreachable today; nothing else would notice if that changed.
#[test]
fn a_key_outside_the_scans_projection_is_declined() {
    let schema = schema_of(&[PgType::Int4, PgType::Text]);
    let layout = layout_of(&schema);
    let child = || Box::new(Batches(Vec::new().into_iter())) as Box<dyn BatchNode>;

    // Column 1 was not projected.
    assert!(
        agg::AggregateBatch::compile(
            child(),
            Arc::clone(&layout),
            &[column(1, PgType::Text)],
            &[count_star()],
            &[0],
        )
        .is_err(),
        "an unprojected grouping key must decline"
    );
    // Nor as an aggregate argument.
    assert!(
        agg::AggregateBatch::compile(
            child(),
            Arc::clone(&layout),
            &[],
            &[aggregate(
                AggFn::Max,
                vec![column(1, PgType::Text)],
                PgType::Text,
                PgType::Text
            )],
            &[0],
        )
        .is_err(),
        "an unprojected aggregate argument must decline"
    );
    // A projected one compiles, so the guard is not a blanket refusal.
    assert!(
        agg::AggregateBatch::compile(
            child(),
            layout,
            &[column(1, PgType::Text)],
            &[count_star()],
            &[0, 1],
        )
        .is_ok()
    );
}

/// The aggregate half of the agreement contract. Same reasoning as
/// [`the_planner_and_the_executor_agree_on_every_shape`]: these are separate walks
/// over `BoundExpr` in separate crates, and nothing else pins them together.
#[test]
fn the_planner_and_the_executor_agree_on_every_aggregate_shape() {
    let schema = schema_of(&[PgType::Int4, PgType::Text, PgType::Int2]);
    let layout = layout_of(&schema);
    let positions: Vec<usize> = (0..schema.columns.len()).collect();
    let int = || column(0, PgType::Int4);

    let shapes: Vec<BoundExpr> = vec![
        // Accepted: a column in range, a representable constant, and either
        // behind the value-transparent `Collate`.
        int(),
        column(1, PgType::Text),
        constant(Value::Int4(1), PgType::Int4),
        BoundExpr::Collate {
            expr: Box::new(column(1, PgType::Text)),
            collation: C_COLLATION_OID,
            explicit: false,
        },
        // Declined, each for its own reason.
        column(9, PgType::Int4),
        constant(Value::Json("{}".into()), PgType::Json),
        BoundExpr::Param {
            index: 0,
            ty: PgType::Int4,
        },
        // A widening cast *is* admissible in a filter, where Arrow does the
        // comparing. Here the value is decoded and handed to the row engine, so
        // the cast would have to be applied — a computation, and out of scope.
        coerce(column(2, PgType::Int2), PgType::Int4),
        BoundExpr::Binary {
            op: BinOp::Add,
            arg_ty: PgType::Int4,
            collation: DEFAULT_COLLATION_OID,
            left: Box::new(int()),
            right: Box::new(constant(Value::Int4(1), PgType::Int4)),
        },
    ];

    for shape in shapes {
        let planner = crabgresql_planner::vectorize::vectorizable_agg_cell(&shape, layout.len());
        // Asked as a grouping key and as an aggregate argument, since the gate
        // states one rule for both and the compiler applies it in two places.
        let as_key = agg::cells(std::slice::from_ref(&shape), &layout, &positions).is_some();
        let as_arg = agg::AggregateBatch::compile(
            Box::new(Batches(Vec::new().into_iter())),
            Arc::clone(&layout),
            &[],
            &[aggregate(
                AggFn::Max,
                vec![shape.clone()],
                shape.ty(),
                shape.ty(),
            )],
            &positions,
        )
        .is_ok();
        assert_eq!(
            planner, as_key,
            "planner says {planner}, key compiler says {as_key} for {shape:?}"
        );
        assert_eq!(
            planner, as_arg,
            "planner says {planner}, argument compiler says {as_arg} for {shape:?}"
        );
    }
}

fn coerce(expr: BoundExpr, ty: PgType) -> BoundExpr {
    BoundExpr::Coerce {
        expr: Box::new(expr),
        ty,
    }
}

/// `smallint_col <> 0` resolves in `int4`, so the binder wraps the **column** in
/// a `Coerce` and leaves the constant alone. Arrow's comparison kernels need one
/// `DataType` on both sides, so the column has to be widened — and the widening
/// must agree with the row path's `cast_value` on every value, nulls included.
#[test]
fn a_widened_smallint_column_compares_as_it_does_on_rows() {
    let schema = schema_of(&[PgType::Int2]);
    let rows: Vec<Tuple> = [
        Some(0i16),
        Some(1),
        Some(-1),
        Some(i16::MIN),
        Some(i16::MAX),
        None,
    ]
    .into_iter()
    .map(|v| vec![v.map_or(Value::Null, Value::Int2)])
    .collect();

    for op in [
        BinOp::Eq,
        BinOp::NotEq,
        BinOp::Lt,
        BinOp::LtEq,
        BinOp::Gt,
        BinOp::GtEq,
    ] {
        for literal in [0i32, 1, -1, i16::MIN as i32, i16::MAX as i32] {
            // int2 -> int4, the shape the binder actually produces.
            assert_same(
                &schema,
                &rows,
                &compare(
                    op,
                    PgType::Int4,
                    coerce(column(0, PgType::Int2), PgType::Int4),
                    constant(Value::Int4(literal), PgType::Int4),
                ),
            );
            // int2 -> int8, and with the widened side on the right, so neither
            // the second widening pair nor operand order is left unexercised.
            assert_same(
                &schema,
                &rows,
                &compare(
                    op,
                    PgType::Int8,
                    constant(Value::Int8(literal.into()), PgType::Int8),
                    coerce(column(0, PgType::Int2), PgType::Int8),
                ),
            );
        }
    }
    // A constant the source type cannot hold is the case the widening exists to
    // make safe: on rows the comparison is simply false/true, and it must not
    // become an out-of-range error or a wrapped match.
    assert_same(
        &schema,
        &rows,
        &compare(
            BinOp::Gt,
            PgType::Int4,
            coerce(column(0, PgType::Int2), PgType::Int4),
            constant(Value::Int4(i32::MAX), PgType::Int4),
        ),
    );
    // Nulls must survive the widening: `IS NULL` reads the widened array's own
    // null buffer, not the original's.
    assert_same(
        &schema,
        &rows,
        &BoundExpr::IsNull {
            expr: Box::new(coerce(column(0, PgType::Int2), PgType::Int4)),
            negated: false,
        },
    );
}

/// The planner decides *whether* and the executor decides *how*, so the two
/// must agree exactly: a shape the planner accepts and the executor declines
/// makes EXPLAIN advertise work that never happens, and the reverse hides work
/// that does. Nothing but this test enforces it — they are separate walks over
/// `BoundExpr` in separate crates.
#[test]
fn the_planner_and_the_executor_agree_on_every_shape() {
    let schema = schema_of(&[PgType::Int4, PgType::Text, PgType::Numeric, PgType::Int2]);
    let layout = layout_of(&schema);
    let int = || column(0, PgType::Int4);
    let one = || constant(Value::Int4(1), PgType::Int4);

    let shapes: Vec<BoundExpr> = vec![
        // Accepted.
        compare(BinOp::Eq, PgType::Int4, int(), one()),
        compare(BinOp::Lt, PgType::Int4, int(), one()),
        logic(
            BinOp::And,
            compare(BinOp::Eq, PgType::Int4, int(), one()),
            compare(BinOp::Gt, PgType::Int4, int(), one()),
        ),
        BoundExpr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(compare(BinOp::Eq, PgType::Int4, int(), one())),
        },
        BoundExpr::IsNull {
            expr: Box::new(int()),
            negated: true,
        },
        compare(
            BinOp::Eq,
            PgType::Text,
            column(1, PgType::Text),
            constant(Value::Text("x".into()), PgType::Text),
        ),
        // Widening integer casts, in both nesting depths.
        compare(
            BinOp::Eq,
            PgType::Int4,
            coerce(column(3, PgType::Int2), PgType::Int4),
            one(),
        ),
        compare(
            BinOp::Eq,
            PgType::Int8,
            coerce(coerce(column(3, PgType::Int2), PgType::Int4), PgType::Int8),
            constant(Value::Int8(1), PgType::Int8),
        ),
        // Declined, each for its own reason.
        compare(
            BinOp::Eq,
            PgType::Numeric,
            column(2, PgType::Numeric),
            one(),
        ),
        // A *narrowing* cast raises `22003` on a value the source holds fine, so
        // which row raises first is observable and no kernel reproduces it.
        compare(
            BinOp::Eq,
            PgType::Int2,
            coerce(int(), PgType::Int2),
            constant(Value::Int2(1), PgType::Int2),
        ),
        // Not every cast between Arrow-representable types is total: `int4 ->
        // text` is, but it is a *rendering*, and `text -> int4` raises.
        compare(
            BinOp::Eq,
            PgType::Text,
            coerce(int(), PgType::Text),
            constant(Value::Text("1".into()), PgType::Text),
        ),
        // Out of range under the cast, so the widening check must not be the only
        // thing consulted — the inner operand is still gated.
        compare(
            BinOp::Eq,
            PgType::Int4,
            coerce(column(9, PgType::Int2), PgType::Int4),
            one(),
        ),
        compare(BinOp::Eq, PgType::Int4, column(9, PgType::Int4), one()),
        compare(
            BinOp::Eq,
            PgType::Int4,
            int(),
            BoundExpr::Param {
                index: 0,
                ty: PgType::Int4,
            },
        ),
        compare(
            BinOp::Eq,
            PgType::Int4,
            int(),
            constant(Value::Json("{}".into()), PgType::Json),
        ),
        BoundExpr::IsNull {
            expr: Box::new(constant(Value::Json("{}".into()), PgType::Json)),
            negated: false,
        },
        constant(Value::Bool(true), PgType::Bool),
    ];

    for shape in shapes {
        let planner = crabgresql_planner::vectorize::vectorizable_predicate(&shape, layout.len());
        let executor = expr::compile_predicate(&shape, &layout).is_some();
        assert_eq!(
            planner, executor,
            "planner says {planner}, executor says {executor} for {shape:?}"
        );
    }
}

/// `Shred` decodes only the columns a scan filled. A batch is full width so a
/// schema ordinal is a batch ordinal, but the columns outside the projection
/// are all-NULL padding — decoding those makes the per-row cost scale with the
/// table rather than the query, and the row scan never did.
#[test]
fn shred_decodes_only_the_projected_columns() {
    let schema = schema_of(&[PgType::Int4, PgType::Text, PgType::Int8]);
    let rows = vec![vec![
        Value::Int4(1),
        Value::Text("x".into()),
        Value::Int8(9),
    ]];
    let batch = build_scan_batch(&schema, &rows).expect("batch");

    // Only column 2 was "projected"; the rest read back as Null, which is what
    // the row scan promises for an unselected column.
    let mut node = Shred::new(
        Box::new(Batches(vec![batch].into_iter())),
        layout_of(&schema),
        vec![2],
    );
    let row = node.next().expect("shred").expect("a row");
    assert_eq!(row, vec![Value::Null, Value::Null, Value::Int8(9)]);
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
