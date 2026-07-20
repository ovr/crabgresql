//! In-memory storage engine: reference implementation of the storage API.
//!
//! Tables live in process memory, write no WAL (effectively UNLOGGED) and are
//! lost on restart. Each row is stored as a chain of **versions** carrying a
//! [`TupleHeader`] (`xmin`/`xmax`/`cmin`/`cmax`); visibility is decided by the
//! shared [`satisfies_mvcc`] rule, so a `ROLLBACK` needs no undo — an
//! uncommitted version is simply invisible and later reclaimed by vacuum.
//!
//! Versions sit behind an `Arc` snapshot cell: a scan grabs the `Arc` in O(1)
//! and iterates a stable view while writers copy-on-write.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crabgresql_storage_api::{
    DeleteResult, IndexMetadata, RelationMetadata, StorageError, TableAm, TableEngine, TableSchema,
    Tid, Tuple, UpdateResult,
};
use crabgresql_txn::{Clog, TupleHeader, TxnContext, XactStatus, satisfies_mvcc};
use crabgresql_types::{PgType, Value};

#[derive(Default)]
pub struct MemoryEngine {
    tables: RwLock<HashMap<String, Arc<MemoryTable>>>,
}

impl MemoryEngine {
    pub fn new() -> Self {
        Self::default()
    }
}

impl TableEngine for MemoryEngine {
    fn create_table(&self, schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError> {
        let mut tables = self
            .tables
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        if tables.contains_key(&schema.name)
            || tables.values().any(|t| {
                t.indexes
                    .read()
                    .unwrap_or_else(|_| panic!("rwlock poisoned"))
                    .iter()
                    .any(|i| i.name == schema.name)
            })
        {
            return Err(StorageError::TableAlreadyExists(schema.name));
        }
        let table = Arc::new(MemoryTable {
            schema: schema.clone(),
            rows: RwLock::new(Arc::new(Vec::new())),
            next_tid: AtomicU64::new(0),
            indexes: RwLock::new(Vec::new()),
            phys: RwLock::new(Vec::new()),
        });
        tables.insert(schema.name, table.clone());
        Ok(table)
    }

    fn open_table(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        let tables = self
            .tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        tables
            .get(name)
            .cloned()
            .map(|t| t as Arc<dyn TableAm>)
            .ok_or_else(|| StorageError::TableNotFound(name.to_string()))
    }

    fn drop_table(&self, name: &str) -> Result<(), StorageError> {
        let mut tables = self
            .tables
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        tables
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| StorageError::TableNotFound(name.to_string()))
    }

    fn create_index(&self, table: &str, index: IndexMetadata) -> Result<(), StorageError> {
        let tables = self
            .tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        if tables.contains_key(&index.name)
            || tables.values().any(|t| {
                t.indexes
                    .read()
                    .unwrap_or_else(|_| panic!("rwlock poisoned"))
                    .iter()
                    .any(|i| i.name == index.name)
            })
        {
            return Err(StorageError::RelationAlreadyExists(index.name));
        }
        let target = tables
            .get(table)
            .ok_or_else(|| StorageError::IndexTableNotFound(table.to_string()))?;

        // Build the physical equality index up front by scanning every version
        // (dead ones included — the probe MVCC-filters them, just like `rows`).
        // The index is servable only when all its key columns have an
        // equality-canonical encoding; otherwise it stays metadata-only.
        //
        // Hold the `rows` read lock across BOTH the build scan and the `phys`
        // publish (lock order rows → phys): an `insert`/`update` takes the `rows`
        // write lock before recording into `phys`, so keeping the read lock here
        // blocks it until the new index is published. Otherwise a row inserted
        // between "snapshot rows" and "push index" would be missing from the
        // build snapshot yet also skip `record_in_indexes` (the index isn't in
        // `phys` yet), leaving it permanently absent from the index.
        let key_columns: Vec<usize> = index.keys.iter().map(|k| k.column).collect();
        let servable = key_columns
            .iter()
            .all(|&c| key_type_indexable(target.schema.columns[c].ty));
        let rows = target
            .rows
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let map = servable.then(|| {
            let mut map: HashMap<Vec<Vec<u8>>, Vec<Tid>> = HashMap::new();
            for v in rows.iter() {
                if let Some(key) = PhysicalIndex::encode_row(&key_columns, &v.tuple) {
                    map.entry(key).or_default().push(v.tid);
                }
            }
            map
        });
        target
            .phys
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .push(PhysicalIndex {
                name: index.name.clone(),
                key_columns,
                map,
            });
        drop(rows);

        target
            .indexes
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .push(index);
        Ok(())
    }

    fn relations(&self) -> Vec<TableSchema> {
        self.tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .values()
            .map(|t| t.schema.clone())
            .collect()
    }

    fn relation_metadata(&self) -> Vec<RelationMetadata> {
        self.tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .values()
            .map(|t| RelationMetadata {
                schema: t.schema.clone(),
                indexes: t
                    .indexes
                    .read()
                    .unwrap_or_else(|_| panic!("rwlock poisoned"))
                    .clone(),
            })
            .collect()
    }
}

/// One row version: its identity, MVCC header, and column values.
#[derive(Clone)]
struct Version {
    tid: Tid,
    header: TupleHeader,
    tuple: Tuple,
}

/// Whether an equality index on a key column of this type can be served by the
/// physical index. The type must have a byte encoding that coincides with SQL
/// `=`, so a hash probe never misses a live match. Types whose equality trims or
/// normalizes their representation (`bpchar` blank-padding, `numeric`/`float`
/// canonicalization) are excluded; an index whose key includes one stays
/// metadata-only and its queries fall back to a sequential scan.
fn key_type_indexable(ty: PgType) -> bool {
    matches!(
        ty,
        PgType::Bool
            | PgType::Int2
            | PgType::Int4
            | PgType::Int8
            | PgType::Oid
            | PgType::Text
            | PgType::Varchar
            | PgType::Name
            | PgType::Uuid
            | PgType::Date
    )
}

/// Equality-canonical byte encoding of one key value, or `None` when it is SQL
/// NULL — NULL never matches under `=`, so a NULL key is neither indexed on
/// insert nor probeable. Only called for [`key_type_indexable`] columns; any
/// other value shape under such a column is treated as unencodable so the probe
/// conservatively falls back.
fn encode_key_value(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Null => None,
        Value::Bool(b) => Some(vec![*b as u8]),
        Value::Int2(n) => Some(n.to_be_bytes().to_vec()),
        Value::Int4(n) => Some(n.to_be_bytes().to_vec()),
        Value::Int8(n) => Some(n.to_be_bytes().to_vec()),
        Value::Oid(n) => Some(n.to_be_bytes().to_vec()),
        Value::Text(s) => Some(s.as_bytes().to_vec()),
        Value::Uuid(b) => Some(b.to_vec()),
        Value::Date(d) => Some(d.to_be_bytes().to_vec()),
        _ => None,
    }
}

/// A physical equality index: maps an encoded key to every tid ever inserted
/// under it. Like [`MemoryTable::rows`] it is append-only — dead versions' tids
/// are left in place and filtered out at probe time by MVCC visibility.
struct PhysicalIndex {
    name: String,
    key_columns: Vec<usize>,
    /// `None` when any key column's type is not [`key_type_indexable`]: the
    /// index still exists as metadata, but the engine cannot serve equality
    /// probes for it, so `index_lookup` returns `None` and callers fall back to
    /// a sequential scan.
    map: Option<HashMap<Vec<Vec<u8>>, Vec<Tid>>>,
}

impl PhysicalIndex {
    /// Encode a row's composite key, or `None` if any key column is NULL
    /// (unindexable — such a row is skipped, matching `=`-never-matches-NULL).
    fn encode_row(key_columns: &[usize], tuple: &Tuple) -> Option<Vec<Vec<u8>>> {
        key_columns
            .iter()
            .map(|&c| encode_key_value(&tuple[c]))
            .collect()
    }

    /// Record `tid` under `tuple`'s key. No-op for an unservable index or a row
    /// whose key contains NULL.
    fn record(&mut self, tuple: &Tuple, tid: Tid) {
        let Some(map) = self.map.as_mut() else {
            return;
        };
        if let Some(key) = Self::encode_row(&self.key_columns, tuple) {
            map.entry(key).or_default().push(tid);
        }
    }
}

pub struct MemoryTable {
    schema: TableSchema,
    /// Every version ever inserted (including dead ones, until vacuum), always
    /// sorted ascending by `tid.packed()`: tids are allocated under the write
    /// lock and only appended, so the order never breaks and lookups binary
    /// search on it.
    rows: RwLock<Arc<Vec<Version>>>,
    next_tid: AtomicU64,
    indexes: RwLock<Vec<IndexMetadata>>,
    /// Physical equality indexes, one per servable [`IndexMetadata`]. Guarded by
    /// its own lock; whenever both are held the order is **`rows` then `phys`**
    /// (writers), and `index_lookup` releases `phys` before touching `rows` so
    /// the two never form a cycle.
    phys: RwLock<Vec<PhysicalIndex>>,
}

impl MemoryTable {
    /// Allocate a never-reused tid. Called under the write lock so the append
    /// that follows keeps `rows` tid-sorted.
    fn alloc_tid(&self) -> Tid {
        Tid::from_packed(self.next_tid.fetch_add(1, Ordering::Relaxed))
    }

    /// Record a freshly inserted version's `tid` in every physical index under
    /// `tuple`'s key. Callers hold the `rows` write lock, so acquiring `phys`
    /// here keeps the rows → phys lock order.
    fn record_in_indexes(&self, tuple: &Tuple, tid: Tid) {
        let mut phys = self
            .phys
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        for index in phys.iter_mut() {
            index.record(tuple, tid);
        }
    }
}

/// A version is still mutable (updatable/deletable) if it has not been deleted,
/// or if the transaction that deleted it aborted (so the delete never happened).
fn is_live(header: &TupleHeader, clog: &Clog) -> bool {
    !header.xmax.is_valid() || clog.status(header.xmax) == XactStatus::Aborted
}

fn find(rows: &[Version], tid: Tid) -> Option<usize> {
    rows.binary_search_by_key(&tid.packed(), |v| v.tid.packed())
        .ok()
}

/// Iterates a shared snapshot, yielding only versions visible to the capturing
/// transaction and cloning one tuple per `next()` call.
struct MvccScan {
    rows: Arc<Vec<Version>>,
    pos: usize,
    txn: TxnContext,
}

impl Iterator for MvccScan {
    type Item = (Tid, Tuple);

    fn next(&mut self) -> Option<(Tid, Tuple)> {
        while let Some(v) = self.rows.get(self.pos) {
            self.pos += 1;
            if satisfies_mvcc(
                &v.header,
                &self.txn.snapshot,
                &self.txn.clog,
                self.txn.xid,
                self.txn.cid,
            ) {
                return Some((v.tid, v.tuple.clone()));
            }
        }
        None
    }
}

impl TableAm for MemoryTable {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    fn indexes(&self) -> Vec<IndexMetadata> {
        self.indexes
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .clone()
    }

    fn scan(&self, txn: &TxnContext) -> Box<dyn Iterator<Item = (Tid, Tuple)> + Send> {
        let rows = Arc::clone(
            &self
                .rows
                .read()
                .unwrap_or_else(|_| panic!("rwlock poisoned")),
        );
        Box::new(MvccScan {
            rows,
            pos: 0,
            txn: txn.clone(),
        })
    }

    fn fetch(&self, tid: Tid, txn: &TxnContext) -> Option<Tuple> {
        let rows = self
            .rows
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let pos = find(&rows, tid)?;
        let v = &rows[pos];
        satisfies_mvcc(&v.header, &txn.snapshot, &txn.clog, txn.xid, txn.cid)
            .then(|| v.tuple.clone())
    }

    fn supports_index_scan(&self, index_name: &str) -> bool {
        self.phys
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .iter()
            .any(|p| p.name == index_name && p.map.is_some())
    }

    fn index_lookup(
        &self,
        index_name: &str,
        key: &[Value],
        txn: &TxnContext,
    ) -> Option<Box<dyn Iterator<Item = (Tid, Tuple)> + Send>> {
        // Collect candidate tids under the `phys` lock, then release it before
        // reading `rows` so the two locks never form a cycle (writers take
        // rows → phys). An empty result is still `Some`: the index served the
        // probe, it just found no matching key. `None` means unservable — the
        // caller falls back to a sequential scan.
        let tids = {
            let phys = self
                .phys
                .read()
                .unwrap_or_else(|_| panic!("rwlock poisoned"));
            let index = phys.iter().find(|p| p.name == index_name)?;
            let map = index.map.as_ref()?;
            match key.iter().map(encode_key_value).collect::<Option<Vec<_>>>() {
                // A NULL in the probe key never matches under `=`.
                None => Vec::new(),
                Some(encoded) => map.get(&encoded).cloned().unwrap_or_default(),
            }
        };
        // MVCC-filter the candidates against the caller's snapshot.
        let rows = Arc::clone(
            &self
                .rows
                .read()
                .unwrap_or_else(|_| panic!("rwlock poisoned")),
        );
        let txn = txn.clone();
        let mut out = Vec::with_capacity(tids.len());
        for tid in tids {
            if let Some(pos) = find(&rows, tid) {
                let v = &rows[pos];
                if satisfies_mvcc(&v.header, &txn.snapshot, &txn.clog, txn.xid, txn.cid) {
                    out.push((v.tid, v.tuple.clone()));
                }
            }
        }
        Some(Box::new(out.into_iter()))
    }

    fn insert(&self, tuple: Tuple, txn: &TxnContext) -> Tid {
        // Copy-on-write: cheap append normally, clones the Vec only while a
        // concurrent scan still holds the previous snapshot.
        let mut rows = self
            .rows
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let tid = self.alloc_tid();
        self.record_in_indexes(&tuple, tid);
        Arc::make_mut(&mut *rows).push(Version {
            tid,
            header: TupleHeader::inserted(txn.xid, txn.cid),
            tuple,
        });
        tid
    }

    fn update(&self, tid: Tid, tuple: Tuple, txn: &TxnContext) -> UpdateResult {
        let mut rows = self
            .rows
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let rows = Arc::make_mut(&mut *rows);
        let Some(pos) = find(rows, tid) else {
            return UpdateResult::NotFound;
        };
        if !is_live(&rows[pos].header, &txn.clog) {
            // Someone else already deleted/updated this version. Conflict
            // detection (EvalPlanQual) is P6; today the row is simply gone.
            return UpdateResult::NotFound;
        }
        // Mark the old version deleted by us and append the new one. The old
        // version's tid stays in the physical index under its old key; the probe
        // drops it by MVCC visibility, so only the new key needs recording.
        rows[pos].header.xmax = txn.xid;
        rows[pos].header.cmax = txn.cid;
        let new_tid = self.alloc_tid();
        self.record_in_indexes(&tuple, new_tid);
        rows.push(Version {
            tid: new_tid,
            header: TupleHeader::inserted(txn.xid, txn.cid),
            tuple,
        });
        UpdateResult::Updated
    }

    fn delete(&self, tid: Tid, txn: &TxnContext) -> DeleteResult {
        let mut rows = self
            .rows
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let rows = Arc::make_mut(&mut *rows);
        let Some(pos) = find(rows, tid) else {
            return DeleteResult::NotFound;
        };
        if !is_live(&rows[pos].header, &txn.clog) {
            return DeleteResult::NotFound;
        }
        rows[pos].header.xmax = txn.xid;
        rows[pos].header.cmax = txn.cid;
        DeleteResult::Deleted
    }

    /// One `rows` lock acquisition, one `phys` lock acquisition, and at most one
    /// copy-on-write clone for the whole batch. New versions are appended after
    /// all old ones are stamped, keeping the tid order intact for the searches
    /// inside the loop.
    fn update_many(&self, updates: Vec<(Tid, Tuple)>, txn: &TxnContext) -> u64 {
        let mut rows = self
            .rows
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let rows = Arc::make_mut(&mut *rows);
        // Hold the `phys` lock once for the batch rather than re-locking per row
        // (lock order rows → phys).
        let mut phys = self
            .phys
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let mut new_versions = Vec::new();
        let mut applied = 0;
        for (tid, tuple) in updates {
            if let Some(pos) = find(rows, tid)
                && is_live(&rows[pos].header, &txn.clog)
            {
                rows[pos].header.xmax = txn.xid;
                rows[pos].header.cmax = txn.cid;
                let new_tid = self.alloc_tid();
                for index in phys.iter_mut() {
                    index.record(&tuple, new_tid);
                }
                new_versions.push(Version {
                    tid: new_tid,
                    header: TupleHeader::inserted(txn.xid, txn.cid),
                    tuple,
                });
                applied += 1;
            }
        }
        rows.extend(new_versions);
        applied
    }

    /// Batch counterpart of [`TableAm::delete`]: stamp every found live version
    /// under one lock.
    fn delete_many(&self, tids: Vec<Tid>, txn: &TxnContext) -> u64 {
        let mut rows = self
            .rows
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let rows = Arc::make_mut(&mut *rows);
        let mut applied = 0;
        for tid in tids {
            if let Some(pos) = find(rows, tid)
                && is_live(&rows[pos].header, &txn.clog)
            {
                rows[pos].header.xmax = txn.xid;
                rows[pos].header.cmax = txn.cid;
                applied += 1;
            }
        }
        applied
    }

    /// TRUNCATE: stamp `xmax` on **every** physically-live version, not just the
    /// ones visible to the caller's snapshot. Like `delete_many` this is MVCC —
    /// transactional (a rollback aborts `xmax`, so the rows become live again) —
    /// but it removes rows a concurrent transaction inserted and hasn't committed
    /// too, so a committed TRUNCATE really empties the table (the default
    /// snapshot-scoped `scan + delete_many` would leave those survivors). This
    /// reproduces PostgreSQL's "TRUNCATE removes everything" observable outcome on
    /// this lock-free reference engine; the durable heap engine gets the same
    /// guarantee from its `AccessExclusiveLock`.
    ///
    /// The physical index (`phys`) is intentionally NOT cleared: its probe already
    /// MVCC-filters dead versions, and keeping the entries lets a rolled-back
    /// TRUNCATE restore visibility without rebuilding the index.
    fn truncate(&self, txn: &TxnContext) {
        let mut rows = self
            .rows
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let rows = Arc::make_mut(&mut *rows);
        for v in rows.iter_mut() {
            if is_live(&v.header, &txn.clog) {
                v.header.xmax = txn.xid;
                v.header.cmax = txn.cid;
            }
        }
    }

    /// Reclaim versions that are dead to everyone: deleted by a **committed**
    /// transaction at or before `oldest`. A single retain pass under the lock. A
    /// version whose deleter aborted (or is still in flight) is live and must be
    /// kept — hence the CLOG check, not just `xmax < oldest`.
    fn vacuum(&self, oldest: crabgresql_txn::Xid, clog: &Clog) {
        let mut rows = self
            .rows
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let rows = Arc::make_mut(&mut *rows);
        rows.retain(|v| {
            !(v.header.xmax.is_valid()
                && v.header.xmax < oldest
                && clog.is_committed(v.header.xmax))
        });
        // Rebuild the physical indexes over the survivors so the tids of
        // reclaimed versions are pruned. Unlike `rows`, the index map is
        // append-only during normal operation, so vacuum is the point where its
        // dead entries are collected (lock order rows → phys).
        for index in self
            .phys
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .iter_mut()
        {
            if let Some(map) = index.map.as_mut() {
                map.clear();
                for v in rows.iter() {
                    if let Some(key) = PhysicalIndex::encode_row(&index.key_columns, &v.tuple) {
                        map.entry(key).or_default().push(v.tid);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_storage_api::{Column, IndexConstraint, IndexKey, IndexMethod};
    use crabgresql_txn::{CommandId, TransactionManager, Xid};
    use crabgresql_types::{PgType, Value};

    fn schema(name: &str) -> TableSchema {
        TableSchema {
            name: name.to_string(),
            columns: vec![
                Column::new("id", PgType::Int4),
                Column::new("name", PgType::Text),
            ],
        }
    }

    /// A unique index on one column, PostgreSQL's default `nulls_distinct`.
    fn unique_index(name: &str, column: usize) -> IndexMetadata {
        IndexMetadata {
            name: name.into(),
            method: IndexMethod::BTree,
            keys: vec![IndexKey {
                column,
                descending: false,
                nulls_first: false,
            }],
            unique: true,
            nulls_distinct: true,
            constraint: Some(IndexConstraint::Unique),
        }
    }

    /// The `id` (column 0) of every row `index_lookup` yields for `key`, sorted;
    /// `None` when the engine cannot serve the probe (falls back to a scan).
    fn probe_ids(
        engine: &MemoryEngine,
        tm: &TransactionManager,
        index: &str,
        key: Value,
    ) -> Option<Vec<Value>> {
        let table = match engine.open_table("t") {
            Ok(table) => table,
            Err(error) => panic!("failed to open memory-storage test table: {error}"),
        };
        table.index_lookup(index, &[key], &read(tm)).map(|iter| {
            let mut ids: Vec<Value> = iter.map(|(_, t)| t[0].clone()).collect();
            ids.sort_by_key(|v| match v {
                Value::Int4(n) => *n,
                _ => 0,
            });
            ids
        })
    }

    /// Insert one row in its own committed autocommit transaction; returns its
    /// tid. Committing makes it visible to later reads.
    fn insert_committed(tm: &TransactionManager, table: &dyn TableAm, tuple: Tuple) -> Tid {
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        let tid = table.insert(tuple, &txn);
        if let Err(error) = tm.commit(xid) {
            panic!("failed to commit memory-storage test transaction: {error}");
        }
        tid
    }

    /// A read context whose fresh snapshot sees every committed write so far.
    fn read(tm: &TransactionManager) -> TxnContext {
        tm.context(Xid::INVALID, CommandId::FIRST)
    }

    /// Column 0 (id) of every visible row.
    fn ids(tm: &TransactionManager, table: &dyn TableAm) -> Vec<Value> {
        table.scan(&read(tm)).map(|(_, t)| t[0].clone()).collect()
    }

    #[test]
    fn insert_then_scan() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        insert_committed(
            &tm,
            &*table,
            vec![Value::Int4(1), Value::Text("one".into())],
        );
        insert_committed(&tm, &*table, vec![Value::Int4(2), Value::Null]);

        let rows: Vec<_> = table.scan(&read(&tm)).collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, vec![Value::Int4(1), Value::Text("one".into())]);
        assert_eq!(rows[1].1, vec![Value::Int4(2), Value::Null]);

        Ok(())
    }

    #[test]
    fn insert_returns_monotonic_tids() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        let a = insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        let b = insert_committed(&tm, &*table, vec![Value::Int4(2), Value::Null]);
        assert!(b > a);
        let tids: Vec<Tid> = table.scan(&read(&tm)).map(|(tid, _)| tid).collect();
        assert_eq!(tids, vec![a, b]);

        Ok(())
    }

    #[test]
    fn uncommitted_insert_is_invisible_until_commit() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        table.insert(vec![Value::Int4(1), Value::Null], &txn);
        // A concurrent reader cannot see the in-flight insert...
        assert_eq!(table.scan(&read(&tm)).count(), 0);
        // ...but the inserting transaction's own later command can.
        let self_read = tm.context(xid, CommandId(1));
        assert_eq!(table.scan(&self_read).count(), 1);
        tm.commit(xid)?;
        assert_eq!(table.scan(&read(&tm)).count(), 1);

        Ok(())
    }

    #[test]
    fn aborted_insert_is_never_visible() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        table.insert(vec![Value::Int4(1), Value::Null], &txn);
        tm.abort(xid);
        assert_eq!(table.scan(&read(&tm)).count(), 0);

        Ok(())
    }

    #[test]
    fn update_makes_new_version_visible_old_dead() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        let tid = insert_committed(
            &tm,
            &*table,
            vec![Value::Int4(1), Value::Text("one".into())],
        );
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        assert_eq!(
            table.update(tid, vec![Value::Int4(1), Value::Text("uno".into())], &txn),
            UpdateResult::Updated
        );
        tm.commit(xid)?;
        let rows: Vec<_> = table.scan(&read(&tm)).map(|(_, t)| t).collect();
        assert_eq!(rows, vec![vec![Value::Int4(1), Value::Text("uno".into())]]);

        Ok(())
    }

    #[test]
    fn rolled_back_update_restores_old_version() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        let tid = insert_committed(
            &tm,
            &*table,
            vec![Value::Int4(1), Value::Text("one".into())],
        );
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        table.update(tid, vec![Value::Int4(1), Value::Text("uno".into())], &txn);
        tm.abort(xid);
        // The old version's delete is void and the new version never committed.
        let rows: Vec<_> = table.scan(&read(&tm)).map(|(_, t)| t).collect();
        assert_eq!(rows, vec![vec![Value::Int4(1), Value::Text("one".into())]]);

        Ok(())
    }

    #[test]
    fn delete_leaves_other_tids_untouched() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        let b = insert_committed(&tm, &*table, vec![Value::Int4(2), Value::Null]);
        insert_committed(&tm, &*table, vec![Value::Int4(3), Value::Null]);
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        assert_eq!(table.delete(b, &txn), DeleteResult::Deleted);
        tm.commit(xid)?;
        assert_eq!(ids(&tm, &*table), vec![Value::Int4(1), Value::Int4(3)]);

        Ok(())
    }

    #[test]
    fn rolled_back_delete_keeps_the_row() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        let a = insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        table.delete(a, &txn);
        tm.abort(xid);
        assert_eq!(ids(&tm, &*table), vec![Value::Int4(1)]);

        Ok(())
    }

    #[test]
    fn vacuum_keeps_live_row_whose_deleter_aborted() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        let a = insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        // Delete the row, then abort the deleter: the row is live again.
        let x = tm.allocate_xid();
        table.delete(a, &tm.context(x, CommandId::FIRST));
        tm.abort(x);
        // Vacuum with a horizon well past the aborted deleter must NOT reclaim
        // the still-live row (the deleter never committed).
        let horizon = tm.allocate_xid();
        table.vacuum(horizon, tm.clog());
        assert_eq!(ids(&tm, &*table), vec![Value::Int4(1)]);

        Ok(())
    }

    #[test]
    fn update_and_delete_of_missing_tid_report_not_found() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        let tid = insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        assert_eq!(table.delete(tid, &txn), DeleteResult::Deleted);
        // Already deleted by this (committed-once-we-commit) txn: gone.
        tm.commit(xid)?;
        let x2 = tm.allocate_xid();
        let t2 = tm.context(x2, CommandId::FIRST);
        assert_eq!(table.delete(tid, &t2), DeleteResult::NotFound);
        assert_eq!(
            table.update(tid, vec![Value::Int4(2), Value::Null], &t2),
            UpdateResult::NotFound
        );
        // A never-allocated tid is also NotFound.
        assert_eq!(
            table.delete(Tid::from_packed(999), &t2),
            DeleteResult::NotFound
        );

        Ok(())
    }

    #[test]
    fn duplicate_create_fails() -> anyhow::Result<()> {
        let engine = MemoryEngine::new();
        engine.create_table(schema("t"))?;
        assert!(matches!(
            engine.create_table(schema("t")),
            Err(StorageError::TableAlreadyExists(_))
        ));

        Ok(())
    }

    #[test]
    fn open_missing_table_fails() {
        let engine = MemoryEngine::new();
        assert!(matches!(
            engine.open_table("nope"),
            Err(StorageError::TableNotFound(_))
        ));
    }

    #[test]
    fn drop_table_removes_it_and_allows_recreate() -> anyhow::Result<()> {
        let engine = MemoryEngine::new();
        engine.create_table(schema("t"))?;
        engine.drop_table("t")?;
        // Gone after drop.
        assert!(matches!(
            engine.open_table("t"),
            Err(StorageError::TableNotFound(_))
        ));
        // The name is free to reuse.
        engine.create_table(schema("t"))?;
        assert!(engine.open_table("t").is_ok());

        Ok(())
    }

    #[test]
    fn drop_missing_table_fails() {
        let engine = MemoryEngine::new();
        assert!(matches!(
            engine.drop_table("nope"),
            Err(StorageError::TableNotFound(_))
        ));
    }

    #[test]
    fn update_many_applies_batch_and_skips_missing() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        let a = insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        let b = insert_committed(&tm, &*table, vec![Value::Int4(2), Value::Null]);
        // Delete b in its own committed txn first.
        let dx = tm.allocate_xid();
        table.delete(b, &tm.context(dx, CommandId::FIRST));
        tm.commit(dx)?;
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        let applied = table.update_many(
            vec![
                (a, vec![Value::Int4(10), Value::Null]),
                (b, vec![Value::Int4(20), Value::Null]),
            ],
            &txn,
        );
        tm.commit(xid)?;
        assert_eq!(applied, 1);
        assert_eq!(ids(&tm, &*table), vec![Value::Int4(10)]);

        Ok(())
    }

    #[test]
    fn delete_many_removes_batch_in_one_pass() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        let a = insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        let b = insert_committed(&tm, &*table, vec![Value::Int4(2), Value::Null]);
        let c = insert_committed(&tm, &*table, vec![Value::Int4(3), Value::Null]);
        let dx = tm.allocate_xid();
        table.delete(b, &tm.context(dx, CommandId::FIRST));
        tm.commit(dx)?;
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        assert_eq!(table.delete_many(vec![a, b, c], &txn), 2);
        tm.commit(xid)?;
        assert_eq!(table.scan(&read(&tm)).count(), 0);

        Ok(())
    }

    #[test]
    fn truncate_empties_table_and_keeps_tids_monotonic() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        let b = insert_committed(&tm, &*table, vec![Value::Int4(2), Value::Null]);
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST));
        tm.commit(tx)?;
        assert_eq!(table.scan(&read(&tm)).count(), 0);
        let c = insert_committed(&tm, &*table, vec![Value::Int4(3), Value::Null]);
        assert!(c > b);
        assert_eq!(ids(&tm, &*table), vec![Value::Int4(3)]);

        Ok(())
    }

    #[test]
    fn truncate_removes_a_concurrent_uncommitted_insert() -> anyhow::Result<()> {
        // A committed TRUNCATE must empty the table entirely, even of a row a
        // concurrent transaction inserted but hasn't committed — the row is not
        // visible to the truncater's snapshot, but TRUNCATE removes everything.
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        // T1 inserts a row but does not commit.
        let a = tm.allocate_xid();
        table.insert(vec![Value::Int4(2), Value::Null], &tm.context(a, CommandId::FIRST));
        // T2 truncates and commits.
        let b = tm.allocate_xid();
        table.truncate(&tm.context(b, CommandId::FIRST));
        tm.commit(b)?;
        // T1 commits its insert afterwards — it was still removed by the truncate.
        tm.commit(a)?;
        assert!(ids(&tm, &*table).is_empty());

        Ok(())
    }

    #[test]
    fn rolled_back_truncate_restores_rows() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        insert_committed(&tm, &*table, vec![Value::Int4(2), Value::Null]);
        let x = tm.allocate_xid();
        table.truncate(&tm.context(x, CommandId::FIRST));
        tm.abort(x);
        // Aborting the TRUNCATE leaves every stamped version live again.
        assert_eq!(ids(&tm, &*table).len(), 2);

        Ok(())
    }

    #[test]
    fn concurrent_inserts_keep_rows_sorted_by_tid() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        std::thread::scope(|s| -> anyhow::Result<()> {
            let mut handles = Vec::new();
            for _ in 0..4 {
                handles.push(s.spawn(|| -> anyhow::Result<()> {
                    for i in 0..250 {
                        let xid = tm.allocate_xid();
                        let txn = tm.context(xid, CommandId::FIRST);
                        table.insert(vec![Value::Int4(i), Value::Null], &txn);
                        tm.commit(xid)?;
                    }
                    Ok(())
                }));
            }
            for handle in handles {
                handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("insert worker panicked"))??;
            }
            Ok(())
        })?;
        let tids: Vec<Tid> = table.scan(&read(&tm)).map(|(tid, _)| tid).collect();
        assert_eq!(tids.len(), 1000);
        assert!(
            tids.windows(2).all(|w| w[0] < w[1]),
            "rows must stay tid-sorted"
        );

        Ok(())
    }

    #[test]
    fn index_lookup_finds_rows_indexed_before_and_after_create() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        // A row inserted before CREATE INDEX must be indexed by the build scan.
        insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Text("a".into())]);
        engine.create_index("t", unique_index("t_id_key", 0))?;
        // ...and a row inserted after must be indexed by insert maintenance.
        insert_committed(&tm, &*table, vec![Value::Int4(2), Value::Text("b".into())]);

        assert_eq!(
            probe_ids(&engine, &tm, "t_id_key", Value::Int4(1)),
            Some(vec![Value::Int4(1)])
        );
        assert_eq!(
            probe_ids(&engine, &tm, "t_id_key", Value::Int4(2)),
            Some(vec![Value::Int4(2)])
        );
        // A key with no row is served (Some) but empty.
        assert_eq!(
            probe_ids(&engine, &tm, "t_id_key", Value::Int4(99)),
            Some(vec![])
        );
        Ok(())
    }

    #[test]
    fn index_lookup_follows_update_and_delete_visibility() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        engine.create_index("t", unique_index("t_id_key", 0))?;
        let tid = insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Text("a".into())]);

        // Move the key from 1 to 5.
        let xid = tm.allocate_xid();
        table.update(
            tid,
            vec![Value::Int4(5), Value::Text("a".into())],
            &tm.context(xid, CommandId::FIRST),
        );
        tm.commit(xid)?;

        // The old key's row is now invisible; the new key finds the live version.
        assert_eq!(
            probe_ids(&engine, &tm, "t_id_key", Value::Int4(1)),
            Some(vec![])
        );
        assert_eq!(
            probe_ids(&engine, &tm, "t_id_key", Value::Int4(5)),
            Some(vec![Value::Int4(5)])
        );

        // Delete it: both keys are now empty.
        let dx = tm.allocate_xid();
        let new_tid = table
            .scan(&read(&tm))
            .find(|(_, t)| t[0] == Value::Int4(5))
            .ok_or_else(|| anyhow::anyhow!("updated row (id 5) not found"))?
            .0;
        table.delete(new_tid, &tm.context(dx, CommandId::FIRST));
        tm.commit(dx)?;
        assert_eq!(
            probe_ids(&engine, &tm, "t_id_key", Value::Int4(5)),
            Some(vec![])
        );
        Ok(())
    }

    #[test]
    fn index_lookup_none_for_unknown_index_or_ineligible_key_type() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        engine.create_index("t", unique_index("t_id_key", 0))?;
        // Unknown index name → unservable, caller falls back to a scan.
        assert!(probe_ids(&engine, &tm, "no_such_index", Value::Int4(1)).is_none());
        assert!(!table.supports_index_scan("no_such_index"));
        // An int4 index is servable.
        assert!(table.supports_index_scan("t_id_key"));

        // A float8 key type is not equality-canonical, so its index stays
        // metadata-only: index_lookup returns None and supports_index_scan is
        // false, so the planner won't route an index scan to it.
        let ft = engine.create_table(TableSchema {
            name: "f".into(),
            columns: vec![Column::new("x", PgType::Float8)],
        })?;
        engine.create_index("f", unique_index("f_x_key", 0))?;
        assert!(!ft.supports_index_scan("f_x_key"));
        assert!(
            ft.index_lookup("f_x_key", &[Value::Float8(1.0)], &read(&tm))
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn vacuum_prunes_dead_tids_from_the_index() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        engine.create_index("t", unique_index("t_id_key", 0))?;
        let mut tid = insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Text("v0".into())]);
        // Update the same key several times; each update appends a new tid under
        // the key and leaves the old (now dead) version behind.
        for i in 1..=5 {
            let xid = tm.allocate_xid();
            table.update(
                tid,
                vec![Value::Int4(1), Value::Text(format!("v{i}"))],
                &tm.context(xid, CommandId::FIRST),
            );
            tm.commit(xid)?;
            tid = table
                .scan(&read(&tm))
                .find(|(_, t)| t[0] == Value::Int4(1))
                .ok_or_else(|| anyhow::anyhow!("live row vanished"))?
                .0;
        }
        // Vacuum past all the dead versions, then confirm the probe still returns
        // exactly the one live row (the pruned tids must not resurrect or drop it).
        let horizon = tm.allocate_xid();
        table.vacuum(horizon, tm.clog());
        assert_eq!(
            probe_ids(&engine, &tm, "t_id_key", Value::Int4(1)),
            Some(vec![Value::Int4(1)])
        );
        Ok(())
    }

    #[test]
    fn scan_is_stable_against_concurrent_writes() -> anyhow::Result<()> {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t"))?;
        let a = insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        // Capture the scan (and its snapshot) before any further writes.
        let scan = table.scan(&read(&tm));
        // Writes committed after the snapshot are invisible to it.
        insert_committed(&tm, &*table, vec![Value::Int4(2), Value::Null]);
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        table.update(a, vec![Value::Int4(99), Value::Null], &txn);
        table.delete(a, &txn);
        tm.commit(xid)?;
        let rows: Vec<_> = scan.collect();
        assert_eq!(rows, vec![(a, vec![Value::Int4(1), Value::Null])]);

        Ok(())
    }
}
