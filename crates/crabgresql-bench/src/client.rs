//! Talking to the server: connect, run a statement, and stream a raw data file
//! in through `COPY … FROM STDIN`.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result, bail};
use bytes::{Bytes, BytesMut};
use futures_util::SinkExt;
use tokio::sync::mpsc;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

/// How much data one `COPY` statement carries. The server buffers a whole
/// `COPY` payload and materializes it as rows before inserting any of it, so
/// this — not the size of the file — is what bounds memory on both sides.
/// Loading a 70 GB TSV therefore means thousands of batches, not one giant
/// transfer that would OOM the (in-process) server.
const COPY_BATCH_BYTES: usize = 8 * 1024 * 1024;

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

/// Stream `path` into the server as a sequence of `COPY` batches, stopping
/// after `max_rows` data lines when set, and skipping the file's header line
/// when `skip_header`. Returns the number of rows the server accepted.
///
/// The file is read on a blocking thread and handed over one batch at a time,
/// so neither side ever holds more than a couple of [`COPY_BATCH_BYTES`]. Each
/// batch is its own transaction: a load that dies half way leaves the table
/// partially filled, which is why the caller re-counts the table rather than
/// trusting that a table which exists is a table that is fully loaded.
pub async fn copy_file_in(
    client: &Client,
    copy_sql: &str,
    path: &Path,
    max_rows: Option<u64>,
    skip_header: bool,
) -> Result<u64> {
    let (tx, mut rx) = mpsc::channel::<Bytes>(2);
    let owned = path.to_path_buf();
    let reader =
        tokio::task::spawn_blocking(move || read_batches(&owned, max_rows, skip_header, &tx));

    let mut accepted = 0u64;
    let mut failure = None;
    while let Some(batch) = rx.recv().await {
        match copy_batch(client, copy_sql, batch).await {
            Ok(rows) => accepted += rows,
            // Stop at the first failure instead of pushing the rest of the
            // file at a server that has already rejected it.
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    // Dropping the receiver releases the reader if it is parked on a full
    // channel, so the join below cannot hang.
    drop(rx);
    let sent = reader.await.context("copy reader thread panicked")?;

    if let Some(e) = failure {
        return Err(e);
    }
    let sent = sent?;
    if sent != accepted {
        bail!("sent {sent} rows but the server accepted {accepted}");
    }
    Ok(accepted)
}

/// Run one `COPY … FROM STDIN` carrying `data`, returning the rows accepted.
async fn copy_batch(client: &Client, copy_sql: &str, data: Bytes) -> Result<u64> {
    let sink = client
        .copy_in::<_, Bytes>(copy_sql)
        .await
        .with_context(|| format!("starting `{copy_sql}`"))?;
    let mut sink = Box::pin(sink);
    // The sink only fails once the connection is gone, and `finish()` then
    // carries the server's own error, so let that be the one reported.
    let _ = sink.send(data).await;
    sink.as_mut().finish().await.context("COPY failed")
}

/// Read `path` on a blocking thread, batching whole lines. Returns the number
/// of data lines handed over — the caller reconciles it against the rows the
/// server accepted, so a short read cannot pass as a complete load.
fn read_batches(
    path: &Path,
    max_rows: Option<u64>,
    skip_header: bool,
    tx: &mpsc::Sender<Bytes>,
) -> Result<u64> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    // One buffer for the whole file: a fresh Vec per line is an allocation per
    // line, and the loader's throughput is a number this tool reports.
    let mut line = Vec::new();
    let mut batch = BytesMut::with_capacity(COPY_BATCH_BYTES);
    let mut rows = 0u64;

    // The header is dropped here rather than by `COPY … HEADER`, which would
    // eat the first data line of every batch after the first.
    if skip_header {
        reader.read_until(b'\n', &mut line)?;
    }

    loop {
        if max_rows.is_some_and(|max| rows >= max) {
            break;
        }
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        // A final line without a newline still has to reach the server as one.
        if !line.ends_with(b"\n") {
            line.push(b'\n');
        }
        batch.extend_from_slice(&line);
        rows += 1;
        if batch.len() >= COPY_BATCH_BYTES && tx.blocking_send(batch.split().freeze()).is_err() {
            return Ok(rows);
        }
    }
    if !batch.is_empty() && tx.blocking_send(batch.freeze()).is_err() {
        return Ok(rows);
    }
    Ok(rows)
}

/// Run a statement that returns no rows, failing loudly if it errors.
pub async fn execute(client: &Client, sql: &str) -> Result<()> {
    client
        .batch_execute(sql)
        .await
        .with_context(|| format!("running `{}`", sql.lines().next().unwrap_or(sql)))?;
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

/// How many rows `table` holds. A table that exists is not necessarily a table
/// that is fully loaded, so every run reports this rather than assuming.
pub async fn count_rows(client: &Client, table: &str) -> Result<u64> {
    let messages = client
        .simple_query(&format!("SELECT count(*) FROM {table}"))
        .await
        .with_context(|| format!("counting rows in `{table}`"))?;
    for message in messages {
        if let SimpleQueryMessage::Row(row) = message
            && let Some(count) = row.get(0)
        {
            return count
                .parse()
                .with_context(|| format!("parsing count(*) result `{count}`"));
        }
    }
    bail!("count(*) on `{table}` returned no row")
}
