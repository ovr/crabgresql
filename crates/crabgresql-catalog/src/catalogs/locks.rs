//! `pg_locks`: the locks held right now.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::oids::{DATABASE_OID, SHARED_RELATION_OIDS};
use crate::registry::builtin_relation_oid_in;
use crate::source::{CatalogLock, CatalogLockTarget};
use crate::{SystemCatalog, cols::*};

/// `pg_catalog.pg_locks` — one row per lock the server currently holds or waits
/// for.
///
/// A view over `pg_lock_status()` in PostgreSQL; served here as a relation whose
/// rows the session supplies, the way [`crate::catalogs::cursors`] serves
/// `pg_cursors`.
///
/// Every column PostgreSQL 18.4 publishes is here, in its order and with its
/// type, so a monitoring query written against PostgreSQL binds unchanged.
/// `classid`/`objid`/`objsubid` and `page`/`tuple` are always NULL: they belong
/// to the `object`, `page` and `tuple` lock types, and this build takes no lock
/// at those levels — which is also what PostgreSQL leaves them for the lock
/// types it does report here.
pub(crate) fn pg_locks_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_locks",
        "pg_catalog",
        vec![
            col("locktype", PgType::Text),
            col("database", PgType::Oid),
            col("relation", PgType::Oid),
            col("page", PgType::Int4),
            col("tuple", PgType::Int2),
            col("virtualxid", PgType::Text),
            col("transactionid", PgType::Xid),
            col("classid", PgType::Oid),
            col("objid", PgType::Oid),
            col("objsubid", PgType::Int2),
            col("virtualtransaction", PgType::Text),
            col("pid", PgType::Int4),
            col("mode", PgType::Text),
            col("granted", PgType::Bool),
            col("fastpath", PgType::Bool),
            col("waitstart", PgType::TimestampTz),
        ],
    )
}

/// The locks the reading session holds: its transaction's, plus one `relation`
/// row per relation the statement resolved.
///
/// A relation row is a true statement about this build and not a stand-in: a
/// scan holds a shared hold on what it reads for as long as its iterator lives.
///
/// The rows last as long as the *statement* rather than the transaction, which
/// is where this parts company with PostgreSQL: there a relation lock is held to
/// the end of the transaction, so a block accumulates rows for everything it has
/// touched. A shared hold here dies with the scan that took it, so the statement
/// is the honest scope. [`crate::source::CatalogSource::locks`] says which
/// sessions are missing.
///
/// A relation whose name this snapshot has no OID for is dropped rather than
/// reported as OID 0, which names no relation in PostgreSQL either.
pub(crate) fn pg_locks_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    cat.locks()
        .iter()
        .filter_map(|lock| lock_row(cat, lock))
        .collect()
}

/// One `pg_locks` row, or `None` for a relation lock naming something this
/// snapshot cannot number.
fn lock_row(cat: &SystemCatalog, lock: &CatalogLock) -> Option<Vec<Value>> {
    let (locktype, database, relation, virtualxid, transactionid) = match &lock.target {
        CatalogLockTarget::Relation { namespace, name } => {
            let oid = relation_oid(cat, namespace, name)?;
            // A shared catalog belongs to every database, and PostgreSQL says so
            // with 0 rather than by naming one of them.
            let database = match SHARED_RELATION_OIDS.contains(&oid) {
                true => 0,
                false => DATABASE_OID,
            };
            (
                "relation",
                Value::Oid(database),
                Value::Oid(oid),
                Value::Null,
                Value::Null,
            )
        }
        // `database` is NULL for both transaction lock types: they are
        // cluster-wide in PostgreSQL, not per database.
        CatalogLockTarget::VirtualXid => (
            "virtualxid",
            Value::Null,
            Value::Null,
            Value::Text(lock.virtualtransaction.clone()),
            Value::Null,
        ),
        CatalogLockTarget::TransactionId(xid) => (
            "transactionid",
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Xid(*xid),
        ),
    };
    Some(vec![
        Value::Text(locktype.to_string()),
        database,
        relation,
        Value::Null, // page
        Value::Null, // tuple
        virtualxid,
        transactionid,
        Value::Null, // classid
        Value::Null, // objid
        Value::Null, // objsubid
        Value::Text(lock.virtualtransaction.clone()),
        Value::Int4(lock.pid),
        Value::Text(lock.mode.to_string()),
        Value::Bool(lock.granted),
        Value::Bool(lock.fastpath),
        match lock.waitstart {
            Some(at) => Value::TimestampTz(at),
            None => Value::Null,
        },
    ])
}

/// The OID a `relation` row reports. A served relation carries PostgreSQL's own
/// fixed OID from the registry; everything else is numbered by this snapshot,
/// which is why the lock travels as a name.
fn relation_oid(cat: &SystemCatalog, namespace: &str, name: &str) -> Option<u32> {
    builtin_relation_oid_in(namespace, name).or_else(|| cat.relation_oid_in(namespace, name))
}
