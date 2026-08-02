//! Rendering of query responses the way `psql -q` (aligned mode, border 1,
//! empty NULL, footer on) prints them, so results can be diffed against the
//! upstream `expected/*.out` files byte for byte.
//!
//! The rules are derived from psql's observable output in the vendored
//! expected files. Known gaps, acceptable while no passing test needs them:
//! wide-character display widths, and psql's truncation of long `LINE n:`
//! excerpts.

use crate::client::{ErrorFields, Field};

/// Type OIDs whose text output psql right-aligns: int8, int2, int4, oid,
/// xid, cid, float4, float8, money, numeric, xid8.
const RIGHT_ALIGNED_OIDS: &[u32] = &[20, 21, 23, 26, 28, 29, 700, 701, 790, 1700, 5069];

pub fn format_table(fields: &[Field], rows: &[Vec<Option<String>>], null_display: &str) -> String {
    let mut out = aligned_table(None, fields, rows, null_display);
    let n = rows.len();
    out.push_str(&format!("({n} row{})\n\n", if n == 1 { "" } else { "s" }));
    out
}

/// psql's `\d` output for one relation: the same aligned table, under a centered
/// title and with a blank line where a query result would print `(N rows)`.
/// Every column is text, so none is right-aligned.
pub fn format_describe(title: &str, headers: &[&str], rows: &[Vec<Option<String>>]) -> String {
    let fields: Vec<Field> = headers
        .iter()
        .map(|name| Field {
            name: (*name).to_string(),
            type_oid: 25,
        })
        .collect();
    let mut out = aligned_table(Some(title), &fields, rows, "");
    out.push('\n');
    out
}

fn aligned_table(
    title: Option<&str>,
    fields: &[Field],
    rows: &[Vec<Option<String>>],
    null_display: &str,
) -> String {
    // A cell containing newlines occupies one output line per line of content,
    // so a column is as wide as its widest *line* — not its widest value.
    let widths: Vec<usize> = fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            rows.iter()
                .flat_map(|row| {
                    row[i]
                        .as_deref()
                        .unwrap_or(null_display)
                        .split('\n')
                        .map(|line| line.chars().count())
                })
                .max()
                .unwrap_or(0)
                .max(field.name.chars().count())
        })
        .collect();

    let mut out = String::new();

    // A title is centered over the whole table — every column plus the space on
    // each side of it, plus one character per `|` separator — and, unlike the
    // header, keeps no padding to its right.
    if let Some(title) = title {
        let width: usize =
            widths.iter().map(|w| w + 2).sum::<usize>() + widths.len().saturating_sub(1);
        let indent = width.saturating_sub(title.chars().count()) / 2;
        out.push_str(&" ".repeat(indent));
        out.push_str(title);
        out.push('\n');
    }

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
    //
    // A row is as tall as its tallest cell. Every line of a cell but its last
    // carries a `+` where the trailing separator space would go — psql's marker
    // for "this value continues" — which is also why a continued line keeps its
    // padding even in the last column, where a single-line value would not.
    for row in rows {
        // `saturating_sub`: a zero-column result still yields one row per
        // tuple (`SELECT * FROM t` where `t` has no columns), and `0 - 1`
        // would panic.
        let last = fields.len().saturating_sub(1);
        let cells: Vec<Vec<&str>> = row
            .iter()
            .map(|value| value.as_deref().unwrap_or(null_display).split('\n').collect())
            .collect();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        for line in 0..height {
            let rendered: Vec<String> = cells
                .iter()
                .enumerate()
                .map(|(i, lines)| {
                    // A cell shorter than the row renders as blanks. `filler`
                    // distinguishes those lines from a cell whose own content is
                    // empty — the two are padded differently in the last column.
                    let filler = line >= lines.len();
                    let value = lines.get(line).copied().unwrap_or("");
                    let pad = " ".repeat(widths[i] - value.chars().count());
                    let continues = line + 1 < lines.len();
                    // A continued line is always left-aligned: the `+` marker
                    // takes the place the right-hand padding would occupy.
                    let right = RIGHT_ALIGNED_OIDS.contains(&fields[i].type_oid);
                    let inner = if right && !continues {
                        format!("{pad}{value}")
                    } else {
                        format!("{value}{pad}")
                    };
                    match (continues, i == last) {
                        (true, _) => format!(" {inner}+"),
                        // The last column drops its *padding* — but only the
                        // padding: a `bpchar` keeps the blanks that are part of
                        // its value. Left-aligned, that means dropping the
                        // trailing pad. Right-aligned, the pad leads the value
                        // so it stays — a NULL still renders as a full-width run
                        // of blanks — except on a filler line, which belongs to
                        // no value at all and collapses to the lone leading space.
                        (false, true) if !right => format!(" {value}"),
                        (false, true) if filler => " ".to_string(),
                        (false, true) => format!(" {pad}{value}"),
                        (false, false) => format!(" {inner} "),
                    }
                })
                .collect();
            out.push_str(&rendered.join("|"));
            out.push('\n');
        }
    }

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

pub fn format_notice(notice: &ErrorFields, query: &str) -> String {
    let mut out = format!("{}:  {}\n", notice.severity(), notice.message());
    if let Some(position) = notice.get(b'P').and_then(|p| p.parse::<usize>().ok())
        && position > 0
    {
        out.push_str(&position_excerpt(query, position));
    }
    push_field(&mut out, "DETAIL", notice.get(b'D'));
    push_field(&mut out, "HINT", notice.get(b'H'));
    push_field(&mut out, "CONTEXT", notice.get(b'W'));
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

/// `LINE n: <query line>` and a caret under the cursor, where `position` is a
/// 1-based character offset over the whole query text. Ports libpq's
/// `reportErrorPosition` (fe-protocol3.c), including its truncation of an
/// over-long line to a 60-column window with leading/trailing `...`. Screen
/// columns are taken as character counts — the wide-character display-width gap
/// noted in the module docs; the regression corpus is single-byte here.
fn position_excerpt(query: &str, position: usize) -> String {
    const DISPLAY_SIZE: usize = 60; // screen width limit, in columns
    const MIN_RIGHT_CUT: usize = 10; // keep at least this far from EOL

    if position == 0 {
        return String::new();
    }
    // libpq renders tabs as a single space; do the same so widths line up.
    let chars: Vec<char> = query
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .collect();
    let total = chars.len();
    let loc = position - 1; // 0-based cursor index
    if loc > total {
        return String::new();
    }

    // Locate the line containing `loc`: its 1-based number and the [ibeg, iend)
    // character range (iend at the terminating newline, or end of text).
    let mut loc_line = 1usize;
    let mut ibeg = 0usize;
    let mut iend = total;
    for cno in 0..total {
        let ch = chars[cno];
        if ch == '\r' || ch == '\n' {
            if cno < loc {
                // A \n immediately after \r does not start a new line.
                if ch == '\r' || cno == 0 || chars[cno - 1] != '\r' {
                    loc_line += 1;
                }
                ibeg = cno + 1;
            } else {
                iend = cno;
                break;
            }
        }
    }

    // Truncate the line to a DISPLAY_SIZE window keeping the cursor visible.
    let mut beg_trunc = false;
    let mut end_trunc = false;
    if iend - ibeg > DISPLAY_SIZE {
        if ibeg + DISPLAY_SIZE >= loc + MIN_RIGHT_CUT {
            // Cutting only the right end is enough.
            while iend - ibeg > DISPLAY_SIZE {
                iend -= 1;
            }
            end_trunc = true;
        } else {
            while loc + MIN_RIGHT_CUT < iend {
                iend -= 1;
                end_trunc = true;
            }
            while iend - ibeg > DISPLAY_SIZE {
                ibeg += 1;
                beg_trunc = true;
            }
        }
    }

    let prefix = format!("LINE {loc_line}: ");
    let mut out = prefix.clone();
    if beg_trunc {
        out.push_str("...");
    }
    out.extend(&chars[ibeg..iend]);
    if end_trunc {
        out.push_str("...");
    }
    out.push('\n');

    let caret_col = prefix.chars().count() + if beg_trunc { 3 } else { 0 } + (loc - ibeg);
    out.push_str(&" ".repeat(caret_col));
    out.push_str("^\n");
    out
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

    /// psql's `\d` shape, byte for byte against PostgreSQL 18.4's output for
    /// `CREATE TABLE bit_defaults (b1 bit(4) DEFAULT '1001', …)`: the title
    /// centered over the 70-column table with no padding after it, and a blank
    /// line where a query result prints `(N rows)`.
    #[test]
    fn describe_centers_its_title_and_ends_with_a_blank_line() {
        let row = |name: &str, ty: &str, default: &str| {
            vec![text(name), text(ty), text(""), text(""), text(default)]
        };
        let out = format_describe(
            "Table \"public.bit_defaults\"",
            &["Column", "Type", "Collation", "Nullable", "Default"],
            &[
                row("b1", "bit(4)", "'1001'::\"bit\""),
                row("b3", "bit varying(5)", "'1001'::bit varying"),
            ],
        );
        assert_eq!(
            out,
            "                     Table \"public.bit_defaults\"\n\
             \x20Column |      Type      | Collation | Nullable |       Default       \n\
             --------+----------------+-----------+----------+---------------------\n\
             \x20b1     | bit(4)         |           |          | '1001'::\"bit\"\n\
             \x20b3     | bit varying(5) |           |          | '1001'::bit varying\n\
             \n"
        );
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
        let out = format_table(&[field("one", 23)], &[vec![text("1")]], "");
        assert_eq!(out, " one \n-----\n   1\n(1 row)\n\n");
    }

    #[test]
    fn bool_column_left_aligns() {
        let out = format_table(&[field("true", 16)], &[vec![text("t")]], "");
        assert_eq!(out, " true \n------\n t\n(1 row)\n\n");
    }

    #[test]
    fn default_column_name_width() {
        let out = format_table(&[field("?column?", 23)], &[vec![text("1")]], "");
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
            "",
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
        let out = format_table(
            &[field("a", 23), field("b", 25)],
            &[vec![text("1"), None]],
            "",
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[2], " 1 | ");
    }

    /// A cell containing newlines occupies one output line per line of content,
    /// each but the last marked with `+`. Pinned against psql 18.4 rendering
    /// `SELECT pg_get_viewdef('v1'), 42 AS n;` — note the right-aligned `n`
    /// column's filler lines collapse to a single space, because the last column
    /// drops trailing whitespace.
    #[test]
    fn multi_line_cells_use_psql_continuation_markers() {
        let out = format_table(
            &[field("pg_get_viewdef", 25), field("n", 23)],
            &[vec![
                text(" SELECT a AS aa,\n    a AS bb\n   FROM t;"),
                text("42"),
            ]],
            "",
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "  pg_get_viewdef  | n  ");
        assert_eq!(lines[1], "------------------+----");
        assert_eq!(lines[2], "  SELECT a AS aa,+| 42");
        assert_eq!(lines[3], "     a AS bb     +| ");
        assert_eq!(lines[4], "    FROM t;       | ");
        assert_eq!(lines[5], "(1 row)");
    }

    #[test]
    fn right_aligned_middle_column_keeps_trailing_space() {
        let out = format_table(
            &[field("id", 23), field("name", 25)],
            &[vec![text("1"), text("ferris")]],
            "",
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], " id |  name  ");
        assert_eq!(lines[1], "----+--------");
        assert_eq!(lines[2], "  1 | ferris");
    }

    #[test]
    fn zero_rows_footer() {
        let out = format_table(&[field("a", 23)], &[], "");
        assert_eq!(out, " a \n---\n(0 rows)\n\n");
    }

    /// `CREATE TABLE t(); INSERT INTO t DEFAULT VALUES; SELECT * FROM t;`
    /// yields a row with no columns, which must not underflow the width index.
    /// The rule line is still narrower than psql's `--`, but that is a diff
    /// rather than a panic that takes the whole run down.
    #[test]
    fn zero_column_result_with_rows() {
        let out = format_table(&[], &[vec![]], "");
        assert_eq!(out, "\n\n\n(1 row)\n\n");
    }

    #[test]
    fn configured_null_marker_affects_width_but_not_empty_strings() {
        let out = format_table(&[field("a", 25)], &[vec![None], vec![text("")]], "(null)");
        assert_eq!(out, "   a    \n--------\n (null)\n \n(2 rows)\n\n");
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
    fn notice_with_position_truncates_long_line_like_libpq() {
        // The exact case from float4.sql: the argument-shell NOTICE points at
        // the `xfloat4` argument (char 28), and the 68-column line is cut to a
        // 60-column window with a trailing `...`. Matches
        // vendor/postgres/regress/expected/float4.out.
        let fields = error_fields(&[
            (b'V', "NOTICE"),
            (b'M', "argument type xfloat4 is only a shell"),
            (b'P', "28"),
        ]);
        let query = "create function xfloat4out(xfloat4) returns cstring immutable strict\n  language internal as 'int4out';";
        // Caret sits under `xfloat4`: 8 cols of "LINE 1: " + 27 cols to the `x`.
        let expected = format!(
            "NOTICE:  argument type xfloat4 is only a shell\n\
             LINE 1: create function xfloat4out(xfloat4) returns cstring immutabl...\n\
             {}^\n",
            " ".repeat(35)
        );
        assert_eq!(format_notice(&fields, query), expected);
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

    /// A nested call's CONTEXT frames are one `W` field, newline-joined. psql
    /// labels only the first line, leaving continuation frames unindented.
    #[test]
    fn multi_frame_context_leaves_continuation_lines_unindented() {
        let fields = error_fields(&[
            (b'V', "ERROR"),
            (b'M', "illegal backlink beginning with XX"),
            (
                b'W',
                "PL/pgSQL function tg_backlink_set(character,character) line 30 at RAISE\n\
                 PL/pgSQL function tg_backlink_a() line 17 at assignment",
            ),
        ]);
        assert_eq!(
            format_error(&fields, "SELECT 1;"),
            "ERROR:  illegal backlink beginning with XX\n\
             CONTEXT:  PL/pgSQL function tg_backlink_set(character,character) line 30 at RAISE\n\
             PL/pgSQL function tg_backlink_a() line 17 at assignment\n"
        );
    }

    #[test]
    fn notice_renders_context() {
        let fields = error_fields(&[
            (b'V', "NOTICE"),
            (b'M', "hello"),
            (b'W', "PL/pgSQL function f() line 2 at RAISE"),
        ]);
        assert_eq!(
            format_notice(&fields, "SELECT f();"),
            "NOTICE:  hello\nCONTEXT:  PL/pgSQL function f() line 2 at RAISE\n"
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
