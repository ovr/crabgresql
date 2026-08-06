//! `pg_type`, `pg_enum` and `pg_cast`: the type catalogs.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;
use crate::oids::*;
use crabgresql_types::{Reg, RegKind};

use crate::{CatalogUserType, PG_CAST_ROWS, PG_TYPE_ROWS};

/// `pg_catalog.pg_type` — a curated, PG-ordered subset of the columns clients
/// query. The rarely-read domain and ACL columns (`typnotnull`, `typtypmod`,
/// `typndims`, `typdefaultbin`, `typdefault`, `typacl`) are omitted for now.
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
            col("typbasetype", PgType::Oid),
            col("typcollation", PgType::Oid),
        ],
    )
}

/// The built-ins first, then this session's `CREATE TYPE`s. The order is what
/// keeps a built-in OID at the same row index across snapshots that differ only
/// in their user types.
pub(crate) fn pg_type_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let mut rows = pg_type_builtin_rows();
    rows.extend(pg_type_user_rows(cat.user_types()));
    rows
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
pub(crate) fn pg_type_user_rows(user_types: &[CatalogUserType]) -> Vec<Vec<Value>> {
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
