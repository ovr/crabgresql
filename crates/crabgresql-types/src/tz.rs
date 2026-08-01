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

/// PG's timezone-displacement limit for a zone token inside a value —
/// a `timestamptz` literal, `AT TIME ZONE`, `make_timestamptz`: magnitudes of
/// `±15:59:59` are accepted, `±16:00:00` and beyond are "out of range".
const MAX_TZ_DISPLACEMENT_SECS: i32 = 16 * 3600 - 1;

/// The limit on the numeric `TimeZone` **GUC** forms (`SET TIME ZONE 7`,
/// `SET TIME ZONE INTERVAL '…'`), which is far wider than the in-value limit
/// above: PG accepts up to `±167:59:59` (one hour short of seven days) and
/// rejects `168` with "UTC timezone offset is out of range."
const MAX_GUC_OFFSET_SECS: i32 = 168 * 3600 - 1;

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
///
/// `Clone` is cheap: `jiff`'s `TimeZone` is reference-counted internally.
#[derive(Clone)]
pub enum Zone {
    /// Seconds east of UTC. `utc = civil - offset`.
    Fixed(i32),
    Named(TimeZone),
}

/// The session's `TimeZone` GUC: the resolved zone plus the two strings PG
/// reports for it — the canonical name (`SHOW TimeZone`, `ParameterStatus`) and,
/// where the zone has one, the abbreviation `to_char`'s `TZ` code prints.
///
/// The abbreviation is carried rather than derived because a [`Zone::Fixed`]
/// cannot say where it came from: `SET TimeZone = 'UTC'` prints `UTC`,
/// `SET TIME ZONE 7` prints `+07`, and `SET TimeZone = '+05:30'` prints nothing
/// at all — three different answers for the same offset. A [`Zone::Named`]
/// leaves it `None` and asks `jiff` per instant, since the abbreviation varies
/// with DST.
#[derive(Clone)]
pub struct SessionZone {
    name: String,
    abbrev: Option<String>,
    zone: Zone,
}

impl SessionZone {
    /// The boot value. PG's default is the host zone; ours is UTC, which keeps
    /// every expected output in the test suites stable.
    pub fn utc() -> SessionZone {
        SessionZone {
            name: "UTC".to_string(),
            abbrev: Some("UTC".to_string()),
            zone: Zone::Fixed(0),
        }
    }

    /// Resolve a `SET TimeZone = '<spec>'` value.
    ///
    /// **This is not [`resolve_zone`]**, and the difference is a sign. A zone
    /// token inside a `timestamptz` literal is ISO — `'12:00:00+05:30'` is east
    /// of UTC — but a bare numeric *GUC* value is POSIX, where the offset counts
    /// west: `SET TimeZone = '+05:30'` puts the session at UTC−5:30. Pinned by a
    /// differential test against PG. Named zones and abbreviations mean the same
    /// thing in both places.
    ///
    /// `Z`/`zulu` are accepted as literal tokens but rejected as GUC values, as
    /// in PG; `UTC` and `GMT` are accepted in both.
    pub fn resolve(spec: &str) -> Result<SessionZone, ZoneError> {
        let token = spec.trim();
        let not_recognized = || ZoneError::NotRecognized(spec.to_string());
        if token.is_empty() {
            return Err(not_recognized());
        }

        // The UTC synonyms, reported in PG's canonical upper case.
        if token.eq_ignore_ascii_case("utc") || token.eq_ignore_ascii_case("gmt") {
            let name = token.to_ascii_uppercase();
            return Ok(SessionZone {
                abbrev: Some(name.clone()),
                name,
                zone: Zone::Fixed(0),
            });
        }
        if token.eq_ignore_ascii_case("z") || token.eq_ignore_ascii_case("zulu") {
            return Err(not_recognized());
        }

        // A bare numeric displacement: POSIX-signed, and with no abbreviation
        // (PG's `TZ` renders empty for one). The name is echoed back as typed.
        if let Some(res) = parse_fixed(token) {
            let east = -res?;
            return Ok(SessionZone {
                name: token.to_string(),
                abbrev: None,
                zone: Zone::Fixed(east),
            });
        }

        // A curated abbreviation keeps its (upper-cased) spelling as both name
        // and abbreviation.
        let upper = token.to_ascii_uppercase();
        if let Some(kind) = ABBREVS.iter().find(|(a, _)| *a == upper).map(|(_, k)| k) {
            let zone = match kind {
                Abbrev::Fixed(secs) => Zone::Fixed(*secs),
                Abbrev::Zone(zone) => TimeZone::get(zone).map(Zone::Named).map_err(|_| not_recognized())?,
            };
            return Ok(SessionZone {
                name: upper.clone(),
                abbrev: Some(upper),
                zone,
            });
        }

        // A full IANA zone. `jiff`'s lookup is case-insensitive; report the
        // canonical spelling back, as PG does (`america/new_york` shows as
        // `America/New_York`).
        let tz = TimeZone::get(token).map_err(|_| not_recognized())?;
        Ok(SessionZone {
            name: tz.iana_name().unwrap_or(token).to_string(),
            abbrev: None,
            zone: Zone::Named(tz),
        })
    }

    /// A fixed zone `secs` east of UTC, named with the POSIX spec PG reports for
    /// the `SET TIME ZONE <number>` and `SET TIME ZONE INTERVAL '…'` forms —
    /// `SET TIME ZONE 7` shows as `<+07>-07`. Note these forms are *east*-signed,
    /// unlike the string form [`SessionZone::resolve`] handles.
    pub fn from_offset_east(secs: i32) -> Result<SessionZone, ZoneError> {
        if secs.abs() > MAX_GUC_OFFSET_SECS {
            return Err(ZoneError::DisplacementOutOfRange(format_offset(secs)));
        }
        let abbrev = format_offset(secs);
        Ok(SessionZone {
            name: format!("<{abbrev}>{}", format_offset(-secs)),
            abbrev: Some(abbrev),
            zone: Zone::Fixed(secs),
        })
    }

    /// The name `SHOW TimeZone` and `ParameterStatus` report.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn zone(&self) -> &Zone {
        &self.zone
    }

    /// The display offset (seconds east) for a stored UTC instant.
    pub fn offset_at(&self, micros: i64) -> i32 {
        offset_for_instant(&self.zone, micros)
    }

    /// The offset (seconds east) for a wall clock read in this zone, following
    /// PG's gap-before / fold-after rule.
    pub fn offset_for_wall(&self, tm: TmLite) -> i32 {
        offset_for_local(&self.zone, tm)
    }

    /// `to_char`'s `TZ` code at an instant. Empty when the zone has no
    /// abbreviation (a bare numeric GUC value), matching PG.
    pub fn abbrev_at(&self, micros: i64) -> String {
        if let Some(abbrev) = &self.abbrev {
            return abbrev.clone();
        }
        match &self.zone {
            Zone::Fixed(_) => String::new(),
            Zone::Named(tz) => tz.to_offset_info(instant(micros)).abbreviation().to_string(),
        }
    }
}

/// Render a UTC offset the way `timestamptz_out` does: `±HH`, widening to
/// `±HH:MM` when the minutes are non-zero and to `±HH:MM:SS` when the seconds
/// are (the LMT-era zones, e.g. `America/New_York` before 1883 at `-04:56:02`).
pub fn format_offset(secs: i32) -> String {
    let sign = if secs < 0 { '-' } else { '+' };
    let abs = secs.unsigned_abs();
    let (h, m, s) = (abs / 3600, (abs % 3600) / 60, abs % 60);
    if s != 0 {
        format!("{sign}{h:02}:{m:02}:{s:02}")
    } else if m != 0 {
        format!("{sign}{h:02}:{m:02}")
    } else {
        format!("{sign}{h:02}")
    }
}

/// Render a UTC offset the way `to_char`'s `OF` code does, which — unlike
/// [`format_offset`] — never emits a seconds field: PG's `DCH_OF` prints the
/// hours and, only when the offset is not a whole hour, the minutes. So an
/// LMT-era offset that `timestamptz_out` shows as `-04:56:02` is `-04:56` here.
pub fn format_offset_hours_minutes(secs: i32) -> String {
    let sign = if secs < 0 { '-' } else { '+' };
    let abs = secs.unsigned_abs();
    let (h, m) = (abs / 3600, (abs % 3600) / 60);
    if m != 0 {
        format!("{sign}{h:02}:{m:02}")
    } else {
        format!("{sign}{h:02}")
    }
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
/// upper-boundary rows (`294276-…`) clamp, and they then take the zone's
/// year-9999 offset — its far-future standard-time rule, which is what PG
/// extrapolates too. Under the default UTC session zone this is unreachable:
/// [`Zone::Fixed`] short-circuits before any `jiff` call.
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
        TmLite {
            year,
            month,
            day,
            hour,
            min,
            sec,
        }
    }

    fn fixed(z: &Zone) -> i32 {
        match z {
            Zone::Fixed(s) => *s,
            Zone::Named(_) => panic!("expected fixed zone"),
        }
    }

    #[test]
    fn fixed_offsets_and_utc_synonyms() -> anyhow::Result<()> {
        assert_eq!(fixed(&resolve_zone("UTC")?), 0);
        assert_eq!(fixed(&resolve_zone("GMT")?), 0);
        assert_eq!(fixed(&resolve_zone("Z")?), 0);
        assert_eq!(fixed(&resolve_zone("zulu")?), 0);
        assert_eq!(fixed(&resolve_zone("+00")?), 0);
        assert_eq!(fixed(&resolve_zone("-08")?), -8 * 3600);
        assert_eq!(fixed(&resolve_zone("-0800")?), -8 * 3600);
        assert_eq!(fixed(&resolve_zone("-08:00")?), -8 * 3600);
        assert_eq!(fixed(&resolve_zone("+05:30")?), 5 * 3600 + 30 * 60);
        assert_eq!(fixed(&resolve_zone("-04:30")?), -(4 * 3600 + 30 * 60));

        Ok(())
    }

    #[test]
    fn displacement_out_of_range() -> anyhow::Result<()> {
        assert!(matches!(
            resolve_zone("+16"),
            Err(ZoneError::DisplacementOutOfRange(_))
        ));
        assert!(matches!(
            resolve_zone("-16"),
            Err(ZoneError::DisplacementOutOfRange(_))
        ));
        // ±15:59:59 is the last accepted magnitude.
        assert_eq!(fixed(&resolve_zone("+15:59:59")?), 15 * 3600 + 59 * 60 + 59);
        // A huge colon-form hour must be rejected as out-of-range, not overflow
        // `h * 3600` (which panicked in debug before the fix).
        for tok in ["+600000:00", "-600000:00", "+2000000000:00", "+99"] {
            assert!(
                matches!(resolve_zone(tok), Err(ZoneError::DisplacementOutOfRange(_))),
                "{tok}"
            );
        }

        Ok(())
    }

    #[test]
    fn unknown_zone() {
        assert!(matches!(
            resolve_zone("Nowhere/Nozone"),
            Err(ZoneError::NotRecognized(_))
        ));
    }

    #[test]
    fn named_zone_fixed_abbrev() -> anyhow::Result<()> {
        // America/New_York in winter is EST (-05:00).
        let z = resolve_zone("America/New_York")?;
        assert_eq!(offset_for_local(&z, tm(1997, 2, 10, 17, 32, 1)), -5 * 3600);
        // In July it is EDT (-04:00).
        assert_eq!(offset_for_local(&z, tm(2013, 7, 15, 17, 15, 23)), -4 * 3600);
        // The fixed PST abbreviation is always -08:00.
        assert_eq!(fixed(&resolve_zone("PST")?), -8 * 3600);

        Ok(())
    }

    #[test]
    fn moscow_dst_gap_and_fold() -> anyhow::Result<()> {
        // Europe/Moscow sprang forward +3 -> +4 on 2011-03-27 02:00 (a gap:
        // 02:00..02:59 is nonexistent) and fell back +4 -> +3 on 2014-10-26
        // 01:00 (a fold: 01:00..01:59 is ambiguous).
        let z = resolve_zone("Europe/Moscow")?;
        // Before the gap: +3.
        assert_eq!(offset_for_local(&z, tm(2011, 3, 27, 1, 0, 0)), 3 * 3600);
        // Inside the gap: PG uses the pre-transition offset, +3.
        assert_eq!(offset_for_local(&z, tm(2011, 3, 27, 2, 30, 0)), 3 * 3600);
        // After the gap: +4.
        assert_eq!(offset_for_local(&z, tm(2011, 3, 27, 3, 0, 0)), 4 * 3600);
        // Inside the fold: PG uses the post-transition offset, +3.
        assert_eq!(offset_for_local(&z, tm(2014, 10, 26, 1, 30, 0)), 3 * 3600);
        // MSK tracks the same history.
        let msk = resolve_zone("MSK")?;
        assert_eq!(offset_for_local(&msk, tm(2011, 3, 27, 1, 0, 0)), 3 * 3600);
        assert_eq!(offset_for_local(&msk, tm(2011, 3, 27, 3, 0, 0)), 4 * 3600);

        Ok(())
    }

    #[test]
    fn offset_for_instant_tracks_dst() -> anyhow::Result<()> {
        let z = resolve_zone("America/New_York")?;
        // 2001-02-16 20:38:40 UTC: micros since 2000 epoch.
        // (computed via the timestamp module in integration tests; here just
        // assert winter EST vs summer EDT via two instants.)
        // 2001-02-16T20:38:40Z -> EST (-5h). Unix secs = 982355920.
        let feb_micros = (982_355_920i64 - 946_684_800) * 1_000_000;
        assert_eq!(offset_for_instant(&z, feb_micros), -5 * 3600);
        // 2001-07-16T20:38:40Z -> EDT (-4h). Unix secs = 995315920.
        let jul_micros = (995_315_920i64 - 946_684_800) * 1_000_000;
        assert_eq!(offset_for_instant(&z, jul_micros), -4 * 3600);

        Ok(())
    }

    #[test]
    fn format_offset_widens_only_as_needed() {
        assert_eq!(format_offset(0), "+00");
        assert_eq!(format_offset(-5 * 3600), "-05");
        // Kolkata: the minutes field appears.
        assert_eq!(format_offset(5 * 3600 + 30 * 60), "+05:30");
        // Chatham.
        assert_eq!(format_offset(12 * 3600 + 45 * 60), "+12:45");
        // America/New_York's pre-1883 LMT, where PG prints seconds too.
        assert_eq!(format_offset(-(4 * 3600 + 56 * 60 + 2)), "-04:56:02");
    }

    /// The sign trap: a bare numeric `TimeZone` GUC value is POSIX (west
    /// positive), while the identical token inside a `timestamptz` literal is
    /// ISO (east positive). `SET TimeZone = '+05:30'` puts the session at
    /// UTC-5:30 — pinned against PG 18.4.
    #[test]
    fn numeric_guc_value_is_posix_signed() -> anyhow::Result<()> {
        let session = SessionZone::resolve("+05:30")?;
        assert_eq!(session.offset_at(0), -(5 * 3600 + 30 * 60));
        assert_eq!(session.name(), "+05:30");
        // No abbreviation: PG's `TZ` renders empty for a numeric zone.
        assert_eq!(session.abbrev_at(0), "");

        // The literal token means the opposite direction.
        assert_eq!(offset_for_instant(&resolve_zone("+05:30")?, 0), 5 * 3600 + 30 * 60);
        Ok(())
    }

    /// `SET TIME ZONE 7` / `SET TIME ZONE INTERVAL '…'` are east-signed and
    /// report PG's POSIX spec as their name.
    #[test]
    fn offset_east_reports_posix_spec_name() -> anyhow::Result<()> {
        let seven = SessionZone::from_offset_east(7 * 3600)?;
        assert_eq!(seven.name(), "<+07>-07");
        assert_eq!(seven.abbrev_at(0), "+07");
        assert_eq!(seven.offset_at(0), 7 * 3600);

        let minus_eight = SessionZone::from_offset_east(-8 * 3600)?;
        assert_eq!(minus_eight.name(), "<-08>+08");

        let half = SessionZone::from_offset_east(5 * 3600 + 30 * 60)?;
        assert_eq!(half.name(), "<+05:30>-05:30");
        Ok(())
    }

    #[test]
    fn guc_names_are_canonicalized() -> anyhow::Result<()> {
        assert_eq!(SessionZone::resolve("utc")?.name(), "UTC");
        assert_eq!(SessionZone::resolve("gmt")?.name(), "GMT");
        // jiff's lookup is case-insensitive; PG reports the tzdb spelling.
        assert_eq!(
            SessionZone::resolve("america/new_york")?.name(),
            "America/New_York"
        );
        assert_eq!(SessionZone::resolve("EST")?.name(), "EST");
        // Accepted as a literal token, rejected as a GUC value — as in PG.
        assert!(SessionZone::resolve("Z").is_err());
        assert!(SessionZone::resolve("Nowhere/Nozone").is_err());
        Ok(())
    }

    #[test]
    fn named_zone_abbreviation_tracks_dst() -> anyhow::Result<()> {
        let ny = SessionZone::resolve("America/New_York")?;
        let feb_micros = (982_355_920i64 - 946_684_800) * 1_000_000;
        let jul_micros = (995_315_920i64 - 946_684_800) * 1_000_000;
        assert_eq!(ny.abbrev_at(feb_micros), "EST");
        assert_eq!(ny.abbrev_at(jul_micros), "EDT");
        Ok(())
    }
}
