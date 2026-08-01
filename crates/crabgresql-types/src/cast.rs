//! Value-level cast machinery shared by the binder (bind-time const folding)
//! and the executor (runtime `Coerce`). Clean-room (see AGENTS.md): this
//! reproduces PG's *observable* cast results — including the SQLSTATE/message on
//! range errors — as pinned by the regression corpus, implemented independently.

use crate::fmt::FmtCtx;
use crate::numeric::ParseError;
use crate::{
    Interval, Numeric, PgType, TimeTz, Value, date, float, interval, json, jsonpath, money,
    parse_bool, time, timestamp, timestamptz, timetz,
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
/// `22021` — see the `"char" -> bpchar` arm in [`cast_value`] for the one place
/// this engine raises it that PostgreSQL does not.
const CHARACTER_NOT_IN_REPERTOIRE: &str = "22021";

fn out_of_range(ty: PgType) -> CastError {
    CastError {
        sqlstate: NUMERIC_VALUE_OUT_OF_RANGE,
        message: format!("{} out of range", ty.name()),
    }
}

/// Adapt a [`json::JsonError`] to a [`CastError`]. The optional DETAIL is
/// dropped here; the binder's literal-input path (`parse_unknown`) preserves it
/// for `'...'::json` const folding, where the caret position also matters.
fn json_err(e: json::JsonError) -> CastError {
    CastError {
        sqlstate: e.sqlstate,
        message: e.message,
    }
}

fn ts_err(e: crate::tsvector::TsError) -> CastError {
    CastError {
        sqlstate: e.sqlstate,
        message: e.message,
    }
}

fn cannot_coerce(from: PgType, to: PgType) -> CastError {
    CastError {
        sqlstate: CANNOT_COERCE,
        message: format!("cannot cast type {} to {}", from.name(), to.name()),
    }
}

/// `22P02` — an input function rejected the text (`'abc'::int4`).
pub(crate) fn invalid_input(ty: PgType, s: &str) -> CastError {
    CastError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("invalid input syntax for type {}: \"{s}\"", ty.name()),
    }
}

/// `22003` on the text→int path, which prints the offending literal (unlike the
/// bare `out_of_range` PG uses for arithmetic and numeric→int overflow).
pub(crate) fn value_out_of_range(ty: PgType, s: &str) -> CastError {
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

/// Pairs where an assignment cannot use the ordinary cast and PostgreSQL falls
/// back to an I/O conversion. Two distinct reasons land here:
///
/// * the pair is marked explicit-only (`pg_cast.castcontext = 'e'`), so it is
///   reachable from `CAST`/`::` but never from an assignment — `int4 -> bool`,
///   and both `int4 <-> "char"` directions;
/// * there is no `pg_cast` entry between the two types *at all*, which is the
///   case for `oidvector` and `int2vector` in either direction. PostgreSQL still
///   accepts `x := y` between them by rendering and re-parsing, so
///   `DECLARE x oidvector; y int2vector := '1 2'; x := y` yields `1 2`.
fn needs_io_fallback_on_assign(from: PgType, to: PgType) -> bool {
    matches!(
        (from, to),
        (PgType::Int4, PgType::Bool)
            | (PgType::Vector(_), PgType::Vector(_))
            | (PgType::Int4, PgType::Char)
            | (PgType::Char, PgType::Int4)
    )
}

/// Re-label a `timestamp`-family error as a cast failure. Both carry a
/// SQLSTATE and a message; only the type differs.
fn cast_err(e: crate::timestamp::TimestampError) -> CastError {
    CastError {
        sqlstate: e.sqlstate,
        message: e.message,
    }
}

/// Cast `v` to `to` in PostgreSQL's *assignment* context — PL/pgSQL `:=`,
/// `SELECT … INTO`, and coercing a `RETURN` value to the declared type.
///
/// When no assignment cast exists, PostgreSQL falls back to an I/O conversion:
/// render the source with its output function, then feed that text to the
/// target's input function. The fallback accepts strictly less than the explicit
/// cast, which is the observable difference — `b := 1` yields true, but `b := 2`
/// raises `22P02` even though `2::boolean` is true. Going through the target's
/// input function is also what makes an out-of-range element report the element
/// type's `22003` rather than a blanket `cannot cast` (see
/// [`needs_io_fallback_on_assign`]).
pub fn cast_value_assign(v: Value, to: PgType, fmt: &FmtCtx) -> Result<Value, CastError> {
    let Some(from) = v.pg_type() else {
        return Ok(v); // NULL assigns to any type.
    };
    if from != to && needs_io_fallback_on_assign(from, to) {
        let text = v
            .encode_text_with(fmt)
            .expect("non-null value has a text encoding");
        return cast_value(Value::Text(text), to, fmt);
    }
    cast_value(v, to, fmt)
}

/// Cast `v` to `to`. `fmt` supplies `extra_float_digits` (float→text) and the
/// session display zone (every `timestamptz` conversion).
pub fn cast_value(v: Value, to: PgType, fmt: &FmtCtx) -> Result<Value, CastError> {
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

        // PostgreSQL exposes an explicit int4 -> boolean cast: zero is false,
        // every other value is true. There is deliberately no int2/int8 arm.
        // Explicit-only, so assignments must go through `cast_value_assign`.
        (Value::Int4(n), PgType::Bool) => Ok(Value::Bool(*n != 0)),

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

        // ---- bool → text / varchar / bpchar ----
        // PG spells these out via its dedicated `booltext` cast. Its *output*
        // function still yields `t`/`f`, which is what display, `concat()` and
        // the cast to `name` use — so this arm must stay ahead of the generic
        // any-to-text arm below without replacing it.
        (Value::Bool(b), PgType::Text | PgType::Varchar | PgType::Bpchar) => {
            Ok(Value::Text(if *b { "true" } else { "false" }.to_string()))
        }

        // ---- inet → text / varchar / bpchar ----
        // Like bool below, `inet` has a dedicated cast that does not agree with
        // its output function: the cast always spells the masklen out, while
        // `inet_out` (display, `concat()`, `array_out`, `::name`) omits `/32`
        // and `/128`. `cidr_out` already always prints one, so `cidr` needs no
        // arm of its own.
        (Value::Inet(v), PgType::Text | PgType::Varchar | PgType::Bpchar) => {
            Ok(Value::Text(crate::net::inet_text(v)))
        }

        // ---- "char" → bpchar ----
        // The one string target that does *not* go through `charout`. PG's
        // `char_bpchar` copies the raw byte, so `'\377'::"char"::bpchar` is one
        // byte wide while `::text`, `::varchar` and `::name` are all the
        // four-character `\377`. A high-bit byte therefore has no UTF-8 image,
        // and this engine — unlike PG, which lets the invalid byte through —
        // refuses it rather than fabricating one.
        (Value::Char(c), PgType::Bpchar) => {
            if *c & 0x80 != 0 {
                return Err(CastError {
                    sqlstate: CHARACTER_NOT_IN_REPERTOIRE,
                    message: format!(
                        "byte sequence 0x{c:02x} in \"char\" has no character in encoding \"UTF8\""
                    ),
                });
            }
            Ok(Value::Text(crate::char::char_out(*c)))
        }

        // ---- anything → text / varchar / bpchar / name (float uses efd) ----
        // These four share the `text` value representation; any length limit for
        // varchar/bpchar is applied separately as a typmod coercion. A
        // `bpchar -> text` trim is handled in the binder, not here, because a
        // padded `bpchar` value is indistinguishable from `text` at this layer.
        (_, PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name) => {
            Ok(Value::Text(v.encode_text_with(fmt).unwrap_or_default()))
        }

        // ---- text → scalar (input functions) ----
        // `text_char`, which PG routes through the same rule as `charin`, so
        // `'\377'::text::"char"` is the byte 0xFF and `''::text::"char"` is the
        // zero byte. Never fails. The reverse direction needs no arm: the
        // generic any-to-text arm above renders via `encode_text_with`, which
        // is `charout` — and `char_text` produces the same string.
        (Value::Text(s), PgType::Char) => Ok(Value::Char(crate::char::char_in(s))),
        // `chartoi4` reads the byte as *signed*, so `'\377'::"char"::int4` is
        // -1 — the opposite convention from the unsigned ordering in
        // `compare_values`. Both are PG's; see `crate::char`.
        (Value::Char(c), PgType::Int4) => Ok(Value::Int4(i32::from(*c as i8))),
        // `i4tochar` range-checks rather than truncating.
        (Value::Int4(x), PgType::Char) => match i8::try_from(*x) {
            Ok(c) => Ok(Value::Char(c as u8)),
            Err(_) => Err(CastError {
                sqlstate: NUMERIC_VALUE_OUT_OF_RANGE,
                message: "\"char\" out of range".to_string(),
            }),
        },
        (Value::Text(s), PgType::Float4) => {
            float::float4in(s)
                .map(Value::Float4)
                .map_err(|e| CastError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                })
        }
        (Value::Text(s), PgType::Float8) => {
            float::float8in(s)
                .map(Value::Float8)
                .map_err(|e| CastError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                })
        }
        (Value::Text(s), PgType::Numeric) => Numeric::parse(s)
            .map(Value::Numeric)
            .map_err(|e| numeric_parse_error(e, s)),

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
        (Value::Text(s), PgType::Timestamp) => {
            timestamp::parse(s)
                .map(Value::Timestamp)
                .map_err(|e| CastError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                })
        }

        // ---- text → interval (interval_in) ----
        (Value::Text(s), PgType::Interval) => {
            interval::parse(s)
                .map(Value::Interval)
                .map_err(|e| CastError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                })
        }

        // ---- text → timestamptz (timestamptz_in) ----
        (Value::Text(s), PgType::TimestampTz) => timestamptz::parse(s, &fmt.zone)
            .map(Value::TimestampTz)
            .map_err(|e| CastError {
                sqlstate: e.sqlstate,
                message: e.message,
            }),

        // ---- timestamp ↔ timestamptz ----
        // Not an identity: a zone-less `timestamp` is a wall clock, a
        // `timestamptz` is an instant, and the session zone is what relates
        // them. Both directions are exactly `AT TIME ZONE <session zone>`.
        (Value::Timestamp(m), PgType::TimestampTz) => {
            timestamptz::timestamp_at_session_zone(*m, &fmt.zone)
                .map(Value::TimestampTz)
                .map_err(cast_err)
        }
        (Value::TimestampTz(m), PgType::Timestamp) => {
            timestamptz::session_zone_wall_clock(*m, &fmt.zone)
                .map(Value::Timestamp)
                .map_err(cast_err)
        }

        // ---- text → date / time / timetz (input functions) ----
        (Value::Text(s), PgType::Date) => date::parse(s).map(Value::Date).map_err(|e| CastError {
            sqlstate: e.sqlstate,
            message: e.message,
        }),
        (Value::Text(s), PgType::Time) => time::parse(s).map(Value::Time).map_err(|e| CastError {
            sqlstate: e.sqlstate,
            message: e.message,
        }),
        (Value::Text(s), PgType::TimeTz) => {
            timetz::parse(s).map(Value::TimeTz).map_err(|e| CastError {
                sqlstate: e.sqlstate,
                message: e.message,
            })
        }

        // ---- date ↔ timestamp / timestamptz ----
        // A date widens to midnight. For `timestamptz` that is midnight *in the
        // session zone*, and the reverse takes the calendar date of the local
        // wall clock — `'2024-06-01 02:00:00+00'::timestamptz::date` is
        // 2024-05-31 in New York.
        (Value::Date(d), PgType::Timestamp) => date::to_timestamp_micros(*d)
            .map(Value::Timestamp)
            .map_err(|e| CastError {
                sqlstate: e.sqlstate,
                message: e.message,
            }),
        (Value::Date(d), PgType::TimestampTz) => date::to_timestamp_micros(*d)
            .map_err(|e| CastError {
                sqlstate: e.sqlstate,
                message: e.message,
            })
            .and_then(|midnight| {
                timestamptz::timestamp_at_session_zone(midnight, &fmt.zone)
                    .map(Value::TimestampTz)
                    .map_err(cast_err)
            }),
        (Value::Timestamp(m), PgType::Date) => Ok(Value::Date(date::from_timestamp_micros(*m))),
        (Value::TimestampTz(m), PgType::Date) => {
            let wall = timestamptz::session_zone_wall_clock(*m, &fmt.zone).map_err(cast_err)?;
            Ok(Value::Date(date::from_timestamp_micros(wall)))
        }

        // ---- time ↔ interval ----
        // time → interval keeps the microseconds as the time-of-day span;
        // interval → time takes the time-of-day part (mod one day).
        (Value::Time(usec), PgType::Interval) => Ok(Value::Interval(Interval {
            months: 0,
            days: 0,
            usec: *usec,
        })),
        (Value::Interval(iv), PgType::Time) => Ok(Value::Time(iv.usec.rem_euclid(86_400_000_000))),

        // ---- time ↔ timetz ----
        // time → timetz attaches the session zone (UTC); timetz → time drops it.
        (Value::Time(usec), PgType::TimeTz) => Ok(Value::TimeTz(TimeTz {
            usec: *usec,
            zone: 0,
        })),
        (Value::TimeTz(v), PgType::Time) => Ok(Value::Time(v.usec)),

        // ---- integer → numeric (exact) ----
        (Value::Int2(n), PgType::Numeric) => Ok(Value::Numeric(Numeric::from_i128(*n as i128))),
        (Value::Int4(n), PgType::Numeric) => Ok(Value::Numeric(Numeric::from_i128(*n as i128))),
        (Value::Int8(n), PgType::Numeric) => Ok(Value::Numeric(Numeric::from_i128(*n as i128))),

        // ---- float → numeric ----
        // PG's float→numeric keeps DBL_DIG (15) / FLT_DIG (6) significant digits
        // and always prints numeric in plain decimal.
        (Value::Float4(f), PgType::Numeric) => {
            Ok(Value::Numeric(Numeric::from_f64_sig(*f as f64, 6)))
        }
        (Value::Float8(f), PgType::Numeric) => Ok(Value::Numeric(Numeric::from_f64_sig(*f, 15))),

        // ---- numeric → integer (round half away from zero + range check) ----
        (Value::Numeric(n), PgType::Int2 | PgType::Int4 | PgType::Int8) => numeric_to_int(n, to),

        // ---- bit-string → integer (two's-complement of the target width) ----
        // PG has bittoint4/bittoint8 only — there is no bit→smallint cast, so
        // Int2 falls through to `cannot_coerce` below.
        (Value::Bit { len, data }, PgType::Int4 | PgType::Int8) => bit_to_int(*len, data, to),

        // ---- text → bit / varbit (bit_in / varbit_in) ----
        (Value::Text(s), PgType::Bit | PgType::Varbit) => crate::bit::input(s)
            .map(|(len, data)| Value::Bit { len, data })
            .map_err(|e| CastError {
                sqlstate: e.sqlstate,
                message: e.message,
            }),

        // ---- bit ↔ varbit: identity on the value (they share the storage);
        // any length rule is applied separately as a typmod coercion. ----
        (Value::Bit { .. }, PgType::Bit | PgType::Varbit) => Ok(v),

        // ---- text → bytea (byteain) ----
        (Value::Text(s), PgType::Bytea) => byteain(s).map(Value::Bytea),

        // ---- text → uuid (uuid_in) ----
        (Value::Text(s), PgType::Uuid) => {
            crate::uuid::parse(s)
                .map(Value::Uuid)
                .map_err(|e| CastError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                })
        }

        // ---- text → inet / cidr (inet_in / cidr_in) ----
        (Value::Text(s), PgType::Inet) => {
            crate::net::inet_in(s)
                .map(Value::Inet)
                .map_err(|e| CastError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                })
        }
        (Value::Text(s), PgType::Cidr) => {
            crate::net::cidr_in(s)
                .map(Value::Cidr)
                .map_err(|e| CastError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                })
        }

        // ---- cidr → inet (implicit in PG; the network keeps its masklen) ----
        (Value::Cidr(v), PgType::Inet) => Ok(Value::Inet(*v)),

        // ---- text → money (cash_in): $, thousands, parentheses-as-negative ----
        (Value::Text(s), PgType::Money) => {
            money::parse(s).map(Value::Money).map_err(|e| CastError {
                sqlstate: e.sqlstate,
                message: e.message,
            })
        }

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
        // int2/int4 always fit the unsigned 32-bit range once reinterpreted, and
        // a negative wraps (PG's `(-1)::oid` is 4294967295). int8 can exceed the
        // range, so it is bounds-checked: a magnitude past 32 bits is `22003 OID
        // out of range` rather than a silent truncation. oid->int8 is exact,
        // oid->int4 reinterprets the bit pattern.
        (Value::Int2(n), PgType::Oid) => Ok(Value::Oid(*n as u32)),
        (Value::Int4(n), PgType::Oid) => Ok(Value::Oid(*n as u32)),
        (Value::Int8(n), PgType::Oid) => int8_to_oid(*n),
        (Value::Oid(n), PgType::Int8) => Ok(Value::Int8(*n as i64)),
        (Value::Oid(n), PgType::Int4) => Ok(Value::Int4(*n as i32)),

        // ---- reg* -> oid/int ----
        // A reg* value is an OID under a name, so shedding the name is a plain
        // reinterpret. The reverse (oid -> reg*) is *not* here: it has to look
        // the name up, so the binder lowers it to a catalog-backed function.
        // reg* -> text is not here either; the text arm above renders the name.
        (Value::Reg(r), PgType::Oid) => Ok(Value::Oid(r.oid)),
        (Value::Reg(r), PgType::Int8) => Ok(Value::Int8(r.oid as i64)),
        (Value::Reg(r), PgType::Int4) => Ok(Value::Int4(r.oid as i32)),

        // ---- text -> oid (oidin: unsigned decimal, wrapping) ----
        (Value::Text(s), PgType::Oid) => text_to_oid(s),

        // ---- text → macaddr / macaddr8 (macaddr_in / macaddr8_in) ----
        (Value::Text(s), PgType::Macaddr) => crate::macaddr::parse_macaddr(s)
            .map(Value::Macaddr)
            .map_err(|e| CastError {
                sqlstate: e.sqlstate,
                message: e.message,
            }),
        (Value::Text(s), PgType::Macaddr8) => crate::macaddr::parse_macaddr8(s)
            .map(Value::Macaddr8)
            .map_err(|e| CastError {
                sqlstate: e.sqlstate,
                message: e.message,
            }),

        // ---- macaddr <-> macaddr8 ----
        (Value::Macaddr(b), PgType::Macaddr8) => Ok(Value::Macaddr8(crate::macaddr::expand6to8(b))),
        (Value::Macaddr8(b), PgType::Macaddr) => crate::macaddr::narrow8to6(b)
            .map(Value::Macaddr)
            .map_err(|e| CastError {
                sqlstate: e.sqlstate,
                message: e.message,
            }),

        // ---- text → tid (tidin). The reverse is the generic → text arm. ----
        (Value::Text(s), PgType::Tid) => crate::tid::parse(s)
            .map(|(block, offset)| Value::Tid { block, offset })
            .map_err(|e| CastError {
                sqlstate: e.sqlstate,
                message: e.message,
            }),

        // ---- text → xid / xid8 (xidin / xid8in) ----
        (Value::Text(s), PgType::Xid) => {
            crate::xid::xid_in(s)
                .map(Value::Xid)
                .map_err(|e| CastError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                })
        }
        (Value::Text(s), PgType::Xid8) => {
            crate::xid::xid8_in(s)
                .map(Value::Xid8)
                .map_err(|e| CastError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                })
        }

        // ---- xid8 → xid: PG's only declared cast for either type. It
        // truncates to the low 32 bits rather than range-checking. ----
        (Value::Xid8(v), PgType::Xid) => Ok(Value::Xid(*v as u32)),

        // ---- text → pg_lsn (pg_lsn_in) ----
        (Value::Text(s), PgType::PgLsn) => {
            crate::pg_lsn::parse(s)
                .map(Value::PgLsn)
                .map_err(|e| CastError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                })
        }

        // ---- text → point / lseg / path (point_in / lseg_in / path_in) ----
        (Value::Text(s), PgType::Point) => {
            crate::geo::parse_point(s)
                .map(Value::Point)
                .map_err(|e| CastError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                })
        }
        (Value::Text(s), PgType::Lseg) => {
            crate::geo::parse_lseg(s)
                .map(Value::Lseg)
                .map_err(|e| CastError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                })
        }
        (Value::Text(s), PgType::Path) => {
            crate::geo::parse_path(s)
                .map(Value::Path)
                .map_err(|e| CastError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                })
        }

        (Value::Text(s), PgType::Box) => {
            crate::geo::parse_box(s)
                .map(Value::Box)
                .map_err(|e| CastError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                })
        }
        (Value::Text(s), PgType::Line) => {
            crate::geo::parse_line(s)
                .map(Value::Line)
                .map_err(|e| CastError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                })
        }
        (Value::Text(s), PgType::Circle) => {
            crate::geo::parse_circle(s)
                .map(Value::Circle)
                .map_err(|e| CastError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                })
        }
        (Value::Text(s), PgType::Polygon) => crate::geo::parse_polygon(s)
            .map(Value::Polygon)
            .map_err(|e| CastError {
                sqlstate: e.sqlstate,
                message: e.message,
            }),

        // ---- intra-family geometric casts (all explicit in PG's pg_cast) ----
        // `lseg → point` is the segment's midpoint; the rest follow the same
        // "collapse to the natural summary shape" pattern.
        (Value::Lseg(l), PgType::Point) => Ok(Value::Point(crate::geo::lseg_center(l))),
        (Value::Point(p), PgType::Box) => Ok(Value::Box(crate::geo::box_from_point(p))),
        (Value::Box(b), PgType::Point) => Ok(Value::Point(crate::geo::box_center(b))),
        (Value::Box(b), PgType::Lseg) => Ok(Value::Lseg(crate::geo::box_diagonal(b))),
        (Value::Box(b), PgType::Circle) => Ok(Value::Circle(crate::geo::box_to_circle(b))),
        (Value::Box(b), PgType::Polygon) => Ok(Value::Polygon(crate::geo::box_to_polygon(b))),
        (Value::Circle(c), PgType::Point) => Ok(Value::Point(crate::geo::circle_center(c))),
        (Value::Circle(c), PgType::Box) => Ok(Value::Box(crate::geo::circle_to_box(c))),
        (Value::Circle(c), PgType::Polygon) => {
            crate::geo::circle_to_polygon(crate::geo::CIRCLE_POLYGON_NPTS, c)
                .map(Value::Polygon)
                .map_err(|e| CastError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                })
        }
        (Value::Polygon(p), PgType::Point) => Ok(Value::Point(crate::geo::poly_center(p))),
        (Value::Polygon(p), PgType::Box) => Ok(Value::Box(crate::geo::poly_bbox(p))),
        (Value::Polygon(p), PgType::Path) => Ok(Value::Path(crate::geo::poly_to_path(p))),
        (Value::Polygon(p), PgType::Circle) => {
            Ok(Value::Circle(crate::geo::circle_from_polygon(p)))
        }
        (Value::Path(p), PgType::Polygon) => crate::geo::path_to_polygon(p)
            .map(Value::Polygon)
            .map_err(|e| CastError {
                sqlstate: e.sqlstate,
                message: e.message,
            }),

        // ---- tsvector / tsquery input ----
        // The reverse direction (→ text/varchar/...) is handled by the generic
        // any-to-text arm above, which re-uses `encode_text_with`.
        (Value::Text(s), PgType::Tsvector) => crate::tsvector::tsvector_in(s)
            .map(Value::Tsvector)
            .map_err(ts_err),
        (Value::Text(s), PgType::Tsquery) => crate::tsquery::tsquery_in(s)
            .map(Value::Tsquery)
            .map_err(ts_err),
        // `tsvector` → `tsquery` is not a cast in PG, and vice versa; both fall
        // through to the generic "cannot cast" error below.

        // ---- json / jsonb I/O and conversions ----
        // `json`/`jsonb` → text/varchar/... is handled by the generic
        // any-to-text arm above (it re-uses `encode_text_with`).
        (Value::Text(s), PgType::Json) => json::json_in(s).map(Value::Json).map_err(json_err),
        (Value::Text(s), PgType::Jsonb) => json::jsonb_in(s).map(Value::Jsonb).map_err(json_err),
        // `text` → `jsonpath`: parse the SQL/JSON path language. jsonpath → text
        // is handled by the generic any-to-text arm (via `encode_text_with`).
        (Value::Text(s), PgType::Jsonpath) => jsonpath::jsonpath_in(s)
            .map(Value::Jsonpath)
            .map_err(json_err),
        // `json` → `jsonb`: re-parse the raw text into the canonical tree.
        (Value::Json(s), PgType::Jsonb) => json::jsonb_in(s).map(Value::Jsonb).map_err(json_err),
        // `jsonb` → `json`: the canonical serialization is always valid JSON.
        (Value::Jsonb(j), PgType::Json) => Ok(Value::Json(json::format(j))),
        // `jsonb` scalar → SQL scalar (the casts PG's `pg_cast` defines). A
        // wrong-kind jsonb is PG's `cannot cast jsonb <kind> to type <t>`.
        (Value::Jsonb(j), PgType::Bool) => match j {
            json::Jsonb::Bool(b) => Ok(Value::Bool(*b)),
            _ => Err(json_err(json::cannot_cast(j, to.name()))),
        },
        (
            Value::Jsonb(j),
            PgType::Numeric
            | PgType::Int2
            | PgType::Int4
            | PgType::Int8
            | PgType::Float4
            | PgType::Float8,
        ) => match j {
            json::Jsonb::Number(n) => cast_value(Value::Numeric(n.clone()), to, fmt),
            _ => Err(json_err(json::cannot_cast(j, to.name()))),
        },

        // ---- array I/O and conversions ----
        // `text` → `array` (`array_in`): parse the `{...}` literal, coercing each
        // element to the array's element type. array → text/varchar/... is
        // handled by the generic any-to-text arm above (via `encode_text_with`).
        (Value::Text(s), PgType::Array(elem_oid)) => {
            let elem = PgType::from_oid(elem_oid).ok_or_else(|| cannot_coerce(from, to))?;
            crate::array::array_in(s, elem, fmt)
                .map(|elems| Value::Array { elem, elems })
                .map_err(|e| CastError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                })
        }
        // `array` → `array` with a different element type: recast every element
        // (e.g. an `int4[]` literal assigned to a `numeric[]` column).
        (Value::Array { elems, .. }, PgType::Array(elem_oid)) => {
            let elem = PgType::from_oid(elem_oid).ok_or_else(|| cannot_coerce(from, to))?;
            let recast = elems
                .iter()
                .map(|e| cast_value(e.clone(), elem, fmt))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Array {
                elem,
                elems: recast,
            })
        }

        // ---- vector I/O ----
        // `text` → `oidvector`/`int2vector` (`oidvectorin`/`int2vectorin`).
        // vector → text goes through the generic any-to-text arm above.
        //
        // There is deliberately no `oid[]` conversion in either direction:
        // `oid[]::oidvector` is `cannot cast type oid[] to oidvector` in PG too,
        // and `oidvector::oid[]` yields a 0-based array this build cannot
        // represent — both fall through to `cannot_coerce`. See `crate::vector`.
        (Value::Text(s), PgType::Vector(kind)) => {
            crate::vector::vector_in(s, kind).map(|elems| Value::Vector { kind, elems })
        }

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
    i64::try_from(cents)
        .map(Value::Money)
        .map_err(|_| out_of_range(PgType::Money))
}

/// `cash_numeric`: exact value `cents / 100` rendered with display scale 2
/// (built from text so the scale is always two, e.g. `123.00`).
fn money_to_numeric(cents: i64) -> Numeric {
    let mag = (cents as i128).unsigned_abs();
    let s = format!(
        "{}{}.{:02}",
        if cents < 0 { "-" } else { "" },
        mag / 100,
        mag % 100
    );
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
                let (Some(o0), Some(o1), Some(o2)) = (octal_val(c), octal_val(d1), octal_val(d2))
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

/// `oidin`: parse an object identifier from text.
///
/// PG's `oidin` is `strtoul(s, &end, 0)` followed by a range check, so it is the
/// *same* acceptor as `xidin` under a different type name — hex and octal
/// spellings convert (`'0x1f'` is 31, `'010'` is 8), a negative wraps into the
/// unsigned range (`'-1'` is 4294967295), a magnitude outside
/// [`crate::xid::wraps_into_u32`]'s band is `22003`, and a trailing character or
/// an empty digit run is `22P02`.
///
/// Shared by the binder's `parse_unknown`, so `'42'::oid` and an untyped literal
/// in an oid context never drift — and by construction it now agrees with each
/// `oidvector` element, which scans through the same two functions.
pub fn text_to_oid(s: &str) -> Result<Value, CastError> {
    let t = s.trim();
    let (scanned, stop) = crate::xid::scan_prefix(t);
    match scanned {
        // A trailing character means the scan stopped early (`'1abc'`, `'08x'`),
        // which PG reports as a syntax error rather than a partial parse.
        Ok(_) if stop != t.len() => Err(invalid_input(PgType::Oid, s)),
        Ok(v) => crate::xid::wraps_into_u32(v)
            .map(Value::Oid)
            .ok_or_else(|| value_out_of_range(PgType::Oid, s)),
        Err(crate::xid::ScanError::Range) => Err(value_out_of_range(PgType::Oid, s)),
        Err(crate::xid::ScanError::Syntax) => Err(invalid_input(PgType::Oid, s)),
    }
}

/// `int8 -> oid`: reinterpret into the unsigned 32-bit range, wrapping a
/// negative (as `int4 -> oid` does), but reject a magnitude past 32 bits with
/// `22003` rather than silently truncating.
fn int8_to_oid(n: i64) -> Result<Value, CastError> {
    if oid_in_range(n) {
        Ok(Value::Oid(n as u32))
    } else {
        Err(out_of_range(PgType::Oid))
    }
}

/// Whether a signed value maps to an oid without losing its magnitude: any
/// unsigned 32-bit value, plus the negatives that wrap into that range.
fn oid_in_range(n: i64) -> bool {
    (-(u32::MAX as i64)..=u32::MAX as i64).contains(&n)
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
        PgType::Int2 => i16::try_from(v)
            .map(Value::Int2)
            .map_err(|_| out_of_range(ty)),
        PgType::Int4 => i32::try_from(v)
            .map(Value::Int4)
            .map_err(|_| out_of_range(ty)),
        PgType::Int8 => i64::try_from(v)
            .map(Value::Int8)
            .map_err(|_| out_of_range(ty)),
        _ => unreachable!("numeric_to_int called with {ty:?}"),
    }
}

/// Reinterpret a right-aligned bit string as the target integer's two's
/// complement. A bit string wider than the target errors (`<typename> out of
/// range`), matching PG's `bittoint4`/`bittoint8` (there is no bittoint2).
fn bit_to_int(len: u32, data: &[u8], ty: PgType) -> Result<Value, CastError> {
    match ty {
        PgType::Int4 if len <= 32 => Ok(Value::Int4(crate::bit::to_u64(len, data) as u32 as i32)),
        PgType::Int8 if len <= 64 => Ok(Value::Int8(crate::bit::to_u64(len, data) as i64)),
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
        // `int4 -> xid` is the coercion PG's `xideqint4` operator performs on
        // its right operand: the int's bits are reinterpreted, not
        // range-checked, so `'4294967295'::xid = -1` is true. It lives here
        // rather than in `cast_value` precisely because it must NOT be
        // reachable as a user-written cast — PG rejects `1::xid` with
        // `cannot cast type integer to xid`. Emitted only by `resolve_xid_op`.
        (Value::Int4(n), PgType::Xid) => Ok(Value::Xid(n as u32)),
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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn byteain_escape_and_hex() -> anyhow::Result<()> {
        // Plain ASCII (escape format, no backslashes) passes through.
        assert_eq!(byteain("abc")?, b"abc");
        assert_eq!(byteain("")?, b"");
        // Escape sequences.
        assert_eq!(byteain("\\\\")?, b"\\");
        assert_eq!(byteain("a\\001b")?, vec![b'a', 1, b'b']);
        // Hex format, with whitespace between pairs ignored (matches PG).
        assert_eq!(byteain("\\xdead")?, vec![0xde, 0xad]);
        assert_eq!(byteain("\\xDE AD")?, vec![0xde, 0xad]);
        assert_eq!(byteain("\\x")?, b"");
        // Malformed input is 22P02.
        assert_eq!(byteain("\\xabc").unwrap_err().sqlstate, "22P02"); // odd nibbles
        assert_eq!(byteain("\\xzz").unwrap_err().sqlstate, "22P02"); // non-hex
        assert_eq!(byteain("\\x a b").unwrap_err().sqlstate, "22P02"); // mid-byte space
        assert_eq!(byteain("\\9").unwrap_err().sqlstate, "22P02"); // bad escape
        assert_eq!(byteain("\\").unwrap_err().sqlstate, "22P02"); // dangling backslash

        Ok(())
    }

    #[test]
    fn float_to_int_edges() -> anyhow::Result<()> {
        assert_eq!(
            cast_value(Value::Float4(32767.4), PgType::Int2, &FmtCtx::utc(1))?,
            Value::Int2(32767)
        );
        assert_eq!(
            cast_value(Value::Float4(32767.6), PgType::Int2, &FmtCtx::utc(1))
                .unwrap_err()
                .message,
            "smallint out of range"
        );
        // f32 of 2147483647 rounds up to 2^31, out of int4 range.
        assert_eq!(
            cast_value(Value::Float4(2147483647.0), PgType::Int4, &FmtCtx::utc(1))
                .unwrap_err()
                .sqlstate,
            "22003"
        );
        assert_eq!(
            cast_value(Value::Float8(-9223372036854775808.5), PgType::Int8, &FmtCtx::utc(1))?,
            Value::Int8(i64::MIN)
        );

        Ok(())
    }

    #[test]
    fn float8_to_float4_range() {
        assert_eq!(
            cast_value(Value::Float8(1e70), PgType::Float4, &FmtCtx::utc(1))
                .unwrap_err()
                .message,
            "value out of range: overflow"
        );
        assert_eq!(
            cast_value(Value::Float8(1e-70), PgType::Float4, &FmtCtx::utc(1))
                .unwrap_err()
                .message,
            "value out of range: underflow"
        );
    }

    #[test]
    fn numeric_nan_to_float() -> anyhow::Result<()> {
        let n = cast_value(Value::Text("nan".into()), PgType::Numeric, &FmtCtx::utc_default())?;
        let f = cast_value(n, PgType::Float4, &FmtCtx::utc(1))?;
        assert_eq!(f.encode_text_with(&FmtCtx::utc_default()).as_deref(), Some("NaN"));

        Ok(())
    }

    #[test]
    fn numeric_rejects_garbage() {
        let e = cast_value(Value::Text("abc".into()), PgType::Numeric, &FmtCtx::utc_default()).unwrap_err();
        assert_eq!(e.sqlstate, "22P02");
        assert_eq!(e.message, "invalid input syntax for type numeric: \"abc\"");
        assert!(cast_value(Value::Text("1.5".into()), PgType::Numeric, &FmtCtx::utc_default()).is_ok());
    }

    fn cast(v: Value, to: PgType) -> Result<Value, CastError> {
        cast_value(v, to, &FmtCtx::utc(1))
    }

    /// PG's `bool -> text` cast spells the value out, even though the type's
    /// output function (used for display, `concat()` and the cast to `name`)
    /// stays `t`/`f`.
    #[test]
    fn bool_to_text_spells_the_value_out() -> anyhow::Result<()> {
        for to in [PgType::Text, PgType::Varchar, PgType::Bpchar] {
            assert_eq!(cast(Value::Bool(true), to)?, Value::Text("true".into()));
            assert_eq!(cast(Value::Bool(false), to)?, Value::Text("false".into()));
        }
        // `name` has no such cast, so it falls back to the output function.
        assert_eq!(
            cast(Value::Bool(true), PgType::Name)?,
            Value::Text("t".into())
        );
        assert_eq!(Value::Bool(true).encode_text_utc().as_deref(), Some("t"));

        Ok(())
    }

    #[test]
    fn text_to_int_ok_and_errors() -> anyhow::Result<()> {
        assert_eq!(
            cast(Value::Text("  123 ".into()), PgType::Int4)?,
            Value::Int4(123)
        );
        assert_eq!(
            cast(Value::Text("-9".into()), PgType::Int8)?,
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
        assert_eq!(
            e.message,
            "value \"99999999999\" is out of range for type integer"
        );
        let e = cast(Value::Text("99999".into()), PgType::Int2).unwrap_err();
        assert_eq!(
            e.message,
            "value \"99999\" is out of range for type smallint"
        );

        Ok(())
    }

    #[test]
    fn text_to_bool_ok_and_error() -> anyhow::Result<()> {
        assert_eq!(
            cast(Value::Text("t".into()), PgType::Bool)?,
            Value::Bool(true)
        );
        assert_eq!(
            cast(Value::Text("no".into()), PgType::Bool)?,
            Value::Bool(false)
        );
        assert_eq!(
            cast(Value::Text("on".into()), PgType::Bool)?,
            Value::Bool(true)
        );
        let e = cast(Value::Text("x".into()), PgType::Bool).unwrap_err();
        assert_eq!(e.sqlstate, "22P02");
        assert_eq!(e.message, "invalid input syntax for type boolean: \"x\"");

        Ok(())
    }

    #[test]
    fn only_int4_casts_to_bool() -> anyhow::Result<()> {
        assert_eq!(cast(Value::Int4(0), PgType::Bool)?, Value::Bool(false));
        assert_eq!(cast(Value::Int4(1), PgType::Bool)?, Value::Bool(true));
        assert_eq!(cast(Value::Int4(2), PgType::Bool)?, Value::Bool(true));
        assert_eq!(cast(Value::Int4(-1), PgType::Bool)?, Value::Bool(true));

        for value in [Value::Int2(1), Value::Int8(1)] {
            let error = cast(value, PgType::Bool).unwrap_err();
            assert_eq!(error.sqlstate, "42846");
        }

        Ok(())
    }

    /// int4 → bool is explicit-only, so an assignment falls back to the I/O
    /// conversion and accepts only what `boolin` accepts.
    #[test]
    fn assigning_int4_to_bool_is_narrower_than_casting() -> anyhow::Result<()> {
        assert_eq!(
            cast_value_assign(Value::Int4(0), PgType::Bool, &FmtCtx::utc(1))?,
            Value::Bool(false)
        );
        assert_eq!(
            cast_value_assign(Value::Int4(1), PgType::Bool, &FmtCtx::utc(1))?,
            Value::Bool(true)
        );
        for n in [2, -1] {
            let e = cast_value_assign(Value::Int4(n), PgType::Bool, &FmtCtx::utc(1)).unwrap_err();
            assert_eq!(e.sqlstate, "22P02");
            assert_eq!(
                e.message,
                format!("invalid input syntax for type boolean: \"{n}\"")
            );
        }
        // Every other pair is unaffected: assignment matches the plain cast.
        assert_eq!(
            cast_value_assign(Value::Int8(7), PgType::Int4, &FmtCtx::utc(1))?,
            Value::Int4(7)
        );
        assert_eq!(
            cast_value_assign(Value::Null, PgType::Bool, &FmtCtx::utc(1))?,
            Value::Null
        );

        Ok(())
    }

    #[test]
    fn int_to_numeric_is_exact() -> anyhow::Result<()> {
        assert_eq!(
            cast(Value::Int4(5), PgType::Numeric)?,
            Value::Numeric(Numeric::from_i128(5))
        );
        assert_eq!(
            cast(Value::Int8(-9), PgType::Numeric)?,
            Value::Numeric(Numeric::from_i128(-9))
        );

        Ok(())
    }

    fn num_text(v: Value, to: PgType) -> String {
        let value = match cast(v, to) {
            Ok(value) => value,
            Err(error) => panic!("numeric test cast failed: {error:?}"),
        };
        match value.encode_text_with(&FmtCtx::utc_default()) {
            Some(text) => text,
            None => panic!("numeric test value has no text encoding"),
        }
    }

    #[test]
    fn float_to_numeric_matches_pg() {
        // 15 significant digits for float8, plain decimal, no exponent.
        assert_eq!(num_text(Value::Float8(1.5), PgType::Numeric), "1.5");
        assert_eq!(num_text(Value::Float8(1.1), PgType::Numeric), "1.1");
        assert_eq!(
            num_text(Value::Float8(2.0 / 3.0), PgType::Numeric),
            "0.666666666666667"
        );
        assert_eq!(num_text(Value::Float8(100.0), PgType::Numeric), "100");
        assert_eq!(
            num_text(Value::Float8(1e20), PgType::Numeric),
            "100000000000000000000"
        );
        assert_eq!(num_text(Value::Float8(0.0015), PgType::Numeric), "0.0015");
        assert_eq!(num_text(Value::Float8(-0.0), PgType::Numeric), "0");
        // 6 significant digits for float4.
        assert_eq!(num_text(Value::Float4(123.456), PgType::Numeric), "123.456");
        assert_eq!(num_text(Value::Float4(0.1), PgType::Numeric), "0.1");
        // Non-finite carry through as numeric's own spellings.
        assert_eq!(
            num_text(Value::Float8(f64::INFINITY), PgType::Numeric),
            "Infinity"
        );
        assert_eq!(
            num_text(Value::Float8(f64::NEG_INFINITY), PgType::Numeric),
            "-Infinity"
        );
        assert_eq!(num_text(Value::Float8(f64::NAN), PgType::Numeric), "NaN");
    }

    fn numeric(s: &str) -> Value {
        match Numeric::parse(s) {
            Ok(value) => Value::Numeric(value),
            Err(error) => panic!("invalid numeric test fixture `{s}`: {error:?}"),
        }
    }

    #[test]
    fn numeric_to_int_rounds_half_away_from_zero() -> anyhow::Result<()> {
        assert_eq!(cast(numeric("0.5"), PgType::Int4)?, Value::Int4(1));
        assert_eq!(cast(numeric("1.5"), PgType::Int4)?, Value::Int4(2));
        assert_eq!(cast(numeric("2.5"), PgType::Int4)?, Value::Int4(3));
        assert_eq!(cast(numeric("-2.5"), PgType::Int4)?, Value::Int4(-3));
        assert_eq!(cast(numeric("2.4"), PgType::Int4)?, Value::Int4(2));
        assert_eq!(cast(numeric("2.6"), PgType::Int4)?, Value::Int4(3));
        assert_eq!(cast(numeric("1e3"), PgType::Int4)?, Value::Int4(1000));
        // Exact large int8 survives the i128 accumulator without precision loss.
        assert_eq!(
            cast(numeric("9223372036854775807"), PgType::Int8)?,
            Value::Int8(i64::MAX)
        );

        Ok(())
    }

    #[test]
    fn numeric_to_int_range_and_special() {
        let e = cast(numeric("99999999999"), PgType::Int4).unwrap_err();
        assert_eq!(e.sqlstate, "22003");
        assert_eq!(e.message, "integer out of range");
        assert_eq!(
            cast(numeric("1e30"), PgType::Int8).unwrap_err().message,
            "bigint out of range"
        );
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

    fn bits(s: &str) -> Value {
        let (len, data) = match crate::bit::from_binary(s) {
            Ok(value) => value,
            Err(error) => panic!("invalid bit-string test fixture `{s}`: {error:?}"),
        };
        Value::Bit { len, data }
    }

    #[test]
    fn bit_to_int_reinterprets_width() -> anyhow::Result<()> {
        assert_eq!(cast(bits("101"), PgType::Int4)?, Value::Int4(5));
        assert_eq!(cast(bits("1111"), PgType::Int4)?, Value::Int4(15));
        // 32 set bits fill int4's width → two's-complement -1.
        assert_eq!(cast(bits(&"1".repeat(32)), PgType::Int4)?, Value::Int4(-1));
        // A 16-bit value is zero-extended (positive) into the wider int4.
        assert_eq!(
            cast(bits("1000000000000000"), PgType::Int4)?,
            Value::Int4(32768)
        );
        // int8 keeps the same reinterpret semantics.
        assert_eq!(cast(bits(&"1".repeat(64)), PgType::Int8)?, Value::Int8(-1));
        // Wider than the target → out of range.
        let e = cast(bits(&"1".repeat(40)), PgType::Int4).unwrap_err();
        assert_eq!(e.sqlstate, "22003");
        assert_eq!(e.message, "integer out of range");

        Ok(())
    }

    #[test]
    fn bit_to_smallint_is_rejected() {
        // PG has bittoint4/bittoint8 but no bit→smallint cast.
        let e = cast(bits("101"), PgType::Int2).unwrap_err();
        assert_eq!(e.sqlstate, "42846");
        assert_eq!(e.message, "cannot cast type bit to smallint");
    }

    #[test]
    fn reinterpret_swaps_int_float_bits() -> anyhow::Result<()> {
        // int4 → float4: the bit pattern of 1 is the smallest subnormal float.
        assert_eq!(
            reinterpret_value(Value::Int4(1), PgType::Float4)?,
            Value::Float4(f32::from_bits(1))
        );
        // float4 → int4 is the inverse.
        assert_eq!(
            reinterpret_value(Value::Float4(f32::from_bits(1)), PgType::Int4)?,
            Value::Int4(1)
        );
        // int8 ↔ float8 over 64 bits.
        assert_eq!(
            reinterpret_value(Value::Int8(4607182418800017408), PgType::Float8)?,
            Value::Float8(1.0)
        );
        assert_eq!(
            reinterpret_value(Value::Float8(1.0), PgType::Int8)?,
            Value::Int8(4607182418800017408)
        );
        // Same representation (e.g. xfloat4→float4): identity relabel.
        assert_eq!(
            reinterpret_value(Value::Float4(1.5), PgType::Float4)?,
            Value::Float4(1.5)
        );
        // NULL passes through.
        assert_eq!(reinterpret_value(Value::Null, PgType::Float4)?, Value::Null);

        Ok(())
    }

    #[test]
    fn unsupported_pair_still_cannot_coerce() {
        let e = cast(Value::Bool(true), PgType::Int4).unwrap_err();
        assert_eq!(e.sqlstate, "42846");
        assert_eq!(e.message, "cannot cast type boolean to integer");
    }

    #[test]
    fn int8_to_oid_wraps_negatives_but_rejects_out_of_range() -> anyhow::Result<()> {
        // In range: exact.
        assert_eq!(cast(Value::Int8(2200), PgType::Oid)?, Value::Oid(2200));
        // Negative wraps like PG's `(-1)::oid` = 4294967295.
        assert_eq!(cast(Value::Int8(-1), PgType::Oid)?, Value::Oid(u32::MAX));
        assert_eq!(
            cast(Value::Int8(u32::MAX as i64), PgType::Oid)?,
            Value::Oid(u32::MAX)
        );
        // Past the 32-bit range errors (22003) instead of silently truncating.
        let e = cast(Value::Int8(u32::MAX as i64 + 1), PgType::Oid).unwrap_err();
        assert_eq!(e.sqlstate, "22003");

        Ok(())
    }

    #[test]
    fn text_to_oid_accepts_negatives_and_bounds() -> anyhow::Result<()> {
        assert_eq!(text_to_oid("42")?, Value::Oid(42));
        assert_eq!(text_to_oid("  2200 ")?, Value::Oid(2200));
        // oidin accepts a leading minus and wraps (PG: '-1'::oid = 4294967295).
        assert_eq!(text_to_oid("-1")?, Value::Oid(u32::MAX));
        // Magnitude past 32 bits is out of range, not a truncated success.
        assert_eq!(text_to_oid("4294967296").unwrap_err().sqlstate, "22003");
        // Non-numeric is 22P02.
        assert_eq!(text_to_oid("abc").unwrap_err().sqlstate, "22P02");

        // `oidin` is `strtoul(base 0)`, so hex and octal convert — and each of
        // these must agree with the same text as an `oidvector` element, which
        // scans through the same `xid::scan_prefix`. All probed against PG 18.4.
        assert_eq!(text_to_oid("0x1f")?, Value::Oid(31));
        assert_eq!(text_to_oid("0X1F")?, Value::Oid(31));
        assert_eq!(text_to_oid("010")?, Value::Oid(8));
        assert_eq!(text_to_oid("-2147483648")?, Value::Oid(2147483648));
        assert_eq!(text_to_oid("18446744073709551615")?, Value::Oid(u32::MAX));
        // The gap between the two accepted bands is out of range...
        assert_eq!(text_to_oid("-2147483649").unwrap_err().sqlstate, "22003");
        assert_eq!(text_to_oid("-4294967295").unwrap_err().sqlstate, "22003");
        // ...while a trailing character is a syntax error, not a partial parse.
        for bad in ["1abc", "08x", "1,2", "", "-", "0b11"] {
            assert_eq!(
                text_to_oid(bad).unwrap_err().sqlstate,
                "22P02",
                "input {bad:?}"
            );
        }

        Ok(())
    }

    /// `'x'::oid` and `'x'::oidvector` must accept exactly the same element
    /// text: both route through `xid::scan_prefix` and `xid::wraps_into_u32`,
    /// and this pins that they cannot drift apart.
    #[test]
    fn oid_and_oidvector_elements_agree() -> anyhow::Result<()> {
        for text in [
            "42",
            "0x1f",
            "010",
            "-1",
            "-2147483648",
            "18446744073709551615",
            "0",
        ] {
            let direct = text_to_oid(text)?;
            let via_vector = crate::vector::vector_in(text, crate::VectorKind::Oid)?;
            assert_eq!(via_vector, vec![direct.clone()], "input {text:?}");
        }
        for bad in ["-2147483649", "4294967296", "abc", "-"] {
            let direct = text_to_oid(bad).unwrap_err();
            let via_vector = crate::vector::vector_in(bad, crate::VectorKind::Oid).unwrap_err();
            assert_eq!(direct.sqlstate, via_vector.sqlstate, "input {bad:?}");
        }

        Ok(())
    }

    // --- session display zone (pinned against PostgreSQL 18.4) -------------

    fn ny() -> FmtCtx {
        FmtCtx::new(
            1,
            std::sync::Arc::new(
                crate::tz::SessionZone::resolve("America/New_York").expect("real zone"),
            ),
        )
    }

    fn text_to(s: &str, ty: PgType, fmt: &FmtCtx) -> anyhow::Result<String> {
        Ok(cast_value(Value::Text(s.to_string()), ty, fmt)?
            .encode_text_with(fmt)
            .ok_or_else(|| anyhow::anyhow!("cast produced NULL"))?)
    }

    /// Cast `v` to `ty` and render it, for the zone-aware cast assertions below.
    fn cast_to(v: Value, ty: PgType, fmt: &FmtCtx) -> anyhow::Result<String> {
        Ok(cast_value(v, ty, fmt)?
            .encode_text_with(fmt)
            .ok_or_else(|| anyhow::anyhow!("cast produced NULL"))?)
    }

    #[test]
    fn timestamp_and_timestamptz_convert_through_the_session_zone() -> anyhow::Result<()> {
        let ny = ny();
        // A zone-less wall clock is read in the session zone, not as UTC.
        let ts = cast_value(
            Value::Text("2024-06-01 12:00:00".into()),
            PgType::Timestamp,
            &ny,
        )?;
        assert_eq!(
            cast_to(ts.clone(), PgType::TimestampTz, &ny)?,
            "2024-06-01 12:00:00-04"
        );

        // ... and an instant renders as the local wall clock.
        let tstz = cast_value(
            Value::Text("2024-06-01 12:00:00+00".into()),
            PgType::TimestampTz,
            &ny,
        )?;
        assert_eq!(
            cast_to(tstz, PgType::Timestamp, &ny)?,
            "2024-06-01 08:00:00"
        );

        // Under UTC both directions stay the identity they always were.
        let utc = FmtCtx::utc_default();
        assert_eq!(
            cast_to(ts, PgType::TimestampTz, &utc)?,
            "2024-06-01 12:00:00+00"
        );
        Ok(())
    }

    #[test]
    fn date_and_timestamptz_convert_through_the_session_zone() -> anyhow::Result<()> {
        let ny = ny();
        // A date widens to midnight *local*, so its UTC instant is 04:00.
        assert_eq!(text_to("2024-06-01", PgType::Date, &ny)?, "2024-06-01");
        let d = cast_value(Value::Text("2024-06-01".into()), PgType::Date, &ny)?;
        assert_eq!(
            cast_to(d, PgType::TimestampTz, &ny)?,
            "2024-06-01 00:00:00-04"
        );

        // The reverse takes the calendar date of the *local* clock: 02:00 UTC
        // on the 1st is still the 31st in New York.
        let v = cast_value(
            Value::Text("2024-06-01 02:00:00+00".into()),
            PgType::TimestampTz,
            &ny,
        )?;
        assert_eq!(cast_to(v, PgType::Date, &ny)?, "2024-05-31");
        Ok(())
    }
}
