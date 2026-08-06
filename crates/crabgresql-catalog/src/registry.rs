//! The one table of served relations.
//!
//! Everything that used to be stated three times — a `match` arm in
//! `build_pg_catalog`, an entry in the OID table, and a name in the test that
//! kept the two in step — is stated once here. "Served" and "has a fixed OID"
//! are the same set *by construction*, so [`crate::SystemCatalog::has_catalog_relation`]
//! can answer from the OID alone and no test has to carry a second copy of the
//! list.
//!
//! Adding a relation is a module under [`crate::catalogs`] publishing the pair
//! below, plus one line in [`CATALOG_RELATIONS`].
//!
//! The single `rows` signature is what keeps that true: a relation gathers what
//! it needs from the snapshot itself, so a new one with unusual inputs adds no
//! argument to any shared call site.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::Value;

use crate::SystemCatalog;
use crate::catalogs::{
    am, attribute, auth, class, collation, constraint, cursors, database, index, inherits,
    language, namespace, proc, sequence, settings, timezone, types,
};
use crate::views::information_schema;

/// Which schema a served relation lives in. Ordered as declared, because
/// [`CATALOG_RELATIONS`] is sorted on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CatalogNamespace {
    InformationSchema,
    PgCatalog,
}

/// One served relation: its identity, its shape, and how its rows are built.
///
/// `schema`/`rows` are plain `fn` pointers rather than a trait object so the
/// whole table stays a `static` with no initialization at run time.
pub(crate) struct CatalogRelDef {
    pub(crate) name: &'static str,
    /// PostgreSQL's own OID for the relation, probed from 18.4 — `0` for the
    /// `information_schema` views, which have none here yet (they are Rust row
    /// builders, not the `pg_class`-reflected views W4 turns them into).
    pub(crate) oid: u32,
    pub(crate) namespace: CatalogNamespace,
    pub(crate) schema: fn() -> TableSchema,
    pub(crate) rows: fn(&SystemCatalog) -> Vec<Vec<Value>>,
}

const fn rel(
    name: &'static str,
    oid: u32,
    namespace: CatalogNamespace,
    schema: fn() -> TableSchema,
    rows: fn(&SystemCatalog) -> Vec<Vec<Value>>,
) -> CatalogRelDef {
    CatalogRelDef {
        name,
        oid,
        namespace,
        schema,
        rows,
    }
}

use CatalogNamespace::{InformationSchema, PgCatalog};

/// Every relation this build serves, sorted by `(namespace, name)` so
/// [`lookup`] can binary-search it. `the_registry_is_sorted_and_its_names_are_unique`
/// fails if a new entry lands out of order.
///
/// The `pg_catalog` OIDs are PostgreSQL's own assignments rather than invented
/// values, so an OID a client hard-codes means the same thing here. The ones in
/// the 12000 band belong to relations `initdb` creates as views rather than
/// bootstrap catalogs; they are deterministic for a given major version, and all
/// were probed from the same 18.4 — `pg_cursors` reading back 12077 there is
/// what confirms the whole band came from one `initdb`.
pub(crate) static CATALOG_RELATIONS: &[CatalogRelDef] = &[
    rel(
        "columns",
        0,
        InformationSchema,
        information_schema::columns_schema,
        information_schema::columns_rows,
    ),
    rel(
        "schemata",
        0,
        InformationSchema,
        information_schema::schemata_schema,
        information_schema::schemata_rows,
    ),
    rel(
        "tables",
        0,
        InformationSchema,
        information_schema::tables_schema,
        information_schema::tables_rows,
    ),
    rel("pg_am", 2601, PgCatalog, am::pg_am_schema, am::pg_am_rows),
    rel(
        "pg_attrdef",
        2604,
        PgCatalog,
        attribute::pg_attrdef_schema,
        attribute::pg_attrdef_rows,
    ),
    rel(
        "pg_attribute",
        1249,
        PgCatalog,
        attribute::pg_attribute_schema,
        attribute::pg_attribute_rows,
    ),
    rel(
        "pg_auth_members",
        1261,
        PgCatalog,
        auth::pg_auth_members_schema,
        auth::pg_auth_members_rows,
    ),
    rel(
        "pg_authid",
        1260,
        PgCatalog,
        auth::pg_authid_schema,
        auth::pg_authid_rows,
    ),
    rel(
        "pg_cast",
        2605,
        PgCatalog,
        types::pg_cast_schema,
        types::pg_cast_rows,
    ),
    rel(
        "pg_class",
        1259,
        PgCatalog,
        class::pg_class_schema,
        class::pg_class_rows,
    ),
    rel(
        "pg_collation",
        3456,
        PgCatalog,
        collation::pg_collation_schema,
        collation::pg_collation_rows,
    ),
    rel(
        "pg_constraint",
        2606,
        PgCatalog,
        constraint::pg_constraint_schema,
        constraint::pg_constraint_rows,
    ),
    rel(
        "pg_cursors",
        12077,
        PgCatalog,
        cursors::pg_cursors_schema,
        cursors::pg_cursors_rows,
    ),
    rel(
        "pg_database",
        1262,
        PgCatalog,
        database::pg_database_schema,
        database::pg_database_rows,
    ),
    rel(
        "pg_enum",
        3501,
        PgCatalog,
        types::pg_enum_schema,
        types::pg_enum_rows,
    ),
    rel(
        "pg_group",
        12010,
        PgCatalog,
        auth::pg_group_schema,
        auth::pg_group_rows,
    ),
    rel(
        "pg_index",
        2610,
        PgCatalog,
        index::pg_index_schema,
        index::pg_index_rows,
    ),
    rel(
        "pg_inherits",
        2611,
        PgCatalog,
        inherits::pg_inherits_schema,
        inherits::pg_inherits_rows,
    ),
    rel(
        "pg_language",
        2612,
        PgCatalog,
        language::pg_language_schema,
        language::pg_language_rows,
    ),
    rel(
        "pg_namespace",
        2615,
        PgCatalog,
        namespace::pg_namespace_schema,
        namespace::pg_namespace_rows,
    ),
    rel(
        "pg_partitioned_table",
        3350,
        PgCatalog,
        inherits::pg_partitioned_table_schema,
        inherits::pg_partitioned_table_rows,
    ),
    rel(
        "pg_proc",
        1255,
        PgCatalog,
        proc::pg_proc_schema,
        proc::pg_proc_rows,
    ),
    rel(
        "pg_roles",
        12000,
        PgCatalog,
        auth::pg_roles_schema,
        auth::pg_roles_rows,
    ),
    rel(
        "pg_sequence",
        2224,
        PgCatalog,
        sequence::pg_sequence_schema,
        sequence::pg_sequence_rows,
    ),
    rel(
        "pg_settings",
        12104,
        PgCatalog,
        settings::pg_settings_schema,
        settings::pg_settings_rows,
    ),
    rel(
        "pg_shadow",
        12005,
        PgCatalog,
        auth::pg_shadow_schema,
        auth::pg_shadow_rows,
    ),
    rel(
        "pg_tablespace",
        1213,
        PgCatalog,
        database::pg_tablespace_schema,
        database::pg_tablespace_rows,
    ),
    rel(
        "pg_timezone_abbrevs",
        12122,
        PgCatalog,
        timezone::pg_timezone_abbrevs_schema,
        timezone::pg_timezone_abbrevs_rows,
    ),
    rel(
        "pg_timezone_names",
        12126,
        PgCatalog,
        timezone::pg_timezone_names_schema,
        timezone::pg_timezone_names_rows,
    ),
    rel(
        "pg_type",
        1247,
        PgCatalog,
        types::pg_type_schema,
        types::pg_type_rows,
    ),
    rel(
        "pg_user",
        12014,
        PgCatalog,
        auth::pg_user_schema,
        auth::pg_user_rows,
    ),
];

/// The definition of `namespace.name`, or `None` if this build serves no such
/// relation.
pub(crate) fn lookup(namespace: CatalogNamespace, name: &str) -> Option<&'static CatalogRelDef> {
    CATALOG_RELATIONS
        .binary_search_by(|def| (def.namespace, def.name).cmp(&(namespace, name)))
        .ok()
        .map(|i| &CATALOG_RELATIONS[i])
}

/// The fixed OID of the `pg_catalog` relation `name`, if this build serves one.
///
/// Catalog relations are not reflected into `pg_class` (only live user relations
/// are), so they have no OID from the positional assignment
/// [`SystemCatalog::relation_oids`](crate::SystemCatalog) hands out. They still
/// need one: a client identifies a relation by casting its name —
/// `'pg_class'::regclass` — and expects the OID back to render as the name again.
pub fn builtin_relation_oid(name: &str) -> Option<u32> {
    lookup(CatalogNamespace::PgCatalog, name).map(|def| def.oid)
}

/// The inverse of [`builtin_relation_oid`]: the `pg_catalog` relation `oid`
/// names, if it is one of the fixed assignments.
pub fn builtin_relation_name(oid: u32) -> Option<&'static str> {
    // Linear, unlike the by-name direction: the table is sorted by name, and a
    // second sorted-by-OID copy would be one more thing to keep in step.
    CATALOG_RELATIONS
        .iter()
        .find(|def| def.namespace == CatalogNamespace::PgCatalog && def.oid == oid)
        .map(|def| def.name)
}
