//! SQL-level prepared statements: `PREPARE`, `EXECUTE`, `DEALLOCATE`.
//!
//! The statement layer over [`crate::session::PreparedStatement`], which the
//! extended query protocol already fills from `Parse`. Both spellings share one
//! namespace, as in PostgreSQL: `DEALLOCATE` drops a statement the protocol
//! prepared, `PREPARE` collides with one, and `pg_prepared_statements` lists
//! both (telling them apart by `from_sql`).
//!
//! Reproduces PostgreSQL's observable behavior (parameter rules, command tags,
//! SQLSTATEs) rather than porting its plan cache. Three deliberate divergences:
//!
//! * `pg_prepared_statements.statement` is re-rendered from the AST for a SQL
//!   `PREPARE`, because the parser reports no span for it — the same tradeoff
//!   [`crate::cursor`] makes for `pg_cursors`.
//! * Every `EXECUTE` re-plans, so `generic_plans` is always 0 and
//!   `custom_plans` counts executions. There is no plan cache to choose between.
//! * `PREPARE p AS SELECT $1` is `42P18` here, where PostgreSQL resolves the
//!   bare parameter to `text`. That is the binder's existing extended-protocol
//!   behavior, unchanged by this module.

use std::sync::Arc;

use crabgresql_parser::ast::{self, Spanned};
use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::TableEngine;
use crabgresql_txn::TransactionManager;
use crabgresql_types::{PgType, Value};

use crate::error::PgError;
use crate::global_catalog::GlobalCatalog;
use crate::query::{
    Analyzed, BoundParams, QueryResult, analyze_statement, bind_catalogs, execute_statement_with,
    normalize_ident, read_only_active, resolve_column_type, single_object_name, statement_kind,
    statement_runtime,
};
use crate::session::{PreparedStatement, Session};

/// `PREPARE name [(type, …)] AS <statement>`.
///
/// `stmt` is the whole statement, kept only so its rendered form can be stored
/// for `pg_prepared_statements`.
pub(crate) fn execute_prepare(
    engine: &Arc<dyn TableEngine>,
    global_catalog: &Arc<GlobalCatalog>,
    stmt: &ast::Statement,
    name: &ast::Ident,
    data_types: &[ast::DataType],
    inner: &ast::Statement,
    session: &mut Session,
) -> Result<QueryResult, PgError> {
    // PostgreSQL's grammar admits only an optimizable statement here, so a
    // utility statement is a *syntax* error at the offending keyword rather than
    // an unsupported-feature report.
    if !is_preparable(inner) {
        return Err(PgError::syntax(format!(
            "syntax error at or near \"{}\"",
            statement_kind(inner)
        )));
    }
    let name = normalize_ident(name);
    if session.prepared.contains_key(&name) {
        return Err(PgError::new(
            sqlstate::DUPLICATE_PREPARED_STATEMENT,
            format!("prepared statement \"{name}\" already exists"),
        ));
    }
    // A declared type list seeds the inference; an omitted one leaves every `$n`
    // to be deduced from its use, exactly as an all-zero `Parse` OID list does.
    let (_, type_catalog, _) = bind_catalogs(engine, global_catalog, session);
    let declared = data_types
        .iter()
        .map(|dt| resolve_column_type(&type_catalog, dt).map(Some))
        .collect::<Result<Vec<_>, _>>()?;
    // The same parse-analysis the extended protocol runs at `Parse`: it resolves
    // the parameter types and the result shape without executing.
    let Analyzed {
        param_types,
        result_columns,
    } = analyze_statement(engine, global_catalog, inner, declared, session)?;
    let prepare_time = session
        .fmt_ctx()
        .stmt_start()
        .map_err(|e| PgError::new(e.sqlstate, e.message))?;
    session.prepared.insert(
        name,
        PreparedStatement {
            stmt: Some(inner.clone()),
            param_types,
            result_columns,
            // Divergence: PostgreSQL keeps the client's raw text. The parser
            // reports no span for PREPARE, so there is nothing to slice; the
            // AST's own rendering round-trips for canonical input.
            statement: format!("{stmt};"),
            from_sql: true,
            prepare_time,
            executions: 0,
        },
    );
    Ok(QueryResult::Command {
        tag: "PREPARE".to_string(),
        notices: session.notices.drain(),
    })
}

/// `EXECUTE name [(expr, …)]`.
///
/// The result — rows and command tag alike — is the prepared statement's own, so
/// `EXECUTE` of an `INSERT` reports `INSERT 0 1`.
pub(crate) fn execute_execute(
    engine: &Arc<dyn TableEngine>,
    global_catalog: &Arc<GlobalCatalog>,
    txnmgr: &Arc<TransactionManager>,
    execute: &ast::Statement,
    session: &mut Session,
    force_materialize: bool,
) -> Result<QueryResult, PgError> {
    let ast::Statement::Execute {
        name,
        parameters,
        immediate,
        into,
        using,
        output,
        default,
        ..
    } = execute
    else {
        return Err(PgError::syntax(
            "EXECUTE requires a prepared statement name",
        ));
    };
    // `EXECUTE IMMEDIATE`, `INTO`, `USING`, `OUTPUT` and `DEFAULT` are other
    // dialects' spellings the shared parser accepts; PostgreSQL's EXECUTE has
    // none of them.
    if *immediate || *output || *default || !into.is_empty() || !using.is_empty() {
        return Err(PgError::feature_not_supported(
            "this form of EXECUTE is not supported yet",
        ));
    }
    let Some(name) = name else {
        return Err(PgError::syntax(
            "EXECUTE requires a prepared statement name",
        ));
    };
    let name = single_object_name(name, "prepared statement")?;
    let (stmt, param_types) = {
        let prepared = lookup(session, &name)?;
        (prepared.stmt.clone(), prepared.param_types.clone())
    };
    let values = bind_arguments(
        engine,
        global_catalog,
        &name,
        &param_types,
        parameters,
        session,
    )?;
    // Counted before the statement runs, so a failing execution still shows in
    // `pg_prepared_statements` — PostgreSQL counts the planning, not the success.
    if let Some(prepared) = session.prepared.get_mut(&name) {
        prepared.executions += 1;
    }
    let Some(stmt) = stmt else {
        // Only reachable for a statement the protocol prepared from an empty
        // query string; SQL `PREPARE` always carries one.
        return Ok(QueryResult::Command {
            tag: String::new(),
            notices: session.notices.drain(),
        });
    };
    let params = BoundParams {
        types: param_types,
        values,
        // The plan is built with the parameters folded in, which is what the
        // `Bind`/`Execute` path does with the values it decoded off the wire.
        extended: true,
    };
    execute_statement_with(
        engine,
        global_catalog,
        txnmgr,
        &stmt,
        session,
        &params,
        force_materialize,
    )
}

/// `DEALLOCATE [PREPARE] { name | ALL }`.
///
/// `ALL` is a keyword only when written unquoted: `DEALLOCATE "ALL"` names a
/// statement, as in PostgreSQL. The parser hands both back as an identifier, so
/// the quoting is what tells them apart.
pub(crate) fn execute_deallocate(
    name: &ast::Ident,
    session: &mut Session,
) -> Result<QueryResult, PgError> {
    if name.quote_style.is_none() && name.value.eq_ignore_ascii_case("all") {
        session.prepared.clear();
        return Ok(QueryResult::Command {
            tag: "DEALLOCATE ALL".to_string(),
            notices: session.notices.drain(),
        });
    }
    let name = normalize_ident(name);
    if session.prepared.remove(&name).is_none() {
        return Err(not_found(&name));
    }
    Ok(QueryResult::Command {
        tag: "DEALLOCATE".to_string(),
        notices: session.notices.drain(),
    })
}

/// The statements `PREPARE` accepts. PostgreSQL's grammar takes only
/// `PreparableStmt` — SELECT (including VALUES and a leading WITH, both of which
/// parse to a Query here), INSERT, UPDATE, DELETE and MERGE. There is no MERGE
/// in this build, so anything else is refused as a syntax error, which is what
/// PostgreSQL's parser does with a utility statement here.
fn is_preparable(stmt: &ast::Statement) -> bool {
    matches!(
        stmt,
        ast::Statement::Query(_)
            | ast::Statement::Insert(_)
            | ast::Statement::Update { .. }
            | ast::Statement::Delete(_)
    )
}

/// Evaluate an `EXECUTE`'s arguments into the values its parameters expect.
///
/// The arguments are constant expressions evaluated here, before the statement
/// runs: they bind in an empty scope, so a column reference or a subquery is
/// rejected, and each is assignment-coerced to its declared type (`EXECUTE
/// p(1.7)` rounds into an `int` parameter).
fn bind_arguments(
    engine: &Arc<dyn TableEngine>,
    global_catalog: &Arc<GlobalCatalog>,
    name: &str,
    param_types: &[PgType],
    args: &[ast::Expr],
    session: &Session,
) -> Result<Vec<Value>, PgError> {
    // A statement with no parameters ignores whatever it was given, without so
    // much as evaluating it — PostgreSQL only reaches its argument evaluator
    // when the statement declared parameters, so `EXECUTE p(1/0)` on a
    // parameterless `p` succeeds.
    if param_types.is_empty() {
        return Ok(Vec::new());
    }
    if args.len() != param_types.len() {
        return Err(PgError::syntax(format!(
            "wrong number of parameters for prepared statement \"{name}\""
        ))
        .with_detail(format!(
            "Expected {} parameters but got {}.",
            param_types.len(),
            args.len()
        )));
    }
    let (catalog, type_catalog, catalog_ops) = bind_catalogs(engine, global_catalog, session);
    let param_ctx = crabgresql_binder::param_ctx_none();
    let scope = crabgresql_binder::Scope::empty(&type_catalog, &param_ctx);
    let mut bound = Vec::with_capacity(args.len());
    for (i, arg) in args.iter().enumerate() {
        // An empty scope refuses subqueries generically; PostgreSQL names the
        // context it refused them in.
        let binding = crabgresql_binder::bind_expr(arg, &scope)
            .map_err(crabgresql_binder::subquery_in_execute_param)?;
        bound.push(crabgresql_binder::coerce_to_param(
            binding,
            i + 1,
            param_types[i],
            arg.span(),
            &scope,
        )?);
    }
    // No transaction is attached: these expressions are constants evaluated
    // before the statement opens its own, exactly as a partition bound is folded.
    let (routines, command_counter) =
        statement_runtime(&catalog, &type_catalog, global_catalog, session);
    let exec_ctx = session.exec_context_for_statement(
        engine,
        &catalog_ops,
        &type_catalog,
        routines,
        command_counter,
        read_only_active(session),
    );
    bound
        .iter()
        .map(|expr| Ok(crabgresql_executor::eval_row_free(expr, &exec_ctx)?))
        .collect()
}

/// The named prepared statement, or PostgreSQL's 26000.
fn lookup<'a>(session: &'a Session, name: &str) -> Result<&'a PreparedStatement, PgError> {
    session.prepared.get(name).ok_or_else(|| not_found(name))
}

fn not_found(name: &str) -> PgError {
    PgError::new(
        sqlstate::INVALID_SQL_STATEMENT_NAME,
        format!("prepared statement \"{name}\" does not exist"),
    )
}
