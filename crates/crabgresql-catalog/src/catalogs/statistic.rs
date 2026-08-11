//! `pg_statistic` and the `pg_stats` view over it: what `ANALYZE` measured
//! about each column.
//!
//! `pg_statistic`'s shape is PostgreSQL's, which is deliberately generic: a row
//! per `(relation, column)` carrying five interchangeable *slots*, each a kind
//! tag plus a numbers array and a values array. A kind says how to read the
//! pair, so a new kind of statistic needs no new column. Three kinds are
//! populated here, the three `ANALYZE` collects:
//!
//! | kind | numbers | values |
//! |---|---|---|
//! | 1 `MCV` | each value's frequency | the most common values |
//! | 2 `HISTOGRAM` | — | the bucket bounds |
//! | 3 `CORRELATION` | physical/logical order correlation | — |
//!
//! **Deviation.** PostgreSQL types the value arrays `anyarray`, whose element
//! type varies per row with the column being described. A relation here has one
//! type per column, so those are `text` holding the array's rendered form —
//! which is what `psql` prints for the `anyarray` too, so the output matches
//! while the declared type does not.
//!
//! A never-analyzed relation contributes no rows at all, exactly as in
//! PostgreSQL: absence is how "not measured" is spelled, distinct from a
//! measured zero.

use crabgresql_storage_api::{ColStats, TableSchema};
use crabgresql_types::{FmtCtx, PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;

/// PostgreSQL's `STATISTIC_KIND_MCV`.
const KIND_MCV: i16 = 1;
/// PostgreSQL's `STATISTIC_KIND_HISTOGRAM`.
const KIND_HISTOGRAM: i16 = 2;
/// PostgreSQL's `STATISTIC_KIND_CORRELATION`.
const KIND_CORRELATION: i16 = 3;

/// How many slots a `pg_statistic` row carries. Fixed by the catalog's shape,
/// not by how many are filled.
const SLOTS: usize = 5;

pub(crate) fn pg_statistic_schema() -> TableSchema {
    let mut columns = vec![
        col("starelid", PgType::Oid),
        col("staattnum", PgType::Int2),
        col("stainherit", PgType::Bool),
        col("stanullfrac", PgType::Float4),
        col("stawidth", PgType::Int4),
        col("stadistinct", PgType::Float4),
    ];
    // The five slots, in PostgreSQL's column order: every kind, then every
    // operator, then every collation, then the two array families.
    columns.extend((1..=SLOTS).map(|i| col(&format!("stakind{i}"), PgType::Int2)));
    columns.extend((1..=SLOTS).map(|i| col(&format!("staop{i}"), PgType::Oid)));
    columns.extend((1..=SLOTS).map(|i| col(&format!("stacoll{i}"), PgType::Oid)));
    columns.extend((1..=SLOTS).map(|i| col(&format!("stanumbers{i}"), PgType::Text)));
    columns.extend((1..=SLOTS).map(|i| col(&format!("stavalues{i}"), PgType::Text)));
    TableSchema::in_namespace("pg_statistic", "pg_catalog", columns)
}

pub(crate) fn pg_statistic_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let fmt = fmt_ctx(cat);
    let mut rows = Vec::new();
    for (oid, schema, attnum, column, stats) in analyzed_columns(cat) {
        let _ = schema;
        let slots = slots_of(column.ty, stats, &fmt);
        let mut row = vec![
            Value::Oid(oid),
            Value::Int2(attnum),
            // No inheritance-wide statistics: `ANALYZE` measures one relation's
            // own rows, which is PostgreSQL's `stainherit = false` row.
            Value::Bool(false),
            Value::Float4(stats.null_frac),
            Value::Int4(stats.avg_width),
            Value::Float4(stats.n_distinct),
        ];
        row.extend(
            slots
                .iter()
                .map(|s| Value::Int2(s.as_ref().map_or(0, |s| s.kind))),
        );
        // staop/stacoll: the operator and collation each slot's values were
        // ordered under. Zero throughout — this system has no `pg_operator` rows
        // to name, and a reader that needs the ordering has the column's type.
        row.extend(std::iter::repeat_n(Value::Oid(0), SLOTS * 2));
        row.extend(
            slots
                .iter()
                .map(|s| optional_text(s.as_ref().and_then(|s| s.numbers.clone()))),
        );
        row.extend(
            slots
                .iter()
                .map(|s| optional_text(s.as_ref().and_then(|s| s.values.clone()))),
        );
        rows.push(row);
    }
    rows
}

pub(crate) fn pg_stats_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_stats",
        "pg_catalog",
        vec![
            col("schemaname", PgType::Name),
            col("tablename", PgType::Name),
            col("attname", PgType::Name),
            col("inherited", PgType::Bool),
            col("null_frac", PgType::Float4),
            col("avg_width", PgType::Int4),
            col("n_distinct", PgType::Float4),
            // anyarray in PostgreSQL; see the module docs.
            col("most_common_vals", PgType::Text),
            col("most_common_freqs", PgType::Text),
            col("histogram_bounds", PgType::Text),
            col("correlation", PgType::Float4),
            // The element-statistics and range-statistics columns PostgreSQL
            // also publishes. Always NULL: `ANALYZE` collects neither kind, and
            // NULL is exactly what PostgreSQL shows for a column that has none.
            col("most_common_elems", PgType::Text),
            col("most_common_elem_freqs", PgType::Text),
            col("elem_count_histogram", PgType::Text),
            col("range_length_histogram", PgType::Text),
            col("range_empty_frac", PgType::Float4),
            col("range_bounds_histogram", PgType::Text),
        ],
    )
}

pub(crate) fn pg_stats_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let fmt = fmt_ctx(cat);
    let mut rows = Vec::new();
    for (_, schema, attnum, column, stats) in analyzed_columns(cat) {
        let _ = attnum;
        rows.push(vec![
            Value::Text(schema.namespace.clone()),
            Value::Text(schema.name.clone()),
            Value::Text(column.name.clone()),
            Value::Bool(false),
            Value::Float4(stats.null_frac),
            Value::Int4(stats.avg_width),
            Value::Float4(stats.n_distinct),
            optional_text(mcv_values(column.ty, stats, &fmt)),
            optional_text(mcv_freqs(stats)),
            optional_text(histogram_bounds(column.ty, stats, &fmt)),
            Value::Float4(stats.correlation),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]);
    }
    rows
}

/// One `pg_statistic` slot: its kind and the two arrays, already rendered.
#[derive(Clone)]
struct Slot {
    kind: i16,
    numbers: Option<String>,
    values: Option<String>,
}

/// The slots a column's statistics fill, in the order `ANALYZE` assigns them —
/// which is the order PostgreSQL assigns them too, so slot 1 is the MCV list
/// wherever there is one.
fn slots_of(ty: PgType, stats: &ColStats, fmt: &FmtCtx) -> [Option<Slot>; SLOTS] {
    let mut slots: [Option<Slot>; SLOTS] = Default::default();
    let mut next = 0;
    let mut push = |slot: Slot| {
        if next < SLOTS {
            slots[next] = Some(slot);
            next += 1;
        }
    };
    if let Some(values) = mcv_values(ty, stats, fmt) {
        push(Slot {
            kind: KIND_MCV,
            numbers: mcv_freqs(stats),
            values: Some(values),
        });
    }
    if let Some(bounds) = histogram_bounds(ty, stats, fmt) {
        push(Slot {
            kind: KIND_HISTOGRAM,
            numbers: None,
            values: Some(bounds),
        });
    }
    push(Slot {
        kind: KIND_CORRELATION,
        numbers: Some(format!("{{{}}}", render_real(stats.correlation))),
        values: None,
    });
    slots
}

fn mcv_values(ty: PgType, stats: &ColStats, fmt: &FmtCtx) -> Option<String> {
    let values: Vec<Value> = stats.mcv.iter().map(|(v, _)| v.clone()).collect();
    (!values.is_empty()).then(|| crabgresql_types::array::format(ty, &values, fmt))
}

fn mcv_freqs(stats: &ColStats) -> Option<String> {
    (!stats.mcv.is_empty()).then(|| {
        let freqs: Vec<String> = stats.mcv.iter().map(|(_, f)| render_real(*f)).collect();
        format!("{{{}}}", freqs.join(","))
    })
}

fn histogram_bounds(ty: PgType, stats: &ColStats, fmt: &FmtCtx) -> Option<String> {
    (!stats.histogram.is_empty())
        .then(|| crabgresql_types::array::format(ty, &stats.histogram, fmt))
}

/// A `real` as PostgreSQL prints it inside one of these arrays: through
/// `float4out`, which is shortest-round-trip.
fn render_real(v: f32) -> String {
    crabgresql_types::float::fmt_f32(v, 0)
}

fn optional_text(s: Option<String>) -> Value {
    s.map_or(Value::Null, Value::Text)
}

/// Every `(relation oid, schema, attnum, column, statistics)` an `ANALYZE` has
/// measured, in `pg_class` OID order.
///
/// A relation with no column statistics yields nothing — it was never analyzed,
/// or its access method reports none — which is how both catalogs spell "not
/// measured".
fn analyzed_columns(
    cat: &SystemCatalog,
) -> impl Iterator<
    Item = (
        u32,
        &TableSchema,
        i16,
        &crabgresql_storage_api::Column,
        &ColStats,
    ),
> {
    cat.relation_oids()
        .iter()
        .zip(cat.relation_stats())
        .flat_map(|((oid, schema), stats)| {
            schema
                .columns
                .iter()
                .enumerate()
                .zip(stats.columns.iter())
                .map(move |((i, column), col_stats)| {
                    (*oid, schema, (i + 1) as i16, column, col_stats)
                })
        })
}

/// The formatting context array rendering needs. Only `bytea_output` can differ
/// per session here, and a `bytea` column's statistics are the one place it
/// shows.
fn fmt_ctx(cat: &SystemCatalog) -> FmtCtx {
    FmtCtx::utc_default().with_bytea_output(cat.bytea_output())
}
