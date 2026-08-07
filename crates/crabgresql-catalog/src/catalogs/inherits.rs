//! `pg_inherits` and `pg_partitioned_table`: the inheritance edges.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;
use crabgresql_storage_api::PartitionStrategy;

/// `pg_catalog.pg_inherits` — the parent/child links of declarative partitions
/// and of table inheritance. One row per leaf partition, and one per
/// `INHERITS (...)` parent of an inheritance child.
pub(crate) fn pg_inherits_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_inherits",
        "pg_catalog",
        vec![
            col("inhrelid", PgType::Oid),
            col("inhparent", PgType::Oid),
            col("inhseqno", PgType::Int4),
            col("inhdetachpending", PgType::Bool),
        ],
    )
}

/// One `pg_inherits` row per parent link: a leaf partition has exactly one, an
/// inheritance child has one per `INHERITS (...)` entry. Both OIDs come from the
/// same positional assignment as `pg_class`, so the `inhrelid`/`inhparent` →
/// `pg_class.oid` joins line up.
///
/// `inhseqno` numbers a child's parents from 1 in declaration order. A partition
/// always gets 1, and only one of the two branches ever fires for a relation:
/// DDL refuses `INHERITS` together with `PARTITION OF` rather than letting one
/// clause quietly win.
pub(crate) fn pg_inherits_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let relations = cat.relation_oids();
    let parent_oid = |namespace: &str, name: &str| -> Option<u32> {
        relations
            .iter()
            .find(|(_, s)| s.namespace == namespace && s.name == name)
            .map(|(oid, _)| *oid)
    };
    let mut rows = Vec::new();
    for (oid, schema) in relations {
        if let Some(part) = &schema.partition_of
            && let Some(parent) = parent_oid(&part.parent_namespace, &part.parent_name)
        {
            rows.push(vec![
                Value::Oid(*oid),
                Value::Oid(parent),
                Value::Int4(1),
                Value::Bool(false),
            ]);
        }
        for (i, inherit) in schema.inherits.iter().enumerate() {
            let Some(parent) = parent_oid(&inherit.namespace, &inherit.name) else {
                continue;
            };
            rows.push(vec![
                Value::Oid(*oid),
                Value::Oid(parent),
                Value::Int4(i as i32 + 1),
                Value::Bool(false),
            ]);
        }
    }
    rows
}

/// `pg_catalog.pg_partitioned_table` — one row per partitioned (parent) table,
/// describing its partition key. A curated subset: `partdefid` (the default
/// partition) is always 0 and the class/collation/expression vectors are omitted.
pub(crate) fn pg_partitioned_table_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_partitioned_table",
        "pg_catalog",
        vec![
            col("partrelid", PgType::Oid),
            col("partstrat", CHARLIKE),
            col("partnatts", PgType::Int2),
            col("partdefid", PgType::Oid),
            // The 1-based key attnums.
            col("partattrs", INT2VECTOR),
        ],
    )
}

/// One `pg_partitioned_table` row per partitioned parent.
pub(crate) fn pg_partitioned_table_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let relations = cat.relation_oids();
    let mut rows = Vec::new();
    for (oid, schema) in relations {
        if let Some(scheme) = &schema.partition_scheme {
            let strat = match scheme.strategy {
                PartitionStrategy::Range => 'r',
            };
            let attrs = attnum_vector(scheme.key_columns.iter().copied());
            rows.push(vec![
                Value::Oid(*oid),
                chr(strat),
                Value::Int2(scheme.key_columns.len() as i16),
                Value::Oid(0),
                attrs,
            ]);
        }
    }
    rows
}
