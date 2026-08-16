//! Arbitrary-precision `numeric` (a.k.a. `decimal`).
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! `numeric` behavior — text output, display scale, comparison order, and the
//! error text/SQLSTATE of the operations the regression corpus exercises —
//! implemented independently from the documented algorithm, not ported from C.
//!
//! Representation. A value is a sign plus a base-10 coefficient (`digits`,
//! most-significant first, each `0..=9`, no leading or trailing zeros) with a
//! decimal `weight` (the power of ten of `digits[0]`) and a display scale
//! `dscale` (how many fractional digits are shown). The numeric value is
//! `sign * Σ digits[i] * 10^(weight - i)`; zero has an empty digit vector.
//! `dscale` is display-only: it can exceed the number of stored fractional
//! digits (trailing zeros) and never affects the value, only its rendering.
//!
//! PostgreSQL stores digits base-`NBASE` (10000); we keep base-10 because the
//! display-scale rules (which are specified in decimal) fall out directly, and
//! the one place the base-10000 grouping is observable — division's result
//! scale — is reproduced via [`Numeric::nbase_weight`]/[`Numeric::first_nbase`].

/// Largest display scale PG stores (`0..=16383`); larger input overflows.
const MAX_DSCALE: i64 = 16383;
/// Largest base-10000 weight PG stores; larger input overflows. A decimal
/// weight `w` maps to base-10000 weight `floor(w/4)`, so this bounds the
/// integer magnitude at ~131072 digits.
const MAX_NBASE_WEIGHT: i64 = 0x7FFF;
/// Division/transcendental "give at least this many significant digits" floor:
/// PG prints `10::numeric / 3` as `3.3333333333333333` and `ln(2)` as
/// `0.6931471805599453` (see `division_scale_matches_pg`, `ln_matches_pg`).
const MIN_SIG_DIGITS: i32 = 16;
/// Ceiling on the display scale exp/ln/log/power pick for themselves. PG stops
/// at 1000 fractional digits there, well short of [`MAX_DSCALE`]: `exp(-5000)`
/// prints 1000 zeros, `exp(-2000)` the 884 its own estimate asks for.
const MAX_RESULT_SCALE: i32 = 1000;
/// Largest decimal weight a stored value can have, from [`MAX_NBASE_WEIGHT`];
/// a result whose weight exceeds this overflows the numeric format.
const MAX_DECIMAL_WEIGHT: f64 = ((MAX_NBASE_WEIGHT + 1) * 4) as f64;

/// `10^i` for every `i` a 128-bit decimal can shift by, so scaling by the
/// stored digits' distance from the point is one multiply rather than a loop.
const POW10_I128: [i128; 39] = {
    let mut table = [1i128; 39];
    let mut i = 1;
    while i < 39 {
        table[i] = table[i - 1] * 10;
        i += 1;
    }
    table
};

/// `10^i` for every `i` a `u64` holds, so the common fold stays in registers.
const POW10_U64: [u64; 20] = {
    let mut table = [1u64; 20];
    let mut i = 1;
    while i < 20 {
        table[i] = table[i - 1] * 10;
        i += 1;
    }
    table
};

/// `10^i` as `u128`, for the steps that pull an over-wide magnitude down into a
/// register.
const POW10_U128: [u128; 39] = {
    let mut table = [1u128; 39];
    let mut i = 1;
    while i < 39 {
        table[i] = table[i - 1] * 10;
        i += 1;
    }
    table
};

/// The base-10 digits of a **non-zero** magnitude, most-significant first, as a
/// canonical [`Numeric`] whose least-significant digit sits at `low`.
///
/// A macro rather than a generic so each width divides by its own type:
/// dividing a `u64` by the literal 10 compiles to a multiply, while the same
/// division on a `u128` is a call to `__udivti3`.
///
/// Trailing zeros are stripped first — a scaled decimal is mostly padding
/// (`1.5` at scale 16 is `15000000000000000`), and extracting those zeros only
/// to drop them again is a wasted pass.
macro_rules! digits_of {
    ($mag:expr, $cap:expr, $neg:expr, $low:expr, $dscale:expr) => {{
        let mut mag = $mag;
        let mut low = $low;
        debug_assert!(mag > 0, "digits_of on zero would not terminate");
        while mag % 10 == 0 {
            mag /= 10;
            low += 1;
        }
        let mut digits = [0u8; $cap];
        let mut len = 0;
        while mag > 0 {
            digits[len] = (mag % 10) as u8;
            mag /= 10;
            len += 1;
        }
        digits[..len].reverse();
        Numeric::from_canonical($neg, digits[..len].to_vec(), low, $dscale)
    }};
}

#[derive(deepsize::DeepSizeOf, Clone, Copy, Debug, PartialEq, Eq)]
enum Sign {
    Pos,
    Neg,
    NaN,
    PInf,
    NInf,
}

/// A `numeric` value. See the module docs for the representation invariant.
#[derive(deepsize::DeepSizeOf, Clone, Debug)]
pub struct Numeric {
    sign: Sign,
    /// Decimal weight of `digits[0]`; irrelevant (kept 0) when `digits` empty.
    weight: i32,
    /// Display scale: number of fractional digits shown, `>= 0`.
    dscale: i32,
    /// Base-10 coefficient, most-significant first, each `0..=9`, canonicalized
    /// to no leading/trailing zeros. Empty means the value is zero.
    digits: Vec<u8>,
}

/// Why [`Numeric::parse`] rejected its input; the caller turns this into the
/// SQLSTATE/message PG uses (`22P02` for syntax, `22003` for magnitude).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    Syntax,
    Overflow,
}

/// An arithmetic error (SQLSTATE + message, with an optional DETAIL line for
/// `numeric field overflow`). Boundary layers map this onto their own error
/// types.
#[derive(Clone, Debug, PartialEq)]
pub struct NumErr {
    pub sqlstate: &'static str,
    pub message: String,
    pub detail: Option<String>,
}

impl NumErr {
    fn new(sqlstate: &'static str, message: impl Into<String>) -> NumErr {
        NumErr {
            sqlstate,
            message: message.into(),
            detail: None,
        }
    }
}

fn floor_div(a: i64, b: i64) -> i64 {
    a.div_euclid(b)
}

/// The surrounding whitespace [`Numeric::parse`] skips: space, tab, newline and
/// carriage return. Deliberately **not** [`str::trim_ascii`], which also strips
/// a form feed — `numeric_in` rejects `'\x0c1'`, and that is observable.
#[inline]
fn trim_numeric_space(s: &str) -> &str {
    const fn is_space(c: u8) -> bool {
        matches!(c, b' ' | b'\t' | b'\n' | b'\r')
    }
    let b = s.as_bytes();
    let mut start = 0;
    while start < b.len() && is_space(b[start]) {
        start += 1;
    }
    let mut end = b.len();
    while end > start && is_space(b[end - 1]) {
        end -= 1;
    }
    &s[start..end]
}

impl Numeric {
    // ---- constructors -----------------------------------------------------

    pub fn nan() -> Numeric {
        Numeric {
            sign: Sign::NaN,
            weight: 0,
            dscale: 0,
            digits: Vec::new(),
        }
    }
    pub fn pos_inf() -> Numeric {
        Numeric {
            sign: Sign::PInf,
            weight: 0,
            dscale: 0,
            digits: Vec::new(),
        }
    }
    pub fn neg_inf() -> Numeric {
        Numeric {
            sign: Sign::NInf,
            weight: 0,
            dscale: 0,
            digits: Vec::new(),
        }
    }
    /// Zero with the given display scale.
    fn zero(dscale: i32) -> Numeric {
        Numeric {
            sign: Sign::Pos,
            weight: 0,
            dscale: dscale.max(0),
            digits: Vec::new(),
        }
    }

    /// The display scale: how many fractional digits this value prints with.
    /// Part of the *value* in PostgreSQL, not of the type.
    pub fn display_scale(&self) -> i32 {
        self.dscale
    }

    pub fn is_nan(&self) -> bool {
        self.sign == Sign::NaN
    }
    pub fn is_infinite(&self) -> bool {
        matches!(self.sign, Sign::PInf | Sign::NInf)
    }
    fn is_special(&self) -> bool {
        !matches!(self.sign, Sign::Pos | Sign::Neg)
    }
    fn is_zero(&self) -> bool {
        !self.is_special() && self.digits.is_empty()
    }
    fn is_neg(&self) -> bool {
        matches!(self.sign, Sign::Neg | Sign::NInf)
    }

    /// Build a canonical value from a base-10 coefficient (`digits`, MSB-first,
    /// possibly with leading/trailing zeros) whose least-significant digit sits
    /// at decimal position `low`. Strips zeros and collapses to a signed zero.
    fn from_coeff(neg: bool, digits: Vec<u8>, low: i32, dscale: i32) -> Numeric {
        let weight = low + digits.len() as i32 - 1;
        let mut n = Numeric {
            sign: if neg { Sign::Neg } else { Sign::Pos },
            weight,
            dscale: dscale.max(0),
            digits,
        };
        n.normalize();
        n
    }

    /// Build from digits already known to be canonical: non-empty, no leading
    /// or trailing zero. Skips [`Numeric::normalize`]'s two passes, which for
    /// digits extracted out of an integer can only confirm what the extraction
    /// guaranteed.
    fn from_canonical(neg: bool, digits: Vec<u8>, low: i32, dscale: i32) -> Numeric {
        debug_assert!(
            digits.first().is_some_and(|d| *d != 0) && digits.last().is_some_and(|d| *d != 0),
            "from_canonical was handed a coefficient normalize would have changed"
        );
        Numeric {
            sign: if neg { Sign::Neg } else { Sign::Pos },
            weight: low + digits.len() as i32 - 1,
            dscale: dscale.max(0),
            digits,
        }
    }

    /// Exact conversion from a signed 128-bit integer (used by int→numeric).
    ///
    /// An integer is a fixed-point value of scale 0, so this is
    /// [`Numeric::from_scaled_i128`] with nothing after the point rather than a
    /// second digit loop that would have to agree with it by hand.
    pub fn from_i128(v: i128) -> Numeric {
        Numeric::from_scaled_i128(v, 0)
    }

    /// `float8_numeric`/`float4_numeric`: render `v` with `sig` significant
    /// decimal digits (plain, trailing zeros dropped) and take it as numeric.
    /// NaN/±Infinity carry through.
    pub fn from_f64_sig(v: f64, sig: usize) -> Numeric {
        if v.is_nan() {
            return Numeric::nan();
        }
        if v.is_infinite() {
            return if v < 0.0 {
                Numeric::neg_inf()
            } else {
                Numeric::pos_inf()
            };
        }
        if v == 0.0 {
            return Numeric::zero(0);
        }
        // `{:.*e}` yields exactly `sig` significant digits; expanding it to a
        // plain decimal (trailing zeros stripped) matches numeric_out.
        Numeric::parse(&sci_to_plain(&format!("{:.*e}", sig - 1, v)))
            .expect("finite f64 renders to valid numeric")
    }

    // ---- parsing ----------------------------------------------------------

    /// `numeric_in`: parse input text. `NaN`, `[+-]Inf[inity]`, and decimal
    /// numbers with optional exponent. Leading/trailing ASCII spaces are
    /// trimmed. The display scale comes from the fractional-digit count (so
    /// `40.500000` keeps scale 6). Out-of-range magnitude is
    /// [`ParseError::Overflow`]; malformed text is [`ParseError::Syntax`].
    pub fn parse(input: &str) -> Result<Numeric, ParseError> {
        let s = trim_numeric_space(input);
        if s.is_empty() {
            return Err(ParseError::Syntax);
        }
        let (neg, rest) = match s.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        if rest.eq_ignore_ascii_case("nan") {
            return Ok(Numeric::nan());
        }
        if rest.eq_ignore_ascii_case("inf") || rest.eq_ignore_ascii_case("infinity") {
            return Ok(if neg {
                Numeric::neg_inf()
            } else {
                Numeric::pos_inf()
            });
        }

        // Split off an exponent (`e`/`E`).
        let (mantissa, exp) = match rest.split_once(['e', 'E']) {
            Some((m, e)) => {
                let e = e.strip_prefix('+').unwrap_or(e);
                match e.parse::<i64>() {
                    Ok(v) => (m, v),
                    // An exponent too big even for i64 is always out of range:
                    // a positive one overflows the weight, a negative one the
                    // scale. Only reject as syntax if it is not all digits.
                    Err(_) => {
                        let digits_only = e
                            .strip_prefix('-')
                            .unwrap_or(e)
                            .bytes()
                            .all(|b| b.is_ascii_digit());
                        if digits_only && !e.is_empty() && e != "-" {
                            return Err(ParseError::Overflow);
                        }
                        return Err(ParseError::Syntax);
                    }
                }
            }
            None => (rest, 0),
        };

        let (int_str, frac_str) = match mantissa.split_once('.') {
            Some((i, f)) => (i, f),
            None => (mantissa, ""),
        };
        if (int_str.is_empty() && frac_str.is_empty())
            || !int_str.bytes().all(|b| b.is_ascii_digit())
            || !frac_str.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(ParseError::Syntax);
        }

        // Combined significant digits and the index of the decimal point,
        // shifted by the exponent. `dscale` is the fractional-digit count after
        // the shift, floored at zero. Saturating arithmetic keeps an extreme
        // exponent (near i64::MIN/MAX) from overflowing here — the range guard
        // below rejects it as an overflow.
        let point = (int_str.len() as i64).saturating_add(exp);
        let dscale = (frac_str.len() as i64).saturating_sub(exp).max(0);
        // Weight of the first combined digit (before any leading-zero strip).
        let dweight = point.saturating_sub(1);
        if dscale > MAX_DSCALE || floor_div(dweight, 4) > MAX_NBASE_WEIGHT {
            return Err(ParseError::Overflow);
        }

        let digits: Vec<u8> = int_str
            .bytes()
            .chain(frac_str.bytes())
            .map(|b| b - b'0')
            .collect();
        let low = (point - digits.len() as i64) as i32;
        Ok(Numeric::from_coeff(neg, digits, low, dscale as i32))
    }

    // ---- rendering --------------------------------------------------------

    /// `numeric_out`: plain decimal with exactly `dscale` fractional digits (no
    /// exponent), `NaN` / `Infinity` / `-Infinity` for the specials.
    pub fn to_display(&self) -> String {
        match self.sign {
            Sign::NaN => return "NaN".to_string(),
            Sign::PInf => return "Infinity".to_string(),
            Sign::NInf => return "-Infinity".to_string(),
            _ => {}
        }
        // Three runs — stored digits, then padding — rather than a walk asking
        // `digit_at` per character, which re-derives `low` and bounds-checks
        // each time. The runs are long: a scale-16 column is mostly padding.
        let neg = self.is_neg() && !self.digits.is_empty();
        let int_len = (self.weight + 1).max(1) as usize;
        let frac_len = self.dscale.max(0) as usize;
        let mut out =
            Vec::with_capacity(neg as usize + int_len + (frac_len > 0) as usize + frac_len);
        if neg {
            out.push(b'-');
        }
        if self.weight < 0 {
            out.push(b'0');
        } else {
            let stored = int_len.min(self.digits.len());
            out.extend(self.digits[..stored].iter().map(|digit| b'0' + digit));
            out.resize(out.len() + int_len - stored, b'0');
        }
        if frac_len > 0 {
            out.push(b'.');
            // Where the fractional digits start; negative means that many
            // zeros come first.
            let start = self.weight + 1;
            let leading = (-start).max(0) as usize;
            let zeros = leading.min(frac_len);
            out.resize(out.len() + zeros, b'0');
            let remaining = frac_len - zeros;
            // Clamped to the length too: zero has no stored digits at all and
            // would otherwise slice from past the end.
            let from = (start.max(0) as usize).min(self.digits.len());
            let taken = remaining.min(self.digits.len() - from);
            out.extend(
                self.digits[from..from + taken]
                    .iter()
                    .map(|digit| b'0' + digit),
            );
            out.resize(out.len() + remaining - taken, b'0');
        }
        String::from_utf8(out).expect("digits and punctuation are ASCII")
    }

    /// Decimal position of the least-significant stored digit. For an empty
    /// (zero) coefficient this is `weight + 1`, which the magnitude helpers
    /// treat as contributing nothing.
    fn low(&self) -> i32 {
        self.weight - (self.digits.len() as i32 - 1)
    }

    /// The base-10 digit at decimal position `pos` (0 = units), or 0 if outside
    /// the stored coefficient.
    fn digit_at(&self, pos: i32) -> u8 {
        if self.digits.is_empty() || pos < self.low() || pos > self.weight {
            0
        } else {
            self.digits[(self.weight - pos) as usize]
        }
    }

    /// Drop leading/trailing zero digits, collapsing an all-zero coefficient to
    /// canonical zero (positive sign, `weight` 0, `dscale` preserved).
    fn normalize(&mut self) {
        if self.is_special() {
            self.digits.clear();
            return;
        }
        let mut start = 0;
        while start < self.digits.len() && self.digits[start] == 0 {
            start += 1;
            self.weight -= 1;
        }
        if start > 0 {
            self.digits.drain(..start);
        }
        while self.digits.last() == Some(&0) {
            self.digits.pop();
        }
        if self.digits.is_empty() {
            self.weight = 0;
            self.sign = Sign::Pos;
        }
    }

    // ---- sign / magnitude helpers ----------------------------------------

    pub fn neg(&self) -> Numeric {
        let mut n = self.clone();
        n.sign = match self.sign {
            Sign::Pos if self.digits.is_empty() => Sign::Pos, // -0 == 0
            Sign::Pos => Sign::Neg,
            Sign::Neg => Sign::Pos,
            Sign::PInf => Sign::NInf,
            Sign::NInf => Sign::PInf,
            Sign::NaN => Sign::NaN,
        };
        n
    }

    pub fn abs(&self) -> Numeric {
        let mut n = self.clone();
        n.sign = match self.sign {
            Sign::Neg => Sign::Pos,
            Sign::NInf => Sign::PInf,
            other => other,
        };
        n
    }

    /// `sign(numeric)`: -1 / 0 / 1 as numeric (scale 0); NaN → NaN.
    pub fn signum(&self) -> Numeric {
        match self.sign {
            Sign::NaN => Numeric::nan(),
            Sign::PInf => Numeric::from_i128(1),
            Sign::NInf => Numeric::from_i128(-1),
            _ if self.is_zero() => Numeric::zero(0),
            _ if self.is_neg() => Numeric::from_i128(-1),
            _ => Numeric::from_i128(1),
        }
    }

    /// Base-10000 weight of the leading digit — the grouping PG uses when it
    /// picks a division result scale.
    fn nbase_weight(&self) -> i64 {
        floor_div(self.weight as i64, 4)
    }

    /// Value of the leading base-10000 "limb" (the 4 decimal digits aligned to
    /// the decimal point covering the leading digit), `0..=9999`.
    fn first_nbase(&self) -> i64 {
        let nw = self.nbase_weight() as i32;
        let mut v = 0i64;
        for k in 0..4 {
            let pos = 4 * nw + (3 - k); // high-to-low within the limb
            v = v * 10 + self.digit_at(pos) as i64;
        }
        v
    }

    // ---- comparison -------------------------------------------------------

    /// Total order used for `<`, `ORDER BY`, etc. NaN sorts greatest and equals
    /// itself; +Infinity is greatest finite-wise, -Infinity least — matching
    /// PG's numeric btree order.
    pub fn cmp(&self, other: &Numeric) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        let rank = |n: &Numeric| match n.sign {
            Sign::NInf => 0,
            Sign::Neg | Sign::Pos => 1,
            Sign::PInf => 2,
            Sign::NaN => 3,
        };
        let (ra, rb) = (rank(self), rank(other));
        if ra != rb {
            return ra.cmp(&rb);
        }
        if ra != 1 {
            return Equal; // both -Inf, both +Inf, or both NaN
        }
        // Both finite: compare sign, then magnitude.
        match (self.is_neg(), other.is_neg()) {
            (false, true) => Greater,
            (true, false) => Less,
            (neg, _) => {
                let ord = cmp_mag(self, other);
                if neg { ord.reverse() } else { ord }
            }
        }
    }

    // ---- addition / subtraction ------------------------------------------

    pub fn add(&self, other: &Numeric) -> Numeric {
        if let Some(s) = self.special_add(other) {
            return s;
        }
        let dscale = self.dscale.max(other.dscale);
        if self.is_neg() == other.is_neg() {
            let (digits, low) = add_mag(self, other);
            Numeric::from_coeff(self.is_neg(), digits, low, dscale)
        } else {
            self.sub_diff_sign(other, dscale)
        }
    }

    pub fn sub(&self, other: &Numeric) -> Numeric {
        self.add(&other.neg())
    }

    /// Add/subtract when operands have opposite sign (or `sub` of same sign):
    /// subtract the smaller magnitude from the larger, keep the larger's sign.
    fn sub_diff_sign(&self, other: &Numeric, dscale: i32) -> Numeric {
        match cmp_mag(self, other) {
            std::cmp::Ordering::Equal => Numeric::zero(dscale),
            std::cmp::Ordering::Greater => {
                let (digits, low) = sub_mag(self, other);
                Numeric::from_coeff(self.is_neg(), digits, low, dscale)
            }
            std::cmp::Ordering::Less => {
                let (digits, low) = sub_mag(other, self);
                Numeric::from_coeff(other.is_neg(), digits, low, dscale)
            }
        }
    }

    fn special_add(&self, other: &Numeric) -> Option<Numeric> {
        if !self.is_special() && !other.is_special() {
            return None;
        }
        Some(match (self.sign, other.sign) {
            (Sign::NaN, _) | (_, Sign::NaN) => Numeric::nan(),
            (Sign::PInf, Sign::NInf) | (Sign::NInf, Sign::PInf) => Numeric::nan(),
            (Sign::PInf, _) | (_, Sign::PInf) => Numeric::pos_inf(),
            (Sign::NInf, _) | (_, Sign::NInf) => Numeric::neg_inf(),
            _ => unreachable!(),
        })
    }

    // ---- multiplication ---------------------------------------------------

    pub fn mul(&self, other: &Numeric) -> Numeric {
        if let Some(s) = self.special_mul(other) {
            return s;
        }
        let neg = self.is_neg() != other.is_neg();
        let dscale = self.dscale + other.dscale;
        if self.is_zero() || other.is_zero() {
            return Numeric::zero(dscale);
        }
        let (digits, low) = mul_mag(self, other);
        Numeric::from_coeff(neg, digits, low, dscale)
    }

    fn special_mul(&self, other: &Numeric) -> Option<Numeric> {
        if !self.is_special() && !other.is_special() {
            return None;
        }
        if self.is_nan() || other.is_nan() {
            return Some(Numeric::nan());
        }
        // inf * 0 is NaN; otherwise the sign rule applies.
        let neg = self.is_neg() != other.is_neg();
        if (self.is_infinite() && other.is_zero()) || (other.is_infinite() && self.is_zero()) {
            return Some(Numeric::nan());
        }
        Some(if neg {
            Numeric::neg_inf()
        } else {
            Numeric::pos_inf()
        })
    }

    // ---- division / modulo ------------------------------------------------

    pub fn div(&self, other: &Numeric) -> Result<Numeric, NumErr> {
        // Order matches PG: NaN propagates first, then any division by zero
        // (even with an infinite dividend), then the ±Infinity algebra.
        if self.is_nan() || other.is_nan() {
            return Ok(Numeric::nan());
        }
        if other.is_zero() {
            return Err(NumErr::new("22012", "division by zero"));
        }
        if self.is_special() || other.is_special() {
            return Ok(self.special_div(other));
        }
        let rscale = self.select_div_scale(other);
        Ok(self.div_to_scale(other, rscale, true))
    }

    /// Result display scale for division: enough to give ~16 significant
    /// digits, but never fewer than either operand's scale (PG's
    /// `select_div_scale`, reproduced from its documented rule).
    fn select_div_scale(&self, other: &Numeric) -> i32 {
        let qweight = self.nbase_weight()
            - other.nbase_weight()
            - if self.first_nbase() <= other.first_nbase() {
                1
            } else {
                0
            };
        let rscale = MIN_SIG_DIGITS as i64 - qweight * 4;
        rscale
            .max(self.dscale as i64)
            .max(other.dscale as i64)
            .max(0)
            .min(MAX_DSCALE) as i32
    }

    /// Divide to exactly `rscale` fractional digits; `round` selects
    /// round-half-away-from-zero versus truncation toward zero.
    fn div_to_scale(&self, other: &Numeric, rscale: i32, round: bool) -> Numeric {
        if self.is_zero() {
            return Numeric::zero(rscale);
        }
        // Every caller must have raised `22012` already: `long_divide` cannot,
        // and with a zero denominator its trial loop takes every quotient digit
        // to 9 and returns that as an answer. `div` and `modulo` screen it; the
        // one that did not, `log_base` via `div_guard`, is why this is here.
        debug_assert!(
            !other.is_zero(),
            "div_to_scale by zero: the caller owes a 22012"
        );
        let neg = self.is_neg() != other.is_neg();
        let a_low = self.low();
        let b_low = other.low();
        // quotient*10^rscale = A*10^(a_low - b_low + rscale) / B
        let shift = a_low - b_low + rscale;
        let (num_pad, den_pad) = if shift >= 0 {
            (shift as usize, 0)
        } else {
            (0, (-shift) as usize)
        };

        // A divisor that fits in a register replaces the schoolbook
        // trial-and-subtract with one machine divide per digit, and allocates
        // nothing but the quotient. This is every `avg`, whose divisor is the
        // row count — and `avg` runs one division per group, so a `GROUP BY`
        // over a near-unique key runs it once per row.
        if let Some(den) = as_register(&other.digits, den_pad) {
            let (mut q, rem) = divide_by_register(&self.digits, num_pad, den);
            // Half away from zero, in the same registers: 2*rem >= den.
            if round && (rem as u128) * 2 >= den as u128 {
                inc_be(&mut q);
            }
            return Numeric::from_coeff(neg, q, -rscale, rscale);
        }

        // At most one pad is nonzero, so both extends can run unconditionally —
        // branching on either one would encode the sign of `shift` in a derived
        // value that no longer says so.
        let mut num = self.digits.clone();
        let mut den = other.digits.clone();
        num.extend(std::iter::repeat_n(0u8, num_pad));
        den.extend(std::iter::repeat_n(0u8, den_pad));
        let (mut q, rem) = long_divide(&num, &den);
        if round {
            // Half away from zero: 2*rem >= den → round the magnitude up.
            let two_rem = add_be(&rem, &rem);
            if cmp_be(&two_rem, &den) != std::cmp::Ordering::Less {
                q = add_be(&q, &[1]);
            }
        }
        Numeric::from_coeff(neg, q, -rscale, rscale)
    }

    /// The ±Infinity algebra of division. Caller has already handled NaN and a
    /// zero divisor, so at least one operand is ±Infinity here.
    fn special_div(&self, other: &Numeric) -> Numeric {
        let neg = self.is_neg() != other.is_neg();
        match (self.is_infinite(), other.is_infinite()) {
            (true, true) => Numeric::nan(),
            (true, false) => {
                if neg {
                    Numeric::neg_inf()
                } else {
                    Numeric::pos_inf()
                }
            }
            (false, true) => Numeric::zero(0),
            (false, false) => unreachable!("special_div requires an infinite operand"),
        }
    }

    /// `x % y = x - trunc(x/y)*y`, with `dscale = max(scale_x, scale_y)`.
    pub fn modulo(&self, other: &Numeric) -> Result<Numeric, NumErr> {
        // NaN first, then division by zero (even `inf % 0`), then ±Infinity.
        if self.is_nan() || other.is_nan() {
            return Ok(Numeric::nan());
        }
        if other.is_zero() {
            return Err(NumErr::new("22012", "division by zero"));
        }
        if self.is_infinite() {
            // inf % y = NaN (no finite remainder).
            return Ok(Numeric::nan());
        }
        if other.is_infinite() {
            // finite % inf = the finite value itself.
            return Ok(self.clone());
        }
        let dscale = self.dscale.max(other.dscale);
        // q = trunc(x/y) toward zero (scale 0), then r = x - q*y.
        let q = self.div_to_scale(other, 0, false);
        let mut r = self.sub(&q.mul(other));
        r.dscale = dscale;
        r.normalize();
        Ok(r)
    }

    /// `gcd(x, y)`, always non-negative, with `dscale = max(scale_x, scale_y)`.
    ///
    /// NaN *and* the infinities answer NaN: an infinity has no finite divisor,
    /// and PG reports that as NaN rather than an error (unlike the integer
    /// overloads, which raise on an unrepresentable magnitude).
    ///
    /// Euclid runs on the decimals directly. It terminates because both
    /// operands are whole multiples of `10^-dscale`, so the remainders are a
    /// strictly decreasing sequence of such multiples — the same argument that
    /// makes integer Euclid terminate, one grid finer.
    pub fn gcd(&self, other: &Numeric) -> Numeric {
        if self.is_special() || other.is_special() {
            return Numeric::nan();
        }
        let dscale = self.dscale.max(other.dscale);
        let mut a = self.abs();
        let mut b = other.abs();
        while !b.is_zero() {
            // `b` is non-zero, so the only error `modulo` can report cannot
            // arise here.
            let r = match a.modulo(&b) {
                Ok(r) => r,
                Err(_) => unreachable!("gcd: modulo by a non-zero divisor"),
            };
            a = b;
            b = r;
        }
        a.round(dscale)
    }

    /// `lcm(x, y)`, always non-negative, with `dscale = max(scale_x, scale_y)`.
    ///
    /// A zero operand answers zero before anything is divided, matching PG —
    /// `lcm(0, x)` is 0 even where `x` would make the product overflow.
    ///
    /// Rounding to `dscale` at the end never loses a digit: the result is a
    /// whole multiple of each operand, so it needs no more fractional digits
    /// than the operand with the fewer of them.
    pub fn lcm(&self, other: &Numeric) -> Result<Numeric, NumErr> {
        if self.is_special() || other.is_special() {
            return Ok(Numeric::nan());
        }
        let dscale = self.dscale.max(other.dscale);
        if self.is_zero() || other.is_zero() {
            return Ok(Numeric::zero(dscale));
        }
        let g = self.gcd(other);
        // `self / g` is exact — `g` divides `self` — so the division introduces
        // no rounding of its own, and dividing before multiplying keeps the
        // intermediate as small as PG keeps it.
        let q = self.div(&g)?;
        Ok(q.mul(other).abs().round(dscale))
    }

    // ---- rounding / truncation -------------------------------------------

    /// `round(x, s)`: round-half-away-from-zero to `s` fractional digits (`s`
    /// may be negative); result `dscale = max(s, 0)`.
    pub fn round(&self, s: i32) -> Numeric {
        if self.is_special() {
            return self.clone();
        }
        self.set_scale(s, true)
    }

    /// Round to at most `prec` significant digits, dropping the display scale.
    /// Unlike [`Numeric::round`], which fixes the number of digits *after* the
    /// point, this bounds the total width — what an iterative computation needs
    /// to keep its intermediates from growing without limit.
    fn round_sig(&self, prec: i32) -> Numeric {
        if self.is_special() || self.is_zero() || (self.digits.len() as i32) <= prec {
            return self.clone();
        }
        let mut v = self.round(prec - 1 - self.weight);
        v.dscale = 0;
        v
    }

    /// `trunc(x, s)`: truncate toward zero to `s` fractional digits.
    pub fn trunc(&self, s: i32) -> Numeric {
        if self.is_special() {
            return self.clone();
        }
        self.set_scale(s, false)
    }

    fn set_scale(&self, s: i32, round: bool) -> Numeric {
        let dscale = s.max(0);
        if self.is_zero() {
            return Numeric::zero(dscale);
        }
        // Keep digits at positions >= -s; the coefficient scaled by 10^s is the
        // integer K = Σ digit_at(pos) * 10^(pos + s) for pos in [-s ..= hi].
        let hi = self.weight;
        if hi < -s {
            // Everything is below the kept scale: rounds to 0 or to one ulp.
            let round_up = round && self.digit_at(-s - 1) >= 5;
            if round_up {
                return Numeric::from_coeff(self.is_neg(), vec![1], -s, dscale);
            }
            return Numeric::zero(dscale);
        }
        let mut k: Vec<u8> = ((-s)..=hi).rev().map(|pos| self.digit_at(pos)).collect();
        if round && self.digit_at(-s - 1) >= 5 {
            k = add_be(&k, &[1]);
        }
        Numeric::from_coeff(self.is_neg(), k, -s, dscale)
    }

    /// `ceil(x)`: smallest integer >= x (scale 0).
    pub fn ceil(&self) -> Numeric {
        if self.is_special() {
            return self.clone();
        }
        let t = self.trunc(0);
        if !self.is_neg() && self.cmp(&t) == std::cmp::Ordering::Greater {
            t.add(&Numeric::from_i128(1))
        } else {
            t
        }
    }

    /// `floor(x)`: largest integer <= x (scale 0).
    pub fn floor(&self) -> Numeric {
        if self.is_special() {
            return self.clone();
        }
        let t = self.trunc(0);
        if self.is_neg() && self.cmp(&t) == std::cmp::Ordering::Less {
            t.sub(&Numeric::from_i128(1))
        } else {
            t
        }
    }

    // ---- typmod -----------------------------------------------------------

    /// Pack `(precision, scale)` into PostgreSQL's single-integer `numeric`
    /// modifier, without the four-byte varlena header `pg_attribute.atttypmod`
    /// adds on top (see the catalog's `atttypmod_of`).
    ///
    /// The scale occupies the low 11 bits as a *signed* field, so
    /// `numeric(4,-2)` round trips; the precision sits in the 16 bits above it.
    /// Verified against PostgreSQL 18.4: `numeric(5,2)`, `numeric(4,-2)` and
    /// `numeric(3)` store `atttypmod` 327686, 264194 and 196612 — this value
    /// plus 4 in each case.
    pub fn pack_typmod(precision: i32, scale: i32) -> i32 {
        (precision << 16) | (scale & 0x7ff)
    }

    /// Inverse of [`Numeric::pack_typmod`].
    pub fn unpack_typmod(typmod: i32) -> (i32, i32) {
        let precision = (typmod >> 16) & 0xffff;
        // Sign-extend the 11-bit scale.
        let scale = ((typmod & 0x7ff) ^ 1024) - 1024;
        (precision, scale)
    }

    /// Apply a `numeric(precision, scale)` type modifier: round to `scale`
    /// fractional digits and verify the result is smaller than
    /// `10^(precision - scale)`, else `22003 numeric field overflow` (with PG's
    /// DETAIL, which states that bound outright). NaN is allowed unchanged;
    /// ±Infinity cannot be stored in a constrained numeric.
    ///
    /// The bound is a **signed** exponent: PostgreSQL allows `scale > precision`
    /// — `numeric(2,5)` holds values below `10^-3` — so clamping it at zero
    /// would reject every value of such a column. For the ordinary
    /// `scale <= precision` it is non-negative and the distinction never
    /// arises.
    pub fn apply_typmod(&self, precision: i32, scale: i32) -> Result<Numeric, NumErr> {
        if self.is_nan() {
            return Ok(self.clone());
        }
        if self.is_infinite() {
            return Err(field_overflow(precision, scale, true));
        }
        let rounded = self.round(scale);
        // Zero is below every bound, and has no leading digit to compare.
        if rounded.is_zero() {
            return Ok(rounded);
        }
        // `weight + 1` is the exponent of the smallest power of ten above it.
        if rounded.weight + 1 > precision - scale {
            return Err(field_overflow(precision, scale, false));
        }
        Ok(rounded)
    }

    // ---- fixed-point (decimal) conversions --------------------------------

    /// The value as the integer `self * 10^scale`, or `None` when it is not
    /// **exactly** representable in `numeric(precision, scale)`.
    ///
    /// Refuses rather than rounds: a column with a typmod has been through
    /// [`Numeric::apply_typmod`] and cannot fail here, and one without has no
    /// rounding rule of its own, so dropping digits would silently store a
    /// different value. `None` for NaN and ±Infinity, which have no fixed-point
    /// image at all.
    pub fn to_scaled_i128(&self, precision: u8, scale: i8) -> Option<i128> {
        let (neg, _) = self.scaled_span(precision, scale)?;
        // The scale's padding is one multiply, not a walk over the positions it
        // occupies: `5` in a scale-16 column has one stored digit and seventeen
        // positions. `scaled_span` proved `low() >= -scale`, so the shift never
        // drops a digit.
        let shift = (self.low() + scale as i32).max(0) as usize;
        // 19 digits always fit a `u64`, and the common column is well inside
        // that — `numeric(15,2)` cannot leave it, and even a scale-16 quotient
        // has seventeen.
        let acc = if self.digits.len() + shift <= 19 {
            let mut acc: u64 = 0;
            for &digit in &self.digits {
                acc = acc * 10 + digit as u64;
            }
            (acc * POW10_U64[shift]) as i128
        } else {
            let mut acc: i128 = 0;
            for &digit in &self.digits {
                acc = acc.checked_mul(10)?.checked_add(digit as i128)?;
            }
            acc.checked_mul(*POW10_I128.get(shift)?)?
        };
        if neg { acc.checked_neg() } else { Some(acc) }
    }

    /// As [`Numeric::to_scaled_i128`], but rendered into `out` as a decimal
    /// string, for the widths no Rust integer covers (`precision > 38`, which
    /// Arrow stores as a 256-bit decimal). The caller parses it into its own
    /// wide integer.
    ///
    /// Writes into a caller-owned buffer so a column's worth of values can
    /// share one allocation; `false` leaves `out` unspecified.
    pub fn write_scaled_string(&self, precision: u8, scale: i8, out: &mut String) -> bool {
        out.clear();
        let Some((neg, hi)) = self.scaled_span(precision, scale) else {
            return false;
        };
        let scale = scale as i32;
        out.reserve((hi + scale + 2).max(0) as usize);
        if neg {
            out.push('-');
        }
        for pos in (-scale..=hi).rev() {
            out.push((b'0' + self.digit_at(pos)) as char);
        }
        true
    }

    /// Whether the value is **exactly** representable as
    /// `numeric(precision, scale)`.
    pub fn fits_decimal(&self, precision: u8, scale: i8) -> bool {
        self.scaled_span(precision, scale).is_some()
    }

    /// The sign and leading digit position of `self * 10^scale`, or `None` when
    /// the value does not fit `numeric(precision, scale)`.
    ///
    /// Two ways to not fit, and both are refusals rather than approximations:
    /// a digit below position `-scale` (the value needs a finer scale than the
    /// column has), or more than `precision` digits in total.
    fn scaled_span(&self, precision: u8, scale: i8) -> Option<(bool, i32)> {
        if self.is_special() {
            return None;
        }
        let scale = scale as i32;
        if self.is_zero() {
            return Some((false, -scale));
        }
        if self.low() < -scale {
            return None;
        }
        // The digit count is fixed by the two ends: `weight` holds the leading
        // digit's position and `-scale` the trailing one.
        let hi = self.weight.max(-scale);
        if (hi + scale + 1) as usize > precision as usize {
            return None;
        }
        Some((self.is_neg(), hi))
    }

    /// Inverse of [`Numeric::to_scaled_i128`]: the value `v * 10^-scale`, with
    /// `scale` as its display scale — which is what makes a `numeric(p, s)`
    /// column round trip exactly, trailing zeros and all.
    ///
    /// Shaped to reach a `u64` in as few 128-bit divisions as possible, since
    /// each is a libcall. What forces the wide path is not a large value but a
    /// large *scale*: a column with no typmod stores at scale 16, so an
    /// unremarkable `321000.00` becomes `3.21e21` on five significant digits.
    /// Those sixteen zeros come off in chunks rather than one at a time.
    pub fn from_scaled_i128(v: i128, scale: i8) -> Numeric {
        if v == 0 {
            return Numeric::zero(scale.max(0) as i32);
        }
        let neg = v < 0;
        let mut mag = v.unsigned_abs();
        let mut low = -(scale as i32);
        let dscale = (scale as i32).max(0);
        if mag > u64::MAX as u128 {
            // Only exact divisions: a chunk that does not divide evenly would
            // drop significant digits, so the step falls back to a smaller one.
            for step in [16usize, 8, 4, 2, 1] {
                let power = POW10_U128[step];
                while mag.is_multiple_of(power) {
                    mag /= power;
                    low += step as i32;
                    if mag <= u64::MAX as u128 {
                        break;
                    }
                }
                if mag <= u64::MAX as u128 {
                    break;
                }
            }
        }
        match u64::try_from(mag) {
            Ok(mag) => digits_of!(mag, 20, neg, low, dscale),
            Err(_) => digits_of!(mag, 39, neg, low, dscale),
        }
    }

    /// As [`Numeric::from_scaled_i128`], from the decimal string a wider
    /// integer renders to. Rejects anything that is not `-?[0-9]+`.
    pub fn from_scaled_str(v: &str, scale: i8) -> Option<Numeric> {
        let (neg, digits) = match v.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, v),
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let digits: Vec<u8> = digits.bytes().map(|b| b - b'0').collect();
        Some(Numeric::from_coeff(
            neg,
            digits,
            -(scale as i32),
            scale as i32,
        ))
    }

    // ---- integer / float conversions -------------------------------------

    /// `numeric_int*`: round half-away-from-zero to an integer, as an `i128`.
    /// `None` when the magnitude does not fit `i128` (the caller reports the
    /// target type's out-of-range error) — NaN/Infinity have no integer image
    /// and are rejected before this by the cast layer.
    pub fn to_i128(&self) -> Option<i128> {
        if self.is_special() {
            return None;
        }
        let r = self.round(0);
        if r.is_zero() {
            return Some(0);
        }
        let mut acc: i128 = 0;
        for &d in &r.digits {
            acc = acc.checked_mul(10)?.checked_add(d as i128)?;
        }
        // Pad for any zero positions between the last stored digit and units.
        for _ in 0..r.low() {
            acc = acc.checked_mul(10)?;
        }
        if r.is_neg() {
            acc.checked_neg()
        } else {
            Some(acc)
        }
    }

    /// `numeric_float8`/`_float4`: nearest double. Uses a bounded significant-
    /// digit string so an astronomically large weight never allocates.
    pub fn to_f64(&self) -> f64 {
        match self.sign {
            Sign::NaN => return f64::NAN,
            Sign::PInf => return f64::INFINITY,
            Sign::NInf => return f64::NEG_INFINITY,
            _ => {}
        }
        if self.is_zero() {
            return 0.0;
        }
        let digitstr: String = self.digits.iter().map(|d| (b'0' + d) as char).collect();
        let low = self.low();
        let s = format!(
            "{}{}e{}",
            if self.is_neg() { "-" } else { "" },
            digitstr,
            low
        );
        s.parse().unwrap_or(f64::NAN)
    }

    // ---- transcendentals --------------------------------------------------

    /// `sqrt(numeric)`: principal square root. Errors on a negative argument.
    pub fn sqrt(&self) -> Result<Numeric, NumErr> {
        match self.sign {
            Sign::NaN => return Ok(Numeric::nan()),
            Sign::PInf => return Ok(Numeric::pos_inf()),
            Sign::NInf => {
                return Err(NumErr::new(
                    "2201F",
                    "cannot take square root of a negative number",
                ));
            }
            _ => {}
        }
        if self.is_neg() {
            return Err(NumErr::new(
                "2201F",
                "cannot take square root of a negative number",
            ));
        }
        if self.is_zero() {
            let rscale = self.sqrt_scale();
            return Ok(Numeric::zero(rscale));
        }
        let rscale = self.sqrt_scale();
        Ok(self.sqrt_to_scale(rscale))
    }

    /// Result scale for `sqrt`: ~16 significant digits, honoring the input's
    /// own scale (reproduced from PG's observable output, see
    /// `sqrt_matches_pg`).
    fn sqrt_scale(&self) -> i32 {
        let base = (MIN_SIG_DIGITS as i64 - 1) - 2 * self.nbase_weight();
        base.max(self.dscale as i64).max(0).min(MAX_DSCALE) as i32
    }

    /// Correctly-rounded sqrt to `rscale` fractional digits via integer isqrt:
    /// `floor(sqrt(value * 10^(2*rscale)))` then round the last digit.
    fn sqrt_to_scale(&self, rscale: i32) -> Numeric {
        // value = A * 10^a_low ; want round(sqrt(value) * 10^rscale).
        // sqrt(value)*10^rscale = sqrt(A * 10^(a_low + 2*rscale)).
        let a_low = self.low();
        let shift = a_low + 2 * rscale;
        // Radicand must be an integer: pad or (rarely) drop low digits. shift
        // is >= 0 here because rscale >= 0 and a_low is small in practice; a
        // negative one only drops digits below the requested result scale.
        let mut radicand = self.digits.clone();
        if shift >= 0 {
            radicand.extend(std::iter::repeat(0u8).take(shift as usize));
        } else {
            // Truncate low digits (only when a_low is very negative); rare.
            let drop = (-shift) as usize;
            if drop >= radicand.len() {
                radicand.clear();
            } else {
                radicand.truncate(radicand.len() - drop);
            }
        }
        let (root, rem) = isqrt_be(&radicand);
        let mut q = root;
        // (root+1)^2 - value = 2*root+1 - rem ; nearest integer root: round up
        // when rem*2 >= 2*root+1  ⇔  rem > root.
        if cmp_be(&rem, &q) == std::cmp::Ordering::Greater {
            q = add_be(&q, &[1]);
        }
        Numeric::from_coeff(false, q, -rscale, rscale)
    }

    /// `ln(numeric)`: natural logarithm. Errors on zero/negative arguments.
    /// Result scale gives ~16 significant digits (see `ln_matches_pg`).
    pub fn ln(&self) -> Result<Numeric, NumErr> {
        if self.is_nan() {
            return Ok(Numeric::nan());
        }
        if self.is_infinite() && !self.is_neg() {
            return Ok(Numeric::pos_inf());
        }
        if self.is_zero() {
            return Err(NumErr::new("2201E", "cannot take logarithm of zero"));
        }
        if self.is_neg() {
            return Err(NumErr::new(
                "2201E",
                "cannot take logarithm of a negative number",
            ));
        }
        let guard = 30;
        let val = self.ln_internal(guard);
        let rscale = log_scale(&val);
        Ok(val.round(rscale))
    }

    /// `log(numeric)` / `log10`: base-10 logarithm.
    pub fn log10(&self) -> Result<Numeric, NumErr> {
        if self.is_nan() {
            return Ok(Numeric::nan());
        }
        if self.is_zero() {
            return Err(NumErr::new("2201E", "cannot take logarithm of zero"));
        }
        if self.is_neg() {
            return Err(NumErr::new(
                "2201E",
                "cannot take logarithm of a negative number",
            ));
        }
        if self.is_infinite() {
            return Ok(Numeric::pos_inf());
        }
        let guard = 30;
        let val = self.ln_internal(guard).div_guard(&ln10(guard), guard);
        let rscale = log_scale(&val);
        Ok(val.round(rscale))
    }

    /// `log(base, x)`: logarithm of `x` to base `base` = ln(x)/ln(base).
    ///
    /// NaN in either operand wins, then zero/negative raise, then the infinities
    /// resolve as limits of the ratio — the order PostgreSQL applies them in.
    pub fn log_base(&self, x: &Numeric) -> Result<Numeric, NumErr> {
        if self.is_nan() || x.is_nan() {
            return Ok(Numeric::nan());
        }
        for arg in [self, x] {
            if arg.is_zero() {
                return Err(NumErr::new("2201E", "cannot take logarithm of zero"));
            }
            if arg.is_neg() {
                return Err(NumErr::new(
                    "2201E",
                    "cannot take logarithm of a negative number",
                ));
            }
        }
        // `ln` and `log10` special-case infinity; this one has to as well, or an
        // infinite operand reaches `ln_internal`, which has no finite series to
        // sum for it. PG's rules, the limits of ln(x)/ln(base): an infinite base
        // sends the ratio to 0, an infinite argument to infinity, and both at
        // once is the indeterminate ∞/∞ — NaN, not 1.
        match (self.is_infinite(), x.is_infinite()) {
            (true, true) => return Ok(Numeric::nan()),
            (true, false) => return Ok(Numeric::zero(0)),
            (false, true) => return Ok(Numeric::pos_inf()),
            (false, false) => {}
        }
        let guard = 30;
        // Base 1 has no logarithm, and PG reports that as the division by zero
        // it literally is rather than as a domain error. Keying on the computed
        // `ln(base)` instead of `base == 1` is what keeps a base merely *near*
        // one working: PG computes `log(1.000016, 8.452010e18)` fine, and it
        // would not if it tested the base itself.
        let ln_base = self.ln_internal(guard);
        if ln_base.is_zero() {
            return Err(NumErr::new("22012", "division by zero"));
        }
        let val = x.ln_internal(guard).div_guard(&ln_base, guard);
        let rscale = log_scale(&val);
        Ok(val.round(rscale))
    }

    /// `exp(numeric)`: e raised to this value.
    pub fn exp(&self) -> Result<Numeric, NumErr> {
        if self.is_nan() {
            return Ok(Numeric::nan());
        }
        if self.is_infinite() {
            return Ok(if self.is_neg() {
                Numeric::zero(0)
            } else {
                Numeric::pos_inf()
            });
        }
        let rscale = exp_scale(self.to_f64());
        let guard = rscale + 24;
        let val = self.exp_internal(guard)?;
        Ok(val.round(rscale))
    }

    /// `x ^ y` / `power(x, y)`: reproduces PG's special-case rules and result
    /// scale. Integer exponents use exact repeated squaring; other exponents go
    /// through `exp(y * ln x)`.
    pub fn power(&self, y: &Numeric) -> Result<Numeric, NumErr> {
        if self.is_nan() || y.is_nan() {
            // PG: x^0 = 1 even for a NaN base; otherwise NaN propagates.
            // TODO: return 1 for `1 ^ NaN`; PG answers 1, this returns NaN.
            if y.is_zero() {
                return Ok(Numeric::from_i128(1));
            }
            return Ok(Numeric::nan());
        }
        if self.is_infinite() || y.is_infinite() {
            return self.power_special(y);
        }
        if self.is_zero() {
            return if y.is_zero() {
                Ok(Numeric::from_i128(1))
            } else if y.is_neg() {
                Err(NumErr::new(
                    "2201F",
                    "zero raised to a negative power is undefined",
                ))
            } else {
                Ok(Numeric::zero(0))
            };
        }
        let y_int = y.as_integer();
        if self.is_neg() && y_int.is_none() {
            return Err(NumErr::new(
                "2201F",
                "a negative number raised to a non-integer power yields a complex result",
            ));
        }
        // Result scale from the estimated result weight, `trunc(y * log10|x|)`,
        // but never below either operand's own scale — `2.0 ^ 100` keeps the
        // base's one fractional digit even though 16 significant digits are
        // long spent. The estimate is done in f64 and may be ±infinite for an
        // extreme base; `scale_from_estimate` clamps safely.
        let weight_est = y.to_f64() * self.abs().to_f64().log10();
        let rscale = scale_from_estimate(weight_est.trunc())
            .max(self.dscale)
            .max(y.dscale)
            .min(MAX_RESULT_SCALE);

        if let Some(n) = y_int {
            // Decide the extreme cases from the estimate instead of computing
            // them: `117743296169.0 ^ 1000000000` overflows by ten orders of
            // magnitude, and multiplying up to the limit first would take
            // minutes. A few digits of margin leave anything borderline to the
            // exact check inside `int_power`.
            if weight_est > MAX_DECIMAL_WEIGHT + 4.0 {
                return Err(NumErr::new("22003", "value overflows numeric format"));
            }
            if weight_est < -(rscale as f64) - 2.0 {
                // Smaller than half an ulp at the result scale: a plain zero.
                return Ok(Numeric::zero(rscale));
            }
            let mag = self.abs().int_power(n, rscale, weight_est)?;
            let neg = self.is_neg() && n % 2 != 0;
            return Ok(if neg { mag.neg() } else { mag });
        }
        // x > 0, non-integer y: exp(y * ln x).
        let guard = rscale + 24;
        let prod = y.mul(&self.ln_internal(guard));
        let val = prod.exp_internal(guard)?;
        Ok(val.round(rscale))
    }

    /// If this value is an exact integer, return it as `i64` (for the
    /// integer-exponent power path). `None` if it has a fractional part or does
    /// not fit `i64`.
    fn as_integer(&self) -> Option<i64> {
        if self.is_special() {
            return None;
        }
        // A fractional digit exists when the lowest stored position is < 0.
        let low = self.low();
        if !self.is_zero() && low < 0 {
            return None;
        }
        let v = self.to_i128()?;
        i64::try_from(v).ok()
    }

    /// `|x|^n` for integer `n`, via repeated squaring, rounded to `rscale`.
    /// Negative `n` inverts. Overflow of the magnitude is a numeric-format error.
    ///
    /// The squaring runs at a bounded precision: exact squaring doubles the
    /// digit count every step, so `1.000000000123 ^ 2147483648` would ask for
    /// ~2^31 · 13 digits and never finish, even though its result is 1.0000000
    /// to sixteen places. `weight_est` is the caller's estimate of the result's
    /// decimal weight and sets how many significant digits that takes.
    fn int_power(&self, n: i64, rscale: i32, weight_est: f64) -> Result<Numeric, NumErr> {
        if n == 0 {
            return Ok(Numeric::from_i128(1).round(rscale));
        }
        // Digits the rounded result needs, plus guard digits: each rounding
        // below costs a relative 10^-prec and there are under 2·64 of them.
        let needed = (weight_est.abs() + rscale as f64).clamp(
            MIN_SIG_DIGITS as f64,
            MAX_DECIMAL_WEIGHT + MAX_RESULT_SCALE as f64,
        );
        let prec = needed as i32 + 24;
        let mut base = self.clone();
        base.dscale = 0;
        let mut exp = n.unsigned_abs();
        let mut acc = Numeric::from_i128(1);
        while exp > 0 {
            if exp & 1 == 1 {
                acc = acc.mul(&base).round_sig(prec);
                if acc.nbase_weight() > MAX_NBASE_WEIGHT {
                    return Err(NumErr::new("22003", "value overflows numeric format"));
                }
            }
            exp >>= 1;
            if exp > 0 {
                base = base.mul(&base).round_sig(prec);
                if base.nbase_weight() > MAX_NBASE_WEIGHT {
                    return Err(NumErr::new("22003", "value overflows numeric format"));
                }
            }
        }
        if n < 0 {
            // 1 / acc, to enough guard digits then round.
            let guard = rscale + 24;
            acc = Numeric::from_i128(1).div_guard(&acc, guard);
        }
        Ok(acc.round(rscale))
    }

    /// `x ^ y` where at least one of `x`/`y` is ±Infinity (NaN handled by the
    /// caller). Reproduces PG's limits: `x^0 = 1`, `1^y = 1`, an infinite base
    /// goes to `+Inf`/`0` by the exponent's sign, and a finite base with an
    /// infinite exponent goes to `+Inf` or `0` depending on whether `|x| > 1`.
    fn power_special(&self, y: &Numeric) -> Result<Numeric, NumErr> {
        let one = Numeric::from_i128(1);
        if y.is_zero() {
            return Ok(one);
        }
        if !self.is_infinite() && self.abs() == one {
            return Ok(one); // 1^y = 1 for any y, including ±Infinity
        }
        if self.is_infinite() {
            return Ok(if y.is_neg() {
                Numeric::zero(0)
            } else {
                Numeric::pos_inf()
            });
        }
        // Finite base, infinite exponent.
        let abs_gt_one = self.abs().cmp(&one) == std::cmp::Ordering::Greater;
        let to_infinity = abs_gt_one != y.is_neg();
        Ok(if to_infinity {
            Numeric::pos_inf()
        } else {
            Numeric::zero(0)
        })
    }

    /// Divide to `guard` fractional digits (rounded) — an internal helper for
    /// the transcendental routines that need a fixed working precision.
    fn div_guard(&self, other: &Numeric, guard: i32) -> Numeric {
        self.div_to_scale(other, guard, true)
    }

    /// Natural log to `guard` fractional digits, by reducing the argument to
    /// near 1 with repeated square roots and summing the `atanh` series.
    ///
    /// The magnitude is taken out first — `ln(m · 10^e) = ln(m) + e·ln(10)`,
    /// with `m` the coefficient read at weight 0, so `m ∈ [1, 10)`. Without
    /// that, the square-root reduction runs at an *absolute* working scale and
    /// underflows: `sqrt(1.234e-89)` is `1.1e-45`, which rounds to zero at
    /// `work` fractional digits, and a zero `t` never leaves the reduction loop.
    /// The series then ran on `w = (0-1)/(0+1) = -1`, diverged to its 100 000-term
    /// cap, and the result was multiplied by `2^100` — a second of work for an
    /// answer off by thirty orders of magnitude.
    fn ln_internal(&self, guard: i32) -> Numeric {
        let work = guard + 8;
        // The coefficient read at weight 0 is the mantissa; the old weight is
        // the power of ten that was factored out.
        let mantissa = Numeric::from_coeff(
            false,
            self.digits.clone(),
            1 - self.digits.len() as i32,
            work,
        );
        mantissa
            .ln_near_one(guard)
            .add(&Numeric::from_i128(self.weight as i128).mul(&ln10(work)))
            .round(guard)
    }

    /// The `ln` kernel: reduce toward 1 by repeated square root and sum the
    /// `atanh` series. Only sound for an argument of moderate magnitude — the
    /// reduction works at an absolute scale, so a very small one would round to
    /// zero — which is why [`ln_internal`](Self::ln_internal) takes the magnitude
    /// out first and only ever hands this a mantissa in `[1, 10)`.
    ///
    /// [`ln10`] calls it directly, on 10 itself; it must not go through
    /// `ln_internal`, which would ask `ln10` for the factored-out `10^1` and
    /// recurse until the stack ran out.
    fn ln_near_one(&self, guard: i32) -> Numeric {
        let work = guard + 8;
        // Reduce t toward 1 by repeated sqrt; ln(self) = 2^s * ln(t).
        let mut t = self.clone();
        let mut s: u32 = 0;
        let lo = match Numeric::parse("0.9") {
            Ok(value) => value,
            Err(_) => panic!("internal numeric literal 0.9 is invalid"),
        };
        let hi = match Numeric::parse("1.1") {
            Ok(value) => value,
            Err(_) => panic!("internal numeric literal 1.1 is invalid"),
        };
        // With the magnitude already out, `t` starts in [1, 10) and six square
        // roots bring it inside [0.9, 1.1]; the cap is a backstop, kept well
        // below 127 so the `1i128 << s` below can't overflow.
        while t.cmp(&lo) == std::cmp::Ordering::Less || t.cmp(&hi) == std::cmp::Ordering::Greater {
            t = t.sqrt_to_scale(work);
            s += 1;
            if s >= 100 {
                break;
            }
        }
        // ln(t) = 2 * (w + w^3/3 + w^5/5 + ...), w = (t-1)/(t+1).
        let one = Numeric::from_i128(1);
        let w = t.sub(&one).div_guard(&t.add(&one), work);
        let w2 = w.mul(&w).round(work);
        let mut term = w.clone();
        let mut sum = w.clone();
        let mut k: i64 = 3;
        loop {
            term = term.mul(&w2).round(work);
            let piece = term.div_guard(&Numeric::from_i128(k as i128), work);
            if piece.is_zero_to_scale(guard) {
                break;
            }
            sum = sum.add(&piece);
            k += 2;
            if k > 100_000 {
                break;
            }
        }
        let ln_t = sum.mul(&Numeric::from_i128(2));
        ln_t.mul(&Numeric::from_i128(1i128 << s)).round(guard)
    }

    /// e^x to `guard` fractional digits, by range-reducing x = m·ln10 + r and
    /// summing the Taylor series for a small remainder.
    fn exp_internal(&self, guard: i32) -> Result<Numeric, NumErr> {
        let work = guard + 8;
        let ln10 = ln10(work);
        // m = round(x / ln10); r = x - m*ln10, |r| <= ln10/2.
        let m_num = self.div_guard(&ln10, 0).round(0);
        let m = m_num
            .to_i128()
            .ok_or_else(|| NumErr::new("22003", "value overflows numeric format"))?;
        if m.unsigned_abs() > MAX_NBASE_WEIGHT as u128 {
            return Err(NumErr::new("22003", "value overflows numeric format"));
        }
        let r = self.sub(&Numeric::from_i128(m).mul(&ln10));
        // Halve r until small, then Taylor, then square back.
        let mut p: u32 = 0;
        let mut rr = r.clone();
        let small = match Numeric::parse("0.01") {
            Ok(value) => value,
            Err(_) => panic!("internal numeric literal 0.01 is invalid"),
        };
        while rr.abs().cmp(&small) == std::cmp::Ordering::Greater {
            rr = rr.div_guard(&Numeric::from_i128(2), work);
            p += 1;
            if p > 60 {
                break;
            }
        }
        // exp(rr) = Σ rr^n / n!, building each term as `prev * rr / n`.
        let one = Numeric::from_i128(1);
        let mut term = one.clone();
        let mut sum = one.clone();
        let mut n: i64 = 1;
        loop {
            term = term
                .mul(&rr)
                .div_guard(&Numeric::from_i128(n as i128), work);
            if term.is_zero_to_scale(guard) {
                break;
            }
            sum = sum.add(&term);
            n += 1;
            if n > 1000 {
                break;
            }
        }
        // Square p times, multiply by 10^m.
        for _ in 0..p {
            sum = sum.mul(&sum).round(work);
        }
        let scale10 = Numeric {
            sign: Sign::Pos,
            weight: m as i32,
            dscale: 0,
            digits: vec![1],
        };
        Ok(sum.mul(&scale10).round(guard))
    }

    /// Whether the value rounds to zero at `scale` fractional digits (loop
    /// termination test for the series).
    fn is_zero_to_scale(&self, scale: i32) -> bool {
        self.is_zero() || self.weight < -scale
    }
}

/// ln(10) to at least `guard` fractional digits. The common transcendental
/// calls (log10, exp, and power's non-integer path) request a modest guard, so
/// ln(10) is computed once at a generous scale and cached; only an unusually
/// large guard recomputes it.
fn ln10(guard: i32) -> Numeric {
    const CACHED_SCALE: i32 = 120;
    if guard <= CACHED_SCALE {
        static LN10: std::sync::OnceLock<Numeric> = std::sync::OnceLock::new();
        return LN10
            .get_or_init(|| Numeric::from_i128(10).ln_near_one(CACHED_SCALE))
            .clone();
    }
    Numeric::from_i128(10).ln_near_one(guard)
}

/// Result scale for ln/log: 16 significant digits, i.e. `16 - max(0, weight)`
/// of the computed logarithm, clamped to `0..=MAX_DSCALE`.
///
/// TODO: floor the result scale at the *input's* display scale, which this
/// helper never sees — PG's `ln(1.2345678e-28)` prints the input's 35
/// fractional digits, where this asks for 15.
fn log_scale(val: &Numeric) -> i32 {
    let w = if val.is_zero() { 0 } else { val.weight.max(0) };
    (MIN_SIG_DIGITS - w).clamp(0, MAX_DSCALE as i32)
}

/// Result scale for exp/power: `16 - trunc(result's decimal weight)`, floored
/// at 0. `weight_est` is a float estimate of that weight; it may be ±infinite
/// (e.g. a base that overflows f64), so the clamp is done in f64 before the
/// cast to avoid an i32 overflow.
fn scale_from_estimate(weight_est: f64) -> i32 {
    if weight_est.is_nan() {
        return 0;
    }
    (MIN_SIG_DIGITS as f64 - weight_est).clamp(0.0, MAX_RESULT_SCALE as f64) as i32
}

/// Result scale for `exp(x)`: from the estimated result weight `trunc(x·log10 e)`.
fn exp_scale(x: f64) -> i32 {
    scale_from_estimate((x * std::f64::consts::LOG10_E).trunc())
}

// ---- magnitude arithmetic over (digits, weight) --------------------------

fn cmp_mag(a: &Numeric, b: &Numeric) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    if a.digits.is_empty() || b.digits.is_empty() {
        return a.digits.len().cmp(&b.digits.len());
    }
    if a.weight != b.weight {
        return a.weight.cmp(&b.weight);
    }
    let n = a.digits.len().max(b.digits.len());
    for i in 0..n {
        let da = a.digits.get(i).copied().unwrap_or(0);
        let db = b.digits.get(i).copied().unwrap_or(0);
        match da.cmp(&db) {
            Equal => continue,
            other => return other,
        }
    }
    Equal
}

/// Sum the magnitudes; returns `(digits MSB-first, low position)`.
fn add_mag(a: &Numeric, b: &Numeric) -> (Vec<u8>, i32) {
    let a_low = a.low();
    let b_low = b.low();
    let low = a_low.min(b_low);
    let hi = a.weight.max(b.weight);
    let n = (hi - low + 1) as usize;
    let mut acc = vec![0i32; n];
    for (i, &d) in a.digits.iter().enumerate() {
        let pos = a.weight - i as i32;
        acc[(pos - low) as usize] += d as i32;
    }
    for (i, &d) in b.digits.iter().enumerate() {
        let pos = b.weight - i as i32;
        acc[(pos - low) as usize] += d as i32;
    }
    let mut carry = 0;
    for slot in acc.iter_mut() {
        let v = *slot + carry;
        *slot = v % 10;
        carry = v / 10;
    }
    // Summing two base-10 coefficients leaves at most a single carry digit.
    let mut digits: Vec<u8> = Vec::with_capacity(acc.len() + 1);
    if carry > 0 {
        digits.push(carry as u8);
    }
    digits.extend(acc.iter().rev().map(|&x| x as u8));
    (digits, low)
}

/// Subtract magnitudes assuming `|a| >= |b|`; returns `(digits MSB-first, low)`.
fn sub_mag(a: &Numeric, b: &Numeric) -> (Vec<u8>, i32) {
    let a_low = a.low();
    let b_low = b.low();
    let low = a_low.min(b_low);
    let hi = a.weight.max(b.weight);
    let n = (hi - low + 1) as usize;
    let mut acc = vec![0i32; n];
    for (i, &d) in a.digits.iter().enumerate() {
        acc[(a.weight - i as i32 - low) as usize] += d as i32;
    }
    for (i, &d) in b.digits.iter().enumerate() {
        acc[(b.weight - i as i32 - low) as usize] -= d as i32;
    }
    for j in 0..n - 1 {
        if acc[j] < 0 {
            acc[j] += 10;
            acc[j + 1] -= 1;
        }
    }
    let digits: Vec<u8> = acc.iter().rev().map(|&x| x as u8).collect();
    (digits, low)
}

/// Multiply magnitudes; returns `(digits MSB-first, low position)`.
fn mul_mag(a: &Numeric, b: &Numeric) -> (Vec<u8>, i32) {
    let a_low = a.low();
    let b_low = b.low();
    let low = a_low + b_low;
    let la = a.digits.len();
    let lb = b.digits.len();
    let mut acc = vec![0u64; la + lb];
    for (i, &da) in a.digits.iter().enumerate() {
        let ia = la - 1 - i; // little-endian index within a
        for (j, &db) in b.digits.iter().enumerate() {
            let ib = lb - 1 - j;
            acc[ia + ib] += da as u64 * db as u64;
        }
    }
    let mut carry = 0u64;
    for slot in acc.iter_mut() {
        let v = *slot + carry;
        *slot = v % 10;
        carry = v / 10;
    }
    debug_assert_eq!(carry, 0);
    let digits: Vec<u8> = acc.iter().rev().map(|&x| x as u8).collect();
    (digits, low)
}

// ---- big-endian base-10 integer helpers (for division & sqrt) ------------

fn trim_leading(v: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < v.len() && v[i] == 0 {
        i += 1;
    }
    &v[i..]
}

fn cmp_be(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let a = trim_leading(a);
    let b = trim_leading(b);
    if a.len() != b.len() {
        return a.len().cmp(&b.len());
    }
    a.cmp(b)
}

fn add_be(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len().max(b.len()) + 1);
    let mut carry = 0u8;
    let (mut ia, mut ib) = (a.len(), b.len());
    while ia > 0 || ib > 0 || carry > 0 {
        let da = if ia > 0 {
            ia -= 1;
            a[ia]
        } else {
            0
        };
        let db = if ib > 0 {
            ib -= 1;
            b[ib]
        } else {
            0
        };
        let v = da + db + carry;
        out.push(v % 10);
        carry = v / 10;
    }
    out.reverse();
    out
}

/// `a - b` assuming `a >= b`, big-endian, trimmed.
fn sub_be(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i8;
    let (mut ia, mut ib) = (a.len(), b.len());
    while ia > 0 {
        ia -= 1;
        let da = a[ia] as i8;
        let db = if ib > 0 {
            ib -= 1;
            b[ib] as i8
        } else {
            0
        };
        let mut v = da - db - borrow;
        if v < 0 {
            v += 10;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out.push(v as u8);
    }
    out.reverse();
    let t = trim_leading(&out).to_vec();
    if t.is_empty() { vec![0] } else { t }
}

fn mul_small_be(a: &[u8], m: u8) -> Vec<u8> {
    if m == 0 {
        return vec![0];
    }
    let mut out = Vec::with_capacity(a.len() + 1);
    let mut carry = 0u32;
    for &d in a.iter().rev() {
        let v = d as u32 * m as u32 + carry;
        out.push((v % 10) as u8);
        carry = v / 10;
    }
    while carry > 0 {
        out.push((carry % 10) as u8);
        carry /= 10;
    }
    out.reverse();
    out
}

/// The divisor as a machine word, with `pad` trailing zeros folded in — the
/// scale shift [`Numeric::div_to_scale`] would otherwise append as digits.
///
/// `None` once it no longer fits. `MAX` is the whole safety property, and it is
/// [`divide_by_register`]'s: the running remainder there is always below the
/// divisor, so `rem*10 + digit <= 10*MAX - 1`, and `(u64::MAX - 9) / 10` is the
/// largest bound that keeps that inside a `u64`. Raising it breaks that loop,
/// not this one.
///
/// Zero also returns `None` — but only the schoolbook path *accepts* it, it does
/// not handle it; see the `debug_assert!` in [`Numeric::div_to_scale`].
fn as_register(den: &[u8], pad: usize) -> Option<u64> {
    const MAX: u64 = (u64::MAX - 9) / 10;
    // 10^19 already exceeds MAX, so a wider pad can only be rejected — and
    // returning before the loop keeps this probe off the fallback's back.
    if pad >= 19 {
        return None;
    }
    let mut value: u64 = 0;
    for &d in trim_leading(den) {
        value = value * 10 + d as u64;
        if value > MAX {
            return None;
        }
    }
    for _ in 0..pad {
        value = value * 10;
        if value > MAX {
            return None;
        }
    }
    (value != 0).then_some(value)
}

/// Long division by a register-sized divisor: returns the quotient big-endian
/// (untrimmed — `from_coeff` normalizes) and the remainder as a word. `pad`
/// trailing zeros extend the numerator without materializing them.
fn divide_by_register(num: &[u8], pad: usize, den: u64) -> (Vec<u8>, u64) {
    let mut quotient = Vec::with_capacity(num.len() + pad);
    let mut rem: u64 = 0;
    for d in num.iter().copied().chain(std::iter::repeat_n(0u8, pad)) {
        rem = rem * 10 + d as u64;
        quotient.push((rem / den) as u8);
        rem %= den;
    }
    (quotient, rem)
}

/// `q += 1` in place, big-endian; grows by a digit only on an all-nines carry.
fn inc_be(q: &mut Vec<u8>) {
    for d in q.iter_mut().rev() {
        if *d < 9 {
            *d += 1;
            return;
        }
        *d = 0;
    }
    q.insert(0, 1);
}

/// Schoolbook long division of big-endian base-10 integers: returns
/// `(quotient, remainder)`, both big-endian and trimmed. `den` must be nonzero.
fn long_divide(num: &[u8], den: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let den = trim_leading(den);
    // The ten multiples `den*0 ..= den*9`, computed once and reused for every
    // quotient digit's trial (instead of recomputing `den*q` per digit).
    let multiples: [Vec<u8>; 10] = std::array::from_fn(|q| mul_small_be(den, q as u8));
    let mut quotient = Vec::with_capacity(num.len());
    let mut rem: Vec<u8> = Vec::new();
    for &digit in num {
        // rem = rem*10 + digit
        rem.push(digit);
        let trimmed = trim_leading(&rem).to_vec();
        rem = if trimmed.is_empty() { vec![0] } else { trimmed };
        // Largest q in 0..=9 with den*q <= rem.
        let mut q = 0u8;
        while q < 9 && cmp_be(&multiples[(q + 1) as usize], &rem) != std::cmp::Ordering::Greater {
            q += 1;
        }
        rem = sub_be(&rem, &multiples[q as usize]);
        quotient.push(q);
    }
    let qt = trim_leading(&quotient).to_vec();
    let rt = trim_leading(&rem).to_vec();
    (
        if qt.is_empty() { vec![0] } else { qt },
        if rt.is_empty() { vec![0] } else { rt },
    )
}

/// Integer square root of a big-endian base-10 integer: returns
/// `(floor(sqrt(n)), n - floor^2)`, both trimmed big-endian. Uses digit-pair
/// long-hand square-root extraction (exact, no floating point).
fn isqrt_be(n: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let n = trim_leading(n);
    if n.is_empty() {
        return (vec![0], vec![0]);
    }
    // Pad to an even number of digits so we can consume two at a time.
    let mut digits = n.to_vec();
    if digits.len() % 2 == 1 {
        digits.insert(0, 0);
    }
    let mut root: Vec<u8> = Vec::new(); // running root, big-endian, trimmed
    let mut rem: Vec<u8> = vec![0]; // running remainder
    let mut i = 0;
    while i < digits.len() {
        // current = rem*100 + next two digits
        let mut current = mul_small_be(&rem, 10);
        current = mul_small_be(&current, 10);
        current = add_be(&current, &[digits[i], digits[i + 1]]);
        i += 2;
        // Find largest d in 0..=9 with (20*root + d)*d <= current.
        let twenty_root = mul_small_be(trim_leading(&root), 20);
        let mut d = 0u8;
        while d < 9 {
            let cand = mul_small_be(&add_be(&twenty_root, &[d + 1]), d + 1);
            if cmp_be(&cand, &current) == std::cmp::Ordering::Greater {
                break;
            }
            d += 1;
        }
        let subtrahend = mul_small_be(&add_be(&twenty_root, &[d]), d);
        rem = sub_be(&current, &subtrahend);
        root.push(d);
    }
    let rt = trim_leading(&root).to_vec();
    let remt = trim_leading(&rem).to_vec();
    (
        if rt.is_empty() { vec![0] } else { rt },
        if remt.is_empty() { vec![0] } else { remt },
    )
}

/// Expand Rust scientific notation (`-6.66e-1`) into a plain decimal string,
/// stripping insignificant trailing zeros. Used by float→numeric.
fn sci_to_plain(sci: &str) -> String {
    let (mantissa, exp) = sci.split_once('e').expect("scientific notation");
    let exp: i32 = exp.parse().expect("exponent");
    let neg = mantissa.starts_with('-');
    let mantissa = mantissa.trim_start_matches('-');
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let mut digits: String = int_part.chars().chain(frac_part.chars()).collect();
    let point = int_part.len() as i32 + exp;
    let keep = digits.trim_end_matches('0').len().max(1);
    digits.truncate(keep);
    let out = if point <= 0 {
        format!("0.{}{}", "0".repeat((-point) as usize), digits)
    } else if point as usize >= digits.len() {
        format!("{}{}", digits, "0".repeat(point as usize - digits.len()))
    } else {
        let p = point as usize;
        format!("{}.{}", &digits[..p], &digits[p..])
    };
    if neg { format!("-{out}") } else { out }
}

/// The `numeric field overflow` error PG raises when a value does not fit a
/// `numeric(p,s)` constraint.
fn field_overflow(precision: i32, scale: i32, infinite: bool) -> NumErr {
    let detail = if infinite {
        format!("A field with precision {precision}, scale {scale} cannot hold an infinite value.")
    } else {
        let max_int = precision - scale;
        let bound = if max_int == 0 {
            "1".to_string()
        } else {
            format!("10^{max_int}")
        };
        format!(
            "A field with precision {precision}, scale {scale} must round to an absolute value less than {bound}."
        )
    };
    NumErr {
        sqlstate: "22003",
        message: "numeric field overflow".to_string(),
        detail: Some(detail),
    }
}

impl PartialEq for Numeric {
    fn eq(&self, other: &Numeric) -> bool {
        // NaN equals NaN in PG's numeric ordering, so a total-order equality is
        // the right notion here (used by tests and by value comparisons).
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl std::hash::Hash for Numeric {
    /// Value-canonical hashing, consistent with [`Numeric::cmp`] / `PartialEq`:
    /// two equal numerics of different display scale (`1.0` and `1.00`) hash
    /// equal, since `digits`/`weight` are stripped of trailing zeros and
    /// `dscale` is display-only. Used to hash `jsonb` numbers for grouping.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self.sign {
            Sign::NaN => 0u8.hash(state),
            Sign::PInf => 1u8.hash(state),
            Sign::NInf => 2u8.hash(state),
            Sign::Pos | Sign::Neg => {
                // Zero normalizes to `Pos` with empty `digits` and `weight` 0, so
                // its hash is fixed regardless of how it was written.
                3u8.hash(state);
                self.is_neg().hash(state);
                self.weight.hash(state);
                self.digits.hash(state);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Numeric {
        match Numeric::parse(s) {
            Ok(value) => value,
            Err(error) => panic!("invalid numeric test fixture `{s}`: {error:?}"),
        }
    }
    fn disp(s: &str) -> String {
        n(s).to_display()
    }

    #[test]
    fn parse_and_display_roundtrip() {
        for s in [
            "0",
            "1",
            "-1",
            "1.5",
            "100",
            "0.05",
            "40.500000",
            "-0.0",
            "12345.6789",
        ] {
            let d = disp(s);
            let expect = if s == "-0.0" { "0.0" } else { s };
            assert_eq!(d, expect, "input {s}");
        }
        assert_eq!(disp("1.50e1"), "15.0");
        assert_eq!(disp("1e3"), "1000");
        assert_eq!(disp(".5"), "0.5");
        assert_eq!(disp("  42 "), "42");
        assert_eq!(disp("NaN"), "NaN");
        assert_eq!(disp("inf"), "Infinity");
        assert_eq!(disp("-Infinity"), "-Infinity");
    }

    #[test]
    fn parse_rejects_and_overflows() {
        assert_eq!(Numeric::parse("abc"), Err(ParseError::Syntax));
        assert_eq!(Numeric::parse(""), Err(ParseError::Syntax));
        assert_eq!(Numeric::parse("1e"), Err(ParseError::Syntax));
        assert_eq!(Numeric::parse("1.2.3"), Err(ParseError::Syntax));
        assert_eq!(Numeric::parse("1e2147483647"), Err(ParseError::Overflow));
        assert_eq!(Numeric::parse("1e131072"), Err(ParseError::Overflow));
        assert!(Numeric::parse("9e131071").is_ok());
        assert_eq!(Numeric::parse("1e-16384"), Err(ParseError::Overflow));
        assert!(Numeric::parse("1e-16383").is_ok());
        assert_eq!(
            Numeric::parse("1e99999999999999999999"),
            Err(ParseError::Overflow)
        );
        assert_eq!(Numeric::parse("0e2000000000"), Err(ParseError::Overflow));
    }

    /// The form feed is what separates this set from ASCII whitespace, and the
    /// vertical tab what separates it from the six characters
    /// [`crate::intlit`] trims — easy to lose to a stock trim that differs.
    #[test]
    fn parse_trims_its_own_whitespace_set() {
        for sep in [" ", "\t", "\n", "\r"] {
            assert_eq!(
                Numeric::parse(&format!("{sep}42{sep}")).map(|v| v.to_display()),
                Ok("42".to_string()),
                "sep {sep:?}"
            );
        }
        for sep in ["\x0b", "\x0c", "\u{a0}"] {
            assert_eq!(
                Numeric::parse(&format!("{sep}42")),
                Err(ParseError::Syntax),
                "sep {sep:?}"
            );
        }
    }

    fn arith(a: &str, op: char, b: &str) -> String {
        let (x, y) = (n(a), n(b));
        match op {
            '+' => x.add(&y),
            '-' => x.sub(&y),
            '*' => x.mul(&y),
            '/' => match x.div(&y) {
                Ok(value) => value,
                Err(error) => panic!("numeric division fixture failed: {error:?}"),
            },
            '%' => match x.modulo(&y) {
                Ok(value) => value,
                Err(error) => panic!("numeric modulo fixture failed: {error:?}"),
            },
            _ => unreachable!(),
        }
        .to_display()
    }

    #[test]
    fn addition_subtraction_scale() {
        assert_eq!(arith("0", '*', "4.2"), "0.0");
        assert_eq!(arith("4.2", '-', "4.2"), "0.0");
        assert_eq!(arith("1.5", '+', "2.25"), "3.75");
        assert_eq!(arith("1", '+', "2"), "3");
        assert_eq!(arith("100", '-', "1"), "99");
        assert_eq!(arith("-5", '+', "3"), "-2");
        assert_eq!(arith("0.1", '+', "0.2"), "0.3");
    }

    #[test]
    fn multiplication_scale() {
        assert_eq!(arith("1.10", '*', "1.10"), "1.2100");
        assert_eq!(arith("2.5", '*', "2"), "5.0");
        assert_eq!(arith("-3", '*', "4"), "-12");
        assert_eq!(arith("12345", '*', "0"), "0");
    }

    #[test]
    fn division_scale_matches_pg() {
        assert_eq!(arith("1", '/', "3"), "0.33333333333333333333");
        assert_eq!(arith("4.2", '/', "4.2"), "1.00000000000000000000");
        assert_eq!(arith("10", '/', "3"), "3.3333333333333333");
        assert_eq!(arith("2", '/', "3"), "0.66666666666666666667");
        assert_eq!(arith("1000", '/', "3"), "333.3333333333333333");
        assert_eq!(arith("1", '/', "30000"), "0.000033333333333333333333");
        assert_eq!(arith("6", '/', "2"), "3.0000000000000000");
    }

    /// The register divider is only ever taken for a divisor small enough to
    /// fit, and it has to be indistinguishable from the schoolbook loop it
    /// replaces — otherwise a quotient digit or the rounding remainder moves and
    /// every `avg` shifts with it. Compare the two directly over random
    /// operands, since which one runs is an internal choice callers cannot see.
    #[test]
    fn the_register_divider_agrees_with_schoolbook() {
        // A xorshift keeps the operands reproducible without a dev-dependency.
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let digits = |v: u64| -> Vec<u8> { v.to_string().bytes().map(|b| b - b'0').collect() };
        let trim = |v: &[u8]| {
            let t = trim_leading(v);
            if t.is_empty() { vec![0] } else { t.to_vec() }
        };

        for case in 0..2_000 {
            let num: Vec<u8> = (0..=(next() % 40)).map(|_| (next() % 10) as u8).collect();
            let pad = (next() % 22) as usize;
            // Span the whole register range, including the bound itself.
            let den = match case % 4 {
                0 => next() % 10 + 1,
                1 => next() % 100_000 + 1,
                2 => (u64::MAX - 9) / 10,
                _ => next() % (u64::MAX / 16) + 1,
            };

            let (q_reg, rem_reg) = divide_by_register(&num, pad, den);

            let mut padded = num.clone();
            padded.extend(std::iter::repeat_n(0u8, pad));
            let (q_school, rem_school) = long_divide(&padded, &digits(den));

            assert_eq!(
                trim(&q_reg),
                q_school,
                "quotient diverged for {num:?} padded by {pad} / {den}"
            );
            assert_eq!(
                trim(&digits(rem_reg)),
                rem_school,
                "remainder diverged for {num:?} padded by {pad} / {den}"
            );
        }
    }

    /// The register path must decline exactly when `rem*10 + digit` would stop
    /// fitting, and fold the scale shift into the divisor rather than the digits.
    #[test]
    fn the_register_bound_is_where_the_running_remainder_stops_fitting() {
        const MAX: u64 = (u64::MAX - 9) / 10;
        assert_eq!(as_register(&[4], 0), Some(4));
        assert_eq!(as_register(&[0, 0, 4], 0), Some(4), "leading zeros trimmed");
        assert_eq!(as_register(&[4], 2), Some(400), "pad multiplies by ten");
        assert_eq!(as_register(&[0], 0), None, "zero is left to schoolbook");
        assert_eq!(as_register(&[], 0), None);

        let max_digits: Vec<u8> = MAX.to_string().bytes().map(|b| b - b'0').collect();
        assert_eq!(
            as_register(&max_digits, 0),
            Some(MAX),
            "the bound itself fits"
        );
        assert_eq!(
            as_register(&max_digits, 1),
            None,
            "one decade past it does not"
        );
    }

    /// A helper-level test: the register path cannot actually reach the growth
    /// carry (an all-nines quotient needs `den == 1`, which makes every
    /// remainder 0, so the rounding guard never fires), but `inc_be` is written
    /// as a general increment and is pinned as one.
    #[test]
    fn incrementing_carries_across_all_nines() {
        let mut q = vec![1, 2, 8];
        inc_be(&mut q);
        assert_eq!(q, vec![1, 2, 9]);

        let mut q = vec![0, 9, 9];
        inc_be(&mut q);
        assert_eq!(q, vec![1, 0, 0], "carry stops at a leading zero");

        let mut q = vec![9, 9];
        inc_be(&mut q);
        assert_eq!(q, vec![1, 0, 0], "an all-nines carry grows the quotient");

        let mut q = Vec::new();
        inc_be(&mut q);
        assert_eq!(q, vec![1]);
    }

    #[test]
    fn modulo_matches_pg() {
        assert_eq!(arith("5.0", '%', "2"), "1.0");
        assert_eq!(arith("-5.5", '%', "2"), "-1.5");
        assert_eq!(arith("11", '%', "4"), "3");
    }

    #[test]
    fn division_by_zero_errors() {
        assert_eq!(
            n("1")
                .div(&n("0"))
                .expect_err("dividing a finite numeric by zero is rejected")
                .sqlstate,
            "22012"
        );
        assert_eq!(
            n("1")
                .modulo(&n("0"))
                .expect_err("taking a finite numeric modulo zero is rejected")
                .sqlstate,
            "22012"
        );
    }

    #[test]
    fn special_value_div_and_mod() -> anyhow::Result<()> {
        // inf/inf = NaN; nan/0 = NaN; inf/0 and inf%0 = division by zero.
        assert_eq!(n("Infinity").div(&n("Infinity"))?.to_display(), "NaN");
        assert_eq!(n("NaN").div(&n("0"))?.to_display(), "NaN");
        assert_eq!(
            n("Infinity")
                .div(&n("0"))
                .expect_err("dividing infinity by zero is division by zero, not NaN")
                .sqlstate,
            "22012"
        );
        assert_eq!(
            n("Infinity")
                .modulo(&n("0"))
                .expect_err("infinity modulo zero is division by zero, not NaN")
                .sqlstate,
            "22012"
        );
        assert_eq!(n("Infinity").div(&n("2"))?.to_display(), "Infinity");
        assert_eq!(n("-Infinity").div(&n("2"))?.to_display(), "-Infinity");
        assert_eq!(n("2").div(&n("Infinity"))?.to_display(), "0");
        assert_eq!(n("Infinity").modulo(&n("2"))?.to_display(), "NaN");

        Ok(())
    }

    #[test]
    fn power_special_values() -> anyhow::Result<()> {
        assert_eq!(n("0.5").power(&n("-Infinity"))?.to_display(), "Infinity");
        assert_eq!(n("1").power(&n("Infinity"))?.to_display(), "1");
        assert_eq!(n("Infinity").power(&n("2"))?.to_display(), "Infinity");
        assert_eq!(n("2").power(&n("Infinity"))?.to_display(), "Infinity");
        assert_eq!(n("2").power(&n("-Infinity"))?.to_display(), "0");

        Ok(())
    }

    #[test]
    fn arithmetic_and_rounding_edge_cases() -> anyhow::Result<()> {
        // Rounding that carries into a new leading digit.
        assert_eq!(n("9.99").round(1).to_display(), "10.0");
        assert_eq!(n("0.0099").round(2).to_display(), "0.01");
        assert_eq!(n("99.99").round(0).to_display(), "100");
        // Subtraction chaining borrows across many digits.
        assert_eq!(arith("1000000", '-', "999999.0"), "1.0");
        assert_eq!(arith("100", '-', "99.9999"), "0.0001");
        // Long division with a multi-digit / internal-zero divisor.
        assert_eq!(arith("100070", '/', "1007"), "99.3743793445878848");
        // Perfect-square sqrt is exact (scale from the input's magnitude).
        assert_eq!(n("152399025").sqrt()?.to_display(), "12345.00000000000");
        // Big multiplication stays exact.
        assert_eq!(
            arith("12345678901234567890", '*', "98765432109876543210"),
            "1219326311370217952237463801111263526900"
        );

        Ok(())
    }

    #[test]
    fn parse_extreme_exponent_does_not_panic() {
        // Exponents at the i64 boundary must classify as overflow, not panic.
        assert_eq!(
            Numeric::parse("1e9223372036854775807"),
            Err(ParseError::Overflow)
        );
        assert_eq!(
            Numeric::parse("1e-9223372036854775808"),
            Err(ParseError::Overflow)
        );
    }

    #[test]
    fn rounding_and_truncation() {
        assert_eq!(n("1.5").round(3).to_display(), "1.500");
        assert_eq!(n("2.5").round(0).to_display(), "3");
        assert_eq!(n("-2.5").round(0).to_display(), "-3");
        assert_eq!(n("1234.5678").round(-2).to_display(), "1200");
        assert_eq!(n("1.6").trunc(0).to_display(), "1");
        assert_eq!(n("-1.6").trunc(0).to_display(), "-1");
        assert_eq!(n("-2.3").signum().to_display(), "-1");
        assert_eq!(n("-1.5").ceil().to_display(), "-1");
        assert_eq!(n("-1.5").floor().to_display(), "-2");
        assert_eq!(n("1.5").ceil().to_display(), "2");
        assert_eq!(n("-1.50").abs().to_display(), "1.50");
    }

    #[test]
    fn comparison_order() {
        use std::cmp::Ordering::*;
        assert_eq!(n("1").cmp(&n("2")), Less);
        assert_eq!(n("2.0").cmp(&n("2")), Equal);
        assert_eq!(n("-1").cmp(&n("1")), Less);
        assert_eq!(n("10").cmp(&n("9")), Greater);
        assert_eq!(n("NaN").cmp(&n("NaN")), Equal);
        assert_eq!(n("NaN").cmp(&n("1")), Greater);
        assert_eq!(n("Infinity").cmp(&n("1e100")), Greater);
        assert_eq!(n("-Infinity").cmp(&n("-1e100")), Less);
    }

    #[test]
    fn typmod_overflow_and_ok() -> anyhow::Result<()> {
        assert_eq!(n("0.99994").apply_typmod(4, 4)?.to_display(), "0.9999");
        let e = n("0.99995")
            .apply_typmod(4, 4)
            .expect_err("0.99995 rounds up to 1.0000, which numeric(4,4) cannot hold");
        assert_eq!(e.sqlstate, "22003");
        assert_eq!(
            e.detail
                .ok_or_else(|| anyhow::anyhow!("typmod detail is missing"))?,
            "A field with precision 4, scale 4 must round to an absolute value less than 1."
        );
        let e = n("12345.6")
            .apply_typmod(5, 2)
            .expect_err("12345.6 needs five integer digits, more than numeric(5,2) allows");
        assert_eq!(
            e.detail
                .ok_or_else(|| anyhow::anyhow!("typmod detail is missing"))?,
            "A field with precision 5, scale 2 must round to an absolute value less than 10^3."
        );
        assert_eq!(n("1.005").apply_typmod(5, 2)?.to_display(), "1.01");
        let e = Numeric::pos_inf()
            .apply_typmod(4, 4)
            .expect_err("infinity cannot be held by a numeric with a declared precision");
        assert_eq!(
            e.detail
                .ok_or_else(|| anyhow::anyhow!("typmod detail is missing"))?,
            "A field with precision 4, scale 4 cannot hold an infinite value."
        );
        assert!(Numeric::nan().apply_typmod(4, 4)?.is_nan());

        Ok(())
    }

    /// A typmod whose scale exceeds its precision, which PostgreSQL has allowed
    /// since 15: `numeric(2,5)` holds values below `10^-3`, so the bound on the
    /// integer side is a *negative* exponent rather than a digit count.
    ///
    /// Every expectation below is PostgreSQL 18.4's own output.
    #[test]
    fn a_scale_past_the_precision_bounds_by_a_negative_exponent() -> anyhow::Result<()> {
        assert_eq!(n("0.00001").apply_typmod(2, 5)?.to_display(), "0.00001");
        // Rounded up to the declared scale, and still below the bound.
        assert_eq!(n("0.0001").apply_typmod(2, 5)?.to_display(), "0.00010");
        // Zero is below every bound, negative exponents included.
        assert_eq!(n("0").apply_typmod(2, 5)?.to_display(), "0.00000");

        let e = n("0.001")
            .apply_typmod(2, 5)
            .expect_err("0.001 is not below 10^-3");
        assert_eq!(e.sqlstate, "22003");
        assert_eq!(
            e.detail
                .ok_or_else(|| anyhow::anyhow!("typmod detail is missing"))?,
            "A field with precision 2, scale 5 must round to an absolute value less than 10^-3."
        );
        Ok(())
    }

    /// The mirror case: a negative scale rounds to the left of the point, so the
    /// bound is a *larger* power of ten than the precision alone would suggest.
    #[test]
    fn a_negative_scale_rounds_left_of_the_point() -> anyhow::Result<()> {
        assert_eq!(n("12345").apply_typmod(4, -2)?.to_display(), "12300");
        let e = n("1234567")
            .apply_typmod(4, -2)
            .expect_err("1234567 rounds to 1234600, which is not below 10^6");
        assert_eq!(
            e.detail
                .ok_or_else(|| anyhow::anyhow!("typmod detail is missing"))?,
            "A field with precision 4, scale -2 must round to an absolute value less than 10^6."
        );
        Ok(())
    }

    #[test]
    fn integer_and_float_conversions() {
        assert_eq!(Numeric::from_i128(-9).to_display(), "-9");
        assert_eq!(Numeric::from_i128(0).to_display(), "0");
        assert_eq!(n("2.5").to_i128(), Some(3));
        assert_eq!(n("-2.5").to_i128(), Some(3i128.wrapping_neg()));
        assert_eq!(n("2.4").to_i128(), Some(2));
        assert_eq!(n("1000").to_i128(), Some(1000));
        assert!((n("1.5").to_f64() - 1.5).abs() < 1e-12);
        assert_eq!(n("Infinity").to_f64(), f64::INFINITY);
        assert_eq!(Numeric::from_f64_sig(1.5, 15).to_display(), "1.5");
        assert_eq!(Numeric::from_f64_sig(100.0, 15).to_display(), "100");
        assert_eq!(
            Numeric::from_f64_sig(2.0 / 3.0, 15).to_display(),
            "0.666666666666667"
        );
    }

    #[test]
    fn ln_matches_pg() -> anyhow::Result<()> {
        assert_eq!(n("2").ln()?.to_display(), "0.6931471805599453");
        assert_eq!(n("20").ln()?.to_display(), "2.9957322735539910");
        assert_eq!(n("200").ln()?.to_display(), "5.2983173665480367");
        assert_eq!(n("0.5").ln()?.to_display(), "-0.6931471805599453");
        assert_eq!(n("0.02").ln()?.to_display(), "-3.9120230054281461");
        assert_eq!(n("1.5").ln()?.to_display(), "0.4054651081081644");
        assert_eq!(n("100").ln()?.to_display(), "4.6051701859880914");
        assert_eq!(n("1e50").ln()?.to_display(), "115.12925464970228");
        assert_eq!(n("1e100").ln()?.to_display(), "230.25850929940457");
        assert_eq!(
            n("0")
                .ln()
                .expect_err("the logarithm of zero is undefined")
                .sqlstate,
            "2201E"
        );
        assert_eq!(
            n("-1")
                .ln()
                .expect_err("the logarithm of a negative number is undefined")
                .sqlstate,
            "2201E"
        );

        Ok(())
    }

    #[test]
    fn ln_of_a_tiny_value_converges() -> anyhow::Result<()> {
        // The square-root reduction works at an absolute scale: without the
        // magnitude taken out first, `sqrt(1.234567e-89)` rounds to zero, the
        // reduction never terminates, the series diverges to its term cap and
        // the answer comes back as `-7037630508829274846715832734222`.
        //
        // The digits are PostgreSQL's. The *scale* is not: PG carries the
        // input's own 95 fractional digits into the result and prints 98, where
        // `log_scale` here asks only for 16 significant digits. That gap is
        // older than this test and is a display-scale question, not a
        // convergence one.
        assert_eq!(
            n("1.234567e-89").log10()?.to_display(),
            "-88.908485335913737"
        );
        assert_eq!(n("1.234567e-89").ln()?.to_display(), "-204.71935297515468");
        assert_eq!(n("1e-300").log10()?.to_display(), "-300.00000000000000");
        Ok(())
    }

    #[test]
    fn log_base_resolves_the_infinities_as_limits() -> anyhow::Result<()> {
        // Without these the infinite operand reaches the series, which has no
        // finite sum for it: it ran to the term cap and returned a 32-digit
        // integer after a second and a half.
        let inf = n("inf");
        assert_eq!(inf.log_base(&n("4.2"))?.to_display(), "0");
        assert_eq!(n("4.2").log_base(&inf)?.to_display(), "Infinity");
        // ∞/∞ is indeterminate — NaN, not 1.
        assert_eq!(inf.log_base(&inf)?.to_display(), "NaN");
        assert_eq!(n("nan").log_base(&inf)?.to_display(), "NaN");
        Ok(())
    }

    #[test]
    fn exp_matches_pg() -> anyhow::Result<()> {
        assert_eq!(n("1").exp()?.to_display(), "2.7182818284590452");
        assert_eq!(n("2").exp()?.to_display(), "7.3890560989306502");
        assert_eq!(n("0.5").exp()?.to_display(), "1.6487212707001281");
        assert_eq!(n("10").exp()?.to_display(), "22026.465794806717");
        assert_eq!(n("0.001").exp()?.to_display(), "1.0010005001667083");
        assert_eq!(n("-1").exp()?.to_display(), "0.3678794411714423");
        assert_eq!(n("-10").exp()?.to_display(), "0.00004539992976248485");

        Ok(())
    }

    #[test]
    fn log_matches_pg() -> anyhow::Result<()> {
        assert_eq!(n("100").log10()?.to_display(), "2.0000000000000000");
        assert_eq!(n("2").log10()?.to_display(), "0.3010299956639812");
        assert_eq!(n("1000").log10()?.to_display(), "3.0000000000000000");
        assert_eq!(n("0.5").log10()?.to_display(), "-0.3010299956639812");
        assert_eq!(n("1e20").log10()?.to_display(), "20.000000000000000");
        assert_eq!(n("2").log_base(&n("8"))?.to_display(), "3.0000000000000000");

        // Base 1 is `ln(x)/0`, and PG reports the division rather than a
        // logarithm domain error — the `2201E` cases above keep their own text.
        let e = n("1")
            .log_base(&n("12.34"))
            .expect_err("base 1 has no logarithm");
        assert_eq!(
            (e.sqlstate, e.message.as_str()),
            ("22012", "division by zero")
        );

        // But a base merely *near* one still computes: the guard has to key on
        // `ln(base)` rounding to zero, not on the base being 1.
        assert_eq!(
            n("1.000016").log_base(&n("8.452010e18"))?.to_display(),
            "2723830.2877097365"
        );

        Ok(())
    }

    #[test]
    fn power_matches_pg() -> anyhow::Result<()> {
        assert_eq!(n("2").power(&n("10"))?.to_display(), "1024.0000000000000");
        assert_eq!(
            n("2.0").power(&n("0.5"))?.to_display(),
            "1.4142135623730950"
        );
        assert_eq!(n("10").power(&n("3"))?.to_display(), "1000.0000000000000");
        assert_eq!(n("2").power(&n("0.5"))?.to_display(), "1.4142135623730950");
        assert_eq!(n("3").power(&n("3"))?.to_display(), "27.000000000000000");
        assert_eq!(n("1.5").power(&n("2"))?.to_display(), "2.2500000000000000");
        assert_eq!(
            n("2").power(&n("100"))?.to_display(),
            "1267650600228229401496703205376"
        );
        assert_eq!(
            n("2").power(&n("-10"))?.to_display(),
            "0.0009765625000000000"
        );
        assert_eq!(n("-2").power(&n("3"))?.to_display(), "-8.0000000000000000");
        // Special-case errors.
        assert_eq!(
            n("0")
                .power(&n("-1"))
                .expect_err("zero raised to a negative exponent is undefined")
                .sqlstate,
            "2201F"
        );
        assert_eq!(
            n("-2")
                .power(&n("0.5"))
                .expect_err("a negative base with a fractional exponent has no real result")
                .sqlstate,
            "2201F"
        );
        assert_eq!(n("0").power(&n("0"))?.to_display(), "1");

        Ok(())
    }

    /// Integer exponents big enough that exact repeated squaring would never
    /// finish. Each of these used to run until it was killed; the estimate and
    /// the bounded working precision decide them in constant time.
    #[test]
    fn extreme_integer_powers_terminate() -> anyhow::Result<()> {
        // Overflows by ten orders of magnitude — decided without multiplying.
        assert_eq!(
            n("117743296169.0")
                .power(&n("1000000000"))
                .expect_err("117743296169.0^1000000000 overflows the numeric exponent range")
                .sqlstate,
            "22003"
        );
        assert_eq!(
            n("10.0")
                .power(&n("2147483647"))
                .expect_err("10.0^2147483647 overflows the numeric exponent range")
                .sqlstate,
            "22003"
        );
        // Underflows past the result scale, which PG caps at 1000 digits.
        let tiny = n("10.0").power(&n("-2147483648"))?.to_display();
        assert_eq!(tiny, format!("0.{}", "0".repeat(1000)));
        // Neither extreme: a base so close to 1 that the result is ordinary,
        // reached in 31 squarings held to a few dozen significant digits.
        assert_eq!(
            n("1.000000000123").power(&n("-2147483648"))?.to_display(),
            "0.7678656556403084"
        );
        // Bounded precision must not cost exactness where the result is exact.
        assert_eq!(
            n("2.0").power(&n("100"))?.to_display(),
            "1267650600228229401496703205376.0"
        );
        Ok(())
    }

    #[test]
    fn sqrt_matches_pg() -> anyhow::Result<()> {
        assert_eq!(n("2").sqrt()?.to_display(), "1.414213562373095");
        assert_eq!(n("9").sqrt()?.to_display(), "3.000000000000000");
        assert_eq!(n("100").sqrt()?.to_display(), "10.000000000000000");
        assert_eq!(n("0.04").sqrt()?.to_display(), "0.20000000000000000");
        assert_eq!(n("2000000").sqrt()?.to_display(), "1414.2135623730950");
        assert_eq!(n("2.0").sqrt()?.to_display(), "1.414213562373095");
        assert_eq!(n("0").sqrt()?.to_display(), "0.000000000000000");
        assert_eq!(
            n("-1")
                .sqrt()
                .expect_err("the square root of a negative number has no real result")
                .sqlstate,
            "2201F"
        );

        Ok(())
    }
}
