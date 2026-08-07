//! `pg_prepared_statements`: the session's prepared statements.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Reg, RegKind, Value};

use crate::SystemCatalog;
use crate::cols::*;

/// A `regtype[]` column: the array `parameter_types` and `result_types` are.
const REGTYPE_ARRAY: PgType = PgType::Array(crabgresql_types::oid::REGTYPE);

/// `pg_catalog.pg_prepared_statements` — every statement this session has
/// prepared, whether by SQL `PREPARE` (`from_sql` true) or by an extended-query
/// `Parse` message (false). A view over `pg_prepared_statement()` in PostgreSQL;
/// served here as a relation whose rows the session supplies, which is
/// indistinguishable to a client reading it.
///
/// `generic_plans` and `custom_plans` count how each execution was planned.
/// Every execution re-plans here, so the first is always 0 and the second is the
/// execution count — there is no plan cache to choose between.
pub(crate) fn pg_prepared_statements_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_prepared_statements",
        "pg_catalog",
        vec![
            col("name", PgType::Text),
            col("statement", PgType::Text),
            col("prepare_time", PgType::TimestampTz),
            col("parameter_types", REGTYPE_ARRAY),
            col("result_types", REGTYPE_ARRAY),
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
                regtype_array(&stmt.parameter_types),
                // NULL, not an empty array: PostgreSQL leaves `result_types`
                // unset for a statement that returns no rows.
                stmt.result_types
                    .as_deref()
                    .map_or(Value::Null, regtype_array),
                Value::Bool(stmt.from_sql),
                Value::Int8(stmt.generic_plans),
                Value::Int8(stmt.custom_plans),
            ]
        })
        .collect()
}

/// A `regtype[]` datum from type OIDs. A `regtype` prints a built-in under its
/// SQL spelling (23 is `integer`); an OID naming nothing prints as its digits,
/// which is what PostgreSQL renders for a type it cannot name.
fn regtype_array(oids: &[u32]) -> Value {
    Value::Array {
        elem: PgType::Reg(RegKind::Type),
        elems: oids
            .iter()
            .map(|&oid| {
                let reg = match PgType::from_oid(oid) {
                    Some(ty) => Reg {
                        kind: RegKind::Type,
                        oid,
                        name: ty.name().to_string(),
                    },
                    None => Reg::unresolved(RegKind::Type, oid),
                };
                Value::Reg(reg)
            })
            .collect(),
    }
}
