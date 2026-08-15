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
    acl, am, attribute, auth, class, collation, constraint, cursors, database, description,
    extension, foreign, index, inherits, language, namespace, opclass, policy, prepared, proc,
    progress, publication, relviews, replication, rewrite, sequence, settings, statistic,
    statistic_ext, timezone, trigger, types,
};
use crate::cols::no_rows;
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
    /// PostgreSQL's own OID for the relation, probed from 18.4.
    ///
    /// TODO: `0` for the three `information_schema` entries. They are built as
    /// Rust rows rather than reflected into `pg_class`, so a client has nothing
    /// to cast `'information_schema.tables'::regclass` to; publishing them as
    /// relations is what would give them an OID worth recording here.
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
        "pg_available_extension_versions",
        12085,
        PgCatalog,
        extension::pg_available_extension_versions_schema,
        extension::pg_available_extension_versions_rows,
    ),
    rel(
        "pg_available_extensions",
        12081,
        PgCatalog,
        extension::pg_available_extensions_schema,
        extension::pg_available_extensions_rows,
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
        "pg_db_role_setting",
        2964,
        PgCatalog,
        acl::pg_db_role_setting_schema,
        no_rows,
    ),
    rel(
        "pg_default_acl",
        826,
        PgCatalog,
        acl::pg_default_acl_schema,
        no_rows,
    ),
    rel(
        "pg_description",
        2609,
        PgCatalog,
        description::pg_description_schema,
        description::pg_description_rows,
    ),
    rel(
        "pg_enum",
        3501,
        PgCatalog,
        types::pg_enum_schema,
        types::pg_enum_rows,
    ),
    rel(
        "pg_extension",
        3079,
        PgCatalog,
        extension::pg_extension_schema,
        extension::pg_extension_rows,
    ),
    rel(
        "pg_foreign_data_wrapper",
        2328,
        PgCatalog,
        foreign::pg_foreign_data_wrapper_schema,
        no_rows,
    ),
    rel(
        "pg_foreign_server",
        1417,
        PgCatalog,
        foreign::pg_foreign_server_schema,
        no_rows,
    ),
    rel(
        "pg_foreign_table",
        3118,
        PgCatalog,
        foreign::pg_foreign_table_schema,
        no_rows,
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
        "pg_indexes",
        12043,
        PgCatalog,
        relviews::pg_indexes_schema,
        relviews::pg_indexes_rows,
    ),
    rel(
        "pg_inherits",
        2611,
        PgCatalog,
        inherits::pg_inherits_schema,
        inherits::pg_inherits_rows,
    ),
    rel(
        "pg_init_privs",
        3394,
        PgCatalog,
        acl::pg_init_privs_schema,
        no_rows,
    ),
    rel(
        "pg_language",
        2612,
        PgCatalog,
        language::pg_language_schema,
        language::pg_language_rows,
    ),
    rel(
        "pg_matviews",
        12038,
        PgCatalog,
        relviews::pg_matviews_schema,
        no_rows,
    ),
    rel(
        "pg_namespace",
        2615,
        PgCatalog,
        namespace::pg_namespace_schema,
        namespace::pg_namespace_rows,
    ),
    rel(
        "pg_opclass",
        2616,
        PgCatalog,
        opclass::pg_opclass_schema,
        opclass::pg_opclass_rows,
    ),
    rel(
        "pg_opfamily",
        2753,
        PgCatalog,
        opclass::pg_opfamily_schema,
        opclass::pg_opfamily_rows,
    ),
    rel(
        "pg_parameter_acl",
        6243,
        PgCatalog,
        acl::pg_parameter_acl_schema,
        no_rows,
    ),
    rel(
        "pg_partitioned_table",
        3350,
        PgCatalog,
        inherits::pg_partitioned_table_schema,
        inherits::pg_partitioned_table_rows,
    ),
    rel(
        "pg_policies",
        12018,
        PgCatalog,
        policy::pg_policies_schema,
        no_rows,
    ),
    rel(
        "pg_policy",
        3256,
        PgCatalog,
        policy::pg_policy_schema,
        no_rows,
    ),
    rel(
        "pg_prepared_statements",
        12095,
        PgCatalog,
        prepared::pg_prepared_statements_schema,
        prepared::pg_prepared_statements_rows,
    ),
    rel(
        "pg_proc",
        1255,
        PgCatalog,
        proc::pg_proc_schema,
        proc::pg_proc_rows,
    ),
    rel(
        "pg_publication",
        6104,
        PgCatalog,
        publication::pg_publication_schema,
        no_rows,
    ),
    rel(
        "pg_publication_namespace",
        6237,
        PgCatalog,
        publication::pg_publication_namespace_schema,
        no_rows,
    ),
    rel(
        "pg_publication_rel",
        6106,
        PgCatalog,
        publication::pg_publication_rel_schema,
        no_rows,
    ),
    rel(
        "pg_publication_tables",
        12068,
        PgCatalog,
        publication::pg_publication_tables_schema,
        no_rows,
    ),
    rel(
        "pg_replication_origin",
        6000,
        PgCatalog,
        replication::pg_replication_origin_schema,
        no_rows,
    ),
    rel(
        "pg_replication_origin_status",
        12343,
        PgCatalog,
        replication::pg_replication_origin_status_schema,
        no_rows,
    ),
    rel(
        "pg_replication_slots",
        12261,
        PgCatalog,
        replication::pg_replication_slots_schema,
        no_rows,
    ),
    rel(
        "pg_rewrite",
        2618,
        PgCatalog,
        rewrite::pg_rewrite_schema,
        rewrite::pg_rewrite_rows,
    ),
    rel(
        "pg_roles",
        12000,
        PgCatalog,
        auth::pg_roles_schema,
        auth::pg_roles_rows,
    ),
    rel(
        "pg_rules",
        12023,
        PgCatalog,
        rewrite::pg_rules_schema,
        no_rows,
    ),
    rel(
        "pg_seclabel",
        3596,
        PgCatalog,
        acl::pg_seclabel_schema,
        no_rows,
    ),
    rel(
        "pg_seclabels",
        12099,
        PgCatalog,
        acl::pg_seclabels_schema,
        no_rows,
    ),
    rel(
        "pg_sequence",
        2224,
        PgCatalog,
        sequence::pg_sequence_schema,
        sequence::pg_sequence_rows,
    ),
    rel(
        "pg_sequences",
        12048,
        PgCatalog,
        relviews::pg_sequences_schema,
        relviews::pg_sequences_rows,
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
        "pg_shdepend",
        1214,
        PgCatalog,
        acl::pg_shdepend_schema,
        no_rows,
    ),
    rel(
        "pg_shdescription",
        2396,
        PgCatalog,
        acl::pg_shdescription_schema,
        no_rows,
    ),
    rel(
        "pg_shseclabel",
        3592,
        PgCatalog,
        acl::pg_shseclabel_schema,
        no_rows,
    ),
    rel(
        "pg_stat_progress_analyze",
        12309,
        PgCatalog,
        progress::pg_stat_progress_analyze_schema,
        no_rows,
    ),
    rel(
        "pg_stat_progress_basebackup",
        12329,
        PgCatalog,
        progress::pg_stat_progress_basebackup_schema,
        no_rows,
    ),
    rel(
        "pg_stat_progress_cluster",
        12319,
        PgCatalog,
        progress::pg_stat_progress_cluster_schema,
        no_rows,
    ),
    rel(
        "pg_stat_progress_copy",
        12333,
        PgCatalog,
        progress::pg_stat_progress_copy_schema,
        no_rows,
    ),
    rel(
        "pg_stat_progress_create_index",
        12324,
        PgCatalog,
        progress::pg_stat_progress_create_index_schema,
        no_rows,
    ),
    rel(
        "pg_stat_progress_vacuum",
        12314,
        PgCatalog,
        progress::pg_stat_progress_vacuum_schema,
        no_rows,
    ),
    rel(
        "pg_stat_replication",
        12231,
        PgCatalog,
        replication::pg_stat_replication_schema,
        no_rows,
    ),
    rel(
        "pg_stat_replication_slots",
        12266,
        PgCatalog,
        replication::pg_stat_replication_slots_schema,
        no_rows,
    ),
    rel(
        "pg_stat_subscription",
        12248,
        PgCatalog,
        replication::pg_stat_subscription_schema,
        no_rows,
    ),
    rel(
        "pg_stat_subscription_stats",
        12347,
        PgCatalog,
        replication::pg_stat_subscription_stats_schema,
        no_rows,
    ),
    rel(
        "pg_stat_wal_receiver",
        12240,
        PgCatalog,
        replication::pg_stat_wal_receiver_schema,
        no_rows,
    ),
    rel(
        "pg_statistic",
        2619,
        PgCatalog,
        statistic::pg_statistic_schema,
        statistic::pg_statistic_rows,
    ),
    rel(
        "pg_statistic_ext",
        3381,
        PgCatalog,
        statistic_ext::pg_statistic_ext_schema,
        no_rows,
    ),
    rel(
        "pg_statistic_ext_data",
        3429,
        PgCatalog,
        statistic_ext::pg_statistic_ext_data_schema,
        no_rows,
    ),
    rel(
        "pg_stats",
        12053,
        PgCatalog,
        statistic::pg_stats_schema,
        statistic::pg_stats_rows,
    ),
    rel(
        "pg_stats_ext",
        12058,
        PgCatalog,
        statistic_ext::pg_stats_ext_schema,
        no_rows,
    ),
    rel(
        "pg_stats_ext_exprs",
        12063,
        PgCatalog,
        statistic_ext::pg_stats_ext_exprs_schema,
        no_rows,
    ),
    rel(
        "pg_subscription",
        6100,
        PgCatalog,
        replication::pg_subscription_schema,
        no_rows,
    ),
    rel(
        "pg_subscription_rel",
        6102,
        PgCatalog,
        replication::pg_subscription_rel_schema,
        no_rows,
    ),
    rel(
        "pg_tables",
        12033,
        PgCatalog,
        relviews::pg_tables_schema,
        relviews::pg_tables_rows,
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
        "pg_trigger",
        2620,
        PgCatalog,
        trigger::pg_trigger_schema,
        no_rows,
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
    rel(
        "pg_user_mapping",
        1418,
        PgCatalog,
        foreign::pg_user_mapping_schema,
        no_rows,
    ),
    rel(
        "pg_user_mappings",
        12338,
        PgCatalog,
        foreign::pg_user_mappings_schema,
        no_rows,
    ),
    rel(
        "pg_views",
        12028,
        PgCatalog,
        relviews::pg_views_schema,
        relviews::pg_views_rows,
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
