//! `time` (without time zone): parsing, output, comparison, interval
//! arithmetic, and the field functions (`date_part`/`extract`/`make_time`).
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior — the `HH:MM:SS[.ffffff]` output, the field values, the
//! `24:00:00` boundary, and the SQLSTATE/message of range and syntax errors —
//! pinned by differential tests against real PG, implemented independently.
//!
//! Representation: microseconds since midnight, held in an `i64` in the closed
//! range `[0, 86_400_000_000]` (the upper bound is `24:00:00`, which PG allows).
//! There are no infinity values.
//!
//! Deviations from PG, acceptable while no passing test needs them: a `time`
//! precision modifier (`time(2)`) is accepted and ignored (full microsecond
//! resolution is kept); a trailing numeric offset or fixed abbreviation is
//! accepted and ignored, but a bare IANA zone name without a date is rejected
//! (as PG does).

use crate::interval::Interval;
use crate::timestamp::{self, fixed_point};
use crate::Numeric;

const INVALID_DATETIME_FORMAT: &str = "22007";
const DATETIME_FIELD_OVERFLOW: &str = "22008";
const INVALID_PARAMETER_VALUE: &str = "22023";

pub const USECS_PER_DAY: i64 = 86_400_000_000;
const USECS_PER_HOUR: i64 = 3_600_000_000;
const USECS_PER_MINUTE: i64 = 60_000_000;
const USECS_PER_SEC: i64 = 1_000_000;

#[derive(Clone, Debug, PartialEq)]
pub struct TimeError {
    pub sqlstate: &'static str,
    pub message: String,
}

fn invalid_syntax(input: &str) -> TimeError {
    TimeError {
        sqlstate: INVALID_DATETIME_FORMAT,
        message: format!("invalid input syntax for type time: \"{input}\""),
    }
}

fn field_out_of_range(input: &str) -> TimeError {
    TimeError {
        sqlstate: DATETIME_FIELD_OVERFLOW,
        message: format!("date/time field value out of range: \"{input}\""),
    }
}

pub fn cmp(a: i64, b: i64) -> std::cmp::Ordering {
    a.cmp(&b)
}

// --- output (time_out) -----------------------------------------------------

/// `time_out`: `HH:MM:SS`, plus up to six fractional digits (trailing zeros
/// trimmed). `24:00:00` is a representable value.
pub fn format(usec: i64) -> String {
    let hour = usec / USECS_PER_HOUR;
    let mut rem = usec % USECS_PER_HOUR;
    let min = rem / USECS_PER_MINUTE;
    rem %= USECS_PER_MINUTE;
    let sec = rem / USECS_PER_SEC;
    let frac = rem % USECS_PER_SEC;
    let mut out = format!("{hour:02}:{min:02}:{sec:02}");
    if frac != 0 {
        let f = format!("{frac:06}");
        out.push('.');
        out.push_str(f.trim_end_matches('0'));
    }
    out
}

// --- input (time_in) -------------------------------------------------------

/// `time_in`. Accepts `HH:MM[:SS[.ffffff]]` with an optional `AM`/`PM` and an
/// optional leading date and/or trailing zone (both ignored). A 7th fractional
/// digit rounds half-up (carrying into the day, so `23:59:59.9999999` becomes
/// `24:00:00`). Unparseable input is `22007`; an out-of-range value is `22008`.
pub fn parse(input: &str) -> Result<i64, TimeError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(invalid_syntax(input));
    }
    let mut time_tok: Option<&str> = None;
    let mut ampm: Option<&str> = None;
    let mut have_date = false;

    for tok in trimmed.split_whitespace() {
        let lower = tok.to_ascii_lowercase();
        if lower == "am" || lower == "pm" {
            ampm = Some(if lower == "am" { "am" } else { "pm" });
            continue;
        }
        if tok.contains(':') && !tok.starts_with(['+', '-']) {
            if time_tok.is_some() {
                return Err(invalid_syntax(input));
            }
            // A numeric offset glued to the time (`13:30:00-04`, `13:30:00+05:30`)
            // is accepted and ignored — this type carries no zone. It begins at
            // the first sign after the time digits.
            time_tok = Some(match tok.find(['+', '-']) {
                Some(pos) => &tok[..pos],
                None => tok,
            });
            continue;
        }
        // A `YYYY-MM-DD` date token is decorative for `time`.
        if is_date_token(tok) {
            have_date = true;
            continue;
        }
        // A numeric offset (`-07`, `+05:30`) or a fixed abbreviation is
        // accepted and ignored; a bare IANA zone name (`America/New_York`)
        // without a date has no determinable offset — reject it as PG does.
        if tok.contains('/') && !have_date {
            return Err(invalid_syntax(input));
        }
        // Otherwise treat the token as a decorative zone/abbreviation.
    }

    let time_tok = time_tok.ok_or_else(|| invalid_syntax(input))?;
    let (mut hour, min, sec, frac) = parse_hms(time_tok).ok_or_else(|| invalid_syntax(input))?;
    // Fold a 12-hour clock reading.
    if let Some(ap) = ampm {
        hour = match (ap, hour) {
            ("am", 12) => 0,
            ("pm", 12) => 12,
            ("pm", h) => h + 12,
            (_, h) => h,
        };
    }
    // Field bounds: PG allows the leap `sec == 60` and the `24:00:00` boundary;
    // the final micro count is range-checked against a whole day.
    if hour > 24 || min > 59 || sec > 60 {
        return Err(field_out_of_range(input));
    }
    let usec = hour * USECS_PER_HOUR + min * USECS_PER_MINUTE + sec * USECS_PER_SEC + frac;
    if !(0..=USECS_PER_DAY).contains(&usec) {
        return Err(field_out_of_range(input));
    }
    Ok(usec)
}

/// A bare `YYYY-MM-DD` (dash-separated, digits only) token.
fn is_date_token(tok: &str) -> bool {
    let parts: Vec<&str> = tok.split('-').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Parse `HH:MM[:SS[.ffffff]]`, returning `(hour, min, sec, usec)`. A 7th+
/// fractional digit rounds half-up. `am`/`pm` is folded by the caller.
fn parse_hms(tok: &str) -> Option<(i64, i64, i64, i64)> {
    let mut parts = tok.split(':');
    let hour: i64 = parts.next()?.parse().ok()?;
    let min: i64 = parts.next()?.parse().ok()?;
    let (sec, frac) = match parts.next() {
        None => (0, 0),
        Some(secpart) => {
            let (whole, fr) = secpart.split_once('.').unwrap_or((secpart, ""));
            let sec: i64 = whole.parse().ok()?;
            (sec, timestamp::parse_fraction(fr)?)
        }
    };
    if parts.next().is_some() {
        return None;
    }
    Some((hour, min, sec, frac))
}

// --- interval arithmetic ---------------------------------------------------

/// `time + interval` (`time_pl_interval`): add the interval's time-of-day part
/// (its month/day fields are irrelevant to a bare time) and wrap into the day.
pub fn pl_interval(usec: i64, span: Interval) -> i64 {
    // Fold the interval into a single day first: for a large interval `span.usec`
    // can be near `i64::MAX`, so adding it directly would overflow. Modular
    // reduction commutes with the add, so the result is unchanged.
    (usec + span.usec.rem_euclid(USECS_PER_DAY)).rem_euclid(USECS_PER_DAY)
}

/// `time - interval` (`time_mi_interval`): the negation of the add.
pub fn mi_interval(usec: i64, span: Interval) -> i64 {
    (usec - span.usec.rem_euclid(USECS_PER_DAY)).rem_euclid(USECS_PER_DAY)
}

/// `time - time` (`time_mi_time`): the microsecond difference, as an interval
/// whose only nonzero field is `usec`.
pub fn mi(a: i64, b: i64) -> Interval {
    Interval { months: 0, days: 0, usec: a - b }
}

// --- field extraction (date_part / extract) --------------------------------

/// Classify a (lowercased) unit spelling for `time`: `Some(true)` supported,
/// `Some(false)` a known field `time` does not carry, `None` unknown.
fn classify(unit: &str) -> Option<bool> {
    match unit {
        "microseconds" | "microsecond" | "usec" | "usecs" | "milliseconds" | "millisecond"
        | "msec" | "msecs" | "second" | "seconds" | "minute" | "minutes" | "hour" | "hours"
        | "epoch" => Some(true),
        // Known date/zone fields, not defined on a bare time.
        "day" | "days" | "month" | "months" | "year" | "years" | "quarter" | "week" | "dow"
        | "isodow" | "doy" | "decade" | "century" | "millennium" | "isoyear" | "timezone"
        | "timezone_hour" | "timezone_h" | "timezone_minute" | "timezone_m" => Some(false),
        _ => None,
    }
}

fn err_unit(unit: &str, supported: bool) -> TimeError {
    let verb = if supported { "not supported" } else { "not recognized" };
    TimeError {
        sqlstate: INVALID_PARAMETER_VALUE,
        message: format!("unit \"{unit}\" {verb} for type time without time zone"),
    }
}

/// `date_part(unit, time) -> float8`.
pub fn date_part(unit: &str, usec: i64) -> Result<f64, TimeError> {
    let lu = unit.trim().to_ascii_lowercase();
    match classify(&lu) {
        None => Err(err_unit(&lu, false)),
        Some(false) => Err(err_unit(&lu, true)),
        Some(true) => Ok(field_f64(&canon(&lu), usec)),
    }
}

/// `extract(unit FROM time) -> numeric`, with PG's per-field scale.
pub fn extract(unit: &str, usec: i64) -> Result<Numeric, TimeError> {
    let lu = unit.trim().to_ascii_lowercase();
    match classify(&lu) {
        None => Err(err_unit(&lu, false)),
        Some(false) => Err(err_unit(&lu, true)),
        Some(true) => {
            let (_, _, _, _, sub_usec) = split(usec);
            let s = match canon(&lu).as_str() {
                "second" => fixed_point(sub_usec, 6),
                "milliseconds" => fixed_point(sub_usec, 3),
                "microseconds" => sub_usec.to_string(),
                "epoch" => fixed_point(usec, 6),
                other => (field_f64(other, usec) as i64).to_string(),
            };
            Ok(Numeric::parse(&s).expect("extract renders a valid numeric literal"))
        }
    }
}

/// `(hour, min, sec, frac, sub_usec)` where `sub_usec = sec*1e6 + frac` is the
/// sub-minute microsecond count used by the seconds-scale fields.
fn split(usec: i64) -> (i64, i64, i64, i64, i64) {
    let hour = usec / USECS_PER_HOUR;
    let mut rem = usec % USECS_PER_HOUR;
    let min = rem / USECS_PER_MINUTE;
    rem %= USECS_PER_MINUTE;
    let sec = rem / USECS_PER_SEC;
    let frac = rem % USECS_PER_SEC;
    (hour, min, sec, frac, rem)
}

fn field_f64(unit: &str, usec: i64) -> f64 {
    let (hour, min, sec, frac, sub_usec) = split(usec);
    match unit {
        "microseconds" => sub_usec as f64,
        "milliseconds" => sub_usec as f64 / 1000.0,
        "second" => sec as f64 + frac as f64 / 1e6,
        "minute" => min as f64,
        "hour" => hour as f64,
        "epoch" => usec as f64 / 1e6,
        _ => unreachable!("field_f64 called with an unsupported unit"),
    }
}

/// Fold plural/alias spellings to the canonical seconds/minute/hour names.
fn canon(unit: &str) -> String {
    match unit {
        "microsecond" | "usec" | "usecs" => "microseconds",
        "millisecond" | "msec" | "msecs" => "milliseconds",
        "seconds" => "second",
        "minutes" => "minute",
        "hours" => "hour",
        other => other,
    }
    .to_string()
}

// --- make_time -------------------------------------------------------------

/// `make_time(hour, min, sec)`. Fields out of range raise `22008`.
pub fn make_time(hour: i64, min: i64, sec: f64) -> Result<i64, TimeError> {
    // PG prints the raw arguments (no zero-padding on hour/sec) in the error.
    let err = || TimeError {
        sqlstate: DATETIME_FIELD_OVERFLOW,
        message: format!("time field value out of range: {hour}:{min:02}:{}", fmt_secs(sec)),
    };
    if !(0..=24).contains(&hour)
        || !(0..=59).contains(&min)
        || !(0.0..60.0).contains(&sec)
        || (hour == 24 && (min != 0 || sec != 0.0))
    {
        return Err(err());
    }
    let whole = sec.trunc() as i64;
    let frac = (sec.fract() * 1e6).round() as i64;
    Ok(hour * USECS_PER_HOUR + min * USECS_PER_MINUTE + whole * USECS_PER_SEC + frac)
}

/// Format a seconds argument the way PG's error does: no trailing `.0`
/// (`2.1` → "2.1", `100.1` → "100.1", `5` → "5").
fn fmt_secs(sec: f64) -> String {
    if sec.fract() == 0.0 {
        format!("{}", sec as i64)
    } else {
        format!("{sec}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> i64 {
        parse(s).unwrap()
    }

    #[test]
    fn parse_format_and_edges() {
        assert_eq!(format(t("13:30:25.575401")), "13:30:25.575401");
        assert_eq!(format(t("00:00")), "00:00:00");
        assert_eq!(format(t("23:59:59.999999")), "23:59:59.999999");
        assert_eq!(format(t("23:59:59.9999999")), "24:00:00"); // rounds up
        assert_eq!(format(t("23:59:60")), "24:00:00"); // rounds up
        assert_eq!(format(t("24:00:00")), "24:00:00");
        assert_eq!(format(t("02:03 PST")), "02:03:00"); // abbrev ignored
        assert!(parse("24:00:00.01").is_err());
        assert!(parse("25:00:00").is_err());
        assert!(parse("15:36:39 America/New_York").is_err());
    }

    #[test]
    fn extract_fields() {
        let x = t("13:30:25.575401");
        assert_eq!(date_part("hour", x).unwrap(), 13.0);
        assert_eq!(date_part("epoch", x).unwrap(), 48625.575401);
        assert_eq!(extract("microsecond", x).unwrap().to_display(), "25575401");
        assert_eq!(extract("second", x).unwrap().to_display(), "25.575401");
        assert_eq!(extract("epoch", x).unwrap().to_display(), "48625.575401");
        assert!(date_part("day", x).is_err());
        assert!(date_part("fortnight", x).is_err());
    }

    #[test]
    fn arithmetic() {
        let base = t("10:00:00");
        assert_eq!(format(pl_interval(base, interval("01:30:00"))), "11:30:00");
        assert_eq!(mi(t("10:00:00"), t("08:00:00")).usec, 2 * USECS_PER_HOUR);

        // A huge interval must fold into the day, not overflow i64.
        let huge = Interval { months: 0, days: 0, usec: i64::MAX };
        let folded = i64::MAX.rem_euclid(USECS_PER_DAY);
        assert_eq!(pl_interval(base, huge), (base + folded).rem_euclid(USECS_PER_DAY));
        assert_eq!(mi_interval(base, huge), (base - folded).rem_euclid(USECS_PER_DAY));
    }

    #[test]
    fn make_time_ok_and_err() {
        assert_eq!(format(make_time(8, 20, 0.0).unwrap()), "08:20:00");
        assert!(make_time(10, 55, 100.1).is_err());
        assert!(make_time(24, 0, 2.1).is_err());
    }

    fn interval(s: &str) -> Interval {
        crate::interval::parse(s).unwrap()
    }
}
