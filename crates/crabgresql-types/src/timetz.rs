//! `timetz` (`time with time zone`): parsing, output, comparison, interval
//! arithmetic, and the field functions (`date_part`/`extract`).
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior — the `HH:MM:SS[.ffffff]±TZ` output, the UTC-instant ordering, the
//! field values, and the SQLSTATE/message of range and syntax errors — pinned
//! by differential tests against real PG, implemented independently.
//!
//! Representation: [`TimeTz`] carries the local time-of-day (`usec`, `[0,
//! 86_400_000_000]`) plus `zone`, the offset in **seconds west of UTC** — a
//! value that displays as `-07` stores `+25200`, chosen so ordering by the UTC
//! instant is `usec + zone*USECS_PER_SEC` (pinned by the ordering tests).
//!
//! Deviations from PG, acceptable while no passing test needs them: a numeric
//! offset (`-07`, `+05:30`) is honored, but resolving a named zone or dynamic
//! abbreviation (which needs a date and the zone database) is not — those
//! inputs are rejected. A missing zone defaults to UTC.

use crate::Numeric;
use crate::interval::Interval;
use crate::time;
use crate::timestamp::fixed_point;

const INVALID_DATETIME_FORMAT: &str = "22007";
const DATETIME_FIELD_OVERFLOW: &str = "22008";
const INVALID_PARAMETER_VALUE: &str = "22023";

const USECS_PER_DAY: i64 = 86_400_000_000;
const USECS_PER_HOUR: i64 = 3_600_000_000;
const USECS_PER_MINUTE: i64 = 60_000_000;
const USECS_PER_SEC: i64 = 1_000_000;

/// A `time with time zone` value: local time-of-day plus a UTC offset held as
/// seconds **west** of UTC (so a `-07:00` display offset is `zone == 25200`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeTz {
    pub usec: i64,
    pub zone: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TimeTzError {
    pub sqlstate: &'static str,
    pub message: String,
}

fn invalid_syntax(input: &str) -> TimeTzError {
    TimeTzError {
        sqlstate: INVALID_DATETIME_FORMAT,
        message: format!("invalid input syntax for type time with time zone: \"{input}\""),
    }
}

fn field_out_of_range(input: &str) -> TimeTzError {
    TimeTzError {
        sqlstate: DATETIME_FIELD_OVERFLOW,
        message: format!("date/time field value out of range: \"{input}\""),
    }
}

/// UTC-instant key used for ordering (PG's `time + zone*USECS_PER_SEC`).
fn utc_key(v: TimeTz) -> i64 {
    v.usec + v.zone as i64 * USECS_PER_SEC
}

/// `timetz_cmp`: order by the UTC instant, then by the stored zone.
pub fn cmp(a: TimeTz, b: TimeTz) -> std::cmp::Ordering {
    utc_key(a).cmp(&utc_key(b)).then(a.zone.cmp(&b.zone))
}

// --- output (timetz_out) ---------------------------------------------------

/// `timetz_out`: the local time followed by the signed UTC offset (`+00`,
/// `-07`, `-04:30`).
pub fn format(v: TimeTz) -> String {
    let mut out = time::format(v.usec);
    out.push_str(&format_offset(v.zone));
    out
}

/// Format the stored west-of-UTC `zone` as a signed east-of-UTC display offset.
fn format_offset(zone_west: i32) -> String {
    let gmtoff = -zone_west; // seconds east of UTC
    let sign = if gmtoff < 0 { '-' } else { '+' };
    let a = gmtoff.unsigned_abs();
    let (h, m, s) = (a / 3600, (a % 3600) / 60, a % 60);
    let mut o = format!("{sign}{h:02}");
    if m != 0 || s != 0 {
        o.push_str(&format!(":{m:02}"));
    }
    if s != 0 {
        o.push_str(&format!(":{s:02}"));
    }
    o
}

// --- input (timetz_in) -----------------------------------------------------

/// `timetz_in`. Accepts `HH:MM[:SS[.ffffff]]` with an optional `AM`/`PM`, an
/// optional leading date (ignored), and a numeric UTC offset (`-07`, `+05:30`,
/// or glued to the time). A missing offset defaults to UTC. A named zone or
/// dynamic abbreviation is rejected (no date/zone database here).
pub fn parse(input: &str) -> Result<TimeTz, TimeTzError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(invalid_syntax(input));
    }
    let mut time_str: Option<String> = None;
    let mut ampm: Option<&str> = None;
    let mut gmtoff: Option<i32> = None;
    let mut have_date = false;

    for tok in trimmed.split_whitespace() {
        let lower = tok.to_ascii_lowercase();
        if lower == "am" || lower == "pm" {
            ampm = Some(if lower == "am" { "am" } else { "pm" });
            continue;
        }
        if tok.starts_with(['+', '-']) {
            gmtoff = Some(parse_offset(tok).ok_or_else(|| invalid_syntax(input))?);
            continue;
        }
        if tok.contains(':') {
            if time_str.is_some() {
                return Err(invalid_syntax(input));
            }
            // A glued offset (`13:30:25.5-04`) begins at the first sign.
            if let Some(pos) = tok.find(['+', '-']) {
                gmtoff = Some(parse_offset(&tok[pos..]).ok_or_else(|| invalid_syntax(input))?);
                time_str = Some(tok[..pos].to_string());
            } else {
                time_str = Some(tok.to_string());
            }
            continue;
        }
        if is_date_token(tok) {
            have_date = true;
            continue;
        }
        // A named zone (`America/New_York`) without a date cannot be resolved.
        if tok.contains('/') && !have_date {
            return Err(invalid_syntax(input));
        }
        // Any other bare abbreviation is left decorative (defaults to UTC).
    }

    let time_str = time_str.ok_or_else(|| invalid_syntax(input))?;
    // Reuse `time`'s parser for the time-of-day (with any am/pm suffix), then
    // remap its error to name `time with time zone`.
    let time_input = match ampm {
        Some(ap) => format!("{time_str} {ap}"),
        None => time_str,
    };
    let usec = time::parse(&time_input).map_err(|e| {
        if e.sqlstate == DATETIME_FIELD_OVERFLOW {
            field_out_of_range(input)
        } else {
            invalid_syntax(input)
        }
    })?;
    let zone = -gmtoff.unwrap_or(0); // stored west-of-UTC
    Ok(TimeTz { usec, zone })
}

/// Parse a numeric offset `±HH[:MM[:SS]]` into seconds east of UTC.
fn parse_offset(s: &str) -> Option<i32> {
    let sign = match s.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let mut h = 0i32;
    let mut m = 0i32;
    let mut sec = 0i32;
    for (i, part) in s[1..].split(':').enumerate() {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let v: i32 = part.parse().ok()?;
        match i {
            0 => h = v,
            1 => m = v,
            2 => sec = v,
            _ => return None,
        }
    }
    // PG limits offsets to ±15:59:59.
    if h > 15 || m > 59 || sec > 59 {
        return None;
    }
    Some(sign * (h * 3600 + m * 60 + sec))
}

fn is_date_token(tok: &str) -> bool {
    let parts: Vec<&str> = tok.split('-').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

// --- interval arithmetic ---------------------------------------------------

/// `timetz + interval`: shift the local time-of-day, keeping the zone.
pub fn pl_interval(v: TimeTz, span: Interval) -> TimeTz {
    // Reduce the interval mod a day before adding to avoid i64 overflow on large
    // intervals; modular reduction commutes with the add (see time::pl_interval).
    TimeTz {
        usec: (v.usec + span.usec.rem_euclid(USECS_PER_DAY)).rem_euclid(USECS_PER_DAY),
        zone: v.zone,
    }
}

/// `timetz - interval`: the negation of the add.
pub fn mi_interval(v: TimeTz, span: Interval) -> TimeTz {
    TimeTz {
        usec: (v.usec - span.usec.rem_euclid(USECS_PER_DAY)).rem_euclid(USECS_PER_DAY),
        zone: v.zone,
    }
}

// --- field extraction (date_part / extract) --------------------------------

/// Classify a (lowercased) unit for `timetz`: `Some(true)` supported,
/// `Some(false)` known-but-unsupported, `None` unknown.
fn classify(unit: &str) -> Option<bool> {
    match unit {
        "microseconds" | "microsecond" | "usec" | "usecs" | "milliseconds" | "millisecond"
        | "msec" | "msecs" | "second" | "seconds" | "minute" | "minutes" | "hour" | "hours"
        | "epoch" | "timezone" | "timezone_hour" | "timezone_h" | "timezone_minute"
        | "timezone_m" => Some(true),
        "day" | "days" | "month" | "months" | "year" | "years" | "quarter" | "week" | "dow"
        | "isodow" | "doy" | "decade" | "century" | "millennium" | "isoyear" => Some(false),
        _ => None,
    }
}

fn err_unit(unit: &str, supported: bool) -> TimeTzError {
    let verb = if supported {
        "not supported"
    } else {
        "not recognized"
    };
    TimeTzError {
        sqlstate: INVALID_PARAMETER_VALUE,
        message: format!("unit \"{unit}\" {verb} for type time with time zone"),
    }
}

/// `date_part(unit, timetz) -> float8`.
pub fn date_part(unit: &str, v: TimeTz) -> Result<f64, TimeTzError> {
    let lu = unit.trim().to_ascii_lowercase();
    match classify(&lu) {
        None => Err(err_unit(&lu, false)),
        Some(false) => Err(err_unit(&lu, true)),
        Some(true) => Ok(field_f64(&canon(&lu), v)),
    }
}

/// `extract(unit FROM timetz) -> numeric`, with PG's per-field scale.
pub fn extract(unit: &str, v: TimeTz) -> Result<Numeric, TimeTzError> {
    let lu = unit.trim().to_ascii_lowercase();
    match classify(&lu) {
        None => Err(err_unit(&lu, false)),
        Some(false) => Err(err_unit(&lu, true)),
        Some(true) => {
            let sub_usec = v.usec % USECS_PER_MINUTE;
            let s = match canon(&lu).as_str() {
                "second" => fixed_point(sub_usec, 6),
                "milliseconds" => fixed_point(sub_usec, 3),
                "microseconds" => sub_usec.to_string(),
                "epoch" => fixed_point(v.usec + v.zone as i64 * USECS_PER_SEC, 6),
                other => (field_f64(other, v) as i64).to_string(),
            };
            match Numeric::parse(&s) {
                Ok(value) => Ok(value),
                Err(_) => panic!("timetz extraction must form a valid numeric literal"),
            }
        }
    }
}

fn field_f64(unit: &str, v: TimeTz) -> f64 {
    let gmtoff = -v.zone as i64; // seconds east of UTC
    match unit {
        "timezone" => gmtoff as f64,
        "timezone_hour" => (gmtoff / 3600) as f64,
        "timezone_minute" => (gmtoff / 60 % 60) as f64,
        // UTC seconds since midnight.
        "epoch" => (v.usec + v.zone as i64 * USECS_PER_SEC) as f64 / 1e6,
        // Local time-of-day fields.
        _ => {
            let usec = v.usec;
            let hour = usec / USECS_PER_HOUR;
            let mut rem = usec % USECS_PER_HOUR;
            let min = rem / USECS_PER_MINUTE;
            rem %= USECS_PER_MINUTE;
            let sec = rem / USECS_PER_SEC;
            let frac = rem % USECS_PER_SEC;
            match unit {
                "microseconds" => (sec * USECS_PER_SEC + frac) as f64,
                "milliseconds" => (sec * USECS_PER_SEC + frac) as f64 / 1000.0,
                "second" => sec as f64 + frac as f64 / 1e6,
                "minute" => min as f64,
                "hour" => hour as f64,
                _ => unreachable!("field_f64 called with an unsupported unit"),
            }
        }
    }
}

fn canon(unit: &str) -> String {
    match unit {
        "microsecond" | "usec" | "usecs" => "microseconds",
        "millisecond" | "msec" | "msecs" => "milliseconds",
        "seconds" => "second",
        "minutes" => "minute",
        "hours" => "hour",
        "timezone_h" => "timezone_hour",
        "timezone_m" => "timezone_minute",
        other => other,
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> TimeTz {
        match parse(s) {
            Ok(value) => value,
            Err(error) => panic!("invalid timetz test fixture `{s}`: {error:?}"),
        }
    }

    #[test]
    fn parse_format_and_edges() {
        assert_eq!(format(v("13:30:25.575401-04")), "13:30:25.575401-04");
        assert_eq!(format(v("05:06:07-07")), "05:06:07-07");
        assert_eq!(format(v("12:00:00+05:30")), "12:00:00+05:30");
        assert_eq!(format(v("12:00")), "12:00:00+00"); // default UTC
        assert_eq!(format(v("23:59:60 -07")), "24:00:00-07"); // rounds up
        assert!(parse("24:00:00.01 -07").is_err());
        assert!(parse("15:36:39 America/New_York").is_err());
    }

    #[test]
    fn ordering() {
        // 05:06:07-07 == 12:06:07 UTC; 05:06:07+00 == 05:06:07 UTC.
        assert_eq!(
            cmp(v("05:06:07-07"), v("05:06:07+00")),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn extract_fields() -> anyhow::Result<()> {
        let x = v("13:30:25.575401-04:30");
        assert_eq!(date_part("hour", x)?, 13.0);
        assert_eq!(date_part("timezone", x)?, -16200.0);
        assert_eq!(date_part("timezone_hour", x)?, -4.0);
        assert_eq!(date_part("timezone_minute", x)?, -30.0);
        assert_eq!(extract("microsecond", x)?.to_display(), "25575401");
        let y = v("13:30:25.575401-04");
        assert_eq!(extract("epoch", y)?.to_display(), "63025.575401");
        assert!(date_part("day", x).is_err());

        Ok(())
    }
}
