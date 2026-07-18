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

use crate::{PG_CAST_ROWS, PG_TYPE_ROWS};

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
pub fn pg_class_rows(relations: &[(u32, &TableSchema)]) -> Vec<Vec<Value>> {
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
pub fn pg_attribute_rows(relations: &[(u32, &TableSchema)]) -> Vec<Vec<Value>> {
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
/// 99, `public` = 2200). `information_schema`'s OID is initdb-assigned and
/// varies, so it is omitted until its views land. Owners are reported as the
/// bootstrap superuser (10) for now.
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
