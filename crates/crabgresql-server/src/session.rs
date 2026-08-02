//! Per-connection session state: GUCs, the temp-table catalog, and the current
//! transaction. The wire-facing control-flow status (`tx_status`, the RFQ
//! `I`/`T`/`E` byte) and the data-level transaction ([`ActiveTxn`], the XID and
//! snapshot MVCC runs against) are tracked side by side.

use std::collections::HashMap;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};

use crabgresql_executor::{
    CatalogOps, ExecContext, ExecError, ExecNode, GucOps, NoticeSink, OutputColumn, RoutineOps,
    SequenceOps,
};
use crabgresql_plpgsql::RoutineCache;

use crate::error::PgError;
use crate::guc;
use crate::query::RowTag;
use crate::routines::SessionNotices;
use crabgresql_parser::ast;
use crabgresql_pg_wire::{Format, TransactionStatus, sqlstate};
use crabgresql_storage_api::{SequenceAdvance, TableEngine, Tuple};
use crabgresql_txn::{
    CommandId, IsolationLevel, LockOwner, Snapshot, SnapshotGuard, TransactionManager, Xid,
};
use crabgresql_types::fmt::Clock;
use crabgresql_types::tz::SessionZone;
use crabgresql_types::{FmtCtx, PgType, Value};

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
    /// Where this portal is in its life: never run, paused mid-result, or
    /// finished. A finished portal must never re-run its statement.
    pub state: PortalState,
}

/// How far a portal has got. A portal is executed at most once: PG answers a
/// second `Execute` of a finished portal from the portal's recorded state rather
/// than running the statement again, which for a data-modifying statement would
/// apply its writes twice.
pub enum PortalState {
    /// Bound but not yet executed.
    Ready,
    /// A row-limited `Execute` (`max_rows > 0`) did not exhaust the result, so the
    /// live iterator waits here and the next `Execute` resumes it — streaming,
    /// rather than buffering the remainder.
    Suspended(SuspendedRows),
    /// Ran to completion. `tag` is the command-tag family of the result set it
    /// produced, or `None` for a statement that produced no result set at all
    /// (a plain `INSERT`/`UPDATE`/`DELETE`, DDL, `SET`): PG re-reports an
    /// exhausted result set as an empty one, but refuses to re-run the latter.
    Done { tag: Option<RowTag> },
}

impl PortalState {
    /// Whether a further `Execute` resumes a paused result.
    pub fn is_suspended(&self) -> bool {
        matches!(self, PortalState::Suspended(_))
    }
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

/// Which way a `FETCH`/`MOVE` asks a cursor to travel, with every surface
/// spelling (`NEXT`, `FIRST`, `FORWARD ALL`, a bare count, …) already folded
/// into one of four shapes. `None` for a count means `ALL`.
///
/// `Forward`/`Backward` deliver *every* row they pass over; `Absolute` and
/// `Relative` land on one row and deliver only that one. That asymmetry is
/// PostgreSQL's, and it is why `FETCH 3` returns three rows while `FETCH
/// RELATIVE 3` returns one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorMove {
    /// `FETCH n` / `NEXT` / `FORWARD [n | ALL]` / `ALL`.
    Forward(Option<i64>),
    /// `PRIOR` / `BACKWARD [n | ALL]`.
    Backward(Option<i64>),
    /// `ABSOLUTE n` / `FIRST` (= 1) / `LAST` (= −1). Negative counts from the end.
    Absolute(i64),
    /// `RELATIVE n`.
    Relative(i64),
}

/// A SQL cursor opened by `DECLARE … CURSOR`.
///
/// The rows are materialised at `DECLARE` time rather than streamed. The
/// executor's [`ExecNode`] is forward-only with no rewind, and a statement's
/// transaction is finalised before its rows are pulled, so holding a live
/// iterator across statements would need per-portal transaction lifetimes.
/// Materialising instead pins the `DECLARE`-time snapshot (which is what
/// PostgreSQL guarantees), makes `SCROLL` free, and makes `WITH HOLD` free —
/// PostgreSQL itself spools a holdable cursor into a tuplestore at commit. The
/// cost is the whole result set in memory.
///
/// Deliberately *not* the same map as [`Session::portals`]. PostgreSQL unifies
/// SQL cursors with extended-protocol portals; keeping them apart here avoids
/// reworking [`PortalState`], and no ordinary client can tell the difference.
pub struct Cursor {
    /// The result-set shape, reported by every `FETCH` and by `Describe`.
    pub columns: Vec<OutputColumn>,
    pub rows: Vec<Tuple>,
    /// `0` = before the first row, `1..=rows.len()` = on that row,
    /// `rows.len() + 1` = after the last. PostgreSQL's portal position model:
    /// the cursor sits *between* rows at both ends, which is what makes
    /// `FETCH PRIOR` after exhaustion return the last row.
    pub pos: usize,
    /// `WITH HOLD`: survives the commit of the block that declared it.
    pub hold: bool,
    /// `Some(false)` = explicit `NO SCROLL`, which rejects any backward
    /// movement. `None` (unspecified) and `Some(true)` both allow it — every
    /// materialised cursor is trivially scrollable.
    pub scroll: Option<bool>,
    /// The `DECLARE` statement, re-rendered from its AST, for
    /// `pg_cursors.statement`.
    pub statement: String,
    /// Declared inside an explicit `BEGIN` block. `ROLLBACK` closes those even
    /// when they are holdable; a holdable cursor declared under autocommit
    /// belongs to no block and survives later rollbacks.
    pub in_block: bool,
    /// The `DECLARE`'s statement timestamp, for `pg_cursors.creation_time`.
    pub creation_time: i64,
}

impl Cursor {
    /// Reposition by `movement` and return the rows traversed, in delivery
    /// order (reversed for backward movement, as PostgreSQL delivers them).
    ///
    /// `Err` is the `NO SCROLL` rejection; every other outcome is a legal move,
    /// including one that lands off either end and returns nothing.
    pub fn fetch(&mut self, movement: CursorMove) -> Result<Vec<Tuple>, PgError> {
        let (from, to) = self.step(movement)?;
        let (lo, hi, reverse) = self.delivered(from, to, movement);
        // `1..=len` are the row positions; `0` and `len + 1` are the gaps at
        // either end and index nothing, so the range is filtered, not clamped.
        let mut rows: Vec<Tuple> = (lo..=hi).filter_map(|at| self.row_at(at)).collect();
        if reverse {
            rows.reverse();
        }
        Ok(rows)
    }

    /// [`Cursor::fetch`] without the rows: how many it *would* have returned,
    /// which is the count `MOVE` reports.
    ///
    /// Counted from the traversed range rather than by building the rows —
    /// `MOVE ALL` over a large cursor would otherwise clone the whole result set
    /// only to read its length.
    pub fn advance(&mut self, movement: CursorMove) -> Result<usize, PgError> {
        let (from, to) = self.step(movement)?;
        let (lo, hi, _) = self.delivered(from, to, movement);
        // The end gaps hold no row, so intersect the range with the row band
        // rather than counting the positions it nominally spans.
        let (lo, hi) = (lo.max(1), hi.min(self.rows.len()));
        Ok(hi.saturating_sub(lo) + usize::from(lo <= hi))
    }

    /// Apply `movement`: reject it if the cursor may not travel that way, else
    /// move and report the positions travelled between.
    fn step(&mut self, movement: CursorMove) -> Result<(usize, usize), PgError> {
        let from = self.pos;
        let to = self.target(movement);
        if self.scroll == Some(false) && self.rewinds(movement, from, to) {
            return Err(PgError::new(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "cursor can only scan forward",
            )
            .with_hint("Declare it with SCROLL option to enable backward scan."));
        }
        self.pos = to;
        Ok((from, to))
    }

    /// Whether `movement` asks the cursor to travel backward — the request a
    /// `NO SCROLL` cursor refuses.
    ///
    /// This is a property of the *request*, not of where it happens to land, and
    /// PostgreSQL draws three lines that the landing position alone does not:
    ///
    /// - `BACKWARD` always rewinds, even `BACKWARD 0` and even at the start,
    ///   where the move is a no-op.
    /// - `ABSOLUTE` with a negative count seeks from the end, so it rewinds even
    ///   when the row it lands on is ahead of the cursor.
    /// - A zero-distance re-fetch (`FETCH 0`, `RELATIVE 0`, `FORWARD 0`) is a
    ///   step back and forward again, so it rewinds — but only while the cursor
    ///   is *on* a row. Resting in either end gap it yields nothing and never
    ///   moves, which is why PostgreSQL allows it there.
    fn rewinds(&self, movement: CursorMove, from: usize, to: usize) -> bool {
        let zero_distance = || (1..=self.rows.len()).contains(&from);
        match movement {
            CursorMove::Forward(None) => false,
            CursorMove::Backward(None) => true,
            CursorMove::Backward(Some(n)) => n >= 0,
            CursorMove::Forward(Some(n)) | CursorMove::Relative(n) => {
                n < 0 || (n == 0 && zero_distance())
            }
            CursorMove::Absolute(n) if n < 0 => true,
            CursorMove::Absolute(_) => to < from || (to == from && zero_distance()),
        }
    }

    /// The position `movement` lands on, clamped to `0..=rows.len() + 1`.
    fn target(&self, movement: CursorMove) -> usize {
        let len = self.rows.len() as i64;
        let pos = self.pos as i64;
        let landing = match movement {
            CursorMove::Forward(None) => len + 1,
            CursorMove::Backward(None) => 0,
            // A negative count reverses the direction: `FETCH -3` is `FETCH
            // BACKWARD 3`, and `FETCH BACKWARD -3` is `FETCH FORWARD 3`.
            CursorMove::Forward(Some(n)) => pos.saturating_add(n),
            CursorMove::Backward(Some(n)) => pos.saturating_sub(n),
            CursorMove::Relative(n) => pos.saturating_add(n),
            // `ABSOLUTE 0` is "before the first row"; a negative count is from
            // the end, so `ABSOLUTE -1` is the last row.
            CursorMove::Absolute(n) if n >= 0 => n,
            CursorMove::Absolute(n) => (len + 1).saturating_add(n),
        };
        landing.clamp(0, len + 1) as usize
    }

    /// The inclusive position range a move from `from` to `to` delivers, and
    /// whether it comes out in reverse.
    ///
    /// `Absolute`/`Relative` land on one row and deliver only that one;
    /// `Forward`/`Backward` deliver everything they pass over. That asymmetry is
    /// PostgreSQL's, and it is why `FETCH 3` returns three rows while `FETCH
    /// RELATIVE 3` returns one. A zero-distance move re-reads the row the cursor
    /// already sits on.
    fn delivered(&self, from: usize, to: usize, movement: CursorMove) -> (usize, usize, bool) {
        let single = matches!(movement, CursorMove::Absolute(_) | CursorMove::Relative(_));
        if single || from == to {
            return (to, to, false);
        }
        match to > from {
            true => (from + 1, to, false),
            // `to < from` here, so `from` is at least 1 and cannot underflow.
            false => (to, from - 1, true),
        }
    }

    /// The row at a 1-based position, or `None` for the two end gaps.
    fn row_at(&self, pos: usize) -> Option<Tuple> {
        self.rows.get(pos.checked_sub(1)?).cloned()
    }
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
    ///
    /// The [`SnapshotGuard`] rides along so the block's snapshot stays counted in
    /// the manager's live-snapshot registry for the block's whole life, not just
    /// the statement that took it — the statement's own guard lives on its
    /// `TxnContext` and dies with it. A read-only block allocates no XID and so
    /// does not move `Snapshot::xmin`, which makes this registration the only
    /// thing holding reclamation off the versions the block is still entitled to
    /// read. Pairing the two in one field is what keeps them from drifting apart.
    pub snapshot: Option<(Snapshot, SnapshotGuard)>,
    /// Command counter: each statement in the block runs at the next `cid`, so a
    /// later statement sees earlier ones' writes.
    pub cid: CommandId,
    /// Whether a snapshot-taking statement has run in this block. `SET
    /// TRANSACTION` may only change the isolation level before the first such
    /// query (PG raises 25001 afterwards).
    pub has_run_query: bool,
    /// When the block started, backing `now()`/`transaction_timestamp()` and
    /// the `'now'` input special for every statement in it. Taken from the
    /// `BEGIN`'s own message stamp rather than read fresh, so the block's clock
    /// is the moment the client asked for a block.
    pub xact_start: i64,
}

impl ActiveTxn {
    /// Open a block with the given isolation level and access mode (seeded from
    /// the session defaults, then overridden by the block's transaction modes).
    pub fn new(iso: IsolationLevel, read_only: bool, xact_start: i64) -> Self {
        ActiveTxn {
            xid: None,
            iso,
            read_only,
            snapshot: None,
            cid: CommandId::FIRST,
            has_run_query: false,
            xact_start,
        }
    }
}

/// A configuration parameter's value from before the current transaction block
/// changed it, captured verbatim (see [`guc::SavedValue`] for why it is not the
/// rendered form).
struct SavedGuc {
    def: &'static guc::GucDef,
    /// Set by `SET LOCAL`, which reverts on commit as well as rollback.
    local: bool,
    value: guc::SavedValue,
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
    /// `TimeZone` GUC — the display zone every `timestamptz` is read and
    /// rendered in. Behind an `Arc` because it is cloned into the formatting
    /// context of every statement and every plan node.
    pub timezone: Arc<SessionZone>,
    /// When the protocol message being processed arrived, backing
    /// `statement_timestamp()`. Stamped once per message — not per statement —
    /// so every statement of a multi-statement simple query shares it, as in
    /// PostgreSQL. Outside a block it is also the transaction start.
    pub stmt_start: i64,
    /// `default_transaction_isolation` GUC — the isolation level a new block
    /// inherits when it names none. Set by `SET SESSION CHARACTERISTICS AS
    /// TRANSACTION …` or a plain `SET default_transaction_isolation = …`.
    pub default_iso: IsolationLevel,
    /// `default_transaction_read_only` GUC — the access mode a new block inherits
    /// when it names none.
    pub default_read_only: bool,
    /// Configuration parameters changed inside the current transaction block,
    /// with the value each held before the *first* change — see
    /// [`Session::save_guc_for_transaction`].
    saved_gucs: Vec<SavedGuc>,
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
    /// Open SQL cursors (`DECLARE … CURSOR`), keyed by name. Reflected into
    /// `pg_catalog.pg_cursors`. Closed by `CLOSE`, by the end of the block that
    /// declared them (unless `WITH HOLD`), and — with the rest of the session —
    /// by the [`Drop`] impl, which needs no help because the rows are plain
    /// owned memory.
    pub cursors: HashMap<String, Cursor>,
    /// Per-session `currval`/`lastval` state, updated by `nextval`/`setval`.
    /// Shared behind an `Arc<Mutex<_>>` so a [`SessionSequences`] handle (which
    /// the executor holds through [`ExecContext`], possibly in a suspended
    /// portal) can reach it without borrowing the session.
    pub seq_state: Arc<Mutex<SessionSeqState>>,
    /// Diagnostics raised mid-execution (`RAISE NOTICE` inside a routine body),
    /// drained by the connection layer as it writes rows.
    pub notices: Arc<SessionNotices>,
    /// Compiled PL/pgSQL bodies, shared for the session's lifetime. Keyed by
    /// catalog OID; needs no invalidation because OIDs are never reused and a
    /// routine's body is fixed once created.
    pub routine_cache: Arc<RoutineCache>,
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
        let bound = self
            .engine
            .sequence(ns, name)
            .map(|def| if ascending { def.max } else { def.min });
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

/// The statement's GUC values, frozen at the start of execution. See
/// [`crate::guc::snapshot`] for why a snapshot is enough.
struct GucSnapshot(std::collections::HashMap<String, String>);

impl GucOps for GucSnapshot {
    fn show(&self, name: &str) -> Option<String> {
        self.0.get(&name.to_ascii_lowercase()).cloned()
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
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|_| panic!("mutex poisoned"));
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
        let state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
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
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|_| panic!("mutex poisoned"));
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
            // PG's boot value is the host zone; ours is UTC, which keeps every
            // expected output in the test suites stable.
            timezone: Arc::new(SessionZone::utc()),
            // Restamped by every incoming message; seeded here so the value is
            // never an invented instant even before the first one arrives.
            stmt_start: crabgresql_types::tz::now_micros(),
            saved_gucs: Vec::new(),
            default_iso: IsolationLevel::ReadCommitted,
            default_read_only: false,
            tx_status: TransactionStatus::Idle,
            xact: None,
            txnmgr,
            lock_owner,
            engine,
            prepared: HashMap::new(),
            portals: HashMap::new(),
            cursors: HashMap::new(),
            seq_state: Arc::new(Mutex::new(SessionSeqState::default())),
            notices: Arc::new(SessionNotices::default()),
            routine_cache: Arc::new(RoutineCache::new()),
        }
    }

    /// The execution context with no sequence or catalog handle — for utility
    /// paths (e.g. `EXPLAIN`'s `Values` node) that never call either family.
    /// Record a parameter's pre-change value so the transaction can put it back.
    ///
    /// PostgreSQL's configuration parameters are transactional. A plain `SET`
    /// inside a block is undone by `ROLLBACK` but survives `COMMIT`; a
    /// `SET LOCAL` is undone by *either*. Both are the same save-stack with
    /// different restore predicates, which is what `local` records.
    ///
    /// Only the first change to a given parameter in a block is saved — later
    /// ones would overwrite the original with an already-modified value. Called
    /// before the setter runs, and a no-op outside a block, where there is
    /// nothing to restore to.
    pub fn save_guc_for_transaction(&mut self, def: &'static guc::GucDef, local: bool) {
        if self.tx_status == TransactionStatus::Idle {
            return;
        }
        if let Some(saved) = self.saved_gucs.iter_mut().find(|s| s.def.key == def.key) {
            // Already saved. A later `SET LOCAL` over a plain `SET` still has to
            // revert at commit, so the flag is sticky once set.
            saved.local |= local;
            return;
        }
        // Nothing to restore for a parameter whose value cannot change.
        let Some(value) = def.capture(self) else {
            return;
        };
        self.saved_gucs.push(SavedGuc { def, local, value });
    }

    /// Undo the block's parameter changes as it ends: everything on `ROLLBACK`,
    /// only the `SET LOCAL`s on `COMMIT`.
    pub fn restore_gucs_at_transaction_end(&mut self, committed: bool) {
        for saved in std::mem::take(&mut self.saved_gucs) {
            if committed && !saved.local {
                continue;
            }
            saved.def.restore(self, saved.value);
        }
    }

    /// The formatting context for this session: `extra_float_digits`, the
    /// display zone and the clock, as the value layer wants them.
    ///
    /// Under autocommit there is no block to take a transaction start from, so
    /// the statement's own stamp serves as both — which is what makes
    /// `now() = statement_timestamp()` outside a block, as in PostgreSQL.
    pub fn fmt_ctx(&self) -> FmtCtx {
        let xact_start = self.xact.as_ref().map_or(self.stmt_start, |x| x.xact_start);
        FmtCtx::new(
            self.extra_float_digits,
            Arc::clone(&self.timezone),
            Clock {
                xact_start,
                stmt_start: self.stmt_start,
            },
        )
    }

    /// Stamp the arrival of a protocol message. Called once per `Query`,
    /// `Parse`, `Bind` and `Execute` — never per statement inside a
    /// multi-statement simple query, which PostgreSQL treats as one stamp.
    pub fn stamp_message(&mut self) {
        self.stmt_start = crabgresql_types::tz::now_micros();
    }

    /// A context for evaluation outside a statement's normal execution path —
    /// `EXPLAIN`'s constant rendering, a folded partition bound. It carries the
    /// session's formatting *and* its GUC values, so a `current_setting()` in
    /// one of those positions answers rather than raising an internal error.
    pub fn exec_context(&self) -> ExecContext {
        ExecContext {
            fmt: self.fmt_ctx(),
            sequences: None,
            catalog: None,
            gucs: Some(Arc::new(GucSnapshot(crate::guc::snapshot(self)))),
            txn: None,
            ..ExecContext::default()
        }
    }

    /// The execution context for the statement-execution path: wired to advance
    /// sequences through `engine` (so a `nextval` default or an explicit
    /// `nextval()` can run and update this session's `currval`/`lastval`) and to
    /// read `catalog` (so `pg_get_userbyid` / `pg_table_is_visible` resolve
    /// against this statement's catalog snapshot).
    #[allow(clippy::too_many_arguments)]
    pub fn exec_context_for_statement(
        &self,
        engine: &Arc<dyn TableEngine>,
        catalog: &Arc<dyn CatalogOps>,
        routines: Arc<dyn RoutineOps>,
        command_counter: Arc<AtomicU32>,
        read_only: bool,
    ) -> ExecContext {
        ExecContext {
            fmt: self.fmt_ctx(),
            sequences: Some(Arc::new(SessionSequences::new(
                Arc::clone(engine),
                Arc::clone(&self.seq_state),
                read_only,
            ))),
            catalog: Some(Arc::clone(catalog)),
            gucs: Some(Arc::new(GucSnapshot(crate::guc::snapshot(self)))),
            txn: None,
            routines: Some(routines),
            notices: Some(Arc::clone(&self.notices) as Arc<dyn NoticeSink>),
            read_only,
            call_depth: 0,
            command_counter: Some(command_counter),
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

#[cfg(test)]
mod cursor_tests {
    use super::*;

    /// A cursor over `1..=10`, positioned before the first row.
    fn cursor(scroll: Option<bool>) -> Cursor {
        Cursor {
            columns: Vec::new(),
            rows: (1..=10).map(|n| vec![Value::Int4(n)]).collect(),
            pos: 0,
            hold: false,
            scroll,
            statement: String::new(),
            in_block: true,
            creation_time: 0,
        }
    }

    /// The integers a fetch delivered, so expectations read as the rows do.
    fn ints(rows: Vec<Tuple>) -> Result<Vec<i32>, PgError> {
        rows.into_iter()
            .map(|row| match row.as_slice() {
                [Value::Int4(n)] => Ok(*n),
                other => Err(PgError::new(
                    sqlstate::INTERNAL_ERROR,
                    format!("unexpected row {other:?}"),
                )),
            })
            .collect()
    }

    /// The exact sequence probed against PostgreSQL 18.4: three forward, one
    /// back, a two-row MOVE, then the next row.
    #[test]
    fn forward_backward_and_move_track_position() -> Result<(), PgError> {
        let mut c = cursor(None);
        assert_eq!(ints(c.fetch(CursorMove::Forward(Some(3)))?)?, [1, 2, 3]);
        assert_eq!(ints(c.fetch(CursorMove::Backward(Some(1)))?)?, [2]);
        assert_eq!(c.advance(CursorMove::Forward(Some(2)))?, 2);
        assert_eq!(ints(c.fetch(CursorMove::Forward(Some(1)))?)?, [5]);
        Ok(())
    }

    /// `ALL` in both directions reports what it passed over, not the row count:
    /// from row 3 of 10, `MOVE ALL` is 7 and `MOVE BACKWARD ALL` is then 10.
    #[test]
    fn all_moves_to_either_end() -> Result<(), PgError> {
        let mut c = cursor(None);
        assert_eq!(c.advance(CursorMove::Forward(Some(3)))?, 3);
        assert_eq!(c.advance(CursorMove::Forward(None))?, 7);
        assert_eq!(c.pos, 11);
        assert_eq!(c.advance(CursorMove::Backward(None))?, 10);
        assert_eq!(c.pos, 0);
        Ok(())
    }

    /// `MOVE` counts exactly what the matching `FETCH` would have returned, for
    /// every direction — it derives the count from the traversed range rather
    /// than building the rows, so the two could drift apart.
    #[test]
    fn move_counts_match_what_fetch_would_deliver() -> Result<(), PgError> {
        let movements = [
            CursorMove::Forward(Some(3)),
            CursorMove::Forward(None),
            CursorMove::Backward(Some(4)),
            CursorMove::Backward(None),
            CursorMove::Absolute(6),
            CursorMove::Absolute(-2),
            CursorMove::Absolute(0),
            CursorMove::Relative(0),
            CursorMove::Relative(-3),
            CursorMove::Forward(Some(0)),
            CursorMove::Forward(Some(-2)),
            CursorMove::Backward(Some(-1)),
        ];
        // Walk both cursors through the same sequence, so each comparison starts
        // from a position the previous movements produced.
        let (mut fetched, mut moved) = (cursor(None), cursor(None));
        for movement in movements {
            let rows = ints(fetched.fetch(movement)?)?;
            assert_eq!(moved.advance(movement)?, rows.len(), "{movement:?}");
            assert_eq!(moved.pos, fetched.pos, "{movement:?}");
        }
        Ok(())
    }

    /// `ABSOLUTE`/`RELATIVE` land on a single row however far they travel, and a
    /// negative `ABSOLUTE` counts from the end.
    #[test]
    fn absolute_and_relative_deliver_one_row() -> Result<(), PgError> {
        let mut c = cursor(None);
        assert_eq!(ints(c.fetch(CursorMove::Absolute(4))?)?, [4]);
        assert_eq!(ints(c.fetch(CursorMove::Relative(-2))?)?, [2]);
        assert_eq!(ints(c.fetch(CursorMove::Absolute(1))?)?, [1]);
        assert_eq!(ints(c.fetch(CursorMove::Absolute(-1))?)?, [10]);
        // `ABSOLUTE 0` is the gap before the first row: legal, and empty.
        assert!(c.fetch(CursorMove::Absolute(0))?.is_empty());
        assert_eq!(c.pos, 0);
        Ok(())
    }

    /// A zero-count move re-reads the row the cursor already sits on.
    #[test]
    fn zero_count_refetches_the_current_row() -> Result<(), PgError> {
        let mut c = cursor(None);
        c.fetch(CursorMove::Forward(Some(2)))?;
        assert_eq!(ints(c.fetch(CursorMove::Forward(Some(0)))?)?, [2]);
        assert_eq!(ints(c.fetch(CursorMove::Relative(0))?)?, [2]);
        assert_eq!(c.pos, 2);
        Ok(())
    }

    /// Both ends are gaps the cursor rests in, so running off one and coming
    /// back returns the edge row rather than nothing.
    #[test]
    fn ends_clamp_without_losing_the_edge_row() -> Result<(), PgError> {
        let mut c = cursor(None);
        assert_eq!(ints(c.fetch(CursorMove::Forward(None))?)?.len(), 10);
        assert!(c.fetch(CursorMove::Forward(Some(1)))?.is_empty());
        assert_eq!(c.pos, 11);
        // Parked after the last row, stepping back returns it — the gap is a
        // position, not a lost row.
        assert_eq!(ints(c.fetch(CursorMove::Backward(Some(1)))?)?, [10]);
        // Overshooting the near end is not an error: it delivers what is left
        // and parks in the gap before the first row.
        assert_eq!(ints(c.fetch(CursorMove::Backward(Some(99)))?)?.len(), 9);
        assert_eq!(c.pos, 0);
        assert!(c.fetch(CursorMove::Backward(Some(1)))?.is_empty());
        Ok(())
    }

    /// A negative count reverses the direction word, as PostgreSQL's does.
    #[test]
    fn negative_counts_reverse_direction() -> Result<(), PgError> {
        let mut c = cursor(None);
        c.fetch(CursorMove::Forward(Some(5)))?;
        assert_eq!(ints(c.fetch(CursorMove::Forward(Some(-2)))?)?, [4, 3]);
        assert_eq!(ints(c.fetch(CursorMove::Backward(Some(-1)))?)?, [4]);
        Ok(())
    }

    /// `NO SCROLL` refuses a *rewind request*, which is not the same as a move
    /// that ends up behind where it started. Each case below was probed against
    /// PostgreSQL 18.4 from the position named.
    #[test]
    fn no_scroll_rejects_rewind_requests() -> Result<(), PgError> {
        // From the gap before the first row, where every one of these is a
        // no-op that still names a backward direction.
        for movement in [
            CursorMove::Backward(Some(1)),
            CursorMove::Backward(None),
            // `BACKWARD 0` rewinds even though it moves nothing, unlike the
            // direction-less `FETCH 0`.
            CursorMove::Backward(Some(0)),
            // A negative ABSOLUTE seeks from the end, so it rewinds even though
            // it lands ahead of the cursor.
            CursorMove::Absolute(-1),
            CursorMove::Forward(Some(-1)),
            CursorMove::Relative(-1),
        ] {
            let err = cursor(Some(false))
                .fetch(movement)
                .expect_err("rewind on NO SCROLL");
            assert_eq!(
                err.code,
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "{movement:?}"
            );
        }
        // Resting in an end gap, a zero-distance re-fetch never moves, so
        // PostgreSQL allows it.
        let mut c = cursor(Some(false));
        assert!(c.fetch(CursorMove::Relative(0))?.is_empty());
        assert!(c.fetch(CursorMove::Absolute(0))?.is_empty());
        assert!(c.fetch(CursorMove::Forward(Some(0)))?.is_empty());
        // On a row it is a step back and forward again, so it is refused — as is
        // re-landing on the row the cursor already occupies.
        assert_eq!(ints(c.fetch(CursorMove::Absolute(2))?)?, [2]);
        for movement in [
            CursorMove::Relative(0),
            CursorMove::Forward(Some(0)),
            CursorMove::Absolute(2),
            CursorMove::Absolute(1),
        ] {
            let err = c.fetch(movement).expect_err("rewind on NO SCROLL");
            assert_eq!(
                err.code,
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "{movement:?}"
            );
        }
        // Every rejection left the position untouched, and forward still works.
        assert_eq!(c.pos, 2);
        assert_eq!(ints(c.fetch(CursorMove::Forward(Some(1)))?)?, [3]);
        assert_eq!(ints(c.fetch(CursorMove::Absolute(5))?)?, [5]);
        Ok(())
    }

    /// An empty result set has only the two gaps, and they are the same gap.
    #[test]
    fn empty_cursor_never_yields_a_row() -> Result<(), PgError> {
        let mut c = cursor(None);
        c.rows.clear();
        for movement in [
            CursorMove::Forward(Some(1)),
            CursorMove::Forward(None),
            CursorMove::Absolute(1),
            CursorMove::Absolute(-1),
        ] {
            assert!(c.fetch(movement)?.is_empty(), "{movement:?}");
            assert_eq!(c.advance(movement)?, 0, "{movement:?}");
        }
        Ok(())
    }
}
