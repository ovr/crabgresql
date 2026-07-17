//! Float input parsing, PG-exact output formatting, and float arithmetic.
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior for `real`/`double precision` — the exact error conditions,
//! SQLSTATEs, and textual representation — as pinned by the `float4`/`float8`
//! regression corpus (compared byte-for-byte). It is implemented independently
//! from PG's documented behavior and IEEE-754 semantics, not translated from
//! PG source.

use std::cmp::Ordering;

/// SQLSTATE + message for an out-of-range / malformed float literal. The
/// message is dynamic (it embeds the offending text), so callers turn this
/// into their crate's error type.
#[derive(Clone, Debug, PartialEq)]
pub struct FloatParseError {
    pub sqlstate: &'static str,
    pub message: String,
}

/// SQLSTATE + static message for an arithmetic condition (overflow, division
/// by zero, domain error).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FloatError {
    pub sqlstate: &'static str,
    pub message: &'static str,
}

// SQLSTATE codes used here (mirrors crabgresql_protocol::sqlstate, kept as
// literals so this crate needs no protocol dependency).
const INVALID_TEXT_REPRESENTATION: &str = "22P02";
const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
const DIVISION_BY_ZERO: &str = "22012";
const INVALID_ARGUMENT_FOR_POWER: &str = "2201F";

fn overflow() -> FloatError {
    FloatError {
        sqlstate: NUMERIC_VALUE_OUT_OF_RANGE,
        message: "value out of range: overflow",
    }
}

fn underflow() -> FloatError {
    FloatError {
        sqlstate: NUMERIC_VALUE_OUT_OF_RANGE,
        message: "value out of range: underflow",
    }
}

// ---------------------------------------------------------------------------
// Input functions
// ---------------------------------------------------------------------------

/// Does the trimmed text spell an infinity (`inf` / `infinity`, optional sign)?
fn spells_infinity(trimmed: &str) -> bool {
    let core = trimmed
        .strip_prefix(['+', '-'])
        .unwrap_or(trimmed);
    core.eq_ignore_ascii_case("inf") || core.eq_ignore_ascii_case("infinity")
}

/// The significand contains a nonzero digit — used to tell a true zero (`0.0`,
/// `0e5`) from a value that underflowed to zero (`10e-400`). Only the mantissa
/// counts: a nonzero exponent on a zero mantissa (`0e5`) is still zero.
fn has_nonzero_digit(trimmed: &str) -> bool {
    let significand = trimmed.split(['e', 'E']).next().unwrap_or(trimmed);
    significand.bytes().any(|b| (b'1'..=b'9').contains(&b))
}

fn invalid_input(orig: &str, type_name: &str) -> FloatParseError {
    FloatParseError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("invalid input syntax for type {type_name}: \"{orig}\""),
    }
}

fn out_of_range(orig: &str, type_name: &str) -> FloatParseError {
    FloatParseError {
        sqlstate: NUMERIC_VALUE_OUT_OF_RANGE,
        message: format!("\"{orig}\" is out of range for type {type_name}"),
    }
}

/// `float8in`: parse text to f64 with PG's error semantics.
pub fn float8in(orig: &str) -> Result<f64, FloatParseError> {
    let trimmed = orig.trim_matches(|c: char| c.is_ascii_whitespace());
    let v: f64 = trimmed
        .parse()
        .map_err(|_| invalid_input(orig, "double precision"))?;
    if v.is_infinite() && !spells_infinity(trimmed) {
        return Err(out_of_range(orig, "double precision"));
    }
    if v == 0.0 && has_nonzero_digit(trimmed) {
        return Err(out_of_range(orig, "double precision"));
    }
    Ok(v)
}

/// `float4in`: parse text to f32 with PG's error semantics. Uses `f32::from_str`
/// directly so correctly-rounded results match strtof (the Paxson cases).
pub fn float4in(orig: &str) -> Result<f32, FloatParseError> {
    let trimmed = orig.trim_matches(|c: char| c.is_ascii_whitespace());
    let v: f32 = trimmed
        .parse()
        .map_err(|_| invalid_input(orig, "real"))?;
    if v.is_infinite() && !spells_infinity(trimmed) {
        return Err(out_of_range(orig, "real"));
    }
    if v == 0.0 && has_nonzero_digit(trimmed) {
        return Err(out_of_range(orig, "real"));
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// Output functions
// ---------------------------------------------------------------------------

/// `float8out` honoring `extra_float_digits` (efd).
pub fn fmt_f64(v: f64, efd: i32) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-Infinity" } else { "Infinity" }.into();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.into();
    }
    if efd >= 1 {
        let d = pg_ryu::shortest_f64(v.abs());
        // pg_ryu gives value = mantissa * 10^exponent; convert to leading-digit
        // form (digits, leading-digit power) that `render` expects.
        let digits = d.mantissa.to_string();
        let lead_exp = d.exponent + digits.len() as i32 - 1;
        let body = render(&digits, lead_exp, 15);
        if v.is_sign_negative() { format!("-{body}") } else { body }
    } else {
        let prec = (15 + efd).max(1) as usize;
        fmt_g(v, prec)
    }
}

/// `float4out` honoring `extra_float_digits` (efd).
pub fn fmt_f32(v: f32, efd: i32) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-Infinity" } else { "Infinity" }.into();
    }
    if efd >= 1 {
        if v == 0.0 {
            return if v.is_sign_negative() { "-0" } else { "0" }.into();
        }
        let d = pg_ryu::shortest_f32(v.abs());
        let digits = d.mantissa.to_string();
        let lead_exp = d.exponent + digits.len() as i32 - 1;
        let body = render(&digits, lead_exp, 6);
        if v.is_sign_negative() { format!("-{body}") } else { body }
    } else {
        let prec = (6 + efd).max(1) as usize;
        fmt_g(v as f64, prec)
    }
}

/// C `%.*g` formatting with `prec` significant digits (efd <= 0 path).
fn fmt_g(v: f64, prec: usize) -> String {
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let neg = v.is_sign_negative();
    // Rust rounds to `prec` significant digits (half-to-even), matching glibc.
    let esci = format!("{:.*e}", prec - 1, v.abs());
    let (mut digits, exp) = split_sci(&esci);
    // %g strips trailing zeros.
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    // %g uses scientific iff exp < -4 or exp >= precision.
    let body = if exp < -4 || exp >= prec as i32 {
        render_sci(&digits, exp)
    } else {
        render_fixed(&digits, exp)
    };
    if neg { format!("-{body}") } else { body }
}

/// Split `{:e}`/`{:.*e}` output ("1.0043e3", "5e-324") into significand digits
/// (dot removed) and the decimal exponent of the leading digit.
fn split_sci(esci: &str) -> (String, i32) {
    let (mantissa, exp_str) = esci.split_once('e').expect("scientific format");
    let exp: i32 = exp_str.parse().expect("valid exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    (digits, exp)
}

/// Shortest path: choose fixed vs scientific by PG's crossover.
fn render(digits: &str, exp: i32, fixed_hi: i32) -> String {
    if (-4..fixed_hi).contains(&exp) {
        render_fixed(digits, exp)
    } else {
        render_sci(digits, exp)
    }
}

/// Fixed-point rendering: `digits` are the significant digits, `exp` the power
/// of ten of the leading digit.
fn render_fixed(digits: &str, exp: i32) -> String {
    let ndigits = digits.len() as i32;
    if exp < 0 {
        // 0.00…digits
        let zeros = (-exp - 1) as usize;
        format!("0.{}{}", "0".repeat(zeros), digits)
    } else if exp >= ndigits - 1 {
        // digits followed by trailing zeros, no decimal point
        let zeros = (exp - (ndigits - 1)) as usize;
        format!("{digits}{}", "0".repeat(zeros))
    } else {
        // decimal point after exp+1 digits
        let split = (exp + 1) as usize;
        format!("{}.{}", &digits[..split], &digits[split..])
    }
}

/// Scientific rendering: `d[.ddd]e±XX`, exponent zero-padded to >= 2 digits.
fn render_sci(digits: &str, exp: i32) -> String {
    let mantissa = if digits.len() == 1 {
        digits.to_string()
    } else {
        format!("{}.{}", &digits[..1], &digits[1..])
    };
    let sign = if exp < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:02}", exp.abs())
}

// ---------------------------------------------------------------------------
// Arithmetic — reproduces PG's observable float overflow/underflow/divide
// and power/sqrt error behavior (SQLSTATE + message), pinned by the corpus.
// ---------------------------------------------------------------------------

macro_rules! float_ops {
    ($add:ident, $sub:ident, $mul:ident, $div:ident, $cmp:ident, $t:ty) => {
        pub fn $add(a: $t, b: $t) -> Result<$t, FloatError> {
            let r = a + b;
            if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
                return Err(overflow());
            }
            Ok(r)
        }
        pub fn $sub(a: $t, b: $t) -> Result<$t, FloatError> {
            let r = a - b;
            if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
                return Err(overflow());
            }
            Ok(r)
        }
        pub fn $mul(a: $t, b: $t) -> Result<$t, FloatError> {
            let r = a * b;
            if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
                return Err(overflow());
            }
            if r == 0.0 && a != 0.0 && b != 0.0 {
                return Err(underflow());
            }
            Ok(r)
        }
        pub fn $div(a: $t, b: $t) -> Result<$t, FloatError> {
            if b == 0.0 && !a.is_nan() {
                return Err(FloatError {
                    sqlstate: DIVISION_BY_ZERO,
                    message: "division by zero",
                });
            }
            let r = a / b;
            if r.is_infinite() && !a.is_infinite() && !b.is_infinite() {
                return Err(overflow());
            }
            if r == 0.0 && a != 0.0 && !b.is_infinite() {
                return Err(underflow());
            }
            Ok(r)
        }
        pub fn $cmp(a: $t, b: $t) -> Ordering {
            if a.is_nan() {
                if b.is_nan() { Ordering::Equal } else { Ordering::Greater }
            } else if b.is_nan() {
                Ordering::Less
            } else {
                a.partial_cmp(&b).unwrap()
            }
        }
    };
}

float_ops!(f8_add, f8_sub, f8_mul, f8_div, f8_cmp, f64);
float_ops!(f4_add, f4_sub, f4_mul, f4_div, f4_cmp, f32);

/// float8 `power()` reproducing PG's observable error semantics. Relies on
/// IEEE `powf` for the special inf/nan results (PG's `power` shows the same
/// IEEE outcomes there), with PG's extra domain/overflow/underflow errors.
pub fn f8_pow(a: f64, b: f64) -> Result<f64, FloatError> {
    if a == 0.0 && b < 0.0 {
        return Err(FloatError {
            sqlstate: INVALID_ARGUMENT_FOR_POWER,
            message: "zero raised to a negative power is undefined",
        });
    }
    if a < 0.0 && b.is_finite() && b.floor() != b {
        return Err(FloatError {
            sqlstate: INVALID_ARGUMENT_FOR_POWER,
            message: "a negative number raised to a non-integer power yields a complex result",
        });
    }
    let r = a.powf(b);
    if r.is_infinite() {
        if !a.is_infinite() && !b.is_infinite() {
            return Err(overflow());
        }
    } else if r == 0.0 && a != 0.0 && !a.is_infinite() && !b.is_infinite() {
        return Err(underflow());
    }
    Ok(r)
}

/// `sqrt()`: erroring on a negative argument as PG does.
pub fn f8_sqrt(a: f64) -> Result<f64, FloatError> {
    if a < 0.0 {
        return Err(FloatError {
            sqlstate: INVALID_ARGUMENT_FOR_POWER,
            message: "cannot take square root of a negative number",
        });
    }
    Ok(a.sqrt())
}

/// `cbrt()`: cube root (no error conditions).
pub fn f8_cbrt(a: f64) -> f64 {
    a.cbrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_malformed() {
        for bad in ["", "   ", "xyz", "5.0.0", "5 . 0", "5.   0", "- 3.0", "N A N", "NaN x", "1 5"]
        {
            let e = float8in(bad).unwrap_err();
            assert_eq!(e.sqlstate, "22P02", "for {bad:?}");
        }
    }

    #[test]
    fn parse_accepts_specials_and_whitespace() {
        assert!(float8in("  NaN ").unwrap().is_nan());
        assert_eq!(float8in("infinity").unwrap(), f64::INFINITY);
        assert_eq!(float8in("          -INFINiTY   ").unwrap(), f64::NEG_INFINITY);
        assert_eq!(float8in("    0.0   ").unwrap(), 0.0);
        assert_eq!(float8in("1004.30  ").unwrap(), 1004.3);
    }

    #[test]
    fn parse_range_errors() {
        assert_eq!(float4in("10e70").unwrap_err().sqlstate, "22003");
        assert_eq!(float4in("10e-70").unwrap_err().sqlstate, "22003");
        assert_eq!(float8in("10e400").unwrap_err().sqlstate, "22003");
        assert_eq!(float8in("10e-400").unwrap_err().sqlstate, "22003");
        // float8 tolerates what float4 cannot.
        assert!(float8in("10e-70").is_ok());
        assert!(float8in("1.2345678901234e-200").is_ok());
    }

    #[test]
    fn zero_mantissa_with_exponent_is_not_underflow() {
        // A zero mantissa is exactly zero regardless of the exponent; only a
        // nonzero value rounding to zero is an underflow error.
        assert_eq!(float8in("0e5").unwrap(), 0.0);
        assert_eq!(float8in("0e-400").unwrap(), 0.0);
        assert_eq!(float8in("0.0e12").unwrap(), 0.0);
        assert_eq!(float8in("10e-400").unwrap_err().sqlstate, "22003");
    }

    #[test]
    fn fmt_shortest_examples() {
        assert_eq!(fmt_f64(0.0, 1), "0");
        assert_eq!(fmt_f64(-0.0, 1), "-0");
        assert_eq!(fmt_f64(1004.3, 1), "1004.3");
        assert_eq!(fmt_f64(-34.84, 1), "-34.84");
        assert_eq!(fmt_f64(0.0001, 1), "0.0001");
        assert_eq!(fmt_f64(1e15, 1), "1e+15");
        assert_eq!(fmt_f64(1.2345678901234e+200, 1), "1.2345678901234e+200");
        assert_eq!(fmt_f64(f64::INFINITY, 1), "Infinity");
        assert_eq!(fmt_f64(f64::NEG_INFINITY, 1), "-Infinity");
        assert_eq!(fmt_f64(f64::NAN, 1), "NaN");
        assert_eq!(fmt_f32(1e6, 1), "1e+06");
        assert_eq!(fmt_f32(999999.94, 1), "999999.94");
    }

    #[test]
    fn fmt_g_examples() {
        assert_eq!(fmt_f64(8.0, 0), "8");
        assert_eq!(fmt_f64(1004.3f64.sqrt(), 0), "31.6906926399535");
        assert_eq!(fmt_f64(-0.0, 0), "-0");
    }

    #[test]
    fn div_by_zero_and_nan() {
        assert_eq!(f8_div(1.0, 0.0).unwrap_err().sqlstate, "22012");
        assert!(f8_div(f64::NAN, 0.0).unwrap().is_nan());
        assert_eq!(f8_div(42.0, f64::INFINITY).unwrap(), 0.0);
    }

    #[test]
    fn pow_edge_cases() {
        assert_eq!(f8_pow(f64::NAN, 0.0).unwrap(), 1.0);
        assert_eq!(f8_pow(1.0, f64::NAN).unwrap(), 1.0);
        assert!(f8_pow(f64::NAN, f64::NAN).unwrap().is_nan());
        assert_eq!(f8_pow(0.0, -1.0).unwrap_err().sqlstate, "2201F");
        assert_eq!(f8_pow(-1.0, 0.5).unwrap_err().sqlstate, "2201F");
        assert!(f8_pow(f64::NEG_INFINITY, -3.0).unwrap().is_sign_negative());
        assert_eq!(f8_pow(f64::NEG_INFINITY, -3.0).unwrap(), 0.0);
        assert_eq!(f8_pow(1004.3, 1e200).unwrap_err().message, "value out of range: overflow");
    }

    #[test]
    fn cmp_nan_is_greatest() {
        assert_eq!(f8_cmp(f64::NAN, f64::NAN), Ordering::Equal);
        assert_eq!(f8_cmp(f64::NAN, 1.0), Ordering::Greater);
        assert_eq!(f8_cmp(1.0, f64::NAN), Ordering::Less);
        assert_eq!(f8_cmp(-0.0, 0.0), Ordering::Equal);
    }
}
