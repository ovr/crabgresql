//! Literal typing: numbers, bit strings, floats and casts.

use super::common::*;

#[test]
fn out_of_range_literal_is_22003_not_22p02() {
    let e = bind_err("SELECT id FROM t WHERE id = '3000000000'");
    assert_eq!(e.code, "22003");
    assert_eq!(
        e.message,
        "value \"3000000000\" is out of range for type integer"
    );
    // Malformed input keeps 22P02.
    let e = bind_err("SELECT id FROM t WHERE id = '30x'");
    assert_eq!(e.code, "22P02");
}

#[test]
fn constant_assignment_range_checks_at_bind_time() {
    // PG const-folds the cast during planning: the error fires even when
    // no row would match.
    let e = bind_err("UPDATE t SET id = 2147483648");
    assert_eq!(e.code, "22003");
    assert_eq!(e.message, "integer out of range");
}

#[test]
fn bool_literals_accept_pg_prefixes() -> anyhow::Result<()> {
    for (sql, expected) in [
        ("UPDATE t SET flag = 'tru'", Value::Bool(true)),
        ("UPDATE t SET flag = 'of'", Value::Bool(false)),
        ("UPDATE t SET flag = 'ye'", Value::Bool(true)),
        ("UPDATE t SET flag = 'N'", Value::Bool(false)),
    ] {
        let LogicalPlan::Update(UpdatePlan { assignments, .. }) = bind_one(sql)? else {
            panic!("expected Update for: {sql}");
        };
        assert_eq!(
            assignments[0].1,
            BoundExpr::Const {
                value: expected,
                ty: PgType::Bool
            },
            "{sql}"
        );
    }
    // A bare "o" is ambiguous between on and off.
    let e = bind_err("UPDATE t SET flag = 'o'");
    assert_eq!(e.code, "22P02");

    Ok(())
}

#[test]
fn arithmetic_on_non_numeric_with_unknown_is_42883() {
    let e = bind_err("SELECT flag + 'x' FROM t");
    assert_eq!(e.code, "42883");
    assert_eq!(e.message, "operator does not exist: boolean + unknown");
    let e = bind_err("SELECT 'x' + name FROM t");
    assert_eq!(e.code, "42883");
    assert_eq!(e.message, "operator does not exist: unknown + text");
}

#[test]
fn decimal_literal_binds_as_numeric() {
    let ValuesPlan { rows, .. } = bound_values("SELECT 1.5");
    let BoundExpr::Const {
        value: Value::Numeric(n),
        ty: PgType::Numeric,
    } = &rows[0][0]
    else {
        panic!("expected numeric const, got {:?}", rows[0][0]);
    };
    assert_eq!(n.to_display(), "1.5");
}

#[test]
fn hex_literal_binds_as_bit() {
    // X'...' is a bit(n) value with n = 4 * hex digits, MSB-first bytes.
    assert_eq!(
        one_projection("SELECT X'00000001'"),
        BoundExpr::Const {
            value: Value::Bit {
                len: 32,
                data: vec![0, 0, 0, 1]
            },
            ty: PgType::Bit
        }
    );
    // Lowercase hex parses too.
    assert_eq!(
        one_projection("SELECT X'ff'"),
        BoundExpr::Const {
            value: Value::Bit {
                len: 8,
                data: vec![0xff]
            },
            ty: PgType::Bit
        }
    );
}

#[test]
fn wide_bit_literal_binds() {
    // Arbitrary width is supported (68 bits, past the old u64 backing).
    let BoundExpr::Const {
        value: Value::Bit { len, .. },
        ty: PgType::Bit,
    } = one_projection("SELECT X'FFFFFFFFFFFFFFFFF'")
    else {
        panic!("expected bit const");
    };
    assert_eq!(len, 68);
}

#[test]
fn hex_literal_with_bad_digit_is_data_exception() {
    // PG's bit_in reports 22P02 naming the first non-hex character; a leading
    // sign (which u64::from_str_radix would accept) is rejected the same way.
    for (sql, bad) in [
        ("SELECT X'GG'", "G"),
        ("SELECT X'+1'", "+"),
        ("SELECT X'-1'", "-"),
        ("SELECT X'1 2'", " "),
    ] {
        let e = bind_err(sql);
        assert_eq!(e.code, "22P02", "{sql}");
        assert_eq!(
            e.message,
            format!("\"{bad}\" is not a valid hexadecimal digit")
        );
    }
}

#[test]
fn empty_hex_literal_binds_as_zero_width_bit() {
    assert_eq!(
        one_projection("SELECT X''"),
        BoundExpr::Const {
            value: Value::Bit {
                len: 0,
                data: vec![]
            },
            ty: PgType::Bit
        }
    );
}

#[test]
fn order_by_on_bit_binds() {
    // `bit` now has an executor comparison, so ORDER BY on it binds.
    assert!(bind_one("SELECT X'FF' ORDER BY 1").is_ok());
}

#[test]
fn float_literal_cast_binds() {
    let ValuesPlan { rows, .. } = bound_values("SELECT 'NaN'::float4");
    let BoundExpr::Const {
        value: Value::Float4(v),
        ty: PgType::Float4,
    } = &rows[0][0]
    else {
        panic!("expected float4 const, got {:?}", rows[0][0]);
    };
    assert!(v.is_nan());
}

#[test]
fn bad_float_literal_carries_position() {
    let e = bind_err("SELECT 'xyz'::float4");
    assert_eq!(e.code, "22P02");
    assert_eq!(e.message, "invalid input syntax for type real: \"xyz\"");
    assert!(e.location.is_some());
}

#[test]
fn float_to_int_cast_overflow_is_22003_without_position() {
    let e = bind_err("SELECT '32767.6'::float4::int2");
    assert_eq!(e.code, "22003");
    assert_eq!(e.message, "smallint out of range");
    assert!(e.location.is_none());
}

#[test]
fn float_out_of_range_literal_has_position() {
    let e = bind_err("SELECT '10e70'::float4");
    assert_eq!(e.code, "22003");
    assert_eq!(e.message, "\"10e70\" is out of range for type real");
    assert!(e.location.is_some());
}

#[test]
fn float_modulo_is_rejected() {
    // `%` exists for the integer types and numeric, but not float.
    let e = bind_err("SELECT '1.5'::float8 % '2.0'::float8");
    assert_eq!(e.code, "42883");
    assert_eq!(
        e.message,
        "operator does not exist: double precision % double precision"
    );
}

#[test]
fn numeric_operators_bind() {
    // Comparison, arithmetic, and modulo all resolve for numeric now.
    assert!(bind_one("SELECT '1'::numeric < '2'::numeric").is_ok());
    let ValuesPlan { rows, .. } = bound_values("SELECT 1.5 + 2.25");
    assert_eq!(rows[0][0].ty(), PgType::Numeric);
    assert!(bind_one("SELECT 5.5 % 2.0").is_ok());
}

#[test]
fn int2_arithmetic_binds() {
    let ValuesPlan { rows, .. } = bound_values("SELECT '1'::int2 + '2'::int2");
    assert_eq!(rows[0][0].ty(), PgType::Int2);
}

#[test]
fn implicit_int_to_float4_function_arg_resolves() {
    // float4send(integer) works via the implicit int4->float4 cast.
    assert!(bind_one("SELECT float4send(1)").is_ok());
}

#[test]
fn cast_keeps_bare_column_name() {
    let QueryPlan { columns, .. } = bound_query("SELECT id::int8 FROM t");
    assert_eq!(columns[0].name, "id");
    // A constant/nested cast falls back to the target type name.
    let ValuesPlan { columns, .. } = bound_values("SELECT 'nan'::numeric::float4");
    assert_eq!(columns[0].name, "float4");
}
