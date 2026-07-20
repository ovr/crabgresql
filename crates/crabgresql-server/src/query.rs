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
use crabgresql_executor::{ExecNode, Execution, OutputColumn, Values, execute};
use crabgresql_parser::ast;
use crabgresql_pg_wire::{TransactionStatus, sqlstate};
use crabgresql_storage_api::{
    Column, IndexConstraint, IndexKey, IndexMetadata, IndexMethod, StorageError, TableAm,
    TableEngine, TableSchema, TypeCatalog,
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
    /// A result set, streamed: the caller pulls tuples from the node and
    /// derives the `SELECT n` tag from the row count.
    Rows {
        columns: Vec<OutputColumn>,
        node: Box<dyn ExecNode>,
    },
    Command {
        tag: String,
        /// Warnings to emit before the CommandComplete, in order.
        notices: Vec<Notice>,
    },
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
    // A DQL plan has a row shape; a data-modifying plan does not (no RETURNING),
    // which `output_columns_of` reports as an error we fold to `None` (NoData).
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
                if_exists,
                ..
            } => return execute_drop_table(&catalog, names, *if_exists),
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
    let txn = build_txn(txnmgr, session, is_write);
    let exec = match execute(
        crabgresql_planner::plan(logical),
        session.exec_context(),
        &txn,
    ) {
        Ok(exec) => exec,
        Err(e) => {
            // Abort path: infallible, so the result is safe to drop.
            let _ = finalize_statement(txnmgr, session, &txn, is_write, false);
            return Err(e.into());
        }
    };
    finalize_statement(txnmgr, session, &txn, is_write, true)?;
    let result = match exec {
        Execution::Rows { columns, node } => QueryResult::Rows { columns, node },
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
    for col in &create.columns {
        let ty = resolve_column_type(type_catalog, &col.data_type)?;
        let typmod = crabgresql_binder::length_typmod(&col.data_type).unwrap_or(-1);
        let column_name = normalize_ident(&col.name);
        let mut column = Column::with_typmod(column_name.clone(), ty, typmod);
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
    match target.create_table(schema) {
        Ok(_) => {
            for index in indexes {
                if let Err(e) = target.create_index(&name, index) {
                    let _ = target.drop_table(&name);
                    return Err(e.into());
                }
            }
        }
        // PG succeeds with a notice; NoticeResponse itself is still todo.
        Err(StorageError::TableAlreadyExists(_)) if create.if_not_exists => {}
        Err(e) => return Err(e.into()),
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
fn execute_drop_table(
    catalog: &Arc<dyn TableEngine>,
    names: &[ast::ObjectName],
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
    let mut to_drop = Vec::new();
    for name in &tnames {
        match catalog.open_table(name) {
            Ok(_) => to_drop.push(name),
            Err(StorageError::TableNotFound(_)) if if_exists => {
                notices.push(Notice::notice(
                    format!("table \"{name}\" does not exist, skipping"),
                    None,
                ));
            }
            Err(StorageError::TableNotFound(_)) => {
                return Err(PgError::new(
                    sqlstate::UNDEFINED_TABLE,
                    format!("table \"{name}\" does not exist"),
                ));
            }
            Err(e) => return Err(e.into()),
        }
    }
    // Phase 2: drop the validated survivors.
    for name in to_drop {
        catalog.drop_table(name)?;
    }
    Ok(QueryResult::Command {
        tag: "DROP TABLE".into(),
        notices,
    })
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
