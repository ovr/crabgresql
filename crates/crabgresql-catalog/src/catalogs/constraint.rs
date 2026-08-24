//! `pg_constraint`: not-null, check and index-backed constraints.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value, oid};

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
            // Present in PostgreSQL 18. TODO: accept `NOT ENFORCED`
            // constraints; DDL refuses them, so everything here is enforced.
            col("conenforced", PgType::Bool),
            col("convalidated", PgType::Bool),
            col("conrelid", PgType::Oid),
            col("contypid", PgType::Oid),
            col("conindid", PgType::Oid),
            col("conparentid", PgType::Oid),
            col("confrelid", PgType::Oid),
            // The three foreign-key action codes. A constraint that is not a
            // foreign key carries a space in each, which is what PostgreSQL
            // stores — not `\0`, and not NULL.
            col("confupdtype", CHARLIKE),
            col("confdeltype", CHARLIKE),
            col("confmatchtype", CHARLIKE),
            col("conislocal", PgType::Bool),
            col("coninhcount", PgType::Int2),
            col("connoinherit", PgType::Bool),
            // conperiod: `PERIOD` (temporal keys) has no production in the
            // parser, so no constraint here is one.
            col("conperiod", PgType::Bool),
            // TODO: `int2[]` upstream, rendered here as the text an array
            // prints as. `PgType::Array` exists now, so nothing blocks it —
            // the column and the value in `pg_constraint_rows` have to move
            // together.
            col("conkey", PgType::Text),
            // The foreign-key and exclusion-constraint detail columns. Every one
            // is NULL here because neither kind of constraint can be created —
            // and NULL is what PostgreSQL stores for them on the kinds that can.
            col("confkey", PgType::Array(oid::INT2)),
            col("conpfeqop", PgType::Array(oid::OID)),
            col("conppeqop", PgType::Array(oid::OID)),
            col("conffeqop", PgType::Array(oid::OID)),
            col("confdelsetcols", PgType::Array(oid::INT2)),
            col("conexclop", PgType::Array(oid::OID)),
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
                // condeferrable / condeferred. TODO: accept `DEFERRABLE` /
                // `INITIALLY DEFERRED` constraints; DDL refuses them, so both
                // are constant false.
                Value::Bool(false),
                Value::Bool(false),
                // conenforced.
                Value::Bool(true),
                Value::Bool(c.validated),
                Value::Oid(c.table_oid),
                // contypid: the domain a domain constraint belongs to, 0 for a
                // table constraint. Exactly one of the two is ever non-zero.
                Value::Oid(c.type_oid),
                Value::Oid(c.index_oid),
                // conparentid. TODO: copy a partitioned parent's `CHECK`
                // constraints into its leaves, each pointing back at the
                // parent's row; a `CHECK` on a partitioned parent is refused at
                // DDL.
                Value::Oid(0),
                // confrelid. TODO: foreign keys; `FOREIGN KEY` is refused at
                // DDL, so no constraint here references another relation.
                Value::Oid(0),
                // confupdtype / confdeltype / confmatchtype.
                chr(' '),
                chr(' '),
                chr(' '),
                Value::Bool(c.islocal),
                Value::Int2(c.inhcount),
                // connoinherit. TODO: `NO INHERIT` constraints; the parser has
                // no production for the clause, so nothing that exists here can
                // be marked with it.
                Value::Bool(false),
                // conperiod.
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
                // confkey / conpfeqop / conppeqop / conffeqop / confdelsetcols /
                // conexclop.
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                match &c.expr {
                    Some(expr) => Value::Text(expr.clone()),
                    None => Value::Null,
                },
            ]
        })
        .collect()
}
