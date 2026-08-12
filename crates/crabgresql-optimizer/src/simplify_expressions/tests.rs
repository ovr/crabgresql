//! Expressions are built by hand here rather than bound from SQL: the binder
//! needs a catalog and a table handle, and every property under test is a
//! property of the rewrite, not of the binding.

use crabgresql_binder::{
    BinOp, BoundExpr, LogicalPlan, OutputColumn, ScalarFn, Strength, Subplan, UnaryOp, ValuesPlan,
};
use crabgresql_types::{FmtCtx, PgType, Value};

use super::SimplifyExpressions;
use crate::{OptimizerContext, OptimizerRule, optimize};

fn ctx() -> OptimizerContext {
    OptimizerContext {
        fmt: FmtCtx::utc_default(),
    }
}

/// Fold and simplify one expression, the way the rule does inside a plan.
fn rewrite(mut expr: BoundExpr) -> BoundExpr {
    let mut on_subplan = |_: &mut Subplan| false;
    super::const_evaluator::fold(&mut expr, &ctx().fmt, &mut on_subplan);
    expr
}

fn int4(v: i32) -> BoundExpr {
    BoundExpr::Const {
        value: Value::Int4(v),
        ty: PgType::Int4,
    }
}

fn null_int4() -> BoundExpr {
    BoundExpr::Const {
        value: Value::Null,
        ty: PgType::Int4,
    }
}

fn boolean(v: bool) -> BoundExpr {
    BoundExpr::Const {
        value: Value::Bool(v),
        ty: PgType::Bool,
    }
}

fn col(index: usize) -> BoundExpr {
    BoundExpr::ColumnRef {
        index,
        ty: PgType::Int4,
    }
}

fn binary(op: BinOp, arg_ty: PgType, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr::Binary {
        op,
        arg_ty,
        collation: 0,
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn arith(op: BinOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    binary(op, PgType::Int4, left, right)
}

fn cmp(op: BinOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    binary(op, PgType::Int4, left, right)
}

fn logic(op: BinOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    binary(op, PgType::Bool, left, right)
}

/// A one-row `SELECT` carrying `predicate`, the smallest plan with a qual.
fn values_with(predicate: BoundExpr) -> LogicalPlan {
    LogicalPlan::Values(ValuesPlan {
        columns: vec![OutputColumn {
            name: "a".into(),
            ty: PgType::Int4,
            collation: None,
            strength: Strength::None,
            typmod: -1,
            generated: None,
        }],
        rows: vec![vec![int4(1)]],
        predicate: Some(predicate),
        sort: Vec::new(),
        distinct: None,
    })
}

/// A one-row `SELECT` projecting `expr` and nothing else.
fn values_projecting(expr: BoundExpr) -> LogicalPlan {
    let LogicalPlan::Values(mut values) = values_with(boolean(true)) else {
        unreachable!("values_with builds a Values plan");
    };
    values.rows = vec![vec![expr]];
    values.predicate = None;
    LogicalPlan::Values(values)
}

fn predicate_of(plan: &LogicalPlan) -> Option<&BoundExpr> {
    match plan {
        LogicalPlan::Values(values) => values.predicate.as_ref(),
        _ => unreachable!("test builds a Values plan"),
    }
}

fn projection_of(plan: &LogicalPlan) -> &BoundExpr {
    match plan {
        LogicalPlan::Values(values) => &values.rows[0][0],
        _ => unreachable!("test builds a Values plan"),
    }
}

#[test]
fn arithmetic_over_constants_folds() {
    let folded = rewrite(cmp(BinOp::Gt, col(0), arith(BinOp::Add, int4(10), int4(5))));
    assert_eq!(folded, cmp(BinOp::Gt, col(0), int4(15)));
}

#[test]
fn nested_arithmetic_folds_in_one_pass() {
    // Bottom-up, so `(2 + 3) * 4` needs no second pass.
    let folded = rewrite(arith(
        BinOp::Mul,
        arith(BinOp::Add, int4(2), int4(3)),
        int4(4),
    ));
    assert_eq!(folded, int4(20));
}

#[test]
fn a_comparison_of_constants_folds_to_a_boolean() {
    assert_eq!(rewrite(cmp(BinOp::Lt, int4(1), int4(2))), boolean(true));
}

#[test]
fn unary_negation_folds() {
    let folded = rewrite(BoundExpr::Unary {
        op: UnaryOp::Neg,
        expr: Box::new(int4(7)),
    });
    assert_eq!(folded, int4(-7));
}

#[test]
fn a_cast_of_a_constant_folds() {
    let folded = rewrite(BoundExpr::Coerce {
        expr: Box::new(int4(42)),
        ty: PgType::Int8,
    });
    assert_eq!(
        folded,
        BoundExpr::Const {
            value: Value::Int8(42),
            ty: PgType::Int8,
        }
    );
}

#[test]
fn the_folded_constant_carries_the_nodes_declared_type() {
    // The value alone cannot say what type a NULL is; the node's `ty()` can.
    let folded = rewrite(arith(BinOp::Add, int4(1), null_int4()));
    assert_eq!(folded, null_int4());
}

#[test]
fn a_column_reference_stops_folding() {
    let expr = arith(BinOp::Add, col(0), int4(5));
    assert_eq!(rewrite(expr.clone()), expr);
}

#[test]
fn a_parameter_stops_folding() {
    // `$1` is a per-execution value. It is substituted with a `Const` before
    // planning, but an unsubstituted one must never be treated as constant.
    let expr = arith(
        BinOp::Add,
        BoundExpr::Param {
            index: 0,
            ty: PgType::Int4,
        },
        int4(5),
    );
    assert_eq!(rewrite(expr.clone()), expr);
}

#[test]
fn a_function_call_is_left_alone() {
    // Folding a call needs `eval_scalar`, which lives above this crate.
    let expr = BoundExpr::FuncCall {
        func: ScalarFn::Upper,
        ret: PgType::Text,
        args: vec![BoundExpr::Const {
            value: Value::Text("abc".into()),
            ty: PgType::Text,
        }],
    };
    assert_eq!(rewrite(expr.clone()), expr);
}

#[test]
fn an_expression_that_would_error_is_left_for_execution() {
    // `1/0` must not raise here: `SELECT 1/0 FROM t` over an empty `t` raises
    // nothing today, and folding may not change that.
    let expr = arith(BinOp::Div, int4(1), int4(0));
    assert_eq!(rewrite(expr.clone()), expr);
}

#[test]
fn an_unfoldable_operand_keeps_its_parent_lazy() {
    // Because `1/0` stays an expression, `false AND 1/0 = 1` is not constant
    // either — so nothing evaluates the division, at plan time or at run time.
    let division = cmp(BinOp::Eq, arith(BinOp::Div, int4(1), int4(0)), int4(1));
    let folded = rewrite(logic(BinOp::And, boolean(false), division));
    assert_eq!(folded, boolean(false), "the AND is decided by its left arm");
}

#[test]
fn case_folds_through_the_arm_it_reaches() {
    // Lazy, as at run time: the unreached `1/0` arm is never evaluated.
    let folded = rewrite(BoundExpr::Case {
        whens: vec![(boolean(false), arith(BinOp::Div, int4(1), int4(0)))],
        else_: Some(Box::new(int4(1))),
        ty: PgType::Int4,
    });
    assert_eq!(folded, int4(1));
}

#[test]
fn coalesce_folds_to_its_first_non_null() {
    let folded = rewrite(BoundExpr::Coalesce {
        args: vec![null_int4(), int4(3), int4(4)],
        ty: PgType::Int4,
    });
    assert_eq!(folded, int4(3));
}

#[test]
fn is_null_folds() {
    let folded = rewrite(BoundExpr::IsNull {
        expr: Box::new(null_int4()),
        negated: false,
    });
    assert_eq!(folded, boolean(true));
}

#[test]
fn the_boolean_identities_drop_a_constant_arm() {
    let x = cmp(BinOp::Gt, col(0), int4(1));
    // TRUE AND x → x, and symmetrically.
    assert_eq!(rewrite(logic(BinOp::And, boolean(true), x.clone())), x);
    assert_eq!(rewrite(logic(BinOp::And, x.clone(), boolean(true))), x);
    // FALSE OR x → x.
    assert_eq!(rewrite(logic(BinOp::Or, boolean(false), x.clone())), x);
    assert_eq!(rewrite(logic(BinOp::Or, x.clone(), boolean(false))), x);
}

#[test]
fn a_decisive_constant_collapses_the_whole_boolean() {
    let x = cmp(BinOp::Gt, col(0), int4(1));
    assert_eq!(
        rewrite(logic(BinOp::And, x.clone(), boolean(false))),
        boolean(false)
    );
    assert_eq!(rewrite(logic(BinOp::Or, x, boolean(true))), boolean(true));
}

#[test]
fn a_null_arm_is_not_decisive() {
    // `x AND NULL` is NULL when `x` is true and false when it is false: it
    // cannot be simplified away without knowing `x`.
    let expr = logic(
        BinOp::And,
        cmp(BinOp::Gt, col(0), int4(1)),
        BoundExpr::Const {
            value: Value::Null,
            ty: PgType::Bool,
        },
    );
    assert_eq!(rewrite(expr.clone()), expr);
}

#[test]
fn an_any_all_template_keeps_its_shape() {
    // `cmp` is `needle op <hole>`, and the executor substitutes each candidate
    // into the hole. Folding it as an ordinary expression collapses it to
    // `NULL` and the statement dies with "ANY/ALL comparison template was not a
    // binary comparison" — so only the needle folds.
    let template = cmp(BinOp::Eq, arith(BinOp::Add, int4(2), int4(3)), null_int4());
    let folded = rewrite(BoundExpr::QuantifiedArray {
        array: Box::new(BoundExpr::ArrayCtor {
            elem: PgType::Int4,
            ty: PgType::Array(PgType::Int4.oid()),
            elems: vec![int4(1), int4(5)],
        }),
        all: false,
        cmp: Box::new(template),
    });
    let BoundExpr::QuantifiedArray { cmp: folded, .. } = folded else {
        panic!("the quantified node itself must survive");
    };
    assert_eq!(*folded, cmp(BinOp::Eq, int4(5), null_int4()));
}

#[test]
fn a_predicate_that_folds_to_true_is_removed() {
    let mut plan = values_with(cmp(BinOp::Eq, int4(1), int4(1)));
    optimize(&mut plan, &ctx());
    assert!(
        predicate_of(&plan).is_none(),
        "a constantly-true qual filters nothing and should not survive"
    );
}

#[test]
fn a_predicate_that_folds_to_false_survives() {
    let mut plan = values_with(cmp(BinOp::Eq, int4(1), int4(2)));
    optimize(&mut plan, &ctx());
    assert_eq!(predicate_of(&plan), Some(&boolean(false)));
}

#[test]
fn a_conjunct_that_folds_to_true_is_dropped_from_the_qual() {
    let live = cmp(BinOp::Gt, col(0), int4(1));
    let mut plan = values_with(logic(
        BinOp::And,
        cmp(BinOp::Eq, int4(1), int4(1)),
        live.clone(),
    ));
    optimize(&mut plan, &ctx());
    assert_eq!(predicate_of(&plan), Some(&live));
}

/// The executor plans a subplan without an optimizer pass, on the strength of
/// this descent having already rewritten its body. If the descent goes, a
/// subquery's body is never simplified at all — and nothing else would say so.
#[test]
fn a_subquery_body_is_simplified_with_its_enclosing_statement() {
    let mut plan = values_projecting(BoundExpr::ScalarSubquery {
        subplan: Subplan::new(values_with(cmp(BinOp::Eq, int4(1), int4(1)))),
        ty: PgType::Int4,
    });
    optimize(&mut plan, &ctx());
    let BoundExpr::ScalarSubquery { subplan, .. } = projection_of(&plan) else {
        panic!("the marker itself must survive: only the executor may fold it");
    };
    assert!(predicate_of(&subplan.plan).is_none());
}

#[test]
fn optimizing_twice_changes_nothing_more() {
    // The fixpoint the pass loop relies on: a second run reports no change.
    let mut plan = values_with(logic(
        BinOp::And,
        cmp(BinOp::Gt, col(0), arith(BinOp::Add, int4(10), int4(5))),
        cmp(BinOp::Eq, int4(1), int4(1)),
    ));
    optimize(&mut plan, &ctx());
    let once = format!("{:?}", predicate_of(&plan));
    assert!(!SimplifyExpressions.rewrite(&mut plan, &ctx()));
    assert_eq!(format!("{:?}", predicate_of(&plan)), once);
}
