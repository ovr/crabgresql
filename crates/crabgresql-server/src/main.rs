use std::path::PathBuf;

use clap::Parser;
use tokio::net::TcpListener;

/// Data directory used when neither `--data-dir` nor `PGDATA` is given.
const DEFAULT_DATA_DIR: &str = "./pgdata";

/// crabgresql — a PostgreSQL-compatible server.
#[derive(Parser)]
#[command(name = "crabgresql", version)]
struct Cli {
    /// Port to listen on. Defaults one above PG's 5432 so a local PostgreSQL can
    /// keep running.
    #[arg(long, short = 'p', env = "CRABGRESQL_PORT", default_value_t = 5433)]
    port: u16,

    /// Data directory (PGDATA). The durable heap engine is opened here and crash
    /// recovery runs at startup. Defaults to `./pgdata` when omitted.
    #[arg(long = "data-dir", short = 'D', env = "PGDATA", default_value = DEFAULT_DATA_DIR)]
    data_dir: PathBuf,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    tracing::info!(
        "opening durable heap engine at {} (running recovery)",
        cli.data_dir.display()
    );
    let (engine, txnmgr) = crabgresql_server::open_pg_engine(&cli.data_dir)?;

    let listener = TcpListener::bind(("127.0.0.1", cli.port)).await?;
    tracing::info!(
        "crabgresql listening on 127.0.0.1:{} (try: psql -h 127.0.0.1 -p {})",
        cli.port,
        cli.port
    );

    crabgresql_server::serve_with(listener, engine, txnmgr).await
}
