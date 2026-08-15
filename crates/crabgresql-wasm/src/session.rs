//! An embedded database driven over an in-memory pipe.
//!
//! There are no sockets on wasm, but the protocol layer is where all the
//! behavior lives — statement analysis, transaction state, GUCs, error fields,
//! command tags, the text encoding of every value. So instead of reaching past
//! it into the executor, this drives a real session over
//! [`tokio::io::duplex`]: pgwire messages in one end, pgwire messages out the
//! other, with the same [`handle_session`] the TCP server runs.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crabgresql_pg_wire::{
    BackendMessage, BackendReader, FrontendMessage, FrontendWriter, ProtocolError, StartupRequest,
};
use crabgresql_server::{GlobalCatalog, handle_session, open_pg_engine};
use crabgresql_storage_api::TableEngine;
use tokio::io::{DuplexStream, ReadHalf, WriteHalf};

use crate::json::{ExecOutput, StatementResult, error_to_json};

/// How much of the pipe may be in flight before a writer waits for the reader.
///
/// Only a buffer, not a limit: both ends run on the same runtime, so a full
/// pipe parks the writer until the other side drains it. Sized to hold a
/// typical result set in one pass rather than to bound anything.
const PIPE_CAPACITY: usize = 64 * 1024;

/// The database and its one session.
///
/// One resource is one session *and* one open data directory, because opening
/// the same directory twice would mean two WALs writing the same log. An
/// embedder that wants concurrent sessions wants a different design (a shared
/// engine behind several pipes), not a second `Database`.
pub struct Database {
    runtime: tokio::runtime::Runtime,
    engine: Arc<dyn TableEngine>,
    to_server: FrontendWriter<WriteHalf<DuplexStream>>,
    from_server: BackendReader<ReadHalf<DuplexStream>>,
}

/// What went wrong at the level of the embedding, not of a statement: a
/// statement error is data (see [`error_to_json`]), this is not.
#[derive(Debug)]
pub enum EmbedError {
    /// The data directory could not be opened or recovered.
    Open(std::io::Error),
    /// The session ended, or spoke something that is not the protocol. Once
    /// this happens the `Database` is unusable — the session task is gone.
    Protocol(String),
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::Open(error) => write!(f, "could not open the data directory: {error}"),
            EmbedError::Protocol(message) => write!(f, "the session ended: {message}"),
        }
    }
}

impl From<ProtocolError> for EmbedError {
    fn from(error: ProtocolError) -> Self {
        EmbedError::Protocol(error.to_string())
    }
}

impl Database {
    /// Open (creating it if needed) and recover `data_dir`, then start a
    /// session on it.
    pub fn open(data_dir: &Path) -> Result<Database, EmbedError> {
        std::fs::create_dir_all(data_dir).map_err(EmbedError::Open)?;
        let (engine, txnmgr) = open_pg_engine(data_dir).map_err(EmbedError::Open)?;

        // Current-thread on purpose: wasm has one thread, and the session task
        // only ever needs to run while `block_on` below is waiting on it.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .map_err(EmbedError::Open)?;

        let (client, server) = tokio::io::duplex(PIPE_CAPACITY);
        let (server_read, server_write) = tokio::io::split(server);
        let (client_read, client_write) = tokio::io::split(client);

        let catalog = Arc::new(GlobalCatalog::new());
        {
            let engine = Arc::clone(&engine);
            // The session ending is not an error here: the client half is
            // dropped with the `Database`, and a closed pipe is how it learns.
            runtime.spawn(async move {
                if let Err(error) =
                    handle_session(server_read, server_write, engine, catalog, txnmgr).await
                {
                    tracing::debug!(%error, "the embedded session ended");
                }
            });
        }

        let mut db = Database {
            runtime,
            engine,
            to_server: FrontendWriter::new(client_write),
            from_server: BackendReader::new(client_read),
        };
        db.start_up()?;
        Ok(db)
    }

    /// Send the startup packet and read the server's opening burst
    /// (authentication, ParameterStatus, BackendKeyData) up to ReadyForQuery.
    ///
    /// The identity is fixed: there is no authentication to speak of, and an
    /// embedded database has exactly one user. `postgres`/`postgres` is what
    /// every client library defaults to, so what the component reports back in
    /// `current_user` matches what an embedder would have guessed.
    fn start_up(&mut self) -> Result<(), EmbedError> {
        let params = HashMap::from([
            ("user".to_string(), "postgres".to_string()),
            ("database".to_string(), "postgres".to_string()),
        ]);
        self.to_server
            .write_startup(&StartupRequest::Startup { params });
        let runtime = &self.runtime;
        let to_server = &mut self.to_server;
        let from_server = &mut self.from_server;
        runtime.block_on(async {
            to_server.flush().await?;
            loop {
                match from_server.read_message().await? {
                    Some(BackendMessage::ReadyForQuery(_)) => return Ok(()),
                    Some(_) => {}
                    None => {
                        return Err(EmbedError::Protocol(
                            "the session closed during startup".to_string(),
                        ));
                    }
                }
            }
        })
    }

    /// Run a simple query and collect everything it produced.
    ///
    /// `Ok(Err(json))` is a *statement* error — the server said no, and the
    /// caller gets the SQLSTATE and message as data. `Err` is the session
    /// itself failing, which is not recoverable.
    pub fn exec(&mut self, sql: &str) -> Result<Result<ExecOutput, String>, EmbedError> {
        self.to_server
            .write_message(&FrontendMessage::Query(sql.to_string()));
        let runtime = &self.runtime;
        let to_server = &mut self.to_server;
        let from_server = &mut self.from_server;
        runtime.block_on(async {
            to_server.flush().await?;
            let mut output = ExecOutput::default();
            let mut current = StatementResult::default();
            // The first error wins, matching a simple query: PG abandons the
            // rest of the message after one, so anything that follows is the
            // wind-down and not a second failure.
            let mut failure: Option<String> = None;
            loop {
                let Some(message) = from_server.read_message().await? else {
                    return Err(EmbedError::Protocol(
                        "the session closed mid-query".to_string(),
                    ));
                };
                match message {
                    BackendMessage::RowDescription(fields) => {
                        current.columns = fields.into_iter().map(|field| field.name).collect();
                    }
                    BackendMessage::DataRow(values) => {
                        current.rows.push(
                            values
                                .into_iter()
                                .map(|value| value.map(|bytes| decode_text(&bytes)))
                                .collect(),
                        );
                    }
                    BackendMessage::CommandComplete(tag) => {
                        current.command = tag;
                        output.results.push(std::mem::take(&mut current));
                    }
                    // An empty statement (`;;`) still occupies a slot, so the
                    // results line up one-to-one with what was submitted.
                    BackendMessage::EmptyQueryResponse => {
                        output.results.push(std::mem::take(&mut current));
                    }
                    BackendMessage::NoticeResponse(fields) => {
                        output.notices.push(error_to_json(&fields));
                    }
                    BackendMessage::ErrorResponse(fields) => {
                        failure.get_or_insert_with(|| error_to_json(&fields));
                    }
                    // Whatever transaction state the session is left in is
                    // carried by the session itself, not reported per call.
                    BackendMessage::ReadyForQuery(_) => break,
                    _ => {}
                }
            }
            Ok(match failure {
                Some(error) => Err(error),
                None => Ok(output),
            })
        })
    }

    /// Flush everything to the data directory and record a clean shutdown, so
    /// the directory is complete for anyone who copies it out of the host's
    /// filesystem. There is no background flush worker on wasm, so this is the
    /// only thing that makes buffered rows reachable without a replay.
    pub fn flush(&self) {
        self.engine.shutdown();
    }
}

/// A text-format value as the wire carries it.
///
/// Lossy because the type is `bytes` on the wire and the encoding is the
/// server's `client_encoding` — which is UTF-8 here, so the replacement
/// character can only appear if a value was mis-encoded upstream, and losing
/// the query to that would be worse than showing it.
fn decode_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_temp() -> anyhow::Result<(tempfile::TempDir, Database)> {
        let dir = tempfile::tempdir()?;
        let db = Database::open(&dir.path().join("pgdata")).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok((dir, db))
    }

    #[test]
    fn a_query_returns_columns_rows_and_a_tag() -> anyhow::Result<()> {
        let (_dir, mut db) = open_temp()?;
        let output = db
            .exec("select 1 as one, null::text as nothing")
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(
            output.to_json(),
            r#"{"results":[{"columns":["one","nothing"],"rows":[["1",null]],"command":"SELECT 1"}],"notices":[]}"#
        );
        Ok(())
    }

    /// Several statements in one message produce several results, in order —
    /// which is the whole reason `results` is a list.
    #[test]
    fn a_multi_statement_message_reports_each_statement() -> anyhow::Result<()> {
        let (_dir, mut db) = open_temp()?;
        let output = db
            .exec("create table t(a int); insert into t values (1),(2); select a from t order by a")
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let tags: Vec<&str> = output
            .results
            .iter()
            .map(|result| result.command.as_str())
            .collect();
        assert_eq!(tags, ["CREATE TABLE", "INSERT 0 2", "SELECT 2"]);
        assert_eq!(
            output.results[2].rows,
            vec![vec![Some("1".to_string())], vec![Some("2".to_string())],]
        );
        Ok(())
    }

    /// A failing statement is data, not a broken session: the SQLSTATE comes
    /// back and the next `exec` still works.
    #[test]
    fn an_error_is_reported_and_the_session_survives() -> anyhow::Result<()> {
        let (_dir, mut db) = open_temp()?;
        let error = db
            .exec("select * from no_such_table")
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .expect_err("a missing relation must be an error");
        assert!(
            error.contains(r#""sqlstate":"42P01""#),
            "expected an undefined_table SQLSTATE, got {error}"
        );

        let output = db
            .exec("select 2")
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(output.results[0].rows, vec![vec![Some("2".to_string())]]);
        Ok(())
    }

    /// Session state has to outlive one call, or `BEGIN` in one `exec` and
    /// `COMMIT` in the next would be two different transactions.
    #[test]
    fn transaction_state_spans_calls() -> anyhow::Result<()> {
        let (_dir, mut db) = open_temp()?;
        for sql in [
            "create table t(a int)",
            "begin",
            "insert into t values (1)",
            "rollback",
        ] {
            db.exec(sql)
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        let output = db
            .exec("select count(*) from t")
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(
            output.results[0].rows,
            vec![vec![Some("0".to_string())]],
            "the rolled-back insert must not be visible"
        );
        Ok(())
    }

    /// A result set larger than the pipe would deadlock if either side could
    /// only make progress while the other was idle.
    #[test]
    fn a_result_set_larger_than_the_pipe_comes_back_whole() -> anyhow::Result<()> {
        let (_dir, mut db) = open_temp()?;
        db.exec("create table wide(a int, pad text)")
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        db.exec("insert into wide select g, repeat('x', 200) from generate_series(1, 2000) g")
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let output = db
            .exec("select a, pad from wide order by a")
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(output.results[0].rows.len(), 2000);
        Ok(())
    }
}
