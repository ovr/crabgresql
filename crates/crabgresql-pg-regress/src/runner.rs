//! Suite orchestration: run each test's `sql/<name>.sql` against an
//! in-process server, write `results/<name>.out`, and compare byte-for-byte
//! against `expected/<name>.out` (or its `_1` … `_9` variants, pg_regress
//! style). Failures land as unified diffs in `<outdir>/regression.diffs`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crabgresql_memory_storage::MemoryEngine;
use similar::TextDiff;
use tokio::net::TcpListener;

use crate::client::{Client, Field, QueryEvent};
use crate::format;
use crate::script::{ScriptItem, lex};

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
    let server = tokio::spawn(crabgresql_server::serve(
        listener,
        Arc::new(MemoryEngine::new()),
    ));

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
    for item in lex(sql) {
        match item {
            ScriptItem::Line(line) => {
                out.push_str(&line);
                out.push('\n');
            }
            ScriptItem::Metacommand(command) => {
                out.push_str(&format::metacommand_stub(&command));
            }
            ScriptItem::Statement(statement) => {
                match tokio::time::timeout(statement_timeout, client.simple_query(&statement)).await
                {
                    Ok(Ok(events)) => render_events(&mut out, &events, &statement),
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
fn render_events(out: &mut String, events: &[QueryEvent], query: &str) {
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
                    out.push_str(&format::format_table(&fields, &rows));
                    rows.clear();
                }
            }
            QueryEvent::EmptyQuery => {}
            QueryEvent::Error(error) => out.push_str(&format::format_error(error, query)),
            QueryEvent::Notice(notice) => out.push_str(&format::format_notice(notice, query)),
        }
    }
}
