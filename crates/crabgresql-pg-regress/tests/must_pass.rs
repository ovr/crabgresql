//! Regression protection for `cargo test`: the crabgresql-authored smoke
//! suite must always pass, plus every upstream test promoted to
//! `suites/upstream_must_pass.txt`. All other upstream failures are
//! informational only — see the `regress` binary.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crabgresql_pg_regress::runner::{SuiteConfig, run_suite};
use crabgresql_pg_regress::schedule::parse_schedule;
use crabgresql_server_process::locate_server_binary;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Pin `PG_ABS_SRCDIR` to the suite's own directory.
///
/// `regress_environment` lets a real process variable win, so a run can be
/// pointed elsewhere — useful for the `regress` binary, but not here: these two
/// tests are the regression gate, and a shell that happens to export
/// `PG_ABS_SRCDIR` (a PostgreSQL build shell does) would redirect every
/// `COPY … FROM :'filename'` and redden the gate for unrelated reasons.
fn pinned_srcdir(suite_dir: &PathBuf) -> BTreeMap<String, String> {
    BTreeMap::from([(
        "PG_ABS_SRCDIR".to_string(),
        std::fs::canonicalize(suite_dir)
            .unwrap_or_else(|_| suite_dir.clone())
            .to_string_lossy()
            .into_owned(),
    )])
}

async fn assert_suite_passes(config: &SuiteConfig) -> anyhow::Result<()> {
    let report = run_suite(config).await?;
    if !report.all_passed() {
        let failed: Vec<&str> = report.failed().map(|o| o.name.as_str()).collect();
        let diffs =
            std::fs::read_to_string(config.outdir.join("regression.diffs")).unwrap_or_default();
        anyhow::bail!("regression tests failed: {failed:?}\n{diffs}");
    }
    Ok(())
}

#[tokio::test]
async fn smoke_suite_passes() -> anyhow::Result<()> {
    let suite_dir = manifest_dir().join("suites/smoke");
    let schedule = std::fs::read_to_string(suite_dir.join("schedule"))?;
    let env = pinned_srcdir(&suite_dir);
    let config = SuiteConfig {
        server_bin: locate_server_binary(None)?,
        regress_dir: suite_dir,
        setup: vec![],
        tests: parse_schedule(&schedule),
        outdir: PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("smoke"),
        statement_timeout: Duration::from_secs(30),
        env,
    };
    assert_suite_passes(&config).await?;

    Ok(())
}

#[tokio::test]
async fn upstream_must_pass() -> anyhow::Result<()> {
    let regress_dir = manifest_dir().join("../../vendor/postgres/regress");
    if !regress_dir.join("sql").is_dir() {
        eprintln!(
            "skipping upstream_must_pass: vendor/postgres/regress is not populated — \
             run scripts/sync-regress.sh"
        );
        return Ok(());
    }
    let list = std::fs::read_to_string(manifest_dir().join("suites/upstream_must_pass.txt"))?;
    let tests: Vec<String> = list
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(String::from)
        .collect();
    if tests.is_empty() {
        return Ok(());
    }
    let env = pinned_srcdir(&regress_dir);
    let config = SuiteConfig {
        server_bin: locate_server_binary(None)?,
        regress_dir,
        setup: vec!["test_setup".to_string()],
        tests,
        outdir: PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("upstream"),
        statement_timeout: Duration::from_secs(30),
        env,
    };
    assert_suite_passes(&config).await?;

    Ok(())
}
