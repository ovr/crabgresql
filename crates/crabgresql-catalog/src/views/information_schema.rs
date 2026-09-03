//! The *shapes* of the SQL-standard `information_schema` views.
//!
//! Only the shapes: each of these views is served by running its own
//! definition, so there is no row builder here to keep in step with one. What a
//! schema still decides is the column **names** a reference to the view exposes
//! before its body is bound — and it is what
//! `every_view_definition_parses_and_names_our_columns` checks the definition
//! text against, so a definition and the name list cannot drift.
//!
//! The types written here are not what a client sees: those are the ones the
//! re-bound query produces. They are stated as the domains the SQL standard
//! gives each column because that is what the definitions cast to, and reading
//! the two side by side is the point.

use crabgresql_storage_api::TableSchema;

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

/// `information_schema.tables`.
///
/// TODO: this reports user relations only, where PostgreSQL also lists the
/// `pg_catalog` and `information_schema` ones. The definition reads `pg_class`,
/// and `pg_class` does not reflect the served relations — so closing the gap is
/// a matter of putting them there, not of anything in this view.
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
