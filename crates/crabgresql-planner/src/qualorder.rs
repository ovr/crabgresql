//! Qual ordering: within one filter, check the cheap conjuncts before the ones
//! that run a whole plan.
//!
//! `AND` short-circuits left to right, so the *written* order of a `WHERE`
//! decides how many rows each conjunct sees. A conjunct holding a correlated
//! subquery costs a full subplan execution per row it reaches, while an ordinary
//! comparison costs a few nanoseconds — so putting the comparison first is not a
//! micro-optimization, it changes the complexity class of the filter:
//!
//! ```sql
//! -- 10 of 10 000 rows pass `thousand = 1`
//! where thousand = 1 and exists (select 1 from tenk1 k where k.unique1 = t.unique2)
//! where exists (select 1 from tenk1 k where k.unique1 = t.unique2) and thousand = 1
//! ```
//!
//! measured 28.6 ms and 22 443 ms respectively before this pass — the same
//! answer, 785× apart, decided purely by which conjunct the user typed first.
//!
//! PostgreSQL orders quals by estimated cost per unit of selectivity; with no
//! cost model here the pass makes the one distinction that matters by orders of
//! magnitude and leaves everything else alone: **a conjunct containing a
//! *correlated* subquery marker sinks as far right as it legally may, every
//! other conjunct keeps its written order.** Narrower is safer — reordering two
//! ordinary conjuncts could only move which of two errors a row raises, for a
//! gain too small to measure.
//!
//! *Correlated* is the operative word. An uncorrelated marker is folded to a
//! `Const` by `resolve_subqueries` before the scan starts, and evaluating a
//! `Const` is a clone — so sinking one buys nothing while costing whatever it
//! was gating.
//!
//! # Volatility
//!
//! A filter with a volatile conjunct is left entirely alone. Reordering does not
//! just move work, it changes how many rows a conjunct is evaluated on, so
//! hoisting a `nextval()` past an `EXISTS` that used to gate it would advance the
//! sequence a different number of times — an observable difference, not a
//! speed-up. That is the same reason [`pushdown::is_relocatable`] refuses to sink
//! a volatile conjunct.
//!
//! The test has to be the *deep* one. `BoundExpr::contains_volatile_fn` stops at
//! a subquery marker, which is exactly the kind of conjunct this pass moves — so
//! asking it would leave the guarantee above unmet for
//! `(SELECT nextval('s') FROM u WHERE u.k = t.k) > 0`, where the volatility is
//! one level down.
//!
//! # Errors, and why this pass is stricter than PostgreSQL
//!
//! Sinking a conjunct exposes the ones it used to gate to rows they never saw:
//!
//! ```sql
//! where exists (select 1 from u where u.k = t.k and t.divisor <> 0)
//!   and 100 / t.divisor > 1
//! ```
//!
//! With the EXISTS last, the division runs on every row of `t`, including
//! `divisor = 0`, and a query that returned rows raises `22012` instead.
//!
//! PostgreSQL would permit this. Its manual (Expression Evaluation Rules) states
//! that the order of evaluation of subexpressions is not defined and offers
//! `WHERE x > 0 AND y/x > 1.5` as the canonical example of what *not* to rely on;
//! `order_qual_clauses` sorts purely by `(security_level, cost)`. The reason a
//! real PostgreSQL answers the query above is unrelated to qual ordering: it
//! decorrelates the `EXISTS` into a semi join and pulls `t.divisor <> 0` — a qual
//! over the outer relation alone — up into the outer scan's filter, ahead of the
//! division.
//!
//! We have no decorrelation, so that route is closed, and a pass that exists to
//! make queries faster should not change which of them succeed. So a conjunct
//! only sinks past a neighbour that [`cannot_raise`] — a barrier that overrides
//! the cost ordering, structurally what `security_level` is to PostgreSQL, with
//! our own criterion. The motivating case is unaffected: `thousand = 1` is a
//! comparison of a column against a constant.
//!
//! [`pushdown::is_relocatable`]: crate::pushdown::is_relocatable

use crabgresql_binder::{BinOp, BoundExpr, UnaryOp};

use crate::{
    PhysicalAggInput, PhysicalInsertSource, PhysicalJoinExpr, PhysicalJoinInput, PhysicalPlan,
};
use crate::{flatten_and, rebuild_and};

/// Reorder the conjuncts of every filter in `plan`.
///
/// Runs after `pushdown::push_where_into_joins`, so each conjunct is ordered
/// against the others that ended up on the same node rather than against the
/// ones it was written next to.
pub(crate) fn reorder_quals(plan: &mut PhysicalPlan) {
    match plan {
        PhysicalPlan::Values { predicate, .. }
        | PhysicalPlan::Select { predicate, .. }
        | PhysicalPlan::IndexScan { predicate, .. }
        | PhysicalPlan::TableFunction { predicate, .. }
        | PhysicalPlan::Update { predicate, .. }
        | PhysicalPlan::Delete { predicate, .. } => reorder(predicate),
        PhysicalPlan::Subquery {
            source, predicate, ..
        } => {
            reorder(predicate);
            reorder_quals(source);
        }
        PhysicalPlan::Join {
            source, predicate, ..
        } => {
            reorder(predicate);
            reorder_join(source);
        }
        PhysicalPlan::Aggregate {
            input,
            predicate,
            having,
            ..
        } => {
            reorder(predicate);
            reorder(having);
            match input {
                PhysicalAggInput::Join(source) => reorder_join(source),
                PhysicalAggInput::Scan { .. } | PhysicalAggInput::SingleRow => {}
            }
        }
        // An `Append` arm is a bare relation: the `WHERE` over it lives on the
        // `Subquery` this node is always wrapped in, and was handled there.
        PhysicalPlan::Append { .. } => {}
        PhysicalPlan::SetOp { arms, .. } => {
            for arm in arms {
                reorder_quals(&mut arm.plan);
            }
        }
        PhysicalPlan::Window { source, .. } | PhysicalPlan::Limit { source, .. } => {
            reorder_quals(source);
        }
        PhysicalPlan::Insert { source, .. } => {
            if let PhysicalInsertSource::Query { input, .. } = source {
                reorder_quals(input);
            }
        }
    }
}

/// Reorder every filter in a join tree: each node's own `ON` condition, plus the
/// conjuncts `sink_leaf_filters` left on a leaf, plus any nested subplan.
fn reorder_join(node: &mut PhysicalJoinExpr) {
    match node {
        PhysicalJoinExpr::Input {
            input, predicate, ..
        } => {
            reorder(predicate);
            match input {
                PhysicalJoinInput::Subplan(source) => reorder_quals(source),
                PhysicalJoinInput::Scan { .. } | PhysicalJoinInput::TableFunction { .. } => {}
            }
        }
        PhysicalJoinExpr::Join {
            left,
            right,
            predicate,
            ..
        } => {
            reorder(predicate);
            reorder_join(left);
            reorder_join(right);
        }
    }
}

/// Sink each expensive conjunct of one predicate as far right as it may legally
/// go, keeping every other conjunct in written order.
fn reorder(predicate: &mut Option<BoundExpr>) {
    let Some(expr) = predicate.take() else { return };
    let mut conjuncts = Vec::new();
    flatten_and(expr, &mut conjuncts);

    // Nothing to gain from a single conjunct, and a volatile one anywhere
    // forbids any move at all (see the module comment).
    let movable = conjuncts.len() > 1
        && conjuncts.iter().any(is_expensive)
        && !conjuncts
            .iter()
            .any(crabgresql_binder::expr_contains_volatile_fn);
    if movable {
        // A bubble rather than a sort, because the swap is conditional: each
        // adjacent exchange has to be justified on its own, and a conjunct that
        // could raise is a barrier the expensive one does not cross. The list is
        // one `WHERE`'s conjuncts, so the quadratic worst case is nothing.
        let mut swapped = true;
        while swapped {
            swapped = false;
            for i in 0..conjuncts.len() - 1 {
                if is_expensive(&conjuncts[i])
                    && !is_expensive(&conjuncts[i + 1])
                    && cannot_raise(&conjuncts[i + 1])
                {
                    conjuncts.swap(i, i + 1);
                    swapped = true;
                }
            }
        }
    }
    *predicate = rebuild_and(conjuncts);
}

/// Whether evaluating this expression can only produce a value, never an error.
///
/// A whitelist, so anything unrecognized is a barrier: `Coerce` can fail a cast,
/// arithmetic can divide by zero or overflow, a `FuncCall` can do either, a
/// `Subscript` can run past an array's end. Comparisons, the boolean connectives
/// and the null tests cannot — which is exactly the shape of the selective
/// conjunct this pass exists to hoist.
fn cannot_raise(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::Const { .. } | BoundExpr::ColumnRef { .. } | BoundExpr::Param { .. } => true,
        BoundExpr::IsNull { expr, .. } | BoundExpr::BoolTest { expr, .. } => cannot_raise(expr),
        BoundExpr::Unary { op, expr, .. } => matches!(op, UnaryOp::Not) && cannot_raise(expr),
        BoundExpr::Binary {
            op, left, right, ..
        } => {
            matches!(
                op,
                BinOp::Eq
                    | BinOp::NotEq
                    | BinOp::Lt
                    | BinOp::LtEq
                    | BinOp::Gt
                    | BinOp::GtEq
                    | BinOp::And
                    | BinOp::Or
            ) && cannot_raise(left)
                && cannot_raise(right)
        }
        _ => false,
    }
}

/// Whether this conjunct is the kind worth sinking: one holding a *correlated*
/// subquery, which costs a subplan execution for every row it reaches.
///
/// An uncorrelated marker is deliberately not included. `resolve_subqueries`
/// folds one to a `Const` before the scan starts, and evaluating a `Const` is a
/// clone — so sinking it buys nothing and costs whatever it was gating.
fn is_expensive(conjunct: &BoundExpr) -> bool {
    crabgresql_binder::expr_contains_correlated_subquery(conjunct)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::plan_sql;

    /// The conjuncts of a plan-level filter, in evaluation order.
    fn quals(plan: PhysicalPlan) -> Vec<BoundExpr> {
        let predicate = match plan {
            PhysicalPlan::Select { predicate, .. }
            | PhysicalPlan::Join { predicate, .. }
            | PhysicalPlan::Aggregate { predicate, .. } => predicate,
            _ => panic!("expected a filtering plan node"),
        };
        let mut out = Vec::new();
        if let Some(predicate) = predicate {
            flatten_and(predicate, &mut out);
        }
        out
    }

    #[test]
    fn a_subquery_conjunct_sinks_behind_a_plain_one() {
        let quals = quals(plan_sql(
            "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM t c WHERE c.id = t.id) AND t.id = 1",
        ));
        assert_eq!(quals.len(), 2);
        assert!(
            !quals[0].contains_subquery(),
            "the cheap equality is checked first"
        );
        assert!(matches!(quals[1], BoundExpr::Exists { .. }));
    }

    #[test]
    fn plain_conjuncts_keep_their_written_order() {
        // Only the subquery moves; everything else is left exactly as typed, so
        // which of two errors a row raises cannot change.
        let quals = quals(plan_sql(
            "SELECT * FROM t WHERE t.big <> 0 AND EXISTS (SELECT 1 FROM t c WHERE c.id = t.id) \
             AND t.id = 1",
        ));
        assert_eq!(quals.len(), 3);
        assert!(!quals[0].contains_subquery());
        assert!(!quals[1].contains_subquery());
        assert!(matches!(quals[2], BoundExpr::Exists { .. }));
    }

    #[test]
    fn a_conjunct_that_could_raise_is_a_barrier() {
        // The division can raise, so the EXISTS must not sink past it: doing so
        // would run `100 / t.big` on rows the subquery previously excluded, and
        // a query that returned rows would start raising 22012 instead.
        let quals = quals(plan_sql(
            "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM t c WHERE c.id = t.id) \
             AND 100 / t.big > 1 AND t.id = 1",
        ));
        assert_eq!(quals.len(), 3);
        assert!(
            matches!(quals[0], BoundExpr::Exists { .. }),
            "the barrier is the first conjunct after it, so nothing moves"
        );
    }

    #[test]
    fn a_subquery_sinks_up_to_the_barrier_but_no_further() {
        // `t.id = 1` is safe, so the EXISTS swaps past it; the division that
        // follows stops it there.
        let quals = quals(plan_sql(
            "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM t c WHERE c.id = t.id) \
             AND t.id = 1 AND 100 / t.big > 1",
        ));
        assert_eq!(quals.len(), 3);
        assert!(!quals[0].contains_subquery(), "the equality is hoisted");
        assert!(matches!(quals[1], BoundExpr::Exists { .. }));
    }

    #[test]
    fn volatility_inside_a_subquery_body_freezes_the_filter_too() {
        // The volatility lives one level down, where
        // `BoundExpr::contains_volatile_fn` does not look — and it is the very
        // conjunct the pass would move, so asking the shallow predicate would
        // let `nextval` fire on a different number of rows.
        let quals = quals(plan_sql(
            "SELECT * FROM t WHERE (SELECT nextval('s') FROM t c WHERE c.id = t.id) > 0 \
             AND t.id = 1",
        ));
        assert_eq!(quals.len(), 2);
        assert!(quals[0].contains_subquery(), "nothing moved");
    }

    #[test]
    fn an_uncorrelated_subquery_conjunct_does_not_move() {
        // `resolve_subqueries` folds this one to a `Const` before the scan
        // starts, so sinking it would cost whatever it was gating and save
        // nothing. It has to stay exactly where it was written.
        let quals = quals(plan_sql(
            "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM t c WHERE c.id = 1) AND t.id = 1",
        ));
        assert_eq!(quals.len(), 2);
        assert!(matches!(quals[0], BoundExpr::Exists { .. }));
    }

    #[test]
    fn a_volatile_conjunct_freezes_the_whole_filter() {
        // Hoisting `nextval` past the EXISTS that gated it would advance the
        // sequence a different number of times — observable, so nothing moves.
        let quals = quals(plan_sql(
            "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM t c WHERE c.id = t.id) \
             AND nextval('s') > t.big",
        ));
        assert_eq!(quals.len(), 2);
        assert!(matches!(quals[0], BoundExpr::Exists { .. }));
    }
}
