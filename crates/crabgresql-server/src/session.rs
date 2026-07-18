//! Per-connection session state: GUCs, the temp-table catalog, and the current
//! transaction. The wire-facing control-flow status (`tx_status`, the RFQ
//! `I`/`T`/`E` byte) and the data-level transaction ([`ActiveTxn`], the XID and
//! snapshot MVCC runs against) are tracked side by side.

use std::sync::Arc;

use crabgresql_executor::ExecContext;
use crabgresql_memory_storage::MemoryEngine;
use crabgresql_pg_wire::TransactionStatus;
use crabgresql_storage_api::TableEngine;
use crabgresql_txn::{CommandId, IsolationLevel, Snapshot, Xid};

/// The data-level state of an explicit `BEGIN … COMMIT/ROLLBACK` block. Separate
/// from [`TransactionStatus`], which is the wire control-flow byte: this holds
/// what MVCC needs.
pub struct ActiveTxn {
    /// The block's XID, allocated lazily on its first write. Read-only
    /// transactions never consume one, matching PostgreSQL.
    pub xid: Option<Xid>,
    pub iso: IsolationLevel,
    /// REPEATABLE READ (and above) freeze one snapshot for the whole block, set
    /// on the first statement; READ COMMITTED leaves this `None` and takes a
    /// fresh snapshot per statement.
    pub snapshot: Option<Snapshot>,
    /// Command counter: each statement in the block runs at the next `cid`, so a
    /// later statement sees earlier ones' writes.
    pub cid: CommandId,
}

impl ActiveTxn {
    pub fn new(iso: IsolationLevel) -> Self {
        ActiveTxn {
            xid: None,
            iso,
            snapshot: None,
            cid: CommandId::FIRST,
        }
    }
}

pub struct Session {
    /// `extra_float_digits` GUC — controls float→text output precision.
    pub extra_float_digits: i32,
    /// Current transaction state, reported in every `ReadyForQuery`. `Idle`
    /// outside a block, `InTransaction` after `BEGIN`, `Failed` once a statement
    /// errors inside a block (only `COMMIT`/`ROLLBACK` clear it).
    pub tx_status: TransactionStatus,
    /// The data-level transaction backing an explicit block: `Some` between
    /// `BEGIN` and its `COMMIT`/`ROLLBACK`, `None` under autocommit (each
    /// statement is then its own implicit transaction).
    pub xact: Option<ActiveTxn>,
    /// Session-local temp-table catalog (PG's `pg_temp`). Searched before the
    /// shared global engine, so a `CREATE TEMP TABLE` shadows a same-named
    /// permanent table. Dropped with the session on disconnect — that is the
    /// temp tables' teardown.
    pub temp: Arc<dyn TableEngine>,
}

impl Session {
    pub fn new() -> Self {
        // PG's default since v12.
        Self {
            extra_float_digits: 1,
            tx_status: TransactionStatus::Idle,
            xact: None,
            temp: Arc::new(MemoryEngine::new()),
        }
    }

    pub fn exec_context(&self) -> ExecContext {
        ExecContext {
            extra_float_digits: self.extra_float_digits,
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}
