//! `pg_index`: the index definitions behind `pg_class`'s index rows.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;
use crabgresql_storage_api::IndexConstraint;

pub(crate) fn pg_index_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_index",
        "pg_catalog",
        vec![
            col("indexrelid", PgType::Oid),
            col("indrelid", PgType::Oid),
            col("indnatts", PgType::Int2),
            col("indnkeyatts", PgType::Int2),
            col("indisunique", PgType::Bool),
            col("indnullsnotdistinct", PgType::Bool),
            col("indisprimary", PgType::Bool),
            col("indimmediate", PgType::Bool),
            col("indisvalid", PgType::Bool),
            col("indkey", INT2VECTOR),
            col("indoption", INT2VECTOR),
        ],
    )
}

pub(crate) fn pg_index_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let indexes = cat.index_oids();
    indexes
        .iter()
        .map(|index| {
            // 1-based key attnums, as PG's `indkey` holds.
            let indkey = attnum_vector(index.metadata.keys.iter().map(|key| key.column));
            let indoption = int2vector(index.metadata.keys.iter().map(|key| {
                let mut option = 0;
                if key.descending {
                    option |= 1;
                }
                if key.nulls_first {
                    option |= 2;
                }
                option
            }));
            vec![
                Value::Oid(index.oid),
                Value::Oid(index.table_oid),
                Value::Int2(index.metadata.keys.len() as i16),
                Value::Int2(index.metadata.keys.len() as i16),
                Value::Bool(index.metadata.unique),
                Value::Bool(!index.metadata.nulls_distinct),
                Value::Bool(index.metadata.constraint == Some(IndexConstraint::PrimaryKey)),
                Value::Bool(true),
                Value::Bool(true),
                indkey,
                indoption,
            ]
        })
        .collect()
}
