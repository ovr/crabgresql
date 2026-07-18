//! Time-zone offset resolution for `timestamp with time zone`.
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior — which UTC offset a zone or abbreviation resolves to at a given
//! instant, including its DST gap/fold rule — pinned by differential tests
//! against real PG. The IANA zone data and DST arithmetic come from the `jiff`
//! crate (bundled tz database); this module is the sole place that names `jiff`.
//! Everything else in the crate speaks only in our own broken-down [`TmLite`]
//! and microsecond values.
//!
//! PG's `DetermineTimeZoneOffset` rule for an ambiguous local time: a **gap**
//! (a nonexistent wall-clock time, spring-forward) uses the offset in effect
//! *before* the transition; a **fold** (an ambiguous wall-clock time,
//! fall-back) uses the offset *after* the transition. `jiff` hands us both
//! bracketing offsets, so this is a direct match.

use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::{AmbiguousOffset, TimeZone};

/// Microseconds from the Unix epoch (1970-01-01) to the PostgreSQL epoch
/// (2000-01-01): 10957 days.
const PG_EPOCH_UNIX_MICROS: i64 = 946_684_800_000_000;

/// PG's timezone-displacement limit: magnitudes of `±15:59:59` are accepted,
/// `±16:00:00` and beyond are "out of range" (matches `make_timestamptz(..,'+16')`).
const MAX_TZ_DISPLACEMENT_SECS: i32 = 16 * 3600 - 1;

/// A plain broken-down civil time crossing the module boundary — no `jiff`
/// types leak out. Fields are already range-validated by the caller.
#[derive(Clone, Copy, Debug)]
pub struct TmLite {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub min: i64,
    pub sec: i64,
}

/// A resolved zone: either a constant offset handled by our own arithmetic
/// (full timestamp range, no `jiff` involvement), or an IANA zone resolved
/// through `jiff`.
pub enum Zone {
    /// Seconds east of UTC. `utc = civil - offset`.
    Fixed(i32),
    Named(TimeZone),
}

/// Why a zone token could not be resolved. The caller maps these to PG's
/// SQLSTATE/message (`22023` / `22009`).
#[derive(Debug)]
pub enum ZoneError {
    /// `time zone "<name>" not recognized` (22023).
    NotRecognized(String),
    /// `time zone displacement out of range: "<name>"` (22009).
    DisplacementOutOfRange(String),
}

/// Classify and resolve a zone token from a `timestamptz` literal, an
/// `AT TIME ZONE` argument, or a `make_timestamptz` zone argument.
///
/// Numeric offsets (`±HH`, `±HHMM`, `±HH:MM[:SS]`), `Z`/`zulu`, and `UTC`/`GMT`
/// resolve to [`Zone::Fixed`] via our own parser. Named IANA zones
/// (`America/New_York`) and the zone-backed abbreviations in [`ABBREVS`]
/// resolve through `jiff`. Unknown tokens are [`ZoneError::NotRecognized`].
pub fn resolve_zone(name: &str) -> Result<Zone, ZoneError> {
    let token = name.trim();
    if token.is_empty() {
        return Err(ZoneError::NotRecognized(name.to_string()));
    }

    // Fixed numeric offsets and the UTC synonyms are handled by our own code so
    // they work across the entire timestamp range without `jiff`.
    if let Some(res) = parse_fixed(token) {
        return res.map(Zone::Fixed);
    }

    // Curated abbreviations, seeded from PG's `tznames/Default` for the entries
    // the tests exercise. Some are fixed offsets; the DST-varying ones map to a
    // reference IANA zone (e.g. MSK -> Europe/Moscow) so their offset tracks
    // that zone's history, matching PG.
    let upper = token.to_ascii_uppercase();
    if let Some(kind) = ABBREVS.iter().find(|(a, _)| *a == upper).map(|(_, k)| k) {
        return match kind {
            Abbrev::Fixed(secs) => Ok(Zone::Fixed(*secs)),
            Abbrev::Zone(zone) => TimeZone::get(zone)
                .map(Zone::Named)
                .map_err(|_| ZoneError::NotRecognized(name.to_string())),
        };
    }

    // A full IANA zone name.
    TimeZone::get(token)
        .map(Zone::Named)
        .map_err(|_| ZoneError::NotRecognized(name.to_string()))
}

/// The UTC offset (seconds east) to apply to a civil wall clock interpreted in
/// `zone`, following PG's gap-before / fold-after rule. `utc = civil - offset`.
pub fn offset_for_local(zone: &Zone, tm: TmLite) -> i32 {
    match zone {
        Zone::Fixed(secs) => *secs,
        Zone::Named(tz) => {
            let dt = civil_datetime(tm);
            match tz.to_ambiguous_timestamp(dt).offset() {
                AmbiguousOffset::Unambiguous { offset } => offset.seconds(),
                // Spring-forward gap: the offset in effect before the transition.
                AmbiguousOffset::Gap { before, .. } => before.seconds(),
                // Fall-back fold: the offset in effect after the transition.
                AmbiguousOffset::Fold { after, .. } => after.seconds(),
            }
        }
    }
}

/// The UTC offset (seconds east) in effect in `zone` at the given UTC instant
/// (our microseconds since the 2000 epoch). Used by `AT TIME ZONE` on a
/// `timestamptz` and by `timezone(zone, timestamptz)`.
pub fn offset_for_instant(zone: &Zone, micros: i64) -> i32 {
    match zone {
        Zone::Fixed(secs) => *secs,
        Zone::Named(tz) => tz.to_offset(instant(micros)).seconds(),
    }
}

/// Build a `jiff` civil datetime, clamping a year beyond `jiff`'s `±9999` range
/// to its maximum. Our low end (-4712) is already in range; only the synthetic
/// upper-boundary rows (`294276-…`) clamp, and under a UTC display zone their
/// named-zone offset never surfaces in output.
fn civil_datetime(tm: TmLite) -> DateTime {
    if tm.year > 9999 {
        return DateTime::MAX;
    }
    if tm.year < -9999 {
        return DateTime::MIN;
    }
    DateTime::new(
        tm.year as i16,
        tm.month as i8,
        tm.day as i8,
        tm.hour as i8,
        tm.min as i8,
        tm.sec as i8,
        0,
    )
    .expect("caller-validated civil datetime is in jiff's range")
}

/// Build a `jiff` UTC timestamp from our micros-since-2000, clamping beyond
/// `jiff`'s range (see [`civil_datetime`]).
fn instant(micros: i64) -> Timestamp {
    let unix = micros.saturating_add(PG_EPOCH_UNIX_MICROS);
    Timestamp::from_microsecond(unix).unwrap_or(if unix >= 0 {
        Timestamp::MAX
    } else {
        Timestamp::MIN
    })
}

/// Parse a fixed-offset token: `Z`/`zulu`/`UTC`/`GMT` (zero) or a signed
/// numeric displacement `±HH`, `±HHMM`, `±HH:MM`, `±HH:MM:SS`. Returns `None`
/// if the token is not a fixed-offset form (so the caller falls through to the
/// abbreviation table / named-zone lookup).
fn parse_fixed(token: &str) -> Option<Result<i32, ZoneError>> {
    let lower = token.to_ascii_lowercase();
    if matches!(lower.as_str(), "z" | "zulu" | "utc" | "gmt") {
        return Some(Ok(0));
    }
    let sign = match token.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let body = &token[1..];
    if body.is_empty() || !body.bytes().all(|b| b.is_ascii_digit() || b == b':') {
        return None;
    }
    // Parse the hour/minute/second components as `i64`: the colon-form hour is
    // otherwise unbounded, so computing `h * 3600` in a narrower type would
    // overflow (panic in debug) before the displacement check below. A body that
    // does not fit in `i64` falls through as unrecognized.
    let (h, m, s): (i64, i64, i64) = if body.contains(':') {
        let mut it = body.split(':');
        let h = it.next()?.parse().ok()?;
        let m = it.next().map(|p| p.parse()).transpose().ok()?.unwrap_or(0);
        let s = it.next().map(|p| p.parse()).transpose().ok()?.unwrap_or(0);
        if it.next().is_some() {
            return Some(Err(ZoneError::NotRecognized(token.to_string())));
        }
        (h, m, s)
    } else {
        // `±HH`, `±HHMM`, `±HHMMSS` by digit count.
        match body.len() {
            1 | 2 => (body.parse().ok()?, 0, 0),
            4 => (body[..2].parse().ok()?, body[2..].parse().ok()?, 0),
            6 => (
                body[..2].parse().ok()?,
                body[2..4].parse().ok()?,
                body[4..].parse().ok()?,
            ),
            _ => return None,
        }
    };
    if !(0..60).contains(&m) || !(0..60).contains(&s) {
        return Some(Err(ZoneError::DisplacementOutOfRange(token.to_string())));
    }
    // `h` is bounded only by `i64`; do the arithmetic in `i64` and reject an
    // out-of-range magnitude rather than overflowing.
    let secs = sign as i64 * (h * 3600 + m * 60 + s);
    if secs.abs() > MAX_TZ_DISPLACEMENT_SECS as i64 {
        return Some(Err(ZoneError::DisplacementOutOfRange(token.to_string())));
    }
    Some(Ok(secs as i32))
}

/// How an abbreviation resolves.
enum Abbrev {
    /// A constant offset (seconds east of UTC).
    Fixed(i32),
    /// A DST-varying abbreviation: resolve through this reference IANA zone so
    /// the offset tracks the zone's history, as PG does.
    Zone(&'static str),
}

/// Curated timezone abbreviations, taken from PG's `src/timezone/tznames/Default`
/// for the entries the smoke suite exercises (extend as differential tests
/// demand — never guess). `PST`/`EST` are fixed standard-time offsets; `MSK`
/// tracks Moscow's DST history (it moved +1h in Mar 2011 and back in Oct 2014),
/// so it maps to the zone rather than a constant.
static ABBREVS: &[(&str, Abbrev)] = &[
    ("PST", Abbrev::Fixed(-8 * 3600)),
    ("EST", Abbrev::Fixed(-5 * 3600)),
    ("MSK", Abbrev::Zone("Europe/Moscow")),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn tm(year: i64, month: i64, day: i64, hour: i64, min: i64, sec: i64) -> TmLite {
        TmLite { year, month, day, hour, min, sec }
    }

    fn fixed(z: &Zone) -> i32 {
        match z {
            Zone::Fixed(s) => *s,
            Zone::Named(_) => panic!("expected fixed zone"),
        }
    }

    #[test]
    fn fixed_offsets_and_utc_synonyms() {
        assert_eq!(fixed(&resolve_zone("UTC").unwrap()), 0);
        assert_eq!(fixed(&resolve_zone("GMT").unwrap()), 0);
        assert_eq!(fixed(&resolve_zone("Z").unwrap()), 0);
        assert_eq!(fixed(&resolve_zone("zulu").unwrap()), 0);
        assert_eq!(fixed(&resolve_zone("+00").unwrap()), 0);
        assert_eq!(fixed(&resolve_zone("-08").unwrap()), -8 * 3600);
        assert_eq!(fixed(&resolve_zone("-0800").unwrap()), -8 * 3600);
        assert_eq!(fixed(&resolve_zone("-08:00").unwrap()), -8 * 3600);
        assert_eq!(fixed(&resolve_zone("+05:30").unwrap()), 5 * 3600 + 30 * 60);
        assert_eq!(fixed(&resolve_zone("-04:30").unwrap()), -(4 * 3600 + 30 * 60));
    }

    #[test]
    fn displacement_out_of_range() {
        assert!(matches!(
            resolve_zone("+16"),
            Err(ZoneError::DisplacementOutOfRange(_))
        ));
        assert!(matches!(
            resolve_zone("-16"),
            Err(ZoneError::DisplacementOutOfRange(_))
        ));
        // ±15:59:59 is the last accepted magnitude.
        assert_eq!(fixed(&resolve_zone("+15:59:59").unwrap()), 15 * 3600 + 59 * 60 + 59);
        // A huge colon-form hour must be rejected as out-of-range, not overflow
        // `h * 3600` (which panicked in debug before the fix).
        for tok in ["+600000:00", "-600000:00", "+2000000000:00", "+99"] {
            assert!(
                matches!(resolve_zone(tok), Err(ZoneError::DisplacementOutOfRange(_))),
                "{tok}"
            );
        }
    }

    #[test]
    fn unknown_zone() {
        assert!(matches!(
            resolve_zone("Nowhere/Nozone"),
            Err(ZoneError::NotRecognized(_))
        ));
    }

    #[test]
    fn named_zone_fixed_abbrev() {
        // America/New_York in winter is EST (-05:00).
        let z = resolve_zone("America/New_York").unwrap();
        assert_eq!(offset_for_local(&z, tm(1997, 2, 10, 17, 32, 1)), -5 * 3600);
        // In July it is EDT (-04:00).
        assert_eq!(offset_for_local(&z, tm(2013, 7, 15, 17, 15, 23)), -4 * 3600);
        // The fixed PST abbreviation is always -08:00.
        assert_eq!(fixed(&resolve_zone("PST").unwrap()), -8 * 3600);
    }

    #[test]
    fn moscow_dst_gap_and_fold() {
        // Europe/Moscow sprang forward +3 -> +4 on 2011-03-27 02:00 (a gap:
        // 02:00..02:59 is nonexistent) and fell back +4 -> +3 on 2014-10-26
        // 01:00 (a fold: 01:00..01:59 is ambiguous).
        let z = resolve_zone("Europe/Moscow").unwrap();
        // Before the gap: +3.
        assert_eq!(offset_for_local(&z, tm(2011, 3, 27, 1, 0, 0)), 3 * 3600);
        // Inside the gap: PG uses the pre-transition offset, +3.
        assert_eq!(offset_for_local(&z, tm(2011, 3, 27, 2, 30, 0)), 3 * 3600);
        // After the gap: +4.
        assert_eq!(offset_for_local(&z, tm(2011, 3, 27, 3, 0, 0)), 4 * 3600);
        // Inside the fold: PG uses the post-transition offset, +3.
        assert_eq!(offset_for_local(&z, tm(2014, 10, 26, 1, 30, 0)), 3 * 3600);
        // MSK tracks the same history.
        let msk = resolve_zone("MSK").unwrap();
        assert_eq!(offset_for_local(&msk, tm(2011, 3, 27, 1, 0, 0)), 3 * 3600);
        assert_eq!(offset_for_local(&msk, tm(2011, 3, 27, 3, 0, 0)), 4 * 3600);
    }

    #[test]
    fn offset_for_instant_tracks_dst() {
        let z = resolve_zone("America/New_York").unwrap();
        // 2001-02-16 20:38:40 UTC: micros since 2000 epoch.
        // (computed via the timestamp module in integration tests; here just
        // assert winter EST vs summer EDT via two instants.)
        // 2001-02-16T20:38:40Z -> EST (-5h). Unix secs = 982355920.
        let feb_micros = (982_355_920i64 - 946_684_800) * 1_000_000;
        assert_eq!(offset_for_instant(&z, feb_micros), -5 * 3600);
        // 2001-07-16T20:38:40Z -> EDT (-4h). Unix secs = 995315920.
        let jul_micros = (995_315_920i64 - 946_684_800) * 1_000_000;
        assert_eq!(offset_for_instant(&z, jul_micros), -4 * 3600);
    }
}
