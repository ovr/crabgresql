//! Rendering of query responses the way `psql -q` (aligned mode, border 1,
//! empty NULL, footer on) prints them, so results can be diffed against the
//! upstream `expected/*.out` files byte for byte.
//!
//! The rules are derived from psql's observable output in the vendored
//! expected files. Known gaps, acceptable while no passing test needs them:
//! multi-line cell values (psql's `+` continuation markers), wide-character
//! display widths, and psql's truncation of long `LINE n:` excerpts.

use crate::client::{ErrorFields, Field};

/// Type OIDs whose text output psql right-aligns: int8, int2, int4, oid,
/// xid, cid, float4, float8, money, numeric, xid8.
const RIGHT_ALIGNED_OIDS: &[u32] = &[20, 21, 23, 26, 28, 29, 700, 701, 790, 1700, 5069];

pub fn format_table(fields: &[Field], rows: &[Vec<Option<String>>]) -> String {
    let widths: Vec<usize> = fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            rows.iter()
                .map(|row| row[i].as_deref().map_or(0, |v| v.chars().count()))
                .max()
                .unwrap_or(0)
                .max(field.name.chars().count())
        })
        .collect();

    let mut out = String::new();

    // Header: names centered (extra space to the right), every cell padded —
    // hence the trailing whitespace psql expected files are known for.
    let header: Vec<String> = fields
        .iter()
        .zip(&widths)
        .map(|(f, &w)| format!(" {} ", center(&f.name, w)))
        .collect();
    out.push_str(&header.join("|"));
    out.push('\n');

    let separator: Vec<String> = widths.iter().map(|w| "-".repeat(w + 2)).collect();
    out.push_str(&separator.join("+"));
    out.push('\n');

    // Data: numeric-ish columns are right-aligned, and the last column drops
    // its trailing padding (so a NULL last cell leaves a lone space).
    for row in rows {
        let last = fields.len() - 1;
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, value)| {
                let value = value.as_deref().unwrap_or("");
                let pad = " ".repeat(widths[i] - value.chars().count());
                match (RIGHT_ALIGNED_OIDS.contains(&fields[i].type_oid), i == last) {
                    (true, false) => format!(" {pad}{value} "),
                    (true, true) => format!(" {pad}{value}"),
                    (false, false) => format!(" {value}{pad} "),
                    (false, true) => format!(" {value}"),
                }
            })
            .collect();
        out.push_str(&cells.join("|"));
        out.push('\n');
    }

    let n = rows.len();
    out.push_str(&format!("({n} row{})\n\n", if n == 1 { "" } else { "s" }));
    out
}

fn center(s: &str, width: usize) -> String {
    let padding = width - s.chars().count();
    let left = padding / 2;
    format!("{}{s}{}", " ".repeat(left), " ".repeat(padding - left))
}

/// `ERROR:  message` plus the optional `LINE n:` excerpt with a caret, and
/// DETAIL/HINT/CONTEXT lines — the shape psql prints ErrorResponses in.
pub fn format_error(error: &ErrorFields, query: &str) -> String {
    let mut out = format!("{}:  {}\n", error.severity(), error.message());
    if let Some(position) = error.get(b'P').and_then(|p| p.parse::<usize>().ok())
        && position > 0
    {
        out.push_str(&position_excerpt(query, position));
    }
    push_field(&mut out, "DETAIL", error.get(b'D'));
    push_field(&mut out, "HINT", error.get(b'H'));
    push_field(&mut out, "CONTEXT", error.get(b'W'));
    out
}

pub fn format_notice(notice: &ErrorFields) -> String {
    let mut out = format!("{}:  {}\n", notice.severity(), notice.message());
    push_field(&mut out, "DETAIL", notice.get(b'D'));
    push_field(&mut out, "HINT", notice.get(b'H'));
    out
}

/// Deterministic stand-in for psql metacommands, which the runner does not
/// implement. `command` is the line's text after the backslash.
pub fn metacommand_stub(command: &str) -> String {
    let name = command.split_whitespace().next().unwrap_or("");
    format!("\\{name}: metacommand not supported by crabgresql regress runner\n")
}

fn push_field(out: &mut String, label: &str, value: Option<&str>) {
    if let Some(value) = value {
        out.push_str(&format!("{label}:  {value}\n"));
    }
}

/// `LINE n: <the query line>` and a caret under the 1-based character
/// `position` (counted over the whole query text).
fn position_excerpt(query: &str, position: usize) -> String {
    let mut remaining = position - 1;
    for (line_no, line) in query.lines().enumerate() {
        let len = line.chars().count();
        if remaining <= len {
            let prefix = format!("LINE {}: ", line_no + 1);
            let caret_at = prefix.chars().count() + remaining;
            return format!("{prefix}{line}\n{}^\n", " ".repeat(caret_at));
        }
        remaining -= len + 1;
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::parse_error_fields;

    fn field(name: &str, type_oid: u32) -> Field {
        Field {
            name: name.into(),
            type_oid,
        }
    }

    fn text(s: &str) -> Option<String> {
        Some(s.into())
    }

    fn error_fields(pairs: &[(u8, &str)]) -> ErrorFields {
        // Build through the wire representation the client parses.
        let mut body = Vec::new();
        for (code, value) in pairs {
            body.push(*code);
            body.extend_from_slice(value.as_bytes());
            body.push(0);
        }
        body.push(0);
        parse_error_fields(&body)
    }

    // Reference outputs below are taken from PostgreSQL's expected files
    // (e.g. vendor/postgres/regress/expected/boolean.out).

    #[test]
    fn int_column_right_aligns_and_header_pads() {
        let out = format_table(&[field("one", 23)], &[vec![text("1")]]);
        assert_eq!(out, " one \n-----\n   1\n(1 row)\n\n");
    }

    #[test]
    fn bool_column_left_aligns() {
        let out = format_table(&[field("true", 16)], &[vec![text("t")]]);
        assert_eq!(out, " true \n------\n t\n(1 row)\n\n");
    }

    #[test]
    fn default_column_name_width() {
        let out = format_table(&[field("?column?", 23)], &[vec![text("1")]]);
        assert_eq!(out, " ?column? \n----------\n        1\n(1 row)\n\n");
    }

    #[test]
    fn header_centering_puts_extra_space_right() {
        let out = format_table(
            &[field("d", 25), field("istrue", 16)],
            &[
                vec![text("true "), text("t")],
                vec![text("false"), text("f")],
            ],
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "   d   | istrue ");
        assert_eq!(lines[1], "-------+--------");
        assert_eq!(lines[2], " true  | t");
        assert_eq!(lines[3], " false | f");
        assert_eq!(lines[4], "(2 rows)");
    }

    #[test]
    fn null_renders_empty_and_last_column_keeps_lone_space() {
        let out = format_table(&[field("a", 23), field("b", 25)], &[vec![text("1"), None]]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[2], " 1 | ");
    }

    #[test]
    fn right_aligned_middle_column_keeps_trailing_space() {
        let out = format_table(
            &[field("id", 23), field("name", 25)],
            &[vec![text("1"), text("ferris")]],
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], " id |  name  ");
        assert_eq!(lines[1], "----+--------");
        assert_eq!(lines[2], "  1 | ferris");
    }

    #[test]
    fn zero_rows_footer() {
        let out = format_table(&[field("a", 23)], &[]);
        assert_eq!(out, " a \n---\n(0 rows)\n\n");
    }

    #[test]
    fn error_with_position_prints_line_and_caret() {
        let fields = error_fields(&[
            (b'V', "ERROR"),
            (b'M', "invalid input syntax for type boolean: \"test\""),
            (b'P', "13"),
        ]);
        let out = format_error(&fields, "SELECT bool 'test' AS error;");
        assert_eq!(
            out,
            "ERROR:  invalid input syntax for type boolean: \"test\"\n\
             LINE 1: SELECT bool 'test' AS error;\n\
             \u{20}                   ^\n"
        );
    }

    #[test]
    fn error_with_detail_and_hint() {
        let fields = error_fields(&[
            (b'V', "ERROR"),
            (b'M', "boom"),
            (b'D', "the details"),
            (b'H', "try harder"),
        ]);
        assert_eq!(
            format_error(&fields, "SELECT 1;"),
            "ERROR:  boom\nDETAIL:  the details\nHINT:  try harder\n"
        );
    }

    #[test]
    fn metacommand_stub_uses_command_name() {
        assert_eq!(
            metacommand_stub("d crabs"),
            "\\d: metacommand not supported by crabgresql regress runner\n"
        );
    }
}
