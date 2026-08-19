//! `GREATEST` / `LEAST`: two more shorthands PostgreSQL implements in its
//! grammar rather than in `pg_proc`.

use super::common::*;
use crate::expr::MinMaxKind;

fn first_column(sql: &str) -> anyhow::Result<(OutputColumn, BoundExpr)> {
    let QueryPlan {
        columns,
        projections,
        ..
    } = bound_query(sql)?;
    Ok((columns[0].clone(), projections[0].clone()))
}

#[test]
fn the_default_column_name_is_the_keyword() -> anyhow::Result<()> {
    for (sql, name, kind) in [
        (
            "SELECT GREATEST(id, 0) FROM t",
            "greatest",
            MinMaxKind::Greatest,
        ),
        ("SELECT LEAST(id, 0) FROM t", "least", MinMaxKind::Least),
    ] {
        let (col, expr) = first_column(sql)?;
        assert_eq!(col.name, name);
        assert_eq!(col.ty, PgType::Int4);
        let BoundExpr::MinMax {
            kind: k, args, ty, ..
        } = expr
        else {
            bail!("expected MinMax for `{sql}`");
        };
        assert_eq!(k, kind);
        assert_eq!(ty, PgType::Int4);
        assert_eq!(args.len(), 2);
    }
    Ok(())
}

#[test]
fn arguments_promote_to_the_common_type() -> anyhow::Result<()> {
    let (col, expr) = first_column("SELECT GREATEST(id, big) FROM t")?;
    assert_eq!(col.ty, PgType::Int8);
    let BoundExpr::MinMax { args, ty, .. } = expr else {
        bail!("expected MinMax");
    };
    assert_eq!(ty, PgType::Int8);
    assert!(matches!(
        &args[0],
        BoundExpr::Coerce {
            ty: PgType::Int8,
            ..
        }
    ));
    Ok(())
}

#[test]
fn an_all_untyped_list_resolves_to_text() -> anyhow::Result<()> {
    assert_eq!(
        first_column("SELECT LEAST(NULL, NULL) FROM t")?.0.ty,
        PgType::Text
    );
    Ok(())
}

#[test]
fn one_argument_is_legal() -> anyhow::Result<()> {
    let (col, expr) = first_column("SELECT GREATEST(id) FROM t")?;
    assert_eq!(col.ty, PgType::Int4);
    let BoundExpr::MinMax { args, .. } = expr else {
        bail!("expected MinMax");
    };
    assert_eq!(args.len(), 1);
    Ok(())
}

#[test]
fn incompatible_arguments_are_42804_under_the_keyword() -> anyhow::Result<()> {
    for (sql, message) in [
        (
            "SELECT GREATEST(id, flag) FROM t",
            "GREATEST types integer and boolean cannot be matched",
        ),
        (
            "SELECT LEAST(id, flag) FROM t",
            "LEAST types integer and boolean cannot be matched",
        ),
    ] {
        let e = bind_err(sql)?;
        assert_eq!(e.code, sqlstate::DATATYPE_MISMATCH, "{sql}");
        assert_eq!(e.message, message);
    }
    Ok(())
}

/// Unlike `COALESCE`, these need an *ordering* on the resolved type: `json` has
/// no comparison function at all, and `xid` has equality without one.
#[test]
fn a_type_with_no_comparison_function_is_42883() -> anyhow::Result<()> {
    for (sql, ty_name) in [
        ("SELECT GREATEST(name::json, name::json) FROM t", "json"),
        ("SELECT LEAST(name::xid, name::xid) FROM t", "xid"),
        ("SELECT GREATEST(name::point, name::point) FROM t", "point"),
    ] {
        let e = bind_err(sql)?;
        assert_eq!(e.code, sqlstate::UNDEFINED_FUNCTION, "{sql}");
        assert_eq!(
            e.message,
            format!("could not identify a comparison function for type {ty_name}")
        );
    }
    Ok(())
}

#[test]
fn an_empty_argument_list_is_a_syntax_error() -> anyhow::Result<()> {
    for sql in ["SELECT GREATEST()", "SELECT LEAST()"] {
        let e = bind_err(sql)?;
        assert_eq!(e.code, sqlstate::SYNTAX_ERROR, "{sql}");
        assert_eq!(e.message, "syntax error at or near \")\"");
    }
    Ok(())
}

/// Both are keywords in PG's grammar, so any other spelling is an ordinary
/// function lookup that finds nothing.
#[test]
fn only_a_bare_keyword_reaches_the_special_form() -> anyhow::Result<()> {
    for sql in [
        "SELECT pg_catalog.greatest(id, 0) FROM t",
        "SELECT \"greatest\"(id, 0) FROM t",
        "SELECT \"least\"(id, 0) FROM t",
    ] {
        let e = bind_err(sql)?;
        assert_eq!(e.code, sqlstate::UNDEFINED_FUNCTION, "{sql}");
    }
    assert_eq!(
        first_column("SELECT GrEaTeSt(id, 0) FROM t")?.0.name,
        "greatest"
    );
    Ok(())
}

#[test]
fn a_decorated_call_is_a_syntax_error() -> anyhow::Result<()> {
    for (sql, token) in [
        ("SELECT greatest(id, 0) OVER () FROM t", "over"),
        ("SELECT least(id, 0) FILTER (WHERE true) FROM t", "filter"),
        ("SELECT greatest(DISTINCT id, 0) FROM t", "distinct"),
        ("SELECT least(x => id) FROM t", "=>"),
    ] {
        let e = bind_err(sql)?;
        assert_eq!(e.code, sqlstate::SYNTAX_ERROR, "{sql}");
        assert_eq!(e.message, format!("syntax error at or near \"{token}\""));
    }
    Ok(())
}

/// In PG a bare `*` is a grammar error while `t.*` is a whole-row reference
/// yielding a `record`, which this engine cannot represent. `COALESCE` is here
/// because the check it exercises is shared by every special form.
#[test]
fn a_bare_star_is_a_syntax_error_and_a_row_wildcard_is_unsupported() -> anyhow::Result<()> {
    for sql in ["SELECT GREATEST(*) FROM t", "SELECT LEAST(*) FROM t"] {
        let e = bind_err(sql)?;
        assert_eq!(e.code, sqlstate::SYNTAX_ERROR, "{sql}");
        assert_eq!(e.message, "syntax error at or near \"*\"");
    }
    for sql in [
        "SELECT GREATEST(t.*) FROM t",
        "SELECT LEAST(t.*, t.*) FROM t",
        "SELECT COALESCE(t.*) FROM t",
    ] {
        let e = bind_err(sql)?;
        assert_eq!(e.code, sqlstate::FEATURE_NOT_SUPPORTED, "{sql}");
        assert_eq!(e.message, "whole-row references are not supported yet");
    }
    Ok(())
}

#[test]
fn conflicting_explicit_collations_are_42p22() -> anyhow::Result<()> {
    let e = bind_err("SELECT GREATEST(name COLLATE \"C\", name COLLATE \"POSIX\") FROM t")?;
    assert_eq!(e.code, sqlstate::INDETERMINATE_COLLATION);
    assert_eq!(
        e.message,
        "collation mismatch between explicit collations \"C\" and \"POSIX\""
    );
    Ok(())
}

/// The ordering runs under the collation, so an argument's `COLLATE` has to reach
/// the node rather than stop at the argument.
#[test]
fn the_node_carries_the_derived_collation() -> anyhow::Result<()> {
    let (_, expr) = first_column("SELECT GREATEST(name, name COLLATE \"C\") FROM t")?;
    let BoundExpr::MinMax { collation, .. } = expr else {
        bail!("expected MinMax");
    };
    assert_eq!(crate::collation_name(collation), "C");
    let (_, plain) = first_column("SELECT GREATEST(name, 'b') FROM t")?;
    let BoundExpr::MinMax { collation, .. } = plain else {
        bail!("expected MinMax");
    };
    assert_eq!(collation, DEFAULT_COLLATION_OID);
    Ok(())
}
