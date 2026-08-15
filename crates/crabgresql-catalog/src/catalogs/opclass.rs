//! `pg_opclass` and `pg_opfamily`: the operator classes an index key is
//! built under, and the families that group them.
//!
//! Nothing in this build *chooses* an operator class — DDL takes no
//! `USING btree (col opclass)` clause, so every key gets its type's default.
//! The catalog still has to name that default: `pg_index.indclass` is a
//! reference into `pg_opclass`, and `\d`, `pg_dump` and `pg_get_indexdef` all
//! read it to decide whether an index definition must spell an opclass out.
//! [`default_opclass`] is where that choice is made.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::cols::*;
use crate::oids::*;
use crate::{PG_CAST_ROWS, PG_OPCLASS_ROWS, PG_OPFAMILY_ROWS, PG_TYPE_ROWS, SystemCatalog};

/// `anyarray`, the `opcintype` of the one class every array type indexes under.
const ANYARRAY_OID: u32 = 2277;
/// `anyenum`, likewise for every enum type.
const ANYENUM_OID: u32 = 3500;

pub(crate) fn pg_opclass_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_opclass",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("opcmethod", PgType::Oid),
            col("opcname", PgType::Name),
            col("opcnamespace", PgType::Oid),
            col("opcowner", PgType::Oid),
            col("opcfamily", PgType::Oid),
            col("opcintype", PgType::Oid),
            col("opcdefault", PgType::Bool),
            col("opckeytype", PgType::Oid),
        ],
    )
}

/// The built-in operator classes, generated from `pg_opclass.dat` — including
/// the classes of access methods this build has no index for. `pg_am` lists
/// those methods too: a row here says the *type* has a class under the method,
/// which is upstream's statement to make, not this server's.
pub(crate) fn pg_opclass_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    PG_OPCLASS_ROWS
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Oid(r.opcmethod),
                Value::Text(r.opcname.to_string()),
                Value::Oid(PG_CATALOG_NAMESPACE_OID),
                Value::Oid(BOOTSTRAP_ROLE_OID),
                Value::Oid(r.opcfamily),
                Value::Oid(r.opcintype),
                Value::Bool(r.opcdefault),
                Value::Oid(r.opckeytype),
            ]
        })
        .collect()
}

pub(crate) fn pg_opfamily_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_opfamily",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("opfmethod", PgType::Oid),
            col("opfname", PgType::Name),
            col("opfnamespace", PgType::Oid),
            col("opfowner", PgType::Oid),
        ],
    )
}

/// The built-in operator families, generated from `pg_opfamily.dat`. Served
/// because `pg_opclass.opcfamily` points here — a client following that
/// reference is the reader this relation exists for.
pub(crate) fn pg_opfamily_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    PG_OPFAMILY_ROWS
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Oid(r.opfmethod),
                Value::Text(r.opfname.to_string()),
                Value::Oid(PG_CATALOG_NAMESPACE_OID),
                Value::Oid(BOOTSTRAP_ROLE_OID),
            ]
        })
        .collect()
}

/// The OID of the default operator class for `ty` under access method
/// `am_oid` — what `pg_index.indclass` reports for a key PostgreSQL's DDL was
/// not given an explicit class for, which here is every key.
///
/// PostgreSQL resolves this in `GetDefaultOpClass`, and the three tiers below
/// are that rule's observable shape:
///
/// 1. a default class whose `opcintype` is the type itself;
/// 2. the polymorphic classes, reached by the type's **`typcategory`** — `A`
///    indexes under the `anyarray` class, `E` under the `anyenum` one. The
///    category is what decides, not how the type is spelled here:
///    `int2vector` is category `A` and has no class of its own, so
///    PostgreSQL gives it `array_ops` even though it is not a `PgType::Array`.
/// 3. a default class for a type this one is *binary-coercible* to, which
///    `pg_cast` already records as a `castmethod` of `b`.
///
/// `0` when no class matches. PostgreSQL would have refused the `CREATE INDEX`
/// outright ("data type X has no default operator class"), so this stands for
/// an index this build accepted and PostgreSQL would not — and `0` is
/// PostgreSQL's own spelling of "no such object", which is the honest report.
pub(crate) fn default_opclass(am_oid: u32, ty: PgType) -> u32 {
    let class_for = |type_oid: u32| {
        PG_OPCLASS_ROWS
            .iter()
            .find(|r| r.opcmethod == am_oid && r.opcdefault && r.opcintype == type_oid)
            .map(|r| r.oid)
    };
    let polymorphic = match ty {
        // A user type has no `pg_type.dat` row to read a category from, and
        // every one this build reflects is an enum; see
        // `catalogs::types::pg_type_user_rows`.
        PgType::User(_) => Some(ANYENUM_OID),
        // `R`/`M` are deliberately absent rather than overlooked: there are no
        // range or multirange types here, so no key can carry one.
        _ => PG_TYPE_ROWS
            .iter()
            .find(|r| r.oid == ty.oid())
            .and_then(|r| match r.typcategory {
                "A" => Some(ANYARRAY_OID),
                "E" => Some(ANYENUM_OID),
                _ => None,
            }),
    };
    let binary_coercible = || {
        PG_CAST_ROWS
            .iter()
            .filter(|c| c.castsource == ty.oid() && c.castmethod == "b")
            .find_map(|c| class_for(c.casttarget))
    };
    class_for(ty.oid())
        .or_else(|| polymorphic.and_then(class_for))
        .or_else(binary_coercible)
        .unwrap_or(0)
}
