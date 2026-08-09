//! Window functions: chains, named windows and frames.

use super::common::*;

/// The window chain is a bare row source under a `Subquery` that carries the
/// query's own target list, so `projections` above it index
/// `[input row…, window slots…]` and every marker has become a `ColumnRef`
/// into that row.
#[test]
fn a_window_call_becomes_a_column_ref_past_the_input_row() {
    let SubqueryPlan {
        source,
        projections,
        ..
    } = bound_subquery("SELECT id, rank() OVER (ORDER BY name) FROM t");
    // `t` is four columns wide, so the single window slot is index 4.
    assert_eq!(
        projections,
        vec![
            BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4
            },
            BoundExpr::ColumnRef {
                index: 4,
                ty: PgType::Int8
            },
        ]
    );
    let WindowPlan {
        funcs,
        input_width,
        output_width,
        ..
    } = source.expect_window();
    assert_eq!((input_width, output_width), (4, 5));
    assert_eq!(funcs.len(), 1);
    assert_eq!(funcs[0].slot, 4);
}

/// Calls that share an `OVER` clause are computed by one step, over one sort
/// of the input — `WINDOW w1 AS (…), w2 AS (…)` with identical bodies must
/// not produce two.
#[test]
fn calls_sharing_a_spec_collapse_into_one_window_step() {
    let SubqueryPlan { source, .. } = bound_subquery(
        "SELECT rank() OVER w1, sum(big) OVER w2 FROM t \
         WINDOW w1 AS (ORDER BY name), w2 AS (ORDER BY name)",
    );
    let WindowPlan { source, funcs, .. } = source.expect_window();
    assert_eq!(funcs.len(), 2, "both calls land on the same step");
    assert!(
        !matches!(*source, LogicalPlan::Window(WindowPlan { .. })),
        "one step, so nothing below it"
    );
}

/// PG evaluates the spec with the most keys first, so the chain's *bottom*
/// is the widest spec. The order is observable: the last one evaluated
/// leaves its sort in place, and that is the order a window query with no
/// ORDER BY of its own returns rows in.
#[test]
fn the_widest_window_spec_is_evaluated_first() {
    let SubqueryPlan { source, .. } = bound_subquery(
        "SELECT rank() OVER (ORDER BY name), \
         sum(big) OVER (PARTITION BY id ORDER BY name) FROM t",
    );
    let WindowPlan { source, spec, .. } = source.expect_window();
    assert_eq!(spec.partition_by.len(), 0, "the 1-key spec is on top");
    let WindowPlan { spec, .. } = source.expect_window();
    assert_eq!(
        spec.partition_by.len(),
        1,
        "the 2-key spec is at the bottom"
    );
}

/// Windows are extracted after aggregation, so by then the inner `sum(x)` is
/// already a `ColumnRef` into the aggregate's `[keys…, aggregates…]` row and
/// the window reads that row.
#[test]
fn a_window_can_sit_over_a_grouped_aggregate() {
    let SubqueryPlan { source, .. } =
        bound_subquery("SELECT sum(sum(big)) OVER (ORDER BY name) FROM t GROUP BY name");
    let WindowPlan {
        source,
        input_width,
        ..
    } = source.expect_window();
    // One group key plus one aggregate.
    assert_eq!(input_width, 2);
    assert!(matches!(
        *source,
        LogicalPlan::Aggregate(AggregatePlan { .. })
    ));
}

/// A window in an ORDER BY expression rides the hidden ("resjunk") column
/// `bind_order_by` already appended, so extraction sweeps it up for free.
#[test]
fn a_window_in_order_by_lands_in_a_hidden_column() {
    let SubqueryPlan {
        columns,
        projections,
        sort,
        ..
    } = bound_subquery("SELECT id FROM t ORDER BY rank() OVER (ORDER BY name)");
    assert_eq!(columns.len(), 1, "one visible output column");
    assert_eq!(projections.len(), 2, "plus one hidden sort column");
    assert_eq!(sort.len(), 1);
    assert_eq!(sort[0].column, 1, "the sort keys on the hidden column");
    assert_eq!(
        projections[1],
        BoundExpr::ColumnRef {
            index: 4,
            ty: PgType::Int8
        }
    );
}

/// Every clause evaluated before windows are, plus the forms PG rejects
/// outright. Text and SQLSTATE are PG 18.4's, observed through psql.
#[test]
fn window_misuse_reports_pg_text_and_sqlstate() {
    for (sql, code, message) in [
        (
            "SELECT 1 FROM t WHERE rank() OVER () > 1",
            "42P20",
            "window functions are not allowed in WHERE",
        ),
        (
            "SELECT a.id FROM t a LEFT JOIN t b ON rank() OVER () > 0",
            "42P20",
            "window functions are not allowed in JOIN conditions",
        ),
        (
            "SELECT a.id FROM t a LEFT JOIN t b ON rank() OVER w = 1 \
             WINDOW w AS (ORDER BY a.id)",
            "42P20",
            "window functions are not allowed in JOIN conditions",
        ),
        (
            "UPDATE t SET id = 1 RETURNING rank() OVER ()",
            "42P20",
            "window functions are not allowed in RETURNING",
        ),
        (
            "UPDATE t SET id = rank() OVER ()",
            "42P20",
            "window functions are not allowed in UPDATE",
        ),
        (
            "UPDATE t SET id = 1 WHERE rank() OVER () > 0",
            "42P20",
            "window functions are not allowed in WHERE",
        ),
        (
            "DELETE FROM t WHERE rank() OVER () > 0",
            "42P20",
            "window functions are not allowed in WHERE",
        ),
        (
            "INSERT INTO t VALUES (rank() OVER ())",
            "42P20",
            "window functions are not allowed in VALUES",
        ),
        (
            "VALUES (rank() OVER ())",
            "42P20",
            "window functions are not allowed in VALUES",
        ),
        (
            "SELECT id FROM t LIMIT rank() OVER ()",
            "42P20",
            "window functions are not allowed in LIMIT",
        ),
        (
            "SELECT id FROM t OFFSET rank() OVER ()",
            "42P20",
            "window functions are not allowed in OFFSET",
        ),
        (
            "SELECT id FROM t GROUP BY id HAVING rank() OVER () > 1",
            "42P20",
            "window functions are not allowed in HAVING",
        ),
        (
            "SELECT id FROM t GROUP BY id, rank() OVER ()",
            "42P20",
            "window functions are not allowed in GROUP BY",
        ),
        (
            "SELECT rank() OVER (PARTITION BY rank() OVER ()) FROM t",
            "42P20",
            "window functions are not allowed in window definitions",
        ),
        (
            "SELECT sum(rank() OVER ()) OVER () FROM t",
            "42P20",
            "window function calls cannot be nested",
        ),
        (
            "SELECT sum(sum(big) OVER ()) FROM t",
            "42803",
            "aggregate function calls cannot contain window function calls",
        ),
        (
            "SELECT sum(DISTINCT big) OVER () FROM t",
            "0A000",
            "DISTINCT is not implemented for window functions",
        ),
        (
            "SELECT rank() OVER w FROM t",
            "42704",
            "window \"w\" does not exist",
        ),
        (
            "SELECT rank() OVER (w PARTITION BY id) FROM t WINDOW w AS (ORDER BY name)",
            "42P20",
            "cannot override PARTITION BY clause of window \"w\"",
        ),
        (
            "SELECT rank() OVER (w ORDER BY id) FROM t WINDOW w AS (ORDER BY name)",
            "42P20",
            "cannot override ORDER BY clause of window \"w\"",
        ),
        (
            "SELECT rank() OVER (w) FROM t \
             WINDOW w AS (ORDER BY name ROWS UNBOUNDED PRECEDING)",
            "42P20",
            "cannot copy window \"w\" because it has a frame clause",
        ),
        (
            "SELECT rank() OVER () FROM t WINDOW w AS (ORDER BY name), w AS (ORDER BY id)",
            "42P20",
            "window \"w\" is already defined",
        ),
        (
            "SELECT rank() FROM t",
            "42809",
            "window function rank requires an OVER clause",
        ),
        (
            "SELECT abs(id) OVER () FROM t",
            "42809",
            "OVER specified, but abs is not a window function nor an aggregate function",
        ),
    ] {
        let error = bind_err(sql);
        assert_eq!(error.code, code, "for: {sql}");
        assert_eq!(error.message, message, "for: {sql}");
    }
}

/// `OVER w` *is* the named window, frame and all; `OVER (w)` **copies** it,
/// and a copy supplies its own frame, so it cannot take one from the base.
/// Conflating the two would reject `OVER w` on a framed window, which PG
/// accepts — its hint on the copy error points at exactly this difference.
#[test]
fn a_named_window_reference_inherits_a_frame_that_a_copy_may_not() {
    let framed = "FROM t WINDOW w AS (ORDER BY name ROWS UNBOUNDED PRECEDING)";
    let copy = bind_err(&format!("SELECT rank() OVER (w) {framed}"));
    assert_eq!(copy.code, "42P20");
    assert_eq!(
        copy.message,
        "cannot copy window \"w\" because it has a frame clause"
    );
    assert_eq!(
        copy.hint.as_deref(),
        Some("Omit the parentheses in this OVER clause.")
    );
    // The reference form gets past the copy rules and inherits the frame,
    // which is then refused only because explicit frames are unimplemented.
    let reference = bind_err(&format!("SELECT rank() OVER w {framed}"));
    assert_eq!(reference.code, "0A000");
    assert_eq!(
        reference.message,
        "explicit window frames are not supported yet"
    );
}

/// Only the default frame is implemented; an explicit one — including the
/// `EXCLUDE` clause the parser now accepts — is refused loudly rather than
/// silently computed as the default.
#[test]
fn an_explicit_window_frame_stays_0a000() {
    for sql in [
        "SELECT sum(big) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
        "SELECT sum(big) OVER (ORDER BY id GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
        "SELECT sum(big) OVER (ORDER BY id RANGE UNBOUNDED PRECEDING EXCLUDE TIES) FROM t",
    ] {
        let error = bind_err(sql);
        assert_eq!(error.code, "0A000", "for: {sql}");
        assert_eq!(
            error.message,
            "explicit window frames are not supported yet"
        );
    }
}

/// Writing the default frame out longhand is common and means exactly the
/// frame a spec gets anyway, so it binds.
#[test]
fn the_default_frame_written_longhand_is_accepted() {
    for sql in [
        "SELECT sum(big) OVER (ORDER BY id RANGE UNBOUNDED PRECEDING) FROM t",
        "SELECT sum(big) OVER (ORDER BY id \
         RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t",
        "SELECT sum(big) OVER (ORDER BY id \
         RANGE UNBOUNDED PRECEDING EXCLUDE NO OTHERS) FROM t",
    ] {
        assert!(
            matches!(bound(sql), LogicalPlan::Subquery(SubqueryPlan { .. })),
            "for: {sql}"
        );
    }
}

/// The clauses that reject a window also reject an aggregate, and had no
/// guard for either before the window work — these are the aggregate half.
#[test]
fn aggregate_misuse_in_dml_reports_pg_text_and_sqlstate() {
    for (sql, clause) in [
        ("UPDATE t SET id = count(*)", "UPDATE"),
        ("UPDATE t SET id = 1 WHERE count(*) > 0", "WHERE"),
        ("DELETE FROM t WHERE count(*) > 0", "WHERE"),
        ("INSERT INTO t VALUES (count(*))", "VALUES"),
        ("VALUES (count(*))", "VALUES"),
        ("SELECT id FROM t LIMIT count(*)", "LIMIT"),
        ("SELECT id FROM t OFFSET count(*)", "OFFSET"),
    ] {
        let error = bind_err(sql);
        assert_eq!(error.code, "42803", "for: {sql}");
        assert_eq!(
            error.message,
            format!("aggregate functions are not allowed in {clause}"),
            "for: {sql}"
        );
    }
}

/// PG analyzes an expression in source order and blames the first misplaced
/// node it meets, so which of an aggregate and a window is reported depends
/// on which is written first.
#[test]
fn the_leftmost_offender_is_the_one_reported() {
    for (sql, code, message) in [
        (
            "SELECT 1 FROM t WHERE count(*) > 0 AND rank() OVER () > 0",
            "42803",
            "aggregate functions are not allowed in WHERE",
        ),
        (
            "SELECT 1 FROM t WHERE rank() OVER () > 0 AND count(*) > 0",
            "42P20",
            "window functions are not allowed in WHERE",
        ),
        (
            "UPDATE t SET id = 1 RETURNING count(*) + rank() OVER ()",
            "42803",
            "aggregate functions are not allowed in RETURNING",
        ),
        (
            "UPDATE t SET id = 1 RETURNING rank() OVER () + count(*)",
            "42P20",
            "window functions are not allowed in RETURNING",
        ),
    ] {
        let error = bind_err(sql);
        assert_eq!(error.code, code, "for: {sql}");
        assert_eq!(error.message, message, "for: {sql}");
    }
}

/// `TABLE t` is `SELECT * FROM t`, so its ORDER BY can hold a window call and
/// must go through the same extraction — otherwise the marker survives into
/// the plan and fails at evaluation.
#[test]
fn a_table_query_order_by_a_window_builds_a_window_chain() {
    let SubqueryPlan {
        source,
        columns,
        projections,
        sort,
        ..
    } = bound_subquery("TABLE t ORDER BY rank() OVER (ORDER BY id DESC)");
    assert!(matches!(*source, LogicalPlan::Window(WindowPlan { .. })));
    assert_eq!(columns.len(), 4, "t's own columns stay visible");
    assert_eq!(projections.len(), 5, "plus the hidden sort column");
    assert_eq!(sort[0].column, 4);
    assert!(
        !projections.iter().any(BoundExpr::contains_window),
        "no marker survives extraction"
    );
}

/// A `WINDOW` definition may name an *earlier* one, and the copy inherits
/// what the base contributes. Expanding at build time is also what makes a
/// self or forward reference report "does not exist", as PG does.
#[test]
fn a_named_window_expands_its_base_at_build_time() {
    let SubqueryPlan { source, .. } = bound_subquery(
        "SELECT rank() OVER w2 FROM t WINDOW w1 AS (PARTITION BY name), \
         w2 AS (w1 ORDER BY id)",
    );
    let WindowPlan { spec, .. } = source.expect_window();
    assert_eq!(spec.partition_by.len(), 1, "w1's PARTITION BY is inherited");
    assert_eq!(spec.order_by.len(), 1);

    for (sql, code, message) in [
        (
            "SELECT 1 FROM t WINDOW w AS (w)",
            "42704",
            "window \"w\" does not exist",
        ),
        (
            "SELECT rank() OVER w2 FROM t \
             WINDOW w2 AS (w1 ORDER BY id), w1 AS (PARTITION BY name)",
            "42704",
            "window \"w1\" does not exist",
        ),
        (
            "SELECT 1 FROM t WINDOW w AS (ORDER BY id), w AS (nosuchwin)",
            "42P20",
            "window \"w\" is already defined",
        ),
        (
            "SELECT 1 FROM t WINDOW w1 AS (ORDER BY id ROWS UNBOUNDED PRECEDING), \
             w2 AS (w1)",
            "42P20",
            "cannot copy window \"w1\" because it has a frame clause",
        ),
        (
            "SELECT 1 FROM t WINDOW w1 AS (ORDER BY id), w2 AS (w1 ORDER BY name)",
            "42P20",
            "cannot override ORDER BY clause of window \"w1\"",
        ),
    ] {
        let error = bind_err(sql);
        assert_eq!(error.code, code, "for: {sql}");
        assert_eq!(error.message, message, "for: {sql}");
    }
}

/// The name alone does not make a call a window call — PG resolves by name
/// *and* argument types, so only the zero-argument form is the builtin. The
/// user-function half needs a catalog and is covered end to end.
#[test]
fn only_a_zero_argument_call_resolves_to_a_window_function() {
    let bare = bind_err("SELECT rank() FROM t");
    assert_eq!(bare.code, "42809");
    assert_eq!(bare.message, "window function rank requires an OVER clause");

    let with_arg = bind_err("SELECT rank(1) FROM t");
    assert_eq!(with_arg.code, "42883");
    assert_eq!(with_arg.message, "function rank(integer) does not exist");
}
