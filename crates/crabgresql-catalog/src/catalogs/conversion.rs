//! `pg_conversion`: the built-in encoding conversions.
//!
//! # The deviation
//!
//! This server speaks UTF-8 and nothing else. Every one of these 98 rows names
//! a conversion between two encodings a connection here can never be in, so
//! none of them will ever run — the widest gap of the wave, and the reason this
//! relation was the last of it worth doing.
//!
//! It is still a relation rather than an empty stub, for the reason the rest of
//! the wave gives: a client asking what conversions PostgreSQL defines gets
//! upstream's answer, `\dc` reads it, and `pg_conversion.conproc` points at
//! `pg_proc` rows this build already publishes. An empty `pg_conversion` would
//! be a different claim — that PostgreSQL defines none — and that one is false.
//!
//! One count worth naming rather than smoothing over: 19devel's data defines 98
//! conversions where PostgreSQL 18.4 ships 128. The rows here follow the
//! vendored `.dat`, so this build reports 98, and a smoke file that pinned the
//! total would be pinning which major version the data came from.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value, encoding};

use crate::cols::*;
use crate::oids::*;
use crate::{PG_CONVERSION_ROWS, SystemCatalog};

pub(crate) fn pg_conversion_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_conversion",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("conname", PgType::Name),
            col("connamespace", PgType::Oid),
            col("conowner", PgType::Oid),
            col("conforencoding", PgType::Int4),
            col("contoencoding", PgType::Int4),
            col("conproc", REGPROC),
            col("condefault", PgType::Bool),
        ],
    )
}

/// The built-in conversions, generated from `pg_conversion.dat`.
///
/// The generated rows carry the encodings by name; the numbers the columns
/// really store come from [`crabgresql_types::encoding`], which is the one
/// place this build records PostgreSQL's numbering. A name that answers to no
/// encoding would arrive as `-1` — `pg_char_to_encoding`'s own miss sentinel —
/// and the crate's tests refuse one, which is where the check belongs: the
/// table it would disagree with lives here, not in codegen.
pub(crate) fn pg_conversion_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    PG_CONVERSION_ROWS
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Text(r.conname.to_string()),
                Value::Oid(PG_CATALOG_NAMESPACE_OID),
                Value::Oid(BOOTSTRAP_ROLE_OID),
                Value::Int4(encoding::char_to_encoding(r.conforencoding)),
                Value::Int4(encoding::char_to_encoding(r.contoencoding)),
                regproc(r.conproc),
                Value::Bool(r.condefault),
            ]
        })
        .collect()
}
