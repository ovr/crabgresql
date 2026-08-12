//! The execution stream: what every node hands up, and how a synchronous caller
//! meets one.
//!
//! Two changes from the Volcano `next() -> Option<Tuple>` this replaces:
//!
//! * **Chunks, not tuples.** A row node yields a [`RowChunk`] — up to
//!   [`ROW_CHUNK`] tuples — so the per-row cost of a virtual call and a
//!   `Result<Option<..>>` is paid once per chunk instead. It also lines the row
//!   path up with the columnar one: [`crate::vector::Shred`] turns one
//!   `RecordBatch` into exactly one chunk rather than shredding a row at a time.
//! * **Streams, not iterators.** A node is a [`Stream`], so a leaf that has to
//!   block — every storage scan today — can hand that work to
//!   `spawn_blocking` instead of holding a tokio worker while the disk answers.
//!
//! # Chunk size is an aim, not an invariant
//!
//! [`ROW_CHUNK`] is what a source *tries* to produce. A filter yields fewer, a
//! set-returning projection may yield more, and the last chunk of a stream is
//! whatever is left. Nothing may assume a chunk's length; an empty chunk means
//! "nothing here", never "nothing left" (only the end of the stream means that).
//!
//! # Order is still the executor's contract
//!
//! Chunking regroups rows, it never reorders them, and a node evaluates each
//! row's expressions exactly once — so a volatile function fires as often, and
//! in the same order, as it did row-at-a-time.
//!
//! # The synchronous bridge
//!
//! Expression evaluation ([`crate::eval`]) is synchronous and re-enters
//! execution: a correlated subquery re-runs its subplan per outer row, and a
//! PL/pgSQL body runs whole statements. Those callers meet a stream through
//! [`drain_rows`] / [`first_row`], which park the current thread until the
//! stream finishes.
//!
//! That is exactly as blocking as the code it replaces, with one hazard worth
//! naming: a leaf reached *under* a bridge must not hand its work to the
//! blocking pool, or a nest of bridges could hold every worker waiting on a pool
//! it is also competing for. [`block_on`] marks the thread while a bridge runs
//! and [`blocking_rows`] pulls inline when it sees the mark.

use std::cell::Cell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use arrow_array::RecordBatch;
use async_stream::try_stream;
use futures_core::Stream;
use futures_util::StreamExt;

use crabgresql_storage_api::{StorageError, Tuple};

use crate::ExecError;

/// How many tuples a source aims to put in one chunk.
///
/// Large enough that the per-chunk overhead (a poll, a `spawn_blocking` hop on a
/// storage leaf) disappears against the per-row work, small enough that a
/// `LIMIT 1` over a huge relation does not read the whole first chunk's worth of
/// pages before answering. It matches the batch sizes the columnar path already
/// works in, so a shredded batch needs no regrouping.
pub const ROW_CHUNK: usize = 1024;

/// The unit a row stream yields: tuples in emission order.
pub type RowChunk = Vec<Tuple>;

/// A row-producing execution node.
pub type RowStream = Pin<Box<dyn Stream<Item = Result<RowChunk, ExecError>> + Send>>;

/// A batch-producing (columnar) execution node — [`RowStream`]'s twin, and the
/// reason `Shred` exists.
pub type BatchStream = Pin<Box<dyn Stream<Item = Result<RecordBatch, ExecError>> + Send>>;

/// A synchronous storage scan, as the engines hand one out.
type TupleIter = Box<dyn Iterator<Item = Result<Tuple, StorageError>> + Send>;

thread_local! {
    /// Set while a synchronous bridge ([`drain_rows`], [`first_row`]) drives a
    /// stream on this thread. See the module docs: it keeps a leaf underneath
    /// such a bridge off the blocking pool.
    static IN_SYNC_BRIDGE: Cell<bool> = const { Cell::new(false) };

    /// The waker [`block_on`] polls with, one per thread.
    static PARK_WAKER: Waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
}

/// Whether work on this thread may go to the blocking pool: only with a runtime
/// to hand it to, and only outside a synchronous bridge.
fn offload_allowed() -> bool {
    !IN_SYNC_BRIDGE.with(Cell::get) && tokio::runtime::Handle::try_current().is_ok()
}

/// Run `future` to completion on this thread, marking it as bridging for as
/// long as it runs.
///
/// Its own driver rather than `futures_executor::block_on`, because bridges
/// **nest**: a query drained by one bridge can evaluate a correlated subquery,
/// whose subplan is drained by another. `futures_executor` guards against
/// re-entering its `LocalPool` and panics on exactly that shape; parking the
/// thread has no such state to re-enter.
///
/// A correlated subquery bridges once per outer row, so the waker is built once
/// per thread rather than per bridge — it only ever unparks its own thread, which
/// is what makes one instance reusable.
fn block_on<F: Future>(future: F) -> F::Output {
    let previous = IN_SYNC_BRIDGE.with(|flag| flag.replace(true));
    let mut future = std::pin::pin!(future);
    let out = PARK_WAKER.with(|waker| {
        let mut cx = Context::from_waker(waker);
        loop {
            if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                break out;
            }
            // `park` may return spuriously, which the loop's re-poll absorbs.
            std::thread::park();
        }
    });
    IN_SYNC_BRIDGE.with(|flag| flag.set(previous));
    out
}

/// Unparks the thread that built it.
struct ThreadWaker(std::thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Replay already-computed rows, in [`ROW_CHUNK`]-sized pieces.
///
/// `RETURNING` projects eagerly and streams the finished rows through this, as
/// does a materialized portal.
pub fn rows_once(rows: Vec<Tuple>) -> RowStream {
    Box::pin(try_stream! {
        let mut rows = rows;
        while rows.len() > ROW_CHUNK {
            let rest = rows.split_off(ROW_CHUNK);
            yield rows;
            rows = rest;
        }
        if !rows.is_empty() {
            yield rows;
        }
    })
}

/// Split materialized rows into chunks. A helper for nodes that buffer their
/// whole input (sort, aggregate, window) and then emit it.
pub fn chunks_of(rows: Vec<Tuple>) -> impl Iterator<Item = RowChunk> {
    let mut rows = rows;
    std::iter::from_fn(move || {
        if rows.is_empty() {
            return None;
        }
        let take = rows.len().min(ROW_CHUNK);
        let rest = rows.split_off(take);
        Some(std::mem::replace(&mut rows, rest))
    })
}

/// How long a pull may take on the reactor before the next one is handed to the
/// blocking pool.
///
/// A scan that is answered from memory takes microseconds, and moving such a
/// pull to another thread costs more than it saves — a task dispatch and a
/// thread handoff per chunk, which on a scan of a few hundred thousand rows is
/// measurable against the query. A scan that is answered from disk takes
/// milliseconds, and holding a tokio worker for that is exactly what starves
/// every other connection on it.
///
/// So the choice is made from what the scan has actually done: the first pull
/// runs inline and every later pull follows the previous one's verdict. Being
/// wrong is bounded either way — one slow pull on the reactor, or one fast pull
/// on the pool.
const OFFLOAD_AFTER: std::time::Duration = std::time::Duration::from_millis(1);

/// Drive a synchronous storage scan as a row stream, one chunk per pull.
///
/// The scan blocks — it reads pages, decodes tuples, waits on a buffer pool — so
/// a pull that proves slow moves to the blocking pool, handing the iterator over
/// and taking it back with the rows (see [`OFFLOAD_AFTER`]). Without a runtime
/// (unit tests) or under a synchronous bridge every pull runs inline: the work
/// is identical, only the thread differs.
pub fn blocking_rows(iter: TupleIter) -> RowStream {
    Box::pin(try_stream! {
        let mut held = Some(iter);
        let mut offload = false;
        loop {
            let mut iter = match held.take() {
                Some(iter) => iter,
                // Unreachable: the iterator is put back at the end of every
                // pass, and the loop exits rather than continuing without it.
                None => break,
            };
            let (iter, chunk) = if offload && offload_allowed() {
                spawn_blocking(move || {
                    let chunk = pull_chunk(&mut iter);
                    (iter, chunk)
                })
                .await?
            } else {
                let started = std::time::Instant::now();
                let chunk = pull_chunk(&mut iter);
                offload = started.elapsed() >= OFFLOAD_AFTER;
                (iter, chunk)
            };
            held = Some(iter);
            let chunk = chunk?;
            // A short chunk means the iterator ran dry inside `pull_chunk`, so
            // there is nothing to come back for.
            let last = chunk.len() < ROW_CHUNK;
            if !chunk.is_empty() {
                yield chunk;
            }
            if last {
                break;
            }
        }
    })
}

/// Drive a synchronous batch scan as a batch stream, one batch per pull, on the
/// same terms as [`blocking_rows`].
pub fn blocking_batches(iter: crabgresql_storage_api::BatchStream) -> BatchStream {
    Box::pin(try_stream! {
        let mut held = Some(iter);
        let mut offload = false;
        loop {
            let mut iter = match held.take() {
                Some(iter) => iter,
                None => break,
            };
            let (iter, batch) = if offload && offload_allowed() {
                spawn_blocking(move || {
                    let batch = iter.next().transpose();
                    (iter, batch)
                })
                .await?
            } else {
                let started = std::time::Instant::now();
                let batch = iter.next().transpose();
                offload = started.elapsed() >= OFFLOAD_AFTER;
                (iter, batch)
            };
            held = Some(iter);
            match batch.map_err(ExecError::from)? {
                Some(batch) => yield batch,
                None => break,
            }
        }
    })
}

/// Pull up to [`ROW_CHUNK`] tuples, stopping early at the end of the scan or at
/// the first error. Rows read before an error are dropped with it: the error
/// aborts the statement, so nothing downstream would have used them.
fn pull_chunk(iter: &mut TupleIter) -> Result<RowChunk, StorageError> {
    let mut chunk = Vec::with_capacity(ROW_CHUNK);
    for row in iter.by_ref().take(ROW_CHUNK) {
        chunk.push(row?);
    }
    Ok(chunk)
}

/// Run `work` on the blocking pool. A panic inside it (or a runtime shutting
/// down mid-statement) surfaces as an internal error rather than a lost stream.
async fn spawn_blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ExecError> {
    tokio::task::spawn_blocking(work).await.map_err(|error| {
        ExecError::new(
            crabgresql_pg_wire::sqlstate::INTERNAL_ERROR,
            format!("execution task failed: {error}"),
        )
    })
}

/// Drain a stream to its rows, blocking the current thread.
///
/// The bridge for the synchronous callers named in the module docs. Everything
/// beneath it runs inline (see [`in_sync_bridge`]), so this parks on work that
/// was already going to happen on this thread.
pub fn drain_rows(stream: RowStream) -> Result<Vec<Tuple>, ExecError> {
    block_on(async move {
        let mut stream = stream;
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.extend(chunk?);
        }
        Ok(out)
    })
}

/// The first row of a stream, or `None` if it has none — for `EXISTS`, which
/// wants existence rather than rows. Stops at the first chunk, so the rest of
/// the plan never runs.
pub fn first_row(stream: RowStream) -> Result<Option<Tuple>, ExecError> {
    block_on(async move {
        let mut stream = stream;
        while let Some(chunk) = stream.next().await {
            if let Some(row) = chunk?.into_iter().next() {
                return Ok(Some(row));
            }
        }
        Ok(None)
    })
}

/// A row stream a caller can take an exact number of rows from.
///
/// The wire protocol asks for row counts that have nothing to do with chunk
/// boundaries — an extended-protocol `Execute` with `max_rows`, a `FETCH n` —
/// so this keeps the tail of a part-consumed chunk and serves the next request
/// from it. A suspended portal holds one of these across `Execute` round trips.
pub struct RowCursor {
    stream: RowStream,
    /// Rows of the current chunk not yet handed out.
    pending: VecDeque<Tuple>,
    /// The stream said it was done; only `pending` is left.
    done: bool,
}

impl RowCursor {
    pub fn new(stream: RowStream) -> Self {
        Self {
            stream,
            pending: VecDeque::new(),
            done: false,
        }
    }

    /// Rows already pulled but not yet handed out — what a caller must consume
    /// before the stream is asked for more.
    pub fn buffered(&self) -> usize {
        self.pending.len()
    }

    /// The next row, or `None` at the end of the stream.
    pub async fn next_row(&mut self) -> Result<Option<Tuple>, ExecError> {
        loop {
            if let Some(row) = self.pending.pop_front() {
                return Ok(Some(row));
            }
            if !self.fill().await? {
                return Ok(None);
            }
        }
    }

    /// Up to `n` rows. A short answer means the stream is exhausted.
    pub async fn take(&mut self, n: usize) -> Result<Vec<Tuple>, ExecError> {
        let mut out = Vec::new();
        while out.len() < n {
            let want = n - out.len();
            if self.pending.is_empty() && !self.fill().await? {
                break;
            }
            let take = want.min(self.pending.len());
            out.extend(self.pending.drain(..take));
        }
        Ok(out)
    }

    /// Every remaining row.
    pub async fn collect(&mut self) -> Result<Vec<Tuple>, ExecError> {
        let mut out: Vec<Tuple> = self.pending.drain(..).collect();
        while let Some(chunk) = self.stream.next().await {
            out.extend(chunk?);
        }
        self.done = true;
        Ok(out)
    }

    /// Pull one more chunk into `pending`. `false` once the stream is done.
    async fn fill(&mut self) -> Result<bool, ExecError> {
        while !self.done {
            match self.stream.next().await {
                Some(chunk) => {
                    let chunk = chunk?;
                    // An empty chunk is "nothing here", not "nothing left".
                    if !chunk.is_empty() {
                        self.pending.extend(chunk);
                        return Ok(true);
                    }
                }
                None => self.done = true,
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_types::Value;

    fn rows(n: usize) -> Vec<Tuple> {
        (0..n).map(|i| vec![Value::Int4(i as i32)]).collect()
    }

    #[test]
    fn rows_once_splits_into_chunks_and_keeps_order() -> anyhow::Result<()> {
        let drained = drain_rows(rows_once(rows(ROW_CHUNK + 7)))?;
        assert_eq!(drained.len(), ROW_CHUNK + 7);
        assert_eq!(drained[0], vec![Value::Int4(0)]);
        assert_eq!(
            drained[ROW_CHUNK + 6],
            vec![Value::Int4((ROW_CHUNK + 6) as i32)]
        );
        Ok(())
    }

    #[test]
    fn empty_input_yields_no_chunks() -> anyhow::Result<()> {
        assert!(drain_rows(rows_once(Vec::new()))?.is_empty());
        Ok(())
    }

    #[test]
    fn first_row_stops_at_the_first_row() -> anyhow::Result<()> {
        assert_eq!(
            first_row(rows_once(rows(3)))?,
            Some(vec![Value::Int4(0)]),
            "the first row of the first chunk"
        );
        assert_eq!(first_row(rows_once(Vec::new()))?, None);
        Ok(())
    }

    /// A portal's row budget has nothing to do with chunk boundaries, so this is
    /// the case that would silently truncate a result set if `take` served only
    /// what one chunk held.
    #[tokio::test]
    async fn cursor_take_crosses_chunk_boundaries() -> anyhow::Result<()> {
        let mut cursor = RowCursor::new(rows_once(rows(ROW_CHUNK * 2 + 5)));
        let first = cursor.take(ROW_CHUNK + 3).await?;
        assert_eq!(first.len(), ROW_CHUNK + 3);
        assert_eq!(
            first[ROW_CHUNK + 2],
            vec![Value::Int4((ROW_CHUNK + 2) as i32)]
        );

        // Resuming picks up exactly where the budget ran out.
        assert_eq!(
            cursor.next_row().await?,
            Some(vec![Value::Int4((ROW_CHUNK + 3) as i32)])
        );

        let rest = cursor.collect().await?;
        assert_eq!(rest.len(), ROW_CHUNK + 1);
        // A cursor past the end keeps answering "no rows" rather than re-running.
        assert!(cursor.take(4).await?.is_empty());
        assert_eq!(cursor.next_row().await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn blocking_rows_reads_a_scan_in_chunks() -> anyhow::Result<()> {
        let scan = rows(ROW_CHUNK + 2).into_iter().map(Ok);
        let mut cursor = RowCursor::new(blocking_rows(Box::new(scan)));
        assert_eq!(cursor.collect().await?.len(), ROW_CHUNK + 2);
        Ok(())
    }

    #[tokio::test]
    async fn blocking_rows_reports_a_scan_error() {
        let scan = vec![
            Ok(vec![Value::Int4(1)]),
            Err(StorageError::CorruptData("bad page".into())),
        ]
        .into_iter();
        let mut cursor = RowCursor::new(blocking_rows(Box::new(scan)));
        let error = cursor.collect().await.err().map(|e| e.code.into_owned());
        assert_eq!(error.as_deref(), Some("XX001"));
    }
}
