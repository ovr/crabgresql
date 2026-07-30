//! `interval`: parsing, output, arithmetic, the justify/age/make constructors,
//! and the field functions (`date_part`/`extract`/`date_trunc`).
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior — the `postgres`-style output, the field values, and the
//! SQLSTATE/message of range and syntax errors — pinned by differential tests
//! against real PG (18.4), implemented independently.
//!
//! Representation: PG's three-field split, held as `{ months, days, usec }`.
//! A month is a calendar month, a day is a calendar day, and `usec` is the
//! sub-day time in microseconds; the three are intentionally *not* normalized
//! against one another (that is what `justify_*` is for). `±infinity` are the
//! sentinels PG uses (all three fields at their `i32`/`i64` extreme), so the
//! natural field order already places `-infinity < finite < +infinity`.

use std::cmp::Ordering;

use crate::Numeric;

// SQLSTATEs, kept as literals (the types crate has no dependency on the
// protocol crate; the binder/executor map these to `sqlstate::*`).
const INVALID_DATETIME_FORMAT: &str = "22007";
const DATETIME_FIELD_OVERFLOW: &str = "22008";
const INTERVAL_FIELD_OVERFLOW: &str = "22015";
const INVALID_PARAMETER_VALUE: &str = "22023";
const DIVISION_BY_ZERO: &str = "22012";

const USECS_PER_DAY: i64 = 86_400_000_000;
const USECS_PER_HOUR: i64 = 3_600_000_000;
const USECS_PER_MINUTE: i64 = 60_000_000;
const USECS_PER_SEC: i64 = 1_000_000;
const SECS_PER_DAY: i64 = 86_400;
const DAYS_PER_MONTH: i64 = 30;
const MONTHS_PER_YEAR: i64 = 12;
/// Average days per year PG uses when reducing months to an epoch (the
/// Gregorian mean year, 365.25 days).
const DAYS_PER_YEAR: f64 = 365.25;

/// An interval value: `months` and `days` are calendar counts, `usec` is the
/// sub-day time in microseconds.
#[derive(deepsize::DeepSizeOf, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub usec: i64,
}

/// `+infinity` / `-infinity` sentinels, matching PG's `INTERVAL_NOEND` /
/// `INTERVAL_NOBEGIN` (every field at its extreme).
pub const POS_INFINITY: Interval = Interval {
    months: i32::MAX,
    days: i32::MAX,
    usec: i64::MAX,
};
pub const NEG_INFINITY: Interval = Interval {
    months: i32::MIN,
    days: i32::MIN,
    usec: i64::MIN,
};

impl Interval {
    pub fn is_finite(self) -> bool {
        self != POS_INFINITY && self != NEG_INFINITY
    }

    /// `Some(1)` for `+infinity`, `Some(-1)` for `-infinity`, `None` if finite.
    pub(crate) fn infinity_sign(self) -> Option<i32> {
        if self == POS_INFINITY {
            Some(1)
        } else if self == NEG_INFINITY {
            Some(-1)
        } else {
            None
        }
    }
}

/// A parse/range/unit error, carrying the SQLSTATE and message PG reports.
#[derive(Clone, Debug, PartialEq)]
pub struct IntervalError {
    pub sqlstate: &'static str,
    pub message: String,
}

fn invalid_syntax(input: &str) -> IntervalError {
    IntervalError {
        sqlstate: INVALID_DATETIME_FORMAT,
        message: format!("invalid input syntax for type interval: \"{input}\""),
    }
}

fn out_of_range() -> IntervalError {
    IntervalError {
        sqlstate: DATETIME_FIELD_OVERFLOW,
        message: "interval out of range".to_string(),
    }
}

fn units_not_recognized(unit: &str) -> IntervalError {
    // A datetime unit that exists but has no meaning for an interval (`dow`,
    // `doy`, ...) is "not supported"; a wholly unknown word is "not recognized".
    let verb = if NOT_SUPPORTED_UNITS.contains(&unit) {
        "not supported"
    } else {
        "not recognized"
    };
    IntervalError {
        sqlstate: INVALID_PARAMETER_VALUE,
        message: format!("unit \"{unit}\" {verb} for type interval"),
    }
}

fn field_value_out_of_range(input: &str) -> IntervalError {
    IntervalError {
        sqlstate: INTERVAL_FIELD_OVERFLOW,
        message: format!("interval field value out of range: \"{input}\""),
    }
}

/// Datetime units PG knows but that carry no meaning for an interval; these get
/// "not supported" rather than "not recognized".
const NOT_SUPPORTED_UNITS: &[&str] = &[
    "dow",
    "isodow",
    "doy",
    "isoyear",
    "julian",
    "timezone",
    "timezone_hour",
    "timezone_minute",
];

fn division_by_zero() -> IntervalError {
    IntervalError {
        sqlstate: DIVISION_BY_ZERO,
        message: "division by zero".to_string(),
    }
}

// --- output (interval_out, IntervalStyle = postgres) -----------------------

/// `interval_out` at the default (`postgres`) IntervalStyle.
pub fn format(iv: Interval) -> String {
    if iv == POS_INFINITY {
        return "infinity".to_string();
    }
    if iv == NEG_INFINITY {
        return "-infinity".to_string();
    }
    let mut out = String::new();
    let mut is_zero = true;
    let mut is_before = false;
    let year = (iv.months / 12) as i64;
    let mon = (iv.months % 12) as i64;
    add_postgres_part(&mut out, year, "year", &mut is_zero, &mut is_before);
    add_postgres_part(&mut out, mon, "mon", &mut is_zero, &mut is_before);
    add_postgres_part(
        &mut out,
        iv.days as i64,
        "day",
        &mut is_zero,
        &mut is_before,
    );

    let (hour, min, sec, fsec) = split_time(iv.usec);
    if is_zero || hour != 0 || min != 0 || sec != 0 || fsec != 0 {
        let minus = hour < 0 || min < 0 || sec < 0 || fsec < 0;
        if !is_zero {
            out.push(' ');
        }
        if minus {
            out.push('-');
        } else if is_before {
            out.push('+');
        }
        out.push_str(&format!("{:02}:{:02}:", hour.abs(), min.abs()));
        append_seconds(&mut out, sec.abs(), fsec.abs());
    }
    out
}

/// Append one leading field (year/mon/day) in PostgreSQL's postgres-IntervalStyle
/// output: a separating space once a field has been emitted, an explicit `+` when a
/// prior field was negative but this one is positive, and a plural `s` unless
/// the value is exactly `1`.
fn add_postgres_part(
    out: &mut String,
    value: i64,
    unit: &str,
    is_zero: &mut bool,
    is_before: &mut bool,
) {
    if value == 0 {
        return;
    }
    if !*is_zero {
        out.push(' ');
    }
    if *is_before && value > 0 {
        out.push('+');
    }
    out.push_str(&format!(
        "{value} {unit}{}",
        if value != 1 { "s" } else { "" }
    ));
    *is_before = value < 0;
    *is_zero = false;
}

/// Whole seconds + fractional microseconds → `SS[.ffffff]` with trailing zeros
/// trimmed (matching PostgreSQL's interval seconds output).
fn append_seconds(out: &mut String, sec: i64, fsec: i64) {
    out.push_str(&format!("{sec:02}"));
    if fsec != 0 {
        let frac = format!("{fsec:06}");
        out.push('.');
        out.push_str(frac.trim_end_matches('0'));
    }
}

/// Split a signed microsecond time into `(hour, min, sec, fsec)`. Hours are not
/// capped at 24 (an interval's time part can exceed a day); every component
/// carries the sign of `usec` (C-style truncation toward zero).
pub(crate) fn split_time(usec: i64) -> (i64, i64, i64, i64) {
    let hour = usec / USECS_PER_HOUR;
    let mut r = usec % USECS_PER_HOUR;
    let min = r / USECS_PER_MINUTE;
    r %= USECS_PER_MINUTE;
    let sec = r / USECS_PER_SEC;
    let fsec = r % USECS_PER_SEC;
    (hour, min, sec, fsec)
}

// --- comparison ------------------------------------------------------------

/// Total order over intervals: `-infinity < finite < +infinity`, finite values
/// compared by their canonical microsecond span (30-day months, 24-hour days),
/// as PG's `interval_cmp` does. Uses `i128` so the span never overflows.
pub fn cmp(a: Interval, b: Interval) -> Ordering {
    let rank = |iv: Interval| iv.infinity_sign().unwrap_or(0);
    match rank(a).cmp(&rank(b)) {
        Ordering::Equal if a.is_finite() => canonical_span(a).cmp(&canonical_span(b)),
        other => other,
    }
}

fn canonical_span(iv: Interval) -> i128 {
    iv.usec as i128
        + iv.months as i128 * DAYS_PER_MONTH as i128 * USECS_PER_DAY as i128
        + iv.days as i128 * USECS_PER_DAY as i128
}

// --- arithmetic ------------------------------------------------------------

/// Negate an interval; `±infinity` flips sign.
pub fn negate(iv: Interval) -> Result<Interval, IntervalError> {
    match iv.infinity_sign() {
        Some(1) => Ok(NEG_INFINITY),
        Some(_) => Ok(POS_INFINITY),
        None => Ok(Interval {
            months: iv.months.checked_neg().ok_or_else(out_of_range)?,
            days: iv.days.checked_neg().ok_or_else(out_of_range)?,
            usec: iv.usec.checked_neg().ok_or_else(out_of_range)?,
        }),
    }
}

pub fn add(a: Interval, b: Interval) -> Result<Interval, IntervalError> {
    match (a.infinity_sign(), b.infinity_sign()) {
        (Some(x), Some(y)) => {
            if x == y {
                Ok(if x > 0 { POS_INFINITY } else { NEG_INFINITY })
            } else {
                Err(out_of_range())
            }
        }
        (Some(x), None) | (None, Some(x)) => Ok(if x > 0 { POS_INFINITY } else { NEG_INFINITY }),
        (None, None) => Ok(Interval {
            months: a.months.checked_add(b.months).ok_or_else(out_of_range)?,
            days: a.days.checked_add(b.days).ok_or_else(out_of_range)?,
            usec: a.usec.checked_add(b.usec).ok_or_else(out_of_range)?,
        }),
    }
}

pub fn sub(a: Interval, b: Interval) -> Result<Interval, IntervalError> {
    add(a, negate(b)?)
}

pub fn mul(iv: Interval, factor: f64) -> Result<Interval, IntervalError> {
    if let Some(sign) = iv.infinity_sign() {
        // ±infinity times a nonzero, finite factor stays infinite (sign by the
        // product of signs); times zero (or a non-finite factor) is undefined.
        if factor == 0.0 || !factor.is_finite() {
            return Err(out_of_range());
        }
        let pos = (sign > 0) == (factor > 0.0);
        return Ok(if pos { POS_INFINITY } else { NEG_INFINITY });
    }
    scale(iv, |x| x * factor)
}

pub fn div(iv: Interval, factor: f64) -> Result<Interval, IntervalError> {
    if factor == 0.0 {
        return Err(division_by_zero());
    }
    if let Some(sign) = iv.infinity_sign() {
        if !factor.is_finite() {
            return Err(out_of_range());
        }
        let pos = (sign > 0) == (factor > 0.0);
        return Ok(if pos { POS_INFINITY } else { NEG_INFINITY });
    }
    scale(iv, |x| x / factor)
}

/// Shared core of interval multiply/divide: apply `op` (multiply or divide by
/// the factor already captured in `op`) to each field, cascading the fractional
/// months and days that integer truncation drops down into the time field, so
/// the result matches what PostgreSQL's interval `*`/`/` operators produce.
fn scale(iv: Interval, op: impl Fn(f64) -> f64) -> Result<Interval, IntervalError> {
    let month_f = op(iv.months as f64);
    let day_f = op(iv.days as f64);
    let months = to_i32(month_f)?;
    let mut days = to_i32(day_f)?;

    // Fractional months become days (30/month); the fraction the day count then
    // drops, plus the fractional days, become sub-day time.
    let month_remainder_days = (month_f - months as f64) * DAYS_PER_MONTH as f64;
    let mut sec_remainder = (day_f - days as f64 + month_remainder_days
        - month_remainder_days.trunc())
        * SECS_PER_DAY as f64;
    sec_remainder = round_micros(sec_remainder);
    // Spilled more than a whole day of seconds: carry it back into days.
    if sec_remainder.abs() >= SECS_PER_DAY as f64 {
        let whole = (sec_remainder / SECS_PER_DAY as f64).trunc();
        days = days.checked_add(to_i32(whole)?).ok_or_else(out_of_range)?;
        sec_remainder -= whole * SECS_PER_DAY as f64;
    }
    days = days
        .checked_add(month_remainder_days.trunc() as i32)
        .ok_or_else(out_of_range)?;
    let time = (op(iv.usec as f64) + sec_remainder * USECS_PER_SEC as f64).round_ties_even();
    // `i64::MAX as f64` rounds up to 2^63, so use `>=` to reject a value at that
    // boundary that the `as i64` cast would otherwise saturate.
    if !time.is_finite() || time >= -(i64::MIN as f64) || time < i64::MIN as f64 {
        return Err(out_of_range());
    }
    Ok(Interval {
        months,
        days,
        usec: time as i64,
    })
}

fn to_i32(x: f64) -> Result<i32, IntervalError> {
    if !x.is_finite() || x > i32::MAX as f64 || x < i32::MIN as f64 {
        return Err(out_of_range());
    }
    Ok(x as i32)
}

/// Add an `i64` delta to an `i32` field, erroring (rather than wrapping) if the
/// result leaves `i32` range — the justify/carry helpers use this so an overflow
/// becomes "interval out of range" instead of a silent wraparound.
fn add_i32(base: i32, delta: i64) -> Result<i32, IntervalError> {
    i32::try_from(base as i64 + delta).map_err(|_| out_of_range())
}

/// Round a seconds value to the nearest microsecond (PostgreSQL rounds interval
/// sub-second results to microsecond precision).
fn round_micros(sec: f64) -> f64 {
    (sec * 1_000_000.0).round_ties_even() / 1_000_000.0
}

// --- justify_days / justify_hours / justify_interval -----------------------

pub fn justify_hours(iv: Interval) -> Result<Interval, IntervalError> {
    if !iv.is_finite() {
        return Ok(iv);
    }
    let mut r = iv;
    let wholeday = r.usec / USECS_PER_DAY;
    r.usec -= wholeday * USECS_PER_DAY;
    r.days = add_i32(r.days, wholeday)?;
    reconcile_day_time(&mut r);
    Ok(r)
}

pub fn justify_days(iv: Interval) -> Result<Interval, IntervalError> {
    if !iv.is_finite() {
        return Ok(iv);
    }
    let mut r = iv;
    let wholemonth = (r.days as i64) / DAYS_PER_MONTH;
    r.days -= (wholemonth * DAYS_PER_MONTH) as i32;
    r.months = add_i32(r.months, wholemonth)?;
    reconcile_month_day(&mut r);
    Ok(r)
}

pub fn justify_interval(iv: Interval) -> Result<Interval, IntervalError> {
    if !iv.is_finite() {
        return Ok(iv);
    }
    let mut r = iv;
    let wholeday = r.usec / USECS_PER_DAY;
    r.usec -= wholeday * USECS_PER_DAY;
    r.days = add_i32(r.days, wholeday)?;
    let wholemonth = (r.days as i64) / DAYS_PER_MONTH;
    r.days -= (wholemonth * DAYS_PER_MONTH) as i32;
    r.months = add_i32(r.months, wholemonth)?;
    // Reconcile so all three fields share a sign (month over day over time).
    if r.months > 0 && (r.days < 0 || (r.days == 0 && r.usec < 0)) {
        r.days += DAYS_PER_MONTH as i32;
        r.months -= 1;
    } else if r.months < 0 && (r.days > 0 || (r.days == 0 && r.usec > 0)) {
        r.days -= DAYS_PER_MONTH as i32;
        r.months += 1;
    }
    reconcile_day_time(&mut r);
    Ok(r)
}

fn reconcile_day_time(r: &mut Interval) {
    if r.days > 0 && r.usec < 0 {
        r.usec += USECS_PER_DAY;
        r.days -= 1;
    } else if r.days < 0 && r.usec > 0 {
        r.usec -= USECS_PER_DAY;
        r.days += 1;
    }
}

fn reconcile_month_day(r: &mut Interval) {
    if r.months > 0 && r.days < 0 {
        r.days += DAYS_PER_MONTH as i32;
        r.months -= 1;
    } else if r.months < 0 && r.days > 0 {
        r.days -= DAYS_PER_MONTH as i32;
        r.months += 1;
    }
}

// --- make_interval ---------------------------------------------------------

/// `make_interval(years, months, weeks, days, hours, mins, secs)`.
pub fn make_interval(
    years: i64,
    months: i64,
    weeks: i64,
    days: i64,
    hours: i64,
    mins: i64,
    secs: f64,
) -> Result<Interval, IntervalError> {
    let total_months = years
        .checked_mul(MONTHS_PER_YEAR)
        .and_then(|m| m.checked_add(months))
        .ok_or_else(out_of_range)?;
    let total_days = weeks
        .checked_mul(7)
        .and_then(|d| d.checked_add(days))
        .ok_or_else(out_of_range)?;
    let usec = hours as f64 * USECS_PER_HOUR as f64
        + mins as f64 * USECS_PER_MINUTE as f64
        + (secs * USECS_PER_SEC as f64).round_ties_even();
    if !usec.is_finite() || usec > i64::MAX as f64 || usec < i64::MIN as f64 {
        return Err(out_of_range());
    }
    Ok(Interval {
        months: i32::try_from(total_months).map_err(|_| out_of_range())?,
        days: i32::try_from(total_days).map_err(|_| out_of_range())?,
        usec: usec as i64,
    })
}

// --- date_part / extract ---------------------------------------------------

/// Fields that grow monotonically with the interval (the highest-order field of
/// each stored component, plus epoch); on `±infinity` PG returns `±Infinity` for
/// these and NULL for every other (oscillating) known field.
const MONOTONIC_UNITS: &[&str] = &[
    "hour",
    "day",
    "year",
    "decade",
    "century",
    "millennium",
    "epoch",
];

const KNOWN_UNITS: &[&str] = &[
    "microseconds",
    "milliseconds",
    "second",
    "minute",
    "hour",
    "day",
    "week",
    "month",
    "quarter",
    "year",
    "decade",
    "century",
    "millennium",
    "epoch",
];

/// `date_part` (float8). `Ok(None)` is SQL NULL (an oscillating field on
/// `±infinity`); an unrecognized unit errors.
pub fn date_part(unit: &str, iv: Interval) -> Result<Option<f64>, IntervalError> {
    let unit = canonical_unit(unit);
    if let Some(sign) = iv.infinity_sign() {
        return non_finite(&unit, sign, f64::INFINITY, f64::NEG_INFINITY);
    }
    let (hour, min, sec, fsec) = split_time(iv.usec);
    let sub_usec = sec * USECS_PER_SEC + fsec;
    let year = (iv.months / 12) as i64;
    let mon = (iv.months % 12) as i64;
    let value = match unit.as_str() {
        "microseconds" => sub_usec as f64,
        "milliseconds" => sub_usec as f64 / 1000.0,
        "second" => sec as f64 + fsec as f64 / 1e6,
        "minute" => min as f64,
        "hour" => hour as f64,
        "day" => iv.days as f64,
        "week" => (iv.days / 7) as f64,
        "month" => mon as f64,
        "quarter" => (mon / 3 + 1) as f64,
        "year" => year as f64,
        "decade" => (year / 10) as f64,
        "century" => (year / 100) as f64,
        "millennium" => (year / 1000) as f64,
        "epoch" => epoch_seconds(iv),
        _ => return Err(units_not_recognized(&unit)),
    };
    Ok(Some(value))
}

/// `extract` (numeric). Same fields as [`date_part`], but with PG's per-field
/// scale on the sub-second and epoch values.
pub fn extract(unit: &str, iv: Interval) -> Result<Option<Numeric>, IntervalError> {
    let unit = canonical_unit(unit);
    if let Some(sign) = iv.infinity_sign() {
        return non_finite(&unit, sign, Numeric::pos_inf(), Numeric::neg_inf());
    }
    let (_, _, sec, fsec) = split_time(iv.usec);
    let sub_usec = sec * USECS_PER_SEC + fsec;
    let s = match unit.as_str() {
        "second" => crate::timestamp::fixed_point(sub_usec, 6),
        "milliseconds" => crate::timestamp::fixed_point(sub_usec, 3),
        "microseconds" => sub_usec.to_string(),
        "epoch" => format!("{:.6}", epoch_seconds(iv)),
        _ => match date_part(&unit, iv)? {
            Some(value) => (value as i64).to_string(),
            None => panic!("finite interval field must have a value"),
        },
    };
    match Numeric::parse(&s) {
        Ok(value) => Ok(Some(value)),
        Err(_) => panic!("interval extraction must form a valid numeric literal"),
    }
}

/// The value of a known field on a `±infinity` interval.
fn non_finite<T>(unit: &str, sign: i32, pos: T, neg: T) -> Result<Option<T>, IntervalError> {
    if !KNOWN_UNITS.contains(&unit) {
        return Err(units_not_recognized(unit));
    }
    if MONOTONIC_UNITS.contains(&unit) {
        Ok(Some(if sign > 0 { pos } else { neg }))
    } else {
        Ok(None)
    }
}

/// Seconds represented by the interval, PG's epoch reduction: a year is 365.25
/// days, a leftover month is 30 days, plus the day and time parts.
fn epoch_seconds(iv: Interval) -> f64 {
    let year = (iv.months / 12) as f64;
    let mon = (iv.months % 12) as f64;
    year * DAYS_PER_YEAR * SECS_PER_DAY as f64
        + mon * DAYS_PER_MONTH as f64 * SECS_PER_DAY as f64
        + iv.days as f64 * SECS_PER_DAY as f64
        + iv.usec as f64 / 1e6
}

// --- date_trunc ------------------------------------------------------------

const TRUNC_UNITS: &[&str] = &[
    "microseconds",
    "milliseconds",
    "second",
    "minute",
    "hour",
    "day",
    "month",
    "quarter",
    "year",
    "decade",
    "century",
    "millennium",
];

/// `date_trunc(unit, interval)`. `±infinity` truncates to itself; an
/// unrecognized unit errors.
pub fn date_trunc(unit: &str, iv: Interval) -> Result<Interval, IntervalError> {
    let unit = canonical_unit(unit);
    if !TRUNC_UNITS.contains(&unit.as_str()) {
        return Err(units_not_recognized(&unit));
    }
    if !iv.is_finite() {
        return Ok(iv);
    }
    let (mut hour, mut min, mut sec, mut fsec) = split_time(iv.usec);
    let mut days = iv.days;
    let mut months = iv.months;
    match unit.as_str() {
        "microseconds" => {}
        "milliseconds" => fsec = (fsec / 1000) * 1000,
        "second" => fsec = 0,
        "minute" => {
            fsec = 0;
            sec = 0;
        }
        "hour" => {
            fsec = 0;
            sec = 0;
            min = 0;
        }
        // day and coarser zero the whole time part.
        "day" => zero_time(&mut hour, &mut min, &mut sec, &mut fsec),
        "month" => {
            zero_time(&mut hour, &mut min, &mut sec, &mut fsec);
            days = 0;
        }
        "quarter" => {
            zero_time(&mut hour, &mut min, &mut sec, &mut fsec);
            days = 0;
            months = (months / 3) * 3;
        }
        "year" => {
            zero_time(&mut hour, &mut min, &mut sec, &mut fsec);
            days = 0;
            months = (months / 12) * 12;
        }
        "decade" => {
            zero_time(&mut hour, &mut min, &mut sec, &mut fsec);
            days = 0;
            months = (months / 120) * 120;
        }
        "century" => {
            zero_time(&mut hour, &mut min, &mut sec, &mut fsec);
            days = 0;
            months = (months / 1200) * 1200;
        }
        "millennium" => {
            zero_time(&mut hour, &mut min, &mut sec, &mut fsec);
            days = 0;
            months = (months / 12000) * 12000;
        }
        _ => return Err(units_not_recognized(&unit)),
    }
    let usec = hour * USECS_PER_HOUR + min * USECS_PER_MINUTE + sec * USECS_PER_SEC + fsec;
    Ok(Interval { months, days, usec })
}

fn zero_time(hour: &mut i64, min: &mut i64, sec: &mut i64, fsec: &mut i64) {
    *hour = 0;
    *min = 0;
    *sec = 0;
    *fsec = 0;
}

/// Canonicalize a unit spelling: lowercase, trimmed, plural/alias-folded.
fn canonical_unit(unit: &str) -> String {
    let u = unit.trim().to_ascii_lowercase();
    match u.as_str() {
        "years" => "year",
        "months" | "mon" | "mons" => "month",
        "days" => "day",
        "hours" => "hour",
        "minutes" => "minute",
        "seconds" => "second",
        "millisecond" | "msec" | "msecs" | "millisecons" => "milliseconds",
        "microsecond" | "usec" | "usecs" => "microseconds",
        "decades" => "decade",
        "centuries" => "century",
        "millenniums" | "millenia" | "millenium" => "millennium",
        "quarters" => "quarter",
        _ => return u,
    }
    .to_string()
}

// --- input (interval_in, a practical subset) -------------------------------

/// A time unit a bare number can attach to. `Second` is the default unit for a
/// number with no field of its own (`interval '1'` is one second).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Microsecond,
    Millisecond,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Year,
    Decade,
    Century,
    Millennium,
}

fn unit_from_word(word: &str) -> Option<Unit> {
    Some(match word {
        "microsecond" | "microseconds" | "us" | "usec" | "usecs" => Unit::Microsecond,
        "millisecond" | "milliseconds" | "ms" | "msec" | "msecs" => Unit::Millisecond,
        "second" | "seconds" | "sec" | "secs" | "s" => Unit::Second,
        "minute" | "minutes" | "min" | "mins" => Unit::Minute,
        "hour" | "hours" | "hr" | "hrs" | "h" => Unit::Hour,
        "day" | "days" | "d" => Unit::Day,
        "week" | "weeks" | "w" => Unit::Week,
        "month" | "months" | "mon" | "mons" => Unit::Month,
        "year" | "years" | "yr" | "yrs" | "y" => Unit::Year,
        "decade" | "decades" | "dec" | "decs" => Unit::Decade,
        "century" | "centuries" | "cent" | "c" => Unit::Century,
        "millennium" | "millennia" | "mil" | "mils" => Unit::Millennium,
        _ => return None,
    })
}

/// `interval_in` with `Second` as the default unit for bare numbers.
pub fn parse(input: &str) -> Result<Interval, IntervalError> {
    parse_with_default(input, Unit::Second)
}

/// `interval_in`, using `default` as the unit for a bare number that carries no
/// unit of its own (the SQL-standard `INTERVAL '1' DAY` leading-field form).
pub fn parse_with_default(input: &str, default: Unit) -> Result<Interval, IntervalError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(invalid_syntax(input));
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "infinity" | "+infinity" => return Ok(POS_INFINITY),
        "-infinity" => return Ok(NEG_INFINITY),
        _ => {}
    }

    let mut acc = Acc::default();
    if is_iso8601(trimmed) {
        parse_iso8601(trimmed, input, &mut acc)?;
        return acc.finish(input);
    }

    let mut ago = false;
    let mut pending: Option<(i128, f64)> = None;
    for tok in trimmed.split_whitespace() {
        let tl = tok.to_ascii_lowercase();
        if tl == "ago" {
            ago = true;
            continue;
        }
        // A `:`-bearing token is an `HH:MM[:SS[.f]]` time.
        if tok.contains(':') {
            flush_pending(&mut pending, default, &mut acc, input)?;
            acc.add_usec(parse_time_token(tok, input)? as i128);
            continue;
        }
        // The SQL-standard `Y-M` year-month token.
        if let Some(months) = try_year_month(tok) {
            flush_pending(&mut pending, default, &mut acc, input)?;
            acc.add_months(months);
            continue;
        }
        let (num, word) = split_num_unit(&tl);
        match (num.is_empty(), word.is_empty()) {
            // `1year`, `-1.5days`: number and unit fused in one token.
            (false, false) => {
                flush_pending(&mut pending, default, &mut acc, input)?;
                let unit = unit_from_word(word).ok_or_else(|| invalid_syntax(input))?;
                apply(parse_number(num, input)?, unit, &mut acc);
            }
            // A bare number: pair it with a following unit word, else `default`.
            (false, true) => {
                flush_pending(&mut pending, default, &mut acc, input)?;
                pending = Some(parse_number(num, input)?);
            }
            // A bare unit word: applies to the pending number.
            (true, false) => {
                let unit = unit_from_word(word).ok_or_else(|| invalid_syntax(input))?;
                let n = pending.take().ok_or_else(|| invalid_syntax(input))?;
                apply(n, unit, &mut acc);
            }
            (true, true) => return Err(invalid_syntax(input)),
        }
    }
    flush_pending(&mut pending, default, &mut acc, input)?;

    if ago {
        acc.negate();
    }
    acc.finish(input)
}

/// The `i128` accumulator the parser fills; `finish` range-checks it down to
/// the stored field widths.
#[derive(Default)]
struct Acc {
    months: i128,
    days: i128,
    usec: i128,
}

impl Acc {
    fn add_months(&mut self, m: i128) {
        self.months += m;
    }
    fn add_days(&mut self, d: i128) {
        self.days += d;
    }
    fn add_usec(&mut self, u: i128) {
        self.usec += u;
    }
    fn negate(&mut self) {
        self.months = -self.months;
        self.days = -self.days;
        self.usec = -self.usec;
    }
    /// Narrow the accumulator to the stored field widths. A field that does not
    /// fit is PG's "interval field value out of range" (22015), carrying the
    /// original input text.
    fn finish(self, input: &str) -> Result<Interval, IntervalError> {
        Ok(Interval {
            months: i32::try_from(self.months).map_err(|_| field_value_out_of_range(input))?,
            days: i32::try_from(self.days).map_err(|_| field_value_out_of_range(input))?,
            usec: i64::try_from(self.usec).map_err(|_| field_value_out_of_range(input))?,
        })
    }
}

fn flush_pending(
    pending: &mut Option<(i128, f64)>,
    default: Unit,
    acc: &mut Acc,
    _input: &str,
) -> Result<(), IntervalError> {
    if let Some(n) = pending.take() {
        apply(n, default, acc);
    }
    Ok(())
}

/// Apply a `(whole, frac)` number in `unit` to the accumulator, cascading any
/// fractional part into finer units the same way PG's interval input does. The
/// whole part is `i128` so an out-of-range field is caught by `Acc::finish`
/// rather than overflowing here.
fn apply(n: (i128, f64), unit: Unit, acc: &mut Acc) {
    let (val, fval) = n;
    match unit {
        Unit::Microsecond => acc.add_usec(val + fval.round_ties_even() as i128),
        Unit::Millisecond => acc.add_usec(((val as f64 + fval) * 1000.0).round_ties_even() as i128),
        Unit::Second => {
            acc.add_usec(val * USECS_PER_SEC as i128);
            acc.add_usec((fval * USECS_PER_SEC as f64).round_ties_even() as i128);
        }
        Unit::Minute => {
            acc.add_usec(val * USECS_PER_MINUTE as i128);
            adjust_fract_seconds(fval, 60.0, acc);
        }
        Unit::Hour => {
            acc.add_usec(val * USECS_PER_HOUR as i128);
            adjust_fract_seconds(fval, 3600.0, acc);
        }
        Unit::Day => {
            acc.add_days(val);
            adjust_fract_seconds(fval, SECS_PER_DAY as f64, acc);
        }
        Unit::Week => {
            acc.add_days(val * 7);
            adjust_fract_days(fval, 7.0, acc);
        }
        Unit::Month => {
            acc.add_months(val);
            adjust_fract_days(fval, DAYS_PER_MONTH as f64, acc);
        }
        Unit::Year => apply_scaled_months(val, fval, MONTHS_PER_YEAR, acc),
        Unit::Decade => apply_scaled_months(val, fval, 120, acc),
        Unit::Century => apply_scaled_months(val, fval, 1200, acc),
        Unit::Millennium => apply_scaled_months(val, fval, 12000, acc),
    }
}

/// Year/decade/century/millennium: the whole part scales to months exactly, the
/// fraction to a (truncated) month count, as PG does (no finer cascade).
fn apply_scaled_months(val: i128, fval: f64, months_per: i64, acc: &mut Acc) {
    acc.add_months(val * months_per as i128);
    if fval != 0.0 {
        acc.add_months((fval * months_per as f64) as i128);
    }
}

fn adjust_fract_seconds(frac: f64, scale_secs: f64, acc: &mut Acc) {
    if frac != 0.0 {
        acc.add_usec((frac * scale_secs * USECS_PER_SEC as f64).round_ties_even() as i128);
    }
}

fn adjust_fract_days(frac: f64, scale_days: f64, acc: &mut Acc) {
    if frac == 0.0 {
        return;
    }
    let d = frac * scale_days;
    let whole = d.trunc();
    acc.add_days(whole as i128);
    acc.add_usec(((d - whole) * USECS_PER_DAY as f64).round_ties_even() as i128);
}

/// Split a token into its leading signed-numeric prefix and trailing unit word.
fn split_num_unit(tok: &str) -> (&str, &str) {
    let split = tok
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '+' || c == '-'))
        .unwrap_or(tok.len());
    (&tok[..split], &tok[split..])
}

/// Parse a signed decimal into `(whole, frac)` where both carry the sign
/// (`-1.5` → `(-1, -0.5)`), so a fractional value with a zero whole keeps it.
/// The whole part is `i128` so a value beyond `i64` is a field-overflow (22015),
/// not a syntax error, and still narrows to the stored width in `Acc::finish`.
fn parse_number(s: &str, input: &str) -> Result<(i128, f64), IntervalError> {
    let neg = s.starts_with('-');
    let body = s.trim_start_matches(['+', '-']);
    if body.is_empty() {
        return Err(invalid_syntax(input));
    }
    let (int_str, frac_str) = body.split_once('.').unwrap_or((body, ""));
    let int_part: i128 = if int_str.is_empty() {
        0
    } else {
        int_str.parse().map_err(|_| {
            // All-digit but unparseable means it overflowed i128 — a field value
            // out of range, as PG reports; anything else is a syntax error.
            if int_str.bytes().all(|b| b.is_ascii_digit()) {
                field_value_out_of_range(input)
            } else {
                invalid_syntax(input)
            }
        })?
    };
    let frac: f64 = if frac_str.is_empty() {
        0.0
    } else {
        if !frac_str.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid_syntax(input));
        }
        format!("0.{frac_str}")
            .parse()
            .map_err(|_| invalid_syntax(input))?
    };
    let sign = if neg { -1.0 } else { 1.0 };
    Ok((if neg { -int_part } else { int_part }, frac * sign))
}

/// Parse an `[+-]HH:MM[:SS[.ffffff]]` time token into signed microseconds. The
/// hours are unbounded, but (per PG's colon-form validation) minutes must be
/// 0-59 and seconds 0-60 — out-of-range fields raise `22015`.
fn parse_time_token(tok: &str, input: &str) -> Result<i64, IntervalError> {
    let syntax = || invalid_syntax(input);
    let neg = tok.starts_with('-');
    let body = tok.trim_start_matches(['+', '-']);
    let mut parts = body.split(':');
    let hour: i64 = parts
        .next()
        .ok_or_else(syntax)?
        .parse()
        .map_err(|_| syntax())?;
    let min: i64 = parts
        .next()
        .ok_or_else(syntax)?
        .parse()
        .map_err(|_| syntax())?;
    let (sec, fsec) = match parts.next() {
        None => (0, 0),
        Some(secpart) => {
            let (whole, frac) = secpart.split_once('.').unwrap_or((secpart, ""));
            (
                whole.parse().map_err(|_| syntax())?,
                crate::timestamp::parse_fraction(frac).ok_or_else(syntax)?,
            )
        }
    };
    if parts.next().is_some() {
        return Err(syntax());
    }
    if min > 59 || sec > 60 {
        return Err(field_value_out_of_range(input));
    }
    // Compute in i128 so a large hours field (`hour * USECS_PER_HOUR`) can't
    // overflow i64 and panic; a result beyond i64 is a field-value overflow.
    let usec = hour as i128 * USECS_PER_HOUR as i128
        + min as i128 * USECS_PER_MINUTE as i128
        + sec as i128 * USECS_PER_SEC as i128
        + fsec as i128;
    let usec = i64::try_from(usec).map_err(|_| field_value_out_of_range(input))?;
    Ok(if neg { -usec } else { usec })
}

/// The SQL-standard `Y-M` year-month token → total months (a leading `-` makes
/// both parts negative). Rejects anything that isn't exactly `digits-digits`.
fn try_year_month(tok: &str) -> Option<i128> {
    let neg = tok.starts_with('-');
    let body = tok.trim_start_matches(['+', '-']);
    let (y, m) = body.split_once('-')?;
    if y.is_empty() || m.is_empty() || m.contains('-') {
        return None;
    }
    if !y.bytes().all(|b| b.is_ascii_digit()) || !m.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let years: i128 = y.parse().ok()?;
    let months: i128 = m.parse().ok()?;
    let total = years * 12 + months;
    Some(if neg { -total } else { total })
}

// --- ISO 8601 duration -----------------------------------------------------

fn is_iso8601(s: &str) -> bool {
    let b = s.as_bytes();
    matches!(b.first(), Some(b'P' | b'p')) && s.len() > 1
}

/// Parse an ISO-8601 duration `P[nY][nM][nW][nD][T[nH][nM][nS]]`. The `M`
/// designator is months before the `T`, minutes after it.
fn parse_iso8601(s: &str, input: &str, acc: &mut Acc) -> Result<(), IntervalError> {
    let mut chars = s[1..].chars().peekable(); // skip leading 'P'
    let mut in_time = false;
    let mut saw_field = false;
    while let Some(&c) = chars.peek() {
        if c == 'T' || c == 't' {
            chars.next();
            in_time = true;
            continue;
        }
        // Read a signed decimal number.
        let mut num = String::new();
        if matches!(c, '+' | '-') {
            num.push(c);
            chars.next();
        }
        while let Some(&d) = chars.peek() {
            if d.is_ascii_digit() || d == '.' {
                num.push(d);
                chars.next();
            } else {
                break;
            }
        }
        let desig = chars.next().ok_or_else(|| invalid_syntax(input))?;
        if num.is_empty() {
            return Err(invalid_syntax(input));
        }
        let unit = match desig.to_ascii_uppercase() {
            'Y' => Unit::Year,
            'M' if in_time => Unit::Minute,
            'M' => Unit::Month,
            'W' => Unit::Week,
            'D' => Unit::Day,
            'H' => Unit::Hour,
            'S' => Unit::Second,
            _ => return Err(invalid_syntax(input)),
        };
        apply(parse_number(&num, input)?, unit, acc);
        saw_field = true;
    }
    if !saw_field {
        return Err(invalid_syntax(input));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn iv(s: &str) -> Interval {
        match parse(s) {
            Ok(value) => value,
            Err(error) => panic!("invalid interval test fixture `{s}`: {error:?}"),
        }
    }
    fn out(s: &str) -> String {
        format(iv(s))
    }

    #[test]
    fn output_formatting() {
        assert_eq!(
            out("1 year 2 mons 3 days 04:05:06"),
            "1 year 2 mons 3 days 04:05:06"
        );
        assert_eq!(out("1.5 days"), "1 day 12:00:00");
        assert_eq!(out("-1 day 2 hours"), "-1 days +02:00:00");
        assert_eq!(out("2 days ago"), "-2 days");
        assert_eq!(out("-00:00:01"), "-00:00:01");
        assert_eq!(out("0"), "00:00:00");
        assert_eq!(out("1 mon"), "1 mon");
        assert_eq!(out("2 mons"), "2 mons");
        assert_eq!(out("-1 mon"), "-1 mons");
        assert_eq!(out("90 minutes"), "01:30:00");
        assert_eq!(out("1.5 mons"), "1 mon 15 days");
        assert_eq!(out("2.5 hours"), "02:30:00");
        assert_eq!(out("1 year -2 mons"), "10 mons");
        assert_eq!(out("100000000 days"), "100000000 days");
        assert_eq!(out("00:00:01.234567"), "00:00:01.234567");
        assert_eq!(out("1.5 seconds"), "00:00:01.5");
        assert_eq!(out("24:00:00"), "24:00:00");
    }

    #[test]
    fn sql_standard_and_iso_forms() -> anyhow::Result<()> {
        assert_eq!(format(parse_with_default("1", Unit::Day)?), "1 day");
        assert_eq!(
            format(parse_with_default("1-2", Unit::Year)?),
            "1 year 2 mons"
        );
        assert_eq!(
            format(parse_with_default("3 4:05:06", Unit::Day)?),
            "3 days 04:05:06"
        );
        assert_eq!(out("P1Y2M3DT4H5M6S"), "1 year 2 mons 3 days 04:05:06");
        assert_eq!(out("1"), "00:00:01");

        Ok(())
    }

    #[test]
    fn infinities() -> anyhow::Result<()> {
        assert_eq!(iv("infinity"), POS_INFINITY);
        assert_eq!(iv("+infinity"), POS_INFINITY);
        assert_eq!(iv("-infinity"), NEG_INFINITY);
        assert_eq!(format(POS_INFINITY), "infinity");
        assert_eq!(format(NEG_INFINITY), "-infinity");
        assert!(!POS_INFINITY.is_finite());
        assert_eq!(negate(POS_INFINITY)?, NEG_INFINITY);
        assert_eq!(add(POS_INFINITY, iv("1 day"))?, POS_INFINITY);
        assert!(add(POS_INFINITY, NEG_INFINITY).is_err());
        assert_eq!(mul(POS_INFINITY, 2.0)?, POS_INFINITY);
        assert!(mul(POS_INFINITY, 0.0).is_err());
        assert_eq!(justify_interval(POS_INFINITY)?, POS_INFINITY);

        Ok(())
    }

    #[test]
    fn comparison() {
        assert_eq!(cmp(iv("2 mons"), iv("70 days")), Ordering::Less);
        assert_eq!(cmp(iv("1 day"), iv("24 hours")), Ordering::Equal);
        assert_eq!(cmp(NEG_INFINITY, iv("1 day")), Ordering::Less);
        assert_eq!(cmp(POS_INFINITY, iv("1 day")), Ordering::Greater);
    }

    #[test]
    fn arithmetic() -> anyhow::Result<()> {
        assert_eq!(format(mul(iv("1 day 3 hours"), 2.5)?), "2 days 19:30:00");
        assert_eq!(format(div(iv("1 day 3 hours"), 2.0)?), "13:30:00");
        assert_eq!(format(mul(iv("2 mons"), 3.0)?), "6 mons");
        assert_eq!(format(mul(iv("1 day"), 3.7)?), "3 days 16:48:00");
        assert_eq!(format(negate(iv("1 day 2 hours"))?), "-1 days -02:00:00");
        assert_eq!(format(add(iv("1 day"), iv("2 hours"))?), "1 day 02:00:00");
        assert_eq!(
            format(sub(iv("5 mons"), iv("2 mons 10 days"))?),
            "3 mons -10 days"
        );
        assert!(div(iv("1 day"), 0.0).is_err());

        Ok(())
    }

    #[test]
    fn justify() -> anyhow::Result<()> {
        assert_eq!(format(justify_days(iv("35 days"))?), "1 mon 5 days");
        assert_eq!(format(justify_hours(iv("27 hours"))?), "1 day 03:00:00");
        assert_eq!(
            format(justify_interval(iv("1 mon 33 days 27 hours"))?),
            "2 mons 4 days 03:00:00"
        );

        Ok(())
    }

    #[test]
    fn fields() -> anyhow::Result<()> {
        let dp = |u: &str, s: &str| -> anyhow::Result<f64> {
            date_part(u, iv(s))?.ok_or_else(|| anyhow::anyhow!("missing {u} field"))
        };
        assert_eq!(dp("hour", "1 day 02:03:04")?, 2.0);
        assert_eq!(dp("day", "1 day 02:03:04")?, 1.0);
        assert_eq!(dp("month", "14 months")?, 2.0);
        assert_eq!(dp("year", "14 months")?, 1.0);
        assert_eq!(dp("quarter", "14 months")?, 1.0);
        assert_eq!(dp("epoch", "1 year 2 mons 3 days 04:05:06")?, 37015506.0);
        assert_eq!(dp("epoch", "1 day 02:03:04")?, 93784.0);
        assert_eq!(dp("minute", "-1 day -02:03:04")?, -3.0);
        let ex = |u: &str, s: &str| -> anyhow::Result<String> {
            Ok(extract(u, iv(s))?
                .ok_or_else(|| anyhow::anyhow!("missing {u} field"))?
                .to_display())
        };
        assert_eq!(ex("second", "00:00:04.5")?, "4.500000");
        assert_eq!(ex("milliseconds", "00:00:04.5")?, "4500.000");
        assert_eq!(ex("microseconds", "00:00:04.5")?, "4500000");
        assert_eq!(ex("epoch", "1 day 02:03:04")?, "93784.000000");
        // Infinite intervals: monotonic fields are ±Infinity, others NULL.
        assert_eq!(date_part("year", POS_INFINITY)?, Some(f64::INFINITY));
        assert_eq!(date_part("epoch", NEG_INFINITY)?, Some(f64::NEG_INFINITY));
        assert_eq!(date_part("month", POS_INFINITY)?, None);
        assert_eq!(
            date_part("bogus", iv("1 day")).unwrap_err().sqlstate,
            "22023"
        );

        Ok(())
    }

    #[test]
    fn trunc_and_make() -> anyhow::Result<()> {
        assert_eq!(
            format(date_trunc("hour", iv("1 day 02:03:04.55"))?),
            "1 day 02:00:00"
        );
        assert_eq!(
            format(date_trunc("day", iv("1 mon 2 days 3 hours"))?),
            "1 mon 2 days"
        );
        assert_eq!(date_trunc("day", POS_INFINITY)?, POS_INFINITY);
        assert_eq!(
            format(make_interval(1, 2, 3, 4, 5, 6, 7.5)?),
            "1 year 2 mons 25 days 05:06:07.5"
        );
        assert_eq!(format(make_interval(0, 0, 0, 0, 0, 0, 0.0)?), "00:00:00");

        Ok(())
    }

    #[test]
    fn errors() {
        assert_eq!(parse("garbage").unwrap_err().sqlstate, "22007");
        // A field beyond i32 is PG's "interval field value out of range" (22015).
        assert_eq!(parse("2147483648 mons").unwrap_err().sqlstate, "22015");
        assert_eq!(format(iv("2147483647 mons")), "178956970 years 7 mons");
    }

    #[test]
    fn overflow_errors_instead_of_panicking() {
        // Regression: these all used to overflow i64/i32 and panic (or wrap);
        // each must now return a clean error, matching PG.
        // A huge hours field in the colon form (was: i64 overflow panic).
        assert_eq!(parse("3000000000:00:00").unwrap_err().sqlstate, "22015");
        // A bare integer beyond i64 (was: reported as 22007 syntax error).
        assert_eq!(
            parse("99999999999999999999 days").unwrap_err().sqlstate,
            "22015"
        );
        // A field beyond i32 (was: silently narrowed).
        assert_eq!(parse("3000000000 days").unwrap_err().sqlstate, "22015");
        assert_eq!(parse("3000000000 mons").unwrap_err().sqlstate, "22015");
        // justify carrying past i32 (was: wrapping_add → silent wrong value).
        let big = Interval {
            months: 0,
            days: i32::MAX,
            usec: USECS_PER_DAY,
        };
        assert!(justify_hours(big).is_err());
        let big_days = Interval {
            months: i32::MAX,
            days: 40,
            usec: 0,
        };
        assert!(justify_days(big_days).is_err());
    }

    #[test]
    fn time_field_validation() {
        // Minutes 0-59, seconds 0-60 (60 carries); out of range is 22015.
        assert_eq!(out("00:00:60"), "00:01:00");
        assert_eq!(out("100:00:00"), "100:00:00");
        assert_eq!(parse("00:00:61").unwrap_err().sqlstate, "22015");
        assert_eq!(parse("01:60:00").unwrap_err().sqlstate, "22015");
        assert_eq!(parse("1 day 00:75:00").unwrap_err().sqlstate, "22015");
    }

    #[test]
    fn infinite_field_split_and_week() -> anyhow::Result<()> {
        // Monotonic fields (incl. hour and day) are ±Infinity; oscillating NULL.
        let dp = |u: &str, i: Interval| date_part(u, i);
        assert_eq!(dp("hour", POS_INFINITY)?, Some(f64::INFINITY));
        assert_eq!(dp("day", POS_INFINITY)?, Some(f64::INFINITY));
        assert_eq!(dp("minute", POS_INFINITY)?, None);
        assert_eq!(dp("month", NEG_INFINITY)?, None);
        assert_eq!(dp("week", POS_INFINITY)?, None);
        assert_eq!(date_part("week", iv("20 days"))?, Some(2.0));
        // A known-but-unsupported unit says "not supported", not "not recognized".
        assert!(
            date_part("dow", iv("1 day"))
                .unwrap_err()
                .message
                .contains("not supported")
        );

        Ok(())
    }
}
