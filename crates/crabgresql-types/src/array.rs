//! One-dimensional array I/O (`array_out` / `array_in`) and the element↔array
//! OID mapping.
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable* array
//! text format — the `{...}` syntax, its quoting/escaping rules, and the
//! case-insensitive unquoted `NULL` element — implemented independently. Only
//! 1-D arrays are handled; a nested `{` is rejected as unsupported.

use crate::{PgType, Value, cast, oid};

/// SQLSTATE + message for a failed array input (`array_in`).
#[derive(Clone, Debug, PartialEq)]
pub struct ArrayError {
    pub sqlstate: &'static str,
    pub message: String,
}

const INVALID_TEXT_REPRESENTATION: &str = "22P02";
const FEATURE_NOT_SUPPORTED: &str = "0A000";

fn malformed(s: &str) -> ArrayError {
    ArrayError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("malformed array literal: \"{s}\""),
    }
}

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

fn needs_quote(s: &str) -> bool {
    s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.chars()
            .any(|c| matches!(c, '{' | '}' | ',' | '"' | '\\') || c.is_whitespace())
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
        return Err(malformed(input));
    }
    let mut elems = Vec::new();
    skip_ws(&mut chars);
    // An empty array literal `{}` yields no elements.
    if chars.peek() == Some(&'}') {
        chars.next();
        skip_ws(&mut chars);
        if chars.next().is_some() {
            return Err(malformed(input));
        }
        return Ok(elems);
    }
    loop {
        let (token, quoted) = read_element(&mut chars, input)?;
        if !quoted && token.eq_ignore_ascii_case("null") {
            elems.push(Value::Null);
        } else {
            let v = cast::cast_value(Value::Text(token), elem, 1).map_err(|e| ArrayError {
                sqlstate: e.sqlstate,
                message: e.message,
            })?;
            elems.push(v);
        }
        skip_ws(&mut chars);
        match chars.next() {
            Some(',') => {
                skip_ws(&mut chars);
            }
            Some('}') => break,
            _ => return Err(malformed(input)),
        }
    }
    skip_ws(&mut chars);
    if chars.next().is_some() {
        return Err(malformed(input));
    }
    Ok(elems)
}

fn skip_ws(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
        chars.next();
    }
}

/// Read one element token, returning its unescaped text and whether it was
/// double-quoted. Leaves the iterator positioned on the following delimiter
/// (`,`/`}`) or trailing whitespace.
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
                    None => return Err(malformed(input)),
                },
                Some('"') => return Ok((s, true)),
                Some(c) => s.push(c),
                None => return Err(malformed(input)),
            }
        }
    }
    // Unquoted: read until a delimiter, honoring backslash escapes and quotes;
    // trailing whitespace is trimmed (an interior quote switches to quoted mode).
    let mut s = String::new();
    let mut quoted = false;
    loop {
        match chars.peek() {
            Some(',') | Some('}') | None => break,
            Some('{') => return Err(malformed(input)),
            Some('\\') => {
                chars.next();
                match chars.next() {
                    Some(c) => s.push(c),
                    None => return Err(malformed(input)),
                }
            }
            Some('"') => {
                chars.next();
                quoted = true;
                loop {
                    match chars.next() {
                        Some('\\') => match chars.next() {
                            Some(c) => s.push(c),
                            None => return Err(malformed(input)),
                        },
                        Some('"') => break,
                        Some(c) => s.push(c),
                        None => return Err(malformed(input)),
                    }
                }
            }
            Some(&c) => {
                chars.next();
                s.push(c);
            }
        }
    }
    if !quoted {
        while s.ends_with(char::is_whitespace) {
            s.pop();
        }
    }
    Ok((s, quoted))
}

/// Feature-not-supported error for an operation this 1-D array slice does not
/// implement yet (multi-dimensional literals, binary I/O, ...).
pub fn unsupported(message: impl Into<String>) -> ArrayError {
    ArrayError {
        sqlstate: FEATURE_NOT_SUPPORTED,
        message: message.into(),
    }
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
    fn oid_mapping_round_trips() {
        assert_eq!(array_oid_for_elem(oid::INT4), Some(oid::INT4_ARRAY));
        assert_eq!(elem_oid_for_array(oid::INT4_ARRAY), Some(oid::INT4));
        assert_eq!(array_oid_for_elem(oid::TEXT), Some(oid::TEXT_ARRAY));
    }
}
