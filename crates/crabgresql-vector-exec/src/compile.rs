//! Compiling a bound expression into a batch expression.
//!
//! Two things happen here, and both are the reason a compile step exists at all:
//!
//! 1. **Index rebasing.** A [`BoundExpr::ColumnRef`] names a *relation* ordinal;
//!    a [`VectorExpr::Column`] names a *batch* position. Resolving that once, per
//!    query, is what lets batches be narrow — the row engine cannot narrow rows
//!    because it addresses columns by relation position at runtime.
//! 2. **The allow-list.** Every construct the vectorized path cannot reproduce
//!    exactly is refused here, by name, in an exhaustive match.

use crabgresql_batch::{ArithOp, BatchSchema, CmpOp, VectorExpr, encoding_of, expr::LogicOp};
use crabgresql_binder::{BinOp, BoundExpr, UnaryOp};
use crabgresql_batch::kernels;
use crabgresql_types::PgType;
use crabgresql_types::collation::DEFAULT_COLLATION_OID;

use crate::plan::NotVectorizable;

/// Compile `expr` against the batch a scan will produce, or explain why not.
pub fn compile(expr: &BoundExpr, schema: &BatchSchema) -> Result<VectorExpr, NotVectorizable> {
    match expr {
        BoundExpr::Const { value, ty } => {
            admit_type(*ty)?;
            Ok(VectorExpr::Literal {
                value: value.clone(),
                ty: *ty,
            })
        }
        BoundExpr::ColumnRef { index, ty } => {
            admit_type(*ty)?;
            let position = schema
                .position_of(*index)
                .ok_or(NotVectorizable::ColumnNotScanned { index: *index })?;
            Ok(VectorExpr::Column { position, ty: *ty })
        }
        // Value-transparent: a collation changes how a value is *compared*, not
        // what it is, and the comparison node carries its own collation. The
        // binder wraps every collatable column reference in one of these, so
        // refusing them outright would refuse every text column.
        BoundExpr::Collate { expr, .. } => compile(expr, schema),
        BoundExpr::Unary { op, expr, .. } => match op {
            UnaryOp::Not => Ok(VectorExpr::Not(Box::new(compile(expr, schema)?))),
            // `-x` and `@x` overflow at `int_min`; `|/` and `||/` are float
            // operations with PostgreSQL's own range rules. Each needs a kernel
            // that raises the same error, which none of them has yet.
            UnaryOp::Neg | UnaryOp::Abs | UnaryOp::Sqrt | UnaryOp::Cbrt => {
                Err(NotVectorizable::Expression("unary arithmetic"))
            }
        },
        BoundExpr::IsNull { expr, negated } => Ok(VectorExpr::IsNull {
            input: Box::new(compile(expr, schema)?),
            negated: *negated,
        }),
        BoundExpr::Binary {
            op,
            arg_ty,
            collation,
            left,
            right,
            ..
        } => compile_binary(*op, *arg_ty, *collation, left, right, schema),
        // Always the searched form: the binder expands `CASE <operand> WHEN v`
        // into `WHEN <operand> = v` before it gets here, so there is one shape
        // to compile rather than two.
        BoundExpr::Case { whens, else_, ty } => {
            admit_type(*ty)?;
            let whens = whens
                .iter()
                .map(|(when, then)| Ok((compile(when, schema)?, compile(then, schema)?)))
                .collect::<Result<Vec<_>, NotVectorizable>>()?;
            let otherwise = else_
                .as_ref()
                .map(|expr| compile(expr, schema).map(Box::new))
                .transpose()?;
            Ok(VectorExpr::Case {
                whens,
                otherwise,
                ty: *ty,
            })
        }

        // --- refused, one arm each, with the reason ----------------------
        //
        // No `_` arm: adding a `BoundExpr` variant fails this build rather than
        // silently falling into a default that might be wrong.
        BoundExpr::Param { .. } => Err(NotVectorizable::Expression(
            "a parameter that was not substituted before execution",
        )),
        BoundExpr::OuterColumnRef { .. } => Err(NotVectorizable::CorrelatedSubquery),
        // Most runtime casts raise, and reproducing each one's exact error text
        // is a per-cast job rather than a kernel — so they are refused. The
        // integer widenings are the exception: they are total over the source
        // type, so there is no error to reproduce.
        //
        // Worth the special case because it is not a corner: comparing a
        // `smallint` column against an integer literal widens the column, and
        // in a ClickBench-shaped relation about half the columns are `smallint`.
        BoundExpr::Coerce { expr, ty } => {
            let from = expr.ty();
            if !kernels::widens(from, *ty) {
                return Err(NotVectorizable::Expression("a runtime cast"));
            }
            Ok(VectorExpr::Widen {
                input: Box::new(compile(expr, schema)?),
                from,
                to: *ty,
            })
        }
        BoundExpr::Reinterpret { .. } => Err(NotVectorizable::Expression("a runtime cast")),
        BoundExpr::FuncCall { .. } => Err(NotVectorizable::Expression("a scalar function")),
        BoundExpr::Routine { .. } => Err(NotVectorizable::Routine),
        BoundExpr::ArrayCtor { .. } | BoundExpr::Subscript { .. } => {
            Err(NotVectorizable::Expression("an array"))
        }
        BoundExpr::Srf { .. } => Err(NotVectorizable::SetReturning),
        BoundExpr::Aggregate { .. } => Err(NotVectorizable::Expression(
            "an aggregate outside an aggregate node",
        )),
        BoundExpr::ScalarSubquery { .. }
        | BoundExpr::Exists { .. }
        | BoundExpr::QuantifiedSubquery { .. }
        | BoundExpr::QuantifiedArray { .. } => Err(NotVectorizable::CorrelatedSubquery),
    }
}

fn compile_binary(
    op: BinOp,
    arg_ty: PgType,
    collation: u32,
    left: &BoundExpr,
    right: &BoundExpr,
    schema: &BatchSchema,
) -> Result<VectorExpr, NotVectorizable> {
    let l = Box::new(compile(left, schema)?);
    let r = Box::new(compile(right, schema)?);

    if let Some(op) = cmp_op(op) {
        if !kernels::compares(arg_ty) {
            return Err(NotVectorizable::UnsupportedType(arg_ty));
        }
        // Equality is collation-independent: every collation crabgresql
        // supports is deterministic, so equal bytes and equal values coincide.
        // Ordering is not, and reproducing ICU's order in a kernel is not
        // something arrow can do.
        let ordering = !matches!(op, CmpOp::Eq | CmpOp::NotEq);
        let collatable = matches!(arg_ty, PgType::Text | PgType::Varchar | PgType::Name);
        if ordering && collatable && collation != DEFAULT_COLLATION_OID {
            return Err(NotVectorizable::Collation(collation));
        }
        return Ok(VectorExpr::Compare {
            op,
            arg_ty,
            left: l,
            right: r,
        });
    }
    if let Some(op) = logic_op(op) {
        return Ok(VectorExpr::Logic {
            op,
            left: l,
            right: r,
        });
    }
    if let Some(op) = arith_op(op) {
        if !kernels::arithmetic(arg_ty) {
            return Err(NotVectorizable::UnsupportedType(arg_ty));
        }
        return Ok(VectorExpr::Arith {
            op,
            ty: arg_ty,
            left: l,
            right: r,
        });
    }
    Err(NotVectorizable::Operator(op))
}

fn cmp_op(op: BinOp) -> Option<CmpOp> {
    let op = match op {
        BinOp::Eq => CmpOp::Eq,
        BinOp::NotEq => CmpOp::NotEq,
        BinOp::Lt => CmpOp::Lt,
        BinOp::LtEq => CmpOp::LtEq,
        BinOp::Gt => CmpOp::Gt,
        BinOp::GtEq => CmpOp::GtEq,
        _ => return None,
    };
    Some(op)
}

fn logic_op(op: BinOp) -> Option<LogicOp> {
    match op {
        BinOp::And => Some(LogicOp::And),
        BinOp::Or => Some(LogicOp::Or),
        _ => None,
    }
}

fn arith_op(op: BinOp) -> Option<ArithOp> {
    let op = match op {
        BinOp::Add => ArithOp::Add,
        BinOp::Sub => ArithOp::Sub,
        BinOp::Mul => ArithOp::Mul,
        BinOp::Div => ArithOp::Div,
        BinOp::Mod => ArithOp::Mod,
        _ => return None,
    };
    Some(op)
}

/// Whether a value of `ty` can exist in a batch at all.
///
/// A weaker condition than being *computable* — `interval` lives in a batch as a
/// struct but no kernel reads it. Computability is checked where an operator is
/// chosen, so a column can be grouped by, or passed through, without every
/// operator having to support it.
fn admit_type(ty: PgType) -> Result<(), NotVectorizable> {
    encoding_of(ty)
        .map(|_| ())
        .ok_or(NotVectorizable::UnsupportedType(ty))
}
