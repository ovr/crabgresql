//! Set-returning functions in FROM and in the target list.

use super::common::*;

#[test]
fn select_where_without_table_binds_predicate() {
    let ValuesPlan {
        rows, predicate, ..
    } = bound_values("SELECT 1 WHERE 1 = 2");
    assert_eq!(rows.len(), 1);
    assert!(predicate.is_some());
}

#[test]
fn set_returning_function_in_from_binds_columns() {
    let TableFunctionPlan { func, columns, .. } =
        bound_table_function("SELECT * FROM pg_input_error_info('1e400', 'float4')");
    assert_eq!(func, crate::TableFn::PgInputErrorInfo);
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["message", "detail", "hint", "sql_error_code"]);
    assert!(columns.iter().all(|c| c.ty == PgType::Text));
}

#[test]
fn set_returning_function_projects_and_filters() {
    // A subset projection over the SRF's columns resolves like a table.
    let TableFunctionPlan {
        columns, predicate, ..
    } = bound_table_function(
        "SELECT sql_error_code FROM pg_input_error_info('1e400', 'float4') \
         WHERE message IS NOT NULL",
    );
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0].name, "sql_error_code");
    assert!(predicate.is_some());
}

#[test]
fn unknown_set_returning_function_is_42883() {
    let e = bind_err("SELECT * FROM no_such_srf('x')");
    assert_eq!(e.code, "42883");
    assert_eq!(e.message, "function no_such_srf(unknown) does not exist");
}

#[test]
fn generate_series_in_from_binds_int4_column() {
    let TableFunctionPlan { func, columns, .. } =
        bound_table_function("SELECT * FROM generate_series(1, 5)");
    assert_eq!(func, crate::TableFn::GenerateSeries(PgType::Int4));
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0].name, "generate_series");
    assert_eq!(columns[0].ty, PgType::Int4);
}

#[test]
fn generate_series_widens_to_int8() {
    // A bigint bound widens the whole series to int8.
    let TableFunctionPlan { func, columns, .. } =
        bound_table_function("SELECT * FROM generate_series(1, 5000000000)");
    assert_eq!(func, crate::TableFn::GenerateSeries(PgType::Int8));
    assert_eq!(columns[0].ty, PgType::Int8);
}

#[test]
fn generate_series_three_arg_step_binds() {
    let TableFunctionPlan { func, args, .. } =
        bound_table_function("SELECT * FROM generate_series(1, 10, 3)");
    assert_eq!(func, crate::TableFn::GenerateSeries(PgType::Int4));
    assert_eq!(args.len(), 3);
}

#[test]
fn generate_series_wrong_arity_is_42883() {
    let e = bind_err("SELECT * FROM generate_series(1)");
    assert_eq!(e.code, "42883");
}

#[test]
fn table_fn_bare_alias_names_the_output_column() -> anyhow::Result<()> {
    // PG names a scalar function's single output column after the alias, so
    // `i` is both the relation qualifier and the column name. The `AS` is
    // optional; both spellings must behave the same.
    for sql in [
        "SELECT i FROM generate_series(1, 5) AS i",
        "SELECT i FROM generate_series(1, 5) i",
    ] {
        let LogicalPlan::TableFunction(TableFunctionPlan { columns, .. }) = bind_one(sql)? else {
            panic!("expected TableFunction for `{sql}`");
        };
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "i");
        assert_eq!(columns[0].ty, PgType::Int4);
    }
    // The qualified spelling still resolves through the same alias.
    bound("SELECT i.i FROM generate_series(1, 5) AS i");

    Ok(())
}

#[test]
fn table_fn_alias_column_list_wins_over_bare_alias() {
    let TableFunctionPlan { columns, .. } =
        bound_table_function("SELECT g FROM generate_series(1, 3) AS s(g)");
    assert_eq!(columns[0].name, "g");
    // A list longer than the rowset is still 42P10.
    let e = bind_err("SELECT * FROM generate_series(1, 3) AS s(a, b)");
    assert_eq!(e.code, "42P10");
}

#[test]
fn composite_table_fn_bare_alias_does_not_rename() {
    // `pg_input_error_info` returns a record: the alias names the relation
    // only, and the row type's column names survive.
    let TableFunctionPlan { columns, .. } =
        bound_table_function("SELECT * FROM pg_input_error_info('1e400', 'float4') AS e");
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["message", "detail", "hint", "sql_error_code"]);
}

#[test]
fn base_table_bare_alias_does_not_rename_columns() {
    // The rename is specific to scalar function FROM items — a bare alias on
    // a real relation names the relation and nothing else.
    let QueryPlan { columns, .. } = bound_query("SELECT * FROM t AS x");
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "big", "name", "flag"]);
}

#[test]
fn unnest_in_from_binds_element_column() {
    let TableFunctionPlan { func, columns, .. } =
        bound_table_function("SELECT * FROM unnest(ARRAY[1, 2, 3])");
    assert_eq!(func, crate::TableFn::Unnest(PgType::Int4));
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0].name, "unnest");
    assert_eq!(columns[0].ty, PgType::Int4);

    // And the alias renames it, like any other scalar SRF.
    let TableFunctionPlan { columns, .. } =
        bound_table_function("SELECT u FROM unnest(ARRAY['a', 'b']) AS u");
    assert_eq!(columns[0].name, "u");
}

#[test]
fn lateral_table_fn_argument_reports_lateral_not_a_missing_column() {
    // A function FROM item is implicitly LATERAL in PG, so all of these are
    // legal there. Bound in an empty scope they would fail with a misleading
    // 42P01/42703 blaming a FROM clause that plainly lists `t`; say what is
    // actually missing instead, exactly as the derived-table arm does.
    for sql in [
        "SELECT * FROM t, generate_series(1, t.id) g",
        "SELECT * FROM t, generate_series(1, id) g",
        "SELECT * FROM t CROSS JOIN generate_series(1, t.id) g",
        "SELECT * FROM t JOIN generate_series(1, t.id) g ON true",
        "SELECT * FROM t CROSS JOIN LATERAL unnest(t.name) u",
    ] {
        let e = bind_err(sql);
        assert_eq!(e.code, "0A000", "for `{sql}`");
        assert_eq!(e.message, "LATERAL is not supported yet", "for `{sql}`");
    }
}

#[test]
fn lateral_argument_that_resolves_but_has_no_overload_reports_the_overload() {
    // `t.name` is text, so even with LATERAL there is no `unnest(text)`. The
    // argument resolves against the left side, so the misleading 42P01 is
    // gone, but the honest answer is PG's 42883 — not a LATERAL gap.
    let e = bind_err("SELECT * FROM t, unnest(t.name) u");
    assert_eq!(e.code, "42883");
    assert_eq!(e.message, "function unnest(text) does not exist");
}

#[test]
fn table_fn_argument_that_resolves_nowhere_keeps_its_own_error() {
    // The LATERAL rewrite above must not swallow a genuinely unknown column:
    // PG reports `column "nosuchcol" does not exist` for both of these.
    for sql in [
        "SELECT * FROM generate_series(1, nosuchcol)",
        "SELECT * FROM t, generate_series(1, nosuchcol) g",
    ] {
        let e = bind_err(sql);
        assert_eq!(e.code, "42703", "for `{sql}`");
        assert_eq!(
            e.message, "column \"nosuchcol\" does not exist",
            "for `{sql}`"
        );
    }
}

#[test]
fn unnest_in_from_rejects_unsupported_forms() {
    assert_eq!(
        bind_err("SELECT * FROM unnest(ARRAY[1, 2]) WITH ORDINALITY").message,
        "WITH ORDINALITY is not supported yet"
    );
    assert_eq!(
        bind_err("SELECT * FROM unnest(ARRAY[1, 2], ARRAY[3, 4])").message,
        "unnest with multiple arrays is not supported yet"
    );
    // A non-array argument is still resolved by `resolve_unnest`.
    assert_eq!(bind_err("SELECT * FROM unnest(1)").code, "42883");
}

#[test]
fn generate_series_in_target_list_is_srf_projection() {
    // A FROM-less SRF in the target list expands over a single dummy row.
    let SubqueryPlan {
        columns,
        projections,
        source,
        ..
    } = bound_subquery("SELECT generate_series(1, 5)");
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0].name, "generate_series");
    assert_eq!(columns[0].ty, PgType::Int4);
    assert!(matches!(projections[0], BoundExpr::Srf { .. }));
    assert!(matches!(*source, LogicalPlan::Values(ValuesPlan { .. })));
}

#[test]
fn generate_series_in_target_list_over_table() {
    // Mixed scalar + SRF projection over a base table stays a Query.
    let QueryPlan { projections, .. } = bound_query("SELECT id, generate_series(1, 2) FROM t");
    assert!(matches!(projections[0], BoundExpr::ColumnRef { .. }));
    assert!(matches!(projections[1], BoundExpr::Srf { .. }));
}

fn table_fn(sql: &str) -> (crate::TableFn, Vec<OutputColumn>) {
    let TableFunctionPlan { func, columns, .. } = bound_table_function(sql);
    (func, columns)
}

#[test]
fn generate_series_numeric_overload_binds() {
    // A decimal argument (typed numeric) selects the numeric overload.
    let (func, columns) = table_fn("SELECT * FROM generate_series(1, 3, 0.5)");
    assert_eq!(func, crate::TableFn::GenerateSeries(PgType::Numeric));
    assert_eq!(columns[0].name, "generate_series");
    assert_eq!(columns[0].ty, PgType::Numeric);
}

#[test]
fn generate_series_timestamp_overload_binds() {
    let (func, columns) = table_fn(
        "SELECT * FROM generate_series(timestamp '2020-01-01', \
         timestamp '2020-01-05', interval '1 day')",
    );
    assert_eq!(func, crate::TableFn::GenerateSeries(PgType::Timestamp));
    assert_eq!(columns[0].ty, PgType::Timestamp);
}

#[test]
fn generate_series_timestamptz_overload_binds() {
    let (func, _columns) = table_fn(
        "SELECT * FROM generate_series(timestamptz '2020-01-01+00', \
         timestamptz '2020-01-05+00', interval '1 day')",
    );
    assert_eq!(func, crate::TableFn::GenerateSeries(PgType::TimestampTz));
}

#[test]
fn generate_series_timestamp_requires_three_args() {
    // The timestamp overload has no 2-arg form: PG rejects it as 42883.
    let e =
        bind_err("SELECT * FROM generate_series(timestamp '2020-01-01', timestamp '2020-01-05')");
    assert_eq!(e.code, "42883");
}
