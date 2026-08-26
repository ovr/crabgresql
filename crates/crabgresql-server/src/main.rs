use std::net::IpAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use crabgresql_config as config;
use crabgresql_server::{initdb, lockfile};
use tokio::net::TcpListener;

/// crabgresql — a PostgreSQL-compatible server.
#[derive(Parser)]
#[command(name = "crabgresql", version)]
struct Cli {
    /// Absent means "run the server", so every command line written before this
    /// binary had subcommands — the image's entrypoint included — still means
    /// what it meant.
    #[command(subcommand)]
    command: Option<Command>,

    /// Address to accept connections on. Loopback by default: authentication is
    /// trust and there is no TLS, so exposing the port is opt-in.
    #[arg(
        long = "listen-address",
        short = 'l',
        env = config::LISTEN_ADDRESS,
        default_value = config::DEFAULT_LISTEN_ADDRESS
    )]
    listen_address: IpAddr,

    /// Port to listen on. Defaults one above PG's 5432 so a local PostgreSQL can
    /// keep running.
    #[arg(long, short = 'p', env = config::PORT, default_value_t = config::DEFAULT_PORT)]
    port: u16,

    /// Data directory (PGDATA). The durable heap engine is opened here and crash
    /// recovery runs at startup. Defaults to `./pgdata` when omitted.
    ///
    /// `global` so it means the same thing on either side of a subcommand:
    /// declared twice, `crabgresql -D dir initdb` created a cluster in
    /// `./pgdata` — the mistyped-`-D` accident `initdb` exists to refuse.
    #[arg(long = "data-dir", short = 'D', global = true, env = config::DATA_DIR, default_value = config::DEFAULT_DATA_DIR)]
    data_dir: PathBuf,

    /// Name of the bootstrap superuser created when the data directory has no
    /// role catalog yet, as `initdb --username` names PostgreSQL's. Ignored on
    /// an existing data directory, whose stored roles are authoritative.
    #[arg(
        long = "superuser",
        env = config::SUPERUSER,
        default_value = crabgresql_server::DEFAULT_SUPERUSER
    )]
    superuser: String,

    /// Extra directory a server-side `COPY … FROM '<file>'` may read. Repeatable,
    /// or colon-separated in the environment. The data directory is always
    /// allowed and is where a relative path resolves; by default nothing else
    /// is, because the read runs with the server's privileges.
    #[arg(
        long = "copy-allow-path",
        value_name = "PATH",
        env = config::COPY_ALLOW_PATHS,
        value_delimiter = ':',
        num_args = 1
    )]
    copy_allow_path: Vec<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new cluster in the data directory: its subdirectories, its
    /// version stamp and its first control file.
    ///
    /// The server does this for itself when started on an absent or empty
    /// directory, so this exists to do it deliberately — before a first start,
    /// under the account the server will run as, and with an error rather than
    /// a new cluster when the directory turns out to hold something already.
    ///
    /// The directory is `--data-dir`, which is shared with the server rather
    /// than redeclared here.
    Initdb {
        /// Skip the fsyncs. Faster, and unsafe for a cluster you intend to keep:
        /// a crash soon after can leave the directory half-durable.
        #[arg(long = "no-sync")]
        no_sync: bool,
    },
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env(config::LOG_FILTER)
                .unwrap_or_else(|_| config::DEFAULT_LOG_FILTER.into()),
        )
        .init();

    // Stamp the process start before anything slow (recovery can take minutes),
    // so `pg_postmaster_start_time()` reports when the server was launched
    // rather than when it first finished booting.
    crabgresql_types::tz::postmaster_start_micros();

    match run(Cli::parse()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        // Printed rather than returned: a `Result` from `main` is rendered with
        // `Debug`, and the first thing somebody who mistyped `--data-dir` would
        // read is `Error: Custom { kind: InvalidData, error: "…" }` with the
        // real message escaped inside it. The exit status is unchanged.
        Err(error) => {
            eprintln!("crabgresql: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> std::io::Result<()> {
    if let Some(Command::Initdb { no_sync }) = &cli.command {
        // Held across the whole of `initdb`, so it cannot write into a cluster a
        // server has open. The directory has to exist for the lock file to be
        // created in it, and creating it is `initdb`'s first act anyway.
        initdb::create_data_dir_if_absent(&cli.data_dir)?;
        let _lock = lockfile::PostmasterLock::acquire(
            &cli.data_dir,
            &lockfile::LockInfo::for_initdb(start_epoch_secs()),
        )?;
        let opts = initdb::InitOptions { sync: !no_sync };
        report(initdb::init_data_dir(&cli.data_dir, &opts)?, &cli.data_dir);
        return Ok(());
    }

    // Before any of the slow work, so the handlers exist for the whole of it —
    // see [`Shutdown::install`].
    let mut shutdown = Shutdown::install();

    // The lock comes before *everything* that touches the directory — before the
    // engine, and before the cluster is even created. Two servers over one data
    // directory each replay the same WAL and write the same files, which loses
    // data rather than merely making a mess, and creating a cluster is as much a
    // write as any other: a start racing a running `initdb` would otherwise
    // overwrite `global/pg_control` and only then be refused.
    //
    // Which is why the directory itself is created first — the lock file lives
    // inside it — and why `initdb` treats a directory holding nothing but a lock
    // file as empty. The lock is released when this function returns.
    initdb::create_data_dir_if_absent(&cli.data_dir)?;
    let lock = lockfile::PostmasterLock::acquire(
        &cli.data_dir,
        &lockfile::LockInfo {
            port: cli.port,
            listen_address: listen_address_line(cli.listen_address),
            start_epoch_secs: start_epoch_secs(),
        },
    )?;

    // From here until the server is serving, a shutdown signal is this task's
    // to handle: see `watch_startup`.
    let startup_done = watch_startup(lock.path().to_path_buf());

    tracing::info!(
        "opening durable heap engine at {} (running recovery)",
        cli.data_dir.display()
    );
    // All of it on a blocking thread, and awaited rather than called: every step
    // is synchronous, and recovery is minutes of it on a large cluster. Run on
    // the runtime's worker it would hold that worker for the whole of startup —
    // and on a one-core container there is only one, so the task watching for a
    // shutdown signal would never be polled, which is exactly the case
    // `watch_startup` exists for.
    let data_dir = cli.data_dir.clone();
    let superuser = cli.superuser.clone();
    let (engine, txnmgr, roles) = tokio::task::spawn_blocking(move || {
        // The cluster first: an empty directory becomes one, and a directory
        // that is not one is refused here rather than silently filled in piece
        // by piece by whichever component ran first.
        report(initdb::ensure_initialized(&data_dir)?, &data_dir);
        let (engine, txnmgr) = crabgresql_server::open_pg_engine(&data_dir)?;
        // Roles are a cluster object: one catalog for the whole data directory,
        // rather than one per database as the relation catalog is.
        let roles =
            std::sync::Arc::new(crabgresql_server::RoleCatalog::open(&data_dir, &superuser)?);
        Ok::<_, std::io::Error>((engine, txnmgr, roles))
    })
    .await
    .map_err(std::io::Error::other)??;

    let copy_files = cli.copy_allow_path.iter().fold(
        crabgresql_server::CopyFileAccess::confined_to(&cli.data_dir),
        |access, root| access.allowing(root),
    );
    // A confinement policy nobody can see is a support ticket.
    tracing::info!(
        "server-side COPY may read {} and {} extra path(s): {:?}",
        cli.data_dir.display(),
        cli.copy_allow_path.len(),
        cli.copy_allow_path
    );
    // Keep a handle to flush + mark a clean shutdown on Ctrl-C / SIGTERM, so
    // unlogged tables' data is kept across the restart (a crash would leave the
    // control file dirty and reset them).
    let engine_for_shutdown = engine.clone();

    // Anything but loopback hands every reachable client a superuser session.
    if !cli.listen_address.is_loopback() {
        tracing::warn!(
            "listening on {} — connections are unauthenticated (trust) and \
             unencrypted; expose this only on a trusted network",
            cli.listen_address
        );
    }

    let listener = TcpListener::bind((cli.listen_address, cli.port)).await?;
    tracing::info!(
        "crabgresql listening on {}:{} (try: psql -h {} -p {})",
        cli.listen_address,
        cli.port,
        cli.listen_address,
        cli.port
    );
    // Only now is the cluster actually open for business, which is what the
    // lock file's status line reports. A failure to say so is not worth
    // refusing to serve over.
    if let Err(error) = lock.mark_ready() {
        tracing::warn!(%error, "could not mark the lock file ready");
    }

    // Startup is over, so the watcher stands down and the select below — which
    // can flush the engine on the way out, as the watcher cannot — takes the
    // signals from here on.
    let _ = startup_done.send(());

    tokio::select! {
        result = crabgresql_server::serve_with(listener, engine, txnmgr, copy_files, roles) => result,
        () = shutdown.recv() => {
            tracing::info!("received shutdown signal; flushing for a clean shutdown");
            engine_for_shutdown.shutdown();
            Ok(())
        }
    }
}

/// Handle a shutdown signal that arrives while the server is still starting,
/// for as long as that lasts. Returns the sender that stands it down.
///
/// Startup is not a moment: it opens the engine, which replays the WAL, which
/// on a large cluster is minutes. A supervisor that stops the server in that
/// window — `docker stop`, `systemctl stop`, a Ctrl-C — has to be obeyed *then*,
/// not after recovery finishes, and until [`Shutdown`] existed it was, by the
/// signal's default disposition. Installing the handlers early took that away
/// and gave nothing back until the listener was up, so this task is the
/// replacement: it ends the process the way the default disposition did, and
/// removes the lock file on the way out, which the default disposition could
/// not.
///
/// It cannot do better than that — there is no engine to flush yet, and the
/// blocking recovery it interrupts is on another thread, exactly where a signal
/// would have interrupted it before. Once the server is serving, the sender
/// hands the signals to the select in [`run`], which can flush. A signal in the
/// instant between the two is handled here, which is the older behavior and no
/// worse than it.
///
/// The `initdb` subcommand deliberately has no watcher: its work is short, and
/// a killed `initdb` leaves a complete lock file with a dead PID, which the next
/// start takes over.
fn watch_startup(lock_path: PathBuf) -> tokio::sync::oneshot::Sender<()> {
    let (done, stood_down) = tokio::sync::oneshot::channel();
    // Its own handlers: tokio notifies every registered `Signal` of a kind, so
    // this does not consume the notification the select in `run` is waiting for.
    let mut watch = Shutdown::install();
    tokio::spawn(async move {
        tokio::select! {
            // Biased so that a stood-down watcher cannot also take a signal that
            // arrived at the same moment: the server is serving, and the select
            // in `run` is the one that can shut it down cleanly.
            biased;
            _ = stood_down => {}
            () = watch.recv() => {
                tracing::info!(
                    "received a shutdown signal during startup; exiting before the \
                     cluster was open"
                );
                lockfile::PostmasterLock::release_at(&lock_path);
                // A requested stop is not a failure, and `exit` is the point:
                // nothing above us can return, because `run` is blocked inside
                // startup.
                std::process::exit(0);
            }
        }
    });
    done
}

/// When this process started, in whole seconds since the epoch — the lock
/// file's third line.
///
/// Taken from the same stamp `pg_postmaster_start_time()` reports, so the file
/// and the function can never disagree about when the server came up. That
/// stamp counts from PostgreSQL's 2000 epoch and this line is a `time_t`, so it
/// crosses `to_unix_micros` on the way out — a `date -r` on the raw value would
/// otherwise read thirty years early.
fn start_epoch_secs() -> i64 {
    let micros = crabgresql_types::tz::postmaster_start_micros();
    crabgresql_types::tz::to_unix_micros(micros).div_euclid(1_000_000)
}

/// The lock file's sixth line: the address the server accepts connections on,
/// as PostgreSQL writes `listen_addresses` there — `*` for a wildcard bind,
/// because "0.0.0.0" and "::" say the same thing in two spellings and the file
/// is read by people.
fn listen_address_line(address: IpAddr) -> String {
    match address.is_unspecified() {
        true => "*".to_string(),
        false => address.to_string(),
    }
}

/// Say what initializing the data directory turned out to involve.
///
/// Every outcome is handled, including the one a given entry point cannot
/// currently produce: that is `initdb`'s business, and an `unreachable!()`
/// asserting this side's reading of it would turn a later relaxation there into
/// a panic.
fn report(outcome: initdb::Outcome, data_dir: &std::path::Path) {
    match outcome {
        initdb::Outcome::Created => {
            tracing::info!("created a cluster in {}", data_dir.display());
        }
        initdb::Outcome::AdoptedLegacy => {
            tracing::info!(
                "{} holds a cluster from before {} existed; stamped it and \
                 continuing with its data",
                data_dir.display(),
                initdb::PG_VERSION_FILE
            );
        }
        initdb::Outcome::AlreadyInitialized => {
            tracing::debug!("{} already holds a cluster", data_dir.display());
        }
    }
}

/// SIGINT (Ctrl-C) and SIGTERM, with their handlers already installed.
///
/// Installed at the top of startup rather than at the moment we first await
/// them, because until a handler exists the signal's *default* disposition
/// applies and kills the process outright: no clean-shutdown flush, no
/// `postmaster.pid` removed, and a control file left dirty enough to reset
/// every unlogged relation on the next start. The window used to cover
/// recovery, which takes minutes on a large cluster, and — worse — the moment
/// straight after the "listening" line, which is exactly when a supervisor or a
/// test harness stops a server it has just watched come up.
///
/// A signal that arrives before the wait is not lost: tokio's driver records it
/// against the handler and [`Shutdown::recv`] returns immediately.
struct Shutdown {
    /// `None` when the handler could not be installed. Nothing here can undo
    /// that, and a server that refused to start over it would be worse than one
    /// that can only be stopped the other way.
    interrupt: Option<tokio::signal::unix::Signal>,
    terminate: Option<tokio::signal::unix::Signal>,
}

impl Shutdown {
    fn install() -> Self {
        use tokio::signal::unix::{SignalKind, signal};
        let install = |kind: SignalKind, name: &str| match signal(kind) {
            Ok(handler) => Some(handler),
            Err(error) => {
                tracing::warn!(%error, "could not install the {name} handler");
                None
            }
        };
        Shutdown {
            interrupt: install(SignalKind::interrupt(), "SIGINT"),
            terminate: install(SignalKind::terminate(), "SIGTERM"),
        }
    }

    /// Resolves when either signal arrives, and never if neither handler could
    /// be installed — there is nothing to wait for, and the caller's other
    /// branch is the whole server.
    async fn recv(&mut self) {
        match (&mut self.interrupt, &mut self.terminate) {
            (Some(interrupt), Some(terminate)) => {
                tokio::select! {
                    _ = interrupt.recv() => {}
                    _ = terminate.recv() => {}
                }
            }
            (Some(only), None) | (None, Some(only)) => {
                only.recv().await;
            }
            (None, None) => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--data-dir` has to mean the same thing wherever it appears. It was
    /// declared twice — once on the server, once on the subcommand — and
    /// `crabgresql -D dir initdb` then took the *subcommand's* copy, defaulted
    /// to `./pgdata`, and created a cluster somewhere nobody asked for.
    #[test]
    fn the_data_directory_survives_the_subcommand_boundary() {
        let before = Cli::try_parse_from(["crabgresql", "-D", "/tmp/spelled-out", "initdb"])
            .expect("-D before the subcommand");
        let after = Cli::try_parse_from(["crabgresql", "initdb", "-D", "/tmp/spelled-out"])
            .expect("-D after the subcommand");

        assert_eq!(before.data_dir, PathBuf::from("/tmp/spelled-out"));
        assert_eq!(before.data_dir, after.data_dir);
        assert!(matches!(before.command, Some(Command::Initdb { .. })));
        assert!(matches!(after.command, Some(Command::Initdb { .. })));
    }

    /// No subcommand is still "run the server", which is what every existing
    /// command line and the image's entrypoint say.
    #[test]
    fn no_subcommand_runs_the_server() {
        let cli = Cli::try_parse_from(["crabgresql", "-D", "/tmp/pg", "-p", "5555"])
            .expect("the bare server form");
        assert!(cli.command.is_none());
        assert_eq!(cli.data_dir, PathBuf::from("/tmp/pg"));
        assert_eq!(cli.port, 5555);
    }

    /// The lock file's third line is a `time_t`, and the stamp it comes from
    /// counts from 2000 — a missed epoch shift puts the server's start time in
    /// 1996 and nothing but a human reading the file would notice.
    #[test]
    fn the_start_time_is_a_unix_timestamp() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_secs() as i64;
        let start = start_epoch_secs();
        assert!(
            (now - start).abs() < 60 * 60,
            "start time {start} is not within an hour of now ({now})"
        );
    }

    #[test]
    fn a_wildcard_bind_is_written_as_a_star() {
        assert_eq!(listen_address_line("0.0.0.0".parse().expect("v4")), "*");
        assert_eq!(listen_address_line("::".parse().expect("v6")), "*");
        assert_eq!(
            listen_address_line("127.0.0.1".parse().expect("v4")),
            "127.0.0.1"
        );
    }

    #[test]
    fn no_sync_belongs_to_initdb() {
        let cli = Cli::try_parse_from(["crabgresql", "initdb", "--no-sync"]).expect("--no-sync");
        assert!(matches!(
            cli.command,
            Some(Command::Initdb { no_sync: true })
        ));
        // And nowhere else: the server has no fsyncs of its own to skip.
        assert!(Cli::try_parse_from(["crabgresql", "--no-sync"]).is_err());
    }
}
