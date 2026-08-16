//! The one traversal that reaches every expression a [`LogicalPlan`] holds.
//!
//! Two passes need it — [`crate::substitute_params`], which replaces `$n`
//! leaves before a portal executes, and the logical optimizer's constant
//! folding — and a plan node that one pass visits while the other forgets it is
//! a silent bug in whichever forgot. So the node walk lives here once, and a
//! pass supplies only what to do with an expression.
//!
//! Scope: this walks the plan *tree* — a node's child plans (`source`, a set-op
//! arm, a join input) are visited too. It deliberately does **not** descend into
//! the subplans embedded in expressions (`BoundExpr::ScalarSubquery`,
//! `Exists`, `QuantifiedSubquery`): those are a separate query level, and
//! whether to enter one is the visitor's decision — it can call
//! [`walk_exprs_mut`] on the subplan itself from inside [`ExprVisitor::expr`].

use crate::expr::BoundExpr;

use super::{
    AggInput, AggregatePlan, AppendPlan, DeletePlan, InsertPlan, InsertSource, JoinExpr, JoinInput,
    JoinPlan, LimitPlan, LogicalPlan, QueryPlan, Returning, SetOpPlan, SubqueryPlan,
    TableFunctionPlan, UpdatePlan, ValuesPlan, WindowPlan,
};

/// What to do with each expression a plan holds.
pub trait ExprVisitor {
    /// Called once per expression tree, in place.
    fn expr(&mut self, expr: &mut BoundExpr);

    /// Called for a node's `WHERE`/`ON` slot instead of [`Self::expr`]. The
    /// default just visits the expression; an optimizer overrides it to be able
    /// to remove the predicate entirely (a qual that folds to `TRUE` filters
    /// nothing, and PostgreSQL drops it rather than evaluating it per row).
    fn predicate(&mut self, predicate: &mut Option<BoundExpr>) {
        if let Some(expr) = predicate {
            self.expr(expr);
        }
    }
}

/// Visit every expression of `plan` and of the plans below it.
pub fn walk_exprs_mut(plan: &mut LogicalPlan, v: &mut dyn ExprVisitor) {
    match plan {
        LogicalPlan::Values(ValuesPlan {
            rows, predicate, ..
        }) => {
            for row in rows {
                walk_all(row, v);
            }
            v.predicate(predicate);
        }
        LogicalPlan::Query(QueryPlan {
            projections,
            predicate,
            ..
        }) => {
            walk_all(projections, v);
            v.predicate(predicate);
        }
        LogicalPlan::Subquery(SubqueryPlan {
            source,
            projections,
            predicate,
            ..
        }) => {
            walk_exprs_mut(source, v);
            walk_all(projections, v);
            v.predicate(predicate);
        }
        LogicalPlan::Window(WindowPlan {
            source,
            spec,
            funcs,
            ..
        }) => {
            walk_exprs_mut(source, v);
            for expr in spec.exprs_mut() {
                v.expr(expr);
            }
            for func in funcs {
                walk_all(func.kind.args_mut(), v);
            }
        }
        LogicalPlan::TableFunction(TableFunctionPlan {
            args,
            projections,
            predicate,
            ..
        }) => {
            walk_all(args, v);
            walk_all(projections, v);
            v.predicate(predicate);
        }
        LogicalPlan::Join(JoinPlan {
            source,
            projections,
            predicate,
            ..
        }) => {
            walk_join(source, v);
            walk_all(projections, v);
            v.predicate(predicate);
        }
        // An Append carries only leaf table handles, no expressions.
        LogicalPlan::Append(AppendPlan { .. }) => {}
        LogicalPlan::SetOp(SetOpPlan { arms, .. }) => {
            for arm in arms.iter_mut() {
                walk_exprs_mut(&mut arm.plan, v);
                if let Some(coercion) = &mut arm.coercion {
                    walk_all(coercion, v);
                }
            }
        }
        LogicalPlan::Limit(LimitPlan { source, .. }) => walk_exprs_mut(source, v),
        LogicalPlan::Aggregate(AggregatePlan {
            input,
            predicate,
            group_exprs,
            aggregates,
            having,
            projections,
            ..
        }) => {
            if let AggInput::Join(join) = input {
                walk_join(join, v);
            }
            v.predicate(predicate);
            walk_all(group_exprs, v);
            for agg in aggregates {
                walk_all(&mut agg.args, v);
            }
            // HAVING is a qual over the grouped row, but removing it is not the
            // same rewrite as removing a scan's WHERE (the aggregate node owns
            // the slot), so it goes through `predicate` like any other filter
            // and the visitor decides.
            v.predicate(having);
            walk_all(projections, v);
        }
        LogicalPlan::Insert(InsertPlan {
            source, returning, ..
        }) => {
            match source {
                InsertSource::Values(rows) => {
                    for row in rows {
                        walk_all(row, v);
                    }
                }
                // The rows hold no expressions at all; only a deferred column
                // default does.
                InsertSource::Tuples { defaults, .. } => {
                    for (_, default) in defaults.iter_mut() {
                        v.expr(default);
                    }
                }
                InsertSource::Query { input, projections } => {
                    walk_exprs_mut(input, v);
                    walk_all(projections, v);
                }
            }
            walk_returning(returning, v);
        }
        LogicalPlan::Update(UpdatePlan {
            predicate,
            assignments,
            returning,
            ..
        }) => {
            v.predicate(predicate);
            for (_, expr) in assignments {
                v.expr(expr);
            }
            walk_returning(returning, v);
        }
        LogicalPlan::Delete(DeletePlan {
            predicate,
            returning,
            ..
        }) => {
            v.predicate(predicate);
            walk_returning(returning, v);
        }
    }
}

fn walk_returning(returning: &mut Option<Returning>, v: &mut dyn ExprVisitor) {
    if let Some(returning) = returning {
        walk_all(&mut returning.projections, v);
    }
}

fn walk_join(join: &mut JoinExpr, v: &mut dyn ExprVisitor) {
    match join {
        JoinExpr::Input { input, .. } => match input {
            JoinInput::Scan { .. } => {}
            JoinInput::Subplan(plan) => walk_exprs_mut(plan, v),
            JoinInput::TableFunction { args, .. } => walk_all(args, v),
        },
        JoinExpr::Join {
            left,
            right,
            predicate,
            ..
        } => {
            walk_join(left, v);
            walk_join(right, v);
            v.predicate(predicate);
        }
    }
}

fn walk_all(exprs: &mut [BoundExpr], v: &mut dyn ExprVisitor) {
    for expr in exprs {
        v.expr(expr);
    }
}
