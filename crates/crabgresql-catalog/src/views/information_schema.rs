//! The SQL-standard `information_schema` views served as relations.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::RelKind;
use crate::SystemCatalog;
use crate::catalogs::attribute::attcollation_of;
use crate::cols::*;

/// `information_schema.schemata`. Information-schema domains are represented
/// as text until the engine supports domains over the built-in types.
pub(crate) fn schemata_schema() -> TableSchema {
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

pub(crate) fn schemata_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let (database, owner) = (cat.database(), cat.owner());
    let (relations, user_schemas) = (cat.live_relations(), cat.user_schemas());
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
pub(crate) fn tables_schema() -> TableSchema {
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

pub(crate) fn tables_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let (database, relations) = (cat.database(), cat.live_relations());
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
pub(crate) fn columns_schema() -> TableSchema {
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

pub(crate) fn columns_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let (database, relations) = (cat.database(), cat.live_relations());
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
                    // `datetime_precision` is the declared fractional-second
                    // precision, defaulting to the 6 every datetime type keeps.
                    // `interval_type` names the fields the modifier admits,
                    // uppercased and with the precision appended
                    // (`DAY TO SECOND(4)`); a full-range `interval(3)` reports
                    // NULL there and carries its precision only in
                    // `datetime_precision`.
                    let (datetime_precision, interval_type) = match column.ty {
                        PgType::Time | PgType::TimeTz | PgType::Timestamp | PgType::TimestampTz => {
                            let p = if column.typmod >= 0 { column.typmod } else { 6 };
                            (Value::Int4(p), Value::Null)
                        }
                        PgType::Interval => {
                            let (range, precision) =
                                crabgresql_types::interval::unpack_typmod(column.typmod);
                            let spelling =
                                crabgresql_types::interval::range_name(range).map(|fields| {
                                    let mut s = fields.to_ascii_uppercase();
                                    if let Some(p) = precision {
                                        s.push_str(&format!("({p})"));
                                    }
                                    Value::Text(s)
                                });
                            (
                                Value::Int4(precision.map_or(6, i32::from)),
                                spelling.unwrap_or(Value::Null),
                            )
                        }
                        _ => (Value::Null, Value::Null),
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
                        interval_type,
                        // `interval_precision` is always NULL in PostgreSQL too:
                        // an interval's precision is reported through
                        // `datetime_precision` instead.
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
