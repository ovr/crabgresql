//! SQL cursors: `DECLARE`, `FETCH`, `MOVE`, `CLOSE`.
//!
//! A cursor is a named, repositionable result set owned by the session. This
//! module is the statement layer over [`crate::session::Cursor`], which holds
//! the rows and does the position arithmetic: here we translate the surface
//! syntax, enforce the transaction-scoping rules, and produce command tags.
//!
//! Reproduces PostgreSQL's observable cursor behavior (row delivery, command
//! tags, SQLSTATEs) rather than porting its portal machinery. Two deliberate
//! divergences are noted at their sites: the rows are materialised at `DECLARE`
//! (see [`crate::session::Cursor`]), and `pg_cursors.statement` is re-rendered
//! from the AST rather than sliced from the client's text.

use std::sync::Arc;

use crabgresql_executor::MaterializedRows;
use crabgresql_parser::ast;
use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::TableEngine;
use crabgresql_txn::TransactionManager;

use crate::error::PgError;
use crate::global_catalog::GlobalCatalog;
use crate::query::{BoundParams, QueryResult, RowTag, StatementFuture, execute_statement_with};
use crate::session::{Cursor, CursorMove, Session};

/// `DECLARE name [BINARY] [[NO] SCROLL] CURSOR [WITH HOLD] FOR <query>`.
///
/// The query runs to completion here, inside this statement's own transaction —
/// see [`Cursor`] for why. `stmt` is the whole statement, kept only so its
/// rendered form can be stored for `pg_cursors`.
///
/// Returns a boxed future rather than being an `async fn`: this calls
/// [`execute_statement_with`] for the cursor's body and is itself called from it,
/// and two mutually recursive `async fn`s have infinitely sized futures. Boxing
/// here breaks the cycle at its one edge.
pub(crate) fn execute_declare<'a>(
    engine: &'a Arc<dyn TableEngine>,
    global_catalog: &'a Arc<GlobalCatalog>,
    txnmgr: &'a Arc<TransactionManager>,
    stmt: &'a ast::Statement,
    stmts: &'a [ast::Declare],
    session: &'a mut Session,
    params: &'a BoundParams,
) -> StatementFuture<'a> {
    Box::pin(declare_cursor(
        engine,
        global_catalog,
        txnmgr,
        stmt,
        stmts,
        session,
        params,
    ))
}

async fn declare_cursor(
    engine: &Arc<dyn TableEngine>,
    global_catalog: &Arc<GlobalCatalog>,
    txnmgr: &Arc<TransactionManager>,
    stmt: &ast::Statement,
    stmts: &[ast::Declare],
    session: &mut Session,
    params: &BoundParams,
) -> Result<QueryResult, PgError> {
    // Other dialects let one DECLARE introduce several variables; PostgreSQL's
    // cursor form declares exactly one name.
    let declare = match stmts {
        [single] if single.declare_type == Some(ast::DeclareType::Cursor) => single,
        _ => {
            return Err(PgError::feature_not_supported(
                "DECLARE of a variable is not supported yet",
            ));
        }
    };
    let Some(query) = declare.for_query.as_ref() else {
        return Err(PgError::syntax("DECLARE CURSOR requires a FOR clause"));
    };
    let Some(name) = declare.names.first().map(cursor_name) else {
        return Err(PgError::syntax("DECLARE CURSOR requires a cursor name"));
    };
    let hold = declare.hold.unwrap_or(false);
    // The *statement* timestamp, not the transaction's — probed against
    // PostgreSQL 18.4, where a cursor declared mid-block reports an instant
    // strictly after that block's `now()`. Stamped before the query runs, so it
    // is this DECLARE's own.
    let creation_time = session
        .fmt_ctx()
        .stmt_start()
        .map_err(|e| PgError::new(e.sqlstate, e.message))?;
    if declare.binary == Some(true) {
        return Err(PgError::feature_not_supported(
            "DECLARE ... BINARY CURSOR is not supported yet",
        ));
    }
    // Without WITH HOLD a cursor dies with the transaction that made it, so
    // declaring one under autocommit could never be useful — PG rejects it
    // rather than opening a cursor the next statement cannot see.
    if !hold && session.xact.is_none() {
        return Err(PgError::new(
            sqlstate::NO_ACTIVE_SQL_TRANSACTION,
            "DECLARE CURSOR can only be used in transaction blocks",
        ));
    }
    if session.cursors.contains_key(&name) {
        return Err(PgError::new(
            sqlstate::DUPLICATE_CURSOR,
            format!("cursor \"{name}\" already exists"),
        ));
    }

    let in_block = session.xact.is_some();
    let source = ast::Statement::Query(query.clone());
    // The cursor body sees the parameters bound to the DECLARE, so a
    // `DECLARE … FOR SELECT … $1` opens over the rows the client asked for.
    let result = execute_statement_with(
        engine,
        global_catalog,
        txnmgr,
        &source,
        session,
        params,
        true,
    )
    .await?;
    let (columns, mut node, notices) = match result {
        QueryResult::Rows {
            columns,
            node,
            notices,
            ..
        } => (columns, node, notices),
        // The binder resolves a cursor's body as a query, so this is not
        // reachable from any statement the grammar admits.
        QueryResult::Command { .. } => {
            return Err(PgError::syntax("DECLARE CURSOR requires a query"));
        }
    };
    // Already materialised by `force_materialize`, so this only moves the rows
    // out of the node — nothing runs after the transaction closed.
    let mut rows = Vec::new();
    while let Some(row) = node.next()? {
        rows.push(row);
    }

    session.cursors.insert(
        name,
        Cursor {
            columns,
            rows,
            pos: 0,
            hold,
            scroll: declare.scroll,
            // Divergence: PostgreSQL keeps the client's raw text. The parser
            // reports no span for DECLARE, so there is nothing to slice; the
            // AST's own rendering round-trips for canonical input.
            statement: format!("{stmt};"),
            in_block,
            creation_time,
        },
    );
    Ok(QueryResult::Command {
        tag: "DECLARE CURSOR".to_string(),
        notices,
    })
}

/// `FETCH [direction] [FROM|IN] name`.
pub(crate) fn execute_fetch(
    name: &ast::Ident,
    direction: &ast::FetchDirection,
    into: Option<&ast::ObjectName>,
    session: &mut Session,
) -> Result<QueryResult, PgError> {
    if into.is_some() {
        return Err(PgError::feature_not_supported(
            "FETCH ... INTO is not supported yet",
        ));
    }
    let movement = movement_of(direction)?;
    let cursor = lookup(session, name)?;
    let columns = cursor.columns.clone();
    let rows = cursor.fetch(movement)?;
    Ok(QueryResult::Rows {
        columns,
        node: Box::new(MaterializedRows::new(rows)),
        tag: RowTag::Fetch,
        notices: Vec::new(),
    })
}

/// `MOVE [direction] [FROM|IN] name` — a `FETCH` that discards its rows and
/// reports how many it passed over.
pub(crate) fn execute_move(
    name: &ast::Ident,
    direction: &ast::FetchDirection,
    session: &mut Session,
) -> Result<QueryResult, PgError> {
    let movement = movement_of(direction)?;
    let moved = lookup(session, name)?.advance(movement)?;
    Ok(QueryResult::Command {
        tag: format!("MOVE {moved}"),
        notices: Vec::new(),
    })
}

/// `CLOSE name` / `CLOSE ALL`.
pub(crate) fn execute_close(
    cursor: &ast::CloseCursor,
    session: &mut Session,
) -> Result<QueryResult, PgError> {
    let tag = match cursor {
        ast::CloseCursor::All => {
            session.cursors.clear();
            "CLOSE CURSOR ALL"
        }
        ast::CloseCursor::Specific { name } => {
            let name = cursor_name(name);
            if session.cursors.remove(&name).is_none() {
                return Err(not_found(&name));
            }
            "CLOSE CURSOR"
        }
    };
    Ok(QueryResult::Command {
        tag: tag.to_string(),
        notices: Vec::new(),
    })
}

/// Close what a committing block leaves behind: everything it declared except
/// the holdable cursors, which outlive it. A surviving cursor stops belonging to
/// any block, so a later `ROLLBACK` must not take it.
pub(crate) fn close_on_commit(session: &mut Session) {
    session.cursors.retain(|_, cursor| cursor.hold);
    for cursor in session.cursors.values_mut() {
        cursor.in_block = false;
    }
}

/// Close what an aborting block leaves behind: everything it declared, holdable
/// or not. A holdable cursor declared under autocommit belongs to no block and
/// survives.
pub(crate) fn close_on_abort(session: &mut Session) {
    session.cursors.retain(|_, cursor| !cursor.in_block);
}

/// The name a cursor is filed under. Unquoted identifiers fold to lowercase, as
/// everywhere else in the catalog — `DECLARE Foo` and `FETCH foo` are the same
/// cursor, and `DECLARE "Foo"` is a different one.
pub(crate) fn cursor_name(ident: &ast::Ident) -> String {
    crate::query::normalize_ident(ident)
}

/// The named cursor, or PostgreSQL's 34000.
fn lookup<'a>(session: &'a mut Session, name: &ast::Ident) -> Result<&'a mut Cursor, PgError> {
    let name = cursor_name(name);
    session
        .cursors
        .get_mut(&name)
        .ok_or_else(|| not_found(&name))
}

fn not_found(name: &str) -> PgError {
    PgError::new(
        sqlstate::INVALID_CURSOR_NAME,
        format!("cursor \"{name}\" does not exist"),
    )
}

/// Fold a surface direction into the four shapes [`CursorMove`] models.
///
/// `FIRST`/`LAST` are `ABSOLUTE 1`/`ABSOLUTE -1`, and `NEXT`/`PRIOR` are one row
/// forward/backward — but *not* `RELATIVE ±1`, because a relative move delivers
/// only its landing row while a directional one delivers everything it passes.
/// At a distance of one the two agree, which is why PostgreSQL can define them
/// either way; keeping them directional here means `FETCH NEXT` and `FETCH 1`
/// share a code path.
fn movement_of(direction: &ast::FetchDirection) -> Result<CursorMove, PgError> {
    let movement = match direction {
        ast::FetchDirection::Next | ast::FetchDirection::Forward { limit: None } => {
            CursorMove::Forward(Some(1))
        }
        ast::FetchDirection::Prior | ast::FetchDirection::Backward { limit: None } => {
            CursorMove::Backward(Some(1))
        }
        ast::FetchDirection::First => CursorMove::Absolute(1),
        ast::FetchDirection::Last => CursorMove::Absolute(-1),
        ast::FetchDirection::All | ast::FetchDirection::ForwardAll => CursorMove::Forward(None),
        ast::FetchDirection::BackwardAll => CursorMove::Backward(None),
        ast::FetchDirection::Count { limit }
        | ast::FetchDirection::Forward { limit: Some(limit) } => {
            CursorMove::Forward(Some(count(limit)?))
        }
        ast::FetchDirection::Backward { limit: Some(limit) } => {
            CursorMove::Backward(Some(count(limit)?))
        }
        ast::FetchDirection::Absolute { limit } => CursorMove::Absolute(count(limit)?),
        ast::FetchDirection::Relative { limit } => CursorMove::Relative(count(limit)?),
    };
    Ok(movement)
}

/// The integer a direction's count literal denotes.
///
/// The parser accepts only PostgreSQL's `SignedIconst` here, so the literal is
/// an `int` and this cannot fail in practice; the error path exists so a parser
/// change cannot silently truncate a count instead. The literal is decoded
/// through the shared acceptor because `FETCH 0x2` is a valid count in PG —
/// the parser's own range check reads it the same way.
fn count(limit: &ast::ValueWithSpan) -> Result<i64, PgError> {
    match &limit.value {
        ast::Value::Number(digits, _) => crabgresql_binder::literal_int(digits)
            .and_then(|v| i32::try_from(v).ok())
            .map(i64::from),
        _ => None,
    }
    .ok_or_else(|| PgError::syntax(format!("syntax error at or near \"{}\"", limit.value)))
}
