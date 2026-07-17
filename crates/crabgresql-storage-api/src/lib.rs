//! Storage engine API: the `TableEngine` / `TableAm` extension point.
//!
//! M0 scope: create/open/scan/insert, no snapshots. When crabgresql-txn lands
//! (M2), `scan`/`insert` grow snapshot and transaction parameters, and
//! `update`/`delete`/`vacuum` join the trait — see docs/ARCHITECTURE.md §1.3.

use std::sync::Arc;

use crabgresql_types::{PgType, Value};

/// A materialized row. Column order matches the table schema.
pub type Tuple = Vec<Value>;

#[derive(Clone, Debug)]
pub struct Column {
    pub name: String,
    pub ty: PgType,
}

#[derive(Clone, Debug)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<Column>,
}

impl TableSchema {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("relation \"{0}\" already exists")]
    TableAlreadyExists(String),
    #[error("relation \"{0}\" does not exist")]
    TableNotFound(String),
}

/// Table access: scans and modifications on one table.
pub trait TableAm: Send + Sync {
    fn schema(&self) -> &TableSchema;

    /// Full scan. The iterator sees a stable snapshot of the table as of the
    /// call (statement-level consistency until real MVCC snapshots exist).
    fn scan(&self) -> Box<dyn Iterator<Item = Tuple> + Send>;

    /// The tuple must have exactly `schema().columns.len()` values in schema
    /// order — executors index tuples by schema position and rely on this.
    fn insert(&self, tuple: Tuple);
}

/// Engine factory: `CREATE TABLE ... USING <engine>`.
pub trait TableEngine: Send + Sync {
    fn create_table(&self, schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError>;

    fn open_table(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError>;
}
