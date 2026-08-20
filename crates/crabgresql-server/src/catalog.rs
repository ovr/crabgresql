//! Per-statement name resolution overlay: a session's temp catalog shadows the
//! shared global engine (PG's `pg_temp`-first search), and the read-only system
//! catalogs (`pg_catalog` and schema-qualified `information_schema`) sit behind
//! both on the search path.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use crabgresql_catalog::{
    CatalogBackend, CatalogCursor, CatalogLock, CatalogLockTarget, CatalogPreparedStatement,
    CatalogRelation, CatalogRoutine, CatalogSequence, CatalogSetting, CatalogSource,
    CatalogUserType, CatalogViewDependency, SerialSequenceLookup, SystemCatalog, ViewDepRelation,
};
use crabgresql_executor::{
    CatalogOps, ConstraintDef, ExtensionVersion, IndexDef, RelationSize, SerialSequence,
};
use crabgresql_storage_api::pgstat::{
    DbStatSnapshot, IndexStatSnapshot, PgStatCounters, RelStatSnapshot,
};
use crabgresql_storage_api::{
    CheckConstraint, ColumnProjection, IndexMetadata, RelationMetadata, SequenceAdvance,
    SequenceDefinition, StorageError, TableAm, TableEngine, TableSchema, TypeCatalog,
    ViewDefinition,
};
use crabgresql_txn::{TxnContext, Xid};
use crabgresql_types::ByteaOutput;

use crate::global_catalog::GlobalCatalog;
use crate::query::{catalog_routine, partition_session_relations};
use crate::session::Session;

/// The relations one statement resolved, and the write it is going to perform.
///
/// PostgreSQL answers `pg_locks` from a cluster-wide lock table. There is none
/// here — a relation's `TableLock` lives inside the access method's open handle
/// — so the honest stand-in is the set of relations this statement's own name
/// resolution reached: every one of them is about to be scanned or written, and
/// a scan really does take a shared hold on the relation it reads.
///
/// Filled by [`SessionCatalog`] as it resolves names during binding and read by
/// [`SessionCatalogSource::locks`] during execution — which works only because
/// `SystemCatalog` calls `locks()` lazily, when a query actually opens
/// `pg_locks`, by which time binding has finished and the set is complete.
#[derive(Default)]
pub struct StatementRelations {
    /// Keyed as the *resolution* spelled it rather than as the client wrote it,
    /// so a view's base tables and a partition's leaves land here too, having
    /// gone through the same engine. A set because PostgreSQL also reports one
    /// lock per relation however many times a statement names it, and an
    /// ordered one because that is what keeps the rows stable across runs.
    resolved: Mutex<BTreeSet<(String, String)>>,
    /// The relation this statement writes and the mode PostgreSQL holds on it.
    /// Decided from the statement rather than from which engine method resolved
    /// the name: an unqualified write resolves through `open_table` but a
    /// `public.`-qualified one through `resolve`, so the method proves nothing.
    write_target: Option<(String, &'static str)>,
}

impl StatementRelations {
    pub fn new(write_target: Option<(String, &'static str)>) -> Self {
        Self {
            resolved: Mutex::new(BTreeSet::new()),
            write_target,
        }
    }

    fn record(&self, namespace: &str, name: &str) {
        if let Ok(mut resolved) = self.resolved.lock() {
            resolved.insert((namespace.to_string(), name.to_string()));
        }
    }

    /// One lock per resolved relation. `AccessShareLock` unless the statement
    /// names the relation as its write target, which is the whole reason the
    /// target is carried here.
    fn locks(&self, holder: &CatalogLock) -> Vec<CatalogLock> {
        let Ok(resolved) = self.resolved.lock() else {
            return Vec::new();
        };
        resolved
            .iter()
            .map(|(namespace, name)| {
                let mode = match &self.write_target {
                    Some((target, mode)) if target == name => *mode,
                    _ => "AccessShareLock",
                };
                CatalogLock {
                    target: CatalogLockTarget::Relation {
                        namespace: namespace.clone(),
                        name: name.clone(),
                    },
                    virtualtransaction: holder.virtualtransaction.clone(),
                    pid: holder.pid,
                    mode,
                    granted: true,
                    // PostgreSQL takes a weak relation lock with no conflicting
                    // holder through the per-backend fast path; a lock strong
                    // enough to conflict always goes through the lock table.
                    fastpath: mode == "AccessShareLock",
                    waitstart: None,
                }
            })
            .collect()
    }
}

/// The live server state one session's [`SystemCatalog`] snapshot reflects.
///
/// Every method but the eagerly snapshotted ones below (`cursors`,
/// `prepared_statements`, `settings`) reads through the captured engine handles
/// when called, which is what keeps a `SELECT 1` from enumerating the database:
/// `SystemCatalog` invokes each at most once, and only when the relation it
/// feeds is opened.
#[derive(Clone)]
pub struct SessionCatalogSource {
    /// Backs `relations` and `schemas`: relation metadata, views, sequences and
    /// the temp-namespace instantiation check all come from here.
    engine: Arc<dyn TableEngine>,
    /// Backs `user_types` and `routines`.
    global_catalog: Arc<GlobalCatalog>,
    database: String,
    owner: String,
    temp_schema: String,
    temp_namespace_oid: u32,
    /// Eager, unlike the engine-backed lists: this source outlives the
    /// `&Session` borrow it is built from, so the cursor metadata is
    /// snapshotted here rather than read on demand. Only names and statement
    /// texts are copied — never rows — and a session with no open cursor copies
    /// nothing.
    cursors: Vec<CatalogCursor>,
    /// Eager for the same reason as `cursors`, and just as cheap: only names,
    /// statement texts and type OIDs are copied — never a plan.
    prepared_statements: Vec<CatalogPreparedStatement>,
    /// Eager for the same reason as `cursors`: every value is rendered from the
    /// session, which this source outlives.
    settings: Vec<CatalogSetting>,
    /// Eager for the same reason as `cursors`, and never more than two rows —
    /// see [`Session::locks`].
    locks: Vec<CatalogLock>,
    backend_pid: i32,
    /// The relations this statement resolved, read at `locks()` time rather than
    /// snapshotted here — the set is still being filled while this source is
    /// built.
    relations: Arc<StatementRelations>,
    /// The transaction timestamp, so the timezone views resolve their offsets at
    /// the same instant `now()` reports. Not the *statement* timestamp: `now()`
    /// is `transaction_timestamp()` here as in PostgreSQL, and the two differ
    /// for every statement after the first in a block — visibly so for a
    /// transaction that spans a DST transition.
    now: i64,
    /// The reading session's `bytea_output`, for `pg_class.relpartbound` — the
    /// one catalog column whose text can hold a rendered `bytea`.
    bytea_output: ByteaOutput,
    /// Read through rather than snapshotted here, like the engine: a statement
    /// that never opens a statistics relation must not pay for walking the
    /// counter table.
    stats: Arc<PgStatCounters>,
    /// Eager like `cursors`, and for the same reason: this source outlives the
    /// `&Session` it is built from.
    backend: CatalogBackend,
    /// Whether this source is the one [`SessionCatalogSource::view_dependencies`]
    /// binds *through* rather than the one a statement reads. It answers that
    /// method with nothing; see there for what it is a backstop against.
    nested: bool,
}

impl SessionCatalogSource {
    pub fn new(
        engine: Arc<dyn TableEngine>,
        global_catalog: Arc<GlobalCatalog>,
        session: &Session,
        relations: Arc<StatementRelations>,
    ) -> Self {
        // Sorted so `SELECT * FROM pg_cursors` is stable across runs, where
        // PostgreSQL's hash order is not.
        let mut cursors: Vec<_> = session
            .cursors
            .iter()
            .map(|(name, cursor)| CatalogCursor {
                name: name.clone(),
                statement: cursor.statement.clone(),
                is_holdable: cursor.hold,
                // `DECLARE … BINARY` is rejected outright, so no open cursor is
                // ever a binary one.
                is_binary: false,
                // Every materialised cursor can scan backward unless it was
                // declared NO SCROLL.
                is_scrollable: cursor.scroll != Some(false),
                creation_time: cursor.creation_time,
            })
            .collect();
        cursors.sort_by(|a, b| a.name.cmp(&b.name));
        // Sorted for the same reason the cursors are.
        let mut prepared_statements: Vec<_> = session
            .prepared
            .iter()
            // The unnamed statement is not a row: PostgreSQL's
            // `pg_prepared_statements` lists only statements a client could name.
            .filter(|(name, _)| !name.is_empty())
            .map(|(name, prepared)| CatalogPreparedStatement {
                name: name.clone(),
                statement: prepared.statement.clone(),
                prepare_time: prepared.prepare_time,
                parameter_types: prepared.param_types.iter().map(|t| t.oid()).collect(),
                result_types: prepared
                    .result_columns
                    .as_ref()
                    .map(|cols| cols.iter().map(|c| c.ty.oid()).collect()),
                from_sql: prepared.from_sql,
                // PostgreSQL splits executions by the plan they used: a
                // statement with no parameters has nothing to specialize on, so
                // its plan is generic from the first execution, while a
                // parameterized one is planned per argument set. This build
                // re-plans everything, so the split is decided by the parameter
                // count alone rather than by a plan cache's choice — which lands
                // on the same column PostgreSQL fills for both shapes.
                generic_plans: match prepared.param_types.is_empty() {
                    true => prepared.executions,
                    false => 0,
                },
                custom_plans: match prepared.param_types.is_empty() {
                    true => 0,
                    false => prepared.executions,
                },
            })
            .collect();
        prepared_statements.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            engine,
            global_catalog,
            database: session.database.clone(),
            owner: session.user.clone(),
            temp_schema: session.temp_schema.clone(),
            temp_namespace_oid: session.temp_namespace_oid,
            cursors,
            prepared_statements,
            settings: crate::guc::catalog_settings(session),
            locks: session.locks(),
            backend_pid: session.backend_id,
            relations,
            now: session.xact_start(),
            bytea_output: session.bytea_output,
            stats: Arc::clone(&session.stats),
            backend: session.backend(),
            nested: false,
        }
    }
}

/// The relations one view's stored query reads, each with the columns it reads;
/// see [`SessionCatalogSource::view_dependencies`] for the rules.
///
/// Planned rather than merely bound: the projection pass runs inside
/// [`crabgresql_planner::plan`], and it is that pass which computes what a scan
/// leaf actually reads.
fn view_reads(
    engine: &Arc<dyn TableEngine>,
    type_catalog: &Arc<dyn TypeCatalog>,
    view: &ViewDefinition,
) -> Vec<ViewDepRelation> {
    let whole = |namespace: &str, name: &str| ViewDepRelation {
        namespace: namespace.to_string(),
        name: name.to_string(),
        columns: None,
    };
    let mut reads: Vec<ViewDepRelation> = view
        .depends_on
        .iter()
        .map(|key| {
            let (namespace, name) = key.split_once('.').unwrap_or(("public", key));
            whole(namespace, name)
        })
        .collect();
    let Some(columns) = view_scan_columns(engine, type_catalog, &view.sql) else {
        return reads;
    };
    for read in &mut reads {
        if let Some(names) = columns.get(&(read.namespace.clone(), read.name.clone())) {
            read.columns = Some(names.clone());
        }
    }
    reads
}

/// Plan `sql` and report, per scanned relation, the column names its scan
/// reads.
///
/// [`ColumnProjection::All`] is read as *every* column rather than as "unknown".
/// That is what it literally means for the common shape — `SELECT * FROM t`
/// narrows to nothing precisely because the scan reads the whole row, and
/// PostgreSQL records one edge per column there — and where it is instead the
/// pass's fail-safe answer, claiming every column errs toward too many edges
/// rather than too few. The wrong direction would be to under-report: a client
/// reading these rows would conclude a column has no dependent when it has one.
fn view_scan_columns(
    engine: &Arc<dyn TableEngine>,
    type_catalog: &Arc<dyn TypeCatalog>,
    sql: &str,
) -> Option<std::collections::HashMap<(String, String), Vec<String>>> {
    let statements = crabgresql_parser::parse(sql).ok()?;
    let query = statements.iter().find_map(|stmt| match stmt {
        crabgresql_parser::ast::Statement::Query(query) => Some(query),
        _ => None,
    })?;
    let logical = crabgresql_binder::bind_query(engine, type_catalog, query).ok()?;
    let physical = crabgresql_planner::plan(logical, Default::default());
    let mut out: std::collections::HashMap<(String, String), Vec<String>> =
        std::collections::HashMap::new();
    for (schema, projection) in crabgresql_planner::scan_projections(&physical) {
        let key = (schema.namespace.clone(), schema.name.clone());
        let read: Vec<String> = match &projection {
            ColumnProjection::All => schema.columns.iter().map(|c| c.name.clone()).collect(),
            ColumnProjection::Some(ordinals) => ordinals
                .iter()
                .filter_map(|i| schema.columns.get(*i).map(|c| c.name.clone()))
                .collect(),
        };
        let names = out.entry(key).or_default();
        names.extend(read);
        names.sort();
        names.dedup();
    }
    Some(out)
}

impl CatalogSource for SessionCatalogSource {
    fn relations(&self) -> Vec<CatalogRelation> {
        // Reflect the permanent relations plus only THIS session's temp
        // relations (the shared visibility rule).
        let (permanent, own_temp) =
            partition_session_relations(self.engine.relation_metadata(), &self.temp_schema);
        let mut rels: Vec<_> = permanent
            .into_iter()
            .map(CatalogRelation::permanent_metadata)
            .collect();
        rels.extend(own_temp.into_iter().map(|metadata| {
            let mut relation =
                CatalogRelation::temporary(metadata.schema, self.temp_schema.clone());
            relation.indexes = metadata.indexes;
            relation.stats = metadata.stats;
            // A temp table toasts and is numbered like any other, so both must
            // reach `pg_class` too — the constructor defaults them, so they have
            // to be carried across explicitly like the two fields above.
            relation.toast = metadata.toast;
            relation.filenodes = metadata.filenodes;
            relation
        }));
        // Views reflect into pg_class as relkind='v' / pg_attribute columns /
        // information_schema.tables as VIEW.
        rels.extend(self.engine.views().into_iter().map(|view| {
            // Deparsed here rather than in the catalog crate, which does not
            // depend on the binder.
            let column_names: Vec<_> = view.columns.iter().map(|c| c.name.clone()).collect();
            let definition =
                crabgresql_binder::ruleutils::view_definition(&view.sql, false, &column_names);
            CatalogRelation::view(
                TableSchema::in_namespace(view.name, view.namespace, view.columns),
                definition,
            )
        }));
        // Sequences reflect into pg_class as relkind='S' and feed
        // pg_catalog.pg_sequence.
        rels.extend(self.engine.sequences().into_iter().map(|seq| {
            // The counter is only reportable once `is_called` — see
            // `CatalogSequence::last_value`.
            let last_value = self
                .engine
                .sequence_current(&seq.namespace, &seq.name)
                .and_then(|(value, is_called)| is_called.then_some(value));
            let relfilenode = self.engine.sequence_relfilenode(&seq.namespace, &seq.name);
            let mut relation = CatalogRelation::sequence(
                seq.name,
                seq.namespace,
                CatalogSequence {
                    type_oid: seq.data_type.oid(),
                    start: seq.start,
                    increment: seq.increment,
                    min: seq.min,
                    max: seq.max,
                    cache: seq.cache,
                    cycle: seq.cycle,
                    last_value,
                    owned_by: seq.owned_by,
                },
            );
            relation.filenodes.rel = relfilenode;
            relation
        }));
        // Stamp each relation with the generation of the DDL that last changed
        // *it*, so its catalog rows carry a state number of their own. Applied
        // here, after every kind of relation has been built, rather than in each
        // of the four constructors above — one lookup per relation against a map
        // the DDL path maintains (`GlobalCatalog::note_ddl_shapes`). A relation
        // the map has never seen keeps `0` and falls back to the catalog-wide
        // generation.
        let ddl_xids = self.global_catalog.relation_ddl_xids();
        for relation in &mut rels {
            let key = (relation.namespace.clone(), relation.schema.name.clone());
            relation.ddl_xid = ddl_xids.get(&key).copied().unwrap_or(0);
        }
        rels
    }

    fn database(&self) -> &str {
        &self.database
    }

    fn owner(&self) -> &str {
        &self.owner
    }

    fn user_types(&self) -> Vec<CatalogUserType> {
        self.global_catalog
            .user_types()
            .into_iter()
            .map(|t| CatalogUserType {
                oid: t.oid,
                name: t.name,
                enum_labels: t.enum_labels,
            })
            .collect()
    }

    fn routines(&self) -> Vec<CatalogRoutine> {
        self.global_catalog
            .functions()
            .iter()
            .map(catalog_routine)
            .collect()
    }

    /// What each stored view reads, at column granularity, for `pg_depend`.
    ///
    /// Two sources are crossed, because neither alone is right. The relation
    /// list is [`ViewDefinition::depends_on`], computed from the view's SQL
    /// *before* views are expanded — so a view over another view keeps naming
    /// that view, which is the edge PostgreSQL records. The columns come from
    /// planning the view's query and reading each scan leaf's stamped
    /// projection, which only exists *after* expansion. A dependency with no
    /// matching leaf (a view, or a relation read only from an expression
    /// subquery) keeps `columns: None` — the whole-relation edge, which is a
    /// shape PostgreSQL stores too.
    ///
    /// Never fails: a view whose SQL no longer binds (its base table was
    /// dropped from under it) contributes relation-granular edges rather than
    /// an error, because a broken view must not make `pg_depend` unreadable.
    ///
    /// One divergence to know about: a view that reads *no* column of a
    /// relation (`SELECT count(*) FROM t`) records the edge on the single
    /// column the projection pass narrows an empty demand to
    /// ([`ColumnProjection::of`] turns an empty demand into the narrowest
    /// column rather than none), where PostgreSQL records the relation as a
    /// whole. That column is a cost artifact, not a read: adding a narrower
    /// column to the relation moves the recorded edge without the view
    /// changing. TODO: have the projection pass report the demand it computed,
    /// so an empty one can be told from a one-column one.
    fn view_dependencies(&self) -> Vec<CatalogViewDependency> {
        // A view whose body reads `pg_depend` would have this method binding a
        // query that reaches `pg_depend` again. What actually stops it is that
        // the relation is registered **deferred**: binding resolves its name and
        // schema without building its rows, so this method is never re-entered
        // and the snapshot's `OnceLock` never sees a second visit. `nested` is
        // the backstop for the day that registration changes — the nested source
        // answers with no view edges, so the recursion terminates one level down
        // instead of deadlocking. Covered by
        // `e2e::a_view_over_pg_depend_is_readable`.
        if self.nested {
            return Vec::new();
        }
        let mut nested = self.clone();
        nested.nested = true;
        let system: Arc<dyn TableEngine> = Arc::new(SystemCatalog::from_source(Arc::new(nested)));
        let engine: Arc<dyn TableEngine> = Arc::new(SessionCatalog::new(
            Arc::clone(&self.engine),
            system,
            self.temp_schema.clone(),
        ));
        let type_catalog: Arc<dyn TypeCatalog> = Arc::clone(&self.global_catalog) as _;
        self.engine
            .views()
            .into_iter()
            .map(|view| CatalogViewDependency {
                reads: view_reads(&engine, &type_catalog, &view),
                namespace: view.namespace,
                name: view.name,
            })
            .collect()
    }

    fn schemas(&self) -> Vec<(String, u32)> {
        let mut schemas = self.engine.schemas();
        // Reflect this session's `pg_temp_N` namespace with a stable synthetic
        // OID, but only once it holds a temp relation (as PG instantiates
        // pg_temp_N lazily). Feeding it through the one `schemas` list keeps
        // `pg_namespace` and `pg_class.relnamespace` consistent; nothing is
        // persisted. `relation_names_in` is cheap.
        if !self.engine.relation_names_in(&self.temp_schema).is_empty() {
            schemas.push((self.temp_schema.clone(), self.temp_namespace_oid));
        }
        schemas
    }

    fn cursors(&self) -> Vec<CatalogCursor> {
        self.cursors.clone()
    }

    fn prepared_statements(&self) -> Vec<CatalogPreparedStatement> {
        self.prepared_statements.clone()
    }

    fn locks(&self) -> Vec<CatalogLock> {
        let mut locks = self.locks.clone();
        // Attributed to the same transaction as the rows above; a source with no
        // session behind it has no holder to name and reports nothing.
        if let Some(holder) = locks.first().cloned() {
            locks.extend(self.relations.locks(&holder));
        }
        locks
    }

    fn backend_pid(&self) -> i32 {
        self.backend_pid
    }

    fn catalog_xmin(&self) -> u64 {
        self.global_catalog.ddl_xid()
    }

    /// The block columns come from the engine's buffer pool, whose totals are
    /// this database's totals because there is exactly one database. An engine
    /// with no pool leaves them at zero — the truth for a relation held in RAM.
    fn database_stats(&self) -> DbStatSnapshot {
        let mut snapshot = self.stats.database_snapshot();
        if let Some(buffers) = self.engine.buffer_stats() {
            snapshot.blks_hit = buffers.hits;
            snapshot.blks_read = buffers.reads;
        }
        snapshot
    }

    fn table_stats(&self) -> Vec<RelStatSnapshot> {
        self.stats.relation_snapshots()
    }

    fn index_stats(&self) -> Vec<IndexStatSnapshot> {
        self.stats.index_snapshots()
    }

    /// This session and no other; see [`CatalogSource::backends`].
    fn backends(&self) -> Vec<CatalogBackend> {
        vec![self.backend.clone()]
    }

    fn settings(&self) -> Vec<CatalogSetting> {
        self.settings.clone()
    }

    fn now(&self) -> i64 {
        self.now
    }

    fn bytea_output(&self) -> ByteaOutput {
        self.bytea_output
    }
}

/// The executor-facing catalog handle: answers `pg_get_userbyid` and
/// `pg_table_is_visible` against the same [`SystemCatalog`] snapshot that built
/// this statement's `pg_class` rows, so the OIDs the client reads back are the
/// OIDs these functions resolve. Owns its `Arc` so it can live in a suspended
/// portal's `ExecContext`, like [`crate::session::SessionSequences`].
pub struct SessionCatalogOps {
    system: Arc<SystemCatalog>,
    temp_schema: String,
    /// The same search-path-aware catalog the statement resolves relations
    /// against, so `pg_get_viewdef` finds a view by exactly the name the query
    /// would. `None` in tests that exercise only the OID-keyed methods.
    relations: Option<Arc<dyn TableEngine>>,
}

impl SessionCatalogOps {
    pub fn new(system: Arc<SystemCatalog>, temp_schema: impl Into<String>) -> Self {
        Self {
            system,
            temp_schema: temp_schema.into(),
            relations: None,
        }
    }

    /// Attach the relation catalog that backs [`CatalogOps::view_sql`].
    pub fn with_relations(mut self, relations: Arc<dyn TableEngine>) -> Self {
        self.relations = Some(relations);
        self
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
    /// found is this one. A relation in a `CREATE SCHEMA` namespace is
    /// correctly invisible because nothing but a qualified name reaches it.
    ///
    /// TODO: walk the session's `search_path` instead of this fixed order, so
    /// visibility follows the configured path as it does in PostgreSQL.
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

    fn proc_name(&self, oid: u32) -> Option<(String, String)> {
        self.system.proc_name(oid)
    }

    fn proc_oids(&self, namespace: Option<&str>, name: &str) -> Vec<u32> {
        self.system.proc_oids(namespace, name)
    }

    fn oper_name(&self, oid: u32) -> Option<(String, String)> {
        self.system.oper_name(oid)
    }

    fn oper_oids(&self, namespace: Option<&str>, name: &str) -> Vec<u32> {
        self.system.oper_oids(namespace, name)
    }

    /// The comments `pg_description` publishes. The session's own catalog has
    /// nothing to add until `COMMENT ON` exists, so this reads the same list
    /// the relation itself serves.
    ///
    /// A catalog *name* resolves the way PostgreSQL's `obj_description`
    /// resolves it — in `pg_catalog` only, so a user table called `pg_type`
    /// cannot answer for the real one — and a name that resolves to nothing
    /// finds no comment rather than raising.
    fn object_description(&self, objoid: u32, objsubid: i32, catalog: Option<&str>) -> Vec<String> {
        let Some(catalog) = catalog else {
            return crabgresql_catalog::object_descriptions_any_class(objoid, objsubid)
                .into_iter()
                .map(str::to_string)
                .collect();
        };
        crabgresql_catalog::builtin_relation_oid(catalog)
            .and_then(|classoid| crabgresql_catalog::object_description(classoid, objoid, objsubid))
            .map(str::to_string)
            .into_iter()
            .collect()
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

    fn view_sql(&self, namespace: Option<&str>, name: &str) -> Option<(String, Vec<String>)> {
        self.relations
            .as_ref()?
            .resolve_view(namespace, name)
            .map(|v| (v.sql, v.columns.into_iter().map(|c| c.name).collect()))
    }

    fn constraint_def(&self, oid: u32) -> Option<ConstraintDef> {
        let (contype, columns, expr) = self.system.constraint_def(oid)?;
        Some(ConstraintDef {
            contype,
            columns,
            expr,
        })
    }

    fn available_extensions(&self) -> Vec<ExtensionVersion> {
        crabgresql_catalog::available_extensions()
            .iter()
            .map(|ext| ExtensionVersion {
                name: ext.name.to_string(),
                version: ext.version.to_string(),
                superuser: ext.superuser,
                trusted: ext.trusted,
                relocatable: ext.relocatable,
                schema: ext.schema.to_string(),
                comment: ext.comment.to_string(),
            })
            .collect()
    }

    fn partition_ancestors(&self, oid: u32) -> Vec<u32> {
        self.system.partition_ancestors(oid)
    }

    fn index_def(&self, oid: u32) -> Option<IndexDef> {
        let (index, table) = self.system.index_def(oid)?;
        Some(IndexDef { index, table })
    }

    fn serial_sequence(&self, oid: u32, column: &str) -> SerialSequence {
        match self.system.serial_sequence(oid, column) {
            SerialSequenceLookup::Owned { namespace, name } => {
                SerialSequence::Owned { namespace, name }
            }
            SerialSequenceLookup::Unowned => SerialSequence::Unowned,
            SerialSequenceLookup::NoColumn { relation } => SerialSequence::NoColumn { relation },
            SerialSequenceLookup::NoRelation => SerialSequence::NoRelation,
        }
    }

    fn current_database(&self) -> String {
        self.system.database().to_string()
    }

    fn current_user(&self) -> String {
        self.system.owner().to_string()
    }

    /// The same string as [`CatalogOps::current_user`]: `SET ROLE` is accepted
    /// and ignored, so nothing can make the two differ.
    ///
    /// TODO: track `SET ROLE` in `current_user`, so it can differ from the
    /// authenticated role this method reports.
    fn session_user(&self) -> String {
        self.system.owner().to_string()
    }

    /// Mirrors the resolution order `table_is_visible` and `rel_oid` above
    /// already implement — temp, then `pg_catalog`, then `public`. With
    /// PostgreSQL's default `"$user", public` and no `$user` schema, the two
    /// agree exactly.
    ///
    /// TODO: return the session's `search_path` setting — `SET search_path` is
    /// among the names [`crate::guc`] silently accepts and ignores, so this
    /// list is fixed.
    fn search_path(&self, include_implicit: bool) -> Vec<String> {
        let mut out = Vec::new();
        if include_implicit {
            // Only once the namespace exists — the same lazy rule
            // `pg_namespace` reflects it by, so this agrees with
            // `pg_my_temp_schema()`.
            if self.my_temp_schema().is_some() {
                out.push(self.temp_schema.clone());
            }
            out.push("pg_catalog".to_string());
        }
        out.push("public".to_string());
        out
    }

    fn backend_pid(&self) -> i32 {
        self.system.backend_pid()
    }

    fn my_temp_schema(&self) -> Option<u32> {
        self.system.namespace_oid(&self.temp_schema)
    }

    /// The catalog measures in pages; the size functions answer in bytes, so the
    /// one multiplication lives here. `u64` throughout: a page count is a `u32`
    /// and `8192 *` it does not fit one.
    fn relation_size(&self, oid: u32) -> Option<RelationSize> {
        let pages = self.system.relation_pages(oid)?;
        let bytes = |pages: u32| u64::from(pages) * crabgresql_storage_api::PAGE_BYTES;
        Some(RelationSize {
            main: bytes(pages.main),
            toast: bytes(pages.toast),
            indexes: bytes(pages.indexes),
        })
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
    /// Where every successful resolution is recorded for `pg_locks`. `None` in
    /// tests and on paths with no statement behind them, which then report no
    /// relation locks rather than a set from nowhere.
    relations: Option<Arc<StatementRelations>>,
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
            relations: None,
        }
    }

    /// Report what this statement resolves into `pg_locks`.
    pub fn recording(mut self, relations: Arc<StatementRelations>) -> Self {
        self.relations = Some(relations);
        self
    }

    /// Note a resolved relation under the name the *resolution* landed on, not
    /// the one the client wrote.
    fn record(&self, table: &Result<Arc<dyn TableAm>, StorageError>) {
        let (Some(relations), Ok(table)) = (&self.relations, table) else {
            return;
        };
        let schema = table.schema();
        relations.record(&schema.namespace, &schema.name);
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
        let table = Self::or_else_not_found(self.open_temp(name), || self.global.open_table(name));
        self.record(&table);
        table
    }

    /// Search-path-aware read resolution. An unqualified name searches temp →
    /// system → global, mirroring PostgreSQL's implicit order (`pg_temp`, then
    /// `pg_catalog`, then the path): so `pg_catalog` wins over a like-named user
    /// relation in `public`, as in PG. A schema qualifier routes to exactly one
    /// namespace.
    fn resolve(&self, schema: Option<&str>, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        let table = match schema {
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
        };
        self.record(&table);
        table
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

    /// Routed exactly like `create_index` — the two are the halves of one
    /// `ALTER TABLE ... ADD PRIMARY KEY`, so they must land on the same
    /// relation. Without this arm the default would forward `"public"` straight
    /// through and mark a same-named *permanent* table's columns NOT NULL.
    fn set_column_not_null(
        &self,
        namespace: &str,
        table: &str,
        columns: &[usize],
    ) -> Result<(), StorageError> {
        if namespace == "public" && self.temp_has(table) {
            self.global
                .set_column_not_null(&self.temp_schema, table, columns)
        } else if self.is_foreign_temp(namespace) {
            Err(StorageError::IndexTableNotFound(table.to_string()))
        } else {
            self.global.set_column_not_null(namespace, table, columns)
        }
    }

    fn add_check_constraint(
        &self,
        namespace: &str,
        table: &str,
        check: CheckConstraint,
    ) -> Result<(), StorageError> {
        if namespace == "public" && self.temp_has(table) {
            self.global
                .add_check_constraint(&self.temp_schema, table, check)
        } else if self.is_foreign_temp(namespace) {
            Err(StorageError::IndexTableNotFound(table.to_string()))
        } else {
            self.global.add_check_constraint(namespace, table, check)
        }
    }

    /// The inverse of `create_index`, routed identically, for a caller that
    /// names the *table* an index belongs to — the compensating drop when a
    /// multi-action `ALTER TABLE` fails partway.
    ///
    /// Without this arm the trait default runs and returns `TableNotFound`
    /// unconditionally, so such a rollback silently drops nothing.
    ///
    /// `DROP INDEX` deliberately does not come through here: there the user
    /// names the *index* and the owning table has to be found first, so it
    /// resolves against the concrete store to avoid a temp table's name
    /// shadowing the permanent relation that actually owns the index.
    fn drop_index(
        &self,
        namespace: &str,
        table: &str,
        index_name: &str,
    ) -> Result<(), StorageError> {
        if namespace == "public" && self.temp_has(table) {
            self.global.drop_index(&self.temp_schema, table, index_name)
        } else if self.is_foreign_temp(namespace) {
            Err(StorageError::IndexTableNotFound(table.to_string()))
        } else {
            self.global.drop_index(namespace, table, index_name)
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

    /// Forwarded rather than left to the trait default, which derives the links
    /// from `relations()` — a schema deep-clone per relation. The binder asks
    /// this of every base relation of every statement, and it is handed *this*
    /// overlay, not the engine underneath, so without this arm the engine's
    /// cheap override is unreachable and the default's cost is what every query
    /// actually pays.
    fn inheritance_links(&self) -> Vec<((String, String), (String, String))> {
        self.global.inheritance_links()
    }

    /// Analyze the same relation `open_table` would resolve: this session's temp
    /// table shadows a permanent one of the same unqualified name, and another
    /// session's temp table is never reachable.
    fn analyze(&self, namespace: &str, name: &str, txn: &TxnContext) -> Result<(), StorageError> {
        if namespace == "public" && self.temp_has(name) {
            self.global.analyze(&self.temp_schema, name, txn)
        } else if self.is_foreign_temp(namespace) {
            Err(StorageError::TableNotFound(name.to_string()))
        } else {
            self.global.analyze(namespace, name, txn)
        }
    }

    /// Same namespace resolution as [`SessionCatalog::analyze`]: an unqualified
    /// name may be this session's temp table, and another session's temp schema
    /// is not reachable at all.
    fn vacuum_table(&self, namespace: &str, name: &str, oldest: Xid) -> Result<u64, StorageError> {
        if namespace == "public" && self.temp_has(name) {
            self.global.vacuum_table(&self.temp_schema, name, oldest)
        } else if self.is_foreign_temp(namespace) {
            Err(StorageError::TableNotFound(name.to_string()))
        } else {
            self.global.vacuum_table(namespace, name, oldest)
        }
    }

    /// Every view is created in the permanent (global) catalog, like the
    /// non-temp default in `create_table`.
    ///
    /// TODO: create temporary views in this session's temp namespace;
    /// `CREATE TEMP VIEW` is rejected outright today.
    fn create_view(&self, def: ViewDefinition) -> Result<(), StorageError> {
        self.global.create_view(def)
    }

    /// Search-path-aware view resolution, mirroring [`SessionCatalog::resolve`].
    /// Views live only in the permanent catalog (see the temp-view `TODO` on
    /// [`SessionCatalog::create_view`]), so an unqualified or
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

    /// Sequences live only in the permanent catalog, so every sequence
    /// operation routes to `global`, like views.
    ///
    /// TODO: support temporary sequences; `CREATE TEMP SEQUENCE` is rejected
    /// outright today.
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

    fn sequence_current(&self, namespace: &str, name: &str) -> Option<(i64, bool)> {
        self.global.sequence_current(namespace, name)
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
        self.global
            .sequence_setval(namespace, name, value, is_called)
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
        let system = Arc::new(crabgresql_catalog::SystemCatalog::from_source(Arc::new(
            crabgresql_catalog::StaticSource::new(
                RELATIONS.iter().map(|(ns, n)| table(ns, n)).collect(),
            )
            .database("db")
            .owner("owner")
            .schemas(vec![
                ("app".to_string(), 16_000),
                ("pg_temp_1".to_string(), 16_001),
            ]),
        )));
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

    /// Records the namespace each index DDL call was routed to.
    #[derive(Default)]
    struct RoutingEngine {
        relations: Vec<(&'static str, &'static str)>,
        dropped: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl TableEngine for RoutingEngine {
        fn create_table(&self, _schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError> {
            unreachable!("routing test never creates")
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
            match self
                .relations
                .iter()
                .find(|(ns, n)| *ns == namespace && *n == name)
            {
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

        fn drop_index(
            &self,
            namespace: &str,
            table: &str,
            _index_name: &str,
        ) -> Result<(), StorageError> {
            self.dropped
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"))
                .push((namespace.to_string(), table.to_string()));
            Ok(())
        }
    }

    /// `drop_index` must route exactly as `create_index` does: they are the make
    /// and unmake of one index, so a statement that creates through the overlay
    /// and compensates through it must reach the same relation.
    ///
    /// The regression this pins is silent, not loud. `SessionCatalog` did not
    /// override `drop_index`, so the trait default ran and returned
    /// `TableNotFound` unconditionally — and the one caller discards the error,
    /// which made every compensating drop a no-op that still looked like a
    /// rollback. Asserting the *namespace reached* is the only thing that
    /// catches it, since a returned `Ok` alone would not.
    #[test]
    fn drop_index_routes_like_create_index() -> anyhow::Result<()> {
        let temp_schema = "pg_temp_1";
        // `shadowed` exists in both namespaces; `only_public` in neither temp.
        let engine = Arc::new(RoutingEngine {
            relations: vec![
                ("public", "shadowed"),
                ("pg_temp_1", "shadowed"),
                ("public", "only_public"),
            ],
            dropped: std::sync::Mutex::new(Vec::new()),
        });
        let system = Arc::new(crabgresql_catalog::SystemCatalog::with_catalog_relations(
            "db",
            "owner",
            Vec::new(),
        ));
        let catalog = SessionCatalog::new(
            Arc::clone(&engine) as Arc<dyn TableEngine>,
            system as Arc<dyn TableEngine>,
            temp_schema,
        );

        // A temp table shadows the permanent one: the drop follows the create.
        catalog.drop_index("public", "shadowed", "shadowed_a_key")?;
        // Nothing shadows this one, so it stays in public.
        catalog.drop_index("public", "only_public", "only_public_a_key")?;
        // Another session's temp table is unreachable, and must not fall through
        // to a same-named permanent relation.
        assert!(
            catalog
                .drop_index("pg_temp_9", "shadowed", "shadowed_a_key")
                .is_err()
        );

        let dropped = engine
            .dropped
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .clone();
        assert_eq!(
            dropped,
            vec![
                ("pg_temp_1".to_string(), "shadowed".to_string()),
                ("public".to_string(), "only_public".to_string()),
            ]
        );
        Ok(())
    }

    /// An engine that answers `inheritance_links` but refuses `relations`.
    ///
    /// This shape is deliberate. `SessionCatalog` once failed to forward
    /// `inheritance_links`, so the trait default ran and derived the links from
    /// `relations()` — a schema deep-clone of the whole catalog on the path that
    /// the method exists to keep cheap. **No assertion on the returned links
    /// could have caught it**, because the default computes the same answer; only
    /// the cost differed. So this double makes the cost fatal instead, and the
    /// test below fails loudly if the forwarding arm is ever removed again.
    struct LinksOnlyEngine;

    impl TableEngine for LinksOnlyEngine {
        fn create_table(&self, _schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError> {
            unreachable!("this test never creates")
        }

        fn open_table(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
            Err(StorageError::TableNotFound(name.to_string()))
        }

        fn resolve(
            &self,
            _namespace: Option<&str>,
            name: &str,
        ) -> Result<Arc<dyn TableAm>, StorageError> {
            Err(StorageError::TableNotFound(name.to_string()))
        }

        fn drop_table(&self, _namespace: &str, name: &str) -> Result<(), StorageError> {
            Err(StorageError::TableNotFound(name.to_string()))
        }

        fn relations(&self) -> Vec<TableSchema> {
            panic!(
                "relations() is the expensive path: SessionCatalog must forward \
                 inheritance_links to the engine instead of falling back to the \
                 trait default that derives links from full schema clones"
            );
        }

        fn inheritance_links(&self) -> Vec<((String, String), (String, String))> {
            vec![(
                ("public".to_string(), "child".to_string()),
                ("public".to_string(), "parent".to_string()),
            )]
        }
    }

    #[test]
    fn session_catalog_forwards_inheritance_links_rather_than_deriving_them() {
        let system = Arc::new(crabgresql_catalog::SystemCatalog::new());
        let catalog = SessionCatalog::new(
            Arc::new(LinksOnlyEngine),
            Arc::clone(&system) as Arc<dyn TableEngine>,
            "pg_temp_1",
        );
        assert_eq!(
            catalog.inheritance_links(),
            vec![(
                ("public".to_string(), "child".to_string()),
                ("public".to_string(), "parent".to_string())
            )],
        );
    }
}
