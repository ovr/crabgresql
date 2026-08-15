//! `pg_rewrite` and the `pg_rules` view over it: the query-rewrite rules.
//!
//! `pg_rules` shows only the rules a user wrote with `CREATE RULE`, and there is
//! no `CREATE RULE` here, so it is empty — as it is in a PostgreSQL database
//! nobody has defined a rule in. (PostgreSQL's own two rows on a fresh cluster
//! belong to system views, and its `pg_rules` filters those out by name.)
//!
//! `pg_rewrite` itself is *not* only about rules: every view carries a
//! `_RETURN` rule, and that rule is where PostgreSQL stores the view's body.
//! That is also why `pg_class.relhasrules` is true for a view and false for
//! everything else here — the two agree by construction.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;

pub(crate) fn pg_rewrite_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_rewrite",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("rulename", PgType::Name),
            col("ev_class", PgType::Oid),
            col("ev_type", CHARLIKE),
            col("ev_enabled", CHARLIKE),
            col("is_instead", PgType::Bool),
            col("ev_qual", NODE_TREE),
            col("ev_action", NODE_TREE),
        ],
    )
}

/// One `_RETURN` row per view, with the constants PostgreSQL 18.4 was observed
/// to store for one: `ev_type = '1'` (SELECT), `ev_enabled = 'O'` (origin),
/// `is_instead`, and an `ev_qual` of `<>` — the empty node tree, which is not
/// the same as NULL.
///
/// `ev_action` is the deparsed body rather than a node tree, the model this
/// build uses for every `pg_node_tree` column (see [`NODE_TREE`]), and NULL for
/// a view [`crate::CatalogRelation::definition`] could not render.
pub(crate) fn pg_rewrite_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    cat.rewrite_oids()
        .into_iter()
        .map(|rule| {
            vec![
                Value::Oid(rule.oid),
                Value::Text("_RETURN".to_string()),
                Value::Oid(rule.view_oid),
                chr('1'),
                chr('O'),
                Value::Bool(true),
                Value::Text("<>".to_string()),
                match rule.definition {
                    Some(sql) => Value::Text(sql),
                    None => Value::Null,
                },
            ]
        })
        .collect()
}

pub(crate) fn pg_rules_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_rules",
        "pg_catalog",
        vec![
            col("schemaname", PgType::Name),
            col("tablename", PgType::Name),
            col("rulename", PgType::Name),
            col("definition", PgType::Text),
        ],
    )
}
