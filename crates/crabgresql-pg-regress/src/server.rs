//! The server under test, run as a child process.
//!
//! pg_regress drives a `postgres` the same way: a real binary, its own
//! process, its own log file. Running it in-process would be less code, but a
//! panic in the engine would take the runner down with it — no report, no
//! stdout, nothing but a signal — and the CLI the users actually get
//! (`--data-dir`, `--copy-allow-path`, their defaults) would never be
//! exercised. Here a crash is an event the runner observes and reports, with
//! the server's stderr — backtrace included — preserved in `server.log`.

use std::io;
use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::process::{Child, Command};

/// Overrides the binary the runner spawns; otherwise it is found next to the
/// running executable (see [`locate_server_binary`]).
pub const SERVER_BIN_ENV: &str = "CRABGRESQL_SERVER_BIN";

/// The child's `RUST_LOG`. The ambient one is deliberately not inherited: a
/// developer's `RUST_LOG=error` would silence the log a crash is diagnosed
/// from, and their `RUST_LOG=trace` would bury it.
pub const SERVER_LOG_ENV: &str = "CRABGRESQL_SERVER_LOG";

/// The name of the shipped server binary, as `crabgresql-server`'s `[[bin]]`
/// declares it.
const SERVER_BIN_NAME: &str = "crabgresql";

/// How to get the binary when it is missing — the one thing the reader of the
/// error needs to know.
const BUILD_HINT: &str = "build it with `cargo build -p crabgresql-server --bin crabgresql`, \
                          or point CRABGRESQL_SERVER_BIN at it";

/// How long the server may take to accept its first connection. Generous
/// because startup includes opening the engine and running crash recovery, and
/// a loaded CI machine is slow at both.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Poll interval while waiting for the port to answer.
const READY_POLL: Duration = Duration::from_millis(20);

/// How many ports to try before giving up. A port picked from the ephemeral
/// range can be taken by someone else between the probe and the server's bind;
/// that is rare and retrying a fresh one fixes it.
const PORT_ATTEMPTS: usize = 5;

/// Lines of `server.log` quoted when the server dies.
const LOG_TAIL_LINES: usize = 40;

/// A running `crabgresql` child process, killed on drop so an aborted run
/// leaves nothing behind.
pub struct ServerProcess {
    child: Child,
    port: u16,
    log_path: PathBuf,
}

/// The `crabgresql` binary to spawn: `CRABGRESQL_SERVER_BIN` if set, otherwise
/// the one built alongside the running executable — `target/<profile>/` for the
/// `regress` binary, and its parent for a test harness in
/// `target/<profile>/deps/`.
pub fn locate_server_binary() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os(SERVER_BIN_ENV) {
        let path = PathBuf::from(path);
        if !path.exists() {
            return Err(io::Error::other(format!(
                "{SERVER_BIN_ENV} points at {}, which does not exist",
                path.display()
            )));
        }
        return Ok(path);
    }
    let exe = std::env::current_exe()?;
    let name = format!("{SERVER_BIN_NAME}{}", std::env::consts::EXE_SUFFIX);
    let candidates: Vec<PathBuf> = exe
        .parent()
        .into_iter()
        .flat_map(|dir| [dir.join(&name), dir.join("..").join(&name)])
        .collect();
    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .ok_or_else(|| {
            io::Error::other(format!(
                "cannot find the {SERVER_BIN_NAME} server binary near {} — {BUILD_HINT}",
                exe.display()
            ))
        })
}

impl ServerProcess {
    /// Start a server over `data_dir` and wait until it accepts connections.
    ///
    /// `copy_allow` is the suite's source tree: scripts load fixtures with
    /// `COPY … FROM :'abs_srcdir/data/x.data'`, which lives there and not in the
    /// throwaway PGDATA, so the server has to be told that tree is readable.
    pub async fn start(
        binary: &Path,
        data_dir: &Path,
        copy_allow: &Path,
        log_path: &Path,
    ) -> io::Result<Self> {
        let mut last: Option<io::Error> = None;
        for _ in 0..PORT_ATTEMPTS {
            let port = free_port()?;
            let mut server = Self {
                child: spawn(binary, data_dir, copy_allow, log_path, port)?,
                port,
                log_path: log_path.to_path_buf(),
            };
            match server.wait_ready().await {
                Ok(()) => return Ok(server),
                // The server exited on its own: most often the port was taken
                // between the probe and its bind, which another port fixes.
                // Anything else fails the same way on every attempt, and the
                // last error is the one reported.
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::other("no server start attempts were made")))
    }

    /// The port the server accepts connections on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The child's exit status if it has already died, without blocking. `None`
    /// means it is still running.
    pub fn exited(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Same, but give a dying child up to `grace` to actually die.
    ///
    /// A crashing server closes its sockets before the kernel is done with it,
    /// so the connection is lost a moment *before* the exit status exists. Ask
    /// the instant the client notices and the answer is "still running", and
    /// the crash gets misreported as one bad test.
    pub async fn exited_within(&mut self, grace: Duration) -> io::Result<Option<ExitStatus>> {
        let deadline = Instant::now() + grace;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(READY_POLL).await;
        }
    }

    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// The end of `server.log`, which is where a panic's message and backtrace
    /// are. Quoted into the failure so a CI log alone explains the crash.
    pub fn log_tail(&self) -> String {
        log_tail(&self.log_path, LOG_TAIL_LINES)
    }

    /// Stop the server and reap it. The data directory is a throwaway, so this
    /// kills rather than asking for the clean-shutdown flush.
    pub async fn shutdown(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }

    /// Poll the port until the server answers, failing as soon as the child
    /// exits instead of waiting out the timeout.
    async fn wait_ready(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Err(io::Error::other(format!(
                    "server exited during startup with {status}; see {}\n{}",
                    self.log_path.display(),
                    self.log_tail(),
                )));
            }
            if TcpStream::connect(("127.0.0.1", self.port)).await.is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(format!(
                    "server did not accept connections on port {} within {:?}; see {}\n{}",
                    self.port,
                    READY_TIMEOUT,
                    self.log_path.display(),
                    self.log_tail(),
                )));
            }
            tokio::time::sleep(READY_POLL).await;
        }
    }
}

/// Launch the child with both streams redirected into `log_path`, which is
/// truncated first: it belongs to this run.
fn spawn(
    binary: &Path,
    data_dir: &Path,
    copy_allow: &Path,
    log_path: &Path,
    port: u16,
) -> io::Result<Child> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = std::fs::File::create(log_path)?;
    let stderr = log.try_clone()?;
    Command::new(binary)
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--listen-address")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("--copy-allow-path")
        .arg(copy_allow)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        // A backtrace is the whole point of keeping the log.
        .env("RUST_BACKTRACE", "1")
        .env(
            crabgresql_config::LOG_FILTER,
            std::env::var(SERVER_LOG_ENV)
                .unwrap_or_else(|_| crabgresql_config::DEFAULT_LOG_FILTER.to_string()),
        )
        // The server reads these as fallbacks for the arguments above; an
        // inherited value must not decide where a regression run stores its
        // data or which files it may read.
        .env_remove(crabgresql_config::DATA_DIR)
        .env_remove(crabgresql_config::PORT)
        .env_remove(crabgresql_config::LISTEN_ADDRESS)
        .env_remove(crabgresql_config::COPY_ALLOW_PATHS)
        // An aborted run (Ctrl-C, a panicking test) must not leave a server
        // holding a port and a data directory.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "cannot run the server binary {} — {BUILD_HINT}: {e}",
                    binary.display()
                ),
            )
        })
}

/// A port nothing is listening on, found by binding one and letting it go. The
/// server binds it again a moment later; see [`PORT_ATTEMPTS`] for the gap.
fn free_port() -> io::Result<u16> {
    let listener = StdTcpListener::bind(("127.0.0.1", 0))?;
    listener.local_addr().map(|addr| addr.port())
}

/// The last `lines` lines of a file, or a note about why there are none.
fn log_tail(path: &Path, lines: usize) -> String {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => return format!("({}: {e})", path.display()),
    };
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing binary has to say how to get one: this is the first thing a
    /// contributor hits after a fresh clone.
    #[tokio::test]
    async fn a_missing_binary_says_how_to_build_it() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let error = ServerProcess::start(
            Path::new("/nonexistent/crabgresql"),
            dir.path(),
            dir.path(),
            &dir.path().join("server.log"),
        )
        .await
        .err()
        .expect("a missing binary cannot start");
        assert!(
            error
                .to_string()
                .contains("cargo build -p crabgresql-server"),
            "unhelpful error: {error}"
        );
    }

    #[test]
    fn log_tail_keeps_the_end_and_survives_a_missing_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("server.log");
        assert!(log_tail(&path, 2).contains("server.log"));
        std::fs::write(&path, "one\ntwo\nthree\n").expect("write");
        assert_eq!(log_tail(&path, 2), "two\nthree");
        assert_eq!(log_tail(&path, 9), "one\ntwo\nthree");
    }
}
