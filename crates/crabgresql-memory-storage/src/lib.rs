//! In-memory storage engine: reference implementation of the storage API.
//!
//! Tables live in process memory, write no WAL (effectively UNLOGGED) and are
//! lost on restart. Version chains and snapshot visibility arrive with M2;
//! until then rows sit behind an `Arc` snapshot: a scan grabs the Arc in O(1)
//! and stays stable while writers copy-on-write.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crabgresql_storage_api::{
    DeleteResult, StorageError, TableAm, TableEngine, TableSchema, Tid, Tuple, UpdateResult,
};

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
}

pub struct MemoryTable {
    schema: TableSchema,
    /// Rows tagged with their tid, always sorted ascending by tid: tids are
    /// allocated under the write lock and never reused, inserts append, and a
    /// delete leaves a gap instead of renumbering survivors. Lookups binary
    /// search on this invariant.
    rows: RwLock<Arc<Vec<(Tid, Tuple)>>>,
    next_tid: AtomicU64,
}

/// Iterates a shared snapshot, cloning one tuple per `next()` call instead of
/// copying the whole table up front.
struct SnapshotIter {
    rows: Arc<Vec<(Tid, Tuple)>>,
    pos: usize,
}

impl Iterator for SnapshotIter {
    type Item = (Tid, Tuple);

    fn next(&mut self) -> Option<(Tid, Tuple)> {
        let row = self.rows.get(self.pos)?.clone();
        self.pos += 1;
        Some(row)
    }
}

impl TableAm for MemoryTable {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    fn scan(&self) -> Box<dyn Iterator<Item = (Tid, Tuple)> + Send> {
        let rows = Arc::clone(&self.rows.read().unwrap());
        Box::new(SnapshotIter { rows, pos: 0 })
    }

    fn insert(&self, tuple: Tuple) -> Tid {
        // Copy-on-write: cheap append normally, clones the Vec only while a
        // concurrent scan still holds the previous snapshot.
        let mut rows = self.rows.write().unwrap();
        // Allocate under the write lock: a tid handed out before locking
        // could be appended after a later tid, breaking the sort invariant.
        let tid = self.next_tid.fetch_add(1, Ordering::Relaxed);
        Arc::make_mut(&mut *rows).push((tid, tuple));
        tid
    }

    fn update(&self, tid: Tid, tuple: Tuple) -> UpdateResult {
        let mut rows = self.rows.write().unwrap();
        let rows = Arc::make_mut(&mut *rows);
        match rows.binary_search_by_key(&tid, |(t, _)| *t) {
            Ok(pos) => {
                rows[pos].1 = tuple;
                UpdateResult::Updated
            }
            Err(_) => UpdateResult::NotFound,
        }
    }

    fn delete(&self, tid: Tid) -> DeleteResult {
        let mut rows = self.rows.write().unwrap();
        let rows = Arc::make_mut(&mut *rows);
        match rows.binary_search_by_key(&tid, |(t, _)| *t) {
            Ok(pos) => {
                rows.remove(pos);
                DeleteResult::Deleted
            }
            Err(_) => DeleteResult::NotFound,
        }
    }

    /// One lock acquisition and at most one copy-on-write clone for the whole
    /// batch — per-row calls would pay both per update.
    fn update_many(&self, updates: Vec<(Tid, Tuple)>) -> u64 {
        let mut rows = self.rows.write().unwrap();
        let rows = Arc::make_mut(&mut *rows);
        let mut applied = 0;
        for (tid, tuple) in updates {
            if let Ok(pos) = rows.binary_search_by_key(&tid, |(t, _)| *t) {
                rows[pos].1 = tuple;
                applied += 1;
            }
        }
        applied
    }

    /// Drop the whole snapshot in one step. `next_tid` keeps advancing so tids
    /// stay monotonic and never-reused across the truncate.
    fn truncate(&self) {
        let mut rows = self.rows.write().unwrap();
        *rows = Arc::new(Vec::new());
    }

    /// Single retain pass instead of per-tid removal (each `Vec::remove`
    /// shifts the whole tail).
    fn delete_many(&self, tids: Vec<Tid>) -> u64 {
        let mut tids = tids;
        tids.sort_unstable();
        let mut rows = self.rows.write().unwrap();
        let rows = Arc::make_mut(&mut *rows);
        let before = rows.len();
        rows.retain(|(t, _)| tids.binary_search(t).is_err());
        (before - rows.len()) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_storage_api::Column;
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

    #[test]
    fn insert_then_scan() {
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        table.insert(vec![Value::Int4(1), Value::Text("one".into())]);
        table.insert(vec![Value::Int4(2), Value::Null]);

        let rows: Vec<_> = table.scan().collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, vec![Value::Int4(1), Value::Text("one".into())]);
        assert_eq!(rows[1].1, vec![Value::Int4(2), Value::Null]);
    }

    #[test]
    fn insert_returns_monotonic_tids() {
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        let a = table.insert(vec![Value::Int4(1), Value::Null]);
        let b = table.insert(vec![Value::Int4(2), Value::Null]);
        assert!(b > a);
        let tids: Vec<Tid> = table.scan().map(|(tid, _)| tid).collect();
        assert_eq!(tids, vec![a, b]);
    }

    #[test]
    fn update_replaces_row_in_place() {
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        let tid = table.insert(vec![Value::Int4(1), Value::Text("one".into())]);
        assert_eq!(
            table.update(tid, vec![Value::Int4(1), Value::Text("uno".into())]),
            UpdateResult::Updated
        );
        let rows: Vec<_> = table.scan().collect();
        assert_eq!(
            rows,
            vec![(tid, vec![Value::Int4(1), Value::Text("uno".into())])]
        );
    }

    #[test]
    fn delete_leaves_other_tids_untouched() {
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        let a = table.insert(vec![Value::Int4(1), Value::Null]);
        let b = table.insert(vec![Value::Int4(2), Value::Null]);
        let c = table.insert(vec![Value::Int4(3), Value::Null]);
        assert_eq!(table.delete(b), DeleteResult::Deleted);
        let tids: Vec<Tid> = table.scan().map(|(tid, _)| tid).collect();
        assert_eq!(tids, vec![a, c]);
        // Tids are never reused: the next insert gets a fresh one.
        let d = table.insert(vec![Value::Int4(4), Value::Null]);
        assert!(d > c);
    }

    #[test]
    fn update_and_delete_of_missing_tid_report_not_found() {
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        let tid = table.insert(vec![Value::Int4(1), Value::Null]);
        assert_eq!(table.delete(tid), DeleteResult::Deleted);
        assert_eq!(table.delete(tid), DeleteResult::NotFound);
        assert_eq!(
            table.update(tid, vec![Value::Int4(2), Value::Null]),
            UpdateResult::NotFound
        );
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
    fn update_many_applies_batch_and_skips_missing() {
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        let a = table.insert(vec![Value::Int4(1), Value::Null]);
        let b = table.insert(vec![Value::Int4(2), Value::Null]);
        table.delete(b);
        let applied = table.update_many(vec![
            (a, vec![Value::Int4(10), Value::Null]),
            (b, vec![Value::Int4(20), Value::Null]),
        ]);
        assert_eq!(applied, 1);
        let rows: Vec<_> = table.scan().collect();
        assert_eq!(rows, vec![(a, vec![Value::Int4(10), Value::Null])]);
    }

    #[test]
    fn delete_many_removes_batch_in_one_pass() {
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        let a = table.insert(vec![Value::Int4(1), Value::Null]);
        let b = table.insert(vec![Value::Int4(2), Value::Null]);
        let c = table.insert(vec![Value::Int4(3), Value::Null]);
        table.delete(b);
        assert_eq!(table.delete_many(vec![a, b, c]), 2);
        assert_eq!(table.scan().count(), 0);
    }

    #[test]
    fn truncate_empties_table_and_keeps_tids_monotonic() {
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        table.insert(vec![Value::Int4(1), Value::Null]);
        let b = table.insert(vec![Value::Int4(2), Value::Null]);
        table.truncate();
        assert_eq!(table.scan().count(), 0);
        // Tids are not reused: a post-truncate insert still advances.
        let c = table.insert(vec![Value::Int4(3), Value::Null]);
        assert!(c > b);
        let rows: Vec<_> = table.scan().collect();
        assert_eq!(rows, vec![(c, vec![Value::Int4(3), Value::Null])]);
    }

    #[test]
    fn concurrent_inserts_keep_rows_sorted_by_tid() {
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for i in 0..250 {
                        table.insert(vec![Value::Int4(i), Value::Null]);
                    }
                });
            }
        });
        let tids: Vec<Tid> = table.scan().map(|(tid, _)| tid).collect();
        assert_eq!(tids.len(), 1000);
        assert!(
            tids.windows(2).all(|w| w[0] < w[1]),
            "rows must stay tid-sorted"
        );
    }

    #[test]
    fn scan_is_stable_against_concurrent_writes() {
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        let a = table.insert(vec![Value::Int4(1), Value::Null]);
        let scan = table.scan();
        table.insert(vec![Value::Int4(2), Value::Null]);
        table.update(a, vec![Value::Int4(99), Value::Null]);
        table.delete(a);
        let rows: Vec<_> = scan.collect();
        assert_eq!(rows, vec![(a, vec![Value::Int4(1), Value::Null])]);
    }
}
