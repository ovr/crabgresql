//! `pg_class`: every live relation, index, TOAST relation and sequence.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{ByteaOutput, FmtCtx, PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;
use crate::oids::*;
use crabgresql_storage_api::{
    IndexMethod, PartitionBoundDatum, PartitionOf, RelStats, TableAccessMethod,
};

use crate::{RelKind, TOAST_NAMESPACE};

/// `pg_catalog.pg_class` — a curated subset of columns for user relations, in
/// PostgreSQL's `attnum` order. Columns crabgresql has no state for are still
/// emitted with their true constant so a client's `\d` predicates evaluate as on
/// PG (e.g. `relchecks = 0` gates the CHECK-constraint listing *off*).
///
/// TODO: the storage and inheritance columns (`relallfrozen`, `relisshared`,
/// `relhassubclass`, `relispopulated`, `relrewrite`, `relfrozenxid`,
/// `relminmxid`, `reloptions`) are absent, so a query naming one fails with
/// "column does not exist" rather than reading a value.
///
/// `relfilenode` is the relation's physical file, taken from the storage engine
/// rather than mirrored off the OID — see [`pg_class_rows`] for the two places
/// it deviates from PostgreSQL.
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
pub(crate) fn pg_class_schema() -> TableSchema {
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
            col("relfilenode", PgType::Oid),
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
            col("relacl", ACLITEM_ARRAY),
            // pg_node_tree in PG; crabgresql stores the already-deparsed
            // `FOR VALUES …` text (see `pg_get_expr`, which just echoes it).
            col("relpartbound", PgType::Text),
        ],
    )
}

/// Deparse a leaf partition's `relpartbound` to the text PostgreSQL's
/// `pg_get_expr(relpartbound, oid)` prints — `FOR VALUES FROM (…) TO (…)`.
/// `MINVALUE`/`MAXVALUE` print as bare keywords. Storing the final text (not a
/// node tree) is a deliberate deviation: `pg_get_expr` then just echoes it.
///
/// TODO: deparse LIST (`FOR VALUES IN (…)`) and HASH (`FOR VALUES WITH
/// (modulus …, remainder …)`, lower-case as PostgreSQL 18.4 prints it) bounds;
/// only RANGE partitions can be created, so only the range form is produced.
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
/// A `bytea` bound takes the *reader's* `bytea_output`, as it does in
/// PostgreSQL: the bound is a datum there and `pg_get_expr` runs it through
/// `byteaout`. Since we store the rendered text instead, the setting has to
/// arrive here rather than at read time. Everything else stays UTC/default —
/// a zone-dependent bound is frozen at DDL time, which was already true.
fn deparse_partbound(part: &PartitionOf, bytea_output: ByteaOutput) -> String {
    let fmt = FmtCtx::utc_default().with_bytea_output(bytea_output);
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
            let text = v.encode_text_with(&fmt).unwrap_or_default();
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
/// Columns crabgresql does not track are their PostgreSQL constants: rules only
/// on views (`relhasrules`), no triggers or row security, no `OF type` and no
/// tablespace of its own. `relchecks` counts the relation's CHECK constraints,
/// which is what makes psql print a `Check constraints:` footer. A heap-backed
/// relation defaults its replica identity to the primary key
/// (`relreplident = 'd'`); views, sequences, and indexes have none (`'n'`).
///
/// `stats` is parallel to `relations`; see [`analyzed_size`] for how it renders.
///
/// `relfilenode` is the engine's real file number, not the OID — crabgresql
/// implements TRUNCATE as a relfilenode swap, so mirroring the OID here would be
/// a lie the very next `TRUNCATE` exposes. Two deviations remain:
///
/// * A **partitioned parent** reports `0`, the PostgreSQL answer for a relation
///   with no storage, even though our engine did give it a heap file. The file
///   exists as an artifact of creating every table the same way and never holds
///   a row: routing sends every tuple to a leaf. Reporting the number instead
///   would claim storage that is not there — and it is exactly what upstream's
///   `sanity_check` checks for.
/// * A **TRUNCATE in an open transaction** still reports the old number until it
///   commits, because the swap lands in the relation catalog at `TxnFinalize`.
///   PostgreSQL shows the new one from inside the transaction.
///
/// A **sequence** reports a number that names no file: its counter lives in the
/// relation catalog rather than in a one-page relation as in PostgreSQL. The
/// number is real and stable — allocated from the same monotonic counter as
/// every table's — so it is `0`, meaning "no storage at all", that would be the
/// wrong answer for a `relkind = 'S'` row.
pub(crate) fn pg_class_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let (relations, kinds, stats) = (
        cat.relation_oids(),
        cat.relation_kinds(),
        cat.relation_stats(),
    );
    let filenodes = cat.relation_filenodes();
    let (indexes, toasts, namespace_oids) =
        (cat.index_oids(), cat.toast_oids(), cat.namespace_oids());
    // Resolve a relation's namespace OID, defaulting to `public` (2200) for any
    // namespace not in the map (should not happen for a live relation).
    let nsp_oid = |namespace: &str| namespace_oids.get(namespace).copied().unwrap_or(2200);
    let mut rows: Vec<Vec<Value>> = relations
        .iter()
        .zip(kinds)
        .zip(stats)
        .zip(filenodes)
        .map(|((((oid, schema), kind), stats), filenodes)| {
            // A partitioned parent has no access method (`relam = 0`) and holds no
            // storage of its own.
            let (relam, relkind) = match kind {
                RelKind::Table => (
                    match schema.access_method {
                        TableAccessMethod::Heap => HEAP_AM_OID,
                        TableAccessMethod::Parquet => PARQUET_AM_OID,
                        TableAccessMethod::Buffer => BUFFER_AM_OID,
                    },
                    'r',
                ),
                RelKind::PartitionedTable => (0, 'p'),
                RelKind::View => (0, 'v'),
                RelKind::Sequence => (0, 'S'),
            };
            // Heap-backed relations (ordinary + partitioned tables) default their
            // replica identity to the primary key; the rest carry none.
            let relreplident = match kind {
                RelKind::Table | RelKind::PartitionedTable => 'd',
                RelKind::View | RelKind::Sequence => 'n',
            };
            let relpartbound = match &schema.partition_of {
                Some(part) => Value::Text(deparse_partbound(part, cat.bytea_output())),
                None => Value::Null,
            };
            // A sequence is one page holding its single row, and PostgreSQL
            // reports it that way from creation — there is nothing to analyze.
            let (relpages, reltuples) = match kind {
                RelKind::Sequence => (Value::Int4(1), Value::Float4(1.0)),
                _ => analyzed_size(stats),
            };
            // A relation PostgreSQL gives no storage reports 0 whatever the
            // engine allocated for it; see this function's doc comment.
            let relfilenode = match kind {
                RelKind::Table | RelKind::Sequence => filenodes.rel,
                RelKind::PartitionedTable | RelKind::View => 0,
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
                Value::Oid(relfilenode),
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
                chr(schema.persistence.as_char()),
                chr(relkind),
                Value::Int2(schema.columns.len() as i16),
                // relchecks: the CHECK constraints on this relation, inherited
                // ones included — PostgreSQL counts a child's copies too.
                Value::Int2(schema.checks.len() as i16),
                // relhasrules: only a view carries the `_RETURN` rule.
                Value::Bool(matches!(kind, RelKind::View)),
                // relhastriggers / relrowsecurity / relforcerowsecurity.
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
                chr(relreplident),
                Value::Bool(schema.partition_of.is_some()),
                Value::Null,
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
            // relfilenode: 0 only for an index with no file at all. An index the
            // planner refuses to read but whose file exists still names it —
            // `relfilenode` is storage, not readability.
            Value::Oid(index.relfilenode),
            Value::Oid(0),
            // TODO: report an index's own relpages/reltuples — per-index size
            // is not tracked, so an index keeps the never-analyzed sentinel
            // even after the table is analyzed. relallvisible: no map.
            Value::Int4(0),
            Value::Float4(-1.0),
            Value::Int4(0),
            Value::Oid(0),
            Value::Bool(false),
            chr('p'),
            chr('i'),
            Value::Int2(index.metadata.keys.len() as i16),
            Value::Int2(0),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            // An index has no replica identity of its own.
            chr('n'),
            Value::Bool(false),
            Value::Null,
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
            Value::Oid(toast.relfilenode),
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
            chr(toast.persistence.as_char()),
            chr('t'),
            Value::Int2(TOAST_COLUMNS.len() as i16),
            Value::Int2(0),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            chr('n'),
            Value::Bool(false),
            Value::Null,
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
pub(crate) const TOAST_COLUMNS: [(&str, PgType); 3] = [
    ("chunk_id", PgType::Oid),
    ("chunk_seq", PgType::Int4),
    ("chunk_data", PgType::Bytea),
];
