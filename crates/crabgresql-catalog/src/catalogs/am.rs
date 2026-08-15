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
/// PostgreSQL 18.4, not from upstream source. `pg_am.dat` *is* vendored, but
/// only for its `descr` fields (see [`crate::catalogs::description`]): seven
/// rows do not justify codegen, and two of the nine below — crabgresql's own
/// `parquet` and `buffer` — have no upstream entry to be generated from.
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

/// Every access method this build publishes, as `(oid, amname, amhandler,
/// amtype)`. `amtype` is `'t'` for a table access method and `'i'` for an index
/// one.
///
/// A list rather than a literal inside [`pg_am_rows`] because
/// [`crate::catalogs::description`] filters `pg_am.dat`'s descriptions against
/// it.
pub(crate) const BUILTIN_AMS: &[(u32, &str, &str, char)] = &[
    (HEAP_AM_OID, "heap", "heap_tableam_handler", 't'),
    (BTREE_AM_OID, "btree", "bthandler", 'i'),
    (HASH_AM_OID, "hash", "hashhandler", 'i'),
    (783, "gist", "gisthandler", 'i'),
    (2742, "gin", "ginhandler", 'i'),
    (3580, "brin", "brinhandler", 'i'),
    (4000, "spgist", "spghandler", 'i'),
    (PARQUET_AM_OID, "parquet", "parquet_tableam_handler", 't'),
    (BUFFER_AM_OID, "buffer", "buffer_tableam_handler", 't'),
];

/// The fixed `pg_am` rows.
pub(crate) fn pg_am_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    BUILTIN_AMS
        .iter()
        .map(|(oid, amname, amhandler, amtype)| {
            vec![
                Value::Oid(*oid),
                Value::Text((*amname).to_string()),
                regproc_by_name(amhandler),
                chr(*amtype),
            ]
        })
        .collect()
}
