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
    BoundExpr, CopyFromPlan, InsertSource, LogicalPlan, bind_copy_from, bind_delete_with_params,
    bind_insert_with_params, bind_query, bind_query_with_params, bind_update_with_params,
    output_columns_of, param_ctx_extended, param_ctx_none, param_types, require_all_resolved,
    substitute_params,
};
use crabgresql_executor::{
    CatalogOps, DmlVerb, ExecContext, ExecNode, Execution, MaterializedRows, OutputColumn,
    RoutineOps, execute,
};
use crabgresql_parser::ast;
use crabgresql_pg_wire::{ErrorFields, TransactionStatus, sqlstate};
use crabgresql_storage_api::{
    Column, IndexConstraint, IndexKey, IndexMetadata, IndexMethod, PartitionBound,
    PartitionBoundDatum, PartitionOf, PartitionScheme, PartitionStrategy, RelPersistence,
    RelationMetadata, RoutineKind as ApiRoutineKind, RoutineSig, SequenceDefinition, StorageError,
    TableAccessMethod, TableAm, TableEngine, TableSchema, TypeCatalog, ViewDefinition,
};
use crabgresql_txn::{CommandId, IsolationLevel, TransactionManager, TxnContext, Xid};
use crabgresql_types::{PgType, Value};

use crate::catalog::{SessionCatalog, SessionCatalogOps};
use crate::error::PgError;
use crate::explain::{ExplainOptions, explain_columns, explain_result, run_analyze};
use crate::global_catalog::{
    ArgMode, CatalogNotice, FuncBody, FuncDropSpec, FuncInfo, GlobalCatalog, RoutineArg,
    RoutineDefinition, RoutineKind, TypeRef, Volatility,
};
use crate::routines::RoutineDispatch;
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
fn partition_session_relations(
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
    let system: Arc<crabgresql_catalog::SystemCatalog> = {
        let global = engine.clone();
        let schemas_engine = engine.clone();
        let database = session.database.clone();
        let owner = session.user.clone();
        let temp_schema = session.temp_schema.clone();
        let temp_schema_for_nsp = session.temp_schema.clone();
        let temp_namespace_oid = session.temp_namespace_oid;
        // User-defined types are reflected into pg_type/pg_enum on demand.
        let types = global_catalog.clone();
        let routines = global_catalog.clone();
        Arc::new(
            crabgresql_catalog::SystemCatalog::with_catalog_relations_fn(
                database,
                owner,
                move || {
                    // Reflect the permanent relations plus only THIS session's temp
                    // relations (the shared visibility rule).
                    let (permanent, own_temp) =
                        partition_session_relations(global.relation_metadata(), &temp_schema);
                    let mut rels: Vec<_> = permanent
                        .into_iter()
                        .map(crabgresql_catalog::CatalogRelation::permanent_metadata)
                        .collect();
                    rels.extend(own_temp.into_iter().map(|metadata| {
                        let mut relation = crabgresql_catalog::CatalogRelation::temporary(
                            metadata.schema,
                            temp_schema.clone(),
                        );
                        relation.indexes = metadata.indexes;
                        relation.stats = metadata.stats;
                        relation
                    }));
                    // Views reflect into pg_class as relkind='v' / pg_attribute
                    // columns / information_schema.tables as VIEW.
                    rels.extend(global.views().into_iter().map(|view| {
                        crabgresql_catalog::CatalogRelation::view(TableSchema::in_namespace(
                            view.name,
                            view.namespace,
                            view.columns,
                        ))
                    }));
                    // Sequences reflect into pg_class as relkind='S' and feed
                    // pg_catalog.pg_sequence.
                    rels.extend(global.sequences().into_iter().map(|seq| {
                        crabgresql_catalog::CatalogRelation::sequence(
                            seq.name,
                            seq.namespace,
                            crabgresql_catalog::CatalogSequence {
                                type_oid: seq.data_type.oid(),
                                start: seq.start,
                                increment: seq.increment,
                                min: seq.min,
                                max: seq.max,
                                cache: seq.cache,
                                cycle: seq.cycle,
                            },
                        )
                    }));
                    rels
                },
            )
            .with_user_types_fn(move || {
                types
                    .user_types()
                    .into_iter()
                    .map(|t| crabgresql_catalog::CatalogUserType {
                        oid: t.oid,
                        name: t.name,
                        enum_labels: t.enum_labels,
                    })
                    .collect()
            })
            .with_routines_fn(move || routines.functions().iter().map(catalog_routine).collect())
            .with_schemas_fn(move || {
                let mut schemas = schemas_engine.schemas();
                // Reflect this session's `pg_temp_N` namespace with a stable
                // synthetic OID, but only once it holds a temp relation (as PG
                // instantiates pg_temp_N lazily). Feeding it through the one
                // `schemas` list keeps `pg_namespace` and `pg_class.relnamespace`
                // consistent; nothing is persisted. `relation_names_in` is cheap.
                if !schemas_engine
                    .relation_names_in(&temp_schema_for_nsp)
                    .is_empty()
                {
                    schemas.push((temp_schema_for_nsp.clone(), temp_namespace_oid));
                }
                schemas
            }),
        )
    };
    let catalog: Arc<dyn TableEngine> = Arc::new(SessionCatalog::new(
        engine.clone(),
        system.clone(),
        session.temp_schema.clone(),
    ));
    // The global catalog is the binder's view of user-defined types and casts,
    // so an expression can cast to/from a `CREATE TYPE` name.
    let type_catalog: Arc<dyn TypeCatalog> = global_catalog.clone();
    let catalog_ops: Arc<dyn CatalogOps> =
        Arc::new(SessionCatalogOps::new(system, session.temp_schema.clone()));
    (catalog, type_catalog, catalog_ops)
}

/// A catalog routine as `pg_proc` reports it.
///
/// A type that does not resolve is reported as OID 0 rather than dropping the
/// row: `pg_proc` should show every routine that exists, and PostgreSQL also
/// prints 0 for a type it cannot name.
fn catalog_routine(info: &FuncInfo) -> crabgresql_catalog::CatalogRoutine {
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

pub fn execute_statement(
    engine: &Arc<dyn TableEngine>,
    global_catalog: &Arc<GlobalCatalog>,
    txnmgr: &Arc<TransactionManager>,
    stmt: &ast::Statement,
    session: &mut Session,
    params: &BoundParams,
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
                return execute_create_table(engine, &type_catalog, create, session);
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
                return execute_drop_function(global_catalog, drop);
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
            ast::Statement::Set(set) => return apply_set(set, session),
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
            ast::Statement::CreateIndex(create) => {
                return execute_create_index(&catalog, txnmgr, session, create);
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
    // Draining is where a routine body actually runs, so it is also where a
    // `RAISE EXCEPTION` surfaces — it needs the same abort path `execute` has
    // above, or the statement's XID is never marked aborted and stays in the
    // in-flight set, pinning the snapshot horizon for the life of the process.
    let exec = if calls_routine {
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
                IsolationLevel::RepeatableRead | IsolationLevel::Serializable => active
                    .snapshot
                    .get_or_insert_with(|| txnmgr.snapshot())
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
    // Turn the decoded rows into an INSERT ... VALUES plan (each field parses via
    // its column's input function against the type catalog bound at prepare time).
    let logical = prepared.plan.build_insert(&prepared.catalog, rows)?;
    // A COPY is a write (read-only was rejected at prepare time); its context
    // carries a sequence handle so a `serial`/`nextval()` column default advances
    // the sequence and updates this session's currval/lastval, as INSERT does,
    // and the catalog snapshot bound at prepare time for the same reason.
    let read_only = read_only_active(session);
    let txn = build_txn(txnmgr, session, true);
    let (routines, command_counter) = statement_runtime(
        &prepared.engine,
        &prepared.catalog,
        &prepared.global,
        session,
    );
    let exec_ctx = session.exec_context_for_statement(
        engine,
        &prepared.catalog_ops,
        routines,
        Arc::clone(&command_counter),
        read_only,
    );
    let exec = match execute(crabgresql_planner::plan(logical), &exec_ctx, &txn) {
        Ok(exec) => exec,
        Err(e) => {
            let _ = finalize_statement(txnmgr, session, &txn, true, false, Some(&command_counter));
            return Err(e.into());
        }
    };
    // A column default can call a routine, whose body advances the counter, so
    // the block's command id has to be read back rather than merely bumped.
    finalize_statement(txnmgr, session, &txn, true, true, Some(&command_counter))?;
    match exec {
        Execution::Inserted(n) => Ok(n),
        _ => Err(PgError::new(
            sqlstate::INTERNAL_ERROR,
            "COPY produced an unexpected execution result",
        )),
    }
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
fn statement_takes_snapshot(stmt: &ast::Statement) -> bool {
    !matches!(
        stmt,
        ast::Statement::StartTransaction { .. }
            | ast::Statement::Commit { .. }
            | ast::Statement::Rollback { .. }
            | ast::Statement::Set(_)
            | ast::Statement::Reset(_)
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
    session.xact = Some(ActiveTxn::new(iso, read_only));
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
    let mut named: Vec<(String, Arc<dyn TableAm>)> =
        Vec::with_capacity(truncate.table_names.len());
    for target in &truncate.table_names {
        if target.only || target.has_asterisk {
            return Err(PgError::feature_not_supported(
                "TRUNCATE ONLY / descendant selection is not supported yet",
            ));
        }
        let name = object_name_to_table_name(&target.name)?;
        let table = engine.open_table(&name)?;
        reject_partitioned_parent(&table, &name)?;
        named.push((name, table));
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

    // Versions below the oldest running transaction's floor are dead to every
    // snapshot that can still be taken, so they are safe to reclaim.
    let oldest = txnmgr.snapshot().xmin;
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
        tracing::debug!(rows = flushed, "VACUUM flushed buffered rows to durable storage");
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
    let index_name = create
        .name
        .as_ref()
        .ok_or_else(|| PgError::syntax("CREATE INDEX requires an index name"))
        .and_then(object_name_to_table_name)?;
    let table_name = object_name_to_table_name(&create.table_name)?;
    if engine.index_name_exists("public", &table_name, &index_name) {
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
    let keys = simple_index_keys(table.schema(), &create.columns)?;
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
        validate_unique_index_build(&table, &index, &txn)?;
    }
    engine.create_index("public", &table_name, index)?;
    Ok(QueryResult::command("CREATE INDEX"))
}

fn validate_unique_index_build(
    table: &Arc<dyn TableAm>,
    index: &IndexMetadata,
    txn: &TxnContext,
) -> Result<(), PgError> {
    let schema = table.schema();
    let mut seen: Vec<crabgresql_storage_api::Tuple> = Vec::new();
    // Only the index's own key columns are ever read below — for the duplicate
    // check, the error DETAIL and the `seen` comparisons alike.
    let projection = crabgresql_storage_api::ColumnProjection::of(
        index.keys.iter().map(|key| key.column),
        schema,
    );
    for row in table.scan(txn, &projection) {
        let (_, tuple) = row?;
        if index.nulls_distinct
            && index
                .keys
                .iter()
                .any(|key| matches!(tuple[key.column], crabgresql_types::Value::Null))
        {
            continue;
        }
        if seen
            .iter()
            .any(|other| index_rows_equal(schema, index, &tuple, other))
        {
            let names = index
                .keys
                .iter()
                .map(|key| schema.columns[key.column].name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let values = index
                .keys
                .iter()
                .map(|key| {
                    tuple[key.column]
                        .encode_text()
                        .unwrap_or_else(|| "null".to_string())
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Err(PgError::new(
                "23505",
                format!("could not create unique index \"{}\"", index.name),
            )
            .with_detail(format!("Key ({names})=({values}) is duplicated.")));
        }
        seen.push(tuple);
    }
    Ok(())
}

fn index_rows_equal(
    schema: &TableSchema,
    index: &IndexMetadata,
    left: &[crabgresql_types::Value],
    right: &[crabgresql_types::Value],
) -> bool {
    index.keys.iter().all(|key| {
        let left = &left[key.column];
        let right = &right[key.column];
        match (left, right) {
            (crabgresql_types::Value::Null, crabgresql_types::Value::Null) => true,
            (crabgresql_types::Value::Null, _) | (_, crabgresql_types::Value::Null) => false,
            _ => crabgresql_executor::compare_values(schema.columns[key.column].ty, left, right)
                .is_eq(),
        }
    })
}

/// `SET`: honors `extra_float_digits`, `default_transaction_isolation`,
/// `default_transaction_read_only`, and the `SET TRANSACTION` family; other GUCs
/// are accepted and ignored (driver compatibility), as before.
fn apply_set(set: &ast::Set, session: &mut Session) -> Result<QueryResult, PgError> {
    match set {
        ast::Set::SetTransaction {
            modes,
            snapshot,
            session: is_session,
        } => {
            return apply_set_transaction(session, modes, snapshot.is_some(), *is_session);
        }
        ast::Set::SingleAssignment {
            variable, values, ..
        } => match single_ident_lower(variable).as_deref() {
            // `SET x = DEFAULT` restores each GUC's boot value (PG accepts it and
            // resets rather than erroring on the "DEFAULT" token).
            Some("extra_float_digits") => {
                session.extra_float_digits = if is_set_default(values) {
                    1
                } else {
                    let v = set_value_to_i32(values)?;
                    if !(-15..=3).contains(&v) {
                        return Err(PgError::new(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            format!(
                                "{v} is outside the valid range for parameter \"extra_float_digits\" (-15 .. 3)"
                            ),
                        ));
                    }
                    v
                };
            }
            Some("default_transaction_isolation") => {
                session.default_iso = if is_set_default(values) {
                    IsolationLevel::ReadCommitted
                } else {
                    parse_default_isolation(values)?
                };
            }
            Some("default_transaction_read_only") => {
                session.default_read_only = if is_set_default(values) {
                    false
                } else {
                    parse_set_bool(values, "default_transaction_read_only")?
                };
            }
            _ => {}
        },
        _ => {}
    }
    Ok(QueryResult::command("SET"))
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
        if let Some(iso) = iso {
            session.default_iso = iso;
        }
        if let Some(read_only) = read_only {
            session.default_read_only = read_only;
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

/// Parse the value of `default_transaction_isolation`. Accepts PG's spellings
/// (`read committed`/`repeatable read`/`serializable`, `read uncommitted` as an
/// alias); an unrecognized value is an invalid-parameter error (22023).
fn parse_default_isolation(values: &[ast::Expr]) -> Result<IsolationLevel, PgError> {
    let raw = set_value_to_string(values);
    match raw.as_deref().map(str::trim).map(str::to_ascii_lowercase) {
        Some(ref s) if s == "read uncommitted" || s == "read committed" => {
            Ok(IsolationLevel::ReadCommitted)
        }
        Some(ref s) if s == "repeatable read" => Ok(IsolationLevel::RepeatableRead),
        Some(ref s) if s == "serializable" => Ok(IsolationLevel::Serializable),
        _ => Err(PgError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!(
                "invalid value for parameter \"default_transaction_isolation\": \"{}\"",
                raw.unwrap_or_default()
            ),
        )),
    }
}

/// Parse a Boolean GUC value using PG's boolean spellings (any unambiguous
/// prefix of true/false/yes/no/off, `on`, `1`/`0`). An unrecognized value is an
/// invalid-parameter error (22023).
fn parse_set_bool(values: &[ast::Expr], param: &str) -> Result<bool, PgError> {
    set_value_to_string(values)
        .as_deref()
        .and_then(crabgresql_types::parse_bool)
        .ok_or_else(|| {
            PgError::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                format!("parameter \"{param}\" requires a Boolean value"),
            )
        })
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
            ast::Value::SingleQuotedString(s) => Some(s.clone()),
            ast::Value::DollarQuotedString(d) => Some(d.value.clone()),
            ast::Value::Number(n, _) => Some(n.clone()),
            ast::Value::Boolean(b) => Some(b.to_string()),
            _ => None,
        },
        ast::Expr::Identifier(ident) => Some(ident.value.clone()),
        _ => None,
    }
}

/// `RESET <param>` / `RESET ALL` restore the session defaults:
/// `extra_float_digits`=1, `default_transaction_isolation`=READ COMMITTED,
/// `default_transaction_read_only`=off.
fn apply_reset(reset: &ast::ResetStatement, session: &mut Session) -> Result<QueryResult, PgError> {
    let (all, name) = match &reset.reset {
        ast::Reset::ALL => (true, None),
        ast::Reset::ConfigurationParameter(name) => (false, single_ident_lower(name)),
    };
    let hit = |param: &str| all || name.as_deref() == Some(param);
    if hit("extra_float_digits") {
        session.extra_float_digits = 1;
    }
    if hit("default_transaction_isolation") {
        session.default_iso = IsolationLevel::ReadCommitted;
    }
    if hit("default_transaction_read_only") {
        session.default_read_only = false;
    }
    Ok(QueryResult::command("RESET"))
}

/// A single-part object name, lowercased (GUC names are case-insensitive).
fn single_ident_lower(name: &ast::ObjectName) -> Option<String> {
    if name.0.len() != 1 {
        return None;
    }
    name.0[0].as_ident().map(normalize_ident)
}

fn set_value_to_i32(exprs: &[ast::Expr]) -> Result<i32, PgError> {
    let [expr] = exprs else {
        return Err(PgError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "parameter \"extra_float_digits\" requires an integer value",
        ));
    };
    parse_i32_expr(expr).ok_or_else(|| {
        PgError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "parameter \"extra_float_digits\" requires an integer value",
        )
    })
}

fn parse_i32_expr(expr: &ast::Expr) -> Option<i32> {
    match expr {
        ast::Expr::Value(v) => match &v.value {
            ast::Value::Number(n, _) => n.parse().ok(),
            ast::Value::SingleQuotedString(s) => s.trim().parse().ok(),
            _ => None,
        },
        ast::Expr::UnaryOp {
            op: ast::UnaryOperator::Minus,
            expr,
        } => parse_i32_expr(expr).map(|v| -v),
        ast::Expr::UnaryOp {
            op: ast::UnaryOperator::Plus,
            expr,
        } => parse_i32_expr(expr),
        _ => None,
    }
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

fn create_table_access_method(
    create: &ast::CreateTable,
) -> Result<TableAccessMethod, PgError> {
    if create.external {
        return Err(PgError::feature_not_supported(
            "external tables are not supported",
        ));
    }
    let Some(format) = &create.hive_formats else {
        return Ok(TableAccessMethod::Heap);
    };
    if format.row_format.is_some()
        || format.serde_properties.is_some()
        || format.location.is_some()
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

fn execute_create_table(
    engine: &Arc<dyn TableEngine>,
    type_catalog: &Arc<dyn TypeCatalog>,
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
    // Table inheritance (`INHERITS (...)`) is a distinct feature from declarative
    // partitioning and is not implemented; reject rather than silently ignore.
    if create.inherits.is_some() {
        return Err(PgError::feature_not_supported(
            "CREATE TABLE ... INHERITS is not supported yet",
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
        let kind = if create.temporary { "temporary" } else { "unlogged" };
        return Err(PgError::feature_not_supported(format!(
            "{kind} partitioned tables are not supported yet"
        )));
    }
    // A leaf partition (`PARTITION OF parent`) inherits the parent's columns and
    // is created as an ordinary heap table carrying its bound; handle it whole.
    if let Some(parent) = &create.partition_of {
        return execute_create_partition(engine, type_catalog, create, &namespace, &name, parent);
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

    let mut columns = Vec::new();
    let mut pending = Vec::<PendingIndex>::new();
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
        // Checked, not bare `length_typmod`: an out-of-range length would be
        // stored on the column and later overflow `pg_attribute.atttypmod`.
        let typmod = crabgresql_binder::checked_length_typmod(&col.data_type)?.unwrap_or(-1);
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
            column.default = Some(format!("nextval('{seq_ref}')"));
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
                                    &constraint_names,
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
                    crabgresql_binder::bind_column_default(expr, &column, type_catalog)?;
                    column.default = Some(expr.to_string());
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
            Some(build_partition_scheme(expr, &columns)?)
        }
        None => None,
    };

    let schema = TableSchema {
        name: name.clone(),
        namespace: namespace.clone(),
        columns,
        persistence,
        access_method,
        partition_scheme,
        partition_of: None,
    };
    let mut indexes = Vec::new();
    for p in pending {
        reject_deferred_characteristics(p.characteristics)?;
        let keys = simple_index_keys(&schema, &p.columns)?;
        // PG names a UNIQUE constraint after every key column, e.g.
        // `t_a_b_key`; only PRIMARY KEY collapses to `t_pkey`.
        let base = match p.constraint {
            IndexConstraint::PrimaryKey => format!("{name}_pkey"),
            IndexConstraint::Unique => {
                let mut base = name.clone();
                for key in &keys {
                    base.push('_');
                    base.push_str(&schema.columns[key.column].name);
                }
                base.push_str("_key");
                base
            }
        };
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
        // PG succeeds with a notice; NoticeResponse itself is still todo. The
        // serial sequences we just created would be orphaned, so drop them.
        Err(StorageError::TableAlreadyExists(_)) if create.if_not_exists => {
            drop_created_sequences(target, &serial_defs);
        }
        Err(e) => {
            drop_created_sequences(target, &serial_defs);
            return Err(e.into());
        }
    }
    Ok(QueryResult::command("CREATE TABLE"))
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
    let Some(idx) = columns.iter().position(|c| c.name == key) else {
        return Err(PgError::new(
            sqlstate::UNDEFINED_COLUMN,
            format!("column \"{key}\" named in partition key does not exist"),
        ));
    };
    // A RANGE key must be a btree-orderable type; otherwise bound comparison
    // would later panic in `compare_values`. PG rejects the same at parent create.
    // (A user type can only reach here as an enum — non-enum user types are
    // rejected as column types upstream — and enums are orderable.)
    if !crabgresql_executor::is_orderable(columns[idx].ty) {
        return Err(PgError::new(
            sqlstate::UNDEFINED_OBJECT,
            format!(
                "data type {} has no default operator class for access method \"btree\"",
                columns[idx].ty.name()
            ),
        ));
    }
    Ok(PartitionScheme {
        strategy: PartitionStrategy::Range,
        key_columns: vec![idx],
    })
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
        (Endpoint::Finite(x), Endpoint::Finite(y)) => {
            crabgresql_executor::compare_values(ty, x, y)
        }
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
) -> Result<Value, PgError> {
    let bound = crabgresql_binder::bind_column_default(expr, key_col, type_catalog)?;
    Ok(crabgresql_executor::eval::eval(
        &bound,
        &[],
        &ExecContext::default(),
    )?)
}

/// Convert one incoming `FOR VALUES` datum into its storage form plus its
/// ordered [`Endpoint`]. A finite bound is folded to a typed [`Value`] once,
/// here, and stored as-is — no text round-trip.
fn incoming_endpoint(
    value: &ast::PartitionBoundValue,
    key_col: &Column,
    type_catalog: &Arc<dyn TypeCatalog>,
) -> Result<(PartitionBoundDatum, Endpoint), PgError> {
    match value {
        ast::PartitionBoundValue::MinValue => Ok((PartitionBoundDatum::MinValue, Endpoint::NegInf)),
        ast::PartitionBoundValue::MaxValue => Ok((PartitionBoundDatum::MaxValue, Endpoint::PosInf)),
        ast::PartitionBoundValue::Expr(expr) => {
            let value = fold_bound_value(expr, key_col, type_catalog)?;
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
    let (parent_qual, parent_name) = split_object_name(parent_ref, "relation")?;
    let parent = engine
        .resolve(parent_qual.as_deref(), &parent_name)
        .map_err(|_| {
            // A view or sequence of that name exists but is not a table: PG reports
            // wrong-object-type, not "does not exist".
            let is_non_table_relation = engine
                .resolve_view(parent_qual.as_deref(), &parent_name)
                .is_some()
                || engine
                    .sequence(parent_qual.as_deref().unwrap_or("public"), &parent_name)
                    .is_some();
            if is_non_table_relation {
                PgError::new(
                    sqlstate::WRONG_OBJECT_TYPE,
                    format!(
                        "inherited relation \"{parent_name}\" is not a table or foreign table"
                    ),
                )
            } else {
                PgError::new(
                    sqlstate::UNDEFINED_TABLE,
                    format!("relation \"{parent_name}\" does not exist"),
                )
            }
        })?;
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
    let (from_datum, lower) = incoming_endpoint(&from_spec[0], key_col, type_catalog)?;
    let (to_datum, upper) = incoming_endpoint(&to_spec[0], key_col, type_catalog)?;
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
    if create.or_replace
        || create.on_commit.is_some()
        || !create.constraints.is_empty()
        || create.like.is_some()
        || create.clone.is_some()
        || !matches!(create.table_options, ast::CreateTableOptions::None)
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
    let schema = TableSchema {
        name: name.clone(),
        namespace: namespace.clone(),
        columns: cols
            .iter()
            .map(|c| {
                let mut col = Column::new(c.name.clone(), c.ty);
                col.collation = c.collation;
                col
            })
            .collect(),
        persistence,
        access_method,
        partition_scheme: None,
        partition_of: None,
    };

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

fn reject_primary_key_options(constraint: &ast::PrimaryKeyConstraint) -> Result<(), PgError> {
    if constraint.index_name.is_some()
        || !constraint.index_options.is_empty()
        || !matches!(constraint.index_type, None | Some(ast::IndexType::BTree))
    {
        return Err(PgError::feature_not_supported(
            "this PRIMARY KEY index form is not supported yet",
        ));
    }
    Ok(())
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
    Ok(())
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
        let column = schema.column_index(&ident).ok_or_else(|| {
            PgError::new(
                sqlstate::UNDEFINED_COLUMN,
                format!("column \"{ident}\" named in key does not exist"),
            )
        })?;
        // A B-tree / UNIQUE index (and PRIMARY KEY) needs an ordering. Types with
        // no default B-tree operator class (`json`, `point`, `lseg`) are rejected
        // here, matching PostgreSQL — otherwise unique enforcement would later
        // call `compare_values` on an unorderable type and panic the backend.
        let ty = schema.columns[column].ty;
        if !ty.has_default_btree_opclass() {
            return Err(PgError::new(
                sqlstate::UNDEFINED_OBJECT,
                format!(
                    "data type {} has no default operator class for access method \"btree\"",
                    ty.name()
                ),
            )
            .with_hint(
                "You must specify an operator class for the index or define a \
                 default operator class for the data type.",
            ));
        }
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

fn fresh_local_name(local: &HashSet<String>, base: &str) -> String {
    if !local.contains(base) {
        return base.to_string();
    }
    for suffix in 1_u64.. {
        let candidate = format!("{base}{suffix}");
        if !local.contains(&candidate) {
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
                let n = match name.0.as_slice() {
                    [part] => part.as_ident().map(normalize_ident),
                    _ => None,
                };
                if let Some(t) = n.as_deref().and_then(builtin_type_by_name) {
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
        let display = builtin_type_by_name(name).map_or_else(|| name.to_string(), |t| t.name().to_string());
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

/// Extract a single-quoted (or dollar-quoted) string literal expression.
fn string_literal(expr: &ast::Expr) -> Result<String, PgError> {
    match expr {
        ast::Expr::Value(v) => match &v.value {
            ast::Value::SingleQuotedString(s) => Ok(s.clone()),
            ast::Value::DollarQuotedString(d) => Ok(d.value.clone()),
            other => Err(PgError::syntax(format!(
                "expected a string literal, found: {other}"
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
                &arg_types,
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
            let arg_names: Vec<Option<String>> = args
                .iter()
                .filter(|a| a.mode.is_input())
                .map(|a| a.name.clone())
                .collect();
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

    let view_columns: Vec<Column> = columns
        .iter()
        .map(|c| Column::new(c.name.clone(), c.ty))
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
    names.into_iter().filter(|n| seen.insert(n.clone())).collect()
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

fn collect_setexpr_relations(body: &ast::SetExpr, names: &mut Vec<String>, scope: &mut Vec<String>) {
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
            if let Some(rel) = parts.last().and_then(|part| part.as_ident()).map(normalize_ident) {
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
    let (dependent_views, mut cascade_notices) =
        plan_view_cascade(catalog, "table", &all_targets, cascade)?;
    notices.append(&mut cascade_notices);
    for name in &plain {
        catalog.drop_table("public", name)?;
    }
    for (ns, name) in &qualified {
        catalog.drop_table(ns, name)?;
    }
    for (ns, view) in &dependent_views {
        catalog.drop_view(ns, view)?;
    }
    // Auto-drop sequences a dropped table owns (a `serial` column's sequence, via
    // PG's OWNED BY). PG removes these silently, without a cascade notice.
    for seq in catalog.sequences() {
        let owned_by_dropped = seq.owned_by.as_deref().is_some_and(|owner| {
            (seq.namespace == "public" && plain.iter().any(|t| t == owner))
                || qualified
                    .iter()
                    .any(|(ns, t)| *ns == seq.namespace && t == owner)
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
    let (dependent_views, mut cascade_notices) =
        plan_view_cascade(catalog, "view", &targets, cascade)?;
    notices.append(&mut cascade_notices);
    for name in &to_drop {
        catalog.drop_view("public", name)?;
    }
    for (ns, view) in &dependent_views {
        catalog.drop_view(ns, view)?;
    }
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
            ast::Value::Number(n, _) => n.parse().ok(),
            ast::Value::SingleQuotedString(s) => s.trim().parse().ok(),
            _ => None,
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

        if !contents.is_empty() && !cascade {
            // RESTRICT (the default): refuse to drop a non-empty schema.
            let detail = contents
                .iter()
                .map(|(kind, obj)| format!("{kind} {name}.{obj} depends on schema {name}"))
                .collect::<Vec<_>>()
                .join("\n");
            return Err(PgError::new(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("cannot drop schema {name} because other objects depend on it"),
            )
            .with_detail(detail)
            .with_hint("Use DROP ... CASCADE to drop the dependent objects too."));
        }
        if !contents.is_empty() {
            // CASCADE: mirror PG's `drop cascades to ...` NOTICE, then drop each
            // contained object in the target namespace.
            let lines: Vec<String> = contents
                .iter()
                .map(|(kind, obj)| format!("drop cascades to {kind} {name}.{obj}"))
                .collect();
            let notice = if contents.len() == 1 {
                Notice::notice(lines[0].clone(), None)
            } else {
                Notice::notice(
                    format!("drop cascades to {} other objects", contents.len()),
                    Some(lines.join("\n")),
                )
            };
            notices.push(notice);
            for (kind, obj) in &contents {
                let _ = match *kind {
                    "table" => engine.drop_table(name, obj),
                    "view" => engine.drop_view(name, obj),
                    _ => engine.drop_sequence(name, obj),
                };
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

/// Resolve the views that depend on a set of relations being dropped. `targets`
/// are the `(namespace, name)` relations being removed, all of object class
/// `target_noun` (`"table"` or `"view"`). Dependencies are matched on the
/// qualified `"namespace.name"` key, so a view in any schema that reads a dropped
/// relation is found (cross-schema included). Under RESTRICT (`!cascade`) any
/// dependent is an error (2BP01, with a DETAIL line per dependency edge). Under
/// CASCADE it returns the transitive set of dependent views to drop (each as
/// `(namespace, name)`), in discovery order, plus the `drop cascades to ...`
/// NOTICE(s) — matching the wording of `DROP TYPE ... CASCADE`.
/// A relation as `(namespace, name)` — the key `plan_view_cascade` matches drop
/// targets and dependent views by.
type QualifiedRelation = (String, String);

fn plan_view_cascade(
    catalog: &Arc<dyn TableEngine>,
    target_noun: &str,
    targets: &[QualifiedRelation],
    cascade: bool,
) -> Result<(Vec<QualifiedRelation>, Vec<Notice>), PgError> {
    let all_views = catalog.views();
    let key = |ns: &str, name: &str| format!("{ns}.{name}");
    // PG omits the `public.` prefix in dependency messages; keep a bare display
    // for public objects and qualify the rest.
    let disp = |ns: &str, name: &str| {
        if ns == "public" {
            name.to_string()
        } else {
            format!("{ns}.{name}")
        }
    };
    let target_keys: Vec<String> = targets.iter().map(|(ns, n)| key(ns, n)).collect();

    if !cascade {
        // RESTRICT: report every (dependent view, target) edge as a DETAIL line,
        // in target order then view order, as PG does.
        let mut detail = Vec::new();
        let mut first_blocked: Option<String> = None;
        for (tns, tn) in targets {
            let tkey = key(tns, tn);
            for view in &all_views {
                if target_keys.contains(&key(&view.namespace, &view.name)) {
                    continue;
                }
                if view.depends_on.iter().any(|d| d == &tkey) {
                    detail.push(format!(
                        "view {} depends on {target_noun} {}",
                        disp(&view.namespace, &view.name),
                        disp(tns, tn)
                    ));
                    first_blocked.get_or_insert_with(|| disp(tns, tn));
                }
            }
        }
        if let Some(blocked) = first_blocked {
            return Err(PgError::new(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("cannot drop {target_noun} {blocked} because other objects depend on it"),
            )
            .with_detail(detail.join("\n"))
            .with_hint("Use DROP ... CASCADE to drop the dependent objects too."));
        }
        return Ok((Vec::new(), Vec::new()));
    }

    // CASCADE: breadth-first transitive closure of dependent views. A view is
    // pulled in when its `depends_on` names anything already being dropped.
    let mut removed: Vec<String> = target_keys;
    let mut dependents: Vec<(String, String)> = Vec::new();
    loop {
        let mut added = false;
        for view in &all_views {
            let vkey = key(&view.namespace, &view.name);
            if removed.contains(&vkey) {
                continue;
            }
            if view.depends_on.iter().any(|d| removed.contains(d)) {
                removed.push(vkey);
                dependents.push((view.namespace.clone(), view.name.clone()));
                added = true;
            }
        }
        if !added {
            break;
        }
    }

    let notices = match dependents.as_slice() {
        [] => Vec::new(),
        [(ns, name)] => vec![Notice::notice(
            format!("drop cascades to view {}", disp(ns, name)),
            None,
        )],
        many => {
            let detail = many
                .iter()
                .map(|(ns, name)| format!("drop cascades to view {}", disp(ns, name)))
                .collect::<Vec<_>>()
                .join("\n");
            vec![Notice::notice(
                format!("drop cascades to {} other objects", many.len()),
                Some(detail),
            )]
        }
    };
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
    drop: &ast::DropFunction,
) -> Result<QueryResult, PgError> {
    execute_drop_routine(
        catalog,
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
    // name and its signature — is rejected in drop_functions once each target is
    // resolved to a concrete routine.
    let cascade = matches!(drop_behavior, Some(ast::DropBehavior::Cascade));
    let notices = catalog.drop_functions(&specs, kind, cascade, if_exists)?;
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
        assert_eq!(deps("WITH c AS (SELECT 1) SELECT * FROM c"), Vec::<String>::new());
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
}
