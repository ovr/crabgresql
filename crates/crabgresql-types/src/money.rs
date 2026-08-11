//! `money` (a.k.a. `cash`): parsing, formatting, comparison, arithmetic, and the
//! `cash_words` spelling function.
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable* money
//! behavior — the `$`/comma/`-$` output format, the `cash_in` acceptor, the
//! English `cash_words` text, and the SQLSTATE/message of range and syntax
//! errors — pinned by the `money` regression test, implemented independently.
//!
//! Representation: a signed count of hundredths ("cents") in an `i64`, assuming
//! `lc_monetary = C` (a `$` symbol, `,` thousands grouping, `.` decimal, 2
//! fractional digits). The value range is exactly `i64`:
//! `-92233720368547758.08` (`i64::MIN` cents) .. `92233720368547758.07`
//! (`i64::MAX` cents).
//!
//! TODO: honor `lc_monetary`; the `$`/`,`/`.` conventions are hardcoded to the
//! `C` locale, which is the locale the upstream `money` regression test assumes.

use std::cmp::Ordering;

// SQLSTATEs (kept as literals; the types crate does not depend on the protocol
// crate — the binder/executor map these to `sqlstate::*`).
const INVALID_TEXT_REPRESENTATION: &str = "22P02";
const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
const DIVISION_BY_ZERO: &str = "22012";

/// A parse/range/arithmetic error, carrying the SQLSTATE and message PG reports.
#[derive(Clone, Debug, PartialEq)]
pub struct MoneyError {
    pub sqlstate: &'static str,
    pub message: String,
}

fn invalid_syntax(input: &str) -> MoneyError {
    MoneyError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("invalid input syntax for type money: \"{input}\""),
    }
}

/// `22003` on the input path, which prints the offending literal.
fn value_out_of_range(input: &str) -> MoneyError {
    MoneyError {
        sqlstate: NUMERIC_VALUE_OUT_OF_RANGE,
        message: format!("value \"{input}\" is out of range for type money"),
    }
}

/// `22003` for arithmetic overflow — the bare message PG uses at run time.
fn out_of_range() -> MoneyError {
    MoneyError {
        sqlstate: NUMERIC_VALUE_OUT_OF_RANGE,
        message: "money out of range".to_string(),
    }
}

fn division_by_zero() -> MoneyError {
    MoneyError {
        sqlstate: DIVISION_BY_ZERO,
        message: "division by zero".to_string(),
    }
}

/// `cash_in`: parse money input text into a count of hundredths.
///
/// Accepts an optional `$`, `,` thousands separators, and a single sign
/// indicator that is either leading (`-`/`+`), trailing (`-`/`+`), or PG's
/// accounting parentheses where a leading `(` denotes a negative amount and the
/// closing `)` is optional (`(1)` and `(1` both = -$1.00). As in PG, a sign
/// ahead of the digits inside the parentheses, or a doubled leading sign, is an
/// error (`(-1)`, `--1`, `+-1` all reject). Two fractional digits are kept; a
/// third digit `>= '5'` rounds the magnitude away from zero (matching `cash_in`,
/// which inspects only the next digit — so `92233720368547758.075` rounds up
/// into overflow).
///
/// TODO: accept the repeated sign indicators PG tolerates — a trailing sign
/// after a leading one (`-1-` and `(1-)` both = -$1.00) and an unmatched
/// closing paren (`1)` = $1.00, `(1))` = -$1.00) — which are rejected here as
/// `22P02`.
pub fn parse(input: &str) -> Result<i64, MoneyError> {
    let mut s = input.trim();
    let mut neg = false;
    // At most one sign indicator total; `paren` also allows a trailing `)`.
    let mut sign_seen = false;
    let mut paren = false;

    if let Some(rest) = s.strip_prefix('(') {
        neg = true;
        sign_seen = true;
        paren = true;
        s = rest.trim_start();
    } else if let Some(rest) = s.strip_prefix('-') {
        neg = true;
        sign_seen = true;
        s = rest.trim_start();
    } else if let Some(rest) = s.strip_prefix('+') {
        sign_seen = true;
        s = rest.trim_start();
    }
    // The currency symbol follows a leading sign, before the digits.
    if let Some(rest) = s.strip_prefix('$') {
        s = rest.trim_start();
    }

    // Accumulate the magnitude in hundredths using i128, so a value that only
    // overflows i64 (the documented ±.07/.08 boundary) is still parsed exactly
    // before the final range check. Stop at the first non-numeric character; the
    // remainder is the trailing sign / closing-paren section.
    let mut mag: i128 = 0;
    let mut seen_digit = false;
    let mut in_frac = false;
    let mut frac_digits = 0u32;
    let mut round_up = false;
    let mut trailing = "";

    for (i, c) in s.char_indices() {
        match c {
            '0'..='9' => {
                seen_digit = true;
                let d = (c as u8 - b'0') as i128;
                if in_frac {
                    match frac_digits {
                        0 | 1 => {
                            mag = mag
                                .checked_mul(10)
                                .and_then(|m| m.checked_add(d))
                                .ok_or_else(|| value_out_of_range(input))?;
                            frac_digits += 1;
                        }
                        // The first digit past the two kept places decides the
                        // round; later digits are ignored, as cash_in does.
                        2 => {
                            if d >= 5 {
                                round_up = true;
                            }
                            frac_digits += 1;
                        }
                        _ => {}
                    }
                } else {
                    mag = mag
                        .checked_mul(10)
                        .and_then(|m| m.checked_add(d))
                        .ok_or_else(|| value_out_of_range(input))?;
                }
            }
            // Thousands separators are accepted and ignored.
            ',' => {}
            '.' => {
                if in_frac {
                    return Err(invalid_syntax(input));
                }
                in_frac = true;
            }
            _ => {
                trailing = &s[i..];
                break;
            }
        }
    }

    if !seen_digit {
        return Err(invalid_syntax(input));
    }

    // A trailing sign or closing paren — at most one, and only after no leading
    // sign (except a `)` that closes a leading `(`).
    match trailing.trim() {
        "" => {}
        ")" if paren => {}
        "-" if !sign_seen => neg = true,
        "+" if !sign_seen => {}
        _ => return Err(invalid_syntax(input)),
    }

    // Scale up to exactly two fractional places, then apply the rounding.
    while frac_digits < 2 {
        mag = mag
            .checked_mul(10)
            .ok_or_else(|| value_out_of_range(input))?;
        frac_digits += 1;
    }
    if round_up {
        mag += 1;
    }
    if neg {
        mag = -mag;
    }
    i64::try_from(mag).map_err(|_| value_out_of_range(input))
}

/// `cash_out`: `$`, comma thousands grouping, always two fractional digits, and
/// a leading `-$` for negatives (e.g. `-$1,234.56`).
pub fn format(cents: i64) -> String {
    // Work on the magnitude in i128 so `i64::MIN` negates cleanly.
    let mag = (cents as i128).unsigned_abs();
    let dollars = mag / 100;
    let frac = mag % 100;
    let sign = if cents < 0 { "-" } else { "" };
    format!("{sign}${}.{frac:02}", group_thousands(dollars))
}

/// Render an unsigned integer with `,` inserted every three digits.
fn group_thousands(mut n: u128) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let mut groups = Vec::new();
    while n > 0 {
        groups.push((n % 1000) as u16);
        n /= 1000;
    }
    let mut out = String::new();
    for (i, g) in groups.iter().rev().enumerate() {
        if i == 0 {
            out.push_str(&g.to_string());
        } else {
            out.push(',');
            out.push_str(&format!("{g:03}"));
        }
    }
    out
}

/// Compare two money values; the natural `i64` order is correct.
pub fn cmp(a: i64, b: i64) -> Ordering {
    a.cmp(&b)
}

pub fn larger(a: i64, b: i64) -> i64 {
    a.max(b)
}

pub fn smaller(a: i64, b: i64) -> i64 {
    a.min(b)
}

// ---- arithmetic -----------------------------------------------------------

/// Unary minus (`cash_um`): negate, erroring on `i64::MIN` which has no positive.
pub fn neg(a: i64) -> Result<i64, MoneyError> {
    a.checked_neg().ok_or_else(out_of_range)
}

pub fn add(a: i64, b: i64) -> Result<i64, MoneyError> {
    a.checked_add(b).ok_or_else(out_of_range)
}

pub fn sub(a: i64, b: i64) -> Result<i64, MoneyError> {
    a.checked_sub(b).ok_or_else(out_of_range)
}

/// `money * int` / `int * money`: exact integer scaling (the int operand has
/// been widened to i64 by the binder).
pub fn mul_int(cents: i64, factor: i64) -> Result<i64, MoneyError> {
    cents.checked_mul(factor).ok_or_else(out_of_range)
}

/// `money * float`: `rint(cents * f)` (round half to even, as C `rint`), with a
/// range check; a NaN/infinite product is out of range.
pub fn mul_float(cents: i64, factor: f64) -> Result<i64, MoneyError> {
    float_to_cents((cents as f64) * factor)
}

/// `money / int`: integer division truncating toward zero (matches `cash_div_*`,
/// which do C integer division and drop the remainder).
pub fn div_int(cents: i64, divisor: i64) -> Result<i64, MoneyError> {
    if divisor == 0 {
        return Err(division_by_zero());
    }
    cents.checked_div(divisor).ok_or_else(out_of_range)
}

/// `money / float`: `rint(cents / f)` with a range check; a zero divisor is a
/// division-by-zero error (not an overflow).
pub fn div_float(cents: i64, divisor: f64) -> Result<i64, MoneyError> {
    if divisor == 0.0 {
        return Err(division_by_zero());
    }
    float_to_cents((cents as f64) / divisor)
}

/// `money / money -> double precision`: the ratio of the two amounts.
pub fn div_cash(a: i64, b: i64) -> Result<f64, MoneyError> {
    if b == 0 {
        return Err(division_by_zero());
    }
    Ok((a as f64) / (b as f64))
}

/// Round a float product/quotient to the nearest cent (`rint`) and range-check
/// it into `i64`. NaN and infinities have no money image → out of range.
fn float_to_cents(v: f64) -> Result<i64, MoneyError> {
    let r = v.round_ties_even();
    if !r.is_finite() || r < i64::MIN as f64 || r >= 9_223_372_036_854_775_808.0 {
        return Err(out_of_range());
    }
    Ok(r as i64)
}

// ---- cash_words -----------------------------------------------------------

const ONES: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];
const TENS: [&str; 10] = [
    "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];
// Scale names for successive groups of three digits. `i64` dollars reach ~9.2e16
// (ten-quadrillions), so quadrillion suffices; quintillion is kept for headroom.
const SCALES: [&str; 7] = [
    "",
    " thousand",
    " million",
    " billion",
    " trillion",
    " quadrillion",
    " quintillion",
];

/// `cash_words`: spell an amount as English text, e.g.
/// `One hundred twenty three dollars and zero cents`. American style (no "and"
/// inside a number, spaces not hyphens), with the first letter capitalized and
/// singular/plural `dollar`/`cent` agreement.
pub fn words(cents: i64) -> String {
    let mag = (cents as i128).unsigned_abs();
    let dollars = (mag / 100) as u64;
    let cents_part = (mag % 100) as u64;

    let mut s = String::new();
    if cents < 0 {
        s.push_str("minus ");
    }
    s.push_str(&spell(dollars));
    s.push_str(if dollars == 1 {
        " dollar and "
    } else {
        " dollars and "
    });
    s.push_str(&spell(cents_part));
    s.push_str(if cents_part == 1 { " cent" } else { " cents" });

    // cash_words output has its first character capitalized.
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => s,
    }
}

/// Spell a non-negative integer in words (lowercase, no leading/trailing space).
fn spell(n: u64) -> String {
    if n == 0 {
        return ONES[0].to_string();
    }
    // Split into groups of three digits, least-significant first.
    let mut groups = Vec::new();
    let mut rem = n;
    while rem > 0 {
        groups.push((rem % 1000) as u16);
        rem /= 1000;
    }
    let mut parts: Vec<String> = Vec::new();
    for (i, g) in groups.iter().enumerate().rev() {
        if *g == 0 {
            continue;
        }
        parts.push(format!("{}{}", spell_three(*g), SCALES[i]));
    }
    parts.join(" ")
}

/// Spell 1..=999 (caller handles zero).
fn spell_three(n: u16) -> String {
    let hundreds = n / 100;
    let rem = n % 100;
    let mut parts: Vec<String> = Vec::new();
    if hundreds > 0 {
        parts.push(format!("{} hundred", ONES[hundreds as usize]));
    }
    if rem > 0 {
        if rem < 20 {
            parts.push(ONES[rem as usize].to_string());
        } else {
            let tens = rem / 10;
            let ones = rem % 10;
            if ones == 0 {
                parts.push(TENS[tens as usize].to_string());
            } else {
                parts.push(format!("{} {}", TENS[tens as usize], ONES[ones as usize]));
            }
        }
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_currency() {
        assert_eq!(parse("123"), Ok(12300));
        assert_eq!(parse("123.45"), Ok(12345));
        assert_eq!(parse("$123.45"), Ok(12345));
        assert_eq!(parse("  $1,234.56 "), Ok(123456));
    }

    #[test]
    fn parentheses_and_signs_are_negative() {
        assert_eq!(parse("(1)"), Ok(-100));
        assert_eq!(parse("($123,456.78)"), Ok(-12345678));
        assert_eq!(parse("-12345"), Ok(-1234500));
        // A leading `(` is the negative flag; the closing `)` is optional (PG).
        assert_eq!(parse("(1"), Ok(-100));
        // Trailing accounting sign.
        assert_eq!(parse("1-"), Ok(-100));
        assert_eq!(parse("1+"), Ok(100));
    }

    #[test]
    fn rejects_multiple_or_nested_signs() {
        // A sign ahead of the digits inside parentheses, or a doubled leading
        // sign, is an error in PG rather than a silently cancelled/duplicated
        // sign. `-1-` and `1)` are stricter than PG, which reads them as -$1.00
        // and $1.00 (see the TODO on `parse`).
        for bad in ["(-1)", "(+1)", "--1", "+-1", "-$-1", "-1-", "1)"] {
            assert_eq!(
                parse(bad)
                    .expect_err("a nested or duplicated sign indicator is not money")
                    .sqlstate,
                "22P02",
                "input {bad:?}"
            );
        }
    }

    #[test]
    fn rounds_third_fractional_digit_away_from_zero() {
        assert_eq!(parse("$123.451"), Ok(12345));
        assert_eq!(parse("$123.454"), Ok(12345));
        assert_eq!(parse("$123.455"), Ok(12346));
        assert_eq!(parse("$123.459"), Ok(12346));
    }

    #[test]
    fn documented_min_max_boundary() {
        assert_eq!(parse("92233720368547758.07"), Ok(i64::MAX));
        assert_eq!(parse("-92233720368547758.08"), Ok(i64::MIN));
        assert_eq!(
            parse("92233720368547758.08")
                .expect_err("one cent above the money maximum is out of range")
                .sqlstate,
            "22003"
        );
        assert_eq!(
            parse("-92233720368547758.09")
                .expect_err("one cent below the money minimum is out of range")
                .sqlstate,
            "22003"
        );
        // Rounding into overflow.
        assert_eq!(
            parse("92233720368547758.075")
                .expect_err("a third digit that rounds up past the money maximum is out of range")
                .sqlstate,
            "22003"
        );
        assert_eq!(
            parse("-92233720368547758.085")
                .expect_err("a third digit that rounds down past the money minimum is out of range")
                .sqlstate,
            "22003"
        );
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(
            parse("\\x0001")
                .expect_err("a backslash-hex byte string is not money")
                .sqlstate,
            "22P02"
        );
        assert_eq!(
            parse("")
                .expect_err("an empty string is not money")
                .sqlstate,
            "22P02"
        );
    }

    #[test]
    fn formats_with_grouping_and_sign() {
        assert_eq!(format(12300), "$123.00");
        assert_eq!(format(123456789000), "$1,234,567,890.00");
        assert_eq!(format(-45), "-$0.45");
        assert_eq!(format(-100), "-$1.00");
        assert_eq!(format(i64::MIN), "-$92,233,720,368,547,758.08");
        assert_eq!(format(i64::MAX), "$92,233,720,368,547,758.07");
    }

    #[test]
    fn words_match_spec() {
        assert_eq!(
            words(12300),
            "One hundred twenty three dollars and zero cents"
        );
        assert_eq!(
            words(12423),
            "One hundred twenty four dollars and twenty three cents"
        );
        assert_eq!(words(100), "One dollar and zero cents");
        assert_eq!(words(1), "Zero dollars and one cent");
        assert_eq!(words(0), "Zero dollars and zero cents");
    }

    #[test]
    fn division_semantics() {
        // int division truncates; float division rounds.
        assert_eq!(div_int(87808, 11), Ok(7982));
        assert_eq!(div_float(87808, 11.0), Ok(7983));
        assert_eq!(div_cash(12300, 200), Ok(61.5));
        assert_eq!(
            div_int(1, 0)
                .expect_err("dividing money by an integer zero is rejected")
                .sqlstate,
            "22012"
        );
    }

    #[test]
    fn unary_minus() {
        assert_eq!(neg(500), Ok(-500));
        assert_eq!(neg(-500), Ok(500));
        // i64::MIN has no positive money image.
        assert_eq!(
            neg(i64::MIN)
                .expect_err("the money minimum has no positive image")
                .message,
            "money out of range"
        );
    }

    #[test]
    fn overflow_arithmetic() {
        assert_eq!(
            add(i64::MAX, 1)
                .expect_err("adding one cent to the money maximum overflows")
                .message,
            "money out of range"
        );
        assert_eq!(
            mul_float(4200, f64::NAN)
                .expect_err("multiplying money by NaN has no money result")
                .message,
            "money out of range"
        );
        assert_eq!(
            mul_float(4200, f64::INFINITY)
                .expect_err("multiplying money by infinity has no money result")
                .message,
            "money out of range"
        );
    }
}
