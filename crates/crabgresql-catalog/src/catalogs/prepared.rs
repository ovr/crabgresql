//! `pg_prepared_statements`: the session's prepared statements.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, RegKind, Value};

use crate::SystemCatalog;
use crate::cols::*;

/// `pg_catalog.pg_prepared_statements` — every statement this session has
/// prepared, whether by SQL `PREPARE` (`from_sql` true) or by an extended-query
/// `Parse` message (false). A view over `pg_prepared_statement()` in PostgreSQL;
/// served here as a relation whose rows the session supplies, which is
/// indistinguishable to a client reading it.
///
/// `generic_plans` and `custom_plans` count how each execution was planned.
/// Every execution re-plans here, so the session splits the count by the
/// statement's shape instead of by a plan cache's choice: a parameterless
/// statement counts as generic (PostgreSQL calls it generic from its first
/// execution too), a parameterized one as custom.
pub(crate) fn pg_prepared_statements_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_prepared_statements",
        "pg_catalog",
        vec![
            col("name", PgType::Text),
            col("statement", PgType::Text),
            col("prepare_time", PgType::TimestampTz),
            col("parameter_types", reg_array_type(RegKind::Type)),
            col("result_types", reg_array_type(RegKind::Type)),
            col("from_sql", PgType::Bool),
            col("generic_plans", PgType::Int8),
            col("custom_plans", PgType::Int8),
        ],
    )
}

/// One row per prepared statement, in the order the session enumerated them.
pub(crate) fn pg_prepared_statements_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let prepared = cat.prepared_statements();
    prepared
        .iter()
        .map(|stmt| {
            vec![
                Value::Text(stmt.name.clone()),
                Value::Text(stmt.statement.clone()),
                Value::TimestampTz(stmt.prepare_time),
                regtype_array(cat, &stmt.parameter_types),
                // NULL, not an empty array: PostgreSQL leaves `result_types`
                // unset for a statement that returns no rows.
                stmt.result_types
                    .as_deref()
                    .map_or(Value::Null, |oids| regtype_array(cat, oids)),
                Value::Bool(stmt.from_sql),
                Value::Int8(stmt.generic_plans),
                Value::Int8(stmt.custom_plans),
            ]
        })
        .collect()
}
