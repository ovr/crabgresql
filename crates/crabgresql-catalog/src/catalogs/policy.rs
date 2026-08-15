//! `pg_policy` and the `pg_policies` view over it: row-level security policies.
//!
//! There is no `CREATE POLICY` and no `ALTER TABLE … ENABLE ROW LEVEL SECURITY`
//! here, so both are empty — as they are in a PostgreSQL database that never
//! defined a policy. `pg_class.relrowsecurity` already reports `false` for every
//! relation, so the two agree: no relation claims RLS, and no policy describes
//! one.
//!
//! psql's `\d <table>` reads `pg_policy` (through `pg_get_expr` on `polqual`) to
//! print the "Policies:" footer, so the join has to resolve even when the answer
//! is nothing.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, oid};

use crate::cols::*;

pub(crate) fn pg_policy_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_policy",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("polname", PgType::Name),
            col("polrelid", PgType::Oid),
            col("polcmd", CHARLIKE),
            col("polpermissive", PgType::Bool),
            col("polroles", PgType::Array(oid::OID)),
            col("polqual", NODE_TREE),
            col("polwithcheck", NODE_TREE),
        ],
    )
}

pub(crate) fn pg_policies_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_policies",
        "pg_catalog",
        vec![
            col("schemaname", PgType::Name),
            col("tablename", PgType::Name),
            col("policyname", PgType::Name),
            col("permissive", PgType::Text),
            col("roles", PgType::Array(oid::NAME)),
            col("cmd", PgType::Text),
            col("qual", PgType::Text),
            col("with_check", PgType::Text),
        ],
    )
}
