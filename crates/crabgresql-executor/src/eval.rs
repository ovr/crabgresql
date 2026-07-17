//! Expression evaluation over one row.
//!
//! Types were settled at bind time: every `Binary` node carries its operand
//! type and `Coerce` nodes mark the only runtime casts, so evaluation
//! dispatches on recorded types and never re-infers. SQL three-valued logic
//! applies throughout: a NULL operand nulls out comparisons and arithmetic,
//! and AND/OR follow the Kleene truth tables.

use crabgresql_binder::{BinOp, BoundExpr, UnaryOp};
use crabgresql_protocol::sqlstate;
use crabgresql_types::{PgType, Value};

use crate::ExecError;

pub fn eval(expr: &BoundExpr, row: &[Value]) -> Result<Value, ExecError> {
    match expr {
        BoundExpr::Const { value, .. } => Ok(value.clone()),
        BoundExpr::ColumnRef { index, .. } => Ok(row[*index].clone()),
        BoundExpr::Unary { op, expr } => eval_unary(*op, eval(expr, row)?),
        BoundExpr::Binary {
            op,
            arg_ty,
            left,
            right,
        } => eval_binary(*op, *arg_ty, left, right, row),
        BoundExpr::IsNull { expr, negated } => {
            let is_null = matches!(eval(expr, row)?, Value::Null);
            Ok(Value::Bool(is_null != *negated))
        }
        BoundExpr::Coerce { expr, ty } => coerce_value(eval(expr, row)?, *ty),
    }
}

/// Runtime side of a bind-time `Coerce` node: int4→int8 widens, int8→int4
/// range-checks. NULL passes through any cast.
pub fn coerce_value(value: Value, ty: PgType) -> Result<Value, ExecError> {
    match (value, ty) {
        (Value::Null, _) => Ok(Value::Null),
        (Value::Int4(v), PgType::Int8) => Ok(Value::Int8(v as i64)),
        (Value::Int8(v), PgType::Int4) => match i32::try_from(v) {
            Ok(v) => Ok(Value::Int4(v)),
            Err(_) => Err(ExecError::new(
                sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
                "integer out of range",
            )),
        },
        (value, _) => Ok(value),
    }
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
        (op, operand) => unreachable!("binder let through {op:?} on {operand:?}"),
    }
}

fn eval_binary(
    op: BinOp,
    arg_ty: PgType,
    left: &BoundExpr,
    right: &BoundExpr,
    row: &[Value],
) -> Result<Value, ExecError> {
    // AND/OR evaluate lazily left-to-right, as PG does at runtime.
    if let BinOp::And | BinOp::Or = op {
        return eval_logic(op, left, right, row);
    }
    let l = eval(left, row)?;
    let r = eval(right, row)?;
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    if op.is_arithmetic() {
        return match arg_ty {
            PgType::Int4 => eval_arith_int4(op, int4(&l), int4(&r)),
            PgType::Int8 => eval_arith_int8(op, int8(&l), int8(&r)),
            other => unreachable!("binder let arithmetic through on {other:?}"),
        };
    }
    let ordering = match arg_ty {
        PgType::Int4 => int4(&l).cmp(&int4(&r)),
        PgType::Int8 => int8(&l).cmp(&int8(&r)),
        // Byte-order comparison: C-collation semantics until collations land.
        PgType::Text => text(&l).cmp(text(&r)),
        // false < true, as in PG.
        PgType::Bool => bool_of(&l).cmp(&bool_of(&r)),
    };
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

/// Kleene three-valued AND/OR with left-to-right lazy evaluation: the right
/// side only runs when the left side has not decided the result.
fn eval_logic(
    op: BinOp,
    left: &BoundExpr,
    right: &BoundExpr,
    row: &[Value],
) -> Result<Value, ExecError> {
    // The operand value that decides the result on its own: false for AND,
    // true for OR.
    let decisive = op == BinOp::Or;
    let l = eval(left, row)?;
    if let Value::Bool(b) = l
        && b == decisive
    {
        return Ok(Value::Bool(decisive));
    }
    let r = eval(right, row)?;
    Ok(match (l, r) {
        (_, Value::Bool(b)) if b == decisive => Value::Bool(decisive),
        (Value::Null, _) | (_, Value::Null) => Value::Null,
        _ => Value::Bool(!decisive),
    })
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

fn text(v: &Value) -> &str {
    match v {
        Value::Text(s) => s,
        other => unreachable!("expected text, got {other:?}"),
    }
}

fn bool_of(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        other => unreachable!("expected bool, got {other:?}"),
    }
}

fn out_of_range(ty: PgType) -> ExecError {
    let message = match ty {
        PgType::Int4 => "integer out of range",
        PgType::Int8 => "bigint out of range",
        _ => unreachable!(),
    };
    ExecError::new(sqlstate::NUMERIC_VALUE_OUT_OF_RANGE, message)
}

fn division_by_zero() -> ExecError {
    ExecError::new(sqlstate::DIVISION_BY_ZERO, "division by zero")
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
