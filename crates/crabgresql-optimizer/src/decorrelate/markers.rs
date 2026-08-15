//! Walking inside one expression.
//!
//! `crabgresql_binder::walk_exprs_mut` reaches every *expression* a plan holds
//! and stops there; this reaches the nodes within one. Two things need it: the
//! subquery markers, which is the granularity both rewrites work at (one marker
//! becomes one join arm), and the column references, which a conjunct moving
//! from a subquery's filter into a join condition has to have rewritten.

use crabgresql_binder::{BoundExpr, Subplan, SubplanId};

/// The subplan `expr` carries, if it is a marker.
///
/// The walk's notion of "marker", not the rule's notion of "rewritable".
/// `ArraySubquery` is here so `rewrite_marker_bodies` descends into an
/// `ARRAY(SELECT …)` body and optimizes the correlated subqueries *inside* it;
/// neither rewrite can act on the marker itself, both matching on the variants
/// they cover. Nor could one: an array collects every row into a single value,
/// where a semi/anti join filters outer rows and ②'s grouped left join stands in
/// for a lone aggregate.
pub(super) fn subplan_of(expr: &BoundExpr) -> Option<&Subplan> {
    match expr {
        BoundExpr::ScalarSubquery { subplan, .. }
        | BoundExpr::ArraySubquery { subplan, .. }
        | BoundExpr::Exists { subplan, .. }
        | BoundExpr::QuantifiedSubquery { subplan, .. } => Some(subplan),
        _ => None,
    }
}

/// The identity of the marker `expr` is, if it is one and still carries one.
pub(super) fn marker_id(expr: &BoundExpr) -> Option<SubplanId> {
    subplan_of(expr)?.id()
}

/// Call `f` on every subquery marker in `expr`, outermost first.
pub(super) fn for_each_marker_mut(expr: &mut BoundExpr, f: &mut dyn FnMut(&mut BoundExpr)) {
    for_each_node_mut(expr, &mut |node| {
        if subplan_of(node).is_some() {
            f(node);
        }
    });
}

/// Call `f` on `expr` and on every sub-expression of it, outermost first.
///
/// A subquery marker's *body* is deliberately not entered: it is a plan of its
/// own, addressing its own row, so neither a rewrite of this level's column
/// indices nor the rule's own recursion belongs there. `f` may replace the node
/// it is given; the walk then continues into the replacement, which is what lets
/// a marker become an ordinary expression mid-traversal.
pub(super) fn for_each_node_mut(expr: &mut BoundExpr, f: &mut dyn FnMut(&mut BoundExpr)) {
    f(expr);
    match expr {
        BoundExpr::Const { .. }
        | BoundExpr::ColumnRef { .. }
        | BoundExpr::Param { .. }
        | BoundExpr::OuterColumnRef { .. }
        | BoundExpr::ScalarSubquery { .. }
        | BoundExpr::ArraySubquery { .. }
        | BoundExpr::Exists { .. } => {}
        BoundExpr::Unary { expr, .. }
        | BoundExpr::IsNull { expr, .. }
        | BoundExpr::BoolTest { expr, .. }
        | BoundExpr::Coerce { expr, .. }
        | BoundExpr::Collate { expr, .. }
        | BoundExpr::Reinterpret { expr, .. } => for_each_node_mut(expr, f),
        BoundExpr::Binary { left, right, .. } => {
            for_each_node_mut(left, f);
            for_each_node_mut(right, f);
        }
        BoundExpr::FuncCall { args, .. }
        | BoundExpr::Routine { args, .. }
        | BoundExpr::Srf { args, .. }
        | BoundExpr::Coalesce { args, .. }
        | BoundExpr::Aggregate { args, .. } => {
            for arg in args {
                for_each_node_mut(arg, f);
            }
        }
        BoundExpr::ArrayCtor { elems, .. } => {
            for elem in elems {
                for_each_node_mut(elem, f);
            }
        }
        BoundExpr::Subscript { base, index, .. } => {
            for_each_node_mut(base, f);
            for_each_node_mut(index, f);
        }
        BoundExpr::Case { whens, else_, .. } => {
            for (condition, result) in whens {
                for_each_node_mut(condition, f);
                for_each_node_mut(result, f);
            }
            if let Some(else_) = else_ {
                for_each_node_mut(else_, f);
            }
        }
        BoundExpr::WindowFunc { kind, spec, .. } => {
            for arg in kind.args_mut().iter_mut().chain(spec.exprs_mut()) {
                for_each_node_mut(arg, f);
            }
        }
        // Only the needle: the rest of a quantified comparison is a *template*
        // (`needle op <hole>`), whose shape the executor drives and whose hole
        // is a NULL constant rather than a place a marker can sit.
        BoundExpr::QuantifiedSubquery { cmp, .. } | BoundExpr::QuantifiedArray { cmp, .. } => {
            if let BoundExpr::Binary { left, .. } = cmp.as_mut() {
                for_each_node_mut(left, f);
            }
        }
    }
}

/// Replace the marker identified by `id` with `with`, wherever in `expr` it
/// sits. Returns whether it was found.
///
/// Identity rather than structural equality because two markers are never equal
/// (`Subplan`'s `PartialEq` returns `false` unconditionally) and because the
/// expression is searched twice: once to decide the rewrite, once to apply it.
pub(super) fn replace_marker(expr: &mut BoundExpr, id: SubplanId, with: &BoundExpr) -> bool {
    let mut replaced = false;
    for_each_marker_mut(expr, &mut |marker| {
        if marker_id(marker) == Some(id) {
            *marker = with.clone();
            replaced = true;
        }
    });
    replaced
}
