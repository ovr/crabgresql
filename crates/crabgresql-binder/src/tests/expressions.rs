//! Column resolution, operators and untyped-literal typing.

use super::common::*;

#[test]
fn resolves_columns_to_indices() -> anyhow::Result<()> {
    let QueryPlan { projections, .. } = bound_query("SELECT name, id FROM t")?;
    assert_eq!(
        projections,
        vec![
            BoundExpr::ColumnRef {
                index: 2,
                ty: PgType::Text
            },
            BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4
            },
        ]
    );
    Ok(())
}

#[test]
fn unknown_column_is_42703() -> anyhow::Result<()> {
    let e = bind_err("SELECT nope FROM t")?;
    assert_eq!(e.code, "42703");
    assert_eq!(e.message, "column \"nope\" does not exist");
    Ok(())
}

#[test]
fn string_concat_lowers_to_text_concat() -> anyhow::Result<()> {
    let expr = one_projection("SELECT 'a' || 'b'")?;
    assert!(matches!(
        expr,
        BoundExpr::FuncCall {
            func: crate::ScalarFn::TextConcat,
            ret: PgType::Text,
            ..
        }
    ));
    Ok(())
}

#[test]
fn concat_of_two_non_text_is_undefined_operator() -> anyhow::Result<()> {
    let e = bind_err("SELECT 1 || 2")?;
    assert_eq!(e.code, "42883");
    assert_eq!(e.message, "operator does not exist: integer || integer");
    Ok(())
}

#[test]
fn like_binds_to_bool_and_not_wraps() -> anyhow::Result<()> {
    assert_eq!(one_projection("SELECT 'a' LIKE 'a%'")?.ty(), PgType::Bool);
    assert!(matches!(
        one_projection("SELECT 'a' NOT LIKE 'b%'")?,
        BoundExpr::Unary {
            op: crate::UnaryOp::Not,
            ..
        }
    ));
    Ok(())
}

#[test]
fn char_types_carry_their_type_and_length() -> anyhow::Result<()> {
    assert_eq!(
        one_projection("SELECT 'abcdef'::varchar(3)")?.ty(),
        PgType::Varchar
    );
    // `char(3)` truncates a constant at bind time (explicit-cast semantics).
    assert_eq!(
        one_projection("SELECT 'abcdef'::char(3)")?,
        BoundExpr::Const {
            value: Value::Text("abc".into()),
            ty: PgType::Bpchar
        }
    );
    // `char(n)` blank-pads a short constant out to the declared length.
    assert_eq!(
        one_projection("SELECT 'a'::char(3)")?,
        BoundExpr::Const {
            value: Value::Text("a  ".into()),
            ty: PgType::Bpchar
        }
    );
    Ok(())
}

#[test]
fn substring_and_position_desugar_to_functions() -> anyhow::Result<()> {
    assert_eq!(
        one_projection("SELECT substring('abc' FROM 2 FOR 1)")?.ty(),
        PgType::Text
    );
    assert_eq!(
        one_projection("SELECT position('b' IN 'abc')")?.ty(),
        PgType::Int4
    );
    assert_eq!(one_projection("SELECT length('abc')")?.ty(), PgType::Int4);
    Ok(())
}

#[test]
fn length_family_resolves_a_bytea_argument_to_the_bytea_overload() -> anyhow::Result<()> {
    // The argument type is what separates the two overloads at runtime: text
    // counts characters, bytea counts bytes. A bare literal must stay text.
    let arg_ty = |sql: &str| -> anyhow::Result<PgType> {
        let BoundExpr::FuncCall { args, .. } = one_projection(sql)? else {
            bail!("expected a function call");
        };
        Ok(args[0].ty())
    };
    assert_eq!(arg_ty("SELECT length('привет')")?, PgType::Text);
    assert_eq!(arg_ty("SELECT length('привет'::bytea)")?, PgType::Bytea);
    assert_eq!(
        arg_ty("SELECT octet_length('\\x001000'::bytea)")?,
        PgType::Bytea
    );
    assert_eq!(arg_ty("SELECT bit_length('abc'::bytea)")?, PgType::Bytea);
    for sql in [
        "SELECT length('a'::bytea)",
        "SELECT octet_length('a'::bytea)",
        "SELECT bit_length('a'::bytea)",
    ] {
        assert_eq!(one_projection(sql)?.ty(), PgType::Int4);
    }
    Ok(())
}

#[test]
fn qualified_column_uses_table_name_or_alias() -> anyhow::Result<()> {
    assert!(bound("SELECT t.id FROM t").is_ok());
    assert!(bound("SELECT x.id FROM t AS x").is_ok());
    // With an alias the bare table name is no longer a valid qualifier.
    let e = bind_err("SELECT t.id FROM t AS x")?;
    assert_eq!(e.code, "42P01");
    assert_eq!(e.message, "missing FROM-clause entry for table \"t\"");
    Ok(())
}

#[test]
fn where_must_be_boolean() -> anyhow::Result<()> {
    let e = bind_err("SELECT id FROM t WHERE 1")?;
    assert_eq!(e.code, "42804");
    assert_eq!(
        e.message,
        "argument of WHERE must be type boolean, not type integer"
    );
    Ok(())
}

#[test]
fn int4_int8_comparison_promotes_via_coerce() -> anyhow::Result<()> {
    let QueryPlan { predicate, .. } = bound_query("SELECT id FROM t WHERE id = big")?;
    let Some(BoundExpr::Binary {
        op: BinOp::Eq,
        arg_ty: PgType::Int8,
        left,
        ..
    }) = predicate
    else {
        bail!("expected int8 equality");
    };
    assert_eq!(
        *left,
        BoundExpr::Coerce {
            expr: Box::new(BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4
            }),
            ty: PgType::Int8,
        }
    );
    Ok(())
}

#[test]
fn unknown_literal_takes_type_from_other_side() -> anyhow::Result<()> {
    let QueryPlan { predicate, .. } = bound_query("SELECT id FROM t WHERE big = '5'")?;
    let Some(BoundExpr::Binary { arg_ty, right, .. }) = predicate else {
        bail!("expected comparison");
    };
    assert_eq!(arg_ty, PgType::Int8);
    assert_eq!(
        *right,
        BoundExpr::Const {
            value: Value::Int8(5),
            ty: PgType::Int8
        }
    );
    Ok(())
}

#[test]
fn between_desugars_to_gte_and_lte() -> anyhow::Result<()> {
    let QueryPlan { predicate, .. } = bound_query("SELECT id FROM t WHERE id BETWEEN 1 AND 3")?;
    // `x BETWEEN low AND high` -> `(x >= low) AND (x <= high)`.
    let Some(BoundExpr::Binary {
        op: BinOp::And,
        left,
        right,
        ..
    }) = predicate
    else {
        bail!("expected AND of two comparisons");
    };
    assert!(matches!(
        *left,
        BoundExpr::Binary {
            op: BinOp::GtEq,
            ..
        }
    ));
    assert!(matches!(
        *right,
        BoundExpr::Binary {
            op: BinOp::LtEq,
            ..
        }
    ));
    Ok(())
}

#[test]
fn not_between_desugars_to_lt_or_gt() -> anyhow::Result<()> {
    let QueryPlan { predicate, .. } = bound_query("SELECT id FROM t WHERE id NOT BETWEEN 1 AND 3")?;
    // `x NOT BETWEEN low AND high` -> `(x < low) OR (x > high)`.
    let Some(BoundExpr::Binary {
        op: BinOp::Or,
        left,
        right,
        ..
    }) = predicate
    else {
        bail!("expected OR of two comparisons");
    };
    assert!(matches!(*left, BoundExpr::Binary { op: BinOp::Lt, .. }));
    assert!(matches!(*right, BoundExpr::Binary { op: BinOp::Gt, .. }));
    Ok(())
}

#[test]
fn between_reports_low_side_error_first() -> anyhow::Result<()> {
    // PG analyzes `(id >= low) AND (id <= high)` left-to-right and fully
    // resolves the low comparison — coercing the bad literal — before it
    // ever looks at the high bound. The low-side 22P02 must win over the
    // undefined-column 42703 the high bound would otherwise raise.
    let e = bind_err("SELECT id FROM t WHERE id BETWEEN 'notint' AND missingcol")?;
    assert_eq!(e.code, "22P02");
    assert_eq!(
        e.message,
        "invalid input syntax for type integer: \"notint\""
    );
    Ok(())
}

#[test]
fn unparsable_unknown_literal_is_22p02() -> anyhow::Result<()> {
    let e = bind_err("SELECT id FROM t WHERE id = 'abc'")?;
    assert_eq!(e.code, "22P02");
    assert_eq!(e.message, "invalid input syntax for type integer: \"abc\"");
    Ok(())
}

#[test]
fn unknown_vs_unknown_comparison_falls_back_to_text() -> anyhow::Result<()> {
    let ValuesPlan { rows, .. } = bound_values("SELECT 'a' = 'b'")?;
    assert_eq!(rows.len(), 1);
    let BoundExpr::Binary { arg_ty, .. } = &rows[0][0] else {
        bail!("expected comparison");
    };
    assert_eq!(*arg_ty, PgType::Text);
    Ok(())
}

#[test]
fn unknown_arithmetic_is_ambiguous_42725() -> anyhow::Result<()> {
    // Like PG, every 42725 "operator is not unique" carries the same
    // DETAIL/HINT and a cursor on the operator.
    let e = bind_err("SELECT '1' + '2'")?;
    assert_eq!(e.code, "42725");
    assert_eq!(e.message, "operator is not unique: unknown + unknown");
    assert_eq!(
        e.detail.as_deref(),
        Some("Could not choose a best candidate operator.")
    );
    assert_eq!(
        e.hint.as_deref(),
        Some("You might need to add explicit type casts.")
    );
    // Cursor at the `+` (1-based column 12).
    assert_eq!(e.location, Some((1, 12)));
    Ok(())
}

#[test]
fn unary_on_untyped_literal_is_ambiguous_42725() -> anyhow::Result<()> {
    // `- unknown` / `~ unknown` are ambiguous in PG with the same DETAIL/HINT.
    let e = bind_err("SELECT - NULL")?;
    assert_eq!(e.code, "42725");
    assert_eq!(e.message, "operator is not unique: - unknown");
    assert_eq!(
        e.detail.as_deref(),
        Some("Could not choose a best candidate operator.")
    );
    assert_eq!(
        e.hint.as_deref(),
        Some("You might need to add explicit type casts.")
    );
    Ok(())
}

#[test]
fn time_plus_time_is_ambiguous_42725() -> anyhow::Result<()> {
    // PG cannot pick a best `+` candidate for `time + time`, so it reports
    // ambiguity (with DETAIL/HINT) and points the cursor at the operator —
    // unlike `timetz + timetz` / `time * time`, which are 42883.
    let e = bind_err("SELECT time '00:01' + time '00:02'")?;
    assert_eq!(e.code, "42725");
    assert_eq!(
        e.message,
        "operator is not unique: time without time zone + time without time zone"
    );
    assert_eq!(
        e.detail.as_deref(),
        Some("Could not choose a best candidate operator.")
    );
    assert_eq!(
        e.hint.as_deref(),
        Some("You might need to add explicit type casts.")
    );
    // Cursor at the `+` (1-based column 21).
    assert_eq!(e.location, Some((1, 21)));
    Ok(())
}

#[test]
fn timetz_plus_timetz_stays_undefined_42883() -> anyhow::Result<()> {
    let e = bind_err("SELECT '00:01+00'::timetz + '00:02+00'::timetz")?;
    assert_eq!(e.code, "42883");
    assert_eq!(
        e.message,
        "operator does not exist: time with time zone + time with time zone"
    );
    Ok(())
}

#[test]
fn mismatched_operator_is_42883() -> anyhow::Result<()> {
    let e = bind_err("SELECT id FROM t WHERE name = id")?;
    assert_eq!(e.code, "42883");
    assert_eq!(e.message, "operator does not exist: text = integer");

    let e = bind_err("SELECT name + name FROM t")?;
    assert_eq!(e.code, "42883");
    assert_eq!(e.message, "operator does not exist: text + text");
    Ok(())
}

#[test]
fn logic_operands_must_be_boolean() -> anyhow::Result<()> {
    let e = bind_err("SELECT flag AND id FROM t")?;
    assert_eq!(e.code, "42804");
    assert_eq!(
        e.message,
        "argument of AND must be type boolean, not type integer"
    );
    Ok(())
}

#[test]
fn min_int4_literal_binds_as_int4() -> anyhow::Result<()> {
    let ValuesPlan { rows, columns, .. } = bound_values("SELECT -2147483648")?;
    assert_eq!(
        rows[0][0],
        BoundExpr::Const {
            value: Value::Int4(i32::MIN),
            ty: PgType::Int4
        }
    );
    assert_eq!(columns[0].ty, PgType::Int4);
    Ok(())
}

/// The digest functions take bytea only. A bare literal reaches them through
/// byteain, but a typed text argument must not coerce — otherwise `sha256`
/// would silently hash a value PG refuses to hash.
#[test]
fn digest_functions_reject_a_typed_text_argument() -> anyhow::Result<()> {
    for (sql, message) in [
        (
            "SELECT sha224('abc'::text)",
            "function sha224(text) does not exist",
        ),
        (
            "SELECT sha256('abc'::text)",
            "function sha256(text) does not exist",
        ),
        (
            "SELECT sha384('abc'::text)",
            "function sha384(text) does not exist",
        ),
        (
            "SELECT sha512('abc'::text)",
            "function sha512(text) does not exist",
        ),
        (
            "SELECT crc32('abc'::text)",
            "function crc32(text) does not exist",
        ),
        (
            "SELECT crc32c('abc'::text)",
            "function crc32c(text) does not exist",
        ),
    ] {
        let e = bind_err(sql)?;
        assert_eq!(e.code, sqlstate::UNDEFINED_FUNCTION, "{sql}");
        assert_eq!(e.message, message);
    }
    Ok(())
}

/// A literal that its input function rejects is that error, not "the function
/// does not exist" — PG resolves the overload from types, so once one candidate
/// is the only reachable one, its coercion failure is the answer. Without the
/// single-candidate rule in `resolve_call` these all report 42883, which sends
/// the reader hunting for a missing overload instead of a bad literal.
#[test]
fn bad_literal_to_a_single_overload_reports_the_input_error() -> anyhow::Result<()> {
    for (sql, code, message) in [
        (
            "SELECT crc32('\\x4')",
            "22023",
            "invalid hexadecimal data: odd number of digits",
        ),
        (
            "SELECT sha256('\\xzz')",
            "22023",
            "invalid hexadecimal digit: \"z\"",
        ),
        (
            "SELECT sha512('\\9')",
            "22P02",
            "invalid input syntax for type bytea",
        ),
    ] {
        let e = bind_err(sql)?;
        assert_eq!(e.code, code, "{sql}");
        assert_eq!(e.message, message, "{sql}");
    }
    // An ambiguous name keeps the overload-resolution error: with more than one
    // candidate PG has not yet committed to an input function.
    let e = bind_err("SELECT to_char('2024-01-01', 'YYYY')")?;
    assert_eq!(e.code, sqlstate::AMBIGUOUS_FUNCTION);
    Ok(())
}

/// The other half of the rule, and the one that is easy to break: when a
/// *typed* argument is why no candidate survived, the call is undefined and the
/// literals are never parsed at all. Reporting a literal's error here would
/// point at an argument PG considers blameless.
#[test]
fn typed_argument_mismatch_outranks_a_bad_literal() -> anyhow::Result<()> {
    for (sql, message) in [
        // `repeat`/`lpad` take text, so the bytea argument alone sinks the
        // signature -- 'x' is never offered to int4's input function.
        (
            "SELECT repeat('a'::bytea, 'x')",
            "function repeat(bytea, unknown) does not exist",
        ),
        (
            "SELECT lpad('a'::bytea, 'x')",
            "function lpad(bytea, unknown) does not exist",
        ),
        // The mismatch can also sit to the right of the bad literal.
        (
            "SELECT encode('\\xzz', 42)",
            "function encode(unknown, integer) does not exist",
        ),
    ] {
        let e = bind_err(sql)?;
        assert_eq!(e.code, sqlstate::UNDEFINED_FUNCTION, "{sql}");
        assert_eq!(e.message, message, "{sql}");
    }
    // With every typed argument fitting, the literal is back in scope.
    let e = bind_err("SELECT repeat('a', 'x')")?;
    assert_eq!(e.code, "22P02");
    assert_eq!(e.message, "invalid input syntax for type integer: \"x\"");
    Ok(())
}

#[test]
fn typed_arguments_that_separate_nothing_are_ambiguous_42725() -> anyhow::Result<()> {
    // `gcd`/`lcm` have int4, int8 and numeric overloads and no smallint one.
    // A smallint reaches all three by implicit cast, none of them is the
    // numeric category's preferred type (float8 is, and is not a candidate),
    // and nothing else separates them — so PG gives up rather than widening to
    // int4, and so do we. Unlike the operator form, the function form puts the
    // whole sentence in HINT and has no DETAIL.
    for (sql, message) in [
        (
            "SELECT gcd(6::int2, 4::int2)",
            "function gcd(smallint, smallint) is not unique",
        ),
        (
            "SELECT lcm(6::int2, 4::int2)",
            "function lcm(smallint, smallint) is not unique",
        ),
    ] {
        let e = bind_err(sql)?;
        assert_eq!(e.code, "42725", "{sql}");
        assert_eq!(e.message, message, "{sql}");
        assert_eq!(e.detail.as_deref(), None, "{sql}");
        assert_eq!(
            e.hint.as_deref(),
            Some(
                "Could not choose a best candidate function. \
                 You might need to add explicit type casts."
            ),
            "{sql}"
        );
    }
    // The widths that *do* have an overload are unaffected.
    assert_eq!(
        one_projection("SELECT gcd(6::int4, 4::int4)")?.ty(),
        PgType::Int4
    );
    assert_eq!(
        one_projection("SELECT gcd(6::int8, 4::int8)")?.ty(),
        PgType::Int8
    );
    Ok(())
}

#[test]
fn abs_keeps_the_argument_type() -> anyhow::Result<()> {
    // PG has one `abs` per numeric type, so the exact match wins over float8 —
    // the category's preferred type, which is what an argument with no exact
    // overload would otherwise land on.
    for (sql, ty) in [
        ("SELECT abs(-3::int2)", PgType::Int2),
        ("SELECT abs(-3::int4)", PgType::Int4),
        ("SELECT abs(-3::int8)", PgType::Int8),
        ("SELECT abs(-3::float4)", PgType::Float4),
        ("SELECT abs(-3::float8)", PgType::Float8),
        ("SELECT abs(-3::numeric)", PgType::Numeric),
    ] {
        assert_eq!(one_projection(sql)?.ty(), ty, "{sql}");
    }
    Ok(())
}

#[test]
fn output_column_names_follow_pg() -> anyhow::Result<()> {
    let QueryPlan { columns, .. } =
        bound_query("SELECT id, (name), id + 1 AS next, id + 1, true FROM t")?;
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["id", "name", "next", "?column?", "bool"]);
    Ok(())
}

#[test]
fn index_property_functions_take_regclass_or_oid() -> anyhow::Result<()> {
    // Two entries in the signature table could not accept both spellings — an
    // unknown literal would fit both and raise 42725 — so the parameter types are
    // tried in order, `regclass` first, which keeps a literal resolving by *name*.
    for (sql, want) in [
        (
            "SELECT pg_index_has_property('t_pkey', 'index_scan')",
            PgType::Reg(crabgresql_types::RegKind::Class),
        ),
        (
            "SELECT pg_index_has_property('t_pkey'::regclass, 'index_scan')",
            PgType::Reg(crabgresql_types::RegKind::Class),
        ),
        (
            "SELECT pg_index_has_property(16384::oid, 'index_scan')",
            PgType::Oid,
        ),
    ] {
        let ValuesPlan { rows, .. } = bound_values(sql)?;
        let BoundExpr::FuncCall { func, ret, args } = &rows[0][0] else {
            bail!("expected a function call for `{sql}`");
        };
        assert_eq!(*func, crate::ScalarFn::PgIndexHasProperty, "for `{sql}`");
        assert_eq!(*ret, PgType::Bool, "for `{sql}`");
        assert_eq!(args[0].ty(), want, "for `{sql}`");
        assert_eq!(args[1].ty(), PgType::Text, "for `{sql}`");
    }
    // The three-argument sibling resolves the same way.
    let ValuesPlan { rows, .. } =
        bound_values("SELECT pg_index_column_has_property(16384::oid, 1, 'asc')")?;
    let BoundExpr::FuncCall { func, args, .. } = &rows[0][0] else {
        bail!("expected a function call");
    };
    assert_eq!(*func, crate::ScalarFn::PgIndexColumnHasProperty);
    assert_eq!(args[1].ty(), PgType::Int4);
    // A wrong arity is still 42883 rather than a mis-resolved overload.
    let e = bind_err("SELECT pg_index_has_property('t_pkey')")?;
    assert_eq!(e.code, "42883");
    Ok(())
}
