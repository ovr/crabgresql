//! `TableSchema` definitions and row builders for the supported `pg_catalog`
//! relations.
//!
//! The column list for each relation follows PostgreSQL's column *names* and
//! order for the frequently-queried leading columns. Fidelity deviations are
//! deliberate and documented (see the crate root): catalog-only types we do not
//! model yet are represented pragmatically — `"char"` columns as `text`, and
//! `regproc` I/O columns as the referenced function's `text` name (which is what
//! PostgreSQL's `regprocout` prints anyway).

use std::collections::HashMap;

use crabgresql_storage_api::{
    Column, IndexConstraint, IndexMethod, PartitionBoundDatum, PartitionOf, PartitionStrategy,
    RelStats, TableAccessMethod, TableSchema,
};
use crabgresql_types::{PgType, Value, VectorKind};

use crate::{
    CatalogCursor, CatalogIndex, CatalogRelation, CatalogRoutine, CatalogSequence, CatalogToast,
    CatalogUserType, PG_CAST_ROWS, PG_TYPE_ROWS, RelKind, TOAST_NAMESPACE,
};

/// Synthetic OID base for `pg_enum` rows (one per enum label). Chosen above the
/// built-in ranges so a per-label OID never collides with a type/relation OID.
const FIRST_ENUM_OID: u32 = 0x8000_0000;

/// A `"char"`/`regproc` column: a single- or short-name catalog column we render
/// as `text` for now. Kept as a named alias so the deviation is greppable.
const CHARLIKE: PgType = PgType::Text;

fn col(name: &str, ty: PgType) -> Column {
    Column::new(name, ty)
}

/// `pg_catalog.pg_type` — a curated, PG-ordered subset of the columns clients
/// query. Trailing rarely-read columns (`typmodin`, `typnotnull`, `typbasetype`,
/// `typdefault`, `typacl`, …) are omitted for now.
pub fn pg_type_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_type",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("typname", PgType::Name),
            col("typnamespace", PgType::Oid),
            col("typowner", PgType::Oid),
            col("typlen", PgType::Int2),
            col("typbyval", PgType::Bool),
            col("typtype", CHARLIKE),
            col("typcategory", CHARLIKE),
            col("typispreferred", PgType::Bool),
            col("typisdefined", PgType::Bool),
            col("typdelim", CHARLIKE),
            col("typrelid", PgType::Oid),
            col("typelem", PgType::Oid),
            col("typarray", PgType::Oid),
            col("typinput", CHARLIKE),
            col("typoutput", CHARLIKE),
            col("typreceive", CHARLIKE),
            col("typsend", CHARLIKE),
            col("typalign", CHARLIKE),
            col("typstorage", CHARLIKE),
            col("typcollation", PgType::Oid),
        ],
    )
}

/// `pg_catalog.pg_collation` — the collations this build ships. `collversion`
/// is omitted: it exists so PostgreSQL can warn when the underlying OS locale
/// data changes under an index, and the ICU data here is compiled in, so there
/// is no external version to drift from.
pub fn pg_collation_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_collation",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("collname", PgType::Name),
            col("collnamespace", PgType::Oid),
            col("collowner", PgType::Oid),
            col("collprovider", CHARLIKE),
            col("collisdeterministic", PgType::Bool),
            col("collencoding", PgType::Int4),
            col("collcollate", PgType::Text),
            col("collctype", PgType::Text),
            col("colllocale", PgType::Text),
        ],
    )
}

/// The `pg_collation` rows, one per collation in the shared registry — the same
/// list [`crabgresql_types::collation::compare_str`] orders strings by, so what
/// the catalog advertises and what queries actually do cannot drift.
pub fn pg_collation_rows() -> Vec<Vec<Value>> {
    crabgresql_types::collation::COLLATIONS
        .iter()
        .map(|c| {
            let opt_text = |s: Option<&str>| s.map_or(Value::Null, |s| Value::Text(s.to_string()));
            vec![
                Value::Oid(c.oid),
                Value::Text(c.name.to_string()),
                // Every collation lives in pg_catalog (11), owned by the
                // bootstrap superuser.
                Value::Oid(11),
                Value::Oid(BOOTSTRAP_ROLE_OID),
                Value::Text(c.provider.as_char().to_string()),
                Value::Bool(c.deterministic),
                Value::Int4(c.encoding),
                opt_text(c.libc_locale),
                opt_text(c.libc_locale),
                opt_text(c.locale),
            ]
        })
        .collect()
}

/// `typcollation`: the collation of values of `oid`'s type, or `0` when the type
/// is not collatable. An OID this build does not model has no collation.
fn typcollation_of(oid: u32) -> u32 {
    PgType::from_oid(oid).map_or(0, crabgresql_types::collation::type_collation)
}

/// The built-in `pg_type` rows generated from `pg_type.dat`. Callers append any
/// user-defined-type rows (a later slice) after these.
pub fn pg_type_builtin_rows() -> Vec<Vec<Value>> {
    PG_TYPE_ROWS
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Text(r.typname.to_string()),
                Value::Oid(r.typnamespace),
                Value::Oid(r.typowner),
                Value::Int2(r.typlen),
                Value::Bool(r.typbyval),
                Value::Text(r.typtype.to_string()),
                Value::Text(r.typcategory.to_string()),
                Value::Bool(r.typispreferred),
                Value::Bool(r.typisdefined),
                Value::Text(r.typdelim.to_string()),
                Value::Oid(r.typrelid),
                Value::Oid(r.typelem),
                Value::Oid(r.typarray),
                Value::Text(r.typinput.to_string()),
                Value::Text(r.typoutput.to_string()),
                Value::Text(r.typreceive.to_string()),
                Value::Text(r.typsend.to_string()),
                Value::Text(r.typalign.to_string()),
                Value::Text(r.typstorage.to_string()),
                Value::Oid(typcollation_of(r.oid)),
            ]
        })
        .collect()
}

/// The `pg_type` rows for user-defined enum types, appended after
/// [`pg_type_builtin_rows`]. Only enums are reflected (`typtype = 'e'`); other
/// `CREATE TYPE` shapes are not surfaced here yet. Column order matches
/// [`pg_type_schema`].
pub fn pg_type_user_rows(user_types: &[CatalogUserType]) -> Vec<Vec<Value>> {
    user_types
        .iter()
        .filter(|t| t.enum_labels.is_some())
        .map(|t| {
            vec![
                Value::Oid(t.oid),
                Value::Text(t.name.clone()),
                // pg_catalog namespace / bootstrap superuser, as elsewhere.
                Value::Oid(11),
                Value::Oid(BOOTSTRAP_ROLE_OID),
                // Enums are a fixed 4-byte, pass-by-value, OID-backed type.
                Value::Int2(4),
                Value::Bool(true),
                Value::Text("e".to_string()),
                Value::Text("E".to_string()),
                Value::Bool(false),
                Value::Bool(true),
                Value::Text(",".to_string()),
                Value::Oid(0),
                Value::Oid(0),
                Value::Oid(0),
                Value::Text("enum_in".to_string()),
                Value::Text("enum_out".to_string()),
                Value::Text("enum_recv".to_string()),
                Value::Text("enum_send".to_string()),
                Value::Text("i".to_string()),
                Value::Text("p".to_string()),
                // An enum is not collatable.
                Value::Oid(0),
            ]
        })
        .collect()
}

/// `pg_catalog.pg_enum` — one row per (enum type, label). `enumsortorder` is the
/// 1-based definition position (PG stores a float4 so labels can be inserted
/// between existing ones; a freshly created enum uses 1, 2, 3, …).
pub fn pg_enum_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_enum",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("enumtypid", PgType::Oid),
            col("enumsortorder", PgType::Float4),
            col("enumlabel", PgType::Name),
        ],
    )
}

/// The `pg_enum` rows for every user-defined enum type, in a stable order (by
/// type OID, then definition order). Per-label OIDs are synthetic.
pub fn pg_enum_rows(user_types: &[CatalogUserType]) -> Vec<Vec<Value>> {
    let mut enums: Vec<&CatalogUserType> = user_types
        .iter()
        .filter(|t| t.enum_labels.is_some())
        .collect();
    enums.sort_by_key(|t| t.oid);
    let mut rows = Vec::new();
    let mut next_oid = FIRST_ENUM_OID;
    for t in enums {
        let labels = t.enum_labels.as_deref().unwrap_or_default();
        for (i, label) in labels.iter().enumerate() {
            rows.push(vec![
                Value::Oid(next_oid),
                Value::Oid(t.oid),
                Value::Float4((i + 1) as f32),
                Value::Text(label.clone()),
            ]);
            next_oid += 1;
        }
    }
    rows
}

/// `pg_catalog.pg_cast` — the built-in casts between types crabgresql exposes.
pub fn pg_cast_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_cast",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("castsource", PgType::Oid),
            col("casttarget", PgType::Oid),
            // regproc; rendered as the upstream function reference text for now.
            col("castfunc", PgType::Text),
            col("castcontext", CHARLIKE),
            col("castmethod", CHARLIKE),
        ],
    )
}

/// The built-in `pg_cast` rows generated from `pg_cast.dat` (restricted to casts
/// between exposed types).
pub fn pg_cast_rows() -> Vec<Vec<Value>> {
    PG_CAST_ROWS
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Oid(r.castsource),
                Value::Oid(r.casttarget),
                Value::Text(r.castfunc.to_string()),
                Value::Text(r.castcontext.to_string()),
                Value::Text(r.castmethod.to_string()),
            ]
        })
        .collect()
}

/// `pg_catalog.pg_sequence` — the definition of each user sequence, one row per
/// [`RelKind::Sequence`] relation, keyed by its `pg_class` OID (`seqrelid`).
pub fn pg_sequence_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_sequence",
        "pg_catalog",
        vec![
            col("seqrelid", PgType::Oid),
            col("seqtypid", PgType::Oid),
            col("seqstart", PgType::Int8),
            col("seqincrement", PgType::Int8),
            col("seqmax", PgType::Int8),
            col("seqmin", PgType::Int8),
            col("seqcache", PgType::Int8),
            col("seqcycle", PgType::Bool),
        ],
    )
}

pub fn pg_sequence_rows(sequences: &[(u32, CatalogSequence)]) -> Vec<Vec<Value>> {
    sequences
        .iter()
        .map(|(oid, s)| {
            vec![
                Value::Oid(*oid),
                Value::Oid(s.type_oid),
                Value::Int8(s.start),
                Value::Int8(s.increment),
                Value::Int8(s.max),
                Value::Int8(s.min),
                Value::Int8(s.cache),
                Value::Bool(s.cycle),
            ]
        })
        .collect()
}

/// `pg_catalog.pg_namespace` — the schemas visible on a fresh cluster.
pub fn pg_namespace_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_namespace",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("nspname", PgType::Name),
            col("nspowner", PgType::Oid),
            // aclitem[]; represented as text and always NULL (default ACL) here.
            col("nspacl", PgType::Text),
        ],
    )
}

/// OID assigned to the heap access method (`pg_am` row `heap` = 2). Reported for
/// every user relation's `relam`.
const HEAP_AM_OID: u32 = 2;
/// Stable OID assigned to the managed Parquet table method. PostgreSQL has no
/// such method, so the value is crabgresql's own — but it must stay *below*
/// `FIRST_USER_OID` (16384), the point where the server's OID allocator starts
/// handing out OIDs to user objects. A built-in catalog row at 16384 would share
/// its OID with the first `CREATE TYPE`/`CREATE SCHEMA`, breaking the
/// cluster-wide uniqueness clients assume. PostgreSQL reserves 1..16383 for
/// built-ins for exactly this reason.
pub const PARQUET_AM_OID: u32 = 16_000;
/// Stable OID of the managed buffer table method; see [`PARQUET_AM_OID`] for why
/// crabgresql's own methods sit below `FIRST_USER_OID`.
pub const BUFFER_AM_OID: u32 = 16_001;
/// OID of the `btree` index access method, shared by `pg_am` and the `relam` of
/// every B-tree index's `pg_class` row so the join between them holds.
const BTREE_AM_OID: u32 = 403;
/// OID of the `hash` index access method; see [`BTREE_AM_OID`].
const HASH_AM_OID: u32 = 405;

/// OID reported as the owner of every relation, type, and schema. PostgreSQL
/// assigns 10 to the bootstrap superuser; crabgresql has no role catalog yet, so
/// one owner stands for the whole cluster. `pg_get_userbyid` resolves it back to
/// the session user, so the two must agree — hence the shared constant.
pub(crate) const BOOTSTRAP_ROLE_OID: u32 = 10;

/// `pg_catalog.pg_am` — the access methods. PostgreSQL lists the methods its
/// build actually registered; crabgresql adds its managed `parquet` and `buffer`
/// table methods alongside PostgreSQL's built-ins so a client that
/// joins `pg_class.relam` or reads `pg_am` sees the shape it expects.
///
/// Fidelity note (`AGENTS.md`): these rows are transcribed from the output of
/// `SELECT oid, amname, amhandler, amtype FROM pg_am ORDER BY oid` on a stock
/// PostgreSQL 18.4, not from upstream source. No `pg_am.dat` is vendored —
/// seven rows do not justify codegen.
pub fn pg_am_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_am",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("amname", PgType::Name),
            col("amhandler", CHARLIKE),
            col("amtype", CHARLIKE),
        ],
    )
}

/// The fixed `pg_am` rows. `amtype` is `'t'` for a table access method and
/// `'i'` for an index one.
pub fn pg_am_rows() -> Vec<Vec<Value>> {
    let row = |oid: u32, amname: &str, amhandler: &str, amtype: &str| {
        vec![
            Value::Oid(oid),
            Value::Text(amname.to_string()),
            Value::Text(amhandler.to_string()),
            Value::Text(amtype.to_string()),
        ]
    };
    vec![
        row(HEAP_AM_OID, "heap", "heap_tableam_handler", "t"),
        row(BTREE_AM_OID, "btree", "bthandler", "i"),
        row(HASH_AM_OID, "hash", "hashhandler", "i"),
        row(783, "gist", "gisthandler", "i"),
        row(2742, "gin", "ginhandler", "i"),
        row(3580, "brin", "brinhandler", "i"),
        row(4000, "spgist", "spghandler", "i"),
        row(PARQUET_AM_OID, "parquet", "parquet_tableam_handler", "t"),
        row(BUFFER_AM_OID, "buffer", "buffer_tableam_handler", "t"),
    ]
}

/// `pg_catalog.pg_cursors` — the session's open `DECLARE … CURSOR` cursors.
///
/// A view over `pg_cursor()` in PostgreSQL; served here as a relation whose rows
/// the session supplies, which is indistinguishable to a client reading it.
///
/// Divergence: `creation_time` is always NULL. It is a `timestamptz` of when the
/// cursor was declared, and crabgresql has no wall clock in the executor yet —
/// no `now()`/`current_timestamp`. The column is kept so `SELECT *` has
/// PostgreSQL's shape.
pub fn pg_cursors_schema() -> TableSchema {
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
pub fn pg_cursors_rows(cursors: &[CatalogCursor]) -> Vec<Vec<Value>> {
    cursors
        .iter()
        .map(|cursor| {
            vec![
                Value::Text(cursor.name.clone()),
                Value::Text(cursor.statement.clone()),
                Value::Bool(cursor.is_holdable),
                Value::Bool(cursor.is_binary),
                Value::Bool(cursor.is_scrollable),
                Value::Null,
            ]
        })
        .collect()
}

/// `pg_catalog.pg_class` — a curated subset of columns for user relations, in
/// PostgreSQL's `attnum` order. Columns crabgresql has no state for are still
/// emitted with their true constant so a client's `\d` predicates evaluate as on
/// PG (e.g. `relchecks = 0` gates the CHECK-constraint listing *off*). Storage
/// bookkeeping columns beyond this set (`relfrozenxid`, `relminmxid`, …) are
/// omitted.
///
/// `relpages`/`reltuples` hold the **last `ANALYZE` snapshot**, not a live
/// measurement — matching PostgreSQL, where a relation that has never been
/// analyzed or vacuumed reports `relpages = 0` and `reltuples = -1` however
/// large it actually is (observed on PostgreSQL 18.4). The planner's own live
/// size estimate is a separate thing: see [`crate::RelStats`].
///
/// `relallvisible` sits between them in `attnum` order and is emitted as a
/// constant `0` — crabgresql keeps no visibility map, and `0` is what PostgreSQL
/// reports for a relation that has never been vacuumed.
pub fn pg_class_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_class",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("relname", PgType::Name),
            col("relnamespace", PgType::Oid),
            col("reltype", PgType::Oid),
            col("reloftype", PgType::Oid),
            col("relowner", PgType::Oid),
            col("relam", PgType::Oid),
            col("reltablespace", PgType::Oid),
            col("relpages", PgType::Int4),
            col("reltuples", PgType::Float4),
            col("relallvisible", PgType::Int4),
            col("reltoastrelid", PgType::Oid),
            col("relhasindex", PgType::Bool),
            col("relpersistence", CHARLIKE),
            col("relkind", CHARLIKE),
            col("relnatts", PgType::Int2),
            col("relchecks", PgType::Int2),
            col("relhasrules", PgType::Bool),
            col("relhastriggers", PgType::Bool),
            col("relrowsecurity", PgType::Bool),
            col("relforcerowsecurity", PgType::Bool),
            col("relreplident", CHARLIKE),
            col("relispartition", PgType::Bool),
            // pg_node_tree in PG; crabgresql stores the already-deparsed
            // `FOR VALUES …` text (see `pg_get_expr`, which just echoes it).
            col("relpartbound", PgType::Text),
        ],
    )
}

/// Deparse a leaf partition's `relpartbound` to the text PostgreSQL's
/// `pg_get_expr(relpartbound, oid)` prints — `FOR VALUES FROM (…) TO (…)`. Only
/// RANGE partitions exist, so only the range form is produced. `MINVALUE`/
/// `MAXVALUE` print as bare keywords. Storing the final text (not a node tree)
/// is a deliberate deviation: `pg_get_expr` then just echoes it.
///
/// Quoting follows what PostgreSQL 18.4 was observed to print: `true`/`false`
/// bare, a non-negative integer bare, and everything else single-quoted with
/// embedded quotes doubled — including negative numbers (`'-10'`), floats,
/// dates, and strings.
///
/// Fidelity note: PostgreSQL actually decides this from the *parse* of the
/// bound, printing a literal bare only when it needed no coercion to the key
/// type — so with an `int8` key even `5` prints as `'5'`, while with an `int4`
/// key it prints bare. crabgresql stores the bound already coerced to the key
/// type and does not record whether a coercion happened, so it cannot make that
/// distinction; the rule above matches PostgreSQL for the `int4`, boolean, and
/// text keys in practice and quotes (the safe, re-parseable form) otherwise.
fn deparse_partbound(part: &PartitionOf) -> String {
    let datum = |d: &PartitionBoundDatum| match d {
        PartitionBoundDatum::MinValue => "MINVALUE".to_string(),
        PartitionBoundDatum::MaxValue => "MAXVALUE".to_string(),
        PartitionBoundDatum::Value(v) => {
            // A boolean bound is an SQL keyword, not a string: PG prints
            // `false`, never the `'f'` of the wire encoding — which would not
            // even re-parse as a bool bound.
            if let Value::Bool(b) = v {
                return if *b { "true" } else { "false" }.to_string();
            }
            let text = v.encode_text_utc().unwrap_or_default();
            let bare = match v {
                Value::Int2(_) | Value::Int4(_) | Value::Int8(_) => !text.starts_with('-'),
                _ => false,
            };
            if bare {
                text
            } else {
                format!("'{}'", text.replace('\'', "''"))
            }
        }
    };
    let list =
        |datums: &[PartitionBoundDatum]| datums.iter().map(datum).collect::<Vec<_>>().join(", ");
    format!(
        "FOR VALUES FROM ({}) TO ({})",
        list(&part.bound.from),
        list(&part.bound.to)
    )
}

/// The `(relpages, reltuples)` pair `pg_class` reports for a relation.
///
/// PostgreSQL only writes these during `VACUUM`/`ANALYZE`, so a relation that
/// has never been analyzed reports `(0, -1)` no matter how large it is — `-1` is
/// the sentinel meaning "unknown", distinct from a genuine zero-row relation
/// (verified against PostgreSQL 18.4). Reporting the planner's live estimate
/// here instead would look more informative and be less correct: a client that
/// checks `reltuples = -1` to decide whether a table needs analyzing would never
/// see one that did.
fn analyzed_size(stats: &RelStats) -> (Value, Value) {
    if !stats.analyzed {
        return (Value::Int4(0), Value::Float4(-1.0));
    }
    (
        Value::Int4(stats.relpages.min(i32::MAX as u32) as i32),
        Value::Float4(stats.reltuples as f32),
    )
}

/// Build `pg_class` rows from `(oid, schema)` pairs paired with their kinds.
/// `relpersistence` comes from each schema (`'p'` permanent, `'u'` unlogged,
/// `'t'` temporary — the memory tables); a table is an ordinary heap (`relkind = 'r'`,
/// `relam = 2`) while a view has no storage access method (`relkind = 'v'`,
/// `relam = 0`). The synthetic OIDs are stable within one catalog snapshot so a
/// join to `pg_attribute.attrelid` lines up.
///
/// Columns crabgresql does not track are their PostgreSQL constants: no CHECK
/// constraints (`relchecks = 0`), rules only on views (`relhasrules`), no
/// triggers or row security, no `OF type` / tablespace / TOAST relation. A
/// heap-backed relation defaults its replica identity to the primary key
/// (`relreplident = 'd'`); views, sequences, and indexes have none (`'n'`).
///
/// `stats` is parallel to `relations`; see [`analyzed_size`] for how it renders.
pub fn pg_class_rows(
    relations: &[(u32, TableSchema)],
    kinds: &[RelKind],
    stats: &[RelStats],
    indexes: &[CatalogIndex],
    toasts: &[CatalogToast],
    namespace_oids: &HashMap<String, u32>,
) -> Vec<Vec<Value>> {
    // Resolve a relation's namespace OID, defaulting to `public` (2200) for any
    // namespace not in the map (should not happen for a live relation).
    let nsp_oid = |namespace: &str| namespace_oids.get(namespace).copied().unwrap_or(2200);
    let mut rows: Vec<Vec<Value>> = relations
        .iter()
        .zip(kinds)
        .zip(stats)
        .map(|(((oid, schema), kind), stats)| {
            // A partitioned parent has no access method (`relam = 0`) and holds no
            // storage of its own.
            let (relam, relkind) = match kind {
                RelKind::Table => (
                    match schema.access_method {
                        TableAccessMethod::Heap => HEAP_AM_OID,
                        TableAccessMethod::Parquet => PARQUET_AM_OID,
                        TableAccessMethod::Buffer => BUFFER_AM_OID,
                    },
                    "r",
                ),
                RelKind::PartitionedTable => (0, "p"),
                RelKind::View => (0, "v"),
                RelKind::Sequence => (0, "S"),
            };
            // Heap-backed relations (ordinary + partitioned tables) default their
            // replica identity to the primary key; the rest carry none.
            let relreplident = match kind {
                RelKind::Table | RelKind::PartitionedTable => "d",
                RelKind::View | RelKind::Sequence => "n",
            };
            let relpartbound = match &schema.partition_of {
                Some(part) => Value::Text(deparse_partbound(part)),
                None => Value::Null,
            };
            // A sequence is one page holding its single row, and PostgreSQL
            // reports it that way from creation — there is nothing to analyze.
            let (relpages, reltuples) = match kind {
                RelKind::Sequence => (Value::Int4(1), Value::Float4(1.0)),
                _ => analyzed_size(stats),
            };
            vec![
                Value::Oid(*oid),
                Value::Text(schema.name.clone()),
                Value::Oid(nsp_oid(&schema.namespace)),
                Value::Oid(0),
                // reloftype: crabgresql has no typed tables.
                Value::Oid(0),
                Value::Oid(BOOTSTRAP_ROLE_OID),
                Value::Oid(relam),
                // reltablespace: default tablespace.
                Value::Oid(0),
                relpages,
                reltuples,
                // relallvisible: no visibility map is kept.
                Value::Int4(0),
                // reltoastrelid: the relation's TOAST relation, or 0 when it has
                // none. Zero is legitimate PostgreSQL state — it is what PG
                // reports for a table with no out-of-line storage — and it is
                // what a table of narrow columns keeps, since the TOAST relation
                // is created only once a row first needs one.
                Value::Oid(
                    toasts
                        .iter()
                        .find(|t| t.table_oid == *oid)
                        .map_or(0, |t| t.oid),
                ),
                Value::Bool(indexes.iter().any(|index| index.table_oid == *oid)),
                Value::Text(schema.persistence.as_char().to_string()),
                Value::Text(relkind.to_string()),
                Value::Int2(schema.columns.len() as i16),
                // relchecks: no CHECK constraints modeled.
                Value::Int2(0),
                // relhasrules: only a view carries the `_RETURN` rule.
                Value::Bool(matches!(kind, RelKind::View)),
                // relhastriggers / relrowsecurity / relforcerowsecurity.
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
                Value::Text(relreplident.to_string()),
                Value::Bool(schema.partition_of.is_some()),
                relpartbound,
            ]
        })
        .collect();
    rows.extend(indexes.iter().map(|index| {
        vec![
            Value::Oid(index.oid),
            Value::Text(index.metadata.name.clone()),
            // An index lives in its table's namespace.
            Value::Oid(nsp_oid(&index.table_schema.namespace)),
            Value::Oid(0),
            Value::Oid(0),
            Value::Oid(BOOTSTRAP_ROLE_OID),
            Value::Oid(match index.metadata.method {
                IndexMethod::BTree => BTREE_AM_OID,
                IndexMethod::Hash => HASH_AM_OID,
            }),
            Value::Oid(0),
            // relpages / reltuples: per-index size is not tracked, so an index
            // reports the never-analyzed sentinel. relallvisible: no map.
            Value::Int4(0),
            Value::Float4(-1.0),
            Value::Int4(0),
            Value::Oid(0),
            Value::Bool(false),
            Value::Text("p".to_string()),
            Value::Text("i".to_string()),
            Value::Int2(index.metadata.keys.len() as i16),
            Value::Int2(0),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            // An index has no replica identity of its own.
            Value::Text("n".to_string()),
            Value::Bool(false),
            Value::Null,
        ]
    }));
    // TOAST relations, as `relkind = 't'` in the `pg_toast` namespace. Publishing
    // the row is what makes a non-zero `reltoastrelid` safe: it is a foreign key
    // into `pg_class.oid`, so an OID with no row here would be a dangling
    // reference of exactly the kind upstream's `oidjoins` test exists to catch.
    rows.extend(toasts.iter().map(|toast| {
        vec![
            Value::Oid(toast.oid),
            Value::Text(toast.name.clone()),
            Value::Oid(namespace_oids.get(TOAST_NAMESPACE).copied().unwrap_or(99)),
            Value::Oid(0),
            Value::Oid(0),
            Value::Oid(BOOTSTRAP_ROLE_OID),
            Value::Oid(HEAP_AM_OID),
            Value::Oid(0),
            Value::Int4(toast.stats.relpages as i32),
            // reltuples: chunks are not rows, so a count here would invite being
            // read as one. The never-analyzed sentinel is the honest answer.
            Value::Float4(-1.0),
            Value::Int4(0),
            // A TOAST relation has no TOAST relation of its own.
            Value::Oid(0),
            // relhasindex: PostgreSQL indexes its TOAST relation on
            // `(chunk_id, chunk_seq)`; ours chains chunks by ctid instead, so
            // there is no `pg_toast_<oid>_index`, and claiming one would be the
            // dangling reference this block exists to avoid.
            Value::Bool(false),
            Value::Text(toast.persistence.as_char().to_string()),
            Value::Text("t".to_string()),
            Value::Int2(TOAST_COLUMNS.len() as i16),
            Value::Int2(0),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Text("n".to_string()),
            Value::Bool(false),
            Value::Null,
        ]
    }));
    rows
}

/// The columns PostgreSQL gives every TOAST relation, published so a `pg_class`
/// row with `relnatts = 3` has matching `pg_attribute` rows to join against.
///
/// This presents PostgreSQL's TOAST schema, not our storage: our chunks carry no
/// `chunk_id`/`chunk_seq` of their own, because the pointer names the first chunk
/// directly and each chunk links to the next. `pg_attribute` is already a
/// presentation layer in exactly this way — it describes every relation in
/// PostgreSQL's terms while the heap stores self-describing datums that look
/// nothing like `attlen`-driven layout.
const TOAST_COLUMNS: [(&str, PgType); 3] = [
    ("chunk_id", PgType::Oid),
    ("chunk_seq", PgType::Int4),
    ("chunk_data", PgType::Bytea),
];

/// `pg_catalog.pg_inherits` — the parent/child links of declarative partitions
/// (and, in PG, table inheritance). One row per leaf partition.
pub fn pg_inherits_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_inherits",
        "pg_catalog",
        vec![
            col("inhrelid", PgType::Oid),
            col("inhparent", PgType::Oid),
            col("inhseqno", PgType::Int4),
            col("inhdetachpending", PgType::Bool),
        ],
    )
}

/// One `pg_inherits` row per leaf partition, linking its OID to its parent's.
/// Both OIDs come from the same positional assignment as `pg_class`, so the
/// `inhrelid`/`inhparent` → `pg_class.oid` joins line up.
pub fn pg_inherits_rows(relations: &[(u32, TableSchema)]) -> Vec<Vec<Value>> {
    let parent_oid = |namespace: &str, name: &str| -> Option<u32> {
        relations
            .iter()
            .find(|(_, s)| s.namespace == namespace && s.name == name)
            .map(|(oid, _)| *oid)
    };
    let mut rows = Vec::new();
    for (oid, schema) in relations {
        if let Some(part) = &schema.partition_of
            && let Some(parent) = parent_oid(&part.parent_namespace, &part.parent_name)
        {
            rows.push(vec![
                Value::Oid(*oid),
                Value::Oid(parent),
                Value::Int4(1),
                Value::Bool(false),
            ]);
        }
    }
    rows
}

/// `pg_catalog.pg_partitioned_table` — one row per partitioned (parent) table,
/// describing its partition key. A curated subset: `partdefid` (the default
/// partition) is always 0 and the class/collation/expression vectors are omitted.
pub fn pg_partitioned_table_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_partitioned_table",
        "pg_catalog",
        vec![
            col("partrelid", PgType::Oid),
            col("partstrat", CHARLIKE),
            col("partnatts", PgType::Int2),
            col("partdefid", PgType::Oid),
            // The 1-based key attnums.
            col("partattrs", INT2VECTOR),
        ],
    )
}

/// One `pg_partitioned_table` row per partitioned parent.
pub fn pg_partitioned_table_rows(relations: &[(u32, TableSchema)]) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for (oid, schema) in relations {
        if let Some(scheme) = &schema.partition_scheme {
            let strat = match scheme.strategy {
                PartitionStrategy::Range => "r",
            };
            let attrs = attnum_vector(scheme.key_columns.iter().copied());
            rows.push(vec![
                Value::Oid(*oid),
                Value::Text(strat.to_string()),
                Value::Int2(scheme.key_columns.len() as i16),
                Value::Oid(0),
                attrs,
            ]);
        }
    }
    rows
}

/// `pg_catalog.pg_attribute` — a curated subset of columns for user relations'
/// columns. System (negative `attnum`) columns are not emitted yet.
pub fn pg_attribute_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_attribute",
        "pg_catalog",
        vec![
            col("attrelid", PgType::Oid),
            col("attname", PgType::Name),
            col("atttypid", PgType::Oid),
            col("attlen", PgType::Int2),
            col("attnum", PgType::Int2),
            col("atttypmod", PgType::Int4),
            col("attnotnull", PgType::Bool),
            col("atthasdef", PgType::Bool),
            col("attidentity", CHARLIKE),
            col("attgenerated", CHARLIKE),
            col("attisdropped", PgType::Bool),
            col("attcollation", PgType::Oid),
        ],
    )
}

/// PostgreSQL's `atttypmod` encoding of a column's declared modifier, from the
/// raw form crabgresql stores in [`Column::typmod`] (a bare length for the
/// character/bit types, `-1` for none). `character`/`character varying` reserve
/// four bytes for the varlena header (`n + VARHDRSZ`); `bit`/`bit varying` store
/// the length directly. Keeping this the true PostgreSQL encoding lets
/// `format_type(atttypid, atttypmod)` reproduce PG's `\d` type strings.
///
/// Two modifiers do not survive to here yet, so `\d` shows the bare type name
/// where PostgreSQL shows a modifier. Both are upstream of this function:
/// - `numeric(p,s)` and `timestamp(p)`/`time(p)`: [`Column::typmod`] is only
///   populated from `length_typmod` (the character and bit types), and it is
///   also what the INSERT path length-coerces against, so persisting these
///   needs its own change rather than a catalog-side re-encode.
/// - a **view**'s columns: a view records its output columns without a modifier
///   (`OutputColumn` carries no typmod), so `\d v` prints `character varying`
///   where PostgreSQL prints `character varying(20)`.
///
/// The addition saturates rather than wrapping: DDL rejects a length beyond
/// PostgreSQL's limit ([`crabgresql_types::text::MAX_CHAR_LENGTH`]), so a value
/// that could overflow is unreachable through a `CREATE TABLE` — but this runs
/// against whatever a data directory already holds, and building a catalog row
/// must never panic the session that reads `pg_attribute`.
fn atttypmod_of(column: &Column) -> i32 {
    const VARHDRSZ: i32 = 4;
    match column.ty {
        _ if column.typmod < 0 => -1,
        PgType::Varchar | PgType::Bpchar => column.typmod.saturating_add(VARHDRSZ),
        _ => column.typmod,
    }
}

/// `attcollation`: the column's explicit `COLLATE`, else the type's own
/// collation — and `0` when the type has none, as PostgreSQL records it.
fn attcollation_of(column: &Column) -> u32 {
    match column.collation {
        Some(oid) => oid,
        None => typcollation_of(column.ty.oid()),
    }
}

/// Build `pg_attribute` rows: one per column of each relation, `attnum` 1-based
/// (user columns only), typed from the column's `PgType` (`atttypid`/`attlen`).
pub fn pg_attribute_rows(
    relations: &[(u32, TableSchema)],
    indexes: &[CatalogIndex],
    toasts: &[CatalogToast],
) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for (oid, schema) in relations {
        for (i, c) in schema.columns.iter().enumerate() {
            rows.push(vec![
                Value::Oid(*oid),
                Value::Text(c.name.clone()),
                Value::Oid(c.ty.oid()),
                Value::Int2(c.ty.typlen()),
                Value::Int2((i + 1) as i16),
                Value::Int4(atttypmod_of(c)),
                Value::Bool(!c.nullable),
                Value::Bool(c.default.is_some()),
                // attidentity / attgenerated: no identity or generated columns.
                Value::Text(String::new()),
                Value::Text(String::new()),
                Value::Bool(false),
                Value::Oid(attcollation_of(c)),
            ]);
        }
    }
    for index in indexes {
        for (position, key) in index.metadata.keys.iter().enumerate() {
            let column = &index.table_schema.columns[key.column];
            rows.push(vec![
                Value::Oid(index.oid),
                Value::Text(column.name.clone()),
                Value::Oid(column.ty.oid()),
                Value::Int2(column.ty.typlen()),
                Value::Int2((position + 1) as i16),
                Value::Int4(atttypmod_of(column)),
                Value::Bool(false),
                Value::Bool(false),
                Value::Text(String::new()),
                Value::Text(String::new()),
                Value::Bool(false),
                Value::Oid(attcollation_of(column)),
            ]);
        }
    }
    // A TOAST relation's columns, so its `pg_class.relnatts` has rows to join.
    for toast in toasts {
        for (i, (name, ty)) in TOAST_COLUMNS.iter().enumerate() {
            rows.push(vec![
                Value::Oid(toast.oid),
                Value::Text((*name).to_string()),
                Value::Oid(ty.oid()),
                Value::Int2(ty.typlen()),
                Value::Int2((i + 1) as i16),
                Value::Int4(-1),
                // PostgreSQL marks all three NOT NULL.
                Value::Bool(true),
                Value::Bool(false),
                Value::Text(String::new()),
                Value::Text(String::new()),
                Value::Bool(false),
                Value::Oid(0),
            ]);
        }
    }
    rows
}

pub fn pg_attrdef_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_attrdef",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("adrelid", PgType::Oid),
            col("adnum", PgType::Int2),
            col("adbin", PgType::Text),
        ],
    )
}

pub fn pg_attrdef_rows(relations: &[(u32, TableSchema)]) -> Vec<Vec<Value>> {
    let mut next_oid = 30000_u32;
    let mut rows = Vec::new();
    for (table_oid, schema) in relations {
        for (position, column) in schema.columns.iter().enumerate() {
            if let Some(default) = &column.default {
                rows.push(vec![
                    Value::Oid(next_oid),
                    Value::Oid(*table_oid),
                    Value::Int2((position + 1) as i16),
                    Value::Text(default.clone()),
                ]);
                next_oid += 1;
            }
        }
    }
    rows
}

pub fn pg_constraint_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_constraint",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("conname", PgType::Name),
            col("connamespace", PgType::Oid),
            col("contype", CHARLIKE),
            col("condeferrable", PgType::Bool),
            col("condeferred", PgType::Bool),
            col("convalidated", PgType::Bool),
            col("conrelid", PgType::Oid),
            col("conindid", PgType::Oid),
            // int2[] is represented as PG array text until catalog arrays land.
            col("conkey", PgType::Text),
        ],
    )
}

pub fn pg_constraint_rows(
    relations: &[(u32, TableSchema)],
    indexes: &[CatalogIndex],
    namespace_oids: &HashMap<String, u32>,
) -> Vec<Vec<Value>> {
    let nsp_oid = |namespace: &str| namespace_oids.get(namespace).copied().unwrap_or(2200);
    let mut next_oid = 31000_u32;
    let mut rows = Vec::new();
    for (table_oid, schema) in relations {
        for (position, column) in schema.columns.iter().enumerate() {
            if let Some(name) = &column.not_null_constraint {
                rows.push(constraint_row(
                    next_oid,
                    name,
                    nsp_oid(&schema.namespace),
                    "n",
                    *table_oid,
                    0,
                    &[position],
                ));
                next_oid += 1;
            }
        }
    }
    for index in indexes {
        if let Some(constraint) = index.metadata.constraint {
            rows.push(constraint_row(
                next_oid,
                &index.metadata.name,
                nsp_oid(&index.table_schema.namespace),
                match constraint {
                    IndexConstraint::PrimaryKey => "p",
                    IndexConstraint::Unique => "u",
                },
                index.table_oid,
                index.oid,
                &index
                    .metadata
                    .keys
                    .iter()
                    .map(|key| key.column)
                    .collect::<Vec<_>>(),
            ));
            next_oid += 1;
        }
    }
    rows
}

fn constraint_row(
    oid: u32,
    name: &str,
    connamespace: u32,
    kind: &str,
    table_oid: u32,
    index_oid: u32,
    columns: &[usize],
) -> Vec<Value> {
    vec![
        Value::Oid(oid),
        Value::Text(name.to_string()),
        Value::Oid(connamespace),
        Value::Text(kind.to_string()),
        Value::Bool(false),
        Value::Bool(false),
        Value::Bool(true),
        Value::Oid(table_oid),
        Value::Oid(index_oid),
        Value::Text(format!(
            "{{{}}}",
            columns
                .iter()
                .map(|column| (column + 1).to_string())
                .collect::<Vec<_>>()
                .join(",")
        )),
    ]
}

pub fn pg_index_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_index",
        "pg_catalog",
        vec![
            col("indexrelid", PgType::Oid),
            col("indrelid", PgType::Oid),
            col("indnatts", PgType::Int2),
            col("indnkeyatts", PgType::Int2),
            col("indisunique", PgType::Bool),
            col("indnullsnotdistinct", PgType::Bool),
            col("indisprimary", PgType::Bool),
            col("indimmediate", PgType::Bool),
            col("indisvalid", PgType::Bool),
            col("indkey", INT2VECTOR),
            col("indoption", INT2VECTOR),
        ],
    )
}

pub fn pg_index_rows(indexes: &[CatalogIndex]) -> Vec<Vec<Value>> {
    indexes
        .iter()
        .map(|index| {
            // 1-based key attnums, as PG's `indkey` holds.
            let indkey = attnum_vector(index.metadata.keys.iter().map(|key| key.column));
            let indoption = int2vector(index.metadata.keys.iter().map(|key| {
                let mut option = 0;
                if key.descending {
                    option |= 1;
                }
                if key.nulls_first {
                    option |= 2;
                }
                option
            }));
            vec![
                Value::Oid(index.oid),
                Value::Oid(index.table_oid),
                Value::Int2(index.metadata.keys.len() as i16),
                Value::Int2(index.metadata.keys.len() as i16),
                Value::Bool(index.metadata.unique),
                Value::Bool(!index.metadata.nulls_distinct),
                Value::Bool(index.metadata.constraint == Some(IndexConstraint::PrimaryKey)),
                Value::Bool(true),
                Value::Bool(true),
                indkey,
                indoption,
            ]
        })
        .collect()
}

/// Fixed `pg_namespace` rows: the reserved catalog/toast schemas plus `public`.
/// OIDs match PostgreSQL's stable assignments (`pg_catalog` = 11, `pg_toast` =
/// 99, `public` = 2200). `information_schema` has an initdb-assigned OID, so
/// it remains absent here; its named discovery surface lives in
/// `information_schema.schemata`. Owners are reported as the bootstrap
/// superuser (10) for now.
pub fn pg_namespace_rows(user_schemas: &[(String, u32)]) -> Vec<Vec<Value>> {
    let row = |oid: u32, name: &str| {
        vec![
            Value::Oid(oid),
            Value::Text(name.to_string()),
            Value::Oid(BOOTSTRAP_ROLE_OID),
            Value::Null,
        ]
    };
    let mut rows = vec![
        row(11, "pg_catalog"),
        row(99, "pg_toast"),
        row(2200, "public"),
    ];
    for (name, oid) in user_schemas {
        rows.push(row(*oid, name));
    }
    rows
}

/// `information_schema.schemata`. Information-schema domains are represented
/// as text until the engine supports domains over the built-in types.
pub fn information_schema_schemata_schema() -> TableSchema {
    TableSchema::in_namespace(
        "schemata",
        "information_schema",
        vec![
            col("catalog_name", PgType::Text),
            col("schema_name", PgType::Text),
            col("schema_owner", PgType::Text),
            col("default_character_set_catalog", PgType::Text),
            col("default_character_set_schema", PgType::Text),
            col("default_character_set_name", PgType::Text),
            col("sql_path", PgType::Text),
        ],
    )
}

pub fn information_schema_schemata_rows(
    database: &str,
    owner: &str,
    relations: &[CatalogRelation],
    user_schemas: &[(String, u32)],
) -> Vec<Vec<Value>> {
    let mut namespaces = vec![
        "information_schema".to_string(),
        "pg_catalog".to_string(),
        "pg_toast".to_string(),
        "public".to_string(),
    ];
    for relation in relations {
        if !namespaces.contains(&relation.namespace) {
            namespaces.push(relation.namespace.clone());
        }
    }
    // Include freshly-created, still-empty user schemas (no relations yet).
    for (name, _) in user_schemas {
        if !namespaces.contains(name) {
            namespaces.push(name.clone());
        }
    }
    namespaces.sort();
    namespaces
        .into_iter()
        .map(|namespace| {
            vec![
                Value::Text(database.to_string()),
                Value::Text(namespace),
                Value::Text(owner.to_string()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]
        })
        .collect()
}

/// `information_schema.tables` for represented user relations. Catalog and
/// information-schema implementation relations are deliberately not invented:
/// their complete PostgreSQL metadata is not modeled yet.
pub fn information_schema_tables_schema() -> TableSchema {
    TableSchema::in_namespace(
        "tables",
        "information_schema",
        vec![
            col("table_catalog", PgType::Text),
            col("table_schema", PgType::Text),
            col("table_name", PgType::Text),
            col("table_type", PgType::Text),
            col("self_referencing_column_name", PgType::Text),
            col("reference_generation", PgType::Text),
            col("user_defined_type_catalog", PgType::Text),
            col("user_defined_type_schema", PgType::Text),
            col("user_defined_type_name", PgType::Text),
            col("is_insertable_into", PgType::Text),
            col("is_typed", PgType::Text),
            col("commit_action", PgType::Text),
        ],
    )
}

pub fn information_schema_tables_rows(
    database: &str,
    relations: &[CatalogRelation],
) -> Vec<Vec<Value>> {
    relations
        .iter()
        // Sequences are not tables: PG omits them from information_schema.tables.
        .filter(|relation| relation.kind != RelKind::Sequence)
        .map(|relation| {
            vec![
                Value::Text(database.to_string()),
                Value::Text(relation.namespace.clone()),
                Value::Text(relation.schema.name.clone()),
                Value::Text(
                    match (relation.kind, relation.temporary) {
                        (RelKind::View, _) => "VIEW",
                        (RelKind::Table, true) => "LOCAL TEMPORARY",
                        (RelKind::Table, false) => "BASE TABLE",
                        // A partitioned parent reflects as BASE TABLE, as in PG.
                        (RelKind::PartitionedTable, _) => "BASE TABLE",
                        (RelKind::Sequence, _) => unreachable!("filtered out above"),
                    }
                    .to_string(),
                ),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Text("YES".to_string()),
                Value::Text("NO".to_string()),
                Value::Null,
            ]
        })
        .collect()
}

/// `information_schema.columns`, including all PostgreSQL-documented columns.
pub fn information_schema_columns_schema() -> TableSchema {
    let text = PgType::Text;
    let cardinal = PgType::Int4;
    TableSchema::in_namespace(
        "columns",
        "information_schema",
        vec![
            col("table_catalog", text),
            col("table_schema", text),
            col("table_name", text),
            col("column_name", text),
            col("ordinal_position", cardinal),
            col("column_default", text),
            col("is_nullable", text),
            col("data_type", text),
            col("character_maximum_length", cardinal),
            col("character_octet_length", cardinal),
            col("numeric_precision", cardinal),
            col("numeric_precision_radix", cardinal),
            col("numeric_scale", cardinal),
            col("datetime_precision", cardinal),
            col("interval_type", text),
            col("interval_precision", cardinal),
            col("character_set_catalog", text),
            col("character_set_schema", text),
            col("character_set_name", text),
            col("collation_catalog", text),
            col("collation_schema", text),
            col("collation_name", text),
            col("domain_catalog", text),
            col("domain_schema", text),
            col("domain_name", text),
            col("udt_catalog", text),
            col("udt_schema", text),
            col("udt_name", text),
            col("scope_catalog", text),
            col("scope_schema", text),
            col("scope_name", text),
            col("maximum_cardinality", cardinal),
            col("dtd_identifier", text),
            col("is_self_referencing", text),
            col("is_identity", text),
            col("identity_generation", text),
            col("identity_start", text),
            col("identity_increment", text),
            col("identity_maximum", text),
            col("identity_minimum", text),
            col("identity_cycle", text),
            col("is_generated", text),
            col("generation_expression", text),
            col("is_updatable", text),
        ],
    )
}

pub fn information_schema_columns_rows(
    database: &str,
    relations: &[CatalogRelation],
) -> Vec<Vec<Value>> {
    relations
        .iter()
        // Sequences are not tables; omit their columns from information_schema.
        .filter(|relation| relation.kind != RelKind::Sequence)
        .flat_map(|relation| {
            relation
                .schema
                .columns
                .iter()
                .enumerate()
                .map(move |(index, column)| {
                    let (character_length, character_octets) = match column.ty {
                        PgType::Varchar | PgType::Bpchar if column.typmod >= 0 => {
                            (Value::Int4(column.typmod), Value::Int4(column.typmod * 4))
                        }
                        PgType::Bit | PgType::Varbit if column.typmod >= 0 => {
                            (Value::Int4(column.typmod), Value::Null)
                        }
                        _ => (Value::Null, Value::Null),
                    };
                    let (precision, radix) = match column.ty {
                        PgType::Int2 => (Value::Int4(16), Value::Int4(2)),
                        PgType::Int4 => (Value::Int4(32), Value::Int4(2)),
                        PgType::Int8 => (Value::Int4(64), Value::Int4(2)),
                        PgType::Float4 => (Value::Int4(24), Value::Int4(2)),
                        PgType::Float8 => (Value::Int4(53), Value::Int4(2)),
                        _ => (Value::Null, Value::Null),
                    };
                    let datetime_precision = match column.ty {
                        PgType::Time
                        | PgType::TimeTz
                        | PgType::Timestamp
                        | PgType::TimestampTz
                        | PgType::Interval => Value::Int4(6),
                        _ => Value::Null,
                    };
                    // PG's view joins pg_collation but excludes
                    // `pg_catalog.default`, so a column left on the database
                    // collation reports NULL here rather than "default".
                    let collation =
                        crabgresql_types::collation::lookup_by_oid(attcollation_of(column))
                            .filter(|c| c.name != "default");
                    let (collation_catalog, collation_schema, collation_name) = match collation {
                        Some(c) => (
                            Value::Text(database.to_string()),
                            Value::Text("pg_catalog".to_string()),
                            Value::Text(c.name.to_string()),
                        ),
                        None => (Value::Null, Value::Null, Value::Null),
                    };
                    vec![
                        Value::Text(database.to_string()),
                        Value::Text(relation.namespace.clone()),
                        Value::Text(relation.schema.name.clone()),
                        Value::Text(column.name.clone()),
                        Value::Int4((index + 1) as i32),
                        column
                            .default
                            .as_ref()
                            .map(|default| Value::Text(default.clone()))
                            .unwrap_or(Value::Null),
                        Value::Text(if column.nullable { "YES" } else { "NO" }.to_string()),
                        Value::Text(column.ty.name().to_string()),
                        character_length,
                        character_octets,
                        precision,
                        radix,
                        // numeric_scale
                        Value::Null,
                        datetime_precision,
                        // interval_type, interval_precision
                        Value::Null,
                        Value::Null,
                        // character_set_{catalog,schema,name}
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        collation_catalog,
                        collation_schema,
                        collation_name,
                        // domain_{catalog,schema,name}
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Text(database.to_string()),
                        Value::Text("pg_catalog".to_string()),
                        Value::Text(column.ty.typname().to_string()),
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Text((index + 1).to_string()),
                        Value::Text("NO".to_string()),
                        Value::Text("NO".to_string()),
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Text("NEVER".to_string()),
                        Value::Null,
                        Value::Text("YES".to_string()),
                    ]
                })
        })
        .collect()
}

/// The `oidvector` and `int2vector` catalog column types. See
/// [`crabgresql_types::vector`].
const OIDVECTOR: PgType = PgType::Vector(VectorKind::Oid);
const INT2VECTOR: PgType = PgType::Vector(VectorKind::Int2);

/// Build an [`OIDVECTOR`] value from a sequence of OIDs.
fn oidvector(elems: impl IntoIterator<Item = u32>) -> Value {
    Value::Vector {
        kind: VectorKind::Oid,
        elems: elems.into_iter().map(Value::Oid).collect(),
    }
}

/// Build an [`INT2VECTOR`] value from a sequence of `int2`s.
fn int2vector(elems: impl IntoIterator<Item = i16>) -> Value {
    Value::Vector {
        kind: VectorKind::Int2,
        elems: elems.into_iter().map(Value::Int2).collect(),
    }
}

/// Build an [`INT2VECTOR`] of 1-based attribute numbers from 0-based column
/// indexes — the shape of `pg_index.indkey` and
/// `pg_partitioned_table.partattrs`.
///
/// `attnum` is an `int2` in PostgreSQL, which caps a relation at 32767 columns;
/// PostgreSQL never reaches that because it rejects a table past 1600 columns,
/// but this build has no such limit. A column index that does not fit is
/// reported as `0`, which is already PostgreSQL's `indkey` sentinel for "this
/// key is not a plain column reference" — the closest honest rendering. It must
/// not be a bare `as i16`: that panics on overflow in a debug build and wraps to
/// a negative attnum in a release one.
fn attnum_vector(columns: impl IntoIterator<Item = usize>) -> Value {
    int2vector(
        columns
            .into_iter()
            .map(|c| i16::try_from(c.saturating_add(1)).unwrap_or(0)),
    )
}

/// `pg_catalog.pg_language`.
pub fn pg_language_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_language",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("lanname", PgType::Name),
            col("lanowner", PgType::Oid),
            col("lanispl", PgType::Bool),
            col("lanpltrusted", PgType::Bool),
            col("lanplcallfoid", PgType::Oid),
            col("laninline", PgType::Oid),
            col("lanvalidator", PgType::Oid),
            col("lanacl", PgType::Text),
        ],
    )
}

/// The fixed `pg_language` rows.
///
/// 12/13/14 are PostgreSQL's bootstrap OIDs and are stable across versions.
/// `plpgsql`'s is not: PostgreSQL assigns it through `CREATE EXTENSION` at
/// initdb time, so it varies by build and there is nothing to reproduce —
/// clients match on `lanname`. The handler OIDs stay 0 until `pg_proc` carries
/// built-in rows for them to point at.
pub fn pg_language_rows() -> Vec<Vec<Value>> {
    let row = |oid: u32, name: &str, ispl: bool, trusted: bool| {
        vec![
            Value::Oid(oid),
            Value::Text(name.to_string()),
            Value::Oid(BOOTSTRAP_ROLE_OID),
            Value::Bool(ispl),
            Value::Bool(trusted),
            Value::Oid(0),
            Value::Oid(0),
            Value::Oid(0),
            Value::Null,
        ]
    };
    vec![
        row(12, "internal", false, false),
        row(13, "c", false, false),
        row(14, "sql", false, true),
        row(PLPGSQL_LANG_OID, "plpgsql", true, true),
    ]
}

/// The `pg_language` OID this build gives `plpgsql`. See [`pg_language_rows`]
/// for why it is ours to choose.
pub const PLPGSQL_LANG_OID: u32 = 13540;

/// `pg_catalog.pg_proc` — the columns clients read, in PostgreSQL's order.
pub fn pg_proc_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_proc",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("proname", PgType::Name),
            col("pronamespace", PgType::Oid),
            col("proowner", PgType::Oid),
            col("prolang", PgType::Oid),
            col("procost", PgType::Float4),
            col("prorows", PgType::Float4),
            col("provariadic", PgType::Oid),
            col("prosupport", CHARLIKE),
            col("prokind", CHARLIKE),
            col("prosecdef", PgType::Bool),
            col("proleakproof", PgType::Bool),
            col("proisstrict", PgType::Bool),
            col("proretset", PgType::Bool),
            col("provolatile", CHARLIKE),
            col("proparallel", CHARLIKE),
            col("pronargs", PgType::Int2),
            col("pronargdefaults", PgType::Int2),
            col("prorettype", PgType::Oid),
            col("proargtypes", OIDVECTOR),
            col("proallargtypes", PgType::Array(crabgresql_types::oid::OID)),
            col("proargmodes", PgType::Array(crabgresql_types::oid::TEXT)),
            col("proargnames", PgType::Array(crabgresql_types::oid::TEXT)),
            col("prosrc", PgType::Text),
            col("probin", PgType::Text),
        ],
    )
}

/// The `pg_proc` rows for the routines this server holds.
///
/// Honest for everything the catalog actually knows. The stubs are the columns
/// nothing here can have an opinion about yet, each set to PostgreSQL's own
/// default rather than to zero: `procost`/`prorows` (no planner cost model),
/// `provariadic`/`pronargdefaults` (VARIADIC and argument defaults are
/// rejected), `prosupport`/`proleakproof`/`proparallel`. `probin` is NULL
/// honestly — there are no C functions.
pub fn pg_proc_rows(
    routines: &[CatalogRoutine],
    namespace_oids: &HashMap<String, u32>,
) -> Vec<Vec<Value>> {
    routines
        .iter()
        .map(|r| {
            // PostgreSQL leaves these NULL rather than empty when there is
            // nothing to report, and clients test for NULL.
            let optional_array = |elem: PgType, values: Vec<Value>| {
                if values.is_empty() {
                    Value::Null
                } else {
                    Value::Array {
                        elem,
                        elems: values,
                    }
                }
            };
            vec![
                Value::Oid(r.oid),
                Value::Text(r.name.clone()),
                Value::Oid(namespace_oids.get(&r.namespace).copied().unwrap_or(2200)),
                Value::Oid(BOOTSTRAP_ROLE_OID),
                Value::Oid(r.lang),
                // PostgreSQL's defaults: 1 for a built-in, 100 for anything
                // whose body it has to run.
                Value::Float4(if r.lang == 12 || r.lang == 13 {
                    1.0
                } else {
                    100.0
                }),
                Value::Float4(if r.retset { 1000.0 } else { 0.0 }),
                Value::Oid(0),
                Value::Text("-".to_string()),
                Value::Text(r.kind.to_string()),
                Value::Bool(r.secdef),
                Value::Bool(false),
                Value::Bool(r.strict),
                Value::Bool(r.retset),
                Value::Text(r.volatile.to_string()),
                Value::Text("u".to_string()),
                Value::Int2(r.arg_types.len() as i16),
                Value::Int2(0),
                Value::Oid(r.ret_type),
                oidvector(r.arg_types.iter().copied()),
                optional_array(
                    PgType::Oid,
                    r.all_arg_types.iter().map(|t| Value::Oid(*t)).collect(),
                ),
                optional_array(
                    PgType::Text,
                    r.arg_modes
                        .iter()
                        .map(|m| Value::Text(m.to_string()))
                        .collect(),
                ),
                optional_array(
                    PgType::Text,
                    r.arg_names.iter().map(|n| Value::Text(n.clone())).collect(),
                ),
                Value::Text(r.src.clone()),
                Value::Null,
            ]
        })
        .collect()
}
