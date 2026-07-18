//! `timestamp` (without time zone): parsing, output, and the field functions
//! (`date_part`/`extract`/`date_trunc`/`isfinite`/`make_timestamp`).
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior — the ISO output format, the field values, and the SQLSTATE/message
//! of range and syntax errors — pinned by differential tests against real PG,
//! implemented independently. The calendar conversions use Howard Hinnant's
//! public-domain `days_from_civil`/`civil_from_days` algorithm
//! (http://howardhinnant.github.io/date_algorithms.html), not PG's source.
//!
//! Representation: microseconds since the PostgreSQL epoch
//! (2000-01-01 00:00:00), held in an `i64`. `i64::MIN`/`i64::MAX` are the
//! `-infinity`/`infinity` sentinels, so the natural integer order already
//! sorts them correctly (they compare less/greater than every finite value).
//!
//! Deviations from PG, acceptable while no passing test needs them: `timestamp`
//! precision modifiers (`timestamp(2)`) are ignored (full microsecond
//! resolution is kept), and the input grammar covers ISO 8601, the traditional
//! `Mon DD HH:MM:SS YYYY` form, and the `infinity`/`epoch` specials — a trailing
//! time zone is accepted and ignored (this type has no zone), but the
//! current-relative specials (`now`/`today`/...) need a transaction clock and
//! are not supported.

use crate::NumericVal;

// SQLSTATEs, kept as literals here (the types crate does not depend on the
// protocol crate; the binder/executor map these to `sqlstate::*`).
pub(crate) const INVALID_DATETIME_FORMAT: &str = "22007";
pub(crate) const DATETIME_FIELD_OVERFLOW: &str = "22008";
pub(crate) const INVALID_PARAMETER_VALUE: &str = "22023";
/// `time zone displacement out of range` — a numeric offset beyond ±15:59:59.
pub(crate) const INVALID_TIME_ZONE_DISPLACEMENT: &str = "22009";

/// `-infinity` / `+infinity` sentinels, matching PG's `DT_NOBEGIN`/`DT_NOEND`.
pub const NEG_INFINITY: i64 = i64::MIN;
pub const POS_INFINITY: i64 = i64::MAX;

const USECS_PER_DAY: i64 = 86_400_000_000;
const USECS_PER_HOUR: i64 = 3_600_000_000;
const USECS_PER_MINUTE: i64 = 60_000_000;
const USECS_PER_SEC: i64 = 1_000_000;
const SECS_PER_DAY: i64 = 86_400;

/// Julian day of 2000-01-01 (the PG epoch) and 1970-01-01 (the Unix epoch).
const POSTGRES_EPOCH_JDATE: i64 = 2_451_545;
const UNIX_EPOCH_JDATE: i64 = 2_440_588;
/// Days from the Unix epoch to the PG epoch; `epoch`'s value and the offset
/// used to convert a timestamp to seconds-since-1970 for the `epoch` field.
const EPOCH_MINUS_PG_DAYS: i64 = UNIX_EPOCH_JDATE - POSTGRES_EPOCH_JDATE; // -10957

/// A parse/range error, carrying the SQLSTATE and message PG reports.
#[derive(Clone, Debug, PartialEq)]
pub struct TimestampError {
    pub sqlstate: &'static str,
    pub message: String,
}

fn invalid_syntax(input: &str) -> TimestampError {
    TimestampError {
        sqlstate: INVALID_DATETIME_FORMAT,
        message: format!("invalid input syntax for type timestamp: \"{input}\""),
    }
}

fn field_out_of_range(input: &str) -> TimestampError {
    TimestampError {
        sqlstate: DATETIME_FIELD_OVERFLOW,
        message: format!("date/time field value out of range: \"{input}\""),
    }
}

fn units_not_recognized(unit: &str) -> TimestampError {
    TimestampError {
        sqlstate: INVALID_PARAMETER_VALUE,
        message: format!("timestamp units \"{unit}\" not recognized"),
    }
}

pub fn is_finite(micros: i64) -> bool {
    micros != NEG_INFINITY && micros != POS_INFINITY
}

// --- proleptic-Gregorian calendar conversions ------------------------------
//
// These use Howard Hinnant's public-domain `days_from_civil` /
// `civil_from_days` algorithm (http://howardhinnant.github.io/date_algorithms.html),
// which counts days from the Unix epoch (1970-01-01). We shift by
// `UNIX_EPOCH_JDATE` to express the result as a Julian day number so the rest of
// the module can keep speaking in Julian days.

/// Days from 1970-01-01 for a proleptic-Gregorian `(year, month, day)`, where
/// `year` is astronomical (1 BC is year 0, 2 BC is -1, ...).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`]: days-from-1970 → `(year, month, day)`.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Gregorian (proleptic) `(year, month, day)` → Julian day number.
fn date2j(y: i64, m: i64, d: i64) -> i64 {
    days_from_civil(y, m, d) + UNIX_EPOCH_JDATE
}

/// Inverse of [`date2j`]: Julian day number → `(year, month, day)`.
fn j2date(jd: i64) -> (i64, i64, i64) {
    civil_from_days(jd - UNIX_EPOCH_JDATE)
}

/// Day of week, 0 = Sunday .. 6 = Saturday. This is the plain modular relation
/// between the Julian day number and the weekday.
fn j2day(jd: i64) -> i64 {
    (jd + 1).rem_euclid(7)
}

/// Broken-down finite timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tm {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub min: i64,
    pub sec: i64,
    pub usec: i64,
}

/// Construct a [`Tm`] from calendar fields (astronomical year). Used by
/// `timestamptz` to encode its range boundaries.
pub(crate) fn tm(year: i64, month: i64, day: i64, hour: i64, min: i64, sec: i64, usec: i64) -> Tm {
    Tm { year, month, day, hour, min, sec, usec }
}

/// Split a finite microsecond timestamp into calendar fields.
pub(crate) fn decode(micros: i64) -> Tm {
    let mut time = micros % USECS_PER_DAY;
    let mut date = micros / USECS_PER_DAY;
    if time < 0 {
        time += USECS_PER_DAY;
        date -= 1;
    }
    let (year, month, day) = j2date(date + POSTGRES_EPOCH_JDATE);
    let hour = time / USECS_PER_HOUR;
    time -= hour * USECS_PER_HOUR;
    let min = time / USECS_PER_MINUTE;
    time -= min * USECS_PER_MINUTE;
    let sec = time / USECS_PER_SEC;
    let usec = time - sec * USECS_PER_SEC;
    Tm { year, month, day, hour, min, sec, usec }
}

/// Reassemble calendar fields into a microsecond timestamp.
pub(crate) fn encode(tm: Tm) -> i64 {
    let date = date2j(tm.year, tm.month, tm.day) - POSTGRES_EPOCH_JDATE;
    date * USECS_PER_DAY
        + tm.hour * USECS_PER_HOUR
        + tm.min * USECS_PER_MINUTE
        + tm.sec * USECS_PER_SEC
        + tm.usec
}

// --- output (timestamp_out, ISO datestyle) ---------------------------------

/// `timestamp_out` at the default (ISO) DateStyle.
pub fn format(micros: i64) -> String {
    if micros == POS_INFINITY {
        return "infinity".to_string();
    }
    if micros == NEG_INFINITY {
        return "-infinity".to_string();
    }
    let (body, bc) = format_parts(micros);
    if bc { format!("{body} BC") } else { body }
}

/// The ISO date/time body of a finite timestamp (no ` BC` suffix) and whether
/// the year is BC. `timestamptz::format` uses this to splice its `+00` offset
/// before the ` BC` suffix, matching PG's `… 4714+00 BC` ordering. Callers must
/// not pass the ±infinity sentinels.
pub(crate) fn format_parts(micros: i64) -> (String, bool) {
    let tm = decode(micros);
    // Years <= 0 are BC: astronomical year 0 is 1 BC, -1 is 2 BC, ...
    let (year, bc) = if tm.year <= 0 {
        (1 - tm.year, true)
    } else {
        (tm.year, false)
    };
    let mut out = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        year, tm.month, tm.day, tm.hour, tm.min, tm.sec
    );
    if tm.usec != 0 {
        // Up to six fractional digits, trailing zeros trimmed.
        let frac = format!("{:06}", tm.usec);
        out.push('.');
        out.push_str(frac.trim_end_matches('0'));
    }
    (out, bc)
}

// --- input (timestamp_in, a practical subset) ------------------------------

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const MONTH_NAMES: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// Outcome of the shared input scan, used by both `timestamp` and
/// `timestamptz`. The final timestamp-range check is left to the caller
/// (`timestamp` bounds the civil value; `timestamptz` the post-offset UTC
/// value), and `timestamp` discards the zone token.
pub(crate) enum Parsed {
    /// A special value (`infinity`/`-infinity`/`epoch`), already in micros.
    Micros(i64),
    /// A calendar time with an astronomical year (BC already folded), plus the
    /// trailing zone token if one was present. Field ranges are validated by
    /// [`validate_fields`]; the year range is not checked here.
    Calendar { tm: Tm, zone: Option<String> },
}

/// The shared scan behind `timestamp::parse` and `timestamptz::parse`. Accepts
/// the ISO 8601 forms, the traditional `[Dow] Mon DD [HH:MM:SS[.f]] YYYY [zone]`
/// form, `YYYYMMDD` compact dates, and the `infinity`/`-infinity`/`epoch`
/// specials. A trailing time-zone token (a `±HH[:MM]` offset, an attached `Z`,
/// or a bare abbreviation / IANA name) is returned in `zone` rather than
/// discarded. Syntactically unparseable input is `22007`.
pub(crate) fn parse_parts(input: &str) -> Result<Parsed, TimestampError> {
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "infinity" | "+infinity" => return Ok(Parsed::Micros(POS_INFINITY)),
        "-infinity" => return Ok(Parsed::Micros(NEG_INFINITY)),
        "epoch" => return Ok(Parsed::Micros(EPOCH_MINUS_PG_DAYS * USECS_PER_DAY)),
        _ => {}
    }

    // Split into whitespace fields, further splitting an ISO date/time joined by
    // 'T' (e.g. "2001-09-22T18:19:20").
    let mut fields: Vec<String> = Vec::new();
    for raw in trimmed.split_whitespace() {
        if let Some((d, t)) = split_iso_t(raw) {
            fields.push(d.to_string());
            fields.push(t.to_string());
        } else {
            fields.push(raw.to_string());
        }
    }
    if fields.is_empty() {
        return Err(invalid_syntax(input));
    }

    let mut year: Option<i64> = None;
    let mut month: Option<i64> = None;
    let mut day: Option<i64> = None;
    let mut hour = 0i64;
    let mut min = 0i64;
    let mut sec = 0i64;
    let mut usec = 0i64;
    let mut have_time = false;
    let mut bc = false;
    let mut have_date = false;
    // The trailing time-zone token, if any (last one wins). `timestamp` ignores
    // it; `timestamptz` resolves it to a UTC offset.
    let mut zone: Option<String> = None;
    // Bare numeric fields (verbose form), as (value, digit count), resolved to
    // day/year after the scan so their order does not matter.
    let mut nums: Vec<(i64, usize)> = Vec::new();

    for field in &fields {
        let fl = field.to_ascii_lowercase();
        if let Some(m) = month_index(&fl) {
            if month.is_some() {
                return Err(invalid_syntax(input));
            }
            month = Some(m);
            continue;
        }
        if WEEKDAYS.contains(&fl.as_str()) {
            continue; // day-of-week name: decorative, ignored
        }
        if fl == "bc" {
            bc = true;
            continue;
        }
        if fl == "ad" {
            continue;
        }
        if field.contains(':') {
            // A `:`-bearing token that starts with a sign is a zone offset
            // (`-07:00`): the zone for `timestamptz`, decorative for `timestamp`.
            if field.starts_with(['+', '-']) {
                zone = Some(field.clone());
                continue;
            }
            if have_time {
                return Err(invalid_syntax(input));
            }
            let (h, mi, s, us, z) = parse_time(field).ok_or_else(|| invalid_syntax(input))?;
            hour = h;
            min = mi;
            sec = s;
            usec = us;
            have_time = true;
            // An attached zone (`18:19:20-07:00`, `...Z`) travels with the time.
            if z.is_some() {
                zone = z;
            }
            continue;
        }
        if let Some((y, m, d)) = parse_date_token(field) {
            if have_date {
                return Err(invalid_syntax(input));
            }
            year = Some(y);
            month = Some(m);
            day = Some(d);
            have_date = true;
            continue;
        }
        // A date with a glued zone and no time, e.g. `2001-02-16+00` or
        // `2001-02-16Z`. A date contains no `+`, and a trailing `Z` is
        // unambiguous, so these safely split into date + zone. (A glued
        // negative offset like `2001-02-16-08` is ambiguous with the date
        // separators; write it space-separated instead.)
        if let Some((date, z)) = split_date_zone(field)
            && let Some((y, m, d)) = parse_date_token(date)
        {
            if have_date {
                return Err(invalid_syntax(input));
            }
            year = Some(y);
            month = Some(m);
            day = Some(d);
            have_date = true;
            zone = Some(z);
            continue;
        }
        if field.bytes().all(|b| b.is_ascii_digit()) {
            let n: i64 = field.parse().map_err(|_| invalid_syntax(input))?;
            nums.push((n, field.len()));
            continue;
        }
        // Anything else is a bare time-zone token (an abbreviation like `PST` or
        // an IANA name like `America/New_York`): the zone for `timestamptz`,
        // decorative for `timestamp`.
        zone = Some(field.clone());
    }

    // Resolve the bare numbers into day/year. The year is the 4+-digit or >31
    // value; the remaining 1-2 digit value is the day. This is order-independent,
    // so "Feb 10 1997" and "10 Feb 1997" both parse. A bare number alongside a
    // full date token (which already fixed y/m/d) is invalid.
    for (n, len) in nums {
        if have_date {
            return Err(invalid_syntax(input));
        }
        if year.is_none() && (len >= 4 || n > 31) {
            year = Some(n);
        } else if day.is_none() {
            day = Some(n);
        } else if year.is_none() {
            year = Some(n);
        } else {
            return Err(invalid_syntax(input));
        }
    }

    // A resolved calendar date is required; this rejects pure garbage.
    let (Some(mut y), Some(m), Some(d)) = (year, month, day) else {
        return Err(invalid_syntax(input));
    };
    if bc {
        // "97 BC" is astronomical year -96 (1 BC == year 0).
        if y <= 0 {
            return Err(field_out_of_range(input));
        }
        y = 1 - y;
    }
    Ok(Parsed::Calendar {
        tm: Tm { year: y, month: m, day: d, hour, min, sec, usec },
        zone,
    })
}

/// Range-check the calendar fields (month/day/hour/min/sec) of a scanned time.
/// Shared by both types; the year range is checked separately by the caller.
pub(crate) fn validate_fields(tm: &Tm, input: &str) -> Result<(), TimestampError> {
    if !(1..=12).contains(&tm.month) || tm.day < 1 || tm.day > days_in_month(tm.year, tm.month) {
        return Err(field_out_of_range(input));
    }
    if tm.hour > 23 || tm.min > 59 || tm.sec > 59 {
        return Err(field_out_of_range(input));
    }
    Ok(())
}

/// `timestamp_in`. A trailing time zone is accepted and ignored (this type
/// carries no zone). Syntactically unparseable input is `22007`; a well-formed
/// value with an out-of-range field is `22008`.
pub fn parse(input: &str) -> Result<i64, TimestampError> {
    match parse_parts(input)? {
        Parsed::Micros(m) => Ok(m),
        Parsed::Calendar { tm, .. } => {
            // Bound the year to PG's timestamp range (4713 BC .. 294276 AD).
            // This both matches PG's out-of-range error and keeps `encode`
            // within i64. (Checked before the field ranges, as PG does.)
            if !(-4712..=294_276).contains(&tm.year) {
                return Err(field_out_of_range(input));
            }
            validate_fields(&tm, input)?;
            Ok(encode(tm))
        }
    }
}

/// Split "YYYY-MM-DD" from "HH:MM:SS" when joined by an ISO `T`.
fn split_iso_t(s: &str) -> Option<(&str, &str)> {
    let idx = s.find(['T', 't'])?;
    let (d, rest) = s.split_at(idx);
    let t = &rest[1..];
    if d.contains('-') && t.contains(':') {
        Some((d, t))
    } else {
        None
    }
}

/// Match a month name: the exact 3-letter abbreviation or the full English
/// name (case already folded). Only exact matches count — a token that merely
/// starts with a month abbreviation (e.g. "marble") is not a month. Compares by
/// value, never by byte-slicing, so non-ASCII input can't panic.
fn month_index(name: &str) -> Option<i64> {
    if let Some(i) = MONTHS.iter().position(|m| *m == name) {
        return Some(i as i64 + 1);
    }
    MONTH_NAMES.iter().position(|m| *m == name).map(|i| i as i64 + 1)
}

/// Parse a `HH:MM[:SS[.ffffff]]` time, optionally suffixed with `am`/`pm` and/or
/// a trailing zone. Returns `(hour, min, sec, usec, zone)`, where `zone` is the
/// attached zone token (`Z`, `-07:00`) if one was present — `timestamp` ignores
/// it, `timestamptz` resolves it.
fn parse_time(field: &str) -> Option<(i64, i64, i64, i64, Option<String>)> {
    // Strip a leading sign-less zone offset attached to the seconds, keeping the
    // digits/dot/colons; a bare abbreviation was already split off as its own
    // whitespace field, so here we only handle an am/pm suffix.
    let mut body = field;
    let mut ampm = 0i64; // 0 none, 1 am, 2 pm
    let lower = field.to_ascii_lowercase();
    if let Some(stripped) = lower.strip_suffix("pm") {
        ampm = 2;
        body = &field[..stripped.len()];
    } else if let Some(stripped) = lower.strip_suffix("am") {
        ampm = 1;
        body = &field[..stripped.len()];
    }

    // Strip an attached zone: the time is digits/colons/dot, so anything after
    // it (a trailing `Z`, or a `+`/`-` offset joined without a space, as in the
    // ISO 8601 form `18:19:20-07:00`) is the zone.
    let mut zone: Option<String> = None;
    if let Some(end) = body.find(|c: char| !(c.is_ascii_digit() || c == ':' || c == '.')) {
        zone = Some(body[end..].to_string());
        body = &body[..end];
    }

    let mut parts = body.split(':');
    let hour: i64 = parts.next()?.parse().ok()?;
    let min: i64 = parts.next()?.parse().ok()?;
    let (sec, usec) = match parts.next() {
        None => (0, 0),
        Some(secpart) => {
            let (whole, frac) = secpart.split_once('.').unwrap_or((secpart, ""));
            let sec: i64 = whole.parse().ok()?;
            (sec, parse_fraction(frac)?)
        }
    };
    if parts.next().is_some() {
        return None;
    }
    let hour = match ampm {
        1 => {
            if hour == 12 {
                0
            } else {
                hour
            }
        }
        2 => {
            if hour == 12 {
                12
            } else {
                hour + 12
            }
        }
        _ => hour,
    };
    Some((hour, min, sec, usec, zone))
}

/// Fractional-seconds string → microseconds, rounding a 7th+ digit half-up.
fn parse_fraction(frac: &str) -> Option<i64> {
    if frac.is_empty() {
        return Some(0);
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut micros = 0i64;
    for i in 0..6 {
        micros *= 10;
        if let Some(b) = frac.as_bytes().get(i) {
            micros += (b - b'0') as i64;
        }
    }
    // Round based on the 7th digit.
    if let Some(&b) = frac.as_bytes().get(6)
        && b >= b'5'
    {
        micros += 1;
    }
    Some(micros)
}

/// Split a date field with a glued zone but no time — `2001-02-16+00`
/// (offset starts at the unambiguous `+`) or `2001-02-16Z`/`...z` (trailing
/// `Z`) — into `(date, zone)`. Returns `None` when there is no such suffix.
///
/// A glued zone with no time can only be a numeric offset (`+HH[:MM[:SS]]`) or
/// `Z`; a named zone or abbreviation is always whitespace-separated. So the
/// `+` remainder must be an offset shape (digits and colons) — otherwise
/// `2001-02-16+garbage` would be wrongly accepted (PG rejects it as `22007`).
fn split_date_zone(field: &str) -> Option<(&str, String)> {
    if let Some(plus) = field.find('+') {
        let rest = &field[plus + 1..];
        if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit() || b == b':') {
            return Some((&field[..plus], field[plus..].to_string()));
        }
        return None;
    }
    if let Some(core) = field.strip_suffix(['Z', 'z']) {
        // Only when the remainder looks like a date, not e.g. a bare "z".
        if core.bytes().any(|b| b.is_ascii_digit()) {
            return Some((core, "Z".to_string()));
        }
    }
    None
}

/// Parse a date token: ISO `YYYY-MM-DD` (dash or slash separated). Returns
/// `(year, month, day)` without range-checking (the caller validates).
fn parse_date_token(field: &str) -> Option<(i64, i64, i64)> {
    // Compact YYYYMMDD.
    if field.len() == 8 && field.bytes().all(|b| b.is_ascii_digit()) {
        let y = field[..4].parse().ok()?;
        let m = field[4..6].parse().ok()?;
        let d = field[6..8].parse().ok()?;
        return Some((y, m, d));
    }
    let sep = if field.contains('-') {
        '-'
    } else if field.contains('/') {
        '/'
    } else {
        return None;
    };
    let parts: Vec<&str> = field.split(sep).collect();
    if parts.len() != 3 {
        return None;
    }
    // ISO order Y-M-D: the year is the first component (4 digits or clearly a
    // year); only this order is accepted (the tested/default DateStyle).
    let y = parts[0].parse().ok()?;
    let m = parts[1].parse().ok()?;
    let d = parts[2].parse().ok()?;
    Some((y, m, d))
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

// --- field extraction (date_part / extract) --------------------------------

/// ISO 8601 `(week, isoyear)` for a Julian day.
fn iso_week_year(jd: i64) -> (i64, i64) {
    let isodow = {
        let d = j2day(jd);
        if d == 0 { 7 } else { d }
    };
    // The Thursday of this ISO week determines the ISO year.
    let thursday = jd + (4 - isodow);
    let (isoyear, _, _) = j2date(thursday);
    let jan4 = date2j(isoyear, 1, 4);
    let jan4_isodow = {
        let d = j2day(jan4);
        if d == 0 { 7 } else { d }
    };
    let week1_thursday = jan4 + (4 - jan4_isodow);
    let week = (thursday - week1_thursday) / 7 + 1;
    (week, isoyear)
}

/// Fields that increase monotonically with the timestamp. On `±infinity` PG
/// returns `±Infinity` for these; every other (oscillating) known field returns
/// NULL (`None` here).
const MONOTONIC_UNITS: &[&str] = &[
    "year",
    "decade",
    "century",
    "millennium",
    "isoyear",
    "julian",
    "epoch",
];

/// The value of a known field on a `±infinity` timestamp: `±Infinity` for a
/// monotonic field, NULL (`None`) for an oscillating one. An unknown unit errors.
fn non_finite<T>(unit: &str, micros: i64, inf: T, neg_inf: T) -> Result<Option<T>, TimestampError> {
    if !KNOWN_UNITS.contains(&unit) {
        return Err(units_not_recognized(unit));
    }
    if MONOTONIC_UNITS.contains(&unit) {
        Ok(Some(if micros == POS_INFINITY { inf } else { neg_inf }))
    } else {
        Ok(None)
    }
}

/// `date_part` (float8). `Ok(None)` is SQL NULL (an oscillating field on
/// `±infinity`); an unrecognized unit errors.
pub fn date_part(unit: &str, micros: i64) -> Result<Option<f64>, TimestampError> {
    date_part_canon(&canonical_unit(unit), micros)
}

/// [`date_part`] with the unit already canonicalized (so `extract` can share it
/// without re-canonicalizing).
fn date_part_canon(unit: &str, micros: i64) -> Result<Option<f64>, TimestampError> {
    if !is_finite(micros) {
        return non_finite(unit, micros, f64::INFINITY, f64::NEG_INFINITY);
    }
    let tm = decode(micros);
    let jd = date2j(tm.year, tm.month, tm.day);
    let value = match unit {
        "microseconds" => (tm.sec * USECS_PER_SEC + tm.usec) as f64,
        "milliseconds" => (tm.sec * USECS_PER_SEC + tm.usec) as f64 / 1000.0,
        "second" => tm.sec as f64 + tm.usec as f64 / 1e6,
        "minute" => tm.min as f64,
        "hour" => tm.hour as f64,
        "day" => tm.day as f64,
        "month" => tm.month as f64,
        "quarter" => ((tm.month - 1) / 3 + 1) as f64,
        "year" => tm.year as f64,
        "decade" => decade(tm.year) as f64,
        "century" => century(tm.year) as f64,
        "millennium" => millennium(tm.year) as f64,
        "dow" => j2day(jd) as f64,
        "isodow" => {
            let d = j2day(jd);
            (if d == 0 { 7 } else { d }) as f64
        }
        "doy" => (jd - date2j(tm.year, 1, 1) + 1) as f64,
        "week" => iso_week_year(jd).0 as f64,
        "isoyear" => iso_week_year(jd).1 as f64,
        "julian" => {
            jd as f64
                + (tm.hour as f64 * 3600.0
                    + tm.min as f64 * 60.0
                    + tm.sec as f64
                    + tm.usec as f64 / 1e6)
                    / SECS_PER_DAY as f64
        }
        "epoch" => epoch_micros(micros) as f64 / 1e6,
        _ => return Err(units_not_recognized(unit)),
    };
    Ok(Some(value))
}

/// `extract` (numeric). Same fields as [`date_part`], but PG returns `numeric`
/// with a per-field scale: sub-second fields keep fractional digits. `Ok(None)`
/// is SQL NULL (an oscillating field on `±infinity`).
pub fn extract(unit: &str, micros: i64) -> Result<Option<NumericVal>, TimestampError> {
    let unit = canonical_unit(unit);
    if !is_finite(micros) {
        return non_finite(
            &unit,
            micros,
            NumericVal::Finite("Infinity".into()),
            NumericVal::Finite("-Infinity".into()),
        );
    }
    let tm = decode(micros);
    let total_sub_usec = tm.sec * USECS_PER_SEC + tm.usec;
    let s = match unit.as_str() {
        // Sub-second fields carry fractional digits (scale 6/3/0).
        "second" => fixed_point(total_sub_usec, 6),
        "milliseconds" => fixed_point(total_sub_usec, 3),
        "microseconds" => total_sub_usec.to_string(),
        "epoch" => fixed_point(epoch_micros(micros), 6),
        // PG returns a high-precision numeric here; we render the float8 Julian
        // date (fractional, but not byte-identical to PG's exotic numeric scale).
        "julian" => format!("{}", date_part_canon("julian", micros)?.expect("finite is Some")),
        // Everything else is an integer field: reuse date_part's value (already
        // canonical, so no re-canonicalization).
        _ => (date_part_canon(&unit, micros)?.expect("finite integer field is Some") as i64)
            .to_string(),
    };
    Ok(Some(NumericVal::Finite(s)))
}

/// Format `scaled` (the field value times `10^scale`) as a fixed-point decimal
/// with `scale` fractional digits, keeping sign. E.g. `40500000` at scale 6 is
/// `40.500000`; at scale 3 it is `40500.000`; at scale 0 it is `40500000`.
fn fixed_point(scaled: i64, scale: usize) -> String {
    let neg = scaled < 0;
    let abs = scaled.unsigned_abs();
    let denom = 10u64.pow(scale as u32);
    let int_part = abs / denom;
    let frac_part = abs % denom;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if scale == 0 {
        out.push_str(&int_part.to_string());
    } else {
        out.push_str(&format!("{}.{:0width$}", int_part, frac_part, width = scale));
    }
    out
}

/// Seconds-since-Unix-epoch expressed in microseconds. The PG epoch is
/// `EPOCH_MINUS_PG_DAYS` (= -10957) days from the Unix epoch, so shifting by the
/// negative of that lands on Unix time.
fn epoch_micros(micros: i64) -> i64 {
    micros - EPOCH_MINUS_PG_DAYS * USECS_PER_DAY
}

fn decade(year: i64) -> i64 {
    if year >= 0 {
        year / 10
    } else {
        -((8 - (year - 1)) / 10)
    }
}

fn century(year: i64) -> i64 {
    if year > 0 {
        (year + 99) / 100
    } else {
        -((99 - (year - 1)) / 100)
    }
}

fn millennium(year: i64) -> i64 {
    if year > 0 {
        (year + 999) / 1000
    } else {
        -((999 - (year - 1)) / 1000)
    }
}

/// Canonicalize a unit spelling: lowercase, trimmed, plural/alias-folded.
fn canonical_unit(unit: &str) -> String {
    let u = unit.trim().to_ascii_lowercase();
    match u.as_str() {
        "years" => "year",
        "months" => "month",
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
        "weeks" => "week",
        "dows" => "dow",
        _ => return u,
    }
    .to_string()
}

/// Units recognized for timestamps (used to distinguish an unknown unit from a
/// known unit applied to `±infinity`).
const KNOWN_UNITS: &[&str] = &[
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
    "dow",
    "isodow",
    "doy",
    "week",
    "isoyear",
    "julian",
    "epoch",
];

// --- date_trunc ------------------------------------------------------------

/// `date_trunc(unit, timestamp)`. `±infinity` truncates to itself. Errors on an
/// unrecognized unit. (BC years are not covered by the century/millennium/decade
/// rounding — no passing test needs them.)
pub fn date_trunc(unit: &str, micros: i64) -> Result<i64, TimestampError> {
    let unit = canonical_unit(unit);
    // Only the truncatable units are valid here; others error like PG (e.g.
    // `date_trunc('epoch', ...)` on a timestamp is not recognized).
    if !TRUNC_UNITS.contains(&unit.as_str()) {
        return Err(units_not_recognized(&unit));
    }
    if !is_finite(micros) {
        return Ok(micros);
    }
    if unit == "microseconds" {
        return Ok(micros);
    }
    let mut tm = decode(micros);
    match unit.as_str() {
        "milliseconds" => tm.usec = (tm.usec / 1000) * 1000,
        "second" => tm.usec = 0,
        "minute" => {
            tm.usec = 0;
            tm.sec = 0;
        }
        "hour" => {
            tm.usec = 0;
            tm.sec = 0;
            tm.min = 0;
        }
        "day" => zero_time(&mut tm),
        "week" => {
            let jd = date2j(tm.year, tm.month, tm.day);
            let isodow = {
                let d = j2day(jd);
                if d == 0 { 7 } else { d }
            };
            let monday = jd - (isodow - 1);
            let (y, m, d) = j2date(monday);
            tm.year = y;
            tm.month = m;
            tm.day = d;
            zero_time(&mut tm);
        }
        "month" => {
            tm.day = 1;
            zero_time(&mut tm);
        }
        "quarter" => {
            tm.month = (tm.month - 1) / 3 * 3 + 1;
            tm.day = 1;
            zero_time(&mut tm);
        }
        "year" => {
            tm.month = 1;
            tm.day = 1;
            zero_time(&mut tm);
        }
        "decade" => {
            tm.year -= tm.year.rem_euclid(10);
            tm.month = 1;
            tm.day = 1;
            zero_time(&mut tm);
        }
        "century" => {
            tm.year = if tm.year > 0 {
                (tm.year - 1) / 100 * 100 + 1
            } else {
                tm.year
            };
            tm.month = 1;
            tm.day = 1;
            zero_time(&mut tm);
        }
        "millennium" => {
            tm.year = if tm.year > 0 {
                (tm.year - 1) / 1000 * 1000 + 1
            } else {
                tm.year
            };
            tm.month = 1;
            tm.day = 1;
            zero_time(&mut tm);
        }
        _ => return Err(units_not_recognized(&unit)),
    }
    Ok(encode(tm))
}

const TRUNC_UNITS: &[&str] = &[
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
];

fn zero_time(tm: &mut Tm) {
    tm.hour = 0;
    tm.min = 0;
    tm.sec = 0;
    tm.usec = 0;
}

// --- make_timestamp --------------------------------------------------------

/// `make_timestamp(year, month, mday, hour, min, sec)`. Year 0 is invalid; a
/// negative year is BC. Out-of-range fields raise `22008`.
pub fn make_timestamp(
    year: i64,
    month: i64,
    mday: i64,
    hour: i64,
    min: i64,
    sec: f64,
) -> Result<i64, TimestampError> {
    let describe = || {
        format!(
            "date field value out of range: {}-{:02}-{:02}",
            year, month, mday
        )
    };
    // A negative (BC) year uses astronomical numbering internally: -1 == 1 BC.
    let astro_year = if year < 0 { year + 1 } else { year };
    if year == 0
        || !(-4712..=294_276).contains(&astro_year)
        || !(1..=12).contains(&month)
        || mday < 1
        || mday > days_in_month(astro_year, month)
    {
        return Err(TimestampError {
            sqlstate: DATETIME_FIELD_OVERFLOW,
            message: describe(),
        });
    }
    // PG's time_overflows: hour 0..24, min 0..59, sec 0..60, with 24:00:00
    // allowed only when min and sec are exactly zero (the end-of-day boundary
    // carries into the next day). sec == 60 is a valid leap-second carry.
    if !(0..=24).contains(&hour)
        || !(0..=59).contains(&min)
        || !(0.0..=60.0).contains(&sec)
        || (hour == 24 && (min != 0 || sec != 0.0))
    {
        return Err(TimestampError {
            sqlstate: DATETIME_FIELD_OVERFLOW,
            message: format!("time field value out of range: {hour}:{min:02}:{sec:09.6}"),
        });
    }
    let whole_sec = sec.trunc() as i64;
    let usec = (sec.fract() * 1e6).round() as i64;
    Ok(encode(Tm {
        year: astro_year,
        month,
        day: mday,
        hour,
        min,
        sec: whole_sec,
        usec,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> i64 {
        parse(s).unwrap()
    }

    #[test]
    fn roundtrip_iso() {
        for s in [
            "2001-02-16 20:38:40",
            "2001-02-16 20:38:40.5",
            "2001-02-16 20:38:40.999999",
            "2001-02-16 20:38:40.000001",
            "1997-01-02 00:00:00",
            "0001-01-01 00:00:00",
        ] {
            assert_eq!(format(ts(s)), s, "{s}");
        }
    }

    #[test]
    fn date_only_and_iso_t() {
        assert_eq!(format(ts("1997-01-02")), "1997-01-02 00:00:00");
        assert_eq!(format(ts("2001-09-22T18:19:20")), "2001-09-22 18:19:20");
    }

    #[test]
    fn verbose_form_and_zone_ignored() {
        assert_eq!(format(ts("Feb 10 17:32:01 1997")), "1997-02-10 17:32:01");
        assert_eq!(
            format(ts("1997-06-10 17:32:01 -07:00")),
            "1997-06-10 17:32:01"
        );
        assert_eq!(format(ts("Mon Feb 10 17:32:01 1997 PST")), "1997-02-10 17:32:01");
    }

    #[test]
    fn glued_date_zone() {
        // A date with a glued numeric offset / `Z` parses (the zone is ignored
        // by this zone-less type), matching PG.
        assert_eq!(format(ts("2001-02-16+00")), "2001-02-16 00:00:00");
        assert_eq!(format(ts("2001-02-16Z")), "2001-02-16 00:00:00");
        // But a bogus glued suffix is a syntax error, not a silently-ignored
        // zone (PG rejects `2001-02-16+garbage`).
        assert_eq!(parse("2001-02-16+garbage").unwrap_err().sqlstate, INVALID_DATETIME_FORMAT);
    }

    #[test]
    fn specials() {
        assert_eq!(format(ts("infinity")), "infinity");
        assert_eq!(format(ts("-infinity")), "-infinity");
        assert_eq!(format(ts("epoch")), "1970-01-01 00:00:00");
        assert_eq!(ts("infinity"), POS_INFINITY);
        assert_eq!(ts("-infinity"), NEG_INFINITY);
    }

    #[test]
    fn bc_years() {
        assert_eq!(format(ts("0097-02-16 BC")), "0097-02-16 00:00:00 BC");
    }

    #[test]
    fn frac_rounding() {
        // .6 stays .6; a 7th digit rounds the microseconds half-up.
        assert_eq!(format(ts("1997-02-10 17:32:01.6")), "1997-02-10 17:32:01.6");
        assert_eq!(
            format(ts("2001-01-01 00:00:00.0000005")),
            "2001-01-01 00:00:00.000001"
        );
    }

    #[test]
    fn syntax_and_range_errors() {
        assert_eq!(parse("garbage").unwrap_err().sqlstate, "22007");
        assert_eq!(parse("2001-13-01").unwrap_err().sqlstate, "22008");
        assert_eq!(parse("2001-02-30").unwrap_err().sqlstate, "22008");
    }

    #[test]
    fn date_part_fields() {
        let t = ts("2001-02-16 20:38:40.5");
        let dp = |u: &str| date_part(u, t).unwrap().unwrap();
        assert_eq!(dp("year"), 2001.0);
        assert_eq!(dp("month"), 2.0);
        assert_eq!(dp("day"), 16.0);
        assert_eq!(dp("hour"), 20.0);
        assert_eq!(dp("minute"), 38.0);
        assert_eq!(dp("second"), 40.5);
        assert_eq!(dp("dow"), 5.0);
        assert_eq!(dp("isodow"), 5.0);
        assert_eq!(dp("doy"), 47.0);
        assert_eq!(dp("quarter"), 1.0);
        assert_eq!(dp("week"), 7.0);
        assert_eq!(dp("decade"), 200.0);
        assert_eq!(dp("century"), 21.0);
        assert_eq!(dp("millennium"), 3.0);
        assert_eq!(dp("isoyear"), 2001.0);
        assert_eq!(dp("microseconds"), 40500000.0);
        assert_eq!(dp("milliseconds"), 40500.0);
        assert_eq!(date_part("epoch", ts("2001-02-16 20:38:40")).unwrap().unwrap(), 982355920.0);
    }

    #[test]
    fn date_part_unknown_unit() {
        assert_eq!(date_part("bogus", ts("2001-02-16")).unwrap_err().sqlstate, "22023");
    }

    #[test]
    fn extract_scales() {
        let t = ts("2001-02-16 20:38:40.5");
        let ex = |u: &str| extract(u, t).unwrap().unwrap();
        assert_eq!(ex("year"), NumericVal::Finite("2001".into()));
        assert_eq!(ex("second"), NumericVal::Finite("40.500000".into()));
        assert_eq!(ex("milliseconds"), NumericVal::Finite("40500.000".into()));
        assert_eq!(ex("microseconds"), NumericVal::Finite("40500000".into()));
        assert_eq!(
            extract("epoch", ts("2001-02-16 20:38:40")).unwrap().unwrap(),
            NumericVal::Finite("982355920.000000".into())
        );
        assert_eq!(ex("epoch"), NumericVal::Finite("982355920.500000".into()));
    }

    #[test]
    fn non_finite_fields_are_infinity_or_null() {
        // Monotonic fields on ±infinity are ±Infinity; oscillating fields NULL.
        assert_eq!(date_part("year", POS_INFINITY).unwrap(), Some(f64::INFINITY));
        assert_eq!(date_part("epoch", NEG_INFINITY).unwrap(), Some(f64::NEG_INFINITY));
        assert_eq!(date_part("month", POS_INFINITY).unwrap(), None);
        assert_eq!(date_part("week", NEG_INFINITY).unwrap(), None);
        assert_eq!(
            extract("year", POS_INFINITY).unwrap(),
            Some(NumericVal::Finite("Infinity".into()))
        );
        assert_eq!(extract("day", POS_INFINITY).unwrap(), None);
        // An unknown unit still errors even on ±infinity.
        assert_eq!(date_part("bogus", POS_INFINITY).unwrap_err().sqlstate, "22023");
    }

    #[test]
    fn extract_julian_keeps_the_fraction() {
        // The fractional day must survive (regression: was truncated to i64).
        let s = match extract("julian", ts("2001-02-16 20:38:40")).unwrap().unwrap() {
            NumericVal::Finite(s) => s,
            other => panic!("expected finite, got {other:?}"),
        };
        assert!(s.starts_with("2451957.86"), "got {s}");
    }

    #[test]
    fn parser_edge_cases() {
        // Day-before-month verbose form.
        assert_eq!(format(ts("10 Feb 1997")), "1997-02-10 00:00:00");
        // ISO 'T' time with an attached zone (Z / numeric offset) is accepted.
        assert_eq!(format(ts("2001-09-22T18:19:20Z")), "2001-09-22 18:19:20");
        assert_eq!(format(ts("2001-09-22T18:19:20-07:00")), "2001-09-22 18:19:20");
        // A full English month name works; a word merely prefixed by one does not.
        assert_eq!(format(ts("February 10 1997")), "1997-02-10 00:00:00");
        assert_eq!(parse("marble 5 2001").unwrap_err().sqlstate, "22007");
        // Non-ASCII input must error, not panic (regression for &name[..3]).
        assert_eq!(parse("aa\u{e9} 2001").unwrap_err().sqlstate, "22007");
        // Out-of-range years error instead of overflowing i64.
        assert_eq!(parse("5000000000-01-01").unwrap_err().sqlstate, "22008");
    }

    #[test]
    fn make_timestamp_boundary_times() {
        // 24:00:00 rolls into the next day; sec == 60 carries a minute.
        assert_eq!(
            format(make_timestamp(2013, 7, 15, 24, 0, 0.0).unwrap()),
            "2013-07-16 00:00:00"
        );
        assert_eq!(
            format(make_timestamp(2013, 7, 15, 8, 15, 60.0).unwrap()),
            "2013-07-15 08:16:00"
        );
        // But 24:00 with a nonzero minute, or sec > 60, is still rejected.
        assert!(make_timestamp(2013, 7, 15, 24, 1, 0.0).is_err());
        assert!(make_timestamp(2013, 7, 15, 8, 15, 60.5).is_err());
    }

    #[test]
    fn date_trunc_fields() {
        let t = ts("2001-02-16 20:38:40.5");
        assert_eq!(format(date_trunc("hour", t).unwrap()), "2001-02-16 20:00:00");
        assert_eq!(format(date_trunc("day", t).unwrap()), "2001-02-16 00:00:00");
        assert_eq!(format(date_trunc("month", t).unwrap()), "2001-02-01 00:00:00");
        assert_eq!(format(date_trunc("year", t).unwrap()), "2001-01-01 00:00:00");
        assert_eq!(format(date_trunc("week", t).unwrap()), "2001-02-12 00:00:00");
        assert_eq!(format(date_trunc("quarter", t).unwrap()), "2001-01-01 00:00:00");
        assert_eq!(format(date_trunc("decade", t).unwrap()), "2000-01-01 00:00:00");
        assert_eq!(format(date_trunc("century", t).unwrap()), "2001-01-01 00:00:00");
        assert_eq!(format(date_trunc("millennium", t).unwrap()), "2001-01-01 00:00:00");
        assert_eq!(
            format(date_trunc("milliseconds", ts("2001-02-16 20:38:40.123456")).unwrap()),
            "2001-02-16 20:38:40.123"
        );
        assert_eq!(date_trunc("day", POS_INFINITY).unwrap(), POS_INFINITY);
    }

    #[test]
    fn make_timestamp_ok_and_range() {
        assert_eq!(
            format(make_timestamp(2013, 7, 15, 8, 15, 23.5).unwrap()),
            "2013-07-15 08:15:23.5"
        );
        assert_eq!(
            format(make_timestamp(2013, 7, 15, 8, 15, 23.0).unwrap()),
            "2013-07-15 08:15:23"
        );
        assert!(make_timestamp(2013, 13, 15, 8, 15, 23.0).is_err());
    }
}
