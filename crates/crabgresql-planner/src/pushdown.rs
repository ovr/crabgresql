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

use crabgresql_binder::{BoundExpr, JoinExpr, JoinKind};

use crate::{flatten_and, rebuild_and};

/// Push AND-conjuncts of a query-level `WHERE` into the join tree they belong
/// to, rewriting `source` in place. Returns the conjuncts that could not be
/// relocated, re-AND-ed, to stay as the plan-level filter.
pub(crate) fn push_where_into_joins(
    source: &mut JoinExpr,
    predicate: Option<BoundExpr>,
) -> Option<BoundExpr> {
    let predicate = predicate?;
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
fn may_attach(kind: JoinKind) -> bool {
    matches!(kind, JoinKind::Cross | JoinKind::Inner)
}

/// Whether a conjunct over only the left input may be evaluated *below* this
/// join instead of above it. Sound exactly when the left side is preserved:
/// every left row then reaches the output with its own columns intact, so
/// filtering before or after the join drops the same rows.
fn may_descend_left(kind: JoinKind) -> bool {
    matches!(kind, JoinKind::Cross | JoinKind::Inner | JoinKind::Left)
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
            BoundExpr::OuterColumnRef { .. }
            | BoundExpr::ScalarSubquery { .. }
            | BoundExpr::Exists { .. }
            | BoundExpr::QuantifiedSubquery { .. } => true,
            BoundExpr::ColumnRef { .. } | BoundExpr::Const { .. } | BoundExpr::Param { .. } => {
                false
            }
            BoundExpr::Unary { expr, .. }
            | BoundExpr::IsNull { expr, .. }
            | BoundExpr::Coerce { expr, .. }
            | BoundExpr::Collate { expr, .. }
            | BoundExpr::Reinterpret { expr, .. } => opaque(expr),
            BoundExpr::Binary { left, right, .. } => opaque(left) || opaque(right),
            BoundExpr::FuncCall { args, .. }
            | BoundExpr::Routine { args, .. }
            | BoundExpr::Srf { args, .. }
            | BoundExpr::Aggregate { args, .. } => args.iter().any(opaque),
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
    use crabgresql_binder::{JoinInput, TableFn};
    use crabgresql_types::{PgType, Value};

    fn leaf() -> JoinExpr {
        JoinExpr::Input {
            input: JoinInput::TableFunction {
                func: TableFn::GenerateSeries(PgType::Int4),
                args: Vec::new(),
            },
            width: 1,
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
        assert!(predicate.is_none(), "the conjunct was not placed or dropped");
    }
}
