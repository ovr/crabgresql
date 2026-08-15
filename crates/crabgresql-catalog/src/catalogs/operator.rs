//! `pg_operator`: PostgreSQL's built-in operators.
//!
//! # The deviation this relation has to state
//!
//! Nothing in this build *reads* this table. Operators are resolved by
//! `crabgresql-binder`'s own code (`expr::operators`), which matches on the
//! syntactic operator and the operand types directly; it has never consulted a
//! catalog and does not start now. So these 805 rows are a description of
//! upstream's operator set, not an inventory of what this server can evaluate —
//! the two overlap, and the overlap is smaller than the table.
//!
//! Publishing the full set is still the right answer, for the reason
//! `pg_opclass` publishes classes for access methods this build has no index
//! for: the row is upstream's statement about what the operator *is* —
//! `<(int4,int4)` is named `<`, returns `bool` and is implemented by
//! `int4lt` — and a client reading `\do`, a `pg_dump` or an "operator is not
//! unique" hint gets a true answer. Shortening the list to what the resolver
//! happens to support would make `pg_operator` disagree with `pg_amop`, which
//! points into it for every strategy of every family.
//!
//! What must not go unstated is the direction that is *not* checkable: the
//! resolver has no enumerable registry, so a test can assert that everything
//! this server evaluates is described here, and cannot assert the converse.
//! See `pg_operator_describes_upstreams_operators` in the crate's tests.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::cols::*;
use crate::oids::*;
use crate::{PG_OPERATOR_ROWS, SystemCatalog};

pub(crate) fn pg_operator_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_operator",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("oprname", PgType::Name),
            col("oprnamespace", PgType::Oid),
            col("oprowner", PgType::Oid),
            col("oprkind", CHARLIKE),
            col("oprcanmerge", PgType::Bool),
            col("oprcanhash", PgType::Bool),
            col("oprleft", PgType::Oid),
            col("oprright", PgType::Oid),
            col("oprresult", PgType::Oid),
            col("oprcom", PgType::Oid),
            col("oprnegate", PgType::Oid),
            col("oprcode", REGPROC),
            col("oprrest", REGPROC),
            col("oprjoin", REGPROC),
        ],
    )
}

pub(crate) fn pg_operator_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    PG_OPERATOR_ROWS
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Text(r.oprname.to_string()),
                Value::Oid(PG_CATALOG_NAMESPACE_OID),
                Value::Oid(BOOTSTRAP_ROLE_OID),
                str_char(r.oprkind),
                Value::Bool(r.oprcanmerge),
                Value::Bool(r.oprcanhash),
                Value::Oid(r.oprleft),
                Value::Oid(r.oprright),
                Value::Oid(r.oprresult),
                Value::Oid(r.oprcom),
                Value::Oid(r.oprnegate),
                regproc(r.oprcode),
                regproc(r.oprrest),
                regproc(r.oprjoin),
            ]
        })
        .collect()
}
