//! `pg_am`: the access methods, including crabgresql's own two.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;
use crate::oids::*;

/// `pg_catalog.pg_am` — the access methods. PostgreSQL lists the methods its
/// build actually registered; crabgresql adds its managed `parquet` and `buffer`
/// table methods alongside PostgreSQL's built-ins so a client that
/// joins `pg_class.relam` or reads `pg_am` sees the shape it expects.
///
/// Fidelity note (`AGENTS.md`): these rows are transcribed from the output of
/// `SELECT oid, amname, amhandler, amtype FROM pg_am ORDER BY oid` on a stock
/// PostgreSQL 18.4, not from upstream source. No `pg_am.dat` is vendored —
/// seven rows do not justify codegen.
pub(crate) fn pg_am_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_am",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("amname", PgType::Name),
            col("amhandler", REGPROC),
            col("amtype", CHARLIKE),
        ],
    )
}

/// The fixed `pg_am` rows. `amtype` is `'t'` for a table access method and
/// `'i'` for an index one.
pub(crate) fn pg_am_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let row = |oid: u32, amname: &str, amhandler: &str, amtype: char| {
        vec![
            Value::Oid(oid),
            Value::Text(amname.to_string()),
            regproc_by_name(amhandler),
            chr(amtype),
        ]
    };
    vec![
        row(HEAP_AM_OID, "heap", "heap_tableam_handler", 't'),
        row(BTREE_AM_OID, "btree", "bthandler", 'i'),
        row(HASH_AM_OID, "hash", "hashhandler", 'i'),
        row(783, "gist", "gisthandler", 'i'),
        row(2742, "gin", "ginhandler", 'i'),
        row(3580, "brin", "brinhandler", 'i'),
        row(4000, "spgist", "spghandler", 'i'),
        row(PARQUET_AM_OID, "parquet", "parquet_tableam_handler", 't'),
        row(BUFFER_AM_OID, "buffer", "buffer_tableam_handler", 't'),
    ]
}
