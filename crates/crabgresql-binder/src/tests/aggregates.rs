//! Aggregate calls, GROUP BY and HAVING.

use super::common::*;

#[test]
fn count_star_becomes_a_single_aggregate() -> anyhow::Result<()> {
    let (group_exprs, aggregates, projections, having) = agg_of("SELECT count(*) FROM t")?;
    assert!(group_exprs.is_empty());
    assert!(having.is_none());
    assert_eq!(aggregates.len(), 1);
    assert_eq!(aggregates[0].func, crate::AggFn::Count);
    assert!(aggregates[0].args.is_empty());
    assert_eq!(aggregates[0].ret, PgType::Int8);
    // The projection reads the single aggregate slot.
    assert_eq!(
        projections,
        vec![BoundExpr::ColumnRef {
            index: 0,
            ty: PgType::Int8
        }]
    );
    Ok(())
}

#[test]
fn min_and_max_extract_two_aggregates() -> anyhow::Result<()> {
    let (_g, aggregates, projections, _h) = agg_of("SELECT min(id), max(id) FROM t")?;
    assert_eq!(aggregates.len(), 2);
    assert_eq!(aggregates[0].func, crate::AggFn::Min);
    assert_eq!(aggregates[1].func, crate::AggFn::Max);
    // MIN/MAX keep the argument type.
    assert_eq!(aggregates[0].ret, PgType::Int4);
    assert_eq!(
        projections,
        vec![
            BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4
            },
            BoundExpr::ColumnRef {
                index: 1,
                ty: PgType::Int4
            },
        ]
    );
    Ok(())
}

#[test]
fn distinct_and_all_aggregate_treatments_are_preserved() -> anyhow::Result<()> {
    let (_g, aggregates, _p, _h) =
        agg_of("SELECT count(DISTINCT id), sum(ALL id), avg(id) FROM t")?;
    assert_eq!(aggregates.len(), 3);
    assert!(aggregates[0].distinct);
    assert!(!aggregates[1].distinct);
    assert!(!aggregates[2].distinct);
    Ok(())
}

#[test]
fn duplicate_treatment_with_wildcard_is_a_syntax_error() -> anyhow::Result<()> {
    for sql in [
        "SELECT count(DISTINCT *) FROM t",
        "SELECT count(ALL *) FROM t",
    ] {
        let err = bind_err(sql)?;
        assert_eq!(err.code, sqlstate::SYNTAX_ERROR);
        assert_eq!(err.message, "syntax error at or near \"*\"");
    }
    Ok(())
}

#[test]
fn expression_over_aggregates_rewrites_each_call() -> anyhow::Result<()> {
    let (_g, aggregates, projections, _h) = agg_of("SELECT max(id) - min(id) FROM t")?;
    assert_eq!(aggregates.len(), 2);
    let BoundExpr::Binary {
        op: BinOp::Sub,
        left,
        right,
        ..
    } = &projections[0]
    else {
        bail!("expected a subtraction over the two aggregate columns");
    };
    assert_eq!(
        **left,
        BoundExpr::ColumnRef {
            index: 0,
            ty: PgType::Int4
        }
    );
    assert_eq!(
        **right,
        BoundExpr::ColumnRef {
            index: 1,
            ty: PgType::Int4
        }
    );
    Ok(())
}

#[test]
fn constant_mixed_with_aggregate_is_kept() -> anyhow::Result<()> {
    let (_g, aggregates, projections, _h) = agg_of("SELECT 'x', count(*) FROM t")?;
    assert_eq!(aggregates.len(), 1);
    assert!(matches!(projections[0], BoundExpr::Const { .. }));
    assert_eq!(
        projections[1],
        BoundExpr::ColumnRef {
            index: 0,
            ty: PgType::Int8
        }
    );
    Ok(())
}

#[test]
fn group_by_puts_keys_before_aggregates() -> anyhow::Result<()> {
    let (group_exprs, aggregates, projections, _h) =
        agg_of("SELECT id, count(*) FROM t GROUP BY id")?;
    assert_eq!(
        group_exprs,
        vec![BoundExpr::ColumnRef {
            index: 0,
            ty: PgType::Int4
        }]
    );
    assert_eq!(aggregates.len(), 1);
    // Group key is slot 0; the aggregate is slot 1 (after the keys).
    assert_eq!(
        projections,
        vec![
            BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4
            },
            BoundExpr::ColumnRef {
                index: 1,
                ty: PgType::Int8
            },
        ]
    );
    Ok(())
}

#[test]
fn group_by_ordinal_references_select_expression() -> anyhow::Result<()> {
    // GROUP BY 1 groups by the first select expression (id), not the literal 1.
    let (group_exprs, _a, _p, _h) = agg_of("SELECT id, count(*) FROM t GROUP BY 1")?;
    assert_eq!(
        group_exprs,
        vec![BoundExpr::ColumnRef {
            index: 0,
            ty: PgType::Int4
        }]
    );
    Ok(())
}

#[test]
fn grouped_compound_expression_is_allowed() -> anyhow::Result<()> {
    // `id + 1` is legal because its column is a group key.
    let (_g, _a, projections, _h) = agg_of("SELECT id + 1 FROM t GROUP BY id")?;
    assert!(matches!(
        projections[0],
        BoundExpr::Binary { op: BinOp::Add, .. }
    ));
    Ok(())
}

#[test]
fn having_forces_aggregation_and_is_rewritten() -> anyhow::Result<()> {
    let (_g, aggregates, _p, having) = agg_of("SELECT id FROM t GROUP BY id HAVING count(*) > 1")?;
    assert_eq!(aggregates.len(), 1);
    // HAVING references the aggregate slot (after the one group key).
    let BoundExpr::Binary { left, .. } = having.context("HAVING present")? else {
        bail!("expected a comparison in HAVING");
    };
    assert_eq!(
        *left,
        BoundExpr::ColumnRef {
            index: 1,
            ty: PgType::Int8
        }
    );
    Ok(())
}

#[test]
fn sum_and_avg_return_types() -> anyhow::Result<()> {
    assert_eq!(agg_of("SELECT sum(id) FROM t")?.1[0].ret, PgType::Int8);
    assert_eq!(agg_of("SELECT sum(big) FROM t")?.1[0].ret, PgType::Numeric);
    assert_eq!(agg_of("SELECT avg(id) FROM t")?.1[0].ret, PgType::Numeric);
    assert_eq!(agg_of("SELECT avg(big) FROM t")?.1[0].ret, PgType::Numeric);
    Ok(())
}

#[test]
fn order_by_aggregate_binds_without_error() -> anyhow::Result<()> {
    // ORDER BY count(*) appends a hidden aggregate column; it must bind.
    let (_g, aggregates, _p, _h) = agg_of("SELECT id FROM t GROUP BY id ORDER BY count(*)")?;
    assert_eq!(aggregates.len(), 1);
    Ok(())
}

#[test]
fn ungrouped_column_is_a_grouping_error() -> anyhow::Result<()> {
    assert_eq!(
        bind_err("SELECT id, count(*) FROM t")?.code,
        sqlstate::GROUPING_ERROR
    );
    assert_eq!(
        bind_err("SELECT id FROM t GROUP BY big")?.code,
        sqlstate::GROUPING_ERROR
    );
    Ok(())
}

#[test]
fn aggregate_in_where_is_rejected() -> anyhow::Result<()> {
    assert_eq!(
        bind_err("SELECT count(*) FROM t WHERE count(*) > 1")?.code,
        sqlstate::GROUPING_ERROR
    );
    Ok(())
}

#[test]
fn nested_aggregate_is_rejected() -> anyhow::Result<()> {
    assert_eq!(
        bind_err("SELECT max(min(id)) FROM t")?.code,
        sqlstate::GROUPING_ERROR
    );
    Ok(())
}

#[test]
fn aggregate_in_group_by_is_rejected() -> anyhow::Result<()> {
    assert_eq!(
        bind_err("SELECT count(*) FROM t GROUP BY count(*)")?.code,
        sqlstate::GROUPING_ERROR
    );
    Ok(())
}

#[test]
fn unsupported_aggregate_argument_is_undefined_function() -> anyhow::Result<()> {
    assert_eq!(
        bind_err("SELECT sum(name) FROM t")?.code,
        sqlstate::UNDEFINED_FUNCTION
    );
    assert_eq!(
        bind_err("SELECT avg(name) FROM t")?.code,
        sqlstate::UNDEFINED_FUNCTION
    );
    Ok(())
}

#[test]
fn group_by_ordinal_out_of_range_is_rejected() -> anyhow::Result<()> {
    assert_eq!(
        bind_err("SELECT id, count(*) FROM t GROUP BY 5")?.code,
        sqlstate::INVALID_COLUMN_REFERENCE
    );
    Ok(())
}

#[test]
fn min_max_reject_boolean() -> anyhow::Result<()> {
    // PG has no min/max(boolean) even though bool is orderable for ORDER BY.
    assert_eq!(
        bind_err("SELECT max(flag) FROM t")?.code,
        sqlstate::UNDEFINED_FUNCTION
    );
    assert_eq!(
        bind_err("SELECT min(flag) FROM t")?.code,
        sqlstate::UNDEFINED_FUNCTION
    );
    Ok(())
}

#[test]
fn group_by_resolves_output_alias() -> anyhow::Result<()> {
    // `z` is not an input column; it resolves to the select-list alias for id.
    let (group_exprs, _a, _p, _h) = agg_of("SELECT id AS z, count(*) FROM t GROUP BY z")?;
    assert_eq!(
        group_exprs,
        vec![BoundExpr::ColumnRef {
            index: 0,
            ty: PgType::Int4
        }]
    );
    Ok(())
}

#[test]
fn parameterless_count_has_pg_message() -> anyhow::Result<()> {
    let e = bind_err("SELECT count()")?;
    assert_eq!(e.code, sqlstate::WRONG_OBJECT_TYPE);
    assert_eq!(
        e.message,
        "count(*) must be used to call a parameterless aggregate function"
    );
    Ok(())
}

#[test]
fn wrong_arity_aggregate_names_argument_types() -> anyhow::Result<()> {
    let e = bind_err("SELECT min(id, big) FROM t")?;
    assert_eq!(e.code, sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(e.message, "function min(integer, bigint) does not exist");
    Ok(())
}

#[test]
fn wildcard_non_count_aggregate_is_undefined() -> anyhow::Result<()> {
    let e = bind_err("SELECT sum(*) FROM t")?;
    assert_eq!(e.code, sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(e.message, "function sum() does not exist");
    Ok(())
}

#[test]
fn aggregate_order_by_becomes_sort_keys_on_the_aggregate() -> anyhow::Result<()> {
    let (_g, aggregates, _p, _h) =
        agg_of("SELECT array_agg(id ORDER BY name DESC NULLS LAST) FROM t")?;
    assert_eq!(aggregates.len(), 1);
    let key = &aggregates[0].order_by[0];
    assert!(matches!(key.expr, BoundExpr::ColumnRef { .. }));
    assert!(!key.asc);
    assert!(!key.nulls_first);
    Ok(())
}

/// PostgreSQL sorts a DISTINCT aggregate's input to dedup it, so a second
/// ordering is only accepted where the two coincide.
#[test]
fn distinct_aggregate_order_by_must_name_an_argument() -> anyhow::Result<()> {
    let e = bind_err("SELECT array_agg(DISTINCT id ORDER BY name) FROM t")?;
    assert_eq!(e.code, sqlstate::INVALID_COLUMN_REFERENCE);
    assert_eq!(
        e.message,
        "in an aggregate with DISTINCT, ORDER BY expressions must appear in argument list"
    );
    assert_eq!(
        agg_of("SELECT array_agg(DISTINCT id ORDER BY id) FROM t")?
            .1
            .len(),
        1
    );
    Ok(())
}

#[test]
fn aggregate_order_by_rejects_a_nested_aggregate_or_window() -> anyhow::Result<()> {
    assert_eq!(
        bind_err("SELECT array_agg(id ORDER BY sum(id)) FROM t")?.message,
        "aggregate function calls cannot be nested"
    );
    assert_eq!(
        bind_err("SELECT array_agg(id ORDER BY rank() OVER ()) FROM t")?.message,
        "aggregate function calls cannot contain window function calls"
    );
    Ok(())
}
