//! The `SimplifyExpressions` rule: constant folding plus the boolean
//! identities, applied to every expression of a plan.
//!
//! Two rewriters: a [`const_evaluator`] that turns a constant subtree into its
//! value and a [`simplifier`] that rewrites shapes. They share one post-order
//! walk of each expression, so a simplification immediately sees the constants
//! folding just produced.

use crabgresql_binder::{BoundExpr, ExprVisitor, LogicalPlan, Subplan, walk_exprs_mut};
use crabgresql_types::Value;

use crate::{OptimizerContext, OptimizerRule};

mod const_evaluator;
mod simplifier;

/// Fold constant expressions and simplify boolean ones.
///
/// Beyond saving per-row work, this is what turns an index key into something
/// the cost model can read: only a literal reaches the planner's `const_of`, so
/// `WHERE id = 2 + 3` is costed by a generic guess until the arithmetic is
/// folded away.
pub struct SimplifyExpressions;

impl OptimizerRule for SimplifyExpressions {
    fn name(&self) -> &'static str {
        "simplify_expressions"
    }

    fn rewrite(&self, plan: &mut LogicalPlan, ctx: &OptimizerContext) -> bool {
        let mut rewriter = Rewriter {
            ctx,
            changed: false,
        };
        walk_exprs_mut(plan, &mut rewriter);
        rewriter.changed
    }
}

struct Rewriter<'a> {
    ctx: &'a OptimizerContext,
    changed: bool,
}

impl ExprVisitor for Rewriter<'_> {
    fn expr(&mut self, expr: &mut BoundExpr) {
        let ctx = self.ctx;
        // A subquery marker's body is a plan of its own, which the plan walk
        // this visitor rides on deliberately stops at. Descending here is the
        // only rewrite that body ever gets: the executor plans a subplan
        // without an optimizer pass, precisely because this one already ran.
        let mut on_subplan =
            |subplan: &mut Subplan| SimplifyExpressions.rewrite(&mut subplan.plan, ctx);
        self.changed |= const_evaluator::fold(expr, &ctx.fmt, &mut on_subplan);
    }

    fn predicate(&mut self, predicate: &mut Option<BoundExpr>) {
        let Some(expr) = predicate else {
            return;
        };
        self.expr(expr);
        // A qual that is constantly true filters nothing. Dropping it is worth
        // a special case because it removes the executor's per-row call
        // altogether, rather than making it cheap.
        //
        // A constant `FALSE`/NULL stays: it still answers correctly, one row at
        // a time. TODO: answer it without touching the heap, which needs a
        // logical plan node for "no rows".
        if matches!(
            expr,
            BoundExpr::Const {
                value: Value::Bool(true),
                ..
            }
        ) {
            *predicate = None;
            self.changed = true;
        }
    }
}

#[cfg(test)]
mod tests;
