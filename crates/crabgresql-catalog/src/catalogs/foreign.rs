//! The foreign-data (SQL/MED) family: wrappers, servers, user mappings, and the
//! tables defined over them.
//!
//! crabgresql has no `CREATE FOREIGN DATA WRAPPER`/`SERVER`/`TABLE`, so all five
//! are empty — PostgreSQL's answer as well for a database with no FDW installed.
//! `pg_class` here never reports `relkind = 'f'`, so nothing points into
//! `pg_foreign_table` and the two stay consistent.
//!
//! psql's `\d <table>` joins `pg_foreign_table` unconditionally (it has to, to
//! find out whether the relation *is* foreign), which is why the empty relation
//! still earns its file.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, oid};

use crate::cols::*;

pub(crate) fn pg_foreign_data_wrapper_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_foreign_data_wrapper",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("fdwname", PgType::Name),
            col("fdwowner", PgType::Oid),
            col("fdwhandler", PgType::Oid),
            col("fdwvalidator", PgType::Oid),
            col("fdwacl", ACLITEM_ARRAY),
            col("fdwoptions", PgType::Array(oid::TEXT)),
        ],
    )
}

pub(crate) fn pg_foreign_server_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_foreign_server",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("srvname", PgType::Name),
            col("srvowner", PgType::Oid),
            col("srvfdw", PgType::Oid),
            col("srvtype", PgType::Text),
            col("srvversion", PgType::Text),
            col("srvacl", ACLITEM_ARRAY),
            col("srvoptions", PgType::Array(oid::TEXT)),
        ],
    )
}

pub(crate) fn pg_foreign_table_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_foreign_table",
        "pg_catalog",
        vec![
            col("ftrelid", PgType::Oid),
            col("ftserver", PgType::Oid),
            col("ftoptions", PgType::Array(oid::TEXT)),
        ],
    )
}

pub(crate) fn pg_user_mapping_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_user_mapping",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("umuser", PgType::Oid),
            col("umserver", PgType::Oid),
            col("umoptions", PgType::Array(oid::TEXT)),
        ],
    )
}

pub(crate) fn pg_user_mappings_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_user_mappings",
        "pg_catalog",
        vec![
            col("umid", PgType::Oid),
            col("srvid", PgType::Oid),
            col("srvname", PgType::Name),
            col("umuser", PgType::Oid),
            col("usename", PgType::Name),
            col("umoptions", PgType::Array(oid::TEXT)),
        ],
    )
}
