//! Compiling a bound predicate to Arrow kernels.
//!
//! Only a deliberately narrow subset compiles. [`compile_predicate`] returns
//! `None` for everything else and the caller keeps the row [`Filter`], so the
//! question this module answers is never "how do I vectorize this?" but "can I
//! prove that vectorizing it changes nothing?".
//!
//! # Where Arrow and PostgreSQL disagree
//!
//! Arrow's kernels are IEEE and byte-order operations; PostgreSQL's operators
//! are defined by its own type semantics. Mostly they coincide. The cases where
//! they do not are the entire content of [`comparable`]:
//!
//! - **`numeric`** is stored as `Utf8` — arbitrary precision has no Arrow type —
//!   so an Arrow comparison would compare *text*, making `'9' > '10'`. Excluded.
//! - **`float4`/`float8`** — PostgreSQL defines `NaN = NaN` as true and `NaN` as
//!   greater than everything (`crabgresql_types::float`); IEEE, and so Arrow's
//!   `eq`, says `NaN` equals nothing. Excluded from comparisons. Floats are
//!   still usable as *sort* keys, where the divergence is repairable.
//! - **`bpchar`** compares with trailing blanks trimmed, which no Arrow kernel
//!   does. Excluded.
//! - **`timetz`/`interval`** are `Struct`s with orders of their own — `interval`
//!   compares by canonical span, not field by field. Excluded.
//! - **text ordering** follows the expression's collation, and an ICU collation
//!   is not byte order. `<`/`>` are allowed only under a byte-order collation.
//!   `=`/`<>` are allowed under any of them, because every supported collation
//!   is deterministic, so equality is bytewise regardless.
//!
//! # Three-valued logic
//!
//! `AND`/`OR` compile to Arrow's **Kleene** kernels. The plain `and`/`or` return
//! NULL whenever either side is NULL, but SQL says `false AND NULL` is `false`
//! and `true OR NULL` is `true`. Using the wrong one silently drops rows.
//!
//! At the top, Arrow's `filter` keeps only rows whose mask is `true` and drops
//! `false` and NULL alike — exactly [`crate::predicate_holds`], which passes
//! only `Value::Bool(true)`.

use std::sync::Arc;

use arrow_arith::boolean::{and_kleene, is_not_null, is_null, not, or_kleene};
use arrow_array::{Array, ArrayRef, BooleanArray, Datum, RecordBatch, Scalar};
use arrow_ord::cmp;
use arrow_schema::ArrowError;
use crabgresql_binder::{BinOp, BoundExpr, UnaryOp};
use crabgresql_storage_api::Column;
use crabgresql_storage_api::arrow::build_array;
use crabgresql_planner::vectorize;
use crabgresql_types::PgType;

use super::BatchLayout;
use crate::ExecError;

/// A predicate compiled to Arrow kernels, evaluated once per batch.
pub struct VectorPredicate {
    root: Node,
}

impl VectorPredicate {
    /// The boolean mask for `batch`: `true` keeps the row, `false` and NULL drop
    /// it.
    pub fn evaluate(&self, batch: &RecordBatch) -> Result<BooleanArray, ExecError> {
        self.root.boolean(batch)
    }
}

/// One node of a compiled predicate.
enum Node {
    /// A batch column, by schema ordinal.
    Column(usize),
    /// A length-1 array built once at compile time, broadcast by the kernels.
    Literal(Scalar<ArrayRef>),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Not(Box<Node>),
    IsNull { operand: Box<Node>, negated: bool },
    Compare { op: BinOp, left: Box<Node>, right: Box<Node> },
}

/// An evaluated operand. The distinction is not cosmetic: Arrow broadcasts a
/// `Scalar` across the batch and requires equal lengths otherwise, so a literal
/// that lost its scalar-ness would fail against any batch of more than one row.
enum Operand {
    Array(ArrayRef),
    Scalar(Scalar<ArrayRef>),
}

impl Operand {
    fn datum(&self) -> &dyn Datum {
        match self {
            Operand::Array(array) => array,
            Operand::Scalar(scalar) => scalar,
        }
    }

    /// The underlying values, whether or not this operand broadcasts.
    fn array(&self) -> &dyn Array {
        self.datum().get().0
    }
}

impl Node {
    fn evaluate(&self, batch: &RecordBatch) -> Result<Operand, ExecError> {
        match self {
            Node::Column(index) => batch
                .columns()
                .get(*index)
                .map(|array| Operand::Array(Arc::clone(array)))
                .ok_or_else(|| internal("vectorized predicate names a missing column")),
            Node::Literal(scalar) => Ok(Operand::Scalar(scalar.clone())),
            Node::And(left, right) => {
                let (left, right) = (left.boolean(batch)?, right.boolean(batch)?);
                and_kleene(&left, &right).map(boxed).map_err(kernel_error)
            }
            Node::Or(left, right) => {
                let (left, right) = (left.boolean(batch)?, right.boolean(batch)?);
                or_kleene(&left, &right).map(boxed).map_err(kernel_error)
            }
            Node::Not(operand) => not(&operand.boolean(batch)?).map(boxed).map_err(kernel_error),
            Node::IsNull { operand, negated } => {
                let operand = operand.evaluate(batch)?;
                let array = operand.array();
                let mask = if *negated { is_not_null(array) } else { is_null(array) };
                mask.map(boxed).map_err(kernel_error)
            }
            Node::Compare { op, left, right } => {
                let (left, right) = (left.evaluate(batch)?, right.evaluate(batch)?);
                let (l, r) = (left.datum(), right.datum());
                let mask = match op {
                    BinOp::Eq => cmp::eq(l, r),
                    BinOp::NotEq => cmp::neq(l, r),
                    BinOp::Lt => cmp::lt(l, r),
                    BinOp::LtEq => cmp::lt_eq(l, r),
                    BinOp::Gt => cmp::gt(l, r),
                    BinOp::GtEq => cmp::gt_eq(l, r),
                    // Unreachable: `compile` accepts only the six above.
                    _ => Err(ArrowError::NotYetImplemented("not a comparison".into())),
                };
                mask.map(boxed).map_err(kernel_error)
            }
        }
    }

    fn boolean(&self, batch: &RecordBatch) -> Result<BooleanArray, ExecError> {
        let operand = self.evaluate(batch)?;
        let mask = operand
            .array()
            .as_any()
            .downcast_ref::<BooleanArray>()
            .cloned()
            // Unreachable: `compile` type-checks every boolean position. An
            // error rather than a panic, so a compiler bug fails the query
            // instead of the backend.
            .ok_or_else(|| internal("vectorized predicate expected a boolean operand"))?;
        Ok(fit(mask, batch.num_rows()))
    }
}

/// Broadcast a mask that describes one value to the whole batch.
///
/// A predicate subtree with no column reference — `1 = 1`, `true`, `NULL IS
/// NULL`, or a comparison of two constants — evaluates against `Scalar`
/// operands and so yields a **length-1** mask. Every consumer of a mask assumes
/// it is as tall as the batch, and both of them fail quietly rather than
/// loudly if it is not:
///
/// - `filter_record_batch` only rejects a mask *longer* than the data, so a
///   length-1 mask silently truncates the batch to its first row;
/// - `and_kleene`/`or_kleene` reject unequal lengths outright, so a constant
///   beside a column (`id = 1 AND true`) fails the query.
///
/// Fitting here rather than at either call site covers both, and covers the
/// zero-row batch, where a length-1 mask is *longer* than the data. NULL-ness
/// is carried through: `x AND NULL` must stay NULL, not collapse to false.
fn fit(mask: BooleanArray, rows: usize) -> BooleanArray {
    if mask.len() == rows {
        return mask;
    }
    // Only a scalar can legitimately disagree with the batch height; anything
    // else is a column, which Arrow already guarantees is batch-length.
    if mask.len() != 1 {
        return mask;
    }
    let value = mask.is_valid(0).then(|| mask.value(0));
    std::iter::repeat_n(value, rows).collect()
}

fn boxed(array: BooleanArray) -> Operand {
    Operand::Array(Arc::new(array))
}

fn internal(message: &str) -> ExecError {
    ExecError::new("XX000", message)
}

fn kernel_error(error: ArrowError) -> ExecError {
    ExecError::new("XX000", format!("vectorized evaluation failed: {error}"))
}

/// Compile `predicate` against a batch of `layout`, or `None` if any part of it
/// falls outside the provable subset.
///
/// The planner's [`vectorize::vectorizable_predicate`] is the gate, and it is
/// consulted **first** — so this can only ever accept a subset of what `EXPLAIN`
/// advertised, never a superset. That direction is the one that matters: a plan
/// annotated columnar which then runs on rows is misleading, but a plan that
/// vectorizes work `EXPLAIN` called row-based is undetectable from the outside.
/// A corpus test pins the two to exact agreement.
pub fn compile_predicate(predicate: &BoundExpr, layout: &BatchLayout) -> Option<VectorPredicate> {
    if !vectorize::vectorizable_predicate(predicate, layout.len()) {
        return None;
    }
    let root = compile_bool(predicate, layout)?;
    Some(VectorPredicate { root })
}

/// Compile an expression that must yield `bool`.
fn compile_bool(expr: &BoundExpr, layout: &BatchLayout) -> Option<Node> {
    match expr {
        // Value-transparent: a collation only decides how a *comparison* below
        // orders, and that is read from the comparison's own `collation`.
        BoundExpr::Collate { expr, .. } => compile_bool(expr, layout),
        BoundExpr::Binary { op: BinOp::And, left, right, .. } => Some(Node::And(
            Box::new(compile_bool(left, layout)?),
            Box::new(compile_bool(right, layout)?),
        )),
        BoundExpr::Binary { op: BinOp::Or, left, right, .. } => Some(Node::Or(
            Box::new(compile_bool(left, layout)?),
            Box::new(compile_bool(right, layout)?),
        )),
        BoundExpr::Unary { op: UnaryOp::Not, expr } => {
            Some(Node::Not(Box::new(compile_bool(expr, layout)?)))
        }
        BoundExpr::IsNull { expr, negated } => Some(Node::IsNull {
            operand: Box::new(compile_operand(expr, layout)?),
            negated: *negated,
        }),
        BoundExpr::Binary { op, arg_ty, collation, left, right }
            if matches!(
                op,
                BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq
            ) =>
        {
            vectorize::comparable(*arg_ty, *op, *collation).then_some(())?;
            Some(Node::Compare {
                op: *op,
                left: Box::new(compile_operand(left, layout)?),
                right: Box::new(compile_operand(right, layout)?),
            })
        }
        // A bare boolean column or constant is a legal `WHERE` on its own.
        BoundExpr::ColumnRef { ty: PgType::Bool, .. } | BoundExpr::Const { ty: PgType::Bool, .. } => {
            compile_operand(expr, layout)
        }
        _ => None,
    }
}

/// Compile a comparison operand: a column or a constant, nothing computed.
///
/// Anything else — an arithmetic expression, a function call, a cast, a
/// parameter, a correlated reference — ends the compile. Those are where the
/// row evaluator's side effects and PostgreSQL-specific semantics live, and
/// none of them is worth reproducing before the simple cases are proven.
fn compile_operand(expr: &BoundExpr, layout: &BatchLayout) -> Option<Node> {
    match expr {
        BoundExpr::Collate { expr, .. } => compile_operand(expr, layout),
        BoundExpr::ColumnRef { index, .. } => {
            // Batches are full width in schema order, so a schema ordinal is a
            // batch ordinal. Checked rather than assumed: an out-of-range index
            // would otherwise become a runtime error mid-scan.
            (*index < layout.len()).then_some(Node::Column(*index))
        }
        BoundExpr::Const { value, ty } => {
            let column = Column::new("const", *ty);
            let array = build_array(&column, std::slice::from_ref(&vec![value.clone()]), 0).ok()?;
            Some(Node::Literal(Scalar::new(array)))
        }
        _ => None,
    }
}
