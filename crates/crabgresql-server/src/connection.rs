//! One client connection: startup handshake, then the simple-query loop.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use crabgresql_executor::OutputColumn;
use crabgresql_parser::ast;
use crabgresql_pg_wire::{
    BackendMessage, BackendWriter, CopyResponse, FieldDescription, Format, FrontendMessage,
    FrontendReader, ProtocolError, StartupRequest, Target, TransactionStatus, sqlstate,
};
use crabgresql_storage_api::TableEngine;
use crabgresql_txn::TransactionManager;
use crabgresql_types::cast::CastError;
use crabgresql_types::{PgType, Value, wire};
use tokio::net::TcpStream;

use crate::error::PgError;
use crate::global_catalog::GlobalCatalog;
use crate::query::{
    Analyzed, BoundParams, Notice, QueryResult, RowTag, analyze_statement, execute_statement,
    fetch_columns, prepare_copy_from, run_copy_insert,
};
use crate::session::{Portal, PortalState, PreparedStatement, Session, SuspendedRows};

/// Fake backend pids for BackendKeyData: every connection needs a distinct
/// one, but there are no processes to kill yet (cancel lands with M1+).
static NEXT_BACKEND_ID: AtomicI32 = AtomicI32::new(1);

/// GUCs reported at startup. Drivers parse `server_version` and rely on
/// `client_encoding` / `standard_conforming_strings` to pick quoting rules.
const STARTUP_PARAMETERS: &[(&str, &str)] = &[
    ("server_version", "19.0 (CrabgreSQL 0.1.0)"),
    ("server_encoding", "UTF8"),
    ("client_encoding", "UTF8"),
    ("DateStyle", "ISO, MDY"),
    ("TimeZone", "UTC"),
    ("integer_datetimes", "on"),
    ("standard_conforming_strings", "on"),
    ("is_superuser", "on"),
];

pub async fn handle_connection(
    socket: TcpStream,
    engine: Arc<dyn TableEngine>,
    catalog: Arc<GlobalCatalog>,
    txnmgr: Arc<TransactionManager>,
) -> Result<(), ProtocolError> {
    socket.set_nodelay(true).ok();
    let (read_half, write_half) = socket.into_split();
    let mut reader = FrontendReader::new(tokio::io::BufReader::new(read_half));
    let mut writer = BackendWriter::new(write_half);

    // Startup phase: refuse encryption upgrades until the client sends a real
    // StartupMessage. Cancel requests arrive on their own connection.
    let params = loop {
        match reader.read_startup().await {
            Ok(None) => return Ok(()), // clean disconnect before startup
            Ok(Some(StartupRequest::Ssl)) | Ok(Some(StartupRequest::GssEnc)) => {
                writer.refuse_encryption().await?;
            }
            Ok(Some(StartupRequest::Cancel { .. })) => return Ok(()),
            Ok(Some(StartupRequest::Startup { params })) => break params,
            Err(ProtocolError::UnsupportedProtocolVersion(v)) => {
                writer.error_response(
                    sqlstate::PROTOCOL_VIOLATION,
                    &format!("unsupported frontend protocol {}.{}", v >> 16, v & 0xffff),
                );
                writer.flush().await?;
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    };

    // Trust auth for M0; SCRAM-SHA-256 is on the roadmap.
    writer.authentication_ok();
    for (name, value) in STARTUP_PARAMETERS {
        writer.parameter_status(name, value);
    }
    let backend_id = NEXT_BACKEND_ID.fetch_add(1, Ordering::Relaxed);
    writer.backend_key_data(backend_id, 0);
    writer.ready_for_query(TransactionStatus::Idle);
    writer.flush().await?;

    // Per-connection session state (GUCs). A fresh connection resets them,
    // matching how the regression runner gives each test its own session.
    let user = params
        .get("user")
        .cloned()
        .unwrap_or_else(|| "postgres".to_string());
    let database = params
        .get("database")
        .cloned()
        .unwrap_or_else(|| user.clone());
    let mut session = Session::with_identity(
        txnmgr.clone(),
        engine.clone(),
        database,
        user,
        format!("pg_temp_{backend_id}"),
        crate::session::TEMP_NAMESPACE_OID_BASE + backend_id as u32,
    );

    // After an error on an extended-protocol message, PG discards everything
    // until Sync and only then sends ReadyForQuery — one error, one RFQ per
    // Parse..Sync batch, or the driver's state machine desyncs. Extended-query
    // replies are buffered and flushed at Sync/Flush (or after a simple Query),
    // so the whole batch travels in one write, as libpq expects.
    let mut skip_until_sync = false;
    loop {
        match reader.read_message().await? {
            None | Some(FrontendMessage::Terminate) => return Ok(()),
            // Sync ends an extended-query batch: clear any error state and send
            // exactly one ReadyForQuery. It is honored even while skipping — that
            // is what ends the skip.
            Some(FrontendMessage::Sync) => {
                skip_until_sync = false;
                writer.ready_for_query(session.tx_status);
                writer.flush().await?;
            }
            // Between an error and the next Sync, every message is dropped.
            Some(_) if skip_until_sync => {}
            Some(FrontendMessage::Query(sql)) => {
                run_simple_query(
                    &sql,
                    &engine,
                    &catalog,
                    &txnmgr,
                    &mut session,
                    &mut writer,
                    &mut reader,
                )
                .await?;
                writer.ready_for_query(session.tx_status);
                writer.flush().await?;
            }
            Some(FrontendMessage::Parse {
                name,
                query,
                param_types,
            }) => {
                let outcome = handle_parse(
                    &engine,
                    &catalog,
                    &mut session,
                    &mut writer,
                    &name,
                    &query,
                    &param_types,
                );
                report(
                    &mut writer,
                    &mut session,
                    &mut skip_until_sync,
                    outcome,
                    Some(&query),
                );
            }
            Some(FrontendMessage::Bind {
                portal,
                statement,
                param_formats,
                params,
                result_formats,
            }) => {
                let outcome = handle_bind(
                    &mut session,
                    &mut writer,
                    portal,
                    &statement,
                    &param_formats,
                    &params,
                    result_formats,
                );
                report(
                    &mut writer,
                    &mut session,
                    &mut skip_until_sync,
                    outcome,
                    None,
                );
            }
            Some(FrontendMessage::Describe { target, name }) => {
                let outcome = handle_describe(&mut session, &mut writer, target, &name);
                report(
                    &mut writer,
                    &mut session,
                    &mut skip_until_sync,
                    outcome,
                    None,
                );
            }
            Some(FrontendMessage::Execute { portal, max_rows }) => {
                // COPY FROM STDIN needs the socket (to read CopyData frames),
                // which the pure execute path lacks — drive it here, where the
                // reader is in scope and errors are ProtocolError. Any other
                // statement goes through the normal buffered-reply path.
                if let Some(copy_stmt) = copy_portal_statement(&session, &portal) {
                    match copy_in_stream(
                        &engine,
                        &catalog,
                        &txnmgr,
                        &mut session,
                        &mut writer,
                        &mut reader,
                        &copy_stmt,
                    )
                    .await?
                    {
                        CopyOutcome::Loaded(n) => writer.command_complete(&format!("COPY {n}")),
                        CopyOutcome::ConnectionClosed => return Ok(()),
                        CopyOutcome::Failed(e) => report(
                            &mut writer,
                            &mut session,
                            &mut skip_until_sync,
                            Err(e),
                            None,
                        ),
                    }
                } else {
                    let outcome = handle_execute(
                        &engine,
                        &catalog,
                        &txnmgr,
                        &mut session,
                        &mut writer,
                        &portal,
                        max_rows,
                    );
                    report(
                        &mut writer,
                        &mut session,
                        &mut skip_until_sync,
                        outcome,
                        None,
                    );
                }
            }
            Some(FrontendMessage::Close { target, name }) => {
                // Close never fails: an unknown name is not an error (PG).
                match target {
                    Target::Statement => {
                        session.prepared.remove(&name);
                    }
                    Target::Portal => {
                        session.portals.remove(&name);
                    }
                }
                writer.write(&BackendMessage::CloseComplete);
            }
            // Flush forces buffered replies onto the wire without ending the
            // batch — no ReadyForQuery, no state change.
            Some(FrontendMessage::Flush) => writer.flush().await?,
            // COPY, function-call and password messages have no place in this
            // flow; answer like any unsupported message and recover at Sync.
            Some(other) => {
                report(
                    &mut writer,
                    &mut session,
                    &mut skip_until_sync,
                    Err(PgError::feature_not_supported(format!(
                        "protocol message '{}' is not supported yet",
                        other.tag() as char
                    ))),
                    None,
                );
            }
        }
    }
}

/// Report the outcome of an extended-query handler. On success the handler has
/// already buffered its completion reply; on error, emit exactly one
/// ErrorResponse, mark an open transaction failed, and start skipping until the
/// next Sync — the protocol's "one error per batch" rule. `sql`, when present,
/// lets a bind-time cursor position resolve to a wire character offset.
fn report(
    writer: &mut BackendWriter<impl tokio::io::AsyncWrite + Unpin>,
    session: &mut Session,
    skip_until_sync: &mut bool,
    outcome: Result<(), PgError>,
    sql: Option<&str>,
) {
    if let Err(e) = outcome {
        let position = e
            .location
            .and_then(|(line, col)| sql.map(|s| char_position(s, line, col)));
        writer.error_fields(e.to_fields(position));
        mark_transaction_failed(session);
        *skip_until_sync = true;
    }
}

/// Flush threshold while streaming rows: bounds server memory and gets first
/// rows onto the wire early instead of buffering the whole result set.
const STREAM_FLUSH_BYTES: usize = 8 * 1024;

/// One `Query` message: parse, run every statement, stream the responses.
/// An execution error aborts the remaining statements, as in PG, and — inside
/// an explicit transaction block — puts the session in the failed state so the
/// next ReadyForQuery reports `E`; the block's eventual ROLLBACK (or COMMIT,
/// reported as ROLLBACK) then aborts its XID, undoing the earlier statements'
/// data effects via MVCC.
async fn run_simple_query(
    sql: &str,
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<GlobalCatalog>,
    txnmgr: &Arc<TransactionManager>,
    session: &mut Session,
    writer: &mut BackendWriter<impl tokio::io::AsyncWrite + Unpin>,
    reader: &mut FrontendReader<impl tokio::io::AsyncRead + Unpin>,
) -> Result<(), ProtocolError> {
    let statements = match crabgresql_parser::parse(sql) {
        Ok(statements) => statements,
        Err(e) => {
            let e = PgError::from(e);
            let position = e.location.map(|(line, col)| char_position(sql, line, col));
            writer.error_fields(e.to_fields(position));
            // A syntax error inside a block aborts it, as in PG.
            mark_transaction_failed(session);
            return Ok(());
        }
    };
    if statements.is_empty() {
        writer.empty_query_response();
        return Ok(());
    }
    for stmt in &statements {
        let efd = session.extra_float_digits;
        // COPY FROM STDIN drives the copy sub-protocol on the socket rather than
        // returning a QueryResult, so it is handled inline here.
        if is_copy_from_stdin(stmt) {
            match copy_in_stream(engine, catalog, txnmgr, session, writer, reader, stmt).await? {
                CopyOutcome::Loaded(n) => writer.command_complete(&format!("COPY {n}")),
                CopyOutcome::ConnectionClosed => return Ok(()),
                CopyOutcome::Failed(e) => {
                    let position = e.location.map(|(line, col)| char_position(sql, line, col));
                    writer.error_fields(e.to_fields(position));
                    mark_transaction_failed(session);
                    return Ok(());
                }
            }
            continue;
        }
        let outcome =
            execute_statement(engine, catalog, txnmgr, stmt, session, &BoundParams::none());
        // Diagnostics a routine raised belong to this statement whether it
        // succeeded or failed. Handlers fold them into their own result, but
        // draining again here is what makes stranding them impossible: a path
        // that forgets — or one that returned an error before folding — would
        // otherwise leave them buffered for whatever statement drains next.
        let stranded = session.notices.drain();
        match outcome {
            Ok(mut result) => {
                result.prepend_notices(stranded);
                if write_result(writer, result, efd, sql).await? == WriteOutcome::Errored {
                    mark_transaction_failed(session);
                    return Ok(());
                }
            }
            Err(e) => {
                // PG order: notices raised before the failure, then the error.
                emit_notices(writer, &stranded, Some(sql));
                let position = e.location.map(|(line, col)| char_position(sql, line, col));
                writer.error_fields(e.to_fields(position));
                mark_transaction_failed(session);
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Whether a statement is `COPY <table> [(cols)] FROM STDIN` — the only COPY
/// form the server drives (COPY TO / file / program are rejected at bind time).
fn is_copy_from_stdin(stmt: &ast::Statement) -> bool {
    matches!(
        stmt,
        ast::Statement::Copy {
            to: false,
            target: ast::CopyTarget::Stdin,
            ..
        }
    )
}

/// If an Execute targets a portal bound to a `COPY … FROM STDIN`, return a clone
/// of that statement so the caller can drive the copy sub-protocol. A suspended
/// portal (never a COPY) or any other statement returns `None`, falling through
/// to the ordinary execute path.
fn copy_portal_statement(session: &Session, portal_name: &str) -> Option<ast::Statement> {
    let portal = session.portals.get(portal_name)?;
    if portal.state.is_suspended() {
        return None;
    }
    let prepared = session.prepared.get(&portal.statement)?;
    let stmt = prepared.stmt.as_ref()?;
    is_copy_from_stdin(stmt).then(|| stmt.clone())
}

/// The outcome of driving a COPY FROM STDIN sub-protocol exchange.
enum CopyOutcome {
    /// The stream completed: `n` rows were loaded (`COPY n`).
    Loaded(u64),
    /// A resolve/decode/insert error, or a client `CopyFail`, to report to the
    /// client (and, in an open block, mark the transaction failed).
    Failed(PgError),
    /// The client disconnected mid-copy; end the connection.
    ConnectionClosed,
}

/// Drive one `COPY <table> FROM STDIN`: resolve the target (before entering copy
/// mode, so a bad table errors without a CopyInResponse), send CopyInResponse,
/// accumulate the `CopyData` frames until `CopyDone`, then decode and load the
/// rows as an INSERT. `Flush`/`Sync` arriving mid-copy are ignored, as PG does
/// (the extended protocol sends a Sync right after Execute); the trailing Sync
/// after CopyDone is left for the main loop to turn into ReadyForQuery.
async fn copy_in_stream(
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<GlobalCatalog>,
    txnmgr: &Arc<TransactionManager>,
    session: &mut Session,
    writer: &mut BackendWriter<impl tokio::io::AsyncWrite + Unpin>,
    reader: &mut FrontendReader<impl tokio::io::AsyncRead + Unpin>,
    stmt: &ast::Statement,
) -> Result<CopyOutcome, ProtocolError> {
    let prepared = match prepare_copy_from(engine, catalog, stmt, session) {
        Ok(prepared) => prepared,
        // Resolve error (missing table, unsupported option, aborted/read-only
        // txn): report it without ever entering copy mode.
        Err(e) => return Ok(CopyOutcome::Failed(e)),
    };

    // Text-based copy-in (text or CSV): overall format text, every column text.
    let column_formats = vec![Format::Text; prepared.plan.column_count()];
    writer.write(&BackendMessage::CopyInResponse(CopyResponse {
        format: Format::Text,
        column_formats,
    }));
    writer.flush().await?;

    let mut buffer: Vec<u8> = Vec::new();
    loop {
        match reader.read_message().await? {
            None | Some(FrontendMessage::Terminate) => return Ok(CopyOutcome::ConnectionClosed),
            Some(FrontendMessage::CopyData(bytes)) => buffer.extend_from_slice(&bytes),
            Some(FrontendMessage::CopyDone) => break,
            Some(FrontendMessage::CopyFail(msg)) => {
                return Ok(CopyOutcome::Failed(PgError::new(
                    sqlstate::QUERY_CANCELED,
                    format!("COPY from stdin failed: {msg}"),
                )));
            }
            // Client libraries send a Sync right after Execute and may Flush
            // mid-stream; PG ignores both during COPY IN.
            Some(FrontendMessage::Flush) => writer.flush().await?,
            Some(FrontendMessage::Sync) => {}
            Some(other) => {
                return Ok(CopyOutcome::Failed(PgError::new(
                    sqlstate::PROTOCOL_VIOLATION,
                    format!(
                        "unexpected message type 0x{:02X} during COPY from stdin",
                        other.tag()
                    ),
                )));
            }
        }
    }

    let rows = match crate::copy::decode(&prepared.plan.format, &buffer) {
        Ok(rows) => rows,
        Err(e) => return Ok(CopyOutcome::Failed(e)),
    };
    match run_copy_insert(engine, txnmgr, session, &prepared, rows.rows) {
        Ok(n) => Ok(CopyOutcome::Loaded(n)),
        Err(e) => Ok(CopyOutcome::Failed(e)),
    }
}

/// After a reported error, an open transaction block enters the failed state
/// (`E`); an error outside a block leaves the status untouched (the implicit
/// transaction ends at the statement boundary).
fn mark_transaction_failed(session: &mut Session) {
    if session.tx_status == TransactionStatus::InTransaction {
        session.tx_status = TransactionStatus::Failed;
    }
}

/// Convert a 1-based (line, column) span start into the 1-based character
/// offset the wire `P` field uses (measured over the whole query string). The
/// runner sends one statement per Query message, so this matches psql's
/// `LINE n:` rendering.
fn char_position(sql: &str, line: u64, column: u64) -> usize {
    let mut offset = 0usize;
    for (i, text) in sql.split('\n').enumerate() {
        if (i as u64) + 1 == line {
            return offset + column as usize;
        }
        offset += text.chars().count() + 1; // + newline
    }
    offset + column as usize
}

#[derive(PartialEq, Eq)]
enum WriteOutcome {
    Completed,
    /// An execution error surfaced mid-stream: ErrorResponse was sent (after
    /// any rows already on the wire, which stay sent — as in PG) and the
    /// remaining statements of this Query message must be skipped.
    Errored,
}

async fn write_result(
    writer: &mut BackendWriter<impl tokio::io::AsyncWrite + Unpin>,
    result: QueryResult,
    efd: i32,
    sql: &str,
) -> Result<WriteOutcome, ProtocolError> {
    match result {
        QueryResult::Command { tag, notices } => {
            // PG order: any NOTICE/WARNING messages, then CommandComplete.
            emit_notices(writer, &notices, Some(sql));
            writer.command_complete(&tag);
        }
        QueryResult::Rows {
            columns,
            mut node,
            tag,
            notices,
        } => {
            // Diagnostics raised while producing the rows go out first, as PG
            // does — it emits them at the point they are raised, which for a
            // materialized result set is before the first row is sent.
            emit_notices(writer, &notices, Some(sql));
            let fields: Vec<FieldDescription> = columns
                .iter()
                .map(|c| FieldDescription::new(c.name.clone(), c.ty.oid(), c.ty.typlen()))
                .collect();
            writer.row_description(&fields);
            let mut count = 0usize;
            loop {
                match node.next() {
                    Ok(Some(row)) => {
                        let cols: Vec<Option<String>> =
                            row.iter().map(|v| v.encode_text_with(efd)).collect();
                        writer.data_row(&cols);
                        count += 1;
                        if writer.buffered() >= STREAM_FLUSH_BYTES {
                            writer.flush().await?;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        // Mid-stream: rows have already gone out. The error can
                        // still carry a HINT and a CONTEXT traceback (a routine
                        // body raising on row N), so route it through the same
                        // field builder as every other emission site.
                        writer.error_fields(PgError::from(e).to_fields(None));
                        return Ok(WriteOutcome::Errored);
                    }
                }
            }
            writer.command_complete(&tag.complete(count));
        }
    }
    Ok(WriteOutcome::Completed)
}

/// Emit any NOTICE/WARNING messages that precede a command's CommandComplete, in
/// order. `sql`, when present, resolves a notice's cursor position to a wire
/// character offset (the simple-query path has the text; extended Execute does
/// not, and passes `None`).
fn emit_notices(
    writer: &mut BackendWriter<impl tokio::io::AsyncWrite + Unpin>,
    notices: &[Notice],
    sql: Option<&str>,
) {
    for notice in notices {
        let position = notice
            .location
            .and_then(|(line, col)| sql.map(|s| char_position(s, line, col)));
        writer.notice_fields(notice.to_fields(position));
    }
}

/// Map a value-layer cast/decode failure to a client `ErrorResponse`.
fn cast_error(e: CastError) -> PgError {
    PgError::new(e.sqlstate, e.message)
}

/// The transfer format for column/parameter `i` from a Bind format list: 0
/// entries means all-text, 1 entry applies to every column, otherwise one entry
/// per column — the encoding the `Bind` message uses for both parameter and
/// result formats. The caller must have validated the list length with
/// [`check_format_count`]; otherwise the `_` arm can index out of bounds.
fn format_at(formats: &[Format], i: usize) -> Format {
    match formats.len() {
        0 => Format::Text,
        1 => formats[0],
        _ => formats[i],
    }
}

/// Reject a `Bind` format list whose length is neither 0, 1, nor exactly
/// `count`, before anything indexes it by position. `kind` names the list
/// ("parameter" / "result") for the error, as PG phrases it.
fn check_format_count(len: usize, count: usize, kind: &str) -> Result<(), PgError> {
    if len <= 1 || len == count {
        Ok(())
    } else {
        Err(PgError::new(
            sqlstate::PROTOCOL_VIOLATION,
            format!("bind message has {len} {kind} formats but {count} {kind}s"),
        ))
    }
}

/// Build a `RowDescription`'s fields from a statement's output columns, applying
/// the requested per-column result `formats`. Query results carry no catalog
/// origin or typmod, matching the simple-query path.
fn field_descriptions(cols: &[OutputColumn], formats: &[Format]) -> Vec<FieldDescription> {
    cols.iter()
        .enumerate()
        .map(|(i, c)| FieldDescription {
            name: c.name.clone(),
            table_oid: 0,
            column_id: 0,
            type_oid: c.ty.oid(),
            type_len: c.ty.typlen(),
            type_modifier: -1,
            format: format_at(formats, i),
        })
        .collect()
}

/// Encode one result row's columns in the portal's requested formats. A binary
/// request for a type without a binary output routine is an honest `0A000`.
fn encode_row(
    row: &[Value],
    formats: &[Format],
    efd: i32,
) -> Result<Vec<Option<Vec<u8>>>, PgError> {
    row.iter()
        .enumerate()
        .map(|(i, v)| match format_at(formats, i) {
            Format::Text => Ok(v.encode_text_with(efd).map(String::into_bytes)),
            Format::Binary => v.encode_binary().map_err(cast_error),
        })
        .collect()
}

/// Parse (`P`): parse and analyze one statement into a prepared statement.
/// Extended-query Parse allows at most one command; parse-analysis runs here so
/// Describe and Bind have the parameter types and result shape.
fn handle_parse(
    engine: &Arc<dyn TableEngine>,
    global_catalog: &Arc<GlobalCatalog>,
    session: &mut Session,
    writer: &mut BackendWriter<impl tokio::io::AsyncWrite + Unpin>,
    name: &str,
    query: &str,
    param_oids: &[u32],
) -> Result<(), PgError> {
    let mut statements = crabgresql_parser::parse(query).map_err(PgError::from)?;
    if statements.len() > 1 {
        return Err(PgError::syntax(
            "cannot insert multiple commands into a prepared statement",
        ));
    }
    // A named prepared statement may not be redefined while it exists (PG
    // 42P05); the unnamed statement ("") is always replaced. Check before doing
    // the analysis work.
    if !name.is_empty() && session.prepared.contains_key(name) {
        return Err(PgError::new(
            sqlstate::DUPLICATE_PREPARED_STATEMENT,
            format!("prepared statement \"{name}\" already exists"),
        ));
    }
    // 0 or 1 statement; an empty query string prepares to `None`.
    let stmt = statements.pop();
    // Map declared parameter type OIDs: 0 = unspecified (infer from context),
    // otherwise a built-in type; an unknown OID is an undefined_object, as in PG.
    let declared = param_oids
        .iter()
        .map(|&oid| {
            if oid == 0 {
                Ok(None)
            } else {
                PgType::from_oid(oid).map(Some).ok_or_else(|| {
                    PgError::new(
                        sqlstate::UNDEFINED_OBJECT,
                        format!("type with OID {oid} does not exist"),
                    )
                })
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let (param_types, result_columns) = match &stmt {
        Some(stmt) => {
            let Analyzed {
                param_types,
                result_columns,
            } = analyze_statement(engine, global_catalog, stmt, declared, session)?;
            (param_types, result_columns)
        }
        None => (Vec::new(), None),
    };
    session.prepared.insert(
        name.to_string(),
        PreparedStatement {
            stmt,
            param_types,
            result_columns,
        },
    );
    writer.write(&BackendMessage::ParseComplete);
    Ok(())
}

/// Bind (`B`): create a portal by decoding the parameter values a prepared
/// statement requires and recording the requested result formats.
fn handle_bind(
    session: &mut Session,
    writer: &mut BackendWriter<impl tokio::io::AsyncWrite + Unpin>,
    portal: String,
    statement: &str,
    param_formats: &[Format],
    params: &[Option<Vec<u8>>],
    result_formats: Vec<Format>,
) -> Result<(), PgError> {
    let prepared = session.prepared.get(statement).ok_or_else(|| {
        PgError::new(
            sqlstate::INVALID_SQL_STATEMENT_NAME,
            format!("prepared statement \"{statement}\" does not exist"),
        )
    })?;
    if params.len() != prepared.param_types.len() {
        return Err(PgError::new(
            sqlstate::PROTOCOL_VIOLATION,
            format!(
                "bind message supplies {} parameters, but prepared statement \"{statement}\" requires {}",
                params.len(),
                prepared.param_types.len()
            ),
        ));
    }
    // A format list must be empty (all text), a single entry (applies to all),
    // or exactly one per parameter/column; any other count would index past the
    // list. `format_at` and the result encoders rely on this being checked here.
    check_format_count(param_formats.len(), params.len(), "parameter")?;
    let result_columns = prepared.result_columns.as_ref().map_or(0, Vec::len);
    check_format_count(result_formats.len(), result_columns, "result")?;
    let mut values = Vec::with_capacity(params.len());
    for (i, bytes) in params.iter().enumerate() {
        let ty = prepared.param_types[i];
        let value = match bytes {
            None => Value::Null,
            Some(bytes) => match format_at(param_formats, i) {
                Format::Text => {
                    let text = std::str::from_utf8(bytes).map_err(|_| {
                        PgError::new(
                            sqlstate::INVALID_TEXT_REPRESENTATION,
                            "invalid byte sequence for encoding \"UTF8\"",
                        )
                    })?;
                    wire::decode_text(ty, text).map_err(cast_error)?
                }
                Format::Binary => wire::decode_binary(ty, bytes).map_err(cast_error)?,
            },
        };
        values.push(value);
    }
    session.portals.insert(
        portal,
        Portal {
            statement: statement.to_string(),
            params: values,
            result_formats,
            state: PortalState::Ready,
        },
    );
    writer.write(&BackendMessage::BindComplete);
    Ok(())
}

/// Describe (`D`): report a statement's parameters + result shape, or a portal's
/// result shape. A statement with no result rows answers `NoData`.
fn handle_describe(
    session: &mut Session,
    writer: &mut BackendWriter<impl tokio::io::AsyncWrite + Unpin>,
    target: Target,
    name: &str,
) -> Result<(), PgError> {
    match target {
        Target::Statement => {
            let prepared = session.prepared.get(name).ok_or_else(|| {
                PgError::new(
                    sqlstate::INVALID_SQL_STATEMENT_NAME,
                    format!("prepared statement \"{name}\" does not exist"),
                )
            })?;
            let oids = prepared.param_types.iter().map(|t| t.oid()).collect();
            writer.write(&BackendMessage::ParameterDescription(oids));
            // Before Bind the result formats are unknown, so RowDescription
            // reports text format (0), as PG does for a statement Describe.
            match &prepared.result_columns {
                Some(cols) => writer.write(&BackendMessage::RowDescription(field_descriptions(
                    cols,
                    &[],
                ))),
                None => writer.write(&BackendMessage::NoData),
            }
        }
        Target::Portal => {
            let portal = session.portals.get(name).ok_or_else(|| {
                PgError::new(
                    sqlstate::INVALID_CURSOR_NAME,
                    format!("portal \"{name}\" does not exist"),
                )
            })?;
            let prepared = session.prepared.get(&portal.statement).ok_or_else(|| {
                PgError::new(
                    sqlstate::INTERNAL_ERROR,
                    "portal references a dropped prepared statement",
                )
            })?;
            // A portal's shape is resolved now, not read from the shape Parse
            // recorded. `FETCH` is the case where the two differ: its columns
            // come from a cursor that may have been declared — or closed and
            // redeclared with different columns — since the statement was
            // prepared, and PG re-derives the portal's descriptor at Bind for
            // exactly this reason.
            let columns = match &prepared.stmt {
                Some(ast::Statement::Fetch { name, .. }) => fetch_columns(session, name),
                _ => prepared.result_columns.clone(),
            };
            let formats = &session.portals[name].result_formats;
            match &columns {
                Some(cols) => writer.write(&BackendMessage::RowDescription(field_descriptions(
                    cols, formats,
                ))),
                None => writer.write(&BackendMessage::NoData),
            }
        }
    }
    Ok(())
}

/// Execute (`E`): run a portal, streaming its rows in the requested formats. A
/// row limit (`max_rows > 0`) suspends the portal after that many rows; a later
/// Execute resumes it. Does not send RowDescription — that is Describe's job.
fn handle_execute(
    engine: &Arc<dyn TableEngine>,
    global_catalog: &Arc<GlobalCatalog>,
    txnmgr: &Arc<TransactionManager>,
    session: &mut Session,
    writer: &mut BackendWriter<impl tokio::io::AsyncWrite + Unpin>,
    portal_name: &str,
    max_rows: i32,
) -> Result<(), PgError> {
    let Some(portal) = session.portals.get(portal_name) else {
        return Err(PgError::new(
            sqlstate::INVALID_CURSOR_NAME,
            format!("portal \"{portal_name}\" does not exist"),
        ));
    };
    match &portal.state {
        // A portal a previous row-limited Execute left suspended resumes from its
        // live iterator, without re-running the query.
        PortalState::Suspended(_) => {
            return resume_portal(session, writer, portal_name, max_rows);
        }
        // A finished portal is answered from what it recorded: an exhausted result
        // set re-reports as an empty one, and a statement that produced no result
        // set cannot be run again at all. Either way the statement does not run a
        // second time — for a data-modifying statement that would write twice.
        PortalState::Done { tag } => {
            match tag {
                Some(tag) => writer.command_complete(&tag.complete(0)),
                None => {
                    return Err(PgError::new(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        format!("portal \"{portal_name}\" cannot be run"),
                    ));
                }
            }
            return Ok(());
        }
        PortalState::Ready => {}
    }
    // Fresh execution. Copy out what `execute_statement` needs, since it borrows
    // the whole session; cloning the parsed statement keeps the prepared
    // statement reusable across executions.
    let stmt_name = session.portals[portal_name].statement.clone();
    let param_values = session.portals[portal_name].params.clone();
    let result_formats = session.portals[portal_name].result_formats.clone();
    let (stmt, param_types) = {
        let prepared = session.prepared.get(&stmt_name).ok_or_else(|| {
            PgError::new(
                sqlstate::INTERNAL_ERROR,
                "portal references a dropped prepared statement",
            )
        })?;
        (prepared.stmt.clone(), prepared.param_types.clone())
    };
    let Some(stmt) = stmt else {
        writer.write(&BackendMessage::EmptyQueryResponse);
        return Ok(());
    };
    let efd = session.extra_float_digits;
    let params = BoundParams {
        types: param_types,
        values: param_values,
        extended: true,
    };
    let outcome = execute_statement(engine, global_catalog, txnmgr, &stmt, session, &params);
    // Drained on both arms — see the simple-query path for why this cannot be
    // left to the statement handlers alone.
    let stranded = session.notices.drain();
    let mut result = match outcome {
        Ok(result) => result,
        Err(e) => {
            // PG order: notices raised before the failure, then the error.
            emit_notices(writer, &stranded, None);
            return Err(e);
        }
    };
    result.prepend_notices(stranded);
    stream_execute(
        session,
        writer,
        portal_name,
        result,
        &result_formats,
        efd,
        max_rows,
    )
}

/// Stream a freshly executed portal's result. Rows are encoded per the portal's
/// result formats; a row limit materializes the remainder into the portal and
/// answers PortalSuspended.
fn stream_execute(
    session: &mut Session,
    writer: &mut BackendWriter<impl tokio::io::AsyncWrite + Unpin>,
    portal_name: &str,
    result: QueryResult,
    formats: &[Format],
    efd: i32,
    max_rows: i32,
) -> Result<(), PgError> {
    match result {
        QueryResult::Command { tag, notices } => {
            emit_notices(writer, &notices, None);
            writer.command_complete(&tag);
            // No result set to re-report, so a further Execute is refused rather
            // than re-running the statement.
            finish_portal(session, portal_name, None);
            Ok(())
        }
        QueryResult::Rows {
            columns: _,
            mut node,
            tag,
            notices,
        } => {
            emit_notices(writer, &notices, None);
            let limit = (max_rows > 0).then_some(max_rows as usize);
            let mut count = 0usize;
            loop {
                match node.next() {
                    Ok(Some(row)) => {
                        writer.write(&BackendMessage::DataRow(encode_row(&row, formats, efd)?));
                        count += 1;
                        if limit == Some(count) {
                            // Row budget reached, result not yet exhausted: keep
                            // the live iterator on the portal and resume it on the
                            // next Execute — streaming, not buffering. A result of
                            // exactly `max_rows` rows suspends here and the next
                            // Execute returns `SELECT 0` (a valid PG sequence).
                            if let Some(portal) = session.portals.get_mut(portal_name) {
                                portal.state = PortalState::Suspended(SuspendedRows {
                                    node,
                                    delivered: count,
                                    tag,
                                });
                            }
                            writer.write(&BackendMessage::PortalSuspended);
                            return Ok(());
                        }
                    }
                    Ok(None) => {
                        writer.command_complete(&tag.complete(count));
                        finish_portal(session, portal_name, Some(tag));
                        return Ok(());
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
}

/// Resume a suspended portal: pull up to `max_rows` more rows from its live
/// iterator, then PortalSuspended (still more) or CommandComplete with the
/// running total (exhausted).
fn resume_portal(
    session: &mut Session,
    writer: &mut BackendWriter<impl tokio::io::AsyncWrite + Unpin>,
    portal_name: &str,
    max_rows: i32,
) -> Result<(), PgError> {
    let efd = session.extra_float_digits;
    let formats = session.portals[portal_name].result_formats.clone();
    let limit = if max_rows > 0 {
        max_rows as usize
    } else {
        usize::MAX
    };
    let portal = session.portals.get_mut(portal_name).ok_or_else(|| {
        PgError::new(
            sqlstate::INVALID_CURSOR_NAME,
            format!("portal \"{portal_name}\" does not exist"),
        )
    })?;
    let PortalState::Suspended(sus) = &mut portal.state else {
        return Err(PgError::new("XX000", "portal is not suspended"));
    };
    let mut served = 0usize;
    // `writer` is independent of `session`, so writing while the portal is
    // mutably borrowed is fine.
    let exhausted = loop {
        if served >= limit {
            break false;
        }
        match sus.node.next() {
            Ok(Some(row)) => {
                writer.write(&BackendMessage::DataRow(encode_row(&row, &formats, efd)?));
                sus.delivered += 1;
                served += 1;
            }
            Ok(None) => break true,
            Err(e) => return Err(e.into()),
        }
    };
    let delivered = sus.delivered;
    let tag = sus.tag;
    if exhausted {
        // Finished, not back to Ready: a further Execute re-reports the exhausted
        // result set as empty rather than running the statement again.
        portal.state = PortalState::Done { tag: Some(tag) };
        writer.command_complete(&tag.complete(delivered));
    } else {
        writer.write(&BackendMessage::PortalSuspended);
    }
    Ok(())
}

/// Record that a portal ran to completion, so a further `Execute` is answered
/// from `tag` instead of re-running the statement. `tag` is the result set's
/// command-tag family, or `None` when the statement produced no result set.
fn finish_portal(session: &mut Session, portal_name: &str, tag: Option<RowTag>) {
    if let Some(portal) = session.portals.get_mut(portal_name) {
        portal.state = PortalState::Done { tag };
    }
}
