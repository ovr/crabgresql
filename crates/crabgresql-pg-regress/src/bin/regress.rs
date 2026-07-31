//! pg_regress-style runner: execute regression scripts against an in-process
//! CrabgreSQL server and diff the output against the expected files.
//!
//! Exit codes: 0 — all tests passed, 1 — at least one failed, 2 — bad usage
//! or an infrastructure error (missing files, I/O).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use crabgresql_pg_regress::runner::{SuiteConfig, run_suite};
use crabgresql_pg_regress::schedule::parse_schedule;

/// Run PostgreSQL regression tests against an in-process CrabgreSQL server.
#[derive(Parser)]
#[command(name = "regress")]
struct Args {
    /// Run only the named tests; test_setup runs first (unchecked) when
    /// present, unless --no-setup
    #[arg(long, value_delimiter = ',', value_name = "NAME,NAME")]
    tests: Option<Vec<String>>,

    /// Schedule file to run [default: <regress-dir>/parallel_schedule]
    #[arg(long, value_name = "PATH", conflicts_with = "tests")]
    schedule: Option<PathBuf>,

    /// Directory containing sql/ and expected/
    #[arg(long, value_name = "DIR", default_value = "vendor/postgres/regress")]
    regress_dir: PathBuf,

    /// Where results/ and regression.diffs are written
    #[arg(long, value_name = "DIR", default_value = "target/regress")]
    outdir: PathBuf,

    /// Per-statement timeout in seconds
    #[arg(long, value_name = "SECONDS", default_value_t = 30)]
    statement_timeout: u64,

    /// Do not run test_setup before --tests
    #[arg(long)]
    no_setup: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = Args::parse();

    let (setup, tests) = match &args.tests {
        Some(tests) => {
            // Mirror pg_regress: an explicit test list still needs the shared
            // tables test_setup creates, but its own output is not checked.
            let setup_sql = args.regress_dir.join("sql/test_setup.sql");
            let setup =
                if !args.no_setup && setup_sql.exists() && !tests.iter().any(|t| t == "test_setup")
                {
                    vec!["test_setup".to_string()]
                } else {
                    vec![]
                };
            (setup, tests.clone())
        }
        None => {
            let path = args
                .schedule
                .clone()
                .unwrap_or_else(|| args.regress_dir.join("parallel_schedule"));
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) => {
                    eprintln!("regress: cannot read schedule {}: {e}", path.display());
                    return ExitCode::from(2);
                }
            };
            (vec![], parse_schedule(&text))
        }
    };
    if tests.is_empty() {
        eprintln!("regress: no tests to run");
        return ExitCode::from(2);
    }

    let config = SuiteConfig {
        regress_dir: args.regress_dir,
        setup,
        tests,
        outdir: args.outdir,
        statement_timeout: Duration::from_secs(args.statement_timeout),
        env: BTreeMap::new(),
    };
    let report = match run_suite(&config).await {
        Ok(report) => report,
        Err(e) => {
            eprintln!("regress: {e}");
            return ExitCode::from(2);
        }
    };

    for (name, passed) in &report.outcomes {
        println!("{} {name}", if *passed { "ok    " } else { "FAILED" });
    }
    let (passed, total) = (report.passed(), report.total());
    println!(
        "\n{passed} of {total} tests passed ({}%).",
        passed * 100 / total
    );
    if report.all_passed() {
        ExitCode::SUCCESS
    } else {
        println!(
            "See {} for details.",
            config.outdir.join("regression.diffs").display()
        );
        ExitCode::FAILURE
    }
}
