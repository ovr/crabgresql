//! Per-connection session state: GUCs, the temp-table catalog, and the current
//! transaction. The wire-facing control-flow status (`tx_status`, the RFQ
//! `I`/`T`/`E` byte) and the data-level transaction ([`ActiveTxn`], the XID and
//! snapshot MVCC runs against) are tracked side by side.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crabgresql_executor::{ExecContext, ExecError, ExecNode, OutputColumn, SequenceOps};

use crate::query::RowTag;
use crabgresql_memory_storage::MemoryEngine;
use crabgresql_parser::ast;
use crabgresql_pg_wire::{Format, TransactionStatus, sqlstate};
use crabgresql_storage_api::{SequenceAdvance, TableEngine};
use crabgresql_txn::{CommandId, IsolationLevel, LockOwner, Snapshot, TransactionManager, Xid};
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
    /// A row-limited `Execute` (`max_rows > 0`) that did not exhaust the result
    /// keeps its live iterator here and resumes it on the next `Execute`, so the
    /// remaining rows are streamed lazily rather than buffered. `None` until the
    /// portal first suspends.
    pub suspended: Option<SuspendedRows>,
}

/// A portal suspended by a row-limited `Execute`: the still-running result
/// iterator and how many rows it has already delivered.
pub struct SuspendedRows {
    /// The result iterator, paused mid-stream. Owns its snapshot, so resuming it
    /// later sees the same rows.
    pub node: Box<dyn ExecNode>,
    /// Total rows already delivered across every `Execute` of this portal, so the
    /// final `CommandComplete` reports the whole portal's count.
    pub delivered: usize,
    /// The command-tag family to report when the portal exhausts (`SELECT n`, or
    /// a `RETURNING` DML's mutation tag).
    pub tag: RowTag,
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
    /// This connection's session-stable table-lock owner, stamped into every
    /// statement's `TxnContext` so a transaction can upgrade its own `AccessShare`
    /// hold to `AccessExclusive` (TRUNCATE a table it has an open cursor on)
    /// without self-deadlocking, while still blocking on other sessions' holds.
    pub lock_owner: LockOwner,
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
    /// Per-session `currval`/`lastval` state, updated by `nextval`/`setval`.
    /// Shared behind an `Arc<Mutex<_>>` so a [`SessionSequences`] handle (which
    /// the executor holds through [`ExecContext`], possibly in a suspended
    /// portal) can reach it without borrowing the session.
    pub seq_state: Arc<Mutex<SessionSeqState>>,
}

/// A session's `currval`/`lastval` state. `nextval`/`setval` write it; `currval`/
/// `lastval` read it. Keyed by the (short) sequence name.
#[derive(Default)]
pub struct SessionSeqState {
    currval: HashMap<String, i64>,
    lastval: Option<i64>,
}

/// The executor-facing sequence handle: routes `nextval`/`setval` to the engine's
/// non-transactional counters and maintains this session's `currval`/`lastval`.
/// Owns its `Arc`s so it can live in a suspended portal's [`ExecContext`].
pub struct SessionSequences {
    engine: Arc<dyn TableEngine>,
    state: Arc<Mutex<SessionSeqState>>,
}

impl SessionSequences {
    pub fn new(engine: Arc<dyn TableEngine>, state: Arc<Mutex<SessionSeqState>>) -> Self {
        Self { engine, state }
    }

    /// Map the engine's `NotFound` to PG's 42P01 for a sequence name.
    fn not_found(name: &str) -> ExecError {
        ExecError::new(
            sqlstate::UNDEFINED_TABLE,
            format!("relation \"{name}\" does not exist"),
        )
    }

    /// Build the 2200H message for a `NO CYCLE` bound, quoting the bound value as
    /// PostgreSQL does.
    fn limit_error(&self, name: &str, ascending: bool) -> ExecError {
        let bound = self.engine.sequence(name).map(|def| {
            if ascending {
                def.max
            } else {
                def.min
            }
        });
        let (edge, value) = if ascending {
            ("maximum", bound.unwrap_or(i64::MAX))
        } else {
            ("minimum", bound.unwrap_or(i64::MIN))
        };
        ExecError::new(
            sqlstate::SEQUENCE_GENERATOR_LIMIT_EXCEEDED,
            format!("nextval: reached {edge} value of sequence \"{name}\" ({value})"),
        )
    }
}

impl SequenceOps for SessionSequences {
    fn nextval(&self, name: &str) -> Result<i64, ExecError> {
        match self.engine.sequence_nextval(name) {
            SequenceAdvance::Value(v) => {
                let mut state = self.state.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
                state.currval.insert(name.to_string(), v);
                state.lastval = Some(v);
                Ok(v)
            }
            SequenceAdvance::NotFound => Err(Self::not_found(name)),
            SequenceAdvance::Overflow => Err(self.limit_error(name, true)),
            SequenceAdvance::Underflow => Err(self.limit_error(name, false)),
        }
    }

    fn currval(&self, name: &str) -> Result<i64, ExecError> {
        let state = self.state.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
        if let Some(v) = state.currval.get(name) {
            return Ok(*v);
        }
        // Not yet advanced in this session: a non-existent sequence is 42P01,
        // an existing-but-unused one is 55000.
        if self.engine.sequence(name).is_none() {
            Err(Self::not_found(name))
        } else {
            Err(ExecError::new(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("currval of sequence \"{name}\" is not yet defined in this session"),
            ))
        }
    }

    fn setval(&self, name: &str, value: i64, is_called: bool) -> Result<i64, ExecError> {
        match self.engine.sequence_setval(name, value, is_called) {
            SequenceAdvance::Value(v) => {
                let mut state = self.state.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
                state.currval.insert(name.to_string(), v);
                state.lastval = Some(v);
                Ok(v)
            }
            SequenceAdvance::NotFound => Err(Self::not_found(name)),
            // setval does not advance, so it cannot overflow.
            _ => Err(Self::not_found(name)),
        }
    }

    fn lastval(&self) -> Result<i64, ExecError> {
        self.state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .lastval
            .ok_or_else(|| {
                ExecError::new(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "lastval is not yet defined in this session",
                )
            })
    }
}

impl Session {
    pub fn with_identity(
        txnmgr: Arc<TransactionManager>,
        database: impl Into<String>,
        user: impl Into<String>,
        temp_schema: impl Into<String>,
    ) -> Self {
        // PG's default since v12.
        let lock_owner = txnmgr.new_lock_owner();
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
            lock_owner,
            temp: Arc::new(MemoryEngine::new()),
            prepared: HashMap::new(),
            portals: HashMap::new(),
            seq_state: Arc::new(Mutex::new(SessionSeqState::default())),
        }
    }

    /// The execution context with no sequence handle — for utility paths (e.g.
    /// `EXPLAIN`'s `Values` node) that never call a sequence function.
    pub fn exec_context(&self) -> ExecContext {
        ExecContext {
            extra_float_digits: self.extra_float_digits,
            sequences: None,
        }
    }

    /// The execution context wired to advance sequences through `engine`, used
    /// for the statement-execution path (so a `nextval` default or an explicit
    /// `nextval()` can run and update this session's `currval`/`lastval`).
    pub fn exec_context_with_sequences(&self, engine: &Arc<dyn TableEngine>) -> ExecContext {
        ExecContext {
            extra_float_digits: self.extra_float_digits,
            sequences: Some(Arc::new(SessionSequences::new(
                Arc::clone(engine),
                Arc::clone(&self.seq_state),
            ))),
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
            let xid = active.xid.unwrap_or(Xid::INVALID);
            if std::thread::panicking() {
                // We're unwinding from a panic. The engine finalize hook takes
                // engine locks that the same panic may have poisoned; re-entering
                // them here would be a fatal double-panic. Retire the XID without
                // the hook — any file a pending TRUNCATE staged is reclaimed by the
                // engine's orphan GC at the next startup.
                self.txnmgr.abort_without_finalize(xid);
            } else {
                self.txnmgr.abort(xid);
            }
        }
    }
}
