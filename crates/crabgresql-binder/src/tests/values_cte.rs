//! `VALUES`, derived tables and CTEs.

use super::common::*;

#[test]
fn standalone_values_binds_to_values_plan() -> anyhow::Result<()> {
    let ValuesPlan { columns, rows, .. } = bound_values("VALUES (1, 'a'), (2, 'b')")?;
    assert_eq!(columns.len(), 2);
    assert_eq!(columns[0].name, "column1");
    assert_eq!(columns[1].name, "column2");
    assert_eq!(rows.len(), 2);
    Ok(())
}

#[test]
fn values_uneven_row_lengths_error() -> anyhow::Result<()> {
    let e = bind_err("VALUES (1), (2, 3)")?;
    assert_eq!(e.code, "42601");
    Ok(())
}

#[test]
fn values_common_type_keeps_real_over_int() -> anyhow::Result<()> {
    // PG's select_common_type resolves (real, int4) to real, not float8
    // (int4 implicitly casts to real). Contrast with operator resolution.
    let ValuesPlan { columns, .. } = bound_values("VALUES (CAST(1.5 AS real)), (2)")?;
    assert_eq!(columns[0].ty, PgType::Float4);
    Ok(())
}

#[test]
fn derived_table_binds_to_subquery_plan() -> anyhow::Result<()> {
    let SubqueryPlan { columns, .. } = bound_subquery("SELECT x FROM (VALUES (1), (2)) v(x)")?;
    assert_eq!(columns[0].name, "x");
    Ok(())
}

#[test]
fn derived_table_requires_alias() -> anyhow::Result<()> {
    let e = bind_err("SELECT * FROM (VALUES (1))")?;
    assert_eq!(e.code, "42601");
    assert_eq!(e.message, "subquery in FROM must have an alias");
    Ok(())
}

#[test]
fn cte_reference_resolves_to_subquery() -> anyhow::Result<()> {
    let SubqueryPlan { columns, .. } = bound_subquery("WITH t(x) AS (VALUES (1)) SELECT x FROM t")?;
    assert_eq!(columns[0].name, "x");
    Ok(())
}

#[test]
fn cte_column_count_mismatch_errors() -> anyhow::Result<()> {
    let e = bind_err("WITH t(a, b) AS (VALUES (1)) SELECT * FROM t")?;
    assert_eq!(e.code, "42P10");
    assert_eq!(
        e.message,
        "WITH query \"t\" has 1 columns available but 2 columns specified"
    );
    Ok(())
}

#[test]
fn derived_table_column_count_mismatch_errors() -> anyhow::Result<()> {
    let e = bind_err("SELECT * FROM (VALUES (1)) v(a, b)")?;
    assert_eq!(e.code, "42P10");
    assert_eq!(
        e.message,
        "table \"v\" has 1 columns available but 2 columns specified"
    );
    Ok(())
}

#[test]
fn duplicate_cte_name_is_42712() -> anyhow::Result<()> {
    let e = bind_err("WITH t AS (VALUES (1)), t AS (VALUES (2)) SELECT * FROM t")?;
    assert_eq!(e.code, "42712");
    assert_eq!(e.message, "WITH query name \"t\" specified more than once");
    Ok(())
}

#[test]
fn with_on_insert_source_binds_as_a_query() -> anyhow::Result<()> {
    // The WITH belongs to the source query and is honored via the query
    // binder (the CTE here is unused; the VALUES still supplies the row).
    let LogicalPlan::Insert(InsertPlan {
        source: InsertSource::Query { .. },
        ..
    }) = bound("INSERT INTO t (id) WITH c AS (SELECT 1) VALUES (10)")?
    else {
        bail!("expected a query-source Insert");
    };
    Ok(())
}

#[test]
fn with_recursive_is_rejected() -> anyhow::Result<()> {
    let e = bind_err("WITH RECURSIVE t(n) AS (VALUES (1)) SELECT n FROM t")?;
    assert_eq!(e.code, "0A000");
    assert_eq!(e.message, "WITH RECURSIVE is not supported yet");
    Ok(())
}

#[test]
fn cte_shadows_a_real_table() -> anyhow::Result<()> {
    // `t` here is the CTE, not the base table `t`; its column is `x`.
    let SubqueryPlan { columns, .. } = bound_subquery("WITH t(x) AS (VALUES (1)) SELECT x FROM t")?;
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0].name, "x");
    Ok(())
}
