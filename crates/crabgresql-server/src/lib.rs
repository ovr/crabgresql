//! crabgresql-server: accepts pgwire connections and wires the layers
//! together. The binary lives in `main.rs`; this library entry point exists
//! so integration tests can run a server in-process on an ephemeral port.

mod catalog;
mod connection;
mod error;
mod global_catalog;
mod query;
mod session;

use std::path::Path;
use std::sync::Arc;

use crabgresql_pg_engine::PgEngine;
use crabgresql_storage_api::TableEngine;
use crabgresql_txn::{Clog, CommitSink, TransactionManager};
use crabgresql_wal::{RmgrRegistry, Wal, recover};
use tokio::net::TcpListener;

use crate::global_catalog::GlobalCatalog;

/// Open the durable heap engine over a data directory and run crash recovery:
/// replay the WAL, rebuild the commit log, seed the XID allocator above every
/// recovered transaction, then checkpoint so the recovered pages are on disk.
/// Returns the engine and a WAL-backed [`TransactionManager`] to hand to
/// [`serve_with`].
pub fn open_pg_engine(
    data_dir: &Path,
) -> std::io::Result<(Arc<dyn TableEngine>, Arc<TransactionManager>)> {
    let wal = Arc::new(Wal::open(data_dir).map_err(std::io::Error::other)?);
    let mut registry = RmgrRegistry::new();
    let engine = PgEngine::new(data_dir, Arc::clone(&wal), &mut registry)?;
    let clog = Arc::new(Clog::new());
    let res = recover(data_dir, &registry, &clog).map_err(std::io::Error::other)?;
    // Make the pages recovery reconstructed durable so restarts start from a
    // clean base (recovery is still correct without this, just longer).
    engine.checkpoint(res.next_xid)?;
    let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
    let txnmgr = Arc::new(TransactionManager::new_recovered(sink, clog, res.next_xid));
    Ok((Arc::new(engine) as Arc<dyn TableEngine>, txnmgr))
}

/// Accept connections forever with the default, non-durable transaction manager
/// (no WAL) — the entry point the memory-engine tests and the in-memory default
/// use. Durable deployments call [`serve_with`] with a WAL-backed manager.
pub async fn serve(listener: TcpListener, engine: Arc<dyn TableEngine>) -> std::io::Result<()> {
    serve_with(listener, engine, Arc::new(TransactionManager::new())).await
}

/// Accept connections forever, one tokio task per session, using the supplied
/// [`TransactionManager`]. The durable path passes a manager built by
/// [`TransactionManager::new_recovered`] so commits are WAL-logged and fsynced.
/// Both the user-object catalog (types/functions/casts) and the manager (XID
/// allocator + commit log) are shared across every connection for the life of
/// the server, matching PG's persistent catalog.
pub async fn serve_with(
    listener: TcpListener,
    engine: Arc<dyn TableEngine>,
    txnmgr: Arc<TransactionManager>,
) -> std::io::Result<()> {
    let catalog = Arc::new(GlobalCatalog::new());
    loop {
        let (socket, peer) = listener.accept().await?;
        let engine = engine.clone();
        let catalog = catalog.clone();
        let txnmgr = txnmgr.clone();
        tokio::spawn(async move {
            tracing::debug!(%peer, "connection opened");
            if let Err(e) = connection::handle_connection(socket, engine, catalog, txnmgr).await {
                tracing::warn!(%peer, error = %e, "connection failed");
            }
        });
    }
}
