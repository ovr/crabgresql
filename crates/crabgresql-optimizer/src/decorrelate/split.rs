//! Splitting a correlated subplan into "the correlation" and "everything else".
//!
//! The one analysis both rewrites in this module rest on. A correlated subplan
//! is decorrelatable when its whole dependence on the enclosing row sits in
//! AND-ed conjuncts of its filter, at least one of them an equality against a
//! column of that row:
//!
//! ```text
//! select … from b where b.k = <outer.k> and b.s <> <outer.s> and b.x > 3
//!                       ^^^^^^^^^^^^^^^^     ^^^^^^^^^^^^^^^^     ^^^^^^^
//!                       a key                an outer residual    a residual
//! ```
//!
//! Strip the first two and what is left is an ordinary, *uncorrelated* plan —
//! one the enclosing query can join against, with the keys becoming the join
//! condition it hashes on and the outer residual a filter on the match.
//!
//! This is deliberately the same shape analysis the executor's hashed-subplan
//! path performs (`crabgresql_executor::subplan`), one layer earlier: there it
//! yields a hash table built once per statement, here a join the planner gets to
//! cost, index and vectorize. What this rule refuses still reaches that path.

use crabgresql_binder::{
    AggregatePlan, BinOp, BoundExpr, JoinPlan, LogicalPlan, QueryPlan, ValuesPlan,
    plan_contains_volatile_fn, plan_outer_ref_slots,
};
use crabgresql_types::PgType;

/// A subplan with its dependence on the enclosing row lifted out of it.
///
/// All three parts are the binder's own conjuncts, unmodified: the caller
/// rebases the two lifted lists into the joined row's index space
/// (`rebase_into_join`), and the third stays inside the arm, where it already
/// addresses the right row.
///
/// This is a classification, not a promise: a conjunct in either of the first
/// two lists is one that *would have to* move if the caller lifts anything, and
/// whether it can is [`liftable_into_a_join`]'s question, asked by whoever does
/// the lifting. The scalar-aggregate rewrite lifts neither list — it groups the
/// keys' inner sides inside the arm instead — so it is not bound by that answer.
pub(super) struct Split {
    /// `inner-expression = outer-column` conjuncts — the correlation proper, and
    /// what the join can hash on.
    pub keys: Vec<BoundExpr>,
    /// Conjuncts that name the enclosing row without being keys, such as TPC-H
    /// Q21's `l2.l_suppkey <> l1.l_suppkey`.
    ///
    /// Sound for a semi or anti join, whose condition decides membership: "some
    /// inner row matches on the key *and* satisfies this" is the same question
    /// the subquery asked. Not sound for the grouped left join of the
    /// scalar-aggregate rewrite, where grouping happens before the join sees an
    /// outer row at all — that rewrite refuses a non-empty list.
    pub outer_residual: Vec<BoundExpr>,
    /// The subplan minus both lists: no outer reference left anywhere in it.
    pub stripped: LogicalPlan,
}

/// Split `plan` into the conjuncts that name the enclosing row and the
/// uncorrelated remainder, or `None` when it is not a shape this can be sure of.
///
/// Refusals are all of one kind: the fallback is merely slower, while a wrong
/// answer is not recoverable.
pub(super) fn split_correlation(plan: &LogicalPlan) -> Option<Split> {
    // Running the body once instead of once per outer row is only invisible if
    // it has no side effects to count — a sequence would advance a different
    // number of times, a routine's writes would happen a different number of
    // times.
    if plan_contains_volatile_fn(plan) {
        return None;
    }
    let predicate = decorrelatable_predicate(plan)?;

    let mut conjuncts = Vec::new();
    if let Some(predicate) = predicate {
        flatten_and(predicate, &mut conjuncts);
    }

    let mut keys = Vec::new();
    let mut outer_residual = Vec::new();
    let mut residual = Vec::new();
    for conjunct in conjuncts {
        if is_correlation_key(conjunct) {
            keys.push(conjunct.clone());
        } else if names_an_outer_row(conjunct) {
            outer_residual.push(conjunct.clone());
        } else {
            residual.push(conjunct.clone());
        }
    }

    let stripped = with_predicate(plan.clone(), rebuild_and(residual))?;
    // Everything that named the enclosing row had to be lifted. A leftover
    // reference — under a projection, inside a nested subquery, in a join's own
    // `ON` — means the remainder is still a function of the outer row, and
    // joining against it would read whatever row happened to be current.
    //
    // `plan_outer_ref_slots` answers both halves of that: `None` for a plan
    // reaching *past* the enclosing row, and the slots of the enclosing row it
    // still reads otherwise.
    if plan_outer_ref_slots(&stripped)?.is_empty() {
        Some(Split {
            keys,
            outer_residual,
            stripped,
        })
    } else {
        None
    }
}

/// Whether a conjunct can be evaluated by the join node instead of inside the
/// subplan — asked of **everything** that moves into the condition, a key as
/// much as a residual.
///
/// Two things have to hold. Every reference it makes must be to the immediately
/// enclosing row — a deeper one belongs to a query further out, which this join
/// is not, and `plan_outer_ref_slots` reports that as `None`. And it must hold no
/// subquery of its own: that body addresses its own level, and rebasing this
/// level's column indices cannot reach inside it to leave the two consistent.
/// The body's level-1 references would go on meaning "one level out" while the
/// level they counted from moved away, so they would read the enclosing query's
/// row — and the columns they need would not be projected by the arm, which
/// collects them with the same walk that stops at the body.
pub(super) fn liftable_into_a_join(conjunct: &BoundExpr) -> bool {
    !conjunct.contains_subquery()
        && !conjunct.contains_volatile_fn()
        && plan_outer_ref_slots(&as_plan(conjunct)).is_some()
}

/// The `WHERE` of a node whose predicate can be split — `Some(None)` for an
/// accepted shape carrying no filter at all (an uncorrelated `IN (SELECT …)` is
/// the case that reaches it), and `None` for a shape this analysis does not
/// model.
///
/// The tail clauses have to be absent: a `LIMIT`, `ORDER BY` or `DISTINCT`
/// applies to the subquery's own result, and the correlation conjunct that would
/// be lifted out of the filter below it is not a filter of that result.
/// `LogicalPlan::Aggregate` is included for the scalar-aggregate rewrite, whose
/// `HAVING` is checked separately by its own shape test.
fn decorrelatable_predicate(plan: &LogicalPlan) -> Option<Option<&BoundExpr>> {
    match plan {
        LogicalPlan::Query(QueryPlan {
            predicate,
            sort,
            distinct,
            ..
        })
        | LogicalPlan::Join(JoinPlan {
            predicate,
            sort,
            distinct,
            ..
        })
        | LogicalPlan::Aggregate(AggregatePlan {
            predicate,
            sort,
            distinct,
            ..
        }) if sort.is_empty() && distinct.is_none() => Some(predicate.as_ref()),
        _ => None,
    }
}

/// `plan` with its predicate replaced — the same three shapes
/// [`decorrelatable_predicate`] accepts.
fn with_predicate(mut plan: LogicalPlan, replacement: Option<BoundExpr>) -> Option<LogicalPlan> {
    match &mut plan {
        LogicalPlan::Query(QueryPlan { predicate, .. })
        | LogicalPlan::Join(JoinPlan { predicate, .. })
        | LogicalPlan::Aggregate(AggregatePlan { predicate, .. }) => *predicate = replacement,
        _ => return None,
    }
    Some(plan)
}

/// Whether `conjunct` is the `inner-expression = outer-column` shape a join can
/// hash on — [`key_sides`] states what that takes.
///
/// A conjunct this rejects is not thereby refused: if it names the enclosing row
/// it can still ride into the join condition as an ordinary filter (see
/// [`Split::outer_residual`]), it just cannot be a hash key.
fn is_correlation_key(conjunct: &BoundExpr) -> bool {
    key_sides(conjunct).is_some()
}

/// The two sides of a correlation key, inner first, with the comparison's type
/// and collation — for the scalar-aggregate rewrite, which groups the arm by the
/// inner side and joins on the outer one.
///
/// The outer side must be a reference to the immediately enclosing row rather
/// than an expression over one, so what it becomes is a column of the left
/// input. The inner side must be evaluable against the inner row alone, and both
/// must already be *of* the comparison's type — see [`operands_match_arg_ty`].
pub(super) fn key_sides(conjunct: &BoundExpr) -> Option<(&BoundExpr, &BoundExpr, PgType, u32)> {
    let BoundExpr::Binary {
        op: BinOp::Eq,
        arg_ty,
        collation,
        left,
        right,
    } = conjunct
    else {
        return None;
    };
    let (inner, outer) = match (is_outer_ref(left), is_outer_ref(right)) {
        (false, true) => (left.as_ref(), right.as_ref()),
        (true, false) => (right.as_ref(), left.as_ref()),
        // Both sides outer, or neither: not a correlation key either way.
        _ => return None,
    };
    if names_an_outer_row(inner) || !operands_match_arg_ty(*arg_ty, inner, outer) {
        return None;
    }
    Some((inner, outer, *arg_ty, *collation))
}

/// Whether both operands of a comparison already evaluate to the type the
/// comparison declares.
///
/// A join key is hashed as its declared `arg_ty` — `hash_key(&[arg_ty], …)`
/// reads the value as that type and reaches an `unreachable!` if it is not one.
/// The binder does not always leave the two in step: `x IN (SELECT n)` over
/// float8 and numeric arrives as a float8 comparison whose candidate side is
/// still numeric, because the per-row path casts each candidate as it goes.
/// Nothing here can insert the missing cast — that is a binder decision about
/// *which* comparison to perform — so a comparison whose operands have not
/// converged is left where it is.
pub(super) fn operands_match_arg_ty(arg_ty: PgType, left: &BoundExpr, right: &BoundExpr) -> bool {
    left.ty() == arg_ty && right.ty() == arg_ty
}

/// Whether this operand *is* the enclosing row's column — a level-1
/// `OuterColumnRef`, bare or under the coercion the binder resolved.
///
/// The coercion has to be allowed for: `unify_types` wraps whichever operand is
/// the narrower one, so `b.big_key = a.small_key` puts a `Coerce` around the
/// outer reference, and refusing it would drop half of all cross-type
/// correlations. Level 1 is the immediately enclosing query; a deeper level
/// belongs to a query further out and is not this join's to answer.
fn is_outer_ref(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::OuterColumnRef { level: 1, .. } => true,
        BoundExpr::Coerce { expr, .. } => {
            matches!(expr.as_ref(), BoundExpr::OuterColumnRef { level: 1, .. })
        }
        _ => false,
    }
}

/// Whether `expr` names an enclosing row anywhere in it — directly, or from
/// inside a subquery marker it carries.
///
/// [`plan_has_outer_refs`] answers this for a plan; wrapping the expression in
/// the smallest plan that can hold one reuses the binder's depth arithmetic
/// rather than duplicating it here, which is what makes the answer right for a
/// reference buried under a marker (the executor's `subplan` module borrows the
/// same trick).
///
/// [`plan_has_outer_refs`]: crabgresql_binder::plan_has_outer_refs
pub(super) fn names_an_outer_row(expr: &BoundExpr) -> bool {
    crabgresql_binder::plan_has_outer_refs(&as_plan(expr))
}

/// The smallest plan that holds one expression, so the binder's plan-level walks
/// — which are the ones that get the depth arithmetic right — can be asked about
/// it.
fn as_plan(expr: &BoundExpr) -> LogicalPlan {
    LogicalPlan::Values(ValuesPlan {
        columns: Vec::new(),
        rows: vec![vec![expr.clone()]],
        predicate: None,
        sort: Vec::new(),
        distinct: None,
    })
}

/// Split a top-level `AND` tree into its conjuncts, by reference.
pub(super) fn flatten_and<'a>(expr: &'a BoundExpr, out: &mut Vec<&'a BoundExpr>) {
    match expr {
        BoundExpr::Binary {
            op: BinOp::And,
            left,
            right,
            ..
        } => {
            flatten_and(left, out);
            flatten_and(right, out);
        }
        other => out.push(other),
    }
}

/// Re-combine conjuncts with `AND`, yielding `None` for an empty list.
pub(super) fn rebuild_and(mut conjuncts: Vec<BoundExpr>) -> Option<BoundExpr> {
    let mut acc = conjuncts.pop()?;
    while let Some(next) = conjuncts.pop() {
        acc = and(next, acc);
    }
    Some(acc)
}

pub(super) fn and(left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr::Binary {
        op: BinOp::And,
        arg_ty: PgType::Bool,
        collation: crabgresql_types::collation::DEFAULT_COLLATION_OID,
        left: Box::new(left),
        right: Box::new(right),
    }
}
