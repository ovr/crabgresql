//! The remaining relations whose subsystem this build does not have: large
//! objects, event triggers, transforms, range types, and prepared (two-phase)
//! transactions.
//!
//! They share nothing but that: each is here because a client joins it and gets
//! zero rows from PostgreSQL as well on a database that never used the feature.
//! The one exception is `pg_range`, whose divergence its docstring states.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, oid};

use crate::cols::*;

/// `pg_largeobject` — the 2 kB chunks a large object is stored in. There is no
/// `lo_*` function family and no large-object protocol message here, so nothing
/// can create one; PostgreSQL's copy is empty until `lo_create` runs.
pub(crate) fn pg_largeobject_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_largeobject",
        "pg_catalog",
        vec![
            col("loid", PgType::Oid),
            col("pageno", PgType::Int4),
            col("data", PgType::Bytea),
        ],
    )
}

/// `pg_largeobject_metadata` — one row per large object, carrying its owner and
/// ACL. Empty for the reason [`pg_largeobject_schema`] gives; `pg_dump` reads
/// this one (not the chunks) to decide whether a dump has large objects at all.
pub(crate) fn pg_largeobject_metadata_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_largeobject_metadata",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("lomowner", PgType::Oid),
            col("lomacl", ACLITEM_ARRAY),
        ],
    )
}

/// `pg_event_trigger` — DDL event triggers. No `CREATE EVENT TRIGGER`, and
/// PostgreSQL ships none by default, so both are empty.
pub(crate) fn pg_event_trigger_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_event_trigger",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("evtname", PgType::Name),
            col("evtevent", PgType::Name),
            col("evtowner", PgType::Oid),
            col("evtfoid", PgType::Oid),
            col("evtenabled", CHARLIKE),
            col("evttags", PgType::Array(oid::TEXT)),
        ],
    )
}

/// `pg_transform` — type/language conversions a procedural language uses.
/// Empty in PostgreSQL until an extension such as `hstore_plperl` installs one;
/// the one language here is SQL plus PL/pgSQL, neither of which has transforms.
pub(crate) fn pg_transform_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_transform",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("trftype", PgType::Oid),
            col("trflang", PgType::Oid),
            col("trffromsql", REGPROC),
            col("trftosql", REGPROC),
        ],
    )
}

/// `pg_range` — one row per range type, naming its subtype and the support
/// functions the range operators go through.
///
/// **Diverges from PostgreSQL, which has six rows here**: `int4range`,
/// `int8range`, `numrange`, `tsrange`, `tstzrange`, `daterange` are built in
/// there. This build models no range type, so `pg_type` carries none of those
/// six, and a row naming an `rngtypid` that `pg_type` cannot resolve would make
/// the standard join — `pg_range` to `pg_type` — return a broken pair rather
/// than nothing. Empty is the answer that keeps the two catalogs consistent
/// with each other; the rows arrive with the types, not before them.
pub(crate) fn pg_range_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_range",
        "pg_catalog",
        vec![
            col("rngtypid", PgType::Oid),
            col("rngsubtype", PgType::Oid),
            col("rngmultitypid", PgType::Oid),
            col("rngcollation", PgType::Oid),
            col("rngsubopc", PgType::Oid),
            col("rngcanonical", REGPROC),
            col("rngsubdiff", REGPROC),
        ],
    )
}

/// `pg_prepared_xacts` — transactions left in the prepared state by
/// `PREPARE TRANSACTION`. Two-phase commit is not implemented, and PostgreSQL's
/// view is empty on any server that has none pending, which is the usual case
/// (`max_prepared_transactions` defaults to 0 there).
pub(crate) fn pg_prepared_xacts_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_prepared_xacts",
        "pg_catalog",
        vec![
            col("transaction", PgType::Xid),
            col("gid", PgType::Text),
            col("prepared", PgType::TimestampTz),
            col("owner", PgType::Name),
            col("database", PgType::Name),
        ],
    )
}
