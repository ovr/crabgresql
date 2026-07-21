//! Per-table access-method routing.
//!
//! The server boots one primary engine (the in-memory engine, or the durable
//! heap engine under `--data-dir`) and one Parquet engine. `RoutingEngine`
//! composes them behind a single `TableEngine` so the rest of the server — the
//! session catalog, the executor — keeps seeing one global engine. A table is
//! routed to the Parquet engine when its schema requests
//! `access_method = Some("parquet")` (from `CREATE TABLE ... USING parquet`);
//! every other relation, and all schemas/views/sequences/indexes, live in the
//! primary engine.

use std::sync::Arc;

use crabgresql_storage_api::{
    IndexMetadata, RelationMetadata, SequenceAdvance, SequenceDefinition, StorageError, TableAm,
    TableEngine, TableSchema, ViewDefinition,
};

/// Routes relations across the primary engine and the Parquet engine by access
/// method, presenting them as a single [`TableEngine`].
pub struct RoutingEngine {
    /// The heap/memory engine backing every non-Parquet relation, and the owner
    /// of all schemas, views, sequences and indexes.
    default: Arc<dyn TableEngine>,
    /// The Parquet-file engine backing `USING parquet` tables.
    parquet: Arc<dyn TableEngine>,
}

impl RoutingEngine {
    pub fn new(default: Arc<dyn TableEngine>, parquet: Arc<dyn TableEngine>) -> Self {
        RoutingEngine { default, parquet }
    }

    /// Try `first`; on `TableNotFound` fall through to the other engine, but
    /// surface any other error unchanged.
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

impl TableEngine for RoutingEngine {
    fn create_table(&self, schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError> {
        let is_parquet = schema.access_method.as_deref() == Some("parquet");
        let (target, other) = if is_parquet {
            (&self.parquet, &self.default)
        } else {
            (&self.default, &self.parquet)
        };
        // Relations share one namespace regardless of access method: a Parquet
        // `t` must collide with a heap `t` (and vice versa), otherwise the second
        // table would be permanently shadowed at resolve time.
        if other.resolve(Some(&schema.namespace), &schema.name).is_ok() {
            return Err(StorageError::TableAlreadyExists(schema.name));
        }
        target.create_table(schema)
    }

    fn open_table(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        Self::or_else_not_found(self.default.open_table(name), || self.parquet.open_table(name))
    }

    fn resolve(&self, schema: Option<&str>, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        Self::or_else_not_found(self.default.resolve(schema, name), || {
            self.parquet.resolve(schema, name)
        })
    }

    fn drop_table(&self, namespace: &str, name: &str) -> Result<(), StorageError> {
        match self.default.drop_table(namespace, name) {
            Err(StorageError::TableNotFound(_)) => self.parquet.drop_table(namespace, name),
            other => other,
        }
    }

    // Schemas, views, sequences and indexes are owned exclusively by the primary
    // engine — Parquet tables cannot carry them (rejected at DDL time).
    fn create_schema(&self, name: &str) -> Result<u32, StorageError> {
        self.default.create_schema(name)
    }

    fn drop_schema(&self, name: &str) -> Result<(), StorageError> {
        self.default.drop_schema(name)
    }

    fn schemas(&self) -> Vec<(String, u32)> {
        self.default.schemas()
    }

    fn schema_exists(&self, name: &str) -> bool {
        self.default.schema_exists(name)
    }

    fn create_index(
        &self,
        namespace: &str,
        table: &str,
        index: IndexMetadata,
    ) -> Result<(), StorageError> {
        self.default.create_index(namespace, table, index)
    }

    fn drop_index(
        &self,
        namespace: &str,
        table: &str,
        index_name: &str,
    ) -> Result<(), StorageError> {
        self.default.drop_index(namespace, table, index_name)
    }

    fn index_name_exists(&self, namespace: &str, table: &str, index_name: &str) -> bool {
        self.default.index_name_exists(namespace, table, index_name)
    }

    fn relations(&self) -> Vec<TableSchema> {
        let mut all = self.default.relations();
        all.extend(self.parquet.relations());
        all
    }

    fn relation_metadata(&self) -> Vec<RelationMetadata> {
        let mut all = self.default.relation_metadata();
        all.extend(self.parquet.relation_metadata());
        all
    }

    fn create_view(&self, def: ViewDefinition) -> Result<(), StorageError> {
        self.default.create_view(def)
    }

    fn resolve_view(&self, schema: Option<&str>, name: &str) -> Option<ViewDefinition> {
        self.default.resolve_view(schema, name)
    }

    fn drop_view(&self, namespace: &str, name: &str) -> Result<(), StorageError> {
        self.default.drop_view(namespace, name)
    }

    fn views(&self) -> Vec<ViewDefinition> {
        self.default.views()
    }

    fn create_sequence(&self, def: SequenceDefinition) -> Result<(), StorageError> {
        self.default.create_sequence(def)
    }

    fn drop_sequence(&self, namespace: &str, name: &str) -> Result<(), StorageError> {
        self.default.drop_sequence(namespace, name)
    }

    fn sequence(&self, namespace: &str, name: &str) -> Option<SequenceDefinition> {
        self.default.sequence(namespace, name)
    }

    fn sequences(&self) -> Vec<SequenceDefinition> {
        self.default.sequences()
    }

    fn sequence_nextval(&self, namespace: &str, name: &str) -> SequenceAdvance {
        self.default.sequence_nextval(namespace, name)
    }

    fn sequence_setval(
        &self,
        namespace: &str,
        name: &str,
        value: i64,
        is_called: bool,
    ) -> SequenceAdvance {
        self.default.sequence_setval(namespace, name, value, is_called)
    }
}
