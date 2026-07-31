//! `pg_catalog` and `information_schema` system catalogs, served as read-only
//! relations.
//!
//! The core seam: a bound `SELECT` lowers to a scan over an `Arc<dyn TableAm>`,
//! and the executor treats every access method alike. So each supported
//! `pg_catalog` relation is materialized as a [`StaticTable`] (rows built from
//! codegen'd built-in data plus, in later slices, live server state) and handed
//! to the same pipeline user tables use — no bespoke executor node.
//!
//! [`SystemCatalog`] implements [`TableEngine`] so the server's session catalog
//! can layer it into name resolution: `pg_catalog.<rel>` routes here directly,
//! and an unqualified name falls back here (pg_catalog is implicitly on the
//! search path).
//!
//! # Fidelity & clean-room
//!
//! Built-in rows are generated at build time from PostgreSQL's vendored catalog
//! `.dat` *data* (`vendor/postgres/catalog/`), never from its C/Perl source; see
//! `build.rs` and `AGENTS.md`. Column coverage is a curated, PG-ordered subset
//! keyed by the names real clients query; several catalog-only types are
//! represented pragmatically (`oid` is real; `"char"`/`regproc` render as
//! `text`). Full column/type parity with upstream `type_sanity`/`\d` is a
//! follow-up.

mod schema;

pub use schema::PLPGSQL_LANG_OID;
mod static_table;

use std::sync::{Arc, OnceLock};

use crabgresql_storage_api::{
    Column, IndexMetadata, RelPersistence, RelStats, RelationMetadata, StorageError, TableAm,
    TableEngine, TableSchema,
};
use crabgresql_types::{PgType, Value};

pub use static_table::StaticTable;

/// First OID handed to a synthetic user relation in `pg_class`. Runtime type,
/// function, and cast OIDs grow upward from PostgreSQL's user-object floor, so
/// relations use a separate high partition until storage owns persistent OIDs.
/// This preserves catalog-wide uniqueness in every reflected snapshot.
const FIRST_REL_OID: u32 = 0x4000_0000;

/// The namespace PostgreSQL keeps TOAST relations in. Never on the search path,
/// so nothing here is reachable by an unqualified name.
const TOAST_NAMESPACE: &str = "pg_toast";

#[derive(Clone)]
struct CatalogIndex {
    oid: u32,
    table_oid: u32,
    table_schema: TableSchema,
    metadata: IndexMetadata,
}

/// A table's out-of-line ("TOAST") relation, as `pg_class` publishes it.
///
/// PostgreSQL puts one of these in the `pg_toast` namespace for every table with
/// a varlena column and points the table's `reltoastrelid` at it. Ours is
/// created lazily — only once a row actually needs it — so a table of narrow
/// columns keeps `reltoastrelid = 0`, which is also legitimate PostgreSQL state
/// (it is what `\d` reports for a table with no TOAST relation).
struct CatalogToast {
    oid: u32,
    /// The `pg_class` OID of the table this belongs to, whose `reltoastrelid`
    /// names `oid`.
    table_oid: u32,
    /// PostgreSQL's name for it: `pg_toast_<parent oid>`.
    name: String,
    /// Mirrors the parent's, as PostgreSQL's does.
    persistence: RelPersistence,
    stats: RelStats,
}

/// A built-in `pg_type` row, generated from `pg_type.dat`. Field types mirror
/// the runtime column types in [`schema::pg_type_schema`]; string fields are the
/// catalog `name`/`"char"`/`regproc` text.
pub struct PgTypeRow {
    pub oid: u32,
    pub typname: &'static str,
    pub typnamespace: u32,
    pub typowner: u32,
    pub typlen: i16,
    pub typbyval: bool,
    pub typtype: &'static str,
    pub typcategory: &'static str,
    pub typispreferred: bool,
    pub typisdefined: bool,
    pub typdelim: &'static str,
    pub typrelid: u32,
    pub typelem: u32,
    pub typarray: u32,
    pub typinput: &'static str,
    pub typoutput: &'static str,
    pub typreceive: &'static str,
    pub typsend: &'static str,
    pub typalign: &'static str,
    pub typstorage: &'static str,
}

/// A built-in `pg_cast` row, generated from `pg_cast.dat`. `castsource`/
/// `casttarget` are resolved type OIDs; `castfunc` is the upstream text
/// reference (a `regprocedure`), not yet a resolved OID.
pub struct PgCastRow {
    pub oid: u32,
    pub castsource: u32,
    pub casttarget: u32,
    pub castfunc: &'static str,
    pub castcontext: &'static str,
    pub castmethod: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/pg_type_rows.rs"));
include!(concat!(env!("OUT_DIR"), "/pg_cast_rows.rs"));

/// Whether `name` is the catalog name of a PostgreSQL built-in type, including
/// types crabgresql recognizes but does not implement yet (for example
/// `xml`). This distinguishes an unsupported built-in from a nonexistent
/// user type without maintaining a second hand-written name list.
pub fn is_builtin_type_name(name: &str) -> bool {
    PG_TYPE_ROWS.iter().any(|row| row.typname == name)
}

/// PostgreSQL's fixed OIDs for the `pg_catalog` relations this build serves.
///
/// Catalog relations are not reflected into `pg_class` (only live user
/// relations are), so they have no OID from the positional assignment
/// [`SystemCatalog::relation_oids`] hands out. They still need one: a client
/// identifies a relation by casting its name — `'pg_class'::regclass` — and
/// expects the OID back to render as the name again. These are PostgreSQL's own
/// assignments (probed from `pg_class` on 18.4) rather than invented values, so
/// an OID a client hard-codes means the same thing here.
///
/// `builtin_relation_oid_is_complete` keeps this in step with what
/// `build_pg_catalog` actually serves.
const BUILTIN_RELATION_OIDS: &[(&str, u32)] = &[
    ("pg_type", 1247),
    ("pg_attribute", 1249),
    ("pg_proc", 1255),
    ("pg_class", 1259),
    ("pg_sequence", 2224),
    ("pg_am", 2601),
    ("pg_attrdef", 2604),
    ("pg_cast", 2605),
    ("pg_constraint", 2606),
    ("pg_index", 2610),
    ("pg_inherits", 2611),
    ("pg_language", 2612),
    ("pg_namespace", 2615),
    ("pg_partitioned_table", 3350),
    ("pg_collation", 3456),
    ("pg_enum", 3501),
    // A view defined by initdb rather than a bootstrap catalog, so its OID comes
    // from the auto-assigned band. Deterministic for a given major version, and
    // probed from the same 18.4 as the rest.
    ("pg_cursors", 12077),
];

/// The fixed OID of the `pg_catalog` relation `name`, if this build serves one.
pub fn builtin_relation_oid(name: &str) -> Option<u32> {
    BUILTIN_RELATION_OIDS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, oid)| *oid)
}

/// The inverse of [`builtin_relation_oid`]: the `pg_catalog` relation `oid`
/// names, if it is one of the fixed assignments.
pub fn builtin_relation_name(oid: u32) -> Option<&'static str> {
    BUILTIN_RELATION_OIDS
        .iter()
        .find(|(_, o)| *o == oid)
        .map(|(name, _)| *name)
}

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogSequence {
    pub type_oid: u32,
    pub start: i64,
    pub increment: i64,
    pub min: i64,
    pub max: i64,
    pub cache: i64,
    pub cycle: bool,
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
        }
    }

    /// A permanent view. Views have no indexes; its namespace rides on `schema`.
    pub fn view(schema: TableSchema) -> Self {
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
        }
    }
}

/// A user-defined type reflected into `pg_type` (and, for enums, `pg_enum`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogUserType {
    pub oid: u32,
    pub name: String,
    /// The enum labels in definition (= sort) order, or `None` for a non-enum
    /// user type (which is not reflected into `pg_type`/`pg_enum` yet).
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

/// Produces the live user relations to reflect into `pg_class`/`pg_attribute`
/// and `information_schema`.
/// Boxed so it can capture the server engines and run lazily — only a query that
/// actually opens `pg_class`/`pg_attribute` pays the cost of enumerating them.
type RelationsFn = Box<dyn Fn() -> Vec<CatalogRelation> + Send + Sync>;

/// Produces the user-defined types to reflect into `pg_type`/`pg_enum`. Boxed and
/// lazy like [`RelationsFn`] — only a query that opens `pg_type`/`pg_enum` pays.
type UserTypesFn = Box<dyn Fn() -> Vec<CatalogUserType> + Send + Sync>;

/// Produces the user-defined routines to reflect into `pg_proc`. Boxed and
/// lazy for the same reason as the others: only a query that opens `pg_proc`
/// pays for enumerating them.
type RoutinesFn = Box<dyn Fn() -> Vec<CatalogRoutine> + Send + Sync>;

/// Produces the user-created schemas (`CREATE SCHEMA`) as `(name, oid)`, to
/// reflect into `pg_namespace` and `information_schema.schemata`. Boxed and lazy
/// like [`RelationsFn`].
type SchemasFn = Box<dyn Fn() -> Vec<(String, u32)> + Send + Sync>;

/// Produces the session's open SQL cursors, to reflect into `pg_cursors`. Boxed
/// and lazy like [`RelationsFn`].
type CursorsFn = Box<dyn Fn() -> Vec<CatalogCursor> + Send + Sync>;

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
}

/// Read-only engine serving `pg_catalog` relations. Constructed per statement so
/// its rows reflect current server state; live user relations are supplied by a
/// closure that is invoked at most once (and only when `pg_class`/`pg_attribute`
/// is opened), memoized in `oids`.
pub struct SystemCatalog {
    relations: RelationsFn,
    user_types_fn: UserTypesFn,
    routines_fn: RoutinesFn,
    schemas_fn: SchemasFn,
    cursors_fn: CursorsFn,
    database: String,
    owner: String,
    live_relations: OnceLock<Vec<CatalogRelation>>,
    oids: OnceLock<Vec<(u32, TableSchema)>>,
    kinds: OnceLock<Vec<RelKind>>,
    stats: OnceLock<Vec<RelStats>>,
    index_oids: OnceLock<Vec<CatalogIndex>>,
    toast_oids: OnceLock<Vec<CatalogToast>>,
    user_types: OnceLock<Vec<CatalogUserType>>,
    routines: OnceLock<Vec<CatalogRoutine>>,
    user_schemas: OnceLock<Vec<(String, u32)>>,
    cursors: OnceLock<Vec<CatalogCursor>>,
}

impl Default for SystemCatalog {
    fn default() -> Self {
        Self::with_relations_fn(Vec::new)
    }
}

impl SystemCatalog {
    /// A catalog with no live relations (built-in metadata only).
    pub fn new() -> Self {
        Self::default()
    }

    /// A catalog reflecting a fixed set of live user relations into
    /// `pg_class`/`pg_attribute`.
    pub fn with_relations(relations: Vec<TableSchema>) -> Self {
        Self::with_relations_fn(move || relations.clone())
    }

    /// A catalog that enumerates its live user relations lazily via `f` (invoked
    /// at most once, only if a relation-backed catalog is opened).
    pub fn with_relations_fn(f: impl Fn() -> Vec<TableSchema> + Send + Sync + 'static) -> Self {
        Self::with_catalog_relations_fn("postgres", "postgres", move || {
            f().into_iter().map(CatalogRelation::permanent).collect()
        })
    }

    /// A catalog with session identity and live relation metadata for the
    /// information schema. The callback is memoized per catalog snapshot.
    pub fn with_catalog_relations_fn(
        database: impl Into<String>,
        owner: impl Into<String>,
        f: impl Fn() -> Vec<CatalogRelation> + Send + Sync + 'static,
    ) -> Self {
        Self {
            relations: Box::new(f),
            user_types_fn: Box::new(Vec::new),
            routines_fn: Box::new(Vec::new),
            schemas_fn: Box::new(Vec::new),
            cursors_fn: Box::new(Vec::new),
            database: database.into(),
            owner: owner.into(),
            live_relations: OnceLock::new(),
            oids: OnceLock::new(),
            kinds: OnceLock::new(),
            stats: OnceLock::new(),
            index_oids: OnceLock::new(),
            toast_oids: OnceLock::new(),
            user_types: OnceLock::new(),
            routines: OnceLock::new(),
            user_schemas: OnceLock::new(),
            cursors: OnceLock::new(),
        }
    }

    /// Attach a provider of user-defined types to reflect into `pg_type`/`pg_enum`
    /// (invoked at most once, only if one of those relations is opened).
    pub fn with_user_types_fn(
        mut self,
        f: impl Fn() -> Vec<CatalogUserType> + Send + Sync + 'static,
    ) -> Self {
        self.user_types_fn = Box::new(f);
        self
    }

    /// Attach a provider of user-defined routines to reflect into `pg_proc`
    /// (invoked at most once, only if `pg_proc` is opened).
    pub fn with_routines_fn(
        mut self,
        f: impl Fn() -> Vec<CatalogRoutine> + Send + Sync + 'static,
    ) -> Self {
        self.routines_fn = Box::new(f);
        self
    }

    /// Attach a provider of user-created schemas to reflect into `pg_namespace`
    /// and `information_schema.schemata` (invoked at most once, only if one of
    /// those relations is opened).
    pub fn with_schemas_fn(
        mut self,
        f: impl Fn() -> Vec<(String, u32)> + Send + Sync + 'static,
    ) -> Self {
        self.schemas_fn = Box::new(f);
        self
    }

    /// Attach a provider of the session's open SQL cursors to reflect into
    /// `pg_cursors` (invoked at most once, only if that relation is opened).
    pub fn with_cursors_fn(
        mut self,
        f: impl Fn() -> Vec<CatalogCursor> + Send + Sync + 'static,
    ) -> Self {
        self.cursors_fn = Box::new(f);
        self
    }

    fn live_relations(&self) -> &[CatalogRelation] {
        self.live_relations.get_or_init(|| (self.relations)())
    }

    fn user_types(&self) -> &[CatalogUserType] {
        self.user_types.get_or_init(|| (self.user_types_fn)())
    }

    fn routines(&self) -> &[CatalogRoutine] {
        self.routines.get_or_init(|| (self.routines_fn)())
    }

    fn user_schemas(&self) -> &[(String, u32)] {
        self.user_schemas.get_or_init(|| (self.schemas_fn)())
    }

    fn cursors(&self) -> &[CatalogCursor] {
        self.cursors.get_or_init(|| (self.cursors_fn)())
    }

    /// Map every namespace name to its OID: the built-in namespaces plus each
    /// user-created schema. Feeds `pg_class.relnamespace` /
    /// `pg_constraint.connamespace`.
    fn namespace_oids(&self) -> std::collections::HashMap<String, u32> {
        let mut map = std::collections::HashMap::new();
        map.insert("pg_catalog".to_string(), 11);
        map.insert(TOAST_NAMESPACE.to_string(), 99);
        map.insert("public".to_string(), 2200);
        for (name, oid) in self.user_schemas() {
            map.insert(name.clone(), *oid);
        }
        map
    }

    /// Assign a stable synthetic OID to each user relation, computed once and
    /// memoized. Sorted by name so `pg_class` and `pg_attribute` (built by
    /// separate `open_table` calls) agree on every relation's OID, keeping their
    /// join consistent.
    fn relation_oids(&self) -> &[(u32, TableSchema)] {
        self.oids.get_or_init(|| {
            let mut rels = self.live_relations().to_vec();
            rels.sort_by(|a, b| {
                a.namespace
                    .cmp(&b.namespace)
                    .then_with(|| a.schema.name.cmp(&b.schema.name))
            });
            rels.into_iter()
                .enumerate()
                .map(|(i, r)| (FIRST_REL_OID + i as u32, r.schema))
                .collect()
        })
    }

    /// The relation kind for each entry of [`SystemCatalog::relation_oids`], in
    /// the same sorted order, so `pg_class` can emit the right `relkind`.
    fn relation_kinds(&self) -> &[RelKind] {
        self.kinds.get_or_init(|| {
            let mut rels = self.live_relations().to_vec();
            rels.sort_by(|a, b| {
                a.namespace
                    .cmp(&b.namespace)
                    .then_with(|| a.schema.name.cmp(&b.schema.name))
            });
            rels.into_iter().map(|r| r.kind).collect()
        })
    }

    /// The size estimates for each entry of [`SystemCatalog::relation_oids`], in
    /// the same sorted order, feeding `pg_class.relpages`/`reltuples`.
    fn relation_stats(&self) -> &[RelStats] {
        self.stats.get_or_init(|| {
            let mut rels = self.live_relations().to_vec();
            rels.sort_by(|a, b| {
                a.namespace
                    .cmp(&b.namespace)
                    .then_with(|| a.schema.name.cmp(&b.schema.name))
            });
            rels.into_iter().map(|r| r.stats).collect()
        })
    }

    /// The `(pg_class OID, params)` of each sequence, for `pg_sequence`. The OID
    /// matches the one [`SystemCatalog::relation_oids`] assigns (same sort), so
    /// `pg_sequence.seqrelid` joins `pg_class.oid`.
    fn sequence_entries(&self) -> Vec<(u32, CatalogSequence)> {
        let mut rels = self.live_relations().to_vec();
        rels.sort_by(|a, b| {
            a.namespace
                .cmp(&b.namespace)
                .then_with(|| a.schema.name.cmp(&b.schema.name))
        });
        rels.into_iter()
            .enumerate()
            .filter_map(|(i, r)| r.sequence.map(|s| (FIRST_REL_OID + i as u32, s)))
            .collect()
    }

    fn index_oids(&self) -> &[CatalogIndex] {
        self.index_oids.get_or_init(|| {
            let mut relations = self.live_relations().to_vec();
            relations.sort_by(|a, b| {
                a.namespace
                    .cmp(&b.namespace)
                    .then_with(|| a.schema.name.cmp(&b.schema.name))
            });
            let first_index_oid = FIRST_REL_OID + relations.len() as u32;
            let mut pending = Vec::new();
            for (position, relation) in relations.into_iter().enumerate() {
                let table_oid = FIRST_REL_OID + position as u32;
                for index in relation.indexes {
                    pending.push((table_oid, relation.schema.clone(), index));
                }
            }
            pending.sort_by(|a, b| a.2.name.cmp(&b.2.name));
            pending
                .into_iter()
                .enumerate()
                .map(
                    |(position, (table_oid, table_schema, metadata))| CatalogIndex {
                        oid: first_index_oid + position as u32,
                        table_oid,
                        table_schema,
                        metadata,
                    },
                )
                .collect()
        })
    }

    /// The TOAST relations of this snapshot, assigned OIDs from a **third block**
    /// that begins after the index block.
    ///
    /// Ordering matters: relations occupy `[FIRST_REL_OID, +nrels)` and indexes
    /// the range straight after, both keyed by position in a sorted list. Putting
    /// TOAST relations last means adding them shifted no OID that already
    /// existed. They are deliberately absent from
    /// [`SystemCatalog::live_relations`], so they never enter unqualified name
    /// resolution or `pg_table_is_visible` — matching PostgreSQL, where
    /// `pg_toast` is never on the search path.
    fn toast_oids(&self) -> &[CatalogToast] {
        self.toast_oids.get_or_init(|| {
            let mut relations = self.live_relations().to_vec();
            relations.sort_by(|a, b| {
                a.namespace
                    .cmp(&b.namespace)
                    .then_with(|| a.schema.name.cmp(&b.schema.name))
            });
            let first_toast_oid =
                FIRST_REL_OID + relations.len() as u32 + self.index_oids().len() as u32;
            relations
                .into_iter()
                .enumerate()
                .filter_map(|(position, relation)| {
                    let table_oid = FIRST_REL_OID + position as u32;
                    relation
                        .toast
                        .map(|stats| (table_oid, relation.schema, stats))
                })
                .enumerate()
                .map(|(slot, (table_oid, schema, stats))| CatalogToast {
                    oid: first_toast_oid + slot as u32,
                    table_oid,
                    // PostgreSQL names it after the parent's OID.
                    name: format!("pg_toast_{table_oid}"),
                    persistence: schema.persistence,
                    stats,
                })
                .collect()
        })
    }

    /// The name of the role `oid` identifies, or `None` if no role has that OID.
    /// Every catalog row reports [`schema::BOOTSTRAP_ROLE_OID`] as its owner
    /// (crabgresql has no role catalog), so exactly one OID resolves — to this
    /// snapshot's session user. Backs `pg_get_userbyid`.
    pub fn role_name(&self, oid: u32) -> Option<&str> {
        (oid == schema::BOOTSTRAP_ROLE_OID).then_some(self.owner.as_str())
    }

    /// The `(namespace, name)` of the relation `oid` identifies — a
    /// table/view/sequence or an index — or `None` if this snapshot has no such
    /// relation. Backs `pg_table_is_visible`, which needs the name as well as
    /// the namespace to answer *reachability* rather than mere membership.
    ///
    /// Reads exactly the fields [`schema::pg_class_rows`] reports as
    /// `relnamespace`/`relname`: `schema.namespace` for a relation and
    /// `table_schema.namespace` for an index. Note that `schema.namespace` is a
    /// distinct field from [`CatalogRelation::namespace`]; they agree for temp
    /// relations only because the server selects those on `schema.namespace`.
    /// Diverging here would make a relation `pg_class` lists invisible.
    ///
    /// OIDs are assigned positionally by [`SystemCatalog::relation_oids`] and
    /// [`SystemCatalog::index_oids`], so this indexes rather than scans — it is
    /// called once per row when a `\d` listing filters on
    /// `pg_table_is_visible`, which a linear scan would make quadratic. The
    /// stored OID is still compared, so a future non-positional assignment
    /// degrades to "not found" rather than to a wrong answer.
    pub fn relation_ref(&self, oid: u32) -> Option<(&str, &str)> {
        // Below the synthetic floor no relation can match, and answering here
        // avoids forcing the lazy relation enumeration for an unrelated OID
        // (`SELECT pg_table_is_visible(1)` should not enumerate the database).
        let offset = oid.checked_sub(FIRST_REL_OID)? as usize;
        let relations = self.relation_oids();
        if let Some((stored, schema)) = relations.get(offset) {
            return (*stored == oid).then_some((schema.namespace.as_str(), schema.name.as_str()));
        }
        let indexes = self.index_oids();
        if let Some(index) = indexes.get(offset - relations.len()) {
            return (index.oid == oid).then_some((
                index.table_schema.namespace.as_str(),
                index.metadata.name.as_str(),
            ));
        }
        let toast = self
            .toast_oids()
            .get(offset - relations.len() - indexes.len())?;
        (toast.oid == oid).then_some((TOAST_NAMESPACE, toast.name.as_str()))
    }

    /// The OID of `namespace.name` in this snapshot, or `None` if it holds no
    /// such relation. The inverse of [`SystemCatalog::relation_ref`]; feeds the
    /// shadowing check in `pg_table_is_visible`.
    pub fn relation_oid_in(&self, namespace: &str, name: &str) -> Option<u32> {
        let relation = self
            .relation_oids()
            .iter()
            .find(|(_, schema)| schema.namespace == namespace && schema.name == name);
        if let Some((oid, _)) = relation {
            return Some(*oid);
        }
        self.index_oids()
            .iter()
            .find(|index| index.table_schema.namespace == namespace && index.metadata.name == name)
            .map(|index| index.oid)
    }

    /// Whether `name` is a `pg_catalog` relation this catalog serves. Cheap for
    /// a name that is not one (the builders run only on a match), which is the
    /// common case — it is consulted per relation when deciding visibility.
    pub fn has_catalog_relation(&self, name: &str) -> bool {
        self.build_pg_catalog(name).is_some()
    }

    /// The name of the schema `oid` identifies, or `None` if this snapshot has
    /// no such schema. The inverse of [`SystemCatalog::namespace_oid`]; both back
    /// `regnamespace`, and both read the same table `pg_namespace` is built from.
    pub fn namespace_name(&self, oid: u32) -> Option<String> {
        self.namespace_oids()
            .into_iter()
            .find(|(_, o)| *o == oid)
            .map(|(name, _)| name)
    }

    pub fn namespace_oid(&self, name: &str) -> Option<u32> {
        self.namespace_oids().get(name).copied()
    }

    /// The `(namespace, name)` of the user type `oid` identifies, or `None` for
    /// an OID no `CREATE TYPE` has. Built-in types are not here — they resolve
    /// without a catalog. User types carry no namespace of their own yet
    /// (`CREATE TYPE app.t` is unsupported), so they all report `public`, which
    /// is where an unqualified name finds them.
    pub fn user_type_ref(&self, oid: u32) -> Option<(&str, &str)> {
        self.user_types()
            .iter()
            .find(|t| t.oid == oid)
            .map(|t| ("public", t.name.as_str()))
    }

    /// The OID of the user type `namespace.name` names. A qualifier other than
    /// `public` matches nothing, for the reason above.
    pub fn user_type_oid(&self, namespace: Option<&str>, name: &str) -> Option<u32> {
        if matches!(namespace, Some(ns) if ns != "public") {
            return None;
        }
        self.user_types()
            .iter()
            .find(|t| t.name == name)
            .map(|t| t.oid)
    }

    /// Build the requested relation's rows + schema, or `None` if unknown.
    fn build_pg_catalog(&self, name: &str) -> Option<(TableSchema, Vec<Vec<Value>>)> {
        match name {
            "pg_type" => {
                let mut rows = schema::pg_type_builtin_rows();
                rows.extend(schema::pg_type_user_rows(self.user_types()));
                Some((schema::pg_type_schema(), rows))
            }
            "pg_enum" => Some((
                schema::pg_enum_schema(),
                schema::pg_enum_rows(self.user_types()),
            )),
            "pg_namespace" => Some((
                schema::pg_namespace_schema(),
                schema::pg_namespace_rows(self.user_schemas()),
            )),
            "pg_class" => Some((
                schema::pg_class_schema(),
                schema::pg_class_rows(
                    self.relation_oids(),
                    self.relation_kinds(),
                    self.relation_stats(),
                    self.index_oids(),
                    self.toast_oids(),
                    &self.namespace_oids(),
                ),
            )),
            "pg_attribute" => Some((
                schema::pg_attribute_schema(),
                schema::pg_attribute_rows(
                    self.relation_oids(),
                    self.index_oids(),
                    self.toast_oids(),
                ),
            )),
            "pg_attrdef" => Some((
                schema::pg_attrdef_schema(),
                schema::pg_attrdef_rows(self.relation_oids()),
            )),
            "pg_constraint" => Some((
                schema::pg_constraint_schema(),
                schema::pg_constraint_rows(
                    self.relation_oids(),
                    self.index_oids(),
                    &self.namespace_oids(),
                ),
            )),
            "pg_index" => Some((
                schema::pg_index_schema(),
                schema::pg_index_rows(self.index_oids()),
            )),
            "pg_am" => Some((schema::pg_am_schema(), schema::pg_am_rows())),
            "pg_cursors" => Some((
                schema::pg_cursors_schema(),
                schema::pg_cursors_rows(self.cursors()),
            )),
            "pg_language" => Some((schema::pg_language_schema(), schema::pg_language_rows())),
            "pg_proc" => Some((
                schema::pg_proc_schema(),
                schema::pg_proc_rows(self.routines(), &self.namespace_oids()),
            )),
            "pg_cast" => Some((schema::pg_cast_schema(), schema::pg_cast_rows())),
            "pg_collation" => Some((schema::pg_collation_schema(), schema::pg_collation_rows())),
            "pg_inherits" => Some((
                schema::pg_inherits_schema(),
                schema::pg_inherits_rows(self.relation_oids()),
            )),
            "pg_partitioned_table" => Some((
                schema::pg_partitioned_table_schema(),
                schema::pg_partitioned_table_rows(self.relation_oids()),
            )),
            "pg_sequence" => Some((
                schema::pg_sequence_schema(),
                schema::pg_sequence_rows(&self.sequence_entries()),
            )),
            _ => None,
        }
    }

    fn build_information_schema(&self, name: &str) -> Option<(TableSchema, Vec<Vec<Value>>)> {
        match name {
            "schemata" => Some((
                schema::information_schema_schemata_schema(),
                schema::information_schema_schemata_rows(
                    &self.database,
                    &self.owner,
                    self.live_relations(),
                    self.user_schemas(),
                ),
            )),
            "tables" => Some((
                schema::information_schema_tables_schema(),
                schema::information_schema_tables_rows(&self.database, self.live_relations()),
            )),
            "columns" => Some((
                schema::information_schema_columns_schema(),
                schema::information_schema_columns_rows(&self.database, self.live_relations()),
            )),
            _ => None,
        }
    }
}

impl TableEngine for SystemCatalog {
    fn create_table(&self, schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError> {
        // The session catalog never routes CREATE here (DDL targets user data).
        unreachable!(
            "cannot create relation \"{}\" in the system catalog",
            schema.name
        )
    }

    fn open_table(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        match self.build_pg_catalog(name) {
            Some((schema, rows)) => Ok(StaticTable::arc(schema, rows)),
            None => Err(StorageError::TableNotFound(name.to_string())),
        }
    }

    fn resolve(
        &self,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Arc<dyn TableAm>, StorageError> {
        let relation = match namespace {
            None | Some("pg_catalog") => self.build_pg_catalog(name),
            Some("information_schema") => self.build_information_schema(name),
            Some(_) => None,
        };
        match relation {
            Some((schema, rows)) => Ok(StaticTable::arc(schema, rows)),
            None => Err(StorageError::TableNotFound(name.to_string())),
        }
    }

    fn drop_table(&self, _namespace: &str, name: &str) -> Result<(), StorageError> {
        // The session catalog routes DROP through temp/global, never here; a
        // system catalog relation is not droppable.
        unreachable!("cannot drop relation \"{name}\" from the system catalog")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn required<T>(value: Option<T>, message: &str) -> anyhow::Result<T> {
        value.ok_or_else(|| anyhow::anyhow!(message.to_string()))
    }
    use crabgresql_types::Value;

    /// One column of a `pg_type` row, located by column name.
    fn type_col(row: &[Value], schema: &TableSchema, col: &str) -> Value {
        let i = schema.column_index(col).expect("column exists");
        row[i].clone()
    }

    #[test]
    fn pg_type_has_builtin_rows_with_pg_oids() {
        let schema = schema::pg_type_schema();
        let rows = schema::pg_type_builtin_rows();
        let by_name = |name: &str| {
            rows.iter()
                .find(|r| type_col(r, &schema, "typname") == Value::Text(name.to_string()))
                .unwrap_or_else(|| panic!("{name} row present"))
                .clone()
        };
        // Driver-critical OIDs must match PG exactly.
        assert_eq!(type_col(&by_name("int4"), &schema, "oid"), Value::Oid(23));
        assert_eq!(type_col(&by_name("text"), &schema, "oid"), Value::Oid(25));
        assert_eq!(type_col(&by_name("bool"), &schema, "oid"), Value::Oid(16));
        // Metadata columns carry through from pg_type.dat.
        assert_eq!(
            type_col(&by_name("int4"), &schema, "typlen"),
            Value::Int2(4)
        );
        assert_eq!(
            type_col(&by_name("bool"), &schema, "typinput"),
            Value::Text("boolin".to_string())
        );
        // Every row is full-width.
        assert!(rows.iter().all(|r| r.len() == schema.columns.len()));
    }

    #[test]
    fn built_in_name_lookup_includes_unimplemented_types() {
        assert!(is_builtin_type_name("int4"));
        assert!(is_builtin_type_name("point"));
        assert!(!is_builtin_type_name("definitely_not_a_pg_type"));
    }

    #[test]
    fn pg_class_and_pg_attribute_agree_on_relation_oids() -> anyhow::Result<()> {
        use crabgresql_storage_api::{Column, TableSchema};
        use crabgresql_types::PgType;

        let rels = vec![
            TableSchema::new("beta", vec![Column::new("x", PgType::Int4)]),
            TableSchema::new(
                "alpha",
                vec![
                    Column::new("id", PgType::Int4),
                    Column::new("label", PgType::Text),
                ],
            ),
        ];
        let cat = SystemCatalog::with_relations(rels);

        let class_schema = schema::pg_class_schema();
        let class = required(cat.build_pg_catalog("pg_class"), "pg_class is missing")?.1;
        let oid_of = |relname: &str| -> anyhow::Result<Value> {
            let i = required(
                class_schema.column_index("relname"),
                "relname column is missing",
            )?;
            let o = required(class_schema.column_index("oid"), "oid column is missing")?;
            required(
                class
                    .iter()
                    .find(|r| r[i] == Value::Text(relname.to_string()))
                    .map(|r| r[o].clone()),
                "relation row is missing",
            )
        };
        // Sorted by name → alpha gets the first OID, beta the next.
        assert_eq!(oid_of("alpha")?, Value::Oid(FIRST_REL_OID));
        assert_eq!(oid_of("beta")?, Value::Oid(FIRST_REL_OID + 1));

        // pg_attribute's attrelid must match pg_class.oid for the same relation.
        let attr_schema = schema::pg_attribute_schema();
        let attr = required(
            cat.build_pg_catalog("pg_attribute"),
            "pg_attribute is missing",
        )?
        .1;
        let arel = required(
            attr_schema.column_index("attrelid"),
            "attrelid column is missing",
        )?;
        let aname = required(
            attr_schema.column_index("attname"),
            "attname column is missing",
        )?;
        let anum = required(
            attr_schema.column_index("attnum"),
            "attnum column is missing",
        )?;
        let atypid = required(
            attr_schema.column_index("atttypid"),
            "atttypid column is missing",
        )?;
        // alpha has two columns, in declared order, tied to alpha's OID.
        let alpha_attrs: Vec<_> = attr
            .iter()
            .filter(|r| r[arel] == Value::Oid(FIRST_REL_OID))
            .collect();
        assert_eq!(alpha_attrs.len(), 2);
        assert_eq!(alpha_attrs[0][aname], Value::Text("id".to_string()));
        assert_eq!(alpha_attrs[0][anum], Value::Int2(1));
        assert_eq!(alpha_attrs[0][atypid], Value::Oid(23)); // int4
        assert_eq!(alpha_attrs[1][atypid], Value::Oid(25)); // text

        Ok(())
    }

    #[test]
    fn pg_type_rows_agree_with_pgtype_for_modeled_types() {
        use crabgresql_types::PgType;
        // Types crabgresql models: their .dat-generated pg_type row must agree
        // with the authoritative PgType::oid()/typlen() used everywhere else, or
        // a pg_attribute.atttypid -> pg_type.oid join silently mismatches.
        let modeled = [
            ("bool", PgType::Bool),
            ("int2", PgType::Int2),
            ("int4", PgType::Int4),
            ("int8", PgType::Int8),
            ("float4", PgType::Float4),
            ("float8", PgType::Float8),
            ("numeric", PgType::Numeric),
            ("text", PgType::Text),
            ("varchar", PgType::Varchar),
            ("bpchar", PgType::Bpchar),
            ("name", PgType::Name),
            ("oid", PgType::Oid),
            ("tid", PgType::Tid),
            ("xid", PgType::Xid),
            ("xid8", PgType::Xid8),
            ("pg_lsn", PgType::PgLsn),
            ("bytea", PgType::Bytea),
            ("date", PgType::Date),
            ("time", PgType::Time),
            ("timetz", PgType::TimeTz),
            ("timestamp", PgType::Timestamp),
            ("timestamptz", PgType::TimestampTz),
            ("interval", PgType::Interval),
            ("uuid", PgType::Uuid),
            ("inet", PgType::Inet),
            ("cidr", PgType::Cidr),
            ("point", PgType::Point),
            ("lseg", PgType::Lseg),
            ("path", PgType::Path),
            ("box", PgType::Box),
            ("polygon", PgType::Polygon),
            ("line", PgType::Line),
            ("circle", PgType::Circle),
            ("json", PgType::Json),
            ("jsonb", PgType::Jsonb),
            ("jsonpath", PgType::Jsonpath),
            ("tsvector", PgType::Tsvector),
            ("tsquery", PgType::Tsquery),
        ];
        for (typname, ty) in modeled {
            let row = PG_TYPE_ROWS
                .iter()
                .find(|r| r.typname == typname)
                .unwrap_or_else(|| panic!("pg_type.dat has a row for {typname}"));
            assert_eq!(row.oid, ty.oid(), "{typname} oid drift (.dat vs PgType)");
            assert_eq!(
                row.typlen,
                ty.typlen(),
                "{typname} typlen drift (.dat vs PgType)"
            );
            // The name a bare or `pg_catalog.`-qualified type name binds through
            // is the same one the catalog reports, in both directions — so a
            // built-in cannot be spelled one way in `pg_type` and another in a
            // cast.
            assert_eq!(
                PgType::from_name(typname),
                Some(ty),
                "{typname} does not resolve back to its PgType"
            );
            assert_eq!(ty.typname(), typname, "{typname} typname drift");
        }
    }

    #[test]
    fn pg_cast_resolves_type_names_to_oids() -> anyhow::Result<()> {
        let schema = schema::pg_cast_schema();
        let rows = schema::pg_cast_rows();
        let src = required(
            schema.column_index("castsource"),
            "castsource column is missing",
        )?;
        let tgt = required(
            schema.column_index("casttarget"),
            "casttarget column is missing",
        )?;
        let ctx = required(
            schema.column_index("castcontext"),
            "castcontext column is missing",
        )?;
        // int4 (23) -> int8 (20) is an implicit cast in PG.
        let int4_to_int8 = rows
            .iter()
            .find(|r| r[src] == Value::Oid(23) && r[tgt] == Value::Oid(20))
            .expect("int4->int8 cast present");
        assert_eq!(int4_to_int8[ctx], Value::Text("i".to_string()));
        // Every emitted cast references exposed types (nonzero, resolved OIDs).
        assert!(
            rows.iter()
                .all(|r| r[src] != Value::Oid(0) && r[tgt] != Value::Oid(0))
        );

        Ok(())
    }

    /// `pg_am` reports PostgreSQL's built-in access methods verbatim, and the
    /// OIDs `pg_class.relam` emits are exactly the ones it can be joined to.
    #[test]
    fn pg_am_lists_the_builtin_access_methods() -> anyhow::Result<()> {
        let schema = schema::pg_am_schema();
        let rows = schema::pg_am_rows();
        let oid = required(schema.column_index("oid"), "oid column is missing")?;
        let amname = required(schema.column_index("amname"), "amname column is missing")?;
        let amtype = required(schema.column_index("amtype"), "amtype column is missing")?;
        assert!(rows.iter().all(|r| r.len() == schema.columns.len()));

        let by_oid = |n: u32| rows.iter().find(|r| r[oid] == Value::Oid(n));
        // heap is the only table access method; the rest are index methods.
        let heap = required(by_oid(2), "heap row is missing")?;
        assert_eq!(heap[amname], Value::Text("heap".to_string()));
        assert_eq!(heap[amtype], Value::Text("t".to_string()));
        let btree = required(by_oid(403), "btree row is missing")?;
        assert_eq!(btree[amname], Value::Text("btree".to_string()));
        assert_eq!(btree[amtype], Value::Text("i".to_string()));
        assert_eq!(
            rows.iter()
                .filter(|r| r[amtype] == Value::Text("i".to_string()))
                .count(),
            6
        );

        // Every `relam` a pg_class row can carry joins to a pg_am row (0 is the
        // no-access-method sentinel views/sequences/partitioned parents use).
        let cat = SystemCatalog::with_relations(vec![TableSchema::new(
            "t",
            vec![Column::new("a", PgType::Int4)],
        )]);
        let (class_schema, class_rows) =
            required(cat.build_pg_catalog("pg_class"), "pg_class is missing")?;
        let relam = required(
            class_schema.column_index("relam"),
            "relam column is missing",
        )?;
        for row in &class_rows {
            if row[relam] == Value::Oid(0) {
                continue;
            }
            assert!(
                rows.iter().any(|am| am[oid] == row[relam]),
                "pg_class.relam {:?} has no pg_am row",
                row[relam]
            );
        }

        Ok(())
    }

    /// The `pg_class` columns psql's `\d` reads but crabgresql has no state for
    /// carry their true PostgreSQL constant, not a placeholder — `relchecks = 0`
    /// is what gates psql's CHECK-constraint query *off*, and `relhasrules`
    /// distinguishes a view (which owns a `_RETURN` rule) from a table.
    /// `relpartbound` carries the deparsed bound a leaf partition was created
    /// with, since `pg_get_expr` only echoes it.
    #[test]
    fn pg_class_reports_describe_columns_and_partition_bounds() -> anyhow::Result<()> {
        use crabgresql_storage_api::{
            Column, PartitionBound, PartitionBoundDatum, PartitionOf, PartitionScheme,
            PartitionStrategy, TableSchema,
        };
        use crabgresql_types::PgType;

        fn plain(name: &str) -> TableSchema {
            TableSchema::new(name, vec![Column::new("a", PgType::Int4)])
        }
        // A leaf partition of `part`, bounded by one datum on each side.
        fn leaf(name: &str, from: PartitionBoundDatum, to: PartitionBoundDatum) -> TableSchema {
            let mut schema = plain(name);
            schema.partition_of = Some(PartitionOf {
                parent_namespace: "public".to_string(),
                parent_name: "part".to_string(),
                key_columns: vec![0],
                bound: PartitionBound {
                    from: vec![from],
                    to: vec![to],
                },
            });
            schema
        }
        // A range-partitioned parent, one leaf with a numeric bound open at the
        // top, and one leaf keyed on text (which must quote its literals).
        let mut parent = plain("part");
        parent.partition_scheme = Some(PartitionScheme {
            strategy: PartitionStrategy::Range,
            key_columns: vec![0],
        });
        let cat = SystemCatalog::with_catalog_relations_fn("db", "owner", move || {
            vec![
                CatalogRelation::permanent(plain("tbl")),
                CatalogRelation::view(plain("vw")),
                CatalogRelation::permanent(parent.clone()),
                CatalogRelation::permanent(leaf(
                    "part_hi",
                    PartitionBoundDatum::Value(Value::Int4(10)),
                    PartitionBoundDatum::MaxValue,
                )),
                CatalogRelation::permanent(leaf(
                    "part_txt",
                    PartitionBoundDatum::MinValue,
                    PartitionBoundDatum::Value(Value::Text("it's".to_string())),
                )),
                CatalogRelation::permanent(leaf(
                    "part_bool",
                    PartitionBoundDatum::Value(Value::Bool(false)),
                    PartitionBoundDatum::Value(Value::Bool(true)),
                )),
                CatalogRelation::permanent(leaf(
                    "part_neg",
                    PartitionBoundDatum::Value(Value::Int4(-10)),
                    PartitionBoundDatum::Value(Value::Int4(0)),
                )),
            ]
        });

        let (schema, rows) = required(cat.build_pg_catalog("pg_class"), "pg_class is missing")?;
        assert!(rows.iter().all(|r| r.len() == schema.columns.len()));
        let relname = required(schema.column_index("relname"), "relname is missing")?;
        let cell = |name: &str, col: &str| -> anyhow::Result<Value> {
            let i = required(schema.column_index(col), col)?;
            required(
                rows.iter()
                    .find(|r| r[relname] == Value::Text(name.to_string()))
                    .map(|r| r[i].clone()),
                name,
            )
        };

        // No CHECK constraints, triggers, row security, typed tables, or
        // non-default tablespace exist here, and nothing has been stored out of
        // line — but each column still answers.
        for col in [
            "relchecks",
            "relhastriggers",
            "relrowsecurity",
            "relforcerowsecurity",
            "reloftype",
            "reltablespace",
            "reltoastrelid",
        ] {
            let zero = match col {
                "relchecks" => Value::Int2(0),
                "relhastriggers" | "relrowsecurity" | "relforcerowsecurity" => Value::Bool(false),
                _ => Value::Oid(0),
            };
            assert_eq!(cell("tbl", col)?, zero, "{col}");
        }
        // Only a view carries a rule; only heap-backed relations default their
        // replica identity to the primary key.
        assert_eq!(cell("tbl", "relhasrules")?, Value::Bool(false));
        assert_eq!(cell("vw", "relhasrules")?, Value::Bool(true));
        assert_eq!(cell("tbl", "relreplident")?, Value::Text("d".to_string()));
        assert_eq!(cell("part", "relreplident")?, Value::Text("d".to_string()));
        assert_eq!(cell("vw", "relreplident")?, Value::Text("n".to_string()));

        // A non-partition has no bound; a leaf's is the text PostgreSQL's
        // `pg_get_expr(relpartbound, oid)` prints — numbers bare, other literals
        // quoted (with embedded quotes doubled), MINVALUE/MAXVALUE as keywords.
        assert_eq!(cell("tbl", "relpartbound")?, Value::Null);
        assert_eq!(cell("part", "relpartbound")?, Value::Null);
        assert_eq!(
            cell("part_hi", "relpartbound")?,
            Value::Text("FOR VALUES FROM (10) TO (MAXVALUE)".to_string())
        );
        assert_eq!(
            cell("part_txt", "relpartbound")?,
            Value::Text("FOR VALUES FROM (MINVALUE) TO ('it''s')".to_string())
        );
        // A boolean bound is a keyword, and a negative number is quoted — both
        // as PostgreSQL prints them, and `'f'` would not even re-parse.
        assert_eq!(
            cell("part_bool", "relpartbound")?,
            Value::Text("FOR VALUES FROM (false) TO (true)".to_string())
        );
        assert_eq!(
            cell("part_neg", "relpartbound")?,
            Value::Text("FOR VALUES FROM ('-10') TO (0)".to_string())
        );

        Ok(())
    }

    /// `pg_attribute.atttypmod` is emitted in PostgreSQL's encoding, not the raw
    /// modifier crabgresql stores on the column, so `format_type(atttypid,
    /// atttypmod)` reproduces PG's `\d` type strings. The character types add
    /// the four-byte varlena header; the bit types do not; a column with no
    /// modifier is `-1`.
    #[test]
    fn pg_attribute_encodes_postgres_atttypmod() -> anyhow::Result<()> {
        use crabgresql_storage_api::{Column, TableSchema};
        use crabgresql_types::PgType;

        let cat = SystemCatalog::with_relations(vec![TableSchema::new(
            "t",
            vec![
                Column::with_typmod("v", PgType::Varchar, 20),
                Column::with_typmod("c", PgType::Bpchar, 10),
                Column::with_typmod("b", PgType::Bit, 5),
                Column::with_typmod("vb", PgType::Varbit, 7),
                Column::new("i", PgType::Int4),
            ],
        )]);
        let (schema, rows) = required(
            cat.build_pg_catalog("pg_attribute"),
            "pg_attribute is missing",
        )?;
        assert!(rows.iter().all(|r| r.len() == schema.columns.len()));
        let attname = required(schema.column_index("attname"), "attname is missing")?;
        let cell = |name: &str, col: &str| -> anyhow::Result<Value> {
            let i = required(schema.column_index(col), col)?;
            required(
                rows.iter()
                    .find(|r| r[attname] == Value::Text(name.to_string()))
                    .map(|r| r[i].clone()),
                name,
            )
        };

        // varchar(20) / character(10) reserve VARHDRSZ; bit(5) / varbit(7) do not.
        assert_eq!(cell("v", "atttypmod")?, Value::Int4(24));
        assert_eq!(cell("c", "atttypmod")?, Value::Int4(14));
        assert_eq!(cell("b", "atttypmod")?, Value::Int4(5));
        assert_eq!(cell("vb", "atttypmod")?, Value::Int4(7));
        assert_eq!(cell("i", "atttypmod")?, Value::Int4(-1));
        // Identity and generated columns do not exist, and PG spells "neither"
        // as the empty string rather than NULL — psql projects both directly.
        assert_eq!(cell("i", "attidentity")?, Value::Text(String::new()));
        assert_eq!(cell("i", "attgenerated")?, Value::Text(String::new()));

        Ok(())
    }

    /// Building `pg_attribute` must not panic on a length that would overflow
    /// PostgreSQL's `n + VARHDRSZ` encoding. DDL rejects such a length, so this
    /// is only reachable from a data directory that already holds one — where a
    /// panic would make the catalog permanently unreadable rather than merely
    /// misreport a column.
    #[test]
    fn oversized_typmod_saturates_instead_of_panicking() -> anyhow::Result<()> {
        use crabgresql_storage_api::{Column, TableSchema};
        use crabgresql_types::PgType;

        let cat = SystemCatalog::with_relations(vec![TableSchema::new(
            "t",
            vec![Column::with_typmod("v", PgType::Varchar, i32::MAX)],
        )]);
        let (schema, rows) = required(
            cat.build_pg_catalog("pg_attribute"),
            "pg_attribute is missing",
        )?;
        let i = required(schema.column_index("atttypmod"), "atttypmod is missing")?;
        assert_eq!(rows[0][i], Value::Int4(i32::MAX));

        Ok(())
    }

    /// Every `pg_catalog` relation this build serves has a fixed OID, and the
    /// mapping round trips. Without this a newly served relation would be
    /// nameable in a query but not castable to `regclass`, which is the exact
    /// gap the OID table exists to close.
    #[test]
    fn builtin_relation_oids_cover_every_served_relation() -> anyhow::Result<()> {
        let cat = SystemCatalog::new();
        for (name, oid) in BUILTIN_RELATION_OIDS {
            assert!(
                cat.has_catalog_relation(name),
                "{name} has a fixed OID but is not served"
            );
            assert_eq!(builtin_relation_oid(name), Some(*oid));
            assert_eq!(builtin_relation_name(*oid), Some(*name));
        }
        // The other direction: nothing served is missing from the table. The
        // served set is only reachable by name, so it is listed here explicitly.
        for name in [
            "pg_type",
            "pg_enum",
            "pg_namespace",
            "pg_class",
            "pg_attribute",
            "pg_attrdef",
            "pg_constraint",
            "pg_index",
            "pg_am",
            "pg_cast",
            "pg_collation",
            "pg_inherits",
            "pg_partitioned_table",
            "pg_sequence",
        ] {
            assert!(cat.has_catalog_relation(name), "{name} is not served");
            assert!(
                builtin_relation_oid(name).is_some(),
                "{name} is served but has no fixed OID"
            );
        }
        // A relation that is not a catalog one has no fixed OID.
        assert_eq!(builtin_relation_oid("no_such_catalog"), None);
        assert_eq!(builtin_relation_name(0), None);

        Ok(())
    }

    /// `pg_get_userbyid`'s and `pg_table_is_visible`'s backing lookups agree with
    /// the `pg_class` rows built from the same snapshot: the `relowner` every row
    /// reports resolves to a name, and every row's OID resolves to the namespace
    /// that row reports.
    #[test]
    fn catalog_lookups_agree_with_pg_class_rows() -> anyhow::Result<()> {
        let cat = SystemCatalog::with_catalog_relations_fn("db", "alice", || {
            vec![CatalogRelation::permanent(TableSchema::in_namespace(
                "t",
                "app",
                vec![Column::new("a", PgType::Int4)],
            ))]
        })
        .with_schemas_fn(|| vec![("app".to_string(), 16_000)]);
        let (schema, rows) = required(cat.build_pg_catalog("pg_class"), "pg_class is missing")?;
        let oid = required(schema.column_index("oid"), "oid column is missing")?;
        let relowner = required(
            schema.column_index("relowner"),
            "relowner column is missing",
        )?;
        let relnamespace = required(
            schema.column_index("relnamespace"),
            "relnamespace column is missing",
        )?;
        let row = required(rows.first(), "expected one pg_class row")?;

        // The owner OID pg_class reports is the one `pg_get_userbyid` resolves.
        // Asserted against the constant, not a literal, so moving the bootstrap
        // OID cannot leave the row and the lookup disagreeing.
        assert_eq!(row[relowner], Value::Oid(schema::BOOTSTRAP_ROLE_OID));
        assert_eq!(cat.role_name(schema::BOOTSTRAP_ROLE_OID), Some("alice"));
        assert_eq!(cat.role_name(schema::BOOTSTRAP_ROLE_OID + 1), None);

        // Every other owner column reports the same role, so `pg_get_userbyid`
        // resolves them all rather than printing `unknown (OID=n)` for some.
        for relation in ["pg_type", "pg_collation", "pg_namespace"] {
            let (s, r) = required(cat.build_pg_catalog(relation), relation)?;
            let owner = required(
                s.columns
                    .iter()
                    .position(|c| c.name.ends_with("owner"))
                    .map(|i| i),
                "an owner column",
            )?;
            for row in &r {
                let Value::Oid(o) = row[owner] else {
                    anyhow::bail!("{relation} owner column was not an OID");
                };
                assert!(
                    cat.role_name(o).is_some(),
                    "{relation} owner OID {o} does not resolve to a role name"
                );
            }
        }

        // ... and the namespace it reports is the one visibility is decided on.
        let Value::Oid(rel_oid) = row[oid] else {
            anyhow::bail!("pg_class.oid was not an OID");
        };
        assert_eq!(cat.relation_ref(rel_oid), Some(("app", "t")));
        assert_eq!(cat.relation_oid_in("app", "t"), Some(rel_oid));
        assert_eq!(
            cat.namespace_oids().get("app").copied().map(Value::Oid),
            Some(row[relnamespace].clone())
        );
        // An OID no relation has resolves to nothing, so the function is NULL —
        // both above the assigned range and below the synthetic floor.
        assert_eq!(cat.relation_ref(rel_oid + 1_000), None);
        assert_eq!(cat.relation_ref(1), None);

        Ok(())
    }

    #[test]
    fn unknown_relation_is_not_found() {
        let cat = SystemCatalog::new();
        assert!(cat.open_table("pg_type").is_ok());
        assert!(cat.open_table("pg_namespace").is_ok());
        assert!(cat.open_table("pg_cast").is_ok());
        assert!(cat.open_table("pg_am").is_ok());
        assert!(matches!(
            cat.open_table("pg_nonexistent"),
            Err(StorageError::TableNotFound(_))
        ));
    }

    #[test]
    fn information_schema_reflects_relation_metadata() -> anyhow::Result<()> {
        use crabgresql_storage_api::{Column, TableSchema};
        use crabgresql_types::PgType;

        let cat = SystemCatalog::with_catalog_relations_fn("appdb", "appuser", || {
            vec![
                CatalogRelation::permanent(TableSchema::new(
                    "widgets",
                    vec![
                        Column::new("id", PgType::Int4),
                        Column::with_typmod("label", PgType::Varchar, 12),
                    ],
                )),
                CatalogRelation::temporary(
                    TableSchema::in_namespace(
                        "scratch",
                        "pg_temp_42",
                        vec![Column::new("created_at", PgType::TimestampTz)],
                    ),
                    "pg_temp_42",
                ),
            ]
        });

        let (tables_schema, tables) = required(
            cat.build_information_schema("tables"),
            "information_schema.tables is missing",
        )?;
        assert_eq!(tables_schema.columns.len(), 12);
        let catalog = required(
            tables_schema.column_index("table_catalog"),
            "table_catalog column is missing",
        )?;
        let namespace = required(
            tables_schema.column_index("table_schema"),
            "table_schema column is missing",
        )?;
        let name = required(
            tables_schema.column_index("table_name"),
            "table_name column is missing",
        )?;
        let kind = required(
            tables_schema.column_index("table_type"),
            "table_type column is missing",
        )?;
        assert!(tables.iter().any(|row| {
            row[catalog] == Value::Text("appdb".to_string())
                && row[namespace] == Value::Text("public".to_string())
                && row[name] == Value::Text("widgets".to_string())
                && row[kind] == Value::Text("BASE TABLE".to_string())
        }));
        assert!(tables.iter().any(|row| {
            row[namespace] == Value::Text("pg_temp_42".to_string())
                && row[name] == Value::Text("scratch".to_string())
                && row[kind] == Value::Text("LOCAL TEMPORARY".to_string())
        }));

        let (columns_schema, columns) = required(
            cat.build_information_schema("columns"),
            "information_schema.columns is missing",
        )?;
        assert_eq!(columns_schema.columns.len(), 44);
        assert!(
            columns
                .iter()
                .all(|row| row.len() == columns_schema.columns.len())
        );
        let table_name = required(
            columns_schema.column_index("table_name"),
            "table_name column is missing",
        )?;
        let column_name = required(
            columns_schema.column_index("column_name"),
            "column_name column is missing",
        )?;
        let ordinal = required(
            columns_schema.column_index("ordinal_position"),
            "ordinal column is missing",
        )?;
        let data_type = required(
            columns_schema.column_index("data_type"),
            "data_type column is missing",
        )?;
        let char_length = required(
            columns_schema.column_index("character_maximum_length"),
            "character_maximum_length column is missing",
        )?;
        let udt_schema = required(
            columns_schema.column_index("udt_schema"),
            "udt_schema column is missing",
        )?;
        let is_generated = required(
            columns_schema.column_index("is_generated"),
            "is_generated column is missing",
        )?;
        let label = required(
            columns.iter().find(|row| {
                row[table_name] == Value::Text("widgets".to_string())
                    && row[column_name] == Value::Text("label".to_string())
            }),
            "label column row is missing",
        )?;
        assert_eq!(label[ordinal], Value::Int4(2));
        assert_eq!(
            label[data_type],
            Value::Text("character varying".to_string())
        );
        assert_eq!(label[char_length], Value::Int4(12));
        assert_eq!(label[udt_schema], Value::Text("pg_catalog".to_string()));
        assert_eq!(label[is_generated], Value::Text("NEVER".to_string()));

        let (_, schemata) = required(
            cat.build_information_schema("schemata"),
            "information_schema.schemata is missing",
        )?;
        assert!(schemata.iter().any(|row| {
            row[1] == Value::Text("pg_temp_42".to_string())
                && row[2] == Value::Text("appuser".to_string())
        }));

        Ok(())
    }

    /// `relpages`/`reltuples` follow PostgreSQL's rule that they are written only
    /// by `ANALYZE`, and both of `pg_class_rows`' row-building paths (relations,
    /// then indexes) stay as wide as the schema.
    #[test]
    fn pg_class_size_columns_report_the_never_analyzed_sentinel() -> anyhow::Result<()> {
        use crabgresql_storage_api::{Column, IndexKey, IndexMethod, RelStats, TableSchema};
        use crabgresql_types::PgType;

        let table = TableSchema::new("tbl", vec![Column::new("a", PgType::Int4)]);
        let index = IndexMetadata {
            name: "tbl_a_idx".to_string(),
            method: IndexMethod::BTree,
            keys: vec![IndexKey {
                column: 0,
                descending: false,
                nulls_first: false,
            }],
            unique: false,
            nulls_distinct: true,
            constraint: None,
        };
        // `analyzed_stats` stands in for a relation ANALYZE has measured; the
        // others have not been analyzed and must report the sentinel.
        let analyzed = RelStats::exact(1234, &table);
        let cat = SystemCatalog::with_catalog_relations_fn("db", "owner", move || {
            let mut measured = CatalogRelation::permanent(TableSchema::new(
                "measured",
                vec![Column::new("a", PgType::Int4)],
            ));
            measured.stats = analyzed.clone();
            let mut indexed = CatalogRelation::permanent(table.clone());
            indexed.indexes = vec![index.clone()];
            vec![
                measured,
                indexed,
                CatalogRelation::view(TableSchema::new("vw", vec![Column::new("a", PgType::Int4)])),
                CatalogRelation::sequence(
                    "sq",
                    "public",
                    CatalogSequence {
                        type_oid: PgType::Int8.oid(),
                        start: 1,
                        increment: 1,
                        min: 1,
                        max: i64::MAX,
                        cache: 1,
                        cycle: false,
                    },
                ),
            ]
        });

        let (schema, rows) = required(cat.build_pg_catalog("pg_class"), "pg_class is missing")?;
        // The index row is built by a separate path from the relation rows;
        // both must match the schema width or a client reads shifted columns.
        assert_eq!(rows.len(), 5, "four relations plus one index");
        assert!(rows.iter().all(|r| r.len() == schema.columns.len()));

        let relname = required(schema.column_index("relname"), "relname is missing")?;
        let cell = |name: &str, col: &str| -> anyhow::Result<Value> {
            let i = required(schema.column_index(col), col)?;
            required(
                rows.iter()
                    .find(|r| r[relname] == Value::Text(name.to_string()))
                    .map(|r| r[i].clone()),
                name,
            )
        };

        // Never analyzed: no pages and the `-1` unknown sentinel — NOT zero,
        // which would claim the relation is known to be empty.
        for name in ["tbl", "vw", "tbl_a_idx"] {
            assert_eq!(cell(name, "relpages")?, Value::Int4(0), "{name}");
            assert_eq!(cell(name, "reltuples")?, Value::Float4(-1.0), "{name}");
        }
        // Analyzed: the measured count, reported as-is.
        assert_eq!(cell("measured", "reltuples")?, Value::Float4(1234.0));
        assert!(matches!(cell("measured", "relpages")?, Value::Int4(p) if p > 0));
        // A sequence is one page holding one row from the moment it is created.
        assert_eq!(cell("sq", "relpages")?, Value::Int4(1));
        assert_eq!(cell("sq", "reltuples")?, Value::Float4(1.0));
        // No visibility map is kept, so nothing is ever all-visible.
        assert_eq!(cell("measured", "relallvisible")?, Value::Int4(0));

        Ok(())
    }

    #[test]
    fn a_toast_relation_is_published_and_its_parent_points_at_it() -> anyhow::Result<()> {
        // `reltoastrelid` is a foreign key into `pg_class.oid`, so publishing the
        // row is what makes a non-zero value safe rather than a dangling
        // reference. A table that has never stored anything out of line keeps 0,
        // which is what PostgreSQL reports for a table with no TOAST relation.
        fn plain(name: &str) -> TableSchema {
            TableSchema::new(name, vec![Column::new("id", PgType::Int4)])
        }
        let cat = SystemCatalog::with_catalog_relations_fn("db", "owner", move || {
            let mut toasted = CatalogRelation::permanent(plain("toasted"));
            toasted.toast = Some(RelStats {
                relpages: 7,
                reltuples: 0.0,
                analyzed: false,
                columns: Vec::new(),
            });
            vec![CatalogRelation::permanent(plain("bare")), toasted]
        });

        let (schema, rows) = required(cat.build_pg_catalog("pg_class"), "pg_class is missing")?;
        let col = |name: &str| required(schema.column_index(name), name);
        let (relname, oid, toastrel) = (col("relname")?, col("oid")?, col("reltoastrelid")?);
        let row = |name: &str| {
            rows.iter()
                .find(|r| r[relname] == Value::Text(name.to_string()))
                .cloned()
        };

        let toasted = required(row("toasted"), "toasted")?;
        let Value::Oid(toast_oid) = toasted[toastrel] else {
            anyhow::bail!("reltoastrelid is not an OID");
        };
        assert_ne!(toast_oid, 0, "a relation with out-of-line storage names it");
        assert_eq!(
            required(row("bare"), "bare")?[toastrel],
            Value::Oid(0),
            "a relation with none must not borrow its neighbour's"
        );

        // The OID resolves to a real row, in `pg_toast`, named after its parent.
        let toast_row = required(
            rows.iter()
                .find(|r| r[oid] == Value::Oid(toast_oid))
                .cloned(),
            "the toast relation has no pg_class row",
        )?;
        let Value::Oid(parent_oid) = toasted[oid] else {
            anyhow::bail!("oid is not an OID");
        };
        assert_eq!(
            toast_row[relname],
            Value::Text(format!("pg_toast_{parent_oid}"))
        );
        assert_eq!(toast_row[col("relkind")?], Value::Text("t".to_string()));
        assert_eq!(toast_row[col("relnamespace")?], Value::Oid(99));
        assert_eq!(toast_row[col("relpages")?], Value::Int4(7));
        // We chain chunks by ctid rather than indexing them, so claiming an index
        // would be the dangling reference this row exists to avoid.
        assert_eq!(toast_row[col("relhasindex")?], Value::Bool(false));

        // Every OID is distinct: the toast block sits after the index block, so
        // it can neither collide with nor shift an existing assignment.
        let mut oids: Vec<&Value> = rows.iter().map(|r| &r[oid]).collect();
        let total = oids.len();
        oids.sort_by_key(|v| match v {
            Value::Oid(o) => *o,
            _ => 0,
        });
        oids.dedup();
        assert_eq!(oids.len(), total, "pg_class OIDs must be unique");

        // Its columns join, so `relnatts` is not a claim without rows behind it.
        let (aschema, arows) = required(
            cat.build_pg_catalog("pg_attribute"),
            "pg_attribute is missing",
        )?;
        let attrelid = required(aschema.column_index("attrelid"), "attrelid")?;
        let attname = required(aschema.column_index("attname"), "attname")?;
        let names: Vec<String> = arows
            .iter()
            .filter(|r| r[attrelid] == Value::Oid(toast_oid))
            .map(|r| match &r[attname] {
                Value::Text(s) => s.clone(),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(names, vec!["chunk_id", "chunk_seq", "chunk_data"]);

        // A toast relation is not a user relation: it must not be reachable by an
        // unqualified name, which is why it never enters `live_relations`.
        assert_eq!(cat.relation_oid_in("public", "pg_toast_1"), None);
        assert_eq!(
            cat.relation_ref(toast_oid),
            Some(("pg_toast", format!("pg_toast_{parent_oid}").as_str()))
        );
        Ok(())
    }
}
