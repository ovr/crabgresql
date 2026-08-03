//! The `buffer` table access method: a WAL-logged, RAM-resident MVCC row store.
//!
//! Rows live only in memory; durability comes entirely from the WAL. That is the
//! deliberate inverse of the heap engine's `TEMP`/`UNLOGGED` memory tables, which
//! are also RAM-resident but skip the WAL: those trade durability for speed,
//! while a buffer table trades *space* for speed and keeps every guarantee. A
//! committed `INSERT` is as durable here as in the heap — the commit record's
//! fsync covers the row — it simply has no file of its own until something moves
//! it to one.
//!
//! That is what makes it useful in front of an append-only columnar store: writes
//! are acknowledged at WAL speed and accumulate in RAM, and a later flush turns
//! many small writes into one large immutable file. Used on its own via
//! `CREATE TABLE ... USING buffer`, it is a fast, fully transactional table whose
//! size is bounded by memory.
//!
//! MVCC is per row and uses the core [`TupleHeader`]/[`satisfies_mvcc`] rule
//! unchanged, so visibility here is the same rule the heap obeys — there is no
//! second definition of "visible" in the tree.
//!
//! Rows are addressed by **logical row id** ([`Tid::logical`]), never by a
//! physical slot. A row keeps its identity when it is flushed to another access
//! method's storage, which a `(block, offset)` address could not express.
//!
//! Recovery replays the whole WAL into [`BufferRedo`], then
//! [`BufferTable::restore`] installs the rows whose transactions committed. There
//! is no checkpoint to reconcile against: the buffer starts empty at every boot
//! and is rebuilt from the log.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crabgresql_storage_api::{
    BatchStream, ColumnProjection, DeleteResult, IndexMetadata, MAX_ROW_ID, RelStats, StorageError,
    TableAm, TableSchema, Tid, Tuple, TupleStream, UpdateResult,
};
use crabgresql_txn::{Clog, CommandId, TupleHeader, TxnContext, XactStatus, Xid, satisfies_mvcc};
use crabgresql_types::Value;
use crabgresql_types::datum::{decode_datum, encode_datum};
use crabgresql_wal::{RedoContext, RmgrId, RmgrRedo, Wal, WalError};
use deepsize::DeepSizeOf;

/// Resource manager for buffer-table records. 10 is the heap, 11 the B-tree,
/// 12 Parquet.
pub const RMGR_BUFFER: RmgrId = RmgrId(13);

/// Rows appended by a transaction.
pub const BUFFER_INSERT: u8 = 1;
/// Rows stamped deleted by a transaction.
pub const BUFFER_DELETE: u8 = 2;

/// Payload format version. Bumping it is how a layout change stays readable:
/// redo rejects a version it does not know rather than misparsing it.
const PAYLOAD_FORMAT: u8 = 1;

/// Rows per batch handed up by [`TableAm::scan_batches`], matching the Parquet
/// reader's own batch size so both leaves of one relation stream at the same
/// granularity.
const BATCH_ROWS: usize = 8_192;

fn corrupt(message: impl Into<String>) -> StorageError {
    StorageError::CorruptData(message.into())
}

fn unsupported(message: impl Into<String>) -> StorageError {
    StorageError::UnsupportedOperation(message.into())
}

/// One MVCC row version resident in RAM.
#[derive(Clone, Debug)]
struct BufferRow {
    /// Stable identity, monotonically assigned and never reused. Survives a
    /// flush into another access method's storage.
    row_id: u64,
    values: Tuple,
    hdr: TupleHeader,
    /// What this row occupies in RAM, charged to the table's byte accounting.
    /// Cached because the flush thresholds read it far more often than rows
    /// change. See [`BufferRow::new`] for what goes into it.
    bytes: usize,
}

impl BufferRow {
    /// A row with its memory footprint already charged.
    ///
    /// The only way to build one, so a row cannot enter the table uncounted or
    /// counted by a formula that has drifted from the other call site's. Takes
    /// the `Tuple` by value because the charge depends on its *capacity*, which
    /// a slice cannot see — and capacity is what the allocator was actually
    /// asked for.
    ///
    /// The number feeds a memory budget an operator sets to bound RSS, so it
    /// may be imprecise but must never be systematically low: a threshold that
    /// admits several times the rows it was asked for is not a tuning
    /// inaccuracy, it is a limit that does not limit. The term worth naming is
    /// the tuple's — `size_of::<Value>()` is charged per *column*, however small
    /// that column's type, because that is what a `Vec<Value>` costs.
    ///
    /// It is a floor rather than an exact figure: [`DeepSizeOf`] sums the
    /// capacities a program asked the allocator for, and an allocator rounds
    /// every request up to a granule and refuses to go below a minimum block,
    /// so a row of many small strings really costs somewhat more than this.
    fn new(row_id: u64, values: Tuple, hdr: TupleHeader) -> BufferRow {
        let bytes = size_of::<BufferRow>() + tuple_heap_bytes(&values);
        BufferRow {
            row_id,
            values,
            hdr,
            bytes,
        }
    }
}

/// What a tuple owns beyond the `Vec` header that sits inline in its holder.
///
/// [`DeepSizeOf::deep_size_of`] reports the header too, and the only way to ask
/// for children alone needs a `deepsize::Context` the crate does not let a
/// caller construct — so the header is subtracted back off here rather than
/// double-counted against the [`BufferRow`] that already contains it.
fn tuple_heap_bytes(values: &Tuple) -> usize {
    values.deep_size_of() - size_of::<Tuple>()
}

/// A row recovered from the WAL, before its transaction's fate is known.
#[derive(Clone, Debug)]
pub struct RestoredRow {
    pub row_id: u64,
    pub values: Tuple,
    pub hdr: TupleHeader,
}

/// Everything replay learned about one relation's buffer.
#[derive(Debug, Default)]
pub struct RestoredBuffer {
    pub rows: Vec<RestoredRow>,
    /// One past the highest row id the log mentions, so restored ids are never
    /// handed out a second time.
    pub next_row_id: u64,
    /// `row_id -> index into rows`, so replaying a delete is a lookup rather than
    /// a scan. A flush emits one `BUFFER_DELETE` naming *every* row it moved, so
    /// deletes are not rare in the log — without this, replay is quadratic in the
    /// relation's lifetime row count and startup time runs away.
    index: HashMap<u64, usize>,
}

/// A WAL-logged, RAM-resident MVCC row store.
pub struct BufferTable {
    schema: Arc<TableSchema>,
    /// The relation identity this table's WAL records carry — the relfilenode
    /// the catalog currently names.
    ///
    /// A buffer table has no file to swap, so it never *initiates* a change here;
    /// but when it fronts an access method that does (a Parquet relation's
    /// TRUNCATE stages a fresh directory), it must follow, because recovery looks
    /// up a relation's replayed rows by the id the catalog names at boot. Rows
    /// logged under a superseded id are then correctly dropped: the only way that
    /// id was superseded is a committed TRUNCATE, which those rows do not survive.
    rel: AtomicU32,
    /// Rows in ascending `row_id` order, which is also insertion order — so
    /// lookups binary-search and scans need no sort. `vacuum` uses `retain`,
    /// which preserves the ordering.
    rows: RwLock<Vec<BufferRow>>,
    next_row_id: AtomicU64,
    /// Live resident bytes, maintained incrementally so a flush scheduler can
    /// read it without walking the rows. Deliberately excludes this `Vec`'s own
    /// growth slack: that is bounded and small next to the rows, but it changes
    /// on every reallocation, and a term that moves over a row's lifetime
    /// cannot live in a per-row charge that `vacuum` later subtracts.
    bytes: AtomicUsize,
    wal: Arc<Wal>,
    indexes: RwLock<Vec<IndexMetadata>>,
    /// The EXPLAIN line this table prints, when it is one leaf of a relation
    /// whose storage is split. `None` — a standalone `USING buffer` table — keeps
    /// PostgreSQL's plain `Seq Scan on <rel>`, because from SQL there is nothing
    /// unusual to report.
    scan_label: Option<String>,
    /// The commit log, attached once the engine has a transaction service.
    ///
    /// [`TableAm::statistics`] takes no [`TxnContext`], but deciding whether a
    /// stamped row is actually dead needs the deleter's fate. Absent (a bare table
    /// in a unit test) the count falls back to treating any `xmax` as dead.
    clog: RwLock<Option<Arc<Clog>>>,
}

impl BufferTable {
    pub fn open(rel: u32, schema: TableSchema, indexes: Vec<IndexMetadata>, wal: Arc<Wal>) -> Self {
        BufferTable {
            schema: Arc::new(schema),
            rel: AtomicU32::new(rel),
            rows: RwLock::new(Vec::new()),
            next_row_id: AtomicU64::new(0),
            bytes: AtomicUsize::new(0),
            wal,
            indexes: RwLock::new(indexes),
            scan_label: None,
            clog: RwLock::new(None),
        }
    }

    /// Label this table as the write buffer of `relation` in EXPLAIN output, so
    /// an `Append` over one relation's two leaves does not print the same line
    /// twice.
    pub fn as_write_buffer_of(mut self, relation: &str) -> Self {
        self.scan_label = Some(format!("Buffer Scan on {relation}"));
        self
    }

    /// The relation identity this table's WAL records carry.
    pub fn relfilenode(&self) -> u32 {
        self.rel.load(Ordering::Relaxed)
    }

    /// Point this table at a new relfilenode, following a committed TRUNCATE on
    /// the relation it fronts. Rows already in memory are unaffected: the id
    /// selects where *future* records are filed and where recovery looks, and the
    /// truncate's own MVCC tombstones decide what stays visible.
    pub fn rebind(&self, new: u32) {
        self.rel.store(new, Ordering::Relaxed);
    }

    /// Append rows, filing their WAL records under `rel` instead of this table's
    /// current relfilenode.
    ///
    /// A relation that stages a TRUNCATE reads and writes in the *staged*
    /// generation until it commits, so rows written by the truncating transaction
    /// must be logged there too — otherwise recovery would look for them under
    /// the committed id and find nothing, losing rows the transaction
    /// acknowledged. Every other transaction passes the live id and is unaffected.
    pub fn append_in(
        &self,
        rel: u32,
        tuples: Vec<Tuple>,
        txn: &TxnContext,
    ) -> Result<Vec<Tid>, StorageError> {
        self.append(rel, tuples, txn)
    }

    /// Append rows under ids already minted by [`Self::alloc_row_ids`].
    ///
    /// Exists so a test can install two batches in the opposite order to the one
    /// their ids were minted in — the interleaving that the unlocked encode step
    /// makes possible and that a plain `extend` would leave unsorted forever.
    #[cfg(test)]
    pub(crate) fn append_at(
        &self,
        first_row_id: u64,
        tuples: Vec<Tuple>,
        txn: &TxnContext,
    ) -> Result<Vec<Tid>, StorageError> {
        self.install(self.relfilenode(), first_row_id, tuples, txn)
    }

    /// Stamp every currently-live row deleted, filing the record under `rel`.
    ///
    /// Deliberately not snapshot-scoped: this backs `TRUNCATE`, which empties the
    /// relation outright. The caller must hold the relation's exclusive lock, so
    /// no other transaction can be adding rows while this runs.
    pub fn delete_all_live_in(&self, rel: u32, txn: &TxnContext) -> Result<u64, StorageError> {
        let live: Vec<u64> = {
            let rows = self.rows_read();
            rows.iter()
                .filter(|row| {
                    !row.hdr.xmax.is_valid() || txn.clog.status(row.hdr.xmax) == XactStatus::Aborted
                })
                .map(|row| row.row_id)
                .collect()
        };
        Ok(self.stamp_deleted(rel, &live, txn).len() as u64)
    }

    /// Stamp rows deleted, filing the record under `rel`. See [`Self::append_in`].
    pub fn delete_many_in(
        &self,
        rel: u32,
        tids: Vec<Tid>,
        txn: &TxnContext,
    ) -> Result<u64, StorageError> {
        Ok(self.stamp_deleted(rel, &self.row_ids_of(tids)?, txn).len() as u64)
    }

    /// The logical row ids `tids` name, erroring on a physical one.
    fn row_ids_of(&self, tids: Vec<Tid>) -> Result<Vec<u64>, StorageError> {
        tids.into_iter()
            .map(|tid| {
                tid.row_id().ok_or_else(|| {
                    corrupt(format!(
                        "buffer table \"{}\" was handed a physical tid",
                        self.schema.name
                    ))
                })
            })
            .collect()
    }

    /// The rows visible to `txn`, as `(tid, values)` — the input to a flush.
    ///
    /// "Visible to a fresh snapshot" is exactly the right selection: it admits
    /// rows whose inserter committed before the flush transaction started, and
    /// excludes rows still in flight (they stay buffered for a later flush) and
    /// rows an aborted transaction wrote. No separate seal state is needed —
    /// MVCC already answers the question.
    pub fn snapshot_rows(&self, txn: &TxnContext) -> Vec<(Tid, Tuple)> {
        // A flush rewrites whole rows into a fragment, so it needs every column.
        self.visible(txn, &ColumnProjection::All)
    }

    /// Supply the commit log, so [`TableAm::statistics`] can tell a row deleted by
    /// a committed transaction from one whose deleter aborted.
    pub fn attach_clog(&self, clog: Arc<Clog>) {
        *self
            .clog
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned")) = Some(clog);
    }

    /// Rows no committed transaction has deleted. An aborted deleter leaves its
    /// `xmax` behind, so the field's presence is not the test.
    pub fn live_rows(&self) -> usize {
        let clog = self
            .clog
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let rows = self.rows_read();
        rows.iter()
            .filter(|row| {
                if !row.hdr.xmax.is_valid() {
                    return true;
                }
                match clog.as_ref() {
                    Some(clog) => clog.status(row.hdr.xmax) != XactStatus::Committed,
                    None => false,
                }
            })
            .count()
    }

    /// Live resident bytes — what these rows cost in RAM, not what they would
    /// cost serialized. Cheap enough for a scheduler to poll.
    pub fn resident_bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Attach a semantic index. There is no physical index structure: rows live
    /// in one `Vec`, so uniqueness is enforced by the executor's visible-row scan
    /// and `supports_index_scan` stays false.
    pub fn add_index(&self, index: IndexMetadata) {
        self.indexes
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .push(index);
    }

    pub fn remove_index(&self, name: &str) {
        self.indexes
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .retain(|index| index.name != name);
    }

    fn rows_read(&self) -> std::sync::RwLockReadGuard<'_, Vec<BufferRow>> {
        self.rows
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
    }

    fn rows_write(&self) -> std::sync::RwLockWriteGuard<'_, Vec<BufferRow>> {
        self.rows
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
    }

    /// Install the rows replay recovered, keeping only those a future snapshot
    /// could see.
    ///
    /// Filtering here rather than at read time is what keeps the steady-state
    /// scan free of recovery concerns: an aborted or never-committed inserter's
    /// rows can never become visible (after a crash an XID without a commit
    /// record will never get one), and a committed deleter's rows are dead to
    /// every snapshot a restarted server can hand out, since the oldest possible
    /// snapshot starts after recovery.
    pub fn restore(&self, restored: RestoredBuffer, clog: &Clog) {
        let mut rows = self.rows_write();
        let mut bytes = 0usize;
        rows.clear();
        for row in restored.rows {
            if clog.status(row.hdr.xmin) != XactStatus::Committed {
                continue;
            }
            if row.hdr.xmax.is_valid() && clog.status(row.hdr.xmax) == XactStatus::Committed {
                continue;
            }
            let restored = BufferRow::new(
                row.row_id,
                row.values,
                // A surviving row is live: its deleter, if any, aborted.
                TupleHeader {
                    xmax: Xid::INVALID,
                    ..row.hdr
                },
            );
            bytes += restored.bytes;
            rows.push(restored);
        }
        rows.sort_by_key(|row| row.row_id);
        self.bytes.store(bytes, Ordering::Relaxed);
        // Above every id the log mentions, including those just discarded: a
        // reused id would make a replayed delete hit the wrong row.
        self.next_row_id
            .store(restored.next_row_id, Ordering::Relaxed);
    }

    /// Snapshot-visible rows, materialized under one read lock.
    ///
    /// The rows are copied rather than streamed because the lock cannot be held
    /// across the iterator's life without blocking every writer for the duration
    /// of the caller's query.
    fn visible(&self, txn: &TxnContext, projection: &ColumnProjection) -> Vec<(Tid, Tuple)> {
        let width = self.schema.columns.len();
        self.rows_read()
            .iter()
            .filter(|row| satisfies_mvcc(&row.hdr, &txn.snapshot, &txn.clog, txn.xid, txn.cid))
            .map(|row| {
                let values = match projection {
                    ColumnProjection::All => row.values.clone(),
                    // Rows live in RAM already, so there is no read to skip —
                    // but cloning a `Value::Text` is not free, and a wide
                    // relation scanned for two columns clones the rest for
                    // nothing. Unselected slots stay `Null`, which the scan
                    // contract leaves unspecified.
                    ColumnProjection::Some(cols) => {
                        let mut values = vec![Value::Null; width];
                        for &index in cols.iter() {
                            // Indexed defensively: the `All` arm above passes a
                            // stored row through whatever its width, so a row
                            // narrower than the schema must degrade here too
                            // rather than panic inside a `TupleStream`.
                            if let Some(value) = row.values.get(index) {
                                values[index] = value.clone();
                            }
                        }
                        values
                    }
                };
                (Tid::logical(row.row_id), values)
            })
            .collect()
    }

    /// Mint `count` consecutive row ids.
    fn alloc_row_ids(&self, count: u64) -> Result<u64, StorageError> {
        let first = self.next_row_id.fetch_add(count, Ordering::Relaxed);
        if first.saturating_add(count) > MAX_ROW_ID {
            return Err(unsupported(format!(
                "buffer table \"{}\" has exhausted its row ids",
                self.schema.name
            )));
        }
        Ok(first)
    }

    /// Append `tuples`, WAL-logging them first.
    ///
    /// The record is appended but **not** flushed: the transaction's commit
    /// record is the durability boundary, and because the WAL is one ordered
    /// stream, flushing at commit makes every earlier append durable for free —
    /// with group commit amortizing the fsync across sessions.
    fn append(
        &self,
        rel: u32,
        tuples: Vec<Tuple>,
        txn: &TxnContext,
    ) -> Result<Vec<Tid>, StorageError> {
        if tuples.is_empty() {
            return Ok(Vec::new());
        }
        let first_row_id = self.alloc_row_ids(tuples.len() as u64)?;
        self.install(rel, first_row_id, tuples, txn)
    }

    /// Encode, WAL-log and install rows under ids already minted. Split from
    /// [`Self::append`] so a test can drive the install order independently of the
    /// allocation order.
    fn install(
        &self,
        rel: u32,
        first_row_id: u64,
        tuples: Vec<Tuple>,
        txn: &TxnContext,
    ) -> Result<Vec<Tid>, StorageError> {
        if tuples.is_empty() {
            return Ok(Vec::new());
        }
        let hdr = TupleHeader::inserted(txn.xid, txn.cid);

        // Encode outside every lock: this is the expensive step and nothing it
        // touches is shared.
        let mut staged = Vec::with_capacity(tuples.len());
        let mut total = 0usize;
        for (offset, values) in tuples.into_iter().enumerate() {
            let row = BufferRow::new(first_row_id + offset as u64, values, hdr);
            total += row.bytes;
            staged.push(row);
        }

        // Held from before the append until after `bytes` is published, because a
        // buffered row's ONLY durable trace is its WAL record — until a flush
        // writes it into a fsynced Parquet fragment, replay is what rebuilds it.
        // A checkpoint therefore refuses to publish a redo point while any buffer
        // holds rows, and it decides that by reading `bytes`. Without this guard a
        // sampler could see `bytes == 0` between the append and the install below,
        // publish a redo above the record, and lose the rows outright.
        //
        // The encoding above stays outside: it is the expensive step and touches
        // nothing shared, and the barrier should span only in-memory work.
        let _delay = self.wal.delay_checkpoint();
        for chunk in encode_insert(rel, txn.cid, &staged) {
            self.wal.append(RMGR_BUFFER, BUFFER_INSERT, txn.xid, &chunk);
        }

        let tids = staged.iter().map(|row| Tid::logical(row.row_id)).collect();
        {
            let mut rows = self.rows_write();
            // Ids come from one counter, but they are minted before this lock is
            // taken and the encoding in between is deliberately unlocked — so two
            // writers CAN arrive out of order, and a plain `extend` would leave
            // `rows` unsorted forever. Every lookup here binary-searches, so that
            // would silently lose rows: a flush would copy them into a chunk and
            // then fail to tombstone them, duplicating them permanently.
            //
            // In-order arrival is the overwhelmingly common case, so check the
            // boundary and fall back to positional inserts only when it fails.
            let in_order = rows
                .last()
                .is_none_or(|last| last.row_id < staged[0].row_id);
            if in_order {
                rows.extend(staged);
            } else {
                for row in staged {
                    let at = rows.partition_point(|existing| existing.row_id < row.row_id);
                    rows.insert(at, row);
                }
            }
        }
        self.bytes.fetch_add(total, Ordering::Relaxed);
        Ok(tids)
    }

    /// Stamp the rows named by `row_ids` deleted by `txn`, WAL-logging the ones
    /// actually stamped.
    ///
    /// A row is stampable unless a *committed* transaction already deleted it; an
    /// aborted or in-flight deleter leaves it live. Doing the check and the stamp
    /// under one write lock is the serialization point, so two concurrent
    /// deleters of the same row cannot both succeed.
    ///
    /// Deliberately does **not** credit the byte accounting. A stamped row is
    /// still in `rows` and still occupying every byte it did before, so a
    /// number that reports resident memory has to keep counting it; only
    /// [`TableAm::vacuum`], which actually drops it, may subtract.
    fn stamp_deleted(&self, rel: u32, row_ids: &[u64], txn: &TxnContext) -> Vec<u64> {
        let mut stamped = Vec::with_capacity(row_ids.len());
        {
            let mut rows = self.rows_write();
            for row_id in row_ids {
                let Ok(index) = rows.binary_search_by_key(row_id, |row| row.row_id) else {
                    continue;
                };
                let row = &mut rows[index];
                let live = !row.hdr.xmax.is_valid()
                    || txn.clog.status(row.hdr.xmax) == XactStatus::Aborted;
                if !live {
                    continue;
                }
                row.hdr.xmax = txn.xid;
                row.hdr.cmax = txn.cid;
                stamped.push(*row_id);
            }
        }
        // No checkpoint barrier here, unlike `install`: this mutates RAM *before*
        // appending, so any redo point sampled mid-window is below the record and
        // the record is replayed. It also never changes `bytes`, so the rows stay
        // counted and a checkpoint keeps refusing to bound itself while they do.
        if !stamped.is_empty() {
            self.wal.append(
                RMGR_BUFFER,
                BUFFER_DELETE,
                txn.xid,
                &encode_delete(rel, txn.cid, &stamped),
            );
        }
        stamped
    }
}

/// The largest `BUFFER_INSERT` payload emitted in one record.
///
/// `Wal::append` stages bytes under one mutex, so a single huge record would
/// hold it for one enormous copy and stall every other session's group commit.
/// Splitting bounds that hold; the rows are contiguous by id either way.
const MAX_INSERT_PAYLOAD: usize = 1 << 20;

/// Encode `rows` as one or more `BUFFER_INSERT` payloads.
///
/// Layout, little-endian throughout:
/// `[fmt:u8][rel:u32][cid:u32][first_row_id:u64][n_rows:u32][n_cols:u16]`
/// then per row a null bitmap of `ceil(n_cols/8)` bytes followed by
/// [`encode_datum`] for each non-null column in schema order.
fn encode_insert(rel: u32, cid: CommandId, rows: &[BufferRow]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < rows.len() {
        let mut payload = Vec::new();
        let mut end = start;
        // Reserve the header, then backfill the row count once the split point is
        // known — the alternative is encoding every row twice.
        let n_cols = rows[start].values.len() as u16;
        payload.push(PAYLOAD_FORMAT);
        payload.extend_from_slice(&rel.to_le_bytes());
        payload.extend_from_slice(&cid.0.to_le_bytes());
        payload.extend_from_slice(&rows[start].row_id.to_le_bytes());
        let count_at = payload.len();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&n_cols.to_le_bytes());
        while end < rows.len() {
            if end > start && payload.len() >= MAX_INSERT_PAYLOAD {
                break;
            }
            encode_row(&rows[end].values, &mut payload);
            end += 1;
        }
        let count = (end - start) as u32;
        payload[count_at..count_at + 4].copy_from_slice(&count.to_le_bytes());
        out.push(payload);
        start = end;
    }
    out
}

fn encode_row(values: &[Value], out: &mut Vec<u8>) {
    let bitmap_len = values.len().div_ceil(8);
    let bitmap_at = out.len();
    out.resize(bitmap_at + bitmap_len, 0);
    for (index, value) in values.iter().enumerate() {
        if matches!(value, Value::Null) {
            out[bitmap_at + index / 8] |= 1 << (index % 8);
        } else {
            encode_datum(value, out);
        }
    }
}

/// `[fmt:u8][rel:u32][cid:u32][n:u32]` then `n` row ids.
fn encode_delete(rel: u32, cid: CommandId, row_ids: &[u64]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(13 + row_ids.len() * 8);
    payload.push(PAYLOAD_FORMAT);
    payload.extend_from_slice(&rel.to_le_bytes());
    payload.extend_from_slice(&cid.0.to_le_bytes());
    payload.extend_from_slice(&(row_ids.len() as u32).to_le_bytes());
    for row_id in row_ids {
        payload.extend_from_slice(&row_id.to_le_bytes());
    }
    payload
}

/// Cursor over a WAL payload. Every read is checked, so a truncated record is a
/// clean error rather than a panic.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WalError> {
        if self.pos + n > self.buf.len() {
            return Err(WalError::Redo(
                "truncated buffer-table WAL payload".to_string(),
            ));
        }
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, WalError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, WalError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, WalError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, WalError> {
        let bytes = self.take(8)?;
        let mut wide = [0u8; 8];
        wide.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(wide))
    }
}

/// Replays buffer-table records, accumulating each relation's rows for
/// [`BufferTable::restore`] to install once the CLOG is rebuilt.
///
/// Replay cannot decide visibility itself: a record's transaction may commit
/// later in the same log. So it applies every record unconditionally and leaves
/// the filtering to `restore`, which runs after the whole log is read.
#[derive(Default)]
pub struct BufferRedo {
    buffers: Mutex<HashMap<u32, RestoredBuffer>>,
}

impl BufferRedo {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the rows replayed for `rel`, leaving nothing behind — each relation
    /// is restored exactly once.
    pub fn take(&self, rel: u32) -> Option<RestoredBuffer> {
        self.buffers
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .remove(&rel)
    }

    /// Drop everything replay collected that nobody claimed.
    ///
    /// Rows logged under a relfilenode a committed TRUNCATE superseded, or under a
    /// relation that was later dropped, have no table to restore them into. They
    /// are correctly unreachable — but without this they would sit decoded in RAM
    /// for the life of the process, invisible to every accounting and reclaimable
    /// by nothing.
    pub fn discard_unclaimed(&self) {
        self.buffers
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .clear();
    }
}

impl RmgrRedo for BufferRedo {
    fn redo(&self, ctx: &RedoContext) -> Result<(), WalError> {
        let mut cursor = Cursor::new(ctx.payload);
        let format = cursor.u8()?;
        if format != PAYLOAD_FORMAT {
            return Err(WalError::Redo(format!(
                "unsupported buffer-table payload format {format}"
            )));
        }
        let rel = cursor.u32()?;
        let cid = CommandId(cursor.u32()?);
        let mut buffers = self
            .buffers
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let buffer = buffers.entry(rel).or_default();
        match ctx.info {
            BUFFER_INSERT => {
                let first_row_id = cursor.u64()?;
                let n_rows = cursor.u32()?;
                let n_cols = cursor.u16()? as usize;
                let hdr = TupleHeader::inserted(ctx.xid, cid);
                for offset in 0..n_rows as u64 {
                    let values = decode_row(&mut cursor, n_cols)?;
                    let row_id = first_row_id + offset;
                    buffer.index.insert(row_id, buffer.rows.len());
                    buffer.rows.push(RestoredRow {
                        row_id,
                        values,
                        hdr,
                    });
                }
                buffer.next_row_id = buffer.next_row_id.max(first_row_id + n_rows as u64);
            }
            BUFFER_DELETE => {
                let n = cursor.u32()?;
                for _ in 0..n {
                    let row_id = cursor.u64()?;
                    if let Some(row) = buffer
                        .index
                        .get(&row_id)
                        .and_then(|at| buffer.rows.get_mut(*at))
                    {
                        row.hdr.xmax = ctx.xid;
                        row.hdr.cmax = cid;
                    }
                    buffer.next_row_id = buffer.next_row_id.max(row_id + 1);
                }
            }
            other => {
                return Err(WalError::Redo(format!(
                    "unknown buffer-table WAL record info byte {other:#x}"
                )));
            }
        }
        Ok(())
    }
}

fn decode_row(cursor: &mut Cursor<'_>, n_cols: usize) -> Result<Tuple, WalError> {
    let bitmap = cursor.take(n_cols.div_ceil(8))?.to_vec();
    let mut values = Vec::with_capacity(n_cols);
    for index in 0..n_cols {
        if bitmap[index / 8] & (1 << (index % 8)) != 0 {
            values.push(Value::Null);
        } else {
            let mut pos = cursor.pos;
            let value = decode_datum(cursor.buf, &mut pos);
            cursor.pos = pos;
            values.push(value);
        }
    }
    Ok(values)
}

impl TableAm for BufferTable {
    fn schema(&self) -> Arc<TableSchema> {
        Arc::clone(&self.schema)
    }

    fn indexes(&self) -> Vec<IndexMetadata> {
        self.indexes
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .clone()
    }

    fn scan_label(&self) -> String {
        match &self.scan_label {
            Some(label) => label.clone(),
            None => format!("Seq Scan on {}", self.schema.name),
        }
    }

    /// Exact, because the rows are in memory and counting them is a walk of a
    /// `Vec` — no sampling and no estimate needed.
    ///
    /// A set `xmax` alone does not make a row dead: an aborted deleter leaves it
    /// live, and `vacuum` never clears the field, so counting on `xmax.is_valid()`
    /// would report a relation as permanently empty after one rolled-back DELETE.
    fn statistics(&self) -> RelStats {
        let live = self.live_rows();
        RelStats::exact(live, &self.schema)
    }

    fn scan(&self, txn: &TxnContext, projection: &ColumnProjection) -> TupleStream {
        Box::new(self.visible(txn, projection).into_iter().map(Ok))
    }

    fn supports_batch_scan(&self) -> bool {
        true
    }

    /// Unlike the Parquet chunk store, this leaf holds rows, so a batch here is
    /// built rather than passed through. It is still worth doing: this table is
    /// one half of a buffered Parquet relation, and a leaf that could only speak
    /// rows would force the whole relation's scan back onto the row path.
    ///
    /// The conversion is bounded by the flush policy, which is what keeps the
    /// buffer small relative to the fragments beside it.
    ///
    /// `projection` is honored exactly as the row path honors it: [`Self::visible`]
    /// returns full-width tuples with `Null` in the unselected slots, which is
    /// also what the [`BatchStream`] contract asks for, so the same call serves
    /// both. Skipping it would clone every `Value::Text` in the buffer for
    /// columns nobody asked for, and would make this leaf disagree with the
    /// Parquet leaf beside it about what an unprojected slot holds.
    fn scan_batches(&self, txn: &TxnContext, projection: &ColumnProjection) -> Option<BatchStream> {
        let width = self.schema.columns.len();
        let rows: Vec<Tuple> = self
            .visible(txn, projection)
            .into_iter()
            .map(|(_, mut tuple)| {
                // The `All` arm of `visible` passes a stored row through at
                // whatever width it has. The row path degrades on a short row
                // rather than panicking; pad here so the batch builder, which
                // rejects a width mismatch outright, degrades the same way.
                tuple.resize(width, Value::Null);
                tuple
            })
            .collect();
        // Chunked rather than one unbounded batch: a buffer at its soft
        // threshold would otherwise be live twice over, as tuples and as one
        // giant Arrow copy, before a single row reached the operator above.
        let schema = self.schema.clone();
        Some(Box::new(
            rows.into_iter()
                .collect::<Vec<_>>()
                .chunks(BATCH_ROWS)
                .map(|chunk| crabgresql_storage_api::arrow::build_scan_batch(&schema, chunk))
                .collect::<Vec<_>>()
                .into_iter(),
        ))
    }

    fn fetch(&self, tid: Tid, txn: &TxnContext) -> Result<Option<Tuple>, StorageError> {
        let Some(row_id) = tid.row_id() else {
            return Err(corrupt(format!(
                "buffer table \"{}\" was handed a physical tid",
                self.schema.name
            )));
        };
        let rows = self.rows_read();
        let Ok(index) = rows.binary_search_by_key(&row_id, |row| row.row_id) else {
            return Ok(None);
        };
        let row = &rows[index];
        Ok(
            satisfies_mvcc(&row.hdr, &txn.snapshot, &txn.clog, txn.xid, txn.cid)
                .then(|| row.values.clone()),
        )
    }

    fn insert(&self, tuple: Tuple, txn: &TxnContext) -> Result<Tid, StorageError> {
        let tids = self.append(self.relfilenode(), vec![tuple], txn)?;
        tids.into_iter()
            .next()
            .ok_or_else(|| corrupt("buffer table insert produced no tid".to_string()))
    }

    fn insert_many(&self, tuples: Vec<Tuple>, txn: &TxnContext) -> Result<Vec<Tid>, StorageError> {
        self.append(self.relfilenode(), tuples, txn)
    }

    /// Update is delete-then-insert, as in the heap: the old version is stamped
    /// first, so two concurrent updaters serialize and the loser adds no second
    /// successor.
    fn update(
        &self,
        tid: Tid,
        tuple: Tuple,
        txn: &TxnContext,
    ) -> Result<UpdateResult, StorageError> {
        let Some(row_id) = tid.row_id() else {
            return Err(corrupt(format!(
                "buffer table \"{}\" was handed a physical tid",
                self.schema.name
            )));
        };
        let rel = self.relfilenode();
        if self.stamp_deleted(rel, &[row_id], txn).is_empty() {
            return Ok(UpdateResult::NotFound);
        }
        self.append(rel, vec![tuple], txn)?;
        Ok(UpdateResult::Updated)
    }

    fn delete(&self, tid: Tid, txn: &TxnContext) -> Result<DeleteResult, StorageError> {
        let Some(row_id) = tid.row_id() else {
            return Err(corrupt(format!(
                "buffer table \"{}\" was handed a physical tid",
                self.schema.name
            )));
        };
        if self
            .stamp_deleted(self.relfilenode(), &[row_id], txn)
            .is_empty()
        {
            return Ok(DeleteResult::NotFound);
        }
        Ok(DeleteResult::Deleted)
    }

    /// One pass and one WAL record for the whole batch, rather than the trait
    /// default's record per row.
    fn delete_many(&self, tids: Vec<Tid>, txn: &TxnContext) -> Result<u64, StorageError> {
        let row_ids = self.row_ids_of(tids)?;
        Ok(self.stamp_deleted(self.relfilenode(), &row_ids, txn).len() as u64)
    }

    /// Reclaim versions dead to every snapshot at or before `oldest`.
    ///
    /// This is where a buffer table's memory actually comes back. Rows are held
    /// after a committed delete because an older snapshot may still need them —
    /// dropping them at delete time would break `REPEATABLE READ`, and would
    /// break a flush, whose whole correctness argument is that the pre-flush copy
    /// stays readable to snapshots that predate it.
    fn vacuum(&self, oldest: Xid, clog: &Clog) {
        let mut rows = self.rows_write();
        let mut freed = 0usize;
        rows.retain(|row| {
            let dead_insert = clog.status(row.hdr.xmin) == XactStatus::Aborted;
            let dead_delete = row.hdr.xmax.is_valid()
                && row.hdr.xmax < oldest
                && clog.status(row.hdr.xmax) == XactStatus::Committed;
            if dead_insert || dead_delete {
                freed += row.bytes;
                return false;
            }
            true
        });
        self.bytes.fetch_sub(freed, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crabgresql_storage_api::{Column, TableAccessMethod};
    use crabgresql_txn::{CommitSink, TransactionManager};
    use crabgresql_types::PgType;
    use crabgresql_wal::{RmgrRegistry, recover};

    use super::*;

    fn schema(name: &str) -> TableSchema {
        let mut schema = TableSchema::new(
            name,
            vec![
                Column::new("id", PgType::Int4),
                Column::new("label", PgType::Text),
            ],
        );
        schema.access_method = TableAccessMethod::Buffer;
        schema
    }

    fn manager(wal: &Arc<Wal>) -> TransactionManager {
        let sink: Arc<dyn CommitSink> = Arc::clone(wal) as Arc<dyn CommitSink>;
        TransactionManager::new_recovered(sink, Arc::new(Clog::new()), Xid::FIRST_NORMAL)
    }

    fn row(id: i32, label: &str) -> Tuple {
        vec![Value::Int4(id), Value::Text(label.to_string())]
    }

    /// Every id a scan can see, sorted so assertions do not depend on row order.
    fn visible_ids(table: &BufferTable, txn: &TxnContext) -> Vec<i32> {
        let mut ids: Vec<i32> = table
            .scan(txn, &ColumnProjection::All)
            .map(|row| match row.expect("scan must not fail").1[0] {
                Value::Int4(id) => id,
                ref other => panic!("unexpected id value {other:?}"),
            })
            .collect();
        ids.sort_unstable();
        ids
    }

    fn open(dir: &Path) -> anyhow::Result<(Arc<Wal>, BufferTable)> {
        let wal = Arc::new(Wal::open(dir)?);
        let table = BufferTable::open(7, schema("b"), Vec::new(), Arc::clone(&wal));
        Ok((wal, table))
    }

    #[test]
    fn a_row_is_invisible_to_its_own_command_and_visible_to_the_next() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (wal, table) = open(dir.path())?;
        let tm = manager(&wal);
        let xid = tm.allocate_xid();

        let insert = tm.context(xid, CommandId::FIRST);
        table.insert(row(1, "one"), &insert)?;
        // The inserting command must not see its own row, or an `INSERT ...
        // SELECT` from the same table would feed on its own output.
        assert_eq!(visible_ids(&table, &insert), Vec::<i32>::new());

        let next = tm.context(xid, CommandId(insert.cid.0 + 1));
        assert_eq!(visible_ids(&table, &next), vec![1]);
        Ok(())
    }

    #[test]
    fn another_session_sees_a_row_only_after_it_commits() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (wal, table) = open(dir.path())?;
        let tm = manager(&wal);

        let writer_xid = tm.allocate_xid();
        let writer = tm.context(writer_xid, CommandId::FIRST);
        table.insert(row(1, "one"), &writer)?;

        // A snapshot taken while the writer is in flight must never see the row,
        // even after the writer commits: the verdict is fixed at snapshot time.
        let early = tm.context(tm.allocate_xid(), CommandId::FIRST);
        assert_eq!(visible_ids(&table, &early), Vec::<i32>::new());
        tm.commit(writer_xid)?;
        assert_eq!(
            visible_ids(&table, &early),
            Vec::<i32>::new(),
            "committing must not retroactively change an existing snapshot"
        );

        let late = tm.context(tm.allocate_xid(), CommandId::FIRST);
        assert_eq!(visible_ids(&table, &late), vec![1]);
        Ok(())
    }

    /// The invariant the accounting used to violate. A row's charge cannot be
    /// less than the `Vec<Value>` it holds, because that memory is resident
    /// whether or not the values in it are large — `size_of::<Value>()` is paid
    /// per column, not per byte of payload. Measuring the *encoded* row instead
    /// under-reported a 105-column ClickBench row roughly sevenfold.
    #[test]
    fn a_rows_charge_covers_its_value_spine() {
        let hdr = TupleHeader::inserted(Xid::FIRST_NORMAL, CommandId::FIRST);
        let values = row(1, "one");
        let columns = values.len();
        let charged = BufferRow::new(0, values, hdr).bytes;

        let spine = size_of::<BufferRow>() + columns * size_of::<Value>();
        assert!(
            charged >= spine,
            "charged {charged} for {columns} columns, but the spine alone is {spine}"
        );
    }

    /// The same invariant at the shape that exposed it, bracketed from both
    /// sides. The lower bound catches a regression back toward encoded size;
    /// the upper bound catches the opposite mistake — charging each value its
    /// inline `size_of::<Value>()` on top of the spine that already holds it
    /// would add 56 bytes a column and blow the allowance.
    #[test]
    fn a_wide_row_is_charged_at_the_hits_shape() {
        const COLUMNS: usize = 105;
        const TEXTS: usize = 40;
        const TEXT_LEN: usize = 24;

        let mut values: Tuple = Vec::with_capacity(COLUMNS);
        for column in 0..COLUMNS {
            if column < TEXTS {
                values.push(Value::Text("x".repeat(TEXT_LEN)));
            } else {
                values.push(Value::Int4(column as i32));
            }
        }
        let hdr = TupleHeader::inserted(Xid::FIRST_NORMAL, CommandId::FIRST);
        let charged = BufferRow::new(0, values, hdr).bytes;

        let base = size_of::<BufferRow>() + COLUMNS * size_of::<Value>();
        assert!(
            charged >= base + TEXTS * TEXT_LEN,
            "charged {charged}, below the {base} spine plus its text"
        );
        assert!(
            charged < base + TEXTS * (TEXT_LEN + 48),
            "charged {charged}, too far above the {base} spine plus its text — \
             the spine is likely being counted twice"
        );
    }

    #[test]
    fn an_aborted_inserts_rows_are_invisible_and_vacuum_reclaims_them() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (wal, table) = open(dir.path())?;
        let tm = manager(&wal);

        let xid = tm.allocate_xid();
        table.insert_many(
            vec![row(1, "one"), row(2, "two")],
            &tm.context(xid, CommandId::FIRST),
        )?;
        let charged = table.resident_bytes();
        assert!(charged > 0, "rows must be charged to the byte accounting");
        tm.abort(xid);

        let reader = tm.context(tm.allocate_xid(), CommandId::FIRST);
        assert_eq!(visible_ids(&table, &reader), Vec::<i32>::new());

        table.vacuum(tm.snapshot().xmin, tm.clog());
        assert_eq!(
            table.resident_bytes(),
            0,
            "vacuum must return an aborted transaction's memory"
        );
        Ok(())
    }

    #[test]
    fn a_committed_delete_is_retained_for_older_snapshots_then_vacuumed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (wal, table) = open(dir.path())?;
        let tm = manager(&wal);

        let writer = tm.allocate_xid();
        let tids = table.insert_many(
            vec![row(1, "one"), row(2, "two")],
            &tm.context(writer, CommandId::FIRST),
        )?;
        tm.commit(writer)?;

        // An open snapshot that predates the delete.
        let older = tm.context(tm.allocate_xid(), CommandId::FIRST);
        assert_eq!(visible_ids(&table, &older), vec![1, 2]);

        let deleter = tm.allocate_xid();
        let deleted = table.delete_many(vec![tids[0]], &tm.context(deleter, CommandId::FIRST))?;
        assert_eq!(deleted, 1);
        tm.commit(deleter)?;

        // This is the property a flush depends on: the pre-delete copy stays
        // readable to a snapshot taken before the deleting transaction.
        assert_eq!(
            visible_ids(&table, &older),
            vec![1, 2],
            "a committed delete must not disturb an older snapshot"
        );
        let newer = tm.context(tm.allocate_xid(), CommandId::FIRST);
        assert_eq!(visible_ids(&table, &newer), vec![2]);

        // With the old snapshot's xmin as the floor, the row is not yet
        // reclaimable; above the deleter it is.
        table.vacuum(older.snapshot.xmin, tm.clog());
        assert_eq!(visible_ids(&table, &older), vec![1, 2]);
        table.vacuum(tm.allocate_xid(), tm.clog());
        assert_eq!(visible_ids(&table, &newer), vec![2]);
        Ok(())
    }

    #[test]
    fn update_replaces_the_row_and_keeps_one_live_version() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (wal, table) = open(dir.path())?;
        let tm = manager(&wal);

        let writer = tm.allocate_xid();
        let tids = table.insert_many(vec![row(1, "one")], &tm.context(writer, CommandId::FIRST))?;
        tm.commit(writer)?;

        let updater = tm.allocate_xid();
        let txn = tm.context(updater, CommandId::FIRST);
        assert_eq!(
            table.update(tids[0], row(9, "nine"), &txn)?,
            UpdateResult::Updated
        );
        // A second update of the same version finds it already stamped.
        assert_eq!(
            table.update(tids[0], row(8, "eight"), &txn)?,
            UpdateResult::NotFound
        );
        tm.commit(updater)?;

        let reader = tm.context(tm.allocate_xid(), CommandId::FIRST);
        assert_eq!(visible_ids(&table, &reader), vec![9]);
        Ok(())
    }

    #[test]
    fn a_logical_tid_fetches_and_a_physical_one_is_rejected() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (wal, table) = open(dir.path())?;
        let tm = manager(&wal);

        let writer = tm.allocate_xid();
        let tids = table.insert_many(vec![row(1, "one")], &tm.context(writer, CommandId::FIRST))?;
        tm.commit(writer)?;
        assert!(
            tids[0].is_logical(),
            "a buffer table must mint logical tids"
        );

        let reader = tm.context(tm.allocate_xid(), CommandId::FIRST);
        assert_eq!(table.fetch(tids[0], &reader)?, Some(row(1, "one")));
        assert_eq!(table.fetch(Tid::logical(9_999), &reader)?, None);
        // A physical tid could only arrive by routing a fetch to the wrong
        // storage, which must be loud rather than silently reading nothing.
        assert!(matches!(
            table.fetch(Tid::new(1, 1), &reader),
            Err(StorageError::CorruptData(_))
        ));
        Ok(())
    }

    /// A restart must not change what the same rows are reported to cost, or a
    /// relation's flush schedule would silently shift the moment the server
    /// came back up.
    ///
    /// Exactness is available here because both sides allocate minimally — the
    /// test builds its tuples with an exact capacity and `decode_row` uses
    /// `Vec::with_capacity(n_cols)`. It deliberately stays on `int4`/`text`:
    /// `numeric`, `jsonb` and `tsvector` are reconstructed by re-parsing their
    /// canonical text on decode, so their capacities are the parser's and need
    /// not match the producer's.
    #[test]
    fn a_restored_row_is_charged_what_the_insert_charged() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let charged_when_inserted;
        {
            let (wal, table) = open(dir.path())?;
            let tm = manager(&wal);
            let xid = tm.allocate_xid();
            table.insert_many(
                vec![
                    row(1, "one"),
                    row(2, "two"),
                    row(3, "a rather longer label"),
                ],
                &tm.context(xid, CommandId::FIRST),
            )?;
            tm.commit(xid)?;
            charged_when_inserted = table.resident_bytes();
            wal.flush(wal.current_lsn())?;
        }

        let redo = Arc::new(BufferRedo::new());
        let mut registry = RmgrRegistry::new();
        registry.register(RMGR_BUFFER, Arc::clone(&redo) as Arc<dyn RmgrRedo>);
        let clog = Clog::new();
        recover(dir.path(), &registry, &clog, crabgresql_wal::Lsn::INVALID)?;

        let wal = Arc::new(Wal::open(dir.path())?);
        let table = BufferTable::open(7, schema("b"), Vec::new(), wal);
        table.restore(
            redo.take(7).expect("replay must recover the relation"),
            &clog,
        );

        assert_eq!(
            table.resident_bytes(),
            charged_when_inserted,
            "the same rows must cost the same before and after a restart"
        );
        Ok(())
    }

    #[test]
    fn committed_rows_are_rebuilt_from_the_wal_and_uncommitted_ones_are_not() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        {
            let (wal, table) = open(dir.path())?;
            let tm = manager(&wal);

            let kept = tm.allocate_xid();
            let tids = table.insert_many(
                vec![row(1, "one"), row(2, "two"), row(3, "three")],
                &tm.context(kept, CommandId::FIRST),
            )?;
            // Delete one of them in the same committed transaction.
            table.delete_many(vec![tids[2]], &tm.context(kept, CommandId(1)))?;
            tm.commit(kept)?;

            let rolled_back = tm.allocate_xid();
            table.insert_many(
                vec![row(4, "four")],
                &tm.context(rolled_back, CommandId::FIRST),
            )?;
            tm.abort(rolled_back);

            // Never resolved — the crash happens here. Force the record to disk
            // so replay genuinely sees an insert with no commit or abort after
            // it; `flushed_lsn` is what is *already* durable, so flushing to it
            // would be a no-op and the record would simply vanish.
            let torn = tm.allocate_xid();
            table.insert_many(vec![row(5, "five")], &tm.context(torn, CommandId::FIRST))?;
            wal.flush(wal.current_lsn())?;
        }

        // Restart: replay the whole log, then install what committed.
        let redo = Arc::new(BufferRedo::new());
        let mut registry = RmgrRegistry::new();
        registry.register(RMGR_BUFFER, Arc::clone(&redo) as Arc<dyn RmgrRedo>);
        let clog = Clog::new();
        let result = recover(dir.path(), &registry, &clog, crabgresql_wal::Lsn::INVALID)?;

        let wal = Arc::new(Wal::open(dir.path())?);
        let table = BufferTable::open(7, schema("b"), Vec::new(), wal);
        let restored = redo.take(7).expect("replay must recover the relation");
        table.restore(restored, &clog);

        let tm =
            TransactionManager::new_recovered(Arc::new(NullSink), Arc::new(clog), result.next_xid);
        let reader = tm.context(tm.allocate_xid(), CommandId::FIRST);
        assert_eq!(
            visible_ids(&table, &reader),
            vec![1, 2],
            "only the committed transaction's surviving rows may come back"
        );
        // A new insert must not reuse a recovered row id, or a replayed delete
        // would later hit the wrong row.
        let writer = tm.allocate_xid();
        let fresh =
            table.insert_many(vec![row(6, "six")], &tm.context(writer, CommandId::FIRST))?;
        assert!(
            fresh[0].row_id().expect("logical tid") >= 5,
            "row ids must resume above every id the log mentions"
        );
        Ok(())
    }

    /// A commit sink for the post-recovery manager in tests that have already
    /// closed their WAL.
    struct NullSink;

    impl CommitSink for NullSink {
        fn log_commit(&self, _xid: Xid) -> std::io::Result<()> {
            Ok(())
        }
        fn log_abort(&self, _xid: Xid) {}
    }

    /// Row ids are minted before the install lock is taken, so two writers can
    /// arrive out of order. Every lookup binary-searches, so an unsorted `rows`
    /// silently loses rows — they get copied into a chunk and never tombstoned.
    #[test]
    fn out_of_order_appends_stay_searchable() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (wal, table) = open(dir.path())?;
        let tm = manager(&wal);

        // Mint B's ids first but install A's rows first, which is exactly what an
        // interleaving of the unlocked encode step produces.
        let first = table.alloc_row_ids(2)?;
        let second = table.alloc_row_ids(2)?;
        assert!(first < second);
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        table.append_at(second, vec![row(3, "c"), row(4, "d")], &txn)?;
        table.append_at(first, vec![row(1, "a"), row(2, "b")], &txn)?;
        tm.commit(xid)?;

        let reader = tm.context(tm.allocate_xid(), CommandId::FIRST);
        assert_eq!(visible_ids(&table, &reader), vec![1, 2, 3, 4]);
        // The out-of-order ids must still resolve, or a flush would copy them to a
        // chunk and fail to tombstone them, duplicating them permanently.
        for row_id in [first, first + 1, second, second + 1] {
            assert!(
                table.fetch(Tid::logical(row_id), &reader)?.is_some(),
                "row {row_id} must be reachable after an out-of-order install"
            );
        }
        let stamped = table.delete_many_in(
            table.relfilenode(),
            (first..first + 2)
                .chain(second..second + 2)
                .map(Tid::logical)
                .collect(),
            &tm.context(tm.allocate_xid(), CommandId::FIRST),
        )?;
        assert_eq!(stamped, 4, "every row must be tombstonable");
        Ok(())
    }

    /// A rolled-back DELETE leaves `xmax` set, and nothing ever clears it, so
    /// counting live rows by "xmax is unset" reports the relation permanently
    /// empty — which flows straight into `pg_class.reltuples`.
    #[test]
    fn an_aborted_delete_does_not_make_rows_look_gone() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (wal, table) = open(dir.path())?;
        let tm = manager(&wal);
        table.attach_clog(Arc::clone(tm.clog()));

        let writer = tm.allocate_xid();
        let tids = table.insert_many(
            vec![row(1, "a"), row(2, "b"), row(3, "c")],
            &tm.context(writer, CommandId::FIRST),
        )?;
        tm.commit(writer)?;
        assert_eq!(table.live_rows(), 3);

        let deleter = tm.allocate_xid();
        table.delete_many(tids.clone(), &tm.context(deleter, CommandId::FIRST))?;
        tm.abort(deleter);

        let reader = tm.context(tm.allocate_xid(), CommandId::FIRST);
        assert_eq!(visible_ids(&table, &reader), vec![1, 2, 3]);
        assert_eq!(
            table.live_rows(),
            3,
            "an aborted deleter leaves the rows live, so they must still be counted"
        );

        // And a committed delete really does drop the count.
        let deleter = tm.allocate_xid();
        table.delete_many(vec![tids[0]], &tm.context(deleter, CommandId::FIRST))?;
        tm.commit(deleter)?;
        assert_eq!(table.live_rows(), 2);
        Ok(())
    }

    #[test]
    fn every_supported_value_survives_a_wal_round_trip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut wide = TableSchema::new(
            "wide",
            vec![
                Column::new("a", PgType::Int4),
                Column::new("b", PgType::Text),
                Column::new("c", PgType::Bool),
                Column::new("d", PgType::Float8),
            ],
        );
        wide.access_method = TableAccessMethod::Buffer;
        let expected = vec![
            vec![
                Value::Int4(42),
                Value::Text("hello".to_string()),
                Value::Bool(true),
                Value::Float8(1.5),
            ],
            // A row that is entirely NULL exercises the bitmap with no datums.
            vec![Value::Null, Value::Null, Value::Null, Value::Null],
            vec![
                Value::Null,
                Value::Text(String::new()),
                Value::Null,
                Value::Float8(f64::NAN),
            ],
        ];
        {
            let wal = Arc::new(Wal::open(dir.path())?);
            let table = BufferTable::open(3, wide.clone(), Vec::new(), Arc::clone(&wal));
            let tm = manager(&wal);
            let xid = tm.allocate_xid();
            table.insert_many(expected.clone(), &tm.context(xid, CommandId::FIRST))?;
            tm.commit(xid)?;
        }

        let redo = Arc::new(BufferRedo::new());
        let mut registry = RmgrRegistry::new();
        registry.register(RMGR_BUFFER, Arc::clone(&redo) as Arc<dyn RmgrRedo>);
        let clog = Clog::new();
        let result = recover(dir.path(), &registry, &clog, crabgresql_wal::Lsn::INVALID)?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let table = BufferTable::open(3, wide, Vec::new(), wal);
        table.restore(redo.take(3).expect("replay recovers the relation"), &clog);

        let tm =
            TransactionManager::new_recovered(Arc::new(NullSink), Arc::new(clog), result.next_xid);
        let reader = tm.context(tm.allocate_xid(), CommandId::FIRST);
        let mut got: Vec<Tuple> = table
            .scan(&reader, &ColumnProjection::All)
            .map(|row| row.expect("scan must not fail").1)
            .collect();
        assert_eq!(got.len(), expected.len());
        // NaN != NaN, so compare it by bit pattern rather than by value.
        for (index, want) in expected.iter().enumerate() {
            for (col, value) in want.iter().enumerate() {
                match (value, &got[index][col]) {
                    (Value::Float8(a), Value::Float8(b)) if a.is_nan() => {
                        assert!(b.is_nan(), "NaN must survive the round trip");
                    }
                    (a, b) => assert_eq!(a, b, "row {index} column {col}"),
                }
            }
        }
        got.clear();
        Ok(())
    }

    #[test]
    fn a_large_batch_splits_into_several_wal_records_and_still_round_trips() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // Each row carries ~4 KiB of text, so 512 rows comfortably exceed the
        // 1 MiB record cap and force the split path.
        let rows: Vec<Tuple> = (0..512)
            .map(|id| vec![Value::Int4(id), Value::Text("x".repeat(4096))])
            .collect();
        {
            let (wal, table) = open(dir.path())?;
            let tm = manager(&wal);
            let xid = tm.allocate_xid();
            table.insert_many(rows.clone(), &tm.context(xid, CommandId::FIRST))?;
            tm.commit(xid)?;
        }

        let redo = Arc::new(BufferRedo::new());
        let mut registry = RmgrRegistry::new();
        registry.register(RMGR_BUFFER, Arc::clone(&redo) as Arc<dyn RmgrRedo>);
        let clog = Clog::new();
        let result = recover(dir.path(), &registry, &clog, crabgresql_wal::Lsn::INVALID)?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let table = BufferTable::open(7, schema("b"), Vec::new(), wal);
        table.restore(redo.take(7).expect("replay recovers the relation"), &clog);

        let tm =
            TransactionManager::new_recovered(Arc::new(NullSink), Arc::new(clog), result.next_xid);
        let reader = tm.context(tm.allocate_xid(), CommandId::FIRST);
        assert_eq!(visible_ids(&table, &reader), (0..512).collect::<Vec<i32>>());
        Ok(())
    }
}
