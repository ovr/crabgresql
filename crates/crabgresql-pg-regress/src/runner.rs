//! Suite orchestration: run each test's `sql/<name>.sql` against an
//! in-process server, write `results/<name>.out`, and compare byte-for-byte
//! against `expected/<name>.out` (or its `_1` … `_9` variants, pg_regress
//! style). Failures land as unified diffs in `<outdir>/regression.diffs`.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use similar::TextDiff;
use tokio::net::TcpListener;

use crate::client::{Client, Field, QueryEvent};
use crate::describe;
use crate::format;
use crate::psql_var::{self, Variables};
use crate::script::{QueryEnd, ScriptItem, is_copy_from_stdin, lex};

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

pub struct TestOutcome {
    pub name: String,
    pub passed: bool,
    /// Wall-clock time spent executing the script, without the diff against
    /// `expected/`.
    pub duration: Duration,
}

pub struct SuiteReport {
    /// One entry per checked test in run order; setup tests are excluded.
    pub outcomes: Vec<TestOutcome>,
    /// Wall-clock time of the whole run, including server startup and the
    /// setup tests that `outcomes` leaves out.
    pub duration: Duration,
}

impl SuiteReport {
    pub fn passed(&self) -> usize {
        self.outcomes.iter().filter(|o| o.passed).count()
    }

    pub fn total(&self) -> usize {
        self.outcomes.len()
    }

    pub fn all_passed(&self) -> bool {
        self.passed() == self.total()
    }

    pub fn failed(&self) -> impl Iterator<Item = &TestOutcome> {
        self.outcomes.iter().filter(|o| !o.passed)
    }

    /// The `n` longest-running tests, slowest first.
    pub fn slowest(&self, n: usize) -> Vec<&TestOutcome> {
        let mut sorted: Vec<&TestOutcome> = self.outcomes.iter().collect();
        sorted.sort_by(|a, b| b.duration.cmp(&a.duration));
        sorted.truncate(n);
        sorted
    }
}

/// Run the whole suite against one server + engine instance, mirroring how
/// pg_regress runs every test in a single cluster. Each test gets a fresh
/// connection, as each pg_regress test gets a fresh psql.
pub async fn run_suite(config: &SuiteConfig) -> io::Result<SuiteReport> {
    let started = Instant::now();
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
        let test_started = Instant::now();
        let output = run_test(port, &sql, config.statement_timeout, &environment).await?;
        let duration = test_started.elapsed();
        let result_path = results_dir.join(format!("{name}.out"));
        std::fs::write(&result_path, &output)?;
        if !check {
            continue;
        }
        let passed = compare(config, name, &output, &result_path, &mut diffs)?;
        outcomes.push(TestOutcome {
            name: name.clone(),
            passed,
            duration,
        });
    }
    server.abort();

    if diffs.is_empty() {
        let _ = std::fs::remove_file(&diffs_path);
    } else {
        std::fs::write(&diffs_path, diffs)?;
    }
    Ok(SuiteReport {
        outcomes,
        duration: started.elapsed(),
    })
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
    // psql's output settings, scoped to this script: `\pset`, `\x`, `\a`
    // and `\t` all change them.
    let mut printing = format::Printing::default();
    // `\set` variables, likewise scoped to this script.
    let mut vars = Variables::new();
    // A `COPY … FROM STDIN` statement is held here until its `CopyData` payload
    // arrives (the data lines are lexed after the statement), then run together.
    let mut pending_copy: Option<String> = None;
    // psql's `previous_buf`: what a `\g`-family command on an empty buffer
    // re-runs (`psql.out:4579`).
    let mut last_query = String::new();
    // psql's `query_buf`: the runner owns it, not the lexer, because text
    // scanned inside an inactive `\if` branch has to be thrown away while the
    // text around it survives.
    let mut buffer = String::new();
    let mut branches: Vec<Branch> = Vec::new();
    for item in lex(sql) {
        // Every input line echoes regardless of branch state; only the work
        // is skipped.
        let active = branches_active(&branches);
        match item {
            // Echoed verbatim: `psql -a` prints the source line, so a variable
            // reference is visible unexpanded even though the statement sent to
            // the server has it substituted.
            ScriptItem::Line(line) => {
                out.push_str(&line);
                out.push('\n');
            }
            // Text scanned while an `\if` branch is inactive is discarded, as
            // psql truncates its query buffer back to where the scan started.
            ScriptItem::Sql(text) => {
                if active {
                    buffer.push_str(&text);
                }
            }
            // `\d <relation>` is the one metacommand that has to talk to the
            // server, so it is dispatched here rather than in the sync
            // `run_metacommand`. It declines far more often than it succeeds
            // (see `describe`), and a declined one falls through to the stub.
            ScriptItem::Metacommand { name, args } => {
                // The conditional commands are the only ones that run while a
                // branch is inactive; everything else — including the
                // "not supported" stub — stays silent (`psql.out:4680`).
                if matches!(name.as_str(), "if" | "elif" | "else" | "endif") {
                    let parsed = psql_var::split_args(&args, &vars);
                    let args: Vec<&str> = parsed.args.iter().map(String::as_str).collect();
                    run_conditional(&name, &args, &mut branches, &mut out);
                    continue;
                }
                if !active {
                    continue;
                }
                // `\quit` ends the script at once: psql stops reading, so not
                // even the remaining lines are echoed (`json_encoding_2.out`).
                if matches!(name.as_str(), "q" | "quit") {
                    break;
                }
                // `\c` and `\c -` reconnect to the same database as the same
                // user, which is the corpus's way of dropping session state
                // (temp tables, GUCs). psql prints nothing: not one
                // "You are now connected" line appears in the vendored
                // expected files. Any other argument names a different target
                // and only `psql.sql` does that, so it still stubs.
                if name == "c" && matches!(args.trim(), "" | "-") {
                    match Client::connect(port).await {
                        Ok(fresh) => client = fresh,
                        Err(_) => {
                            out.push_str("connection to server was lost\n");
                            break;
                        }
                    }
                    buffer.clear();
                    continue;
                }
                let described = match describe_pattern(&name, &args) {
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
                        &name,
                        &args,
                        environment,
                        &mut vars,
                        &mut printing,
                        &mut out,
                    ),
                }
            }
            ScriptItem::Statement { end } => {
                // psql resets the buffer whether or not it sent it, so an
                // inactive branch's half-built statement does not leak forward.
                let scanned = std::mem::take(&mut buffer);
                if !active {
                    continue;
                }
                // `\gdesc` and `\crosstabview` re-render the result rather than
                // just sending it; neither is implemented, so they stub and
                // drop the buffer.
                if let QueryEnd::Backslash { name, .. } = &end
                    && !matches!(name.as_str(), "g" | "gx" | "gset" | "gexec")
                {
                    out.push_str(&format::metacommand_stub(name));
                    continue;
                }
                // A `\g`-family command on an empty buffer re-runs the previous
                // query (`psql.out:4579`); a bare `;` on an empty buffer does not.
                let text = match (scanned.trim().is_empty(), &end) {
                    (true, QueryEnd::Backslash { .. }) => last_query.clone(),
                    (true, _) => continue,
                    (false, _) => psql_var::substitute(&scanned, &vars),
                };
                if text.trim().is_empty() {
                    continue;
                }
                last_query = text.clone();
                // Defer a COPY FROM STDIN: it runs once its data is collected.
                if is_copy_from_stdin(&text) {
                    pending_copy = Some(text);
                    continue;
                }
                let events =
                    match tokio::time::timeout(statement_timeout, client.simple_query(&text)).await
                    {
                        Ok(Ok(events)) => events,
                        Ok(Err(_)) => {
                            out.push_str("connection to server was lost\n");
                            break;
                        }
                        Err(_) => {
                            out.push_str(
                                "FATAL:  statement timeout in crabgresql regress runner\n",
                            );
                            break;
                        }
                    };
                match &end {
                    QueryEnd::Backslash { name, args } if name == "gset" => {
                        let prefix = psql_var::split_args(args, &vars)
                            .args
                            .first()
                            .cloned()
                            .unwrap_or_default();
                        capture_gset(&mut out, &events, &text, &prefix, &mut vars);
                    }
                    // `\gexec` runs every cell of the result as a query, in
                    // row-major order, echoing each before it runs.
                    QueryEnd::Backslash { name, .. } if name == "gexec" => {
                        // A failed generating query prints its error and
                        // generates nothing.
                        if events.iter().any(|e| matches!(e, QueryEvent::Error(_))) {
                            render_events(&mut out, &events, &text, &printing);
                            continue;
                        }
                        let mut lost = None;
                        for generated in gexec_queries(&events) {
                            out.push_str(&generated);
                            out.push('\n');
                            match tokio::time::timeout(
                                statement_timeout,
                                client.simple_query(&generated),
                            )
                            .await
                            {
                                Ok(Ok(events)) => {
                                    render_events(&mut out, &events, &generated, &printing)
                                }
                                Ok(Err(_)) => {
                                    lost = Some("connection to server was lost\n");
                                    break;
                                }
                                Err(_) => {
                                    lost = Some(
                                        "FATAL:  statement timeout in crabgresql regress runner\n",
                                    );
                                    break;
                                }
                            }
                        }
                        if let Some(marker) = lost {
                            out.push_str(marker);
                            break;
                        }
                    }
                    // `\gx` is `\g` with expanded output for this query only:
                    // the setting is not persisted (`psql.out:31`).
                    QueryEnd::Backslash { name, .. } if name == "gx" => {
                        let once = format::Printing {
                            expanded: true,
                            ..printing.clone()
                        };
                        render_events(&mut out, &events, &text, &once);
                    }
                    _ => render_events(&mut out, &events, &text, &printing),
                }
            }
            ScriptItem::CopyData(data) => {
                let Some(statement) = pending_copy.take() else {
                    continue;
                };
                match tokio::time::timeout(statement_timeout, client.copy_in(&statement, &data))
                    .await
                {
                    Ok(Ok(events)) => render_events(&mut out, &events, &statement, &printing),
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
fn render_events(
    out: &mut String,
    events: &[QueryEvent],
    query: &str,
    printing: &format::Printing,
) {
    let mut fields: Option<Vec<Field>> = None;
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    for event in events {
        match event {
            QueryEvent::RowDescription(f) => {
                fields = Some(f.clone());
                rows.clear();
            }
            QueryEvent::Row(row) => rows.push(row.clone()),
            // psql writes a copy-out straight to its output: no column header,
            // no table framing, no `(n rows)` footer, and the `COPY n` tag is
            // suppressed like every other tag. The payload already ends in a
            // newline. It carries no RowDescription, so `fields`/`rows` — which
            // belong to a result set that may still be in flight — are untouched.
            QueryEvent::CopyOut(bytes) => out.push_str(&String::from_utf8_lossy(bytes)),
            QueryEvent::CommandComplete(_tag) => {
                if let Some(fields) = fields.take() {
                    out.push_str(&format::format_table(printing, &fields, &rows));
                    rows.clear();
                }
            }
            QueryEvent::EmptyQuery => {}
            QueryEvent::Error(error) => out.push_str(&format::format_error(error, query)),
            QueryEvent::Notice(notice) => out.push_str(&format::format_notice(notice, query)),
        }
    }
}

/// One level of psql's `\if` stack.
#[derive(Clone, Copy, PartialEq)]
enum Branch {
    /// The `\if` condition was true and we are in that arm.
    IfTrue,
    /// The `\if` condition was false; a later `\elif` may still fire.
    IfFalse,
    /// An arm already ran (or the whole block sits inside an inactive one), so
    /// nothing further in this block runs and no condition is even evaluated.
    Ignored,
    ElseTrue,
    ElseFalse,
}

/// Whether commands and query text at this point run or are thrown away.
fn branches_active(branches: &[Branch]) -> bool {
    branches
        .last()
        .is_none_or(|b| matches!(b, Branch::IfTrue | Branch::ElseTrue))
}

/// psql's `ParseVariableBool`: the six spellings, matched case-insensitively by
/// unique prefix, plus the numeric forms. `None` is psql's parse failure.
fn parse_bool(value: &str) -> Option<bool> {
    let value = value.to_ascii_lowercase();
    if value.is_empty() {
        return None;
    }
    for (word, result) in [
        ("true", true),
        ("false", false),
        ("yes", true),
        ("no", false),
        ("on", true),
        ("off", false),
    ] {
        // "o" is ambiguous between "on" and "off", so psql requires two
        // characters there; every other word is unique at one.
        let minimum = if word.starts_with('o') { 2 } else { 1 };
        if value.len() >= minimum && word.starts_with(&value) {
            return Some(result);
        }
    }
    value.parse::<i64>().ok().map(|n| n != 0)
}

/// psql joins a conditional's arguments into one boolean expression; an
/// unparsable one is reported and taken as false (`psql.out:4666`).
fn evaluate_condition(args: &[&str], out: &mut String) -> bool {
    let expression = args.join(" ");
    parse_bool(&expression).unwrap_or_else(|| {
        out.push_str(&format!(
            "unrecognized value \"{expression}\" for \"\\if expression\": Boolean expected\n"
        ));
        false
    })
}

/// Apply an `\if`-family command to the branch stack, appending whatever psql
/// prints (nothing, except for the malformed-block errors).
fn run_conditional(name: &str, args: &[&str], branches: &mut Vec<Branch>, out: &mut String) {
    let active = branches_active(branches);
    match name {
        "if" => branches.push(if !active {
            Branch::Ignored
        } else if evaluate_condition(args, out) {
            Branch::IfTrue
        } else {
            Branch::IfFalse
        }),
        "elif" => {
            let next = match branches.last() {
                None => return out.push_str("\\elif: no matching \\if\n"),
                Some(Branch::ElseTrue | Branch::ElseFalse) => {
                    return out.push_str("\\elif: cannot occur after \\else\n");
                }
                // An arm already ran, or the block is inside an inactive one:
                // the condition is not even evaluated.
                Some(Branch::IfTrue) => Branch::Ignored,
                Some(Branch::Ignored) => Branch::Ignored,
                Some(Branch::IfFalse) => {
                    if evaluate_condition(args, out) {
                        Branch::IfTrue
                    } else {
                        Branch::IfFalse
                    }
                }
            };
            *branches.last_mut().expect("checked above") = next;
        }
        "else" => {
            let next = match branches.last() {
                None => return out.push_str("\\else: no matching \\if\n"),
                Some(Branch::ElseTrue | Branch::ElseFalse) => {
                    return out.push_str("\\else: cannot occur after \\else\n");
                }
                Some(Branch::IfFalse) => Branch::ElseTrue,
                Some(Branch::IfTrue | Branch::Ignored) => Branch::ElseFalse,
            };
            *branches.last_mut().expect("checked above") = next;
        }
        "endif" => {
            if branches.pop().is_none() {
                out.push_str("\\endif: no matching \\if\n");
            }
        }
        _ => unreachable!("not an \\if-family command: {name}"),
    }
}

/// A result set as `\gset` and `\gexec` consume it: column names and rows of
/// already-rendered text.
type ResultSet = (Vec<String>, Vec<Vec<Option<String>>>);

/// The last result set in `events`. `\gset` and `\gexec` both act on it and
/// both ignore everything a multi-statement query produced before it, as psql
/// does.
fn last_result(events: &[QueryEvent]) -> Option<ResultSet> {
    let mut result: Option<ResultSet> = None;
    for event in events {
        match event {
            QueryEvent::RowDescription(fields) => {
                result = Some((fields.iter().map(|f| f.name.clone()).collect(), Vec::new()));
            }
            QueryEvent::Row(row) => {
                if let Some((_, rows)) = result.as_mut() {
                    rows.push(row.clone());
                }
            }
            _ => {}
        }
    }
    result
}

/// `\gset [prefix]`: bind the single result row's columns to psql variables.
/// The wording of the two failure messages is psql's, verbatim and unprefixed
/// (`psql.out:262`); a NULL column *unsets* its variable rather than emptying it.
fn capture_gset(
    out: &mut String,
    events: &[QueryEvent],
    query: &str,
    prefix: &str,
    vars: &mut Variables,
) {
    // An error means there is no result to bind; psql prints it and stops.
    if events.iter().any(|e| matches!(e, QueryEvent::Error(_))) {
        render_events(out, events, query, &format::Printing::default());
        return;
    }
    let Some((names, rows)) = last_result(events) else {
        return;
    };
    match rows.len() {
        1 => {}
        0 => {
            out.push_str("no rows returned for \\gset\n");
            return;
        }
        _ => {
            out.push_str("more than one row returned for \\gset\n");
            return;
        }
    }
    for (name, value) in names.iter().zip(&rows[0]) {
        let name = format!("{prefix}{name}");
        if !is_valid_variable_name(&name) {
            out.push_str(&format!("invalid variable name: \"{name}\"\n"));
            continue;
        }
        match value {
            Some(value) => vars.set(&name, value.clone()),
            None => vars.unset(&name),
        }
    }
}

/// psql's `VALID_VARIABLE_CHARS` applied to a whole name, which is what rejects
/// a `\gset` target built from a column alias containing a space.
fn is_valid_variable_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The queries `\gexec` generates: every non-NULL cell of the last result set,
/// row-major.
fn gexec_queries(events: &[QueryEvent]) -> Vec<String> {
    let Some((_, rows)) = last_result(events) else {
        return Vec::new();
    };
    rows.iter().flatten().flatten().cloned().collect()
}

/// The relation `\d <name>` describes, or `None` for any other metacommand —
/// including `\d` with no argument (which lists relations), `\d+`, and a `\d`
/// with several arguments. The lexer has already stripped anything chained on
/// after a `\`, so only the whitespace guard is left to enforce here.
fn describe_pattern<'a>(name: &str, arguments: &'a str) -> Option<&'a str> {
    let arguments = arguments.trim();
    if name != "d" || arguments.is_empty() {
        return None;
    }
    (!arguments.contains(char::is_whitespace)).then_some(arguments)
}

/// Run one backslash command, appending whatever psql would print. The four
/// implemented commands (`\set`, `\unset`, `\getenv`, `\pset null`) print
/// nothing on success; everything else gets the "not supported" stub.
///
/// Chaining is the lexer's job: `\set x y \\ -- note` (`regproc.sql:108`)
/// arrives here as a `\set` and then a separate, argument-less `\\`.
fn run_metacommand(
    name: &str,
    arguments: &str,
    environment: &BTreeMap<String, String>,
    vars: &mut Variables,
    printing: &mut format::Printing,
    out: &mut String,
) {
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
        // `\\` is a bare separator between commands: it does nothing and
        // prints nothing.
        ("\\", _) => {}
        // The output-format commands all print nothing: not one status line
        // ("Tuples only is on.", "Output format is …") appears in the whole
        // vendored corpus. An option or value that is not implemented keeps
        // the stub rather than silently rendering the wrong shape.
        ("pset", [option, values @ ..]) => {
            if !set_print_option(option, values.first().copied(), printing) {
                out.push_str(&format::metacommand_stub(name));
            }
        }
        // `\pset` with no argument lists every setting; `\x`, `\a` and `\t`
        // with none toggle.
        ("x", []) => printing.expanded = !printing.expanded,
        ("x", [value, ..]) => match parse_bool(value) {
            Some(on) => printing.expanded = on,
            None => out.push_str(&format::metacommand_stub(name)),
        },
        ("a", _) => printing.aligned = !printing.aligned,
        ("t", []) => printing.tuples_only = !printing.tuples_only,
        ("t", [value, ..]) => match parse_bool(value) {
            Some(on) => printing.tuples_only = on,
            None => out.push_str(&format::metacommand_stub(name)),
        },
        // `\echo` joins its arguments with single spaces. An *unquoted* leading
        // `-n` is the suppress-newline flag; `'-n'` is a literal argument
        // (`psql.out:4547`). Leaving the line open is what makes the next
        // echoed input line run on, as psql does (`psql.out:4546`).
        ("echo" | "qecho" | "warn", _) => {
            let suppress = args.first() == Some(&"-n") && !parsed.quoted[0];
            let args = if suppress { &args[1..] } else { &args[..] };
            out.push_str(&args.join(" "));
            if !suppress {
                out.push('\n');
            }
        }
        _ => out.push_str(&format::metacommand_stub(name)),
    }
}

/// Apply one `\pset` option, or report that the runner does not implement it.
/// A value-less `\pset <option>` queries the setting, which `-q` silences and
/// which notably does *not* reset it (`psql.out:480`).
fn set_print_option(option: &str, value: Option<&str>, printing: &mut format::Printing) -> bool {
    let Some(value) = value else {
        return true;
    };
    match option {
        "null" => printing.null_display = value.to_string(),
        "expanded" => match parse_bool(value) {
            Some(on) => printing.expanded = on,
            None => return false,
        },
        "tuples_only" => match parse_bool(value) {
            Some(on) => printing.tuples_only = on,
            None => return false,
        },
        "format" => match value {
            "aligned" | "a" => printing.aligned = true,
            "unaligned" | "u" => printing.aligned = false,
            // wrapped, csv, html, latex, troff-ms: not implemented, and
            // rendering them as `aligned` would be a silent lie.
            _ => return false,
        },
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a script's worth of metacommands, returning the printed output, the
    /// resulting NULL marker and the variables.
    fn run_all(commands: &[&str]) -> (String, String, Variables) {
        let environment = BTreeMap::from([("PG_ABS_SRCDIR".to_string(), "/src".to_string())]);
        let mut vars = Variables::new();
        let mut printing = format::Printing::default();
        let mut out = String::new();
        for command in commands {
            let chars: Vec<char> = command.chars().collect();
            let (name, name_end) = crate::script::command_name_at(&chars, 0);
            let arguments: String = chars[name_end..].iter().collect();
            run_metacommand(
                &name,
                &arguments,
                &environment,
                &mut vars,
                &mut printing,
                &mut out,
            );
        }
        (out, printing.null_display, vars)
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
        assert_eq!(describe_pattern("d", " bit_defaults"), Some("bit_defaults"));
        assert_eq!(describe_pattern("d", ""), None);
        assert_eq!(describe_pattern("d+", " t"), None);
        assert_eq!(describe_pattern("dt", ""), None);
        assert_eq!(describe_pattern("d", " a b"), None);
    }

    #[test]
    fn unimplemented_metacommands_still_stub() {
        // `\pset format html` is a *supported command* with a value the runner
        // cannot render, so it stubs rather than silently printing an aligned
        // table and claiming it is HTML.
        let (out, _, _) = run_all(&["pset format html", "d crabs", "psetnull x"]);
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

    /// psql's `ParseVariableBool`, including the `o` prefix that is ambiguous
    /// between `on` and `off`.
    #[test]
    fn boolean_spellings_match_psql() {
        for value in ["true", "TRUE", "t", "yes", "Y", "on", "1", "42", "-1"] {
            assert_eq!(parse_bool(value), Some(true), "for {value}");
        }
        for value in ["false", "f", "no", "off", "0"] {
            assert_eq!(parse_bool(value), Some(false), "for {value}");
        }
        for value in ["", "o", "maybe", ":skip_test", "invalid boolean expression"] {
            assert_eq!(parse_bool(value), None, "for {value}");
        }
    }

    /// Drive a script's worth of `\if`-family commands, returning the printed
    /// output and whether each step left the branch active.
    fn run_conditionals(commands: &[&str]) -> (String, Vec<bool>) {
        let mut branches = Vec::new();
        let mut out = String::new();
        let mut active = Vec::new();
        for command in commands {
            let mut words = command.split_whitespace();
            let name = words.next().expect("a command name");
            let args: Vec<&str> = words.collect();
            run_conditional(name, &args, &mut branches, &mut out);
            active.push(branches_active(&branches));
        }
        (out, active)
    }

    /// `psql.out:4655` — false, then a true `\elif`, then an `\else` that must
    /// not fire because an arm already ran.
    #[test]
    fn false_then_true_elif_wins_and_else_is_skipped() {
        let (out, active) = run_conditionals(&["if false", "elif true", "else", "endif"]);
        assert_eq!(out, "");
        assert_eq!(active, [false, true, false, true]);
    }

    /// An `\if` nested inside an inactive branch never evaluates its condition
    /// and stays inactive through its own `\else` (`psql.out:4694`).
    #[test]
    fn nested_block_inside_a_false_branch_stays_inactive() {
        let (out, active) =
            run_conditionals(&["if false", "if true", "else", "endif", "else", "endif"]);
        assert_eq!(out, "");
        assert_eq!(active, [false, false, false, false, true, true]);
    }

    /// An unparsable expression is reported and taken as false, so the `\else`
    /// arm runs (`psql.out:4666`).
    #[test]
    fn invalid_boolean_expression_is_reported_and_false() {
        let (out, active) = run_conditionals(&["if invalid boolean expression", "else", "endif"]);
        assert_eq!(
            out,
            "unrecognized value \"invalid boolean expression\" for \"\\if expression\": \
             Boolean expected\n"
        );
        assert_eq!(active, [false, true, true]);
    }

    #[test]
    fn unmatched_and_double_else_report_psqls_wording() {
        let (out, _) = run_conditionals(&["endif", "else", "elif"]);
        assert_eq!(
            out,
            "\\endif: no matching \\if\n\\else: no matching \\if\n\\elif: no matching \\if\n"
        );

        let (out, _) = run_conditionals(&["if true", "else", "else", "endif"]);
        assert_eq!(out, "\\else: cannot occur after \\else\n");

        let (out, _) = run_conditionals(&["if false", "else", "elif", "endif"]);
        assert_eq!(out, "\\elif: cannot occur after \\else\n");
    }

    fn result_events(names: &[&str], rows: &[&[Option<&str>]]) -> Vec<QueryEvent> {
        let mut events = vec![QueryEvent::RowDescription(
            names
                .iter()
                .map(|name| Field {
                    name: (*name).to_string(),
                    type_oid: 25,
                })
                .collect(),
        )];
        for row in rows {
            events.push(QueryEvent::Row(
                row.iter().map(|v| v.map(str::to_string)).collect(),
            ));
        }
        events.push(QueryEvent::CommandComplete("SELECT".to_string()));
        events
    }

    #[track_caller]
    fn gset(events: &[QueryEvent], prefix: &str) -> (String, Variables) {
        let mut out = String::new();
        let mut vars = Variables::new();
        capture_gset(&mut out, events, "SELECT 1", prefix, &mut vars);
        (out, vars)
    }

    #[test]
    fn gset_binds_one_row_and_unsets_nulls() {
        // psql.out:262 — a NULL column removes the variable rather than
        // emptying it.
        let events = result_events(&["var1", "var2", "var3"], &[&[Some("1"), None, Some("3")]]);
        let (out, vars) = gset(&events, "");
        assert_eq!(out, "");
        assert_eq!(vars.get("var1"), Some("1"));
        assert_eq!(vars.get("var2"), None);
        assert_eq!(vars.get("var3"), Some("3"));

        let (_, vars) = gset(&events, "pre_");
        assert_eq!(vars.get("pre_var1"), Some("1"));
    }

    #[test]
    fn gset_reports_the_wrong_row_count_verbatim() {
        let (out, _) = gset(&result_events(&["a"], &[]), "");
        assert_eq!(out, "no rows returned for \\gset\n");

        let (out, _) = gset(&result_events(&["a"], &[&[Some("1")], &[Some("2")]]), "");
        assert_eq!(out, "more than one row returned for \\gset\n");
    }

    #[test]
    fn gset_rejects_a_column_that_is_not_a_variable_name() {
        // psql.out:238 — `SELECT 1 AS "bad name" \gset`.
        let (out, vars) = gset(&result_events(&["bad name"], &[&[Some("1")]]), "");
        assert_eq!(out, "invalid variable name: \"bad name\"\n");
        assert_eq!(vars.get("bad name"), None);
    }

    #[test]
    fn gexec_generates_every_non_null_cell_row_major() {
        let events = result_events(
            &["a", "b"],
            &[
                &[Some("SELECT 1"), None],
                &[Some("SELECT 2"), Some("SELECT 3")],
            ],
        );
        assert_eq!(gexec_queries(&events), ["SELECT 1", "SELECT 2", "SELECT 3"]);
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
