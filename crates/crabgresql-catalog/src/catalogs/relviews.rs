//! The per-relkind views PostgreSQL defines over `pg_class`: `pg_tables`,
//! `pg_views`, `pg_matviews`, `pg_sequences` and `pg_indexes`.
//!
//! Nothing here is new state — every column is already in the snapshot that
//! `pg_class`, `pg_index` and `pg_sequence` are built from. What they add is the
//! shape clients actually query: `\dt` and `pg_dump` reach for `pg_tables`
//! rather than assembling the three-way join themselves.
//!
//! Rows come out sorted by `(schema, name)`, the order [`SystemCatalog`] numbers
//! relations in. PostgreSQL leaves the order of a view unspecified, so this is a
//! free choice — made deterministic on purpose, because a catalog whose row
//! order moves between two identical queries is a nuisance to diff.

use crabgresql_storage_api::{TableSchema, index_definition};
use crabgresql_types::{PgType, RegKind, Value};

use crate::SystemCatalog;
use crate::cols::*;
use crate::source::RelKind;

/// The relations of one kind, sorted the way `pg_class` numbers them.
fn relations_of<'a>(cat: &'a SystemCatalog, kinds: &[RelKind]) -> Vec<&'a crate::CatalogRelation> {
    let mut rels: Vec<_> = cat
        .live_relations()
        .iter()
        .filter(|r| kinds.contains(&r.kind))
        .collect();
    rels.sort_by(|a, b| {
        a.namespace
            .cmp(&b.namespace)
            .then_with(|| a.schema.name.cmp(&b.schema.name))
    });
    rels
}

pub(crate) fn pg_tables_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_tables",
        "pg_catalog",
        vec![
            col("schemaname", PgType::Name),
            col("tablename", PgType::Name),
            col("tableowner", PgType::Name),
            col("tablespace", PgType::Name),
            col("hasindexes", PgType::Bool),
            col("hasrules", PgType::Bool),
            col("hastriggers", PgType::Bool),
            col("rowsecurity", PgType::Bool),
        ],
    )
}

/// One row per ordinary and partitioned table, as `\dt` lists them.
///
/// The last three flags are constant `false` and agree with `pg_class`, which
/// reports the same for `relhasrules`/`relhastriggers`/`relrowsecurity`: there
/// is no `CREATE RULE`, no `CREATE TRIGGER` and no RLS here, so no table can
/// have one. `tablespace` is NULL — the default tablespace, which PostgreSQL
/// also reports as NULL rather than as `pg_default`.
pub(crate) fn pg_tables_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let owner = cat.owner().to_string();
    relations_of(cat, &[RelKind::Table, RelKind::PartitionedTable])
        .into_iter()
        .map(|relation| {
            vec![
                Value::Text(relation.schema.namespace.clone()),
                Value::Text(relation.schema.name.clone()),
                Value::Text(owner.clone()),
                Value::Null,
                Value::Bool(!relation.indexes.is_empty()),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
            ]
        })
        .collect()
}

pub(crate) fn pg_views_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_views",
        "pg_catalog",
        vec![
            col("schemaname", PgType::Name),
            col("viewname", PgType::Name),
            col("viewowner", PgType::Name),
            col("definition", PgType::Text),
        ],
    )
}

/// One row per view. `definition` is the deparsed body the source supplied; a
/// view the deparser could not render reports NULL rather than a body that is
/// not the one that was created — see [`crate::CatalogRelation::definition`].
pub(crate) fn pg_views_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let owner = cat.owner().to_string();
    relations_of(cat, &[RelKind::View])
        .into_iter()
        .map(|relation| {
            vec![
                Value::Text(relation.schema.namespace.clone()),
                Value::Text(relation.schema.name.clone()),
                Value::Text(owner.clone()),
                match &relation.definition {
                    Some(sql) => Value::Text(sql.clone()),
                    None => Value::Null,
                },
            ]
        })
        .collect()
}

pub(crate) fn pg_matviews_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_matviews",
        "pg_catalog",
        vec![
            col("schemaname", PgType::Name),
            col("matviewname", PgType::Name),
            col("matviewowner", PgType::Name),
            col("tablespace", PgType::Name),
            col("hasindexes", PgType::Bool),
            col("ispopulated", PgType::Bool),
            col("definition", PgType::Text),
        ],
    )
}

pub(crate) fn pg_sequences_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_sequences",
        "pg_catalog",
        vec![
            col("schemaname", PgType::Name),
            col("sequencename", PgType::Name),
            col("sequenceowner", PgType::Name),
            col("data_type", PgType::Reg(RegKind::Type)),
            col("start_value", PgType::Int8),
            col("min_value", PgType::Int8),
            col("max_value", PgType::Int8),
            col("increment_by", PgType::Int8),
            col("cycle", PgType::Bool),
            col("cache_size", PgType::Int8),
            col("last_value", PgType::Int8),
        ],
    )
}

/// One row per sequence, with its parameters as `CREATE SEQUENCE` set them.
///
/// `last_value` is NULL until the sequence has been read from, which is
/// PostgreSQL's distinction between a fresh sequence and one that has already
/// handed out its start value — the counter holds the same number in both.
pub(crate) fn pg_sequences_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let owner = cat.owner().to_string();
    relations_of(cat, &[RelKind::Sequence])
        .into_iter()
        .filter_map(|relation| {
            let params = relation.sequence.as_ref()?;
            Some(vec![
                Value::Text(relation.schema.namespace.clone()),
                Value::Text(relation.schema.name.clone()),
                Value::Text(owner.clone()),
                regtype_named(cat, params.type_oid),
                Value::Int8(params.start),
                Value::Int8(params.min),
                Value::Int8(params.max),
                Value::Int8(params.increment),
                Value::Bool(params.cycle),
                Value::Int8(params.cache),
                match params.last_value {
                    Some(value) => Value::Int8(value),
                    None => Value::Null,
                },
            ])
        })
        .collect()
}

pub(crate) fn pg_indexes_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_indexes",
        "pg_catalog",
        vec![
            col("schemaname", PgType::Name),
            col("tablename", PgType::Name),
            col("indexname", PgType::Name),
            col("tablespace", PgType::Name),
            col("indexdef", PgType::Text),
        ],
    )
}

/// One row per index, in the order `pg_class` numbers indexes in.
///
/// `indexdef` is built by [`index_definition`], the same renderer
/// `pg_get_indexdef` calls, so the view and the function cannot disagree.
pub(crate) fn pg_indexes_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    cat.index_oids()
        .iter()
        .map(|index| {
            vec![
                Value::Text(index.table_schema.namespace.clone()),
                Value::Text(index.table_schema.name.clone()),
                Value::Text(index.metadata.name.clone()),
                Value::Null,
                Value::Text(index_definition(&index.metadata, &index.table_schema)),
            ]
        })
        .collect()
}
