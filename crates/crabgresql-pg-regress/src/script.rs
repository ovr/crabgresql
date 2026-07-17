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
    /// A backslash metacommand: the line's text after `\`, e.g. `d tab`.
    /// Any pending statement buffer is discarded (psql's `\g`-family would
    /// execute it; the runner supports no metacommands at all).
    Metacommand(String),
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

    for line in input.lines() {
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
                        items.push(ScriptItem::Statement(std::mem::take(&mut stmt)));
                        has_content = false;
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

    // psql executes whatever is left in the buffer at EOF, `;` or not.
    if !stmt.trim().is_empty() {
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
}
