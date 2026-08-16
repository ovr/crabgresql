//! crabgresql-server: accepts pgwire connections and wires the layers
//! together. The binary lives in `main.rs`; this library entry point exists
//! so integration tests can run a server in-process on an ephemeral port.

mod catalog;
mod connection;
mod copy;
mod copy_access;
mod cursor;
mod error;
mod explain;
mod func_deps;
mod global_catalog;
mod guc;
mod prepare;
mod query;
mod routines;
mod session;

use std::path::Path;
use std::sync::Arc;

use crabgresql_pg_engine::PgEngine;
use crabgresql_storage_api::TableEngine;
use crabgresql_storage_api::pgstat::PgStatCounters;
use crabgresql_txn::{CommitSink, TransactionManager, TxnFinalize};
use crabgresql_wal::Wal;
use tokio::net::TcpListener;

pub use crate::copy_access::CopyFileAccess;
use crate::global_catalog::GlobalCatalog;

/// Open the durable heap engine over a data directory and run crash recovery:
/// replay the WAL from the redo point the last checkpoint published, rebuild the
/// commit log, seed the XID allocator above every recovered transaction, then
/// checkpoint so the recovered pages are on disk.
/// Returns the engine and a WAL-backed [`TransactionManager`] to hand to
/// [`serve_with`].
pub fn open_pg_engine(
    data_dir: &Path,
) -> std::io::Result<(Arc<dyn TableEngine>, Arc<TransactionManager>)> {
    let wal = Arc::new(Wal::open(data_dir).map_err(std::io::Error::other)?);
    // The full open + crash-recovery sequence lives in the engine so the server
    // and the recovery tests share exactly one code path.
    let (engine, clog, next_xid) = PgEngine::open_recovered(data_dir, Arc::clone(&wal))?;
    let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
    let mut txnmgr = TransactionManager::new_recovered(sink, clog, next_xid);
    // Wire the engine's finalize hook so commit/abort apply or discard the swaps
    // and release the TRUNCATE table lock, on every finalize path.
    txnmgr.set_finalize(Arc::clone(&engine) as Arc<dyn TxnFinalize>);
    let txnmgr = Arc::new(txnmgr);
    // Only after the finalize hook is wired: `VACUUM` flushes a RAM write buffer
    // by committing its own transaction, and a commit that fired before the hook
    // existed would never promote the fragment it had just written.
    engine.attach_txn_manager(Arc::clone(&txnmgr));
    Ok((engine as Arc<dyn TableEngine>, txnmgr))
}

/// Accept connections forever with the default, non-durable transaction manager
/// (no WAL) and no server-side COPY file access — the in-memory entry point,
/// which has no data directory to anchor either on. Durable deployments call
/// [`serve_with`] with a WAL-backed manager.
pub async fn serve(listener: TcpListener, engine: Arc<dyn TableEngine>) -> std::io::Result<()> {
    serve_with(
        listener,
        engine,
        Arc::new(TransactionManager::new()),
        CopyFileAccess::deny_all(),
    )
    .await
}

/// Accept connections forever, one tokio task per session, using the supplied
/// [`TransactionManager`]. The durable path passes a manager built by
/// [`TransactionManager::new_recovered`] so commits are WAL-logged and fsynced.
/// Both the user-object catalog (types/functions/casts) and the manager (XID
/// allocator + commit log) are shared across every connection for the life of
/// the server, matching PG's persistent catalog.
///
/// `copy_files` is passed explicitly rather than defaulted: it decides which
/// files a client can make the server read, and a silent default is exactly the
/// kind of security setting that rots unnoticed.
pub async fn serve_with(
    listener: TcpListener,
    engine: Arc<dyn TableEngine>,
    txnmgr: Arc<TransactionManager>,
    copy_files: CopyFileAccess,
) -> std::io::Result<()> {
    let catalog = Arc::new(GlobalCatalog::with_copy_files(copy_files));
    // Nothing durably records when a definition last changed, so the catalog's
    // generation starts wherever the transaction counter does — "as far as this
    // server knows, everything is as of now". Left at zero it would be
    // `InvalidTransactionId`, which `age()` answers with 2147483647, so a client
    // testing `age(xmin) < threshold` would see no relation as fresh at exactly
    // the moment it first caches a schema.
    catalog.seed_ddl_generation(txnmgr.clog().next_xid_floor().0);
    // One server, one set of counters, for every connection and for as long as
    // the server runs. `stats_reset` is stamped now because nothing reads a
    // statistics file back (see `crabgresql_storage_api::pgstat`).
    let stats = Arc::new(PgStatCounters::new(crabgresql_types::tz::now_micros()));
    loop {
        let (socket, peer) = listener.accept().await?;
        let engine = engine.clone();
        let catalog = catalog.clone();
        let txnmgr = txnmgr.clone();
        let stats = stats.clone();
        tokio::spawn(async move {
            tracing::debug!(%peer, "connection opened");
            if let Err(e) =
                connection::handle_connection(socket, engine, catalog, txnmgr, stats).await
            {
                tracing::warn!(%peer, error = %e, "connection failed");
            }
        });
    }
}
