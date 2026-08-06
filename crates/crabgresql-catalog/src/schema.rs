//! `TableSchema` definitions and row builders for the supported `pg_catalog`
//! relations.
//!
//! The column list for each relation follows PostgreSQL's column *names* and
//! order for the frequently-queried leading columns. Fidelity deviations are
//! deliberate and documented (see the crate root): catalog-only types we do not
//! model yet are omitted rather than faked.

use std::collections::HashMap;

use crabgresql_storage_api::{
    Column, IndexConstraint, IndexMethod, PartitionBoundDatum, PartitionOf, PartitionStrategy,
    RelStats, TableAccessMethod, TableSchema,
};
use crabgresql_types::{PgType, Reg, RegKind, Value, VectorKind};

use crate::{
    CatalogConstraint, CatalogCursor, CatalogIndex, CatalogRelation, CatalogRoutine,
    CatalogSequence, CatalogSetting, CatalogToast, CatalogUserType, PG_CAST_ROWS, PG_PROC_ROWS,
    PG_TYPE_ROWS, ProcRef, RelKind, TOAST_NAMESPACE,
};

/// Synthetic OID base for `pg_enum` rows (one per enum label). Chosen above the
/// built-in ranges so a per-label OID never collides with a type/relation OID.
const FIRST_ENUM_OID: u32 = 0x8000_0000;

/// A `"char"` column: PostgreSQL's one-byte ad-hoc type, which is what the
/// catalog's flag columns (`typtype`, `typcategory`, `relkind`, `provolatile`,
/// `castcontext`, …) really are.
const CHARLIKE: PgType = PgType::Char;

/// A `regproc` column: an OID that names a function and prints as that
/// function's name. Distinct from [`CHARLIKE`], which the two shared until the
/// alias was split — `typinput` and friends hold multi-character names a
/// one-byte type would truncate.
const REGPROC: PgType = PgType::Reg(RegKind::Proc);

fn col(name: &str, ty: PgType) -> Column {
    Column::new(name, ty)
}

/// A `"char"` datum from the single character the catalogs spell it with.
fn chr(c: char) -> Value {
    Value::Char(c as u8)
}

/// A `"char"` datum from a string the codegen or a catalog struct carries as
/// text. An empty string becomes `\0`, which is how PostgreSQL stores an unset
/// flag and prints back as the empty string.
fn str_char(s: &str) -> Value {
    Value::Char(s.bytes().next().unwrap_or(0))
}

/// A `regproc` datum from a codegen-resolved reference.
fn regproc(r: ProcRef) -> Value {
    Value::Reg(Reg {
        kind: RegKind::Proc,
        oid: r.oid,
        name: r.name.to_string(),
    })
}

/// A `regproc` datum for a function named at runtime rather than by codegen —
/// an access method handler, say. An unknown name is `0`, which prints as `-`
/// exactly as PostgreSQL renders a missing reference.
fn regproc_by_name(name: &str) -> Value {
    let own = OWN_AM_HANDLERS
        .iter()
        .find(|(_, handler)| *handler == name)
        .map(|(oid, _)| *oid);
    match own.or_else(|| crate::builtin_proc_oid(name)) {
        Some(oid) => Value::Reg(Reg {
            kind: RegKind::Proc,
            oid,
            name: name.to_string(),
        }),
        None => Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
    }
}

/// `pg_catalog.pg_type` — a curated, PG-ordered subset of the columns clients
/// query. The rarely-read domain and ACL columns (`typnotnull`, `typtypmod`,
/// `typndims`, `typdefaultbin`, `typdefault`, `typacl`) are omitted for now.
pub fn pg_type_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_type",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("typname", PgType::Name),
            col("typnamespace", PgType::Oid),
            col("typowner", PgType::Oid),
            col("typlen", PgType::Int2),
            col("typbyval", PgType::Bool),
            col("typtype", CHARLIKE),
            col("typcategory", CHARLIKE),
            col("typispreferred", PgType::Bool),
            col("typisdefined", PgType::Bool),
            col("typdelim", CHARLIKE),
            col("typrelid", PgType::Oid),
            col("typsubscript", REGPROC),
            col("typelem", PgType::Oid),
            col("typarray", PgType::Oid),
            col("typinput", REGPROC),
            col("typoutput", REGPROC),
            col("typreceive", REGPROC),
            col("typsend", REGPROC),
            col("typmodin", REGPROC),
            col("typmodout", REGPROC),
            col("typanalyze", REGPROC),
            col("typalign", CHARLIKE),
            col("typstorage", CHARLIKE),
            col("typbasetype", PgType::Oid),
            col("typcollation", PgType::Oid),
        ],
    )
}

/// `pg_catalog.pg_collation` — the collations this build ships. `collversion`
/// is omitted: it exists so PostgreSQL can warn when the underlying OS locale
/// data changes under an index, and the ICU data here is compiled in, so there
/// is no external version to drift from.
pub fn pg_collation_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_collation",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("collname", PgType::Name),
            col("collnamespace", PgType::Oid),
            col("collowner", PgType::Oid),
            col("collprovider", CHARLIKE),
            col("collisdeterministic", PgType::Bool),
            col("collencoding", PgType::Int4),
            col("collcollate", PgType::Text),
            col("collctype", PgType::Text),
            col("colllocale", PgType::Text),
        ],
    )
}

/// The `pg_collation` rows, one per collation in the shared registry — the same
/// list [`crabgresql_types::collation::compare_str`] orders strings by, so what
/// the catalog advertises and what queries actually do cannot drift.
pub fn pg_collation_rows() -> Vec<Vec<Value>> {
    crabgresql_types::collation::COLLATIONS
        .iter()
        .map(|c| {
            let opt_text = |s: Option<&str>| s.map_or(Value::Null, |s| Value::Text(s.to_string()));
            vec![
                Value::Oid(c.oid),
                Value::Text(c.name.to_string()),
                // Every collation lives in pg_catalog (11), owned by the
                // bootstrap superuser.
                Value::Oid(11),
                Value::Oid(BOOTSTRAP_ROLE_OID),
                chr(c.provider.as_char()),
                Value::Bool(c.deterministic),
                Value::Int4(c.encoding),
                opt_text(c.libc_locale),
                opt_text(c.libc_locale),
                opt_text(c.locale),
            ]
        })
        .collect()
}

/// The collation of values of `oid`'s type, or `0` when the type is not
/// collatable. An OID this build does not model has no collation.
///
/// An array takes its element's collation, as PostgreSQL records it — `text[]`
/// sorts under `default`, `name[]` under `C`. That is deliberately *not* spelled
/// as `PgType::is_collatable`, which stays false for `Array`: it also answers
/// `is_text_family`, so widening it would change operator selection and the
/// `COLLATE` acceptance gate. Nothing is lost by the split, because comparing
/// two arrays already compares their elements under the default collation.
///
/// The generated rows carry their own `typcollation` (from `pg_type.dat`); this
/// is the runtime path, for a column whose type this build models.
pub(crate) fn typcollation_of(oid: u32) -> u32 {
    match PgType::from_oid(oid) {
        Some(PgType::Array(elem)) => typcollation_of(elem),
        Some(ty) => crabgresql_types::collation::type_collation(ty),
        None => 0,
    }
}

/// The built-in `pg_type` rows generated from `pg_type.dat`. Callers append any
/// user-defined-type rows (a later slice) after these.
pub fn pg_type_builtin_rows() -> Vec<Vec<Value>> {
    PG_TYPE_ROWS
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Text(r.typname.to_string()),
                Value::Oid(r.typnamespace),
                Value::Oid(r.typowner),
                Value::Int2(r.typlen),
                Value::Bool(r.typbyval),
                str_char(r.typtype),
                str_char(r.typcategory),
                Value::Bool(r.typispreferred),
                Value::Bool(r.typisdefined),
                str_char(r.typdelim),
                Value::Oid(r.typrelid),
                regproc(r.typsubscript),
                Value::Oid(r.typelem),
                Value::Oid(r.typarray),
                regproc(r.typinput),
                regproc(r.typoutput),
                regproc(r.typreceive),
                regproc(r.typsend),
                regproc(r.typmodin),
                regproc(r.typmodout),
                regproc(r.typanalyze),
                str_char(r.typalign),
                str_char(r.typstorage),
                // typbasetype: nonzero only for a domain, and `pg_type.dat` has
                // none — every entry in it is a base or pseudo type, and a
                // derived array row is not a domain either.
                Value::Oid(0),
                Value::Oid(r.typcollation),
            ]
        })
        .collect()
}

/// The `pg_type` rows for user-defined enum types, appended after
/// [`pg_type_builtin_rows`]. Only enums are reflected (`typtype = 'e'`); other
/// `CREATE TYPE` shapes are not surfaced here yet. Column order matches
/// [`pg_type_schema`].
pub fn pg_type_user_rows(user_types: &[CatalogUserType]) -> Vec<Vec<Value>> {
    user_types
        .iter()
        .filter(|t| t.enum_labels.is_some())
        .map(|t| {
            vec![
                Value::Oid(t.oid),
                Value::Text(t.name.clone()),
                // `public`, where CREATE TYPE puts a user type — which is also
                // what `SystemCatalog::user_type_ref` reports for it, and what
                // lets a user type share a name with a built-in. Owner is the
                // bootstrap superuser, as elsewhere.
                Value::Oid(PUBLIC_NAMESPACE_OID),
                Value::Oid(BOOTSTRAP_ROLE_OID),
                // Enums are a fixed 4-byte, pass-by-value, OID-backed type.
                Value::Int2(4),
                Value::Bool(true),
                chr('e'),
                chr('E'),
                Value::Bool(false),
                Value::Bool(true),
                chr(','),
                // typrelid / typsubscript: an enum is not a composite and is
                // not subscriptable.
                Value::Oid(0),
                Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
                Value::Oid(0),
                Value::Oid(0),
                regproc_by_name("enum_in"),
                regproc_by_name("enum_out"),
                regproc_by_name("enum_recv"),
                regproc_by_name("enum_send"),
                // typmodin / typmodout / typanalyze: an enum takes no modifier
                // and uses the default statistics routine. All three are `-` on
                // a `CREATE TYPE ... AS ENUM` row (probed against 18.4).
                Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
                Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
                Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
                chr('i'),
                chr('p'),
                // typbasetype: an enum is not a domain.
                Value::Oid(0),
                // An enum is not collatable.
                Value::Oid(0),
            ]
        })
        .collect()
}

/// `pg_catalog.pg_enum` — one row per (enum type, label). `enumsortorder` is the
/// 1-based definition position (PG stores a float4 so labels can be inserted
/// between existing ones; a freshly created enum uses 1, 2, 3, …).
pub fn pg_enum_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_enum",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("enumtypid", PgType::Oid),
            col("enumsortorder", PgType::Float4),
            col("enumlabel", PgType::Name),
        ],
    )
}

/// The `pg_enum` rows for every user-defined enum type, in a stable order (by
/// type OID, then definition order). Per-label OIDs are synthetic.
pub fn pg_enum_rows(user_types: &[CatalogUserType]) -> Vec<Vec<Value>> {
    let mut enums: Vec<&CatalogUserType> = user_types
        .iter()
        .filter(|t| t.enum_labels.is_some())
        .collect();
    enums.sort_by_key(|t| t.oid);
    let mut rows = Vec::new();
    let mut next_oid = FIRST_ENUM_OID;
    for t in enums {
        let labels = t.enum_labels.as_deref().unwrap_or_default();
        for (i, label) in labels.iter().enumerate() {
            rows.push(vec![
                Value::Oid(next_oid),
                Value::Oid(t.oid),
                Value::Float4((i + 1) as f32),
                Value::Text(label.clone()),
            ]);
            next_oid += 1;
        }
    }
    rows
}

/// `pg_catalog.pg_cast` — the built-in casts between types crabgresql exposes.
pub fn pg_cast_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_cast",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("castsource", PgType::Oid),
            col("casttarget", PgType::Oid),
            col("castfunc", PgType::Oid),
            col("castcontext", CHARLIKE),
            col("castmethod", CHARLIKE),
        ],
    )
}

/// The built-in `pg_cast` rows generated from `pg_cast.dat` (restricted to casts
/// between exposed types).
pub fn pg_cast_rows() -> Vec<Vec<Value>> {
    PG_CAST_ROWS
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Oid(r.castsource),
                Value::Oid(r.casttarget),
                Value::Oid(r.castfunc),
                str_char(r.castcontext),
                str_char(r.castmethod),
            ]
        })
        .collect()
}

/// `pg_catalog.pg_sequence` — the definition of each user sequence, one row per
/// [`RelKind::Sequence`] relation, keyed by its `pg_class` OID (`seqrelid`).
pub fn pg_sequence_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_sequence",
        "pg_catalog",
        vec![
            col("seqrelid", PgType::Oid),
            col("seqtypid", PgType::Oid),
            col("seqstart", PgType::Int8),
            col("seqincrement", PgType::Int8),
            col("seqmax", PgType::Int8),
            col("seqmin", PgType::Int8),
            col("seqcache", PgType::Int8),
            col("seqcycle", PgType::Bool),
        ],
    )
}

pub fn pg_sequence_rows(sequences: &[(u32, CatalogSequence)]) -> Vec<Vec<Value>> {
    sequences
        .iter()
        .map(|(oid, s)| {
            vec![
                Value::Oid(*oid),
                Value::Oid(s.type_oid),
                Value::Int8(s.start),
                Value::Int8(s.increment),
                Value::Int8(s.max),
                Value::Int8(s.min),
                Value::Int8(s.cache),
                Value::Bool(s.cycle),
            ]
        })
        .collect()
}

/// `pg_catalog.pg_namespace` — the schemas visible on a fresh cluster.
pub fn pg_namespace_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_namespace",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("nspname", PgType::Name),
            col("nspowner", PgType::Oid),
            // aclitem[]; represented as text and always NULL (default ACL) here.
            col("nspacl", PgType::Text),
        ],
    )
}

/// OID assigned to the heap access method (`pg_am` row `heap` = 2). Reported for
/// every user relation's `relam`.
const HEAP_AM_OID: u32 = 2;
/// Stable OID assigned to the managed Parquet table method. PostgreSQL has no
/// such method, so the value is crabgresql's own — but it must stay *below*
/// `FIRST_USER_OID` (16384), the point where the server's OID allocator starts
/// handing out OIDs to user objects. A built-in catalog row at 16384 would share
/// its OID with the first `CREATE TYPE`/`CREATE SCHEMA`, breaking the
/// cluster-wide uniqueness clients assume. PostgreSQL reserves 1..16383 for
/// built-ins for exactly this reason.
pub const PARQUET_AM_OID: u32 = 16_000;
/// Stable OID of the managed buffer table method; see [`PARQUET_AM_OID`] for why
/// crabgresql's own methods sit below `FIRST_USER_OID`.
pub const BUFFER_AM_OID: u32 = 16_001;
/// OID of the `btree` index access method, shared by `pg_am` and the `relam` of
/// every B-tree index's `pg_class` row so the join between them holds.
const BTREE_AM_OID: u32 = 403;
/// OID of the `hash` index access method; see [`BTREE_AM_OID`].
const HASH_AM_OID: u32 = 405;

/// OID reported as the owner of every relation, type, and schema. PostgreSQL
/// assigns 10 to the bootstrap superuser; crabgresql has no role catalog yet, so
/// one owner stands for the whole cluster. `pg_get_userbyid` resolves it back to
/// the session user, so the two must agree — hence the shared constant.
pub(crate) const BOOTSTRAP_ROLE_OID: u32 = 10;

/// `pg_namespace.oid` of `public`, PostgreSQL's fixed value. Where a user type
/// lives, and so what its `typnamespace` reports — the schema an unqualified
/// name reaches only after `pg_catalog`.
pub(crate) const PUBLIC_NAMESPACE_OID: u32 = 2200;

/// OID of the one database a crabgresql server serves. PostgreSQL assigns a
/// fresh OID per `CREATE DATABASE`, so there is no upstream value to reuse: this
/// one is fixed here so `pg_database.oid` joins against itself consistently and
/// `current_database()::regclass`-style round-trips stay stable across restarts.
///
/// It sits in the same reserved band as [`PARQUET_AM_OID`], and for the same
/// reason: at 16384 it would have shared its OID with the first `CREATE SCHEMA`
/// or `CREATE TYPE` the server ever ran, since that is where the OID allocator
/// starts.
const DATABASE_OID: u32 = 16_002;

/// The `pg_proc` rows for crabgresql's own access-method handlers, which have no
/// upstream function to point at. `pg_am.amhandler` is a reference into
/// `pg_proc`, so leaving these at 0 would print `-` where PostgreSQL prints a
/// handler name for every method it ships. They sit in the same reserved band as
/// [`PARQUET_AM_OID`], and for the same reason.
const OWN_AM_HANDLERS: [(u32, &str); 2] = [
    (16_003, "parquet_tableam_handler"),
    (16_004, "buffer_tableam_handler"),
];

/// `pg_default` and `pg_global`, PostgreSQL's two bootstrap tablespaces.
/// crabgresql has no `CREATE TABLESPACE`, so these two rows are the whole
/// relation — which is also true of a stock PostgreSQL cluster nobody has added
/// one to.
const DEFAULT_TABLESPACE_OID: u32 = 1663;
const GLOBAL_TABLESPACE_OID: u32 = 1664;

/// `pg_catalog.pg_database` — one row, for the database this session is
/// connected to. PostgreSQL lists every database in the cluster; a crabgresql
/// server serves exactly one, so the connected database *is* the relation.
///
/// `datacl` (`aclitem[]`) is omitted: no `GRANT` exists to populate it, and
/// `aclitem` is not a type this build models. Same reasoning as `pg_type.typacl`
/// at the top of this file.
pub fn pg_database_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_database",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("datname", PgType::Name),
            col("datdba", PgType::Oid),
            col("encoding", PgType::Int4),
            col("datlocprovider", CHARLIKE),
            col("datistemplate", PgType::Bool),
            col("datallowconn", PgType::Bool),
            col("dathasloginevt", PgType::Bool),
            col("datconnlimit", PgType::Int4),
            col("datfrozenxid", PgType::Xid),
            col("datminmxid", PgType::Xid),
            col("dattablespace", PgType::Oid),
            col("datcollate", PgType::Text),
            col("datctype", PgType::Text),
            col("datlocale", PgType::Text),
            col("daticurules", PgType::Text),
            col("datcollversion", PgType::Text),
        ],
    )
}

/// The single `pg_database` row.
///
/// `encoding` is 6 (`UTF8`) because that is the only encoding the server
/// advertises (`server_encoding`), and the locale columns report `C`: the
/// default collation compares bytewise, and `datcollate`/`datctype` must name
/// the collation a `CREATE TABLE` with no `COLLATE` clause actually gets.
/// `datfrozenxid`/`datminmxid` report 1, PostgreSQL's `FirstNormalTransactionId`
/// — this build never advances a per-database freeze horizon.
pub fn pg_database_rows(database: &str) -> Vec<Vec<Value>> {
    vec![vec![
        Value::Oid(DATABASE_OID),
        Value::Text(database.to_string()),
        Value::Oid(BOOTSTRAP_ROLE_OID),
        Value::Int4(6),
        // 'c' — the libc locale provider, which is what a bytewise default is.
        chr('c'),
        Value::Bool(false),
        Value::Bool(true),
        Value::Bool(false),
        Value::Int4(-1),
        Value::Xid(1),
        Value::Xid(1),
        Value::Oid(DEFAULT_TABLESPACE_OID),
        Value::Text("C".to_string()),
        Value::Text("C".to_string()),
        Value::Null,
        Value::Null,
        Value::Null,
    ]]
}

/// `pg_catalog.pg_tablespace` — the two bootstrap tablespaces, as in
/// PostgreSQL. `spcacl` (`aclitem[]`) is omitted for the reason given on
/// [`pg_database_schema`].
pub fn pg_tablespace_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_tablespace",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("spcname", PgType::Name),
            col("spcowner", PgType::Oid),
            col("spcoptions", PgType::Array(crabgresql_types::oid::TEXT)),
        ],
    )
}

pub fn pg_tablespace_rows() -> Vec<Vec<Value>> {
    let row = |oid: u32, name: &str| {
        vec![
            Value::Oid(oid),
            Value::Text(name.to_string()),
            Value::Oid(BOOTSTRAP_ROLE_OID),
            Value::Null,
        ]
    };
    vec![
        row(DEFAULT_TABLESPACE_OID, "pg_default"),
        row(GLOBAL_TABLESPACE_OID, "pg_global"),
    ]
}

/// The one role a crabgresql server has: the session user, a superuser that can
/// log in.
///
/// All six role relations below are built from this, rather than each writing
/// its own literals, so they cannot drift apart — `pg_user` must always show
/// exactly the `pg_authid` rows that can log in, and `pg_group` exactly the ones
/// that cannot. Replacing this with an enumeration of a real role catalog is
/// then a change to one function.
struct BootstrapRole<'a> {
    oid: u32,
    name: &'a str,
    superuser: bool,
    inherit: bool,
    createrole: bool,
    createdb: bool,
    canlogin: bool,
    replication: bool,
    bypassrls: bool,
    connlimit: i32,
}

fn roles(owner: &str) -> Vec<BootstrapRole<'_>> {
    vec![BootstrapRole {
        oid: BOOTSTRAP_ROLE_OID,
        name: owner,
        superuser: true,
        inherit: true,
        createrole: true,
        createdb: true,
        canlogin: true,
        replication: true,
        bypassrls: true,
        // -1: no per-role connection limit, PostgreSQL's default.
        connlimit: -1,
    }]
}

/// `pg_catalog.pg_authid` — the role catalog. One row, the bootstrap superuser.
///
/// `rolpassword` is NULL here (and `********` in `pg_roles`/`pg_user` below),
/// exactly as in PostgreSQL: the shadow relations mask the hash rather than
/// omitting the column, and `pg_authid` itself is the superuser-only relation
/// that would hold it. crabgresql stores no password at all, so the mask is the
/// whole truth.
pub fn pg_authid_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_authid",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("rolname", PgType::Name),
            col("rolsuper", PgType::Bool),
            col("rolinherit", PgType::Bool),
            col("rolcreaterole", PgType::Bool),
            col("rolcreatedb", PgType::Bool),
            col("rolcanlogin", PgType::Bool),
            col("rolreplication", PgType::Bool),
            col("rolbypassrls", PgType::Bool),
            col("rolconnlimit", PgType::Int4),
            col("rolpassword", PgType::Text),
            col("rolvaliduntil", PgType::TimestampTz),
        ],
    )
}

pub fn pg_authid_rows(owner: &str) -> Vec<Vec<Value>> {
    roles(owner)
        .into_iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Text(r.name.to_string()),
                Value::Bool(r.superuser),
                Value::Bool(r.inherit),
                Value::Bool(r.createrole),
                Value::Bool(r.createdb),
                Value::Bool(r.canlogin),
                Value::Bool(r.replication),
                Value::Bool(r.bypassrls),
                Value::Int4(r.connlimit),
                Value::Null,
                Value::Null,
            ]
        })
        .collect()
}

/// `pg_catalog.pg_roles` — `pg_authid` with the password masked. Note the
/// column order is PostgreSQL's own and differs from `pg_authid`: `rolbypassrls`
/// comes after `rolvaliduntil`, and `oid` is last.
pub fn pg_roles_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_roles",
        "pg_catalog",
        vec![
            col("rolname", PgType::Name),
            col("rolsuper", PgType::Bool),
            col("rolinherit", PgType::Bool),
            col("rolcreaterole", PgType::Bool),
            col("rolcreatedb", PgType::Bool),
            col("rolcanlogin", PgType::Bool),
            col("rolreplication", PgType::Bool),
            col("rolconnlimit", PgType::Int4),
            col("rolpassword", PgType::Text),
            col("rolvaliduntil", PgType::TimestampTz),
            col("rolbypassrls", PgType::Bool),
            col("rolconfig", PgType::Array(crabgresql_types::oid::TEXT)),
            col("oid", PgType::Oid),
        ],
    )
}

pub fn pg_roles_rows(owner: &str) -> Vec<Vec<Value>> {
    roles(owner)
        .into_iter()
        .map(|r| {
            vec![
                Value::Text(r.name.to_string()),
                Value::Bool(r.superuser),
                Value::Bool(r.inherit),
                Value::Bool(r.createrole),
                Value::Bool(r.createdb),
                Value::Bool(r.canlogin),
                Value::Bool(r.replication),
                Value::Int4(r.connlimit),
                Value::Text(MASKED_PASSWORD.to_string()),
                Value::Null,
                Value::Bool(r.bypassrls),
                // `ALTER ROLE … SET` is unsupported, so no per-role GUCs exist.
                Value::Null,
                Value::Oid(r.oid),
            ]
        })
        .collect()
}

/// What `pg_roles`/`pg_user` print instead of a password hash. PostgreSQL emits
/// this literal rather than NULL, so a client cannot tell a role with no
/// password from one whose hash it may not read.
const MASKED_PASSWORD: &str = "********";

/// The shared column list of `pg_user` and `pg_shadow`: the login roles, under
/// the pre-8.1 `use*` names.
fn pg_user_columns(name: &str) -> TableSchema {
    TableSchema::in_namespace(
        name,
        "pg_catalog",
        vec![
            col("usename", PgType::Name),
            col("usesysid", PgType::Oid),
            col("usecreatedb", PgType::Bool),
            col("usesuper", PgType::Bool),
            col("userepl", PgType::Bool),
            col("usebypassrls", PgType::Bool),
            col("passwd", PgType::Text),
            col("valuntil", PgType::TimestampTz),
            col("useconfig", PgType::Array(crabgresql_types::oid::TEXT)),
        ],
    )
}

/// `pg_catalog.pg_user` — the roles that can log in, password masked.
pub fn pg_user_schema() -> TableSchema {
    pg_user_columns("pg_user")
}

/// `pg_catalog.pg_shadow` — `pg_user` with the password column unmasked. The
/// two differ only there, and here both are as truthful as each other: nothing
/// stores a password.
pub fn pg_shadow_schema() -> TableSchema {
    pg_user_columns("pg_shadow")
}

fn user_rows(owner: &str, passwd: Value) -> Vec<Vec<Value>> {
    roles(owner)
        .into_iter()
        .filter(|r| r.canlogin)
        .map(|r| {
            vec![
                Value::Text(r.name.to_string()),
                Value::Oid(r.oid),
                Value::Bool(r.createdb),
                Value::Bool(r.superuser),
                Value::Bool(r.replication),
                Value::Bool(r.bypassrls),
                passwd.clone(),
                Value::Null,
                Value::Null,
            ]
        })
        .collect()
}

pub fn pg_user_rows(owner: &str) -> Vec<Vec<Value>> {
    user_rows(owner, Value::Text(MASKED_PASSWORD.to_string()))
}

pub fn pg_shadow_rows(owner: &str) -> Vec<Vec<Value>> {
    user_rows(owner, Value::Null)
}

/// `pg_catalog.pg_group` — the roles that cannot log in, with their members.
///
/// Empty here, and empty as a *consequence*: the one role crabgresql has is a
/// login role. A stock PostgreSQL 18 shows 16 rows because `initdb` creates the
/// predefined `pg_read_all_data`/`pg_monitor`/… roles, which this build does not.
pub fn pg_group_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_group",
        "pg_catalog",
        vec![
            col("groname", PgType::Name),
            col("grosysid", PgType::Oid),
            col("grolist", PgType::Array(crabgresql_types::oid::OID)),
        ],
    )
}

pub fn pg_group_rows(owner: &str) -> Vec<Vec<Value>> {
    roles(owner)
        .into_iter()
        .filter(|r| !r.canlogin)
        .map(|r| {
            vec![
                Value::Text(r.name.to_string()),
                Value::Oid(r.oid),
                Value::Array {
                    elem: PgType::Oid,
                    elems: Vec::new(),
                },
            ]
        })
        .collect()
}

/// `pg_catalog.pg_auth_members` — role membership. Always empty: `GRANT <role>`
/// does not exist here, and with a single role there is nothing to be a member
/// of. (A stock PostgreSQL 18 has three rows, all between predefined roles.)
pub fn pg_auth_members_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_auth_members",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("roleid", PgType::Oid),
            col("member", PgType::Oid),
            col("grantor", PgType::Oid),
            col("admin_option", PgType::Bool),
            col("inherit_option", PgType::Bool),
            col("set_option", PgType::Bool),
        ],
    )
}

pub fn pg_auth_members_rows() -> Vec<Vec<Value>> {
    Vec::new()
}

/// `pg_catalog.pg_am` — the access methods. PostgreSQL lists the methods its
/// build actually registered; crabgresql adds its managed `parquet` and `buffer`
/// table methods alongside PostgreSQL's built-ins so a client that
/// joins `pg_class.relam` or reads `pg_am` sees the shape it expects.
///
/// Fidelity note (`AGENTS.md`): these rows are transcribed from the output of
/// `SELECT oid, amname, amhandler, amtype FROM pg_am ORDER BY oid` on a stock
/// PostgreSQL 18.4, not from upstream source. No `pg_am.dat` is vendored —
/// seven rows do not justify codegen.
pub fn pg_am_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_am",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("amname", PgType::Name),
            col("amhandler", REGPROC),
            col("amtype", CHARLIKE),
        ],
    )
}

/// The fixed `pg_am` rows. `amtype` is `'t'` for a table access method and
/// `'i'` for an index one.
pub fn pg_am_rows() -> Vec<Vec<Value>> {
    let row = |oid: u32, amname: &str, amhandler: &str, amtype: char| {
        vec![
            Value::Oid(oid),
            Value::Text(amname.to_string()),
            regproc_by_name(amhandler),
            chr(amtype),
        ]
    };
    vec![
        row(HEAP_AM_OID, "heap", "heap_tableam_handler", 't'),
        row(BTREE_AM_OID, "btree", "bthandler", 'i'),
        row(HASH_AM_OID, "hash", "hashhandler", 'i'),
        row(783, "gist", "gisthandler", 'i'),
        row(2742, "gin", "ginhandler", 'i'),
        row(3580, "brin", "brinhandler", 'i'),
        row(4000, "spgist", "spghandler", 'i'),
        row(PARQUET_AM_OID, "parquet", "parquet_tableam_handler", 't'),
        row(BUFFER_AM_OID, "buffer", "buffer_tableam_handler", 't'),
    ]
}

/// `pg_catalog.pg_timezone_names` — every IANA zone the bundled tz database
/// knows, with its offset and DST flag **at the given instant** (PostgreSQL
/// reports these as of `now()`, so a zone's row changes with the season).
pub fn pg_timezone_names_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_timezone_names",
        "pg_catalog",
        vec![
            col("name", PgType::Text),
            col("abbrev", PgType::Text),
            col("utc_offset", PgType::Interval),
            col("is_dst", PgType::Bool),
        ],
    )
}

pub fn pg_timezone_names_rows(at_micros: i64) -> Vec<Vec<Value>> {
    crabgresql_types::tz::timezone_names(at_micros)
        .into_iter()
        .map(|z| {
            vec![
                Value::Text(z.name),
                Value::Text(z.abbrev),
                offset_interval(z.utc_offset_secs),
                Value::Bool(z.is_dst),
            ]
        })
        .collect()
}

/// `pg_catalog.pg_timezone_abbrevs` — the abbreviations a datetime literal
/// accepts.
///
/// Divergence, deliberate: PostgreSQL 18.4 loads 198 abbreviations from the file
/// its `timezone_abbreviations` names, and this server has a curated 15 (see
/// `crabgresql_types::tz::timezone_abbrevs` for why growing that table is a
/// change to value parsing, not to a view). Consequences a reader should
/// expect: `count(*)` is 15, the offsets span 9 distinct values, and upstream's
/// `sysviews` check `count(distinct utc_offset) >= 24` reports false.
/// PostgreSQL 18's second half of this view — the abbreviations from the
/// *session zone's* own history, which is where its `LMT` rows come from — is
/// not implemented.
pub fn pg_timezone_abbrevs_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_timezone_abbrevs",
        "pg_catalog",
        vec![
            col("abbrev", PgType::Text),
            col("utc_offset", PgType::Interval),
            col("is_dst", PgType::Bool),
        ],
    )
}

pub fn pg_timezone_abbrevs_rows(at_micros: i64) -> Vec<Vec<Value>> {
    crabgresql_types::tz::timezone_abbrevs(at_micros)
        .into_iter()
        .map(|a| {
            vec![
                Value::Text(a.abbrev.to_string()),
                offset_interval(a.utc_offset_secs),
                Value::Bool(a.is_dst),
            ]
        })
        .collect()
}

/// A UTC offset as the `interval` both timezone views report it. Whole seconds
/// only: no tz database entry carries a sub-second offset.
fn offset_interval(secs: i32) -> Value {
    Value::Interval(crabgresql_types::interval::Interval {
        months: 0,
        days: 0,
        usec: i64::from(secs) * 1_000_000,
    })
}

/// `pg_catalog.pg_settings` — the configuration parameters.
///
/// A view over `pg_show_all_settings()` in PostgreSQL; served here as a
/// relation whose rows the session supplies, which is indistinguishable to a
/// client reading it. Rows come in `SHOW ALL`'s order (by name,
/// case-insensitively), and a parameter PostgreSQL flags `GUC_NO_SHOW_ALL` —
/// `is_superuser` — is absent from both, as it is upstream.
///
/// `sourcefile`/`sourceline` are always NULL and `pending_restart` always
/// false: this server reads no configuration file and has nothing that a
/// restart would change.
pub fn pg_settings_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_settings",
        "pg_catalog",
        vec![
            col("name", PgType::Text),
            col("setting", PgType::Text),
            col("unit", PgType::Text),
            col("category", PgType::Text),
            col("short_desc", PgType::Text),
            col("extra_desc", PgType::Text),
            col("context", PgType::Text),
            col("vartype", PgType::Text),
            col("source", PgType::Text),
            col("min_val", PgType::Text),
            col("max_val", PgType::Text),
            col("enumvals", PgType::Array(crabgresql_types::oid::TEXT)),
            col("boot_val", PgType::Text),
            col("reset_val", PgType::Text),
            col("sourcefile", PgType::Text),
            col("sourceline", PgType::Int4),
            col("pending_restart", PgType::Bool),
        ],
    )
}

pub fn pg_settings_rows(settings: &[CatalogSetting]) -> Vec<Vec<Value>> {
    let text = |s: Option<&str>| s.map_or(Value::Null, |s| Value::Text(s.to_string()));
    settings
        .iter()
        .map(|s| {
            vec![
                Value::Text(s.name.to_string()),
                Value::Text(s.setting.clone()),
                text(s.unit),
                Value::Text(s.category.to_string()),
                Value::Text(s.short_desc.to_string()),
                text(s.extra_desc),
                Value::Text(s.context.to_string()),
                Value::Text(s.vartype.to_string()),
                Value::Text(s.source.to_string()),
                text(s.min_val),
                text(s.max_val),
                s.enumvals.map_or(Value::Null, |vals| Value::Array {
                    elem: PgType::Text,
                    elems: vals.iter().map(|v| Value::Text(v.to_string())).collect(),
                }),
                Value::Text(s.boot_val.to_string()),
                Value::Text(s.reset_val.clone()),
                Value::Null,
                Value::Null,
                Value::Bool(false),
            ]
        })
        .collect()
}

/// `pg_catalog.pg_cursors` — the session's open `DECLARE … CURSOR` cursors.
///
/// A view over `pg_cursor()` in PostgreSQL; served here as a relation whose rows
/// the session supplies, which is indistinguishable to a client reading it.
///
/// `creation_time` is the `DECLARE`'s *statement* timestamp, as in PostgreSQL:
/// a cursor declared mid-block reports an instant strictly after that block's
/// `now()`, and two cursors declared in separate messages differ.
pub fn pg_cursors_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_cursors",
        "pg_catalog",
        vec![
            col("name", PgType::Text),
            col("statement", PgType::Text),
            col("is_holdable", PgType::Bool),
            col("is_binary", PgType::Bool),
            col("is_scrollable", PgType::Bool),
            col("creation_time", PgType::TimestampTz),
        ],
    )
}

/// One row per open cursor, in the order the session enumerated them.
pub fn pg_cursors_rows(cursors: &[CatalogCursor]) -> Vec<Vec<Value>> {
    cursors
        .iter()
        .map(|cursor| {
            vec![
                Value::Text(cursor.name.clone()),
                Value::Text(cursor.statement.clone()),
                Value::Bool(cursor.is_holdable),
                Value::Bool(cursor.is_binary),
                Value::Bool(cursor.is_scrollable),
                Value::TimestampTz(cursor.creation_time),
            ]
        })
        .collect()
}

/// `pg_catalog.pg_class` — a curated subset of columns for user relations, in
/// PostgreSQL's `attnum` order. Columns crabgresql has no state for are still
/// emitted with their true constant so a client's `\d` predicates evaluate as on
/// PG (e.g. `relchecks = 0` gates the CHECK-constraint listing *off*). Storage
/// bookkeeping columns beyond this set (`relfrozenxid`, `relminmxid`, …) are
/// omitted.
///
/// `relpages`/`reltuples` hold the **last `ANALYZE` snapshot**, not a live
/// measurement — matching PostgreSQL, where a relation that has never been
/// analyzed or vacuumed reports `relpages = 0` and `reltuples = -1` however
/// large it actually is (observed on PostgreSQL 18.4). The planner's own live
/// size estimate is a separate thing: see [`crate::RelStats`].
///
/// `relallvisible` sits between them in `attnum` order and is emitted as a
/// constant `0` — crabgresql keeps no visibility map, and `0` is what PostgreSQL
/// reports for a relation that has never been vacuumed.
pub fn pg_class_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_class",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("relname", PgType::Name),
            col("relnamespace", PgType::Oid),
            col("reltype", PgType::Oid),
            col("reloftype", PgType::Oid),
            col("relowner", PgType::Oid),
            col("relam", PgType::Oid),
            col("reltablespace", PgType::Oid),
            col("relpages", PgType::Int4),
            col("reltuples", PgType::Float4),
            col("relallvisible", PgType::Int4),
            col("reltoastrelid", PgType::Oid),
            col("relhasindex", PgType::Bool),
            col("relpersistence", CHARLIKE),
            col("relkind", CHARLIKE),
            col("relnatts", PgType::Int2),
            col("relchecks", PgType::Int2),
            col("relhasrules", PgType::Bool),
            col("relhastriggers", PgType::Bool),
            col("relrowsecurity", PgType::Bool),
            col("relforcerowsecurity", PgType::Bool),
            col("relreplident", CHARLIKE),
            col("relispartition", PgType::Bool),
            // pg_node_tree in PG; crabgresql stores the already-deparsed
            // `FOR VALUES …` text (see `pg_get_expr`, which just echoes it).
            col("relpartbound", PgType::Text),
        ],
    )
}

/// Deparse a leaf partition's `relpartbound` to the text PostgreSQL's
/// `pg_get_expr(relpartbound, oid)` prints — `FOR VALUES FROM (…) TO (…)`. Only
/// RANGE partitions exist, so only the range form is produced. `MINVALUE`/
/// `MAXVALUE` print as bare keywords. Storing the final text (not a node tree)
/// is a deliberate deviation: `pg_get_expr` then just echoes it.
///
/// Quoting follows what PostgreSQL 18.4 was observed to print: `true`/`false`
/// bare, a non-negative integer bare, and everything else single-quoted with
/// embedded quotes doubled — including negative numbers (`'-10'`), floats,
/// dates, and strings.
///
/// Fidelity note: PostgreSQL actually decides this from the *parse* of the
/// bound, printing a literal bare only when it needed no coercion to the key
/// type — so with an `int8` key even `5` prints as `'5'`, while with an `int4`
/// key it prints bare. crabgresql stores the bound already coerced to the key
/// type and does not record whether a coercion happened, so it cannot make that
/// distinction; the rule above matches PostgreSQL for the `int4`, boolean, and
/// text keys in practice and quotes (the safe, re-parseable form) otherwise.
fn deparse_partbound(part: &PartitionOf) -> String {
    let datum = |d: &PartitionBoundDatum| match d {
        PartitionBoundDatum::MinValue => "MINVALUE".to_string(),
        PartitionBoundDatum::MaxValue => "MAXVALUE".to_string(),
        PartitionBoundDatum::Value(v) => {
            // A boolean bound is an SQL keyword, not a string: PG prints
            // `false`, never the `'f'` of the wire encoding — which would not
            // even re-parse as a bool bound.
            if let Value::Bool(b) = v {
                return if *b { "true" } else { "false" }.to_string();
            }
            let text = v.encode_text_utc().unwrap_or_default();
            let bare = match v {
                Value::Int2(_) | Value::Int4(_) | Value::Int8(_) => !text.starts_with('-'),
                _ => false,
            };
            if bare {
                text
            } else {
                format!("'{}'", text.replace('\'', "''"))
            }
        }
    };
    let list =
        |datums: &[PartitionBoundDatum]| datums.iter().map(datum).collect::<Vec<_>>().join(", ");
    format!(
        "FOR VALUES FROM ({}) TO ({})",
        list(&part.bound.from),
        list(&part.bound.to)
    )
}

/// The `(relpages, reltuples)` pair `pg_class` reports for a relation.
///
/// PostgreSQL only writes these during `VACUUM`/`ANALYZE`, so a relation that
/// has never been analyzed reports `(0, -1)` no matter how large it is — `-1` is
/// the sentinel meaning "unknown", distinct from a genuine zero-row relation
/// (verified against PostgreSQL 18.4). Reporting the planner's live estimate
/// here instead would look more informative and be less correct: a client that
/// checks `reltuples = -1` to decide whether a table needs analyzing would never
/// see one that did.
fn analyzed_size(stats: &RelStats) -> (Value, Value) {
    if !stats.analyzed {
        return (Value::Int4(0), Value::Float4(-1.0));
    }
    (
        Value::Int4(stats.relpages.min(i32::MAX as u32) as i32),
        Value::Float4(stats.reltuples as f32),
    )
}

/// Build `pg_class` rows from `(oid, schema)` pairs paired with their kinds.
/// `relpersistence` comes from each schema (`'p'` permanent, `'u'` unlogged,
/// `'t'` temporary — the memory tables); a table is an ordinary heap (`relkind = 'r'`,
/// `relam = 2`) while a view has no storage access method (`relkind = 'v'`,
/// `relam = 0`). The synthetic OIDs are stable within one catalog snapshot so a
/// join to `pg_attribute.attrelid` lines up.
///
/// Columns crabgresql does not track are their PostgreSQL constants: rules only
/// on views (`relhasrules`), no triggers or row security, no `OF type` /
/// tablespace / TOAST relation. `relchecks` counts the relation's CHECK
/// constraints, which is what makes psql print a `Check constraints:` footer. A
/// heap-backed relation defaults its replica identity to the primary key
/// (`relreplident = 'd'`); views, sequences, and indexes have none (`'n'`).
///
/// `stats` is parallel to `relations`; see [`analyzed_size`] for how it renders.
pub fn pg_class_rows(
    relations: &[(u32, TableSchema)],
    kinds: &[RelKind],
    stats: &[RelStats],
    indexes: &[CatalogIndex],
    toasts: &[CatalogToast],
    namespace_oids: &HashMap<String, u32>,
) -> Vec<Vec<Value>> {
    // Resolve a relation's namespace OID, defaulting to `public` (2200) for any
    // namespace not in the map (should not happen for a live relation).
    let nsp_oid = |namespace: &str| namespace_oids.get(namespace).copied().unwrap_or(2200);
    let mut rows: Vec<Vec<Value>> = relations
        .iter()
        .zip(kinds)
        .zip(stats)
        .map(|(((oid, schema), kind), stats)| {
            // A partitioned parent has no access method (`relam = 0`) and holds no
            // storage of its own.
            let (relam, relkind) = match kind {
                RelKind::Table => (
                    match schema.access_method {
                        TableAccessMethod::Heap => HEAP_AM_OID,
                        TableAccessMethod::Parquet => PARQUET_AM_OID,
                        TableAccessMethod::Buffer => BUFFER_AM_OID,
                    },
                    'r',
                ),
                RelKind::PartitionedTable => (0, 'p'),
                RelKind::View => (0, 'v'),
                RelKind::Sequence => (0, 'S'),
            };
            // Heap-backed relations (ordinary + partitioned tables) default their
            // replica identity to the primary key; the rest carry none.
            let relreplident = match kind {
                RelKind::Table | RelKind::PartitionedTable => 'd',
                RelKind::View | RelKind::Sequence => 'n',
            };
            let relpartbound = match &schema.partition_of {
                Some(part) => Value::Text(deparse_partbound(part)),
                None => Value::Null,
            };
            // A sequence is one page holding its single row, and PostgreSQL
            // reports it that way from creation — there is nothing to analyze.
            let (relpages, reltuples) = match kind {
                RelKind::Sequence => (Value::Int4(1), Value::Float4(1.0)),
                _ => analyzed_size(stats),
            };
            vec![
                Value::Oid(*oid),
                Value::Text(schema.name.clone()),
                Value::Oid(nsp_oid(&schema.namespace)),
                Value::Oid(0),
                // reloftype: crabgresql has no typed tables.
                Value::Oid(0),
                Value::Oid(BOOTSTRAP_ROLE_OID),
                Value::Oid(relam),
                // reltablespace: default tablespace.
                Value::Oid(0),
                relpages,
                reltuples,
                // relallvisible: no visibility map is kept.
                Value::Int4(0),
                // reltoastrelid: the relation's TOAST relation, or 0 when it has
                // none. Zero is legitimate PostgreSQL state — it is what PG
                // reports for a table with no out-of-line storage — and it is
                // what a table of narrow columns keeps, since the TOAST relation
                // is created only once a row first needs one.
                Value::Oid(
                    toasts
                        .iter()
                        .find(|t| t.table_oid == *oid)
                        .map_or(0, |t| t.oid),
                ),
                Value::Bool(indexes.iter().any(|index| index.table_oid == *oid)),
                chr(schema.persistence.as_char()),
                chr(relkind),
                Value::Int2(schema.columns.len() as i16),
                // relchecks: the CHECK constraints on this relation, inherited
                // ones included — PostgreSQL counts a child's copies too.
                Value::Int2(schema.checks.len() as i16),
                // relhasrules: only a view carries the `_RETURN` rule.
                Value::Bool(matches!(kind, RelKind::View)),
                // relhastriggers / relrowsecurity / relforcerowsecurity.
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
                chr(relreplident),
                Value::Bool(schema.partition_of.is_some()),
                relpartbound,
            ]
        })
        .collect();
    rows.extend(indexes.iter().map(|index| {
        vec![
            Value::Oid(index.oid),
            Value::Text(index.metadata.name.clone()),
            // An index lives in its table's namespace.
            Value::Oid(nsp_oid(&index.table_schema.namespace)),
            Value::Oid(0),
            Value::Oid(0),
            Value::Oid(BOOTSTRAP_ROLE_OID),
            Value::Oid(match index.metadata.method {
                IndexMethod::BTree => BTREE_AM_OID,
                IndexMethod::Hash => HASH_AM_OID,
            }),
            Value::Oid(0),
            // relpages / reltuples: per-index size is not tracked, so an index
            // reports the never-analyzed sentinel. relallvisible: no map.
            Value::Int4(0),
            Value::Float4(-1.0),
            Value::Int4(0),
            Value::Oid(0),
            Value::Bool(false),
            chr('p'),
            chr('i'),
            Value::Int2(index.metadata.keys.len() as i16),
            Value::Int2(0),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            // An index has no replica identity of its own.
            chr('n'),
            Value::Bool(false),
            Value::Null,
        ]
    }));
    // TOAST relations, as `relkind = 't'` in the `pg_toast` namespace. Publishing
    // the row is what makes a non-zero `reltoastrelid` safe: it is a foreign key
    // into `pg_class.oid`, so an OID with no row here would be a dangling
    // reference of exactly the kind upstream's `oidjoins` test exists to catch.
    rows.extend(toasts.iter().map(|toast| {
        vec![
            Value::Oid(toast.oid),
            Value::Text(toast.name.clone()),
            Value::Oid(namespace_oids.get(TOAST_NAMESPACE).copied().unwrap_or(99)),
            Value::Oid(0),
            Value::Oid(0),
            Value::Oid(BOOTSTRAP_ROLE_OID),
            Value::Oid(HEAP_AM_OID),
            Value::Oid(0),
            Value::Int4(toast.stats.relpages as i32),
            // reltuples: chunks are not rows, so a count here would invite being
            // read as one. The never-analyzed sentinel is the honest answer.
            Value::Float4(-1.0),
            Value::Int4(0),
            // A TOAST relation has no TOAST relation of its own.
            Value::Oid(0),
            // relhasindex: PostgreSQL indexes its TOAST relation on
            // `(chunk_id, chunk_seq)`; ours chains chunks by ctid instead, so
            // there is no `pg_toast_<oid>_index`, and claiming one would be the
            // dangling reference this block exists to avoid.
            Value::Bool(false),
            chr(toast.persistence.as_char()),
            chr('t'),
            Value::Int2(TOAST_COLUMNS.len() as i16),
            Value::Int2(0),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            chr('n'),
            Value::Bool(false),
            Value::Null,
        ]
    }));
    rows
}

/// The columns PostgreSQL gives every TOAST relation, published so a `pg_class`
/// row with `relnatts = 3` has matching `pg_attribute` rows to join against.
///
/// This presents PostgreSQL's TOAST schema, not our storage: our chunks carry no
/// `chunk_id`/`chunk_seq` of their own, because the pointer names the first chunk
/// directly and each chunk links to the next. `pg_attribute` is already a
/// presentation layer in exactly this way — it describes every relation in
/// PostgreSQL's terms while the heap stores self-describing datums that look
/// nothing like `attlen`-driven layout.
const TOAST_COLUMNS: [(&str, PgType); 3] = [
    ("chunk_id", PgType::Oid),
    ("chunk_seq", PgType::Int4),
    ("chunk_data", PgType::Bytea),
];

/// `pg_catalog.pg_inherits` — the parent/child links of declarative partitions
/// and of table inheritance. One row per leaf partition, and one per
/// `INHERITS (...)` parent of an inheritance child.
pub fn pg_inherits_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_inherits",
        "pg_catalog",
        vec![
            col("inhrelid", PgType::Oid),
            col("inhparent", PgType::Oid),
            col("inhseqno", PgType::Int4),
            col("inhdetachpending", PgType::Bool),
        ],
    )
}

/// One `pg_inherits` row per parent link: a leaf partition has exactly one, an
/// inheritance child has one per `INHERITS (...)` entry. Both OIDs come from the
/// same positional assignment as `pg_class`, so the `inhrelid`/`inhparent` →
/// `pg_class.oid` joins line up.
///
/// `inhseqno` numbers a child's parents from 1 in declaration order. A partition
/// always gets 1, and only one of the two branches ever fires for a relation:
/// DDL refuses `INHERITS` together with `PARTITION OF` rather than letting one
/// clause quietly win.
pub fn pg_inherits_rows(relations: &[(u32, TableSchema)]) -> Vec<Vec<Value>> {
    let parent_oid = |namespace: &str, name: &str| -> Option<u32> {
        relations
            .iter()
            .find(|(_, s)| s.namespace == namespace && s.name == name)
            .map(|(oid, _)| *oid)
    };
    let mut rows = Vec::new();
    for (oid, schema) in relations {
        if let Some(part) = &schema.partition_of
            && let Some(parent) = parent_oid(&part.parent_namespace, &part.parent_name)
        {
            rows.push(vec![
                Value::Oid(*oid),
                Value::Oid(parent),
                Value::Int4(1),
                Value::Bool(false),
            ]);
        }
        for (i, inherit) in schema.inherits.iter().enumerate() {
            let Some(parent) = parent_oid(&inherit.namespace, &inherit.name) else {
                continue;
            };
            rows.push(vec![
                Value::Oid(*oid),
                Value::Oid(parent),
                Value::Int4(i as i32 + 1),
                Value::Bool(false),
            ]);
        }
    }
    rows
}

/// `pg_catalog.pg_partitioned_table` — one row per partitioned (parent) table,
/// describing its partition key. A curated subset: `partdefid` (the default
/// partition) is always 0 and the class/collation/expression vectors are omitted.
pub fn pg_partitioned_table_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_partitioned_table",
        "pg_catalog",
        vec![
            col("partrelid", PgType::Oid),
            col("partstrat", CHARLIKE),
            col("partnatts", PgType::Int2),
            col("partdefid", PgType::Oid),
            // The 1-based key attnums.
            col("partattrs", INT2VECTOR),
        ],
    )
}

/// One `pg_partitioned_table` row per partitioned parent.
pub fn pg_partitioned_table_rows(relations: &[(u32, TableSchema)]) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for (oid, schema) in relations {
        if let Some(scheme) = &schema.partition_scheme {
            let strat = match scheme.strategy {
                PartitionStrategy::Range => 'r',
            };
            let attrs = attnum_vector(scheme.key_columns.iter().copied());
            rows.push(vec![
                Value::Oid(*oid),
                chr(strat),
                Value::Int2(scheme.key_columns.len() as i16),
                Value::Oid(0),
                attrs,
            ]);
        }
    }
    rows
}

/// `pg_catalog.pg_attribute` — a curated subset of columns for user relations'
/// columns. System (negative `attnum`) columns are not emitted yet.
pub fn pg_attribute_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_attribute",
        "pg_catalog",
        vec![
            col("attrelid", PgType::Oid),
            col("attname", PgType::Name),
            col("atttypid", PgType::Oid),
            col("attlen", PgType::Int2),
            col("attnum", PgType::Int2),
            col("atttypmod", PgType::Int4),
            col("attbyval", PgType::Bool),
            col("attalign", CHARLIKE),
            col("attstorage", CHARLIKE),
            col("attnotnull", PgType::Bool),
            col("atthasdef", PgType::Bool),
            col("attidentity", CHARLIKE),
            col("attgenerated", CHARLIKE),
            col("attisdropped", PgType::Bool),
            col("attcollation", PgType::Oid),
        ],
    )
}

/// A column's physical layout — `attbyval`, `attalign`, `attstorage` — taken
/// from the *type's* `pg_type` row rather than restated here, which is what
/// upstream's `type_sanity` checks the two agree on. A type with no built-in
/// row (a `CREATE TYPE` enum) reports the fixed 4-byte pass-by-value layout
/// [`pg_type_user_rows`] gives it.
fn attlayout_of(ty: PgType) -> (Value, Value, Value) {
    match PG_TYPE_ROWS.iter().find(|row| row.oid == ty.oid()) {
        Some(row) => (
            Value::Bool(row.typbyval),
            str_char(row.typalign),
            str_char(row.typstorage),
        ),
        None => (Value::Bool(true), chr('i'), chr('p')),
    }
}

/// `attcollation`: the column's explicit `COLLATE`, else the type's own
/// collation — and `0` when the type has none, as PostgreSQL records it.
fn attcollation_of(column: &Column) -> u32 {
    match column.collation {
        Some(oid) => oid,
        None => typcollation_of(column.ty.oid()),
    }
}

/// Build `pg_attribute` rows: one per column of each relation, `attnum` 1-based
/// (user columns only), typed from the column's `PgType` (`atttypid`/`attlen`).
pub fn pg_attribute_rows(
    relations: &[(u32, TableSchema)],
    indexes: &[CatalogIndex],
    toasts: &[CatalogToast],
) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for (oid, schema) in relations {
        for (i, c) in schema.columns.iter().enumerate() {
            let (byval, align, storage) = attlayout_of(c.ty);
            rows.push(vec![
                Value::Oid(*oid),
                Value::Text(c.name.clone()),
                Value::Oid(c.ty.oid()),
                Value::Int2(c.ty.typlen()),
                Value::Int2((i + 1) as i16),
                Value::Int4(c.atttypmod()),
                byval,
                align,
                storage,
                Value::Bool(!c.nullable),
                Value::Bool(c.default.is_some()),
                // attidentity / attgenerated: no identity or generated columns.
                // PostgreSQL spells "not one" as `\0`, which prints empty.
                chr('\0'),
                chr('\0'),
                Value::Bool(false),
                Value::Oid(attcollation_of(c)),
            ]);
        }
    }
    for index in indexes {
        for (position, key) in index.metadata.keys.iter().enumerate() {
            let column = &index.table_schema.columns[key.column];
            let (byval, align, storage) = attlayout_of(column.ty);
            rows.push(vec![
                Value::Oid(index.oid),
                Value::Text(column.name.clone()),
                Value::Oid(column.ty.oid()),
                Value::Int2(column.ty.typlen()),
                Value::Int2((position + 1) as i16),
                Value::Int4(column.atttypmod()),
                byval,
                align,
                storage,
                Value::Bool(false),
                Value::Bool(false),
                chr('\0'),
                chr('\0'),
                Value::Bool(false),
                Value::Oid(attcollation_of(column)),
            ]);
        }
    }
    // A TOAST relation's columns, so its `pg_class.relnatts` has rows to join.
    for toast in toasts {
        for (i, (name, ty)) in TOAST_COLUMNS.iter().enumerate() {
            let (byval, align, storage) = attlayout_of(*ty);
            rows.push(vec![
                Value::Oid(toast.oid),
                Value::Text((*name).to_string()),
                Value::Oid(ty.oid()),
                Value::Int2(ty.typlen()),
                Value::Int2((i + 1) as i16),
                Value::Int4(-1),
                byval,
                align,
                storage,
                // PostgreSQL marks all three NOT NULL.
                Value::Bool(true),
                Value::Bool(false),
                chr('\0'),
                chr('\0'),
                Value::Bool(false),
                Value::Oid(0),
            ]);
        }
    }
    rows
}

pub fn pg_attrdef_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_attrdef",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("adrelid", PgType::Oid),
            col("adnum", PgType::Int2),
            col("adbin", PgType::Text),
        ],
    )
}

pub fn pg_attrdef_rows(relations: &[(u32, TableSchema)]) -> Vec<Vec<Value>> {
    let mut next_oid = 30000_u32;
    let mut rows = Vec::new();
    for (table_oid, schema) in relations {
        for (position, column) in schema.columns.iter().enumerate() {
            if let Some(default) = &column.default {
                rows.push(vec![
                    Value::Oid(next_oid),
                    Value::Oid(*table_oid),
                    Value::Int2((position + 1) as i16),
                    Value::Text(default.clone()),
                ]);
                next_oid += 1;
            }
        }
    }
    rows
}

pub fn pg_constraint_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_constraint",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("conname", PgType::Name),
            col("connamespace", PgType::Oid),
            col("contype", CHARLIKE),
            col("condeferrable", PgType::Bool),
            col("condeferred", PgType::Bool),
            // Present in PostgreSQL 18; `NOT ENFORCED` is refused at DDL, so
            // everything here is enforced.
            col("conenforced", PgType::Bool),
            col("convalidated", PgType::Bool),
            col("conrelid", PgType::Oid),
            col("contypid", PgType::Oid),
            col("conindid", PgType::Oid),
            col("conparentid", PgType::Oid),
            col("confrelid", PgType::Oid),
            col("conislocal", PgType::Bool),
            col("coninhcount", PgType::Int2),
            col("connoinherit", PgType::Bool),
            // int2[] is represented as PG array text until catalog arrays land.
            col("conkey", PgType::Text),
            // pg_node_tree in PostgreSQL, modelled as the stored SQL text the
            // same way `pg_class.relpartbound` is. `pg_get_expr` re-renders it
            // for the reader.
            col("conbin", PgType::Text),
        ],
    )
}

/// Render the constraints [`crate::SystemCatalog::constraint_oids`] already
/// numbered. Pure: it assigns nothing, so the OID a row reports is the same one
/// `pg_get_constraintdef` resolves against.
pub fn pg_constraint_rows(
    constraints: &[CatalogConstraint],
    namespace_oids: &HashMap<String, u32>,
) -> Vec<Vec<Value>> {
    let nsp_oid = |namespace: &str| namespace_oids.get(namespace).copied().unwrap_or(2200);
    constraints
        .iter()
        .map(|c| {
            vec![
                Value::Oid(c.oid),
                Value::Text(c.name.clone()),
                Value::Oid(nsp_oid(&c.namespace)),
                str_char(c.contype),
                // condeferrable / condeferred: DEFERRABLE is refused at DDL.
                Value::Bool(false),
                Value::Bool(false),
                // conenforced.
                Value::Bool(true),
                Value::Bool(c.validated),
                Value::Oid(c.table_oid),
                // contypid: domain constraints are not modelled.
                Value::Oid(0),
                Value::Oid(c.index_oid),
                // conparentid: a partition's copied constraint would point at
                // its parent's; partitioned tables carry no checks here.
                Value::Oid(0),
                // confrelid: foreign keys are not supported.
                Value::Oid(0),
                Value::Bool(c.islocal),
                Value::Int2(c.inhcount),
                // connoinherit: `NO INHERIT` has no parser support, so nothing
                // that exists here can be marked with it.
                Value::Bool(false),
                // NULL, not an empty array, when the constraint reads no column
                // — PostgreSQL stores NULL for a predicate like `CHECK (1 > 0)`,
                // so a client testing `conkey IS NULL` agrees. Probed against
                // 18.4.
                match c.columns.is_empty() {
                    true => Value::Null,
                    false => Value::Text(format!(
                        "{{{}}}",
                        c.columns
                            .iter()
                            .map(|column| (*column + 1).to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    )),
                },
                match &c.expr {
                    Some(expr) => Value::Text(expr.clone()),
                    None => Value::Null,
                },
            ]
        })
        .collect()
}

pub fn pg_index_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_index",
        "pg_catalog",
        vec![
            col("indexrelid", PgType::Oid),
            col("indrelid", PgType::Oid),
            col("indnatts", PgType::Int2),
            col("indnkeyatts", PgType::Int2),
            col("indisunique", PgType::Bool),
            col("indnullsnotdistinct", PgType::Bool),
            col("indisprimary", PgType::Bool),
            col("indimmediate", PgType::Bool),
            col("indisvalid", PgType::Bool),
            col("indkey", INT2VECTOR),
            col("indoption", INT2VECTOR),
        ],
    )
}

pub fn pg_index_rows(indexes: &[CatalogIndex]) -> Vec<Vec<Value>> {
    indexes
        .iter()
        .map(|index| {
            // 1-based key attnums, as PG's `indkey` holds.
            let indkey = attnum_vector(index.metadata.keys.iter().map(|key| key.column));
            let indoption = int2vector(index.metadata.keys.iter().map(|key| {
                let mut option = 0;
                if key.descending {
                    option |= 1;
                }
                if key.nulls_first {
                    option |= 2;
                }
                option
            }));
            vec![
                Value::Oid(index.oid),
                Value::Oid(index.table_oid),
                Value::Int2(index.metadata.keys.len() as i16),
                Value::Int2(index.metadata.keys.len() as i16),
                Value::Bool(index.metadata.unique),
                Value::Bool(!index.metadata.nulls_distinct),
                Value::Bool(index.metadata.constraint == Some(IndexConstraint::PrimaryKey)),
                Value::Bool(true),
                Value::Bool(true),
                indkey,
                indoption,
            ]
        })
        .collect()
}

/// Fixed `pg_namespace` rows: the reserved catalog/toast schemas plus `public`.
/// OIDs match PostgreSQL's stable assignments (`pg_catalog` = 11, `pg_toast` =
/// 99, `public` = 2200). `information_schema` has an initdb-assigned OID, so
/// it remains absent here; its named discovery surface lives in
/// `information_schema.schemata`. Owners are reported as the bootstrap
/// superuser (10) for now.
pub fn pg_namespace_rows(user_schemas: &[(String, u32)]) -> Vec<Vec<Value>> {
    let row = |oid: u32, name: &str| {
        vec![
            Value::Oid(oid),
            Value::Text(name.to_string()),
            Value::Oid(BOOTSTRAP_ROLE_OID),
            Value::Null,
        ]
    };
    let mut rows = vec![
        row(11, "pg_catalog"),
        row(99, "pg_toast"),
        row(2200, "public"),
    ];
    for (name, oid) in user_schemas {
        rows.push(row(*oid, name));
    }
    rows
}

/// `information_schema.schemata`. Information-schema domains are represented
/// as text until the engine supports domains over the built-in types.
pub fn information_schema_schemata_schema() -> TableSchema {
    TableSchema::in_namespace(
        "schemata",
        "information_schema",
        vec![
            col("catalog_name", PgType::Text),
            col("schema_name", PgType::Text),
            col("schema_owner", PgType::Text),
            col("default_character_set_catalog", PgType::Text),
            col("default_character_set_schema", PgType::Text),
            col("default_character_set_name", PgType::Text),
            col("sql_path", PgType::Text),
        ],
    )
}

pub fn information_schema_schemata_rows(
    database: &str,
    owner: &str,
    relations: &[CatalogRelation],
    user_schemas: &[(String, u32)],
) -> Vec<Vec<Value>> {
    let mut namespaces = vec![
        "information_schema".to_string(),
        "pg_catalog".to_string(),
        "pg_toast".to_string(),
        "public".to_string(),
    ];
    for relation in relations {
        if !namespaces.contains(&relation.namespace) {
            namespaces.push(relation.namespace.clone());
        }
    }
    // Include freshly-created, still-empty user schemas (no relations yet).
    for (name, _) in user_schemas {
        if !namespaces.contains(name) {
            namespaces.push(name.clone());
        }
    }
    namespaces.sort();
    namespaces
        .into_iter()
        .map(|namespace| {
            vec![
                Value::Text(database.to_string()),
                Value::Text(namespace),
                Value::Text(owner.to_string()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]
        })
        .collect()
}

/// `information_schema.tables` for represented user relations. Catalog and
/// information-schema implementation relations are deliberately not invented:
/// their complete PostgreSQL metadata is not modeled yet.
pub fn information_schema_tables_schema() -> TableSchema {
    TableSchema::in_namespace(
        "tables",
        "information_schema",
        vec![
            col("table_catalog", PgType::Text),
            col("table_schema", PgType::Text),
            col("table_name", PgType::Text),
            col("table_type", PgType::Text),
            col("self_referencing_column_name", PgType::Text),
            col("reference_generation", PgType::Text),
            col("user_defined_type_catalog", PgType::Text),
            col("user_defined_type_schema", PgType::Text),
            col("user_defined_type_name", PgType::Text),
            col("is_insertable_into", PgType::Text),
            col("is_typed", PgType::Text),
            col("commit_action", PgType::Text),
        ],
    )
}

pub fn information_schema_tables_rows(
    database: &str,
    relations: &[CatalogRelation],
) -> Vec<Vec<Value>> {
    relations
        .iter()
        // Sequences are not tables: PG omits them from information_schema.tables.
        .filter(|relation| relation.kind != RelKind::Sequence)
        .map(|relation| {
            vec![
                Value::Text(database.to_string()),
                Value::Text(relation.namespace.clone()),
                Value::Text(relation.schema.name.clone()),
                Value::Text(
                    match (relation.kind, relation.temporary) {
                        (RelKind::View, _) => "VIEW",
                        (RelKind::Table, true) => "LOCAL TEMPORARY",
                        (RelKind::Table, false) => "BASE TABLE",
                        // A partitioned parent reflects as BASE TABLE, as in PG.
                        (RelKind::PartitionedTable, _) => "BASE TABLE",
                        (RelKind::Sequence, _) => unreachable!("filtered out above"),
                    }
                    .to_string(),
                ),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Text("YES".to_string()),
                Value::Text("NO".to_string()),
                Value::Null,
            ]
        })
        .collect()
}

/// `information_schema.columns`, including all PostgreSQL-documented columns.
pub fn information_schema_columns_schema() -> TableSchema {
    let text = PgType::Text;
    let cardinal = PgType::Int4;
    TableSchema::in_namespace(
        "columns",
        "information_schema",
        vec![
            col("table_catalog", text),
            col("table_schema", text),
            col("table_name", text),
            col("column_name", text),
            col("ordinal_position", cardinal),
            col("column_default", text),
            col("is_nullable", text),
            col("data_type", text),
            col("character_maximum_length", cardinal),
            col("character_octet_length", cardinal),
            col("numeric_precision", cardinal),
            col("numeric_precision_radix", cardinal),
            col("numeric_scale", cardinal),
            col("datetime_precision", cardinal),
            col("interval_type", text),
            col("interval_precision", cardinal),
            col("character_set_catalog", text),
            col("character_set_schema", text),
            col("character_set_name", text),
            col("collation_catalog", text),
            col("collation_schema", text),
            col("collation_name", text),
            col("domain_catalog", text),
            col("domain_schema", text),
            col("domain_name", text),
            col("udt_catalog", text),
            col("udt_schema", text),
            col("udt_name", text),
            col("scope_catalog", text),
            col("scope_schema", text),
            col("scope_name", text),
            col("maximum_cardinality", cardinal),
            col("dtd_identifier", text),
            col("is_self_referencing", text),
            col("is_identity", text),
            col("identity_generation", text),
            col("identity_start", text),
            col("identity_increment", text),
            col("identity_maximum", text),
            col("identity_minimum", text),
            col("identity_cycle", text),
            col("is_generated", text),
            col("generation_expression", text),
            col("is_updatable", text),
        ],
    )
}

pub fn information_schema_columns_rows(
    database: &str,
    relations: &[CatalogRelation],
) -> Vec<Vec<Value>> {
    relations
        .iter()
        // Sequences are not tables; omit their columns from information_schema.
        .filter(|relation| relation.kind != RelKind::Sequence)
        .flat_map(|relation| {
            relation
                .schema
                .columns
                .iter()
                .enumerate()
                .map(move |(index, column)| {
                    let (character_length, character_octets) = match column.ty {
                        PgType::Varchar | PgType::Bpchar if column.typmod >= 0 => {
                            (Value::Int4(column.typmod), Value::Int4(column.typmod * 4))
                        }
                        PgType::Bit | PgType::Varbit if column.typmod >= 0 => {
                            (Value::Int4(column.typmod), Value::Null)
                        }
                        _ => (Value::Null, Value::Null),
                    };
                    let (precision, radix) = match column.ty {
                        PgType::Int2 => (Value::Int4(16), Value::Int4(2)),
                        PgType::Int4 => (Value::Int4(32), Value::Int4(2)),
                        PgType::Int8 => (Value::Int4(64), Value::Int4(2)),
                        PgType::Float4 => (Value::Int4(24), Value::Int4(2)),
                        PgType::Float8 => (Value::Int4(53), Value::Int4(2)),
                        _ => (Value::Null, Value::Null),
                    };
                    // `datetime_precision` is the declared fractional-second
                    // precision, defaulting to the 6 every datetime type keeps.
                    // `interval_type` names the fields the modifier admits,
                    // uppercased and with the precision appended
                    // (`DAY TO SECOND(4)`); a full-range `interval(3)` reports
                    // NULL there and carries its precision only in
                    // `datetime_precision`.
                    let (datetime_precision, interval_type) = match column.ty {
                        PgType::Time | PgType::TimeTz | PgType::Timestamp | PgType::TimestampTz => {
                            let p = if column.typmod >= 0 { column.typmod } else { 6 };
                            (Value::Int4(p), Value::Null)
                        }
                        PgType::Interval => {
                            let (range, precision) =
                                crabgresql_types::interval::unpack_typmod(column.typmod);
                            let spelling =
                                crabgresql_types::interval::range_name(range).map(|fields| {
                                    let mut s = fields.to_ascii_uppercase();
                                    if let Some(p) = precision {
                                        s.push_str(&format!("({p})"));
                                    }
                                    Value::Text(s)
                                });
                            (
                                Value::Int4(precision.map_or(6, i32::from)),
                                spelling.unwrap_or(Value::Null),
                            )
                        }
                        _ => (Value::Null, Value::Null),
                    };
                    // PG's view joins pg_collation but excludes
                    // `pg_catalog.default`, so a column left on the database
                    // collation reports NULL here rather than "default".
                    let collation =
                        crabgresql_types::collation::lookup_by_oid(attcollation_of(column))
                            .filter(|c| c.name != "default");
                    let (collation_catalog, collation_schema, collation_name) = match collation {
                        Some(c) => (
                            Value::Text(database.to_string()),
                            Value::Text("pg_catalog".to_string()),
                            Value::Text(c.name.to_string()),
                        ),
                        None => (Value::Null, Value::Null, Value::Null),
                    };
                    vec![
                        Value::Text(database.to_string()),
                        Value::Text(relation.namespace.clone()),
                        Value::Text(relation.schema.name.clone()),
                        Value::Text(column.name.clone()),
                        Value::Int4((index + 1) as i32),
                        column
                            .default
                            .as_ref()
                            .map(|default| Value::Text(default.clone()))
                            .unwrap_or(Value::Null),
                        Value::Text(if column.nullable { "YES" } else { "NO" }.to_string()),
                        Value::Text(column.ty.name().to_string()),
                        character_length,
                        character_octets,
                        precision,
                        radix,
                        // numeric_scale
                        Value::Null,
                        datetime_precision,
                        interval_type,
                        // `interval_precision` is always NULL in PostgreSQL too:
                        // an interval's precision is reported through
                        // `datetime_precision` instead.
                        Value::Null,
                        // character_set_{catalog,schema,name}
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        collation_catalog,
                        collation_schema,
                        collation_name,
                        // domain_{catalog,schema,name}
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Text(database.to_string()),
                        Value::Text("pg_catalog".to_string()),
                        Value::Text(column.ty.typname().to_string()),
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Text((index + 1).to_string()),
                        Value::Text("NO".to_string()),
                        Value::Text("NO".to_string()),
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Text("NEVER".to_string()),
                        Value::Null,
                        Value::Text("YES".to_string()),
                    ]
                })
        })
        .collect()
}

/// The `oidvector` and `int2vector` catalog column types. See
/// [`crabgresql_types::vector`].
const OIDVECTOR: PgType = PgType::Vector(VectorKind::Oid);
const INT2VECTOR: PgType = PgType::Vector(VectorKind::Int2);

/// Build an [`OIDVECTOR`] value from a sequence of OIDs.
fn oidvector(elems: impl IntoIterator<Item = u32>) -> Value {
    Value::Vector {
        kind: VectorKind::Oid,
        elems: elems.into_iter().map(Value::Oid).collect(),
    }
}

/// Build an [`INT2VECTOR`] value from a sequence of `int2`s.
fn int2vector(elems: impl IntoIterator<Item = i16>) -> Value {
    Value::Vector {
        kind: VectorKind::Int2,
        elems: elems.into_iter().map(Value::Int2).collect(),
    }
}

/// Build an [`INT2VECTOR`] of 1-based attribute numbers from 0-based column
/// indexes — the shape of `pg_index.indkey` and
/// `pg_partitioned_table.partattrs`.
///
/// `attnum` is an `int2` in PostgreSQL, which caps a relation at 32767 columns;
/// PostgreSQL never reaches that because it rejects a table past 1600 columns,
/// but this build has no such limit. A column index that does not fit is
/// reported as `0`, which is already PostgreSQL's `indkey` sentinel for "this
/// key is not a plain column reference" — the closest honest rendering. It must
/// not be a bare `as i16`: that panics on overflow in a debug build and wraps to
/// a negative attnum in a release one.
fn attnum_vector(columns: impl IntoIterator<Item = usize>) -> Value {
    int2vector(
        columns
            .into_iter()
            .map(|c| i16::try_from(c.saturating_add(1)).unwrap_or(0)),
    )
}

/// `pg_catalog.pg_language`.
pub fn pg_language_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_language",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("lanname", PgType::Name),
            col("lanowner", PgType::Oid),
            col("lanispl", PgType::Bool),
            col("lanpltrusted", PgType::Bool),
            col("lanplcallfoid", PgType::Oid),
            col("laninline", PgType::Oid),
            col("lanvalidator", PgType::Oid),
            col("lanacl", PgType::Text),
        ],
    )
}

/// The fixed `pg_language` rows.
///
/// 12/13/14 are PostgreSQL's bootstrap OIDs and are stable across versions.
/// `plpgsql`'s is not: PostgreSQL assigns it through `CREATE EXTENSION` at
/// initdb time, so it varies by build and there is nothing to reproduce —
/// clients match on `lanname`. The handler OIDs stay 0 until `pg_proc` carries
/// built-in rows for them to point at.
pub fn pg_language_rows() -> Vec<Vec<Value>> {
    let row = |oid: u32, name: &str, ispl: bool, trusted: bool| {
        vec![
            Value::Oid(oid),
            Value::Text(name.to_string()),
            Value::Oid(BOOTSTRAP_ROLE_OID),
            Value::Bool(ispl),
            Value::Bool(trusted),
            Value::Oid(0),
            Value::Oid(0),
            Value::Oid(0),
            Value::Null,
        ]
    };
    vec![
        row(12, "internal", false, false),
        row(13, "c", false, false),
        row(14, "sql", false, true),
        row(PLPGSQL_LANG_OID, "plpgsql", true, true),
    ]
}

/// The `pg_language` OID this build gives `plpgsql`. See [`pg_language_rows`]
/// for why it is ours to choose.
pub const PLPGSQL_LANG_OID: u32 = 13540;

/// `pg_catalog.pg_proc` — the columns clients read, in PostgreSQL's order.
pub fn pg_proc_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_proc",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("proname", PgType::Name),
            col("pronamespace", PgType::Oid),
            col("proowner", PgType::Oid),
            col("prolang", PgType::Oid),
            col("procost", PgType::Float4),
            col("prorows", PgType::Float4),
            col("provariadic", PgType::Oid),
            col("prosupport", REGPROC),
            col("prokind", CHARLIKE),
            col("prosecdef", PgType::Bool),
            col("proleakproof", PgType::Bool),
            col("proisstrict", PgType::Bool),
            col("proretset", PgType::Bool),
            col("provolatile", CHARLIKE),
            col("proparallel", CHARLIKE),
            col("pronargs", PgType::Int2),
            col("pronargdefaults", PgType::Int2),
            col("prorettype", PgType::Oid),
            col("proargtypes", OIDVECTOR),
            col("proallargtypes", PgType::Array(crabgresql_types::oid::OID)),
            col("proargmodes", PgType::Array(crabgresql_types::oid::TEXT)),
            col("proargnames", PgType::Array(crabgresql_types::oid::TEXT)),
            col("prosrc", PgType::Text),
            col("probin", PgType::Text),
        ],
    )
}

/// The built-in `pg_proc` rows generated from `pg_proc.dat` — the functions the
/// other catalogs reference, and only those (see `gen_pg_proc` in `build.rs`).
/// Callers append the session's `CREATE FUNCTION` routines after these.
///
/// `proallargtypes`/`proargmodes`/`proargnames` are NULL for every one: none of
/// these take OUT parameters, and codegen refuses to emit an entry that does
/// rather than drop the columns silently.
pub fn pg_proc_builtin_rows() -> Vec<Vec<Value>> {
    // crabgresql's own table-AM handlers, so `pg_am.amhandler` resolves for
    // every method this build ships rather than only the upstream ones. They
    // are shaped like PostgreSQL's: volatile, strict, `internal` argument,
    // returning `table_am_handler` (oid 269).
    let own = OWN_AM_HANDLERS.iter().map(|(oid, name)| {
        vec![
            Value::Oid(*oid),
            Value::Text((*name).to_string()),
            Value::Oid(11),
            Value::Oid(BOOTSTRAP_ROLE_OID),
            Value::Oid(12),
            Value::Float4(1.0),
            Value::Float4(0.0),
            Value::Oid(0),
            Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
            chr('f'),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(false),
            chr('v'),
            chr('s'),
            Value::Int2(1),
            Value::Int2(0),
            Value::Oid(269),
            oidvector([2281]),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Text((*name).to_string()),
            Value::Null,
        ]
    });
    PG_PROC_ROWS
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Text(r.proname.to_string()),
                // Every built-in lives in `pg_catalog`, owned by the bootstrap
                // superuser.
                Value::Oid(11),
                Value::Oid(BOOTSTRAP_ROLE_OID),
                Value::Oid(r.prolang),
                Value::Float4(r.procost),
                Value::Float4(r.prorows),
                Value::Oid(r.provariadic),
                regproc(r.prosupport),
                str_char(r.prokind),
                Value::Bool(r.prosecdef),
                Value::Bool(r.proleakproof),
                Value::Bool(r.proisstrict),
                Value::Bool(r.proretset),
                str_char(r.provolatile),
                str_char(r.proparallel),
                Value::Int2(r.pronargs),
                Value::Int2(0),
                Value::Oid(r.prorettype),
                oidvector(r.proargtypes.iter().copied()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Text(r.prosrc.to_string()),
                Value::Null,
            ]
        })
        .chain(own)
        .collect()
}

/// The `pg_proc` rows for the routines this server holds, appended after
/// [`pg_proc_builtin_rows`].
///
/// Honest for everything the catalog actually knows. The stubs are the columns
/// nothing here can have an opinion about yet, each set to PostgreSQL's own
/// default rather than to zero: `procost`/`prorows` (no planner cost model),
/// `provariadic`/`pronargdefaults` (VARIADIC and argument defaults are
/// rejected), `prosupport`/`proleakproof`/`proparallel`. `probin` is NULL
/// honestly — there are no C functions.
pub fn pg_proc_rows(
    routines: &[CatalogRoutine],
    namespace_oids: &HashMap<String, u32>,
) -> Vec<Vec<Value>> {
    routines
        .iter()
        .map(|r| {
            // PostgreSQL leaves these NULL rather than empty when there is
            // nothing to report, and clients test for NULL.
            let optional_array = |elem: PgType, values: Vec<Value>| {
                if values.is_empty() {
                    Value::Null
                } else {
                    Value::Array {
                        elem,
                        elems: values,
                    }
                }
            };
            vec![
                Value::Oid(r.oid),
                Value::Text(r.name.clone()),
                Value::Oid(namespace_oids.get(&r.namespace).copied().unwrap_or(2200)),
                Value::Oid(BOOTSTRAP_ROLE_OID),
                Value::Oid(r.lang),
                // PostgreSQL's defaults: 1 for a built-in, 100 for anything
                // whose body it has to run.
                Value::Float4(if r.lang == 12 || r.lang == 13 {
                    1.0
                } else {
                    100.0
                }),
                Value::Float4(if r.retset { 1000.0 } else { 0.0 }),
                Value::Oid(0),
                // prosupport: a user routine has no planner support function.
                Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
                chr(r.kind),
                Value::Bool(r.secdef),
                Value::Bool(false),
                Value::Bool(r.strict),
                Value::Bool(r.retset),
                chr(r.volatile),
                chr('u'),
                Value::Int2(r.arg_types.len() as i16),
                Value::Int2(0),
                Value::Oid(r.ret_type),
                oidvector(r.arg_types.iter().copied()),
                optional_array(
                    PgType::Oid,
                    r.all_arg_types.iter().map(|t| Value::Oid(*t)).collect(),
                ),
                optional_array(
                    PgType::Text,
                    r.arg_modes
                        .iter()
                        .map(|m| Value::Text(m.to_string()))
                        .collect(),
                ),
                optional_array(
                    PgType::Text,
                    r.arg_names.iter().map(|n| Value::Text(n.clone())).collect(),
                ),
                Value::Text(r.src.clone()),
                Value::Null,
            ]
        })
        .collect()
}
