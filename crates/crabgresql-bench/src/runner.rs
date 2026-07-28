//! Orchestration: bring up a target, load the dataset once, time every query.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::net::TcpListener;
use tokio_postgres::{Client, SimpleQueryMessage};

use crate::client;
use crate::report::{Outcome, QueryRun, SuiteRun, TableRows};
use crate::suite::Suite;

pub struct RunConfig {
    /// Raw data to load: the data file for a single-table suite, the directory
    /// holding the per-table files for a multi-table one. Without it the
    /// tables must already exist.
    pub data: Option<PathBuf>,
    /// Load at most this many rows *per table* — how you run a 1M-row slice of
    /// a 100M-row benchmark.
    pub rows: Option<u64>,
    /// Data directory for the in-process server; a temporary one when unset,
    /// so a persistent path is what lets a load be reused by the next run.
    pub data_dir: Option<PathBuf>,
    /// Benchmark an external server instead (a libpq connection string), for
    /// side-by-side numbers against stock PostgreSQL.
    pub url: Option<String>,
    /// Timed repetitions per query. Run 1 is cold, the rest are warm.
    pub runs: u32,
    /// Query numbers to run; all of them when empty.
    pub only: Vec<usize>,
    /// Per-run wall-clock limit. Exceeding it abandons the connection, since
    /// the server has no query cancellation yet.
    pub timeout: Duration,
    /// `USING <am>` for the created table (`parquet`, `buffer`, …).
    pub access_method: Option<String>,
    /// Drop and reload even if the table is already there.
    pub reload: bool,
}

/// The server under test, kept alive for the length of the run.
struct Target {
    conninfo: String,
    description: String,
    _data_dir: Option<tempfile::TempDir>,
    server: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl Drop for Target {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
        }
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
        bail!(
            "no queries selected: {} has queries 1..={}",
            suite.name,
            suite.queries().len(),
        );
    }

    let target = start_target(config).await?;
    let mut client = client::connect(&target.conninfo).await?;
    let loaded = load_if_needed(&client, suite, config).await?;

    let mut results = Vec::with_capacity(queries.len());
    for query in queries {
        let mut runs = Vec::with_capacity(config.runs as usize);
        for _ in 0..config.runs {
            let outcome = time_query(&client, &query.sql, config.timeout).await;
            // A timeout leaves the abandoned query running server-side, and a
            // lost connection leaves the client permanently unusable — both
            // need a fresh connection, or every later query fails too and the
            // report reads as a wall of engine gaps that never happened.
            let broken = matches!(outcome, Outcome::TimedOut | Outcome::Disconnected(_));
            runs.push(outcome);
            if broken {
                match client::connect(&target.conninfo).await {
                    Ok(fresh) => client = fresh,
                    // Keep what has already been measured rather than throwing
                    // the whole run (and its load) away.
                    Err(e) => {
                        eprintln!("bench: cannot reconnect, reporting partial results: {e:#}");
                        results.push(QueryRun {
                            number: query.number,
                            runs,
                        });
                        return Ok(report(suite, &target, &loaded, results));
                    }
                }
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

    Ok(report(suite, &target, &loaded, results))
}

fn report(suite: &Suite, target: &Target, loaded: &Loaded, queries: Vec<QueryRun>) -> SuiteRun {
    SuiteRun {
        suite: suite.name.to_string(),
        target: target.description.clone(),
        access_method: loaded.access_method.clone(),
        tables: loaded.tables.clone(),
        load_time: loaded.time,
        queries,
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
    let mut present = Vec::new();
    let mut missing = Vec::new();
    for table in suite.tables {
        if client::table_exists(client, table).await? {
            present.push(*table);
        } else {
            missing.push(*table);
        }
    }

    // A dataset that is only half there is not a dataset. Rebuilding all of it
    // is the only honest answer, and doing it silently would throw away
    // whatever the earlier run had loaded, so ask.
    if !present.is_empty() && !missing.is_empty() && !config.reload {
        bail!(
            "{} is partly loaded: {} exist but {} do not; \
             pass --reload to rebuild the dataset",
            suite.name,
            present.join(", "),
            missing.join(", "),
        );
    }

    if missing.is_empty() && !config.reload {
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
        let mut tables = Vec::with_capacity(suite.tables.len());
        for table in suite.tables {
            tables.push(TableRows {
                name: (*table).to_string(),
                rows: client::count_rows(client, table).await?,
            });
        }
        return Ok(Loaded {
            tables,
            time: None,
            access_method: None,
        });
    }

    let Some(data) = &config.data else {
        bail!("{}", no_data_hint(suite));
    };
    let sources = data_files(suite, data)?;

    // Drop in reverse of load order, so a suite whose DDL ever grows foreign
    // keys drops referencing tables before the tables they reference.
    for table in suite.tables.iter().rev() {
        if present.contains(table) {
            client::execute(client, &format!("DROP TABLE {table}")).await?;
        }
    }
    for statement in suite.schema_statements(config.access_method.as_deref())? {
        client::execute(client, &statement)
            .await
            .context("creating the benchmark tables")?;
    }

    let started = Instant::now();
    let mut tables = Vec::with_capacity(suite.tables.len());
    for (table, source) in suite.tables.iter().zip(&sources) {
        eprintln!("bench: loading {} …", source.display());
        let sent = client::copy_file_in(
            client,
            &suite.copy_statement(table),
            source,
            config.rows,
            suite.has_header(),
        )
        .await
        .with_context(|| format!("loading `{table}` from {}", source.display()))?;

        // Trust the table, not the loader: `COPY` runs in batches, so a load
        // that died part way would otherwise be indistinguishable from a whole
        // one.
        let rows = client::count_rows(client, table).await?;
        if rows != sent {
            bail!("loaded {sent} rows but `{table}` holds {rows}");
        }
        tables.push(TableRows {
            name: (*table).to_string(),
            rows,
        });
    }
    let elapsed = started.elapsed();

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
/// given the directory the per-table files live in, named `<table>.<ext>` —
/// with `.csv` accepted as well as the format's own extension, because that is
/// what a generator's bulk export tends to emit.
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
            let candidates = [
                data.join(format!("{table}.{}", suite.format.extension())),
                data.join(format!("{table}.csv")),
            ];
            candidates
                .iter()
                .find(|path| path.exists())
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no data file for `{table}`: tried {}\n{}",
                        candidates
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(" and "),
                        no_data_hint(suite),
                    )
                })
        })
        .collect()
}

/// What to tell someone who has no data yet.
fn no_data_hint(suite: &Suite) -> String {
    format!(
        "{}'s tables do not exist and no usable --data was given.\n\
         Get the dataset first: {}",
        suite.name, suite.dataset_url,
    )
}

/// Either connect to the external server named by `--url`, or boot one in this
/// process over the durable heap engine, exactly as `crabgresql-server` does.
async fn start_target(config: &RunConfig) -> Result<Target> {
    if let Some(url) = &config.url {
        return Ok(Target {
            conninfo: url.clone(),
            description: format!("external server ({url})"),
            _data_dir: None,
            server: None,
        });
    }

    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
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
    let (engine, txnmgr) = crabgresql_server::open_pg_engine(&path)
        .with_context(|| format!("opening the engine over {}", path.display()))?;
    let server = tokio::spawn(crabgresql_server::serve_with(listener, engine, txnmgr));

    Ok(Target {
        conninfo: format!("host=127.0.0.1 port={port} user=postgres dbname=bench"),
        description: format!("in-process CrabgreSQL ({})", path.display()),
        _data_dir: temp_dir,
        server: Some(server),
    })
}
