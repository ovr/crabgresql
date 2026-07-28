//! Type-dispatched kernels: comparison, arithmetic, and literal broadcast.
//!
//! Every entry point takes the [`PgType`] the plan recorded for its operands and
//! dispatches on that, never on the arrays' Arrow types. `numeric`, `text` and
//! `bpchar` are all `Utf8` and mean three different things, so the Arrow type
//! cannot decide semantics.
//!
//! # What arrow does and does not give us
//!
//! Verified in `tests/arrow_assumptions.rs` against the version in the lock
//! file, because two of these are the opposite of the obvious guess:
//!
//! * **Integer arithmetic is checked.** `numeric::add` errors on overflow rather
//!   than wrapping, which is the decision PostgreSQL makes. Only the message
//!   differs, so [`arith`] remaps it.
//! * **Float comparison is IEEE 754 totalOrder, not `==`.** It therefore agrees
//!   with PostgreSQL that `NaN = NaN` and that NaN sorts above infinity. It
//!   disagrees only about signed zero, which [`compare`] fixes by canonicalizing
//!   `-0.0` to `0.0` on the operands.
//! * **Float arithmetic is plain IEEE**, so `1e308 * 10` yields infinity where
//!   PostgreSQL raises `22003`, and `1e-300 * 1e-300` yields zero where
//!   PostgreSQL raises underflow. There is no arrow kernel with those rules, so
//!   float arithmetic is refused here and evaluated a row at a time instead.

use std::sync::Arc;

use arrow_array::{
    Array, ArrayRef, BooleanArray, Float32Array, Float64Array, Int16Array, Int32Array, Int64Array,
    StringArray,
};
use arrow_array::types::{Float32Type, Float64Type, Int32Type, Int64Type};
use crabgresql_types::{PgType, Value};

use crate::{BatchError, batch_type_of};

/// SQLSTATE `22003`, `numeric_value_out_of_range`.
const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
/// SQLSTATE `22012`, `division_by_zero`.
const DIVISION_BY_ZERO: &str = "22012";

/// The comparison operators a batch kernel implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

/// The arithmetic operators a batch kernel implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

impl ArithOp {
    /// Whether this operator can raise, which decides whether a `CASE` branch or
    /// an `AND` right operand must be evaluated under a narrowed selection.
    pub fn can_raise(self) -> bool {
        // Every one of them can: `+`, `-` and `*` overflow, `/` and `%` divide
        // by zero. Spelled out rather than `true` so that adding a total
        // operator later is a visible decision.
        match self {
            ArithOp::Add | ArithOp::Sub | ArithOp::Mul => true,
            ArithOp::Div | ArithOp::Mod => true,
        }
    }
}

/// Whether [`compare`] handles `ty`.
///
/// Exhaustive, with no `_` arm: a new [`PgType`] fails this build rather than
/// silently becoming uncomparable — or worse, comparable by the wrong rules.
pub fn compares(ty: PgType) -> bool {
    match ty {
        // Integers, booleans, and the temporal types a batch stores as
        // PostgreSQL-domain integers. `date` and `timestamp` infinities are the
        // extremes of their integer domain, so order and equality survive.
        PgType::Bool
        | PgType::Int2
        | PgType::Int4
        | PgType::Int8
        | PgType::Date
        | PgType::Time
        | PgType::Timestamp
        | PgType::TimestampTz => true,

        // totalOrder plus signed-zero canonicalization reproduces `f8_cmp`.
        PgType::Float4 | PgType::Float8 => true,

        // Bytewise, which is what every deterministic collation reduces to for
        // *equality*. Ordering under a non-default collation is refused by the
        // caller, which is the only place the collation is known.
        PgType::Text | PgType::Varchar | PgType::Name => true,

        // `bpchar` ignores trailing blanks in both comparison and hashing, and
        // is indistinguishable from `text` below the `PgType` — arrow would
        // compare `'a'` and `'a  '` as different. Needs its own kernel.
        PgType::Bpchar => false,
        // Text on disk, and text order is not numeric order: `"9" > "10"`.
        PgType::Numeric => false,

        PgType::Money
        | PgType::Oid
        | PgType::Reg(_)
        | PgType::Bytea
        | PgType::Uuid
        | PgType::Bit
        | PgType::Varbit
        | PgType::TimeTz
        | PgType::Interval
        | PgType::Inet
        | PgType::Cidr
        | PgType::Macaddr
        | PgType::Macaddr8
        | PgType::Point
        | PgType::Lseg
        | PgType::Json
        | PgType::Jsonb
        | PgType::Jsonpath
        | PgType::Tsvector
        | PgType::Tsquery
        | PgType::User(_)
        | PgType::Array(_) => false,
    }
}

/// Whether [`arith`] handles `ty`.
///
/// Integers only. Floats are excluded because PostgreSQL raises `22003` on
/// overflow *and* on underflow-to-zero from non-zero operands, and arrow's
/// float kernels do neither — they produce infinity and zero. `numeric` is
/// excluded because it is text in a batch.
pub fn arithmetic(ty: PgType) -> bool {
    matches!(ty, PgType::Int2 | PgType::Int4 | PgType::Int8)
}

/// Compare two same-typed arrays, yielding one boolean per row.
///
/// A null operand yields a null result, which the caller treats as "not true" —
/// matching the row engine, where only `Bool(true)` passes a filter.
pub fn compare(
    op: CmpOp,
    ty: PgType,
    left: &ArrayRef,
    right: &ArrayRef,
) -> Result<BooleanArray, BatchError> {
    if !compares(ty) {
        return Err(BatchError::internal(format!(
            "no batch comparison kernel for {}",
            ty.name()
        )));
    }
    // PostgreSQL's float comparison treats `-0.0` and `0.0` as equal; arrow's
    // totalOrder separates them. Folding the sign off zero is the whole
    // compensation, and it leaves NaN alone because `NaN == 0.0` is false.
    let left = canonical_zero(ty, left);
    let right = canonical_zero(ty, right);

    let compare = match op {
        CmpOp::Eq => arrow_ord::cmp::eq,
        CmpOp::NotEq => arrow_ord::cmp::neq,
        CmpOp::Lt => arrow_ord::cmp::lt,
        CmpOp::LtEq => arrow_ord::cmp::lt_eq,
        CmpOp::Gt => arrow_ord::cmp::gt,
        CmpOp::GtEq => arrow_ord::cmp::gt_eq,
    };
    compare(&left, &right).map_err(|error| BatchError::internal(format!("compare batch: {error}")))
}

/// Fold `-0.0` into `0.0` so arrow's totalOrder matches PostgreSQL's float
/// comparison. A no-op for every other type.
fn canonical_zero(ty: PgType, array: &ArrayRef) -> ArrayRef {
    match ty {
        PgType::Float4 => match array.as_any().downcast_ref::<Float32Array>() {
            Some(values) => Arc::new(values.unary::<_, Float32Type>(|v| if v == 0.0 { 0.0 } else { v })),
            None => Arc::clone(array),
        },
        PgType::Float8 => match array.as_any().downcast_ref::<Float64Array>() {
            Some(values) => Arc::new(values.unary::<_, Float64Type>(|v| if v == 0.0 { 0.0 } else { v })),
            None => Arc::clone(array),
        },
        _ => Arc::clone(array),
    }
}

/// Apply an arithmetic operator, raising PostgreSQL's error on overflow or
/// division by zero.
///
/// arrow reaches the same *decision* — its integer kernels are checked — but
/// reports it as `ArithmeticOverflow("Overflow happened on: 32767 + 1")`. The
/// remap below is what keeps a vectorized plan unobservable through its error
/// text.
pub fn arith(
    op: ArithOp,
    ty: PgType,
    left: &ArrayRef,
    right: &ArrayRef,
) -> Result<ArrayRef, BatchError> {
    if !arithmetic(ty) {
        return Err(BatchError::internal(format!(
            "no batch arithmetic kernel for {}",
            ty.name()
        )));
    }
    let apply = match op {
        ArithOp::Add => arrow_arith::numeric::add,
        ArithOp::Sub => arrow_arith::numeric::sub,
        ArithOp::Mul => arrow_arith::numeric::mul,
        ArithOp::Div => arrow_arith::numeric::div,
        ArithOp::Mod => arrow_arith::numeric::rem,
    };
    apply(left, right).map_err(|error| match error {
        arrow_schema::ArrowError::DivideByZero => {
            BatchError::new(DIVISION_BY_ZERO, "division by zero")
        }
        arrow_schema::ArrowError::ArithmeticOverflow(_) => out_of_range(ty),
        other => BatchError::internal(format!("batch arithmetic: {other}")),
    })
}

/// The row engine's overflow message for an integer type, reproduced verbatim.
fn out_of_range(ty: PgType) -> BatchError {
    let message = match ty {
        PgType::Int2 => "smallint out of range",
        PgType::Int4 => "integer out of range",
        PgType::Int8 => "bigint out of range",
        // `arithmetic` admits only the three above, so this is unreachable in
        // practice — reported rather than panicked so a future widening that
        // forgets this table degrades to a clear internal error.
        other => {
            return BatchError::internal(format!("no overflow message for {}", other.name()));
        }
    };
    BatchError::new(NUMERIC_VALUE_OUT_OF_RANGE, message)
}

/// Whether `from` widens to `to` without any possibility of failure.
///
/// Only the integer widenings, and only upward. That restriction is what makes
/// this admissible at all: every `i16` is an `i32`, so the conversion has no
/// error case to reproduce and no rounding to disagree about. A *narrowing*
/// cast raises `22003` on overflow and a numeric cast rounds, so neither is a
/// widening and neither belongs here.
///
/// It earns its place because `smallint_column <> 0` compares an `int2` against
/// an `int4` literal, so the binder widens the column — and in a ClickBench-shaped
/// relation, roughly half the columns are `smallint`.
pub fn widens(from: PgType, to: PgType) -> bool {
    matches!(
        (from, to),
        (PgType::Int2, PgType::Int4)
            | (PgType::Int2, PgType::Int8)
            | (PgType::Int4, PgType::Int8)
    )
}

/// Apply a widening admitted by [`widens`].
pub fn widen(from: PgType, to: PgType, array: &ArrayRef) -> Result<ArrayRef, BatchError> {
    let mismatch = || {
        BatchError::internal(format!(
            "cannot widen {} held as {} to {}",
            from.name(),
            array.data_type(),
            to.name()
        ))
    };
    let widened: ArrayRef = match (from, to) {
        (PgType::Int2, PgType::Int4) => {
            let values = array.as_any().downcast_ref::<Int16Array>().ok_or_else(mismatch)?;
            Arc::new(values.unary::<_, Int32Type>(i32::from))
        }
        (PgType::Int2, PgType::Int8) => {
            let values = array.as_any().downcast_ref::<Int16Array>().ok_or_else(mismatch)?;
            Arc::new(values.unary::<_, Int64Type>(i64::from))
        }
        (PgType::Int4, PgType::Int8) => {
            let values = array.as_any().downcast_ref::<Int32Array>().ok_or_else(mismatch)?;
            Arc::new(values.unary::<_, Int64Type>(i64::from))
        }
        _ => {
            return Err(BatchError::internal(format!(
                "{} does not widen to {}",
                from.name(),
                to.name()
            )));
        }
    };
    Ok(widened)
}

/// A scalar repeated `len` times, as the array its type requires.
///
/// `Value::Null` becomes a fully-null array of the right type, so a comparison
/// against a null constant yields nulls rather than failing to build.
pub fn broadcast(value: &Value, ty: PgType, len: usize) -> Result<ArrayRef, BatchError> {
    let data_type = batch_type_of(ty).ok_or_else(|| {
        BatchError::internal(format!("{} has no batch representation", ty.name()))
    })?;
    if matches!(value, Value::Null) {
        return Ok(arrow_array::new_null_array(&data_type, len));
    }
    let array: ArrayRef = match (ty, value) {
        (PgType::Bool, Value::Bool(v)) => Arc::new(BooleanArray::from(vec![*v; len])),
        (PgType::Int2, Value::Int2(v)) => Arc::new(Int16Array::from(vec![*v; len])),
        (PgType::Int4, Value::Int4(v)) => Arc::new(Int32Array::from(vec![*v; len])),
        (PgType::Int8, Value::Int8(v)) => Arc::new(Int64Array::from(vec![*v; len])),
        (PgType::Float4, Value::Float4(v)) => Arc::new(Float32Array::from(vec![*v; len])),
        (PgType::Float8, Value::Float8(v)) => Arc::new(Float64Array::from(vec![*v; len])),
        // Already PostgreSQL-domain on both sides: the constant came from the
        // binder as PostgreSQL days/microseconds and the column was rebased at
        // the scan, so neither side is shifted here.
        (PgType::Date, Value::Date(v)) => Arc::new(Int32Array::from(vec![*v; len])),
        (PgType::Time, Value::Time(v)) => Arc::new(Int64Array::from(vec![*v; len])),
        (PgType::Timestamp, Value::Timestamp(v)) => Arc::new(Int64Array::from(vec![*v; len])),
        (PgType::TimestampTz, Value::TimestampTz(v)) => Arc::new(Int64Array::from(vec![*v; len])),
        (
            PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name,
            Value::Text(v),
        ) => Arc::new(StringArray::from(vec![v.as_str(); len])),
        (PgType::Numeric, Value::Numeric(v)) => {
            Arc::new(StringArray::from(vec![v.to_display(); len]))
        }
        (ty, value) => {
            return Err(BatchError::internal(format!(
                "cannot broadcast {value:?} as {}",
                ty.name()
            )));
        }
    };
    Ok(array)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i32s(values: Vec<Option<i32>>) -> ArrayRef {
        Arc::new(Int32Array::from(values))
    }

    fn f64s(values: Vec<Option<f64>>) -> ArrayRef {
        Arc::new(Float64Array::from(values))
    }

    #[test]
    fn integer_overflow_reports_postgresqls_message_not_arrows() {
        let a: ArrayRef = Arc::new(Int16Array::from(vec![i16::MAX]));
        let b: ArrayRef = Arc::new(Int16Array::from(vec![1]));
        let error = arith(ArithOp::Add, PgType::Int2, &a, &b).expect_err("overflow");
        assert_eq!(error.code, "22003");
        assert_eq!(error.message, "smallint out of range");

        let a = i32s(vec![Some(i32::MAX)]);
        let b = i32s(vec![Some(1)]);
        let error = arith(ArithOp::Add, PgType::Int4, &a, &b).expect_err("overflow");
        assert_eq!(error.message, "integer out of range");
    }

    #[test]
    fn division_by_zero_reports_postgresqls_sqlstate() {
        let a = i32s(vec![Some(1)]);
        let b = i32s(vec![Some(0)]);
        let error = arith(ArithOp::Div, PgType::Int4, &a, &b).expect_err("divide by zero");
        assert_eq!(error.code, "22012");
        assert_eq!(error.message, "division by zero");
    }

    /// The compensation that makes arrow's totalOrder match `f8_cmp`.
    #[test]
    fn signed_zero_compares_equal_and_nan_still_compares_equal() {
        let neg = f64s(vec![Some(-0.0)]);
        let pos = f64s(vec![Some(0.0)]);
        let out = compare(CmpOp::Eq, PgType::Float8, &neg, &pos).expect("compare");
        assert!(out.value(0), "-0.0 = 0.0 must hold, as in PostgreSQL");

        let out = compare(CmpOp::Lt, PgType::Float8, &neg, &pos).expect("compare");
        assert!(!out.value(0), "-0.0 < 0.0 must not hold");

        let nan = f64s(vec![Some(f64::NAN)]);
        let same = f64s(vec![Some(f64::NAN)]);
        let out = compare(CmpOp::Eq, PgType::Float8, &nan, &same).expect("compare");
        assert!(out.value(0), "NaN = NaN must hold, as in PostgreSQL");

        let finite = f64s(vec![Some(1e308)]);
        let out = compare(CmpOp::Gt, PgType::Float8, &nan, &finite).expect("compare");
        assert!(out.value(0), "NaN must sort above every finite value");
    }

    #[test]
    fn a_null_operand_compares_unknown_not_false() {
        let a = i32s(vec![Some(1), None]);
        let b = i32s(vec![Some(1), Some(1)]);
        let out = compare(CmpOp::Eq, PgType::Int4, &a, &b).expect("compare");
        assert!(out.value(0));
        assert!(out.is_null(1));
    }

    /// Float arithmetic must be refused rather than silently producing infinity
    /// where PostgreSQL raises `22003`.
    #[test]
    fn float_arithmetic_is_refused() {
        assert!(!arithmetic(PgType::Float8));
        let a = f64s(vec![Some(1e308)]);
        let b = f64s(vec![Some(10.0)]);
        let error = arith(ArithOp::Mul, PgType::Float8, &a, &b).expect_err("refused");
        assert!(error.message.contains("no batch arithmetic kernel"));
    }

    /// `bpchar` and `numeric` look like `text` to arrow and mean something else,
    /// so the kernel refuses them rather than comparing them as bytes.
    #[test]
    fn types_whose_semantics_arrow_cannot_express_are_refused() {
        assert!(!compares(PgType::Bpchar));
        assert!(!compares(PgType::Numeric));
        assert!(compares(PgType::Text));

        let a: ArrayRef = Arc::new(StringArray::from(vec!["a"]));
        let b: ArrayRef = Arc::new(StringArray::from(vec!["a  "]));
        assert!(compare(CmpOp::Eq, PgType::Bpchar, &a, &b).is_err());
    }

    #[test]
    fn broadcasting_a_null_constant_yields_a_typed_null_array() {
        let array = broadcast(&Value::Null, PgType::Int4, 3).expect("broadcast");
        assert_eq!(array.len(), 3);
        assert_eq!(array.null_count(), 3);
        assert_eq!(array.data_type(), &arrow_schema::DataType::Int32);
    }

    /// A date constant is broadcast as PostgreSQL days, matching the column the
    /// scan already rebased. Shifting either side here would move the window by
    /// 30 years — the failure this whole design is arranged to prevent.
    #[test]
    fn a_date_constant_is_broadcast_in_the_postgresql_domain() {
        let array = broadcast(&Value::Date(4_930), PgType::Date, 1).expect("broadcast");
        let days = array.as_any().downcast_ref::<Int32Array>().expect("int32");
        assert_eq!(days.value(0), 4_930);
    }

    /// Widening is infallible for every value of the source type, which is the
    /// whole reason it is admitted where a narrowing cast is not.
    #[test]
    fn widening_is_total_over_the_source_range() {
        let extremes: ArrayRef = Arc::new(Int16Array::from(vec![
            Some(i16::MIN),
            Some(-1),
            Some(0),
            None,
            Some(i16::MAX),
        ]));
        let wide = widen(PgType::Int2, PgType::Int4, &extremes).expect("widen");
        let wide = wide.as_any().downcast_ref::<Int32Array>().expect("int32");
        assert_eq!(wide.value(0), i32::from(i16::MIN));
        assert_eq!(wide.value(4), i32::from(i16::MAX));
        assert!(wide.is_null(3), "nulls stay null");
    }

    #[test]
    fn only_upward_integer_casts_are_treated_as_widening() {
        assert!(widens(PgType::Int2, PgType::Int4));
        assert!(widens(PgType::Int4, PgType::Int8));
        // Narrowing raises 22003 on overflow, so it is a real cast, not a widening.
        assert!(!widens(PgType::Int8, PgType::Int4));
        // int -> float loses precision above 2^53, and numeric -> anything parses.
        assert!(!widens(PgType::Int4, PgType::Float8));
        assert!(!widens(PgType::Numeric, PgType::Float8));
        assert!(widen(PgType::Int8, PgType::Int4, &i32s(vec![Some(1)])).is_err());
    }

    #[test]
    fn comparison_and_arithmetic_agree_on_which_types_they_admit() {
        // Arithmetic is a strict subset of comparison: anything computable is
        // comparable, but not the reverse.
        for ty in [PgType::Int2, PgType::Int4, PgType::Int8] {
            assert!(arithmetic(ty) && compares(ty), "{ty:?}");
        }
        for ty in [PgType::Float8, PgType::Text, PgType::Date] {
            assert!(!arithmetic(ty) && compares(ty), "{ty:?}");
        }
    }
}
