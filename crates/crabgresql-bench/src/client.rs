//! Talking to the server: connect, run a statement, and stream a raw data file
//! in through `COPY … FROM STDIN`.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bytes::{Bytes, BytesMut};
use futures_util::SinkExt;
use tokio_postgres::{Client, NoTls};

/// How much data one `CopyData` frame carries. Big enough that the per-frame
/// overhead disappears, small enough that a loader never holds much more than
/// a few of these in flight.
const COPY_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// Connect and spawn the connection task that drives the socket. The task ends
/// when the returned client is dropped.
pub async fn connect(conninfo: &str) -> Result<Client> {
    let (client, connection) = tokio_postgres::connect(conninfo, NoTls)
        .await
        .with_context(|| format!("connecting to `{conninfo}`"))?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("bench: connection closed: {e}");
        }
    });
    Ok(client)
}

/// Stream `path` into the server through `copy_sql`, stopping after
/// `max_rows` lines when set. Returns the number of rows the server accepted.
///
/// The file is read on a blocking thread and handed over in chunks, so a
/// 70 GB TSV never lands in memory; only a few [`COPY_CHUNK_BYTES`] buffers do.
pub async fn copy_file_in(
    client: &Client,
    copy_sql: &str,
    path: &Path,
    max_rows: Option<u64>,
) -> Result<u64> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Bytes>>(2);
    let path = path.to_path_buf();
    let reader = tokio::task::spawn_blocking(move || read_chunks(&path, max_rows, &tx));

    let sink = client
        .copy_in::<_, Bytes>(copy_sql)
        .await
        .with_context(|| format!("starting `{copy_sql}`"))?;
    let mut sink = Box::pin(sink);
    let mut copy_error = None;
    while let Some(chunk) = rx.recv().await {
        let chunk = chunk?;
        // Stop pulling from the reader on a server-side error, but keep the
        // channel draining so the blocking thread is never left wedged on a
        // full channel; the real error is reported after `finish()`.
        if copy_error.is_none()
            && let Err(e) = sink.send(chunk).await
        {
            copy_error = Some(e);
        }
    }
    reader.await.context("copy reader thread panicked")??;

    let rows = sink.as_mut().finish().await;
    match (copy_error, rows) {
        (_, Ok(rows)) => Ok(rows),
        (Some(e), Err(_)) | (None, Err(e)) => Err(e).context("COPY failed"),
    }
}

/// Read `path` line by line on a blocking thread, batching whole lines into
/// chunks. Sending stops early if the receiver is gone (the copy failed).
fn read_chunks(
    path: &PathBuf,
    max_rows: Option<u64>,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes>>,
) -> Result<()> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) => {
            let _ = tx.blocking_send(Err(
                anyhow::Error::new(e).context(format!("opening {}", path.display()))
            ));
            return Ok(());
        }
    };
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let mut chunk = BytesMut::with_capacity(COPY_CHUNK_BYTES);
    let mut rows = 0u64;
    loop {
        if max_rows.is_some_and(|max| rows >= max) {
            break;
        }
        let mut line = Vec::new();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        // A final line without a newline still has to reach the server as one.
        if !line.ends_with(b"\n") {
            line.push(b'\n');
        }
        chunk.extend_from_slice(&line);
        rows += 1;
        if chunk.len() >= COPY_CHUNK_BYTES && tx.blocking_send(Ok(chunk.split().freeze())).is_err()
        {
            return Ok(());
        }
    }
    if !chunk.is_empty() {
        let _ = tx.blocking_send(Ok(chunk.freeze()));
    }
    Ok(())
}

/// Run a statement that returns no rows, failing loudly if it errors.
pub async fn execute(client: &Client, sql: &str) -> Result<()> {
    client
        .batch_execute(sql)
        .await
        .with_context(|| format!("running `{}`", first_line(sql)))?;
    Ok(())
}

/// True if `table` exists in the current search path.
pub async fn table_exists(client: &Client, table: &str) -> Result<bool> {
    let rows = client
        .simple_query(&format!("SELECT 1 FROM {table} LIMIT 0"))
        .await;
    match rows {
        Ok(_) => Ok(true),
        Err(e) => match e.as_db_error().map(|db| db.code().code().to_string()) {
            // 42P01 undefined_table
            Some(code) if code == "42P01" => Ok(false),
            _ => bail!(e),
        },
    }
}

fn first_line(sql: &str) -> &str {
    sql.lines().next().unwrap_or(sql)
}
