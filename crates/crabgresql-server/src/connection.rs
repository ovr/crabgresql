//! One client connection: startup handshake, then the simple-query loop.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use crabgresql_pg_wire::{
    BackendWriter, FieldDescription, FrontendMessage, FrontendReader, ProtocolError,
    StartupRequest, TransactionStatus, sqlstate,
};
use crabgresql_storage_api::TableEngine;
use crabgresql_txn::TransactionManager;
use tokio::net::TcpStream;

use crate::global_catalog::GlobalCatalog;
use crate::query::{NoticeSeverity, QueryResult, execute_statement};
use crate::session::Session;

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
        database,
        user,
        format!("pg_temp_{backend_id}"),
    );

    // After an error on an extended-protocol message, PG discards everything
    // until Sync and only then sends ReadyForQuery — one error, one RFQ per
    // Parse..Sync batch, or the driver's state machine desyncs.
    let mut skip_until_sync = false;
    loop {
        match reader.read_message().await? {
            None | Some(FrontendMessage::Terminate) => return Ok(()),
            Some(FrontendMessage::Sync) => {
                skip_until_sync = false;
                writer.ready_for_query(session.tx_status);
                writer.flush().await?;
            }
            Some(_) if skip_until_sync => {}
            Some(FrontendMessage::Query(sql)) => {
                run_simple_query(&sql, &engine, &catalog, &txnmgr, &mut session, &mut writer)
                    .await?;
                writer.ready_for_query(session.tx_status);
                writer.flush().await?;
            }
            // The codec decodes the extended-query, COPY and function-call
            // messages, but the engine only runs the simple-query protocol, so
            // every other frontend message is answered like an unsupported one.
            Some(other) => {
                writer.error_response(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    &format!(
                        "protocol message '{}' is not supported yet (only the simple query protocol is implemented)",
                        other.tag() as char
                    ),
                );
                writer.flush().await?;
                // An error inside a block aborts it: the next Sync must report 'E'.
                mark_transaction_failed(&mut session);
                skip_until_sync = true;
            }
        }
    }
}

/// Flush threshold while streaming rows: bounds server memory and gets first
/// rows onto the wire early instead of buffering the whole result set.
const STREAM_FLUSH_BYTES: usize = 8 * 1024;

/// One `Query` message: parse, run every statement, stream the responses.
/// An execution error aborts the remaining statements, as in PG, and — inside
/// an explicit transaction block — puts the session in the failed state so the
/// next ReadyForQuery reports `E`. (PG also rolls back the earlier statements'
/// data effects; that undo needs the M2 transaction engine.)
async fn run_simple_query(
    sql: &str,
    engine: &Arc<dyn TableEngine>,
    catalog: &Arc<GlobalCatalog>,
    txnmgr: &Arc<TransactionManager>,
    session: &mut Session,
    writer: &mut BackendWriter<impl tokio::io::AsyncWrite + Unpin>,
) -> Result<(), ProtocolError> {
    let statements = match crabgresql_parser::parse(sql) {
        Ok(statements) => statements,
        Err(e) => {
            writer.error_response(sqlstate::SYNTAX_ERROR, &e.to_string());
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
        match execute_statement(engine, catalog, txnmgr, stmt, session) {
            Ok(result) => {
                if write_result(writer, result, efd, sql).await? == WriteOutcome::Errored {
                    mark_transaction_failed(session);
                    return Ok(());
                }
            }
            Err(e) => {
                let position = e.location.map(|(line, col)| char_position(sql, line, col));
                writer.error_response_detailed(
                    e.code,
                    &e.message,
                    e.detail.as_deref(),
                    e.hint.as_deref(),
                    position,
                );
                mark_transaction_failed(session);
                return Ok(());
            }
        }
    }
    Ok(())
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
            for notice in &notices {
                match notice.severity {
                    NoticeSeverity::Warning => {
                        writer.warning_response(notice.code, &notice.message)
                    }
                    NoticeSeverity::Notice => writer.notice_response(
                        notice.code,
                        &notice.message,
                        notice.detail.as_deref(),
                        notice
                            .location
                            .map(|(line, col)| char_position(sql, line, col)),
                    ),
                }
            }
            writer.command_complete(&tag);
        }
        QueryResult::Rows { columns, mut node } => {
            let fields: Vec<FieldDescription> = columns
                .iter()
                .map(|c| FieldDescription::new(c.name.clone(), c.ty.oid(), c.ty.typlen()))
                .collect();
            writer.row_description(&fields);
            let mut count = 0u64;
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
                        writer.error_response_full(e.code, &e.message, e.detail.as_deref(), None);
                        return Ok(WriteOutcome::Errored);
                    }
                }
            }
            writer.command_complete(&format!("SELECT {count}"));
        }
    }
    Ok(WriteOutcome::Completed)
}
