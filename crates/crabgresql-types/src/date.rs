//! `date`: parsing, ISO output, comparison, integer/interval arithmetic, and
//! the field functions (`date_part`/`extract`/`isfinite`/`make_date`).
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior — the ISO output format, the field values, and the SQLSTATE/message
//! of range and syntax errors — pinned by differential tests against real PG,
//! implemented independently. The calendar conversions reuse the public-domain
//! `days_from_civil`/`civil_from_days` algorithm already used by
//! [`crate::timestamp`], not PG's source.
//!
//! Representation: signed days since the PostgreSQL epoch (2000-01-01), held in
//! an `i32`. `i32::MIN`/`i32::MAX` are the `-infinity`/`infinity` sentinels, so
//! the natural integer order already sorts them correctly.
//!
//! Deviations from PG, acceptable while no passing test needs them: only ISO
//! `Y-M-D` and the verbose `Mon DD, YYYY` input forms are accepted (the
//! `SET DateStyle` ymd/dmy/mdy matrix, Julian `J…` input, and `.NNN`
//! day-of-year forms are not), and the current-relative specials
//! (`now`/`today`/`yesterday`/`tomorrow`) need a transaction clock and are not
//! supported — matching how `timestamp` parses today.

use crate::interval::Interval;
use crate::timestamp::{
    self, POSTGRES_EPOCH_JDATE, century, date2j, days_in_month, decade, iso_week_year, j2date,
    j2day, millennium,
};
use crate::Numeric;

// SQLSTATEs (kept as literals; the types crate does not depend on the protocol
// crate — the binder/executor map these to `sqlstate::*`).
const INVALID_DATETIME_FORMAT: &str = "22007";
const DATETIME_FIELD_OVERFLOW: &str = "22008";
const INVALID_PARAMETER_VALUE: &str = "22023";

/// `-infinity` / `+infinity` sentinels: `'-infinity'::date` sorts before and
/// `'infinity'::date` after every finite date, and `isfinite` reports false.
pub const NEG_INFINITY: i32 = i32::MIN;
pub const POS_INFINITY: i32 = i32::MAX;

/// PG's finite `date` range, expressed in days since 2000-01-01:
/// `4714-11-24 BC` (Julian day 0) .. `5874897-12-31`.
const DATE_MINVAL: i32 = -(POSTGRES_EPOCH_JDATE as i32); // Julian day 0
const JULIAN_MAX: i32 = 2_147_483_494;
const DATE_MAXVAL: i32 = JULIAN_MAX - POSTGRES_EPOCH_JDATE as i32;

/// Days from the Unix epoch (1970-01-01) to the PG epoch, as a date value; the
/// value of `date 'epoch'` and the shift used for the `epoch` field.
const UNIX_EPOCH_DATE: i32 = -10957;

const SECS_PER_DAY: i64 = 86_400;

/// A parse/range error, carrying the SQLSTATE and message PG reports.
#[derive(Clone, Debug, PartialEq)]
pub struct DateError {
    pub sqlstate: &'static str,
    pub message: String,
}

fn invalid_syntax(input: &str) -> DateError {
    DateError {
        sqlstate: INVALID_DATETIME_FORMAT,
        message: format!("invalid input syntax for type date: \"{input}\""),
    }
}

fn field_out_of_range(input: &str) -> DateError {
    DateError {
        sqlstate: DATETIME_FIELD_OVERFLOW,
        message: format!("date/time field value out of range: \"{input}\""),
    }
}

pub fn is_finite(d: i32) -> bool {
    d != NEG_INFINITY && d != POS_INFINITY
}

/// Compare two date values; the `i32` order already handles the ±infinity
/// sentinels (they are `i32::MIN`/`i32::MAX`).
pub fn cmp(a: i32, b: i32) -> std::cmp::Ordering {
    a.cmp(&b)
}

// --- output (date_out, ISO DateStyle) --------------------------------------

/// `date_out` at the default (ISO) DateStyle.
pub fn format(d: i32) -> String {
    if d == POS_INFINITY {
        return "infinity".to_string();
    }
    if d == NEG_INFINITY {
        return "-infinity".to_string();
    }
    let (year, month, day) = j2date(d as i64 + POSTGRES_EPOCH_JDATE);
    // Years <= 0 are BC: astronomical year 0 is 1 BC, -1 is 2 BC, ...
    let (year, bc) = if year <= 0 { (1 - year, true) } else { (year, false) };
    let body = format!("{year:04}-{month:02}-{day:02}");
    if bc { format!("{body} BC") } else { body }
}

// --- input (date_in, a practical subset) -----------------------------------

/// `date_in`. Accepts the ISO `Y-M-D` and verbose `Mon DD, YYYY` forms, `BC`,
/// and the `infinity`/`-infinity`/`epoch` specials; a trailing time and zone
/// are accepted and ignored (this type has no time-of-day). Unparseable input
/// is `22007`; a well-formed value out of range is `22008`.
pub fn parse(input: &str) -> Result<i32, DateError> {
    // Reuse the shared timestamp scanner, then keep only the date part. A comma
    // is a field separator in the verbose form (`January 8, 1999`), which PG
    // treats as whitespace. Its error messages name `timestamp`, so remap them.
    let cleaned = input.replace(',', " ");
    let parsed = timestamp::parse_parts(&cleaned).map_err(|e| {
        if e.sqlstate == DATETIME_FIELD_OVERFLOW {
            field_out_of_range(input)
        } else {
            invalid_syntax(input)
        }
    })?;
    let tm = match parsed {
        timestamp::Parsed::Micros(m) => {
            // The specials, remapped from the timestamp scale to date days.
            if m == timestamp::POS_INFINITY {
                return Ok(POS_INFINITY);
            }
            if m == timestamp::NEG_INFINITY {
                return Ok(NEG_INFINITY);
            }
            // The only other special is `epoch`.
            return Ok(UNIX_EPOCH_DATE);
        }
        timestamp::Parsed::Calendar { tm, .. } => tm,
    };
    if !(1..=12).contains(&tm.month) || tm.day < 1 || tm.day > days_in_month(tm.year, tm.month) {
        return Err(field_out_of_range(input));
    }
    let jd = date2j(tm.year, tm.month, tm.day) - POSTGRES_EPOCH_JDATE;
    to_date_value(jd).ok_or_else(|| field_out_of_range(input))
}

/// Range-check a Julian-relative day count and narrow it to the `i32` date value.
fn to_date_value(jd: i64) -> Option<i32> {
    if jd < DATE_MINVAL as i64 || jd > DATE_MAXVAL as i64 {
        return None;
    }
    Some(jd as i32)
}

// --- integer / interval arithmetic -----------------------------------------

fn out_of_range() -> DateError {
    DateError {
        sqlstate: DATETIME_FIELD_OVERFLOW,
        message: "date out of range".to_string(),
    }
}

/// `date + int4` / `date - int4` (via a negative `n`): shift by whole days. An
/// infinite date is unchanged.
pub fn add_days(d: i32, n: i32) -> Result<i32, DateError> {
    if !is_finite(d) {
        return Ok(d);
    }
    let sum = d as i64 + n as i64;
    to_date_value(sum).ok_or_else(out_of_range)
}

/// `date - int4` (`date_mii`): shift back by whole days. A separate entry point
/// (rather than `add_days(d, -n)`) so `n == i32::MIN` does not overflow on
/// negation; the subtraction is done in `i64`.
pub fn sub_days(d: i32, n: i32) -> Result<i32, DateError> {
    if !is_finite(d) {
        return Ok(d);
    }
    let diff = d as i64 - n as i64;
    to_date_value(diff).ok_or_else(out_of_range)
}

/// `date - date` (`date_mi`): the difference in days, as `int4`.
pub fn sub_date(a: i32, b: i32) -> Result<i32, DateError> {
    if !is_finite(a) || !is_finite(b) {
        return Err(DateError {
            sqlstate: DATETIME_FIELD_OVERFLOW,
            message: "cannot subtract infinite dates".to_string(),
        });
    }
    Ok(a - b)
}

/// `date field value out of range for timestamp` — a finite date whose midnight
/// falls outside the `timestamp` microsecond range (PG's `date2timestamp`).
fn out_of_range_for_timestamp() -> DateError {
    DateError {
        sqlstate: DATETIME_FIELD_OVERFLOW,
        message: "date out of range for timestamp".to_string(),
    }
}

const USECS_PER_DAY: i64 = SECS_PER_DAY * 1_000_000;

/// A finite date as microseconds since 2000-01-01 (midnight), or the matching
/// timestamp ±infinity sentinel. Errors when the date is representable as a
/// `date` but its midnight overflows the `timestamp` range (dates run to year
/// 5874897, far past `timestamp`'s 294276) — matching PG's `date out of range
/// for timestamp`.
pub fn to_timestamp_micros(d: i32) -> Result<i64, DateError> {
    if d == POS_INFINITY {
        return Ok(timestamp::POS_INFINITY);
    }
    if d == NEG_INFINITY {
        return Ok(timestamp::NEG_INFINITY);
    }
    (d as i64)
        .checked_mul(USECS_PER_DAY)
        .ok_or_else(out_of_range_for_timestamp)
}

/// The calendar date of a timestamp (`timestamp::to_date` direction): the
/// `date` cast of a `timestamp`/`timestamptz`, infinities preserved.
pub fn from_timestamp_micros(micros: i64) -> i32 {
    if micros == timestamp::POS_INFINITY {
        return POS_INFINITY;
    }
    if micros == timestamp::NEG_INFINITY {
        return NEG_INFINITY;
    }
    let tm = timestamp::decode(micros);
    (date2j(tm.year, tm.month, tm.day) - POSTGRES_EPOCH_JDATE) as i32
}

/// `date + interval` / `date - interval` → `timestamp`: widen the date to
/// midnight, then apply the timestamp/interval arithmetic.
pub fn pl_interval(d: i32, span: Interval) -> Result<i64, DateError> {
    timestamp::pl_interval(to_timestamp_micros(d)?, span).map_err(from_ts_err)
}

pub fn mi_interval(d: i32, span: Interval) -> Result<i64, DateError> {
    timestamp::mi_interval(to_timestamp_micros(d)?, span).map_err(from_ts_err)
}

/// `date + time` → `timestamp`: midnight of the date plus the time-of-day.
pub fn pl_time(d: i32, time_usec: i64) -> Result<i64, DateError> {
    let micros = to_timestamp_micros(d)?;
    if !is_finite(d) {
        return Ok(micros);
    }
    micros.checked_add(time_usec).ok_or_else(out_of_range)
}

/// `date + timetz` → `timestamptz`: the UTC instant of the date's midnight plus
/// the zoned time-of-day (`local usec + zone-west offset`). Matches PG's
/// `datetimetz_timestamptz`.
pub fn pl_timetz(d: i32, t: crate::TimeTz) -> Result<i64, DateError> {
    let micros = to_timestamp_micros(d)?;
    if !is_finite(d) {
        return Ok(micros);
    }
    // `t.zone` is seconds west of UTC, so adding it shifts the local time to UTC.
    micros
        .checked_add(t.usec)
        .and_then(|m| m.checked_add(t.zone as i64 * 1_000_000))
        .ok_or_else(out_of_range)
}

fn from_ts_err(e: timestamp::TimestampError) -> DateError {
    DateError { sqlstate: e.sqlstate, message: e.message }
}

// --- field extraction (date_part / extract) --------------------------------

fn not_supported(unit: &str) -> DateError {
    DateError {
        sqlstate: INVALID_PARAMETER_VALUE,
        message: format!("unit \"{unit}\" not supported for type date"),
    }
}

fn not_recognized(unit: &str) -> DateError {
    DateError {
        sqlstate: INVALID_PARAMETER_VALUE,
        message: format!("unit \"{unit}\" not recognized for type date"),
    }
}

/// Supported `date` fields, and whether each increases monotonically with the
/// date (so `±infinity` yields `±Infinity` rather than NULL).
enum Field {
    /// A supported field: `true` if monotonic.
    Ok(bool),
    /// A known time-of-day / zone field that `date` does not carry.
    NotSupported,
    /// An unknown unit spelling.
    Unknown,
}

/// Classify a (lowercased) unit spelling for `date`.
fn classify(unit: &str) -> Field {
    match unit {
        "year" | "decade" | "century" | "millennium" | "isoyear" | "julian" | "epoch" => {
            Field::Ok(true)
        }
        "month" | "day" | "quarter" | "week" | "dow" | "isodow" | "doy" => Field::Ok(false),
        // Aliases/plurals of the supported set.
        "years" => Field::Ok(true),
        "months" | "days" | "quarters" | "weeks" | "dows" | "decades" => Field::Ok(false),
        "centuries" | "millenniums" | "millenia" | "millenium" => Field::Ok(true),
        // Present on `timestamp`/`time`, but not on a bare `date`.
        "microseconds" | "microsecond" | "usec" | "usecs" | "milliseconds" | "millisecond"
        | "msec" | "msecs" | "second" | "seconds" | "minute" | "minutes" | "hour" | "hours"
        | "timezone" | "timezone_hour" | "timezone_h" | "timezone_minute" | "timezone_m" => {
            Field::NotSupported
        }
        _ => Field::Unknown,
    }
}

/// `date_part(unit, date) -> float8`. `Ok(None)` is SQL NULL (an oscillating
/// field on `±infinity`).
pub fn date_part(unit: &str, d: i32) -> Result<Option<f64>, DateError> {
    let lu = unit.trim().to_ascii_lowercase();
    match classify(&lu) {
        Field::NotSupported => Err(not_supported(&lu)),
        Field::Unknown => Err(not_recognized(&lu)),
        Field::Ok(monotonic) => {
            if !is_finite(d) {
                return Ok(if monotonic {
                    Some(if d == POS_INFINITY { f64::INFINITY } else { f64::NEG_INFINITY })
                } else {
                    None
                });
            }
            Ok(Some(field_value(&canon(&lu), d) as f64))
        }
    }
}

/// `extract(unit FROM date) -> numeric`. Same fields as [`date_part`]; every
/// supported `date` field is integer-valued.
pub fn extract(unit: &str, d: i32) -> Result<Option<Numeric>, DateError> {
    let lu = unit.trim().to_ascii_lowercase();
    match classify(&lu) {
        Field::NotSupported => Err(not_supported(&lu)),
        Field::Unknown => Err(not_recognized(&lu)),
        Field::Ok(monotonic) => {
            if !is_finite(d) {
                return Ok(if monotonic {
                    Some(if d == POS_INFINITY { Numeric::pos_inf() } else { Numeric::neg_inf() })
                } else {
                    None
                });
            }
            let v = field_value(&canon(&lu), d);
            Ok(Some(Numeric::parse(&v.to_string()).expect("integer literal is valid numeric")))
        }
    }
}

/// Fold plural/alias spellings to the canonical field name used by [`field_value`].
fn canon(unit: &str) -> String {
    match unit {
        "years" => "year",
        "months" => "month",
        "days" => "day",
        "quarters" => "quarter",
        "weeks" => "week",
        "dows" => "dow",
        "decades" => "decade",
        "centuries" => "century",
        "millenniums" | "millenia" | "millenium" => "millennium",
        other => other,
    }
    .to_string()
}

/// The integer value of a supported field for a finite date.
fn field_value(unit: &str, d: i32) -> i64 {
    let jd = d as i64 + POSTGRES_EPOCH_JDATE;
    let (year, month, day) = j2date(jd);
    match unit {
        // PG's calendar has no year 0, so a BC (astronomical <= 0) year reports
        // one less: astronomical -2019 (2020 BC) extracts as -2020.
        "year" => {
            if year <= 0 {
                year - 1
            } else {
                year
            }
        }
        "month" => month,
        "day" => day,
        "quarter" => (month - 1) / 3 + 1,
        "decade" => decade(year),
        "century" => century(year),
        "millennium" => millennium(year),
        "isoyear" => iso_week_year(jd).1,
        "week" => iso_week_year(jd).0,
        "dow" => j2day(jd),
        "isodow" => {
            let dd = j2day(jd);
            if dd == 0 { 7 } else { dd }
        }
        "doy" => jd - date2j(year, 1, 1) + 1,
        "julian" => jd,
        "epoch" => (d as i64 - UNIX_EPOCH_DATE as i64) * SECS_PER_DAY,
        _ => unreachable!("field_value called with an unsupported unit"),
    }
}

// --- make_date -------------------------------------------------------------

/// `make_date(year, month, mday)`. Year 0 is invalid; a negative year is BC.
/// Out-of-range fields raise `22008` with the `y-mm-dd` describing the input.
pub fn make_date(year: i64, month: i64, mday: i64) -> Result<i32, DateError> {
    let describe = || format!("date field value out of range: {year}-{month:02}-{mday:02}");
    let err = || DateError {
        sqlstate: DATETIME_FIELD_OVERFLOW,
        message: describe(),
    };
    if year == 0 {
        return Err(err());
    }
    // A negative (BC) year uses astronomical numbering internally: -1 == 1 BC.
    let astro_year = if year < 0 { year + 1 } else { year };
    if !(1..=12).contains(&month) || mday < 1 || mday > days_in_month(astro_year, month) {
        return Err(err());
    }
    let jd = date2j(astro_year, month, mday) - POSTGRES_EPOCH_JDATE;
    to_date_value(jd).ok_or_else(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> i32 {
        parse(s).unwrap()
    }

    #[test]
    fn parse_format_roundtrip() {
        for s in ["2020-08-11", "1999-01-08", "0001-01-01", "0044-03-15 BC"] {
            assert_eq!(format(d(s)), s);
        }
        assert_eq!(format(d("January 8, 1999")), "1999-01-08");
    }

    #[test]
    fn specials() {
        assert_eq!(format(d("infinity")), "infinity");
        assert_eq!(format(d("-infinity")), "-infinity");
        assert_eq!(format(d("epoch")), "1970-01-01");
        assert!(!is_finite(d("infinity")));
        assert!(is_finite(d("2020-01-01")));
    }

    #[test]
    fn arithmetic() {
        assert_eq!(sub_date(d("2020-01-02"), d("2020-01-01")).unwrap(), 1);
        assert_eq!(format(add_days(d("2020-01-01"), 31).unwrap()), "2020-02-01");
        assert_eq!(format(add_days(POS_INFINITY, 5).unwrap()), "infinity");
    }

    #[test]
    fn extract_fields() {
        assert_eq!(date_part("year", d("2020-08-11")).unwrap(), Some(2020.0));
        assert_eq!(date_part("doy", d("2020-08-11")).unwrap(), Some(224.0));
        assert_eq!(date_part("epoch", d("2020-08-11")).unwrap(), Some(1_597_104_000.0));
        assert_eq!(date_part("julian", d("2020-08-11")).unwrap(), Some(2_459_073.0));
        assert!(date_part("hour", d("2020-08-11")).is_err());
        assert!(date_part("bogus", d("2020-08-11")).is_err());
        assert_eq!(date_part("day", POS_INFINITY).unwrap(), None);
        assert_eq!(date_part("year", POS_INFINITY).unwrap(), Some(f64::INFINITY));
    }

    #[test]
    fn make_date_ok_and_err() {
        assert_eq!(format(make_date(2013, 7, 15).unwrap()), "2013-07-15");
        assert_eq!(format(make_date(-44, 3, 15).unwrap()), "0044-03-15 BC");
        assert!(make_date(0, 7, 15).is_err());
        assert!(make_date(2013, 2, 30).is_err());
    }
}
