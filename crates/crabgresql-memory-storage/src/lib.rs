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
    DeleteResult, StorageError, TableAm, TableEngine, TableSchema, Tid, Tuple, UpdateResult,
};
use crabgresql_txn::{Clog, TupleHeader, TxnContext, XactStatus, satisfies_mvcc};

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
        let mut tables = self.tables.write().unwrap();
        if tables.contains_key(&schema.name) {
            return Err(StorageError::TableAlreadyExists(schema.name));
        }
        let table = Arc::new(MemoryTable {
            schema: schema.clone(),
            rows: RwLock::new(Arc::new(Vec::new())),
            next_tid: AtomicU64::new(0),
        });
        tables.insert(schema.name, table.clone());
        Ok(table)
    }

    fn open_table(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        let tables = self.tables.read().unwrap();
        tables
            .get(name)
            .cloned()
            .map(|t| t as Arc<dyn TableAm>)
            .ok_or_else(|| StorageError::TableNotFound(name.to_string()))
    }

    fn drop_table(&self, name: &str) -> Result<(), StorageError> {
        let mut tables = self.tables.write().unwrap();
        tables
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| StorageError::TableNotFound(name.to_string()))
    }

    fn relations(&self) -> Vec<TableSchema> {
        self.tables.read().unwrap().values().map(|t| t.schema.clone()).collect()
    }
}

/// One row version: its identity, MVCC header, and column values.
#[derive(Clone)]
struct Version {
    tid: Tid,
    header: TupleHeader,
    tuple: Tuple,
}

pub struct MemoryTable {
    schema: TableSchema,
    /// Every version ever inserted (including dead ones, until vacuum), always
    /// sorted ascending by `tid.packed()`: tids are allocated under the write
    /// lock and only appended, so the order never breaks and lookups binary
    /// search on it.
    rows: RwLock<Arc<Vec<Version>>>,
    next_tid: AtomicU64,
}

impl MemoryTable {
    /// Allocate a never-reused tid. Called under the write lock so the append
    /// that follows keeps `rows` tid-sorted.
    fn alloc_tid(&self) -> Tid {
        Tid::from_packed(self.next_tid.fetch_add(1, Ordering::Relaxed))
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

    fn scan(&self, txn: &TxnContext) -> Box<dyn Iterator<Item = (Tid, Tuple)> + Send> {
        let rows = Arc::clone(&self.rows.read().unwrap());
        Box::new(MvccScan {
            rows,
            pos: 0,
            txn: txn.clone(),
        })
    }

    fn fetch(&self, tid: Tid, txn: &TxnContext) -> Option<Tuple> {
        let rows = self.rows.read().unwrap();
        let pos = find(&rows, tid)?;
        let v = &rows[pos];
        satisfies_mvcc(&v.header, &txn.snapshot, &txn.clog, txn.xid, txn.cid)
            .then(|| v.tuple.clone())
    }

    fn insert(&self, tuple: Tuple, txn: &TxnContext) -> Tid {
        // Copy-on-write: cheap append normally, clones the Vec only while a
        // concurrent scan still holds the previous snapshot.
        let mut rows = self.rows.write().unwrap();
        let tid = self.alloc_tid();
        Arc::make_mut(&mut *rows).push(Version {
            tid,
            header: TupleHeader::inserted(txn.xid, txn.cid),
            tuple,
        });
        tid
    }

    fn update(&self, tid: Tid, tuple: Tuple, txn: &TxnContext) -> UpdateResult {
        let mut rows = self.rows.write().unwrap();
        let rows = Arc::make_mut(&mut *rows);
        let Some(pos) = find(rows, tid) else {
            return UpdateResult::NotFound;
        };
        if !is_live(&rows[pos].header, &txn.clog) {
            // Someone else already deleted/updated this version. Conflict
            // detection (EvalPlanQual) is P6; today the row is simply gone.
            return UpdateResult::NotFound;
        }
        // Mark the old version deleted by us and append the new one.
        rows[pos].header.xmax = txn.xid;
        rows[pos].header.cmax = txn.cid;
        let new_tid = self.alloc_tid();
        rows.push(Version {
            tid: new_tid,
            header: TupleHeader::inserted(txn.xid, txn.cid),
            tuple,
        });
        UpdateResult::Updated
    }

    fn delete(&self, tid: Tid, txn: &TxnContext) -> DeleteResult {
        let mut rows = self.rows.write().unwrap();
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

    /// One lock acquisition and at most one copy-on-write clone for the whole
    /// batch. New versions are appended after all old ones are stamped, keeping
    /// the tid order intact for the searches inside the loop.
    fn update_many(&self, updates: Vec<(Tid, Tuple)>, txn: &TxnContext) -> u64 {
        let mut rows = self.rows.write().unwrap();
        let rows = Arc::make_mut(&mut *rows);
        let mut new_versions = Vec::new();
        let mut applied = 0;
        for (tid, tuple) in updates {
            if let Some(pos) = find(rows, tid)
                && is_live(&rows[pos].header, &txn.clog)
            {
                rows[pos].header.xmax = txn.xid;
                rows[pos].header.cmax = txn.cid;
                new_versions.push(Version {
                    tid: self.alloc_tid(),
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
        let mut rows = self.rows.write().unwrap();
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

    /// Drop every version in one step. `next_tid` keeps advancing so tids stay
    /// monotonic and never-reused. (Truncate is not yet transactional — a
    /// rollback will not bring the rows back; that fidelity waits for the heap
    /// engine.)
    fn truncate(&self, _txn: &TxnContext) {
        let mut rows = self.rows.write().unwrap();
        *rows = Arc::new(Vec::new());
    }

    /// Reclaim versions that are dead to everyone: deleted by a **committed**
    /// transaction at or before `oldest`. A single retain pass under the lock. A
    /// version whose deleter aborted (or is still in flight) is live and must be
    /// kept — hence the CLOG check, not just `xmax < oldest`.
    fn vacuum(&self, oldest: crabgresql_txn::Xid, clog: &Clog) {
        let mut rows = self.rows.write().unwrap();
        let rows = Arc::make_mut(&mut *rows);
        rows.retain(|v| {
            !(v.header.xmax.is_valid() && v.header.xmax < oldest && clog.is_committed(v.header.xmax))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_storage_api::Column;
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

    /// Insert one row in its own committed autocommit transaction; returns its
    /// tid. Committing makes it visible to later reads.
    fn insert_committed(tm: &TransactionManager, table: &dyn TableAm, tuple: Tuple) -> Tid {
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        let tid = table.insert(tuple, &txn);
        tm.commit(xid).unwrap();
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
    fn insert_then_scan() {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Text("one".into())]);
        insert_committed(&tm, &*table, vec![Value::Int4(2), Value::Null]);

        let rows: Vec<_> = table.scan(&read(&tm)).collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, vec![Value::Int4(1), Value::Text("one".into())]);
        assert_eq!(rows[1].1, vec![Value::Int4(2), Value::Null]);
    }

    #[test]
    fn insert_returns_monotonic_tids() {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        let a = insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        let b = insert_committed(&tm, &*table, vec![Value::Int4(2), Value::Null]);
        assert!(b > a);
        let tids: Vec<Tid> = table.scan(&read(&tm)).map(|(tid, _)| tid).collect();
        assert_eq!(tids, vec![a, b]);
    }

    #[test]
    fn uncommitted_insert_is_invisible_until_commit() {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        table.insert(vec![Value::Int4(1), Value::Null], &txn);
        // A concurrent reader cannot see the in-flight insert...
        assert_eq!(table.scan(&read(&tm)).count(), 0);
        // ...but the inserting transaction's own later command can.
        let self_read = tm.context(xid, CommandId(1));
        assert_eq!(table.scan(&self_read).count(), 1);
        tm.commit(xid).unwrap();
        assert_eq!(table.scan(&read(&tm)).count(), 1);
    }

    #[test]
    fn aborted_insert_is_never_visible() {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        table.insert(vec![Value::Int4(1), Value::Null], &txn);
        tm.abort(xid);
        assert_eq!(table.scan(&read(&tm)).count(), 0);
    }

    #[test]
    fn update_makes_new_version_visible_old_dead() {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        let tid = insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Text("one".into())]);
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        assert_eq!(
            table.update(tid, vec![Value::Int4(1), Value::Text("uno".into())], &txn),
            UpdateResult::Updated
        );
        tm.commit(xid).unwrap();
        let rows: Vec<_> = table.scan(&read(&tm)).map(|(_, t)| t).collect();
        assert_eq!(rows, vec![vec![Value::Int4(1), Value::Text("uno".into())]]);
    }

    #[test]
    fn rolled_back_update_restores_old_version() {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        let tid = insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Text("one".into())]);
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        table.update(tid, vec![Value::Int4(1), Value::Text("uno".into())], &txn);
        tm.abort(xid);
        // The old version's delete is void and the new version never committed.
        let rows: Vec<_> = table.scan(&read(&tm)).map(|(_, t)| t).collect();
        assert_eq!(rows, vec![vec![Value::Int4(1), Value::Text("one".into())]]);
    }

    #[test]
    fn delete_leaves_other_tids_untouched() {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        let b = insert_committed(&tm, &*table, vec![Value::Int4(2), Value::Null]);
        insert_committed(&tm, &*table, vec![Value::Int4(3), Value::Null]);
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        assert_eq!(table.delete(b, &txn), DeleteResult::Deleted);
        tm.commit(xid).unwrap();
        assert_eq!(ids(&tm, &*table), vec![Value::Int4(1), Value::Int4(3)]);
    }

    #[test]
    fn rolled_back_delete_keeps_the_row() {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        let a = insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        table.delete(a, &txn);
        tm.abort(xid);
        assert_eq!(ids(&tm, &*table), vec![Value::Int4(1)]);
    }

    #[test]
    fn vacuum_keeps_live_row_whose_deleter_aborted() {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
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
    }

    #[test]
    fn update_and_delete_of_missing_tid_report_not_found() {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        let tid = insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        assert_eq!(table.delete(tid, &txn), DeleteResult::Deleted);
        // Already deleted by this (committed-once-we-commit) txn: gone.
        tm.commit(xid).unwrap();
        let x2 = tm.allocate_xid();
        let t2 = tm.context(x2, CommandId::FIRST);
        assert_eq!(table.delete(tid, &t2), DeleteResult::NotFound);
        assert_eq!(
            table.update(tid, vec![Value::Int4(2), Value::Null], &t2),
            UpdateResult::NotFound
        );
        // A never-allocated tid is also NotFound.
        assert_eq!(table.delete(Tid::from_packed(999), &t2), DeleteResult::NotFound);
    }

    #[test]
    fn duplicate_create_fails() {
        let engine = MemoryEngine::new();
        engine.create_table(schema("t")).unwrap();
        assert!(matches!(
            engine.create_table(schema("t")),
            Err(StorageError::TableAlreadyExists(_))
        ));
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
    fn drop_table_removes_it_and_allows_recreate() {
        let engine = MemoryEngine::new();
        engine.create_table(schema("t")).unwrap();
        engine.drop_table("t").unwrap();
        // Gone after drop.
        assert!(matches!(
            engine.open_table("t"),
            Err(StorageError::TableNotFound(_))
        ));
        // The name is free to reuse.
        engine.create_table(schema("t")).unwrap();
        assert!(engine.open_table("t").is_ok());
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
    fn update_many_applies_batch_and_skips_missing() {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        let a = insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        let b = insert_committed(&tm, &*table, vec![Value::Int4(2), Value::Null]);
        // Delete b in its own committed txn first.
        let dx = tm.allocate_xid();
        table.delete(b, &tm.context(dx, CommandId::FIRST));
        tm.commit(dx).unwrap();
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        let applied = table.update_many(
            vec![
                (a, vec![Value::Int4(10), Value::Null]),
                (b, vec![Value::Int4(20), Value::Null]),
            ],
            &txn,
        );
        tm.commit(xid).unwrap();
        assert_eq!(applied, 1);
        assert_eq!(ids(&tm, &*table), vec![Value::Int4(10)]);
    }

    #[test]
    fn delete_many_removes_batch_in_one_pass() {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        let a = insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        let b = insert_committed(&tm, &*table, vec![Value::Int4(2), Value::Null]);
        let c = insert_committed(&tm, &*table, vec![Value::Int4(3), Value::Null]);
        let dx = tm.allocate_xid();
        table.delete(b, &tm.context(dx, CommandId::FIRST));
        tm.commit(dx).unwrap();
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        assert_eq!(table.delete_many(vec![a, b, c], &txn), 2);
        tm.commit(xid).unwrap();
        assert_eq!(table.scan(&read(&tm)).count(), 0);
    }

    #[test]
    fn truncate_empties_table_and_keeps_tids_monotonic() {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        let b = insert_committed(&tm, &*table, vec![Value::Int4(2), Value::Null]);
        let tx = tm.allocate_xid();
        table.truncate(&tm.context(tx, CommandId::FIRST));
        tm.commit(tx).unwrap();
        assert_eq!(table.scan(&read(&tm)).count(), 0);
        let c = insert_committed(&tm, &*table, vec![Value::Int4(3), Value::Null]);
        assert!(c > b);
        assert_eq!(ids(&tm, &*table), vec![Value::Int4(3)]);
    }

    #[test]
    fn concurrent_inserts_keep_rows_sorted_by_tid() {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for i in 0..250 {
                        let xid = tm.allocate_xid();
                        let txn = tm.context(xid, CommandId::FIRST);
                        table.insert(vec![Value::Int4(i), Value::Null], &txn);
                        tm.commit(xid).unwrap();
                    }
                });
            }
        });
        let tids: Vec<Tid> = table.scan(&read(&tm)).map(|(tid, _)| tid).collect();
        assert_eq!(tids.len(), 1000);
        assert!(
            tids.windows(2).all(|w| w[0] < w[1]),
            "rows must stay tid-sorted"
        );
    }

    #[test]
    fn scan_is_stable_against_concurrent_writes() {
        let tm = TransactionManager::new();
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        let a = insert_committed(&tm, &*table, vec![Value::Int4(1), Value::Null]);
        // Capture the scan (and its snapshot) before any further writes.
        let scan = table.scan(&read(&tm));
        // Writes committed after the snapshot are invisible to it.
        insert_committed(&tm, &*table, vec![Value::Int4(2), Value::Null]);
        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        table.update(a, vec![Value::Int4(99), Value::Null], &txn);
        table.delete(a, &txn);
        tm.commit(xid).unwrap();
        let rows: Vec<_> = scan.collect();
        assert_eq!(rows, vec![(a, vec![Value::Int4(1), Value::Null])]);
    }
}
