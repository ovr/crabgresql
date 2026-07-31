//! Date/time formatting and parsing driven by a PostgreSQL format picture:
//! `to_char(interval|timestamp|timestamptz, text)`, `to_date(text, text)` and
//! `to_timestamp(text, text)`.
//!
//! Clean-room (see AGENTS.md): reproduces PG 18.4's observable output, error
//! text and SQLSTATE, pinned by differential tests — implemented independently.
//!
//! Supported codes: `YYYY YYY YY Y`, `MM`, `DD`, `HH HH12 HH24`, `MI`, `SS`,
//! `MS`, `US`, `AM/PM/A.M./P.M.` and their lowercase spellings, `Month/MONTH/
//! month`, `Mon/MON/mon`, `Day/DAY/day`, `Dy/DY/dy`, `TZ/tz`, `OF`, the `FM`
//! fill-mode prefix, the `TH`/`th` ordinal suffix, `"quoted"` literal text, and
//! passthrough of anything else.
//!
//! Divergences, both deliberate:
//!
//! * The codes `Q W WW IW IYYY IDDD DDD J SSSS RM CC BC AD SP TM D` are not
//!   implemented and pass through as literal text rather than being expanded.
//! * The session display zone is hardcoded to UTC (see `timestamptz::format`),
//!   so `TZ` is always `UTC` and `OF` always `+00`. Under `SET timezone='UTC'`
//!   this is identical to PG.

use crate::interval::{self, Interval};
use crate::timestamp::{self, Tm, tm};

// SQLSTATEs (kept as literals; the types crate does not depend on the protocol
// crate — the binder/executor map these to `sqlstate::*`).
const INVALID_DATETIME_FORMAT: &str = "22007";
const DATETIME_FIELD_OVERFLOW: &str = "22008";

/// A `to_char` / `to_date` / `to_timestamp` failure. Unlike `DateError` and
/// `TimestampError` this carries the optional DETAIL and HINT lines PG prints
/// for a format or field error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatError {
    pub sqlstate: &'static str,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
}

impl FormatError {
    fn new(sqlstate: &'static str, message: String) -> FormatError {
        FormatError {
            sqlstate,
            message,
            detail: None,
            hint: None,
        }
    }

    fn with_detail(mut self, detail: &str) -> FormatError {
        self.detail = Some(detail.to_string());
        self
    }

    fn with_hint(mut self, hint: &str) -> FormatError {
        self.hint = Some(hint.to_string());
        self
    }
}

/// `to_char(interval, …)` rejects the codes that need a calendar date.
fn interval_unsupported() -> FormatError {
    FormatError::new(
        INVALID_DATETIME_FORMAT,
        "invalid format specification for an interval value".to_string(),
    )
    .with_hint("Intervals are not tied to specific calendar dates.")
}

fn invalid_value(raw: &str, code: &str) -> FormatError {
    FormatError::new(
        INVALID_DATETIME_FORMAT,
        format!("invalid value \"{raw}\" for \"{code}\""),
    )
    .with_detail("Value must be an integer.")
}

fn no_match(raw: &str, code: &str) -> FormatError {
    FormatError::new(
        INVALID_DATETIME_FORMAT,
        format!("invalid value \"{raw}\" for \"{code}\""),
    )
    .with_detail("The given value did not match any of the allowed values for this field.")
}

fn twelve_hour(hour: i64) -> FormatError {
    FormatError::new(
        INVALID_DATETIME_FORMAT,
        format!("hour \"{hour}\" is invalid for the 12-hour clock"),
    )
    .with_hint("Use the 24-hour clock, or give an hour between 1 and 12.")
}

fn field_out_of_range(input: &str) -> FormatError {
    FormatError::new(
        DATETIME_FIELD_OVERFLOW,
        format!("date/time field value out of range: \"{input}\""),
    )
}

fn date_out_of_range(input: &str) -> FormatError {
    FormatError::new(
        DATETIME_FIELD_OVERFLOW,
        format!("date out of range: \"{input}\""),
    )
}

// --- the format picture ----------------------------------------------------

/// The letter case a name-valued code renders in, taken from how the code is
/// spelled: `MONTH` → upper, `Month` → capitalized, `month` → lower.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Case {
    Upper,
    Cap,
    Lower,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Code {
    /// `YYYY`/`YYY`/`YY`/`Y`, carrying the digit count.
    Year(u8),
    Month,
    Day,
    Hour24,
    Hour12,
    Minute,
    Second,
    Milli,
    Micro,
    MonthName(Case),
    MonthAbbr(Case),
    DayName(Case),
    DayAbbr(Case),
    /// `AM`/`PM` (`dotted` = the `A.M.` spelling). Which of the two is printed
    /// depends on the value, not on the spelling.
    Meridiem {
        dotted: bool,
        upper: bool,
    },
    /// `TZ`/`tz`.
    TzAbbr {
        upper: bool,
    },
    /// `OF`.
    TzOffset,
}

impl Code {
    /// Whether `to_char(interval, …)` accepts this code. The calendar-bound
    /// codes are an error there, not a silent zero.
    fn valid_for_interval(self) -> bool {
        !matches!(
            self,
            Code::MonthName(_)
                | Code::MonthAbbr(_)
                | Code::DayName(_)
                | Code::DayAbbr(_)
                | Code::TzAbbr { .. }
                | Code::TzOffset
        )
    }

    /// Digits this code consumes when parsing, when the width is capped.
    fn parse_width(self) -> usize {
        match self {
            Code::Year(n) => n as usize,
            Code::Milli => 3,
            Code::Micro => 6,
            _ => 2,
        }
    }
}

/// One element of a parsed picture. Runs of passthrough characters and quoted
/// text are coalesced into a single `Literal`.
enum Node {
    Literal(String),
    Field {
        code: Code,
        /// The code's spelling as tabulated, used in parse error messages.
        spell: &'static str,
        fm: bool,
        /// `Some(upper)` when a `TH`/`th` suffix follows the field.
        th: Option<bool>,
    },
}

/// Codes whose spelling PG matches case-*sensitively*. Longest first.
const EXACT_CODES: &[(&str, Code)] = &[
    ("MONTH", Code::MonthName(Case::Upper)),
    ("Month", Code::MonthName(Case::Cap)),
    ("month", Code::MonthName(Case::Lower)),
    ("MON", Code::MonthAbbr(Case::Upper)),
    ("Mon", Code::MonthAbbr(Case::Cap)),
    ("mon", Code::MonthAbbr(Case::Lower)),
    ("DAY", Code::DayName(Case::Upper)),
    ("Day", Code::DayName(Case::Cap)),
    ("day", Code::DayName(Case::Lower)),
    ("DY", Code::DayAbbr(Case::Upper)),
    ("Dy", Code::DayAbbr(Case::Cap)),
    ("dy", Code::DayAbbr(Case::Lower)),
    (
        "A.M.",
        Code::Meridiem {
            dotted: true,
            upper: true,
        },
    ),
    (
        "P.M.",
        Code::Meridiem {
            dotted: true,
            upper: true,
        },
    ),
    (
        "a.m.",
        Code::Meridiem {
            dotted: true,
            upper: false,
        },
    ),
    (
        "p.m.",
        Code::Meridiem {
            dotted: true,
            upper: false,
        },
    ),
    (
        "AM",
        Code::Meridiem {
            dotted: false,
            upper: true,
        },
    ),
    (
        "PM",
        Code::Meridiem {
            dotted: false,
            upper: true,
        },
    ),
    (
        "am",
        Code::Meridiem {
            dotted: false,
            upper: false,
        },
    ),
    (
        "pm",
        Code::Meridiem {
            dotted: false,
            upper: false,
        },
    ),
    ("TZ", Code::TzAbbr { upper: true }),
    ("tz", Code::TzAbbr { upper: false }),
];

/// Codes PG matches case-*insensitively*. Longest first, so `YYYY` wins over
/// `YY` and `HH24` over `HH`.
const FOLDED_CODES: &[(&str, Code)] = &[
    ("HH24", Code::Hour24),
    ("HH12", Code::Hour12),
    ("YYYY", Code::Year(4)),
    ("YYY", Code::Year(3)),
    ("OF", Code::TzOffset),
    ("HH", Code::Hour12),
    ("YY", Code::Year(2)),
    ("MI", Code::Minute),
    ("SS", Code::Second),
    ("MS", Code::Milli),
    ("US", Code::Micro),
    ("MM", Code::Month),
    ("DD", Code::Day),
    ("Y", Code::Year(1)),
];

/// Match a format code at the start of `s`. The case-sensitive table is tried
/// first: `MOnth` and `aM` are not codes in PG, they pass through verbatim, so
/// the folded table must not be allowed to see them.
fn match_code(s: &[char]) -> Option<(Code, &'static str)> {
    let head: String = s.iter().take(5).collect();
    for (spelling, code) in EXACT_CODES {
        if head.starts_with(spelling) {
            return Some((*code, spelling));
        }
    }
    let folded = head.to_ascii_uppercase();
    for (spelling, code) in FOLDED_CODES {
        if folded.starts_with(spelling) {
            return Some((*code, spelling));
        }
    }
    None
}

/// Split a picture into literal runs and fields. `FM` binds to the field that
/// follows it; a `TH`/`th` immediately after a field is that field's ordinal
/// suffix (elsewhere it is literal text).
fn parse_picture(fmt: &str) -> Vec<Node> {
    let chars: Vec<char> = fmt.chars().collect();
    let mut nodes: Vec<Node> = Vec::new();
    let mut literal = String::new();
    let mut fm = false;
    let mut i = 0;
    while i < chars.len() {
        // Quoted literal text: everything up to the closing quote, verbatim.
        if chars[i] == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                literal.push(chars[i]);
                i += 1;
            }
            i += 1; // skip the closing quote (absent at end of string)
            continue;
        }
        // `FM` is a prefix, not a field: it sets fill mode for the next field.
        if chars[i..].len() >= 2
            && chars[i].eq_ignore_ascii_case(&'F')
            && chars[i + 1].eq_ignore_ascii_case(&'M')
        {
            fm = true;
            i += 2;
            continue;
        }
        if let Some((code, spell)) = match_code(&chars[i..]) {
            if !literal.is_empty() {
                nodes.push(Node::Literal(std::mem::take(&mut literal)));
            }
            i += spell.chars().count();
            let th = match chars.get(i..i + 2) {
                Some(['T', 'H']) => Some(true),
                Some(['t', 'h']) => Some(false),
                _ => None,
            };
            if th.is_some() {
                i += 2;
            }
            nodes.push(Node::Field {
                code,
                spell,
                fm,
                th,
            });
            fm = false;
            continue;
        }
        literal.push(chars[i]);
        i += 1;
    }
    if !literal.is_empty() {
        nodes.push(Node::Literal(literal));
    }
    nodes
}

// --- rendering -------------------------------------------------------------

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
/// Sunday-first, matching `timestamp::j2day`.
const DAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// PG blank-pads the full month and day names to a fixed width.
const NAME_WIDTH: usize = 9;

/// An interval's fields, in the same signed decomposition `date_part` uses
/// (hours are not capped at 24).
struct IvFields {
    year: i64,
    mon: i64,
    day: i64,
    hour: i64,
    min: i64,
    sec: i64,
    fsec: i64,
}

impl IvFields {
    fn of(iv: Interval) -> IvFields {
        let (hour, min, sec, fsec) = interval::split_time(iv.usec);
        IvFields {
            year: (iv.months / 12) as i64,
            mon: (iv.months % 12) as i64,
            day: iv.days as i64,
            hour,
            min,
            sec,
            fsec,
        }
    }
}

/// A timestamp's fields plus its display zone. `zone` is `None` for a plain
/// `timestamp`, where PG renders `TZ` as the empty string but still renders
/// `OF` as `+00`.
struct DtFields {
    /// The *displayed* year: 1-based BC, so astronomical 0 shows as 1.
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    min: i64,
    sec: i64,
    usec: i64,
    /// 0 = Sunday.
    dow: usize,
    zone: Option<&'static str>,
}

impl DtFields {
    fn of(micros: i64, zone: Option<&'static str>) -> DtFields {
        let t = timestamp::decode(micros);
        DtFields {
            year: if t.year <= 0 { 1 - t.year } else { t.year },
            month: t.month,
            day: t.day,
            hour: t.hour,
            min: t.min,
            sec: t.sec,
            usec: t.usec,
            dow: timestamp::j2day(timestamp::date2j(t.year, t.month, t.day)) as usize,
            zone,
        }
    }
}

/// `to_char(interval, fmt)`. `Ok(None)` is SQL NULL (a non-finite interval).
pub fn to_char_interval(iv: Interval, fmt: &str) -> Result<Option<String>, FormatError> {
    if !iv.is_finite() {
        return Ok(None);
    }
    let f = IvFields::of(iv);
    let mut out = String::new();
    for node in &parse_picture(fmt) {
        match node {
            Node::Literal(s) => out.push_str(s),
            Node::Field { code, fm, th, .. } => {
                if !code.valid_for_interval() {
                    return Err(interval_unsupported());
                }
                let (text, value) = render_interval(*code, &f, *fm);
                out.push_str(&text);
                if let Some(upper) = th {
                    out.push_str(ordinal(value, *upper));
                }
            }
        }
    }
    Ok(Some(out))
}

/// `to_char(timestamp, fmt)`. `Ok(None)` is SQL NULL (±infinity).
pub fn to_char_timestamp(micros: i64, fmt: &str) -> Result<Option<String>, FormatError> {
    to_char_dt(micros, fmt, None)
}

/// `to_char(timestamptz, fmt)`. The display zone is UTC (see the module header).
pub fn to_char_timestamptz(micros: i64, fmt: &str) -> Result<Option<String>, FormatError> {
    to_char_dt(micros, fmt, Some("UTC"))
}

fn to_char_dt(
    micros: i64,
    fmt: &str,
    zone: Option<&'static str>,
) -> Result<Option<String>, FormatError> {
    if !timestamp::is_finite(micros) {
        return Ok(None);
    }
    let f = DtFields::of(micros, zone);
    let mut out = String::new();
    for node in &parse_picture(fmt) {
        match node {
            Node::Literal(s) => out.push_str(s),
            Node::Field { code, fm, th, .. } => {
                let (text, value) = render_dt(*code, &f, *fm);
                out.push_str(&text);
                if let Some(upper) = th {
                    out.push_str(ordinal(value, *upper));
                }
            }
        }
    }
    Ok(Some(out))
}

/// Render one code against an interval, returning the text and the numeric
/// value a `TH` suffix would attach to.
fn render_interval(code: Code, f: &IvFields, fm: bool) -> (String, i64) {
    match code {
        Code::Year(n) => (year_digits(f.year, n, fm), f.year),
        Code::Month => (num(f.mon, 2, fm), f.mon),
        // PG quirk: the interval day field alone uses C `%02d` semantics, where
        // a negative sign counts toward the width (`-2`, not `-02`).
        Code::Day => (num_sign_in_width(f.day, 2, fm), f.day),
        Code::Hour24 => (num(f.hour, 2, fm), f.hour),
        Code::Hour12 => {
            let h = hour12(f.hour.abs());
            let h = if f.hour < 0 { -h } else { h };
            (num(h, 2, fm), h)
        }
        Code::Minute => (num(f.min, 2, fm), f.min),
        Code::Second => (num(f.sec, 2, fm), f.sec),
        Code::Milli => (num(f.fsec / 1000, 3, fm), f.fsec / 1000),
        Code::Micro => (num(f.fsec, 6, fm), f.fsec),
        Code::Meridiem { dotted, upper } => (meridiem(f.hour.rem_euclid(24), dotted, upper), 0),
        // Rejected before we get here.
        _ => (String::new(), 0),
    }
}

fn render_dt(code: Code, f: &DtFields, fm: bool) -> (String, i64) {
    match code {
        Code::Year(n) => (year_digits(f.year, n, fm), f.year),
        Code::Month => (num(f.month, 2, fm), f.month),
        Code::Day => (num(f.day, 2, fm), f.day),
        Code::Hour24 => (num(f.hour, 2, fm), f.hour),
        Code::Hour12 => {
            let h = hour12(f.hour);
            (num(h, 2, fm), h)
        }
        Code::Minute => (num(f.min, 2, fm), f.min),
        Code::Second => (num(f.sec, 2, fm), f.sec),
        // Sub-second fields are left-scaled, not right-padded: `.9` is 900 ms.
        Code::Milli => (num(f.usec / 1000, 3, fm), f.usec / 1000),
        Code::Micro => (num(f.usec, 6, fm), f.usec),
        Code::MonthName(case) => (
            pad_name(MONTH_NAMES[(f.month - 1) as usize], case, fm),
            f.month,
        ),
        Code::MonthAbbr(case) => (abbr(MONTH_NAMES[(f.month - 1) as usize], case), f.month),
        Code::DayName(case) => (pad_name(DAY_NAMES[f.dow], case, fm), f.dow as i64),
        Code::DayAbbr(case) => (abbr(DAY_NAMES[f.dow], case), f.dow as i64),
        Code::Meridiem { dotted, upper } => (meridiem(f.hour, dotted, upper), 0),
        Code::TzAbbr { upper } => {
            let z = f.zone.unwrap_or("");
            (
                if upper {
                    z.to_ascii_uppercase()
                } else {
                    z.to_ascii_lowercase()
                },
                0,
            )
        }
        Code::TzOffset => ("+00".to_string(), 0),
    }
}

/// The 12-hour clock reading of a 24-hour hour.
fn hour12(hour: i64) -> i64 {
    let h = hour % 12;
    if h == 0 { 12 } else { h }
}

fn meridiem(hour: i64, dotted: bool, upper: bool) -> String {
    let s = match (hour < 12, dotted, upper) {
        (true, false, true) => "AM",
        (true, false, false) => "am",
        (true, true, true) => "A.M.",
        (true, true, false) => "a.m.",
        (false, false, true) => "PM",
        (false, false, false) => "pm",
        (false, true, true) => "P.M.",
        (false, true, false) => "p.m.",
    };
    s.to_string()
}

/// `YYYY` prints the whole year; the shorter spellings print its last `n`
/// digits, zero-padded.
fn year_digits(year: i64, n: u8, fm: bool) -> String {
    let width = n as usize;
    if n >= 4 {
        return num(year, width, fm);
    }
    let unit = 10i64.pow(u32::from(n));
    num(year.abs() % unit * year.signum(), width, fm)
}

fn pad_name(name: &str, case: Case, fm: bool) -> String {
    let cased = recase(name, case);
    if fm {
        cased
    } else {
        format!("{cased:<NAME_WIDTH$}")
    }
}

/// Abbreviations are the first three letters and are never blank-padded.
fn abbr(name: &str, case: Case) -> String {
    recase(&name[..3], case)
}

fn recase(name: &str, case: Case) -> String {
    match case {
        Case::Upper => name.to_ascii_uppercase(),
        Case::Lower => name.to_ascii_lowercase(),
        Case::Cap => name.to_string(),
    }
}

/// The English ordinal suffix for `value`, uppercased when the picture spelled
/// the suffix `TH` rather than `th`.
fn ordinal(value: i64, upper: bool) -> &'static str {
    let v = value.unsigned_abs();
    let suffix = if (11..=13).contains(&(v % 100)) {
        "th"
    } else {
        match v % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    };
    if upper {
        match suffix {
            "st" => "ST",
            "nd" => "ND",
            "rd" => "RD",
            _ => "TH",
        }
    } else {
        suffix
    }
}

/// Render a signed field value: a leading `-` for negatives, then the absolute
/// value zero-padded to `width` (or unpadded when `fm`).
fn num(value: i64, width: usize, fm: bool) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let abs = value.unsigned_abs();
    if fm {
        format!("{sign}{abs}")
    } else {
        format!("{sign}{abs:0width$}")
    }
}

/// Like [`num`], but with C `%0*d` semantics where the sign counts toward the
/// field width (`-2` at width 2, not `-02`). PG uses this only for interval `DD`.
fn num_sign_in_width(value: i64, width: usize, fm: bool) -> String {
    if fm {
        format!("{value}")
    } else {
        format!("{value:0width$}")
    }
}

// --- parsing ---------------------------------------------------------------

/// The fields collected while scanning an input against a picture. Everything
/// is optional: PG defaults a missing field rather than failing.
#[derive(Default)]
struct Parsed {
    /// Already run through [`complete_year`].
    year: Option<i64>,
    month: Option<i64>,
    day: Option<i64>,
    hour24: Option<i64>,
    hour12: Option<i64>,
    pm: Option<bool>,
    min: Option<i64>,
    sec: Option<i64>,
    /// Already scaled to microseconds.
    usec: Option<i64>,
    /// Seconds east of UTC, from `TZ`/`OF`.
    offset: Option<i32>,
}

/// `to_timestamp(text, text)` → microseconds since 2000-01-01 UTC.
pub fn from_char_timestamptz(input: &str, fmt: &str) -> Result<i64, FormatError> {
    let (t, offset) = from_char(input, fmt)?;
    // PG's timestamp range, 4713 BC .. 294276 AD. Checked before the field
    // ranges, as `timestamp::parse` does.
    if !(-4712..=294_276).contains(&t.year) {
        return Err(field_out_of_range(input));
    }
    timestamp::validate_fields(&t, input).map_err(|e| FormatError::new(e.sqlstate, e.message))?;
    Ok(timestamp::encode(t) - i64::from(offset) * 1_000_000)
}

/// `to_date(text, text)` → days since 2000-01-01. The `date` range runs far
/// past `timestamp`'s, so this bounds the Julian day rather than the year.
pub fn from_char_date(input: &str, fmt: &str) -> Result<i32, FormatError> {
    let (t, _) = from_char(input, fmt)?;
    let jd = timestamp::date2j(t.year, t.month, t.day);
    if !(0..=JULIAN_MAX).contains(&jd) {
        return Err(date_out_of_range(input));
    }
    timestamp::validate_fields(&t, input).map_err(|e| FormatError::new(e.sqlstate, e.message))?;
    let days = jd - timestamp::POSTGRES_EPOCH_JDATE;
    i32::try_from(days).map_err(|_| date_out_of_range(input))
}

/// The largest Julian day a `date` can hold (`5874897-12-31`).
const JULIAN_MAX: i64 = 2_147_483_494;

/// Scan `input` against `fmt`, returning the assembled calendar time and the
/// zone offset (seconds east of UTC) if the picture carried one.
fn from_char(input: &str, fmt: &str) -> Result<(Tm, i32), FormatError> {
    let nodes = parse_picture(fmt);
    let chars: Vec<char> = input.chars().collect();
    let mut pos = 0usize;
    let mut p = Parsed::default();

    for (idx, node) in nodes.iter().enumerate() {
        match node {
            Node::Literal(s) => scan_literal(s, &chars, &mut pos),
            Node::Field {
                code,
                spell,
                fm,
                th,
                ..
            } => {
                // A field's width is only capped when another field follows it
                // directly: `to_date('2024','YY')` reads all four digits, but
                // `'YYYYMMDD'` splits `'20240305'` into fixed-width chunks.
                let capped = !fm && matches!(nodes.get(idx + 1), Some(Node::Field { .. }));
                scan_field(*code, spell, capped, &chars, &mut pos, &mut p)?;
                if th.is_some() {
                    scan_ordinal(&chars, &mut pos);
                }
            }
        }
    }

    assemble(&p)
}

/// Literal picture text only skips input: whitespace is consumed freely, and
/// each non-alphanumeric picture character eats one non-alphanumeric input
/// character if present. This is what lets `'2024/03/05'` match `'YYYY-MM-DD'`.
fn scan_literal(s: &str, chars: &[char], pos: &mut usize) {
    for c in s.chars() {
        while chars.get(*pos).is_some_and(|c| c.is_whitespace()) {
            *pos += 1;
        }
        if !c.is_alphanumeric() && chars.get(*pos).is_some_and(|c| !c.is_alphanumeric()) {
            *pos += 1;
        }
    }
}

fn skip_space(chars: &[char], pos: &mut usize) {
    while chars.get(*pos).is_some_and(|c| c.is_whitespace()) {
        *pos += 1;
    }
}

/// Consume an `st`/`nd`/`rd`/`th` suffix if one is there.
fn scan_ordinal(chars: &[char], pos: &mut usize) {
    let two: String = chars
        .iter()
        .skip(*pos)
        .take(2)
        .collect::<String>()
        .to_ascii_lowercase();
    if matches!(two.as_str(), "st" | "nd" | "rd" | "th") {
        *pos += 2;
    }
}

fn scan_field(
    code: Code,
    spell: &str,
    capped: bool,
    chars: &[char],
    pos: &mut usize,
    p: &mut Parsed,
) -> Result<(), FormatError> {
    // Input that runs out mid-picture is not an error: the remaining fields
    // simply keep their defaults (`to_date('2024', 'YYYY-MM-DD')` is Jan 1st).
    skip_space(chars, pos);
    if *pos >= chars.len() {
        return Ok(());
    }
    match code {
        // The full-name and abbreviation codes are not interchangeable on
        // input: `MON` consumes exactly three characters (so `MARCH` leaves
        // `CH` behind for the next field), and `MONTH` only matches a full name.
        Code::MonthName(_) => {
            let word = take_alpha(chars, pos);
            p.month = Some(match_full(&word, &MONTH_NAMES).ok_or_else(|| no_match(&word, spell))?);
        }
        Code::MonthAbbr(_) => {
            let word = take_exact(chars, pos, 3);
            p.month = Some(match_abbr(&word, &MONTH_NAMES).ok_or_else(|| no_match(&word, spell))?);
        }
        // The weekday is checked for validity but does not affect the date,
        // matching PG.
        Code::DayName(_) => {
            let word = take_alpha(chars, pos);
            match_full(&word, &DAY_NAMES).ok_or_else(|| no_match(&word, spell))?;
        }
        Code::DayAbbr(_) => {
            let word = take_exact(chars, pos, 3);
            match_abbr(&word, &DAY_NAMES).ok_or_else(|| no_match(&word, spell))?;
        }
        Code::Meridiem { .. } => {
            let word = take_meridiem(chars, pos);
            p.pm = Some(match word.as_str() {
                "am" | "a.m." => false,
                "pm" | "p.m." => true,
                _ => return Err(no_match(&word, spell)),
            });
        }
        Code::TzAbbr { .. } => {
            let word = take_alpha(chars, pos);
            let zone = crate::tz::resolve_zone(&word).map_err(|_| no_match(&word, spell))?;
            p.offset = Some(crate::tz::offset_for_instant(&zone, 0));
        }
        Code::TzOffset => {
            p.offset = Some(take_offset(chars, pos).ok_or_else(|| no_match("", spell))?);
        }
        _ => {
            let width = if capped {
                Some(code.parse_width())
            } else {
                None
            };
            let (value, digits) = take_number(chars, pos, width, spell)?;
            match code {
                Code::Year(n) => p.year = Some(complete_year(value, digits, n)),
                Code::Month => p.month = Some(value),
                Code::Day => p.day = Some(value),
                Code::Hour24 => p.hour24 = Some(value),
                Code::Hour12 => p.hour12 = Some(value),
                Code::Minute => p.min = Some(value),
                Code::Second => p.sec = Some(value),
                // Sub-second values are left-aligned: `'12'` under `US` is
                // 0.12 s, not 12 µs.
                Code::Milli => p.usec = Some(scale(value, digits, 3) * 1000),
                Code::Micro => p.usec = Some(scale(value, digits, 6)),
                _ => {}
            }
        }
    }
    Ok(())
}

/// Left-align `value` in a `width`-digit field.
fn scale(value: i64, digits: u32, width: u32) -> i64 {
    if digits >= width {
        value
    } else {
        value * 10i64.pow(width - digits)
    }
}

fn take_alpha(chars: &[char], pos: &mut usize) -> String {
    skip_space(chars, pos);
    let start = *pos;
    while chars.get(*pos).is_some_and(|c| c.is_alphabetic()) {
        *pos += 1;
    }
    chars[start..*pos].iter().collect()
}

/// `AM`/`PM` or the dotted `A.M.`/`P.M.` spelling, folded to lowercase.
fn take_meridiem(chars: &[char], pos: &mut usize) -> String {
    skip_space(chars, pos);
    let four: String = chars
        .iter()
        .skip(*pos)
        .take(4)
        .collect::<String>()
        .to_ascii_lowercase();
    for candidate in ["a.m.", "p.m.", "am", "pm"] {
        if four.starts_with(candidate) {
            *pos += candidate.len();
            return candidate.to_string();
        }
    }
    take_alpha(chars, pos).to_ascii_lowercase()
}

/// `±HH`, `±HHMM` or `±HH:MM`, as seconds east of UTC.
fn take_offset(chars: &[char], pos: &mut usize) -> Option<i32> {
    skip_space(chars, pos);
    let sign = match chars.get(*pos) {
        Some('+') => 1,
        Some('-') => -1,
        _ => return None,
    };
    *pos += 1;
    let (hours, _) = take_digits(chars, pos, Some(2))?;
    let mut secs = hours as i32 * 3600;
    if chars.get(*pos) == Some(&':') {
        *pos += 1;
    }
    if chars.get(*pos).is_some_and(|c| c.is_ascii_digit())
        && let Some((mins, _)) = take_digits(chars, pos, Some(2))
    {
        secs += mins as i32 * 60;
    }
    Some(sign * secs)
}

fn take_number(
    chars: &[char],
    pos: &mut usize,
    width: Option<usize>,
    spell: &str,
) -> Result<(i64, u32), FormatError> {
    skip_space(chars, pos);
    let negative = chars.get(*pos) == Some(&'-');
    if negative {
        *pos += 1;
    }
    match take_digits(chars, pos, width) {
        Some((value, digits)) => Ok((if negative { -value } else { value }, digits)),
        None => {
            // PG quotes the raw input slice, truncated to the field's width.
            let raw: String = chars
                .iter()
                .skip(*pos)
                .take(width.unwrap_or(spell.len()))
                .collect();
            Err(invalid_value(&raw, spell))
        }
    }
}

fn take_digits(chars: &[char], pos: &mut usize, width: Option<usize>) -> Option<(i64, u32)> {
    let start = *pos;
    let limit = width.map_or(usize::MAX, |w| start + w);
    let mut value: i64 = 0;
    while *pos < limit {
        match chars.get(*pos) {
            Some(c) if c.is_ascii_digit() => {
                value = value
                    .checked_mul(10)?
                    .checked_add(i64::from(*c as u8 - b'0'))?;
                *pos += 1;
            }
            _ => break,
        }
    }
    if *pos == start {
        return None;
    }
    Some((value, (*pos - start) as u32))
}

/// Take exactly `n` characters, however few are left.
fn take_exact(chars: &[char], pos: &mut usize, n: usize) -> String {
    skip_space(chars, pos);
    let end = (*pos + n).min(chars.len());
    let word: String = chars[*pos..end].iter().collect();
    *pos = end;
    word
}

/// Case-insensitive match of a full name, returning the 1-based index.
fn match_full(word: &str, names: &[&str]) -> Option<i64> {
    let w = word.to_ascii_lowercase();
    if w.is_empty() {
        return None;
    }
    names
        .iter()
        .position(|n| n.to_ascii_lowercase() == w)
        .map(|i| i as i64 + 1)
}

/// Case-insensitive match of a three-letter abbreviation.
fn match_abbr(word: &str, names: &[&str]) -> Option<i64> {
    let w = word.to_ascii_lowercase();
    if w.len() != 3 {
        return None;
    }
    names
        .iter()
        .position(|n| n[..3].to_ascii_lowercase() == w)
        .map(|i| i as i64 + 1)
}

/// PG's short-year completion. `YYYY` never completes, and neither does any
/// input that spelled out four or more digits (`to_date('0070','YYY')` is year
/// 70). Otherwise the window is picked from the *value's* magnitude, not from
/// the code's width or the input's digit count: a value below 100 lands in
/// `[1970, 2069]` and one below 1000 in `[1520, 2519]` — the same rule
/// centered on 2020, at two scales.
fn complete_year(value: i64, digits: u32, code_width: u8) -> i64 {
    if code_width >= 4 || digits >= 4 || value < 0 {
        return value;
    }
    let unit = if value < 100 {
        100
    } else if value < 1000 {
        1000
    } else {
        return value;
    };
    let base = 2020 - unit / 2;
    base + (value - base).rem_euclid(unit)
}

fn assemble(p: &Parsed) -> Result<(Tm, i32), FormatError> {
    // PG's default is astronomical year 0, i.e. 1 BC.
    let year = p.year.unwrap_or(0);
    let hour = match (p.hour24, p.hour12) {
        (Some(h), _) => h,
        (None, Some(h)) => {
            if !(1..=12).contains(&h) {
                return Err(twelve_hour(h));
            }
            let h = h % 12;
            if p.pm == Some(true) { h + 12 } else { h }
        }
        (None, None) => 0,
    };
    let t = tm(
        year,
        p.month.unwrap_or(1),
        p.day.unwrap_or(1),
        hour,
        p.min.unwrap_or(0),
        p.sec.unwrap_or(0),
        p.usec.unwrap_or(0),
    );
    Ok((t, p.offset.unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interval::parse as parse_interval;
    use crate::timestamp::parse as parse_ts;

    fn iv(s: &str, fmt: &str) -> String {
        let value = match parse_interval(s) {
            Ok(value) => value,
            Err(error) => panic!("invalid interval test fixture `{s}`: {error:?}"),
        };
        match to_char_interval(value, fmt) {
            Ok(Some(output)) => output,
            other => panic!("unexpected to_char(interval) result for `{fmt}`: {other:?}"),
        }
    }

    fn ts(s: &str, fmt: &str) -> String {
        let value = match parse_ts(s) {
            Ok(value) => value,
            Err(error) => panic!("invalid timestamp test fixture `{s}`: {error:?}"),
        };
        match to_char_timestamp(value, fmt) {
            Ok(Some(output)) => output,
            other => panic!("unexpected to_char(timestamp) result for `{fmt}`: {other:?}"),
        }
    }

    fn tstz(s: &str, fmt: &str) -> String {
        let value = match parse_ts(s) {
            Ok(value) => value,
            Err(error) => panic!("invalid timestamp test fixture `{s}`: {error:?}"),
        };
        match to_char_timestamptz(value, fmt) {
            Ok(Some(output)) => output,
            other => panic!("unexpected to_char(timestamptz) result for `{fmt}`: {other:?}"),
        }
    }

    const STAMP: &str = "2024-03-05 14:07:09.987654";

    /// Unwrap the error side of a result; `unwrap_err` is disallowed here.
    fn expect_err<T: std::fmt::Debug>(result: Result<T, FormatError>) -> FormatError {
        match result {
            Err(e) => e,
            Ok(value) => panic!("expected an error, got {value:?}"),
        }
    }

    #[test]
    fn interval_codes() {
        assert_eq!(
            iv("1 year 2 mons 3 days 04:05:06", "YYYY-MM-DD HH24:MI:SS"),
            "0001-02-03 04:05:06"
        );
        assert_eq!(
            iv("1 day 02:03:04.567", "HH24:MI:SS.MS.US"),
            "02:03:04.567.567000"
        );
        assert_eq!(iv("5 days", "DD"), "05");
        assert_eq!(iv("-1 day -02:00:00", "HH24:MI:SS"), "-02:00:00");
        assert_eq!(iv("25 hours", "HH24"), "25");
        assert_eq!(iv("25 hours", "HH12"), "01");
        assert_eq!(
            iv("1 day 02:03:04", "\"time is\" HH24:MI:SS"),
            "time is 02:03:04"
        );
        assert_eq!(iv("1 day 02:03:04", "FMHH24:FMMI:FMSS"), "2:3:4");
        assert_eq!(iv("90 minutes", "MI"), "30");
    }

    #[test]
    fn interval_year_widths_and_meridiem() {
        assert_eq!(
            iv(
                "1 year 2 mons 3 days 14:05:06",
                "YYYY|YYY|YY|Y|AM|A.M.|pm|DDTH|MMth"
            ),
            "0001|001|01|1|PM|P.M.|pm|03RD|02nd"
        );
        assert_eq!(iv("-1 year", "YYYY|YYY|YY|Y"), "-0001|-001|-01|-1");
    }

    #[test]
    fn interval_negative_field_padding() {
        // DD counts the sign in its width (`-2`), other fields sign-then-pad.
        assert_eq!(iv("-2 days -3 hours", "DD HH24"), "-2 -03");
        assert_eq!(iv("-5 mons", "MM"), "-05");
        assert_eq!(iv("-1 year -2 mons", "YYYY MM"), "-0001 -02");
        assert_eq!(iv("-40 days", "DD"), "-40");
    }

    #[test]
    fn interval_rejects_calendar_codes() {
        for fmt in [
            "Month", "MONTH", "Mon", "MON", "Day", "DAY", "Dy", "DY", "TZ", "tz", "OF",
        ] {
            let zero = Interval {
                months: 0,
                days: 0,
                usec: 0,
            };
            let err = match to_char_interval(zero, fmt) {
                Err(e) => e,
                other => panic!("expected an error for `{fmt}`, got {other:?}"),
            };
            assert_eq!(err.sqlstate, "22007");
            assert_eq!(
                err.message,
                "invalid format specification for an interval value"
            );
            assert_eq!(
                err.hint.as_deref(),
                Some("Intervals are not tied to specific calendar dates.")
            );
        }
    }

    #[test]
    fn infinite_is_null() {
        assert_eq!(
            to_char_interval(crate::interval::POS_INFINITY, "HH24"),
            Ok(None)
        );
        assert_eq!(to_char_timestamp(timestamp::POS_INFINITY, "YYYY"), Ok(None));
        assert_eq!(
            to_char_timestamptz(timestamp::NEG_INFINITY, "YYYY"),
            Ok(None)
        );
    }

    #[test]
    fn datetime_numeric_codes() {
        assert_eq!(
            ts(STAMP, "YYYY|YYY|YY|Y|MM|DD|HH|HH12|HH24|MI|SS|MS|US"),
            "2024|024|24|4|03|05|02|02|14|07|09|987|987654"
        );
    }

    #[test]
    fn fractions_truncate_and_scale() {
        assert_eq!(ts("2024-03-05 14:07:09.999999", "HH24:MI:SS"), "14:07:09");
        assert_eq!(ts("2024-03-05 14:07:09.9", "MS|US"), "900|900000");
    }

    #[test]
    fn names_and_case() {
        assert_eq!(
            ts(STAMP, "Month|MONTH|month|Mon|MON|mon|Day|DAY|day|Dy|DY|dy"),
            "March    |MARCH    |march    |Mar|MAR|mar|Tuesday  |TUESDAY  |tuesday  |Tue|TUE|tue"
        );
        // Only the exact spellings are codes; anything else passes through.
        assert_eq!(
            ts(STAMP, "[MOnth][moNTH][MOn][aM][Am]"),
            "[MOnth][moNTH][MOn][aM][Am]"
        );
    }

    #[test]
    fn fill_mode() {
        assert_eq!(ts(STAMP, "FMYYYY FMMM FMDD"), "2024 3 5");
        assert_eq!(
            ts(STAMP, "FMMonth|FMDay|FMMon|FMDy"),
            "March|Tuesday|Mar|Tue"
        );
    }

    #[test]
    fn meridiem_codes() {
        assert_eq!(
            ts(
                "2024-03-05 00:30:00",
                "HH|HH12|AM|am|A.M.|a.m.|PM|pm|P.M.|p.m."
            ),
            "12|12|AM|am|A.M.|a.m.|AM|am|A.M.|a.m."
        );
        assert_eq!(
            ts("2024-03-05 13:30:00", "AM|am|A.M.|a.m.|PM|pm|P.M.|p.m."),
            "PM|pm|P.M.|p.m.|PM|pm|P.M.|p.m."
        );
    }

    #[test]
    fn ordinal_suffix() {
        assert_eq!(
            ts(STAMP, "DDTH|DDth|MMth|HH24th|FMDDTH"),
            "05TH|05th|03rd|14th|5TH"
        );
    }

    #[test]
    fn bc_years_use_the_display_year() {
        assert_eq!(
            ts("0001-01-01 BC", "YYYY|YYY|YY|Y|MM|DD"),
            "0001|001|01|1|01|01"
        );
        assert_eq!(ts("4713-01-01 BC", "YYYY"), "4713");
    }

    #[test]
    fn zone_codes() {
        assert_eq!(tstz(STAMP, "TZ|tz|OF"), "UTC|utc|+00");
        // A plain timestamp has no zone abbreviation, but still reports +00.
        assert_eq!(ts(STAMP, "[TZ][OF]"), "[][+00]");
    }

    #[test]
    fn quoted_and_passthrough() {
        assert_eq!(ts(STAMP, "\\HH24 \"q\"\" YYYY\""), "\\14 q YYYY");
    }

    // --- parsing -----------------------------------------------------------

    fn to_date(input: &str, fmt: &str) -> String {
        match from_char_date(input, fmt) {
            Ok(d) => crate::date::format(d),
            Err(e) => panic!("unexpected to_date error for `{input}`/`{fmt}`: {e:?}"),
        }
    }

    fn to_ts(input: &str, fmt: &str) -> String {
        match from_char_timestamptz(input, fmt) {
            Ok(m) => crate::timestamptz::format(m),
            Err(e) => panic!("unexpected to_timestamp error for `{input}`/`{fmt}`: {e:?}"),
        }
    }

    #[test]
    fn parse_defaults() {
        assert_eq!(to_date("", "YYYY-MM-DD"), "0001-01-01 BC");
        assert_eq!(to_date("2024", "YYYY"), "2024-01-01");
        assert_eq!(to_date("2024-02", "YYYY-MM"), "2024-02-01");
        assert_eq!(
            to_ts("2024-03-05", "YYYY-MM-DD HH24:MI:SS"),
            "2024-03-05 00:00:00+00"
        );
    }

    #[test]
    fn parse_year_completion() {
        for (input, fmt, expected) in [
            ("5", "Y", "2005-01-01"),
            ("5", "YY", "2005-01-01"),
            ("7", "YYY", "2007-01-01"),
            ("69", "YY", "2069-01-01"),
            ("70", "YY", "1970-01-01"),
            ("070", "YYY", "1970-01-01"),
            ("515", "YYY", "2515-01-01"),
            ("520", "YYY", "1520-01-01"),
            ("995", "YYY", "1995-01-01"),
            // Four or more spelled-out digits, or a `YYYY` code, never complete.
            ("0070", "YYYY", "0070-01-01"),
            ("0070", "YYY", "0070-01-01"),
            ("2024", "YYY", "2024-01-01"),
            ("70", "YYYY", "0070-01-01"),
            // The window comes from the value, not the code: `Y` sees 150.
            ("150", "Y", "2150-01-01"),
        ] {
            assert_eq!(to_date(input, fmt), expected, "{input} / {fmt}");
        }
    }

    #[test]
    fn parse_field_widths() {
        assert_eq!(to_date("20240305", "YYYYMMDD"), "2024-03-05");
        // Greedy when no field follows: `YY` eats all four digits.
        assert_eq!(to_date("2024", "YY"), "2024-01-01");
        assert_eq!(to_date("2024-01", "YY-MM"), "2024-01-01");
        // Fixed widths split `2024|30|5`, and month 30 is out of range.
        assert_eq!(
            expect_err(from_char_date("2024305", "YYYYMMDD")).sqlstate,
            "22008"
        );
        // `FM` forces greedy even when a field follows, so `YYYY` swallows the
        // lot and the (valid, if remote) year 2024305 comes out.
        assert_eq!(to_date("2024305", "FMYYYYMMDD"), "2024305-01-01");
    }

    #[test]
    fn parse_separators_and_garbage() {
        assert_eq!(to_date("2024/03/05", "YYYY-MM-DD"), "2024-03-05");
        assert_eq!(to_date("2024-01-01xyz", "YYYY-MM-DD"), "2024-01-01");
        assert_eq!(to_date("  2024   03 ", "YYYY MM"), "2024-03-01");
    }

    #[test]
    fn parse_names_and_meridiem() {
        assert_eq!(to_date("05 MARCH 2024", "DD MONTH YYYY"), "2024-03-05");
        assert_eq!(to_date("05 march 2024", "DD Month YYYY"), "2024-03-05");
        assert_eq!(to_date("05 mar 2024", "DD Mon YYYY"), "2024-03-05");
        assert_eq!(to_date("2024 TUE 05 03", "YYYY DY DD MM"), "2024-03-05");
        // `Mon` takes exactly three characters, so a full name spills over.
        assert_eq!(
            expect_err(from_char_date("05 MARCH 2024", "DD MON YYYY")).message,
            "invalid value \"CH 2\" for \"YYYY\""
        );
        assert_eq!(to_date("5th 2024", "DDTH YYYY"), "2024-01-05");
        // No year in the picture, so the default (1 BC) year shows through.
        assert_eq!(to_ts("12 00", "HH12 MI"), "0001-01-01 00:00:00+00 BC");
        assert_eq!(to_ts("12 00 PM", "HH12 MI AM"), "0001-01-01 12:00:00+00 BC");
    }

    #[test]
    fn parse_fractions() {
        assert_eq!(to_ts("2024 3", "YYYY MS"), "2024-01-01 00:00:00.3+00");
        assert_eq!(to_ts("2024 12", "YYYY US"), "2024-01-01 00:00:00.12+00");
        assert_eq!(
            to_ts("2024 123456", "YYYY US"),
            "2024-01-01 00:00:00.123456+00"
        );
    }

    #[test]
    fn parse_errors() {
        let e = expect_err(from_char_date("2024-XX-05", "YYYY-MM-DD"));
        assert_eq!(e.sqlstate, "22007");
        assert_eq!(e.message, "invalid value \"XX\" for \"MM\"");
        assert_eq!(e.detail.as_deref(), Some("Value must be an integer."));

        let e = expect_err(from_char_date("garbage", "YYYY-MM-DD"));
        assert_eq!(e.message, "invalid value \"garb\" for \"YYYY\"");

        let e = expect_err(from_char_timestamptz("abc", "Mon"));
        assert_eq!(e.sqlstate, "22007");
        assert_eq!(e.message, "invalid value \"abc\" for \"Mon\"");
        assert_eq!(
            e.detail.as_deref(),
            Some("The given value did not match any of the allowed values for this field.")
        );

        let e = expect_err(from_char_timestamptz(
            "2024-03-05 13:00 PM",
            "YYYY-MM-DD HH12:MI AM",
        ));
        assert_eq!(e.sqlstate, "22007");
        assert_eq!(e.message, "hour \"13\" is invalid for the 12-hour clock");
        assert!(e.hint.is_some());

        let e = expect_err(from_char_timestamptz(
            "2024-03-05 25:00",
            "YYYY-MM-DD HH24:MI",
        ));
        assert_eq!(e.sqlstate, "22008");
        assert_eq!(
            e.message,
            "date/time field value out of range: \"2024-03-05 25:00\""
        );

        let e = expect_err(from_char_date("2024-02-30", "YYYY-MM-DD"));
        assert_eq!(e.sqlstate, "22008");
    }
}
