//! The background flush worker for RAM write buffers.
//!
//! A Parquet relation acknowledges writes into a WAL-logged RAM buffer and only
//! later turns them into a durable chunk. Something has to decide *when* — left
//! alone, a buffer grows until the process runs out of memory and every restart
//! replays a longer WAL. `VACUUM` is the explicit hook; this is the automatic
//! one.
//!
//! It is a plain OS thread rather than a tokio task on purpose: a flush encodes
//! Parquet and `fsync`s, and parking a runtime worker on that would stall
//! connection handling on an unrelated socket.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crabgresql_config as config;

use crate::PgEngine;

/// When a relation's buffered rows should become a durable chunk.
///
/// Both sizes are what the rows occupy in RAM, not what they would serialize
/// to: these are memory budgets, and a budget measured in encoded bytes admits
/// several times the rows an operator asked for.
#[derive(Clone, Copy, Debug)]
pub struct BufferFlushPolicy {
    /// Per-relation resident size that makes a buffer flush-eligible on its own.
    pub table_soft_bytes: usize,
    /// Total resident bytes across all relations that makes *every* buffer
    /// eligible, regardless of its own size, and past which a writer waits for
    /// the flush to catch up. The backstop against many small relations adding
    /// up to a memory problem no single one would trigger — and, because it
    /// blocks, the only one of these that actually bounds anything when the
    /// writer outruns the flush.
    pub global_hard_bytes: usize,
    /// How long a buffer may hold rows before being flushed anyway, so a
    /// low-volume relation still becomes durable-as-a-file in bounded time.
    pub max_age: Duration,
    /// How often the worker looks.
    pub tick: Duration,
}

impl Default for BufferFlushPolicy {
    fn default() -> Self {
        BufferFlushPolicy {
            table_soft_bytes: config::BUFFER_TABLE_SOFT_BYTES.default,
            global_hard_bytes: config::BUFFER_GLOBAL_HARD_BYTES.default,
            max_age: config::BUFFER_MAX_AGE.default,
            tick: config::BUFFER_TICK.default,
        }
    }
}

impl BufferFlushPolicy {
    /// Read the policy from the environment, falling back to the defaults.
    ///
    /// Environment variables rather than GUCs because `SET` is session-scoped
    /// and a per-session knob for a process-wide background thread would be
    /// misleading. The names, defaults and accepted ranges live in
    /// `crabgresql-config` with every other environment variable.
    ///
    /// TODO: expose the flush policy as GUCs, once a setting can be changed for
    /// the whole process instead of one session.
    ///
    /// A value we cannot use as written is corrected — clamped into range, or
    /// replaced by the default when it does not parse — and the correction is
    /// logged, since a knob that silently does something other than what it
    /// was set to is worse than one that is ignored loudly.
    pub fn from_env() -> Self {
        let complain = |message: String| tracing::warn!("{message}");
        BufferFlushPolicy {
            table_soft_bytes: config::BUFFER_TABLE_SOFT_BYTES.get(complain),
            global_hard_bytes: config::BUFFER_GLOBAL_HARD_BYTES.get(complain),
            max_age: config::BUFFER_MAX_AGE.get(complain),
            tick: config::BUFFER_TICK.get(complain),
        }
    }
}

/// One relation with rows waiting in its buffer.
pub struct BufferedRelation {
    pub namespace: String,
    pub name: String,
    /// Identity for the worker's age bookkeeping. Stable for the relation's life
    /// except across a TRUNCATE, which empties the buffer anyway.
    pub rel: u32,
    pub bytes: usize,
    /// Whether flushing this relation can actually return its memory. A
    /// standalone `USING buffer` table has nowhere to flush to, so its bytes
    /// are held until a snapshot releases them and no amount of flushing helps.
    /// It is still worth vacuuming on age, so it stays in the list — but it
    /// must not count toward a global total whose only remedy is a flush.
    pub flushable: bool,
}

/// How long a writer will wait for the flush worker to make room before giving
/// up and proceeding anyway.
///
/// Backpressure that cannot be escaped is a hang: a flush that fails for a
/// reason retrying will not fix — a full disk, a permission change — would
/// otherwise wedge every writing session for the life of the process. Letting
/// the statement through instead trades the memory bound for liveness, which is
/// the right way round for a bound that exists to protect against a slow flush
/// rather than a broken one. The wait is long enough that a healthy flush
/// finishes well inside it, so reaching the end is a real complaint.
const WRITE_CAPACITY_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether buffered rows are over the global limit, and a place for writers to
/// wait until they are not.
///
/// The flag is written only by the flush worker, at the end of each pass, since
/// a pass is the only thing that can change the answer downward. Writers only
/// ever read it.
#[derive(Default)]
pub struct BufferPressure {
    over_limit: Mutex<bool>,
    relieved: Condvar,
}

impl BufferPressure {
    /// Record what the last sweep saw, waking every waiting writer when the
    /// buffers have come back under the limit.
    pub fn set(&self, over_limit: bool) {
        let Ok(mut flag) = self.over_limit.lock() else {
            return;
        };
        *flag = over_limit;
        if !over_limit {
            self.relieved.notify_all();
        }
    }

    /// What the last sweep found. For tests and diagnostics; a writer uses
    /// [`BufferPressure::wait`], which cannot race between the check and the
    /// wait.
    pub fn is_over_limit(&self) -> bool {
        self.over_limit.lock().map(|flag| *flag).unwrap_or_default()
    }

    /// Block while the buffers are over the limit, for at most
    /// [`WRITE_CAPACITY_TIMEOUT`].
    ///
    /// The caller must hold no XID and no snapshot; see
    /// [`TableEngine::await_write_capacity`](crabgresql_storage_api::TableEngine::await_write_capacity)
    /// for why that is a correctness requirement and not just good manners.
    pub fn wait(&self) {
        let Ok(flag) = self.over_limit.lock() else {
            return;
        };
        if !*flag {
            return;
        }
        let waited = Instant::now();
        let Ok((flag, timeout)) =
            self.relieved
                .wait_timeout_while(flag, WRITE_CAPACITY_TIMEOUT, |over| *over)
        else {
            return;
        };
        // Drop the guard before logging: a warning is not worth holding the
        // lock the flush worker needs to clear the flag.
        let still_over = *flag;
        drop(flag);
        if timeout.timed_out() && still_over {
            tracing::warn!(
                waited_ms = waited.elapsed().as_millis(),
                "buffered writes are over the global limit and the flush has not \
                 caught up; letting the statement through anyway"
            );
        }
    }
}

/// Signals the worker to stop and lets `stop_and_join` wait for it.
type Stop = Arc<(Mutex<bool>, Condvar)>;

/// The background flush thread.
pub struct FlushWorker {
    stop: Stop,
    handle: Option<JoinHandle<()>>,
}

impl FlushWorker {
    /// Start the worker over `engine`.
    ///
    /// The handle is weak so the thread can never keep the engine (and its file
    /// descriptors) alive past shutdown: if the engine is dropped without
    /// `stop_and_join`, the next tick fails to upgrade and the thread exits.
    pub fn spawn(engine: Weak<PgEngine>, policy: BufferFlushPolicy) -> FlushWorker {
        let stop: Stop = Arc::new((Mutex::new(false), Condvar::new()));
        let signal = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("crabgresql-buffer-flush".to_string())
            .spawn(move || run(engine, policy, signal))
            .ok();
        if handle.is_none() {
            tracing::error!(
                "could not start the buffer flush worker; \
                 buffers will only be flushed by VACUUM"
            );
        }
        FlushWorker { stop, handle }
    }

    /// Ask the worker to stop and wait for it.
    ///
    /// Deliberately does **not** flush on the way out. Buffered rows are already
    /// WAL-durable and are rebuilt at the next startup, so a shutdown flush would
    /// buy nothing while making a clean exit slow and able to fail — at the
    /// moment the transaction service is being torn down.
    ///
    /// "Rebuilt at the next startup" costs something now that replay is bounded:
    /// those rows are reachable only by replaying their records, so a checkpoint
    /// with rows still resident cannot bound replay at all (see
    /// `PgEngine::redo_clamp`). Leaving them is still the right call here — the
    /// alternative is a flush transaction during teardown — but that is why this
    /// runs *before* the shutdown checkpoint samples anything.
    pub fn stop_and_join(mut self) {
        let (flag, condvar) = &*self.stop;
        match flag.lock() {
            Ok(mut stopping) => {
                *stopping = true;
                condvar.notify_all();
            }
            // A poisoned flag means the worker panicked; there is nothing to
            // signal and the join below will surface it.
            Err(_) => return,
        }
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            tracing::error!("the buffer flush worker panicked");
        }
    }
}

fn run(engine: Weak<PgEngine>, policy: BufferFlushPolicy, stop: Stop) {
    // When each relation's buffer was first seen non-empty, so a quiet relation
    // still flushes on age. Kept here rather than in the buffer so the storage
    // layer never has to consult a clock.
    let mut waiting_since: HashMap<u32, Instant> = HashMap::new();
    let (flag, condvar) = &*stop;
    loop {
        {
            let Ok(stopping) = flag.lock() else { return };
            if *stopping {
                return;
            }
            let Ok((stopping, _)) = condvar.wait_timeout(stopping, policy.tick) else {
                return;
            };
            if *stopping {
                return;
            }
        }
        let Some(engine) = engine.upgrade() else {
            return;
        };
        sweep(&engine, &policy, &mut waiting_since);
    }
}

/// One pass: flush every relation the policy says is due.
fn sweep(
    engine: &Arc<PgEngine>,
    policy: &BufferFlushPolicy,
    waiting_since: &mut HashMap<u32, Instant>,
) {
    let buffered = engine.buffered_relations();
    // A relation whose buffer emptied (flushed, truncated, or dropped) starts its
    // age clock over the next time it fills.
    waiting_since.retain(|rel, _| buffered.iter().any(|r| r.rel == *rel));

    let total = flushable_bytes(&buffered);
    let now = Instant::now();
    for relation in buffered {
        let since = *waiting_since.entry(relation.rel).or_insert(now);
        let due = relation.bytes >= policy.table_soft_bytes
            || total >= policy.global_hard_bytes
            || now.duration_since(since) >= policy.max_age;
        if !due {
            continue;
        }
        match engine.flush_buffer(&relation.namespace, &relation.name) {
            Ok(rows) => {
                waiting_since.remove(&relation.rel);
                if rows > 0 {
                    tracing::debug!(
                        table = %relation.name,
                        rows,
                        "flushed buffered rows to durable storage"
                    );
                }
            }
            // A relation that vanished mid-sweep, or a transient I/O failure: the
            // rows are still WAL-durable and still buffered, so the next tick
            // retries. Failing to flush must never take the server down.
            Err(error) => {
                tracing::warn!(
                    table = %relation.name,
                    error = %error,
                    "buffer flush failed; will retry"
                );
            }
        }
    }

    // Recomputed rather than decremented: what the flushes above actually
    // returned is the number a blocked writer is waiting on, and a flush frees
    // nothing while an older snapshot can still read the rows it copied.
    engine
        .buffer_pressure()
        .set(flushable_bytes(&engine.buffered_relations()) >= policy.global_hard_bytes);
}

/// Buffered bytes a flush could actually return.
///
/// A standalone `USING buffer` relation is excluded: it has nowhere to flush
/// to, so counting it would let one such table hold the global limit true
/// forever — every Parquet relation force-flushed on every tick, and every
/// writer waiting on a condition no flush can clear.
fn flushable_bytes(buffered: &[BufferedRelation]) -> usize {
    buffered
        .iter()
        .filter(|relation| relation.flushable)
        .map(|relation| relation.bytes)
        .sum()
}

#[cfg(test)]
mod tests {
    use crabgresql_storage_api::{
        Column, ColumnProjection, TableAccessMethod, TableAm, TableEngine, TableSchema,
    };
    use crabgresql_txn::{CommandId, CommitSink, TransactionManager, TxnFinalize};
    use crabgresql_types::{PgType, Value};
    use crabgresql_wal::Wal;

    use super::*;

    fn parquet_files(dir: &std::path::Path) -> usize {
        let root = dir.join("parquet");
        let Ok(entries) = std::fs::read_dir(&root) else {
            return 0;
        };
        entries
            .flatten()
            .filter_map(|entry| std::fs::read_dir(entry.path()).ok())
            .flat_map(|inner| inner.flatten())
            .filter(|file| file.path().extension().and_then(|x| x.to_str()) == Some("parquet"))
            .count()
    }

    /// An engine with its finalize hook wired, a transaction service attached,
    /// and one Parquet relation — everything a flush needs except the worker,
    /// which each test starts with its own policy.
    #[allow(clippy::type_complexity)]
    fn wired_engine(
        dir: &std::path::Path,
    ) -> anyhow::Result<(Arc<PgEngine>, Arc<TransactionManager>, Arc<dyn TableAm>)> {
        let wal = Arc::new(Wal::open(dir)?);
        let (engine, clog, next_xid) = PgEngine::open_recovered_from_with_pool(
            dir,
            Arc::clone(&wal),
            crabgresql_wal::Lsn::INVALID,
            crate::BufferPoolPolicy::minimal(),
        )?;
        let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
        let mut tm = TransactionManager::new_recovered(sink, clog, next_xid);
        tm.set_finalize(Arc::clone(&engine) as Arc<dyn TxnFinalize>);
        let tm = Arc::new(tm);
        // Set the service directly rather than through `attach_txn_manager`, which
        // would also start a worker on the production policy.
        let _ = engine.txnmgr.set(Arc::downgrade(&tm));

        let mut schema = TableSchema::new("p", vec![Column::new("id", PgType::Int4)]);
        schema.access_method = TableAccessMethod::Parquet;
        // The engine refuses an engine-managed relation with no declared order.
        schema.sort_key = vec![crabgresql_storage_api::IndexKey {
            column: 0,
            descending: false,
            nulls_first: false,
        }];
        let table = engine.create_table(schema)?;
        Ok((engine, tm, table))
    }

    /// The worker flushes a relation once its buffer crosses the soft limit,
    /// without anyone running `VACUUM`.
    #[test]
    fn the_worker_flushes_a_buffer_that_crosses_the_soft_limit() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (engine, tm, table) = wired_engine(dir.path())?;

        // A policy that fires almost immediately, so the test does not depend on
        // the production 32 MiB / 60 s defaults.
        let worker = FlushWorker::spawn(
            Arc::downgrade(&engine),
            BufferFlushPolicy {
                table_soft_bytes: 1,
                global_hard_bytes: usize::MAX,
                max_age: Duration::from_secs(3600),
                tick: Duration::from_millis(5),
            },
        );

        let xid = tm.allocate_xid();
        table.insert_many(
            (0..4).map(|id| vec![Value::Int4(id)]).collect(),
            &tm.context(xid, CommandId::FIRST),
        )?;
        tm.commit(xid)?;

        // Poll rather than sleep a fixed amount: the assertion is "the worker gets
        // there", not "it gets there within exactly N ms".
        let deadline = Instant::now() + Duration::from_secs(10);
        while parquet_files(dir.path()) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            parquet_files(dir.path()),
            1,
            "the worker must have consolidated the buffer into one fragment"
        );

        let reader = tm.context(tm.allocate_xid(), CommandId::FIRST);
        let mut ids: Vec<i32> = table
            .scan(&reader, &ColumnProjection::All)
            .map(|row| match row.expect("scan must not fail").1[0] {
                Value::Int4(id) => id,
                ref other => panic!("unexpected id {other:?}"),
            })
            .collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![0, 1, 2, 3],
            "a background flush must not lose a row"
        );

        worker.stop_and_join();
        Ok(())
    }

    /// Stopping the worker interrupts its wait rather than riding it out, and
    /// leaves buffered rows buffered — they are already WAL-durable, so a
    /// shutdown flush would only make a clean exit slow and fallible.
    #[test]
    fn stopping_the_worker_returns_promptly_and_does_not_flush() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (engine, tm, table) = wired_engine(dir.path())?;
        let xid = tm.allocate_xid();
        table.insert_many(
            (0..4).map(|id| vec![Value::Int4(id)]).collect(),
            &tm.context(xid, CommandId::FIRST),
        )?;
        tm.commit(xid)?;

        // An hour-long tick: a stop that waited for one would hang the test.
        let worker = FlushWorker::spawn(
            Arc::downgrade(&engine),
            BufferFlushPolicy {
                table_soft_bytes: 1,
                tick: Duration::from_secs(3600),
                ..BufferFlushPolicy::default()
            },
        );
        let started = Instant::now();
        worker.stop_and_join();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "stop must interrupt the wait, not ride it out"
        );
        assert_eq!(
            parquet_files(dir.path()),
            0,
            "stopping must not flush: the rows are WAL-durable and are rebuilt at startup"
        );
        assert_eq!(
            engine.buffered_relations().len(),
            1,
            "the rows must still be buffered"
        );
        Ok(())
    }

    /// Over the global limit a writer waits, and the flush that brings the
    /// buffers back under it is what lets the writer through. Without this the
    /// "hard" limit only decides when a flush *starts*, which bounds nothing
    /// when the writer outruns the flush.
    #[test]
    fn a_writer_waits_while_the_buffers_are_over_the_global_limit() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (engine, tm, table) = wired_engine(dir.path())?;

        let xid = tm.allocate_xid();
        table.insert_many(
            (0..4).map(|id| vec![Value::Int4(id)]).collect(),
            &tm.context(xid, CommandId::FIRST),
        )?;
        tm.commit(xid)?;

        // Assert the wait directly rather than racing the worker: with the flag
        // set and nobody to clear it, `await_write_capacity` must not return
        // promptly, and clearing it must wake the waiter.
        engine.buffer_pressure().set(true);
        let waiter = Arc::clone(&engine);
        let (running, started_waiting) = std::sync::mpsc::channel();
        let released = std::thread::spawn(move || {
            // Signalled from inside the thread, immediately before blocking, so
            // that "still unfinished" below cannot instead mean "never
            // scheduled". Nothing clears the flag until after the sleep, so the
            // few instructions between this and the wait cannot lose a wakeup.
            running
                .send(())
                .expect("the main thread is waiting on this");
            waiter.await_write_capacity();
        });
        started_waiting.recv_timeout(Duration::from_secs(5))?;

        // Both ends of every interval below come from this one clock: an elapsed
        // time measured across two threads' clocks can come in under the sleep
        // it in fact outlasted.
        let started = Instant::now();
        // Long enough to distinguish "waited" from "returned immediately", short
        // enough not to slow the suite.
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !released.is_finished(),
            "the writer returned while the flag was still set, so it never waited"
        );
        engine.buffer_pressure().set(false);

        released
            .join()
            .map_err(|_| anyhow::anyhow!("waiter panicked"))?;
        assert!(
            started.elapsed() < WRITE_CAPACITY_TIMEOUT,
            "the writer rode out the timeout instead of being woken"
        );
        Ok(())
    }

    /// The worker is what raises and lowers the flag, and it does so from what
    /// a flush actually returned rather than from what it copied. A reader that
    /// predates the flush holds the buffer copies back, so the memory really is
    /// still in use and a writer really should wait — and once that reader is
    /// gone, the next sweep reclaims and lets the writer through.
    #[test]
    fn the_sweep_raises_pressure_while_a_flush_cannot_reclaim() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (engine, tm, table) = wired_engine(dir.path())?;

        let xid = tm.allocate_xid();
        table.insert_many(
            (0..16).map(|id| vec![Value::Int4(id)]).collect(),
            &tm.context(xid, CommandId::FIRST),
        )?;
        tm.commit(xid)?;

        // A snapshot older than the flush: its rows may be copied into a
        // fragment, but the buffer's copies cannot be dropped while it lives.
        let reader = tm.context(crabgresql_txn::Xid::INVALID, CommandId::FIRST);

        let worker = FlushWorker::spawn(
            Arc::downgrade(&engine),
            BufferFlushPolicy {
                table_soft_bytes: 1,
                global_hard_bytes: 1,
                max_age: Duration::from_secs(3600),
                tick: Duration::from_millis(5),
            },
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        while !engine.buffer_pressure().is_over_limit() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            engine.buffer_pressure().is_over_limit(),
            "rows a flush could not reclaim must keep the limit shut"
        );

        drop(reader);
        let deadline = Instant::now() + Duration::from_secs(10);
        while engine.buffer_pressure().is_over_limit() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !engine.buffer_pressure().is_over_limit(),
            "once nothing can read the copies, the next sweep must let writers through"
        );

        worker.stop_and_join();
        Ok(())
    }

    /// A relation with nowhere to flush to must not drive the global limit. Its
    /// bytes are real, but no flush can return them, so counting them would
    /// stall every writer on a condition that can never clear — the one way
    /// backpressure turns into a hang.
    #[test]
    fn a_relation_that_cannot_flush_does_not_hold_the_limit_shut() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (engine, tm, _parquet) = wired_engine(dir.path())?;

        let mut schema = TableSchema::new("b", vec![Column::new("id", PgType::Int4)]);
        schema.access_method = TableAccessMethod::Buffer;
        // Engine-managed relations must declare their order, `buffer` included.
        schema.sort_key = vec![crabgresql_storage_api::IndexKey {
            column: 0,
            descending: false,
            nulls_first: false,
        }];
        let standalone = engine.create_table(schema)?;

        let xid = tm.allocate_xid();
        standalone.insert_many(
            (0..64).map(|id| vec![Value::Int4(id)]).collect(),
            &tm.context(xid, CommandId::FIRST),
        )?;
        tm.commit(xid)?;

        let buffered = engine.buffered_relations();
        assert!(
            buffered.iter().any(|relation| relation.name == "b"),
            "an unflushable relation still belongs in the list, so it is vacuumed on age"
        );
        assert_eq!(
            flushable_bytes(&buffered),
            0,
            "its bytes must not count toward a limit only a flush can relieve"
        );

        // End to end: a worker whose limit is one byte must still leave writers
        // alone, because the only buffered rows are ones it cannot flush.
        let worker = FlushWorker::spawn(
            Arc::downgrade(&engine),
            BufferFlushPolicy {
                table_soft_bytes: usize::MAX,
                global_hard_bytes: 1,
                max_age: Duration::from_secs(3600),
                tick: Duration::from_millis(5),
            },
        );
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        engine.await_write_capacity();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a writer must not wait on a relation no flush can relieve"
        );
        worker.stop_and_join();
        Ok(())
    }

    /// A dropped engine lets the worker exit on its own, so a forgotten handle
    /// cannot keep the data directory open.
    #[test]
    fn the_worker_exits_when_the_engine_is_dropped() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let (engine, _clog, _next) = PgEngine::open_recovered_with_pool(
            dir.path(),
            Arc::clone(&wal),
            crate::BufferPoolPolicy::minimal(),
        )?;
        let weak = Arc::downgrade(&engine);
        let worker = FlushWorker::spawn(
            weak.clone(),
            BufferFlushPolicy {
                tick: Duration::from_millis(5),
                ..BufferFlushPolicy::default()
            },
        );
        drop(engine);

        let deadline = Instant::now() + Duration::from_secs(10);
        while weak.upgrade().is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            weak.upgrade().is_none(),
            "the worker must hold only a weak reference to the engine"
        );
        worker.stop_and_join();
        Ok(())
    }
}
