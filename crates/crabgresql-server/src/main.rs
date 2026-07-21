use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use crabgresql_memory_storage::MemoryEngine;
use crabgresql_storage_api::TableEngine;
use crabgresql_txn::TransactionManager;
use tokio::net::TcpListener;

/// crabgresql — a PostgreSQL-compatible server.
#[derive(Parser)]
#[command(name = "crabgresql", version)]
struct Cli {
    /// Port to listen on. Defaults one above PG's 5432 so a local PostgreSQL can
    /// keep running.
    #[arg(long, short = 'p', env = "CRABGRESQL_PORT", default_value_t = 5433)]
    port: u16,

    /// Data directory (PGDATA). When set, the durable heap engine is used and
    /// crash recovery runs at startup; when omitted, tables live in memory and
    /// are lost on exit.
    #[arg(long = "data-dir", short = 'D', env = "PGDATA")]
    data_dir: Option<PathBuf>,

    /// Directory for `CREATE TABLE ... USING parquet` files. Defaults to
    /// `<data-dir>/parquet` when a data dir is set, otherwise a per-process
    /// directory under the system temp dir (ephemeral, like the memory engine).
    #[arg(long = "parquet-dir", env = "CRABGRESQL_PARQUET_DIR")]
    parquet_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    let (engine, txnmgr): (Arc<dyn TableEngine>, Arc<TransactionManager>) = match &cli.data_dir {
        Some(dir) => {
            tracing::info!(
                "opening durable heap engine at {} (running recovery)",
                dir.display()
            );
            crabgresql_server::open_pg_engine(dir)?
        }
        None => {
            tracing::info!("no --data-dir: using the in-memory engine (data is not persisted)");
            (
                Arc::new(MemoryEngine::new()),
                Arc::new(TransactionManager::new()),
            )
        }
    };

    // Route `USING parquet` tables to a Parquet engine composed over the boot
    // engine. Parquet files go under the data dir when durable, else a
    // per-process temp dir (ephemeral, matching the memory engine's lifetime).
    let parquet_dir = cli.parquet_dir.clone().unwrap_or_else(|| match &cli.data_dir {
        Some(dir) => dir.join("parquet"),
        None => std::env::temp_dir().join(format!("crabgresql-parquet-{}", std::process::id())),
    });
    tracing::info!("parquet tables stored under {}", parquet_dir.display());
    let engine = crabgresql_server::with_parquet_engine(engine, &parquet_dir)?;

    let listener = TcpListener::bind(("127.0.0.1", cli.port)).await?;
    tracing::info!(
        "crabgresql listening on 127.0.0.1:{} (try: psql -h 127.0.0.1 -p {})",
        cli.port,
        cli.port
    );

    crabgresql_server::serve_with(listener, engine, txnmgr).await
}
