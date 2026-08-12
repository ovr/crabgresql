//! Arithmetic over two already-evaluated, non-NULL [`Value`]s.
//!
//! Lives here rather than in the executor because two callers need it: the
//! executor evaluates arithmetic per row, and the logical optimizer folds it
//! once at plan time (`10 + 5` → `15`). Both have to agree on overflow, on
//! division by zero and on `int MIN % -1`, so there is one implementation and
//! the callers wrap its error in their own type — exactly what the executor
//! already does for [`crate::numeric`] and [`crate::cast`].
//!
//! The operator enums mirror the binder's `BinOp`/`UnaryOp`, restricted to the
//! arithmetic subset; the binder is a *later* crate than this one, so it maps
//! its own enum onto these at the call site.

use crate::compare::{float4, float8, int2, int4, int8, numeric};
use crate::{Numeric, PgType, Value, float};

// SQLSTATE codes used here (mirrors crabgresql_pg_wire::sqlstate, kept as
// literals so this crate needs no protocol dependency).
const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
const DIVISION_BY_ZERO: &str = "22012";

/// What arithmetic refused to produce. Same shape as [`crate::cast::CastError`]
/// and [`crate::numeric::NumErr`], so a caller maps all three the same way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArithError {
    pub sqlstate: &'static str,
    pub message: String,
    pub detail: Option<String>,
}

impl ArithError {
    fn new(sqlstate: &'static str, message: impl Into<String>) -> ArithError {
        ArithError {
            sqlstate,
            message: message.into(),
            detail: None,
        }
    }
}

/// The arithmetic subset of the binder's `BinOp`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
}

/// The binder's `UnaryOp`, whose members are all arithmetic but `Not`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryArithOp {
    Not,
    Neg,
    /// `@` — absolute value; result type is the operand type.
    Abs,
    /// `|/` — square root (float8).
    Sqrt,
    /// `||/` — cube root (float8).
    Cbrt,
}

/// Apply an arithmetic operator to two non-NULL operands of `arg_ty`.
///
/// `arg_ty` is the operand type the binder settled on, and it is also the result
/// type — promotion happened at bind time. A type outside the numeric set never
/// reaches here: the binder resolves `date + interval` and friends to function
/// calls, not to a `Binary` node.
pub fn eval_arith(op: ArithOp, arg_ty: PgType, l: &Value, r: &Value) -> Result<Value, ArithError> {
    match arg_ty {
        PgType::Int2 => arith_int2(op, int2(l), int2(r)),
        PgType::Int4 => arith_int4(op, int4(l), int4(r)),
        PgType::Int8 => arith_int8(op, int8(l), int8(r)),
        PgType::Float4 => arith_f4(op, float4(l), float4(r)),
        PgType::Float8 => arith_f8(op, float8(l), float8(r)),
        PgType::Numeric => arith_numeric(op, numeric(l), numeric(r)),
        other => unreachable!("binder let arithmetic through on {other:?}"),
    }
}

/// Apply a unary operator to one already-evaluated operand. NULL in, NULL out.
pub fn eval_unary(op: UnaryArithOp, operand: Value) -> Result<Value, ArithError> {
    match (op, operand) {
        (_, Value::Null) => Ok(Value::Null),
        (UnaryArithOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (UnaryArithOp::Neg, Value::Int4(v)) => v
            .checked_neg()
            .map(Value::Int4)
            .ok_or_else(|| out_of_range(PgType::Int4)),
        (UnaryArithOp::Neg, Value::Int8(v)) => v
            .checked_neg()
            .map(Value::Int8)
            .ok_or_else(|| out_of_range(PgType::Int8)),
        (UnaryArithOp::Neg, Value::Int2(v)) => v
            .checked_neg()
            .map(Value::Int2)
            .ok_or_else(|| out_of_range(PgType::Int2)),
        (UnaryArithOp::Neg, Value::Float4(v)) => Ok(Value::Float4(-v)),
        (UnaryArithOp::Neg, Value::Float8(v)) => Ok(Value::Float8(-v)),
        (UnaryArithOp::Neg, Value::Numeric(v)) => Ok(Value::Numeric(v.neg())),
        (UnaryArithOp::Abs, Value::Int2(v)) => v
            .checked_abs()
            .map(Value::Int2)
            .ok_or_else(|| out_of_range(PgType::Int2)),
        (UnaryArithOp::Abs, Value::Int4(v)) => v
            .checked_abs()
            .map(Value::Int4)
            .ok_or_else(|| out_of_range(PgType::Int4)),
        (UnaryArithOp::Abs, Value::Int8(v)) => v
            .checked_abs()
            .map(Value::Int8)
            .ok_or_else(|| out_of_range(PgType::Int8)),
        (UnaryArithOp::Abs, Value::Float4(v)) => Ok(Value::Float4(v.abs())),
        (UnaryArithOp::Abs, Value::Float8(v)) => Ok(Value::Float8(v.abs())),
        (UnaryArithOp::Abs, Value::Numeric(v)) => Ok(Value::Numeric(v.abs())),
        (UnaryArithOp::Sqrt, Value::Float8(v)) => {
            float::f8_sqrt(v).map(Value::Float8).map_err(float_error)
        }
        (UnaryArithOp::Cbrt, Value::Float8(v)) => Ok(Value::Float8(float::f8_cbrt(v))),
        (op, operand) => unreachable!("binder let through {op:?} on {operand:?}"),
    }
}

/// The overflow error PostgreSQL raises for an integer type, by its own name.
pub fn out_of_range(ty: PgType) -> ArithError {
    let message = match ty {
        PgType::Int2 => "smallint out of range",
        PgType::Int4 => "integer out of range",
        PgType::Int8 => "bigint out of range",
        _ => unreachable!(),
    };
    ArithError::new(NUMERIC_VALUE_OUT_OF_RANGE, message)
}

fn float_error(e: float::FloatError) -> ArithError {
    ArithError::new(e.sqlstate, e.message)
}

fn numeric_error(e: crate::numeric::NumErr) -> ArithError {
    ArithError {
        sqlstate: e.sqlstate,
        message: e.message,
        detail: e.detail,
    }
}

fn division_by_zero() -> ArithError {
    ArithError::new(DIVISION_BY_ZERO, "division by zero")
}

fn arith_int2(op: ArithOp, a: i16, b: i16) -> Result<Value, ArithError> {
    let result = match op {
        ArithOp::Add => a.checked_add(b),
        ArithOp::Sub => a.checked_sub(b),
        ArithOp::Mul => a.checked_mul(b),
        ArithOp::Div => {
            if b == 0 {
                return Err(division_by_zero());
            }
            a.checked_div(b)
        }
        ArithOp::Mod => {
            if b == 0 {
                return Err(division_by_zero());
            }
            return Ok(Value::Int2(a.checked_rem(b).unwrap_or(0)));
        }
        _ => unreachable!(),
    };
    result
        .map(Value::Int2)
        .ok_or_else(|| out_of_range(PgType::Int2))
}

fn arith_int4(op: ArithOp, a: i32, b: i32) -> Result<Value, ArithError> {
    let result = match op {
        ArithOp::Add => a.checked_add(b),
        ArithOp::Sub => a.checked_sub(b),
        ArithOp::Mul => a.checked_mul(b),
        ArithOp::Div => {
            if b == 0 {
                return Err(division_by_zero());
            }
            // MIN / -1 overflows; MIN % -1 is 0 in PG, but checked_rem
            // refuses it, so special-case below.
            a.checked_div(b)
        }
        ArithOp::Mod => {
            if b == 0 {
                return Err(division_by_zero());
            }
            return Ok(Value::Int4(a.checked_rem(b).unwrap_or(0)));
        }
        _ => unreachable!(),
    };
    result
        .map(Value::Int4)
        .ok_or_else(|| out_of_range(PgType::Int4))
}

fn arith_int8(op: ArithOp, a: i64, b: i64) -> Result<Value, ArithError> {
    let result = match op {
        ArithOp::Add => a.checked_add(b),
        ArithOp::Sub => a.checked_sub(b),
        ArithOp::Mul => a.checked_mul(b),
        ArithOp::Div => {
            if b == 0 {
                return Err(division_by_zero());
            }
            a.checked_div(b)
        }
        ArithOp::Mod => {
            if b == 0 {
                return Err(division_by_zero());
            }
            return Ok(Value::Int8(a.checked_rem(b).unwrap_or(0)));
        }
        _ => unreachable!(),
    };
    result
        .map(Value::Int8)
        .ok_or_else(|| out_of_range(PgType::Int8))
}

fn arith_f4(op: ArithOp, a: f32, b: f32) -> Result<Value, ArithError> {
    let r = match op {
        ArithOp::Add => float::f4_add(a, b),
        ArithOp::Sub => float::f4_sub(a, b),
        ArithOp::Mul => float::f4_mul(a, b),
        ArithOp::Div => float::f4_div(a, b),
        other => unreachable!("float4 arithmetic {other:?}"),
    };
    r.map(Value::Float4).map_err(float_error)
}

fn arith_f8(op: ArithOp, a: f64, b: f64) -> Result<Value, ArithError> {
    let r = match op {
        ArithOp::Add => float::f8_add(a, b),
        ArithOp::Sub => float::f8_sub(a, b),
        ArithOp::Mul => float::f8_mul(a, b),
        ArithOp::Div => float::f8_div(a, b),
        ArithOp::Pow => float::f8_pow(a, b),
        other => unreachable!("float8 arithmetic {other:?}"),
    };
    r.map(Value::Float8).map_err(float_error)
}

fn arith_numeric(op: ArithOp, a: &Numeric, b: &Numeric) -> Result<Value, ArithError> {
    let r = match op {
        ArithOp::Add => a.add(b),
        ArithOp::Sub => a.sub(b),
        ArithOp::Mul => a.mul(b),
        ArithOp::Div => a.div(b).map_err(numeric_error)?,
        ArithOp::Mod => a.modulo(b).map_err(numeric_error)?,
        other => unreachable!("numeric arithmetic {other:?}"),
    };
    Ok(Value::Numeric(r))
}
