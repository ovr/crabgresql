//! `timestamp` (without time zone): parsing, output, and the field functions
//! (`date_part`/`extract`/`date_trunc`/`isfinite`/`make_timestamp`).
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior — the ISO output format, the field values, and the SQLSTATE/message
//! of range and syntax errors — pinned by differential tests against real PG,
//! implemented independently. The Julian-day conversions use the standard
//! published calendar algorithm (Fliegel & Van Flandern, "Explanatory
//! Supplement to the Astronomical Almanac"), not PG's source.
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
const INVALID_DATETIME_FORMAT: &str = "22007";
const DATETIME_FIELD_OVERFLOW: &str = "22008";
const INVALID_PARAMETER_VALUE: &str = "22023";

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

// --- Julian-day calendar conversions (standard almanac algorithm) ----------

/// Gregorian (proleptic) `(year, month, day)` → Julian day number. `year` is
/// the astronomical year (1 BC is year 0, 2 BC is -1, ...).
fn date2j(y: i64, m: i64, d: i64) -> i64 {
    let (y, m) = if m > 2 { (y + 4800, m + 1) } else { (y + 4799, m + 13) };
    let century = y / 100;
    let mut julian = y * 365 - 32167;
    julian += y / 4 - century + century / 4;
    julian + 7834 * m / 256 + d
}

/// Inverse of [`date2j`]: Julian day number → `(year, month, day)`.
fn j2date(jd: i64) -> (i64, i64, i64) {
    let mut julian = jd + 32044;
    let quad = julian / 146_097;
    let extra = (julian - quad * 146_097) * 4 + 3;
    julian += 60 + quad * 3 + extra / 146_097;
    let quad = julian / 1461;
    julian -= quad * 1461;
    let mut y = julian * 4 / 1461;
    julian = if y != 0 {
        (julian + 305) % 365
    } else {
        (julian + 306) % 366
    } + 123;
    y += quad * 4;
    let year = y - 4800;
    let quad = julian * 2141 / 65_536;
    let day = julian - 7834 * quad / 256;
    let month = (quad + 10) % 12 + 1;
    (year, month, day)
}

/// Day of week, 0 = Sunday .. 6 = Saturday (PG's `j2day`).
fn j2day(jd: i64) -> i64 {
    (jd + 1).rem_euclid(7)
}

/// Broken-down finite timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Tm {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    min: i64,
    sec: i64,
    usec: i64,
}

/// Split a finite microsecond timestamp into calendar fields.
fn decode(micros: i64) -> Tm {
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
fn encode(tm: Tm) -> i64 {
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
    if bc {
        out.push_str(" BC");
    }
    out
}

// --- input (timestamp_in, a practical subset) ------------------------------

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// `timestamp_in`. Accepts the ISO 8601 forms, the traditional
/// `[Dow] Mon DD [HH:MM:SS[.f]] YYYY [zone]` form, `YYYYMMDD` compact dates, and
/// the `infinity`/`-infinity`/`epoch` specials. A trailing time-zone token is
/// accepted and ignored (this type carries no zone). Syntactically unparseable
/// input is `22007`; a well-formed value with an out-of-range field is `22008`.
pub fn parse(input: &str) -> Result<i64, TimestampError> {
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "infinity" | "+infinity" => return Ok(POS_INFINITY),
        "-infinity" => return Ok(NEG_INFINITY),
        "epoch" => return Ok(EPOCH_MINUS_PG_DAYS * USECS_PER_DAY),
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
            // (`-07:00`), decorative for this zone-less type — ignore it.
            if field.starts_with(['+', '-']) {
                continue;
            }
            if have_time {
                return Err(invalid_syntax(input));
            }
            let (h, mi, s, us) = parse_time(field).ok_or_else(|| invalid_syntax(input))?;
            hour = h;
            min = mi;
            sec = s;
            usec = us;
            have_time = true;
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
        if field.bytes().all(|b| b.is_ascii_digit()) {
            let n: i64 = field.parse().map_err(|_| invalid_syntax(input))?;
            // In the verbose form the day precedes the year: "Feb 10 ... 1997".
            if month.is_some() && day.is_none() && year.is_none() {
                day = Some(n);
            } else if year.is_none() {
                year = Some(n);
            } else if day.is_none() {
                day = Some(n);
            } else {
                return Err(invalid_syntax(input));
            }
            continue;
        }
        // Anything else (a bare time-zone abbreviation or numeric offset) is
        // decorative for this zone-less type and ignored.
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
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return Err(field_out_of_range(input));
    }
    if hour > 23 || min > 59 || sec > 59 {
        return Err(field_out_of_range(input));
    }
    Ok(encode(Tm { year: y, month: m, day: d, hour, min, sec, usec }))
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

fn month_index(name: &str) -> Option<i64> {
    let short = if name.len() > 3 { &name[..3] } else { name };
    MONTHS.iter().position(|m| *m == short).map(|i| i as i64 + 1)
}

/// Parse a `HH:MM[:SS[.ffffff]]` time, optionally suffixed with `am`/`pm` and/or
/// a trailing zone that is ignored. Returns `(hour, min, sec, usec)`.
fn parse_time(field: &str) -> Option<(i64, i64, i64, i64)> {
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
    Some((hour, min, sec, usec))
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

/// `date_part` (float8). Errors on an unrecognized unit; on `±infinity` only
/// `epoch` is defined (`±Infinity`), other units raise the field-overflow error.
pub fn date_part(unit: &str, micros: i64) -> Result<f64, TimestampError> {
    let unit = canonical_unit(unit);
    if !is_finite(micros) {
        return match unit.as_str() {
            "epoch" => Ok(if micros == POS_INFINITY {
                f64::INFINITY
            } else {
                f64::NEG_INFINITY
            }),
            _ if KNOWN_UNITS.contains(&unit.as_str()) => Err(TimestampError {
                sqlstate: DATETIME_FIELD_OVERFLOW,
                message: format!(
                    "cannot extract {} from a non-finite timestamp",
                    &unit
                ),
            }),
            _ => Err(units_not_recognized(&unit)),
        };
    }
    let tm = decode(micros);
    let jd = date2j(tm.year, tm.month, tm.day);
    let value = match unit.as_str() {
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
        "julian" => jd as f64 + (tm.hour as f64 * 3600.0 + tm.min as f64 * 60.0 + tm.sec as f64 + tm.usec as f64 / 1e6) / SECS_PER_DAY as f64,
        "epoch" => epoch_micros(micros) as f64 / 1e6,
        _ => return Err(units_not_recognized(&unit)),
    };
    Ok(value)
}

/// `extract` (numeric). Same fields as [`date_part`], but PG returns `numeric`
/// with a per-field scale: sub-second fields keep fractional digits.
pub fn extract(unit: &str, micros: i64) -> Result<NumericVal, TimestampError> {
    let unit = canonical_unit(unit);
    if !is_finite(micros) {
        return match unit.as_str() {
            "epoch" => Ok(NumericVal::Finite(
                if micros == POS_INFINITY { "Infinity" } else { "-Infinity" }.to_string(),
            )),
            _ if KNOWN_UNITS.contains(&unit.as_str()) => Err(TimestampError {
                sqlstate: DATETIME_FIELD_OVERFLOW,
                message: format!("cannot extract {} from a non-finite timestamp", &unit),
            }),
            _ => Err(units_not_recognized(&unit)),
        };
    }
    let tm = decode(micros);
    let total_sub_usec = tm.sec * USECS_PER_SEC + tm.usec;
    let s = match unit.as_str() {
        // Sub-second fields carry fractional digits (scale 6/3/0).
        "second" => fixed_point(total_sub_usec, 6),
        "milliseconds" => fixed_point(total_sub_usec, 3),
        "microseconds" => total_sub_usec.to_string(),
        "epoch" => fixed_point(epoch_micros(micros), 6),
        // Everything else is an integer field: reuse date_part's integer value.
        _ => (date_part(&unit, micros)? as i64).to_string(),
    };
    Ok(NumericVal::Finite(s))
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
    if year == 0
        || !(1..=12).contains(&month)
        || mday < 1
        || mday > days_in_month(if year < 0 { year + 1 } else { year }, month)
    {
        return Err(TimestampError {
            sqlstate: DATETIME_FIELD_OVERFLOW,
            message: describe(),
        });
    }
    if !(0..=23).contains(&hour) || !(0..=59).contains(&min) || !(0.0..60.0).contains(&sec) {
        return Err(TimestampError {
            sqlstate: DATETIME_FIELD_OVERFLOW,
            message: format!("time field value out of range: {hour}:{min:02}:{sec:09.6}"),
        });
    }
    // A negative (BC) year uses astronomical numbering internally: -1 == 1 BC.
    let astro_year = if year < 0 { year + 1 } else { year };
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
        assert_eq!(date_part("year", t).unwrap(), 2001.0);
        assert_eq!(date_part("month", t).unwrap(), 2.0);
        assert_eq!(date_part("day", t).unwrap(), 16.0);
        assert_eq!(date_part("hour", t).unwrap(), 20.0);
        assert_eq!(date_part("minute", t).unwrap(), 38.0);
        assert_eq!(date_part("second", t).unwrap(), 40.5);
        assert_eq!(date_part("dow", t).unwrap(), 5.0);
        assert_eq!(date_part("isodow", t).unwrap(), 5.0);
        assert_eq!(date_part("doy", t).unwrap(), 47.0);
        assert_eq!(date_part("quarter", t).unwrap(), 1.0);
        assert_eq!(date_part("week", t).unwrap(), 7.0);
        assert_eq!(date_part("decade", t).unwrap(), 200.0);
        assert_eq!(date_part("century", t).unwrap(), 21.0);
        assert_eq!(date_part("millennium", t).unwrap(), 3.0);
        assert_eq!(date_part("isoyear", t).unwrap(), 2001.0);
        assert_eq!(date_part("microseconds", t).unwrap(), 40500000.0);
        assert_eq!(date_part("milliseconds", t).unwrap(), 40500.0);
        assert_eq!(
            date_part("epoch", ts("2001-02-16 20:38:40")).unwrap(),
            982355920.0
        );
    }

    #[test]
    fn date_part_unknown_unit() {
        assert_eq!(date_part("bogus", ts("2001-02-16")).unwrap_err().sqlstate, "22023");
    }

    #[test]
    fn extract_scales() {
        let t = ts("2001-02-16 20:38:40.5");
        assert_eq!(extract("year", t).unwrap(), NumericVal::Finite("2001".into()));
        assert_eq!(extract("second", t).unwrap(), NumericVal::Finite("40.500000".into()));
        assert_eq!(
            extract("milliseconds", t).unwrap(),
            NumericVal::Finite("40500.000".into())
        );
        assert_eq!(
            extract("microseconds", t).unwrap(),
            NumericVal::Finite("40500000".into())
        );
        assert_eq!(
            extract("epoch", ts("2001-02-16 20:38:40")).unwrap(),
            NumericVal::Finite("982355920.000000".into())
        );
        assert_eq!(
            extract("epoch", t).unwrap(),
            NumericVal::Finite("982355920.500000".into())
        );
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
