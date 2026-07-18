//! crabgresql-server: accepts pgwire connections and wires the layers
//! together. The binary lives in `main.rs`; this library entry point exists
//! so integration tests can run a server in-process on an ephemeral port.

mod catalog;
mod connection;
mod error;
mod global_catalog;
mod query;
mod session;

use std::sync::Arc;

use crabgresql_storage_api::TableEngine;
use tokio::net::TcpListener;

use crate::global_catalog::GlobalCatalog;

/// Accept connections forever, one tokio task per session. The user-object
/// catalog (types/functions/casts) is created once here and shared across every
/// connection for the life of the server, matching PG's persistent catalog.
pub async fn serve(listener: TcpListener, engine: Arc<dyn TableEngine>) -> std::io::Result<()> {
    let catalog = Arc::new(GlobalCatalog::new());
    loop {
        let (socket, peer) = listener.accept().await?;
        let engine = engine.clone();
        let catalog = catalog.clone();
        tokio::spawn(async move {
            tracing::debug!(%peer, "connection opened");
            if let Err(e) = connection::handle_connection(socket, engine, catalog).await {
                tracing::warn!(%peer, error = %e, "connection failed");
            }
        });
    }
}
