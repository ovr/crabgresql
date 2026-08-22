//! Constant folding: replace a subtree whose every leaf is already a constant
//! with the constant it evaluates to.
//!
//! The obvious implementation — hand the subtree to the executor's evaluator —
//! is not available: the executor sits *above* this crate. So the arithmetic,
//! the comparisons and the casts are taken from `crabgresql-types`, which both
//! this crate and the executor share; that is the whole reason
//! [`crabgresql_types::arith`] exists as its own module. Nothing here
//! re-implements a value operation the executor implements separately.
//!
//! # What is not folded
//!
//! - `ColumnRef` / `Param` / `OuterColumnRef` — runtime values, not constants.
//! - `Aggregate` / `WindowFunc` / `Srf` — markers a plan node computes.
//! - `ScalarSubquery` / `ArraySubquery` / `Exists` / `QuantifiedSubquery` — the executor's
//!   `resolve_subqueries` folds these once the statement runs, where a
//!   transaction to run the subplan against exists.
//! - `Routine` — a PL/pgSQL body is an imperative program needing the
//!   interpreter, and PostgreSQL defaults a routine to `VOLATILE`.
//! - `FuncCall` — TODO: fold a non-volatile scalar call (`upper('abc')`), which
//!   needs `eval_scalar` in a crate below this one; it lives in the executor,
//!   which is above it. Until then such a call runs per row.
//! - `Collate` is never *removed*, only descended into: the collation it
//!   carries is what `expr_collation` reads to settle how a comparison or a sort
//!   orders strings.
//!
//! # Errors are not raised here
//!
//! An expression that fails to evaluate — `1/0`, an out-of-range cast — is left
//! exactly as it was, and the error surfaces at execution as it does today.
//! Folding must not make `SELECT 1/0 FROM empty_table` start failing, and
//! leaving the node alone also keeps `WHERE false AND 1/0 = 1` lazy: the
//! unfolded `1/0` makes its parent non-constant, so the AND is never folded
//! either.

use std::cmp::Ordering;

use crabgresql_binder::{BinOp, BoundExpr, MinMaxKind, Subplan, UnaryOp};
use crabgresql_types::compare::{compare_values, compare_values_collated};
use crabgresql_types::{FmtCtx, PgType, Value, arith, cast};

/// Rewrite `expr` bottom up: fold every constant subtree to its value, then let
/// [`super::simplifier`] rewrite what the new constants opened up. Returns
/// whether anything changed.
///
/// `on_subplan` is called for each subquery body reached on the way down — the
/// body is a plan of its own, so what to do with it is the caller's business
/// (see [`super::SimplifyExpressions`], which recurses into it).
pub(super) fn fold(
    expr: &mut BoundExpr,
    fmt: &FmtCtx,
    on_subplan: &mut dyn FnMut(&mut Subplan) -> bool,
) -> bool {
    let mut changed = fold_children(expr, fmt, on_subplan);
    if !matches!(expr, BoundExpr::Const { .. })
        && foldable(expr)
        && let Some(value) = eval_const(expr, fmt)
    {
        // The node's *declared* type, taken before the node is replaced: a NULL
        // result carries no type of its own, and a `Coerce` to `text` of a value
        // that is already `Value::Text` must still report `text`.
        let ty = expr.ty();
        *expr = BoundExpr::Const { value, ty };
        changed = true;
    }
    changed | super::simplifier::simplify(expr)
}

/// Recurse into every child expression, and hand every subquery body to
/// `on_subplan`.
fn fold_children(
    expr: &mut BoundExpr,
    fmt: &FmtCtx,
    on_subplan: &mut dyn FnMut(&mut Subplan) -> bool,
) -> bool {
    let mut changed = false;
    match expr {
        BoundExpr::Const { .. }
        | BoundExpr::ColumnRef { .. }
        | BoundExpr::Param { .. }
        | BoundExpr::OuterColumnRef { .. } => {}
        BoundExpr::ScalarSubquery { subplan, .. }
        | BoundExpr::ArraySubquery { subplan, .. }
        | BoundExpr::Exists { subplan, .. } => {
            changed |= on_subplan(subplan);
        }
        BoundExpr::Unary { expr, .. }
        | BoundExpr::IsNull { expr, .. }
        | BoundExpr::BoolTest { expr, .. }
        | BoundExpr::Coerce { expr, .. }
        | BoundExpr::Collate { expr, .. }
        | BoundExpr::Reinterpret { expr, .. } => changed |= fold(expr, fmt, on_subplan),
        BoundExpr::Binary { left, right, .. } => {
            changed |= fold(left, fmt, on_subplan);
            changed |= fold(right, fmt, on_subplan);
        }
        BoundExpr::FuncCall { args, .. }
        | BoundExpr::Routine { args, .. }
        | BoundExpr::Srf { args, .. }
        | BoundExpr::Coalesce { args, .. }
        | BoundExpr::MinMax { args, .. }
        | BoundExpr::Aggregate { args, .. } => {
            for arg in args {
                changed |= fold(arg, fmt, on_subplan);
            }
        }
        BoundExpr::ArrayCtor { elems, .. } => {
            for elem in elems {
                changed |= fold(elem, fmt, on_subplan);
            }
        }
        BoundExpr::Subscript { base, index, .. } => {
            changed |= fold(base, fmt, on_subplan);
            changed |= fold(index, fmt, on_subplan);
        }
        BoundExpr::Case { whens, else_, .. } => {
            for (condition, result) in whens {
                changed |= fold(condition, fmt, on_subplan);
                changed |= fold(result, fmt, on_subplan);
            }
            if let Some(else_) = else_ {
                changed |= fold(else_, fmt, on_subplan);
            }
        }
        BoundExpr::WindowFunc { kind, spec, .. } => {
            for arg in kind.args_mut().iter_mut().chain(spec.exprs_mut()) {
                changed |= fold(arg, fmt, on_subplan);
            }
        }
        // The needle of `x IN (SELECT …)` is an ordinary expression of this
        // level even though the subplan beside it is not.
        BoundExpr::QuantifiedSubquery { subplan, cmp, .. } => {
            changed |= on_subplan(subplan);
            changed |= fold_template(cmp, fmt, on_subplan);
        }
        BoundExpr::QuantifiedArray { array, cmp, .. } => {
            changed |= fold(array, fmt, on_subplan);
            changed |= fold_template(cmp, fmt, on_subplan);
        }
    }
    changed
}

/// The `cmp` of an `ANY`/`ALL` is a **template**, not an expression: it is
/// `needle op <hole>`, and the executor substitutes each candidate into that
/// hole. Its shape is load-bearing, so only the needle is folded here — folding
/// the comparison itself would collapse the whole template to `Const(NULL)`, and
/// folding the hole's coercion chain would change what every candidate is cast
/// to.
///
/// TODO: fold the needle of a call-shaped template (`~~`, `~`, `@>`, …), which
/// needs the same off-the-hole-path walk the executor does.
fn fold_template(
    cmp: &mut BoundExpr,
    fmt: &FmtCtx,
    on_subplan: &mut dyn FnMut(&mut Subplan) -> bool,
) -> bool {
    match cmp {
        BoundExpr::Binary { left, .. } => fold(left, fmt, on_subplan),
        _ => false,
    }
}

/// Whether this node may be replaced by its value, assuming its children are
/// constants. See the module docs for why each excluded variant is excluded.
fn foldable(expr: &BoundExpr) -> bool {
    matches!(
        expr,
        BoundExpr::Unary { .. }
            | BoundExpr::Binary { .. }
            | BoundExpr::IsNull { .. }
            | BoundExpr::BoolTest { .. }
            | BoundExpr::Coerce { .. }
            | BoundExpr::Reinterpret { .. }
            | BoundExpr::ArrayCtor { .. }
            | BoundExpr::Case { .. }
            | BoundExpr::Coalesce { .. }
            | BoundExpr::MinMax { .. }
    )
}

/// The value of a constant expression, or `None` if it is not constant or does
/// not evaluate. The two are deliberately the same answer here — both mean
/// "leave the node alone".
///
/// Note this is *not* a second evaluator: every case either delegates to
/// `crabgresql-types` (arithmetic, comparison, casts) or is the three-valued
/// logic of a node that has no value operation at all (`IS NULL`, `CASE`).
fn eval_const(expr: &BoundExpr, fmt: &FmtCtx) -> Option<Value> {
    match expr {
        BoundExpr::Const { value, .. } => Some(value.clone()),
        // Value-transparent: the collation labels the operand, it does not
        // change it.
        BoundExpr::Collate { expr, .. } => eval_const(expr, fmt),
        BoundExpr::Unary { op, expr } => {
            arith::eval_unary(unary_arith_op(*op), eval_const(expr, fmt)?).ok()
        }
        BoundExpr::Binary {
            op,
            arg_ty,
            collation,
            left,
            right,
        } => eval_binary(*op, *arg_ty, *collation, left, right, fmt),
        BoundExpr::IsNull { expr, negated } => {
            let is_null = matches!(eval_const(expr, fmt)?, Value::Null);
            Some(Value::Bool(is_null != *negated))
        }
        BoundExpr::BoolTest {
            expr,
            value,
            negated,
        } => {
            let hit = match (eval_const(expr, fmt)?, value) {
                (Value::Bool(b), Some(want)) => b == *want,
                (Value::Null, None) => true,
                (Value::Bool(_), None) | (Value::Null, Some(_)) => false,
                // A non-boolean operand is a binder invariant break. The
                // executor reports it; an optimizer stays silent and leaves the
                // node for the executor to report.
                _ => return None,
            };
            Some(Value::Bool(hit != *negated))
        }
        BoundExpr::Coerce { expr, ty } => cast::cast_value(eval_const(expr, fmt)?, *ty, fmt).ok(),
        BoundExpr::Reinterpret { expr, rep, .. } => {
            cast::reinterpret_value(eval_const(expr, fmt)?, *rep).ok()
        }
        BoundExpr::ArrayCtor { elem, elems, .. } => {
            let values = elems
                .iter()
                .map(|e| eval_const(e, fmt))
                .collect::<Option<Vec<_>>>()?;
            Some(Value::Array {
                elem: *elem,
                elems: values,
            })
        }
        // Lazy, as at run time: only the arm that is reached is evaluated, so
        // `CASE WHEN false THEN 1/0 ELSE 1 END` folds to `1`.
        BoundExpr::Case { whens, else_, .. } => {
            for (condition, result) in whens {
                if matches!(eval_const(condition, fmt)?, Value::Bool(true)) {
                    return eval_const(result, fmt);
                }
            }
            match else_ {
                Some(else_) => eval_const(else_, fmt),
                None => Some(Value::Null),
            }
        }
        // Likewise lazy: `coalesce(1, 1/0)` is `1`.
        BoundExpr::Coalesce { args, .. } => {
            for arg in args {
                match eval_const(arg, fmt)? {
                    Value::Null => {}
                    value => return Some(value),
                }
            }
            Some(Value::Null)
        }
        // Nothing short-circuits, so `greatest(1, 1/0)` folds to nothing and
        // stays for the executor to fail on.
        BoundExpr::MinMax {
            kind,
            args,
            ty,
            collation,
        } => {
            let want = match kind {
                MinMaxKind::Greatest => Ordering::Greater,
                MinMaxKind::Least => Ordering::Less,
            };
            let mut best = Value::Null;
            for arg in args {
                let value = eval_const(arg, fmt)?;
                if matches!(value, Value::Null) {
                    continue;
                }
                if matches!(best, Value::Null)
                    || compare_values_collated(*ty, &value, &best, *collation) == want
                {
                    best = value;
                }
            }
            Some(best)
        }
        BoundExpr::ColumnRef { .. }
        | BoundExpr::Param { .. }
        | BoundExpr::OuterColumnRef { .. }
        | BoundExpr::FuncCall { .. }
        | BoundExpr::Routine { .. }
        | BoundExpr::Srf { .. }
        | BoundExpr::Subscript { .. }
        | BoundExpr::Aggregate { .. }
        | BoundExpr::WindowFunc { .. }
        | BoundExpr::ScalarSubquery { .. }
        | BoundExpr::ArraySubquery { .. }
        | BoundExpr::Exists { .. }
        | BoundExpr::QuantifiedSubquery { .. }
        | BoundExpr::QuantifiedArray { .. } => None,
    }
}

fn eval_binary(
    op: BinOp,
    arg_ty: PgType,
    collation: u32,
    left: &BoundExpr,
    right: &BoundExpr,
    fmt: &FmtCtx,
) -> Option<Value> {
    if let BinOp::And | BinOp::Or = op {
        return eval_logic(op, left, right, fmt);
    }
    let l = eval_const(left, fmt)?;
    let r = eval_const(right, fmt)?;
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Some(Value::Null);
    }
    if op.is_arithmetic() {
        return arith::eval_arith(arith_op(op), arg_ty, &l, &r).ok();
    }
    Some(Value::Bool(apply_comparison(op, arg_ty, collation, &l, &r)))
}

/// AND/OR under SQL's three-valued logic, evaluated left to right and stopping
/// at the operand that decides the answer on its own — the same order and the
/// same short-circuit the executor uses, so an unevaluable right operand cannot
/// change a decided result.
fn eval_logic(op: BinOp, left: &BoundExpr, right: &BoundExpr, fmt: &FmtCtx) -> Option<Value> {
    let decisive = op == BinOp::Or;
    let l = eval_const(left, fmt)?;
    if let Value::Bool(b) = l
        && b == decisive
    {
        return Some(Value::Bool(decisive));
    }
    let r = eval_const(right, fmt)?;
    Some(match (l, r) {
        (_, Value::Bool(b)) if b == decisive => Value::Bool(decisive),
        (Value::Null, _) | (_, Value::Null) => Value::Null,
        _ => Value::Bool(!decisive),
    })
}

/// A comparison of two non-NULL constants. Mirrors the executor's
/// `apply_comparison`, including its equality short-circuit: every supported
/// collation is deterministic, so equal bytes and equal values coincide and
/// equality never needs the collator.
fn apply_comparison(op: BinOp, arg_ty: PgType, collation: u32, l: &Value, r: &Value) -> bool {
    if matches!(op, BinOp::Eq | BinOp::NotEq) {
        let eq = compare_values(arg_ty, l, r).is_eq();
        return if op == BinOp::Eq { eq } else { !eq };
    }
    let ordering = compare_values_collated(arg_ty, l, r, collation);
    match op {
        BinOp::Lt => ordering.is_lt(),
        BinOp::LtEq => ordering.is_le(),
        BinOp::Gt => ordering.is_gt(),
        BinOp::GtEq => ordering.is_ge(),
        other => unreachable!("{other:?} is not a comparison"),
    }
}

/// The binder's `UnaryOp` in the types crate's spelling; the binder is a later
/// crate, so the shared implementation cannot name its enum.
fn unary_arith_op(op: UnaryOp) -> arith::UnaryArithOp {
    match op {
        UnaryOp::Not => arith::UnaryArithOp::Not,
        UnaryOp::Neg => arith::UnaryArithOp::Neg,
        UnaryOp::Abs => arith::UnaryArithOp::Abs,
        UnaryOp::Sqrt => arith::UnaryArithOp::Sqrt,
        UnaryOp::Cbrt => arith::UnaryArithOp::Cbrt,
    }
}

/// Likewise for the arithmetic subset of `BinOp`. Only reached under
/// [`BinOp::is_arithmetic`].
fn arith_op(op: BinOp) -> arith::ArithOp {
    match op {
        BinOp::Add => arith::ArithOp::Add,
        BinOp::Sub => arith::ArithOp::Sub,
        BinOp::Mul => arith::ArithOp::Mul,
        BinOp::Div => arith::ArithOp::Div,
        BinOp::Mod => arith::ArithOp::Mod,
        BinOp::Pow => arith::ArithOp::Pow,
        other => unreachable!("{other:?} is not arithmetic"),
    }
}
