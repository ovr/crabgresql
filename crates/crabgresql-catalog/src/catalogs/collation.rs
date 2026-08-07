//! `pg_collation`, published from the collations this build actually sorts by.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;
use crate::oids::*;

/// `pg_catalog.pg_collation` — the collations this build ships. `collversion`
/// is omitted: it exists so PostgreSQL can warn when the underlying OS locale
/// data changes under an index, and the ICU data here is compiled in, so there
/// is no external version to drift from.
pub(crate) fn pg_collation_schema() -> TableSchema {
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
pub(crate) fn pg_collation_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
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
