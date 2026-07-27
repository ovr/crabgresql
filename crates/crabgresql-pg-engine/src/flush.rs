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
#[derive(Clone, Copy, Debug)]
pub struct BufferFlushPolicy {
    /// Per-relation size that makes a buffer flush-eligible on its own.
    pub table_soft_bytes: usize,
    /// Total buffered bytes across all relations that makes *every* buffer
    /// eligible, regardless of its own size. The backstop against many small
    /// relations adding up to a memory problem no single one would trigger.
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
            table_soft_bytes: config::DEFAULT_BUFFER_TABLE_SOFT_BYTES,
            global_hard_bytes: config::DEFAULT_BUFFER_GLOBAL_HARD_BYTES,
            max_age: Duration::from_millis(config::DEFAULT_BUFFER_MAX_AGE_MS),
            tick: Duration::from_millis(config::DEFAULT_BUFFER_TICK_MS),
        }
    }
}

impl BufferFlushPolicy {
    /// Read the policy from the environment, falling back to the defaults.
    ///
    /// Environment variables rather than GUCs because `SET` is session-scoped and
    /// there is no storage-settings plumbing yet; a per-session knob for a
    /// process-wide background thread would be misleading. Moving these to real
    /// GUCs is a follow-up. The names and defaults live in `crabgresql-config`
    /// with every other environment variable.
    pub fn from_env() -> Self {
        let default = BufferFlushPolicy::default();
        BufferFlushPolicy {
            table_soft_bytes: config::bytes_or(
                config::BUFFER_TABLE_SOFT_BYTES,
                default.table_soft_bytes,
            ),
            global_hard_bytes: config::bytes_or(
                config::BUFFER_GLOBAL_HARD_BYTES,
                default.global_hard_bytes,
            ),
            max_age: config::duration_ms_or(config::BUFFER_MAX_AGE_MS, default.max_age),
            tick: config::duration_ms_or(config::BUFFER_TICK_MS, default.tick),
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

    let total: usize = buffered.iter().map(|r| r.bytes).sum();
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
}

#[cfg(test)]
mod tests {
    use crabgresql_storage_api::{
        Column, TableAccessMethod, TableAm, TableEngine, TableSchema,
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
            .filter(|file| {
                file.path().extension().and_then(|x| x.to_str()) == Some("parquet")
            })
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
        let (engine, clog, next_xid) = PgEngine::open_recovered(dir, Arc::clone(&wal))?;
        let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
        let mut tm = TransactionManager::new_recovered(sink, clog, next_xid);
        tm.set_finalize(Arc::clone(&engine) as Arc<dyn TxnFinalize>);
        let tm = Arc::new(tm);
        // Set the service directly rather than through `attach_txn_manager`, which
        // would also start a worker on the production policy.
        let _ = engine.txnmgr.set(Arc::downgrade(&tm));

        let mut schema = TableSchema::new("p", vec![Column::new("id", PgType::Int4)]);
        schema.access_method = TableAccessMethod::Parquet;
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
            .scan(&reader)
            .map(|row| match row.expect("scan must not fail").1[0] {
                Value::Int4(id) => id,
                ref other => panic!("unexpected id {other:?}"),
            })
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![0, 1, 2, 3], "a background flush must not lose a row");

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

    /// A dropped engine lets the worker exit on its own, so a forgotten handle
    /// cannot keep the data directory open.
    #[test]
    fn the_worker_exits_when_the_engine_is_dropped() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let (engine, _clog, _next) = PgEngine::open_recovered(dir.path(), Arc::clone(&wal))?;
        let weak = Arc::downgrade(&engine);
        let worker = FlushWorker::spawn(weak.clone(), BufferFlushPolicy {
            tick: Duration::from_millis(5),
            ..BufferFlushPolicy::default()
        });
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
