//! Binary/text wire I/O for the extended query protocol: decoding Bind
//! parameters and encoding result columns in the client-requested format.
//!
//! Text I/O routes through the shared [`crate::cast`] input functions so a
//! text-format parameter parses exactly like the same literal in SQL. Binary
//! I/O is implemented for the common fixed-width scalars and the string/`bytea`
//! types; a binary request for any other type is an honest `0A000`, matching a
//! server that lacks that type's `send`/`recv` function. Layouts follow the
//! documented binary formats (network byte order), not PG's C source.

use crate::cast::{self, CastError};
use crate::{PgType, Value};

/// `22P03` invalid_binary_representation — a binary value the type's `recv`
/// function would reject (wrong length, out-of-domain byte).
const INVALID_BINARY: &str = "22P03";
/// `0A000` feature_not_supported — no binary I/O for this type yet.
const FEATURE_NOT_SUPPORTED: &str = "0A000";

fn invalid_binary(ty: PgType) -> CastError {
    CastError {
        sqlstate: INVALID_BINARY,
        message: format!("invalid binary representation for type {}", ty.name()),
    }
}

fn no_binary(ty: PgType) -> CastError {
    CastError {
        sqlstate: FEATURE_NOT_SUPPORTED,
        message: format!("binary format is not supported for type {}", ty.name()),
    }
}

/// Decode a Bind parameter delivered in **text** format into a `Value` of `ty`.
/// Reuses the type's SQL input function, so a text parameter and the equivalent
/// literal fail and succeed identically.
pub fn decode_text(ty: PgType, s: &str) -> Result<Value, CastError> {
    match ty {
        // The string types share text's representation; a bind parameter carries
        // no typmod, so there is no length to enforce here (as in PG, where an
        // untyped `$1::bpchar` is not blank-padded).
        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => {
            Ok(Value::Text(s.to_string()))
        }
        _ => cast::cast_value(Value::Text(s.to_string()), ty, 1),
    }
}

/// Decode a Bind parameter delivered in **binary** format into a `Value` of
/// `ty`. Implemented for the scalar/string/`bytea` set; other types → `0A000`.
pub fn decode_binary(ty: PgType, b: &[u8]) -> Result<Value, CastError> {
    let fixed = |n: usize| -> Result<&[u8], CastError> {
        if b.len() == n {
            Ok(b)
        } else {
            Err(invalid_binary(ty))
        }
    };
    Ok(match ty {
        PgType::Bool => match fixed(1)?[0] {
            0 => Value::Bool(false),
            1 => Value::Bool(true),
            _ => return Err(invalid_binary(ty)),
        },
        PgType::Int2 => Value::Int2(i16::from_be_bytes(fixed(2)?.try_into().unwrap())),
        PgType::Int4 => Value::Int4(i32::from_be_bytes(fixed(4)?.try_into().unwrap())),
        PgType::Int8 => Value::Int8(i64::from_be_bytes(fixed(8)?.try_into().unwrap())),
        PgType::Oid => Value::Oid(u32::from_be_bytes(fixed(4)?.try_into().unwrap())),
        PgType::Float4 => {
            Value::Float4(f32::from_bits(u32::from_be_bytes(fixed(4)?.try_into().unwrap())))
        }
        PgType::Float8 => {
            Value::Float8(f64::from_bits(u64::from_be_bytes(fixed(8)?.try_into().unwrap())))
        }
        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => Value::Text(
            String::from_utf8(b.to_vec()).map_err(|_| invalid_binary(ty))?,
        ),
        PgType::Bytea => Value::Bytea(b.to_vec()),
        other => return Err(no_binary(other)),
    })
}

impl Value {
    /// Encode this value in **binary** format for a result `DataRow`. `None`
    /// encodes SQL NULL. Implemented for the scalar/string/`bytea` set; any
    /// other type → `0A000`, so a client that asked for binary on an
    /// unsupported column gets an honest error instead of wrong bytes.
    pub fn encode_binary(&self) -> Result<Option<Vec<u8>>, CastError> {
        Ok(Some(match self {
            Value::Null => return Ok(None),
            Value::Bool(v) => vec![*v as u8],
            Value::Int2(v) => v.to_be_bytes().to_vec(),
            Value::Int4(v) => v.to_be_bytes().to_vec(),
            Value::Int8(v) => v.to_be_bytes().to_vec(),
            Value::Oid(v) => v.to_be_bytes().to_vec(),
            Value::Float4(v) => v.to_bits().to_be_bytes().to_vec(),
            Value::Float8(v) => v.to_bits().to_be_bytes().to_vec(),
            Value::Text(s) => s.as_bytes().to_vec(),
            Value::Bytea(b) => b.clone(),
            other => {
                let ty = other.pg_type().unwrap_or(PgType::Text);
                return Err(no_binary(ty));
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int4_binary_round_trips() {
        let bytes = Value::Int4(-12345).encode_binary().unwrap().unwrap();
        assert_eq!(bytes, (-12345i32).to_be_bytes());
        assert_eq!(decode_binary(PgType::Int4, &bytes).unwrap(), Value::Int4(-12345));
    }

    #[test]
    fn float8_binary_round_trips() {
        let v = Value::Float8(3.5);
        let bytes = v.encode_binary().unwrap().unwrap();
        assert_eq!(decode_binary(PgType::Float8, &bytes).unwrap(), v);
    }

    #[test]
    fn bool_binary_rejects_bad_byte() {
        assert!(decode_binary(PgType::Bool, &[2]).is_err());
        assert!(decode_binary(PgType::Int4, &[0, 0, 0]).is_err()); // short
    }

    #[test]
    fn text_param_matches_literal_input() {
        assert_eq!(decode_text(PgType::Int4, " 42 ").unwrap(), Value::Int4(42));
        assert_eq!(decode_text(PgType::Bool, "t").unwrap(), Value::Bool(true));
        assert_eq!(
            decode_text(PgType::Text, "hi").unwrap(),
            Value::Text("hi".into())
        );
    }

    #[test]
    fn unsupported_binary_is_feature_error() {
        let err = Value::Date(0).encode_binary().unwrap_err();
        assert_eq!(err.sqlstate, FEATURE_NOT_SUPPORTED);
        let err = decode_binary(PgType::Date, &[0, 0, 0, 0]).unwrap_err();
        assert_eq!(err.sqlstate, FEATURE_NOT_SUPPORTED);
    }
}
