//! Storage engine API: the `TableEngine` / `TableAm` extension point.
//!
//! Every data method carries a [`TxnContext`]: the engine judges visibility
//! against the caller's snapshot and stamps writes with the caller's XID, so
//! **MVCC lives in the engine while snapshots and XIDs stay the core's job**
//! (docs/ARCHITECTURE.md §1.3). `crabgresql-memory-storage` is the reference
//! implementation of this contract; `crabgresql-pg-engine` (the durable heap
//! engine) is the canonical consumer the shapes here are designed for — hence a
//! real `(block, offset)` [`Tid`] rather than an opaque scalar.

use std::sync::Arc;

use crabgresql_txn::{Clog, TxnContext, Xid};
use crabgresql_types::{PgType, Value};

pub use crabgresql_txn as txn;

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

/// A user relation together with its mutable index metadata.
#[derive(Clone, Debug)]
pub struct RelationMetadata {
    pub schema: TableSchema,
    pub indexes: Vec<IndexMetadata>,
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
    #[error("relation \"{0}\" already exists")]
    RelationAlreadyExists(String),
    #[error("index target relation \"{0}\" does not exist")]
    IndexTableNotFound(String),
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

    /// Full scan yielding only the versions visible to `txn`'s snapshot. The
    /// iterator captures the snapshot up front, so a DML statement never
    /// re-visits rows it modified itself (the reader's own new versions carry
    /// the reader's command id and stay invisible to the same command).
    fn scan(&self, txn: &TxnContext) -> Box<dyn Iterator<Item = (Tid, Tuple)> + Send>;

    /// Fetch one version by tid if it is visible to `txn` — the re-read
    /// EvalPlanQual needs after a conflict, and a point lookup for indexes.
    fn fetch(&self, tid: Tid, txn: &TxnContext) -> Option<Tuple>;

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
pub trait TableEngine: Send + Sync {
    fn create_table(&self, schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError>;

    fn open_table(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError>;

    /// Remove a table and all its data. `TableNotFound` if it doesn't exist.
    fn drop_table(&self, name: &str) -> Result<(), StorageError>;

    /// Register a semantic index after the caller has validated its keys and,
    /// for UNIQUE, the table's existing contents.
    fn create_index(&self, _table: &str, index: IndexMetadata) -> Result<(), StorageError> {
        Err(StorageError::RelationAlreadyExists(index.name))
    }

    /// Whether `index_name` is occupied in the namespace of `table`. The table
    /// parameter matters for session overlays where temp and public may use the
    /// same relation name.
    fn index_name_exists(&self, _table: &str, index_name: &str) -> bool {
        self.open_table(index_name).is_ok()
            || self.relation_metadata().iter().any(|relation| {
                relation
                    .indexes
                    .iter()
                    .any(|index| index.name == index_name)
            })
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

    /// Enumerate user relations including live index metadata for catalog
    /// reflection. Engines with tables override this; the fallback preserves
    /// compatibility for read-only/system engines.
    fn relation_metadata(&self) -> Vec<RelationMetadata> {
        self.relations()
            .into_iter()
            .map(|schema| RelationMetadata {
                schema,
                indexes: Vec::new(),
            })
            .collect()
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
