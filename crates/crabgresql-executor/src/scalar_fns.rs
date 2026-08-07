//! Scalar function evaluation.
//!
//! Clean-room (see AGENTS.md): every function reproduces PG's *observable*
//! result — value, and the SQLSTATE/message of any domain/range error — pinned
//! by the float regression corpus. The degree-based trig functions are built to
//! return exact values at the special angles the tests check (e.g.
//! `sind(30) = 0.5` exactly), using an independently-derived first-quadrant
//! reduction with libm calls forced to run time via `black_box`.

use std::hint::black_box;

use crabgresql_binder::GeoFn;
use crabgresql_binder::JsonFn;
use crabgresql_binder::JsonPathFn;
use crabgresql_binder::ScalarFn;
use crabgresql_binder::TsFn;
use crabgresql_pg_wire::sqlstate;
use crabgresql_types::json;
use crabgresql_types::json::Jsonb;
use crabgresql_types::jsonpath;
use crabgresql_types::{
    FmtCtx, Inet, Interval, Numeric, PgType, TimeTz, Value, bit, date, float, formatting,
    formatting_num, geo, interval, macaddr, money, net, pg_lsn, text, time, timestamp, timestamptz,
    timetz,
};

use crate::ExecError;
use crate::eval::array_elems;

const RADIANS_PER_DEGREE: f64 = 0.017_453_292_519_943_295;

fn err(sqlstate: &'static str, message: impl Into<String>) -> ExecError {
    ExecError::new(sqlstate, message)
}

fn overflow() -> ExecError {
    err(
        sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
        "value out of range: overflow",
    )
}

fn underflow() -> ExecError {
    err(
        sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
        "value out of range: underflow",
    )
}

fn out_of_range_input() -> ExecError {
    err(
        sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
        "input is out of range",
    )
}

/// Evaluate a scalar function. All functions are STRICT: a NULL argument yields
/// NULL without invoking the function.
///
/// Still a pure function — `fmt` is session *state*, not a handle to anything
/// side-effecting. It is threaded in because rendering a value to text
/// (`concat`, `format`, `… ::text`) and every `timestamptz` operation depend on
/// `extra_float_digits` and the session `TimeZone`. Functions needing a real
/// handle (sequences, the catalog) are dispatched ahead of this in `eval.rs`.
pub fn eval_scalar(func: ScalarFn, args: &[Value], fmt: &FmtCtx) -> Result<Value, ExecError> {
    // Non-strict string functions run even when arguments are NULL, so they are
    // handled before the STRICT NULL short-circuit below.
    match func {
        ScalarFn::Concat => {
            let mut out = String::new();
            for a in args {
                if let Some(s) = a.encode_text_with(fmt) {
                    out.push_str(&s);
                }
            }
            return Ok(Value::Text(out));
        }
        ScalarFn::ConcatWs => {
            // A NULL separator yields NULL; the remaining NULL args are skipped.
            let Some(sep) = args.first().and_then(|a| a.encode_text_with(fmt)) else {
                return Ok(Value::Null);
            };
            let parts: Vec<String> = args[1..]
                .iter()
                .filter_map(|a| a.encode_text_with(fmt))
                .collect();
            return Ok(Value::Text(parts.join(&sep)));
        }
        ScalarFn::Format => {
            // A NULL format string yields NULL.
            let Some(picture) = args.first().and_then(|a| a.encode_text_with(fmt)) else {
                return Ok(Value::Null);
            };
            let fmt_args: Vec<text::FormatArg> =
                args[1..].iter().map(|a| a.encode_text_with(fmt)).collect();
            return text::format(&picture, &fmt_args)
                .map(Value::Text)
                .map_err(text_err);
        }
        // quote_nullable is non-strict: a NULL argument becomes the text `NULL`.
        ScalarFn::QuoteNullable => {
            return Ok(Value::Text(text::quote_nullable(
                args[0].encode_text_with(fmt).as_deref(),
            )));
        }
        // array_to_string is strict on the array and delimiter, but not on the
        // optional null-string: a NULL there means NULL elements are omitted (as
        // in the two-argument form), so it must run before the STRICT check.
        ScalarFn::ArrayToString => {
            if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
                return Ok(Value::Null);
            }
            let delim = text(&args[1]);
            let null_str = match args.get(2) {
                Some(Value::Null) | None => None,
                Some(v) => Some(text(v)),
            };
            let mut parts: Vec<String> = Vec::new();
            for v in array_elems(&args[0]) {
                match v {
                    Value::Null => {
                        if let Some(ns) = null_str {
                            parts.push(ns.to_string());
                        }
                    }
                    _ => parts.push(v.encode_text_with(fmt).unwrap_or_default()),
                }
            }
            return Ok(Value::Text(parts.join(delim)));
        }
        _ => {}
    }
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    match func {
        ScalarFn::BoolEq | ScalarFn::BoolNe => {
            let (Value::Bool(left), Value::Bool(right)) = (&args[0], &args[1]) else {
                unreachable!("boolean comparison received non-boolean arguments")
            };
            let equal = left == right;
            return Ok(Value::Bool(if matches!(func, ScalarFn::BoolEq) {
                equal
            } else {
                !equal
            }));
        }
        // --- geometric (point / lseg) ---
        ScalarFn::Geo(g) => return eval_geo(g, args),
        // The server's build identity: a compile-time constant, so unlike the
        // rest of the version/session surface it needs no handle at all.
        ScalarFn::Version => {
            return Ok(Value::Text(crabgresql_types::version::version_string()));
        }
        // The encoding table is a compile-time constant too, so these two are
        // pure despite sitting beside the session-identity functions in SQL.
        // Both are STRICT, and both report a sentinel rather than NULL for a
        // miss: the empty string one way, `-1` the other.
        ScalarFn::PgEncodingToChar => {
            return Ok(Value::Text(
                crabgresql_types::encoding::encoding_to_char(i4(&args[0])).to_string(),
            ));
        }
        ScalarFn::PgCharToEncoding => {
            return Ok(Value::Int4(crabgresql_types::encoding::char_to_encoding(
                text(&args[0]),
            )));
        }
        // Sequence functions are side-effecting and are dispatched by `eval`
        // (which has the session's SequenceOps handle) before it ever reaches
        // this pure evaluator; seeing one here is an internal wiring error.
        ScalarFn::Nextval | ScalarFn::Currval | ScalarFn::Setval | ScalarFn::Lastval => {
            return Err(ExecError::new(
                sqlstate::INTERNAL_ERROR,
                "sequence function reached the pure scalar evaluator",
            ));
        }
        // Likewise the catalog functions, which `eval` dispatches through the
        // session's CatalogOps handle.
        // `pg_typeof` is here too: it needs the catalog to name a user type, and
        // it is not STRICT, so the NULL short-circuit above would be wrong for it.
        ScalarFn::PgGetUserById
        | ScalarFn::PgTableIsVisible
        | ScalarFn::PgTypeof(_)
        | ScalarFn::CurrentDatabase
        | ScalarFn::CurrentSchema
        | ScalarFn::CurrentSchemas
        | ScalarFn::CurrentUser
        | ScalarFn::SessionUser
        | ScalarFn::PgMyTempSchema
        | ScalarFn::PgIsOtherTempSchema => {
            return Err(ExecError::new(
                sqlstate::INTERNAL_ERROR,
                "catalog function reached the pure scalar evaluator",
            ));
        }
        // Likewise the deparse/formatting functions, dispatched by `eval` because
        // they are not uniformly STRICT.
        ScalarFn::FormatType | ScalarFn::PgGetExpr => {
            return Err(ExecError::new(
                sqlstate::INTERNAL_ERROR,
                "deparse function reached the pure scalar evaluator",
            ));
        }
        // --- tid accessors (STRICT) ---
        ScalarFn::TidBlock => {
            let (block, _) = tid(&args[0]);
            return Ok(Value::Int8(i64::from(block)));
        }
        ScalarFn::TidOffset => {
            let (_, offset) = tid(&args[0]);
            return Ok(Value::Int4(i32::from(offset)));
        }
        ScalarFn::Xid8Cmp => {
            let (l, r) = (xid8(&args[0]), xid8(&args[1]));
            return Ok(Value::Int4(match l.cmp(&r) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }));
        }
        // --- pg_lsn arithmetic (STRICT). The binder always puts the LSN first. ---
        ScalarFn::PgLsnMi => {
            return Ok(Value::Numeric(pg_lsn::sub(lsn(&args[0]), lsn(&args[1]))));
        }
        ScalarFn::PgLsnPli => {
            return pg_lsn::add_numeric(lsn(&args[0]), num(&args[1]))
                .map(Value::PgLsn)
                .map_err(|e| ExecError::new(e.sqlstate, e.message));
        }
        ScalarFn::PgLsnMii => {
            return pg_lsn::sub_numeric(lsn(&args[0]), num(&args[1]))
                .map(Value::PgLsn)
                .map_err(|e| ExecError::new(e.sqlstate, e.message));
        }
        ScalarFn::NumericPgLsn => {
            return pg_lsn::from_numeric(num(&args[0]))
                .map(Value::PgLsn)
                .map_err(|e| ExecError::new(e.sqlstate, e.message));
        }
        // --- jsonpath (STRICT: any NULL arg already short-circuited to NULL) ---
        ScalarFn::JsonPath(f) => return eval_jsonpath(f, args),
        // --- json/jsonb extraction (STRICT on the target and key) ---
        ScalarFn::Json(f) => return eval_json(f, args),
        ScalarFn::Ts(f) => return eval_ts(f, args),
        // --- array containment / size (STRICT) ---
        ScalarFn::ArrayContains => return Ok(Value::Bool(array_contains(&args[0], &args[1]))),
        ScalarFn::ArrayContainedBy => {
            return Ok(Value::Bool(array_contains(&args[1], &args[0])));
        }
        ScalarFn::ArrayOverlap => return Ok(Value::Bool(array_overlap(&args[0], &args[1]))),
        ScalarFn::ArrayLength | ScalarFn::ArrayUpper => {
            // `array_length(arr, dim)` / `array_upper(arr, dim)`: only dimension
            // 1 exists here, where the 1-based upper bound equals the length. An
            // empty array or any other dimension yields NULL.
            let elems = array_elems(&args[0]);
            return Ok(if i4(&args[1]) == 1 && !elems.is_empty() {
                Value::Int4(elems.len() as i32)
            } else {
                Value::Null
            });
        }
        ScalarFn::Cardinality => {
            return Ok(Value::Int4(array_elems(&args[0]).len() as i32));
        }
        // array_cat/append/prepend are non-strict and need the result element
        // type, so `eval` dispatches them before this pure evaluator; reaching
        // here is an internal wiring error.
        ScalarFn::ArrayCat | ScalarFn::ArrayAppend | ScalarFn::ArrayPrepend => {
            return Err(ExecError::new(
                sqlstate::INTERNAL_ERROR,
                "array constructor function reached the pure scalar evaluator",
            ));
        }
        // --- string functions ---
        ScalarFn::TextConcat => {
            return Ok(Value::Text(format!("{}{}", text(&args[0]), text(&args[1]))));
        }
        ScalarFn::Length => return Ok(Value::Int4(text::char_length(text(&args[0])))),
        ScalarFn::OctetLength => return Ok(Value::Int4(text::octet_length(text(&args[0])))),
        ScalarFn::BitLength => return Ok(Value::Int4(text::bit_length(text(&args[0])))),
        ScalarFn::Upper => return Ok(Value::Text(text::upper(text(&args[0])))),
        ScalarFn::Lower => return Ok(Value::Text(text::lower(text(&args[0])))),
        ScalarFn::Initcap => return Ok(Value::Text(text::initcap(text(&args[0])))),
        ScalarFn::Substr => {
            let len = args.get(2).map(i4);
            return text::substr(text(&args[0]), i4(&args[1]), len)
                .map(Value::Text)
                .map_err(text_err);
        }
        ScalarFn::StrPos => {
            return Ok(Value::Int4(text::strpos(text(&args[0]), text(&args[1]))));
        }
        ScalarFn::Overlay => {
            let count = args.get(3).map(i4);
            return text::overlay(text(&args[0]), text(&args[1]), i4(&args[2]), count)
                .map(Value::Text)
                .map_err(text_err);
        }
        ScalarFn::Ltrim | ScalarFn::Rtrim | ScalarFn::Btrim => {
            let chars = args.get(1).map(|v| text(v)).unwrap_or(" ");
            let side = match func {
                ScalarFn::Ltrim => text::TrimSide::Leading,
                ScalarFn::Rtrim => text::TrimSide::Trailing,
                _ => text::TrimSide::Both,
            };
            return Ok(Value::Text(text::trim(text(&args[0]), chars, side)));
        }
        ScalarFn::Lpad | ScalarFn::Rpad => {
            let fill = args.get(2).map(|v| text(v)).unwrap_or(" ");
            let left = matches!(func, ScalarFn::Lpad);
            return text::pad(text(&args[0]), i4(&args[1]), fill, left)
                .map(Value::Text)
                .map_err(text_err);
        }
        ScalarFn::Replace => {
            return Ok(Value::Text(text::replace(
                text(&args[0]),
                text(&args[1]),
                text(&args[2]),
            )));
        }
        ScalarFn::Translate => {
            return Ok(Value::Text(text::translate(
                text(&args[0]),
                text(&args[1]),
                text(&args[2]),
            )));
        }
        ScalarFn::Repeat => {
            return text::repeat(text(&args[0]), i4(&args[1]))
                .map(Value::Text)
                .map_err(text_err);
        }
        ScalarFn::Reverse => return Ok(Value::Text(text::reverse(text(&args[0])))),
        ScalarFn::Left => return Ok(Value::Text(text::left(text(&args[0]), i4(&args[1])))),
        ScalarFn::Right => return Ok(Value::Text(text::right(text(&args[0]), i4(&args[1])))),
        ScalarFn::Ascii => return Ok(Value::Int4(text::ascii(text(&args[0])))),
        ScalarFn::Chr => return text::chr(i4(&args[0])).map(Value::Text).map_err(text_err),
        ScalarFn::SplitPart => {
            return text::split_part(text(&args[0]), text(&args[1]), i4(&args[2]))
                .map(Value::Text)
                .map_err(text_err);
        }
        ScalarFn::StartsWith => {
            return Ok(Value::Bool(text::starts_with(
                text(&args[0]),
                text(&args[1]),
            )));
        }
        ScalarFn::ToHex => return Ok(Value::Text(text::to_hex_i32(i4(&args[0])))),
        ScalarFn::ToHexInt8 => return Ok(Value::Text(text::to_hex_i64(i8(&args[0])))),
        ScalarFn::Like | ScalarFn::ILike => {
            let ci = matches!(func, ScalarFn::ILike);
            let escape = escape_char(args.get(2))?;
            return text::like(text(&args[0]), text(&args[1]), escape, ci)
                .map(Value::Bool)
                .map_err(text_err);
        }
        ScalarFn::RegexMatch | ScalarFn::RegexIMatch => {
            let ci = matches!(func, ScalarFn::RegexIMatch);
            return text::regex_match(text(&args[0]), text(&args[1]), ci)
                .map(Value::Bool)
                .map_err(text_err);
        }
        // The `regexp_*` family shares one variant per function across all its
        // arities, so absent trailing arguments fall back to PG's defaults.
        ScalarFn::RegexpReplace => {
            // Two 4-argument forms overlap here, so the fourth argument is
            // discriminated by type rather than by arity: `text` is the flags
            // form, `int4` the `start [, n [, flags]]` one.
            let (start, n, flags) = match args.get(3) {
                None | Some(Value::Text(_)) => (1, None, args.get(3).map_or("", text)),
                // Without an explicit `n` this form replaces just the first
                // match at or after `start`; `n = 0` is what asks for all.
                _ => (
                    i4(&args[3]),
                    Some(args.get(4).map_or(1, i4)),
                    args.get(5).map_or("", text),
                ),
            };
            return text::regexp_replace_at(
                text(&args[0]),
                text(&args[1]),
                text(&args[2]),
                start,
                n,
                flags,
            )
            .map(Value::Text)
            .map_err(text_err);
        }
        ScalarFn::RegexpLike => {
            let flags = args.get(2).map_or("", text);
            return text::regexp_like(text(&args[0]), text(&args[1]), flags)
                .map(Value::Bool)
                .map_err(text_err);
        }
        ScalarFn::RegexpCount => {
            let start = args.get(2).map_or(1, i4);
            let flags = args.get(3).map_or("", text);
            return text::regexp_count(text(&args[0]), text(&args[1]), start, flags)
                .map(Value::Int4)
                .map_err(text_err);
        }
        ScalarFn::RegexpSubstr => {
            let start = args.get(2).map_or(1, i4);
            let n = args.get(3).map_or(1, i4);
            let flags = args.get(4).map_or("", text);
            let subexpr = args.get(5).map_or(0, i4);
            return text::regexp_substr(text(&args[0]), text(&args[1]), start, n, flags, subexpr)
                .map(|found| found.map_or(Value::Null, Value::Text))
                .map_err(text_err);
        }
        ScalarFn::SimilarTo => {
            let escape = escape_char(args.get(2))?;
            return text::similar_to_match(text(&args[0]), text(&args[1]), escape)
                .map(Value::Bool)
                .map_err(text_err);
        }
        ScalarFn::SubstringRegex => {
            return text::substring_regex(text(&args[0]), text(&args[1]))
                .map(|found| found.map_or(Value::Null, Value::Text))
                .map_err(text_err);
        }
        ScalarFn::SubstringSimilar => {
            let escape = escape_char(args.get(2))?;
            return text::substring_similar(text(&args[0]), text(&args[1]), escape)
                .map(|found| found.map_or(Value::Null, Value::Text))
                .map_err(text_err);
        }
        ScalarFn::Encode => {
            return text::encode(bytea(&args[0]), text(&args[1]))
                .map(Value::Text)
                .map_err(text_err);
        }
        ScalarFn::Decode => {
            return text::decode(text(&args[0]), text(&args[1]))
                .map(Value::Bytea)
                .map_err(text_err);
        }
        ScalarFn::QuoteIdent => return Ok(Value::Text(text::quote_ident(text(&args[0])))),
        ScalarFn::QuoteLiteral => return Ok(Value::Text(text::quote_literal(text(&args[0])))),
        ScalarFn::VarcharTypmod => {
            // A third arg of 0 selects assignment semantics (error on overflow);
            // an explicit `::varchar(n)` cast (no third arg) truncates silently.
            let explicit = args.get(2).map(|v| i4(v) != 0).unwrap_or(true);
            return text::varchar_input(text(&args[0]), i4(&args[1]), explicit)
                .map(Value::Text)
                .map_err(text_err);
        }
        ScalarFn::BpcharTypmod => {
            let explicit = args.get(2).map(|v| i4(v) != 0).unwrap_or(true);
            return text::bpchar_input(text(&args[0]), i4(&args[1]), explicit)
                .map(Value::Text)
                .map_err(text_err);
        }
        ScalarFn::NameInput => return Ok(Value::Text(text::name_input(text(&args[0])))),
        ScalarFn::BpcharToText => return Ok(Value::Text(text::bpchar_rtrim(text(&args[0])))),
        // --- integer bitwise / shift ---
        // PG's int2 shifts widen to int32, shift there, then truncate back, and
        // check nothing on the way — which is why `(-1)::int2 << 15` is -32768
        // rather than an overflow error. int4/int8 shift in their own width.
        // The count is taken modulo the width, as the underlying machine
        // instruction does; PG leaves that case to the hardware too.
        ScalarFn::IntShl | ScalarFn::IntShr => {
            let n = i4(&args[1]) as u32;
            let left = matches!(func, ScalarFn::IntShl);
            return Ok(match &args[0] {
                Value::Int2(a) => {
                    let wide = *a as i32;
                    let r = if left {
                        wide.wrapping_shl(n)
                    } else {
                        wide.wrapping_shr(n)
                    };
                    Value::Int2(r as i16)
                }
                Value::Int4(a) => Value::Int4(if left {
                    a.wrapping_shl(n)
                } else {
                    a.wrapping_shr(n)
                }),
                Value::Int8(a) => Value::Int8(if left {
                    a.wrapping_shl(n)
                } else {
                    a.wrapping_shr(n)
                }),
                other => unreachable!("expected an integer, got {other:?}"),
            });
        }
        ScalarFn::IntAnd | ScalarFn::IntOr | ScalarFn::IntXor => {
            let apply = |a: i64, b: i64| match func {
                ScalarFn::IntAnd => a & b,
                ScalarFn::IntOr => a | b,
                _ => a ^ b,
            };
            return Ok(match (&args[0], &args[1]) {
                (Value::Int2(a), Value::Int2(b)) => Value::Int2(apply(*a as i64, *b as i64) as i16),
                (Value::Int4(a), Value::Int4(b)) => Value::Int4(apply(*a as i64, *b as i64) as i32),
                (Value::Int8(a), Value::Int8(b)) => Value::Int8(apply(*a, *b)),
                (a, b) => unreachable!("expected two integers of one width, got {a:?} / {b:?}"),
            });
        }
        ScalarFn::IntNot => {
            return Ok(match &args[0] {
                Value::Int2(a) => Value::Int2(!a),
                Value::Int4(a) => Value::Int4(!a),
                Value::Int8(a) => Value::Int8(!a),
                other => unreachable!("expected an integer, got {other:?}"),
            });
        }
        // --- bit / varbit ---
        ScalarFn::BitNot => {
            let (len, data) = bits(&args[0]);
            let (len, data) = bit::not(len, data);
            return Ok(Value::Bit { len, data });
        }
        ScalarFn::BitAnd | ScalarFn::BitOr | ScalarFn::BitXor => {
            let (la, da) = bits(&args[0]);
            let (lb, db) = bits(&args[1]);
            let r = match func {
                ScalarFn::BitAnd => bit::and(la, da, lb, db),
                ScalarFn::BitOr => bit::or(la, da, lb, db),
                _ => bit::xor(la, da, lb, db),
            };
            return r
                .map(|(len, data)| Value::Bit { len, data })
                .map_err(bit_err);
        }
        ScalarFn::BitConcat => {
            let (la, da) = bits(&args[0]);
            let (lb, db) = bits(&args[1]);
            let (len, data) = bit::concat(la, da, lb, db);
            return Ok(Value::Bit { len, data });
        }
        ScalarFn::BitShl | ScalarFn::BitShr => {
            let (len, data) = bits(&args[0]);
            let n = i4(&args[1]);
            let (len, data) = if matches!(func, ScalarFn::BitShl) {
                bit::shift_left(len, data, n)
            } else {
                bit::shift_right(len, data, n)
            };
            return Ok(Value::Bit { len, data });
        }
        ScalarFn::BitLen => return Ok(Value::Int4(bit::length(bits(&args[0]).0))),
        ScalarFn::BitCount => {
            let (len, data) = bits(&args[0]);
            return Ok(Value::Int8(bit::bit_count(len, data)));
        }
        ScalarFn::GetBit => {
            let (len, data) = bits(&args[0]);
            return bit::get_bit(len, data, i4(&args[1]))
                .map(Value::Int4)
                .map_err(bit_err);
        }
        ScalarFn::SetBit => {
            let (len, data) = bits(&args[0]);
            return bit::set_bit(len, data, i4(&args[1]), i4(&args[2]))
                .map(|(len, data)| Value::Bit { len, data })
                .map_err(bit_err);
        }
        ScalarFn::SubstrBit => {
            let (len, data) = bits(&args[0]);
            let count = args.get(2).map(i4);
            return bit::substring(len, data, i4(&args[1]), count)
                .map(|(len, data)| Value::Bit { len, data })
                .map_err(bit_err);
        }
        ScalarFn::BitPosition => {
            let (sl, sd) = bits(&args[0]);
            let (ul, ud) = bits(&args[1]);
            return Ok(Value::Int4(bit::position(sl, sd, ul, ud)));
        }
        ScalarFn::OverlayBit => {
            let (sl, sd) = bits(&args[0]);
            let (rl, rd) = bits(&args[1]);
            let count = args.get(3).map(i4);
            return bit::overlay(sl, sd, rl, rd, i4(&args[2]), count)
                .map(|(len, data)| Value::Bit { len, data })
                .map_err(bit_err);
        }
        ScalarFn::BitTypmod | ScalarFn::VarbitTypmod => {
            // A third arg of 0 selects assignment semantics (error on mismatch);
            // an explicit `::bit(n)` cast (no third arg) truncates/pads.
            let (len, data) = bits(&args[0]);
            let explicit = args.get(2).map(|v| i4(v) != 0).unwrap_or(true);
            return bit::coerce(
                len,
                data,
                i4(&args[1]),
                matches!(func, ScalarFn::VarbitTypmod),
                explicit,
            )
            .map(|(len, data)| Value::Bit { len, data })
            .map_err(bit_err);
        }
        ScalarFn::Float4Send => {
            let f = f4(&args[0]);
            return Ok(Value::Bytea(f.to_be_bytes().to_vec()));
        }
        ScalarFn::Float8Send => {
            let f = f8(&args[0]);
            return Ok(Value::Bytea(f.to_be_bytes().to_vec()));
        }
        // `pg_input_is_valid` is dispatched by `eval`, which holds the catalog a
        // `reg*` target's input function needs; reaching the pure evaluator is
        // an internal wiring error.
        ScalarFn::PgInputIsValid => {
            return Err(ExecError::new(
                sqlstate::INTERNAL_ERROR,
                "pg_input_is_valid reached the pure scalar evaluator",
            ));
        }
        ScalarFn::DatePart => {
            // `None` is SQL NULL (an oscillating field on ±infinity).
            return Ok(
                match timestamp::date_part(text(&args[0]), ts(&args[1])).map_err(ts_err)? {
                    Some(v) => Value::Float8(v),
                    None => Value::Null,
                },
            );
        }
        ScalarFn::Extract => {
            return Ok(
                match timestamp::extract(text(&args[0]), ts(&args[1])).map_err(ts_err)? {
                    Some(n) => Value::Numeric(n),
                    None => Value::Null,
                },
            );
        }
        ScalarFn::DateTrunc => {
            return timestamp::date_trunc(text(&args[0]), ts(&args[1]))
                .map(Value::Timestamp)
                .map_err(ts_err);
        }
        ScalarFn::DateBin => {
            return timestamp::bin(iv(&args[0]), ts(&args[1]), ts(&args[2]))
                .map(Value::Timestamp)
                .map_err(ts_err);
        }
        ScalarFn::Isfinite => {
            return Ok(Value::Bool(timestamp::is_finite(ts(&args[0]))));
        }
        ScalarFn::MakeTimestamp => {
            return timestamp::make_timestamp(
                i4(&args[0]) as i64,
                i4(&args[1]) as i64,
                i4(&args[2]) as i64,
                i4(&args[3]) as i64,
                i4(&args[4]) as i64,
                f8(&args[5]),
            )
            .map(Value::Timestamp)
            .map_err(ts_err);
        }
        ScalarFn::DatePartTz => {
            return Ok(
                match timestamptz::date_part(text(&args[0]), tstz(&args[1]), &fmt.zone)
                    .map_err(ts_err)?
                {
                    Some(v) => Value::Float8(v),
                    None => Value::Null,
                },
            );
        }
        ScalarFn::ExtractTz => {
            return Ok(
                match timestamptz::extract(text(&args[0]), tstz(&args[1]), &fmt.zone)
                    .map_err(ts_err)?
                {
                    Some(n) => Value::Numeric(n),
                    None => Value::Null,
                },
            );
        }
        ScalarFn::DateTruncTz => {
            // The 3rd argument (a text zone) is optional; without it the
            // truncation happens in the session zone.
            return match args.get(2) {
                Some(zone) => {
                    timestamptz::date_trunc_in_zone(text(&args[0]), tstz(&args[1]), text(zone))
                }
                None => timestamptz::date_trunc(text(&args[0]), tstz(&args[1]), fmt.zone.zone()),
            }
            .map(Value::TimestampTz)
            .map_err(ts_err);
        }
        // Binning a `timestamptz` works on the UTC instant, so unlike
        // `date_trunc` above it never consults the session zone.
        ScalarFn::DateBinTz => {
            return timestamp::bin(iv(&args[0]), tstz(&args[1]), tstz(&args[2]))
                .map(Value::TimestampTz)
                .map_err(ts_err);
        }
        ScalarFn::IsfiniteTz => {
            return Ok(Value::Bool(timestamptz::is_finite_tstz(tstz(&args[0]))));
        }
        ScalarFn::MakeTimestampTz => {
            // The 7th argument (a text zone) is optional.
            let zone = args.get(6).map(text);
            return timestamptz::make_timestamptz(
                i4(&args[0]) as i64,
                i4(&args[1]) as i64,
                i4(&args[2]) as i64,
                i4(&args[3]) as i64,
                i4(&args[4]) as i64,
                f8(&args[5]),
                zone,
                &fmt.zone,
            )
            .map(Value::TimestampTz)
            .map_err(ts_err);
        }
        ScalarFn::TimezoneToTz => {
            // timezone(zone, timestamp) -> timestamptz.
            return timestamptz::timestamp_at_zone(text(&args[0]), ts(&args[1]))
                .map(Value::TimestampTz)
                .map_err(ts_err);
        }
        ScalarFn::TimezoneToTs => {
            // timezone(zone, timestamptz) -> timestamp.
            return timestamptz::at_zone_to_timestamp(text(&args[0]), tstz(&args[1]))
                .map(Value::Timestamp)
                .map_err(ts_err);
        }
        ScalarFn::TimezoneIntervalToTz => {
            // timezone(interval, timestamp) -> timestamptz.
            let off = timestamptz::interval_zone_offset(iv(&args[0]), fmt.interval_style)
                .map_err(ts_err)?;
            return timestamptz::timestamp_at_offset(off, ts(&args[1]))
                .map(Value::TimestampTz)
                .map_err(ts_err);
        }
        ScalarFn::TimezoneIntervalToTs => {
            // timezone(interval, timestamptz) -> timestamp.
            let off = timestamptz::interval_zone_offset(iv(&args[0]), fmt.interval_style)
                .map_err(ts_err)?;
            return timestamptz::at_offset_to_timestamp(off, tstz(&args[1]))
                .map(Value::Timestamp)
                .map_err(ts_err);
        }
        ScalarFn::TimezoneLocalToTz => {
            // timestamp AT LOCAL -> timestamptz.
            return timestamptz::timestamp_at_session_zone(ts(&args[0]), &fmt.zone)
                .map(Value::TimestampTz)
                .map_err(ts_err);
        }
        ScalarFn::TimezoneLocalToTs => {
            // timestamptz AT LOCAL -> timestamp.
            return timestamptz::session_zone_wall_clock(tstz(&args[0]), &fmt.zone)
                .map(Value::Timestamp)
                .map_err(ts_err);
        }
        ScalarFn::TimezoneTimeTz => {
            // timezone(zone, timetz) -> timetz.
            return timetz::at_zone_named(
                ttz(&args[1]),
                text(&args[0]),
                fmt.xact_start().map_err(clock_err)?,
            )
            .map(Value::TimeTz)
            .map_err(timetz_err);
        }
        ScalarFn::TimezoneIntervalTimeTz => {
            // timezone(interval, timetz) -> timetz.
            let off = timestamptz::interval_zone_offset(iv(&args[0]), fmt.interval_style)
                .map_err(ts_err)?;
            return Ok(Value::TimeTz(timetz::at_zone(ttz(&args[1]), off)));
        }
        ScalarFn::TimezoneLocalTimeTz => {
            // timetz AT LOCAL -> timetz.
            //
            // The session zone is read through `zone_offset_today`, the same
            // accessor `timetz_in` and the `time -> timetz` cast use for a value
            // with no zone of its own. Reading it any other way here would make
            // `AT LOCAL` shift a zone-less literal instead of leaving it alone:
            // under a DST session zone the value would be built at one offset
            // and then rotated to another, inventing an hour of wall clock.
            let off = fmt.zone_offset_today();
            return Ok(Value::TimeTz(timetz::at_zone(ttz(&args[0]), off)));
        }
        // Numeric-typed math: the argument(s) and result are `numeric`.
        ScalarFn::NumRound => {
            let s = args.get(1).map(i4).unwrap_or(0);
            return Ok(Value::Numeric(num(&args[0]).round(s)));
        }
        ScalarFn::NumTrunc => {
            let s = args.get(1).map(i4).unwrap_or(0);
            return Ok(Value::Numeric(num(&args[0]).trunc(s)));
        }
        ScalarFn::NumCeil => return Ok(Value::Numeric(num(&args[0]).ceil())),
        ScalarFn::NumFloor => return Ok(Value::Numeric(num(&args[0]).floor())),
        ScalarFn::NumAbs => return Ok(Value::Numeric(num(&args[0]).abs())),
        ScalarFn::NumSign => return Ok(Value::Numeric(num(&args[0]).signum())),
        ScalarFn::NumMod => {
            return num(&args[0])
                .modulo(num(&args[1]))
                .map(Value::Numeric)
                .map_err(num_err);
        }
        // `mod(intN, intN)`: remainder truncated toward zero (`MIN % -1 = 0`),
        // division by zero is 22012 — same semantics as the `%` operator.
        ScalarFn::ModInt => {
            let zero = || err(sqlstate::DIVISION_BY_ZERO, "division by zero");
            return match (&args[0], &args[1]) {
                (Value::Int2(a), Value::Int2(b)) => {
                    if *b == 0 {
                        Err(zero())
                    } else {
                        Ok(Value::Int2(a.checked_rem(*b).unwrap_or(0)))
                    }
                }
                (Value::Int4(a), Value::Int4(b)) => {
                    if *b == 0 {
                        Err(zero())
                    } else {
                        Ok(Value::Int4(a.checked_rem(*b).unwrap_or(0)))
                    }
                }
                (Value::Int8(a), Value::Int8(b)) => {
                    if *b == 0 {
                        Err(zero())
                    } else {
                        Ok(Value::Int8(a.checked_rem(*b).unwrap_or(0)))
                    }
                }
                (a, b) => unreachable!("mod(int) on {a:?}, {b:?}"),
            };
        }
        ScalarFn::NumSqrt => {
            return num(&args[0]).sqrt().map(Value::Numeric).map_err(num_err);
        }
        ScalarFn::NumLn => return num(&args[0]).ln().map(Value::Numeric).map_err(num_err),
        ScalarFn::NumLog10 => return num(&args[0]).log10().map(Value::Numeric).map_err(num_err),
        ScalarFn::NumLog => {
            return num(&args[0])
                .log_base(num(&args[1]))
                .map(Value::Numeric)
                .map_err(num_err);
        }
        ScalarFn::NumExp => return num(&args[0]).exp().map(Value::Numeric).map_err(num_err),
        ScalarFn::NumPower => {
            return num(&args[0])
                .power(num(&args[1]))
                .map(Value::Numeric)
                .map_err(num_err);
        }
        ScalarFn::NumApplyTypmod => {
            return num(&args[0])
                .apply_typmod(i4(&args[1]), i4(&args[2]))
                .map(Value::Numeric)
                .map_err(num_err);
        }
        ScalarFn::TimeApplyTypmod => {
            let precision = i4(&args[1]);
            return Ok(match &args[0] {
                Value::Time(usec) => Value::Time(time::apply_typmod(*usec, precision)),
                Value::TimeTz(t) => Value::TimeTz(TimeTz {
                    usec: time::apply_typmod(t.usec, precision),
                    zone: t.zone,
                }),
                Value::Timestamp(usec) => {
                    Value::Timestamp(timestamp::apply_typmod(*usec, precision))
                }
                Value::TimestampTz(usec) => {
                    Value::TimestampTz(timestamp::apply_typmod(*usec, precision))
                }
                other => unreachable!("expected a datetime arg, got {other:?}"),
            });
        }
        ScalarFn::IntervalTypmod => {
            return match &args[0] {
                Value::Interval(iv) => interval::apply_typmod(*iv, i4(&args[1]))
                    .map(Value::Interval)
                    .map_err(iv_err),
                other => unreachable!("expected an interval arg, got {other:?}"),
            };
        }
        // A literal the binder could not fold, because `sql_standard` reads a
        // leading minus as propagating to the later fields and only the session
        // knows the style.
        ScalarFn::IntervalIn => {
            let unit = interval::Unit::from_code(i4(&args[1]))
                .unwrap_or_else(|| unreachable!("the binder emits a real unit code"));
            return interval::parse_with_style(text(&args[0]), unit, fmt.interval_style)
                .map(Value::Interval)
                .map_err(iv_err);
        }
        // md5(text)/md5(bytea) hash the raw input bytes; both return the
        // 32-char lowercase hex digest as text.
        ScalarFn::Md5 => {
            let bytes = match &args[0] {
                Value::Text(s) => s.as_bytes(),
                Value::Bytea(b) => b.as_slice(),
                other => unreachable!("expected text/bytea arg, got {other:?}"),
            };
            return Ok(Value::Text(crate::md5::md5_hex(bytes)));
        }

        // --- interval operators ---
        ScalarFn::IntervalNeg => {
            return interval::negate(iv(&args[0]))
                .map(Value::Interval)
                .map_err(iv_err);
        }
        ScalarFn::IntervalPl => {
            return interval::add(iv(&args[0]), iv(&args[1]))
                .map(Value::Interval)
                .map_err(iv_err);
        }
        ScalarFn::IntervalMi => {
            return interval::sub(iv(&args[0]), iv(&args[1]))
                .map(Value::Interval)
                .map_err(iv_err);
        }
        ScalarFn::IntervalMul => {
            return interval::mul(iv(&args[0]), f8(&args[1]))
                .map(Value::Interval)
                .map_err(iv_err);
        }
        ScalarFn::IntervalDiv => {
            return interval::div(iv(&args[0]), f8(&args[1]))
                .map(Value::Interval)
                .map_err(iv_err);
        }
        ScalarFn::TimestampPlInterval => {
            return timestamp::pl_interval(ts(&args[0]), iv(&args[1]))
                .map(Value::Timestamp)
                .map_err(ts_err);
        }
        ScalarFn::TimestampMiInterval => {
            return timestamp::mi_interval(ts(&args[0]), iv(&args[1]))
                .map(Value::Timestamp)
                .map_err(ts_err);
        }
        ScalarFn::TimestampMi => {
            return timestamp::mi(ts(&args[0]), ts(&args[1]))
                .map(Value::Interval)
                .map_err(ts_err);
        }
        ScalarFn::TimestampTzPlInterval => {
            return timestamptz::pl_interval(tstz(&args[0]), iv(&args[1]), fmt.zone.zone())
                .map(Value::TimestampTz)
                .map_err(ts_err);
        }
        ScalarFn::TimestampTzMiInterval => {
            return timestamptz::mi_interval(tstz(&args[0]), iv(&args[1]), fmt.zone.zone())
                .map(Value::TimestampTz)
                .map_err(ts_err);
        }
        // Not a missing zone argument: the difference of two instants is the
        // same span from every zone, which is why PG's `timestamptz_mi` is
        // literally the `timestamp_mi` C function under a second name.
        ScalarFn::TimestampTzMi => {
            return timestamp::mi(tstz(&args[0]), tstz(&args[1]))
                .map(Value::Interval)
                .map_err(ts_err);
        }

        // --- inet/cidr operators ---
        ScalarFn::NetworkContainedBy => {
            return Ok(Value::Bool(net::contained_by(
                inet(&args[0]),
                inet(&args[1]),
            )));
        }
        ScalarFn::NetworkContains => {
            return Ok(Value::Bool(net::contains(inet(&args[0]), inet(&args[1]))));
        }
        ScalarFn::NetworkOverlaps => {
            return Ok(Value::Bool(net::overlaps(inet(&args[0]), inet(&args[1]))));
        }
        ScalarFn::InetAnd => {
            return net::bit_and(inet(&args[0]), inet(&args[1]))
                .map(Value::Inet)
                .map_err(net_err);
        }
        ScalarFn::InetOr => {
            return net::bit_or(inet(&args[0]), inet(&args[1]))
                .map(Value::Inet)
                .map_err(net_err);
        }
        ScalarFn::InetNot => return Ok(Value::Inet(net::bit_not(inet(&args[0])))),
        ScalarFn::InetPlInt8 => {
            return net::add_offset(inet(&args[0]), i8(&args[1]))
                .map(Value::Inet)
                .map_err(net_err);
        }
        ScalarFn::InetMiInt8 => {
            return net::sub_offset(inet(&args[0]), i8(&args[1]))
                .map(Value::Inet)
                .map_err(net_err);
        }
        ScalarFn::InetMi => {
            return net::diff(inet(&args[0]), inet(&args[1]))
                .map(Value::Int8)
                .map_err(net_err);
        }

        // --- inet/cidr functions ---
        ScalarFn::Host => return Ok(Value::Text(net::host(inet(&args[0])))),
        ScalarFn::Masklen => return Ok(Value::Int4(net::masklen(inet(&args[0])))),
        ScalarFn::Family => return Ok(Value::Int4(net::family(inet(&args[0])))),
        ScalarFn::Network => return Ok(Value::Cidr(net::network(inet(&args[0])))),
        ScalarFn::AbbrevInet => return Ok(Value::Text(net::abbrev_inet(inet(&args[0])))),
        ScalarFn::AbbrevCidr => return Ok(Value::Text(net::abbrev_cidr(inet(&args[0])))),

        // --- macaddr / macaddr8 operators + functions (width-dispatched) ---
        ScalarFn::MacaddrNot => {
            return Ok(match &args[0] {
                Value::Macaddr(b) => Value::Macaddr(macaddr::not6(b)),
                Value::Macaddr8(b) => Value::Macaddr8(macaddr::not8(b)),
                other => unreachable!("expected macaddr/macaddr8, got {other:?}"),
            });
        }
        ScalarFn::MacaddrAnd => {
            return Ok(match (&args[0], &args[1]) {
                (Value::Macaddr(a), Value::Macaddr(b)) => Value::Macaddr(macaddr::and6(a, b)),
                (Value::Macaddr8(a), Value::Macaddr8(b)) => Value::Macaddr8(macaddr::and8(a, b)),
                other => unreachable!("expected matching macaddr args, got {other:?}"),
            });
        }
        ScalarFn::MacaddrOr => {
            return Ok(match (&args[0], &args[1]) {
                (Value::Macaddr(a), Value::Macaddr(b)) => Value::Macaddr(macaddr::or6(a, b)),
                (Value::Macaddr8(a), Value::Macaddr8(b)) => Value::Macaddr8(macaddr::or8(a, b)),
                other => unreachable!("expected matching macaddr args, got {other:?}"),
            });
        }
        ScalarFn::MacaddrTrunc => {
            return Ok(match &args[0] {
                Value::Macaddr(b) => Value::Macaddr(macaddr::trunc6(b)),
                Value::Macaddr8(b) => Value::Macaddr8(macaddr::trunc8(b)),
                other => unreachable!("expected macaddr/macaddr8, got {other:?}"),
            });
        }
        ScalarFn::Macaddr8Set7bit => {
            return Ok(match &args[0] {
                Value::Macaddr8(b) => Value::Macaddr8(macaddr::set7bit(b)),
                other => unreachable!("expected macaddr8, got {other:?}"),
            });
        }

        // --- interval functions ---
        ScalarFn::DatePartInterval => {
            return Ok(
                match interval::date_part(text(&args[0]), iv(&args[1])).map_err(iv_err)? {
                    Some(v) => Value::Float8(v),
                    None => Value::Null,
                },
            );
        }
        ScalarFn::ExtractInterval => {
            return Ok(
                match interval::extract(text(&args[0]), iv(&args[1])).map_err(iv_err)? {
                    Some(n) => Value::Numeric(n),
                    None => Value::Null,
                },
            );
        }
        ScalarFn::DateTruncInterval => {
            return interval::date_trunc(text(&args[0]), iv(&args[1]))
                .map(Value::Interval)
                .map_err(iv_err);
        }
        ScalarFn::IsfiniteInterval => {
            return Ok(Value::Bool(iv(&args[0]).is_finite()));
        }
        ScalarFn::MakeInterval => {
            return interval::make_interval(
                i4(&args[0]) as i64,
                i4(&args[1]) as i64,
                i4(&args[2]) as i64,
                i4(&args[3]) as i64,
                i4(&args[4]) as i64,
                i4(&args[5]) as i64,
                f8(&args[6]),
            )
            .map(Value::Interval)
            .map_err(iv_err);
        }
        ScalarFn::JustifyDays => {
            return interval::justify_days(iv(&args[0]))
                .map(Value::Interval)
                .map_err(iv_err);
        }
        ScalarFn::JustifyHours => {
            return interval::justify_hours(iv(&args[0]))
                .map(Value::Interval)
                .map_err(iv_err);
        }
        ScalarFn::JustifyInterval => {
            return interval::justify_interval(iv(&args[0]))
                .map(Value::Interval)
                .map_err(iv_err);
        }
        ScalarFn::Age => {
            return timestamp::age(ts(&args[0]), ts(&args[1]))
                .map(Value::Interval)
                .map_err(ts_err);
        }
        ScalarFn::AgeTz => {
            return timestamptz::age(tstz(&args[0]), tstz(&args[1]), fmt.zone.zone())
                .map(Value::Interval)
                .map_err(ts_err);
        }
        // The one-argument forms anchor at `current_date`, so they need the
        // transaction clock as well as the zone — PG spells them as SQL
        // wrappers around `age(cast(current_date as …), $1)`.
        ScalarFn::AgeToday => {
            let anchor = today_midnight(fmt)?;
            return timestamp::age(anchor, ts(&args[0]))
                .map(Value::Interval)
                .map_err(ts_err);
        }
        ScalarFn::AgeTodayTz => {
            // Deliberately composed as the same two casts PG's wrapper uses:
            // local midnight, read back as an instant, then compared as wall
            // clocks. In a zone whose transition falls *at* midnight the round
            // trip is not the identity — and PG shows that same quirk.
            let anchor = timestamptz::timestamp_at_session_zone(today_midnight(fmt)?, &fmt.zone)
                .map_err(ts_err)?;
            return timestamptz::age(anchor, tstz(&args[0]), fmt.zone.zone())
                .map(Value::Interval)
                .map_err(ts_err);
        }
        // A non-finite interval or timestamp yields NULL, matching PG.
        ScalarFn::ToCharInterval => {
            return formatting::to_char_interval(iv(&args[0]), text(&args[1]))
                .map(null_or_text)
                .map_err(fmt_err);
        }
        ScalarFn::ToCharTime => {
            // PG routes `to_char(time, …)` through its implicit
            // `time -> interval` cast, so the interval codes apply.
            let as_interval = Interval {
                months: 0,
                days: 0,
                usec: tm(&args[0]),
            };
            return formatting::to_char_interval(as_interval, text(&args[1]))
                .map(null_or_text)
                .map_err(fmt_err);
        }
        ScalarFn::ToCharTimestamp => {
            return formatting::to_char_timestamp(ts(&args[0]), text(&args[1]))
                .map(null_or_text)
                .map_err(fmt_err);
        }
        ScalarFn::ToCharTimestampTz => {
            return formatting::to_char_timestamptz(tstz(&args[0]), text(&args[1]), &fmt.zone)
                .map(null_or_text)
                .map_err(fmt_err);
        }
        ScalarFn::ToDate => {
            return formatting::from_char_date(text(&args[0]), text(&args[1]))
                .map(Value::Date)
                .map_err(fmt_err);
        }
        ScalarFn::ToTimestampFormat => {
            return formatting::from_char_timestamptz(text(&args[0]), text(&args[1]), &fmt.zone)
                .map(Value::TimestampTz)
                .map_err(fmt_err);
        }
        ScalarFn::ToCharNumeric => {
            return formatting_num::numeric(num(&args[0]), text(&args[1]))
                .map(Value::Text)
                .map_err(fmt_err);
        }
        ScalarFn::ToCharInt4 => {
            return formatting_num::int8(i64::from(i4(&args[0])), text(&args[1]))
                .map(Value::Text)
                .map_err(fmt_err);
        }
        ScalarFn::ToCharInt8 => {
            return formatting_num::int8(i8(&args[0]), text(&args[1]))
                .map(Value::Text)
                .map_err(fmt_err);
        }
        ScalarFn::ToCharFloat8 => {
            return formatting_num::float8(f8(&args[0]), text(&args[1]))
                .map(Value::Text)
                .map_err(fmt_err);
        }
        ScalarFn::ToCharFloat4 => {
            return formatting_num::float4(f4(&args[0]), text(&args[1]))
                .map(Value::Text)
                .map_err(fmt_err);
        }
        ScalarFn::ToNumber => {
            // An empty picture yields NULL, matching PG.
            return formatting_num::to_number(text(&args[0]), text(&args[1]))
                .map(|n| n.map_or(Value::Null, Value::Numeric))
                .map_err(fmt_err);
        }
        ScalarFn::ToTimestampUnix => {
            return timestamptz::from_unix_epoch(f8(&args[0]))
                .map(Value::TimestampTz)
                .map_err(ts_err);
        }

        // --- date operators/functions ---
        ScalarFn::DatePlDays => {
            return date::add_days(dt(&args[0]), i4(&args[1]))
                .map(Value::Date)
                .map_err(date_err);
        }
        ScalarFn::DateMiDays => {
            return date::sub_days(dt(&args[0]), i4(&args[1]))
                .map(Value::Date)
                .map_err(date_err);
        }
        ScalarFn::DateMi => {
            return date::sub_date(dt(&args[0]), dt(&args[1]))
                .map(Value::Int4)
                .map_err(date_err);
        }
        ScalarFn::DatePlInterval => {
            return date::pl_interval(dt(&args[0]), iv(&args[1]))
                .map(Value::Timestamp)
                .map_err(date_err);
        }
        ScalarFn::DateMiInterval => {
            return date::mi_interval(dt(&args[0]), iv(&args[1]))
                .map(Value::Timestamp)
                .map_err(date_err);
        }
        ScalarFn::DatePlTime => {
            return date::pl_time(dt(&args[0]), tm(&args[1]))
                .map(Value::Timestamp)
                .map_err(date_err);
        }
        ScalarFn::DatePlTimeTz => {
            return date::pl_timetz(dt(&args[0]), ttz(&args[1]))
                .map(Value::TimestampTz)
                .map_err(date_err);
        }
        ScalarFn::DatePartDate => {
            return Ok(
                match date::date_part(text(&args[0]), dt(&args[1])).map_err(date_err)? {
                    Some(v) => Value::Float8(v),
                    None => Value::Null,
                },
            );
        }
        ScalarFn::ExtractDate => {
            return Ok(
                match date::extract(text(&args[0]), dt(&args[1])).map_err(date_err)? {
                    Some(n) => Value::Numeric(n),
                    None => Value::Null,
                },
            );
        }
        ScalarFn::IsfiniteDate => {
            return Ok(Value::Bool(date::is_finite(dt(&args[0]))));
        }
        ScalarFn::MakeDate => {
            return date::make_date(
                i4(&args[0]) as i64,
                i4(&args[1]) as i64,
                i4(&args[2]) as i64,
            )
            .map(Value::Date)
            .map_err(date_err);
        }

        // --- time operators/functions ---
        ScalarFn::TimePlInterval => {
            return Ok(Value::Time(time::pl_interval(tm(&args[0]), iv(&args[1]))));
        }
        ScalarFn::TimeMiInterval => {
            return Ok(Value::Time(time::mi_interval(tm(&args[0]), iv(&args[1]))));
        }
        ScalarFn::TimeMi => {
            return Ok(Value::Interval(time::mi(tm(&args[0]), tm(&args[1]))));
        }
        ScalarFn::DatePartTime => {
            return time::date_part(text(&args[0]), tm(&args[1]))
                .map(Value::Float8)
                .map_err(time_err);
        }
        ScalarFn::ExtractTime => {
            return time::extract(text(&args[0]), tm(&args[1]))
                .map(Value::Numeric)
                .map_err(time_err);
        }
        ScalarFn::MakeTime => {
            return time::make_time(i4(&args[0]) as i64, i4(&args[1]) as i64, f8(&args[2]))
                .map(Value::Time)
                .map_err(time_err);
        }

        // --- timetz operators/functions ---
        ScalarFn::TimeTzPlInterval => {
            return Ok(Value::TimeTz(timetz::pl_interval(
                ttz(&args[0]),
                iv(&args[1]),
            )));
        }
        ScalarFn::TimeTzMiInterval => {
            return Ok(Value::TimeTz(timetz::mi_interval(
                ttz(&args[0]),
                iv(&args[1]),
            )));
        }
        ScalarFn::DatePartTimeTz => {
            return timetz::date_part(text(&args[0]), ttz(&args[1]))
                .map(Value::Float8)
                .map_err(timetz_err);
        }
        ScalarFn::ExtractTimeTz => {
            return timetz::extract(text(&args[0]), ttz(&args[1]))
                .map(Value::Numeric)
                .map_err(timetz_err);
        }

        // --- money operators/functions ---
        ScalarFn::CashUm => {
            return money::neg(money_of(&args[0]))
                .map(Value::Money)
                .map_err(cash_err);
        }
        ScalarFn::CashPl => {
            return money::add(money_of(&args[0]), money_of(&args[1]))
                .map(Value::Money)
                .map_err(cash_err);
        }
        ScalarFn::CashMi => {
            return money::sub(money_of(&args[0]), money_of(&args[1]))
                .map(Value::Money)
                .map_err(cash_err);
        }
        ScalarFn::CashMulInt => {
            return money::mul_int(money_of(&args[0]), i8(&args[1]))
                .map(Value::Money)
                .map_err(cash_err);
        }
        ScalarFn::CashMulFlt => {
            return money::mul_float(money_of(&args[0]), f8(&args[1]))
                .map(Value::Money)
                .map_err(cash_err);
        }
        ScalarFn::CashDivInt => {
            return money::div_int(money_of(&args[0]), i8(&args[1]))
                .map(Value::Money)
                .map_err(cash_err);
        }
        ScalarFn::CashDivFlt => {
            return money::div_float(money_of(&args[0]), f8(&args[1]))
                .map(Value::Money)
                .map_err(cash_err);
        }
        ScalarFn::CashDivCash => {
            return money::div_cash(money_of(&args[0]), money_of(&args[1]))
                .map(Value::Float8)
                .map_err(cash_err);
        }
        ScalarFn::CashWords => {
            return Ok(Value::Text(money::words(money_of(&args[0]))));
        }
        ScalarFn::CashLarger => {
            return Ok(Value::Money(money::larger(
                money_of(&args[0]),
                money_of(&args[1]),
            )));
        }
        ScalarFn::CashSmaller => {
            return Ok(Value::Money(money::smaller(
                money_of(&args[0]),
                money_of(&args[1]),
            )));
        }
        _ => {}
    }
    // The remaining functions are float8 → float8 (or float8×float8 → float8).
    let a = f8(&args[0]);
    let result = match func {
        ScalarFn::Trunc => Ok(a.trunc()),
        ScalarFn::Round => Ok(a.round_ties_even()),
        ScalarFn::Ceil => Ok(a.ceil()),
        ScalarFn::Floor => Ok(a.floor()),
        // Else branch returns the argument so sign(NaN)=NaN and sign(-0)=-0, as PG.
        ScalarFn::Sign => Ok(if a > 0.0 {
            1.0
        } else if a < 0.0 {
            -1.0
        } else {
            a
        }),
        ScalarFn::Sqrt => float::f8_sqrt(a).map_err(float_err),
        ScalarFn::Cbrt => Ok(float::f8_cbrt(a)),
        ScalarFn::Exp => dexp(a),
        ScalarFn::Ln => dln(a),
        ScalarFn::Log10F8 => dlog10(a),
        ScalarFn::AbsF8 => Ok(a.abs()),
        ScalarFn::Power => float::f8_pow(a, f8(&args[1])).map_err(float_err),
        ScalarFn::Sinh => Ok(a.sinh()),
        ScalarFn::Cosh => Ok(a.cosh()),
        ScalarFn::Tanh => Ok(a.tanh()),
        ScalarFn::Asinh => Ok(a.asinh()),
        ScalarFn::Acosh => {
            if a < 1.0 {
                Err(out_of_range_input())
            } else {
                Ok(a.acosh())
            }
        }
        ScalarFn::Atanh => {
            if a.is_nan() {
                Ok(f64::NAN)
            } else if !(-1.0..=1.0).contains(&a) {
                Err(out_of_range_input())
            } else {
                Ok(a.atanh())
            }
        }
        ScalarFn::Erf => Ok(crate::special_fns::erf(a)),
        ScalarFn::Erfc => Ok(crate::special_fns::erfc(a)),
        ScalarFn::Gamma => dgamma(a),
        ScalarFn::Lgamma => dlgamma(a),
        ScalarFn::Sind => dsind(a),
        ScalarFn::Cosd => dcosd(a),
        ScalarFn::Tand => dtand(a),
        ScalarFn::Cotd => dcotd(a),
        ScalarFn::Asind => dasind(a),
        ScalarFn::Acosd => dacosd(a),
        ScalarFn::Atand => Ok(datand(a)),
        ScalarFn::Atan2d => Ok(datan2d(a, f8(&args[1]))),
        // Every non-float8 function returns early in the first `match` above.
        other => unreachable!("non-float8 function reached the float8 tail: {other:?}"),
    };
    result.map(Value::Float8)
}

fn text_err(e: text::TextError) -> ExecError {
    ExecError::new(e.sqlstate, e.message)
}

/// The `ESCAPE` argument shared by `SIMILAR TO` and the SQL-regex `substring`
/// form. An absent clause defaults to `\`; `ESCAPE ''` disables escaping
/// altogether; anything longer than one character is an error.
fn escape_char(arg: Option<&Value>) -> Result<Option<char>, ExecError> {
    match arg {
        None => Ok(Some('\\')),
        Some(v) => {
            let s = text(v);
            if s.chars().count() > 1 {
                return Err(
                    err(sqlstate::INVALID_ESCAPE_SEQUENCE, "invalid escape string").with_hint(
                        Some("Escape string must be empty or one character.".to_string()),
                    ),
                );
            }
            Ok(s.chars().next())
        }
    }
}

fn bit_err(e: bit::BitError) -> ExecError {
    ExecError::new(e.sqlstate, e.message)
}

fn bits(v: &Value) -> (u32, &[u8]) {
    match v {
        Value::Bit { len, data } => (*len, data),
        other => unreachable!("expected bit arg, got {other:?}"),
    }
}

fn float_err(e: float::FloatError) -> ExecError {
    ExecError::new(e.sqlstate, e.message)
}

fn dexp(x: f64) -> Result<f64, ExecError> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    let r = x.exp();
    if r.is_infinite() {
        if x.is_finite() {
            return Err(overflow());
        }
    } else if r == 0.0 && x.is_finite() {
        return Err(underflow());
    }
    Ok(r)
}

fn dln(x: f64) -> Result<f64, ExecError> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x == 0.0 {
        return Err(err(
            sqlstate::INVALID_ARGUMENT_FOR_LOG,
            "cannot take logarithm of zero",
        ));
    }
    if x < 0.0 {
        return Err(err(
            sqlstate::INVALID_ARGUMENT_FOR_LOG,
            "cannot take logarithm of a negative number",
        ));
    }
    Ok(x.ln())
}

/// `dlog10`: base-10 logarithm with PG's domain errors.
fn dlog10(x: f64) -> Result<f64, ExecError> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x == 0.0 {
        return Err(err(
            sqlstate::INVALID_ARGUMENT_FOR_LOG,
            "cannot take logarithm of zero",
        ));
    }
    if x < 0.0 {
        return Err(err(
            sqlstate::INVALID_ARGUMENT_FOR_LOG,
            "cannot take logarithm of a negative number",
        ));
    }
    Ok(x.log10())
}

fn dgamma(x: f64) -> Result<f64, ExecError> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return if x > 0.0 {
            Ok(f64::INFINITY)
        } else {
            Err(overflow())
        };
    }
    let r = crate::special_fns::tgamma(x);
    if r.is_infinite() || r.is_nan() {
        return Err(overflow());
    }
    if r == 0.0 {
        return Err(underflow());
    }
    Ok(r)
}

fn dlgamma(x: f64) -> Result<f64, ExecError> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Ok(f64::INFINITY);
    }
    let r = crate::special_fns::lgamma(x);
    if r.is_infinite() {
        return Err(overflow());
    }
    Ok(r)
}

// --- degree-based trig -----------------------------------------------------

fn sin_rt(x: f64) -> f64 {
    black_box((x * RADIANS_PER_DEGREE).sin())
}

fn cos_rt(x: f64) -> f64 {
    black_box((x * RADIANS_PER_DEGREE).cos())
}

/// sin over the first quadrant [0, 90], exact at 0, 30, 90.
fn sind_q1(x: f64) -> f64 {
    if x <= 30.0 {
        sin_rt(x) / (2.0 * sin_rt(30.0))
    } else {
        cosd_q1(90.0 - x)
    }
}

/// cos over the first quadrant [0, 90], exact at 0, 60, 90.
fn cosd_q1(x: f64) -> f64 {
    if x <= 60.0 {
        1.0 - (1.0 - cos_rt(x)) / (2.0 * (1.0 - cos_rt(60.0)))
    } else {
        sind_q1(90.0 - x)
    }
}

/// sind over all reals via a period-360 reduction to the first quadrant.
fn sind_reduced(x: f64) -> f64 {
    let mut sign = 1.0;
    let mut a = x % 360.0;
    if a < 0.0 {
        a = -a;
        sign = -sign;
    }
    if a > 180.0 {
        a = 360.0 - a;
        sign = -sign;
    }
    if a > 90.0 {
        a = 180.0 - a;
    }
    sign * sind_q1(a)
}

/// cosd over all reals via a period-360 reduction to the first quadrant.
fn cosd_reduced(x: f64) -> f64 {
    let mut sign = 1.0;
    let mut a = x % 360.0;
    if a < 0.0 {
        a = -a;
    }
    if a > 180.0 {
        a = 360.0 - a;
    }
    if a > 90.0 {
        a = 180.0 - a;
        sign = -sign;
    }
    sign * cosd_q1(a)
}

fn dsind(x: f64) -> Result<f64, ExecError> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Err(out_of_range_input());
    }
    Ok(sind_reduced(x))
}

fn dcosd(x: f64) -> Result<f64, ExecError> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Err(out_of_range_input());
    }
    Ok(cosd_reduced(x))
}

// tand/cotd are computed as sind/cosd (and its reciprocal), each via the same
// period-360 reduction, so the denominator's signed zero carries the correct
// sign at the poles: e.g. cosd(270) = +0 gives tand(270) = -1/+0 = -Infinity,
// where a period-180 tan reduction would lose that sign. Dividing by tan(45)/
// cot(45) makes the ±1 endpoints exact.
fn dtand(x: f64) -> Result<f64, ExecError> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Err(out_of_range_input());
    }
    let tan45 = sind_q1(45.0) / cosd_q1(45.0);
    let mut result = (sind_reduced(x) / cosd_reduced(x)) / tan45;
    if result == 0.0 {
        result = 0.0; // force +0
    }
    Ok(result)
}

fn dcotd(x: f64) -> Result<f64, ExecError> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Err(out_of_range_input());
    }
    let cot45 = cosd_q1(45.0) / sind_q1(45.0);
    let mut result = (cosd_reduced(x) / sind_reduced(x)) / cot45;
    if result == 0.0 {
        result = 0.0; // force +0
    }
    Ok(result)
}

/// asin over [0, 1] in degrees, exact at 0, 0.5, 1.
fn asind_q1(x: f64) -> f64 {
    if x <= 0.5 {
        (black_box(x.asin()) / black_box(0.5f64.asin())) * 30.0
    } else {
        90.0 - (black_box(x.acos()) / black_box(0.5f64.acos())) * 60.0
    }
}

/// acos over [0, 1] in degrees, exact at 0, 0.5, 1.
fn acosd_q1(x: f64) -> f64 {
    if x <= 0.5 {
        90.0 - (black_box(x.asin()) / black_box(0.5f64.asin())) * 30.0
    } else {
        (black_box(x.acos()) / black_box(0.5f64.acos())) * 60.0
    }
}

fn dasind(x: f64) -> Result<f64, ExecError> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if !(-1.0..=1.0).contains(&x) {
        return Err(out_of_range_input());
    }
    Ok(if x >= 0.0 { asind_q1(x) } else { -asind_q1(-x) })
}

fn dacosd(x: f64) -> Result<f64, ExecError> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if !(-1.0..=1.0).contains(&x) {
        return Err(out_of_range_input());
    }
    Ok(if x >= 0.0 {
        acosd_q1(x)
    } else {
        180.0 - acosd_q1(-x)
    })
}

fn datand(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    let atan_1_0 = black_box(1.0f64.atan());
    (black_box(x.atan()) / atan_1_0) * 45.0
}

fn datan2d(y: f64, x: f64) -> f64 {
    if y.is_nan() || x.is_nan() {
        return f64::NAN;
    }
    let atan_1_0 = black_box(1.0f64.atan());
    (black_box(y.atan2(x)) / atan_1_0) * 45.0
}

fn f4(v: &Value) -> f32 {
    match v {
        Value::Float4(v) => *v,
        other => unreachable!("expected float4 arg, got {other:?}"),
    }
}

fn f8(v: &Value) -> f64 {
    match v {
        Value::Float8(v) => *v,
        other => unreachable!("expected float8 arg, got {other:?}"),
    }
}

fn point_of(v: &Value) -> [f64; 2] {
    match v {
        Value::Point(p) => *p,
        other => unreachable!("expected point arg, got {other:?}"),
    }
}

fn lseg_of(v: &Value) -> [f64; 4] {
    match v {
        Value::Lseg(l) => *l,
        other => unreachable!("expected lseg arg, got {other:?}"),
    }
}

/// Evaluate a `jsonb_path_*` function (or the `@?`/`@@` operators). Args are
/// `[target jsonb, path jsonpath]` optionally followed by `[vars jsonb, silent
/// bool]`. The SQL functions are STRICT — any NULL argument already
/// short-circuited to NULL upstream, so on entry every argument is non-NULL. The
/// `ExistsOp`/`MatchOp` operator variants are always silent and take no vars.
/// Structural errors surface with their SQLSTATE unless `silent` suppresses them
/// (a missing-variable error always raises).
fn eval_jsonpath(f: JsonPathFn, args: &[Value]) -> Result<Value, ExecError> {
    // The operators run silently with no variables; the functions read the
    // optional (guaranteed non-NULL) `vars`/`silent` arguments.
    let (silent, vars) = match f {
        JsonPathFn::ExistsOp | JsonPathFn::MatchOp => (true, None),
        _ => {
            let vars = match args.get(2) {
                Some(Value::Jsonb(v)) => Some(v),
                _ => None,
            };
            (matches!(args.get(3), Some(Value::Bool(true))), vars)
        }
    };
    let target = match &args[0] {
        Value::Jsonb(j) => j,
        other => unreachable!("expected jsonb target, got {other:?}"),
    };
    let path = match &args[1] {
        Value::Jsonpath(p) => p,
        other => unreachable!("expected jsonpath arg, got {other:?}"),
    };
    let opt_bool = |o: Option<bool>| o.map(Value::Bool).unwrap_or(Value::Null);
    match f {
        JsonPathFn::Exists | JsonPathFn::ExistsOp => jsonpath::exists(path, target, vars, silent)
            .map(opt_bool)
            .map_err(json_err),
        JsonPathFn::Match | JsonPathFn::MatchOp => {
            jsonpath::match_predicate(path, target, vars, silent)
                .map(opt_bool)
                .map_err(json_err)
        }
        JsonPathFn::QueryArray => jsonpath::query(path, target, vars, silent)
            .map(|items| Value::Jsonb(Jsonb::Array(items)))
            .map_err(json_err),
        JsonPathFn::QueryFirst => jsonpath::query(path, target, vars, silent)
            .map(|items| {
                items
                    .into_iter()
                    .next()
                    .map(Value::Jsonb)
                    .unwrap_or(Value::Null)
            })
            .map_err(json_err),
    }
}

/// The `tsvector`/`tsquery` family. Every variant is STRICT, so `args` holds no
/// NULLs — except inside the `text[]` arguments, where a NULL element is
/// meaningful (`setweight`/`ts_delete` skip them, `array_to_tsvector` rejects
/// them).
fn eval_ts<'a>(f: TsFn, args: &'a [Value]) -> Result<Value, ExecError> {
    use crabgresql_types::{tsquery, tsvector};

    // Borrow rather than clone: this runs once per row per node, and every
    // callee below takes `&`. Matches `eval_jsonpath` above.
    let vector = |v: &'a Value| match v {
        Value::Tsvector(t) => t,
        other => unreachable!("expected tsvector, got {other:?}"),
    };
    let query = |v: &'a Value| match v {
        Value::Tsquery(q) => q,
        other => unreachable!("expected tsquery, got {other:?}"),
    };
    // A `text` or `text[]` argument as a list of optional lexemes, so the two
    // `ts_delete` overloads share one code path.
    let words = |v: &Value| -> Vec<Option<String>> {
        match v {
            Value::Text(s) => vec![Some(s.clone())],
            Value::Array { elems, .. } => elems
                .iter()
                .map(|e| match e {
                    Value::Text(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            other => unreachable!("expected text or text[], got {other:?}"),
        }
    };
    let ts_err = |e: tsvector::TsError| ExecError::new(e.sqlstate, e.message);

    Ok(match f {
        TsFn::Match => Value::Bool(tsquery::matches(vector(&args[0]), query(&args[1]))),
        TsFn::VectorConcat => Value::Tsvector(tsvector::concat(vector(&args[0]), vector(&args[1]))),
        TsFn::Strip => Value::Tsvector(tsvector::strip(vector(&args[0]))),
        TsFn::VectorLength => Value::Int4(tsvector::length(vector(&args[0]))),
        TsFn::SetWeight | TsFn::SetWeightLexemes => {
            // PG takes a `"char"`, which keeps only the first character.
            let label = match &args[1] {
                Value::Text(s) => s.chars().next().unwrap_or('\0'),
                other => unreachable!("expected text weight, got {other:?}"),
            };
            let w = tsvector::weight_from_char(label).map_err(ts_err)?;
            let tv = vector(&args[0]);
            Value::Tsvector(match f {
                TsFn::SetWeight => tsvector::setweight(tv, w),
                _ => tsvector::setweight_lexemes(tv, w, &words(&args[2])),
            })
        }
        TsFn::Delete => Value::Tsvector(tsvector::ts_delete(vector(&args[0]), &words(&args[1]))),
        TsFn::Filter => {
            // Unlike setweight/ts_delete, which skip NULL entries, ts_filter
            // rejects them outright.
            let mut weights = Vec::new();
            for label in words(&args[1]) {
                let label = label.ok_or_else(|| {
                    ExecError::new(
                        sqlstate::NULL_VALUE_NOT_ALLOWED,
                        "weight array may not contain nulls",
                    )
                })?;
                weights.push(tsvector::weight_from_label(&label).map_err(ts_err)?);
            }
            Value::Tsvector(tsvector::ts_filter(vector(&args[0]), &weights))
        }
        TsFn::VectorToArray => Value::Array {
            elem: PgType::Text,
            elems: tsvector::to_array(vector(&args[0]))
                .into_iter()
                .map(Value::Text)
                .collect(),
        },
        TsFn::ArrayToVector => {
            Value::Tsvector(tsvector::from_array(&words(&args[0])).map_err(ts_err)?)
        }
        TsFn::NumNode => Value::Int4(tsquery::numnode(query(&args[0]))),
        TsFn::QueryTree => Value::Text(tsquery::querytree(query(&args[0]))),
        TsFn::QueryAnd => {
            Value::Tsquery(tsquery::and(query(&args[0]), query(&args[1])).map_err(ts_err)?)
        }
        TsFn::QueryOr => {
            Value::Tsquery(tsquery::or(query(&args[0]), query(&args[1])).map_err(ts_err)?)
        }
        TsFn::QueryNot => Value::Tsquery(tsquery::not(query(&args[0])).map_err(ts_err)?),
        TsFn::QueryPhrase => {
            Value::Tsquery(tsquery::phrase(query(&args[0]), query(&args[1]), 1).map_err(ts_err)?)
        }
        TsFn::QueryPhraseDist => {
            let dist = match &args[2] {
                Value::Int4(n) => *n,
                other => unreachable!("expected int4 distance, got {other:?}"),
            };
            // Same range the `<N>` operator accepts. Without this an out-of-range
            // distance would build a tsquery whose text form no longer re-parses.
            let dist = u16::try_from(dist)
                .ok()
                .filter(|d| u32::from(*d) <= tsquery::MAX_DISTANCE)
                .ok_or_else(|| {
                    ExecError::new(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "distance in phrase operator must be an integer value between zero and 16384 inclusive",
                    )
                })?;
            Value::Tsquery(tsquery::phrase(query(&args[0]), query(&args[1]), dist).map_err(ts_err)?)
        }
    })
}

fn json_err(e: crabgresql_types::json::JsonError) -> ExecError {
    // JSON errors carry PG's DETAIL (the `\u0000` cast message, the parse
    // diagnostics); keep it rather than reporting the bare primary message.
    ExecError::new(e.sqlstate, e.message).with_detail(e.detail)
}

/// The `->` / `->>` / `#>` / `#>>` extraction operators, over both `json` and
/// `jsonb`. The blanket STRICT check upstream already turned a NULL target or
/// key into NULL, so `args` holds no top-level NULLs — but a NULL *element*
/// inside a `#>` path array is still possible and also yields NULL.
///
/// Every miss (absent key, wrong container kind, out-of-range subscript) is
/// NULL rather than an error; only a `\u0000` on a `->>` path can fail.
fn eval_json(f: JsonFn, args: &[Value]) -> Result<Value, ExecError> {
    use JsonFn::{
        ArrayElement, ArrayElementText, ExtractPath, ExtractPathText, ObjectField, ObjectFieldText,
    };
    // `#>`/`#>>` take a `text[]`; a NULL anywhere in it makes the whole result
    // NULL, as PG's non-STRICT-on-elements behavior does.
    let path: Vec<&str> = match f {
        ExtractPath | ExtractPathText => {
            let elems = array_elems(&args[1]);
            if elems.iter().any(|e| matches!(e, Value::Null)) {
                return Ok(Value::Null);
            }
            elems.iter().map(text).collect()
        }
        _ => Vec::new(),
    };
    match &args[0] {
        Value::Jsonb(doc) => {
            let found = match f {
                ObjectField | ObjectFieldText => json::jsonb_object_field(doc, text(&args[1])),
                ArrayElement | ArrayElementText => {
                    json::jsonb_array_element(doc, i4(&args[1]) as i64)
                }
                ExtractPath | ExtractPathText => json::jsonb_extract_path(doc, &path),
            };
            Ok(match found {
                None => Value::Null,
                Some(v) if json_returns_text(f) => {
                    json::jsonb_as_text(v).map_or(Value::Null, Value::Text)
                }
                Some(v) => Value::Jsonb(v.clone()),
            })
        }
        Value::Json(doc) => {
            let found = match f {
                ObjectField | ObjectFieldText => json::json_object_field(doc, text(&args[1])),
                ArrayElement | ArrayElementText => {
                    json::json_array_element(doc, i4(&args[1]) as i64)
                }
                ExtractPath | ExtractPathText => json::json_extract_path(doc, &path),
            };
            Ok(match found {
                None => Value::Null,
                Some(raw) if json_returns_text(f) => json::json_as_text(raw)
                    .map_err(json_err)?
                    .map_or(Value::Null, Value::Text),
                // The `json` operators return the verbatim source substring.
                Some(raw) => Value::Json(raw.to_string()),
            })
        }
        other => unreachable!("expected json/jsonb target, got {other:?}"),
    }
}

/// Whether the operator is one of the `->>`/`#>>` spellings, which return `text`.
fn json_returns_text(f: JsonFn) -> bool {
    matches!(
        f,
        JsonFn::ObjectFieldText | JsonFn::ArrayElementText | JsonFn::ExtractPathText
    )
}

fn path_of(v: &Value) -> &geo::PathVal {
    match v {
        Value::Path(p) => p,
        other => unreachable!("expected path arg, got {other:?}"),
    }
}

fn box_of(v: &Value) -> [f64; 4] {
    match v {
        Value::Box(b) => *b,
        other => unreachable!("expected box arg, got {other:?}"),
    }
}

fn line_of(v: &Value) -> [f64; 3] {
    match v {
        Value::Line(l) => *l,
        other => unreachable!("expected line arg, got {other:?}"),
    }
}

fn circle_of(v: &Value) -> [f64; 3] {
    match v {
        Value::Circle(c) => *c,
        other => unreachable!("expected circle arg, got {other:?}"),
    }
}

fn polygon_of(v: &Value) -> &geo::PolygonVal {
    match v {
        Value::Polygon(p) => p,
        other => unreachable!("expected polygon arg, got {other:?}"),
    }
}

/// Evaluate a geometric operator or function. Arguments
/// arrive in the fixed order documented on each [`GeoFn`]; a geometric error
/// (range / divide-by-zero) maps to its SQLSTATE. The cases PG has no answer for
/// — `#`'s non-intersection, `area` of an open path, concatenating a closed one,
/// the distance between two segment-less paths — are NULL.
fn eval_geo(g: GeoFn, args: &[Value]) -> Result<Value, ExecError> {
    let geo_err = |e: geo::GeoError| err(e.sqlstate, e.message);
    Ok(match g {
        GeoFn::PointConstruct => Value::Point([f8(&args[0]), f8(&args[1])]),
        GeoFn::PointDist => Value::Float8(geo::point_distance(
            &point_of(&args[0]),
            &point_of(&args[1]),
        )),
        GeoFn::PointLeft => Value::Bool(geo::point_left(&point_of(&args[0]), &point_of(&args[1]))),
        GeoFn::PointRight => {
            Value::Bool(geo::point_right(&point_of(&args[0]), &point_of(&args[1])))
        }
        GeoFn::PointAbove => {
            Value::Bool(geo::point_above(&point_of(&args[0]), &point_of(&args[1])))
        }
        GeoFn::PointBelow => {
            Value::Bool(geo::point_below(&point_of(&args[0]), &point_of(&args[1])))
        }
        GeoFn::PointEq => Value::Bool(geo::point_eq(&point_of(&args[0]), &point_of(&args[1]))),
        GeoFn::PointHoriz => Value::Bool(geo::point_horizontal(
            &point_of(&args[0]),
            &point_of(&args[1]),
        )),
        GeoFn::PointVert => Value::Bool(geo::point_vertical(
            &point_of(&args[0]),
            &point_of(&args[1]),
        )),
        GeoFn::PointAdd => {
            Value::Point(geo::point_add(&point_of(&args[0]), &point_of(&args[1])).map_err(geo_err)?)
        }
        GeoFn::PointSub => {
            Value::Point(geo::point_sub(&point_of(&args[0]), &point_of(&args[1])).map_err(geo_err)?)
        }
        GeoFn::PointMul => {
            Value::Point(geo::point_mul(&point_of(&args[0]), &point_of(&args[1])).map_err(geo_err)?)
        }
        GeoFn::PointDiv => {
            Value::Point(geo::point_div(&point_of(&args[0]), &point_of(&args[1])).map_err(geo_err)?)
        }
        GeoFn::PointSlope => {
            Value::Float8(geo::point_slope(&point_of(&args[0]), &point_of(&args[1])))
        }
        GeoFn::DistPointSeg => {
            Value::Float8(geo::dist_point_seg(&point_of(&args[0]), &lseg_of(&args[1])))
        }
        GeoFn::PointOnSeg => {
            Value::Bool(geo::point_on_seg(&point_of(&args[0]), &lseg_of(&args[1])))
        }
        GeoFn::ClosePointSeg => Value::Point(geo::close_point_seg(
            &point_of(&args[0]),
            &lseg_of(&args[1]),
        )),
        GeoFn::LsegConstruct => Value::Lseg(geo::lseg_from_points(
            &point_of(&args[0]),
            &point_of(&args[1]),
        )),
        GeoFn::LsegLength => Value::Float8(geo::lseg_length(&lseg_of(&args[0]))),
        GeoFn::LsegCenter => Value::Point(geo::lseg_center(&lseg_of(&args[0]))),
        GeoFn::LsegVert => Value::Bool(geo::lseg_vertical(&lseg_of(&args[0]))),
        GeoFn::LsegHoriz => Value::Bool(geo::lseg_horizontal(&lseg_of(&args[0]))),
        GeoFn::LsegEq => Value::Bool(geo::lseg_eq(&lseg_of(&args[0]), &lseg_of(&args[1]))),
        GeoFn::LsegNe => Value::Bool(geo::lseg_ne(&lseg_of(&args[0]), &lseg_of(&args[1]))),
        GeoFn::LsegLt => Value::Bool(geo::lseg_lt(&lseg_of(&args[0]), &lseg_of(&args[1]))),
        GeoFn::LsegLe => Value::Bool(geo::lseg_le(&lseg_of(&args[0]), &lseg_of(&args[1]))),
        GeoFn::LsegGt => Value::Bool(geo::lseg_gt(&lseg_of(&args[0]), &lseg_of(&args[1]))),
        GeoFn::LsegGe => Value::Bool(geo::lseg_ge(&lseg_of(&args[0]), &lseg_of(&args[1]))),
        GeoFn::LsegParallel => {
            Value::Bool(geo::lseg_parallel(&lseg_of(&args[0]), &lseg_of(&args[1])))
        }
        GeoFn::LsegPerpendicular => Value::Bool(geo::lseg_perpendicular(
            &lseg_of(&args[0]),
            &lseg_of(&args[1]),
        )),
        GeoFn::LsegInterpt => match geo::lseg_interpt(&lseg_of(&args[0]), &lseg_of(&args[1])) {
            Some(p) => Value::Point(p),
            None => Value::Null,
        },
        GeoFn::CloseSegSeg => match geo::close_seg_seg(&lseg_of(&args[0]), &lseg_of(&args[1])) {
            Some(p) => Value::Point(p),
            None => Value::Null,
        },
        GeoFn::DistSegSeg => {
            Value::Float8(geo::dist_seg_seg(&lseg_of(&args[0]), &lseg_of(&args[1])))
        }
        GeoFn::PathIsOpen => Value::Bool(geo::path_isopen(path_of(&args[0]))),
        GeoFn::PathIsClosed => Value::Bool(geo::path_isclosed(path_of(&args[0]))),
        GeoFn::PathPopen => Value::Path(geo::path_popen(path_of(&args[0]))),
        GeoFn::PathPclose => Value::Path(geo::path_pclose(path_of(&args[0]))),
        GeoFn::PathNpoints => Value::Int4(geo::path_npoints(path_of(&args[0]))),
        GeoFn::PathLength => Value::Float8(geo::path_length(path_of(&args[0]))),
        GeoFn::PathArea => match geo::path_area(path_of(&args[0])) {
            Some(a) => Value::Float8(a),
            None => Value::Null,
        },
        GeoFn::PathConcat => match geo::path_concat(path_of(&args[0]), path_of(&args[1])) {
            Some(p) => Value::Path(p),
            None => Value::Null,
        },
        GeoFn::PathAddPt => {
            Value::Path(geo::path_add_pt(path_of(&args[0]), &point_of(&args[1])).map_err(geo_err)?)
        }
        GeoFn::PathSubPt => {
            Value::Path(geo::path_sub_pt(path_of(&args[0]), &point_of(&args[1])).map_err(geo_err)?)
        }
        GeoFn::PathMulPt => {
            Value::Path(geo::path_mul_pt(path_of(&args[0]), &point_of(&args[1])).map_err(geo_err)?)
        }
        GeoFn::PathDivPt => {
            Value::Path(geo::path_div_pt(path_of(&args[0]), &point_of(&args[1])).map_err(geo_err)?)
        }
        GeoFn::PathDist => match geo::path_distance(path_of(&args[0]), path_of(&args[1])) {
            Some(d) => Value::Float8(d),
            None => Value::Null,
        },
        GeoFn::DistPathPoint => {
            Value::Float8(geo::dist_path_point(path_of(&args[0]), &point_of(&args[1])))
        }
        GeoFn::OnPpath => Value::Bool(geo::on_ppath(&point_of(&args[0]), path_of(&args[1]))),
        GeoFn::PathContainPt => {
            Value::Bool(geo::path_contain_pt(path_of(&args[0]), &point_of(&args[1])))
        }
        GeoFn::PathInter => Value::Bool(geo::path_inter(path_of(&args[0]), path_of(&args[1]))),
        GeoFn::PathEq
        | GeoFn::PathNe
        | GeoFn::PathLt
        | GeoFn::PathLe
        | GeoFn::PathGt
        | GeoFn::PathGe => {
            let ord = geo::path_n_cmp(path_of(&args[0]), path_of(&args[1]));
            Value::Bool(match g {
                GeoFn::PathEq => ord.is_eq(),
                GeoFn::PathNe => ord.is_ne(),
                GeoFn::PathLt => ord.is_lt(),
                GeoFn::PathLe => ord.is_le(),
                GeoFn::PathGt => ord.is_gt(),
                _ => ord.is_ge(),
            })
        }

        // -- box -----------------------------------------------------------
        GeoFn::BoxFromPoint => Value::Box(geo::box_from_point(&point_of(&args[0]))),
        GeoFn::BoxConstruct => Value::Box(geo::box_from_points(
            &point_of(&args[0]),
            &point_of(&args[1]),
        )),
        GeoFn::BoxArea => Value::Float8(geo::box_area(&box_of(&args[0]))),
        GeoFn::BoxWidth => Value::Float8(geo::box_width(&box_of(&args[0]))),
        GeoFn::BoxHeight => Value::Float8(geo::box_height(&box_of(&args[0]))),
        GeoFn::BoxCenter => Value::Point(geo::box_center(&box_of(&args[0]))),
        GeoFn::BoxDiagonal => Value::Lseg(geo::box_diagonal(&box_of(&args[0]))),
        GeoFn::BoundBox => Value::Box(geo::bound_box(&box_of(&args[0]), &box_of(&args[1]))),
        GeoFn::BoxOverlap => Value::Bool(geo::box_overlap(&box_of(&args[0]), &box_of(&args[1]))),
        GeoFn::BoxLeft => Value::Bool(geo::box_left(&box_of(&args[0]), &box_of(&args[1]))),
        GeoFn::BoxRight => Value::Bool(geo::box_right(&box_of(&args[0]), &box_of(&args[1]))),
        GeoFn::BoxOverLeft => Value::Bool(geo::box_over_left(&box_of(&args[0]), &box_of(&args[1]))),
        GeoFn::BoxOverRight => {
            Value::Bool(geo::box_over_right(&box_of(&args[0]), &box_of(&args[1])))
        }
        GeoFn::BoxBelow => Value::Bool(geo::box_below(&box_of(&args[0]), &box_of(&args[1]))),
        GeoFn::BoxAbove => Value::Bool(geo::box_above(&box_of(&args[0]), &box_of(&args[1]))),
        GeoFn::BoxOverBelow => {
            Value::Bool(geo::box_over_below(&box_of(&args[0]), &box_of(&args[1])))
        }
        GeoFn::BoxOverAbove => {
            Value::Bool(geo::box_over_above(&box_of(&args[0]), &box_of(&args[1])))
        }
        GeoFn::BoxBelowEq => Value::Bool(geo::box_below_eq(&box_of(&args[0]), &box_of(&args[1]))),
        GeoFn::BoxAboveEq => Value::Bool(geo::box_above_eq(&box_of(&args[0]), &box_of(&args[1]))),
        GeoFn::BoxContain => Value::Bool(geo::box_contain(&box_of(&args[0]), &box_of(&args[1]))),
        GeoFn::BoxContained => {
            Value::Bool(geo::box_contained(&box_of(&args[0]), &box_of(&args[1])))
        }
        GeoFn::BoxSame => Value::Bool(geo::box_same(&box_of(&args[0]), &box_of(&args[1]))),
        GeoFn::BoxIntersects => {
            Value::Bool(geo::box_intersects(&box_of(&args[0]), &box_of(&args[1])))
        }
        GeoFn::BoxIntersect => {
            geo::box_intersect(&box_of(&args[0]), &box_of(&args[1])).map_or(Value::Null, Value::Box)
        }
        // A `None` ordering means a NaN area, which every comparison answers
        // false — including `<>`. (PG gives `box` no `<>` operator at all.)
        GeoFn::BoxEq | GeoFn::BoxLt | GeoFn::BoxLe | GeoFn::BoxGt | GeoFn::BoxGe => {
            let ord = geo::box_area_cmp(&box_of(&args[0]), &box_of(&args[1]));
            Value::Bool(ord.is_some_and(|o| match g {
                GeoFn::BoxEq => o.is_eq(),
                GeoFn::BoxLt => o.is_lt(),
                GeoFn::BoxLe => o.is_le(),
                GeoFn::BoxGt => o.is_gt(),
                _ => o.is_ge(),
            }))
        }
        GeoFn::BoxAddPt => {
            Value::Box(geo::box_add_pt(&box_of(&args[0]), &point_of(&args[1])).map_err(geo_err)?)
        }
        GeoFn::BoxSubPt => {
            Value::Box(geo::box_sub_pt(&box_of(&args[0]), &point_of(&args[1])).map_err(geo_err)?)
        }
        GeoFn::BoxMulPt => {
            Value::Box(geo::box_mul_pt(&box_of(&args[0]), &point_of(&args[1])).map_err(geo_err)?)
        }
        GeoFn::BoxDivPt => {
            Value::Box(geo::box_div_pt(&box_of(&args[0]), &point_of(&args[1])).map_err(geo_err)?)
        }
        GeoFn::BoxContainPt => {
            Value::Bool(geo::box_contain_pt(&box_of(&args[0]), &point_of(&args[1])))
        }
        GeoFn::ClosePointBox => {
            Value::Point(geo::close_point_box(&point_of(&args[0]), &box_of(&args[1])))
        }
        GeoFn::DistPointBox => {
            Value::Float8(geo::dist_point_box(&point_of(&args[0]), &box_of(&args[1])))
        }
        GeoFn::LsegInsideBox => {
            Value::Bool(geo::lseg_inside_box(&lseg_of(&args[0]), &box_of(&args[1])))
        }
        GeoFn::LsegIntersectsBox => Value::Bool(geo::lseg_intersects_box(
            &lseg_of(&args[0]),
            &box_of(&args[1]),
        )),
        GeoFn::DistLsegBox => {
            Value::Float8(geo::dist_lseg_box(&lseg_of(&args[0]), &box_of(&args[1])))
        }
        GeoFn::CloseLsegBox => {
            Value::Point(geo::close_lseg_box(&lseg_of(&args[0]), &box_of(&args[1])))
        }
        GeoFn::DistBoxBox => Value::Float8(geo::dist_box_box(&box_of(&args[0]), &box_of(&args[1]))),
        GeoFn::BoxToCircle => Value::Circle(geo::box_to_circle(&box_of(&args[0]))),
        GeoFn::BoxToPolygon => Value::Polygon(geo::box_to_polygon(&box_of(&args[0]))),

        // -- line ----------------------------------------------------------
        GeoFn::LineConstruct => Value::Line(
            geo::line_from_points(&point_of(&args[0]), &point_of(&args[1])).map_err(geo_err)?,
        ),
        GeoFn::LineEq => Value::Bool(geo::line_eq(&line_of(&args[0]), &line_of(&args[1]))),
        GeoFn::LineHoriz => Value::Bool(geo::line_horizontal(&line_of(&args[0]))),
        GeoFn::LineVert => Value::Bool(geo::line_vertical(&line_of(&args[0]))),
        GeoFn::LineParallel => {
            Value::Bool(geo::line_parallel(&line_of(&args[0]), &line_of(&args[1])))
        }
        GeoFn::LinePerpendicular => Value::Bool(geo::line_perpendicular(
            &line_of(&args[0]),
            &line_of(&args[1]),
        )),
        GeoFn::LineInterpt => geo::line_interpt(&line_of(&args[0]), &line_of(&args[1]))
            .map_or(Value::Null, Value::Point),
        GeoFn::LineIntersects => {
            Value::Bool(geo::line_intersects(&line_of(&args[0]), &line_of(&args[1])))
        }
        GeoFn::DistLineLine => {
            Value::Float8(geo::dist_line_line(&line_of(&args[0]), &line_of(&args[1])))
        }
        GeoFn::DistPointLine => Value::Float8(geo::dist_point_line(
            &point_of(&args[0]),
            &line_of(&args[1]),
        )),
        GeoFn::ClosePointLine => Value::Point(geo::close_point_line(
            &point_of(&args[0]),
            &line_of(&args[1]),
        )),
        GeoFn::PointOnLine => {
            Value::Bool(geo::point_on_line(&point_of(&args[0]), &line_of(&args[1])))
        }
        GeoFn::LsegOnLine => Value::Bool(geo::lseg_on_line(&lseg_of(&args[0]), &line_of(&args[1]))),
        GeoFn::LsegIntersectsLine => Value::Bool(geo::lseg_intersects_line(
            &lseg_of(&args[0]),
            &line_of(&args[1]),
        )),
        GeoFn::DistLsegLine => {
            Value::Float8(geo::dist_lseg_line(&lseg_of(&args[0]), &line_of(&args[1])))
        }
        GeoFn::CloseLineLseg => geo::close_line_lseg(&line_of(&args[0]), &lseg_of(&args[1]))
            .map_or(Value::Null, Value::Point),
        GeoFn::LineIntersectsBox => Value::Bool(geo::line_intersects_box(
            &line_of(&args[0]),
            &box_of(&args[1]),
        )),

        // -- circle --------------------------------------------------------
        GeoFn::CircleConstruct => Value::Circle(geo::circle_from_point_radius(
            &point_of(&args[0]),
            f8(&args[1]),
        )),
        GeoFn::CircleCenter => Value::Point(geo::circle_center(&circle_of(&args[0]))),
        GeoFn::CircleRadius => Value::Float8(geo::circle_radius(&circle_of(&args[0]))),
        GeoFn::CircleDiameter => Value::Float8(geo::circle_diameter(&circle_of(&args[0]))),
        GeoFn::CircleArea => Value::Float8(geo::circle_area(&circle_of(&args[0]))),
        GeoFn::CircleToBox => Value::Box(geo::circle_to_box(&circle_of(&args[0]))),
        GeoFn::CircleFromPolygon => Value::Circle(geo::circle_from_polygon(polygon_of(&args[0]))),
        GeoFn::CircleToPolygon => Value::Polygon(
            geo::circle_to_polygon(geo::CIRCLE_POLYGON_NPTS, &circle_of(&args[0]))
                .map_err(geo_err)?,
        ),
        GeoFn::CircleToPolygonN => Value::Polygon(
            geo::circle_to_polygon(i4(&args[0]), &circle_of(&args[1])).map_err(geo_err)?,
        ),
        GeoFn::CircleSame => {
            Value::Bool(geo::circle_same(&circle_of(&args[0]), &circle_of(&args[1])))
        }
        GeoFn::CircleOverlap => Value::Bool(geo::circle_overlap(
            &circle_of(&args[0]),
            &circle_of(&args[1]),
        )),
        GeoFn::CircleLeft => {
            Value::Bool(geo::circle_left(&circle_of(&args[0]), &circle_of(&args[1])))
        }
        GeoFn::CircleRight => Value::Bool(geo::circle_right(
            &circle_of(&args[0]),
            &circle_of(&args[1]),
        )),
        GeoFn::CircleOverLeft => Value::Bool(geo::circle_over_left(
            &circle_of(&args[0]),
            &circle_of(&args[1]),
        )),
        GeoFn::CircleOverRight => Value::Bool(geo::circle_over_right(
            &circle_of(&args[0]),
            &circle_of(&args[1]),
        )),
        GeoFn::CircleBelow => Value::Bool(geo::circle_below(
            &circle_of(&args[0]),
            &circle_of(&args[1]),
        )),
        GeoFn::CircleAbove => Value::Bool(geo::circle_above(
            &circle_of(&args[0]),
            &circle_of(&args[1]),
        )),
        GeoFn::CircleOverBelow => Value::Bool(geo::circle_over_below(
            &circle_of(&args[0]),
            &circle_of(&args[1]),
        )),
        GeoFn::CircleOverAbove => Value::Bool(geo::circle_over_above(
            &circle_of(&args[0]),
            &circle_of(&args[1]),
        )),
        GeoFn::CircleContain => Value::Bool(geo::circle_contain(
            &circle_of(&args[0]),
            &circle_of(&args[1]),
        )),
        GeoFn::CircleContained => Value::Bool(geo::circle_contained(
            &circle_of(&args[0]),
            &circle_of(&args[1]),
        )),
        GeoFn::CircleContainPt => Value::Bool(geo::circle_contain_pt(
            &circle_of(&args[0]),
            &point_of(&args[1]),
        )),
        GeoFn::CircleContainPtSwapped => Value::Bool(geo::circle_contain_pt(
            &circle_of(&args[1]),
            &point_of(&args[0]),
        )),
        GeoFn::CircleEq
        | GeoFn::CircleNe
        | GeoFn::CircleLt
        | GeoFn::CircleLe
        | GeoFn::CircleGt
        | GeoFn::CircleGe => {
            let ord = geo::circle_area_cmp(&circle_of(&args[0]), &circle_of(&args[1]));
            Value::Bool(ord.is_some_and(|o| match g {
                GeoFn::CircleEq => o.is_eq(),
                GeoFn::CircleNe => o.is_ne(),
                GeoFn::CircleLt => o.is_lt(),
                GeoFn::CircleLe => o.is_le(),
                GeoFn::CircleGt => o.is_gt(),
                _ => o.is_ge(),
            }))
        }
        GeoFn::DistCircleCircle => Value::Float8(geo::dist_circle_circle(
            &circle_of(&args[0]),
            &circle_of(&args[1]),
        )),
        GeoFn::DistPointCircle => Value::Float8(geo::dist_point_circle(
            &point_of(&args[0]),
            &circle_of(&args[1]),
        )),
        GeoFn::CircleAddPt => Value::Circle(
            geo::circle_add_pt(&circle_of(&args[0]), &point_of(&args[1])).map_err(geo_err)?,
        ),
        GeoFn::CircleSubPt => Value::Circle(
            geo::circle_sub_pt(&circle_of(&args[0]), &point_of(&args[1])).map_err(geo_err)?,
        ),
        GeoFn::CircleMulPt => Value::Circle(
            geo::circle_mul_pt(&circle_of(&args[0]), &point_of(&args[1])).map_err(geo_err)?,
        ),
        GeoFn::CircleDivPt => Value::Circle(
            geo::circle_div_pt(&circle_of(&args[0]), &point_of(&args[1])).map_err(geo_err)?,
        ),

        // -- polygon -------------------------------------------------------
        GeoFn::PolyNpoints => Value::Int4(geo::poly_npoints(polygon_of(&args[0]))),
        GeoFn::PolyCenter => Value::Point(geo::poly_center(polygon_of(&args[0]))),
        GeoFn::PolyToBox => Value::Box(geo::poly_bbox(polygon_of(&args[0]))),
        GeoFn::PolyToPath => Value::Path(geo::poly_to_path(polygon_of(&args[0]))),
        GeoFn::PathToPolygon => {
            Value::Polygon(geo::path_to_polygon(path_of(&args[0])).map_err(geo_err)?)
        }
        GeoFn::PolySame => Value::Bool(geo::poly_same(polygon_of(&args[0]), polygon_of(&args[1]))),
        GeoFn::PolyOverlap => Value::Bool(geo::poly_overlap(
            polygon_of(&args[0]),
            polygon_of(&args[1]),
        )),
        GeoFn::PolyLeft => Value::Bool(geo::poly_left(polygon_of(&args[0]), polygon_of(&args[1]))),
        GeoFn::PolyRight => {
            Value::Bool(geo::poly_right(polygon_of(&args[0]), polygon_of(&args[1])))
        }
        GeoFn::PolyOverLeft => Value::Bool(geo::poly_over_left(
            polygon_of(&args[0]),
            polygon_of(&args[1]),
        )),
        GeoFn::PolyOverRight => Value::Bool(geo::poly_over_right(
            polygon_of(&args[0]),
            polygon_of(&args[1]),
        )),
        GeoFn::PolyBelow => {
            Value::Bool(geo::poly_below(polygon_of(&args[0]), polygon_of(&args[1])))
        }
        GeoFn::PolyAbove => {
            Value::Bool(geo::poly_above(polygon_of(&args[0]), polygon_of(&args[1])))
        }
        GeoFn::PolyOverBelow => Value::Bool(geo::poly_over_below(
            polygon_of(&args[0]),
            polygon_of(&args[1]),
        )),
        GeoFn::PolyOverAbove => Value::Bool(geo::poly_over_above(
            polygon_of(&args[0]),
            polygon_of(&args[1]),
        )),
        GeoFn::PolyContain => Value::Bool(geo::poly_contain(
            polygon_of(&args[0]),
            polygon_of(&args[1]),
        )),
        GeoFn::PolyContained => Value::Bool(geo::poly_contained(
            polygon_of(&args[0]),
            polygon_of(&args[1]),
        )),
        GeoFn::PolyContainPt => Value::Bool(geo::poly_contain_pt(
            polygon_of(&args[0]),
            &point_of(&args[1]),
        )),
        GeoFn::PolyContainPtSwapped => Value::Bool(geo::poly_contain_pt(
            polygon_of(&args[1]),
            &point_of(&args[0]),
        )),
        GeoFn::DistPolyPoly => Value::Float8(geo::dist_poly_poly(
            polygon_of(&args[0]),
            polygon_of(&args[1]),
        )),
        GeoFn::DistPolyPoint => Value::Float8(geo::dist_poly_point(
            polygon_of(&args[0]),
            &point_of(&args[1]),
        )),
        GeoFn::DistPolyCircle => Value::Float8(geo::dist_poly_circle(
            polygon_of(&args[0]),
            &circle_of(&args[1]),
        )),
    })
}

fn text(v: &Value) -> &str {
    match v {
        Value::Text(s) => s,
        other => unreachable!("expected text arg, got {other:?}"),
    }
}

fn ts(v: &Value) -> i64 {
    match v {
        Value::Timestamp(t) => *t,
        other => unreachable!("expected timestamp arg, got {other:?}"),
    }
}

fn iv(v: &Value) -> Interval {
    match v {
        Value::Interval(iv) => *iv,
        other => unreachable!("expected interval arg, got {other:?}"),
    }
}

fn iv_err(e: crabgresql_types::interval::IntervalError) -> ExecError {
    ExecError::new(e.sqlstate, e.message)
}

/// `FormatError` carries the DETAIL/HINT lines PG prints for a `to_char` /
/// `to_date` / `to_timestamp` failure, which `DateError` and `TimestampError`
/// have no room for.
fn fmt_err(e: crabgresql_types::formatting::FormatError) -> ExecError {
    ExecError::new(e.sqlstate, e.message)
        .with_detail(e.detail)
        .with_hint(e.hint)
}

/// `to_char` returns SQL NULL for the values PG declines to render.
fn null_or_text(rendered: Option<String>) -> Value {
    rendered.map_or(Value::Null, Value::Text)
}

fn tstz(v: &Value) -> i64 {
    match v {
        Value::TimestampTz(t) => *t,
        other => unreachable!("expected timestamptz arg, got {other:?}"),
    }
}

fn dt(v: &Value) -> i32 {
    match v {
        Value::Date(d) => *d,
        other => unreachable!("expected date arg, got {other:?}"),
    }
}

fn tm(v: &Value) -> i64 {
    match v {
        Value::Time(t) => *t,
        other => unreachable!("expected time arg, got {other:?}"),
    }
}

fn ttz(v: &Value) -> TimeTz {
    match v {
        Value::TimeTz(t) => *t,
        other => unreachable!("expected timetz arg, got {other:?}"),
    }
}

fn date_err(e: crabgresql_types::date::DateError) -> ExecError {
    ExecError::new(e.sqlstate, e.message)
}

fn time_err(e: crabgresql_types::time::TimeError) -> ExecError {
    ExecError::new(e.sqlstate, e.message)
}

fn clock_err(e: crabgresql_types::fmt::ClockError) -> ExecError {
    ExecError::new(e.sqlstate, e.message)
}

/// `current_date` as a zone-less `timestamp`: the anchor the one-argument
/// `age()` measures from. Reads the transaction clock, not the wall clock, so
/// `age(x)` is stable across a transaction the way `now()` is.
fn today_midnight(fmt: &FmtCtx) -> Result<i64, ExecError> {
    let now = fmt.xact_start().map_err(clock_err)?;
    timestamptz::today_midnight_local(now, fmt.zone.zone()).map_err(ts_err)
}

fn timetz_err(e: crabgresql_types::timetz::TimeTzError) -> ExecError {
    ExecError::new(e.sqlstate, e.message)
}

fn i4(v: &Value) -> i32 {
    match v {
        Value::Int4(n) => *n,
        other => unreachable!("expected int4 arg, got {other:?}"),
    }
}

fn tid(v: &Value) -> (u32, u16) {
    match v {
        Value::Tid { block, offset } => (*block, *offset),
        other => unreachable!("expected tid arg, got {other:?}"),
    }
}

fn xid8(v: &Value) -> u64 {
    match v {
        Value::Xid8(x) => *x,
        other => unreachable!("expected xid8 arg, got {other:?}"),
    }
}

fn lsn(v: &Value) -> u64 {
    match v {
        Value::PgLsn(x) => *x,
        other => unreachable!("expected pg_lsn arg, got {other:?}"),
    }
}

fn money_of(v: &Value) -> i64 {
    match v {
        Value::Money(c) => *c,
        other => unreachable!("expected money arg, got {other:?}"),
    }
}

/// Element equality for array containment/overlap. PG's `array_contain_compare`
/// treats a NULL element as matching nothing (a NULL is never "contained", even
/// by another NULL), so any NULL operand is unequal; two non-NULLs use the
/// element type's total order.
fn elem_eq(elem: PgType, x: &Value, y: &Value) -> bool {
    !matches!(x, Value::Null)
        && !matches!(y, Value::Null)
        && crate::eval::compare_values(elem, x, y) == std::cmp::Ordering::Equal
}

/// `a @> b`: every element of `b` is present in `a` (element equality). The
/// element type is read from `a`'s array value.
fn array_contains(a: &Value, b: &Value) -> bool {
    let elem = match a {
        Value::Array { elem, .. } => *elem,
        _ => unreachable!("array_contains left is not an array"),
    };
    let (ae, be) = (array_elems(a), array_elems(b));
    be.iter().all(|y| ae.iter().any(|x| elem_eq(elem, x, y)))
}

/// `a && b`: the arrays share at least one (non-NULL) element.
fn array_overlap(a: &Value, b: &Value) -> bool {
    let elem = match a {
        Value::Array { elem, .. } => *elem,
        _ => unreachable!("array_overlap left is not an array"),
    };
    let (ae, be) = (array_elems(a), array_elems(b));
    ae.iter()
        .any(|x| !matches!(x, Value::Null) && be.iter().any(|y| elem_eq(elem, x, y)))
}

fn cash_err(e: crabgresql_types::money::MoneyError) -> ExecError {
    ExecError::new(e.sqlstate, e.message)
}

fn i8(v: &Value) -> i64 {
    match v {
        Value::Int8(n) => *n,
        other => unreachable!("expected int8 arg, got {other:?}"),
    }
}

fn bytea(v: &Value) -> &[u8] {
    match v {
        Value::Bytea(b) => b,
        other => unreachable!("expected bytea arg, got {other:?}"),
    }
}

fn ts_err(e: timestamp::TimestampError) -> ExecError {
    ExecError::new(e.sqlstate, e.message)
}

fn num(v: &Value) -> &Numeric {
    match v {
        Value::Numeric(n) => n,
        other => unreachable!("expected numeric arg, got {other:?}"),
    }
}

fn inet(v: &Value) -> &Inet {
    match v {
        Value::Inet(i) | Value::Cidr(i) => i,
        other => unreachable!("expected inet/cidr arg, got {other:?}"),
    }
}

fn net_err(e: net::NetError) -> ExecError {
    ExecError::new(e.sqlstate, e.message).with_detail(e.detail.map(String::from))
}

fn num_err(e: crabgresql_types::numeric::NumErr) -> ExecError {
    ExecError::new(e.sqlstate, e.message).with_detail(e.detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boolean_equality_functions_are_strict() -> anyhow::Result<()> {
        for (left, right, equal) in [
            (false, false, true),
            (false, true, false),
            (true, false, false),
            (true, true, true),
        ] {
            let args = [Value::Bool(left), Value::Bool(right)];
            assert_eq!(
                eval_scalar(ScalarFn::BoolEq, &args, &FmtCtx::utc_default())?,
                Value::Bool(equal)
            );
            assert_eq!(
                eval_scalar(ScalarFn::BoolNe, &args, &FmtCtx::utc_default())?,
                Value::Bool(!equal)
            );
        }
        for func in [ScalarFn::BoolEq, ScalarFn::BoolNe] {
            assert_eq!(
                eval_scalar(
                    func,
                    &[Value::Bool(true), Value::Null],
                    &FmtCtx::utc_default()
                )?,
                Value::Null
            );
        }

        Ok(())
    }

    fn call(f: ScalarFn, x: f64) -> f64 {
        let result = match eval_scalar(f, &[Value::Float8(x)], &FmtCtx::utc_default()) {
            Ok(value) => value,
            Err(error) => panic!("scalar-function test fixture failed: {error}"),
        };
        match result {
            Value::Float8(v) => v,
            other => panic!("expected float8, got {other:?}"),
        }
    }

    #[test]
    fn degree_trig_exact_endpoints_and_pole_signs() {
        // Exact special-angle values the IN-list tests depend on.
        assert_eq!(call(ScalarFn::Sind, 30.0), 0.5);
        assert_eq!(call(ScalarFn::Sind, 270.0), -1.0);
        assert_eq!(call(ScalarFn::Cosd, 90.0), 0.0);
        assert_eq!(call(ScalarFn::Tand, 45.0), 1.0);
        assert_eq!(call(ScalarFn::Tand, 135.0), -1.0);
        assert_eq!(call(ScalarFn::Tand, 225.0), 1.0);
        // Pole signs: the period-360 reduction keeps them distinct.
        assert_eq!(call(ScalarFn::Tand, 90.0), f64::INFINITY);
        assert_eq!(call(ScalarFn::Tand, 270.0), f64::NEG_INFINITY);
        assert_eq!(call(ScalarFn::Cotd, 0.0), f64::INFINITY);
        assert_eq!(call(ScalarFn::Cotd, 180.0), f64::NEG_INFINITY);
        // tand(180)/cotd(270) are +0, not -0.
        assert!(call(ScalarFn::Tand, 180.0).is_sign_positive());
        assert_eq!(call(ScalarFn::Tand, 180.0), 0.0);
        assert!(call(ScalarFn::Cotd, 270.0).is_sign_positive());
        assert_eq!(call(ScalarFn::Cotd, 270.0), 0.0);
    }

    #[test]
    fn atanh_and_sign_preserve_nan() {
        assert!(call(ScalarFn::Atanh, f64::NAN).is_nan());
        assert_eq!(
            eval_scalar(
                ScalarFn::Atanh,
                &[Value::Float8(2.0)],
                &FmtCtx::utc_default()
            )
            .expect_err("atanh of an argument outside [-1, 1] must be rejected")
            .code,
            "22003"
        );
        assert!(call(ScalarFn::Sign, f64::NAN).is_nan());
        // sign(-0.0) keeps the negative zero.
        assert!(call(ScalarFn::Sign, -0.0).is_sign_negative());
    }
}
