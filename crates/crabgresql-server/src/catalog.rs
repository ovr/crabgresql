//! Per-statement name resolution overlay: a session's temp catalog shadows the
//! shared global engine (PG's `pg_temp`-first search), and the read-only system
//! catalogs (`pg_catalog` and schema-qualified `information_schema`) sit behind
//! both on the search path.

use std::sync::Arc;

use crabgresql_catalog::SystemCatalog;
use crabgresql_executor::CatalogOps;
use crabgresql_storage_api::{
    IndexMetadata, RelationMetadata, SequenceAdvance, SequenceDefinition, StorageError, TableAm,
    TableEngine, TableSchema, ViewDefinition,
};

/// The executor-facing catalog handle: answers `pg_get_userbyid` and
/// `pg_table_is_visible` against the same [`SystemCatalog`] snapshot that built
/// this statement's `pg_class` rows, so the OIDs the client reads back are the
/// OIDs these functions resolve. Owns its `Arc` so it can live in a suspended
/// portal's `ExecContext`, like [`crate::session::SessionSequences`].
pub struct SessionCatalogOps {
    system: Arc<SystemCatalog>,
    temp_schema: String,
}

impl SessionCatalogOps {
    pub fn new(system: Arc<SystemCatalog>, temp_schema: impl Into<String>) -> Self {
        Self {
            system,
            temp_schema: temp_schema.into(),
        }
    }
}

impl CatalogOps for SessionCatalogOps {
    fn role_name(&self, oid: u32) -> Option<String> {
        self.system.role_name(oid).map(str::to_string)
    }

    /// A relation is visible when *its own unqualified name reaches it* — PG's
    /// rule, which is about name resolution, not namespace membership: a
    /// relation another one shadows is invisible even though its schema is on
    /// the path.
    ///
    /// So this walks [`SessionCatalog::resolve`]'s search order for the
    /// relation's own name (temp → system catalog → global, which resolves
    /// unqualified names in `public` only) and reports whether the relation
    /// found is this one. `search_path` is still a no-op, so the order is
    /// crabgresql's rather than PostgreSQL's; a relation in a `CREATE SCHEMA`
    /// namespace is correctly invisible because nothing but a qualified name
    /// reaches it.
    ///
    /// Kept in step with `resolve` by `visibility_follows_resolution_order`
    /// below, which fails if the two disagree about a shadowed name.
    fn table_is_visible(&self, oid: u32) -> Option<bool> {
        // A catalog relation is always reachable unqualified: `pg_catalog`
        // precedes the rest of the path, so nothing in a user schema shadows
        // it. (This is why `'pg_class'::regclass` renders bare, not qualified.)
        if crabgresql_catalog::builtin_relation_name(oid).is_some() {
            return Some(true);
        }
        let (namespace, name) = self.system.relation_ref(oid)?;
        // 1. This session's temp namespace shadows everything.
        if self
            .system
            .relation_oid_in(&self.temp_schema, name)
            .is_some()
        {
            return Some(namespace == self.temp_schema);
        }
        // 2. Then pg_catalog. A user relation sharing a catalog relation's name
        //    is unreachable unqualified, so it is not visible; the catalog
        //    relation itself is (nothing shadows it, and `pg_catalog` is always
        //    on the path).
        if self.system.has_catalog_relation(name) {
            return Some(namespace == "pg_catalog");
        }
        // 3. Otherwise the global engine, which resolves unqualified in public.
        Some(namespace == "public")
    }

    fn rel_name(&self, oid: u32) -> Option<(String, String)> {
        // A catalog relation has no `pg_class` row to look up — it answers from
        // the fixed OID assignments instead, so `1259::regclass` renders as
        // `pg_class` the way it does in PostgreSQL.
        if let Some(name) = crabgresql_catalog::builtin_relation_name(oid) {
            return Some(("pg_catalog".to_string(), name.to_string()));
        }
        self.system
            .relation_ref(oid)
            .map(|(ns, name)| (ns.to_string(), name.to_string()))
    }

    /// An unqualified name resolves the way `table_is_visible` reports: this
    /// session's temp schema, then `pg_catalog`, then `public`. Asking the
    /// catalog for each in turn keeps `'t'::regclass` landing on the same
    /// relation a bare `SELECT * FROM t` would.
    ///
    /// The `pg_catalog` step goes through the fixed OID table rather than the
    /// `pg_class` rows: catalog relations are not reflected into `pg_class`, so
    /// they have no positional OID to find there.
    fn rel_oid(&self, namespace: Option<&str>, name: &str) -> Option<u32> {
        let in_catalog = |name: &str| {
            self.system
                .has_catalog_relation(name)
                .then(|| crabgresql_catalog::builtin_relation_oid(name))
                .flatten()
        };
        match namespace {
            Some("pg_catalog") => in_catalog(name),
            Some(ns) => self.system.relation_oid_in(ns, name),
            None => self
                .system
                .relation_oid_in(&self.temp_schema, name)
                .or_else(|| in_catalog(name))
                .or_else(|| self.system.relation_oid_in("public", name)),
        }
    }

    fn namespace_name(&self, oid: u32) -> Option<String> {
        self.system.namespace_name(oid)
    }

    fn namespace_oid(&self, name: &str) -> Option<u32> {
        self.system.namespace_oid(name)
    }

    fn user_type_name(&self, oid: u32) -> Option<(String, String)> {
        self.system
            .user_type_ref(oid)
            .map(|(ns, name)| (ns.to_string(), name.to_string()))
    }

    fn user_type_oid(&self, namespace: Option<&str>, name: &str) -> Option<u32> {
        self.system.user_type_oid(namespace, name)
    }
}

/// Resolves relations against this session's temp namespace first, then the
/// shared global engine, then the read-only system catalog — so a
/// `CREATE TEMP TABLE t` hides a permanent `t`, and `pg_catalog` relations
/// (`pg_type`, …) resolve when nothing user-defined shadows them.
/// `information_schema` is available only when schema-qualified. Temp tables live
/// in the one shared engine under this session's `pg_temp_N` namespace (PG-style),
/// so temp resolution is just a namespace-scoped lookup in `global`. Holds two
/// cheap `Arc` clones; build one per statement.
pub struct SessionCatalog {
    global: Arc<dyn TableEngine>,
    system: Arc<dyn TableEngine>,
    temp_schema: String,
}

impl SessionCatalog {
    pub fn new(
        global: Arc<dyn TableEngine>,
        system: Arc<dyn TableEngine>,
        temp_schema: impl Into<String>,
    ) -> Self {
        Self {
            global,
            system,
            temp_schema: temp_schema.into(),
        }
    }

    /// Resolve `name` in this session's temp namespace, if present there.
    fn open_temp(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        self.global.resolve(Some(&self.temp_schema), name)
    }

    /// Whether `name` names a table in this session's temp namespace.
    fn temp_has(&self, name: &str) -> bool {
        self.open_temp(name).is_ok()
    }

    /// A `pg_temp_N` namespace belonging to ANOTHER session. Temp tables all live
    /// in the one shared engine now, so without this guard a session could reach
    /// another backend's temp relations by qualifying with its namespace (e.g.
    /// `SELECT * FROM pg_temp_3.t`). PostgreSQL forbids cross-session temp access;
    /// we make a foreign temp namespace simply not resolve, as the old per-session
    /// temp engine did (`TableNotFound` → 42P01). The `pg_temp` alias and this
    /// session's own `temp_schema` are handled before this and never reach it.
    fn is_foreign_temp(&self, namespace: &str) -> bool {
        namespace.starts_with("pg_temp_") && namespace != self.temp_schema
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
        Self::or_else_not_found(self.open_temp(name), || self.global.open_table(name))
    }

    /// Search-path-aware read resolution. An unqualified name searches temp →
    /// system → global, mirroring PostgreSQL's implicit order (`pg_temp`, then
    /// `pg_catalog`, then the path): so `pg_catalog` wins over a like-named user
    /// relation in `public`, as in PG. A schema qualifier routes to exactly one
    /// namespace.
    fn resolve(&self, schema: Option<&str>, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        match schema {
            None => Self::or_else_not_found(self.open_temp(name), || {
                Self::or_else_not_found(self.system.open_table(name), || {
                    self.global.open_table(name)
                })
            }),
            Some("pg_catalog") | Some("information_schema") => self.system.resolve(schema, name),
            Some("public") => self.global.open_table(name),
            Some("pg_temp") => self.open_temp(name),
            Some(namespace) if namespace == self.temp_schema => self.open_temp(name),
            // Another session's temp namespace is off-limits (see `is_foreign_temp`).
            Some(namespace) if self.is_foreign_temp(namespace) => {
                Err(StorageError::TableNotFound(name.to_string()))
            }
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
            match self.global.drop_table(&self.temp_schema, name) {
                Err(StorageError::TableNotFound(_)) => self.global.drop_table("public", name),
                other => other,
            }
        } else if self.is_foreign_temp(namespace) {
            // Never drop another session's temp table.
            Err(StorageError::TableNotFound(name.to_string()))
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
        // A temp table (in this session's `pg_temp_N` namespace) shadows a
        // permanent one of the same unqualified name.
        if namespace == "public" && self.temp_has(table) {
            self.global.create_index(&self.temp_schema, table, index)
        } else if self.is_foreign_temp(namespace) {
            // Never index another session's temp table.
            Err(StorageError::IndexTableNotFound(table.to_string()))
        } else {
            self.global.create_index(namespace, table, index)
        }
    }

    fn index_name_exists(&self, namespace: &str, table: &str, index_name: &str) -> bool {
        if namespace == "public" && self.temp_has(table) {
            self.global
                .index_name_exists(&self.temp_schema, table, index_name)
        } else if self.is_foreign_temp(namespace) {
            false
        } else {
            self.global.index_name_exists(namespace, table, index_name)
        }
    }

    fn relations(&self) -> Vec<TableSchema> {
        self.global.relations()
    }

    fn relation_names_in(&self, namespace: &str) -> Vec<String> {
        self.global.relation_names_in(namespace)
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
            // A foreign session's temp namespace holds no views we may see.
            Some(namespace) if self.is_foreign_temp(namespace) => None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_catalog::CatalogRelation;
    use crabgresql_storage_api::Column;
    use crabgresql_types::PgType;

    fn table(namespace: &str, name: &str) -> CatalogRelation {
        let schema =
            TableSchema::in_namespace(name, namespace, vec![Column::new("a", PgType::Int4)]);
        // `temporary` takes the namespace separately and leaves `schema.namespace`
        // alone, so build every relation through `permanent` (which derives the
        // namespace from the schema) and keep the two fields equal — the
        // invariant `SystemCatalog::relation_ref` documents.
        CatalogRelation::permanent(schema)
    }

    /// `pg_table_is_visible` must agree with what an unqualified name actually
    /// resolves to. PG's rule is reachability, not namespace membership, so a
    /// shadowed relation is invisible even though its schema is on the path.
    /// This asserts the two against each other rather than restating the rule,
    /// so a change to `SessionCatalog::resolve`'s search order fails here.
    /// `shadowed` exists in public AND in this session's temp namespace;
    /// `only_public` is plainly reachable; `tucked` needs a schema qualifier;
    /// `pg_am` collides with a catalog relation's name.
    const RELATIONS: &[(&str, &str)] = &[
        ("public", "shadowed"),
        ("pg_temp_1", "shadowed"),
        ("public", "only_public"),
        ("app", "tucked"),
        ("public", "pg_am"),
    ];

    #[test]
    fn visibility_follows_resolution_order() -> anyhow::Result<()> {
        let temp_schema = "pg_temp_1";
        let system = Arc::new(
            crabgresql_catalog::SystemCatalog::with_catalog_relations_fn("db", "owner", || {
                RELATIONS.iter().map(|(ns, n)| table(ns, n)).collect()
            })
            .with_schemas_fn(|| {
                vec![
                    ("app".to_string(), 16_000),
                    ("pg_temp_1".to_string(), 16_001),
                ]
            }),
        );
        let ops = SessionCatalogOps::new(Arc::clone(&system), temp_schema);
        let catalog = SessionCatalog::new(
            Arc::new(StubEngine(RELATIONS.to_vec())),
            Arc::clone(&system) as Arc<dyn TableEngine>,
            temp_schema,
        );

        for (namespace, name) in RELATIONS {
            let oid = system
                .relation_oid_in(namespace, name)
                .ok_or_else(|| anyhow::anyhow!("{namespace}.{name} has no OID"))?;
            let visible = ops
                .table_is_visible(oid)
                .ok_or_else(|| anyhow::anyhow!("{namespace}.{name} has no relation"))?;
            // What does the bare name actually resolve to? Visibility must be
            // exactly "resolution lands on this relation".
            let reachable = match catalog.resolve(None, name) {
                Ok(found) => found.schema().namespace == *namespace,
                Err(_) => false,
            };
            assert_eq!(
                visible, reachable,
                "pg_table_is_visible disagrees with unqualified resolution for {namespace}.{name}"
            );
        }

        // Spell out the cases the rule exists for, so a regression names itself
        // rather than only showing up as a disagreement above.
        let vis = |ns: &str, name: &str| {
            system
                .relation_oid_in(ns, name)
                .and_then(|oid| ops.table_is_visible(oid))
        };
        assert_eq!(vis(temp_schema, "shadowed"), Some(true));
        assert_eq!(
            vis("public", "shadowed"),
            Some(false),
            "temp shadows public"
        );
        assert_eq!(
            vis("public", "pg_am"),
            Some(false),
            "pg_catalog shadows public"
        );
        assert_eq!(vis("public", "only_public"), Some(true));
        assert_eq!(vis("app", "tucked"), Some(false), "needs a qualifier");
        // An OID no relation has is NULL, not false — and answering it must not
        // depend on the relation list.
        assert_eq!(ops.table_is_visible(1), None);

        Ok(())
    }

    /// A stand-in for the global engine: resolves `public` only, as
    /// `PgEngine::open_table` does, so the temp → system → global ordering under
    /// test is the real one.
    struct StubEngine(Vec<(&'static str, &'static str)>);

    impl TableEngine for StubEngine {
        fn create_table(&self, _schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError> {
            unreachable!("the visibility test never creates")
        }

        fn open_table(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
            self.resolve(Some("public"), name)
        }

        fn resolve(
            &self,
            namespace: Option<&str>,
            name: &str,
        ) -> Result<Arc<dyn TableAm>, StorageError> {
            let namespace = namespace.unwrap_or("public");
            match self.0.iter().find(|(ns, n)| *ns == namespace && *n == name) {
                Some((ns, n)) => Ok(crabgresql_catalog::StaticTable::arc(
                    table(ns, n).schema,
                    Vec::new(),
                )),
                None => Err(StorageError::TableNotFound(name.to_string())),
            }
        }

        fn drop_table(&self, _namespace: &str, name: &str) -> Result<(), StorageError> {
            Err(StorageError::TableNotFound(name.to_string()))
        }
    }
}
