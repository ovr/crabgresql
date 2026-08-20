//! `pg_catalog` and `information_schema` system catalogs, served as read-only
//! relations.
//!
//! The core seam: a bound `SELECT` lowers to a scan over an `Arc<dyn TableAm>`,
//! and the executor treats every access method alike. So each supported
//! `pg_catalog` relation is materialized as a [`StaticTable`] (rows built from
//! codegen'd built-in data and from live server state) and handed to the same
//! pipeline user tables use — no bespoke executor node.
//!
//! [`SystemCatalog`] implements [`TableEngine`] so the server's session catalog
//! can layer it into name resolution: `pg_catalog.<rel>` routes here directly,
//! and an unqualified name falls back here (pg_catalog is implicitly on the
//! search path).
//!
//! # Layout
//!
//! - [`registry`] — the one table of served relations. Which relations exist,
//!   their OIDs, and the two `fn` pointers that build each one.
//! - `catalogs/` and `views/` — a module per relation family, each publishing a
//!   `*_schema()` and a `*_rows(&SystemCatalog)`. Adding a relation is a module
//!   here plus one registry line; nothing else in the crate changes.
//! - [`cols`] and [`oids`] — what those modules share: the catalog-only column
//!   types and every fixed OID.
//! - [`source`] — the live state a session hands in ([`CatalogSource`]).
//! - This file — [`SystemCatalog`]: the per-statement snapshot, the memoized OID
//!   assignments the relation modules read, and the [`TableEngine`] impl.
//!
//! # Fidelity & clean-room
//!
//! Built-in rows are generated at build time from PostgreSQL's vendored catalog
//! `.dat` *data* (`vendor/postgres/catalog/`), never from its C/Perl source; the
//! codegen is the `crabgresql-bki` crate, and `AGENTS.md` carries the policy.
//!
//! TODO: column coverage is a curated, PG-ordered subset keyed by the names real
//! clients query, not parity — upstream's `type_sanity` and psql's `\d` read
//! columns no relation here publishes. The catalog-only column *types* are real
//! (`oid`, `"char"`, `regproc`); it is the column list that is short.

pub(crate) mod catalogs;
pub(crate) mod cols;
pub(crate) mod oids;
pub(crate) mod registry;
mod source;
mod static_table;
pub(crate) mod views;

pub use catalogs::depend::nextval_target;
pub use catalogs::description::{object_description, object_descriptions_any_class};
pub use catalogs::extension::{AvailableExtension, available_extensions};
pub use oids::PLPGSQL_LANG_OID;
pub use registry::{builtin_relation_name, builtin_relation_oid};
pub use source::{
    CatalogBackend, CatalogCursor, CatalogLock, CatalogLockTarget, CatalogPreparedStatement,
    CatalogRelation, CatalogRoutine, CatalogSequence, CatalogSetting, CatalogSource,
    CatalogUserType, CatalogViewDependency, RelKind, StaticSource, ViewDepRelation,
};

#[cfg(test)]
mod tests;

use std::sync::{Arc, OnceLock};

use crabgresql_storage_api::pgstat::{DbStatSnapshot, IndexStatSnapshot, RelStatSnapshot};
use crabgresql_storage_api::{
    IndexConstraint, IndexMetadata, RelPersistence, RelStats, RelationFilenodes, StorageError,
    TableAm, TableEngine, TableSchema,
};
use crabgresql_txn::Xid;
#[cfg(test)]
use crabgresql_types::Value;

use crate::registry::CatalogNamespace;

pub use static_table::StaticTable;

/// First OID handed to a synthetic user relation in `pg_class`. Runtime type,
/// function, and cast OIDs grow upward from PostgreSQL's user-object floor, so
/// relations use a separate high partition, which preserves catalog-wide
/// uniqueness in every reflected snapshot.
///
/// TODO: these OIDs are assigned per snapshot rather than stored, and the
/// assignment is positional (see [`SystemCatalog::relation_oids`]). A relation's
/// OID is therefore stable only while the set of relations is — creating one
/// renumbers every relation sorting after it — so a client that holds an OID
/// across DDL can address the wrong relation. Storage owning a persistent OID
/// per relation is what fixes it.
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
    /// The index's own physical file, or `0` when it has none — an index the
    /// planner may use but that is backed by no B-tree file at all.
    relfilenode: u32,
    /// This index's own physical size, or `None` for one the engine holds as
    /// metadata only. Only [`RelStats::relpages`] is meaningful — an index entry
    /// is not a row, so no tuple count is carried.
    stats: Option<RelStats>,
}

/// One row of `pg_rewrite`: the `_RETURN` rule a view's body is stored as. The
/// `catalogs::rewrite` module says why a view is the only thing that has one.
#[derive(Clone)]
pub struct CatalogRewrite {
    pub oid: u32,
    /// `ev_class`: the view the rule is attached to.
    pub view_oid: u32,
    /// The view's deparsed body, or `None` when the deparser could not render
    /// it — see [`CatalogRelation::definition`].
    pub definition: Option<String>,
}

/// One row of `pg_attrdef`: a column default (or a generated column's
/// expression), numbered before anything renders it.
///
/// Numbered separately from the rendering for the reason [`CatalogConstraint`]
/// gives, plus one of its own: `pg_depend` names an `attrdef` row as the
/// *dependent* object, so the OID in that edge and the OID `pg_attrdef` prints
/// have to come from the same place.
#[derive(Clone)]
pub(crate) struct CatalogAttrDef {
    pub(crate) oid: u32,
    /// `adrelid`: the relation the column belongs to.
    pub(crate) table_oid: u32,
    /// `adnum`: the column's one-based position.
    pub(crate) attnum: i16,
    /// `adbin` as this build stores it — the canonical SQL text, which
    /// `pg_get_expr` echoes.
    pub(crate) expr: String,
}

/// One row of `pg_constraint`, resolved to an OID before anything renders it.
///
/// The rows used to be built and numbered in the same pass, from a counter local
/// to the renderer. That was enough while `pg_constraint` was only ever read as a
/// table, but `pg_get_constraintdef(oid)` looks a constraint up *by* its OID, so
/// the numbering has to exist independently of the rendering — and in the same
/// monotone band as every other relation-shaped object, or a constraint OID
/// could collide with a relation's.
#[derive(Clone)]
pub struct CatalogConstraint {
    pub oid: u32,
    pub name: String,
    /// `pg_constraint.contype`: `n` not-null, `p` primary key, `u` unique,
    /// `c` check.
    pub contype: &'static str,
    /// The namespace of the table it constrains — resolved to `connamespace` by
    /// whoever renders the row.
    pub namespace: String,
    pub table_oid: u32,
    /// The backing index for `p`/`u`, `0` for a constraint with no index.
    pub index_oid: u32,
    /// Constrained column positions, zero-based; rendered as `conkey`'s
    /// one-based array.
    pub columns: Vec<usize>,
    /// `pg_constraint.conbin` for a check constraint: the predicate as stored
    /// SQL. `None` for every other `contype`, which has no expression.
    pub expr: Option<String>,
    /// `convalidated` / `conislocal` / `coninhcount`. The non-check contypes
    /// have no way to be unvalidated or inherited here, so they report
    /// `(true, true, 0)`.
    pub validated: bool,
    pub islocal: bool,
    pub inhcount: i16,
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
    relfilenode: u32,
}

/// What a relation occupies on disk, in 8 KB pages, split the way the four
/// `pg_*_size` functions add it up. See [`SystemCatalog::relation_pages`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelationPages {
    /// The relation's own storage — PostgreSQL's main fork. The other forks
    /// (`fsm`, `vm`, `init`) have no counterpart here and are not carried.
    pub main: u32,
    /// Its out-of-line storage, or zero when it has none.
    pub toast: u32,
    /// Every index on it, summed. Zero for a relation that is itself an index:
    /// PostgreSQL reports `pg_indexes_size` of an index as zero.
    pub indexes: u32,
}

/// What [`SystemCatalog::serial_sequence`] found. The three non-answers are
/// distinct because PostgreSQL renders each differently: a missing relation and
/// a missing column are errors with different SQLSTATEs, and a column that owns
/// no sequence is a NULL.
pub enum SerialSequenceLookup {
    Owned {
        namespace: String,
        name: String,
    },
    Unowned,
    /// Carries the relation's name, which PostgreSQL's message quotes.
    NoColumn {
        relation: String,
    },
    NoRelation,
}

/// A `regproc` reference: the function's OID and the name it prints as.
///
/// PostgreSQL stores only the OID and resolves the name in `regprocout`. The
/// catalog rows here are static, so codegen resolves both at build time and the
/// pair travels together — the same trade [`crabgresql_types::Reg`] makes.
#[derive(Clone, Copy, Debug)]
pub struct ProcRef {
    pub oid: u32,
    pub name: &'static str,
}

/// A built-in `pg_type` row, generated from `pg_type.dat`. Field types mirror
/// the runtime column types in [`catalogs::types::pg_type_schema`]; string fields are the
/// catalog `name`/`"char"` text.
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
    pub typsubscript: ProcRef,
    pub typelem: u32,
    pub typarray: u32,
    pub typinput: ProcRef,
    pub typoutput: ProcRef,
    pub typreceive: ProcRef,
    pub typsend: ProcRef,
    pub typmodin: ProcRef,
    pub typmodout: ProcRef,
    pub typanalyze: ProcRef,
    pub typalign: &'static str,
    pub typstorage: &'static str,
    pub typcollation: u32,
}

/// A built-in `pg_cast` row, generated from `pg_cast.dat`. `castsource`,
/// `casttarget` and `castfunc` are resolved OIDs; `castfunc` is `0` for a
/// binary-coercible cast, which needs no function.
pub struct PgCastRow {
    pub oid: u32,
    pub castsource: u32,
    pub casttarget: u32,
    pub castfunc: u32,
    pub castcontext: &'static str,
    pub castmethod: &'static str,
}

/// A built-in `pg_proc` row, generated from `pg_proc.dat` — restricted to the
/// functions the other catalogs reference (see `crabgresql-bki`'s `pg_proc`
/// module for why the rest are left out).
pub struct PgProcRow {
    pub oid: u32,
    pub proname: &'static str,
    pub prolang: u32,
    pub procost: f32,
    pub prorows: f32,
    pub provariadic: u32,
    pub prosupport: ProcRef,
    pub prokind: &'static str,
    pub prosecdef: bool,
    pub proleakproof: bool,
    pub proisstrict: bool,
    pub proretset: bool,
    pub provolatile: &'static str,
    pub proparallel: &'static str,
    pub pronargs: i16,
    pub prorettype: u32,
    pub proargtypes: &'static [u32],
    /// `proallargtypes`, `proargmodes` and `proargnames`: empty means the
    /// column is NULL, which is what PostgreSQL stores for a function with no
    /// OUT parameters and no declared argument names. An empty *array* is not a
    /// value any of the three ever takes, so the two cannot be confused.
    pub proallargtypes: &'static [u32],
    pub proargmodes: &'static [&'static str],
    pub proargnames: &'static [&'static str],
    pub prosrc: &'static str,
    /// The shared library a C function lives in — the conversion functions are
    /// the only referenced ones that have it. Empty reads as NULL.
    pub probin: &'static str,
}

/// A built-in `pg_opclass` row, generated from `pg_opclass.dat`. `opcnamespace`
/// and `opcowner` are not carried: every operator class lives in `pg_catalog`
/// and belongs to the bootstrap role, and the `.dat` never says otherwise, so
/// [`catalogs::opclass`] fills both from `oids`.
pub struct PgOpclassRow {
    pub oid: u32,
    pub opcmethod: u32,
    pub opcname: &'static str,
    pub opcfamily: u32,
    pub opcintype: u32,
    pub opcdefault: bool,
    /// The type the index actually stores, or `0` when that is `opcintype`
    /// itself — as PostgreSQL declares the column.
    pub opckeytype: u32,
}

/// A built-in `pg_opfamily` row, generated from `pg_opfamily.dat`. Namespace
/// and owner are constants for the same reason as in [`PgOpclassRow`].
pub struct PgOpfamilyRow {
    pub oid: u32,
    pub opfmethod: u32,
    pub opfname: &'static str,
}

/// A built-in `pg_aggregate` row, generated from `pg_aggregate.dat`.
///
/// There is no `oid`: an aggregate is identified by the `pg_proc` row it
/// extends, so [`Self::aggfnoid`] is both the key and the reference.
///
/// `agginitval`/`aggminitval` carry the empty string for the column's NULL —
/// no upstream entry has an initial state of `''`, so the two cannot be
/// confused.
pub struct PgAggregateRow {
    pub aggfnoid: ProcRef,
    /// `n` for a plain aggregate, `o` for an ordered-set one and `h` for a
    /// hypothetical-set one. Only the latter two take direct arguments.
    pub aggkind: &'static str,
    pub aggnumdirectargs: i16,
    pub aggtransfn: ProcRef,
    pub aggfinalfn: ProcRef,
    pub aggcombinefn: ProcRef,
    pub aggserialfn: ProcRef,
    pub aggdeserialfn: ProcRef,
    pub aggmtransfn: ProcRef,
    pub aggminvtransfn: ProcRef,
    pub aggmfinalfn: ProcRef,
    pub aggfinalextra: bool,
    pub aggmfinalextra: bool,
    pub aggfinalmodify: &'static str,
    pub aggmfinalmodify: &'static str,
    /// The ordering operator `MIN`/`MAX` are equivalent to, or 0.
    pub aggsortop: u32,
    pub aggtranstype: u32,
    pub aggtransspace: i32,
    pub aggmtranstype: u32,
    pub aggmtransspace: i32,
    pub agginitval: &'static str,
    pub aggminitval: &'static str,
}

/// A built-in `pg_operator` row, generated from `pg_operator.dat`. Namespace
/// and owner are not carried, for the reason [`PgOpclassRow`] gives.
pub struct PgOperatorRow {
    pub oid: u32,
    pub oprname: &'static str,
    /// `b` for an infix operator, `l` for a prefix one — whose
    /// [`Self::oprleft`] is 0.
    pub oprkind: &'static str,
    pub oprcanmerge: bool,
    pub oprcanhash: bool,
    pub oprleft: u32,
    pub oprright: u32,
    pub oprresult: u32,
    /// The commutator and negator, each an operator OID or 0 for none.
    pub oprcom: u32,
    pub oprnegate: u32,
    pub oprcode: ProcRef,
    /// The planner's restriction and join selectivity estimators, or the
    /// catalog's `-` when upstream declares none.
    pub oprrest: ProcRef,
    pub oprjoin: ProcRef,
}

/// A built-in `pg_amop` row, generated from `pg_amop.dat`: the operator that
/// implements one strategy of an operator family.
pub struct PgAmopRow {
    pub oid: u32,
    pub amopfamily: u32,
    pub amoplefttype: u32,
    pub amoprighttype: u32,
    pub amopstrategy: i16,
    /// `s` for a search operator, `o` for an ordering one — the only kind that
    /// also names an [`Self::amopsortfamily`].
    pub amoppurpose: &'static str,
    pub amopopr: u32,
    pub amopmethod: u32,
    pub amopsortfamily: u32,
}

/// A built-in `pg_amproc` row, generated from `pg_amproc.dat`: a support
/// function an operator family gives its access method.
pub struct PgAmprocRow {
    pub oid: u32,
    pub amprocfamily: u32,
    pub amproclefttype: u32,
    pub amprocrighttype: u32,
    pub amprocnum: i16,
    pub amproc: ProcRef,
}

/// A built-in `pg_conversion` row, generated from `pg_conversion.dat`.
/// Namespace and owner are not carried, for the reason [`PgOpclassRow`] gives.
pub struct PgConversionRow {
    pub oid: u32,
    pub conname: &'static str,
    /// The two encodings, by **name** rather than by the number the column
    /// stores. PostgreSQL's numbering lives in [`crabgresql_types::encoding`]
    /// and [`catalogs::conversion`] resolves through it, so codegen needs no
    /// second copy of that table — which is what a copy would be: one nothing
    /// keeps in step.
    pub conforencoding: &'static str,
    pub contoencoding: &'static str,
    pub conproc: ProcRef,
    pub condefault: bool,
}

/// The five text-search catalogs' bootstrap rows, generated from the
/// `pg_ts_*.dat` files. They describe the `default` parser, the four
/// dictionary templates and the `simple` dictionary and configuration built
/// from them; the twenty-nine snowball ones a stock PostgreSQL also publishes
/// are added by [`catalogs::textsearch`], because `initdb` creates those from
/// SQL rather than from any `.dat`.
pub struct PgTsParserRow {
    pub oid: u32,
    pub prsname: &'static str,
    pub prsstart: ProcRef,
    pub prstoken: ProcRef,
    pub prsend: ProcRef,
    pub prsheadline: ProcRef,
    pub prslextype: ProcRef,
}

pub struct PgTsTemplateRow {
    pub oid: u32,
    pub tmplname: &'static str,
    pub tmplinit: ProcRef,
    pub tmpllexize: ProcRef,
}

/// See [`PgTsParserRow`]. `dictinitoption` carries the empty string for the
/// column's NULL — a dictionary configured with an empty option string is not
/// a thing PostgreSQL accepts, so the two cannot be confused.
pub struct PgTsDictRow {
    pub oid: u32,
    pub dictname: &'static str,
    pub dicttemplate: u32,
    pub dictinitoption: &'static str,
}

pub struct PgTsConfigRow {
    pub oid: u32,
    pub cfgname: &'static str,
    pub cfgparser: u32,
}

/// See [`PgTsParserRow`]. This one has no `oid`: a row is keyed by
/// `(mapcfg, maptokentype, mapseqno)`, and the sequence number is what orders
/// the dictionaries a token is looked up in.
pub struct PgTsConfigMapRow {
    pub mapcfg: u32,
    pub maptokentype: i32,
    pub mapseqno: i32,
    pub mapdict: u32,
}

/// A built-in `pg_description` row, generated from the `descr` fields of the
/// vendored `.dat` files. `catalog` is the `pg_catalog` relation the described
/// object lives in, by name: codegen has no business knowing the fixed relation
/// OIDs, which [`registry`] owns and [`catalogs::description`] resolves through.
///
/// There is no `objsubid`: PostgreSQL's own bootstrap data describes whole
/// objects only — a fresh 18.4 has not one row with `objsubid > 0` — so the
/// column is 0 for every row here.
pub struct PgDescriptionRow {
    pub catalog: &'static str,
    pub objoid: u32,
    pub description: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/pg_type_rows.rs"));
include!(concat!(env!("OUT_DIR"), "/pg_cast_rows.rs"));
include!(concat!(env!("OUT_DIR"), "/pg_proc_rows.rs"));
include!(concat!(env!("OUT_DIR"), "/pg_opfamily_rows.rs"));
include!(concat!(env!("OUT_DIR"), "/pg_opclass_rows.rs"));
include!(concat!(env!("OUT_DIR"), "/pg_operator_rows.rs"));
include!(concat!(env!("OUT_DIR"), "/pg_aggregate_rows.rs"));
include!(concat!(env!("OUT_DIR"), "/pg_conversion_rows.rs"));
include!(concat!(env!("OUT_DIR"), "/pg_ts_parser_rows.rs"));
include!(concat!(env!("OUT_DIR"), "/pg_ts_template_rows.rs"));
include!(concat!(env!("OUT_DIR"), "/pg_ts_dict_rows.rs"));
include!(concat!(env!("OUT_DIR"), "/pg_ts_config_rows.rs"));
include!(concat!(env!("OUT_DIR"), "/pg_ts_config_map_rows.rs"));
include!(concat!(env!("OUT_DIR"), "/pg_amop_rows.rs"));
include!(concat!(env!("OUT_DIR"), "/pg_amproc_rows.rs"));
include!(concat!(env!("OUT_DIR"), "/pg_description_rows.rs"));

/// Whether `name` is the catalog name of a PostgreSQL built-in type, including
/// types crabgresql recognizes but does not implement yet (for example
/// `xml`). This distinguishes an unsupported built-in from a nonexistent
/// user type without maintaining a second hand-written name list.
pub fn is_builtin_type_name(name: &str) -> bool {
    PG_TYPE_ROWS.iter().any(|row| row.typname == name)
}

/// The OID of the built-in function `name`, or `None` if this build publishes
/// no `pg_proc` row for it. Only the functions the other catalogs reference are
/// published, so a name that is real upstream can still be absent here.
pub fn builtin_proc_oid(name: &str) -> Option<u32> {
    PG_PROC_ROWS
        .iter()
        .find(|row| row.proname == name)
        .map(|row| row.oid)
}

/// The name of the built-in function `oid`, the inverse of
/// [`builtin_proc_oid`].
pub fn builtin_proc_name(oid: u32) -> Option<&'static str> {
    PG_PROC_ROWS
        .iter()
        .find(|row| row.oid == oid)
        .map(|row| row.proname)
}

/// The name of the built-in operator `oid`, the inverse of
/// [`builtin_oper_oids`].
pub fn builtin_oper_name(oid: u32) -> Option<&'static str> {
    PG_OPERATOR_ROWS
        .iter()
        .find(|row| row.oid == oid)
        .map(|row| row.oprname)
}

/// The OIDs of every built-in operator named `name`, in `pg_operator` order.
/// Plural where [`builtin_proc_oid`] is singular: an operator name is shared by
/// every operand-type combination it is defined for, so `=` has some ninety.
pub fn builtin_oper_oids(name: &str) -> Vec<u32> {
    PG_OPERATOR_ROWS
        .iter()
        .filter(|row| row.oprname == name)
        .map(|row| row.oid)
        .collect()
}

/// Read-only engine serving `pg_catalog` relations. Constructed per statement so
/// its rows reflect current server state; the live state comes from a
/// [`CatalogSource`] whose every method is invoked at most once, and only when
/// the relation it feeds is opened.
pub struct SystemCatalog {
    source: Arc<dyn CatalogSource>,
    live_relations: OnceLock<Vec<CatalogRelation>>,
    oids: OnceLock<Vec<(u32, TableSchema)>>,
    kinds: OnceLock<Vec<RelKind>>,
    ddl_xids: OnceLock<Vec<Xid>>,
    xmin_by_oid: OnceLock<Arc<std::collections::HashMap<u32, Xid>>>,
    stats: OnceLock<Vec<RelStats>>,
    filenodes: OnceLock<Vec<RelationFilenodes>>,
    index_oids: OnceLock<Vec<CatalogIndex>>,
    toast_oids: OnceLock<Vec<CatalogToast>>,
    constraint_oids: OnceLock<Vec<CatalogConstraint>>,
    attrdef_oids: OnceLock<Vec<CatalogAttrDef>>,
    user_types: OnceLock<Vec<CatalogUserType>>,
    routines: OnceLock<Vec<CatalogRoutine>>,
    user_schemas: OnceLock<Vec<(String, u32)>>,
    cursors: OnceLock<Vec<CatalogCursor>>,
    prepared_statements: OnceLock<Vec<CatalogPreparedStatement>>,
    settings: OnceLock<Vec<CatalogSetting>>,
    locks: OnceLock<Vec<CatalogLock>>,
    database_stats: OnceLock<DbStatSnapshot>,
    table_stats: OnceLock<Vec<RelStatSnapshot>>,
    index_stats: OnceLock<Vec<IndexStatSnapshot>>,
    backends: OnceLock<Vec<CatalogBackend>>,
    view_dependencies: OnceLock<Vec<CatalogViewDependency>>,
    namespace_oids: OnceLock<std::collections::HashMap<String, u32>>,
}

impl Default for SystemCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemCatalog {
    /// A catalog with no live relations (built-in metadata only).
    pub fn new() -> Self {
        Self::from_source(Arc::new(StaticSource::default()))
    }

    /// A catalog reflecting a fixed set of live user relations into
    /// `pg_class`/`pg_attribute`.
    pub fn with_relations(relations: Vec<TableSchema>) -> Self {
        Self::from_source(Arc::new(StaticSource::new(
            relations
                .into_iter()
                .map(CatalogRelation::permanent)
                .collect(),
        )))
    }

    /// A catalog with session identity and a fixed set of live relations.
    pub fn with_catalog_relations(
        database: impl Into<String>,
        owner: impl Into<String>,
        relations: Vec<CatalogRelation>,
    ) -> Self {
        Self::from_source(Arc::new(
            StaticSource::new(relations).database(database).owner(owner),
        ))
    }

    /// A catalog reflecting `source`.
    ///
    /// The snapshot is per statement, not per session: every memoized answer
    /// below goes stale the moment any DDL runs. The `source` itself may be
    /// long-lived; the `SystemCatalog` wrapped around it must not be.
    pub fn from_source(source: Arc<dyn CatalogSource>) -> Self {
        Self {
            source,
            live_relations: OnceLock::new(),
            oids: OnceLock::new(),
            kinds: OnceLock::new(),
            ddl_xids: OnceLock::new(),
            xmin_by_oid: OnceLock::new(),
            stats: OnceLock::new(),
            filenodes: OnceLock::new(),
            index_oids: OnceLock::new(),
            toast_oids: OnceLock::new(),
            constraint_oids: OnceLock::new(),
            attrdef_oids: OnceLock::new(),
            user_types: OnceLock::new(),
            routines: OnceLock::new(),
            user_schemas: OnceLock::new(),
            cursors: OnceLock::new(),
            prepared_statements: OnceLock::new(),
            settings: OnceLock::new(),
            locks: OnceLock::new(),
            database_stats: OnceLock::new(),
            table_stats: OnceLock::new(),
            index_stats: OnceLock::new(),
            backends: OnceLock::new(),
            view_dependencies: OnceLock::new(),
            namespace_oids: OnceLock::new(),
        }
    }

    /// The accessors below are the only places `self.source` is read, apart
    /// from the four cheap answers (`database`, `owner`, `now`,
    /// `bytea_output`) that need no memoization. Keeping it that way is what
    /// makes the source's cost a per-snapshot one: a call site outside a
    /// `OnceLock` would re-enumerate the database on every row that reached it.
    pub(crate) fn live_relations(&self) -> &[CatalogRelation] {
        self.live_relations.get_or_init(|| self.source.relations())
    }

    fn user_types(&self) -> &[CatalogUserType] {
        self.user_types.get_or_init(|| self.source.user_types())
    }

    fn routines(&self) -> &[CatalogRoutine] {
        self.routines.get_or_init(|| self.source.routines())
    }

    pub(crate) fn user_schemas(&self) -> &[(String, u32)] {
        self.user_schemas.get_or_init(|| self.source.schemas())
    }

    fn cursors(&self) -> &[CatalogCursor] {
        self.cursors.get_or_init(|| self.source.cursors())
    }

    fn prepared_statements(&self) -> &[CatalogPreparedStatement] {
        self.prepared_statements
            .get_or_init(|| self.source.prepared_statements())
    }

    fn settings(&self) -> &[CatalogSetting] {
        self.settings.get_or_init(|| self.source.settings())
    }

    fn locks(&self) -> &[CatalogLock] {
        self.locks.get_or_init(|| self.source.locks())
    }

    fn database_stats(&self) -> &DbStatSnapshot {
        self.database_stats
            .get_or_init(|| self.source.database_stats())
    }

    fn table_stats(&self) -> &[RelStatSnapshot] {
        self.table_stats.get_or_init(|| self.source.table_stats())
    }

    fn index_stats(&self) -> &[IndexStatSnapshot] {
        self.index_stats.get_or_init(|| self.source.index_stats())
    }

    fn backends(&self) -> &[CatalogBackend] {
        self.backends.get_or_init(|| self.source.backends())
    }

    pub(crate) fn view_dependencies(&self) -> &[CatalogViewDependency] {
        self.view_dependencies
            .get_or_init(|| self.source.view_dependencies())
    }

    /// Map every namespace name to its OID: the built-in namespaces plus each
    /// user-created schema. Feeds `pg_class.relnamespace` /
    /// `pg_constraint.connamespace`.
    ///
    /// Memoized like the accessors above, and for a sharper reason: this one
    /// backs `pg_my_temp_schema()` and `pg_is_other_temp_schema()`, which the
    /// executor evaluates once per *row*. Rebuilt per call it was `O(#schemas)`
    /// of `String` cloning on every row that reached it.
    pub(crate) fn namespace_oids(&self) -> &std::collections::HashMap<String, u32> {
        self.namespace_oids.get_or_init(|| {
            let mut map = std::collections::HashMap::new();
            map.insert("pg_catalog".to_string(), 11);
            map.insert(TOAST_NAMESPACE.to_string(), 99);
            map.insert("public".to_string(), 2200);
            for (name, oid) in self.user_schemas() {
                map.insert(name.clone(), *oid);
            }
            map
        })
    }

    /// Assign a stable synthetic OID to each user relation, computed once and
    /// memoized. Sorted by name so `pg_class` and `pg_attribute` (built by
    /// separate `open_table` calls) agree on every relation's OID, keeping their
    /// join consistent.
    pub(crate) fn relation_oids(&self) -> &[(u32, TableSchema)] {
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

    /// The DDL generation for each entry of [`SystemCatalog::relation_oids`], in
    /// the same sorted order — the `xmin` that relation's own catalog rows
    /// report. `0` (nothing recorded) falls back to the catalog-wide generation
    /// here rather than at the reader, so a consumer never has to know about the
    /// sentinel.
    pub(crate) fn relation_ddl_xids(&self) -> &[Xid] {
        self.ddl_xids.get_or_init(|| {
            let fallback = self.source.catalog_xmin();
            let mut rels = self.live_relations().to_vec();
            rels.sort_by(|a, b| {
                a.namespace
                    .cmp(&b.namespace)
                    .then_with(|| a.schema.name.cmp(&b.schema.name))
            });
            rels.into_iter()
                .map(|r| Xid(if r.ddl_xid == 0 { fallback } else { r.ddl_xid }))
                .collect()
        })
    }

    /// `oid -> xmin` over [`SystemCatalog::relation_ddl_xids`], for a relation
    /// whose rows carry the OID of the relation they describe.
    pub(crate) fn relation_xmin_by_oid(&self) -> Arc<std::collections::HashMap<u32, Xid>> {
        Arc::clone(self.xmin_by_oid.get_or_init(|| {
            Arc::new(
                self.relation_oids()
                    .iter()
                    .zip(self.relation_ddl_xids())
                    .map(|((oid, _), xid)| (*oid, *xid))
                    .collect(),
            )
        }))
    }

    /// The relation kind for each entry of [`SystemCatalog::relation_oids`], in
    /// the same sorted order, so `pg_class` can emit the right `relkind`.
    pub(crate) fn relation_kinds(&self) -> &[RelKind] {
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
    pub(crate) fn relation_stats(&self) -> &[RelStats] {
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

    /// The physical file numbers for each entry of
    /// [`SystemCatalog::relation_oids`], in the same sorted order, feeding
    /// `pg_class.relfilenode`.
    pub(crate) fn relation_filenodes(&self) -> &[RelationFilenodes] {
        self.filenodes.get_or_init(|| {
            let mut rels = self.live_relations().to_vec();
            rels.sort_by(|a, b| {
                a.namespace
                    .cmp(&b.namespace)
                    .then_with(|| a.schema.name.cmp(&b.schema.name))
            });
            rels.into_iter().map(|r| r.filenodes).collect()
        })
    }

    /// The `(pg_class OID, params)` of each sequence, for `pg_sequence`. The OID
    /// matches the one [`SystemCatalog::relation_oids`] assigns (same sort), so
    /// `pg_sequence.seqrelid` joins `pg_class.oid`.
    pub(crate) fn sequence_entries(&self) -> Vec<(u32, CatalogSequence)> {
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

    pub(crate) fn index_oids(&self) -> &[CatalogIndex] {
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
                    let relfilenode = relation.filenodes.index(&index.name);
                    let stats = relation
                        .index_stats
                        .iter()
                        .find(|(name, _)| *name == index.name)
                        .map(|(_, stats)| stats.clone());
                    pending.push((
                        table_oid,
                        relation.schema.clone(),
                        index,
                        relfilenode,
                        stats,
                    ));
                }
            }
            pending.sort_by(|a, b| a.2.name.cmp(&b.2.name));
            pending
                .into_iter()
                .enumerate()
                .map(
                    |(position, (table_oid, table_schema, metadata, relfilenode, stats))| {
                        CatalogIndex {
                            oid: first_index_oid + position as u32,
                            table_oid,
                            table_schema,
                            metadata,
                            relfilenode,
                            stats,
                        }
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
    pub(crate) fn toast_oids(&self) -> &[CatalogToast] {
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
                    let relfilenode = relation.filenodes.toast;
                    relation
                        .toast
                        .map(|stats| (table_oid, relation.schema, stats, relfilenode))
                })
                .enumerate()
                .map(
                    |(slot, (table_oid, schema, stats, relfilenode))| CatalogToast {
                        oid: first_toast_oid + slot as u32,
                        table_oid,
                        // PostgreSQL names it after the parent's OID.
                        name: format!("pg_toast_{table_oid}"),
                        persistence: schema.persistence,
                        stats,
                        relfilenode,
                    },
                )
                .collect()
        })
    }

    /// The relation `oid` and every partitioned ancestor above it, innermost
    /// first — what `pg_partition_ancestors` reports.
    ///
    /// Empty for a relation that is neither a partition nor partitioned: that is
    /// PostgreSQL's answer (no rows), and it is what lets psql's `\d` join
    /// against this for any relation at all. The walk is bounded by the number
    /// of relations, so a parent chain that somehow cycles terminates instead of
    /// hanging.
    pub fn partition_ancestors(&self, oid: u32) -> Vec<u32> {
        let relations = self.relation_oids();
        let Some((_, schema)) = relations
            .get(oid.wrapping_sub(FIRST_REL_OID) as usize)
            .filter(|(stored, _)| *stored == oid)
        else {
            return Vec::new();
        };
        if schema.partition_scheme.is_none() && schema.partition_of.is_none() {
            return Vec::new();
        }
        let mut out = vec![oid];
        let mut current = schema;
        while let Some(part) = &current.partition_of {
            let Some((parent_oid, parent)) = relations
                .iter()
                .find(|(_, s)| s.name == part.parent_name && s.namespace == part.parent_namespace)
            else {
                break;
            };
            out.push(*parent_oid);
            current = parent;
            if out.len() > relations.len() {
                break;
            }
        }
        out
    }

    /// The `pg_rewrite` rules of this snapshot — one `_RETURN` per view —
    /// assigned OIDs from a **fifth block** beginning after the constraints, for
    /// the reason [`SystemCatalog::toast_oids`] gives: appending never moves an
    /// OID that already existed.
    ///
    /// Not memoized, unlike the blocks above: only `pg_rewrite` reads it, and
    /// nothing looks a rule up by OID.
    pub(crate) fn rewrite_oids(&self) -> Vec<CatalogRewrite> {
        let first = FIRST_REL_OID
            + self.relation_oids().len() as u32
            + self.index_oids().len() as u32
            + self.toast_oids().len() as u32
            + self.constraint_oids().len() as u32;
        let mut relations = self.live_relations().to_vec();
        // The same sort `relation_oids` uses, so `position` is the view's own
        // `pg_class` OID rather than a number that happens to look like one.
        relations.sort_by(|a, b| {
            a.namespace
                .cmp(&b.namespace)
                .then_with(|| a.schema.name.cmp(&b.schema.name))
        });
        relations
            .into_iter()
            .enumerate()
            .filter(|(_, relation)| relation.kind == RelKind::View)
            .enumerate()
            .map(|(slot, (position, relation))| CatalogRewrite {
                oid: first + slot as u32,
                view_oid: FIRST_REL_OID + position as u32,
                definition: relation.definition,
            })
            .collect()
    }

    /// The constraints of this snapshot, assigned OIDs from a **fourth block**
    /// beginning after the TOAST block — extending the invariant
    /// [`SystemCatalog::toast_oids`] documents by one more segment, for the same
    /// reason: appending keeps every OID that already existed where it was.
    ///
    /// Order within the block is not-null constraints first (by relation, then
    /// column position), then the index-backed ones in index-OID order. It only
    /// has to be *deterministic* — `pg_constraint` rows carry no inherent order,
    /// and callers that want one sort by `conname`.
    pub(crate) fn constraint_oids(&self) -> &[CatalogConstraint] {
        self.constraint_oids.get_or_init(|| {
            let indexes = self.index_oids();
            let first = FIRST_REL_OID
                + self.relation_oids().len() as u32
                + indexes.len() as u32
                + self.toast_oids().len() as u32;
            let mut out = Vec::new();
            for (table_oid, schema) in self.relation_oids() {
                for (position, column) in schema.columns.iter().enumerate() {
                    // Only a column with an explicit NOT NULL declaration
                    // carries a constraint name and becomes a row here; a
                    // PRIMARY KEY sets `Column::nullable` to false without
                    // recording one.
                    // TODO: emit the `<table>_<column>_not_null` rows
                    // PostgreSQL creates for every PRIMARY KEY column
                    // (upstream `constraints` regression test: "Primary keys
                    // cause not-null constraints to be created").
                    let Some(name) = &column.not_null_constraint else {
                        continue;
                    };
                    out.push(CatalogConstraint {
                        oid: first + out.len() as u32,
                        name: name.clone(),
                        contype: "n",
                        namespace: schema.namespace.clone(),
                        table_oid: *table_oid,
                        index_oid: 0,
                        columns: vec![position],
                        expr: None,
                        validated: true,
                        islocal: true,
                        inhcount: 0,
                    });
                }
                for check in &schema.checks {
                    out.push(CatalogConstraint {
                        oid: first + out.len() as u32,
                        name: check.name.clone(),
                        contype: "c",
                        namespace: schema.namespace.clone(),
                        table_oid: *table_oid,
                        // A check is not index-backed.
                        index_oid: 0,
                        columns: check.columns.clone(),
                        expr: Some(check.expr.clone()),
                        validated: check.validated,
                        islocal: check.islocal,
                        inhcount: check.inhcount,
                    });
                }
            }
            for index in indexes {
                let Some(constraint) = index.metadata.constraint else {
                    continue;
                };
                out.push(CatalogConstraint {
                    oid: first + out.len() as u32,
                    // An index-backed constraint and its index share a name.
                    name: index.metadata.name.clone(),
                    contype: match constraint {
                        IndexConstraint::PrimaryKey => "p",
                        IndexConstraint::Unique => "u",
                    },
                    namespace: index.table_schema.namespace.clone(),
                    table_oid: index.table_oid,
                    index_oid: index.oid,
                    columns: index.metadata.keys.iter().map(|key| key.column).collect(),
                    expr: None,
                    validated: true,
                    islocal: true,
                    inhcount: 0,
                });
            }
            out
        })
    }

    /// The column defaults of this snapshot, assigned OIDs from a **sixth
    /// block** beginning after the rewrite rules — the same appending rule
    /// [`SystemCatalog::toast_oids`] states, one segment further along.
    ///
    /// A generated column's expression is an `attrdef` row too: PostgreSQL keeps
    /// it in the same catalog and tells the two apart by `attgenerated`, which
    /// is why [`crate::catalogs::attribute`] reads one list for both.
    pub(crate) fn attrdef_oids(&self) -> &[CatalogAttrDef] {
        self.attrdef_oids.get_or_init(|| {
            let first = FIRST_REL_OID
                + self.relation_oids().len() as u32
                + self.index_oids().len() as u32
                + self.toast_oids().len() as u32
                + self.constraint_oids().len() as u32
                + self.rewrite_oids().len() as u32;
            let mut out = Vec::new();
            for (table_oid, schema) in self.relation_oids() {
                for (position, column) in schema.columns.iter().enumerate() {
                    let Some(expr) = column
                        .default
                        .as_ref()
                        .or(column.generated.as_ref().map(|g| &g.expr))
                    else {
                        continue;
                    };
                    out.push(CatalogAttrDef {
                        oid: first + out.len() as u32,
                        table_oid: *table_oid,
                        attnum: (position + 1) as i16,
                        expr: expr.clone(),
                    });
                }
            }
            out
        })
    }

    /// The sequence column `column` of relation `oid` owns — what
    /// `pg_get_serial_sequence` reports.
    ///
    /// The column name is matched **exactly**: PostgreSQL takes this argument
    /// literally rather than folding it, so `pg_get_serial_sequence('t',
    /// 'ColX')` finds a column `"ColX"` and `'colx'` does not.
    ///
    /// Answers from [`catalogs::depend::owned_sequences`], the same rule
    /// `pg_depend`'s auto edge is built from.
    ///
    /// A `pg_catalog` relation answers too: nothing there owns a sequence, but
    /// its columns still exist, and PostgreSQL distinguishes
    /// `pg_get_serial_sequence('pg_class', 'relname')` (NULL) from
    /// `('pg_class', 'nosuchcol')` (`42703`). Name resolution hands over the
    /// catalog's own OID for those, so without this they would all collapse to
    /// the NULL.
    pub fn serial_sequence(&self, oid: u32, column: &str) -> SerialSequenceLookup {
        let Some((_, schema)) = self.relation_oids().iter().find(|(rel, _)| *rel == oid) else {
            return self.catalog_relation_column(oid, column);
        };
        let Some(position) = schema.column_index(column) else {
            return SerialSequenceLookup::NoColumn {
                relation: schema.name.clone(),
            };
        };
        let owned = catalogs::depend::owned_sequences(self)
            .into_iter()
            .find(|owned| owned.table_oid == oid && owned.column == position);
        let Some(owned) = owned else {
            return SerialSequenceLookup::Unowned;
        };
        match self
            .relation_oids()
            .iter()
            .find(|(rel, _)| *rel == owned.sequence_oid)
        {
            Some((_, sequence)) => SerialSequenceLookup::Owned {
                namespace: sequence.namespace.clone(),
                name: sequence.name.clone(),
            },
            None => SerialSequenceLookup::Unowned,
        }
    }

    /// [`SystemCatalog::serial_sequence`] for an OID that is not a live user
    /// relation: a served `pg_catalog` relation reports whether it has the
    /// column, anything else reports nothing at all.
    fn catalog_relation_column(&self, oid: u32, column: &str) -> SerialSequenceLookup {
        let Some(name) = registry::builtin_relation_name(oid) else {
            return SerialSequenceLookup::NoRelation;
        };
        let Some(def) = registry::lookup(CatalogNamespace::PgCatalog, name) else {
            return SerialSequenceLookup::NoRelation;
        };
        match (def.schema)().column_index(column) {
            Some(_) => SerialSequenceLookup::Unowned,
            None => SerialSequenceLookup::NoColumn {
                relation: name.to_string(),
            },
        }
    }

    /// The index `oid` identifies and the table it is defined on, resolved
    /// against the same numbering [`SystemCatalog::index_oids`] hands out — so
    /// `pg_get_indexdef` and the `pg_class`/`pg_index` rows agree by
    /// construction.
    ///
    /// The OID is the *index's* own, from the block that follows the relations;
    /// looking one up by the table's OID would find nothing. Indexed rather than
    /// scanned, for the reason [`Self::constraint_def`] gives.
    pub fn index_def(&self, oid: u32) -> Option<(IndexMetadata, TableSchema)> {
        let indexes = self.index_oids();
        let base = indexes.first()?.oid;
        let index = indexes.get(oid.checked_sub(base)? as usize)?;
        if index.oid != oid {
            return None;
        }
        Some((index.metadata.clone(), index.table_schema.clone()))
    }

    /// The constraint `oid` identifies, resolved against the same numbering
    /// [`SystemCatalog::constraint_oids`] hands out — so `pg_get_constraintdef`
    /// and the `pg_constraint` rows agree by construction.
    ///
    /// Column *names* rather than positions, because the caller renders DDL and
    /// has no schema in hand. An unknown OID is `None`, which PostgreSQL reports
    /// as a NULL result rather than an error.
    ///
    /// Both lookups index rather than scan: each block is one dense run, so the
    /// offset from its base *is* the position. `pg_get_constraintdef` runs once
    /// per output row, and a linear scan here made
    /// `SELECT pg_get_constraintdef(oid) FROM pg_constraint` quadratic — over a
    /// list whose elements are whole `TableSchema`s. The stored OID is still
    /// compared afterwards, so a future non-positional assignment degrades to
    /// not-found rather than to the wrong constraint (as in [`Self::relation_ref`]).
    pub fn constraint_def(&self, oid: u32) -> Option<(String, Vec<String>, Option<String>)> {
        let constraints = self.constraint_oids();
        let base = constraints.first()?.oid;
        let constraint = constraints.get(oid.checked_sub(base)? as usize)?;
        if constraint.oid != oid {
            return None;
        }
        let relations = self.relation_oids();
        let (stored, schema) =
            relations.get(constraint.table_oid.checked_sub(FIRST_REL_OID)? as usize)?;
        if *stored != constraint.table_oid {
            return None;
        }
        let columns = constraint
            .columns
            .iter()
            .filter_map(|position| schema.columns.get(*position))
            .map(|column| column.name.clone())
            .collect();
        Some((
            constraint.contype.to_string(),
            columns,
            constraint.expr.clone(),
        ))
    }

    /// The name of the role `oid` identifies, or `None` if no role has that OID.
    /// Every catalog row reports [`oids::BOOTSTRAP_ROLE_OID`] as its owner
    /// (crabgresql has no role catalog), so exactly one OID resolves — to this
    /// snapshot's session user. Backs `pg_get_userbyid`.
    pub fn role_name(&self, oid: u32) -> Option<&str> {
        (oid == oids::BOOTSTRAP_ROLE_OID).then_some(self.source.owner())
    }

    /// The name of the function `oid` identifies, or `None` if this snapshot
    /// has none. Backs `regproc` output: built-in rows first, then this
    /// session's `CREATE FUNCTION` routines.
    pub fn proc_name(&self, oid: u32) -> Option<String> {
        builtin_proc_name(oid)
            .map(str::to_string)
            .or_else(|| self.routine_by_oid(oid).map(|r| r.name.clone()))
    }

    /// The OID of the function `namespace.name` names, or `None` when there is
    /// no such function *or* the name is carried by more than one — `regprocin`
    /// resolves a bare name, so an overloaded one is not resolvable this way.
    pub fn proc_oid(&self, namespace: Option<&str>, name: &str) -> Option<u32> {
        // Built-ins all live in `pg_catalog`, so any other qualifier names a
        // user routine instead.
        let in_catalog = !matches!(namespace, Some(ns) if ns != "pg_catalog");
        if let Some(oid) = builtin_proc_oid(name).filter(|_| in_catalog) {
            return Some(oid);
        }
        let mut matched = self
            .routines()
            .iter()
            .filter(|r| r.name == name && namespace.is_none_or(|ns| ns == r.namespace))
            .map(|r| r.oid);
        let first = matched.next()?;
        matched.next().is_none().then_some(first)
    }

    /// Backs `regoper` output. Every operator this build publishes is a
    /// built-in, so the namespace is always `pg_catalog` — `CREATE OPERATOR` is
    /// not implemented, and when it is this reads the user rows the same way
    /// [`SystemCatalog::proc_name`] reads `CREATE FUNCTION`'s.
    pub fn oper_name(&self, oid: u32) -> Option<(String, String)> {
        builtin_oper_name(oid).map(|name| ("pg_catalog".to_string(), name.to_string()))
    }

    /// Built-ins all live in `pg_catalog`, so any other qualifier names
    /// nothing. Why the whole list comes back: `CatalogOps` in
    /// `crabgresql-executor`.
    pub fn oper_oids(&self, namespace: Option<&str>, name: &str) -> Vec<u32> {
        if matches!(namespace, Some(ns) if ns != "pg_catalog") {
            return Vec::new();
        }
        builtin_oper_oids(name)
    }

    fn routine_by_oid(&self, oid: u32) -> Option<CatalogRoutine> {
        self.routines().iter().find(|r| r.oid == oid).cloned()
    }

    /// The database this snapshot's session is connected to. Backs
    /// `current_database()`/`current_catalog`.
    pub fn database(&self) -> &str {
        self.source.database()
    }

    /// The session user. Backs `current_user`/`session_user`, and is the name
    /// every catalog row's owner OID resolves to.
    pub fn owner(&self) -> &str {
        self.source.owner()
    }

    /// The `(namespace, name)` of the relation `oid` identifies — a
    /// table/view/sequence or an index — or `None` if this snapshot has no such
    /// relation. Backs `pg_table_is_visible`, which needs the name as well as
    /// the namespace to answer *reachability* rather than mere membership.
    ///
    /// Reads exactly the fields [`catalogs::class::pg_class_rows`] reports as
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

    /// The physical size, in 8 KB pages, of everything the relation `oid`
    /// identifies owns — its own storage, its TOAST relation and its indexes,
    /// kept apart because the four size functions add them up differently
    /// (`pg_relation_size` is `main` alone, `pg_table_size` adds `toast`,
    /// `pg_total_relation_size` adds `indexes` on top).
    ///
    /// `None` means "no such relation", which those functions report as NULL.
    /// It is distinct from a relation with nothing to measure, which answers
    /// zeros — as PostgreSQL does for a view, a partitioned parent, and (here
    /// only) a `pg_catalog` relation, which exists but has no file behind it.
    ///
    /// A **live** count where the engine can give one: the page count comes from
    /// [`RelStats::curpages`], not the `relpages` frozen at the last `ANALYZE`
    /// that `pg_class` must report. `pg_relation_size` is a physical question,
    /// and PostgreSQL answers it by measuring the file rather than reading the
    /// catalog.
    ///
    /// A sequence is one page from creation, matching both PostgreSQL's 8192 and
    /// the `relpages = 1` this catalog already publishes for it.
    ///
    /// Indexed positionally, exactly as [`SystemCatalog::relation_ref`] is, and
    /// for the same reason.
    pub fn relation_pages(&self, oid: u32) -> Option<RelationPages> {
        // A `pg_catalog` relation is real but has no storage: its rows are
        // built per statement. Zero, not `None` — the relation exists.
        if builtin_relation_name(oid).is_some() {
            return Some(RelationPages::default());
        }
        let offset = oid.checked_sub(FIRST_REL_OID)? as usize;
        let relations = self.relation_oids();
        if let Some((stored, _)) = relations.get(offset) {
            if *stored != oid {
                return None;
            }
            let kind = self.relation_kinds()[offset];
            // A sequence's single page is its fixed shape, not a measurement:
            // nothing here stores one in a heap file to measure.
            if matches!(kind, RelKind::Sequence) {
                return Some(RelationPages {
                    main: 1,
                    ..RelationPages::default()
                });
            }
            let stats = &self.relation_stats()[offset];
            return Some(RelationPages {
                main: stats.curpages.unwrap_or(stats.relpages),
                toast: self
                    .toast_oids()
                    .iter()
                    .find(|toast| toast.table_oid == oid)
                    .map_or(0, |toast| toast.stats.relpages),
                indexes: self
                    .index_oids()
                    .iter()
                    .filter(|index| index.table_oid == oid)
                    .map(|index| index.stats.as_ref().map_or(0, |stats| stats.relpages))
                    .sum(),
            });
        }
        let indexes = self.index_oids();
        if let Some(index) = indexes.get(offset - relations.len()) {
            if index.oid != oid {
                return None;
            }
            // An index's own indexes are itself, which is why PostgreSQL reports
            // `pg_indexes_size(<index>) = 0` while `pg_relation_size` of the
            // same OID is its whole size.
            return Some(RelationPages {
                main: index.stats.as_ref().map_or(0, |stats| stats.relpages),
                ..RelationPages::default()
            });
        }
        let toast = self
            .toast_oids()
            .get(offset - relations.len() - indexes.len())?;
        (toast.oid == oid).then(|| RelationPages {
            main: toast.stats.relpages,
            ..RelationPages::default()
        })
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

    /// Narrower than [`SystemCatalog::relation_oid_in`], which resolves an
    /// index by its own name alone: matching the owning relation too is what
    /// stops a stale counter for a dropped index from landing on a like-named
    /// one elsewhere.
    pub fn index_oid_in(&self, namespace: &str, table: &str, index: &str) -> Option<u32> {
        self.index_oids()
            .iter()
            .find(|candidate| {
                candidate.table_schema.namespace == namespace
                    && candidate.table_schema.name == table
                    && candidate.metadata.name == index
            })
            .map(|candidate| candidate.oid)
    }

    /// Whether `name` is a `pg_catalog` relation this catalog serves.
    ///
    /// A name lookup, not a build. It used to answer with
    /// `build_pg_catalog(name).is_some()`, which materialized the relation's
    /// whole row set and threw it away — and `rel_oid` calls this per row, so
    /// `SELECT 'pg_timezone_names'::regclass FROM generate_series(1, 10000)`
    /// enumerated the tz database ten thousand times (measured: 2.1 s against
    /// 0.03 s for `pg_class`).
    ///
    /// The served set and the OID table are the same table since the registry
    /// (`registry::CATALOG_RELATIONS`), so the two cannot drift and this is a
    /// binary search rather than a build.
    pub fn has_catalog_relation(&self, name: &str) -> bool {
        builtin_relation_oid(name).is_some()
    }

    /// The name of the schema `oid` identifies, or `None` if this snapshot has
    /// no such schema. The inverse of [`SystemCatalog::namespace_oid`]; both back
    /// `regnamespace`, and both read the same table `pg_namespace` is built from.
    pub fn namespace_name(&self, oid: u32) -> Option<String> {
        self.namespace_oids()
            .iter()
            .find(|(_, o)| **o == oid)
            .map(|(name, _)| name.clone())
    }

    pub fn namespace_oid(&self, name: &str) -> Option<u32> {
        self.namespace_oids().get(name).copied()
    }

    /// The `(namespace, name)` of the user type `oid` identifies, or `None` for
    /// an OID no `CREATE TYPE` has. Built-in types are not here — they resolve
    /// without a catalog.
    ///
    /// TODO: user types carry no namespace of their own — `CREATE TYPE app.t`
    /// is rejected — so every one of them reports `public`, which is where an
    /// unqualified name finds them.
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

    /// The schema and rows of the `pg_catalog` relation `name`, or `None` if
    /// this build serves no such relation.
    ///
    /// For tests only: a query goes through [`SystemCatalog::open`], which wraps
    /// the same pair in the access method — but asserting on rows through that
    /// would mean driving a transaction to scan them.
    #[cfg(test)]
    fn build_pg_catalog(&self, name: &str) -> Option<(TableSchema, Vec<Vec<Value>>)> {
        self.build(CatalogNamespace::PgCatalog, name)
    }

    /// The `information_schema` counterpart of [`SystemCatalog::build_pg_catalog`].
    #[cfg(test)]
    fn build_information_schema(&self, name: &str) -> Option<(TableSchema, Vec<Vec<Value>>)> {
        self.build(CatalogNamespace::InformationSchema, name)
    }

    /// Materialize `namespace.name` from the registry.
    ///
    /// TODO(perf): nothing is cached, so `pg_class a, pg_class b` builds the
    /// relation twice and every scan of `pg_proc` rebuilds ~500 rows. The cache
    /// belongs one level up, in [`SystemCatalog::open`], which wraps this in a
    /// [`StaticTable`] that already holds its schema and rows behind `Arc`s, so
    /// caching the `Arc<dyn TableAm>` is a refcount bump where caching the owned
    /// pair returned here would clone every row on each hit. Safe either way — a
    /// `SystemCatalog` lives exactly one statement.
    #[cfg(test)]
    fn build(
        &self,
        namespace: CatalogNamespace,
        name: &str,
    ) -> Option<(TableSchema, Vec<Vec<Value>>)> {
        let def = registry::lookup(namespace, name)?;
        Some(((def.schema)(), (def.rows)(self)))
    }

    /// Open a served relation as a [`StaticTable`].
    ///
    /// A deferred relation ([`registry::CatalogRelDef::deferred`]) is handed a
    /// builder rather than rows, and builds them against a snapshot of its own
    /// when the scan first reads it. The second snapshot is what makes the
    /// deferral possible at all — this one is behind a `&self` the builder
    /// cannot outlive — and it costs one relation enumeration on a relation
    /// nothing scans in a hot path. Both snapshots read the same source, so they
    /// assign the same OIDs.
    fn open(
        &self,
        namespace: CatalogNamespace,
        name: &str,
    ) -> Result<Arc<dyn TableAm>, StorageError> {
        let Some(def) = registry::lookup(namespace, name) else {
            return Err(StorageError::TableNotFound(name.to_string()));
        };
        let schema = (def.schema)();
        let xmin = Xid(self.source.catalog_xmin());
        if !def.deferred {
            // A relation whose rows describe other relations reports each row's
            // own generation; everything else reports the catalog-wide one.
            // A named column the schema does not have degrades to the latter
            // rather than failing the scan — `xmin_columns_exist` is the test
            // that keeps the registry honest.
            let table = StaticTable::new(schema, (def.rows)(self));
            return Ok(Arc::new(self.with_catalog_xmin(table, def, xmin)));
        }
        let source = Arc::clone(&self.source);
        let build = def.rows;
        let deferred = StaticTable::deferred(schema, move || {
            build(&SystemCatalog::from_source(Arc::clone(&source)))
        });
        Ok(Arc::new(self.with_catalog_xmin(deferred, def, xmin)))
    }

    /// Give `table` the `xmin` its registry entry asks for: the described
    /// relation's own DDL generation when the entry names the column holding
    /// that relation's OID, else the catalog-wide one.
    ///
    /// A named column the schema does not have degrades to the catalog-wide
    /// value rather than failing the scan; `xmin_columns_exist_and_are_oids` is
    /// the test that keeps the registry honest.
    fn with_catalog_xmin(
        &self,
        table: StaticTable,
        def: &registry::CatalogRelDef,
        xmin: Xid,
    ) -> StaticTable {
        match def
            .xmin_column
            .and_then(|name| table.schema().column_index(name))
        {
            None => table.with_xmin(xmin),
            Some(column) => table.with_relation_xmin(column, self.relation_xmin_by_oid(), xmin),
        }
    }

    /// The instant the timezone relations resolve their offsets at; see
    /// [`CatalogSource::now`].
    pub(crate) fn now(&self) -> i64 {
        self.source.now()
    }

    /// This connection's backend id; see [`CatalogSource::backend_pid`].
    pub fn backend_pid(&self) -> i32 {
        self.source.backend_pid()
    }

    /// The reading session's `bytea_output`; see [`CatalogSource::bytea_output`].
    pub(crate) fn bytea_output(&self) -> crabgresql_types::ByteaOutput {
        self.source.bytea_output()
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
        self.open(CatalogNamespace::PgCatalog, name)
    }

    fn resolve(
        &self,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Arc<dyn TableAm>, StorageError> {
        match namespace {
            None | Some("pg_catalog") => self.open(CatalogNamespace::PgCatalog, name),
            Some("information_schema") => self.open(CatalogNamespace::InformationSchema, name),
            Some(_) => Err(StorageError::TableNotFound(name.to_string())),
        }
    }

    fn drop_table(&self, _namespace: &str, name: &str) -> Result<(), StorageError> {
        // The session catalog routes DROP through temp/global, never here; a
        // system catalog relation is not droppable.
        unreachable!("cannot drop relation \"{name}\" from the system catalog")
    }
}
