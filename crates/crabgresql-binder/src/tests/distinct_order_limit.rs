//! DISTINCT, ORDER BY and LIMIT/OFFSET.

use super::common::*;

/// A single-table SELECT's projections and sort keys.
fn query_parts(sql: &str) -> anyhow::Result<(Vec<BoundExpr>, Vec<SortKey>)> {
    let QueryPlan {
        projections, sort, ..
    } = bound_query(sql)?;
    Ok((projections, sort))
}

fn distinct_of(sql: &str) -> anyhow::Result<(Vec<BoundExpr>, Option<Vec<DistinctKey>>)> {
    let QueryPlan {
        projections,
        distinct,
        ..
    } = bound_query(sql)?;
    Ok((projections, distinct))
}

#[test]
fn select_distinct_keys_every_visible_column() -> anyhow::Result<()> {
    // Plain DISTINCT deduplicates on all visible output columns, in order.
    let (projections, distinct) = distinct_of("SELECT id, name FROM t")?;
    assert!(distinct.is_none(), "no DISTINCT keyword → no distinct keys");
    let _ = projections;
    let (_, distinct) = distinct_of("SELECT DISTINCT id, name FROM t")?;
    assert_eq!(
        distinct,
        Some(vec![
            DistinctKey {
                column: 0,
                ty: PgType::Int4,
            },
            DistinctKey {
                column: 1,
                ty: PgType::Text,
            },
        ])
    );
    Ok(())
}

#[test]
fn select_all_keeps_duplicates() -> anyhow::Result<()> {
    // The explicit ALL default is not DISTINCT.
    let (_, distinct) = distinct_of("SELECT ALL id FROM t")?;
    assert!(distinct.is_none());
    Ok(())
}

#[test]
fn distinct_on_resolves_expressions_to_columns() -> anyhow::Result<()> {
    // DISTINCT ON (id): id is a select-list column, so the key reuses it.
    let (projections, distinct) =
        distinct_of("SELECT DISTINCT ON (id) id, name FROM t ORDER BY id, name")?;
    assert_eq!(projections.len(), 2, "ON key reuses the visible column");
    assert_eq!(
        distinct,
        Some(vec![DistinctKey {
            column: 0,
            ty: PgType::Int4,
        }])
    );
    Ok(())
}

#[test]
fn distinct_on_hidden_expression_appends_column() -> anyhow::Result<()> {
    // DISTINCT ON (big) where big is not selected: it becomes a hidden
    // column, and ORDER BY big reuses that same hidden column (prefix match).
    let (projections, distinct) =
        distinct_of("SELECT DISTINCT ON (big) id FROM t ORDER BY big, id")?;
    assert_eq!(projections.len(), 2, "one hidden column for the ON expr");
    assert_eq!(
        distinct,
        Some(vec![DistinctKey {
            column: 1,
            ty: PgType::Int8,
        }])
    );
    Ok(())
}

#[test]
fn select_distinct_order_by_not_in_select_list_is_42p10() -> anyhow::Result<()> {
    // PG requires DISTINCT's ORDER BY keys to be select-list columns.
    let e = bind_err("SELECT DISTINCT id FROM t ORDER BY big")?;
    assert_eq!(e.code, "42P10");
    assert_eq!(
        e.message,
        "for SELECT DISTINCT, ORDER BY expressions must appear in select list"
    );
    Ok(())
}

#[test]
fn distinct_on_not_matching_order_by_is_42p10() -> anyhow::Result<()> {
    // DISTINCT ON expressions must be a prefix of ORDER BY.
    let e = bind_err("SELECT DISTINCT ON (id) id FROM t ORDER BY name")?;
    assert_eq!(e.code, "42P10");
    assert_eq!(
        e.message,
        "SELECT DISTINCT ON expressions must match initial ORDER BY expressions"
    );
    Ok(())
}

#[test]
fn distinct_on_matches_reordered_order_by_prefix() -> anyhow::Result<()> {
    // The ON expressions are the *set* of leading ORDER BY expressions;
    // their order relative to each other does not matter (PG accepts this).
    let (_, distinct) =
        distinct_of("SELECT DISTINCT ON (id, big) id, big FROM t ORDER BY big, id")?;
    assert_eq!(
        distinct,
        Some(vec![
            DistinctKey {
                column: 0,
                ty: PgType::Int4,
            },
            DistinctKey {
                column: 1,
                ty: PgType::Int8,
            },
        ])
    );
    // Extra trailing ORDER BY keys (per-group tiebreak) are still allowed.
    let (_, distinct) = distinct_of("SELECT DISTINCT ON (id) id, name FROM t ORDER BY id, name")?;
    assert_eq!(
        distinct,
        Some(vec![DistinctKey {
            column: 0,
            ty: PgType::Int4,
        }])
    );
    Ok(())
}

#[test]
fn distinct_on_more_expressions_than_order_by_is_42p10() -> anyhow::Result<()> {
    // ON has two expressions but ORDER BY only covers one — not a match.
    let e = bind_err("SELECT DISTINCT ON (id, big) id, big FROM t ORDER BY id")?;
    assert_eq!(e.code, "42P10");
    assert_eq!(
        e.message,
        "SELECT DISTINCT ON expressions must match initial ORDER BY expressions"
    );
    Ok(())
}

#[test]
fn order_by_ordinal_carries_type_and_direction() -> anyhow::Result<()> {
    // `ORDER BY 2 DESC` → second output column (id, int4), descending, and
    // the PG default NULLS FIRST for a descending sort.
    let (projections, sort) = query_parts("SELECT name, id FROM t ORDER BY 2 DESC")?;
    assert_eq!(projections.len(), 2, "no hidden column for an ordinal");
    assert_eq!(
        sort,
        vec![SortKey {
            column: 1,
            ty: PgType::Int4,
            collation: DEFAULT_COLLATION_OID,
            asc: false,
            nulls_first: true,
        }]
    );
    Ok(())
}

#[test]
fn order_by_output_name_resolves_to_visible_column() -> anyhow::Result<()> {
    // A bare name matches a select-list output name first (SQL92).
    let (projections, sort) = query_parts("SELECT name, id FROM t ORDER BY name")?;
    assert_eq!(projections.len(), 2);
    assert_eq!(
        sort,
        vec![SortKey {
            column: 0,
            ty: PgType::Text,
            collation: DEFAULT_COLLATION_OID,
            asc: true,
            nulls_first: false,
        }]
    );
    Ok(())
}

#[test]
fn order_by_alias_resolves_to_its_column() -> anyhow::Result<()> {
    let (projections, sort) = query_parts("SELECT id + big AS s FROM t ORDER BY s")?;
    assert_eq!(projections.len(), 1);
    assert_eq!(sort[0].column, 0);
    assert_eq!(sort[0].ty, PgType::Int8);
    Ok(())
}

#[test]
fn order_by_nonselected_column_appends_hidden() -> anyhow::Result<()> {
    // `big` is not in the select list, so it becomes a hidden column past
    // the single visible output. Its type drives comparison.
    let (projections, sort) = query_parts("SELECT id FROM t ORDER BY big")?;
    assert_eq!(projections.len(), 2, "one hidden column appended");
    assert_eq!(
        projections[1],
        BoundExpr::ColumnRef {
            index: 1,
            ty: PgType::Int8
        }
    );
    assert_eq!(sort[0].column, 1);
    assert_eq!(sort[0].ty, PgType::Int8);
    Ok(())
}

#[test]
fn order_by_expression_reuses_equal_projection() -> anyhow::Result<()> {
    // `ORDER BY id + big` equals the sole projection `id + big`, so it is
    // reused rather than appended (PG's target-entry reuse).
    let (projections, sort) = query_parts("SELECT id + big AS s FROM t ORDER BY id + big")?;
    assert_eq!(projections.len(), 1, "reused, not appended");
    assert_eq!(sort[0].column, 0);
    Ok(())
}

#[test]
fn order_by_qualified_name_binds_as_expression() -> anyhow::Result<()> {
    // A qualified name skips the output-name match and binds against the
    // FROM scope, appending a hidden column when not selected.
    let (projections, sort) = query_parts("SELECT id FROM t ORDER BY t.name")?;
    assert_eq!(projections.len(), 2);
    assert_eq!(sort[0].column, 1);
    assert_eq!(sort[0].ty, PgType::Text);
    Ok(())
}

#[test]
fn order_by_ambiguous_output_name_is_42702() -> anyhow::Result<()> {
    let e = bind_err("SELECT id AS c, big AS c FROM t ORDER BY c")?;
    assert_eq!(e.code, "42702");
    assert_eq!(e.message, "ORDER BY \"c\" is ambiguous");
    Ok(())
}

#[test]
fn order_by_alias_not_visible_inside_expression() -> anyhow::Result<()> {
    // A top-level bare alias resolves, but inside an expression the alias is
    // invisible — PG reports the underlying column as undefined.
    let e = bind_err("SELECT 1 AS a ORDER BY a + 1")?;
    assert_eq!(e.code, "42703");
    Ok(())
}

#[test]
fn order_by_upper_of_column_binds() -> anyhow::Result<()> {
    let (projections, sort) = query_parts("SELECT id FROM t ORDER BY upper(name)")?;
    assert_eq!(projections.len(), 2);
    assert_eq!(sort[0].column, 1);
    assert_eq!(sort[0].ty, PgType::Text);
    Ok(())
}

#[test]
fn values_order_by_column_name_resolves() -> anyhow::Result<()> {
    let ValuesPlan { sort, .. } = bound_values("VALUES (3), (1) ORDER BY column1")?;
    assert_eq!(sort[0].column, 0);
    assert_eq!(sort[0].ty, PgType::Int4);
    Ok(())
}

#[test]
fn values_order_by_expression_stays_0a000() -> anyhow::Result<()> {
    // A standalone VALUES list has no projection tuple to hang a hidden
    // column on.
    // TODO: bind expression sort keys over a standalone VALUES list — PG
    // answers `VALUES (3), (1) ORDER BY column1 + 1` with 1, 3.
    let e = bind_err("VALUES (3), (1) ORDER BY column1 + 1")?;
    assert_eq!(e.code, "0A000");
    Ok(())
}

#[test]
fn limit_offset_wraps_body() -> anyhow::Result<()> {
    let LimitPlan {
        source,
        limit,
        offset,
    } = bound_limit("SELECT id FROM t LIMIT 5 OFFSET 2")?;
    assert_eq!(limit, Some(5));
    assert_eq!(offset, Some(2));
    assert!(matches!(*source, LogicalPlan::Query(QueryPlan { .. })));
    Ok(())
}

#[test]
fn offset_zero_is_a_bare_offset() -> anyhow::Result<()> {
    // The float4/float8 optimization-fence shape: `OFFSET 0`, no LIMIT.
    let LimitPlan { limit, offset, .. } = bound_limit("SELECT id FROM t OFFSET 0")?;
    assert_eq!(limit, None);
    assert_eq!(offset, Some(0));
    Ok(())
}

#[test]
fn limit_all_is_no_bound() -> anyhow::Result<()> {
    // `LIMIT ALL OFFSET 3` carries only the offset; the limit is unbounded.
    let LimitPlan { limit, offset, .. } = bound_limit("SELECT id FROM t LIMIT ALL OFFSET 3")?;
    assert_eq!(limit, None);
    assert_eq!(offset, Some(3));
    Ok(())
}

#[test]
fn offset_in_derived_table_wraps_subplan() -> anyhow::Result<()> {
    // `OFFSET 0` inside a FROM subquery binds as a Limit at that level.
    let SubqueryPlan { source, .. } =
        bound_subquery("SELECT * FROM (SELECT id FROM t OFFSET 0) s")?;
    assert!(matches!(*source, LogicalPlan::Limit(LimitPlan { .. })));
    Ok(())
}

#[test]
fn negative_limit_and_offset_rejected() -> anyhow::Result<()> {
    assert_eq!(bind_err("SELECT id FROM t LIMIT -1")?.code, "2201W");
    assert_eq!(bind_err("SELECT id FROM t OFFSET -1")?.code, "2201X");
    Ok(())
}

#[test]
fn non_constant_limit_stays_0a000() -> anyhow::Result<()> {
    let e = bind_err("SELECT id FROM t LIMIT id")?;
    assert_eq!(e.code, "0A000");
    Ok(())
}
