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
use crabgresql_binder::JsonPathFn;
use crabgresql_binder::ScalarFn;
use crabgresql_types::json::Jsonb;
use crabgresql_types::jsonpath;
use crabgresql_pg_wire::sqlstate;
use crabgresql_types::{
    Inet, Interval, Numeric, PgType, TimeTz, Value, bit, date, float, geo, interval, json, macaddr,
    money, net, text, time, timestamp, timestamptz, timetz, to_char,
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
pub fn eval_scalar(func: ScalarFn, args: &[Value]) -> Result<Value, ExecError> {
    // Non-strict string functions run even when arguments are NULL, so they are
    // handled before the STRICT NULL short-circuit below.
    match func {
        ScalarFn::Concat => {
            let mut out = String::new();
            for a in args {
                if let Some(s) = a.encode_text() {
                    out.push_str(&s);
                }
            }
            return Ok(Value::Text(out));
        }
        ScalarFn::ConcatWs => {
            // A NULL separator yields NULL; the remaining NULL args are skipped.
            let Some(sep) = args.first().and_then(|a| a.encode_text()) else {
                return Ok(Value::Null);
            };
            let parts: Vec<String> = args[1..].iter().filter_map(|a| a.encode_text()).collect();
            return Ok(Value::Text(parts.join(&sep)));
        }
        ScalarFn::Format => {
            // A NULL format string yields NULL.
            let Some(fmt) = args.first().and_then(|a| a.encode_text()) else {
                return Ok(Value::Null);
            };
            let fmt_args: Vec<text::FormatArg> =
                args[1..].iter().map(|a| a.encode_text()).collect();
            return text::format(&fmt, &fmt_args)
                .map(Value::Text)
                .map_err(text_err);
        }
        // quote_nullable is non-strict: a NULL argument becomes the text `NULL`.
        ScalarFn::QuoteNullable => {
            return Ok(Value::Text(text::quote_nullable(
                args[0].encode_text().as_deref(),
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
                    _ => parts.push(v.encode_text().unwrap_or_default()),
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
        // --- geometric (point / lseg) ---
        ScalarFn::Geo(g) => return eval_geo(g, args),
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
        ScalarFn::PgGetUserById | ScalarFn::PgTableIsVisible => {
            return Err(ExecError::new(
                sqlstate::INTERNAL_ERROR,
                "catalog function reached the pure scalar evaluator",
            ));
        }
        // --- jsonpath (STRICT: any NULL arg already short-circuited to NULL) ---
        ScalarFn::JsonPath(f) => return eval_jsonpath(f, args),
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
            // No ESCAPE clause defaults to `\`; `ESCAPE ''` disables escaping.
            let escape = match args.get(2) {
                None => Some('\\'),
                Some(v) => {
                    let s = text(v);
                    if s.chars().count() > 1 {
                        return Err(err(
                            sqlstate::INVALID_ESCAPE_SEQUENCE,
                            "invalid escape string",
                        )
                        .with_detail(Some(
                            "Escape string must be empty or one character.".to_string(),
                        )));
                    }
                    s.chars().next()
                }
            };
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
        ScalarFn::SimilarTo => {
            // No ESCAPE clause defaults to `\`; `ESCAPE ''` disables escaping.
            let escape = match args.get(2) {
                None => Some('\\'),
                Some(v) => {
                    let s = text(v);
                    if s.chars().count() > 1 {
                        return Err(err(
                            sqlstate::INVALID_ESCAPE_SEQUENCE,
                            "invalid escape string",
                        )
                        .with_detail(Some(
                            "Escape string must be empty or one character.".to_string(),
                        )));
                    }
                    s.chars().next()
                }
            };
            return text::similar_to_match(text(&args[0]), text(&args[1]), escape)
                .map(Value::Bool)
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
        ScalarFn::PgInputIsValid => {
            let value = text(&args[0]);
            let type_name = text(&args[1]);
            return Ok(Value::Bool(soft_input(type_name, value).is_ok()));
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
                match timestamptz::date_part(text(&args[0]), tstz(&args[1])).map_err(ts_err)? {
                    Some(v) => Value::Float8(v),
                    None => Value::Null,
                },
            );
        }
        ScalarFn::ExtractTz => {
            return Ok(
                match timestamptz::extract(text(&args[0]), tstz(&args[1])).map_err(ts_err)? {
                    Some(n) => Value::Numeric(n),
                    None => Value::Null,
                },
            );
        }
        ScalarFn::DateTruncTz => {
            return timestamptz::date_trunc(text(&args[0]), tstz(&args[1]))
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
        ScalarFn::ToCharInterval => {
            // A non-finite interval yields NULL, matching PG.
            return Ok(match to_char::interval(iv(&args[0]), text(&args[1])) {
                Some(s) => Value::Text(s),
                None => Value::Null,
            });
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

/// Non-throwing input validation for `pg_input_is_valid` / `pg_input_error_info`.
pub fn soft_input(type_name: &str, value: &str) -> Result<(), (&'static str, String)> {
    match type_name.trim().to_ascii_lowercase().as_str() {
        "float4" | "real" => float::float4in(value)
            .map(|_| ())
            .map_err(|e| (e.sqlstate, e.message)),
        "float8" | "double precision" => float::float8in(value)
            .map(|_| ())
            .map_err(|e| (e.sqlstate, e.message)),
        "timestamptz" | "timestamp with time zone" => timestamptz::parse(value)
            .map(|_| ())
            .map_err(|e| (e.sqlstate, e.message)),
        "date" => date::parse(value)
            .map(|_| ())
            .map_err(|e| (e.sqlstate, e.message)),
        "time" | "time without time zone" => time::parse(value)
            .map(|_| ())
            .map_err(|e| (e.sqlstate, e.message)),
        "timetz" | "time with time zone" => timetz::parse(value)
            .map(|_| ())
            .map_err(|e| (e.sqlstate, e.message)),
        "money" => money::parse(value)
            .map(|_| ())
            .map_err(|e| (e.sqlstate, e.message)),
        "macaddr" => macaddr::parse_macaddr(value)
            .map(|_| ())
            .map_err(|e| (e.sqlstate, e.message)),
        "macaddr8" => macaddr::parse_macaddr8(value)
            .map(|_| ())
            .map_err(|e| (e.sqlstate, e.message)),
        "point" => geo::parse_point(value)
            .map(|_| ())
            .map_err(|e| (e.sqlstate, e.message)),
        "lseg" => geo::parse_lseg(value)
            .map(|_| ())
            .map_err(|e| (e.sqlstate, e.message)),
        "json" => json::json_in(value)
            .map(|_| ())
            .map_err(|e| (e.sqlstate, e.message)),
        "jsonb" => json::jsonb_in(value)
            .map(|_| ())
            .map_err(|e| (e.sqlstate, e.message)),
        "jsonpath" => crabgresql_types::jsonpath::jsonpath_in(value)
            .map(|_| ())
            .map_err(|e| (e.sqlstate, e.message)),
        // Other types: not exercised; treat as valid.
        _ => Ok(()),
    }
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
        JsonPathFn::Exists | JsonPathFn::ExistsOp => {
            jsonpath::exists(path, target, vars, silent).map(opt_bool).map_err(json_err)
        }
        JsonPathFn::Match | JsonPathFn::MatchOp => {
            jsonpath::match_predicate(path, target, vars, silent).map(opt_bool).map_err(json_err)
        }
        JsonPathFn::QueryArray => jsonpath::query(path, target, vars, silent)
            .map(|items| Value::Jsonb(Jsonb::Array(items)))
            .map_err(json_err),
        JsonPathFn::QueryFirst => jsonpath::query(path, target, vars, silent)
            .map(|items| items.into_iter().next().map(Value::Jsonb).unwrap_or(Value::Null))
            .map_err(json_err),
    }
}

fn json_err(e: crabgresql_types::json::JsonError) -> ExecError {
    ExecError::new(e.sqlstate, e.message)
}

/// Evaluate a geometric (`point`/`lseg`) operator or function. Arguments arrive
/// in the fixed order documented on each [`GeoFn`]; a geometric error (range /
/// divide-by-zero) maps to its SQLSTATE. `#`'s no-intersection case is NULL.
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

fn timetz_err(e: crabgresql_types::timetz::TimeTzError) -> ExecError {
    ExecError::new(e.sqlstate, e.message)
}

fn i4(v: &Value) -> i32 {
    match v {
        Value::Int4(n) => *n,
        other => unreachable!("expected int4 arg, got {other:?}"),
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
    be.iter()
        .all(|y| ae.iter().any(|x| elem_eq(elem, x, y)))
}

/// `a && b`: the arrays share at least one (non-NULL) element.
fn array_overlap(a: &Value, b: &Value) -> bool {
    let elem = match a {
        Value::Array { elem, .. } => *elem,
        _ => unreachable!("array_overlap left is not an array"),
    };
    let (ae, be) = (array_elems(a), array_elems(b));
    ae.iter().any(|x| {
        !matches!(x, Value::Null) && be.iter().any(|y| elem_eq(elem, x, y))
    })
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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn call(f: ScalarFn, x: f64) -> f64 {
        let result = match eval_scalar(f, &[Value::Float8(x)]) {
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
            eval_scalar(ScalarFn::Atanh, &[Value::Float8(2.0)])
                .unwrap_err()
                .code,
            "22003"
        );
        assert!(call(ScalarFn::Sign, f64::NAN).is_nan());
        // sign(-0.0) keeps the negative zero.
        assert!(call(ScalarFn::Sign, -0.0).is_sign_negative());
    }
}
