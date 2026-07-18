//! Scalar function evaluation.
//!
//! Clean-room (see AGENTS.md): every function reproduces PG's *observable*
//! result — value, and the SQLSTATE/message of any domain/range error — pinned
//! by the float regression corpus. The degree-based trig functions are built to
//! return exact values at the special angles the tests check (e.g.
//! `sind(30) = 0.5` exactly), using an independently-derived first-quadrant
//! reduction with libm calls forced to run time via `black_box`.

use std::hint::black_box;

use crabgresql_binder::ScalarFn;
use crabgresql_pg_wire::sqlstate;
use crabgresql_types::{
    Interval, Numeric, TimeTz, Value, date, float, interval, time, timestamp, timestamptz, timetz,
    to_char,
};

use crate::ExecError;

const RADIANS_PER_DEGREE: f64 = 0.017_453_292_519_943_295;

fn err(sqlstate: &'static str, message: impl Into<String>) -> ExecError {
    ExecError::new(sqlstate, message)
}

fn overflow() -> ExecError {
    err(sqlstate::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow")
}

fn underflow() -> ExecError {
    err(sqlstate::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: underflow")
}

fn out_of_range_input() -> ExecError {
    err(sqlstate::NUMERIC_VALUE_OUT_OF_RANGE, "input is out of range")
}

/// Evaluate a scalar function. All functions are STRICT: a NULL argument yields
/// NULL without invoking the function.
pub fn eval_scalar(func: ScalarFn, args: &[Value]) -> Result<Value, ExecError> {
    if args.iter().any(|a| matches!(a, Value::Null)) {
        return Ok(Value::Null);
    }
    match func {
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
            return Ok(match timestamp::date_part(text(&args[0]), ts(&args[1])).map_err(ts_err)? {
                Some(v) => Value::Float8(v),
                None => Value::Null,
            });
        }
        ScalarFn::Extract => {
            return Ok(match timestamp::extract(text(&args[0]), ts(&args[1])).map_err(ts_err)? {
                Some(n) => Value::Numeric(n),
                None => Value::Null,
            });
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
            return num(&args[0]).modulo(num(&args[1])).map(Value::Numeric).map_err(num_err);
        }
        // `mod(intN, intN)`: remainder truncated toward zero (`MIN % -1 = 0`),
        // division by zero is 22012 — same semantics as the `%` operator.
        ScalarFn::ModInt => {
            let zero = || err(sqlstate::DIVISION_BY_ZERO, "division by zero");
            return match (&args[0], &args[1]) {
                (Value::Int2(a), Value::Int2(b)) => {
                    if *b == 0 { Err(zero()) } else { Ok(Value::Int2(a.checked_rem(*b).unwrap_or(0))) }
                }
                (Value::Int4(a), Value::Int4(b)) => {
                    if *b == 0 { Err(zero()) } else { Ok(Value::Int4(a.checked_rem(*b).unwrap_or(0))) }
                }
                (Value::Int8(a), Value::Int8(b)) => {
                    if *b == 0 { Err(zero()) } else { Ok(Value::Int8(a.checked_rem(*b).unwrap_or(0))) }
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
            return num(&args[0]).log_base(num(&args[1])).map(Value::Numeric).map_err(num_err);
        }
        ScalarFn::NumExp => return num(&args[0]).exp().map(Value::Numeric).map_err(num_err),
        ScalarFn::NumPower => {
            return num(&args[0]).power(num(&args[1])).map(Value::Numeric).map_err(num_err);
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
            return interval::negate(iv(&args[0])).map(Value::Interval).map_err(iv_err);
        }
        ScalarFn::IntervalPl => {
            return interval::add(iv(&args[0]), iv(&args[1])).map(Value::Interval).map_err(iv_err);
        }
        ScalarFn::IntervalMi => {
            return interval::sub(iv(&args[0]), iv(&args[1])).map(Value::Interval).map_err(iv_err);
        }
        ScalarFn::IntervalMul => {
            return interval::mul(iv(&args[0]), f8(&args[1])).map(Value::Interval).map_err(iv_err);
        }
        ScalarFn::IntervalDiv => {
            return interval::div(iv(&args[0]), f8(&args[1])).map(Value::Interval).map_err(iv_err);
        }
        ScalarFn::TimestampPlInterval => {
            return timestamp::pl_interval(ts(&args[0]), iv(&args[1])).map(Value::Timestamp).map_err(ts_err);
        }
        ScalarFn::TimestampMiInterval => {
            return timestamp::mi_interval(ts(&args[0]), iv(&args[1])).map(Value::Timestamp).map_err(ts_err);
        }
        ScalarFn::TimestampMi => {
            return timestamp::mi(ts(&args[0]), ts(&args[1])).map(Value::Interval).map_err(ts_err);
        }

        // --- interval functions ---
        ScalarFn::DatePartInterval => {
            return Ok(match interval::date_part(text(&args[0]), iv(&args[1])).map_err(iv_err)? {
                Some(v) => Value::Float8(v),
                None => Value::Null,
            });
        }
        ScalarFn::ExtractInterval => {
            return Ok(match interval::extract(text(&args[0]), iv(&args[1])).map_err(iv_err)? {
                Some(n) => Value::Numeric(n),
                None => Value::Null,
            });
        }
        ScalarFn::DateTruncInterval => {
            return interval::date_trunc(text(&args[0]), iv(&args[1])).map(Value::Interval).map_err(iv_err);
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
            return interval::justify_days(iv(&args[0])).map(Value::Interval).map_err(iv_err);
        }
        ScalarFn::JustifyHours => {
            return interval::justify_hours(iv(&args[0])).map(Value::Interval).map_err(iv_err);
        }
        ScalarFn::JustifyInterval => {
            return interval::justify_interval(iv(&args[0])).map(Value::Interval).map_err(iv_err);
        }
        ScalarFn::Age => {
            return timestamp::age(ts(&args[0]), ts(&args[1])).map(Value::Interval).map_err(ts_err);
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
            return date::add_days(dt(&args[0]), i4(&args[1])).map(Value::Date).map_err(date_err);
        }
        ScalarFn::DateMiDays => {
            return date::sub_days(dt(&args[0]), i4(&args[1])).map(Value::Date).map_err(date_err);
        }
        ScalarFn::DateMi => {
            return date::sub_date(dt(&args[0]), dt(&args[1])).map(Value::Int4).map_err(date_err);
        }
        ScalarFn::DatePlInterval => {
            return date::pl_interval(dt(&args[0]), iv(&args[1])).map(Value::Timestamp).map_err(date_err);
        }
        ScalarFn::DateMiInterval => {
            return date::mi_interval(dt(&args[0]), iv(&args[1])).map(Value::Timestamp).map_err(date_err);
        }
        ScalarFn::DatePlTime => {
            return date::pl_time(dt(&args[0]), tm(&args[1])).map(Value::Timestamp).map_err(date_err);
        }
        ScalarFn::DatePlTimeTz => {
            return date::pl_timetz(dt(&args[0]), ttz(&args[1])).map(Value::TimestampTz).map_err(date_err);
        }
        ScalarFn::DatePartDate => {
            return Ok(match date::date_part(text(&args[0]), dt(&args[1])).map_err(date_err)? {
                Some(v) => Value::Float8(v),
                None => Value::Null,
            });
        }
        ScalarFn::ExtractDate => {
            return Ok(match date::extract(text(&args[0]), dt(&args[1])).map_err(date_err)? {
                Some(n) => Value::Numeric(n),
                None => Value::Null,
            });
        }
        ScalarFn::IsfiniteDate => {
            return Ok(Value::Bool(date::is_finite(dt(&args[0]))));
        }
        ScalarFn::MakeDate => {
            return date::make_date(i4(&args[0]) as i64, i4(&args[1]) as i64, i4(&args[2]) as i64)
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
            return time::date_part(text(&args[0]), tm(&args[1])).map(Value::Float8).map_err(time_err);
        }
        ScalarFn::ExtractTime => {
            return time::extract(text(&args[0]), tm(&args[1])).map(Value::Numeric).map_err(time_err);
        }
        ScalarFn::MakeTime => {
            return time::make_time(i4(&args[0]) as i64, i4(&args[1]) as i64, f8(&args[2]))
                .map(Value::Time)
                .map_err(time_err);
        }

        // --- timetz operators/functions ---
        ScalarFn::TimeTzPlInterval => {
            return Ok(Value::TimeTz(timetz::pl_interval(ttz(&args[0]), iv(&args[1]))));
        }
        ScalarFn::TimeTzMiInterval => {
            return Ok(Value::TimeTz(timetz::mi_interval(ttz(&args[0]), iv(&args[1]))));
        }
        ScalarFn::DatePartTimeTz => {
            return timetz::date_part(text(&args[0]), ttz(&args[1])).map(Value::Float8).map_err(timetz_err);
        }
        ScalarFn::ExtractTimeTz => {
            return timetz::extract(text(&args[0]), ttz(&args[1])).map(Value::Numeric).map_err(timetz_err);
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
        ScalarFn::Float4Send
        | ScalarFn::Float8Send
        | ScalarFn::PgInputIsValid
        | ScalarFn::Md5
        | ScalarFn::DatePart
        | ScalarFn::Extract
        | ScalarFn::DateTrunc
        | ScalarFn::Isfinite
        | ScalarFn::MakeTimestamp
        | ScalarFn::IntervalNeg
        | ScalarFn::IntervalPl
        | ScalarFn::IntervalMi
        | ScalarFn::IntervalMul
        | ScalarFn::IntervalDiv
        | ScalarFn::TimestampPlInterval
        | ScalarFn::TimestampMiInterval
        | ScalarFn::TimestampMi
        | ScalarFn::DatePartInterval
        | ScalarFn::ExtractInterval
        | ScalarFn::DateTruncInterval
        | ScalarFn::IsfiniteInterval
        | ScalarFn::MakeInterval
        | ScalarFn::JustifyDays
        | ScalarFn::JustifyHours
        | ScalarFn::JustifyInterval
        | ScalarFn::Age
        | ScalarFn::ToCharInterval
        | ScalarFn::DatePartTz
        | ScalarFn::ExtractTz
        | ScalarFn::DateTruncTz
        | ScalarFn::IsfiniteTz
        | ScalarFn::MakeTimestampTz
        | ScalarFn::TimezoneToTz
        | ScalarFn::TimezoneToTs
        | ScalarFn::DatePlDays
        | ScalarFn::DateMiDays
        | ScalarFn::DateMi
        | ScalarFn::DatePlInterval
        | ScalarFn::DateMiInterval
        | ScalarFn::DatePlTime
        | ScalarFn::DatePlTimeTz
        | ScalarFn::DatePartDate
        | ScalarFn::ExtractDate
        | ScalarFn::IsfiniteDate
        | ScalarFn::MakeDate
        | ScalarFn::TimePlInterval
        | ScalarFn::TimeMiInterval
        | ScalarFn::TimeMi
        | ScalarFn::DatePartTime
        | ScalarFn::ExtractTime
        | ScalarFn::MakeTime
        | ScalarFn::TimeTzPlInterval
        | ScalarFn::TimeTzMiInterval
        | ScalarFn::DatePartTimeTz
        | ScalarFn::ExtractTimeTz
        | ScalarFn::NumRound
        | ScalarFn::NumTrunc
        | ScalarFn::NumCeil
        | ScalarFn::NumFloor
        | ScalarFn::NumAbs
        | ScalarFn::NumSign
        | ScalarFn::NumMod
        | ScalarFn::NumSqrt
        | ScalarFn::NumLn
        | ScalarFn::NumLog10
        | ScalarFn::NumLog
        | ScalarFn::NumExp
        | ScalarFn::NumPower
        | ScalarFn::NumApplyTypmod
        | ScalarFn::ModInt => unreachable!("numeric functions return early"),
    };
    result.map(Value::Float8)
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
        return Err(err(sqlstate::INVALID_ARGUMENT_FOR_LOG, "cannot take logarithm of zero"));
    }
    if x < 0.0 {
        return Err(err(sqlstate::INVALID_ARGUMENT_FOR_LOG, "cannot take logarithm of a negative number"));
    }
    Ok(x.ln())
}

/// `dlog10`: base-10 logarithm with PG's domain errors.
fn dlog10(x: f64) -> Result<f64, ExecError> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x == 0.0 {
        return Err(err(sqlstate::INVALID_ARGUMENT_FOR_LOG, "cannot take logarithm of zero"));
    }
    if x < 0.0 {
        return Err(err(sqlstate::INVALID_ARGUMENT_FOR_LOG, "cannot take logarithm of a negative number"));
    }
    Ok(x.log10())
}

fn dgamma(x: f64) -> Result<f64, ExecError> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return if x > 0.0 { Ok(f64::INFINITY) } else { Err(overflow()) };
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
    Ok(if x >= 0.0 {
        asind_q1(x)
    } else {
        -asind_q1(-x)
    })
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
        "date" => date::parse(value).map(|_| ()).map_err(|e| (e.sqlstate, e.message)),
        "time" | "time without time zone" => {
            time::parse(value).map(|_| ()).map_err(|e| (e.sqlstate, e.message))
        }
        "timetz" | "time with time zone" => {
            timetz::parse(value).map(|_| ()).map_err(|e| (e.sqlstate, e.message))
        }
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

fn ts_err(e: timestamp::TimestampError) -> ExecError {
    ExecError::new(e.sqlstate, e.message)
}

fn num(v: &Value) -> &Numeric {
    match v {
        Value::Numeric(n) => n,
        other => unreachable!("expected numeric arg, got {other:?}"),
    }
}

fn num_err(e: crabgresql_types::numeric::NumErr) -> ExecError {
    ExecError::new(e.sqlstate, e.message).with_detail(e.detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(f: ScalarFn, x: f64) -> f64 {
        match eval_scalar(f, &[Value::Float8(x)]).unwrap() {
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
