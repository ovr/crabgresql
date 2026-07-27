//! Benchmark runner CLI.
//!
//! Exit codes: 0 — every selected query ran, 1 — at least one failed, 2 — bad
//! usage or an infrastructure error (missing dataset, I/O, server startup).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use crabgresql_bench::runner::{RunConfig, run};
use crabgresql_bench::suites;

/// Run analytical benchmarks against CrabgreSQL.
#[derive(Parser)]
#[command(name = "bench")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the available benchmark suites
    List,
    /// Run a benchmark suite
    Run {
        /// Suite name, e.g. `clickbench`
        suite: String,

        /// Raw data file to load (uncompressed); not needed once loaded into
        /// a persistent --data-dir
        #[arg(long, value_name = "PATH")]
        data: Option<PathBuf>,

        /// Load only the first N rows of --data
        #[arg(long, value_name = "N")]
        rows: Option<u64>,

        /// Data directory for the in-process server [default: a temp dir]
        #[arg(long, value_name = "DIR")]
        data_dir: Option<PathBuf>,

        /// Benchmark an external server instead (libpq connection string)
        #[arg(long, value_name = "CONNINFO")]
        url: Option<String>,

        /// Timed repetitions per query
        #[arg(long, value_name = "N", default_value_t = 3)]
        runs: u32,

        /// Run only these query numbers
        #[arg(long, value_delimiter = ',', value_name = "N,N")]
        query: Vec<usize>,

        /// Per-run timeout in seconds
        #[arg(long, value_name = "SECONDS", default_value_t = 120)]
        timeout: u64,

        /// Access method for the benchmark table (parquet, buffer, …)
        #[arg(long, value_name = "AM")]
        using: Option<String>,

        /// Drop and reload the table even if it is already populated
        #[arg(long)]
        reload: bool,

        /// Also write the results as JSON to this path
        #[arg(long, value_name = "PATH")]
        json: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let Command::Run {
        suite,
        data,
        rows,
        data_dir,
        url,
        runs,
        query,
        timeout,
        using,
        reload,
        json,
    } = args.command
    else {
        for suite in suites::ALL {
            println!("{:<12} {}", suite.name, suite.description);
        }
        return ExitCode::SUCCESS;
    };

    let Some(suite) = suites::find(&suite) else {
        eprintln!("bench: unknown suite `{suite}`; try `bench list`");
        return ExitCode::from(2);
    };
    if runs == 0 {
        eprintln!("bench: --runs must be at least 1");
        return ExitCode::from(2);
    }

    let config = RunConfig {
        data,
        rows,
        data_dir,
        url,
        runs,
        only: query,
        timeout: Duration::from_secs(timeout),
        access_method: using,
        reload,
    };
    let report = match run(suite, &config).await {
        Ok(report) => report,
        Err(e) => {
            eprintln!("bench: {e:#}");
            return ExitCode::from(2);
        }
    };

    print!("{}", report.table());
    if let Some(path) = json
        && let Err(e) = std::fs::write(&path, report.json())
    {
        eprintln!("bench: cannot write {}: {e}", path.display());
        return ExitCode::from(2);
    }

    if report.succeeded() == report.queries.len() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
