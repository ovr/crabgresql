//! Per-connection session state: GUCs, the temp-table catalog, and the current
//! transaction. The wire-facing control-flow status (`tx_status`, the RFQ
//! `I`/`T`/`E` byte) and the data-level transaction ([`ActiveTxn`], the XID and
//! snapshot MVCC runs against) are tracked side by side.

use std::collections::HashMap;
use std::sync::Arc;

use crabgresql_executor::{ExecContext, OutputColumn};
use crabgresql_memory_storage::MemoryEngine;
use crabgresql_parser::ast;
use crabgresql_pg_wire::{Format, TransactionStatus};
use crabgresql_storage_api::TableEngine;
use crabgresql_txn::{CommandId, IsolationLevel, Snapshot, TransactionManager, Xid};
use crabgresql_types::{PgType, Value};

/// A prepared statement (extended protocol `Parse`). Parse-analysis runs once,
/// here, so `Describe` can answer without executing and `Bind` knows each
/// parameter's type. Re-binding at `Execute` uses these resolved types, so the
/// plan is deterministic across executions.
pub struct PreparedStatement {
    /// The parsed statement; `None` for an empty query string (its `Execute`
    /// answers `EmptyQueryResponse`).
    pub stmt: Option<ast::Statement>,
    /// Resolved type per `$n` placeholder (index = n − 1), reported verbatim by
    /// `ParameterDescription`.
    pub param_types: Vec<PgType>,
    /// Result-column shape, or `None` for a statement that returns no rows
    /// (utility or data-modifying) — `Describe` then answers `NoData`.
    pub result_columns: Option<Vec<OutputColumn>>,
}

/// A portal (extended protocol `Bind`): a prepared statement plus concrete
/// parameter values and the client's requested result formats.
pub struct Portal {
    /// Name of the prepared statement this portal was bound from.
    pub statement: String,
    /// Decoded parameter values, parallel to the statement's `param_types`.
    pub params: Vec<Value>,
    /// Per-column result formats. Length 0 = all text, 1 = applies to every
    /// column, otherwise one entry per column (as the `Bind` message encodes).
    pub result_formats: Vec<Format>,
    /// A row-limited `Execute` (`max_rows > 0`) materializes the whole result
    /// here and delivers it in chunks across successive `Execute`s, sending
    /// `PortalSuspended` until it is drained. `None` until first suspended.
    pub suspended: Option<SuspendedRows>,
}

/// The remaining rows of a portal suspended by a row-limited `Execute`.
pub struct SuspendedRows {
    /// The rows not yet delivered, materialized when the portal first suspended.
    pub rows: Vec<Vec<Value>>,
    /// Index of the next row to deliver.
    pub next: usize,
    /// Total rows already delivered across every Execute of this portal, so the
    /// final CommandComplete reports the whole portal's `SELECT n`.
    pub delivered: usize,
}

/// The data-level state of an explicit `BEGIN … COMMIT/ROLLBACK` block. Separate
/// from [`TransactionStatus`], which is the wire control-flow byte: this holds
/// what MVCC needs.
pub struct ActiveTxn {
    /// The block's XID, allocated lazily on its first write. Read-only
    /// transactions never consume one, matching PostgreSQL.
    pub xid: Option<Xid>,
    pub iso: IsolationLevel,
    /// `READ ONLY` access mode: writes in this block are rejected with SQLSTATE
    /// 25006. Set from the block's transaction modes (or the session default).
    pub read_only: bool,
    /// REPEATABLE READ (and above) freeze one snapshot for the whole block, set
    /// on the first statement; READ COMMITTED leaves this `None` and takes a
    /// fresh snapshot per statement.
    pub snapshot: Option<Snapshot>,
    /// Command counter: each statement in the block runs at the next `cid`, so a
    /// later statement sees earlier ones' writes.
    pub cid: CommandId,
    /// Whether a snapshot-taking statement has run in this block. `SET
    /// TRANSACTION` may only change the isolation level before the first such
    /// query (PG raises 25001 afterwards).
    pub has_run_query: bool,
}

impl ActiveTxn {
    /// Open a block with the given isolation level and access mode (seeded from
    /// the session defaults, then overridden by the block's transaction modes).
    pub fn new(iso: IsolationLevel, read_only: bool) -> Self {
        ActiveTxn {
            xid: None,
            iso,
            read_only,
            snapshot: None,
            cid: CommandId::FIRST,
            has_run_query: false,
        }
    }
}

pub struct Session {
    /// Database and role accepted during startup. The server currently has one
    /// physical database, but these are still the current connection identity
    /// reported by information-schema metadata.
    pub database: String,
    pub user: String,
    /// Concrete namespace assigned to this connection's temporary relations.
    pub temp_schema: String,
    /// `extra_float_digits` GUC — controls float→text output precision.
    pub extra_float_digits: i32,
    /// `default_transaction_isolation` GUC — the isolation level a new block
    /// inherits when it names none. Set by `SET SESSION CHARACTERISTICS AS
    /// TRANSACTION …` or a plain `SET default_transaction_isolation = …`.
    pub default_iso: IsolationLevel,
    /// `default_transaction_read_only` GUC — the access mode a new block inherits
    /// when it names none.
    pub default_read_only: bool,
    /// Current transaction state, reported in every `ReadyForQuery`. `Idle`
    /// outside a block, `InTransaction` after `BEGIN`, `Failed` once a statement
    /// errors inside a block (only `COMMIT`/`ROLLBACK` clear it).
    pub tx_status: TransactionStatus,
    /// The data-level transaction backing an explicit block: `Some` between
    /// `BEGIN` and its `COMMIT`/`ROLLBACK`, `None` under autocommit (each
    /// statement is then its own implicit transaction).
    pub xact: Option<ActiveTxn>,
    /// The shared transaction manager, held so an abandoned block can be aborted
    /// when the session is dropped (see the [`Drop`] impl).
    pub txnmgr: Arc<TransactionManager>,
    /// Session-local temp-table catalog (PG's `pg_temp`). Searched before the
    /// shared global engine, so a `CREATE TEMP TABLE` shadows a same-named
    /// permanent table. Dropped with the session on disconnect — that is the
    /// temp tables' teardown.
    pub temp: Arc<dyn TableEngine>,
    /// Extended-protocol prepared statements, keyed by name (`""` = the unnamed
    /// statement, which `Parse` overwrites each time).
    pub prepared: HashMap<String, PreparedStatement>,
    /// Extended-protocol portals, keyed by name (`""` = the unnamed portal).
    pub portals: HashMap<String, Portal>,
}

impl Session {
    pub fn with_identity(
        txnmgr: Arc<TransactionManager>,
        database: impl Into<String>,
        user: impl Into<String>,
        temp_schema: impl Into<String>,
    ) -> Self {
        // PG's default since v12.
        Self {
            database: database.into(),
            user: user.into(),
            temp_schema: temp_schema.into(),
            extra_float_digits: 1,
            default_iso: IsolationLevel::ReadCommitted,
            default_read_only: false,
            tx_status: TransactionStatus::Idle,
            xact: None,
            txnmgr,
            temp: Arc::new(MemoryEngine::new()),
            prepared: HashMap::new(),
            portals: HashMap::new(),
        }
    }

    pub fn exec_context(&self) -> ExecContext {
        ExecContext {
            extra_float_digits: self.extra_float_digits,
        }
    }
}

impl Drop for Session {
    /// If the client disconnects with an explicit block still open, abort its
    /// XID so its writes become dead and the XID is retired from the in-flight
    /// set — otherwise it would pin the snapshot horizon forever and leave the
    /// rows it touched un-modifiable. Autocommit statements are already finalized
    /// at the statement boundary, so only an open block needs this.
    fn drop(&mut self) {
        if let Some(active) = self.xact.take() {
            self.txnmgr.abort(active.xid.unwrap_or(Xid::INVALID));
        }
    }
}
