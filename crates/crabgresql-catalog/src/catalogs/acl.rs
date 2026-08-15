//! Privileges, security labels, and the two shared catalogs that hang off a
//! role: what `GRANT`, `SECURITY LABEL`, `COMMENT ON DATABASE` and
//! `ALTER ROLE … SET` would write.
//!
//! None of those statements exists in this build, so every relation here is
//! empty — the same answer PostgreSQL gives on a database where nobody has run
//! them. The `aclitem[]` columns are typed as `text` for the reason
//! [`crate::cols::ACLITEM_ARRAY`] documents: there is no `aclitem` type here,
//! and no row ever renders one.
//!
//! Three of these are *not* empty on a stock PostgreSQL. Where that is so the
//! docstring says which rows are missing and why, because "empty because
//! reality is empty" and "empty because the subsystem is missing" are different
//! claims and only the first stays true as the build grows.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, oid};

use crate::cols::*;

/// `pg_default_acl` — the per-schema defaults `ALTER DEFAULT PRIVILEGES` sets.
/// Empty in PostgreSQL too until someone runs that statement.
pub(crate) fn pg_default_acl_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_default_acl",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("defaclrole", PgType::Oid),
            col("defaclnamespace", PgType::Oid),
            col("defaclobjtype", CHARLIKE),
            col("defaclacl", ACLITEM_ARRAY),
        ],
    )
}

/// `pg_init_privs` — the privileges an object had when `initdb` (or `CREATE
/// EXTENSION`) finished with it, so that `pg_dump` can emit only the *changes*
/// made since.
///
/// **Diverges from PostgreSQL, which has ~228 rows here on a fresh cluster.**
/// Every one of them describes a system catalog's initial ACL. This build does
/// not reflect its catalogs into `pg_class` at all — they are served from
/// [`crate::registry`] with fixed OIDs, not stored as relations — so there is no
/// object for such a row to point at, and no `GRANT` that could have changed it.
/// The empty answer costs nothing here: `pg_dump` reads this to *subtract* a
/// baseline, and a baseline of "nothing was granted" matches a build where
/// nothing can be.
pub(crate) fn pg_init_privs_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_init_privs",
        "pg_catalog",
        vec![
            col("objoid", PgType::Oid),
            col("classoid", PgType::Oid),
            col("objsubid", PgType::Int4),
            col("privtype", CHARLIKE),
            col("initprivs", ACLITEM_ARRAY),
        ],
    )
}

/// `pg_parameter_acl` — the GUCs someone has `GRANT SET`ed on. Empty in
/// PostgreSQL as well until that happens.
pub(crate) fn pg_parameter_acl_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_parameter_acl",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("parname", PgType::Text),
            col("paracl", ACLITEM_ARRAY),
        ],
    )
}

/// `pg_shdepend` — dependencies of local objects on *shared* ones: which role
/// owns what, which role a grant names. PostgreSQL's rows here all record an
/// ownership by a non-bootstrap role; this build has only the bootstrap role
/// (see [`crate::catalogs::auth`]), and PostgreSQL deliberately records no
/// `pg_shdepend` row for objects owned by it, so both servers agree on empty.
pub(crate) fn pg_shdepend_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_shdepend",
        "pg_catalog",
        vec![
            col("dbid", PgType::Oid),
            col("classid", PgType::Oid),
            col("objid", PgType::Oid),
            col("objsubid", PgType::Int4),
            col("refclassid", PgType::Oid),
            col("refobjid", PgType::Oid),
            col("deptype", CHARLIKE),
        ],
    )
}

/// `pg_seclabel` — labels `SECURITY LABEL` attached to local objects. There is
/// no label provider here (none is loaded in a stock PostgreSQL either), so the
/// statement could not succeed even if it parsed, and both are empty.
pub(crate) fn pg_seclabel_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_seclabel",
        "pg_catalog",
        vec![
            col("objoid", PgType::Oid),
            col("classoid", PgType::Oid),
            col("objsubid", PgType::Int4),
            col("provider", PgType::Text),
            col("label", PgType::Text),
        ],
    )
}

/// `pg_shseclabel` — the shared-object half of [`pg_seclabel_schema`]: labels on
/// roles, databases, tablespaces.
pub(crate) fn pg_shseclabel_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_shseclabel",
        "pg_catalog",
        vec![
            col("objoid", PgType::Oid),
            col("classoid", PgType::Oid),
            col("provider", PgType::Text),
            col("label", PgType::Text),
        ],
    )
}

/// `pg_seclabels` — the human-readable view over the two label catalogs, adding
/// the object's type, schema and name. Empty because its inputs are.
pub(crate) fn pg_seclabels_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_seclabels",
        "pg_catalog",
        vec![
            col("objoid", PgType::Oid),
            col("classoid", PgType::Oid),
            col("objsubid", PgType::Int4),
            col("objtype", PgType::Text),
            col("objnamespace", PgType::Oid),
            col("objname", PgType::Text),
            col("provider", PgType::Text),
            col("label", PgType::Text),
        ],
    )
}

/// `pg_shdescription` — comments on shared objects: databases, roles,
/// tablespaces.
///
/// **Diverges from PostgreSQL, which has three rows here**, the comments
/// `initdb` puts on `template1`, `template0` and `postgres`. A crabgresql server
/// holds exactly one database and reports it as the single `pg_database` row
/// (see [`crate::catalogs::database`]); the three PostgreSQL describes do not
/// exist to be commented on, and `COMMENT ON DATABASE` is not implemented, so
/// nothing else could have written a row. This is the local-object catalog
/// `pg_description`'s shared twin — that one *does* carry its bootstrap rows.
pub(crate) fn pg_shdescription_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_shdescription",
        "pg_catalog",
        vec![
            col("objoid", PgType::Oid),
            col("classoid", PgType::Oid),
            col("description", PgType::Text),
        ],
    )
}

/// `pg_db_role_setting` — the per-database and per-role GUC defaults
/// `ALTER ROLE … SET` and `ALTER DATABASE … SET` write.
///
/// Empty in PostgreSQL too until one of those runs. Neither is implemented
/// here; when `ALTER ROLE … SET` arrives this stops being a stub and becomes a
/// projection over wherever those settings are kept, alongside
/// [`crate::catalogs::settings`].
pub(crate) fn pg_db_role_setting_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_db_role_setting",
        "pg_catalog",
        vec![
            col("setdatabase", PgType::Oid),
            col("setrole", PgType::Oid),
            col("setconfig", PgType::Array(oid::TEXT)),
        ],
    )
}
