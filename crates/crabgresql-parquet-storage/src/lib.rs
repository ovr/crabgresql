//! Parquet-backed table access method.
//!
//! Each table is persisted as a single Apache Parquet file under a base
//! directory; the file is self-describing (its `TableSchema` lives in the
//! Parquet key-value metadata, see [`convert`]), so the catalog is rebuilt on
//! restart by scanning the directory. Selected via `CREATE TABLE ... USING
//! parquet` and routed here by the server's routing engine.
//!
//! Scope: **append + read**. `SELECT` and `INSERT` are supported; the engine is
//! append-only, so the server rejects `UPDATE`/`DELETE` on a Parquet table
//! before they reach the storage layer (`update`/`delete` here are defensive
//! no-ops). Rows are held in memory as the authoritative copy for the process
//! and mirrored to the Parquet file after every insert.

mod convert;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crabgresql_storage_api::{
    DeleteResult, RelationMetadata, StorageError, TableAm, TableEngine, TableSchema, Tid, Tuple,
    UpdateResult,
};
use crabgresql_txn::TxnContext;

use convert::{first_unsupported_column, read_parquet, write_parquet};

/// A relation's key: `(namespace, name)`, matching the memory/heap engines.
type RelKey = (String, String);

/// Storage engine that persists each table as a Parquet file.
pub struct ParquetEngine {
    base_dir: PathBuf,
    tables: RwLock<HashMap<RelKey, Arc<ParquetTable>>>,
}

impl ParquetEngine {
    /// Create an engine over `base_dir`, recovering any tables already present
    /// (each `*.parquet` file's embedded schema is authoritative). Creates the
    /// directory if it does not exist.
    pub fn open(base_dir: impl Into<PathBuf>) -> Result<Self, StorageError> {
        let base_dir = base_dir.into();
        std::fs::create_dir_all(&base_dir)
            .map_err(|e| StorageError::Io(format!("parquet: creating base dir: {e}")))?;
        let mut tables = HashMap::new();
        let entries = std::fs::read_dir(&base_dir)
            .map_err(|e| StorageError::Io(format!("parquet: reading base dir: {e}")))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| StorageError::Io(format!("parquet: reading dir entry: {e}")))?;
            let path = entry.path();
            // Recover finished table files only; skip in-progress `.parquet.tmp`
            // writes and anything that is not a Parquet file.
            if path.extension().and_then(|e| e.to_str()) != Some("parquet") {
                continue;
            }
            match read_parquet(&path) {
                Ok((schema, rows)) => {
                    let key = (schema.namespace.clone(), schema.name.clone());
                    tables.insert(
                        key,
                        Arc::new(ParquetTable {
                            schema,
                            path,
                            rows: RwLock::new(rows),
                        }),
                    );
                }
                Err(e) => {
                    tracing::error!(error = %e, path = %path.display(), "parquet: skipping unreadable table file");
                }
            }
        }
        Ok(ParquetEngine {
            base_dir,
            tables: RwLock::new(tables),
        })
    }

    /// Path of the Parquet file backing `(namespace, name)`. The embedded
    /// metadata is authoritative on read, so the filename is only for humans.
    fn file_path(&self, namespace: &str, name: &str) -> PathBuf {
        self.base_dir.join(format!("{namespace}__{name}.parquet"))
    }
}

impl TableEngine for ParquetEngine {
    fn create_table(&self, schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError> {
        if let Some(col) = first_unsupported_column(&schema) {
            return Err(StorageError::Unsupported(format!(
                "column \"{}\" has a type not supported by the parquet access method",
                col.name
            )));
        }
        let key = (schema.namespace.clone(), schema.name.clone());
        let mut tables = self
            .tables
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        if tables.contains_key(&key) {
            return Err(StorageError::TableAlreadyExists(schema.name));
        }
        let path = self.file_path(&schema.namespace, &schema.name);
        let mut schema = schema;
        schema.access_method = Some("parquet".to_string());
        // Write an empty, self-describing file so the table survives a restart
        // even before its first row.
        write_parquet(&path, &schema, &[])?;
        let table = Arc::new(ParquetTable {
            schema,
            path,
            rows: RwLock::new(Vec::new()),
        });
        tables.insert(key, table.clone());
        Ok(table)
    }

    fn open_table(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        self.resolve(Some("public"), name)
    }

    fn resolve(&self, schema: Option<&str>, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        let namespace = schema.unwrap_or("public");
        self.tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .get(&(namespace.to_string(), name.to_string()))
            .cloned()
            .map(|t| t as Arc<dyn TableAm>)
            .ok_or_else(|| StorageError::TableNotFound(name.to_string()))
    }

    fn drop_table(&self, namespace: &str, name: &str) -> Result<(), StorageError> {
        let key = (namespace.to_string(), name.to_string());
        let table = self
            .tables
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .remove(&key)
            .ok_or_else(|| StorageError::TableNotFound(name.to_string()))?;
        // Best-effort file removal: the catalog entry is already gone, so a
        // leftover file only wastes disk and is ignored on the next recovery
        // (its name no longer resolves).
        if let Err(e) = std::fs::remove_file(&table.path) {
            tracing::warn!(error = %e, path = %table.path.display(), "parquet: failed to remove dropped table file");
        }
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
                // Parquet tables carry no indexes.
                indexes: Vec::new(),
            })
            .collect()
    }
}

/// One Parquet-backed table: rows live in memory (authoritative for the process)
/// and are mirrored to `path` on every mutation.
pub struct ParquetTable {
    schema: TableSchema,
    path: PathBuf,
    rows: RwLock<Vec<Tuple>>,
}

impl ParquetTable {
    /// Rewrite the backing file from the current rows, logging any I/O error
    /// (the trait's mutation methods are infallible, and the in-memory rows
    /// remain correct for this process regardless).
    fn flush(&self, rows: &[Tuple]) {
        if let Err(e) = write_parquet(&self.path, &self.schema, rows) {
            tracing::error!(error = %e, table = %self.schema.name, "parquet: failed to persist table file");
        }
    }
}

impl TableAm for ParquetTable {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    fn access_method(&self) -> &str {
        "parquet"
    }

    fn scan(&self, _txn: &TxnContext) -> Box<dyn Iterator<Item = (Tid, Tuple)> + Send> {
        // Append-only: every stored row is visible. Snapshot into an owned vec so
        // the iterator does not hold the lock.
        let rows = self.rows.read().unwrap_or_else(|_| panic!("rwlock poisoned"));
        let out: Vec<(Tid, Tuple)> = rows
            .iter()
            .enumerate()
            .map(|(i, t)| (Tid::from_packed(i as u64), t.clone()))
            .collect();
        Box::new(out.into_iter())
    }

    fn fetch(&self, tid: Tid, _txn: &TxnContext) -> Option<Tuple> {
        self.rows
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .get(tid.packed() as usize)
            .cloned()
    }

    fn insert(&self, tuple: Tuple, _txn: &TxnContext) -> Tid {
        let mut rows = self
            .rows
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        rows.push(tuple);
        let idx = rows.len() - 1;
        self.flush(&rows);
        Tid::from_packed(idx as u64)
    }

    // The Parquet access method is append-only. The server rejects UPDATE/DELETE
    // on these tables before they reach here, so these are defensive no-ops.
    fn update(&self, _tid: Tid, _tuple: Tuple, _txn: &TxnContext) -> UpdateResult {
        UpdateResult::NotFound
    }

    fn delete(&self, _tid: Tid, _txn: &TxnContext) -> DeleteResult {
        DeleteResult::NotFound
    }

    fn truncate(&self, _txn: &TxnContext) {
        let mut rows = self
            .rows
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        rows.clear();
        self.flush(&rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_storage_api::{Column, TableSchema};
    use crabgresql_txn::{CommandId, TransactionManager};
    use crabgresql_types::{PgType, Value};

    fn sample_schema() -> TableSchema {
        TableSchema {
            name: "p".to_string(),
            namespace: "public".to_string(),
            columns: vec![
                Column::new("id", PgType::Int4),
                Column::new("name", PgType::Text),
                Column::new("ts", PgType::Timestamp),
            ],
            access_method: Some("parquet".to_string()),
        }
    }

    #[test]
    fn insert_scan_and_recover() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tm = TransactionManager::new();
        let txn = tm.context(tm.allocate_xid(), CommandId::FIRST);

        // Create, insert two rows, and scan them back within one process.
        {
            let engine = ParquetEngine::open(dir.path()).expect("open engine");
            let table = engine.create_table(sample_schema()).expect("create");
            table.insert(
                vec![Value::Int4(1), Value::Text("a".into()), Value::Timestamp(0)],
                &txn,
            );
            table.insert(
                vec![Value::Int4(2), Value::Null, Value::Timestamp(1_000_000)],
                &txn,
            );
            let mut rows: Vec<_> = table.scan(&txn).map(|(_, t)| t).collect();
            rows.sort_by_key(|r| match r[0] {
                Value::Int4(v) => v,
                _ => 0,
            });
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][1], Value::Text("a".into()));
            assert_eq!(rows[1][1], Value::Null);
            assert_eq!(rows[1][2], Value::Timestamp(1_000_000));
        }

        // Re-open the directory: the file's embedded schema and rows recover.
        let engine = ParquetEngine::open(dir.path()).expect("reopen engine");
        let table = engine.open_table("p").expect("recovered table");
        assert_eq!(table.access_method(), "parquet");
        assert_eq!(table.schema().columns.len(), 3);
        let rows: Vec<_> = table.scan(&txn).collect();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn rejects_unsupported_column_type() {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = ParquetEngine::open(dir.path()).expect("open engine");
        let schema = TableSchema {
            name: "bad".to_string(),
            namespace: "public".to_string(),
            columns: vec![Column::new("n", PgType::Numeric)],
            access_method: Some("parquet".to_string()),
        };
        assert!(matches!(
            engine.create_table(schema),
            Err(StorageError::Unsupported(_))
        ));
    }
}
