//! One-dimensional array I/O (`array_out` / `array_in`) and the element↔array
//! OID mapping.
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable* array
//! text format — the `{...}` syntax, its quoting/escaping rules, and the
//! case-insensitive unquoted `NULL` element — implemented independently. Only
//! 1-D arrays are handled; a nested `{` is rejected as unsupported.

use crate::{PgType, Value, cast, oid};

/// SQLSTATE + message (+ optional DETAIL) for a failed array input (`array_in`).
/// The DETAIL mirrors PG's `array_in` (e.g. `Unexpected "," character.`); like
/// `json`, it is carried through the binder's literal-input path and dropped on
/// the runtime cast path (which has no DETAIL channel).
#[derive(Clone, Debug, PartialEq)]
pub struct ArrayError {
    pub sqlstate: &'static str,
    pub message: String,
    pub detail: Option<&'static str>,
}

const INVALID_TEXT_REPRESENTATION: &str = "22P02";

fn malformed(s: &str, detail: &'static str) -> ArrayError {
    ArrayError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("malformed array literal: \"{s}\""),
        detail: Some(detail),
    }
}

// PG's `array_in` DETAIL strings.
const DETAIL_START: &str = "Array value must start with \"{\" or dimension information.";
const DETAIL_EOF: &str = "Unexpected end of input.";
const DETAIL_JUNK: &str = "Junk after closing right brace.";
const DETAIL_COMMA: &str = "Unexpected \",\" character.";
const DETAIL_RBRACE: &str = "Unexpected \"}\" character.";
const DETAIL_LBRACE: &str = "Unexpected \"{\" character.";

/// The array type OID for an element type OID (`int4` → `_int4` = 1007), or
/// `None` when this build has no array type for that element.
pub fn array_oid_for_elem(elem: u32) -> Option<u32> {
    ARRAY_OID_PAIRS
        .iter()
        .find(|(e, _)| *e == elem)
        .map(|(_, a)| *a)
}

/// The element type OID for an array type OID (`_int4` = 1007 → `int4`), the
/// reverse of [`array_oid_for_elem`]. Used by [`PgType::from_oid`] to decode a
/// declared array parameter OID.
pub fn elem_oid_for_array(arr: u32) -> Option<u32> {
    ARRAY_OID_PAIRS
        .iter()
        .find(|(_, a)| *a == arr)
        .map(|(e, _)| *e)
}

/// (element OID, array OID) pairs, matching PostgreSQL's `pg_type.typarray`.
const ARRAY_OID_PAIRS: &[(u32, u32)] = &[
    (oid::BOOL, oid::BOOL_ARRAY),
    (oid::BYTEA, oid::BYTEA_ARRAY),
    (oid::NAME, oid::NAME_ARRAY),
    (oid::INT2, oid::INT2_ARRAY),
    (oid::INT4, oid::INT4_ARRAY),
    (oid::INT8, oid::INT8_ARRAY),
    (oid::TEXT, oid::TEXT_ARRAY),
    (oid::VARCHAR, oid::VARCHAR_ARRAY),
    (oid::BPCHAR, oid::BPCHAR_ARRAY),
    (oid::OID, oid::OID_ARRAY),
    (oid::FLOAT4, oid::FLOAT4_ARRAY),
    (oid::FLOAT8, oid::FLOAT8_ARRAY),
    (oid::NUMERIC, oid::NUMERIC_ARRAY),
    (oid::MONEY, oid::MONEY_ARRAY),
    (oid::POINT, oid::POINT_ARRAY),
    (oid::LSEG, oid::LSEG_ARRAY),
    (oid::MACADDR, oid::MACADDR_ARRAY),
    (oid::MACADDR8, oid::MACADDR8_ARRAY),
    (oid::INET, oid::INET_ARRAY),
    (oid::CIDR, oid::CIDR_ARRAY),
    (oid::UUID, oid::UUID_ARRAY),
    (oid::JSON, oid::JSON_ARRAY),
    (oid::JSONB, oid::JSONB_ARRAY),
    (oid::JSONPATH, oid::JSONPATH_ARRAY),
    (oid::DATE, oid::DATE_ARRAY),
    (oid::TIME, oid::TIME_ARRAY),
    (oid::TIMETZ, oid::TIMETZ_ARRAY),
    (oid::TIMESTAMP, oid::TIMESTAMP_ARRAY),
    (oid::TIMESTAMPTZ, oid::TIMESTAMPTZ_ARRAY),
    (oid::INTERVAL, oid::INTERVAL_ARRAY),
    (oid::BIT, oid::BIT_ARRAY),
    (oid::VARBIT, oid::VARBIT_ARRAY),
];

/// `array_out`: render a 1-D array as `{e1,e2,...}`. A NULL element prints as an
/// unquoted `NULL`; any other element is rendered with its own output function
/// and double-quoted when it is empty, equals `NULL` case-insensitively, or
/// contains a delimiter, brace, quote, backslash, or whitespace.
pub fn format(elems: &[Value], efd: i32) -> String {
    let mut out = String::from("{");
    for (i, v) in elems.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match v {
            Value::Null => out.push_str("NULL"),
            _ => {
                let s = v.encode_text_with(efd).unwrap_or_default();
                if needs_quote(&s) {
                    push_quoted(&mut out, &s);
                } else {
                    out.push_str(&s);
                }
            }
        }
    }
    out.push('}');
    out
}

/// PG's `array_isspace`: the six ASCII whitespace characters array I/O treats as
/// whitespace. Deliberately not Rust's Unicode-aware `char::is_whitespace`, which
/// would over-quote elements containing e.g. a non-breaking space.
fn is_array_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

fn needs_quote(s: &str) -> bool {
    s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.chars()
            .any(|c| matches!(c, '{' | '}' | ',' | '"' | '\\') || is_array_space(c))
}

fn push_quoted(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
}

/// `array_in`: parse a 1-D array literal `{...}` into element values, coercing
/// each element token to `elem` through the shared cast machinery (so an element
/// parses exactly like the same scalar literal). An unquoted, case-insensitive
/// `NULL` token is a NULL element; a quoted `"NULL"` is the text "NULL".
pub fn array_in(input: &str, elem: PgType) -> Result<Vec<Value>, ArrayError> {
    let trimmed = input.trim();
    let mut chars = trimmed.chars().peekable();
    if chars.next() != Some('{') {
        return Err(malformed(input, DETAIL_START));
    }
    let mut elems = Vec::new();
    skip_ws(&mut chars);
    // An empty array literal `{}` yields no elements.
    if chars.peek() == Some(&'}') {
        chars.next();
        skip_ws(&mut chars);
        if chars.next().is_some() {
            return Err(malformed(input, DETAIL_JUNK));
        }
        return Ok(elems);
    }
    loop {
        let (token, quoted) = read_element(&mut chars, input)?;
        // An empty, unquoted, unescaped token (`{a,,c}`, `{1,}`, `{,1}`) is a
        // missing element, which PG rejects as malformed. A quoted `""` is a
        // legitimate empty-string element and keeps `quoted = true`. PG's DETAIL
        // names the character that follows the missing element.
        if !quoted && token.is_empty() {
            let detail = match chars.peek() {
                Some('}') => DETAIL_RBRACE,
                Some(',') => DETAIL_COMMA,
                _ => DETAIL_EOF,
            };
            return Err(malformed(input, detail));
        }
        if !quoted && token.eq_ignore_ascii_case("null") {
            elems.push(Value::Null);
        } else {
            let v = cast::cast_value(Value::Text(token), elem, 1).map_err(|e| ArrayError {
                sqlstate: e.sqlstate,
                message: e.message,
                detail: None,
            })?;
            elems.push(v);
        }
        skip_ws(&mut chars);
        match chars.next() {
            Some(',') => {
                skip_ws(&mut chars);
            }
            Some('}') => break,
            // EOF before a closing brace, or any other stray character.
            None => return Err(malformed(input, DETAIL_EOF)),
            Some(_) => return Err(malformed(input, DETAIL_COMMA)),
        }
    }
    skip_ws(&mut chars);
    if chars.next().is_some() {
        return Err(malformed(input, DETAIL_JUNK));
    }
    Ok(elems)
}

fn skip_ws(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while matches!(chars.peek(), Some(&c) if is_array_space(c)) {
        chars.next();
    }
}

/// Read one element token, returning its unescaped text and whether any part was
/// double-quoted or backslash-escaped (which forces it to text and disables the
/// NULL keyword). Leaves the iterator positioned on the following delimiter
/// (`,`/`}`). Trailing **unquoted, unescaped** whitespace is trimmed, but
/// whitespace that was quoted or escaped is significant and kept.
fn read_element(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    input: &str,
) -> Result<(String, bool), ArrayError> {
    if chars.peek() == Some(&'"') {
        chars.next();
        let mut s = String::new();
        loop {
            match chars.next() {
                Some('\\') => match chars.next() {
                    Some(c) => s.push(c),
                    None => return Err(malformed(input, DETAIL_EOF)),
                },
                Some('"') => return Ok((s, true)),
                Some(c) => s.push(c),
                None => return Err(malformed(input, DETAIL_EOF)),
            }
        }
    }
    // Unquoted: read until a delimiter, honoring backslash escapes and interior
    // quotes. `last_sig` tracks the length up to the last significant (non-
    // whitespace, or quoted/escaped) character, so trailing unquoted whitespace
    // is dropped while an escaped/quoted trailing space is preserved.
    let mut s = String::new();
    let mut forced_text = false;
    let mut last_sig = 0usize;
    loop {
        match chars.peek() {
            Some(',') | Some('}') | None => break,
            Some('{') => return Err(malformed(input, DETAIL_LBRACE)),
            Some('\\') => {
                chars.next();
                match chars.next() {
                    Some(c) => {
                        s.push(c);
                        forced_text = true;
                        last_sig = s.len();
                    }
                    None => return Err(malformed(input, DETAIL_EOF)),
                }
            }
            Some('"') => {
                chars.next();
                forced_text = true;
                loop {
                    match chars.next() {
                        Some('\\') => match chars.next() {
                            Some(c) => s.push(c),
                            None => return Err(malformed(input, DETAIL_EOF)),
                        },
                        Some('"') => break,
                        Some(c) => s.push(c),
                        None => return Err(malformed(input, DETAIL_EOF)),
                    }
                }
                last_sig = s.len();
            }
            Some(&c) => {
                chars.next();
                s.push(c);
                if !is_array_space(c) {
                    last_sig = s.len();
                }
            }
        }
    }
    s.truncate(last_sig);
    Ok((s, forced_text))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_int_array() {
        let elems = array_in("{1,2,3}", PgType::Int4).unwrap();
        assert_eq!(
            elems,
            vec![Value::Int4(1), Value::Int4(2), Value::Int4(3)]
        );
        assert_eq!(format(&elems, 1), "{1,2,3}");
    }

    #[test]
    fn empty_array() {
        assert_eq!(array_in("{}", PgType::Int4).unwrap(), vec![]);
        assert_eq!(format(&[], 1), "{}");
    }

    #[test]
    fn null_and_quoting() {
        let elems = array_in(r#"{a,"b,c",NULL,"NULL",""}"#, PgType::Text).unwrap();
        assert_eq!(
            elems,
            vec![
                Value::Text("a".into()),
                Value::Text("b,c".into()),
                Value::Null,
                Value::Text("NULL".into()),
                Value::Text("".into()),
            ]
        );
        // Round-trip: the delimiter/empty/NULL-lookalike elements are quoted.
        assert_eq!(format(&elems, 1), r#"{a,"b,c",NULL,"NULL",""}"#);
    }

    #[test]
    fn whitespace_between_elements_is_trimmed() {
        let elems = array_in("{ 1 , 2 , 3 }", PgType::Int4).unwrap();
        assert_eq!(elems, vec![Value::Int4(1), Value::Int4(2), Value::Int4(3)]);
    }

    #[test]
    fn backslash_escape_in_quotes() {
        let elems = array_in(r#"{"a\"b","c\\d"}"#, PgType::Text).unwrap();
        assert_eq!(
            elems,
            vec![Value::Text("a\"b".into()), Value::Text("c\\d".into())]
        );
    }

    #[test]
    fn malformed_missing_braces() {
        assert!(array_in("1,2,3", PgType::Int4).is_err());
        assert!(array_in("{1,2", PgType::Int4).is_err());
    }

    #[test]
    fn malformed_detail_matches_pg() {
        // DETAIL strings verified against PostgreSQL's array_in.
        let d = |s: &str| array_in(s, PgType::Text).unwrap_err().detail.unwrap();
        assert_eq!(d("1,2,3"), DETAIL_START);
        assert_eq!(d("abc"), DETAIL_START);
        assert_eq!(d("{1,2"), DETAIL_EOF);
        assert_eq!(d("{1,2}}"), DETAIL_JUNK);
        assert_eq!(d("{1,2} junk"), DETAIL_JUNK);
        assert_eq!(d("{a,,c}"), DETAIL_COMMA);
        assert_eq!(d("{,1}"), DETAIL_COMMA);
        assert_eq!(d("{1,}"), DETAIL_RBRACE);
    }

    #[test]
    fn empty_unquoted_element_is_malformed() {
        // A missing element between/around commas is malformed, but a quoted
        // empty string is a legitimate element.
        assert!(array_in("{a,,c}", PgType::Text).is_err());
        assert!(array_in("{1,}", PgType::Text).is_err());
        assert!(array_in("{,1}", PgType::Text).is_err());
        assert_eq!(
            array_in(r#"{a,"",c}"#, PgType::Text).unwrap(),
            vec![
                Value::Text("a".into()),
                Value::Text(String::new()),
                Value::Text("c".into())
            ]
        );
    }

    #[test]
    fn escaped_trailing_whitespace_is_kept() {
        // A backslash-escaped trailing space is significant and must survive the
        // unquoted trailing-whitespace trim; an unescaped one is dropped.
        assert_eq!(
            array_in("{a\\ }", PgType::Text).unwrap(),
            vec![Value::Text("a ".into())]
        );
        assert_eq!(
            array_in("{a }", PgType::Text).unwrap(),
            vec![Value::Text("a".into())]
        );
    }

    #[test]
    fn non_ascii_whitespace_element_is_not_quoted() {
        // PG's array_out only treats ASCII whitespace as needing quotes; a
        // non-breaking space (U+00A0) is left bare.
        assert_eq!(format(&[Value::Text("a\u{00A0}b".into())], 1), "{a\u{00A0}b}");
    }

    #[test]
    fn oid_mapping_round_trips() {
        assert_eq!(array_oid_for_elem(oid::INT4), Some(oid::INT4_ARRAY));
        assert_eq!(elem_oid_for_array(oid::INT4_ARRAY), Some(oid::INT4));
        assert_eq!(array_oid_for_elem(oid::TEXT), Some(oid::TEXT_ARRAY));
    }
}
