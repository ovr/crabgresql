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
const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
const UNTRANSLATABLE_CHARACTER: &str = "22P05";
const PROGRAM_LIMIT_EXCEEDED: &str = "54001";

/// Maximum JSON nesting depth. Bounds every recursion over a [`Jsonb`] value —
/// parsing, `Drop`, [`format`], and [`cmp`] — so a pathologically nested literal
/// returns a controlled error instead of overflowing the ~2 MB worker-thread
/// stack (measured ~2 KB/level while parsing, so this keeps the deepest
/// recursion under ~1/4 of the stack). Far exceeds any real document; PG relies
/// on the configurable `check_stack_depth()` for the same protection.
const MAX_DEPTH: usize = 200;

/// A parsed, canonical `jsonb` value. Object keys are sorted (shorter first,
/// then byte order) with duplicates removed keeping the last value; numbers are
/// canonical [`Numeric`]s. These invariants make structural equality (`PartialEq`)
/// and [`Hash`](std::hash::Hash) coincide with jsonb equality, and let [`cmp`]
/// implement PG's total order.
#[derive(deepsize::DeepSizeOf, Clone, Debug, PartialEq, Hash)]
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
        is_jsonb: type_name == "jsonb",
        depth: 0,
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
    /// Whether the target is `jsonb` (which rejects ``) vs `json`.
    is_jsonb: bool,
    /// Current container nesting depth, bounded by [`MAX_DEPTH`].
    depth: usize,
}

impl<'a> Parser<'a> {
    /// A parser positioned at `pos` over `bytes`, used by the extraction
    /// operators to decode a single string in place (rather than re-parsing the
    /// whole document).
    fn at(bytes: &'a [u8], pos: usize, type_name: &'a str, is_jsonb: bool) -> Parser<'a> {
        Parser {
            bytes,
            pos,
            type_name,
            is_jsonb,
            depth: 0,
        }
    }
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// `stack depth limit exceeded` (54001), returned instead of overflowing the
    /// stack on deeply nested input (PG's `check_stack_depth` equivalent).
    fn too_deep(&self) -> JsonError {
        JsonError {
            sqlstate: PROGRAM_LIMIT_EXCEEDED,
            message: "stack depth limit exceeded".to_string(),
            detail: None,
        }
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

    /// The "token" at the cursor, for error messages (PG's `"%s"` slots): a
    /// structural character is a one-char token; otherwise it is the run up to
    /// the next whitespace or structural character.
    fn token_at(&self) -> String {
        self.token_from(self.pos)
    }

    /// [`token_at`](Self::token_at), but from an explicit start offset (used when
    /// a number token has already advanced the cursor past its start).
    fn token_from(&self, start: usize) -> String {
        match self.bytes.get(start).copied() {
            None => String::new(),
            Some(b) if is_structural(b) => (b as char).to_string(),
            Some(_) => {
                let mut end = start;
                while let Some(&b) = self.bytes.get(end) {
                    if b.is_ascii_whitespace() || is_structural(b) {
                        break;
                    }
                    end += 1;
                }
                String::from_utf8_lossy(&self.bytes[start..end]).into_owned()
            }
        }
    }

    fn parse_value(&mut self) -> Result<Jsonb, JsonError> {
        self.skip_ws();
        match self.peek() {
            None => Err(self.ended()),
            // Containers recurse: bound the depth so nesting cannot overflow the
            // stack. Decrement on the way back up (the whole parse aborts on the
            // error path, so no cleanup is needed there).
            Some(b'{') => {
                self.depth += 1;
                if self.depth > MAX_DEPTH {
                    return Err(self.too_deep());
                }
                let r = self.parse_object();
                self.depth -= 1;
                r
            }
            Some(b'[') => {
                self.depth += 1;
                if self.depth > MAX_DEPTH {
                    return Err(self.too_deep());
                }
                let r = self.parse_array();
                self.depth -= 1;
                r
            }
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
        // `first` is only true for the key right after `{`; there a bare `}`
        // would have been consumed above, so the "expected key" error offers
        // `}` as an alternative. After a comma PG expects a key only.
        let mut first = true;
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                let detail = if self.peek().is_none() {
                    "The input string ended unexpectedly.".to_string()
                } else if first {
                    format!("Expected string or \"}}\", but found \"{}\".", self.token_at())
                } else {
                    format!("Expected string, but found \"{}\".", self.token_at())
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
            pairs.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    first = false;
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
        Ok(Jsonb::Object(canonicalize_object(pairs)))
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
                    // Copy the whole run of ordinary bytes (no quote, backslash,
                    // or control char) in one shot. The bytes came from a valid
                    // `&str`, so the run is on char boundaries and is valid UTF-8.
                    let start = self.pos;
                    while let Some(c) = self.peek() {
                        if c == b'"' || c == b'\\' || c < 0x20 {
                            break;
                        }
                        self.pos += 1;
                    }
                    out.push_str(std::str::from_utf8(&self.bytes[start..self.pos]).unwrap_or(""));
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
                    // High surrogate: must be followed by a \uDC00..DFFF low
                    // surrogate to form a pair.
                    if self.peek() == Some(b'\\') && self.bytes.get(self.pos + 1) == Some(&b'u') {
                        self.pos += 2;
                        let lo = self.parse_hex4()?;
                        if (0xDC00..=0xDFFF).contains(&lo) {
                            let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                            out.push(char::from_u32(c).unwrap_or('\u{fffd}'));
                        } else if (0xD800..=0xDBFF).contains(&lo) {
                            return Err(self.bad_escape(
                                "Unicode high surrogate must not follow a high surrogate.",
                            ));
                        } else {
                            return Err(self.bad_escape(
                                "Unicode low surrogate must follow a high surrogate.",
                            ));
                        }
                    } else {
                        return Err(
                            self.bad_escape("Unicode low surrogate must follow a high surrogate.")
                        );
                    }
                } else if (0xDC00..=0xDFFF).contains(&cp) {
                    return Err(
                        self.bad_escape("Unicode low surrogate must follow a high surrogate.")
                    );
                } else {
                    self.emit_codepoint(out, cp)?;
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

    /// Push a decoded Unicode scalar. `jsonb` cannot store a NUL (it is a text
    /// datum), so a `\u0000` escape is rejected there with PG's wording. `json`
    /// validates through the same parser but keeps the raw input text, so it
    /// accepts the escape (`is_jsonb` is false).
    fn emit_codepoint(&self, out: &mut String, cp: u32) -> Result<(), JsonError> {
        if cp == 0 && self.is_jsonb {
            return Err(JsonError {
                sqlstate: UNTRANSLATABLE_CHARACTER,
                message: "unsupported Unicode escape sequence".to_string(),
                detail: Some("\\u0000 cannot be converted to text.".to_string()),
            });
        }
        out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
        Ok(())
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
            // A magnitude too large for `numeric` is PG's `numeric_in` error,
            // surfaced verbatim (22003), not a JSON syntax error.
            ParseError::Overflow => JsonError {
                sqlstate: NUMERIC_VALUE_OUT_OF_RANGE,
                message: "value overflows numeric format".to_string(),
                detail: None,
            },
        })
    }

    fn invalid_token_from(&self, start: usize) -> JsonError {
        JsonError::syntax(
            self.type_name,
            format!("Token \"{}\" is invalid.", self.token_from(start)),
        )
    }
}

/// The JSON structural characters, which are one-character tokens.
fn is_structural(b: u8) -> bool {
    matches!(b, b',' | b':' | b'{' | b'}' | b'[' | b']')
}

/// Canonicalize an object's pairs (in input order): drop duplicate keys keeping
/// the **last** occurrence (jsonb semantics), then sort by [`key_cmp`]. Runs in
/// O(n log n) — a duplicate-key scan per insertion would be O(n²).
fn canonicalize_object(mut pairs: Vec<(String, Jsonb)>) -> Vec<(String, Jsonb)> {
    if pairs.len() > 1 {
        // Reverse so the last occurrence of each key comes first, then keep the
        // first-seen (i.e. last-in-input) via a set of retained keys.
        pairs.reverse();
        let mut seen = std::collections::HashSet::with_capacity(pairs.len());
        pairs.retain(|(k, _)| seen.insert(k.clone()));
    }
    // key_cmp is a total order over distinct keys, so the sort is deterministic
    // regardless of the retained (reversed) order above.
    pairs.sort_by(|a, b| key_cmp(&a.0, &b.0));
    pairs
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

// ---------------------------------------------------------------------------
// Extraction operators (`->`, `->>`, `#>`, `#>>`)
// ---------------------------------------------------------------------------
//
// The two representations need two different strategies:
//
// * `jsonb` is already a tree, so extraction is an ordinary lookup and the
//   result is re-rendered by [`format`] on the way out.
// * `json` keeps the raw input text, and PG's operators return the **verbatim
//   source substring** of the matched value — inner whitespace, key order,
//   duplicate keys and unnormalized numbers all survive. So the `json_*`
//   helpers below scan the raw bytes and return `&str` subslices of the input.
//
// Every accessor returns `None` (SQL NULL, not an error) for a missing key, a
// wrong container kind, or an out-of-range subscript, which is what PG does.

/// Resolve a possibly-negative subscript against a container of `len` elements.
/// A negative subscript counts from the end (`-1` is the last element).
fn resolve_index(idx: i64, len: usize) -> Option<usize> {
    let i = if idx < 0 {
        i64::try_from(len).ok()?.checked_add(idx)?
    } else {
        idx
    };
    let i = usize::try_from(i).ok()?;
    (i < len).then_some(i)
}

/// A `#>` path element used as an array subscript. PG's `strtoint` skips leading
/// whitespace, accepts a leading sign, and rejects trailing characters, which is
/// Rust's `parse::<i64>` once the leading whitespace is gone. A value that does
/// not fit an `i64` yields `None`, i.e. SQL NULL, as PG does.
///
/// The skip is the C `isspace` set, which is neither of the obvious candidates:
/// `str::trim_start` also strips the Unicode `White_Space` set (U+00A0,
/// U+2000..200A, U+3000, ...) that `strtol` does not recognize, while Rust's
/// `is_ascii_whitespace` omits the vertical tab that it does. Both were verified
/// against PostgreSQL — `#> ARRAY[e'\v1']` extracts, `#> ARRAY['<U+3000>1']` is
/// NULL. This is a wider set than [`is_json_ws`] on purpose: it describes what
/// `strtol` accepts, not what the JSON grammar calls whitespace.
fn path_index(step: &str) -> Option<i64> {
    step.trim_start_matches([' ', '\t', '\n', '\u{0b}', '\u{0c}', '\r'])
        .parse::<i64>()
        .ok()
}

/// The value of `key` in the jsonb object `value`. `None` if `value` is not an
/// object or has no such key.
pub fn jsonb_object_field<'a>(value: &'a Jsonb, key: &str) -> Option<&'a Jsonb> {
    let pairs = match value {
        Jsonb::Object(pairs) => pairs,
        _ => return None,
    };
    // Objects are sorted by `key_cmp` (see `canonicalize_object`), so the lookup
    // is a binary search under that same order. Note the argument direction:
    // `key_cmp(stored, wanted)`.
    let at = pairs.binary_search_by(|(k, _)| key_cmp(k, key)).ok()?;
    Some(&pairs[at].1)
}

/// Element `idx` of the jsonb array `value`; negative counts from the end.
/// `None` if `value` is not an array or the subscript is out of range.
pub fn jsonb_array_element(value: &Jsonb, idx: i64) -> Option<&Jsonb> {
    let items = match value {
        Jsonb::Array(items) => items,
        _ => return None,
    };
    items.get(resolve_index(idx, items.len())?)
}

/// Walk `path` through `value`: a step into an object matches a key, a step into
/// an array must parse as a subscript. An empty path returns `value` itself.
pub fn jsonb_extract_path<'a>(value: &'a Jsonb, path: &[&str]) -> Option<&'a Jsonb> {
    let mut cur = value;
    for step in path {
        cur = match cur {
            Jsonb::Object(_) => jsonb_object_field(cur, step)?,
            Jsonb::Array(_) => jsonb_array_element(cur, path_index(step)?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// The `->>` / `#>>` projection of a jsonb value: a string yields its bare
/// content (no quotes, no escapes), JSON `null` yields SQL NULL, and everything
/// else yields its canonical rendering.
pub fn jsonb_as_text(value: &Jsonb) -> Option<String> {
    match value {
        Jsonb::Null => None,
        Jsonb::String(s) => Some(s.clone()),
        other => Some(format(other)),
    }
}

/// The four characters JSON counts as insignificant whitespace.
///
/// Deliberately **not** `u8::is_ascii_whitespace`, which also accepts a form feed
/// (0x0C). Mixing the two definitions is a liveness bug, not a cosmetic one: if
/// [`raw_skip_value`]'s scalar-token loop treats a byte as whitespace while its
/// `match` does not, the cursor stops advancing and the scan spins forever. Every
/// whitespace test in the raw scanner goes through here so the two can't drift.
fn is_json_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// Skip whitespace from `i`, returning the offset of the next other byte.
fn raw_skip_ws(b: &[u8], mut i: usize) -> usize {
    while matches!(b.get(i), Some(&c) if is_json_ws(c)) {
        i += 1;
    }
    i
}

/// Skip a JSON string starting at its opening quote; returns the offset just
/// past the closing quote. A backslash always consumes the following byte, which
/// also covers `\uXXXX` (the four hex digits are ordinary bytes here).
fn raw_skip_string(b: &[u8], mut i: usize) -> Option<usize> {
    i += 1;
    loop {
        match *b.get(i)? {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
}

/// Skip one JSON value starting at `i`; returns the offset just past its last
/// byte, so the caller gets the value's trimmed extent for free.
///
/// Deliberately **iterative** — one depth counter, no recursion — so deeply
/// nested input cannot overflow the stack and [`MAX_DEPTH`] does not apply. It
/// is also **total**: any malformed input yields `None` rather than a panic or a
/// hang, which matters because `Value::Json` is reconstructed from a heap tuple
/// without being re-validated.
///
/// Totality rests on every arm advancing `i` by at least one byte. The `_` arm is
/// the delicate one: it is reached only for a byte that is neither `"`, nor
/// [`is_structural`], nor [`is_json_ws`] — exactly the bytes its loop condition
/// consumes — so it always makes progress. Widening either predicate without
/// widening the other reintroduces a spin.
fn raw_skip_value(b: &[u8], mut i: usize) -> Option<usize> {
    let mut depth = 0usize;
    loop {
        match *b.get(i)? {
            b'"' => i = raw_skip_string(b, i)?,
            b'[' | b'{' => {
                depth += 1;
                i += 1;
                continue;
            }
            b']' | b'}' => {
                depth = depth.checked_sub(1)?;
                i += 1;
            }
            b',' | b':' => {
                if depth == 0 {
                    return None;
                }
                i += 1;
                continue;
            }
            c if is_json_ws(c) => {
                i += 1;
                continue;
            }
            // A scalar token (number / true / false / null) runs to the next
            // structural character or whitespace.
            _ => {
                while matches!(b.get(i), Some(&c) if !is_structural(c) && !is_json_ws(c)) {
                    i += 1;
                }
            }
        }
        if depth == 0 {
            return Some(i);
        }
    }
}

/// Compare a raw (still-quoted) object key against `key`. Fast path: a key with
/// no escapes compares as raw bytes with no allocation, which matters because
/// every key of every object on the path is compared.
fn raw_key_matches(raw: &[u8], key: &str) -> bool {
    let inner = &raw[1..raw.len() - 1];
    if !inner.contains(&b'\\') {
        return inner == key.as_bytes();
    }
    // Escaped key: decode through the shared string parser. `is_jsonb` is false
    // here — a `\u0000` inside a key is a legitimate key to compare against, not
    // a `text` datum being produced.
    Parser::at(raw, 0, "json", false)
        .parse_string()
        .is_ok_and(|k| k == key)
}

/// The verbatim source text of `key`'s value in the json object `doc`, with the
/// outer whitespace trimmed but the inner whitespace preserved. `None` when
/// `doc` is not an object, has no such key, or is malformed.
///
/// Scans the **whole** object rather than stopping at the first hit: `json`
/// preserves duplicate keys and PG's operator returns the last occurrence.
pub fn json_object_field<'a>(doc: &'a str, key: &str) -> Option<&'a str> {
    let b = doc.as_bytes();
    let mut i = raw_skip_ws(b, 0);
    if *b.get(i)? != b'{' {
        return None;
    }
    i += 1;
    let mut found: Option<(usize, usize)> = None;
    loop {
        i = raw_skip_ws(b, i);
        match *b.get(i)? {
            b'}' => return found.map(|(s, e)| &doc[s..e]),
            b'"' => {}
            _ => return None,
        }
        let kstart = i;
        i = raw_skip_string(b, i)?;
        let matched = raw_key_matches(&b[kstart..i], key);
        i = raw_skip_ws(b, i);
        if *b.get(i)? != b':' {
            return None;
        }
        i = raw_skip_ws(b, i + 1);
        let vstart = i;
        i = raw_skip_value(b, vstart)?;
        if matched {
            found = Some((vstart, i));
        }
        i = raw_skip_ws(b, i);
        match *b.get(i)? {
            b',' => i += 1,
            b'}' => return found.map(|(s, e)| &doc[s..e]),
            _ => return None,
        }
    }
}

/// Count the elements of the json array whose `[` sits at `start`.
fn raw_array_len(b: &[u8], start: usize) -> Option<usize> {
    let mut i = raw_skip_ws(b, start + 1);
    if *b.get(i)? == b']' {
        return Some(0);
    }
    let mut n = 0usize;
    loop {
        i = raw_skip_value(b, raw_skip_ws(b, i))?;
        n += 1;
        i = raw_skip_ws(b, i);
        match *b.get(i)? {
            b',' => i += 1,
            b']' => return Some(n),
            _ => return None,
        }
    }
}

/// The verbatim source text of element `idx` of the json array `doc`; negative
/// counts from the end. `None` when `doc` is not an array, the subscript is out
/// of range, or the input is malformed.
pub fn json_array_element(doc: &str, idx: i64) -> Option<&str> {
    let b = doc.as_bytes();
    let start = raw_skip_ws(b, 0);
    if *b.get(start)? != b'[' {
        return None;
    }
    // A negative subscript needs the element count, so it costs a counting pass
    // first. Cheaper than materializing a span per element on every lookup —
    // arrays can be large and negative subscripts are rare.
    let want = if idx < 0 {
        resolve_index(idx, raw_array_len(b, start)?)?
    } else {
        usize::try_from(idx).ok()?
    };
    let mut i = start + 1;
    let mut at = 0usize;
    loop {
        i = raw_skip_ws(b, i);
        if *b.get(i)? == b']' {
            return None;
        }
        let vstart = i;
        i = raw_skip_value(b, vstart)?;
        if at == want {
            return Some(&doc[vstart..i]);
        }
        at += 1;
        i = raw_skip_ws(b, i);
        match *b.get(i)? {
            b',' => i += 1,
            _ => return None,
        }
    }
}

/// The value in `doc` with its outer whitespace trimmed and nothing else
/// changed.
fn json_trim(doc: &str) -> Option<&str> {
    let b = doc.as_bytes();
    let start = raw_skip_ws(b, 0);
    let end = raw_skip_value(b, start)?;
    Some(&doc[start..end])
}

/// Walk `path` through the json document `doc`, returning the verbatim source
/// text of the value it lands on. An empty path returns the whole value, trimmed
/// — PG drops the outer whitespace that the `json` input preserved.
pub fn json_extract_path<'a>(doc: &'a str, path: &[&str]) -> Option<&'a str> {
    // Only the empty path needs the trim, and only for its own sake — it is the
    // one case that returns `doc` itself. Trimming unconditionally would scan the
    // whole document on every call just to throw the span away, doubling the cost
    // of `#>` against the equivalent `->`.
    if path.is_empty() {
        return json_trim(doc);
    }
    let mut cur = doc;
    for step in path {
        // The accessors skip their own leading whitespace and hand back trimmed
        // spans, so only the first step can see any — an O(whitespace) skip, not
        // an O(document) one.
        let b = cur.as_bytes();
        cur = match *b.get(raw_skip_ws(b, 0))? {
            b'{' => json_object_field(cur, step)?,
            b'[' => json_array_element(cur, path_index(step)?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// The `->>` / `#>>` projection of a raw json value: a JSON string is unquoted
/// and unescaped, JSON `null` is SQL NULL (`Ok(None)`), and everything else is
/// its verbatim source text (so `1e2` and `1.500` stay as written, unlike the
/// jsonb path which normalizes through `numeric`).
///
/// `Err` only for `\u0000`, which cannot become a `text` datum.
pub fn json_as_text(value: &str) -> Result<Option<String>, JsonError> {
    let b = value.as_bytes();
    let i = raw_skip_ws(b, 0);
    if b.get(i) == Some(&b'"') {
        // Decode through the shared string parser so escapes, surrogate pairs
        // and PG's error DETAILs all match the input path. `is_jsonb` is true
        // because the result becomes a `text` datum — which is exactly why
        // `\u0000` is rejected here even though `json_in` accepted it.
        return Parser::at(b, i, "json", true).parse_string().map(Some);
    }
    // Trim with the JSON whitespace set, not `str::trim`'s Unicode one: an
    // NBSP or U+2028 is an ordinary character here and must survive into the
    // `text` result.
    let end = raw_skip_value(b, i).unwrap_or(b.len());
    let trimmed = &value[i..end];
    if trimmed == "null" {
        return Ok(None);
    }
    Ok(Some(trimmed.to_string()))
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

    #[test]
    fn deep_nesting_errors_instead_of_overflowing() {
        // Just past the limit returns a controlled 54001 error, not a crash.
        let deep = "[".repeat(MAX_DEPTH + 5) + &"]".repeat(MAX_DEPTH + 5);
        let err = jsonb_in(&deep).unwrap_err();
        assert_eq!(err.sqlstate, PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(err.message, "stack depth limit exceeded");
        // A document nested right up to the limit still parses.
        let ok = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        assert!(jsonb_in(&ok).is_ok());
    }

    #[test]
    fn jsonb_rejects_nul_escape_but_json_keeps_it() {
        let err = jsonb_in("\"\\u0000\"").unwrap_err();
        assert_eq!(err.sqlstate, UNTRANSLATABLE_CHARACTER);
        assert_eq!(err.message, "unsupported Unicode escape sequence");
        assert_eq!(err.detail.as_deref(), Some("\\u0000 cannot be converted to text."));
        // `json` preserves the raw text verbatim.
        assert_eq!(json_in("\"\\u0000\"").unwrap(), "\"\\u0000\"");
    }

    #[test]
    fn numeric_overflow_is_out_of_range() {
        let err = jsonb_in("1e1000000").unwrap_err();
        assert_eq!(err.sqlstate, NUMERIC_VALUE_OUT_OF_RANGE);
        assert_eq!(err.message, "value overflows numeric format");
    }

    #[test]
    fn object_error_detail_matches_pg_position() {
        // After a comma only a key is valid (no "or }").
        let err = jsonb_in("{\"a\":1,}").unwrap_err();
        assert_eq!(err.detail.as_deref(), Some("Expected string, but found \"}\"."));
        // At the first key, "}" is offered as an alternative.
        let err = jsonb_in("{,}").unwrap_err();
        assert_eq!(err.detail.as_deref(), Some("Expected string or \"}\", but found \",\"."));
        // Two high surrogates in a row is a distinct message.
        let err = jsonb_in("\"\\ud800\\ud800\"").unwrap_err();
        assert_eq!(err.detail.as_deref(), Some("Unicode high surrogate must not follow a high surrogate."));
    }

    #[test]
    fn canonicalize_dedups_last_wins_at_scale() -> Result<()> {
        // Exercises the O(n log n) canonicalizer with duplicate keys.
        let obj = "{\"a\":1,\"b\":2,\"a\":3,\"c\":4,\"b\":5}";
        assert_eq!(out(obj)?, "{\"a\": 3, \"b\": 5, \"c\": 4}");
        Ok(())
    }

    // --- extraction operators (`->`, `->>`, `#>`, `#>>`) ---

    #[test]
    fn json_extraction_returns_verbatim_source_spans() {
        // The outer whitespace is trimmed but the inner spelling survives --
        // this is what distinguishes `json` from a jsonb round-trip.
        let doc = "{\"a\":   [ 1,2 ]  , \"b\":2}";
        assert_eq!(json_object_field(doc, "a"), Some("[ 1,2 ]"));
        assert_eq!(json_object_field(doc, "b"), Some("2"));
        // A nested object is not re-rendered (jsonb would print `{"b": 1}`).
        assert_eq!(json_extract_path("{\"a\":{\"b\":1}}", &["a"]), Some("{\"b\":1}"));
        // Numbers keep their written form rather than normalizing through numeric.
        assert_eq!(json_object_field("{\"a\":1e2}", "a"), Some("1e2"));
        assert_eq!(json_object_field("{\"a\":1.500}", "a"), Some("1.500"));
    }

    #[test]
    fn json_object_field_keeps_the_last_duplicate() {
        // `json` preserves duplicate keys, and PG's operator returns the last --
        // so the lookup cannot stop at the first match.
        assert_eq!(json_object_field("{\"a\": 1,  \"a\": 2}", "a"), Some("2"));
        assert_eq!(json_object_field("{\"a\":1,\"b\":2,\"a\":3}", "a"), Some("3"));
    }

    #[test]
    fn json_object_field_matches_escaped_keys() {
        // Slow path: the stored key carries an escape and must be decoded.
        assert_eq!(json_object_field("{\"a\\u0062\":1}", "ab"), Some("1"));
        assert_eq!(json_object_field("{\"a\\tb\":1}", "a\tb"), Some("1"));
        // Fast path: no escape, compared as raw bytes.
        assert_eq!(json_object_field("{\"ab\":1}", "ab"), Some("1"));
        // A key that only looks similar must not match.
        assert_eq!(json_object_field("{\"ab\":1}", "a"), None);
        // A quote inside the key does not end it early.
        assert_eq!(json_object_field("{\"a\\\"b\":1}", "a\"b"), Some("1"));
    }

    #[test]
    fn json_array_element_handles_negative_and_out_of_range() {
        let doc = "[10, 20 , 30]";
        assert_eq!(json_array_element(doc, 0), Some("10"));
        assert_eq!(json_array_element(doc, 1), Some("20"));
        assert_eq!(json_array_element(doc, -1), Some("30"));
        assert_eq!(json_array_element(doc, -3), Some("10"));
        assert_eq!(json_array_element(doc, -4), None);
        assert_eq!(json_array_element(doc, 3), None);
        assert_eq!(json_array_element("[]", 0), None);
        assert_eq!(json_array_element("[]", -1), None);
    }

    #[test]
    fn json_extraction_misses_are_none_not_errors() {
        // A wrong container kind is NULL in PG, not an error.
        assert_eq!(json_object_field("[1,2]", "a"), None);
        assert_eq!(json_object_field("\"s\"", "a"), None);
        assert_eq!(json_object_field("42", "a"), None);
        assert_eq!(json_array_element("{\"a\":1}", 0), None);
        assert_eq!(json_object_field("{\"a\":1}", "zz"), None);
        // A path that runs past the end, and a non-integer step into an array.
        assert_eq!(json_extract_path("{\"a\":1}", &["a", "b"]), None);
        assert_eq!(json_extract_path("[1,2]", &["xx"]), None);
        // A negative step into an array does work.
        assert_eq!(json_extract_path("[1,2,3]", &["-1"]), Some("3"));
    }

    #[test]
    fn json_extract_path_trims_on_an_empty_path() {
        // PG drops the outer whitespace the `json` input preserved.
        assert_eq!(json_extract_path("  {\"a\":1}  ", &[]), Some("{\"a\":1}"));
        assert_eq!(json_extract_path("{\"a\":{\"b\":[1,2]}}", &["a", "b", "1"]), Some("2"));
    }

    #[test]
    fn json_as_text_unescapes_only_strings() -> Result<()> {
        let t = |s: &str| json_as_text(s).map_err(|e| anyhow!("{}", e.message));
        // A string is unquoted and unescaped.
        assert_eq!(t("\"x\\tz\"")?, Some("x\tz".to_string()));
        assert_eq!(t("\"\\u00e9\"")?, Some("é".to_string()));
        // Everything else is the verbatim source text.
        assert_eq!(t("1.500")?, Some("1.500".to_string()));
        assert_eq!(t("1e2")?, Some("1e2".to_string()));
        assert_eq!(t("{\"b\" : 1}")?, Some("{\"b\" : 1}".to_string()));
        assert_eq!(t("true")?, Some("true".to_string()));
        // JSON null is SQL NULL.
        assert_eq!(t("null")?, None);
        // A NUL cannot become a `text` datum, even though `json_in` accepts it.
        let err = json_as_text("\"\\u0000\"").unwrap_err();
        assert_eq!(err.sqlstate, UNTRANSLATABLE_CHARACTER);
        assert_eq!(err.detail.as_deref(), Some("\\u0000 cannot be converted to text."));
        Ok(())
    }

    #[test]
    fn json_scanner_is_total_on_malformed_input() {
        // `Value::Json` is rebuilt from a heap tuple without re-validation, so
        // the scanner must never panic or hang on text that is not valid JSON.
        // Every control byte is included: a form feed (0x0C) once spun forever,
        // because `is_ascii_whitespace` accepts it while the JSON whitespace set
        // does not, so the scalar-token loop consumed nothing and the cursor
        // stopped advancing. See `is_json_ws`.
        let controls: Vec<String> = (0u8..=0x20)
            .flat_map(|c| {
                let c = c as char;
                [format!("[{c}]"), format!("{{\"a\":{c}}}"), format!("{{\"a\":{c}1}}"), format!("[1,{c}2]")]
            })
            .collect();
        let fixed = [
            "", "{", "}", "[", "{\"a\"", "{\"a\":", "{\"a\":[1,2", "[1,2", "\"unterminated",
            "{\"a\":1,", "{,}", "[,]", "{\"a\" 1}", "\\", "{\"a\":\"\\",
        ];
        for doc in fixed.iter().map(|s| s.to_string()).chain(controls) {
            let doc = doc.as_str();
            let _ = json_object_field(doc, "a");
            let _ = json_array_element(doc, 0);
            let _ = json_array_element(doc, -1);
            let _ = json_extract_path(doc, &["a"]);
            let _ = json_extract_path(doc, &["0"]);
            let _ = json_extract_path(doc, &[]);
            let _ = json_as_text(doc);
        }
    }

    #[test]
    fn json_scanner_does_not_recurse() {
        // The scanner is iterative, so MAX_DEPTH does not apply to it and deep
        // nesting cannot overflow the stack -- unlike the tree parser.
        let deep = format!("{}1{}", "[".repeat(100_000), "]".repeat(100_000));
        assert_eq!(json_extract_path(&deep, &[]), Some(deep.as_str()));
        // Unbalanced-deep input terminates rather than running away.
        let unbalanced = "[".repeat(100_000);
        assert_eq!(json_array_element(&unbalanced, 0), None);
        // A deep document nested well past MAX_DEPTH still resolves a step.
        let n = MAX_DEPTH * 50;
        let nested = format!("{{\"a\":{}1{}}}", "[".repeat(n), "]".repeat(n));
        assert!(json_object_field(&nested, "a").is_some());
    }

    #[test]
    fn jsonb_accessors_follow_canonical_key_order() -> Result<()> {
        // Compare the rendered form: `Numeric` has no `From<i32>`, and rendering
        // is what the operator ultimately returns anyway.
        let shown = |v: Option<&Jsonb>| v.map(format);
        // Keys are stored shorter-first, so the binary search must use key_cmp.
        let obj = jb("{\"aa\":1,\"b\":2,\"ccc\":3}")?;
        assert_eq!(shown(jsonb_object_field(&obj, "b")).as_deref(), Some("2"));
        assert_eq!(shown(jsonb_object_field(&obj, "aa")).as_deref(), Some("1"));
        assert_eq!(shown(jsonb_object_field(&obj, "ccc")).as_deref(), Some("3"));
        assert_eq!(jsonb_object_field(&obj, "zz"), None);
        // Wrong container kinds and out-of-range subscripts are None.
        let arr = jb("[10,20,30]")?;
        assert_eq!(jsonb_object_field(&arr, "a"), None);
        assert_eq!(jsonb_array_element(&obj, 0), None);
        assert_eq!(shown(jsonb_array_element(&arr, -1)).as_deref(), Some("30"));
        assert_eq!(jsonb_array_element(&arr, 3), None);
        assert_eq!(jsonb_array_element(&arr, -4), None);
        // Paths walk both container kinds; an empty path is the value itself.
        let doc = jb("{\"a\":{\"b\":[\"c\",\"d\"]}}")?;
        assert_eq!(shown(jsonb_extract_path(&doc, &["a", "b", "1"])).as_deref(), Some("\"d\""));
        assert_eq!(shown(jsonb_extract_path(&doc, &["a", "b", "-1"])).as_deref(), Some("\"d\""));
        assert_eq!(jsonb_extract_path(&doc, &[]), Some(&doc));
        assert_eq!(jsonb_extract_path(&doc, &["a", "zz"]), None);
        Ok(())
    }

    #[test]
    fn jsonb_as_text_renders_canonically() -> Result<()> {
        let t = |s: &str| -> Result<Option<String>> { Ok(jsonb_as_text(&jb(s)?)) };
        // A string loses its quotes and escapes; JSON null is SQL NULL.
        assert_eq!(t("\"x\\tz\"")?, Some("x\tz".to_string()));
        assert_eq!(t("null")?, None);
        // Everything else is the canonical rendering -- numeric scale is kept,
        // but the exponent form is normalized (unlike the `json` path).
        assert_eq!(t("1.500")?, Some("1.500".to_string()));
        assert_eq!(t("1e2")?, Some("100".to_string()));
        assert_eq!(t("{\"b\" : 1}")?, Some("{\"b\": 1}".to_string()));
        assert_eq!(t("true")?, Some("true".to_string()));
        Ok(())
    }

    #[test]
    fn path_index_matches_pg_strtoint() {
        // Leading whitespace and a sign are accepted; trailing text is not.
        assert_eq!(path_index("1"), Some(1));
        assert_eq!(path_index(" 1"), Some(1));
        assert_eq!(path_index("+1"), Some(1));
        assert_eq!(path_index("-1"), Some(-1));
        assert_eq!(path_index("1 "), None);
        assert_eq!(path_index("xx"), None);
        assert_eq!(path_index(""), None);
        // Out of i64 range yields NULL rather than wrapping.
        assert_eq!(path_index("99999999999999999999"), None);
        // Only ASCII whitespace is skipped. PG's C-locale `strtol` does not
        // recognize the Unicode spaces, so a step prefixed with one is not a
        // number and must be NULL -- `str::trim_start` would wrongly accept it.
        assert_eq!(path_index("\u{3000}1"), None);
        assert_eq!(path_index("\u{00a0}1"), None);
        assert_eq!(path_index("\u{2003}1"), None);
        // ...but every character C's `isspace` accepts is, including the
        // vertical tab that Rust's `is_ascii_whitespace` leaves out. Verified
        // against PostgreSQL: `'[1,2,3]'::json #> ARRAY[e'\v1']` yields 2.
        for c in [' ', '\t', '\n', '\u{0b}', '\u{0c}', '\r'] {
            assert_eq!(path_index(&format!("{c}1")), Some(1), "C isspace {:?}", c);
        }
    }
}
