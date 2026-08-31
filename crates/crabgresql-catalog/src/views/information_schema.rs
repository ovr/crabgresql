//! The SQL-standard `information_schema` views served as relations.

use crabgresql_storage_api::{TableSchema, pg_typmod};
use crabgresql_types::{PgType, Value};

use crate::RelKind;
use crate::SystemCatalog;
use crate::catalogs::attribute::attcollation_of;

/// The type-shape columns `information_schema.columns` and
/// `information_schema.domains` both report. Factored out because a domain and
/// a column of that domain answer them identically — the domain's modifier
/// applied to the domain's base type.
struct TypeAttributes {
    character_length: Value,
    character_octets: Value,
    numeric_precision: Value,
    numeric_radix: Value,
    numeric_scale: Value,
    datetime_precision: Value,
    interval_type: Value,
}

/// PostgreSQL defines these two views *in terms of* the
/// `information_schema._pg_*` functions, so both go through the one
/// implementation those functions use — that is what keeps a view column and a
/// direct call from drifting apart.
///
/// `typmod` arrives **declared** — a `varchar(10)` column stores 10 — while the
/// shared answers are stated over PostgreSQL's `atttypmod` encoding, where the
/// same column reads 14. [`pg_typmod`] is the conversion the catalog boundary
/// already uses, so it is the one applied here.
fn type_attributes(ty: PgType, typmod: i32) -> TypeAttributes {
    use crabgresql_types::info_schema as shape;
    let typmod = pg_typmod(ty, typmod);
    // The four answers that strip the varlena header can only overflow on a
    // modifier no catalog row holds (they need one near `i32`'s ends); a
    // `Value::Null` is the closest thing a view column has to the error a direct
    // call would raise.
    let int = |v: Option<i32>| v.map_or(Value::Null, Value::Int4);
    TypeAttributes {
        character_length: int(shape::char_max_length(ty, typmod).unwrap_or(None)),
        character_octets: int(shape::char_octet_length(ty, typmod).unwrap_or(None)),
        numeric_precision: int(shape::numeric_precision(ty, typmod).unwrap_or(None)),
        numeric_radix: int(shape::numeric_precision_radix(ty, typmod)),
        numeric_scale: int(shape::numeric_scale(ty, typmod).unwrap_or(None)),
        datetime_precision: int(shape::datetime_precision(ty, typmod)),
        interval_type: shape::interval_type(ty, typmod).map_or(Value::Null, Value::Text),
    }
}

/// The column constructors these views are written in.
///
/// The SQL standard types every `information_schema` column as a domain, and
/// PostgreSQL follows it — `table_name` is `information_schema.sql_identifier`,
/// not `name`. The domain is what `\d` and `pg_typeof` report; the value in the
/// row is the base value, and the wire still carries the base OID, because
/// `RowDescription` un-domains every column (see the server's
/// `undomain_columns`).
///
/// One per domain rather than a `col(name, domain(..))` pair, so a column list
/// reads as the definition text in [`crate::views::definitions`] does.
mod domain_col {
    use crabgresql_storage_api::Column;
    use crabgresql_types::PgType;

    use crate::info_schema;

    fn of(name: &str, oid: u32) -> Column {
        Column::new(name, PgType::User(oid))
    }

    pub(super) fn sql_identifier(name: &str) -> Column {
        of(name, info_schema::SQL_IDENTIFIER)
    }

    pub(super) fn character_data(name: &str) -> Column {
        of(name, info_schema::CHARACTER_DATA)
    }

    pub(super) fn cardinal_number(name: &str) -> Column {
        of(name, info_schema::CARDINAL_NUMBER)
    }

    pub(super) fn yes_or_no(name: &str) -> Column {
        of(name, info_schema::YES_OR_NO)
    }
}

use domain_col::{cardinal_number, character_data, sql_identifier, yes_or_no};

/// A domain reduced to what the two views need: its name, the built-in it is
/// ultimately over, and its modifier.
struct ResolvedDomain {
    oid: u32,
    /// The schema it lives in: `public` for a `CREATE DOMAIN`,
    /// `information_schema` for the five `initdb` makes.
    namespace: String,
    name: String,
    /// The end of the `typbasetype` chain — the shape a value really has, and
    /// what a *column* of the domain reports.
    base: PgType,
    /// The immediate base, which `information_schema.domains` reports rather
    /// than the end of the chain: over `dd AS posint`, 18.4 shows
    /// `data_type = USER-DEFINED`, `udt_schema = public`, `udt_name = posint`.
    immediate: Immediate,
    typmod: i32,
    default: Option<String>,
    collation: u32,
}

/// A domain's immediate base: a built-in, or another domain named by name.
/// One value rather than two `Option`s, so "exactly one of them is set" is not
/// an invariant every reader has to re-derive.
enum Immediate {
    Builtin(PgType),
    Domain(String),
}

/// The `information_schema` domains first, then every `CREATE DOMAIN` — the
/// order `pg_type` publishes them in, and PostgreSQL lists its own five here
/// too (they are owned by the bootstrap superuser, which the view's privilege
/// test admits).
fn resolved_domains(cat: &SystemCatalog) -> Vec<ResolvedDomain> {
    let bootstrap = crate::info_schema::DOMAINS.iter().map(|d| ResolvedDomain {
        oid: d.oid,
        namespace: crate::info_schema::NAMESPACE.to_string(),
        name: d.name.to_string(),
        base: d.base,
        immediate: Immediate::Builtin(d.base),
        typmod: d.typmod,
        default: d.default.map(str::to_string),
        collation: d.collation,
    });
    bootstrap
        .chain(cat.user_types().iter().filter_map(|t| {
            let d = t.domain.as_ref()?;
            resolved_domain(cat, t, d)
        }))
        .collect()
}

fn resolved_domain(
    cat: &SystemCatalog,
    t: &crate::CatalogUserType,
    d: &crate::CatalogDomain,
) -> Option<ResolvedDomain> {
    // A base this build cannot name is skipped rather than reported as
    // something else; the only way that happens is a domain over a domain,
    // whose immediate base is a user OID no `PgType` answers to.
    let immediate = match cat
        .user_types()
        .iter()
        .find(|u| u.oid == d.basetype && u.domain.is_some())
    {
        Some(u) => Immediate::Domain(u.name.clone()),
        None => Immediate::Builtin(PgType::from_oid(d.basetype)?),
    };
    Some(ResolvedDomain {
        oid: t.oid,
        // Every `CREATE DOMAIN` lands in `public`, as every user type does here.
        namespace: "public".to_string(),
        name: t.name.clone(),
        base: PgType::from_oid(d.resolved_basetype)?,
        immediate,
        typmod: d.typmod,
        default: d.default.clone(),
        collation: d.collation,
    })
}

/// `information_schema.domains` — one row per domain, `initdb`'s five included.
pub(crate) fn domains_schema() -> TableSchema {
    TableSchema::in_namespace(
        "domains",
        "information_schema",
        vec![
            sql_identifier("domain_catalog"),
            sql_identifier("domain_schema"),
            sql_identifier("domain_name"),
            character_data("data_type"),
            cardinal_number("character_maximum_length"),
            cardinal_number("character_octet_length"),
            sql_identifier("character_set_catalog"),
            sql_identifier("character_set_schema"),
            sql_identifier("character_set_name"),
            sql_identifier("collation_catalog"),
            sql_identifier("collation_schema"),
            sql_identifier("collation_name"),
            cardinal_number("numeric_precision"),
            cardinal_number("numeric_precision_radix"),
            cardinal_number("numeric_scale"),
            cardinal_number("datetime_precision"),
            character_data("interval_type"),
            cardinal_number("interval_precision"),
            character_data("domain_default"),
            sql_identifier("udt_catalog"),
            sql_identifier("udt_schema"),
            sql_identifier("udt_name"),
            sql_identifier("scope_catalog"),
            sql_identifier("scope_schema"),
            sql_identifier("scope_name"),
            cardinal_number("maximum_cardinality"),
            sql_identifier("dtd_identifier"),
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
                Immediate::Builtin(base) => type_attributes(base, d.typmod),
                Immediate::Domain(_) => type_attributes(PgType::User(0), -1),
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
                Value::Text(d.namespace),
                Value::Text(d.name),
                // `USER-DEFINED` is what PostgreSQL reports when the base is
                // itself a domain — the standard has no name for one.
                Value::Text(match &d.immediate {
                    Immediate::Builtin(base) => base.name().to_string(),
                    Immediate::Domain(_) => "USER-DEFINED".to_string(),
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
                attrs.numeric_scale,
                attrs.datetime_precision,
                attrs.interval_type,
                // interval_precision, as in `columns`: always NULL, the value
                // travels in `datetime_precision`.
                Value::Null,
                d.default.map_or(Value::Null, Value::Text),
                Value::Text(database.to_string()),
                match &d.immediate {
                    Immediate::Domain(_) => Value::Text("public".to_string()),
                    Immediate::Builtin(_) => Value::Text("pg_catalog".to_string()),
                },
                match &d.immediate {
                    Immediate::Domain(name) => Value::Text(name.clone()),
                    Immediate::Builtin(base) => Value::Text(base.typname().to_string()),
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
pub(crate) fn schemata_schema() -> TableSchema {
    TableSchema::in_namespace(
        "schemata",
        "information_schema",
        vec![
            sql_identifier("catalog_name"),
            sql_identifier("schema_name"),
            sql_identifier("schema_owner"),
            sql_identifier("default_character_set_catalog"),
            sql_identifier("default_character_set_schema"),
            sql_identifier("default_character_set_name"),
            character_data("sql_path"),
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
            sql_identifier("table_catalog"),
            sql_identifier("table_schema"),
            sql_identifier("table_name"),
            character_data("table_type"),
            sql_identifier("self_referencing_column_name"),
            character_data("reference_generation"),
            sql_identifier("user_defined_type_catalog"),
            sql_identifier("user_defined_type_schema"),
            sql_identifier("user_defined_type_name"),
            yes_or_no("is_insertable_into"),
            yes_or_no("is_typed"),
            character_data("commit_action"),
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
    TableSchema::in_namespace(
        "columns",
        "information_schema",
        vec![
            sql_identifier("table_catalog"),
            sql_identifier("table_schema"),
            sql_identifier("table_name"),
            sql_identifier("column_name"),
            cardinal_number("ordinal_position"),
            character_data("column_default"),
            yes_or_no("is_nullable"),
            character_data("data_type"),
            cardinal_number("character_maximum_length"),
            cardinal_number("character_octet_length"),
            cardinal_number("numeric_precision"),
            cardinal_number("numeric_precision_radix"),
            cardinal_number("numeric_scale"),
            cardinal_number("datetime_precision"),
            character_data("interval_type"),
            cardinal_number("interval_precision"),
            sql_identifier("character_set_catalog"),
            sql_identifier("character_set_schema"),
            sql_identifier("character_set_name"),
            sql_identifier("collation_catalog"),
            sql_identifier("collation_schema"),
            sql_identifier("collation_name"),
            sql_identifier("domain_catalog"),
            sql_identifier("domain_schema"),
            sql_identifier("domain_name"),
            sql_identifier("udt_catalog"),
            sql_identifier("udt_schema"),
            sql_identifier("udt_name"),
            sql_identifier("scope_catalog"),
            sql_identifier("scope_schema"),
            sql_identifier("scope_name"),
            cardinal_number("maximum_cardinality"),
            sql_identifier("dtd_identifier"),
            yes_or_no("is_self_referencing"),
            yes_or_no("is_identity"),
            character_data("identity_generation"),
            character_data("identity_start"),
            character_data("identity_increment"),
            character_data("identity_maximum"),
            character_data("identity_minimum"),
            yes_or_no("identity_cycle"),
            character_data("is_generated"),
            character_data("generation_expression"),
            yes_or_no("is_updatable"),
        ],
    )
}

pub(crate) fn columns_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let (database, relations) = (cat.database(), cat.live_relations());
    // Once for the view, not twice per user-typed column: resolving one domain
    // scans the type list, and this view is read over a whole database. Built
    // from the same list `domains_rows` publishes, so a column of a domain and
    // the domain's own row cannot describe it differently.
    let domains: std::collections::HashMap<u32, ResolvedDomain> = resolved_domains(cat)
        .into_iter()
        .map(|d| (d.oid, d))
        .collect();
    let domains = &domains;
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
                    let domain = match column.ty {
                        PgType::User(oid) => domains.get(&oid),
                        _ => None,
                    };
                    let (ty, typmod) = match &domain {
                        Some(d) => (d.base, d.typmod),
                        None => (column.ty, column.typmod),
                    };
                    let (domain_catalog, domain_schema, domain_name) = match &domain {
                        Some(d) => (
                            Value::Text(database.to_string()),
                            Value::Text(d.namespace.clone()),
                            Value::Text(d.name.clone()),
                        ),
                        None => (Value::Null, Value::Null, Value::Null),
                    };
                    let TypeAttributes {
                        character_length,
                        character_octets,
                        numeric_precision: precision,
                        numeric_radix: radix,
                        numeric_scale,
                        datetime_precision,
                        interval_type,
                    } = type_attributes(ty, typmod);
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
                        numeric_scale,
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
