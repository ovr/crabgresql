//! The role catalogs: `pg_authid` and the five views over it.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;
use crate::oids::*;

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
    let owner = cat.owner();
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
    let owner = cat.owner();
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
                // TODO: `ALTER ROLE … SET`, which is what would put a per-role
                // GUC here; with no statement that sets one, `rolconfig` (and
                // `useconfig` below) can only be NULL.
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
pub(crate) fn pg_user_schema() -> TableSchema {
    pg_user_columns("pg_user")
}

/// `pg_catalog.pg_shadow` — `pg_user` with the password column unmasked. The
/// two differ only there, and here both are as truthful as each other: nothing
/// stores a password.
pub(crate) fn pg_shadow_schema() -> TableSchema {
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

pub(crate) fn pg_user_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let owner = cat.owner();
    user_rows(owner, Value::Text(MASKED_PASSWORD.to_string()))
}

pub(crate) fn pg_shadow_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let owner = cat.owner();
    user_rows(owner, Value::Null)
}

/// `pg_catalog.pg_group` — the roles that cannot log in, with their members.
///
/// Empty here, and empty as a *consequence*: the one role crabgresql has is a
/// login role. A stock PostgreSQL 18 shows 16 rows because `initdb` creates the
/// predefined `pg_read_all_data`/`pg_monitor`/… roles, which this build does not.
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
    let owner = cat.owner();
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

pub(crate) fn pg_auth_members_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    Vec::new()
}
