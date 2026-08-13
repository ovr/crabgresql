//! Run the shipped `crabgresql` binary as a child process.
//!
//! The regression runner and the benchmark runner both drive a server they own
//! for the length of a run, the way pg_regress and pgbench drive a real
//! `postgres`. In-process would be less code, but a panic in the engine would
//! take the harness down with it — no report, nothing but a signal — and the
//! CLI users actually get (`--data-dir`, `--copy-allow-path`, their defaults)
//! would never be exercised. Here a crash is an event the caller observes and
//! reports, with the server's stderr preserved in its log file.

use std::io;
use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use tokio::process::{Child, Command};

/// Overrides the binary to spawn; otherwise it is found next to the running
/// executable (see [`locate_server_binary`]).
pub const SERVER_BIN_ENV: &str = "CRABGRESQL_SERVER_BIN";

/// The child's `RUST_LOG`. The ambient one is deliberately not inherited: a
/// developer's `RUST_LOG=error` would silence the log a crash is diagnosed
/// from, and their `RUST_LOG=trace` would bury it.
pub const SERVER_LOG_ENV: &str = "CRABGRESQL_SERVER_LOG";

/// As `crabgresql-server`'s `[[bin]]` declares it.
const SERVER_BIN_NAME: &str = "crabgresql";

const BUILD_HINT: &str = "build it with `cargo build -p crabgresql-server --bin crabgresql`, \
                          or point CRABGRESQL_SERVER_BIN at it";

/// How long the server may take to accept its first connection. Generous
/// because startup includes opening the engine and running crash recovery, and
/// a loaded CI machine is slow at both.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

const READY_POLL: Duration = Duration::from_millis(20);

/// How many ports to try before giving up. A port that was free a moment ago can
/// be taken before the child binds it — by another harness picking one the same
/// way, most often — and the child then dies with "address in use".
const PORT_ATTEMPTS: usize = 5;

const LOG_TAIL_LINES: usize = 40;

/// A running `crabgresql` child process, killed on drop.
pub struct ServerProcess {
    child: Child,
    port: u16,
    log_path: PathBuf,
}

/// The `crabgresql` binary to spawn: `CRABGRESQL_SERVER_BIN` if set, otherwise
/// the one built alongside the running executable — `target/<profile>/` for a
/// binary, and its parent for a test harness in `target/<profile>/deps/`.
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
    /// Start a server over `data_dir` on a loopback port picked here, and wait
    /// until it reports that it is listening on it.
    ///
    /// `copy_allow` are extra directories a server-side `COPY … FROM '<file>'`
    /// may read: the regression suites keep their fixtures in the source tree
    /// rather than in the data directory. A load that goes through
    /// `COPY … FROM STDIN` needs none.
    pub async fn start(
        binary: &Path,
        data_dir: &Path,
        copy_allow: &[PathBuf],
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
                // A child that died on its own most often lost the port between
                // our check and its bind, which another port fixes.
                Err(StartError::ChildExited(e)) => last = Some(e),
                // A server that came up but never announced its listener will
                // not on a different port either, and retrying would spend the
                // readiness timeout again for the same result.
                Err(StartError::Timeout(e)) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::other("no server start attempts were made")))
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// The child's process id while it runs, for a caller that has to signal it.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// The child's exit status if it has already died, without blocking.
    pub fn exited(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Same, but wait up to `grace` for a child that is on its way out.
    ///
    /// A dying server closes its sockets before the kernel is done with it, so
    /// the connection is lost a moment *before* the exit status exists. Asking
    /// the instant a client notices gets "still running", and the crash is
    /// misreported as one bad query.
    pub async fn exited_within(&mut self, grace: Duration) -> io::Result<Option<ExitStatus>> {
        match tokio::time::timeout(grace, self.child.wait()).await {
            Ok(status) => status.map(Some),
            Err(_elapsed) => Ok(None),
        }
    }

    /// The end of the log, which is where a panic's message and backtrace are.
    pub fn log_tail(&self) -> String {
        log_tail(&self.log_path, LOG_TAIL_LINES)
    }

    /// Stop the server and reap it, without asking for the clean-shutdown
    /// flush: a caller's data directory is either a throwaway or reused by a
    /// run that recovers it anyway.
    pub async fn shutdown(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }

    /// Wait until the child announces the port it bound, or until it dies
    /// trying.
    ///
    /// Readiness is the child's own log line rather than merely a connectable
    /// port. The port was free when we picked it, but a stranger — most often
    /// another harness picking one the same way — can bind it before our child
    /// gets there, and a probe would happily connect to theirs and run a whole
    /// suite against it. The log file is ours by construction, so a line in it
    /// is proof that the listener behind the port is ours too. It is written
    /// straight after the bind, so a connection cannot be refused after it
    /// appears.
    ///
    /// The file has to be polled — nothing portable announces a write — but the
    /// child is awaited, so a server that loses the port or fails to open its
    /// engine is reported the moment SIGCHLD arrives.
    async fn wait_ready(&mut self) -> Result<(), StartError> {
        let port = self.port;
        let log_path = self.log_path.clone();
        let needle = listening_line(port);
        let announced = async {
            let deadline = Instant::now() + READY_TIMEOUT;
            loop {
                if std::fs::read_to_string(&log_path).is_ok_and(|log| log.contains(&needle)) {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                tokio::time::sleep(READY_POLL).await;
            }
        };
        tokio::select! {
            status = self.child.wait() => Err(StartError::ChildExited(match status {
                Ok(status) => io::Error::other(format!(
                    "server exited during startup with {status}; see {}\n{}",
                    log_path.display(),
                    log_tail(&log_path, LOG_TAIL_LINES),
                )),
                Err(e) => e,
            })),
            ready = announced => if ready {
                Ok(())
            } else {
                Err(StartError::Timeout(io::Error::other(format!(
                    "server did not report listening on port {port} within \
                     {READY_TIMEOUT:?}; see {}\n{}",
                    log_path.display(),
                    log_tail(&log_path, LOG_TAIL_LINES),
                ))))
            },
        }
    }
}

/// What the server logs once its listener is up, which is what
/// [`ServerProcess::wait_ready`] waits for. Kept in one place because the
/// harness reads a line the server writes: the integration test asserts the
/// same text, so a reworded message fails there rather than only as a startup
/// timeout here.
fn listening_line(port: u16) -> String {
    format!("listening on 127.0.0.1:{port}")
}

/// Why a start attempt failed, which decides whether another port would help.
enum StartError {
    ChildExited(io::Error),
    Timeout(io::Error),
}

/// Launch the child with both streams redirected into `log_path`, which is
/// truncated first: it belongs to this run.
fn spawn(
    binary: &Path,
    data_dir: &Path,
    copy_allow: &[PathBuf],
    log_path: &Path,
    port: u16,
) -> io::Result<Child> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = std::fs::File::create(log_path)?;
    let stderr = log.try_clone()?;
    let mut command = Command::new(binary);
    command
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--listen-address")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string());
    for path in copy_allow {
        command.arg("--copy-allow-path").arg(path);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        // A backtrace is the whole point of keeping the log.
        .env("RUST_BACKTRACE", "1")
        .env(crabgresql_config::LOG_FILTER, log_filter())
        // The server reads these as fallbacks for the arguments above; an
        // inherited value must not decide where a run stores its data or which
        // files it may read.
        .env_remove(crabgresql_config::DATA_DIR)
        .env_remove(crabgresql_config::PORT)
        .env_remove(crabgresql_config::LISTEN_ADDRESS)
        .env_remove(crabgresql_config::COPY_ALLOW_PATHS)
        // Ctrl-C or a panicking harness must not leave a server holding a port
        // and a data directory.
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

/// The child's `RUST_LOG`. An override keeps `crabgresql=info` appended: the
/// harness reads readiness off the server's own "listening on" line, and a
/// filter that hid it would turn every start into a timeout.
fn log_filter() -> String {
    match std::env::var(SERVER_LOG_ENV) {
        Ok(filter) => format!("{filter},crabgresql=info"),
        Err(_) => crabgresql_config::DEFAULT_LOG_FILTER.to_string(),
    }
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
            &[],
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

    /// The port-race case: something else holds the port, so the child dies
    /// binding it while the probe happily connects to the *other* listener.
    /// Readiness must not be declared on someone else's socket.
    #[tokio::test]
    async fn a_child_that_lost_its_port_is_not_ready() {
        let binary = locate_server_binary().expect("the server binary");
        let dir = tempfile::tempdir().expect("a temp dir");
        let log_path = dir.path().join("server.log");
        // Held open for the whole test: this is the listener the probe finds.
        let thief = StdTcpListener::bind(("127.0.0.1", 0)).expect("a port");
        let port = thief.local_addr().expect("an address").port();
        let mut server = ServerProcess {
            child: spawn(&binary, dir.path(), &[], &log_path, port).expect("spawn"),
            port,
            log_path,
        };
        match server.wait_ready().await {
            Err(StartError::ChildExited(_)) => {}
            Err(StartError::Timeout(e)) => panic!("reported as a timeout: {e}"),
            Ok(()) => panic!("declared ready on a port the server does not own"),
        }
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
