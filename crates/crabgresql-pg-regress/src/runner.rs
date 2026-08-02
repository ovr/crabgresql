//! Suite orchestration: run each test's `sql/<name>.sql` against an
//! in-process server, write `results/<name>.out`, and compare byte-for-byte
//! against `expected/<name>.out` (or its `_1` … `_9` variants, pg_regress
//! style). Failures land as unified diffs in `<outdir>/regression.diffs`.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use similar::TextDiff;
use tokio::net::TcpListener;

use crate::client::{Client, Field, QueryEvent};
use crate::describe;
use crate::format;
use crate::psql_var::{self, Variables};
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
    /// The environment `\getenv` reads. Leave empty to take pg_regress's
    /// defaults from [`regress_environment`].
    pub env: BTreeMap<String, String>,
}

impl SuiteConfig {
    /// The environment scripts see, with [`regress_environment`] filling in any
    /// variable the caller did not set.
    fn environment(&self) -> BTreeMap<String, String> {
        let mut env = regress_environment(&self.regress_dir, &self.outdir);
        env.extend(self.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        env
    }
}

/// The four variables pg_regress passes to psql, which upstream scripts read
/// with `\getenv` to locate their data files and the C test library. Real
/// process environment entries win, so a caller can point a run elsewhere.
///
/// `PG_LIBDIR` has no meaningful value here — nothing loads C modules — so it
/// defaults to empty; the `CREATE FUNCTION … LANGUAGE C` statements that use it
/// fail either way.
pub fn regress_environment(regress_dir: &Path, outdir: &Path) -> BTreeMap<String, String> {
    let absolute = |path: &Path| {
        std::fs::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .into_owned()
    };
    let dlsuffix = if cfg!(target_os = "macos") {
        ".dylib"
    } else {
        ".so"
    };
    [
        ("PG_ABS_SRCDIR", absolute(regress_dir)),
        ("PG_ABS_BUILDDIR", absolute(outdir)),
        ("PG_LIBDIR", String::new()),
        ("PG_DLSUFFIX", dlsuffix.to_string()),
    ]
    .into_iter()
    .map(|(name, default)| {
        let value = std::env::var(name).unwrap_or(default);
        (name.to_string(), value)
    })
    .collect()
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
    // Scripts load fixtures with `COPY … FROM :'abs_srcdir/data/x.data'`, which
    // is the suite's source tree, not the throwaway PGDATA — so the server has
    // to be told that tree is readable.
    let copy_files = crabgresql_server::CopyFileAccess::confined_to(data_dir.path())
        .allowing(&config.regress_dir);
    let server = tokio::spawn(crabgresql_server::serve_with(
        listener, engine, txnmgr, copy_files,
    ));

    let results_dir = config.outdir.join("results");
    std::fs::create_dir_all(&results_dir)?;
    let diffs_path = config.outdir.join("regression.diffs");
    let mut diffs = String::new();

    let environment = config.environment();
    let setup = config.setup.iter().map(|name| (name, false));
    let checked = config.tests.iter().map(|name| (name, true));
    let mut outcomes = Vec::new();
    for (name, check) in setup.chain(checked) {
        let sql_path = config.regress_dir.join("sql").join(format!("{name}.sql"));
        // Lossy: a few upstream scripts are deliberately not UTF-8 (encoding
        // tests); they can only fail, but they must not abort the run.
        let sql = String::from_utf8_lossy(&std::fs::read(&sql_path)?).into_owned();
        let output = run_test(port, &sql, config.statement_timeout, &environment).await?;
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
async fn run_test(
    port: u16,
    sql: &str,
    statement_timeout: Duration,
    environment: &BTreeMap<String, String>,
) -> io::Result<String> {
    let mut client = Client::connect(port).await?;
    let mut out = String::new();
    // psql starts with an empty NULL marker. `\pset null` updates this for the
    // lifetime of the current script/connection only.
    let mut null_display = String::new();
    // `\set` variables, likewise scoped to this script.
    let mut vars = Variables::new();
    // A `COPY … FROM STDIN` statement is held here until its `CopyData` payload
    // arrives (the data lines are lexed after the statement), then run together.
    let mut pending_copy: Option<String> = None;
    for item in lex(sql) {
        match item {
            // Echoed verbatim: `psql -a` prints the source line, so a variable
            // reference is visible unexpanded even though the statement sent to
            // the server has it substituted.
            ScriptItem::Line(line) => {
                out.push_str(&line);
                out.push('\n');
            }
            // `\d <relation>` is the one metacommand that has to talk to the
            // server, so it is dispatched here rather than in the sync
            // `run_metacommand`. It declines far more often than it succeeds
            // (see `describe`), and a declined one falls through to the stub.
            ScriptItem::Metacommand(command) => {
                let described = match describe_pattern(&command) {
                    Some(pattern) => {
                        match describe::describe(&mut client, pattern, statement_timeout).await {
                            Ok(described) => described,
                            // Same handling as a statement that loses the
                            // connection: mark it and abandon the file.
                            Err(_) => {
                                out.push_str("connection to server was lost\n");
                                break;
                            }
                        }
                    }
                    None => None,
                };
                match described {
                    Some(text) => out.push_str(&text),
                    None => run_metacommand(
                        &command,
                        environment,
                        &mut vars,
                        &mut null_display,
                        &mut out,
                    ),
                }
            }
            ScriptItem::Statement(statement) => {
                let statement = psql_var::substitute(&statement, &vars);
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

/// The relation `\d <name>` describes, or `None` for any other metacommand —
/// including `\d` with no argument (which lists relations), `\d+`, and a `\d`
/// with several arguments or another command chained onto it.
fn describe_pattern(command: &str) -> Option<&str> {
    let (name, arguments) = split_command_name(command);
    if name != "d" || arguments.is_empty() {
        return None;
    }
    (!arguments.contains(char::is_whitespace) && !arguments.contains('\\')).then_some(arguments)
}

/// Run one backslash command, appending whatever psql would print. The four
/// implemented commands (`\set`, `\unset`, `\getenv`, `\pset null`) print
/// nothing on success; everything else gets the "not supported" stub.
///
/// A command may be followed by another on the same line, introduced by an
/// unquoted `\` — the corpus uses `\\` purely to hang a trailing comment off a
/// `\set` (`regproc.sql:108`). Chained commands are dispatched in turn, so the
/// no-output `\\` stays silent instead of being mistaken for an argument.
fn run_metacommand(
    command: &str,
    environment: &BTreeMap<String, String>,
    vars: &mut Variables,
    null_display: &mut String,
    out: &mut String,
) {
    let mut command = command.to_string();
    loop {
        let (name, arguments) = split_command_name(&command);
        // Arguments expand against the variables as they stand *before* this
        // command runs, which is what makes `\set dobody :dobody '…'` append.
        let parsed = psql_var::split_args(arguments, vars);
        let args: Vec<&str> = parsed.args.iter().map(String::as_str).collect();
        match (name, args.as_slice()) {
            // `\set name [value …]` concatenates every value with no
            // separator; with no value at all the variable becomes empty.
            ("set", [variable, values @ ..]) => vars.set(variable, values.concat()),
            ("unset", [variable, ..]) => vars.unset(variable),
            // `\getenv name ENVVAR` leaves `name` unset when the environment
            // has no such entry.
            ("getenv", [variable, source, ..]) => match environment.get(*source) {
                Some(value) => vars.set(variable, value.clone()),
                None => vars.unset(variable),
            },
            ("pset", ["null", value, ..]) => *null_display = (*value).to_string(),
            // `\pset null` with no value queries the setting, which is silent
            // under `-q`.
            ("pset", ["null"]) => {}
            // `\\` ends the query buffer and swallows the rest of the line.
            ("\\", _) => {}
            _ => out.push_str(&format::metacommand_stub(&command)),
        }
        if parsed.rest.is_empty() {
            return;
        }
        // Drop the `\` that introduced the next command.
        command = parsed.rest[1..].to_string();
    }
}

/// Split `set VERBOSITY terse` into `("set", "VERBOSITY terse")`. psql ends a
/// command name at the first character that cannot be part of one, so
/// `\pset null` splits on the space while a lone `\\` yields the name `\`.
fn split_command_name(command: &str) -> (&str, &str) {
    let end = command
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '?' && c != '!')
        .unwrap_or(command.len());
    if end == 0 {
        // A non-alphanumeric command is a single character, e.g. `\\` or `\.`.
        return command.split_at(1.min(command.len()));
    }
    let (name, rest) = command.split_at(end);
    (name, rest.trim_start())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a script's worth of metacommands, returning the printed output, the
    /// resulting NULL marker and the variables.
    fn run_all(commands: &[&str]) -> (String, String, Variables) {
        let environment = BTreeMap::from([("PG_ABS_SRCDIR".to_string(), "/src".to_string())]);
        let mut vars = Variables::new();
        let mut null_display = String::new();
        let mut out = String::new();
        for command in commands {
            run_metacommand(
                command,
                &environment,
                &mut vars,
                &mut null_display,
                &mut out,
            );
        }
        (out, null_display, vars)
    }

    #[track_caller]
    fn assert_null_marker(command: &str, expected: &str) {
        let (out, null_display, _) = run_all(&[command]);
        assert_eq!(out, "", "{command} printed something");
        assert_eq!(null_display, expected);
    }

    #[test]
    fn parses_pset_null_values() {
        assert_null_marker("pset null '(null)'", "(null)");
        assert_null_marker("pset null ''", "");
        assert_null_marker("pset null NULL", "NULL");
        assert_null_marker(r"pset null '\\N'", r"\N");
        // psql keeps double quotes in the value of an option like `\pset`.
        assert_null_marker(r#"pset null "(null)""#, r#""(null)""#);
        // Querying the setting is silent under `-q` and changes nothing.
        assert_null_marker("pset null", "");
    }

    /// `\d <name>` is dispatched to the server (see `describe`); every other
    /// spelling — the bare listing, `\d+`, a pattern with a wildcard or a
    /// chained command — is left to the stub.
    #[test]
    fn only_a_plain_relation_name_is_described() {
        assert_eq!(describe_pattern("d bit_defaults"), Some("bit_defaults"));
        assert_eq!(describe_pattern("d"), None);
        assert_eq!(describe_pattern("d+ t"), None);
        assert_eq!(describe_pattern("dt"), None);
        assert_eq!(describe_pattern("d a b"), None);
        assert_eq!(describe_pattern(r"d t \\ trailing"), None);
    }

    #[test]
    fn unimplemented_metacommands_still_stub() {
        let (out, _, _) = run_all(&["pset format aligned", "d crabs", "psetnull x"]);
        assert_eq!(
            out,
            "\\pset: metacommand not supported by crabgresql regress runner\n\
             \\d: metacommand not supported by crabgresql regress runner\n\
             \\psetnull: metacommand not supported by crabgresql regress runner\n"
        );
    }

    #[test]
    fn set_concatenates_values_and_unset_removes_them() {
        let (out, _, vars) = run_all(&["set filename /src '/data/onek.data'", "set empty"]);
        assert_eq!(out, "");
        assert_eq!(vars.get("filename"), Some("/src/data/onek.data"));
        // `\set name` with no value makes the variable empty, not undefined.
        assert_eq!(vars.get("empty"), Some(""));

        let (_, _, vars) = run_all(&["set a 1", "unset a"]);
        assert_eq!(vars.get("a"), None);
    }

    #[test]
    fn getenv_reads_the_environment_and_unsets_when_missing() {
        let (out, _, vars) = run_all(&[
            "set libdir stale",
            "getenv abs_srcdir PG_ABS_SRCDIR",
            "getenv libdir PG_LIBDIR",
        ]);
        assert_eq!(out, "");
        assert_eq!(vars.get("abs_srcdir"), Some("/src"));
        assert_eq!(vars.get("libdir"), None);
    }

    #[test]
    fn set_arguments_expand_earlier_variables() {
        // largeobject.sql builds a DO body by appending to itself.
        let (_, _, vars) = run_all(&[
            "getenv abs_srcdir PG_ABS_SRCDIR",
            "set filename :abs_srcdir '/data/onek.data'",
            "set body 'lo_export(loid, ' :'filename' ');'",
        ]);
        assert_eq!(vars.get("filename"), Some("/src/data/onek.data"));
        assert_eq!(
            vars.get("body"),
            Some("lo_export(loid, '/src/data/onek.data');")
        );
    }

    #[test]
    fn trailing_backslash_command_is_dispatched_separately() {
        // regproc.sql:108 — `\\` ends the arguments and swallows the comment,
        // printing nothing.
        let (out, _, vars) =
            run_all([r"set VERBOSITY sqlstate \\ -- encoding-dependent"].as_slice());
        assert_eq!(out, "");
        assert_eq!(vars.get("VERBOSITY"), Some("sqlstate"));
    }
}
