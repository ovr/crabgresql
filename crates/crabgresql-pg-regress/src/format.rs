//! Rendering of query responses the way `psql -q` (aligned mode, border 1,
//! empty NULL, footer on) prints them, so results can be diffed against the
//! upstream `expected/*.out` files byte for byte.
//!
//! The rules are derived from psql's observable output in the vendored
//! expected files.
//!
//! TODO: measure widths in display columns instead of characters — both table
//! column widths and the `LINE n:` excerpt window — so wide characters (CJK,
//! combining marks) line up the way psql aligns them. Acceptable while no
//! passing test needs it.

use crate::client::{ErrorFields, Field};

/// Type OIDs whose text output psql right-aligns: int8, int2, int4, oid,
/// xid, cid, float4, float8, money, numeric, xid8.
const RIGHT_ALIGNED_OIDS: &[u32] = &[20, 21, 23, 26, 28, 29, 700, 701, 790, 1700, 5069];

/// psql's output settings, as far as the corpus exercises them: what `\pset`,
/// `\x`, `\a` and `\t` change about a result table.
#[derive(Clone, Debug, PartialEq)]
pub struct Printing {
    /// `\pset null`. psql starts with the empty string.
    pub null_display: String,
    /// `\x` / `\pset expanded`: one `-[ RECORD n ]` block per row.
    pub expanded: bool,
    /// `\a` / `\pset format`: false is psql's `unaligned`, which pads nothing
    /// and ignores `border` entirely.
    pub aligned: bool,
    /// `\t` / `\pset tuples_only`: drop the header block and the `(N rows)`
    /// footer.
    pub tuples_only: bool,
}

impl Default for Printing {
    fn default() -> Self {
        Self {
            null_display: String::new(),
            expanded: false,
            aligned: true,
            tuples_only: false,
        }
    }
}

/// psql's `unaligned` field separator. `\pset fieldsep` is only ever exercised
/// as an error case in the corpus, so the default is the only value needed.
const FIELD_SEP: &str = "|";

pub fn format_table(options: &Printing, fields: &[Field], rows: &[Vec<Option<String>>]) -> String {
    if options.expanded {
        return expanded_table(options, fields, rows);
    }
    let mut out = if options.aligned {
        aligned_table(None, fields, rows, options)
    } else {
        unaligned_table(fields, rows, options)
    };
    if !options.tuples_only {
        let n = rows.len();
        out.push_str(&format!("({n} row{})\n", if n == 1 { "" } else { "s" }));
    }
    // The aligned printer always closes with a blank line, even with the header
    // and footer suppressed (`partition_prune.out:4517`); the unaligned one
    // never does (`explain.out:262`).
    if options.aligned {
        out.push('\n');
    }
    out
}

/// psql's `unaligned` mode: values joined by the field separator with no
/// padding at all, and no `border` influence whatsoever.
fn unaligned_table(fields: &[Field], rows: &[Vec<Option<String>>], options: &Printing) -> String {
    let mut out = String::new();
    if !options.tuples_only {
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        out.push_str(&names.join(FIELD_SEP));
        out.push('\n');
    }
    for row in rows {
        let cells: Vec<&str> = row
            .iter()
            .map(|v| v.as_deref().unwrap_or(&options.null_display))
            .collect();
        out.push_str(&cells.join(FIELD_SEP));
        out.push('\n');
    }
    out
}

/// psql's expanded (`\x`) mode: one block per row, `name | value`, with the
/// column names down the left. The `(N rows)` footer is suppressed — except for
/// an empty result, which prints `(0 rows)` and nothing else
/// (`stats_import.out:1501`).
fn expanded_table(options: &Printing, fields: &[Field], rows: &[Vec<Option<String>>]) -> String {
    let mut out = String::new();
    if rows.is_empty() {
        if !options.tuples_only {
            out.push_str("(0 rows)\n");
        }
        out.push('\n');
        return out;
    }

    // A name or a value can itself span lines, and both columns are sized by
    // the widest single line.
    let names: Vec<Vec<&str>> = fields
        .iter()
        .map(|f| f.name.split('\n').collect())
        .collect();
    let name_width = names
        .iter()
        .flatten()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    let value_width = rows
        .iter()
        .flatten()
        .flat_map(|value| {
            value
                .as_deref()
                .unwrap_or(&options.null_display)
                .split('\n')
                .map(|line| line.chars().count())
        })
        .max()
        .unwrap_or(0);

    if !options.aligned {
        return expanded_unaligned(options, fields, rows);
    }

    // The rule that runs through psql's expanded output: the two columns plus
    // the continuation flag, the `|` and the space after it.
    let divider = format!(
        "{}+{}",
        "-".repeat(name_width + 1),
        "-".repeat(value_width + 1)
    );
    for (n, row) in rows.iter().enumerate() {
        if options.tuples_only {
            // Without the record labels psql separates records with a bare
            // divider, and prints none before the first (`psql.out:2937`).
            if n > 0 {
                out.push_str(&divider);
                out.push('\n');
            }
        } else {
            // `-[ RECORD n ]` is laid over the divider; a label wider than the
            // divider is simply all there is (`psql.out:39`).
            let label = format!("-[ RECORD {} ]", n + 1);
            out.push_str(&if label.len() >= divider.len() {
                label
            } else {
                format!("{label}{}", &divider[label.len()..])
            });
            out.push('\n');
        }
        let values: Vec<Vec<&str>> = row
            .iter()
            .map(|value| {
                value
                    .as_deref()
                    .unwrap_or(&options.null_display)
                    .split('\n')
                    .collect()
            })
            .collect();
        for (name, value) in names.iter().zip(&values) {
            for line in 0..name.len().max(value.len()) {
                let segment = name.get(line).copied().unwrap_or("");
                let pad = " ".repeat(name_width - segment.chars().count());
                let continues = if line + 1 < name.len() { '+' } else { ' ' };
                out.push_str(&format!("{segment}{pad}{continues}|"));
                // A line with no value segment stops right after the `|`, with
                // no trailing space (`psql.out:829`).
                if let Some(text) = value.get(line) {
                    out.push(' ');
                    if line + 1 < value.len() {
                        let pad = " ".repeat(value_width - text.chars().count());
                        out.push_str(&format!("{text}{pad}+"));
                    } else {
                        out.push_str(text);
                    }
                }
                out.push('\n');
            }
        }
    }
    out.push('\n');
    out
}

/// Expanded output with `\a`: `name|value` lines, records separated by a blank
/// line (`psql.out:2974`).
fn expanded_unaligned(
    options: &Printing,
    fields: &[Field],
    rows: &[Vec<Option<String>>],
) -> String {
    let mut out = String::new();
    for (n, row) in rows.iter().enumerate() {
        if n > 0 {
            out.push('\n');
        }
        for (field, value) in fields.iter().zip(row) {
            let value = value.as_deref().unwrap_or(&options.null_display);
            out.push_str(&format!("{}{FIELD_SEP}{value}\n", field.name));
        }
    }
    if !options.tuples_only {
        out.push('\n');
    }
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
    let mut out = aligned_table(Some(title), &fields, rows, &Printing::default());
    out.push('\n');
    out
}

fn aligned_table(
    title: Option<&str>,
    fields: &[Field],
    rows: &[Vec<Option<String>>],
    options: &Printing,
) -> String {
    let null_display = options.null_display.as_str();
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
    // hence the trailing whitespace psql expected files are known for. `\t`
    // drops the whole block, rows keep their alignment
    // (`partition_prune.out:4517`).
    if !options.tuples_only {
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
    }

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
            .map(|value| {
                value
                    .as_deref()
                    .unwrap_or(null_display)
                    .split('\n')
                    .collect()
            })
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

/// Deterministic stand-in for the psql metacommands the runner does not
/// implement. `name` is the command name the lexer sliced off, without the
/// leading backslash.
pub fn metacommand_stub(name: &str) -> String {
    format!("\\{name}: metacommand not supported by crabgresql regress runner\n")
}

fn push_field(out: &mut String, label: &str, value: Option<&str>) {
    if let Some(value) = value {
        out.push_str(&format!("{label}:  {value}\n"));
    }
}

/// `LINE n: <query line>` and a caret under the cursor, where `position` is a
/// 1-based character offset over the whole query text. Reproduces the excerpt
/// libpq prints for an error position, including its truncation of an
/// over-long line to a 60-column window with leading/trailing `...`
/// (`float4.out:493`). Screen columns are taken as character counts — the
/// wide-character display-width gap noted in the module docs; the regression
/// corpus is single-byte here.
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

    /// `\x`: one `-[ RECORD n ]` block per row, no `(N rows)` footer.
    /// Reproduces `timestamptz.out:3295`, whose separator is
    /// `name_width + value_width + 3` wide.
    #[test]
    fn expanded_lays_the_record_label_over_the_divider() {
        let out = format_table(
            &Printing {
                expanded: true,
                ..Printing::default()
            },
            &[field("ttz_at_local", 25), field("t_func", 25)],
            &[vec![
                text("Fri Jul 07 23:38:00 1978"),
                text("Fri Jul 07 19:38:00 1978 UTC"),
            ]],
        );
        assert_eq!(
            out,
            "-[ RECORD 1 ]+-----------------------------\n\
             ttz_at_local | Fri Jul 07 23:38:00 1978\n\
             t_func       | Fri Jul 07 19:38:00 1978 UTC\n\n"
        );
    }

    /// A label wider than the divider is all that prints (`psql.out:39`), and a
    /// NULL or empty value still leaves the space after the `|`
    /// (`enum.out:50`).
    #[test]
    fn expanded_short_divider_and_empty_values() {
        let out = format_table(
            &Printing {
                expanded: true,
                ..Printing::default()
            },
            &[field("one", 23), field("two", 23)],
            &[vec![text("1"), None]],
        );
        assert_eq!(out, "-[ RECORD 1 ]\none | 1\ntwo | \n\n");
    }

    /// An empty result prints the footer and nothing else
    /// (`stats_import.out:1501`).
    #[test]
    fn expanded_empty_result_prints_only_the_footer() {
        let out = format_table(
            &Printing {
                expanded: true,
                ..Printing::default()
            },
            &[field("a", 23)],
            &[],
        );
        assert_eq!(out, "(0 rows)\n\n");
    }

    /// `\a`: no padding, header and footer still print, and — unlike aligned —
    /// no trailing blank line (`explain.out:262`).
    #[test]
    fn unaligned_pads_nothing_and_ends_without_a_blank_line() {
        let out = format_table(
            &Printing {
                aligned: false,
                ..Printing::default()
            },
            &[field("backend_type", 25), field("object", 25)],
            &[vec![text("walwriter"), text("wal")]],
        );
        assert_eq!(out, "backend_type|object\nwalwriter|wal\n(1 row)\n");
    }

    /// `\t` drops the header block and the footer but keeps both the row
    /// alignment and the trailing blank line (`partition_prune.out:4517`).
    #[test]
    fn tuples_only_keeps_alignment_and_the_trailing_blank_line() {
        let out = format_table(
            &Printing {
                tuples_only: true,
                ..Printing::default()
            },
            &[field("tableoid", 25), field("a", 23)],
            &[vec![text("hp_prefix_test_p5"), text("1")]],
        );
        assert_eq!(out, " hp_prefix_test_p5 | 1\n\n");
    }

    /// `\a\t` over an empty result prints nothing at all
    /// (`opr_sanity.out:900`).
    #[test]
    fn unaligned_tuples_only_empty_result_prints_nothing() {
        let out = format_table(
            &Printing {
                aligned: false,
                tuples_only: true,
                ..Printing::default()
            },
            &[field("oid", 25)],
            &[],
        );
        assert_eq!(out, "");
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
        let out = format_table(
            &Printing::default(),
            &[field("one", 23)],
            &[vec![text("1")]],
        );
        assert_eq!(out, " one \n-----\n   1\n(1 row)\n\n");
    }

    #[test]
    fn bool_column_left_aligns() {
        let out = format_table(
            &Printing::default(),
            &[field("true", 16)],
            &[vec![text("t")]],
        );
        assert_eq!(out, " true \n------\n t\n(1 row)\n\n");
    }

    #[test]
    fn default_column_name_width() {
        let out = format_table(
            &Printing::default(),
            &[field("?column?", 23)],
            &[vec![text("1")]],
        );
        assert_eq!(out, " ?column? \n----------\n        1\n(1 row)\n\n");
    }

    #[test]
    fn header_centering_puts_extra_space_right() {
        let out = format_table(
            &Printing::default(),
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
        let out = format_table(
            &Printing::default(),
            &[field("a", 23), field("b", 25)],
            &[vec![text("1"), None]],
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
            &Printing::default(),
            &[field("pg_get_viewdef", 25), field("n", 23)],
            &[vec![
                text(" SELECT a AS aa,\n    a AS bb\n   FROM t;"),
                text("42"),
            ]],
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
            &Printing::default(),
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
        let out = format_table(&Printing::default(), &[field("a", 23)], &[]);
        assert_eq!(out, " a \n---\n(0 rows)\n\n");
    }

    /// `CREATE TABLE t(); INSERT INTO t DEFAULT VALUES; SELECT * FROM t;`
    /// yields a row with no columns, which must not underflow the width index.
    /// TODO: match psql's zero-column output — a `--` rule and the footer, with
    /// no blank header or row line (`alter_table.out:1624`). The blank lines
    /// rendered here are a diff rather than a panic that takes the whole run
    /// down.
    #[test]
    fn zero_column_result_with_rows() {
        let out = format_table(&Printing::default(), &[], &[vec![]]);
        assert_eq!(out, "\n\n\n(1 row)\n\n");
    }

    #[test]
    fn configured_null_marker_affects_width_but_not_empty_strings() {
        let out = format_table(
            &Printing {
                null_display: "(null)".into(),
                ..Printing::default()
            },
            &[field("a", 25)],
            &[vec![None], vec![text("")]],
        );
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
            metacommand_stub("d"),
            "\\d: metacommand not supported by crabgresql regress runner\n"
        );
    }
}
