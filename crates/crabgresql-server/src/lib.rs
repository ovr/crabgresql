//! crabgresql-server: accepts pgwire connections and wires the layers
//! together. The binary lives in `main.rs`; this library entry point exists
//! so integration tests can run a server in-process on an ephemeral port.

mod connection;
mod error;
mod query;
mod session;

use std::sync::Arc;

use crabgresql_storage_api::TableEngine;
use tokio::net::TcpListener;

/// Accept connections forever, one tokio task per session.
pub async fn serve(listener: TcpListener, engine: Arc<dyn TableEngine>) -> std::io::Result<()> {
    loop {
        let (socket, peer) = listener.accept().await?;
        let engine = engine.clone();
        tokio::spawn(async move {
            tracing::debug!(%peer, "connection opened");
            if let Err(e) = connection::handle_connection(socket, engine).await {
                tracing::warn!(%peer, error = %e, "connection failed");
            }
        });
    }
}
