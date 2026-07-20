//! `json` and `jsonb` types.
//!
//! Clean-room reproduction of PostgreSQL's observable behavior (I/O text, error
//! text, SQLSTATE) for the JSON family. Hand-written recursive-descent parser —
//! the crate has no JSON dependency, and every scalar type here rolls its own
//! I/O.
//!
//! Two representations, matching the two SQL types:
//!
//! * `json` keeps the **raw input text verbatim** (whitespace, key order, and
//!   duplicate keys are all preserved). Input is only validated for well-formed
//!   JSON; the stored `Value::Json(String)` is the original spelling.
//! * `jsonb` is a **canonical parsed tree** ([`Jsonb`]): insignificant
//!   whitespace is dropped, object keys are sorted (shorter keys first, then by
//!   byte order — PG's storage order) with duplicates collapsed keeping the last
//!   value, and numbers are normalized through [`crate::numeric`] so
//!   `'1.0'::jsonb` prints `1.0`, `'1e2'::jsonb` prints `100`, and comparisons
//!   match `numeric_cmp` (value-based: `'1.0'::jsonb = '1.00'::jsonb`).
//!
//! Equality/ordering are defined only for `jsonb` (via [`cmp`]), reproducing
//! PG's `compareJsonbContainers`. `json` has no default equality operator, so it
//! is left out of the executor's comparison/hash paths.

use crate::numeric::{Numeric, ParseError};
use std::cmp::Ordering;

// SQLSTATE literals (kept local; the types crate must not depend on the wire
// crate). Mirrors `crabgresql_pg_wire::sqlstate`.
const INVALID_TEXT_REPRESENTATION: &str = "22P02";
const INVALID_PARAMETER_VALUE: &str = "22023";

/// A parsed, canonical `jsonb` value. Object keys are sorted (shorter first,
/// then byte order) with duplicates removed keeping the last value; numbers are
/// canonical [`Numeric`]s. These invariants make structural equality (`PartialEq`)
/// and [`Hash`](std::hash::Hash) coincide with jsonb equality, and let [`cmp`]
/// implement PG's total order.
#[derive(Clone, Debug, PartialEq, Hash)]
pub enum Jsonb {
    Null,
    Bool(bool),
    Number(Numeric),
    String(String),
    Array(Vec<Jsonb>),
    /// Invariant: keys unique and sorted by [`key_cmp`].
    Object(Vec<(String, Jsonb)>),
}

/// Error from JSON input or a `jsonb`→scalar cast: SQLSTATE + rendered message
/// (+ optional DETAIL), matching PostgreSQL's wording.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonError {
    pub sqlstate: &'static str,
    pub message: String,
    pub detail: Option<String>,
}

impl JsonError {
    /// `invalid input syntax for type <json|jsonb>` (22P02). Unlike most types,
    /// PG's JSON input error omits the offending value from the primary message
    /// (it appears in the DETAIL / CONTEXT instead).
    fn syntax(type_name: &str, detail: impl Into<String>) -> JsonError {
        JsonError {
            sqlstate: INVALID_TEXT_REPRESENTATION,
            message: format!("invalid input syntax for type {type_name}"),
            detail: Some(detail.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

/// `json_in`: validate that `s` is well-formed JSON and return it **unchanged**
/// (the `json` type preserves the original text).
pub fn json_in(s: &str) -> Result<String, JsonError> {
    parse_tree(s, "json")?;
    Ok(s.to_string())
}

/// `jsonb_in`: parse and canonicalize into a [`Jsonb`] tree.
pub fn jsonb_in(s: &str) -> Result<Jsonb, JsonError> {
    parse_tree(s, "jsonb")
}

/// Parse `s` as JSON, canonicalizing objects and numbers. `type_name` selects
/// the error message spelling (`json` vs `jsonb`).
fn parse_tree(s: &str, type_name: &str) -> Result<Jsonb, JsonError> {
    let mut p = Parser {
        bytes: s.as_bytes(),
        pos: 0,
        type_name,
    };
    p.skip_ws();
    if p.pos >= p.bytes.len() {
        return Err(JsonError::syntax(type_name, "The input string ended unexpectedly."));
    }
    let value = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        let tok = p.token_at();
        return Err(JsonError::syntax(
            type_name,
            format!("Expected end of input, but found \"{tok}\"."),
        ));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    type_name: &'a str,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn ended(&self) -> JsonError {
        JsonError::syntax(self.type_name, "The input string ended unexpectedly.")
    }

    fn invalid_token(&self) -> JsonError {
        JsonError::syntax(
            self.type_name,
            format!("Token \"{}\" is invalid.", self.token_at()),
        )
    }

    /// The "token" starting at the cursor, for error messages: a run up to the
    /// next whitespace or structural character. Used to fill PG's `"%s"` slots.
    fn token_at(&self) -> String {
        let start = self.pos;
        let mut end = start;
        while let Some(b) = self.bytes.get(end) {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b',' | b':' | b'{' | b'}' | b'[' | b']') {
                break;
            }
            end += 1;
        }
        String::from_utf8_lossy(&self.bytes[start..end]).into_owned()
    }

    fn parse_value(&mut self) -> Result<Jsonb, JsonError> {
        self.skip_ws();
        match self.peek() {
            None => Err(self.ended()),
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => Ok(Jsonb::String(self.parse_string()?)),
            Some(b'-' | b'0'..=b'9') => Ok(Jsonb::Number(self.parse_number()?)),
            Some(b't') => self.parse_keyword("true", Jsonb::Bool(true)),
            Some(b'f') => self.parse_keyword("false", Jsonb::Bool(false)),
            Some(b'n') => self.parse_keyword("null", Jsonb::Null),
            Some(_) => Err(self.invalid_token()),
        }
    }

    fn parse_keyword(&mut self, kw: &str, value: Jsonb) -> Result<Jsonb, JsonError> {
        if self.bytes[self.pos..].starts_with(kw.as_bytes()) {
            self.pos += kw.len();
            Ok(value)
        } else {
            Err(self.invalid_token())
        }
    }

    fn parse_array(&mut self) -> Result<Jsonb, JsonError> {
        self.pos += 1; // consume '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Jsonb::Array(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Jsonb::Array(items));
                }
                None => return Err(self.ended()),
                Some(_) => {
                    return Err(JsonError::syntax(
                        self.type_name,
                        format!("Expected \",\" or \"]\", but found \"{}\".", self.token_at()),
                    ));
                }
            }
        }
    }

    fn parse_object(&mut self) -> Result<Jsonb, JsonError> {
        self.pos += 1; // consume '{'
        let mut pairs: Vec<(String, Jsonb)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Jsonb::Object(pairs));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                let detail = if self.peek().is_none() {
                    "The input string ended unexpectedly.".to_string()
                } else {
                    format!("Expected string or \"}}\", but found \"{}\".", self.token_at())
                };
                return Err(JsonError::syntax(self.type_name, detail));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                let detail = if self.peek().is_none() {
                    "The input string ended unexpectedly.".to_string()
                } else {
                    format!("Expected \":\", but found \"{}\".", self.token_at())
                };
                return Err(JsonError::syntax(self.type_name, detail));
            }
            self.pos += 1; // consume ':'
            let value = self.parse_value()?;
            insert_last_wins(&mut pairs, key, value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                None => return Err(self.ended()),
                Some(_) => {
                    return Err(JsonError::syntax(
                        self.type_name,
                        format!("Expected \",\" or \"}}\", but found \"{}\".", self.token_at()),
                    ));
                }
            }
        }
        pairs.sort_by(|a, b| key_cmp(&a.0, &b.0));
        Ok(Jsonb::Object(pairs))
    }

    /// Parse a JSON string literal (cursor at the opening `"`), decoding escapes.
    fn parse_string(&mut self) -> Result<String, JsonError> {
        self.pos += 1; // consume opening quote
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(self.ended()),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    self.parse_escape(&mut out)?;
                }
                Some(c) if c < 0x20 => {
                    // Unescaped control character is not allowed in JSON strings.
                    return Err(JsonError::syntax(
                        self.type_name,
                        format!("Character with value 0x{c:02x} must be escaped."),
                    ));
                }
                Some(_) => {
                    // Copy one UTF-8 scalar. Find its byte length from the lead
                    // byte; the input is valid UTF-8 (it came from a &str).
                    let len = utf8_len(self.bytes[self.pos]);
                    let end = (self.pos + len).min(self.bytes.len());
                    out.push_str(&String::from_utf8_lossy(&self.bytes[self.pos..end]));
                    self.pos = end;
                }
            }
        }
    }

    fn parse_escape(&mut self, out: &mut String) -> Result<(), JsonError> {
        let esc = self.peek().ok_or_else(|| self.ended())?;
        self.pos += 1;
        match esc {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let cp = self.parse_hex4()?;
                if (0xD800..=0xDBFF).contains(&cp) {
                    // High surrogate: must be followed by \uDC00..DFFF.
                    if self.peek() == Some(b'\\') && self.bytes.get(self.pos + 1) == Some(&b'u') {
                        self.pos += 2;
                        let lo = self.parse_hex4()?;
                        if (0xDC00..=0xDFFF).contains(&lo) {
                            let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                            out.push(char::from_u32(c).unwrap_or('\u{fffd}'));
                        } else {
                            return Err(self.bad_escape("Unicode low surrogate must follow a high surrogate."));
                        }
                    } else {
                        return Err(self.bad_escape("Unicode low surrogate must follow a high surrogate."));
                    }
                } else if (0xDC00..=0xDFFF).contains(&cp) {
                    return Err(self.bad_escape("Unicode low surrogate must follow a high surrogate."));
                } else {
                    out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                }
            }
            other => {
                return Err(self.bad_escape(format!(
                    "Escape sequence \"\\{}\" is invalid.",
                    other as char
                )));
            }
        }
        Ok(())
    }

    fn bad_escape(&self, detail: impl Into<String>) -> JsonError {
        JsonError::syntax(self.type_name, detail)
    }

    fn parse_hex4(&mut self) -> Result<u32, JsonError> {
        if self.pos + 4 > self.bytes.len() {
            return Err(self.ended());
        }
        let mut v = 0u32;
        for _ in 0..4 {
            let b = self.bytes[self.pos];
            let d = match b {
                b'0'..=b'9' => (b - b'0') as u32,
                b'a'..=b'f' => (b - b'a' + 10) as u32,
                b'A'..=b'F' => (b - b'A' + 10) as u32,
                _ => {
                    return Err(
                        self.bad_escape("\"\\u\" must be followed by four hexadecimal digits.")
                    );
                }
            };
            v = v * 16 + d;
            self.pos += 1;
        }
        Ok(v)
    }

    /// Scan a JSON number per RFC 8259 grammar, then normalize through
    /// [`Numeric`] (matching PG's `jsonb` numeric normalization).
    fn parse_number(&mut self) -> Result<Numeric, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        // Integer part: `0` or `[1-9][0-9]*`.
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.invalid_token_from(start)),
        }
        // Fraction.
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.invalid_token_from(start));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        // Exponent.
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.invalid_token_from(start));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap_or("");
        Numeric::parse(text).map_err(|e| match e {
            ParseError::Syntax => self.invalid_token_from(start),
            ParseError::Overflow => JsonError::syntax(self.type_name, "value overflows numeric format"),
        })
    }

    fn invalid_token_from(&self, start: usize) -> JsonError {
        let end = self.bytes[start..]
            .iter()
            .position(|b| {
                matches!(
                    b,
                    b' ' | b'\t' | b'\n' | b'\r' | b',' | b':' | b'{' | b'}' | b'[' | b']'
                )
            })
            .map(|i| start + i)
            .unwrap_or(self.bytes.len());
        let tok = String::from_utf8_lossy(&self.bytes[start..end.max(self.pos)]);
        JsonError::syntax(self.type_name, format!("Token \"{tok}\" is invalid."))
    }
}

fn utf8_len(lead: u8) -> usize {
    if lead < 0x80 {
        1
    } else if lead >> 5 == 0b110 {
        2
    } else if lead >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// Insert `(key, value)` keeping the **last** occurrence of a duplicate key,
/// matching jsonb object semantics.
fn insert_last_wins(pairs: &mut Vec<(String, Jsonb)>, key: String, value: Jsonb) {
    if let Some(slot) = pairs.iter_mut().find(|(k, _)| *k == key) {
        slot.1 = value;
    } else {
        pairs.push((key, value));
    }
}

/// jsonb object-key order: shorter keys sort first, then plain byte order. This
/// is PG's storage order (`lengthCompareJsonbStringValue`), which is also the
/// order keys print in and are compared in.
fn key_cmp(a: &str, b: &str) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.as_bytes().cmp(b.as_bytes()))
}

// ---------------------------------------------------------------------------
// Output (`jsonb_out`)
// ---------------------------------------------------------------------------

/// Serialize a [`Jsonb`] to its canonical text form (`jsonb_out`): `, ` between
/// elements, `": "` after keys, numbers via `numeric_out`, strings escaped as PG
/// does.
pub fn format(value: &Jsonb) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &Jsonb) {
    match value {
        Jsonb::Null => out.push_str("null"),
        Jsonb::Bool(true) => out.push_str("true"),
        Jsonb::Bool(false) => out.push_str("false"),
        Jsonb::Number(n) => out.push_str(&n.to_display()),
        Jsonb::String(s) => write_escaped(out, s),
        Jsonb::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Jsonb::Object(pairs) => {
            out.push('{');
            for (i, (key, val)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_escaped(out, key);
                out.push_str(": ");
                write_value(out, val);
            }
            out.push('}');
        }
    }
}

/// Escape a string as PG's `escape_json`: `"` and `\` escaped, control chars
/// under 0x20 as their short form or `\u00xx`, everything else (including
/// non-ASCII) passed through as UTF-8. Forward slash is not escaped.
fn write_escaped(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------
// Ordering (`jsonb` total order — PG's `compareJsonbContainers`)
// ---------------------------------------------------------------------------

/// Type rank for cross-type comparison: `Object > Array > Boolean > Number >
/// String > Null`.
fn type_rank(value: &Jsonb) -> u8 {
    match value {
        Jsonb::Null => 0,
        Jsonb::String(_) => 1,
        Jsonb::Number(_) => 2,
        Jsonb::Bool(_) => 3,
        Jsonb::Array(_) => 4,
        Jsonb::Object(_) => 5,
    }
}

/// Total order over `jsonb`, reproducing PG's `compareJsonbContainers`:
/// different types compare by [`type_rank`]; arrays/objects compare by
/// length/pair-count first, then element-/pair-wise; scalars compare by their
/// underlying type (numbers via `numeric_cmp`, strings by byte order).
pub fn cmp(a: &Jsonb, b: &Jsonb) -> Ordering {
    let (ra, rb) = (type_rank(a), type_rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Jsonb::Null, Jsonb::Null) => Ordering::Equal,
        (Jsonb::Bool(x), Jsonb::Bool(y)) => x.cmp(y),
        (Jsonb::Number(x), Jsonb::Number(y)) => x.cmp(y),
        (Jsonb::String(x), Jsonb::String(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Jsonb::Array(x), Jsonb::Array(y)) => {
            if x.len() != y.len() {
                return x.len().cmp(&y.len());
            }
            for (ex, ey) in x.iter().zip(y) {
                let c = cmp(ex, ey);
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        }
        (Jsonb::Object(x), Jsonb::Object(y)) => {
            if x.len() != y.len() {
                return x.len().cmp(&y.len());
            }
            for ((kx, vx), (ky, vy)) in x.iter().zip(y) {
                let c = kx.as_bytes().cmp(ky.as_bytes());
                if c != Ordering::Equal {
                    return c;
                }
                let c = cmp(vx, vy);
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        }
        // `type_rank` equality guarantees the same variant; unreachable.
        _ => Ordering::Equal,
    }
}

// ---------------------------------------------------------------------------
// `jsonb` → scalar extraction (the casts PG's `pg_cast` defines)
// ---------------------------------------------------------------------------

/// The jsonb value kind, as named in PG's `cannot cast jsonb <kind> to ...`.
pub fn kind(value: &Jsonb) -> &'static str {
    match value {
        Jsonb::Null => "null",
        Jsonb::String(_) => "string",
        Jsonb::Number(_) => "numeric",
        Jsonb::Bool(_) => "boolean",
        Jsonb::Array(_) => "array",
        Jsonb::Object(_) => "object",
    }
}

/// `cannot cast jsonb <kind> to type <sqltype>` (22023) — PG's error when a
/// `jsonb` value has the wrong kind for the target scalar type.
pub fn cannot_cast(value: &Jsonb, sqltype: &str) -> JsonError {
    JsonError {
        sqlstate: INVALID_PARAMETER_VALUE,
        message: format!("cannot cast jsonb {} to type {sqltype}", kind(value)),
        detail: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, anyhow};

    fn jb(s: &str) -> Result<Jsonb> {
        jsonb_in(s).map_err(|e| anyhow!("jsonb_in({s:?}) failed: {}", e.message))
    }

    fn out(s: &str) -> Result<String> {
        Ok(format(&jb(s)?))
    }

    #[test]
    fn roundtrip_scalars() -> Result<()> {
        assert_eq!(out("null")?, "null");
        assert_eq!(out("true")?, "true");
        assert_eq!(out("false")?, "false");
        assert_eq!(out("  42 ")?, "42");
        assert_eq!(out("\"hi\"")?, "\"hi\"");
        Ok(())
    }

    #[test]
    fn json_preserves_raw_text() -> Result<()> {
        let raw = "{\"b\":1,   \"a\" :2}";
        assert_eq!(json_in(raw).map_err(|e| anyhow!("{}", e.message))?, raw);
        Ok(())
    }

    #[test]
    fn canonicalizes_objects() -> Result<()> {
        // Keys sorted shorter-first then by byte order; duplicate keys keep last.
        assert_eq!(out("{\"b\":1,\"a\":2,\"a\":3}")?, "{\"a\": 3, \"b\": 1}");
        assert_eq!(out("{\"aa\":1,\"b\":2}")?, "{\"b\": 2, \"aa\": 1}");
        assert_eq!(out("[1,2,  3]")?, "[1, 2, 3]");
        Ok(())
    }

    #[test]
    fn normalizes_numbers_via_numeric() -> Result<()> {
        assert_eq!(out("1.0")?, "1.0");
        assert_eq!(out("1.00")?, "1.00");
        assert_eq!(out("1e2")?, "100");
        assert_eq!(out("-0")?, "0");
        Ok(())
    }

    #[test]
    fn equal_numbers_of_different_scale() -> Result<()> {
        assert_eq!(jb("1.0")?, jb("1.00")?);
        assert_eq!(cmp(&jb("1.0")?, &jb("1.00")?), Ordering::Equal);
        Ok(())
    }

    #[test]
    fn ordering_matches_pg() -> Result<()> {
        // Type rank: Null < String < Number < Bool < Array < Object.
        assert_eq!(cmp(&jb("null")?, &jb("\"a\"")?), Ordering::Less);
        assert_eq!(cmp(&jb("\"a\"")?, &jb("1")?), Ordering::Less);
        assert_eq!(cmp(&jb("1")?, &jb("true")?), Ordering::Less);
        assert_eq!(cmp(&jb("true")?, &jb("[1]")?), Ordering::Less);
        assert_eq!(cmp(&jb("[1]")?, &jb("{}")?), Ordering::Less);
        // Arrays compare by length first.
        assert_eq!(cmp(&jb("[5]")?, &jb("[1,2,3]")?), Ordering::Less);
        Ok(())
    }

    #[test]
    fn decodes_string_escapes() -> Result<()> {
        assert_eq!(jb("\"a\\nb\"")?, Jsonb::String("a\nb".to_string()));
        assert_eq!(jb("\"\\u0041\"")?, Jsonb::String("A".to_string()));
        // Surrogate pair for U+1F600.
        assert_eq!(jb("\"\\ud83d\\ude00\"")?, Jsonb::String("\u{1f600}".to_string()));
        Ok(())
    }

    #[test]
    fn rejects_garbage() -> Result<()> {
        assert!(jsonb_in("{bad").is_err());
        assert!(jsonb_in("[1,2").is_err());
        assert!(jsonb_in("01").is_err());
        assert!(jsonb_in("").is_err());
        assert!(jsonb_in("{\"a\" 1}").is_err());
        assert!(jsonb_in("truefoo").is_err());
        Ok(())
    }
}
