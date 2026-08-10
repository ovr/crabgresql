//! UNION and UNION ALL; INTERSECT/EXCEPT rejection.

use super::common::*;

/// The pieces of a bound set operation.
fn setop_of(
    sql: &str,
) -> anyhow::Result<(
    Vec<SetOpArm>,
    Vec<OutputColumn>,
    Vec<SortKey>,
    Option<Vec<DistinctKey>>,
)> {
    let SetOpPlan {
        arms,
        columns,
        sort,
        distinct,
    } = bound_set_op(sql)?;
    Ok((arms, columns, sort, distinct))
}

#[test]
fn union_all_binds_to_a_flat_setop() -> anyhow::Result<()> {
    // Same-typed arms, no ORDER BY: a bare concat, arms untouched.
    let (arms, columns, sort, distinct) = setop_of("SELECT id FROM t UNION ALL SELECT id FROM t")?;
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0].name, "id");
    assert_eq!(columns[0].ty, PgType::Int4);
    assert!(sort.is_empty());
    assert!(distinct.is_none(), "UNION ALL keeps duplicates");
    assert_eq!(arms.len(), 2);
    for arm in &arms {
        assert_eq!(plan_name(&arm.plan), "Query");
        assert!(arm.coercion.is_none(), "same-typed arms need no coercion");
    }
    Ok(())
}

#[test]
fn union_deduplicates_on_every_output_column() -> anyhow::Result<()> {
    let (_, _, sort, distinct) = setop_of("SELECT id FROM t UNION SELECT id FROM t")?;
    assert!(sort.is_empty());
    assert_eq!(
        distinct.context("UNION should deduplicate")?,
        vec![DistinctKey {
            column: 0,
            ty: PgType::Int4,
        }]
    );
    Ok(())
}

#[test]
fn union_unifies_arm_types_and_coerces() -> anyhow::Result<()> {
    // int4 + int8 unify to int8; only the int4 arm needs coercing, and the
    // result column keeps the first arm's name.
    let (arms, columns, ..) = setop_of("SELECT id FROM t UNION ALL SELECT big FROM t")?;
    assert_eq!(columns[0].name, "id");
    assert_eq!(columns[0].ty, PgType::Int8);
    assert!(arms[0].coercion.is_some(), "int4 arm is coerced to int8");
    assert!(arms[1].coercion.is_none(), "int8 arm already matches");
    Ok(())
}

#[test]
fn union_column_count_mismatch_is_42601() -> anyhow::Result<()> {
    let err = bind_err("SELECT id FROM t UNION SELECT id, big FROM t")?;
    assert_eq!(err.code, sqlstate::SYNTAX_ERROR);
    assert_eq!(
        err.message,
        "each UNION query must have the same number of columns"
    );
    Ok(())
}

#[test]
fn union_incompatible_types_is_42804() -> anyhow::Result<()> {
    let err = bind_err("SELECT id FROM t UNION SELECT flag FROM t")?;
    assert_eq!(err.code, sqlstate::DATATYPE_MISMATCH);
    assert_eq!(
        err.message,
        "UNION types integer and boolean cannot be matched"
    );
    Ok(())
}

#[test]
fn union_all_order_by_ordinal_sorts_without_dedup() -> anyhow::Result<()> {
    let (_, _, sort, distinct) =
        setop_of("SELECT id FROM t UNION ALL SELECT id FROM t ORDER BY 1")?;
    assert!(distinct.is_none(), "UNION ALL keeps duplicates");
    assert_eq!(sort.len(), 1);
    assert_eq!(sort[0].column, 0);
    Ok(())
}

#[test]
fn equivalent_union_chain_flattens_into_one_node() -> anyhow::Result<()> {
    // `a UNION b UNION c` is one three-armed node with a single dedup, not
    // nested pairs that would deduplicate at every level.
    let (arms, _, sort, distinct) =
        setop_of("SELECT id FROM t UNION SELECT id FROM t UNION SELECT id FROM t ORDER BY 1")?;
    assert_eq!(arms.len(), 3);
    assert!(arms.iter().all(|a| plan_name(&a.plan) == "Query"));
    assert!(distinct.is_some());
    assert_eq!(sort.len(), 1);
    Ok(())
}

#[test]
fn union_all_over_a_distinct_arm_keeps_the_inner_dedup() -> anyhow::Result<()> {
    // Flattening must not absorb a DISTINCT child into an ALL parent: the
    // inner deduplication happens first and would otherwise be lost.
    let (arms, _, _, distinct) =
        setop_of("(SELECT id FROM t UNION SELECT id FROM t) UNION ALL SELECT id FROM t")?;
    assert!(distinct.is_none(), "the outer UNION ALL keeps duplicates");
    assert_eq!(arms.len(), 2);
    assert_eq!(
        plan_name(&arms[0].plan),
        "SetOp",
        "the inner UNION stays its own node"
    );
    Ok(())
}

#[test]
fn a_long_union_chain_binds_without_deep_nesting() -> anyhow::Result<()> {
    // A flat chain must not nest one level per arm: that recursed deeply
    // enough through bind/plan/execute to abort the process.
    let mut sql = String::from("SELECT id FROM t");
    for _ in 0..500 {
        sql.push_str(" UNION ALL SELECT id FROM t");
    }
    let (arms, ..) = setop_of(&sql)?;
    assert_eq!(arms.len(), 501);
    Ok(())
}

#[test]
fn deeply_nested_set_ops_are_rejected_rather_than_crashing() -> anyhow::Result<()> {
    let mut sql = String::from("SELECT id FROM t");
    for _ in 0..MAX_SET_OP_NESTING + 5 {
        // Alternating quantifiers defeat flattening, forcing real nesting.
        sql = format!("({sql} UNION SELECT id FROM t) UNION ALL SELECT id FROM t");
    }
    // The contract is a clean error rather than an aborted process. The
    // parser guards its own recursion at the same depth, so it reaches this
    // shape first and is what reports here; the binder's limit stays as a
    // backstop for callers that raise the parser's.
    let err = crabgresql_parser::parse(&sql).expect_err("should be rejected");
    assert!(
        err.to_string().contains("recursion limit exceeded"),
        "expected a depth error, got: {err}"
    );
    Ok(())
}

#[test]
fn a_null_arm_takes_its_type_from_the_other_arms() -> anyhow::Result<()> {
    // PG resolves an unknown-typed set-operation column from the other arms,
    // so the NULL-padding idiom keeps the real column type.
    let (arms, columns, ..) = setop_of("SELECT id FROM t UNION ALL SELECT NULL")?;
    assert_eq!(columns[0].ty, PgType::Int4, "NULL must not force text");
    assert!(
        arms[1].coercion.is_some(),
        "the NULL arm is re-typed to the resolved column type"
    );
    Ok(())
}

#[test]
fn an_all_null_column_falls_back_to_text() -> anyhow::Result<()> {
    let (_, columns, ..) = setop_of("SELECT NULL UNION ALL SELECT NULL")?;
    assert_eq!(columns[0].ty, PgType::Text);
    Ok(())
}

#[test]
fn union_order_by_unknown_column_is_42703() -> anyhow::Result<()> {
    let err = bind_err("SELECT id FROM t UNION SELECT id FROM t ORDER BY nosuch")?;
    assert_eq!(err.code, sqlstate::UNDEFINED_COLUMN);
    assert_eq!(err.message, "column \"nosuch\" does not exist");
    Ok(())
}

#[test]
fn union_order_by_expression_is_42p10() -> anyhow::Result<()> {
    let err = bind_err("SELECT id FROM t UNION SELECT id FROM t ORDER BY id + 1")?;
    assert_eq!(err.code, sqlstate::INVALID_COLUMN_REFERENCE);
    assert_eq!(
        err.message,
        "invalid UNION/INTERSECT/EXCEPT ORDER BY clause"
    );
    assert!(err.hint.is_some(), "PG hints at result column names");
    Ok(())
}

#[test]
fn union_on_a_type_without_equality_is_42883() -> anyhow::Result<()> {
    let err = bind_err("SELECT '{}'::json UNION SELECT '{}'::json")?;
    assert_eq!(err.code, sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(
        err.message,
        "could not identify an equality operator for type json"
    );
    Ok(())
}

#[test]
fn intersect_and_except_are_still_unsupported() -> anyhow::Result<()> {
    assert_eq!(
        bind_err("SELECT id FROM t INTERSECT SELECT id FROM t")?.code,
        sqlstate::FEATURE_NOT_SUPPORTED
    );
    assert_eq!(
        bind_err("SELECT id FROM t EXCEPT SELECT id FROM t")?.code,
        sqlstate::FEATURE_NOT_SUPPORTED
    );
    Ok(())
}
