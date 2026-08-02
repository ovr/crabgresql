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
//! A missing zone takes the offset the session zone is at on the transaction's
//! date — see [`crate::FmtCtx::zone_offset_today`].
//!
//! One further deviation from PG, acceptable while no passing test needs it: a
//! zone-backed abbreviation (`MSK`) needs a date here, because we resolve it
//! through its reference zone. PG additionally knows *when that zone used that
//! abbreviation*, so it can answer `'15:36:39 MSK'` with the zone's standard
//! offset and no date at all.

use crate::Numeric;
use crate::fmt::FmtCtx;
use crate::interval::Interval;
use crate::time;
use crate::timestamp::{self, fixed_point};
use crate::tz::{self, TmLite, Zone};

const INVALID_DATETIME_FORMAT: &str = "22007";
const DATETIME_FIELD_OVERFLOW: &str = "22008";
const INVALID_TIME_ZONE_DISPLACEMENT: &str = "22009";
const INVALID_PARAMETER_VALUE: &str = "22023";

const USECS_PER_DAY: i64 = 86_400_000_000;
const USECS_PER_HOUR: i64 = 3_600_000_000;
const USECS_PER_MINUTE: i64 = 60_000_000;
const USECS_PER_SEC: i64 = 1_000_000;

/// A `time with time zone` value: local time-of-day plus a UTC offset held as
/// seconds **west** of UTC (so a `-07:00` display offset is `zone == 25200`).
#[derive(deepsize::DeepSizeOf, Clone, Copy, Debug, PartialEq, Eq)]
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
    // The shared renderer, so `timetz_out` and `timestamptz_out` cannot drift
    // apart on how wide an offset prints. It counts east; `zone` counts west.
    out.push_str(&tz::format_offset(-v.zone));
    out
}

// --- input (timetz_in) -----------------------------------------------------

/// `timetz_in`. Accepts `HH:MM[:SS[.ffffff]]` with an optional `AM`/`PM`, an
/// optional leading date, and a zone: a numeric UTC offset (`-07`, `+05:30`, or
/// glued to the time), a fixed abbreviation (`PDT`), or a named IANA zone.
///
/// A zone that only means something at an instant — a named zone, or a
/// zone-backed abbreviation — needs the date to resolve, so it is an error
/// without one. Every token is accounted for: an unrecognized trailing word is
/// `22007`, never silently ignored.
///
/// A missing offset takes `session`'s, so `'03:30'::timetz` and
/// `'03:30'::time::timetz` agree — PG resolves both through the session zone
/// too, and having only one of them do it made the same value compare unequal
/// to itself.
///
/// The two whole-value specials `time` accepts are accepted here too, and they
/// differ in how they treat the session zone: `now` takes the offset in effect
/// at the transaction timestamp, while `allballs` is `00:00:00+00` in every
/// zone — PG's decoder gives it a literal zero offset rather than the session's.
pub fn parse(input: &str, fmt: &FmtCtx) -> Result<TimeTz, TimeTzError> {
    let session = &fmt.zone;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(invalid_syntax(input));
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "now" => {
            let at = fmt.xact_start().map_err(|e| TimeTzError {
                sqlstate: e.sqlstate,
                message: e.message,
            })?;
            let wall = timestamp::session_wall_clock(fmt).map_err(|e| TimeTzError {
                sqlstate: e.sqlstate,
                message: e.message,
            })?;
            return Ok(TimeTz {
                usec: wall.rem_euclid(USECS_PER_DAY),
                zone: -session.offset_at(at),
            });
        }
        "allballs" => return Ok(TimeTz { usec: 0, zone: 0 }),
        _ => {}
    }
    let mut time_str: Option<String> = None;
    let mut ampm: Option<&str> = None;
    let mut zone_tok: Option<String> = None;
    let mut date: Option<(i64, i64, i64)> = None;

    // Record a zone token, rejecting a second one (`'15:36:39 MSK m2'`).
    let set_zone = |tok: &str, seen: &mut Option<String>| -> Result<(), TimeTzError> {
        if seen.is_some() {
            return Err(invalid_syntax(input));
        }
        *seen = Some(tok.to_string());
        Ok(())
    };

    for tok in trimmed.split_whitespace() {
        let lower = tok.to_ascii_lowercase();
        if lower == "am" || lower == "pm" {
            // A second meridiem is bad syntax, not last-one-wins — the same rule
            // the zone and date guards below apply.
            if ampm.is_some() {
                return Err(invalid_syntax(input));
            }
            ampm = Some(if lower == "am" { "am" } else { "pm" });
            continue;
        }
        if tok.starts_with(['+', '-']) {
            set_zone(tok, &mut zone_tok)?;
            continue;
        }
        if tok.contains(':') && time_str.is_none() {
            // A glued offset (`13:30:25.5-04`) begins at the first sign.
            match tok.find(['+', '-']) {
                Some(pos) => {
                    set_zone(&tok[pos..], &mut zone_tok)?;
                    time_str = Some(tok[..pos].to_string());
                }
                None => time_str = Some(tok.to_string()),
            }
            continue;
        }
        if let Some((y, m, d)) = parse_date_token(tok) {
            if date.is_some() {
                return Err(invalid_syntax(input));
            }
            if !valid_date(y, m, d) {
                return Err(field_out_of_range(input));
            }
            date = Some((y, m, d));
            continue;
        }
        // Anything left is a zone name or abbreviation; resolved below, once
        // the time-of-day has had its chance to raise the better error.
        set_zone(tok, &mut zone_tok)?;
    }

    let time_str = time_str.ok_or_else(|| invalid_syntax(input))?;
    // Reuse `time`'s parser for the time-of-day (with any am/pm suffix), then
    // remap its error to name `time with time zone`. This runs *before* zone
    // resolution because PG reports the time error first: `'25:00:00 PDT'` is
    // `22008`, not a zone complaint.
    let time_input = match ampm {
        Some(ap) => format!("{time_str} {ap}"),
        None => time_str,
    };
    let usec = time::parse(&time_input, fmt).map_err(|e| {
        if e.sqlstate == DATETIME_FIELD_OVERFLOW {
            field_out_of_range(input)
        } else {
            invalid_syntax(input)
        }
    })?;

    let gmtoff = match &zone_tok {
        // No zone of its own: the offset the session zone is at today, so this
        // agrees with what the `time -> timetz` cast produces.
        None => fmt.zone_offset_today(),
        Some(tok) => resolve(tok, date, usec, input)?,
    };
    Ok(TimeTz {
        usec,
        zone: -gmtoff, // stored west-of-UTC
    })
}

/// Resolve a zone token to seconds **east** of UTC.
///
/// A [`Zone::Fixed`] answer is good on its own. A [`Zone::Named`] one is only
/// meaningful at an instant, so it needs the literal's date; without one, PG
/// reports plain bad syntax (not "zone not recognized"), because its decoder
/// never gets far enough to blame the zone.
fn resolve(
    tok: &str,
    date: Option<(i64, i64, i64)>,
    usec: i64,
    input: &str,
) -> Result<i32, TimeTzError> {
    let zone = tz::resolve_zone(tok).map_err(|e| zone_error(e, tok, input))?;
    match zone {
        Zone::Fixed(secs) => Ok(secs),
        Zone::Named(_) => {
            let (year, month, day) = date.ok_or_else(|| invalid_syntax(input))?;
            // `24:00:00` has no hour 24 in a civil clock; clamp for the purpose
            // of picking an offset, which no zone transition can distinguish.
            let tod = usec.min(USECS_PER_DAY - 1);
            Ok(tz::offset_for_local(
                &zone,
                TmLite {
                    year,
                    month,
                    day,
                    hour: tod / USECS_PER_HOUR,
                    min: tod % USECS_PER_HOUR / USECS_PER_MINUTE,
                    sec: tod % USECS_PER_MINUTE / USECS_PER_SEC,
                },
            ))
        }
    }
}

/// Map a zone-resolution failure onto PG's `timetz_in` errors.
///
/// Which error depends on how far PG's decoder got. A bare word is just another
/// unrecognized field, so the whole input is bad syntax — `'15:36:39 m2'` is
/// `22007`, not `time zone "m2" not recognized`. But a token *shaped* like a
/// zone spec reaches the zone lookup, and its failure is blamed on the zone:
/// `22023`, quoting the lowercased token rather than the input.
fn zone_error(e: tz::ZoneError, tok: &str, input: &str) -> TimeTzError {
    match e {
        tz::ZoneError::NotRecognized(_) if looks_like_zone_spec(tok) => TimeTzError {
            sqlstate: INVALID_PARAMETER_VALUE,
            message: format!("time zone \"{}\" not recognized", tok.to_ascii_lowercase()),
        },
        tz::ZoneError::NotRecognized(_) => invalid_syntax(input),
        tz::ZoneError::DisplacementOutOfRange(_) => TimeTzError {
            sqlstate: INVALID_TIME_ZONE_DISPLACEMENT,
            message: format!("time zone displacement out of range: \"{input}\""),
        },
    }
}

/// Whether a token is shaped like a zone *spec* rather than a plain word, and so
/// reaches PG's zone lookup: an IANA-style `Area/Location`, or the POSIX
/// `<letters><±offset>` form. Verified against PG 18 — `'Nowhere/Nozone'` and
/// `'UTC+168'` are blamed on the zone, while `'foo'` and `'EST5EDT'` are not.
fn looks_like_zone_spec(tok: &str) -> bool {
    tok.contains('/')
        || matches!(tok.find(['+', '-']), Some(sign) if sign > 0
            && tok[..sign].bytes().all(|b| b.is_ascii_alphabetic())
            && tok[sign + 1..].bytes().all(|b| b.is_ascii_digit() || b == b':' || b == b'.'))
}

/// A `YYYY-MM-DD` token, as year/month/day.
///
/// Shape only — three all-digit parts. Whether the fields name a real day is a
/// separate question, answered by [`valid_date`], because the two failures get
/// different errors: a token that is not a date at all is a zone token, while
/// `2003-02-30` *is* a date token and is `22008`.
fn parse_date_token(tok: &str) -> Option<(i64, i64, i64)> {
    let parts: Vec<&str> = tok.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut it = parts.iter().map(|p| {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        p.parse::<i64>().ok()
    });
    let y = it.next()??;
    let m = it.next()??;
    let d = it.next()??;
    Some((y, m, d))
}

/// Whether `(y, m, d)` names a day that exists, month length and leap years
/// included.
///
/// Not a nicety: the resolved date reaches `jiff` through
/// [`tz::offset_for_local`], whose civil-datetime constructor treats its input
/// as already validated and panics otherwise — so without this check
/// `timetz '2003-02-30 12:00 America/New_York'` takes the backend down. PG
/// answers `22008`.
fn valid_date(y: i64, m: i64, d: i64) -> bool {
    // There is no year 0 in the proleptic Gregorian calendar PG uses; 1 BC is
    // followed by 1 AD. A large year is fine — PG takes `300000-01-01` here,
    // because the date only picks a zone offset and is then discarded.
    y != 0 && (1..=12).contains(&m) && d >= 1 && d <= crate::timestamp::days_in_month(y, m)
}

// --- zone rotation (timetz_zone / timetz_izone / AT LOCAL) -----------------

/// `timetz AT TIME ZONE <zone>` (`timetz_zone`/`timetz_izone`): the same instant
/// of day, read in a different zone. `off_east` is seconds east of UTC.
///
/// The instant is a time *of day*, so the result wraps modulo a day and loses
/// any notion of which day it landed on — `00:01-07` at `-10` is `21:01-10`, the
/// previous day's evening, exactly as PG reports it.
pub fn at_zone(v: TimeTz, off_east: i32) -> TimeTz {
    let utc = utc_key(v);
    TimeTz {
        usec: (utc + off_east as i64 * USECS_PER_SEC).rem_euclid(USECS_PER_DAY),
        zone: -off_east,
    }
}

/// `timetz AT TIME ZONE '<name>'` (`timetz_zone`). Unlike `timetz_in`, an
/// unresolvable name here *is* blamed on the zone — this is the
/// `AT TIME ZONE`/`timezone()` path, where PG reports `22023 time zone "…" not
/// recognized`.
///
/// A named zone cannot name an offset without a date, and a `timetz` carries
/// none — so PG's `timetz_zone` resolves the zone at the current instant. `at`
/// is that instant, the session's transaction timestamp, which keeps the answer
/// stable for the whole transaction instead of drifting per row.
pub fn at_zone_named(v: TimeTz, zone: &str, at: i64) -> Result<TimeTz, TimeTzError> {
    let zone = tz::resolve_zone(zone).map_err(|e| match e {
        tz::ZoneError::NotRecognized(name) => TimeTzError {
            sqlstate: INVALID_PARAMETER_VALUE,
            message: format!("time zone \"{name}\" not recognized"),
        },
        tz::ZoneError::DisplacementOutOfRange(name) => TimeTzError {
            sqlstate: INVALID_TIME_ZONE_DISPLACEMENT,
            message: format!("time zone displacement out of range: \"{name}\""),
        },
    })?;
    Ok(at_zone(v, tz::offset_for_instant(&zone, at)))
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
    use crate::tz::SessionZone;

    /// A clockless input context in `z` — every case in this module spells out
    /// its own time, so a relative special would be a test bug.
    fn in_zone(z: &SessionZone) -> FmtCtx {
        FmtCtx::utc_default().with_zone(std::sync::Arc::new(z.clone()))
    }

    fn v(s: &str) -> TimeTz {
        match parse(s, &FmtCtx::utc_default()) {
            Ok(value) => value,
            Err(error) => panic!("invalid timetz test fixture `{s}`: {error:?}"),
        }
    }

    #[test]
    fn parse_format_and_edges() {
        assert_eq!(format(v("13:30:25.575401-04")), "13:30:25.575401-04");
        assert_eq!(format(v("05:06:07-07")), "05:06:07-07");
        assert_eq!(format(v("12:00:00+05:30")), "12:00:00+05:30");
        assert_eq!(format(v("12:00")), "12:00:00+00"); // no offset: the session's
        assert_eq!(format(v("23:59:60 -07")), "24:00:00-07"); // rounds up
        assert!(parse("24:00:00.01 -07", &FmtCtx::utc_default()).is_err());
        assert!(parse("15:36:39 America/New_York", &FmtCtx::utc_default()).is_err());
    }

    /// An input with no offset of its own takes the session zone's, so it
    /// agrees with what `'12:00'::time::timetz` produces.
    #[test]
    fn a_missing_offset_takes_the_session_zone() -> anyhow::Result<()> {
        let ny = SessionZone::resolve("America/New_York")?;
        assert_eq!(format(parse("12:00", &in_zone(&ny))?), "12:00:00-05");
        // An explicit offset still wins.
        assert_eq!(
            format(parse("12:00+05:30", &in_zone(&ny))?),
            "12:00:00+05:30"
        );
        Ok(())
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

#[cfg(test)]
mod zone_tests {
    use super::*;

    /// Every entry of `tz::DATETIME_ABBREVS` that names a constant offset, read
    /// off a live PostgreSQL 18.4 (`select timetz '12:00 <abbrev>'`) before
    /// being pinned. The table's doc comment says to add abbreviations only as
    /// differential tests demand, so this is that test — without it nothing
    /// would catch a mistyped offset.
    #[test]
    fn fixed_abbreviations_match_pg() {
        let cases = [
            ("AST", "-04"),
            ("ADT", "-03"),
            ("EST", "-05"),
            ("EDT", "-04"),
            ("CST", "-06"),
            ("CDT", "-05"),
            ("MST", "-07"),
            ("MDT", "-06"),
            ("PST", "-08"),
            ("PDT", "-07"),
            ("AKST", "-09"),
            ("AKDT", "-08"),
            ("HST", "-10"),
        ];
        for (abbrev, offset) in cases {
            let input = format!("12:00 {abbrev}");
            match parse(&input, &FmtCtx::utc_default()) {
                Ok(v) => assert_eq!(format(v), format!("12:00:00{offset}"), "abbrev {abbrev}"),
                Err(e) => panic!("abbrev {abbrev} should parse, got {e:?}"),
            }
        }
    }

    /// Every case here was read off a live PostgreSQL 18.4 before being pinned.
    #[test]
    fn zone_tokens_resolve_or_fail_like_pg() {
        let ok = [
            ("00:01 PDT", "00:01:00-07"),
            ("07:07 PST", "07:07:00-08"),
            ("08:08 EDT", "08:08:00-04"),
            ("11:59:59.99 PM PDT", "23:59:59.99-07"),
            ("2003-03-07 15:36:39 America/New_York", "15:36:39-05"),
            ("2003-07-07 15:36:39 America/New_York", "15:36:39-04"),
            ("2003-03-07 12:00 america/new_york", "12:00:00-05"),
            // The abbreviation prefix is ignored; the offset is POSIX-signed.
            ("12:00:00 UTC+10", "12:00:00-10"),
            ("12:00 UTC-3", "12:00:00+03"),
            ("12:00 Z", "12:00:00+00"),
            ("12:00 GMT", "12:00:00+00"),
            ("12:00-0730", "12:00:00-07:30"),
            ("24:00:00 PDT", "24:00:00-07"),
            ("23:59:60 PDT", "24:00:00-07"),
        ];
        for (input, want) in ok {
            match parse(input, &FmtCtx::utc_default()) {
                Ok(v) => assert_eq!(format(v), want, "input {input}"),
                Err(e) => panic!("input {input} should parse, got {e:?}"),
            }
        }

        // Syntax errors (22007): an unknown word, a zone that needs a date, and
        // a repeated meridiem. A bare word never gets blamed on the zone.
        for input in [
            "15:36:39 America/New_York",
            "15:36:39 m2",
            "15:36:39 MSK m2",
            "2003-07-07 15:36:39 MSK m2",
            "12:00 foo",
            "12:00 EST5EDT",
            "12:00 am pm",
        ] {
            let Err(e) = parse(input, &FmtCtx::utc_default()) else {
                panic!("input {input} should fail");
            };
            assert_eq!(e.sqlstate, INVALID_DATETIME_FORMAT, "input {input}");
        }

        // Field overflow (22008) wins over any zone complaint. The date cases
        // are regression guards: an impossible day used to reach `jiff`'s
        // civil-datetime constructor, which panics rather than erroring, so
        // `'2003-02-30 …'` with a named zone took the whole backend down.
        for input in [
            "24:00:00.01 PDT",
            "23:59:60.01 PDT",
            "24:01:00 PDT",
            "25:00:00 PDT",
            "2003-02-30 12:00 America/New_York",
            "2003-04-31 12:00 America/New_York",
            "2003-02-30 12:00-04",
            "2003-13-01 12:00-04",
            // No year 0: 1 BC is followed by 1 AD.
            "0000-01-01 12:00-04",
            "0000-06-15 12:00 America/New_York",
        ] {
            let Err(e) = parse(input, &FmtCtx::utc_default()) else {
                panic!("input {input} should fail");
            };
            assert_eq!(e.sqlstate, DATETIME_FIELD_OVERFLOW, "input {input}");
        }

        // A token *shaped* like a zone spec reaches PG's zone lookup, so its
        // failure is 22023 blaming the (lowercased) token, not 22007 blaming the
        // whole input. Pinned against PG 18.4.
        for (input, want) in [
            ("12:00 Nowhere/Nozone", "time zone \"nowhere/nozone\" not recognized"),
            ("12:00 UTC+168", "time zone \"utc+168\" not recognized"),
        ] {
            let Err(e) = parse(input, &FmtCtx::utc_default()) else {
                panic!("input {input} should fail");
            };
            assert_eq!(e.sqlstate, INVALID_PARAMETER_VALUE, "input {input}");
            assert_eq!(e.message, want, "input {input}");
        }

        // A bare numeric offset past ±15:59:59 is 22009, quoting the whole input.
        let Err(e) = parse("12:00:00+16:00", &FmtCtx::utc_default()) else {
            panic!("expected a displacement error");
        };
        assert_eq!(e.sqlstate, INVALID_TIME_ZONE_DISPLACEMENT);
        assert_eq!(
            e.message,
            "time zone displacement out of range: \"12:00:00+16:00\""
        );
    }

    // --- the whole-value specials (pinned against PostgreSQL 18.4) ---------

    fn at(zone: &str) -> FmtCtx {
        FmtCtx::utc_at(1, 763_860_600_123_456, 763_860_600_123_456).with_zone(std::sync::Arc::new(
            crate::tz::SessionZone::resolve(zone).expect("real zone"),
        ))
    }

    fn rel(input: &str, zone: &str) -> String {
        match parse(input, &at(zone)) {
            Ok(v) => format(v),
            Err(e) => panic!("{input:?} in {zone}: {e:?}"),
        }
    }

    /// `now` takes the offset in effect at the transaction timestamp — so it
    /// carries the session zone. `allballs` does *not*: PG's decoder gives it
    /// a literal zero offset, and it reads `00:00:00+00` in every zone.
    #[test]
    fn now_takes_the_session_offset_and_allballs_does_not() {
        assert_eq!(rel("now", "UTC"), "23:30:00.123456+00");
        assert_eq!(rel("now", "America/New_York"), "19:30:00.123456-04");
        assert_eq!(rel("now", "Asia/Kolkata"), "05:00:00.123456+05:30");
        for zone in ["UTC", "America/New_York", "Asia/Kolkata"] {
            assert_eq!(rel("allballs", zone), "00:00:00+00", "allballs in {zone}");
        }
    }

    #[test]
    fn the_date_shaped_specials_are_rejected() {
        for bad in ["today", "tomorrow", "yesterday", "epoch", "infinity"] {
            let e = parse(bad, &at("UTC")).expect_err(bad);
            assert_eq!(e.sqlstate, INVALID_DATETIME_FORMAT, "{bad}");
        }
    }

    /// A zone-less `timetz` takes the offset the session zone is at *today*,
    /// which at the frozen instant is New York's DST offset — not the standard
    /// `-05` a clockless context would fall back to.
    #[test]
    fn a_zoneless_literal_takes_todays_offset() {
        assert_eq!(rel("03:30", "America/New_York"), "03:30:00-04");
    }
}
