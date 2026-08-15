//! The `pg_publication` family: what logical replication would publish.
//!
//! crabgresql has no logical replication and no `CREATE PUBLICATION`, so all
//! four relations are empty — the same answer PostgreSQL gives for a cluster
//! nobody has published from.
//!
//! All four are here rather than only the one a client names because psql 18's
//! `\d <table>` reaches for `pg_publication`, `pg_publication_rel` and
//! `pg_publication_namespace` in a single query: serving two of the three would
//! leave that query failing exactly as it does today.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, oid};

use crate::cols::*;

pub(crate) fn pg_publication_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_publication",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("pubname", PgType::Name),
            col("pubowner", PgType::Oid),
            col("puballtables", PgType::Bool),
            col("pubinsert", PgType::Bool),
            col("pubupdate", PgType::Bool),
            col("pubdelete", PgType::Bool),
            col("pubtruncate", PgType::Bool),
            col("pubviaroot", PgType::Bool),
            col("pubgencols", CHARLIKE),
        ],
    )
}

pub(crate) fn pg_publication_rel_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_publication_rel",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("prpubid", PgType::Oid),
            col("prrelid", PgType::Oid),
            col("prqual", NODE_TREE),
            col("prattrs", INT2VECTOR),
        ],
    )
}

pub(crate) fn pg_publication_namespace_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_publication_namespace",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("pnpubid", PgType::Oid),
            col("pnnspid", PgType::Oid),
        ],
    )
}

pub(crate) fn pg_publication_tables_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_publication_tables",
        "pg_catalog",
        vec![
            col("pubname", PgType::Name),
            col("schemaname", PgType::Name),
            col("tablename", PgType::Name),
            col("attnames", PgType::Array(oid::NAME)),
            col("rowfilter", PgType::Text),
        ],
    )
}
