//! Two servers, one data directory — what `postmaster.pid` exists to prevent.
//!
//! The unit tests in `lockfile.rs` exercise the file; these exercise the thing
//! that matters, which is that the *shipped binary* refuses to open a cluster
//! another process has open, and lets go of it when it exits cleanly.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use crabgresql_server::lockfile::LOCK_FILE;
use crabgresql_server_process::{ServerProcess, locate_server_binary};

/// Start a server the way the harness does — a loopback port of its own, and a
/// wait for its own "listening on" line — over `data_dir`.
async fn start(data_dir: &Path, log: &Path) -> ServerProcess {
    let binary = locate_server_binary(None).expect("the server binary");
    ServerProcess::start(&binary, data_dir, &[], log)
        .await
        .expect("the first server starts")
}

/// Run the binary to completion, which is what a server that refuses to start
/// does. The port is one nothing else in this test uses; it is never bound,
/// because the refusal comes first.
fn run_server(data_dir: &Path, port: u16) -> Output {
    let binary = locate_server_binary(None).expect("the server binary");
    Command::new(binary)
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--listen-address")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .output()
        .expect("the server binary runs")
}

fn run_initdb(data_dir: &Path) -> Output {
    let binary = locate_server_binary(None).expect("the server binary");
    Command::new(binary)
        .arg("initdb")
        .arg("--data-dir")
        .arg(data_dir)
        .output()
        .expect("the server binary runs")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A data directory and a log path outside it — a log *inside* a fresh one would
/// make the directory non-empty and stop the very server it records.
fn workspace() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let data_dir = dir.path().join("pgdata");
    let log = dir.path().join("server.log");
    (dir, data_dir, log)
}

/// The whole point: the second server does not open the cluster, and says who
/// has it.
#[tokio::test]
async fn a_second_server_refuses_the_running_ones_data_directory() {
    let (_dir, data_dir, log) = workspace();
    let first = start(&data_dir, &log).await;
    let holder = first.pid().expect("a running child has a pid");

    let refused = run_server(&data_dir, 1);

    assert!(
        !refused.status.success(),
        "the second server started anyway: {refused:?}"
    );
    let text = stderr(&refused);
    assert!(text.contains("already exists"), "unhelpful stderr: {text}");
    assert!(
        text.contains(&format!("PID {holder}")),
        "stderr must name the running server ({holder}): {text}"
    );
    // And the running server is untouched by the attempt.
    assert!(data_dir.join(LOCK_FILE).exists());
    first.shutdown().await;
}

/// `initdb` takes the same lock: rewriting the skeleton of a cluster a server
/// has open is the same mistake with a different command line.
#[tokio::test]
async fn initdb_refuses_a_running_data_directory() {
    let (_dir, data_dir, log) = workspace();
    let first = start(&data_dir, &log).await;

    let refused = run_initdb(&data_dir);

    assert!(!refused.status.success(), "initdb ran anyway: {refused:?}");
    assert!(
        stderr(&refused).contains("already exists"),
        "unhelpful stderr: {}",
        stderr(&refused)
    );
    first.shutdown().await;
}

/// `initdb` on a directory that does not exist yet has nothing to lock out, and
/// must not be stopped by the attempt to look.
#[test]
fn initdb_still_creates_an_absent_directory() {
    let (_dir, data_dir, _log) = workspace();

    let created = run_initdb(&data_dir);

    assert!(created.status.success(), "initdb failed: {created:?}");
    assert!(data_dir.join("PG_VERSION").exists());
    // The lock is released when the command exits, so nothing is left behind to
    // block the first server.
    assert!(!data_dir.join(LOCK_FILE).exists());
}

/// A clean shutdown releases the directory, and the file goes with it.
#[tokio::test]
async fn a_clean_shutdown_removes_the_lock_file() {
    let (_dir, data_dir, log) = workspace();
    let mut first = start(&data_dir, &log).await;
    let lock = data_dir.join(LOCK_FILE);
    assert!(lock.exists(), "a running server holds its lock file");

    let pid = first.pid().expect("a running child has a pid") as i32;
    // SAFETY: SIGTERM to a child of this process, which is the signal its
    // shutdown path is written for.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0, "SIGTERM");
    first
        .exited_within(Duration::from_secs(30))
        .await
        .expect("waiting on the child")
        .expect("the server exits on SIGTERM");

    assert!(!lock.exists(), "the lock file outlived the server");

    // And the directory is immediately reusable.
    let second = start(&data_dir, &log).await;
    second.shutdown().await;
}

/// What a `SIGKILL`ed server leaves behind must not need an operator with a
/// `rm`: the next start takes the stale file over.
#[tokio::test]
async fn a_killed_server_leaves_a_file_the_next_one_takes_over() {
    let (_dir, data_dir, log) = workspace();
    let first = start(&data_dir, &log).await;
    let lock = data_dir.join(LOCK_FILE);

    // `shutdown` is a kill, so `Drop` never runs and the file stays.
    first.shutdown().await;
    assert!(lock.exists(), "a killed server leaves its lock file behind");

    let second = start(&data_dir, &log).await;
    let contents = std::fs::read_to_string(&lock).expect("the new lock file");
    assert_eq!(
        contents.lines().next().expect("a pid line"),
        second.pid().expect("a running child has a pid").to_string(),
        "the stale file was not taken over"
    );
    second.shutdown().await;
}
