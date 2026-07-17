//! Regression protection for `cargo test`: the crabgresql-authored smoke
//! suite must always pass, plus every upstream test promoted to
//! `suites/upstream_must_pass.txt`. All other upstream failures are
//! informational only — see the `regress` binary.

use std::path::PathBuf;
use std::time::Duration;

use crabgresql_pg_regress::runner::{SuiteConfig, run_suite};
use crabgresql_pg_regress::schedule::parse_schedule;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

async fn assert_suite_passes(config: &SuiteConfig) {
    let report = run_suite(config).await.expect("suite should run");
    if !report.all_passed() {
        let failed: Vec<&str> = report
            .outcomes
            .iter()
            .filter(|(_, ok)| !ok)
            .map(|(name, _)| name.as_str())
            .collect();
        let diffs =
            std::fs::read_to_string(config.outdir.join("regression.diffs")).unwrap_or_default();
        panic!("regression tests failed: {failed:?}\n{diffs}");
    }
}

#[tokio::test]
async fn smoke_suite_passes() {
    let suite_dir = manifest_dir().join("suites/smoke");
    let schedule = std::fs::read_to_string(suite_dir.join("schedule")).unwrap();
    let config = SuiteConfig {
        regress_dir: suite_dir,
        setup: vec![],
        tests: parse_schedule(&schedule),
        outdir: PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("smoke"),
        statement_timeout: Duration::from_secs(30),
    };
    assert_suite_passes(&config).await;
}

#[tokio::test]
async fn upstream_must_pass() {
    let regress_dir = manifest_dir().join("../../vendor/postgres/regress");
    if !regress_dir.join("sql").is_dir() {
        eprintln!(
            "skipping upstream_must_pass: vendor/postgres/regress is not populated — \
             run scripts/sync-regress.sh"
        );
        return;
    }
    let list =
        std::fs::read_to_string(manifest_dir().join("suites/upstream_must_pass.txt")).unwrap();
    let tests: Vec<String> = list
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(String::from)
        .collect();
    if tests.is_empty() {
        return;
    }
    let config = SuiteConfig {
        regress_dir,
        setup: vec!["test_setup".to_string()],
        tests,
        outdir: PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("upstream"),
        statement_timeout: Duration::from_secs(30),
    };
    assert_suite_passes(&config).await;
}
