//! Per-statement name resolution overlay: a session's temp catalog shadows the
//! shared global engine (PG's `pg_temp`-first search), and the read-only system
//! catalog (`pg_catalog`) sits behind both on the search path.

use std::sync::Arc;

use crabgresql_storage_api::{StorageError, TableAm, TableEngine, TableSchema};

/// Resolves relations against a session-local temp store first, then the shared
/// global engine, then the read-only system catalog — so a `CREATE TEMP TABLE t`
/// hides a permanent `t`, and `pg_catalog` relations (`pg_type`, …) resolve when
/// nothing user-defined shadows them. Holds three cheap `Arc` clones; build one
/// per statement.
pub struct SessionCatalog {
    temp: Arc<dyn TableEngine>,
    global: Arc<dyn TableEngine>,
    system: Arc<dyn TableEngine>,
}

impl SessionCatalog {
    pub fn new(
        temp: Arc<dyn TableEngine>,
        global: Arc<dyn TableEngine>,
        system: Arc<dyn TableEngine>,
    ) -> Self {
        Self { temp, global, system }
    }

    /// Fall through `TableNotFound` to `next`, but surface any other error.
    fn or_else_not_found(
        first: Result<Arc<dyn TableAm>, StorageError>,
        next: impl FnOnce() -> Result<Arc<dyn TableAm>, StorageError>,
    ) -> Result<Arc<dyn TableAm>, StorageError> {
        match first {
            Err(StorageError::TableNotFound(_)) => next(),
            other => other,
        }
    }
}

impl TableEngine for SessionCatalog {
    /// Unqualified, write-safe lookup: temp then global only. Writes resolve
    /// through this, so a mutation never reaches the read-only system catalog.
    fn open_table(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        Self::or_else_not_found(self.temp.open_table(name), || self.global.open_table(name))
    }

    /// Search-path-aware read resolution. An unqualified name searches temp →
    /// global → system (so `pg_catalog` is implicitly on the path, after user
    /// relations). A schema qualifier routes to exactly one namespace.
    fn resolve(
        &self,
        schema: Option<&str>,
        name: &str,
    ) -> Result<Arc<dyn TableAm>, StorageError> {
        match schema {
            None => Self::or_else_not_found(self.open_table(name), || self.system.open_table(name)),
            Some("pg_catalog") => self.system.open_table(name),
            Some("public") => self.global.open_table(name),
            Some("pg_temp") => self.temp.open_table(name),
            Some(_) => Err(StorageError::TableNotFound(name.to_string())),
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

    fn relations(&self) -> Vec<TableSchema> {
        self.global.relations()
    }
}
