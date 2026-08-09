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
    // A bare `char` is `char(1)` and blank-pads a short constant.
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

#[test]
fn output_column_names_follow_pg() -> anyhow::Result<()> {
    let QueryPlan { columns, .. } =
        bound_query("SELECT id, (name), id + 1 AS next, id + 1, true FROM t")?;
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["id", "name", "next", "?column?", "bool"]);
    Ok(())
}
