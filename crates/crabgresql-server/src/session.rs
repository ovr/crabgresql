//! Per-connection session state (GUCs). Grows a temp-table catalog in a later
//! milestone; for now it carries the settings runtime evaluation depends on.

use std::sync::Arc;

use crabgresql_executor::ExecContext;
use crabgresql_memory_storage::MemoryEngine;
use crabgresql_pg_wire::TransactionStatus;
use crabgresql_storage_api::TableEngine;

pub struct Session {
    /// `extra_float_digits` GUC — controls float→text output precision.
    pub extra_float_digits: i32,
    /// Current transaction state, reported in every `ReadyForQuery`. `Idle`
    /// outside a block, `InTransaction` after `BEGIN`, `Failed` once a statement
    /// errors inside a block (only `COMMIT`/`ROLLBACK` clear it). Real MVCC
    /// rollback of data is M2; this only tracks the control-flow state.
    pub tx_status: TransactionStatus,
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
