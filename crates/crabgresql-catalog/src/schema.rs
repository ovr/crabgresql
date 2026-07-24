//! `TableSchema` definitions and row builders for the supported `pg_catalog`
//! relations.
//!
//! The column list for each relation follows PostgreSQL's column *names* and
//! order for the frequently-queried leading columns. Fidelity deviations are
//! deliberate and documented (see the crate root): catalog-only types we do not
//! model yet are represented pragmatically — `"char"` columns as `text`, and
//! `regproc` I/O columns as the referenced function's `text` name (which is what
//! PostgreSQL's `regprocout` prints anyway).

use std::collections::HashMap;

use crabgresql_storage_api::{
    Column, IndexConstraint, IndexMethod, PartitionStrategy, TableSchema,
};
use crabgresql_types::{PgType, Value};

use crate::{
    CatalogIndex, CatalogRelation, CatalogSequence, CatalogUserType, PG_CAST_ROWS, PG_TYPE_ROWS,
    RelKind,
};

/// Synthetic OID base for `pg_enum` rows (one per enum label). Chosen above the
/// built-in ranges so a per-label OID never collides with a type/relation OID.
const FIRST_ENUM_OID: u32 = 0x8000_0000;

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
    TableSchema::in_namespace(
        "pg_type",
        "pg_catalog",
        vec![
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
            col("typcollation", PgType::Oid),
        ],
    )
}

/// `pg_catalog.pg_collation` — the collations this build ships. `collversion`
/// is omitted: it exists so PostgreSQL can warn when the underlying OS locale
/// data changes under an index, and the ICU data here is compiled in, so there
/// is no external version to drift from.
pub fn pg_collation_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_collation",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("collname", PgType::Name),
            col("collnamespace", PgType::Oid),
            col("collowner", PgType::Oid),
            col("collprovider", CHARLIKE),
            col("collisdeterministic", PgType::Bool),
            col("collencoding", PgType::Int4),
            col("collcollate", PgType::Text),
            col("collctype", PgType::Text),
            col("colllocale", PgType::Text),
        ],
    )
}

/// The `pg_collation` rows, one per collation in the shared registry — the same
/// list [`crabgresql_types::collation::compare_str`] orders strings by, so what
/// the catalog advertises and what queries actually do cannot drift.
pub fn pg_collation_rows() -> Vec<Vec<Value>> {
    crabgresql_types::collation::COLLATIONS
        .iter()
        .map(|c| {
            let opt_text = |s: Option<&str>| s.map_or(Value::Null, |s| Value::Text(s.to_string()));
            vec![
                Value::Oid(c.oid),
                Value::Text(c.name.to_string()),
                // Every collation lives in pg_catalog (11), owned by the
                // bootstrap superuser (10).
                Value::Oid(11),
                Value::Oid(10),
                Value::Text(c.provider.as_char().to_string()),
                Value::Bool(c.deterministic),
                Value::Int4(c.encoding),
                opt_text(c.libc_locale),
                opt_text(c.libc_locale),
                opt_text(c.locale),
            ]
        })
        .collect()
}

/// `typcollation`: the collation of values of `oid`'s type, or `0` when the type
/// is not collatable. An OID this build does not model has no collation.
fn typcollation_of(oid: u32) -> u32 {
    PgType::from_oid(oid).map_or(0, crabgresql_types::collation::type_collation)
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
                Value::Oid(typcollation_of(r.oid)),
            ]
        })
        .collect()
}

/// The `pg_type` rows for user-defined enum types, appended after
/// [`pg_type_builtin_rows`]. Only enums are reflected (`typtype = 'e'`); other
/// `CREATE TYPE` shapes are not surfaced here yet. Column order matches
/// [`pg_type_schema`].
pub fn pg_type_user_rows(user_types: &[CatalogUserType]) -> Vec<Vec<Value>> {
    user_types
        .iter()
        .filter(|t| t.enum_labels.is_some())
        .map(|t| {
            vec![
                Value::Oid(t.oid),
                Value::Text(t.name.clone()),
                // pg_catalog namespace / bootstrap superuser, as elsewhere.
                Value::Oid(11),
                Value::Oid(10),
                // Enums are a fixed 4-byte, pass-by-value, OID-backed type.
                Value::Int2(4),
                Value::Bool(true),
                Value::Text("e".to_string()),
                Value::Text("E".to_string()),
                Value::Bool(false),
                Value::Bool(true),
                Value::Text(",".to_string()),
                Value::Oid(0),
                Value::Oid(0),
                Value::Oid(0),
                Value::Text("enum_in".to_string()),
                Value::Text("enum_out".to_string()),
                Value::Text("enum_recv".to_string()),
                Value::Text("enum_send".to_string()),
                Value::Text("i".to_string()),
                Value::Text("p".to_string()),
                // An enum is not collatable.
                Value::Oid(0),
            ]
        })
        .collect()
}

/// `pg_catalog.pg_enum` — one row per (enum type, label). `enumsortorder` is the
/// 1-based definition position (PG stores a float4 so labels can be inserted
/// between existing ones; a freshly created enum uses 1, 2, 3, …).
pub fn pg_enum_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_enum",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("enumtypid", PgType::Oid),
            col("enumsortorder", PgType::Float4),
            col("enumlabel", PgType::Name),
        ],
    )
}

/// The `pg_enum` rows for every user-defined enum type, in a stable order (by
/// type OID, then definition order). Per-label OIDs are synthetic.
pub fn pg_enum_rows(user_types: &[CatalogUserType]) -> Vec<Vec<Value>> {
    let mut enums: Vec<&CatalogUserType> = user_types
        .iter()
        .filter(|t| t.enum_labels.is_some())
        .collect();
    enums.sort_by_key(|t| t.oid);
    let mut rows = Vec::new();
    let mut next_oid = FIRST_ENUM_OID;
    for t in enums {
        let labels = t.enum_labels.as_deref().unwrap_or_default();
        for (i, label) in labels.iter().enumerate() {
            rows.push(vec![
                Value::Oid(next_oid),
                Value::Oid(t.oid),
                Value::Float4((i + 1) as f32),
                Value::Text(label.clone()),
            ]);
            next_oid += 1;
        }
    }
    rows
}

/// `pg_catalog.pg_cast` — the built-in casts between types crabgresql exposes.
pub fn pg_cast_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_cast",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("castsource", PgType::Oid),
            col("casttarget", PgType::Oid),
            // regproc; rendered as the upstream function reference text for now.
            col("castfunc", PgType::Text),
            col("castcontext", CHARLIKE),
            col("castmethod", CHARLIKE),
        ],
    )
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

/// `pg_catalog.pg_sequence` — the definition of each user sequence, one row per
/// [`RelKind::Sequence`] relation, keyed by its `pg_class` OID (`seqrelid`).
pub fn pg_sequence_schema() -> TableSchema {
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

pub fn pg_sequence_rows(sequences: &[(u32, CatalogSequence)]) -> Vec<Vec<Value>> {
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

/// `pg_catalog.pg_namespace` — the schemas visible on a fresh cluster.
pub fn pg_namespace_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_namespace",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("nspname", PgType::Name),
            col("nspowner", PgType::Oid),
            // aclitem[]; represented as text and always NULL (default ACL) here.
            col("nspacl", PgType::Text),
        ],
    )
}

/// OID assigned to the heap access method (`pg_am` row `heap` = 2). Reported for
/// every user relation's `relam`.
const HEAP_AM_OID: u32 = 2;

/// `pg_catalog.pg_class` — a curated subset of columns for user relations.
/// Index/partition/stats columns beyond this set are omitted for now.
pub fn pg_class_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_class",
        "pg_catalog",
        vec![
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
            col("relispartition", PgType::Bool),
        ],
    )
}

/// Build `pg_class` rows from `(oid, schema)` pairs paired with their kinds.
/// `relpersistence` comes from each schema (`'p'` permanent, `'u'` unlogged,
/// `'t'` temporary — the memory tables); a table is an ordinary heap (`relkind = 'r'`,
/// `relam = 2`) while a view has no storage access method (`relkind = 'v'`,
/// `relam = 0`). The synthetic OIDs are stable within one catalog snapshot so a
/// join to `pg_attribute.attrelid` lines up.
pub fn pg_class_rows(
    relations: &[(u32, TableSchema)],
    kinds: &[RelKind],
    indexes: &[CatalogIndex],
    namespace_oids: &HashMap<String, u32>,
) -> Vec<Vec<Value>> {
    // Resolve a relation's namespace OID, defaulting to `public` (2200) for any
    // namespace not in the map (should not happen for a live relation).
    let nsp_oid = |namespace: &str| namespace_oids.get(namespace).copied().unwrap_or(2200);
    let mut rows: Vec<Vec<Value>> = relations
        .iter()
        .zip(kinds)
        .map(|((oid, schema), kind)| {
            // A partitioned parent has no access method (`relam = 0`) and holds no
            // storage of its own; a leaf partition is an ordinary heap.
            let (relam, relkind) = match kind {
                RelKind::Table => (HEAP_AM_OID, "r"),
                RelKind::PartitionedTable => (0, "p"),
                RelKind::View => (0, "v"),
                RelKind::Sequence => (0, "S"),
            };
            vec![
                Value::Oid(*oid),
                Value::Text(schema.name.clone()),
                Value::Oid(nsp_oid(&schema.namespace)),
                Value::Oid(0),
                Value::Oid(10),
                Value::Oid(relam),
                Value::Int2(schema.columns.len() as i16),
                Value::Bool(indexes.iter().any(|index| index.table_oid == *oid)),
                Value::Text(schema.persistence.as_char().to_string()),
                Value::Text(relkind.to_string()),
                Value::Bool(schema.partition_of.is_some()),
            ]
        })
        .collect();
    rows.extend(indexes.iter().map(|index| {
        vec![
            Value::Oid(index.oid),
            Value::Text(index.metadata.name.clone()),
            // An index lives in its table's namespace.
            Value::Oid(nsp_oid(&index.table_schema.namespace)),
            Value::Oid(0),
            Value::Oid(10),
            Value::Oid(match index.metadata.method {
                IndexMethod::BTree => 403,
                IndexMethod::Hash => 405,
            }),
            Value::Int2(index.metadata.keys.len() as i16),
            Value::Bool(false),
            Value::Text("p".to_string()),
            Value::Text("i".to_string()),
            Value::Bool(false),
        ]
    }));
    rows
}

/// `pg_catalog.pg_inherits` — the parent/child links of declarative partitions
/// (and, in PG, table inheritance). One row per leaf partition.
pub fn pg_inherits_schema() -> TableSchema {
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

/// One `pg_inherits` row per leaf partition, linking its OID to its parent's.
/// Both OIDs come from the same positional assignment as `pg_class`, so the
/// `inhrelid`/`inhparent` → `pg_class.oid` joins line up.
pub fn pg_inherits_rows(relations: &[(u32, TableSchema)]) -> Vec<Vec<Value>> {
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
    }
    rows
}

/// `pg_catalog.pg_partitioned_table` — one row per partitioned (parent) table,
/// describing its partition key. A curated subset: `partdefid` (the default
/// partition) is always 0 and the class/collation/expression vectors are omitted.
pub fn pg_partitioned_table_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_partitioned_table",
        "pg_catalog",
        vec![
            col("partrelid", PgType::Oid),
            col("partstrat", CHARLIKE),
            col("partnatts", PgType::Int2),
            col("partdefid", PgType::Oid),
            // PG types this `int2vector`; we render the 1-based key attnums as the
            // same space-separated text `int2vectorout` produces.
            col("partattrs", PgType::Text),
        ],
    )
}

/// One `pg_partitioned_table` row per partitioned parent.
pub fn pg_partitioned_table_rows(relations: &[(u32, TableSchema)]) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for (oid, schema) in relations {
        if let Some(scheme) = &schema.partition_scheme {
            let strat = match scheme.strategy {
                PartitionStrategy::Range => "r",
            };
            let attrs = scheme
                .key_columns
                .iter()
                .map(|i| (i + 1).to_string())
                .collect::<Vec<_>>()
                .join(" ");
            rows.push(vec![
                Value::Oid(*oid),
                Value::Text(strat.to_string()),
                Value::Int2(scheme.key_columns.len() as i16),
                Value::Oid(0),
                Value::Text(attrs),
            ]);
        }
    }
    rows
}

/// `pg_catalog.pg_attribute` — a curated subset of columns for user relations'
/// columns. System (negative `attnum`) columns are not emitted yet.
pub fn pg_attribute_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_attribute",
        "pg_catalog",
        vec![
            col("attrelid", PgType::Oid),
            col("attname", PgType::Name),
            col("atttypid", PgType::Oid),
            col("attlen", PgType::Int2),
            col("attnum", PgType::Int2),
            col("atttypmod", PgType::Int4),
            col("attnotnull", PgType::Bool),
            col("atthasdef", PgType::Bool),
            col("attisdropped", PgType::Bool),
            col("attcollation", PgType::Oid),
        ],
    )
}

/// `attcollation`: the column's explicit `COLLATE`, else the type's own
/// collation — and `0` when the type has none, as PostgreSQL records it.
fn attcollation_of(column: &Column) -> u32 {
    match column.collation {
        Some(oid) => oid,
        None => typcollation_of(column.ty.oid()),
    }
}

/// Build `pg_attribute` rows: one per column of each relation, `attnum` 1-based
/// (user columns only), typed from the column's `PgType` (`atttypid`/`attlen`).
pub fn pg_attribute_rows(
    relations: &[(u32, TableSchema)],
    indexes: &[CatalogIndex],
) -> Vec<Vec<Value>> {
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
                Value::Bool(!c.nullable),
                Value::Bool(c.default.is_some()),
                Value::Bool(false),
                Value::Oid(attcollation_of(c)),
            ]);
        }
    }
    for index in indexes {
        for (position, key) in index.metadata.keys.iter().enumerate() {
            let column = &index.table_schema.columns[key.column];
            rows.push(vec![
                Value::Oid(index.oid),
                Value::Text(column.name.clone()),
                Value::Oid(column.ty.oid()),
                Value::Int2(column.ty.typlen()),
                Value::Int2((position + 1) as i16),
                Value::Int4(column.typmod),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
                Value::Oid(attcollation_of(column)),
            ]);
        }
    }
    rows
}

pub fn pg_attrdef_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_attrdef",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("adrelid", PgType::Oid),
            col("adnum", PgType::Int2),
            col("adbin", PgType::Text),
        ],
    )
}

pub fn pg_attrdef_rows(relations: &[(u32, TableSchema)]) -> Vec<Vec<Value>> {
    let mut next_oid = 30000_u32;
    let mut rows = Vec::new();
    for (table_oid, schema) in relations {
        for (position, column) in schema.columns.iter().enumerate() {
            if let Some(default) = &column.default {
                rows.push(vec![
                    Value::Oid(next_oid),
                    Value::Oid(*table_oid),
                    Value::Int2((position + 1) as i16),
                    Value::Text(default.clone()),
                ]);
                next_oid += 1;
            }
        }
    }
    rows
}

pub fn pg_constraint_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_constraint",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("conname", PgType::Name),
            col("connamespace", PgType::Oid),
            col("contype", CHARLIKE),
            col("condeferrable", PgType::Bool),
            col("condeferred", PgType::Bool),
            col("convalidated", PgType::Bool),
            col("conrelid", PgType::Oid),
            col("conindid", PgType::Oid),
            // int2[] is represented as PG array text until catalog arrays land.
            col("conkey", PgType::Text),
        ],
    )
}

pub fn pg_constraint_rows(
    relations: &[(u32, TableSchema)],
    indexes: &[CatalogIndex],
    namespace_oids: &HashMap<String, u32>,
) -> Vec<Vec<Value>> {
    let nsp_oid = |namespace: &str| namespace_oids.get(namespace).copied().unwrap_or(2200);
    let mut next_oid = 31000_u32;
    let mut rows = Vec::new();
    for (table_oid, schema) in relations {
        for (position, column) in schema.columns.iter().enumerate() {
            if let Some(name) = &column.not_null_constraint {
                rows.push(constraint_row(
                    next_oid,
                    name,
                    nsp_oid(&schema.namespace),
                    "n",
                    *table_oid,
                    0,
                    &[position],
                ));
                next_oid += 1;
            }
        }
    }
    for index in indexes {
        if let Some(constraint) = index.metadata.constraint {
            rows.push(constraint_row(
                next_oid,
                &index.metadata.name,
                nsp_oid(&index.table_schema.namespace),
                match constraint {
                    IndexConstraint::PrimaryKey => "p",
                    IndexConstraint::Unique => "u",
                },
                index.table_oid,
                index.oid,
                &index
                    .metadata
                    .keys
                    .iter()
                    .map(|key| key.column)
                    .collect::<Vec<_>>(),
            ));
            next_oid += 1;
        }
    }
    rows
}

fn constraint_row(
    oid: u32,
    name: &str,
    connamespace: u32,
    kind: &str,
    table_oid: u32,
    index_oid: u32,
    columns: &[usize],
) -> Vec<Value> {
    vec![
        Value::Oid(oid),
        Value::Text(name.to_string()),
        Value::Oid(connamespace),
        Value::Text(kind.to_string()),
        Value::Bool(false),
        Value::Bool(false),
        Value::Bool(true),
        Value::Oid(table_oid),
        Value::Oid(index_oid),
        Value::Text(format!(
            "{{{}}}",
            columns
                .iter()
                .map(|column| (column + 1).to_string())
                .collect::<Vec<_>>()
                .join(",")
        )),
    ]
}

pub fn pg_index_schema() -> TableSchema {
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
            col("indkey", PgType::Text),
            col("indoption", PgType::Text),
        ],
    )
}

pub fn pg_index_rows(indexes: &[CatalogIndex]) -> Vec<Vec<Value>> {
    indexes
        .iter()
        .map(|index| {
            let indkey = index
                .metadata
                .keys
                .iter()
                .map(|key| (key.column + 1).to_string())
                .collect::<Vec<_>>()
                .join(" ");
            let indoption = index
                .metadata
                .keys
                .iter()
                .map(|key| {
                    let mut option = 0;
                    if key.descending {
                        option |= 1;
                    }
                    if key.nulls_first {
                        option |= 2;
                    }
                    option.to_string()
                })
                .collect::<Vec<_>>()
                .join(" ");
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
                Value::Text(indkey),
                Value::Text(indoption),
            ]
        })
        .collect()
}

/// Fixed `pg_namespace` rows: the reserved catalog/toast schemas plus `public`.
/// OIDs match PostgreSQL's stable assignments (`pg_catalog` = 11, `pg_toast` =
/// 99, `public` = 2200). `information_schema` has an initdb-assigned OID, so
/// it remains absent here; its named discovery surface lives in
/// `information_schema.schemata`. Owners are reported as the bootstrap
/// superuser (10) for now.
pub fn pg_namespace_rows(user_schemas: &[(String, u32)]) -> Vec<Vec<Value>> {
    let row = |oid: u32, name: &str| {
        vec![
            Value::Oid(oid),
            Value::Text(name.to_string()),
            Value::Oid(10),
            Value::Null,
        ]
    };
    let mut rows = vec![
        row(11, "pg_catalog"),
        row(99, "pg_toast"),
        row(2200, "public"),
    ];
    for (name, oid) in user_schemas {
        rows.push(row(*oid, name));
    }
    rows
}

/// `information_schema.schemata`. Information-schema domains are represented
/// as text until the engine supports domains over the built-in types.
pub fn information_schema_schemata_schema() -> TableSchema {
    TableSchema::in_namespace(
        "schemata",
        "information_schema",
        vec![
            col("catalog_name", PgType::Text),
            col("schema_name", PgType::Text),
            col("schema_owner", PgType::Text),
            col("default_character_set_catalog", PgType::Text),
            col("default_character_set_schema", PgType::Text),
            col("default_character_set_name", PgType::Text),
            col("sql_path", PgType::Text),
        ],
    )
}

pub fn information_schema_schemata_rows(
    database: &str,
    owner: &str,
    relations: &[CatalogRelation],
    user_schemas: &[(String, u32)],
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
    // Include freshly-created, still-empty user schemas (no relations yet).
    for (name, _) in user_schemas {
        if !namespaces.contains(name) {
            namespaces.push(name.clone());
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
    TableSchema::in_namespace(
        "tables",
        "information_schema",
        vec![
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
    )
}

pub fn information_schema_tables_rows(
    database: &str,
    relations: &[CatalogRelation],
) -> Vec<Vec<Value>> {
    relations
        .iter()
        // Sequences are not tables: PG omits them from information_schema.tables.
        .filter(|relation| relation.kind != RelKind::Sequence)
        .map(|relation| {
            vec![
                Value::Text(database.to_string()),
                Value::Text(relation.namespace.clone()),
                Value::Text(relation.schema.name.clone()),
                Value::Text(
                    match (relation.kind, relation.temporary) {
                        (RelKind::View, _) => "VIEW",
                        (RelKind::Table, true) => "LOCAL TEMPORARY",
                        (RelKind::Table, false) => "BASE TABLE",
                        // A partitioned parent reflects as BASE TABLE, as in PG.
                        (RelKind::PartitionedTable, _) => "BASE TABLE",
                        (RelKind::Sequence, _) => unreachable!("filtered out above"),
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
    TableSchema::in_namespace(
        "columns",
        "information_schema",
        vec![
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
    )
}

pub fn information_schema_columns_rows(
    database: &str,
    relations: &[CatalogRelation],
) -> Vec<Vec<Value>> {
    relations
        .iter()
        // Sequences are not tables; omit their columns from information_schema.
        .filter(|relation| relation.kind != RelKind::Sequence)
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
                    // PG's view joins pg_collation but excludes
                    // `pg_catalog.default`, so a column left on the database
                    // collation reports NULL here rather than "default".
                    let collation =
                        crabgresql_types::collation::lookup_by_oid(attcollation_of(column))
                            .filter(|c| c.name != "default");
                    let (collation_catalog, collation_schema, collation_name) = match collation {
                        Some(c) => (
                            Value::Text(database.to_string()),
                            Value::Text("pg_catalog".to_string()),
                            Value::Text(c.name.to_string()),
                        ),
                        None => (Value::Null, Value::Null, Value::Null),
                    };
                    vec![
                        Value::Text(database.to_string()),
                        Value::Text(relation.namespace.clone()),
                        Value::Text(relation.schema.name.clone()),
                        Value::Text(column.name.clone()),
                        Value::Int4((index + 1) as i32),
                        column
                            .default
                            .as_ref()
                            .map(|default| Value::Text(default.clone()))
                            .unwrap_or(Value::Null),
                        Value::Text(if column.nullable { "YES" } else { "NO" }.to_string()),
                        Value::Text(column.ty.name().to_string()),
                        character_length,
                        character_octets,
                        precision,
                        radix,
                        // numeric_scale
                        Value::Null,
                        datetime_precision,
                        // interval_type, interval_precision
                        Value::Null,
                        Value::Null,
                        // character_set_{catalog,schema,name}
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        collation_catalog,
                        collation_schema,
                        collation_name,
                        // domain_{catalog,schema,name}
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
