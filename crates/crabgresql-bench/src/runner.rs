//! Orchestration: bring up a target, load the dataset once, time every query.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::net::TcpListener;
use tokio_postgres::{Client, SimpleQueryMessage};

use crate::client;
use crate::report::{Outcome, QueryRun, SuiteRun};
use crate::suite::Suite;

pub struct RunConfig {
    /// Raw data file to load. Without it the table must already exist.
    pub data: Option<PathBuf>,
    /// Load at most this many rows — how you run a 1M-row slice of a 100M-row
    /// benchmark.
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

pub async fn run(suite: &Suite, config: &RunConfig) -> Result<SuiteRun> {
    let target = start_target(config).await?;
    let mut client = client::connect(&target.conninfo).await?;

    let (loaded_rows, load_time) = load_if_needed(&client, suite, config).await?;

    let queries: Vec<_> = suite
        .queries()
        .into_iter()
        .filter(|q| config.only.is_empty() || config.only.contains(&q.number))
        .collect();
    if queries.is_empty() {
        bail!("no queries selected");
    }

    let mut results = Vec::with_capacity(queries.len());
    for query in queries {
        let mut runs = Vec::with_capacity(config.runs as usize);
        for _ in 0..config.runs {
            let outcome = time_query(&client, &query.sql, config.timeout).await;
            let broken = matches!(outcome, Outcome::TimedOut);
            runs.push(outcome);
            if broken {
                // The abandoned query is still running server-side; a fresh
                // connection at least keeps the remaining queries runnable.
                client = client::connect(&target.conninfo).await?;
            }
        }
        let result = QueryRun {
            number: query.number,
            sql: query.sql,
            runs,
        };
        eprintln!(
            "  q{:<3} {}",
            result.number,
            match (result.best(), result.failure()) {
                (Some(best), _) => format!("{best:.3}s"),
                (None, Some(failure)) => failure.lines().next().unwrap_or("failed").to_string(),
                (None, None) => "not run".to_string(),
            }
        );
        results.push(result);
    }

    Ok(SuiteRun {
        suite: suite.name.to_string(),
        target: target.description.clone(),
        loaded_rows,
        load_time,
        queries: results,
    })
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
        Ok(Err(e)) => Outcome::Failed(match e.as_db_error() {
            Some(db) => format!("{}: {}", db.severity(), db.message()),
            None => e.to_string(),
        }),
        Err(_) => Outcome::TimedOut,
    }
}

/// Create the table and stream the dataset in, unless it is already loaded.
async fn load_if_needed(
    client: &Client,
    suite: &Suite,
    config: &RunConfig,
) -> Result<(Option<u64>, Option<Duration>)> {
    let exists = client::table_exists(client, suite.table).await?;
    if exists && !config.reload {
        if config.data.is_some() {
            eprintln!(
                "bench: `{}` already exists, skipping load (pass --reload to rebuild it)",
                suite.table
            );
        }
        return Ok((None, None));
    }
    let Some(data) = &config.data else {
        bail!(
            "table `{}` does not exist and no --data was given.\n\
             Fetch the dataset first, e.g.\n  \
             curl -sSL {} | gzip -d > hits.tsv\n\
             then re-run with --data hits.tsv",
            suite.table,
            suite.dataset_url,
        );
    };

    if exists {
        client::execute(client, &format!("DROP TABLE {}", suite.table)).await?;
    }
    client::execute(client, &suite.schema(config.access_method.as_deref()))
        .await
        .context("creating the benchmark table")?;

    eprintln!("bench: loading {} …", data.display());
    let started = Instant::now();
    let rows = client::copy_file_in(client, &suite.copy_statement(), data, config.rows).await?;
    let elapsed = started.elapsed();
    eprintln!("bench: loaded {rows} rows in {:.1}s", elapsed.as_secs_f64());
    Ok((Some(rows), Some(elapsed)))
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
