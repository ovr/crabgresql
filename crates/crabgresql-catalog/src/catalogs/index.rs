//! `pg_index`: the index definitions behind `pg_class`'s index rows.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::catalogs::{attribute, opclass};
use crate::cols::*;
use crate::oids::{BTREE_AM_OID, HASH_AM_OID};
use crabgresql_storage_api::{IndexConstraint, IndexMethod};

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
            col("indisexclusion", PgType::Bool),
            col("indimmediate", PgType::Bool),
            col("indisclustered", PgType::Bool),
            col("indisvalid", PgType::Bool),
            col("indcheckxmin", PgType::Bool),
            col("indisready", PgType::Bool),
            col("indislive", PgType::Bool),
            col("indisreplident", PgType::Bool),
            col("indkey", INT2VECTOR),
            col("indcollation", OIDVECTOR),
            col("indclass", OIDVECTOR),
            col("indoption", INT2VECTOR),
            col("indexprs", NODE_TREE),
            col("indpred", NODE_TREE),
        ],
    )
}

pub(crate) fn pg_index_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let indexes = cat.index_oids();
    indexes
        .iter()
        .map(|index| {
            let am_oid = match index.metadata.method {
                IndexMethod::BTree => BTREE_AM_OID,
                IndexMethod::Hash => HASH_AM_OID,
            };
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
            // The collation each key sorts under, by the same rule
            // `pg_attribute.attcollation` reports — `0` for a key whose type has
            // no collation, which is most of them.
            let indcollation = oidvector(index.metadata.keys.iter().map(|key| {
                index
                    .table_schema
                    .columns
                    .get(key.column)
                    .map_or(0, attribute::attcollation_of)
            }));
            vec![
                Value::Oid(index.oid),
                Value::Oid(index.table_oid),
                Value::Int2(index.metadata.keys.len() as i16),
                Value::Int2(index.metadata.keys.len() as i16),
                Value::Bool(index.metadata.unique),
                Value::Bool(!index.metadata.nulls_distinct),
                Value::Bool(index.metadata.constraint == Some(IndexConstraint::PrimaryKey)),
                // indisexclusion: there are no EXCLUDE constraints.
                Value::Bool(false),
                Value::Bool(true),
                // indisclustered: nothing has been `CLUSTER`ed — PostgreSQL's
                // answer too until someone runs the command.
                Value::Bool(false),
                Value::Bool(true),
                // indcheckxmin: the index was never built by a transaction whose
                // rows a reader must recheck.
                Value::Bool(false),
                // indisready / indislive: an index here is usable the moment it
                // exists — there is no CONCURRENTLY that leaves one half-built.
                Value::Bool(true),
                Value::Bool(true),
                // indisreplident: no `REPLICA IDENTITY USING INDEX`.
                Value::Bool(false),
                indkey,
                indcollation,
                // DDL takes no explicit operator class, so every key reports
                // its type's default under the index's access method.
                oidvector(index.metadata.keys.iter().map(|key| {
                    index
                        .table_schema
                        .columns
                        .get(key.column)
                        .map_or(0, |column| opclass::default_opclass(am_oid, column.ty))
                })),
                indoption,
                // indexprs / indpred: DDL rejects expression and partial
                // indexes, so no index can carry either.
                Value::Null,
                Value::Null,
            ]
        })
        .collect()
}
