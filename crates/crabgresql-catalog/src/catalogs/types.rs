//! `pg_type`, `pg_enum` and `pg_cast`: the type catalogs.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;
use crate::oids::*;
use crabgresql_types::{Reg, RegKind};

use crate::{CatalogDomain, CatalogUserType, PG_CAST_ROWS, PG_TYPE_ROWS};

/// `pg_catalog.pg_type` — a curated, PG-ordered subset of the columns clients
/// query.
///
/// The rows generated from `pg_type.dat` spell the six domain columns out at
/// PostgreSQL's BKI defaults, because none of them is a domain: every entry in
/// that file is a base or pseudo type, and a derived array row is not a domain
/// either. The domains come from the other two sources below —
/// `information_schema`'s five and every `CREATE DOMAIN` — and both read those
/// six off the type.
pub(crate) fn pg_type_schema() -> TableSchema {
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
            col("typnotnull", PgType::Bool),
            col("typbasetype", PgType::Oid),
            col("typtypmod", PgType::Int4),
            col("typndims", PgType::Int4),
            col("typcollation", PgType::Oid),
            col("typdefaultbin", NODE_TREE),
            col("typdefault", PgType::Text),
            col("typacl", ACLITEM_ARRAY),
        ],
    )
}

/// The built-ins, then `information_schema`'s domains, then this session's
/// `CREATE TYPE`s. The order is what keeps a built-in OID at the same row index
/// across snapshots that differ only in their user types — which is why the
/// user types stay last and the fixed bootstrap set is spliced ahead of them.
pub(crate) fn pg_type_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let mut rows = pg_type_builtin_rows();
    rows.extend(pg_type_bootstrap_domain_rows());
    rows.extend(pg_type_user_rows(cat.user_types()));
    rows
}

/// The `pg_type` rows for the `information_schema` domains and their array
/// types, in [`crate::info_schema::DOMAINS`] order — the domain first, then its
/// array, as `initdb` created each pair.
fn pg_type_bootstrap_domain_rows() -> Vec<Vec<Value>> {
    crate::info_schema::DOMAINS
        .iter()
        .flat_map(|d| {
            let ty = d.as_catalog_type();
            let domain = ty.domain.as_ref().expect("bootstrap type is a domain");
            [
                pg_type_domain_row(
                    &ty,
                    domain,
                    DomainPlacement {
                        namespace: crate::info_schema::NAMESPACE_OID,
                        typarray: d.array_oid,
                    },
                ),
                pg_type_domain_array_row(d),
            ]
        })
        .collect()
}

/// One `pg_type` row for the array type over an `information_schema` domain.
///
/// An array over a domain is an ordinary `typtype = 'b'` row: PostgreSQL's
/// generic array I/O, the element's collation, and `typelem` pointing back at
/// the domain. It carries no `typarray` of its own — nothing makes an array of
/// an array.
fn pg_type_domain_array_row(d: &crate::info_schema::BootstrapDomain) -> Vec<Value> {
    // An array aligns like its element, but never looser than `int`: `name` is
    // char-aligned and `_sql_identifier` is still `i`, while `timestamptz` is
    // double-aligned and `_time_stamp` keeps `d`. Probed on 18.4.
    let typalign = match PG_TYPE_ROWS
        .iter()
        .find(|r| r.oid == d.base.oid())
        .map(|r| r.typalign)
    {
        Some("d") => "d",
        _ => "i",
    };
    vec![
        Value::Oid(d.array_oid),
        Value::Text(d.array_name.to_string()),
        Value::Oid(crate::info_schema::NAMESPACE_OID),
        Value::Oid(BOOTSTRAP_ROLE_OID),
        Value::Int2(-1),
        Value::Bool(false),
        chr('b'),
        chr('A'),
        Value::Bool(false),
        Value::Bool(true),
        chr(','),
        Value::Oid(0),
        regproc_by_name("array_subscript_handler"),
        Value::Oid(d.oid),
        Value::Oid(0),
        regproc_by_name("array_in"),
        regproc_by_name("array_out"),
        regproc_by_name("array_recv"),
        regproc_by_name("array_send"),
        // typmodin / typmodout: the element's modifier is fixed on the domain.
        Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
        Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
        regproc_by_name("array_typanalyze"),
        str_char(typalign),
        chr('x'),
        // typnotnull / typbasetype / typtypmod / typndims
        Value::Bool(false),
        Value::Oid(0),
        Value::Int4(-1),
        Value::Int4(0),
        Value::Oid(d.collation),
        // typdefaultbin / typdefault / typacl
        Value::Null,
        Value::Null,
        Value::Null,
    ]
}

/// The built-in `pg_type` rows generated from `pg_type.dat`.
pub(crate) fn pg_type_builtin_rows() -> Vec<Vec<Value>> {
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
                // typnotnull / typbasetype
                Value::Bool(false),
                Value::Oid(0),
                // typtypmod: `format_type` renders the pair (0, -1) as `-`,
                // which is what a client reading a definition expects.
                Value::Int4(-1),
                // typndims: nonzero only on a domain over an array.
                Value::Int4(0),
                Value::Oid(r.typcollation),
                // typdefaultbin / typdefault / typacl
                Value::Null,
                Value::Null,
                Value::Null,
            ]
        })
        .collect()
}

/// The `pg_type` rows for user-defined enums and domains, appended after
/// [`pg_type_builtin_rows`]. Column order matches [`pg_type_schema`].
///
/// TODO: reflect a `CREATE TYPE ... (INPUT = …)` base type into `pg_type`. Only
/// enums (`typtype = 'e'`) and domains (`typtype = 'd'`) are emitted, so any
/// other user type is invisible to a client reading the catalog.
pub(crate) fn pg_type_user_rows(user_types: &[CatalogUserType]) -> Vec<Vec<Value>> {
    let mut rows: Vec<Vec<Value>> = user_types
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
                // typnotnull / typbasetype / typtypmod / typndims
                Value::Bool(false),
                Value::Oid(0),
                Value::Int4(-1),
                Value::Int4(0),
                // An enum is not collatable.
                Value::Oid(0),
                // typdefaultbin / typdefault / typacl
                Value::Null,
                Value::Null,
                Value::Null,
            ]
        })
        .collect();
    rows.extend(
        user_types
            .iter()
            .filter_map(|t| Some((t, t.domain.as_ref()?)))
            .map(|(t, d)| {
                pg_type_domain_row(
                    t,
                    d,
                    DomainPlacement {
                        // `public`, where CREATE DOMAIN puts a domain — see the
                        // enum row above for why that is also what
                        // `SystemCatalog::user_type_ref` reports.
                        namespace: PUBLIC_NAMESPACE_OID,
                        // This build creates no array type for a user domain,
                        // so `ARRAY(SELECT domain_col)` raises an honest 0A000.
                        typarray: 0,
                    },
                )
            }),
    );
    rows
}

/// Where a domain sits, which is the only thing the `information_schema` five
/// and a `CREATE DOMAIN` disagree about. A struct rather than two `u32`
/// arguments, which nothing would stop a caller from swapping.
pub(crate) struct DomainPlacement {
    pub(crate) namespace: u32,
    /// The domain's array type, or `0` for a domain that has none.
    pub(crate) typarray: u32,
}

/// One `pg_type` row for a domain, whether `initdb` or `CREATE DOMAIN` made it.
///
/// A domain borrows most of its row from the base type it is over — width,
/// pass-by-value, alignment, storage, category, and the output/send functions —
/// because its values *are* base values. What it does not borrow is the input
/// side: PostgreSQL 18.4 shows `typinput = domain_in` and
/// `typreceive = domain_recv` on every domain row, since reading a value in is
/// where the constraints run.
fn pg_type_domain_row(
    t: &CatalogUserType,
    d: &CatalogDomain,
    placement: DomainPlacement,
) -> Vec<Value> {
    let base = PG_TYPE_ROWS.iter().find(|r| r.oid == d.resolved_basetype);
    // A base this build has no `pg_type.dat` row for cannot contribute its
    // physical columns; the variable-length defaults are the safe answer, and
    // no such base can currently exist (the base is always a built-in).
    let (typlen, typbyval, typcategory, typalign, typstorage) = match base {
        Some(r) => (
            r.typlen,
            r.typbyval,
            r.typcategory,
            r.typalign,
            r.typstorage,
        ),
        None => (-1, false, "S", "i", "x"),
    };
    vec![
        Value::Oid(t.oid),
        Value::Text(t.name.clone()),
        Value::Oid(placement.namespace),
        Value::Oid(BOOTSTRAP_ROLE_OID),
        Value::Int2(typlen),
        Value::Bool(typbyval),
        chr('d'),
        str_char(typcategory),
        Value::Bool(false),
        Value::Bool(true),
        chr(','),
        // typrelid / typsubscript / typelem: a domain is not a composite and is
        // not subscriptable — an array *over* a domain is a separate row.
        Value::Oid(0),
        Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
        Value::Oid(0),
        Value::Oid(placement.typarray),
        regproc_by_name("domain_in"),
        base.map_or_else(
            || Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
            |r| regproc(r.typoutput),
        ),
        regproc_by_name("domain_recv"),
        base.map_or_else(
            || Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
            |r| regproc(r.typsend),
        ),
        // typmodin / typmodout / typanalyze: a domain's modifier is fixed at
        // creation, so it needs no modifier I/O of its own.
        Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
        Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
        Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
        str_char(typalign),
        str_char(typstorage),
        Value::Bool(d.not_null),
        Value::Oid(d.basetype),
        Value::Int4(crabgresql_storage_api::pg_typmod(
            PgType::from_oid(d.resolved_basetype).unwrap_or(PgType::Text),
            d.typmod,
        )),
        // typndims: nonzero only on a domain over an array, which this build
        // has no way to declare.
        Value::Int4(0),
        // A domain over a collatable base carries the base's collation unless
        // one was named — 18.4 shows `typcollation = 100` (the default
        // collation) on a domain over `text`, not 0.
        Value::Oid(match d.collation {
            0 => base.map_or(0, |r| r.typcollation),
            explicit => explicit,
        }),
        // typdefaultbin mirrors typdefault here: both hold the same stored SQL,
        // which is what `information_schema.domains.domain_default` reads.
        d.default.clone().map_or(Value::Null, Value::Text),
        d.default.clone().map_or(Value::Null, Value::Text),
        Value::Null,
    ]
}

/// `pg_catalog.pg_enum` — one row per (enum type, label). `enumsortorder` is the
/// 1-based definition position (PG stores a float4 so labels can be inserted
/// between existing ones; a freshly created enum uses 1, 2, 3, …).
pub(crate) fn pg_enum_schema() -> TableSchema {
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
pub(crate) fn pg_enum_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let user_types = cat.user_types();
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
pub(crate) fn pg_cast_schema() -> TableSchema {
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
pub(crate) fn pg_cast_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
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
