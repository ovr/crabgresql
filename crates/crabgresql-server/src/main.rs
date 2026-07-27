use std::path::PathBuf;

use clap::Parser;
use crabgresql_config as config;
use tokio::net::TcpListener;

/// crabgresql — a PostgreSQL-compatible server.
#[derive(Parser)]
#[command(name = "crabgresql", version)]
struct Cli {
    /// Port to listen on. Defaults one above PG's 5432 so a local PostgreSQL can
    /// keep running.
    #[arg(long, short = 'p', env = config::PORT, default_value_t = config::DEFAULT_PORT)]
    port: u16,

    /// Data directory (PGDATA). The durable heap engine is opened here and crash
    /// recovery runs at startup. Defaults to `./pgdata` when omitted.
    #[arg(long = "data-dir", short = 'D', env = config::DATA_DIR, default_value = config::DEFAULT_DATA_DIR)]
    data_dir: PathBuf,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env(config::LOG_FILTER)
                .unwrap_or_else(|_| config::DEFAULT_LOG_FILTER.into()),
        )
        .init();

    let cli = Cli::parse();

    tracing::info!(
        "opening durable heap engine at {} (running recovery)",
        cli.data_dir.display()
    );
    let (engine, txnmgr) = crabgresql_server::open_pg_engine(&cli.data_dir)?;
    // Keep a handle to flush + mark a clean shutdown on Ctrl-C / SIGTERM, so
    // unlogged tables' data is kept across the restart (a crash would leave the
    // control file dirty and reset them).
    let engine_for_shutdown = engine.clone();

    let listener = TcpListener::bind(("127.0.0.1", cli.port)).await?;
    tracing::info!(
        "crabgresql listening on 127.0.0.1:{} (try: psql -h 127.0.0.1 -p {})",
        cli.port,
        cli.port
    );

    tokio::select! {
        result = crabgresql_server::serve_with(listener, engine, txnmgr) => result,
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
