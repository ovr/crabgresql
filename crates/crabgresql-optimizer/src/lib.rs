//! The logical optimizer: rewrites a bound [`LogicalPlan`] into an equivalent
//! one that is cheaper to execute, before the planner turns it into a physical
//! plan.
//!
//! A list of [`OptimizerRule`]s applied to the plan tree over repeated passes
//! until it stops changing. Rules see only the logical plan — nothing here
//! knows about access paths, join algorithms or execution nodes, which is what
//! keeps the crate below the planner and the executor in the dependency graph.
//!
//! Today the list holds one rule, [`SimplifyExpressions`]: constant folding plus
//! the boolean simplifications it opens up. The payoff is more than a per-row
//! addition saved. A folded key is a *literal*, and the planner reads a literal
//! where it cannot read an expression: `key_selectivity` consults the column's
//! distribution only through `const_of`, so `WHERE id = 2 + 3` is costed by
//! PostgreSQL's generic `var_eq_non_const` guess until the arithmetic is folded
//! away. A qual that folds to `TRUE` disappears from the scan entirely.

use crabgresql_binder::LogicalPlan;
use crabgresql_types::FmtCtx;

mod simplify_expressions;

pub use simplify_expressions::SimplifyExpressions;

/// What a rule may read about the session it is optimizing for.
///
/// Only the formatting/parsing context so far, which is what a cast between a
/// string and a date/time type needs (`TimeZone`, `DateStyle`,
/// `extra_float_digits`). Optimization is redone for every execution of a
/// statement, never cached across them, so folding against this session's
/// settings cannot leak into another session's answer.
pub struct OptimizerContext {
    pub fmt: FmtCtx,
}

/// One logical rewrite.
pub trait OptimizerRule {
    /// For diagnostics and tests.
    fn name(&self) -> &'static str;

    /// Rewrite `plan` in place; `true` if anything changed, which is what drives
    /// the fixpoint loop in [`Optimizer::optimize`].
    fn rewrite(&self, plan: &mut LogicalPlan, ctx: &OptimizerContext) -> bool;
}

/// How many times the rule list is re-applied while the plan keeps changing.
/// One rule can open work for another (folding `1 = 1` to `TRUE` lets the AND
/// simplification drop it, which can leave the parent constant in turn), and a
/// cap is what keeps a rule that oscillates from hanging the session.
const MAX_PASSES: usize = 3;

/// The rule list, applied in order.
pub struct Optimizer {
    rules: Vec<Box<dyn OptimizerRule>>,
}

impl Default for Optimizer {
    fn default() -> Self {
        Optimizer {
            rules: vec![Box::new(SimplifyExpressions)],
        }
    }
}

impl Optimizer {
    /// The default rule list.
    pub fn new() -> Optimizer {
        Optimizer::default()
    }

    /// An optimizer running exactly `rules` — for tests, and for a caller that
    /// wants one rewrite without the rest.
    pub fn with_rules(rules: Vec<Box<dyn OptimizerRule>>) -> Optimizer {
        Optimizer { rules }
    }

    pub fn optimize(&self, plan: &mut LogicalPlan, ctx: &OptimizerContext) {
        for _ in 0..MAX_PASSES {
            let mut changed = false;
            for rule in &self.rules {
                changed |= rule.rewrite(plan, ctx);
            }
            if !changed {
                return;
            }
        }
    }
}

/// Run the default rule list over `plan`. The entry point every caller that
/// plans a statement uses.
pub fn optimize(plan: &mut LogicalPlan, ctx: &OptimizerContext) {
    Optimizer::new().optimize(plan, ctx);
}
