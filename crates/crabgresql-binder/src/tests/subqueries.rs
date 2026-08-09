//! Scalar subqueries, EXISTS and IN.

use super::common::*;

#[test]
fn scalar_subquery_binds_with_column_type() {
    // A FROM-less SELECT is a Values plan; its one projection is the marker.
    let ValuesPlan { rows, .. } = bound_values("SELECT (SELECT big FROM t)");
    assert!(matches!(
        rows[0][0],
        BoundExpr::ScalarSubquery {
            ty: PgType::Int8,
            ..
        }
    ));
}

#[test]
fn exists_binds_to_marker() {
    let QueryPlan { predicate, .. } =
        bound_query("SELECT id FROM t WHERE EXISTS (SELECT 1 FROM t)");
    assert!(matches!(
        predicate,
        Some(BoundExpr::Exists { negated: false, .. })
    ));
}

#[test]
fn not_exists_sets_negated() {
    let QueryPlan { predicate, .. } =
        bound_query("SELECT id FROM t WHERE NOT EXISTS (SELECT 1 FROM t)");
    assert!(matches!(
        predicate,
        Some(BoundExpr::Exists { negated: true, .. })
    ));
}

/// `IN (SELECT …)` is PG's `= ANY (…)`, so it binds to the quantified marker
/// with an equality template and no `ALL` flag.
#[test]
fn in_subquery_binds_to_marker() {
    let QueryPlan { predicate, .. } =
        bound_query("SELECT id FROM t WHERE id IN (SELECT id FROM t)");
    let Some(BoundExpr::QuantifiedSubquery { all, cmp, .. }) = predicate else {
        panic!("expected QuantifiedSubquery predicate");
    };
    assert!(!all);
    // The comparison template is `id = <hole>`, an equality Binary.
    assert!(matches!(*cmp, BoundExpr::Binary { op: BinOp::Eq, .. }));
}

/// `NOT IN (SELECT …)` is PG's `<> ALL (…)` — the De Morgan dual, so it binds
/// to an inequality template with `all` set rather than a negated equality.
#[test]
fn not_in_subquery_binds_to_all_of_inequality() {
    let QueryPlan { predicate, .. } =
        bound_query("SELECT id FROM t WHERE id NOT IN (SELECT id FROM t)");
    let Some(BoundExpr::QuantifiedSubquery { all, cmp, .. }) = predicate else {
        panic!("expected QuantifiedSubquery predicate");
    };
    assert!(all);
    assert!(matches!(
        *cmp,
        BoundExpr::Binary {
            op: BinOp::NotEq,
            ..
        }
    ));
}

#[test]
fn scalar_subquery_multiple_columns_errors() {
    let e = bind_err("SELECT (SELECT id, big FROM t)");
    assert_eq!(e.code, "42601");
    assert_eq!(e.message, "subquery must return only one column");
}

#[test]
fn in_subquery_multiple_columns_errors() {
    let e = bind_err("SELECT id FROM t WHERE id IN (SELECT id, big FROM t)");
    assert_eq!(e.code, "42601");
    assert_eq!(e.message, "subquery has too many columns");
}

#[test]
fn correlated_qualified_reference_binds_to_outer_column() {
    // A qualified reference to an enclosing relation resolves outward rather
    // than erroring: `x.id` becomes an OuterColumnRef at level 1, index 0
    // (the outer row's `id`).
    let pred = bound_query("SELECT id FROM t x WHERE EXISTS (SELECT 1 FROM t WHERE id = x.id)")
        .predicate
        .expect("a WHERE predicate");
    let BoundExpr::Exists { subplan, negated } = pred else {
        panic!("expected EXISTS marker, got {pred:?}");
    };
    assert!(!negated);
    let LogicalPlan::Query(QueryPlan {
        predicate: Some(inner),
        ..
    }) = &*subplan.0
    else {
        panic!("expected inner Query with a predicate");
    };
    let BoundExpr::Binary { right, .. } = inner else {
        panic!("expected `id = x.id` comparison, got {inner:?}");
    };
    assert!(
        matches!(
            **right,
            BoundExpr::OuterColumnRef {
                level: 1,
                index: 0,
                ..
            }
        ),
        "expected outer reference to x.id, got {right:?}"
    );
}

#[test]
fn correlated_unqualified_reference_binds_to_outer_column() {
    // An unqualified name absent from the subquery's own relation falls
    // through to the enclosing query. Here `flag` is not selected from in the
    // subquery's FROM-less body, so it resolves to the outer row.
    let pred = bound_query("SELECT id FROM t WHERE EXISTS (SELECT 1 WHERE flag)")
        .predicate
        .expect("a WHERE predicate");
    let BoundExpr::Exists { subplan, .. } = pred else {
        panic!("expected EXISTS marker, got {pred:?}");
    };
    // The FROM-less inner body binds as a single-row Values plan; its WHERE
    // is the bare `flag` outer reference (level 1, the outer row's `flag`).
    let LogicalPlan::Values(ValuesPlan {
        predicate: Some(inner),
        ..
    }) = &*subplan.0
    else {
        panic!("expected inner Values with a predicate");
    };
    assert!(
        matches!(
            inner,
            BoundExpr::OuterColumnRef {
                level: 1,
                index: 3,
                ..
            }
        ),
        "expected outer reference to flag (index 3), got {inner:?}"
    );
}

#[test]
fn uncorrelated_missing_column_still_errors_42703() {
    // A name in neither the subquery nor any enclosing query is still the
    // ordinary undefined-column error.
    let e = bind_err("SELECT id FROM t x WHERE EXISTS (SELECT 1 FROM t WHERE nope = 1)");
    assert_eq!(e.code, "42703");
}

#[test]
fn scalar_subquery_column_named_after_inner_column() {
    let ValuesPlan { columns, .. } = bound_values("SELECT (SELECT max(id) FROM t)");
    assert_eq!(columns[0].name, "max");
}

#[test]
fn exists_column_named_exists() {
    let ValuesPlan { columns, .. } = bound_values("SELECT EXISTS (SELECT 1 FROM t)");
    assert_eq!(columns[0].name, "exists");
}

#[test]
fn exists_strips_target_list_to_a_constant() {
    // The EXISTS subplan's projection is replaced with a constant so the
    // original target list (here a division by zero) is never evaluated.
    let ValuesPlan { rows, .. } = bound_values("SELECT EXISTS (SELECT id / 0 FROM t)");
    let BoundExpr::Exists { subplan, .. } = &rows[0][0] else {
        panic!("expected Exists");
    };
    // Borrowed out of the row, so the consuming extractor does not apply.
    let LogicalPlan::Query(QueryPlan { projections, .. }) = subplan.0.as_ref() else {
        panic!("expected Query subplan");
    };
    assert!(matches!(projections.as_slice(), [BoundExpr::Const { .. }]));
}

#[test]
fn update_set_accepts_subquery() {
    let UpdatePlan { assignments, .. } = bound_update("UPDATE t SET id = (SELECT max(id) FROM t)");
    assert!(matches!(assignments[0].1, BoundExpr::ScalarSubquery { .. }));
}

#[test]
fn delete_where_accepts_in_subquery() {
    let DeletePlan { predicate, .. } = bound_delete("DELETE FROM t WHERE id IN (SELECT id FROM t)");
    assert!(matches!(
        predicate,
        Some(BoundExpr::QuantifiedSubquery { .. })
    ));
}
