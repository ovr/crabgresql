//! Suite orchestration: run each test's `sql/<name>.sql` against an
//! in-process server, write `results/<name>.out`, and compare byte-for-byte
//! against `expected/<name>.out` (or its `_1` … `_9` variants, pg_regress
//! style). Failures land as unified diffs in `<outdir>/regression.diffs`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use similar::TextDiff;
use tokio::net::TcpListener;

use crate::client::{Client, Field, QueryEvent};
use crate::format;
use crate::script::{ScriptItem, is_copy_from_stdin, lex};

pub struct SuiteConfig {
    /// Directory containing `sql/` and `expected/`.
    pub regress_dir: PathBuf,
    /// Tests run first whose output is not checked (pg_regress's
    /// `test_setup` role when running an explicit test list).
    pub setup: Vec<String>,
    /// Tests to run and check, in order.
    pub tests: Vec<String>,
    /// Where `results/` and `regression.diffs` are written.
    pub outdir: PathBuf,
    pub statement_timeout: Duration,
}

pub struct SuiteReport {
    /// `(test name, passed)` in run order, setup tests excluded.
    pub outcomes: Vec<(String, bool)>,
}

impl SuiteReport {
    pub fn passed(&self) -> usize {
        self.outcomes.iter().filter(|(_, ok)| *ok).count()
    }

    pub fn total(&self) -> usize {
        self.outcomes.len()
    }

    pub fn all_passed(&self) -> bool {
        self.passed() == self.total()
    }
}

/// Run the whole suite against one server + engine instance, mirroring how
/// pg_regress runs every test in a single cluster. Each test gets a fresh
/// connection, as each pg_regress test gets a fresh psql.
pub async fn run_suite(config: &SuiteConfig) -> io::Result<SuiteReport> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    // The whole suite runs against one durable pg-engine over a throwaway data
    // directory (kept alive until this function returns, past `server.abort()`).
    let data_dir = tempfile::tempdir()?;
    let (engine, txnmgr) = crabgresql_server::open_pg_engine(data_dir.path())?;
    let server = tokio::spawn(crabgresql_server::serve_with(listener, engine, txnmgr));

    let results_dir = config.outdir.join("results");
    std::fs::create_dir_all(&results_dir)?;
    let diffs_path = config.outdir.join("regression.diffs");
    let mut diffs = String::new();

    let setup = config.setup.iter().map(|name| (name, false));
    let checked = config.tests.iter().map(|name| (name, true));
    let mut outcomes = Vec::new();
    for (name, check) in setup.chain(checked) {
        let sql_path = config.regress_dir.join("sql").join(format!("{name}.sql"));
        // Lossy: a few upstream scripts are deliberately not UTF-8 (encoding
        // tests); they can only fail, but they must not abort the run.
        let sql = String::from_utf8_lossy(&std::fs::read(&sql_path)?).into_owned();
        let output = run_test(port, &sql, config.statement_timeout).await?;
        let result_path = results_dir.join(format!("{name}.out"));
        std::fs::write(&result_path, &output)?;
        if !check {
            continue;
        }
        let passed = compare(config, name, &output, &result_path, &mut diffs)?;
        outcomes.push((name.clone(), passed));
    }
    server.abort();

    if diffs.is_empty() {
        let _ = std::fs::remove_file(&diffs_path);
    } else {
        std::fs::write(&diffs_path, diffs)?;
    }
    Ok(SuiteReport { outcomes })
}

/// True if the output matches any expected candidate exactly; otherwise the
/// diff against the closest candidate is appended to `diffs`.
fn compare(
    config: &SuiteConfig,
    name: &str,
    output: &str,
    result_path: &Path,
    diffs: &mut String,
) -> io::Result<bool> {
    let mut closest: Option<(f32, PathBuf, String)> = None;
    for candidate in expected_candidates(&config.regress_dir, name) {
        let expected = String::from_utf8_lossy(&std::fs::read(&candidate)?).into_owned();
        if expected == output {
            return Ok(true);
        }
        let ratio = TextDiff::from_lines(expected.as_str(), output).ratio();
        if closest.as_ref().is_none_or(|(best, _, _)| ratio > *best) {
            closest = Some((ratio, candidate, expected));
        }
    }
    match closest {
        Some((_, expected_path, expected)) => {
            diffs.push_str(&format!(
                "diff -U3 {} {}\n",
                expected_path.display(),
                result_path.display()
            ));
            diffs.push_str(
                &TextDiff::from_lines(expected.as_str(), output)
                    .unified_diff()
                    .context_radius(3)
                    .header(
                        &expected_path.display().to_string(),
                        &result_path.display().to_string(),
                    )
                    .to_string(),
            );
            diffs.push('\n');
        }
        None => diffs.push_str(&format!("test {name}: no expected file found\n\n")),
    }
    Ok(false)
}

/// `expected/<name>.out` plus pg_regress's `<name>_1.out` … `<name>_9.out`
/// alternative-output convention.
fn expected_candidates(regress_dir: &Path, name: &str) -> Vec<PathBuf> {
    let expected = regress_dir.join("expected");
    std::iter::once(expected.join(format!("{name}.out")))
        .chain((1..=9).map(|i| expected.join(format!("{name}_{i}.out"))))
        .filter(|path| path.exists())
        .collect()
}

/// Execute one script on a fresh connection, producing the text psql would
/// print. A timeout or lost connection appends a deterministic marker and
/// abandons the rest of the file, like a dying psql would.
async fn run_test(port: u16, sql: &str, statement_timeout: Duration) -> io::Result<String> {
    let mut client = Client::connect(port).await?;
    let mut out = String::new();
    // psql starts with an empty NULL marker. `\pset null` updates this for the
    // lifetime of the current script/connection only.
    let mut null_display = String::new();
    // A `COPY … FROM STDIN` statement is held here until its `CopyData` payload
    // arrives (the data lines are lexed after the statement), then run together.
    let mut pending_copy: Option<String> = None;
    for item in lex(sql) {
        match item {
            ScriptItem::Line(line) => {
                out.push_str(&line);
                out.push('\n');
            }
            ScriptItem::Metacommand(command) => {
                match pset_null(&command) {
                    PsetNull::Set(value) => null_display = value,
                    // With no value, quiet psql leaves the setting unchanged
                    // and emits nothing.
                    PsetNull::Query => {}
                    PsetNull::Other => out.push_str(&format::metacommand_stub(&command)),
                }
            }
            ScriptItem::Statement(statement) => {
                // Defer a COPY FROM STDIN: it runs once its data is collected.
                if is_copy_from_stdin(&statement) {
                    pending_copy = Some(statement);
                    continue;
                }
                match tokio::time::timeout(statement_timeout, client.simple_query(&statement)).await
                {
                    Ok(Ok(events)) => render_events(&mut out, &events, &statement, &null_display),
                    Ok(Err(_)) => {
                        out.push_str("connection to server was lost\n");
                        break;
                    }
                    Err(_) => {
                        out.push_str("FATAL:  statement timeout in crabgresql regress runner\n");
                        break;
                    }
                }
            }
            ScriptItem::CopyData(data) => {
                let Some(statement) = pending_copy.take() else {
                    continue;
                };
                match tokio::time::timeout(statement_timeout, client.copy_in(&statement, &data))
                    .await
                {
                    Ok(Ok(events)) => render_events(&mut out, &events, &statement, &null_display),
                    Ok(Err(_)) => {
                        out.push_str("connection to server was lost\n");
                        break;
                    }
                    Err(_) => {
                        out.push_str("FATAL:  statement timeout in crabgresql regress runner\n");
                        break;
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Print a statement's responses: result tables, errors and notices. Command
/// tags are suppressed, as under `psql -q`.
fn render_events(out: &mut String, events: &[QueryEvent], query: &str, null_display: &str) {
    let mut fields: Option<Vec<Field>> = None;
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    for event in events {
        match event {
            QueryEvent::RowDescription(f) => {
                fields = Some(f.clone());
                rows.clear();
            }
            QueryEvent::Row(row) => rows.push(row.clone()),
            QueryEvent::CommandComplete(_tag) => {
                if let Some(fields) = fields.take() {
                    out.push_str(&format::format_table(&fields, &rows, null_display));
                    rows.clear();
                }
            }
            QueryEvent::EmptyQuery => {}
            QueryEvent::Error(error) => out.push_str(&format::format_error(error, query)),
            QueryEvent::Notice(notice) => out.push_str(&format::format_notice(notice, query)),
        }
    }
}

/// What a metacommand means to the runner.
enum PsetNull {
    /// `\pset null <value>` — set the NULL marker.
    Set(String),
    /// `\pset null` — query the setting, which is silent under `-q`.
    Query,
    /// Any other metacommand; the runner does not implement it.
    Other,
}

/// Recognize `\pset null [value]`.
fn pset_null(command: &str) -> PsetNull {
    let Some(rest) = strip_word(command, "pset").and_then(|rest| strip_word(rest, "null")) else {
        return PsetNull::Other;
    };
    if rest.is_empty() {
        PsetNull::Query
    } else {
        PsetNull::Set(parse_meta_argument(rest))
    }
}

/// Strip `word` from the front of `s` if it is followed by a word boundary,
/// returning the remainder with leading whitespace removed.
fn strip_word<'a>(s: &'a str, word: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(word)?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim_start())
}

/// Parse the first argument of a psql metacommand, to the extent the corpus
/// needs it: single quotes group and disappear, and `\` keeps the next
/// character literally.
///
/// This is deliberately narrower than psql, which also decodes C escapes
/// (`\n`, `\t`, `\xNN`, octal) inside single quotes, leaves double quotes in
/// the value, and treats an *unquoted* backslash as the start of the next
/// metacommand rather than an escape. Every `\pset null` argument in the
/// vendored corpus is a plain single-quoted literal, and the one that does
/// contain a backslash (`'\\N'`, strings.sql) decodes the same under both
/// rules — so the divergences are unreachable today. Widen this only
/// alongside a test that needs it.
fn parse_meta_argument(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        // A backslash is never a quote character, so it means the same thing
        // inside and outside one.
        if c == '\\' {
            out.extend(chars.next());
        } else if c == '\'' {
            quoted = !quoted;
        } else if !quoted && c.is_whitespace() {
            break;
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[track_caller]
    fn assert_sets(command: &str, expected: &str) {
        match pset_null(command) {
            PsetNull::Set(value) => assert_eq!(value, expected),
            _ => panic!("{command} did not set the NULL marker"),
        }
    }

    #[test]
    fn parses_pset_null_values() {
        assert_sets("pset null '(null)'", "(null)");
        assert_sets("pset null ''", "");
        assert_sets("pset null NULL", "NULL");
        assert_sets(r"pset null '\\N'", r"\N");
        // psql keeps double quotes in the value of an option like `\pset`.
        assert_sets(r#"pset null "(null)""#, r#""(null)""#);
        assert!(matches!(pset_null("pset null"), PsetNull::Query));
        assert!(matches!(pset_null("pset format aligned"), PsetNull::Other));
        // `null` must be a whole word, and `pset` must be the whole command.
        assert!(matches!(pset_null("pset nullx x"), PsetNull::Other));
        assert!(matches!(pset_null("psetnull x"), PsetNull::Other));
    }
}
