//! Scanner for PostgreSQL's catalog *data* files (`vendor/postgres/catalog/*.dat`).
//!
//! The format is a Perl array of `{ key => 'value', ... }` hashes. This reads
//! that data only — it is an original scanner, NOT a port of PostgreSQL's
//! `Catalog.pm`, and it never reads PostgreSQL's C headers.

use std::collections::HashMap;
use std::path::Path;

/// One `.dat` entry: its `key => value` pairs with quotes stripped.
pub type Entry = HashMap<String, String>;

/// Read and parse one `.dat` file, telling cargo to re-run the build when it
/// changes.
pub fn read_dat(dir: &Path, file: &str) -> std::io::Result<Vec<Entry>> {
    let path = dir.join(file);
    println!("cargo:rerun-if-changed={}", path.display());
    let src = std::fs::read_to_string(&path)?;
    Ok(parse_dat(&src))
}

/// Parse a `.dat` file into a list of key→value maps. Comments (`#` to end of
/// line, outside quotes) and the surrounding `[` `]` are ignored. Single-quoted
/// values may contain `\'`/`\\` escapes.
pub fn parse_dat(src: &str) -> Vec<Entry> {
    let bytes = src.as_bytes();
    let mut i = 0;
    let n = bytes.len();
    let mut entries = Vec::new();

    while i < n {
        match bytes[i] {
            b'#' => {
                while i < n && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'{' => {
                let (entry, next) = parse_entry(bytes, i + 1);
                entries.push(entry);
                i = next;
            }
            _ => i += 1,
        }
    }
    entries
}

/// Parse one `{ ... }` body starting at `start` (just past the `{`), returning
/// the entry and the index just past the closing `}`.
fn parse_entry(bytes: &[u8], start: usize) -> (Entry, usize) {
    let mut entry = Entry::new();
    let mut i = start;
    let n = bytes.len();
    loop {
        i = skip_ws_and_commas(bytes, i);
        if i >= n || bytes[i] == b'}' {
            return (entry, i + 1);
        }
        // A key: identifier chars up to whitespace or '='.
        let key_start = i;
        while i < n && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        let key = String::from_utf8_lossy(&bytes[key_start..i]).into_owned();
        i = skip_ws_and_commas(bytes, i);
        if i + 1 < n && bytes[i] == b'=' && bytes[i + 1] == b'>' {
            i += 2;
        }
        i = skip_ws_and_commas(bytes, i);
        // Value: quoted string or bareword (e.g. `_null_`).
        let value = if i < n && bytes[i] == b'\'' {
            let (v, next) = parse_quoted(bytes, i + 1);
            i = next;
            v
        } else {
            let vs = i;
            while i < n && bytes[i] != b',' && bytes[i] != b'}' && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            String::from_utf8_lossy(&bytes[vs..i]).into_owned()
        };
        entry.insert(key, value);
    }
}

fn skip_ws_and_commas(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
        i += 1;
    }
    i
}

/// Parse a single-quoted value starting past the opening quote; returns the
/// unescaped content and the index past the closing quote.
fn parse_quoted(bytes: &[u8], start: usize) -> (String, usize) {
    let mut out = String::new();
    let mut i = start;
    let n = bytes.len();
    while i < n {
        match bytes[i] {
            b'\\' if i + 1 < n => {
                out.push(bytes[i + 1] as char);
                i += 2;
            }
            b'\'' => return (out, i + 1),
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    (out, i)
}

/// A field's value, with `_null_` (the `.dat` spelling of SQL NULL) read as
/// absent.
pub fn get<'a>(e: &'a Entry, key: &str) -> Option<&'a str> {
    e.get(key).map(String::as_str).filter(|v| *v != "_null_")
}

/// An OID-valued field; absent reads as 0, PostgreSQL's "no object".
pub fn oid_field(e: &Entry, key: &str) -> u32 {
    match get(e, key) {
        Some(value) => value
            .parse()
            .unwrap_or_else(|_| panic!("bad oid {key}={value:?}")),
        None => 0,
    }
}

/// A `{a,b,c}` array field, as `proargnames`, `proargmodes` and
/// `proallargtypes` spell an array of names. Absent (or `_null_`) reads as
/// `None`, which is what PostgreSQL stores for "this function has none".
///
/// The elements of these three are catalog identifiers — no commas, no quoting —
/// so splitting on `,` is exact. A quoted element belongs to the `proargdefaults`
/// form (`{"{}",false}`), which is a serialized expression tree rather than a
/// list of names; refusing it here keeps a caller from reading one as a name.
pub fn array_field(e: &Entry, key: &str) -> Option<Vec<String>> {
    let raw = get(e, key)?;
    let inner = raw
        .strip_prefix('{')
        .and_then(|v| v.strip_suffix('}'))
        .unwrap_or_else(|| panic!("{key} is not a braced list: {raw:?}"));
    assert!(
        !inner.contains('"'),
        "{key} carries a quoted element, which is not a list of names: {raw:?}"
    );
    if inner.is_empty() {
        return Some(Vec::new());
    }
    Some(inner.split(',').map(|v| v.trim().to_string()).collect())
}

/// A `t`/`f` field, falling back to the column's BKI default.
pub fn bool_field(e: &Entry, key: &str, default: bool) -> bool {
    match get(e, key) {
        Some("t") => true,
        Some("f") => false,
        None => default,
        Some(other) => panic!("bad bool {key}={other:?}"),
    }
}

/// A string field, falling back to the column's BKI default.
pub fn str_field<'a>(e: &'a Entry, key: &str, default: &'a str) -> &'a str {
    get(e, key).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_entries_and_ignores_comments_and_brackets() {
        let entries = parse_dat(
            "# a leading comment { not an entry }\n\
             [\n\
             { oid => '16', typname => 'bool' }, # trailing comment\n\
             { oid => '17',\n  typname => 'bytea' },\n\
             ]\n",
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(get(&entries[0], "typname"), Some("bool"));
        assert_eq!(oid_field(&entries[1], "oid"), 17);
    }

    #[test]
    fn reads_barewords_and_escapes() {
        let entries = parse_dat("[{ typdelim => '\\'', typanalyze => _null_, oid => 25 }]");
        let e = &entries[0];
        // `_null_` is a bareword the accessors read as absent, so the column
        // falls back to its default rather than to the literal text.
        assert_eq!(get(e, "typanalyze"), None);
        assert_eq!(str_field(e, "typanalyze", "-"), "-");
        // An escaped quote is part of the value, not its terminator.
        assert_eq!(get(e, "typdelim"), Some("'"));
        // An unquoted number is a value like any other.
        assert_eq!(oid_field(e, "oid"), 25);
    }

    #[test]
    fn a_braced_list_reads_as_its_elements() {
        let entries = parse_dat(
            "[{ proargnames => '{acl,grantor}', proargmodes => '{}', \
             proallargtypes => _null_ }]",
        );
        let e = &entries[0];
        assert_eq!(
            array_field(e, "proargnames"),
            Some(vec!["acl".to_string(), "grantor".to_string()])
        );
        // `{}` is an array with no elements; `_null_` is no array at all. Only
        // the second one means the column is NULL.
        assert_eq!(array_field(e, "proargmodes"), Some(Vec::new()));
        assert_eq!(array_field(e, "proallargtypes"), None);
        assert_eq!(array_field(e, "nosuchfield"), None);
    }

    #[test]
    #[should_panic(expected = "not a list of names")]
    fn a_serialized_expression_is_not_read_as_a_list() {
        // `proargdefaults` shares the braced spelling but holds expressions.
        let entries = parse_dat("[{ proargdefaults => '{\"{}\",false}' }]");
        array_field(&entries[0], "proargdefaults");
    }

    #[test]
    fn missing_fields_take_their_defaults() {
        let entries = parse_dat("[{ proname => 'array_in' }]");
        let e = &entries[0];
        assert_eq!(oid_field(e, "oid"), 0);
        assert!(bool_field(e, "proisstrict", true));
        assert_eq!(str_field(e, "provolatile", "i"), "i");
    }
}
