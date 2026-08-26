//! The table of served relations, split the way PostgreSQL splits them:
//! [`CATALOG_RELATIONS`] holds the bootstrap catalogs, [`CATALOG_VIEWS`] the
//! relations `initdb` creates as views. Both are read through [`lookup`] and
//! [`all`], so nothing outside this module has to know which list an entry is in
//! — the split exists because a view has something a table does not, its
//! definition text.
//!
//! Everything that used to be stated three times — a `match` arm in
//! `build_pg_catalog`, an entry in the OID table, and a name in the test that
//! kept the two in step — is stated once here. "Served" and "has a fixed OID"
//! are the same set *by construction*, so [`crate::SystemCatalog::has_catalog_relation`]
//! can answer from the OID alone and no test has to carry a second copy of the
//! list.
//!
//! Adding a relation is a module under [`crate::catalogs`] publishing the pair
//! below, plus one line in [`CATALOG_RELATIONS`] — or, for a relation
//! PostgreSQL defines as a view, one line in [`CATALOG_VIEWS`] and its
//! definition in [`crate::views::definitions`].
//!
//! The single `rows` signature is what keeps that true: a relation gathers what
//! it needs from the snapshot itself, so a new one with unusual inputs adds no
//! argument to any shared call site.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::Value;

use crate::SystemCatalog;
use crate::catalogs::{
    acl, aggregate, am, amop, attribute, auth, class, collation, constraint, conversion, cursors,
    database, depend, description, extension, foreign, index, inherits, language, locks,
    misc_empty, namespace, opclass, operator, policy, prepared, proc, progress, publication,
    relviews, replication, rewrite, sequence, settings, stat_activity, stat_database, stat_gssapi,
    stat_indexes, stat_io, stat_ssl, stat_tables, statio, statistic, statistic_ext, textsearch,
    timezone, trigger, types,
};
use crate::cols::no_rows;
use crate::views::{definitions, information_schema};

/// Which schema a served relation lives in. Ordered as declared, because both
/// registry tables are sorted on it.
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
    /// TODO: reflect served relations in `pg_class` so their fixed OIDs join to
    /// catalog rows.
    pub(crate) oid: u32,
    pub(crate) namespace: CatalogNamespace,
    pub(crate) schema: fn() -> TableSchema,
    pub(crate) rows: fn(&SystemCatalog) -> Vec<Vec<Value>>,
    /// Whether the rows are built when the relation is first *read* rather than
    /// when the binder resolves its name. True for `pg_locks` and `pg_depend`,
    /// for two different reasons; see
    /// [`crate::static_table::StaticTable::deferred`].
    pub(crate) deferred: bool,
    /// For a relation whose every row *describes another relation*: the column
    /// holding that relation's OID. Set it and each row reports the described
    /// relation's own DDL generation as its `xmin`, instead of the catalog-wide
    /// one — so `CREATE TABLE b` stops looking like it changed `a`.
    ///
    /// `None` for a relation whose rows describe something else (a type, a
    /// function, a setting): there is no per-object generation to report, and
    /// those keep the catalog-wide value.
    ///
    /// Nothing checks the name against the schema at compile time, so a typo
    /// silently degrades that relation to the catalog-wide generation — which
    /// `xmin_columns_exist` in this crate's tests is here to catch.
    pub(crate) xmin_column: Option<&'static str>,
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
        deferred: false,
        xmin_column: None,
    }
}

/// [`rel`] for a relation whose rows describe other relations; see
/// [`CatalogRelDef::xmin_column`].
const fn rel_of_relation(
    name: &'static str,
    oid: u32,
    namespace: CatalogNamespace,
    schema: fn() -> TableSchema,
    rows: fn(&SystemCatalog) -> Vec<Vec<Value>>,
    xmin_column: &'static str,
) -> CatalogRelDef {
    CatalogRelDef {
        xmin_column: Some(xmin_column),
        ..rel(name, oid, namespace, schema, rows)
    }
}

/// See [`CatalogRelDef::deferred`].
const fn rel_deferred(
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
        deferred: true,
        xmin_column: None,
    }
}

/// The relation part is *embedded* rather than repeated, so a view is deferred
/// or reports a per-relation `xmin` by exactly the same fields a table does —
/// `pg_locks` is a view here and needs [`CatalogRelDef::deferred`].
pub(crate) struct CatalogViewDef {
    pub(crate) rel: CatalogRelDef,
    /// Verbatim as PostgreSQL 18.4's `pg_get_viewdef` prints it; see
    /// [`crate::views::definitions`].
    pub(crate) definition: &'static str,
}

const fn view(
    name: &'static str,
    oid: u32,
    namespace: CatalogNamespace,
    schema: fn() -> TableSchema,
    rows: fn(&SystemCatalog) -> Vec<Vec<Value>>,
    definition: &'static str,
) -> CatalogViewDef {
    CatalogViewDef {
        rel: rel(name, oid, namespace, schema, rows),
        definition,
    }
}

/// [`view`] for a view whose rows are built at read time; see
/// [`CatalogRelDef::deferred`].
const fn view_deferred(
    name: &'static str,
    oid: u32,
    namespace: CatalogNamespace,
    schema: fn() -> TableSchema,
    rows: fn(&SystemCatalog) -> Vec<Vec<Value>>,
    definition: &'static str,
) -> CatalogViewDef {
    CatalogViewDef {
        rel: rel_deferred(name, oid, namespace, schema, rows),
        definition,
    }
}

use CatalogNamespace::{InformationSchema, PgCatalog};

/// Every *bootstrap catalog* this build serves — all 64 of PostgreSQL's —
/// sorted by `(namespace, name)` so [`lookup`] can binary-search it.
/// `the_registry_is_sorted_and_its_names_are_unique` fails if a new entry lands
/// out of order, or if a name appears in [`CATALOG_VIEWS`] as well.
///
/// The OIDs are PostgreSQL's own assignments rather than invented values, so an
/// OID a client hard-codes means the same thing here. The 12000 band belongs to
/// the views, which is where [`CATALOG_VIEWS`] documents it.
pub(crate) static CATALOG_RELATIONS: &[CatalogRelDef] = &[
    rel(
        "pg_aggregate",
        2600,
        PgCatalog,
        aggregate::pg_aggregate_schema,
        aggregate::pg_aggregate_rows,
    ),
    rel("pg_am", 2601, PgCatalog, am::pg_am_schema, am::pg_am_rows),
    rel(
        "pg_amop",
        2602,
        PgCatalog,
        amop::pg_amop_schema,
        amop::pg_amop_rows,
    ),
    rel(
        "pg_amproc",
        2603,
        PgCatalog,
        amop::pg_amproc_schema,
        amop::pg_amproc_rows,
    ),
    rel_of_relation(
        "pg_attrdef",
        2604,
        PgCatalog,
        attribute::pg_attrdef_schema,
        attribute::pg_attrdef_rows,
        "adrelid",
    ),
    rel_of_relation(
        "pg_attribute",
        1249,
        PgCatalog,
        attribute::pg_attribute_schema,
        attribute::pg_attribute_rows,
        "attrelid",
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
    rel_of_relation(
        "pg_class",
        1259,
        PgCatalog,
        class::pg_class_schema,
        class::pg_class_rows,
        "oid",
    ),
    rel(
        "pg_collation",
        3456,
        PgCatalog,
        collation::pg_collation_schema,
        collation::pg_collation_rows,
    ),
    rel_of_relation(
        "pg_constraint",
        2606,
        PgCatalog,
        constraint::pg_constraint_schema,
        constraint::pg_constraint_rows,
        "conrelid",
    ),
    rel(
        "pg_conversion",
        2607,
        PgCatalog,
        conversion::pg_conversion_schema,
        conversion::pg_conversion_rows,
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
    rel_deferred(
        "pg_depend",
        2608,
        PgCatalog,
        depend::pg_depend_schema,
        depend::pg_depend_rows,
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
        "pg_event_trigger",
        3466,
        PgCatalog,
        misc_empty::pg_event_trigger_schema,
        no_rows,
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
    rel_of_relation(
        "pg_index",
        2610,
        PgCatalog,
        index::pg_index_schema,
        index::pg_index_rows,
        "indrelid",
    ),
    rel_of_relation(
        "pg_inherits",
        2611,
        PgCatalog,
        inherits::pg_inherits_schema,
        inherits::pg_inherits_rows,
        "inhrelid",
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
        "pg_largeobject",
        2613,
        PgCatalog,
        misc_empty::pg_largeobject_schema,
        no_rows,
    ),
    rel(
        "pg_largeobject_metadata",
        2995,
        PgCatalog,
        misc_empty::pg_largeobject_metadata_schema,
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
        "pg_operator",
        2617,
        PgCatalog,
        operator::pg_operator_schema,
        operator::pg_operator_rows,
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
        "pg_policy",
        3256,
        PgCatalog,
        policy::pg_policy_schema,
        no_rows,
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
        "pg_range",
        3541,
        PgCatalog,
        misc_empty::pg_range_schema,
        no_rows,
    ),
    rel(
        "pg_replication_origin",
        6000,
        PgCatalog,
        replication::pg_replication_origin_schema,
        no_rows,
    ),
    rel_of_relation(
        "pg_rewrite",
        2618,
        PgCatalog,
        rewrite::pg_rewrite_schema,
        rewrite::pg_rewrite_rows,
        "ev_class",
    ),
    rel(
        "pg_seclabel",
        3596,
        PgCatalog,
        acl::pg_seclabel_schema,
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
        "pg_tablespace",
        1213,
        PgCatalog,
        database::pg_tablespace_schema,
        database::pg_tablespace_rows,
    ),
    rel(
        "pg_transform",
        3576,
        PgCatalog,
        misc_empty::pg_transform_schema,
        no_rows,
    ),
    rel_of_relation(
        "pg_trigger",
        2620,
        PgCatalog,
        trigger::pg_trigger_schema,
        no_rows,
        "tgrelid",
    ),
    rel(
        "pg_ts_config",
        3602,
        PgCatalog,
        textsearch::pg_ts_config_schema,
        textsearch::pg_ts_config_rows,
    ),
    rel(
        "pg_ts_config_map",
        3603,
        PgCatalog,
        textsearch::pg_ts_config_map_schema,
        textsearch::pg_ts_config_map_rows,
    ),
    rel(
        "pg_ts_dict",
        3600,
        PgCatalog,
        textsearch::pg_ts_dict_schema,
        textsearch::pg_ts_dict_rows,
    ),
    rel(
        "pg_ts_parser",
        3601,
        PgCatalog,
        textsearch::pg_ts_parser_schema,
        textsearch::pg_ts_parser_rows,
    ),
    rel(
        "pg_ts_template",
        3764,
        PgCatalog,
        textsearch::pg_ts_template_schema,
        textsearch::pg_ts_template_rows,
    ),
    rel(
        "pg_type",
        1247,
        PgCatalog,
        types::pg_type_schema,
        types::pg_type_rows,
    ),
    rel(
        "pg_user_mapping",
        1418,
        PgCatalog,
        foreign::pg_user_mapping_schema,
        no_rows,
    ),
];

/// Every relation this build serves that PostgreSQL defines as a *view*, in the
/// same `(namespace, name)` order — 63 of `pg_catalog`'s 80, plus the four
/// `information_schema` entries.
///
/// The `pg_catalog` OIDs sit in the 12000 band because `initdb` creates these
/// relations rather than bootstrapping them; they are deterministic for a given
/// major version, and all were probed from the same 18.4 — `pg_cursors` reading
/// back 12077 there is what confirms the whole band came from one `initdb`.
/// The `information_schema` views are the same kind of assignment one band
/// higher, `initdb` running `information_schema.sql` after the catalog views.
/// Probed from a freshly created database rather than a developer's
/// `template1`, where a `CREATE OR REPLACE VIEW` had renumbered `tables` into
/// the user band.
///
/// Every one is still a Rust row builder: the `definition` is not what produces
/// the rows, it is what a later change re-parses to produce them. Until then it
/// is checked against the row builder rather than trusted — see
/// [`crate::views::definitions`].
pub(crate) static CATALOG_VIEWS: &[CatalogViewDef] = &[
    view(
        "columns",
        13787,
        InformationSchema,
        information_schema::columns_schema,
        information_schema::columns_rows,
        definitions::information_schema::COLUMNS,
    ),
    view(
        "domains",
        13811,
        InformationSchema,
        information_schema::domains_schema,
        information_schema::domains_rows,
        definitions::information_schema::DOMAINS,
    ),
    view(
        "schemata",
        13873,
        InformationSchema,
        information_schema::schemata_schema,
        information_schema::schemata_rows,
        definitions::information_schema::SCHEMATA,
    ),
    view(
        "tables",
        13916,
        InformationSchema,
        information_schema::tables_schema,
        information_schema::tables_rows,
        definitions::information_schema::TABLES,
    ),
    view(
        "pg_available_extension_versions",
        12085,
        PgCatalog,
        extension::pg_available_extension_versions_schema,
        extension::pg_available_extension_versions_rows,
        definitions::pg_catalog::PG_AVAILABLE_EXTENSION_VERSIONS,
    ),
    view(
        "pg_available_extensions",
        12081,
        PgCatalog,
        extension::pg_available_extensions_schema,
        extension::pg_available_extensions_rows,
        definitions::pg_catalog::PG_AVAILABLE_EXTENSIONS,
    ),
    view(
        "pg_cursors",
        12077,
        PgCatalog,
        cursors::pg_cursors_schema,
        cursors::pg_cursors_rows,
        definitions::pg_catalog::PG_CURSORS,
    ),
    view(
        "pg_group",
        12010,
        PgCatalog,
        auth::pg_group_schema,
        auth::pg_group_rows,
        definitions::pg_catalog::PG_GROUP,
    ),
    view(
        "pg_indexes",
        12043,
        PgCatalog,
        relviews::pg_indexes_schema,
        relviews::pg_indexes_rows,
        definitions::pg_catalog::PG_INDEXES,
    ),
    view_deferred(
        "pg_locks",
        12073,
        PgCatalog,
        locks::pg_locks_schema,
        locks::pg_locks_rows,
        definitions::pg_catalog::PG_LOCKS,
    ),
    view(
        "pg_matviews",
        12038,
        PgCatalog,
        relviews::pg_matviews_schema,
        no_rows,
        definitions::pg_catalog::PG_MATVIEWS,
    ),
    view(
        "pg_policies",
        12018,
        PgCatalog,
        policy::pg_policies_schema,
        no_rows,
        definitions::pg_catalog::PG_POLICIES,
    ),
    view(
        "pg_prepared_statements",
        12095,
        PgCatalog,
        prepared::pg_prepared_statements_schema,
        prepared::pg_prepared_statements_rows,
        definitions::pg_catalog::PG_PREPARED_STATEMENTS,
    ),
    view(
        "pg_prepared_xacts",
        12090,
        PgCatalog,
        misc_empty::pg_prepared_xacts_schema,
        no_rows,
        definitions::pg_catalog::PG_PREPARED_XACTS,
    ),
    view(
        "pg_publication_tables",
        12068,
        PgCatalog,
        publication::pg_publication_tables_schema,
        no_rows,
        definitions::pg_catalog::PG_PUBLICATION_TABLES,
    ),
    view(
        "pg_replication_origin_status",
        12343,
        PgCatalog,
        replication::pg_replication_origin_status_schema,
        no_rows,
        definitions::pg_catalog::PG_REPLICATION_ORIGIN_STATUS,
    ),
    view(
        "pg_replication_slots",
        12261,
        PgCatalog,
        replication::pg_replication_slots_schema,
        no_rows,
        definitions::pg_catalog::PG_REPLICATION_SLOTS,
    ),
    view(
        "pg_roles",
        12000,
        PgCatalog,
        auth::pg_roles_schema,
        auth::pg_roles_rows,
        definitions::pg_catalog::PG_ROLES,
    ),
    view(
        "pg_rules",
        12023,
        PgCatalog,
        rewrite::pg_rules_schema,
        no_rows,
        definitions::pg_catalog::PG_RULES,
    ),
    view(
        "pg_seclabels",
        12099,
        PgCatalog,
        acl::pg_seclabels_schema,
        no_rows,
        definitions::pg_catalog::PG_SECLABELS,
    ),
    view(
        "pg_sequences",
        12048,
        PgCatalog,
        relviews::pg_sequences_schema,
        relviews::pg_sequences_rows,
        definitions::pg_catalog::PG_SEQUENCES,
    ),
    view(
        "pg_settings",
        12104,
        PgCatalog,
        settings::pg_settings_schema,
        settings::pg_settings_rows,
        definitions::pg_catalog::PG_SETTINGS,
    ),
    view(
        "pg_shadow",
        12005,
        PgCatalog,
        auth::pg_shadow_schema,
        auth::pg_shadow_rows,
        definitions::pg_catalog::PG_SHADOW,
    ),
    view(
        "pg_stat_activity",
        12226,
        PgCatalog,
        stat_activity::pg_stat_activity_schema,
        stat_activity::pg_stat_activity_rows,
        definitions::pg_catalog::PG_STAT_ACTIVITY,
    ),
    view(
        "pg_stat_all_indexes",
        12187,
        PgCatalog,
        stat_indexes::pg_stat_all_indexes_schema,
        stat_indexes::pg_stat_all_indexes_rows,
        definitions::pg_catalog::PG_STAT_ALL_INDEXES,
    ),
    view(
        "pg_stat_all_tables",
        12146,
        PgCatalog,
        stat_tables::pg_stat_all_tables_schema,
        stat_tables::pg_stat_all_tables_rows,
        definitions::pg_catalog::PG_STAT_ALL_TABLES,
    ),
    view(
        "pg_stat_database",
        12270,
        PgCatalog,
        stat_database::pg_stat_database_schema,
        stat_database::pg_stat_database_rows,
        definitions::pg_catalog::PG_STAT_DATABASE,
    ),
    view(
        "pg_stat_database_conflicts",
        12275,
        PgCatalog,
        stat_database::pg_stat_database_conflicts_schema,
        stat_database::pg_stat_database_conflicts_rows,
        definitions::pg_catalog::PG_STAT_DATABASE_CONFLICTS,
    ),
    view(
        "pg_stat_gssapi",
        12257,
        PgCatalog,
        stat_gssapi::pg_stat_gssapi_schema,
        stat_gssapi::pg_stat_gssapi_rows,
        definitions::pg_catalog::PG_STAT_GSSAPI,
    ),
    view(
        "pg_stat_io",
        12301,
        PgCatalog,
        stat_io::pg_stat_io_schema,
        no_rows,
        definitions::pg_catalog::PG_STAT_IO,
    ),
    view(
        "pg_stat_progress_analyze",
        12309,
        PgCatalog,
        progress::pg_stat_progress_analyze_schema,
        no_rows,
        definitions::pg_catalog::PG_STAT_PROGRESS_ANALYZE,
    ),
    view(
        "pg_stat_progress_basebackup",
        12329,
        PgCatalog,
        progress::pg_stat_progress_basebackup_schema,
        no_rows,
        definitions::pg_catalog::PG_STAT_PROGRESS_BASEBACKUP,
    ),
    view(
        "pg_stat_progress_cluster",
        12319,
        PgCatalog,
        progress::pg_stat_progress_cluster_schema,
        no_rows,
        definitions::pg_catalog::PG_STAT_PROGRESS_CLUSTER,
    ),
    view(
        "pg_stat_progress_copy",
        12333,
        PgCatalog,
        progress::pg_stat_progress_copy_schema,
        no_rows,
        definitions::pg_catalog::PG_STAT_PROGRESS_COPY,
    ),
    view(
        "pg_stat_progress_create_index",
        12324,
        PgCatalog,
        progress::pg_stat_progress_create_index_schema,
        no_rows,
        definitions::pg_catalog::PG_STAT_PROGRESS_CREATE_INDEX,
    ),
    view(
        "pg_stat_progress_vacuum",
        12314,
        PgCatalog,
        progress::pg_stat_progress_vacuum_schema,
        no_rows,
        definitions::pg_catalog::PG_STAT_PROGRESS_VACUUM,
    ),
    view(
        "pg_stat_replication",
        12231,
        PgCatalog,
        replication::pg_stat_replication_schema,
        no_rows,
        definitions::pg_catalog::PG_STAT_REPLICATION,
    ),
    view(
        "pg_stat_replication_slots",
        12266,
        PgCatalog,
        replication::pg_stat_replication_slots_schema,
        no_rows,
        definitions::pg_catalog::PG_STAT_REPLICATION_SLOTS,
    ),
    view(
        "pg_stat_ssl",
        12253,
        PgCatalog,
        stat_ssl::pg_stat_ssl_schema,
        stat_ssl::pg_stat_ssl_rows,
        definitions::pg_catalog::PG_STAT_SSL,
    ),
    view(
        "pg_stat_subscription",
        12248,
        PgCatalog,
        replication::pg_stat_subscription_schema,
        no_rows,
        definitions::pg_catalog::PG_STAT_SUBSCRIPTION,
    ),
    view(
        "pg_stat_subscription_stats",
        12347,
        PgCatalog,
        replication::pg_stat_subscription_stats_schema,
        no_rows,
        definitions::pg_catalog::PG_STAT_SUBSCRIPTION_STATS,
    ),
    view(
        "pg_stat_sys_indexes",
        12192,
        PgCatalog,
        stat_indexes::pg_stat_sys_indexes_schema,
        stat_indexes::pg_stat_sys_indexes_rows,
        definitions::pg_catalog::PG_STAT_SYS_INDEXES,
    ),
    view(
        "pg_stat_sys_tables",
        12156,
        PgCatalog,
        stat_tables::pg_stat_sys_tables_schema,
        stat_tables::pg_stat_sys_tables_rows,
        definitions::pg_catalog::PG_STAT_SYS_TABLES,
    ),
    view(
        "pg_stat_user_indexes",
        12196,
        PgCatalog,
        stat_indexes::pg_stat_user_indexes_schema,
        stat_indexes::pg_stat_user_indexes_rows,
        definitions::pg_catalog::PG_STAT_USER_INDEXES,
    ),
    view(
        "pg_stat_user_tables",
        12165,
        PgCatalog,
        stat_tables::pg_stat_user_tables_schema,
        stat_tables::pg_stat_user_tables_rows,
        definitions::pg_catalog::PG_STAT_USER_TABLES,
    ),
    view(
        "pg_stat_wal_receiver",
        12240,
        PgCatalog,
        replication::pg_stat_wal_receiver_schema,
        no_rows,
        definitions::pg_catalog::PG_STAT_WAL_RECEIVER,
    ),
    view(
        "pg_stat_xact_all_tables",
        12151,
        PgCatalog,
        stat_tables::pg_stat_xact_all_tables_schema,
        no_rows,
        definitions::pg_catalog::PG_STAT_XACT_ALL_TABLES,
    ),
    view(
        "pg_stat_xact_sys_tables",
        12161,
        PgCatalog,
        stat_tables::pg_stat_xact_sys_tables_schema,
        no_rows,
        definitions::pg_catalog::PG_STAT_XACT_SYS_TABLES,
    ),
    view(
        "pg_stat_xact_user_tables",
        12170,
        PgCatalog,
        stat_tables::pg_stat_xact_user_tables_schema,
        no_rows,
        definitions::pg_catalog::PG_STAT_XACT_USER_TABLES,
    ),
    view(
        "pg_statio_all_indexes",
        12200,
        PgCatalog,
        statio::pg_statio_all_indexes_schema,
        statio::pg_statio_all_indexes_rows,
        definitions::pg_catalog::PG_STATIO_ALL_INDEXES,
    ),
    view(
        "pg_statio_all_sequences",
        12213,
        PgCatalog,
        statio::pg_statio_all_sequences_schema,
        statio::pg_statio_all_sequences_rows,
        definitions::pg_catalog::PG_STATIO_ALL_SEQUENCES,
    ),
    view(
        "pg_statio_all_tables",
        12174,
        PgCatalog,
        statio::pg_statio_all_tables_schema,
        statio::pg_statio_all_tables_rows,
        definitions::pg_catalog::PG_STATIO_ALL_TABLES,
    ),
    view(
        "pg_statio_sys_indexes",
        12205,
        PgCatalog,
        statio::pg_statio_sys_indexes_schema,
        statio::pg_statio_sys_indexes_rows,
        definitions::pg_catalog::PG_STATIO_SYS_INDEXES,
    ),
    view(
        "pg_statio_sys_sequences",
        12218,
        PgCatalog,
        statio::pg_statio_sys_sequences_schema,
        statio::pg_statio_sys_sequences_rows,
        definitions::pg_catalog::PG_STATIO_SYS_SEQUENCES,
    ),
    view(
        "pg_statio_sys_tables",
        12179,
        PgCatalog,
        statio::pg_statio_sys_tables_schema,
        statio::pg_statio_sys_tables_rows,
        definitions::pg_catalog::PG_STATIO_SYS_TABLES,
    ),
    view(
        "pg_statio_user_indexes",
        12209,
        PgCatalog,
        statio::pg_statio_user_indexes_schema,
        statio::pg_statio_user_indexes_rows,
        definitions::pg_catalog::PG_STATIO_USER_INDEXES,
    ),
    view(
        "pg_statio_user_sequences",
        12222,
        PgCatalog,
        statio::pg_statio_user_sequences_schema,
        statio::pg_statio_user_sequences_rows,
        definitions::pg_catalog::PG_STATIO_USER_SEQUENCES,
    ),
    view(
        "pg_statio_user_tables",
        12183,
        PgCatalog,
        statio::pg_statio_user_tables_schema,
        statio::pg_statio_user_tables_rows,
        definitions::pg_catalog::PG_STATIO_USER_TABLES,
    ),
    view(
        "pg_stats",
        12053,
        PgCatalog,
        statistic::pg_stats_schema,
        statistic::pg_stats_rows,
        definitions::pg_catalog::PG_STATS,
    ),
    view(
        "pg_stats_ext",
        12058,
        PgCatalog,
        statistic_ext::pg_stats_ext_schema,
        no_rows,
        definitions::pg_catalog::PG_STATS_EXT,
    ),
    view(
        "pg_stats_ext_exprs",
        12063,
        PgCatalog,
        statistic_ext::pg_stats_ext_exprs_schema,
        no_rows,
        definitions::pg_catalog::PG_STATS_EXT_EXPRS,
    ),
    view(
        "pg_tables",
        12033,
        PgCatalog,
        relviews::pg_tables_schema,
        relviews::pg_tables_rows,
        definitions::pg_catalog::PG_TABLES,
    ),
    view(
        "pg_timezone_abbrevs",
        12122,
        PgCatalog,
        timezone::pg_timezone_abbrevs_schema,
        timezone::pg_timezone_abbrevs_rows,
        definitions::pg_catalog::PG_TIMEZONE_ABBREVS,
    ),
    view(
        "pg_timezone_names",
        12126,
        PgCatalog,
        timezone::pg_timezone_names_schema,
        timezone::pg_timezone_names_rows,
        definitions::pg_catalog::PG_TIMEZONE_NAMES,
    ),
    view(
        "pg_user",
        12014,
        PgCatalog,
        auth::pg_user_schema,
        auth::pg_user_rows,
        definitions::pg_catalog::PG_USER,
    ),
    view(
        "pg_user_mappings",
        12338,
        PgCatalog,
        foreign::pg_user_mappings_schema,
        no_rows,
        definitions::pg_catalog::PG_USER_MAPPINGS,
    ),
    view(
        "pg_views",
        12028,
        PgCatalog,
        relviews::pg_views_schema,
        relviews::pg_views_rows,
        definitions::pg_catalog::PG_VIEWS,
    ),
];

/// The definition of `namespace.name`, or `None` if this build serves no such
/// relation. A caller that does not care whether the relation is a table or a
/// view — which is every caller outside this module — asks here.
pub(crate) fn lookup(namespace: CatalogNamespace, name: &str) -> Option<&'static CatalogRelDef> {
    let key = |def: &CatalogRelDef| (def.namespace, def.name).cmp(&(namespace, name));
    if let Ok(i) = CATALOG_RELATIONS.binary_search_by(key) {
        return Some(&CATALOG_RELATIONS[i]);
    }
    CATALOG_VIEWS
        .binary_search_by(|def| key(&def.rel))
        .ok()
        .map(|i| &CATALOG_VIEWS[i].rel)
}

/// `None` for a name this build serves as a base table, as well as for one it
/// does not serve at all.
pub(crate) fn view_definition(
    namespace: CatalogNamespace,
    name: &str,
) -> Option<&'static CatalogViewDef> {
    CATALOG_VIEWS
        .binary_search_by(|def| (def.rel.namespace, def.rel.name).cmp(&(namespace, name)))
        .ok()
        .map(|i| &CATALOG_VIEWS[i])
}

/// Every served relation of both tables, tables before views. Unordered as a
/// whole — each table is sorted, the concatenation is not — so a caller that
/// needs an order sorts for itself.
pub(crate) fn all() -> impl Iterator<Item = &'static CatalogRelDef> {
    CATALOG_RELATIONS
        .iter()
        .chain(CATALOG_VIEWS.iter().map(|def| &def.rel))
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
    builtin_relation_ref(oid)
        .filter(|(namespace, _)| *namespace == "pg_catalog")
        .map(|(_, name)| name)
}

fn catalog_namespace(namespace: &str) -> Option<CatalogNamespace> {
    match namespace {
        "pg_catalog" => Some(CatalogNamespace::PgCatalog),
        "information_schema" => Some(CatalogNamespace::InformationSchema),
        _ => None,
    }
}

/// Kept separate from [`builtin_relation_oid`] because the lookups are not
/// interchangeable.
///
/// `builtin_relation_oid` answers for `pg_catalog` alone because its callers ask
/// a second question with it: whether an unqualified name reaches a catalog
/// relation rather than a user one. `information_schema` is off the search path,
/// so a user table named `tables` is reached unqualified and would be wrongly
/// reported as shadowed if that lookup spanned both schemas. Here the schema is
/// given, so there is nothing to shadow.
pub fn builtin_relation_oid_in(namespace: &str, name: &str) -> Option<u32> {
    lookup(catalog_namespace(namespace)?, name).map(|def| def.oid)
}

/// `InvalidOid` is never an object to ask about, so 0 answers nothing even
/// though no entry claims it.
pub fn builtin_relation_ref(oid: u32) -> Option<(&'static str, &'static str)> {
    // Linear, unlike the by-name direction: both tables are sorted by name, and
    // a second sorted-by-OID copy would be one more thing to keep in step.
    all()
        .find(|def| def.oid == oid && oid != 0)
        .map(|def| match def.namespace {
            CatalogNamespace::PgCatalog => ("pg_catalog", def.name),
            CatalogNamespace::InformationSchema => ("information_schema", def.name),
        })
}

/// The `pg_catalog` relations `initdb` keeps *closed* to PUBLIC — everything a
/// stock cluster answers `has_table_privilege(<some role>, …, 'SELECT')` with
/// `false` for. The rest of the catalog is world-readable, which is why
/// [`public_reads`] takes this list as the exception rather than the rule.
///
/// PostgreSQL 18.4 closes sixteen relations; the eight here are the ones this
/// build serves, probed with
/// `SELECT relname FROM pg_class WHERE relnamespace = 11 AND relkind IN ('r','v')
/// AND NOT has_table_privilege('pg_signal_backend', oid, 'SELECT')`. They are
/// closed because they hold password verifiers (`pg_authid`, `pg_shadow`),
/// planner statistics that would leak table contents (`pg_statistic`,
/// `pg_statistic_ext_data`), connection strings (`pg_subscription`,
/// `pg_user_mapping`), replication state, or large-object data.
///
/// Sorted, and every name must be one this build serves — both are asserted by
/// `restricted_catalogs_are_sorted_and_served`, so a typo cannot quietly open a
/// catalog by naming a relation that does not exist.
pub(crate) static RESTRICTED_CATALOGS: &[&str] = &[
    "pg_authid",
    "pg_largeobject",
    "pg_replication_origin_status",
    "pg_shadow",
    "pg_statistic",
    "pg_statistic_ext_data",
    "pg_subscription",
    "pg_user_mapping",
];

pub(crate) fn public_reads(name: &str) -> bool {
    RESTRICTED_CATALOGS.binary_search(&name).is_err()
}

/// The SQL that defines the `pg_catalog` view `name`, as PostgreSQL 18.4 prints
/// it. The rows of that view are built by Rust rather than by this SQL; see
/// [`crate::views::definitions`] for what the text is for.
pub fn builtin_view_definition(name: &str) -> Option<&'static str> {
    view_definition(CatalogNamespace::PgCatalog, name).map(|def| def.definition)
}
