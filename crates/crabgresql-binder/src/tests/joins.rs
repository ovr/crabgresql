//! Join binding: CROSS, ON, USING and NATURAL.

use super::common::*;

#[test]
fn cross_join_builds_join_plan_with_offsets() -> anyhow::Result<()> {
    // Two derived tables: a(x) at offset 0, b(y) at offset 1.
    let JoinPlan {
        source,
        columns,
        projections,
        ..
    } = bound_join("SELECT a.x, b.y FROM (VALUES (1)) a(x), (VALUES (2)) b(y)")?;
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
    Ok(())
}

#[test]
fn cross_join_wildcard_expands_every_relation_in_order() -> anyhow::Result<()> {
    let JoinPlan {
        columns,
        projections,
        ..
    } = bound_join("SELECT * FROM (VALUES (1, 2)) a(x, y), (VALUES (3)) b(z)")?;
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
    Ok(())
}

#[test]
fn cross_join_qualified_refs_use_combined_row_index() -> anyhow::Result<()> {
    // `t` occupies indices 0..4 (id, big, name, flag); b.y follows at 4.
    let JoinPlan { projections, .. } = bound_join("SELECT t.id, b.y FROM t, (VALUES (2)) b(y)")?;
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
    Ok(())
}

#[test]
fn ambiguous_unqualified_column_is_42702() -> anyhow::Result<()> {
    let e = bind_err("SELECT x FROM (VALUES (1)) a(x), (VALUES (2)) b(x)")?;
    assert_eq!(e.code, "42702");
    assert_eq!(e.message, "column reference \"x\" is ambiguous");
    Ok(())
}

#[test]
fn duplicate_from_qualifier_is_42712() -> anyhow::Result<()> {
    let e = bind_err("SELECT * FROM t, t")?;
    assert_eq!(e.code, "42712");
    assert_eq!(e.message, "table name \"t\" specified more than once");
    Ok(())
}

#[test]
fn explicit_cross_join_flattens_like_a_comma() -> anyhow::Result<()> {
    let JoinPlan { source, .. } =
        bound_join("SELECT * FROM (VALUES (1)) a(x) CROSS JOIN (VALUES (2)) b(y)")?;
    assert!(matches!(
        source,
        JoinExpr::Join {
            kind: JoinKind::Cross,
            predicate: None,
            ..
        }
    ));
    Ok(())
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
        let LogicalPlan::Join(JoinPlan { source, .. }) = bound(sql)? else {
            bail!("expected Join for {sql}");
        };
        let JoinExpr::Join {
            kind, predicate, ..
        } = source
        else {
            bail!("expected binary join for {sql}");
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
fn chained_join_is_left_associative_and_offsets_keep_growing() -> anyhow::Result<()> {
    let JoinPlan {
        source,
        projections,
        ..
    } = bound_join(
        "SELECT c.z FROM (VALUES (1)) a(x) \
         LEFT JOIN (VALUES (1)) b(y) ON a.x = b.y \
         JOIN (VALUES (1)) c(z) ON b.y = c.z",
    )?;
    let JoinExpr::Join {
        left,
        kind: JoinKind::Inner,
        ..
    } = source
    else {
        bail!("expected top inner join");
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
    Ok(())
}

#[test]
fn join_on_scope_excludes_prior_comma_group() -> anyhow::Result<()> {
    let e = bind_err(
        "SELECT * FROM (VALUES (1)) a(x), \
         (VALUES (1)) b(y) JOIN (VALUES (1)) c(z) ON a.x = c.z",
    )?;
    assert_eq!(e.code, "42P01");
    assert_eq!(e.message, "missing FROM-clause entry for table \"a\"");
    Ok(())
}

#[test]
fn join_on_must_be_boolean() -> anyhow::Result<()> {
    let e = bind_err("SELECT * FROM t a JOIN t b ON a.id")?;
    assert_eq!(e.code, "42804");
    assert_eq!(
        e.message,
        "argument of JOIN/ON must be type boolean, not type integer"
    );
    Ok(())
}

#[test]
fn aggregate_in_join_on_is_rejected() -> anyhow::Result<()> {
    let e = bind_err("SELECT * FROM t a JOIN t b ON count(*) > 0")?;
    assert_eq!(e.code, "42803");
    assert_eq!(
        e.message,
        "aggregate functions are not allowed in JOIN conditions"
    );
    Ok(())
}

#[test]
fn using_join_merges_column_and_builds_equality() -> anyhow::Result<()> {
    // `id` is merged (once, first); the other three columns of each side
    // follow — 1 + 3 + 3 = 7 output columns.
    let JoinPlan {
        source,
        columns,
        projections,
        ..
    } = bound_join("SELECT * FROM t a JOIN t b USING (id)")?;
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
    Ok(())
}

#[test]
fn using_merged_column_is_unqualified_while_sides_stay_addressable() -> anyhow::Result<()> {
    let JoinPlan { projections, .. } =
        bound_join("SELECT id, a.id, b.id FROM t a JOIN t b USING (id)")?;
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
    Ok(())
}

#[test]
fn using_full_join_merges_with_coalesce() -> anyhow::Result<()> {
    let JoinPlan { projections, .. } = bound_join("SELECT id FROM t a FULL JOIN t b USING (id)")?;
    // A full join's merged column is COALESCE(left, right), lowered to CASE.
    assert!(matches!(
        projections[0],
        BoundExpr::Case {
            ty: PgType::Int4,
            ..
        }
    ));
    Ok(())
}

#[test]
fn natural_join_equates_every_common_column() -> anyhow::Result<()> {
    let JoinPlan {
        source, columns, ..
    } = bound_join("SELECT * FROM t a NATURAL JOIN t b")?;
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
    Ok(())
}

#[test]
fn natural_join_without_common_columns_is_a_cross_product() -> anyhow::Result<()> {
    let JoinPlan {
        source, columns, ..
    } = bound_join("SELECT * FROM (VALUES (1)) a(x) NATURAL JOIN (VALUES (2)) b(y)")?;
    assert_eq!(columns.len(), 2);
    assert!(matches!(
        source,
        JoinExpr::Join {
            kind: JoinKind::Inner,
            predicate: None,
            ..
        }
    ));
    Ok(())
}

#[test]
fn using_column_missing_on_a_side_is_42703() -> anyhow::Result<()> {
    let right = bind_err("SELECT * FROM t a JOIN (VALUES (1)) b(x) USING (id)")?;
    assert_eq!(right.code, "42703");
    assert_eq!(
        right.message,
        "column \"id\" specified in USING clause does not exist in right table"
    );
    let left = bind_err("SELECT * FROM (VALUES (1)) a(x) JOIN t b USING (id)")?;
    assert_eq!(left.code, "42703");
    assert_eq!(
        left.message,
        "column \"id\" specified in USING clause does not exist in left table"
    );
    Ok(())
}

#[test]
fn using_join_in_a_later_comma_group_shifts_merged_indices() -> anyhow::Result<()> {
    // `t` (4 columns) is the first comma group, so the merged `id` and the
    // rest of the USING group live at combined-row offsets 4 and up.
    let JoinPlan {
        columns,
        projections,
        ..
    } = bound_join(
        "SELECT * FROM t, \
         (VALUES (5, 50)) a(id, x) JOIN (VALUES (5, 500)) b(id, y) USING (id)",
    )?;
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
    Ok(())
}

#[test]
fn duplicate_using_column_is_42701() -> anyhow::Result<()> {
    let e = bind_err("SELECT * FROM t a JOIN t b USING (id, id)")?;
    assert_eq!(e.code, "42701");
    assert_eq!(
        e.message,
        "column name \"id\" appears more than once in USING clause"
    );
    Ok(())
}

#[test]
fn using_merged_column_uses_common_type_not_comparison_type() -> anyhow::Result<()> {
    // real + int4: PG's select_common_type resolves the merged column to
    // real, even though the equality comparison promotes to float8.
    let JoinPlan { projections, .. } =
        bound_join("SELECT x FROM (VALUES (1.0::real)) a(x) JOIN (VALUES (1)) b(x) USING (x)")?;
    assert_eq!(projections[0].ty(), PgType::Float4);
    Ok(())
}

#[test]
fn using_column_ambiguous_on_a_side_is_42702() -> anyhow::Result<()> {
    let e = bind_err("SELECT * FROM (VALUES (1, 2)) a(x, x) JOIN (VALUES (1)) b(x) USING (x)")?;
    assert_eq!(e.code, "42702");
    assert_eq!(
        e.message,
        "common column name \"x\" appears more than once in left table"
    );
    Ok(())
}

#[test]
fn aggregate_accepts_join_input() -> anyhow::Result<()> {
    let LogicalPlan::Aggregate(AggregatePlan {
        input: AggInput::Join(source),
        aggregates,
        ..
    }) = bound("SELECT count(*) FROM t a LEFT JOIN t b ON a.id = b.id")?
    else {
        bail!("expected Aggregate over Join");
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
fn where_referencing_both_relations_binds() -> anyhow::Result<()> {
    let JoinPlan { predicate, .. } =
        bound_join("SELECT a.x FROM (VALUES (1)) a(x), (VALUES (1)) b(y) WHERE a.x = b.y")?;
    assert!(predicate.is_some());
    Ok(())
}

#[test]
fn duplicate_column_within_relation_is_ambiguous_42702() -> anyhow::Result<()> {
    // A duplicate column alias makes a reference ambiguous, as in PG —
    // whether unqualified or qualified into that relation.
    let e = bind_err("SELECT x FROM (VALUES (1, 2)) a(x, x)")?;
    assert_eq!(e.code, "42702");
    assert_eq!(e.message, "column reference \"x\" is ambiguous");
    let e = bind_err("SELECT a.x FROM (VALUES (1, 2)) a(x, x)")?;
    assert_eq!(e.code, "42702");
    assert_eq!(e.message, "column reference \"x\" is ambiguous");
    Ok(())
}

#[test]
fn qualified_missing_column_names_the_qualifier() -> anyhow::Result<()> {
    // PG prints `column q.c does not exist` for a qualified reference,
    // unquoted and qualifier-prefixed (contrast the unqualified form).
    let e = bind_err("SELECT x.nope FROM t x")?;
    assert_eq!(e.code, "42703");
    assert_eq!(e.message, "column x.nope does not exist");
    Ok(())
}

/// A parenthesised join is one join *input* to the chain around it, not a run of
/// relations spliced into it. The tree shape is what proves it: the right child
/// of the outer join is itself a `Join`, so a `LEFT JOIN (b JOIN c)` cannot
/// degenerate into `(a LEFT JOIN b) JOIN c` — which would drop the unmatched
/// left rows the outer join exists to keep.
#[test]
fn a_parenthesised_join_is_one_input_to_the_chain_around_it() -> anyhow::Result<()> {
    let JoinPlan {
        source,
        projections,
        ..
    } = bound_join(
        "SELECT c.z FROM (VALUES (1)) a(x) \
         LEFT JOIN ((VALUES (1)) b(y) JOIN (VALUES (1)) c(z) ON b.y = c.z) ON a.x = c.z",
    )?;
    let JoinExpr::Join {
        left,
        right,
        kind: JoinKind::Left,
        predicate: Some(predicate),
    } = source
    else {
        bail!("expected a top-level LEFT join");
    };
    assert!(matches!(*left, JoinExpr::Input { .. }));
    assert!(matches!(
        *right,
        JoinExpr::Join {
            kind: JoinKind::Inner,
            ..
        }
    ));
    // The *enclosing* predicate indexes the enclosing chain's combined row, so
    // `c.z` is at 2 there — the inner join's own predicate is base-0 to its own
    // subtree and is not touched by the nesting.
    assert!(predicate.any_node(&|e| matches!(e, BoundExpr::ColumnRef { index: 2, .. })));
    assert_eq!(
        projections[0],
        BoundExpr::ColumnRef {
            index: 2,
            ty: PgType::Int4
        }
    );
    Ok(())
}

/// The six-level nesting `pg_get_viewdef` prints for `information_schema.columns`
/// binds, and every relation in it stays addressable by its own alias.
#[test]
fn deeply_nested_parenthesised_joins_bind() -> anyhow::Result<()> {
    let JoinPlan { projections, .. } = bound_join(
        "SELECT a.w, b.x, c.y, d.z FROM ((((VALUES (1)) a(w) \
         LEFT JOIN (VALUES (1)) b(x) ON a.w = b.x) \
         JOIN ((VALUES (1)) c(y) JOIN (VALUES (1)) d(z) ON c.y = d.z) ON a.w = c.y))",
    )?;
    let indices: Vec<_> = projections
        .iter()
        .map(|e| match e {
            BoundExpr::ColumnRef { index, .. } => Ok(*index),
            other => Err(anyhow!("expected a column reference, got {other:?}")),
        })
        .collect::<anyhow::Result<_>>()?;
    assert_eq!(indices, vec![0, 1, 2, 3]);
    Ok(())
}

/// A qualifier is unique across the whole FROM clause, parentheses or not — so
/// the check has to see every relation a nested group contributes, not just its
/// first.
#[test]
fn a_duplicate_qualifier_inside_a_nested_group_is_42712() -> anyhow::Result<()> {
    let e = bind_err(
        "SELECT 1 FROM (VALUES (1)) a(x) \
         JOIN ((VALUES (1)) b(y) JOIN (VALUES (1)) a(z) ON true) ON true",
    )?;
    assert_eq!(e.code, "42712");
    assert_eq!(e.message, "table name \"a\" specified more than once");
    Ok(())
}

/// Nothing outside a parenthesised group is in the row its items are fed, so a
/// `LATERAL` reference out of one hits the same barrier a reference out of a
/// comma group hits — the enclosing join is spliced in above the whole subtree.
///
/// A deliberate divergence: PostgreSQL answers this query, because `a` does
/// precede the group in the FROM clause. Getting there needs the enclosing row
/// threaded into the subtree, so the honest answer is 0A000 rather than a
/// silent bind against a like-named enclosing relation.
#[test]
fn a_nested_group_cannot_see_the_chain_it_sits_in() -> anyhow::Result<()> {
    let e = bind_err(
        "SELECT 1 FROM (VALUES (1)) a(x) \
         JOIN ((VALUES (1)) b(y) JOIN LATERAL (SELECT a.x) c ON true) ON true",
    )?;
    assert_eq!(e.code, sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(
        e.message,
        "LATERAL reference to \"a\" from outside this join chain is not supported yet"
    );
    Ok(())
}
