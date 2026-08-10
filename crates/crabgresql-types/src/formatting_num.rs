//! Number formatting and parsing driven by a PostgreSQL picture:
//! `to_char(numeric|int|float, text)` and `to_number(text, text)`.
//!
//! Clean-room (see AGENTS.md): reproduces PG 18.4's observable output, error
//! text and SQLSTATE, pinned by differential tests — implemented independently.
//!
//! Supported codes: `9 0 . D , G S MI PL SG PR L V RN EEEE TH/th FM`, plus
//! `"quoted"` text and passthrough of anything else. `B` (blank-when-zero) and
//! `C` (ISO currency) are recognized so they do not print as literal letters,
//! but they always render as nothing. For `B` that is PG's behavior too: it
//! prints a zero value identically with and without it, `to_char(0, 'B0999')`
//! is ` 0000` either way.
//!
//! `L`, `D` and `G` are fixed to `$`, `.` and `,`, the glyphs PG prints under a
//! `$`-currency locale.
//!
//! TODO: resolve `L` from `lc_monetary` and `D`/`G` from `lc_numeric`. PG
//! prints a blank, not `$`, for `L` under `lc_monetary = C` — `to_char(123,
//! 'L999')` is `  123` there against `$ 123` under a `$` locale — and swaps
//! `D`/`G` to `,`/`.` under `lc_numeric = de_DE.UTF-8`.
//!
//! The layout is two-phase, because zero suppression and the *floating* sign
//! both depend on the whole field: [`parse_format`] measures the picture, then
//! [`render`] fills it and finally slides the sign next to the leading digit.

use crate::Numeric;
use crate::formatting::{FormatError, ordinal};

const SYNTAX_ERROR: &str = "42601";
const INVALID_TEXT_REPRESENTATION: &str = "22P02";

fn syntax(message: &str) -> FormatError {
    FormatError {
        sqlstate: SYNTAX_ERROR,
        message: message.to_string(),
        detail: None,
        hint: None,
    }
}

fn bad_number(raw: &str) -> FormatError {
    FormatError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("invalid input syntax for type numeric: \"{raw}\""),
        detail: None,
        hint: None,
    }
}

// --- the picture -----------------------------------------------------------

/// Where a sign is drawn, and which glyphs it uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SignKind {
    /// `S`: `+` or `-`, floating when it leads the digits.
    S,
    /// `MI`: `-` or a blank, always anchored.
    Mi,
    /// `PL`: `+` or a blank, always anchored.
    Pl,
    /// `SG`: `+` or `-`, always anchored.
    Sg,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Item {
    /// `9`: a digit position with leading-zero suppression.
    Digit,
    /// `0`: a digit position that always prints.
    Zero,
    /// `.` or `D`.
    Decimal,
    /// `,` or `G`.
    Group,
    /// `L`, rendered as `$` in the C locale.
    Currency,
    Sign(SignKind),
    /// The closing `>` of a `PR` pair; the `<` goes in the sign slot.
    Pr,
    /// `TH`/`th`, carrying whether the suffix is uppercased.
    Ordinal(bool),
    /// `B` and `C`: recognized so they are not mistaken for literal text.
    Ignored,
    Literal(String),
}

/// A measured picture. `pre`/`post` are digit-position counts; everything else
/// is a property the renderer needs before it can emit a single character.
struct NumFormat {
    items: Vec<Item>,
    pre: usize,
    post: usize,
    has_dec: bool,
    fm: bool,
    /// Index (among integer digit positions) of the first `0` code, from which
    /// point every position prints.
    zero_start: Option<usize>,
    /// A leading blank is reserved for the sign unless `S`/`MI`/`SG` provides
    /// one. `PL` does not count — PG keeps the slot alongside it.
    sign_slot: bool,
    /// Set by `PR`; the sign becomes `<`/`>`.
    pr: bool,
    /// Digit positions after `V`; the value is scaled by this many powers of 10.
    v_shift: u32,
    /// `EEEE`.
    sci: bool,
    /// `RN`/`rn`, carrying whether the numeral is uppercased.
    roman: Option<bool>,
}

/// Longest-first so `EEEE` beats `E`, `MI` beats `M`, and so on. Matching is
/// case-insensitive, except that `RN`/`rn` and `TH`/`th` also record the case.
const CODES: &[&str] = &[
    "EEEE", "TH", "RN", "MI", "PL", "SG", "PR", "S", "L", "D", "G", "V", "B", "C", "9", "0", ".",
    ",",
];

fn parse_format(fmt: &str) -> Result<NumFormat, FormatError> {
    let chars: Vec<char> = fmt.chars().collect();
    let mut f = NumFormat {
        items: Vec::new(),
        pre: 0,
        post: 0,
        has_dec: false,
        fm: false,
        zero_start: None,
        sign_slot: true,
        pr: false,
        v_shift: 0,
        sci: false,
        roman: None,
    };
    let mut literal = String::new();
    let mut seen_sign = false;
    let mut after_v = false;
    let mut i = 0;

    macro_rules! flush {
        () => {
            if !literal.is_empty() {
                f.items.push(Item::Literal(std::mem::take(&mut literal)));
            }
        };
    }

    while i < chars.len() {
        if chars[i] == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                literal.push(chars[i]);
                i += 1;
            }
            i += 1;
            continue;
        }
        // `FM` is a whole-picture flag here, not a per-field prefix.
        if chars[i..].len() >= 2
            && chars[i].eq_ignore_ascii_case(&'F')
            && chars[i + 1].eq_ignore_ascii_case(&'M')
        {
            f.fm = true;
            i += 2;
            continue;
        }
        let head: String = chars[i..].iter().take(4).collect();
        let folded = head.to_ascii_uppercase();
        let Some(code) = CODES.iter().find(|c| folded.starts_with(**c)) else {
            literal.push(chars[i]);
            i += 1;
            continue;
        };
        if f.sci {
            return Err(syntax("\"EEEE\" must be the last pattern used"));
        }
        flush!();
        match *code {
            "9" | "0" => {
                if f.pr {
                    return Err(syntax("\"9\" must be ahead of \"PR\""));
                }
                if f.has_dec {
                    f.post += 1;
                } else {
                    if *code == "0" && f.zero_start.is_none() {
                        f.zero_start = Some(f.pre);
                    }
                    f.pre += 1;
                    if after_v {
                        f.v_shift += 1;
                    }
                }
                f.items.push(if *code == "0" {
                    Item::Zero
                } else {
                    Item::Digit
                });
            }
            "." | "D" => {
                if f.has_dec {
                    return Err(syntax("multiple decimal points"));
                }
                if f.v_shift > 0 || after_v {
                    return Err(syntax("cannot use \"V\" and decimal point together"));
                }
                f.has_dec = true;
                f.items.push(Item::Decimal);
            }
            "," | "G" => f.items.push(Item::Group),
            "L" => f.items.push(Item::Currency),
            "V" => {
                if f.has_dec {
                    return Err(syntax("cannot use \"V\" and decimal point together"));
                }
                after_v = true;
            }
            "B" | "C" => f.items.push(Item::Ignored),
            "S" | "MI" | "PL" | "SG" => {
                let kind = match *code {
                    "S" => SignKind::S,
                    "MI" => SignKind::Mi,
                    "PL" => SignKind::Pl,
                    _ => SignKind::Sg,
                };
                if kind == SignKind::S && seen_sign {
                    return Err(syntax("cannot use \"S\" twice"));
                }
                if f.pr {
                    return Err(syntax(
                        "cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together",
                    ));
                }
                if kind == SignKind::S {
                    seen_sign = true;
                }
                // `PL` prints its own glyph but does not take over the sign slot.
                if kind != SignKind::Pl {
                    f.sign_slot = false;
                }
                f.items.push(Item::Sign(kind));
            }
            "PR" => {
                if !f.sign_slot || f.items.iter().any(|it| matches!(it, Item::Sign(_))) {
                    return Err(syntax(
                        "cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together",
                    ));
                }
                f.pr = true;
                f.items.push(Item::Pr);
            }
            "RN" => f.roman = Some(head.starts_with("RN")),
            "TH" => f.items.push(Item::Ordinal(head.starts_with("TH"))),
            "EEEE" => f.sci = true,
            _ => {}
        }
        i += code.len();
    }
    flush!();

    if f.sci {
        // `EEEE` composes only with digit and decimal-point patterns.
        let clean = f.items.iter().all(|it| {
            matches!(
                it,
                Item::Digit | Item::Zero | Item::Decimal | Item::Literal(_)
            )
        });
        if !clean || f.v_shift > 0 || f.fm {
            return Err(syntax("\"EEEE\" is incompatible with other formats").with_eeee_detail());
        }
    }
    Ok(f)
}

trait EeeeDetail {
    fn with_eeee_detail(self) -> FormatError;
}

impl EeeeDetail for FormatError {
    fn with_eeee_detail(mut self) -> FormatError {
        self.detail = Some(
            "\"EEEE\" may only be used together with digit and decimal point patterns.".into(),
        );
        self
    }
}

// --- rendering -------------------------------------------------------------

/// `to_char(numeric, text)`.
pub fn numeric(n: &Numeric, fmt: &str) -> Result<String, FormatError> {
    render(n, fmt, None)
}

/// `to_char(int4/int8, text)`.
pub fn int8(v: i64, fmt: &str) -> Result<String, FormatError> {
    render(&Numeric::from_i128(i128::from(v)), fmt, None)
}

/// `to_char(float8, text)`. PG converts through `DBL_DIG` significant digits,
/// so a picture asking for more fraction positions than that simply gets a
/// shorter field.
pub fn float8(v: f64, fmt: &str) -> Result<String, FormatError> {
    render(&Numeric::from_f64_sig(v, 15), fmt, Some(15))
}

/// `to_char(float4, text)`, capped at `FLT_DIG` significant digits.
pub fn float4(v: f32, fmt: &str) -> Result<String, FormatError> {
    render(&Numeric::from_f64_sig(f64::from(v), 6), fmt, Some(6))
}

/// One output cell, so the sign can be slid into place after the field is laid
/// out and `FM` can drop exactly the padding.
struct Cell {
    ch: char,
    /// Blank padding `FM` removes: the sign slot, a suppressed digit position,
    /// or a group separator inside the suppressed region.
    padding: bool,
    /// A fraction digit `FM` may strip when it is a trailing zero.
    strippable_frac: bool,
}

impl Cell {
    fn plain(ch: char) -> Cell {
        Cell {
            ch,
            padding: false,
            strippable_frac: false,
        }
    }
    fn pad(ch: char) -> Cell {
        Cell {
            ch,
            padding: true,
            strippable_frac: false,
        }
    }
}

fn render(n: &Numeric, fmt: &str, sig_cap: Option<usize>) -> Result<String, FormatError> {
    let f = parse_format(fmt)?;
    if let Some(upper) = f.roman {
        return Ok(roman(n, upper, f.fm));
    }
    // `V` is a decimal shift, applied before anything is measured.
    let scaled;
    let n = if f.v_shift > 0 {
        scaled = n.mul(&pow10(f.v_shift));
        &scaled
    } else {
        n
    };
    if f.sci {
        return Ok(scientific(n, &f));
    }
    if n.is_nan() {
        return Ok(nan_field(&f));
    }

    // A float's fraction is limited by its significant digits, so a picture
    // that asks for more simply produces a shorter field.
    let post = match sig_cap {
        Some(cap) => f.post.min(cap.saturating_sub(int_digit_count(n))),
        None => f.post,
    };
    let rounded = n.round(post as i32);
    let display = rounded.to_display();
    let negative = display.starts_with('-');
    let body = display.trim_start_matches('-');
    let (int_part, frac_part) = match body.split_once('.') {
        Some((i, fr)) => (i, fr),
        None => (body, ""),
    };
    // A value below 1 leaves the integer positions blank when the picture has a
    // decimal point, but prints a lone `0` when it does not.
    let int_digits = if f.has_dec && int_part == "0" {
        ""
    } else {
        int_part
    };
    let overflow = n.is_infinite() || int_digits.len() > f.pre;

    let mut cells: Vec<Cell> = Vec::new();
    let mut slot: Option<usize> = None;
    let mut int_seen = 0usize; // integer digit positions emitted so far
    let mut printed_int = false;
    let mut frac_seen = 0usize;
    let mut last_value = 0i64; // for a trailing TH suffix

    for item in &f.items {
        // The sign slot is emitted lazily, just before the digit region starts.
        if f.sign_slot
            && slot.is_none()
            && matches!(item, Item::Digit | Item::Zero | Item::Decimal | Item::Group)
        {
            slot = Some(cells.len());
            cells.push(Cell::pad(' '));
        }
        match item {
            Item::Digit | Item::Zero => {
                if f.has_dec && int_seen >= f.pre {
                    // A fraction position. Positions past a float's
                    // significant-digit budget are simply not emitted.
                    if frac_seen >= post {
                        continue;
                    }
                    let ch = frac_part
                        .as_bytes()
                        .get(frac_seen)
                        .map_or('0', |b| *b as char);
                    frac_seen += 1;
                    cells.push(if overflow {
                        Cell::plain('#')
                    } else {
                        Cell {
                            ch,
                            padding: false,
                            strippable_frac: matches!(item, Item::Digit),
                        }
                    });
                    continue;
                }
                let idx = int_seen;
                int_seen += 1;
                if overflow {
                    cells.push(Cell::plain('#'));
                    printed_int = true;
                    continue;
                }
                // Right-align the digits in the integer positions.
                let first = f.pre - int_digits.len();
                let forced = f.zero_start.is_some_and(|z| idx >= z);
                if idx >= first {
                    let ch = int_digits.as_bytes()[idx - first] as char;
                    cells.push(Cell::plain(ch));
                    printed_int = true;
                    // Only the last two digits matter to `ordinal`, and a
                    // picture may hold more digit positions than an i64 does.
                    last_value = (last_value * 10 + i64::from(ch as u8 - b'0')) % 100;
                } else if forced {
                    cells.push(Cell::plain('0'));
                    printed_int = true;
                } else {
                    cells.push(Cell::pad(' '));
                }
            }
            Item::Decimal => cells.push(Cell::plain('.')),
            Item::Group => {
                // A separator inside the suppressed region is blank, but the
                // column is still reserved.
                if printed_int || overflow {
                    cells.push(Cell::plain(','));
                } else {
                    cells.push(Cell::pad(' '));
                }
            }
            Item::Currency => cells.push(Cell::plain('$')),
            Item::Sign(kind) => {
                let ch = match (kind, negative) {
                    (SignKind::S | SignKind::Sg, true) => '-',
                    (SignKind::S | SignKind::Sg, false) => '+',
                    (SignKind::Mi, true) => '-',
                    (SignKind::Pl, false) => '+',
                    _ => ' ',
                };
                // A leading `S` floats; the anchored kinds never move.
                let floats = *kind == SignKind::S && slot.is_none();
                if floats {
                    slot = Some(cells.len());
                }
                cells.push(Cell {
                    ch,
                    padding: ch == ' ',
                    strippable_frac: false,
                });
            }
            Item::Pr => cells.push(if negative {
                Cell::plain('>')
            } else {
                Cell::pad(' ')
            }),
            Item::Ordinal(upper) => {
                if !negative {
                    cells.extend(ordinal(last_value, *upper).chars().map(Cell::plain));
                }
            }
            Item::Ignored => {}
            Item::Literal(s) => cells.extend(s.chars().map(Cell::plain)),
        }
    }

    // Float the sign: it belongs immediately left of the first printed digit.
    if let Some(at) = slot {
        let wanted = match (f.pr, negative, &f.items[..]) {
            (true, true, _) => Some('<'),
            (true, false, _) => None,
            (false, true, _) if f.sign_slot => Some('-'),
            _ => None,
        };
        let ch = wanted.unwrap_or(cells[at].ch);
        if ch != ' ' {
            let first = cells
                .iter()
                .position(|c| c.ch.is_ascii_digit() || c.ch == '#')
                .unwrap_or(at + 1);
            let target = if first > at { first - 1 } else { at };
            cells[at].ch = ' ';
            cells[at].padding = true;
            cells[target].ch = ch;
            cells[target].padding = false;
        }
    }

    if !f.fm {
        return Ok(cells.iter().map(|c| c.ch).collect());
    }
    // Fill mode: drop the padding, then the trailing `9`-coded fraction zeros.
    let mut kept: Vec<&Cell> = cells.iter().filter(|c| !c.padding).collect();
    while kept
        .last()
        .is_some_and(|c| c.strippable_frac && c.ch == '0')
    {
        kept.pop();
    }
    let mut out: String = kept.iter().map(|c| c.ch).collect();
    // With a decimal point but no integer digit, PG still shows the zero.
    if f.has_dec && !printed_int && !overflow {
        let at = out.find('.').unwrap_or(0);
        out.insert(at, '0');
    }
    Ok(out)
}

/// The number of digits left of the point, used to budget a float's fraction.
/// A leading `0` counts, matching how PG spends `DBL_DIG` on `0.1`.
fn int_digit_count(n: &Numeric) -> usize {
    let d = n.to_display();
    let body = d.trim_start_matches('-');
    body.split('.').next().unwrap_or("0").len()
}

fn pow10(n: u32) -> Numeric {
    let mut s = String::from("1");
    s.extend(std::iter::repeat_n('0', n as usize));
    Numeric::parse(&s).unwrap_or_else(|_| Numeric::from_i128(1))
}

/// The `EEEE` field with every column filled with `#` — PG's rendering for a
/// value no exponent can express. The exponent's own four columns fill too.
fn sci_overflow_field(f: &NumFormat) -> String {
    let mut out = String::new();
    if f.sign_slot {
        out.push(' ');
    }
    out.push_str(&"#".repeat(f.pre));
    if f.has_dec {
        out.push('.');
    }
    out.push_str(&"#".repeat(f.post));
    out.push_str("####");
    out
}

/// PG right-aligns `NaN` in the sign slot plus the integer positions, and keeps
/// a `PR` pair's trailing column.
fn nan_field(f: &NumFormat) -> String {
    if f.fm {
        return "NaN".to_string();
    }
    let width = usize::from(f.sign_slot) + f.pre;
    let mut out = format!("{:>width$}", "NaN");
    if f.pr {
        out.push(' ');
    }
    out
}

/// `EEEE`: one integer digit, `post` fraction digits, and a signed two-digit
/// exponent.
fn scientific(n: &Numeric, f: &NumFormat) -> String {
    if n.is_nan() {
        return nan_field(f);
    }
    if n.is_infinite() {
        // No exponent can represent an infinity, so the field overflows just as
        // it does on the ordinary path: every digit position becomes `#`.
        return sci_overflow_field(f);
    }
    let display = n.to_display();
    let negative = display.starts_with('-');
    let body = display.trim_start_matches('-');
    let (int_part, frac_part) = match body.split_once('.') {
        Some((i, fr)) => (i, fr),
        None => (body, ""),
    };
    let digits: String = format!("{int_part}{frac_part}");
    let first = digits.find(|c: char| c != '0');
    let (mantissa, exp) = match first {
        None => (Numeric::from_i128(0), 0i32),
        Some(k) => {
            let exp = int_part.len() as i32 - 1 - k as i32;
            let rest = &digits[k..];
            let text = if rest.len() > 1 {
                format!("{}.{}", &rest[..1], &rest[1..])
            } else {
                rest.to_string()
            };
            (
                Numeric::parse(&text).unwrap_or_else(|_| Numeric::from_i128(0)),
                exp,
            )
        }
    };
    let mut mantissa = mantissa.round(f.post as i32);
    let mut exp = exp;
    // Rounding can carry into a second integer digit (9.99 -> 10.0).
    if mantissa.to_display().trim_start_matches('-').len() > 1 + usize::from(f.post > 0) + f.post {
        mantissa = mantissa
            .mul(&Numeric::parse("0.1").unwrap_or_else(|_| Numeric::from_i128(1)))
            .round(f.post as i32);
        exp += 1;
    }
    let sign = if negative { "-" } else { " " };
    format!("{sign}{}e{:+03}", mantissa.to_display(), exp)
}

const ROMAN_WIDTH: usize = 15;

/// `RN`: PG renders roman numerals right-aligned in 15 columns, and fills the
/// field with `#` outside `1..=3999`.
fn roman(n: &Numeric, upper: bool, fm: bool) -> String {
    const UNITS: [(i64, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let text = n.round(0).to_display();
    let value: i64 = text.parse().unwrap_or(0);
    if !(1..=3999).contains(&value) {
        return "#".repeat(ROMAN_WIDTH);
    }
    let mut rest = value;
    let mut out = String::new();
    for (v, s) in UNITS {
        while rest >= v {
            out.push_str(s);
            rest -= v;
        }
    }
    if !upper {
        out = out.to_ascii_lowercase();
    }
    if fm {
        out
    } else {
        format!("{out:>ROMAN_WIDTH$}")
    }
}

// --- parsing ---------------------------------------------------------------

/// `to_number(text, text)`. `Ok(None)` is SQL NULL, which PG returns for an
/// empty picture.
pub fn to_number(input: &str, fmt: &str) -> Result<Option<Numeric>, FormatError> {
    let f = parse_format(fmt)?;
    if f.roman.is_some() {
        return Err(bad_number("invalid Roman numeral"));
    }
    if f.sci {
        return Err(syntax("\"EEEE\" not supported for input"));
    }
    if f.items.is_empty() {
        return Ok(None);
    }

    let chars: Vec<char> = input.chars().collect();
    let mut pos = 0usize;
    let mut negative = false;
    let mut int = String::new();
    let mut frac = String::new();
    let mut in_frac = false;

    // `PR` writes its sign as angle brackets around the whole field.
    if f.pr {
        let mut probe = pos;
        while chars.get(probe).is_some_and(|c| c.is_whitespace()) {
            probe += 1;
        }
        if chars.get(probe) == Some(&'<') {
            negative = true;
            pos = probe + 1;
        }
    }

    for item in &f.items {
        match item {
            Item::Digit | Item::Zero => {
                // Each digit position skips at most one leading blank, then
                // consumes exactly one character — a digit if it finds one.
                if chars.get(pos) == Some(&' ') {
                    pos += 1;
                }
                if matches!(chars.get(pos), Some('+') | Some('-')) {
                    negative |= chars[pos] == '-';
                    pos += 1;
                    if chars.get(pos) == Some(&' ') {
                        pos += 1;
                    }
                }
                match chars.get(pos) {
                    Some(c) if c.is_ascii_digit() => {
                        if in_frac { &mut frac } else { &mut int }.push(*c);
                        pos += 1;
                    }
                    Some(_) => pos += 1,
                    None => {}
                }
            }
            Item::Decimal => {
                in_frac = true;
                if chars.get(pos).is_some_and(|c| !c.is_ascii_digit()) {
                    pos += 1;
                }
            }
            Item::Group | Item::Currency | Item::Ignored => {
                if chars.get(pos).is_some_and(|c| !c.is_ascii_digit()) {
                    pos += 1;
                }
            }
            Item::Sign(_) => {
                while chars.get(pos) == Some(&' ') {
                    pos += 1;
                }
                match chars.get(pos) {
                    Some('-') => {
                        negative = true;
                        pos += 1;
                    }
                    Some('+') => pos += 1,
                    _ => {}
                }
            }
            Item::Pr => {
                if chars.get(pos) == Some(&'>') {
                    negative = true;
                    pos += 1;
                }
            }
            Item::Ordinal(_) => pos = (pos + 2).min(chars.len()),
            Item::Literal(s) => pos = (pos + s.chars().count()).min(chars.len()),
        }
    }

    // PG hands the reassembled digits to `numeric_in`, so its error quotes them
    // rather than the original input.
    let mut raw = String::from(if negative { "-" } else { " " });
    raw.push_str(&int);
    if !frac.is_empty() {
        raw.push('.');
        raw.push_str(&frac);
    }
    Numeric::parse(raw.trim())
        .map(Some)
        .map_err(|_| bad_number(&raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(v: &str, fmt: &str) -> String {
        let n = match Numeric::parse(v) {
            Ok(n) => n,
            Err(e) => panic!("invalid numeric fixture `{v}`: {e:?}"),
        };
        match numeric(&n, fmt) {
            Ok(s) => s,
            Err(e) => panic!("unexpected to_char error for `{v}`/`{fmt}`: {e:?}"),
        }
    }

    #[test]
    fn sign_placement() {
        for (fmt, pos, neg) in [
            ("99", "  1", " -1"),
            ("S99", " +1", " -1"),
            ("99S", " 1+", " 1-"),
            ("MI99", "  1", "- 1"),
            ("99MI", " 1 ", " 1-"),
            ("PL99", "+  1", "  -1"),
            ("99PL", "  1+", " -1 "),
            ("SG99", "+ 1", "- 1"),
            ("99SG", " 1+", " 1-"),
            ("99PR", "  1 ", " <1>"),
            ("L99", "$  1", "$ -1"),
            ("99L", "  1$", " -1$"),
            ("0999", " 0001", "-0001"),
            ("9099", "  001", " -001"),
            ("9,999", "     1", "    -1"),
            ("9G999", "     1", "    -1"),
            ("FM9", "1", "-1"),
            ("FM99PR", "1", "<1>"),
            ("FML99D99", "$1.", "$-1."),
        ] {
            assert_eq!(num("1", fmt), pos, "to_char(1, '{fmt}')");
            assert_eq!(num("-1", fmt), neg, "to_char(-1, '{fmt}')");
        }
    }

    #[test]
    fn zero_and_suppression() {
        assert_eq!(num("0", "99"), "  0");
        assert_eq!(num("0", "9999"), "    0");
        assert_eq!(num("0", "9999.9999"), "     .0000");
        assert_eq!(num("0", "9999.99"), "     .00");
        assert_eq!(num("0.5", "9999.99"), "     .50");
        assert_eq!(num("0", "0999.99"), " 0000.00");
        assert_eq!(num("0", "FM999.999"), "0.");
        assert_eq!(num("0", "FM999"), "0");
    }

    #[test]
    fn rounding_is_half_away_from_zero() {
        assert_eq!(num("2.5", "9"), " 3");
        assert_eq!(num("-2.5", "9"), "-3");
        assert_eq!(num("1.005", "9.99"), " 1.01");
        assert_eq!(num("1.015", "9.99"), " 1.02");
        // A value that rounds to zero loses its sign.
        assert_eq!(num("-0.01", "99.9"), "   .0");
    }

    #[test]
    fn overflow_is_hashes() {
        assert_eq!(num("1234", "99"), " ##");
        assert_eq!(num("12345", "9,999.99"), " #,###.##");
        assert_eq!(num("-12345", "9,999.99"), "-#,###.##");
        assert_eq!(num("-12345", "9,999.99PR"), "<#,###.##>");
        assert_eq!(num("9.99", "9.9"), " #.#");
        assert_eq!(num("1", ".9"), " .#");
        assert_eq!(num("1234", "9."), " #.");
    }

    #[test]
    fn fill_mode() {
        assert_eq!(num("1.5", "FM99.999"), "1.5");
        assert_eq!(num("1.5", "FM99.990"), "1.500");
        assert_eq!(num("-0.01", "FM99.9"), "0.");
    }

    #[test]
    fn specials() {
        assert_eq!(num("NaN", "999"), " NaN");
        assert_eq!(num("NaN", "9999.99"), "  NaN");
        assert_eq!(num("NaN", "FM9999.99"), "NaN");
        assert_eq!(num("NaN", "9999PR"), "  NaN ");
        assert_eq!(num("Infinity", "999"), " ###");
        assert_eq!(num("Infinity", "9999.99"), " ####.##");
        assert_eq!(num("-Infinity", "9999PR"), "<####>");
    }

    #[test]
    fn infinities_overflow_the_eeee_field() {
        assert_eq!(num("Infinity", "9.99EEEE"), " #.######");
        assert_eq!(num("-Infinity", "9.99EEEE"), " #.######");
    }

    #[test]
    fn ordinal_survives_more_digits_than_an_i64() {
        // 23 digit positions: the ordinal accumulator must not overflow.
        assert_eq!(
            num("99999999999999999999999", "99999999999999999999999TH"),
            " 99999999999999999999999TH"
        );
    }

    #[test]
    fn degenerate_pictures() {
        assert_eq!(num("1234", ""), "");
        assert_eq!(num("1234", "FM"), "");
        assert_eq!(num("1234", "9 9 9 9"), " 1 2 3 4");
        assert_eq!(num("1234", "\"x\"999"), "x ###");
    }

    #[test]
    fn v_shifts_the_value() {
        assert_eq!(num("1234", "999V9"), " ####");
        assert_eq!(num("1234", "99V99"), " ####");
        assert_eq!(num("1.2", "99V9"), "  12");
    }

    #[test]
    fn ordinal_suffix() {
        assert_eq!(num("1", "999th"), "   1st");
        assert_eq!(num("2", "999TH"), "   2ND");
        assert_eq!(num("3", "9TH"), " 3RD");
        assert_eq!(num("11", "999th"), "  11th");
        // PG omits the suffix on a negative value.
        assert_eq!(num("-1", "999th"), "  -1");
    }

    #[test]
    fn roman_numerals() {
        assert_eq!(num("4", "RN"), "             IV");
        assert_eq!(num("1999", "FMRN"), "MCMXCIX");
        assert_eq!(num("4000", "RN"), "###############");
        assert_eq!(num("14", "rn"), "            xiv");
    }

    #[test]
    fn scientific() {
        assert_eq!(num("-0.0004859", "9.99EEEE"), "-4.86e-04");
        assert_eq!(num("12345", "9.99EEEE"), " 1.23e+04");
        assert_eq!(num("0", "9.99EEEE"), " 0.00e+00");
        assert_eq!(num("1234", "9EEEE"), " 1e+03");
    }

    #[test]
    fn float_significant_digit_cap() {
        assert_eq!(
            float8(0.1, "0.999999999999999999").as_deref(),
            Ok(" 0.10000000000000")
        );
        assert_eq!(float4(123.456, "999.999999999").as_deref(), Ok(" 123.456"));
    }

    #[test]
    fn format_errors() {
        for (fmt, message) in [
            ("S9S", "cannot use \"S\" twice"),
            ("9.9.9", "multiple decimal points"),
            (
                "MI9PR",
                "cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together",
            ),
            ("9EEEE9", "\"EEEE\" must be the last pattern used"),
            ("999.9V9", "cannot use \"V\" and decimal point together"),
            ("9PR9", "\"9\" must be ahead of \"PR\""),
        ] {
            let e = match numeric(&Numeric::from_i128(1), fmt) {
                Err(e) => e,
                other => panic!("expected an error for `{fmt}`, got {other:?}"),
            };
            assert_eq!(e.sqlstate, "42601", "{fmt}");
            assert_eq!(e.message, message, "{fmt}");
        }
        let e = match numeric(&Numeric::from_i128(1), "9V9EEEE") {
            Err(e) => e,
            other => panic!("expected an error, got {other:?}"),
        };
        assert_eq!(e.message, "\"EEEE\" is incompatible with other formats");
        assert!(e.detail.is_some());
    }

    #[test]
    fn to_number_cases() {
        for (input, fmt, expected) in [
            ("12,454.8-", "99G999D9S", "-12454.8"),
            ("  1234", "9999", "123"),
            ("1234", "9G999", "1234"),
            ("$1,234.56", "L9G999D99", "1234.56"),
            ("a1c", "9999", "1"),
            ("1a2", "999", "12"),
            ("12a", "999", "12"),
            ("1 2", "999", "12"),
            ("1.2.3", "9.9", "1.2"),
            ("+12", "S99", "12"),
            ("12-", "99MI", "-12"),
            ("<123>", "999PR", "-123"),
            ("1 2 3", "999", "123"),
            ("12345", "9999", "1234"),
        ] {
            let got = match to_number(input, fmt) {
                Ok(Some(n)) => n.to_display(),
                other => panic!("unexpected to_number result for `{input}`/`{fmt}`: {other:?}"),
            };
            assert_eq!(got, expected, "to_number('{input}', '{fmt}')");
        }
        assert_eq!(to_number("123", ""), Ok(None));

        let e = match to_number("abc", "9999") {
            Err(e) => e,
            other => panic!("expected an error, got {other:?}"),
        };
        assert_eq!(e.sqlstate, "22P02");
        assert_eq!(e.message, "invalid input syntax for type numeric: \" \"");
        let e = match to_number("-", "999") {
            Err(e) => e,
            other => panic!("expected an error, got {other:?}"),
        };
        assert_eq!(e.message, "invalid input syntax for type numeric: \"-\"");
    }
}
