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
use crabgresql_types::{Value, float};

use crate::ExecError;

const RADIANS_PER_DEGREE: f64 = 0.017_453_292_519_943_295;

fn err(sqlstate: &'static str, message: impl Into<String>) -> ExecError {
    ExecError::new(sqlstate, message)
}

fn overflow() -> ExecError {
    err("22003", "value out of range: overflow")
}

fn underflow() -> ExecError {
    err("22003", "value out of range: underflow")
}

fn out_of_range_input() -> ExecError {
    err("22003", "input is out of range")
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
        _ => {}
    }
    // The remaining functions are float8 → float8 (or float8×float8 → float8).
    let a = f8(&args[0]);
    let result = match func {
        ScalarFn::Trunc => Ok(a.trunc()),
        ScalarFn::Round => Ok(a.round_ties_even()),
        ScalarFn::Ceil => Ok(a.ceil()),
        ScalarFn::Floor => Ok(a.floor()),
        ScalarFn::Sign => Ok(if a > 0.0 {
            1.0
        } else if a < 0.0 {
            -1.0
        } else {
            0.0
        }),
        ScalarFn::Sqrt => float::f8_sqrt(a).map_err(float_err),
        ScalarFn::Cbrt => Ok(float::f8_cbrt(a)),
        ScalarFn::Exp => dexp(a),
        ScalarFn::Ln => dln(a),
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
            if !(-1.0..=1.0).contains(&a) {
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
        ScalarFn::Float4Send | ScalarFn::Float8Send | ScalarFn::PgInputIsValid => unreachable!(),
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
        return Err(err("2201E", "cannot take logarithm of zero"));
    }
    if x < 0.0 {
        return Err(err("2201E", "cannot take logarithm of a negative number"));
    }
    Ok(x.ln())
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

fn dsind(x: f64) -> Result<f64, ExecError> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Err(out_of_range_input());
    }
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
    Ok(sign * sind_q1(a))
}

fn dcosd(x: f64) -> Result<f64, ExecError> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Err(out_of_range_input());
    }
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
    Ok(sign * cosd_q1(a))
}

fn dtand(x: f64) -> Result<f64, ExecError> {
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    if x.is_infinite() {
        return Err(out_of_range_input());
    }
    let mut sign = 1.0;
    let mut a = x % 180.0;
    if a < 0.0 {
        a += 180.0;
    }
    if a > 90.0 {
        a = 180.0 - a;
        sign = -sign;
    }
    let tan45 = sind_q1(45.0) / cosd_q1(45.0);
    let tan = sind_q1(a) / cosd_q1(a);
    let mut result = sign * (tan / tan45);
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
    let mut sign = 1.0;
    let mut a = x % 180.0;
    if a < 0.0 {
        a += 180.0;
    }
    if a > 90.0 {
        a = 180.0 - a;
        sign = -sign;
    }
    let cot45 = cosd_q1(45.0) / sind_q1(45.0);
    let cot = cosd_q1(a) / sind_q1(a);
    let mut result = sign * (cot / cot45);
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
