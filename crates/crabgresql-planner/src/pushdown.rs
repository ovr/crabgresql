//! Join-condition extraction: move `WHERE` conjuncts into the join tree.
//!
//! The binder puts a comma-separated `FROM` together as `CROSS` joins carrying
//! no condition, and leaves the whole `WHERE` on the plan node *above* the tree
//! (`LogicalPlan::Join`/`Aggregate`'s `predicate`). Left alone that plans to a
//! Cartesian product streamed into a filter, while the equivalent explicit
//! `a JOIN b ON a.id = b.id` plans to a hash join.
//!
//! This pass closes the gap by relocating each AND-conjunct of the `WHERE` to
//! the deepest join node whose row still contains every column it references.
//! Once a cross-side equality sits on a join node, the existing
//! [`extract_hash_keys`](super::extract_hash_keys) turns it into a hash key for
//! free — this pass never mentions hash joins itself.
//!
//! # Index spaces
//!
//! A join node's predicate is base-0 relative to *its own subtree*: the binder
//! binds each `ON` against that comma group's relations starting at 0, and
//! splices groups together without renumbering their internal predicates. The
//! query-level `WHERE`, by contrast, indexes the whole combined row. The two
//! coincide only on the left spine from the root, so a conjunct is rebased by
//! the target subtree's base offset at the moment it is attached.

use crabgresql_binder::{BinOp, BoundExpr, JoinExpr, JoinKind};

use crate::{flatten_and, flatten_or, rebuild_and, rebuild_or};

/// Push AND-conjuncts of a query-level `WHERE` into the join tree they belong
/// to, rewriting `source` in place. Returns the conjuncts that could not be
/// relocated, re-AND-ed, to stay as the plan-level filter.
pub(crate) fn push_where_into_joins(
    source: &mut JoinExpr,
    predicate: Option<BoundExpr>,
) -> Option<BoundExpr> {
    let predicate = predicate?;
    // Before splitting on AND: a `WHERE` that is one big OR has no conjunct to
    // split, but its arms may share one. Factoring first is what gives the loop
    // below anything to place at all for such a query.
    let predicate = factor_common_or_conjuncts(predicate);
    let mut conjuncts = Vec::new();
    flatten_and(predicate, &mut conjuncts);

    let mut kept = Vec::new();
    for conjunct in conjuncts {
        if !is_relocatable(&conjunct) {
            kept.push(conjunct);
            continue;
        }
        if let Some(back) = place(source, 0, conjunct) {
            kept.push(back);
        }
    }
    rebuild_and(kept)
}

/// Rewrite `(A AND B) OR (A AND C)` into `A AND (B OR C)`, so that a predicate
/// written as one big `OR` still exposes the conjuncts hiding in every arm.
///
/// This is the shape TPC-H Q19 is written in: its whole `WHERE` is an `OR` of
/// three arms that each repeat the join equality `p_partkey = l_partkey` and the
/// same two `lineitem` restrictions. Without factoring, [`flatten_and`] yields a
/// single conjunct spanning both relations, nothing can be placed, and the join
/// runs as a Cartesian product. PostgreSQL does the same transformation in
/// `prepqual.c`.
///
/// Distributing a conjunct out of an `OR` is sound under SQL's three-valued
/// logic: Kleene logic is a distributive lattice, so `(A∧B)∨(A∧C)` and
/// `A∧(B∨C)` agree on every assignment including NULL. What it does change is
/// *how often* a factored conjunct is evaluated, which is why a volatile one is
/// left where it was written. Hoisting only a subset of the common conjuncts
/// stays correct — distributivity is applied to the hoisted ones alone.
///
/// # Where this runs
///
/// Only where splitting a predicate into conjuncts can change the *plan*: the
/// join tree ([`push_where_into_joins`]) and index selection
/// ([`choose_access`](crate::choose_access), which classifies each conjunct as a
/// possible index key). A `Subquery`/`TableFunction`/`Values` filter or an
/// aggregate over a plain scan evaluates its predicate whole, so there is
/// nothing to win there — and the rewrite is not free of observable effect: a
/// hoisted conjunct is evaluated on rows the arms used to gate, so
/// `(x <> 0 AND 100/x > 1) OR (y <> 0 AND 100/x > 1)` starts raising `22012` on
/// a `x = 0` row. PostgreSQL makes exactly that trade in `prepqual.c`, but
/// [`qualorder`](crate::qualorder) sets the house rule that a pass which exists
/// to make queries faster should not change which of them succeed — so this one
/// stays where it pays for itself.
///
/// DML is out of scope for a different reason: `dml_targets` keeps the whole
/// predicate as written and uses the conjuncts only to pick a probe.
pub(crate) fn factor_common_or_conjuncts(expr: BoundExpr) -> BoundExpr {
    match expr {
        BoundExpr::Binary { op: BinOp::And, .. } => {
            let mut conjuncts = Vec::new();
            flatten_and(expr, &mut conjuncts);
            let conjuncts = conjuncts
                .into_iter()
                .map(factor_common_or_conjuncts)
                .collect();
            rebuild_and(conjuncts).expect("an AND flattens to at least two conjuncts")
        }
        BoundExpr::Binary { op: BinOp::Or, .. } => factor_or(expr),
        other => other,
    }
}

/// The `OR` case of [`factor_common_or_conjuncts`], split out to keep the
/// bookkeeping of "which conjunct of which arm is already spoken for" local.
fn factor_or(expr: BoundExpr) -> BoundExpr {
    let mut flat = Vec::new();
    flatten_or(expr, &mut flat);
    // Factor each arm first: an arm may itself be an AND over a nested OR, and
    // the inner factoring can expose a conjunct the outer one can then share.
    let arms: Vec<Vec<BoundExpr>> = flat
        .into_iter()
        .map(|arm| {
            let mut conjuncts = Vec::new();
            flatten_and(factor_common_or_conjuncts(arm), &mut conjuncts);
            conjuncts
        })
        .collect();

    // A conjunct of the first arm is common when every *other* arm still has an
    // unclaimed conjunct equal to it. Claiming one occurrence per arm is what
    // keeps `(A AND A AND B) OR (A AND C)` from hoisting `A` twice against a
    // single `A` on the right.
    let mut claimed: Vec<Vec<bool>> = arms.iter().map(|arm| vec![false; arm.len()]).collect();
    for i in 0..arms[0].len() {
        // Relocating a volatile expression changes how many times it runs, and
        // hoisting it out of an OR does so even before any pushdown.
        if arms[0][i].contains_volatile_fn() {
            continue;
        }
        let candidate = &arms[0][i];
        let matches: Option<Vec<usize>> = arms
            .iter()
            .enumerate()
            .skip(1)
            .map(|(a, arm)| (0..arm.len()).find(|&j| !claimed[a][j] && arm[j] == *candidate))
            .collect();
        let Some(matches) = matches else { continue };
        claimed[0][i] = true;
        for (a, j) in matches.into_iter().enumerate() {
            claimed[a + 1][j] = true;
        }
    }

    let mut common = Vec::new();
    let mut residuals = Vec::with_capacity(arms.len());
    for (a, arm) in arms.into_iter().enumerate() {
        let mut residual = Vec::new();
        for (i, conjunct) in arm.into_iter().enumerate() {
            if claimed[a][i] {
                // Every arm claims the same multiset, so collecting from the
                // first alone gives each common conjunct exactly once.
                if a == 0 {
                    common.push(conjunct);
                }
            } else {
                residual.push(conjunct);
            }
        }
        residuals.push(residual);
    }

    if common.is_empty() {
        return rebuild_or(
            residuals
                .into_iter()
                .map(|arm| rebuild_and(arm).expect("an unfactored arm keeps its conjuncts"))
                .collect(),
        )
        .expect("an OR flattens to at least two arms");
    }
    // An arm whose conjuncts were all common is now `TRUE`, and `TRUE OR x` is
    // `TRUE`, so the disjunction disappears and only the common part remains.
    if !residuals.iter().any(Vec::is_empty) {
        let disjunction = rebuild_or(
            residuals
                .into_iter()
                .map(|arm| rebuild_and(arm).expect("a non-empty residual rebuilds"))
                .collect(),
        )
        .expect("an OR flattens to at least two arms");
        common.push(disjunction);
    }
    rebuild_and(common).expect("common is non-empty here")
}

/// Place `conjunct` in `node` or a descendant, preferring the deepest legal
/// home. `base` is the index at which `node`'s row starts in the enclosing
/// combined row; the conjunct is rebased by `-base` when it lands, since a
/// node's predicate is base-0 relative to its own subtree.
///
/// Returns `None` once placed, or hands the conjunct back untouched when
/// nothing in this subtree may take it.
fn place(node: &mut JoinExpr, base: usize, conjunct: BoundExpr) -> Option<BoundExpr> {
    // A leaf has no predicate slot, so a conjunct that reaches one goes back up
    // to the nearest join node.
    let JoinExpr::Join {
        left,
        right,
        kind,
        predicate,
    } = node
    else {
        return Some(conjunct);
    };
    let Some((lo, hi)) = conjunct.column_ref_bounds() else {
        // `None` means this expression has no visible dependency in the
        // current row's index space. It does not mean the conjunct was placed:
        // correlated subplans, for example, carry their dependency in an
        // OuterColumnRef that bounds deliberately ignore.
        return Some(conjunct);
    };
    let split = base + left.width();
    debug_assert!(
        lo >= base && hi < split + right.width(),
        "a conjunct only descends into a subtree that contains all its columns"
    );

    // Sink as far as the columns and the join kinds allow before landing.
    let conjunct = if hi < split && may_descend_left(*kind) {
        place(left, base, conjunct)?
    } else if lo >= split && may_descend_right(*kind) {
        place(right, split, conjunct)?
    } else {
        conjunct
    };

    if !may_attach(*kind) {
        return Some(conjunct);
    }
    let mut local = conjunct;
    local.shift_column_refs(-(base as isize));
    *predicate = rebuild_and(predicate.take().into_iter().chain([local]).collect());
    // A cross join is an inner join with no condition; now that it has one, the
    // kind has to say so — the executor's nested loop applies a predicate on any
    // kind, but `Cross` is the planner's own marker for "unconditional".
    if *kind == JoinKind::Cross {
        *kind = JoinKind::Inner;
    }
    None
}

/// Whether a conjunct of the query-level `WHERE` may be AND-ed into this join's
/// own condition.
///
/// `WHERE` is applied above the whole tree, an `ON` clause before null
/// extension, so this is only sound where the join cannot null-extend. It is
/// unsound for an outer join *even for a conjunct over its preserved side*:
/// in `a LEFT JOIN b ON true WHERE a.x = 1`, moving `a.x = 1` into the `ON`
/// makes an `a` row with `x = 2` fail to match, get null-extended, and be
/// emitted — a row the `WHERE` would have dropped.
///
/// It is unsound for a semi/anti join for the mirror-image reason: the `ON`
/// condition there decides whether a left row *has a match*, so AND-ing a
/// `WHERE` conjunct into it makes an anti join emit exactly the rows the
/// `WHERE` meant to drop.
fn may_attach(kind: JoinKind) -> bool {
    matches!(kind, JoinKind::Cross | JoinKind::Inner)
}

/// Whether a conjunct over only the left input may be evaluated *below* this
/// join instead of above it. Sound exactly when the left side is preserved:
/// every left row then reaches the output with its own columns intact, so
/// filtering before or after the join drops the same rows.
///
/// That covers the semi/anti kinds as well: each emits left rows unchanged (and
/// at most once), so dropping a left row before the match test drops exactly the
/// same output row.
fn may_descend_left(kind: JoinKind) -> bool {
    matches!(
        kind,
        JoinKind::Cross | JoinKind::Inner | JoinKind::Left | JoinKind::Semi | JoinKind::Anti
    )
}

/// Mirror of [`may_descend_left`] for the right input.
///
/// Descending into a *null-supplying* side is never sound. In the anti-join
/// idiom `a LEFT JOIN b ON a.id = b.id WHERE b.y IS NULL`, pushing `b.y IS NULL`
/// into `b` drops the `b` rows an `a` row used to match, so that `a` row is now
/// null-extended and *passes* the `WHERE` — a row that should not exist.
fn may_descend_right(kind: JoinKind) -> bool {
    matches!(kind, JoinKind::Cross | JoinKind::Inner | JoinKind::Right)
}

/// Whether the planner can see every column this conjunct depends on, and may
/// therefore move it to a narrower row.
///
/// [`BoundExpr::column_ref_bounds`] is an over-approximating hull, which is the
/// safe direction for a containment test — it can only refuse a legal move. But
/// it reports *nothing* for a subplan or an outer reference, and that is not the
/// same as "depends on no column": a correlated subquery carries its dependency
/// on this row inside its body as an `OuterColumnRef`, filled in at execution
/// against whatever row the node it sits on produces. Moving such a conjunct to
/// a narrower row silently reads a different column. Refuse them all.
///
/// A volatile conjunct is refused too. Relocating one does not just reorder
/// work: a conjunct sunk to a scan leaf is evaluated once per *scanned* row
/// where above the join it ran once per *joined* row, so `nextval()` would
/// advance a different number of times and a routine's side effects would
/// change count. PostgreSQL declines to push volatile quals for the same
/// reason.
pub(crate) fn is_relocatable(expr: &BoundExpr) -> bool {
    fn opaque(expr: &BoundExpr) -> bool {
        match expr {
            // A window marker is opaque for a different reason than the rest: a
            // window is evaluated above the whole join, and a `WHERE` can never
            // contain one, so reaching here means the extraction pass missed it.
            // Refusing keeps that binder bug from also relocating the conjunct.
            BoundExpr::OuterColumnRef { .. }
            | BoundExpr::ScalarSubquery { .. }
            | BoundExpr::ArraySubquery { .. }
            | BoundExpr::Exists { .. }
            | BoundExpr::QuantifiedSubquery { .. }
            | BoundExpr::WindowFunc { .. } => true,
            BoundExpr::ColumnRef { .. } | BoundExpr::Const { .. } | BoundExpr::Param { .. } => {
                false
            }
            BoundExpr::Unary { expr, .. }
            | BoundExpr::IsNull { expr, .. }
            | BoundExpr::BoolTest { expr, .. }
            | BoundExpr::Coerce { expr, .. }
            | BoundExpr::Collate { expr, .. }
            | BoundExpr::Reinterpret { expr, .. } => opaque(expr),
            BoundExpr::Binary { left, right, .. } => opaque(left) || opaque(right),
            BoundExpr::FuncCall { args, .. }
            | BoundExpr::Routine { args, .. }
            | BoundExpr::Srf { args, .. }
            | BoundExpr::Coalesce { args, .. }
            | BoundExpr::MinMax { args, .. } => args.iter().any(opaque),
            BoundExpr::Aggregate {
                agg_args, order_by, ..
            } => BoundExpr::agg_exprs(agg_args, order_by).any(opaque),
            BoundExpr::ArrayCtor { elems, .. } => elems.iter().any(opaque),
            BoundExpr::Subscript { base, index, .. } => opaque(base) || opaque(index),
            BoundExpr::Case { whens, else_, .. } => {
                whens.iter().any(|(c, r)| opaque(c) || opaque(r))
                    || else_.as_deref().is_some_and(opaque)
            }
            BoundExpr::QuantifiedArray { array, cmp, .. } => opaque(array) || opaque(cmp),
        }
    }
    // A conjunct referencing no column at all (`1 = 1`, `$1 IS NULL`) has no
    // subtree it belongs to more than any other, so leave it at the top.
    expr.column_ref_bounds().is_some() && !opaque(expr) && !expr.contains_volatile_fn()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_binder::{JoinInput, ScalarFn, TableFn};
    use crabgresql_types::collation::DEFAULT_COLLATION_OID;
    use crabgresql_types::{PgType, Value};

    fn col(index: usize) -> BoundExpr {
        BoundExpr::ColumnRef {
            index,
            ty: PgType::Int4,
        }
    }

    fn lit(value: i32) -> BoundExpr {
        BoundExpr::Const {
            value: Value::Int4(value),
            ty: PgType::Int4,
        }
    }

    fn binary(op: BinOp, left: BoundExpr, right: BoundExpr) -> BoundExpr {
        BoundExpr::Binary {
            op,
            arg_ty: if matches!(op, BinOp::And | BinOp::Or) {
                PgType::Bool
            } else {
                PgType::Int4
            },
            collation: DEFAULT_COLLATION_OID,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn eq(left: BoundExpr, right: BoundExpr) -> BoundExpr {
        binary(BinOp::Eq, left, right)
    }

    /// Fold the list with `AND`/`OR` the way [`rebuild_and`]/[`rebuild_or`] do,
    /// so a test that expects "unchanged" compares against the same associativity.
    fn all(op: BinOp, mut exprs: Vec<BoundExpr>) -> BoundExpr {
        let mut acc = exprs.pop().expect("non-empty");
        while let Some(next) = exprs.pop() {
            acc = binary(op, next, acc);
        }
        acc
    }

    /// `nextval(col0) = 1` — a conjunct that must never be hoisted out of an OR.
    fn volatile() -> BoundExpr {
        eq(
            BoundExpr::FuncCall {
                func: ScalarFn::Nextval,
                ret: PgType::Int4,
                args: vec![col(0)],
            },
            lit(1),
        )
    }

    fn leaf() -> JoinExpr {
        JoinExpr::Input {
            input: JoinInput::TableFunction {
                func: TableFn::GenerateSeries(PgType::Int4),
                args: Vec::new(),
                ordinality: false,
            },
            width: 1,
            lateral: false,
        }
    }

    #[test]
    fn place_returns_a_conjunct_with_no_visible_column_bounds() {
        let mut join = JoinExpr::Join {
            left: Box::new(leaf()),
            right: Box::new(leaf()),
            kind: JoinKind::Inner,
            predicate: None,
        };
        let conjunct = BoundExpr::Const {
            value: Value::Bool(true),
            ty: PgType::Bool,
        };

        assert_eq!(place(&mut join, 0, conjunct.clone()), Some(conjunct));
        let JoinExpr::Join { predicate, .. } = join else {
            panic!("expected Join node");
        };
        assert!(
            predicate.is_none(),
            "the conjunct was not placed or dropped"
        );
    }

    #[test]
    fn a_left_only_conjunct_descends_through_a_semi_join_without_attaching() {
        for kind in [JoinKind::Semi, JoinKind::Anti] {
            // The left child is a join, not a leaf, so there is a predicate slot
            // below to land in.
            let mut join = JoinExpr::Join {
                left: Box::new(JoinExpr::Join {
                    left: Box::new(leaf()),
                    right: Box::new(leaf()),
                    kind: JoinKind::Inner,
                    predicate: None,
                }),
                right: Box::new(leaf()),
                kind,
                predicate: None,
            };
            let conjunct = eq(col(0), lit(1));

            assert_eq!(place(&mut join, 0, conjunct.clone()), None, "{kind:?}");
            let JoinExpr::Join {
                left, predicate, ..
            } = join
            else {
                panic!("expected Join node");
            };
            assert!(
                predicate.is_none(),
                "{kind:?}: the conjunct must not become part of the match test"
            );
            let JoinExpr::Join { predicate, .. } = *left else {
                panic!("expected a Join node below");
            };
            assert_eq!(predicate, Some(conjunct), "{kind:?}");
        }
    }

    /// Two leaves: nowhere below to land, and this node may not take it either.
    #[test]
    fn a_semi_join_over_two_leaves_hands_a_conjunct_back() {
        for kind in [JoinKind::Semi, JoinKind::Anti] {
            let mut join = JoinExpr::Join {
                left: Box::new(leaf()),
                right: Box::new(leaf()),
                kind,
                predicate: None,
            };
            let conjunct = eq(col(0), lit(1));

            assert_eq!(
                place(&mut join, 0, conjunct.clone()),
                Some(conjunct),
                "{kind:?}"
            );
            let JoinExpr::Join { predicate, .. } = join else {
                panic!("expected Join node");
            };
            assert!(predicate.is_none(), "{kind:?}");
        }
    }

    #[test]
    fn common_conjunct_is_hoisted_out_of_an_or() {
        // (c0 = c1 AND c0 = 1) OR (c0 = c1 AND c0 = 2)
        let join_key = eq(col(0), col(1));
        let input = all(
            BinOp::Or,
            vec![
                all(BinOp::And, vec![join_key.clone(), eq(col(0), lit(1))]),
                all(BinOp::And, vec![join_key.clone(), eq(col(0), lit(2))]),
            ],
        );

        let expected = all(
            BinOp::And,
            vec![
                join_key,
                all(BinOp::Or, vec![eq(col(0), lit(1)), eq(col(0), lit(2))]),
            ],
        );
        assert_eq!(factor_common_or_conjuncts(input), expected);
    }

    #[test]
    fn an_arm_left_empty_swallows_the_whole_or() {
        // `A OR (A AND x)` is `A`: the first arm's residual is empty, so the
        // disjunction is TRUE wherever A holds and drops out entirely.
        let join_key = eq(col(0), col(1));
        let input = all(
            BinOp::Or,
            vec![
                join_key.clone(),
                all(BinOp::And, vec![join_key.clone(), eq(col(0), lit(1))]),
            ],
        );

        assert_eq!(factor_common_or_conjuncts(input), join_key);
    }

    #[test]
    fn an_or_without_a_common_conjunct_is_left_alone() {
        let input = all(
            BinOp::Or,
            vec![
                all(BinOp::And, vec![eq(col(0), lit(1)), eq(col(1), lit(2))]),
                all(BinOp::And, vec![eq(col(0), lit(3)), eq(col(1), lit(4))]),
            ],
        );

        assert_eq!(factor_common_or_conjuncts(input.clone()), input);
    }

    #[test]
    fn a_common_conjunct_is_hoisted_across_three_arms() {
        // Q19's arity. With two arms the cross-arm bookkeeping cannot be wrong —
        // every claim is against arm 1 — so the third arm is what exercises it.
        let join_key = eq(col(0), col(1));
        let arm = |n| all(BinOp::And, vec![join_key.clone(), eq(col(0), lit(n))]);
        let input = all(BinOp::Or, vec![arm(1), arm(2), arm(3)]);

        let expected = all(
            BinOp::And,
            vec![
                join_key,
                all(
                    BinOp::Or,
                    vec![eq(col(0), lit(1)), eq(col(0), lit(2)), eq(col(0), lit(3))],
                ),
            ],
        );
        assert_eq!(factor_common_or_conjuncts(input), expected);
    }

    #[test]
    fn a_conjunct_missing_from_one_arm_is_not_hoisted() {
        // Present in arms 0 and 1, absent from arm 2: a majority is not enough,
        // since the third arm can be satisfied without it.
        let join_key = eq(col(0), col(1));
        let input = all(
            BinOp::Or,
            vec![
                all(BinOp::And, vec![join_key.clone(), eq(col(0), lit(1))]),
                all(BinOp::And, vec![join_key, eq(col(0), lit(2))]),
                all(BinOp::And, vec![eq(col(1), lit(3)), eq(col(0), lit(4))]),
            ],
        );

        assert_eq!(factor_common_or_conjuncts(input.clone()), input);
    }

    #[test]
    fn a_volatile_common_conjunct_stays_in_every_arm() {
        // Hoisting `nextval(...) = 1` would advance the sequence once per row
        // instead of once per arm evaluated, so only the stable conjunct moves.
        let join_key = eq(col(0), col(1));
        let input = all(
            BinOp::Or,
            vec![
                all(BinOp::And, vec![join_key.clone(), volatile()]),
                all(
                    BinOp::And,
                    vec![join_key.clone(), volatile(), eq(col(0), lit(2))],
                ),
            ],
        );

        let expected = all(
            BinOp::And,
            vec![
                join_key,
                all(
                    BinOp::Or,
                    vec![
                        volatile(),
                        all(BinOp::And, vec![volatile(), eq(col(0), lit(2))]),
                    ],
                ),
            ],
        );
        assert_eq!(factor_common_or_conjuncts(input), expected);
    }

    #[test]
    fn a_repeated_conjunct_is_hoisted_only_as_often_as_every_arm_has_it() {
        // `(A AND A AND x) OR (A AND y)`: the right arm supplies one `A`, so one
        // `A` is hoisted and the other stays in the left arm's residual.
        let a = eq(col(0), col(1));
        let input = all(
            BinOp::Or,
            vec![
                all(BinOp::And, vec![a.clone(), a.clone(), eq(col(0), lit(1))]),
                all(BinOp::And, vec![a.clone(), eq(col(0), lit(2))]),
            ],
        );

        let expected = all(
            BinOp::And,
            vec![
                a.clone(),
                all(
                    BinOp::Or,
                    vec![
                        all(BinOp::And, vec![a, eq(col(0), lit(1))]),
                        eq(col(0), lit(2)),
                    ],
                ),
            ],
        );
        assert_eq!(factor_common_or_conjuncts(input), expected);
    }
}
