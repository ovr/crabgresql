//! `pg_trigger`: the triggers defined on each relation.
//!
//! crabgresql has no `CREATE TRIGGER`, so this relation is empty — and empty is
//! the *correct* answer, not a placeholder: PostgreSQL returns zero rows here
//! for every database nobody has created a trigger in. What the relation earns
//! by existing is the join: psql's `\d <table>` left-joins `pg_trigger` to list
//! a table's triggers, and a missing relation fails that whole query rather
//! than the trigger section of its output.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::PgType;

use crate::cols::*;

pub(crate) fn pg_trigger_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_trigger",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("tgrelid", PgType::Oid),
            col("tgparentid", PgType::Oid),
            col("tgname", PgType::Name),
            col("tgfoid", PgType::Oid),
            col("tgtype", PgType::Int2),
            col("tgenabled", CHARLIKE),
            col("tgisinternal", PgType::Bool),
            col("tgconstrrelid", PgType::Oid),
            col("tgconstrindid", PgType::Oid),
            col("tgconstraint", PgType::Oid),
            col("tgdeferrable", PgType::Bool),
            col("tginitdeferred", PgType::Bool),
            col("tgnargs", PgType::Int2),
            col("tgattr", INT2VECTOR),
            col("tgargs", PgType::Bytea),
            col("tgqual", NODE_TREE),
            col("tgoldtable", PgType::Name),
            col("tgnewtable", PgType::Name),
        ],
    )
}
