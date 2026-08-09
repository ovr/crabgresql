//! CASE expressions and boolean tests.

use super::common::*;

fn case_column(sql: &str) -> (OutputColumn, BoundExpr) {
    let LogicalPlan::Query(QueryPlan {
        columns,
        projections,
        ..
    }) = bound(sql)
    else {
        panic!("expected Query");
    };
    (columns[0].clone(), projections[0].clone())
}

#[test]
fn case_default_column_name_is_case() {
    let (col, expr) = case_column("SELECT CASE WHEN flag THEN id END FROM t");
    assert_eq!(col.name, "case");
    assert!(matches!(expr, BoundExpr::Case { .. }));
}

#[test]
fn case_result_branches_promote_to_common_type() {
    // int4 THEN, int8 ELSE -> int8, with a Coerce inserted on the int4 arm.
    let (col, expr) = case_column("SELECT CASE WHEN flag THEN id ELSE big END FROM t");
    assert_eq!(col.ty, PgType::Int8);
    let BoundExpr::Case { whens, else_, ty } = expr else {
        panic!("expected Case");
    };
    assert_eq!(ty, PgType::Int8);
    assert!(matches!(
        &whens[0].1,
        BoundExpr::Coerce {
            ty: PgType::Int8,
            ..
        }
    ));
    assert!(matches!(
        else_.as_deref(),
        Some(BoundExpr::ColumnRef {
            ty: PgType::Int8,
            ..
        })
    ));
}

#[test]
fn all_untyped_case_branches_resolve_to_text() {
    let (col, _) = case_column("SELECT CASE WHEN flag THEN NULL ELSE NULL END FROM t");
    assert_eq!(col.ty, PgType::Text);
}

#[test]
fn simple_case_desugars_operand_to_equality() {
    // CASE id WHEN 1 THEN ... becomes a boolean `id = 1` condition.
    let (_, expr) = case_column("SELECT CASE id WHEN 1 THEN 'a' END FROM t");
    let BoundExpr::Case { whens, .. } = expr else {
        panic!("expected Case");
    };
    assert!(matches!(
        &whens[0].0,
        BoundExpr::Binary {
            op: BinOp::Eq,
            arg_ty: PgType::Int4,
            ..
        }
    ));
}

#[test]
fn non_boolean_when_condition_is_42804() {
    let e = bind_err("SELECT CASE WHEN id THEN 1 END FROM t");
    assert_eq!(e.code, "42804");
    assert_eq!(
        e.message,
        "argument of CASE/WHEN must be type boolean, not type integer"
    );
}

/// The first projected expression of a bound `SELECT`.
fn first_projection(sql: &str) -> BoundExpr {
    let LogicalPlan::Query(QueryPlan { projections, .. }) = bound(sql) else {
        panic!("expected Query");
    };
    projections.into_iter().next().expect("no projections")
}

#[test]
fn boolean_test_operand_must_be_boolean() {
    // PG names the clause after the spelling that was used, so each form
    // reports itself.
    for (sql, context) in [
        ("SELECT id IS TRUE FROM t", "IS TRUE"),
        ("SELECT id IS NOT TRUE FROM t", "IS NOT TRUE"),
        ("SELECT id IS FALSE FROM t", "IS FALSE"),
        ("SELECT id IS NOT FALSE FROM t", "IS NOT FALSE"),
        ("SELECT id IS UNKNOWN FROM t", "IS UNKNOWN"),
        ("SELECT id IS NOT UNKNOWN FROM t", "IS NOT UNKNOWN"),
    ] {
        let e = bind_err(sql);
        assert_eq!(e.code, "42804", "{sql}");
        assert_eq!(
            e.message,
            format!("argument of {context} must be type boolean, not type integer"),
            "{sql}"
        );
    }
}

#[test]
fn every_boolean_context_points_its_cursor_at_the_operand() {
    // PG prints `LINE n: ... ^` under the non-boolean operand for all of
    // these, so `to_bool_operand` takes the operand span rather than each
    // caller remembering to stamp one. `operand` is the token the cursor
    // must land on; its 1-based column is derived so the case cannot claim
    // a position the SQL does not have.
    for (sql, context, operand) in [
        ("SELECT 1 FROM t WHERE id", "WHERE", "id"),
        ("SELECT CASE WHEN id THEN 1 END FROM t", "CASE/WHEN", "id"),
        ("SELECT NOT id FROM t", "NOT", "id"),
        ("SELECT 1 FROM t GROUP BY id HAVING id", "HAVING", "id"),
        ("SELECT 1 FROM t a JOIN t b ON a.id", "JOIN/ON", "a.id"),
        ("SELECT 1 FROM t WHERE flag AND id", "AND", "id"),
        ("SELECT id IS TRUE FROM t", "IS TRUE", "id"),
    ] {
        // The offending operand is the last such token in every fixture.
        let col = sql.rfind(operand).expect("operand not in fixture") + 1;
        let e = bind_err(sql);
        assert_eq!(e.code, "42804", "{sql}");
        assert_eq!(
            e.message,
            format!("argument of {context} must be type boolean, not type integer"),
            "{sql}"
        );
        assert_eq!(e.location, Some((1, col as u64)), "{sql}");
    }
}

#[test]
fn boolean_test_takes_an_untyped_literal_as_boolean() {
    // Unlike IS NULL, which defaults an unknown to text, the boolean tests
    // give it boolean from context — so 'true' parses rather than failing.
    assert!(matches!(
        first_projection("SELECT 'true' IS TRUE FROM t"),
        BoundExpr::BoolTest { .. }
    ));
    let e = bind_err("SELECT 'a' IS TRUE FROM t");
    assert_eq!(e.message, "invalid input syntax for type boolean: \"a\"");
}

#[test]
fn is_unknown_is_a_bool_test_against_null() {
    // UNKNOWN is the third boolean value, so it rides the same node rather
    // than collapsing into IsNull — which would lose the spelling EXPLAIN
    // has to print back.
    assert!(matches!(
        first_projection("SELECT flag IS UNKNOWN FROM t"),
        BoundExpr::BoolTest {
            value: None,
            negated: false,
            ..
        }
    ));
    assert!(matches!(
        first_projection("SELECT flag IS NOT UNKNOWN FROM t"),
        BoundExpr::BoolTest {
            value: None,
            negated: true,
            ..
        }
    ));
}

#[test]
fn incompatible_case_results_are_42804() {
    // ELSE participates first in unification, matching PG's type order.
    let e = bind_err("SELECT CASE WHEN flag THEN id ELSE name END FROM t");
    assert_eq!(e.code, "42804");
    assert_eq!(e.message, "CASE types text and integer cannot be matched");
}

#[test]
fn simple_case_untyped_operand_resolves_to_text() {
    // PG gives an untyped-literal operand its own type (text) before
    // comparing, so a NULL or string operand against an integer WHEN value
    // is `text = integer` (operator does not exist), not a read of the
    // operand as integer.
    for sql in [
        "SELECT CASE NULL WHEN 1 THEN 'a' ELSE 'b' END",
        "SELECT CASE 'x' WHEN 1 THEN 'a' END",
    ] {
        let e = bind_err(sql);
        assert_eq!(e.code, "42883", "{sql}");
        assert_eq!(
            e.message, "operator does not exist: text = integer",
            "{sql}"
        );
    }
    // Two untyped literals still compare as text (unchanged).
    assert!(bind_one("SELECT CASE 'x' WHEN 'y' THEN 1 ELSE 2 END").is_ok());
}
