use std::sync::Arc;

use crabgresql_memory_storage::MemoryEngine;
use tokio::net::TcpListener;

/// Default port: one above PG's 5432 so a local PostgreSQL can keep running.
const DEFAULT_PORT: u16 = 5433;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let port = port_from_args_or_env().unwrap_or(DEFAULT_PORT);
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!("crabgresql listening on 127.0.0.1:{port} (try: psql -h 127.0.0.1 -p {port})");

    let engine = Arc::new(MemoryEngine::new());
    crabgresql_server::serve(listener, engine).await
}

/// `--port N` / `-p N` beats `CRABGRESQL_PORT`; both beat the default.
fn port_from_args_or_env() -> Option<u16> {
    let args: Vec<String> = std::env::args().collect();
    for pair in args.windows(2) {
        if pair[0] == "--port" || pair[0] == "-p" {
            match pair[1].parse() {
                Ok(port) => return Some(port),
                Err(_) => {
                    eprintln!("invalid port: {}", pair[1]);
                    std::process::exit(1);
                }
            }
        }
    }
    std::env::var("CRABGRESQL_PORT").ok()?.parse().ok()
}
