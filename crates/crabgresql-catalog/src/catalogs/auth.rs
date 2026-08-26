//! The role catalogs: `pg_authid` and the five relations derived from it.
//!
//! All six are built from the one role list [`SystemCatalog::roles`] answers,
//! rather than each writing its own literals, so they cannot drift apart —
//! `pg_user` must always show exactly the `pg_authid` rows that can log in, and
//! `pg_group` exactly the ones that cannot.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;
use crate::source::CatalogRole;

/// `pg_catalog.pg_authid` — the role catalog, and the only relation here that
/// shows the stored password. In PostgreSQL that is what makes it the
/// superuser-only one; nothing restricts reads here yet.
pub(crate) fn pg_authid_schema() -> TableSchema {
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

pub(crate) fn pg_authid_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    cat.roles()
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Text(r.name.clone()),
                Value::Bool(r.superuser),
                Value::Bool(r.inherit),
                Value::Bool(r.createrole),
                Value::Bool(r.createdb),
                Value::Bool(r.canlogin),
                Value::Bool(r.replication),
                Value::Bool(r.bypassrls),
                Value::Int4(r.connlimit),
                password(r),
                valid_until(r),
            ]
        })
        .collect()
}

/// `pg_catalog.pg_roles` — `pg_authid` with the password masked. Note the
/// column order is PostgreSQL's own and differs from `pg_authid`: `rolbypassrls`
/// comes after `rolvaliduntil`, and `oid` is last.
pub(crate) fn pg_roles_schema() -> TableSchema {
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

pub(crate) fn pg_roles_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    cat.roles()
        .iter()
        .map(|r| {
            vec![
                Value::Text(r.name.clone()),
                Value::Bool(r.superuser),
                Value::Bool(r.inherit),
                Value::Bool(r.createrole),
                Value::Bool(r.createdb),
                Value::Bool(r.canlogin),
                Value::Bool(r.replication),
                Value::Int4(r.connlimit),
                Value::Text(MASKED_PASSWORD.to_string()),
                valid_until(r),
                Value::Bool(r.bypassrls),
                config(r),
                Value::Oid(r.oid),
            ]
        })
        .collect()
}

/// What `pg_roles`/`pg_user` print instead of a password hash. PostgreSQL emits
/// this literal rather than NULL, so a client cannot tell a role with no
/// password from one whose hash it may not read.
const MASKED_PASSWORD: &str = "********";

/// The verifier as `pg_authid`/`pg_shadow` show it: NULL for a role with no
/// password.
fn password(role: &CatalogRole) -> Value {
    match &role.password {
        Some(password) => Value::Text(password.clone()),
        None => Value::Null,
    }
}

fn valid_until(role: &CatalogRole) -> Value {
    match role.valid_until {
        Some(micros) => Value::TimestampTz(micros),
        None => Value::Null,
    }
}

/// `rolconfig`/`useconfig`: NULL rather than an empty array for a role with no
/// per-role settings, which is what PostgreSQL shows.
fn config(role: &CatalogRole) -> Value {
    if role.config.is_empty() {
        return Value::Null;
    }
    Value::array_1d(
        PgType::Text,
        role.config
            .iter()
            .map(|entry| Value::Text(entry.clone()))
            .collect(),
    )
}

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
pub(crate) fn pg_user_schema() -> TableSchema {
    pg_user_columns("pg_user")
}

/// `pg_catalog.pg_shadow` — `pg_user` with the password column unmasked. The
/// two differ only there.
pub(crate) fn pg_shadow_schema() -> TableSchema {
    pg_user_columns("pg_shadow")
}

fn user_rows(cat: &SystemCatalog, mask: bool) -> Vec<Vec<Value>> {
    cat.roles()
        .iter()
        .filter(|r| r.canlogin)
        .map(|r| {
            vec![
                Value::Text(r.name.clone()),
                Value::Oid(r.oid),
                Value::Bool(r.createdb),
                Value::Bool(r.superuser),
                Value::Bool(r.replication),
                Value::Bool(r.bypassrls),
                if mask {
                    Value::Text(MASKED_PASSWORD.to_string())
                } else {
                    password(r)
                },
                valid_until(r),
                config(r),
            ]
        })
        .collect()
}

pub(crate) fn pg_user_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    user_rows(cat, true)
}

pub(crate) fn pg_shadow_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    user_rows(cat, false)
}

/// `pg_catalog.pg_group` — the roles that cannot log in, with their members.
///
/// A stock PostgreSQL 18 shows more rows than a fresh crabgresql cluster: its
/// `initdb` creates the predefined `pg_read_all_data`/`pg_monitor`/… roles,
/// which this build does not.
pub(crate) fn pg_group_schema() -> TableSchema {
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

pub(crate) fn pg_group_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let members = cat.role_members();
    cat.roles()
        .iter()
        .filter(|r| !r.canlogin)
        .map(|r| {
            vec![
                Value::Text(r.name.clone()),
                Value::Oid(r.oid),
                Value::array_1d(
                    PgType::Oid,
                    members
                        .iter()
                        .filter(|m| m.role == r.oid)
                        .map(|m| Value::Oid(m.member))
                        .collect(),
                ),
            ]
        })
        .collect()
}

/// `pg_catalog.pg_auth_members` — role membership, one row per
/// `GRANT <role> TO <role>`.
pub(crate) fn pg_auth_members_schema() -> TableSchema {
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

pub(crate) fn pg_auth_members_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    cat.role_members()
        .iter()
        .map(|m| {
            vec![
                Value::Oid(m.oid),
                Value::Oid(m.role),
                Value::Oid(m.member),
                Value::Oid(m.grantor),
                Value::Bool(m.admin_option),
                Value::Bool(m.inherit_option),
                Value::Bool(m.set_option),
            ]
        })
        .collect()
}
