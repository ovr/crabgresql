//! `pg_description`: the comments attached to database objects.
//!
//! One row, and it is a real one: the description PostgreSQL ships for the
//! `plpgsql` extension, which this build genuinely installs (see
//! [`crate::catalogs::extension`]). psql's `\dx` reads its Description column
//! from here rather than from `pg_available_extensions.comment`, so without the
//! row the extension lists with a blank description where PostgreSQL prints one.
//!
//! The other two halves of this relation are absent, both honestly:
//!
//! - the **user** half needs `COMMENT ON`, which the binder does not parse, so
//!   no user object here has a comment to report;
//! - the **bootstrap** half is some 5400 rows describing PostgreSQL's own
//!   catalogs and functions, which live in its `.dat` files and would have to be
//!   generated the way [`crate::catalogs::proc`]'s rows are.
//!
//! So `\d+` on a user table shows an empty description — exactly what
//! PostgreSQL shows for a table nobody commented on.
//!
//! TODO: emit the bootstrap descriptions from the `.dat` scanner in
//! `crabgresql-bki`, and the user ones once `COMMENT ON` exists.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::catalogs::extension::available_extensions;
use crate::cols::*;
use crate::oids::*;
use crate::registry::builtin_relation_oid;

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

/// The `plpgsql` extension's description, as PostgreSQL 18.4 stores it:
/// `objsubid = 0` describes the object as a whole rather than one of its
/// columns, and `classoid` names the catalog it lives in.
pub(crate) fn pg_description_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    // `pg_extension` is served by this build, so the OID always resolves; `0`
    // would be the "no such catalog" answer if it ever stopped being.
    let classoid = builtin_relation_oid("pg_extension").unwrap_or(0);
    available_extensions()
        .iter()
        .map(|(_, _, comment)| {
            vec![
                Value::Oid(PLPGSQL_EXTENSION_OID),
                Value::Oid(classoid),
                Value::Int4(0),
                Value::Text(comment.to_string()),
            ]
        })
        .collect()
}
