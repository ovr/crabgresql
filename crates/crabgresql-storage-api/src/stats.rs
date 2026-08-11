//! Planner-facing relation statistics — the data `ANALYZE` collects and
//! `pg_class.relpages`/`reltuples` render.
//!
//! Statistics are **estimates, deliberately**. PostgreSQL's contract for these
//! values is best-effort: they may be stale, they are not transactional, and a
//! crash may lose them and fall back to a derived guess. Nothing may depend on
//! them for correctness — they exist to let the planner pick between plans that
//! are all correct.
//!
//! A relation that has never been analyzed still reports something usable:
//! [`RelStats::from_pages`] derives a row count from the relation's physical
//! size and its schema's average row width, so the planner is never reasoning
//! about a table it believes is empty. [`RelStats::unknown`] is the last resort
//! for engines that cannot report even a page count.

use std::sync::Arc;

use crabgresql_types::Value;

use crate::TableSchema;

/// Bytes of a heap page available to tuples, after the page header and the
/// per-tuple line pointer and header overhead. Used only to turn a page count
/// into a row estimate, so an approximation is the point rather than a
/// shortcoming.
const USABLE_BYTES_PER_PAGE: f64 = 8000.0;

/// Assumed width of a varlena column when estimating a row width without
/// having looked at any data. Deliberately pessimistic-but-small: overestimating
/// makes the planner think tables have fewer rows than they do.
const ASSUMED_VARLENA_WIDTH: f64 = 32.0;

/// Size and distribution estimates for one relation.
#[derive(Clone, Debug, PartialEq)]
pub struct RelStats {
    /// Physical size in 8 KB pages — PostgreSQL's `pg_class.relpages`.
    pub relpages: u32,
    /// Estimated live row count — PostgreSQL's `pg_class.reltuples`.
    pub reltuples: f64,
    /// `false` when these numbers were derived from the relation's physical
    /// size, `true` when `ANALYZE` measured them. The planner may weigh a
    /// derived estimate less confidently; `pg_stats` shows nothing for a
    /// relation that was never analyzed.
    pub analyzed: bool,
    /// The relation's size in pages **now**, where the engine can read it
    /// cheaply — as distinct from [`Self::relpages`], which is frozen at the
    /// last `ANALYZE`.
    ///
    /// The two exist separately because their consumers want opposite things.
    /// `pg_class.relpages`/`reltuples` must report the measurement verbatim, so
    /// that a client checking `reltuples = -1` can tell a table that needs
    /// analyzing from one that does not. A plan estimate must not: a relation
    /// analyzed at a hundred rows and since grown to a million would otherwise
    /// be planned as a hundred rows forever, since nothing re-analyzes it on its
    /// own. PostgreSQL resolves it the same way — `estimate_rel_size` scales the
    /// measured tuple density by the *current* block count.
    ///
    /// `None` means the engine cannot answer cheaply; the planner then falls
    /// back to [`Self::relpages`].
    pub curpages: Option<u32>,
    /// Per-column distribution statistics, one entry per schema column in
    /// `attnum` order, or empty when no column statistics were collected.
    /// A non-empty slice always has exactly `schema().columns.len()` entries,
    /// so callers may index it by column position.
    ///
    /// Shared rather than owned because `TableAm::statistics` returns by value
    /// and both of its callers copy it per statement: the planner once per
    /// planned relation, and the `pg_catalog` snapshot once per relation per
    /// access. Deep-copying every MCV list and histogram there was measurable on
    /// a wide analyzed table and bought nothing — nobody mutates these.
    pub columns: Arc<[ColStats]>,
}

impl RelStats {
    /// The "nothing is known" fallback: an empty relation with no column
    /// statistics. Engines that cannot report a physical size inherit this.
    pub fn unknown(_schema: &TableSchema) -> Self {
        RelStats {
            relpages: 0,
            reltuples: 0.0,
            analyzed: false,
            curpages: None,
            columns: no_columns(),
        }
    }

    /// Derive an estimate from the relation's physical size, as PostgreSQL does
    /// for a relation whose `relpages` is zero: assume the pages are full and
    /// divide by the schema's average row width.
    pub fn from_pages(relpages: u32, schema: &TableSchema) -> Self {
        let width = estimated_row_width(schema);
        let rows_per_page = (USABLE_BYTES_PER_PAGE / width).floor().max(1.0);
        RelStats {
            relpages,
            reltuples: f64::from(relpages) * rows_per_page,
            analyzed: false,
            // Derived from the size the relation has right now, so the two
            // page counts are the same number by construction.
            curpages: Some(relpages),
            columns: no_columns(),
        }
    }

    /// An exact count, for engines that hold their rows in memory and can count
    /// them for free. Such a count is never an estimate, so it is reported as
    /// analyzed.
    pub fn exact(rows: usize, schema: &TableSchema) -> Self {
        let width = estimated_row_width(schema);
        let rows_per_page = (USABLE_BYTES_PER_PAGE / width).floor().max(1.0);
        let rows = rows as f64;
        let relpages = (rows / rows_per_page).ceil() as u32;
        RelStats {
            relpages,
            reltuples: rows,
            analyzed: true,
            curpages: Some(relpages),
            columns: no_columns(),
        }
    }
}

/// The empty column-statistics slice. A fresh `Arc<[ColStats]>` per call, which
/// allocates nothing for a zero-length slice.
fn no_columns() -> Arc<[ColStats]> {
    Arc::from([])
}

/// Distribution statistics for one column, mirroring the columns of
/// PostgreSQL's `pg_stats` view.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ColStats {
    /// Fraction of rows in which the column is NULL.
    pub null_frac: f32,
    /// Average stored width in bytes.
    pub avg_width: i32,
    /// Number of distinct non-null values, in PostgreSQL's `pg_stats` encoding:
    /// a positive number is an absolute count, a negative number is the negated
    /// fraction of `reltuples` (so `-1` means every row is distinct), and zero
    /// means unknown.
    pub n_distinct: f32,
    /// The most common values and the fraction of rows holding each, most
    /// frequent first — `pg_stats.most_common_vals`/`most_common_freqs`.
    pub mcv: Vec<(Value, f32)>,
    /// Equi-depth histogram bounds over the values *not* in [`Self::mcv`],
    /// ascending — `pg_stats.histogram_bounds`.
    pub histogram: Vec<Value>,
    /// How strongly physical row order correlates with the column's sort order,
    /// in `[-1, 1]` — `pg_stats.correlation`.
    pub correlation: f32,
}

/// Average bytes per row for a schema, before any data has been seen: the sum
/// of the columns' fixed widths, with varlena columns charged a flat assumed
/// width. Never zero, so it is always safe to divide by.
fn estimated_row_width(schema: &TableSchema) -> f64 {
    let width: f64 = schema
        .columns
        .iter()
        .map(|column| match column.ty.typlen() {
            len if len > 0 => f64::from(len),
            _ => ASSUMED_VARLENA_WIDTH,
        })
        .sum();
    width.max(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Column;
    use crabgresql_types::PgType;

    fn schema(types: &[PgType]) -> TableSchema {
        TableSchema::new(
            "t",
            types
                .iter()
                .enumerate()
                .map(|(i, ty)| Column::new(format!("c{i}"), *ty))
                .collect(),
        )
    }

    #[test]
    fn unknown_reports_an_empty_relation() {
        let stats = RelStats::unknown(&schema(&[PgType::Int4]));
        assert_eq!(stats.relpages, 0);
        assert_eq!(stats.reltuples, 0.0);
        assert!(!stats.analyzed);
        assert!(stats.columns.is_empty());
    }

    #[test]
    fn row_estimate_scales_with_pages_and_shrinks_with_width() {
        // 8000 usable bytes / 4-byte rows = 2000 rows per page.
        let narrow = RelStats::from_pages(3, &schema(&[PgType::Int4]));
        assert_eq!(narrow.relpages, 3);
        assert_eq!(narrow.reltuples, 6000.0);
        assert!(!narrow.analyzed, "a derived estimate is not analyzed");

        // Same page count, wider rows: strictly fewer rows.
        let wide = RelStats::from_pages(3, &schema(&[PgType::Int4; 8]));
        assert!(
            wide.reltuples < narrow.reltuples,
            "wider rows must estimate fewer rows per page: {wide:?}"
        );
    }

    #[test]
    fn a_zero_page_relation_estimates_no_rows() {
        assert_eq!(
            RelStats::from_pages(0, &schema(&[PgType::Int4])).reltuples,
            0.0
        );
    }

    #[test]
    fn a_varlena_only_schema_still_divides() {
        // typlen is -1 for text; the assumed width keeps the divisor positive.
        let stats = RelStats::from_pages(1, &schema(&[PgType::Text]));
        assert!(stats.reltuples > 0.0, "{stats:?}");
    }

    #[test]
    fn an_exact_count_is_reported_as_analyzed() {
        let stats = RelStats::exact(7, &schema(&[PgType::Int4]));
        assert_eq!(stats.reltuples, 7.0);
        assert!(stats.analyzed);
        // Seven 4-byte rows fit in one page.
        assert_eq!(stats.relpages, 1);
    }

    #[test]
    fn an_exact_count_of_zero_occupies_no_pages() {
        assert_eq!(RelStats::exact(0, &schema(&[PgType::Int4])).relpages, 0);
    }
}
