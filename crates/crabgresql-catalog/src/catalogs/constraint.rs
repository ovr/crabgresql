//! `pg_constraint`: not-null, check and index-backed constraints.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;

pub(crate) fn pg_constraint_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_constraint",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("conname", PgType::Name),
            col("connamespace", PgType::Oid),
            col("contype", CHARLIKE),
            col("condeferrable", PgType::Bool),
            col("condeferred", PgType::Bool),
            // Present in PostgreSQL 18; `NOT ENFORCED` is refused at DDL, so
            // everything here is enforced.
            col("conenforced", PgType::Bool),
            col("convalidated", PgType::Bool),
            col("conrelid", PgType::Oid),
            col("contypid", PgType::Oid),
            col("conindid", PgType::Oid),
            col("conparentid", PgType::Oid),
            col("confrelid", PgType::Oid),
            col("conislocal", PgType::Bool),
            col("coninhcount", PgType::Int2),
            col("connoinherit", PgType::Bool),
            // TODO: `int2[]` upstream, rendered here as the text an array
            // prints as. `PgType::Array` exists now, so nothing blocks it —
            // the column and the value in `pg_constraint_rows` have to move
            // together.
            col("conkey", PgType::Text),
            // pg_node_tree in PostgreSQL, modelled as the stored SQL text the
            // same way `pg_class.relpartbound` is. `pg_get_expr` re-renders it
            // for the reader.
            col("conbin", PgType::Text),
        ],
    )
}

/// Render the constraints [`crate::SystemCatalog::constraint_oids`] already
/// numbered. Pure: it assigns nothing, so the OID a row reports is the same one
/// `pg_get_constraintdef` resolves against.
pub(crate) fn pg_constraint_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let (constraints, namespace_oids) = (cat.constraint_oids(), cat.namespace_oids());
    let nsp_oid = |namespace: &str| namespace_oids.get(namespace).copied().unwrap_or(2200);
    constraints
        .iter()
        .map(|c| {
            vec![
                Value::Oid(c.oid),
                Value::Text(c.name.clone()),
                Value::Oid(nsp_oid(&c.namespace)),
                str_char(c.contype),
                // condeferrable / condeferred: DEFERRABLE is refused at DDL.
                Value::Bool(false),
                Value::Bool(false),
                // conenforced.
                Value::Bool(true),
                Value::Bool(c.validated),
                Value::Oid(c.table_oid),
                // contypid: domain constraints are not modelled.
                Value::Oid(0),
                Value::Oid(c.index_oid),
                // conparentid: a partition's copied constraint would point at
                // its parent's; partitioned tables carry no checks here.
                Value::Oid(0),
                // confrelid: foreign keys are not supported.
                Value::Oid(0),
                Value::Bool(c.islocal),
                Value::Int2(c.inhcount),
                // connoinherit: `NO INHERIT` has no parser support, so nothing
                // that exists here can be marked with it.
                Value::Bool(false),
                // NULL, not an empty array, when the constraint reads no column
                // — PostgreSQL stores NULL for a predicate like `CHECK (1 > 0)`,
                // so a client testing `conkey IS NULL` agrees. Probed against
                // 18.4.
                match c.columns.is_empty() {
                    true => Value::Null,
                    false => Value::Text(format!(
                        "{{{}}}",
                        c.columns
                            .iter()
                            .map(|column| (*column + 1).to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    )),
                },
                match &c.expr {
                    Some(expr) => Value::Text(expr.clone()),
                    None => Value::Null,
                },
            ]
        })
        .collect()
}
