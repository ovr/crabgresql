//! Per-connection session state: GUCs, the temp-table catalog, and the current
//! transaction. The wire-facing control-flow status (`tx_status`, the RFQ
//! `I`/`T`/`E` byte) and the data-level transaction ([`ActiveTxn`], the XID and
//! snapshot MVCC runs against) are tracked side by side.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crabgresql_executor::{
    CatalogOps, ExecContext, ExecError, ExecNode, OutputColumn, SequenceOps,
};

use crate::query::RowTag;
use crabgresql_parser::ast;
use crabgresql_pg_wire::{Format, TransactionStatus, sqlstate};
use crabgresql_storage_api::{SequenceAdvance, TableEngine};
use crabgresql_txn::{CommandId, IsolationLevel, LockOwner, Snapshot, TransactionManager, Xid};
use crabgresql_types::{PgType, Value};

/// Base for a session's synthetic `pg_temp_N` namespace OID. A high reserved band
/// disjoint from the built-in namespace OIDs (`pg_catalog`=11, `pg_toast`=99,
/// `public`=2200) and user `CREATE SCHEMA` OIDs (from `FIRST_NORMAL_OBJECT_ID`
/// 16384 up), so `pg_class.relnamespace` and `pg_namespace` agree for temp tables
/// without persisting the temp schema. `backend_id + this` stays well below u32 max.
pub(crate) const TEMP_NAMESPACE_OID_BASE: u32 = 0x7000_0000;

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
    /// Synthetic OID reflected for [`Session::temp_schema`] in `pg_namespace` and
    /// `pg_class.relnamespace` once the session instantiates a temp relation. A
    /// high reserved value (see [`TEMP_NAMESPACE_OID_BASE`]) disjoint from built-in
    /// and user-schema OIDs, so the reflection is consistent without persisting the
    /// temp schema.
    pub temp_namespace_oid: u32,
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
    /// The shared engine, held so the session can drop this connection's temp
    /// tables (memory tables in its `pg_temp_N` namespace) at disconnect — the
    /// temp tables' teardown (see the [`Drop`] impl).
    pub engine: Arc<dyn TableEngine>,
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
/// `lastval` read it. Keyed by the canonical qualified name `"namespace.name"`
/// (namespace resolved to `public` when the reference was unqualified) so
/// `currval('s')` and `currval('app.s')` never collide.
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
    /// Whether the current statement runs read-only: `nextval`/`setval` mutate,
    /// so they are rejected (25006) just like a DML write.
    read_only: bool,
}

impl SessionSequences {
    pub fn new(
        engine: Arc<dyn TableEngine>,
        state: Arc<Mutex<SessionSeqState>>,
        read_only: bool,
    ) -> Self {
        Self {
            engine,
            state,
            read_only,
        }
    }

    /// Resolve the reference's schema: the written qualifier, or `public` when
    /// unqualified (a real search_path is a follow-up).
    fn resolve_ns(namespace: Option<&str>) -> &str {
        namespace.unwrap_or("public")
    }

    /// The name as the caller wrote it (`app.s` when qualified, else `s`), for
    /// error text that quotes the reference.
    fn display(namespace: Option<&str>, name: &str) -> String {
        match namespace {
            Some(ns) => format!("{ns}.{name}"),
            None => name.to_string(),
        }
    }

    /// Canonical `currval`/`lastval` state key: the namespace resolved to `public`
    /// when unqualified, so `s` and `public.s` share one entry.
    fn state_key(namespace: Option<&str>, name: &str) -> String {
        format!("{}.{}", Self::resolve_ns(namespace), name)
    }

    /// The engine's `NotFound` maps to PG's 42P01 for a truly absent name, but to
    /// 42809 `"x" is not a sequence` when the name is a live table or view — PG
    /// opens the relation and rejects the wrong relkind.
    fn not_found(&self, namespace: Option<&str>, name: &str) -> ExecError {
        let ns = Self::resolve_ns(namespace);
        let display = Self::display(namespace, name);
        if self.engine.resolve(Some(ns), name).is_ok()
            || self.engine.resolve_view(Some(ns), name).is_some()
        {
            ExecError::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("\"{display}\" is not a sequence"),
            )
        } else {
            ExecError::new(
                sqlstate::UNDEFINED_TABLE,
                format!("relation \"{display}\" does not exist"),
            )
        }
    }

    /// PG's 25006 for a sequence-advancing function in a read-only transaction.
    fn read_only_error(func: &str) -> ExecError {
        ExecError::new(
            sqlstate::READ_ONLY_SQL_TRANSACTION,
            format!("cannot execute {func}() in a read-only transaction"),
        )
    }

    /// Build the 2200H message for a `NO CYCLE` bound, quoting the bound value as
    /// PostgreSQL does.
    fn limit_error(&self, namespace: Option<&str>, name: &str, ascending: bool) -> ExecError {
        let ns = Self::resolve_ns(namespace);
        let display = Self::display(namespace, name);
        let bound = self.engine.sequence(ns, name).map(|def| {
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
            format!("nextval: reached {edge} value of sequence \"{display}\" ({value})"),
        )
    }
}

impl SequenceOps for SessionSequences {
    fn nextval(&self, namespace: Option<&str>, name: &str) -> Result<i64, ExecError> {
        if self.read_only {
            return Err(Self::read_only_error("nextval"));
        }
        let ns = Self::resolve_ns(namespace);
        match self.engine.sequence_nextval(ns, name) {
            SequenceAdvance::Value(v) => {
                let mut state = self.state.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
                state.currval.insert(Self::state_key(namespace, name), v);
                state.lastval = Some(v);
                Ok(v)
            }
            SequenceAdvance::NotFound => Err(self.not_found(namespace, name)),
            SequenceAdvance::Overflow => Err(self.limit_error(namespace, name, true)),
            SequenceAdvance::Underflow => Err(self.limit_error(namespace, name, false)),
        }
    }

    fn currval(&self, namespace: Option<&str>, name: &str) -> Result<i64, ExecError> {
        // The sequence must still exist — PG opens the relation — so a dropped
        // one is 42P01 (or 42809 for a wrong relkind) even if this session
        // advanced it earlier; only then is the session's cached value consulted.
        let ns = Self::resolve_ns(namespace);
        if self.engine.sequence(ns, name).is_none() {
            return Err(self.not_found(namespace, name));
        }
        let state = self.state.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
        match state.currval.get(&Self::state_key(namespace, name)) {
            Some(v) => Ok(*v),
            None => Err(ExecError::new(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!(
                    "currval of sequence \"{}\" is not yet defined in this session",
                    Self::display(namespace, name)
                ),
            )),
        }
    }

    fn setval(
        &self,
        namespace: Option<&str>,
        name: &str,
        value: i64,
        is_called: bool,
    ) -> Result<i64, ExecError> {
        if self.read_only {
            return Err(Self::read_only_error("setval"));
        }
        let ns = Self::resolve_ns(namespace);
        let Some(def) = self.engine.sequence(ns, name) else {
            return Err(self.not_found(namespace, name));
        };
        if value < def.min || value > def.max {
            return Err(ExecError::new(
                sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
                format!(
                    "setval: value {value} is out of bounds for sequence \"{}\" ({}..{})",
                    Self::display(namespace, name),
                    def.min,
                    def.max
                ),
            ));
        }
        match self.engine.sequence_setval(ns, name, value, is_called) {
            SequenceAdvance::Value(v) => {
                let mut state = self.state.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
                state.currval.insert(Self::state_key(namespace, name), v);
                // setval does NOT define lastval: PG's lastval reflects only the
                // most recent nextval in the session.
                Ok(v)
            }
            // Concurrently dropped between the existence check and the write.
            _ => Err(self.not_found(namespace, name)),
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
        engine: Arc<dyn TableEngine>,
        database: impl Into<String>,
        user: impl Into<String>,
        temp_schema: impl Into<String>,
        temp_namespace_oid: u32,
    ) -> Self {
        // PG's default since v12.
        let lock_owner = txnmgr.new_lock_owner();
        Self {
            database: database.into(),
            user: user.into(),
            temp_schema: temp_schema.into(),
            temp_namespace_oid,
            extra_float_digits: 1,
            default_iso: IsolationLevel::ReadCommitted,
            default_read_only: false,
            tx_status: TransactionStatus::Idle,
            xact: None,
            txnmgr,
            lock_owner,
            engine,
            prepared: HashMap::new(),
            portals: HashMap::new(),
            seq_state: Arc::new(Mutex::new(SessionSeqState::default())),
        }
    }

    /// The execution context with no sequence or catalog handle — for utility
    /// paths (e.g. `EXPLAIN`'s `Values` node) that never call either family.
    pub fn exec_context(&self) -> ExecContext {
        ExecContext {
            extra_float_digits: self.extra_float_digits,
            sequences: None,
            catalog: None,
            txn: None,
        }
    }

    /// The execution context for the statement-execution path: wired to advance
    /// sequences through `engine` (so a `nextval` default or an explicit
    /// `nextval()` can run and update this session's `currval`/`lastval`) and to
    /// read `catalog` (so `pg_get_userbyid` / `pg_table_is_visible` resolve
    /// against this statement's catalog snapshot).
    pub fn exec_context_for_statement(
        &self,
        engine: &Arc<dyn TableEngine>,
        catalog: &Arc<dyn CatalogOps>,
        read_only: bool,
    ) -> ExecContext {
        ExecContext {
            extra_float_digits: self.extra_float_digits,
            sequences: Some(Arc::new(SessionSequences::new(
                Arc::clone(engine),
                Arc::clone(&self.seq_state),
                read_only,
            ))),
            catalog: Some(Arc::clone(catalog)),
            txn: None,
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
        // Drop this connection's temp tables (memory tables in its `pg_temp_N`
        // namespace). They now live in the shared, process-lifetime engine, so this
        // is what reclaims them — run it even while panicking, otherwise a panicking
        // connection leaks its temp tables and their RAM until process exit. Isolate
        // it in `catch_unwind` so an engine lock the same panic poisoned turns into a
        // skipped cleanup, not a fatal double-panic abort.
        let engine = Arc::clone(&self.engine);
        let temp_schema = self.temp_schema.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            // O(this session's temp tables), reading just their names — not a
            // deep clone of every schema in the cluster.
            for name in engine.relation_names_in(&temp_schema) {
                let _ = engine.drop_table(&temp_schema, &name);
            }
        }));
    }
}
