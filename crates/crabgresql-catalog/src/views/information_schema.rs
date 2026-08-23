//! The SQL-standard `information_schema` views served as relations.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::RelKind;
use crate::SystemCatalog;
use crate::catalogs::attribute::attcollation_of;
use crate::cols::*;

/// The type-shape columns `information_schema.columns` and
/// `information_schema.domains` both report. Factored out because a domain and
/// a column of that domain answer them identically — the domain's modifier
/// applied to the domain's base type.
struct TypeAttributes {
    character_length: Value,
    character_octets: Value,
    numeric_precision: Value,
    numeric_radix: Value,
    datetime_precision: Value,
    interval_type: Value,
}

fn type_attributes(ty: PgType, typmod: i32) -> TypeAttributes {
    let (character_length, character_octets) = match ty {
        PgType::Varchar | PgType::Bpchar if typmod >= 0 => {
            (Value::Int4(typmod), Value::Int4(typmod * 4))
        }
        PgType::Bit | PgType::Varbit if typmod >= 0 => (Value::Int4(typmod), Value::Null),
        _ => (Value::Null, Value::Null),
    };
    let (numeric_precision, numeric_radix) = match ty {
        PgType::Int2 => (Value::Int4(16), Value::Int4(2)),
        PgType::Int4 => (Value::Int4(32), Value::Int4(2)),
        PgType::Int8 => (Value::Int4(64), Value::Int4(2)),
        PgType::Float4 => (Value::Int4(24), Value::Int4(2)),
        PgType::Float8 => (Value::Int4(53), Value::Int4(2)),
        _ => (Value::Null, Value::Null),
    };
    // `datetime_precision` is the declared fractional-second precision,
    // defaulting to the 6 every datetime type keeps. `interval_type` names the
    // fields the modifier admits, uppercased and with the precision appended
    // (`DAY TO SECOND(4)`); a full-range `interval(3)` reports NULL there and
    // carries its precision only in `datetime_precision`.
    let (datetime_precision, interval_type) = match ty {
        PgType::Time | PgType::TimeTz | PgType::Timestamp | PgType::TimestampTz => {
            let p = if typmod >= 0 { typmod } else { 6 };
            (Value::Int4(p), Value::Null)
        }
        PgType::Interval => {
            let (range, precision) = crabgresql_types::interval::unpack_typmod(typmod);
            let spelling = crabgresql_types::interval::range_name(range).map(|fields| {
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
    TypeAttributes {
        character_length,
        character_octets,
        numeric_precision,
        numeric_radix,
        datetime_precision,
        interval_type,
    }
}

/// A domain reduced to what the two views need: its name, the built-in it is
/// ultimately over, and its modifier.
struct ResolvedDomain {
    name: String,
    /// The end of the `typbasetype` chain — the shape a value really has, and
    /// what a *column* of the domain reports.
    base: PgType,
    /// The immediate base, `None` when it is itself a domain. `information_
    /// schema.domains` reports that one: over `dd AS posint`, 18.4 shows
    /// `data_type = USER-DEFINED`, `udt_schema = public`, `udt_name = posint`.
    immediate: Option<PgType>,
    /// The immediate base's name when it is a domain, for `udt_name`.
    immediate_name: Option<String>,
    typmod: i32,
    default: Option<String>,
    collation: u32,
}

fn resolved_domains(cat: &SystemCatalog) -> Vec<ResolvedDomain> {
    cat.user_types()
        .iter()
        .filter_map(|t| {
            let d = t.domain.as_ref()?;
            resolved_domain(cat, t, d)
        })
        .collect()
}

fn domain_of_column(
    cat: &SystemCatalog,
    column: &crabgresql_storage_api::Column,
) -> Option<ResolvedDomain> {
    let PgType::User(oid) = column.ty else {
        return None;
    };
    let t = cat.user_types().iter().find(|t| t.oid == oid)?;
    let d = t.domain.as_ref()?;
    resolved_domain(cat, t, d)
}

fn resolved_domain(
    cat: &SystemCatalog,
    t: &crate::CatalogUserType,
    d: &crate::CatalogDomain,
) -> Option<ResolvedDomain> {
    // A base this build cannot name is skipped rather than reported as
    // something else; the only way that happens is a domain over a domain,
    // whose immediate base is a user OID no `PgType` answers to.
    let immediate_name = cat
        .user_types()
        .iter()
        .find(|u| u.oid == d.basetype && u.domain.is_some())
        .map(|u| u.name.clone());
    Some(ResolvedDomain {
        name: t.name.clone(),
        base: PgType::from_oid(d.resolved_basetype)?,
        immediate: PgType::from_oid(d.basetype).filter(|_| immediate_name.is_none()),
        immediate_name,
        typmod: d.typmod,
        default: d.default.clone(),
        collation: d.collation,
    })
}

/// `information_schema.domains` — one row per `CREATE DOMAIN`.
pub(crate) fn domains_schema() -> TableSchema {
    let text = PgType::Text;
    let cardinal = PgType::Int4;
    TableSchema::in_namespace(
        "domains",
        "information_schema",
        vec![
            col("domain_catalog", text),
            col("domain_schema", text),
            col("domain_name", text),
            col("data_type", text),
            col("character_maximum_length", cardinal),
            col("character_octet_length", cardinal),
            col("character_set_catalog", text),
            col("character_set_schema", text),
            col("character_set_name", text),
            col("collation_catalog", text),
            col("collation_schema", text),
            col("collation_name", text),
            col("numeric_precision", cardinal),
            col("numeric_precision_radix", cardinal),
            col("numeric_scale", cardinal),
            col("datetime_precision", cardinal),
            col("interval_type", text),
            col("interval_precision", cardinal),
            col("domain_default", text),
            col("udt_catalog", text),
            col("udt_schema", text),
            col("udt_name", text),
            col("scope_catalog", text),
            col("scope_schema", text),
            col("scope_name", text),
            col("maximum_cardinality", cardinal),
            col("dtd_identifier", text),
        ],
    )
}

pub(crate) fn domains_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let database = cat.database();
    resolved_domains(cat)
        .into_iter()
        .map(|d| {
            // A domain over another domain reports no shape at all: its
            // `data_type` is `USER-DEFINED`, and PostgreSQL leaves every length
            // and precision column NULL beside it.
            let attrs = match d.immediate {
                Some(base) => type_attributes(base, d.typmod),
                None => type_attributes(PgType::User(0), -1),
            };
            // As in `columns`, PostgreSQL's view excludes `pg_catalog.default`,
            // so a domain left on the database collation reports NULL.
            let collation = crabgresql_types::collation::lookup_by_oid(d.collation)
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
                // Every `CREATE DOMAIN` lands in `public`, as every user type
                // does here.
                Value::Text("public".to_string()),
                Value::Text(d.name),
                // `USER-DEFINED` is what PostgreSQL reports when the base is
                // itself a domain — the standard has no name for one.
                Value::Text(match d.immediate {
                    Some(base) => base.name().to_string(),
                    None => "USER-DEFINED".to_string(),
                }),
                attrs.character_length,
                attrs.character_octets,
                // character_set_{catalog,schema,name}
                Value::Null,
                Value::Null,
                Value::Null,
                collation_catalog,
                collation_schema,
                collation_name,
                attrs.numeric_precision,
                attrs.numeric_radix,
                // numeric_scale
                Value::Null,
                attrs.datetime_precision,
                attrs.interval_type,
                // interval_precision, as in `columns`: always NULL, the value
                // travels in `datetime_precision`.
                Value::Null,
                d.default.map_or(Value::Null, Value::Text),
                Value::Text(database.to_string()),
                match &d.immediate_name {
                    Some(_) => Value::Text("public".to_string()),
                    None => Value::Text("pg_catalog".to_string()),
                },
                match (&d.immediate_name, d.immediate) {
                    (Some(name), _) => Value::Text(name.clone()),
                    (None, Some(base)) => Value::Text(base.typname().to_string()),
                    (None, None) => Value::Null,
                },
                // scope_{catalog,schema,name} / maximum_cardinality: reference
                // and array-domain columns, neither of which exists here.
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Text("1".to_string()),
            ]
        })
        .collect()
}

/// `information_schema.schemata`.
///
/// TODO: the SQL standard types these columns as domains (`sql_identifier`,
/// `character_data`); they are plain `text` until the engine has domains over
/// the built-in types.
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

/// `information_schema.tables` for represented user relations.
///
/// TODO: report the `pg_catalog` and `information_schema` relations that
/// PostgreSQL also lists here; they are served as Rust row builders rather than
/// reflected into `pg_class`, and nothing records which of them PostgreSQL
/// implements as a table and which as a view, so `table_type` would have to be
/// invented.
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
                    // A domain column reports the *base* type's shape — its
                    // `data_type`, length and `udt_name` are the base's, with
                    // the domain's own modifier applied, and the domain is
                    // named only through the `domain_*` triple. Probed on 18.4
                    // over `CREATE DOMAIN dcol AS varchar(5)`.
                    let domain = domain_of_column(cat, column);
                    let (ty, typmod) = match &domain {
                        Some(d) => (d.base, d.typmod),
                        None => (column.ty, column.typmod),
                    };
                    let (domain_catalog, domain_schema, domain_name) = match &domain {
                        Some(d) => (
                            Value::Text(database.to_string()),
                            Value::Text("public".to_string()),
                            Value::Text(d.name.clone()),
                        ),
                        None => (Value::Null, Value::Null, Value::Null),
                    };
                    let attrs = type_attributes(ty, typmod);
                    let (character_length, character_octets) =
                        (attrs.character_length, attrs.character_octets);
                    let (precision, radix) = (attrs.numeric_precision, attrs.numeric_radix);
                    let (datetime_precision, interval_type) =
                        (attrs.datetime_precision, attrs.interval_type);
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
                        Value::Text(ty.name().to_string()),
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
                        domain_catalog,
                        domain_schema,
                        domain_name,
                        Value::Text(database.to_string()),
                        Value::Text("pg_catalog".to_string()),
                        Value::Text(ty.typname().to_string()),
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
                        // is_generated / generation_expression. PostgreSQL
                        // reports the same non-pretty text `pg_get_expr` gives
                        // without its `pretty` flag, which is what the catalog
                        // stores.
                        Value::Text(
                            match column.generated.is_some() {
                                true => "ALWAYS",
                                false => "NEVER",
                            }
                            .to_string(),
                        ),
                        column
                            .generated
                            .as_ref()
                            .map(|g| Value::Text(g.expr.clone()))
                            .unwrap_or(Value::Null),
                        Value::Text("YES".to_string()),
                    ]
                })
        })
        .collect()
}
