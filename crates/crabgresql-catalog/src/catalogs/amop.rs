//! `pg_amop` and `pg_amproc`: the two halves of what an operator family gives
//! its access method — which operator answers each strategy, and which support
//! function the method calls itself.
//!
//! These complete the operator-class stack `pg_opclass`/`pg_opfamily` began.
//! A class alone says a type is indexable; only these say what `<` means to
//! btree strategy 1, and they are what `\dAo` and `\dAp` read.
//!
//! As with `pg_opclass`, every upstream row is published, including those of
//! access methods this build has no index for: the row is a statement about
//! what upstream's method does with the type, not a promise that this server
//! will build the index. What this build *does* consult is its own comparison
//! code — `default_opclass` in [`super::opclass`] is the only catalog fact the
//! index path reads.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::cols::*;
use crate::{PG_AMOP_ROWS, PG_AMPROC_ROWS, SystemCatalog};

pub(crate) fn pg_amop_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_amop",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("amopfamily", PgType::Oid),
            col("amoplefttype", PgType::Oid),
            col("amoprighttype", PgType::Oid),
            col("amopstrategy", PgType::Int2),
            col("amoppurpose", CHARLIKE),
            col("amopopr", PgType::Oid),
            col("amopmethod", PgType::Oid),
            col("amopsortfamily", PgType::Oid),
        ],
    )
}

pub(crate) fn pg_amop_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    PG_AMOP_ROWS
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Oid(r.amopfamily),
                Value::Oid(r.amoplefttype),
                Value::Oid(r.amoprighttype),
                Value::Int2(r.amopstrategy),
                str_char(r.amoppurpose),
                Value::Oid(r.amopopr),
                Value::Oid(r.amopmethod),
                Value::Oid(r.amopsortfamily),
            ]
        })
        .collect()
}

pub(crate) fn pg_amproc_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_amproc",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("amprocfamily", PgType::Oid),
            col("amproclefttype", PgType::Oid),
            col("amprocrighttype", PgType::Oid),
            col("amprocnum", PgType::Int2),
            col("amproc", REGPROC),
        ],
    )
}

pub(crate) fn pg_amproc_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    PG_AMPROC_ROWS
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Oid(r.amprocfamily),
                Value::Oid(r.amproclefttype),
                Value::Oid(r.amprocrighttype),
                Value::Int2(r.amprocnum),
                regproc(r.amproc),
            ]
        })
        .collect()
}
