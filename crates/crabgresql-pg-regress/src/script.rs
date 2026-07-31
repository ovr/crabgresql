//! psql-style script lexing: echo every input line (as `psql -a` does) and
//! split statements on top-level semicolons.
//!
//! Statement boundaries respect single-quoted strings (with `''` doubling and
//! `E'...'` backslash escapes), double-quoted identifiers, dollar quoting and
//! `--` / nested `/* */` comments. Like psql, leading whitespace and comments
//! are not part of a statement while the buffer is still empty.

/// One event of a regression script, in output order: every physical line is
/// echoed, and each statement fires right after the line that completes it.
#[derive(Debug, PartialEq)]
pub enum ScriptItem {
    /// A physical input line, echoed verbatim (without trailing newline).
    Line(String),
    /// A complete SQL statement, including its terminating `;`.
    Statement(String),
    /// A backslash metacommand: the line's text after `\`, e.g. `d tab`. This
    /// is the whole rest of the line, so a chained command (`\set x y \\ …`)
    /// arrives as one item for the runner to split. Any pending statement
    /// buffer is discarded — psql's `\g`-family would execute it, but the
    /// runner implements none of those.
    Metacommand(String),
    /// The inline data body of a preceding `COPY … FROM STDIN` statement: every
    /// physical line up to (but not including) the terminating `\.`, joined with
    /// newlines. The data lines are still echoed individually as `Line`s, as
    /// psql does under `-a`; this carries the payload to feed over the wire.
    CopyData(String),
}

/// Whether a completed statement is `COPY … FROM STDIN` (the only COPY form the
/// runner streams inline data for). Whitespace is collapsed so a multi-line
/// statement and `FROM  STDIN` both match. `FROM STDIN` must be a trailing token
/// sequence (optionally followed by `WITH`-options), not merely a substring, so
/// `COPY (… FROM stdin) TO …` and a `STDIN`-prefixed identifier are not
/// misclassified.
pub fn is_copy_from_stdin(sql: &str) -> bool {
    let upper = sql.trim().trim_end_matches(';').to_ascii_uppercase();
    let normalized = upper.split_whitespace().collect::<Vec<_>>().join(" ");
    if !normalized.starts_with("COPY ") {
        return false;
    }
    const MARK: &str = " FROM STDIN";
    match normalized.find(MARK) {
        Some(idx) => {
            let after = &normalized[idx + MARK.len()..];
            after.is_empty() || after.starts_with(' ')
        }
        None => false,
    }
}

/// Quoting/comment state that survives across lines. The dollar-quote tag is
/// tracked separately so the state stays `Copy`.
#[derive(Clone, Copy, PartialEq)]
enum State {
    Normal,
    SingleQuote { escapes: bool },
    DoubleQuote,
    DollarQuote,
    BlockComment(u32),
}

pub fn lex(input: &str) -> Vec<ScriptItem> {
    let mut items = Vec::new();
    let mut stmt = String::new();
    // False until the statement buffer holds a real token: whitespace and
    // comments before that stay out of the statement, as in psql.
    let mut has_content = false;
    let mut state = State::Normal;
    let mut dollar_tag = String::new();
    // `Some` while collecting the inline data of a `COPY … FROM STDIN`: each
    // line is echoed and accumulated until a lone `\.` closes the payload.
    let mut copy_data: Option<String> = None;

    for line in input.lines() {
        // Inside a COPY FROM STDIN payload, every line is data, not SQL, until
        // the `\.` terminator. psql does not echo copy-in data under -a, so these
        // lines produce no `Line` item — only the accumulated `CopyData`.
        if let Some(data) = copy_data.as_mut() {
            if line == "\\." {
                items.push(ScriptItem::CopyData(std::mem::take(data)));
                copy_data = None;
            } else {
                data.push_str(line);
                data.push('\n');
            }
            continue;
        }
        // psql drops empty lines entirely — no echo, nothing added to the
        // statement buffer — unless it is inside a quoted string.
        let in_quote = matches!(
            state,
            State::SingleQuote { .. } | State::DoubleQuote | State::DollarQuote
        );
        if line.is_empty() && !in_quote {
            continue;
        }
        items.push(ScriptItem::Line(line.to_string()));
        let chars: Vec<char> = line.chars().collect();
        let mut in_line_comment = false;
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if in_line_comment {
                if has_content {
                    stmt.push(c);
                }
                i += 1;
                continue;
            }
            match state {
                State::Normal => {
                    if c == ';' {
                        stmt.push(';');
                        let statement = std::mem::take(&mut stmt);
                        let is_copy = is_copy_from_stdin(&statement);
                        items.push(ScriptItem::Statement(statement));
                        has_content = false;
                        // A COPY … FROM STDIN switches subsequent lines to data
                        // collection; anything after the `;` on this line is not
                        // part of the payload (psql reads data from the next line).
                        if is_copy {
                            copy_data = Some(String::new());
                            break;
                        }
                    } else if c == '-' && chars.get(i + 1) == Some(&'-') {
                        in_line_comment = true;
                        if has_content {
                            stmt.push_str("--");
                        }
                        i += 2;
                        continue;
                    } else if c == '/' && chars.get(i + 1) == Some(&'*') {
                        state = State::BlockComment(1);
                        if has_content {
                            stmt.push_str("/*");
                        }
                        i += 2;
                        continue;
                    } else if c == '\\' {
                        let rest: String = chars[i + 1..].iter().collect();
                        items.push(ScriptItem::Metacommand(rest.trim_end().to_string()));
                        stmt.clear();
                        has_content = false;
                        break;
                    } else if c.is_whitespace() && !has_content {
                        // leading whitespace stays out of the statement
                    } else if c == '\'' {
                        // E'...' allows backslash escapes; a bare quote after
                        // an identifier ending in e does not.
                        let escapes = i >= 1
                            && matches!(chars[i - 1], 'e' | 'E')
                            && (i < 2 || !is_ident_char(chars[i - 2]));
                        state = State::SingleQuote { escapes };
                        has_content = true;
                        stmt.push(c);
                    } else if c == '"' {
                        state = State::DoubleQuote;
                        has_content = true;
                        stmt.push(c);
                    } else if c == '$'
                        && let Some(len) = dollar_tag_at(&chars, i)
                    {
                        dollar_tag = chars[i..i + len].iter().collect();
                        stmt.push_str(&dollar_tag);
                        state = State::DollarQuote;
                        has_content = true;
                        i += len;
                        continue;
                    } else {
                        has_content = true;
                        stmt.push(c);
                    }
                }
                State::SingleQuote { escapes } => {
                    stmt.push(c);
                    if escapes && c == '\\' {
                        if let Some(&next) = chars.get(i + 1) {
                            stmt.push(next);
                            i += 2;
                            continue;
                        }
                    } else if c == '\'' {
                        if chars.get(i + 1) == Some(&'\'') {
                            stmt.push('\'');
                            i += 2;
                            continue;
                        }
                        state = State::Normal;
                    }
                }
                State::DoubleQuote => {
                    stmt.push(c);
                    if c == '"' {
                        if chars.get(i + 1) == Some(&'"') {
                            stmt.push('"');
                            i += 2;
                            continue;
                        }
                        state = State::Normal;
                    }
                }
                State::DollarQuote => {
                    if c == '$' && starts_with_at(&chars, i, &dollar_tag) {
                        stmt.push_str(&dollar_tag);
                        i += dollar_tag.chars().count();
                        state = State::Normal;
                        continue;
                    }
                    stmt.push(c);
                }
                State::BlockComment(depth) => {
                    if c == '*' && chars.get(i + 1) == Some(&'/') {
                        if has_content {
                            stmt.push_str("*/");
                        }
                        state = match depth {
                            1 => State::Normal,
                            d => State::BlockComment(d - 1),
                        };
                        i += 2;
                        continue;
                    }
                    if c == '/' && chars.get(i + 1) == Some(&'*') {
                        if has_content {
                            stmt.push_str("/*");
                        }
                        state = State::BlockComment(depth + 1);
                        i += 2;
                        continue;
                    }
                    if has_content {
                        stmt.push(c);
                    }
                }
            }
            i += 1;
        }
        if has_content {
            stmt.push('\n');
        }
    }

    // A COPY payload with no closing `\.` at EOF still delivers what it collected.
    if let Some(data) = copy_data.take() {
        items.push(ScriptItem::CopyData(data));
    } else if !stmt.trim().is_empty() {
        // psql executes whatever is left in the buffer at EOF, `;` or not.
        items.push(ScriptItem::Statement(stmt.trim_end().to_string()));
    }
    items
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Length of a `$tag$` opener at `chars[i]` (which is `$`), if there is one.
/// `$1` and friends are positional parameters, not quote openers.
fn dollar_tag_at(chars: &[char], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < chars.len() && is_ident_char(chars[j]) {
        j += 1;
    }
    let is_tag =
        j < chars.len() && chars[j] == '$' && chars.get(i + 1).is_none_or(|c| !c.is_ascii_digit());
    is_tag.then(|| j - i + 1)
}

fn starts_with_at(chars: &[char], i: usize, tag: &str) -> bool {
    tag.chars()
        .enumerate()
        .all(|(k, t)| chars.get(i + k) == Some(&t))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statements(input: &str) -> Vec<String> {
        lex(input)
            .into_iter()
            .filter_map(|item| match item {
                ScriptItem::Statement(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn splits_on_top_level_semicolons() {
        assert_eq!(
            statements("SELECT 1;\nSELECT 2;\n"),
            ["SELECT 1;", "SELECT 2;"]
        );
    }

    #[test]
    fn two_statements_on_one_line_echo_once() {
        let items = lex("SELECT 1; SELECT 2;\n");
        assert_eq!(
            items,
            [
                ScriptItem::Line("SELECT 1; SELECT 2;".into()),
                ScriptItem::Statement("SELECT 1;".into()),
                ScriptItem::Statement("SELECT 2;".into()),
            ]
        );
    }

    #[test]
    fn statement_spans_lines_and_fires_after_final_line() {
        let items = lex("SELECT\n1;\n");
        assert_eq!(
            items,
            [
                ScriptItem::Line("SELECT".into()),
                ScriptItem::Line("1;".into()),
                ScriptItem::Statement("SELECT\n1;".into()),
            ]
        );
    }

    #[test]
    fn semicolons_in_quotes_do_not_split() {
        assert_eq!(statements("SELECT 'a;b';"), ["SELECT 'a;b';"]);
        assert_eq!(
            statements("SELECT \"a;b\" FROM t;"),
            ["SELECT \"a;b\" FROM t;"]
        );
        assert_eq!(statements("SELECT $$a;b$$;"), ["SELECT $$a;b$$;"]);
        assert_eq!(statements("SELECT $q$a;$x$b$q$;"), ["SELECT $q$a;$x$b$q$;"]);
    }

    #[test]
    fn quote_doubling_stays_inside_string() {
        assert_eq!(statements("SELECT 'it''s;fine';"), ["SELECT 'it''s;fine';"]);
    }

    #[test]
    fn escape_string_backslash_quote() {
        assert_eq!(statements(r"SELECT E'a\';b';"), [r"SELECT E'a\';b';"]);
        // Standard strings do not treat backslash as an escape.
        assert_eq!(statements(r"SELECT 'a\';"), [r"SELECT 'a\';"]);
    }

    #[test]
    fn comments_hide_semicolons() {
        assert_eq!(statements("SELECT 1 -- one;\n;"), ["SELECT 1 -- one;\n;"]);
        assert_eq!(
            statements("SELECT /* ; /* nested ; */ still */ 1;"),
            ["SELECT /* ; /* nested ; */ still */ 1;"]
        );
    }

    #[test]
    fn leading_comments_and_blank_lines_stay_out_of_statements() {
        assert_eq!(statements("-- header\n\n  SELECT 1;\n"), ["SELECT 1;"]);
        assert_eq!(statements("/* multi\nline */\nSELECT 1;\n"), ["SELECT 1;"]);
    }

    #[test]
    fn metacommand_takes_rest_of_line() {
        let items = lex("\\d crabs\nSELECT 1;\n");
        assert_eq!(
            items,
            [
                ScriptItem::Line("\\d crabs".into()),
                ScriptItem::Metacommand("d crabs".into()),
                ScriptItem::Line("SELECT 1;".into()),
                ScriptItem::Statement("SELECT 1;".into()),
            ]
        );
    }

    #[test]
    fn metacommand_discards_pending_buffer() {
        assert_eq!(statements("SELECT 1 \\gset\nSELECT 2;\n"), ["SELECT 2;"]);
    }

    #[test]
    fn positional_parameter_is_not_a_dollar_quote() {
        assert_eq!(statements("SELECT $1;"), ["SELECT $1;"]);
    }

    #[test]
    fn pending_buffer_runs_at_eof() {
        assert_eq!(statements("SELECT 1"), ["SELECT 1"]);
    }

    #[test]
    fn multiline_string_keeps_newline() {
        assert_eq!(statements("SELECT 'a\nb';"), ["SELECT 'a\nb';"]);
    }

    #[test]
    fn empty_lines_are_dropped_outside_quotes() {
        // Like psql: not echoed and not part of the statement, even when the
        // buffer is non-empty.
        let items = lex("\nSELECT\n\n1;\n\n");
        assert_eq!(
            items,
            [
                ScriptItem::Line("SELECT".into()),
                ScriptItem::Line("1;".into()),
                ScriptItem::Statement("SELECT\n1;".into()),
            ]
        );
    }

    #[test]
    fn empty_line_inside_string_is_echoed_and_kept() {
        let items = lex("SELECT 'a\n\nb';\n");
        assert_eq!(
            items,
            [
                ScriptItem::Line("SELECT 'a".into()),
                ScriptItem::Line("".into()),
                ScriptItem::Line("b';".into()),
                ScriptItem::Statement("SELECT 'a\n\nb';".into()),
            ]
        );
    }

    #[test]
    fn copy_from_stdin_collects_data_without_echoing_it() {
        // The COPY statement echoes; the data lines and `\.` do not (psql -a
        // does not echo copy-in data). The payload arrives as one CopyData.
        let items = lex("COPY t FROM stdin;\n1\ta\n2\tb\n\\.\nSELECT 1;\n");
        assert_eq!(
            items,
            [
                ScriptItem::Line("COPY t FROM stdin;".into()),
                ScriptItem::Statement("COPY t FROM stdin;".into()),
                ScriptItem::CopyData("1\ta\n2\tb\n".into()),
                ScriptItem::Line("SELECT 1;".into()),
                ScriptItem::Statement("SELECT 1;".into()),
            ]
        );
    }

    #[test]
    fn copy_data_is_never_parsed_as_sql() {
        // A `;` inside copy data does not split a statement, and blank data lines
        // are preserved verbatim in the payload.
        let items = lex("COPY t FROM stdin;\na;b\n\nc\n\\.\n");
        assert_eq!(
            items,
            [
                ScriptItem::Line("COPY t FROM stdin;".into()),
                ScriptItem::Statement("COPY t FROM stdin;".into()),
                ScriptItem::CopyData("a;b\n\nc\n".into()),
            ]
        );
    }

    #[test]
    fn is_copy_from_stdin_recognizes_forms() {
        assert!(is_copy_from_stdin("COPY t FROM stdin;"));
        assert!(is_copy_from_stdin("COPY t (a, b) FROM STDIN"));
        assert!(is_copy_from_stdin(
            "COPY t FROM stdin WITH (FORMAT csv)"
        ));
        assert!(!is_copy_from_stdin("COPY t TO stdout;"));
        assert!(!is_copy_from_stdin("COPY t FROM '/tmp/f';"));
        assert!(!is_copy_from_stdin("SELECT 1;"));
        // "FROM STDIN" as a substring inside a COPY TO (query) must not match.
        assert!(!is_copy_from_stdin("COPY (SELECT a FROM stdin) TO stdout;"));
        // A STDIN-prefixed identifier is not the STDIN keyword.
        assert!(!is_copy_from_stdin("COPY t FROM stdin_table;"));
    }
}
