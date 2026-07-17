//! Value-level cast machinery shared by the binder (bind-time const folding)
//! and the executor (runtime `Coerce`). Clean-room (see AGENTS.md): this
//! reproduces PG's *observable* cast results — including the SQLSTATE/message on
//! range errors — as pinned by the regression corpus, implemented independently.

use crate::{NumericVal, PgType, Value, float, parse_bool};

/// SQLSTATE + message for a failed cast.
#[derive(Clone, Debug, PartialEq)]
pub struct CastError {
    pub sqlstate: &'static str,
    pub message: String,
}

const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
const CANNOT_COERCE: &str = "42846";
const INVALID_TEXT_REPRESENTATION: &str = "22P02";
const FEATURE_NOT_SUPPORTED: &str = "0A000";

fn out_of_range(ty: PgType) -> CastError {
    CastError {
        sqlstate: NUMERIC_VALUE_OUT_OF_RANGE,
        message: format!("{} out of range", ty.name()),
    }
}

fn cannot_coerce(from: PgType, to: PgType) -> CastError {
    CastError {
        sqlstate: CANNOT_COERCE,
        message: format!("cannot cast type {} to {}", from.name(), to.name()),
    }
}

/// `22P02` — an input function rejected the text (`'abc'::int4`).
fn invalid_input(ty: PgType, s: &str) -> CastError {
    CastError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("invalid input syntax for type {}: \"{s}\"", ty.name()),
    }
}

/// `22003` on the text→int path, which prints the offending literal (unlike the
/// bare `out_of_range` PG uses for arithmetic and numeric→int overflow).
fn value_out_of_range(ty: PgType, s: &str) -> CastError {
    CastError {
        sqlstate: NUMERIC_VALUE_OUT_OF_RANGE,
        message: format!("value \"{s}\" is out of range for type {}", ty.name()),
    }
}

/// `0A000` — a `NaN`/`infinity` numeric has no integer image
/// (`'NaN'::numeric::int` → "cannot convert NaN to integer").
fn cannot_convert(what: &str, ty: PgType) -> CastError {
    CastError {
        sqlstate: FEATURE_NOT_SUPPORTED,
        message: format!("cannot convert {what} to {}", ty.name()),
    }
}

/// Round-half-to-even (rint) then bounds-check `[lo, hi)` — reproduces PG's
/// observable float→integer conversion, where the upper bound is exclusive at
/// the power of two (e.g. `2147483647::float4::int4` errors). Returns the
/// rounded integral f64.
fn float_to_int_bounds(v: f64, lo: f64, hi: f64, ty: PgType) -> Result<f64, CastError> {
    let r = v.round_ties_even();
    if r.is_nan() || r < lo || r >= hi {
        return Err(out_of_range(ty));
    }
    Ok(r)
}

/// Cast `v` to `to`. `efd` (extra_float_digits) only affects float→text.
pub fn cast_value(v: Value, to: PgType, efd: i32) -> Result<Value, CastError> {
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    let from = v.pg_type().expect("non-null value has a type");
    if from == to {
        return Ok(v);
    }
    match (&v, to) {
        // ---- integer widening / narrowing ----
        (Value::Int2(n), PgType::Int4) => Ok(Value::Int4(*n as i32)),
        (Value::Int2(n), PgType::Int8) => Ok(Value::Int8(*n as i64)),
        (Value::Int4(n), PgType::Int8) => Ok(Value::Int8(*n as i64)),
        (Value::Int4(n), PgType::Int2) => i16::try_from(*n)
            .map(Value::Int2)
            .map_err(|_| out_of_range(PgType::Int2)),
        (Value::Int8(n), PgType::Int2) => i16::try_from(*n)
            .map(Value::Int2)
            .map_err(|_| out_of_range(PgType::Int2)),
        (Value::Int8(n), PgType::Int4) => i32::try_from(*n)
            .map(Value::Int4)
            .map_err(|_| out_of_range(PgType::Int4)),

        // ---- integer → float ----
        (Value::Int2(n), PgType::Float4) => Ok(Value::Float4(*n as f32)),
        (Value::Int2(n), PgType::Float8) => Ok(Value::Float8(*n as f64)),
        (Value::Int4(n), PgType::Float4) => Ok(Value::Float4(*n as f32)),
        (Value::Int4(n), PgType::Float8) => Ok(Value::Float8(*n as f64)),
        (Value::Int8(n), PgType::Float4) => Ok(Value::Float4(*n as f32)),
        (Value::Int8(n), PgType::Float8) => Ok(Value::Float8(*n as f64)),

        // ---- float widening / narrowing ----
        (Value::Float4(f), PgType::Float8) => Ok(Value::Float8(*f as f64)),
        (Value::Float8(f), PgType::Float4) => {
            let r = *f as f32;
            if r.is_infinite() && !f.is_infinite() {
                return Err(CastError {
                    sqlstate: NUMERIC_VALUE_OUT_OF_RANGE,
                    message: "value out of range: overflow".into(),
                });
            }
            if r == 0.0 && *f != 0.0 {
                return Err(CastError {
                    sqlstate: NUMERIC_VALUE_OUT_OF_RANGE,
                    message: "value out of range: underflow".into(),
                });
            }
            Ok(Value::Float4(r))
        }

        // ---- float → integer (rint + range check) ----
        (Value::Float4(f), PgType::Int2) => {
            float_to_int_bounds(*f as f64, -32768.0, 32768.0, PgType::Int2)
                .map(|r| Value::Int2(r as i16))
        }
        (Value::Float8(f), PgType::Int2) => {
            float_to_int_bounds(*f, -32768.0, 32768.0, PgType::Int2).map(|r| Value::Int2(r as i16))
        }
        (Value::Float4(f), PgType::Int4) => {
            float_to_int_bounds(*f as f64, -2147483648.0, 2147483648.0, PgType::Int4)
                .map(|r| Value::Int4(r as i32))
        }
        (Value::Float8(f), PgType::Int4) => {
            float_to_int_bounds(*f, -2147483648.0, 2147483648.0, PgType::Int4)
                .map(|r| Value::Int4(r as i32))
        }
        (Value::Float4(f), PgType::Int8) => float_to_int_bounds(
            *f as f64,
            -9223372036854775808.0,
            9223372036854775808.0,
            PgType::Int8,
        )
        .map(|r| Value::Int8(r as i64)),
        (Value::Float8(f), PgType::Int8) => float_to_int_bounds(
            *f,
            -9223372036854775808.0,
            9223372036854775808.0,
            PgType::Int8,
        )
        .map(|r| Value::Int8(r as i64)),

        // ---- anything → text (float uses efd) ----
        (_, PgType::Text) => Ok(Value::Text(
            v.encode_text_with(efd).unwrap_or_default(),
        )),

        // ---- text → scalar (input functions) ----
        (Value::Text(s), PgType::Float4) => float::float4in(s)
            .map(Value::Float4)
            .map_err(|e| CastError { sqlstate: e.sqlstate, message: e.message }),
        (Value::Text(s), PgType::Float8) => float::float8in(s)
            .map(Value::Float8)
            .map_err(|e| CastError { sqlstate: e.sqlstate, message: e.message }),
        (Value::Text(s), PgType::Numeric) => {
            NumericVal::parse(s).map(Value::Numeric).ok_or_else(|| CastError {
                sqlstate: "22P02",
                message: format!("invalid input syntax for type numeric: \"{s}\""),
            })
        }

        // ---- numeric → float ----
        (Value::Numeric(n), PgType::Float4) => Ok(Value::Float4(numeric_to_f64(n) as f32)),
        (Value::Numeric(n), PgType::Float8) => Ok(Value::Float8(numeric_to_f64(n))),

        // ---- text → integer (int input functions) ----
        (Value::Text(s), PgType::Int2 | PgType::Int4 | PgType::Int8) => text_to_int(s, to),

        // ---- text → boolean (boolin) ----
        (Value::Text(s), PgType::Bool) => parse_bool(s)
            .map(Value::Bool)
            .ok_or_else(|| invalid_input(PgType::Bool, s)),

        // ---- integer → numeric (exact) ----
        (Value::Int2(n), PgType::Numeric) => Ok(Value::Numeric(NumericVal::Finite(n.to_string()))),
        (Value::Int4(n), PgType::Numeric) => Ok(Value::Numeric(NumericVal::Finite(n.to_string()))),
        (Value::Int8(n), PgType::Numeric) => Ok(Value::Numeric(NumericVal::Finite(n.to_string()))),

        // ---- float → numeric ----
        // PG's float→numeric keeps DBL_DIG (15) / FLT_DIG (6) significant digits
        // and always prints numeric in plain decimal.
        (Value::Float4(f), PgType::Numeric) => Ok(Value::Numeric(float_to_numeric(*f as f64, 6))),
        (Value::Float8(f), PgType::Numeric) => Ok(Value::Numeric(float_to_numeric(*f, 15))),

        // ---- numeric → integer (round half away from zero + range check) ----
        (Value::Numeric(n), PgType::Int2 | PgType::Int4 | PgType::Int8) => numeric_to_int(n, to),

        // ---- bit-string → integer (two's-complement of the target width) ----
        (Value::Bit { len, bits }, PgType::Int2 | PgType::Int4 | PgType::Int8) => {
            bit_to_int(*len, *bits, to)
        }

        _ => Err(cannot_coerce(from, to)),
    }
}

fn numeric_to_f64(n: &NumericVal) -> f64 {
    match n {
        NumericVal::NaN => f64::NAN,
        NumericVal::Finite(s) => s.parse().unwrap_or(f64::NAN),
    }
}

/// `int2in`/`int4in`/`int8in`: trim, base-10, optional sign. A well-formed
/// number that does not fit is `22003` (printing the literal); anything else is
/// `22P02`. The error's type name comes from `ty`, so int2/int4/int8 print
/// smallint/integer/bigint.
// TODO: the binder's `parse_unknown` runs the same int input rules with a
// position; a future refactor could share this acceptor across both.
fn text_to_int(s: &str, ty: PgType) -> Result<Value, CastError> {
    use std::num::IntErrorKind;
    let map = |e: std::num::ParseIntError| match e.kind() {
        IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => value_out_of_range(ty, s),
        _ => invalid_input(ty, s),
    };
    let t = s.trim();
    match ty {
        PgType::Int2 => t.parse::<i16>().map(Value::Int2).map_err(map),
        PgType::Int4 => t.parse::<i32>().map(Value::Int4).map_err(map),
        PgType::Int8 => t.parse::<i64>().map(Value::Int8).map_err(map),
        _ => unreachable!("text_to_int called with {ty:?}"),
    }
}

/// `float8_numeric`/`float4_numeric`: render the float with `sig` significant
/// digits and hand it to numeric, which always prints plain decimal (no
/// exponent). NaN and ±infinity carry through as numeric's own spellings.
fn float_to_numeric(v: f64, sig: usize) -> NumericVal {
    if v.is_nan() {
        return NumericVal::NaN;
    }
    if v.is_infinite() {
        let s = if v < 0.0 { "-Infinity" } else { "Infinity" };
        return NumericVal::Finite(s.to_string());
    }
    if v == 0.0 {
        // Rust would render -0.0 as "-0"; numeric has no signed zero.
        return NumericVal::Finite("0".to_string());
    }
    // `{:.*e}` gives exactly `sig` significant digits (1 before the point,
    // `sig - 1` after); expanding that to plain decimal matches numeric_out,
    // regardless of whether PG's %g would have chosen %e or %f form.
    NumericVal::Finite(sci_to_plain(&format!("{:.*e}", sig - 1, v)))
}

/// Expand Rust scientific notation (`-6.66e-1`) into a plain decimal string,
/// stripping insignificant trailing zeros.
fn sci_to_plain(sci: &str) -> String {
    let (mantissa, exp) = sci.split_once('e').expect("scientific notation");
    let exp: i32 = exp.parse().expect("exponent");
    let neg = mantissa.starts_with('-');
    let mantissa = mantissa.trim_start_matches('-');
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));

    let mut digits: String = int_part.chars().chain(frac_part.chars()).collect();
    // The decimal point sits after `int_part` digits, then shifts by `exp`.
    let point = int_part.len() as i32 + exp;
    // Trailing zeros past the point are insignificant; keep at least one digit.
    let keep = digits.trim_end_matches('0').len().max(1);
    digits.truncate(keep);

    let out = if point <= 0 {
        format!("0.{}{}", "0".repeat((-point) as usize), digits)
    } else if point as usize >= digits.len() {
        format!("{}{}", digits, "0".repeat(point as usize - digits.len()))
    } else {
        let p = point as usize;
        format!("{}.{}", &digits[..p], &digits[p..])
    };
    if neg { format!("-{out}") } else { out }
}

/// `numeric_int2`/`_int4`/`_int8`: round half away from zero, then range-check.
/// NaN/infinity have no integer image (`0A000`); overflow is the bare
/// `<typename> out of range` (`22003`), matching PG's numeric→int messages.
fn numeric_to_int(n: &NumericVal, ty: PgType) -> Result<Value, CastError> {
    let s = match n {
        NumericVal::NaN => return Err(cannot_convert("NaN", ty)),
        NumericVal::Finite(s) => s.as_str(),
    };
    let core = s.trim().strip_prefix(['+', '-']).unwrap_or(s.trim());
    if core.eq_ignore_ascii_case("inf") || core.eq_ignore_ascii_case("infinity") {
        return Err(cannot_convert("infinity", ty));
    }
    let v = round_decimal(s).map_err(|e| match e {
        DecimalError::Overflow => out_of_range(ty),
        // Unreachable for well-formed numerics (NumericVal::parse validated the
        // text); map to out-of-range rather than invent a message.
        DecimalError::Malformed => out_of_range(ty),
    })?;
    let (lo, hi) = match ty {
        PgType::Int2 => (i16::MIN as i128, i16::MAX as i128),
        PgType::Int4 => (i32::MIN as i128, i32::MAX as i128),
        PgType::Int8 => (i64::MIN as i128, i64::MAX as i128),
        _ => unreachable!("numeric_to_int called with {ty:?}"),
    };
    if v < lo || v > hi {
        return Err(out_of_range(ty));
    }
    Ok(match ty {
        PgType::Int2 => Value::Int2(v as i16),
        PgType::Int4 => Value::Int4(v as i32),
        PgType::Int8 => Value::Int8(v as i64),
        _ => unreachable!(),
    })
}

enum DecimalError {
    Overflow,
    Malformed,
}

/// Parse a decimal string (`[±]digits[.digits][(e|E)[±]digits]`) and round to
/// the nearest integer, ties away from zero, into an `i128`. Overflowing the
/// `i128` accumulator is reported as `Overflow` (the caller maps it to the
/// target type's out-of-range error).
fn round_decimal(s: &str) -> Result<i128, DecimalError> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (mantissa, exp) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i32>().map_err(|_| DecimalError::Malformed)?),
        None => (s, 0),
    };
    let (int_str, frac_str) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if (int_str.is_empty() && frac_str.is_empty())
        || !int_str.bytes().all(|b| b.is_ascii_digit())
        || !frac_str.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(DecimalError::Malformed);
    }
    let digits: Vec<u8> = int_str
        .bytes()
        .chain(frac_str.bytes())
        .map(|b| b - b'0')
        .collect();
    // The decimal point sits after `int_str` digits, shifted by the exponent.
    let point = int_str.len() as i32 + exp;

    // Accumulate the integer part (indices 0..point), padding with zeros when
    // the exponent pushes the point past the available digits.
    let mut acc: i128 = 0;
    for i in 0..point.max(0) {
        let d = digits.get(i as usize).copied().unwrap_or(0) as i128;
        acc = acc
            .checked_mul(10)
            .and_then(|a| a.checked_add(d))
            .ok_or(DecimalError::Overflow)?;
    }
    // Round: the first dropped fractional digit decides (ties away from zero,
    // so any first digit >= 5 rounds the magnitude up).
    let first_frac = if point < 0 {
        0
    } else {
        digits.get(point as usize).copied().unwrap_or(0)
    };
    if first_frac >= 5 {
        acc = acc.checked_add(1).ok_or(DecimalError::Overflow)?;
    }
    Ok(if neg { -acc } else { acc })
}

/// Reinterpret a right-aligned bit string as the target integer's two's
/// complement. A bit string wider than the target errors (`<typename> out of
/// range`), matching PG's `bittoint4`/`bittoint8`.
fn bit_to_int(len: u16, bits: u64, ty: PgType) -> Result<Value, CastError> {
    let width = match ty {
        PgType::Int2 => 16,
        PgType::Int4 => 32,
        PgType::Int8 => 64,
        _ => unreachable!("bit_to_int called with {ty:?}"),
    };
    if len as u32 > width {
        return Err(out_of_range(ty));
    }
    Ok(match ty {
        PgType::Int2 => Value::Int2(bits as u16 as i16),
        PgType::Int4 => Value::Int4(bits as u32 as i32),
        PgType::Int8 => Value::Int8(bits as i64),
        _ => unreachable!(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_to_int_edges() {
        assert_eq!(
            cast_value(Value::Float4(32767.4), PgType::Int2, 1).unwrap(),
            Value::Int2(32767)
        );
        assert_eq!(
            cast_value(Value::Float4(32767.6), PgType::Int2, 1).unwrap_err().message,
            "smallint out of range"
        );
        // f32 of 2147483647 rounds up to 2^31, out of int4 range.
        assert_eq!(
            cast_value(Value::Float4(2147483647.0), PgType::Int4, 1).unwrap_err().sqlstate,
            "22003"
        );
        assert_eq!(
            cast_value(Value::Float8(-9223372036854775808.5), PgType::Int8, 1).unwrap(),
            Value::Int8(i64::MIN)
        );
    }

    #[test]
    fn float8_to_float4_range() {
        assert_eq!(
            cast_value(Value::Float8(1e70), PgType::Float4, 1).unwrap_err().message,
            "value out of range: overflow"
        );
        assert_eq!(
            cast_value(Value::Float8(1e-70), PgType::Float4, 1).unwrap_err().message,
            "value out of range: underflow"
        );
    }

    #[test]
    fn numeric_nan_to_float() {
        let n = cast_value(Value::Text("nan".into()), PgType::Numeric, 1).unwrap();
        let f = cast_value(n, PgType::Float4, 1).unwrap();
        assert_eq!(f.encode_text_with(1).as_deref(), Some("NaN"));
    }

    #[test]
    fn numeric_rejects_garbage() {
        let e = cast_value(Value::Text("abc".into()), PgType::Numeric, 1).unwrap_err();
        assert_eq!(e.sqlstate, "22P02");
        assert_eq!(e.message, "invalid input syntax for type numeric: \"abc\"");
        assert!(cast_value(Value::Text("1.5".into()), PgType::Numeric, 1).is_ok());
    }

    fn cast(v: Value, to: PgType) -> Result<Value, CastError> {
        cast_value(v, to, 1)
    }

    #[test]
    fn text_to_int_ok_and_errors() {
        assert_eq!(
            cast(Value::Text("  123 ".into()), PgType::Int4).unwrap(),
            Value::Int4(123)
        );
        assert_eq!(
            cast(Value::Text("-9".into()), PgType::Int8).unwrap(),
            Value::Int8(-9)
        );
        // Malformed (including a decimal) is 22P02, echoing the original text.
        let e = cast(Value::Text("1.5".into()), PgType::Int4).unwrap_err();
        assert_eq!(e.sqlstate, "22P02");
        assert_eq!(e.message, "invalid input syntax for type integer: \"1.5\"");
        // A well-formed but too-large number is 22003 and prints the literal,
        // with the target type's name.
        let e = cast(Value::Text("99999999999".into()), PgType::Int4).unwrap_err();
        assert_eq!(e.sqlstate, "22003");
        assert_eq!(e.message, "value \"99999999999\" is out of range for type integer");
        let e = cast(Value::Text("99999".into()), PgType::Int2).unwrap_err();
        assert_eq!(e.message, "value \"99999\" is out of range for type smallint");
    }

    #[test]
    fn text_to_bool_ok_and_error() {
        assert_eq!(cast(Value::Text("t".into()), PgType::Bool).unwrap(), Value::Bool(true));
        assert_eq!(cast(Value::Text("no".into()), PgType::Bool).unwrap(), Value::Bool(false));
        assert_eq!(cast(Value::Text("on".into()), PgType::Bool).unwrap(), Value::Bool(true));
        let e = cast(Value::Text("x".into()), PgType::Bool).unwrap_err();
        assert_eq!(e.sqlstate, "22P02");
        assert_eq!(e.message, "invalid input syntax for type boolean: \"x\"");
    }

    #[test]
    fn int_to_numeric_is_exact() {
        assert_eq!(
            cast(Value::Int4(5), PgType::Numeric).unwrap(),
            Value::Numeric(NumericVal::Finite("5".into()))
        );
        assert_eq!(
            cast(Value::Int8(-9), PgType::Numeric).unwrap(),
            Value::Numeric(NumericVal::Finite("-9".into()))
        );
    }

    fn num_text(v: Value, to: PgType) -> String {
        cast(v, to).unwrap().encode_text_with(1).unwrap()
    }

    #[test]
    fn float_to_numeric_matches_pg() {
        // 15 significant digits for float8, plain decimal, no exponent.
        assert_eq!(num_text(Value::Float8(1.5), PgType::Numeric), "1.5");
        assert_eq!(num_text(Value::Float8(1.1), PgType::Numeric), "1.1");
        assert_eq!(num_text(Value::Float8(2.0 / 3.0), PgType::Numeric), "0.666666666666667");
        assert_eq!(num_text(Value::Float8(100.0), PgType::Numeric), "100");
        assert_eq!(num_text(Value::Float8(1e20), PgType::Numeric), "100000000000000000000");
        assert_eq!(num_text(Value::Float8(0.0015), PgType::Numeric), "0.0015");
        assert_eq!(num_text(Value::Float8(-0.0), PgType::Numeric), "0");
        // 6 significant digits for float4.
        assert_eq!(num_text(Value::Float4(123.456), PgType::Numeric), "123.456");
        assert_eq!(num_text(Value::Float4(0.1), PgType::Numeric), "0.1");
        // Non-finite carry through as numeric's own spellings.
        assert_eq!(num_text(Value::Float8(f64::INFINITY), PgType::Numeric), "Infinity");
        assert_eq!(num_text(Value::Float8(f64::NEG_INFINITY), PgType::Numeric), "-Infinity");
        assert_eq!(num_text(Value::Float8(f64::NAN), PgType::Numeric), "NaN");
    }

    fn numeric(s: &str) -> Value {
        Value::Numeric(NumericVal::parse(s).unwrap())
    }

    #[test]
    fn numeric_to_int_rounds_half_away_from_zero() {
        assert_eq!(cast(numeric("0.5"), PgType::Int4).unwrap(), Value::Int4(1));
        assert_eq!(cast(numeric("1.5"), PgType::Int4).unwrap(), Value::Int4(2));
        assert_eq!(cast(numeric("2.5"), PgType::Int4).unwrap(), Value::Int4(3));
        assert_eq!(cast(numeric("-2.5"), PgType::Int4).unwrap(), Value::Int4(-3));
        assert_eq!(cast(numeric("2.4"), PgType::Int4).unwrap(), Value::Int4(2));
        assert_eq!(cast(numeric("2.6"), PgType::Int4).unwrap(), Value::Int4(3));
        assert_eq!(cast(numeric("1e3"), PgType::Int4).unwrap(), Value::Int4(1000));
        // Exact large int8 survives the i128 accumulator without precision loss.
        assert_eq!(
            cast(numeric("9223372036854775807"), PgType::Int8).unwrap(),
            Value::Int8(i64::MAX)
        );
    }

    #[test]
    fn numeric_to_int_range_and_special() {
        let e = cast(numeric("99999999999"), PgType::Int4).unwrap_err();
        assert_eq!(e.sqlstate, "22003");
        assert_eq!(e.message, "integer out of range");
        assert_eq!(cast(numeric("1e30"), PgType::Int8).unwrap_err().message, "bigint out of range");
        let e = cast(Value::Numeric(NumericVal::NaN), PgType::Int4).unwrap_err();
        assert_eq!(e.sqlstate, "0A000");
        assert_eq!(e.message, "cannot convert NaN to integer");
        let e = cast(numeric("infinity"), PgType::Int2).unwrap_err();
        assert_eq!(e.sqlstate, "0A000");
        assert_eq!(e.message, "cannot convert infinity to smallint");
    }

    #[test]
    fn bit_to_int_reinterprets_width() {
        assert_eq!(cast(Value::Bit { len: 3, bits: 0b101 }, PgType::Int4).unwrap(), Value::Int4(5));
        assert_eq!(cast(Value::Bit { len: 4, bits: 0b1111 }, PgType::Int4).unwrap(), Value::Int4(15));
        // 32 set bits fill int4's width → two's-complement -1.
        assert_eq!(
            cast(Value::Bit { len: 32, bits: 0xFFFF_FFFF }, PgType::Int4).unwrap(),
            Value::Int4(-1)
        );
        // A 16-bit value is zero-extended (positive) into the wider int4.
        assert_eq!(
            cast(Value::Bit { len: 16, bits: 0x8000 }, PgType::Int4).unwrap(),
            Value::Int4(32768)
        );
        // Wider than the target → out of range.
        let e = cast(Value::Bit { len: 40, bits: 1 }, PgType::Int4).unwrap_err();
        assert_eq!(e.sqlstate, "22003");
        assert_eq!(e.message, "integer out of range");
    }

    #[test]
    fn unsupported_pair_still_cannot_coerce() {
        let e = cast(Value::Bool(true), PgType::Int4).unwrap_err();
        assert_eq!(e.sqlstate, "42846");
        assert_eq!(e.message, "cannot cast type boolean to integer");
    }
}
