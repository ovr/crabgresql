//! Join binding: CROSS, ON, USING and NATURAL.

use super::common::*;

#[test]
fn cross_join_builds_join_plan_with_offsets() {
    // Two derived tables: a(x) at offset 0, b(y) at offset 1.
    let JoinPlan {
        source,
        columns,
        projections,
        ..
    } = bound_join("SELECT a.x, b.y FROM (VALUES (1)) a(x), (VALUES (2)) b(y)");
    assert!(matches!(
        source,
        JoinExpr::Join {
            kind: JoinKind::Cross,
            predicate: None,
            ..
        }
    ));
    assert_eq!(
        projections[0],
        BoundExpr::ColumnRef {
            index: 0,
            ty: PgType::Int4
        }
    );
    assert_eq!(
        projections[1],
        BoundExpr::ColumnRef {
            index: 1,
            ty: PgType::Int4
        }
    );
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["x", "y"]);
}

#[test]
fn cross_join_wildcard_expands_every_relation_in_order() {
    let JoinPlan {
        columns,
        projections,
        ..
    } = bound_join("SELECT * FROM (VALUES (1, 2)) a(x, y), (VALUES (3)) b(z)");
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["x", "y", "z"]);
    // b.z sits after a's two columns.
    assert_eq!(
        projections[2],
        BoundExpr::ColumnRef {
            index: 2,
            ty: PgType::Int4
        }
    );
}

#[test]
fn cross_join_qualified_refs_use_combined_row_index() {
    // `t` occupies indices 0..4 (id, big, name, flag); b.y follows at 4.
    let JoinPlan { projections, .. } = bound_join("SELECT t.id, b.y FROM t, (VALUES (2)) b(y)");
    assert_eq!(
        projections[0],
        BoundExpr::ColumnRef {
            index: 0,
            ty: PgType::Int4
        }
    );
    assert_eq!(
        projections[1],
        BoundExpr::ColumnRef {
            index: 4,
            ty: PgType::Int4
        }
    );
}

#[test]
fn ambiguous_unqualified_column_is_42702() {
    let e = bind_err("SELECT x FROM (VALUES (1)) a(x), (VALUES (2)) b(x)");
    assert_eq!(e.code, "42702");
    assert_eq!(e.message, "column reference \"x\" is ambiguous");
}

#[test]
fn duplicate_from_qualifier_is_42712() {
    let e = bind_err("SELECT * FROM t, t");
    assert_eq!(e.code, "42712");
    assert_eq!(e.message, "table name \"t\" specified more than once");
}

#[test]
fn explicit_cross_join_flattens_like_a_comma() {
    let JoinPlan { source, .. } =
        bound_join("SELECT * FROM (VALUES (1)) a(x) CROSS JOIN (VALUES (2)) b(y)");
    assert!(matches!(
        source,
        JoinExpr::Join {
            kind: JoinKind::Cross,
            predicate: None,
            ..
        }
    ));
}

#[test]
fn on_join_kinds_bind_boolean_predicates() -> anyhow::Result<()> {
    for (sql, expected) in [
        ("SELECT * FROM t a JOIN t b ON a.id = b.id", JoinKind::Inner),
        (
            "SELECT * FROM t a LEFT JOIN t b ON a.id = b.id",
            JoinKind::Left,
        ),
        (
            "SELECT * FROM t a RIGHT OUTER JOIN t b ON a.id = b.id",
            JoinKind::Right,
        ),
        (
            "SELECT * FROM t a FULL JOIN t b ON a.id = b.id",
            JoinKind::Full,
        ),
    ] {
        let LogicalPlan::Join(JoinPlan { source, .. }) = bind_one(sql)? else {
            panic!("expected Join for {sql}");
        };
        let JoinExpr::Join {
            kind, predicate, ..
        } = source
        else {
            panic!("expected binary join for {sql}");
        };
        assert_eq!(kind, expected, "{sql}");
        assert!(matches!(
            predicate,
            Some(BoundExpr::Binary {
                op: BinOp::Eq,
                arg_ty: PgType::Int4,
                ..
            })
        ));
    }

    Ok(())
}

#[test]
fn chained_join_is_left_associative_and_offsets_keep_growing() {
    let JoinPlan {
        source,
        projections,
        ..
    } = bound_join(
        "SELECT c.z FROM (VALUES (1)) a(x) \
         LEFT JOIN (VALUES (1)) b(y) ON a.x = b.y \
         JOIN (VALUES (1)) c(z) ON b.y = c.z",
    );
    let JoinExpr::Join {
        left,
        kind: JoinKind::Inner,
        ..
    } = source
    else {
        panic!("expected top inner join");
    };
    assert!(matches!(
        *left,
        JoinExpr::Join {
            kind: JoinKind::Left,
            ..
        }
    ));
    assert_eq!(
        projections[0],
        BoundExpr::ColumnRef {
            index: 2,
            ty: PgType::Int4
        }
    );
}

#[test]
fn join_on_scope_excludes_prior_comma_group() {
    let e = bind_err(
        "SELECT * FROM (VALUES (1)) a(x), \
         (VALUES (1)) b(y) JOIN (VALUES (1)) c(z) ON a.x = c.z",
    );
    assert_eq!(e.code, "42P01");
    assert_eq!(e.message, "missing FROM-clause entry for table \"a\"");
}

#[test]
fn join_on_must_be_boolean() {
    let e = bind_err("SELECT * FROM t a JOIN t b ON a.id");
    assert_eq!(e.code, "42804");
    assert_eq!(
        e.message,
        "argument of JOIN/ON must be type boolean, not type integer"
    );
}

#[test]
fn aggregate_in_join_on_is_rejected() {
    let e = bind_err("SELECT * FROM t a JOIN t b ON count(*) > 0");
    assert_eq!(e.code, "42803");
    assert_eq!(
        e.message,
        "aggregate functions are not allowed in JOIN conditions"
    );
}

#[test]
fn using_join_merges_column_and_builds_equality() {
    // `id` is merged (once, first); the other three columns of each side
    // follow — 1 + 3 + 3 = 7 output columns.
    let JoinPlan {
        source,
        columns,
        projections,
        ..
    } = bound_join("SELECT * FROM t a JOIN t b USING (id)");
    assert!(matches!(
        source,
        JoinExpr::Join {
            kind: JoinKind::Inner,
            predicate: Some(BoundExpr::Binary {
                op: BinOp::Eq,
                arg_ty: PgType::Int4,
                ..
            }),
            ..
        }
    ));
    assert_eq!(columns.len(), 7);
    assert_eq!(columns[0].name, "id");
    // The merged column carries the left side's value (index 0).
    assert_eq!(
        projections[0],
        BoundExpr::ColumnRef {
            index: 0,
            ty: PgType::Int4
        }
    );
}

#[test]
fn using_merged_column_is_unqualified_while_sides_stay_addressable() {
    let JoinPlan { projections, .. } =
        bound_join("SELECT id, a.id, b.id FROM t a JOIN t b USING (id)");
    // Unqualified `id` and `a.id` are the left copy (index 0); `b.id` the
    // right copy (index 4).
    let left = BoundExpr::ColumnRef {
        index: 0,
        ty: PgType::Int4,
    };
    let right = BoundExpr::ColumnRef {
        index: 4,
        ty: PgType::Int4,
    };
    assert_eq!(projections, vec![left.clone(), left, right]);
}

#[test]
fn using_full_join_merges_with_coalesce() {
    let JoinPlan { projections, .. } = bound_join("SELECT id FROM t a FULL JOIN t b USING (id)");
    // A full join's merged column is COALESCE(left, right), lowered to CASE.
    assert!(matches!(
        projections[0],
        BoundExpr::Case {
            ty: PgType::Int4,
            ..
        }
    ));
}

#[test]
fn natural_join_equates_every_common_column() {
    let JoinPlan {
        source, columns, ..
    } = bound_join("SELECT * FROM t a NATURAL JOIN t b");
    // All four columns are shared, so all four merge and the predicate ANDs
    // four equalities; no columns remain.
    assert_eq!(columns.len(), 4);
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "big", "name", "flag"]);
    assert!(matches!(
        source,
        JoinExpr::Join {
            kind: JoinKind::Inner,
            predicate: Some(BoundExpr::Binary { op: BinOp::And, .. }),
            ..
        }
    ));
}

#[test]
fn natural_join_without_common_columns_is_a_cross_product() {
    let JoinPlan {
        source, columns, ..
    } = bound_join("SELECT * FROM (VALUES (1)) a(x) NATURAL JOIN (VALUES (2)) b(y)");
    assert_eq!(columns.len(), 2);
    assert!(matches!(
        source,
        JoinExpr::Join {
            kind: JoinKind::Inner,
            predicate: None,
            ..
        }
    ));
}

#[test]
fn using_column_missing_on_a_side_is_42703() {
    let right = bind_err("SELECT * FROM t a JOIN (VALUES (1)) b(x) USING (id)");
    assert_eq!(right.code, "42703");
    assert_eq!(
        right.message,
        "column \"id\" specified in USING clause does not exist in right table"
    );
    let left = bind_err("SELECT * FROM (VALUES (1)) a(x) JOIN t b USING (id)");
    assert_eq!(left.code, "42703");
    assert_eq!(
        left.message,
        "column \"id\" specified in USING clause does not exist in left table"
    );
}

#[test]
fn using_join_in_a_later_comma_group_shifts_merged_indices() {
    // `t` (4 columns) is the first comma group, so the merged `id` and the
    // rest of the USING group live at combined-row offsets 4 and up.
    let JoinPlan {
        columns,
        projections,
        ..
    } = bound_join(
        "SELECT * FROM t, \
         (VALUES (5, 50)) a(id, x) JOIN (VALUES (5, 500)) b(id, y) USING (id)",
    );
    // t's 4 columns, then merged id, a.x, b.y — 7 in all.
    assert_eq!(columns.len(), 7);
    // The merged `id` carries a.id, shifted past t to index 4.
    assert_eq!(
        projections[4],
        BoundExpr::ColumnRef {
            index: 4,
            ty: PgType::Int4
        }
    );
    // b.y is a's width past that (a occupies 4,5; b occupies 6,7).
    assert_eq!(
        projections[6],
        BoundExpr::ColumnRef {
            index: 7,
            ty: PgType::Int4
        }
    );
}

#[test]
fn duplicate_using_column_is_42701() {
    let e = bind_err("SELECT * FROM t a JOIN t b USING (id, id)");
    assert_eq!(e.code, "42701");
    assert_eq!(
        e.message,
        "column name \"id\" appears more than once in USING clause"
    );
}

#[test]
fn using_merged_column_uses_common_type_not_comparison_type() {
    // real + int4: PG's select_common_type resolves the merged column to
    // real, even though the equality comparison promotes to float8.
    let JoinPlan { projections, .. } =
        bound_join("SELECT x FROM (VALUES (1.0::real)) a(x) JOIN (VALUES (1)) b(x) USING (x)");
    assert_eq!(projections[0].ty(), PgType::Float4);
}

#[test]
fn using_column_ambiguous_on_a_side_is_42702() {
    let e = bind_err("SELECT * FROM (VALUES (1, 2)) a(x, x) JOIN (VALUES (1)) b(x) USING (x)");
    assert_eq!(e.code, "42702");
    assert_eq!(
        e.message,
        "common column name \"x\" appears more than once in left table"
    );
}

#[test]
fn aggregate_accepts_join_input() -> anyhow::Result<()> {
    let LogicalPlan::Aggregate(AggregatePlan {
        input: AggInput::Join(source),
        aggregates,
        ..
    }) = bind_one("SELECT count(*) FROM t a LEFT JOIN t b ON a.id = b.id")?
    else {
        panic!("expected Aggregate over Join");
    };
    assert_eq!(aggregates.len(), 1);
    assert!(matches!(
        source,
        JoinExpr::Join {
            kind: JoinKind::Left,
            ..
        }
    ));

    Ok(())
}

#[test]
fn where_referencing_both_relations_binds() {
    let JoinPlan { predicate, .. } =
        bound_join("SELECT a.x FROM (VALUES (1)) a(x), (VALUES (1)) b(y) WHERE a.x = b.y");
    assert!(predicate.is_some());
}

#[test]
fn duplicate_column_within_relation_is_ambiguous_42702() {
    // A duplicate column alias makes a reference ambiguous, as in PG —
    // whether unqualified or qualified into that relation.
    let e = bind_err("SELECT x FROM (VALUES (1, 2)) a(x, x)");
    assert_eq!(e.code, "42702");
    assert_eq!(e.message, "column reference \"x\" is ambiguous");
    let e = bind_err("SELECT a.x FROM (VALUES (1, 2)) a(x, x)");
    assert_eq!(e.code, "42702");
    assert_eq!(e.message, "column reference \"x\" is ambiguous");
}

#[test]
fn qualified_missing_column_names_the_qualifier() {
    // PG prints `column q.c does not exist` for a qualified reference,
    // unquoted and qualifier-prefixed (contrast the unqualified form).
    let e = bind_err("SELECT x.nope FROM t x");
    assert_eq!(e.code, "42703");
    assert_eq!(e.message, "column x.nope does not exist");
}
