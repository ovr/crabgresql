//! Orchestration: bring up a target, load the dataset once, time every query.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crabgresql_server_process::{ServerProcess, locate_server_binary};
use crabgresql_storage_api::TableAccessMethod;
use tokio_postgres::{Client, SimpleQueryMessage};

use crate::client;
use crate::report::{Outcome, QueryRun, SuiteRun, TableRows};
use crate::suite::Suite;

pub struct RunConfig {
    /// Raw data to load: the data file for a single-table suite, the directory
    /// holding the per-table files for a multi-table one. Without it the
    /// tables must already exist.
    pub data: Option<PathBuf>,
    /// Load at most this many rows — how you run a 1M-row slice of a 100M-row
    /// benchmark. Single-table suites only: slicing each table of a joined
    /// schema independently leaves its keys dangling.
    pub rows: Option<u64>,
    /// Data directory for the server under test; a temporary one when unset,
    /// so a persistent path is what lets a load be reused by the next run.
    pub data_dir: Option<PathBuf>,
    /// The `crabgresql` binary to benchmark. Unset takes the one built next to
    /// this executable; ignored with `url`.
    pub server_bin: Option<PathBuf>,
    /// Benchmark an external server instead (a libpq connection string), for
    /// side-by-side numbers against stock PostgreSQL.
    pub url: Option<String>,
    /// Timed repetitions per query. Run 1 is cold, the rest are warm.
    pub runs: u32,
    /// Query numbers to run; all of them when empty.
    pub only: Vec<usize>,
    /// Per-run wall-clock limit. Exceeding it abandons the connection, leaving
    /// the query running server-side.
    ///
    /// TODO: cancel a timed-out query instead of abandoning it — the server
    /// hands out BackendKeyData but does not act on a protocol cancel request.
    pub timeout: Duration,
    /// `USING <am>` for the created tables (`parquet`, `buffer`, …).
    pub access_method: Option<String>,
    /// Drop and reload every table, even if the dataset is already loaded.
    pub reload: bool,
}

/// The server under test, kept alive for the length of the run. The child is
/// killed when this is dropped, whichever way the run ended.
///
/// Field order is load-bearing: fields drop in declaration order, so the server
/// has to be listed before the temporary data directory. The other way round
/// unlinks the tree while the flush worker is still writing to it, which leaves
/// a re-created directory behind in `/tmp`.
struct Target {
    server: Option<ServerProcess>,
    _data_dir: Option<tempfile::TempDir>,
    conninfo: String,
    description: String,
}

/// How long a lost connection is given to become an exit status: a dying server
/// closes its sockets before the kernel is done with it, so the client notices
/// first. Long enough for the SIGCHLD hop, short enough not to be felt.
const EXIT_GRACE: Duration = Duration::from_millis(500);

impl Target {
    /// The reason the server is gone, or `None` if it is still running (always,
    /// for an external `--url` target — that one is not ours to diagnose).
    ///
    /// `lost_connection` says whether the client already noticed, which is what
    /// makes waiting for the status worth it.
    async fn server_died(&mut self, lost_connection: bool) -> Option<String> {
        let server = self.server.as_mut()?;
        let status = match lost_connection {
            true => server.exited_within(EXIT_GRACE).await,
            false => server.exited(),
        };
        let status = status.ok().flatten()?;
        Some(format!(
            "bench: the server exited with {status}; see {}\n{}",
            server.log_path().display(),
            server.log_tail(),
        ))
    }
}

/// What the load phase established about the dataset under test.
struct Loaded {
    tables: Vec<TableRows>,
    /// `None` when this run reused a dataset an earlier run had loaded.
    time: Option<Duration>,
    /// The access method actually in force, which is only known to be the
    /// requested one when this run created the tables.
    access_method: Option<String>,
}

pub async fn run(suite: &Suite, config: &RunConfig) -> Result<SuiteRun> {
    // Select the queries before doing anything expensive: a typo in --query
    // should fail in milliseconds, not after a multi-hour load.
    let queries: Vec<_> = suite
        .queries()
        .into_iter()
        .filter(|q| config.only.is_empty() || config.only.contains(&q.number))
        .collect();
    if queries.is_empty() {
        // Report the highest *number*, not the count: `Numbered` suites take
        // their numbers from markers, so the two need not agree.
        let highest = suite
            .queries()
            .iter()
            .map(|q| q.number)
            .max()
            .unwrap_or_default();
        bail!(
            "no queries selected: {} has queries 1..={highest}",
            suite.name,
        );
    }

    let mut target = start_target(config).await?;
    let mut client = client::connect(&target.conninfo).await?;
    let loaded = load_if_needed(&client, suite, config).await?;

    let total_queries = queries.len();
    let mut results = Vec::with_capacity(total_queries);
    for query in queries {
        let mut runs = Vec::with_capacity(config.runs as usize);
        for _ in 0..config.runs {
            // Setup and teardown are the query's, but not the measurement's:
            // Q15's view has to exist before the SELECT and be gone after it,
            // and neither the DDL's cost nor its cleanup is what we time.
            if !query.setup.is_empty()
                && let Err(e) = client::execute(&client, &query.setup).await
            {
                eprintln!("bench: q{} setup failed: {e:#}", query.number);
            }
            let outcome = time_query(&client, &query.sql, config.timeout).await;
            // A timeout leaves the abandoned query running server-side, and a
            // lost connection leaves the client permanently unusable — both
            // need a fresh connection, or every later query fails too and the
            // report reads as a wall of engine gaps that never happened.
            let broken = matches!(outcome, Outcome::TimedOut | Outcome::Disconnected(_));
            if broken {
                // Say so now: the reconnect below can take a while when the
                // abandoned query is still monopolizing the server, and a
                // silent stall is indistinguishable from a hang.
                eprintln!("  q{:<3} {:>9}  {}", query.number, "-", outcome.summary());
            }
            let lost_connection = matches!(outcome, Outcome::Disconnected(_));
            runs.push(outcome);
            // A dead server makes every later query meaningless, and reconnecting
            // to it would spend the timeout to say so once per query. Report what
            // was measured, with the reason and the server's own log.
            if broken && let Some(reason) = target.server_died(lost_connection).await {
                eprintln!("{reason}");
                results.push(QueryRun {
                    number: query.number,
                    runs,
                });
                let reason = format!(
                    "{reason}\n{} of {total_queries} queries did not run.",
                    total_queries - results.len(),
                );
                return Ok(report(suite, &target, &loaded, results, Some(reason)));
            }
            if broken {
                match reconnect(&target.conninfo, config.timeout).await {
                    Ok(fresh) => client = fresh,
                    // Keep what has already been measured rather than throwing
                    // the whole run (and its load) away.
                    Err(e) => {
                        eprintln!("bench: cannot reconnect, reporting partial results: {e:#}");
                        results.push(QueryRun {
                            number: query.number,
                            runs,
                        });
                        return Ok(report(suite, &target, &loaded, results, None));
                    }
                }
            }
            // Always, even after a failure — a leaked object turns every later
            // run of this query into a bogus "already exists".
            if !query.teardown.is_empty()
                && let Err(e) = client::execute(&client, &query.teardown).await
            {
                eprintln!("bench: q{} teardown failed: {e:#}", query.number);
            }
        }
        let result = QueryRun {
            number: query.number,
            runs,
        };
        eprintln!(
            "  q{:<3} {:>9}  {}",
            result.number,
            match result.best() {
                Some(best) => format!("{best:.3}s"),
                None => "-".to_string(),
            },
            result.status(),
        );
        results.push(result);
    }

    Ok(report(suite, &target, &loaded, results, None))
}

fn report(
    suite: &Suite,
    target: &Target,
    loaded: &Loaded,
    queries: Vec<QueryRun>,
    crash: Option<String>,
) -> SuiteRun {
    SuiteRun {
        suite: suite.name.to_string(),
        target: target.description.clone(),
        access_method: loaded.access_method.clone(),
        tables: loaded.tables.clone(),
        load_time: loaded.time,
        queries,
        crash,
    }
}

/// Open a fresh connection, giving up after `timeout`.
///
/// The bound matters: the server has no query cancellation, so the query we
/// just abandoned is still burning CPU and may not accept a new session at all.
/// Waiting forever would hang the whole run with no output.
async fn reconnect(conninfo: &str, timeout: Duration) -> Result<Client> {
    match tokio::time::timeout(timeout, client::connect(conninfo)).await {
        Ok(result) => result,
        Err(_) => bail!(
            "no new connection after {:.0}s; the abandoned query is still \
             holding the server",
            timeout.as_secs_f64(),
        ),
    }
}

/// Run one query to completion and time it, including receiving every row.
async fn time_query(client: &Client, sql: &str, timeout: Duration) -> Outcome {
    let started = Instant::now();
    match tokio::time::timeout(timeout, client.simple_query(sql)).await {
        Ok(Ok(messages)) => Outcome::Ok {
            elapsed: started.elapsed(),
            rows: messages
                .iter()
                .filter(|m| matches!(m, SimpleQueryMessage::Row(_)))
                .count(),
        },
        Ok(Err(e)) => match e.as_db_error() {
            Some(db) => Outcome::Failed(format!("{}: {}", db.severity(), db.message())),
            // Not a server error: the connection itself is gone.
            None => Outcome::Disconnected(format!("connection lost: {e}")),
        },
        Err(_) => Outcome::TimedOut,
    }
}

/// Create the suite's tables and stream the dataset in, unless it is already
/// loaded.
async fn load_if_needed(client: &Client, suite: &Suite, config: &RunConfig) -> Result<Loaded> {
    // --rows slices each table independently, which is a valid sample of one
    // denormalized table and a broken database for anything with keys: the
    // first N orders reference customers that the first N customers do not
    // contain, so the joins match almost nothing and every query still passes.
    if config.rows.is_some() && suite.is_multi_table() {
        bail!(
            "--rows cannot slice {}: truncating each of its {} tables \
             independently leaves the keys dangling, so the queries would \
             measure a database that is not {0}. Regenerate the data at a \
             smaller scale factor instead.",
            suite.name,
            suite.tables.len(),
        );
    }
    // Refuse an unknown access method here, before the DROP loop below empties a
    // persistent --data-dir. The server would catch it too, but only after the
    // existing tables are already gone, so a typo would cost the dataset.
    if let Some(am) = &config.access_method
        && TableAccessMethod::from_name(am).is_none()
    {
        bail!("unknown access method `{am}`: expected heap, parquet, or buffer");
    }

    // Probe rows, not mere existence. The tables are all created before any
    // COPY runs, so an interrupted load leaves every one of them present and
    // an existence check would wave the empty ones through.
    let mut loaded = Vec::with_capacity(suite.tables.len());
    for table in suite.tables {
        let rows = match client::table_exists(client, table).await? {
            true => Some(client::count_rows(client, table).await?),
            false => None,
        };
        loaded.push((*table, rows));
    }
    let present: Vec<&str> = loaded
        .iter()
        .filter(|(_, rows)| rows.is_some())
        .map(|(table, _)| *table)
        .collect();
    let unusable: Vec<&str> = loaded
        .iter()
        .filter(|(_, rows)| rows.unwrap_or(0) == 0)
        .map(|(table, _)| *table)
        .collect();

    // A dataset that is only half there is not a dataset. Rebuilding all of it
    // is the only honest answer, and doing it silently would throw away
    // whatever the earlier run had loaded, so ask.
    if !unusable.is_empty() && unusable.len() < suite.tables.len() && !config.reload {
        bail!(
            "{} is partly loaded: {} {} missing or empty; \
             pass --reload to rebuild the dataset",
            suite.name,
            unusable.join(", "),
            if unusable.len() == 1 { "is" } else { "are" },
        );
    }

    if unusable.is_empty() && !config.reload {
        // The tables are whatever an earlier run left behind. Their access
        // method and row counts are not this run's to claim, so refuse the
        // options that would otherwise be silently ignored, and report the
        // storage as unknown rather than as the one that was asked for.
        if let Some(am) = &config.access_method {
            bail!(
                "{}'s tables already exist, so they cannot be created `USING {am}`; \
                 pass --reload to rebuild them",
                suite.name,
            );
        }
        if config.rows.is_some() {
            bail!(
                "{}'s tables already exist, so --rows would be ignored; \
                 pass --reload to rebuild them",
                suite.name,
            );
        }
        if config.data.is_some() {
            eprintln!(
                "bench: {}'s tables already exist, skipping load \
                 (pass --reload to rebuild them)",
                suite.name,
            );
        }
        return Ok(Loaded {
            tables: loaded
                .iter()
                .map(|(table, rows)| TableRows {
                    name: (*table).to_string(),
                    rows: rows.unwrap_or(0),
                })
                .collect(),
            time: None,
            access_method: None,
        });
    }

    let Some(data) = &config.data else {
        bail!(
            "{}'s tables are not loaded and no --data was given.\n{}",
            suite.name,
            no_data_hint(suite),
        );
    };
    let sources = data_files(suite, data)?;

    // Name what is about to go. These are ordinary words — `orders`,
    // `customer`, `lineitem` — and with --url they are dropped from whatever
    // database the connection string points at.
    if !present.is_empty() {
        eprintln!("bench: dropping {}", present.join(", "));
    }
    // Drop in reverse of load order, so a suite whose DDL ever grows foreign
    // keys drops referencing tables before the tables they reference. CASCADE
    // because a query's own setup may have left a view behind (TPC-H Q15), and
    // a rebuild that cannot proceed would strand the data directory.
    for table in suite.tables.iter().rev() {
        if present.contains(table) {
            client::execute(client, &format!("DROP TABLE {table} CASCADE")).await?;
        }
    }
    for statement in suite.schema_statements(config.access_method.as_deref())? {
        client::execute(client, &statement)
            .await
            .context("creating the benchmark tables")?;
    }

    let started = Instant::now();
    let mut sent = Vec::with_capacity(suite.tables.len());
    for (table, source) in suite.tables.iter().zip(&sources) {
        eprintln!("bench: loading {} …", source.display());
        sent.push(
            client::copy_file_in(
                client,
                &suite.copy_statement(table),
                source,
                config.rows,
                suite.has_header(),
                suite.format.trailing_delimiter(),
            )
            .await
            .with_context(|| format!("loading `{table}` from {}", source.display()))?,
        );
    }
    // Stop the clock before verifying: the count(*) below is a full scan of
    // every table, and folding it into the number we publish as loader
    // throughput would understate the loader by however long the scans take.
    let elapsed = started.elapsed();

    // Trust the table, not the loader: `COPY` runs in batches, so a load that
    // died part way would otherwise be indistinguishable from a whole one.
    let mut tables = Vec::with_capacity(suite.tables.len());
    for (table, sent) in suite.tables.iter().zip(sent) {
        let rows = client::count_rows(client, table).await?;
        if rows != sent {
            bail!("loaded {sent} rows but `{table}` holds {rows}");
        }
        tables.push(TableRows {
            name: (*table).to_string(),
            rows,
        });
    }

    let total: u64 = tables.iter().map(|t| t.rows).sum();
    eprintln!(
        "bench: loaded {total} rows in {:.1}s",
        elapsed.as_secs_f64()
    );
    Ok(Loaded {
        tables,
        time: Some(elapsed),
        // This run created the tables, so the storage is known — name the
        // default explicitly rather than leaving it to be inferred.
        access_method: Some(
            config
                .access_method
                .clone()
                .unwrap_or_else(|| "heap".to_string()),
        ),
    })
}

/// The raw file backing each of the suite's tables, in load order.
///
/// A single-table suite is pointed straight at its file. A multi-table one is
/// given the directory the per-table files live in, each named
/// `<table>.<ext>` for the suite format's own extension. Only that extension
/// is accepted: the `COPY` options come from the format, so a file named for a
/// different one would be read with the wrong delimiter and its header, if any,
/// taken as data.
fn data_files(suite: &Suite, data: &Path) -> Result<Vec<PathBuf>> {
    if !suite.is_multi_table() {
        return Ok(vec![data.to_path_buf()]);
    }
    if !data.is_dir() {
        bail!(
            "{} loads {} tables, so --data must be the directory holding their \
             data files, but {} is not a directory",
            suite.name,
            suite.tables.len(),
            data.display(),
        );
    }
    suite
        .tables
        .iter()
        .map(|table| {
            let path = data.join(format!("{table}.{}", suite.format.extension()));
            if path.exists() {
                return Ok(path);
            }
            bail!(
                "no data file for `{table}`: {} does not exist.\n{}",
                path.display(),
                no_data_hint(suite),
            )
        })
        .collect()
}

/// What to tell someone who has no data yet.
fn no_data_hint(suite: &Suite) -> String {
    format!("Get the dataset first:\n{}", suite.dataset_hint)
}

/// Either connect to the external server named by `--url`, or start the
/// `crabgresql` binary as a child process over the durable heap engine.
///
/// A child process is what a benchmark should measure: the same binary a user
/// runs, and a load that OOMs or panics it no longer takes the harness holding
/// the results down with it. Its log goes to `<data-dir>/server.log`.
async fn start_target(config: &RunConfig) -> Result<Target> {
    if let Some(url) = &config.url {
        return Ok(Target {
            server: None,
            _data_dir: None,
            conninfo: url.clone(),
            description: format!("external server ({url})"),
        });
    }

    let (temp_dir, path) = match &config.data_dir {
        Some(path) => {
            std::fs::create_dir_all(path)
                .with_context(|| format!("creating {}", path.display()))?;
            (None, path.clone())
        }
        None => {
            let dir = tempfile::tempdir()?;
            let path = dir.path().to_path_buf();
            (Some(dir), path)
        }
    };
    let binary = match &config.server_bin {
        Some(path) => path.clone(),
        None => locate_server_binary()?,
    };
    // The dataset is streamed in through `COPY … FROM STDIN`, so the server
    // needs no read access outside its own data directory.
    let server = ServerProcess::start(&binary, &path, &[], &path.join("server.log"))
        .await
        .with_context(|| format!("starting {} over {}", binary.display(), path.display()))?;

    Ok(Target {
        conninfo: format!(
            "host=127.0.0.1 port={} user=postgres dbname=bench",
            server.port()
        ),
        server: Some(server),
        _data_dir: temp_dir,
        description: format!("CrabgreSQL ({})", path.display()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::{DataFormat, QueryFormat};

    const MULTI: Suite = Suite {
        name: "multi",
        description: "",
        tables: &["region", "nation"],
        sort_keys: &["id", "id"],
        schema_sql: "CREATE TABLE region (id INT);\nCREATE TABLE nation (id INT);\n",
        queries_sql: "-- Q1\nselect 1;\n",
        queries_format: QueryFormat::Numbered,
        dataset_hint: "generate it",
        format: DataFormat::Psv,
    };

    const SINGLE: Suite = Suite {
        tables: &["hits"],
        schema_sql: "CREATE TABLE hits (id INT);\n",
        ..MULTI
    };

    #[test]
    fn a_single_table_suite_is_pointed_straight_at_its_file() -> Result<()> {
        let path = Path::new("/tmp/hits.tsv");
        assert_eq!(data_files(&SINGLE, path)?, vec![path.to_path_buf()]);
        Ok(())
    }

    #[test]
    fn a_multi_table_suite_reads_one_file_per_table_in_load_order() -> Result<()> {
        let dir = tempfile::tempdir()?;
        for table in MULTI.tables {
            std::fs::write(dir.path().join(format!("{table}.tbl")), "1\n")?;
        }
        assert_eq!(
            data_files(&MULTI, dir.path())?,
            vec![dir.path().join("region.tbl"), dir.path().join("nation.tbl")],
        );
        Ok(())
    }

    #[test]
    fn a_multi_table_suite_refuses_a_file_and_names_the_missing_table() -> Result<()> {
        let dir = tempfile::tempdir()?;
        // A file where a directory belongs.
        let file = dir.path().join("everything.tbl");
        std::fs::write(&file, "1\n")?;
        let err = data_files(&MULTI, &file)
            .expect_err("a file is not a directory")
            .to_string();
        assert!(err.contains("must be the directory"), "{err}");

        // A directory missing one table's file names that table, not the first.
        std::fs::write(dir.path().join("region.tbl"), "1\n")?;
        let err = data_files(&MULTI, dir.path())
            .expect_err("nation.tbl is missing")
            .to_string();
        assert!(err.contains("nation.tbl"), "{err}");
        assert!(err.contains("generate it"), "{err}");
        Ok(())
    }

    #[test]
    fn only_the_formats_own_extension_is_accepted() -> Result<()> {
        // A `.csv` would be read with the format's `|` delimiter and its header
        // taken as data, so it must not be silently picked up.
        let dir = tempfile::tempdir()?;
        for table in MULTI.tables {
            std::fs::write(dir.path().join(format!("{table}.csv")), "1\n")?;
        }
        assert!(data_files(&MULTI, dir.path()).is_err());
        Ok(())
    }
}
