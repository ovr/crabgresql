//! Per-statement name resolution overlay: a session's temp catalog shadows the
//! shared global engine, matching PG's `pg_temp`-first search.

use std::sync::Arc;

use crabgresql_storage_api::{StorageError, TableAm, TableEngine, TableSchema};

/// Resolves relations against a session-local temp store first, then the shared
/// global engine — so a `CREATE TEMP TABLE t` hides a permanent `t` of the same
/// name for the life of the session. Holds two cheap `Arc` clones; build one per
/// statement.
pub struct SessionCatalog {
    temp: Arc<dyn TableEngine>,
    global: Arc<dyn TableEngine>,
}

impl SessionCatalog {
    pub fn new(temp: Arc<dyn TableEngine>, global: Arc<dyn TableEngine>) -> Self {
        Self { temp, global }
    }
}

impl TableEngine for SessionCatalog {
    fn open_table(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        match self.temp.open_table(name) {
            // Not a temp table: fall through to the permanent namespace. A real
            // miss there yields the same `TableNotFound(name)` / 42P01 text.
            Err(StorageError::TableNotFound(_)) => self.global.open_table(name),
            // Temp hit, or a genuine error from the temp store.
            other => other,
        }
    }

    fn create_table(&self, schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError> {
        // Non-temp default. The explicit CREATE TABLE path routes temp vs global
        // itself; this is only reachable if a future binder CTAS path creates
        // through the overlay, where permanent is the safe default.
        self.global.create_table(schema)
    }

    fn drop_table(&self, name: &str) -> Result<(), StorageError> {
        // Temp-first, mirroring `open_table`: a `DROP TABLE t` drops the session's
        // temp `t` if one shadows a permanent one, else the permanent table.
        match self.temp.drop_table(name) {
            Err(StorageError::TableNotFound(_)) => self.global.drop_table(name),
            other => other,
        }
    }
}
