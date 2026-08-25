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
    /// Query text scanned since the previous item, to be appended to the
    /// runner's query buffer. It arrives in fragments rather than as one
    /// statement because psql throws away only the text scanned while an `\if`
    /// branch was inactive — the runner drops exactly the fragments it saw
    /// while inactive and keeps the rest, which is what makes
    /// `select \if false \\ (bogus \else \\ 42 \endif \\ forty_two;` select 42
    /// (`psql.out:4600`).
    Sql(String),
    /// Send the query buffer. It can be empty: a `\g`-family command on an
    /// empty buffer re-runs the previous query.
    Statement { end: QueryEnd },
    /// A backslash command that does *not* terminate the query buffer, split
    /// into its name and the raw argument text (which the runner expands with
    /// [`crate::psql_var::split_args`]). The pending statement buffer is left
    /// untouched, and SQL scanning resumes right after the arguments.
    Metacommand { name: String, args: String },
    /// The inline data body of a preceding `COPY … FROM STDIN` statement: every
    /// physical line up to (but not including) the terminating `\.`, joined with
    /// newlines. The data lines are not echoed as `Line`s — psql under `-a`
    /// leaves copy-in data out of its output (`copy2.out:393`) — so this item
    /// is the only carrier of the payload to feed over the wire.
    CopyData(String),
}

/// What ended a query buffer.
#[derive(Debug, PartialEq)]
pub enum QueryEnd {
    /// A top-level `;`, which is part of the `Sql` fragments preceding it.
    Semicolon,
    /// End of file with a non-empty buffer, which psql also executes.
    Eof,
    /// One of psql's query-buffer terminators — `\g`, `\gx`, `\gset`, `\gexec`,
    /// `\gdesc`, `\crosstabview` — with its raw argument text.
    Backslash { name: String, args: String },
}

/// Whether a backslash command sends the query buffer instead of leaving it
/// pending. psql calls these the `\g` family.
fn is_query_terminator(name: &str) -> bool {
    matches!(
        name,
        "g" | "gx" | "gset" | "gexec" | "gdesc" | "crosstabview"
    )
}

/// Split the text after a `\` into the command name and the index just past it.
/// psql ends a command name at the first character that cannot be part of one,
/// so `\pset null` splits on the space while a lone `\\` yields the name `\`.
pub fn command_name_at(chars: &[char], start: usize) -> (String, usize) {
    let mut end = start;
    while end < chars.len()
        && (chars[end].is_ascii_alphanumeric()
            || chars[end] == '_'
            || chars[end] == '?'
            || chars[end] == '!')
    {
        end += 1;
    }
    // A non-alphanumeric command is a single character, e.g. `\\` or `\.`.
    if end == start {
        end = (start + 1).min(chars.len());
    }
    (chars[start..end].iter().collect(), end)
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

/// Tracks psql's `BEGIN ATOMIC ... END` rule: inside such a routine body a
/// semicolon separates the body's statements instead of ending the `CREATE`,
/// so the lexer has to know where the block closes.
///
/// psql decides this on keywords, and so does this: the block opens on `ATOMIC`
/// directly after `BEGIN` in a statement that began with `CREATE FUNCTION` or
/// `CREATE PROCEDURE`, and closes on the matching `END` — with `CASE` counted
/// as well, since a `CASE … END` inside the body would otherwise close it
/// early.
#[derive(Default)]
struct AtomicBlock {
    /// 0 outside a block; how many `END`s are still owed inside one.
    depth: u32,
    /// The previous word scanned, so `BEGIN ATOMIC` can be recognized as a pair.
    previous: String,
}

impl AtomicBlock {
    /// Feed one keyword-shaped word, `stmt` being the statement buffer scanned
    /// so far (used only to check that this is a routine definition).
    fn word(&mut self, word: &str, stmt: &str) {
        let upper = word.to_ascii_uppercase();
        if self.depth == 0 {
            if upper == "ATOMIC" && self.previous == "BEGIN" && defines_routine(stmt) {
                self.depth = 1;
            }
        } else {
            match upper.as_str() {
                "CASE" => self.depth += 1,
                "END" => self.depth -= 1,
                _ => {}
            }
        }
        self.previous = upper;
    }

    /// Whether a `;` here separates body statements rather than ending one.
    fn inside(&self) -> bool {
        self.depth > 0
    }

    fn reset(&mut self) {
        self.depth = 0;
        self.previous.clear();
    }
}

/// Whether the statement scanned so far is a `CREATE FUNCTION`/`PROCEDURE`,
/// which is the only place `BEGIN ATOMIC` opens a routine body.
fn defines_routine(stmt: &str) -> bool {
    let upper = stmt.to_ascii_uppercase();
    let mut words = upper.split_whitespace();
    words.next() == Some("CREATE")
        && words.any(|word| word.starts_with("FUNCTION") || word.starts_with("PROCEDURE"))
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
    // The lexer keeps the whole buffer so it can classify a completed statement
    // (`is_copy_from_stdin`), but hands it out in fragments: `emitted` is how
    // much of `stmt` has already left as a `Sql` item.
    let mut stmt = String::new();
    let mut emitted = 0usize;
    // False until the statement buffer holds a real token: whitespace and
    // comments before that stay out of the statement, as in psql.
    let mut has_content = false;
    let mut state = State::Normal;
    let mut dollar_tag = String::new();
    // psql's `BEGIN ATOMIC` tracking, plus the word being scanned that feeds it.
    let mut atomic = AtomicBlock::default();
    let mut word = String::new();
    // `Some` while collecting the inline data of a `COPY … FROM STDIN`: each
    // line is accumulated until a lone `\.` closes the payload.
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
                    // A word ends at the first character that cannot continue
                    // it, so the keyword rule is applied *before* this
                    // character is handled — that is what lets the `;` after
                    // `END` see a closed block.
                    if !is_ident_char(c) && !word.is_empty() {
                        atomic.word(&word, &stmt);
                        word.clear();
                    }
                    if is_ident_char(c) {
                        word.push(c);
                    }
                    if c == ';' && atomic.inside() {
                        stmt.push(';');
                        has_content = true;
                    } else if c == ';' {
                        stmt.push(';');
                        flush_sql(&mut items, &stmt, &mut emitted);
                        let is_copy = is_copy_from_stdin(&stmt);
                        stmt.clear();
                        emitted = 0;
                        items.push(ScriptItem::Statement {
                            end: QueryEnd::Semicolon,
                        });
                        has_content = false;
                        atomic.reset();
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
                        // `\;` is not a command at all: psql appends the
                        // semicolon to the query buffer *without* sending, so
                        // `SELECT 1\; SELECT 2;` reaches the server as one
                        // Query holding two statements (transactions.out:977).
                        if chars.get(i + 1) == Some(&';') {
                            stmt.push(';');
                            has_content = true;
                            i += 2;
                            continue;
                        }
                        let (name, name_end) = command_name_at(&chars, i + 1);
                        // `\\` is a bare separator that takes no arguments, so
                        // SQL scanning resumes immediately after it — that is
                        // what makes `\if false \\ (bogus \else \\ 42 \endif \\
                        // forty_two;` (psql.out:4600) select 42.
                        let args_end = if name == "\\" {
                            name_end
                        } else {
                            crate::psql_var::arguments_extent(&chars, name_end)
                        };
                        let args: String = chars[name_end..args_end].iter().collect();
                        flush_sql(&mut items, &stmt, &mut emitted);
                        if is_query_terminator(&name) {
                            has_content = false;
                            let is_copy = is_copy_from_stdin(&stmt);
                            stmt.clear();
                            emitted = 0;
                            items.push(ScriptItem::Statement {
                                end: QueryEnd::Backslash { name, args },
                            });
                            atomic.reset();
                            if is_copy {
                                copy_data = Some(String::new());
                                break;
                            }
                        } else {
                            items.push(ScriptItem::Metacommand { name, args });
                        }
                        i = args_end;
                        continue;
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
        // A word cannot span lines, so the last one on this line ends here.
        if !word.is_empty() {
            atomic.word(&word, &stmt);
            word.clear();
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
        let trimmed = stmt.trim_end();
        emitted = emitted.min(trimmed.len());
        flush_sql(&mut items, trimmed, &mut emitted);
        items.push(ScriptItem::Statement { end: QueryEnd::Eof });
    }
    items
}

/// Hand out the part of the query buffer scanned since the last item.
fn flush_sql(items: &mut Vec<ScriptItem>, stmt: &str, emitted: &mut usize) {
    if *emitted < stmt.len() {
        items.push(ScriptItem::Sql(stmt[*emitted..].to_string()));
        *emitted = stmt.len();
    }
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

    /// The query text each `Statement` sends, reassembled from the `Sql`
    /// fragments the way the runner does when every branch is active.
    fn statements(input: &str) -> Vec<String> {
        let mut sent = Vec::new();
        let mut buffer = String::new();
        for item in lex(input) {
            match item {
                ScriptItem::Sql(text) => buffer.push_str(&text),
                ScriptItem::Statement { .. } => sent.push(std::mem::take(&mut buffer)),
                _ => {}
            }
        }
        sent
    }

    fn sql(text: &str) -> ScriptItem {
        ScriptItem::Sql(text.to_string())
    }

    /// A `BEGIN ATOMIC` body's semicolons separate its statements; only the one
    /// after `END` sends the `CREATE`. Same rule psql applies, which is why the
    /// upstream `create_function_sql` script is readable at all.
    #[test]
    fn an_atomic_body_holds_its_semicolons() {
        assert_eq!(
            statements(
                "CREATE FUNCTION f(a int) RETURNS int LANGUAGE SQL\nBEGIN ATOMIC\n  SELECT a;\nEND;\nSELECT 1;\n"
            ),
            vec![
                "CREATE FUNCTION f(a int) RETURNS int LANGUAGE SQL\nBEGIN ATOMIC\n  SELECT a;\nEND;",
                "SELECT 1;",
            ]
        );
        // Several statements, and a CASE whose END must not close the block.
        assert_eq!(
            statements(
                "CREATE FUNCTION f(a int) RETURNS int LANGUAGE SQL BEGIN ATOMIC \
                 SELECT CASE WHEN a > 0 THEN a ELSE 0 END; SELECT a; END; SELECT 2;"
            )
            .len(),
            2
        );
    }

    /// `BEGIN` outside a routine definition is still a transaction command, and
    /// the word `atomic` elsewhere is just an identifier.
    #[test]
    fn begin_outside_a_routine_definition_is_unaffected() {
        assert_eq!(
            statements("BEGIN; SELECT 1; COMMIT;"),
            vec!["BEGIN;", "SELECT 1;", "COMMIT;"]
        );
        assert_eq!(
            statements("SELECT atomic FROM t; SELECT 1;"),
            vec!["SELECT atomic FROM t;", "SELECT 1;"]
        );
    }

    fn semicolon() -> ScriptItem {
        ScriptItem::Statement {
            end: QueryEnd::Semicolon,
        }
    }

    fn meta(name: &str, args: &str) -> ScriptItem {
        ScriptItem::Metacommand {
            name: name.to_string(),
            args: args.to_string(),
        }
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
                sql("SELECT 1;"),
                semicolon(),
                sql("SELECT 2;"),
                semicolon(),
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
                sql("SELECT\n1;"),
                semicolon(),
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
    fn metacommand_splits_into_name_and_arguments() {
        let items = lex("\\d crabs\nSELECT 1;\n");
        assert_eq!(
            items,
            [
                ScriptItem::Line("\\d crabs".into()),
                meta("d", " crabs"),
                ScriptItem::Line("SELECT 1;".into()),
                sql("SELECT 1;"),
                semicolon(),
            ]
        );
    }

    /// psql's `\g` family sends the pending buffer rather than discarding it.
    #[test]
    fn query_terminator_carries_the_pending_buffer() {
        let items = lex("SELECT 1 \\gset\nSELECT 2;\n");
        assert_eq!(
            items,
            [
                ScriptItem::Line("SELECT 1 \\gset".into()),
                sql("SELECT 1 "),
                ScriptItem::Statement {
                    end: QueryEnd::Backslash {
                        name: "gset".into(),
                        args: String::new(),
                    },
                },
                ScriptItem::Line("SELECT 2;".into()),
                sql("SELECT 2;"),
                semicolon(),
            ]
        );
    }

    /// `\g` on its own line sends what the previous lines accumulated
    /// (`errors.sql:284`), newline and all.
    #[test]
    fn bare_g_sends_the_buffer_built_on_earlier_lines() {
        assert_eq!(statements("CREATE TABLE\n\\g\n"), ["CREATE TABLE\n"]);
    }

    /// A non-terminating command leaves the buffer alone and SQL scanning
    /// resumes after its arguments — `psql.out:4586` needs both.
    #[test]
    fn metacommand_does_not_disturb_the_query_buffer() {
        // The indentation ahead of each command is ordinary SQL text and stays
        // in the buffer, exactly as psql accumulates it.
        assert_eq!(
            statements("select\n  \\if true\n    42\n  \\endif\n  forty_two;\n"),
            ["select\n  \n    42\n  \n  forty_two;"]
        );
    }

    /// `\\` takes no arguments, so everything after it is SQL again
    /// (`psql.out:4600`).
    #[test]
    fn double_backslash_takes_no_arguments() {
        let items = lex(r"select \if false \\ (bogus \else \\ 42 \endif \\ forty_two;");
        let commands: Vec<&ScriptItem> = items
            .iter()
            .filter(|item| matches!(item, ScriptItem::Metacommand { .. }))
            .collect();
        assert_eq!(
            commands,
            [
                &meta("if", " false "),
                &meta("\\", ""),
                &meta("else", " "),
                &meta("\\", ""),
                &meta("endif", " "),
                &meta("\\", ""),
            ]
        );
        // Every non-command run stays in the buffer; the runner drops the
        // inactive-branch text later, as psql does.
        assert_eq!(
            statements(r"select \if false \\ (bogus \else \\ 42 \endif \\ forty_two;"),
            ["select  (bogus  42  forty_two;"]
        );
    }

    /// `\;` is a buffer-internal separator: one Query with two statements,
    /// no metacommand (`transactions.out:977`).
    #[test]
    fn backslash_semicolon_joins_statements_into_one_query() {
        let items = lex("SELECT 1\\; SELECT 2;\n");
        assert_eq!(
            items,
            [
                ScriptItem::Line("SELECT 1\\; SELECT 2;".into()),
                sql("SELECT 1; SELECT 2;"),
                semicolon(),
            ]
        );
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
                sql("SELECT\n1;"),
                semicolon(),
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
                sql("SELECT 'a\n\nb';"),
                semicolon(),
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
                sql("COPY t FROM stdin;"),
                semicolon(),
                ScriptItem::CopyData("1\ta\n2\tb\n".into()),
                ScriptItem::Line("SELECT 1;".into()),
                sql("SELECT 1;"),
                semicolon(),
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
                sql("COPY t FROM stdin;"),
                semicolon(),
                ScriptItem::CopyData("a;b\n\nc\n".into()),
            ]
        );
    }

    #[test]
    fn is_copy_from_stdin_recognizes_forms() {
        assert!(is_copy_from_stdin("COPY t FROM stdin;"));
        assert!(is_copy_from_stdin("COPY t (a, b) FROM STDIN"));
        assert!(is_copy_from_stdin("COPY t FROM stdin WITH (FORMAT csv)"));
        assert!(!is_copy_from_stdin("COPY t TO stdout;"));
        assert!(!is_copy_from_stdin("COPY t FROM '/tmp/f';"));
        assert!(!is_copy_from_stdin("SELECT 1;"));
        // "FROM STDIN" as a substring inside a COPY TO (query) must not match.
        assert!(!is_copy_from_stdin("COPY (SELECT a FROM stdin) TO stdout;"));
        // A STDIN-prefixed identifier is not the STDIN keyword.
        assert!(!is_copy_from_stdin("COPY t FROM stdin_table;"));
    }
}
