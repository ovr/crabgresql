//! Value-level cast machinery shared by the binder (bind-time const folding)
//! and the executor (runtime `Coerce`). Clean-room (see AGENTS.md): this
//! reproduces PG's *observable* cast results — including the SQLSTATE/message on
//! range errors — as pinned by the regression corpus, implemented independently.

use crate::{NumericVal, PgType, Value, float};

/// SQLSTATE + message for a failed cast.
#[derive(Clone, Debug, PartialEq)]
pub struct CastError {
    pub sqlstate: &'static str,
    pub message: String,
}

const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
const CANNOT_COERCE: &str = "42846";

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
        (Value::Text(s), PgType::Numeric) => Ok(Value::Numeric(numeric_in(s))),

        // ---- numeric → float ----
        (Value::Numeric(n), PgType::Float4) => Ok(Value::Float4(numeric_to_f64(n) as f32)),
        (Value::Numeric(n), PgType::Float8) => Ok(Value::Float8(numeric_to_f64(n))),

        _ => Err(cannot_coerce(from, to)),
    }
}

/// Minimal `numeric_in`: only the forms these tests reach (`NaN`, decimal).
fn numeric_in(s: &str) -> NumericVal {
    let t = s.trim();
    if t.eq_ignore_ascii_case("nan") {
        NumericVal::NaN
    } else {
        NumericVal::Finite(t.to_string())
    }
}

fn numeric_to_f64(n: &NumericVal) -> f64 {
    match n {
        NumericVal::NaN => f64::NAN,
        NumericVal::Finite(s) => s.parse().unwrap_or(f64::NAN),
    }
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
}
