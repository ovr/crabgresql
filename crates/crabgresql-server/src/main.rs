use std::net::IpAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use crabgresql_config as config;
use crabgresql_server::initdb;
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
        let opts = initdb::InitOptions { sync: !no_sync };
        report(initdb::init_data_dir(&cli.data_dir, &opts)?, &cli.data_dir);
        return Ok(());
    }

    // Before the engine opens anything: an absent or empty directory becomes a
    // cluster, and a directory that is not one is refused here rather than
    // silently filled in piece by piece by whichever component ran first.
    report(initdb::ensure_initialized(&cli.data_dir)?, &cli.data_dir);

    tracing::info!(
        "opening durable heap engine at {} (running recovery)",
        cli.data_dir.display()
    );
    let (engine, txnmgr) = crabgresql_server::open_pg_engine(&cli.data_dir)?;

    // Roles are a cluster object: one catalog for the whole data directory,
    // rather than one per database as the relation catalog is.
    let roles = std::sync::Arc::new(crabgresql_server::RoleCatalog::open(
        &cli.data_dir,
        &cli.superuser,
    )?);

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

    tokio::select! {
        result = crabgresql_server::serve_with(listener, engine, txnmgr, copy_files, roles) => result,
        () = shutdown_signal() => {
            tracing::info!("received shutdown signal; flushing for a clean shutdown");
            engine_for_shutdown.shutdown();
            Ok(())
        }
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

/// Resolves when the process receives SIGINT (Ctrl-C) or SIGTERM.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(term) => term,
        Err(error) => {
            tracing::warn!(%error, "could not install SIGTERM handler; Ctrl-C only");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
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
