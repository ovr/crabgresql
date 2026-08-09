use std::net::IpAddr;
use std::path::PathBuf;

use clap::Parser;
use crabgresql_config as config;
use tokio::net::TcpListener;

/// crabgresql — a PostgreSQL-compatible server.
#[derive(Parser)]
#[command(name = "crabgresql", version)]
struct Cli {
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
    #[arg(long = "data-dir", short = 'D', env = config::DATA_DIR, default_value = config::DEFAULT_DATA_DIR)]
    data_dir: PathBuf,

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

#[tokio::main]
async fn main() -> std::io::Result<()> {
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

    let cli = Cli::parse();

    tracing::info!(
        "opening durable heap engine at {} (running recovery)",
        cli.data_dir.display()
    );
    let (engine, txnmgr) = crabgresql_server::open_pg_engine(&cli.data_dir)?;

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
        result = crabgresql_server::serve_with(listener, engine, txnmgr, copy_files) => result,
        () = shutdown_signal() => {
            tracing::info!("received shutdown signal; flushing for a clean shutdown");
            engine_for_shutdown.shutdown();
            Ok(())
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
