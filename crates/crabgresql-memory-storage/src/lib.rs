//! In-memory storage engine: reference implementation of the storage API.
//!
//! Tables live in process memory, write no WAL (effectively UNLOGGED) and are
//! lost on restart. Version chains and snapshot visibility arrive with M2;
//! until then rows sit behind an `Arc` snapshot: a scan grabs the Arc in O(1)
//! and stays stable while writers copy-on-write.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crabgresql_storage_api::{StorageError, TableAm, TableEngine, TableSchema, Tuple};

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
    rows: RwLock<Arc<Vec<Tuple>>>,
}

/// Iterates a shared snapshot, cloning one tuple per `next()` call instead of
/// copying the whole table up front.
struct SnapshotIter {
    rows: Arc<Vec<Tuple>>,
    pos: usize,
}

impl Iterator for SnapshotIter {
    type Item = Tuple;

    fn next(&mut self) -> Option<Tuple> {
        let tuple = self.rows.get(self.pos)?.clone();
        self.pos += 1;
        Some(tuple)
    }
}

impl TableAm for MemoryTable {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    fn scan(&self) -> Box<dyn Iterator<Item = Tuple> + Send> {
        let rows = Arc::clone(&self.rows.read().unwrap());
        Box::new(SnapshotIter { rows, pos: 0 })
    }

    fn insert(&self, tuple: Tuple) {
        // Copy-on-write: cheap append normally, clones the Vec only while a
        // concurrent scan still holds the previous snapshot.
        Arc::make_mut(&mut *self.rows.write().unwrap()).push(tuple);
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
        assert_eq!(rows[0], vec![Value::Int4(1), Value::Text("one".into())]);
        assert_eq!(rows[1], vec![Value::Int4(2), Value::Null]);
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
    fn scan_is_stable_against_concurrent_insert() {
        let engine = MemoryEngine::new();
        let table = engine.create_table(schema("t")).unwrap();
        table.insert(vec![Value::Int4(1), Value::Null]);
        let scan = table.scan();
        table.insert(vec![Value::Int4(2), Value::Null]);
        assert_eq!(scan.count(), 1);
    }
}
