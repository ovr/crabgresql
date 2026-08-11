//! `ANALYZE`: measure a relation and hand back the statistics the planner and
//! `pg_class` report.
//!
//! # Sampling
//!
//! PostgreSQL samples: it reads `300 × default_statistics_target` rows drawn
//! from randomly chosen blocks and extrapolates. This implementation **reads
//! every row instead**, and keeps a bounded, deterministic *decimation* of them
//! ([`Decimator`]) to compute the per-column distributions from. The trade is
//! deliberate:
//!
//! - the result is exact where it can be, so `reltuples` is a count rather than
//!   an estimate, and so is `n_distinct` whenever nothing was decimated away;
//! - it is deterministic, which is what makes the statistics testable at all —
//!   a randomly sampled `n_distinct` cannot be asserted, this one can.
//!
//! TODO(perf): read only the blocks the sample is drawn from, instead of every
//! block — the decimation bounds memory but not time, so `ANALYZE` on a large
//! relation is O(rows) rather than O(sample). The two-stage sampler is a change
//! to [`analyze_heap`]'s body alone: build it on [`crate::heap::HeapTable`]'s
//! block-at-a-time scan (which already captures the block count up front and
//! holds the relation against a concurrent TRUNCATE), not on `TableAm::scan`,
//! which yields tuples and so loses the block boundary the extrapolation needs.
//!
//! # What is measured per column
//!
//! [`ColStats`] mirrors `pg_statistic`: null fraction, average width, distinct
//! count (in `pg_stats`' signed encoding), most-common values with their
//! frequencies, an equi-depth histogram of the rest, and the correlation between
//! physical row order and value order. Only types with a default btree operator
//! class get the distribution half — the rest are the types
//! [`compare_values_collated`] has no ordering for, and PostgreSQL's
//! `compute_minimal_stats` path (equality but no ordering) is not reproduced
//! here: the one such type in this system is `xid`, which no plan estimates
//! against. Such a column still reports `null_frac` and `avg_width`.

use std::cmp::Ordering;

use crabgresql_storage_api::{ColStats, RelStats, TableAm, TableSchema, Tuple};
use crabgresql_txn::TxnContext;
use crabgresql_types::compare::compare_values_collated;
use crabgresql_types::datum::encode_datum;
use crabgresql_types::{PgType, Value, collation::DEFAULT_COLLATION_OID};

use crate::heap::HeapTable;

/// PostgreSQL's `default_statistics_target`: the histogram/MCV size, and the
/// multiplier on the sample row count.
pub const DEFAULT_STATISTICS_TARGET: usize = 100;

/// PostgreSQL's `WIDTH_THRESHOLD`. A value wider than this is counted toward
/// `null_frac`'s denominator and `avg_width`, and assumed distinct, but never
/// stored in an MCV list or a histogram — one such value would otherwise be
/// copied into the catalog file on every `ANALYZE`.
const WIDTH_THRESHOLD: usize = 1024;

/// How large a sample `ANALYZE` should draw.
#[derive(Clone, Copy, Debug)]
pub struct SampleTarget {
    /// Rows to retain for the per-column distributions. Every row is still
    /// read; this bounds only what is held in memory (see the module docs).
    pub target_rows: usize,
}

impl Default for SampleTarget {
    /// PostgreSQL's default: `300 × default_statistics_target` rows.
    fn default() -> Self {
        SampleTarget {
            target_rows: 300 * DEFAULT_STATISTICS_TARGET,
        }
    }
}

/// Measure `table` under `txn`'s snapshot.
///
/// Counts only rows visible to `txn`, as PostgreSQL does — statistics describe
/// what a query can actually see, so in-flight and dead versions do not count.
pub fn analyze_heap(table: &HeapTable, txn: &TxnContext, target: SampleTarget) -> RelStats {
    let schema = table.schema();
    tracing::debug!(
        relation = %schema.name,
        target_rows = target.target_rows,
        "ANALYZE: reading every row, retaining a bounded decimation"
    );
    let mut sampler = Decimator::new(&schema, target.target_rows);
    // One call, so the row count, the page count and the values all describe the
    // same file even when this transaction has a staged TRUNCATE or another
    // commits one mid-measurement — see [`HeapTable::measure_visiting`].
    let (relpages, reltuples) = table.measure_visiting(txn, &mut |tuple| sampler.push(tuple));
    RelStats {
        relpages,
        reltuples,
        analyzed: true,
        // Just measured, so the live count is the measured one. It diverges only
        // later, as the relation grows away from this `ANALYZE`.
        curpages: Some(relpages),
        columns: sampler.finish(&schema, reltuples).into(),
    }
}

/// What `pg_statistic.stawidth` reports for one value: the width PostgreSQL
/// would store it at, not the width this system's datum codec spends.
///
/// The two differ, and the reported one has to be PostgreSQL's — `avg_width` is
/// a number users read and do capacity arithmetic with, and it says `4` for an
/// `int4` on every PostgreSQL there has ever been. A fixed-width type is its
/// `typlen`, exactly. A varlena is its payload plus the one-byte header
/// PostgreSQL gives a short value (four once it exceeds what that header can
/// count), where the payload is what is left after this codec's own framing —
/// a tag byte and a four-byte length.
///
/// Approximate for the few types that frame their payload differently
/// (`bit`/`varbit` carry a bit count of their own), which comes out a few bytes
/// wide. `stawidth` is an average of estimates to begin with; being wrong about
/// `int4` would not have been in the same class.
fn stored_width(ty: PgType, encoded: usize) -> usize {
    let fixed = ty.typlen();
    if fixed > 0 {
        return fixed as usize;
    }
    let payload = encoded.saturating_sub(DATUM_FRAMING);
    payload + if payload < 127 { 1 } else { 4 }
}

/// The tag byte and length prefix `crabgresql_types::datum` puts in front of a
/// variable-length payload.
const DATUM_FRAMING: usize = 5;

/// One sampled cell. `TooWide` is not `Val`: such a value takes part in
/// `avg_width` and counts as distinct, but never reaches an MCV list or a
/// histogram, so it must not be retained (see [`WIDTH_THRESHOLD`]).
enum Cell {
    Null,
    TooWide,
    Val(Value),
}

/// A bounded, deterministic decimation of a scan: keeps every `stride`-th row
/// and doubles `stride` (dropping every second retained row) whenever the buffer
/// would exceed the target, so the retained set stays between half the target
/// and the target — the same ceiling PostgreSQL's sample has.
///
/// Deterministic on purpose — the same table always yields the same statistics,
/// so a test can assert an MCV list rather than a tolerance. The doubling is
/// what makes it work without knowing the row count up front: at every moment
/// the retained rows are an evenly spaced sample of everything seen so far.
///
/// The ceiling is in *rows*, so a relation of very wide columns still costs
/// `target × columns × `[`WIDTH_THRESHOLD`] at the peak. That exposure is
/// PostgreSQL's too — its sample holds whole tuples — and narrowing it further
/// would mean sampling fewer rows than the statistics target asks for.
struct Decimator {
    /// Per column, the retained cells in physical row order. Indexed by schema
    /// position, so a column is analyzed without transposing the sample.
    columns: Vec<Vec<Cell>>,
    /// Width sums and counts feeding `avg_width`, over *every* non-null value
    /// seen, not just the retained ones.
    widths: Vec<(u64, u64)>,
    /// Each column's type, for [`stored_width`]. Held rather than looked up per
    /// row: `push` runs on the scan's hot path.
    types: Vec<PgType>,
    /// Nulls seen per column, over every row (not just retained ones), so
    /// `null_frac` is exact.
    nulls: Vec<u64>,
    rows_seen: u64,
    rows_kept: usize,
    target: usize,
    stride: u64,
}

impl Decimator {
    fn new(schema: &TableSchema, target_rows: usize) -> Self {
        let ncols = schema.columns.len();
        Decimator {
            columns: (0..ncols).map(|_| Vec::new()).collect(),
            widths: vec![(0, 0); ncols],
            types: schema.columns.iter().map(|c| c.ty).collect(),
            nulls: vec![0; ncols],
            rows_seen: 0,
            rows_kept: 0,
            target: target_rows.max(1),
            stride: 1,
        }
    }

    fn push(&mut self, tuple: &Tuple) {
        // Every row is counted for `null_frac` and `avg_width`; only every
        // `stride`-th is retained for the distributions.
        let retain = self.rows_seen.is_multiple_of(self.stride);
        self.rows_seen += 1;
        let mut scratch = Vec::new();
        for (column, cell) in self.columns.iter_mut().enumerate() {
            let value = tuple.get(column).unwrap_or(&Value::Null);
            if matches!(value, Value::Null) {
                self.nulls[column] += 1;
                if retain {
                    cell.push(Cell::Null);
                }
                continue;
            }
            scratch.clear();
            encode_datum(value, &mut scratch);
            let width = scratch.len();
            self.widths[column].0 += stored_width(self.types[column], width) as u64;
            self.widths[column].1 += 1;
            if retain {
                cell.push(if width > WIDTH_THRESHOLD {
                    Cell::TooWide
                } else {
                    Cell::Val(value.clone())
                });
            }
        }
        if retain {
            self.rows_kept += 1;
            if self.rows_kept > self.target {
                self.halve();
            }
        }
    }

    /// Drop every second retained row and double the stride, so the retained set
    /// stays an evenly spaced sample of everything seen so far.
    fn halve(&mut self) {
        for cell in &mut self.columns {
            let mut keep = false;
            cell.retain(|_| {
                keep = !keep;
                keep
            });
        }
        self.stride *= 2;
        self.rows_kept = self.rows_kept.div_ceil(2);
    }

    fn finish(self, schema: &TableSchema, reltuples: f64) -> Vec<ColStats> {
        let rows_seen = self.rows_seen;
        let Decimator {
            columns,
            widths,
            nulls,
            ..
        } = self;
        columns
            .into_iter()
            .enumerate()
            .map(|(i, cells)| {
                let column = &schema.columns[i];
                let mut stats = ColStats {
                    null_frac: if rows_seen == 0 {
                        0.0
                    } else {
                        nulls[i] as f32 / rows_seen as f32
                    },
                    avg_width: match widths[i] {
                        (_, 0) => 0,
                        (sum, count) => (sum / count) as i32,
                    },
                    n_distinct: 0.0,
                    mcv: Vec::new(),
                    histogram: Vec::new(),
                    correlation: 0.0,
                };
                if column.ty.has_default_btree_opclass() {
                    scalar_stats(
                        column.ty,
                        column.collation.unwrap_or(DEFAULT_COLLATION_OID),
                        cells,
                        reltuples,
                        &mut stats,
                    );
                }
                stats
            })
            .collect()
    }
}

/// PostgreSQL's `compute_scalar_stats`: sort the retained values, then derive
/// the distinct count, the most common values, an equi-depth histogram of what
/// is left, and the order correlation.
fn scalar_stats(ty: PgType, collation: u32, cells: Vec<Cell>, reltuples: f64, out: &mut ColStats) {
    let sample_rows = cells.len();
    if sample_rows == 0 {
        return;
    }
    // Physical position is carried along so the correlation can be computed
    // after sorting. Too-wide values are excluded from every ordered statistic
    // but still counted as distinct below.
    let mut too_wide = 0usize;
    let mut values: Vec<(usize, Value)> = Vec::new();
    for (position, cell) in cells.into_iter().enumerate() {
        match cell {
            Cell::Val(v) => values.push((position, v)),
            Cell::TooWide => too_wide += 1,
            Cell::Null => {}
        }
    }
    let nonnull = values.len() + too_wide;
    if values.is_empty() {
        // Nothing orderable to describe; each too-wide value counts as distinct.
        out.n_distinct = encode_n_distinct(too_wide as f64, too_wide, nonnull, reltuples);
        return;
    }
    // Stable, so equal values keep physical order — which is what makes the
    // correlation of an already-ordered column come out at exactly 1.
    values.sort_by(|a, b| compare_values_collated(ty, &a.1, &b.1, collation));

    out.correlation = correlation(&values);

    // Runs of equal values, in ascending order.
    let mut runs: Vec<(Value, usize)> = Vec::new();
    for (_, value) in &values {
        match runs.last_mut() {
            Some((last, count))
                if compare_values_collated(ty, last, value, collation) == Ordering::Equal =>
            {
                *count += 1;
            }
            _ => runs.push((value.clone(), 1)),
        }
    }
    let ndistinct = runs.len() + too_wide;
    let singletons = runs.iter().filter(|(_, c)| *c == 1).count() + too_wide;
    out.n_distinct = encode_n_distinct(
        haas_stokes(ndistinct, singletons, nonnull, reltuples),
        ndistinct,
        nonnull,
        reltuples,
    );

    let mcv = pick_mcv(&runs, ndistinct, values.len());
    out.mcv = mcv
        .iter()
        .map(|&i| (runs[i].0.clone(), runs[i].1 as f32 / sample_rows as f32))
        .collect();
    out.histogram = histogram(ty, collation, &values, &runs, &mcv);
}

/// Pearson correlation between physical position and value order, over the
/// values sorted ascending: `x` is the position in sorted order, `y` the
/// original physical position. `1` for a column stored in ascending order,
/// `-1` for descending — which is exactly what `cost_index` interpolates on.
fn correlation(sorted: &[(usize, Value)]) -> f32 {
    let n = sorted.len() as f64;
    if n < 2.0 {
        return 0.0;
    }
    let (mut sx, mut sy, mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (rank, (position, _)) in sorted.iter().enumerate() {
        let (x, y) = (rank as f64, *position as f64);
        sx += x;
        sy += y;
        sxx += x * x;
        syy += y * y;
        sxy += x * y;
    }
    let denominator = ((n * sxx - sx * sx) * (n * syy - sy * sy)).sqrt();
    if denominator == 0.0 {
        return 0.0;
    }
    (((n * sxy - sx * sy) / denominator) as f32).clamp(-1.0, 1.0)
}

/// The Haas–Stokes distinct-value estimator PostgreSQL uses when the sample is
/// smaller than the relation. When nothing was decimated away the sample *is*
/// the relation, so the counted value is exact and is returned unchanged.
fn haas_stokes(ndistinct: usize, singletons: usize, nonnull: usize, reltuples: f64) -> f64 {
    let (d, f1, n) = (ndistinct as f64, singletons as f64, nonnull as f64);
    if n <= 0.0 || n >= reltuples {
        return d;
    }
    let denominator = (n - f1) + f1 * n / reltuples;
    if denominator <= 0.0 {
        return d;
    }
    (n * d / denominator).clamp(d, reltuples)
}

/// `pg_stats`' signed encoding: a positive number is an absolute distinct count,
/// a negative one is the negated fraction of `reltuples`. PostgreSQL switches to
/// the fraction whenever the estimate exceeds 10% of the relation, so a key
/// column of a growing table keeps reporting `-1` instead of a count that goes
/// stale on the next insert.
fn encode_n_distinct(estimate: f64, ndistinct: usize, nonnull: usize, reltuples: f64) -> f32 {
    if nonnull == 0 {
        return 0.0;
    }
    if ndistinct == nonnull && nonnull as f64 >= reltuples {
        // Every value in the (complete) sample was distinct: the column is
        // unique as far as this measurement can tell.
        return -1.0;
    }
    if reltuples > 0.0 && estimate > 0.1 * reltuples {
        return -((estimate / reltuples) as f32).min(1.0);
    }
    estimate as f32
}

/// PostgreSQL's most-common-value rule: a value qualifies when it occurs more
/// than once *and* is significantly more common than average (`1.25 ×`), unless
/// every distinct value fits in the list, in which case all of them are stored
/// and the histogram is left empty. Returns indexes into `runs`, most common
/// first.
fn pick_mcv(runs: &[(Value, usize)], ndistinct: usize, values: usize) -> Vec<usize> {
    if ndistinct == values {
        // All distinct: no value is "common", and PG stores no MCV list.
        return Vec::new();
    }
    let mut candidates: Vec<usize> = (0..runs.len()).filter(|&i| runs[i].1 > 1).collect();
    // Most common first; ties broken by value order, so the list is stable.
    candidates.sort_by(|&a, &b| runs[b].1.cmp(&runs[a].1).then(a.cmp(&b)));
    let multiple = candidates.len();
    if multiple == ndistinct && ndistinct <= DEFAULT_STATISTICS_TARGET {
        // Every distinct value repeats and they all fit: store the exact
        // distribution, as PG does.
        return candidates;
    }
    let average = values as f64 / ndistinct as f64;
    let mincount = (1.25 * average).max(2.0);
    candidates.truncate(DEFAULT_STATISTICS_TARGET);
    candidates.retain(|&i| runs[i].1 as f64 >= mincount);
    candidates
}

/// An equi-depth histogram over the values not claimed by the MCV list, with at
/// most `DEFAULT_STATISTICS_TARGET + 1` bounds. Duplicates are kept (the depth
/// is in rows, not distinct values), so a bound may repeat.
fn histogram(
    ty: PgType,
    collation: u32,
    sorted: &[(usize, Value)],
    runs: &[(Value, usize)],
    mcv: &[usize],
) -> Vec<Value> {
    if mcv.len() == runs.len() {
        // The MCV list already describes the whole column exactly.
        return Vec::new();
    }
    let excluded: Vec<&Value> = mcv.iter().map(|&i| &runs[i].0).collect();
    let remaining: Vec<&Value> = sorted
        .iter()
        .map(|(_, v)| v)
        .filter(|v| {
            !excluded
                .iter()
                .any(|e| compare_values_collated(ty, e, v, collation) == Ordering::Equal)
        })
        .collect();
    if remaining.len() < 2 {
        return Vec::new();
    }
    let bounds = (DEFAULT_STATISTICS_TARGET + 1).min(remaining.len());
    (0..bounds)
        .map(|i| remaining[i * (remaining.len() - 1) / (bounds - 1)].clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_storage_api::Column;

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

    /// Run the sampler over `rows` exactly as `measure_visiting` would.
    fn stats_of(schema: &TableSchema, rows: &[Tuple], target: usize) -> Vec<ColStats> {
        let mut sampler = Decimator::new(schema, target);
        for row in rows {
            sampler.push(row);
        }
        sampler.finish(schema, rows.len() as f64)
    }

    #[test]
    fn a_unique_ascending_column_is_all_distinct_and_perfectly_correlated() {
        let schema = schema(&[PgType::Int4]);
        let rows: Vec<Tuple> = (0..1000).map(|i| vec![Value::Int4(i)]).collect();
        let stats = stats_of(&schema, &rows, 10_000);
        assert_eq!(stats[0].null_frac, 0.0);
        // PostgreSQL's width for an int4, not this codec's encoded size.
        assert_eq!(stats[0].avg_width, 4);
        // Every value distinct: the negated-fraction encoding, -1.
        assert_eq!(stats[0].n_distinct, -1.0);
        assert!(stats[0].mcv.is_empty(), "mcv: {:?}", stats[0].mcv);
        assert_eq!(stats[0].correlation, 1.0);
        assert_eq!(stats[0].histogram.len(), DEFAULT_STATISTICS_TARGET + 1);
        assert_eq!(stats[0].histogram[0], Value::Int4(0));
        assert_eq!(
            stats[0].histogram[DEFAULT_STATISTICS_TARGET],
            Value::Int4(999)
        );
        // Bounds ascend.
        assert!(
            stats[0]
                .histogram
                .windows(2)
                .all(|w| matches!((&w[0], &w[1]), (Value::Int4(a), Value::Int4(b)) if a < b))
        );
    }

    #[test]
    fn a_descending_column_correlates_negatively() {
        let schema = schema(&[PgType::Int4]);
        let rows: Vec<Tuple> = (0..500).rev().map(|i| vec![Value::Int4(i)]).collect();
        let stats = stats_of(&schema, &rows, 10_000);
        assert_eq!(stats[0].correlation, -1.0);
    }

    #[test]
    fn a_skewed_column_reports_its_common_values_with_frequencies() {
        let schema = schema(&[PgType::Text]);
        // 600 "a", 300 "b", and 100 distinct one-offs.
        let mut rows: Vec<Tuple> = Vec::new();
        rows.extend((0..600).map(|_| vec![Value::Text("a".into())]));
        rows.extend((0..300).map(|_| vec![Value::Text("b".into())]));
        rows.extend((0..100).map(|i| vec![Value::Text(format!("x{i}"))]));
        let stats = stats_of(&schema, &rows, 10_000);
        let mcv: Vec<(&str, f32)> = stats[0]
            .mcv
            .iter()
            .map(|(v, f)| match v {
                Value::Text(s) => (s.as_str(), *f),
                other => panic!("expected text, got {other:?}"),
            })
            .collect();
        assert_eq!(mcv, vec![("a", 0.6), ("b", 0.3)]);
        // 102 distinct values in 1000 rows is over PG's 10% switchover, so the
        // count is reported as the negated fraction rather than absolutely.
        assert_eq!(stats[0].n_distinct, -0.102);
        // The histogram describes only the 100 one-offs left over.
        assert!(
            stats[0]
                .histogram
                .iter()
                .all(|v| matches!(v, Value::Text(s) if s.starts_with('x'))),
            "histogram: {:?}",
            stats[0].histogram
        );
    }

    #[test]
    fn nulls_are_counted_but_never_described() {
        let schema = schema(&[PgType::Int4]);
        let rows: Vec<Tuple> = (0..100)
            .map(|i| {
                vec![if i % 4 == 0 {
                    Value::Null
                } else {
                    Value::Int4(i)
                }]
            })
            .collect();
        let stats = stats_of(&schema, &rows, 10_000);
        assert_eq!(stats[0].null_frac, 0.25);
        assert!(
            !stats[0].histogram.contains(&Value::Null),
            "a NULL reached the histogram"
        );
    }

    #[test]
    fn every_distinct_value_repeating_is_stored_exactly_with_no_histogram() {
        let schema = schema(&[PgType::Bool]);
        let mut rows: Vec<Tuple> = Vec::new();
        rows.extend((0..70).map(|_| vec![Value::Bool(true)]));
        rows.extend((0..30).map(|_| vec![Value::Bool(false)]));
        let stats = stats_of(&schema, &rows, 10_000);
        assert_eq!(
            stats[0].mcv,
            vec![(Value::Bool(true), 0.7), (Value::Bool(false), 0.3)]
        );
        assert!(stats[0].histogram.is_empty());
        assert_eq!(stats[0].n_distinct, 2.0);
    }

    /// The retained sample never exceeds the target — checked at every step, not
    /// just at the end, because the peak is what costs memory and it is reached
    /// mid-scan.
    #[test]
    fn the_retained_sample_never_exceeds_the_target() {
        let schema = schema(&[PgType::Int4]);
        let target = 64;
        let mut sampler = Decimator::new(&schema, target);
        for i in 0..10_000 {
            sampler.push(&vec![Value::Int4(i)]);
            assert!(
                sampler.columns[0].len() <= target,
                "retained {} rows at step {i}, target {target}",
                sampler.columns[0].len()
            );
        }
    }

    /// Decimation keeps the sample bounded and evenly spread: the shape of the
    /// distribution survives even though most rows were dropped.
    #[test]
    fn decimation_bounds_the_sample_and_preserves_the_distribution() {
        let schema = schema(&[PgType::Int4]);
        let rows: Vec<Tuple> = (0..10_000).map(|i| vec![Value::Int4(i)]).collect();
        let stats = stats_of(&schema, &rows, 100);
        // Still recognizably "one distinct value per row", and still ascending.
        assert_eq!(stats[0].n_distinct, -1.0);
        assert_eq!(stats[0].correlation, 1.0);
        // The histogram spans the whole relation, not just its first rows.
        let last = stats[0].histogram.last().expect("bounds exist");
        assert!(
            matches!(last, Value::Int4(v) if *v > 9_000),
            "last bound: {last:?}"
        );
        // null_frac and avg_width are exact — they are counted over every row,
        // not over the retained ones.
        assert_eq!(stats[0].null_frac, 0.0);
        assert_eq!(stats[0].avg_width, 4);
    }

    #[test]
    fn a_type_without_a_btree_opclass_reports_only_width_and_nulls() {
        let schema = schema(&[PgType::Json]);
        let rows: Vec<Tuple> = (0..10)
            .map(|i| vec![Value::Json(format!("{{\"a\":{i}}}"))])
            .collect();
        let stats = stats_of(&schema, &rows, 10_000);
        assert!(stats[0].avg_width > 0);
        assert_eq!(stats[0].n_distinct, 0.0);
        assert!(stats[0].mcv.is_empty());
        assert!(stats[0].histogram.is_empty());
    }
}
