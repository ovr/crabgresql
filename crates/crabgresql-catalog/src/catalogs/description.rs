//! `pg_description`: the comments attached to database objects.
//!
//! Three sources, all of them objects this build genuinely has:
//!
//! - the **bootstrap** half, generated from the `descr` fields of the vendored
//!   `.dat` files (see `crabgresql-bki`'s `pg_description` module) — the
//!   built-in types, the functions `pg_proc` publishes, the access methods, the
//!   languages and the reserved schemas;
//! - the two objects whose descriptions PostgreSQL itself does not take from a
//!   `.dat` file: the `plpgsql` extension and its language, both created by
//!   `CREATE EXTENSION` there and both really present here;
//! - crabgresql's own `parquet` and `buffer` access methods, described in
//!   **our** words (see [`OWN_AM_DESCRIPTIONS`]).
//!
//! What is still missing is the **user** half: `COMMENT ON` is not parsed, so no
//! user object here has a comment to report. That also makes every `objsubid` 0
//! — which is not a gap: PostgreSQL's bootstrap data has no column comments
//! either (a fresh 18.4 has not one row with `objsubid > 0`, nor one with
//! `classoid = 'pg_class'`), so `\d+` shows an empty Description column there
//! too.
//!
//! The count is smaller than PostgreSQL's ~5400 for a reason rather than a
//! deferral: most of the rest describes `pg_collation`, whose descriptions
//! `initdb` writes from the locales it finds rather than from any `.dat`, and
//! the `pg_proc` entries no served catalog references.
//!
//! Nothing may describe an object that is not there — the invariant
//! [`PUBLISHED`] enforces for the catalogs codegen could not.
//!
//! TODO: emit the user descriptions once `COMMENT ON` exists.

use std::collections::HashMap;
use std::sync::OnceLock;

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::catalogs::am::BUILTIN_AMS;
use crate::catalogs::extension::available_extensions;
use crate::catalogs::language::BUILTIN_LANGUAGES;
use crate::catalogs::namespace::BUILTIN_NAMESPACES;
use crate::cols::*;
use crate::oids::*;
use crate::registry::builtin_relation_oid;
use crate::{PG_DESCRIPTION_ROWS, SystemCatalog};

pub(crate) fn pg_description_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_description",
        "pg_catalog",
        vec![
            col("objoid", PgType::Oid),
            col("classoid", PgType::Oid),
            col("objsubid", PgType::Int4),
            col("description", PgType::Text),
        ],
    )
}

/// One comment: `(classoid, objoid, objsubid, description)`, the shape of a
/// `pg_description` row.
type Description = (u32, u32, i32, &'static str);

/// A `pg_catalog` relation and the test for whether it really has the row a
/// generated description names.
type Published = (&'static str, fn(u32) -> bool);

/// The generated descriptions this build publishes, class by class.
///
/// `pg_type`, `pg_proc` and `pg_operator` need no test — `catalogs::types` and
/// `catalogs::operator` publish every generated row of theirs, and codegen
/// already restricted the functions to the ones `catalogs::proc` publishes. The
/// other three are hand-written, so the test is
/// the list they are written from: `pg_namespace.dat`, for instance, also
/// describes the subscription conflict-log schema, which this build has not
/// got.
const PUBLISHED: &[Published] = &[
    ("pg_type", |_| true),
    ("pg_proc", |_| true),
    ("pg_operator", |_| true),
    ("pg_conversion", |_| true),
    ("pg_ts_parser", |_| true),
    ("pg_ts_template", |_| true),
    ("pg_ts_dict", |_| true),
    ("pg_ts_config", |_| true),
    ("pg_am", |oid| BUILTIN_AMS.iter().any(|(o, ..)| *o == oid)),
    ("pg_language", |oid| {
        BUILTIN_LANGUAGES.iter().any(|(o, ..)| *o == oid)
    }),
    ("pg_namespace", |oid| {
        BUILTIN_NAMESPACES.iter().any(|(o, _)| *o == oid)
    }),
];

/// crabgresql's own access methods. Upstream has nothing to copy for a method
/// it has never heard of, and a blank Description in `\dA+` would read as a
/// missing row rather than as an object nobody described — so the wording here
/// is ours.
const OWN_AM_DESCRIPTIONS: &[(u32, &str)] = &[
    (
        PARQUET_AM_OID,
        "managed append-only Parquet table access method",
    ),
    (BUFFER_AM_OID, "WAL-logged RAM-resident table access method"),
];

/// Every description this build publishes: one source for both readers — the
/// relation's rows and the `obj_description` lookup — so the function and a
/// direct `SELECT` cannot disagree about what is commented.
fn descriptions() -> &'static [Description] {
    static ROWS: OnceLock<Vec<Description>> = OnceLock::new();
    ROWS.get_or_init(|| {
        // Every relation named here is served, so this always resolves; a row
        // that ever fell to `0` is dropped below rather than filed under a
        // catalog that does not exist.
        let classoid = |name: &str| builtin_relation_oid(name).unwrap_or(0);
        let mut rows: Vec<Description> = PG_DESCRIPTION_ROWS
            .iter()
            .filter(|row| {
                PUBLISHED
                    .iter()
                    .any(|(catalog, published)| *catalog == row.catalog && published(row.objoid))
            })
            .map(|row| (classoid(row.catalog), row.objoid, 0, row.description))
            .collect();

        let extension = classoid("pg_extension");
        let language = classoid("pg_language");
        for ext in available_extensions() {
            // The extension and the language it installs carry the same comment
            // on PostgreSQL, both written by `CREATE EXTENSION` rather than by
            // any `.dat`.
            rows.push((extension, PLPGSQL_EXTENSION_OID, 0, ext.comment));
            rows.push((language, PLPGSQL_LANG_OID, 0, ext.comment));
        }

        // The snowball template, dictionaries and configurations, whose
        // comments `initdb` writes with `COMMENT ON` rather than taking from a
        // `.dat` — the same standing as the rows themselves.
        rows.extend(
            crate::catalogs::textsearch::snowball_descriptions()
                .into_iter()
                .map(|(catalog, objoid, description)| (classoid(catalog), objoid, 0, description)),
        );

        let am = classoid("pg_am");
        rows.extend(
            OWN_AM_DESCRIPTIONS
                .iter()
                .map(|(oid, description)| (am, *oid, 0, *description)),
        );

        rows.retain(|(classoid, ..)| *classoid != 0);
        rows.sort_unstable();
        rows
    })
}

/// The comment on `objoid` in `classoid`, as `obj_description` and
/// `col_description` report it. `objsubid` is 0 for a whole object and the
/// column number for a column comment.
pub fn object_description(classoid: u32, objoid: u32, objsubid: i32) -> Option<&'static str> {
    static INDEX: OnceLock<HashMap<(u32, u32, i32), &'static str>> = OnceLock::new();
    INDEX
        .get_or_init(|| {
            descriptions()
                .iter()
                .map(|(classoid, objoid, objsubid, description)| {
                    ((*classoid, *objoid, *objsubid), *description)
                })
                .collect()
        })
        .get(&(classoid, objoid, objsubid))
        .copied()
}

/// Every comment on `objoid` whatever catalog it lives in — what the deprecated
/// one-argument `obj_description(oid)` searches. More than one is possible in
/// principle (OIDs are unique per catalog, not globally), and PostgreSQL raises
/// rather than picking one, so the caller is handed all of them.
pub fn object_descriptions_any_class(objoid: u32, objsubid: i32) -> Vec<&'static str> {
    descriptions()
        .iter()
        .filter(|(_, oid, subid, _)| *oid == objoid && *subid == objsubid)
        .map(|(_, _, _, description)| *description)
        .collect()
}

/// The rows of `pg_catalog.pg_description`, in PostgreSQL's column order.
pub(crate) fn pg_description_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    descriptions()
        .iter()
        .map(|(classoid, objoid, objsubid, description)| {
            vec![
                Value::Oid(*objoid),
                Value::Oid(*classoid),
                Value::Int4(*objsubid),
                Value::Text((*description).to_string()),
            ]
        })
        .collect()
}
