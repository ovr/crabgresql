//! Per-statement name resolution overlay: a session's temp catalog shadows the
//! shared global engine (PG's `pg_temp`-first search), and the read-only system
//! catalogs (`pg_catalog` and schema-qualified `information_schema`) sit behind
//! both on the search path.

use std::sync::Arc;

use crabgresql_storage_api::{
    IndexMetadata, RelationMetadata, SequenceAdvance, SequenceDefinition, StorageError, TableAm,
    TableEngine, TableSchema, ViewDefinition,
};

/// Resolves relations against a session-local temp store first, then the shared
/// global engine, then the read-only system catalog — so a `CREATE TEMP TABLE t`
/// hides a permanent `t`, and `pg_catalog` relations (`pg_type`, …) resolve when
/// nothing user-defined shadows them. `information_schema` is available only
/// when schema-qualified. Holds three cheap `Arc` clones; build one per statement.
pub struct SessionCatalog {
    temp: Arc<dyn TableEngine>,
    global: Arc<dyn TableEngine>,
    system: Arc<dyn TableEngine>,
    temp_schema: String,
}

impl SessionCatalog {
    pub fn new(
        temp: Arc<dyn TableEngine>,
        global: Arc<dyn TableEngine>,
        system: Arc<dyn TableEngine>,
        temp_schema: impl Into<String>,
    ) -> Self {
        Self {
            temp,
            global,
            system,
            temp_schema: temp_schema.into(),
        }
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
    /// system → global, mirroring PostgreSQL's implicit order (`pg_temp`, then
    /// `pg_catalog`, then the path): so `pg_catalog` wins over a like-named user
    /// relation in `public`, as in PG. A schema qualifier routes to exactly one
    /// namespace.
    fn resolve(&self, schema: Option<&str>, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        match schema {
            None => Self::or_else_not_found(self.temp.open_table(name), || {
                Self::or_else_not_found(self.system.open_table(name), || {
                    self.global.open_table(name)
                })
            }),
            Some("pg_catalog") | Some("information_schema") => self.system.resolve(schema, name),
            Some("public") => self.global.open_table(name),
            Some("pg_temp") => self.temp.open_table(name),
            Some(namespace) if namespace == self.temp_schema => self.temp.open_table(name),
            // Any other qualifier names a user schema; route it to the global
            // engine, which holds every user namespace.
            Some(namespace) => self.global.resolve(Some(namespace), name),
        }
    }

    fn create_table(&self, schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError> {
        // Non-temp default. The explicit CREATE TABLE path routes temp vs global
        // itself; this is only reachable if a future binder CTAS path creates
        // through the overlay, where permanent is the safe default.
        self.global.create_table(schema)
    }

    fn drop_table(&self, namespace: &str, name: &str) -> Result<(), StorageError> {
        // For an unqualified/`public` drop, mirror `open_table`: drop the session's
        // temp `t` if one shadows a permanent one, else the permanent table. A
        // schema-qualified table lives only in the global engine's namespace.
        if namespace == "public" {
            match self.temp.drop_table("public", name) {
                Err(StorageError::TableNotFound(_)) => self.global.drop_table("public", name),
                other => other,
            }
        } else {
            self.global.drop_table(namespace, name)
        }
    }

    // User schemas live in the shared global engine.
    fn create_schema(&self, name: &str) -> Result<u32, StorageError> {
        self.global.create_schema(name)
    }

    fn drop_schema(&self, name: &str) -> Result<(), StorageError> {
        self.global.drop_schema(name)
    }

    fn schemas(&self) -> Vec<(String, u32)> {
        self.global.schemas()
    }

    fn schema_exists(&self, name: &str) -> bool {
        self.global.schema_exists(name)
    }

    fn create_index(
        &self,
        namespace: &str,
        table: &str,
        index: IndexMetadata,
    ) -> Result<(), StorageError> {
        // A temp table (in the `public`-keyed temp store) shadows a permanent one.
        if namespace == "public" && self.temp.open_table(table).is_ok() {
            self.temp.create_index("public", table, index)
        } else {
            self.global.create_index(namespace, table, index)
        }
    }

    fn index_name_exists(&self, namespace: &str, table: &str, index_name: &str) -> bool {
        if namespace == "public" && self.temp.open_table(table).is_ok() {
            self.temp.index_name_exists("public", table, index_name)
        } else {
            self.global.index_name_exists(namespace, table, index_name)
        }
    }

    fn relations(&self) -> Vec<TableSchema> {
        self.global.relations()
    }

    fn relation_metadata(&self) -> Vec<RelationMetadata> {
        self.global.relation_metadata()
    }

    /// A view is created in the permanent (global) catalog; temp views are not
    /// supported yet, so like the CTAS default this routes to `global`.
    fn create_view(&self, def: ViewDefinition) -> Result<(), StorageError> {
        self.global.create_view(def)
    }

    /// Search-path-aware view resolution, mirroring [`SessionCatalog::resolve`].
    /// Views live only in the permanent catalog for now, so an unqualified or
    /// `public.`-qualified name reaches `global`; other namespaces (temp,
    /// `pg_catalog`) hold no user views.
    fn resolve_view(&self, schema: Option<&str>, name: &str) -> Option<ViewDefinition> {
        match schema {
            None | Some("public") => self.global.resolve_view(None, name),
            // Temp and system namespaces hold no user views; any other qualifier
            // is a user schema, resolved against the global engine.
            Some("pg_temp") | Some("pg_catalog") | Some("information_schema") => None,
            Some(namespace) if namespace == self.temp_schema => None,
            Some(namespace) => self.global.resolve_view(Some(namespace), name),
        }
    }

    fn drop_view(&self, namespace: &str, name: &str) -> Result<(), StorageError> {
        self.global.drop_view(namespace, name)
    }

    fn views(&self) -> Vec<ViewDefinition> {
        self.global.views()
    }

    /// Sequences live only in the permanent catalog (temp sequences unsupported),
    /// so every sequence operation routes to `global`, like views.
    fn create_sequence(&self, def: SequenceDefinition) -> Result<(), StorageError> {
        self.global.create_sequence(def)
    }

    fn drop_sequence(&self, namespace: &str, name: &str) -> Result<(), StorageError> {
        self.global.drop_sequence(namespace, name)
    }

    fn sequence(&self, namespace: &str, name: &str) -> Option<SequenceDefinition> {
        self.global.sequence(namespace, name)
    }

    fn sequences(&self) -> Vec<SequenceDefinition> {
        self.global.sequences()
    }

    fn sequence_nextval(&self, namespace: &str, name: &str) -> SequenceAdvance {
        self.global.sequence_nextval(namespace, name)
    }

    fn sequence_setval(
        &self,
        namespace: &str,
        name: &str,
        value: i64,
        is_called: bool,
    ) -> SequenceAdvance {
        self.global.sequence_setval(namespace, name, value, is_called)
    }
}
