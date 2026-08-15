//! The JSON the component hands back.
//!
//! Small enough to write by hand — every value is a string, a null, or an array
//! of those — and hand-written keeps the wasm build free of a serialization
//! framework it would otherwise carry for four shapes.

use crabgresql_pg_wire::ErrorFields;

/// One statement's result: what a simple query produces per statement.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StatementResult {
    /// Column names, in output order. Empty for a statement that returns no
    /// rows at all (`INSERT`, `CREATE TABLE`, …).
    pub columns: Vec<String>,
    /// Row values in the wire's text encoding; `None` is SQL NULL.
    pub rows: Vec<Vec<Option<String>>>,
    /// The command tag, e.g. `SELECT 1` or `INSERT 0 3`.
    pub command: String,
}

/// A value that is already a complete JSON document.
///
/// A newtype rather than a bare `String` because the difference decides how it
/// is written out: a `String` would be quoted and escaped, which is how a
/// notice ends up as a JSON document *inside* a JSON string that only the
/// caller who knows to `JSON.parse` it twice can read.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Json(pub String);

/// Everything one `exec` produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExecOutput {
    pub results: Vec<StatementResult>,
    /// NOTICE/WARNING conditions raised along the way, in the order the server
    /// sent them, each with the same fields an error has. Kept separate from
    /// the results because a notice belongs to the session, not to any one row
    /// set.
    pub notices: Vec<Json>,
}

impl ExecOutput {
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"results\":[");
        for (i, result) in self.results.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            result.write_json(&mut out);
        }
        out.push_str("],\"notices\":[");
        for (i, notice) in self.notices.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            // Verbatim: it is already an object, not a string that happens to
            // contain one.
            out.push_str(&notice.0);
        }
        out.push_str("]}");
        out
    }
}

impl StatementResult {
    fn write_json(&self, out: &mut String) {
        out.push_str("{\"columns\":[");
        for (i, column) in self.columns.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_string(out, column);
        }
        out.push_str("],\"rows\":[");
        for (i, row) in self.rows.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('[');
            for (j, value) in row.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                match value {
                    Some(value) => write_string(out, value),
                    None => out.push_str("null"),
                }
            }
            out.push(']');
        }
        out.push_str("],\"command\":");
        write_string(out, &self.command);
        out.push('}');
    }
}

/// A server error or notice, rendered as JSON so the embedder can branch on the
/// SQLSTATE rather than on message text.
pub fn error_to_json(fields: &ErrorFields) -> Json {
    let field = |key: u8| {
        fields
            .fields
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, value)| value.as_str())
    };
    let mut out = String::from("{\"sqlstate\":");
    write_string(&mut out, field(b'C').unwrap_or("XX000"));
    out.push_str(",\"message\":");
    write_string(&mut out, field(b'M').unwrap_or(""));
    for (key, name) in [
        (b'D', "detail"),
        (b'H', "hint"),
        (b'P', "position"),
        (b'S', "severity"),
    ] {
        out.push_str(&format!(",\"{name}\":"));
        match field(key) {
            Some(value) => write_string(&mut out, value),
            None => out.push_str("null"),
        }
    }
    out.push('}');
    Json(out)
}

/// Append `value` as a JSON string literal.
///
/// Escapes what RFC 8259 requires — the two structural characters, and every
/// control character below 0x20, the ones with short forms by name and the rest
/// as `\u00XX`. Nothing else is escaped: the output is UTF-8 and JSON says so.
fn write_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_set_renders_columns_rows_and_the_tag() {
        let output = ExecOutput {
            results: vec![StatementResult {
                columns: vec!["a".to_string(), "b".to_string()],
                rows: vec![
                    vec![Some("1".to_string()), None],
                    vec![Some("2".to_string()), Some("x".to_string())],
                ],
                command: "SELECT 2".to_string(),
            }],
            notices: vec![],
        };
        assert_eq!(
            output.to_json(),
            r#"{"results":[{"columns":["a","b"],"rows":[["1",null],["2","x"]],"command":"SELECT 2"}],"notices":[]}"#
        );
    }

    /// The escaping is the only place this file can be wrong in a way that
    /// makes the host's `JSON.parse` fail, so it is pinned against the
    /// characters that would break it.
    #[test]
    fn strings_escape_quotes_backslashes_and_control_characters() {
        let output = ExecOutput {
            results: vec![StatementResult {
                columns: vec!["quote\"".to_string()],
                rows: vec![vec![Some("back\\slash\nline\u{1}".to_string())]],
                command: "SELECT 1".to_string(),
            }],
            notices: vec![],
        };
        assert_eq!(
            output.to_json(),
            r#"{"results":[{"columns":["quote\""],"rows":[["back\\slash\nline\u0001"]],"command":"SELECT 1"}],"notices":[]}"#
        );
    }

    /// A statement that returns no rows still reports its tag — that is how an
    /// embedder reads back "how many rows did the INSERT touch".
    #[test]
    fn a_tag_only_statement_has_no_columns() {
        let output = ExecOutput {
            results: vec![StatementResult {
                columns: vec![],
                rows: vec![],
                command: "INSERT 0 3".to_string(),
            }],
            notices: vec![],
        };
        assert_eq!(
            output.to_json(),
            r#"{"results":[{"columns":[],"rows":[],"command":"INSERT 0 3"}],"notices":[]}"#
        );
    }

    /// A notice is embedded as an object. Rendering it as a string would make
    /// the caller `JSON.parse` twice, and only the caller who knew to.
    #[test]
    fn a_notice_is_an_object_not_a_string() {
        let output = ExecOutput {
            results: vec![],
            notices: vec![error_to_json(&ErrorFields::notice(
                "00000",
                "table \"t\" does not exist, skipping",
            ))],
        };
        assert_eq!(
            output.to_json(),
            r#"{"results":[],"notices":[{"sqlstate":"00000","message":"table \"t\" does not exist, skipping","detail":null,"hint":null,"position":null,"severity":"NOTICE"}]}"#
        );
    }

    #[test]
    fn an_error_reports_its_sqlstate_and_absent_fields_as_null() {
        let fields =
            ErrorFields::error("42P01", "relation \"t\" does not exist").with_hint("check");
        assert_eq!(
            error_to_json(&fields).0,
            r#"{"sqlstate":"42P01","message":"relation \"t\" does not exist","detail":null,"hint":"check","position":null,"severity":"ERROR"}"#
        );
    }
}
