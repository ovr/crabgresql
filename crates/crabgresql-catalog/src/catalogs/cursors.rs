//! `pg_cursors`: the session's open cursors.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;

/// `pg_catalog.pg_cursors` — the session's open `DECLARE … CURSOR` cursors.
///
/// A view over `pg_cursor()` in PostgreSQL; served here as a relation whose rows
/// the session supplies, which is indistinguishable to a client reading it.
///
/// `creation_time` is the `DECLARE`'s *statement* timestamp, as in PostgreSQL:
/// a cursor declared mid-block reports an instant strictly after that block's
/// `now()`, and two cursors declared in separate messages differ.
pub(crate) fn pg_cursors_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_cursors",
        "pg_catalog",
        vec![
            col("name", PgType::Text),
            col("statement", PgType::Text),
            col("is_holdable", PgType::Bool),
            col("is_binary", PgType::Bool),
            col("is_scrollable", PgType::Bool),
            col("creation_time", PgType::TimestampTz),
        ],
    )
}

/// One row per open cursor, in the order the session enumerated them.
pub(crate) fn pg_cursors_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let cursors = cat.cursors();
    cursors
        .iter()
        .map(|cursor| {
            vec![
                Value::Text(cursor.name.clone()),
                Value::Text(cursor.statement.clone()),
                Value::Bool(cursor.is_holdable),
                Value::Bool(cursor.is_binary),
                Value::Bool(cursor.is_scrollable),
                Value::TimestampTz(cursor.creation_time),
            ]
        })
        .collect()
}
