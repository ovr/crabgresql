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

/// A materialized row. Always as wide as the table schema, with column order
/// matching it. A scan restricted by a [`ColumnProjection`] still returns a
/// full-width tuple; only the values at unselected positions are unspecified.
pub type Tuple = Vec<Value>;

/// Row identity — PostgreSQL's `ctid`: the physical `(block, offset)` address of
/// a tuple version. Stable for a version's lifetime and never reused while the
/// version lives. The heap engine fills both fields from the page it lands on;
/// the in-memory engine synthesizes them from a monotonic counter (`block` the
/// high bits, `offset` the low), so its tids are just as opaque but share the
/// type the heap engine needs.
///
/// The top bit of `block` is reserved: see [`TID_LOGICAL_FLAG`]. Clear, the tid
/// is a physical address as above; set, it is a logical row id
/// ([`Tid::logical`]) for an access method that rewrites storage underneath its
/// rows. One relation may hand out both kinds, and [`TableAm::fetch`] routes on
/// [`Tid::is_logical`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tid {
    pub block: u32,
    pub offset: u16,
}

/// Set in [`Tid::block`], this marks a tid as a **logical row id** — an
/// identity that survives the row being physically rewritten — rather than a
/// physical `(block, offset)` locator.
///
/// An access method that rewrites storage underneath a row (a columnar store
/// compacting files, or a write buffer flushing to one) cannot use a physical
/// address as identity: the address changes while the row does not. Reserving
/// the top bit splits the space in two so one relation can hand out both kinds
/// and route a `fetch` by inspecting the tid alone, with no side table.
///
/// The flag means "addressed by identity", not "resident in RAM": a row keeps
/// its logical tid after it is written to durable storage.
pub const TID_LOGICAL_FLAG: u32 = 0x8000_0000;

/// The largest physical block number an access method may use, now that
/// [`TID_LOGICAL_FLAG`] owns the top bit. Engines whose blocks count up from 0
/// are unaffected in practice; the limit exists so an overflow is an error
/// rather than a tid that silently reads as logical.
pub const MAX_PHYSICAL_BLOCK: u32 = TID_LOGICAL_FLAG - 1;

/// The largest logical row id representable in a [`Tid`] — 47 bits, the 31 left
/// in `block` plus the 16 in `offset`.
pub const MAX_ROW_ID: u64 = (1 << 47) - 1;

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

    /// The tid naming logical row `row_id`. Panics in debug on overflow past
    /// [`MAX_ROW_ID`]; callers mint ids from a counter they also bound.
    pub const fn logical(row_id: u64) -> Self {
        debug_assert!(row_id <= MAX_ROW_ID, "logical row id overflows a tid");
        Tid {
            block: TID_LOGICAL_FLAG | (row_id >> 16) as u32,
            offset: (row_id & 0xffff) as u16,
        }
    }

    /// Whether this tid names a logical row id rather than a physical address.
    pub const fn is_logical(self) -> bool {
        self.block & TID_LOGICAL_FLAG != 0
    }

    /// The logical row id this tid names, or `None` if it is a physical locator.
    pub const fn row_id(self) -> Option<u64> {
        if self.is_logical() {
            Some((((self.block & !TID_LOGICAL_FLAG) as u64) << 16) | self.offset as u64)
        } else {
            None
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

/// Source of fresh, never-reused relfilenodes — the engine's catalog counter.
///
/// A table access method that swaps its physical relation on TRUNCATE needs an id
/// that can never alias another relation's file or directory, so it must draw from
/// the *same* counter as every other relation rather than keep a private one. The
/// engine owns that counter; an out-of-crate access method (Parquet) reaches it
/// through this trait.
pub trait RelfilenodeAllocator: Send + Sync {
    /// Issue a relfilenode no relation has ever used. Does not persist the
    /// catalog: the id becomes durable only when the swap that staged it commits,
    /// and recovery re-observes ids from the WAL before issuing new ones.
    fn alloc_relfilenode(&self) -> u32;
}

/// Physical table access method selected by `CREATE TABLE ... USING`.
///
/// This is persisted as part of [`TableSchema`]. Existing catalogs that predate
/// access-method persistence decode as [`TableAccessMethod::Heap`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TableAccessMethod {
    #[default]
    Heap,
    Parquet,
    /// A WAL-logged, RAM-resident row store. Durable like the heap (the commit
    /// record covers its rows) but with no file of its own, so its size is
    /// bounded by memory. Distinct from an `UNLOGGED`/`TEMP` heap table, which is
    /// also RAM-resident but deliberately skips the WAL.
    Buffer,
}

impl TableAccessMethod {
    /// The `pg_am.amname` this method is known by — the spelling accepted by
    /// `CREATE TABLE ... USING` and reported back in error text. Single source of
    /// truth so a message can never name a method other than the table's own.
    pub fn as_str(self) -> &'static str {
        match self {
            TableAccessMethod::Heap => "heap",
            TableAccessMethod::Parquet => "parquet",
            TableAccessMethod::Buffer => "buffer",
        }
    }

    /// Resolve an `amname` written by the user. `None` is the 42704 case.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "heap" => Some(TableAccessMethod::Heap),
            "parquet" => Some(TableAccessMethod::Parquet),
            "buffer" => Some(TableAccessMethod::Buffer),
            _ => None,
        }
    }

    /// Whether the engine, rather than the buffer pool, owns this method's
    /// storage.
    ///
    /// These methods have no unlogged or temporary form (they define their own
    /// relationship to the WAL) and no partition routing. One predicate keeps the
    /// CREATE TABLE, CTAS, and engine-side guards from drifting apart as methods
    /// are added.
    pub fn is_engine_managed(self) -> bool {
        matches!(
            self,
            TableAccessMethod::Parquet | TableAccessMethod::Buffer
        )
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
    /// The physical access method. Plain `CREATE TABLE` uses `Heap`; an explicit
    /// `USING parquet` selects the append-only Parquet implementation.
    pub access_method: TableAccessMethod,
    /// `Some` on a partitioned (parent) table: its partition key. Such a table
    /// is `relkind = 'p'` and holds no rows of its own.
    pub partition_scheme: Option<PartitionScheme>,
    /// `Some` on a leaf partition: the parent it attaches to and its bound.
    pub partition_of: Option<PartitionOf>,
    /// The layout sort key: the order an engine-managed access method stores
    /// rows in, from `ORDER BY (...)` or defaulted to the `PRIMARY KEY`. A heap
    /// relation is always empty — `ORDER BY` on one is rejected at DDL time —
    /// and so is a relation created before the key was recorded.
    ///
    /// Declaring it is not the same as honoring it: nothing sorts on this yet.
    /// The sorted flush is `ROADMAP.md`'s Parquet step 3, which owns it together
    /// with the V2 fragment footer. A standalone `USING buffer` relation never
    /// will — it has nowhere to flush — and carries a key only so both
    /// engine-managed methods answer the same DDL alike.
    pub sort_key: Vec<IndexKey>,
}

impl TableSchema {
    /// A table schema in the `public` namespace — the common case.
    pub fn new(name: impl Into<String>, columns: Vec<Column>) -> Self {
        TableSchema {
            name: name.into(),
            namespace: "public".to_string(),
            columns,
            persistence: RelPersistence::Permanent,
            access_method: TableAccessMethod::Heap,
            partition_scheme: None,
            partition_of: None,
            sort_key: Vec::new(),
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
            access_method: TableAccessMethod::Heap,
            partition_scheme: None,
            partition_of: None,
            sort_key: Vec::new(),
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
    #[error("{0}")]
    UnsupportedOperation(String),
    #[error("{0}")]
    UnsupportedType(String),
    #[error("{0}")]
    Io(String),
    #[error("{0}")]
    CorruptData(String),
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

/// Mutations a table access method can execute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableCapabilities {
    pub insert: bool,
    pub update: bool,
    pub delete: bool,
    pub truncate: bool,
}

impl TableCapabilities {
    pub const MUTABLE: Self = Self {
        insert: true,
        update: true,
        delete: true,
        truncate: true,
    };

    pub const APPEND_ONLY: Self = Self {
        insert: true,
        update: false,
        delete: false,
        truncate: false,
    };
}

/// A fallible tuple stream. Storage failures can occur after a scan has begun
/// (for example while opening the next Parquet fragment), so errors travel as
/// iterator items instead of being collapsed into an eager open result.
pub type TupleStream = Box<
    dyn Iterator<Item = Result<(Tid, Tuple), StorageError>> + Send,
>;

/// Which of a relation's columns a scan actually needs.
///
/// A columnar engine reads only the selected columns off disk; a row store
/// ignores the request entirely. Either is correct — see [`TableAm::scan`] for
/// the contract that makes ignoring it free.
///
/// Tuples stay full width regardless: this narrows the *work*, never the row
/// shape. The whole executor addresses columns by schema position
/// (`ColumnRef { index }` indexes the row directly), so a narrowed row would
/// invalidate every index above the scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnProjection {
    /// Every column. The state of any scan whose needs could not be proven.
    All,
    /// Sorted, deduplicated, in-range schema ordinals. Never empty and never a
    /// complete cover — [`ColumnProjection::of`] normalizes those away, and is
    /// the only sanctioned way to build this variant.
    Some(Arc<[usize]>),
}

impl ColumnProjection {
    /// Build a projection from the schema ordinals a plan proved it reads.
    ///
    /// Every branch that cannot produce a *correct narrower* answer returns
    /// [`Self::All`]. Reading too much is only slow; reading too little returns
    /// wrong rows, because the pruned slots come back as placeholder values.
    ///
    /// Three cases are normalized so consumers never have to:
    ///
    /// * An **out-of-range** ordinal means the demand was computed in the wrong
    ///   index space. That is a planner bug, and the safe response is to read
    ///   everything rather than silently drop the column it names.
    /// * An **empty** set (`SELECT count(*)`, which reads no column at all)
    ///   becomes the single narrowest column rather than a zero-column read.
    ///   Counting rows still only pays for one column — the entire win — while
    ///   avoiding any dependence on how a storage format reports the row count
    ///   of a batch with no columns.
    /// * A set **covering every column** becomes `All`, so engines take their
    ///   unprojected fast path and equality against `All` is meaningful. This
    ///   is checked last, so a one-column relation whose demand was empty
    ///   normalizes to `All` rather than to the complete cover `Some([0])`.
    pub fn of(columns: impl IntoIterator<Item = usize>, schema: &TableSchema) -> Self {
        let width = schema.columns.len();
        let mut wanted: Vec<usize> = Vec::new();
        for index in columns {
            if index >= width {
                return ColumnProjection::All;
            }
            wanted.push(index);
        }
        wanted.sort_unstable();
        wanted.dedup();

        if wanted.is_empty() {
            // A relation with no columns at all leaves nothing to prune, and
            // `width == 0` is caught by the cover check just below.
            if width == 0 {
                return ColumnProjection::All;
            }
            wanted.push(narrowest_column(schema));
        }
        if wanted.len() >= width {
            return ColumnProjection::All;
        }
        ColumnProjection::Some(wanted.into())
    }
}

/// The column whose values are cheapest to read: the narrowest fixed-width one,
/// else the first column. `typlen` follows PostgreSQL's convention — a positive
/// byte width for a fixed-width type, negative for a variable-length one — so
/// ordering by it puts a `bool` ahead of a `text`.
///
/// Only called for a non-empty schema, so column 0 is always a valid fallback.
fn narrowest_column(schema: &TableSchema) -> usize {
    schema
        .columns
        .iter()
        .enumerate()
        .filter(|(_, column)| column.ty.typlen() > 0)
        .min_by_key(|(_, column)| column.ty.typlen())
        .map_or(0, |(index, _)| index)
}

/// Table access: scans and modifications on one table, all judged against the
/// caller's [`TxnContext`].
pub trait TableAm: Send + Sync {
    fn schema(&self) -> &TableSchema;

    fn capabilities(&self) -> TableCapabilities {
        TableCapabilities::MUTABLE
    }

    /// Semantic indexes currently attached to this table.
    fn indexes(&self) -> Vec<IndexMetadata> {
        Vec::new()
    }

    /// The engine-internal storage leaves a read of this relation must union,
    /// in scan order, or `None` (the default) when the relation *is* its own
    /// storage and is scanned directly.
    ///
    /// These are not catalog relations: they have no `pg_class` row, no OID, and
    /// no name a user can write. They exist so an access method whose storage is
    /// physically several sources — say a durable columnar store fronted by a
    /// RAM write buffer — can be read as an `Append` and gain per-leaf planning,
    /// without any of that becoming SQL-visible.
    ///
    /// A write target is always the relation itself. An access method that
    /// splits its storage routes writes internally, so [`TableAm::capabilities`]
    /// and every DML error keep describing the relation and its declared access
    /// method, never a leaf. Leaves are produced for reads only.
    ///
    /// All leaves are scanned under one [`TxnContext`], so they share a snapshot.
    /// An access method that moves rows between leaves must make that move a
    /// transaction, so a shared snapshot yields every row exactly once.
    fn storage_leaves(&self) -> Option<Vec<Arc<dyn TableAm>>> {
        None
    }

    /// The whole `EXPLAIN` node line for a sequential scan of this relation.
    ///
    /// Returning the entire line, not just a name, is what lets a relation that
    /// is physically several sources label each one distinctly: an `Append` over
    /// two leaves of the *same* relation would otherwise print the same line
    /// twice and read like a planner bug. The default reproduces PostgreSQL's
    /// `Seq Scan on <rel>` and is what every catalog relation and SQL partition
    /// uses.
    fn scan_label(&self) -> String {
        format!("Seq Scan on {}", self.schema().name)
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
    ///
    /// `projection` names the columns the caller will actually read. Every
    /// tuple is still full width in schema order, but the values at positions
    /// **outside** `projection` are *unspecified* — an engine may return the
    /// real value or a placeholder, and callers must not read them. An engine
    /// that cannot prune ignores the argument entirely; that is always correct,
    /// which is why this is a performance hint rather than a contract on the
    /// result. Pass [`ColumnProjection::All`] whenever the whole row is needed
    /// — notably every DML path, which rebuilds full rows by ordinal.
    ///
    /// Pruning columns must never change the number, order, or [`Tid`] of the
    /// rows produced: `fetch` addresses rows by position within a scan.
    fn scan(&self, txn: &TxnContext, projection: &ColumnProjection) -> TupleStream;

    /// Fetch one version by tid if it is visible to `txn` — the re-read
    /// EvalPlanQual needs after a conflict, and a point lookup for indexes.
    fn fetch(&self, tid: Tid, txn: &TxnContext) -> Result<Option<Tuple>, StorageError>;

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
    fn insert(&self, tuple: Tuple, txn: &TxnContext) -> Result<Tid, StorageError>;

    /// Insert a statement's complete tuple batch. Engines with a columnar write
    /// path override this to build one or more fragments rather than one file per
    /// tuple.
    fn insert_many(
        &self,
        tuples: Vec<Tuple>,
        txn: &TxnContext,
    ) -> Result<Vec<Tid>, StorageError> {
        tuples
            .into_iter()
            .map(|tuple| self.insert(tuple, txn))
            .collect()
    }

    /// Replace the version identified by `tid`: the old version is marked
    /// deleted by `txn` and a new version holding `tuple` is inserted. The tuple
    /// contract matches [`TableAm::insert`].
    fn update(
        &self,
        tid: Tid,
        tuple: Tuple,
        txn: &TxnContext,
    ) -> Result<UpdateResult, StorageError>;

    /// Mark the version identified by `tid` deleted by `txn`.
    fn delete(&self, tid: Tid, txn: &TxnContext) -> Result<DeleteResult, StorageError>;

    /// Apply a batch of replacements, returning how many rows were found and
    /// updated (vanished tids are skipped, not counted). Engines should
    /// override this to apply the whole batch under one lock — per-row calls
    /// make a large UPDATE quadratic.
    fn update_many(
        &self,
        updates: Vec<(Tid, Tuple)>,
        txn: &TxnContext,
    ) -> Result<u64, StorageError> {
        let mut applied = 0;
        for (tid, tuple) in updates {
            if self.update(tid, tuple, txn)? == UpdateResult::Updated {
                applied += 1;
            }
        }
        Ok(applied)
    }

    /// Batch counterpart of [`TableAm::delete`], mirroring
    /// [`TableAm::update_many`].
    fn delete_many(&self, tids: Vec<Tid>, txn: &TxnContext) -> Result<u64, StorageError> {
        let mut applied = 0;
        for tid in tids {
            if self.delete(tid, txn)? == DeleteResult::Deleted {
                applied += 1;
            }
        }
        Ok(applied)
    }

    /// Remove every row (TRUNCATE). Row identity is not preserved: engines need
    /// not keep tids reusable after a truncate. The default scans and deletes;
    /// engines should override with a whole-table reset.
    fn truncate(&self, txn: &TxnContext) -> Result<(), StorageError> {
        let tids: Result<Vec<Tid>, StorageError> = self
            .scan(txn, &ColumnProjection::All)
            .map(|row| row.map(|(tid, _)| tid))
            .collect();
        self.delete_many(tids?, txn)?;
        Ok(())
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

    /// Reject a schema the relation's access method cannot represent, before
    /// any of the DDL layer's own per-column validation runs.
    ///
    /// [`TableEngine::create_table`] checks this anyway, but only at the very
    /// end. A column of a type the method does not store would otherwise be
    /// diagnosed by whichever earlier rule happened to trip on it — a `json`
    /// column collects a complaint about B-tree operator classes long before
    /// anything says the method cannot store `json` at all.
    fn validate_schema(&self, _schema: &TableSchema) -> Result<(), StorageError> {
        Ok(())
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

    /// Tidy a relation's storage (`VACUUM`): reclaim versions dead to every
    /// snapshot at or before `oldest`, and make durable whatever an access method
    /// is holding in memory. Returns the number of rows made durable, which is
    /// zero for an access method that only reclaims.
    ///
    /// Deliberately not a [`TableAm`] method: an access method with a RAM write
    /// buffer flushes by running its own transaction, and only the engine has the
    /// transaction service. The default reclaims via [`TableAm::vacuum`].
    fn vacuum_table(
        &self,
        _namespace: &str,
        name: &str,
        _oldest: Xid,
    ) -> Result<u64, StorageError> {
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

/// How a registered routine is implemented, from the binder's point of view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoutineImpl {
    /// A `LANGUAGE SQL` body as SQL **text** — a `SELECT <expr>` the binder
    /// re-parses and inlines into the calling expression, keeping this crate
    /// free of a parser/binder dependency exactly as [`ViewDefinition`] carries
    /// a view's query text.
    Sql(String),
    /// A `LANGUAGE plpgsql` body, which is an imperative program and cannot be
    /// inlined. The binder emits a call the executor dispatches at run time,
    /// identified by [`RoutineSig::oid`].
    PlPgSql,
}

/// Whether a routine was created by `CREATE FUNCTION` or `CREATE PROCEDURE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutineKind {
    Function,
    Procedure,
}

/// A registered routine, as the binder needs it to resolve a call.
///
/// Functions and procedures share one signature — PostgreSQL resolves overloads
/// across all of `pg_proc` regardless of language or kind, so `f(int)` in SQL
/// and `f(text)` in PL/pgSQL compete in one pool and an equally good match
/// between them is ambiguous. Splitting them into two lookups would give the
/// wrong preference and the wrong ambiguity behavior.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutineSig {
    /// Catalog OID — the handle a runtime call carries.
    pub oid: u32,
    pub name: String,
    pub arg_types: Vec<PgType>,
    /// `void` for a procedure, which declares no return type.
    pub return_type: PgType,
    pub kind: RoutineKind,
    /// `STRICT`: a NULL argument yields NULL without entering the body.
    pub strict: bool,
    pub imp: RoutineImpl,
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

    /// Every user-defined routine registered under `name` (case-insensitive),
    /// functions *and* procedures, one [`RoutineSig`] per overload. The binder
    /// consults this when no built-in matches a call, then either inlines a SQL
    /// body or emits a runtime call. Callers with no user routines (binder unit
    /// tests) get the empty default.
    fn routines(&self, _name: &str) -> Vec<RoutineSig> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_and_physical_tids_never_collide() {
        // The two spaces must stay disjoint at their boundaries, which is what
        // lets `fetch` route on the tid alone. Note `offset` 0 is legal for a
        // logical id but not for a heap line pointer — irrelevant, because the
        // flag, not the offset, is what separates them.
        let physical = [
            Tid::new(0, 0),
            Tid::new(0, 1),
            Tid::new(1, u16::MAX),
            Tid::new(MAX_PHYSICAL_BLOCK, u16::MAX),
        ];
        let logical = [0, 1, 0xffff, 0x1_0000, MAX_ROW_ID];

        for tid in physical {
            assert!(!tid.is_logical(), "{tid:?} must read as physical");
            assert_eq!(tid.row_id(), None);
        }
        for row_id in logical {
            let tid = Tid::logical(row_id);
            assert!(tid.is_logical(), "row id {row_id} must read as logical");
            assert_eq!(tid.row_id(), Some(row_id), "row id must round-trip");
        }

        // Every logical tid sorts above every physical one, so a scan that
        // yields durable rows before buffered ones is already tid-ordered.
        let highest_physical = physical
            .iter()
            .map(|t| t.packed())
            .max()
            .expect("the physical fixture is non-empty");
        let lowest_logical = logical
            .iter()
            .map(|id| Tid::logical(*id).packed())
            .min()
            .expect("the logical fixture is non-empty");
        assert!(
            highest_physical < lowest_logical,
            "physical tids must sort below logical ones"
        );
    }

    #[test]
    fn the_logical_flag_is_the_only_bit_separating_the_spaces() {
        // Row id 0 and physical (0, 0) differ in exactly the flag: proof the
        // split costs one bit and nothing else.
        assert_eq!(Tid::logical(0).block, TID_LOGICAL_FLAG);
        assert_eq!(Tid::logical(0).offset, 0);
        assert_eq!(MAX_PHYSICAL_BLOCK, TID_LOGICAL_FLAG - 1);
        // The largest row id saturates the tid exactly — every bit but the flag
        // is in use, so 47 is the true capacity and not a rounded-down guess.
        assert_eq!(Tid::logical(MAX_ROW_ID), Tid::new(u32::MAX, u16::MAX));
    }
}

#[cfg(test)]
mod column_projection_tests {
    use super::*;
    use crabgresql_types::PgType;

    fn schema(types: &[PgType]) -> TableSchema {
        TableSchema::new(
            "t",
            types
                .iter()
                .enumerate()
                .map(|(index, ty)| Column::new(format!("c{index}"), *ty))
                .collect(),
        )
    }

    #[test]
    fn a_proven_subset_is_kept_sorted_and_deduplicated() {
        let schema = schema(&[PgType::Int4, PgType::Int8, PgType::Text, PgType::Bool]);
        assert_eq!(
            ColumnProjection::of([2, 0, 2], &schema),
            ColumnProjection::Some(vec![0, 2].into())
        );
    }

    /// A set covering every column takes the unprojected fast path, so an
    /// engine can branch on `All` and mean it.
    #[test]
    fn a_complete_cover_normalizes_to_all() {
        let schema = schema(&[PgType::Int4, PgType::Text]);
        assert_eq!(ColumnProjection::of([0, 1], &schema), ColumnProjection::All);
    }

    /// `count(*)` reads no column, but a one-column relation has no narrower
    /// answer than "all of it" — the cover check has to run *after* the
    /// empty-set fill for this to hold.
    #[test]
    fn an_empty_demand_narrows_but_never_produces_a_cover() {
        let wide = schema(&[PgType::Text, PgType::Bool, PgType::Int8]);
        assert_eq!(
            ColumnProjection::of([], &wide),
            ColumnProjection::Some(vec![1].into()),
            "the 1-byte bool is cheaper than the int8 or the varlena text"
        );

        let single = schema(&[PgType::Int4]);
        assert_eq!(ColumnProjection::of([], &single), ColumnProjection::All);
        assert_eq!(ColumnProjection::of([], &schema(&[])), ColumnProjection::All);
    }

    /// The load-bearing direction: an ordinal outside the schema means the
    /// demand was computed in the wrong index space. Reading everything is
    /// merely slow; dropping the column would return NULL for a real value.
    #[test]
    fn an_out_of_range_ordinal_fails_safe_to_all() {
        let schema = schema(&[PgType::Int4, PgType::Text, PgType::Bool]);
        assert_eq!(ColumnProjection::of([3], &schema), ColumnProjection::All);
        assert_eq!(ColumnProjection::of([0, 9], &schema), ColumnProjection::All);
        // Not: drop the 9, keep {0}. And not: drop it, find the set empty, and
        // fall through to the narrowest column.
        assert_eq!(ColumnProjection::of([9], &schema), ColumnProjection::All);
    }
}
