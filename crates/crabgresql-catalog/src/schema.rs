//! `TableSchema` definitions and row builders for the supported `pg_catalog`
//! relations.
//!
//! The column list for each relation follows PostgreSQL's column *names* and
//! order for the frequently-queried leading columns. Fidelity deviations are
//! deliberate and documented (see the crate root): catalog-only types we do not
//! model yet are represented pragmatically — `"char"` columns as `text`, and
//! `regproc` I/O columns as the referenced function's `text` name (which is what
//! PostgreSQL's `regprocout` prints anyway).

use crabgresql_storage_api::{Column, TableSchema};
use crabgresql_types::{PgType, Value};

use crate::{CatalogRelation, PG_CAST_ROWS, PG_TYPE_ROWS};

/// A `"char"`/`regproc` column: a single- or short-name catalog column we render
/// as `text` for now. Kept as a named alias so the deviation is greppable.
const CHARLIKE: PgType = PgType::Text;

fn col(name: &str, ty: PgType) -> Column {
    Column::new(name, ty)
}

/// `pg_catalog.pg_type` — a curated, PG-ordered subset of the columns clients
/// query. Trailing rarely-read columns (`typmodin`, `typnotnull`, `typbasetype`,
/// `typdefault`, `typacl`, …) are omitted for now.
pub fn pg_type_schema() -> TableSchema {
    TableSchema {
        name: "pg_type".to_string(),
        columns: vec![
            col("oid", PgType::Oid),
            col("typname", PgType::Name),
            col("typnamespace", PgType::Oid),
            col("typowner", PgType::Oid),
            col("typlen", PgType::Int2),
            col("typbyval", PgType::Bool),
            col("typtype", CHARLIKE),
            col("typcategory", CHARLIKE),
            col("typispreferred", PgType::Bool),
            col("typisdefined", PgType::Bool),
            col("typdelim", CHARLIKE),
            col("typrelid", PgType::Oid),
            col("typelem", PgType::Oid),
            col("typarray", PgType::Oid),
            col("typinput", CHARLIKE),
            col("typoutput", CHARLIKE),
            col("typreceive", CHARLIKE),
            col("typsend", CHARLIKE),
            col("typalign", CHARLIKE),
            col("typstorage", CHARLIKE),
        ],
    }
}

/// The built-in `pg_type` rows generated from `pg_type.dat`. Callers append any
/// user-defined-type rows (a later slice) after these.
pub fn pg_type_builtin_rows() -> Vec<Vec<Value>> {
    PG_TYPE_ROWS
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Text(r.typname.to_string()),
                Value::Oid(r.typnamespace),
                Value::Oid(r.typowner),
                Value::Int2(r.typlen),
                Value::Bool(r.typbyval),
                Value::Text(r.typtype.to_string()),
                Value::Text(r.typcategory.to_string()),
                Value::Bool(r.typispreferred),
                Value::Bool(r.typisdefined),
                Value::Text(r.typdelim.to_string()),
                Value::Oid(r.typrelid),
                Value::Oid(r.typelem),
                Value::Oid(r.typarray),
                Value::Text(r.typinput.to_string()),
                Value::Text(r.typoutput.to_string()),
                Value::Text(r.typreceive.to_string()),
                Value::Text(r.typsend.to_string()),
                Value::Text(r.typalign.to_string()),
                Value::Text(r.typstorage.to_string()),
            ]
        })
        .collect()
}

/// `pg_catalog.pg_cast` — the built-in casts between types crabgresql exposes.
pub fn pg_cast_schema() -> TableSchema {
    TableSchema {
        name: "pg_cast".to_string(),
        columns: vec![
            col("oid", PgType::Oid),
            col("castsource", PgType::Oid),
            col("casttarget", PgType::Oid),
            // regproc; rendered as the upstream function reference text for now.
            col("castfunc", PgType::Text),
            col("castcontext", CHARLIKE),
            col("castmethod", CHARLIKE),
        ],
    }
}

/// The built-in `pg_cast` rows generated from `pg_cast.dat` (restricted to casts
/// between exposed types).
pub fn pg_cast_rows() -> Vec<Vec<Value>> {
    PG_CAST_ROWS
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Oid(r.castsource),
                Value::Oid(r.casttarget),
                Value::Text(r.castfunc.to_string()),
                Value::Text(r.castcontext.to_string()),
                Value::Text(r.castmethod.to_string()),
            ]
        })
        .collect()
}

/// `pg_catalog.pg_namespace` — the schemas visible on a fresh cluster.
pub fn pg_namespace_schema() -> TableSchema {
    TableSchema {
        name: "pg_namespace".to_string(),
        columns: vec![
            col("oid", PgType::Oid),
            col("nspname", PgType::Name),
            col("nspowner", PgType::Oid),
            // aclitem[]; represented as text and always NULL (default ACL) here.
            col("nspacl", PgType::Text),
        ],
    }
}

/// OID assigned to the heap access method (`pg_am` row `heap` = 2). Reported for
/// every user relation's `relam`.
const HEAP_AM_OID: u32 = 2;

/// `pg_catalog.pg_class` — a curated subset of columns for user relations.
/// Index/partition/stats columns beyond this set are omitted for now.
pub fn pg_class_schema() -> TableSchema {
    TableSchema {
        name: "pg_class".to_string(),
        columns: vec![
            col("oid", PgType::Oid),
            col("relname", PgType::Name),
            col("relnamespace", PgType::Oid),
            col("reltype", PgType::Oid),
            col("relowner", PgType::Oid),
            col("relam", PgType::Oid),
            col("relnatts", PgType::Int2),
            col("relhasindex", PgType::Bool),
            col("relpersistence", CHARLIKE),
            col("relkind", CHARLIKE),
        ],
    }
}

/// Build `pg_class` rows from `(oid, schema)` pairs. All user relations live in
/// `public` (namespace 2200), are ordinary heaps (`relkind = 'r'`, `relam = 2`)
/// and permanent (`relpersistence = 'p'`); the synthetic OIDs are stable within
/// one catalog snapshot so a join to `pg_attribute.attrelid` lines up.
pub fn pg_class_rows(relations: &[(u32, TableSchema)]) -> Vec<Vec<Value>> {
    relations
        .iter()
        .map(|(oid, schema)| {
            vec![
                Value::Oid(*oid),
                Value::Text(schema.name.clone()),
                Value::Oid(2200),
                Value::Oid(0),
                Value::Oid(10),
                Value::Oid(HEAP_AM_OID),
                Value::Int2(schema.columns.len() as i16),
                Value::Bool(false),
                Value::Text("p".to_string()),
                Value::Text("r".to_string()),
            ]
        })
        .collect()
}

/// `pg_catalog.pg_attribute` — a curated subset of columns for user relations'
/// columns. System (negative `attnum`) columns are not emitted yet.
pub fn pg_attribute_schema() -> TableSchema {
    TableSchema {
        name: "pg_attribute".to_string(),
        columns: vec![
            col("attrelid", PgType::Oid),
            col("attname", PgType::Name),
            col("atttypid", PgType::Oid),
            col("attlen", PgType::Int2),
            col("attnum", PgType::Int2),
            col("atttypmod", PgType::Int4),
            col("attnotnull", PgType::Bool),
            col("attisdropped", PgType::Bool),
        ],
    }
}

/// Build `pg_attribute` rows: one per column of each relation, `attnum` 1-based
/// (user columns only), typed from the column's `PgType` (`atttypid`/`attlen`).
pub fn pg_attribute_rows(relations: &[(u32, TableSchema)]) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for (oid, schema) in relations {
        for (i, c) in schema.columns.iter().enumerate() {
            rows.push(vec![
                Value::Oid(*oid),
                Value::Text(c.name.clone()),
                Value::Oid(c.ty.oid()),
                Value::Int2(c.ty.typlen()),
                Value::Int2((i + 1) as i16),
                Value::Int4(c.typmod),
                Value::Bool(false),
                Value::Bool(false),
            ]);
        }
    }
    rows
}

/// Fixed `pg_namespace` rows: the reserved catalog/toast schemas plus `public`.
/// OIDs match PostgreSQL's stable assignments (`pg_catalog` = 11, `pg_toast` =
/// 99, `public` = 2200). `information_schema` has an initdb-assigned OID, so
/// it remains absent here; its named discovery surface lives in
/// `information_schema.schemata`. Owners are reported as the bootstrap
/// superuser (10) for now.
pub fn pg_namespace_rows() -> Vec<Vec<Value>> {
    let row = |oid: u32, name: &str| {
        vec![
            Value::Oid(oid),
            Value::Text(name.to_string()),
            Value::Oid(10),
            Value::Null,
        ]
    };
    vec![
        row(11, "pg_catalog"),
        row(99, "pg_toast"),
        row(2200, "public"),
    ]
}

/// `information_schema.schemata`. Information-schema domains are represented
/// as text until the engine supports domains over the built-in types.
pub fn information_schema_schemata_schema() -> TableSchema {
    TableSchema {
        name: "schemata".to_string(),
        columns: vec![
            col("catalog_name", PgType::Text),
            col("schema_name", PgType::Text),
            col("schema_owner", PgType::Text),
            col("default_character_set_catalog", PgType::Text),
            col("default_character_set_schema", PgType::Text),
            col("default_character_set_name", PgType::Text),
            col("sql_path", PgType::Text),
        ],
    }
}

pub fn information_schema_schemata_rows(
    database: &str,
    owner: &str,
    relations: &[CatalogRelation],
) -> Vec<Vec<Value>> {
    let mut namespaces = vec![
        "information_schema".to_string(),
        "pg_catalog".to_string(),
        "pg_toast".to_string(),
        "public".to_string(),
    ];
    for relation in relations {
        if !namespaces.contains(&relation.namespace) {
            namespaces.push(relation.namespace.clone());
        }
    }
    namespaces.sort();
    namespaces
        .into_iter()
        .map(|namespace| {
            vec![
                Value::Text(database.to_string()),
                Value::Text(namespace),
                Value::Text(owner.to_string()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]
        })
        .collect()
}

/// `information_schema.tables` for represented user relations. Catalog and
/// information-schema implementation relations are deliberately not invented:
/// their complete PostgreSQL metadata is not modeled yet.
pub fn information_schema_tables_schema() -> TableSchema {
    TableSchema {
        name: "tables".to_string(),
        columns: vec![
            col("table_catalog", PgType::Text),
            col("table_schema", PgType::Text),
            col("table_name", PgType::Text),
            col("table_type", PgType::Text),
            col("self_referencing_column_name", PgType::Text),
            col("reference_generation", PgType::Text),
            col("user_defined_type_catalog", PgType::Text),
            col("user_defined_type_schema", PgType::Text),
            col("user_defined_type_name", PgType::Text),
            col("is_insertable_into", PgType::Text),
            col("is_typed", PgType::Text),
            col("commit_action", PgType::Text),
        ],
    }
}

pub fn information_schema_tables_rows(
    database: &str,
    relations: &[CatalogRelation],
) -> Vec<Vec<Value>> {
    relations
        .iter()
        .map(|relation| {
            vec![
                Value::Text(database.to_string()),
                Value::Text(relation.namespace.clone()),
                Value::Text(relation.schema.name.clone()),
                Value::Text(
                    if relation.temporary {
                        "LOCAL TEMPORARY"
                    } else {
                        "BASE TABLE"
                    }
                    .to_string(),
                ),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Text("YES".to_string()),
                Value::Text("NO".to_string()),
                Value::Null,
            ]
        })
        .collect()
}

/// `information_schema.columns`, including all PostgreSQL-documented columns.
pub fn information_schema_columns_schema() -> TableSchema {
    let text = PgType::Text;
    let cardinal = PgType::Int4;
    TableSchema {
        name: "columns".to_string(),
        columns: vec![
            col("table_catalog", text),
            col("table_schema", text),
            col("table_name", text),
            col("column_name", text),
            col("ordinal_position", cardinal),
            col("column_default", text),
            col("is_nullable", text),
            col("data_type", text),
            col("character_maximum_length", cardinal),
            col("character_octet_length", cardinal),
            col("numeric_precision", cardinal),
            col("numeric_precision_radix", cardinal),
            col("numeric_scale", cardinal),
            col("datetime_precision", cardinal),
            col("interval_type", text),
            col("interval_precision", cardinal),
            col("character_set_catalog", text),
            col("character_set_schema", text),
            col("character_set_name", text),
            col("collation_catalog", text),
            col("collation_schema", text),
            col("collation_name", text),
            col("domain_catalog", text),
            col("domain_schema", text),
            col("domain_name", text),
            col("udt_catalog", text),
            col("udt_schema", text),
            col("udt_name", text),
            col("scope_catalog", text),
            col("scope_schema", text),
            col("scope_name", text),
            col("maximum_cardinality", cardinal),
            col("dtd_identifier", text),
            col("is_self_referencing", text),
            col("is_identity", text),
            col("identity_generation", text),
            col("identity_start", text),
            col("identity_increment", text),
            col("identity_maximum", text),
            col("identity_minimum", text),
            col("identity_cycle", text),
            col("is_generated", text),
            col("generation_expression", text),
            col("is_updatable", text),
        ],
    }
}

pub fn information_schema_columns_rows(
    database: &str,
    relations: &[CatalogRelation],
) -> Vec<Vec<Value>> {
    relations
        .iter()
        .flat_map(|relation| {
            relation
                .schema
                .columns
                .iter()
                .enumerate()
                .map(move |(index, column)| {
                    let (character_length, character_octets) = match column.ty {
                        PgType::Varchar | PgType::Bpchar if column.typmod >= 0 => {
                            (Value::Int4(column.typmod), Value::Int4(column.typmod * 4))
                        }
                        PgType::Bit | PgType::Varbit if column.typmod >= 0 => {
                            (Value::Int4(column.typmod), Value::Null)
                        }
                        _ => (Value::Null, Value::Null),
                    };
                    let (precision, radix) = match column.ty {
                        PgType::Int2 => (Value::Int4(16), Value::Int4(2)),
                        PgType::Int4 => (Value::Int4(32), Value::Int4(2)),
                        PgType::Int8 => (Value::Int4(64), Value::Int4(2)),
                        PgType::Float4 => (Value::Int4(24), Value::Int4(2)),
                        PgType::Float8 => (Value::Int4(53), Value::Int4(2)),
                        _ => (Value::Null, Value::Null),
                    };
                    let datetime_precision = match column.ty {
                        PgType::Time
                        | PgType::TimeTz
                        | PgType::Timestamp
                        | PgType::TimestampTz
                        | PgType::Interval => Value::Int4(6),
                        _ => Value::Null,
                    };
                    vec![
                        Value::Text(database.to_string()),
                        Value::Text(relation.namespace.clone()),
                        Value::Text(relation.schema.name.clone()),
                        Value::Text(column.name.clone()),
                        Value::Int4((index + 1) as i32),
                        Value::Null,
                        Value::Text("YES".to_string()),
                        Value::Text(column.ty.name().to_string()),
                        character_length,
                        character_octets,
                        precision,
                        radix,
                        Value::Null,
                        datetime_precision,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Text(database.to_string()),
                        Value::Text("pg_catalog".to_string()),
                        Value::Text(column.ty.typname().to_string()),
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Text((index + 1).to_string()),
                        Value::Text("NO".to_string()),
                        Value::Text("NO".to_string()),
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Text("NEVER".to_string()),
                        Value::Null,
                        Value::Text("YES".to_string()),
                    ]
                })
        })
        .collect()
}
