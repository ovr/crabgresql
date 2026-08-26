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

/// `initdb` creates the directory before it can lock it, and the lock must not
/// get in the way of that.
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

/// The lock is taken before the cluster is *created*, not after: creating one is
/// as much a write as any other, and a start racing another start over an empty
/// directory would otherwise have two servers writing one `pg_control`. The
/// observable half of that ordering is that a server pointed at nothing at all
/// still ends up holding a lock file in the directory it made.
#[tokio::test]
async fn a_server_locks_a_data_directory_it_created_itself() {
    let (_dir, data_dir, log) = workspace();
    assert!(!data_dir.exists(), "the directory must not exist yet");

    let first = start(&data_dir, &log).await;

    assert!(data_dir.join("PG_VERSION").exists(), "no cluster");
    assert!(data_dir.join(LOCK_FILE).exists(), "no lock file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&data_dir)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "a created data directory is owner-only"
        );
    }
    // And it is a real lock, not just a file: the directory it created is as
    // exclusive as one it was handed.
    let refused = run_server(&data_dir, 1);
    assert!(!refused.status.success(), "a second server started anyway");

    first.shutdown().await;
}

/// A refused server must not have written anything on its way to being refused.
/// `global/pg_control` is the file that matters: the failure this whole
/// interlock exists to prevent is two servers publishing redo points into it.
///
/// The holder is this test process rather than a running server, so nothing but
/// the refused server can write to the directory while it is watched.
#[test]
fn a_refused_server_does_not_touch_the_control_file() {
    let (_dir, data_dir, _log) = workspace();
    assert!(run_initdb(&data_dir).status.success(), "initdb");
    let control = data_dir.join("global").join("pg_control");
    let before = std::fs::read(&control).expect("a control file");
    // A lock file naming a PID that is certainly alive: our own.
    std::fs::write(
        data_dir.join(LOCK_FILE),
        format!("{}\n{}\n", std::process::id(), data_dir.display()),
    )
    .expect("a lock file");

    let refused = run_server(&data_dir, 1);

    assert!(!refused.status.success(), "the server started anyway");
    assert_eq!(
        std::fs::read(&control).expect("a control file"),
        before,
        "the refused server rewrote the control file"
    );
}

/// The same ordering where it costs most: an *empty* directory. The refused
/// server must leave it empty rather than initializing a cluster into it and
/// only then noticing that somebody holds the lock.
#[test]
fn a_refused_server_does_not_create_a_cluster() {
    let (_dir, data_dir, _log) = workspace();
    std::fs::create_dir_all(&data_dir).expect("an empty data directory");
    std::fs::write(
        data_dir.join(LOCK_FILE),
        format!("{}\n{}\n", std::process::id(), data_dir.display()),
    )
    .expect("a lock file");

    let refused = run_server(&data_dir, 1);

    assert!(!refused.status.success(), "the server started anyway");
    assert!(
        !data_dir.join("PG_VERSION").exists(),
        "the refused server initialized the directory anyway"
    );
    assert!(
        !data_dir.join("global").exists(),
        "the refused server wrote a skeleton anyway"
    );
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

/// A server stopped while it is still starting must not leave its lock file
/// behind: it never opened the cluster, and the next start should not have to
/// reason about a file from a server that never ran.
///
/// The signal lands somewhere in startup — after the lock is taken if the timing
/// works out, before it if it does not — so this asserts the invariant that
/// holds either way. It cannot, on a cluster this small, tell the watcher from
/// the latched-signal path that runs once the listener is up; that difference is
/// only visible on a recovery long enough to matter, and is checked by hand.
#[test]
fn a_signal_during_startup_leaves_no_lock_file() {
    let (_dir, data_dir, _log) = workspace();
    assert!(run_initdb(&data_dir).status.success(), "initdb");
    let binary = locate_server_binary(None).expect("the server binary");

    let mut child = Command::new(binary)
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--listen-address")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(free_port().to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the server binary runs");
    // Straight away: the point is to arrive while the server is still opening
    // its cluster.
    std::thread::sleep(Duration::from_millis(5));
    // SAFETY: SIGTERM to a child of this process.
    assert_eq!(
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) },
        0,
        "SIGTERM"
    );

    let status = child.wait().expect("waiting on the child");
    assert!(
        !data_dir.join(LOCK_FILE).exists(),
        "a server stopped during startup left its lock file behind ({status})"
    );
}

/// A port nothing is listening on, found by binding one and letting it go — the
/// same trick the server harness uses.
fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("a port")
        .local_addr()
        .expect("an address")
        .port()
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
