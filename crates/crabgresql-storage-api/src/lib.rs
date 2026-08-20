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

use arrow_array::RecordBatch;
use crabgresql_txn::{Clog, TupleHeader, TxnContext, Xid};
use crabgresql_types::{PgType, Value};

pub use crabgresql_txn as txn;

pub mod arrow;
pub mod pgstat;
pub mod sort;

mod stats;
pub use stats::{ColStats, RelStats};

/// What an engine's buffer pool reports about the blocks it served. See
/// [`TableEngine::buffer_stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BufferStats {
    /// Blocks found resident.
    pub hits: u64,
    /// Blocks that had to be read from storage.
    pub reads: u64,
}

/// The rows an index probe yields: `(tid, tuple)` per match, or the error that
/// stopped the probe. Fallible because a value may live out of line, and a read
/// that cannot reassemble it must reach the caller rather than silently yield
/// one row fewer than a sequential scan of the same table.
pub type IndexProbe = Box<dyn Iterator<Item = Result<(Tid, Tuple), StorageError>> + Send>;

/// One end of an index range: the value, and whether the bound itself matches.
#[derive(Clone, Copy, Debug)]
pub struct IndexBound<'a> {
    pub value: &'a Value,
    pub inclusive: bool,
}

/// What [`TableAm::index_lookup`] searches for: the index's leading key columns
/// pinned to values, and optional bounds on the one column after them.
///
/// This is the shape a B-tree can actually serve, and it is deliberately not
/// more general — an index orders its keys left to right, so a predicate can
/// narrow a contiguous stretch of it only by fixing a prefix and then bounding
/// the next column. `eq` holds that prefix, in key order, and may be shorter
/// than the index's key list (an index on `(a, b)` searched by `a` alone) or
/// empty (bounds on the first key column). `lower`/`upper` apply to key column
/// number `eq.len()`, which must exist.
///
/// Borrowed rather than owned because `UNIQUE` enforcement probes once per
/// inserted row; an owning key would allocate on that path.
#[derive(Clone, Copy, Debug)]
pub struct IndexProbeKey<'a> {
    pub eq: &'a [Value],
    pub lower: Option<IndexBound<'a>>,
    pub upper: Option<IndexBound<'a>>,
}

impl<'a> IndexProbeKey<'a> {
    /// The whole key pinned by equality — every index key column, in key order.
    pub fn equality(eq: &'a [Value]) -> Self {
        IndexProbeKey {
            eq,
            lower: None,
            upper: None,
        }
    }
}

/// One of PostgreSQL's per-row system columns.
///
/// `oid` is absent because PostgreSQL 12 removed it. The order of the variants
/// is the order slots are appended to a row in, so it is load-bearing: the
/// binder pushes [`OutputColumn`](crate::OutputColumn)s and the executor pushes
/// [`Value`]s by walking the same sorted list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SysCol {
    /// The OID of the relation the row lives in. Answerable by every access
    /// method: it is a fact about the relation, not about the row's storage.
    TableOid,
    /// The row version's physical address, PG's `ctid`.
    Ctid,
    /// The inserting transaction.
    Xmin,
    /// The command within [`SysCol::Xmin`] that inserted the row.
    Cmin,
    /// The deleting transaction, or `0` while the version is live.
    Xmax,
    /// The command within [`SysCol::Xmax`] that deleted the row.
    Cmax,
}

impl SysCol {
    /// Every system column, in row order.
    pub const ALL: [SysCol; 6] = [
        SysCol::TableOid,
        SysCol::Ctid,
        SysCol::Xmin,
        SysCol::Cmin,
        SysCol::Xmax,
        SysCol::Cmax,
    ];

    /// The name a query addresses this column by.
    pub const fn name(self) -> &'static str {
        match self {
            SysCol::TableOid => "tableoid",
            SysCol::Ctid => "ctid",
            SysCol::Xmin => "xmin",
            SysCol::Cmin => "cmin",
            SysCol::Xmax => "xmax",
            SysCol::Cmax => "cmax",
        }
    }

    /// The type the slot carries.
    pub const fn ty(self) -> PgType {
        match self {
            SysCol::TableOid => PgType::Oid,
            SysCol::Ctid => PgType::Tid,
            SysCol::Xmin | SysCol::Xmax => PgType::Xid,
            SysCol::Cmin | SysCol::Cmax => PgType::Cid,
        }
    }

    /// Whether answering this needs the row version's MVCC header rather than
    /// just its tid — the difference between
    /// [`TableAm::scan`](crabgresql_storage_api::TableAm::scan) and
    /// [`TableAm::scan_with_system`](crabgresql_storage_api::TableAm::scan_with_system).
    pub const fn needs_header(self) -> bool {
        matches!(
            self,
            SysCol::Xmin | SysCol::Cmin | SysCol::Xmax | SysCol::Cmax
        )
    }

    /// Whether the access method has to be able to produce this per row. Only
    /// `tableoid` does not: every relation has an identity, whatever it stores.
    pub const fn needs_storage_support(self) -> bool {
        !matches!(self, SysCol::TableOid)
    }

    /// The negative `attnum` `pg_attribute` lists this column at, read off a
    /// live PostgreSQL 18.4 — `ctid` is `-1` and the numbers run backwards to
    /// `tableoid` at `-6`. (`oid` no longer has one: PostgreSQL 12 removed the
    /// optional row OID, and with it the `-2` that used to hold the gap.)
    pub const fn attnum(self) -> i16 {
        match self {
            SysCol::Ctid => -1,
            SysCol::Xmin => -2,
            SysCol::Cmin => -3,
            SysCol::Xmax => -4,
            SysCol::Cmax => -5,
            SysCol::TableOid => -6,
        }
    }
}

/// A materialized row. Always as wide as the table schema, with column order
/// matching it. A scan restricted by a [`ColumnProjection`] still returns a
/// full-width tuple; only the values at unselected positions are unspecified.
pub type Tuple = Vec<Value>;

/// Row identity — PostgreSQL's `ctid`: the physical `(block, offset)` address of
/// a tuple version. Stable for a version's lifetime and never reused while the
/// version lives. The heap engine fills both fields from the page it lands on —
/// including a RAM-backed UNLOGGED/TEMP relation, whose pages are ordinary heap
/// pages that simply never reach a file. An access method with no pages at all
/// (the read-only system catalogs) synthesizes the pair from a row counter
/// (`block` the high bits, `offset` the low), so its tids are just as opaque but
/// share the type the heap engine needs.
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
    /// offset)` pair — how a pageless access method numbers its rows, and how
    /// the heap's B-tree decodes a tid back out of an index key.
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
    /// Whether SQL NULL is accepted. A PRIMARY KEY also sets this to false —
    /// at `CREATE TABLE`, or later through `ALTER TABLE ... ADD PRIMARY KEY`,
    /// which republishes the whole schema rather than mutating this in place
    /// (see [`TableAm::schema`]).
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
    /// The column's generation expression, when it is a generated column.
    /// Mutually exclusive with [`Column::default`] — PostgreSQL rejects a column
    /// declaring both.
    pub generated: Option<GeneratedColumn>,
}

/// A generated column's kind and expression.
///
/// The expression is canonical SQL text, for the same reason
/// [`Column::default`] is: this crate depends on neither the parser nor the
/// binder. A `Stored` column's value is computed once per write and lives in the
/// tuple; a `Virtual` column stores nothing (its slot holds NULL) and the binder
/// substitutes the expression wherever the column is referenced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedColumn {
    pub kind: Generation,
    pub expr: String,
}

/// Whether a generated column's value is materialized on write (`STORED`) or
/// recomputed on every read (`VIRTUAL`, PostgreSQL's default since 18).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Generation {
    Stored,
    Virtual,
}

impl Generation {
    /// `pg_attribute.attgenerated`: `s` for stored, `v` for virtual.
    pub fn attgenerated(self) -> char {
        match self {
            Generation::Stored => 's',
            Generation::Virtual => 'v',
        }
    }
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
            generated: None,
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
            generated: None,
        }
    }

    /// Whether this column's value is recomputed on read rather than stored.
    pub fn is_virtual_generated(&self) -> bool {
        matches!(
            self.generated,
            Some(GeneratedColumn {
                kind: Generation::Virtual,
                ..
            })
        )
    }

    /// PostgreSQL's `atttypmod` encoding of this column's declared modifier,
    /// from the raw form [`Self::typmod`] stores: a bare length for the
    /// character/bit types, the packed `(precision, scale)` for `numeric`, the
    /// packed `(fields, precision)` for `interval`, a bare precision for the
    /// other datetime types, `-1` for none.
    ///
    /// `character`/`character varying`/`numeric` reserve four bytes for the
    /// varlena header (`raw + VARHDRSZ`); the fixed-width types store their
    /// modifier directly. Keeping this the true PostgreSQL encoding lets
    /// `format_type(atttypid, atttypmod)` reproduce PG's `\d` type strings — and
    /// lets an error message name a type the way PostgreSQL names it.
    ///
    /// The addition saturates rather than wrapping: DDL rejects a length beyond
    /// PostgreSQL's limit ([`crabgresql_types::text::MAX_CHAR_LENGTH`]) and a
    /// precision beyond `numeric`'s, so a value that could overflow is
    /// unreachable through a `CREATE TABLE` — but this also runs against
    /// whatever a data directory already holds, and building a catalog row must
    /// never panic the session that reads `pg_attribute`.
    pub fn atttypmod(&self) -> i32 {
        const VARHDRSZ: i32 = 4;
        match self.ty {
            _ if self.typmod < 0 => -1,
            PgType::Varchar | PgType::Bpchar | PgType::Numeric => {
                self.typmod.saturating_add(VARHDRSZ)
            }
            _ => self.typmod,
        }
    }
}

/// A `CHECK` constraint: a boolean predicate every row of the relation must
/// satisfy, kept as canonical SQL text for the same reason [`Column::default`]
/// is — this crate depends on neither the parser nor the binder, and the
/// on-disk catalog is a frozen format that a binder IR must not be pinned to.
/// The binder reparses and binds it once per DML statement; the executor
/// evaluates it per row.
///
/// PostgreSQL rejects only a predicate that evaluates to **false**: an unknown
/// (NULL) result passes, which is why a `CHECK (x > 3)` admits a NULL `x`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckConstraint {
    /// Always resolved at DDL time — PostgreSQL has no anonymous constraints in
    /// its catalog. An unnamed one is given `{table}_{column}_check` or
    /// `{table}_check`, deduplicated with a numeric suffix.
    pub name: String,
    /// The predicate in `pg_get_expr`'s *non-pretty* spelling, every operator
    /// node parenthesised — `CHECK (x + y < 100)` stores `((x + y) < 100)`.
    /// This is `pg_constraint.conbin` as we model it.
    pub expr: String,
    /// `pg_constraint.conkey`: the column positions the predicate references,
    /// ascending and deduplicated. Derived from the bound expression at DDL
    /// time, so it follows the relation's *final* column layout.
    pub columns: Vec<usize>,
    /// `pg_constraint.convalidated`. Always `true`: DDL rejects `NOT VALID`, so
    /// no unvalidated constraint can be created.
    ///
    /// TODO: accept `NOT VALID` on a CHECK constraint — record it unvalidated
    /// (skipping the existing-row scan) until `VALIDATE CONSTRAINT` clears it.
    pub validated: bool,
    /// `pg_constraint.conislocal`: declared on this relation itself, rather than
    /// only inherited. A child that redeclares a parent's constraint has both
    /// this and a non-zero [`Self::inhcount`].
    pub islocal: bool,
    /// `pg_constraint.coninhcount`: how many direct parents contributed it.
    /// Zero on a relation that is not an inheritance child.
    pub inhcount: i16,
}

/// Access method recorded for a semantic index: the `pg_am` name DDL declared,
/// which the catalogs reflect and DDL validates against (`hash` rejects
/// UNIQUE). Whether a probe can actually be served physically is a separate
/// question, answered per index by [`TableAm::supports_index_scan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexMethod {
    BTree,
    Hash,
}

impl IndexMethod {
    /// The `pg_am.amname` this method is spelled with — the word `USING` takes
    /// in DDL and `pg_get_indexdef` prints back.
    pub fn name(self) -> &'static str {
        match self {
            IndexMethod::BTree => "btree",
            IndexMethod::Hash => "hash",
        }
    }
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

/// Metadata for an index relation. Only simple column indexes are represented.
///
/// TODO: represent expression keys, a partial index's `WHERE` predicate and
/// `INCLUDE` columns — DDL rejects all three rather than lose them here.
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

/// Reproduce an index's `CREATE INDEX` statement, as PostgreSQL's
/// `pg_get_indexdef` prints it and `pg_indexes.indexdef` reports it.
///
/// Lives next to [`IndexMetadata`] rather than in the executor, where the rest
/// of the SQL-surface rendering lives, because it has *two* readers that cannot
/// see each other: `pg_get_indexdef` in `crabgresql-executor` and
/// `pg_indexes.indexdef` in `crabgresql-catalog`, neither crate depending on the
/// other. Two copies of this grammar would drift silently — a client comparing
/// the function against the view would be the one to find out.
///
/// Observed from PostgreSQL 18.4: the table is always schema-qualified and the
/// index never is; a sort direction prints only when it is `DESC`; a null
/// placement prints only when it differs from the direction's default (`NULLS
/// LAST` for ascending, `NULLS FIRST` for descending); `NULLS NOT DISTINCT`
/// trails the whole key list.
pub fn index_definition(index: &IndexMetadata, table: &TableSchema) -> String {
    let mut out = String::from("CREATE ");
    if index.unique {
        out.push_str("UNIQUE ");
    }
    out.push_str("INDEX ");
    out.push_str(&crabgresql_types::text::quote_ident(&index.name));
    out.push_str(" ON ");
    out.push_str(&crabgresql_types::text::quote_ident(&table.namespace));
    out.push('.');
    out.push_str(&crabgresql_types::text::quote_ident(&table.name));
    out.push_str(" USING ");
    out.push_str(index.method.name());
    out.push_str(" (");
    for (i, key) in index.keys.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        // `?column?` is PostgreSQL's own placeholder for a column it cannot
        // name. A key never outruns the schema it was built from, so this
        // stands for a corrupt pairing rather than for an expression key —
        // which `IndexMetadata` cannot represent at all.
        match table.columns.get(key.column) {
            Some(column) => out.push_str(&crabgresql_types::text::quote_ident(&column.name)),
            None => out.push_str("?column?"),
        }
        if key.descending {
            out.push_str(" DESC");
        }
        if key.nulls_first != key.descending {
            out.push_str(match key.nulls_first {
                true => " NULLS FIRST",
                false => " NULLS LAST",
            });
        }
    }
    out.push(')');
    if !index.nulls_distinct {
        out.push_str(" NULLS NOT DISTINCT");
    }
    out
}

/// The partitioning strategy of a partitioned (parent) table.
///
/// TODO: add LIST and HASH partitioning — `PARTITION BY` accepts only RANGE.
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

/// One entry of a table's `INHERITS (...)` list. The parent is named rather
/// than OID'd — this layer has no OIDs, and it mirrors how [`PartitionOf`]
/// points at its own parent.
///
/// Inheritance and partitioning differ in every way that matters downstream: an
/// inheritance parent owns rows of its own (`relkind = 'r'`), a child's columns
/// are a *superset* of the parent's in an order of the child's choosing, and an
/// INSERT aimed at the parent stays in the parent instead of being routed.
#[derive(Clone, Debug, PartialEq)]
pub struct InheritParent {
    pub namespace: String,
    pub name: String,
}

/// The physical file numbers behind one relation — PostgreSQL's `relfilenode`,
/// the source of `pg_class.relfilenode` for the relation, its out-of-line chunk
/// store, and each of its indexes.
///
/// `0` everywhere means "no file", which is a real state rather than a gap: an
/// index can be metadata-only, and a relation only grows a chunk store once a
/// row first needs one. An engine that keeps no per-relation files at all leaves
/// the whole struct at its default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelationFilenodes {
    /// The relation's own heap file.
    pub rel: u32,
    /// The relation's out-of-line chunk store, or `0` when it has none.
    pub toast: u32,
    /// Keyed by index *name* rather than by position: the order of
    /// [`RelationMetadata::indexes`] is the table handle's, and the catalog is
    /// free to keep its own.
    pub indexes: Vec<(String, u32)>,
}

impl RelationFilenodes {
    /// The file number of the index named `name`, or `0` if it has none (or is
    /// not in this snapshot).
    pub fn index(&self, name: &str) -> u32 {
        self.indexes
            .iter()
            .find(|(index, _)| index == name)
            .map_or(0, |(_, rel)| *rel)
    }
}

/// A user relation together with its mutable index metadata and size estimates.
#[derive(Clone, Debug)]
pub struct RelationMetadata {
    pub schema: TableSchema,
    pub indexes: Vec<IndexMetadata>,
    /// What [`TableAm::statistics`] reported when this snapshot was taken —
    /// the source of `pg_class.relpages`/`reltuples`.
    pub stats: RelStats,
    /// The size of the relation's out-of-line storage, or `None` if it has none.
    /// Feeds the `pg_class` row of the TOAST relation and the parent's
    /// `reltoastrelid`; see [`TableAm::toast_statistics`].
    pub toast: Option<RelStats>,
    /// The physical file numbers `pg_class.relfilenode` reports for this
    /// relation and everything hanging off it.
    pub filenodes: RelationFilenodes,
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
        matches!(self, TableAccessMethod::Parquet | TableAccessMethod::Buffer)
    }

    /// Whether this method actually stores rows in [`TableSchema::sort_key`]
    /// order.
    ///
    /// Narrower than [`is_engine_managed`](Self::is_engine_managed), and
    /// deliberately so: a standalone `USING buffer` relation declares a key only
    /// to answer the same DDL as its sibling, but it is a RAM row store with
    /// nowhere to flush and orders nothing. Rules that exist to keep a declared
    /// order truthful — refusing a key the columnar sort cannot honor, say —
    /// have nothing to protect there, so they ask this rather than assuming
    /// every engine-managed method sorts.
    pub fn honors_sort_key(self) -> bool {
        matches!(self, TableAccessMethod::Parquet)
    }

    /// Whether a write batch becomes one immutable on-disk unit, so the size of
    /// the batch a bulk load hands over decides the size of the units it leaves
    /// behind. [`TableAccessMethod::bulk_load_batch_rows`] is what that is for.
    ///
    /// A row store answers `false`: it places rows one at a time and coalesces
    /// nothing, so a bigger batch buys it only a bigger memory floor.
    pub fn writes_whole_batches(self) -> bool {
        matches!(self, TableAccessMethod::Parquet)
    }

    /// How many rows a bulk load should decode before handing them over.
    ///
    /// For a row store this is only a memory bound, and a small one is right.
    /// For a method that turns each batch into one immutable unit it also decides
    /// how many of those units the load leaves behind: at 1024 rows a large file
    /// lands as thousands of tiny fragments — each with its own footer, file
    /// fsync, directory fsync and WAL flush — that no later flush compacts and
    /// every subsequent scan pays for. `u16::MAX` is the largest unit those
    /// methods build, their row offsets being 16-bit, so filling one exactly
    /// turns that cost per 1024 rows into the same cost per 65535.
    ///
    /// Advisory: a writer that cannot use a whole batch splits it itself.
    pub fn bulk_load_batch_rows(self) -> usize {
        if self.writes_whole_batches() {
            u16::MAX as usize
        } else {
            1024
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
    /// The physical access method. Plain `CREATE TABLE` uses `Heap`; an explicit
    /// `USING parquet` selects the append-only Parquet implementation.
    pub access_method: TableAccessMethod,
    /// `Some` on a partitioned (parent) table: its partition key. Such a table
    /// is `relkind = 'p'` and holds no rows of its own.
    pub partition_scheme: Option<PartitionScheme>,
    /// `Some` on a leaf partition: the parent it attaches to and its bound.
    pub partition_of: Option<PartitionOf>,
    /// The parents this table inherits from (`CREATE TABLE ... INHERITS (...)`),
    /// in declaration order — the order that assigns `pg_inherits.inhseqno`.
    /// Empty for every table that is not an inheritance child, and mutually
    /// exclusive with [`Self::partition_of`].
    ///
    /// Only the link is stored. The parent↔child column correspondence is
    /// recomputed by column *name* wherever it is needed, which is exact because
    /// the merge at `CREATE TABLE` gives the child a column of every parent name
    /// and nothing may afterwards rename or drop it (the `ALTER TABLE` forms
    /// this server implements add constraints, never columns).
    pub inherits: Vec<InheritParent>,
    /// The layout sort key: the order an engine-managed access method stores
    /// rows in, from `ORDER BY (...)` or defaulted to the `PRIMARY KEY`. A heap
    /// relation is always empty — `ORDER BY` on one is rejected at DDL time —
    /// and so is a relation created before the key was recorded.
    ///
    /// The Parquet engine honors it per write: each write is sorted whole
    /// before it is cut into fragments, so the fragments *of one write* have
    /// disjoint key ranges. The relation as a whole is **not** clustered — two
    /// writes produce two sorted runs that overlap freely — so a reader may
    /// prune within a write's fragments and nothing more. A method that does
    /// not [`honor a key`](TableAccessMethod::honors_sort_key) — a standalone
    /// `USING buffer` relation — carries one only so both engine-managed
    /// methods answer the same DDL alike.
    ///
    /// TODO: merge the per-write sorted runs by compaction, so pruning can
    /// exclude fragments across the whole relation and not only within a write.
    /// TODO: cut fragments at the 64 MiB target chunk size; a fragment is capped
    /// at 65,535 rows by the `Tid` offset until the V2 fragment footer lands.
    ///
    /// Only a key [`sort::sortable_layout`] accepts is honored. DDL rejects the
    /// rest, but a relation created before that check is stored in insertion
    /// order rather than failing its writes forever — so such a relation keeps
    /// working while its own `CREATE TABLE` no longer replays, which a
    /// dump/restore of one has to reckon with.
    pub sort_key: Vec<IndexKey>,
    /// The relation's `CHECK` constraints, including those inherited from a
    /// parent — inheritance copies the predicate into the child (as it copies
    /// columns), so enforcing a child's own list enforces its parents' too.
    ///
    /// Order is creation order, which is *not* the order violations are
    /// reported in: PostgreSQL resolves a failing row against the constraints
    /// sorted by name, so two violated checks always name the alphabetically
    /// first one.
    pub checks: Vec<CheckConstraint>,
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
            inherits: Vec::new(),
            sort_key: Vec::new(),
            checks: Vec::new(),
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
            inherits: Vec::new(),
            sort_key: Vec::new(),
            checks: Vec::new(),
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
            _ if self.cycle => SequenceAdvance::Value(if ascending { self.min } else { self.max }),
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
    /// A row that cannot be made to fit one page even after every out-of-line
    /// candidate has been moved out. PostgreSQL reports this as
    /// `54000 program_limit_exceeded`, with `max` being the largest tuple an
    /// otherwise-empty page can hold.
    #[error("row is too big: size {size}, maximum size {max}")]
    RowTooBig { size: usize, max: usize },
    /// A single value too large to store, independent of the row it sits in.
    /// PostgreSQL caps a varlena at 1 GB and reports `54000` for it too.
    #[error("value is too large: size {size}, maximum size {max}")]
    ValueTooBig { size: usize, max: usize },
    /// A `numeric` a columnar column's decimal cannot hold: NaN or ±Infinity,
    /// or — in a column with no typmod to round by — a value needing a finer
    /// scale or more digits than it has. PostgreSQL's analogous typmod failure
    /// is `22003`, which this borrows along with the shape of its DETAIL.
    #[error("numeric field overflow")]
    NumericFieldOverflow { detail: Option<String> },
    /// A key too large for an index page. A row can be far bigger than this and
    /// still be storable — only the *indexed* columns are capped — so the error
    /// names the index and the heap tuple rather than the row, as PostgreSQL's
    /// does. `54000` again.
    ///
    /// PostgreSQL says "btree version 4 maximum 2704"; we model neither index
    /// versions nor its page layout, so `max` is our own cap and the phrase is
    /// dropped. The DETAIL and HINT are its wording verbatim.
    #[error("index row size {size} exceeds btree maximum {max} for index \"{index}\"")]
    IndexRowTooBig {
        size: usize,
        max: usize,
        index: String,
        relation: String,
        tid: Tid,
    },
}

impl StorageError {
    /// PostgreSQL's DETAIL for this error, or `None` where it raises none.
    pub fn detail(&self) -> Option<String> {
        match self {
            StorageError::IndexRowTooBig { relation, tid, .. } => Some(format!(
                "Index row references tuple ({},{}) in relation \"{relation}\".",
                tid.block, tid.offset
            )),
            StorageError::NumericFieldOverflow { detail } => detail.clone(),
            _ => None,
        }
    }

    /// PostgreSQL's HINT for this error, or `None` where it raises none.
    pub fn hint(&self) -> Option<String> {
        match self {
            StorageError::IndexRowTooBig { .. } => Some(
                "Values larger than 1/3 of a buffer page cannot be indexed.\nConsider a function \
                 index of an MD5 hash of the value, or use full text indexing."
                    .to_string(),
            ),
            _ => None,
        }
    }
}

/// Outcome of `TableAm::update`.
///
/// `Conflict` is the EvalPlanQual / serialization seam: the row the caller
/// meant to update was updated or deleted by another transaction that committed
/// after the caller's snapshot. `updater` is that transaction (whom to wait on
/// or abort against); `latest` is the newest live version's tid to re-read
/// under READ COMMITTED. The variant's shape is fixed, but no engine
/// constructs it.
///
/// TODO: return `Conflict` from the engines when the target version was
/// updated or deleted by a transaction that committed after the caller's
/// snapshot, so READ COMMITTED can re-read the latest version and the stricter
/// isolation levels can raise a serialization failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateResult {
    /// Applied, with the [`Tid`] the **new** version landed at — what
    /// `UPDATE … RETURNING ctid` reports, and never the tid that was passed in:
    /// an MVCC update writes a new version and leaves the old one to vacuum.
    Updated(Tid),
    NotFound,
    Conflict {
        updater: Xid,
        latest: Option<Tid>,
    },
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
pub type TupleStream = Box<dyn Iterator<Item = Result<(Tid, Tuple), StorageError>> + Send>;

/// A [`TupleStream`] that also carries each version's MVCC header — what the
/// `xmin`, `xmax`, `cmin` and `cmax` system columns read. Produced by
/// [`TableAm::scan_with_system`]; see there for who can produce one.
pub type SystemTupleStream =
    Box<dyn Iterator<Item = Result<(Tid, TupleHeader, Tuple), StorageError>> + Send>;

/// A fallible stream of Arrow batches — the columnar twin of [`TupleStream`],
/// with the same reason for carrying errors as items.
///
/// Every batch is **full width** in schema order, exactly as a [`Tuple`] is:
/// columns outside the scan's [`ColumnProjection`] are present as all-NULL
/// arrays (see [`arrow::null_array`]). The executor addresses columns by schema
/// position, so a batch that packed only the projected columns would invalidate
/// every index above the scan — the same reasoning that keeps rows full width.
///
/// Values are in `Value` semantics, not Arrow's: see the invariant on
/// [`arrow`]. Notably a `Date32` here holds PostgreSQL epoch days.
///
/// Unlike a tuple stream, no [`Tid`] accompanies a batch. A batch scan is for
/// read-only pipelines that never address a row by identity; anything needing a
/// tid (DML, EvalPlanQual re-reads, index lookups) uses [`TableAm::scan`].
pub type BatchStream = Box<dyn Iterator<Item = Result<RecordBatch, StorageError>> + Send>;

/// Which of a relation's columns a scan actually needs.
///
/// A columnar engine reads only the selected columns off disk. A row store
/// reads the whole tuple either way, but still has a decode, an allocation and
/// (in the heap's case) a detoast to skip per unread column. Ignoring the
/// request entirely is also correct — see [`TableAm::scan`] for the contract
/// that makes it free.
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
    /// The relation's shape, as an immutable snapshot.
    ///
    /// A snapshot rather than a borrow because DDL changes a relation's shape
    /// while other sessions hold a handle on it, and PostgreSQL's own model is
    /// the same: a backend reads a relcache entry, and DDL makes the *next*
    /// open see a new one rather than mutating the live one underneath. An
    /// engine publishes a new schema by swapping the `Arc`; readers that
    /// already took one keep the consistent version they started with.
    ///
    /// Cheap enough to call freely — a refcount bump — but callers on a
    /// per-row path should hoist it out of the loop, since an engine whose
    /// schema can change takes a lock to hand one out.
    fn schema(&self) -> Arc<TableSchema>;

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
        RelStats::unknown(&self.schema())
    }

    /// The size of this relation's out-of-line ("TOAST") storage, or `None` for
    /// an access method that has none or has not needed it yet. Only the durable
    /// heap reports one — a columnar or in-memory access method has no page limit
    /// to overflow, so it never stores an attribute out of line.
    ///
    /// `Some` is what makes `pg_class.reltoastrelid` non-zero, which in
    /// PostgreSQL is the observable "this table has a TOAST relation" signal.
    fn toast_statistics(&self) -> Option<RelStats> {
        None
    }

    /// The relfilenode of this relation's out-of-line chunk store, or `0` when it
    /// has none. Same contract as [`TableAm::toast_statistics`], and the source of
    /// the chunk store's own `pg_class.relfilenode`.
    ///
    /// Answered by the table handle rather than by an engine's catalog because a
    /// **temporary** relation's chunk store is never recorded in one — it holds no
    /// durable definition at all — while the handle knows it either way.
    fn toast_relfilenode(&self) -> u32 {
        0
    }

    /// The size of one index's physical storage, or `None` for an engine that
    /// keeps the index as metadata only. Same cost contract as
    /// [`TableAm::statistics`]: cheap enough for the planner to ask per plan, so
    /// never a scan.
    ///
    /// Only `relpages` is meaningful. An index entry is not a row, so reporting a
    /// tuple count here would invite it to be read as one — the planner derives
    /// the entries it expects to visit from the table's `reltuples` and the
    /// estimated selectivity instead, exactly as PostgreSQL's
    /// `genericcostestimate` does.
    ///
    /// `None` is not "the index is empty": it means the size is unknown, and the
    /// planner falls back to estimating it from the table. An engine whose index
    /// is metadata-only also declines [`TableAm::supports_index_scan`], so the
    /// path is never costed at all.
    fn index_statistics(&self, _index_name: &str) -> Option<RelStats> {
        None
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

    /// Whether this access method can answer PostgreSQL's per-row system
    /// columns — `ctid`, `xmin`, `xmax`, `cmin`, `cmax`. An engine that says
    /// yes **must** return `Some` from [`TableAm::scan_with_system`], and its
    /// [`Tid`]s must address rows the way `ctid` promises.
    ///
    /// All five travel together deliberately. An engine either keeps a version
    /// header per row or it does not; one that answered `ctid` while raising on
    /// `xmin` would make `select ctid, xmin from t` fail for a reason nothing
    /// in the query explains. `tableoid` is not gated here at all — it is a
    /// fact about the *relation*, which every access method has.
    ///
    /// The default is `false`: the columnar engines store column chunks, not
    /// row versions, and have no header to surface. A reference to a system
    /// column on such a relation is rejected at bind time.
    ///
    /// **A delegating wrapper must forward this, [`TableAm::scan_with_system`],
    /// [`TableAm::index_lookup_with_system`] and [`TableAm::update_many_tids`]**
    /// — the same trap [`TableAm::scan_batches`] carries, and with the same
    /// silent failure: forgetting one compiles, and a leaf that *does* keep
    /// headers quietly stops answering.
    fn supports_system_columns(&self) -> bool {
        false
    }

    /// [`TableAm::scan`] with each version's MVCC header attached.
    ///
    /// `None` means the engine keeps no header, and then
    /// [`TableAm::supports_system_columns`] must be `false`. Separate from
    /// `scan` rather than folded into it because carrying the header costs a
    /// copy per row, and nearly every scan wants none: only a statement that
    /// names `xmin`/`xmax`/`cmin`/`cmax` reaches this. A statement naming only
    /// `ctid` uses the ordinary `scan` and keeps the [`Tid`] it already yields.
    ///
    /// Same projection contract as [`TableAm::scan`]: full-width tuples, values
    /// outside `projection` unspecified.
    fn scan_with_system(
        &self,
        _txn: &TxnContext,
        _projection: &ColumnProjection,
    ) -> Option<SystemTupleStream> {
        None
    }

    /// Whether the engine can serve [`TableAm::scan_batches`]. The planner
    /// consults this to decide whether a read can run vectorized, so it must
    /// agree with what `scan_batches` actually does — the same discipline
    /// [`TableAm::supports_index_scan`] enforces for index scans, and for the
    /// same reason: `EXPLAIN` advertises the choice.
    ///
    /// The default is `false`. The durable heap engine stores rows, so
    /// assembling batches from it would cost more than the vectorized operators
    /// above could win back; it stays on the row path deliberately.
    ///
    /// **A delegating wrapper must forward this and [`TableAm::scan_batches`].**
    /// Both have defaults, so a wrapper that forgets them compiles cleanly and
    /// silently reports "no batch path" for a leaf that has one — the feature
    /// then does nothing, with no error to notice.
    fn supports_batch_scan(&self) -> bool {
        false
    }

    /// Scan as Arrow batches instead of tuples, for a vectorized pipeline.
    ///
    /// Same visibility contract as [`TableAm::scan`]: the engine judges every
    /// row against `txn`'s snapshot before it reaches a batch, so a batch
    /// contains exactly the rows the row scan would have produced, in the same
    /// order. `projection` is the same performance hint, with the same rule
    /// that unselected columns hold unspecified values — here, NULLs.
    ///
    /// `None` means "no batch path": the caller falls back to [`TableAm::scan`],
    /// so a vectorized plan stays correct on every engine. Returning `None` here
    /// while reporting `true` from [`TableAm::supports_batch_scan`] is a bug in
    /// the engine, not a case the caller must handle gracefully — but the
    /// fallback keeps it from being a wrong-results bug.
    fn scan_batches(
        &self,
        _txn: &TxnContext,
        _projection: &ColumnProjection,
    ) -> Option<BatchStream> {
        None
    }

    /// Whether the engine can physically serve an index scan on `index_name` —
    /// i.e. whether [`TableAm::index_lookup`] would return `Some` rather than
    /// fall back to a scan. The planner consults this so it only chooses an
    /// index scan the executor can actually perform, keeping `EXPLAIN` honest.
    ///
    /// The answer is about the *index*, not about a particular key *value*: an
    /// engine that says yes here and then declines a probe for one value would
    /// make a per-key caller (`UNIQUE` enforcement) fall back once per row.
    /// Declining because the index went away under a concurrent `DROP INDEX` is
    /// the sanctioned reason, and it is sticky rather than per-value.
    ///
    /// A probe's *shape* is a different matter, and this cannot answer for it: a
    /// key an engine serves in full may be one it cannot serve as a prefix (see
    /// [`TableAm::index_lookup`] on unconstrained key columns). A caller that
    /// builds partial keys — the planner — applies that rule itself so it does
    /// not advertise a path the engine will decline.
    ///
    /// The default is `false` — no physical index — which the columnar engines
    /// and the read-only system catalogs inherit; the durable heap engine
    /// answers for a B-tree over key types `btkey` can encode.
    fn supports_index_scan(&self, _index_name: &str) -> bool {
        false
    }

    /// Probe the physical index `index_name` for the versions [`IndexProbeKey`]
    /// describes, yielding those visible to `txn`. Returns `None` when the
    /// engine has no physical index able to serve this probe (no such index, or
    /// a key type it cannot index) — the caller then falls back to a full
    /// [`TableAm::scan`] and re-checks the key itself, so an index scan stays
    /// correct on every engine.
    ///
    /// `Some` of an empty iterator is a different answer: the probe *was*
    /// served and nothing matched. A **NULL** anywhere in the key lands here:
    /// no row is indexed under a NULL and no comparison a bound expresses is
    /// ever true of one, so "served, no match" is the accurate answer, and only
    /// a caller for whom NULLs *do* collide (`UNIQUE NULLS NOT DISTINCT`) has
    /// to stay off the probe. A **non-NULL** value the engine cannot encode
    /// (its type does not match the key column) is the opposite case: the index
    /// says nothing about it, so the engine declines with `None` rather than
    /// reporting an empty result that would silently drop rows.
    ///
    /// A key column this probe leaves **unconstrained** is the same case again,
    /// and it is why a partial key is not always servable. An engine whose index
    /// omits rows with a NULL key column (the durable heap's B-tree does) cannot
    /// answer `eq = [1]` on an index over `(a, b)` while some row has `a = 1`
    /// and `b` NULL: that row satisfies the probe and is not in the index. Such
    /// an engine declines unless every unconstrained key column is `NOT NULL`.
    /// The constrained ones never need that check, since a NULL satisfies
    /// neither an equality nor a bound.
    ///
    /// A range's bounds are **prefix-wise**, which is what a composite key
    /// requires: on an index over `(a, b, c)`, a probe with `eq = [1]` and an
    /// exclusive lower bound of `5` must exclude every row with `b = 5`
    /// whatever its `c`, even though such a row sorts after the bound. Stated
    /// per end, where a *matching* key is one whose column `eq.len()` holds the
    /// bound's value:
    ///
    /// * exclusive lower: past the bound, and not one of the matching keys;
    /// * inclusive lower: at or past the bound (the matching keys are past it);
    /// * inclusive upper: before the bound, or one of the matching keys;
    /// * exclusive upper: before the bound.
    ///
    /// A `DESC` key column reverses each bound's direction; that is the
    /// engine's business, not the caller's — bounds are always stated in value
    /// order.
    ///
    /// The bounds themselves are ordered the way the engine's index is, which
    /// for a byte-ordered index is not the way every collation orders text: an
    /// engine may store keys by their bytes, and under an ICU collation the
    /// stretch between two bounds is then not the set of rows the comparison
    /// selects. Bounding a collatable column is therefore the **caller's** to
    /// restrict to byte-order collations
    /// (`crabgresql_types::collation::is_byte_order`); equality is exempt,
    /// since every supported collation is deterministic.
    ///
    /// The default is `None`, which the columnar engines and the read-only
    /// system catalogs inherit; the durable heap engine serves this from its
    /// B-tree.
    fn index_lookup(
        &self,
        _index_name: &str,
        _key: &IndexProbeKey<'_>,
        _txn: &TxnContext,
    ) -> Option<IndexProbe> {
        None
    }

    /// [`TableAm::index_lookup`] with each version's MVCC header attached — the
    /// probe's counterpart to [`TableAm::scan_with_system`], with the same
    /// contract: `None` declines the probe and sends the caller to a scan.
    ///
    /// An engine that answers [`TableAm::supports_system_columns`] must answer
    /// this wherever it answers `index_lookup`, or a statement reading `xmin`
    /// off an index-probed relation has nowhere to get it. Declining here is
    /// still *correct* — the caller falls back to a scan — so the cost of
    /// forgetting is a plan, not an error, which is why the promise is stated
    /// rather than enforced.
    fn index_lookup_with_system(
        &self,
        _index_name: &str,
        _key: &IndexProbeKey<'_>,
        _txn: &TxnContext,
    ) -> Option<SystemTupleStream> {
        None
    }

    /// Insert a new version stamped with `txn`'s XID. The tuple must have
    /// exactly `schema().columns.len()` values in schema order — executors index
    /// tuples by schema position and rely on this.
    fn insert(&self, tuple: Tuple, txn: &TxnContext) -> Result<Tid, StorageError>;

    /// Insert a statement's complete tuple batch. Engines with a columnar write
    /// path override this to build one or more fragments rather than one file per
    /// tuple.
    fn insert_many(&self, tuples: Vec<Tuple>, txn: &TxnContext) -> Result<Vec<Tid>, StorageError> {
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
        Ok(self
            .update_many_tids(updates, txn)?
            .iter()
            .filter(|tid| tid.is_some())
            .count() as u64)
    }

    /// [`TableAm::update_many`] reporting where each new version landed, in
    /// input order: `None` for a row that was gone by the time it was reached.
    ///
    /// Separate from `update_many` because the count is all a plain `UPDATE`
    /// needs, and an engine that batches its writes has nothing to gain from
    /// materializing the tids. Only `RETURNING ctid` reaches this, and only
    /// through the row-at-a-time path.
    fn update_many_tids(
        &self,
        updates: Vec<(Tid, Tuple)>,
        txn: &TxnContext,
    ) -> Result<Vec<Option<Tid>>, StorageError> {
        updates
            .into_iter()
            .map(|(tid, tuple)| {
                Ok(match self.update(tid, tuple, txn)? {
                    UpdateResult::Updated(new) => Some(new),
                    _ => None,
                })
            })
            .collect()
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

    /// Whether transaction `xid` has truncated this table and not yet ended, so
    /// its rows live in a replacement file that an abort will throw away whole.
    ///
    /// This is the precondition for `COPY … FREEZE`: a frozen row is visible to
    /// everyone the instant it is written and carries no XID whose abort could
    /// hide it again, so freezing is only safe where a rollback discards the
    /// storage itself. Engines whose TRUNCATE is not a discardable swap answer
    /// `false` — the default — and callers then refuse to freeze.
    fn truncated_by(&self, _xid: Xid) -> bool {
        false
    }

    /// Reclaim versions dead to every transaction at or before `oldest`. A
    /// version is reclaimable only if its deleter **committed** — `clog` decides
    /// that; a version stamped by an aborted or in-flight deleter is still live.
    /// The default is a no-op, for an access method with nothing to reclaim: a
    /// read-only catalog, or an append-only Parquet relation whose fragments are
    /// immutable.
    ///
    /// TODO: reclaim dead versions on a schedule (autovacuum) — a heap relation
    /// is vacuumed only when a `VACUUM` statement asks for it.
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

    /// Block until the engine is willing to accept more writes.
    ///
    /// An engine that acknowledges writes into RAM before making them durable
    /// needs somewhere to say "not yet": without it, a writer that outruns the
    /// background flush is bounded by nothing but the machine's memory. The
    /// default is a no-op — an engine that writes through has no such window.
    ///
    /// **Must be called before the statement's transaction is built**, with no
    /// XID allocated and no snapshot registered. An engine's remedy for
    /// pressure is to reclaim rows nothing can still see, and a caller waiting
    /// here while holding either one is part of the horizon that decides what
    /// "nothing can still see" means — it would be waiting for a condition its
    /// own waiting prevents.
    fn await_write_capacity(&self) {}

    /// How this engine's buffer pool answered the block requests it served, for
    /// `pg_stat_database.blks_hit`/`blks_read`. `None` from an engine with no
    /// pool: a relation held in RAM reads no block, and counting its accesses
    /// as hits would report a cache that is not there.
    ///
    /// Database-wide rather than per-relation because that is all the pool
    /// knows — it counts pins, and a pin names a `RelFileNode`. This build
    /// serves one database, so the totals *are* that database's, which is why
    /// `pg_statio_*` stays empty while these two columns are live.
    fn buffer_stats(&self) -> Option<BufferStats> {
        None
    }

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

    /// Mark `columns` (positions into the relation's column list) NOT NULL,
    /// durably — what `ALTER TABLE ... ADD PRIMARY KEY` does to its key columns.
    /// The caller has already scanned the table and found no NULL there.
    ///
    /// The whole key goes in one call so the engine can make it durable as one
    /// write: a crash leaves the key entirely NOT NULL or untouched, never half.
    ///
    /// The default rejects, deliberately: an engine that cannot record this must
    /// fail the statement loudly rather than let a PRIMARY KEY land on columns
    /// that still accept NULL. It reports the refusal as *unsupported* rather
    /// than as a missing relation — the relation is right there, this engine
    /// just cannot alter it, and a fabricated `TableNotFound` would surface as
    /// `relation does not exist` for something the caller just opened.
    fn set_column_not_null(
        &self,
        _namespace: &str,
        _table: &str,
        _columns: &[usize],
    ) -> Result<(), StorageError> {
        Err(StorageError::UnsupportedOperation(
            "this engine does not support a NOT NULL column constraint".to_string(),
        ))
    }

    /// Append a `CHECK` constraint to `table`, durably — what
    /// `ALTER TABLE ... ADD CONSTRAINT ... CHECK` does. The caller has already
    /// bound the predicate, resolved its name, and scanned the table's existing
    /// rows against it.
    ///
    /// The default rejects for the same reason [`Self::set_column_not_null`]'s
    /// does: an engine that cannot record the constraint must fail loudly rather
    /// than accept DDL declaring a rule it will never enforce.
    fn add_check_constraint(
        &self,
        _namespace: &str,
        _table: &str,
        _check: CheckConstraint,
    ) -> Result<(), StorageError> {
        Err(StorageError::UnsupportedOperation(
            "this engine does not support CHECK constraints".to_string(),
        ))
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

    /// Every `INHERITS (...)` link in the engine as
    /// `((child_namespace, child_name), (parent_namespace, parent_name))`,
    /// without cloning full schemas.
    ///
    /// The binder asks this of **every** base relation a statement names, to
    /// learn whether the relation has descendants to fan out to — so it is on
    /// the path of every query, and the usual answer is "no links at all". That
    /// is why it exists separately from [`relations`](Self::relations): a schema
    /// deep-clone per relation per FROM item is a real cost to pay for reading
    /// one usually-empty vector, and [`relation_metadata`](Self::relation_metadata)
    /// is worse still — it stats every relation's files.
    ///
    /// The default derives from `relations`; an engine with a relation registry
    /// overrides it to read just the links.
    fn inheritance_links(&self) -> Vec<((String, String), (String, String))> {
        self.relations()
            .into_iter()
            .flat_map(|schema| {
                schema
                    .inherits
                    .iter()
                    .map(move |parent| {
                        (
                            (schema.namespace.clone(), schema.name.clone()),
                            (parent.namespace.clone(), parent.name.clone()),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
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
    fn analyze(&self, _namespace: &str, name: &str, _txn: &TxnContext) -> Result<(), StorageError> {
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
                toast: None,
                filenodes: RelationFilenodes::default(),
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

    /// A sequence's current counter as `(last_value, is_called)`, without
    /// advancing it, or `None` if there is no such sequence. Read-only
    /// counterpart to [`TableEngine::sequence_nextval`], for the catalog:
    /// `pg_sequences.last_value` reports it, and reporting it must not have the
    /// side effect of consuming a value. The default knows no sequences.
    fn sequence_current(&self, _namespace: &str, _name: &str) -> Option<(i64, bool)> {
        None
    }

    /// A sequence's `relfilenode`, or `0` when the engine assigns none.
    ///
    /// A sequence is a relation with storage in PostgreSQL, so `pg_class` has to
    /// report a file number for it like it does for a table. Ours keeps its
    /// counter in the relation catalog rather than in a one-page file, so the
    /// number names no file today; it is still allocated from the same monotonic
    /// counter as every table's, so it can never alias one.
    fn sequence_relfilenode(&self, _namespace: &str, _name: &str) -> u32 {
        0
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
    /// The declared parameter names, positionally aligned with `arg_types`;
    /// `None` where the declaration gave no name. A `LANGUAGE SQL` body may
    /// refer to its arguments by these names as well as by `$n`.
    pub arg_names: Vec<Option<String>>,
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

    /// Called once for each user routine a call resolves to **directly**.
    ///
    /// The default does nothing; only a caller that wants to know what an
    /// expression depends on — `DROP FUNCTION`, deciding whether a stored
    /// default or CHECK still needs the routine — wraps a catalog to record it.
    ///
    /// "Directly" is the whole contract, and it mirrors PostgreSQL: a routine
    /// reached through an *inlined* SQL body is not a dependency there either
    /// (dropping a function that an inlined body calls succeeds, and the
    /// breakage surfaces at the next call as 42883). Reporting it is also the
    /// only way to see a `LANGUAGE SQL` routine at all — it is inlined during
    /// binding, so no OID survives in the bound tree to walk for.
    fn note_routine_use(&self, _oid: u32) {}

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
        assert_eq!(
            ColumnProjection::of([], &schema(&[])),
            ColumnProjection::All
        );
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
