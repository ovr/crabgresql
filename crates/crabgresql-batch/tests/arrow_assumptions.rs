//! Executable assertions about the arrow-rs kernels this crate builds on.
//!
//! Every one of these is a property the vectorized executor's correctness rests
//! on, asserted directly against the arrow version in the lock file rather than
//! taken from its documentation. An arrow upgrade that changes any of them then
//! fails CI, instead of quietly changing query answers.
//!
//! Where a property *differs* from crabgresql's row engine, the test says so and
//! names the compensation. Those are the load-bearing ones.

use arrow_array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int16Array, Int32Array, StringArray,
};
use arrow_schema::ArrowError;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Integer arithmetic: checked, and how the overflow is reported.
// ---------------------------------------------------------------------------

/// PostgreSQL raises `22003` on integer overflow rather than wrapping, and so
/// does the row engine (`eval_arith_int4` uses `checked_add`). `numeric::add`
/// agrees on the *decision* — it errors — which is why the kernels may use it.
///
/// It does not agree on the *text*, so every caller must remap the error.
#[test]
fn integer_addition_is_checked_not_wrapping() {
    let a: ArrayRef = Arc::new(Int16Array::from(vec![i16::MAX]));
    let b: ArrayRef = Arc::new(Int16Array::from(vec![1]));
    let error = arrow_arith::numeric::add(&a, &b).expect_err("overflow must not wrap");
    assert!(
        matches!(error, ArrowError::ArithmeticOverflow(_)),
        "unexpected overflow error: {error:?}"
    );

    // And the message is *not* PostgreSQL's, which is why `kernels::arith`
    // rewrites it to `smallint out of range` before it can reach a client.
    assert!(
        !error.to_string().contains("out of range"),
        "arrow's message unexpectedly matches PostgreSQL's: {error}"
    );
}

/// The wrapping variant exists and is the silent-wrong-answer trap: it must
/// never appear in this crate.
#[test]
fn the_wrapping_variant_would_silently_corrupt() {
    let a: ArrayRef = Arc::new(Int16Array::from(vec![i16::MAX]));
    let b: ArrayRef = Arc::new(Int16Array::from(vec![1]));
    let wrapped = arrow_arith::numeric::add_wrapping(&a, &b).expect("wrapping never errors");
    let wrapped = wrapped
        .as_any()
        .downcast_ref::<Int16Array>()
        .expect("int16");
    assert_eq!(wrapped.value(0), i16::MIN);
}

/// Integer division by zero errors rather than trapping or producing a
/// sentinel. PostgreSQL raises `22012`; the kernel remaps.
#[test]
fn integer_division_by_zero_errors() {
    let a: ArrayRef = Arc::new(Int32Array::from(vec![1]));
    let b: ArrayRef = Arc::new(Int32Array::from(vec![0]));
    let error = arrow_arith::numeric::div(&a, &b).expect_err("division by zero must error");
    assert!(
        matches!(error, ArrowError::DivideByZero),
        "unexpected division error: {error:?}"
    );
}

/// Arithmetic skips null slots: a null operand yields a null result and cannot
/// fault. This is what lets a kernel evaluate `1/x` on a batch whose `x` has
/// nulls without raising on them.
#[test]
fn arithmetic_does_not_fault_on_null_slots() {
    let a: ArrayRef = Arc::new(Int32Array::from(vec![Some(1), Some(1)]));
    let b: ArrayRef = Arc::new(Int32Array::from(vec![None, Some(2)]));
    let out = arrow_arith::numeric::div(&a, &b).expect("null divisor must not fault");
    let out = out.as_any().downcast_ref::<Int32Array>().expect("int32");
    assert!(out.is_null(0));
    assert_eq!(out.value(1), 0);
}

// ---------------------------------------------------------------------------
// Float comparison: IEEE, and therefore a declared divergence.
// ---------------------------------------------------------------------------

/// arrow-ord orders floats by IEEE 754 *totalOrder*, not by `<`/`==`. On NaN
/// that is exactly what PostgreSQL and the row engine's `f8_cmp` do: `NaN = NaN`
/// is true, and NaN sorts above every finite value including infinity.
///
/// This was worth measuring rather than assuming — the naive expectation is that
/// arrow is IEEE and therefore wrong about NaN, which would have made every
/// float comparison a divergence. It is not, so NaN needs no compensation.
#[test]
fn float_comparison_agrees_with_postgresql_on_nan() {
    let nan: ArrayRef = Arc::new(Float64Array::from(vec![f64::NAN]));
    let same: ArrayRef = Arc::new(Float64Array::from(vec![f64::NAN]));
    let finite: ArrayRef = Arc::new(Float64Array::from(vec![1.0]));
    let infinite: ArrayRef = Arc::new(Float64Array::from(vec![f64::INFINITY]));

    assert!(
        arrow_ord::cmp::eq(&nan, &same).expect("compare").value(0),
        "NaN = NaN must be true, as it is in PostgreSQL"
    );
    assert!(
        arrow_ord::cmp::gt(&nan, &finite).expect("compare").value(0),
        "NaN must sort above finite values"
    );
    assert!(
        arrow_ord::cmp::gt(&nan, &infinite).expect("compare").value(0),
        "NaN must sort above infinity"
    );
}

/// The one place totalOrder and PostgreSQL disagree: totalOrder separates `-0.0`
/// from `0.0`, and PostgreSQL does not (`f8_cmp(-0.0, 0.0) == Equal`).
///
/// This is why [`crate::kernels`] canonicalizes signed zero on float comparison
/// operands instead of handing the raw array to arrow. The compensation is one
/// pass and it is cheap; the alternative would be a permanent, user-visible
/// divergence on `WHERE f = 0`.
///
/// [`crate::kernels`]: crabgresql_batch::kernels
#[test]
fn total_order_separates_signed_zero_and_postgresql_does_not() {
    let neg: ArrayRef = Arc::new(Float64Array::from(vec![-0.0]));
    let pos: ArrayRef = Arc::new(Float64Array::from(vec![0.0]));

    assert!(
        !arrow_ord::cmp::eq(&neg, &pos).expect("compare").value(0),
        "arrow now folds signed zero; the canonicalization in kernels::cmp \
         can be deleted"
    );
    assert!(
        arrow_ord::cmp::lt(&neg, &pos).expect("compare").value(0),
        "totalOrder places -0.0 below 0.0"
    );
}

/// And the compensation, asserted end to end: canonicalizing zero makes arrow
/// agree with `f8_cmp` on signed zero while leaving NaN alone.
#[test]
fn canonicalizing_signed_zero_restores_postgresql_equality() {
    let canon = |v: f64| if v == 0.0 { 0.0 } else { v };
    let neg: ArrayRef = Arc::new(Float64Array::from(vec![canon(-0.0)]));
    let pos: ArrayRef = Arc::new(Float64Array::from(vec![canon(0.0)]));
    assert!(arrow_ord::cmp::eq(&neg, &pos).expect("compare").value(0));

    // `NaN == 0.0` is false, so NaN survives the canonicalization untouched.
    let nan: ArrayRef = Arc::new(Float64Array::from(vec![canon(f64::NAN)]));
    let same: ArrayRef = Arc::new(Float64Array::from(vec![canon(f64::NAN)]));
    assert!(arrow_ord::cmp::eq(&nan, &same).expect("compare").value(0));
}

// ---------------------------------------------------------------------------
// Boolean logic: SQL is Kleene, arrow's default is not.
// ---------------------------------------------------------------------------

/// `NULL AND FALSE` is FALSE in SQL, and `NOT (NULL AND FALSE)` is therefore
/// TRUE — observable as a row count, not just as a projected value. Arrow's
/// plain `and` intersects validity and gets this wrong; `and_kleene` is right.
///
/// The kernels must use the Kleene variants, and this test is why.
#[test]
fn plain_boolean_and_is_not_kleene_but_and_kleene_is() {
    let unknown = BooleanArray::from(vec![None]);
    let f = BooleanArray::from(vec![Some(false)]);

    let plain = arrow_arith::boolean::and(&unknown, &f).expect("and");
    assert!(plain.is_null(0), "arrow's plain `and` is now Kleene");

    let kleene = arrow_arith::boolean::and_kleene(&unknown, &f).expect("and_kleene");
    assert!(!kleene.is_null(0), "NULL AND FALSE must be known");
    assert!(!kleene.value(0), "NULL AND FALSE must be FALSE");
}

/// The dual: `NULL OR TRUE` is TRUE.
#[test]
fn or_kleene_decides_on_a_true_operand() {
    let unknown = BooleanArray::from(vec![None]);
    let t = BooleanArray::from(vec![Some(true)]);

    let plain = arrow_arith::boolean::or(&unknown, &t).expect("or");
    assert!(plain.is_null(0), "arrow's plain `or` is now Kleene");

    let kleene = arrow_arith::boolean::or_kleene(&unknown, &t).expect("or_kleene");
    assert!(!kleene.is_null(0), "NULL OR TRUE must be known");
    assert!(kleene.value(0), "NULL OR TRUE must be TRUE");
}

/// `NOT NULL` stays NULL under three-valued logic.
#[test]
fn negation_preserves_unknown() {
    let unknown = BooleanArray::from(vec![None]);
    let out = arrow_arith::boolean::not(&unknown).expect("not");
    assert!(out.is_null(0));
}

// ---------------------------------------------------------------------------
// Comparison null semantics, and strings.
// ---------------------------------------------------------------------------

/// A comparison with a null operand is unknown, not false. The distinction
/// matters under `NOT`, and it is what `Batch::filter` relies on when it drops
/// null mask entries.
#[test]
fn comparison_with_null_is_unknown() {
    let a: ArrayRef = Arc::new(Int32Array::from(vec![Some(1), None]));
    let b: ArrayRef = Arc::new(Int32Array::from(vec![Some(1), Some(1)]));
    let out = arrow_ord::cmp::eq(&a, &b).expect("compare");
    assert!(out.value(0));
    assert!(out.is_null(1));
}

/// String comparison is bytewise. Equality is safe under every collation
/// crabgresql supports (all are deterministic, so equal bytes and equal values
/// coincide), but *ordering* is not — which is why the gate refuses `<`/`>` on
/// strings whose collation is not the default byte order.
#[test]
fn string_comparison_is_bytewise() {
    let a: ArrayRef = Arc::new(StringArray::from(vec!["Z"]));
    let b: ArrayRef = Arc::new(StringArray::from(vec!["a"]));
    let out = arrow_ord::cmp::lt(&a, &b).expect("compare");
    assert!(out.value(0), "expected byte order, where 'Z' < 'a'");
}

/// Trailing blanks are significant to arrow. `bpchar` ignores them in both
/// comparison and hashing, so `char(n)` cannot share the `text` kernels — hence
/// its exclusion from the gate's type whitelist.
#[test]
fn trailing_blanks_are_significant_to_arrow() {
    let a: ArrayRef = Arc::new(StringArray::from(vec!["a"]));
    let b: ArrayRef = Arc::new(StringArray::from(vec!["a  "]));
    let out = arrow_ord::cmp::eq(&a, &b).expect("compare");
    assert!(!out.value(0), "arrow now folds trailing blanks");
}

// ---------------------------------------------------------------------------
// Selection kernels.
// ---------------------------------------------------------------------------

/// `filter` drops rows whose mask entry is null, not just false. `Batch::filter`
/// documents this as matching the row engine, where only `Bool(true)` passes.
#[test]
fn filter_drops_null_mask_entries() {
    let values: ArrayRef = Arc::new(Int32Array::from(vec![1, 2, 3]));
    let mask = BooleanArray::from(vec![Some(true), None, Some(false)]);
    let out = arrow_select::filter::filter(values.as_ref(), &mask).expect("filter");
    let out = out.as_any().downcast_ref::<Int32Array>().expect("int32");
    assert_eq!(out.len(), 1);
    assert_eq!(out.value(0), 1);
}

/// `take` preserves nulls in the gathered values and admits repeats — the two
/// properties the `CASE` scatter path depends on.
#[test]
fn take_preserves_nulls_and_allows_repeats() {
    let values: ArrayRef = Arc::new(Int32Array::from(vec![Some(10), None, Some(30)]));
    let indices = arrow_array::UInt32Array::from(vec![2, 1, 2]);
    let out = arrow_select::take::take(values.as_ref(), &indices, None).expect("take");
    let out = out.as_any().downcast_ref::<Int32Array>().expect("int32");
    assert_eq!(out.value(0), 30);
    assert!(out.is_null(1));
    assert_eq!(out.value(2), 30);
}
