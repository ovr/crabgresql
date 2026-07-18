//! Expression evaluation over one row.
//!
//! Types were settled at bind time: every `Binary` node carries its operand
//! type and `Coerce` nodes mark the only runtime casts, so evaluation
//! dispatches on recorded types and never re-infers. SQL three-valued logic
//! applies throughout: a NULL operand nulls out comparisons and arithmetic,
//! and AND/OR follow the Kleene truth tables.

use std::cmp::Ordering;

use crabgresql_binder::{BinOp, BoundExpr, UnaryOp};
use crabgresql_pg_wire::sqlstate;
use crabgresql_types::{
    Interval, Numeric, PgType, TimeTz, Value, cast, date, float, interval, time, timetz,
};

use crate::{ExecContext, ExecError};

pub fn eval(expr: &BoundExpr, row: &[Value], ctx: ExecContext) -> Result<Value, ExecError> {
    match expr {
        BoundExpr::Const { value, .. } => Ok(value.clone()),
        BoundExpr::ColumnRef { index, .. } => Ok(row[*index].clone()),
        BoundExpr::Unary { op, expr } => eval_unary(*op, eval(expr, row, ctx)?),
        BoundExpr::Binary {
            op,
            arg_ty,
            left,
            right,
        } => eval_binary(*op, *arg_ty, left, right, row, ctx),
        BoundExpr::IsNull { expr, negated } => {
            let is_null = matches!(eval(expr, row, ctx)?, Value::Null);
            Ok(Value::Bool(is_null != *negated))
        }
        BoundExpr::Coerce { expr, ty } => coerce_value(eval(expr, row, ctx)?, *ty, ctx),
        BoundExpr::FuncCall { func, args, .. } => {
            let arg_values = args
                .iter()
                .map(|a| eval(a, row, ctx))
                .collect::<Result<Vec<_>, _>>()?;
            crate::scalar_fns::eval_scalar(*func, &arg_values)
        }
        // CASE tests conditions top-to-bottom and evaluates only the winning
        // branch's result (false and NULL conditions both skip); a missing ELSE
        // yields NULL.
        BoundExpr::Case { whens, else_, .. } => {
            for (cond, result) in whens {
                if matches!(eval(cond, row, ctx)?, Value::Bool(true)) {
                    return eval(result, row, ctx);
                }
            }
            match else_ {
                Some(e) => eval(e, row, ctx),
                None => Ok(Value::Null),
            }
        }
        // An SRF marker only expands via the `ProjectSet` node; reaching scalar
        // evaluation means it appeared where a set is not allowed (WHERE, an
        // operator argument, ORDER BY, ...). PG reports this as 0A000.
        BoundExpr::Srf { .. } => Err(ExecError::new(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "set-valued function called in context that cannot accept a set",
        )),
    }
}

/// Runtime side of a bind-time `Coerce` node, via the shared cast machinery.
/// NULL passes through any cast.
pub fn coerce_value(value: Value, ty: PgType, ctx: ExecContext) -> Result<Value, ExecError> {
    cast::cast_value(value, ty, ctx.extra_float_digits)
        .map_err(|e| ExecError::new(e.sqlstate, e.message))
}

fn eval_unary(op: UnaryOp, operand: Value) -> Result<Value, ExecError> {
    match (op, operand) {
        (_, Value::Null) => Ok(Value::Null),
        (UnaryOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (UnaryOp::Neg, Value::Int4(v)) => v
            .checked_neg()
            .map(Value::Int4)
            .ok_or_else(|| out_of_range(PgType::Int4)),
        (UnaryOp::Neg, Value::Int8(v)) => v
            .checked_neg()
            .map(Value::Int8)
            .ok_or_else(|| out_of_range(PgType::Int8)),
        (UnaryOp::Neg, Value::Int2(v)) => v
            .checked_neg()
            .map(Value::Int2)
            .ok_or_else(|| out_of_range(PgType::Int2)),
        (UnaryOp::Neg, Value::Float4(v)) => Ok(Value::Float4(-v)),
        (UnaryOp::Neg, Value::Float8(v)) => Ok(Value::Float8(-v)),
        (UnaryOp::Neg, Value::Numeric(v)) => Ok(Value::Numeric(v.neg())),
        (UnaryOp::Abs, Value::Int2(v)) => v
            .checked_abs()
            .map(Value::Int2)
            .ok_or_else(|| out_of_range(PgType::Int2)),
        (UnaryOp::Abs, Value::Int4(v)) => v
            .checked_abs()
            .map(Value::Int4)
            .ok_or_else(|| out_of_range(PgType::Int4)),
        (UnaryOp::Abs, Value::Int8(v)) => v
            .checked_abs()
            .map(Value::Int8)
            .ok_or_else(|| out_of_range(PgType::Int8)),
        (UnaryOp::Abs, Value::Float4(v)) => Ok(Value::Float4(v.abs())),
        (UnaryOp::Abs, Value::Float8(v)) => Ok(Value::Float8(v.abs())),
        (UnaryOp::Abs, Value::Numeric(v)) => Ok(Value::Numeric(v.abs())),
        (UnaryOp::Sqrt, Value::Float8(v)) => {
            float::f8_sqrt(v).map(Value::Float8).map_err(float_error)
        }
        (UnaryOp::Cbrt, Value::Float8(v)) => Ok(Value::Float8(float::f8_cbrt(v))),
        (op, operand) => unreachable!("binder let through {op:?} on {operand:?}"),
    }
}

fn eval_binary(
    op: BinOp,
    arg_ty: PgType,
    left: &BoundExpr,
    right: &BoundExpr,
    row: &[Value],
    ctx: ExecContext,
) -> Result<Value, ExecError> {
    // AND/OR evaluate lazily left-to-right, as PG does at runtime.
    if let BinOp::And | BinOp::Or = op {
        return eval_logic(op, left, right, row, ctx);
    }
    let l = eval(left, row, ctx)?;
    let r = eval(right, row, ctx)?;
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    if op.is_arithmetic() {
        return match arg_ty {
            PgType::Int2 => eval_arith_int2(op, int2(&l), int2(&r)),
            PgType::Int4 => eval_arith_int4(op, int4(&l), int4(&r)),
            PgType::Int8 => eval_arith_int8(op, int8(&l), int8(&r)),
            PgType::Float4 => eval_arith_f4(op, float4(&l), float4(&r)),
            PgType::Float8 => eval_arith_f8(op, float8(&l), float8(&r)),
            PgType::Numeric => eval_arith_numeric(op, numeric(&l), numeric(&r)),
            other => unreachable!("binder let arithmetic through on {other:?}"),
        };
    }
    let ordering = compare_values(arg_ty, &l, &r);
    let result = match op {
        BinOp::Eq => ordering.is_eq(),
        BinOp::NotEq => ordering.is_ne(),
        BinOp::Lt => ordering.is_lt(),
        BinOp::LtEq => ordering.is_le(),
        BinOp::Gt => ordering.is_gt(),
        BinOp::GtEq => ordering.is_ge(),
        _ => unreachable!(),
    };
    Ok(Value::Bool(result))
}

/// Total-order comparison of two non-null values of type `ty`. Floats use PG's
/// total order (NaN sorts greatest, `NaN = NaN`), so this also drives ORDER BY.
pub fn compare_values(ty: PgType, l: &Value, r: &Value) -> Ordering {
    match ty {
        PgType::Int2 => int2(l).cmp(&int2(r)),
        PgType::Int4 => int4(l).cmp(&int4(r)),
        PgType::Int8 => int8(l).cmp(&int8(r)),
        PgType::Float4 => float::f4_cmp(float4(l), float4(r)),
        PgType::Float8 => float::f8_cmp(float8(l), float8(r)),
        // Byte-order comparison: C-collation semantics until collations land.
        PgType::Text => text(l).cmp(text(r)),
        PgType::Bytea => bytea(l).cmp(bytea(r)),
        // false < true, as in PG.
        PgType::Bool => bool_of(l).cmp(&bool_of(r)),
        // Microsecond order; the ±infinity sentinels sort naturally.
        PgType::Timestamp => timestamp_of(l).cmp(&timestamp_of(r)),
        PgType::TimestampTz => timestamptz_of(l).cmp(&timestamptz_of(r)),
        // Canonical-span order (30-day months, 24-hour days), infinities first/last.
        PgType::Interval => interval::cmp(interval_of(l), interval_of(r)),
        // Arbitrary-precision total order; NaN sorts greatest (== itself).
        PgType::Numeric => numeric(l).cmp(numeric(r)),
        // Day order (the ±infinity sentinels sort naturally); microsecond order;
        // UTC-instant-then-zone order.
        PgType::Date => date::cmp(date_of(l), date_of(r)),
        PgType::Time => time::cmp(time_of(l), time_of(r)),
        PgType::TimeTz => timetz::cmp(timetz_of(l), timetz_of(r)),
        other => unreachable!("comparison not supported for {other:?}"),
    }
}

/// Kleene three-valued AND/OR with left-to-right lazy evaluation: the right
/// side only runs when the left side has not decided the result.
fn eval_logic(
    op: BinOp,
    left: &BoundExpr,
    right: &BoundExpr,
    row: &[Value],
    ctx: ExecContext,
) -> Result<Value, ExecError> {
    // The operand value that decides the result on its own: false for AND,
    // true for OR.
    let decisive = op == BinOp::Or;
    let l = eval(left, row, ctx)?;
    if let Value::Bool(b) = l
        && b == decisive
    {
        return Ok(Value::Bool(decisive));
    }
    let r = eval(right, row, ctx)?;
    Ok(match (l, r) {
        (_, Value::Bool(b)) if b == decisive => Value::Bool(decisive),
        (Value::Null, _) | (_, Value::Null) => Value::Null,
        _ => Value::Bool(!decisive),
    })
}

fn int2(v: &Value) -> i16 {
    match v {
        Value::Int2(v) => *v,
        other => unreachable!("expected int2, got {other:?}"),
    }
}

fn int4(v: &Value) -> i32 {
    match v {
        Value::Int4(v) => *v,
        other => unreachable!("expected int4, got {other:?}"),
    }
}

fn int8(v: &Value) -> i64 {
    match v {
        Value::Int8(v) => *v,
        other => unreachable!("expected int8, got {other:?}"),
    }
}

fn float4(v: &Value) -> f32 {
    match v {
        Value::Float4(v) => *v,
        other => unreachable!("expected float4, got {other:?}"),
    }
}

fn float8(v: &Value) -> f64 {
    match v {
        Value::Float8(v) => *v,
        other => unreachable!("expected float8, got {other:?}"),
    }
}

fn numeric(v: &Value) -> &Numeric {
    match v {
        Value::Numeric(n) => n,
        other => unreachable!("expected numeric, got {other:?}"),
    }
}

fn text(v: &Value) -> &str {
    match v {
        Value::Text(s) => s,
        other => unreachable!("expected text, got {other:?}"),
    }
}

fn bytea(v: &Value) -> &[u8] {
    match v {
        Value::Bytea(b) => b,
        other => unreachable!("expected bytea, got {other:?}"),
    }
}

fn bool_of(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        other => unreachable!("expected bool, got {other:?}"),
    }
}

fn timestamp_of(v: &Value) -> i64 {
    match v {
        Value::Timestamp(t) => *t,
        other => unreachable!("expected timestamp, got {other:?}"),
    }
}

fn interval_of(v: &Value) -> Interval {
    match v {
        Value::Interval(iv) => *iv,
        other => unreachable!("expected interval, got {other:?}"),
    }
}

fn timestamptz_of(v: &Value) -> i64 {
    match v {
        Value::TimestampTz(t) => *t,
        other => unreachable!("expected timestamptz, got {other:?}"),
    }
}

fn date_of(v: &Value) -> i32 {
    match v {
        Value::Date(d) => *d,
        other => unreachable!("expected date, got {other:?}"),
    }
}

fn time_of(v: &Value) -> i64 {
    match v {
        Value::Time(t) => *t,
        other => unreachable!("expected time, got {other:?}"),
    }
}

fn timetz_of(v: &Value) -> TimeTz {
    match v {
        Value::TimeTz(t) => *t,
        other => unreachable!("expected timetz, got {other:?}"),
    }
}

fn out_of_range(ty: PgType) -> ExecError {
    let message = match ty {
        PgType::Int2 => "smallint out of range",
        PgType::Int4 => "integer out of range",
        PgType::Int8 => "bigint out of range",
        _ => unreachable!(),
    };
    ExecError::new(sqlstate::NUMERIC_VALUE_OUT_OF_RANGE, message)
}

fn float_error(e: float::FloatError) -> ExecError {
    ExecError::new(e.sqlstate, e.message)
}

fn division_by_zero() -> ExecError {
    ExecError::new(sqlstate::DIVISION_BY_ZERO, "division by zero")
}

fn eval_arith_int2(op: BinOp, a: i16, b: i16) -> Result<Value, ExecError> {
    let result = match op {
        BinOp::Add => a.checked_add(b),
        BinOp::Sub => a.checked_sub(b),
        BinOp::Mul => a.checked_mul(b),
        BinOp::Div => {
            if b == 0 {
                return Err(division_by_zero());
            }
            a.checked_div(b)
        }
        BinOp::Mod => {
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

fn eval_arith_int4(op: BinOp, a: i32, b: i32) -> Result<Value, ExecError> {
    let result = match op {
        BinOp::Add => a.checked_add(b),
        BinOp::Sub => a.checked_sub(b),
        BinOp::Mul => a.checked_mul(b),
        BinOp::Div => {
            if b == 0 {
                return Err(division_by_zero());
            }
            // MIN / -1 overflows; MIN % -1 is 0 in PG, but checked_rem
            // refuses it, so special-case below.
            a.checked_div(b)
        }
        BinOp::Mod => {
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

fn eval_arith_int8(op: BinOp, a: i64, b: i64) -> Result<Value, ExecError> {
    let result = match op {
        BinOp::Add => a.checked_add(b),
        BinOp::Sub => a.checked_sub(b),
        BinOp::Mul => a.checked_mul(b),
        BinOp::Div => {
            if b == 0 {
                return Err(division_by_zero());
            }
            a.checked_div(b)
        }
        BinOp::Mod => {
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

fn eval_arith_f4(op: BinOp, a: f32, b: f32) -> Result<Value, ExecError> {
    let r = match op {
        BinOp::Add => float::f4_add(a, b),
        BinOp::Sub => float::f4_sub(a, b),
        BinOp::Mul => float::f4_mul(a, b),
        BinOp::Div => float::f4_div(a, b),
        other => unreachable!("float4 arithmetic {other:?}"),
    };
    r.map(Value::Float4).map_err(float_error)
}

fn eval_arith_f8(op: BinOp, a: f64, b: f64) -> Result<Value, ExecError> {
    let r = match op {
        BinOp::Add => float::f8_add(a, b),
        BinOp::Sub => float::f8_sub(a, b),
        BinOp::Mul => float::f8_mul(a, b),
        BinOp::Div => float::f8_div(a, b),
        BinOp::Pow => float::f8_pow(a, b),
        other => unreachable!("float8 arithmetic {other:?}"),
    };
    r.map(Value::Float8).map_err(float_error)
}

fn eval_arith_numeric(op: BinOp, a: &Numeric, b: &Numeric) -> Result<Value, ExecError> {
    let r = match op {
        BinOp::Add => a.add(b),
        BinOp::Sub => a.sub(b),
        BinOp::Mul => a.mul(b),
        BinOp::Div => a.div(b).map_err(numeric_error)?,
        BinOp::Mod => a.modulo(b).map_err(numeric_error)?,
        other => unreachable!("numeric arithmetic {other:?}"),
    };
    Ok(Value::Numeric(r))
}

fn numeric_error(e: crabgresql_types::numeric::NumErr) -> ExecError {
    ExecError::new(e.sqlstate, e.message).with_detail(e.detail)
}
