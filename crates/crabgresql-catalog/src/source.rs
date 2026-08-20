//! What the server hands the catalog: the shapes of live state, and the trait a
//! session implements to supply it.
//!
//! Kept apart from [`crate::SystemCatalog`] because the direction of dependency
//! is one-way — these types know nothing about how a relation is rendered, so
//! new live state is added here without touching the snapshot machinery.

use crabgresql_storage_api::pgstat::{DbStatSnapshot, IndexStatSnapshot, RelStatSnapshot};
use crabgresql_storage_api::{
    Column, IndexMetadata, RelStats, RelationFilenodes, RelationMetadata, TableSchema,
};
use crabgresql_types::{ByteaOutput, PgType};

/// The relation kind reflected into `pg_class.relkind` / `information_schema`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelKind {
    /// An ordinary table (`relkind = 'r'`, table_type `BASE TABLE`).
    Table,
    /// A partitioned (parent) table (`relkind = 'p'`). Holds no rows of its own;
    /// still `BASE TABLE` in `information_schema.tables`, as in PG.
    PartitionedTable,
    /// A view (`relkind = 'v'`, table_type `VIEW`).
    View,
    /// A sequence (`relkind = 'S'`). Not a table, so it is omitted from
    /// `information_schema.tables`/`.columns`.
    Sequence,
}

/// A sequence's parameters, reflected into `pg_sequence`. Carried on the
/// [`CatalogRelation`] whose [`RelKind::Sequence`] entry it belongs to, so the
/// sequence's `pg_class` OID (assigned positionally) can be reused as `seqrelid`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogSequence {
    pub type_oid: u32,
    pub start: i64,
    pub increment: i64,
    pub min: i64,
    pub max: i64,
    pub cache: i64,
    pub cycle: bool,
    /// The counter as `pg_sequences.last_value` reports it: `None` until the
    /// sequence has been read from, which is how PostgreSQL distinguishes a
    /// fresh sequence from one that has handed out its `start` value — the two
    /// hold the same number and differ only in having been called.
    pub last_value: Option<i64>,
    /// The table this sequence is owned by (PostgreSQL's `OWNED BY`). A
    /// `serial` column is the only thing that sets it — the standalone clause is
    /// refused at DDL — so an owned sequence always lives in its table's
    /// namespace, which is why the name here is unqualified.
    ///
    /// It is what makes the sequence's `pg_depend` edge an *auto* dependency on
    /// the owning column rather than nothing at all — a plain
    /// `DEFAULT nextval('s')` records the reverse edge only (from the default),
    /// and PostgreSQL was observed to keep the two cases exactly that far apart.
    /// The column itself is not carried: it is recovered from the owning table's
    /// defaults, which is the only place the link is written down.
    pub owned_by: Option<String>,
}

/// A live relation exposed through the system catalogs.
#[derive(Clone, Debug)]
pub struct CatalogRelation {
    pub schema: TableSchema,
    pub indexes: Vec<IndexMetadata>,
    pub namespace: String,
    pub temporary: bool,
    pub kind: RelKind,
    /// Sequence parameters, `Some` only when `kind` is [`RelKind::Sequence`].
    pub sequence: Option<CatalogSequence>,
    /// Size estimates feeding `pg_class.relpages`/`reltuples`. Relations with no
    /// storage of their own — views and partitioned parents — leave this at
    /// [`RelStats::unknown`], which renders as PostgreSQL's never-analyzed
    /// sentinel; sequences are reported from their fixed shape instead.
    pub stats: RelStats,
    /// The size of this relation's out-of-line ("TOAST") storage, or `None` if it
    /// has none. `Some` is what gives the relation a `pg_toast.pg_toast_<oid>`
    /// row and a non-zero `pg_class.reltoastrelid`.
    pub toast: Option<RelStats>,
    /// A view's body, **already deparsed** into the canonical SQL PostgreSQL
    /// prints — what `pg_views.definition` shows and what `pg_rewrite` stores as
    /// the `_RETURN` rule's action. `None` for everything that is not a view,
    /// and for a view whose body the deparser cannot render.
    ///
    /// Deparsed by the supplier rather than here: rendering lives in
    /// `crabgresql-binder`, which this crate deliberately does not depend on.
    /// A view that cannot be rendered must stay `None` rather than fall back to
    /// the SQL as typed — the two are different strings, and a dump built from
    /// the wrong one is wrong silently.
    pub definition: Option<String>,
    /// The transaction id of the DDL that last changed this relation's
    /// definition, or `0` when nothing has recorded one — after a restart, say,
    /// since no durable record says when a definition last moved.
    ///
    /// This is what the relation's own catalog rows report as their `xmin`; `0`
    /// falls back to [`CatalogSource::catalog_xmin`], the catalog-wide
    /// generation. See [`crate::StaticTable::with_xmin`] for why a state number
    /// is the honest answer here at all.
    pub ddl_xid: u64,
    /// The physical file numbers behind this relation, feeding
    /// `pg_class.relfilenode` for it, its TOAST relation, and its indexes.
    ///
    /// All zeros for a supplier that keeps no files, and for a view — which is
    /// the right answer for a view either way. A **partitioned parent** is the
    /// one case where this is non-zero and `pg_class` must still report `0`: our
    /// engine gives one a heap file it never stores a row in, while PostgreSQL
    /// gives it no storage at all. See `pg_class_rows`.
    pub filenodes: RelationFilenodes,
}

/// The relkind of a stored user relation: a partitioned parent (carrying a
/// partition key) is `'p'`, everything else an ordinary table `'r'`. A leaf
/// partition is still an ordinary table (its `partition_of` only sets
/// `relispartition`).
fn table_kind(schema: &TableSchema) -> RelKind {
    if schema.partition_scheme.is_some() {
        RelKind::PartitionedTable
    } else {
        RelKind::Table
    }
}

impl CatalogRelation {
    pub fn permanent(schema: TableSchema) -> Self {
        let namespace = schema.namespace.clone();
        let kind = table_kind(&schema);
        let stats = RelStats::unknown(&schema);
        Self {
            schema,
            indexes: Vec::new(),
            namespace,
            temporary: false,
            kind,
            sequence: None,
            stats,
            toast: None,
            definition: None,
            ddl_xid: 0,
            filenodes: RelationFilenodes::default(),
        }
    }

    pub fn permanent_metadata(metadata: RelationMetadata) -> Self {
        let namespace = metadata.schema.namespace.clone();
        let kind = table_kind(&metadata.schema);
        Self {
            schema: metadata.schema,
            indexes: metadata.indexes,
            namespace,
            temporary: false,
            kind,
            sequence: None,
            stats: metadata.stats,
            toast: metadata.toast,
            definition: None,
            ddl_xid: 0,
            filenodes: metadata.filenodes,
        }
    }

    pub fn temporary(schema: TableSchema, namespace: impl Into<String>) -> Self {
        let kind = table_kind(&schema);
        let stats = RelStats::unknown(&schema);
        Self {
            schema,
            indexes: Vec::new(),
            namespace: namespace.into(),
            temporary: true,
            kind,
            sequence: None,
            stats,
            toast: None,
            definition: None,
            ddl_xid: 0,
            filenodes: RelationFilenodes::default(),
        }
    }

    /// A permanent view. Views have no indexes; its namespace rides on `schema`.
    ///
    /// See [`CatalogRelation::definition`] for what `definition` must hold.
    pub fn view(schema: TableSchema, definition: Option<String>) -> Self {
        let namespace = schema.namespace.clone();
        let stats = RelStats::unknown(&schema);
        Self {
            schema,
            indexes: Vec::new(),
            namespace,
            temporary: false,
            kind: RelKind::View,
            sequence: None,
            stats,
            toast: None,
            definition,
            ddl_xid: 0,
            filenodes: RelationFilenodes::default(),
        }
    }

    /// A permanent sequence in `namespace`. Its `pg_class` shape is PG's three
    /// sequence columns (`last_value`, `log_cnt`, `is_called`); `params` feeds
    /// `pg_sequence`.
    pub fn sequence(
        name: impl Into<String>,
        namespace: impl Into<String>,
        params: CatalogSequence,
    ) -> Self {
        let namespace = namespace.into();
        let schema = TableSchema::in_namespace(
            name,
            namespace.clone(),
            vec![
                Column::new("last_value", PgType::Int8),
                Column::new("log_cnt", PgType::Int8),
                Column::new("is_called", PgType::Bool),
            ],
        );
        let stats = RelStats::unknown(&schema);
        Self {
            schema,
            indexes: Vec::new(),
            namespace,
            temporary: false,
            kind: RelKind::Sequence,
            sequence: Some(params),
            stats,
            toast: None,
            definition: None,
            ddl_xid: 0,
            filenodes: RelationFilenodes::default(),
        }
    }
}

/// What a view's stored query reads, for the `pg_depend` edges of its `_RETURN`
/// rule.
///
/// Supplied by the server rather than derived here: recovering it means binding
/// the view's SQL, and this crate depends on neither the parser nor the binder —
/// the same division [`CatalogRelation::definition`] draws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogViewDependency {
    pub namespace: String,
    pub name: String,
    pub reads: Vec<ViewDepRelation>,
}

/// One relation a view reads, and how precisely the read is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewDepRelation {
    pub namespace: String,
    pub name: String,
    /// The columns read, by name, or `None` for "the relation as a whole"
    /// (`refobjsubid = 0`).
    ///
    /// PostgreSQL stores the coarse form only for a query that names a relation
    /// without reading a column of it; here it also stands for a read this
    /// build cannot resolve to columns — another view, or a relation reached
    /// only from an expression subquery. Coarse under-reports what depends on a
    /// *column*, which is the direction that costs a client a false "nothing
    /// depends on it".
    pub columns: Option<Vec<String>>,
}

/// A user-defined type reflected into `pg_type` (and, for enums, `pg_enum`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogUserType {
    pub oid: u32,
    pub name: String,
    /// The enum labels in definition (= sort) order, or `None` for a non-enum
    /// user type.
    ///
    /// TODO: reflect non-enum `CREATE TYPE` shapes into `pg_type` — only rows
    /// with labels here are emitted (`typtype = 'e'`), so any other user type
    /// is invisible to a client reading the catalog.
    pub enum_labels: Option<Vec<String>>,
}

/// A user-defined routine reflected into `pg_proc`. Built by whoever owns the
/// function catalog, so this crate stays free of server types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogRoutine {
    pub oid: u32,
    pub name: String,
    pub namespace: String,
    /// `pg_proc.prokind`: `'f'` function, `'p'` procedure.
    pub kind: char,
    /// `pg_proc.prolang` — a `pg_language` OID.
    pub lang: u32,
    /// Input-argument type OIDs, in signature order (`proargtypes`). A `0`
    /// stands for a type that does not resolve, as PostgreSQL reports for one
    /// it cannot name.
    pub arg_types: Vec<u32>,
    /// Every argument including OUT/INOUT, or empty when they are all IN —
    /// PostgreSQL leaves `proallargtypes` NULL in that case.
    pub all_arg_types: Vec<u32>,
    /// One of `i`/`o`/`b` per entry of `all_arg_types`, or empty.
    pub arg_modes: Vec<char>,
    /// Argument names, or empty when no argument is named.
    pub arg_names: Vec<String>,
    pub ret_type: u32,
    pub retset: bool,
    /// `pg_proc.provolatile`: `i`/`s`/`v`.
    pub volatile: char,
    pub strict: bool,
    pub secdef: bool,
    /// `pg_proc.prosrc` — the body as written.
    pub src: String,
}

/// The live server state one [`crate::SystemCatalog`] snapshot reflects.
///
/// Every method that enumerates state is called **at most once** per snapshot,
/// and only when a query actually opens the relation it feeds —
/// `SystemCatalog` owns that memoization, so an implementation is free to be
/// expensive: `relations()` enumerates the whole database, and a `SELECT 1`
/// must never pay for it. The cheap answers — `database`, `owner`, `now` and
/// `bytea_output` — are read straight through with no memoization, and
/// `bytea_output` is asked once per row that renders a partition bound, so
/// they must stay trivial.
///
/// `relations`, `database` and `owner` have no default on purpose. A silent
/// `Vec::new()` for the first would empty `pg_class`, `pg_attribute` and the
/// information schema at once, and a `"postgres"` default for the other two
/// would label every catalog row with a session identity nobody chose. The
/// optional methods are ones whose empty answer is a truthful "this deployment
/// has none".
pub trait CatalogSource: Send + Sync {
    /// The live user relations to reflect into `pg_class`/`pg_attribute` and
    /// `information_schema`.
    fn relations(&self) -> Vec<CatalogRelation>;

    /// The database this snapshot's session is connected to.
    fn database(&self) -> &str;

    /// The session user. Every catalog row reports it as its owner, and it is
    /// the one name [`crate::SystemCatalog::role_name`] resolves.
    fn owner(&self) -> &str;

    /// The user-defined types to reflect into `pg_type`/`pg_enum`.
    fn user_types(&self) -> Vec<CatalogUserType> {
        Vec::new()
    }

    /// The user-defined routines to reflect into `pg_proc`.
    fn routines(&self) -> Vec<CatalogRoutine> {
        Vec::new()
    }

    /// What each view's query reads, for `pg_depend`.
    ///
    /// Asked for by `pg_depend` alone, which is why that relation is registered
    /// as deferred: answering this binds every stored view, and a `SELECT` that
    /// merely touches `pg_class` must not pay for it.
    fn view_dependencies(&self) -> Vec<CatalogViewDependency> {
        Vec::new()
    }

    /// The user-created schemas (`CREATE SCHEMA`) as `(name, oid)`, to reflect
    /// into `pg_namespace` and `information_schema.schemata`.
    fn schemas(&self) -> Vec<(String, u32)> {
        Vec::new()
    }

    /// The session's open SQL cursors, to reflect into `pg_cursors`.
    fn cursors(&self) -> Vec<CatalogCursor> {
        Vec::new()
    }

    /// The session's prepared statements, to reflect into
    /// `pg_prepared_statements`.
    fn prepared_statements(&self) -> Vec<CatalogPreparedStatement> {
        Vec::new()
    }

    /// The locks to reflect into `pg_locks`.
    ///
    /// PostgreSQL answers this from the cluster-wide lock table, so a backend
    /// sees every other backend's locks. There is no such table here: a
    /// relation's lock (`crabgresql_txn::TableLock`) lives inside the access
    /// method's open handle, reachable only through that table, and nothing
    /// enumerates the holders. So a session reports the locks *it* holds and
    /// this relation shows one session's share of what PostgreSQL would list.
    ///
    /// TODO: register the per-relation locks in one server-visible table so
    /// this can report other sessions' holds — the same missing lock API the
    /// `ALTER TABLE ADD CONSTRAINT` note in the server's `query.rs` needs.
    fn locks(&self) -> Vec<CatalogLock> {
        Vec::new()
    }

    /// The database-wide counters, for `pg_stat_database`.
    ///
    /// The default renders as a row of zeros and a NULL `stats_reset` — what
    /// PostgreSQL shows for counters nothing has touched.
    fn database_stats(&self) -> DbStatSnapshot {
        DbStatSnapshot::default()
    }

    /// The per-relation counters, for `pg_stat_all_tables`. Named rather than
    /// numbered, and resolved by the reading snapshot — exactly as
    /// [`CatalogLockTarget::Relation`] is, and for the same reason.
    fn table_stats(&self) -> Vec<RelStatSnapshot> {
        Vec::new()
    }

    /// The per-index counters, for `pg_stat_all_indexes`.
    fn index_stats(&self) -> Vec<IndexStatSnapshot> {
        Vec::new()
    }

    /// The backends, for `pg_stat_activity`.
    ///
    /// PostgreSQL answers from shared memory, so one backend sees every other.
    /// Nothing enumerates the live connections here — the gap
    /// [`CatalogSource::locks`] documents, and the one that leaves
    /// `CancelRequest` unanswered — so a session reports itself and the view
    /// shows one row where PostgreSQL shows the cluster.
    ///
    /// TODO: register every connection in one server-wide table, so this can
    /// report the other backends and a cancel request can find its session.
    fn backends(&self) -> Vec<CatalogBackend> {
        Vec::new()
    }

    /// The configuration parameters, to reflect into `pg_settings`. The GUC
    /// table lives in the server, so this crate takes the rendered rows rather
    /// than depending on it.
    fn settings(&self) -> Vec<CatalogSetting> {
        Vec::new()
    }

    /// This connection's backend id; see [`CatalogLock::pid`]. `0` for a
    /// snapshot with no session behind it — a value PostgreSQL's own `pg_locks`
    /// never prints, so it cannot be mistaken for a live backend.
    fn backend_pid(&self) -> i32 {
        0
    }

    /// The transaction id every row of this snapshot reports as its `xmin`:
    /// the one that ran the most recent DDL.
    ///
    /// Catalog rows are derived from live server state per statement and carry
    /// no version history, so there is no per-row xid to report. What a client
    /// reads `xmin` off a catalog relation *for* is a state number — DataGrip
    /// compares `age(xmin)` against a threshold to decide whether its cached
    /// schema is stale — and the DDL generation answers that question exactly:
    /// it moves when the schema moves and stands still otherwise.
    ///
    /// `0` (`InvalidTransactionId`) for a snapshot with no server behind it.
    fn catalog_xmin(&self) -> u64 {
        0
    }

    /// The instant `pg_timezone_names`/`pg_timezone_abbrevs` resolve their
    /// offsets at, in `timestamptz` micros. PostgreSQL reports a zone's offset
    /// and DST flag as of *now*, so a session supplies its transaction
    /// timestamp — `now()`, not `statement_timestamp()` — and the view agrees
    /// with `now()` for every statement in the block.
    fn now(&self) -> i64 {
        crabgresql_types::tz::now_micros()
    }

    /// The reading session's `bytea_output`, for the one catalog column that
    /// renders a `bytea` datum: `pg_class.relpartbound`.
    ///
    /// A partition bound is stored already deparsed and `pg_get_expr` only
    /// echoes it (see `deparse_partbound`), so unlike a column default it has no
    /// read-time re-render hook to hang the GUC on — the reader's setting has to
    /// reach the row builder itself. Defaults to PG's boot value, which is the
    /// answer for a catalog with no session behind it.
    fn bytea_output(&self) -> ByteaOutput {
        ByteaOutput::Hex
    }
}

/// A [`CatalogSource`] over data known up front. Clones its vectors on every
/// call, which is harmless under `SystemCatalog`'s memoization and is why this
/// is for tests, fixtures and empty catalogs rather than for a live server.
#[derive(Clone, Debug)]
pub struct StaticSource {
    relations: Vec<CatalogRelation>,
    database: String,
    owner: String,
    user_types: Vec<CatalogUserType>,
    routines: Vec<CatalogRoutine>,
    schemas: Vec<(String, u32)>,
    cursors: Vec<CatalogCursor>,
    prepared_statements: Vec<CatalogPreparedStatement>,
    settings: Vec<CatalogSetting>,
    locks: Vec<CatalogLock>,
    view_dependencies: Vec<CatalogViewDependency>,
}

impl Default for StaticSource {
    /// Written out rather than derived: `database` and `owner` default to
    /// `postgres`, not to the empty string.
    fn default() -> Self {
        Self {
            relations: Vec::new(),
            database: "postgres".to_string(),
            owner: "postgres".to_string(),
            user_types: Vec::new(),
            routines: Vec::new(),
            schemas: Vec::new(),
            cursors: Vec::new(),
            prepared_statements: Vec::new(),
            settings: Vec::new(),
            locks: Vec::new(),
            view_dependencies: Vec::new(),
        }
    }
}

impl StaticSource {
    pub fn new(relations: Vec<CatalogRelation>) -> Self {
        Self {
            relations,
            ..Self::default()
        }
    }

    pub fn database(mut self, database: impl Into<String>) -> Self {
        self.database = database.into();
        self
    }

    pub fn owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = owner.into();
        self
    }

    pub fn user_types(mut self, user_types: Vec<CatalogUserType>) -> Self {
        self.user_types = user_types;
        self
    }

    pub fn routines(mut self, routines: Vec<CatalogRoutine>) -> Self {
        self.routines = routines;
        self
    }

    pub fn schemas(mut self, schemas: Vec<(String, u32)>) -> Self {
        self.schemas = schemas;
        self
    }

    pub fn cursors(mut self, cursors: Vec<CatalogCursor>) -> Self {
        self.cursors = cursors;
        self
    }

    pub fn prepared_statements(mut self, prepared: Vec<CatalogPreparedStatement>) -> Self {
        self.prepared_statements = prepared;
        self
    }

    pub fn settings(mut self, settings: Vec<CatalogSetting>) -> Self {
        self.settings = settings;
        self
    }

    pub fn locks(mut self, locks: Vec<CatalogLock>) -> Self {
        self.locks = locks;
        self
    }

    pub fn view_dependencies(mut self, deps: Vec<CatalogViewDependency>) -> Self {
        self.view_dependencies = deps;
        self
    }
}

impl CatalogSource for StaticSource {
    fn relations(&self) -> Vec<CatalogRelation> {
        self.relations.clone()
    }

    fn database(&self) -> &str {
        &self.database
    }

    fn owner(&self) -> &str {
        &self.owner
    }

    fn user_types(&self) -> Vec<CatalogUserType> {
        self.user_types.clone()
    }

    fn routines(&self) -> Vec<CatalogRoutine> {
        self.routines.clone()
    }

    fn schemas(&self) -> Vec<(String, u32)> {
        self.schemas.clone()
    }

    fn cursors(&self) -> Vec<CatalogCursor> {
        self.cursors.clone()
    }

    fn prepared_statements(&self) -> Vec<CatalogPreparedStatement> {
        self.prepared_statements.clone()
    }

    fn settings(&self) -> Vec<CatalogSetting> {
        self.settings.clone()
    }

    fn locks(&self) -> Vec<CatalogLock> {
        self.locks.clone()
    }

    fn view_dependencies(&self) -> Vec<CatalogViewDependency> {
        self.view_dependencies.clone()
    }
}

/// One configuration parameter, as `pg_settings` shows it. Session-local like
/// [`CatalogCursor`]: `setting` and `source` differ per connection.
///
/// Column-for-column with PostgreSQL's view, minus `sourcefile`, `sourceline`
/// and `pending_restart` — crabgresql has no configuration file and nothing
/// that needs a restart, so those three are constants the row builder supplies
/// rather than fields every caller would fill identically.
///
/// The metadata borrows rather than owning: it comes straight off the server's
/// `static GUCS` table, so `&'static str` is both free and a guarantee that
/// `pg_settings` shows the same strings `SHOW` does. Only `setting` and
/// `reset_val` are session-derived.
#[derive(Clone, Debug)]
pub struct CatalogSetting {
    /// Canonical spelling (`TimeZone`), not the lower-cased lookup key.
    pub name: &'static str,
    /// The current value, rendered exactly as `SHOW` prints it.
    pub setting: String,
    pub unit: Option<&'static str>,
    pub category: &'static str,
    pub short_desc: &'static str,
    pub extra_desc: Option<&'static str>,
    pub context: &'static str,
    pub vartype: &'static str,
    /// `default` or `session`.
    pub source: &'static str,
    pub min_val: Option<&'static str>,
    pub max_val: Option<&'static str>,
    /// `None` unless `vartype` is `enum`, in PostgreSQL's declaration order.
    pub enumvals: Option<&'static [&'static str]>,
    pub boot_val: &'static str,
    /// What `RESET <name>` would leave behind.
    pub reset_val: String,
}

/// One open cursor, as `pg_cursors` shows it. Cursors are session-local in
/// PostgreSQL too, so this is never shared between connections.
#[derive(Clone, Debug)]
pub struct CatalogCursor {
    pub name: String,
    /// The `DECLARE` statement text.
    pub statement: String,
    pub is_holdable: bool,
    pub is_binary: bool,
    pub is_scrollable: bool,
    /// When the cursor was declared, in `timestamptz` micros.
    pub creation_time: i64,
}

/// What a [`CatalogLock`] is a lock *on* — `pg_locks.locktype`, and with it
/// which of the relation/virtualxid/transactionid identity columns is filled.
///
/// Only the three kinds this build can hold are modelled. PostgreSQL's other
/// `locktype`s (`page`, `tuple`, `object`, `userlock`, `advisory`,
/// `applytransaction`) name lock levels no code here takes, so a row of that
/// kind would be an invented one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogLockTarget {
    /// A whole relation, named rather than numbered: relation OIDs are assigned
    /// by the snapshot itself (see [`crate::SystemCatalog::relation_oid_in`]),
    /// so the session that reports the lock has no OID to hand over and the row
    /// builder resolves the name against the same snapshot it is rendering.
    Relation { namespace: String, name: String },
    /// The session's own virtual transaction, held for the transaction's life.
    /// The `virtualxid` column repeats `virtualtransaction`.
    VirtualXid,
    /// A real, assigned XID. Only a transaction that has written holds one —
    /// this build allocates an XID lazily, exactly as PostgreSQL does.
    TransactionId(u32),
}

/// One row of `pg_locks`: a lock a session holds (or waits for) right now.
///
/// Built by the session rather than by a lock manager — see
/// [`CatalogSource::locks`] for what that means for the rows a client sees.
#[derive(Clone, Debug)]
pub struct CatalogLock {
    pub target: CatalogLockTarget,
    /// `pg_locks.virtualtransaction`, PostgreSQL's `backendID/localXID`
    /// spelling. Identifies the holder even when no real XID exists.
    pub virtualtransaction: String,
    /// `pg_locks.pid`: the holder's backend. This build serves every session
    /// from one OS process, so it reports the connection's backend id — the
    /// same integer the client was handed in `BackendKeyData`, which is what a
    /// client uses the column for.
    pub pid: i32,
    pub mode: &'static str,
    pub granted: bool,
    /// `pg_locks.fastpath`: whether PostgreSQL would have taken this one
    /// through the per-backend fast path (a weak relation lock with no
    /// conflicting holder) rather than the shared lock table.
    pub fastpath: bool,
    /// When the wait for an ungranted lock began, in `timestamptz` micros.
    /// NULL for a granted lock, as in PostgreSQL.
    pub waitstart: Option<i64>,
}

/// One backend, as `pg_stat_activity` shows it. See [`CatalogSource::backends`]
/// for why a session can only describe itself.
///
/// Only the columns this build can answer are fields; `wait_event*`,
/// `query_id`, `leader_pid` and the client address are constants the row
/// builder supplies, since no session could fill them differently.
#[derive(Clone, Debug)]
pub struct CatalogBackend {
    /// The connection's backend id; see [`CatalogLock::pid`].
    pub pid: i32,
    /// Empty when the client named itself neither at startup nor with `SET`,
    /// as in PostgreSQL.
    pub application_name: String,
    /// `timestamptz` micros, as are the three stamps below.
    pub backend_start: i64,
    /// `None` outside an explicit block.
    pub xact_start: Option<i64>,
    pub query_start: i64,
    /// Equal to `query_start` for a backend running a query, which is the only
    /// state a session can observe itself in.
    pub state_change: i64,
    /// PostgreSQL's `state` string, always `active` here for the reason above.
    pub state: &'static str,
    pub query: String,
    /// `None` until the transaction writes and is assigned one.
    pub backend_xid: Option<u32>,
    /// The oldest XID this backend's snapshot still considers running.
    pub backend_xmin: Option<u32>,
}

/// One prepared statement, as `pg_prepared_statements` shows it. Session-local
/// in PostgreSQL too, and holding both spellings — the SQL `PREPARE` and the
/// extended protocol's `Parse`, told apart by `from_sql`.
#[derive(Clone, Debug)]
pub struct CatalogPreparedStatement {
    pub name: String,
    /// The statement text, as the session recorded it.
    pub statement: String,
    /// When the statement was prepared, in `timestamptz` micros.
    pub prepare_time: i64,
    /// Type OID per `$n`, rendered as `regtype[]`.
    pub parameter_types: Vec<u32>,
    /// Type OID per result column, or `None` for a statement that returns no
    /// rows — which PostgreSQL reports as NULL, not as an empty array.
    pub result_types: Option<Vec<u32>>,
    pub from_sql: bool,
    pub generic_plans: i64,
    pub custom_plans: i64,
}
