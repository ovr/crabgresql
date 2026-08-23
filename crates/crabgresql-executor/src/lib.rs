//! Volcano (iterator) executor.
//!
//! Scan, filter, projection, join, aggregate, window, sort/distinct and limit
//! nodes, one per file under `node` and re-exported here; expression evaluation
//! lives in [`eval`]. This module keeps what drives them: the execution context,
//! [`execute`] and the plan-to-node construction it does.
//!
//! DML (INSERT/UPDATE/DELETE) runs as plain functions rather than plan nodes: it
//! yields a row count, and — with `RETURNING` — a row stream projected over the
//! affected tuples the function already owns.

mod agg;
mod checks;
pub mod eval;
mod generate_series;
pub mod generated;
mod hash;
mod index_props;
mod keyindex;
mod md5;
mod node;
pub mod reg;
pub mod scalar_fns;
mod special_fns;
mod subplan;
mod tally;
#[cfg(test)]
mod testutil;
mod unique;
mod uuid_gen;
pub mod vector;

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::Arc;

use rustc_hash::FxHashMap;

pub use crabgresql_binder::OutputColumn;
use crabgresql_binder::{
    BoundExpr, DistinctKey, LogicalPlan, RelationIdent, Returning, SortKey, SysCol, SystemEmit,
    needs_header, projects_after_write,
};
use crabgresql_planner::{
    DmlIndexProbe, DmlTarget, PhysicalAggInput, PhysicalAppendArm, PhysicalInsertSource,
    PhysicalJoinExpr, PhysicalJoinInput, PhysicalPlan, map_assigned_columns,
    update_needs_unique_snapshot,
};
use crabgresql_storage_api::pgstat::WriteKind;
use crabgresql_storage_api::{
    ColumnProjection, IndexMetadata, PartitionBoundDatum, PartitionStrategy, ProcInfo,
    StorageError, TableAm, TableSchema, Tid, Tuple, TypeCatalog,
};
use crabgresql_txn::{TupleHeader, TxnContext};
use crabgresql_types::{FmtCtx, PgType, Value, cast};

use checks::{CheckSet, NotNullSet};
use eval::eval;
pub use eval::{
    coerce_value, coerce_value_assign, compare_values, compare_values_collated, is_orderable,
};
use generated::GeneratedSet;
pub use node::{
    Aggregate, Append, Concat, Distinct, Filter, HashJoin, IndexScan, LateralJoin, Limit,
    MaterializedRows, NestedLoopJoin, ProjectSet, Projection, SeqScan, Sort, TableFunctionSource,
    Values, WindowAgg,
};
use node::{index_probe_rows, index_probe_system_rows};
use tally::count_write;
use unique::UniqueKeySet;

/// Side-effecting sequence operations (`nextval`/`currval`/`setval`/`lastval`),
/// which the otherwise-pure expression evaluator cannot express: they mutate
/// non-transactional engine counters and per-session `currval`/`lastval` state.
/// The server supplies an implementation through [`ExecContext::sequences`]; the
/// executor calls it when it evaluates the corresponding functions.
/// The sequence functions take a possibly schema-qualified name. `namespace` is
/// the schema written by the caller (e.g. `nextval('app.s')`), or `None` when the
/// reference was unqualified — the implementation resolves `None` to its default
/// schema.
///
/// TODO: resolve an unqualified sequence name through `search_path`; the
/// implementation always answers `public`.
pub trait SequenceOps: Send + Sync {
    /// Advance the sequence and return its new value. Errors: 42P01 (no such
    /// sequence), 2200H (reached min/max with `NO CYCLE`).
    fn nextval(&self, namespace: Option<&str>, name: &str) -> Result<i64, ExecError>;
    /// The value `nextval` most recently returned for this sequence in this
    /// session. Errors 55000 if `nextval` has not run for it yet.
    fn currval(&self, namespace: Option<&str>, name: &str) -> Result<i64, ExecError>;
    /// Set the sequence's counter; returns `value`.
    fn setval(
        &self,
        namespace: Option<&str>,
        name: &str,
        value: i64,
        is_called: bool,
    ) -> Result<i64, ExecError>;
    /// The value the last `nextval` in this session returned, for any sequence.
    /// Errors 55000 if no `nextval` has run in the session yet.
    fn lastval(&self) -> Result<i64, ExecError>;
}

/// Read access to the session's GUC table, which the pure expression evaluator
/// has no handle to. Backs `current_setting()`; the server supplies an
/// implementation through [`ExecContext::gucs`].
///
/// The single method deliberately mirrors `SHOW`: both must render a setting the
/// same way, and routing them through one implementation is what keeps them from
/// drifting.
pub trait GucOps: Send + Sync {
    /// The setting's value as `SHOW` would render it, or `None` if the session
    /// has no GUC by that (case-insensitive) name.
    fn show(&self, name: &str) -> Option<String>;
}

/// Catalog lookups the pure expression evaluator cannot express: they read the
/// session's `pg_catalog` snapshot, which `eval_scalar(func, &[Value])` has no
/// handle to. The server supplies an implementation through
/// [`ExecContext::catalog`].
///
/// Every method returns `Option`, reporting only what it found; how a miss
/// renders (a placeholder string, NULL) is PostgreSQL-observable behavior and
/// lives in the executor, so an implementation never learns the SQL surface.
pub trait CatalogOps: Send + Sync {
    /// The name of the role `oid` identifies, or `None` if no role has that OID.
    fn role_name(&self, oid: u32) -> Option<String>;
    /// Whether the relation `oid` identifies is reachable by an unqualified
    /// name, or `None` if there is no such relation.
    fn table_is_visible(&self, oid: u32) -> Option<bool>;
    /// The `(namespace, name)` of the relation `oid` identifies, or `None` if
    /// there is no such relation. Backs `regclass` output and the `regclass`
    /// spelling of the sequence functions.
    fn rel_name(&self, oid: u32) -> Option<(String, String)>;
    /// The OID of the relation `namespace.name` names, or `None` if there is no
    /// such relation. `None` for `namespace` searches the unqualified path, as
    /// `pg_table_is_visible` reports it. Backs `regclass` input.
    fn rel_oid(&self, namespace: Option<&str>, name: &str) -> Option<u32>;
    /// The name of the function `oid` identifies, or `None` if there is no such
    /// function. Backs `regproc` output.
    fn proc_name(&self, oid: u32) -> Option<String>;
    /// The OID of the function `namespace.name` names, or `None` if there is no
    /// such function. `Some` only when the name is unambiguous, as `regprocin`
    /// requires. Backs `regproc` input.
    fn proc_oid(&self, namespace: Option<&str>, name: &str) -> Option<u32>;
    /// The `(namespace, name, argument types)` of the function `oid`
    /// identifies, or `None` if there is no such function. Backs
    /// `regprocedure` output, which renders the whole signature.
    fn proc_signature(&self, oid: u32) -> Option<(String, String, Vec<u32>)>;
    /// The OID of the function `namespace.name(args)` names, or `None` if there
    /// is no such function.
    ///
    /// Separate from [`CatalogOps::proc_oid`] rather than a wider version of
    /// it, because the two disagree about what an overload set means: a bare
    /// name carried by several functions is unresolvable, while a signature
    /// picks exactly one of them out.
    fn proc_oid_by_signature(
        &self,
        namespace: Option<&str>,
        name: &str,
        args: &[u32],
    ) -> Option<u32>;
    /// The operator `oid` identifies, or `None` if there is no such operator.
    /// Backs both operator kinds of `reg*` output: `regoper` needs the namespace
    /// to qualify a name several operators share, `regoperator` prints the
    /// operand types as well.
    fn oper_signature(&self, oid: u32) -> Option<CatalogOperator>;
    /// The OIDs of every operator `namespace.name` names. `None` for
    /// `namespace` searches the unqualified path.
    ///
    /// A **list**, not an `Option`, because both halves of `regoper` need the
    /// count rather than a winner: input raises "more than one operator named"
    /// where `regproc` would simply miss, and output schema-qualifies exactly
    /// when the bare name would not read back as this operator. Which of those
    /// is an error is the executor's to decide, so an implementation never
    /// learns the SQL surface.
    fn oper_oids(&self, namespace: Option<&str>, name: &str) -> Vec<u32>;
    /// Singular where [`CatalogOps::oper_oids`] is plural, and that is the whole
    /// difference between the two operator kinds: the operands make the name
    /// unique, so `regoperator` input has the one-or-nothing shape `regproc` has
    /// and needs no ambiguity rule. Backs `regoperator` input.
    fn oper_oid(&self, namespace: Option<&str>, name: &str, left: u32, right: u32) -> Option<u32>;
    /// The comments on `objoid`, as `obj_description`/`col_description` read
    /// them out of `pg_description`. `catalog` is the `pg_catalog` relation the
    /// object lives in; `None` is the deprecated one-argument
    /// `obj_description(oid)`, which searches every catalog at once.
    ///
    /// A **list**, not an `Option`: the any-catalog form can match twice (OIDs
    /// are unique per catalog, not across them) and PostgreSQL raises there
    /// rather than picking one. Which of the two is an error is the executor's
    /// to decide, so an implementation never learns the SQL surface. A catalog
    /// name that names no relation is empty, not an error.
    fn object_description(&self, objoid: u32, objsubid: i32, catalog: Option<&str>) -> Vec<String>;
    /// The name of the schema `oid` identifies, and its inverse. Back
    /// `regnamespace`.
    fn namespace_name(&self, oid: u32) -> Option<String>;
    fn namespace_oid(&self, name: &str) -> Option<u32>;
    /// The `(namespace, name)` of the *user* type `oid` identifies, and its
    /// inverse. Built-in types resolve without a catalog (`PgType::from_oid` /
    /// `PgType::from_name`), so these see only `CREATE TYPE` names.
    fn user_type_name(&self, oid: u32) -> Option<(String, String)>;
    fn user_type_oid(&self, namespace: Option<&str>, name: &str) -> Option<u32>;
    /// The SQL text stored for the view `namespace.name` and its column names,
    /// or `None` if there is no such view. Handed back verbatim; re-parsing and
    /// re-rendering it in PostgreSQL's canonical shape is [`crabgresql_binder::ruleutils`]'s
    /// job, so an implementation never learns the SQL surface. The columns are
    /// what a `SELECT *` in the body expands to, frozen at `CREATE VIEW` time.
    /// Backs `pg_get_viewdef`.
    fn view_sql(&self, namespace: Option<&str>, name: &str) -> Option<(String, Vec<String>)>;
    /// The constraint `oid` identifies, or `None` if this snapshot has none.
    /// Backs `pg_get_constraintdef`, which resolves *by OID* and so needs the
    /// reverse of the numbering `pg_constraint`'s rows are built from.
    fn constraint_def(&self, oid: u32) -> Option<ConstraintDef>;

    /// The relation the rewrite rule `oid` is attached to — `pg_rewrite.ev_class`
    /// — or `None` if no rule has that OID. Backs `pg_get_ruledef`.
    ///
    /// The OID alone, with no rule name and no body: every rule here is a view's
    /// `_RETURN` rule, so the name is a constant and the body is the view's,
    /// read back through [`CatalogOps::view_sql`].
    fn rule_relation(&self, oid: u32) -> Option<u32>;

    /// The argument and result shape of the function `oid`, or `None` if there
    /// is no such function. Backs the `pg_get_function_*` trio, which renders
    /// what [`CatalogOps::proc_signature`] only identifies by.
    fn proc_info(&self, oid: u32) -> Option<ProcInfo>;

    /// Every installable extension version, for the `pg_available_extensions()`
    /// and `pg_available_extension_versions()` functions. The same rows the views
    /// of those names publish — one source, so `\dx` and a direct read of a view
    /// cannot disagree.
    fn available_extensions(&self) -> Vec<ExtensionVersion>;

    /// The relation `oid` and each partitioned ancestor above it, innermost
    /// first, as `pg_partition_ancestors` reports them. **Empty** for a relation
    /// that is neither a partition nor partitioned — PostgreSQL returns no rows
    /// there, not one row naming the relation itself.
    fn partition_ancestors(&self, oid: u32) -> Vec<u32>;

    /// The index `oid` identifies together with the table it indexes, or `None`
    /// if this snapshot has no such index. Backs `pg_get_indexdef`, which
    /// resolves by the index's *own* relation OID — the one `pg_class` gives the
    /// index, not the one it gives the table.
    fn index_def(&self, oid: u32) -> Option<IndexDef>;

    /// The partition key of the relation `oid` identifies, behind
    /// `pg_get_partkeydef`. `None` both for an OID no relation answers to and
    /// for a relation that is not partitioned — PostgreSQL reports NULL for
    /// either, so the two need not be told apart.
    fn partition_key_def(&self, oid: u32) -> Option<PartitionKeyDef>;

    /// The sequence column `column` of relation `oid` owns, behind
    /// `pg_get_serial_sequence`. The column name is matched exactly, as
    /// PostgreSQL matches it; see [`SerialSequence`] for why the misses are
    /// distinguished.
    fn serial_sequence(&self, oid: u32, column: &str) -> SerialSequence;

    /// The database this connection was opened against — `current_database()`
    /// and `current_catalog`.
    fn current_database(&self) -> String;

    /// The role this connection authenticated as. crabgresql has no `SET ROLE`,
    /// so `current_user` and `session_user` are always the same string; the
    /// split lives here so a future `SET ROLE` changes one method rather than
    /// the SQL surface.
    fn current_user(&self) -> String;
    fn session_user(&self) -> String;

    /// The schemas an unqualified name is searched in, outermost first, as
    /// `current_schemas` reports them. `include_implicit` adds the ones
    /// PostgreSQL never lists in `search_path` itself: this session's temp
    /// namespace, once instantiated, and `pg_catalog`.
    fn search_path(&self, include_implicit: bool) -> Vec<String>;

    /// This session's temp namespace OID, or `None` before a temp relation has
    /// instantiated it. Backs `pg_my_temp_schema()`, which reports 0 for the
    /// `None` case.
    fn my_temp_schema(&self) -> Option<u32>;

    /// This connection's backend id, behind `pg_backend_pid()`. It identifies a
    /// connection rather than an OS process — every session here is served from
    /// the same one — which is what `WHERE pid = pg_backend_pid()` needs of it.
    fn backend_pid(&self) -> i32;

    /// What the relation `oid` occupies on disk, or `None` if there is no such
    /// relation. Backs the four `pg_*_size` functions, which each sum a
    /// different subset of [`RelationSize`]'s parts — which is why this reports
    /// the parts rather than one total.
    ///
    /// `None` and an all-zero answer are different: the first is an OID naming
    /// nothing (NULL), the second a relation with no storage behind it (0), as
    /// PostgreSQL answers for a view.
    fn relation_size(&self, oid: u32) -> Option<RelationSize>;
}

/// A relation's physical size in **bytes**, split into the parts the size
/// functions add up differently. See [`CatalogOps::relation_size`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelationSize {
    /// The relation's own storage.
    pub main: u64,
    /// Its out-of-line ("TOAST") storage, or zero when it has none.
    pub toast: u64,
    /// Every index on it, summed.
    pub indexes: u64,
}

/// One installable extension version, as [`CatalogOps::available_extensions`]
/// reports it: everything `pg_available_extension_versions()` publishes.
///
/// `pg_available_extensions()` reads the same rows and shows three of the fields;
/// `installed` is not carried because the *view* computes it, not the function.
/// Neither is `requires`, which is NULL for every extension here.
#[derive(Clone, Debug)]
pub struct ExtensionVersion {
    pub name: String,
    /// The version, which is also the extension's `default_version`: each one
    /// here offers exactly one.
    pub version: String,
    pub superuser: bool,
    pub trusted: bool,
    pub relocatable: bool,
    pub schema: String,
    pub comment: String,
}

/// What [`CatalogOps::serial_sequence`] found.
///
/// The three misses are separate because PostgreSQL renders each differently: a
/// column that owns no sequence is NULL, a missing column is a `42703` naming
/// the relation, and a relation that vanished between the name lookup and this
/// call is NULL rather than a second `42P01` (the name lookup already raised
/// one for a name that never resolved).
#[derive(Clone, Debug)]
pub enum SerialSequence {
    Owned { namespace: String, name: String },
    Unowned,
    NoColumn { relation: String },
    NoRelation,
}

/// An operator as the catalog holds it, for the two `reg*` kinds that name one.
/// Where an operator *lives* is the catalog's to say and how it *prints* is the
/// executor's, so this carries the raw columns and no rendering — the same split
/// [`ConstraintDef`] makes.
#[derive(Clone, Debug)]
pub struct CatalogOperator {
    pub namespace: String,
    /// `pg_operator.oprname`: punctuation, not an identifier — `+`, `||/`, `~~`.
    pub name: String,
    /// `oprleft`, which is **0** for a prefix operator: it has no left operand,
    /// and `regoperator` prints that absence as `NONE`.
    pub left: u32,
    pub right: u32,
}

/// What `pg_get_constraintdef` needs to reproduce a constraint's DDL. Rendering
/// lives in the executor, so an implementation never learns the SQL surface —
/// the same split [`CatalogOps::view_sql`] makes.
#[derive(Clone, Debug)]
pub struct ConstraintDef {
    /// `pg_constraint.contype`: `c` check, `p` primary key, `u` unique,
    /// `n` not-null.
    pub contype: String,
    /// The constrained columns' names, in key order.
    pub columns: Vec<String>,
    /// The stored predicate of a check constraint; `None` for the rest.
    pub expr: Option<String>,
    /// Whether this constrains a **domain** rather than a relation. It changes
    /// two things in the rendering: a `NOT NULL` names no column, and a check's
    /// operand is the `VALUE` placeholder rather than a column.
    pub is_domain: bool,
}

/// What `pg_get_indexdef` needs to reproduce an index's DDL: the index and the
/// table it is defined on, since the statement names both and takes its column
/// names from the second.
///
/// Unlike [`ConstraintDef`], the rendering this feeds does *not* live in the
/// executor — [`crabgresql_storage_api::index_definition`] owns it, so that
/// `pg_indexes.indexdef` in the catalog crate can print the same string. See
/// that function for why.
#[derive(Clone, Debug)]
pub struct IndexDef {
    pub index: IndexMetadata,
    pub table: TableSchema,
}

/// What `pg_get_partkeydef` needs to reproduce a `PARTITION BY` clause: the
/// strategy and the key columns' names, in key order. Rendering lives in the
/// executor, so an implementation never learns the SQL surface — the same split
/// [`ConstraintDef`] makes.
#[derive(Clone, Debug)]
pub struct PartitionKeyDef {
    pub strategy: PartitionStrategy,
    pub columns: Vec<String>,
}

/// Severity of a diagnostic produced during execution. `Debug` and `Log` reach
/// the server log rather than the client under PostgreSQL's default
/// `client_min_messages`; the rest travel as a `NoticeResponse`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Debug,
    Log,
    Info,
    Notice,
    Warning,
}

/// A diagnostic raised *during* execution rather than by the statement as a
/// whole — what `RAISE NOTICE` inside a routine body produces.
#[derive(Clone, Debug)]
pub struct RuntimeNotice {
    pub severity: Severity,
    pub code: String,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    /// The `CONTEXT:` traceback, innermost frame first.
    pub context: Vec<String>,
}

/// Where mid-execution diagnostics go.
///
/// Deliberately a sink on the context rather than a return channel on
/// [`ExecNode::next`]: a notice can be raised while a result set is streaming,
/// and threading it through every node's signature would touch every executor
/// node to serve one caller. The server installs a session-owned buffer and
/// drains it as it writes rows, so a notice raised on row 3 reaches the client
/// between row 3 and row 4.
pub trait NoticeSink: Send + Sync {
    fn emit(&self, notice: RuntimeNotice);
}

/// Invocation of a user-defined routine whose body the binder cannot inline.
///
/// A `LANGUAGE SQL` body is a single expression and is expanded into the
/// calling expression at bind time. A PL/pgSQL body is an imperative program
/// that binds, plans and executes SQL of its own — vocabulary the executor does
/// not have — so the server supplies an implementation through
/// [`ExecContext::routines`] and `eval` dispatches to it.
///
/// `ctx` is the caller's own context; an implementation clones it (bumping
/// `call_depth`) for the statements it runs, so the body sees the same sequence
/// handle, notice sink and catalog snapshot as its caller. It must not *store*
/// a context: the context holds an `Arc` to the implementation, and putting one
/// back would leak the cycle.
pub trait RoutineOps: Send + Sync {
    /// Call a routine and produce its return value. A procedure yields
    /// `Value::Null`.
    fn call(
        &self,
        oid: u32,
        args: Vec<Value>,
        ctx: &ExecContext,
        txn: &TxnContext,
    ) -> Result<Value, ExecError>;

    /// Run a `DO $$ ... $$` block. Its body is passed verbatim — an anonymous
    /// block has no catalog entry to look up.
    fn run_inline_block(
        &self,
        body: &str,
        ctx: &ExecContext,
        txn: &TxnContext,
    ) -> Result<(), ExecError>;
}

/// Session state that runtime evaluation depends on: the value-formatting
/// context (`extra_float_digits` and the display `TimeZone`) and, when present,
/// the handles the side-effecting sequence functions and the catalog functions
/// dispatch through.
#[derive(Clone)]
pub struct ExecContext {
    /// `extra_float_digits` and the session display zone — everything value
    /// rendering, parsing, and casting needs from the session.
    pub fmt: FmtCtx,
    /// `seq_page_cost` and `random_page_cost`, for the paths that plan a
    /// statement of their own — a routine body, a correlated subplan re-planned
    /// per outer row — rather than being handed a finished plan. Carried here
    /// because those paths have a context and no session: a `SET
    /// random_page_cost` inside a function must reach the statements the
    /// function runs, the way every other session setting does.
    pub costs: crabgresql_planner::cost::CostSettings,
    /// `None` in contexts that never call a sequence function (e.g. `EXPLAIN`'s
    /// `Values` node); a sequence function reaching a `None` context is an
    /// internal wiring error, reported as 5-char `XX000`.
    pub sequences: Option<Arc<dyn SequenceOps>>,
    /// The `pg_catalog` snapshot the catalog functions (`pg_get_userbyid`,
    /// `pg_table_is_visible`) resolve against. `None` in the same non-executing
    /// contexts as `sequences`, and an internal `XX000` if one reaches it.
    pub catalog: Option<Arc<dyn CatalogOps>>,
    /// The session's GUC table, for `current_setting()`. `None` in the same
    /// non-executing contexts as `sequences`, and an internal `XX000` if one
    /// reaches it.
    pub gucs: Option<Arc<dyn GucOps>>,
    /// Resolves type and function names when the executor has to *bind* stored
    /// SQL text, which today means a relation's CHECK constraints: they are
    /// kept as canonical SQL (like a column default) and bound once per
    /// statement, against a schema the executor may only discover mid-flight —
    /// a partition leaf or an inheritance child. `None` in the same
    /// non-executing contexts as `sequences`; a schema carrying checks that
    /// reaches a `None` context is an internal wiring error (`XX000`).
    pub types: Option<Arc<dyn TypeCatalog>>,
    /// The transaction a correlated subquery re-executes against, per outer row.
    /// Injected by [`execute`] once, at the top of the statement, and cloned into
    /// every node so `eval` can run a correlated subplan when it reaches one.
    /// `None` outside a real `execute` (a subquery marker never survives to a
    /// context without it).
    pub txn: Option<TxnContext>,
    /// The interpreter a non-inlinable routine call dispatches through.
    pub routines: Option<Arc<dyn RoutineOps>>,
    /// Where a `RAISE` inside a routine body deposits its diagnostics.
    pub notices: Option<Arc<dyn NoticeSink>>,
    /// Whether the enclosing transaction is read-only. Until now this only
    /// reached the sequence handle; a routine body's DML needs it too, since
    /// the statement-level check cannot see inside a body.
    pub read_only: bool,
    /// How many routine calls deep execution is, for the stack-depth limit.
    ///
    /// A field rather than a thread-local: a suspended portal holds a live
    /// `ExecNode` and its cloned context across `Execute` round-trips, and
    /// tokio may resume it on a different worker thread. Cloning the context
    /// into every node also scopes the depth to the call tree for free.
    pub call_depth: u32,
    /// The transaction's command counter, shared with the session so that
    /// statements run inside a routine body advance the same counter the
    /// session reads back at statement end. Without it the next top-level
    /// statement would reuse a command id the body already stamped rows with,
    /// and those rows would be invisible to it.
    pub command_counter: Option<Arc<std::sync::atomic::AtomicU32>>,
    /// What the executor has learned about this statement's subplans (see
    /// [`subplan`]). Created by the outermost [`execute`] and inherited, not
    /// replaced, by the nested executions a correlated subquery drives — a cache
    /// scoped to a single outer row would never be hit.
    pub subplans: Option<Arc<subplan::SubplanCache>>,
    /// Where the scan nodes report what they read. `None` in a context with no
    /// server behind it — a unit test, an `EXPLAIN` — and then nothing is
    /// counted at all, which is the truth for work that never ran.
    pub stats: Option<Arc<crabgresql_storage_api::pgstat::PgStatCounters>>,
}

impl Default for ExecContext {
    fn default() -> Self {
        // PG's default since v12.
        Self {
            fmt: FmtCtx::utc_default(),
            costs: crabgresql_planner::cost::CostSettings::default(),
            sequences: None,
            catalog: None,
            gucs: None,
            types: None,
            txn: None,
            routines: None,
            notices: None,
            read_only: false,
            call_depth: 0,
            command_counter: None,
            subplans: None,
            stats: None,
        }
    }
}

/// A runtime execution error, reported to the client as `ErrorResponse`.
/// Distinct from a bind error: it can surface mid-stream, after rows of the
/// result set have already been sent.
#[derive(Debug)]
pub struct ExecError {
    /// 5-character SQLSTATE code. A `Cow` because a routine body can name its
    /// own SQLSTATE at runtime (`RAISE ... USING ERRCODE`); every built-in
    /// error still passes a `&'static str` from [`sqlstate`].
    pub code: Cow<'static, str>,
    pub message: String,
    /// Optional DETAIL line (e.g. numeric field overflow explains the p/s).
    pub detail: Option<String>,
    /// Optional HINT line.
    pub hint: Option<String>,
    /// The `CONTEXT:` traceback: the call frames this error unwound through,
    /// innermost first. Empty for an error raised at the top level. Accreted
    /// while unwinding rather than tracked in a frame stack, so the happy path
    /// costs nothing and there is no pop to forget.
    pub context: Vec<String>,
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExecError {}

impl From<StorageError> for ExecError {
    fn from(error: StorageError) -> Self {
        let code = match error {
            StorageError::UnsupportedOperation(_) | StorageError::UnsupportedType(_) => "0A000",
            StorageError::Io(_) => "58030",
            StorageError::CorruptData(_) => "XX001",
            StorageError::TableNotFound(_) | StorageError::IndexTableNotFound(_) => "42P01",
            StorageError::TableAlreadyExists(_) | StorageError::RelationAlreadyExists(_) => "42P07",
            StorageError::SchemaAlreadyExists(_) => "42P06",
            StorageError::SchemaNotFound(_) => "3F000",
            StorageError::RowTooBig { .. }
            | StorageError::ValueTooBig { .. }
            | StorageError::IndexRowTooBig { .. } => "54000",
            StorageError::NumericFieldOverflow { .. } => "22003",
        };
        Self::new(code, error.to_string())
            .with_detail(error.detail())
            .with_hint(error.hint())
    }
}

impl ExecError {
    pub fn new(code: impl Into<Cow<'static, str>>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            detail: None,
            hint: None,
            context: Vec::new(),
        }
    }

    /// Attach a DETAIL line.
    pub fn with_detail(mut self, detail: Option<String>) -> Self {
        self.detail = detail;
        self
    }

    /// Attach a HINT line.
    pub fn with_hint(mut self, hint: Option<String>) -> Self {
        self.hint = hint;
        self
    }

    /// Record the call frame this error is propagating out of. Called while
    /// unwinding, so frames land innermost-first without a frame stack.
    pub fn push_context(mut self, frame: impl Into<String>) -> Self {
        self.context.push(frame.into());
        self
    }

    /// The `CONTEXT` wire field: frames newline-joined, innermost first, or
    /// `None` when no frame contributed.
    pub fn context(&self) -> Option<String> {
        (!self.context.is_empty()).then(|| self.context.join("\n"))
    }
}

/// A bind-time error raised from inside execution — the runtime type-name
/// resolution behind `pg_input_is_valid` / `pg_input_error_info`. The cursor
/// position is dropped: that type name arrived as a runtime string, so it has
/// no place in the query text.
impl From<crabgresql_binder::BindError> for ExecError {
    fn from(e: crabgresql_binder::BindError) -> Self {
        ExecError::new(e.code, e.message)
            .with_detail(e.detail)
            .with_hint(e.hint)
    }
}

/// A soft input failure, carried as an `ExecError` for its shape rather than to
/// be raised: `pg_input_error_info` reports these fields as a row, and the
/// `reg*` half of that path already produces `ExecError`s.
impl From<crabgresql_binder::SoftError> for ExecError {
    fn from(e: crabgresql_binder::SoftError) -> Self {
        ExecError::new(e.code, e.message)
            .with_detail(e.detail)
            .with_hint(e.hint)
    }
}

/// A Volcano execution node: `next()` pulls one tuple at a time.
pub trait ExecNode: Send {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError>;
}

/// The outcome of a statement: a streamable result set, or a mutation count.
pub enum Execution {
    Rows {
        columns: Vec<OutputColumn>,
        node: Box<dyn ExecNode>,
    },
    Inserted(u64),
    Updated(u64),
    Deleted(u64),
    /// A data-modifying statement with a `RETURNING` clause: the affected rows
    /// projected as a result set, plus the DML verb so the server still emits
    /// the mutation command tag (`INSERT 0 n` / `UPDATE n` / `DELETE n`) rather
    /// than `SELECT n`. RETURNING is scalar one-in/one-out (the binder rejects
    /// aggregates and set-returning functions), so the streamed row count is one
    /// per affected row. It can still exceed the count `update_many`/`delete_many`
    /// actually applied when a matched row is skipped as non-live at write time.
    ///
    /// TODO: reconcile the RETURNING row count with the rows actually written,
    /// which needs cross-transaction write-conflict resolution (see
    /// [`update_direct`]).
    ReturningRows {
        columns: Vec<OutputColumn>,
        node: Box<dyn ExecNode>,
        verb: DmlVerb,
    },
}

/// Which data-modifying statement produced a [`Execution::ReturningRows`],
/// selecting the command tag the server reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmlVerb {
    Insert,
    Update,
    Delete,
}

/// Optimize a bound plan and turn it into a physical one — the step every
/// caller takes between the binder and [`execute`].
///
/// The two halves belong together: the logical rewrites
/// ([`crabgresql_optimizer`]) feed the planner — a folded key is a literal, and
/// only a literal lets the cost model read the column's distribution — so a
/// statement planned without them is silently costed worse. Keeping both in one
/// function is what keeps the pipeline identical at every call site.
///
/// Optimization runs per execution, against this session's [`FmtCtx`], and its
/// result is never cached across executions — a prepared statement keeps the
/// plan the binder produced.
pub fn optimize_and_plan(logical: LogicalPlan, ctx: &ExecContext) -> PhysicalPlan {
    optimize_and_plan_with(logical, &ctx.fmt, ctx.costs)
}

/// [`optimize_and_plan`] for a caller that has the session settings but no
/// [`ExecContext`] yet — the server builds one only after it has a plan.
pub fn optimize_and_plan_with(
    mut logical: LogicalPlan,
    fmt: &FmtCtx,
    costs: crabgresql_planner::cost::CostSettings,
) -> PhysicalPlan {
    crabgresql_optimizer::optimize(
        &mut logical,
        &crabgresql_optimizer::OptimizerContext::new(fmt.clone()),
    );
    crabgresql_planner::plan(logical, costs)
}

pub fn execute(
    mut plan: PhysicalPlan,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    // Build the enriched context by copying only the fields we keep — not
    // `..ctx.clone()`, which would clone the old `txn` Snapshot (a `Vec<Xid>`)
    // only to overwrite it. One `txn.clone()` per execute, none wasted.
    //
    // Correlated subqueries are left in place by the fold below and evaluated per
    // outer row by `eval`, which needs the transaction to re-run their subplans —
    // so thread it through the context every node is built with (and nested
    // `run_subplan` → `execute` re-injects it, so deeper levels see it too). The
    // subplan cache is threaded the same way, but *inherited* where one already
    // exists: a cache that started over at each nested execution would be built
    // per outer row and hit never.
    let ctx = &ExecContext {
        fmt: ctx.fmt.clone(),
        costs: ctx.costs,
        sequences: ctx.sequences.clone(),
        catalog: ctx.catalog.clone(),
        gucs: ctx.gucs.clone(),
        types: ctx.types.clone(),
        txn: Some(txn.clone()),
        routines: ctx.routines.clone(),
        notices: ctx.notices.clone(),
        read_only: ctx.read_only,
        call_depth: ctx.call_depth,
        command_counter: ctx.command_counter.clone(),
        subplans: Some(ctx.subplans.clone().unwrap_or_default()),
        stats: ctx.stats.clone(),
    };
    // Fold every *non-correlated* subquery to a constant/comparison before any
    // node evaluates an expression.
    resolve_subqueries(&mut plan, ctx, txn)?;
    match plan {
        PhysicalPlan::Values {
            columns,
            rows,
            predicate,
            sort,
            distinct,
        } => {
            // The emitted tuple width, including any hidden ORDER BY / DISTINCT ON
            // columns a FROM-less `SELECT DISTINCT ON (expr)` appended past the
            // visible output — captured before `rows` is moved so a Distinct can
            // keep those columns through the sort.
            let full_width = rows.first().map_or(columns.len(), Vec::len);
            let mut node: Box<dyn ExecNode> = Box::new(Values::new(rows, ctx.clone()));
            if let Some(predicate) = predicate {
                node = Box::new(Filter::new(node, predicate, ctx.clone()));
            }
            node = finish_sort_distinct(node, sort, distinct, full_width, &columns)?;
            Ok(Execution::Rows { columns, node })
        }
        PhysicalPlan::Select {
            table,
            projection,
            system,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        } => project_pipeline(
            scan_source(&table, txn, &projection, system.as_ref(), ctx)?,
            projections,
            predicate,
            sort,
            distinct,
            columns,
            ctx,
        ),
        PhysicalPlan::Append { arms, columns } => {
            // A fanned-out relation read: concatenate every arm's scan. The
            // wrapping Subquery applies this level's projection/predicate/sort.
            Ok(Execution::Rows {
                columns,
                node: append_source(&arms, ctx, txn)?.into_rows(),
            })
        }
        PhysicalPlan::SetOp {
            arms,
            columns,
            sort,
            distinct,
        } => {
            // A UNION [ALL]: run each arm, coerce the ones whose own column types
            // differ from the unified output, and concatenate the streams. Then
            // apply this node's own deduplication (UNION) and ORDER BY.
            let mut children: Vec<Box<dyn ExecNode>> = Vec::with_capacity(arms.len());
            for arm in arms {
                let Execution::Rows { node, .. } = execute(arm.plan, ctx, txn)? else {
                    return Err(ExecError::new(
                        "XX000",
                        "UNION arm did not produce a row set",
                    ));
                };
                children.push(match arm.coercion {
                    Some(projections) => Box::new(Projection::new(node, projections, ctx.clone())),
                    None => node,
                });
            }
            let node = finish_sort_distinct(
                Box::new(Concat::new(children)),
                sort,
                distinct,
                columns.len(),
                &columns,
            )?;
            Ok(Execution::Rows { columns, node })
        }
        PhysicalPlan::IndexScan {
            table,
            projection,
            system,
            index_name,
            key,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        } => {
            let source = IndexScan::new(&table, &index_name, key, system, ctx, txn, &projection)?;
            project_pipeline(
                Source::Rows(Box::new(source)),
                projections,
                predicate,
                sort,
                distinct,
                columns,
                ctx,
            )
        }
        PhysicalPlan::Subquery {
            source,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        } => {
            // Stream the source's rows straight into this level's pipeline. A
            // single FROM reference needs no materialization.
            // TODO: materialize a CTE referenced more than once, instead of
            // re-running its body at every reference.
            let source = subquery_source(*source, ctx, txn)?;
            project_pipeline(source, projections, predicate, sort, distinct, columns, ctx)
        }
        PhysicalPlan::Window {
            source,
            spec,
            funcs,
            output_width,
            ..
        } => {
            let Execution::Rows { columns, node } = execute(*source, ctx, txn)? else {
                return Err(ExecError::new(
                    "XX000",
                    "window source did not produce a row set",
                ));
            };
            // A bare row source: the query's projection, ORDER BY and DISTINCT
            // live on the `Subquery` the binder wrapped this chain in, and it
            // supplies the real output columns — these are never surfaced.
            Ok(Execution::Rows {
                columns,
                node: Box::new(WindowAgg::new(node, spec, funcs, output_width, ctx)?),
            })
        }
        PhysicalPlan::TableFunction {
            func,
            args,
            ordinality,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        } => project_pipeline(
            Source::Rows(Box::new(TableFunctionSource::new(
                func,
                args,
                ordinality,
                ctx.clone(),
            ))),
            projections,
            predicate,
            sort,
            distinct,
            columns,
            ctx,
        ),
        PhysicalPlan::Join {
            source,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        } => {
            let joined = build_join_expr(source, ctx, txn)?;
            project_pipeline(
                Source::Rows(joined),
                projections,
                predicate,
                sort,
                distinct,
                columns,
                ctx,
            )
        }
        PhysicalPlan::Limit {
            source,
            limit,
            offset,
        } => {
            let Execution::Rows { columns, node } = execute(*source, ctx, txn)? else {
                return Err(ExecError::new(
                    "XX000",
                    "LIMIT source did not produce a row set",
                ));
            };
            Ok(Execution::Rows {
                columns,
                node: Box::new(Limit::new(node, limit, offset)),
            })
        }
        PhysicalPlan::Aggregate {
            input,
            predicate,
            group_exprs,
            aggregates,
            having,
            columns,
            projections,
            sort,
            distinct,
        } => {
            // Source rows: a base table scan or the single virtual row of a
            // FROM-less aggregate.
            let source: Box<dyn ExecNode> = match input {
                PhysicalAggInput::Scan { table, projection } => {
                    Box::new(SeqScan::new(&table, txn, &projection, ctx))
                }
                PhysicalAggInput::Join(source) => build_join_expr(source, ctx, txn)?,
                PhysicalAggInput::SingleRow => Box::new(Values::new(vec![vec![]], ctx.clone())),
            };
            // WHERE filters rows before aggregation.
            let mut node: Box<dyn ExecNode> = match predicate {
                Some(predicate) => Box::new(Filter::new(source, predicate, ctx.clone())),
                None => source,
            };
            node = Box::new(Aggregate::new(node, group_exprs, aggregates, ctx.clone()));
            // HAVING filters the per-group rows.
            if let Some(having) = having {
                node = Box::new(Filter::new(node, having, ctx.clone()));
            }
            // The projection list and ORDER BY were rewritten to reference the
            // aggregate output row, so the standard tail finishes the job.
            project_pipeline(
                Source::Rows(node),
                projections,
                None,
                sort,
                distinct,
                columns,
                ctx,
            )
        }
        PhysicalPlan::Insert {
            table,
            source,
            returning,
            routing,
            freeze,
            system,
        } => execute_insert(
            &table, source, returning, routing, freeze, &system, ctx, txn,
        ),
        PhysicalPlan::Update {
            table,
            predicate,
            assignments,
            returning,
            routing,
            inherited,
            probe,
            system,
        } => execute_update(
            &table,
            &predicate,
            &assignments,
            returning,
            routing,
            inherited,
            probe.as_ref(),
            &system,
            ctx,
            txn,
        ),
        PhysicalPlan::Delete {
            table,
            predicate,
            returning,
            routing,
            inherited,
            probe,
            system,
        } => execute_delete(
            &table,
            &predicate,
            returning,
            routing,
            inherited,
            probe.as_ref(),
            &system,
            ctx,
            txn,
        ),
    }
}

/// Fold every non-correlated subquery expression in `plan` to a plain
/// [`BoundExpr`] the evaluator handles: a scalar subquery to a `Const`, `EXISTS`
/// to a boolean `Const`, and `IN (SELECT …)` to an OR-chain of equality
/// comparisons (wrapped in `NOT` when negated). Each subplan runs exactly once —
/// a non-correlated subquery does not depend on the outer row — via a recursive
/// `plan` + `execute`. Walks nested source plans so one top-level call covers the
/// whole tree; runs before any node evaluates an expression, so `eval` never sees
/// a subquery marker.
fn resolve_subqueries(
    plan: &mut PhysicalPlan,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<(), ExecError> {
    match plan {
        PhysicalPlan::Values {
            rows, predicate, ..
        } => {
            for row in rows {
                resolve_exprs(row, ctx, txn)?;
            }
            resolve_opt(predicate, ctx, txn)?;
        }
        PhysicalPlan::Select {
            projections,
            predicate,
            ..
        } => {
            resolve_exprs(projections, ctx, txn)?;
            resolve_opt(predicate, ctx, txn)?;
        }
        // An Append holds only leaf table handles — no subquery expressions.
        PhysicalPlan::Append { .. } => {}
        // A SetOp holds no exprs of its own, but each arm's sub-plan may — recurse.
        PhysicalPlan::SetOp { arms, .. } => {
            for arm in arms.iter_mut() {
                resolve_subqueries(&mut arm.plan, ctx, txn)?;
                if let Some(coercion) = &mut arm.coercion {
                    resolve_exprs(coercion, ctx, txn)?;
                }
            }
        }
        PhysicalPlan::IndexScan {
            key,
            projections,
            predicate,
            ..
        } => {
            for value in key.exprs_mut() {
                resolve_expr(value, ctx, txn)?;
            }
            resolve_exprs(projections, ctx, txn)?;
            resolve_opt(predicate, ctx, txn)?;
        }
        PhysicalPlan::Subquery {
            source,
            projections,
            predicate,
            ..
        } => {
            resolve_subqueries(source, ctx, txn)?;
            resolve_exprs(projections, ctx, txn)?;
            resolve_opt(predicate, ctx, txn)?;
        }
        PhysicalPlan::Window {
            source,
            spec,
            funcs,
            ..
        } => {
            resolve_subqueries(source, ctx, txn)?;
            for expr in spec.exprs_mut() {
                resolve_expr(expr, ctx, txn)?;
            }
            for func in funcs {
                resolve_exprs(func.kind.args_mut(), ctx, txn)?;
            }
        }
        PhysicalPlan::TableFunction {
            args,
            projections,
            predicate,
            ..
        } => {
            resolve_exprs(args, ctx, txn)?;
            resolve_exprs(projections, ctx, txn)?;
            resolve_opt(predicate, ctx, txn)?;
        }
        PhysicalPlan::Join {
            source,
            projections,
            predicate,
            ..
        } => {
            resolve_join(source, ctx, txn)?;
            resolve_exprs(projections, ctx, txn)?;
            resolve_opt(predicate, ctx, txn)?;
        }
        PhysicalPlan::Aggregate {
            input,
            predicate,
            group_exprs,
            aggregates,
            having,
            projections,
            ..
        } => {
            if let PhysicalAggInput::Join(join) = input {
                resolve_join(join, ctx, txn)?;
            }
            resolve_opt(predicate, ctx, txn)?;
            resolve_exprs(group_exprs, ctx, txn)?;
            for agg in aggregates.iter_mut() {
                for arg in agg.exprs_mut() {
                    resolve_expr(arg, ctx, txn)?;
                }
            }
            resolve_opt(having, ctx, txn)?;
            resolve_exprs(projections, ctx, txn)?;
        }
        PhysicalPlan::Limit { source, .. } => resolve_subqueries(source, ctx, txn)?,
        PhysicalPlan::Insert {
            source, returning, ..
        } => {
            match source {
                PhysicalInsertSource::Values(rows) => {
                    for row in rows {
                        resolve_exprs(row, ctx, txn)?;
                    }
                }
                // The rows are values, so only the deferred defaults can hold a
                // subquery. Not walking the rows is the point: a bulk load's
                // per-cell walk is O(rows x columns) for nothing.
                PhysicalInsertSource::Tuples { defaults, .. } => {
                    for (_, default) in defaults.iter_mut() {
                        resolve_expr(default, ctx, txn)?;
                    }
                }
                PhysicalInsertSource::Query { input, projections } => {
                    resolve_subqueries(input, ctx, txn)?;
                    resolve_exprs(projections, ctx, txn)?;
                }
            }
            resolve_returning(returning, ctx, txn)?;
        }
        PhysicalPlan::Update {
            predicate,
            assignments,
            returning,
            routing,
            inherited,
            probe,
            ..
        } => {
            resolve_opt(predicate, ctx, txn)?;
            for (_, value) in assignments.iter_mut() {
                resolve_expr(value, ctx, txn)?;
            }
            resolve_probe_keys(routing, inherited, probe, ctx, txn)?;
            resolve_returning(returning, ctx, txn)?;
        }
        PhysicalPlan::Delete {
            predicate,
            returning,
            routing,
            inherited,
            probe,
            ..
        } => {
            resolve_opt(predicate, ctx, txn)?;
            resolve_probe_keys(routing, inherited, probe, ctx, txn)?;
            resolve_returning(returning, ctx, txn)?;
        }
    }
    Ok(())
}

/// A DML probe's key values are expressions of their own, folded like any other.
/// Exactly one of the three arms is ever populated, but folding all of them keeps
/// this independent of which.
fn resolve_probe_keys(
    routing: &mut Option<Vec<DmlTarget>>,
    inherited: &mut [DmlTarget],
    probe: &mut Option<DmlIndexProbe>,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<(), ExecError> {
    let arms = routing
        .iter_mut()
        .flatten()
        .chain(inherited.iter_mut())
        .map(|target| &mut target.probe)
        .chain(std::iter::once(probe));
    for probe in arms.flatten() {
        for value in probe.key.exprs_mut() {
            resolve_expr(value, ctx, txn)?;
        }
    }
    Ok(())
}

fn resolve_exprs(
    exprs: &mut [BoundExpr],
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<(), ExecError> {
    for e in exprs {
        resolve_expr(e, ctx, txn)?;
    }
    Ok(())
}

fn resolve_opt(
    expr: &mut Option<BoundExpr>,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<(), ExecError> {
    if let Some(e) = expr {
        resolve_expr(e, ctx, txn)?;
    }
    Ok(())
}

fn resolve_returning(
    returning: &mut Option<Returning>,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<(), ExecError> {
    if let Some(r) = returning {
        resolve_exprs(&mut r.projections, ctx, txn)?;
    }
    Ok(())
}

fn resolve_join(
    join: &mut PhysicalJoinExpr,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<(), ExecError> {
    match join {
        PhysicalJoinExpr::Input {
            input, predicate, ..
        } => {
            match input {
                PhysicalJoinInput::Scan { .. } => {}
                PhysicalJoinInput::Subplan(source) => resolve_subqueries(source, ctx, txn)?,
                PhysicalJoinInput::TableFunction { args, .. } => resolve_exprs(args, ctx, txn)?,
            }
            resolve_opt(predicate, ctx, txn)?;
        }
        PhysicalJoinExpr::Join {
            left,
            right,
            predicate,
            hash_keys,
            ..
        } => {
            resolve_join(left, ctx, txn)?;
            resolve_join(right, ctx, txn)?;
            resolve_opt(predicate, ctx, txn)?;
            for key in hash_keys.iter_mut() {
                resolve_expr(&mut key.left, ctx, txn)?;
                resolve_expr(&mut key.right, ctx, txn)?;
            }
        }
        PhysicalJoinExpr::Lateral {
            left, predicate, ..
        } => {
            resolve_join(left, ctx, txn)?;
            resolve_opt(predicate, ctx, txn)?;
            // The lateral side is deliberately skipped: its markers may read the
            // left row, which does not exist yet. `LateralJoin` resolves them
            // once per left row, after substitution.
        }
    }
    Ok(())
}

/// Recurse an expression tree, resolving nested subqueries bottom-up, then fold
/// this node if it is itself a subquery marker.
fn resolve_expr(
    expr: &mut BoundExpr,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<(), ExecError> {
    match expr {
        BoundExpr::Const { .. }
        | BoundExpr::ColumnRef { .. }
        | BoundExpr::Param { .. }
        | BoundExpr::OuterColumnRef { .. } => {}
        BoundExpr::Unary { expr, .. }
        | BoundExpr::IsNull { expr, .. }
        | BoundExpr::BoolTest { expr, .. }
        | BoundExpr::Coerce { expr, .. }
        | BoundExpr::Collate { expr, .. }
        | BoundExpr::Reinterpret { expr, .. }
        | BoundExpr::CoerceToDomain { expr, .. } => resolve_expr(expr, ctx, txn)?,
        BoundExpr::Binary { left, right, .. } => {
            resolve_expr(left, ctx, txn)?;
            resolve_expr(right, ctx, txn)?;
        }
        BoundExpr::FuncCall { args, .. }
        | BoundExpr::Routine { args, .. }
        | BoundExpr::Srf { args, .. }
        | BoundExpr::Coalesce { args, .. }
        | BoundExpr::MinMax { args, .. } => {
            resolve_exprs(args, ctx, txn)?;
        }
        BoundExpr::ArrayCtor { elems, .. } => resolve_exprs(elems, ctx, txn)?,
        BoundExpr::Subscript { base, index, .. } => {
            resolve_expr(base, ctx, txn)?;
            resolve_expr(index, ctx, txn)?;
        }
        BoundExpr::Case { whens, else_, .. } => {
            for (cond, result) in whens.iter_mut() {
                resolve_expr(cond, ctx, txn)?;
                resolve_expr(result, ctx, txn)?;
            }
            if let Some(e) = else_ {
                resolve_expr(e, ctx, txn)?;
            }
        }
        BoundExpr::Aggregate {
            agg_args, order_by, ..
        } => {
            for a in BoundExpr::agg_exprs_mut(agg_args, order_by) {
                resolve_expr(a, ctx, txn)?;
            }
        }
        // A window marker is extracted before planning, so this is unreachable
        // for a *bound* plan — but the same walk runs over a `Window` node's own
        // expressions, so recursing keeps a subquery in an argument or an OVER
        // clause foldable rather than silently skipped.
        BoundExpr::WindowFunc { kind, spec, .. } => {
            for a in kind.args_mut().iter_mut().chain(spec.exprs_mut()) {
                resolve_expr(a, ctx, txn)?;
            }
        }
        // The IN / ANY / ALL needle (in `cmp`) may itself hold a subquery; fold
        // those first.
        BoundExpr::QuantifiedSubquery { cmp, .. } => resolve_expr(cmp, ctx, txn)?,
        // `x op ANY/ALL(array)` is an ordinary expression, not a foldable marker;
        // recurse into both operands so any nested subqueries fold.
        BoundExpr::QuantifiedArray { array, cmp, .. } => {
            resolve_expr(array, ctx, txn)?;
            resolve_expr(cmp, ctx, txn)?;
            // A literal `ARRAY[…]` is row-invariant, so build its `Value::Array`
            // once here rather than rebuilding it for every row in `eval`.
            if let Some(folded) = fold_const_array(array) {
                **array = folded;
            }
        }
        BoundExpr::ScalarSubquery { .. }
        | BoundExpr::ArraySubquery { .. }
        | BoundExpr::Exists { .. } => {}
    }
    // A correlated subquery cannot fold to a constant here — its value depends on
    // the outer row — so leave the marker for `eval` to fold per row. Only
    // non-correlated markers fold once, up front.
    if is_foldable_subquery(expr) {
        let taken = std::mem::replace(
            expr,
            BoundExpr::Const {
                value: Value::Null,
                ty: PgType::Bool,
            },
        );
        *expr = fold_subquery(taken, ctx, txn)?;
    }
    Ok(())
}

/// Whether `expr` is a subquery marker that can be folded before execution: one
/// whose subplan has no correlated outer reference. A correlated marker is left
/// in place for per-outer-row folding in `eval`.
fn is_foldable_subquery(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::ScalarSubquery { subplan, .. }
        | BoundExpr::ArraySubquery { subplan, .. }
        | BoundExpr::Exists { subplan, .. }
        | BoundExpr::QuantifiedSubquery { subplan, .. } => {
            !crabgresql_binder::plan_has_outer_refs(&subplan.plan)
        }
        _ => false,
    }
}

/// Run a subquery marker's subplan once and fold it to a plain expression.
fn fold_subquery(
    expr: BoundExpr,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<BoundExpr, ExecError> {
    match expr {
        BoundExpr::ScalarSubquery { subplan, ty } => {
            let rows = run_subplan(*subplan.plan, ctx, txn)?;
            Ok(BoundExpr::Const {
                value: scalar_subquery_value(rows, ty, ctx)?,
                ty,
            })
        }
        BoundExpr::ArraySubquery { subplan, elem, ty } => {
            let rows = run_subplan(*subplan.plan, ctx, txn)?;
            Ok(BoundExpr::Const {
                value: array_subquery_value(rows, elem, ctx)?,
                ty,
            })
        }
        BoundExpr::Exists { subplan, negated } => {
            // EXISTS only needs to know whether a row exists, so stop at the first
            // one rather than draining the whole subplan; the binder already
            // stripped the target list to a constant so no per-row projection
            // (or its errors) is evaluated. NOT EXISTS inverts the test.
            let exists = subplan_has_rows(*subplan.plan, ctx, txn)?;
            Ok(BoundExpr::Const {
                value: Value::Bool(exists != negated),
                ty: PgType::Bool,
            })
        }
        // The subquery runs once here; its candidate values become a constant
        // array so the per-row work reuses the single `QuantifiedArray`
        // evaluation path (which evaluates the needle exactly once per row).
        BoundExpr::QuantifiedSubquery { subplan, all, cmp } => {
            let rows = run_subplan(*subplan.plan, ctx, txn)?;
            let elem = hole_ty(&cmp).unwrap_or(PgType::Text);
            Ok(BoundExpr::QuantifiedArray {
                array: Box::new(BoundExpr::Const {
                    value: Value::Array {
                        elem,
                        elems: dedup_candidates(subquery_column(rows)),
                    },
                    ty: PgType::Array(elem.oid()),
                }),
                all,
                cmp,
            })
        }
        // Not a subquery marker (unreachable — the caller matched one).
        other => Ok(other),
    }
}

/// Drop candidates a quantified comparison would ask the same question of twice.
///
/// `x op ANY/ALL (SELECT …)` compares the needle against every candidate for
/// every outer row, so a subquery returning 10 000 rows over 100 distinct values
/// does a hundred times the work it needs to. Two equal candidates give the same
/// answer under any of the six comparison operators — they compare identically
/// against everything, being equal — and duplicate NULLs are equally redundant,
/// since the evaluator only records *that* it saw one.
///
/// The type is read off the values themselves rather than from the comparison's
/// hole: the candidates arrive as the subquery produced them and are coerced
/// only later, per row, so `float_col IN (SELECT num_col …)` hands numerics to a
/// float8 comparison. Values of mixed types, or of a type whose hash does not
/// separate them the way its comparison does (interval, inet, …), are left as
/// they came — the same refusal the hash join makes.
///
/// Deduplicating on the *source* type is conservative in the right direction:
/// coercion is a function, so values equal before it are equal after it, while
/// values it would map together are simply kept apart.
fn dedup_candidates(values: Vec<Value>) -> Vec<Value> {
    let Some(elem) = values.iter().find_map(Value::pg_type) else {
        return values;
    };
    let uniform = values
        .iter()
        .all(|value| value.pg_type().is_none_or(|ty| ty == elem));
    if values.len() < 2 || !uniform || !elem.hashes_distinctly() {
        return values;
    }
    let tys = [elem];
    let mut seen: FxHashMap<u64, Vec<Value>> = FxHashMap::default();
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let bucket = seen.entry(agg::hash_key(&tys, &[&value])).or_default();
        // A bucket hit can be a hash collision, so confirm on the values.
        if bucket
            .iter()
            .any(|other| agg::keys_equal(&tys, &[other], &[&value]))
        {
            continue;
        }
        bucket.push(value.clone());
        out.push(value);
    }
    out
}

/// The value a scalar subquery folds to from its materialized `rows`: no row →
/// NULL, one row → its single column coerced to `ty` (the type the outer
/// operator was bound against — a set-op / promoted column can be narrower), and
/// more than one row → the `21000` cardinality violation.
fn scalar_subquery_value(
    rows: Vec<Tuple>,
    ty: PgType,
    ctx: &ExecContext,
) -> Result<Value, ExecError> {
    match rows.len() {
        0 => Ok(Value::Null),
        1 => {
            let value = rows
                .into_iter()
                .next()
                .and_then(|row| row.into_iter().next())
                .unwrap_or(Value::Null);
            coerce_value(value, ty, ctx)
        }
        _ => Err(ExecError::new(
            crabgresql_pg_wire::sqlstate::CARDINALITY_VIOLATION,
            "more than one row returned by a subquery used as an expression",
        )),
    }
}

/// The array an `ARRAY(SELECT …)` folds to from its materialized `rows`.
///
/// The coercion is there for the reason [`scalar_subquery_value`] coerces: a
/// set-op or promoted column can arrive narrower than the type the array was
/// bound against. No row count is an error, and none means NULL — zero rows
/// give `{}`, a divergence invisible until `array_to_string(array(…), ',')` of
/// an empty set returns NULL instead of `''`.
fn array_subquery_value(
    rows: Vec<Tuple>,
    elem: PgType,
    ctx: &ExecContext,
) -> Result<Value, ExecError> {
    let elems = subquery_column(rows)
        .into_iter()
        .map(|value| coerce_value(value, elem, ctx))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Value::Array { elem, elems })
}

/// Evaluate a *correlated* subquery marker for one outer `row`: the per-row
/// counterpart of `fold_subquery`. The value is a scalar subquery's single
/// value, an `ARRAY(SELECT …)`'s array, `EXISTS` as a bool, or a quantified
/// `IN`/`op ANY`/`op ALL` as the
/// comparison's answer for the outer needle (evaluated against `row`). Getting
/// it need not run the subplan for this row: a hashed `EXISTS` probes a table
/// built once for the whole statement, and a memo hit reuses the answer an
/// earlier row with the same correlation key produced. Only when neither
/// applies is the subplan cloned, its outer references filled from `row` (via
/// `crabgresql_binder::substitute_outer`), and re-executed. Called from `eval`
/// when it reaches a marker `resolve_subqueries` left in place, which only
/// happens under a real `execute`, so `ctx.txn` is present.
pub(crate) fn eval_correlated_subquery(
    marker: &BoundExpr,
    row: &[Value],
    ctx: &ExecContext,
) -> Result<Value, ExecError> {
    let txn = ctx.txn.as_ref().ok_or_else(|| {
        ExecError::new(
            crabgresql_pg_wire::sqlstate::INTERNAL_ERROR,
            "correlated subquery evaluated without a transaction context",
        )
    })?;
    // Every correlated marker carries a subplan; read it out once, since each
    // of the three paths below — hashed, memoized, re-run — starts from it.
    let subplan = match marker {
        BoundExpr::ScalarSubquery { subplan, .. }
        | BoundExpr::ArraySubquery { subplan, .. }
        | BoundExpr::Exists { subplan, .. }
        | BoundExpr::QuantifiedSubquery { subplan, .. } => subplan,
        // `eval` only calls this for a subquery marker.
        _ => {
            return Err(ExecError::new(
                crabgresql_pg_wire::sqlstate::INTERNAL_ERROR,
                "eval_correlated_subquery called on a non-subquery expression",
            ));
        }
    };
    // An `EXISTS` whose correlation is plain equality is answered from a hash
    // table built once for the whole statement — no clone, no plan, no scan.
    if let BoundExpr::Exists { negated, .. } = marker
        && let Some(hashed) = subplan::hashed_exists(subplan, ctx, txn)?
    {
        return Ok(Value::Bool(hashed.probe(row, ctx)? != *negated));
    }
    // Otherwise the subplan really is re-run — but only for an outer row that
    // does not agree with an earlier one on every slot the subplan reads. A
    // quantified marker is excluded: its needle is evaluated against the outer
    // row *outside* the subplan, so the subplan's own slots do not determine the
    // answer.
    let memo = match marker {
        BoundExpr::QuantifiedSubquery { .. } => None,
        _ => subplan::memo_key(subplan, row, ctx),
    };
    if let Some(key) = &memo
        && let Some(hit) = subplan::memo_get(key, ctx)
    {
        return Ok(hit);
    }

    let mut logical = (*subplan.plan).clone();
    crabgresql_binder::substitute_outer(&mut logical, row);
    let value = match marker {
        BoundExpr::ScalarSubquery { ty, .. } => {
            let rows = run_subplan(logical, ctx, txn)?;
            scalar_subquery_value(rows, *ty, ctx)?
        }
        BoundExpr::ArraySubquery { elem, .. } => {
            let rows = run_subplan(logical, ctx, txn)?;
            array_subquery_value(rows, *elem, ctx)?
        }
        BoundExpr::Exists { negated, .. } => {
            let exists = subplan_has_rows(logical, ctx, txn)?;
            Value::Bool(exists != *negated)
        }
        BoundExpr::QuantifiedSubquery { all, cmp, .. } => {
            let rows = run_subplan(logical, ctx, txn)?;
            // The template's needle reads the current row, so the quantifier is
            // evaluated against `row` (needle once, then each candidate).
            eval_quantified(cmp, &subquery_column(rows), *all, row, ctx)?
        }
        // Unreachable: `subplan` above already matched every one of these.
        _ => Value::Null,
    };
    if let Some(key) = memo {
        subplan::memo_put(key, &value, ctx);
    }
    Ok(value)
}

/// Plan and execute a subplan, draining its result set into materialized rows.
///
/// Planned, not [`optimize_and_plan`]ed, and deliberately so: a subplan's body
/// was already rewritten once, with the statement that encloses it (the
/// optimizer descends into every subquery marker it walks). Optimizing again
/// here would buy only the folding of the constants `substitute_outer` just
/// baked in — and it would buy it once per *outer row*, which is exactly the
/// shape (a correlated subquery) where per-row work is already the problem.
pub(crate) fn run_subplan(
    logical: LogicalPlan,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Vec<Tuple>, ExecError> {
    match execute(crabgresql_planner::plan(logical, ctx.costs), ctx, txn)? {
        Execution::Rows { node, .. } => drain(node),
        _ => Err(ExecError::new(
            crabgresql_pg_wire::sqlstate::INTERNAL_ERROR,
            "subquery did not produce a result set",
        )),
    }
}

/// Whether a subplan yields at least one row, stopping at the first — for
/// `EXISTS`, which needs existence only, not the rows themselves. Planned
/// without a rewrite pass for the reason [`run_subplan`] gives.
fn subplan_has_rows(
    logical: LogicalPlan,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<bool, ExecError> {
    match execute(crabgresql_planner::plan(logical, ctx.costs), ctx, txn)? {
        Execution::Rows { mut node, .. } => Ok(node.next()?.is_some()),
        _ => Err(ExecError::new(
            crabgresql_pg_wire::sqlstate::INTERNAL_ERROR,
            "subquery did not produce a result set",
        )),
    }
}

fn drain(mut node: Box<dyn ExecNode>) -> Result<Vec<Tuple>, ExecError> {
    let mut out = Vec::new();
    while let Some(tuple) = node.next()? {
        out.push(tuple);
    }
    Ok(out)
}

/// The first column of a subquery's materialized rows — the candidate values a
/// quantified comparison (`op ANY/ALL (SELECT …)`) tests against. An empty row
/// contributes a NULL.
fn subquery_column(rows: Vec<Tuple>) -> Vec<Value> {
    rows.into_iter()
        .map(|mut row| {
            if row.is_empty() {
                Value::Null
            } else {
                row.swap_remove(0)
            }
        })
        .collect()
}

/// Evaluate `left op ANY/ALL (values)` against one `row`.
///
/// The needle is evaluated **exactly once** — PG's `ScalarArrayOpExpr` evaluates
/// its scalar side once, so a volatile or side-effecting needle
/// (`nextval('s') = ANY(…)`, `random() > ALL(…)`) must not be re-run per
/// candidate. Each candidate is then substituted into the `cmp` template's
/// `<hole>` (preserving the coercions the binder resolved) and compared against
/// that single needle value.
///
/// Three-valued logic, matching the OR/AND chain this replaces: `ANY` yields
/// true on the first match, `ALL` false on the first mismatch (both
/// short-circuit); otherwise a NULL anywhere makes the result NULL, and an
/// exhausted set yields the quantifier's identity (`ANY` ⇒ false, `ALL` ⇒ true,
/// so an empty set is false / vacuously true).
fn eval_quantified(
    cmp: &BoundExpr,
    values: &[Value],
    all: bool,
    row: &[Value],
    ctx: &ExecContext,
) -> Result<Value, ExecError> {
    // Without the binder's NULL `Const` placeholder there is nothing to
    // substitute a candidate into, and every comparison would silently be
    // `needle op NULL` — i.e. a wrong answer with no error. Fail loudly instead.
    if hole_ty(cmp).is_none() {
        return Err(ExecError::new(
            crabgresql_pg_wire::sqlstate::INTERNAL_ERROR,
            "ANY/ALL comparison template has no candidate placeholder",
        ));
    }
    let BoundExpr::Binary {
        op,
        arg_ty,
        collation,
        left,
        right,
    } = cmp
    else {
        return eval_quantified_call(cmp, values, all, row, ctx);
    };
    // The one and only evaluation of the needle. Borrowed when it is a column
    // or a literal, since every comparison below only reads it.
    let needle_slot;
    let needle = eval::eval_ref!(needle_slot, left, row, ctx);
    let needle_null = matches!(needle, Value::Null);
    let mut saw_null = needle_null;
    // The common case is a bare hole (`Const { Null, ty }`): the candidate only
    // needs the hole's coercion, with no node to build and evaluate per element.
    let bare_hole = match right.as_ref() {
        BoundExpr::Const {
            value: Value::Null,
            ty,
        } => Some(*ty),
        _ => None,
    };
    for value in values {
        // Candidates are coerced even once the result can only be NULL, since a
        // coercion failure is observable — as it was when this was an OR/AND
        // chain that evaluated every element. A cast the hole does not actually
        // need is borrowed past instead, so a large candidate set is not cloned
        // element by element just to hand each one straight back.
        let candidate_slot;
        let candidate = match bare_hole {
            Some(ty) if cast::is_identity_cast(value, ty) => value,
            Some(ty) => {
                candidate_slot = eval::coerce_value(value.clone(), ty, ctx)?;
                &candidate_slot
            }
            None => {
                candidate_slot =
                    eval::eval(&substitute_hole(right, value.clone(), ctx)?, row, ctx)?;
                &candidate_slot
            }
        };
        // Only *this* candidate (or the needle) being NULL skips the comparison;
        // an earlier NULL must not mask a later decisive one, so
        // `1 = ANY(ARRAY[NULL, 1])` is still true rather than NULL.
        if needle_null || matches!(candidate, Value::Null) {
            saw_null = true;
            continue;
        }
        if eval::apply_comparison(*op, *arg_ty, *collation, needle, candidate) != all {
            // ANY found a match, or ALL found a counterexample.
            return Ok(Value::Bool(!all));
        }
    }
    if saw_null {
        Ok(Value::Null)
    } else {
        Ok(Value::Bool(all))
    }
}

/// [`eval_quantified`] for the operator spellings that lower to a call rather
/// than a `Binary` — `~~`, `~`, `@>`, `<<` on `inet`, `~=` on geometry. There is
/// no `apply_comparison` to hand two values to, so the filled template is built
/// and evaluated per candidate; that cost is why `Binary` keeps its own path.
fn eval_quantified_call(
    cmp: &BoundExpr,
    values: &[Value],
    all: bool,
    row: &[Value],
    ctx: &ExecContext,
) -> Result<Value, ExecError> {
    let template = fold_off_hole_path(cmp, row, ctx)?;
    let mut saw_null = false;
    for value in values {
        // Every candidate is coerced even once the result can only be NULL,
        // since a coercion failure is observable.
        let filled = substitute_hole(&template, value.clone(), ctx)?;
        match eval::eval(&filled, row, ctx)? {
            Value::Null => saw_null = true,
            Value::Bool(b) if b != all => return Ok(Value::Bool(!all)),
            _ => {}
        }
    }
    if saw_null {
        Ok(Value::Null)
    } else {
        Ok(Value::Bool(all))
    }
}

/// Replace every subtree that is not on the hole's path by the constant it
/// evaluates to — the needle above all, which is what keeps it from being
/// re-evaluated per candidate. Descends exactly as [`substitute_hole`] does, so
/// the two can never disagree about which node the hole is.
fn fold_off_hole_path(
    expr: &BoundExpr,
    row: &[Value],
    ctx: &ExecContext,
) -> Result<BoundExpr, ExecError> {
    let fold = |e: &BoundExpr| -> Result<BoundExpr, ExecError> {
        Ok(BoundExpr::Const {
            value: eval::eval(e, row, ctx)?,
            ty: e.ty(),
        })
    };
    match expr {
        BoundExpr::Const { .. } => Ok(expr.clone()),
        BoundExpr::Unary { op, expr } => Ok(BoundExpr::Unary {
            op: *op,
            expr: Box::new(fold_off_hole_path(expr, row, ctx)?),
        }),
        BoundExpr::Coerce { expr, ty } => Ok(BoundExpr::Coerce {
            expr: Box::new(fold_off_hole_path(expr, row, ctx)?),
            ty: *ty,
        }),
        BoundExpr::Reinterpret {
            expr,
            reported,
            rep,
        } => Ok(BoundExpr::Reinterpret {
            expr: Box::new(fold_off_hole_path(expr, row, ctx)?),
            reported: *reported,
            rep: *rep,
        }),
        BoundExpr::Collate {
            expr,
            collation,
            explicit,
        } => Ok(BoundExpr::Collate {
            expr: Box::new(fold_off_hole_path(expr, row, ctx)?),
            collation: *collation,
            explicit: *explicit,
        }),
        BoundExpr::FuncCall { func, ret, args } => {
            let last = args.len().saturating_sub(1);
            let args = args
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    if i == last {
                        fold_off_hole_path(a, row, ctx)
                    } else {
                        fold(a)
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(BoundExpr::FuncCall {
                func: *func,
                ret: *ret,
                args,
            })
        }
        other => fold(other),
    }
}

/// Fold an all-constant `ARRAY[…]` constructor into the `Value::Array` it always
/// produces, so a quantified comparison over a literal array borrows one
/// constant instead of rebuilding the array per row. `None` when any element is
/// non-constant (a column/param reference), which must stay per-row.
fn fold_const_array(expr: &BoundExpr) -> Option<BoundExpr> {
    let BoundExpr::ArrayCtor { elem, ty, elems } = expr else {
        return None;
    };
    let values = elems
        .iter()
        .map(|e| match e {
            BoundExpr::Const { value, .. } => Some(value.clone()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some(BoundExpr::Const {
        value: Value::Array {
            elem: *elem,
            elems: values,
        },
        ty: *ty,
    })
}

/// The declared type of a template's `<hole>`, reached by the same last-argument
/// descent [`substitute_hole`] uses. Used to label the constant array a folded
/// `op ANY/ALL (SELECT …)` becomes; `None` if the template has no hole.
fn hole_ty(cmp: &BoundExpr) -> Option<PgType> {
    fn find(expr: &BoundExpr) -> Option<PgType> {
        match expr {
            BoundExpr::Const {
                value: Value::Null,
                ty,
            } => Some(*ty),
            BoundExpr::Const { .. } => None,
            BoundExpr::Unary { expr, .. }
            | BoundExpr::Coerce { expr, .. }
            | BoundExpr::Reinterpret { expr, .. }
            | BoundExpr::Collate { expr, .. } => find(expr),
            BoundExpr::FuncCall { args, .. } => find(args.last()?),
            _ => None,
        }
    }
    match cmp {
        BoundExpr::Binary { right, .. } => find(right),
        other => find(other),
    }
}

/// Substitute a candidate value into the template's `<hole>` and coerce it to
/// that hole's declared type.
///
/// The hole is the operator's right operand, so it is the *last* argument of
/// whatever the binder lowered the operator to, under whichever coercion
/// wrappers that left (a `bpchar → text` cast is a `FuncCall`, not a `Coerce`)
/// and under the `NOT` of a negated `!~~`. Descending only the last argument is
/// what keeps a NULL *needle* — `NULL ~~ ANY(…)`, also a NULL `Const` — from
/// being mistaken for the hole.
fn substitute_hole(
    expr: &BoundExpr,
    value: Value,
    ctx: &ExecContext,
) -> Result<BoundExpr, ExecError> {
    match expr {
        // The placeholder: a NULL `Const` carrying the hole's declared type.
        BoundExpr::Const {
            value: Value::Null,
            ty,
        } => Ok(BoundExpr::Const {
            value: coerce_value(value, *ty, ctx)?,
            ty: *ty,
        }),
        // Any other constant (e.g. a coercion function's typmod argument) stays.
        BoundExpr::Const { .. } => Ok(expr.clone()),
        BoundExpr::Unary { op, expr } => Ok(BoundExpr::Unary {
            op: *op,
            expr: Box::new(substitute_hole(expr, value, ctx)?),
        }),
        BoundExpr::Coerce { expr, ty } => Ok(BoundExpr::Coerce {
            expr: Box::new(substitute_hole(expr, value, ctx)?),
            ty: *ty,
        }),
        BoundExpr::Reinterpret {
            expr,
            reported,
            rep,
        } => Ok(BoundExpr::Reinterpret {
            expr: Box::new(substitute_hole(expr, value, ctx)?),
            reported: *reported,
            rep: *rep,
        }),
        BoundExpr::Collate {
            expr,
            collation,
            explicit,
        } => Ok(BoundExpr::Collate {
            expr: Box::new(substitute_hole(expr, value, ctx)?),
            collation: *collation,
            explicit: *explicit,
        }),
        BoundExpr::FuncCall { func, ret, args } => {
            let mut args = args.clone();
            if let Some(last) = args.last_mut() {
                *last = substitute_hole(last, value, ctx)?;
            }
            Ok(BoundExpr::FuncCall {
                func: *func,
                ret: *ret,
                args,
            })
        }
        other => Ok(other.clone()),
    }
}

#[allow(clippy::too_many_arguments)]
/// A pipeline's row source, before it is decided how far up the columnar
/// segment reaches.
///
/// Holding the batch form rather than shredding at the scan is what lets a
/// vectorizable operator be pushed *below* the shred; a source that never had a
/// batch form stays [`Source::Rows`] and every such attempt simply declines.
enum Source {
    Rows(Box<dyn ExecNode>),
    Batches {
        node: Box<dyn vector::BatchNode>,
        layout: vector::BatchLayout,
        /// Which batch columns carry values. A scan's batch is full width, but
        /// the columns outside its `ColumnProjection` are all-NULL padding, and
        /// shredding those would undo the projection pushdown. An operator that
        /// builds its own columns (a projection, a sort) makes this dense.
        positions: Vec<usize>,
    },
}

impl Source {
    /// End the columnar segment, shredding back to tuples if it had begun.
    fn into_rows(self) -> Box<dyn ExecNode> {
        match self {
            Source::Rows(node) => node,
            Source::Batches {
                node,
                layout,
                positions,
            } => Box::new(vector::Shred::new(node, layout, positions)),
        }
    }

    /// Apply `predicate` columnar-side if it compiles, returning it unconsumed
    /// otherwise so the caller can build the row [`Filter`] instead.
    fn filter(self, predicate: BoundExpr) -> (Self, Option<BoundExpr>) {
        let Source::Batches {
            node,
            layout,
            positions,
        } = self
        else {
            return (self, Some(predicate));
        };
        // A filter drops rows, never columns, so `positions` is unchanged.
        match vector::expr::compile_predicate(&predicate, &layout) {
            Some(compiled) => (
                Source::Batches {
                    node: Box::new(vector::FilterBatch::new(node, compiled)),
                    layout,
                    positions,
                },
                None,
            ),
            None => (
                Source::Batches {
                    node,
                    layout,
                    positions,
                },
                Some(predicate),
            ),
        }
    }

    /// Apply the projection **and** the sort columnar-side, or neither.
    ///
    /// They are one decision because a [`SortKey`] indexes the projected tuple:
    /// a columnar sort is unreachable unless the projection below it also stayed
    /// columnar. The `bool` reports whether it happened; `false` hands the source
    /// back untouched and the caller builds the row `Projection`/`Sort` pair as
    /// it always did.
    ///
    /// `sort` must be non-empty — a columnar projection on its own gains
    /// nothing, since the row `Projection` it would replace does exactly the
    /// tuple-building work `Shred` would then have to do anyway.
    fn project_and_sort(
        self,
        projections: &[BoundExpr],
        sort: &[SortKey],
        visible_width: usize,
    ) -> Result<(Self, bool), ExecError> {
        let Source::Batches {
            node,
            layout,
            positions,
        } = self
        else {
            return Ok((self, false));
        };
        let projected = vector::ProjectBatch::layout(projections, &layout);
        let takes = vector::ProjectBatch::compile(projections, &layout);
        let takes = match takes {
            Some(takes)
                if vector::SortBatch::compilable(sort, &projected)
                    && visible_width <= projected.len() =>
            {
                takes
            }
            _ => {
                return Ok((
                    Source::Batches {
                        node,
                        layout,
                        positions,
                    },
                    false,
                ));
            }
        };
        let project = vector::ProjectBatch::new(node, takes, &projected);
        let sorted = vector::SortBatch::new(Box::new(project), sort, &projected, visible_width)?;
        Ok((
            Source::Batches {
                node: Box::new(sorted),
                // The sort dropped the hidden ORDER BY columns, so what remains
                // above it is the visible prefix of the projected layout.
                layout: Arc::from(&projected[..visible_width]),
                // Every surviving column was built by the projection, so there
                // is no NULL padding left to skip.
                positions: (0..visible_width).collect(),
            },
            true,
        ))
    }
}

/// A `Subquery`'s child as a pipeline source, keeping the batch form when the
/// child is a bare storage node with no tail of its own.
///
/// This is what lets an engine-managed relation vectorize its `WHERE`. Such a
/// relation reads as an `Append` over its storage leaves wrapped in a
/// `Subquery`, and the predicate lives on the **`Subquery`**, not on the
/// `Append` (see the planner's reduced-EXPLAIN comment on that variant). Going
/// through `execute` would shred at the `Append` and leave this level's filter
/// with nothing but rows — the columnar filter would never fire for the very
/// relations it exists to serve.
///
/// Only `Append` qualifies: it is the one variant that carries no
/// predicate/projection/sort of its own, so nothing is skipped by building it
/// directly. Every other child goes the ordinary way.
fn subquery_source(
    source: PhysicalPlan,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Source, ExecError> {
    if let PhysicalPlan::Append { arms, .. } = &source {
        return append_source(arms, ctx, txn);
    }
    let Execution::Rows { node, .. } = execute(source, ctx, txn)? else {
        return Err(ExecError::new(
            "XX000",
            "subquery source did not produce a row set",
        ));
    };
    Ok(Source::Rows(node))
}

/// The batch columns a scan under `projection` actually fills.
///
/// A batch is full width so a schema ordinal is a batch ordinal, but the
/// columns outside the projection are all-NULL padding. Naming only the real
/// ones keeps `Shred`'s per-row cost proportional to the query rather than to
/// the table's width.
fn scan_positions(projection: &ColumnProjection, width: usize) -> Vec<usize> {
    match projection {
        ColumnProjection::All => (0..width).collect(),
        ColumnProjection::Some(cols) => cols.to_vec(),
    }
}

/// The source for a single-table scan: columnar if the engine can hand up
/// batches, the plain row scan otherwise.
///
/// The choice is per-relation and invisible above the shred, so an engine
/// gaining or losing a batch path can never change a query's answer.
fn scan_source(
    table: &Arc<dyn TableAm>,
    txn: &TxnContext,
    projection: &ColumnProjection,
    system: Option<&SystemEmit>,
    ctx: &ExecContext,
) -> Result<Source, ExecError> {
    // A scan that appends slots stays on the row path: the batch layout is the
    // relation's schema, and the only batch-capable access methods reach the
    // executor as an `Append` over their storage leaves anyway.
    if let Some(emit) = system {
        return Ok(Source::Rows(scan_with_slots(
            table, emit, projection, ctx, txn,
        )?));
    }
    Ok(match vector::BatchScan::open(table, txn, projection, ctx) {
        Some(scan) => {
            let layout = vector::layout_of(&table.schema());
            let positions = scan_positions(projection, layout.len());
            Source::Batches {
                node: Box::new(scan),
                layout,
                positions,
            }
        }
        None => Source::Rows(Box::new(SeqScan::new(table, txn, projection, ctx))),
    })
}

/// The source for an `Append` over its arms — a partitioned parent, an
/// inheritance parent with its descendants, or the storage leaves of one
/// engine-managed relation.
///
/// All arms or none: [`vector::BatchAppend`] concatenates their outputs, so a
/// single row-only arm puts the whole node back on the row path rather than
/// mixing representations. An arm that remaps disqualifies the batch path
/// outright — see [`vector::BatchAppend::open`].
fn append_source(
    arms: &[PhysicalAppendArm],
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Source, ExecError> {
    // Only identity arms reach the batch path, and they all share the named
    // relation's layout, so any of them describes the batch. `Append` over zero
    // arms is not a shape the planner emits.
    let first = arms.first();
    // The batch is the relation's own columns plus whatever system slots the
    // arms append, in that order — so the layout the shred decodes against has
    // to be widened the same way, or it reads a row narrower than the projection
    // above it indexes into. (`BatchAppend::open` admits `tableoid` and nothing
    // else; the `map` below follows whatever it admits.)
    let layout = first.map(|arm| {
        let schema = arm.relation.table.schema();
        let mut columns = schema.columns.clone();
        columns.extend(
            arm.relation
                .system
                .iter()
                .flat_map(|emit| emit.cols.iter())
                .map(|c| crabgresql_storage_api::Column::new(c.name(), c.ty())),
        );
        vector::layout_from(columns)
    });
    Ok(match (vector::BatchAppend::open(arms, txn, ctx), layout) {
        (Some(append), Some(layout)) => {
            let declared = first
                .expect("layout implies an arm")
                .relation
                .table
                .schema()
                .columns
                .len();
            let mut positions =
                scan_positions(&first.expect("layout implies an arm").projection, declared);
            // A system slot is never in the scan's projection — that addresses
            // stored columns — but it is always in the batch, and always read.
            positions.extend(declared..layout.len());
            Source::Batches {
                node: Box::new(append),
                layout,
                positions,
            }
        }
        _ => Source::Rows(Box::new(Append::new(arms, ctx, txn)?)),
    })
}

/// Wrap a source node in the standard `Filter -> Projection -> Sort` tail and
/// package it as a streamable result set. Shared by table scans, subquery
/// sources, and set-returning functions (every SELECT-shaped plan with a
/// projection list).
fn project_pipeline(
    source: Source,
    projections: Vec<BoundExpr>,
    predicate: Option<BoundExpr>,
    sort: Vec<SortKey>,
    distinct: Option<Vec<DistinctKey>>,
    columns: Vec<OutputColumn>,
    ctx: &ExecContext,
) -> Result<Execution, ExecError> {
    // Offer the predicate to the columnar segment first: a filter that runs on
    // batches removes rows before `Shred` ever builds a tuple for them, which is
    // where vectorizing actually pays. Whatever it declines comes back to be
    // applied by the row `Filter`, unchanged.
    let (source, predicate) = match predicate {
        Some(predicate) => source.filter(predicate),
        None => (source, None),
    };
    // The projected tuple width, including any hidden ORDER BY / DISTINCT ON
    // columns appended past the visible output — captured before `projections`
    // is consumed so a Distinct can keep those columns through the sort.
    let full_width = projections.len();

    // Then the projection and sort, but only together, and only when three
    // things hold:
    //
    // * `predicate` is gone. If the columnar filter declined it, it is still
    //   waiting for the row `Filter` below — and that filter has to run *before*
    //   the projection, or rows the WHERE excludes would be projected, sorted
    //   and returned.
    // * there is no DISTINCT, which is a row node and needs the hidden ORDER BY
    //   columns that the sort would have dropped.
    // * there is a sort at all; a columnar projection alone gains nothing.
    let (source, sorted) = match predicate.is_none() && distinct.is_none() && !sort.is_empty() {
        true => source.project_and_sort(&projections, &sort, columns.len())?,
        false => (source, false),
    };
    if sorted {
        // Projection and sort are both already done, columnar-side.
        return Ok(Execution::Rows {
            columns,
            node: source.into_rows(),
        });
    }

    let mut node = source.into_rows();
    if let Some(predicate) = predicate {
        node = Box::new(Filter::new(node, predicate, ctx.clone()));
    }
    // A set-returning function in the target list turns one input row into many,
    // so it needs `ProjectSet` rather than the one-in/one-out `Projection`.
    node = if projections.iter().any(BoundExpr::is_srf) {
        Box::new(ProjectSet::new(node, projections, ctx.clone()))
    } else {
        Box::new(Projection::new(node, projections, ctx.clone()))
    };
    node = finish_sort_distinct(node, sort, distinct, full_width, &columns)?;
    Ok(Execution::Rows { columns, node })
}

/// Apply the ORDER BY and DISTINCT tail. Without DISTINCT this is just the sort
/// (which trims hidden columns). With DISTINCT the sort must run first but keep
/// its hidden columns, so it is built with a no-op width and the `Distinct` node
/// performs the final trim to the visible output width.
fn finish_sort_distinct(
    node: Box<dyn ExecNode>,
    sort: Vec<SortKey>,
    distinct: Option<Vec<DistinctKey>>,
    full_width: usize,
    columns: &[OutputColumn],
) -> Result<Box<dyn ExecNode>, ExecError> {
    let Some(keys) = distinct else {
        return maybe_sort(node, sort, columns);
    };
    let mut node = node;
    if !sort.is_empty() {
        node = Box::new(Sort::new(node, sort, full_width)?);
    }
    Ok(Box::new(Distinct::new(node, keys, columns.len())?))
}

/// Evaluate an expression that references no row — a `CALL` argument, which
/// has no row for a column reference to come from. The binder has already
/// rejected any column reference, so the empty row is never indexed.
pub fn eval_row_free(expr: &BoundExpr, ctx: &ExecContext) -> Result<Value, ExecError> {
    eval(expr, &[], ctx)
}

/// Project a bound `RETURNING` list over the rows a DML statement affects,
/// eagerly. The projection must run *before* the caller commits: a faulting
/// RETURNING expression (division by zero, a failed cast) then propagates out of
/// `execute` and aborts the statement, rolling the mutation back — matching
/// PostgreSQL, and unlike a lazy node that would fault mid-stream after the
/// write already committed. RETURNING is scalar one-in/one-out (the binder
/// rejects aggregates and set-returning functions), so this is one output row
/// per affected row.
/// `system`, when set, names the system columns the statement reads together
/// with one [`SysSource`] per affected row, positionally aligned with
/// `affected`. The binder put those slots past the target's declared columns, so
/// the projection reads them as ordinary columns; widening happens here, per
/// row, because *which* relation a row belongs to — and which version it was —
/// is only settled by routing or by the fan-out loop.
fn project_returning<'a>(
    affected: impl IntoIterator<Item = &'a Tuple>,
    projections: &[BoundExpr],
    system: Option<(&[SysCol], &[SysSource])>,
    ctx: &ExecContext,
) -> Result<Vec<Tuple>, ExecError> {
    let mut out = Vec::new();
    for (i, row) in affected.into_iter().enumerate() {
        // Only the statements that read a system column pay for the widened copy.
        let widened;
        let row: &Tuple = match system {
            None => row,
            Some((cols, sources)) => {
                widened = sources[i].widen(row, cols);
                &widened
            }
        };
        out.push(
            projections
                .iter()
                .map(|expr| eval(expr, row, ctx))
                .collect::<Result<Tuple, _>>()?,
        );
    }
    Ok(out)
}

/// The per-row data a DML statement's system slots are filled from: which
/// relation the row lives in, which version it is, and — for a statement reading
/// `xmin`/`xmax`/`cmin`/`cmax` — that version's MVCC header.
///
/// The OID is resolved once per target and the rest comes off the scan, so
/// building one costs a copy of a handful of integers.
#[derive(Clone, Copy)]
pub(crate) struct SysSource {
    oid: Option<u32>,
    tid: Tid,
    hdr: Option<TupleHeader>,
}

impl SysSource {
    /// A source for a row that is being formed rather than read — an INSERT's
    /// new version, which has no tid until it is placed.
    fn placed(oid: Option<u32>, tid: Tid, hdr: Option<TupleHeader>) -> Self {
        SysSource { oid, tid, hdr }
    }

    /// `row` with the slots `cols` names appended — the widened copy a
    /// predicate, a SET expression or a RETURNING projection binds against.
    fn widen(&self, row: &[Value], cols: &[SysCol]) -> Tuple {
        let mut wide = Vec::with_capacity(row.len() + cols.len());
        wide.extend_from_slice(row);
        push_system(&mut wide, cols, self.oid, self.tid, self.hdr.as_ref());
        wide
    }
}

/// Each DML target's own OID, resolved once per relation. `None` for a target
/// whose statement never reads `tableoid`.
fn target_oids(targets: &[DmlTarget], ctx: &ExecContext) -> Result<Vec<Option<u32>>, ExecError> {
    targets
        .iter()
        .map(|target| {
            target
                .relation
                .system
                .as_ref()
                .filter(|emit| emit.cols.contains(&SysCol::TableOid))
                .map(|emit| resolve_tableoid(&emit.ident, ctx))
                .transpose()
        })
        .collect()
}

/// Package eagerly-projected `RETURNING` output rows as a streamable result.
fn returning_rows(output: Vec<Tuple>, columns: Vec<OutputColumn>, verb: DmlVerb) -> Execution {
    Execution::ReturningRows {
        columns,
        node: Box::new(MaterializedRows::new(output)),
        verb,
    }
}

/// Statement atomicity: evaluate everything first, mutate only after nothing
/// can fail, so a failure in a later row leaves no earlier rows behind. The
/// writes are stamped with `txn`'s XID and become durable/visible only when the
/// transaction commits — unless `freeze` is set, which stamps them frozen and so
/// visible at once (`COPY … FREEZE`; the caller has verified that a rollback
/// discards this target's storage).
///
/// `freeze` is applied to a *derived* context handed to the `insert_many` calls
/// below, and never to the ambient `txn` or to [`ExecContext::txn`]. That is what
/// keeps it off a column `DEFAULT` that calls a routine: the routine reads the
/// context out of `ctx` (see `eval`), writes to relations nobody checked, and
/// freezing those rows would leave them visible after a rollback.
#[allow(clippy::too_many_arguments)]
fn execute_insert(
    table: &Arc<dyn TableAm>,
    source: PhysicalInsertSource,
    returning: Option<Returning>,
    routing: Option<Vec<Arc<dyn TableAm>>>,
    freeze: bool,
    system: &[SysCol],
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    // Materialize every source tuple before any write. Draining a query source
    // to completion first is what makes `INSERT INTO t SELECT ... FROM t` read
    // only pre-insert rows (no Halloween problem), and lets validation/routing
    // see a stable set so the statement stays all-or-nothing.
    let (tuples, notnull_verified) = collect_insert_tuples(source, ctx, txn)?;
    match routing {
        // Partitioned parent: route each row to the leaf whose RANGE bound admits
        // its key and write there. `notnull_verified` indexes the parent's shape,
        // so it says nothing about the leaf a row is checked against — ignored
        // here, and empty by construction for the one source that builds it.
        Some(leaves) => insert_routed(table, tuples, returning, &leaves, freeze, system, ctx, txn),
        // Ordinary table: rows go straight to `table`.
        None => insert_direct(
            table,
            tuples,
            returning,
            &notnull_verified,
            freeze,
            system,
            ctx,
            txn,
        ),
    }
}

/// The context a target's rows are written under: `txn` itself, or a frozen
/// derivation of it. A separate step so the freeze is visible at the write and
/// reaches nothing else — reads, constraint checks and `RETURNING` keep using the
/// plain context.
///
/// Returns an `Option` the caller unwraps against `txn` rather than an owned
/// context, so the overwhelmingly common unfrozen path borrows instead of
/// cloning: a `TxnContext` carries a `Snapshot` whose in-progress list is a
/// `Vec<Xid>` as long as the number of transactions in flight, plus two `Arc`s.
/// Every ordinary INSERT would otherwise pay an allocation and two contended
/// refcount bumps for a feature it is not using.
fn write_context(freeze: bool, txn: &TxnContext) -> Option<TxnContext> {
    freeze.then(|| txn.with_freeze())
}

/// Evaluate an INSERT's source into fully-formed, schema-order tuples, plus the
/// columns the source vouches are non-NULL in every one of them (empty unless it
/// arrived already proven — see [`PhysicalInsertSource::Tuples`]). No validation
/// or writing happens here; the caller does both after the whole source is
/// consumed.
///
/// Each tuple is pre-sized and filled by hand rather than `collect`ed. The
/// fallible-iterator shunt behind `collect::<Result<Vec<_>, _>>()` reports a
/// lower size hint of zero, so the `Vec` grows geometrically and lands on the
/// next power of two — 128 slots for a 105-column row. At `size_of::<Value>()`
/// a slot that is dead weight in every tuple the statement produces, and for a
/// buffered relation it stays resident until the rows are flushed.
fn collect_insert_tuples(
    source: PhysicalInsertSource,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<(Vec<Tuple>, Vec<u32>), ExecError> {
    let mut tuples: Vec<Tuple> = Vec::new();
    let mut verified: Vec<u32> = Vec::new();
    match source {
        PhysicalInsertSource::Values(rows) => {
            for row in rows {
                let mut tuple = Tuple::with_capacity(row.len());
                for expr in row {
                    // A constant is already the value the tuple wants, so take
                    // it rather than asking `eval` to clone it back out. The
                    // rows are ours — nothing reads this plan again — and for a
                    // bulk load that clone is a second deep copy of every text
                    // cell. Everything else still evaluates: `VALUES` holds a
                    // `Coerce` for a session-dependent literal, a `FuncCall` for
                    // a `nextval`/`now()` default, a routine call, a subquery.
                    tuple.push(match expr {
                        BoundExpr::Const { value, .. } => value,
                        expr => eval(&expr, &[], ctx)?,
                    });
                }
                tuples.push(tuple);
            }
        }
        // Already-formed rows: only the columns whose default did not fold are
        // left to fill. Row-major and in ascending column order, which is the
        // order the `Values` path evaluates a full-width row in — what makes a
        // `serial` column's sequence advance identically.
        PhysicalInsertSource::Tuples {
            rows,
            defaults,
            notnull_verified,
        } => {
            tuples = rows;
            // Safe against the defaults filled below: a builder only vouches for
            // the columns it filled itself, never one it left to the executor.
            verified = notnull_verified;
            if !defaults.is_empty() {
                for tuple in &mut tuples {
                    for (index, default) in &defaults {
                        tuple[*index] = eval(default, &[], ctx)?;
                    }
                }
            }
        }
        PhysicalInsertSource::Query { input, projections } => {
            let Execution::Rows { mut node, .. } = execute(*input, ctx, txn)? else {
                return Err(ExecError::new(
                    "XX000",
                    "insert source did not produce a row set",
                ));
            };
            while let Some(row) = node.next()? {
                let mut tuple = Tuple::with_capacity(projections.len());
                for expr in &projections {
                    tuple.push(eval(expr, &row, ctx)?);
                }
                tuples.push(tuple);
            }
        }
    }
    Ok((tuples, verified))
}

/// Constraint-check and write every tuple to a single table (the non-partitioned
/// path). Each row is validated against the pre-existing rows plus the earlier
/// rows of this statement, so a duplicate within one INSERT is caught.
#[allow(clippy::too_many_arguments)]
fn insert_direct(
    table: &Arc<dyn TableAm>,
    tuples: Vec<Tuple>,
    returning: Option<Returning>,
    notnull_verified: &[u32],
    freeze: bool,
    system: &[SysCol],
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    // Existing rows are only consulted to enforce UNIQUE keys; a table with no
    // unique index reads nothing at all (NOT NULL and CHECK read only the new
    // row). The checks bind once here rather than per tuple.
    let schema = table.schema();
    let indexes = table.indexes();
    let mut visible = UniqueKeySet::for_insert(table, txn, &schema, &indexes)?;
    // From the schema read just now, minus what the source proved: a column it
    // never filled, or one made not-null since, stays in the list.
    let notnull = NotNullSet::for_schema_excluding(&schema, notnull_verified);
    let checks = CheckSet::for_schema(&schema, ctx)?;
    // Generated columns are filled in before anything looks at the row: NOT NULL,
    // CHECK, UNIQUE and RETURNING all see the computed values.
    let generated = GeneratedSet::for_schema(&schema, ctx)?;
    let mut tuples = tuples;
    // Skipped outright for the overwhelmingly common relation with no generated
    // column, rather than walking the whole batch to call a no-op per row.
    if !generated.is_empty() {
        for tuple in &mut tuples {
            generated.compute(tuple, ctx)?;
        }
    }
    for tuple in &tuples {
        validate_constraints(&schema, tuple, &mut visible, &notnull, &checks, ctx)?;
        // The rows of this statement are not written until every one of them has
        // been checked, so the set is what carries a duplicate within one INSERT.
        visible.record(tuple, None);
    }
    let inserted = tuples.len() as u64;
    // One relation, so every row reports the same OID.
    let oid = system
        .contains(&SysCol::TableOid)
        .then(|| resolve_tableoid(&RelationIdent::of(&schema), ctx))
        .transpose()?;
    let frozen = write_context(freeze, txn);
    let write_txn = frozen.as_ref().unwrap_or(txn);
    // `ctid` and the MVCC header exist only once the row has been placed, so a
    // RETURNING that reads one has to project *after* the write. Otherwise the
    // projection runs first, so a faulting RETURNING expression aborts the
    // statement with nothing written and the tuples move into `insert` uncloned.
    // Both orders are atomic — an error rolls the statement back either way —
    // but only the first order avoids the copy, so it stays the default.
    let after_write = projects_after_write(system);
    let projected = match (&returning, after_write) {
        (Some(returning), false) => {
            let sources = vec![SysSource::placed(oid, Tid::new(0, 0), None); tuples.len()];
            Some(project_returning(
                &tuples,
                &returning.projections,
                (!system.is_empty()).then_some((system, sources.as_slice())),
                ctx,
            )?)
        }
        _ => None,
    };
    // Kept only for the after-write projection, which needs the row as it was
    // before `blank_virtual` erased the virtual columns it may name.
    let projection_rows = (returning.is_some() && after_write).then(|| tuples.clone());
    // A virtual column stores nothing; its value existed only for the checks and
    // the projection.
    generated.blank_virtual(&mut tuples);
    let written = tuples.len() as u64;
    let tids = table.insert_many(tuples, write_txn)?;
    count_write(ctx, table, WriteKind::Insert, written);
    let output = match (projected, projection_rows) {
        (projected, None) => projected,
        (_, Some(rows)) => {
            let returning = returning.as_ref().expect("projection rows imply RETURNING");
            let hdr = inserted_header(write_txn);
            let sources: Vec<SysSource> = tids
                .iter()
                .map(|&tid| SysSource::placed(oid, tid, Some(hdr)))
                .collect();
            Some(project_returning(
                &rows,
                &returning.projections,
                Some((system, &sources)),
                ctx,
            )?)
        }
    };
    finish_insert(returning, output, inserted)
}

/// The header a row this transaction just inserted carries, as the engines stamp
/// it: `xmin` from the writing context (the frozen XID under `COPY … FREEZE`),
/// `cmin` from its command id, and no deleter yet.
fn inserted_header(txn: &TxnContext) -> TupleHeader {
    TupleHeader::inserted(txn.insert_xid(), txn.cid)
}

/// Route each tuple to the leaf partition of `parent` that admits its key, then
/// validate and write. Each row is processed in order — routed, then validated
/// against its destination leaf — so a routing failure (23514) and a constraint
/// failure (23502/23505) are reported in the same order PostgreSQL would (it
/// routes then checks constraints, row by row), and a NOT NULL / unique violation
/// names the destination partition. A leaf is an ordinary heap and may carry a
/// UNIQUE index, so uniqueness is enforced against the destination leaf's
/// pre-existing rows plus earlier same-statement rows routed to it, exactly as
/// [`insert_direct`] does for a plain table. All checks run before any write, so
/// the statement stays all-or-nothing.
#[allow(clippy::too_many_arguments)]
fn insert_routed(
    parent: &Arc<dyn TableAm>,
    tuples: Vec<Tuple>,
    returning: Option<Returning>,
    leaves: &[Arc<dyn TableAm>],
    freeze: bool,
    system: &[SysCol],
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    let parent_schema = parent.schema();
    // Per-leaf key set a UNIQUE check consults, built lazily the first time a
    // row routes to a leaf that has a unique index (leaves without one, the
    // common case, never get scanned). `None` = not yet seeded.
    let mut visible: Vec<Option<UniqueKeySet>> = (0..leaves.len()).map(|_| None).collect();
    // Each leaf's shape, fetched on first use like `visible` above — a wide
    // partitioned table has many more leaves than a statement touches, and
    // materializing all of them would make a single-row INSERT cost
    // O(partitions) reads of a lock-guarded schema and index list.
    let mut shapes: Vec<Option<(Arc<TableSchema>, Vec<IndexMetadata>)>> = vec![None; leaves.len()];
    // Each leaf's bound checks, lazily for the same reason and on the same
    // schedule as `shapes` — kept in its own vector because binding can fail and
    // `get_or_insert_with` has no room for a `Result`.
    let mut leaf_checks: Vec<Option<CheckSet>> = (0..leaves.len()).map(|_| None).collect();
    // Each leaf's not-null columns, on the same schedule: a row is checked
    // against the leaf's own shape, so the parent's list would not answer.
    let mut leaf_notnull: Vec<Option<NotNullSet>> = (0..leaves.len()).map(|_| None).collect();
    // The leaves' generated columns, lazily like their checks. A generated
    // column is never part of a partition key, so routing reads only stored
    // values and can run before this.
    let mut leaf_generated: Vec<Option<GeneratedSet>> = (0..leaves.len()).map(|_| None).collect();
    let mut routes: Vec<usize> = Vec::with_capacity(tuples.len());
    let mut tuples = tuples;
    for tuple in &mut tuples {
        let leaf = route_tuple(&parent_schema, leaves, tuple, ctx)?;
        let (leaf_schema, leaf_indexes) =
            &*shapes[leaf].get_or_insert_with(|| (leaves[leaf].schema(), leaves[leaf].indexes()));
        if leaf_generated[leaf].is_none() {
            leaf_generated[leaf] = Some(GeneratedSet::for_schema(leaf_schema, ctx)?);
        }
        leaf_generated[leaf]
            .as_ref()
            .expect("just seeded")
            .compute(tuple, ctx)?;
        let has_unique = leaf_indexes.iter().any(|index| index.unique);
        if has_unique && visible[leaf].is_none() {
            visible[leaf] = Some(UniqueKeySet::for_insert(
                &leaves[leaf],
                txn,
                leaf_schema,
                leaf_indexes,
            )?);
        }
        if leaf_checks[leaf].is_none() {
            leaf_checks[leaf] = Some(CheckSet::for_schema(leaf_schema, ctx)?);
        }
        let checks = leaf_checks[leaf].as_ref().expect("just seeded");
        let notnull = leaf_notnull[leaf].get_or_insert_with(|| NotNullSet::for_schema(leaf_schema));
        match visible[leaf].as_mut() {
            Some(seen) => {
                validate_constraints(leaf_schema, tuple, seen, notnull, checks, ctx)?;
                seen.record(tuple, None);
            }
            None => validate_constraints(
                leaf_schema,
                tuple,
                &mut UniqueKeySet::none(),
                notnull,
                checks,
                ctx,
            )?,
        }
        routes.push(leaf);
    }
    let inserted = tuples.len() as u64;
    // Each row reports the leaf it routed to, which is the whole point of
    // `tableoid` on a partitioned target: `routes` is index-aligned with
    // `tuples`, so the answer is already in hand here.
    let oids = match system.contains(&SysCol::TableOid) {
        true => routes
            .iter()
            .map(|&leaf| {
                resolve_tableoid(&RelationIdent::of(&leaves[leaf].schema()), ctx).map(Some)
            })
            .collect::<Result<Vec<_>, _>>()?,
        false => vec![None; routes.len()],
    };
    // As in `insert_direct`: `ctid` and the MVCC header only exist once the row
    // is placed, so a RETURNING naming one projects after the write.
    let after_write = projects_after_write(system);
    let projected = match (&returning, after_write) {
        (Some(returning), false) => {
            let sources: Vec<SysSource> = oids
                .iter()
                .map(|&oid| SysSource::placed(oid, Tid::new(0, 0), None))
                .collect();
            Some(project_returning(
                &tuples,
                &returning.projections,
                (!system.is_empty()).then_some((system, sources.as_slice())),
                ctx,
            )?)
        }
        _ => None,
    };
    // Which output row each (leaf, position-within-leaf) pair belongs to, so the
    // tids `insert_many` hands back per leaf can be scattered into statement
    // order — the order RETURNING must emit.
    let projection_rows = (returning.is_some() && after_write).then(|| tuples.clone());
    let mut batches: Vec<Vec<Tuple>> = vec![Vec::new(); leaves.len()];
    let mut origins: Vec<Vec<usize>> = vec![Vec::new(); leaves.len()];
    for (i, (tuple, leaf)) in tuples.into_iter().zip(routes).enumerate() {
        origins[leaf].push(i);
        batches[leaf].push(tuple);
    }
    // Each batch is blanked by its own leaf's set: the leaves of one parent
    // share its column list, but each answers for its own storage.
    for (leaf, batch) in batches.iter_mut().enumerate() {
        if let Some(generated) = &leaf_generated[leaf] {
            generated.blank_virtual(batch.iter_mut());
        }
    }
    let frozen = write_context(freeze, txn);
    let write_txn = frozen.as_ref().unwrap_or(txn);
    let mut tids = vec![Tid::new(0, 0); inserted as usize];
    for ((leaf, tuples), origin) in leaves.iter().zip(batches).zip(&origins) {
        let written = tuples.len() as u64;
        for (&at, tid) in origin.iter().zip(leaf.insert_many(tuples, write_txn)?) {
            tids[at] = tid;
        }
        count_write(ctx, leaf, WriteKind::Insert, written);
    }
    let output = match (projected, projection_rows) {
        (projected, None) => projected,
        (_, Some(rows)) => {
            let returning = returning.as_ref().expect("projection rows imply RETURNING");
            let hdr = inserted_header(write_txn);
            let sources: Vec<SysSource> = oids
                .iter()
                .zip(&tids)
                .map(|(&oid, &tid)| SysSource::placed(oid, tid, Some(hdr)))
                .collect();
            Some(project_returning(
                &rows,
                &returning.projections,
                Some((system, &sources)),
                ctx,
            )?)
        }
    };
    finish_insert(returning, output, inserted)
}

/// Shared tail of the INSERT paths: emit RETURNING rows or the inserted count.
fn finish_insert(
    returning: Option<Returning>,
    output: Option<Vec<Tuple>>,
    inserted: u64,
) -> Result<Execution, ExecError> {
    match (returning, output) {
        (Some(returning), Some(output)) => {
            Ok(returning_rows(output, returning.columns, DmlVerb::Insert))
        }
        _ => Ok(Execution::Inserted(inserted)),
    }
}

/// Pick the leaf partition of `parent` whose RANGE bound admits `tuple`'s
/// partition key, returning its index in `leaves`. A NULL key — which no range
/// partition accepts — or a key outside every leaf's bound is rejected with
/// `23514`, matching PostgreSQL's `no partition of relation … found for row`.
fn route_tuple(
    parent: &TableSchema,
    leaves: &[Arc<dyn TableAm>],
    tuple: &Tuple,
    ctx: &ExecContext,
) -> Result<usize, ExecError> {
    // The RANGE-admits rule lives in `leaf_admits` (shared with the leaf-bound
    // check), so a routed row lands in exactly the leaf a direct INSERT would.
    for (idx, leaf) in leaves.iter().enumerate() {
        if leaf_admits(&leaf.schema(), tuple) {
            return Ok(idx);
        }
    }
    // No leaf admits the key (or it is NULL, which no range partition accepts):
    // PostgreSQL's tuple-routing failure. The DETAIL clips each field to 64 bytes
    // exactly as `display_tuple` does, so a long key reads byte-identically to PG.
    let scheme = parent
        .partition_scheme
        .as_ref()
        .expect("routing target is a partitioned parent");
    let col = scheme.key_columns[0];
    Err(ExecError::new(
        "23514",
        format!("no partition of relation \"{}\" found for row", parent.name),
    )
    .with_detail(Some(format!(
        "Partition key of the failing row contains ({}) = ({}).",
        parent.columns[col].name,
        clip_failing_row_field(display_value(&tuple[col], ctx)),
    ))))
}

/// UPDATE dispatch: an ordinary table updates in place ([`update_direct`]); a
/// partitioned parent re-routes each NEW row through its leaves, moving a row to
/// a different leaf when the key change relocates it ([`update_routed`]).
#[allow(clippy::too_many_arguments)]
fn execute_update(
    table: &Arc<dyn TableAm>,
    predicate: &Option<BoundExpr>,
    assignments: &[(usize, BoundExpr)],
    returning: Option<Returning>,
    routing: Option<Vec<DmlTarget>>,
    inherited: Vec<DmlTarget>,
    probe: Option<&DmlIndexProbe>,
    system: &[SysCol],
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    match routing {
        Some(leaves) => update_routed(
            table,
            &leaves,
            predicate,
            assignments,
            returning,
            system,
            ctx,
            txn,
        ),
        None if !inherited.is_empty() => update_inherited(
            &inherited,
            predicate,
            assignments,
            returning,
            system,
            ctx,
            txn,
        ),
        None => update_direct(
            table,
            predicate,
            assignments,
            returning,
            probe,
            system,
            ctx,
            txn,
        ),
    }
}

/// UPDATE through an inheritance parent: every descendant is updated **in
/// place**. Nothing is routed and nothing moves — that is the whole difference
/// from [`update_routed`], and the reason the two are separate functions rather
/// than one with a flag.
///
/// Each target's rows are read through its
/// [`view`](MappedRelation::view) as rows of the *named* relation, so the bound
/// predicate, SET targets and RETURNING projections all keep the named
/// relation's index space and are never rewritten. The NEW named-relation row is
/// then [`scatter`](MappedRelation::scatter)ed back over the columns it came
/// from, leaving a wider child's own extra columns untouched.
///
/// Constraints are validated against the full child tuple, so a child's own NOT
/// NULL or UNIQUE applies and its error text names the child — which is what PG
/// reports.
fn update_inherited(
    targets: &[DmlTarget],
    predicate: &Option<BoundExpr>,
    assignments: &[(usize, BoundExpr)],
    returning: Option<Returning>,
    system: &[SysCol],
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    let needs_header = needs_header(system);
    // Read every target once up front, so the match set is fixed before any
    // write (Halloween-safe) and RETURNING can fault with nothing written.
    let scans: Vec<Vec<SystemRow>> = targets
        .iter()
        .map(|target| {
            dml_rows(
                &target.relation.table,
                target.probe.as_ref(),
                needs_header,
                ctx,
                txn,
            )?
            .collect::<Result<Vec<_>, _>>()
            .map_err(ExecError::from)
        })
        .collect::<Result<_, _>>()?;
    // Each target's shape, once per target rather than per matched row. Eager
    // like the `has_unique` vector it feeds: an inheritance UPDATE scans every
    // target anyway, so there is no leaf here that the statement does not touch.
    let shapes: Vec<(Arc<TableSchema>, Vec<IndexMetadata>)> = targets
        .iter()
        .map(|target| {
            (
                target.relation.table.schema(),
                target.relation.table.indexes(),
            )
        })
        .collect();
    // Only a statement that writes a unique key can create a conflict, and the
    // planner withholds a probe from exactly those targets, so a probed target
    // never needs the whole-relation snapshot below. The assignment columns are
    // the named relation's, so each target translates them into its own first.
    let has_unique: Vec<bool> = targets
        .iter()
        .zip(&shapes)
        .map(|(target, (_, indexes))| {
            // Inheritance updates strictly in place — nothing moves. An
            // untranslatable assignment falls back to the snapshot.
            map_assigned_columns(assignments, &target.relation.map)
                .is_none_or(|assigned| update_needs_unique_snapshot(indexes, &assigned, false))
        })
        .collect();
    // Per-target simulation of its live rows after this statement, for UNIQUE
    // checks only. Each target's uniqueness is its own — inheritance does not
    // make a parent's unique index span its children, in PG or here.
    let mut simulated: Vec<UniqueKeySet> = scans
        .iter()
        .zip(&has_unique)
        .zip(&shapes)
        .map(|((rows, unique), (schema, indexes))| {
            if !*unique {
                return UniqueKeySet::none();
            }
            let mut set = UniqueKeySet::simulation(schema, indexes);
            for (tid, _, tuple) in rows {
                set.record(tuple, Some(*tid));
            }
            set
        })
        .collect();

    // One bound check set per target relation, alongside `simulated`.
    let checks: Vec<CheckSet> = shapes
        .iter()
        .map(|(schema, _)| CheckSet::for_schema(schema, ctx))
        .collect::<Result<_, _>>()?;

    // One per target, like the checks: an inheritance child may hold columns the
    // named relation does not.
    let notnull: Vec<NotNullSet> = shapes
        .iter()
        .map(|(schema, _)| NotNullSet::for_schema(schema))
        .collect();

    // One bound generated-column set per target, on the same schedule. Each
    // child answers with its own: an inheritance child may redeclare the
    // column's expression, and its layout is its own in any case.
    let generated: Vec<GeneratedSet> = shapes
        .iter()
        .map(|(schema, _)| GeneratedSet::for_schema(schema, ctx))
        .collect::<Result<_, _>>()?;

    // Each target's own OID, resolved once per relation rather than per row.
    let target_oids = target_oids(&targets, ctx)?;
    let mut pending: Vec<Vec<(Tid, Tuple)>> = vec![Vec::new(); targets.len()];
    let mut new_rows: Vec<Tuple> = Vec::new();
    // Which target and which pending slot each RETURNING row came from, so the
    // NEW versions' tids can be scattered back into statement order after the
    // write — `UPDATE … RETURNING ctid` reports the new version, as PG does.
    let mut returned_at: Vec<(usize, usize)> = Vec::new();
    let mut returned_sources: Vec<SysSource> = Vec::new();
    for (i, rows) in scans.iter().enumerate() {
        let target = &targets[i];
        for (tid, hdr, old) in rows {
            let old_view = target.relation.view(old);
            // WHERE and SET read the system columns of the row as it is *now*,
            // and they answer for the child this row actually lives in. The
            // slots are kept out of `new_view`, which `rebuild` turns back into
            // a stored tuple.
            let source = SysSource::placed(target_oids[i], *tid, *hdr);
            let probe;
            let old_probe: &[Value] = match system.is_empty() {
                true => &old_view,
                false => {
                    probe = source.widen(&old_view, system);
                    &probe
                }
            };
            if !predicate_holds(predicate, old_probe, ctx)? {
                continue;
            }
            // Every SET expression sees the OLD row: `SET a = b, b = a` swaps.
            let mut new_view = old_view.to_vec();
            for (index, expr) in assignments {
                new_view[*index] = eval(expr, old_probe, ctx)?;
            }
            let mut new = target.relation.rebuild(old, new_view);
            // Recomputed from the NEW row, whichever columns the statement
            // assigned — as upstream does, and for the same reason a CHECK is
            // re-evaluated unconditionally.
            generated[i].compute(&mut new, ctx)?;
            // RETURNING is bound against the named relation, so it sees the NEW
            // row in that shape, not the child's wider one. Read back through
            // the view so it carries the generated values just computed; nothing
            // is cloned without a RETURNING clause to read it.
            let returned = returning
                .is_some()
                .then(|| target.relation.view(&new).into_owned());

            if has_unique[i] {
                // Mirror `update_direct`: a tid absent from the simulation is a
                // row that vanished under us — skip it rather than update it.
                if !simulated[i].forget(old, *tid) {
                    continue;
                }
                if let Err(error) = validate_constraints(
                    &shapes[i].0,
                    &new,
                    &mut simulated[i],
                    &notnull[i],
                    &checks[i],
                    ctx,
                ) {
                    simulated[i].record(old, Some(*tid));
                    return Err(error);
                }
                simulated[i].record(&new, Some(*tid));
            } else {
                validate_constraints(
                    &shapes[i].0,
                    &new,
                    &mut UniqueKeySet::none(),
                    &notnull[i],
                    &checks[i],
                    ctx,
                )?;
            }
            if let Some(view) = returned {
                new_rows.push(view);
                returned_at.push((i, pending[i].len()));
                returned_sources.push(source);
            }
            pending[i].push((*tid, new));
        }
    }

    // A RETURNING that names `ctid` or the MVCC header describes the NEW
    // version, which does not exist until the write — so that case projects
    // after it. Everything else projects first, so a faulting expression aborts
    // the statement with nothing written.
    let after_write = projects_after_write(system);
    let projected = match (&returning, after_write) {
        (Some(returning), false) => Some(project_returning(
            new_rows.iter(),
            &returning.projections,
            (!system.is_empty()).then_some((system, returned_sources.as_slice())),
            ctx,
        )?),
        _ => None,
    };
    let mut affected = 0u64;
    let mut new_tids: Vec<Vec<Option<Tid>>> = vec![Vec::new(); targets.len()];
    for (i, target) in targets.iter().enumerate() {
        generated[i].blank_virtual(pending[i].iter_mut().map(|(_, tuple)| tuple));
        let batch = std::mem::take(&mut pending[i]);
        let updated = match after_write {
            false => target.relation.table.update_many(batch, txn)?,
            true => {
                let tids = target.relation.table.update_many_tids(batch, txn)?;
                let applied = tids.iter().filter(|tid| tid.is_some()).count() as u64;
                new_tids[i] = tids;
                applied
            }
        };
        count_write(ctx, &target.relation.table, WriteKind::Update, updated);
        affected += updated;
    }
    let output = match (projected, after_write) {
        (projected, false) => projected,
        (_, true) => match &returning {
            None => None,
            Some(returning) => {
                let hdr = inserted_header(txn);
                // A row the write did not apply to gets no RETURNING row at
                // all, as upstream — see `update_direct`.
                let (rows, sources): (Vec<&Tuple>, Vec<SysSource>) = new_rows
                    .iter()
                    .zip(&returned_at)
                    .zip(&returned_sources)
                    .filter_map(|((row, &(target, at)), source)| {
                        new_tids[target][at]
                            .map(|tid| (row, SysSource::placed(source.oid, tid, Some(hdr))))
                    })
                    .unzip();
                Some(project_returning(
                    rows,
                    &returning.projections,
                    Some((system, &sources)),
                    ctx,
                )?)
            }
        },
    };
    match (returning, output) {
        (Some(returning), Some(output)) => {
            Ok(returning_rows(output, returning.columns, DmlVerb::Update))
        }
        _ => Ok(Execution::Updated(affected)),
    }
}

/// The scan sees `txn`'s snapshot, and the new versions it writes carry `txn`'s
/// command id, so the statement never re-visits rows it wrote itself (no
/// Halloween problem). A row that vanished under us (`NotFound`) is skipped, not
/// counted. Cross-transaction write-write conflicts resolve last-writer-wins.
///
/// TODO: re-check a concurrently updated row against the new version under READ
/// COMMITTED (EvalPlanQual) and raise the 40001 serialization failure under
/// REPEATABLE READ, instead of letting the last writer win.
#[allow(clippy::too_many_arguments)]
fn update_direct(
    table: &Arc<dyn TableAm>,
    predicate: &Option<BoundExpr>,
    assignments: &[(usize, BoundExpr)],
    returning: Option<Returning>,
    probe: Option<&DmlIndexProbe>,
    system: &[SysCol],
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    let needs_header = needs_header(system);
    let original: Vec<SystemRow> =
        dml_rows(table, probe, needs_header, ctx, txn)?.collect::<Result<_, _>>()?;
    // `simulated` mirrors the post-update table so a UNIQUE check sees other
    // rows' new values. It is only needed when the statement actually writes a
    // unique key — otherwise every row keeps the key it had and no conflict can
    // appear. That is also why a probe is safe here: the planner withholds one
    // whenever this is true, because `original` would then hold only the matching
    // rows and the check would miss conflicts with the rest of the table.
    let schema = table.schema();
    let indexes = table.indexes();
    let assigned: Vec<usize> = assignments.iter().map(|(column, _)| *column).collect();
    let has_unique = update_needs_unique_snapshot(&indexes, &assigned, false);
    let mut simulated = if has_unique {
        let mut set = UniqueKeySet::simulation(&schema, &indexes);
        for (tid, _, tuple) in &original {
            set.record(tuple, Some(*tid));
        }
        set
    } else {
        UniqueKeySet::none()
    };
    // Unlike the unique snapshot, this is not conditional on which columns the
    // statement assigns: PostgreSQL re-evaluates *every* check against the new
    // row, not only those reading an updated column. A generated column is
    // recomputed on the same terms.
    let checks = CheckSet::for_schema(&schema, ctx)?;
    let notnull = NotNullSet::for_schema(&schema);
    let generated = GeneratedSet::for_schema(&schema, ctx)?;
    // One relation, so every row reports the same OID.
    let oid = system
        .contains(&SysCol::TableOid)
        .then(|| resolve_tableoid(&RelationIdent::of(&schema), ctx))
        .transpose()?;
    let mut pending: Vec<(Tid, Tuple)> = Vec::new();
    // One `SysSource` per pending row, index-aligned with it.
    let mut sources: Vec<SysSource> = Vec::new();
    for (tid, hdr, old) in original {
        // WHERE and SET read the system columns of the row as it is *now*; the
        // slots stay out of `new`, which is the tuple that gets stored.
        let source = SysSource::placed(oid, tid, hdr);
        let probe_row;
        let old_probe: &[Value] = match system.is_empty() {
            true => &old,
            false => {
                probe_row = source.widen(&old, system);
                &probe_row
            }
        };
        if !predicate_holds(predicate, old_probe, ctx)? {
            continue;
        }
        // Every SET expression sees the OLD row: `SET a = b, b = a` swaps.
        let mut new = old.clone();
        for (index, expr) in assignments {
            new[*index] = eval(expr, old_probe, ctx)?;
        }
        generated.compute(&mut new, ctx)?;
        if has_unique {
            // The row's own OLD key must not conflict with its NEW one, so it
            // leaves the simulation for the check. A tid the simulation does not
            // hold is a row that vanished under us — skip it rather than update
            // it. On a violation the OLD key goes back, leaving the simulation
            // as it was.
            if !simulated.forget(&old, tid) {
                continue;
            }
            if let Err(error) =
                validate_constraints(&schema, &new, &mut simulated, &notnull, &checks, ctx)
            {
                simulated.record(&old, Some(tid));
                return Err(error);
            }
            simulated.record(&new, Some(tid));
        } else {
            validate_constraints(
                &schema,
                &new,
                &mut UniqueKeySet::none(),
                &notnull,
                &checks,
                ctx,
            )?;
        }
        pending.push((tid, new));
        sources.push(source);
    }
    // With RETURNING, project the NEW rows (in schema order) before
    // `update_many` consumes `pending`, so a faulting expression aborts the
    // statement before any row is written. The exception is a RETURNING naming
    // `ctid` or the MVCC header: those describe the NEW version, which does not
    // exist until the write, so that case projects after it.
    let after_write = projects_after_write(system);
    let projected = match (&returning, after_write) {
        (Some(returning), false) => Some(project_returning(
            pending.iter().map(|(_, new)| new),
            &returning.projections,
            (!system.is_empty()).then_some((system, sources.as_slice())),
            ctx,
        )?),
        _ => None,
    };
    let new_rows = (returning.is_some() && after_write).then(|| {
        pending
            .iter()
            .map(|(_, new)| new.clone())
            .collect::<Vec<_>>()
    });
    generated.blank_virtual(pending.iter_mut().map(|(_, tuple)| tuple));
    let (updated, new_tids) = match after_write {
        false => (table.update_many(pending, txn)?, Vec::new()),
        true => {
            let tids = table.update_many_tids(pending, txn)?;
            (tids.iter().filter(|tid| tid.is_some()).count() as u64, tids)
        }
    };
    count_write(ctx, table, WriteKind::Update, updated);
    let output = match new_rows {
        None => projected,
        Some(rows) => {
            let returning = returning.as_ref().expect("new_rows implies RETURNING");
            let hdr = inserted_header(txn);
            // A `None` tid is a row that was gone by the time the write reached
            // it. PostgreSQL emits no RETURNING row for one, so neither does
            // this — inventing an address for a version that was never written
            // is what a client's `WHERE ctid = …` follow-up would then chase.
            let (rows, sources): (Vec<&Tuple>, Vec<SysSource>) = rows
                .iter()
                .zip(&sources)
                .zip(&new_tids)
                .filter_map(|((row, source), tid)| {
                    tid.map(|tid| (row, SysSource::placed(source.oid, tid, Some(hdr))))
                })
                .unzip();
            Some(project_returning(
                rows,
                &returning.projections,
                Some((system, &sources)),
                ctx,
            )?)
        }
    };
    match (returning, output) {
        (Some(returning), Some(output)) => {
            Ok(returning_rows(output, returning.columns, DmlVerb::Update))
        }
        _ => Ok(Execution::Updated(updated)),
    }
}

/// UPDATE through a partitioned parent, with cross-partition row movement.
///
/// Every leaf is scanned (the parent owns no rows of its own). For each row the
/// predicate keeps, the NEW tuple is built (every SET sees the OLD row, as in
/// [`update_direct`]) and re-routed with [`route_tuple`]: a NEW key admitted by no
/// leaf raises `23514` "no partition of relation … found for row" (PostgreSQL's
/// tuple-routing failure). A row whose NEW key still belongs to its own leaf is
/// updated in place; a row that now belongs elsewhere *moves* — deleted from the
/// old leaf and inserted into the new one (PostgreSQL's DELETE+INSERT semantics).
///
/// Constraints are validated against the **destination** leaf, so a NOT NULL /
/// UNIQUE violation names the partition the row lands in. A leaf is an ordinary
/// heap and may carry a UNIQUE index (via `CREATE UNIQUE INDEX` on the leaf), so
/// uniqueness is checked against a per-leaf simulation of the post-statement rows
/// — mirroring [`update_direct`] within a leaf and treating a moved-in row as an
/// insert into its destination — but only for leaves that actually have a unique
/// index (the common case has none, so no simulation is built). All routing and
/// validation runs before any write, and the whole matched set is taken from the
/// snapshot first, so the statement stays all-or-nothing and Halloween-safe.
#[allow(clippy::too_many_arguments)]
fn update_routed(
    parent: &Arc<dyn TableAm>,
    leaves: &[DmlTarget],
    predicate: &Option<BoundExpr>,
    assignments: &[(usize, BoundExpr)],
    returning: Option<Returning>,
    system: &[SysCol],
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    let parent_schema = parent.schema();
    // Each leaf's shape, once per leaf rather than per routed row. Eager like
    // the `leaf_has_unique` vector it feeds: a routed UPDATE scans every leaf to
    // find its matches, so every leaf is touched regardless. Routing addresses a
    // leaf as a bare relation, so the handles are kept alongside.
    let leaf_tables: Vec<Arc<dyn TableAm>> = leaves
        .iter()
        .map(|leaf| Arc::clone(&leaf.relation.table))
        .collect();
    let leaf_shapes: Vec<(Arc<TableSchema>, Vec<IndexMetadata>)> = leaf_tables
        .iter()
        .map(|leaf| (leaf.schema(), leaf.indexes()))
        .collect();
    // Every unique index counts here, written key or not: a row moved in from
    // another leaf arrives as an insert and can collide on a key this statement
    // never touched. The planner withholds a probe from such a leaf, so the
    // simulation below always has the leaf's whole row set to check against.
    // A leaf's columns are the parent's one-for-one, so no translation is needed.
    let assigned: Vec<usize> = assignments.iter().map(|(column, _)| *column).collect();
    let leaf_has_unique: Vec<bool> = leaf_shapes
        .iter()
        .map(|(_, indexes)| update_needs_unique_snapshot(indexes, &assigned, true))
        .collect();
    // Read every leaf once (the parent owns no rows). The collected snapshot
    // drives the match loop and — for leaves with a unique index — seeds the
    // post-statement simulation, so a leaf is never read twice and the match set
    // is fixed before any write (Halloween-safe).
    let needs_header = needs_header(system);
    let scans: Vec<Vec<SystemRow>> = leaves
        .iter()
        .map(|leaf| {
            dml_rows(
                &leaf.relation.table,
                leaf.probe.as_ref(),
                needs_header,
                ctx,
                txn,
            )?
            .collect::<Result<Vec<_>, _>>()
            .map_err(ExecError::from)
        })
        .collect::<Result<_, _>>()?;
    // Per-leaf simulation of the leaf's live rows after this statement, used only
    // for UNIQUE checks; seeded from `scans` for leaves that carry a unique index,
    // left empty (and unused) for the common unique-free leaf.
    let mut simulated: Vec<UniqueKeySet> = scans
        .iter()
        .enumerate()
        .map(|(i, rows)| {
            if !leaf_has_unique[i] {
                return UniqueKeySet::none();
            }
            let mut set = UniqueKeySet::simulation(&leaf_shapes[i].0, &leaf_shapes[i].1);
            for (tid, _, tuple) in rows {
                set.record(tuple, Some(*tid));
            }
            set
        })
        .collect();

    // One bound check set per leaf, alongside `simulated`. Every leaf is already
    // materialized here (`leaf_shapes` above), so there is nothing to defer.
    let leaf_checks: Vec<CheckSet> = leaf_shapes
        .iter()
        .map(|(schema, _)| CheckSet::for_schema(schema, ctx))
        .collect::<Result<_, _>>()?;

    let leaf_notnull: Vec<NotNullSet> = leaf_shapes
        .iter()
        .map(|(schema, _)| NotNullSet::for_schema(schema))
        .collect();

    // One bound generated-column set per leaf, alongside `leaf_checks`. A row
    // that moves between leaves is generated for its *destination*, which is the
    // relation whose constraints it then has to satisfy.
    let leaf_generated: Vec<GeneratedSet> = leaf_shapes
        .iter()
        .map(|(schema, _)| GeneratedSet::for_schema(schema, ctx))
        .collect::<Result<_, _>>()?;

    // Writes are grouped per leaf: in-place updates, deletes of moved-out rows,
    // and inserts of moved-in rows. `new_rows` records every affected NEW row in
    // scan order for RETURNING.
    let mut pending_update: Vec<Vec<(Tid, Tuple)>> = vec![Vec::new(); leaves.len()];
    let mut pending_delete: Vec<Vec<Tid>> = vec![Vec::new(); leaves.len()];
    let mut pending_insert: Vec<Vec<Tuple>> = vec![Vec::new(); leaves.len()];
    let mut new_rows: Vec<Tuple> = Vec::new();
    // Each leaf's own OID, resolved once per leaf.
    let leaf_oids = target_oids(leaves, ctx)?;
    // Where each RETURNING row's NEW version will land, so the tids the writes
    // report can be scattered back into scan order: `Ok` is an in-place update
    // of leaf `l` at position `i` in its update batch, `Err` a move into leaf
    // `l` at position `i` in its insert batch.
    let mut returned_at: Vec<Result<(usize, usize), (usize, usize)>> = Vec::new();
    let mut returned_oids: Vec<Option<u32>> = Vec::new();

    for (src, rows) in scans.iter().enumerate() {
        for (tid, hdr, old) in rows {
            let tid = *tid;
            // WHERE and SET read the system columns of the row as it is
            // *currently* stored — the row has not moved yet, and PostgreSQL
            // matches on where it is, not where the update would put it.
            let source = SysSource::placed(leaf_oids[src], tid, *hdr);
            let probe_row;
            let old_probe: &[Value] = match system.is_empty() {
                true => old,
                false => {
                    probe_row = source.widen(old, system);
                    &probe_row
                }
            };
            if !predicate_holds(predicate, old_probe, ctx)? {
                continue;
            }
            // Every SET expression sees the OLD row: `SET a = b, b = a` swaps.
            let mut new = old.clone();
            for (index, expr) in assignments {
                new[*index] = eval(expr, old_probe, ctx)?;
            }
            let dst = route_tuple(&parent_schema, &leaf_tables, &new, ctx)?;
            // No generated column can be part of the partition key, so routing
            // above read only stored values and this cannot change `dst`.
            leaf_generated[dst].compute(&mut new, ctx)?;

            // Validate against the destination leaf. When it has a unique index,
            // check the NEW row against that leaf's simulated rows (excluding the
            // row's own OLD slot for an in-place update); otherwise only NOT NULL
            // and the partition bound apply (the latter passes by construction).
            if leaf_has_unique[dst] {
                if src == dst {
                    // Mirror `update_direct`: a tid absent from the simulation is a
                    // row that vanished under us — skip it rather than update it.
                    if !simulated[dst].forget(old, tid) {
                        continue;
                    }
                    if let Err(error) = validate_constraints(
                        &leaf_shapes[dst].0,
                        &new,
                        &mut simulated[dst],
                        &leaf_notnull[dst],
                        &leaf_checks[dst],
                        ctx,
                    ) {
                        simulated[dst].record(old, Some(tid));
                        return Err(error);
                    }
                    simulated[dst].record(&new, Some(tid));
                } else {
                    validate_constraints(
                        &leaf_shapes[dst].0,
                        &new,
                        &mut simulated[dst],
                        &leaf_notnull[dst],
                        &leaf_checks[dst],
                        ctx,
                    )?;
                    // Recorded without its tid: in the destination the row is an
                    // insert, and its tid belongs to the source leaf's space —
                    // retracting it here would have to name a row of *this* leaf.
                    simulated[dst].record(&new, None);
                }
            } else {
                validate_constraints(
                    &leaf_shapes[dst].0,
                    &new,
                    &mut UniqueKeySet::none(),
                    &leaf_notnull[dst],
                    &leaf_checks[dst],
                    ctx,
                )?;
            }
            // A moved-out row leaves its source leaf's simulation so a later row
            // routed back to that leaf does not see the stale OLD value.
            if src != dst && leaf_has_unique[src] {
                simulated[src].forget(old, tid);
            }

            // `new_rows` is only read (for RETURNING) below; skip cloning into it
            // when there's no RETURNING clause to project.
            if returning.is_some() {
                new_rows.push(new.clone());
                // RETURNING reports the NEW row, so it names the leaf the row
                // ends up in.
                returned_oids.push(leaf_oids[dst]);
                returned_at.push(match src == dst {
                    true => Ok((dst, pending_update[dst].len())),
                    false => Err((dst, pending_insert[dst].len())),
                });
            }
            if src == dst {
                pending_update[src].push((tid, new));
            } else {
                pending_delete[src].push(tid);
                pending_insert[dst].push(new);
            }
        }
    }

    // As in `update_direct`: project before any write unless the RETURNING names
    // `ctid` or the MVCC header, which describe the NEW version.
    let after_write = projects_after_write(system);
    let projected = match (&returning, after_write) {
        (Some(returning), false) => {
            let sources: Vec<SysSource> = returned_oids
                .iter()
                .map(|&oid| SysSource::placed(oid, Tid::new(0, 0), None))
                .collect();
            Some(project_returning(
                new_rows.iter(),
                &returning.projections,
                (!system.is_empty()).then_some((system, sources.as_slice())),
                ctx,
            )?)
        }
        _ => None,
    };
    // Apply per leaf. A moved row is counted once (via its source-leaf delete);
    // an in-place update once (via `update_many`); moved-in inserts are not
    // counted — so the total equals the number of matched rows, as PostgreSQL's
    // UPDATE tag reports.
    let mut affected = 0u64;
    let mut updated_tids: Vec<Vec<Option<Tid>>> = vec![Vec::new(); leaves.len()];
    let mut inserted_tids: Vec<Vec<Tid>> = vec![Vec::new(); leaves.len()];
    for i in 0..leaves.len() {
        leaf_generated[i].blank_virtual(pending_update[i].iter_mut().map(|(_, tuple)| tuple));
        leaf_generated[i].blank_virtual(pending_insert[i].iter_mut());
        let batch = std::mem::take(&mut pending_update[i]);
        let updated = match after_write {
            false => leaf_tables[i].update_many(batch, txn)?,
            true => {
                let tids = leaf_tables[i].update_many_tids(batch, txn)?;
                let applied = tids.iter().filter(|tid| tid.is_some()).count() as u64;
                updated_tids[i] = tids;
                applied
            }
        };
        let deleted = leaf_tables[i].delete_many(std::mem::take(&mut pending_delete[i]), txn)?;
        let moved_in = std::mem::take(&mut pending_insert[i]);
        let inserted = moved_in.len() as u64;
        inserted_tids[i] = leaf_tables[i].insert_many(moved_in, txn)?;
        // The per-relation counters report a moved row as both a delete and an
        // insert, as PostgreSQL does, even though the UPDATE tag counts it once.
        count_write(ctx, &leaf_tables[i], WriteKind::Update, updated);
        count_write(ctx, &leaf_tables[i], WriteKind::Delete, deleted);
        count_write(ctx, &leaf_tables[i], WriteKind::Insert, inserted);
        affected += updated;
        affected += deleted;
    }
    let output = match (projected, after_write) {
        (projected, false) => projected,
        (_, true) => match &returning {
            None => None,
            Some(returning) => {
                let hdr = inserted_header(txn);
                // An in-place update the write did not apply to gets no
                // RETURNING row, as upstream — see `update_direct`. A moved row
                // always has one: it was written as an insert.
                let (rows, sources): (Vec<&Tuple>, Vec<SysSource>) = new_rows
                    .iter()
                    .zip(&returned_at)
                    .zip(&returned_oids)
                    .filter_map(|((row, at), &oid)| {
                        let tid = match at {
                            Ok((leaf, i)) => updated_tids[*leaf][*i]?,
                            Err((leaf, i)) => inserted_tids[*leaf][*i],
                        };
                        Some((row, SysSource::placed(oid, tid, Some(hdr))))
                    })
                    .unzip();
                Some(project_returning(
                    rows,
                    &returning.projections,
                    Some((system, &sources)),
                    ctx,
                )?)
            }
        },
    };
    match (returning, output) {
        (Some(returning), Some(output)) => {
            Ok(returning_rows(output, returning.columns, DmlVerb::Update))
        }
        _ => Ok(Execution::Updated(affected)),
    }
}

/// Takes the relation's shape rather than the relation, so a caller validating
/// a batch fetches it once instead of per row — `schema()` costs a lock and a
/// clone on an engine whose DDL can republish it. The unique indexes arrive
/// inside `existing`, which was built from them.
fn validate_constraints(
    schema: &TableSchema,
    tuple: &Tuple,
    existing: &mut UniqueKeySet,
    notnull: &NotNullSet,
    checks: &CheckSet,
    ctx: &ExecContext,
) -> Result<(), ExecError> {
    notnull.validate(schema, tuple, ctx)?;

    // Order matches PostgreSQL's observable behavior, probed against 18.4 on
    // both INSERT and UPDATE: not-null (above), then CHECK, then the partition
    // constraint, then the unique key (below). CHECK really does precede the
    // partition bound — a row violating both reports the check constraint —
    // which is the one pair here that reads backwards from how the two are
    // usually described.
    checks.validate(schema, tuple, ctx)?;
    check_partition_bound(schema, tuple, ctx)?;

    let Some(conflict) = existing.conflict(tuple)? else {
        return Ok(());
    };
    // Named by the set that found it, not by re-indexing a slice passed
    // alongside: the two are the same list today, and nothing would say so.
    let names = conflict
        .columns
        .iter()
        .map(|column| schema.columns[*column].name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let values = conflict
        .columns
        .iter()
        .map(|column| display_value(&tuple[*column], ctx))
        .collect::<Vec<_>>()
        .join(", ");
    Err(ExecError::new(
        "23505",
        format!(
            "duplicate key value violates unique constraint \"{}\"",
            conflict.name
        ),
    )
    .with_detail(Some(format!("Key ({names})=({values}) already exists."))))
}

/// Enforce a leaf partition's RANGE bound against a fully-formed row. A key
/// value outside `[from, to)` — or a NULL key, which no range partition admits —
/// is rejected with 23514, matching PostgreSQL's observable behavior (error text
/// and SQLSTATE) for a direct INSERT/UPDATE into a partition. A non-partition
/// relation (`None`) passes. This only ever runs on a leaf handle: a write to the
/// partitioned parent routes each row to a leaf (see `route_tuple`) and validates
/// against that leaf, so the parent schema itself never reaches here.
fn check_partition_bound(
    schema: &TableSchema,
    tuple: &Tuple,
    ctx: &ExecContext,
) -> Result<(), ExecError> {
    // A non-partition relation has no bound to enforce; a leaf whose bound admits
    // the row passes. Only the out-of-range case falls through to the error.
    if schema.partition_of.is_none() || leaf_admits(schema, tuple) {
        return Ok(());
    }
    Err(ExecError::new(
        "23514",
        format!(
            "new row for relation \"{}\" violates partition constraint",
            schema.name
        ),
    )
    .with_detail(Some(format!(
        "Failing row contains ({}).",
        display_tuple(schema, tuple, ctx)
    ))))
}

/// The inclusive lower bound admits `key` when `key >= from` (or the bound is
/// `MINVALUE`). `MAXVALUE` as a lower bound admits nothing.
fn lower_admits(from: &PartitionBoundDatum, ty: PgType, key: &Value) -> bool {
    match from {
        PartitionBoundDatum::MinValue => true,
        PartitionBoundDatum::MaxValue => false,
        PartitionBoundDatum::Value(v) => compare_values(ty, key, v) != Ordering::Less,
    }
}

/// The exclusive upper bound admits `key` when `key < to` (or the bound is
/// `MAXVALUE`). `MINVALUE` as an upper bound admits nothing.
fn upper_admits(to: &PartitionBoundDatum, ty: PgType, key: &Value) -> bool {
    match to {
        PartitionBoundDatum::MaxValue => true,
        PartitionBoundDatum::MinValue => false,
        PartitionBoundDatum::Value(v) => compare_values(ty, key, v) == Ordering::Less,
    }
}

/// Whether the leaf partition described by `leaf`'s schema admits `tuple`'s RANGE
/// key: a non-NULL key inside `[from, to)`. Single-column key (DDL-enforced); a
/// NULL key is admitted by no range partition. This is the one place the RANGE
/// "does this leaf admit this row" rule is composed — shared by leaf-bound
/// enforcement ([`check_partition_bound`]) and parent tuple routing
/// ([`route_tuple`]) so the two never disagree about which leaf a row belongs to.
/// Panics if `leaf` is not a partition leaf; both callers guarantee it.
fn leaf_admits(leaf: &TableSchema, tuple: &Tuple) -> bool {
    let part = leaf
        .partition_of
        .as_ref()
        .expect("leaf_admits called on a partition leaf");
    let col = part.key_columns[0];
    let ty = leaf.columns[col].ty;
    let key = &tuple[col];
    !matches!(key, Value::Null)
        && lower_admits(&part.bound.from[0], ty, key)
        && upper_admits(&part.bound.to[0], ty, key)
}

fn display_value(value: &Value, ctx: &ExecContext) -> String {
    value
        .encode_text_with(&ctx.fmt)
        .unwrap_or_else(|| "null".to_string())
}

/// PostgreSQL renders each column of a "Failing row contains (...)" DETAIL with a
/// 64-byte field limit: a longer value is clipped on a character boundary and
/// `...` appended. Match that so the DETAIL stays byte-identical to PG's.
const FAILING_ROW_FIELD_MAXLEN: usize = 64;

fn clip_failing_row_field(mut s: String) -> String {
    if s.len() <= FAILING_ROW_FIELD_MAXLEN {
        return s;
    }
    let mut end = FAILING_ROW_FIELD_MAXLEN;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push_str("...");
    s
}

/// The `Failing row contains (…)` list a constraint violation reports.
///
/// A **virtual** generated column prints the literal `virtual` rather than a
/// value, as upstream does: nothing is stored for it, so there is no row value
/// to name — the constraint was checked against a value computed for the
/// occasion. Probed against PostgreSQL 18.4.
fn display_tuple(schema: &TableSchema, tuple: &Tuple, ctx: &ExecContext) -> String {
    tuple
        .iter()
        .enumerate()
        .map(|(index, value)| {
            match schema
                .columns
                .get(index)
                .is_some_and(|c| c.is_virtual_generated())
            {
                true => "virtual".to_string(),
                false => clip_failing_row_field(display_value(value, ctx)),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// See the concurrency note on [`update_direct`].
/// DELETE dispatch: an ordinary table deletes from its own storage
/// ([`delete_direct`]); a partitioned parent scans every leaf and deletes matching
/// rows from whichever leaf holds them ([`delete_routed`]).
#[allow(clippy::too_many_arguments)]
fn execute_delete(
    table: &Arc<dyn TableAm>,
    predicate: &Option<BoundExpr>,
    returning: Option<Returning>,
    routing: Option<Vec<DmlTarget>>,
    inherited: Vec<DmlTarget>,
    probe: Option<&DmlIndexProbe>,
    system: &[SysCol],
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    match routing {
        Some(leaves) => delete_routed(&leaves, predicate, returning, system, ctx, txn),
        None if !inherited.is_empty() => {
            delete_inherited(&inherited, predicate, returning, system, ctx, txn)
        }
        None => delete_direct(table, predicate, returning, probe, system, ctx, txn),
    }
}

/// The header a row this transaction just deleted carries: its own `xmin`/`cmin`
/// untouched and the deleter stamped on — which is what `DELETE … RETURNING
/// xmax` reports, as PostgreSQL does. Called after the predicate has matched but
/// before the write, because the value does not depend on the write succeeding.
fn deleted_header(hdr: Option<TupleHeader>, txn: &TxnContext) -> Option<TupleHeader> {
    hdr.map(|hdr| TupleHeader {
        xmax: txn.xid,
        cmax: txn.cid,
        ..hdr
    })
}

/// DELETE through an inheritance parent: each matching row is removed from
/// whichever descendant holds it. As in [`update_inherited`], rows are matched
/// through each target's [`view`](MappedRelation::view), so the predicate and
/// RETURNING stay in the named relation's index space; and as in
/// [`delete_direct`], RETURNING is projected before anything is removed.
fn delete_inherited(
    targets: &[DmlTarget],
    predicate: &Option<BoundExpr>,
    returning: Option<Returning>,
    system: &[SysCol],
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    let needs_header = needs_header(system);
    let mut pending: Vec<Vec<Tid>> = vec![Vec::new(); targets.len()];
    // RETURNING sees the deleted (OLD) rows as rows of the named relation, in
    // scan (target) order.
    let mut deleted: Vec<Tuple> = Vec::new();
    // Each target's own OID, resolved once per relation.
    let target_oids = target_oids(targets, ctx)?;
    let mut deleted_sources: Vec<SysSource> = Vec::new();
    for (i, target) in targets.iter().enumerate() {
        for row in dml_rows(
            &target.relation.table,
            target.probe.as_ref(),
            needs_header,
            ctx,
            txn,
        )? {
            let (tid, hdr, tuple) = row?;
            let view = target.relation.view(&tuple);
            // WHERE reads the system columns of the child this row lives in, so
            // `DELETE FROM parent WHERE tableoid = 'child'::regclass` removes
            // exactly that child's rows — and it reads them as the row is
            // *stored*, see `delete_direct`.
            let matched = SysSource::placed(target_oids[i], tid, hdr);
            let probe_row;
            let row_probe: &[Value] = match system.is_empty() {
                true => &view,
                false => {
                    probe_row = matched.widen(&view, system);
                    &probe_row
                }
            };
            if predicate_holds(predicate, row_probe, ctx)? {
                pending[i].push(tid);
                if returning.is_some() {
                    // The slots are re-appended by `project_returning`, so only
                    // the declared columns are kept here.
                    deleted.push(view.into_owned());
                    deleted_sources.push(SysSource::placed(
                        target_oids[i],
                        tid,
                        deleted_header(hdr, txn),
                    ));
                }
            }
        }
    }
    let output = match &returning {
        Some(returning) => Some(project_returning(
            deleted.iter(),
            &returning.projections,
            (!system.is_empty()).then_some((system, deleted_sources.as_slice())),
            ctx,
        )?),
        None => None,
    };
    let mut affected = 0u64;
    for (i, target) in targets.iter().enumerate() {
        let deleted = target
            .relation
            .table
            .delete_many(std::mem::take(&mut pending[i]), txn)?;
        count_write(ctx, &target.relation.table, WriteKind::Delete, deleted);
        affected += deleted;
    }
    match (returning, output) {
        (Some(returning), Some(output)) => {
            Ok(returning_rows(output, returning.columns, DmlVerb::Delete))
        }
        _ => Ok(Execution::Deleted(affected)),
    }
}

#[allow(clippy::too_many_arguments)]
fn delete_direct(
    table: &Arc<dyn TableAm>,
    predicate: &Option<BoundExpr>,
    returning: Option<Returning>,
    probe: Option<&DmlIndexProbe>,
    system: &[SysCol],
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    let needs_header = needs_header(system);
    let mut pending: Vec<Tid> = Vec::new();
    // RETURNING sees the deleted (OLD) rows; capture them alongside the tids.
    let mut deleted: Vec<Tuple> = Vec::new();
    let mut sources: Vec<SysSource> = Vec::new();
    // One relation, so every row reports the same OID.
    let oid = system
        .contains(&SysCol::TableOid)
        .then(|| resolve_tableoid(&RelationIdent::of(&table.schema()), ctx))
        .transpose()?;
    for row in dml_rows(table, probe, needs_header, ctx, txn)? {
        let (tid, hdr, tuple) = row?;
        // The WHERE clause reads the row as it is *stored*: `xmax` is still
        // invalid on a live row, so `WHERE xmax = '0'::xid` matches every row
        // and its negation matches none, as upstream. Stamping the deleter in
        // first would make the predicate answer about a row state this very
        // statement is about to create.
        let matched = SysSource::placed(oid, tid, hdr);
        let probe_row;
        let row_probe: &[Value] = match system.is_empty() {
            true => &tuple,
            false => {
                probe_row = matched.widen(&tuple, system);
                &probe_row
            }
        };
        if predicate_holds(predicate, row_probe, ctx)? {
            pending.push(tid);
            if returning.is_some() {
                deleted.push(tuple);
                // RETURNING *does* describe the row after the delete: `xmax`
                // and `cmax` name this deleter, as upstream reports them.
                sources.push(SysSource::placed(oid, tid, deleted_header(hdr, txn)));
            }
        }
    }
    // Project the OLD rows before `delete_many` so a faulting RETURNING
    // expression aborts the statement before any row is removed.
    match returning {
        Some(returning) => {
            let output = project_returning(
                deleted.iter(),
                &returning.projections,
                (!system.is_empty()).then_some((system, sources.as_slice())),
                ctx,
            )?;
            let deleted = table.delete_many(pending, txn)?;
            count_write(ctx, table, WriteKind::Delete, deleted);
            Ok(returning_rows(output, returning.columns, DmlVerb::Delete))
        }
        None => {
            let deleted = table.delete_many(pending, txn)?;
            count_write(ctx, table, WriteKind::Delete, deleted);
            Ok(Execution::Deleted(deleted))
        }
    }
}

/// DELETE through a partitioned parent: scan every leaf (the parent owns no rows),
/// delete each matching row from the leaf that holds it, and — as in
/// [`delete_direct`] — project any RETURNING over the OLD rows before removing
/// them. The command count is the sum of the per-leaf deletes.
fn delete_routed(
    leaves: &[DmlTarget],
    predicate: &Option<BoundExpr>,
    returning: Option<Returning>,
    system: &[SysCol],
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    let needs_header = needs_header(system);
    let mut pending: Vec<Vec<Tid>> = vec![Vec::new(); leaves.len()];
    // RETURNING sees the deleted (OLD) rows, in scan (leaf) order.
    let mut deleted: Vec<Tuple> = Vec::new();
    // Each leaf's own OID, resolved once per leaf: `DELETE FROM p WHERE
    // tableoid = 'p1'::regclass` must remove exactly that partition's rows.
    let leaf_oids = target_oids(leaves, ctx)?;
    let mut sources: Vec<SysSource> = Vec::new();
    for (i, leaf) in leaves.iter().enumerate() {
        for row in dml_rows(
            &leaf.relation.table,
            leaf.probe.as_ref(),
            needs_header,
            ctx,
            txn,
        )? {
            let (tid, hdr, tuple) = row?;
            // Stored state for the predicate, post-delete state for RETURNING
            // — see `delete_direct`.
            let matched = SysSource::placed(leaf_oids[i], tid, hdr);
            let probe_row;
            let row_probe: &[Value] = match system.is_empty() {
                true => &tuple,
                false => {
                    probe_row = matched.widen(&tuple, system);
                    &probe_row
                }
            };
            if predicate_holds(predicate, row_probe, ctx)? {
                pending[i].push(tid);
                if returning.is_some() {
                    deleted.push(tuple);
                    sources.push(SysSource::placed(
                        leaf_oids[i],
                        tid,
                        deleted_header(hdr, txn),
                    ));
                }
            }
        }
    }
    match returning {
        Some(returning) => {
            let output = project_returning(
                deleted.iter(),
                &returning.projections,
                (!system.is_empty()).then_some((system, sources.as_slice())),
                ctx,
            )?;
            for (i, leaf) in leaves.iter().enumerate() {
                let deleted = leaf
                    .relation
                    .table
                    .delete_many(std::mem::take(&mut pending[i]), txn)?;
                count_write(ctx, &leaf.relation.table, WriteKind::Delete, deleted);
            }
            Ok(returning_rows(output, returning.columns, DmlVerb::Delete))
        }
        None => {
            let mut affected = 0u64;
            for (i, leaf) in leaves.iter().enumerate() {
                let deleted = leaf
                    .relation
                    .table
                    .delete_many(std::mem::take(&mut pending[i]), txn)?;
                count_write(ctx, &leaf.relation.table, WriteKind::Delete, deleted);
                affected += deleted;
            }
            Ok(Execution::Deleted(affected))
        }
    }
}

/// WHERE keeps a row only when the predicate is exactly true: false and NULL
/// both drop it.
pub(crate) fn predicate_holds(
    predicate: &Option<BoundExpr>,
    row: &[Value],
    ctx: &ExecContext,
) -> Result<bool, ExecError> {
    match predicate {
        None => Ok(true),
        Some(p) => Ok(matches!(eval(p, row, ctx)?, Value::Bool(true))),
    }
}

/// Build the row source for one join input: a table scan, a set-returning
/// function, or a recursively-executed subplan (derived table / CTE / VALUES).
fn build_join_source(
    input: PhysicalJoinInput,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Box<dyn ExecNode>, ExecError> {
    Ok(match input {
        PhysicalJoinInput::Scan {
            table,
            projection,
            system,
        } => match &system {
            None => Box::new(SeqScan::new(&table, txn, &projection, ctx)),
            Some(emit) => scan_with_slots(&table, emit, &projection, ctx, txn)?,
        },
        PhysicalJoinInput::TableFunction {
            func,
            args,
            ordinality,
        } => Box::new(TableFunctionSource::new(
            func,
            args,
            ordinality,
            ctx.clone(),
        )),
        PhysicalJoinInput::Subplan(source) => {
            let Execution::Rows { node, .. } = execute(*source, ctx, txn)? else {
                return Err(ExecError::new(
                    "XX000",
                    "join source did not produce a row set",
                ));
            };
            node
        }
    })
}

/// Recursively build a physical join tree. Leaf construction is shared with
/// standalone subquery/table-function sources; each binary node streams its
/// left side and materializes its right side.
fn build_join_expr(
    source: PhysicalJoinExpr,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Box<dyn ExecNode>, ExecError> {
    let uses_hash_join = source.uses_hash_join();
    match source {
        PhysicalJoinExpr::Input {
            input, predicate, ..
        } => {
            let source = build_join_source(input, ctx, txn)?;
            Ok(match predicate {
                Some(predicate) => Box::new(Filter::new(source, predicate, ctx.clone())),
                None => source,
            })
        }
        PhysicalJoinExpr::Join {
            left,
            right,
            kind,
            predicate,
            hash_keys,
        } => {
            let left_width = left.width();
            let right_width = right.width();
            let left = build_join_expr(*left, ctx, txn)?;
            let right = build_join_expr(*right, ctx, txn)?;
            if uses_hash_join {
                Ok(Box::new(HashJoin::new(
                    left,
                    right,
                    left_width,
                    right_width,
                    kind,
                    hash_keys,
                    predicate,
                    ctx.clone(),
                )?))
            } else {
                Ok(Box::new(NestedLoopJoin::new(
                    left,
                    right,
                    left_width,
                    right_width,
                    kind,
                    predicate,
                    ctx.clone(),
                )?))
            }
        }
        PhysicalJoinExpr::Lateral {
            left,
            right,
            // Deliberately dropped: `right_shape` is the inspection copy, and
            // the rows come from `right` substituted per left row.
            right_shape: _,
            right_width,
            kind,
            predicate,
        } => {
            let left_width = left.width();
            Ok(Box::new(LateralJoin::new(
                build_join_expr(*left, ctx, txn)?,
                right,
                left_width,
                right_width,
                kind,
                predicate,
                ctx.clone(),
            )?))
        }
    }
}

/// Wrap `node` in a `Sort` when there are ORDER BY keys. `columns` is the
/// client-visible output width; sort keys may address hidden ("resjunk")
/// columns past it, which the sort trims before emitting.
fn maybe_sort(
    node: Box<dyn ExecNode>,
    sort: Vec<SortKey>,
    columns: &[OutputColumn],
) -> Result<Box<dyn ExecNode>, ExecError> {
    if sort.is_empty() {
        return Ok(node);
    }
    Ok(Box::new(Sort::new(node, sort, columns.len())?))
}

/// The OID a `tableoid` slot reports for `ident`.
///
/// A relation the catalog cannot name is an internal inconsistency, not a user
/// error: the binder resolved this very relation moments ago, so the two views
/// disagreeing means the plan and the catalog have drifted. Saying so beats
/// reporting OID 0 as though it were data.
pub(crate) fn resolve_tableoid(ident: &RelationIdent, ctx: &ExecContext) -> Result<u32, ExecError> {
    let ops = ctx.catalog.as_deref().ok_or_else(|| {
        ExecError::new(
            crabgresql_pg_wire::sqlstate::INTERNAL_ERROR,
            "tableoid evaluated without a catalog context",
        )
    })?;
    ops.rel_oid(Some(&ident.namespace), &ident.name)
        .ok_or_else(|| {
            ExecError::new(
                crabgresql_pg_wire::sqlstate::INTERNAL_ERROR,
                format!(
                    "no OID for relation \"{}.{}\" the plan scans",
                    ident.namespace, ident.name
                ),
            )
        })
}

/// One scanned row with whatever system data its arm has to emit: the tid every
/// scan yields, and the MVCC header only [`TableAm::scan_with_system`] does.
pub(crate) type SystemRow = (Tid, Option<TupleHeader>, Tuple);

/// Append the slots `cols` names to `row`, in `cols` order — the one place a
/// [`SysCol`] becomes a [`Value`], shared by the read path and every DML path.
///
/// `oid` must be `Some` when `cols` contains [`SysCol::TableOid`] and `hdr` must
/// be `Some` when any of them [needs one](SysCol::needs_header); both are
/// settled per scan, not per row, so a mismatch is a wiring bug rather than
/// anything a query can provoke.
///
/// PostgreSQL's `xid`/`cid` are 32-bit while ours are wider, so both narrow the
/// way `age(xid)` narrows — a truncation, not a range check, which is what makes
/// the two agree.
pub(crate) fn push_system(
    row: &mut Tuple,
    cols: &[SysCol],
    oid: Option<u32>,
    tid: Tid,
    hdr: Option<&TupleHeader>,
) {
    for col in cols {
        row.push(match col {
            SysCol::TableOid => Value::Oid(oid.expect("a tableoid slot without a resolved OID")),
            SysCol::Ctid => Value::Tid {
                block: tid.block,
                offset: tid.offset,
            },
            _ => {
                let hdr = hdr.expect("a header slot without a scanned header");
                match col {
                    SysCol::Xmin => Value::Xid(hdr.xmin.0 as u32),
                    SysCol::Xmax => Value::Xid(hdr.xmax.0 as u32),
                    // One number for both, which is what upstream shows:
                    // PostgreSQL keeps the two in a single field and the system
                    // columns read it raw, so `cmin` and `cmax` always agree on
                    // a row. The storage here does keep them apart — visibility
                    // needs to judge an own insert and an own delete
                    // independently, which upstream buys with combo cids — so
                    // the field to report is the one that was last written:
                    // the deleter's once there is a deleter.
                    SysCol::Cmin | SysCol::Cmax => Value::Cid(match hdr.xmax.is_valid() {
                        true => hdr.cmax.0,
                        false => hdr.cmin.0,
                    }),
                    SysCol::TableOid | SysCol::Ctid => unreachable!("handled above"),
                }
            }
        });
    }
}

/// A relation's rows with the tid every scan yields and, when `needs_header`,
/// the MVCC header only [`TableAm::scan_with_system`] does.
///
/// The one place that decides between the two scans. The binder has already
/// refused a header column on an access method that declines it, so the `expect`
/// below is a wiring invariant rather than anything a query can reach.
pub(crate) fn system_scan(
    table: &Arc<dyn TableAm>,
    projection: &ColumnProjection,
    needs_header: bool,
    txn: &TxnContext,
) -> Box<dyn Iterator<Item = Result<SystemRow, StorageError>> + Send> {
    match needs_header {
        false => Box::new(
            table
                .scan(txn, projection)
                .map(|row| row.map(|(tid, tuple)| (tid, None, tuple))),
        ),
        true => Box::new(
            table
                .scan_with_system(txn, projection)
                .expect(
                    "the binder rejects a header system column the access method declines, \
                     so a statement reaching here can produce one",
                )
                .map(|row| row.map(|(tid, hdr, tuple)| (tid, Some(hdr), tuple))),
        ),
    }
}

/// A scan of one relation that appends `emit`'s system slots to every row — the
/// single-relation twin of what an [`Append`] arm does.
///
/// This is why a relation that emits system columns is *not* wrapped in a
/// one-armed `Append`: an `Append` arm carries no index probe, so doing that
/// would cost `SELECT ctid … WHERE pk = …` its index path.
fn scan_with_slots(
    table: &Arc<dyn TableAm>,
    emit: &SystemEmit,
    projection: &ColumnProjection,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Box<dyn ExecNode>, ExecError> {
    let cols = Arc::clone(&emit.cols);
    let oid = match cols.contains(&SysCol::TableOid) {
        true => Some(resolve_tableoid(&emit.ident, ctx)?),
        false => None,
    };
    let rows = system_scan(
        table,
        projection,
        cols.iter().any(|c| c.needs_header()),
        txn,
    );
    Ok(Box::new(MappedScan {
        iter: Box::new(rows.map(move |row| {
            row.map(|(tid, hdr, mut tuple)| {
                push_system(&mut tuple, &cols, oid, tid, hdr.as_ref());
                tuple
            })
        })),
    }))
}

/// A row source that is already a plain tuple iterator.
struct MappedScan {
    iter: Box<dyn Iterator<Item = Result<Tuple, StorageError>> + Send>,
}

impl ExecNode for MappedScan {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        self.iter.next().transpose().map_err(ExecError::from)
    }
}

/// The rows one `UPDATE`/`DELETE` target contributes: an index probe when the
/// planner chose one for it, else the whole relation.
///
/// Always full width — `UPDATE` rebuilds each row by ordinal and `RETURNING` may
/// name any column — which also means the probe's fallback path can always read
/// its key columns back.
///
/// A probe's rows are deduplicated by `Tid`. A tid names exactly one row version,
/// so a repeat is always a defect in the source, and DML is where it does visible
/// damage: `update_direct` would write the row twice and report the inflated
/// count. No source is known today: the one that was — `TRUNCATE` swapping the
/// heap's relfilenode without resetting the index, so a stale `key -> tid` entry
/// came back alongside the row that reused the slot — was repaired in the engine
/// by swapping the indexes in lockstep. The filter stays as a barrier: it is
/// cheap, and it is DML that would turn such a defect into wrong data rather
/// than a wrong read.
/// `needs_header` asks for each row's MVCC header. Both sources can produce one
/// — `index_probe_system_rows` for a probe, `scan_with_system` otherwise — so
/// reading `xmin` no longer costs the statement its index path.
fn dml_rows(
    table: &Arc<dyn TableAm>,
    probe: Option<&DmlIndexProbe>,
    needs_header: bool,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Box<dyn Iterator<Item = Result<SystemRow, StorageError>> + Send>, ExecError> {
    match probe {
        Some(probe) => {
            let rows = match needs_header {
                false => index_probe_rows(
                    table,
                    &probe.index_name,
                    &probe.key,
                    ctx,
                    txn,
                    &ColumnProjection::All,
                )
                .map(|rows| -> Box<dyn Iterator<Item = _> + Send> {
                    Box::new(rows.map(|row| row.map(|(tid, tuple)| (tid, None, tuple))))
                })?,
                true => index_probe_system_rows(
                    table,
                    &probe.index_name,
                    &probe.key,
                    ctx,
                    txn,
                    &ColumnProjection::All,
                )?,
            };
            let mut seen = HashSet::new();
            Ok(Box::new(rows.filter(move |row| match row {
                Ok((tid, _, _)) => seen.insert(*tid),
                // Errors pass through; only rows are deduplicated.
                Err(_) => true,
            })))
        }
        None => Ok(system_scan(
            table,
            &ColumnProjection::All,
            needs_header,
            txn,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_binder::{BinOp, UnaryOp};
    use crabgresql_planner::IndexProbeSpec;
    use crabgresql_storage_api::{
        Column, IndexConstraint, IndexKey, IndexMethod, IndexProbeKey, TableEngine, TableSchema,
    };
    use crabgresql_txn::TxnContext;
    use crabgresql_types::PgType;
    use eval::coerce_value;
    use testutil::{
        binary, boolean, collect, eval_const, indexed_table, int4, rtxn, test_ok, test_table, wtxn,
    };

    /// The batch columns a scan fills, which is what `Shred` decodes.
    ///
    /// Worth its own test because the effect of getting it wrong is invisible
    /// in results: an unprojected column is all-NULL padding, so decoding it
    /// yields the same `Value::Null` that skipping it leaves behind. Only the
    /// per-row cost changes — on a wide relation, by the ratio of the table's
    /// width to the query's — so no assertion on rows can catch a regression
    /// here and this has to pin the derivation directly.
    #[test]
    fn scan_positions_names_only_the_projected_columns() {
        assert_eq!(scan_positions(&ColumnProjection::All, 3), vec![0, 1, 2]);

        let schema = TableSchema::new(
            "t",
            vec![
                Column::new("a", PgType::Int4),
                Column::new("b", PgType::Int4),
                Column::new("c", PgType::Int4),
            ],
        );
        let narrowed = ColumnProjection::of([2], &schema);
        assert_eq!(scan_positions(&narrowed, 3), vec![2]);
    }

    /// (op, left, right, expected), with `None` as SQL NULL.
    type TruthTableRow = (BinOp, Option<bool>, Option<bool>, Option<bool>);

    #[test]
    fn and_or_follow_kleene_tables() -> anyhow::Result<()> {
        let cases: &[TruthTableRow] = &[
            (BinOp::And, Some(true), Some(true), Some(true)),
            (BinOp::And, Some(true), Some(false), Some(false)),
            (BinOp::And, Some(false), None, Some(false)),
            (BinOp::And, None, Some(false), Some(false)),
            (BinOp::And, None, Some(true), None),
            (BinOp::And, None, None, None),
            (BinOp::Or, Some(false), Some(false), Some(false)),
            (BinOp::Or, Some(false), Some(true), Some(true)),
            (BinOp::Or, Some(true), None, Some(true)),
            (BinOp::Or, None, Some(true), Some(true)),
            (BinOp::Or, None, Some(false), None),
            (BinOp::Or, None, None, None),
        ];
        for (op, l, r, expected) in cases {
            let expr = binary(*op, PgType::Bool, boolean(*l), boolean(*r));
            let expected = expected.map(Value::Bool).unwrap_or(Value::Null);
            assert_eq!(eval_const(&expr)?, expected, "{l:?} {op:?} {r:?}");
        }

        Ok(())
    }

    #[test]
    fn null_operand_nulls_comparison() -> anyhow::Result<()> {
        let expr = binary(
            BinOp::Eq,
            PgType::Int4,
            int4(1),
            BoundExpr::Const {
                value: Value::Null,
                ty: PgType::Int4,
            },
        );
        assert_eq!(eval_const(&expr)?, Value::Null);

        Ok(())
    }

    #[test]
    fn not_follows_three_valued_logic() -> anyhow::Result<()> {
        let not = |v| BoundExpr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(boolean(v)),
        };
        assert_eq!(eval_const(&not(Some(true)))?, Value::Bool(false));
        assert_eq!(eval_const(&not(None))?, Value::Null);

        Ok(())
    }

    #[test]
    fn is_null_is_never_null() -> anyhow::Result<()> {
        let is_null = |v: Value, negated| BoundExpr::IsNull {
            expr: Box::new(BoundExpr::Const {
                value: v,
                ty: PgType::Int4,
            }),
            negated,
        };
        assert_eq!(eval_const(&is_null(Value::Null, false))?, Value::Bool(true));
        assert_eq!(
            eval_const(&is_null(Value::Int4(1), false))?,
            Value::Bool(false)
        );
        assert_eq!(eval_const(&is_null(Value::Null, true))?, Value::Bool(false));

        Ok(())
    }

    #[test]
    fn bool_test_is_never_null() -> anyhow::Result<()> {
        let test = |operand, value, negated| BoundExpr::BoolTest {
            expr: Box::new(boolean(operand)),
            value,
            negated,
        };
        // The full truth table, matching PG: each operand is exactly one of the
        // three values, so exactly one un-negated test holds for it.
        //             operand      IS TRUE  IS FALSE  IS UNKNOWN
        let expected = [
            (Some(true), [true, false, false]),
            (Some(false), [false, true, false]),
            (None, [false, false, true]),
        ];
        for (operand, [is_t, is_f, is_unk]) in expected {
            for (value, want) in [(Some(true), is_t), (Some(false), is_f), (None, is_unk)] {
                assert_eq!(
                    eval_const(&test(operand, value, false))?,
                    Value::Bool(want),
                    "{operand:?} IS {value:?}"
                );
                // The negated spelling is the exact complement — never NULL.
                assert_eq!(
                    eval_const(&test(operand, value, true))?,
                    Value::Bool(!want),
                    "{operand:?} IS NOT {value:?}"
                );
            }
        }

        Ok(())
    }

    #[test]
    fn case_selects_first_true_branch_lazily() -> anyhow::Result<()> {
        // CASE WHEN <cond1> THEN 10 WHEN <cond2> THEN 20 ELSE 30 END, over
        // constant conditions; false/NULL skip, only the winner is returned.
        let case = |c1: Option<bool>, c2: Option<bool>, else_: Option<BoundExpr>| BoundExpr::Case {
            whens: vec![(boolean(c1), int4(10)), (boolean(c2), int4(20))],
            else_: else_.map(Box::new),
            ty: PgType::Int4,
        };
        let e30 = || Some(int4(30));
        assert_eq!(
            eval_const(&case(Some(true), Some(true), e30()))?,
            Value::Int4(10)
        );
        assert_eq!(
            eval_const(&case(Some(false), Some(true), e30()))?,
            Value::Int4(20)
        );
        // NULL condition behaves like false: falls through to ELSE.
        assert_eq!(
            eval_const(&case(None, Some(false), e30()))?,
            Value::Int4(30)
        );
        // No branch matches and no ELSE: NULL.
        assert_eq!(eval_const(&case(Some(false), None, None))?, Value::Null);

        Ok(())
    }

    #[test]
    fn case_does_not_evaluate_unselected_results() -> anyhow::Result<()> {
        // The losing branch divides by zero; a lazy CASE must never touch it.
        let bomb = binary(BinOp::Div, PgType::Int4, int4(1), int4(0));
        let expr = BoundExpr::Case {
            whens: vec![(boolean(Some(true)), int4(1)), (boolean(Some(true)), bomb)],
            else_: None,
            ty: PgType::Int4,
        };
        assert_eq!(eval_const(&expr)?, Value::Int4(1));

        Ok(())
    }

    #[test]
    fn arithmetic_overflow_is_22003() {
        let expr = binary(BinOp::Add, PgType::Int4, int4(i32::MAX), int4(1));
        let e = eval_const(&expr).expect_err("an int4 addition that overflows must be rejected");
        assert_eq!(e.code, "22003");
        assert_eq!(e.message, "integer out of range");

        let expr = binary(
            BinOp::Mul,
            PgType::Int8,
            BoundExpr::Const {
                value: Value::Int8(i64::MAX),
                ty: PgType::Int8,
            },
            BoundExpr::Const {
                value: Value::Int8(2),
                ty: PgType::Int8,
            },
        );
        assert_eq!(
            eval_const(&expr)
                .expect_err("an int8 multiplication that overflows must be rejected")
                .message,
            "bigint out of range"
        );
    }

    #[test]
    fn division_and_modulo_by_zero_are_22012() {
        for op in [BinOp::Div, BinOp::Mod] {
            let e = eval_const(&binary(op, PgType::Int4, int4(1), int4(0)))
                .expect_err("a division by zero must be rejected");
            assert_eq!(e.code, "22012");
            assert_eq!(e.message, "division by zero");
        }
    }

    #[test]
    fn min_over_minus_one_edge_cases() -> anyhow::Result<()> {
        // MIN / -1 overflows ...
        let e = eval_const(&binary(BinOp::Div, PgType::Int4, int4(i32::MIN), int4(-1)))
            .expect_err("int4 MIN divided by -1 must be rejected as out of range");
        assert_eq!(e.code, "22003");
        // ... but MIN % -1 is 0, as in PG.
        assert_eq!(
            eval_const(&binary(BinOp::Mod, PgType::Int4, int4(i32::MIN), int4(-1)))?,
            Value::Int4(0)
        );

        Ok(())
    }

    #[test]
    fn negating_min_is_22003() {
        let expr = BoundExpr::Unary {
            op: UnaryOp::Neg,
            expr: Box::new(int4(i32::MIN)),
        };
        assert_eq!(
            eval_const(&expr)
                .expect_err("negating int4 MIN must be rejected as out of range")
                .code,
            "22003"
        );
    }

    #[test]
    fn text_and_bool_comparisons() -> anyhow::Result<()> {
        let text_const = |s: &str| BoundExpr::Const {
            value: Value::Text(s.into()),
            ty: PgType::Text,
        };
        let expr = binary(BinOp::Lt, PgType::Text, text_const("a"), text_const("b"));
        assert_eq!(eval_const(&expr)?, Value::Bool(true));
        // false < true
        let expr = binary(
            BinOp::Lt,
            PgType::Bool,
            boolean(Some(false)),
            boolean(Some(true)),
        );
        assert_eq!(eval_const(&expr)?, Value::Bool(true));

        Ok(())
    }

    #[test]
    fn coerce_range_checks_int8_to_int4() -> anyhow::Result<()> {
        let ctx = &ExecContext::default();
        assert_eq!(
            coerce_value(Value::Int8(7), PgType::Int4, &ctx)?,
            Value::Int4(7)
        );
        let e = coerce_value(Value::Int8(i64::MAX), PgType::Int4, &ctx)
            .expect_err("an int8 beyond the int4 range must be rejected");
        assert_eq!(e.code, "22003");
        assert_eq!(coerce_value(Value::Null, PgType::Int4, &ctx)?, Value::Null);

        Ok(())
    }

    /// The explicit int4 → bool cast accepts any integer, but PL/pgSQL assigns
    /// through an I/O conversion, which only accepts what `boolin` does.
    #[test]
    fn assigning_int4_to_bool_goes_through_boolin() -> anyhow::Result<()> {
        let ctx = &ExecContext::default();
        for (n, expected) in [(0, false), (1, true)] {
            assert_eq!(
                coerce_value_assign(Value::Int4(n), PgType::Bool, ctx)?,
                Value::Bool(expected)
            );
        }
        for n in [2, -1, -42] {
            let e = coerce_value_assign(Value::Int4(n), PgType::Bool, ctx)
                .expect_err("an integer other than 0 or 1 must be rejected by boolin");
            assert_eq!(e.code, "22P02");
            assert_eq!(
                e.message,
                format!("invalid input syntax for type boolean: \"{n}\"")
            );
            // The explicit cast still accepts it, as `SELECT n::boolean` must.
            assert_eq!(
                coerce_value(Value::Int4(n), PgType::Bool, ctx)?,
                Value::Bool(true)
            );
        }
        assert_eq!(
            coerce_value_assign(Value::Null, PgType::Bool, ctx)?,
            Value::Null
        );

        Ok(())
    }

    /// A one-column unique index on `key_type`, with `nulls_distinct` as given,
    /// over a table holding `rows`. `numeric` is deliberately not indexable by
    /// `btkey`, so it is how a caller asks for a metadata-only index — one the
    /// engine declines to probe.
    fn unique_table(key_type: PgType, nulls_distinct: bool, rows: Vec<Value>) -> Arc<dyn TableAm> {
        let engine = crabgresql_pg_engine::ephemeral_engine();
        let table = test_ok(engine.create_table(TableSchema::in_namespace(
            "t",
            "public",
            vec![Column::new("k", key_type)],
        )));
        test_ok(engine.create_index(
            "public",
            "t",
            IndexMetadata {
                name: "t_k_key".into(),
                method: IndexMethod::BTree,
                keys: vec![IndexKey {
                    column: 0,
                    descending: false,
                    nulls_first: false,
                }],
                unique: true,
                nulls_distinct,
                constraint: Some(IndexConstraint::Unique),
            },
        ));
        let txn = wtxn();
        for row in rows {
            test_ok(table.insert(vec![row], &txn));
        }
        table
    }

    /// Whether `key` collides with what `table`'s unique index already holds.
    fn collides(table: &Arc<dyn TableAm>, key: Value) -> bool {
        let schema = table.schema();
        let indexes = table.indexes();
        let txn = rtxn();
        let mut set = test_ok(UniqueKeySet::for_insert(table, &txn, &schema, &indexes));
        test_ok(set.conflict(&vec![key])).is_some()
    }

    /// A table that advertises an index scan and then declines every probe,
    /// counting the scans it serves. That is the state a `DROP INDEX` landing
    /// mid-statement leaves behind: the set sampled `supports_index_scan` once,
    /// and `index_lookup` answers `None` from there on.
    struct DeclinedProbe {
        inner: Arc<dyn TableAm>,
        scans: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl TableAm for DeclinedProbe {
        fn schema(&self) -> Arc<TableSchema> {
            self.inner.schema()
        }
        fn indexes(&self) -> Vec<IndexMetadata> {
            self.inner.indexes()
        }
        fn supports_index_scan(&self, _index_name: &str) -> bool {
            true
        }
        fn index_lookup(
            &self,
            _index_name: &str,
            _key: &IndexProbeKey<'_>,
            _txn: &TxnContext,
        ) -> Option<crabgresql_storage_api::IndexProbe> {
            None
        }
        fn scan(
            &self,
            txn: &TxnContext,
            projection: &ColumnProjection,
        ) -> crabgresql_storage_api::TupleStream {
            self.scans
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.scan(txn, projection)
        }
        fn fetch(&self, tid: Tid, txn: &TxnContext) -> Result<Option<Tuple>, StorageError> {
            self.inner.fetch(tid, txn)
        }
        fn insert(&self, tuple: Tuple, txn: &TxnContext) -> Result<Tid, StorageError> {
            self.inner.insert(tuple, txn)
        }
        fn update(
            &self,
            tid: Tid,
            tuple: Tuple,
            txn: &TxnContext,
        ) -> Result<crabgresql_storage_api::UpdateResult, StorageError> {
            self.inner.update(tid, tuple, txn)
        }
        fn delete(
            &self,
            tid: Tid,
            txn: &TxnContext,
        ) -> Result<crabgresql_storage_api::DeleteResult, StorageError> {
            self.inner.delete(tid, txn)
        }
    }

    #[test]
    fn unique_key_set_reseeds_once_when_the_engine_declines_a_probe() {
        let scans = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let table: Arc<dyn TableAm> = Arc::new(DeclinedProbe {
            inner: unique_table(PgType::Int4, true, vec![Value::Int4(1), Value::Int4(2)]),
            scans: Arc::clone(&scans),
        });
        let schema = table.schema();
        let indexes = table.indexes();
        let txn = rtxn();
        let mut set = test_ok(UniqueKeySet::for_insert(&table, &txn, &schema, &indexes));
        // Nothing was read to build the set: every index looked probeable.
        assert_eq!(scans.load(std::sync::atomic::Ordering::Relaxed), 0);
        // The declined probe still catches the duplicate...
        assert!(test_ok(set.conflict(&vec![Value::Int4(2)])).is_some());
        assert!(test_ok(set.conflict(&vec![Value::Int4(9)])).is_none());
        // ...and every later row is answered from the buckets, so the relation
        // is read once for the statement rather than once per row.
        assert_eq!(scans.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn unique_key_set_probes_the_physical_index() {
        let table = unique_table(PgType::Int4, true, vec![Value::Int4(1), Value::Int4(2)]);
        assert!(collides(&table, Value::Int4(2)));
        assert!(!collides(&table, Value::Int4(9)));
    }

    #[test]
    fn unique_key_set_scans_an_index_the_engine_will_not_probe() {
        // `numeric` has no `btkey` encoding, so the index is metadata-only and
        // the set falls back to reading the relation. Equality is still
        // `compare_values`, under which `1.0` and `1.00` are the same key.
        let numeric = |s: &str| Value::Numeric(test_ok(crabgresql_types::Numeric::parse(s)));
        let table = unique_table(PgType::Numeric, true, vec![numeric("1.0")]);
        assert!(collides(&table, numeric("1.00")));
        assert!(!collides(&table, numeric("2")));
    }

    #[test]
    fn unique_key_set_never_probes_a_nulls_not_distinct_index() {
        // Two NULLs collide here, which an equality probe cannot answer: it has
        // no NULL to encode and would report the key as absent.
        let table = unique_table(PgType::Int4, false, vec![Value::Null]);
        assert!(collides(&table, Value::Null));
        // The default (`NULLS DISTINCT`) is the opposite: NULL collides with
        // nothing, itself included.
        let table = unique_table(PgType::Int4, true, vec![Value::Null]);
        assert!(!collides(&table, Value::Null));
    }

    #[test]
    fn unique_key_set_holds_the_statement_own_rows() {
        // A statement's rows are not written until every one is checked, so no
        // engine can answer for them — the set is what catches a duplicate
        // within one INSERT.
        let table = unique_table(PgType::Int4, true, vec![Value::Int4(1)]);
        let schema = table.schema();
        let indexes = table.indexes();
        let txn = rtxn();
        let mut set = test_ok(UniqueKeySet::for_insert(&table, &txn, &schema, &indexes));
        let row = vec![Value::Int4(7)];
        assert!(test_ok(set.conflict(&row)).is_none());
        set.record(&row, None);
        assert!(test_ok(set.conflict(&row)).is_some());
    }

    #[test]
    fn unique_key_set_forgets_a_superseded_row() {
        // What an UPDATE does to its own row: retract the OLD key so the NEW one
        // does not collide with it, and report whether the row was there at all.
        let schema = TableSchema::in_namespace("t", "public", vec![Column::new("k", PgType::Int4)]);
        let indexes = vec![IndexMetadata {
            name: "t_k_key".into(),
            method: IndexMethod::BTree,
            keys: vec![IndexKey {
                column: 0,
                descending: false,
                nulls_first: false,
            }],
            unique: true,
            nulls_distinct: true,
            constraint: Some(IndexConstraint::Unique),
        }];
        let mut set = UniqueKeySet::simulation(&schema, &indexes);
        let tid = Tid {
            block: 0,
            offset: 1,
        };
        let row = vec![Value::Int4(1)];
        set.record(&row, Some(tid));
        assert!(test_ok(set.conflict(&row)).is_some());
        assert!(set.forget(&row, tid));
        assert!(test_ok(set.conflict(&row)).is_none());
        // Gone once: a second retraction says the simulation no longer holds it.
        assert!(!set.forget(&row, tid));
    }

    /// `WHERE id = 2`, in the parent's column space.
    fn id_eq_2() -> Option<BoundExpr> {
        Some(binary(
            BinOp::Eq,
            PgType::Int4,
            BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4,
            },
            int4(2),
        ))
    }

    fn probe_on_id(index_name: &str, id: i32) -> Option<DmlIndexProbe> {
        Some(DmlIndexProbe {
            index_name: index_name.into(),
            key: IndexProbeSpec::equality(vec![(0, int4(id))]),
            residual: None,
        })
    }

    fn remaining(table: &Arc<dyn TableAm>) -> Vec<Tuple> {
        table
            .scan(&rtxn(), &ColumnProjection::All)
            .map(|row| test_ok(row).1)
            .collect()
    }

    /// A probed DML statement must agree with the scanned one row for row — the
    /// probe narrows the source, the predicate still decides.
    #[test]
    fn probed_delete_matches_the_scanned_delete() {
        for probe in [probe_on_id("t_id_key", 2), None] {
            let table = indexed_table();
            let Execution::Deleted(n) = test_ok(execute_delete(
                &table,
                &id_eq_2(),
                None,
                None,
                Vec::new(),
                probe.as_ref(),
                &[],
                &ExecContext::default(),
                &wtxn(),
            )) else {
                panic!("expected Deleted");
            };
            assert_eq!(n, 1);
            assert_eq!(
                remaining(&table),
                vec![
                    vec![Value::Int4(1), Value::Text("one".into())],
                    vec![Value::Int4(3), Value::Null],
                ]
            );
        }
    }

    #[test]
    fn probed_update_matches_the_scanned_update() {
        // `label` is not the unique key, so the planner would allow a probe here.
        let assignments = vec![(
            1usize,
            BoundExpr::Const {
                value: Value::Text("hit".into()),
                ty: PgType::Text,
            },
        )];
        for probe in [probe_on_id("t_id_key", 2), None] {
            let table = indexed_table();
            let Execution::Updated(n) = test_ok(execute_update(
                &table,
                &id_eq_2(),
                &assignments,
                None,
                None,
                Vec::new(),
                probe.as_ref(),
                &[],
                &ExecContext::default(),
                &wtxn(),
            )) else {
                panic!("expected Updated");
            };
            assert_eq!(n, 1);
            // By id, not scan order: an updated row lands at the end of the heap.
            let mut rows = remaining(&table);
            rows.sort_by_key(|row| match row[0] {
                Value::Int4(id) => id,
                _ => unreachable!("id is int4"),
            });
            assert_eq!(
                rows,
                vec![
                    vec![Value::Int4(1), Value::Text("one".into())],
                    vec![Value::Int4(2), Value::Text("hit".into())],
                    vec![Value::Int4(3), Value::Null],
                ]
            );
        }
    }

    /// A probe naming an index the engine cannot serve falls back to a scan, so
    /// the statement still affects exactly the predicate's rows.
    #[test]
    fn a_probe_the_engine_declines_falls_back_to_a_scan() {
        let table = test_table();
        let Execution::Deleted(n) = test_ok(execute_delete(
            &table,
            &id_eq_2(),
            None,
            None,
            Vec::new(),
            probe_on_id("missing_index", 2).as_ref(),
            &[],
            &ExecContext::default(),
            &wtxn(),
        )) else {
            panic!("expected Deleted");
        };
        assert_eq!(n, 1);
        assert_eq!(remaining(&table).len(), 2);
    }

    #[test]
    fn update_evaluates_against_old_row_and_buffers() -> anyhow::Result<()> {
        let table = test_table();
        // SET id = id + 1 for every row.
        let assignments = vec![(
            0usize,
            binary(
                BinOp::Add,
                PgType::Int4,
                BoundExpr::ColumnRef {
                    index: 0,
                    ty: PgType::Int4,
                },
                int4(1),
            ),
        )];
        let Execution::Updated(n) = execute_update(
            &table,
            &None,
            &assignments,
            None,
            None,
            Vec::new(),
            None,
            &[],
            &ExecContext::default(),
            &wtxn(),
        )?
        else {
            panic!("expected Updated");
        };
        assert_eq!(n, 3);
        let ids: Vec<Value> = table
            .scan(&rtxn(), &ColumnProjection::All)
            .map(|row| row.unwrap_or_else(|error| panic!("scan failed: {error}")).1[0].clone())
            .collect();
        assert_eq!(ids, vec![Value::Int4(2), Value::Int4(3), Value::Int4(4)]);

        Ok(())
    }

    #[test]
    fn failing_update_mutates_nothing() {
        let table = test_table();
        // id / (id - 2) fails on the id=2 row after the id=1 row succeeded.
        let assignments = vec![(
            0usize,
            binary(
                BinOp::Div,
                PgType::Int4,
                BoundExpr::ColumnRef {
                    index: 0,
                    ty: PgType::Int4,
                },
                binary(
                    BinOp::Sub,
                    PgType::Int4,
                    BoundExpr::ColumnRef {
                        index: 0,
                        ty: PgType::Int4,
                    },
                    int4(2),
                ),
            ),
        )];
        let Err(e) = execute_update(
            &table,
            &None,
            &assignments,
            None,
            None,
            Vec::new(),
            None,
            &[],
            &ExecContext::default(),
            &wtxn(),
        ) else {
            panic!("expected error");
        };
        assert_eq!(e.code, "22012");
        let ids: Vec<Value> = table
            .scan(&rtxn(), &ColumnProjection::All)
            .map(|row| row.unwrap_or_else(|error| panic!("scan failed: {error}")).1[0].clone())
            .collect();
        assert_eq!(ids, vec![Value::Int4(1), Value::Int4(2), Value::Int4(3)]);
    }

    #[test]
    fn delete_with_predicate_removes_matching_rows() -> anyhow::Result<()> {
        let table = test_table();
        let predicate = Some(binary(
            BinOp::Gt,
            PgType::Int4,
            BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4,
            },
            int4(1),
        ));
        let Execution::Deleted(n) = execute_delete(
            &table,
            &predicate,
            None,
            None,
            Vec::new(),
            None,
            &[],
            &ExecContext::default(),
            &wtxn(),
        )?
        else {
            panic!("expected Deleted");
        };
        assert_eq!(n, 2);
        assert_eq!(table.scan(&rtxn(), &ColumnProjection::All).count(), 1);

        Ok(())
    }

    /// A fresh engine with `t(id int4, label text)` seeded with three rows, for
    /// the `RETURNING` tests.
    fn returning_engine() -> Arc<dyn TableEngine> {
        let engine = crabgresql_pg_engine::ephemeral_engine();
        let table = test_ok(engine.create_table(TableSchema::in_namespace(
            "t",
            "public",
            vec![
                Column::new("id", PgType::Int4),
                Column::new("label", PgType::Text),
            ],
        )));
        let txn = wtxn();
        test_ok(table.insert(vec![Value::Int4(1), Value::Text("one".into())], &txn));
        test_ok(table.insert(vec![Value::Int4(2), Value::Text("two".into())], &txn));
        test_ok(table.insert(vec![Value::Int4(3), Value::Text("three".into())], &txn));
        engine
    }

    /// Parse → bind → plan → execute a DML `RETURNING` statement, draining the
    /// projected rows. Panics unless the plan produced [`Execution::ReturningRows`].
    fn run_returning(
        engine: &Arc<dyn TableEngine>,
        sql: &str,
    ) -> (Vec<OutputColumn>, Vec<Tuple>, DmlVerb) {
        use crabgresql_parser::ast;
        let stmts = test_ok(crabgresql_parser::parse(sql));
        let catalog: Arc<dyn crabgresql_storage_api::TypeCatalog> =
            Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
        let logical = match &stmts[0] {
            ast::Statement::Insert(insert) => {
                test_ok(crabgresql_binder::bind_insert(engine, &catalog, insert))
            }
            ast::Statement::Update(update) => {
                test_ok(crabgresql_binder::bind_update(engine, &catalog, update))
            }
            ast::Statement::Delete(delete) => {
                test_ok(crabgresql_binder::bind_delete(engine, &catalog, delete))
            }
            other => panic!("expected a DML statement, got {other:?}"),
        };
        let physical = crabgresql_planner::plan(logical, Default::default());
        let Execution::ReturningRows {
            columns,
            mut node,
            verb,
        } = test_ok(execute(physical, &ExecContext::default(), &wtxn()))
        else {
            panic!("expected ReturningRows");
        };
        (columns, collect(node.as_mut()), verb)
    }

    #[test]
    fn insert_returning_projects_inserted_rows() {
        let engine = returning_engine();
        let (columns, rows, verb) = run_returning(
            &engine,
            "INSERT INTO t (id, label) VALUES (10, 'ten'), (11, 'eleven') RETURNING id, label",
        );
        assert_eq!(verb, DmlVerb::Insert);
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "label"]);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(10), Value::Text("ten".into())],
                vec![Value::Int4(11), Value::Text("eleven".into())],
            ]
        );
        // The rows were actually persisted, not just projected.
        let table = test_ok(engine.open_table("t"));
        assert_eq!(table.scan(&rtxn(), &ColumnProjection::All).count(), 5);
    }

    #[test]
    fn insert_returning_star_and_computed_alias() {
        let engine = returning_engine();
        let (columns, rows, _) = run_returning(
            &engine,
            "INSERT INTO t (id, label) VALUES (10, 'ten') RETURNING *, id + 1 AS next",
        );
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "label", "next"]);
        assert_eq!(
            rows,
            vec![vec![
                Value::Int4(10),
                Value::Text("ten".into()),
                Value::Int4(11),
            ]]
        );
    }

    /// Parse → bind → plan → execute a non-RETURNING INSERT, returning the
    /// inserted row count. Panics unless the plan produced [`Execution::Inserted`].
    fn run_insert(engine: &Arc<dyn TableEngine>, sql: &str) -> u64 {
        use crabgresql_parser::ast;
        let stmts = test_ok(crabgresql_parser::parse(sql));
        let catalog: Arc<dyn crabgresql_storage_api::TypeCatalog> =
            Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
        let ast::Statement::Insert(insert) = &stmts[0] else {
            panic!("expected an INSERT statement");
        };
        let logical = test_ok(crabgresql_binder::bind_insert(engine, &catalog, insert));
        let physical = crabgresql_planner::plan(logical, Default::default());
        match test_ok(execute(physical, &ExecContext::default(), &wtxn())) {
            Execution::Inserted(n) => n,
            _ => panic!("expected Inserted"),
        }
    }

    #[test]
    fn insert_select_copies_rows() {
        let engine = returning_engine();
        // `INSERT ... SELECT` from the same table doubles it (the source is drained
        // under the snapshot before any write, so it never sees its own inserts).
        let inserted = run_insert(&engine, "INSERT INTO t (id, label) SELECT id, label FROM t");
        assert_eq!(inserted, 3);
        let table = test_ok(engine.open_table("t"));
        assert_eq!(table.scan(&rtxn(), &ColumnProjection::All).count(), 6);
    }

    #[test]
    fn insert_select_honors_order_by_and_limit() {
        let engine = returning_engine();
        let inserted = run_insert(
            &engine,
            "INSERT INTO t (id, label) SELECT id, label FROM t ORDER BY id DESC LIMIT 1",
        );
        assert_eq!(inserted, 1);
        let table = test_ok(engine.open_table("t"));
        assert_eq!(table.scan(&rtxn(), &ColumnProjection::All).count(), 4);
    }

    #[test]
    fn insert_table_source_copies_rows() {
        let engine = returning_engine();
        let inserted = run_insert(&engine, "INSERT INTO t (id, label) TABLE t");
        assert_eq!(inserted, 3);
        let table = test_ok(engine.open_table("t"));
        assert_eq!(table.scan(&rtxn(), &ColumnProjection::All).count(), 6);
    }

    #[test]
    fn insert_select_projects_returning() {
        let engine = returning_engine();
        let (columns, rows, verb) = run_returning(
            &engine,
            "INSERT INTO t (id, label) SELECT id, label FROM t WHERE id = 1 RETURNING id, label",
        );
        assert_eq!(verb, DmlVerb::Insert);
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "label"]);
        assert_eq!(rows, vec![vec![Value::Int4(1), Value::Text("one".into())]]);
    }

    #[test]
    fn update_returning_projects_new_rows() {
        let engine = returning_engine();
        let (columns, rows, verb) = run_returning(
            &engine,
            "UPDATE t SET id = id + 100 WHERE id > 1 RETURNING id, label",
        );
        assert_eq!(verb, DmlVerb::Update);
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "label"]);
        // The NEW (post-update) id values.
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(102), Value::Text("two".into())],
                vec![Value::Int4(103), Value::Text("three".into())],
            ]
        );
    }

    #[test]
    fn delete_returning_projects_deleted_rows_reordered() {
        let engine = returning_engine();
        let (columns, rows, verb) =
            run_returning(&engine, "DELETE FROM t WHERE id > 1 RETURNING label, id");
        assert_eq!(verb, DmlVerb::Delete);
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["label", "id"]);
        // The deleted (OLD) rows, columns reordered as requested.
        assert_eq!(
            rows,
            vec![
                vec![Value::Text("two".into()), Value::Int4(2)],
                vec![Value::Text("three".into()), Value::Int4(3)],
            ]
        );
        let table = test_ok(engine.open_table("t"));
        assert_eq!(table.scan(&rtxn(), &ColumnProjection::All).count(), 1);
    }

    /// Parse → bind → plan → execute a query against a fresh engine.
    fn run_rows(sql: &str) -> (Vec<OutputColumn>, Vec<Tuple>) {
        run_rows_on(
            &(crabgresql_pg_engine::ephemeral_engine() as Arc<dyn TableEngine>),
            sql,
        )
    }

    /// As [`run_rows`], but against a caller-provided engine (for queries over
    /// real tables).
    fn run_rows_on(engine: &Arc<dyn TableEngine>, sql: &str) -> (Vec<OutputColumn>, Vec<Tuple>) {
        let stmts = test_ok(crabgresql_parser::parse(sql));
        let crabgresql_parser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected a query");
        };
        let catalog: Arc<dyn crabgresql_storage_api::TypeCatalog> =
            Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
        let logical = test_ok(crabgresql_binder::bind_query(engine, &catalog, query));
        let physical = crabgresql_planner::plan(logical, Default::default());
        let Execution::Rows { columns, mut node } =
            test_ok(execute(physical, &ExecContext::default(), &rtxn()))
        else {
            panic!("expected rows");
        };
        let mut rows = Vec::new();
        while let Some(tuple) = test_ok(node.next()) {
            rows.push(tuple);
        }
        (columns, rows)
    }

    /// An engine with `t(a int, b int)` seeded with two groups (a=1,2) plus a
    /// singleton (a=3), and one NULL `b`.
    fn agg_engine() -> Arc<dyn TableEngine> {
        let engine = crabgresql_pg_engine::ephemeral_engine();
        let table = test_ok(engine.create_table(TableSchema::in_namespace(
            "t",
            "public",
            vec![
                Column::new("a", PgType::Int4),
                Column::new("b", PgType::Int4),
            ],
        )));
        let txn = wtxn();
        let seed: [(i32, Option<i32>); 5] = [
            (1, Some(10)),
            (1, Some(20)),
            (2, Some(5)),
            (2, None),
            (3, Some(7)),
        ];
        for (a, b) in seed {
            let b = b.map(Value::Int4).unwrap_or(Value::Null);
            test_ok(table.insert(vec![Value::Int4(a), b], &txn));
        }
        engine
    }

    #[test]
    fn whole_table_aggregates_ignore_nulls() {
        let engine = agg_engine();
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT count(*), count(b), min(b), max(b), sum(b) FROM t",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            vec![
                Value::Int8(5),  // count(*) — every row
                Value::Int8(4),  // count(b) — NULL skipped
                Value::Int4(5),  // min(b)
                Value::Int4(20), // max(b)
                Value::Int8(42), // sum(b): int4 widens to bigint
            ]
        );
    }

    #[test]
    fn string_agg_concatenates_skipping_null_values() {
        // NULL values are skipped; a per-row NULL delimiter contributes nothing.
        let (_c, rows) = run_rows(
            "SELECT string_agg(x, d) FROM (VALUES ('a', ','), (NULL, '/'), ('b', NULL), ('c', '-')) t(x, d)",
        );
        assert_eq!(rows, vec![vec![Value::Text("ab-c".to_string())]]);
    }

    #[test]
    fn string_agg_over_empty_group_is_null() {
        let (_c, rows) = run_rows("SELECT string_agg(x, ',') FROM (VALUES ('a')) t(x) WHERE false");
        assert_eq!(rows, vec![vec![Value::Null]]);
    }

    #[test]
    fn array_agg_keeps_nulls_and_arrival_order() {
        let (cols, rows) = run_rows("SELECT array_agg(x) FROM (VALUES (2), (NULL), (1)) t(x)");
        assert_eq!(cols[0].ty, PgType::Array(crabgresql_types::oid::INT4));
        assert_eq!(
            rows,
            vec![vec![Value::Array {
                elem: PgType::Int4,
                elems: vec![Value::Int4(2), Value::Null, Value::Int4(1)],
            }]]
        );
    }

    /// An aggregate's own `ORDER BY` decides the order its inputs are folded in
    /// — the whole point of the clause for `array_agg`. Values verified against
    /// PostgreSQL 18.4.
    #[test]
    fn array_agg_follows_its_own_order_by() {
        let ints = |ns: [i32; 3]| {
            vec![vec![Value::Array {
                elem: PgType::Int4,
                elems: ns.into_iter().map(Value::Int4).collect(),
            }]]
        };
        let rows = "(VALUES (1, 3), (2, 1), (3, 2)) t(x, y)";
        assert_eq!(
            run_rows(&format!("SELECT array_agg(x ORDER BY y) FROM {rows}")).1,
            ints([2, 3, 1])
        );
        assert_eq!(
            run_rows(&format!("SELECT array_agg(x ORDER BY y DESC) FROM {rows}")).1,
            ints([1, 3, 2])
        );
    }

    /// NULL placement follows the key's `NULLS FIRST`/`LAST`, defaulting to
    /// last for ASC — the same rule the query's own ORDER BY uses.
    #[test]
    fn aggregate_order_by_places_nulls_by_the_key() {
        let rows = "(VALUES (1, 3), (2, NULL), (3, 2)) t(x, y)";
        let ints = |ns: [i32; 3]| {
            vec![vec![Value::Array {
                elem: PgType::Int4,
                elems: ns.into_iter().map(Value::Int4).collect(),
            }]]
        };
        assert_eq!(
            run_rows(&format!("SELECT array_agg(x ORDER BY y) FROM {rows}")).1,
            ints([3, 1, 2])
        );
        assert_eq!(
            run_rows(&format!(
                "SELECT array_agg(x ORDER BY y NULLS FIRST) FROM {rows}"
            ))
            .1,
            ints([2, 3, 1])
        );
    }

    /// The clause is legal on every aggregate, not only the order-sensitive
    /// ones, and an empty group is still NULL rather than an empty array.
    #[test]
    fn aggregate_order_by_on_other_aggregates_and_empty_groups() {
        let (_c, rows) = run_rows(
            "SELECT string_agg(s, ',' ORDER BY k DESC) \
             FROM (VALUES ('a', 1), ('b', 2), ('c', 3)) t(s, k)",
        );
        assert_eq!(rows, vec![vec![Value::Text("c,b,a".to_string())]]);
        let (_c, rows) = run_rows("SELECT sum(x ORDER BY x) FROM (VALUES (1), (2)) t(x)");
        assert_eq!(rows, vec![vec![Value::Int8(3)]]);
        let (_c, rows) =
            run_rows("SELECT array_agg(x ORDER BY y) FROM (VALUES (1, 3)) t(x, y) WHERE false");
        assert_eq!(rows, vec![vec![Value::Null]]);
    }

    /// `DISTINCT` with an explicit `ORDER BY` dedups on the sorted keys rather
    /// than through `array_agg`'s finalize sort, so the ORDER BY — including a
    /// descending one, whose NULLs then come first — decides the output order.
    #[test]
    fn array_agg_distinct_honours_an_explicit_order_by() {
        let (_c, rows) = run_rows(
            "SELECT array_agg(DISTINCT x ORDER BY x DESC) \
             FROM (VALUES (3), (1), (NULL), (2), (1)) t(x)",
        );
        assert_eq!(
            rows,
            vec![vec![Value::Array {
                elem: PgType::Int4,
                elems: vec![Value::Null, Value::Int4(3), Value::Int4(2), Value::Int4(1)],
            }]]
        );
    }

    #[test]
    fn array_agg_over_empty_group_is_null() {
        // Not the empty array: a group of one NULL row is what gives `{NULL}`.
        let (_c, rows) = run_rows("SELECT array_agg(x) FROM (VALUES (1)) t(x) WHERE false");
        assert_eq!(rows, vec![vec![Value::Null]]);
    }

    #[test]
    fn array_agg_distinct_sorts_with_nulls_last() {
        let (_c, rows) =
            run_rows("SELECT array_agg(DISTINCT x) FROM (VALUES (3), (1), (NULL), (2), (1)) t(x)");
        assert_eq!(
            rows,
            vec![vec![Value::Array {
                elem: PgType::Int4,
                elems: vec![Value::Int4(1), Value::Int4(2), Value::Int4(3), Value::Null,],
            }]]
        );
    }

    #[test]
    fn array_agg_renders_as_an_array_literal() {
        // The quoting rules are `array_out`'s: an element with a comma or the
        // literal word NULL is quoted, a plain one is not.
        let (_c, rows) =
            run_rows("SELECT array_agg(x)::text FROM (VALUES ('a'), (NULL), ('b, c')) t(x)");
        assert_eq!(
            rows,
            vec![vec![Value::Text("{a,NULL,\"b, c\"}".to_string())]]
        );
    }

    #[test]
    fn array_upper_matches_length_on_dimension_one() {
        let (_c, rows) = run_rows(
            "SELECT array_upper(ARRAY[10, 20, 30], 1), array_upper('{}'::int[], 1), array_upper(ARRAY[1], 2)",
        );
        assert_eq!(rows, vec![vec![Value::Int4(3), Value::Null, Value::Null]]);
    }

    #[test]
    fn array_to_string_skips_or_replaces_nulls() {
        // Two-arg form skips NULLs; the optional third argument replaces them; a
        // NULL delimiter yields NULL.
        let (_c, rows) = run_rows(
            "SELECT array_to_string(ARRAY[1, NULL, 3], ','), array_to_string(ARRAY[1, NULL, 3], ',', '*'), array_to_string(ARRAY[1, 2], NULL)",
        );
        assert_eq!(
            rows,
            vec![vec![
                Value::Text("1,3".to_string()),
                Value::Text("1,*,3".to_string()),
                Value::Null,
            ]]
        );
    }

    #[test]
    fn array_subquery_collects_rows_in_subplan_order() {
        // Both spellings of "the subplan decides": arms written 2, 1 stay 2, 1,
        // and an inner ORDER BY reorders what the VALUES list fixed.
        let (columns, rows) = run_rows(
            "SELECT array(SELECT 2 UNION ALL SELECT 1), array(SELECT n FROM (VALUES (3), (1), (2)) v(n) ORDER BY n)",
        );
        assert_eq!(columns[0].name, "array");
        assert_eq!(
            rows,
            vec![vec![
                Value::Array {
                    elem: PgType::Int4,
                    elems: vec![Value::Int4(2), Value::Int4(1)],
                },
                Value::Array {
                    elem: PgType::Int4,
                    elems: vec![Value::Int4(1), Value::Int4(2), Value::Int4(3)],
                },
            ]]
        );
    }

    #[test]
    fn array_subquery_of_no_rows_is_the_empty_array_not_null() {
        // The one place this differs from a scalar subquery, checked through
        // `array_to_string` too, where a NULL would surface as a NULL row.
        let (_c, rows) = run_rows(
            "SELECT array(SELECT 1 WHERE false), array_to_string(array(SELECT 1 WHERE false), ',')",
        );
        assert_eq!(
            rows,
            vec![vec![
                Value::Array {
                    elem: PgType::Int4,
                    elems: Vec::new(),
                },
                Value::Text(String::new()),
            ]]
        );
    }

    #[test]
    fn array_subquery_keeps_null_rows_as_null_elements() {
        // Unlike a quantified subquery's candidate list, nothing here drops or
        // dedups: a NULL row is a NULL element, and duplicates are kept.
        let (_c, rows) =
            run_rows("SELECT array(SELECT n FROM (VALUES (1), (NULL), (1), (2)) v(n))");
        assert_eq!(
            rows,
            vec![vec![Value::Array {
                elem: PgType::Int4,
                elems: vec![Value::Int4(1), Value::Null, Value::Int4(1), Value::Int4(2)],
            }]]
        );
    }

    #[test]
    fn correlated_array_subquery_is_rebuilt_per_outer_row() -> anyhow::Result<()> {
        // `i.v = o.k` correlates, so the marker survives `resolve_subqueries`
        // and is folded against each outer row instead of once up front.
        let engine = exists_engine(&[(1, 10), (2, 20)], &[(Some(7), 1), (Some(8), 1)])?;
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT k, array(SELECT i.k FROM i WHERE i.v = o.k ORDER BY i.k) FROM o ORDER BY k",
        );
        assert_eq!(
            rows,
            vec![
                vec![
                    Value::Int4(1),
                    Value::Array {
                        elem: PgType::Int4,
                        elems: vec![Value::Int4(7), Value::Int4(8)],
                    },
                ],
                vec![
                    Value::Int4(2),
                    Value::Array {
                        elem: PgType::Int4,
                        elems: Vec::new(),
                    },
                ],
            ]
        );
        Ok(())
    }

    #[test]
    fn fromless_distinct_on_hidden_expression() {
        // A FROM-less `SELECT DISTINCT ON (expr)` where the ON expression is not
        // in the select list appends a hidden column; the Values pipeline must
        // keep that column through the sort (not trim to the visible width) so
        // Distinct can read it. Regression: previously truncated → out-of-bounds.
        let (columns, rows) = run_rows("SELECT DISTINCT ON (1 + 1) 5 ORDER BY 1 + 1");
        assert_eq!(
            columns.len(),
            1,
            "the hidden ON column never reaches output"
        );
        assert_eq!(rows, vec![vec![Value::Int4(5)]]);
    }

    #[test]
    fn distinct_aggregates_deduplicate_per_group_and_per_call() -> anyhow::Result<()> {
        let engine = crabgresql_pg_engine::ephemeral_engine();
        let table = engine.create_table(TableSchema::in_namespace(
            "d",
            "public",
            vec![
                Column::new("g", PgType::Int4),
                Column::new("v", PgType::Int4),
            ],
        ))?;
        let txn = wtxn();
        for (g, v) in [
            (1, Some(10)),
            (1, Some(10)),
            (1, None),
            (2, Some(5)),
            (2, Some(5)),
            (2, None),
        ] {
            table.insert(
                vec![Value::Int4(g), v.map(Value::Int4).unwrap_or(Value::Null)],
                &txn,
            )?;
        }
        let engine: Arc<dyn TableEngine> = engine;
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT g, count(DISTINCT v), sum(DISTINCT v), avg(DISTINCT v), min(DISTINCT v), max(DISTINCT v) FROM d GROUP BY g ORDER BY g",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(
            &rows[0][..3],
            &[Value::Int4(1), Value::Int8(1), Value::Int8(10)]
        );
        assert_eq!(&rows[0][4..], &[Value::Int4(10), Value::Int4(10)]);
        assert_eq!(
            &rows[1][..3],
            &[Value::Int4(2), Value::Int8(1), Value::Int8(5)]
        );
        assert_eq!(&rows[1][4..], &[Value::Int4(5), Value::Int4(5)]);
        let Value::Numeric(avg_one) = &rows[0][3] else {
            panic!("avg(int) should be numeric, got {:?}", rows[0][3]);
        };
        let Value::Numeric(avg_two) = &rows[1][3] else {
            panic!("avg(int) should be numeric, got {:?}", rows[1][3]);
        };
        assert_eq!(avg_one.to_display(), "10.0000000000000000");
        assert_eq!(avg_two.to_display(), "5.0000000000000000");

        // The two calls use independent seen-value sets, even when their
        // inputs have the same type and values.
        let (_c, rows) = run_rows_on(&engine, "SELECT count(DISTINCT g), sum(DISTINCT g) FROM d");
        assert_eq!(rows, vec![vec![Value::Int8(2), Value::Int8(3)]]);

        Ok(())
    }

    #[test]
    fn empty_group_is_zero_count_and_null_sum() {
        let engine = agg_engine();
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT count(*), sum(b), min(b) FROM t WHERE a > 100",
        );
        // The implicit group still yields one row.
        assert_eq!(rows, vec![vec![Value::Int8(0), Value::Null, Value::Null]]);
    }

    #[test]
    fn avg_of_integers_is_numeric() {
        let engine = agg_engine();
        let (_c, rows) = run_rows_on(&engine, "SELECT avg(b) FROM t");
        // 42 / 4 = 10.5, as numeric.
        let Value::Numeric(n) = &rows[0][0] else {
            panic!("avg(int) should be numeric, got {:?}", rows[0][0]);
        };
        assert_eq!(n.to_display(), "10.5000000000000000");
    }

    #[test]
    fn group_by_produces_one_row_per_group() {
        let engine = agg_engine();
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT a, count(*), sum(b) FROM t GROUP BY a ORDER BY a",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int8(2), Value::Int8(30)],
                vec![Value::Int4(2), Value::Int8(2), Value::Int8(5)],
                vec![Value::Int4(3), Value::Int8(1), Value::Int8(7)],
            ]
        );
    }

    #[test]
    fn having_filters_groups() {
        let engine = agg_engine();
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT a FROM t GROUP BY a HAVING count(*) > 1 ORDER BY a",
        );
        assert_eq!(rows, vec![vec![Value::Int4(1)], vec![Value::Int4(2)]]);
    }

    #[test]
    fn from_less_count_star_is_one() {
        let (_c, rows) = run_rows("SELECT count(*)");
        assert_eq!(rows, vec![vec![Value::Int8(1)]]);
    }

    #[test]
    fn max_minus_min_over_aggregate_row() {
        let engine = agg_engine();
        let (_c, rows) = run_rows_on(&engine, "SELECT max(b) - min(b) AS span FROM t");
        assert_eq!(rows, vec![vec![Value::Int4(15)]]);
    }

    #[test]
    fn group_by_null_key_forms_one_group() -> anyhow::Result<()> {
        // Rows with a NULL group key group together (NULL == NULL), distinct from
        // the non-NULL groups. Exercises the hash-grouping NULL path.
        let engine = crabgresql_pg_engine::ephemeral_engine();
        let table = engine.create_table(TableSchema::in_namespace(
            "g",
            "public",
            vec![
                Column::new("k", PgType::Int4),
                Column::new("v", PgType::Int4),
            ],
        ))?;
        let txn = wtxn();
        for (k, v) in [(Some(1), 10), (None, 20), (Some(1), 5), (None, 7)] {
            let k = k.map(Value::Int4).unwrap_or(Value::Null);
            table.insert(vec![k, Value::Int4(v)], &txn)?;
        }
        let engine: Arc<dyn TableEngine> = engine;
        // ORDER BY k with NULLS LAST-for-ASC (PG default) so the row order is fixed.
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT k, count(*), sum(v) FROM g GROUP BY k ORDER BY k",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int8(2), Value::Int8(15)],
                vec![Value::Null, Value::Int8(2), Value::Int8(27)],
            ]
        );

        Ok(())
    }

    /// Both zeros are one value and every NaN is one value, at either float
    /// width. `float4` reaches this through a widening cast, so it needs its own
    /// case: the cast alone preserves `-0.0`, and only the canonicalization
    /// after it folds the two zeros together.
    #[test]
    fn distinct_floats_fold_both_zeros_and_every_nan() {
        for ty in ["float4", "float8"] {
            let rows = format!(
                "(VALUES (0.0::{ty}), (-0.0::{ty}), ('NaN'::{ty}), ('NaN'::{ty}), (1.0::{ty})) t(x)"
            );
            let (_c, got) = run_rows(&format!("SELECT count(DISTINCT x) FROM {rows}"));
            assert_eq!(got, vec![vec![Value::Int8(3)]], "count(DISTINCT) over {ty}");
            let (_c, got) = run_rows(&format!("SELECT x FROM {rows} GROUP BY x"));
            assert_eq!(got.len(), 3, "GROUP BY over {ty}: {got:?}");
        }
    }

    /// A group carries DISTINCT state only when some aggregate asks for it, so
    /// the three shapes — none DISTINCT, some DISTINCT, all DISTINCT — have to
    /// agree on which accumulator gets which state. Reading that state by
    /// zipping it would feed *no* aggregate in the first shape.
    #[test]
    fn aggregates_are_fed_whether_or_not_any_is_distinct() {
        let rows = "(VALUES (1, 10), (1, 10), (1, 20), (2, 30)) t(g, x)";

        // No DISTINCT: the state vector is empty, and every aggregate must
        // still see every row.
        let (_c, got) = run_rows(&format!(
            "SELECT g, count(*), sum(x) FROM {rows} GROUP BY g ORDER BY g"
        ));
        assert_eq!(
            got,
            vec![
                vec![Value::Int4(1), Value::Int8(3), Value::Int8(40)],
                vec![Value::Int4(2), Value::Int8(1), Value::Int8(30)],
            ]
        );

        // Mixed: the DISTINCT state belongs to the second and fourth
        // aggregates, and the plain ones must not pick it up.
        let (_c, got) = run_rows(&format!(
            "SELECT g, count(*), count(DISTINCT x), sum(x), sum(DISTINCT x) \
             FROM {rows} GROUP BY g ORDER BY g"
        ));
        assert_eq!(
            got,
            vec![
                vec![
                    Value::Int4(1),
                    Value::Int8(3),
                    Value::Int8(2),
                    Value::Int8(40),
                    Value::Int8(30),
                ],
                vec![
                    Value::Int4(2),
                    Value::Int8(1),
                    Value::Int8(1),
                    Value::Int8(30),
                    Value::Int8(30),
                ],
            ]
        );

        // The implicit single group is built at its own call site, so it needs
        // its own case.
        let (_c, got) = run_rows(&format!("SELECT count(*), count(DISTINCT x) FROM {rows}"));
        assert_eq!(got, vec![vec![Value::Int8(4), Value::Int8(3)]]);
        let (_c, got) = run_rows(&format!("SELECT count(*), sum(x) FROM {rows}"));
        assert_eq!(got, vec![vec![Value::Int8(4), Value::Int8(70)]]);
    }

    /// `count(DISTINCT x)`, `SELECT DISTINCT x` and `GROUP BY x` are three
    /// separate lookups over the same notion of key equality, each with its own
    /// specialized storage. They may not disagree on how many values there are.
    #[test]
    fn distinct_paths_agree_on_key_equality() {
        // Text: case and *trailing* blanks each make a new value, and the two
        // NULLs are one group for GROUP BY but no value at all for the
        // aggregate.
        let rows = "(VALUES ('a'), ('a'), ('A'), ('a '), (' a'), (NULL), (NULL)) t(x)";
        let (_c, agg) = run_rows(&format!("SELECT count(DISTINCT x) FROM {rows}"));
        assert_eq!(agg, vec![vec![Value::Int8(4)]]);
        let (_c, distinct) = run_rows(&format!("SELECT DISTINCT x FROM {rows}"));
        assert_eq!(
            distinct.len(),
            5,
            "DISTINCT keeps the NULL group: {distinct:?}"
        );
        let (_c, grouped) = run_rows(&format!("SELECT x FROM {rows} GROUP BY x"));
        assert_eq!(
            grouped.len(),
            5,
            "GROUP BY keeps the NULL group: {grouped:?}"
        );

        // Numeric stays on the general path: display scale is not identity.
        let rows = "(VALUES (1), (1.0), (1.00), (2), (NULL)) t(x)";
        let (_c, agg) = run_rows(&format!("SELECT count(DISTINCT x) FROM {rows}"));
        assert_eq!(agg, vec![vec![Value::Int8(2)]]);
        let (_c, grouped) = run_rows(&format!("SELECT x FROM {rows} GROUP BY x"));
        assert_eq!(grouped.len(), 3, "two numerics and the NULL: {grouped:?}");
    }

    #[test]
    fn group_by_float_treats_neg_zero_and_nan_like_pg() -> anyhow::Result<()> {
        // -0.0 groups with 0.0, and NaN groups with NaN — the hash and keys_equal
        // must agree on both. Two 0.0-family rows, two NaN rows.
        let engine = crabgresql_pg_engine::ephemeral_engine();
        let table = engine.create_table(TableSchema::in_namespace(
            "f",
            "public",
            vec![Column::new("x", PgType::Float8)],
        ))?;
        let txn = wtxn();
        for x in [0.0_f64, -0.0, f64::NAN, f64::NAN] {
            table.insert(vec![Value::Float8(x)], &txn)?;
        }
        let engine: Arc<dyn TableEngine> = engine;
        let (_c, rows) = run_rows_on(&engine, "SELECT count(*) FROM f GROUP BY x");
        // Exactly two groups (the 0.0 family and the NaN family), each of size 2.
        let mut counts: Vec<Value> = rows.into_iter().map(|r| r[0].clone()).collect();
        counts.sort_by_key(|v| match v {
            Value::Int8(n) => *n,
            _ => -1,
        });
        assert_eq!(counts, vec![Value::Int8(2), Value::Int8(2)]);

        let (_c, rows) = run_rows_on(&engine, "SELECT count(DISTINCT x) FROM f");
        assert_eq!(rows, vec![vec![Value::Int8(2)]]);

        Ok(())
    }

    #[test]
    fn pg_input_error_info_reports_range_error() {
        let (columns, rows) = run_rows("SELECT * FROM pg_input_error_info('1e400', 'float4')");
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["message", "detail", "hint", "sql_error_code"]);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            vec![
                Value::Text("\"1e400\" is out of range for type real".into()),
                Value::Null,
                Value::Null,
                Value::Text("22003".into()),
            ]
        );
    }

    #[test]
    fn pg_input_error_info_is_all_null_for_valid_input() {
        let (_columns, rows) = run_rows("SELECT * FROM pg_input_error_info('34.5', 'float4')");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], vec![Value::Null; 4]);
    }

    /// Drain a query, returning the first runtime error (SRF errors surface on
    /// the first `next()`, not at plan time).
    fn run_err(sql: &str) -> ExecError {
        let engine: Arc<dyn TableEngine> = crabgresql_pg_engine::ephemeral_engine();
        let stmts = test_ok(crabgresql_parser::parse(sql));
        let crabgresql_parser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected a query");
        };
        let catalog: Arc<dyn crabgresql_storage_api::TypeCatalog> =
            Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
        let logical = test_ok(crabgresql_binder::bind_query(&engine, &catalog, query));
        let physical = crabgresql_planner::plan(logical, Default::default());
        let Execution::Rows { mut node, .. } =
            test_ok(execute(physical, &ExecContext::default(), &rtxn()))
        else {
            panic!("expected rows");
        };
        loop {
            match node.next() {
                Ok(Some(_)) => continue,
                Ok(None) => panic!("expected a runtime error for: {sql}"),
                Err(e) => return e,
            }
        }
    }

    /// The single `generate_series` column, extracted from result tuples.
    fn series_col(rows: &[Tuple]) -> Vec<Value> {
        rows.iter().map(|r| r[0].clone()).collect()
    }

    #[test]
    fn generate_series_from_yields_int4_range() {
        let (columns, rows) = run_rows("SELECT * FROM generate_series(1, 5)");
        assert_eq!(columns[0].name, "generate_series");
        assert_eq!(columns[0].ty, PgType::Int4);
        assert_eq!(
            series_col(&rows),
            (1..=5).map(Value::Int4).collect::<Vec<_>>()
        );
    }

    #[test]
    fn from_alias_names_the_series_column() {
        // The alias is both the relation qualifier and the output column name,
        // so `SELECT i` resolves and the result header says `i`.
        let (columns, rows) = run_rows("SELECT i FROM generate_series(1, 3) AS i");
        assert_eq!(columns[0].name, "i");
        assert_eq!(columns[0].ty, PgType::Int4);
        assert_eq!(
            series_col(&rows),
            (1..=3).map(Value::Int4).collect::<Vec<_>>()
        );
    }

    #[test]
    fn unnest_in_from_yields_elements() {
        let (columns, rows) = run_rows("SELECT u FROM unnest(ARRAY['a', 'b']) AS u");
        assert_eq!(columns[0].name, "u");
        assert_eq!(
            series_col(&rows),
            vec![Value::Text("a".into()), Value::Text("b".into())]
        );
    }

    #[test]
    fn unnest_of_an_outer_column_is_re_evaluated_per_row() {
        // The array is a column of the enclosing query, so the marker cannot be
        // folded once: each outer row substitutes its own array into the
        // function's argument. A NULL array yields no rows, and `ARRAY(SELECT …)`
        // over no rows is the empty array — not NULL.
        let (_c, rows) = run_rows(
            "SELECT ARRAY(SELECT u FROM unnest(a) u WHERE u > 'a')::text \
             FROM (VALUES (1, ARRAY['a', 'b', 'c']), (2, ARRAY['a']), (3, NULL::text[])) t(x, a) \
             ORDER BY x",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Text("{b,c}".to_string())],
                vec![Value::Text("{}".to_string())],
                vec![Value::Text("{}".to_string())],
            ]
        );
    }

    #[test]
    fn generate_subscripts_yields_the_valid_subscripts() {
        let (columns, rows) =
            run_rows("SELECT * FROM generate_subscripts(ARRAY['a', 'b', 'c'], 1)");
        assert_eq!(columns[0].name, "generate_subscripts");
        assert_eq!(columns[0].ty, PgType::Int4);
        assert_eq!(series_col(&rows), [1, 2, 3].map(Value::Int4));

        let (_c, rows) = run_rows("SELECT generate_subscripts(ARRAY['a', 'b', 'c'], 1, true)");
        assert_eq!(series_col(&rows), [3, 2, 1].map(Value::Int4));
        let (_c, rows) = run_rows("SELECT generate_subscripts(ARRAY['a', 'b', 'c'], 1, false)");
        assert_eq!(series_col(&rows), [1, 2, 3].map(Value::Int4));
    }

    #[test]
    fn generate_subscripts_pairs_with_a_subscripted_array() {
        // The idiom the function exists for: index and element side by side.
        let (_c, rows) = run_rows(
            "SELECT i, (ARRAY['x', 'y'])[i] FROM generate_subscripts(ARRAY['x', 'y'], 1) i",
        );
        let pairs: Vec<(Value, Value)> =
            rows.iter().map(|r| (r[0].clone(), r[1].clone())).collect();
        assert_eq!(
            pairs,
            vec![
                (Value::Int4(1), Value::Text("x".into())),
                (Value::Int4(2), Value::Text("y".into())),
            ]
        );
    }

    #[test]
    fn generate_subscripts_on_a_vector_is_zero_based() {
        // `oidvector`/`int2vector` are stored from 0, which is what their
        // subscripts are — `('11 22 33'::oidvector)[0]` is the first element.
        let (_c, rows) = run_rows("SELECT generate_subscripts('11 22 33'::oidvector, 1)");
        assert_eq!(series_col(&rows), [0, 1, 2].map(Value::Int4));
    }

    #[test]
    fn generate_subscripts_empty_cases_yield_no_rows() {
        for sql in [
            // A dimension the array does not have (the engine's arrays are 1-D,
            // so everything but 1 is absent), and a non-positive one.
            "SELECT generate_subscripts(ARRAY['a', 'b'], 0)",
            "SELECT generate_subscripts(ARRAY['a', 'b'], 2)",
            "SELECT generate_subscripts(ARRAY['a', 'b'], -1)",
            "SELECT generate_subscripts('{}'::text[], 1)",
            // STRICT: a NULL in any argument yields the empty set, not an error.
            "SELECT generate_subscripts(NULL::text[], 1)",
            "SELECT generate_subscripts(ARRAY['a', 'b'], NULL)",
            "SELECT generate_subscripts(ARRAY['a', 'b'], 1, NULL)",
        ] {
            let (_c, rows) = run_rows(sql);
            assert!(rows.is_empty(), "expected no rows for: {sql}");
        }
    }

    #[test]
    fn generate_series_target_list_yields_rows() {
        let (columns, rows) = run_rows("SELECT generate_series(1, 5)");
        assert_eq!(columns[0].name, "generate_series");
        assert_eq!(
            series_col(&rows),
            (1..=5).map(Value::Int4).collect::<Vec<_>>()
        );
    }

    #[test]
    fn generate_series_step_and_direction() {
        let (_c, rows) = run_rows("SELECT generate_series(1, 10, 3)");
        assert_eq!(series_col(&rows), [1, 4, 7, 10].map(Value::Int4));
        // A descending series with a negative step.
        let (_c, rows) = run_rows("SELECT generate_series(5, 1, -2)");
        assert_eq!(series_col(&rows), [5, 3, 1].map(Value::Int4));
    }

    #[test]
    fn generate_series_empty_ranges_yield_no_rows() {
        // Ascending series with start > stop.
        let (_c, rows) = run_rows("SELECT generate_series(5, 1)");
        assert!(rows.is_empty());
        // Positive step in the wrong direction.
        let (_c, rows) = run_rows("SELECT generate_series(5, 1, 1)");
        assert!(rows.is_empty());
    }

    #[test]
    fn generate_series_zero_step_is_22023() {
        let e = run_err("SELECT generate_series(1, 5, 0)");
        assert_eq!(e.code, "22023");
        assert_eq!(e.message, "step size cannot equal zero");
    }

    #[test]
    fn generate_series_int8_range() {
        let (columns, rows) = run_rows("SELECT generate_series(1, 5000000001, 2500000000)");
        assert_eq!(columns[0].ty, PgType::Int8);
        assert_eq!(
            series_col(&rows),
            [1_i64, 2_500_000_001, 5_000_000_001].map(Value::Int8)
        );
    }

    #[test]
    fn generate_series_mixed_with_scalar_over_table() {
        let engine = engine_with_nums(); // nums(n int4) = 1, 2, 3
        let (columns, rows) = run_rows_on(&engine, "SELECT n, generate_series(1, 2) FROM nums");
        assert_eq!(columns.len(), 2);
        // Each of the 3 input rows expands to 2 output rows (scalar repeats).
        assert_eq!(rows.len(), 6);
        let pairs: Vec<(Value, Value)> =
            rows.iter().map(|r| (r[0].clone(), r[1].clone())).collect();
        assert!(pairs.contains(&(Value::Int4(1), Value::Int4(1))));
        assert!(pairs.contains(&(Value::Int4(1), Value::Int4(2))));
        assert!(pairs.contains(&(Value::Int4(3), Value::Int4(2))));
    }

    /// The single generate_series column, rendered as PG-formatted text.
    fn series_text(rows: &[Tuple]) -> Vec<String> {
        rows.iter()
            .map(|r| {
                r[0].encode_text_with(&FmtCtx::utc_default())
                    .unwrap_or_default()
            })
            .collect()
    }

    #[test]
    fn generate_series_numeric_range_keeps_scale() {
        // The start keeps its scale ("1"); adding 0.5 gives scale 1 thereafter.
        let (columns, rows) = run_rows("SELECT generate_series(1, 3, 0.5)");
        assert_eq!(columns[0].ty, PgType::Numeric);
        assert_eq!(series_text(&rows), ["1", "1.5", "2.0", "2.5", "3.0"]);
    }

    #[test]
    fn generate_series_numeric_default_step_and_backward() {
        // 2-arg numeric defaults the step to 1.
        let (_c, rows) = run_rows("SELECT generate_series(1.5, 3)");
        assert_eq!(series_text(&rows), ["1.5", "2.5"]);
        // A negative numeric step counts down.
        let (_c, rows) = run_rows("SELECT generate_series(3.0, 1.0, -0.5)");
        assert_eq!(series_text(&rows), ["3.0", "2.5", "2.0", "1.5", "1.0"]);
    }

    #[test]
    fn generate_series_numeric_nan_bounds_error_22023() {
        for (sql, msg) in [
            (
                "SELECT generate_series('NaN'::numeric, 3)",
                "start value cannot be NaN",
            ),
            (
                "SELECT generate_series(1, 'NaN'::numeric)",
                "stop value cannot be NaN",
            ),
            (
                "SELECT generate_series(1, 3, 'NaN'::numeric)",
                "step size cannot be NaN",
            ),
        ] {
            let e = run_err(sql);
            assert_eq!(e.code, "22023", "{sql}");
            assert_eq!(e.message, msg, "{sql}");
        }
    }

    #[test]
    fn generate_series_numeric_infinite_bounds_error_22023() {
        // Infinite numeric bounds and step are rejected rather than looping
        // forever, all three with "cannot be infinity" (only the interval-step
        // overload says "cannot be infinite").
        for (sql, msg) in [
            (
                "SELECT generate_series('infinity'::numeric, 3)",
                "start value cannot be infinity",
            ),
            (
                "SELECT generate_series(1, 'infinity'::numeric)",
                "stop value cannot be infinity",
            ),
            (
                "SELECT generate_series(1, 3, 'infinity'::numeric)",
                "step size cannot be infinity",
            ),
        ] {
            let e = run_err(sql);
            assert_eq!(e.code, "22023", "{sql}");
            assert_eq!(e.message, msg, "{sql}");
        }
    }

    #[test]
    fn generate_series_null_argument_short_circuits_before_validation() {
        // `generate_series` is strict: a NULL argument yields 0 rows before any
        // NaN / infinity / zero-step validation fires.
        for sql in [
            "SELECT generate_series(NULL::int, 5, 0)",
            "SELECT generate_series(NULL::numeric, 'NaN'::numeric)",
            "SELECT generate_series(NULL::numeric, 'infinity'::numeric)",
            "SELECT generate_series(1, 3, NULL::numeric)",
            "SELECT generate_series(NULL::timestamp, timestamp '2020-01-05', interval '0')",
        ] {
            let (_c, rows) = run_rows(sql);
            assert!(rows.is_empty(), "{sql} should yield no rows");
        }
    }

    #[test]
    fn generate_series_timestamp_forward_and_backward() {
        let (columns, rows) = run_rows(
            "SELECT generate_series(timestamp '2020-01-01', timestamp '2020-01-04', \
             interval '1 day')",
        );
        assert_eq!(columns[0].ty, PgType::Timestamp);
        assert_eq!(
            series_text(&rows),
            [
                "2020-01-01 00:00:00",
                "2020-01-02 00:00:00",
                "2020-01-03 00:00:00",
                "2020-01-04 00:00:00",
            ]
        );
        // A negative interval steps backward.
        let (_c, rows) = run_rows(
            "SELECT generate_series(timestamp '2020-01-03', timestamp '2020-01-01', \
             interval '-1 day')",
        );
        assert_eq!(
            series_text(&rows),
            [
                "2020-01-03 00:00:00",
                "2020-01-02 00:00:00",
                "2020-01-01 00:00:00"
            ]
        );
    }

    #[test]
    fn generate_series_timestamp_month_step_clamps_day() {
        // pl_interval clamps the day-of-month, incrementally from cur.
        let (_c, rows) = run_rows(
            "SELECT generate_series(timestamp '2020-01-31', timestamp '2020-04-30', \
             interval '1 month')",
        );
        assert_eq!(
            series_text(&rows),
            [
                "2020-01-31 00:00:00",
                "2020-02-29 00:00:00",
                "2020-03-29 00:00:00",
                "2020-04-29 00:00:00",
            ]
        );
    }

    #[test]
    fn generate_series_timestamp_zero_interval_is_22023() {
        let e = run_err(
            "SELECT generate_series(timestamp '2020-01-01', timestamp '2020-01-05', interval '0')",
        );
        assert_eq!(e.code, "22023");
        assert_eq!(e.message, "step size cannot equal zero");
    }

    #[test]
    fn generate_series_timestamp_overflow_is_22008() {
        // Stepping past the max timestamp raises rather than silently stopping.
        let e = run_err(
            "SELECT generate_series(timestamp '294276-12-30', timestamp '294276-12-31', \
             interval '1 day')",
        );
        assert_eq!(e.code, "22008");
        assert_eq!(e.message, "timestamp out of range");
    }

    #[test]
    fn generate_series_timestamptz_forward_range() {
        let (columns, rows) = run_rows(
            "SELECT generate_series(timestamptz '2020-01-01 00:00+00', \
             timestamptz '2020-01-03 00:00+00', interval '1 day')",
        );
        assert_eq!(columns[0].ty, PgType::TimestampTz);
        assert_eq!(
            series_text(&rows),
            [
                "2020-01-01 00:00:00+00",
                "2020-01-02 00:00:00+00",
                "2020-01-03 00:00:00+00",
            ]
        );
    }

    /// A `nums(n int4)` table seeded with 1, 2, 3.
    fn engine_with_nums() -> Arc<dyn TableEngine> {
        let engine: Arc<dyn TableEngine> = crabgresql_pg_engine::ephemeral_engine();
        let table = test_ok(engine.create_table(TableSchema::in_namespace(
            "nums",
            "public",
            vec![Column::new("n", PgType::Int4)],
        )));
        let txn = wtxn();
        for n in [1, 2, 3] {
            test_ok(table.insert(vec![Value::Int4(n)], &txn));
        }
        engine
    }

    #[test]
    fn standalone_values_names_columns_and_keeps_rows() {
        let (columns, rows) = run_rows("VALUES (1), (2), (3)");
        assert_eq!(columns[0].name, "column1");
        assert_eq!(columns[0].ty, PgType::Int4);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1)],
                vec![Value::Int4(2)],
                vec![Value::Int4(3)],
            ]
        );
    }

    #[test]
    fn values_column_unifies_to_common_type() {
        // Mixed int4/int8 widens the whole column to int8.
        let (columns, rows) = run_rows("VALUES (1), (9000000000)");
        assert_eq!(columns[0].ty, PgType::Int8);
        assert_eq!(rows[0], vec![Value::Int8(1)]);
        assert_eq!(rows[1], vec![Value::Int8(9_000_000_000)]);
    }

    #[test]
    fn derived_table_projects_and_filters() {
        let (columns, rows) =
            run_rows("SELECT y FROM (VALUES (1, 'a'), (2, 'b')) v(x, y) WHERE x > 1");
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "y");
        assert_eq!(rows, vec![vec![Value::Text("b".into())]]);
    }

    #[test]
    fn cte_of_values_is_scannable_by_name() {
        let (columns, rows) =
            run_rows("WITH t(x) AS (VALUES (1), (2)) SELECT x FROM t WHERE x = 2");
        assert_eq!(columns[0].name, "x");
        assert_eq!(rows, vec![vec![Value::Int4(2)]]);
    }

    #[test]
    fn cte_over_table_and_ordering() {
        let engine = engine_with_nums();
        let (columns, rows) = run_rows_on(
            &engine,
            "WITH big AS (SELECT n FROM nums WHERE n >= 2) SELECT n FROM big ORDER BY 1 DESC",
        );
        assert_eq!(columns[0].name, "n");
        assert_eq!(rows, vec![vec![Value::Int4(3)], vec![Value::Int4(2)]]);
    }

    #[test]
    fn derived_table_over_real_table() {
        let engine = engine_with_nums();
        let (_columns, rows) = run_rows_on(
            &engine,
            "SELECT n FROM (SELECT n FROM nums WHERE n <> 2) s ORDER BY 1",
        );
        assert_eq!(rows, vec![vec![Value::Int4(1)], vec![Value::Int4(3)]]);
    }

    #[test]
    fn aggregate_over_derived_table() {
        let engine = engine_with_nums();
        // sum() over a derived table (subquery in FROM): the FROM form that used
        // to error with "aggregates over this FROM form are not supported yet".
        let (columns, rows) = run_rows_on(
            &engine,
            "SELECT sum(n) FROM (SELECT n FROM nums WHERE n <> 2) s",
        );
        assert_eq!(columns.len(), 1);
        assert_eq!(rows, vec![vec![Value::Int8(4)]]);
    }

    #[test]
    fn grouped_aggregate_over_derived_table() {
        let engine = agg_engine();
        let (columns, rows) = run_rows_on(
            &engine,
            "SELECT a, sum(b) FROM (SELECT a, b FROM t) s GROUP BY a ORDER BY a",
        );
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "sum"]);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int8(30)],
                vec![Value::Int4(2), Value::Int8(5)],
                vec![Value::Int4(3), Value::Int8(7)],
            ]
        );
    }

    #[test]
    fn aggregate_over_values_in_from() {
        let (_c, rows) = run_rows("SELECT count(*), sum(x) FROM (VALUES (1), (2), (3)) v(x)");
        assert_eq!(rows, vec![vec![Value::Int8(3), Value::Int8(6)]]);
    }

    #[test]
    fn aggregate_over_cte_reference() {
        let engine = engine_with_nums();
        let (_c, rows) = run_rows_on(
            &engine,
            "WITH big AS (SELECT n FROM nums WHERE n >= 2) SELECT sum(n) FROM big",
        );
        assert_eq!(rows, vec![vec![Value::Int8(5)]]);
    }

    #[test]
    fn aggregate_over_set_returning_function() {
        let (_c, rows) =
            run_rows("SELECT count(*), sum(generate_series) FROM generate_series(1, 3)");
        assert_eq!(rows, vec![vec![Value::Int8(3), Value::Int8(6)]]);
    }

    #[test]
    fn cross_join_of_values_is_cartesian_in_pg_order() {
        // First relation outermost (slowest), last relation innermost (fastest).
        let (columns, rows) =
            run_rows("SELECT * FROM (VALUES (1), (2)) a(x), (VALUES (10), (20)) b(y)");
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["x", "y"]);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int4(10)],
                vec![Value::Int4(1), Value::Int4(20)],
                vec![Value::Int4(2), Value::Int4(10)],
                vec![Value::Int4(2), Value::Int4(20)],
            ]
        );
    }

    #[test]
    fn cross_join_over_real_tables_with_join_predicate() {
        let engine = engine_with_nums();
        let (_columns, rows) = run_rows_on(
            &engine,
            "SELECT a.n, b.n FROM nums a, nums b WHERE a.n = b.n ORDER BY 1",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int4(1)],
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(3), Value::Int4(3)],
            ]
        );
    }

    #[test]
    fn explicit_cross_join_matches_comma_semantics() {
        let (_columns, rows) =
            run_rows("SELECT * FROM (VALUES (1)) a(x) CROSS JOIN (VALUES (7), (8)) b(y)");
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int4(7)],
                vec![Value::Int4(1), Value::Int4(8)],
            ]
        );
    }

    #[test]
    fn cross_join_with_an_empty_relation_yields_no_rows() {
        // The inner relation is empty, so the product is empty.
        let (_columns, rows) =
            run_rows("SELECT * FROM (VALUES (1), (2)) a(x), (SELECT 1 WHERE false) b(z)");
        assert!(rows.is_empty());
    }

    #[test]
    fn inner_join_matches_duplicates_and_rejects_null_predicates() {
        let (_columns, rows) = run_rows(
            "SELECT a.x, b.y \
             FROM (VALUES (1), (2), (NULL)) a(x) \
             JOIN (VALUES (2), (2), (3), (NULL)) b(y) ON a.x = b.y",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(2), Value::Int4(2)],
            ]
        );
    }

    #[test]
    fn left_right_and_full_join_null_extend_unmatched_rows() {
        let values = "(VALUES (1), (2), (NULL)) a(x) JOIN_KIND \
             (VALUES (2), (2), (3), (NULL)) b(y) ON a.x = b.y";
        let query =
            |kind: &str| format!("SELECT a.x, b.y FROM {}", values.replace("JOIN_KIND", kind));

        let (_, left) = run_rows(&query("LEFT JOIN"));
        assert_eq!(
            left,
            vec![
                vec![Value::Int4(1), Value::Null],
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Null, Value::Null],
            ]
        );

        let (_, right) = run_rows(&query("RIGHT JOIN"));
        assert_eq!(
            right,
            vec![
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Null, Value::Int4(3)],
                vec![Value::Null, Value::Null],
            ]
        );

        let (_, full) = run_rows(&query("FULL JOIN"));
        assert_eq!(
            full,
            vec![
                vec![Value::Int4(1), Value::Null],
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Null, Value::Null],
                vec![Value::Null, Value::Int4(3)],
                vec![Value::Null, Value::Null],
            ]
        );
    }

    #[test]
    fn outer_join_handles_empty_preserved_side() {
        let (_, right) = run_rows(
            "SELECT a.x, b.y FROM (SELECT 1 WHERE false) a(x) \
             RIGHT JOIN (VALUES (7), (8)) b(y) ON true",
        );
        assert_eq!(
            right,
            vec![
                vec![Value::Null, Value::Int4(7)],
                vec![Value::Null, Value::Int4(8)],
            ]
        );
        let (_, left) = run_rows(
            "SELECT a.x, b.y FROM (VALUES (7), (8)) a(x) \
             LEFT JOIN (SELECT 1 WHERE false) b(y) ON true",
        );
        assert_eq!(
            left,
            vec![
                vec![Value::Int4(7), Value::Null],
                vec![Value::Int4(8), Value::Null],
            ]
        );
    }

    #[test]
    fn chained_outer_join_predicate_sees_null_extended_left_row() {
        let (_, rows) = run_rows(
            "SELECT a.x, b.y, c.z FROM (VALUES (1)) a(x) \
             LEFT JOIN (VALUES (9)) b(y) ON false \
             JOIN (VALUES (2)) c(z) ON b.y IS NULL",
        );
        assert_eq!(
            rows,
            vec![vec![Value::Int4(1), Value::Null, Value::Int4(2)]]
        );
    }

    #[test]
    fn aggregates_and_grouping_consume_outer_join_rows() {
        let (_, rows) = run_rows(
            "SELECT count(*), count(b.y) \
             FROM (VALUES (1), (2), (NULL)) a(x) \
             LEFT JOIN (VALUES (2), (2), (3), (NULL)) b(y) ON a.x = b.y",
        );
        assert_eq!(rows, vec![vec![Value::Int8(4), Value::Int8(2)]]);

        let (_, grouped) = run_rows(
            "SELECT a.x, count(b.y) \
             FROM (VALUES (1), (2), (NULL)) a(x) \
             LEFT JOIN (VALUES (2), (2)) b(y) ON a.x = b.y \
             GROUP BY a.x HAVING count(*) >= 1 ORDER BY a.x",
        );
        assert_eq!(
            grouped,
            vec![
                vec![Value::Int4(1), Value::Int8(0)],
                vec![Value::Int4(2), Value::Int8(2)],
                vec![Value::Null, Value::Int8(0)],
            ]
        );
    }

    #[test]
    fn hash_join_matches_duplicate_keys_on_both_sides() {
        // Two left rows and two right rows share key 2, so the equi-join emits
        // their 2×2 cross product — left-driven, right rows in input order.
        let (_columns, rows) = run_rows(
            "SELECT a.x, b.y \
             FROM (VALUES (1), (2), (2)) a(x) \
             JOIN (VALUES (2), (2), (3)) b(y) ON a.x = b.y",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(2), Value::Int4(2)],
            ]
        );
    }

    #[test]
    fn hash_join_on_composite_key() {
        // Both key columns must match; (1,20) matches only the right (1,20) row.
        let (_columns, rows) = run_rows(
            "SELECT a.x, a.y, b.z \
             FROM (VALUES (1, 10), (1, 20)) a(x, y) \
             JOIN (VALUES (1, 20, 100), (1, 10, 200), (2, 20, 300)) b(x, y, z) \
             ON a.x = b.x AND a.y = b.y \
             ORDER BY a.y",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int4(10), Value::Int4(200)],
                vec![Value::Int4(1), Value::Int4(20), Value::Int4(100)],
            ]
        );
    }

    #[test]
    fn hash_join_applies_residual_predicate() {
        // `a.x = b.x` is the hash key; `a.v < b.v` is a residual conjunct checked
        // per candidate pair. Only the pair (x=1, 5 < 9) survives.
        let (_columns, rows) = run_rows(
            "SELECT a.v, b.v \
             FROM (VALUES (1, 5), (2, 9)) a(x, v) \
             JOIN (VALUES (1, 9), (1, 3), (2, 1)) b(x, v) ON a.x = b.x AND a.v < b.v",
        );
        assert_eq!(rows, vec![vec![Value::Int4(5), Value::Int4(9)]]);
    }

    /// Two tables of `(k, v)` int4 rows, for the hashed-`EXISTS` tests.
    fn exists_engine(
        outer: &[(i32, i32)],
        inner: &[(Option<i32>, i32)],
    ) -> anyhow::Result<Arc<dyn TableEngine>> {
        let engine = crabgresql_pg_engine::ephemeral_engine();
        let o = engine.create_table(TableSchema::in_namespace(
            "o",
            "public",
            vec![
                Column::new("k", PgType::Int4),
                Column::new("v", PgType::Int4),
            ],
        ))?;
        let i = engine.create_table(TableSchema::in_namespace(
            "i",
            "public",
            vec![
                Column::new("k", PgType::Int4),
                Column::new("v", PgType::Int4),
            ],
        ))?;
        let txn = wtxn();
        for (k, v) in outer {
            o.insert(vec![Value::Int4(*k), Value::Int4(*v)], &txn)?;
        }
        for (k, v) in inner {
            let k = k.map_or(Value::Null, Value::Int4);
            i.insert(vec![k, Value::Int4(*v)], &txn)?;
        }
        Ok(engine as Arc<dyn TableEngine>)
    }

    #[test]
    fn a_cross_type_correlation_still_hashes() -> anyhow::Result<()> {
        // `unify_types` wraps the *narrower* operand, so an int4 outer column
        // against an int8 inner one arrives as `Coerce{OuterColumnRef}` and used
        // to fall off the hashed path entirely. Both orientations must answer
        // the same, and the same as the per-row path.
        let engine = crabgresql_pg_engine::ephemeral_engine();
        let narrow = engine.create_table(TableSchema::in_namespace(
            "narrow",
            "public",
            vec![Column::new("k", PgType::Int4)],
        ))?;
        let wide = engine.create_table(TableSchema::in_namespace(
            "wide",
            "public",
            vec![Column::new("k", PgType::Int8)],
        ))?;
        let txn = wtxn();
        for k in [1_i32, 2, 3] {
            narrow.insert(vec![Value::Int4(k)], &txn)?;
        }
        for k in [2_i64, 3, 9] {
            wide.insert(vec![Value::Int8(k)], &txn)?;
        }
        let engine: Arc<dyn TableEngine> = engine;
        for sql in [
            "SELECT k FROM narrow o WHERE EXISTS (SELECT 1 FROM wide i WHERE i.k = o.k) ORDER BY k",
            "SELECT k FROM narrow o WHERE EXISTS (SELECT 1 FROM wide i WHERE o.k = i.k) ORDER BY k",
        ] {
            let (_c, rows) = run_rows_on(&engine, sql);
            assert_eq!(
                rows,
                vec![vec![Value::Int4(2)], vec![Value::Int4(3)]],
                "{sql}"
            );
        }
        Ok(())
    }

    #[test]
    fn hashed_exists_matches_the_per_row_answer() -> anyhow::Result<()> {
        // The correlation `i.k = o.k` is stripped and hashed; the residual
        // `i.v > 0` still applies. o.k = 3 has an inner row but it fails the
        // residual, and o.k = 4 has none at all.
        let engine = exists_engine(
            &[(1, 10), (2, 20), (3, 30), (4, 40)],
            &[(Some(1), 1), (Some(2), 5), (Some(3), -1)],
        )?;
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT k FROM o WHERE EXISTS (SELECT 1 FROM i WHERE i.k = o.k AND i.v > 0) ORDER BY k",
        );
        assert_eq!(rows, vec![vec![Value::Int4(1)], vec![Value::Int4(2)]]);

        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT k FROM o WHERE NOT EXISTS (SELECT 1 FROM i WHERE i.k = o.k AND i.v > 0) \
             ORDER BY k",
        );
        assert_eq!(rows, vec![vec![Value::Int4(3)], vec![Value::Int4(4)]]);
        Ok(())
    }

    #[test]
    fn hashed_exists_never_matches_a_null_key() -> anyhow::Result<()> {
        // `i.k = o.k` is never true when either side is NULL, so a NULL inner key
        // must stay out of the hash table and a NULL outer key must match nothing
        // — the same rule the hash join follows. Without it, NULL would collide
        // with NULL in one bucket and `EXISTS` would wrongly report a row.
        let engine = exists_engine(&[(1, 10)], &[(None, 1), (Some(1), 1)])?;
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT count(*) FROM o WHERE EXISTS (SELECT 1 FROM i WHERE i.k = o.v)",
        );
        assert_eq!(
            rows,
            vec![vec![Value::Int8(0)]],
            "o.v = 10 matches no inner key, and the NULL key is not one"
        );
        Ok(())
    }

    #[test]
    fn in_subquery_candidates_of_a_foreign_type_are_left_alone() {
        // The candidates are numerics while the comparison is on float8, since
        // they are coerced per row and not at fold time. Hashing them as float8
        // would reach the wrong accumulator; the dedup has to read the type off
        // the values. `1.000000000000000000001` is a distinct numeric that still
        // rounds to 1.0, so it must not be dropped either.
        let (_c, rows) = run_rows(
            "SELECT f FROM (VALUES (1::float8), (2::float8)) t(f) \
             WHERE f IN (SELECT n FROM (VALUES (1::numeric), \
                                               (1.000000000000000000001::numeric)) u(n)) \
             ORDER BY f",
        );
        assert_eq!(rows, vec![vec![Value::Float8(1.0)]]);
    }

    #[test]
    fn in_subquery_candidates_are_deduplicated() -> anyhow::Result<()> {
        // Six candidate rows over two distinct values, plus NULLs. Dropping the
        // duplicates must not change either answer: `IN` still matches, and
        // `NOT IN` is still NULL rather than false because a NULL candidate
        // survives the dedup.
        let engine = exists_engine(
            &[(1, 0), (3, 0)],
            &[
                (Some(1), 0),
                (Some(1), 0),
                (Some(2), 0),
                (Some(2), 0),
                (None, 0),
                (None, 0),
            ],
        )?;
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT o.k, o.k IN (SELECT i.k FROM i) FROM o ORDER BY o.k",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Bool(true)],
                // 3 matches nothing, but a NULL candidate makes the answer NULL.
                vec![Value::Int4(3), Value::Null],
            ]
        );
        Ok(())
    }

    /// Run `sql` twice — once through the logical optimizer, which rewrites a
    /// correlated subquery into a join, and once with that rule turned off, which
    /// leaves the executor's per-row path to answer it — and assert the two agree.
    ///
    /// The pair is the point: the per-row path is the definition of the right
    /// answer here, and a decorrelation that quietly changes cardinality, NULL
    /// handling or an empty group's value shows up as a difference and nowhere
    /// else. Every query passed in orders its rows, so the comparison is on
    /// content rather than on the order two different plans happen to produce.
    fn same_decorrelated_or_not(engine: &Arc<dyn TableEngine>, sql: &str) -> Vec<Tuple> {
        let decorrelated = run_optimized(engine, sql, true);
        let per_row = run_optimized(engine, sql, false);
        assert_eq!(decorrelated, per_row, "the two paths disagree on `{sql}`");
        decorrelated
    }

    fn run_optimized(engine: &Arc<dyn TableEngine>, sql: &str, decorrelate: bool) -> Vec<Tuple> {
        let stmts = test_ok(crabgresql_parser::parse(sql));
        let crabgresql_parser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected a query");
        };
        let catalog: Arc<dyn crabgresql_storage_api::TypeCatalog> =
            Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
        let mut logical = test_ok(crabgresql_binder::bind_query(engine, &catalog, query));
        let mut ctx = crabgresql_optimizer::OptimizerContext::new(FmtCtx::utc_default());
        ctx.decorrelate = decorrelate;
        crabgresql_optimizer::optimize(&mut logical, &ctx);
        let Execution::Rows { mut node, .. } = test_ok(execute(
            crabgresql_planner::plan(logical, Default::default()),
            &ExecContext::default(),
            &rtxn(),
        )) else {
            panic!("expected rows");
        };
        let mut rows = Vec::new();
        while let Some(tuple) = test_ok(node.next()) {
            rows.push(tuple);
        }
        rows
    }

    /// A semi join must not multiply the outer row the way an inner join would:
    /// `k = 1` matches three inner rows and still counts once. The inner NULL key
    /// matches nothing, and `k = 4` has no inner row at all.
    #[test]
    fn a_decorrelated_exists_neither_duplicates_nor_invents_rows() -> anyhow::Result<()> {
        let engine = exists_engine(
            &[(1, 10), (2, 20), (4, 40)],
            &[
                (Some(1), 1),
                (Some(1), 2),
                (Some(1), 3),
                (Some(2), 5),
                (None, 9),
            ],
        )?;
        let rows = same_decorrelated_or_not(
            &engine,
            "SELECT k FROM o WHERE EXISTS (SELECT 1 FROM i WHERE i.k = o.k) ORDER BY k",
        );
        assert_eq!(rows, vec![vec![Value::Int4(1)], vec![Value::Int4(2)]]);
        Ok(())
    }

    /// TPC-H Q21's shape: the correlation is an equality *and* an inequality, the
    /// second of which can only be a filter on the match test. Both `EXISTS` and
    /// `NOT EXISTS` have to keep answering what they did — the inequality decides
    /// which inner rows count as a match, so getting it lost would make every
    /// outer row match, and dropping the wrong side of it would invert the anti
    /// join.
    #[test]
    fn a_correlated_inequality_still_decides_the_match() -> anyhow::Result<()> {
        let engine = exists_engine(
            &[(1, 10), (2, 20), (3, 30)],
            // k = 1 has a row that differs in `v` and one that does not; k = 2 has
            // only the one that does; k = 3 has none at all.
            &[(Some(1), 10), (Some(1), 99), (Some(2), 20)],
        )?;
        let rows = same_decorrelated_or_not(
            &engine,
            "SELECT k FROM o WHERE EXISTS (SELECT 1 FROM i WHERE i.k = o.k AND i.v <> o.v) \
             ORDER BY k",
        );
        assert_eq!(rows, vec![vec![Value::Int4(1)]]);

        let rows = same_decorrelated_or_not(
            &engine,
            "SELECT k FROM o WHERE NOT EXISTS (SELECT 1 FROM i WHERE i.k = o.k AND i.v <> o.v) \
             ORDER BY k",
        );
        assert_eq!(rows, vec![vec![Value::Int4(2)], vec![Value::Int4(3)]]);
        Ok(())
    }

    /// The complement, including the case an anti join has to get right on its
    /// own: an outer row whose key is NULL matches nothing, so `NOT EXISTS` holds
    /// for it. `o.v` is the key here precisely so one of them is NULL.
    #[test]
    fn a_decorrelated_not_exists_keeps_the_unmatched_rows() -> anyhow::Result<()> {
        let engine = exists_engine(&[(1, 10), (2, 20), (3, 30)], &[(Some(10), 1), (None, 2)])?;
        let rows = same_decorrelated_or_not(
            &engine,
            "SELECT k FROM o WHERE NOT EXISTS (SELECT 1 FROM i WHERE i.k = o.v AND i.v > 0) \
             ORDER BY k",
        );
        assert_eq!(rows, vec![vec![Value::Int4(2)], vec![Value::Int4(3)]]);
        Ok(())
    }

    /// `IN` puts the needle in the join condition beside the correlation key.
    /// A NULL among the candidates changes nothing under a `WHERE`: the row is
    /// dropped either way, which is what makes the semi join sound here (and
    /// `NOT IN`, which is not, keeps the per-row path).
    #[test]
    fn a_decorrelated_in_matches_the_per_row_answer() -> anyhow::Result<()> {
        let engine = exists_engine(
            &[(1, 1), (2, 5), (3, 7)],
            &[(Some(1), 1), (Some(2), 9), (Some(3), 7), (None, 7)],
        )?;
        let rows = same_decorrelated_or_not(
            &engine,
            "SELECT k FROM o WHERE o.v IN (SELECT i.v FROM i WHERE i.k = o.k) ORDER BY k",
        );
        assert_eq!(rows, vec![vec![Value::Int4(1)], vec![Value::Int4(3)]]);
        Ok(())
    }

    /// A scalar aggregate becomes a grouped left join, and the outer rows with no
    /// group must come back with the value the aggregate has over no rows: NULL
    /// for `avg`, which the join's own miss supplies.
    #[test]
    fn a_decorrelated_average_is_null_where_the_group_is_empty() -> anyhow::Result<()> {
        let engine = exists_engine(
            &[(1, 0), (2, 0), (4, 0)],
            &[(Some(1), 2), (Some(1), 4), (Some(2), 5)],
        )?;
        let rows = same_decorrelated_or_not(
            &engine,
            "SELECT k, (SELECT avg(i.v) FROM i WHERE i.k = o.k) FROM o ORDER BY k",
        );
        assert_eq!(
            rows,
            vec![
                vec![
                    Value::Int4(1),
                    Value::Numeric(test_ok(crabgresql_types::Numeric::parse(
                        "3.0000000000000000"
                    )))
                ],
                vec![
                    Value::Int4(2),
                    Value::Numeric(test_ok(crabgresql_types::Numeric::parse(
                        "5.0000000000000000"
                    )))
                ],
                vec![Value::Int4(4), Value::Null],
            ]
        );
        Ok(())
    }

    /// The count bug: `count` over an empty group is 0, while the left join
    /// answers a missing group with NULL. `k = 4` matches nothing and must still
    /// count 0 — both as a value and through the comparison that reads it.
    #[test]
    fn a_decorrelated_count_answers_zero_for_a_missing_group() -> anyhow::Result<()> {
        let engine = exists_engine(&[(1, 0), (4, 0)], &[(Some(1), 2), (Some(1), 4)])?;
        let rows = same_decorrelated_or_not(
            &engine,
            "SELECT k, (SELECT count(*) FROM i WHERE i.k = o.k) FROM o ORDER BY k",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int8(2)],
                vec![Value::Int4(4), Value::Int8(0)],
            ]
        );

        let rows = same_decorrelated_or_not(
            &engine,
            "SELECT k FROM o WHERE (SELECT count(*) FROM i WHERE i.k = o.k) = 0 ORDER BY k",
        );
        assert_eq!(rows, vec![vec![Value::Int4(4)]]);
        Ok(())
    }

    /// The TPC-H Q17 shape: the aggregate arrives wrapped in arithmetic. The
    /// wrapper rides along into the arm, and a missing group still reads NULL
    /// because multiplication is strict.
    #[test]
    fn an_aggregate_under_arithmetic_still_decorrelates() -> anyhow::Result<()> {
        let engine = exists_engine(&[(1, 3), (4, 0)], &[(Some(1), 10), (Some(1), 20)])?;
        let rows = same_decorrelated_or_not(
            &engine,
            "SELECT k FROM o WHERE o.v < (SELECT avg(i.v) * 2 FROM i WHERE i.k = o.k) ORDER BY k",
        );
        assert_eq!(
            rows,
            vec![vec![Value::Int4(1)]],
            "k = 4 compares against NULL"
        );
        Ok(())
    }

    /// Several markers on one node, of both kinds, each becoming its own arm —
    /// and each arm's columns landing past the previous one's.
    #[test]
    fn several_markers_on_one_node_all_decorrelate() -> anyhow::Result<()> {
        let engine = exists_engine(
            &[(1, 1), (2, 9), (3, 0)],
            &[(Some(1), 5), (Some(2), 5), (Some(1), 7)],
        )?;
        let rows = same_decorrelated_or_not(
            &engine,
            "SELECT k FROM o \
             WHERE EXISTS (SELECT 1 FROM i WHERE i.k = o.k) \
               AND o.v < (SELECT sum(i.v) FROM i WHERE i.k = o.k) \
             ORDER BY k",
        );
        // Each marker excludes a different row: `k = 3` has no inner row at all,
        // and `k = 2`'s group sums to 5, which its `v = 9` is not below.
        assert_eq!(rows, vec![vec![Value::Int4(1)]]);
        Ok(())
    }

    /// `IN` over a float8 column and a numeric one compares as float8, but each
    /// candidate is still a numeric: the per-row path casts them as it reaches
    /// them, so the binder left no cast in the comparison itself. The join
    /// condition has to carry that cast, since a hash key is hashed *as* the type
    /// it declares — without it the build side hashes a numeric as a float8 and
    /// the server panics rather than merely answering wrongly.
    #[test]
    fn a_candidate_the_per_row_path_casts_is_cast_in_the_join_too() -> anyhow::Result<()> {
        let engine = crabgresql_pg_engine::ephemeral_engine();
        let floats = engine.create_table(TableSchema::in_namespace(
            "floats",
            "public",
            vec![Column::new("f", PgType::Float8)],
        ))?;
        let numerics = engine.create_table(TableSchema::in_namespace(
            "numerics",
            "public",
            vec![Column::new("n", PgType::Numeric)],
        ))?;
        let txn = wtxn();
        for f in [1.0_f64, 2.0, 3.0] {
            floats.insert(vec![Value::Float8(f)], &txn)?;
        }
        for n in ["1", "1.000000000000000000001", "2"] {
            numerics.insert(
                vec![Value::Numeric(test_ok(crabgresql_types::Numeric::parse(n)))],
                &txn,
            )?;
        }
        let engine: Arc<dyn TableEngine> = engine;
        let rows = same_decorrelated_or_not(
            &engine,
            "SELECT f FROM floats WHERE f IN (SELECT n FROM numerics) ORDER BY f",
        );
        assert_eq!(
            rows,
            vec![vec![Value::Float8(1.0)], vec![Value::Float8(2.0)]]
        );
        Ok(())
    }

    /// A correlation key whose *inner* side holds a subquery of its own. The
    /// conjunct cannot move into a join condition: the walk that rebases column
    /// indices stops at a marker's body, so the body's own level-1 references
    /// would keep meaning "one level out" while the level they counted from moved
    /// away — they would then read the enclosing query's row instead of the one
    /// they were written against, and the columns they need would not be
    /// projected by the arm at all.
    #[test]
    fn a_key_holding_a_subquery_is_not_lifted_into_the_join() -> anyhow::Result<()> {
        // `(select c.v … where c.k = b.k)` is 7 for b.k = 1 and 8 for b.k = 2, so
        // the outer rows keyed 7 and 8 have a match and the one keyed 9 does not.
        let engine = exists_engine(&[(7, 0), (8, 0), (9, 0)], &[(Some(1), 7), (Some(2), 8)])?;
        let rows = same_decorrelated_or_not(
            &engine,
            "SELECT k FROM o WHERE EXISTS ( \
               SELECT 1 FROM i b WHERE (SELECT c.v FROM i c WHERE c.k = b.k LIMIT 1) = o.k) \
             ORDER BY k",
        );
        assert_eq!(rows, vec![vec![Value::Int4(7)], vec![Value::Int4(8)]]);
        Ok(())
    }

    /// A marker the rule refuses still has to answer, and answer the same:
    /// `NOT IN` (three-valued), an `EXISTS` under an `OR`, and a non-aggregate
    /// scalar subquery all keep the per-row path.
    #[test]
    fn the_refused_shapes_still_answer() -> anyhow::Result<()> {
        let engine = exists_engine(&[(1, 1), (2, 2), (3, 3)], &[(Some(1), 1), (Some(2), 2)])?;
        for sql in [
            "SELECT k FROM o WHERE k NOT IN (SELECT i.k FROM i) ORDER BY k",
            "SELECT k FROM o WHERE k = 3 OR EXISTS (SELECT 1 FROM i WHERE i.k = o.k) ORDER BY k",
            "SELECT k FROM o WHERE o.v = (SELECT i.v FROM i WHERE i.k = o.k) ORDER BY k",
            // Always true: an implicit group emits its row even for an outer key
            // nothing matches, so this must not become a semi join on that key.
            "SELECT k FROM o WHERE EXISTS (SELECT count(*) FROM i WHERE i.k = o.k) ORDER BY k",
            // `coalesce` manufactures a value where the aggregate is NULL, so it
            // does not agree with a left join's miss: for `k = 3` the subquery
            // answers -1, while the arm has no row at all.
            "SELECT k FROM o WHERE (SELECT coalesce(max(i.v), -1) FROM i WHERE i.k = o.k) < 0 \
             ORDER BY k",
        ] {
            same_decorrelated_or_not(&engine, sql);
        }
        Ok(())
    }

    #[test]
    fn a_key_type_the_hash_cannot_separate_is_not_memoized() {
        // `point` is outside `hashes_distinctly`, so `hash_key` puts every key
        // in one bucket and `compare_values` — which has no `Point` arm — is
        // asked to resolve it. Without the guard the *second* outer row lands in
        // the first one's bucket and hits `unreachable!`.
        let (_c, rows) = run_rows(
            "SELECT (SELECT count(*) FROM (VALUES (1)) c(x) WHERE c.x = 1 AND o.p IS NOT NULL) \
             FROM (VALUES ('(1,2)'::point), ('(3,4)'::point)) o(p)",
        );
        assert_eq!(rows, vec![vec![Value::Int8(1)], vec![Value::Int8(1)]]);
    }

    #[test]
    fn memoized_scalar_subquery_answers_each_distinct_key_once() -> anyhow::Result<()> {
        // Four outer rows over two distinct correlation keys: the second row of
        // each key is answered from the memo, and must get its own key's answer
        // rather than the previously computed one.
        let engine = exists_engine(
            &[(1, 0), (2, 0), (1, 0), (2, 0)],
            &[(Some(1), 7), (Some(2), 9)],
        )?;
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT (SELECT i.v FROM i WHERE i.k = o.k) FROM o ORDER BY 1",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(7)],
                vec![Value::Int4(7)],
                vec![Value::Int4(9)],
                vec![Value::Int4(9)],
            ]
        );
        Ok(())
    }

    #[test]
    fn memo_key_covers_every_outer_column_the_subplan_reads() -> anyhow::Result<()> {
        // Two outer rows share `k` but differ in `v`, and the subplan reads
        // both. Keying on only one of them would serve the first row's answer
        // to the second.
        let engine = exists_engine(&[(1, 7), (1, 9)], &[(Some(1), 7), (Some(1), 9)])?;
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT o.v, (SELECT count(*) FROM i WHERE i.k = o.k AND i.v = o.v) FROM o ORDER BY o.v",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(7), Value::Int8(1)],
                vec![Value::Int4(9), Value::Int8(1)],
            ]
        );
        Ok(())
    }

    #[test]
    fn correlated_exists_on_an_inequality_still_answers_per_row() -> anyhow::Result<()> {
        // `i.k < o.k` is not an equality, so nothing is hashable and the marker
        // falls back to the per-outer-row path. The answer has to be the same.
        let engine = exists_engine(&[(1, 10), (5, 50)], &[(Some(3), 1)])?;
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT k FROM o WHERE EXISTS (SELECT 1 FROM i WHERE i.k < o.k) ORDER BY k",
        );
        assert_eq!(rows, vec![vec![Value::Int4(5)]]);
        Ok(())
    }

    #[test]
    fn explicit_on_correlated_residual_uses_the_join_row() {
        // The correlated scalar subquery reads `a.big` from the full joined row.
        // Sinking the residual onto b would substitute its outer reference from
        // b's two-column row and read `b.big` instead, returning no rows.
        let (_, rows) = run_rows(
            "SELECT a.id, b.big \
             FROM (VALUES (1, 10), (2, 20)) a(id, big) \
             JOIN (VALUES (1, 100), (2, 200), (3, 300)) b(id, big) \
               ON a.id = b.id \
              AND b.big = (SELECT max(c.v) \
                           FROM (VALUES (10, 100), (20, 200)) c(k, v) \
                           WHERE c.k = a.big) \
             ORDER BY a.id",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int4(100)],
                vec![Value::Int4(2), Value::Int4(200)],
            ]
        );
    }

    #[test]
    fn hash_join_left_outer_with_residual_null_extends() {
        // A LEFT join whose ON has a residual: a left row is null-extended when no
        // right row satisfies the *whole* ON (key equality AND the residual).
        let (_columns, rows) = run_rows(
            "SELECT a.x, b.v \
             FROM (VALUES (1, 5), (2, 9)) a(x, v) \
             LEFT JOIN (VALUES (1, 9), (2, 1)) b(x, v) ON a.x = b.x AND a.v < b.v \
             ORDER BY a.x",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int4(9)],
                // x=2 has a key match (b.x=2) but 9 < 1 is false, so null-extended.
                vec![Value::Int4(2), Value::Null],
            ]
        );
    }

    #[test]
    fn comma_join_where_equality_matches_the_explicit_on_form() {
        // The planner now extracts `a.x = b.y` from the WHERE into the join, so
        // this runs as a hash join. Rows — and their order — must be exactly what
        // the explicit ON form produces, duplicates included.
        let comma = "SELECT a.x, b.y \
                     FROM (VALUES (1), (2), (2)) a(x), (VALUES (2), (2), (3)) b(y) \
                     WHERE a.x = b.y";
        let explicit = "SELECT a.x, b.y \
                        FROM (VALUES (1), (2), (2)) a(x) \
                        JOIN (VALUES (2), (2), (3)) b(y) ON a.x = b.y";
        let (_, comma_rows) = run_rows(comma);
        let (_, explicit_rows) = run_rows(explicit);
        assert_eq!(comma_rows, explicit_rows);
        assert_eq!(
            comma_rows,
            vec![
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(2), Value::Int4(2)],
            ]
        );
    }

    #[test]
    fn comma_join_null_keys_never_match() {
        // NULL = NULL is unknown, so a NULL key joins nothing — the hash join
        // excludes it at build and probe time, matching the nested loop it replaced.
        let (_, rows) = run_rows(
            "SELECT a.x, b.y \
             FROM (VALUES (1), (NULL)) a(x), (VALUES (1), (NULL)) b(y) \
             WHERE a.x = b.y",
        );
        assert_eq!(rows, vec![vec![Value::Int4(1), Value::Int4(1)]]);
    }

    #[test]
    fn comma_join_non_equi_condition_still_filters() {
        // No hash key here, so the condition rides the nested loop as the node's
        // predicate on a join whose kind flipped from Cross to Inner. A node left
        // as Cross would return the whole product instead.
        let (_, rows) = run_rows(
            "SELECT a.x, b.y \
             FROM (VALUES (1), (5)) a(x), (VALUES (2), (9)) b(y) \
             WHERE a.x < b.y",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int4(2)],
                vec![Value::Int4(1), Value::Int4(9)],
                vec![Value::Int4(5), Value::Int4(9)],
            ]
        );
    }

    #[test]
    fn three_way_comma_join_matches_the_explicit_on_form() {
        let comma = "SELECT a.x, b.y, c.z \
                     FROM (VALUES (1), (2)) a(x), (VALUES (1), (2)) b(y), (VALUES (2), (3)) c(z) \
                     WHERE a.x = b.y AND b.y = c.z";
        let explicit = "SELECT a.x, b.y, c.z \
                        FROM (VALUES (1), (2)) a(x) \
                        JOIN (VALUES (1), (2)) b(y) ON a.x = b.y \
                        JOIN (VALUES (2), (3)) c(z) ON b.y = c.z";
        let (_, comma_rows) = run_rows(comma);
        let (_, explicit_rows) = run_rows(explicit);
        assert_eq!(comma_rows, explicit_rows);
        assert_eq!(
            comma_rows,
            vec![vec![Value::Int4(2), Value::Int4(2), Value::Int4(2)]]
        );
    }

    #[test]
    fn leaf_filters_restrict_both_sides_of_a_comma_join() {
        // `a.x > 1` and `b.k < 30` sink to their own scans, leaving only the
        // equality on the join. The surviving rows must be exactly what the
        // unrestricted join filtered afterwards would have produced.
        let (_, rows) = run_rows(
            "SELECT a.x, b.k \
             FROM (VALUES (1, 10), (2, 20), (3, 30)) a(x, k), \
                  (VALUES (1, 10), (2, 20), (3, 30)) b(x, k) \
             WHERE a.x = b.x AND a.x > 1 AND b.k < 30 ORDER BY a.x",
        );
        assert_eq!(rows, vec![vec![Value::Int4(2), Value::Int4(20)]]);
    }

    #[test]
    fn anti_join_idiom_survives_pushdown() {
        // `b.y IS NULL` reads the null-supplying side of a LEFT join. Pushing it
        // below the join would drop the b rows an a row matched, null-extend that
        // a row, and let it pass — so this must keep returning only the genuinely
        // unmatched left rows.
        let (_, rows) = run_rows(
            "SELECT a.x \
             FROM (VALUES (1), (2), (3)) a(x) \
             LEFT JOIN (VALUES (2)) b(y) ON a.x = b.y \
             WHERE b.y IS NULL ORDER BY a.x",
        );
        assert_eq!(rows, vec![vec![Value::Int4(1)], vec![Value::Int4(3)]]);
    }

    #[test]
    fn comma_join_with_an_outer_join_group_keeps_outer_semantics() {
        // A bushy FROM: the LEFT join's null-extended row must survive into the
        // cross join, and the WHERE conjunct over the preserved side must not
        // change which rows the LEFT join emits.
        let (_, rows) = run_rows(
            "SELECT a.x, b.y, c.z \
             FROM (VALUES (1), (2)) a(x) LEFT JOIN (VALUES (2)) b(y) ON a.x = b.y, \
                  (VALUES (7)) c(z) \
             WHERE a.x = 1 ORDER BY a.x",
        );
        assert_eq!(
            rows,
            vec![vec![Value::Int4(1), Value::Null, Value::Int4(7)]]
        );
    }

    #[test]
    fn equi_join_on_money_matches_correctly() {
        // money hashes distinctly now, so this runs as a hash join.
        let (_columns, rows) = run_rows(
            "SELECT a.x, b.y \
             FROM (VALUES ('$1.00'::money), ('$2.00'::money)) a(x) \
             JOIN (VALUES ('$2.00'::money), ('$3.00'::money)) b(y) ON a.x = b.y",
        );
        assert_eq!(rows, vec![vec![Value::Money(200), Value::Money(200)]]);
    }

    #[test]
    fn equi_join_on_interval_matches_via_nested_loop_fallback() {
        // interval is not hash-distinct, so the planner keeps this as a nested
        // loop; the result must still be correct (and NULL keys still excluded).
        let (_columns, rows) = run_rows(
            "SELECT a.x, b.y \
             FROM (VALUES ('1 day'::interval), ('24 hours'::interval), (NULL)) a(x) \
             JOIN (VALUES ('1 day'::interval), ('2 days'::interval)) b(y) ON a.x = b.y \
             ORDER BY a.x",
        );
        // '1 day' and '24 hours' are equal intervals, so both non-null left rows
        // match the single '1 day' right row; the NULL left key matches nothing.
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| !matches!(r[1], Value::Null)));
    }

    #[test]
    fn hash_join_coerces_mixed_width_keys() {
        // int4 = int8 promotes the int4 side to int8; both sides must hash under
        // the same promoted type for the match to be found.
        let engine = engine_with_nums();
        let (_columns, rows) = run_rows_on(
            &engine,
            "SELECT a.n, b.big FROM nums a \
             JOIN (VALUES (1::int8), (3::int8)) b(big) ON a.n = b.big ORDER BY a.n",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int8(1)],
                vec![Value::Int4(3), Value::Int8(3)],
            ]
        );
    }
}
