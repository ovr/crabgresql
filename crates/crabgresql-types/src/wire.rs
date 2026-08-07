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
use crate::{FmtCtx, PgType, Value, VectorKind};

/// `22P03` invalid_binary_representation — a binary value the type's `recv`
/// function would reject (wrong length, out-of-domain byte).
const INVALID_BINARY: &str = "22P03";
/// `0A000` feature_not_supported — no binary I/O for this type yet.
const FEATURE_NOT_SUPPORTED: &str = "0A000";

fn invalid_binary(ty: PgType) -> CastError {
    CastError {
        sqlstate: INVALID_BINARY,
        message: format!("invalid binary representation for type {}", ty.name()),
        detail: None,
    }
}

fn no_binary(ty: PgType) -> CastError {
    CastError {
        sqlstate: FEATURE_NOT_SUPPORTED,
        message: format!("binary format is not supported for type {}", ty.name()),
        detail: None,
    }
}

fn fixed<const N: usize>(b: &[u8], ty: PgType) -> Result<[u8; N], CastError> {
    b.try_into().map_err(|_| invalid_binary(ty))
}

/// Decode a Bind parameter delivered in **text** format into a `Value` of `ty`.
/// Reuses the type's SQL input function, so a text parameter and the equivalent
/// literal fail and succeed identically.
pub fn decode_text(ty: PgType, s: &str, fmt: &FmtCtx) -> Result<Value, CastError> {
    match ty {
        // `name` truncates to 63 characters (`namein`), as PG does.
        PgType::Name => Ok(Value::Text(crate::text::name_input(s))),
        // The other string types share text's representation; a bind parameter
        // carries no typmod, so there is no length to enforce here (as in PG,
        // where an untyped `$1::bpchar` is not blank-padded).
        PgType::Text | PgType::Varchar | PgType::Bpchar => Ok(Value::Text(s.to_string())),
        _ => cast::cast_value(Value::Text(s.to_string()), ty, fmt),
    }
}

/// Decode a Bind parameter delivered in **binary** format into a `Value` of
/// `ty`. Implemented for the scalar/string/`bytea` set; other types → `0A000`.
pub fn decode_binary(ty: PgType, b: &[u8]) -> Result<Value, CastError> {
    Ok(match ty {
        // PG's `boolrecv` treats any nonzero byte as true, not only 1.
        PgType::Bool => Value::Bool(fixed::<1>(b, ty)?[0] != 0),
        // `charrecv` takes the byte as-is — no octal decoding, which applies
        // only to the *text* input function.
        PgType::Char => Value::Char(fixed::<1>(b, ty)?[0]),
        PgType::Int2 => Value::Int2(i16::from_be_bytes(fixed(b, ty)?)),
        PgType::Int4 => Value::Int4(i32::from_be_bytes(fixed(b, ty)?)),
        PgType::Int8 => Value::Int8(i64::from_be_bytes(fixed(b, ty)?)),
        PgType::Oid => Value::Oid(u32::from_be_bytes(fixed(b, ty)?)),
        // Transaction ids and LSNs are bare unsigned counters on the wire.
        PgType::Xid => Value::Xid(u32::from_be_bytes(fixed(b, ty)?)),
        PgType::Xid8 => Value::Xid8(u64::from_be_bytes(fixed(b, ty)?)),
        PgType::PgLsn => Value::PgLsn(u64::from_be_bytes(fixed(b, ty)?)),
        // `uuid_recv`: the 16 stored bytes, no punctuation.
        PgType::Uuid => Value::Uuid(fixed::<16>(b, ty)?),
        // `tid` is the only composite here: a 4-byte block then a 2-byte
        // offset, six bytes with no padding.
        PgType::Tid => {
            let raw: [u8; 6] = fixed(b, ty)?;
            Value::Tid {
                block: u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]),
                offset: u16::from_be_bytes([raw[4], raw[5]]),
            }
        }
        // PG's `regclassrecv` and friends send only the OID, so a value arriving
        // over the binary protocol has no name yet and renders like an
        // unresolved one. Clients send reg* parameters this way only rarely;
        // resolution would need a catalog this layer does not have.
        PgType::Reg(kind) => Value::Reg(crate::Reg::unresolved(
            kind,
            u32::from_be_bytes(fixed(b, ty)?),
        )),
        PgType::Float4 => Value::Float4(f32::from_bits(u32::from_be_bytes(fixed(b, ty)?))),
        PgType::Float8 => Value::Float8(f64::from_bits(u64::from_be_bytes(fixed(b, ty)?))),
        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => {
            let s = String::from_utf8(b.to_vec()).map_err(|_| invalid_binary(ty))?;
            // `name` truncates to 63 characters (`namerecv`); the rest keep their
            // full length.
            match ty {
                PgType::Name => Value::Text(crate::text::name_input(&s)),
                _ => Value::Text(s),
            }
        }
        PgType::Bytea => Value::Bytea(b.to_vec()),
        PgType::Vector(kind) => Value::Vector {
            kind,
            elems: decode_vector(b, kind)?,
        },
        other => return Err(no_binary(other)),
    })
}

/// `oidvectorrecv`/`int2vectorrecv`: PG sends these in the ordinary array binary
/// layout — `ndim`, a has-nulls flag, the element type OID, then per dimension a
/// length and a lower bound, then each element as a 4-byte length plus payload.
/// A vector is always 1-D with **lower bound 0** (the reason its subscripts are
/// 0-based), and never has nulls. Probed against PostgreSQL 18.4; the exact byte
/// layout is pinned by `vector_binary_layout_matches_pg` below.
fn decode_vector(b: &[u8], kind: VectorKind) -> Result<Vec<Value>, CastError> {
    let ty = PgType::Vector(kind);
    let be = |s: &[u8]| i32::from_be_bytes([s[0], s[1], s[2], s[3]]);
    if b.len() < 20 {
        return Err(invalid_binary(ty));
    }
    let (ndim, elem_oid, count, lower) =
        (be(&b[0..4]), be(&b[8..12]), be(&b[12..16]), be(&b[16..20]));
    // Derived from the element type, as the encoder does, so the two cannot
    // disagree about the stride if a third VectorKind is ever added.
    let width = kind.element().typlen() as usize;
    // Reject anything that is not the shape PG sends, rather than guessing:
    // a wrong element type or lower bound would silently change the value.
    if ndim != 1 || lower != 0 || count < 0 || elem_oid != kind.element().oid() as i32 {
        return Err(invalid_binary(ty));
    }
    let count = count as usize;
    if b.len() != 20 + count * (4 + width) {
        return Err(invalid_binary(ty));
    }
    let mut elems = Vec::with_capacity(count);
    let mut pos = 20;
    for _ in 0..count {
        if be(&b[pos..pos + 4]) != width as i32 {
            return Err(invalid_binary(ty));
        }
        pos += 4;
        elems.push(match kind {
            VectorKind::Oid => Value::Oid(u32::from_be_bytes(fixed(&b[pos..pos + 4], ty)?)),
            VectorKind::Int2 => Value::Int2(i16::from_be_bytes(fixed(&b[pos..pos + 2], ty)?)),
        });
        pos += width;
    }
    Ok(elems)
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
            // `charsend`: the raw byte, unescaped.
            Value::Char(c) => vec![*c],
            Value::Int2(v) => v.to_be_bytes().to_vec(),
            Value::Int4(v) => v.to_be_bytes().to_vec(),
            Value::Int8(v) => v.to_be_bytes().to_vec(),
            Value::Oid(v) => v.to_be_bytes().to_vec(),
            Value::Xid(v) => v.to_be_bytes().to_vec(),
            Value::Xid8(v) => v.to_be_bytes().to_vec(),
            Value::PgLsn(v) => v.to_be_bytes().to_vec(),
            // `uuid_send`: the stored bytes, which are already the wire form.
            Value::Uuid(u) => u.to_vec(),
            // Block then offset, six bytes (PG's `tidsend`).
            Value::Tid { block, offset } => {
                let mut out = Vec::with_capacity(6);
                out.extend_from_slice(&block.to_be_bytes());
                out.extend_from_slice(&offset.to_be_bytes());
                out
            }
            // Like PG's `regclasssend`: the OID alone goes on the wire, never
            // the name.
            Value::Reg(r) => r.oid.to_be_bytes().to_vec(),
            Value::Float4(v) => v.to_bits().to_be_bytes().to_vec(),
            Value::Float8(v) => v.to_bits().to_be_bytes().to_vec(),
            Value::Text(s) => s.as_bytes().to_vec(),
            Value::Bytea(b) => b.clone(),
            // `oidvectorsend`/`int2vectorsend` — see `decode_vector` for the
            // layout. `ndim` stays 1 even for an empty vector.
            Value::Vector { kind, elems } => {
                let elem = kind.element();
                let width = elem.typlen() as i32;
                let mut out = Vec::with_capacity(20 + elems.len() * (4 + width as usize));
                out.extend_from_slice(&1i32.to_be_bytes()); // ndim
                out.extend_from_slice(&0i32.to_be_bytes()); // has nulls
                out.extend_from_slice(&elem.oid().to_be_bytes());
                out.extend_from_slice(&(elems.len() as i32).to_be_bytes());
                out.extend_from_slice(&0i32.to_be_bytes()); // lower bound
                for e in elems {
                    out.extend_from_slice(&width.to_be_bytes());
                    match e {
                        Value::Oid(v) => out.extend_from_slice(&v.to_be_bytes()),
                        Value::Int2(v) => out.extend_from_slice(&v.to_be_bytes()),
                        other => unreachable!("vector element is not oid/int2: {other:?}"),
                    }
                }
                out
            }
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
    fn int4_binary_round_trips() -> anyhow::Result<()> {
        let bytes = Value::Int4(-12345)
            .encode_binary()?
            .ok_or_else(|| anyhow::anyhow!("int4 encoded as NULL"))?;
        assert_eq!(bytes, (-12345i32).to_be_bytes());
        assert_eq!(decode_binary(PgType::Int4, &bytes)?, Value::Int4(-12345));

        Ok(())
    }

    /// `uuid_send`/`uuid_recv` put the 16 stored bytes on the wire unpunctuated,
    /// so the binary form is not the 36-character text one.
    #[test]
    fn uuid_binary_round_trips() -> anyhow::Result<()> {
        let value = Value::Uuid(crate::uuid::parse("5b35380a-7143-4912-9b55-f322699c6770")?);
        let bytes = value
            .encode_binary()?
            .ok_or_else(|| anyhow::anyhow!("uuid encoded as NULL"))?;
        assert_eq!(bytes.len(), 16);
        assert_eq!(bytes[0], 0x5b);
        assert_eq!(decode_binary(PgType::Uuid, &bytes)?, value);

        let e = decode_binary(PgType::Uuid, &bytes[..15])
            .expect_err("a uuid is exactly 16 bytes on the wire");
        assert_eq!(e.sqlstate, "22P03");

        Ok(())
    }

    /// The four fixed-width types added alongside `tid`. Each asserts the exact
    /// wire layout as well as the round trip, since the byte order is what a
    /// driver depends on — a round trip alone would pass on little-endian too.
    #[test]
    fn tid_xid_and_pg_lsn_binary_round_trip() -> anyhow::Result<()> {
        let cases = [
            (Value::Xid(4294967295), PgType::Xid, vec![0xff; 4]),
            (Value::Xid8(1), PgType::Xid8, vec![0, 0, 0, 0, 0, 0, 0, 1]),
            (
                Value::PgLsn(0x0000_0001_016A_E7F8),
                PgType::PgLsn,
                vec![0, 0, 0, 1, 0x01, 0x6A, 0xE7, 0xF8],
            ),
            (
                Value::Tid {
                    block: 42,
                    offset: 7,
                },
                PgType::Tid,
                vec![0, 0, 0, 42, 0, 7],
            ),
        ];
        for (value, ty, expected) in cases {
            let bytes = value
                .encode_binary()?
                .ok_or_else(|| anyhow::anyhow!("{ty:?} encoded as NULL"))?;
            assert_eq!(bytes, expected, "{ty:?} wire layout");
            assert_eq!(decode_binary(ty, &bytes)?, value, "{ty:?} round trip");
        }
        // A short or long payload is a 22P03, not a panic.
        assert_eq!(
            decode_binary(PgType::Tid, &[0, 0, 0, 42, 0])
                .expect_err("5 bytes is not a tid")
                .sqlstate,
            "22P03"
        );

        Ok(())
    }

    /// The expected bytes here were captured from PostgreSQL 18.4 with
    /// `COPY (SELECT '11 22'::oidvector) TO ... WITH (FORMAT binary)`, so this
    /// pins the real layout rather than just a self-consistent round trip. Note
    /// `ndim` stays 1 for an empty vector, and the lower bound is 0 — the same
    /// 0 that makes vector subscripts 0-based.
    #[test]
    fn vector_binary_layout_matches_pg() -> anyhow::Result<()> {
        #[rustfmt::skip]
        let cases = [
            (
                Value::Vector { kind: VectorKind::Oid, elems: vec![Value::Oid(11), Value::Oid(22)] },
                PgType::Vector(VectorKind::Oid),
                vec![
                    0, 0, 0, 1,     // ndim
                    0, 0, 0, 0,     // has nulls
                    0, 0, 0, 26,    // element type = oid
                    0, 0, 0, 2,     // dim[0]
                    0, 0, 0, 0,     // lower bound
                    0, 0, 0, 4, 0, 0, 0, 11,
                    0, 0, 0, 4, 0, 0, 0, 22,
                ],
            ),
            (
                Value::Vector { kind: VectorKind::Oid, elems: vec![] },
                PgType::Vector(VectorKind::Oid),
                vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 26, 0, 0, 0, 0, 0, 0, 0, 0],
            ),
            (
                Value::Vector { kind: VectorKind::Int2, elems: vec![Value::Int2(-5), Value::Int2(7)] },
                PgType::Vector(VectorKind::Int2),
                vec![
                    0, 0, 0, 1,
                    0, 0, 0, 0,
                    0, 0, 0, 21,    // element type = int2
                    0, 0, 0, 2,
                    0, 0, 0, 0,
                    0, 0, 0, 2, 0xff, 0xfb,
                    0, 0, 0, 2, 0x00, 0x07,
                ],
            ),
        ];
        for (value, ty, expected) in cases {
            let bytes = value
                .encode_binary()?
                .ok_or_else(|| anyhow::anyhow!("{ty:?} encoded as NULL"))?;
            assert_eq!(bytes, expected, "{ty:?} wire layout");
            assert_eq!(decode_binary(ty, &bytes)?, value, "{ty:?} round trip");
        }

        // A truncated payload, and a shape PG never sends, are both 22P03
        // rather than a panic or a silently different value.
        let ov = PgType::Vector(VectorKind::Oid);
        for bad in [
            vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 26, 0, 0, 0, 1, 0, 0, 0, 0], // count 1, no element
            vec![0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 26, 0, 0, 0, 0, 0, 0, 0, 0], // ndim 2
            vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 26, 0, 0, 0, 0, 0, 0, 0, 1], // lower bound 1
            vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 23, 0, 0, 0, 0, 0, 0, 0, 0], // element type int4
            vec![0, 0, 0, 1],                                                  // short header
        ] {
            assert_eq!(
                decode_binary(ov, &bad)
                    .expect_err("not a valid oidvector payload")
                    .sqlstate,
                "22P03"
            );
        }

        Ok(())
    }

    #[test]
    fn float8_binary_round_trips() -> anyhow::Result<()> {
        let v = Value::Float8(3.5);
        let bytes = v
            .encode_binary()?
            .ok_or_else(|| anyhow::anyhow!("float8 encoded as NULL"))?;
        assert_eq!(decode_binary(PgType::Float8, &bytes)?, v);

        Ok(())
    }

    #[test]
    fn bool_binary_accepts_any_nonzero_and_rejects_short() -> anyhow::Result<()> {
        // PG's boolrecv: any nonzero byte is true.
        assert_eq!(decode_binary(PgType::Bool, &[0])?, Value::Bool(false));
        assert_eq!(decode_binary(PgType::Bool, &[1])?, Value::Bool(true));
        assert_eq!(decode_binary(PgType::Bool, &[2])?, Value::Bool(true));
        assert!(decode_binary(PgType::Int4, &[0, 0, 0]).is_err()); // short

        Ok(())
    }

    #[test]
    fn name_param_truncates_to_63_chars() -> anyhow::Result<()> {
        let long = "x".repeat(100);
        let Value::Text(t) = decode_text(PgType::Name, &long, &FmtCtx::utc_default())? else {
            panic!("name decodes to text");
        };
        assert_eq!(t.chars().count(), 63);

        Ok(())
    }

    #[test]
    fn text_param_matches_literal_input() -> anyhow::Result<()> {
        assert_eq!(
            decode_text(PgType::Int4, " 42 ", &FmtCtx::utc_default())?,
            Value::Int4(42)
        );
        assert_eq!(
            decode_text(PgType::Bool, "t", &FmtCtx::utc_default())?,
            Value::Bool(true)
        );
        assert_eq!(
            decode_text(PgType::Text, "hi", &FmtCtx::utc_default())?,
            Value::Text("hi".into())
        );

        Ok(())
    }

    #[test]
    fn unsupported_binary_is_feature_error() {
        let err = Value::Date(0)
            .encode_binary()
            .expect_err("date has no binary send yet");
        assert_eq!(err.sqlstate, FEATURE_NOT_SUPPORTED);
        let err =
            decode_binary(PgType::Date, &[0, 0, 0, 0]).expect_err("date has no binary recv yet");
        assert_eq!(err.sqlstate, FEATURE_NOT_SUPPORTED);
    }
}
