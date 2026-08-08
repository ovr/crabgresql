//! SQL-level prepared statements: `PREPARE`, `EXECUTE`, `DEALLOCATE`.
//!
//! The statement layer over [`crate::session::PreparedStatement`], which the
//! extended query protocol already fills from `Parse`. Both spellings share one
//! namespace, as in PostgreSQL: `DEALLOCATE` drops a statement the protocol
//! prepared, `PREPARE` collides with one, and `pg_prepared_statements` lists
//! both (telling them apart by `from_sql`).
//!
//! Reproduces PostgreSQL's observable behavior (parameter rules, command tags,
//! SQLSTATEs) rather than porting its plan cache.
//!
//! One deliberate difference: every execution re-plans, so `generic_plans` and
//! `custom_plans` are split by whether the statement takes parameters rather
//! than by a plan cache's choice. That is correct because it reports the column
//! PostgreSQL fills for each shape — a parameterless statement is generic from
//! its first execution there too — and this build has no cached plan for the two
//! counters to distinguish.
//!
//! TODO: record a span for `Statement::Prepare` in the parser, so
//! `pg_prepared_statements.statement` can carry the client's raw text instead of
//! the AST's rendering of it, and so a `PREPARE` of a utility statement can point
//! a caret at the offending keyword. `pg_cursors` waits on the same change.
//! TODO: resolve a bare `$n` in a target list to `text`, as PostgreSQL does;
//! `PREPARE p AS SELECT $1` raises `42P18` until then. The gap is the binder's,
//! shared with the extended protocol, not this module's.

use std::sync::Arc;

use crabgresql_parser::ast::{self, Spanned};
use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::{TableEngine, TypeCatalog};
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
    // The global catalog *is* what `bind_catalogs` hands back as the type view,
    // so resolving a declared type needs nothing built: a `SystemCatalog`
    // snapshot here would eagerly copy every cursor, prepared statement and GUC
    // row only to be dropped unread.
    let type_catalog: Arc<dyn TypeCatalog> = global_catalog.clone();
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
            // TODO: slice the client's raw text once the parser records a span
            // for PREPARE. PostgreSQL stores what the client wrote; the AST's own
            // rendering round-trips for canonical input, which is why the smoke
            // suite matches, but comments and spacing are lost.
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

/// How deep `EXECUTE` may nest.
///
/// A prepared statement can name another one, so `EXECUTE` re-enters the
/// statement executor. PostgreSQL bounds that with `max_stack_depth`, a *byte*
/// budget checked by probing the actual stack pointer; there is no equivalent
/// here, so this is a frame count. A level costs a whole
/// bind/plan/execute recursion, measured by chaining prepared statements until
/// the process died: a 2 MB thread stack survives 64 levels and overflows by 68,
/// so roughly 31 KB each.
///
/// Set far below half of that because the budget is shared: a PL/pgSQL call can
/// nest on top of an `EXECUTE` chain, and its own cap of 24 frames
/// (`crabgresql_plpgsql`'s `MAX_CALL_DEPTH`, at ~40 KB each) already claims
/// about half the stack. The failure mode being guarded against is a process
/// abort — one connection's recursion killing every other session — not an
/// error.
const MAX_EXECUTE_DEPTH: u32 = 16;

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
    let Resolved { stmt, params } = resolve_execute(engine, global_catalog, execute, session)?;
    let Some(stmt) = stmt else {
        // Only reachable for a statement the protocol prepared from an empty
        // query string; SQL `PREPARE` always carries one.
        return Ok(QueryResult::Command {
            tag: String::new(),
            notices: session.notices.drain(),
        });
    };
    // The statement being run may itself be an `EXECUTE` — nothing stops a
    // `Parse` message from preparing one, including one that names itself — so
    // this call is what recursion travels through, and the counter has to wrap
    // exactly it. Checked before the increment so the depth cannot be exceeded
    // by the frame that reports it.
    if session.execute_depth >= MAX_EXECUTE_DEPTH {
        return Err(PgError::new(
            sqlstate::STATEMENT_TOO_COMPLEX,
            "stack depth limit exceeded",
        )
        .with_hint(
            "Increase the configuration parameter max_stack_depth, after ensuring \
             the platform's stack depth limit is adequate.",
        ));
    }
    session.execute_depth += 1;
    let result = execute_statement_with(
        engine,
        global_catalog,
        txnmgr,
        &stmt,
        session,
        &params,
        force_materialize,
    );
    // Decremented on both paths: an error unwinds through here, and leaving the
    // depth raised would make every later statement on this session look nested.
    session.execute_depth -= 1;
    result
}

/// What an `EXECUTE` resolves to: the statement it names and the parameters its
/// arguments evaluated to.
pub(crate) struct Resolved {
    /// `None` for a statement the protocol prepared from an empty query string.
    pub stmt: Option<ast::Statement>,
    pub params: BoundParams,
}

/// Resolve `EXECUTE name (args)` to the statement it runs, evaluating the
/// arguments against the parameter types the statement was prepared with.
///
/// Shared with the `EXPLAIN EXECUTE` path, which needs the same statement and
/// the same parameters but plans them instead of running them — reusing the
/// caller's `BoundParams` there would explain the wrong values.
pub(crate) fn resolve_execute(
    engine: &Arc<dyn TableEngine>,
    global_catalog: &Arc<GlobalCatalog>,
    execute: &ast::Statement,
    session: &mut Session,
) -> Result<Resolved, PgError> {
    let (name, parameters) = execute_parts(execute)?;
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
    Ok(Resolved {
        stmt,
        params: BoundParams {
            types: param_types,
            values,
            // The plan is built with the parameters folded in, which is what the
            // `Bind`/`Execute` path does with the values it decoded off the wire.
            extended: true,
        },
    })
}

/// The two fields PostgreSQL's `EXECUTE` has, rejecting the spellings the shared
/// parser accepts for other dialects (`EXECUTE IMMEDIATE`, `INTO`, `USING`,
/// `OUTPUT`, `DEFAULT`), which PostgreSQL's grammar has none of.
fn execute_parts(execute: &ast::Statement) -> Result<(&ast::ObjectName, &[ast::Expr]), PgError> {
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
        return Err(PgError::new(
            sqlstate::INTERNAL_ERROR,
            "statement is not an EXECUTE",
        ));
    };
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
    Ok((name, parameters))
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
/// parse to a Query here), INSERT, UPDATE and DELETE. Anything else is refused as
/// a syntax error, which is what PostgreSQL's parser does with a utility
/// statement here.
///
/// TODO: admit MERGE once it is implemented; PostgreSQL prepares it too.
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
        let expr =
            crabgresql_binder::coerce_to_param(binding, i + 1, param_types[i], arg.span(), &scope)?;
        // The guards every other empty-scope binder applies, in the order
        // `bind_column_default` applies them. Without these an aggregate reaches
        // the evaluator and reports its generic 0A000 instead of PostgreSQL's
        // 42803, which names the clause it was rejected from.
        if expr.contains_srf() {
            return Err(PgError::feature_not_supported(
                "set-returning functions are not allowed in EXECUTE parameters",
            ));
        }
        crabgresql_binder::reject_agg_or_window(&expr, "EXECUTE parameters")?;
        bound.push(expr);
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
