//! `timestamp with time zone`: parsing, output, the field functions, and the
//! zone conversions (`AT TIME ZONE` / `make_timestamptz`).
//!
//! Clean-room (see AGENTS.md): reproduces PostgreSQL's *observable* behavior,
//! pinned by differential tests. The value is stored exactly like `timestamp` —
//! `i64` microseconds since 2000-01-01, with `i64::MIN`/`MAX` as the
//! `-infinity`/`infinity` sentinels — but the instant is in **UTC**. On input a
//! zone token (offset, abbreviation, or IANA name) is resolved to a UTC offset
//! and subtracted; on output the value is rendered in UTC (there is no session
//! `TimeZone` yet), so the offset is always `+00`. Zone resolution and DST live
//! in [`crate::tz`]; the calendar core is shared with [`crate::timestamp`].

use crate::NumericVal;
use crate::timestamp::{
    self, DATETIME_FIELD_OVERFLOW, INVALID_DATETIME_FORMAT, INVALID_PARAMETER_VALUE,
    INVALID_TIME_ZONE_DISPLACEMENT, NEG_INFINITY, POS_INFINITY, Parsed, TimestampError, decode,
    encode, format_parts, is_finite, validate_fields,
};
use crate::tz::{self, TmLite, ZoneError};

const USECS_PER_SEC: i64 = 1_000_000;

/// PG's `timestamptz` range is `[4714-11-24 00:00:00 UTC BC, 294276-12-31
/// 23:59:59.999999 UTC]`. We express it as a half-open micro range on the
/// stored value: `MIN_MICROS <= v < END_MICROS`.
fn min_micros() -> i64 {
    // 4714-11-24 00:00:00 BC == astronomical year -4713.
    encode_ymd(-4713, 11, 24)
}
fn end_micros() -> i64 {
    // One past the last valid instant: 294277-01-01 00:00:00.
    encode_ymd(294_277, 1, 1)
}
fn encode_ymd(year: i64, month: i64, day: i64) -> i64 {
    encode(timestamp::tm(year, month, day, 0, 0, 0, 0))
}

fn syntax_error(input: &str) -> TimestampError {
    TimestampError {
        sqlstate: INVALID_DATETIME_FORMAT,
        message: format!("invalid input syntax for type timestamp with time zone: \"{input}\""),
    }
}

fn out_of_range(input: &str) -> TimestampError {
    TimestampError {
        sqlstate: DATETIME_FIELD_OVERFLOW,
        message: format!("timestamp out of range: \"{input}\""),
    }
}

/// Re-label a `22023` unit error delegated to `timestamp` with the `timestamp
/// with time zone` type name (`timestamp units "x"` -> `timestamp with time
/// zone units "x"`), matching PG.
fn relabel_units(e: TimestampError) -> TimestampError {
    if e.sqlstate == INVALID_PARAMETER_VALUE {
        TimestampError {
            sqlstate: e.sqlstate,
            message: e
                .message
                .replacen("timestamp units", "timestamp with time zone units", 1),
        }
    } else {
        e
    }
}

/// Map a [`ZoneError`] to PG's SQLSTATE/message.
fn zone_error(e: ZoneError) -> TimestampError {
    match e {
        ZoneError::NotRecognized(name) => TimestampError {
            sqlstate: INVALID_PARAMETER_VALUE,
            message: format!("time zone \"{name}\" not recognized"),
        },
        ZoneError::DisplacementOutOfRange(name) => TimestampError {
            sqlstate: INVALID_TIME_ZONE_DISPLACEMENT,
            message: format!("time zone displacement out of range: \"{name}\""),
        },
    }
}

fn tmlite(micros: i64) -> TmLite {
    let tm = decode(micros);
    TmLite { year: tm.year, month: tm.month, day: tm.day, hour: tm.hour, min: tm.min, sec: tm.sec }
}

/// `timestamptz_in`. Interprets any trailing zone token to convert the wall
/// clock to UTC (no zone token means UTC, the session zone); `infinity`/`epoch`
/// pass through. Syntax errors are `22007`, out-of-range results `22008`, an
/// unknown zone `22023`, a bad numeric offset `22009`.
pub fn parse(input: &str) -> Result<i64, TimestampError> {
    let parsed = timestamp::parse_parts(input).map_err(|e| relabel_syntax(e, input))?;
    let (tm, zone) = match parsed {
        // infinity/-infinity/epoch are zone-independent.
        Parsed::Micros(m) => return Ok(m),
        Parsed::Calendar { tm, zone } => (tm, zone),
    };
    validate_fields(&tm, input)?;
    let civil = encode(tm);
    let off_secs = match zone {
        None => 0,
        Some(tok) => {
            let zone = tz::resolve_zone(&tok).map_err(zone_error)?;
            tz::offset_for_local(&zone, TmLite {
                year: tm.year,
                month: tm.month,
                day: tm.day,
                hour: tm.hour,
                min: tm.min,
                sec: tm.sec,
            })
        }
    };
    let utc = civil - off_secs as i64 * USECS_PER_SEC;
    if utc < min_micros() || utc >= end_micros() {
        return Err(out_of_range(input));
    }
    Ok(utc)
}

/// Re-label a `22007` error from the shared scan with the `timestamp with time
/// zone` type name (other SQLSTATEs keep their type-agnostic messages).
fn relabel_syntax(e: TimestampError, input: &str) -> TimestampError {
    if e.sqlstate == INVALID_DATETIME_FORMAT {
        syntax_error(input)
    } else {
        e
    }
}

/// `timestamptz_out`, rendered in UTC. The offset is `+00`, spliced before any
/// ` BC` suffix (matching PG's `… 4714+00 BC` ordering).
pub fn format(micros: i64) -> String {
    if micros == POS_INFINITY {
        return "infinity".to_string();
    }
    if micros == NEG_INFINITY {
        return "-infinity".to_string();
    }
    let (body, bc) = format_parts(micros);
    if bc { format!("{body}+00 BC") } else { format!("{body}+00") }
}

// --- field functions -------------------------------------------------------
//
// Under a UTC display zone the stored value *is* the UTC wall clock, so every
// field except the timezone group is identical to `timestamp`. We intercept the
// `timezone`/`timezone_hour`/`timezone_minute` fields (always 0 here) and defer
// the rest.

fn is_tz_field(unit: &str) -> bool {
    matches!(
        unit.trim().to_ascii_lowercase().as_str(),
        "timezone" | "timezone_hour" | "timezone_minute"
    )
}

/// `date_part(text, timestamptz) -> float8`. `Ok(None)` is SQL NULL.
pub fn date_part(unit: &str, micros: i64) -> Result<Option<f64>, TimestampError> {
    if is_tz_field(unit) {
        // The offset in the UTC session zone is 0 for every finite value; on
        // ±infinity the field is NULL (an oscillating field).
        return Ok(if is_finite(micros) { Some(0.0) } else { None });
    }
    timestamp::date_part(unit, micros).map_err(relabel_units)
}

/// `EXTRACT(field FROM timestamptz) -> numeric`. `Ok(None)` is SQL NULL.
pub fn extract(unit: &str, micros: i64) -> Result<Option<NumericVal>, TimestampError> {
    if is_tz_field(unit) {
        return Ok(if is_finite(micros) {
            Some(NumericVal::Finite("0".to_string()))
        } else {
            None
        });
    }
    timestamp::extract(unit, micros).map_err(relabel_units)
}

/// `date_trunc(text, timestamptz) -> timestamptz`. Under UTC this truncates the
/// UTC wall clock, so it defers to the `timestamp` implementation.
pub fn date_trunc(unit: &str, micros: i64) -> Result<i64, TimestampError> {
    timestamp::date_trunc(unit, micros).map_err(relabel_units)
}

/// `isfinite(timestamptz) -> bool`.
pub fn is_finite_tstz(micros: i64) -> bool {
    is_finite(micros)
}

/// `make_timestamptz(year, month, mday, hour, min, sec[, zone])`. Without a
/// zone the fields are taken as UTC (the session zone).
pub fn make_timestamptz(
    year: i64,
    month: i64,
    mday: i64,
    hour: i64,
    min: i64,
    sec: f64,
    zone: Option<&str>,
) -> Result<i64, TimestampError> {
    let civil = timestamp::make_timestamp(year, month, mday, hour, min, sec)?;
    let off_secs = match zone {
        None => 0,
        Some(tok) => {
            let zone = tz::resolve_zone(tok).map_err(zone_error)?;
            tz::offset_for_local(&zone, tmlite(civil))
        }
    };
    Ok(civil - off_secs as i64 * USECS_PER_SEC)
}

/// `timestamptz AT TIME ZONE zone` (= `timezone(zone, timestamptz)`): the wall
/// clock the instant shows in `zone`, as a zone-less `timestamp`. `±infinity`
/// passes through.
pub fn at_zone_to_timestamp(zone: &str, micros: i64) -> Result<i64, TimestampError> {
    if !is_finite(micros) {
        return Ok(micros);
    }
    let zone = tz::resolve_zone(zone).map_err(zone_error)?;
    let off_secs = tz::offset_for_instant(&zone, micros);
    Ok(micros + off_secs as i64 * USECS_PER_SEC)
}

/// `timestamp AT TIME ZONE zone` (= `timezone(zone, timestamp)`): interpret the
/// zone-less wall clock as being in `zone`, yielding the UTC `timestamptz`
/// instant. `±infinity` passes through.
pub fn timestamp_at_zone(zone: &str, micros: i64) -> Result<i64, TimestampError> {
    if !is_finite(micros) {
        return Ok(micros);
    }
    let zone = tz::resolve_zone(zone).map_err(zone_error)?;
    let off_secs = tz::offset_for_local(&zone, tmlite(micros));
    Ok(micros - off_secs as i64 * USECS_PER_SEC)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> i64 {
        parse(s).unwrap()
    }

    #[test]
    fn offset_normalizes_to_utc() {
        // -08:00 shifts the wall clock forward 8h to UTC.
        assert_eq!(format(p("1997-02-10 17:32:01-08")), "1997-02-11 01:32:01+00");
        assert_eq!(format(p("1997-02-10 17:32:01-0800")), "1997-02-11 01:32:01+00");
        assert_eq!(format(p("1997-02-10 17:32:01 -08:00")), "1997-02-11 01:32:01+00");
        // No zone token -> already UTC.
        assert_eq!(format(p("2001-02-16 20:38:40")), "2001-02-16 20:38:40+00");
        assert_eq!(format(p("2001-02-16 20:38:40+00")), "2001-02-16 20:38:40+00");
    }

    #[test]
    fn named_zone_and_abbrev_input() {
        // America/New_York in February is EST (-05:00).
        assert_eq!(
            format(p("1997-02-10 17:32:01 America/New_York")),
            "1997-02-10 22:32:01+00"
        );
        // PST is a fixed -08:00.
        assert_eq!(format(p("1997-02-10 17:32:01 PST")), "1997-02-11 01:32:01+00");
        // UTC / Z synonyms.
        assert_eq!(format(p("1997-02-10 17:32:01 UTC")), "1997-02-10 17:32:01+00");
        assert_eq!(format(p("2001-09-22T18:19:20Z")), "2001-09-22 18:19:20+00");
    }

    #[test]
    fn specials_and_fractions() {
        assert_eq!(format(p("infinity")), "infinity");
        assert_eq!(format(p("-infinity")), "-infinity");
        assert_eq!(format(p("epoch")), "1970-01-01 00:00:00+00");
        assert_eq!(format(p("2001-02-16 20:38:40.5+00")), "2001-02-16 20:38:40.5+00");
    }

    #[test]
    fn bc_and_boundaries() {
        assert_eq!(format(p("0097-02-16 20:00:00+00 BC")), "0097-02-16 20:00:00+00 BC");
        // Lower boundary: 4714-11-24 00:00:00 UTC BC is valid.
        assert!(parse("4714-11-24 00:00:00+00 BC").is_ok());
        assert!(parse("4714-11-23 16:00:00-08 BC").is_ok()); // == the same instant
        // One second earlier is out of range.
        let e = parse("4714-11-23 23:59:59+00 BC").unwrap_err();
        assert_eq!(e.sqlstate, DATETIME_FIELD_OVERFLOW);
        // Upper boundary.
        assert!(parse("294276-12-31 23:59:59+00").is_ok());
        assert_eq!(
            parse("294277-01-01 00:00:00+00").unwrap_err().sqlstate,
            DATETIME_FIELD_OVERFLOW
        );
    }

    #[test]
    fn errors() {
        let e = parse("garbage").unwrap_err();
        assert_eq!(e.sqlstate, INVALID_DATETIME_FORMAT);
        assert_eq!(
            e.message,
            "invalid input syntax for type timestamp with time zone: \"garbage\""
        );
        assert_eq!(
            parse("2001-01-01 00:00 Nowhere/Nozone").unwrap_err().sqlstate,
            INVALID_PARAMETER_VALUE
        );
    }

    #[test]
    fn at_time_zone_round_trip() {
        // A UTC instant shown in New York (EST -5h) reads 5h earlier.
        let utc = p("2001-02-16 20:38:40+00");
        let wall = at_zone_to_timestamp("America/New_York", utc).unwrap();
        assert_eq!(timestamp::format(wall), "2001-02-16 15:38:40");
        // Interpreting that wall clock back in New York returns the UTC instant.
        let back = timestamp_at_zone("America/New_York", wall).unwrap();
        assert_eq!(back, utc);
    }

    #[test]
    fn make_and_fields() {
        // 6-arg is UTC.
        assert_eq!(
            format(make_timestamptz(2013, 7, 15, 8, 15, 23.5, None).unwrap()),
            "2013-07-15 08:15:23.5+00"
        );
        // 7-arg with a summer EDT zone (-04:00) shifts +4h to UTC.
        assert_eq!(
            format(make_timestamptz(2013, 7, 15, 17, 15, 23.0, Some("America/New_York")).unwrap()),
            "2013-07-15 21:15:23+00"
        );
        // timezone* fields are 0 under the UTC session zone.
        let v = p("2001-02-16 20:38:40+00");
        assert_eq!(date_part("timezone", v).unwrap(), Some(0.0));
        assert_eq!(date_part("timezone_hour", v).unwrap(), Some(0.0));
        assert_eq!(date_part("hour", v).unwrap(), Some(20.0));
    }
}
