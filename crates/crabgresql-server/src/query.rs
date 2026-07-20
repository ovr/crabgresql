//! Simple-query execution: AST → bind → plan → Volcano executor.
//!
//! DQL/DML statements run through the binder/planner pipeline. DDL
//! (CREATE TABLE) and session commands (SET) execute directly here until the
//! catalog and GUC store exist.

use std::collections::HashSet;
use std::sync::Arc;

use crabgresql_binder::{
    BoundExpr, LogicalPlan, bind_delete_with_params, bind_insert_with_params,
    bind_query_with_params, bind_update_with_params, output_columns_of, param_ctx_extended,
    param_ctx_none, param_types, require_all_resolved, substitute_params,
};
use crabgresql_executor::{DmlVerb, ExecNode, Execution, OutputColumn, Values, execute};
use crabgresql_parser::ast;
use crabgresql_pg_wire::{TransactionStatus, sqlstate};
use crabgresql_storage_api::{
    Column, IndexConstraint, IndexKey, IndexMetadata, IndexMethod, SequenceDefinition, StorageError,
    TableAm, TableEngine, TableSchema, TypeCatalog, ViewDefinition,
};
use crabgresql_txn::{CommandId, IsolationLevel, TransactionManager, TxnContext, Xid};
use crabgresql_types::{PgType, Value};

use crate::catalog::SessionCatalog;
use crate::error::PgError;
use crate::global_catalog::{CatalogNotice, GlobalCatalog, TypeRef};
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
    pub code: &'static str,
    pub message: String,
    pub detail: Option<String>,
    /// 1-based (line, column) of the token this NOTICE points at, when PG
    /// renders a `LINE n:` cursor excerpt. Converted to a wire character offset
    /// when the NOTICE is sent.
    pub location: Option<(u64, u64)>,
}

impl Notice {
    /// A `WARNING`-severity message (no DETAIL), as used by transaction control.
    fn warning(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: NoticeSeverity::Warning,
            code,
            message: message.into(),
            detail: None,
            location: None,
        }
    }

    /// A `NOTICE`-severity message with an optional DETAIL line.
    #[allow(clippy::self_named_constructors)]
    fn notice(message: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            severity: NoticeSeverity::Notice,
            // psql does not print the SQLSTATE of a NOTICE; PG uses
            // successful_completion (00000) for these.
            code: "00000",
            message: message.into(),
            detail,
            location: None,
        }
    }
}

pub enum QueryResult {
    /// A result set, streamed: the caller pulls tuples from the node and derives
    /// the CommandComplete tag from `tag` and the row count.
    Rows {
        columns: Vec<OutputColumn>,
        node: Box<dyn ExecNode>,
        /// Which command tag to report once the rows are drained. A plain SELECT
        /// (and EXPLAIN) uses `SELECT n`; a `RETURNING` DML keeps its mutation
        /// tag (`INSERT 0 n` / `UPDATE n` / `DELETE n`).
        tag: RowTag,
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
}

impl RowTag {
    /// The CommandComplete tag for `count` streamed rows.
    pub fn complete(self, count: usize) -> String {
        match self {
            RowTag::Select => format!("SELECT {count}"),
            RowTag::Insert => format!("INSERT 0 {count}"),
            RowTag::Update => format!("UPDATE {count}"),
            RowTag::Delete => format!("DELETE {count}"),
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

/// Build the binder's two catalog views for this session: the `pg_temp`-first
/// relation overlay (temp shadows permanent, both behind the read-only
/// `pg_catalog`) and the user-type/cast view. Shared by `execute_statement` and
/// `analyze_statement` so binding sees identical name resolution either way.
fn bind_catalogs(
    engine: &Arc<dyn TableEngine>,
    global_catalog: &Arc<GlobalCatalog>,
    session: &Session,
) -> (Arc<dyn TableEngine>, Arc<dyn TypeCatalog>) {
    // The read-only system catalog (`pg_catalog`), rebuilt per statement so its
    // rows reflect current server state: live user relations (permanent + this
    // session's temp tables) are reflected into pg_class/pg_attribute. The
    // relation enumeration is lazy — only a query that actually opens
    // pg_class/pg_attribute pays for it. It sits behind temp + global on the
    // search path.
    let system: Arc<dyn TableEngine> = {
        let global = engine.clone();
        let temp = session.temp.clone();
        let database = session.database.clone();
        let owner = session.user.clone();
        let temp_schema = session.temp_schema.clone();
        // User-defined types are reflected into pg_type/pg_enum on demand.
        let types = global_catalog.clone();
        Arc::new(
            crabgresql_catalog::SystemCatalog::with_catalog_relations_fn(
                database,
                owner,
                move || {
                    let mut rels: Vec<_> = global
                        .relation_metadata()
                        .into_iter()
                        .map(crabgresql_catalog::CatalogRelation::permanent_metadata)
                        .collect();
                    rels.extend(temp.relation_metadata().into_iter().map(|metadata| {
                        let mut relation = crabgresql_catalog::CatalogRelation::temporary(
                            metadata.schema,
                            temp_schema.clone(),
                        );
                        relation.indexes = metadata.indexes;
                        relation
                    }));
                    // Views reflect into pg_class as relkind='v' / pg_attribute
                    // columns / information_schema.tables as VIEW.
                    rels.extend(global.views().into_iter().map(|view| {
                        crabgresql_catalog::CatalogRelation::view(TableSchema {
                            name: view.name,
                            columns: view.columns,
                        })
                    }));
                    // Sequences reflect into pg_class as relkind='S' and feed
                    // pg_catalog.pg_sequence.
                    rels.extend(global.sequences().into_iter().map(|seq| {
                        crabgresql_catalog::CatalogRelation::sequence(
                            seq.name,
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
            }),
        )
    };
    let catalog: Arc<dyn TableEngine> = Arc::new(SessionCatalog::new(
        session.temp.clone(),
        engine.clone(),
        system,
        session.temp_schema.clone(),
    ));
    // The global catalog is the binder's view of user-defined types and casts,
    // so an expression can cast to/from a `CREATE TYPE` name.
    let type_catalog: Arc<dyn TypeCatalog> = global_catalog.clone();
    (catalog, type_catalog)
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
    let (catalog, type_catalog) = bind_catalogs(engine, global_catalog, session);
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
        ast::Statement::Explain { statement, .. } => {
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
                result_columns: Some(vec![OutputColumn {
                    name: "QUERY PLAN".to_string(),
                    ty: PgType::Text,
                }]),
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
    // Resolution overlay: the session's temp catalog shadows the shared global
    // engine (PG's `pg_temp`-first search). CREATE routes temp vs global itself,
    // so it keeps the raw engine + session below.
    let (catalog, type_catalog) = bind_catalogs(engine, global_catalog, session);
    let logical = match bind_dml_with_params(&catalog, &type_catalog, stmt, params)? {
        Some(logical) => logical,
        // Not DQL/DML: fall through to the utility-statement handlers below.
        None => match stmt {
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
                return execute_create_function(global_catalog, create);
            }
            ast::Statement::CreateCast {
                source,
                target,
                method,
                ..
            } => return execute_create_cast(global_catalog, source, target, method),
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
            ast::Statement::CreateIndex(create) => {
                return execute_create_index(&catalog, txnmgr, session, create);
            }
            ast::Statement::Explain {
                analyze, statement, ..
            } => {
                // Plain `EXPLAIN <stmt>` only: ANALYZE would run the statement,
                // which this reduced EXPLAIN does not do. VERBOSE/FORMAT are
                // ignored.
                if *analyze {
                    return Err(PgError::feature_not_supported(
                        "EXPLAIN ANALYZE is not supported yet",
                    ));
                }
                let Some(inner) =
                    bind_dml_with_params(&catalog, &type_catalog, statement, params)?
                else {
                    return Err(PgError::feature_not_supported(format!(
                        "EXPLAIN of {} is not supported yet",
                        statement_kind(statement)
                    )));
                };
                let plan = crabgresql_planner::plan(inner);
                let rows: Vec<Vec<BoundExpr>> = crabgresql_planner::explain(&plan)
                    .into_iter()
                    .map(|line| {
                        vec![BoundExpr::Const {
                            value: Value::Text(line),
                            ty: PgType::Text,
                        }]
                    })
                    .collect();
                let node: Box<dyn ExecNode> =
                    Box::new(Values::new(rows, session.exec_context()));
                return Ok(QueryResult::Rows {
                    columns: vec![OutputColumn {
                        name: "QUERY PLAN".to_string(),
                        ty: PgType::Text,
                    }],
                    node,
                    tag: RowTag::Select,
                });
            }
            other => {
                return Err(PgError::feature_not_supported(format!(
                    "statement is not supported yet: {}",
                    statement_kind(other)
                )));
            }
        },
    };
    // A write statement needs an XID to stamp its versions; a read runs with
    // none. Decide from the bound plan, not the surface AST: the binder already
    // resolved the statement to an Insert/Update/Delete node, so a new writing
    // statement kind can't accidentally run XID-less and produce invisible rows.
    let is_write = matches!(
        logical,
        LogicalPlan::Insert { .. } | LogicalPlan::Update { .. } | LogicalPlan::Delete { .. }
    );
    // A write in a READ ONLY transaction (or under the read-only session default)
    // is rejected before it stamps any version, matching PG's 25006.
    if is_write && read_only_active(session) {
        let verb = match logical {
            LogicalPlan::Insert { .. } => "INSERT",
            LogicalPlan::Update { .. } => "UPDATE",
            LogicalPlan::Delete { .. } => "DELETE",
            _ => unreachable!("is_write implies a DML plan"),
        };
        return Err(PgError::new(
            sqlstate::READ_ONLY_SQL_TRANSACTION,
            format!("cannot execute {verb} in a read-only transaction"),
        ));
    }
    let read_only = read_only_active(session);
    let txn = build_txn(txnmgr, session, is_write);
    // Sequence functions (`nextval` in a `serial` default or written explicitly)
    // advance non-transactional counters in the shared engine and update this
    // session's `currval`/`lastval`, so the execution context carries a handle to
    // both. Sequences resolve against the global engine (temp sequences are
    // unsupported), matching temp-view handling. The read-only flag lets a bare
    // `SELECT nextval(...)` be rejected (25006) even though it is not a DML write.
    let exec_ctx = session.exec_context_with_sequences(engine, read_only);
    let exec = match execute(crabgresql_planner::plan(logical), &exec_ctx, &txn) {
        Ok(exec) => exec,
        Err(e) => {
            // Abort path: infallible, so the result is safe to drop.
            let _ = finalize_statement(txnmgr, session, &txn, is_write, false);
            return Err(e.into());
        }
    };
    finalize_statement(txnmgr, session, &txn, is_write, true)?;
    let result = match exec {
        Execution::Rows { columns, node } => QueryResult::Rows {
            columns,
            node,
            tag: RowTag::Select,
        },
        Execution::ReturningRows {
            columns,
            node,
            verb,
        } => QueryResult::Rows {
            columns,
            node,
            tag: verb.into(),
        },
        Execution::Inserted(n) => QueryResult::command(format!("INSERT 0 {n}")),
        Execution::Updated(n) => QueryResult::command(format!("UPDATE {n}")),
        Execution::Deleted(n) => QueryResult::command(format!("DELETE {n}")),
    };
    Ok(result)
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
            active.cid = CommandId(active.cid.0 + 1);
        }
    }
    Ok(())
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
        ast::Statement::Truncate(_) => "TRUNCATE TABLE",
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
        named.push((name, table));
    }
    // Acquire the tables' exclusive locks in a deterministic order (by name), so
    // two concurrent multi-table TRUNCATEs can never deadlock, and drop duplicates
    // named twice in one statement.
    named.sort_by(|a, b| a.0.cmp(&b.0));
    named.dedup_by(|a, b| a.0 == b.0);
    // TRUNCATE is a write: run it under a real transaction so autocommit commits
    // it. On the durable heap engine this is fully transactional — the swap is
    // applied on commit and discarded on rollback (or a crash before commit).
    let txn = build_txn(txnmgr, session, true);
    for (_, table) in &named {
        table.truncate(&txn);
    }
    finalize_statement(txnmgr, session, &txn, true, true)?;
    Ok(QueryResult::command("TRUNCATE TABLE"))
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
    if engine.index_name_exists(&table_name, &index_name) {
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
    engine.create_index(&table_name, index)?;
    Ok(QueryResult::command("CREATE INDEX"))
}

fn validate_unique_index_build(
    table: &Arc<dyn TableAm>,
    index: &IndexMetadata,
    txn: &TxnContext,
) -> Result<(), PgError> {
    let schema = table.schema();
    let mut seen: Vec<crabgresql_storage_api::Tuple> = Vec::new();
    for (_, tuple) in table.scan(txn) {
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
fn normalize_ident(ident: &ast::Ident) -> String {
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

fn execute_create_table(
    engine: &Arc<dyn TableEngine>,
    type_catalog: &Arc<dyn TypeCatalog>,
    create: &ast::CreateTable,
    session: &Session,
) -> Result<QueryResult, PgError> {
    let name = object_name_to_table_name(&create.name)?;
    let target = if create.temporary {
        &session.temp
    } else {
        engine
    };
    // Clauses we can't honor must be rejected, not silently dropped: CREATE
    // TABLE AS would otherwise create an empty table (the SELECT is discarded),
    // and ON COMMIT DROP/DELETE ROWS needs the M2 transaction engine — accepting
    // it would leave a plain session-lifetime table that diverges from PG.
    if create.query.is_some() {
        return Err(PgError::feature_not_supported(
            "CREATE TABLE ... AS is not supported yet",
        ));
    }
    if create.on_commit.is_some() {
        return Err(PgError::feature_not_supported(
            "CREATE TABLE ... ON COMMIT is not supported yet",
        ));
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
        let typmod = crabgresql_binder::length_typmod(&col.data_type).unwrap_or(-1);
        let mut column = Column::with_typmod(column_name.clone(), ty, typmod);
        if let Some(base) = serial_base {
            // Name the sequence `t_col_seq`, dodging existing relations and any
            // other serial sequences created earlier in this same statement.
            let taken: Vec<String> = serial_defs.iter().map(|d| d.name.clone()).collect();
            let seq_name =
                unique_relation_name(target, &taken, &format!("{name}_{column_name}_seq"));
            column.default = Some(format!("nextval('{seq_name}')"));
            serial_defs.push(SequenceDefinition {
                name: seq_name,
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

    let schema = TableSchema {
        name: name.clone(),
        columns,
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
            .unwrap_or_else(|| fresh_relation_name(target, &constraint_names, &base));
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
                if let Err(e) = target.create_index(&name, index) {
                    let _ = target.drop_table(&name);
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
    local: &HashSet<String>,
    base: &str,
) -> String {
    let exists = |candidate: &str| {
        local.contains(candidate)
            || engine.open_table(candidate).is_ok()
            || engine
                .relation_metadata()
                .iter()
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
/// than a second width table.
fn builtin_type_by_name(name: &str) -> Option<PgType> {
    Some(match name {
        "int8" | "bigint" => PgType::Int8,
        "int4" | "integer" | "int" => PgType::Int4,
        "int2" | "smallint" => PgType::Int2,
        "float8" | "double precision" => PgType::Float8,
        "float4" | "real" => PgType::Float4,
        "numeric" | "decimal" => PgType::Numeric,
        "bool" | "boolean" => PgType::Bool,
        "bytea" => PgType::Bytea,
        "text" => PgType::Text,
        "varchar" | "character varying" => PgType::Varchar,
        "bpchar" | "char" | "character" => PgType::Bpchar,
        "name" => PgType::Name,
        "oid" => PgType::Oid,
        "bit" => PgType::Bit,
        "varbit" | "bit varying" => PgType::Varbit,
        "date" => PgType::Date,
        "time" => PgType::Time,
        "timetz" => PgType::TimeTz,
        "timestamp" => PgType::Timestamp,
        "timestamptz" => PgType::TimestampTz,
        "interval" => PgType::Interval,
        "macaddr" => PgType::Macaddr,
        "macaddr8" => PgType::Macaddr8,
        "json" => PgType::Json,
        "jsonb" => PgType::Jsonb,
        "jsonpath" => PgType::Jsonpath,
        _ => return None,
    })
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

/// `CREATE FUNCTION ... LANGUAGE internal AS '<builtin>'`.
fn execute_create_function(
    catalog: &GlobalCatalog,
    create: &ast::CreateFunction,
) -> Result<QueryResult, PgError> {
    let lang = create
        .language
        .as_ref()
        .map(|i| i.value.to_ascii_lowercase());
    if lang.as_deref() != Some("internal") {
        return Err(PgError::feature_not_supported(
            "CREATE FUNCTION is only supported for LANGUAGE internal",
        ));
    }
    let internal_name = function_internal_name(create)?;
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
    let mut args = Vec::new();
    for arg in create.args.iter().flatten() {
        // An empty span (line 0) means the arg was built without source
        // location; only parsed, bare arguments carry a caret position.
        let start = arg.data_type_span.start;
        let position = (start.line != 0).then_some((start.line, start.column));
        args.push((resolve_type_ref(catalog, &arg.data_type)?, position));
    }
    let notices = catalog.create_function(&name, args, ret, &internal_name)?;
    Ok(QueryResult::Command {
        tag: "CREATE FUNCTION".into(),
        notices: to_notices(notices),
    })
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
    let name = object_name_to_table_name(&create.name)?;
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

    let existing_table = catalog.open_table(&name).is_ok();
    if create.or_replace {
        if existing_table {
            return Err(PgError::new(
                sqlstate::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not a view"),
            ));
        }
        if let Some(old) = catalog.resolve_view(None, &name) {
            check_view_replace_compatible(&old, &view_columns)?;
            // A replaced view may (transitively) reference itself; PG permits
            // creating such a view and only errors when it is used, so the
            // binder detects the cycle at expansion time rather than here.
            catalog.drop_view(&name)?;
        }
    } else if existing_table || catalog.resolve_view(None, &name).is_some() {
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
        // when an in-scope CTE of the same name shadows the base relation.
        ast::TableFactor::Table {
            name, args: None, ..
        } => {
            if let Some(part) = name.0.last().and_then(|part| part.as_ident()) {
                let name = normalize_ident(part);
                if !scope.contains(&name) {
                    names.push(name);
                }
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
    let tnames = names
        .iter()
        .map(object_name_to_table_name)
        .collect::<Result<Vec<_>, _>>()?;
    // A target named twice is rejected up front, before anything is dropped, as
    // in PG — otherwise the second pass would re-drop (and, if a temp table
    // shadows a permanent one, silently drop the permanent table too).
    for (i, name) in tnames.iter().enumerate() {
        if tnames[..i].contains(name) {
            return Err(PgError::new(
                sqlstate::DUPLICATE_OBJECT,
                format!("table \"{name}\" specified more than once"),
            ));
        }
    }
    // Phase 1: validate. A missing target without IF EXISTS aborts the whole
    // statement before anything is dropped; with IF EXISTS it becomes a skip
    // NOTICE. PG spells the missing-object noun "table" here, not "relation".
    let mut notices = Vec::new();
    let mut to_drop: Vec<String> = Vec::new();
    for name in &tnames {
        match catalog.open_table(name) {
            Ok(_) => to_drop.push(name.clone()),
            Err(StorageError::TableNotFound(_)) => {
                // A view shares the relation namespace: PG rejects DROP TABLE on
                // one as a wrong-object-type error rather than "does not exist".
                if catalog.resolve_view(None, name).is_some() {
                    return Err(PgError::new(
                        sqlstate::WRONG_OBJECT_TYPE,
                        format!("\"{name}\" is not a table"),
                    )
                    .with_hint("Use DROP VIEW to remove a view."));
                }
                if if_exists {
                    notices.push(Notice::notice(
                        format!("table \"{name}\" does not exist, skipping"),
                        None,
                    ));
                } else {
                    return Err(PgError::new(
                        sqlstate::UNDEFINED_TABLE,
                        format!("table \"{name}\" does not exist"),
                    ));
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    // Phase 2: resolve dependent views (RESTRICT errors, CASCADE drops them),
    // then drop the tables followed by their cascaded views.
    let (dependent_views, mut cascade_notices) =
        plan_view_cascade(catalog, "table", &to_drop, cascade)?;
    notices.append(&mut cascade_notices);
    for name in &to_drop {
        catalog.drop_table(name)?;
    }
    for view in &dependent_views {
        catalog.drop_view(view)?;
    }
    // Auto-drop sequences a dropped table owns (a `serial` column's sequence, via
    // PG's OWNED BY). PG removes these silently, without a cascade notice.
    for seq in catalog.sequences() {
        if seq
            .owned_by
            .as_deref()
            .is_some_and(|owner| to_drop.iter().any(|t| t == owner))
        {
            let _ = catalog.drop_sequence(&seq.name);
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
    let (dependent_views, mut cascade_notices) =
        plan_view_cascade(catalog, "view", &to_drop, cascade)?;
    notices.append(&mut cascade_notices);
    for name in &to_drop {
        catalog.drop_view(name)?;
    }
    for view in &dependent_views {
        catalog.drop_view(view)?;
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
fn unique_relation_name(engine: &Arc<dyn TableEngine>, extra: &[String], base: &str) -> String {
    let taken = |n: &str| {
        extra.iter().any(|x| x == n)
            || engine.sequence(n).is_some()
            || engine.open_table(n).is_ok()
            || engine.resolve_view(None, n).is_some()
            || engine
                .relation_metadata()
                .iter()
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
        let _ = engine.drop_sequence(&def.name);
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
    let seq_name = object_name_to_table_name(name)?;
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
    let def = build_sequence_definition(seq_name.clone(), data_type, options, None)?;
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
        if catalog.sequence(name).is_some() {
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
            let Some(def) = catalog.sequence(name) else {
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
        catalog.drop_sequence(name)?;
    }
    Ok(QueryResult::Command {
        tag: "DROP SEQUENCE".into(),
        notices,
    })
}

/// Resolve the views that depend on a set of relations being dropped. `targets`
/// are the relation names being removed, all of object class `target_noun`
/// (`"table"` or `"view"`). Under RESTRICT (`!cascade`) any dependent is an
/// error (2BP01, with a DETAIL line per dependency edge). Under CASCADE it
/// returns the transitive set of dependent view names to drop, in
/// discovery order, plus the `drop cascades to ...` NOTICE(s) — matching the
/// wording of `DROP TYPE ... CASCADE`.
fn plan_view_cascade(
    catalog: &Arc<dyn TableEngine>,
    target_noun: &str,
    targets: &[String],
    cascade: bool,
) -> Result<(Vec<String>, Vec<Notice>), PgError> {
    let all_views = catalog.views();
    let is_target = |name: &str| targets.iter().any(|t| t == name);

    if !cascade {
        // RESTRICT: report every (dependent view, target) edge as a DETAIL line,
        // in target order then view order, as PG does.
        let mut detail = Vec::new();
        let mut first_blocked: Option<&str> = None;
        for target in targets {
            for view in &all_views {
                if is_target(&view.name) {
                    continue;
                }
                if view.depends_on.iter().any(|d| d == target) {
                    detail.push(format!("view {} depends on {target_noun} {target}", view.name));
                    first_blocked.get_or_insert(target.as_str());
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
    let mut removed: Vec<String> = targets.to_vec();
    let mut dependents: Vec<String> = Vec::new();
    loop {
        let mut added = false;
        for view in &all_views {
            if removed.iter().any(|r| r == &view.name) {
                continue;
            }
            if view.depends_on.iter().any(|d| removed.iter().any(|r| r == d)) {
                removed.push(view.name.clone());
                dependents.push(view.name.clone());
                added = true;
            }
        }
        if !added {
            break;
        }
    }

    let notices = match dependents.as_slice() {
        [] => Vec::new(),
        [one] => vec![Notice::notice(format!("drop cascades to view {one}"), None)],
        many => {
            let detail = many
                .iter()
                .map(|v| format!("drop cascades to view {v}"))
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
        assert_eq!(deps("WITH c AS (SELECT 1) SELECT * FROM c"), Vec::<String>::new());
        assert_eq!(deps("WITH c AS (SELECT * FROM t) SELECT * FROM c"), vec!["t"]);
    }

    #[test]
    fn referenced_relations_scopes_cte_shadowing_to_its_own_query() {
        // The CTE `c` shadows a base table only inside the derived subquery; the
        // outer `c` is the real relation and must remain a dependency.
        assert_eq!(
            deps("SELECT * FROM (WITH c AS (SELECT 1) SELECT * FROM c) d, c"),
            vec!["c"]
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
