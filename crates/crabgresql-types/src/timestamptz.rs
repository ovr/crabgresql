//! `timestamp with time zone`: parsing, output, the field functions, and the
//! zone conversions (`AT TIME ZONE` / `make_timestamptz`).
//!
//! Clean-room (see AGENTS.md): reproduces PostgreSQL's *observable* behavior,
//! pinned by differential tests. The value is stored exactly like `timestamp` —
//! `i64` microseconds since 2000-01-01, with `i64::MIN`/`MAX` as the
//! `-infinity`/`infinity` sentinels — but the instant is in **UTC**.
//!
//! Everything user-visible is relative to the session's display zone (the
//! `TimeZone` GUC, threaded in as a [`SessionZone`]). On input an explicit zone
//! token wins and a missing one means the session zone; on output the instant is
//! rotated into the session zone and the offset printed alongside. The field
//! functions and `date_trunc` likewise operate on the *local* wall clock — only
//! `epoch` and the ordering of the stored value are zone-independent. Zone
//! resolution and DST live in [`crate::tz`]; the calendar core is shared with
//! [`crate::timestamp`].

use crate::Numeric;
use crate::fmt::FmtCtx;
use crate::timestamp::{
    self, DATETIME_FIELD_OVERFLOW, INVALID_DATETIME_FORMAT, INVALID_PARAMETER_VALUE,
    INVALID_TIME_ZONE_DISPLACEMENT, NEG_INFINITY, POS_INFINITY, Parsed, TimestampError, decode,
    encode, format_parts, is_finite, validate_fields,
};
use crate::tz::{self, SessionZone, TmLite, ZoneError};

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

/// The valueless `timestamp out of range` error PG raises when an offset or a
/// constructor pushes an instant past the representable range (no quoted input).
fn out_of_range_bare() -> TimestampError {
    TimestampError {
        sqlstate: DATETIME_FIELD_OVERFLOW,
        message: "timestamp out of range".to_string(),
    }
}

/// `date/time field value out of range` — PG's error when a *field* (here the
/// year) overflows its `int` decoder, distinct from the value-range
/// `out_of_range`. The boundary is `i32` (verified against PG).
fn field_out_of_range(input: &str) -> TimestampError {
    TimestampError {
        sqlstate: DATETIME_FIELD_OVERFLOW,
        message: format!("date/time field value out of range: \"{input}\""),
    }
}

/// Whether a stored microsecond value is within PG's timestamp range,
/// `[4714-11-24 00:00:00 BC, 294276-12-31 23:59:59.999999]` (both types share it).
fn in_range(micros: i64) -> bool {
    micros >= min_micros() && micros < end_micros()
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

fn clock_unavailable(e: crate::fmt::ClockError) -> TimestampError {
    TimestampError {
        sqlstate: e.sqlstate,
        message: e.message,
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
    TmLite {
        year: tm.year,
        month: tm.month,
        day: tm.day,
        hour: tm.hour,
        min: tm.min,
        sec: tm.sec,
    }
}

/// `timestamptz_in`. Interprets any trailing zone token to convert the wall
/// clock to UTC; **a missing zone token means the session zone**, not UTC.
/// `infinity`/`epoch` pass through. Syntax errors are `22007`, out-of-range
/// results `22008`, an unknown zone `22023`, a bad numeric offset `22009`.
pub fn parse(input: &str, fmt: &FmtCtx) -> Result<i64, TimestampError> {
    let session = &fmt.zone;
    let parsed = timestamp::parse_parts(input, fmt).map_err(|e| relabel_syntax(e, input))?;
    let (tm, zone) = match parsed {
        // infinity/-infinity/epoch are zone-independent.
        Parsed::Micros(m) => return Ok(m),
        // `'now'` is already an instant. Taking it directly rather than
        // rendering it as a wall clock and reading that back is what keeps it
        // exact across a DST fold, where the round trip is ambiguous.
        Parsed::Now => return fmt.xact_start().map_err(clock_unavailable),
        Parsed::Calendar { tm, zone } => (tm, zone),
    };
    validate_fields(&tm, input)?;
    // Bound the year before `encode` (which multiplies by USECS_PER_DAY) so a
    // wildly out-of-range year cannot overflow i64. The astronomical span
    // `-4713..=294_276` covers PG's full range (4714 BC .. 294276 AD); the exact
    // sub-year boundary is enforced by the post-offset `in_range` check below.
    // PG distinguishes a year that overflows its `int` field decoder
    // ("date/time field value out of range") from one merely past the timestamp
    // range ("timestamp out of range"); the boundary is `i32`.
    if !(-4713..=294_276).contains(&tm.year) {
        if tm.year < i32::MIN as i64 || tm.year > i32::MAX as i64 {
            return Err(field_out_of_range(input));
        }
        return Err(out_of_range(input));
    }
    let civil = encode(tm);
    let wall = TmLite {
        year: tm.year,
        month: tm.month,
        day: tm.day,
        hour: tm.hour,
        min: tm.min,
        sec: tm.sec,
    };
    let off_secs = match zone {
        None => session.offset_for_wall(wall),
        Some(tok) => {
            let zone = tz::resolve_zone(&tok).map_err(zone_error)?;
            tz::offset_for_local(&zone, wall)
        }
    };
    let utc = civil - off_secs as i64 * USECS_PER_SEC;
    if !in_range(utc) {
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

/// `timestamptz_out`: the instant rendered as the wall clock the session zone
/// shows, followed by that zone's offset — `±HH`, widening to `±HH:MM` or
/// `±HH:MM:SS` (see [`tz::format_offset`]). The offset is spliced before any
/// ` BC` suffix, matching PG's `… 4714-04:56:02 BC` ordering.
///
/// Divergence: rotating an instant at the very edge of the representable range
/// into a non-UTC zone would leave it, and an output function has no way to
/// raise. We saturate and render the clamped wall clock rather than panicking;
/// PG applies the offset in a wider internal type. Only the two synthetic
/// boundary values can reach this.
pub fn format(micros: i64, session: &SessionZone) -> String {
    if micros == POS_INFINITY {
        return "infinity".to_string();
    }
    if micros == NEG_INFINITY {
        return "-infinity".to_string();
    }
    let off_secs = session.offset_at(micros);
    let local = micros.saturating_add(off_secs as i64 * USECS_PER_SEC);
    let (body, bc) = format_parts(local);
    let offset = tz::format_offset(off_secs);
    if bc {
        format!("{body}{offset} BC")
    } else {
        format!("{body}{offset}")
    }
}

/// The local wall clock this instant shows in `session`, as a stored
/// `timestamp` value. Saturating for the same reason [`format`] is.
fn to_wall_clock(micros: i64, session: &SessionZone) -> i64 {
    micros.saturating_add(session.offset_at(micros) as i64 * USECS_PER_SEC)
}

// --- field functions -------------------------------------------------------
//
// Three groups. The `timezone*` fields report the session zone's offset. `epoch`
// is the instant itself and so zone-independent. Everything else — `year`,
// `day`, `hour`, `dow`, `week`, … — is a property of the *local* wall clock, so
// the instant is rotated into the session zone before deferring to `timestamp`.
// (Under a UTC session zone the rotation is a no-op, which is why deferring
// unconditionally used to look correct.)

/// The offset fields, in the spellings PG's `timestamptz` accepts.
fn tz_field(unit: &str) -> Option<TzField> {
    match unit.trim().to_ascii_lowercase().as_str() {
        "timezone" => Some(TzField::Seconds),
        "timezone_hour" => Some(TzField::Hour),
        "timezone_minute" => Some(TzField::Minute),
        _ => None,
    }
}

enum TzField {
    Seconds,
    Hour,
    Minute,
}

/// The value of an offset field. Both the hour and the minute carry the
/// offset's sign, as in PG: `America/St_Johns` reports `-3` and `-30`.
fn tz_field_value(field: TzField, off_secs: i32) -> i64 {
    match field {
        TzField::Seconds => off_secs as i64,
        TzField::Hour => (off_secs / 3600) as i64,
        TzField::Minute => ((off_secs % 3600) / 60) as i64,
    }
}

/// Whether a unit names the instant rather than the wall clock, and so must not
/// be rotated into the session zone.
fn is_epoch_unit(unit: &str) -> bool {
    unit.trim().eq_ignore_ascii_case("epoch")
}

/// `date_part(text, timestamptz) -> float8`. `Ok(None)` is SQL NULL.
pub fn date_part(
    unit: &str,
    micros: i64,
    session: &SessionZone,
) -> Result<Option<f64>, TimestampError> {
    if let Some(field) = tz_field(unit) {
        // On ±infinity an offset field is NULL (it oscillates).
        if !is_finite(micros) {
            return Ok(None);
        }
        let value = tz_field_value(field, session.offset_at(micros));
        return Ok(Some(value as f64));
    }
    timestamp::date_part(unit, local_for_field(unit, micros, session)).map_err(relabel_units)
}

/// `EXTRACT(field FROM timestamptz) -> numeric`. `Ok(None)` is SQL NULL.
pub fn extract(
    unit: &str,
    micros: i64,
    session: &SessionZone,
) -> Result<Option<Numeric>, TimestampError> {
    if let Some(field) = tz_field(unit) {
        if !is_finite(micros) {
            return Ok(None);
        }
        let value = tz_field_value(field, session.offset_at(micros));
        return Ok(Some(Numeric::from_i128(value as i128)));
    }
    timestamp::extract(unit, local_for_field(unit, micros, session)).map_err(relabel_units)
}

/// The value a non-offset field is read from: the local wall clock, except for
/// `epoch` (the instant) and the infinities (which have no wall clock).
fn local_for_field(unit: &str, micros: i64, session: &SessionZone) -> i64 {
    if is_epoch_unit(unit) || !is_finite(micros) {
        micros
    } else {
        to_wall_clock(micros, session)
    }
}

/// `date_trunc(text, timestamptz) -> timestamptz`: truncate the **local** wall
/// clock, then convert back.
///
/// Which offset converts back depends on the unit, matching PG's `redotz` flag:
///
/// * `day` and coarser **re-resolve** the offset from the truncated wall clock,
///   which is what makes `date_trunc('day', …)` land on real local midnight on
///   a DST-transition day — on 2024-03-10 in `America/New_York` the input is at
///   `-04` but midnight that morning is at `-05`, and PG returns
///   `2024-03-10 00:00:00-05`.
/// * `hour` and finer **reuse the input's** offset. Re-resolving them would be
///   wrong inside a fall-back fold, where the truncated wall clock is ambiguous:
///   `date_trunc('hour', '2024-11-03 01:30:00-04')` must stay at `-04`, and
///   re-resolving picks the after-transition `-05`, moving the result an hour
///   *later* than the value it truncated.
pub fn date_trunc(
    unit: &str,
    micros: i64,
    session: &SessionZone,
) -> Result<i64, TimestampError> {
    // The unit is validated even on the infinities, where there is no wall clock
    // to truncate — `timestamp::date_trunc` checks it before its own finiteness
    // short-circuit, and the two types must agree.
    let offset = session.offset_at(micros);
    let truncated = timestamp::date_trunc(unit, to_wall_clock(micros, session))
        .map_err(relabel_units)?;
    if !is_finite(micros) {
        return Ok(micros);
    }
    let offset = if redo_zone(unit) {
        session.offset_for_wall(tmlite(truncated))
    } else {
        offset
    };
    let utc = truncated - offset as i64 * USECS_PER_SEC;
    if !in_range(utc) {
        return Err(out_of_range_bare());
    }
    Ok(utc)
}

/// Whether truncating to `unit` re-resolves the zone offset from the truncated
/// wall clock (PG's `redotz`): true for `day` and coarser, false for the
/// sub-day units, which keep the input's offset.
fn redo_zone(unit: &str) -> bool {
    !matches!(
        unit.trim().to_ascii_lowercase().as_str(),
        "microseconds" | "milliseconds" | "second" | "minute" | "hour"
    )
}

/// `isfinite(timestamptz) -> bool`.
pub fn is_finite_tstz(micros: i64) -> bool {
    is_finite(micros)
}

/// `make_timestamptz(year, month, mday, hour, min, sec[, zone])`. Without an
/// explicit zone the fields are read in the session zone.
pub fn make_timestamptz(
    year: i64,
    month: i64,
    mday: i64,
    hour: i64,
    min: i64,
    sec: f64,
    zone: Option<&str>,
    session: &SessionZone,
) -> Result<i64, TimestampError> {
    let civil = timestamp::make_timestamp(year, month, mday, hour, min, sec)?;
    let off_secs = match zone {
        None => session.offset_for_wall(tmlite(civil)),
        Some(tok) => {
            let zone = tz::resolve_zone(tok).map_err(zone_error)?;
            tz::offset_for_local(&zone, tmlite(civil))
        }
    };
    let utc = civil - off_secs as i64 * USECS_PER_SEC;
    // Applying the zone offset can push the instant past the representable
    // range even though the civil value was in range (PG errors here).
    if !in_range(utc) {
        return Err(out_of_range_bare());
    }
    Ok(utc)
}

/// `to_timestamp(double precision)`: seconds since the Unix epoch. Infinities
/// pass through to the sentinels; `NaN` is an error, as is a value outside the
/// representable range.
pub fn from_unix_epoch(seconds: f64) -> Result<i64, TimestampError> {
    if seconds.is_nan() {
        return Err(TimestampError {
            sqlstate: DATETIME_FIELD_OVERFLOW,
            message: "timestamp cannot be NaN".to_string(),
        });
    }
    if seconds.is_infinite() {
        return Ok(if seconds > 0.0 {
            POS_INFINITY
        } else {
            NEG_INFINITY
        });
    }
    let out_of_range = || TimestampError {
        sqlstate: DATETIME_FIELD_OVERFLOW,
        // PG builds this message with `%g`, i.e. six significant digits and a
        // two-digit exponent — not `float8out`, which would print all fifteen.
        message: format!("timestamp out of range: \"{}\"", format_g(seconds)),
    };
    // Shift to the PG epoch first, so the `i64` conversion sees the stored
    // value; `rint` semantics (ties to even) match PG's rounding of the
    // fractional second.
    let micros = ((seconds - EPOCH_SECS) * USECS_PER_SEC as f64).round_ties_even();
    if !micros.is_finite() || micros < i64::MIN as f64 || micros > i64::MAX as f64 {
        return Err(out_of_range());
    }
    let micros = micros as i64;
    if !in_range(micros) {
        return Err(out_of_range());
    }
    Ok(micros)
}

/// Seconds from the Unix epoch (1970-01-01) to the PG epoch (2000-01-01).
const EPOCH_SECS: f64 = 946_684_800.0;

/// C's `%g`: six significant digits, trailing zeros trimmed, switching to
/// exponent form when the exponent is below -4 or at least the precision.
fn format_g(v: f64) -> String {
    const SIG: usize = 6;
    if v == 0.0 {
        return "0".to_string();
    }
    let exp = v.abs().log10().floor() as i32;
    if exp < -4 || exp >= SIG as i32 {
        let mantissa = format!("{:.*}", SIG - 1, v / 10f64.powi(exp));
        let mantissa = trim_zeros(&mantissa);
        format!(
            "{mantissa}e{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        )
    } else {
        let decimals = (SIG as i32 - 1 - exp).max(0) as usize;
        trim_zeros(&format!("{v:.decimals$}")).to_string()
    }
}

fn trim_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

/// `timestamptz AT TIME ZONE zone` (= `timezone(zone, timestamptz)`): the wall
/// clock the instant shows in `zone`, as a zone-less `timestamp`. `±infinity`
/// passes through.
pub fn at_zone_to_timestamp(zone: &str, micros: i64) -> Result<i64, TimestampError> {
    let zone = tz::resolve_zone(zone).map_err(zone_error)?;
    instant_to_wall(micros, &zone)
}

/// `timestamptz AT TIME ZONE INTERVAL '…'`: the same, with the zone given as a
/// fixed displacement (seconds east) rather than a name.
pub fn at_offset_to_timestamp(off_east: i32, micros: i64) -> Result<i64, TimestampError> {
    instant_to_wall(micros, &tz::Zone::Fixed(off_east))
}

/// The `timestamptz → timestamp` cast: the wall clock the instant shows in the
/// **session** zone. Same operation as `AT TIME ZONE <session zone>`.
pub fn session_zone_wall_clock(
    micros: i64,
    session: &SessionZone,
) -> Result<i64, TimestampError> {
    instant_to_wall(micros, session.zone())
}

/// Shared core: rotate a UTC instant into `zone`'s wall clock. `±infinity`
/// passes through; a result outside the timestamp range is `22008`, as in PG.
fn instant_to_wall(micros: i64, zone: &tz::Zone) -> Result<i64, TimestampError> {
    if !is_finite(micros) {
        return Ok(micros);
    }
    let off_secs = tz::offset_for_instant(zone, micros);
    let wall = micros + off_secs as i64 * USECS_PER_SEC;
    if !in_range(wall) {
        return Err(out_of_range_bare());
    }
    Ok(wall)
}

/// `timestamp AT TIME ZONE zone` (= `timezone(zone, timestamp)`): interpret the
/// zone-less wall clock as being in `zone`, yielding the UTC `timestamptz`
/// instant. `±infinity` passes through.
pub fn timestamp_at_zone(zone: &str, micros: i64) -> Result<i64, TimestampError> {
    let zone = tz::resolve_zone(zone).map_err(zone_error)?;
    wall_to_instant(micros, &zone)
}

/// `timestamp AT TIME ZONE INTERVAL '…'`: the same, with the zone given as a
/// fixed displacement (seconds east) rather than a name.
pub fn timestamp_at_offset(off_east: i32, micros: i64) -> Result<i64, TimestampError> {
    wall_to_instant(micros, &tz::Zone::Fixed(off_east))
}

/// An `INTERVAL` used in a zone position, as seconds east of UTC. PG rejects one
/// carrying months or days — those are not a fixed displacement — quoting the
/// interval as it renders.
pub fn interval_zone_offset(iv: crate::interval::Interval) -> Result<i32, TimestampError> {
    if iv.months != 0 || iv.days != 0 {
        return Err(TimestampError {
            sqlstate: INVALID_PARAMETER_VALUE,
            message: format!(
                "interval time zone \"{}\" must not include months or days",
                crate::interval::format(iv)
            ),
        });
    }
    Ok((iv.usec / USECS_PER_SEC) as i32)
}

/// The `timestamp → timestamptz` cast: read the zone-less wall clock in the
/// **session** zone. Same operation as `AT TIME ZONE <session zone>`.
pub fn timestamp_at_session_zone(
    micros: i64,
    session: &SessionZone,
) -> Result<i64, TimestampError> {
    wall_to_instant(micros, session.zone())
}

/// Shared core: read a wall clock as being in `zone`, yielding the UTC instant.
fn wall_to_instant(micros: i64, zone: &tz::Zone) -> Result<i64, TimestampError> {
    if !is_finite(micros) {
        return Ok(micros);
    }
    let off_secs = tz::offset_for_local(zone, tmlite(micros));
    let utc = micros - off_secs as i64 * USECS_PER_SEC;
    if !in_range(utc) {
        return Err(out_of_range_bare());
    }
    Ok(utc)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The zone every pre-existing case in this module is written against.
    fn utc() -> SessionZone {
        SessionZone::utc()
    }

    /// An input context in `z` with no clock — every case here spells out its
    /// own instant, so a relative special would be a test bug.
    fn in_zone(z: SessionZone) -> FmtCtx {
        FmtCtx::utc_default().with_zone(std::sync::Arc::new(z))
    }

    fn p(s: &str) -> i64 {
        match parse(s, &in_zone(utc())) {
            Ok(value) => value,
            Err(error) => panic!("invalid timestamptz test fixture `{s}`: {error:?}"),
        }
    }

    #[test]
    fn offset_normalizes_to_utc() {
        // -08:00 shifts the wall clock forward 8h to UTC.
        assert_eq!(
            format(p("1997-02-10 17:32:01-08"), &utc()),
            "1997-02-11 01:32:01+00"
        );
        assert_eq!(
            format(p("1997-02-10 17:32:01-0800"), &utc()),
            "1997-02-11 01:32:01+00"
        );
        assert_eq!(
            format(p("1997-02-10 17:32:01 -08:00"), &utc()),
            "1997-02-11 01:32:01+00"
        );
        // No zone token -> already UTC.
        assert_eq!(format(p("2001-02-16 20:38:40"), &utc()), "2001-02-16 20:38:40+00");
        assert_eq!(
            format(p("2001-02-16 20:38:40+00"), &utc()),
            "2001-02-16 20:38:40+00"
        );
    }

    #[test]
    fn named_zone_and_abbrev_input() {
        // America/New_York in February is EST (-05:00).
        assert_eq!(
            format(p("1997-02-10 17:32:01 America/New_York"), &utc()),
            "1997-02-10 22:32:01+00"
        );
        // PST is a fixed -08:00.
        assert_eq!(
            format(p("1997-02-10 17:32:01 PST"), &utc()),
            "1997-02-11 01:32:01+00"
        );
        // UTC / Z synonyms.
        assert_eq!(
            format(p("1997-02-10 17:32:01 UTC"), &utc()),
            "1997-02-10 17:32:01+00"
        );
        assert_eq!(format(p("2001-09-22T18:19:20Z"), &utc()), "2001-09-22 18:19:20+00");
    }

    #[test]
    fn specials_and_fractions() {
        assert_eq!(format(p("infinity"), &utc()), "infinity");
        assert_eq!(format(p("-infinity"), &utc()), "-infinity");
        assert_eq!(format(p("epoch"), &utc()), "1970-01-01 00:00:00+00");
        assert_eq!(
            format(p("2001-02-16 20:38:40.5+00"), &utc()),
            "2001-02-16 20:38:40.5+00"
        );
    }

    #[test]
    fn bc_and_boundaries() {
        assert_eq!(
            format(p("0097-02-16 20:00:00+00 BC"), &utc()),
            "0097-02-16 20:00:00+00 BC"
        );
        // Lower boundary: 4714-11-24 00:00:00 UTC BC is valid.
        assert!(parse("4714-11-24 00:00:00+00 BC", &in_zone(utc())).is_ok());
        assert!(parse("4714-11-23 16:00:00-08 BC", &in_zone(utc())).is_ok()); // == the same instant
        // One second earlier is out of range.
        let e = parse("4714-11-23 23:59:59+00 BC", &in_zone(utc())).unwrap_err();
        assert_eq!(e.sqlstate, DATETIME_FIELD_OVERFLOW);
        // Upper boundary.
        assert!(parse("294276-12-31 23:59:59+00", &in_zone(utc())).is_ok());
        assert_eq!(
            parse("294277-01-01 00:00:00+00", &in_zone(utc()))
                .unwrap_err()
                .sqlstate,
            DATETIME_FIELD_OVERFLOW
        );
    }

    #[test]
    fn errors() {
        let e = parse("garbage", &in_zone(utc())).unwrap_err();
        assert_eq!(e.sqlstate, INVALID_DATETIME_FORMAT);
        assert_eq!(
            e.message,
            "invalid input syntax for type timestamp with time zone: \"garbage\""
        );
        assert_eq!(
            parse("2001-01-01 00:00 Nowhere/Nozone", &in_zone(utc()))
                .unwrap_err()
                .sqlstate,
            INVALID_PARAMETER_VALUE
        );
    }

    #[test]
    fn at_time_zone_round_trip() -> anyhow::Result<()> {
        // A UTC instant shown in New York (EST -5h) reads 5h earlier.
        let instant = p("2001-02-16 20:38:40+00");
        let wall = at_zone_to_timestamp("America/New_York", instant)?;
        assert_eq!(timestamp::format(wall), "2001-02-16 15:38:40");
        // Interpreting that wall clock back in New York returns the UTC instant.
        let back = timestamp_at_zone("America/New_York", wall)?;
        assert_eq!(back, instant);

        Ok(())
    }

    #[test]
    fn make_and_fields() -> anyhow::Result<()> {
        // 6-arg is UTC.
        assert_eq!(
            format(make_timestamptz(2013, 7, 15, 8, 15, 23.5, None, &utc())?, &utc()),
            "2013-07-15 08:15:23.5+00"
        );
        // 7-arg with a summer EDT zone (-04:00) shifts +4h to UTC.
        assert_eq!(
            format(
                make_timestamptz(2013, 7, 15, 17, 15, 23.0, Some("America/New_York"), &utc())?,
                &utc()
            ),
            "2013-07-15 21:15:23+00"
        );
        // timezone* fields are 0 under the UTC session zone.
        let v = p("2001-02-16 20:38:40+00");
        assert_eq!(date_part("timezone", v, &utc())?, Some(0.0));
        assert_eq!(date_part("timezone_hour", v, &utc())?, Some(0.0));
        assert_eq!(date_part("hour", v, &utc())?, Some(20.0));

        Ok(())
    }

    // A wildly out-of-range year must be rejected with 22008 *before* `encode`,
    // which would otherwise overflow i64 (`date * USECS_PER_DAY`) and panic in
    // debug / wrap in release. Regression for the missing pre-encode year guard.
    #[test]
    fn huge_year_does_not_overflow() {
        // In i32 field range but past the timestamp range: "timestamp out of range".
        for input in ["999999-01-01", "300000-01-01 00:00:00+00"] {
            let e = parse(input, &in_zone(utc())).expect_err(input);
            assert_eq!(e.sqlstate, DATETIME_FIELD_OVERFLOW, "{input}");
            assert_eq!(
                e.message,
                format!("timestamp out of range: \"{input}\""),
                "{input}"
            );
        }
        // Beyond the i32 field range: "date/time field value out of range"
        // (must not overflow i64 in `encode`). Both signs.
        for input in ["5000000000-01-01", "5000000000-01-01 BC"] {
            let e = parse(input, &in_zone(utc())).expect_err(input);
            assert_eq!(e.sqlstate, DATETIME_FIELD_OVERFLOW, "{input}");
            assert_eq!(
                e.message,
                format!("date/time field value out of range: \"{input}\""),
                "{input}"
            );
        }
    }

    // A huge colon-form offset hour must not overflow the offset arithmetic
    // (`h * 3600`); it is rejected as a displacement out of range (22009).
    // Regression for the i32 multiply overflow in `tz::parse_fixed`.
    #[test]
    fn huge_offset_does_not_overflow() {
        for input in [
            "2001-01-01 12:00:00 +600000:00",
            "2001-01-01 12:00:00 -600000:00",
            "2001-01-01 12:00:00 +99:00",
        ] {
            let e = parse(input, &in_zone(utc())).expect_err(input);
            assert_eq!(
                e.sqlstate,
                crate::timestamp::INVALID_TIME_ZONE_DISPLACEMENT,
                "{input}"
            );
        }
    }

    // Applying an offset can push an in-range civil value past the boundary;
    // that must error rather than return a silent out-of-band value.
    #[test]
    fn offset_overflow_is_rejected() -> anyhow::Result<()> {
        // make_timestamptz: civil is in range, but the -10h zone pushes it past
        // the upper boundary.
        assert_eq!(
            make_timestamptz(294276, 12, 31, 23, 0, 0.0, Some("-10"), &utc())
                .unwrap_err()
                .sqlstate,
            DATETIME_FIELD_OVERFLOW
        );
        // AT TIME ZONE past the upper boundary (timestamp -> timestamptz).
        let near_max = timestamp::parse("294276-12-31 23:59:59", &in_zone(utc()))?;
        assert_eq!(
            timestamp_at_zone("America/New_York", near_max)
                .unwrap_err()
                .sqlstate,
            DATETIME_FIELD_OVERFLOW
        );

        Ok(())
    }

    // A date with a glued `+` zone is only accepted when the remainder is a
    // numeric offset; `+garbage` is a syntax error (matches PG), not a value.
    #[test]
    fn glued_date_zone_requires_numeric_offset() {
        assert_eq!(format(p("2001-02-16+00"), &utc()), "2001-02-16 00:00:00+00");
        let e = parse("2001-02-16+garbage", &in_zone(utc())).unwrap_err();
        assert_eq!(e.sqlstate, INVALID_DATETIME_FORMAT);
    }

    // --- session display zone ---------------------------------------------
    //
    // Every expectation below is pinned against PostgreSQL 18.4.

    fn zone(name: &str) -> SessionZone {
        SessionZone::resolve(name).expect("test fixture names a real zone")
    }

    /// Parse in `z`, then render in `z`.
    fn round(s: &str, z: &SessionZone) -> String {
        match parse(s, &in_zone(z.clone())) {
            Ok(v) => format(v, z),
            Err(e) => panic!("invalid timestamptz test fixture `{s}`: {e:?}"),
        }
    }

    #[test]
    fn zone_less_input_is_read_in_the_session_zone() {
        let ny = zone("America/New_York");
        // The same text is a different instant in a different zone, and comes
        // back with that zone's offset rather than `+00`.
        assert_eq!(round("2024-06-01 12:00:00", &ny), "2024-06-01 12:00:00-04");
        assert_eq!(round("2024-01-15 12:00:00", &ny), "2024-01-15 12:00:00-05");
        // An explicit zone token still wins; only the rendering follows the
        // session.
        assert_eq!(
            round("2024-06-01 12:00:00+00", &ny),
            "2024-06-01 08:00:00-04"
        );
    }

    #[test]
    fn output_offset_widens_for_sub_hour_zones() {
        assert_eq!(
            round("2024-06-01 12:00:00", &zone("Asia/Kolkata")),
            "2024-06-01 12:00:00+05:30"
        );
        assert_eq!(
            round("2024-06-01 12:00:00", &zone("Pacific/Chatham")),
            "2024-06-01 12:00:00+12:45"
        );
        // Pre-1883 New York ran on local mean time, which PG prints to the
        // second.
        assert_eq!(
            round("1875-06-01 12:00:00", &zone("America/New_York")),
            "1875-06-01 12:00:00-04:56:02"
        );
    }

    #[test]
    fn fields_read_the_local_wall_clock() -> anyhow::Result<()> {
        let ny = zone("America/New_York");
        // 02:00 UTC on Jan 1 is still December 31st in New York.
        let v = parse("2024-01-01 02:00:00+00", &in_zone(ny.clone()))?;
        assert_eq!(date_part("day", v, &ny)?, Some(31.0));
        assert_eq!(date_part("month", v, &ny)?, Some(12.0));
        assert_eq!(date_part("year", v, &ny)?, Some(2023.0));
        // `epoch` names the instant, not the wall clock, so the zone cannot
        // move it.
        assert_eq!(date_part("epoch", v, &ny)?, Some(1_704_074_400.0));
        assert_eq!(date_part("epoch", v, &utc())?, Some(1_704_074_400.0));
        Ok(())
    }

    #[test]
    fn timezone_fields_report_the_session_offset() -> anyhow::Result<()> {
        let ny = zone("America/New_York");
        let v = parse("2024-01-15 12:00:00-05", &in_zone(ny.clone()))?;
        assert_eq!(date_part("timezone", v, &ny)?, Some(-18000.0));
        assert_eq!(date_part("timezone_hour", v, &ny)?, Some(-5.0));
        assert_eq!(date_part("timezone_minute", v, &ny)?, Some(0.0));
        assert_eq!(
            extract("timezone", v, &ny)?,
            Some(Numeric::from_i128(-18000))
        );

        // A half-hour zone west of UTC: the sign is carried on *both* fields.
        let stj = zone("America/St_Johns");
        let v = parse("2024-01-15 12:00:00-03:30", &in_zone(stj.clone()))?;
        assert_eq!(date_part("timezone_hour", v, &stj)?, Some(-3.0));
        assert_eq!(date_part("timezone_minute", v, &stj)?, Some(-30.0));

        // Still NULL on the infinities, which have no offset.
        assert_eq!(date_part("timezone", POS_INFINITY, &ny)?, None);
        Ok(())
    }

    /// `date_trunc` must re-resolve the offset from the *truncated* wall clock,
    /// not reuse the input's. On a spring-forward day the input is at `-04` but
    /// local midnight that morning was still at `-05`.
    #[test]
    fn date_trunc_lands_on_local_midnight_across_dst() -> anyhow::Result<()> {
        let ny = zone("America/New_York");
        let v = parse("2024-03-10 15:00:00-04", &in_zone(ny.clone()))?;
        assert_eq!(
            format(date_trunc("day", v, &ny)?, &ny),
            "2024-03-10 00:00:00-05"
        );
        assert_eq!(
            format(date_trunc("month", v, &ny)?, &ny),
            "2024-03-01 00:00:00-05"
        );
        Ok(())
    }

    /// The sub-day units keep the *input's* offset. Re-resolving them from the
    /// truncated wall clock is wrong inside a fall-back fold, where that clock
    /// is ambiguous — it would move the result an hour later than the value it
    /// truncated. Pinned against PG 18.4.
    #[test]
    fn date_trunc_sub_day_units_keep_the_input_offset() -> anyhow::Result<()> {
        let ny = zone("America/New_York");
        // 01:30-04 is the *first* pass through 01:30 on fall-back day.
        let v = parse("2024-11-03 01:30:00-04", &in_zone(ny.clone()))?;
        assert_eq!(
            format(date_trunc("hour", v, &ny)?, &ny),
            "2024-11-03 01:00:00-04"
        );
        assert_eq!(
            format(date_trunc("minute", v, &ny)?, &ny),
            "2024-11-03 01:30:00-04"
        );
        // `day` and coarser still re-resolve, which is what lands them on local
        // midnight.
        assert_eq!(
            format(date_trunc("day", v, &ny)?, &ny),
            "2024-11-03 00:00:00-04"
        );
        Ok(())
    }

    /// The unit is validated even on the infinities, where there is no wall
    /// clock — `timestamp` does the same, and the two types must agree.
    #[test]
    fn date_trunc_validates_the_unit_on_infinity() {
        let ny = zone("America/New_York");
        let e = date_trunc("bogus", POS_INFINITY, &ny).expect_err("unknown unit");
        assert_eq!(e.sqlstate, INVALID_PARAMETER_VALUE);
        assert_eq!(
            date_trunc("day", POS_INFINITY, &ny).expect("infinity truncates"),
            POS_INFINITY
        );
    }

    #[test]
    fn make_timestamptz_without_a_zone_uses_the_session() -> anyhow::Result<()> {
        let ny = zone("America/New_York");
        assert_eq!(
            format(make_timestamptz(2024, 6, 1, 12, 0, 0.0, None, &ny)?, &ny),
            "2024-06-01 12:00:00-04"
        );
        Ok(())
    }

    #[test]
    fn session_zone_conversions_are_not_an_identity() -> anyhow::Result<()> {
        let ny = zone("America/New_York");
        // timestamp -> timestamptz reads the wall clock in the session zone.
        let wall = timestamp::parse("2024-06-01 12:00:00", &in_zone(utc()))
            .map_err(|e| anyhow::anyhow!(e.message))?;
        assert_eq!(
            format(timestamp_at_session_zone(wall, &ny)?, &ny),
            "2024-06-01 12:00:00-04"
        );
        // ... and back the other way.
        let instant = parse("2024-06-01 12:00:00+00", &in_zone(ny.clone()))?;
        assert_eq!(
            timestamp::format(session_zone_wall_clock(instant, &ny)?),
            "2024-06-01 08:00:00"
        );
        // Both are an identity under UTC, which is why this used to look right.
        assert_eq!(timestamp_at_session_zone(wall, &utc())?, wall);
        assert_eq!(session_zone_wall_clock(wall, &utc())?, wall);
        // The infinities are zone-independent.
        assert_eq!(timestamp_at_session_zone(POS_INFINITY, &ny)?, POS_INFINITY);
        assert_eq!(session_zone_wall_clock(NEG_INFINITY, &ny)?, NEG_INFINITY);
        Ok(())
    }

    // --- the relative input specials (pinned against PostgreSQL 18.4) ------

    fn at(zone: &str) -> FmtCtx {
        FmtCtx::utc_at(1, 763_860_600_123_456, 763_860_600_123_456)
            .with_zone(std::sync::Arc::new(zone_of(zone)))
    }

    fn zone_of(name: &str) -> SessionZone {
        SessionZone::resolve(name).expect("test fixture names a real zone")
    }

    /// Parse `input` against the frozen clock in `zone`, then render it there.
    fn rel(input: &str, zone: &str) -> String {
        match parse(input, &at(zone)) {
            Ok(v) => format(v, &zone_of(zone)),
            Err(e) => panic!("{input:?} in {zone}: {e:?}"),
        }
    }

    /// The instant `input` parses to, unrendered.
    fn rel_micros(input: &str, zone: &str) -> i64 {
        match parse(input, &at(zone)) {
            Ok(v) => v,
            Err(e) => panic!("{input:?} in {zone}: {e:?}"),
        }
    }

    /// `'now'` is the transaction timestamp itself. It is taken as an instant,
    /// never rendered to a wall clock and read back — so it round-trips to the
    /// exact microsecond in every zone, including one mid-DST.
    #[test]
    fn now_is_the_transaction_instant_exactly() {
        for zone in [
            "UTC",
            "America/New_York",
            "Asia/Kolkata",
            "America/Santiago",
        ] {
            assert_eq!(
                rel_micros("now", zone),
                763_860_600_123_456,
                "now in {zone}"
            );
        }
    }

    /// `'today'` is local midnight — the *session's* midnight, turned back into
    /// an instant. At the frozen moment Kolkata is already on the 16th, and its
    /// midnight is 18.5 hours before UTC's.
    #[test]
    fn today_is_local_midnight_as_an_instant() {
        assert_eq!(rel("today", "UTC"), "2024-03-15 00:00:00+00");
        assert_eq!(rel("today", "America/New_York"), "2024-03-15 00:00:00-04");
        assert_eq!(rel("today", "Asia/Kolkata"), "2024-03-16 00:00:00+05:30");
        // A zone token on a relative date overrides the session's, so this is
        // midnight EST of the session's today.
        assert_eq!(rel("today EST", "UTC"), "2024-03-15 05:00:00+00");
    }
}
