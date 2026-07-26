//! Storage engine API: the `TableEngine` / `TableAm` extension point.
//!
//! Every data method carries a [`TxnContext`]: the engine judges visibility
//! against the caller's snapshot and stamps writes with the caller's XID, so
//! **MVCC lives in the engine while snapshots and XIDs stay the core's job**
//! (docs/ARCHITECTURE.md §1.3). `crabgresql-pg-engine` (the durable heap engine,
//! with RAM-backed memory tables for the UNLOGGED/TEMP cases) is the
//! implementation of this contract — hence a real `(block, offset)` [`Tid`]
//! rather than an opaque scalar.

use std::sync::Arc;

use crabgresql_txn::{Clog, TxnContext, Xid};
use crabgresql_types::{PgType, Value};

pub use crabgresql_txn as txn;

mod stats;
pub use stats::{ColStats, RelStats};

/// A materialized row. Column order matches the table schema.
pub type Tuple = Vec<Value>;

/// Row identity — PostgreSQL's `ctid`: the physical `(block, offset)` address of
/// a tuple version. Stable for a version's lifetime and never reused while the
/// version lives. The heap engine fills both fields from the page it lands on;
/// the in-memory engine synthesizes them from a monotonic counter (`block` the
/// high bits, `offset` the low), so its tids are just as opaque but share the
/// type the heap engine needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tid {
    pub block: u32,
    pub offset: u16,
}

impl Tid {
    pub const fn new(block: u32, offset: u16) -> Self {
        Tid { block, offset }
    }

    /// A single monotonic key over `(block, offset)` — the sort/lookup order for
    /// engines that keep versions in one ordered vector.
    pub const fn packed(self) -> u64 {
        ((self.block as u64) << 16) | self.offset as u64
    }

    /// Inverse of [`Tid::packed`]: pack a monotonic counter into a `(block,
    /// offset)` pair (used by the in-memory engine).
    pub const fn from_packed(n: u64) -> Self {
        Tid {
            block: (n >> 16) as u32,
            offset: (n & 0xffff) as u16,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Column {
    pub name: String,
    pub ty: PgType,
    /// The declared type modifier (e.g. a `varchar(n)`/`char(n)` length), or
    /// `-1` when the type has none. Applied to values on INSERT/UPDATE.
    pub typmod: i32,
    /// Whether SQL NULL is accepted. A PRIMARY KEY also sets this to false.
    pub nullable: bool,
    /// The name of an explicit NOT NULL constraint. PRIMARY KEY-implied
    /// non-nullability has no separate entry here.
    pub not_null_constraint: Option<String>,
    /// Canonical SQL text of the column default. The binder reparses and binds
    /// it once per DML statement, then the executor evaluates it per row.
    pub default: Option<String>,
    /// The OID of an explicit `COLLATE` on the column, or `None` for the type's
    /// default collation. Only ever set on a collatable type
    /// (`PgType::is_collatable`); it decides how the column's values order in
    /// comparisons and `ORDER BY` unless a nearer `COLLATE` overrides it.
    pub collation: Option<u32>,
}

impl Column {
    /// A column with no type modifier.
    pub fn new(name: impl Into<String>, ty: PgType) -> Self {
        Column {
            name: name.into(),
            ty,
            typmod: -1,
            nullable: true,
            not_null_constraint: None,
            default: None,
            collation: None,
        }
    }

    /// A column carrying a declared type modifier.
    pub fn with_typmod(name: impl Into<String>, ty: PgType, typmod: i32) -> Self {
        Column {
            name: name.into(),
            ty,
            typmod,
            nullable: true,
            not_null_constraint: None,
            default: None,
            collation: None,
        }
    }
}

/// Access method recorded for a semantic index. Physical index access arrives
/// later; this metadata already reproduces DDL, catalogs, and uniqueness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexMethod {
    BTree,
    Hash,
}

/// A simple column key in an index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexKey {
    pub column: usize,
    pub descending: bool,
    pub nulls_first: bool,
}

/// A table constraint backed by an index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexConstraint {
    PrimaryKey,
    Unique,
}

/// Metadata for an index relation. Only simple column indexes are represented;
/// unsupported expression/partial/include forms are rejected by DDL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexMetadata {
    pub name: String,
    pub method: IndexMethod,
    pub keys: Vec<IndexKey>,
    pub unique: bool,
    /// `true` is PostgreSQL's default: a key containing NULL does not conflict.
    pub nulls_distinct: bool,
    pub constraint: Option<IndexConstraint>,
}

/// The partitioning strategy of a partitioned (parent) table. Only `Range` is
/// supported so far; `List`/`Hash` are rejected at DDL time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartitionStrategy {
    Range,
}

/// The partition key of a partitioned (parent) table. A relation carrying a
/// `PartitionScheme` is `relkind = 'p'` — metadata only, with no rows of its
/// own (rows live in its leaf partitions).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionScheme {
    pub strategy: PartitionStrategy,
    /// Column positions (into [`TableSchema::columns`]) forming the key.
    pub key_columns: Vec<usize>,
}

/// One datum of a RANGE partition bound. `MinValue`/`MaxValue` are the
/// unbounded ends (`MINVALUE`/`MAXVALUE`); `Value` holds the bound literal
/// already folded to the key column's type, so it compares directly against a
/// row's key value with no re-parse. (`Value` has no `Eq`, so neither do the
/// partition types below — `PartialEq` is all the callers need.)
#[derive(Clone, Debug, PartialEq)]
pub enum PartitionBoundDatum {
    Value(Value),
    MinValue,
    MaxValue,
}

/// A RANGE partition bound: `FOR VALUES FROM (from...) TO (to...)`. `from` is
/// inclusive, `to` is exclusive, each a tuple over the parent's key columns.
#[derive(Clone, Debug, PartialEq)]
pub struct PartitionBound {
    pub from: Vec<PartitionBoundDatum>,
    pub to: Vec<PartitionBoundDatum>,
}

/// Set on a leaf partition (`relispartition = true`): which parent it belongs
/// to and the bound that admits a row into it.
#[derive(Clone, Debug, PartialEq)]
pub struct PartitionOf {
    pub parent_namespace: String,
    pub parent_name: String,
    /// Column positions (into [`TableSchema::columns`]) forming the partition
    /// key — a copy of the parent's [`PartitionScheme::key_columns`]. Carried
    /// on the leaf so it can enforce its own bound without resolving the parent.
    pub key_columns: Vec<usize>,
    pub bound: PartitionBound,
}

/// A user relation together with its mutable index metadata and size estimates.
#[derive(Clone, Debug)]
pub struct RelationMetadata {
    pub schema: TableSchema,
    pub indexes: Vec<IndexMetadata>,
    /// What [`TableAm::statistics`] reported when this snapshot was taken —
    /// the source of `pg_class.relpages`/`reltuples`.
    pub stats: RelStats,
}

/// How a relation is stored, mirroring PostgreSQL's `pg_class.relpersistence`.
/// Three classes differing along three independent axes — storage, WAL, and
/// catalog durability:
///
/// | class | storage | WAL | catalog row | on crash |
/// |---|---|---|---|---|
/// | `Permanent` (`'p'`) | on-disk | logged | persisted | recovered from WAL |
/// | `Unlogged` (`'u'`) | on-disk | skipped | persisted | truncated to empty |
/// | `Temporary` (`'t'`) | RAM | skipped | not persisted | (session-scoped) |
///
/// `Unlogged` therefore behaves like `Permanent` except it skips the WAL and its
/// data (not its definition) is reset after a crash — as in PostgreSQL. Use the
/// axis-specific helpers below rather than testing the variant directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RelPersistence {
    #[default]
    Permanent,
    Unlogged,
    Temporary,
}

impl RelPersistence {
    /// Storage axis: whether the relation's pages live in RAM (the storage
    /// manager's memory store) rather than an on-disk file. Only `Temporary`.
    pub fn is_ram_backed(self) -> bool {
        matches!(self, RelPersistence::Temporary)
    }

    /// WAL axis: whether mutations skip the write-ahead log (`Unlogged` and
    /// `Temporary`). WAL-skipped pages evict straight to their backing store.
    pub fn is_wal_skipped(self) -> bool {
        matches!(self, RelPersistence::Unlogged | RelPersistence::Temporary)
    }

    /// Catalog axis: whether the relation's catalog row is written to the durable
    /// catalog file, so its definition survives a restart (`Permanent`,
    /// `Unlogged`). `Temporary` rows are never persisted.
    pub fn persists_catalog(self) -> bool {
        matches!(self, RelPersistence::Permanent | RelPersistence::Unlogged)
    }

    /// Whether this is specifically an `Unlogged` relation — on-disk but
    /// WAL-silent, and reset to empty after a crash.
    pub fn is_unlogged(self) -> bool {
        matches!(self, RelPersistence::Unlogged)
    }

    /// The `pg_class.relpersistence` character for this relation.
    pub fn as_char(self) -> char {
        match self {
            RelPersistence::Permanent => 'p',
            RelPersistence::Unlogged => 'u',
            RelPersistence::Temporary => 't',
        }
    }
}

#[derive(Clone, Debug)]
pub struct TableSchema {
    pub name: String,
    /// The schema (PostgreSQL namespace) this relation lives in. Unqualified
    /// user relations default to `public`; a schema-qualified `CREATE` sets it
    /// explicitly. `name` stays the bare `relname` (it appears verbatim in
    /// error text and `pg_class.relname`).
    pub namespace: String,
    pub columns: Vec<Column>,
    /// How the relation is stored: durable heap (`Permanent`) or a RAM-backed,
    /// WAL-skipping memory table (`Unlogged`/`Temporary`).
    pub persistence: RelPersistence,
    /// `Some` on a partitioned (parent) table: its partition key. Such a table
    /// is `relkind = 'p'` and holds no rows of its own.
    pub partition_scheme: Option<PartitionScheme>,
    /// `Some` on a leaf partition: the parent it attaches to and its bound.
    pub partition_of: Option<PartitionOf>,
}

impl TableSchema {
    /// A table schema in the `public` namespace — the common case.
    pub fn new(name: impl Into<String>, columns: Vec<Column>) -> Self {
        TableSchema {
            name: name.into(),
            namespace: "public".to_string(),
            columns,
            persistence: RelPersistence::Permanent,
            partition_scheme: None,
            partition_of: None,
        }
    }

    /// A table schema in an explicit namespace.
    pub fn in_namespace(
        name: impl Into<String>,
        namespace: impl Into<String>,
        columns: Vec<Column>,
    ) -> Self {
        TableSchema {
            name: name.into(),
            namespace: namespace.into(),
            columns,
            persistence: RelPersistence::Permanent,
            partition_scheme: None,
            partition_of: None,
        }
    }
}

/// A stored view: a named query plus the metadata needed to resolve, reflect,
/// and drop it. The query is carried as **SQL text** (re-parsed by the binder on
/// each reference) so this crate stays free of a parser dependency. `columns` is
/// the view's output shape, derived by binding the query at `CREATE VIEW` time;
/// `depends_on` lists the relation names the query references at the surface
/// (a view over another view names the *view*, not its base tables) so
/// `DROP ... CASCADE`/`RESTRICT` can walk the dependency graph.
#[derive(Clone, Debug)]
pub struct ViewDefinition {
    pub name: String,
    /// The schema (namespace) the view lives in; `public` for unqualified views.
    pub namespace: String,
    pub sql: String,
    pub columns: Vec<Column>,
    pub depends_on: Vec<String>,
}

impl TableSchema {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }
}

/// A stored sequence: the immutable definition (parameters set at `CREATE
/// SEQUENCE`) used for reflection and `DROP`. The mutable counter
/// (`last_value`/`is_called`) lives inside the engine and is advanced
/// **non-transactionally** — `nextval` is never rolled back — so it is not
/// carried here. `owned_by` names the table a `serial` column created this
/// sequence for, so `DROP TABLE` can auto-drop it (PG's `OWNED BY`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SequenceDefinition {
    pub name: String,
    /// The schema (namespace) the sequence lives in; `public` when unqualified.
    pub namespace: String,
    /// Backing integer type: `Int2`, `Int4`, or `Int8`.
    pub data_type: PgType,
    pub start: i64,
    pub increment: i64,
    pub min: i64,
    pub max: i64,
    pub cache: i64,
    pub cycle: bool,
    pub owned_by: Option<String>,
}

impl SequenceDefinition {
    /// Compute `nextval` from the current counter state (`last_value`,
    /// `is_called`). Both engines keep that pair and call this so the min/max/
    /// cycle arithmetic stays identical. On success the caller stores the
    /// returned value as the new `last_value` with `is_called = true`.
    pub fn next_value(&self, last_value: i64, is_called: bool) -> SequenceAdvance {
        // When the sequence has not been "called" yet, `nextval` returns the
        // current value unadvanced: `last_value` is seeded to `start` at
        // creation and set directly by `setval(x, false)`.
        if !is_called {
            return SequenceAdvance::Value(last_value);
        }
        let ascending = self.increment > 0;
        match last_value.checked_add(self.increment) {
            Some(next) if ascending && next <= self.max => SequenceAdvance::Value(next),
            Some(next) if !ascending && next >= self.min => SequenceAdvance::Value(next),
            // Out of range (or i64 wrap): cycle wraps to the far bound, otherwise
            // it is a hard overflow/underflow.
            _ if self.cycle => {
                SequenceAdvance::Value(if ascending { self.min } else { self.max })
            }
            _ if ascending => SequenceAdvance::Overflow,
            _ => SequenceAdvance::Underflow,
        }
    }
}

/// Outcome of [`TableEngine::sequence_nextval`] / [`TableEngine::sequence_setval`].
/// The server maps the failure variants to SQLSTATEs, keeping this crate free of
/// the wire-protocol dependency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SequenceAdvance {
    /// The new current value.
    Value(i64),
    /// No sequence by that name exists.
    NotFound,
    /// An ascending sequence hit `max` with `NO CYCLE`.
    Overflow,
    /// A descending sequence hit `min` with `NO CYCLE`.
    Underflow,
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("relation \"{0}\" already exists")]
    TableAlreadyExists(String),
    #[error("relation \"{0}\" does not exist")]
    TableNotFound(String),
    #[error("relation \"{0}\" already exists")]
    RelationAlreadyExists(String),
    #[error("index target relation \"{0}\" does not exist")]
    IndexTableNotFound(String),
    #[error("schema \"{0}\" already exists")]
    SchemaAlreadyExists(String),
    #[error("schema \"{0}\" does not exist")]
    SchemaNotFound(String),
}

/// Outcome of `TableAm::update`.
///
/// `Conflict` is the EvalPlanQual / serialization seam: the row the caller
/// meant to update was updated or deleted by another transaction that committed
/// after the caller's snapshot. `updater` is that transaction (whom to wait on
/// or abort against); `latest` is the newest live version's tid to re-read
/// under READ COMMITTED. The in-memory engine does not yet raise it — conflict
/// handling arrives with the isolation work (P6) — but the shape is fixed here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateResult {
    Updated,
    NotFound,
    Conflict { updater: Xid, latest: Option<Tid> },
}

/// Outcome of `TableAm::delete`, mirroring [`UpdateResult`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteResult {
    Deleted,
    NotFound,
    Conflict { updater: Xid, latest: Option<Tid> },
}

/// Table access: scans and modifications on one table, all judged against the
/// caller's [`TxnContext`].
pub trait TableAm: Send + Sync {
    fn schema(&self) -> &TableSchema;

    /// Semantic indexes currently attached to this table.
    fn indexes(&self) -> Vec<IndexMetadata> {
        Vec::new()
    }

    /// Size and distribution estimates for the planner, and the source of
    /// `pg_class.relpages`/`reltuples`. Engines should override this with
    /// whatever they can report cheaply — the planner may call it once per
    /// relation per statement, so it must not scan.
    ///
    /// The default is [`RelStats::unknown`]: an engine that cannot report a
    /// physical size reports nothing rather than a fabricated number.
    fn statistics(&self) -> RelStats {
        RelStats::unknown(self.schema())
    }

    /// Full scan yielding only the versions visible to `txn`'s snapshot. The
    /// iterator captures the snapshot up front, so a DML statement never
    /// re-visits rows it modified itself (the reader's own new versions carry
    /// the reader's command id and stay invisible to the same command).
    fn scan(&self, txn: &TxnContext) -> Box<dyn Iterator<Item = (Tid, Tuple)> + Send>;

    /// Fetch one version by tid if it is visible to `txn` — the re-read
    /// EvalPlanQual needs after a conflict, and a point lookup for indexes.
    fn fetch(&self, tid: Tid, txn: &TxnContext) -> Option<Tuple>;

    /// Whether the engine can physically serve an equality index scan on
    /// `index_name` — i.e. whether [`TableAm::index_lookup`] would return `Some`
    /// rather than fall back to a scan. The planner consults this so it only
    /// chooses an index scan the executor can actually perform, keeping `EXPLAIN`
    /// honest. The default is `false` (no physical index): the durable heap
    /// engine and system catalogs report no index-scan support, so their queries
    /// plan (and display) as sequential scans.
    fn supports_index_scan(&self, _index_name: &str) -> bool {
        false
    }

    /// Probe the physical index `index_name` for versions whose key equals
    /// `key` (one [`Value`] per index key column, in key order), yielding those
    /// visible to `txn`. Returns `None` when the engine has no physical index
    /// able to serve this probe (no such index, or a key type it cannot index) —
    /// the caller then falls back to a full [`TableAm::scan`], so an index scan
    /// stays correct on every engine. The default is `None`: only the in-memory
    /// reference engine builds a physical index today; the durable heap engine
    /// and read-only system catalogs inherit the fallback.
    fn index_lookup(
        &self,
        _index_name: &str,
        _key: &[Value],
        _txn: &TxnContext,
    ) -> Option<Box<dyn Iterator<Item = (Tid, Tuple)> + Send>> {
        None
    }

    /// Insert a new version stamped with `txn`'s XID. The tuple must have
    /// exactly `schema().columns.len()` values in schema order — executors index
    /// tuples by schema position and rely on this.
    fn insert(&self, tuple: Tuple, txn: &TxnContext) -> Tid;

    /// Replace the version identified by `tid`: the old version is marked
    /// deleted by `txn` and a new version holding `tuple` is inserted. The tuple
    /// contract matches [`TableAm::insert`].
    fn update(&self, tid: Tid, tuple: Tuple, txn: &TxnContext) -> UpdateResult;

    /// Mark the version identified by `tid` deleted by `txn`.
    fn delete(&self, tid: Tid, txn: &TxnContext) -> DeleteResult;

    /// Apply a batch of replacements, returning how many rows were found and
    /// updated (vanished tids are skipped, not counted). Engines should
    /// override this to apply the whole batch under one lock — per-row calls
    /// make a large UPDATE quadratic.
    fn update_many(&self, updates: Vec<(Tid, Tuple)>, txn: &TxnContext) -> u64 {
        let mut applied = 0;
        for (tid, tuple) in updates {
            if self.update(tid, tuple, txn) == UpdateResult::Updated {
                applied += 1;
            }
        }
        applied
    }

    /// Batch counterpart of [`TableAm::delete`], mirroring
    /// [`TableAm::update_many`].
    fn delete_many(&self, tids: Vec<Tid>, txn: &TxnContext) -> u64 {
        let mut applied = 0;
        for tid in tids {
            if self.delete(tid, txn) == DeleteResult::Deleted {
                applied += 1;
            }
        }
        applied
    }

    /// Remove every row (TRUNCATE). Row identity is not preserved: engines need
    /// not keep tids reusable after a truncate. The default scans and deletes;
    /// engines should override with a whole-table reset.
    fn truncate(&self, txn: &TxnContext) {
        let tids: Vec<Tid> = self.scan(txn).map(|(tid, _)| tid).collect();
        self.delete_many(tids, txn);
    }

    /// Reclaim versions dead to every transaction at or before `oldest`. A
    /// version is reclaimable only if its deleter **committed** — `clog` decides
    /// that; a version stamped by an aborted or in-flight deleter is still live.
    /// The default is a no-op: the in-memory engine keeps dead versions until it
    /// is asked to vacuum, and there is no background vacuum before M5.
    fn vacuum(&self, _oldest: Xid, _clog: &Clog) {}
}

/// Engine factory: `CREATE TABLE ... USING <engine>`.
///
/// ## Namespaces
///
/// Relation methods take an explicit `namespace: &str` (the schema); unqualified
/// callers pass `"public"`. `create_table`/`create_view`/`create_sequence` need
/// no separate argument because the target namespace rides on the definition
/// struct (`TableSchema::namespace`, `ViewDefinition::namespace`,
/// `SequenceDefinition::namespace`, defaulting to `public`).
///
/// [`TableEngine::open_table`] and [`TableEngine::resolve`] are deliberately
/// distinct, not a public/namespaced pair: `open_table` is the **unqualified
/// write-safe** lookup (a session overlay searches temp then the global engine,
/// never the read-only system catalogs), while `resolve` is the
/// **search-path-aware read** that also reaches `pg_catalog`/`information_schema`
/// and honors a `Some(schema)` qualifier.
pub trait TableEngine: Send + Sync {
    fn create_table(&self, schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError>;

    fn open_table(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError>;

    /// Flush all data and record a clean shutdown, so a durable engine does NOT
    /// reset its unlogged relations at the next startup. Called once on graceful
    /// server exit. The default is a no-op (engines with no durable state).
    fn shutdown(&self) {}

    /// Remove a table and all its data from `namespace`. `TableNotFound` if it
    /// doesn't exist.
    fn drop_table(&self, namespace: &str, name: &str) -> Result<(), StorageError>;

    /// Register a user schema (`CREATE SCHEMA`), returning the OID the engine
    /// allocated for it. The engine owns OID allocation so the durable engine
    /// can persist the counter. `SchemaAlreadyExists` on a name collision (the
    /// caller handles `IF NOT EXISTS`). The default rejects it — only an engine
    /// that keeps a schema registry overrides this.
    fn create_schema(&self, name: &str) -> Result<u32, StorageError> {
        Err(StorageError::SchemaNotFound(name.to_string()))
    }

    /// Remove a user schema. The caller has already verified it is empty (or is
    /// dropping its contents first, for CASCADE). `SchemaNotFound` if absent.
    fn drop_schema(&self, name: &str) -> Result<(), StorageError> {
        Err(StorageError::SchemaNotFound(name.to_string()))
    }

    /// Enumerate user-created schemas as `(name, oid)`. Built-in namespaces
    /// (`public`, `pg_catalog`, …) are not included. The default is empty.
    fn schemas(&self) -> Vec<(String, u32)> {
        Vec::new()
    }

    /// Whether a user schema by this name exists.
    fn schema_exists(&self, name: &str) -> bool {
        self.schemas().iter().any(|(n, _)| n == name)
    }

    /// Register a semantic index on a table in `namespace` after the caller has
    /// validated its keys and, for UNIQUE, the table's existing contents.
    fn create_index(
        &self,
        _namespace: &str,
        _table: &str,
        index: IndexMetadata,
    ) -> Result<(), StorageError> {
        Err(StorageError::RelationAlreadyExists(index.name))
    }

    /// Remove the index named `index_name` from `table` in `namespace`. The
    /// caller has already located the owning table (indexes name the index, not
    /// the table) and validated the drop. The default rejects it — only an
    /// engine that stores indexes overrides this.
    fn drop_index(
        &self,
        _namespace: &str,
        _table: &str,
        index_name: &str,
    ) -> Result<(), StorageError> {
        Err(StorageError::TableNotFound(index_name.to_string()))
    }

    /// Whether `index_name` is occupied in `namespace` (an index shares the
    /// relation namespace with tables/views/sequences). The `table` parameter
    /// matters for session overlays where temp and public may use the same name.
    fn index_name_exists(&self, namespace: &str, _table: &str, index_name: &str) -> bool {
        self.resolve(Some(namespace), index_name).is_ok()
            || self
                .relation_metadata()
                .iter()
                .filter(|relation| relation.schema.namespace == namespace)
                .any(|relation| relation.indexes.iter().any(|i| i.name == index_name))
    }

    /// Resolve a possibly schema-qualified relation. The default ignores any
    /// schema (`None` behaves like [`TableEngine::open_table`]; a `Some(_)`
    /// qualifier is unknown to a plain data engine, so it is not found) — only a
    /// schema-aware resolver (the server's session catalog) overrides this to
    /// route `pg_catalog.*` to the system catalog and honor the search path.
    fn resolve(&self, schema: Option<&str>, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        match schema {
            None => self.open_table(name),
            Some(_) => Err(StorageError::TableNotFound(name.to_string())),
        }
    }

    /// Enumerate the engine's user relations (name + schema) for catalog
    /// reflection (`pg_class`/`pg_attribute`). The default is empty; data
    /// engines that keep a relation registry override it.
    fn relations(&self) -> Vec<TableSchema> {
        Vec::new()
    }

    /// Names of relations in `namespace`, without cloning full schemas. The
    /// default derives from [`relations`](Self::relations); engines keyed by
    /// namespace override it to read just the names. Used for per-namespace
    /// teardown (dropping a session's temp tables at disconnect).
    fn relation_names_in(&self, namespace: &str) -> Vec<String> {
        self.relations()
            .into_iter()
            .filter(|s| s.namespace == namespace)
            .map(|s| s.name)
            .collect()
    }

    /// Measure a relation under `txn` and record the result, so later planning
    /// and `pg_class.relpages`/`reltuples` report it (`ANALYZE`).
    ///
    /// Non-transactional by design, matching PostgreSQL: the result stands even
    /// if `txn` later rolls back. That is safe because statistics never affect a
    /// query's result, only which correct plan is chosen.
    ///
    /// The default reports that the engine cannot analyze; only engines with
    /// real storage override it.
    fn analyze(
        &self,
        _namespace: &str,
        name: &str,
        _txn: &TxnContext,
    ) -> Result<(), StorageError> {
        Err(StorageError::TableNotFound(name.to_string()))
    }

    /// Enumerate user relations including live index metadata for catalog
    /// reflection. Engines with tables override this; the fallback preserves
    /// compatibility for read-only/system engines.
    fn relation_metadata(&self) -> Vec<RelationMetadata> {
        self.relations()
            .into_iter()
            .map(|schema| RelationMetadata {
                stats: RelStats::unknown(&schema),
                schema,
                indexes: Vec::new(),
            })
            .collect()
    }

    /// Register a view. The caller (server) has already bound the query, derived
    /// its columns, and computed `depends_on`. The default rejects it — only an
    /// engine that keeps a view registry (memory, heap) overrides this.
    fn create_view(&self, def: ViewDefinition) -> Result<(), StorageError> {
        Err(StorageError::TableNotFound(def.name))
    }

    /// Resolve a possibly schema-qualified view name to its stored definition,
    /// for the binder to expand as a subplan. `None` means "not a view here" —
    /// the caller falls back to table resolution. The default knows no views.
    fn resolve_view(&self, _schema: Option<&str>, _name: &str) -> Option<ViewDefinition> {
        None
    }

    /// Remove a view from `namespace`. `TableNotFound` if absent. The default
    /// knows no views.
    fn drop_view(&self, _namespace: &str, name: &str) -> Result<(), StorageError> {
        Err(StorageError::TableNotFound(name.to_string()))
    }

    /// Enumerate the engine's views for catalog reflection and dependency
    /// (CASCADE/RESTRICT) checks. The default is empty.
    fn views(&self) -> Vec<ViewDefinition> {
        Vec::new()
    }

    /// Register a sequence. `TableAlreadyExists` on a name collision (the caller
    /// handles `IF NOT EXISTS`). The default rejects it — only an engine that
    /// keeps a sequence registry (memory, heap) overrides this.
    fn create_sequence(&self, def: SequenceDefinition) -> Result<(), StorageError> {
        Err(StorageError::TableNotFound(def.name))
    }

    /// Remove a sequence from `namespace`. `TableNotFound` if absent. The default
    /// knows none.
    fn drop_sequence(&self, _namespace: &str, name: &str) -> Result<(), StorageError> {
        Err(StorageError::TableNotFound(name.to_string()))
    }

    /// The immutable definition of a sequence in `namespace`, or `None` if there
    /// is none by that name. The default knows no sequences.
    fn sequence(&self, _namespace: &str, _name: &str) -> Option<SequenceDefinition> {
        None
    }

    /// Enumerate the engine's sequences for catalog reflection and owned-drop.
    /// The default is empty.
    fn sequences(&self) -> Vec<SequenceDefinition> {
        Vec::new()
    }

    /// Advance a sequence and return its new value (`nextval`). This mutates
    /// **non-transactional** counter state and, on the durable engine, persists
    /// immediately — it is not tied to the caller's transaction and survives
    /// `ROLLBACK`. The default knows no sequences.
    fn sequence_nextval(&self, _namespace: &str, _name: &str) -> SequenceAdvance {
        SequenceAdvance::NotFound
    }

    /// Set a sequence's current value (`setval`). With `is_called = true` the
    /// next `nextval` returns `value + increment`; with `false` it returns
    /// `value`. Non-transactional, like [`TableEngine::sequence_nextval`].
    fn sequence_setval(
        &self,
        _namespace: &str,
        _name: &str,
        _value: i64,
        _is_called: bool,
    ) -> SequenceAdvance {
        SequenceAdvance::NotFound
    }
}

/// A resolved `CREATE TYPE`: its OID and the builtin representation its values
/// are physically stored as (from a `LIKE` clause), or `None` when the type
/// declared only an `INTERNALLENGTH` and has no known backing builtin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserType {
    pub oid: u32,
    pub backing: Option<PgType>,
}

/// The labels of a `CREATE TYPE ... AS ENUM`, in definition (= sort) order, plus
/// the type's own name — enough for the binder to turn a text literal into a
/// [`crabgresql_types::Value::Enum`] and to render the `invalid input value for
/// enum <name>` error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumInfo {
    pub name: String,
    pub labels: Vec<String>,
}

/// A registered `CREATE FUNCTION ... LANGUAGE SQL`, as the binder needs it to
/// resolve and inline a call: the declared argument types, the declared return
/// type, and the body as SQL **text** (a `SELECT <expr>` the binder re-parses on
/// each call, keeping this crate free of a parser/binder dependency, exactly as
/// [`ViewDefinition`] carries a view's query text).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlFunctionSig {
    pub arg_types: Vec<PgType>,
    pub return_type: PgType,
    pub body: String,
}

/// A registered `CREATE CAST (source AS target)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserCast {
    /// `WITHOUT FUNCTION` — a binary-coercible cast that reinterprets the value's
    /// bit pattern rather than running a conversion function.
    pub without_function: bool,
}

/// Read-only view of the server's user-defined types and casts, so the query
/// binder can resolve a `CREATE TYPE` name in an expression cast and apply a
/// user-defined cast. The DDL layer (`GlobalCatalog`) implements this; callers
/// with no user types (binder unit tests) use [`EmptyTypeCatalog`].
pub trait TypeCatalog: Send + Sync {
    /// Resolve a fully-defined `CREATE TYPE` name (case-insensitive) to its OID
    /// and backing. Shell types are deliberately excluded from query-time type
    /// resolution.
    fn resolve_type(&self, name: &str) -> Option<UserType>;

    /// Whether `name` exists only as a `CREATE TYPE name;` shell. DDL can refer
    /// to shells while table columns and expressions must reject them.
    fn is_shell_type(&self, _name: &str) -> bool {
        false
    }

    /// The catalog name for a user type OID, for PG-compatible diagnostics.
    fn user_type_name(&self, _oid: u32) -> Option<String> {
        None
    }

    /// The `CREATE CAST (source AS target)` for this ordered pair, if one was
    /// registered. `source`/`target` are either builtins or `PgType::User(oid)`.
    fn find_cast(&self, source: PgType, target: PgType) -> Option<UserCast>;

    /// The backing builtin representation of a type: the type itself for a
    /// builtin, or a user type's `LIKE` rep. Falls back to `ty` when unknown.
    fn backing_rep(&self, ty: PgType) -> PgType;

    /// The labels of the enum type with this OID, or `None` if the OID is not a
    /// `CREATE TYPE ... AS ENUM`. `Some(..)` is the binder's "is this an enum?"
    /// test for a `PgType::User(oid)`.
    fn enum_info(&self, _oid: u32) -> Option<EnumInfo> {
        None
    }

    /// Every `LANGUAGE SQL` function registered under `name` (case-insensitive),
    /// one [`SqlFunctionSig`] per overload. The binder consults this when no
    /// built-in matches a call, then re-parses and inlines the chosen body.
    /// Callers with no user functions (binder unit tests) get the empty default.
    fn sql_functions(&self, _name: &str) -> Vec<SqlFunctionSig> {
        Vec::new()
    }
}

/// A [`TypeCatalog`] with no user-defined types or casts — the default for
/// callers (and binder unit tests) that never register any.
pub struct EmptyTypeCatalog;

impl TypeCatalog for EmptyTypeCatalog {
    fn resolve_type(&self, _name: &str) -> Option<UserType> {
        None
    }

    fn find_cast(&self, _source: PgType, _target: PgType) -> Option<UserCast> {
        None
    }

    fn backing_rep(&self, ty: PgType) -> PgType {
        ty
    }
}
