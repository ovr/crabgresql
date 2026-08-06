//! Simple-query execution: AST → bind → plan → Volcano executor.
//!
//! DQL/DML statements run through the binder/planner pipeline. DDL
//! (CREATE TABLE) and session commands (SET) execute directly here until the
//! catalog and GUC store exist.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};

use crabgresql_binder::{
    BoundExpr, CopyFromPlan, CopyFromSource, InsertSource, LogicalPlan, bind_copy_from,
    bind_delete_with_params, bind_insert_with_params, bind_query, bind_query_with_params,
    bind_update_with_params, output_columns_of, param_ctx_extended, param_ctx_none, param_types,
    require_all_resolved, substitute_params,
};
use crabgresql_executor::{
    CatalogOps, DmlVerb, ExecContext, ExecNode, Execution, MaterializedRows, OutputColumn,
    RoutineOps, execute,
};
use crabgresql_parser::ast;
use crabgresql_pg_wire::{ErrorFields, TransactionStatus, sqlstate};
use crabgresql_storage_api::{
    CheckConstraint, Column, IndexConstraint, IndexKey, IndexMetadata, IndexMethod, InheritParent,
    PartitionBound, PartitionBoundDatum, PartitionOf, PartitionScheme, PartitionStrategy,
    RelPersistence, RelationMetadata, RoutineKind as ApiRoutineKind, RoutineSig,
    SequenceDefinition, StorageError, TableAccessMethod, TableAm, TableEngine, TableSchema,
    TypeCatalog, ViewDefinition,
};
use crabgresql_txn::{CommandId, IsolationLevel, TransactionManager, TxnContext, Xid};
use crabgresql_types::{FmtCtx, PgType, Value};

use crate::catalog::{SessionCatalog, SessionCatalogOps, SessionCatalogSource};
use crate::error::PgError;
use crate::explain::{ExplainOptions, explain_columns, explain_result, run_analyze};
use crate::global_catalog::{
    ArgMode, CatalogNotice, FuncBody, FuncDropSpec, FuncInfo, GlobalCatalog, RoutineArg,
    RoutineDefinition, RoutineKind, TypeRef, Volatility,
};
use crate::guc;
use crate::routines::{RoutineDispatch, SessionNotices};
use crate::session::{ActiveTxn, Session};

/// Severity of a non-error message sent before a command's CommandComplete.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NoticeSeverity {
    /// A true `NOTICE` (e.g. "drop cascades to N other objects").
    Notice,
    /// A `WARNING` (e.g. "there is no transaction in progress").
    Warning,
}

/// A non-error message sent before a command's CommandComplete.
pub struct Notice {
    pub severity: NoticeSeverity,
    pub code: Cow<'static, str>,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    /// 1-based (line, column) of the token this NOTICE points at, when PG
    /// renders a `LINE n:` cursor excerpt. Converted to a wire character offset
    /// when the NOTICE is sent.
    pub location: Option<(u64, u64)>,
    /// The `CONTEXT:` traceback, innermost frame first — PG attaches one to a
    /// `RAISE NOTICE` inside a routine body just as it does to an error.
    pub context: Vec<String>,
}

impl Notice {
    /// A `WARNING`-severity message (no DETAIL), as used by transaction control.
    fn warning(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: NoticeSeverity::Warning,
            code: Cow::Borrowed(code),
            message: message.into(),
            detail: None,
            hint: None,
            location: None,
            context: Vec::new(),
        }
    }

    /// A `NOTICE`-severity message with an optional DETAIL line.
    #[allow(clippy::self_named_constructors)]
    fn notice(message: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            severity: NoticeSeverity::Notice,
            // psql does not print the SQLSTATE of a NOTICE; PG uses
            // successful_completion (00000) for these.
            code: Cow::Borrowed("00000"),
            message: message.into(),
            detail,
            hint: None,
            location: None,
            context: Vec::new(),
        }
    }

    /// The wire fields for this notice. `position` is the cursor offset the
    /// caller resolved from [`Notice::location`]; only the caller holds the SQL
    /// text needed to convert (line, column) into a character offset.
    pub fn to_fields(&self, position: Option<usize>) -> ErrorFields {
        let mut fields = match self.severity {
            NoticeSeverity::Notice => ErrorFields::notice(&self.code, &self.message),
            NoticeSeverity::Warning => ErrorFields::warning(&self.code, &self.message),
        };
        if let Some(detail) = &self.detail {
            fields = fields.with_detail(detail);
        }
        if let Some(hint) = &self.hint {
            fields = fields.with_hint(hint);
        }
        if let Some(position) = position {
            fields = fields.with_position(position);
        }
        if !self.context.is_empty() {
            fields = fields.with_context(&self.context.join("\n"));
        }
        fields
    }
}

pub enum QueryResult {
    /// A result set, streamed: the caller pulls tuples from the node and derives
    /// the CommandComplete tag from `tag` and the row count.
    Rows {
        columns: Vec<OutputColumn>,
        node: Box<dyn ExecNode>,
        /// Which command tag to report once the rows are drained. A plain SELECT
        /// uses `SELECT n`, EXPLAIN the bare `EXPLAIN`, and a `RETURNING` DML keeps
        /// its mutation tag (`INSERT 0 n` / `UPDATE n` / `DELETE n`).
        tag: RowTag,
        /// Diagnostics raised while producing the rows — a `RAISE NOTICE` from
        /// a routine body. Emitted before the RowDescription.
        ///
        /// They can be collected up front because a plan that calls a routine
        /// is materialized before this is built (see `materialize`); a truly
        /// streamed result set would need them interleaved with the rows, which
        /// is what the session-owned buffer they come from is shaped for.
        notices: Vec<Notice>,
    },
    Command {
        tag: String,
        /// Warnings to emit before the CommandComplete, in order.
        notices: Vec<Notice>,
    },
}

/// The CommandComplete tag family for a streamed result set. The row count is
/// filled in once the rows are drained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowTag {
    Select,
    Insert,
    Update,
    Delete,
    /// `EXPLAIN`, whose tag carries no count — the plan's line count is not a row
    /// count, and a driver that reads the trailing integer would report it as one.
    Explain,
    /// `FETCH`, whose count is the rows this fetch returned — not the cursor's
    /// size and not its position.
    Fetch,
    /// `SHOW`, whose tag carries no count — as with `EXPLAIN`, a driver reading
    /// a trailing integer would report it as a row count.
    Show,
}

impl RowTag {
    /// The CommandComplete tag for `count` streamed rows.
    pub fn complete(self, count: usize) -> String {
        match self {
            RowTag::Select => format!("SELECT {count}"),
            RowTag::Insert => format!("INSERT 0 {count}"),
            RowTag::Update => format!("UPDATE {count}"),
            RowTag::Delete => format!("DELETE {count}"),
            RowTag::Explain => "EXPLAIN".to_string(),
            RowTag::Fetch => format!("FETCH {count}"),
            RowTag::Show => "SHOW".to_string(),
        }
    }
}

impl From<DmlVerb> for RowTag {
    fn from(verb: DmlVerb) -> Self {
        match verb {
            DmlVerb::Insert => RowTag::Insert,
            DmlVerb::Update => RowTag::Update,
            DmlVerb::Delete => RowTag::Delete,
        }
    }
}

impl QueryResult {
    /// A command result with no accompanying warnings.
    fn command(tag: impl Into<String>) -> Self {
        QueryResult::Command {
            tag: tag.into(),
            notices: Vec::new(),
        }
    }

    /// Put `notices` ahead of any this result already carries.
    ///
    /// The connection layer folds in whatever was still buffered when the
    /// statement returned. Prepending rather than appending keeps them in the
    /// order they were raised: anything left in the buffer was raised during
    /// execution, before the handler attached its own.
    pub fn prepend_notices(&mut self, mut notices: Vec<Notice>) {
        if notices.is_empty() {
            return;
        }
        let own = match self {
            QueryResult::Rows { notices, .. } | QueryResult::Command { notices, .. } => notices,
        };
        notices.append(own);
        *own = notices;
    }
}

/// Parameters supplied to a statement. A simple (`Q`) query passes
/// [`BoundParams::none`]; the extended protocol passes the types resolved at
/// `Parse` and the values decoded at `Bind`.
pub struct BoundParams {
    /// Resolved type per `$n` placeholder (index = n − 1). Empty for a simple
    /// query.
    pub types: Vec<PgType>,
    /// Decoded value per `$n`, parallel to `types`.
    pub values: Vec<Value>,
    /// Whether `$n` placeholders are permitted. A simple query sets this false,
    /// so a stray `$n` is rejected with `42P02` rather than inferred.
    pub extended: bool,
}

impl BoundParams {
    /// No parameters, and none permitted — the simple-query default.
    pub fn none() -> Self {
        Self {
            types: Vec::new(),
            values: Vec::new(),
            extended: false,
        }
    }
}

/// What `Parse`/`Describe` needs about a statement without executing it: the
/// resolved parameter types (for `ParameterDescription`) and the result column
/// shape (`None` = `NoData`: a utility or data-modifying statement).
pub struct Analyzed {
    pub param_types: Vec<PgType>,
    pub result_columns: Option<Vec<OutputColumn>>,
}

/// The per-session relation-visibility rule, in one place: temp tables from every
/// session share the one engine (each under its own `pg_temp_N` namespace,
/// persistence `Temporary`), so a session may see only the **permanent** relations
/// plus the **temp** relations in ITS OWN `temp_schema` — never another session's.
/// Splits an engine-wide `relation_metadata()` snapshot into `(permanent, own_temp)`;
/// foreign temp relations fall out of both. Used by the catalog reflection and by
/// `execute_drop_index` so the rule is not re-derived per call site.
pub(crate) fn partition_session_relations(
    all: Vec<RelationMetadata>,
    temp_schema: &str,
) -> (Vec<RelationMetadata>, Vec<RelationMetadata>) {
    let mut permanent = Vec::new();
    let mut own_temp = Vec::new();
    for m in all {
        if m.schema.persistence == RelPersistence::Temporary {
            if m.schema.namespace == temp_schema {
                own_temp.push(m);
            }
            // A foreign session's temp relation is invisible here.
        } else {
            permanent.push(m);
        }
    }
    (permanent, own_temp)
}

/// Build this session's three catalog views: the `pg_temp`-first relation
/// overlay for the binder (temp shadows permanent, both behind the read-only
/// `pg_catalog`), the user-type/cast view, and the executor's handle for the
/// catalog functions. Shared by `execute_statement` and `analyze_statement` so
/// binding sees identical name resolution either way.
///
/// The third view wraps the *same* `SystemCatalog` the first is built from, so
/// `pg_table_is_visible(c.oid)` resolves the very OIDs the `pg_class` scan in
/// the same statement emitted.
fn bind_catalogs(
    engine: &Arc<dyn TableEngine>,
    global_catalog: &Arc<GlobalCatalog>,
    session: &Session,
) -> (
    Arc<dyn TableEngine>,
    Arc<dyn TypeCatalog>,
    Arc<dyn CatalogOps>,
) {
    // The read-only system catalog (`pg_catalog`), rebuilt per statement so its
    // rows reflect current server state: live user relations (permanent + this
    // session's temp tables) are reflected into pg_class/pg_attribute. The
    // relation enumeration is lazy — only a query that actually opens
    // pg_class/pg_attribute pays for it. It sits behind temp + global on the
    // search path.
    let system: Arc<crabgresql_catalog::SystemCatalog> =
        Arc::new(crabgresql_catalog::SystemCatalog::from_source(Arc::new(
            SessionCatalogSource::new(engine.clone(), global_catalog.clone(), session),
        )));
    let catalog: Arc<dyn TableEngine> = Arc::new(SessionCatalog::new(
        engine.clone(),
        system.clone(),
        session.temp_schema.clone(),
    ));
    // The global catalog is the binder's view of user-defined types and casts,
    // so an expression can cast to/from a `CREATE TYPE` name.
    let type_catalog: Arc<dyn TypeCatalog> = global_catalog.clone();
    let catalog_ops: Arc<dyn CatalogOps> = Arc::new(
        SessionCatalogOps::new(system, session.temp_schema.clone())
            .with_relations(Arc::clone(&catalog)),
    );
    (catalog, type_catalog, catalog_ops)
}

/// A catalog routine as `pg_proc` reports it.
///
/// A type that does not resolve is reported as OID 0 rather than dropping the
/// row: `pg_proc` should show every routine that exists, and PostgreSQL also
/// prints 0 for a type it cannot name.
pub(crate) fn catalog_routine(info: &FuncInfo) -> crabgresql_catalog::CatalogRoutine {
    let oid_of = |r: &TypeRef| match r {
        TypeRef::Builtin(t) => t.oid(),
        TypeRef::User(_) | TypeRef::Cstring => 0,
    };
    // PostgreSQL leaves proallargtypes/proargmodes NULL unless some argument is
    // OUT or INOUT, and proargnames NULL unless some argument is named.
    let has_output = info.all_args.iter().any(|a| !a.mode.is_input());
    let named = info.all_args.iter().any(|a| a.name.is_some());
    crabgresql_catalog::CatalogRoutine {
        oid: info.oid,
        name: info.name.clone(),
        namespace: info.namespace.clone(),
        kind: info.kind.prokind(),
        lang: info.lang_oid,
        arg_types: info.args.iter().map(oid_of).collect(),
        all_arg_types: if has_output {
            info.all_args.iter().map(|a| oid_of(&a.ty)).collect()
        } else {
            Vec::new()
        },
        arg_modes: if has_output {
            info.all_args.iter().map(|a| a.mode.proargmode()).collect()
        } else {
            Vec::new()
        },
        arg_names: if named {
            info.all_args
                .iter()
                .map(|a| a.name.clone().unwrap_or_default())
                .collect()
        } else {
            Vec::new()
        },
        ret_type: oid_of(&info.ret),
        // SETOF is rejected at CREATE, so nothing here returns a set yet.
        retset: false,
        volatile: info.volatility.provolatile(),
        strict: info.strict,
        secdef: info.secdef,
        src: info.src.clone(),
    }
}

/// Bind a DQL/DML statement against `catalog`, threading `$n` placeholders
/// through `params`. Returns the resolved logical plan with every `Param`
/// already replaced by its bound value (so downstream planning/execution is
/// parameter-free); a non-DQL/DML statement returns `Ok(None)` for the caller
/// to handle inline. `require_params` gates whether all placeholders must have
/// resolved (extended protocol) — a simple query has none.
fn bind_dml_with_params(
    catalog: &Arc<dyn TableEngine>,
    type_catalog: &Arc<dyn TypeCatalog>,
    stmt: &ast::Statement,
    params: &BoundParams,
) -> Result<Option<LogicalPlan>, PgError> {
    let ctx = if params.extended {
        param_ctx_extended(params.types.iter().map(|t| Some(*t)).collect())
    } else {
        param_ctx_none()
    };
    let logical = match stmt {
        ast::Statement::Query(query) => bind_query_with_params(catalog, type_catalog, query, &ctx)?,
        ast::Statement::Insert(insert) => {
            bind_insert_with_params(catalog, type_catalog, insert, &ctx)?
        }
        ast::Statement::Update(update) => {
            bind_update_with_params(catalog, type_catalog, update, &ctx)?
        }
        ast::Statement::Delete(delete) => {
            bind_delete_with_params(catalog, type_catalog, delete, &ctx)?
        }
        _ => return Ok(None),
    };
    if params.extended {
        // Every placeholder must have received a type; then fold the bound
        // values in so the plan no longer mentions parameters.
        require_all_resolved(&ctx)?;
        let mut logical = logical;
        substitute_params(&mut logical, &params.values);
        return Ok(Some(logical));
    }
    Ok(Some(logical))
}

/// Analyze a statement for `Parse`/`Describe`: resolve its parameter types and
/// result-column shape without executing. `declared` holds the parameter type
/// OIDs a `Parse` message supplied, mapped to types (`None` = infer).
pub fn analyze_statement(
    engine: &Arc<dyn TableEngine>,
    global_catalog: &Arc<GlobalCatalog>,
    stmt: &ast::Statement,
    declared: Vec<Option<PgType>>,
    session: &Session,
) -> Result<Analyzed, PgError> {
    // Describe binds but never executes, so the catalog handle is dropped here;
    // Execute re-enters `execute_statement`, which builds a fresh one.
    let (catalog, type_catalog, _) = bind_catalogs(engine, global_catalog, session);
    let ctx = param_ctx_extended(declared);
    let logical = match stmt {
        ast::Statement::Query(query) => {
            bind_query_with_params(&catalog, &type_catalog, query, &ctx)?
        }
        ast::Statement::Insert(insert) => {
            bind_insert_with_params(&catalog, &type_catalog, insert, &ctx)?
        }
        ast::Statement::Update(update) => {
            bind_update_with_params(&catalog, &type_catalog, update, &ctx)?
        }
        ast::Statement::Delete(delete) => {
            bind_delete_with_params(&catalog, &type_catalog, delete, &ctx)?
        }
        // EXPLAIN resolves its parameters against the inner statement and always
        // returns a single "QUERY PLAN" text column. Binding the inner here lets
        // a prepared `EXPLAIN … $1` report its parameter types at Describe rather
        // than erroring at Execute.
        // The options are deliberately *not* validated here: PG inspects them at
        // Describe only to pick the result column type and raises nothing, leaving
        // every option error to Execute. Rejecting at Parse would abort an open
        // transaction block for a statement that was never executed. A spelling
        // PG's *grammar* rejects is different — that is a parse error there, so it
        // has to fail here too rather than at Execute.
        ast::Statement::Explain {
            query_plan,
            estimate,
            statement,
            format,
            ..
        } => {
            if let Some(modifier) = query_plan
                .then_some("QUERY")
                .or_else(|| estimate.then_some("ESTIMATE"))
                .or_else(|| format.is_some().then_some("FORMAT"))
            {
                return Err(PgError::syntax(format!(
                    "syntax error at or near \"{modifier}\""
                )));
            }
            match statement.as_ref() {
                ast::Statement::Query(q) => {
                    bind_query_with_params(&catalog, &type_catalog, q, &ctx)?
                }
                ast::Statement::Insert(i) => {
                    bind_insert_with_params(&catalog, &type_catalog, i, &ctx)?
                }
                ast::Statement::Update(u) => {
                    bind_update_with_params(&catalog, &type_catalog, u, &ctx)?
                }
                ast::Statement::Delete(d) => {
                    bind_delete_with_params(&catalog, &type_catalog, d, &ctx)?
                }
                // EXPLAIN of a non-DML statement: no parameters, error at Execute.
                _ => {
                    return Ok(Analyzed {
                        param_types: Vec::new(),
                        result_columns: None,
                    });
                }
            };
            require_all_resolved(&ctx)?;
            return Ok(Analyzed {
                param_types: param_types(&ctx).into_iter().flatten().collect(),
                result_columns: Some(explain_columns()),
            });
        }
        // `FETCH` returns whatever its cursor holds, so Describe answers from the
        // open cursor rather than from a plan. An unknown name reports NoData and
        // leaves the 34000 to Execute — Parse must not fail for a cursor the
        // client is about to declare. `Describe` of a *portal* re-runs this
        // rather than reading the prepared statement's cached shape, because the
        // cursor may have been declared (or redeclared) since Parse.
        ast::Statement::Fetch { name, .. } => {
            return Ok(Analyzed {
                param_types: Vec::new(),
                result_columns: fetch_columns(session, name),
            });
        }
        // A cursor body may carry `$n`, and PG resolves those at Bind like any
        // other statement's — so DECLARE reports its parameter types here rather
        // than falling through to the no-parameter utility arm.
        ast::Statement::Declare { stmts } => {
            let Some(query) = declared_cursor_query(stmts) else {
                return Ok(Analyzed {
                    param_types: Vec::new(),
                    result_columns: None,
                });
            };
            bind_query_with_params(&catalog, &type_catalog, query, &ctx)?;
            require_all_resolved(&ctx)?;
            return Ok(Analyzed {
                param_types: param_types(&ctx).into_iter().flatten().collect(),
                // DECLARE itself returns no rows; the cursor's shape is reported
                // by the FETCH that reads it.
                result_columns: None,
            });
        }
        // `SHOW` takes no parameters but *does* return rows, so it needs its own
        // arm: the utility catch-all below would report NoData and then Execute
        // would stream DataRows the client was told not to expect. An
        // unrecognized name reports its shape anyway and leaves the 42704 to
        // Execute, as the `FETCH` arm above does for an unknown cursor.
        ast::Statement::ShowVariable { variable } => {
            return Ok(Analyzed {
                param_types: Vec::new(),
                result_columns: Some(show_columns(variable)),
            });
        }
        // Utility statements (DDL/SET/transaction control) take no parameters and
        // return no rows; their errors surface at Execute, as in PG.
        _ => {
            return Ok(Analyzed {
                param_types: Vec::new(),
                result_columns: None,
            });
        }
    };
    require_all_resolved(&ctx)?;
    // A `Parse` may leave trailing parameters undeclared and unused; PG reports a
    // type for every parameter up to the highest referenced. `param_types` grew
    // to that count during binding, and `require_all_resolved` proved each is
    // `Some`.
    let param_types = param_types(&ctx).into_iter().flatten().collect();
    // A plan with a row shape (a DQL query, or a data-modifying statement with a
    // RETURNING clause) reports its columns; a data-modifying plan without
    // RETURNING makes `output_columns_of` return an error we fold to `None`
    // (NoData).
    let result_columns = output_columns_of(&logical).ok();
    Ok(Analyzed {
        param_types,
        result_columns,
    })
}

/// The result shape a `FETCH` of `name` would produce, or `None` when no such
/// cursor is open (`Describe` then answers `NoData` and leaves the 34000 to
/// Execute).
///
/// Resolved from the live cursor every time it is asked. A cursor can be
/// declared, closed and redeclared with a different shape while a prepared
/// `FETCH` sits unchanged, so an answer cached at Parse would describe a result
/// set that no longer exists.
pub(crate) fn fetch_columns(session: &Session, name: &ast::Ident) -> Option<Vec<OutputColumn>> {
    let name = crate::cursor::cursor_name(name);
    session
        .cursors
        .get(&name)
        .map(|cursor| cursor.columns.clone())
}

/// The body of a single `DECLARE … CURSOR FOR <query>`, or `None` for any other
/// `DECLARE` shape (which this build does not support).
fn declared_cursor_query(stmts: &[ast::Declare]) -> Option<&ast::Query> {
    match stmts {
        [single] if single.declare_type == Some(ast::DeclareType::Cursor) => {
            single.for_query.as_deref()
        }
        _ => None,
    }
}

pub fn execute_statement(
    engine: &Arc<dyn TableEngine>,
    global_catalog: &Arc<GlobalCatalog>,
    txnmgr: &Arc<TransactionManager>,
    stmt: &ast::Statement,
    session: &mut Session,
    params: &BoundParams,
) -> Result<QueryResult, PgError> {
    execute_statement_with(engine, global_catalog, txnmgr, stmt, session, params, false)
}

/// [`execute_statement`], plus the one knob `DECLARE … CURSOR` needs.
///
/// `force_materialize` drains a streamed result set inside the statement's own
/// transaction instead of handing back a live iterator — the treatment a plan
/// that calls a routine already gets. `DECLARE` needs it because the rows
/// outlive the statement that produced them.
pub(crate) fn execute_statement_with(
    engine: &Arc<dyn TableEngine>,
    global_catalog: &Arc<GlobalCatalog>,
    txnmgr: &Arc<TransactionManager>,
    stmt: &ast::Statement,
    session: &mut Session,
    params: &BoundParams,
    force_materialize: bool,
) -> Result<QueryResult, PgError> {
    // In an aborted transaction block, PG rejects everything but COMMIT/ROLLBACK
    // until the block ends.
    if session.tx_status == TransactionStatus::Failed
        && !matches!(
            stmt,
            ast::Statement::Commit { .. } | ast::Statement::Rollback { .. }
        )
    {
        return Err(PgError::new(
            sqlstate::IN_FAILED_SQL_TRANSACTION,
            "current transaction is aborted, commands ignored until end of transaction block",
        ));
    }
    // PG's "SET TRANSACTION ISOLATION LEVEL must be called before any query" rule
    // keys off whether a snapshot-taking statement has run in this block.
    // Transaction control and SET/RESET take no snapshot; every other statement
    // (SELECT, DML, and DDL like CREATE/DROP) does — so mark the block here, at
    // the statement boundary, rather than only on the DML path.
    if let Some(active) = session.xact.as_mut()
        && statement_takes_snapshot(stmt)
    {
        active.has_run_query = true;
    }
    // A read-only transaction rejects data-changing DDL up front, before name
    // resolution: PG reports 25006 for `DROP TABLE missing` rather than the
    // undefined-table error. (DML is checked after binding below, because PG
    // resolves the target relation first for INSERT/UPDATE/DELETE.)
    if read_only_active(session)
        && let Some(command) = read_only_prohibited_ddl(stmt)
    {
        return Err(PgError::new(
            sqlstate::READ_ONLY_SQL_TRANSACTION,
            format!("cannot execute {command} in a read-only transaction"),
        ));
    }
    // `EXPLAIN [ANALYZE] <stmt>`: from here on the inner statement is the one
    // being processed, and `explain` records how to report it. Unwrapping here —
    // after the aborted-block and read-only-DDL checks, before binding — is what
    // lets ANALYZE reach the same transaction machinery as a bare statement: PG
    // really runs it, so a write must take an XID, honor the read-only check, and
    // commit. Unwrapping any earlier would make `BEGIN READ ONLY; EXPLAIN CREATE
    // TABLE t(…)` report 25006 instead of EXPLAIN's own unsupported-statement
    // error.
    // The modifiers are carried unresolved past the binder on purpose: PG
    // parse-analyzes the inner statement *before* it reads the option list, so
    // `EXPLAIN (BOGUS) SELECT * FROM missing` reports the missing relation, not the
    // bad option. Grammar errors are the exception — those precede name resolution
    // in PG too, so they are raised here.
    let (stmt, explain_options) = match stmt {
        ast::Statement::Explain {
            analyze,
            verbose,
            query_plan,
            estimate,
            statement,
            format,
            options,
            ..
        } => {
            // `EXPLAIN QUERY PLAN` (SQLite), `EXPLAIN ESTIMATE` (ClickHouse) and the
            // bare `EXPLAIN FORMAT <kind>` are other dialects' spellings that the
            // shared parser accepts. PG's grammar has none of them, so neither does
            // crabgresql — silently treating them as a plain EXPLAIN would answer a
            // question that was not asked. PG echoes the offending token as the
            // client wrote it and adds a cursor position; the parser keeps neither
            // for these flags, so the keyword is reported in upper case without a
            // caret.
            if let Some(modifier) = query_plan
                .then_some("QUERY")
                .or_else(|| estimate.then_some("ESTIMATE"))
                .or_else(|| format.is_some().then_some("FORMAT"))
            {
                return Err(PgError::syntax(format!(
                    "syntax error at or near \"{modifier}\""
                )));
            }
            (
                statement.as_ref(),
                Some((*analyze, *verbose, options.as_deref())),
            )
        }
        other => (other, None),
    };
    // Resolution overlay: the session's temp catalog shadows the shared global
    // engine (PG's `pg_temp`-first search). CREATE routes temp vs global itself,
    // so it keeps the raw engine + session below.
    let (catalog, type_catalog, catalog_ops) = bind_catalogs(engine, global_catalog, session);
    let logical = match bind_dml_with_params(&catalog, &type_catalog, stmt, params)? {
        Some(logical) => logical,
        // EXPLAIN of a utility statement: report the gap rather than falling
        // through to the handler, which would run the utility for real.
        None if explain_options.is_some() => {
            return Err(PgError::feature_not_supported(format!(
                "EXPLAIN of {} is not supported yet",
                statement_kind(stmt)
            )));
        }
        // Not DQL/DML: fall through to the utility-statement handlers below.
        None => match stmt {
            ast::Statement::CreateTable(create) if create.query.is_some() => {
                return execute_create_table_as(
                    engine,
                    &catalog,
                    &type_catalog,
                    &catalog_ops,
                    global_catalog,
                    txnmgr,
                    create,
                    session,
                );
            }
            ast::Statement::CreateTable(create) => {
                return execute_create_table(engine, &type_catalog, &catalog_ops, create, session);
            }
            ast::Statement::CreateView(create) => {
                return execute_create_view(&catalog, &type_catalog, create);
            }
            ast::Statement::CreateSequence {
                temporary,
                if_not_exists,
                name,
                data_type,
                sequence_options,
                owned_by,
            } => {
                return execute_create_sequence(
                    &catalog,
                    *temporary,
                    *if_not_exists,
                    name,
                    data_type,
                    sequence_options,
                    owned_by,
                );
            }
            ast::Statement::CreateType {
                name,
                representation,
            } => return execute_create_type(global_catalog, name, representation),
            ast::Statement::AlterType(alter) => {
                return execute_alter_type(global_catalog, alter);
            }
            ast::Statement::CreateFunction(create) => {
                return execute_create_function(global_catalog, &type_catalog, create);
            }
            ast::Statement::CreateCast {
                source,
                target,
                method,
                ..
            } => return execute_create_cast(global_catalog, source, target, method),
            ast::Statement::CreateSchema {
                schema_name,
                if_not_exists,
                ..
            } => {
                return execute_create_schema(engine, schema_name, *if_not_exists);
            }
            ast::Statement::Drop {
                object_type: ast::ObjectType::Schema,
                names,
                cascade,
                if_exists,
                ..
            } => return execute_drop_schema(engine, names, *cascade, *if_exists),
            ast::Statement::Drop {
                object_type: ast::ObjectType::Table,
                names,
                cascade,
                if_exists,
                ..
            } => return execute_drop_table(&catalog, names, *cascade, *if_exists),
            ast::Statement::Drop {
                object_type: ast::ObjectType::View,
                names,
                cascade,
                if_exists,
                ..
            } => return execute_drop_view(&catalog, names, *cascade, *if_exists),
            ast::Statement::Drop {
                object_type: ast::ObjectType::Sequence,
                names,
                cascade,
                if_exists,
                ..
            } => return execute_drop_sequence(&catalog, names, *cascade, *if_exists),
            ast::Statement::Drop {
                object_type: ast::ObjectType::Index,
                names,
                cascade,
                if_exists,
                ..
            } => return execute_drop_index(engine, &catalog, session, names, *cascade, *if_exists),
            ast::Statement::Drop {
                object_type: ast::ObjectType::Type,
                names,
                cascade,
                if_exists,
                ..
            } => return execute_drop_type(global_catalog, names, *cascade, *if_exists),
            ast::Statement::DropCast {
                if_exists,
                source,
                target,
                ..
            } => return execute_drop_cast(global_catalog, source, target, *if_exists),
            ast::Statement::DropFunction(drop) => {
                return execute_drop_function(global_catalog, &catalog, &type_catalog, drop);
            }
            ast::Statement::CreateProcedure(create) => {
                return execute_create_procedure(global_catalog, create);
            }
            ast::Statement::DropProcedure {
                if_exists,
                proc_desc,
                drop_behavior,
            } => {
                return execute_drop_routine(
                    global_catalog,
                    &catalog,
                    &type_catalog,
                    RoutineKind::Procedure,
                    proc_desc,
                    *if_exists,
                    drop_behavior.as_ref(),
                );
            }
            ast::Statement::Do(block) => {
                return execute_do(engine, global_catalog, txnmgr, block, session);
            }
            ast::Statement::Call(call) => {
                return execute_call(engine, global_catalog, txnmgr, call, session);
            }
            ast::Statement::Declare { stmts } => {
                return crate::cursor::execute_declare(
                    engine,
                    global_catalog,
                    txnmgr,
                    stmt,
                    stmts,
                    session,
                    params,
                );
            }
            ast::Statement::Fetch {
                name,
                direction,
                into,
                ..
            } => {
                return crate::cursor::execute_fetch(name, direction, into.as_ref(), session);
            }
            ast::Statement::Move {
                name, direction, ..
            } => return crate::cursor::execute_move(name, direction, session),
            ast::Statement::Close { cursor } => {
                return crate::cursor::execute_close(cursor, session);
            }
            ast::Statement::Set(set) => return apply_set(set, session),
            ast::Statement::ShowVariable { variable } => {
                return execute_show(variable, session);
            }
            ast::Statement::Reset(reset) => return apply_reset(reset, session),
            ast::Statement::StartTransaction {
                modes,
                begin,
                modifier,
                statements,
                exception,
                has_end_keyword,
                ..
            } => {
                return begin_transaction(
                    session,
                    modes,
                    *begin,
                    modifier,
                    statements,
                    exception,
                    *has_end_keyword,
                );
            }
            ast::Statement::Commit {
                chain, modifier, ..
            } => return commit_transaction(txnmgr, session, *chain, modifier),
            ast::Statement::Rollback { chain, savepoint } => {
                return rollback_transaction(txnmgr, session, *chain, savepoint);
            }
            ast::Statement::Truncate(truncate) => {
                return execute_truncate(&catalog, txnmgr, session, truncate);
            }
            ast::Statement::Analyze(analyze) => {
                return execute_analyze(&catalog, txnmgr, session, analyze);
            }
            ast::Statement::Vacuum(vacuum) => {
                return execute_vacuum(&catalog, txnmgr, session, vacuum);
            }
            ast::Statement::AlterTable(alter) => {
                return execute_alter_table(
                    &catalog,
                    engine,
                    &type_catalog,
                    &catalog_ops,
                    global_catalog,
                    txnmgr,
                    session,
                    alter,
                );
            }
            ast::Statement::CreateIndex(create) => {
                return execute_create_index(&catalog, txnmgr, session, create);
            }
            // `COPY … FROM '<file>'` needs no wire sub-protocol: the server reads
            // the file itself, so it runs on the ordinary execute path. The STDIN
            // form never reaches here — the connection intercepts it before
            // execution (see `is_copy_from_stdin`).
            ast::Statement::Copy { .. } => {
                return execute_copy_from_file(engine, global_catalog, txnmgr, session, stmt);
            }
            other => {
                return Err(PgError::feature_not_supported(format!(
                    "statement is not supported yet: {}",
                    statement_kind(other)
                )));
            }
        },
    };
    // The inner statement bound, so the modifiers can be read now — after name
    // resolution, where PG reads them.
    let explain = match explain_options {
        Some((analyze, verbose, options)) => {
            Some(ExplainOptions::resolve(analyze, verbose, options)?)
        }
        None => None,
    };
    // A write statement needs an XID to stamp its versions; a read runs with
    // none. Decide from the bound plan, not the surface AST: the binder already
    // resolved the statement to an Insert/Update/Delete node, so a new writing
    // statement kind can't accidentally run XID-less and produce invisible rows.
    // A routine body may write, and nothing here can tell before running it, so
    // a statement that calls one is treated as a write: it needs an XID to stamp
    // the body's versions with. Conservative — a pure routine burns an XID — and
    // observably harmless, since PostgreSQL defaults routines to VOLATILE anyway.
    let calls_routine = crabgresql_binder::plan_calls_routine(&logical);
    let dml_verb = match logical {
        LogicalPlan::Insert { .. } => Some("INSERT"),
        LogicalPlan::Update { .. } => Some("UPDATE"),
        LogicalPlan::Delete { .. } => Some("DELETE"),
        _ => None,
    };
    // A plain `EXPLAIN <write>` never executes, so it is not a write: PG accepts
    // `BEGIN READ ONLY; EXPLAIN DELETE FROM t` and raises 25006 only once ANALYZE
    // makes the statement run for real. That covers a routine call too — plain
    // `EXPLAIN SELECT f()` plans without ever entering the body.
    let runs = explain.is_none_or(|opts| opts.analyze);
    let is_write = (calls_routine || dml_verb.is_some()) && runs;
    // A write in a READ ONLY transaction (or under the read-only session default)
    // is rejected before it stamps any version, matching PG's 25006.
    //
    // This keys off the DML node, not off `is_write`: a routine is only
    // *treated* as a write so it has an XID to stamp with, and whether it really
    // writes is decided inside the body, which does its own read-only check. But
    // that reasoning covers only the routine — an outer `INSERT INTO t VALUES
    // (f(1))` writes no matter what `f` does, and nothing downstream would catch
    // it (the executor's DML paths never consult `ExecContext::read_only`).
    if let Some(verb) = dml_verb
        && runs
        && read_only_active(session)
    {
        return Err(PgError::new(
            sqlstate::READ_ONLY_SQL_TRANSACTION,
            format!("cannot execute {verb} in a read-only transaction"),
        ));
    }
    // Only EXPLAIN reports a planning time, so only EXPLAIN pays for the clock.
    // The clock brackets planning alone, as PG's does — parse analysis (here, the
    // binder) is outside it there too. crabgresql's planner does far less than
    // PG's, so this number is legitimately small; it is not comparable to PG's.
    let planning_started = explain.is_some().then(Instant::now);
    let plan = crabgresql_planner::plan(logical);
    let planning = planning_started.map_or(Duration::ZERO, |started| started.elapsed());
    // Rendered here, while the plan is still borrowable — `execute` consumes it.
    let explaining = explain.map(|opts| (opts, crabgresql_planner::explain(&plan)));
    let read_only = read_only_active(session);
    // The routine handle and command counter a called routine's body runs under.
    // Built before the EXPLAIN branch, not after: `EXPLAIN ANALYZE SELECT f()`
    // really does enter the body, so it needs the same runtime a bare call gets.
    let (routines, command_counter) =
        statement_runtime(&catalog, &type_catalog, global_catalog, session);
    if is_write {
        await_write_capacity(engine, session);
    }
    // Every statement that reads takes a snapshot, EXPLAIN included: PG pins the
    // transaction snapshot for it, so inside a REPEATABLE READ block a plain
    // EXPLAIN freezes the view the rest of the block sees. That is why this runs
    // before the EXPLAIN paths return, not after.
    let txn = build_txn(txnmgr, session, is_write);
    if let Some((opts, mut lines)) = explaining {
        // A plain EXPLAIN stops at the plan; only ANALYZE runs the statement, and
        // it shares this statement's transaction bookkeeping — a write has an XID
        // and commits (or aborts, if the run faults mid-stream) exactly as the
        // bare statement would.
        let execution = match opts.analyze {
            false => None,
            true => {
                let exec_ctx = session.exec_context_for_statement(
                    engine,
                    &catalog_ops,
                    &type_catalog,
                    Arc::clone(&routines),
                    Arc::clone(&command_counter),
                    read_only,
                );
                match run_analyze(plan, &exec_ctx, &txn) {
                    Ok(execution) => Some(execution),
                    Err(e) => {
                        // Abort path: infallible, so the result is safe to drop.
                        let _ = finalize_statement(
                            txnmgr,
                            session,
                            &txn,
                            is_write,
                            false,
                            Some(&command_counter),
                        );
                        return Err(e.into());
                    }
                }
            }
        };
        finalize_statement(
            txnmgr,
            session,
            &txn,
            is_write,
            true,
            Some(&command_counter),
        )?;
        if opts.summary {
            lines.extend(crabgresql_planner::explain_summary(planning, execution));
        }
        return Ok(explain_result(lines, session));
    }
    // Sequence functions (`nextval` in a `serial` default or written explicitly)
    // advance non-transactional counters in the shared engine and update this
    // session's `currval`/`lastval`, so the execution context carries a handle to
    // both. Sequences resolve against the global engine (temp sequences are
    // unsupported), matching temp-view handling. The read-only flag lets a bare
    // `SELECT nextval(...)` be rejected (25006) even though it is not a DML write.
    // The catalog handle is the same snapshot this statement bound against, so
    // `pg_table_is_visible(c.oid)` agrees with the `pg_class` rows it filters.
    let exec_ctx = session.exec_context_for_statement(
        engine,
        &catalog_ops,
        &type_catalog,
        routines,
        Arc::clone(&command_counter),
        read_only,
    );
    let exec = match execute(plan, &exec_ctx, &txn) {
        Ok(exec) => exec,
        Err(e) => {
            // Abort path: infallible, so the result is safe to drop.
            let _ = finalize_statement(
                txnmgr,
                session,
                &txn,
                is_write,
                false,
                Some(&command_counter),
            );
            return Err(e.into());
        }
    };
    // `finalize_statement` closes the statement's transaction, and a streamed
    // result set is pulled *after* it returns — so a routine called per row
    // would run its body after its own transaction had already committed or
    // aborted. Drain the rows here instead when a routine is in the plan.
    //
    // A deliberate, temporary deviation: PostgreSQL streams and holds the
    // transaction open until the portal closes. Undoing it needs per-portal
    // transaction lifetimes; until then the cost is the result set's memory,
    // paid only by statements that call a routine.
    //
    // `force_materialize` is the same need from the other direction: `DECLARE …
    // CURSOR` keeps its rows past the end of this statement, so they have to be
    // read while the transaction is still open.
    //
    // Draining is where a routine body actually runs, so it is also where a
    // `RAISE EXCEPTION` surfaces — it needs the same abort path `execute` has
    // above, or the statement's XID is never marked aborted and stays in the
    // in-flight set, pinning the snapshot horizon for the life of the process.
    let exec = if calls_routine || force_materialize {
        match materialize(exec) {
            Ok(exec) => exec,
            Err(e) => {
                // Abort path: infallible, so the result is safe to drop.
                let _ = finalize_statement(
                    txnmgr,
                    session,
                    &txn,
                    is_write,
                    false,
                    Some(&command_counter),
                );
                return Err(e);
            }
        }
    } else {
        exec
    };
    finalize_statement(
        txnmgr,
        session,
        &txn,
        is_write,
        true,
        Some(&command_counter),
    )?;
    // Anything a routine body raised is buffered on the session; hand it to the
    // caller alongside the result so it goes out ahead of the rows.
    let notices = session.notices.drain();
    let result = match exec {
        Execution::Rows { columns, node } => QueryResult::Rows {
            columns,
            node,
            tag: RowTag::Select,
            notices,
        },
        Execution::ReturningRows {
            columns,
            node,
            verb,
        } => QueryResult::Rows {
            columns,
            node,
            tag: verb.into(),
            notices,
        },
        Execution::Inserted(n) => command_with(format!("INSERT 0 {n}"), notices),
        Execution::Updated(n) => command_with(format!("UPDATE {n}"), notices),
        Execution::Deleted(n) => command_with(format!("DELETE {n}"), notices),
    };
    Ok(result)
}

/// The routine dispatcher and command counter one statement executes with.
///
/// Both are per statement. The dispatcher binds a routine body against the same
/// temp-first overlay and `pg_catalog` snapshot the caller bound against, so a
/// body resolves names exactly as its caller did. The counter is seeded from the
/// open block's command id and read back by [`finalize_statement`], so command
/// ids advanced *inside* a routine body are not lost — without that, the next
/// top-level statement would reuse an id the body already stamped rows with and
/// could not see them.
fn statement_runtime(
    catalog: &Arc<dyn TableEngine>,
    type_catalog: &Arc<dyn TypeCatalog>,
    global_catalog: &Arc<GlobalCatalog>,
    session: &Session,
) -> (Arc<dyn RoutineOps>, Arc<AtomicU32>) {
    let routines: Arc<dyn RoutineOps> = Arc::new(RoutineDispatch::new(
        Arc::clone(catalog),
        Arc::clone(type_catalog),
        Arc::clone(global_catalog),
        Arc::clone(&session.routine_cache),
    ));
    let start = session
        .xact
        .as_ref()
        .map_or(CommandId::FIRST.0, |active| active.cid.0);
    (routines, Arc::new(AtomicU32::new(start)))
}

/// A command-tag result carrying diagnostics raised while it ran.
fn command_with(tag: String, notices: Vec<Notice>) -> QueryResult {
    QueryResult::Command { tag, notices }
}

/// Pull a result set into memory, so nothing is left to run after the
/// statement's transaction closes. Mutation counts pass through untouched.
fn materialize(exec: Execution) -> Result<Execution, PgError> {
    let (columns, mut node, verb) = match exec {
        Execution::Rows { columns, node } => (columns, node, None),
        Execution::ReturningRows {
            columns,
            node,
            verb,
        } => (columns, node, Some(verb)),
        done => return Ok(done),
    };
    let mut rows = Vec::new();
    while let Some(row) = node.next()? {
        rows.push(row);
    }
    let node: Box<dyn ExecNode> = Box::new(MaterializedRows::new(rows));
    Ok(match verb {
        Some(verb) => Execution::ReturningRows {
            columns,
            node,
            verb,
        },
        None => Execution::Rows { columns, node },
    })
}

/// Build the [`TxnContext`] a statement executes under. Under autocommit each
/// statement is its own implicit transaction (a write allocates a throwaway XID,
/// a read uses none); inside an explicit block the XID is allocated lazily on
/// the first write and reused, and the snapshot policy follows the isolation
/// level (fresh per statement for READ COMMITTED, frozen once for REPEATABLE
/// READ and above).
/// Let the engine hold this statement back if its write buffers are full.
///
/// Must run before [`build_txn`], which is where a write allocates its XID: the
/// engine relieves pressure by reclaiming rows no snapshot can still see, and
/// the reclamation horizon is bounded by the oldest XID in flight. A waiter
/// holding one would be waiting for a flush that its own wait makes impossible.
///
/// Which is also why an explicit transaction block is exempt. Inside `BEGIN`
/// the XID is already allocated and outlives the statement, so there is no
/// point in the block at which waiting is safe; such a session writes through
/// and the flush worker catches up afterwards.
fn await_write_capacity(engine: &Arc<dyn TableEngine>, session: &Session) {
    if session.xact.is_none() {
        engine.await_write_capacity();
    }
}

fn build_txn(txnmgr: &TransactionManager, session: &mut Session, is_write: bool) -> TxnContext {
    // The connection's session-stable table-lock owner, stamped onto every
    // context so a transaction can upgrade its own AccessShare hold (an open
    // cursor) to AccessExclusive (TRUNCATE) without self-deadlocking.
    let lock_owner = session.lock_owner;
    let mut ctx = match &mut session.xact {
        Some(active) => {
            let xid = if is_write {
                *active.xid.get_or_insert_with(|| txnmgr.allocate_xid())
            } else {
                active.xid.unwrap_or(Xid::INVALID)
            };
            let snapshot = match active.iso {
                IsolationLevel::ReadCommitted => txnmgr.snapshot(),
                // `freeze_snapshot` keeps the block's registration alive for the
                // block's whole life. The per-statement guard on the `TxnContext`
                // below only covers this statement, and a read-only block holds no
                // XID, so without this the block is invisible to `reclaim_horizon`
                // while it sits idle between statements — exactly when a concurrent
                // VACUUM would run.
                IsolationLevel::RepeatableRead | IsolationLevel::Serializable => active
                    .snapshot
                    .get_or_insert_with(|| txnmgr.freeze_snapshot())
                    .0
                    .clone(),
            };
            txnmgr.context_with(xid, active.cid, snapshot, active.iso)
        }
        None => {
            let xid = if is_write {
                txnmgr.allocate_xid()
            } else {
                Xid::INVALID
            };
            txnmgr.context(xid, CommandId::FIRST)
        }
    };
    ctx.lock_owner = lock_owner;
    ctx
}

/// Close out a statement's transaction bookkeeping. Under autocommit a write
/// commits (or aborts on error) immediately — its effects are meant to persist
/// at the statement boundary. Inside a block nothing is finalized here; the XID
/// lives until `COMMIT`/`ROLLBACK`. The command counter advances so the next
/// statement in a block sees this one's writes.
fn finalize_statement(
    txnmgr: &TransactionManager,
    session: &mut Session,
    txn: &TxnContext,
    is_write: bool,
    ok: bool,
    command_counter: Option<&Arc<AtomicU32>>,
) -> Result<(), PgError> {
    match &mut session.xact {
        None => {
            if is_write {
                if ok {
                    // The commit fsyncs the WAL on the durable engine; a failure
                    // there is a system error, surfaced to the client.
                    // TODO(perf): this blocking fsync runs on the tokio reactor
                    // worker. Move statement execution onto a blocking pool (or
                    // guard with block_in_place on multi-thread runtimes) so
                    // concurrent committers don't stall the accept loop.
                    txnmgr.commit(txn.xid).map_err(commit_io_error)?;
                } else {
                    txnmgr.abort(txn.xid);
                }
            }
        }
        Some(active) => {
            // Read the counter back rather than adding one: a routine body may
            // have advanced it several times, and reusing an id it already
            // stamped rows with would hide those rows from the next statement.
            let used = command_counter.map_or(active.cid.0, |c| {
                c.load(std::sync::atomic::Ordering::Acquire)
            });
            active.cid = CommandId(used.max(active.cid.0) + 1);
        }
    }
    Ok(())
}

/// A bound COPY plus the catalogs its rows will be built against — the type
/// catalog its data fields parse against, and the relation snapshot a column
/// default's catalog function would resolve through. Returned by
/// [`prepare_copy_from`] and threaded into [`run_copy_insert`] so both are built
/// once per COPY, not twice.
pub struct PreparedCopy {
    pub plan: CopyFromPlan,
    catalog: Arc<dyn TypeCatalog>,
    catalog_ops: Arc<dyn CatalogOps>,
    /// The temp-first engine overlay this COPY bound against, kept so a column
    /// default that calls a routine resolves names the same way the COPY did.
    engine: Arc<dyn TableEngine>,
    global: Arc<GlobalCatalog>,
}

/// Resolve `COPY <table> [(cols)] FROM STDIN` up to the point the connection
/// can send `CopyInResponse`: bind the target relation, column list, and
/// text/CSV format, and reject the write in an aborted (25P02) or read-only
/// (25006) transaction. No row data is read here — that streams in over the wire
/// afterwards. Kept separate from [`execute_statement`] because COPY needs socket
/// access the pure execute path does not have.
pub fn prepare_copy_from(
    engine: &Arc<dyn TableEngine>,
    global_catalog: &Arc<GlobalCatalog>,
    stmt: &ast::Statement,
    session: &Session,
) -> Result<PreparedCopy, PgError> {
    // In an aborted transaction block PG rejects everything but COMMIT/ROLLBACK,
    // before entering copy mode. COPY bypasses execute_statement's guard, so
    // re-establish it here.
    if session.tx_status == TransactionStatus::Failed {
        return Err(PgError::new(
            sqlstate::IN_FAILED_SQL_TRANSACTION,
            "current transaction is aborted, commands ignored until end of transaction block",
        ));
    }
    let ast::Statement::Copy {
        source,
        to,
        target,
        options,
        legacy_options,
        ..
    } = stmt
    else {
        return Err(PgError::new(
            sqlstate::INTERNAL_ERROR,
            "prepare_copy_from called with a non-COPY statement",
        ));
    };
    let (catalog, type_catalog, catalog_ops) = bind_catalogs(engine, global_catalog, session);
    let plan = bind_copy_from(
        &catalog,
        &type_catalog,
        source,
        *to,
        target,
        options,
        legacy_options,
    )?;
    // A COPY FROM is a write: rejected in a READ ONLY transaction, before it
    // stamps any version. (The table is resolved first, matching DML's 25006
    // ordering.)
    if read_only_active(session) {
        return Err(PgError::new(
            sqlstate::READ_ONLY_SQL_TRANSACTION,
            "cannot execute COPY in a read-only transaction",
        ));
    }
    Ok(PreparedCopy {
        plan,
        catalog: type_catalog,
        catalog_ops,
        engine: catalog,
        global: Arc::clone(global_catalog),
    })
}

/// Execute a bound COPY as an INSERT of the decoded field rows, under a write
/// transaction, returning the number of rows loaded (the `COPY n` count). Reuses
/// the same XID/commit lifecycle and constraint checks as an ordinary INSERT.
pub fn run_copy_insert(
    engine: &Arc<dyn TableEngine>,
    txnmgr: &Arc<TransactionManager>,
    session: &mut Session,
    prepared: &PreparedCopy,
    rows: Vec<Vec<Option<String>>>,
) -> Result<u64, PgError> {
    run_copy_rows(engine, txnmgr, session, prepared, |insert| {
        insert(rows).map(|_| ())
    })
}

/// Load a COPY's rows, however many batches they arrive in, as **one**
/// transaction: `produce` is handed an inserter it may call repeatedly, and
/// every batch lands under the same XID and command id. That is what makes a
/// server-side file COPY atomic — a row the file's tail cannot parse must leave
/// none of its head visible.
///
/// The write context (capacity wait, XID, routine runtime, execution context) is
/// built once here rather than per batch, and the statement is finalized once:
/// committed if `produce` and every batch succeeded, aborted otherwise.
pub fn run_copy_rows(
    engine: &Arc<dyn TableEngine>,
    txnmgr: &Arc<TransactionManager>,
    session: &mut Session,
    prepared: &PreparedCopy,
    produce: impl FnOnce(
        &mut dyn FnMut(Vec<Vec<Option<String>>>) -> Result<u64, PgError>,
    ) -> Result<(), PgError>,
) -> Result<u64, PgError> {
    // A COPY is a write (read-only was rejected at prepare time); its context
    // carries a sequence handle so a `serial`/`nextval()` column default advances
    // the sequence and updates this session's currval/lastval, as INSERT does,
    // and the catalog snapshot bound at prepare time for the same reason.
    let read_only = read_only_active(session);
    await_write_capacity(engine, session);
    let txn = build_txn(txnmgr, session, true);
    // FREEZE stamps rows visible to everyone with no XID whose abort could take
    // them back, so it is only sound where a rollback throws the storage away:
    // the relation must have been truncated by this very transaction. Checked
    // here rather than at bind time because only now is there a transaction to
    // name — and before any row is written, so a refusal loads nothing.
    //
    // The check names the very relation the plan writes to, and the freeze itself
    // rides on the plan node so it reaches only that relation's write — see
    // `crabgresql_txn::TxnContext::freeze_inserts`. Authorizing one relation and
    // freezing the whole transaction is how this leaked rows into tables a
    // rollback does not discard.
    //
    // PostgreSQL also accepts a table *created* in the current subtransaction.
    // This engine's DDL is not transactional, so a rolled-back CREATE leaves the
    // relation behind and its frozen rows with it; that half is refused, with
    // PostgreSQL's wording kept because the wording is the observable part.
    if prepared.plan.freeze && !prepared.plan.target().truncated_by(txn.xid) {
        // No command id was consumed and no routine ran, so there is no counter to
        // read back — unlike the failure paths below, which have executed batches.
        let _ = finalize_statement(txnmgr, session, &txn, true, false, None);
        return Err(PgError::new(
            sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
            "cannot perform COPY FREEZE because the table was not created or \
             truncated in the current subtransaction",
        ));
    }
    let (routines, command_counter) = statement_runtime(
        &prepared.engine,
        &prepared.catalog,
        &prepared.global,
        session,
    );
    let exec_ctx = session.exec_context_for_statement(
        engine,
        &prepared.catalog_ops,
        &prepared.catalog,
        routines,
        Arc::clone(&command_counter),
        read_only,
    );

    let mut loaded = 0u64;
    let outcome = produce(&mut |rows| {
        // Turn the decoded rows into an INSERT ... VALUES plan (each field parses
        // via its column's input function against the type catalog bound at
        // prepare time).
        let logical = prepared.plan.build_insert(&prepared.catalog, rows)?;
        // Each batch runs at its own command id so it can see the rows the
        // previous batches wrote. Without this a UNIQUE index is only enforced
        // *within* a batch, because the duplicate check scans the table through
        // this context and `satisfies_mvcc` hides same-command inserts.
        let batch_txn = txn.with_cid(CommandId(
            command_counter.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1,
        ));
        match execute(crabgresql_planner::plan(logical), &exec_ctx, &batch_txn)? {
            Execution::Inserted(n) => {
                loaded += n;
                Ok(n)
            }
            _ => Err(PgError::new(
                sqlstate::INTERNAL_ERROR,
                "COPY produced an unexpected execution result",
            )),
        }
    });
    if let Err(e) = outcome {
        let _ = finalize_statement(txnmgr, session, &txn, true, false, Some(&command_counter));
        return Err(e);
    }
    // A column default can call a routine, whose body advances the counter, so
    // the block's command id has to be read back rather than merely bumped.
    finalize_statement(txnmgr, session, &txn, true, true, Some(&command_counter))?;
    Ok(loaded)
}

/// Run `COPY <table> [(cols)] FROM '<file>'`: bind it like the STDIN form, then
/// stream the file through the shared text/CSV decoder, inserting in batches
/// inside one transaction.
///
/// The file is resolved, authorized and opened *before* [`run_copy_rows`] takes
/// a transaction, so a path that can never work — outside the permitted roots,
/// missing, a directory — costs no XID and leaves an open block usable.
///
/// A `COPY … FROM STDIN` reaching here is a routing bug rather than a user
/// error, so it reports the wire-level requirement instead of silently loading
/// nothing.
fn execute_copy_from_file(
    engine: &Arc<dyn TableEngine>,
    global_catalog: &Arc<GlobalCatalog>,
    txnmgr: &Arc<TransactionManager>,
    session: &mut Session,
    stmt: &ast::Statement,
) -> Result<QueryResult, PgError> {
    let prepared = prepare_copy_from(engine, global_catalog, stmt, session)?;
    let path = match &prepared.plan.source {
        CopyFromSource::File(path) => path.clone(),
        CopyFromSource::Stdin => {
            return Err(PgError::new(
                sqlstate::PROTOCOL_VIOLATION,
                "COPY FROM STDIN must be driven by the copy-in protocol",
            ));
        }
    };
    let file = crate::copy::open_source_file(global_catalog.copy_files(), &path)?;
    let format = prepared.plan.format.clone();
    // The write target decides how much to decode at once: for a row store the
    // batch is only a memory bound, but a method that turns each batch into one
    // immutable unit also has the units' size decided here.
    let batch_rows = prepared
        .plan
        .target()
        .schema()
        .access_method
        .bulk_load_batch_rows();
    let rows = run_copy_rows(engine, txnmgr, session, &prepared, |insert| {
        crate::copy::read_file_rows(file, &path, &format, batch_rows, |batch| {
            insert(batch).map(|_| ())
        })
    })?;
    Ok(command_with(format!("COPY {rows}"), Vec::new()))
}

/// Map a WAL/commit I/O failure to a SQLSTATE 58030 system error.
fn commit_io_error(e: std::io::Error) -> PgError {
    PgError::new(
        sqlstate::IO_ERROR,
        format!("could not commit transaction: {e}"),
    )
}

/// `AND CHAIN` (commit/rollback then immediately open an identical block) is not
/// implemented yet; shared by the BEGIN/COMMIT/ROLLBACK handlers.
fn and_chain_unsupported() -> PgError {
    PgError::feature_not_supported("AND CHAIN is not supported yet")
}

/// Map the parser's SQL isolation level to the core [`IsolationLevel`]. `READ
/// UNCOMMITTED` aliases READ COMMITTED (PG never permits dirty reads);
/// `SERIALIZABLE` maps to REPEATABLE READ-strength visibility for now (true SSI
/// is M3, per the `IsolationLevel::Serializable` note in `crabgresql-txn`).
/// `SNAPSHOT` (a non-standard extension) is not supported.
fn map_isolation_level(level: ast::TransactionIsolationLevel) -> Result<IsolationLevel, PgError> {
    use ast::TransactionIsolationLevel as Sql;
    Ok(match level {
        Sql::ReadUncommitted | Sql::ReadCommitted => IsolationLevel::ReadCommitted,
        Sql::RepeatableRead => IsolationLevel::RepeatableRead,
        Sql::Serializable => IsolationLevel::Serializable,
        Sql::Snapshot => {
            return Err(PgError::feature_not_supported(
                "SNAPSHOT isolation level is not supported",
            ));
        }
    })
}

/// Fold a list of transaction modes into an optional isolation level and an
/// optional read-only flag. A `None` means the mode was not named (so the caller
/// keeps its default); repeated modes are last-wins, as in PG.
fn apply_modes(
    modes: &[ast::TransactionMode],
) -> Result<(Option<IsolationLevel>, Option<bool>), PgError> {
    let mut iso = None;
    let mut read_only = None;
    for mode in modes {
        match mode {
            ast::TransactionMode::IsolationLevel(level) => iso = Some(map_isolation_level(*level)?),
            ast::TransactionMode::AccessMode(ast::TransactionAccessMode::ReadOnly) => {
                read_only = Some(true);
            }
            ast::TransactionMode::AccessMode(ast::TransactionAccessMode::ReadWrite) => {
                read_only = Some(false);
            }
        }
    }
    Ok((iso, read_only))
}

/// Whether the statement about to run is in a read-only context: an explicit
/// `READ ONLY` block, or (under autocommit) the `default_transaction_read_only`
/// GUC. Writes in such a context are rejected with SQLSTATE 25006.
fn read_only_active(session: &Session) -> bool {
    match &session.xact {
        Some(active) => active.read_only,
        None => session.default_read_only,
    }
}

/// Whether executing `stmt` acquires a snapshot (PG's `FirstSnapshotSet`).
/// Transaction control and `SET`/`RESET` take none; every other statement —
/// queries, DML, and DDL — does. Used to enforce the "SET TRANSACTION ISOLATION
/// LEVEL before any query" rule uniformly across statement kinds.
///
/// `FETCH`/`MOVE` read no table: they walk a cursor whose snapshot was taken
/// back at its `DECLARE`. `DECLARE` is where that snapshot is taken, and `CLOSE`
/// counts as a query in PostgreSQL — `PlannedStmtRequiresSnapshot` exempts
/// `FetchStmt` but not `ClosePortalStmt`, so `BEGIN; CLOSE ALL; SET TRANSACTION
/// ISOLATION LEVEL …` raises 25001 there. Both therefore keep the default.
fn statement_takes_snapshot(stmt: &ast::Statement) -> bool {
    !matches!(
        stmt,
        ast::Statement::StartTransaction { .. }
            | ast::Statement::Commit { .. }
            | ast::Statement::Rollback { .. }
            | ast::Statement::Set(_)
            | ast::Statement::Reset(_)
            | ast::Statement::Fetch { .. }
            | ast::Statement::Move { .. }
    )
}

/// The PG command name for a data-changing DDL/utility statement that a
/// read-only transaction must reject (25006) *before* name resolution, or `None`
/// if the statement is not one of them. DML (INSERT/UPDATE/DELETE) is excluded:
/// PG resolves the target relation first there, so it is checked after binding.
fn read_only_prohibited_ddl(stmt: &ast::Statement) -> Option<&'static str> {
    Some(match stmt {
        ast::Statement::CreateTable(_) => "CREATE TABLE",
        ast::Statement::CreateView(_) => "CREATE VIEW",
        ast::Statement::CreateSequence { .. } => "CREATE SEQUENCE",
        ast::Statement::CreateIndex(_) => "CREATE INDEX",
        ast::Statement::CreateType { .. } => "CREATE TYPE",
        ast::Statement::AlterTable(_) => "ALTER TABLE",
        ast::Statement::AlterType(_) => "ALTER TYPE",
        ast::Statement::CreateFunction(_) => "CREATE FUNCTION",
        ast::Statement::CreateCast { .. } => "CREATE CAST",
        ast::Statement::CreateSchema { .. } => "CREATE SCHEMA",
        ast::Statement::Truncate(_) => "TRUNCATE TABLE",
        ast::Statement::Drop {
            object_type: ast::ObjectType::Schema,
            ..
        } => "DROP SCHEMA",
        ast::Statement::Drop {
            object_type: ast::ObjectType::Table,
            ..
        } => "DROP TABLE",
        ast::Statement::Drop {
            object_type: ast::ObjectType::View,
            ..
        } => "DROP VIEW",
        ast::Statement::Drop {
            object_type: ast::ObjectType::Sequence,
            ..
        } => "DROP SEQUENCE",
        ast::Statement::Drop {
            object_type: ast::ObjectType::Type,
            ..
        } => "DROP TYPE",
        ast::Statement::DropCast { .. } => "DROP CAST",
        _ => return None,
    })
}

/// `BEGIN` / `START TRANSACTION`. Enters the transaction block, seeding its
/// isolation level and access mode from the transaction modes (falling back to
/// the session defaults). A redundant BEGIN warns but stays in the block. Data
/// rollback and MVCC snapshots are already wired through [`ActiveTxn`]; the
/// block's XID is allocated lazily on its first write.
fn begin_transaction(
    session: &mut Session,
    modes: &[ast::TransactionMode],
    begin: bool,
    modifier: &Option<ast::TransactionModifier>,
    statements: &[ast::Statement],
    exception: &Option<Vec<ast::ExceptionWhen>>,
    has_end_keyword: bool,
) -> Result<QueryResult, PgError> {
    // `BEGIN ... END` is a procedural (atomic) block, not transaction control —
    // reject it regardless of the current state.
    if !statements.is_empty() || exception.is_some() || has_end_keyword {
        return Err(PgError::feature_not_supported(
            "BEGIN ... END atomic blocks are not supported yet",
        ));
    }
    // PG completes `BEGIN` and `START TRANSACTION` with distinct tags.
    let tag = if begin { "BEGIN" } else { "START TRANSACTION" };
    // Inside a block PG ignores the command's arguments and only warns, so an
    // unsupported mode must not abort an already-open transaction.
    if session.tx_status == TransactionStatus::InTransaction {
        return Ok(QueryResult::Command {
            tag: tag.into(),
            notices: vec![Notice::warning(
                sqlstate::ACTIVE_SQL_TRANSACTION,
                "there is already a transaction in progress",
            )],
        });
    }
    if modifier.is_some() {
        return Err(and_chain_unsupported());
    }
    // Resolve modes before mutating any state, so an unsupported mode (SNAPSHOT)
    // fails without half-opening a block. Named modes override the session
    // defaults; unnamed ones inherit them.
    let (mode_iso, mode_read_only) = apply_modes(modes)?;
    let iso = mode_iso.unwrap_or(session.default_iso);
    let read_only = mode_read_only.unwrap_or(session.default_read_only);
    session.tx_status = TransactionStatus::InTransaction;
    // A `BEGIN` inside an extended-query batch adopts the implicit block's
    // start rather than opening a new one, as in PostgreSQL; outside one,
    // `xact_start()` is this message's own stamp.
    session.xact = Some(ActiveTxn::new(iso, read_only, session.xact_start()));
    Ok(QueryResult::command(tag))
}

/// `COMMIT` / `END`. Ends the block. A COMMIT of a failed block is reported as
/// ROLLBACK, and a COMMIT with no block open warns — both as in PG.
fn commit_transaction(
    txnmgr: &TransactionManager,
    session: &mut Session,
    chain: bool,
    modifier: &Option<ast::TransactionModifier>,
) -> Result<QueryResult, PgError> {
    if chain {
        return Err(and_chain_unsupported());
    }
    if modifier.is_some() {
        return Err(PgError::feature_not_supported(
            "transaction modifiers are not supported yet",
        ));
    }
    let mut notices = Vec::new();
    // A COMMIT of a failed block is a rollback, so its plain `SET`s revert too.
    let committed = session.tx_status != TransactionStatus::Failed;
    session.restore_gucs_at_transaction_end(committed);
    let tag = match session.tx_status {
        // A COMMIT of a failed block rolls back: abort the block's XID so its
        // writes become dead.
        TransactionStatus::Failed => {
            abort_active(txnmgr, session);
            "ROLLBACK"
        }
        TransactionStatus::Idle => {
            notices.push(Notice::warning(
                sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                "there is no transaction in progress",
            ));
            "COMMIT"
        }
        TransactionStatus::InTransaction => {
            if let Some(active) = session.xact.take() {
                txnmgr
                    .commit(active.xid.unwrap_or(Xid::INVALID))
                    .map_err(commit_io_error)?;
            }
            // The block's cursors end with it, except the holdable ones.
            crate::cursor::close_on_commit(session);
            "COMMIT"
        }
    };
    session.tx_status = TransactionStatus::Idle;
    Ok(QueryResult::Command {
        tag: tag.into(),
        notices,
    })
}

/// `ROLLBACK`. Ends the block (in any state); with no block open it warns.
/// Aborting the block's XID makes every version it wrote dead (MVCC undo, no
/// physical rollback needed), and vacuum later reclaims them.
fn rollback_transaction(
    txnmgr: &TransactionManager,
    session: &mut Session,
    chain: bool,
    savepoint: &Option<ast::Ident>,
) -> Result<QueryResult, PgError> {
    if chain {
        return Err(and_chain_unsupported());
    }
    if savepoint.is_some() {
        return Err(PgError::feature_not_supported(
            "ROLLBACK TO SAVEPOINT is not supported yet",
        ));
    }
    let mut notices = Vec::new();
    if session.tx_status == TransactionStatus::Idle {
        notices.push(Notice::warning(
            sqlstate::NO_ACTIVE_SQL_TRANSACTION,
            "there is no transaction in progress",
        ));
    }
    // Abort the block's XID: every version it wrote becomes dead, with no undo.
    abort_active(txnmgr, session);
    session.restore_gucs_at_transaction_end(false);
    session.tx_status = TransactionStatus::Idle;
    Ok(QueryResult::Command {
        tag: "ROLLBACK".into(),
        notices,
    })
}

/// Abort and clear the session's active transaction, if any.
fn abort_active(txnmgr: &TransactionManager, session: &mut Session) {
    if let Some(active) = session.xact.take() {
        txnmgr.abort(active.xid.unwrap_or(Xid::INVALID));
    }
    // A rollback closes every cursor the block declared, holdable ones included
    // — a holdable cursor only earns its reprieve by committing.
    crate::cursor::close_on_abort(session);
}

/// `TRUNCATE [TABLE] name [, ...]` (bare form only). All named tables are
/// resolved before any is emptied, so a missing table fails the whole statement.
fn execute_truncate(
    engine: &Arc<dyn TableEngine>,
    txnmgr: &Arc<TransactionManager>,
    session: &mut Session,
    truncate: &ast::Truncate,
) -> Result<QueryResult, PgError> {
    if truncate.cascade.is_some() {
        return Err(PgError::feature_not_supported(
            "TRUNCATE ... CASCADE/RESTRICT is not supported yet",
        ));
    }
    if truncate.identity.is_some() {
        return Err(PgError::feature_not_supported(
            "TRUNCATE ... RESTART/CONTINUE IDENTITY is not supported yet",
        ));
    }
    if truncate.if_exists {
        return Err(PgError::feature_not_supported(
            "TRUNCATE ... IF EXISTS is not supported yet",
        ));
    }
    if truncate.partitions.is_some() {
        return Err(PgError::feature_not_supported(
            "TRUNCATE of a partition list is not supported yet",
        ));
    }
    if truncate.on_cluster.is_some() {
        return Err(PgError::feature_not_supported(
            "TRUNCATE ... ON CLUSTER is not supported yet",
        ));
    }
    // Keyed by `(namespace, name)`, not the bare name: a recursion can reach
    // children in other schemas, and a bare-name key would let one of them
    // collide with an unrelated same-named relation and silently drop it from
    // the statement.
    let mut named: Vec<((String, String), Arc<dyn TableAm>)> =
        Vec::with_capacity(truncate.table_names.len());
    let mut push = |table: Arc<dyn TableAm>| {
        let schema = table.schema();
        named.push((
            (schema.namespace.clone(), schema.name.clone()),
            table.clone(),
        ));
    };
    for target in &truncate.table_names {
        let name = object_name_to_table_name(&target.name)?;
        let table = engine.open_table(&name)?;
        reject_partitioned_parent(&table, &name)?;
        push(table.clone());
        // TRUNCATE recurses into the inheritance children unless told `ONLY`
        // (`t*` is the explicit spelling of the default). Emptying the parent
        // alone would leave the rows a plain `SELECT * FROM parent` still
        // returns, which is not what "truncate" can be allowed to mean.
        if target.only {
            continue;
        }
        for child in crabgresql_binder::inheritance_descendants(engine, &table.schema())? {
            push(child);
        }
    }
    // Acquire the tables' exclusive locks in a deterministic order (by name), so
    // two concurrent multi-table TRUNCATEs can never deadlock, and drop duplicates
    // named twice in one statement.
    named.sort_by(|a, b| a.0.cmp(&b.0));
    named.dedup_by(|a, b| a.0 == b.0);
    if let Some((_, table)) = named
        .iter()
        .find(|(_, table)| !table.capabilities().truncate)
    {
        // Name the offending table's own method, not a hardcoded one — the guard
        // is generic over `capabilities()`, so the message must be too.
        let method = table.schema().access_method.as_str();
        return Err(PgError::feature_not_supported(format!(
            "table access method \"{method}\" does not support TRUNCATE"
        )));
    }
    // TRUNCATE is a write: run it under a real transaction so autocommit commits
    // it. On the durable heap engine this is fully transactional — the swap is
    // applied on commit and discarded on rollback (or a crash before commit).
    let txn = build_txn(txnmgr, session, true);
    for (_, table) in &named {
        // A failed TRUNCATE must still end the statement as a failure: an access
        // method whose `truncate` is fallible (Parquet stages a directory and flushes
        // WAL) may have left an earlier table in the list holding its exclusive
        // AccessExclusive hold, and only the abort's finalize hook releases it.
        if let Err(error) = table.truncate(&txn) {
            let _ = finalize_statement(txnmgr, session, &txn, true, false, None);
            return Err(error.into());
        }
    }
    finalize_statement(txnmgr, session, &txn, true, true, None)?;
    Ok(QueryResult::command("TRUNCATE TABLE"))
}

/// `ANALYZE [table [(column, ...)]]` — measure relations and record what the
/// planner and `pg_class.relpages`/`reltuples` report.
///
/// Runs as a **read**: it only scans rows, and the statistics it writes are
/// non-transactional, so they stand even if the surrounding transaction rolls
/// back. That is also why no read-only check applies — PostgreSQL 18.4 accepts
/// `ANALYZE` inside a `READ ONLY` transaction and still updates `reltuples`.
///
/// A bare `ANALYZE` covers every relation this session can reach, skipping ones
/// with no rows of their own (partitioned parents) and other sessions' temp
/// tables, which the engine refuses by reporting them as absent.
fn execute_analyze(
    engine: &Arc<dyn TableEngine>,
    txnmgr: &Arc<TransactionManager>,
    session: &mut Session,
    analyze: &ast::Analyze,
) -> Result<QueryResult, PgError> {
    if analyze.partitions.is_some() {
        return Err(PgError::feature_not_supported(
            "ANALYZE of a partition list is not supported yet",
        ));
    }
    if analyze.for_columns || !analyze.columns.is_empty() {
        return Err(PgError::feature_not_supported(
            "ANALYZE of a column list is not supported yet",
        ));
    }
    if analyze.cache_metadata || analyze.noscan || analyze.compute_statistics {
        return Err(PgError::feature_not_supported(
            "ANALYZE ... CACHE METADATA / NOSCAN / COMPUTE STATISTICS is not supported yet",
        ));
    }

    // Resolve every target before measuring any, so a typo in the second name
    // does not leave the first already analyzed.
    let targets: Vec<(String, String)> = match &analyze.table_name {
        Some(name) => {
            let name = object_name_to_table_name(name)?;
            let table = engine.open_table(&name)?;
            reject_partitioned_parent(&table, &name)?;
            vec![("public".to_string(), name)]
        }
        None => engine
            .relations()
            .into_iter()
            // A partitioned parent holds no rows of its own; PostgreSQL derives
            // its statistics from its children, which is not implemented here.
            .filter(|schema| schema.partition_scheme.is_none())
            .map(|schema| (schema.namespace, schema.name))
            .collect(),
    };

    let txn = build_txn(txnmgr, session, false);
    for (namespace, name) in &targets {
        match engine.analyze(namespace, name, &txn) {
            Ok(()) => {}
            // A bare ANALYZE walks every relation the engine lists, which
            // includes other sessions' temp tables; the engine reports those as
            // absent and they are simply skipped. A named target was already
            // resolved above, so it cannot land here.
            Err(StorageError::TableNotFound(_)) if analyze.table_name.is_none() => {}
            Err(error) => {
                finalize_statement(txnmgr, session, &txn, false, false, None)?;
                return Err(error.into());
            }
        }
    }
    finalize_statement(txnmgr, session, &txn, false, true, None)?;
    Ok(QueryResult::command("ANALYZE"))
}

/// `VACUUM [table]` — tidy a relation's storage.
///
/// For a relation with a RAM write buffer (a Parquet table) this is the explicit
/// flush: buffered rows become one durable chunk. For a heap relation it reclaims
/// versions dead to every snapshot.
///
/// Unlike `ANALYZE`, this cannot run inside a transaction block: the flush is its
/// own transaction, which is also why PostgreSQL raises `25001` here.
fn execute_vacuum(
    engine: &Arc<dyn TableEngine>,
    txnmgr: &Arc<TransactionManager>,
    session: &mut Session,
    vacuum: &ast::VacuumStatement,
) -> Result<QueryResult, PgError> {
    // PostgreSQL rejects a transaction block before it looks at any option, so
    // `BEGIN; VACUUM FULL t;` reports 25001 there and must here too — a client
    // keying retry logic on that SQLSTATE would otherwise see 0A000 for the same
    // violation.
    if session.tx_status == TransactionStatus::InTransaction {
        return Err(PgError::new(
            sqlstate::ACTIVE_SQL_TRANSACTION,
            "VACUUM cannot run inside a transaction block",
        ));
    }
    // Modifiers the grammar accepts but this implementation does not honor. A
    // `VACUUM FULL` that silently ran a plain vacuum would report success for work
    // it never did, so each is a stated gap instead. `ANALYZE` is honored below;
    // `VERBOSE` only adds progress messages, so accepting it silently is faithful
    // enough — the work it describes still happens.
    for (present, name) in [
        (vacuum.full, "FULL"),
        (vacuum.freeze, "FREEZE"),
        (vacuum.sort_only, "SORT ONLY"),
        (vacuum.delete_only, "DELETE ONLY"),
        (vacuum.reindex, "REINDEX"),
        (vacuum.recluster, "RECLUSTER"),
        (vacuum.boost, "BOOST"),
        (vacuum.threshold.is_some(), "TO ... PERCENT"),
    ] {
        if present {
            return Err(PgError::feature_not_supported(format!(
                "VACUUM {name} is not supported yet"
            )));
        }
    }

    // Resolve every target first, so a typo in the second name does not leave the
    // first already vacuumed.
    let targets: Vec<(String, String)> = match &vacuum.table_name {
        Some(name) => {
            let name = object_name_to_table_name(name)?;
            let table = engine.open_table(&name)?;
            // A partitioned parent holds no rows of its own. PostgreSQL recurses
            // into the children; `ANALYZE` here already rejects rather than
            // recursing, and the two disagreeing about the same object would be
            // worse than both being conservative.
            reject_partitioned_parent(&table, &name)?;
            vec![("public".to_string(), name)]
        }
        None => engine
            .relations()
            .into_iter()
            .filter(|schema| schema.partition_scheme.is_none())
            .map(|schema| (schema.namespace, schema.name))
            .collect(),
    };

    // Versions below the reclamation horizon are dead to every reader that exists
    // or can still be created, so they are safe to reclaim. Deliberately not
    // `snapshot().xmin` — see [`TransactionManager::reclaim_horizon`]. The
    // background flush worker chooses the same floor.
    let oldest = txnmgr.reclaim_horizon();
    let mut flushed = 0u64;
    for (namespace, name) in &targets {
        match engine.vacuum_table(namespace, name, oldest) {
            Ok(rows) => flushed += rows,
            // A bare VACUUM walks every relation the engine lists, including other
            // sessions' temp tables, which it reports as absent. A named target was
            // resolved above, so it cannot land here.
            Err(StorageError::TableNotFound(_)) if vacuum.table_name.is_none() => {}
            Err(error) => return Err(error.into()),
        }
    }
    if flushed > 0 {
        tracing::debug!(
            rows = flushed,
            "VACUUM flushed buffered rows to durable storage"
        );
    }
    // `VACUUM ANALYZE` is one statement that does both, as in PostgreSQL.
    if vacuum.analyze {
        let txn = build_txn(txnmgr, session, false);
        for (namespace, name) in &targets {
            match engine.analyze(namespace, name, &txn) {
                Ok(()) => {}
                Err(StorageError::TableNotFound(_)) if vacuum.table_name.is_none() => {}
                Err(error) => {
                    finalize_statement(txnmgr, session, &txn, false, false, None)?;
                    return Err(error.into());
                }
            }
        }
        finalize_statement(txnmgr, session, &txn, false, true, None)?;
    }
    Ok(QueryResult::command("VACUUM"))
}

/// Register a semantic index. It is reflected through the catalogs and UNIQUE
/// indexes validate existing and future rows; physical lookup/IndexScan waits
/// for the B-tree access-method milestone.
fn execute_create_index(
    engine: &Arc<dyn TableEngine>,
    txnmgr: &TransactionManager,
    session: &mut Session,
    create: &ast::CreateIndex,
) -> Result<QueryResult, PgError> {
    let method = match &create.using {
        None | Some(ast::IndexType::BTree) => IndexMethod::BTree,
        Some(ast::IndexType::Hash) => IndexMethod::Hash,
        Some(other) => {
            return Err(PgError::feature_not_supported(format!(
                "index access method \"{other}\" is not supported yet"
            )));
        }
    };
    if create.unique && method == IndexMethod::Hash {
        return Err(PgError::feature_not_supported(
            "access method \"hash\" does not support unique indexes",
        ));
    }
    if create.concurrently
        || !create.include.is_empty()
        || !create.with.is_empty()
        || !create.index_options.is_empty()
        || !create.alter_options.is_empty()
        || create.predicate.is_some()
    {
        return Err(PgError::feature_not_supported(
            "this CREATE INDEX form is not supported yet",
        ));
    }
    if create.nulls_distinct.is_some() && !create.unique {
        return Err(PgError::feature_not_supported(
            "NULLS [NOT] DISTINCT requires a unique index",
        ));
    }
    let table_name = object_name_to_table_name(&create.table_name)?;
    let explicit_name = create
        .name
        .as_ref()
        .map(object_name_to_table_name)
        .transpose()?;
    // Only an explicitly named index can collide; a generated one dodges existing
    // names by construction. Keeping this check ahead of opening the table also
    // preserves the error ordering the named form has always had.
    if let Some(index_name) = &explicit_name
        && engine.index_name_exists("public", &table_name, index_name)
    {
        if create.if_not_exists {
            let mut result = QueryResult::command("CREATE INDEX");
            if let QueryResult::Command { notices, .. } = &mut result {
                notices.push(Notice::notice(
                    format!("relation \"{index_name}\" already exists, skipping"),
                    None,
                ));
            }
            return Ok(result);
        }
        return Err(PgError::new(
            sqlstate::DUPLICATE_TABLE,
            format!("relation \"{index_name}\" already exists"),
        ));
    }
    let table = engine.open_table(&table_name)?;
    reject_partitioned_parent(&table, &table_name)?;
    let keys = simple_index_keys(&table.schema(), &create.columns)?;
    // PG names an unnamed index after the table and every key column, e.g.
    // `t_a_b_idx`, then bumps the label on collision (`t_a_b_idx1`).
    let index_name = explicit_name.unwrap_or_else(|| {
        let schema = table.schema();
        let mut base = table_name.clone();
        for key in &keys {
            base.push('_');
            base.push_str(&schema.columns[key.column].name);
        }
        base.push_str("_idx");
        fresh_index_name(engine, &table_name, &HashSet::new(), &base)
    });
    let index = IndexMetadata {
        name: index_name.clone(),
        method,
        keys,
        unique: create.unique,
        nulls_distinct: create.nulls_distinct.unwrap_or(true),
        constraint: None,
    };
    if create.unique {
        let txn = build_txn(txnmgr, session, false);
        validate_unique_index_build(&table, &index, &txn, &session.fmt_ctx())?;
    }
    engine.create_index("public", &table_name, index)?;
    Ok(QueryResult::command("CREATE INDEX"))
}

/// One `ADD CONSTRAINT` as pass 1 classified it, before the relation is even
/// resolved.
enum Requested {
    Index {
        name: Option<String>,
        columns: Vec<ast::IndexColumn>,
        kind: IndexConstraint,
        nulls_distinct: bool,
    },
    Check {
        name: Option<String>,
        expr: ast::Expr,
    },
}

/// One `ADD CONSTRAINT` resolved against the table but not yet applied.
enum PendingConstraint {
    /// The index to create, and — for a PRIMARY KEY — the key columns that are
    /// still nullable and therefore have to be marked NOT NULL.
    Index {
        index: IndexMetadata,
        not_null: Vec<usize>,
    },
    /// The relations the constraint lands on: the named one first, then every
    /// inheritance descendant, since PostgreSQL recurses `ADD CHECK` down the
    /// tree.
    Check { targets: Vec<CheckTarget> },
}

/// One relation an `ADD CHECK` applies to, with the constraint as it lands
/// *there* — a descendant's copy differs from the parent's in `conislocal` and
/// `coninhcount`, and its predicate is bound against its own column layout.
///
/// The bound predicate travels with the constraint so pass 4's scan cannot drift
/// from the text pass 5 stores.
struct CheckTarget {
    table: Arc<dyn TableAm>,
    namespace: String,
    relation: String,
    check: CheckConstraint,
    bound: crabgresql_binder::BoundExpr,
}

/// `ALTER TABLE ... ADD [CONSTRAINT n] {PRIMARY KEY|UNIQUE} (cols)`. Every other
/// `ALTER TABLE` action is rejected as unsupported.
///
/// Runs in passes — classify, resolve, plan, validate, apply — because PostgreSQL
/// applies a multi-action `ALTER TABLE` atomically. Our DDL is not transactional
/// (see the module's other `execute_*` handlers), so the nearest we get is to
/// raise every user-visible error before the first write and compensate for what
/// lands after it.
///
/// Two gaps that shape are **not** enough to close, both shared with
/// `CREATE INDEX` and neither introduced here:
///
/// * **`ROLLBACK` does not undo it.** The constraint is applied to the durable
///   catalog immediately. Worse, pass 4 validates under the session's own
///   snapshot, so `BEGIN; DELETE FROM t; ALTER TABLE t ADD PRIMARY KEY (a);
///   ROLLBACK;` validates against rows the rollback then brings back — leaving a
///   NULL, or a duplicate, under a key that forbids it.
/// * **No lock spans validation and application.** The exclusive hold lives
///   inside each engine call, so another session can commit a violating row in
///   between and neither session is told. PostgreSQL holds ACCESS EXCLUSIVE for
///   the whole statement.
///
/// Closing either needs machinery this tree does not have: a server-visible
/// table-lock API (`TableLock` is private to the access method, and neither
/// `TableAm` nor `TableEngine` exposes locking), and a transactional-DDL hook
/// general enough to stage a schema change — `TxnFinalize` today serves one
/// bespoke TRUNCATE registry.
/// `engine` is the session's resolution overlay (temp shadows global); every
/// catalog read and write below routes through it. `raw_engine` is the
/// underlying engine, which is what [`Session::exec_context_for_statement`] takes
/// on every other statement path — a CHECK predicate calling `nextval` advances
/// the shared counter, not a temp-shadowed one. The two are passed in already
/// built rather than derived here with `bind_catalogs`, because both are
/// `&Arc<dyn TableEngine>` and a single missed rename in this function's body
/// would be a silent temp-routing bug the compiler cannot catch.
#[allow(clippy::too_many_arguments)]
fn execute_alter_table(
    engine: &Arc<dyn TableEngine>,
    raw_engine: &Arc<dyn TableEngine>,
    type_catalog: &Arc<dyn TypeCatalog>,
    catalog_ops: &Arc<dyn CatalogOps>,
    global_catalog: &Arc<GlobalCatalog>,
    txnmgr: &TransactionManager,
    session: &mut Session,
    alter: &ast::AlterTable,
) -> Result<QueryResult, PgError> {
    if alter.table_type.is_some() || alter.location.is_some() || alter.on_cluster.is_some() {
        return Err(PgError::feature_not_supported(
            "this ALTER TABLE form is not supported yet",
        ));
    }

    // Pass 1: classify the actions. Nothing here touches the catalog, so an
    // unsupported action in the list rejects the whole statement before the
    // supported ones ahead of it have done anything.
    let mut requested = Vec::new();
    for op in &alter.operations {
        let ast::AlterTableOperation::AddConstraint {
            constraint,
            not_valid,
        } = op
        else {
            return Err(PgError::feature_not_supported(format!(
                "ALTER TABLE operation is not supported yet: {op}"
            )));
        };
        let (action, characteristics) = match constraint {
            ast::TableConstraint::PrimaryKey(pk) => {
                if *not_valid {
                    // PostgreSQL rejects this in its grammar; we reach it here
                    // because the vendored parser accepts the suffix uniformly.
                    return Err(PgError::new(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "PRIMARY KEY constraints cannot be marked NOT VALID",
                    ));
                }
                reject_primary_key_options(pk)?;
                (
                    Requested::Index {
                        name: pk.name.as_ref().map(normalize_ident),
                        columns: pk.columns.clone(),
                        kind: IndexConstraint::PrimaryKey,
                        nulls_distinct: true,
                    },
                    pk.characteristics,
                )
            }
            ast::TableConstraint::Unique(unique) => {
                if *not_valid {
                    return Err(PgError::new(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "UNIQUE constraints cannot be marked NOT VALID",
                    ));
                }
                reject_unique_options(unique)?;
                (
                    Requested::Index {
                        name: unique.name.as_ref().map(normalize_ident),
                        columns: unique.columns.clone(),
                        kind: IndexConstraint::Unique,
                        nulls_distinct: !matches!(
                            unique.nulls_distinct,
                            ast::NullsDistinctOption::NotDistinct
                        ),
                    },
                    unique.characteristics,
                )
            }
            ast::TableConstraint::Check(check) => {
                // PostgreSQL supports `NOT VALID` here — the constraint lands
                // unvalidated and a later `VALIDATE CONSTRAINT` scans. Neither
                // half exists yet, and accepting the suffix while validating
                // anyway would reject rows the user asked us to tolerate.
                if *not_valid {
                    return Err(PgError::feature_not_supported(
                        "CHECK constraints cannot be marked NOT VALID yet",
                    ));
                }
                reject_not_enforced(check.enforced)?;
                (
                    Requested::Check {
                        name: check.name.as_ref().map(normalize_ident),
                        expr: (*check.expr).clone(),
                    },
                    None,
                )
            }
            ast::TableConstraint::PrimaryKeyUsingIndex(_)
            | ast::TableConstraint::UniqueUsingIndex(_) => {
                return Err(PgError::feature_not_supported(
                    "ALTER TABLE ... USING INDEX is not supported yet",
                ));
            }
            other => {
                return Err(PgError::feature_not_supported(format!(
                    "table constraint is not supported yet: {other}"
                )));
            }
        };
        reject_deferred_characteristics(characteristics)?;
        requested.push(action);
    }

    // Pass 2: resolve the relation.
    let table_name = object_name_to_table_name(&alter.name)?;
    if engine.resolve_view(None, &table_name).is_some() {
        return Err(PgError::new(
            sqlstate::WRONG_OBJECT_TYPE,
            format!("ALTER action ADD CONSTRAINT cannot be performed on relation \"{table_name}\""),
        )
        .with_detail("This operation is not supported for views."));
    }
    let table = match engine.open_table(&table_name) {
        Ok(table) => table,
        Err(StorageError::TableNotFound(_)) if alter.if_exists => {
            let mut result = QueryResult::command("ALTER TABLE");
            if let QueryResult::Command { notices, .. } = &mut result {
                notices.push(Notice::notice(
                    format!("relation \"{table_name}\" does not exist, skipping"),
                    None,
                ));
            }
            return Ok(result);
        }
        Err(e) => return Err(e.into()),
    };
    reject_partitioned_parent(&table, &table_name)?;

    let schema = table.schema();
    let wants_primary_key = requested.iter().any(|r| {
        matches!(
            r,
            Requested::Index {
                kind: IndexConstraint::PrimaryKey,
                ..
            }
        )
    });

    // Only the heap can record a column as NOT NULL, so a PRIMARY KEY on any
    // other method is refused here rather than discovered from the engine in the
    // apply pass — which would report it after an earlier action had landed.
    // Named from the relation's own method, as the TRUNCATE guard above is, so
    // the message stays true when a third method appears. UNIQUE is unaffected:
    // it changes no column, and `CREATE UNIQUE INDEX` already works there.
    if wants_primary_key && schema.access_method != TableAccessMethod::Heap {
        let method = schema.access_method.as_str();
        return Err(PgError::feature_not_supported(format!(
            "PRIMARY KEY on a table using access method \"{method}\" is not supported yet"
        )));
    }

    // A PRIMARY KEY marks its key columns NOT NULL, and PostgreSQL pushes that
    // down the inheritance tree — flipping every descendant's matching column
    // and scanning its rows for NULLs. We have no fan-out for that, so a parent
    // is refused rather than given a key its children do not honor. A child with
    // no children of its own is fine: there is nothing below it to reach.
    //
    // Asked through the shared helper because descent must be namespace-exact
    // and transitive: matching a bare relation name would let an unrelated
    // `public.par` with children veto a key on a temp or schema-qualified `par`
    // that has none.
    if wants_primary_key && !crabgresql_binder::inheritance_descendants(engine, &schema)?.is_empty()
    {
        return Err(PgError::feature_not_supported(format!(
            "ADD PRIMARY KEY on \"{table_name}\", which has inheritance children, is not supported yet"
        )));
    }

    // PostgreSQL recurses `ADD CHECK` down the inheritance tree, giving every
    // descendant its own copy — which is what makes the constraint hold for rows
    // written through a child. Resolved here, before pass 3, so a descendant
    // that cannot take the constraint fails the statement before anything lands.
    let descendants = match requested
        .iter()
        .any(|r| matches!(r, Requested::Check { .. }))
    {
        true => crabgresql_binder::inheritance_descendants(engine, &schema)?,
        false => Vec::new(),
    };

    // `ONLY` asks for exactly the opposite of that recursion, and PostgreSQL
    // does not grant it: rather than leave the children unconstrained it refuses
    // the statement outright (42P16, no DETAIL — probed against 18.4). On a table
    // with no children `ONLY` is accepted and means nothing, which is why this
    // keys off `descendants` rather than off the presence of a CHECK: the guard
    // is silent exactly where PostgreSQL is silent.
    if alter.only && !descendants.is_empty() {
        return Err(PgError::new(
            sqlstate::INVALID_TABLE_DEFINITION,
            "constraint must be added to child tables too",
        ));
    }

    // The relations this statement reaches, for the `coninhcount` each
    // descendant records. Keyed on the *relation's own* namespace rather than
    // the `"public"` the apply pass writes through, because a temp parent's
    // children record their real `pg_temp_N` in `inherits`.
    let descendant_schemas: Vec<Arc<TableSchema>> =
        descendants.iter().map(|d| d.schema()).collect();
    let affected: HashSet<(&str, &str)> =
        std::iter::once((schema.namespace.as_str(), schema.name.as_str()))
            .chain(
                descendant_schemas
                    .iter()
                    .map(|s| (s.namespace.as_str(), s.name.as_str())),
            )
            .collect();

    // Pass 3: turn each action into the index it creates, raising every name,
    // column and cardinality error.
    //
    // Two namespaces, because PostgreSQL has two and they collide differently
    // (all probed against 18.4):
    //
    // * `claimed_relations` — the relation namespace an index enters. Starts
    //   empty; it holds only what *this statement* will create, and existing
    //   occupancy is asked of the engine. A collision here is 42P07
    //   `relation "c" already exists`.
    // * `claimed_constraints` — the relation's constraint namespace: its checks,
    //   its NOT NULL constraints, and its **index-backed** constraints. A
    //   collision is 42710 `constraint "c" for relation "t" already exists`.
    //
    // The `constraint.is_some()` filter is load-bearing: a *plain* index is a
    // relation but not a constraint, so `CREATE INDEX c ON t(y)` does not stop
    // `ADD CONSTRAINT c CHECK (...)`, and a plain index named `t_x_check` does
    // not push a generated check name to `t_x_check1`. Treating every index name
    // as a constraint name over-rejected both.
    let mut claimed_relations: HashSet<String> = HashSet::new();
    let mut pending: Vec<PendingConstraint> = Vec::new();
    let existing_primary_key = table
        .indexes()
        .iter()
        .any(|index| index.constraint == Some(IndexConstraint::PrimaryKey));
    let mut added_primary_key = false;
    let mut claimed_constraints: HashSet<String> = schema
        .checks
        .iter()
        .map(|c| c.name.clone())
        .chain(
            schema
                .columns
                .iter()
                .filter_map(|c| c.not_null_constraint.clone()),
        )
        .chain(
            table
                .indexes()
                .iter()
                .filter(|i| i.constraint.is_some())
                .map(|i| i.name.clone()),
        )
        .collect();
    for action in requested {
        let (explicit_name, columns, kind, nulls_distinct) = match action {
            Requested::Index {
                name,
                columns,
                kind,
                nulls_distinct,
            } => (name, columns, kind, nulls_distinct),
            Requested::Check { name, expr } => {
                let (bound, key_columns) =
                    crabgresql_binder::bind_check_constraint(&expr, &schema, type_catalog)?;
                let name = name.unwrap_or_else(|| {
                    fresh_local_name(
                        |c| claimed_constraints.contains(c),
                        &check_name_base(&schema, &key_columns),
                    )
                });
                if !claimed_constraints.insert(name.clone()) {
                    return Err(PgError::new(
                        "42710",
                        format!(
                            "constraint \"{name}\" for relation \"{table_name}\" already exists"
                        ),
                    ));
                }
                let source = expr.to_string();
                let text = crabgresql_binder::ruleutils::deparse_check_expr(
                    &source,
                    &table_name,
                    type_catalog,
                )
                .unwrap_or(source);
                // Descendants bind the *canonical* text, not the source: the
                // source may qualify its columns with the parent's name, which
                // no child's scope answers to. Deparsing has already dropped
                // that qualifier, so this is the only form that re-binds
                // everywhere — the same reason `rebind_check_columns` works off
                // stored text on the CREATE TABLE path.
                let canonical = crabgresql_binder::ruleutils::parse_expression(&text);
                let mut targets = vec![CheckTarget {
                    table: Arc::clone(&table),
                    namespace: "public".to_string(),
                    relation: table_name.clone(),
                    check: CheckConstraint {
                        name: name.clone(),
                        expr: text.clone(),
                        columns: key_columns,
                        validated: true,
                        islocal: true,
                        inhcount: 0,
                    },
                    bound,
                }];
                for child in &descendants {
                    let child_schema = child.schema();
                    // A descendant that already carries this name would need its
                    // `coninhcount` bumped instead of a second row, and nothing
                    // here can express that — so it is refused rather than
                    // silently left with whichever predicate it already had.
                    if child_schema.checks.iter().any(|c| c.name == name) {
                        return Err(PgError::feature_not_supported(format!(
                            "ADD CHECK would collide with constraint \"{name}\" already on \
                             inheritance child \"{}\"",
                            child_schema.name
                        )));
                    }
                    // Bound against the *child's* layout: an inheritance child
                    // may carry extra columns of its own, so the parent's
                    // positions do not transfer.
                    let (child_bound, child_columns) = crabgresql_binder::bind_check_constraint(
                        canonical.as_ref().unwrap_or(&expr),
                        &child_schema,
                        type_catalog,
                    )?;
                    // `coninhcount` counts the links *into the set this statement
                    // touches*, so a diamond descendant reached through two
                    // parents records 2 (probed: three parents give 3).
                    // `inheritance_descendants` is transitive and deduplicated,
                    // emitting one target per relation, so the count has to be
                    // recovered from the descendant's own parent list. A
                    // descendant is in that set only via a parent that is also in
                    // it, so 1 is the floor a torn catalog degrades to.
                    let inhcount = child_schema
                        .inherits
                        .iter()
                        .filter(|p| affected.contains(&(p.namespace.as_str(), p.name.as_str())))
                        .count()
                        .max(1) as i16;
                    // One notice per link beyond the first, as PostgreSQL emits.
                    for _ in 1..inhcount {
                        session.notices.push(Notice::notice(
                            format!("merging constraint \"{name}\" with inherited definition"),
                            None,
                        ));
                    }
                    targets.push(CheckTarget {
                        table: Arc::clone(child),
                        namespace: child_schema.namespace.clone(),
                        relation: child_schema.name.clone(),
                        check: CheckConstraint {
                            name: name.clone(),
                            expr: text.clone(),
                            columns: child_columns,
                            validated: true,
                            islocal: false,
                            inhcount,
                        },
                        bound: child_bound,
                    });
                }
                pending.push(PendingConstraint::Check { targets });
                continue;
            }
        };
        let keys = simple_index_keys(&schema, &columns)?;
        if kind == IndexConstraint::PrimaryKey {
            if existing_primary_key || added_primary_key {
                return Err(PgError::new(
                    "42P16",
                    format!("multiple primary keys for table \"{table_name}\" are not allowed"),
                ));
            }
            added_primary_key = true;
        }
        let index_name = match explicit_name {
            // Both namespaces, in PostgreSQL's own order: the index has to enter
            // the relation namespace, and that is what fails first, so an
            // existing *relation* of that name (a plain index, a table) is 42P07.
            // Only if the name is free there does the constraint namespace get a
            // say, and a name already held by a CHECK or a NOT NULL is 42710 —
            // both probed against 18.4.
            Some(name) => {
                if claimed_relations.contains(&name)
                    || engine.index_name_exists("public", &table_name, &name)
                    || engine.resolve(Some("public"), &name).is_ok()
                {
                    return Err(PgError::new(
                        sqlstate::DUPLICATE_TABLE,
                        format!("relation \"{name}\" already exists"),
                    ));
                }
                if claimed_constraints.contains(&name) {
                    return Err(PgError::new(
                        sqlstate::DUPLICATE_OBJECT,
                        format!(
                            "constraint \"{name}\" for relation \"{table_name}\" already exists"
                        ),
                    ));
                }
                name
            }
            None => {
                let base = constraint_index_base(&table_name, &schema, kind, &keys);
                fresh_index_name(engine, &table_name, &claimed_relations, &base)
            }
        };
        // An index-backed constraint occupies both namespaces.
        claimed_relations.insert(index_name.clone());
        claimed_constraints.insert(index_name.clone());
        // Only columns that are still nullable need the catalog rewrite; one
        // already NOT NULL is left alone, which is what makes re-running the
        // same ALTER TABLE after a crash idempotent.
        let not_null = match kind {
            IndexConstraint::PrimaryKey => keys
                .iter()
                .map(|key| key.column)
                .filter(|&c| schema.columns[c].nullable)
                .collect(),
            IndexConstraint::Unique => Vec::new(),
        };
        pending.push(PendingConstraint::Index {
            index: IndexMetadata {
                name: index_name,
                method: IndexMethod::BTree,
                keys,
                unique: true,
                nulls_distinct,
                constraint: Some(kind),
            },
            not_null,
        });
    }

    // Pass 4: check the rows already in the table. Uniqueness first: PostgreSQL
    // builds the index before it verifies not-null, so a table holding both a
    // duplicate and a NULL reports the duplicate — the opposite of the order the
    // same two constraints are reported in on INSERT.
    let txn = build_txn(txnmgr, session, false);
    // A CHECK predicate may call anything DML may call — the binder deliberately
    // admits volatile and user-defined functions — so the scan needs the same
    // fully-wired runtime a statement gets, not the bare formatting context.
    // `session.exec_context()` leaves `sequences`/`routines`/`catalog` unset, and
    // a predicate touching any of them then failed the ALTER with an internal
    // error on a non-empty table while succeeding on an empty one.
    //
    // `build_txn(.., false)` allocates no XID, so a routine that *writes* still
    // cannot stamp rows from here; giving ALTER an XID is a separate decision.
    let (routines, command_counter) =
        statement_runtime(engine, type_catalog, global_catalog, session);
    let exec_ctx = ExecContext {
        // Attached as `execute_call` does: these expressions never travel
        // through `execute`, which is what otherwise injects the transaction.
        txn: Some(txn.clone()),
        ..session.exec_context_for_statement(
            raw_engine,
            catalog_ops,
            type_catalog,
            Arc::clone(&routines),
            Arc::clone(&command_counter),
            read_only_active(session),
        )
    };
    // Passes 4 and 5 run inside a closure so the command counter is settled
    // exactly once on the way out, whichever pass failed: a routine called from
    // a predicate advances it, and dropping that leaves the next statement in an
    // explicit block reusing a command id these rows were already stamped with.
    let outcome = (|| -> Result<(), PgError> {
        for p in &pending {
            match p {
                PendingConstraint::Index { index, .. } => {
                    validate_unique_index_build(&table, index, &txn, &session.fmt_ctx())?;
                    if index.constraint == Some(IndexConstraint::PrimaryKey) {
                        validate_primary_key_not_null(&table, index, &txn)?;
                    }
                }
                // Every target is scanned, descendants included: a child holding a
                // row the parent's new constraint rejects must fail the statement
                // before the parent's copy lands.
                PendingConstraint::Check { targets } => {
                    for t in targets {
                        validate_check_constraint(
                            &t.table,
                            &t.relation,
                            &t.check,
                            &t.bound,
                            &txn,
                            &exec_ctx,
                        )?;
                    }
                }
            }
        }

        // Pass 5: apply. NOT NULL is made durable before the index for the reason
        // spelled out on `PgEngine::set_column_not_null`: a key resting on columns
        // that still accept NULL is the dangerous half to lose.
        let mut created: Vec<String> = Vec::new();
        for p in pending {
            let (applied, index_name) = match p {
                PendingConstraint::Index { index, not_null } => {
                    let applied = (|| {
                        if !not_null.is_empty() {
                            engine.set_column_not_null("public", &table_name, &not_null)?;
                        }
                        engine.create_index("public", &table_name, index.clone())
                    })();
                    (applied, Some(index.name))
                }
                // A landed check is not undone by the compensation below: like the
                // NOT NULL flips, it is the conservative direction — it rejects
                // strictly more rows, and re-running the statement re-converges.
                // That also covers a partly-applied fan-out, where the parent has
                // the constraint and some descendant does not yet.
                PendingConstraint::Check { targets } => {
                    let applied = targets.into_iter().try_for_each(|t| {
                        engine.add_check_constraint(&t.namespace, &t.relation, t.check)
                    });
                    (applied, None)
                }
            };
            if let Err(e) = applied {
                // Passes 1-4 raise everything a statement can be wrong about, so what
                // reaches here is an engine or IO failure. Undo the indexes that did
                // land; the NOT NULL flips are deliberately left in place, being the
                // conservative direction (they reject strictly more rows, and
                // re-running the statement re-converges).
                //
                // The drop goes through the same handle the create did, so it lands
                // on the same relation — an overlay routing `public` to this
                // session's temp schema must undo the index it put there, not look
                // for it under `public`.
                for name in created.iter().rev() {
                    let _ = engine.drop_index("public", &table_name, name);
                }
                return Err(e.into());
            }
            created.extend(index_name);
        }
        Ok(())
    })();
    finalize_statement(
        txnmgr,
        session,
        &txn,
        false,
        outcome.is_ok(),
        Some(&command_counter),
    )?;
    outcome?;
    Ok(QueryResult::command("ALTER TABLE"))
}

/// Reject `ADD PRIMARY KEY` on a table whose key columns already hold a NULL:
/// the key would make them NOT NULL, and existing rows have to satisfy that.
///
/// PostgreSQL names the first offending *row* in physical order and, within it,
/// the lowest-numbered column rather than the first in key order — so `PRIMARY
/// KEY (b, a)` over a row `(1, NULL)` complains about `b`. Sorting the key
/// columns before the per-row test is what reproduces that.
fn validate_primary_key_not_null(
    table: &Arc<dyn TableAm>,
    index: &IndexMetadata,
    txn: &TxnContext,
) -> Result<(), PgError> {
    let schema = table.schema();
    let mut columns: Vec<usize> = index.keys.iter().map(|key| key.column).collect();
    columns.sort_unstable();
    let projection = crabgresql_storage_api::ColumnProjection::of(columns.iter().copied(), &schema);
    for row in table.scan(txn, &projection) {
        let (_, tuple) = row?;
        for &c in &columns {
            if matches!(tuple[c], crabgresql_types::Value::Null) {
                return Err(PgError::new(
                    "23502",
                    format!(
                        "column \"{}\" of relation \"{}\" contains null values",
                        schema.columns[c].name, schema.name
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Reject `ALTER TABLE ... ADD CONSTRAINT ... CHECK` when a row already in the
/// table fails the new predicate.
///
/// Reads only the predicate's own columns. A projection narrows the *work*, not
/// the row shape — tuples stay full width and pruned slots come back as
/// placeholders — so `check.columns`, which are schema ordinals produced by
/// `bind_check_constraint`, still index the tuple directly. That is the same
/// contract [`validate_primary_key_not_null`] and [`validate_unique_index_build`]
/// rely on, and it matters here: a full-width scan would detoast every column of
/// every row for a predicate reading one.
///
/// PostgreSQL's message here names the *constraint and relation* rather than the
/// offending row, and carries **no DETAIL** — unlike the DML-time violation,
/// which prints `Failing row contains (…)`. Probed against 18.4. A predicate
/// evaluating to NULL passes, as at DML time.
fn validate_check_constraint(
    table: &Arc<dyn TableAm>,
    table_name: &str,
    check: &CheckConstraint,
    bound: &crabgresql_binder::BoundExpr,
    txn: &TxnContext,
    ctx: &crabgresql_executor::ExecContext,
) -> Result<(), PgError> {
    let schema = table.schema();
    let projection =
        crabgresql_storage_api::ColumnProjection::of(check.columns.iter().copied(), &schema);
    for row in table.scan(txn, &projection) {
        let (_, tuple) = row?;
        if matches!(
            crabgresql_executor::eval::eval(bound, &tuple, ctx)?,
            crabgresql_types::Value::Bool(false)
        ) {
            return Err(PgError::new(
                "23514",
                format!(
                    "check constraint \"{}\" of relation \"{table_name}\" is violated by some row",
                    check.name
                ),
            ));
        }
    }
    Ok(())
}

fn validate_unique_index_build(
    table: &Arc<dyn TableAm>,
    index: &IndexMetadata,
    txn: &TxnContext,
    fmt: &FmtCtx,
) -> Result<(), PgError> {
    let schema = table.schema();
    // Only the index's own key columns are ever read below — for the duplicate
    // check and the error DETAIL alike.
    let projection = crabgresql_storage_api::ColumnProjection::of(
        index.keys.iter().map(|key| key.column),
        &schema,
    );
    let rows = table
        .scan(txn, &projection)
        .map(|row| row.map(|(_, tuple)| tuple));
    let Some(key) = find_duplicate(rows, &schema, index)? else {
        return Ok(());
    };
    let names = index
        .keys
        .iter()
        .map(|key| schema.columns[key.column].name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    // `key` holds one value per index key, in `index.keys` order, so the names
    // and the values are rendered from the same walk.
    let values = key
        .iter()
        .map(|value| {
            value
                .encode_text_with(fmt)
                .unwrap_or_else(|| "null".to_string())
        })
        .collect::<Vec<_>>()
        .join(", ");
    Err(PgError::new(
        "23505",
        format!("could not create unique index \"{}\"", index.name),
    )
    .with_detail(format!("Key ({names})=({values}) is duplicated.")))
}

/// How many keys accumulate before the run is sorted and checked on its own.
///
/// The runs are what give the check back its fail-fast: without them the whole
/// relation has to be read before any duplicate can be reported. They are not
/// free real estate either — a sorted run is only a *cheap* run for the final
/// sort while it stays long relative to the input (Rust's `sort_by` extends
/// shorter ones rather than merging them as they are), which puts the break-even
/// for a fixed 8192 at around 67M rows. Below that the two sorts together cost
/// what the single sort cost: `n·log(n) = n·log(run) + n·log(n/run)`.
const EARLY_RUN: usize = 8192;

/// The key of one row that repeats another's, or `None` when every key is
/// unique. The values are in `index.keys` order, which is the order the caller
/// names the columns in.
///
/// Split out from [`validate_unique_index_build`] so the strategy is separable
/// from the error: the caller owns the SQLSTATE, the column names and the
/// rendered values.
///
/// Sorted, not hashed — which is what PostgreSQL, InnoDB, SQLite, SQL Server
/// and Oracle all do. Equality here *is* an ordering comparison, so [`key_cmp`]
/// makes the two agree by construction; a hash would be a second definition of
/// key equality to keep in lockstep with the first, and a divergence between
/// them is a duplicate silently let through, not an error. A bulk-load build
/// wants the same shape — but not this buffer: the stream here has dropped the
/// heap `Tid`, which is both a leaf item's payload and its tiebreak, and it
/// orders by `compare_values` rather than by the `btkey` bytes the tree is laid
/// out in (which cannot encode `numeric` or `bpchar` at all). Such a build would
/// re-sort on `(key bytes, tid)`; what it inherits from here is the argument,
/// not the code.
///
/// Which duplicate is named is deliberately left unpinned, and two things move
/// it: the runs (a pair inside one run is reported before the whole relation is
/// even read) and, past that, ascending key order with NULLs first — *not* the
/// index's own order, which [`key_cmp`] does not reproduce. For a key whose
/// equal values are not identical (`numeric` `1.0` and `1.00`, `bpchar` `'ab'`
/// and `'ab  '`) that also decides which of the two representations is rendered.
/// PostgreSQL pins none of this either: it finds the collision inside its build
/// sort, so its answer follows neither scan nor sort order — probed on 18.4, a
/// heap of `1, 100..10000, 1, 50, 50` reports `50` even though the pair of `1`s
/// completes first.
///
/// Memory is one key per surviving row — strictly less than the linear
/// predecessor, which retained the full-width tuple. The fail-fast is narrower
/// than it sounds, though, and worth stating plainly: a duplicate short-circuits
/// the scan only when its two rows land in the same run. A pair further apart
/// than that still reads the relation to the end, so this bounds the common case
/// (a table dense with duplicates) rather than every case.
fn find_duplicate(
    rows: impl Iterator<Item = Result<crabgresql_storage_api::Tuple, StorageError>>,
    schema: &TableSchema,
    index: &IndexMetadata,
) -> Result<Option<Vec<crabgresql_types::Value>>, StorageError> {
    find_duplicate_in_runs(rows, schema, index, EARLY_RUN)
}

/// [`find_duplicate`] with the run length exposed, so a test can reach the
/// short-circuit without materializing [`EARLY_RUN`] rows.
fn find_duplicate_in_runs(
    rows: impl Iterator<Item = Result<crabgresql_storage_api::Tuple, StorageError>>,
    schema: &TableSchema,
    index: &IndexMetadata,
    run: usize,
) -> Result<Option<Vec<crabgresql_types::Value>>, StorageError> {
    let tys: Vec<PgType> = index
        .keys
        .iter()
        .map(|key| schema.columns[key.column].ty)
        .collect();
    // Moving the value out of the row costs no allocation, but it leaves a hole,
    // so it is only sound while each column is read once. A repeated key column
    // is legal — PostgreSQL accepts `UNIQUE (a, a)` and renders it
    // `Key (a, a)=(5, 5)` — and would read that hole. Decided once, not per row.
    let distinct_columns = index
        .keys
        .iter()
        .map(|key| key.column)
        .collect::<HashSet<_>>()
        .len()
        == index.keys.len();
    let mut keyed: Vec<Vec<crabgresql_types::Value>> = Vec::new();
    let mut sorted = 0;
    let mut runs = 0;
    for row in rows {
        let mut tuple = row?;
        // A NULL key is exempt under the default NULLS DISTINCT, so it never
        // reaches the sort. Under NULLS NOT DISTINCT it does, and the ordering
        // below groups NULLs together so two of them land adjacent. The count
        // that drives the runs is therefore of surviving rows, not scanned ones.
        if index.nulls_distinct
            && index
                .keys
                .iter()
                .any(|key| matches!(tuple[key.column], crabgresql_types::Value::Null))
        {
            continue;
        }
        // Rows arrive under `ColumnProjection::of`, i.e. full width with `Null`
        // padding outside the key — which is why this indexes by `key.column`
        // and everything downstream can stop caring about the table's shape.
        keyed.push(
            index
                .keys
                .iter()
                .map(|key| {
                    if distinct_columns {
                        std::mem::replace(&mut tuple[key.column], crabgresql_types::Value::Null)
                    } else {
                        tuple[key.column].clone()
                    }
                })
                .collect(),
        );
        if keyed.len() - sorted >= run {
            if let Some(duplicate) = sort_and_scan(&mut keyed[sorted..], &tys) {
                return Ok(Some(duplicate));
            }
            sorted = keyed.len();
            runs += 1;
        }
    }
    // Exactly one run and nothing pushed after it: the vector is already sorted
    // and already scanned. The test has to be on the run *count* — `sorted ==
    // keyed.len()` alone would also hold for two runs, and skipping there would
    // skip the merge that finds a pair split across them.
    if runs == 1 && sorted == keyed.len() {
        return Ok(None);
    }
    Ok(sort_and_scan(&mut keyed, &tys))
}

/// Sort `keys` and return the second member of the first equal-adjacent pair.
fn sort_and_scan(
    keys: &mut [Vec<crabgresql_types::Value>],
    tys: &[PgType],
) -> Option<Vec<crabgresql_types::Value>> {
    keys.sort_by(|left, right| key_cmp(tys, left, right));
    keys.windows(2)
        .find(|pair| key_cmp(tys, &pair[0], &pair[1]).is_eq())
        .map(|pair| pair[1].clone())
}

/// Order two index keys, for the sole purpose of making equal ones adjacent.
///
/// `descending` and `nulls_first` are ignored — hence `tys` rather than the
/// keys themselves: reversing a column or moving the NULLs to the other end
/// permutes the result but cannot change which keys are neighbours, and
/// adjacency is all [`find_duplicate`] reads. Equality is unchanged from the
/// linear scan this replaced — the same `compare_values` call, judged with
/// `is_eq`.
fn key_cmp(
    tys: &[PgType],
    left: &[crabgresql_types::Value],
    right: &[crabgresql_types::Value],
) -> std::cmp::Ordering {
    // Indexed rather than zipped: `zip` stops at the shortest, so a `tys` short
    // by one would silently call two different keys equal — a spurious 23505
    // refusing a legitimate index, which is the very failure the sorted-not-
    // hashed argument above exists to rule out.
    debug_assert_eq!(tys.len(), left.len());
    debug_assert_eq!(tys.len(), right.len());
    for (i, ty) in tys.iter().enumerate() {
        let ordering = match (&left[i], &right[i]) {
            (crabgresql_types::Value::Null, crabgresql_types::Value::Null) => {
                std::cmp::Ordering::Equal
            }
            (crabgresql_types::Value::Null, _) => std::cmp::Ordering::Less,
            (_, crabgresql_types::Value::Null) => std::cmp::Ordering::Greater,
            (left, right) => crabgresql_executor::compare_values(*ty, left, right),
        };
        if ordering.is_ne() {
            return ordering;
        }
    }
    std::cmp::Ordering::Equal
}

/// `SET`. Parameters are resolved through the [`crate::guc`] table; the
/// `SET TRANSACTION` family is handled separately, and an unrecognized name is
/// accepted and ignored for driver compatibility (see the `guc` module header).
///
/// The parser produces *two* shapes for the zone: `SET TIME ZONE 'x'` becomes
/// [`ast::Set::SetTimeZone`], while `SET timezone TO 'x'` and
/// `SET TIME ZONE = 'x'` are rewritten into a [`ast::Set::SingleAssignment`]
/// whose variable is the literal string `TIMEZONE`. Both must be handled.
fn apply_set(set: &ast::Set, session: &mut Session) -> Result<QueryResult, PgError> {
    let (name, value, scope) = match set {
        ast::Set::SetTransaction {
            modes,
            snapshot,
            session: is_session,
        } => {
            // `SET SESSION CHARACTERISTICS` writes the same two parameters a
            // plain `SET` of them writes, so it has to go through the same save
            // stack — but only for the modes it actually names, and only once
            // the statement can no longer fail. Both live in
            // `apply_set_transaction`, which is where that is known.
            return apply_set_transaction(session, modes, snapshot.is_some(), *is_session);
        }
        ast::Set::SingleAssignment {
            variable,
            values,
            scope,
            ..
        } => {
            let Some(name) = single_ident_lower(variable) else {
                // A qualified name (`plpgsql.variable_conflict`) names no
                // parameter we model; accept and ignore.
                return Ok(QueryResult::command("SET"));
            };
            let Some(value) = set_value(&name, values)? else {
                return Ok(QueryResult::command("SET"));
            };
            (name, value, *scope)
        }
        ast::Set::SetTimeZone { local, value } => {
            let scope = local.then_some(ast::ContextModifier::Local);
            ("timezone".to_string(), timezone_value(value)?, scope)
        }
        // SET ROLE / SET NAMES / SESSION AUTHORIZATION: accepted and ignored.
        _ => return Ok(QueryResult::command("SET")),
    };

    let Some(def) = guc::lookup(&name) else {
        return Ok(QueryResult::command("SET"));
    };

    let local = scope == Some(ast::ContextModifier::Local);
    let mut notices = Vec::new();
    if local && session.tx_status == TransactionStatus::Idle {
        // PG warns and does nothing: there is no transaction for the setting to
        // be local to.
        notices.push(Notice::warning(
            sqlstate::NO_ACTIVE_SQL_TRANSACTION,
            "SET LOCAL can only be used in transaction blocks",
        ));
        return Ok(QueryResult::Command {
            tag: "SET".into(),
            notices,
        });
    }
    session.assign_guc(def, value, local)?;
    Ok(QueryResult::Command {
        tag: "SET".into(),
        notices,
    })
}

/// Reduce a `SET x = <value>` operand list to a [`guc::GucValue`]. `DEFAULT`
/// (the bare keyword) means the boot value.
///
/// `name` selects the sign convention for a bare number: `TimeZone` reads one as
/// an east-signed hour offset in *both* statement spellings (`SET TIME ZONE 7`
/// and `SET timezone TO 7` agree in PG), which is the opposite of how it reads a
/// numeric *string* — see [`guc`].
fn set_value(name: &str, values: &[ast::Expr]) -> Result<Option<guc::GucValue>, PgError> {
    if is_set_default(values) {
        return Ok(Some(guc::GucValue::Default));
    }
    if name == "timezone"
        && let [expr] = values
        && let Some(secs) = numeric_hours_east(expr)
    {
        return offset_east(secs?).map(Some);
    }
    Ok(set_value_to_string(values).map(guc::GucValue::Str))
}

/// Reduce a `SET TIME ZONE <expr>` operand.
///
/// `DEFAULT` and `LOCAL` both restore the boot value. A bare number and an
/// `INTERVAL` are east-signed hour offsets; a quoted string is POSIX-signed and
/// is passed through as-is (see [`guc`]).
fn timezone_value(value: &ast::Expr) -> Result<guc::GucValue, PgError> {
    if let Some(secs) = numeric_hours_east(value) {
        return offset_east(secs?);
    }
    match value {
        ast::Expr::Identifier(ident)
            if ident.quote_style.is_none()
                && (ident.value.eq_ignore_ascii_case("default")
                    || ident.value.eq_ignore_ascii_case("local")) =>
        {
            Ok(guc::GucValue::Default)
        }
        ast::Expr::Value(v) => v
            .value
            .as_pg_string()
            .map(|s| guc::GucValue::Str(s.to_string()))
            .ok_or_else(unsupported_timezone_value),
        ast::Expr::Identifier(ident) => Ok(guc::GucValue::Str(ident.value.clone())),
        _ => Err(unsupported_timezone_value()),
    }
}

/// The east-signed offset an expression denotes, if it is one of the numeric
/// `TimeZone` forms: a bare number, a signed number, or `INTERVAL '…'`.
/// `None` means "not a numeric form" (try the string path); `Some(Err(..))`
/// means it was one and was malformed.
fn numeric_hours_east(expr: &ast::Expr) -> Option<Result<i32, PgError>> {
    match expr {
        ast::Expr::Value(v) => match &v.value {
            ast::Value::Number(n, _) => Some(hours_to_secs(n, 1)),
            _ => None,
        },
        ast::Expr::UnaryOp {
            op: op @ (ast::UnaryOperator::Minus | ast::UnaryOperator::Plus),
            expr,
        } => {
            let sign = if matches!(op, ast::UnaryOperator::Minus) {
                -1
            } else {
                1
            };
            match expr.as_ref() {
                ast::Expr::Value(v) => match &v.value {
                    ast::Value::Number(n, _) => Some(hours_to_secs(n, sign)),
                    _ => None,
                },
                _ => None,
            }
        }
        // `SET TIME ZONE INTERVAL '-08:00' HOUR TO MINUTE` — the spelling in
        // PG's own documentation. The qualifiers only bound which fields may
        // appear, so the value text carries the whole offset.
        ast::Expr::Interval(interval) => {
            let ast::Expr::Value(v) = interval.value.as_ref() else {
                return Some(Err(unsupported_timezone_value()));
            };
            let text = v.value.as_pg_string()?;
            Some(
                crabgresql_types::interval::parse(text)
                    .map_err(|e| PgError::new(e.sqlstate, e.message))
                    .and_then(|iv| {
                        // Months and days have no fixed length in seconds, so PG
                        // rejects them here rather than guessing.
                        if iv.months != 0 || iv.days != 0 {
                            return Err(invalid_timezone_value(text));
                        }
                        i32::try_from(iv.usec / 1_000_000).map_err(|_| invalid_timezone_value(text))
                    }),
            )
        }
        _ => None,
    }
}

/// Parse an hour count into seconds east. Fractional hours are allowed
/// (`SET TIME ZONE 167.5` is `+167:30` in PG).
fn hours_to_secs(digits: &str, sign: i32) -> Result<i32, PgError> {
    let hours: f64 = digits.parse().map_err(|_| invalid_timezone_value(digits))?;
    let secs = (hours * 3600.0).round();
    if !secs.is_finite() || secs.abs() > i32::MAX as f64 {
        return Err(invalid_timezone_value(digits));
    }
    Ok(sign * secs as i32)
}

/// Build the zone for an east-signed offset, mapping the range rejection to
/// PG's message. The bound itself lives in `SessionZone::from_offset_east`.
fn offset_east(secs: i32) -> Result<guc::GucValue, PgError> {
    if crabgresql_types::tz::SessionZone::from_offset_east(secs).is_err() {
        return Err(PgError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!(
                "invalid value for parameter \"TimeZone\": \"{}\"",
                secs / 3600
            ),
        )
        .with_detail("UTC timezone offset is out of range."));
    }
    Ok(guc::GucValue::OffsetSecondsEast(secs))
}

fn invalid_timezone_value(value: &str) -> PgError {
    PgError::new(
        sqlstate::INVALID_PARAMETER_VALUE,
        format!("invalid value for parameter \"TimeZone\": \"{value}\""),
    )
}

fn unsupported_timezone_value() -> PgError {
    PgError::feature_not_supported("this SET TIME ZONE value form is not supported yet")
}

/// `SET TRANSACTION …` (the current transaction) or `SET SESSION CHARACTERISTICS
/// AS TRANSACTION …` (the session default). Both feed the isolation level and
/// access mode into the same [`ActiveTxn`] / session-default state a BEGIN block
/// reads, so no new visibility machinery is needed.
fn apply_set_transaction(
    session: &mut Session,
    modes: &[ast::TransactionMode],
    has_snapshot: bool,
    is_session: bool,
) -> Result<QueryResult, PgError> {
    if has_snapshot {
        return Err(PgError::feature_not_supported(
            "SET TRANSACTION SNAPSHOT is not supported yet",
        ));
    }
    let (iso, read_only) = apply_modes(modes)?;
    if is_session {
        // SET SESSION CHARACTERISTICS: change the defaults new blocks inherit.
        // Each mode is applied through the same seam a plain `SET` of the
        // parameter uses, so the two spellings agree about being transactional
        // and about `pg_settings.source`. Only the modes the statement actually
        // named are touched — `AS TRANSACTION READ ONLY` leaves the isolation
        // level's source at `default`, as in PG — and nothing is recorded until
        // `has_snapshot` and `apply_modes` above have both had their chance to
        // fail.
        if let Some(iso) = iso {
            let def = guc::lookup("default_transaction_isolation")
                .ok_or_else(|| guc::unrecognized("default_transaction_isolation"))?;
            session.assign_guc_with(def, false, true, |s| {
                s.default_iso = iso;
                Ok(())
            })?;
        }
        if let Some(read_only) = read_only {
            let def = guc::lookup("default_transaction_read_only")
                .ok_or_else(|| guc::unrecognized("default_transaction_read_only"))?;
            session.assign_guc_with(def, false, true, |s| {
                s.default_read_only = read_only;
                Ok(())
            })?;
        }
        return Ok(QueryResult::command("SET"));
    }
    // Current transaction. Outside a block PG only WARNs and still succeeds; a
    // failed block is already rejected upstream (25P02), so the non-block case
    // reaching here is Idle.
    let Some(active) = session.xact.as_mut() else {
        return Ok(QueryResult::Command {
            tag: "SET".into(),
            notices: vec![Notice::warning(
                sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                "SET TRANSACTION can only be used in transaction blocks",
            )],
        });
    };
    // Only the isolation level is snapshot-gated: PG rejects a post-query
    // ISOLATION LEVEL change with 25001 but still lets READ ONLY/WRITE change any
    // time in the block.
    if iso.is_some() && active.has_run_query {
        return Err(PgError::new(
            sqlstate::ACTIVE_SQL_TRANSACTION,
            "SET TRANSACTION ISOLATION LEVEL must be called before any query",
        ));
    }
    if let Some(iso) = iso {
        active.iso = iso;
    }
    if let Some(read_only) = read_only {
        active.read_only = read_only;
    }
    Ok(QueryResult::command("SET"))
}

/// Whether a `SET var = value` names the literal `DEFAULT` keyword, which resets
/// the GUC to its boot value.
fn is_set_default(values: &[ast::Expr]) -> bool {
    matches!(
        values,
        [ast::Expr::Identifier(ident)]
            if ident.quote_style.is_none() && ident.value.eq_ignore_ascii_case("default")
    )
}

/// The single scalar value of a `SET var = value`, rendered as a string. Covers
/// the literal and bare-identifier forms both `default_transaction_*` GUCs use.
fn set_value_to_string(exprs: &[ast::Expr]) -> Option<String> {
    let [expr] = exprs else {
        return None;
    };
    match expr {
        ast::Expr::Value(v) => match &v.value {
            ast::Value::Number(n, _) => Some(n.clone()),
            ast::Value::Boolean(b) => Some(b.to_string()),
            other => other.as_pg_string().map(str::to_string),
        },
        ast::Expr::Identifier(ident) => Some(ident.value.clone()),
        // A negative number parses as a unary minus over the digits, not as a
        // `Number` token — `SET extra_float_digits = -1` and the `TO -3` form
        // both arrive this way, and dropping them silently ignores the SET.
        ast::Expr::UnaryOp {
            op: op @ (ast::UnaryOperator::Minus | ast::UnaryOperator::Plus),
            expr,
        } => {
            let inner = set_value_to_string(std::slice::from_ref(expr.as_ref()))?;
            Some(if matches!(op, ast::UnaryOperator::Minus) {
                format!("-{inner}")
            } else {
                inner
            })
        }
        _ => None,
    }
}

/// `RESET <param>` / `RESET ALL`: restore boot values through the [`crate::guc`]
/// table. An unrecognized name is accepted and ignored, as `SET` does.
fn apply_reset(reset: &ast::ResetStatement, session: &mut Session) -> Result<QueryResult, PgError> {
    match &reset.reset {
        ast::Reset::ALL => {
            for def in guc::GUCS {
                // `RESET ALL` silently skips a read-only parameter where
                // `RESET <name>` on the same one raises 55P02. Skipping it here
                // rather than inside the setter also keeps it out of the save
                // stack, where it could neither change nor ever be a `session`.
                if def.is_read_only() {
                    continue;
                }
                session.assign_guc(def, guc::GucValue::Default, false)?;
            }
        }
        ast::Reset::ConfigurationParameter(name) => {
            let Some(def) = single_ident_lower(name).and_then(|n| guc::lookup(&n)) else {
                return Ok(QueryResult::command("RESET"));
            };
            session.assign_guc(def, guc::GucValue::Default, false)?;
        }
    }
    Ok(QueryResult::command("RESET"))
}

/// `SHOW <name>` / `SHOW ALL`.
///
/// `SHOW TIME ZONE` reaches here as two identifiers (`TIME`, `ZONE`), so the
/// parts are joined before lookup — that is also how `SHOW TRANSACTION
/// ISOLATION LEVEL`-style multi-word names would resolve. Unlike `SET`, an
/// unrecognized name is an error: there is no value to invent.
fn execute_show(variable: &[ast::Ident], session: &Session) -> Result<QueryResult, PgError> {
    let columns = show_columns(variable);
    if is_show_all(variable) {
        let rows = guc::GUCS
            .iter()
            // `SHOW ALL` skips the parameters PG flags `GUC_NO_SHOW_ALL`, which
            // `SHOW <name>` still answers for. Already sorted by name: `GUCS` is.
            .filter(|def| def.show_all)
            .map(|def| {
                vec![
                    Value::Text(def.name.to_string()),
                    Value::Text((def.show)(session)),
                    Value::Text(def.description.to_string()),
                ]
            })
            .collect::<Vec<_>>();
        return Ok(show_result(columns, rows));
    }
    let joined = join_show_name(variable);
    let Some(def) = guc::lookup(&joined) else {
        return Err(guc::unrecognized(&joined.to_ascii_lowercase()));
    };
    Ok(show_result(
        columns,
        vec![vec![Value::Text((def.show)(session))]],
    ))
}

/// The columns `SHOW <variable>` returns. Shared by Describe (which must agree
/// with what Execute streams) and by [`execute_show`].
///
/// `SHOW ALL` has PG's three-column shape; every other name yields one column
/// titled with the parameter's canonical spelling, falling back to the name as
/// written when it resolves to nothing — Describe reports a shape even for a
/// name Execute will reject.
fn show_columns(variable: &[ast::Ident]) -> Vec<OutputColumn> {
    let text = |name: String| OutputColumn::new(name, PgType::Text);
    if is_show_all(variable) {
        return vec![
            text("name".to_string()),
            text("setting".to_string()),
            text("description".to_string()),
        ];
    }
    let joined = join_show_name(variable);
    let title = guc::lookup(&joined).map_or(joined, |def| def.name.to_string());
    vec![text(title)]
}

fn is_show_all(variable: &[ast::Ident]) -> bool {
    variable.len() == 1 && variable[0].value.eq_ignore_ascii_case("all")
}

/// `SHOW TIME ZONE` arrives as two identifiers, so the parts are joined before
/// lookup; a single identifier is itself.
fn join_show_name(variable: &[ast::Ident]) -> String {
    variable
        .iter()
        .map(|i| i.value.as_str())
        .collect::<Vec<_>>()
        .join("")
}

/// A materialized `SHOW` result set. Every column is `text`, and the command tag
/// is the bare `SHOW` — not `SELECT n`, whose trailing integer a driver would
/// report as a row count.
fn show_result(columns: Vec<OutputColumn>, rows: Vec<Vec<Value>>) -> QueryResult {
    QueryResult::Rows {
        columns,
        node: Box::new(MaterializedRows::new(rows)),
        tag: RowTag::Show,
        notices: Vec::new(),
    }
}

/// A single-part object name, lowercased (GUC names are case-insensitive).
fn single_ident_lower(name: &ast::ObjectName) -> Option<String> {
    if name.0.len() != 1 {
        return None;
    }
    name.0[0].as_ident().map(normalize_ident)
}

fn statement_kind(stmt: &ast::Statement) -> String {
    // First word of the SQL rendering, e.g. "DROP" or "TRUNCATE".
    stmt.to_string()
        .split_whitespace()
        .next()
        .unwrap_or("?")
        .to_string()
}

/// Unquoted identifiers fold to lowercase, as in PG.
pub(crate) fn normalize_ident(ident: &ast::Ident) -> String {
    match ident.quote_style {
        Some(_) => ident.value.clone(),
        None => ident.value.to_lowercase(),
    }
}

/// A single-part object name, lowercased. `noun` names the object class for the
/// error text ("relation", "type", "function"). Qualified names are not yet
/// supported.
fn single_object_name(name: &ast::ObjectName, noun: &str) -> Result<String, PgError> {
    if name.0.len() != 1 {
        return Err(PgError::feature_not_supported(format!(
            "qualified {noun} names are not supported yet: {name}"
        )));
    }
    match name.0[0].as_ident() {
        Some(ident) => Ok(normalize_ident(ident)),
        None => Err(PgError::syntax(format!("invalid {noun} name: {name}"))),
    }
}

fn object_name_to_table_name(name: &ast::ObjectName) -> Result<String, PgError> {
    single_object_name(name, "relation")
}

fn create_table_access_method(create: &ast::CreateTable) -> Result<TableAccessMethod, PgError> {
    if create.external {
        return Err(PgError::feature_not_supported(
            "external tables are not supported",
        ));
    }
    let Some(format) = &create.hive_formats else {
        return Ok(TableAccessMethod::Heap);
    };
    if format.row_format.is_some() || format.serde_properties.is_some() || format.location.is_some()
    {
        return Err(PgError::feature_not_supported(
            "external table storage options and LOCATION are not supported",
        ));
    }
    let Some(ast::HiveIOFormat::Using { format }) = &format.storage else {
        return Err(PgError::feature_not_supported(
            "only CREATE TABLE ... USING heap, USING buffer or USING parquet is supported",
        ));
    };
    let name = normalize_ident(format);
    TableAccessMethod::from_name(&name).ok_or_else(|| {
        PgError::new(
            sqlstate::UNDEFINED_OBJECT,
            format!("access method \"{name}\" does not exist"),
        )
    })
}

/// Split a possibly schema-qualified object name into `(schema, name)`. One part
/// is unqualified (`None`); two parts are `schema.name`; three or more (a
/// database qualifier) are rejected, matching the binder's `split_relation_name`.
fn split_object_name(
    name: &ast::ObjectName,
    noun: &str,
) -> Result<(Option<String>, String), PgError> {
    let ident_at = |part: &ast::ObjectNamePart| -> Result<String, PgError> {
        part.as_ident()
            .map(normalize_ident)
            .ok_or_else(|| PgError::syntax(format!("invalid {noun} name: {name}")))
    };
    match name.0.as_slice() {
        [one] => Ok((None, ident_at(one)?)),
        [schema, one] => Ok((Some(ident_at(schema)?), ident_at(one)?)),
        _ => Err(PgError::feature_not_supported(format!(
            "cross-database {noun} names are not supported yet: {name}"
        ))),
    }
}

/// Resolve the namespace a schema-qualified (or unqualified) `CREATE` targets,
/// validating it. Unqualified and `public` go to `public`; `pg_catalog`/
/// `information_schema` are permission-denied; any other name must be an existing
/// user schema (else `3F000`). `display` is the bare object name, for error text.
fn resolve_create_namespace(
    engine: &Arc<dyn TableEngine>,
    schema: Option<&str>,
    display: &str,
) -> Result<String, PgError> {
    match schema {
        None | Some("public") => Ok("public".to_string()),
        Some(ns @ ("pg_catalog" | "information_schema")) => Err(PgError::new(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            format!("permission denied to create \"{ns}.{display}\""),
        )),
        Some(ns) => {
            if engine.schema_exists(ns) {
                Ok(ns.to_string())
            } else {
                Err(PgError::new(
                    sqlstate::INVALID_SCHEMA_NAME,
                    format!("schema \"{ns}\" does not exist"),
                ))
            }
        }
    }
}

/// The deparsed form of a column default the binder left for execution, or
/// `None` when it is not one.
///
/// A literal the binder cannot fold comes back as exactly one shape: a
/// `Coerce` over a text `Const`. It is deferred for one of two reasons, both of
/// them "the binder holds no session" — a `timestamptz`/`timetz` text needs the
/// display zone, and a relative one (`'now'`, `'today'`, `'tomorrow'`,
/// `'yesterday'`) needs the transaction clock. Here the session *is* in hand,
/// so the constant resolves, which is what PostgreSQL does: it evaluates a
/// literal default when the DDL runs and stores the resulting value, not the
/// text. That is the distinction PG's manual draws between `DEFAULT 'now'` and
/// `DEFAULT now()`.
///
/// Recognising the *bound* shape rather than the written syntax is what makes
/// `DEFAULT 'now'` and `DEFAULT 'now'::timestamp` — the spelling the manual
/// actually uses — behave alike, and it covers an array element (`timestamptz[]
/// DEFAULT '{now}'`) without naming a single type. Matching the AST instead
/// froze only the unqualified spelling and left the rest re-reading the clock
/// on every insert.
///
/// What a reader then sees is the stored value put back into *their* zone,
/// which `pg_get_expr` does on the way out.
fn session_literal_default(
    bound: &crabgresql_binder::BoundExpr,
    column: &Column,
    session: &Session,
) -> Option<String> {
    let crabgresql_binder::BoundExpr::Coerce { expr, ty } = bound else {
        return None;
    };
    let crabgresql_binder::BoundExpr::Const {
        value: Value::Text(text),
        ..
    } = expr.as_ref()
    else {
        return None;
    };
    let fmt = session.fmt_ctx();
    let value = crabgresql_types::cast::cast_value(Value::Text(text.clone()), *ty, &fmt).ok()?;
    // Labelled by the same helper the folded path uses, so the two renderings
    // cannot drift; the *value* is what differs, since this one needs a session.
    Some(format!(
        "'{}'::{}",
        value.encode_text_with(&fmt)?.replace('\'', "''"),
        crabgresql_binder::const_type_label(column.ty)
    ))
}

fn execute_create_table(
    engine: &Arc<dyn TableEngine>,
    type_catalog: &Arc<dyn TypeCatalog>,
    catalog_ops: &Arc<dyn CatalogOps>,
    create: &ast::CreateTable,
    session: &Session,
) -> Result<QueryResult, PgError> {
    let access_method = create_table_access_method(create)?;
    let (schema_qual, name) = split_object_name(&create.name, "relation")?;
    // A TEMP table lives in the session temp keyspace; a schema qualifier on it
    // is only meaningful as `pg_temp` (which we already route), so reject any
    // other qualifier as PG does. Non-temp tables resolve their target schema.
    let namespace = if create.temporary {
        match schema_qual.as_deref() {
            None | Some("pg_temp") => {}
            Some(other) if other == session.temp_schema => {}
            Some(_) => {
                return Err(PgError::feature_not_supported(
                    "cannot create a temporary relation in a non-temporary schema",
                ));
            }
        }
        // Temp relations live in this session's `pg_temp_N` namespace in the shared
        // engine (PG-style), backed by memory tables.
        session.temp_schema.clone()
    } else {
        resolve_create_namespace(engine, schema_qual.as_deref(), &name)?
    };
    // `IF NOT EXISTS` on a relation that is already there is a no-op, and PG
    // decides that *before* it analyzes anything else. Checking it here rather
    // than after the columns are resolved is what makes the no-op total: the
    // rest of this function raises notices (the inheritance merge, the datetime
    // clamp) and errors (a type conflict, a missing parent, the sort-key rule)
    // that all describe a table this statement is not going to touch.
    if create.if_not_exists && engine.resolve(Some(&namespace), &name).is_ok() {
        return Ok(QueryResult::Command {
            tag: "CREATE TABLE".into(),
            notices: vec![Notice::notice(
                format!("relation \"{name}\" already exists, skipping"),
                None,
            )],
        });
    }
    // Temp and UNLOGGED tables are memory tables (RAM-backed, WAL-skipping); a
    // plain table is a durable heap. All of them live in the one shared engine now.
    let persistence = if create.temporary {
        RelPersistence::Temporary
    } else if create.unlogged {
        RelPersistence::Unlogged
    } else {
        RelPersistence::Permanent
    };
    // An engine-managed method defines its own relationship to the WAL, so
    // UNLOGGED and TEMP have no meaning for it, and it holds no per-partition
    // storage to route into. Reject both rather than silently ignoring the
    // clause and handing back a table that is not what was asked for.
    if access_method.is_engine_managed() && persistence != RelPersistence::Permanent {
        return Err(PgError::feature_not_supported(format!(
            "table access method \"{}\" only supports permanent tables",
            access_method.as_str(),
        )));
    }
    if access_method.is_engine_managed()
        && (create.partition_by.is_some() || create.partition_of.is_some())
    {
        return Err(PgError::feature_not_supported(format!(
            "table access method \"{}\" does not support partitioning",
            access_method.as_str(),
        )));
    }
    let target = engine;
    // CTAS (`create.query.is_some()`) is dispatched to `execute_create_table_as`
    // before reaching here, so this path only builds an empty table. Clauses we
    // can't honor must be rejected, not silently dropped: ON COMMIT DROP/DELETE
    // ROWS needs the M2 transaction engine — accepting it would leave a plain
    // session-lifetime table that diverges from PG.
    if create.on_commit.is_some() {
        return Err(PgError::feature_not_supported(
            "CREATE TABLE ... ON COMMIT is not supported yet",
        ));
    }
    // Table inheritance is a distinct feature from declarative partitioning, and
    // the two do not compose here: a partitioned relation anywhere in a hierarchy
    // would make the read fan-out have to expand a leaf set inside a descendant
    // set. PG allows the combination; we reject it rather than half-honor it.
    let inherits = create.inherits.as_deref().unwrap_or(&[]);
    if !inherits.is_empty() {
        // Both `PARTITION BY` (this table is a partitioned parent) and
        // `PARTITION OF` (it is a leaf) are refused. The `PARTITION OF` case
        // matters most: the parser accepts the two clauses together, and the
        // dispatch below would hand the statement to `execute_create_partition`,
        // which builds its schema from the parent alone — silently discarding
        // the parents this clause named.
        if create.partition_by.is_some() || create.partition_of.is_some() {
            return Err(PgError::feature_not_supported(
                "CREATE TABLE ... INHERITS with declarative partitioning is not supported yet",
            ));
        }
        if access_method.is_engine_managed() {
            return Err(PgError::feature_not_supported(format!(
                "table access method \"{}\" does not support inheritance",
                access_method.as_str(),
            )));
        }
    }
    // Redshift's `SORTKEY (...)` parses ungated, and is a second spelling of the
    // layout order `ORDER BY` now owns. Accepting it silently would let a table
    // claim an order nothing recorded; `ORDER BY` is the spelling we implement.
    if create.sortkey.is_some() {
        return Err(PgError::feature_not_supported(
            "CREATE TABLE ... SORTKEY is not supported; use ORDER BY (columns)",
        ));
    }
    // Declarative partitioning (initial slice: RANGE, DDL + catalog reflection).
    // A partitioned parent or a leaf partition may not be a memory table (TEMP or
    // UNLOGGED) in this slice: leaf partitions are always created Permanent, so a
    // memory-table parent would leave durable partitions dangling off a parent
    // that vanishes on restart.
    if (create.partition_by.is_some() || create.partition_of.is_some())
        && (create.temporary || create.unlogged)
    {
        let kind = if create.temporary {
            "temporary"
        } else {
            "unlogged"
        };
        return Err(PgError::feature_not_supported(format!(
            "{kind} partitioned tables are not supported yet"
        )));
    }
    // Refuse `ORDER BY` on a method with no layout to order here, above the
    // `PARTITION OF` dispatch: a leaf partition is always a heap, and letting it
    // fall through would swallow the clause the plain heap path below rejects.
    if create.order_by.is_some() {
        reject_layout_order(access_method)?;
    }
    // A leaf partition (`PARTITION OF parent`) inherits the parent's columns and
    // is created as an ordinary heap table carrying its bound; handle it whole.
    if let Some(parent) = &create.partition_of {
        return execute_create_partition(
            engine,
            type_catalog,
            create,
            &namespace,
            &name,
            parent,
            &session.temp_schema,
            // A bound may name the session identity (`current_user`,
            // `current_schema`, `pg_my_temp_schema()`), which PostgreSQL folds
            // into the stored bound — so the fold needs this statement's
            // catalog snapshot. `exec_context()` deliberately leaves it unset
            // for EXPLAIN's constant rendering, where no user expression runs.
            //
            // Only `catalog`: `sequences`/`routines` stay unset because
            // PostgreSQL does not fold a volatile function into a bound either,
            // and implementing that without a probed reference does not belong
            // in a bug fix. `nextval(…)` in a bound therefore still raises.
            &ExecContext {
                catalog: Some(Arc::clone(catalog_ops)),
                ..session.exec_context()
            },
        );
    }
    #[derive(Clone)]
    struct PendingIndex {
        explicit_name: Option<String>,
        columns: Vec<ast::IndexColumn>,
        unique: bool,
        nulls_distinct: bool,
        constraint: IndexConstraint,
        characteristics: Option<ast::ConstraintCharacteristics>,
    }

    /// A `CHECK` clause in the shape the parser handed over, held until the
    /// inherited-column merge below has fixed the final column positions its
    /// predicate binds against. Binding earlier would index a layout that the
    /// merge then rearranges — silently, and only for a table with `INHERITS`.
    struct PendingCheck {
        explicit_name: Option<String>,
        expr: ast::Expr,
    }

    let mut columns = Vec::new();
    let mut pending = Vec::<PendingIndex>::new();
    let mut pending_checks = Vec::<PendingCheck>::new();
    let mut constraint_names = HashSet::new();
    // `serial`/`bigserial`/`smallserial` desugar to an int column plus an owned
    // sequence and a `nextval(...)` default. The sequences are created together
    // just before the table, so a failed table create can roll them back.
    let mut serial_defs: Vec<SequenceDefinition> = Vec::new();
    for col in &create.columns {
        let column_name = normalize_ident(&col.name);
        let serial_base = serial_base_type(&col.data_type);
        if serial_base.is_some() && create.temporary {
            return Err(PgError::feature_not_supported(
                "serial in a temporary table is not supported yet",
            ));
        }
        let ty = match serial_base {
            Some(base) => base,
            None => resolve_column_type(type_catalog, &col.data_type)?,
        };
        reject_stored_reg_type(ty, &column_name)?;
        let typmod = crabgresql_binder::declared_typmod(ty, &col.data_type)?.unwrap_or(-1);
        let mut column = Column::with_typmod(column_name.clone(), ty, typmod);
        if let Some(base) = serial_base {
            // Name the sequence `t_col_seq`, dodging existing relations and any
            // other serial sequences created earlier in this same statement.
            let taken: Vec<String> = serial_defs.iter().map(|d| d.name.clone()).collect();
            let seq_name = unique_relation_name(
                target,
                &namespace,
                &taken,
                &format!("{name}_{column_name}_seq"),
            );
            // A qualified table's serial default must reference the sequence by
            // its schema too, so `nextval` resolves it in the same namespace.
            let seq_ref = if namespace == "public" {
                seq_name.clone()
            } else {
                format!("{namespace}.{seq_name}")
            };
            // Written the way PG's deparse prints it — `nextval` takes a
            // `regclass`, and the cast is what a re-parse of this text needs to
            // resolve the name.
            column.default = Some(format!("nextval('{seq_ref}'::regclass)"));
            serial_defs.push(SequenceDefinition {
                name: seq_name,
                namespace: namespace.clone(),
                data_type: base,
                start: 1,
                increment: 1,
                min: 1,
                max: serial_type_max(base),
                cache: 1,
                cycle: false,
                owned_by: Some(name.clone()),
            });
        }
        for option in &col.options {
            match &option.option {
                ast::ColumnOption::Null => {}
                ast::ColumnOption::NotNull => {
                    if !column.nullable {
                        return Err(PgError::new(
                            "42710",
                            format!(
                                "NOT NULL constraint specified more than once for column \"{column_name}\" of table \"{name}\""
                            ),
                        ));
                    }
                    column.nullable = false;
                    let constraint_name =
                        option
                            .name
                            .as_ref()
                            .map(normalize_ident)
                            .unwrap_or_else(|| {
                                fresh_local_name(
                                    |c| constraint_names.contains(c),
                                    &format!("{name}_{column_name}_not_null"),
                                )
                            });
                    if !constraint_names.insert(constraint_name.clone()) {
                        return Err(PgError::new(
                            "42710",
                            format!(
                                "constraint \"{constraint_name}\" for relation \"{name}\" already exists"
                            ),
                        ));
                    }
                    column.not_null_constraint = Some(constraint_name);
                }
                ast::ColumnOption::Default(expr) => {
                    if column.default.is_some() {
                        return Err(PgError::syntax(format!(
                            "multiple default values specified for column \"{column_name}\" of table \"{name}\""
                        )));
                    }
                    let bound =
                        crabgresql_binder::bind_column_default(expr, &column, type_catalog)?;
                    // The default is stored already deparsed, so `pg_get_expr`
                    // (which echoes this text) prints what PostgreSQL's `\d`
                    // does. A literal takes its type from the column, so the
                    // binder renders it; everything else goes through the same
                    // deparser a view definition does. A plain NULL for a type
                    // needing no coercion is not a default at all.
                    let source = expr.to_string();
                    column.default = match crabgresql_binder::deparse_literal_default(
                        expr,
                        &column,
                        type_catalog,
                    )? {
                        crabgresql_binder::ColumnDefault::Deparsed(text) => Some(text),
                        crabgresql_binder::ColumnDefault::Omit => None,
                        crabgresql_binder::ColumnDefault::Source => Some(
                            session_literal_default(&bound, &column, session)
                                .or_else(|| {
                                    crabgresql_binder::ruleutils::deparse_stored_expr(
                                        &source,
                                        type_catalog,
                                    )
                                })
                                .unwrap_or(source),
                        ),
                    };
                }
                ast::ColumnOption::PrimaryKey(pk) => {
                    reject_primary_key_options(pk)?;
                    pending.push(PendingIndex {
                        explicit_name: option.name.as_ref().map(normalize_ident),
                        columns: vec![ast::IndexColumn::from(col.name.clone())],
                        unique: true,
                        nulls_distinct: true,
                        constraint: IndexConstraint::PrimaryKey,
                        characteristics: pk.characteristics,
                    });
                }
                ast::ColumnOption::Unique(unique) => {
                    reject_unique_options(unique)?;
                    pending.push(PendingIndex {
                        explicit_name: option.name.as_ref().map(normalize_ident),
                        columns: vec![ast::IndexColumn::from(col.name.clone())],
                        unique: true,
                        nulls_distinct: !matches!(
                            unique.nulls_distinct,
                            ast::NullsDistinctOption::NotDistinct
                        ),
                        constraint: IndexConstraint::Unique,
                        characteristics: unique.characteristics,
                    });
                }
                // `col text COLLATE "de-x-icu"`: the column's values order under
                // this collation wherever no nearer COLLATE overrides it.
                ast::ColumnOption::Collation(collation) => {
                    if column.collation.is_some() {
                        return Err(PgError::syntax(format!(
                            "multiple COLLATE clauses not allowed for column \"{column_name}\""
                        )));
                    }
                    if !column.ty.is_collatable() {
                        return Err(PgError::new(
                            sqlstate::WRONG_OBJECT_TYPE,
                            format!("collations are not supported by type {}", column.ty.name()),
                        ));
                    }
                    column.collation = Some(crabgresql_binder::resolve_collation(collation)?);
                }
                // `col int CHECK (col > 3)`. The parser leaves
                // `CheckConstraint.name` empty for a column clause and puts a
                // `CONSTRAINT n` on the enclosing option instead, which is where
                // the NOT NULL arm above reads it from too.
                ast::ColumnOption::Check(check) => {
                    reject_not_enforced(check.enforced)?;
                    pending_checks.push(PendingCheck {
                        explicit_name: check
                            .name
                            .as_ref()
                            .or(option.name.as_ref())
                            .map(normalize_ident),
                        expr: (*check.expr).clone(),
                    });
                }
                other => {
                    return Err(PgError::feature_not_supported(format!(
                        "column constraint is not supported yet: {other}"
                    )));
                }
            }
        }
        // A serial column is NOT NULL; apply it after the option loop so an
        // explicit (redundant) NOT NULL is tolerated rather than double-counted.
        if serial_base.is_some() {
            column.nullable = false;
        }
        columns.push(column);
    }

    for constraint in &create.constraints {
        match constraint {
            ast::TableConstraint::PrimaryKey(pk) => {
                reject_primary_key_options(pk)?;
                pending.push(PendingIndex {
                    explicit_name: pk.name.as_ref().map(normalize_ident),
                    columns: pk.columns.clone(),
                    unique: true,
                    nulls_distinct: true,
                    constraint: IndexConstraint::PrimaryKey,
                    characteristics: pk.characteristics,
                });
            }
            ast::TableConstraint::Unique(unique) => {
                reject_unique_options(unique)?;
                pending.push(PendingIndex {
                    explicit_name: unique.name.as_ref().map(normalize_ident),
                    columns: unique.columns.clone(),
                    unique: true,
                    nulls_distinct: !matches!(
                        unique.nulls_distinct,
                        ast::NullsDistinctOption::NotDistinct
                    ),
                    constraint: IndexConstraint::Unique,
                    characteristics: unique.characteristics,
                });
            }
            ast::TableConstraint::Check(check) => {
                reject_not_enforced(check.enforced)?;
                pending_checks.push(PendingCheck {
                    explicit_name: check.name.as_ref().map(normalize_ident),
                    expr: (*check.expr).clone(),
                });
            }
            other => {
                return Err(PgError::feature_not_supported(format!(
                    "table constraint is not supported yet: {other}"
                )));
            }
        }
    }

    if pending
        .iter()
        .filter(|p| p.constraint == IndexConstraint::PrimaryKey)
        .count()
        > 1
    {
        return Err(PgError::new(
            "42P16",
            format!("multiple primary keys for table \"{name}\" are not allowed"),
        ));
    }

    // A partitioned parent (`PARTITION BY ...`) carries a partition key and holds
    // no rows of its own. This slice supports RANGE only, without keys/serial.
    let partition_scheme = match create.partition_by.as_deref() {
        Some(expr) => {
            if !pending.is_empty() {
                return Err(PgError::feature_not_supported(
                    "primary key and unique constraints on partitioned tables are not supported yet",
                ));
            }
            if !serial_defs.is_empty() {
                return Err(PgError::feature_not_supported(
                    "serial columns in partitioned tables are not supported yet",
                ));
            }
            // PostgreSQL copies a partitioned parent's checks into every leaf, so
            // that a row routed to a leaf still meets the parent's constraint. We
            // create leaves from the parent's columns alone, so accepting one
            // here would declare a constraint and then never enforce it — the one
            // failure worse than refusing the DDL.
            if !pending_checks.is_empty() {
                return Err(PgError::feature_not_supported(
                    "CHECK constraints on partitioned tables are not supported yet",
                ));
            }
            Some(build_partition_scheme(expr, &columns)?)
        }
        None => None,
    };

    // An engine-managed relation caches its `TableSchema` at open and has no
    // republish path, so `ALTER TABLE ... ADD CHECK` cannot reach one — the
    // constraint would land in the durable catalog and stay invisible to the
    // running handle until a restart. Enforcement itself works there (it lives in
    // the executor, keyed off `TableAm::schema()`), so this is a narrower refusal
    // than it looks: it keeps CREATE and ALTER agreeing about the same relation
    // rather than accepting a constraint that only half the DDL surface can add.
    if access_method.is_engine_managed() && !pending_checks.is_empty() {
        return Err(PgError::feature_not_supported(format!(
            "CHECK constraints on a table using access method \"{}\" are not supported yet",
            access_method.as_str(),
        )));
    }

    // Fold in the inherited columns now that the table's own are fully resolved
    // (serial desugaring, NOT NULL, DEFAULT, COLLATE all applied), and before
    // anything indexes into the layout: the merge is what decides the final
    // column *positions*, which every PRIMARY KEY and sort key below refers to.
    // The merge's own notices go straight to the session sink, so they reach the
    // client ahead of the error when the merge fails; `notices` here carries only
    // what this function raises on the success path.
    let mut notices: Vec<Notice> = Vec::new();
    let (columns, inherit_links) = if inherits.is_empty() {
        (columns, Vec::new())
    } else {
        merge_inherited_columns(
            engine,
            inherits,
            create.temporary,
            &session.temp_schema,
            columns,
            &session.notices,
        )?
    };

    let schema = TableSchema {
        name: name.clone(),
        namespace: namespace.clone(),
        columns,
        persistence,
        access_method,
        partition_scheme,
        partition_of: None,
        inherits: inherit_links,
        // Resolved below, once the PRIMARY KEY's columns are known.
        sort_key: Vec::new(),
        // Resolved below, once the inherited-column merge has fixed the final
        // column positions a CHECK predicate binds against.
        checks: Vec::new(),
    };
    // Ask the access method about the column types before any index or sort-key
    // rule looks at them, so "this method cannot store `json`" wins over the
    // B-tree-operator-class complaint that the same column would otherwise draw.
    target.validate_schema(&schema)?;
    let mut indexes = Vec::new();
    for p in pending {
        reject_deferred_characteristics(p.characteristics)?;
        let keys = simple_index_keys(&schema, &p.columns)?;
        let base = constraint_index_base(&name, &schema, p.constraint, &keys);
        let index_name = p
            .explicit_name
            .unwrap_or_else(|| fresh_relation_name(target, &namespace, &constraint_names, &base));
        if !constraint_names.insert(index_name.clone()) {
            return Err(PgError::new(
                "42710",
                format!("constraint \"{index_name}\" for relation \"{name}\" already exists"),
            ));
        }
        indexes.push(IndexMetadata {
            name: index_name,
            method: IndexMethod::BTree,
            keys,
            unique: p.unique,
            nulls_distinct: p.nulls_distinct,
            constraint: Some(p.constraint),
        });
    }
    // TEMP tables go in the session-local catalog, which shadows a same-named
    // permanent table; its separate keyspace means shadowing never raises 42P07.
    let mut schema = schema;
    for index in &indexes {
        if index.constraint == Some(IndexConstraint::PrimaryKey) {
            for key in &index.keys {
                schema.columns[key.column].nullable = false;
            }
        }
    }
    schema.sort_key = build_sort_key(
        create.order_by.as_ref(),
        access_method,
        &schema,
        indexes
            .iter()
            .find(|index| index.constraint == Some(IndexConstraint::PrimaryKey)),
    )?;
    // The parents are resolved *before* the table's own checks are named, so a
    // generated name can step around one the merge is about to inherit —
    // PostgreSQL's `ChooseConstraintName` consults the inherited set too, and
    // without this a child declaring `CHECK (x < 100)` under a parent already
    // holding `c_x_check` collided instead of becoming `c_x_check1`.
    //
    // Re-resolving rather than threading handles down from the column merge: it
    // has already accepted every one of them, so this cannot fail differently,
    // and the alternative widens a five-argument helper for one caller.
    let parents = match inherits.is_empty() {
        true => Vec::new(),
        false => inherits
            .iter()
            .map(|p| resolve_parent_relation(engine, p, &session.temp_schema).map(|(t, _)| t))
            .collect::<Result<Vec<_>, _>>()?,
    };
    let inherited_names: HashSet<String> = parents
        .iter()
        .flat_map(|p| {
            p.schema()
                .checks
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>()
        })
        .collect();
    // Resolve the CHECK clauses last: their names dedupe against the index-backed
    // ones claimed just above, and their predicates bind against the layout the
    // inherited-column merge settled — including the PRIMARY KEY's nullability
    // flips, which nothing here reads but which a future `conkey` consumer might.
    let own_checks = resolve_checks(
        pending_checks
            .into_iter()
            .map(|p| (p.explicit_name, p.expr)),
        &schema,
        &mut constraint_names,
        &inherited_names,
        type_catalog,
    )?;
    schema.checks = if parents.is_empty() {
        own_checks
    } else {
        merge_inherited_checks(
            &parents,
            own_checks,
            &schema,
            type_catalog,
            &session.notices,
        )?
    };
    // Create the serial columns' owned sequences before the table. Names were
    // chosen unique above, so a failure here is unexpected; clean up on it.
    for def in &serial_defs {
        if let Err(e) = target.create_sequence(def.clone()) {
            drop_created_sequences(target, &serial_defs);
            return Err(e.into());
        }
    }
    match target.create_table(schema) {
        Ok(_) => {
            for index in indexes {
                if let Err(e) = target.create_index(&namespace, &name, index) {
                    let _ = target.drop_table(&namespace, &name);
                    drop_created_sequences(target, &serial_defs);
                    return Err(e.into());
                }
            }
        }
        // The relation appeared between the check above and this create (or the
        // check could not see it). PG reports the skip; the serial sequences we
        // just created would be orphaned, so drop them.
        Err(StorageError::TableAlreadyExists(_)) if create.if_not_exists => {
            drop_created_sequences(target, &serial_defs);
            // Nothing was created, so the merge that would have happened did
            // not: reporting it would describe a table this statement did not
            // touch. The merge notices are already in the session sink, so drop
            // them there rather than from the local list.
            notices.clear();
            let _ = session.notices.drain();
            notices.push(Notice::notice(
                format!("relation \"{name}\" already exists, skipping"),
                None,
            ));
        }
        Err(e) => {
            drop_created_sequences(target, &serial_defs);
            return Err(e.into());
        }
    }
    Ok(QueryResult::Command {
        tag: "CREATE TABLE".to_string(),
        notices,
    })
}

/// Reject a direct physical DDL operation (TRUNCATE, CREATE INDEX) on a
/// partitioned parent. The parent owns no storage of its own — the operation
/// would have to fan out to its partitions, which is not implemented yet — so it
/// is rejected rather than silently applied to the empty parent relation. (DML
/// and queries against the parent are supported: they route/union across the
/// leaves; only these physical DDL operations remain unsupported.)
fn reject_partitioned_parent(table: &Arc<dyn TableAm>, name: &str) -> Result<(), PgError> {
    if table.schema().partition_scheme.is_some() {
        return Err(PgError::feature_not_supported(format!(
            "\"{name}\" is a partitioned table; operating on it directly is not supported yet"
        )));
    }
    Ok(())
}

/// Decode a `PARTITION BY <strategy> (<cols>)` clause into a [`PartitionScheme`].
/// The parser stores the clause as a function-call expression (`RANGE(d)`); this
/// slice supports RANGE with a single simple-column key.
fn build_partition_scheme(
    expr: &ast::Expr,
    columns: &[Column],
) -> Result<PartitionScheme, PgError> {
    let ast::Expr::Function(func) = expr else {
        return Err(PgError::feature_not_supported(
            "only PARTITION BY RANGE (column) is supported yet",
        ));
    };
    let strategy = func
        .name
        .0
        .last()
        .and_then(|p| p.as_ident())
        .map(|i| i.value.to_uppercase());
    match strategy.as_deref() {
        Some("RANGE") => {}
        Some("LIST") => {
            return Err(PgError::feature_not_supported(
                "LIST partitioning is not supported yet",
            ));
        }
        Some("HASH") => {
            return Err(PgError::feature_not_supported(
                "HASH partitioning is not supported yet",
            ));
        }
        _ => {
            return Err(PgError::feature_not_supported(
                "only PARTITION BY RANGE (column) is supported yet",
            ));
        }
    }
    let ast::FunctionArguments::List(list) = &func.args else {
        return Err(PgError::syntax("PARTITION BY requires a column list"));
    };
    if list.args.len() != 1 {
        return Err(PgError::feature_not_supported(
            "multi-column partition keys are not supported yet",
        ));
    }
    let key = match &list.args[0] {
        ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(ast::Expr::Identifier(ident))) => {
            normalize_ident(ident)
        }
        _ => {
            return Err(PgError::feature_not_supported(
                "only simple-column partition keys are supported yet",
            ));
        }
    };
    // A RANGE key must be a btree-orderable type; otherwise bound comparison
    // would later panic in `compare_values`. PG rejects the same at parent create.
    // (A user type can only reach here as an enum — non-enum user types are
    // rejected as column types upstream — and enums are orderable.)
    let idx = resolve_orderable_column(columns, &key, "partition key", None)?;
    Ok(PartitionScheme {
        strategy: PartitionStrategy::Range,
        key_columns: vec![idx],
    })
}

/// Resolve a relation's layout sort key: the order an engine-managed access
/// method stores its rows in.
///
/// PostgreSQL has no such clause, so the rule is ClickHouse MergeTree's — an
/// explicit `ORDER BY (...)` wins, a `PRIMARY KEY` supplies the default, and a
/// table declaring neither is refused. Refusing is the point: an unordered
/// column store gives up range pruning, compression locality, and
/// merge-friendly compaction, and handing one back silently would hide all
/// three. Nor is it ever forced — every type these methods accept is
/// btree-orderable, so a table can always name a key.
///
/// A heap relation has no layout order to declare, so `ORDER BY` on one is an
/// error rather than a clause quietly dropped.
///
/// A standalone `USING buffer` relation is the awkward case: it is RAM plus WAL
/// with nowhere to flush, so its key is recorded and nothing will ever apply it.
/// It is required all the same, so that the two engine-managed methods answer
/// the same DDL identically — a `buffer` table that silently accepted what
/// `parquet` refuses would be a worse surprise than a key that costs nothing.
/// Resolve a column named in an *ordered* key — an index key, a layout sort key,
/// or a RANGE partition key — and reject a type that has no B-tree ordering.
///
/// All three kinds compare their columns, so all three need the same check; it
/// lives here so the message and the predicate cannot drift between them. What
/// legitimately differs is spelled out by the arguments: `noun` names the key in
/// the "does not exist" message, and `hint` carries PostgreSQL's operator-class
/// advice, which only the index path gives because its wording ("for the index")
/// is wrong for the other two.
fn resolve_orderable_column(
    columns: &[Column],
    name: &str,
    noun: &str,
    hint: Option<&str>,
) -> Result<usize, PgError> {
    let Some(idx) = columns.iter().position(|c| c.name == name) else {
        return Err(PgError::new(
            sqlstate::UNDEFINED_COLUMN,
            format!("column \"{name}\" named in {noun} does not exist"),
        ));
    };
    let ty = columns[idx].ty;
    if !ty.has_default_btree_opclass() {
        let error = PgError::new(
            sqlstate::UNDEFINED_OBJECT,
            format!(
                "data type {} has no default operator class for access method \"btree\"",
                ty.name()
            ),
        );
        return Err(match hint {
            Some(hint) => error.with_hint(hint),
            None => error,
        });
    }
    Ok(idx)
}

/// PostgreSQL's advice on the missing-operator-class error, given only where it
/// reads correctly: an index.
const OPCLASS_HINT: &str = "You must specify an operator class for the index or \
                            define a default operator class for the data type.";

/// Refuse `ORDER BY` on an access method that has no layout to order. The one
/// place this message lives, so the `PARTITION OF` dispatch and the plain
/// CREATE TABLE / CTAS paths cannot drift apart on it.
fn reject_layout_order(access_method: TableAccessMethod) -> Result<(), PgError> {
    if access_method.is_engine_managed() {
        return Ok(());
    }
    Err(PgError::feature_not_supported(format!(
        "table access method \"{}\" does not support ORDER BY",
        access_method.as_str(),
    )))
}

/// Point a CTAS user at the clause position when their `ORDER BY` went to the
/// query instead of the table.
///
/// `CREATE TABLE t USING parquet AS SELECT ... ORDER BY x` parses the trailing
/// clause as the query's ordering — the table's own `ORDER BY` has to precede
/// `AS` — so the bare missing-key error tells someone who *did* write `ORDER BY`
/// to add one. Only reachable when the table declared no key of its own, which
/// is the only way that error fires on this path.
fn annotate_ctas_order_by(error: PgError, create: &ast::CreateTable) -> PgError {
    if create.order_by.is_some() {
        return error;
    }
    if !create.query.as_ref().is_some_and(|q| q.order_by.is_some()) {
        return error;
    }
    error.with_hint(
        "The trailing ORDER BY orders the query, not the table. Write \
         ORDER BY (columns) before AS to declare the table's sort key.",
    )
}

/// Refuse a sort key the storage layer could not actually store rows in.
///
/// A columnar engine orders a fragment with Arrow's kernels, and for a handful
/// of types Arrow's total order is not PostgreSQL's: `numeric` is stored as
/// text, `timetz` and `interval` as structs, and a text column under an ICU
/// collation orders by locale rather than by bytes. Such a key would be
/// accepted, persisted, and then quietly ignored on every write — so it is
/// rejected here instead, where the user can still choose another column.
///
/// Asked only of a method that [honors a
/// key](TableAccessMethod::honors_sort_key): a standalone `USING buffer`
/// relation stores nothing in key order anyway, so it has no promise to keep
/// and no reason to refuse one column while accepting another.
///
/// `defaulted` distinguishes the key the user wrote from the one inherited from
/// the PRIMARY KEY, because the remedy differs.
fn reject_unsortable_key(
    access_method: TableAccessMethod,
    schema: &TableSchema,
    keys: &[IndexKey],
    defaulted: bool,
) -> Result<(), PgError> {
    if !access_method.honors_sort_key() {
        return Ok(());
    }
    // The same walk the write path uses to decide what it will honor, so the
    // two cannot disagree about which keys are real.
    let Some(column) = crabgresql_storage_api::sort::unsortable_column(&schema.columns, keys)
    else {
        return Ok(());
    };
    let error = PgError::new(
        sqlstate::INVALID_OBJECT_DEFINITION,
        format!(
            "column \"{}\" of type {} cannot be used in a sort key",
            column.name,
            column.ty.name()
        ),
    );
    Err(error.with_hint(if defaulted {
        "The PRIMARY KEY supplies the sort key. Add an explicit \
         ORDER BY (columns) naming a column the storage layer can order."
    } else {
        "Name a column the storage layer can order."
    }))
}

fn build_sort_key(
    order_by: Option<&ast::OneOrManyWithParens<ast::Expr>>,
    access_method: TableAccessMethod,
    schema: &TableSchema,
    primary_key: Option<&IndexMetadata>,
) -> Result<Vec<IndexKey>, PgError> {
    if !access_method.is_engine_managed() {
        if order_by.is_some() {
            reject_layout_order(access_method)?;
        }
        return Ok(Vec::new());
    }
    let Some(order_by) = order_by else {
        // The PRIMARY KEY's `IndexKey`s were already resolved and validated by
        // the index-building loop, so the default costs no second resolution.
        return match primary_key {
            Some(pk) => {
                reject_unsortable_key(access_method, schema, &pk.keys, true)?;
                Ok(pk.keys.clone())
            }
            None => Err(PgError::new(
                sqlstate::INVALID_OBJECT_DEFINITION,
                format!(
                    "table access method \"{}\" requires ORDER BY or PRIMARY KEY",
                    access_method.as_str(),
                ),
            )
            // `ORDER BY` first, deliberately. A PRIMARY KEY also supplies the
            // key, but a unique index makes every INSERT scan the whole relation
            // to enforce it (`insert_direct` has no B-tree to probe on an
            // engine-managed table), which is ruinous at columnar scale. The two
            // are not interchangeable and the hint must not imply they are.
            .with_hint(
                "Add ORDER BY (columns) to the CREATE TABLE. A PRIMARY KEY \
                 supplies one too, at the cost of enforcing uniqueness on \
                 every insert.",
            )),
        };
    };
    // `OneOrManyWithParens` derefs to a slice, which flattens both spellings.
    let exprs: &[ast::Expr] = order_by;
    // `ORDER BY ()` parses to an empty list — ClickHouse's `ORDER BY tuple()`,
    // its opt-out from the same rule. We have no opt-out, and saying so here is
    // clearer than reusing the "you declared nothing" message: the user did
    // declare something, it was just empty.
    if exprs.is_empty() {
        return Err(PgError::new(
            sqlstate::INVALID_OBJECT_DEFINITION,
            "ORDER BY must name at least one column",
        ));
    }
    let mut keys: Vec<IndexKey> = Vec::with_capacity(exprs.len());
    for expr in exprs {
        let ast::Expr::Identifier(ident) = expr else {
            return Err(PgError::feature_not_supported(
                "only simple column references are supported in ORDER BY",
            ));
        };
        let name = normalize_ident(ident);
        // The orderability half is unreachable for today's methods — every
        // unorderable type (`json`, `jsonpath`, `point`, `lseg`) is outside what
        // Parquet stores, so `validate_schema` rejects the column first with the
        // truer message — but it holds for the method that eventually stores one.
        let column = resolve_orderable_column(&schema.columns, &name, "sort key", None)?;
        if keys.iter().any(|key| key.column == column) {
            return Err(PgError::new(
                sqlstate::INVALID_OBJECT_DEFINITION,
                format!("sort key column \"{name}\" appears more than once"),
            ));
        }
        // The clause parses its elements as bare expressions, so `a DESC` does
        // not parse and there is no direction or NULL ordering to carry yet.
        // Ascending / NULLS LAST is PG's default for an unqualified key.
        keys.push(IndexKey {
            column,
            descending: false,
            nulls_first: false,
        });
    }
    reject_unsortable_key(access_method, schema, &keys, false)?;
    Ok(keys)
}

/// A single-dimension RANGE endpoint, ordered `NegInf < Finite(v) < PosInf`.
enum Endpoint {
    NegInf,
    Finite(Value),
    PosInf,
}

fn endpoint_cmp(a: &Endpoint, b: &Endpoint, ty: PgType) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Endpoint::NegInf, Endpoint::NegInf) | (Endpoint::PosInf, Endpoint::PosInf) => {
            Ordering::Equal
        }
        (Endpoint::NegInf, _) | (_, Endpoint::PosInf) => Ordering::Less,
        (Endpoint::PosInf, _) | (_, Endpoint::NegInf) => Ordering::Greater,
        (Endpoint::Finite(x), Endpoint::Finite(y)) => crabgresql_executor::compare_values(ty, x, y),
    }
}

/// Bind and const-fold a partition-bound expression against the key column's
/// type, yielding the concrete [`Value`] used for ordering/overlap checks.
/// Partition bounds must be constants, so binding happens in an empty scope
/// (column references fail) and evaluation runs with no row.
fn fold_bound_value(
    expr: &ast::Expr,
    key_col: &Column,
    type_catalog: &Arc<dyn TypeCatalog>,
    ctx: &ExecContext,
) -> Result<Value, PgError> {
    let bound = crabgresql_binder::bind_column_default(expr, key_col, type_catalog)?;
    // The session's context, not a default one: a `timestamptz` bound is a wall
    // clock read in the *defining* session's zone, exactly as the INSERT that
    // routes a row reads its value. Folding under UTC here while routing under
    // the session zone put rows in the wrong partition, and the wrong bound is
    // persisted. It carries the session's GUCs for the same reason — a bound
    // may call `current_setting()`.
    Ok(crabgresql_executor::eval::eval(&bound, &[], ctx)?)
}

/// Convert one incoming `FOR VALUES` datum into its storage form plus its
/// ordered [`Endpoint`]. A finite bound is folded to a typed [`Value`] once,
/// here, and stored as-is — no text round-trip.
fn incoming_endpoint(
    value: &ast::PartitionBoundValue,
    key_col: &Column,
    type_catalog: &Arc<dyn TypeCatalog>,
    ctx: &ExecContext,
) -> Result<(PartitionBoundDatum, Endpoint), PgError> {
    match value {
        ast::PartitionBoundValue::MinValue => Ok((PartitionBoundDatum::MinValue, Endpoint::NegInf)),
        ast::PartitionBoundValue::MaxValue => Ok((PartitionBoundDatum::MaxValue, Endpoint::PosInf)),
        ast::PartitionBoundValue::Expr(expr) => {
            let value = fold_bound_value(expr, key_col, type_catalog, ctx)?;
            // A NULL bound has no place in the RANGE order (and would panic the
            // downstream `compare_values`); reject it as PG does.
            if value == Value::Null {
                return Err(PgError::new(
                    sqlstate::INVALID_OBJECT_DEFINITION,
                    "cannot specify NULL in range bound",
                ));
            }
            Ok((
                PartitionBoundDatum::Value(value.clone()),
                Endpoint::Finite(value),
            ))
        }
    }
}

/// Recompute a persisted sibling datum's ordered [`Endpoint`] for overlap
/// checks. A stored finite bound is already the folded [`Value`].
fn stored_endpoint(datum: &PartitionBoundDatum) -> Endpoint {
    match datum {
        PartitionBoundDatum::MinValue => Endpoint::NegInf,
        PartitionBoundDatum::MaxValue => Endpoint::PosInf,
        PartitionBoundDatum::Value(v) => Endpoint::Finite(v.clone()),
    }
}

/// Resolve the relation a `PARTITION OF` or `INHERITS (...)` clause names,
/// returning it with its bare name for error text. Both clauses report a
/// non-table relation of that name as wrong-object-type rather than as missing,
/// and both use PG's inheritance wording for it.
///
/// An **unqualified** name is looked for in `temp_schema` before the global
/// engine, because that is the order a read resolves in and a temp table shadows
/// a permanent one of the same name. DDL runs against the raw engine rather than
/// the session overlay, whose `resolve` would do this for us, so without the
/// extra probe naming a temp parent reports `relation "t" does not exist` about a
/// relation that plainly does — worse than the "not supported" both clauses
/// actually mean to say about it.
fn resolve_parent_relation(
    engine: &Arc<dyn TableEngine>,
    parent_ref: &ast::ObjectName,
    temp_schema: &str,
) -> Result<(Arc<dyn TableAm>, String), PgError> {
    let (qual, name) = split_object_name(parent_ref, "relation")?;
    let resolved = match qual.as_deref() {
        None => engine
            .resolve(Some(temp_schema), &name)
            .or_else(|_| engine.resolve(None, &name)),
        Some(_) => engine.resolve(qual.as_deref(), &name),
    };
    let table = resolved.map_err(|_| {
        // A view or sequence of that name exists but is not a table: PG reports
        // wrong-object-type, not "does not exist".
        let is_non_table_relation = engine.resolve_view(qual.as_deref(), &name).is_some()
            || engine
                .sequence(qual.as_deref().unwrap_or("public"), &name)
                .is_some();
        if is_non_table_relation {
            PgError::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("inherited relation \"{name}\" is not a table or foreign table"),
            )
        } else {
            PgError::new(
                sqlstate::UNDEFINED_TABLE,
                format!("relation \"{name}\" does not exist"),
            )
        }
    })?;
    Ok((table, name))
}

/// Fold the columns of a table's `INHERITS (...)` parents together with its own
/// declared columns, returning the merged layout and the parent links to persist.
///
/// The `merging ...` notices go to the session sink rather than into the return
/// value, because PostgreSQL raises them even when the merge then fails —
/// `NOTICE: merging column "a" ...` followed by `ERROR: column "a" has a type
/// conflict`. A `Result`'s `Ok` half cannot carry them past that error.
///
/// Two passes:
///
/// 1. The parents, left to right. A name seen for the first time is appended; a
///    name a previous parent already contributed is merged into that earlier
///    *position*, which is why `stud_emp INHERITS (emp, student)` orders its
///    columns `name, age, location, salary, manager, gpa` rather than
///    interleaving `student`'s copy of `person`'s columns a second time.
/// 2. The child's own columns. Again by name, again merging in place — so a
///    child that redeclares an inherited column refines it rather than
///    duplicating it, and only genuinely new names extend the layout.
///
/// Merging is conservative in both passes: NOT NULL is OR'd (either side may
/// tighten), a default flows down from whichever side has one, and a conflict in
/// type or collation is an error rather than a silent pick.
///
/// Two parents disagreeing about a DEFAULT is only *provisionally* an error. The
/// hint PG prints for it — "specify a default explicitly" — describes a
/// resolution the child is allowed to supply, so the conflict is recorded in
/// pass 1 and raised after pass 2, once the child has had its chance to settle
/// it.
fn merge_inherited_columns(
    engine: &Arc<dyn TableEngine>,
    parents: &[ast::ObjectName],
    child_temporary: bool,
    temp_schema: &str,
    own: Vec<Column>,
    notices: &SessionNotices,
) -> Result<(Vec<Column>, Vec<InheritParent>), PgError> {
    let mut merged: Vec<Column> = Vec::new();
    let mut links: Vec<InheritParent> = Vec::new();
    // Columns whose parents supplied different defaults, pending the child's
    // own declaration.
    let mut default_conflicts: Vec<String> = Vec::new();

    for parent_ref in parents {
        let (parent, parent_name) = resolve_parent_relation(engine, parent_ref, temp_schema)?;
        let parent_schema = parent.schema();
        let link = InheritParent {
            namespace: parent_schema.namespace.clone(),
            name: parent_schema.name.clone(),
        };
        if links.contains(&link) {
            return Err(PgError::new(
                sqlstate::DUPLICATE_TABLE,
                format!("relation \"{parent_name}\" would be inherited from more than once"),
            ));
        }
        // A partitioned parent holds no rows and a partition enforces a bound;
        // an inheritance child of either would need routing rules this slice
        // does not have, so both are refused at DDL rather than half-honored.
        if parent_schema.partition_scheme.is_some() {
            return Err(PgError::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("cannot inherit from partitioned table \"{parent_name}\""),
            ));
        }
        if parent_schema.partition_of.is_some() {
            return Err(PgError::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("cannot inherit from partition \"{parent_name}\""),
            ));
        }
        // PG allows a temp child of a temp parent, and a temp child of a
        // permanent one. We allow neither, and the reason is the fan-out rather
        // than the DDL: descendants are discovered from the engine-wide link set,
        // which is not filtered by session, so another session reading a
        // permanent parent would find a temp child it cannot resolve and fail
        // the SELECT outright. Refusing at CREATE keeps that from being built.
        //
        // Tested before the parent's persistence so a temp/temp hierarchy — the
        // combination this gap is actually about — gets *this* message. The
        // other way round, it drew the permanent-child wording below and called
        // a temporary parent's temporary child a cross-session problem.
        if child_temporary {
            return Err(PgError::feature_not_supported(
                "temporary tables in an inheritance hierarchy are not supported yet",
            ));
        }
        // A permanent child of a temporary parent would outlive its parent.
        if parent_schema.persistence == RelPersistence::Temporary {
            return Err(PgError::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("cannot inherit from temporary relation \"{parent_name}\""),
            ));
        }
        // An engine-managed parent is refused for the same reason the guard on
        // the table being created refuses an engine-managed child: such a
        // relation reads through its own storage leaves, and mixing those into an
        // Append beside a heap descendant takes the whole node off the batch path
        // — silently, for every read of the parent. The child-side guard alone
        // left this reachable from the other end.
        if parent_schema.access_method.is_engine_managed() {
            return Err(PgError::feature_not_supported(format!(
                "table access method \"{}\" does not support inheritance",
                parent_schema.access_method.as_str(),
            )));
        }
        links.push(link);

        for col in &parent_schema.columns {
            let Some(existing) = merged.iter_mut().find(|c| c.name == col.name) else {
                merged.push(col.clone());
                continue;
            };
            // Raised before the merge is attempted, because PostgreSQL reports
            // the merge and *then* the conflict that stopped it.
            notices.push(Notice::notice(
                format!(
                    "merging multiple inherited definitions of column \"{}\"",
                    col.name
                ),
                None,
            ));
            merge_column_into(existing, col, &col.name, true)?;
            // Both sides having a default, and disagreeing, is the one clash the
            // child can still resolve.
            match (&existing.default, &col.default) {
                (Some(a), Some(b)) if a != b => {
                    if !default_conflicts.contains(&col.name) {
                        default_conflicts.push(col.name.clone());
                    }
                }
                (None, Some(_)) => existing.default = col.default.clone(),
                _ => {}
            }
        }
    }

    for (own_position, col) in own.into_iter().enumerate() {
        let Some(position) = merged.iter().position(|c| c.name == col.name) else {
            merged.push(col);
            continue;
        };
        // PG distinguishes the two cases by whether the column had to move:
        // declared at the position it already merged into, it is a plain merge;
        // declared anywhere else, PG says so and explains where it went. Raised
        // before the merge is attempted, as in pass 1.
        notices.push(if own_position == position {
            Notice::notice(
                format!("merging column \"{}\" with inherited definition", col.name),
                None,
            )
        } else {
            Notice::notice(
                format!(
                    "moving and merging column \"{}\" with inherited definition",
                    col.name
                ),
                Some(
                    "User-specified column moved to the position of the inherited column."
                        .to_string(),
                ),
            )
        });
        merge_column_into(&mut merged[position], &col, &col.name, false)?;
        // The child's own declaration is the resolution PG's hint asks for, so
        // it settles any disagreement its parents had.
        if col.default.is_some() {
            merged[position].default = col.default.clone();
            default_conflicts.retain(|name| *name != col.name);
        }
    }

    if let Some(name) = default_conflicts.first() {
        return Err(PgError::new(
            // 42611 invalid_column_definition.
            "42611",
            format!("column \"{name}\" inherits conflicting default values"),
        )
        .with_hint("To resolve the conflict, specify a default explicitly."));
    }

    Ok((merged, links))
}

/// A column's type as PostgreSQL names it in an error message.
///
/// Goes through the same `format_type` spelling `\d` uses, so a type reported in
/// a DETAIL and the same type in a catalog listing cannot drift apart. The
/// modifier has to be converted first: `format_type` speaks the
/// `pg_attribute.atttypmod` dialect (where a character type carries its varlena
/// header) and `Column::typmod` does not, so a `char(5)` column would otherwise
/// render as `character(1)`.
fn column_type_name(column: &Column) -> String {
    column
        .ty
        .format_type(Some(column.atttypmod()))
        .unwrap_or_else(|| column.ty.name().to_string())
}

/// Fold `incoming` into the already-merged column `existing`, or report the
/// conflict that stops it. `inherited` selects PG's two spellings of the same
/// complaint: a clash between two parents names the column as inherited, a clash
/// between the child's own declaration and what it inherited does not.
///
/// DEFAULTs are the caller's business, not this function's: a disagreement
/// between two parents is not final until the child has declared its own
/// columns, so resolving it needs a view of both passes.
fn merge_column_into(
    existing: &mut Column,
    incoming: &Column,
    name: &str,
    inherited: bool,
) -> Result<(), PgError> {
    let qualifier = if inherited {
        "inherited column"
    } else {
        "column"
    };
    if existing.ty != incoming.ty || existing.typmod != incoming.typmod {
        return Err(PgError::new(
            sqlstate::DATATYPE_MISMATCH,
            format!("{qualifier} \"{name}\" has a type conflict"),
        )
        .with_detail(format!(
            "{} versus {}",
            column_type_name(existing),
            column_type_name(incoming),
        )));
    }
    if existing.collation != incoming.collation {
        return Err(PgError::new(
            // 42P21 collation_mismatch.
            "42P21",
            format!("{qualifier} \"{name}\" has a collation conflict"),
        ));
    }
    // NOT NULL is a restriction: either side may impose it, neither may lift it.
    if !incoming.nullable {
        existing.nullable = false;
        if existing.not_null_constraint.is_none() {
            existing.not_null_constraint = incoming.not_null_constraint.clone();
        }
    }
    Ok(())
}

/// `CREATE TABLE <child> PARTITION OF <parent> FOR VALUES FROM (...) TO (...)`:
/// create a leaf partition as an ordinary heap table that inherits the parent's
/// columns and records its bound. RANGE only; the bound is validated non-empty
/// and non-overlapping with existing siblings.
fn execute_create_partition(
    engine: &Arc<dyn TableEngine>,
    type_catalog: &Arc<dyn TypeCatalog>,
    create: &ast::CreateTable,
    namespace: &str,
    name: &str,
    parent_ref: &ast::ObjectName,
    temp_schema: &str,
    ctx: &ExecContext,
) -> Result<QueryResult, PgError> {
    // A partition inherits its shape from the parent: no redeclared columns,
    // constraints, or sub-partitioning in this slice.
    if !create.columns.is_empty() || !create.constraints.is_empty() {
        return Err(PgError::feature_not_supported(
            "column and constraint definitions on a partition are not supported yet",
        ));
    }
    if create.partition_by.is_some() {
        return Err(PgError::feature_not_supported(
            "sub-partitioning is not supported yet",
        ));
    }
    let (parent, parent_name) = resolve_parent_relation(engine, parent_ref, temp_schema)?;
    let parent_schema = parent.schema();
    let Some(scheme) = &parent_schema.partition_scheme else {
        return Err(PgError::new(
            sqlstate::WRONG_OBJECT_TYPE,
            format!("\"{parent_name}\" is not partitioned"),
        ));
    };
    // Only single-column RANGE keys exist so far.
    if scheme.key_columns.len() != 1 {
        return Err(PgError::feature_not_supported(
            "multi-column partition keys are not supported yet",
        ));
    }
    let key_col = &parent_schema.columns[scheme.key_columns[0]];
    let Some(for_values) = &create.for_values else {
        return Err(PgError::syntax("missing FOR VALUES for partition"));
    };
    let (from_spec, to_spec) = match for_values {
        ast::ForValues::From { from, to } => (from, to),
        ast::ForValues::In(_) => {
            return Err(PgError::feature_not_supported(
                "LIST partition bounds (FOR VALUES IN) are not supported yet",
            ));
        }
        ast::ForValues::With { .. } => {
            return Err(PgError::feature_not_supported(
                "HASH partition bounds (FOR VALUES WITH) are not supported yet",
            ));
        }
        ast::ForValues::Default => {
            return Err(PgError::feature_not_supported(
                "default partitions are not supported yet",
            ));
        }
    };
    if from_spec.len() != 1 || to_spec.len() != 1 {
        return Err(PgError::new(
            sqlstate::INVALID_OBJECT_DEFINITION,
            "FROM/TO must specify exactly one value per partition key column",
        ));
    }
    let (from_datum, lower) = incoming_endpoint(&from_spec[0], key_col, type_catalog, ctx)?;
    let (to_datum, upper) = incoming_endpoint(&to_spec[0], key_col, type_catalog, ctx)?;
    // The bound must be non-empty: lower strictly below upper.
    if endpoint_cmp(&lower, &upper, key_col.ty) != std::cmp::Ordering::Less {
        return Err(PgError::new(
            sqlstate::INVALID_OBJECT_DEFINITION,
            format!("empty range bound specified for partition \"{name}\""),
        ));
    }
    // Reject overlap with any existing sibling of the same parent. Two half-open
    // ranges [lo, hi) overlap iff lo_a < hi_b && lo_b < hi_a.
    for sibling in engine.relation_metadata() {
        let Some(part) = &sibling.schema.partition_of else {
            continue;
        };
        if part.parent_namespace != parent_schema.namespace
            || part.parent_name != parent_schema.name
        {
            continue;
        }
        // A relation of this exact name already existing is a name collision, not
        // a range overlap: skip it here and let `create_table` below report it
        // (42P07, or a no-op under IF NOT EXISTS) — otherwise the partition would
        // be reported as overlapping itself.
        if sibling.schema.namespace == namespace && sibling.schema.name == name {
            continue;
        }
        let sib_lo = stored_endpoint(&part.bound.from[0]);
        let sib_hi = stored_endpoint(&part.bound.to[0]);
        if endpoint_cmp(&lower, &sib_hi, key_col.ty) == std::cmp::Ordering::Less
            && endpoint_cmp(&sib_lo, &upper, key_col.ty) == std::cmp::Ordering::Less
        {
            return Err(PgError::new(
                sqlstate::INVALID_OBJECT_DEFINITION,
                format!(
                    "partition \"{name}\" would overlap partition \"{}\"",
                    sibling.schema.name
                ),
            ));
        }
    }
    let schema = TableSchema {
        name: name.to_string(),
        namespace: namespace.to_string(),
        columns: parent_schema.columns.clone(),
        persistence: RelPersistence::Permanent,
        access_method: TableAccessMethod::Heap,
        partition_scheme: None,
        partition_of: Some(PartitionOf {
            parent_namespace: parent_schema.namespace.clone(),
            parent_name: parent_schema.name.clone(),
            key_columns: scheme.key_columns.clone(),
            bound: PartitionBound {
                from: vec![from_datum],
                to: vec![to_datum],
            },
        }),
        // Partitioning and inheritance are mutually exclusive links.
        inherits: Vec::new(),
        // A leaf partition is always a heap, which declares no layout order.
        sort_key: Vec::new(),
        // PostgreSQL copies a partitioned parent's CHECK constraints into every
        // leaf. Nothing to copy here: declaring one on a partitioned table is
        // refused at DDL, so the parent never has any.
        checks: Vec::new(),
    };
    match engine.create_table(schema) {
        Ok(_) => Ok(QueryResult::command("CREATE TABLE")),
        Err(StorageError::TableAlreadyExists(_)) if create.if_not_exists => {
            Ok(QueryResult::command("CREATE TABLE"))
        }
        Err(e) => Err(e.into()),
    }
}

/// `CREATE TABLE <name> [ (cols) ] AS <query>`: derive the new table's column
/// shape from the query (à la `CREATE VIEW`), create it, then stream the query's
/// rows into it (à la `INSERT ... SELECT`). The completion tag is `SELECT <n>`,
/// matching PG's CTAS / `SELECT INTO`.
#[allow(clippy::too_many_arguments)]
fn execute_create_table_as(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TableEngine>,
    type_catalog: &Arc<dyn TypeCatalog>,
    catalog_ops: &Arc<dyn CatalogOps>,
    global_catalog: &Arc<GlobalCatalog>,
    txnmgr: &Arc<TransactionManager>,
    create: &ast::CreateTable,
    session: &mut Session,
) -> Result<QueryResult, PgError> {
    let access_method = create_table_access_method(create)?;
    let (schema_qual, name) = split_object_name(&create.name, "relation")?;
    // Resolve the target namespace the same way a plain CREATE TABLE does: a
    // TEMP table lives in the session temp keyspace (only `pg_temp` qualifiers
    // allowed), everything else resolves/validates its schema qualifier.
    let namespace = if create.temporary {
        match schema_qual.as_deref() {
            None | Some("pg_temp") => {}
            Some(other) if other == session.temp_schema => {}
            Some(_) => {
                return Err(PgError::feature_not_supported(
                    "cannot create a temporary relation in a non-temporary schema",
                ));
            }
        }
        session.temp_schema.clone()
    } else {
        resolve_create_namespace(engine, schema_qual.as_deref(), &name)?
    };
    // Reject CTAS forms we don't implement rather than silently dropping them.
    // A table cannot be `OR REPLACE`d; ON COMMIT needs the M2 txn engine; table
    // constraints / LIKE / CLONE / storage options are not derived from a query.
    // `PARTITION BY` and `INHERITS` are unimplemented on the plain path too, and
    // the Redshift trio parses ungated — every one of them would otherwise be
    // accepted here and thrown away, which is what the plain path refuses to do.
    if create.or_replace
        || create.on_commit.is_some()
        || !create.constraints.is_empty()
        || create.like.is_some()
        || create.clone.is_some()
        || !matches!(create.table_options, ast::CreateTableOptions::None)
        || create.partition_by.is_some()
        || create.inherits.is_some()
        || create.sortkey.is_some()
        || create.distkey.is_some()
        || create.diststyle.is_some()
    {
        return Err(PgError::feature_not_supported(
            "this CREATE TABLE ... AS form is not supported yet",
        ));
    }
    // Every table — temp or permanent — lives in the one shared engine now; a
    // temp table is a memory table in this session's `pg_temp_N` namespace. The
    // new table is later opened through the overlay `catalog`, which resolves the
    // temp namespace too.
    let target: Arc<dyn TableEngine> = engine.clone();
    let persistence = if create.temporary {
        RelPersistence::Temporary
    } else if create.unlogged {
        RelPersistence::Unlogged
    } else {
        RelPersistence::Permanent
    };
    if access_method.is_engine_managed() && persistence != RelPersistence::Permanent {
        return Err(PgError::feature_not_supported(format!(
            "table access method \"{}\" only supports permanent tables",
            access_method.as_str(),
        )));
    }

    // Bind the defining query and take its output column shape.
    let query = create.query.as_ref().expect("CTAS dispatch guards query");
    let plan = bind_query(catalog, type_catalog, query)?;
    let mut cols = output_columns_of(&plan)?;

    // An explicit column list renames the leading outputs; names only (no types
    // or constraints), and it may not exceed the query's column count.
    if !create.columns.is_empty() {
        for col in &create.columns {
            if col.data_type != ast::DataType::Unspecified || !col.options.is_empty() {
                return Err(PgError::feature_not_supported(
                    "column types or constraints in CREATE TABLE ... AS are not supported yet",
                ));
            }
        }
        if create.columns.len() > cols.len() {
            return Err(PgError::new(
                sqlstate::SYNTAX_ERROR,
                "CREATE TABLE AS specifies too many column names",
            ));
        }
        for (out, def) in cols.iter_mut().zip(&create.columns) {
            out.name = normalize_ident(&def.name);
        }
    }

    // Duplicate output column names are rejected, as for a plain table.
    let mut seen = HashSet::new();
    for col in &cols {
        if !seen.insert(col.name.clone()) {
            return Err(PgError::new(
                sqlstate::DUPLICATE_COLUMN,
                format!("column \"{}\" specified more than once", col.name),
            ));
        }
    }

    // A projected `reg*` cannot be stored, for the same reason a declared one
    // cannot (see `reject_stored_reg_type`).
    for c in &cols {
        reject_stored_reg_type(c.ty, &c.name)?;
    }
    let mut schema = TableSchema {
        name: name.clone(),
        namespace: namespace.clone(),
        columns: cols
            .iter()
            .map(|c| {
                let mut col = Column::with_typmod(c.name.clone(), c.ty, c.typmod);
                col.collation = c.collation;
                col
            })
            .collect(),
        persistence,
        access_method,
        partition_scheme: None,
        partition_of: None,
        // CTAS derives its shape from a query; `INHERITS` is rejected above.
        inherits: Vec::new(),
        sort_key: Vec::new(),
        // A `CREATE TABLE AS` carries no constraint clauses at all — the form
        // that would declare one is rejected as unsupported.
        checks: Vec::new(),
    };
    // As on the plain path: an `IF NOT EXISTS` re-run against an existing
    // relation is a no-op and must not trip over the sort-key rule.
    if create.if_not_exists && engine.resolve(Some(&namespace), &name).is_ok() {
        return Ok(QueryResult::Command {
            tag: "CREATE TABLE AS".into(),
            notices: vec![Notice::notice(
                format!("relation \"{name}\" already exists, skipping"),
                None,
            )],
        });
    }
    // CTAS declares no constraints (the guard above rejects them), so there is
    // no PRIMARY KEY to fall back on: an engine-managed CTAS must spell its key.
    schema.sort_key = build_sort_key(create.order_by.as_ref(), access_method, &schema, None)
        .map_err(|e| annotate_ctas_order_by(e, create))?;

    // Create the table first so a name collision short-circuits before the query
    // runs. IF NOT EXISTS on an existing relation runs nothing (PG NOTICE).
    match target.create_table(schema) {
        Ok(_) => {}
        Err(StorageError::TableAlreadyExists(_)) if create.if_not_exists => {
            return Ok(QueryResult::Command {
                tag: "CREATE TABLE AS".into(),
                notices: vec![Notice::notice(
                    format!("relation \"{name}\" already exists, skipping"),
                    None,
                )],
            });
        }
        Err(e) => return Err(e.into()),
    }

    // Populate: build an INSERT ... SELECT over the new table. Columns are derived
    // straight from the query's output types, so each target column is an identity
    // ColumnRef into the source row — no coercion needed.
    let table = match catalog.open_table(&name) {
        Ok(table) => table,
        Err(e) => {
            let _ = target.drop_table(&namespace, &name);
            return Err(e.into());
        }
    };
    let projections = cols
        .iter()
        .enumerate()
        .map(|(index, c)| BoundExpr::ColumnRef { index, ty: c.ty })
        .collect();
    let logical = LogicalPlan::Insert {
        table,
        source: InsertSource::Query {
            input: Box::new(plan),
            projections,
        },
        returning: None,
        // CTAS populates a freshly-created ordinary table — never partitioned.
        routing: None,
        // `CREATE TABLE … AS` has no FREEZE spelling, and this table's DDL is not
        // transactional, so there would be no storage for a rollback to discard.
        freeze: false,
    };

    // Run the populate INSERT through the standard write tail. The DDL catalog
    // write above is not MVCC-transactional (as with every DDL path here), so on
    // a populate failure we best-effort drop the half-created table.
    let read_only = read_only_active(session);
    let txn = build_txn(txnmgr, session, true);
    let (routines, command_counter) =
        statement_runtime(catalog, type_catalog, global_catalog, session);
    let exec_ctx = session.exec_context_for_statement(
        engine,
        catalog_ops,
        type_catalog,
        routines,
        Arc::clone(&command_counter),
        read_only,
    );
    let exec = match execute(crabgresql_planner::plan(logical), &exec_ctx, &txn) {
        Ok(exec) => exec,
        Err(e) => {
            let _ = finalize_statement(txnmgr, session, &txn, true, false, Some(&command_counter));
            let _ = target.drop_table(&namespace, &name);
            return Err(e.into());
        }
    };
    // The source query can call a routine, whose body advances the counter, so
    // the block's command id has to be read back rather than merely bumped.
    finalize_statement(txnmgr, session, &txn, true, true, Some(&command_counter))?;
    let n = match exec {
        Execution::Inserted(n) => n,
        // The populate plan is a RETURNING-less INSERT, so execution yields an
        // insert count and nothing else.
        _ => unreachable!("CTAS populate is a RETURNING-less INSERT"),
    };
    Ok(QueryResult::command(format!("SELECT {n}")))
}

/// `CHECK (...) NOT ENFORCED` declares a constraint the server records but never
/// applies. Nothing here models that, and silently enforcing one anyway would be
/// the opposite of what was asked for, so it is refused. `ENFORCED` is the
/// default and is accepted as a no-op.
fn reject_not_enforced(enforced: Option<bool>) -> Result<(), PgError> {
    match enforced {
        Some(false) => Err(PgError::feature_not_supported(
            "not-enforced constraints are not supported yet",
        )),
        _ => Ok(()),
    }
}

/// Fold the `CHECK` constraints of a table's `INHERITS (...)` parents together
/// with its own, the way [`merge_inherited_columns`] does for columns.
///
/// Inheritance here is a *copy*: the child gets its own entry for each of its
/// parents' constraints, which is what makes enforcing the child's own list
/// enforce its parents' too. Each parent's predicate is re-bound against the
/// child's merged layout rather than copied with its positions, because the
/// merge may have placed the same column elsewhere.
///
/// Merging is by **name**, and the rules were probed against PostgreSQL 18.4:
///
/// * two parents contributing the same name with different predicates is
///   `42710 check constraint name "pc" appears multiple times but with
///   different expressions`;
/// * the child redeclaring a parent's constraint with the *same* predicate
///   merges — `NOTICE: merging constraint "pc" with inherited definition`,
///   leaving `conislocal = true` over the inherited `coninhcount`;
/// * the child redeclaring it with a *different* predicate is
///   `42710 constraint "pc" for relation "c" already exists`.
///
/// Notices go to the session sink rather than the return value, for the reason
/// [`merge_inherited_columns`] spells out: PostgreSQL raises them even when the
/// merge then fails.
fn merge_inherited_checks(
    parents: &[Arc<dyn TableAm>],
    own: Vec<CheckConstraint>,
    child: &TableSchema,
    type_catalog: &Arc<dyn TypeCatalog>,
    notices: &SessionNotices,
) -> Result<Vec<CheckConstraint>, PgError> {
    let mut merged: Vec<CheckConstraint> = Vec::new();
    for parent in parents {
        let parent_schema = parent.schema();
        for check in &parent_schema.checks {
            // Re-bound, not copied: `conkey` has to index the child's layout,
            // and this also proves every column the predicate reads survived the
            // merge.
            let columns = rebind_check_columns(&check.expr, child, type_catalog)?;
            match merged.iter_mut().find(|c| c.name == check.name) {
                Some(existing) if existing.expr == check.expr => existing.inhcount += 1,
                Some(_) => {
                    return Err(PgError::new(
                        "42710",
                        format!(
                            "check constraint name \"{}\" appears multiple times but with different expressions",
                            check.name
                        ),
                    ));
                }
                None => merged.push(CheckConstraint {
                    name: check.name.clone(),
                    expr: check.expr.clone(),
                    columns,
                    validated: check.validated,
                    // Inherited, not declared here — until the child's own
                    // clauses below say otherwise.
                    islocal: false,
                    inhcount: 1,
                }),
            }
        }
    }
    for check in own {
        match merged.iter_mut().find(|c| c.name == check.name) {
            Some(existing) if existing.expr == check.expr => {
                notices.push(Notice::notice(
                    format!(
                        "merging constraint \"{}\" with inherited definition",
                        check.name
                    ),
                    None,
                ));
                existing.islocal = true;
            }
            Some(_) => {
                return Err(PgError::new(
                    "42710",
                    format!(
                        "constraint \"{}\" for relation \"{}\" already exists",
                        check.name, child.name
                    ),
                ));
            }
            None => merged.push(check),
        }
    }
    Ok(merged)
}

/// The generated-name stem for an unnamed `CHECK`.
///
/// PostgreSQL keys this on the *predicate*, not on where the clause was written:
/// one referenced column gives `{table}_{column}_check` and anything else gives
/// `{table}_check`. So the table-level `CHECK (y <> 0)` is named `t_y_check`
/// exactly like the column-level spelling, while `CHECK (x + y < 100)` — two
/// columns — is `t_check`. Probed against 18.4.
fn check_name_base(schema: &TableSchema, columns: &[usize]) -> String {
    match columns {
        [only] => format!("{}_{}_check", schema.name, schema.columns[*only].name),
        _ => format!("{}_check", schema.name),
    }
}

/// The column positions a stored predicate reads when bound against `schema` —
/// how an inherited constraint's `conkey` is recomputed for the child.
fn rebind_check_columns(
    expr: &str,
    schema: &TableSchema,
    type_catalog: &Arc<dyn TypeCatalog>,
) -> Result<Vec<usize>, PgError> {
    let parsed = crabgresql_binder::ruleutils::parse_expression(expr).ok_or_else(|| {
        PgError::new(
            sqlstate::INTERNAL_ERROR,
            format!("stored check constraint \"{expr}\" is not a single expression"),
        )
    })?;
    let (_, columns) = crabgresql_binder::bind_check_constraint(&parsed, schema, type_catalog)?;
    Ok(columns)
}

/// Bind, name, and canonicalize a relation's `CHECK` clauses.
///
/// Must run against the relation's **final** column layout: the predicate is
/// stored with `conkey` positions into it, and an expression bound against an
/// earlier layout would silently read the wrong columns.
///
/// Naming follows PostgreSQL: an explicit `CONSTRAINT n` is taken verbatim and
/// collides with 42710, while a generated one comes from [`check_name_base`] and
/// is deduplicated by [`fresh_local_name`] into `base`, `base1`, `base2`… — so
/// two checks on one column become `t_c_check` and `t_c_check1`. `claimed`
/// carries the names already taken by NOT NULL and index-backed constraints, and
/// grows as this runs; `inherited` carries the names the parents are about to
/// contribute, which the merge below has not applied yet.
///
/// The two are consulted differently, and the difference is what makes
/// inheritance work: a **generated** name skips an inherited one and takes a
/// suffix, while an **explicit** one deliberately does not, so that it reaches
/// [`merge_inherited_checks`] and merges with the parent's definition (or
/// collides with it). PostgreSQL draws the same line.
///
/// The collision message is `CREATE TABLE`'s, which names no relation —
/// PostgreSQL says `check constraint "dup" already exists` here and
/// `constraint "dup" for relation "t" already exists` from `ALTER TABLE`, a
/// distinction probed against 18.4.
///
/// The stored text is the deparsed form, not the source: `CHECK (x + y < 100)`
/// becomes `((x + y) < 100)`, which is what `pg_get_expr(conbin, conrelid)`
/// returns and what a later re-bind at DML time re-parses.
fn resolve_checks(
    pending: impl Iterator<Item = (Option<String>, ast::Expr)>,
    schema: &TableSchema,
    claimed: &mut HashSet<String>,
    inherited: &HashSet<String>,
    type_catalog: &Arc<dyn TypeCatalog>,
) -> Result<Vec<CheckConstraint>, PgError> {
    let mut out = Vec::new();
    for (explicit_name, expr) in pending {
        let (_, columns) = crabgresql_binder::bind_check_constraint(&expr, schema, type_catalog)?;
        let name = explicit_name.unwrap_or_else(|| {
            fresh_local_name(
                |c| claimed.contains(c) || inherited.contains(c),
                &check_name_base(schema, &columns),
            )
        });
        if !claimed.insert(name.clone()) {
            return Err(PgError::new(
                "42710",
                format!("check constraint \"{name}\" already exists"),
            ));
        }
        // Deparsing re-parses the source text rather than rendering the bound
        // tree, the same split the DEFAULT path uses: the deparser has the
        // precedence rules, and folding a constant here would rewrite the user's
        // predicate. If it cannot re-parse its own input, keep the source.
        let source = expr.to_string();
        let text =
            crabgresql_binder::ruleutils::deparse_check_expr(&source, &schema.name, type_catalog)
                .unwrap_or(source);
        out.push(CheckConstraint {
            name,
            expr: text,
            columns,
            // `NOT VALID` is rejected at parse-to-plan time, so everything that
            // reaches here was validated by construction — the relation is empty
            // at `CREATE TABLE`, and `ALTER TABLE ADD` scans before it lands.
            validated: true,
            islocal: true,
            inhcount: 0,
        });
    }
    Ok(out)
}

fn reject_deferred_characteristics(
    characteristics: Option<ast::ConstraintCharacteristics>,
) -> Result<(), PgError> {
    if let Some(c) = characteristics
        && (c.deferrable == Some(true) || c.initially.is_some() || c.enforced == Some(false))
    {
        return Err(PgError::feature_not_supported(
            "deferrable or not-enforced constraints are not supported yet",
        ));
    }
    Ok(())
}

/// The columns of a PRIMARY KEY / UNIQUE constraint are plain column names: no
/// ordering, and no repeats.
///
/// PostgreSQL's grammar has no place for `ASC`/`DESC`/`NULLS` there at all — the
/// vendored parser accepts them only because constraint columns share
/// `CREATE INDEX`'s column parser, where they *are* legal — and it rejects a
/// repeated column with 42701.
///
/// Enforcing it here also keeps the layout sort key honest: an engine-managed
/// table with no `ORDER BY` inherits these columns verbatim, so anything this
/// lets through becomes a stored key that `ORDER BY` itself could not express.
fn reject_constraint_key_columns(columns: &[ast::IndexColumn], noun: &str) -> Result<(), PgError> {
    let mut seen: Vec<String> = Vec::new();
    for col in columns {
        let options = &col.column.options;
        if options.asc.is_some() || options.nulls_first.is_some() {
            let token = match options.asc {
                Some(true) => "ASC",
                Some(false) => "DESC",
                None => "NULLS",
            };
            return Err(PgError::syntax(format!(
                "syntax error at or near \"{token}\""
            )));
        }
        // A non-identifier is an expression key; `simple_index_keys` rejects it
        // with its own message, so leave it alone rather than guessing a name.
        let ast::Expr::Identifier(ident) = &col.column.expr else {
            continue;
        };
        let name = normalize_ident(ident);
        if seen.contains(&name) {
            return Err(PgError::new(
                sqlstate::DUPLICATE_COLUMN,
                format!("column \"{name}\" appears twice in {noun} constraint"),
            ));
        }
        seen.push(name);
    }
    Ok(())
}

fn reject_primary_key_options(constraint: &ast::PrimaryKeyConstraint) -> Result<(), PgError> {
    if constraint.index_name.is_some()
        || !constraint.index_options.is_empty()
        || !matches!(constraint.index_type, None | Some(ast::IndexType::BTree))
    {
        return Err(PgError::feature_not_supported(
            "this PRIMARY KEY index form is not supported yet",
        ));
    }
    reject_constraint_key_columns(&constraint.columns, "primary key")
}

fn reject_unique_options(constraint: &ast::UniqueConstraint) -> Result<(), PgError> {
    if constraint.index_name.is_some()
        || !constraint.index_options.is_empty()
        || !constraint.index_type_display.is_none()
        || !matches!(constraint.index_type, None | Some(ast::IndexType::BTree))
    {
        return Err(PgError::feature_not_supported(
            "this UNIQUE index form is not supported yet",
        ));
    }
    reject_constraint_key_columns(&constraint.columns, "unique")
}

fn simple_index_keys(
    schema: &TableSchema,
    columns: &[ast::IndexColumn],
) -> Result<Vec<IndexKey>, PgError> {
    let mut keys = Vec::with_capacity(columns.len());
    for col in columns {
        if col.operator_class.is_some() || col.column.with_fill.is_some() {
            return Err(PgError::feature_not_supported(
                "index operator classes and WITH FILL are not supported yet",
            ));
        }
        let ident = match &col.column.expr {
            ast::Expr::Identifier(ident) => normalize_ident(ident),
            _ => {
                return Err(PgError::feature_not_supported(
                    "expression indexes are not supported yet",
                ));
            }
        };
        // A B-tree / UNIQUE index (and PRIMARY KEY) needs an ordering. Types with
        // no default B-tree operator class (`json`, `point`, `lseg`) are rejected
        // here, matching PostgreSQL — otherwise unique enforcement would later
        // call `compare_values` on an unorderable type and panic the backend.
        let column = resolve_orderable_column(&schema.columns, &ident, "key", Some(OPCLASS_HINT))?;
        keys.push(IndexKey {
            column,
            descending: col.column.options.asc == Some(false),
            nulls_first: col
                .column
                .options
                .nulls_first
                .unwrap_or(col.column.options.asc == Some(false)),
        });
    }
    if keys.is_empty() {
        return Err(PgError::syntax("index must have at least one column"));
    }
    Ok(keys)
}

fn fresh_relation_name(
    engine: &Arc<dyn TableEngine>,
    namespace: &str,
    local: &HashSet<String>,
    base: &str,
) -> String {
    // Relation and index names are unique per schema, so a generated name only has
    // to dodge collisions in `namespace` — scoping the scan here also keeps it from
    // depending on other sessions' `pg_temp_N` relations in the shared engine.
    let exists = |candidate: &str| {
        local.contains(candidate)
            || engine.resolve(Some(namespace), candidate).is_ok()
            || engine
                .relation_metadata()
                .iter()
                .filter(|relation| relation.schema.namespace == namespace)
                .any(|relation| relation.indexes.iter().any(|index| index.name == candidate))
    };
    if !exists(base) {
        return base.to_string();
    }
    for suffix in 1_u64.. {
        let candidate = format!("{base}{suffix}");
        if !exists(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// The name PostgreSQL gives the index behind an unnamed PRIMARY KEY / UNIQUE
/// constraint: `t_pkey` for the primary key (which collapses to the table name,
/// there being at most one), `t_a_b_key` for a unique constraint, named after
/// every key column. Shared by `CREATE TABLE` and `ALTER TABLE ... ADD` so the
/// two spellings of one constraint produce one name.
fn constraint_index_base(
    table: &str,
    schema: &TableSchema,
    constraint: IndexConstraint,
    keys: &[IndexKey],
) -> String {
    match constraint {
        IndexConstraint::PrimaryKey => format!("{table}_pkey"),
        IndexConstraint::Unique => {
            let mut base = table.to_string();
            for key in keys {
                base.push('_');
                base.push_str(&schema.columns[key.column].name);
            }
            base.push_str("_key");
            base
        }
    }
}

/// Pick a free name for an index PG would auto-name. Unlike `fresh_relation_name`
/// this goes through `index_name_exists`, which resolves the table's *effective*
/// namespace — so an index on a temp table dodges the other indexes in that
/// session's `pg_temp_N` schema rather than the ones in `public`.
///
/// `local` holds the names an in-flight statement has already claimed but not
/// yet created. One `ALTER TABLE` may add several constraints, and until they
/// reach the engine none of them is visible to `index_name_exists` — without
/// this, `ADD UNIQUE (a), ADD UNIQUE (a)` would generate `t_a_key` twice.
fn fresh_index_name(
    engine: &Arc<dyn TableEngine>,
    table: &str,
    local: &HashSet<String>,
    base: &str,
) -> String {
    let taken = |candidate: &str| {
        local.contains(candidate)
            || engine.index_name_exists("public", table, candidate)
            || engine.resolve(Some("public"), candidate).is_ok()
    };
    if !taken(base) {
        return base.to_string();
    }
    for suffix in 1_u64.. {
        let candidate = format!("{base}{suffix}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

fn fresh_local_name(taken: impl Fn(&str) -> bool, base: &str) -> String {
    if !taken(base) {
        return base.to_string();
    }
    for suffix in 1_u64.. {
        let candidate = format!("{base}{suffix}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// Shared with cast/typed-literal binding, so CREATE TABLE and `::` casts agree
/// on the type name mapping.
fn map_data_type(dt: &ast::DataType) -> Result<PgType, PgError> {
    crabgresql_binder::map_data_type(dt).map_err(PgError::from)
}

/// Resolve a column's declared type: a built-in, or — when the name is a bare
/// custom identifier — a `CREATE TYPE` name from the catalog (e.g. an enum),
/// yielding `PgType::User(oid)`. A bare name that is neither is an
/// undefined-object error (42704), matching PG (and `resolve_type_ref`).
/// Refuse to give a stored column a `reg*` type.
///
/// A `reg*` value is an OID, and its whole contract is that the OID keeps
/// naming the same object. crabgresql assigns relation OIDs positionally per
/// catalog snapshot (`SystemCatalog::relation_oids`) rather than durably, so
/// unrelated DDL renumbers them: a stored `regclass` would silently come to name
/// a *different* relation, and equality against a freshly cast value would go
/// false. Rejecting the column is the honest boundary until relation OIDs are
/// persistent — using `reg*` in expressions (casts, comparisons, the catalog
/// queries psql sends) is unaffected, since those resolve within one snapshot.
fn reject_stored_reg_type(ty: PgType, column: &str) -> Result<(), PgError> {
    let kind = match ty {
        PgType::Reg(kind) => Some(kind),
        PgType::Array(elem) => match PgType::from_oid(elem) {
            Some(PgType::Reg(kind)) => Some(kind),
            _ => None,
        },
        _ => None,
    };
    match kind {
        None => Ok(()),
        Some(kind) => Err(PgError::feature_not_supported(format!(
            "column \"{column}\" cannot have type {} because relation OIDs are \
             not stable across schema changes yet",
            kind.typname()
        ))),
    }
}

fn resolve_column_type(
    type_catalog: &Arc<dyn TypeCatalog>,
    dt: &ast::DataType,
) -> Result<PgType, PgError> {
    match map_data_type(dt) {
        Ok(t) => Ok(t),
        Err(orig) => match datatype_simple_name(dt) {
            Some(name) if type_catalog.is_shell_type(&name) => Err(PgError::new(
                sqlstate::UNDEFINED_OBJECT,
                format!("type \"{name}\" is only a shell"),
            )),
            Some(name) => match type_catalog.resolve_type(&name) {
                Some(ut) if type_catalog.enum_info(ut.oid).is_some() => Ok(PgType::User(ut.oid)),
                Some(_) => Err(PgError::feature_not_supported(format!(
                    "type \"{name}\" is not supported yet"
                ))),
                None if crabgresql_catalog::is_builtin_type_name(&name) => Err(orig),
                None => Err(PgError::new(
                    sqlstate::UNDEFINED_OBJECT,
                    format!("type \"{name}\" does not exist"),
                )),
            },
            None => Err(orig),
        },
    }
}

/// Wrap catalog-produced notices as NOTICE-severity messages for the wire.
fn to_notices(notices: Vec<CatalogNotice>) -> Vec<Notice> {
    notices
        .into_iter()
        .map(|n| Notice {
            location: n.position,
            ..Notice::notice(n.message, n.detail)
        })
        .collect()
}

/// The single lowercased name of a bare custom type (e.g. `xfloat8`, `cstring`),
/// or `None` for a built-in `DataType` variant.
fn datatype_simple_name(dt: &ast::DataType) -> Option<String> {
    match dt {
        ast::DataType::Custom(obj, mods) if mods.is_empty() && obj.0.len() == 1 => {
            obj.0[0].as_ident().map(normalize_ident)
        }
        _ => None,
    }
}

/// Resolve a SQL type name to a catalog [`TypeRef`]: the `cstring` pseudo-type, a
/// user-defined type (consulting the catalog), or a built-in type. A bare name
/// that is neither a user type nor a known built-in is an undefined-object error
/// (42704), matching PG — not a "not supported" feature gap.
fn resolve_type_ref(catalog: &GlobalCatalog, dt: &ast::DataType) -> Result<TypeRef, PgError> {
    if let Some(name) = datatype_simple_name(dt) {
        if name == "cstring" {
            return Ok(TypeRef::Cstring);
        }
        if catalog.is_user_type(&name) {
            return Ok(TypeRef::User(name));
        }
        // A bare custom name is either a built-in spelled as a custom identifier
        // (bpchar/varchar/name) or a genuinely unknown type.
        return match crabgresql_binder::map_data_type(dt) {
            Ok(pg) => Ok(TypeRef::Builtin(pg)),
            Err(_) => Err(PgError::new(
                sqlstate::UNDEFINED_OBJECT,
                format!("type \"{name}\" does not exist"),
            )),
        };
    }
    Ok(TypeRef::Builtin(map_data_type(dt)?))
}

/// The built-in type a catalog type name refers to (for a `LIKE` clause), so its
/// `pg_type.typlen` can be read from the authoritative `PgType::typlen()` rather
/// than a second width table. The name table is `PgType::from_name`, shared with
/// the binder so a `LIKE` clause and a cast agree on what a spelling means.
fn builtin_type_by_name(name: &str) -> Option<PgType> {
    PgType::from_name(name)
}

/// Derive a base type's physical width (`pg_type.typlen`) and backing builtin
/// from its CREATE TYPE options: `INTERNALLENGTH` or `LIKE`. The backing (the
/// `LIKE` type's representation) drives query-time `WITHOUT FUNCTION` casts;
/// `INTERNALLENGTH` alone yields no backing. Defaults to variable width (-1).
fn type_shape_from_options(
    catalog: &GlobalCatalog,
    options: &[ast::UserDefinedTypeSqlDefinitionOption],
) -> Result<(i32, Option<PgType>), PgError> {
    use ast::UserDefinedTypeSqlDefinitionOption as Opt;
    let mut typlen = -1;
    let mut backing = None;
    for opt in options {
        match opt {
            Opt::InternalLength(ast::UserDefinedTypeInternalLength::Fixed(n)) => typlen = *n as i32,
            Opt::InternalLength(ast::UserDefinedTypeInternalLength::Variable) => typlen = -1,
            Opt::Like(name) => {
                // `LIKE builtin` copies its width and representation; `LIKE
                // usertype` inherits the user type's width and backing. Only an
                // unqualified name can resolve — crabgresql has no schema
                // namespaces, so a schema-qualified target never names a
                // builtin/user type (cf. `single_object_name`, which likewise
                // rejects qualified names for the type being created).
                let ident = match name.0.as_slice() {
                    [part] => part.as_ident(),
                    _ => None,
                };
                let n = ident.map(normalize_ident);
                // A `LIKE` target is a type *name as written*, so quoting is
                // significant exactly as it is for a cast: unquoted `char` is
                // the `char(1)` keyword (`bpchar`), quoted `"char"` is the
                // one-byte type. `PgType::from_name` is a catalog-typname
                // lookup and cannot see the difference, so ask the grammar.
                let builtin = ident.and_then(|i| match i.quote_style {
                    None => crabgresql_binder::builtin_type_from_syntax(&i.value),
                    Some(_) => n.as_deref().and_then(builtin_type_by_name),
                });
                if let Some(t) = builtin {
                    typlen = t.typlen() as i32;
                    backing = Some(t);
                } else if let Some(len) = n.as_deref().and_then(|n| catalog.user_type_typlen(n)) {
                    typlen = len;
                    let Some(name) = n.as_deref() else {
                        return Err(PgError::new("XX000", "type name is missing"));
                    };
                    backing = catalog.user_type_backing(name);
                } else {
                    // An unresolvable target is an undefined-object error
                    // (42704), matching PG — reported verbatim (qualifier
                    // included), with a caret at the start of the name.
                    let display = name
                        .0
                        .iter()
                        .map(|p| p.as_ident().map(normalize_ident).unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join(".");
                    let mut err = PgError::new(
                        sqlstate::UNDEFINED_OBJECT,
                        format!("type \"{display}\" does not exist"),
                    );
                    let start = name
                        .0
                        .first()
                        .and_then(|p| p.as_ident())
                        .map(|i| i.span.start);
                    if let Some(start) = start {
                        if start.line != 0 {
                            err.location = Some((start.line, start.column));
                        }
                    }
                    return Err(err);
                }
            }
            _ => {}
        }
    }
    Ok((typlen, backing))
}

/// `CREATE TYPE name;` (shell) or `CREATE TYPE name (INPUT=..., OUTPUT=..., ...)`.
fn execute_create_type(
    catalog: &GlobalCatalog,
    name: &ast::ObjectName,
    representation: &Option<ast::UserDefinedTypeRepresentation>,
) -> Result<QueryResult, PgError> {
    let tname = single_object_name(name, "type")?;
    if crabgresql_catalog::is_builtin_type_name(&tname) {
        return Err(PgError::new(
            sqlstate::DUPLICATE_OBJECT,
            format!("type \"{tname}\" already exists"),
        ));
    }
    let notices = match representation {
        None => catalog.create_shell_type(&tname)?,
        Some(ast::UserDefinedTypeRepresentation::SqlDefinition { options }) => {
            let (typlen, backing) = type_shape_from_options(catalog, options)?;
            catalog.define_type(&tname, typlen, backing)?
        }
        Some(ast::UserDefinedTypeRepresentation::Enum { labels }) => {
            // Labels are stored verbatim (case-sensitive) — an enum label is a
            // string constant in PG, not an identifier, so it is never folded.
            let labels = labels.iter().map(|i| i.value.clone()).collect();
            catalog.create_enum_type(&tname, labels)?
        }
        Some(_) => {
            return Err(PgError::feature_not_supported(
                "CREATE TYPE AS (composite / range) is not supported yet",
            ));
        }
    };
    Ok(QueryResult::Command {
        tag: "CREATE TYPE".into(),
        notices: to_notices(notices),
    })
}

/// Reject an enum-only `ALTER TYPE` operation targeting a builtin type. Builtins
/// exist but are never enums, so PG reports `<type> is not an enum`
/// (WRONG_OBJECT_TYPE) rather than the catalog's "type does not exist" (builtins
/// are absent from the user-type map). The name is rendered as PG's `format_type_be`
/// spelling (e.g. `integer`, not `int4`) when known.
fn reject_non_enum_builtin(name: &str) -> Result<(), PgError> {
    if crabgresql_catalog::is_builtin_type_name(name) {
        let display =
            builtin_type_by_name(name).map_or_else(|| name.to_string(), |t| t.name().to_string());
        return Err(PgError::new(
            sqlstate::WRONG_OBJECT_TYPE,
            format!("{display} is not an enum"),
        ));
    }
    Ok(())
}

/// `ALTER TYPE`. Supports `RENAME TO` (any user type) and, for enums,
/// `ADD VALUE` and `RENAME VALUE`. The mutation and its PG error text/SQLSTATE
/// live on [`GlobalCatalog`]; here we only normalize the AST names.
fn execute_alter_type(
    catalog: &GlobalCatalog,
    alter: &ast::AlterType,
) -> Result<QueryResult, PgError> {
    let tname = single_object_name(&alter.name, "type")?;
    let notices = match &alter.operation {
        ast::AlterTypeOperation::Rename(rename) => {
            // The builtin-name collision of the target is enforced inside
            // rename_type, after it confirms the source type exists.
            catalog.rename_type(&tname, &normalize_ident(&rename.new_name))?
        }
        ast::AlterTypeOperation::AddValue(add) => {
            reject_non_enum_builtin(&tname)?;
            // Enum labels are string constants, not identifiers — stored verbatim.
            let position = match &add.position {
                Some(ast::AlterTypeAddValuePosition::Before(n)) => Some((true, n.value.clone())),
                Some(ast::AlterTypeAddValuePosition::After(n)) => Some((false, n.value.clone())),
                None => None,
            };
            catalog.add_enum_value(&tname, &add.value.value, add.if_not_exists, position)?
        }
        ast::AlterTypeOperation::RenameValue(rename) => {
            reject_non_enum_builtin(&tname)?;
            catalog.rename_enum_value(&tname, &rename.from.value, &rename.to.value)?
        }
    };
    Ok(QueryResult::Command {
        tag: "ALTER TYPE".into(),
        notices: to_notices(notices),
    })
}

/// The `AS '<builtin>'` internal function name of a `LANGUAGE internal` function.
fn function_internal_name(create: &ast::CreateFunction) -> Result<String, PgError> {
    match &create.function_body {
        Some(ast::CreateFunctionBody::AsBeforeOptions { body, .. })
        | Some(ast::CreateFunctionBody::AsAfterOptions(body)) => string_literal(body),
        _ => Err(PgError::feature_not_supported(
            "CREATE FUNCTION LANGUAGE internal requires AS '<builtin>'",
        )),
    }
}

/// The normalized `SELECT <expr>` body text of a `LANGUAGE SQL` function. A
/// `RETURN expr` / `AS RETURN expr` form becomes `SELECT expr`; an `AS '<body>'`
/// string is taken verbatim (it is already a `SELECT`); an `AS RETURN (select)`
/// is rendered from its query. Multi-statement `BEGIN ATOMIC` bodies are not
/// supported yet. The binder re-parses this text to validate and inline the body.
fn sql_function_body_text(create: &ast::CreateFunction) -> Result<String, PgError> {
    match &create.function_body {
        Some(ast::CreateFunctionBody::Return(expr))
        | Some(ast::CreateFunctionBody::AsReturnExpr(expr)) => Ok(format!("SELECT {expr}")),
        Some(ast::CreateFunctionBody::AsReturnSelect(select)) => Ok(select.to_string()),
        Some(ast::CreateFunctionBody::AsBeforeOptions { body, .. })
        | Some(ast::CreateFunctionBody::AsAfterOptions(body)) => string_literal(body),
        _ => Err(PgError::feature_not_supported(
            "CREATE FUNCTION LANGUAGE SQL requires a RETURN expression or AS '<body>'",
        )),
    }
}

/// The body of a `LANGUAGE plpgsql` routine, verbatim. Unlike a SQL body it is
/// never normalized: a `CONTEXT:` line reports a line number relative to the
/// text as written, and PostgreSQL counts the remainder of the `$$` line as
/// line 1 — so not even leading whitespace may be trimmed.
fn routine_body_text(create: &ast::CreateFunction) -> Result<String, PgError> {
    match &create.function_body {
        Some(ast::CreateFunctionBody::AsBeforeOptions { body, .. })
        | Some(ast::CreateFunctionBody::AsAfterOptions(body)) => string_literal(body),
        _ => Err(PgError::feature_not_supported(
            "CREATE FUNCTION LANGUAGE plpgsql requires AS '<body>'",
        )),
    }
}

/// The query-time [`PgType`] a resolved [`TypeRef`] denotes, for handing a SQL
/// function's declared argument/return types to the binder. A `cstring` or shell
/// user type — neither usable in a SQL function's signature — is refused.
fn pg_type_of_ref(
    type_catalog: &Arc<dyn TypeCatalog>,
    r: &TypeRef,
) -> Result<crabgresql_types::PgType, PgError> {
    match r {
        TypeRef::Builtin(t) => Ok(*t),
        TypeRef::User(name) => type_catalog
            .resolve_type(name)
            .map(|u| crabgresql_types::PgType::User(u.oid))
            .ok_or_else(|| {
                PgError::feature_not_supported(format!(
                    "SQL functions over type \"{name}\" are not supported yet"
                ))
            }),
        TypeRef::Cstring => Err(PgError::feature_not_supported(
            "SQL functions with a cstring argument or return type are not supported",
        )),
    }
}

/// Extract a string literal expression, in any of PG's quoting styles.
fn string_literal(expr: &ast::Expr) -> Result<String, PgError> {
    match expr {
        ast::Expr::Value(v) => match v.value.as_pg_string() {
            Some(s) => Ok(s.to_string()),
            None => Err(PgError::syntax(format!(
                "expected a string literal, found: {}",
                v.value
            ))),
        },
        other => Err(PgError::syntax(format!(
            "expected a string literal, found: {other}"
        ))),
    }
}

/// `CREATE FUNCTION`: `LANGUAGE internal AS '<builtin>'` (the type-bootstrap I/O
/// functions) and `LANGUAGE SQL` scalar functions, whose body is validated and
/// stored for inline expansion at call time.
fn execute_create_function(
    catalog: &GlobalCatalog,
    type_catalog: &Arc<dyn TypeCatalog>,
    create: &ast::CreateFunction,
) -> Result<QueryResult, PgError> {
    let lang = create
        .language
        .as_ref()
        .map(|i| i.value.to_ascii_lowercase());
    let name = single_object_name(&create.name, "function")?;
    let ret = match &create.return_type {
        Some(ast::FunctionReturnType::DataType(dt)) => resolve_type_ref(catalog, dt)?,
        Some(ast::FunctionReturnType::SetOf(_)) => {
            return Err(PgError::feature_not_supported(
                "SETOF return types are not supported yet",
            ));
        }
        None => {
            return Err(PgError::feature_not_supported(
                "CREATE FUNCTION without RETURNS is not supported yet",
            ));
        }
    };
    let args = routine_args(catalog, create.args.as_deref().unwrap_or(&[]))?;
    // A SQL/PL-pgSQL body refers to its arguments by these names, so a name may
    // not be reused; PG rejects the declaration rather than the body.
    let mut seen = std::collections::HashSet::new();
    for arg in &args {
        if let Some(argname) = &arg.name
            && !seen.insert(argname)
        {
            return Err(PgError::new(
                // 42P13 invalid_function_definition
                "42P13",
                format!("parameter name \"{argname}\" used more than once"),
            ));
        }
    }
    // Input arguments only, matching the identity the catalog registers, so the
    // names stay positionally aligned with the argument types.
    let arg_names: Vec<Option<String>> = args
        .iter()
        .filter(|a| a.mode.is_input())
        .map(|a| a.name.clone())
        .collect();

    let body = match lang.as_deref() {
        Some("internal") => FuncBody::Internal(function_internal_name(create)?),
        Some("sql") => {
            let body_sql = sql_function_body_text(create)?;
            // PG binds a `LANGUAGE SQL` body at CREATE time, reporting parse/type
            // errors then rather than on first call. Binding here also fixes the
            // register-after-validate ordering that makes a self-referential body
            // fail with `undefined_function` instead of recursing forever.
            let arg_types = args
                .iter()
                .filter(|a| a.mode.is_input())
                .map(|a| pg_type_of_ref(type_catalog, &a.ty))
                .collect::<Result<Vec<_>, _>>()?;
            let return_type = pg_type_of_ref(type_catalog, &ret)?;
            crabgresql_binder::bind_sql_function_body(
                type_catalog,
                &name,
                &arg_types,
                &arg_names,
                return_type,
                &body_sql,
            )
            .map_err(PgError::from)?;
            FuncBody::Sql(body_sql)
        }
        Some("plpgsql") => {
            let body_sql = routine_body_text(create)?;
            // Only the syntax is checked, never the SQL inside the body. That is
            // what PostgreSQL's PL/pgSQL validator does, and it is not a
            // shortcut: `create_function` registers the routine *after*
            // validating, so binding the body here would make every recursive
            // routine fail to create with 42883.
            crabgresql_plpgsql::compile(&body_sql, &arg_names)
                .map_err(|e| compile_failure(&name, e))?;
            FuncBody::PlPgSql(body_sql)
        }
        _ => {
            return Err(PgError::feature_not_supported(
                "CREATE FUNCTION is only supported for LANGUAGE internal, LANGUAGE SQL and \
                 LANGUAGE plpgsql",
            ));
        }
    };
    let notices = catalog.create_function(RoutineDefinition {
        name,
        kind: RoutineKind::Function,
        args,
        ret,
        body,
        volatility: routine_volatility(create.behavior.as_ref()),
        strict: routine_strict(create.called_on_null.as_ref()),
        secdef: matches!(create.security, Some(ast::FunctionSecurity::Definer)),
    })?;
    Ok(QueryResult::Command {
        tag: "CREATE FUNCTION".into(),
        notices: to_notices(notices),
    })
}

/// Resolve a parsed routine's parameter list. `OUT` parameters are kept — they
/// are excluded from the routine's *identity* by the catalog, not here, because
/// `pg_proc.proallargtypes`/`proargmodes` still need to report them.
fn routine_args(
    catalog: &GlobalCatalog,
    args: &[ast::OperateFunctionArg],
) -> Result<Vec<RoutineArg>, PgError> {
    args.iter()
        .map(|arg| {
            let mode = match arg.mode {
                None | Some(ast::ArgMode::In) => ArgMode::In,
                Some(ast::ArgMode::Out) => ArgMode::Out,
                Some(ast::ArgMode::InOut) => ArgMode::InOut,
                Some(ast::ArgMode::Variadic) => {
                    return Err(PgError::feature_not_supported(
                        "VARIADIC parameters are not supported yet",
                    ));
                }
            };
            if arg.default_expr.is_some() {
                return Err(PgError::feature_not_supported(
                    "parameter defaults are not supported yet",
                ));
            }
            // An empty span (line 0) means the arg was built without source
            // location; only parsed, bare arguments carry a caret position.
            let start = arg.data_type_span.start;
            Ok(RoutineArg {
                ty: resolve_type_ref(catalog, &arg.data_type)?,
                mode,
                name: arg.name.as_ref().map(normalize_ident),
                position: (start.line != 0).then_some((start.line, start.column)),
            })
        })
        .collect()
}

/// `IMMUTABLE | STABLE | VOLATILE`, defaulting to PG's `VOLATILE`.
fn routine_volatility(behavior: Option<&ast::FunctionBehavior>) -> Volatility {
    match behavior {
        Some(ast::FunctionBehavior::Immutable) => Volatility::Immutable,
        Some(ast::FunctionBehavior::Stable) => Volatility::Stable,
        Some(ast::FunctionBehavior::Volatile) | None => Volatility::Volatile,
    }
}

/// `STRICT` and `RETURNS NULL ON NULL INPUT` are spellings of the same thing;
/// `CALLED ON NULL INPUT` is PG's default.
fn routine_strict(called_on_null: Option<&ast::FunctionCalledOnNull>) -> bool {
    matches!(
        called_on_null,
        Some(ast::FunctionCalledOnNull::Strict | ast::FunctionCalledOnNull::ReturnsNullOnNullInput)
    )
}

/// `CREATE PROCEDURE name(args) LANGUAGE plpgsql AS $$ ... $$`.
///
/// PostgreSQL shares one attribute-clause production between CREATE FUNCTION
/// and CREATE PROCEDURE and rejects the function-only ones here rather than in
/// the grammar, so the parser accepts them and this is where they are refused.
fn execute_create_procedure(
    catalog: &GlobalCatalog,
    create: &ast::CreateProcedure,
) -> Result<QueryResult, PgError> {
    for (attribute, present) in [
        ("volatility", create.behavior.is_some()),
        ("strictness", create.called_on_null.is_some()),
        ("parallel", create.parallel.is_some()),
    ] {
        if present {
            return Err(PgError::new(
                sqlstate::INVALID_OBJECT_DEFINITION,
                format!("invalid attribute in procedure definition: {attribute}"),
            ));
        }
    }
    let name = single_object_name(&create.name, "procedure")?;
    let args = routine_args(catalog, create.args.as_deref().unwrap_or(&[]))?;
    let lang = create
        .language
        .as_ref()
        .map(|i| i.value.to_ascii_lowercase());
    let Some("plpgsql") = lang.as_deref() else {
        return Err(PgError::feature_not_supported(
            "CREATE PROCEDURE is only supported for LANGUAGE plpgsql",
        ));
    };
    let body_sql = procedure_body_text(create)?;
    let arg_names: Vec<Option<String>> = args
        .iter()
        .filter(|a| a.mode.is_input())
        .map(|a| a.name.clone())
        .collect();
    crabgresql_plpgsql::compile(&body_sql, &arg_names).map_err(|e| compile_failure(&name, e))?;

    let notices = catalog.create_function(RoutineDefinition {
        name,
        kind: RoutineKind::Procedure,
        args,
        // A procedure declares no return type. There is no `void` here yet, so
        // a placeholder is stored; nothing reads it, because `RoutineKind`
        // already tells the interpreter a procedure returns nothing.
        ret: TypeRef::Builtin(PgType::Text),
        body: FuncBody::PlPgSql(body_sql),
        volatility: Volatility::Volatile,
        strict: false,
        secdef: matches!(create.security, Some(ast::FunctionSecurity::Definer)),
    })?;
    Ok(QueryResult::Command {
        tag: "CREATE PROCEDURE".into(),
        notices: to_notices(notices),
    })
}

/// A routine body that would not compile, as PostgreSQL's PL/pgSQL validator
/// reports it. PostgreSQL adds a `LINE n:` excerpt with a caret into the body;
/// reproducing that needs a mapping from a body offset back into the statement
/// text, which the parser does not carry, so the CONTEXT line names the line
/// instead.
fn compile_failure(name: &str, e: crabgresql_plpgsql::CompileError) -> PgError {
    let line = e.line;
    let mut err = PgError::new(e.code, e.message);
    err.hint = e.hint;
    err.context.push(format!(
        "compilation of PL/pgSQL function \"{name}\" near line {line}"
    ));
    err
}

/// A procedure's body, verbatim — see [`routine_body_text`] for why it is not
/// normalized.
fn procedure_body_text(create: &ast::CreateProcedure) -> Result<String, PgError> {
    match &create.function_body {
        Some(ast::CreateFunctionBody::AsBeforeOptions { body, .. })
        | Some(ast::CreateFunctionBody::AsAfterOptions(body)) => string_literal(body),
        _ => Err(PgError::feature_not_supported(
            "CREATE PROCEDURE requires AS '<body>'",
        )),
    }
}

/// `DO [LANGUAGE lang] $$ ... $$` — an anonymous block, run as if it were the
/// body of a void-returning procedure.
///
/// Unlike a routine call this has no catalog entry, so the body is compiled
/// fresh every time; a `DO` block is written to run once.
fn execute_do(
    engine: &Arc<dyn TableEngine>,
    global_catalog: &Arc<GlobalCatalog>,
    txnmgr: &Arc<TransactionManager>,
    block: &ast::DoStatement,
    session: &mut Session,
) -> Result<QueryResult, PgError> {
    let lang = block
        .language
        .as_ref()
        .map(|i| i.value.to_ascii_lowercase())
        .unwrap_or_else(|| "plpgsql".to_string());
    match lang.as_str() {
        "plpgsql" => {}
        "sql" | "internal" | "c" => {
            return Err(PgError::feature_not_supported(format!(
                "language \"{lang}\" does not support inline code execution"
            )));
        }
        other => {
            return Err(PgError::new(
                sqlstate::UNDEFINED_OBJECT,
                format!("language \"{other}\" does not exist"),
            ));
        }
    }
    let body = string_literal(&ast::Expr::Value(block.body.clone()))?;

    let (catalog, type_catalog, catalog_ops) = bind_catalogs(engine, global_catalog, session);
    let (routines, command_counter) =
        statement_runtime(&catalog, &type_catalog, global_catalog, session);
    // A DO block is opaque: nothing here can tell whether it writes, so it gets
    // an XID like any other write. Its own DML does the read-only check.
    let read_only = read_only_active(session);
    let txn = build_txn(txnmgr, session, true);
    let exec_ctx = session.exec_context_for_statement(
        engine,
        &catalog_ops,
        &type_catalog,
        Arc::clone(&routines),
        Arc::clone(&command_counter),
        read_only,
    );
    let outcome = routines.run_inline_block(&body, &exec_ctx, &txn);
    if let Err(e) = outcome {
        let _ = finalize_statement(txnmgr, session, &txn, true, false, Some(&command_counter));
        return Err(e.into());
    }
    finalize_statement(txnmgr, session, &txn, true, true, Some(&command_counter))?;
    Ok(QueryResult::Command {
        tag: "DO".into(),
        notices: session.notices.drain(),
    })
}

/// `CALL proc(args)`.
///
/// A `CALL` argument is a constant expression — there is no row for a column
/// reference to come from — so the arguments are bound against an empty scope
/// and evaluated before the body is entered.
fn execute_call(
    engine: &Arc<dyn TableEngine>,
    global_catalog: &Arc<GlobalCatalog>,
    txnmgr: &Arc<TransactionManager>,
    call: &ast::Function,
    session: &mut Session,
) -> Result<QueryResult, PgError> {
    let name = single_object_name(&call.name, "procedure")?;
    let (catalog, type_catalog, catalog_ops) = bind_catalogs(engine, global_catalog, session);

    let ast::FunctionArguments::List(list) = &call.args else {
        return Err(PgError::feature_not_supported(
            "CALL with no argument list is not supported yet",
        ));
    };
    let params = param_ctx_none();
    let scope = crabgresql_binder::Scope::empty(&type_catalog, &params);
    let mut args = Vec::with_capacity(list.args.len());
    for arg in &list.args {
        let ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(expr)) = arg else {
            return Err(PgError::feature_not_supported(
                "named and wildcard CALL arguments are not supported yet",
            ));
        };
        args.push(crabgresql_binder::bind_scalar(expr, &scope)?);
    }

    let sig = resolve_procedure(&type_catalog, &name, &args)?;

    let (routines, command_counter) =
        statement_runtime(&catalog, &type_catalog, global_catalog, session);
    let read_only = read_only_active(session);
    let txn = build_txn(txnmgr, session, true);
    let exec_ctx = ExecContext {
        // An argument can itself call a routine (`CALL p(f(1))`), so it is
        // evaluated under the same runtime the body gets — including the
        // transaction, which nothing else attaches here because these
        // expressions never go through `execute`.
        txn: Some(txn.clone()),
        ..session.exec_context_for_statement(
            engine,
            &catalog_ops,
            &type_catalog,
            Arc::clone(&routines),
            Arc::clone(&command_counter),
            read_only,
        )
    };
    // Coerce each argument to its declared type, as a function call's would be.
    let mut values = Vec::with_capacity(args.len());
    for (arg, ty) in args.iter().zip(sig.arg_types.iter()) {
        let coerced = crabgresql_executor::eval_row_free(arg, &exec_ctx)
            .and_then(|value| crabgresql_executor::coerce_value(value, *ty, &exec_ctx));
        match coerced {
            Ok(value) => values.push(value),
            Err(e) => {
                // The transaction is open by now, so a bad argument has to abort
                // it rather than leaving the XID in flight.
                let _ =
                    finalize_statement(txnmgr, session, &txn, true, false, Some(&command_counter));
                return Err(e.into());
            }
        }
    }

    if let Err(e) = routines.call(sig.oid, values, &exec_ctx, &txn) {
        let _ = finalize_statement(txnmgr, session, &txn, true, false, Some(&command_counter));
        return Err(e.into());
    }
    finalize_statement(txnmgr, session, &txn, true, true, Some(&command_counter))?;
    Ok(QueryResult::Command {
        tag: "CALL".into(),
        notices: session.notices.drain(),
    })
}

/// Resolve `CALL name(args)` to a procedure. A name that resolves only to a
/// *function* is `42809` with the SELECT hint, mirroring how calling a
/// procedure as a function is refused in the other direction.
fn resolve_procedure(
    type_catalog: &Arc<dyn TypeCatalog>,
    name: &str,
    args: &[crabgresql_binder::BoundExpr],
) -> Result<RoutineSig, PgError> {
    let sigs = type_catalog.routines(name);
    let arg_types: Vec<PgType> = args.iter().map(|a| a.ty()).collect();
    let matches_arity = |sig: &RoutineSig| sig.arg_types.len() == args.len();

    if let Some(sig) = sigs
        .iter()
        .find(|s| s.kind == ApiRoutineKind::Procedure && s.arg_types == arg_types)
        .or_else(|| {
            sigs.iter()
                .find(|s| s.kind == ApiRoutineKind::Procedure && matches_arity(s))
        })
    {
        return Ok(sig.clone());
    }
    let rendered: Vec<&str> = arg_types.iter().map(|t| t.name()).collect();
    let signature = format!("{name}({})", rendered.join(", "));
    if sigs.iter().any(matches_arity) {
        return Err(PgError::new(
            sqlstate::WRONG_OBJECT_TYPE,
            format!("{signature} is not a procedure"),
        )
        .with_hint("To call a function, use SELECT."));
    }
    Err(PgError::new(
        sqlstate::UNDEFINED_FUNCTION,
        format!("procedure {signature} does not exist"),
    ))
}

/// `CREATE CAST (source AS target) ...`.
fn execute_create_cast(
    catalog: &GlobalCatalog,
    source: &ast::DataType,
    target: &ast::DataType,
    method: &ast::CastMethod,
) -> Result<QueryResult, PgError> {
    let source_ref = resolve_type_ref(catalog, source)?;
    let target_ref = resolve_type_ref(catalog, target)?;
    let without_function = matches!(method, ast::CastMethod::WithoutFunction);
    let notices = catalog.create_cast(source_ref, target_ref, without_function)?;
    Ok(QueryResult::Command {
        tag: "CREATE CAST".into(),
        notices: to_notices(notices),
    })
}

/// `DROP TABLE name [, ...] [IF EXISTS] [CASCADE|RESTRICT]`. Resolves against the
/// temp-first overlay so a session temp table is dropped ahead of a same-named
/// permanent one. All targets are validated before any is dropped, so a multi-name
/// DROP is atomic like PG. `CASCADE`/`RESTRICT` are accepted and ignored: no object
/// depends on a table in this engine, so both simply drop the named tables.
/// `CREATE [OR REPLACE] VIEW name [(cols)] AS <query>`. Binds the defining query
/// to validate it and derive the output columns, stores the view (as its SELECT
/// text plus derived columns and surface dependencies), and handles name
/// collisions, `IF NOT EXISTS`, and `OR REPLACE`.
fn execute_create_view(
    catalog: &Arc<dyn TableEngine>,
    type_catalog: &Arc<dyn TypeCatalog>,
    create: &ast::CreateView,
) -> Result<QueryResult, PgError> {
    let (schema_qual, name) = split_object_name(&create.name, "relation")?;
    let namespace = resolve_create_namespace(catalog, schema_qual.as_deref(), &name)?;
    // Reject forms we do not implement rather than silently ignoring them.
    if create.materialized {
        return Err(PgError::feature_not_supported(
            "materialized views are not supported yet",
        ));
    }
    if create.temporary {
        return Err(PgError::feature_not_supported(
            "temporary views are not supported yet",
        ));
    }
    if create.or_alter
        || create.to.is_some()
        || create.with_no_schema_binding
        || !create.cluster_by.is_empty()
        || !matches!(create.options, ast::CreateTableOptions::None)
    {
        return Err(PgError::feature_not_supported(
            "this CREATE VIEW form is not supported yet",
        ));
    }

    // Bind the defining query: validates it and yields the output column shape.
    let plan = crabgresql_binder::bind_query(catalog, type_catalog, &create.query)?;
    let mut columns = output_columns_of(&plan)?;

    // An explicit column list renames the outputs; its length must match exactly.
    if !create.columns.is_empty() {
        for col in &create.columns {
            if col.data_type.is_some() || col.options.is_some() {
                return Err(PgError::feature_not_supported(
                    "column options in CREATE VIEW are not supported yet",
                ));
            }
        }
        // PG rejects only *too many* names; a shorter list renames the leading
        // columns and leaves the trailing ones with their query-derived names.
        if create.columns.len() > columns.len() {
            return Err(PgError::new(
                sqlstate::SYNTAX_ERROR,
                "CREATE VIEW specifies more column names than columns",
            ));
        }
        for (out, def) in columns.iter_mut().zip(&create.columns) {
            out.name = normalize_ident(&def.name);
        }
    }

    // Duplicate output column names are rejected, as for a table.
    let mut seen = HashSet::new();
    for col in &columns {
        if !seen.insert(col.name.clone()) {
            return Err(PgError::new(
                sqlstate::DUPLICATE_COLUMN,
                format!("column \"{}\" specified more than once", col.name),
            ));
        }
    }

    // A view's columns describe themselves exactly as the query's output does,
    // modifier and collation included — that is what makes `\d v` print
    // `character varying(20)` rather than a bare `character varying`.
    let view_columns: Vec<Column> = columns
        .iter()
        .map(|c| {
            let mut col = Column::with_typmod(c.name.clone(), c.ty, c.typmod);
            col.collation = c.collation;
            col
        })
        .collect();
    let depends_on = referenced_relations(&create.query);
    let sql = create.query.to_string();

    let existing_table = catalog.resolve(Some(&namespace), &name).is_ok();
    if create.or_replace {
        if existing_table {
            return Err(PgError::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not a view"),
            ));
        }
        if let Some(old) = catalog.resolve_view(Some(&namespace), &name) {
            check_view_replace_compatible(&old, &view_columns)?;
            // A replaced view may (transitively) reference itself; PG permits
            // creating such a view and only errors when it is used, so the
            // binder detects the cycle at expansion time rather than here.
            catalog.drop_view(&namespace, &name)?;
        }
    } else if existing_table || catalog.resolve_view(Some(&namespace), &name).is_some() {
        if create.if_not_exists {
            return Ok(QueryResult::Command {
                tag: "CREATE VIEW".into(),
                notices: vec![Notice::notice(
                    format!("relation \"{name}\" already exists, skipping"),
                    None,
                )],
            });
        }
        return Err(PgError::new(
            sqlstate::DUPLICATE_TABLE,
            format!("relation \"{name}\" already exists"),
        ));
    }

    catalog.create_view(ViewDefinition {
        name,
        namespace,
        sql,
        columns: view_columns,
        depends_on,
    })?;
    Ok(QueryResult::command("CREATE VIEW"))
}

/// Enforce PG's `CREATE OR REPLACE VIEW` rule: the new column list may only add
/// trailing columns; existing columns must keep their name and type.
fn check_view_replace_compatible(old: &ViewDefinition, new: &[Column]) -> Result<(), PgError> {
    if new.len() < old.columns.len() {
        return Err(PgError::new(
            sqlstate::INVALID_TABLE_DEFINITION,
            "cannot drop columns from view",
        ));
    }
    for (o, n) in old.columns.iter().zip(new) {
        if o.name != n.name {
            return Err(PgError::new(
                sqlstate::INVALID_TABLE_DEFINITION,
                format!(
                    "cannot change name of view column \"{}\" to \"{}\"",
                    o.name, n.name
                ),
            ));
        }
        if o.ty != n.ty {
            return Err(PgError::new(
                sqlstate::INVALID_TABLE_DEFINITION,
                format!(
                    "cannot change data type of view column \"{}\" from {} to {}",
                    o.name,
                    o.ty.name(),
                    n.ty.name()
                ),
            ));
        }
    }
    Ok(())
}

/// The surface relation names a query references in FROM position (including
/// joins, derived tables, nested joins, set operations, and CTE bodies), minus
/// the names bound by the query's own `WITH` clauses. A view over another view
/// records the *view* name — the dependency edge `DROP ... CASCADE` walks. NB:
/// subqueries embedded in expressions (e.g. a scalar subquery in the SELECT
/// list) are not traced yet.
fn referenced_relations(query: &ast::Query) -> Vec<String> {
    let mut names = Vec::new();
    // The CTE names currently in scope, as a stack: a query's `WITH` names shadow
    // like-named base tables only within that query (and its nested scopes), so
    // they are pushed on entry and popped on exit rather than filtered globally.
    let mut scope: Vec<String> = Vec::new();
    collect_query_relations(query, &mut names, &mut scope);
    let mut seen = HashSet::new();
    names
        .into_iter()
        .filter(|n| seen.insert(n.clone()))
        .collect()
}

fn collect_query_relations(query: &ast::Query, names: &mut Vec<String>, scope: &mut Vec<String>) {
    let pushed = query.with.as_ref().map_or(0, |with| {
        for cte in &with.cte_tables {
            scope.push(normalize_ident(&cte.alias.name));
        }
        with.cte_tables.len()
    });
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            collect_query_relations(&cte.query, names, scope);
        }
    }
    collect_setexpr_relations(&query.body, names, scope);
    scope.truncate(scope.len() - pushed);
}

fn collect_setexpr_relations(
    body: &ast::SetExpr,
    names: &mut Vec<String>,
    scope: &mut Vec<String>,
) {
    match body {
        ast::SetExpr::Select(select) => {
            for twj in &select.from {
                collect_factor_relations(&twj.relation, names, scope);
                for join in &twj.joins {
                    collect_factor_relations(&join.relation, names, scope);
                }
            }
        }
        ast::SetExpr::Query(query) => collect_query_relations(query, names, scope),
        ast::SetExpr::SetOperation { left, right, .. } => {
            collect_setexpr_relations(left, names, scope);
            collect_setexpr_relations(right, names, scope);
        }
        _ => {}
    }
}

fn collect_factor_relations(
    factor: &ast::TableFactor,
    names: &mut Vec<String>,
    scope: &mut Vec<String>,
) {
    match factor {
        // A plain relation reference (a table function carries `args`); skip it
        // when an in-scope CTE of the same name shadows the base relation. The
        // reference is recorded as a qualified `"namespace.name"` key (unqualified
        // → `public`) so a view's dependency set can distinguish same-named
        // relations in different schemas.
        ast::TableFactor::Table {
            name, args: None, ..
        } => {
            let parts = &name.0;
            if let Some(rel) = parts
                .last()
                .and_then(|part| part.as_ident())
                .map(normalize_ident)
            {
                // A CTE name is always unqualified, so it shadows only a bare
                // reference of the same name.
                let schema = (parts.len() >= 2)
                    .then(|| parts[parts.len() - 2].as_ident().map(normalize_ident))
                    .flatten();
                if schema.is_none() && scope.contains(&rel) {
                    return;
                }
                let namespace = schema.unwrap_or_else(|| "public".to_string());
                names.push(format!("{namespace}.{rel}"));
            }
        }
        ast::TableFactor::Derived { subquery, .. } => {
            collect_query_relations(subquery, names, scope)
        }
        ast::TableFactor::NestedJoin {
            table_with_joins, ..
        } => {
            collect_factor_relations(&table_with_joins.relation, names, scope);
            for join in &table_with_joins.joins {
                collect_factor_relations(&join.relation, names, scope);
            }
        }
        _ => {}
    }
}

fn execute_drop_table(
    catalog: &Arc<dyn TableEngine>,
    names: &[ast::ObjectName],
    cascade: bool,
    if_exists: bool,
) -> Result<QueryResult, PgError> {
    // Each target splits into `(namespace, bare)`. `None` means "unqualified"
    // (or `public.`-qualified) — the temp-first, view-cascade path below, exactly
    // as before schemas. `Some(ns)` is a user schema, dropped directly in that
    // namespace (its display form keeps the qualifier for error text).
    let mut targets: Vec<(Option<String>, String, String)> = Vec::new();
    for name in names {
        let (schema, bare) = split_object_name(name, "table")?;
        let display = match &schema {
            Some(ns) => format!("{ns}.{bare}"),
            None => bare.clone(),
        };
        let namespace = match schema.as_deref() {
            None | Some("public") => None,
            Some(ns) => Some(ns.to_string()),
        };
        targets.push((namespace, bare, display));
    }
    // A target named twice is rejected up front, before anything is dropped, as
    // in PG — otherwise the second pass would re-drop (and, if a temp table
    // shadows a permanent one, silently drop the permanent table too).
    // Dedup by the resolved (namespace, bare) key, not the written display form,
    // so `DROP TABLE public.t, t` (both resolve to public.t) is caught as PG's
    // 42710 rather than double-dropping.
    for (i, (ns, bare, display)) in targets.iter().enumerate() {
        if targets[..i]
            .iter()
            .any(|(ns2, bare2, _)| ns2 == ns && bare2 == bare)
        {
            return Err(PgError::new(
                sqlstate::DUPLICATE_OBJECT,
                format!("table \"{display}\" specified more than once"),
            ));
        }
    }
    // Phase 1: validate. A missing target without IF EXISTS aborts the whole
    // statement before anything is dropped; with IF EXISTS it becomes a skip
    // NOTICE. PG spells the missing-object noun "table" here, not "relation".
    let mut notices = Vec::new();
    // Unqualified/public targets go through the temp-first + view-cascade path;
    // user-schema targets are dropped directly.
    let mut plain: Vec<String> = Vec::new();
    let mut qualified: Vec<(String, String)> = Vec::new();
    for (namespace, bare, display) in &targets {
        let (exists, is_view) = match namespace {
            None => (
                catalog.open_table(bare).is_ok(),
                catalog.resolve_view(None, bare).is_some(),
            ),
            Some(ns) => (
                catalog.resolve(Some(ns), bare).is_ok(),
                catalog.resolve_view(Some(ns), bare).is_some(),
            ),
        };
        if exists {
            match namespace {
                None => plain.push(bare.clone()),
                Some(ns) => qualified.push((ns.clone(), bare.clone())),
            }
        } else if is_view {
            // A view shares the relation namespace: PG rejects DROP TABLE on one
            // as a wrong-object-type error rather than "does not exist".
            return Err(PgError::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("\"{display}\" is not a table"),
            )
            .with_hint("Use DROP VIEW to remove a view."));
        } else if if_exists {
            notices.push(Notice::notice(
                format!("table \"{display}\" does not exist, skipping"),
                None,
            ));
        } else {
            return Err(PgError::new(
                sqlstate::UNDEFINED_TABLE,
                format!("table \"{display}\" does not exist"),
            ));
        }
    }
    // A partitioned parent's partitions are dependent objects that PG drops
    // together with the parent (no CASCADE needed). Expand the target set with
    // every partition of a parent being dropped that was not itself named, so the
    // parent is never left with orphaned children (dangling `relispartition`).
    let mut targeted: std::collections::HashSet<(String, String)> = plain
        .iter()
        .map(|n| ("public".to_string(), n.clone()))
        .chain(qualified.iter().cloned())
        .collect();
    let parents = targeted.clone();
    for meta in catalog.relation_metadata() {
        let Some(part) = &meta.schema.partition_of else {
            continue;
        };
        if !parents.contains(&(part.parent_namespace.clone(), part.parent_name.clone())) {
            continue;
        }
        let child = (meta.schema.namespace.clone(), meta.schema.name.clone());
        if targeted.insert(child.clone()) {
            qualified.push(child);
        }
    }
    // Phase 2: resolve dependent views across every target namespace (RESTRICT
    // errors, CASCADE drops them), then drop the tables followed by their
    // cascaded views.
    let mut all_targets: Vec<(String, String)> = plain
        .iter()
        .map(|n| ("public".to_string(), n.clone()))
        .collect();
    all_targets.extend(qualified.iter().cloned());
    let (dependents, mut cascade_notices) =
        plan_drop_cascade(catalog, "table", &all_targets, cascade)?;
    notices.append(&mut cascade_notices);
    for name in &plain {
        catalog.drop_table("public", name)?;
    }
    for (ns, name) in &qualified {
        catalog.drop_table(ns, name)?;
    }
    drop_cascaded(catalog, &dependents)?;
    // Auto-drop sequences a dropped table owns (a `serial` column's sequence, via
    // PG's OWNED BY). PG removes these silently, without a cascade notice.
    //
    // A table CASCADE pulled in counts as dropped here too: since inheritance
    // children became dependents, a cascade can remove a table that owns
    // sequences, and skipping those would leave a `t_id_seq` behind whose table
    // is gone — so recreating `t` would silently get `t_id_seq1`.
    let dropped_tables: Vec<(&str, &str)> = plain
        .iter()
        .map(|n| ("public", n.as_str()))
        .chain(qualified.iter().map(|(ns, n)| (ns.as_str(), n.as_str())))
        .chain(
            dependents
                .iter()
                .filter(|(kind, ..)| *kind == DependentKind::Table)
                .map(|(_, ns, n)| (ns.as_str(), n.as_str())),
        )
        .collect();
    for seq in catalog.sequences() {
        let owned_by_dropped = seq.owned_by.as_deref().is_some_and(|owner| {
            dropped_tables
                .iter()
                .any(|(ns, t)| *ns == seq.namespace && *t == owner)
        });
        if owned_by_dropped {
            let _ = catalog.drop_sequence(&seq.namespace, &seq.name);
        }
    }
    Ok(QueryResult::Command {
        tag: "DROP TABLE".into(),
        notices,
    })
}

/// `DROP VIEW name [, ...] [CASCADE|RESTRICT]`. Mirrors [`execute_drop_table`]:
/// two-phase validate/drop, IF EXISTS skip-notices, wrong-object-type detection
/// (a table named here is `"x" is not a view`), and dependent-view cascade.
fn execute_drop_view(
    catalog: &Arc<dyn TableEngine>,
    names: &[ast::ObjectName],
    cascade: bool,
    if_exists: bool,
) -> Result<QueryResult, PgError> {
    let vnames = names
        .iter()
        .map(object_name_to_table_name)
        .collect::<Result<Vec<_>, _>>()?;
    for (i, name) in vnames.iter().enumerate() {
        if vnames[..i].contains(name) {
            return Err(PgError::new(
                sqlstate::DUPLICATE_OBJECT,
                format!("view \"{name}\" specified more than once"),
            ));
        }
    }
    let mut notices = Vec::new();
    let mut to_drop: Vec<String> = Vec::new();
    for name in &vnames {
        if catalog.resolve_view(None, name).is_some() {
            to_drop.push(name.clone());
        } else if catalog.open_table(name).is_ok() {
            return Err(PgError::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not a view"),
            )
            .with_hint("Use DROP TABLE to remove a table."));
        } else if if_exists {
            notices.push(Notice::notice(
                format!("view \"{name}\" does not exist, skipping"),
                None,
            ));
        } else {
            return Err(PgError::new(
                sqlstate::UNDEFINED_TABLE,
                format!("view \"{name}\" does not exist"),
            ));
        }
    }
    let targets: Vec<(String, String)> = to_drop
        .iter()
        .map(|n| ("public".to_string(), n.clone()))
        .collect();
    let (dependents, mut cascade_notices) = plan_drop_cascade(catalog, "view", &targets, cascade)?;
    notices.append(&mut cascade_notices);
    for name in &to_drop {
        catalog.drop_view("public", name)?;
    }
    drop_cascaded(catalog, &dependents)?;
    Ok(QueryResult::Command {
        tag: "DROP VIEW".into(),
        notices,
    })
}

/// The base integer type a `serial` pseudotype expands to, or `None` if the type
/// is not a serial family name. `serial`/`serial4` → int4, `bigserial`/`serial8`
/// → int8, `smallserial`/`serial2` → int2.
fn serial_base_type(dt: &ast::DataType) -> Option<PgType> {
    let ast::DataType::Custom(obj, mods) = dt else {
        return None;
    };
    if !mods.is_empty() {
        return None;
    }
    match obj
        .0
        .last()
        .and_then(|p| p.as_ident())
        .map(normalize_ident)
        .as_deref()
    {
        Some("serial" | "serial4") => Some(PgType::Int4),
        Some("bigserial" | "serial8") => Some(PgType::Int8),
        Some("smallserial" | "serial2") => Some(PgType::Int2),
        _ => None,
    }
}

/// The upper bound of a serial sequence: the backing integer type's maximum.
fn serial_type_max(base: PgType) -> i64 {
    match base {
        PgType::Int2 => i16::MAX as i64,
        PgType::Int4 => i32::MAX as i64,
        _ => i64::MAX,
    }
}

/// The inclusive `(min, max)` a sequence data type can hold.
fn seq_type_bounds(ty: PgType) -> (i64, i64) {
    match ty {
        PgType::Int2 => (i16::MIN as i64, i16::MAX as i64),
        PgType::Int4 => (i32::MIN as i64, i32::MAX as i64),
        _ => (i64::MIN, i64::MAX),
    }
}

/// PostgreSQL's spelling of a sequence data type, for out-of-range diagnostics.
fn seq_type_name(ty: PgType) -> &'static str {
    match ty {
        PgType::Int2 => "smallint",
        PgType::Int4 => "integer",
        _ => "bigint",
    }
}

/// Pick a relation name not already used by a table, view, index, or sequence
/// (nor by `extra`, names reserved earlier in the same statement), appending a
/// numeric suffix as PostgreSQL does for auto-named serial sequences.
fn unique_relation_name(
    engine: &Arc<dyn TableEngine>,
    namespace: &str,
    extra: &[String],
    base: &str,
) -> String {
    let taken = |n: &str| {
        extra.iter().any(|x| x == n)
            || engine.sequence(namespace, n).is_some()
            || engine.resolve(Some(namespace), n).is_ok()
            || engine.resolve_view(Some(namespace), n).is_some()
            || engine
                .relation_metadata()
                .iter()
                .filter(|r| r.schema.namespace == namespace)
                .any(|r| r.indexes.iter().any(|i| i.name == n))
    };
    if !taken(base) {
        return base.to_string();
    }
    for i in 1.. {
        let candidate = format!("{base}{i}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    unreachable!("an unbounded suffix search always finds a free name")
}

/// Best-effort removal of sequences created earlier in a `CREATE TABLE` that then
/// failed — DDL is not transactional, so this avoids leaking orphan sequences.
fn drop_created_sequences(engine: &Arc<dyn TableEngine>, defs: &[SequenceDefinition]) {
    for def in defs {
        let _ = engine.drop_sequence(&def.namespace, &def.name);
    }
}

/// Read an integer sequence option (`INCREMENT 2`, `START -1`, ...). Only integer
/// literals (optionally signed) are accepted.
fn parse_i64_expr(expr: &ast::Expr) -> Option<i64> {
    match expr {
        ast::Expr::Value(v) => match &v.value {
            // The literal keeps its written spelling, so `INCREMENT 0x2` needs
            // the same acceptor an expression would use; a quoted option value
            // is an ordinary string and stays on `str::parse`.
            ast::Value::Number(n, _) => {
                crabgresql_binder::literal_int(n).and_then(|v| i64::try_from(v).ok())
            }
            other => other.as_pg_string()?.trim().parse().ok(),
        },
        ast::Expr::UnaryOp {
            op: ast::UnaryOperator::Minus,
            expr,
        } => parse_i64_expr(expr).and_then(|v| v.checked_neg()),
        ast::Expr::UnaryOp {
            op: ast::UnaryOperator::Plus,
            expr,
        } => parse_i64_expr(expr),
        _ => None,
    }
}

fn eval_seq_int(expr: &ast::Expr, option: &str) -> Result<i64, PgError> {
    parse_i64_expr(expr).ok_or_else(|| {
        PgError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("{option} value must be an integer"),
        )
    })
}

/// Fold the parsed `CREATE SEQUENCE` options over PostgreSQL's defaults into a
/// [`SequenceDefinition`], validating the min/max/start/increment/cache relations.
fn build_sequence_definition(
    name: String,
    namespace: String,
    data_type: PgType,
    options: &[ast::SequenceOptions],
    owned_by: Option<String>,
) -> Result<SequenceDefinition, PgError> {
    let (type_min, type_max) = seq_type_bounds(data_type);
    let mut increment = 1i64;
    let mut min: Option<i64> = None;
    let mut max: Option<i64> = None;
    let mut start: Option<i64> = None;
    let mut cache = 1i64;
    let mut cycle = false;
    for opt in options {
        match opt {
            ast::SequenceOptions::IncrementBy(e, _) => increment = eval_seq_int(e, "INCREMENT")?,
            ast::SequenceOptions::MinValue(Some(e)) => min = Some(eval_seq_int(e, "MINVALUE")?),
            ast::SequenceOptions::MinValue(None) => min = None,
            ast::SequenceOptions::MaxValue(Some(e)) => max = Some(eval_seq_int(e, "MAXVALUE")?),
            ast::SequenceOptions::MaxValue(None) => max = None,
            ast::SequenceOptions::StartWith(e, _) => start = Some(eval_seq_int(e, "START")?),
            ast::SequenceOptions::Cache(e) => cache = eval_seq_int(e, "CACHE")?,
            // `Cycle(true)` renders as `NO CYCLE`, so cycling is its negation.
            ast::SequenceOptions::Cycle(no) => cycle = !no,
        }
    }
    if increment == 0 {
        return Err(PgError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "INCREMENT must not be zero",
        ));
    }
    let ascending = increment > 0;
    let min = min.unwrap_or(if ascending { 1 } else { type_min });
    let max = max.unwrap_or(if ascending { type_max } else { -1 });
    // An explicit MIN/MAX outside the backing integer type's range is rejected
    // (PG checks this before the MIN<MAX relation). MINVALUE is reported first.
    if min < type_min || min > type_max {
        return Err(PgError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!(
                "MINVALUE ({min}) is out of range for sequence data type {}",
                seq_type_name(data_type)
            ),
        ));
    }
    if max < type_min || max > type_max {
        return Err(PgError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!(
                "MAXVALUE ({max}) is out of range for sequence data type {}",
                seq_type_name(data_type)
            ),
        ));
    }
    if min >= max {
        return Err(PgError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("MINVALUE ({min}) must be less than MAXVALUE ({max})"),
        ));
    }
    let start = start.unwrap_or(if ascending { min } else { max });
    if start < min {
        return Err(PgError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("START value ({start}) cannot be less than MINVALUE ({min})"),
        ));
    }
    if start > max {
        return Err(PgError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("START value ({start}) cannot be greater than MAXVALUE ({max})"),
        ));
    }
    if cache < 1 {
        return Err(PgError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("CACHE ({cache}) must be greater than zero"),
        ));
    }
    Ok(SequenceDefinition {
        name,
        namespace,
        data_type,
        start,
        increment,
        min,
        max,
        cache,
        cycle,
        owned_by,
    })
}

/// `CREATE SEQUENCE [IF NOT EXISTS] name [AS type] [options]`. `OWNED BY` is
/// accepted and ignored (dependency tracking beyond serial's own sequences is a
/// v1 gap); temporary sequences are not supported.
#[allow(clippy::too_many_arguments)]
fn execute_create_sequence(
    catalog: &Arc<dyn TableEngine>,
    temporary: bool,
    if_not_exists: bool,
    name: &ast::ObjectName,
    data_type: &Option<ast::DataType>,
    options: &[ast::SequenceOptions],
    _owned_by: &Option<ast::ObjectName>,
) -> Result<QueryResult, PgError> {
    if temporary {
        return Err(PgError::feature_not_supported(
            "temporary sequences are not supported yet",
        ));
    }
    let (schema_qual, seq_name) = split_object_name(name, "relation")?;
    let namespace = resolve_create_namespace(catalog, schema_qual.as_deref(), &seq_name)?;
    let data_type = match data_type {
        None => PgType::Int8,
        Some(dt) => match crabgresql_binder::map_data_type(dt)? {
            ty @ (PgType::Int2 | PgType::Int4 | PgType::Int8) => ty,
            _ => {
                return Err(PgError::new(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "sequence type must be smallint, integer, or bigint",
                ));
            }
        },
    };
    let def = build_sequence_definition(seq_name.clone(), namespace, data_type, options, None)?;
    match catalog.create_sequence(def) {
        Ok(()) => Ok(QueryResult::command("CREATE SEQUENCE")),
        Err(StorageError::TableAlreadyExists(_)) if if_not_exists => Ok(QueryResult::Command {
            tag: "CREATE SEQUENCE".into(),
            notices: vec![Notice::notice(
                format!("relation \"{seq_name}\" already exists, skipping"),
                None,
            )],
        }),
        Err(StorageError::TableAlreadyExists(_)) => Err(PgError::new(
            sqlstate::DUPLICATE_TABLE,
            format!("relation \"{seq_name}\" already exists"),
        )),
        Err(e) => Err(e.into()),
    }
}

/// `DROP SEQUENCE name [, ...] [CASCADE|RESTRICT]`. Mirrors [`execute_drop_view`]'s
/// two-phase validate/drop and wrong-object-type detection. Under RESTRICT a
/// sequence still owned by a live table's `serial` column is blocked (2BP01);
/// CASCADE drops it anyway. (Reverse dependencies on *manually* written
/// `nextval(...)` defaults, which have no OWNED BY link, remain untracked.)
fn execute_drop_sequence(
    catalog: &Arc<dyn TableEngine>,
    names: &[ast::ObjectName],
    cascade: bool,
    if_exists: bool,
) -> Result<QueryResult, PgError> {
    let snames = names
        .iter()
        .map(object_name_to_table_name)
        .collect::<Result<Vec<_>, _>>()?;
    for (i, name) in snames.iter().enumerate() {
        if snames[..i].contains(name) {
            return Err(PgError::new(
                sqlstate::DUPLICATE_OBJECT,
                format!("sequence \"{name}\" specified more than once"),
            ));
        }
    }
    let mut notices = Vec::new();
    let mut to_drop: Vec<String> = Vec::new();
    for name in &snames {
        if catalog.sequence("public", name).is_some() {
            to_drop.push(name.clone());
        } else if catalog.resolve_view(None, name).is_some() {
            return Err(PgError::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not a sequence"),
            )
            .with_hint("Use DROP VIEW to remove a view."));
        } else if catalog.open_table(name).is_ok() {
            return Err(PgError::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not a sequence"),
            )
            .with_hint("Use DROP TABLE to remove a table."));
        } else if if_exists {
            notices.push(Notice::notice(
                format!("sequence \"{name}\" does not exist, skipping"),
                None,
            ));
        } else {
            return Err(PgError::new(
                sqlstate::UNDEFINED_TABLE,
                format!("sequence \"{name}\" does not exist"),
            ));
        }
    }
    // RESTRICT: a sequence owned by a still-existing table (a `serial` column's
    // sequence) has a dependent default and cannot be dropped directly.
    if !cascade {
        for name in &to_drop {
            let Some(def) = catalog.sequence("public", name) else {
                continue;
            };
            let Some(owner) = def.owned_by.as_deref() else {
                continue;
            };
            let Ok(table) = catalog.open_table(owner) else {
                continue;
            };
            let column = table
                .schema()
                .columns
                .iter()
                .find(|c| {
                    c.default
                        .as_deref()
                        .is_some_and(|d| d.contains(&format!("nextval('{name}')")))
                })
                .map(|c| c.name.clone())
                .unwrap_or_default();
            return Err(PgError::new(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("cannot drop sequence {name} because other objects depend on it"),
            )
            .with_detail(format!(
                "default value for column {column} of table {owner} depends on sequence {name}"
            ))
            .with_hint("Use DROP ... CASCADE to drop the dependent objects too."));
        }
    }
    for name in &to_drop {
        catalog.drop_sequence("public", name)?;
    }
    Ok(QueryResult::Command {
        tag: "DROP SEQUENCE".into(),
        notices,
    })
}

/// `DROP INDEX name [, ...] [IF EXISTS] [CASCADE|RESTRICT]`. A DROP INDEX names
/// the index, not its table, so each target is resolved by scanning the session's
/// relations (temp store first, then the permanent engine, mirroring PG's
/// `pg_temp`-first search) for the index and dropping it from its owning table.
/// All targets are validated before any is dropped, so a multi-name DROP is
/// atomic like PG. An index backing a PRIMARY KEY / UNIQUE constraint cannot be
/// dropped directly (PG requires dropping the constraint). `CASCADE`/`RESTRICT`
/// are accepted and ignored: nothing depends on a plain index in this engine.
fn execute_drop_index(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<dyn TableEngine>,
    session: &Session,
    names: &[ast::ObjectName],
    _cascade: bool,
    if_exists: bool,
) -> Result<QueryResult, PgError> {
    let targets = names
        .iter()
        .map(|name| split_object_name(name, "index"))
        .collect::<Result<Vec<_>, _>>()?;
    // A target named twice is rejected up front; the schema qualifier is part of
    // the identity, so `s1.i, s2.i` are distinct and allowed.
    for (i, target) in targets.iter().enumerate() {
        if targets[..i].contains(target) {
            return Err(PgError::new(
                sqlstate::DUPLICATE_OBJECT,
                format!("index \"{}\" specified more than once", target.1),
            ));
        }
    }
    // Snapshot the engine's relations once, split into the permanent set and this
    // session's temp set (the shared visibility rule).
    let (permanent_meta, temp_meta) =
        partition_session_relations(engine.relation_metadata(), &session.temp_schema);
    let mut notices = Vec::new();
    // (namespace, owning table, index name) to remove in phase 2.
    let mut to_drop: Vec<(String, String, String)> = Vec::new();
    for (schema_qual, name) in &targets {
        let ns = schema_qual.as_deref().unwrap_or("public");
        // A temp index lives in the session temp keyspace (`pg_temp`); only an
        // unqualified or temp-qualified name can name one, and it shadows a
        // permanent index of the same name.
        let temp_qualified = matches!(schema_qual.as_deref(), None | Some("pg_temp"))
            || schema_qual.as_deref() == Some(session.temp_schema.as_str());
        let owner = temp_qualified
            .then(|| {
                temp_meta
                    .iter()
                    .find(|r| r.indexes.iter().any(|i| i.name == *name))
                    .map(|rel| (true, rel))
            })
            .flatten()
            .or_else(|| {
                permanent_meta
                    .iter()
                    .find(|r| r.schema.namespace == ns && r.indexes.iter().any(|i| i.name == *name))
                    .map(|rel| (false, rel))
            });
        if let Some((is_temp, rel)) = owner {
            // An index backing a constraint requires the constraint be dropped
            // first — the same error PG raises (2BP01).
            let backs_constraint = rel
                .indexes
                .iter()
                .find(|i| i.name == *name)
                .is_some_and(|i| i.constraint.is_some());
            if backs_constraint {
                return Err(PgError::new(
                    sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop index {name} because constraint {name} on table {} requires it",
                        rel.schema.name
                    ),
                )
                .with_hint(format!(
                    "You can drop constraint {name} on table {} instead.",
                    rel.schema.name
                )));
            }
            // A temp index lives in this session's `pg_temp_N` namespace.
            let drop_ns = if is_temp {
                session.temp_schema.as_str()
            } else {
                rel.schema.namespace.as_str()
            };
            to_drop.push((drop_ns.to_string(), rel.schema.name.clone(), name.clone()));
        } else if catalog.resolve(schema_qual.as_deref(), name).is_ok() {
            return Err(PgError::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not an index"),
            )
            .with_hint("Use DROP TABLE to remove a table."));
        } else if catalog.resolve_view(schema_qual.as_deref(), name).is_some() {
            return Err(PgError::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not an index"),
            )
            .with_hint("Use DROP VIEW to remove a view."));
        } else if catalog.sequence(ns, name).is_some() {
            return Err(PgError::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not an index"),
            )
            .with_hint("Use DROP SEQUENCE to remove a sequence."));
        } else if if_exists {
            notices.push(Notice::notice(
                format!("index \"{name}\" does not exist, skipping"),
                None,
            ));
        } else {
            // PG reports a missing index as UNDEFINED_OBJECT (42704), not the
            // UNDEFINED_TABLE (42P01) it uses for tables/views/sequences.
            return Err(PgError::new(
                sqlstate::UNDEFINED_OBJECT,
                format!("index \"{name}\" does not exist"),
            ));
        }
    }
    // Route each drop to the concrete store that owns the index, rather than the
    // session overlay's table-name shadowing (which would misroute a permanent
    // index when a temp table shares its table's name).
    for (ns, table, index_name) in &to_drop {
        engine.drop_index(ns, table, index_name)?;
    }
    Ok(QueryResult::Command {
        tag: "DROP INDEX".into(),
        notices,
    })
}

/// `CREATE SCHEMA [IF NOT EXISTS] name`. Registers a user namespace with an
/// engine-allocated OID. Rejects a `pg_`-prefixed name (reserved for system
/// schemas, 42939), reports a collision as 42P06 (or a skip NOTICE under
/// `IF NOT EXISTS`), and does not yet support `AUTHORIZATION` or schema-element
/// forms.
fn execute_create_schema(
    engine: &Arc<dyn TableEngine>,
    schema_name: &ast::SchemaName,
    if_not_exists: bool,
) -> Result<QueryResult, PgError> {
    let name = match schema_name {
        ast::SchemaName::Simple(obj) => single_object_name(obj, "schema")?,
        ast::SchemaName::UnnamedAuthorization(_) | ast::SchemaName::NamedAuthorization(_, _) => {
            return Err(PgError::feature_not_supported(
                "CREATE SCHEMA AUTHORIZATION is not supported yet",
            ));
        }
    };
    // `pg_*` is reserved for system schemas, as in PG.
    if name.starts_with("pg_") {
        return Err(PgError::new(
            sqlstate::RESERVED_NAME,
            format!("unacceptable schema name \"{name}\""),
        )
        .with_detail("The prefix \"pg_\" is reserved for system schemas."));
    }
    // A name collides with an existing user schema or a reserved built-in.
    let exists =
        engine.schema_exists(&name) || matches!(name.as_str(), "public" | "information_schema");
    if exists {
        if if_not_exists {
            return Ok(QueryResult::Command {
                tag: "CREATE SCHEMA".into(),
                notices: vec![Notice::notice(
                    format!("schema \"{name}\" already exists, skipping"),
                    None,
                )],
            });
        }
        return Err(PgError::new(
            sqlstate::DUPLICATE_SCHEMA,
            format!("schema \"{name}\" already exists"),
        ));
    }
    engine.create_schema(&name)?;
    Ok(QueryResult::command("CREATE SCHEMA"))
}

/// `DROP SCHEMA [IF EXISTS] name [, ...] [CASCADE|RESTRICT]`. Two-phase like
/// [`execute_drop_sequence`]: every target is validated (missing → 3F000, or a
/// skip NOTICE under `IF EXISTS`) before anything is dropped. Under RESTRICT
/// (the default) a non-empty schema is an error (2BP01); CASCADE first drops the
/// schema's contained tables, views, and sequences (emitting a `drop cascades
/// to ...` NOTICE), then the schema.
fn execute_drop_schema(
    engine: &Arc<dyn TableEngine>,
    names: &[ast::ObjectName],
    cascade: bool,
    if_exists: bool,
) -> Result<QueryResult, PgError> {
    let snames = names
        .iter()
        .map(|n| single_object_name(n, "schema"))
        .collect::<Result<Vec<_>, _>>()?;
    for (i, name) in snames.iter().enumerate() {
        if snames[..i].contains(name) {
            return Err(PgError::new(
                sqlstate::DUPLICATE_OBJECT,
                format!("schema \"{name}\" specified more than once"),
            ));
        }
    }
    // Phase 1: validate every target before dropping any.
    let mut notices = Vec::new();
    let mut to_drop: Vec<String> = Vec::new();
    for name in &snames {
        if engine.schema_exists(name) {
            to_drop.push(name.clone());
        } else if if_exists {
            notices.push(Notice::notice(
                format!("schema \"{name}\" does not exist, skipping"),
                None,
            ));
        } else {
            return Err(PgError::new(
                sqlstate::INVALID_SCHEMA_NAME,
                format!("schema \"{name}\" does not exist"),
            ));
        }
    }
    // Phase 2: for each surviving target, enumerate its contents (a `(kind,
    // name)` list, sorted for deterministic output), then apply RESTRICT/CASCADE.
    for name in &to_drop {
        let tables: Vec<String> = engine
            .relation_metadata()
            .into_iter()
            .filter(|r| r.schema.namespace == *name)
            .map(|r| r.schema.name)
            .collect();
        // A serial column's OWNED BY sequence is an internal dependency of its
        // table, dropped silently with it (as in `DROP TABLE`), so it is neither
        // listed nor RESTRICT-blocking; only standalone sequences are dependents.
        let mut owned_seqs: Vec<String> = Vec::new();
        let mut contents: Vec<(&str, String)> = Vec::new();
        for t in &tables {
            contents.push(("table", t.clone()));
        }
        for v in engine.views() {
            if v.namespace == *name {
                contents.push(("view", v.name));
            }
        }
        for s in engine.sequences() {
            if s.namespace != *name {
                continue;
            }
            match s.owned_by.as_deref() {
                Some(owner) if tables.iter().any(|t| t == owner) => owned_seqs.push(s.name),
                _ => contents.push(("sequence", s.name)),
            }
        }
        contents.sort();

        // Objects in *other* schemas can depend on this schema's relations — an
        // inheritance child, or a view over one of its tables *or views*. They
        // are dependents of the schema just as its own contents are, and CASCADE
        // has to take them too: leaving a child behind strands its `inherits`
        // link on a parent that no longer exists, and recreating that name would
        // silently re-adopt it.
        //
        // Seeded from tables *and* views, because `ViewDefinition::depends_on`
        // records view-over-view edges — a probe over tables alone misses an
        // outside view whose only path into this schema runs through a view in
        // it.
        let graph = dependency_graph(engine);
        let in_schema: Vec<QualifiedRelation> = contents
            .iter()
            .filter(|(kind, _)| *kind == "table" || *kind == "view")
            .map(|(_, obj)| (name.clone(), obj.clone()))
            .collect();
        let external: Vec<Dependent> = drop_dependents(&graph, &in_schema)
            .into_iter()
            .filter(|(_, ns, _)| ns != name)
            .collect();

        if !contents.is_empty() || !external.is_empty() {
            if !cascade {
                // RESTRICT. The schema's own contents depend on the *schema*;
                // an outside dependent depends on the particular relation it
                // names, and PG says which — so report the real edge rather
                // than re-attributing it to the schema.
                //
                // Interleaved, not grouped: PG lists each of the schema's own
                // objects followed immediately by whatever depends on *that*
                // object, so `s.p` is followed by the child of `s.p` rather than
                // by the schema's next content.
                let edges = dependency_edges(&graph, &in_schema);
                let mut detail: Vec<String> = Vec::new();
                for (kind, obj) in &contents {
                    detail.push(format!("{kind} {name}.{obj} depends on schema {name}"));
                    for ((dkind, dns, dn), (_, tn)) in &edges {
                        if dns != name && tn == obj {
                            detail.push(format!(
                                "{} {} depends on {} {}",
                                dkind.noun(),
                                dep_display(dns, dn),
                                kind,
                                dep_display(name, tn)
                            ));
                        }
                    }
                }
                return Err(PgError::new(
                    sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!("cannot drop schema {name} because other objects depend on it"),
                )
                .with_detail(detail.join("\n"))
                .with_hint("Use DROP ... CASCADE to drop the dependent objects too."));
            }
            // CASCADE: PG's `drop cascades to ...` NOTICE over the schema's own
            // contents and every outside dependent, then the drops.
            // Same interleaving as the RESTRICT DETAIL above: each of the
            // schema's own objects, then whatever depended on that one.
            let edges = dependency_edges(&graph, &in_schema);
            let mut lines: Vec<String> = Vec::new();
            let mut listed: Vec<&Dependent> = Vec::new();
            for (kind, obj) in &contents {
                lines.push(format!("drop cascades to {kind} {name}.{obj}"));
                for (dep, (_, tn)) in &edges {
                    if dep.1 != *name && tn == obj && !listed.contains(&dep) {
                        lines.push(format!(
                            "drop cascades to {} {}",
                            dep.0.noun(),
                            dep_display(&dep.1, &dep.2)
                        ));
                        listed.push(dep);
                    }
                }
            }
            // Anything reached only transitively (a view over a cascaded child)
            // has no direct edge into the schema, so it follows.
            for dep in &external {
                if !listed.contains(&dep) {
                    lines.push(format!(
                        "drop cascades to {} {}",
                        dep.0.noun(),
                        dep_display(&dep.1, &dep.2)
                    ));
                }
            }
            notices.push(if lines.len() == 1 {
                Notice::notice(lines[0].clone(), None)
            } else {
                Notice::notice(
                    format!("drop cascades to {} other objects", lines.len()),
                    Some(lines.join("\n")),
                )
            });
            // Outside dependents first, for the same reason `drop_cascaded`
            // orders views ahead of tables: never leave the catalog holding an
            // object that points at one already removed.
            drop_cascaded(engine, &external)?;
            for (kind, obj) in &contents {
                let _ = match *kind {
                    "table" => engine.drop_table(name, obj),
                    "view" => engine.drop_view(name, obj),
                    _ => engine.drop_sequence(name, obj),
                };
            }
            // A cascaded outside table owns its serial sequences just as one in
            // this schema does; `owned_seqs` only covers the latter, so sweep
            // the former here (as `execute_drop_table` does for its own
            // cascade). Otherwise `c_id_seq` outlives `c`.
            for seq in engine.sequences() {
                let owned_by_cascaded = seq.owned_by.as_deref().is_some_and(|owner| {
                    external.iter().any(|(kind, ns, t)| {
                        *kind == DependentKind::Table && *ns == seq.namespace && t == owner
                    })
                });
                if owned_by_cascaded {
                    let _ = engine.drop_sequence(&seq.namespace, &seq.name);
                }
            }
        }
        // Drop the serial-owned sequences silently, with their tables (they were
        // excluded from the cascade listing as internal dependencies).
        for seq in &owned_seqs {
            let _ = engine.drop_sequence(name, seq);
        }
        engine.drop_schema(name)?;
    }
    Ok(QueryResult::Command {
        tag: "DROP SCHEMA".into(),
        notices,
    })
}

/// A relation as `(namespace, name)` — how a drop target and a dependency edge
/// name what they point at. Matched on the qualified [`dep_key`], so a dependent
/// in any schema is found, cross-schema included.
type QualifiedRelation = (String, String);

/// What a dependent found by [`plan_drop_cascade`] is, so the caller drops it
/// with the right verb and the messages name it correctly.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DependentKind {
    Table,
    View,
}

impl DependentKind {
    fn noun(self) -> &'static str {
        match self {
            DependentKind::Table => "table",
            DependentKind::View => "view",
        }
    }
}

/// One dependent object: what it is and where it lives.
type Dependent = (DependentKind, String, String);

/// Drop the objects a CASCADE pulled in, views first.
///
/// The order is for the catalog's sake, not for a dependency check —
/// `TableEngine::drop_table` performs none. Dropping a view before the relation
/// it reads means the catalog never holds a view whose stored query names a
/// relation that is already gone, so a concurrent reader cannot observe one.
fn drop_cascaded(catalog: &Arc<dyn TableEngine>, dependents: &[Dependent]) -> Result<(), PgError> {
    for (kind, ns, name) in dependents {
        if *kind == DependentKind::View {
            catalog.drop_view(ns, name)?;
        }
    }
    for (kind, ns, name) in dependents {
        if *kind == DependentKind::Table {
            catalog.drop_table(ns, name)?;
        }
    }
    Ok(())
}

/// The qualified `"namespace.name"` key both dependency sources speak.
fn dep_key(namespace: &str, name: &str) -> String {
    format!("{namespace}.{name}")
}

/// How PostgreSQL names an object in a dependency message: bare in `public`,
/// schema-qualified everywhere else.
fn dep_display(namespace: &str, name: &str) -> String {
    if namespace == "public" {
        name.to_string()
    } else {
        format!("{namespace}.{name}")
    }
}

/// Every object that can depend on a relation, paired with the keys it depends
/// on — the graph both the RESTRICT report and the CASCADE closure read.
///
/// Two kinds of edge feed it. A **view** depends on every relation its stored
/// query reads. An **inheritance child** depends on each of its `INHERITS (...)`
/// parents; PostgreSQL treats that link exactly as it treats a view's, refusing
/// the drop under RESTRICT and dropping the child under CASCADE. (A *partition*
/// is different: it is an internal dependency PostgreSQL drops with its parent
/// and no CASCADE, which `execute_drop_table` handles by expanding the target
/// set instead.)
///
/// A child of two parents contributes one entry per parent rather than one
/// merged entry. Both readers are indifferent — RESTRICT iterates targets on the
/// outside, so it reports one line per matching (dependent, target) edge either
/// way, and the closure dedupes on its own — so folding them would be
/// bookkeeping that buys nothing.
fn dependency_graph(catalog: &Arc<dyn TableEngine>) -> Vec<(Dependent, Vec<String>)> {
    let mut graph: Vec<(Dependent, Vec<String>)> = Vec::new();
    for view in catalog.views() {
        graph.push((
            (
                DependentKind::View,
                view.namespace.clone(),
                view.name.clone(),
            ),
            view.depends_on.clone(),
        ));
    }
    // `inheritance_links` rather than `relation_metadata`: a DROP should not
    // clone (and stat) the whole catalog to read a usually-empty list.
    //
    // Sorted, because the engine reads them out of a `HashMap` and so hands them
    // over in an order that varies between processes. Everything downstream —
    // the DETAIL lines, the `drop cascades to` list — is user-visible output
    // that must not depend on that. (PostgreSQL orders by OID, i.e. creation
    // order; by name is a deliberate divergence, since nothing here has OIDs.)
    let mut links = catalog.inheritance_links();
    links.sort();
    for (child, parent) in links {
        graph.push((
            (DependentKind::Table, child.0, child.1),
            vec![dep_key(&parent.0, &parent.1)],
        ));
    }
    graph
}

/// The direct `(dependent, target)` edges into `targets`, in target order then
/// dependent order — PostgreSQL's DETAIL ordering. A target that is itself being
/// dropped is not reported as depending on another.
fn dependency_edges(
    graph: &[(Dependent, Vec<String>)],
    targets: &[QualifiedRelation],
) -> Vec<(Dependent, QualifiedRelation)> {
    let target_keys: Vec<String> = targets.iter().map(|(ns, n)| dep_key(ns, n)).collect();
    let mut edges = Vec::new();
    for (tns, tn) in targets {
        let tkey = dep_key(tns, tn);
        for (dep, depends_on) in graph {
            if target_keys.contains(&dep_key(&dep.1, &dep.2)) {
                continue;
            }
            if depends_on.iter().any(|d| *d == tkey) {
                edges.push((dep.clone(), (tns.clone(), tn.clone())));
            }
        }
    }
    edges
}

/// The transitive set of objects a CASCADE must take with `targets`, in
/// discovery order. An object joins when anything it depends on is already
/// going — so a view over an inheritance child that is itself cascading away
/// comes along too.
fn drop_dependents(
    graph: &[(Dependent, Vec<String>)],
    targets: &[QualifiedRelation],
) -> Vec<Dependent> {
    let mut removed: HashSet<String> = targets.iter().map(|(ns, n)| dep_key(ns, n)).collect();
    let mut dependents: Vec<Dependent> = Vec::new();
    loop {
        let mut added = false;
        for (dep, depends_on) in graph {
            let dkey = dep_key(&dep.1, &dep.2);
            if removed.contains(&dkey) {
                continue;
            }
            if depends_on.iter().any(|d| removed.contains(d)) {
                removed.insert(dkey);
                dependents.push(dep.clone());
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    dependents
}

/// PostgreSQL's `drop cascades to ...` NOTICE(s) for a set of dependents: one
/// line on its own, more than one summarized with the lines as DETAIL —
/// matching the wording of `DROP TYPE ... CASCADE`.
fn cascade_notices(dependents: &[Dependent]) -> Vec<Notice> {
    let line = |(kind, ns, name): &Dependent| {
        format!("drop cascades to {} {}", kind.noun(), dep_display(ns, name))
    };
    match dependents {
        [] => Vec::new(),
        [only] => vec![Notice::notice(line(only), None)],
        many => vec![Notice::notice(
            format!("drop cascades to {} other objects", many.len()),
            Some(many.iter().map(line).collect::<Vec<_>>().join("\n")),
        )],
    }
}

/// `DROP TABLE`/`DROP VIEW`'s use of the three above: under RESTRICT any
/// dependent is an error (2BP01, one DETAIL line per edge); under CASCADE the
/// transitive set to drop plus its notices.
fn plan_drop_cascade(
    catalog: &Arc<dyn TableEngine>,
    target_noun: &str,
    targets: &[QualifiedRelation],
    cascade: bool,
) -> Result<(Vec<Dependent>, Vec<Notice>), PgError> {
    let graph = dependency_graph(catalog);
    if !cascade {
        let edges = dependency_edges(&graph, targets);
        if let Some(((_, _, _), (tns, tn))) = edges.first() {
            let blocked = dep_display(tns, tn);
            let detail = edges
                .iter()
                .map(|((kind, dns, dn), (tns, tn))| {
                    format!(
                        "{} {} depends on {target_noun} {}",
                        kind.noun(),
                        dep_display(dns, dn),
                        dep_display(tns, tn)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(PgError::new(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("cannot drop {target_noun} {blocked} because other objects depend on it"),
            )
            .with_detail(detail)
            .with_hint("Use DROP ... CASCADE to drop the dependent objects too."));
        }
        return Ok((Vec::new(), Vec::new()));
    }
    let dependents = drop_dependents(&graph, targets);
    let notices = cascade_notices(&dependents);
    Ok((dependents, notices))
}

/// `DROP TYPE name [, ...] [CASCADE|RESTRICT]`. All names are resolved and
/// validated before any type is removed, so a failure on one target leaves the
/// others intact (PG runs a multi-target DROP atomically).
fn execute_drop_type(
    catalog: &GlobalCatalog,
    names: &[ast::ObjectName],
    cascade: bool,
    if_exists: bool,
) -> Result<QueryResult, PgError> {
    let tnames = names
        .iter()
        .map(|name| single_object_name(name, "type"))
        .collect::<Result<Vec<_>, _>>()?;
    let refs: Vec<&str> = tnames.iter().map(String::as_str).collect();
    let notices = catalog.drop_types(&refs, cascade, if_exists)?;
    Ok(QueryResult::Command {
        tag: "DROP TYPE".into(),
        notices: to_notices(notices),
    })
}

/// `DROP FUNCTION name [ ( argtypes ) ] [, ...] [IF EXISTS] [CASCADE|RESTRICT]`.
/// Only `LANGUAGE internal` functions exist in this catalog, but the drop path
/// is general: each target is resolved by (name, argument types), with the
/// argument list optional when the name is unambiguous, as in PG. `OUT`
/// parameters do not contribute to a function's identity, so they are left out
/// of the lookup signature.
fn execute_drop_function(
    catalog: &GlobalCatalog,
    engine: &Arc<dyn TableEngine>,
    type_catalog: &Arc<dyn TypeCatalog>,
    drop: &ast::DropFunction,
) -> Result<QueryResult, PgError> {
    execute_drop_routine(
        catalog,
        engine,
        type_catalog,
        RoutineKind::Function,
        &drop.func_desc,
        drop.if_exists,
        drop.drop_behavior.as_ref(),
    )
}

/// `DROP FUNCTION`/`DROP PROCEDURE`. The two differ only in which kind of
/// routine they may name: PostgreSQL reports `42809` rather than "does not
/// exist" when the name resolves to the other kind, because a confusing
/// success is worse than a clear refusal.
fn execute_drop_routine(
    catalog: &GlobalCatalog,
    engine: &Arc<dyn TableEngine>,
    type_catalog: &Arc<dyn TypeCatalog>,
    kind: RoutineKind,
    descs: &[ast::FunctionDesc],
    if_exists: bool,
    drop_behavior: Option<&ast::DropBehavior>,
) -> Result<QueryResult, PgError> {
    let mut specs: Vec<FuncDropSpec> = Vec::with_capacity(descs.len());
    for desc in descs {
        let name = single_object_name(&desc.name, kind.noun())?;
        let args = match &desc.args {
            Some(args) => {
                let mut resolved = Vec::new();
                for arg in args {
                    if matches!(arg.mode, Some(ast::ArgMode::Out)) {
                        continue;
                    }
                    resolved.push(resolve_type_ref(catalog, &arg.data_type)?);
                }
                Some(resolved)
            }
            None => None,
        };
        specs.push(FuncDropSpec { name, args });
    }
    // Naming the same routine twice — via two identical signatures, or a bare
    // name and its signature — is rejected while resolving, once each target is
    // a concrete routine.
    let cascade = matches!(drop_behavior, Some(ast::DropBehavior::Cascade));
    let resolved = catalog.resolve_drop_routines(&specs, kind, if_exists)?;
    // A procedure cannot appear in an expression (the binder rejects one with
    // 42809), so nothing this scan looks at could depend on it.
    let dependents = match kind {
        RoutineKind::Function => {
            crate::func_deps::routine_dependents(engine, type_catalog, &resolved)
        }
        RoutineKind::Procedure => Vec::new(),
    };
    if !dependents.is_empty() {
        // PostgreSQL's CASCADE drops the dependent constraint / clears the
        // default. Nothing here can do either — there is no
        // `ALTER TABLE DROP CONSTRAINT`, no `ALTER COLUMN DROP DEFAULT`, and no
        // engine method behind them — so CASCADE is refused rather than
        // reported as done. The alternative is the silent success this guard
        // exists to remove. CASCADE with no dependents is unaffected.
        if cascade {
            return Err(PgError::feature_not_supported(
                "DROP FUNCTION ... CASCADE is not supported yet",
            )
            .with_detail(
                crate::func_deps::dependency_error(&resolved, &dependents)
                    .detail
                    .unwrap_or_default(),
            ));
        }
        return Err(crate::func_deps::dependency_error(&resolved, &dependents));
    }
    let notices = catalog.drop_resolved_routines(&specs, &resolved);
    Ok(QueryResult::Command {
        tag: match kind {
            RoutineKind::Function => "DROP FUNCTION".into(),
            RoutineKind::Procedure => "DROP PROCEDURE".into(),
        },
        notices: to_notices(notices),
    })
}

/// `DROP CAST (source AS target) [CASCADE|RESTRICT]`.
fn execute_drop_cast(
    catalog: &GlobalCatalog,
    source: &ast::DataType,
    target: &ast::DataType,
    if_exists: bool,
) -> Result<QueryResult, PgError> {
    let source_ref = resolve_type_ref(catalog, source)?;
    let target_ref = resolve_type_ref(catalog, target)?;
    let notices = catalog.drop_cast(source_ref, target_ref, if_exists)?;
    Ok(QueryResult::Command {
        tag: "DROP CAST".into(),
        notices: to_notices(notices),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `build_sort_key` for the `ORDER BY` of a parsed `CREATE TABLE`, against a
    /// two-column `(id int4, at timestamp)` relation.
    ///
    /// The key it returns has no SQL surface — nothing reflects it in
    /// `pg_catalog`, `\d`, or EXPLAIN — so the DDL tests can only prove the
    /// statement was accepted. These prove what was actually recorded.
    fn sort_key_of(sql: &str, pk: Option<&IndexMetadata>) -> Result<Vec<IndexKey>, PgError> {
        let stmts = crabgresql_parser::parse(sql).expect("parse");
        let ast::Statement::CreateTable(create) = &stmts[0] else {
            panic!("expected CREATE TABLE");
        };
        let mut schema = TableSchema::new(
            "t",
            vec![
                Column::new("id", PgType::Int4),
                Column::new("at", PgType::Timestamp),
            ],
        );
        schema.access_method = TableAccessMethod::Parquet;
        build_sort_key(
            create.order_by.as_ref(),
            TableAccessMethod::Parquet,
            &schema,
            pk,
        )
    }

    /// [`sort_key_of`] for a statement expected to be accepted.
    fn sort_key(sql: &str) -> Vec<IndexKey> {
        sort_key_of(sql, None).unwrap_or_else(|e| panic!("{sql}: {}", e.message))
    }

    fn key(column: usize) -> IndexKey {
        IndexKey {
            column,
            descending: false,
            nulls_first: false,
        }
    }

    #[test]
    fn an_explicit_sort_key_records_column_indexes_in_clause_order() {
        // `at` is column 1 and comes first, so a key that recorded clause
        // positions instead of column indexes would read `[0, 1]` and pass every
        // DDL-level test in the suite.
        assert_eq!(
            sort_key("CREATE TABLE t (id int4, at timestamp) ORDER BY (at, id)"),
            vec![key(1), key(0)]
        );
        // One column, with and without parentheses, is the same key.
        assert_eq!(
            sort_key("CREATE TABLE t (id int4) ORDER BY (id)"),
            vec![key(0)]
        );
        assert_eq!(
            sort_key("CREATE TABLE t (id int4) ORDER BY id"),
            vec![key(0)]
        );
    }

    #[test]
    fn the_primary_key_default_is_the_whole_key_in_its_own_order() {
        let pk = IndexMetadata {
            name: "t_pkey".to_string(),
            method: IndexMethod::BTree,
            keys: vec![key(1), key(0)],
            unique: true,
            nulls_distinct: true,
            constraint: Some(IndexConstraint::PrimaryKey),
        };
        assert_eq!(
            sort_key_of("CREATE TABLE t (id int4, at timestamp)", Some(&pk))
                .expect("the PRIMARY KEY supplies the default"),
            vec![key(1), key(0)]
        );
    }

    #[test]
    fn a_sort_key_that_cannot_be_honored_is_refused() {
        let code = |sql: &str| {
            sort_key_of(sql, None)
                .expect_err("must be refused")
                .code
                .to_string()
        };
        // Declared nothing at all.
        assert_eq!(code("CREATE TABLE t (id int4)"), "42P17");
        // ClickHouse's `ORDER BY tuple()` opt-out, which we do not offer.
        assert_eq!(code("CREATE TABLE t (id int4) ORDER BY ()"), "42P17");
        assert_eq!(code("CREATE TABLE t (id int4) ORDER BY (id, id)"), "42P17");
        assert_eq!(code("CREATE TABLE t (id int4) ORDER BY (nope)"), "42703");
        assert_eq!(code("CREATE TABLE t (id int4) ORDER BY (id + 1)"), "0A000");
    }

    #[test]
    fn a_method_with_no_layout_takes_no_key_and_refuses_the_clause() {
        let stmts =
            crabgresql_parser::parse("CREATE TABLE t (id int4) ORDER BY (id)").expect("parse");
        let ast::Statement::CreateTable(create) = &stmts[0] else {
            panic!("expected CREATE TABLE");
        };
        let schema = TableSchema::new("t", vec![Column::new("id", PgType::Int4)]);
        // Heap with the clause: refused rather than recorded and never honored.
        let err = build_sort_key(
            create.order_by.as_ref(),
            TableAccessMethod::Heap,
            &schema,
            None,
        )
        .expect_err("heap must refuse ORDER BY");
        assert_eq!(err.code, "0A000");
        // Heap without it: an empty key, not an error.
        assert_eq!(
            build_sort_key(None, TableAccessMethod::Heap, &schema, None)
                .expect("heap needs no key"),
            Vec::new()
        );
    }

    /// The referenced relations of the query in a `SELECT` statement.
    fn deps(sql: &str) -> Vec<String> {
        let stmts = crabgresql_parser::parse(sql).expect("parse");
        match &stmts[0] {
            ast::Statement::Query(query) => referenced_relations(query),
            other => panic!("expected a query, got {other:?}"),
        }
    }

    #[test]
    fn referenced_relations_excludes_cte_names_but_keeps_their_bodies() {
        // A CTE name is not a dependency; the base table inside its body is.
        // Dependencies are recorded as qualified `namespace.name` keys.
        assert_eq!(
            deps("WITH c AS (SELECT 1) SELECT * FROM c"),
            Vec::<String>::new()
        );
        assert_eq!(
            deps("WITH c AS (SELECT * FROM t) SELECT * FROM c"),
            vec!["public.t"]
        );
        // A schema-qualified reference keeps its schema.
        assert_eq!(deps("SELECT * FROM app.t"), vec!["app.t"]);
    }

    #[test]
    fn referenced_relations_scopes_cte_shadowing_to_its_own_query() {
        // The CTE `c` shadows a base table only inside the derived subquery; the
        // outer `c` is the real relation and must remain a dependency.
        assert_eq!(
            deps("SELECT * FROM (WITH c AS (SELECT 1) SELECT * FROM c) d, c"),
            vec!["public.c"]
        );
    }

    /// Extract the base-type option list from a parsed `CREATE TYPE name (...)`.
    fn create_type_options(sql: &str) -> Vec<ast::UserDefinedTypeSqlDefinitionOption> {
        let stmts = crabgresql_parser::parse(sql).expect("parse");
        match &stmts[0] {
            ast::Statement::CreateType {
                representation: Some(ast::UserDefinedTypeRepresentation::SqlDefinition { options }),
                ..
            } => options.clone(),
            other => panic!("expected CREATE TYPE base definition, got {other:?}"),
        }
    }

    #[test]
    fn like_builtin_target_sets_width_and_backing() {
        let catalog = GlobalCatalog::new();
        let options = create_type_options("CREATE TYPE t (input = ti, output = tou, like = int8)");
        let (typlen, backing) = type_shape_from_options(&catalog, &options).expect("ok");
        assert_eq!(typlen, 8);
        assert_eq!(backing, Some(PgType::Int8));
    }

    #[test]
    fn unknown_like_target_is_undefined_object_with_caret() {
        let catalog = GlobalCatalog::new();
        let options =
            create_type_options("CREATE TYPE t (input = ti, output = tou, like = no_such_type)");
        let err = type_shape_from_options(&catalog, &options).expect_err("must reject");
        assert_eq!(err.code, sqlstate::UNDEFINED_OBJECT);
        assert_eq!(err.message, "type \"no_such_type\" does not exist");
        // Carries a cursor position so the client can render a LINE/caret excerpt.
        assert!(
            err.location.is_some(),
            "unknown LIKE target must carry a position"
        );
    }

    #[test]
    fn qualified_like_target_does_not_resolve_and_echoes_full_name() {
        let catalog = GlobalCatalog::new();
        // `int8` is a builtin, but crabgresql has no schema namespaces, so a
        // schema-qualified target must not resolve to it (PG rejects
        // `public.int8`), and the error echoes the full qualified name.
        let options =
            create_type_options("CREATE TYPE t (input = ti, output = tou, like = public.int8)");
        let err = type_shape_from_options(&catalog, &options).expect_err("must reject");
        assert_eq!(err.code, sqlstate::UNDEFINED_OBJECT);
        assert_eq!(err.message, "type \"public.int8\" does not exist");
    }

    /// Feed [`find_duplicate`] the rows a key-projected scan would hand it.
    /// The engine-level cases (which relation, which snapshot) are covered by
    /// the e2e tests; these pin the rule itself, which is pure.
    fn duplicate_of(
        columns: Vec<Column>,
        keys: Vec<usize>,
        nulls_distinct: bool,
        rows: Vec<Vec<Value>>,
    ) -> Option<Vec<Value>> {
        let schema = TableSchema::new("t", columns);
        let index = IndexMetadata {
            name: "t_pkey".into(),
            method: IndexMethod::BTree,
            keys: keys.into_iter().map(key).collect(),
            unique: true,
            nulls_distinct,
            constraint: Some(IndexConstraint::PrimaryKey),
        };
        find_duplicate(rows.into_iter().map(Ok), &schema, &index).expect("scan")
    }

    fn int_column() -> Vec<Column> {
        vec![Column::new("k", PgType::Int4)]
    }

    /// [`duplicate_of`] with the run length forced small enough to reach the
    /// short-circuit, plus how many rows were actually pulled from the scan.
    fn duplicate_in_runs(
        run: usize,
        keys: Vec<usize>,
        rows: Vec<Vec<Value>>,
    ) -> (Option<Vec<Value>>, usize) {
        let schema = TableSchema::new("t", int_column());
        let index = IndexMetadata {
            name: "t_pkey".into(),
            method: IndexMethod::BTree,
            keys: keys.into_iter().map(key).collect(),
            unique: true,
            nulls_distinct: true,
            constraint: Some(IndexConstraint::PrimaryKey),
        };
        let mut pulled = 0;
        let found = find_duplicate_in_runs(
            rows.into_iter().map(|row| {
                pulled += 1;
                Ok(row)
            }),
            &schema,
            &index,
            run,
        )
        .expect("scan");
        (found, pulled)
    }

    fn ints(values: &[i32]) -> Vec<Vec<Value>> {
        values.iter().map(|&v| vec![Value::Int4(v)]).collect()
    }

    /// The point of sorting in runs: a duplicate inside one of them is reported
    /// without reading the rest of the relation. A table that is merely large
    /// used to have to be read to the end before it could be refused.
    #[test]
    fn a_duplicate_inside_a_run_stops_the_scan() {
        let rows = ints(&[7, 3, 7, 1, 5, 9, 2, 8, 6, 4]);
        let (found, pulled) = duplicate_in_runs(4, vec![0], rows);
        assert_eq!(found, Some(vec![Value::Int4(7)]));
        assert_eq!(
            pulled, 4,
            "the scan should stop at the end of the first run"
        );
    }

    /// The runs only bound the common case. A pair further apart than one run
    /// survives to the final sort, which is what keeps the check exhaustive.
    #[test]
    fn a_pair_split_across_runs_is_still_found() {
        let rows = ints(&[7, 3, 1, 5, 9, 2, 8, 7]);
        let (found, pulled) = duplicate_in_runs(4, vec![0], rows);
        assert_eq!(found, Some(vec![Value::Int4(7)]));
        assert_eq!(
            pulled, 8,
            "a split pair can only be found after a full scan"
        );
    }

    /// Exactly one full run and nothing after it: already sorted, already
    /// scanned, and the tail must not report a duplicate that is not there.
    #[test]
    fn a_single_full_run_of_distinct_keys_is_clean() {
        assert_eq!(duplicate_in_runs(4, vec![0], ints(&[4, 2, 3, 1])).0, None);
    }

    /// A key column may legally repeat — PostgreSQL accepts `UNIQUE (a, a)` and
    /// renders it `Key (a, a)=(5, 5)`. The key has to carry the value twice, so
    /// the row cannot be read destructively here.
    #[test]
    fn a_repeated_key_column_carries_its_value_twice() {
        let rows = vec![vec![Value::Int4(5)], vec![Value::Int4(5)]];
        assert_eq!(
            duplicate_of(int_column(), vec![0, 0], true, rows),
            Some(vec![Value::Int4(5), Value::Int4(5)])
        );
    }

    #[test]
    fn distinct_keys_have_no_duplicate() {
        let rows = vec![
            vec![Value::Int4(3)],
            vec![Value::Int4(1)],
            vec![Value::Int4(2)],
        ];
        assert_eq!(duplicate_of(int_column(), vec![0], true, rows), None);
    }

    #[test]
    fn a_repeated_key_is_reported_whatever_the_scan_order() {
        // Sorting is what makes the pair adjacent, so the answer must not
        // depend on how far apart the two rows were in the scan.
        for spread in [1, 2, 3] {
            let mut rows = vec![vec![Value::Int4(7)]];
            rows.extend((0..spread).map(|i| vec![Value::Int4(100 + i)]));
            rows.push(vec![Value::Int4(7)]);
            assert_eq!(
                duplicate_of(int_column(), vec![0], true, rows),
                Some(vec![Value::Int4(7)]),
                "spread {spread}"
            );
        }
    }

    /// Under the default NULLS DISTINCT a NULL key is exempt, so any number of
    /// them coexist; NULLS NOT DISTINCT makes two of them collide. The value
    /// renders as the bare token `null` in the caller's DETAIL, which is why
    /// the row itself has to come back rather than a formatted string.
    #[test]
    fn null_keys_collide_only_when_nulls_are_not_distinct() {
        let rows = vec![vec![Value::Null], vec![Value::Null], vec![Value::Int4(5)]];
        assert_eq!(
            duplicate_of(int_column(), vec![0], true, rows.clone()),
            None
        );
        assert_eq!(
            duplicate_of(int_column(), vec![0], false, rows),
            Some(vec![Value::Null])
        );
    }

    /// A multi-column key is duplicated only when *every* column matches, and a
    /// NULL in any one of them exempts the row under NULLS DISTINCT.
    #[test]
    fn a_composite_key_needs_every_column_to_match() {
        let columns = || {
            vec![
                Column::new("a", PgType::Int4),
                Column::new("b", PgType::Text),
            ]
        };
        let partial = vec![
            vec![Value::Int4(1), Value::Text("x".into())],
            vec![Value::Int4(1), Value::Text("y".into())],
            vec![Value::Int4(2), Value::Text("x".into())],
        ];
        assert_eq!(duplicate_of(columns(), vec![0, 1], true, partial), None);
        let full = vec![
            vec![Value::Int4(1), Value::Text("x".into())],
            vec![Value::Int4(2), Value::Text("y".into())],
            vec![Value::Int4(1), Value::Text("x".into())],
        ];
        assert_eq!(
            duplicate_of(columns(), vec![0, 1], true, full),
            Some(vec![Value::Int4(1), Value::Text("x".into())])
        );
        let one_null = vec![
            vec![Value::Int4(1), Value::Null],
            vec![Value::Int4(1), Value::Null],
        ];
        assert_eq!(duplicate_of(columns(), vec![0, 1], true, one_null), None);
    }

    /// Equality is the type's, not the representation's: `numeric` ignores
    /// trailing zeroes and `bpchar` ignores trailing blanks, so both pairs are
    /// duplicates even though the two rows differ byte for byte. This is the
    /// property a hash-bucketed dedup would have had to reproduce separately.
    #[test]
    fn equality_follows_the_type_not_the_bytes() {
        let numeric_value =
            |text: &str| Value::Numeric(crabgresql_types::Numeric::parse(text).expect("numeric"));
        let numeric = vec![vec![numeric_value("1.0")], vec![numeric_value("1.00")]];
        assert!(
            duplicate_of(
                vec![Column::new("k", PgType::Numeric)],
                vec![0],
                true,
                numeric
            )
            .is_some()
        );
        let padded = vec![
            vec![Value::Text("ab".into())],
            vec![Value::Text("ab  ".into())],
        ];
        assert!(
            duplicate_of(
                vec![Column::new("k", PgType::Bpchar)],
                vec![0],
                true,
                padded
            )
            .is_some()
        );
    }
}
