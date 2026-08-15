//! The fixed OIDs the catalog modules share.
//!
//! They live together because they are cross-referenced: `pg_am.oid` is a user
//! relation's `relam`, `pg_authid.oid` is every other catalog's owner column, and
//! a value invented here has to stay clear of both PostgreSQL's assignments and
//! the server's `FIRST_USER_OID`. Scattered across the modules that emit them,
//! two could quietly take the same number.

/// Synthetic OID base for `pg_enum` rows (one per enum label). Chosen above the
/// built-in ranges so a per-label OID never collides with a type/relation OID.
pub(crate) const FIRST_ENUM_OID: u32 = 0x8000_0000;

/// OID assigned to the heap access method (`pg_am` row `heap` = 2). Reported for
/// every user relation's `relam`.
pub(crate) const HEAP_AM_OID: u32 = 2;
/// Stable OID assigned to the managed Parquet table method. PostgreSQL has no
/// such method, so the value is crabgresql's own — but it must stay *below*
/// `FIRST_USER_OID` (16384), the point where the server's OID allocator starts
/// handing out OIDs to user objects. A built-in catalog row at 16384 would share
/// its OID with the first `CREATE TYPE`/`CREATE SCHEMA`, breaking the
/// cluster-wide uniqueness clients assume. PostgreSQL reserves 1..16383 for
/// built-ins for exactly this reason.
pub(crate) const PARQUET_AM_OID: u32 = 16_000;
/// Stable OID of the managed buffer table method; see [`PARQUET_AM_OID`] for why
/// crabgresql's own methods sit below `FIRST_USER_OID`.
pub(crate) const BUFFER_AM_OID: u32 = 16_001;
/// OID of the `btree` index access method, shared by `pg_am` and the `relam` of
/// every B-tree index's `pg_class` row so the join between them holds.
pub(crate) const BTREE_AM_OID: u32 = 403;
/// OID of the `hash` index access method; see [`BTREE_AM_OID`].
pub(crate) const HASH_AM_OID: u32 = 405;

/// OID reported as the owner of every relation, type, and schema. PostgreSQL
/// assigns 10 to the bootstrap superuser. `pg_get_userbyid` resolves it back to
/// the session user, so the two must agree — hence the shared constant.
///
/// TODO: one owner stands for the whole cluster. Nothing creates a second role,
/// so every `relowner`/`typowner`/`nspowner` reports this OID whoever connects;
/// per-object ownership needs `CREATE ROLE` and an owner stored per object.
pub(crate) const BOOTSTRAP_ROLE_OID: u32 = 10;

/// `pg_namespace.oid` of `public`, PostgreSQL's fixed value. Where a user type
/// lives, and so what its `typnamespace` reports — the schema an unqualified
/// name reaches only after `pg_catalog`.
pub(crate) const PUBLIC_NAMESPACE_OID: u32 = 2200;

/// `pg_namespace.oid` of `pg_catalog`, PostgreSQL's fixed value. Every built-in
/// object reports it as its namespace, which is why it is cross-referenced from
/// more than the one relation that emits the row.
pub(crate) const PG_CATALOG_NAMESPACE_OID: u32 = 11;

/// OID of the one database a crabgresql server serves. PostgreSQL assigns a
/// fresh OID per `CREATE DATABASE`, so there is no upstream value to reuse: this
/// one is fixed here so `pg_database.oid` joins against itself consistently and
/// `current_database()::regclass`-style round-trips stay stable across restarts.
///
/// It sits in the same reserved band as [`PARQUET_AM_OID`], and for the same
/// reason: at 16384 it would have shared its OID with the first `CREATE SCHEMA`
/// or `CREATE TYPE` the server ever ran, since that is where the OID allocator
/// starts.
pub(crate) const DATABASE_OID: u32 = 16_002;

/// The `pg_proc` rows for crabgresql's own access-method handlers, which have no
/// upstream function to point at. `pg_am.amhandler` is a reference into
/// `pg_proc`, so leaving these at 0 would print `-` where PostgreSQL prints a
/// handler name for every method it ships. They sit in the same reserved band as
/// [`PARQUET_AM_OID`], and for the same reason.
pub(crate) const OWN_AM_HANDLERS: [(u32, &str); 2] = [
    (16_003, "parquet_tableam_handler"),
    (16_004, "buffer_tableam_handler"),
];

/// `pg_default` and `pg_global`, PostgreSQL's two bootstrap tablespaces.
/// crabgresql has no `CREATE TABLESPACE`, so these two rows are the whole
/// relation — which is also true of a stock PostgreSQL cluster nobody has added
/// one to.
pub(crate) const DEFAULT_TABLESPACE_OID: u32 = 1663;
pub(crate) const GLOBAL_TABLESPACE_OID: u32 = 1664;

/// The `pg_language` OID this build gives `plpgsql`. See [`crate::catalogs::language::pg_language_rows`]
/// for why it is ours to choose.
pub const PLPGSQL_LANG_OID: u32 = 13540;

/// `pg_extension.oid` of the `plpgsql` extension, PostgreSQL 18.4's own
/// assignment. Like the 12000-band view OIDs in [`crate::registry`], it comes
/// from `initdb` rather than from a `.dat` file, so it is deterministic for a
/// major version rather than fixed forever — and reusing it still beats
/// inventing a number, because a client that hard-codes one hard-codes this.
pub(crate) const PLPGSQL_EXTENSION_OID: u32 = 14049;
