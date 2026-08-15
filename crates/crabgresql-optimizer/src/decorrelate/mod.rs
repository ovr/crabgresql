//! The `DecorrelateSubqueries` rule: turn a correlated subquery into a join.
//!
//! A correlated subquery is otherwise a nested loop with a *plan* in it. The
//! executor clones the subplan, substitutes the outer row's values into it,
//! plans it and runs it — once per outer row (`crabgresql_executor`'s
//! `eval_correlated_subquery`). Two mitigations live there already: a hashed
//! `EXISTS` built once per statement, and a memo keyed on the slots the subplan
//! reads. Both leave the subquery invisible to the planner, which is the real
//! cost: no index is considered for it, no qual is pushed into it, and the
//! vectorized hash join never sees it.
//!
//! This rule removes the nesting instead. Two rewrites, one shape analysis
//! ([`split`]) and one splice ([`attach`]) between them:
//!
//! ```text
//! …  where exists (select 1 from b where b.k = a.k and b.x > 3)
//! →  a SEMI JOIN (select b.k from b where b.x > 3) on a.k = k        -- ①
//!
//! …  where exists (select 1 from b where b.k = a.k and b.s <> a.s)
//! →  a SEMI JOIN (select b.k, b.s from b) on a.k = k and b.s <> a.s  -- ①'
//!
//! …  where a.q < (select 0.2 * avg(b.q) from b where b.k = a.k)
//! →  a LEFT JOIN (select b.k, 0.2 * avg(b.q) from b group by b.k)    -- ②
//!      on a.k = k  …  where a.q < avg
//! ```
//!
//! ① is a membership test, so it is a semi join (`NOT EXISTS` an anti join) —
//! the kinds `crabgresql_binder::JoinKind` gained for exactly this. Its
//! condition need not be the equality alone (①', TPC-H Q21): a semi join's `ON`
//! decides *whether a left row has a match*, which is the same question the
//! subquery's filter answered, so any correlated conjunct may join it there. ②
//! is a *value*, so it is a left join: one row per group, NULL where the group
//! is absent, which is what a scalar subquery over no rows returns — and there
//! the extra conjunct is not available, because the grouping happens before the
//! join sees an outer row at all.
//!
//! # What is refused
//!
//! Everything the analysis cannot prove, because what it refuses still runs —
//! down the per-row path, merely slower — while a wrong answer does not
//! recover. `NOT IN` in particular: its three-valued semantics are not an anti
//! join's, as `JoinKind::Anti`'s own documentation warns.

use crabgresql_binder::{
    AggFn, AggInput, AggregatePlan, BinOp, BoundExpr, InsertPlan, InsertSource, JoinExpr,
    JoinInput, JoinKind, JoinPlan, LimitPlan, LogicalPlan, OutputColumn, QueryPlan, SetOpPlan,
    Subplan, SubplanId, SubqueryPlan, WindowPlan, walk_exprs_mut,
};
use crabgresql_types::{PgType, Value};

use crate::{OptimizerContext, OptimizerRule};

mod attach;
mod markers;
mod split;

use attach::Arm;
use split::{and, key_sides, names_an_outer_row, rebuild_and, split_correlation};

/// Rewrite correlated subqueries into join arms. See the module documentation.
pub struct DecorrelateSubqueries;

impl OptimizerRule for DecorrelateSubqueries {
    fn name(&self) -> &'static str {
        "decorrelate_subqueries"
    }

    fn rewrite(&self, plan: &mut LogicalPlan, ctx: &OptimizerContext) -> bool {
        if !ctx.decorrelate {
            return false;
        }
        rewrite_all(plan)
    }
}

/// Rewrite `plan` and everything below it — the plans nested in its nodes, and
/// the plans nested in its expressions.
fn rewrite_all(plan: &mut LogicalPlan) -> bool {
    // Bodies first: decorrelating an inner subquery can only make the outer
    // one easier to read, never harder, and a body rewritten *after* its marker
    // was lifted into a join arm would be rewritten in its new home anyway.
    let mut changed = rewrite_marker_bodies(plan);
    changed |= rewrite_nodes(plan);
    changed
}

/// Descend into every subquery marker of every node of `plan`, applying the
/// whole rule to each body.
fn rewrite_marker_bodies(plan: &mut LogicalPlan) -> bool {
    struct Bodies {
        changed: bool,
    }
    impl crabgresql_binder::ExprVisitor for Bodies {
        fn expr(&mut self, expr: &mut BoundExpr) {
            markers::for_each_marker_mut(expr, &mut |marker| {
                if let Some(subplan) = subplan_mut(marker) {
                    self.changed |= rewrite_all(&mut subplan.plan);
                }
            });
        }
    }
    let mut visitor = Bodies { changed: false };
    walk_exprs_mut(plan, &mut visitor);
    visitor.changed
}

/// Apply the node-level rewrites bottom up: a child is rewritten before the
/// parent that reads its rows.
fn rewrite_nodes(plan: &mut LogicalPlan) -> bool {
    let mut changed = rewrite_child_nodes(plan);
    // One rewrite at a time, re-examined: a node can carry several markers, and
    // each attached arm shifts where the next one's columns land.
    while rewrite_here(plan) {
        changed = true;
    }
    changed
}

/// [`rewrite_nodes`] for the plans a node holds as its row source.
fn rewrite_child_nodes(plan: &mut LogicalPlan) -> bool {
    let mut changed = false;
    match plan {
        LogicalPlan::Subquery(SubqueryPlan { source, .. })
        | LogicalPlan::Window(WindowPlan { source, .. })
        | LogicalPlan::Limit(LimitPlan { source, .. }) => changed |= rewrite_nodes(source),
        LogicalPlan::SetOp(SetOpPlan { arms, .. }) => {
            for arm in arms {
                changed |= rewrite_nodes(&mut arm.plan);
            }
        }
        LogicalPlan::Join(JoinPlan { source, .. }) => changed |= rewrite_join_inputs(source),
        LogicalPlan::Aggregate(AggregatePlan {
            input: AggInput::Join(source),
            ..
        }) => changed |= rewrite_join_inputs(source),
        LogicalPlan::Insert(InsertPlan {
            source: InsertSource::Query { input, .. },
            ..
        }) => changed |= rewrite_nodes(input),
        _ => {}
    }
    changed
}

fn rewrite_join_inputs(source: &mut JoinExpr) -> bool {
    match source {
        JoinExpr::Input {
            input: JoinInput::Subplan(plan),
            ..
        } => rewrite_nodes(plan),
        JoinExpr::Input { .. } => false,
        JoinExpr::Join { left, right, .. } => {
            rewrite_join_inputs(left) | rewrite_join_inputs(right)
        }
    }
}

fn rewrite_here(node: &mut LogicalPlan) -> bool {
    rewrite_semi_anti(node) || rewrite_scalar_aggregate(node)
}

// ---------------------------------------------------------------- ① semi/anti

/// Turn one `EXISTS` / `NOT EXISTS` / `x op ANY (…)` conjunct of this node's
/// `WHERE` into a semi or anti join.
///
/// Only a top-level `AND` conjunct: the join *is* the filter, so a marker under
/// an `OR` (or under a `NOT` the binder did not fold into `negated`) is not one
/// this may answer by dropping rows.
fn rewrite_semi_anti(node: &mut LogicalPlan) -> bool {
    let Some(left_width) = attach::source_width(node) else {
        return false;
    };
    let Some(predicate) = node_predicate(node) else {
        return false;
    };
    let mut conjuncts = Vec::new();
    split::flatten_and(predicate, &mut conjuncts);
    let Some((id, arm)) = conjuncts
        .into_iter()
        .find_map(|conjunct| as_semi_anti(conjunct, left_width))
    else {
        return false;
    };
    // Attach before removing the conjunct: an attachment that turned out to be
    // impossible then leaves the plan as it was, and a removal that somehow
    // failed leaves the subquery evaluated twice — slow, not wrong.
    if !attach::attach_arm(node, arm) {
        return false;
    }
    // The answer is whether the *marker* is gone, not whether an arm was added:
    // this is what the caller's loop reruns on, and reporting progress that did
    // not remove a marker would find the same one again on the next pass.
    remove_conjunct(node, id)
}

/// The join arm one `WHERE` conjunct becomes, if it is a marker this rewrite
/// covers.
fn as_semi_anti(conjunct: &BoundExpr, left_width: usize) -> Option<(SubplanId, Arm)> {
    match conjunct {
        BoundExpr::Exists { subplan, negated } => {
            let split = split_correlation(&subplan.plan)?;
            // An *uncorrelated* `EXISTS` is one boolean the executor folds
            // before the scan starts. A join would be strictly more work.
            if split.keys.is_empty() {
                return None;
            }
            let arm = lift(split, left_width, None)?;
            Some((
                subplan.id()?,
                Arm {
                    plan: arm.plan,
                    width: arm.width,
                    kind: if *negated {
                        JoinKind::Anti
                    } else {
                        JoinKind::Semi
                    },
                    on: arm.on?,
                },
            ))
        }
        // `x IN (SELECT …)` is `= ANY`, and every `op ANY` is a semi join on
        // `op`: the outer row survives exactly when some candidate satisfies the
        // comparison. NULLs need no special case — a comparison that is NULL
        // rather than true is not a match, and `ANY` answering NULL instead of
        // false drops the row from a `WHERE` just the same.
        //
        // `ALL` is the one that does not translate: `x <> ALL (…)`, which is how
        // `NOT IN` binds, is false when *any* comparison is NULL, while an anti
        // join would emit the row.
        BoundExpr::QuantifiedSubquery {
            subplan,
            all: false,
            cmp,
        } => {
            let BoundExpr::Binary { left: needle, .. } = cmp.as_ref() else {
                return None;
            };
            // The needle moves from the filter into the join condition, where a
            // nested loop evaluates it per candidate pair rather than per row.
            // That is a question about side effects, so only volatility bars it.
            if needle.contains_volatile_fn() || names_an_outer_row(needle) {
                return None;
            }
            let split = split_correlation(&subplan.plan)?;
            let value = single_projection(&split.stripped)?.clone();
            let arm = lift(split, left_width, Some(value))?;
            let comparison = fill_hole(cmp, arm.value?)?;
            Some((
                subplan.id()?,
                Arm {
                    plan: arm.plan,
                    width: arm.width,
                    kind: JoinKind::Semi,
                    on: match arm.on {
                        Some(lifted) => and(lifted, comparison),
                        // Uncorrelated: the needle comparison is the whole
                        // condition, and it is still a semi join — one the
                        // executor would otherwise answer by scanning a
                        // materialized candidate list once per outer row.
                        None => comparison,
                    },
                },
            ))
        }
        _ => None,
    }
}

/// A subplan turned into a join arm.
struct Lifted {
    plan: LogicalPlan,
    width: usize,
    /// `None` for an uncorrelated subplan, which has nothing to lift.
    on: Option<BoundExpr>,
    /// Where the subquery's own output column landed in the joined row, for the
    /// quantified comparison that asked for it.
    value: Option<BoundExpr>,
}

/// Turn a [`Split`] into an arm of a semi/anti join.
///
/// The lifted conjuncts are evaluated by the join node, so every column they
/// read has to be a column the arm *emits*: the arm projects exactly those, and
/// each conjunct is rebased onto where they landed. `value`, when given, is
/// projected past them — it is the subquery's own output column, which only a
/// quantified comparison needs.
///
/// Both halves of that only work on a conjunct this walk can see all of, which
/// is what [`split::liftable_into_a_join`] asks — of the keys as much as of the
/// residual. A key is no more rewritable for being an equality: `where (select
/// … where c.k = b.k) = outer.k` is one, and its inner side is a whole query
/// level whose own references this cannot touch.
///
/// [`Split`]: split::Split
fn lift(split: split::Split, left_width: usize, value: Option<BoundExpr>) -> Option<Lifted> {
    let mut lifted: Vec<BoundExpr> = split
        .keys
        .iter()
        .chain(&split.outer_residual)
        .cloned()
        .collect();
    if !lifted.iter().all(split::liftable_into_a_join) {
        return None;
    }
    // Deduplicated and in index order, so the arm projects each column once and
    // the slot a conjunct is rebased onto does not depend on which conjunct
    // mentioned it first.
    let mut columns = std::collections::BTreeMap::new();
    for conjunct in &mut lifted {
        markers::for_each_node_mut(conjunct, &mut |node| {
            if let BoundExpr::ColumnRef { index, ty } = node {
                columns.insert(*index, *ty);
            }
        });
    }
    let slots: std::collections::BTreeMap<usize, usize> = columns
        .keys()
        .enumerate()
        .map(|(slot, index)| (*index, slot))
        .collect();
    let mut projections: Vec<BoundExpr> = columns
        .iter()
        .map(|(index, ty)| BoundExpr::ColumnRef {
            index: *index,
            ty: *ty,
        })
        .collect();
    let value = value.map(|value| {
        let index = left_width + projections.len();
        let ty = value.ty();
        projections.push(value);
        BoundExpr::ColumnRef { index, ty }
    });
    for conjunct in &mut lifted {
        rebase_into_join(conjunct, left_width, &slots);
    }
    Some(Lifted {
        width: projections.len(),
        plan: reproject(split.stripped, projections)?,
        on: rebuild_and(lifted),
        value,
    })
}

/// Rewrite a conjunct lifted out of a subplan so that it reads the joined row.
///
/// Two index spaces meet in one expression and each moves somewhere else. A
/// `ColumnRef` addressed the subplan's own row and now addresses the arm column
/// that value was projected into. An `OuterColumnRef` at level 1 addressed the
/// enclosing row, which *is* the left input here, so it becomes a plain
/// `ColumnRef` at the same index. Each node is visited once, so a reference
/// rewritten from one space is not then rewritten again as if it were in the
/// other.
fn rebase_into_join(
    conjunct: &mut BoundExpr,
    left_width: usize,
    slots: &std::collections::BTreeMap<usize, usize>,
) {
    markers::for_each_node_mut(conjunct, &mut |node| match node {
        BoundExpr::ColumnRef { index, .. } => {
            // Every column the conjunct reads was collected into `slots`.
            *index = left_width + slots.get(index).copied().unwrap_or(*index);
        }
        BoundExpr::OuterColumnRef {
            level: 1,
            index,
            ty,
        } => {
            *node = BoundExpr::ColumnRef {
                index: *index,
                ty: *ty,
            };
        }
        _ => {}
    });
}

// -------------------------------------------------------- ② scalar aggregate

/// Replace one correlated scalar *aggregate* subquery with a column of a
/// grouped left-join arm.
///
/// Unlike ①, this rewrite is value-preserving rather than row-preserving, so the
/// marker may sit anywhere in an expression this node evaluates against its
/// source row — under an `OR`, inside a `CASE`, as an aggregate's argument.
fn rewrite_scalar_aggregate(node: &mut LogicalPlan) -> bool {
    let Some(left_width) = attach::source_width(node) else {
        return false;
    };
    let mut found = None;
    for_each_source_expr(node, &mut |expr| {
        markers::for_each_marker_mut(expr, &mut |marker| {
            if found.is_none() {
                found = as_scalar_aggregate(marker, left_width);
            }
        });
    });
    let Some((id, arm, replacement)) = found else {
        return false;
    };
    if !attach::attach_arm(node, arm) {
        return false;
    }
    // As in ①, the answer is whether the marker is gone: a replacement that
    // found nothing would otherwise report progress and be asked again.
    let mut replaced = false;
    for_each_source_expr(node, &mut |expr| {
        replaced |= markers::replace_marker(expr, id, &replacement);
    });
    replaced
}

/// The left-join arm and the expression that replaces the marker, if this
/// marker is a correlated aggregate over one implicit group.
fn as_scalar_aggregate(
    marker: &BoundExpr,
    left_width: usize,
) -> Option<(SubplanId, Arm, BoundExpr)> {
    let BoundExpr::ScalarSubquery { subplan, .. } = marker else {
        return None;
    };
    // The shape that makes this sound: exactly one aggregate over the *whole*
    // subquery result. Grouping it by the correlation keys then yields exactly
    // one row per key, so the left join neither multiplies outer rows nor can
    // trip the "more than one row returned by a subquery" check the per-row path
    // performs. A non-aggregate scalar subquery keeps that check — and therefore
    // keeps the per-row path.
    let LogicalPlan::Aggregate(AggregatePlan {
        group_exprs,
        aggregates,
        having,
        projections,
        ..
    }) = subplan.plan.as_ref()
    else {
        return None;
    };
    if !group_exprs.is_empty() || having.is_some() || aggregates.len() != 1 {
        return None;
    }
    let [projection] = projections.as_slice() else {
        return None;
    };
    let empty_group = empty_group_value(aggregates[0].func);
    let value_ty = projection.ty();
    // What the subquery returns for an outer row with no matching inner rows.
    // The left join answers a miss with NULL, so the two must agree.
    match empty_group {
        // NULL on the empty group, which the miss reproduces — as long as the
        // projection wrapped around the aggregate propagates that NULL.
        None if is_strict_in_slot(projection, 0) => {}
        // `count` answers 0, which NULL does not. Recognisable only when the
        // projection is the aggregate itself, so the substitute can restore the
        // 0 the miss lost.
        Some(_) if matches!(projection, BoundExpr::ColumnRef { index: 0, .. }) => {}
        _ => return None,
    }

    let split = split_correlation(&subplan.plan)?;
    // Uncorrelated: one execution before the scan starts, which the executor
    // already does. Nothing to win, and a join arm to pay for.
    if split.keys.is_empty() {
        return None;
    }
    // A conjunct naming the outer row that is not a key cannot ride into this
    // join's condition the way it can into a semi join's: the arm is *grouped*
    // before the join sees an outer row, so a filter that depends on one would
    // have to have been applied before the aggregate it changes.
    if !split.outer_residual.is_empty() {
        return None;
    }
    let LogicalPlan::Aggregate(mut arm) = split.stripped else {
        unreachable!("split preserves the node kind, matched as an Aggregate above");
    };
    let keys = split.keys;
    let slot = keys.len();
    // The aggregate's output row is `[group keys…, aggregates…]`, so grouping by
    // `n` keys moves the aggregate from slot 0 to slot `n` — and the projection
    // that reads it moves with it.
    let mut value = arm.projections.remove(0);
    value.shift_column_refs(slot as isize);
    let sides: Vec<_> = keys
        .iter()
        .map(|key| key_sides(key))
        .collect::<Option<_>>()?;
    arm.group_exprs = sides.iter().map(|(inner, ..)| (*inner).clone()).collect();
    arm.projections = sides
        .iter()
        .enumerate()
        .map(|(i, (inner, ..))| BoundExpr::ColumnRef {
            index: i,
            ty: inner.ty(),
        })
        .chain([value])
        .collect();
    arm.columns = arm
        .projections
        .iter()
        .enumerate()
        .map(|(i, expr)| OutputColumn::new(format!("k{i}"), expr.ty()))
        .collect();

    let column = BoundExpr::ColumnRef {
        index: left_width + slot,
        ty: value_ty,
    };
    let replacement = match empty_group {
        Some(zero) => BoundExpr::Coalesce {
            args: vec![
                column,
                BoundExpr::Const {
                    value: zero,
                    ty: value_ty,
                },
            ],
            ty: value_ty,
        },
        None => column,
    };
    Some((
        subplan.id()?,
        Arm {
            plan: LogicalPlan::Aggregate(arm),
            width: slot + 1,
            kind: JoinKind::Left,
            on: key_condition(&sides, left_width)?,
        },
        replacement,
    ))
}

/// What this aggregate returns for a group with no rows in it, when that is not
/// NULL.
///
/// `count` is the only one: PostgreSQL's other aggregates all start from a NULL
/// state and stay there on an empty input. The match is exhaustive on purpose —
/// a new aggregate has to answer this question before it can be decorrelated.
fn empty_group_value(func: AggFn) -> Option<Value> {
    match func {
        AggFn::Count => Some(Value::Int8(0)),
        AggFn::Min | AggFn::Max | AggFn::Sum | AggFn::Avg | AggFn::StringAgg => None,
    }
}

/// Whether `expr` is NULL whenever the value in `slot` is.
///
/// A whitelist of nodes that are strict in every operand, which is what lets the
/// TPC-H shape `0.2 * avg(x)` ride along with the aggregate it wraps: with the
/// aggregate NULL the product is NULL, so a left-join miss reproduces the empty
/// group exactly. `AND`/`OR` are excluded because they are not strict (`false
/// AND NULL` is `false`), and so is everything that can manufacture a value —
/// `coalesce`, `CASE`, a function call.
fn is_strict_in_slot(expr: &BoundExpr, slot: usize) -> bool {
    match expr {
        BoundExpr::ColumnRef { index, .. } => *index == slot,
        BoundExpr::Coerce { expr, .. }
        | BoundExpr::Reinterpret { expr, .. }
        | BoundExpr::Collate { expr, .. }
        | BoundExpr::Unary { expr, .. } => is_strict_in_slot(expr, slot),
        BoundExpr::Binary {
            op, left, right, ..
        } if !matches!(op, BinOp::And | BinOp::Or) => {
            is_strict_in_slot(left, slot) || is_strict_in_slot(right, slot)
        }
        _ => false,
    }
}

// ------------------------------------------------------------------ plumbing

/// The condition equating each correlation key with the grouped arm column it
/// became — the scalar-aggregate rewrite's counterpart to [`lift`], whose arm
/// projects the keys' *inner* sides as its grouping columns rather than the raw
/// columns they read.
///
/// The outer reference becomes a plain `ColumnRef`: at this level the enclosing
/// row *is* the left input, so what was a level-1 reference is a column of the
/// joined row. Keeping the binder's coercion around it keeps the comparison the
/// one the subquery's filter performed.
fn key_condition(
    keys: &[(&BoundExpr, &BoundExpr, PgType, u32)],
    left_width: usize,
) -> Option<BoundExpr> {
    let conjuncts = keys
        .iter()
        .enumerate()
        .map(|(i, (inner, outer, arg_ty, collation))| BoundExpr::Binary {
            op: BinOp::Eq,
            arg_ty: *arg_ty,
            collation: *collation,
            left: Box::new(outer_ref_to_column(outer)),
            right: Box::new(BoundExpr::ColumnRef {
                index: left_width + i,
                ty: inner.ty(),
            }),
        })
        .collect();
    rebuild_and(conjuncts)
}

/// Rewrite a level-1 `OuterColumnRef` — bare or under the binder's coercion —
/// as a reference to the same slot of the row being joined.
fn outer_ref_to_column(expr: &BoundExpr) -> BoundExpr {
    match expr {
        BoundExpr::OuterColumnRef { index, ty, .. } => BoundExpr::ColumnRef {
            index: *index,
            ty: *ty,
        },
        BoundExpr::Coerce { expr, ty } => BoundExpr::Coerce {
            expr: Box::new(outer_ref_to_column(expr)),
            ty: *ty,
        },
        // `split` accepts no other shape as a key's outer side.
        other => other.clone(),
    }
}

/// Give `plan` a new target list. Only the two shapes whose projections index
/// the plan's own source row — which is where a stripped correlation key lives.
fn reproject(mut plan: LogicalPlan, projections: Vec<BoundExpr>) -> Option<LogicalPlan> {
    let columns = projections
        .iter()
        .enumerate()
        .map(|(i, expr)| OutputColumn::new(format!("k{i}"), expr.ty()))
        .collect();
    match &mut plan {
        LogicalPlan::Query(QueryPlan {
            columns: slot,
            projections: target,
            ..
        })
        | LogicalPlan::Join(JoinPlan {
            columns: slot,
            projections: target,
            ..
        }) => {
            *slot = columns;
            *target = projections;
        }
        // Notably an `Aggregate`: `exists (select count(*) …)` is true for every
        // outer row, since an implicit group always produces its one row, and a
        // semi join on the correlation keys would drop the rows with no match.
        _ => return None,
    }
    Some(plan)
}

/// The single expression a one-column subquery projects.
fn single_projection(plan: &LogicalPlan) -> Option<&BoundExpr> {
    let projections = match plan {
        LogicalPlan::Query(QueryPlan { projections, .. })
        | LogicalPlan::Join(JoinPlan { projections, .. }) => projections,
        _ => return None,
    };
    match projections.as_slice() {
        [only] => Some(only),
        _ => None,
    }
}

/// Substitute `candidate` into the hole of a quantified comparison's template,
/// giving an ordinary comparison the join node can evaluate.
///
/// The template is `needle op <hole>`, where the hole is a NULL constant under
/// whatever coercions the binder resolved for the candidate values. Those
/// coercions are kept — they are what every candidate was to be cast by, and the
/// arm column is now that candidate — and the *bare* hole's own type becomes a
/// cast, because that is the one the per-row path applies as it goes
/// (`eval_quantified`) rather than one the binder wrote down.
///
/// `None` unless both operands then have the comparison's declared type: what
/// this becomes is a join condition, and a hash key that is not the type it says
/// it is is not merely slow (see `split::operands_match_arg_ty`).
fn fill_hole(cmp: &BoundExpr, candidate: BoundExpr) -> Option<BoundExpr> {
    let BoundExpr::Binary {
        op,
        arg_ty,
        collation,
        left,
        right,
    } = cmp
    else {
        return None;
    };
    let right = fill_hole_operand(right, candidate)?;
    if !split::operands_match_arg_ty(*arg_ty, left, &right) {
        return None;
    }
    Some(BoundExpr::Binary {
        op: *op,
        arg_ty: *arg_ty,
        collation: *collation,
        left: left.clone(),
        right: Box::new(right),
    })
}

fn fill_hole_operand(hole: &BoundExpr, candidate: BoundExpr) -> Option<BoundExpr> {
    match hole {
        BoundExpr::Const { ty, .. } if *ty == candidate.ty() => Some(candidate),
        BoundExpr::Const { ty, .. } => Some(BoundExpr::Coerce {
            expr: Box::new(candidate),
            ty: *ty,
        }),
        BoundExpr::Coerce { expr, ty } => Some(BoundExpr::Coerce {
            expr: Box::new(fill_hole_operand(expr, candidate)?),
            ty: *ty,
        }),
        BoundExpr::Collate {
            expr,
            collation,
            explicit,
        } => Some(BoundExpr::Collate {
            expr: Box::new(fill_hole_operand(expr, candidate)?),
            collation: *collation,
            explicit: *explicit,
        }),
        // Not a template the executor could have driven either; it reports that
        // itself, so leave the marker alone.
        _ => None,
    }
}

/// This node's `WHERE`, for the three node kinds that carry one over a row
/// source a join arm can be added to.
fn node_predicate(node: &LogicalPlan) -> Option<&BoundExpr> {
    match node {
        LogicalPlan::Query(QueryPlan { predicate, .. })
        | LogicalPlan::Join(JoinPlan { predicate, .. })
        | LogicalPlan::Aggregate(AggregatePlan { predicate, .. }) => predicate.as_ref(),
        _ => None,
    }
}

/// Drop the top-level conjunct that *is* the marker `id`, now that a join node
/// answers it.
///
/// By identity rather than by position: the node may have changed kind on the
/// way here (a `Query` grows into a `Join`), and the predicate travelled with
/// it.
fn remove_conjunct(node: &mut LogicalPlan, id: SubplanId) -> bool {
    let slot = match node {
        LogicalPlan::Query(QueryPlan { predicate, .. })
        | LogicalPlan::Join(JoinPlan { predicate, .. })
        | LogicalPlan::Aggregate(AggregatePlan { predicate, .. }) => predicate,
        _ => return false,
    };
    let Some(predicate) = slot.take() else {
        return false;
    };
    let mut conjuncts = Vec::new();
    flatten_and_owned(predicate, &mut conjuncts);
    let before = conjuncts.len();
    conjuncts.retain(|conjunct| markers::marker_id(conjunct) != Some(id));
    let removed = conjuncts.len() < before;
    *slot = rebuild_and(conjuncts);
    removed
}

/// [`split::flatten_and`] taking ownership, for the rebuild that follows.
fn flatten_and_owned(expr: BoundExpr, out: &mut Vec<BoundExpr>) {
    match expr {
        BoundExpr::Binary {
            op: BinOp::And,
            left,
            right,
            ..
        } => {
            flatten_and_owned(*left, out);
            flatten_and_owned(*right, out);
        }
        other => out.push(other),
    }
}

/// Every expression this node evaluates against its **source** row.
///
/// Deliberately not an `Aggregate`'s `having` or `projections`: those index the
/// *grouped* row, so a marker there cannot be answered by a column of the
/// aggregate's input, and replacing it with one would read an unrelated value.
fn for_each_source_expr(node: &mut LogicalPlan, f: &mut dyn FnMut(&mut BoundExpr)) {
    match node {
        LogicalPlan::Query(QueryPlan {
            projections,
            predicate,
            ..
        })
        | LogicalPlan::Join(JoinPlan {
            projections,
            predicate,
            ..
        }) => {
            for expr in projections {
                f(expr);
            }
            if let Some(predicate) = predicate {
                f(predicate);
            }
        }
        LogicalPlan::Aggregate(AggregatePlan {
            predicate,
            group_exprs,
            aggregates,
            ..
        }) => {
            if let Some(predicate) = predicate {
                f(predicate);
            }
            for expr in group_exprs {
                f(expr);
            }
            for aggregate in aggregates {
                for arg in &mut aggregate.args {
                    f(arg);
                }
            }
        }
        _ => {}
    }
}

fn subplan_mut(expr: &mut BoundExpr) -> Option<&mut Subplan> {
    match expr {
        BoundExpr::ScalarSubquery { subplan, .. }
        | BoundExpr::ArraySubquery { subplan, .. }
        | BoundExpr::Exists { subplan, .. }
        | BoundExpr::QuantifiedSubquery { subplan, .. } => Some(subplan),
        _ => None,
    }
}
