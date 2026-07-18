//! Value-level cast machinery shared by the binder (bind-time const folding)
//! and the executor (runtime `Coerce`). Clean-room (see AGENTS.md): this
//! reproduces PG's *observable* cast results — including the SQLSTATE/message on
//! range errors — as pinned by the regression corpus, implemented independently.

use crate::numeric::ParseError;
use crate::{
    Interval, Numeric, PgType, TimeTz, Value, date, float, interval, money, parse_bool, time,
    timestamp, timestamptz, timetz,
};

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

        // ---- anything → text / varchar / bpchar / name (float uses efd) ----
        // These four share the `text` value representation; any length limit for
        // varchar/bpchar is applied separately as a typmod coercion. A
        // `bpchar -> text` trim is handled in the binder, not here, because a
        // padded `bpchar` value is indistinguishable from `text` at this layer.
        (_, PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name) => {
            Ok(Value::Text(v.encode_text_with(efd).unwrap_or_default()))
        }

        // ---- text → scalar (input functions) ----
        (Value::Text(s), PgType::Float4) => float::float4in(s)
            .map(Value::Float4)
            .map_err(|e| CastError { sqlstate: e.sqlstate, message: e.message }),
        (Value::Text(s), PgType::Float8) => float::float8in(s)
            .map(Value::Float8)
            .map_err(|e| CastError { sqlstate: e.sqlstate, message: e.message }),
        (Value::Text(s), PgType::Numeric) => {
            Numeric::parse(s).map(Value::Numeric).map_err(|e| numeric_parse_error(e, s))
        }

        // ---- numeric → float ----
        (Value::Numeric(n), PgType::Float4) => Ok(Value::Float4(n.to_f64() as f32)),
        (Value::Numeric(n), PgType::Float8) => Ok(Value::Float8(n.to_f64())),

        // ---- text → integer (int input functions) ----
        (Value::Text(s), PgType::Int2 | PgType::Int4 | PgType::Int8) => text_to_int(s, to),

        // ---- text → boolean (boolin) ----
        (Value::Text(s), PgType::Bool) => parse_bool(s)
            .map(Value::Bool)
            .ok_or_else(|| invalid_input(PgType::Bool, s)),

        // ---- text → timestamp (timestamp_in) ----
        (Value::Text(s), PgType::Timestamp) => timestamp::parse(s)
            .map(Value::Timestamp)
            .map_err(|e| CastError { sqlstate: e.sqlstate, message: e.message }),

        // ---- text → interval (interval_in) ----
        (Value::Text(s), PgType::Interval) => interval::parse(s)
            .map(Value::Interval)
            .map_err(|e| CastError { sqlstate: e.sqlstate, message: e.message }),

        // ---- text → timestamptz (timestamptz_in) ----
        (Value::Text(s), PgType::TimestampTz) => timestamptz::parse(s)
            .map(Value::TimestampTz)
            .map_err(|e| CastError { sqlstate: e.sqlstate, message: e.message }),

        // ---- timestamp ↔ timestamptz ----
        // With the session zone fixed to UTC these are an identity on the raw
        // microseconds (the wall clock equals the UTC instant), infinities
        // included. WARNING: this is correct ONLY because the session/display
        // zone is always UTC. When a real `TimeZone` GUC is added these arms
        // must convert using it — they will NOT fail on their own, so revisit
        // here (and `timestamptz::{parse,format,make_timestamptz}`) at that time.
        (Value::Timestamp(m), PgType::TimestampTz) => Ok(Value::TimestampTz(*m)),
        (Value::TimestampTz(m), PgType::Timestamp) => Ok(Value::Timestamp(*m)),

        // ---- text → date / time / timetz (input functions) ----
        (Value::Text(s), PgType::Date) => date::parse(s)
            .map(Value::Date)
            .map_err(|e| CastError { sqlstate: e.sqlstate, message: e.message }),
        (Value::Text(s), PgType::Time) => time::parse(s)
            .map(Value::Time)
            .map_err(|e| CastError { sqlstate: e.sqlstate, message: e.message }),
        (Value::Text(s), PgType::TimeTz) => timetz::parse(s)
            .map(Value::TimeTz)
            .map_err(|e| CastError { sqlstate: e.sqlstate, message: e.message }),

        // ---- date ↔ timestamp / timestamptz ----
        // A date widens to midnight; the UTC identity (session zone is UTC)
        // carries it to timestamptz too. The reverse takes the calendar date.
        (Value::Date(d), PgType::Timestamp) => date::to_timestamp_micros(*d)
            .map(Value::Timestamp)
            .map_err(|e| CastError { sqlstate: e.sqlstate, message: e.message }),
        (Value::Date(d), PgType::TimestampTz) => date::to_timestamp_micros(*d)
            .map(Value::TimestampTz)
            .map_err(|e| CastError { sqlstate: e.sqlstate, message: e.message }),
        (Value::Timestamp(m), PgType::Date) => Ok(Value::Date(date::from_timestamp_micros(*m))),
        (Value::TimestampTz(m), PgType::Date) => Ok(Value::Date(date::from_timestamp_micros(*m))),

        // ---- time ↔ interval ----
        // time → interval keeps the microseconds as the time-of-day span;
        // interval → time takes the time-of-day part (mod one day).
        (Value::Time(usec), PgType::Interval) => {
            Ok(Value::Interval(Interval { months: 0, days: 0, usec: *usec }))
        }
        (Value::Interval(iv), PgType::Time) => {
            Ok(Value::Time(iv.usec.rem_euclid(86_400_000_000)))
        }

        // ---- time ↔ timetz ----
        // time → timetz attaches the session zone (UTC); timetz → time drops it.
        (Value::Time(usec), PgType::TimeTz) => Ok(Value::TimeTz(TimeTz { usec: *usec, zone: 0 })),
        (Value::TimeTz(v), PgType::Time) => Ok(Value::Time(v.usec)),

        // ---- integer → numeric (exact) ----
        (Value::Int2(n), PgType::Numeric) => Ok(Value::Numeric(Numeric::from_i128(*n as i128))),
        (Value::Int4(n), PgType::Numeric) => Ok(Value::Numeric(Numeric::from_i128(*n as i128))),
        (Value::Int8(n), PgType::Numeric) => Ok(Value::Numeric(Numeric::from_i128(*n as i128))),

        // ---- float → numeric ----
        // PG's float→numeric keeps DBL_DIG (15) / FLT_DIG (6) significant digits
        // and always prints numeric in plain decimal.
        (Value::Float4(f), PgType::Numeric) => Ok(Value::Numeric(Numeric::from_f64_sig(*f as f64, 6))),
        (Value::Float8(f), PgType::Numeric) => Ok(Value::Numeric(Numeric::from_f64_sig(*f, 15))),

        // ---- numeric → integer (round half away from zero + range check) ----
        (Value::Numeric(n), PgType::Int2 | PgType::Int4 | PgType::Int8) => numeric_to_int(n, to),

        // ---- bit-string → integer (two's-complement of the target width) ----
        // PG has bittoint4/bittoint8 only — there is no bit→smallint cast, so
        // Int2 falls through to `cannot_coerce` below.
        (Value::Bit { len, bits }, PgType::Int4 | PgType::Int8) => bit_to_int(*len, *bits, to),

        // ---- text → bytea (byteain) ----
        (Value::Text(s), PgType::Bytea) => byteain(s).map(Value::Bytea),

        // ---- text → uuid (uuid_in) ----
        (Value::Text(s), PgType::Uuid) => crate::uuid::parse(s)
            .map(Value::Uuid)
            .map_err(|e| CastError { sqlstate: e.sqlstate, message: e.message }),

        // ---- text → inet / cidr (inet_in / cidr_in) ----
        (Value::Text(s), PgType::Inet) => crate::net::inet_in(s)
            .map(Value::Inet)
            .map_err(|e| CastError { sqlstate: e.sqlstate, message: e.message }),
        (Value::Text(s), PgType::Cidr) => crate::net::cidr_in(s)
            .map(Value::Cidr)
            .map_err(|e| CastError { sqlstate: e.sqlstate, message: e.message }),

        // ---- cidr → inet (implicit in PG; the network keeps its masklen) ----
        (Value::Cidr(v), PgType::Inet) => Ok(Value::Inet(*v)),

        // ---- text → money (cash_in): $, thousands, parentheses-as-negative ----
        (Value::Text(s), PgType::Money) => money::parse(s)
            .map(Value::Money)
            .map_err(|e| CastError { sqlstate: e.sqlstate, message: e.message }),

        // ---- int4/int8/numeric → money (whole units scaled to hundredths) ----
        // PG has no int2 → money cast; int2 literals reach money after widening
        // to int4, so that arm falls through to `cannot_coerce`.
        (Value::Int4(n), PgType::Money) => (*n as i64)
            .checked_mul(100)
            .map(Value::Money)
            .ok_or_else(|| out_of_range(PgType::Money)),
        (Value::Int8(n), PgType::Money) => n
            .checked_mul(100)
            .map(Value::Money)
            .ok_or_else(|| out_of_range(PgType::Money)),
        (Value::Numeric(n), PgType::Money) => numeric_to_money(n),

        // ---- money → numeric (always two fractional digits) ----
        (Value::Money(c), PgType::Numeric) => Ok(Value::Numeric(money_to_numeric(*c))),

        // ---- integer <-> oid ----
        // PG's int4->oid/int8->oid wrap through the unsigned 32-bit range (an
        // out-of-range int8 wraps rather than erroring, matching `int8_oid`);
        // oid->int8 is exact, oid->int4 reinterprets the bit pattern.
        (Value::Int2(n), PgType::Oid) => Ok(Value::Oid(*n as u32)),
        (Value::Int4(n), PgType::Oid) => Ok(Value::Oid(*n as u32)),
        (Value::Int8(n), PgType::Oid) => Ok(Value::Oid(*n as u32)),
        (Value::Oid(n), PgType::Int8) => Ok(Value::Int8(*n as i64)),
        (Value::Oid(n), PgType::Int4) => Ok(Value::Int4(*n as i32)),

        // ---- text -> oid (oidin: unsigned decimal, wrapping) ----
        (Value::Text(s), PgType::Oid) => text_to_oid(s),

        _ => Err(cannot_coerce(from, to)),
    }
}

/// `numeric_cash`: scale a numeric by 100, round to the nearest cent (half away
/// from zero, like `numeric`'s round), and range-check into `i64` money.
/// NaN/±infinity have no money image (`cannot convert ... to money`, 0A000), as
/// with the numeric→integer casts.
fn numeric_to_money(n: &Numeric) -> Result<Value, CastError> {
    if n.is_nan() {
        return Err(cannot_convert("NaN", PgType::Money));
    }
    if n.is_infinite() {
        return Err(cannot_convert("infinity", PgType::Money));
    }
    let cents = n
        .mul(&Numeric::from_i128(100))
        .round(0)
        .to_i128()
        .ok_or_else(|| out_of_range(PgType::Money))?;
    i64::try_from(cents).map(Value::Money).map_err(|_| out_of_range(PgType::Money))
}

/// `cash_numeric`: exact value `cents / 100` rendered with display scale 2
/// (built from text so the scale is always two, e.g. `123.00`).
fn money_to_numeric(cents: i64) -> Numeric {
    let mag = (cents as i128).unsigned_abs();
    let s = format!("{}{}.{:02}", if cents < 0 { "-" } else { "" }, mag / 100, mag % 100);
    Numeric::parse(&s).expect("money renders to a valid numeric")
}

/// `byteain`: parse PG's bytea input syntax into raw bytes. A leading `\x`
/// selects hex format (an even run of hex digits); otherwise the traditional
/// escape format applies (`\\` → `\`, `\ooo` octal → that byte, any other byte
/// literal). Malformed input is `22P02`. Shared with the binder's
/// `parse_unknown` so the two never drift.
pub fn byteain(s: &str) -> Result<Vec<u8>, CastError> {
    let bytes = s.as_bytes();
    if let Some(hex) = bytes.strip_prefix(b"\\x") {
        // Hex format: pairs of hex digits, with whitespace between pairs
        // ignored (matching PG's hex_decode).
        let mut out = Vec::with_capacity(hex.len() / 2);
        let mut hi: Option<u8> = None;
        for &c in hex {
            if c.is_ascii_whitespace() {
                // Whitespace is only allowed between pairs, not mid-byte.
                if hi.is_some() {
                    return Err(invalid_input(PgType::Bytea, s));
                }
                continue;
            }
            let nibble = hex_val(c).ok_or_else(|| invalid_input(PgType::Bytea, s))?;
            match hi.take() {
                None => hi = Some(nibble),
                Some(h) => out.push((h << 4) | nibble),
            }
        }
        // A dangling half-byte (odd number of hex digits) is invalid.
        if hi.is_some() {
            return Err(invalid_input(PgType::Bytea, s));
        }
        return Ok(out);
    }
    // Escape format.
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        // A backslash escape: `\\` or `\ooo` (three octal digits).
        match bytes.get(i + 1) {
            Some(b'\\') => {
                out.push(b'\\');
                i += 2;
            }
            Some(&c) if (b'0'..=b'3').contains(&c) => {
                let (Some(&d1), Some(&d2)) = (bytes.get(i + 2), bytes.get(i + 3)) else {
                    return Err(invalid_input(PgType::Bytea, s));
                };
                let (Some(o0), Some(o1), Some(o2)) =
                    (octal_val(c), octal_val(d1), octal_val(d2))
                else {
                    return Err(invalid_input(PgType::Bytea, s));
                };
                out.push((o0 << 6) | (o1 << 3) | o2);
                i += 4;
            }
            _ => return Err(invalid_input(PgType::Bytea, s)),
        }
    }
    Ok(out)
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn octal_val(c: u8) -> Option<u8> {
    (b'0'..=b'7').contains(&c).then(|| c - b'0')
}

/// Map a numeric input-parse failure to PG's error: malformed text is `22P02`
/// (echoing the literal), an out-of-range magnitude is `22003 value overflows
/// numeric format`.
fn numeric_parse_error(e: ParseError, s: &str) -> CastError {
    match e {
        ParseError::Syntax => CastError {
            sqlstate: INVALID_TEXT_REPRESENTATION,
            message: format!("invalid input syntax for type numeric: \"{s}\""),
        },
        ParseError::Overflow => CastError {
            sqlstate: NUMERIC_VALUE_OUT_OF_RANGE,
            message: "value overflows numeric format".to_string(),
        },
    }
}

/// `int2in`/`int4in`/`int8in`: trim, base-10, optional sign. A well-formed
/// number that does not fit is `22003` (printing the literal); anything else is
/// `22P02`. The error's type name comes from `ty`, so int2/int4/int8 print
/// smallint/integer/bigint. Shared with the binder's `parse_unknown`, which
/// resolves unknown literals through the same acceptor (adding the cursor
/// position on the `CastError` it returns).
pub fn text_to_int(s: &str, ty: PgType) -> Result<Value, CastError> {
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

/// `oidin`: parse an unsigned 32-bit object identifier from text. Malformed
/// input is `22P02`; a magnitude past `u32::MAX` is `22003` (`value "…" is out
/// of range for type oid`). Shared by the binder's `parse_unknown` so `'42'::oid`
/// and an untyped literal in an oid context never drift.
pub fn text_to_oid(s: &str) -> Result<Value, CastError> {
    use std::num::IntErrorKind;
    let t = s.trim();
    t.parse::<u32>().map(Value::Oid).map_err(|e| match e.kind() {
        IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => {
            value_out_of_range(PgType::Oid, s)
        }
        _ => invalid_input(PgType::Oid, s),
    })
}

/// `numeric_int2`/`_int4`/`_int8`: round half away from zero, then range-check.
/// NaN/infinity have no integer image (`0A000`); an out-of-range magnitude is
/// the bare `<typename> out of range` (`22003`), matching PG's numeric→int.
fn numeric_to_int(n: &Numeric, ty: PgType) -> Result<Value, CastError> {
    if n.is_nan() {
        return Err(cannot_convert("NaN", ty));
    }
    if n.is_infinite() {
        return Err(cannot_convert("infinity", ty));
    }
    let v = n.to_i128().ok_or_else(|| out_of_range(ty))?;
    match ty {
        PgType::Int2 => i16::try_from(v).map(Value::Int2).map_err(|_| out_of_range(ty)),
        PgType::Int4 => i32::try_from(v).map(Value::Int4).map_err(|_| out_of_range(ty)),
        PgType::Int8 => i64::try_from(v).map(Value::Int8).map_err(|_| out_of_range(ty)),
        _ => unreachable!("numeric_to_int called with {ty:?}"),
    }
}

/// Reinterpret a right-aligned bit string as the target integer's two's
/// complement. A bit string wider than the target errors (`<typename> out of
/// range`), matching PG's `bittoint4`/`bittoint8` (there is no bittoint2).
fn bit_to_int(len: u16, bits: u64, ty: PgType) -> Result<Value, CastError> {
    match ty {
        PgType::Int4 if len <= 32 => Ok(Value::Int4(bits as u32 as i32)),
        PgType::Int8 if len <= 64 => Ok(Value::Int8(bits as i64)),
        PgType::Int4 | PgType::Int8 => Err(out_of_range(ty)),
        _ => unreachable!("bit_to_int called with {ty:?}"),
    }
}

/// Reinterpret a value's bit pattern as the backing builtin `rep` — the runtime
/// of a `CREATE CAST ... WITHOUT FUNCTION` (binary-coercible) cast. Widths are
/// validated equal when the cast is created, so only same-width pairs reach
/// here: int4↔float4 and int8↔float8 swap the interpretation of the same 32/64
/// bits, and a value already in `rep`'s representation (e.g. `xfloat4`→`float4`,
/// both f32) passes through unchanged.
pub fn reinterpret_value(v: Value, rep: PgType) -> Result<Value, CastError> {
    match (v, rep) {
        (Value::Null, _) => Ok(Value::Null),
        (Value::Int4(n), PgType::Float4) => Ok(Value::Float4(f32::from_bits(n as u32))),
        (Value::Float4(f), PgType::Int4) => Ok(Value::Int4(f.to_bits() as i32)),
        (Value::Int8(n), PgType::Float8) => Ok(Value::Float8(f64::from_bits(n as u64))),
        (Value::Float8(f), PgType::Int8) => Ok(Value::Int8(f.to_bits() as i64)),
        // Already the target representation (identical backing rep): relabel only.
        (v @ (Value::Int4(_) | Value::Float4(_) | Value::Int8(_) | Value::Float8(_)), _) => Ok(v),
        (v, to) => Err(cannot_coerce(
            v.pg_type().expect("non-null value has a type"),
            to,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byteain_escape_and_hex() {
        // Plain ASCII (escape format, no backslashes) passes through.
        assert_eq!(byteain("abc").unwrap(), b"abc");
        assert_eq!(byteain("").unwrap(), b"");
        // Escape sequences.
        assert_eq!(byteain("\\\\").unwrap(), b"\\");
        assert_eq!(byteain("a\\001b").unwrap(), vec![b'a', 1, b'b']);
        // Hex format, with whitespace between pairs ignored (matches PG).
        assert_eq!(byteain("\\xdead").unwrap(), vec![0xde, 0xad]);
        assert_eq!(byteain("\\xDE AD").unwrap(), vec![0xde, 0xad]);
        assert_eq!(byteain("\\x").unwrap(), b"");
        // Malformed input is 22P02.
        assert_eq!(byteain("\\xabc").unwrap_err().sqlstate, "22P02"); // odd nibbles
        assert_eq!(byteain("\\xzz").unwrap_err().sqlstate, "22P02"); // non-hex
        assert_eq!(byteain("\\x a b").unwrap_err().sqlstate, "22P02"); // mid-byte space
        assert_eq!(byteain("\\9").unwrap_err().sqlstate, "22P02"); // bad escape
        assert_eq!(byteain("\\").unwrap_err().sqlstate, "22P02"); // dangling backslash
    }

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
            Value::Numeric(Numeric::from_i128(5))
        );
        assert_eq!(
            cast(Value::Int8(-9), PgType::Numeric).unwrap(),
            Value::Numeric(Numeric::from_i128(-9))
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
        Value::Numeric(Numeric::parse(s).unwrap())
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
        let e = cast(Value::Numeric(Numeric::nan()), PgType::Int4).unwrap_err();
        assert_eq!(e.sqlstate, "0A000");
        assert_eq!(e.message, "cannot convert NaN to integer");
        let e = cast(numeric("infinity"), PgType::Int2).unwrap_err();
        assert_eq!(e.sqlstate, "0A000");
        assert_eq!(e.message, "cannot convert infinity to smallint");
    }

    // A numeric literal whose exponent puts it beyond numeric's storable range
    // is rejected at the text→numeric cast with `22003 value overflows numeric
    // format` (matching PG), rather than reaching the int conversion. The parse
    // must not allocate the astronomically many digits the exponent implies.
    #[test]
    fn numeric_input_overflow_is_rejected_before_int_cast() {
        for lit in [
            "1e2147483647",
            "1e99999999999999999999",
            "0e2000000000",
            "1e-2000000000",
            "1e-99999999999999999999",
        ] {
            let e = cast(Value::Text(lit.into()), PgType::Numeric).unwrap_err();
            assert_eq!(e.sqlstate, "22003", "{lit}");
            assert_eq!(e.message, "value overflows numeric format", "{lit}");
        }
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
        // int8 keeps the same reinterpret semantics.
        assert_eq!(
            cast(Value::Bit { len: 64, bits: u64::MAX }, PgType::Int8).unwrap(),
            Value::Int8(-1)
        );
        // Wider than the target → out of range.
        let e = cast(Value::Bit { len: 40, bits: 1 }, PgType::Int4).unwrap_err();
        assert_eq!(e.sqlstate, "22003");
        assert_eq!(e.message, "integer out of range");
    }

    #[test]
    fn bit_to_smallint_is_rejected() {
        // PG has bittoint4/bittoint8 but no bit→smallint cast.
        let e = cast(Value::Bit { len: 3, bits: 0b101 }, PgType::Int2).unwrap_err();
        assert_eq!(e.sqlstate, "42846");
        assert_eq!(e.message, "cannot cast type bit to smallint");
    }

    #[test]
    fn reinterpret_swaps_int_float_bits() {
        // int4 → float4: the bit pattern of 1 is the smallest subnormal float.
        assert_eq!(
            reinterpret_value(Value::Int4(1), PgType::Float4).unwrap(),
            Value::Float4(f32::from_bits(1))
        );
        // float4 → int4 is the inverse.
        assert_eq!(
            reinterpret_value(Value::Float4(f32::from_bits(1)), PgType::Int4).unwrap(),
            Value::Int4(1)
        );
        // int8 ↔ float8 over 64 bits.
        assert_eq!(
            reinterpret_value(Value::Int8(4607182418800017408), PgType::Float8).unwrap(),
            Value::Float8(1.0)
        );
        assert_eq!(
            reinterpret_value(Value::Float8(1.0), PgType::Int8).unwrap(),
            Value::Int8(4607182418800017408)
        );
        // Same representation (e.g. xfloat4→float4): identity relabel.
        assert_eq!(
            reinterpret_value(Value::Float4(1.5), PgType::Float4).unwrap(),
            Value::Float4(1.5)
        );
        // NULL passes through.
        assert_eq!(reinterpret_value(Value::Null, PgType::Float4).unwrap(), Value::Null);
    }

    #[test]
    fn unsupported_pair_still_cannot_coerce() {
        let e = cast(Value::Bool(true), PgType::Int4).unwrap_err();
        assert_eq!(e.sqlstate, "42846");
        assert_eq!(e.message, "cannot cast type boolean to integer");
    }
}
