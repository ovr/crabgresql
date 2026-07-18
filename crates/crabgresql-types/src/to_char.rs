//! `to_char` formatting. Currently covers the `interval` overload: the numeric
//! field codes (`YYYY`/`MM`/`DD`/`HH24`/`HH12`/`MI`/`SS`/`MS`/`US`), the `FM`
//! fill-mode prefix that strips leading zeros, `"quoted"` literal text, and
//! passthrough of any other character.
//!
//! Clean-room (see AGENTS.md): reproduces PG 18.4's observable `to_char` output
//! for intervals, pinned by differential tests — implemented independently.

use crate::interval::{self, Interval};

/// `to_char(interval, fmt)`. Returns `None` (SQL NULL) for a non-finite
/// interval, matching PG.
pub fn interval(iv: Interval, fmt: &str) -> Option<String> {
    if !iv.is_finite() {
        return None;
    }
    let f = Fields::of(iv);
    let mut out = String::new();
    let mut fm = false;
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // Quoted literal text: everything up to the closing quote, verbatim.
        if chars[i] == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                out.push(chars[i]);
                i += 1;
            }
            i += 1; // skip closing quote
            continue;
        }
        if let Some((code, len)) = match_code(&chars[i..]) {
            if code == Code::Fm {
                fm = true;
                i += len;
                continue;
            }
            out.push_str(&render(code, &f, fm));
            fm = false;
            i += len;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    Some(out)
}

/// The interval's fields, in the same signed decomposition `date_part` uses
/// (hours are not capped at 24).
struct Fields {
    year: i64,
    mon: i64,
    day: i64,
    hour: i64,
    min: i64,
    sec: i64,
    fsec: i64,
}

impl Fields {
    fn of(iv: Interval) -> Fields {
        let (hour, min, sec, fsec) = interval::split_time(iv.usec);
        Fields {
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Code {
    Fm,
    Year4,
    Month,
    Day,
    Hour24,
    Hour12,
    Minute,
    Second,
    Milli,
    Micro,
}

/// Match a format code at the start of `s`, longest first. Returns the code and
/// how many characters it consumed. Matching is case-insensitive.
fn match_code(s: &[char]) -> Option<(Code, usize)> {
    // (uppercased spelling, code); order matters — longer spellings first.
    const CODES: &[(&str, Code)] = &[
        ("HH24", Code::Hour24),
        ("HH12", Code::Hour12),
        ("YYYY", Code::Year4),
        ("HH", Code::Hour12),
        ("MI", Code::Minute),
        ("SS", Code::Second),
        ("MS", Code::Milli),
        ("US", Code::Micro),
        ("MM", Code::Month),
        ("DD", Code::Day),
        ("FM", Code::Fm),
    ];
    let upper: String = s.iter().take(4).collect::<String>().to_ascii_uppercase();
    for (spelling, code) in CODES {
        if upper.starts_with(spelling) {
            return Some((*code, spelling.len()));
        }
    }
    None
}

fn render(code: Code, f: &Fields, fm: bool) -> String {
    match code {
        Code::Fm => String::new(),
        Code::Year4 => num(f.year, 4, fm),
        Code::Month => num(f.mon, 2, fm),
        // PG quirk: the interval day field alone uses C `%02d` semantics, where
        // a negative sign counts toward the width (`-2`, not `-02`).
        Code::Day => num_sign_in_width(f.day, 2, fm),
        Code::Hour24 => num(f.hour, 2, fm),
        Code::Hour12 => {
            let h = f.hour.abs() % 12;
            let h = if h == 0 { 12 } else { h };
            num(if f.hour < 0 { -h } else { h }, 2, fm)
        }
        Code::Minute => num(f.min, 2, fm),
        Code::Second => num(f.sec, 2, fm),
        Code::Milli => num(f.fsec / 1000, 3, fm),
        Code::Micro => num(f.fsec, 6, fm),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interval::parse;

    fn tc(s: &str, fmt: &str) -> String {
        interval(parse(s).unwrap(), fmt).unwrap()
    }

    #[test]
    fn codes() {
        assert_eq!(tc("1 year 2 mons 3 days 04:05:06", "YYYY-MM-DD HH24:MI:SS"), "0001-02-03 04:05:06");
        assert_eq!(tc("1 day 02:03:04.567", "HH24:MI:SS.MS.US"), "02:03:04.567.567000");
        assert_eq!(tc("5 days", "DD"), "05");
        assert_eq!(tc("-1 day -02:00:00", "HH24:MI:SS"), "-02:00:00");
        assert_eq!(tc("25 hours", "HH24"), "25");
        assert_eq!(tc("25 hours", "HH12"), "01");
        assert_eq!(tc("1 day 02:03:04", "\"time is\" HH24:MI:SS"), "time is 02:03:04");
        assert_eq!(tc("1 day 02:03:04", "FMHH24:FMMI:FMSS"), "2:3:4");
        assert_eq!(tc("90 minutes", "MI"), "30");
    }

    #[test]
    fn negative_field_padding() {
        // DD counts the sign in its width (`-2`), other fields sign-then-pad.
        assert_eq!(tc("-2 days -3 hours", "DD HH24"), "-2 -03");
        assert_eq!(tc("-5 mons", "MM"), "-05");
        assert_eq!(tc("-1 year -2 mons", "YYYY MM"), "-0001 -02");
        assert_eq!(tc("-40 days", "DD"), "-40");
    }

    #[test]
    fn infinite_is_null() {
        assert_eq!(interval(crate::interval::POS_INFINITY, "HH24"), None);
    }
}
