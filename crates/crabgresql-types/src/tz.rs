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
pub(crate) const MAX_TZ_DISPLACEMENT_SECS: i32 = 16 * 3600 - 1;

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
    /// Cached [`SessionZone::standard_offset`] — see [`resolve_standard_offset`].
    standard_offset: i32,
}

impl SessionZone {
    /// The one place a `SessionZone` is built, so the cached standard offset
    /// cannot be forgotten by a new constructor.
    fn new(name: String, abbrev: Option<String>, zone: Zone) -> SessionZone {
        SessionZone {
            standard_offset: resolve_standard_offset(&zone),
            name,
            abbrev,
            zone,
        }
    }

    /// The boot value. PG's default is the host zone; ours is UTC, which keeps
    /// every expected output in the test suites stable.
    pub fn utc() -> SessionZone {
        SessionZone::new("UTC".to_string(), Some("UTC".to_string()), Zone::Fixed(0))
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
            return Ok(SessionZone::new(name.clone(), Some(name), Zone::Fixed(0)));
        }
        if token.eq_ignore_ascii_case("z") || token.eq_ignore_ascii_case("zulu") {
            return Err(not_recognized());
        }

        // A bare numeric displacement: POSIX-signed, and with no abbreviation
        // (PG's `TZ` renders empty for one). The name is echoed back as typed.
        if let Some(res) = parse_fixed(token) {
            let east = -res?;
            return Ok(SessionZone::new(token.to_string(), None, Zone::Fixed(east)));
        }

        // Note what is *not* here: [`DATETIME_ABBREVS`]. The GUC namespace is
        // narrower than the literal one — PG rejects `SET TimeZone = 'PDT'` and
        // `SET TimeZone = 'MSK'`, while accepting both inside a datetime value.
        // `EST`/`MST`/`HST` keep working below because they are IANA zone names
        // in their own right, not because they are abbreviations.

        // A full IANA zone. `jiff`'s lookup is case-insensitive; report the
        // canonical spelling back, as PG does (`america/new_york` shows as
        // `America/New_York`).
        if let Ok(tz) = TimeZone::get(token) {
            let name = tz.iana_name().unwrap_or(token).to_string();
            return Ok(SessionZone::new(name, None, Zone::Named(tz)));
        }

        // The POSIX `<letters><±offset>` form, which — unlike the abbreviation
        // table above — the GUC *does* accept: PG takes `SET TimeZone = 'UTC+5'`
        // and echoes it back verbatim from `SHOW`. Same west-counting sign as in
        // a value, so this is the one spelling that means the same in both
        // namespaces.
        let east = parse_abbrev_prefix_offset(token).ok_or_else(not_recognized)?;
        Ok(SessionZone::new(
            token.to_string(),
            None,
            Zone::Fixed(east),
        ))
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
        let name = format!("<{abbrev}>{}", format_offset(-secs));
        Ok(SessionZone::new(name, Some(abbrev), Zone::Fixed(secs)))
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

    /// The zone's **standard-time** offset (seconds east). Resolved once at
    /// construction, since the zone is immutable and the answer cannot vary
    /// between rows.
    ///
    /// The clock-free fallback for [`crate::FmtCtx::zone_offset_today`], which
    /// is what a date-less value (`time -> timetz`, `timetz_in` with no zone
    /// token) actually attaches. It agrees with today's offset for a
    /// fixed-offset zone and for any zone outside its DST window; where DST is
    /// in effect it is an hour out.
    pub fn standard_offset(&self) -> i32 {
        self.standard_offset
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

/// Classify and resolve a zone token from a `timestamptz` literal or a
/// `make_timestamptz` zone argument.
///
/// Numeric offsets (`±HH`, `±HHMM`, `±HH:MM[:SS]`), `Z`/`zulu`, and `UTC`/`GMT`
/// resolve to [`Zone::Fixed`] via our own parser. Named IANA zones
/// (`America/New_York`) and the zone-backed abbreviations in [`ABBREVS`]
/// resolve through `jiff`. Unknown tokens are [`ZoneError::NotRecognized`].
///
/// **Not** the resolver for `AT TIME ZONE` or the three-argument `date_trunc` —
/// see [`resolve_zone_arg`], which reads a bare numeric offset with the opposite
/// sign.
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
    if let Some(kind) = lookup_abbrev(token) {
        return match kind {
            Abbrev::Fixed(secs) => Ok(Zone::Fixed(*secs)),
            Abbrev::Zone(zone) => TimeZone::get(zone)
                .map(Zone::Named)
                .map_err(|_| ZoneError::NotRecognized(name.to_string())),
        };
    }

    // A full IANA zone name.
    if let Ok(tz) = TimeZone::get(token) {
        return Ok(Zone::Named(tz));
    }

    // Last resort: PG's abbreviation-prefix form, `<alpha><POSIX offset>` —
    // `UTC+10`, `GMT+5`, and (verified against PG 18) `PDT+5` and even `XYZ+5`,
    // which all mean the same thing. The prefix is *entirely ignored*; only the
    // offset counts, and it is POSIX-signed (hours **west**), so `UTC+10` is
    // UTC−10 — the opposite of the bare `+10` handled by `parse_fixed` above.
    parse_abbrev_prefix_offset(token)
        .map(Zone::Fixed)
        .ok_or_else(|| ZoneError::NotRecognized(name.to_string()))
}

/// Classify and resolve a zone token that arrives as a *function argument* —
/// `AT TIME ZONE`, `timezone(zone, …)`, and the three-argument `date_trunc`.
/// PG funnels all three through one reader (`parse_sane_timezone`), and its
/// grammar is neither [`resolve_zone`]'s nor [`SessionZone::resolve`]'s.
///
/// The difference that matters is the **sign of a bare numeric offset**, and it
/// is the reverse of the same spelling inside a value. Verified against PG 18
/// with the session in UTC:
///
/// | token       | here                | [`resolve_zone`] (in a value) |
/// |-------------|---------------------|-------------------------------|
/// | `+05:30`    | UTC−5:30 (POSIX)    | UTC+5:30 (ISO)                |
/// | `05:30`     | UTC−5:30            | not recognized                |
/// | `+0530`     | not recognized      | UTC+5:30                      |
/// | `+16`       | UTC−16              | out of range                  |
///
/// So the colon-less `±HHMM` form is a value-only spelling, the wide
/// [`MAX_GUC_OFFSET_SECS`] band applies here rather than the in-value
/// [`MAX_TZ_DISPLACEMENT_SECS`] one, and an unsigned offset is legal. Named
/// zones, the [`ABBREVS`] table, `Z`/`zulu` and `UTC`/`GMT` mean the same in
/// both readers.
///
/// The token is **not trimmed**, unlike [`resolve_zone`]'s: PG matches a zone
/// argument as given, so `'UTC '` and `' UTC'` are both `22023`. Only the
/// numeric form tolerates leading whitespace (`' +05:30'` is UTC−5:30), which
/// [`parse_posix_offset`] handles.
pub fn resolve_zone_arg(name: &str) -> Result<Zone, ZoneError> {
    let token = name;
    let not_recognized = || ZoneError::NotRecognized(name.to_string());
    if token.is_empty() {
        return Err(not_recognized());
    }

    // The UTC synonyms, handled by our own code so they work across the entire
    // timestamp range without `jiff`.
    if matches!(
        token.to_ascii_lowercase().as_str(),
        "z" | "zulu" | "utc" | "gmt"
    ) {
        return Ok(Zone::Fixed(0));
    }

    // Same abbreviation table as a datetime value: PG accepts `VET` and `MSK`
    // here, unlike in the `TimeZone` GUC.
    if let Some(kind) = lookup_abbrev(token) {
        return match kind {
            Abbrev::Fixed(secs) => Ok(Zone::Fixed(*secs)),
            Abbrev::Zone(zone) => TimeZone::get(zone)
                .map(Zone::Named)
                .map_err(|_| not_recognized()),
        };
    }

    // A full IANA zone name. Tried before the POSIX form below so that a real
    // zone whose name ends in a displacement (`Etc/GMT+5`) keeps its own,
    // opposite-signed meaning.
    if let Ok(tz) = TimeZone::get(token) {
        return Ok(Zone::Named(tz));
    }

    // A POSIX displacement, with or without the ignored alphabetic prefix:
    // `+05:30`, `05:30`, `UTC+10` and `XYZ5` all land here.
    let split = token
        .find(|c: char| !c.is_ascii_alphabetic())
        .ok_or_else(not_recognized)?;
    parse_posix_offset(&token[split..])
        .map(Zone::Fixed)
        .ok_or_else(not_recognized)
}

/// PG's `<abbrev><POSIX offset>` zone form. Returns seconds **east** of UTC.
///
/// The leading run of ASCII letters is discarded — PG does not check it against
/// the abbreviation table, so `XYZ+5` resolves exactly like `UTC+5` (verified
/// against PG 18). The remainder is a POSIX displacement, which counts hours
/// *west*, so `UTC+10` is UTC−10 — the opposite of the bare `+10` that
/// [`parse_fixed`] handles.
fn parse_abbrev_prefix_offset(token: &str) -> Option<i32> {
    let split = token.find(['+', '-'])?;
    let (prefix, body) = token.split_at(split);
    if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    parse_posix_offset(body)
}

/// A POSIX displacement, `[+-]?HH[:MM[:SS]]`, in seconds **east** of UTC — the
/// sign is flipped on the way out, since POSIX counts hours *west*.
///
/// The field rules are PG's, pinned by probing 18.4 rather than by counting
/// digits the way a `timestamptz` literal's offset is counted:
///
/// * a field is *digits*, however many — `5:0000000030` is 5:30 and `005:30` is
///   5:30, so there is no two-digit cap;
/// * minutes are `0..=59` (`5:60` is unrecognized), but **seconds may be 60**
///   and carry: `5:30:60` is 5:31:00 and `5:59:60` is 6:00:00;
/// * the [`MAX_GUC_OFFSET_SECS`] band is checked on the **hour field**, not on
///   the total, which is why `167:59:60` (a flat 168 h) is accepted while
///   `+168` and `+200:00` are not. Inside a value the much narrower
///   [`MAX_TZ_DISPLACEMENT_SECS`] applies instead, so a bare `+16` is legal
///   here and out of range there.
///
/// A fractional part is where PG stops being a displacement reader at all: it
/// treats the rest as a DST abbreviation and applies default transition rules,
/// so `'5.5'`, `'5.0'` and `'5.'` mean 5 h, 0 h and 4 h west respectively. We
/// implement no default DST rules, so any fraction is refused — see
/// [`resolve_zone_arg`] for the divergence note.
fn parse_posix_offset(body: &str) -> Option<i32> {
    // Leading whitespace is skipped, as PG's reader does — `' +05:30'` is a
    // legal zone argument. Trailing whitespace is *not*: PG reads it as the
    // start of a DST abbreviation, which we do not implement (see
    // [`resolve_zone_arg`]), so we refuse it rather than guess.
    let body = body.trim_start();
    let (sign, digits): (i64, &str) = match body.as_bytes().first()? {
        b'+' => (1, &body[1..]),
        b'-' => (-1, &body[1..]),
        _ => (1, body),
    };
    let mut parts = digits.split(':');
    // The hour is unbounded in digits (`+167` and `00005` are both legal), so
    // parse it as i64 and let the band check reject an over-large one rather
    // than overflowing.
    let hh = field(parts.next()?)?;
    // The band is the hour's, not the total's: `167:59:60` is a flat 168 h and
    // PG takes it, while `+168` is one hour too far.
    if hh * 3600 > MAX_GUC_OFFSET_SECS as i64 {
        return None;
    }
    let mut secs = hh * 3600;
    for (i, part) in parts.enumerate() {
        let v = field(part)?;
        match i {
            // Minutes cap at 59 …
            0 if v < 60 => secs += v * 60,
            // … seconds at 60, which carries into the minute.
            1 if v <= 60 => secs += v,
            _ => return None,
        }
    }
    // POSIX counts west; our `Zone::Fixed` counts east.
    Some((-sign * secs) as i32)
}

/// One `:`-separated field of a POSIX displacement: digits only, any number of
/// them, leading zeros and all. `None` for anything else — including a
/// fractional part, which PG reads as the start of a DST abbreviation.
fn field(part: &str) -> Option<i64> {
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
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

/// The zone's standard-time (non-DST) offset in seconds east — see
/// [`SessionZone::standard_offset`], which caches this.
///
/// The two probes are deliberately in the **far future**, not at a fixed real
/// date. A real date has to be picked, goes stale, and is then wrong by however
/// much the zone's standard offset has moved since: probing the 2000 epoch put
/// `Pacific/Apia` at `-11` instead of `+13` (the 2011 dateline move) and
/// `Europe/Istanbul` at `+02` instead of `+03`. Past the last recorded
/// transition, `jiff` answers from the zone's POSIX extrapolation rule — its
/// *current* standard time and DST rule projected forward — which is the thing
/// we actually want and which follows a tzdb update for free. `civil_datetime`
/// leans on the same property for out-of-range years.
///
/// Two probes six months apart because the hemispheres put their DST in
/// opposite halves of the year.
fn resolve_standard_offset(zone: &Zone) -> i32 {
    let tz = match zone {
        Zone::Fixed(secs) => return *secs,
        Zone::Named(tz) => tz,
    };
    // ~2100-01-01 and ~2100-07-01, as our micros-since-2000. Well past every
    // recorded transition and well inside `jiff`'s ±9999 range.
    const WINTERISH: i64 = 36_525 * 86_400_000_000;
    const SUMMERISH: i64 = WINTERISH + 182 * 86_400_000_000;
    let a = tz.to_offset_info(instant(WINTERISH));
    if !a.dst().is_dst() {
        return a.offset().seconds();
    }
    let b = tz.to_offset_info(instant(SUMMERISH));
    if !b.dst().is_dst() {
        return b.offset().seconds();
    }
    // A zone tzdb models as DST all year round has no standard offset to find.
    a.offset().seconds()
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

/// The wall clock, as our microseconds since the 2000 epoch.
///
/// The one place in the engine that reads real time. Everything time-dependent
/// above this layer takes its instant from the session's clock
/// ([`crate::fmt::Clock`]) so that a value is stable for as long as PostgreSQL
/// says it is; only `clock_timestamp()`, which is volatile by definition, and
/// the session stamping that fills that clock in call this.
pub fn now_micros() -> i64 {
    Timestamp::now().as_microsecond() - PG_EPOCH_UNIX_MICROS
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
///
/// The distinction is not cosmetic: it is exactly PG's *static* versus
/// *dynamic* split, and it decides whether an abbreviation can be used without
/// a date. A [`Abbrev::Fixed`] entry names one offset for all time, so
/// `'00:01 PDT'` is meaningful on its own; a [`Abbrev::Zone`] entry only means
/// something at an instant, so `'15:36:39 MSK'` with no date is an error.
pub(crate) enum Abbrev {
    /// A constant offset (seconds east of UTC).
    Fixed(i32),
    /// A DST-varying abbreviation: resolve through this reference IANA zone so
    /// the offset tracks the zone's history, as PG does.
    Zone(&'static str),
}

/// Curated timezone abbreviations for **datetime literals**, taken from PG's
/// `src/timezone/tznames/Default` for the entries our suites exercise (extend as
/// differential tests demand — never guess).
///
/// This is deliberately *not* the same namespace as the `TimeZone` GUC accepts.
/// PG rejects `SET TimeZone = 'PDT'` while happily reading `timestamptz
/// '2020-06-01 12:00 PDT'`; `SET TimeZone = 'EST'` works only because `EST` also
/// happens to be an IANA zone name. So [`SessionZone::resolve`] does not consult
/// this table at all — see its doc comment.
static DATETIME_ABBREVS: &[(&str, Abbrev)] = &[
    // North America, standard and daylight.
    ("AST", Abbrev::Fixed(-4 * 3600)),
    ("ADT", Abbrev::Fixed(-3 * 3600)),
    ("EST", Abbrev::Fixed(-5 * 3600)),
    ("EDT", Abbrev::Fixed(-4 * 3600)),
    ("CST", Abbrev::Fixed(-6 * 3600)),
    ("CDT", Abbrev::Fixed(-5 * 3600)),
    ("MST", Abbrev::Fixed(-7 * 3600)),
    ("MDT", Abbrev::Fixed(-6 * 3600)),
    ("PST", Abbrev::Fixed(-8 * 3600)),
    ("PDT", Abbrev::Fixed(-7 * 3600)),
    ("AKST", Abbrev::Fixed(-9 * 3600)),
    ("AKDT", Abbrev::Fixed(-8 * 3600)),
    ("HST", Abbrev::Fixed(-10 * 3600)),
    // `MSK` tracks Moscow's DST history (it moved +1h in Mar 2011 and back in
    // Oct 2014), so it maps to the zone rather than a constant — and therefore
    // needs a date.
    ("MSK", Abbrev::Zone("Europe/Moscow")),
    // `VET` likewise varies: Venezuela ran at -04:30 from 2007 to 2016 and at
    // -04 either side of that, and PG's reading of the abbreviation follows the
    // zone (verified against PG 18.4). Exercised by upstream's `timestamptz`
    // suite as the "variable-offset abbreviation" case for `date_trunc`.
    ("VET", Abbrev::Zone("America/Caracas")),
];

/// Look up a datetime-literal abbreviation, case-insensitively.
pub(crate) fn lookup_abbrev(token: &str) -> Option<&'static Abbrev> {
    let upper = token.to_ascii_uppercase();
    DATETIME_ABBREVS
        .iter()
        .find(|(a, _)| *a == upper)
        .map(|(_, k)| k)
}

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

    /// The third sign convention: a zone token passed as a *function argument*
    /// (`AT TIME ZONE`, the three-argument `date_trunc`) is POSIX like the GUC
    /// string, not ISO like the same token inside a value. Pinned against PG
    /// 18.4 with the session in UTC.
    #[test]
    fn zone_argument_offsets_are_posix_signed() -> anyhow::Result<()> {
        let arg = |t: &str| -> anyhow::Result<i32> { Ok(fixed(&resolve_zone_arg(t)?)) };
        // The headline reversal: west here, east inside a value.
        assert_eq!(arg("+05:30")?, -(5 * 3600 + 30 * 60));
        assert_eq!(fixed(&resolve_zone("+05:30")?), 5 * 3600 + 30 * 60);
        assert_eq!(arg("-05:30")?, 5 * 3600 + 30 * 60);
        // The sign is optional, unlike in a value.
        assert_eq!(arg("05:30")?, -(5 * 3600 + 30 * 60));
        assert_eq!(arg("5")?, -5 * 3600);
        assert_eq!(arg("+05:30:15")?, -(5 * 3600 + 30 * 60 + 15));
        // A field is digits, however many: no two-digit cap, leading zeros fine.
        assert_eq!(arg("+5:3")?, -(5 * 3600 + 3 * 60));
        assert_eq!(arg("5:0000000030")?, -(5 * 3600 + 30 * 60));
        assert_eq!(arg("005:30")?, -(5 * 3600 + 30 * 60));
        assert_eq!(arg("5:059")?, -(5 * 3600 + 59 * 60));
        // Minutes cap at 59; seconds may be 60 and carry.
        assert_eq!(arg("5:30:60")?, -(5 * 3600 + 31 * 60));
        assert_eq!(arg("5:59:60")?, -6 * 3600);
        for tok in ["5:60", "5:100", "5:30:100"] {
            assert!(
                matches!(resolve_zone_arg(tok), Err(ZoneError::NotRecognized(_))),
                "{tok}"
            );
        }
        // The colon-less `±HHMM` spelling is value-only: PG does not read it here.
        assert!(matches!(
            resolve_zone_arg("+0530"),
            Err(ZoneError::NotRecognized(_))
        ));
        // The wide GUC band applies, so `+16` is a legal argument even though it
        // is out of range inside a value. `+168` is one hour too far, and PG
        // reports it as unrecognized rather than out of range.
        assert_eq!(arg("+16")?, -16 * 3600);
        assert_eq!(arg("+167")?, -167 * 3600);
        // The band is the hour field's, not the total's, so a flat 168 h spelled
        // with a carrying seconds field is legal where `+168` is not.
        assert_eq!(arg("167:59:60")?, -168 * 3600);
        for tok in ["+168", "+200:00"] {
            assert!(
                matches!(resolve_zone_arg(tok), Err(ZoneError::NotRecognized(_))),
                "{tok}"
            );
        }
        Ok(())
    }

    /// **Known divergence from PostgreSQL**, pinned here so it is visible rather
    /// than silent.
    ///
    /// A fraction is where PG stops reading a displacement: it takes the rest as
    /// a DST abbreviation and applies its default transition rules, so with the
    /// session in UTC and a June instant, PG 18.4 answers (west of UTC)
    ///
    /// ```text
    /// '5.5' -> 5 h    '-5.5' -> 5 h   (the sign belongs to the std part)
    /// '5.0' -> 0 h    '5.'   -> 4 h   (std 5, DST unnamed, June is DST)
    /// '5:30.5' -> 5 h        '5:30:30.9' -> 9 h
    /// ```
    ///
    /// Reproducing that means implementing tzcode's default DST rules, which is
    /// the line AGENTS.md draws. We refuse the whole family with `22023`
    /// instead, which is at least a diagnosable answer rather than a wrong one.
    ///
    /// The `TimeZone` GUC shares the reader and so shares the refusal: PG takes
    /// `SET TimeZone = 'UTC+5.5'` (and runs at −05, or −04 while its default DST
    /// rule is in effect), we do not.
    #[test]
    fn a_fractional_displacement_is_refused() {
        for tok in [
            "5.5", "-5.5", "+5.5", "5.0", "5.", ".5", "5:30.5", "5:30:30.9", "UTC+5.5",
        ] {
            assert!(
                matches!(resolve_zone_arg(tok), Err(ZoneError::NotRecognized(_))),
                "{tok}"
            );
        }
        assert!(SessionZone::resolve("UTC+5.5").is_err());
        assert!(resolve_zone("UTC+5.5").is_err());
    }

    /// Everything that is *not* a bare displacement means the same as it does
    /// inside a value — including the abbreviations the `TimeZone` GUC refuses.
    #[test]
    fn zone_argument_names_match_the_value_namespace() -> anyhow::Result<()> {
        for tok in ["utc", "GMT", "Z", "zulu"] {
            assert_eq!(fixed(&resolve_zone_arg(tok)?), 0, "{tok}");
        }
        // A DST-varying abbreviation resolves through its reference zone; PG
        // takes it here but rejects `SET TimeZone = 'MSK'`.
        let msk = resolve_zone_arg("MSK")?;
        assert_eq!(offset_for_local(&msk, tm(2011, 3, 27, 1, 0, 0)), 3 * 3600);
        assert!(SessionZone::resolve("MSK").is_err());
        // A full IANA name, and the `<letters><POSIX offset>` form.
        let ny = resolve_zone_arg("America/New_York")?;
        assert_eq!(
            offset_for_local(&ny, tm(2013, 7, 15, 17, 15, 23)),
            -4 * 3600
        );
        assert_eq!(fixed(&resolve_zone_arg("UTC+10")?), -10 * 3600);
        assert_eq!(fixed(&resolve_zone_arg("XYZ5")?), -5 * 3600);
        // An IANA name that ends in a displacement keeps its own, opposite sign.
        assert_eq!(
            offset_for_instant(&resolve_zone_arg("Etc/GMT+5")?, 0),
            -5 * 3600
        );
        for tok in ["", "   ", "Nowhere/Nozone"] {
            assert!(
                matches!(resolve_zone_arg(tok), Err(ZoneError::NotRecognized(_))),
                "{tok:?}"
            );
        }
        Ok(())
    }

    /// A zone *argument* is matched as given: PG does not trim it, so a padded
    /// name is `22023` rather than the zone it names. Only the numeric form
    /// tolerates leading whitespace. Pinned against PG 18.4, where
    /// `AT TIME ZONE 'UTC '` errors while `AT TIME ZONE ' +05:30'` is UTC−5:30.
    #[test]
    fn a_padded_zone_argument_is_not_the_zone() -> anyhow::Result<()> {
        for tok in [
            "UTC ",
            " UTC",
            "  utc  ",
            "America/New_York ",
            " America/New_York",
            "MSK ",
        ] {
            assert!(
                matches!(resolve_zone_arg(tok), Err(ZoneError::NotRecognized(_))),
                "{tok:?}"
            );
        }
        // Leading whitespace on a displacement is skipped; trailing is not —
        // PG reads it as the start of a DST abbreviation, a form we refuse.
        assert_eq!(fixed(&resolve_zone_arg(" +05:30")?), -(5 * 3600 + 30 * 60));
        assert_eq!(fixed(&resolve_zone_arg("\t5")?), -5 * 3600);
        assert!(matches!(
            resolve_zone_arg("+05:30 "),
            Err(ZoneError::NotRecognized(_))
        ));
        // The value reader keeps its own trim: a datetime literal's lexer has
        // already split the token out, so padding never reaches it in practice.
        assert_eq!(fixed(&resolve_zone("  UTC  ")?), 0);
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

    #[test]
    fn standard_offset_ignores_dst_in_either_hemisphere() -> anyhow::Result<()> {
        let off =
            |n: &str| -> anyhow::Result<i32> { Ok(SessionZone::resolve(n)?.standard_offset()) };
        assert_eq!(SessionZone::utc().standard_offset(), 0);
        assert_eq!(
            SessionZone::from_offset_east(7 * 3600)?.standard_offset(),
            7 * 3600
        );
        // Northern: the winter probe is already standard time.
        assert_eq!(off("America/New_York")?, -5 * 3600);
        // Southern: our winter is their DST, so the second probe answers.
        assert_eq!(off("Australia/Sydney")?, 10 * 3600);
        // A zone that never observes DST, and a sub-hour one.
        assert_eq!(off("Asia/Kolkata")?, 5 * 3600 + 1800);

        // Zones whose *standard* offset moved after 2000. Probing a fixed real
        // date rather than the zone's extrapolated rule got all three wrong —
        // Apia by a full day, since it jumped the dateline in 2011.
        assert_eq!(off("Pacific/Apia")?, 13 * 3600);
        assert_eq!(off("Europe/Istanbul")?, 3 * 3600);
        assert_eq!(off("America/Sao_Paulo")?, -3 * 3600);
        Ok(())
    }
}
