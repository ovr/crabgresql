//! `COALESCE` and `NULLIF`: the two shorthands PostgreSQL implements in its
//! grammar rather than in `pg_proc`.

use super::common::*;

fn first_column(sql: &str) -> anyhow::Result<(OutputColumn, BoundExpr)> {
    let QueryPlan {
        columns,
        projections,
        ..
    } = bound_query(sql)?;
    Ok((columns[0].clone(), projections[0].clone()))
}

#[test]
fn coalesce_default_column_name_is_coalesce() -> anyhow::Result<()> {
    let (col, expr) = first_column("SELECT COALESCE(id, 0) FROM t")?;
    assert_eq!(col.name, "coalesce");
    assert_eq!(col.ty, PgType::Int4);
    let BoundExpr::Coalesce { args, ty } = expr else {
        bail!("expected Coalesce");
    };
    assert_eq!(ty, PgType::Int4);
    assert_eq!(args.len(), 2);
    Ok(())
}

/// The whole reason `COALESCE` is its own node: every argument appears once, so
/// the executor can stop at the first non-NULL one.
#[test]
fn coalesce_holds_each_argument_once() -> anyhow::Result<()> {
    let (_, expr) = first_column("SELECT COALESCE(id, id, id) FROM t")?;
    let BoundExpr::Coalesce { args, .. } = expr else {
        bail!("expected Coalesce");
    };
    assert_eq!(args.len(), 3);
    assert!(
        args.iter()
            .all(|a| matches!(a, BoundExpr::ColumnRef { index: 0, .. }))
    );
    Ok(())
}

#[test]
fn coalesce_arguments_promote_to_common_type() -> anyhow::Result<()> {
    // int4 and int8 -> int8, with a Coerce inserted on the int4 argument.
    let (col, expr) = first_column("SELECT COALESCE(id, big) FROM t")?;
    assert_eq!(col.ty, PgType::Int8);
    let BoundExpr::Coalesce { args, ty } = expr else {
        bail!("expected Coalesce");
    };
    assert_eq!(ty, PgType::Int8);
    assert!(matches!(
        &args[0],
        BoundExpr::Coerce {
            ty: PgType::Int8,
            ..
        }
    ));
    assert!(matches!(
        &args[1],
        BoundExpr::ColumnRef {
            ty: PgType::Int8,
            ..
        }
    ));
    Ok(())
}

#[test]
fn an_all_untyped_coalesce_resolves_to_text() -> anyhow::Result<()> {
    let (col, _) = first_column("SELECT COALESCE(NULL, NULL) FROM t")?;
    assert_eq!(col.ty, PgType::Text);
    Ok(())
}

/// A single argument is legal (PG's grammar only demands a non-empty list).
#[test]
fn coalesce_accepts_one_argument() -> anyhow::Result<()> {
    let (col, expr) = first_column("SELECT COALESCE(id) FROM t")?;
    assert_eq!(col.ty, PgType::Int4);
    let BoundExpr::Coalesce { args, .. } = expr else {
        bail!("expected Coalesce");
    };
    assert_eq!(args.len(), 1);
    Ok(())
}

#[test]
fn incompatible_coalesce_arguments_are_42804() -> anyhow::Result<()> {
    let e = bind_err("SELECT COALESCE(id, flag) FROM t")?;
    assert_eq!(e.code, sqlstate::DATATYPE_MISMATCH);
    assert_eq!(
        e.message,
        "COALESCE types integer and boolean cannot be matched"
    );
    Ok(())
}

/// An untyped argument is read at the type the list resolved to, so a literal
/// that does not parse there fails the way a written cast would.
#[test]
fn an_unparsable_untyped_coalesce_argument_is_22p02() -> anyhow::Result<()> {
    let e = bind_err("SELECT COALESCE(id, 'x') FROM t")?;
    assert_eq!(e.message, "invalid input syntax for type integer: \"x\"");
    Ok(())
}

#[test]
fn an_empty_coalesce_is_a_syntax_error() -> anyhow::Result<()> {
    let e = bind_err("SELECT COALESCE()")?;
    assert_eq!(e.code, sqlstate::SYNTAX_ERROR);
    assert_eq!(e.message, "syntax error at or near \")\"");
    Ok(())
}

/// Neither form is reachable through a schema, or under quotes: PG spells them as
/// keywords, so any other spelling is an ordinary function lookup that finds
/// nothing.
#[test]
fn only_a_bare_keyword_reaches_the_special_form() -> anyhow::Result<()> {
    for sql in [
        "SELECT pg_catalog.coalesce(id, 0) FROM t",
        "SELECT \"coalesce\"(id, 0) FROM t",
        "SELECT \"nullif\"(id, 0) FROM t",
        "SELECT \"NULLIF\"(id, 0) FROM t",
    ] {
        let e = bind_err(sql)?;
        assert_eq!(e.code, sqlstate::UNDEFINED_FUNCTION, "{sql}");
    }
    // The keyword itself is case-insensitive, as any keyword is.
    assert_eq!(
        first_column("SELECT CoAlEsCe(id, 0) FROM t")?.0.name,
        "coalesce"
    );
    Ok(())
}

/// PG's grammar gives these a bare expression list, so every decoration a function
/// call may carry is a syntax error — reported at the token PG stops on.
#[test]
fn a_decorated_special_form_is_a_syntax_error() -> anyhow::Result<()> {
    for (sql, token) in [
        ("SELECT coalesce(id, 0) OVER () FROM t", "over"),
        ("SELECT nullif(id, 0) OVER () FROM t", "over"),
        (
            "SELECT coalesce(id, 0) FILTER (WHERE true) FROM t",
            "filter",
        ),
        (
            "SELECT coalesce(id, 0) WITHIN GROUP (ORDER BY id) FROM t",
            "within",
        ),
        ("SELECT coalesce(DISTINCT id, 0) FROM t", "distinct"),
        ("SELECT coalesce(ALL id, 0) FROM t", "all"),
        ("SELECT coalesce(id ORDER BY id) FROM t", "order"),
        ("SELECT coalesce(x => id) FROM t", "=>"),
    ] {
        let e = bind_err(sql)?;
        assert_eq!(e.code, sqlstate::SYNTAX_ERROR, "{sql}");
        assert_eq!(e.message, format!("syntax error at or near \"{token}\""));
    }
    Ok(())
}

#[test]
fn nullif_default_column_name_is_nullif() -> anyhow::Result<()> {
    let (col, _) = first_column("SELECT NULLIF(id, 0) FROM t")?;
    assert_eq!(col.name, "nullif");
    assert_eq!(col.ty, PgType::Int4);
    Ok(())
}

/// `NULLIF(a, b)` is the standard's `CASE WHEN a = b THEN NULL ELSE a END`, and
/// binds to exactly that.
#[test]
fn nullif_lowers_to_a_case_over_equality() -> anyhow::Result<()> {
    let (_, expr) = first_column("SELECT NULLIF(id, 0) FROM t")?;
    let BoundExpr::Case { whens, else_, ty } = expr else {
        bail!("expected Case");
    };
    assert_eq!(ty, PgType::Int4);
    let [(cond, result)] = whens.as_slice() else {
        bail!("expected exactly one WHEN");
    };
    assert!(matches!(
        cond,
        BoundExpr::Binary {
            op: BinOp::Eq,
            arg_ty: PgType::Int4,
            ..
        }
    ));
    assert!(matches!(
        result,
        BoundExpr::Const {
            value: Value::Null,
            ty: PgType::Int4,
        }
    ));
    assert!(matches!(
        else_.as_deref(),
        Some(BoundExpr::ColumnRef {
            index: 0,
            ty: PgType::Int4,
        })
    ));
    Ok(())
}

/// The result is the left argument as the chosen `=` operator takes it. PG compares
/// these pairs cross-type, so the left argument is never coerced and keeps its own
/// type — which is also what keeps `float4` from being widened (and printed at
/// `float8` precision) and a `timestamp` from picking up a zone.
#[test]
fn nullif_keeps_a_left_argument_the_operator_does_not_coerce() -> anyhow::Result<()> {
    for (sql, ty) in [
        ("SELECT NULLIF(id::int2, big) FROM t", PgType::Int2),
        ("SELECT NULLIF(id, big) FROM t", PgType::Int4),
        (
            "SELECT NULLIF(id::float4, id::float8) FROM t",
            PgType::Float4,
        ),
        (
            "SELECT NULLIF(name::timestamp, name::timestamptz) FROM t",
            PgType::Timestamp,
        ),
        (
            "SELECT NULLIF(name::date, name::timestamp) FROM t",
            PgType::Date,
        ),
        ("SELECT NULLIF(name::name, name) FROM t", PgType::Name),
    ] {
        assert_eq!(first_column(sql)?.0.ty, ty, "{sql}");
    }
    Ok(())
}

/// Where PG has no cross-type operator it coerces both sides first, and the
/// comparison's own operand type is the result: `int = numeric` compares in
/// `numeric`, and `varchar = varchar` compares as `text`.
#[test]
fn nullif_reports_the_compared_type_when_the_operand_is_coerced() -> anyhow::Result<()> {
    for (sql, ty) in [
        ("SELECT NULLIF(id, 2.5) FROM t", PgType::Numeric),
        ("SELECT NULLIF(id, id::float8) FROM t", PgType::Float8),
        (
            "SELECT NULLIF(name::varchar, name::varchar) FROM t",
            PgType::Text,
        ),
        ("SELECT NULLIF(name, name::name) FROM t", PgType::Text),
    ] {
        assert_eq!(first_column(sql)?.0.ty, ty, "{sql}");
    }
    Ok(())
}

#[test]
fn nullif_needs_exactly_two_arguments() -> anyhow::Result<()> {
    let one = bind_err("SELECT NULLIF(id) FROM t")?;
    assert_eq!(one.code, sqlstate::SYNTAX_ERROR);
    assert_eq!(one.message, "syntax error at or near \")\"");
    let three = bind_err("SELECT NULLIF(id, 1, 2) FROM t")?;
    assert_eq!(three.code, sqlstate::SYNTAX_ERROR);
    assert_eq!(three.message, "syntax error at or near \",\"");
    Ok(())
}

/// Incomparable arguments fail as the written comparison would — operator
/// resolution is what decides, so the message is its own.
#[test]
fn incomparable_nullif_arguments_report_the_missing_operator() -> anyhow::Result<()> {
    let e = bind_err("SELECT NULLIF(id, flag) FROM t")?;
    assert_eq!(e.code, sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(e.message, "operator does not exist: integer = boolean");
    Ok(())
}

/// The ELSE arm repeats the first argument, so a volatile one would be evaluated
/// twice. That is refused rather than answered wrongly.
#[test]
fn nullif_refuses_a_volatile_first_argument() -> anyhow::Result<()> {
    let e = bind_err("SELECT NULLIF(clock_timestamp(), NULL)")?;
    assert_eq!(e.code, sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(
        e.message,
        "NULLIF over a volatile expression is not supported yet"
    );
    Ok(())
}
