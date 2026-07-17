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
    /// Rows tagged with their tid. Tids are monotonic and never reused, so a
    /// delete leaves a gap instead of renumbering survivors.
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
        let tid = self.next_tid.fetch_add(1, Ordering::Relaxed);
        Arc::make_mut(&mut *self.rows.write().unwrap()).push((tid, tuple));
        tid
    }

    fn update(&self, tid: Tid, tuple: Tuple) -> UpdateResult {
        let mut rows = self.rows.write().unwrap();
        let rows = Arc::make_mut(&mut *rows);
        match rows.iter_mut().find(|(t, _)| *t == tid) {
            Some((_, slot)) => {
                *slot = tuple;
                UpdateResult::Updated
            }
            None => UpdateResult::NotFound,
        }
    }

    fn delete(&self, tid: Tid) -> DeleteResult {
        let mut rows = self.rows.write().unwrap();
        let rows = Arc::make_mut(&mut *rows);
        match rows.iter().position(|(t, _)| *t == tid) {
            Some(pos) => {
                rows.remove(pos);
                DeleteResult::Deleted
            }
            None => DeleteResult::NotFound,
        }
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
                Column {
                    name: "id".into(),
                    ty: PgType::Int4,
                },
                Column {
                    name: "name".into(),
                    ty: PgType::Text,
                },
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
