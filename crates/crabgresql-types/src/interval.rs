//! `interval`: parsing, output, arithmetic, the justify/make constructors, and
//! the field functions (`date_part`/`extract`/`date_trunc`).
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

use std::borrow::Cow;
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

// --- output ----------------------------------------------------------------

/// The `IntervalStyle` GUC: which of the four renderings `interval_out`
/// produces.
///
/// It picks the *input* reading too: `sql_standard` makes a leading minus
/// propagate to the following unsigned fields, which is what lets its own
/// one-sign output read back as the value it was printed from. See
/// [`parse_with_style`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IntervalStyle {
    /// `1 year 2 mons 3 days 04:05:06` — PostgreSQL's default.
    #[default]
    Postgres,
    /// `@ 1 year 2 mons 3 days 4 hours 5 mins 6 secs`, with `ago` for a
    /// wholly-negative span.
    PostgresVerbose,
    /// `+1-2 +3 +4:05:06`, the SQL specification's literal forms.
    SqlStandard,
    /// `P1Y2M3DT4H5M6S`, the ISO-8601 duration form.
    Iso8601,
}

impl IntervalStyle {
    /// Parse a `SET IntervalStyle` value. Names are case-insensitive in PG, and
    /// nothing more: `SET IntervalStyle TO ' postgres '` is
    /// `invalid value for parameter` there, so the padding is not trimmed away.
    pub fn from_name(name: &str) -> Option<IntervalStyle> {
        match name.to_ascii_lowercase().as_str() {
            "postgres" => Some(IntervalStyle::Postgres),
            "postgres_verbose" => Some(IntervalStyle::PostgresVerbose),
            "sql_standard" => Some(IntervalStyle::SqlStandard),
            "iso_8601" => Some(IntervalStyle::Iso8601),
            _ => None,
        }
    }

    /// The canonical lower-case spelling `SHOW IntervalStyle` prints.
    pub fn name(self) -> &'static str {
        match self {
            IntervalStyle::Postgres => "postgres",
            IntervalStyle::PostgresVerbose => "postgres_verbose",
            IntervalStyle::SqlStandard => "sql_standard",
            IntervalStyle::Iso8601 => "iso_8601",
        }
    }
}

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

/// `IntervalStyle = postgres_verbose`: `@ 1 year 2 mons 3 days 4 hours 5 mins
/// 6.7 secs`, `@ 1 day 2 hours ago` for a negative span, and `@ 0` for zero.
///
/// Only the deparser wants this. PostgreSQL's `ruleutils` renders an interval
/// constant in verbose style regardless of the session's `IntervalStyle`, so a
/// dumped view definition reads the same for every client — which is why
/// `pg_get_viewdef` shows `'@ 0'::interval` where `SELECT` would show
/// `00:00:00`.
///
/// Unlike [`format`], every field is signed the same way: verbose style factors
/// a wholly-negative interval's sign out into the trailing `ago`.
pub fn format_verbose(iv: Interval) -> String {
    if iv == POS_INFINITY {
        return "infinity".to_string();
    }
    if iv == NEG_INFINITY {
        return "-infinity".to_string();
    }
    // `ago` is PG's rule for a span whose *first* nonzero field is negative.
    let ago = iv.months < 0
        || (iv.months == 0 && iv.days < 0)
        || (iv.months == 0 && iv.days == 0 && iv.usec < 0);
    // Nothing may be negated at its stored width: `i32::MIN` months and days
    // and `i64::MIN` micros are all reachable intervals rather than the
    // sentinels. Widening the month and day counts is enough for them; the
    // microseconds are split *first*, because `-i64::MIN` has no i64 answer
    // while each of its four components does.
    let (months, days) = (iv.months as i64, iv.days as i64);
    let (hour, min, sec, fsec) = split_time(iv.usec);
    let (months, days, hour, min, sec, fsec) = if ago {
        (-months, -days, -hour, -min, -sec, -fsec)
    } else {
        (months, days, hour, min, sec, fsec)
    };
    let mut parts: Vec<String> = Vec::new();
    // Only an exact `1` is singular — `-1` pluralizes, so a mixed-sign interval
    // reads `@ 1 mon -1 days`. (The seconds field below is the exception: PG
    // makes it singular on the magnitude, giving `-1 sec`.)
    let mut push = |value: i64, unit: &str| {
        if value != 0 {
            parts.push(format!(
                "{value} {unit}{}",
                if value != 1 { "s" } else { "" }
            ));
        }
    };
    push(months / 12, "year");
    push(months % 12, "mon");
    push(days, "day");
    push(hour, "hour");
    push(min, "min");
    if sec != 0 || fsec != 0 {
        let plural = if sec.abs() == 1 && fsec == 0 { "" } else { "s" };
        parts.push(format!("{} sec{plural}", seconds_unpadded(sec, fsec)));
    }
    if parts.is_empty() {
        return "@ 0".to_string();
    }
    let body = parts.join(" ");
    if ago {
        format!("@ {body} ago")
    } else {
        format!("@ {body}")
    }
}

/// `IntervalStyle = sql_standard`: the SQL specification's two literal forms,
/// `<years>-<months>` and `<days> <hours>:<mins>:<secs>`.
///
/// The spec allows exactly one sign, in front of the whole value, so that form
/// can only be used when the interval is unambiguous: every field of one sign,
/// and only one of the two groups populated. Anything else falls back to a
/// non-standard third form that spells all three groups out with a sign each —
/// `interval '1 day -1 hour'` is `+0-0 +1 -1:00:00`. Read off a live PostgreSQL
/// 18.4.
pub fn format_sql_standard(iv: Interval) -> String {
    if iv == POS_INFINITY {
        return "infinity".to_string();
    }
    if iv == NEG_INFINITY {
        return "-infinity".to_string();
    }
    // Widen before anything can negate: `i32::MIN` days and `i64::MIN` micros
    // are reachable intervals, not the sentinels, and `split_time` divides
    // first so its four components always negate safely.
    let (mut year, mut mon) = ((iv.months / 12) as i64, (iv.months % 12) as i64);
    let mut day = iv.days as i64;
    let (mut hour, mut min, mut sec, mut fsec) = split_time(iv.usec);

    let fields = [year, mon, day, hour, min, sec, fsec];
    let has_negative = fields.iter().any(|&f| f < 0);
    let has_positive = fields.iter().any(|&f| f > 0);
    let has_year_month = year != 0 || mon != 0;
    let has_day_time = day != 0 || hour != 0 || min != 0 || sec != 0 || fsec != 0;
    // Whether the one-sign spec form applies at all.
    let standard = !(has_negative && has_positive) && !(has_year_month && has_day_time);

    let mut out = String::new();
    if has_negative && standard {
        out.push('-');
        year = -year;
        mon = -mon;
        day = -day;
        hour = -hour;
        min = -min;
        sec = -sec;
        fsec = -fsec;
    }
    let group_sign = |negative: bool| if negative { '-' } else { '+' };

    if !has_year_month && !has_day_time {
        out.push('0');
    } else if !standard {
        out.push(group_sign(year < 0 || mon < 0));
        out.push_str(&format!("{}-{} ", year.abs(), mon.abs()));
        out.push(group_sign(day < 0));
        out.push_str(&format!("{} ", day.abs()));
        out.push(group_sign(hour < 0 || min < 0 || sec < 0 || fsec < 0));
        out.push_str(&format!("{}:{:02}:", hour.abs(), min.abs()));
        append_seconds(&mut out, sec.abs(), fsec.abs());
    } else if has_year_month {
        out.push_str(&format!("{year}-{mon}"));
    } else {
        // Hours are not zero-padded in this style, unlike `postgres`'s.
        if day != 0 {
            out.push_str(&format!("{day} "));
        }
        out.push_str(&format!("{hour}:{min:02}:"));
        append_seconds(&mut out, sec, fsec);
    }
    out
}

/// `IntervalStyle = iso_8601`: the `PnYnMnDTnHnMnS` duration form.
///
/// A zero interval is `PT0S`; otherwise every zero field is dropped and every
/// nonzero one carries its own sign, so a mixed-sign span survives the round
/// trip (`P1Y2M-3DT-4H-5M-6.7S`). Unlike the other styles the seconds are not
/// zero-padded. Read off a live PostgreSQL 18.4.
pub fn format_iso8601(iv: Interval) -> String {
    if iv == POS_INFINITY {
        return "infinity".to_string();
    }
    if iv == NEG_INFINITY {
        return "-infinity".to_string();
    }
    if iv.months == 0 && iv.days == 0 && iv.usec == 0 {
        return "PT0S".to_string();
    }
    let (year, mon) = ((iv.months / 12) as i64, (iv.months % 12) as i64);
    let (hour, min, sec, fsec) = split_time(iv.usec);
    let mut out = String::from("P");
    for (value, designator) in [(year, 'Y'), (mon, 'M'), (iv.days as i64, 'D')] {
        if value != 0 {
            out.push_str(&format!("{value}{designator}"));
        }
    }
    if hour != 0 || min != 0 || sec != 0 || fsec != 0 {
        out.push('T');
        for (value, designator) in [(hour, 'H'), (min, 'M')] {
            if value != 0 {
                out.push_str(&format!("{value}{designator}"));
            }
        }
        if sec != 0 || fsec != 0 {
            out.push_str(&seconds_unpadded(sec, fsec));
            out.push('S');
        }
    }
    out
}

/// `interval_out` under `style`.
pub fn format_with(iv: Interval, style: IntervalStyle) -> String {
    match style {
        IntervalStyle::Postgres => format(iv),
        IntervalStyle::PostgresVerbose => format_verbose(iv),
        IntervalStyle::SqlStandard => format_sql_standard(iv),
        IntervalStyle::Iso8601 => format_iso8601(iv),
    }
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

/// The same seconds, signed and *not* zero-padded: `1`, `0.1`, `-54.775808`.
/// The `postgres_verbose` and `iso_8601` styles both want this form.
fn seconds_unpadded(sec: i64, fsec: i64) -> String {
    let mut out = String::new();
    if sec < 0 || fsec < 0 {
        out.push('-');
    }
    out.push_str(&sec.abs().to_string());
    if fsec != 0 {
        out.push('.');
        out.push_str(format!("{:06}", fsec.abs()).trim_end_matches('0'));
    }
    out
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

// --- type modifier (interval(p), interval <fields>) ------------------------
//
// An interval's modifier carries two things at once: which *fields* the type
// admits (`interval day to second`) and the fractional-second precision
// (`interval(3)`). They pack into one `i32` as `(range << 16) | precision`,
// with `NO_PRECISION` in the low half meaning "not specified". The bit values
// were read back off a real PostgreSQL 18.4 catalog (`atttypmod` for a column of
// each of the fourteen spellings), not taken from its source.

/// Range bits, one per field the type admits.
pub const MASK_MONTH: u16 = 1 << 1;
pub const MASK_YEAR: u16 = 1 << 2;
pub const MASK_DAY: u16 = 1 << 3;
pub const MASK_HOUR: u16 = 1 << 10;
pub const MASK_MINUTE: u16 = 1 << 11;
pub const MASK_SECOND: u16 = 1 << 12;

/// The range a bare `interval(p)` gets: every field admitted.
pub const FULL_RANGE: u16 = 0x7FFF;

/// The low half of a modifier when no precision was written.
const NO_PRECISION: u16 = 0xFFFF;

/// Pack a range mask and an optional precision into one modifier.
pub fn pack_typmod(range: u16, precision: Option<u8>) -> i32 {
    let p = precision.map_or(NO_PRECISION, u16::from);
    ((range as i32) << 16) | (p as i32)
}

/// Split a modifier back into its range mask and precision. A negative modifier
/// (no modifier at all) yields the full range and no precision.
pub fn unpack_typmod(typmod: i32) -> (u16, Option<u8>) {
    if typmod < 0 {
        return (FULL_RANGE, None);
    }
    let range = ((typmod >> 16) & 0x7FFF) as u16;
    let p = (typmod & 0xFFFF) as u16;
    let precision = if p == NO_PRECISION {
        None
    } else {
        Some(p.min(crate::timestamp::MAX_PRECISION as u16) as u8)
    };
    (range, precision)
}

/// How a range mask spells itself in `format_type`, e.g. `day to second`.
/// `None` for the full range and for any combination PostgreSQL does not name,
/// both of which print as a bare `interval`.
pub fn range_name(range: u16) -> Option<&'static str> {
    Some(match range {
        MASK_YEAR => "year",
        MASK_MONTH => "month",
        MASK_DAY => "day",
        MASK_HOUR => "hour",
        MASK_MINUTE => "minute",
        MASK_SECOND => "second",
        r if r == MASK_YEAR | MASK_MONTH => "year to month",
        r if r == MASK_DAY | MASK_HOUR => "day to hour",
        r if r == MASK_DAY | MASK_HOUR | MASK_MINUTE => "day to minute",
        r if r == MASK_DAY | MASK_HOUR | MASK_MINUTE | MASK_SECOND => "day to second",
        r if r == MASK_HOUR | MASK_MINUTE => "hour to minute",
        r if r == MASK_HOUR | MASK_MINUTE | MASK_SECOND => "hour to second",
        r if r == MASK_MINUTE | MASK_SECOND => "minute to second",
        _ => return None,
    })
}

/// Coerce `iv` to what its declared modifier admits.
///
/// The *lowest* field in the range decides everything; fields above it are left
/// alone. `interval year` keeps whole years and drops the rest, `interval hour`
/// truncates the time toward zero at the hour, and a range reaching `second`
/// rounds the fractional part to the declared precision (half away from zero,
/// like [`crate::timestamp::apply_typmod`]). Verified against PostgreSQL 18.4:
/// `interval '1 year 2 months 3 days 4:05:06.789'` cast to `interval year` is
/// `1 year`, to `interval hour` is `1 year 2 mons 3 days 04:00:00`, and to
/// `interval minute to second(0)` is `1 year 2 mons 3 days 04:05:07`.
///
/// A negative modifier, a range mask with none of the six field bits set, or a
/// non-finite value all leave the interval unchanged.
///
/// Rounding is the one step that can fail: a `usec` near the `i64` extreme has
/// no room for the half-unit the rounding adds, and PG reports that as `interval
/// out of range` rather than saturating (`interval '2562047788:00:54.775807'
/// second(2)` is an error, while the same literal without a precision is a
/// perfectly good value).
pub fn apply_typmod(iv: Interval, typmod: i32) -> Result<Interval, IntervalError> {
    if typmod < 0 || !iv.is_finite() {
        return Ok(iv);
    }
    let (range, precision) = unpack_typmod(typmod);
    if range == FULL_RANGE {
        return round_usec(iv, precision);
    }
    // Truncate at the lowest admitted field, then round if that field is
    // `second`. `%` truncating toward zero is what makes a negative interval
    // truncate toward zero too (`-1 day -2:30` as `interval hour` keeps
    // `-02:00:00`).
    let mut out = iv;
    if range & MASK_SECOND != 0 {
        return round_usec(out, precision);
    }
    if range & MASK_MINUTE != 0 {
        out.usec -= out.usec % USECS_PER_MINUTE;
    } else if range & MASK_HOUR != 0 {
        out.usec -= out.usec % USECS_PER_HOUR;
    } else if range & MASK_DAY != 0 {
        out.usec = 0;
    } else if range & MASK_MONTH != 0 {
        out.days = 0;
        out.usec = 0;
    } else if range & MASK_YEAR != 0 {
        out.months -= out.months % MONTHS_PER_YEAR as i32;
        out.days = 0;
        out.usec = 0;
    }
    Ok(out)
}

/// Round `iv.usec` to `precision` fractional-second digits, half away from zero.
///
/// Away-from-zero is reached by moving the value a half-unit *outwards* and then
/// truncating toward zero, which `/` already does. Doing it that way rather than
/// rounding the magnitude is what gives PG's boundary on both sides: a negative
/// `usec` within a half-unit of `i64::MIN` has a rounded form, and reaching it
/// through `-((-usec) + half)` would overflow one step before the value itself
/// does. PostgreSQL 18.4: `interval '-9223372036854770808 microseconds'
/// second(2)` is `-2562047788:00:54.77`, and one microsecond lower is
/// `interval out of range`.
fn round_usec(mut iv: Interval, precision: Option<u8>) -> Result<Interval, IntervalError> {
    let Some(p) = precision else { return Ok(iv) };
    let p = p as i32;
    if !(0..crate::timestamp::MAX_PRECISION).contains(&p) {
        return Ok(iv);
    }
    let scale = 10_i64.pow((crate::timestamp::MAX_PRECISION - p) as u32);
    let half = scale / 2;
    let shifted = if iv.usec < 0 {
        iv.usec.checked_sub(half)
    } else {
        iv.usec.checked_add(half)
    }
    .ok_or_else(out_of_range)?;
    iv.usec = shifted / scale * scale;
    Ok(iv)
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

// --- input (interval_in) ---------------------------------------------------

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

impl Unit {
    /// The wire encoding for a deferred `interval_in`: the binder cannot fold a
    /// style-sensitive literal, so it emits a call carrying the text and the
    /// leading-field default, and this is how the default travels.
    pub fn as_code(self) -> i32 {
        self as i32
    }

    pub fn from_code(code: i32) -> Option<Unit> {
        Some(match code {
            0 => Unit::Microsecond,
            1 => Unit::Millisecond,
            2 => Unit::Second,
            3 => Unit::Minute,
            4 => Unit::Hour,
            5 => Unit::Day,
            6 => Unit::Week,
            7 => Unit::Month,
            8 => Unit::Year,
            9 => Unit::Decade,
            10 => Unit::Century,
            11 => Unit::Millennium,
            _ => return None,
        })
    }
}

fn unit_from_word(word: &str) -> Option<Unit> {
    Some(match word {
        "microsecond" | "microseconds" | "us" | "usec" | "usecs" => Unit::Microsecond,
        "millisecond" | "milliseconds" | "ms" | "msec" | "msecs" => Unit::Millisecond,
        "second" | "seconds" | "sec" | "secs" | "s" => Unit::Second,
        // A bare `m` is minutes, not months — PG's interval unit table resolves
        // the collision in favour of the smaller unit.
        "minute" | "minutes" | "min" | "mins" | "m" => Unit::Minute,
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

/// `interval_in` with `Second` as the default unit for bare numbers, at the
/// default `postgres` style.
pub fn parse(input: &str) -> Result<Interval, IntervalError> {
    parse_with_style(input, Unit::Second, IntervalStyle::Postgres)
}

/// Whether `IntervalStyle` can change what this literal *means*.
///
/// Only `sql_standard` does, and only when a leading `-` has some later
/// unsigned field to propagate to — so a single-field literal is never
/// style-sensitive, because negating its positive fields is a no-op when it has
/// only one and that one is already negative. Never true for an ISO-8601
/// duration, which the rule does not reach. Unparseable text is `false`, so a
/// syntax error is still reported from wherever it was reported before.
///
/// The binder uses this to decide whether a literal may still be folded at bind
/// time; almost every one may.
pub fn style_sensitive(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() || is_iso8601(trimmed) {
        return false;
    }
    match interval_fields(trimmed) {
        Ok(fields) => leading_minus_propagates(&fields),
        Err(()) => false,
    }
}

/// PG's `sql_standard` sign rule, as a question about the scanned fields: the
/// first field carries a `-` and no later one carries a sign of its own.
///
/// All-or-nothing, and any later sign — `+` as much as `-` — turns it off for
/// the whole literal, which is what keeps a fully-signed rendering (the shape
/// `postgres` style emits) reading back unchanged.
fn leading_minus_propagates(fields: &[Field<'_>]) -> bool {
    fields.len() >= 2
        && fields[0].text.starts_with('-')
        && !fields[1..].iter().any(|f| f.text.starts_with(['+', '-']))
}

/// What a scanned field is, which its text alone cannot always say.
///
/// `'1.days'` is an error, `'1. days'` is a day, `'.days'` is zero days: the
/// three differ only in where a `.` sits relative to the digits, so the
/// classification has to come out of the scanner rather than be re-derived by
/// the caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FieldKind {
    /// A digit run followed by `:` — an `HH:MM[:SS[.f]]` time.
    Time,
    /// A run reclassified by an embedded `- / .`. Only the SQL-standard `Y-M`
    /// spelling of this is legal; everything else here is a syntax error.
    Date,
    /// Everything else; `split_num_unit` decides what it means.
    Plain,
}

/// One field of an interval literal.
struct Field<'a> {
    /// Borrowed except for a sign that reached across whitespace to its token —
    /// `'- 2 hours'` is `-2 hours`, and that `-2` exists in no slice of the
    /// input.
    text: Cow<'a, str>,
    kind: FieldKind,
}

/// Whether a byte separates fields when it appears *between* them.
///
/// PostgreSQL's splitter is positional, not a character class. Twenty-seven
/// ASCII punctuation characters are pure separators wherever they appear, and
/// `/` and `:` join them here at a field start — but `+ - .` never separate,
/// and `/ :` glue into a token once one has begun. That is why `'1 day, 2
/// hours'` and `'/2 hours'` parse while `'1 day/2 hours'` and `'2 hours/'` are
/// syntax errors. Probed against PostgreSQL 18.4.
fn is_field_gap(c: u8) -> bool {
    is_interval_space(c)
        || c == b'/'
        || c == b':'
        || (c.is_ascii_punctuation() && !matches!(c, b'+' | b'-' | b'.'))
}

/// C's `isspace`, which has `\v` where Rust's `is_ascii_whitespace` does not —
/// `interval '5\x0bdays'` parses in PG. A non-breaking space is *not* space.
fn is_interval_space(c: u8) -> bool {
    c.is_ascii_whitespace() || c == 0x0B
}

/// A byte that can be part of a word or number. Bytes above ASCII count, so a
/// multi-byte character is never split across a slice boundary; such a field
/// simply fails to classify later.
fn is_word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c >= 0x80
}

/// Split an interval literal into its fields.
///
/// Nothing here is a keyword: the `@` of the `postgres_verbose` form is one of
/// the twenty-seven pure separators, which is why `'@ 14 seconds ago'` parses
/// at all and why `'@'` alone is a syntax error rather than a zero interval. A
/// sign at a field start reaches across whitespace to fuse with the token after
/// it (`'- infinity'` is `-infinity`), and a sign with nothing to fuse to is
/// `Err`. Probed against PostgreSQL 18.4.
fn interval_fields(s: &str) -> Result<Vec<Field<'_>>, ()> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if is_field_gap(b[i]) {
            i += 1;
            continue;
        }
        if b[i] == b'+' || b[i] == b'-' {
            let sign = i;
            i += 1;
            let mut j = i;
            while j < b.len() && is_interval_space(b[j]) {
                j += 1;
            }
            // A sign at the very end has nothing to fuse to — `'2 hours -'`.
            if j >= b.len() {
                return Err(());
            }
            if j == i {
                // Glued to its token: one slice, scanned from the sign.
                let (end, kind) = scan_token(b, i);
                out.push(Field {
                    text: Cow::Borrowed(&s[sign..end]),
                    kind,
                });
                i = end;
                continue;
            }
            // Reached across whitespace. Only a real token can be fused to; a
            // second sign or a separator is `22007`.
            if b[j] == b'+' || b[j] == b'-' || is_field_gap(b[j]) {
                return Err(());
            }
            let (end, kind) = scan_token(b, j);
            out.push(Field {
                text: Cow::Owned(format!("{}{}", b[sign] as char, &s[j..end])),
                kind,
            });
            i = end;
            continue;
        }
        let (end, kind) = scan_token(b, i);
        out.push(Field {
            text: Cow::Borrowed(&s[i..end]),
            kind,
        });
        i = end;
    }
    Ok(out)
}

/// Scan one token starting at `start` (past any sign), returning where it ends
/// and what it is.
fn scan_token(b: &[u8], start: usize) -> (usize, FieldKind) {
    let mut i = start;
    // A `.` with no digits before it opens a number — `'.5'`, and a lone `'.'`
    // worth zero. It is *not* the `Date`-forming `.` that follows digits.
    if b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        return (i, FieldKind::Plain);
    }
    if b[i].is_ascii_digit() {
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        match b.get(i) {
            Some(b':') => {
                while i < b.len() && matches!(b[i], b'0'..=b'9' | b':' | b'.') {
                    i += 1;
                }
                return (i, FieldKind::Time);
            }
            Some(&d @ (b'-' | b'/' | b'.')) => {
                i += 1;
                if b.get(i).is_some_and(u8::is_ascii_digit) {
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                    // A second copy of the same separator makes it a three-part
                    // date, which no interval field may be.
                    if b.get(i) == Some(&d) {
                        i += 1;
                        while i < b.len() && (b[i].is_ascii_digit() || b[i] == d) {
                            i += 1;
                        }
                        return (i, FieldKind::Date);
                    }
                    // `1-2` is the year-month spelling; `1.5` is a decimal, and
                    // only the `-` form survives classification later.
                    return (
                        i,
                        if d == b'.' {
                            FieldKind::Plain
                        } else {
                            FieldKind::Date
                        },
                    );
                }
                // A separator with no digit after it — `'1.'`, `'1-'`, `'1/'`.
                // Only a trailing `.` is a number; the others drag whatever
                // follows into a date.
                if d == b'.' && (i >= b.len() || is_field_gap(b[i])) {
                    return (i, FieldKind::Plain);
                }
                while i < b.len() && (is_word_byte(b[i]) || b[i] == d) {
                    i += 1;
                }
                return (i, FieldKind::Date);
            }
            // `+` ends a digit run, unlike `-`: `'1+2 hours'` is two fields.
            Some(&c) if !is_word_byte(c) => return (i, FieldKind::Plain),
            _ => {}
        }
    }
    // A word run, possibly after digits (`'1day'`).
    while i < b.len() && is_word_byte(b[i]) {
        i += 1;
    }
    // `+ - . /` glued to the right of a word poison it into a date; `:` does
    // not, which is what makes `'2 hours:'` and `'1 day:2 hours'` legal.
    if matches!(b.get(i), Some(b'+' | b'-' | b'.' | b'/')) {
        while i < b.len() && (is_word_byte(b[i]) || matches!(b[i], b'+' | b'-' | b'.' | b'/')) {
            i += 1;
        }
        return (i, FieldKind::Date);
    }
    (i, FieldKind::Plain)
}

/// One decoded field, before its unit is known.
enum Piece {
    /// An `HH:MM[:SS[.f]]` token, as signed microseconds.
    Time(i64),
    /// A SQL-standard `Y-M` token, as signed months.
    YearMonth(i128),
    /// A number, with the unit fused to it if it carried one (`'5days'`) or
    /// attached from the word after it (`'5 days'`).
    Value { n: (i128, f64), unit: Option<Unit> },
}

/// Decode every field into a [`Piece`], then attach each unit word to the
/// number before it.
///
/// A word with no number can still be legal: PG lets one trail a time or a
/// year-month token and discards it, so `'1:30 days'` is an hour and a half and
/// `'1:30 days 5 days'` is five days past one — the discarded word claims no
/// field. Exactly one may be absorbed, and a number in between cancels the
/// opportunity, which is what rejects `'1:30 days days'` and `'5 days days'`.
fn decode_fields(
    fields: &[Field<'_>],
    force_negative: bool,
    input: &str,
) -> Result<Vec<Piece>, IntervalError> {
    // `sql_standard`'s leading minus reaches each field's *decoded value*, not
    // the accumulated total: `'-1.5 days 2 hours'` is `-1 14:00:00`, where
    // negating the sum would give `-1 -10:00:00`.
    let signed = |v: i128| if force_negative && v > 0 { -v } else { v };
    let mut out: Vec<Piece> = Vec::with_capacity(fields.len());
    let mut absorb = false;
    for field in fields {
        let tok: &str = &field.text;
        match field.kind {
            FieldKind::Time => {
                let usec = parse_time_token(tok, input)?;
                out.push(Piece::Time(if force_negative && usec > 0 {
                    -usec
                } else {
                    usec
                }));
                absorb = true;
            }
            // The only legal `Date` field is the SQL-standard `Y-M` year-month.
            // Everything else the scanner marked — `'1/5'`, `'1-2-3'`,
            // `'1.days'`, `'day/2'` — reaches here to be rejected.
            FieldKind::Date => {
                out.push(Piece::YearMonth(signed(
                    try_year_month(tok).ok_or_else(|| invalid_syntax(input))?,
                )));
                absorb = true;
            }
            FieldKind::Plain => {
                let tl = tok.to_ascii_lowercase();
                let (num, word) = split_num_unit(&tl);
                match (num.is_empty(), word.is_empty()) {
                    // `1year`, `-1.5days`: number and unit fused in one token.
                    (false, false) => {
                        let unit = unit_from_word(word).ok_or_else(|| invalid_syntax(input))?;
                        out.push(Piece::Value {
                            n: signed_number(parse_number(num, input)?, force_negative),
                            unit: Some(unit),
                        });
                        absorb = false;
                    }
                    (false, true) => {
                        out.push(Piece::Value {
                            n: signed_number(parse_number(num, input)?, force_negative),
                            unit: None,
                        });
                        absorb = false;
                    }
                    (true, false) => {
                        let unit = unit_from_word(word).ok_or_else(|| invalid_syntax(input))?;
                        match out.last_mut() {
                            Some(Piece::Value {
                                unit: slot @ None, ..
                            }) => *slot = Some(unit),
                            _ if absorb => absorb = false,
                            _ => return Err(invalid_syntax(input)),
                        }
                    }
                    (true, true) => return Err(invalid_syntax(input)),
                }
            }
        }
    }
    Ok(out)
}

/// A decoded `(whole, frac)` number under the `sql_standard` sign rule: negated
/// when it is positive, left alone when it already carries a sign.
fn signed_number(n: (i128, f64), force_negative: bool) -> (i128, f64) {
    let (whole, frac) = n;
    if force_negative && (whole > 0 || (whole == 0 && frac > 0.0)) {
        (-whole, -frac)
    } else {
        n
    }
}

/// Assign a unit to every number that still lacks one, and fold the whole list
/// into `acc`.
///
/// A number with no unit of its own is *days* when the next field is a time or
/// an hour count — the SQL `D HH:MM:SS` form, which is why `interval '3 4:05:06'`
/// is three days and six seconds past four rather than three seconds — and
/// otherwise takes `default`, but only as the very last field. Anywhere else it
/// is a syntax error, which is what makes `'1 5 days'` and `'1,5 days'` errors
/// in PG rather than a silent extra second. Read off PostgreSQL 18.4.
fn combine(
    pieces: &[Piece],
    default: Unit,
    acc: &mut Acc,
    input: &str,
) -> Result<(), IntervalError> {
    for (i, piece) in pieces.iter().enumerate() {
        match piece {
            Piece::Time(usec) => {
                acc.claim(TIME_BITS, input)?;
                acc.add_usec(*usec as i128);
            }
            Piece::YearMonth(months) => {
                acc.claim(YEAR_MONTH_BITS, input)?;
                acc.add_months(*months);
            }
            Piece::Value { n, unit: Some(u) } => {
                acc.claim(unit_bit(*u), input)?;
                apply(*n, *u, acc);
            }
            Piece::Value { n, unit: None } => {
                let unit = match pieces.get(i + 1) {
                    Some(Piece::Time(_)) => Unit::Day,
                    Some(Piece::Value {
                        unit: Some(Unit::Hour),
                        ..
                    }) => Unit::Day,
                    None => default,
                    _ => return Err(invalid_syntax(input)),
                };
                acc.claim(unit_bit(unit), input)?;
                apply(*n, unit, acc);
            }
        }
    }
    Ok(())
}

/// `Some(1)`/`Some(-1)` if this field spells an infinity, else `None`.
///
/// Compared rather than lower-cased: this runs on *every* field of *every*
/// interval literal, and folding a `String` per field to match three fixed
/// spellings was measurably the largest single cost in `interval_in`.
fn infinity_sign(field: &str) -> Option<i32> {
    if field.eq_ignore_ascii_case("infinity") || field.eq_ignore_ascii_case("+infinity") {
        Some(1)
    } else if field.eq_ignore_ascii_case("-infinity") {
        Some(-1)
    } else {
        None
    }
}

/// `interval_in`, using `default` as the unit for a bare number that carries no
/// unit of its own (the SQL-standard `INTERVAL '1' DAY` leading-field form), at
/// the default `postgres` style.
pub fn parse_with_default(input: &str, default: Unit) -> Result<Interval, IntervalError> {
    parse_with_style(input, default, IntervalStyle::Postgres)
}

/// `interval_in` under a session `IntervalStyle`.
///
/// Only `sql_standard` reads differently: a leading minus propagates to every
/// later unsigned field, so `'-1 year 2 months'` is fourteen months under that
/// style and ten under the others. It is what makes PG's own `sql_standard`
/// output — which prints one sign for the whole value — read back as the value
/// it was printed from.
pub fn parse_with_style(
    input: &str,
    default: Unit,
    style: IntervalStyle,
) -> Result<Interval, IntervalError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(invalid_syntax(input));
    }

    // ISO-8601 is decided on the string, before it is split into fields: the
    // `P…` designators are punctuation-free but the form is positional, and
    // `'@ P1Y2M'` must stay a syntax error rather than becoming `P1Y2M`.
    let mut acc = Acc::default();
    if is_iso8601(trimmed) {
        parse_iso8601(trimmed, input, &mut acc)?;
        return acc.finish(input);
    }

    let fields = interval_fields(trimmed).map_err(|()| invalid_syntax(input))?;
    // Computed on the whole field list, `ago` included — it carries no sign, so
    // where it sits cannot change the answer, and PG asks the same question of
    // the same array.
    let force_negative = style == IntervalStyle::SqlStandard && leading_minus_propagates(&fields);
    // Nothing but separators — `'@'`, `'@@@'`.
    let Some((last, rest)) = fields.split_last() else {
        return Err(invalid_syntax(input));
    };

    // `infinity` is decided on the *fields*, not the string, so the separators
    // around it are as invisible here as anywhere else (`'@ infinity'` and
    // `'infinity @'` are both accepted) while anything alongside it is a
    // syntax error: `'infinity ago'`, `'infinity years'`, `'+infinity
    // -infinity'`. Probed against PostgreSQL 18.4.
    if let Some(sign) = fields.iter().find_map(|f| infinity_sign(&f.text)) {
        if fields.len() != 1 {
            return Err(invalid_syntax(input));
        }
        return Ok(if sign > 0 { POS_INFINITY } else { NEG_INFINITY });
    }

    // `ago` negates the whole span, and may appear exactly once, as the final
    // field, and never alone: `'2 days ago'` is fine, `'1 day ago ago'`,
    // `'2 minutes ago 5 days'`, `'ago 5 days'` and a bare `'ago'` are 22007.
    let ago = last.text.eq_ignore_ascii_case("ago");
    let fields = if ago { rest } else { &fields[..] };
    if fields.is_empty() || fields.iter().any(|f| f.text.eq_ignore_ascii_case("ago")) {
        return Err(invalid_syntax(input));
    }

    combine(
        &decode_fields(fields, force_negative, input)?,
        default,
        &mut acc,
        input,
    )?;

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
    /// Which units the literal has already named, one bit per [`Unit`].
    mask: u16,
}

/// The bit [`Acc::claim`] tracks for a unit.
///
/// Twelve of them, not the six of the typmod range encoding: `interval_in`
/// distinguishes units the modifier collapses, so `'1 second 5 milliseconds'`,
/// `'1 year 1 decade'` and `'1 month 1 week'` are all legal.
const fn unit_bit(u: Unit) -> u16 {
    1 << (u as u16)
}

/// An `HH:MM[:SS]` token fills every sub-day field at once, which is why PG
/// rejects `'1:30 2 seconds'` and `'1:30 5 ms'` while accepting `'1:30 2 days'`.
const TIME_BITS: u16 = unit_bit(Unit::Microsecond)
    | unit_bit(Unit::Millisecond)
    | unit_bit(Unit::Second)
    | unit_bit(Unit::Minute)
    | unit_bit(Unit::Hour);

/// The SQL-standard `Y-M` token claims the month field only: `'1-2 3 years'` is
/// four years and two months, while `'1-2 3 months'` is a duplicate.
const YEAR_MONTH_BITS: u16 = unit_bit(Unit::Month);

impl Acc {
    /// Claim the fields a token fills. Naming one twice is PG's `22007` —
    /// `'1 day 1 day'`, `'2 hours 3 hours'`, `'1:00 2:00'`.
    fn claim(&mut self, bits: u16, input: &str) -> Result<(), IntervalError> {
        if self.mask & bits != 0 {
            return Err(invalid_syntax(input));
        }
        self.mask |= bits;
        Ok(())
    }

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
    let signed = s.starts_with(['+', '-']);
    let body = s.trim_start_matches(['+', '-']);
    if body.is_empty() {
        return Err(invalid_syntax(input));
    }
    // A sign must be followed by an integer digit: `'.5'` and `'-0.5'` are
    // numbers, `'-.5'` and `'+.5'` are `22007`. Read off PostgreSQL 18.4.
    if signed && !body.starts_with(|c: char| c.is_ascii_digit()) {
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
    // An empty component is zero, so `'1:'` and `'1::'` are one hour — that is
    // what makes the scanner's `HH:` token, and `'1:days'`, work.
    let component = |part: Option<&str>| -> Result<i64, IntervalError> {
        match part.ok_or_else(syntax)? {
            "" => Ok(0),
            digits => digits.parse().map_err(|_| syntax()),
        }
    };
    let hour = component(parts.next())?;
    let min = component(parts.next().or(Some("")))?;
    let (sec, fsec) = match parts.next() {
        None | Some("") => (0, 0),
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
    if y.is_empty() || m.contains('-') {
        return None;
    }
    if !y.bytes().all(|b| b.is_ascii_digit()) || !m.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let years: i128 = y.parse().ok()?;
    // An omitted month count is zero, so `'1-'` is a year — which is what makes
    // `interval '1- 5 days'` five days past a year in PG.
    let months: i128 = if m.is_empty() { 0 } else { m.parse().ok()? };
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
///
/// TODO: accept the ISO-8601 alternative format too — `P0002-10-15T10:30:20`,
/// its basic spelling `P00021015T103020`, and the truncations PG allows
/// (`P0002`, `PT10`, `PT10:30`) are all `22007` here, where PG reads them as
/// `2 years 10 mons 15 days 10:30:20` and so on.
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
    /// `apply_typmod` for the cases that are expected to fit; the overflow ones
    /// are checked against the error directly.
    fn coerce(value: Interval, typmod: i32) -> Interval {
        match apply_typmod(value, typmod) {
            Ok(value) => value,
            Err(error) => panic!("typmod {typmod} rejected {value:?}: {error:?}"),
        }
    }

    /// Every spelling of the modifier, with the `atttypmod` PostgreSQL 18.4
    /// stores for a column declared that way.
    const TYPMOD_CASES: &[(&str, i32, u16, Option<u8>)] = &[
        ("interval(3)", 2147418115, FULL_RANGE, Some(3)),
        ("interval year", 327679, MASK_YEAR, None),
        ("interval month", 196607, MASK_MONTH, None),
        ("interval day", 589823, MASK_DAY, None),
        ("interval hour", 67174399, MASK_HOUR, None),
        ("interval minute", 134283263, MASK_MINUTE, None),
        ("interval second", 268500991, MASK_SECOND, None),
        ("interval second(2)", 268435458, MASK_SECOND, Some(2)),
        (
            "interval year to month",
            458751,
            MASK_YEAR | MASK_MONTH,
            None,
        ),
        ("interval day to hour", 67698687, MASK_DAY | MASK_HOUR, None),
        (
            "interval day to minute",
            201916415,
            MASK_DAY | MASK_HOUR | MASK_MINUTE,
            None,
        ),
        (
            "interval day to second",
            470351871,
            MASK_DAY | MASK_HOUR | MASK_MINUTE | MASK_SECOND,
            None,
        ),
        (
            "interval day to second(4)",
            470286340,
            MASK_DAY | MASK_HOUR | MASK_MINUTE | MASK_SECOND,
            Some(4),
        ),
        (
            "interval hour to minute",
            201392127,
            MASK_HOUR | MASK_MINUTE,
            None,
        ),
        (
            "interval hour to second",
            469827583,
            MASK_HOUR | MASK_MINUTE | MASK_SECOND,
            None,
        ),
        (
            "interval hour to second(1)",
            469762049,
            MASK_HOUR | MASK_MINUTE | MASK_SECOND,
            Some(1),
        ),
        (
            "interval minute to second",
            402718719,
            MASK_MINUTE | MASK_SECOND,
            None,
        ),
        (
            "interval minute to second(0)",
            402653184,
            MASK_MINUTE | MASK_SECOND,
            Some(0),
        ),
    ];

    #[test]
    fn typmod_round_trips_postgres_atttypmod() {
        for &(spelling, typmod, range, precision) in TYPMOD_CASES {
            assert_eq!(pack_typmod(range, precision), typmod, "packing {spelling}");
            assert_eq!(
                unpack_typmod(typmod),
                (range, precision),
                "unpacking {spelling}"
            );
        }
        // No modifier at all reads as "everything, unspecified precision", and
        // applying it is a no-op.
        assert_eq!(unpack_typmod(-1), (FULL_RANGE, None));
    }

    #[test]
    fn typmod_names_its_fields() {
        for &(spelling, typmod, range, precision) in TYPMOD_CASES {
            let mut rendered = "interval".to_string();
            if let Some(fields) = range_name(range) {
                rendered = format!("{rendered} {fields}");
            }
            if let Some(p) = precision {
                rendered = format!("{rendered}({p})");
            }
            assert_eq!(rendered, spelling, "typmod {typmod}");
        }
        assert_eq!(range_name(FULL_RANGE), None);
    }

    /// The lowest admitted field decides what survives; the fields above it are
    /// untouched. Values from PostgreSQL 18.4.
    #[test]
    fn typmod_coerces_the_value() {
        let src = iv("1 year 2 months 3 days 4:05:06.789");
        let apply = |range, precision| format(coerce(src, pack_typmod(range, precision)));

        assert_eq!(apply(MASK_YEAR, None), "1 year");
        assert_eq!(apply(MASK_MONTH, None), "1 year 2 mons");
        assert_eq!(apply(MASK_YEAR | MASK_MONTH, None), "1 year 2 mons");
        assert_eq!(apply(MASK_DAY, None), "1 year 2 mons 3 days");
        assert_eq!(apply(MASK_HOUR, None), "1 year 2 mons 3 days 04:00:00");
        assert_eq!(apply(MASK_MINUTE, None), "1 year 2 mons 3 days 04:05:00");
        assert_eq!(
            apply(MASK_SECOND, None),
            "1 year 2 mons 3 days 04:05:06.789"
        );
        assert_eq!(
            apply(MASK_MINUTE | MASK_SECOND, Some(0)),
            "1 year 2 mons 3 days 04:05:07"
        );
        assert_eq!(
            apply(MASK_DAY | MASK_HOUR | MASK_MINUTE | MASK_SECOND, Some(2)),
            "1 year 2 mons 3 days 04:05:06.79"
        );

        // Rounding is half away from zero, and truncation is toward zero, so a
        // negative interval mirrors the positive one rather than drifting down.
        let round = |s: &str, p| format(coerce(iv(s), pack_typmod(FULL_RANGE, Some(p))));
        assert_eq!(round("0.005 sec", 2), "00:00:00.01");
        assert_eq!(round("-0.005 sec", 2), "-00:00:00.01");
        assert_eq!(round("0.015 sec", 2), "00:00:00.02");
        assert_eq!(
            format(coerce(iv("-1 day -2:30:00"), pack_typmod(MASK_HOUR, None))),
            "-1 days -02:00:00"
        );

        // `±infinity` and "no modifier" pass through untouched.
        assert_eq!(
            coerce(POS_INFINITY, pack_typmod(MASK_YEAR, None)),
            POS_INFINITY
        );
        assert_eq!(coerce(src, -1), src);
    }

    /// A `usec` at either `i64` extreme is a perfectly good interval, but it has
    /// no room for the half-unit rounding moves it by, so declaring a precision
    /// on it is an error rather than a saturated value. PostgreSQL 18.4:
    /// `SELECT interval '2562047788:00:54.775807' second(2)` is
    /// `ERROR: interval out of range`.
    #[test]
    fn rounding_overflow_is_out_of_range() {
        let at = |usec| Interval {
            months: 0,
            days: 0,
            usec,
        };
        for src in [at(i64::MAX), at(i64::MIN)] {
            assert_eq!(
                apply_typmod(src, pack_typmod(MASK_SECOND, Some(2))),
                Err(out_of_range()),
                "{src:?}"
            );

            // Full precision, and no precision at all, keep rounding out of the
            // way entirely — the value stands.
            assert_eq!(coerce(src, pack_typmod(MASK_SECOND, Some(6))), src);
            assert_eq!(coerce(src, pack_typmod(FULL_RANGE, None)), src);
        }

        // The boundary is the half-unit, and it sits symmetrically: a value
        // within half a step of either extreme has nowhere to round to. Read
        // off PostgreSQL 18.4 on both sides — the negative half is what a
        // magnitude-based rounding gets wrong, because negating `i64::MIN + half`
        // overflows while moving it outwards does not.
        let second_2 = pack_typmod(FULL_RANGE, Some(2));
        let second_0 = pack_typmod(FULL_RANGE, Some(0));
        assert_eq!(
            coerce(at(i64::MAX - 5000), second_2).usec,
            9_223_372_036_854_770_000
        );
        assert_eq!(
            coerce(at(i64::MIN + 5000), second_2).usec,
            -9_223_372_036_854_770_000
        );
        assert_eq!(
            coerce(at(i64::MIN + 500_000), second_0).usec,
            -9_223_372_036_854_000_000
        );
        for (usec, typmod) in [
            (i64::MAX - 4999, second_2),
            (i64::MIN + 4999, second_2),
            (i64::MIN + 499_999, second_0),
        ] {
            assert_eq!(
                apply_typmod(at(usec), typmod),
                Err(out_of_range()),
                "{usec}"
            );
        }

        // Away from the extremes the two branches must stay bit-identical to
        // rounding the magnitude — moving outwards then truncating toward zero
        // is the same operation, and only the overflow point differs.
        for (usec, want) in [
            (15_000, 20_000),
            (-15_000, -20_000),
            (5_000, 10_000),
            (-5_000, -10_000),
            (4_999, 0),
            (-4_999, 0),
        ] {
            assert_eq!(coerce(at(usec), second_2).usec, want, "{usec}");
        }
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
            date_part("bogus", iv("1 day"))
                .expect_err("`bogus` names no interval field")
                .sqlstate,
            "22023"
        );

        Ok(())
    }

    /// The SQL `D HH:MM:SS` form: a unit-less number immediately before a time is
    /// days regardless of the default unit, so the default cannot turn it into
    /// seconds. Pinned against PostgreSQL 18.4.
    #[test]
    fn a_number_before_a_time_is_days() -> anyhow::Result<()> {
        // The default unit is what a *lone* number takes...
        assert_eq!(format(parse("3")?), "00:00:03");
        // ...but never the leading number of a `D HH:MM:SS`.
        assert_eq!(format(parse("3 4:05:06")?), "3 days 04:05:06");
        assert_eq!(format(parse("3 4:05")?), "3 days 04:05:00");
        assert_eq!(format(parse("3 4:05:06.75")?), "3 days 04:05:06.75");
        // Holds for every default a field qualifier could supply.
        for unit in [
            Unit::Second,
            Unit::Minute,
            Unit::Hour,
            Unit::Month,
            Unit::Year,
        ] {
            assert_eq!(
                format(parse_with_default("3 4:05:06", unit)?),
                "3 days 04:05:06",
                "default {unit:?} must not retype the leading number"
            );
        }
        // Signs are independent, and an explicit unit word still wins.
        assert_eq!(format(parse("-3 4:05:06")?), "-3 days +04:05:06");
        assert_eq!(format(parse("3 -4:05:06")?), "3 days -04:05:06");
        assert_eq!(format(parse("3 days 4:05:06")?), "3 days 04:05:06");
        // A bare time keeps its own fields.
        assert_eq!(format(parse("4:05:06")?), "04:05:06");

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
        assert_eq!(
            parse("garbage")
                .expect_err("`garbage` carries neither a number nor a unit word")
                .sqlstate,
            "22007"
        );
        // A field beyond i32 is PG's "interval field value out of range" (22015).
        assert_eq!(
            parse("2147483648 mons")
                .expect_err("2147483648 months is one past the i32 the field holds")
                .sqlstate,
            "22015"
        );
        assert_eq!(format(iv("2147483647 mons")), "178956970 years 7 mons");
    }

    #[test]
    fn overflow_errors_instead_of_panicking() {
        // Regression: these all used to overflow i64/i32 and panic (or wrap);
        // each must now return a clean error, matching PG.
        // A huge hours field in the colon form (was: i64 overflow panic).
        assert_eq!(
            parse("3000000000:00:00")
                .expect_err("three billion hours is more microseconds than an i64 holds")
                .sqlstate,
            "22015"
        );
        // A bare integer beyond i64 (was: reported as 22007 syntax error).
        assert_eq!(
            parse("99999999999999999999 days")
                .expect_err("a twenty-digit day count is a field overflow, not bad syntax")
                .sqlstate,
            "22015"
        );
        // A field beyond i32 (was: silently narrowed).
        assert_eq!(
            parse("3000000000 days")
                .expect_err("three billion days does not fit the i32 day field")
                .sqlstate,
            "22015"
        );
        assert_eq!(
            parse("3000000000 mons")
                .expect_err("three billion months does not fit the i32 month field")
                .sqlstate,
            "22015"
        );
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
        assert_eq!(
            parse("00:00:61")
                .expect_err("61 is past the 60 the seconds field admits")
                .sqlstate,
            "22015"
        );
        assert_eq!(
            parse("01:60:00")
                .expect_err("60 is past the 59 the minutes field admits")
                .sqlstate,
            "22015"
        );
        assert_eq!(
            parse("1 day 00:75:00")
                .expect_err("a 75-minute field is rejected even behind a day count")
                .sqlstate,
            "22015"
        );
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
                .expect_err("`dow` is a datetime field with no meaning for an interval")
                .message
                .contains("not supported")
        );

        Ok(())
    }

    /// `IntervalStyle = postgres_verbose`, the style `pg_get_viewdef` renders an
    /// interval constant in. Every expectation was read off a live PostgreSQL
    /// 18.4 under `SET IntervalStyle='postgres_verbose'`.
    #[test]
    fn verbose_style_matches_pg() -> anyhow::Result<()> {
        let cases = [
            ("0", "@ 0"),
            ("00:00", "@ 0"),
            ("1 sec", "@ 1 sec"),
            ("2 secs", "@ 2 secs"),
            ("1.5 secs", "@ 1.5 secs"),
            ("-1 sec", "@ 1 sec ago"),
            ("0.000001 sec", "@ 0.000001 secs"),
            ("1 year", "@ 1 year"),
            ("-1 year -2 mons", "@ 1 year 2 mons ago"),
            // A mixed-sign span keeps its per-field signs, and `-1` pluralizes
            // every unit but seconds, which PG makes singular on the magnitude.
            ("1 mon -1 day", "@ 1 mon -1 days"),
            ("1 day -1 hour", "@ 1 day -1 hours"),
            ("1 day -1 sec", "@ 1 day -1 sec"),
            ("25:00:00", "@ 25 hours"),
            (
                "1 year 2 mons 3 days 04:05:06.7",
                "@ 1 year 2 mons 3 days 4 hours 5 mins 6.7 secs",
            ),
            // Not the infinity sentinel: `i32::MIN` months, which must not
            // overflow while being negated for the `ago` form.
            ("-178956970 years -8 mons", "@ 178956970 years 8 mons ago"),
        ];
        for (input, want) in cases {
            let iv = parse(input).map_err(|e| anyhow::anyhow!("{input}: {e:?}"))?;
            assert_eq!(format_verbose(iv), want, "input {input}");
        }
        assert_eq!(format_verbose(POS_INFINITY), "infinity");
        assert_eq!(format_verbose(NEG_INFINITY), "-infinity");

        Ok(())
    }

    /// Every style's rendering of the same span, read off a live PostgreSQL
    /// 18.4. The literals are parsed under `IntervalStyle = postgres` — the
    /// `sql_standard` *input* rule (a leading minus propagating to later
    /// unsigned fields) would otherwise change what is being rendered.
    ///
    /// `(input, postgres, postgres_verbose, sql_standard, iso_8601)`
    const STYLE_CASES: &[(&str, &str, &str, &str, &str)] = &[
        ("0", "00:00:00", "@ 0", "0", "PT0S"),
        ("1-2", "1 year 2 mons", "@ 1 year 2 mons", "1-2", "P1Y2M"),
        (
            "-1-2",
            "-1 years -2 mons",
            "@ 1 year 2 mons ago",
            "-1-2",
            "P-1Y-2M",
        ),
        (
            "1 2:03:04",
            "1 day 02:03:04",
            "@ 1 day 2 hours 3 mins 4 secs",
            "1 2:03:04",
            "P1DT2H3M4S",
        ),
        (
            "-1 days -2:03:04",
            "-1 days -02:03:04",
            "@ 1 day 2 hours 3 mins 4 secs ago",
            "-1 2:03:04",
            "P-1DT-2H-3M-4S",
        ),
        (
            "-0.1 sec",
            "-00:00:00.1",
            "@ 0.1 secs ago",
            "-0:00:00.1",
            "PT-0.1S",
        ),
        ("1 min", "00:01:00", "@ 1 min", "0:01:00", "PT1M"),
        ("1 mon", "1 mon", "@ 1 mon", "0-1", "P1M"),
        ("1 day", "1 day", "@ 1 day", "1 0:00:00", "P1D"),
        (
            "-14 mons",
            "-1 years -2 mons",
            "@ 1 year 2 mons ago",
            "-1-2",
            "P-1Y-2M",
        ),
        // The `-0` trap: the year-month group's sign comes from the whole
        // month count, not from `months / 12`.
        ("-10 mons", "-10 mons", "@ 10 mons ago", "-0-10", "P-10M"),
        ("-1 mon", "-1 mons", "@ 1 mon ago", "-0-1", "P-1M"),
        ("1 year -1 mon", "11 mons", "@ 11 mons", "0-11", "P11M"),
        // Mixed signs, or both groups populated, force sql_standard's
        // three-group fallback.
        (
            "1 day -1 hours",
            "1 day -01:00:00",
            "@ 1 day -1 hours",
            "+0-0 +1 -1:00:00",
            "P1DT-1H",
        ),
        (
            "-1 days +1 hours",
            "-1 days +01:00:00",
            "@ 1 day -1 hours ago",
            "+0-0 -1 +1:00:00",
            "P-1DT1H",
        ),
        (
            "-1 mon -1 hour",
            "-1 mons -01:00:00",
            "@ 1 mon 1 hour ago",
            "-0-1 +0 -1:00:00",
            "P-1MT-1H",
        ),
        (
            "-1 mon -1 day",
            "-1 mons -1 days",
            "@ 1 mon 1 day ago",
            "-0-1 -1 +0:00:00",
            "P-1M-1D",
        ),
        (
            "1 mon 1 day",
            "1 mon 1 day",
            "@ 1 mon 1 day",
            "+0-1 +1 +0:00:00",
            "P1M1D",
        ),
        (
            "-1 mon -0.000001 sec",
            "-1 mons -00:00:00.000001",
            "@ 1 mon 0.000001 secs ago",
            "-0-1 +0 -0:00:00.000001",
            "P-1MT-0.000001S",
        ),
        (
            "90000 hours",
            "90000:00:00",
            "@ 90000 hours",
            "90000:00:00",
            "PT90000H",
        ),
        (
            "2:03:04.45679",
            "02:03:04.45679",
            "@ 2 hours 3 mins 4.45679 secs",
            "2:03:04.45679",
            "PT2H3M4.45679S",
        ),
        (
            "1 year 2 mons -3 days -04:05:06.7",
            "1 year 2 mons -3 days -04:05:06.7",
            "@ 1 year 2 mons -3 days -4 hours -5 mins -6.7 secs",
            "+1-2 -3 -4:05:06.7",
            "P1Y2M-3DT-4H-5M-6.7S",
        ),
        (
            "1 year 2 mons 3 days 04:05:06.7",
            "1 year 2 mons 3 days 04:05:06.7",
            "@ 1 year 2 mons 3 days 4 hours 5 mins 6.7 secs",
            "+1-2 +3 +4:05:06.7",
            "P1Y2M3DT4H5M6.7S",
        ),
        ("1 sec", "00:00:01", "@ 1 sec", "0:00:01", "PT1S"),
        ("-1 sec", "-00:00:01", "@ 1 sec ago", "-0:00:01", "PT-1S"),
        ("24:00:00", "24:00:00", "@ 24 hours", "24:00:00", "PT24H"),
        (
            "-00:00:00.000001",
            "-00:00:00.000001",
            "@ 0.000001 secs ago",
            "-0:00:00.000001",
            "PT-0.000001S",
        ),
        // The overflow canary: `i32::MIN` days and `i64::MIN` micros are
        // reachable intervals, so no style may negate a field in place.
        (
            "-2147483647 months -2147483648 days -9223372036854775808 microseconds",
            "-178956970 years -7 mons -2147483648 days -2562047788:00:54.775808",
            "@ 178956970 years 7 mons 2147483648 days 2562047788 hours 54.775808 secs ago",
            "-178956970-7 -2147483648 -2562047788:00:54.775808",
            "P-178956970Y-7M-2147483648DT-2562047788H-54.775808S",
        ),
    ];

    #[test]
    fn every_style_matches_pg() {
        for (input, pg, verbose, sql, iso) in STYLE_CASES {
            let v = iv(input);
            assert_eq!(&format(v), pg, "postgres: {input}");
            assert_eq!(&format_verbose(v), verbose, "postgres_verbose: {input}");
            assert_eq!(&format_sql_standard(v), sql, "sql_standard: {input}");
            assert_eq!(&format_iso8601(v), iso, "iso_8601: {input}");
            // `format_with` is the dispatcher every caller goes through.
            for (style, want) in [
                (IntervalStyle::Postgres, pg),
                (IntervalStyle::PostgresVerbose, verbose),
                (IntervalStyle::SqlStandard, sql),
                (IntervalStyle::Iso8601, iso),
            ] {
                assert_eq!(&format_with(v, style), want, "{style:?}: {input}");
            }
        }
        // Both infinities render the same under every style.
        for style in [
            IntervalStyle::Postgres,
            IntervalStyle::PostgresVerbose,
            IntervalStyle::SqlStandard,
            IntervalStyle::Iso8601,
        ] {
            assert_eq!(format_with(POS_INFINITY, style), "infinity", "{style:?}");
            assert_eq!(format_with(NEG_INFINITY, style), "-infinity", "{style:?}");
        }
    }

    #[test]
    fn style_names_round_trip() {
        for style in [
            IntervalStyle::Postgres,
            IntervalStyle::PostgresVerbose,
            IntervalStyle::SqlStandard,
            IntervalStyle::Iso8601,
        ] {
            assert_eq!(IntervalStyle::from_name(style.name()), Some(style));
        }
        // PG matches the name case-insensitively, and does nothing else to it:
        // padding is part of the value, so `SET IntervalStyle TO ' postgres '`
        // is rejected. Read off PostgreSQL 18.4.
        assert_eq!(
            IntervalStyle::from_name("SQL_Standard"),
            Some(IntervalStyle::SqlStandard)
        );
        assert_eq!(IntervalStyle::from_name(" postgres "), None);
        assert_eq!(IntervalStyle::from_name("postgres "), None);
        assert_eq!(IntervalStyle::from_name("bogus"), None);
        assert_eq!(IntervalStyle::default(), IntervalStyle::Postgres);
    }

    /// The error every rejected literal reports, so the cases below assert the
    /// SQLSTATE and the quoted input rather than just "it failed".
    fn rejects(input: &str) {
        match parse(input) {
            Ok(v) => panic!("{input:?} should be a syntax error, parsed as {v:?}"),
            Err(e) => {
                assert_eq!(e.sqlstate, INVALID_DATETIME_FORMAT, "{input:?}");
                assert_eq!(
                    e.message,
                    format!("invalid input syntax for type interval: \"{input}\""),
                    "{input:?}"
                );
            }
        }
    }

    /// `@` is a delimiter, not a keyword: it disappears anywhere it appears,
    /// and so does every other ASCII punctuation character except `+ - . :`.
    /// A literal made of nothing but delimiters has no fields at all, which is
    /// a syntax error rather than a zero interval. Read off PostgreSQL 18.4.
    #[test]
    fn punctuation_is_a_field_delimiter() {
        for (input, want) in [
            ("@ 14 seconds ago", "-00:00:14"),
            ("@1 day", "1 day"),
            ("@@ 5 days", "5 days"),
            ("5 days @", "5 days"),
            ("5 days ago @", "-5 days"),
            ("1 day @ 2 hours", "1 day 02:00:00"),
            ("1 day, 2 hours", "1 day 02:00:00"),
            ("1 day # 2 hours", "1 day 02:00:00"),
            ("1 day_2 hours", "1 day 02:00:00"),
            ("1 day(2 hours", "1 day 02:00:00"),
            ("@ 1 day 2 hours ago", "-1 days -02:00:00"),
        ] {
            assert_eq!(out(input), want, "input {input}");
        }
        for input in ["@", "@@@", "@ 30 eons ago", "@ P1Y2M"] {
            rejects(input);
        }
        // `+ - . /` glued to the right of a token poison it into a date, which
        // no interval field may be.
        for input in ["1 day-2 hours", "1 day+2 hours", "1 day.5", "1 day/2 hours"] {
            rejects(input);
        }
    }

    /// `+ - . / :` are positional rather than separators, and the position is
    /// what decides: `/` and `:` are skipped between fields but glue into a
    /// token once one has begun, `+` ends a digit run where `-` continues it,
    /// and a sign at a field start reaches across whitespace to fuse with the
    /// token after it. Read off PostgreSQL 18.4.
    #[test]
    fn the_five_special_characters_are_positional() {
        for (input, want) in [
            // A sign fuses across whitespace with whatever follows.
            ("- 2 hours", "-02:00:00"),
            ("+ 2 hours", "02:00:00"),
            ("1 day - 2 hours", "1 day -02:00:00"),
            ("1 day + 2 hours", "1 day 02:00:00"),
            ("- infinity", "-infinity"),
            ("+ infinity", "infinity"),
            // `/` and `:` separate between fields...
            ("/2 hours", "02:00:00"),
            (":2 hours", "02:00:00"),
            ("1 day / 2 hours", "1 day 02:00:00"),
            // ...and `:` does not glue to a word, so these stay two fields.
            ("2 hours:", "02:00:00"),
            ("1 day:2 hours", "1 day 02:00:00"),
            // An omitted time component is zero.
            ("1:", "01:00:00"),
            ("1::", "01:00:00"),
            // A trailing `.` ends a number; `1-` is a bare year.
            ("1.", "00:00:01"),
            ("1. days", "1 day"),
            (".", "00:00:00"),
            (".5", "00:00:00.5"),
            (".days", "00:00:00"),
            ("1- 5 days", "1 year 5 days"),
            ("1-.5 days", "1 year 12:00:00"),
            // `\v` is whitespace to PG even though it is not to Rust.
            ("5\u{0b}days", "5 days"),
        ] {
            assert_eq!(out(input), want, "input {input}");
        }
        for input in [
            // `/` glued to the right of a token is not a separator.
            "1 day/2 hours",
            "2 hours/",
            "1/5 days",
            "1.days",
            "1-days",
            "day/2",
            // A three-part date is not an interval field.
            "1-5-5 days",
            "1/5/5 days",
            // A sign with nothing to fuse to.
            "2 hours -",
            "--2 hours",
            "-, 2 hours",
            "- @ 2 hours",
            // A sign must be followed by a digit.
            "-.5",
            "+.5",
            // A non-ASCII byte is its own field and fails to classify — the
            // point being that slicing it must not panic.
            "5 day\u{e9}",
        ] {
            rejects(input);
        }
    }

    /// A literal may name each field only once, and a `HH:MM:SS` token names
    /// every sub-day field at once while a `Y-M` token names only the month.
    /// Read off PostgreSQL 18.4.
    #[test]
    fn a_field_may_be_named_only_once() {
        for input in [
            "1 day 1 day",
            "2 hours 3 hours",
            "1 year 1 year",
            "1:00 2:00",
            "1 day 2:00 3:00",
            // the time token owns hours through microseconds
            "1:30 2 hours",
            "1:30 2 minutes",
            "1:30 2 seconds",
            "1:30 5 ms",
            "1:30 2 us",
            // the year-month token owns months
            "1-2 3 months",
        ] {
            rejects(input);
        }
        for (input, want) in [
            // Units the typmod encoding collapses are still distinct on input.
            ("1 second 5 milliseconds", "00:00:01.005"),
            ("1 year 1 decade", "11 years"),
            ("1 month 1 week", "1 mon 7 days"),
            ("1 week 1 day", "8 days"),
            // A time token leaves the day and month fields free, and a
            // year-month token leaves the sub-day ones free.
            ("1:30 2 days", "2 days 01:30:00"),
            ("1-2 3 years", "4 years 2 mons"),
            ("1:30 1-2", "1 year 2 mons 01:30:00"),
            ("1-2 1:30", "1 year 2 mons 01:30:00"),
            // A fractional cascade is invisible to the mask, so the finer unit
            // it spills into may still be named.
            ("1.5 days 2 hours", "1 day 14:00:00"),
        ] {
            assert_eq!(out(input), want, "input {input}");
        }
    }

    /// A number with no unit of its own is days when the next field is a time
    /// or an hour count, and otherwise takes the default unit — but only as the
    /// very last field. Anywhere else it is a syntax error, which is what makes
    /// `'1 5 days'` and `'1,5 days'` errors rather than a silent extra second.
    #[test]
    fn a_bare_number_is_days_before_an_hour_and_default_only_last() {
        for (input, want) in [
            ("2 3 hours", "2 days 03:00:00"),
            ("2 3 h", "2 days 03:00:00"),
            ("2 3 hrs", "2 days 03:00:00"),
            ("2 3:00", "2 days 03:00:00"),
            ("1 mon 3 2:00", "1 mon 3 days 02:00:00"),
            ("1 mon 2 3 hours", "1 mon 2 days 03:00:00"),
            ("1+2 hours", "1 day 02:00:00"),
            ("1 day 2 hours 3", "1 day 02:00:03"),
            ("5 days 1", "5 days 00:00:01"),
        ] {
            assert_eq!(out(input), want, "input {input}");
        }
        for input in [
            // a bare number before anything but a time or an hour count
            "2 3 minutes",
            "2 3 seconds",
            "2 3 days",
            "2 3 months",
            "2 1-2",
            "1 2 3 hours",
            "2 3 4:05",
            "1 5 days",
            "1 5",
            "1,5 days",
            "..",
            "5 days ..",
            "1 day . 2 hours",
            "3.5:00",
            // the promoted or defaulted unit is already taken
            "1 day 3 4:05:06",
            "1 day 3 2:00",
            "1 day 2:00 3",
            "3 4:05:06 7",
            "3 2:00 4",
        ] {
            rejects(input);
        }
        // The leading-field form defaults the trailing bare number, and the
        // duplicate rule applies to it too.
        assert_eq!(
            format(parse_with_default("1", Unit::Day).expect("one day")),
            "1 day"
        );
        for input in ["5 days 1", "1 5"] {
            assert!(
                parse_with_default(input, Unit::Day).is_err(),
                "input {input}"
            );
        }
    }

    /// One unit word may trail a time or year-month token, and is discarded
    /// rather than applied — it claims no field, so a later use of that unit is
    /// still legal. Only one, and a number in between cancels the opportunity.
    #[test]
    fn a_stray_unit_word_is_absorbed_once_after_a_time() {
        for (input, want) in [
            ("1:30 days", "01:30:00"),
            ("1:30 months", "01:30:00"),
            ("1-2 hours", "1 year 2 mons"),
            ("1:days", "01:00:00"),
            ("1:30 days 5 days", "5 days 01:30:00"),
        ] {
            assert_eq!(out(input), want, "input {input}");
        }
        for input in ["1:30 days days", "5 days days", "days 5", "5 hours days"] {
            rejects(input);
        }
    }

    /// Under `sql_standard` a leading minus propagates to every later unsigned
    /// field — all-or-nothing, and any later sign of its own turns it off for
    /// the whole literal. It does not reach the ISO-8601 form, and `ago`
    /// composes after it. Read off PostgreSQL 18.4.
    #[test]
    fn sql_standard_input_propagates_a_leading_minus() {
        let sql =
            |input: &str| match parse_with_style(input, Unit::Second, IntervalStyle::SqlStandard) {
                Ok(v) => format(v),
                Err(e) => panic!("{input:?}: {e:?}"),
            };
        for (input, pg_style, sql_style) in [
            ("-1 2:03:04", "-1 days +02:03:04", "-1 days -02:03:04"),
            // The discriminator for per-field rather than per-total negation:
            // negating the sum would give `-1 days -10:00:00`.
            (
                "-1.5 days 2 hours",
                "-1 days -10:00:00",
                "-1 days -14:00:00",
            ),
            ("-1 year 2 months", "-10 mons", "-1 years -2 mons"),
            ("-0 2:00", "02:00:00", "-02:00:00"),
            ("-1:30 2 days", "2 days -01:30:00", "-2 days -01:30:00"),
            ("- 1 day 2 hours", "-1 days +02:00:00", "-1 days -02:00:00"),
            ("@ -1 day 2 hours", "-1 days +02:00:00", "-1 days -02:00:00"),
            // `ago` negates the whole span afterwards.
            ("-1 day 2 hours ago", "1 day -02:00:00", "1 day 02:00:00"),
            // A later sign of its own — `+` as much as `-` — turns it off.
            ("1 day -2 hours", "1 day -02:00:00", "1 day -02:00:00"),
            ("-1 day +2 hours", "-1 days +02:00:00", "-1 days +02:00:00"),
            // A single field has nothing to propagate to.
            ("-1", "-00:00:01", "-00:00:01"),
            ("-2 hours", "-02:00:00", "-02:00:00"),
            ("-1-2", "-1 years -2 mons", "-1 years -2 mons"),
            // The ISO-8601 form is exempt.
            ("P-1Y2M", "-10 mons", "-10 mons"),
        ] {
            assert_eq!(out(input), pg_style, "postgres: {input}");
            assert_eq!(sql(input), sql_style, "sql_standard: {input}");
        }
    }

    /// The predicate the binder folds on. It must be conservative: a `false`
    /// that should be `true` silently reads a literal under the wrong style,
    /// and a `true` that should be `false` only costs a deferred fold.
    #[test]
    fn style_sensitive_is_conservative() {
        for input in [
            "-1 2:03:04",
            "-1 year 2 months",
            "- 1 day 2 hours",
            "@ -1 day 2 hours",
            "-1 day 2 hours ago",
            // Over-reported: the unit word is a second *field*, but it decodes
            // into the number before it, so there is nothing for the sign to
            // propagate to. Erring this way only costs a deferred fold.
            "-2 hours",
        ] {
            assert!(style_sensitive(input), "input {input}");
        }
        for input in [
            // one field has nothing to propagate to
            "-1",
            "-1-2",
            "-infinity",
            // a later sign turns the rule off
            "1 day -2 hours",
            "-1 day +2 hours",
            // no leading minus at all
            "1 day 2 hours",
            "2 days ago",
            // ISO-8601 is exempt, and garbage keeps its diagnostic where it was
            "P-1Y2M",
            "garbage",
            "1 day/2 hours",
            "",
        ] {
            assert!(!style_sensitive(input), "input {input}");
        }
    }

    /// `ago` negates the whole span. It may appear once, must be the last
    /// field, and cannot stand alone.
    #[test]
    fn ago_appears_once_and_last() {
        assert_eq!(out("2 days ago"), "-2 days");
        assert_eq!(out("@ 14 seconds ago"), "-00:00:14");
        assert_eq!(out("1 mon -1 day ago"), "-1 mons +1 day");
        for input in [
            "ago",
            "@ ago",
            "1 day ago ago",
            "42 days 2 seconds ago ago",
            "2 minutes ago 5 days",
            "ago 5 days",
        ] {
            rejects(input);
        }
    }

    /// An infinity must be the whole value. Because that is decided on the
    /// fields, the delimiters around it are as invisible as anywhere else.
    #[test]
    fn infinity_must_stand_alone() {
        for (input, want) in [
            ("infinity", "infinity"),
            ("+infinity", "infinity"),
            ("-infinity", "-infinity"),
            ("@ infinity", "infinity"),
            ("infinity @", "infinity"),
            ("@ -infinity", "-infinity"),
            ("  Infinity  ", "infinity"),
        ] {
            assert_eq!(out(input), want, "input {input}");
        }
        for input in [
            "infinity ago",
            "infinity years",
            "infinity infinity",
            "+infinity -infinity",
            "-infinity -infinity",
            "1 day infinity",
        ] {
            rejects(input);
        }
    }
}
