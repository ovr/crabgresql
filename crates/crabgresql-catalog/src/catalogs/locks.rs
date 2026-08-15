//! `pg_locks`: the locks held right now.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::oids::DATABASE_OID;
use crate::registry::builtin_relation_oid;
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

/// The locks the reading session holds, in the order it reported them, followed
/// by this scan's own `AccessShareLock` on `pg_locks`.
///
/// That last row is what most clients came for — PostgreSQL's answer to a bare
/// `SELECT * FROM pg_locks` under autocommit is exactly two rows, the reader's
/// `virtualxid` lock and its `AccessShareLock` on `pg_locks` itself — and it is
/// a true statement about this build too: a scan holds a shared hold on the
/// relation it reads for the iterator's life, so a statement that reached this
/// function does hold a read lock on `pg_locks`.
///
/// What it is *not* is the whole set. Locks appear for the reading session only
/// ([`crate::source::CatalogSource::locks`] says why), and the relation locks of
/// a statement's *other* tables are missing: this builder learns of a relation
/// by being asked for its rows, so `SELECT * FROM pg_locks, t` lists `pg_locks`
/// and not `t`, where PostgreSQL lists both.
pub(crate) fn pg_locks_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let locks = cat.locks();
    let mut rows: Vec<Vec<Value>> = locks.iter().map(lock_row).collect();
    // This scan's own hold, attributed to the session that reported the locks
    // above. A snapshot with no session behind it (a fixture catalog) reports
    // no locks at all rather than inventing a holder for this one.
    if let Some(holder) = locks.first() {
        rows.push(lock_row(&CatalogLock {
            target: CatalogLockTarget::Relation(builtin_relation_oid("pg_locks").unwrap_or(0)),
            virtualtransaction: holder.virtualtransaction.clone(),
            pid: holder.pid,
            mode: "AccessShareLock",
            granted: true,
            // PostgreSQL takes a weak relation lock with no conflicting holder
            // through the per-backend fast path; a catalog read is the
            // canonical case of one.
            fastpath: true,
            waitstart: None,
        }));
    }
    rows
}

/// One `pg_locks` row. The lock's target decides which of the identity columns
/// carries it and the rest stay NULL, exactly as PostgreSQL fills them.
fn lock_row(lock: &CatalogLock) -> Vec<Value> {
    let (locktype, database, relation, virtualxid, transactionid) = match lock.target {
        CatalogLockTarget::Relation(oid) => (
            "relation",
            Value::Oid(DATABASE_OID),
            Value::Oid(oid),
            Value::Null,
            Value::Null,
        ),
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
            Value::Xid(xid),
        ),
    };
    vec![
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
    ]
}
