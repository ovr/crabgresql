//! `pg_sequence`: the parameters of every live sequence.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;

/// `pg_catalog.pg_sequence` — the definition of each user sequence, one row per
/// [`crate::RelKind::Sequence`] relation, keyed by its `pg_class` OID (`seqrelid`).
pub(crate) fn pg_sequence_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_sequence",
        "pg_catalog",
        vec![
            col("seqrelid", PgType::Oid),
            col("seqtypid", PgType::Oid),
            col("seqstart", PgType::Int8),
            col("seqincrement", PgType::Int8),
            col("seqmax", PgType::Int8),
            col("seqmin", PgType::Int8),
            col("seqcache", PgType::Int8),
            col("seqcycle", PgType::Bool),
        ],
    )
}

pub(crate) fn pg_sequence_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let sequences = &cat.sequence_entries();
    sequences
        .iter()
        .map(|(oid, s)| {
            vec![
                Value::Oid(*oid),
                Value::Oid(s.type_oid),
                Value::Int8(s.start),
                Value::Int8(s.increment),
                Value::Int8(s.max),
                Value::Int8(s.min),
                Value::Int8(s.cache),
                Value::Bool(s.cycle),
            ]
        })
        .collect()
}
