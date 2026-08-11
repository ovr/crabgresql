//! Cost estimation: how many rows a qualification keeps, and what each access
//! path would cost to run.
//!
//! The shape and the constants are PostgreSQL's (`selfuncs.c`, `costsize.c`),
//! because the point is to reach the same *decision* it reaches — a cost model
//! of one's own invention would plan differently for reasons no `EXPLAIN` from
//! upstream could explain. The numbers are arbitrary units anchored to
//! `seq_page_cost = 1`; only their ratios matter.
//!
//! What is deliberately absent: startup versus total cost (this system's
//! `EXPLAIN` prints no costs and no plan here is ever cut short, so a single
//! total decides everything), parameterized paths, and multi-clause correlation.
//!
//! [`range_selectivity`] has no caller in the planner yet — index scans are
//! equality probes today. It is written and tested alongside its equality twin
//! because the histogram it reads is what `ANALYZE` collects for it, and
//! because the two must agree about how MCVs, NULLs and the histogram divide up
//! a column's mass; splitting them apart in time is how they drift.

use std::cmp::Ordering;

use crabgresql_storage_api::{ColStats, RelStats, TableSchema};
use crabgresql_types::compare::compare_values_collated;
use crabgresql_types::{PgType, Value};

/// The two page costs, as the session has them set.
///
/// Settable because the right ratio is a property of the storage, not of the
/// query: PostgreSQL's 4:1 default describes a spinning disk, and a relation
/// served from a warm buffer pool is nearer 1:1 — measured here, a mid-
/// selectivity scan the 4:1 ratio talks the planner out of runs 1.7x faster
/// through the index. PG exposes exactly these as `seq_page_cost` and
/// `random_page_cost`, so the tuning knob a user already knows is the one that
/// works.
///
/// TODO: expose `cpu_tuple_cost`, `cpu_index_tuple_cost` and
/// `cpu_operator_cost` too — PostgreSQL has all five, and they are still
/// constants below.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CostSettings {
    pub seq_page_cost: f64,
    pub random_page_cost: f64,
}

impl Default for CostSettings {
    /// PostgreSQL's defaults.
    fn default() -> Self {
        CostSettings {
            seq_page_cost: 1.0,
            random_page_cost: 4.0,
        }
    }
}

/// Cost of processing one heap tuple.
const CPU_TUPLE_COST: f64 = 0.01;
/// Cost of processing one index entry.
const CPU_INDEX_TUPLE_COST: f64 = 0.005;
/// Cost of evaluating one simple operator or function.
const CPU_OPERATOR_COST: f64 = 0.0025;

/// What PostgreSQL assumes a relation of unknown size holds: `estimate_rel_size`
/// substitutes 10 pages for a relation whose `relpages` is zero, rather than
/// concluding that scanning it is free. Without this a never-measured relation
/// would make every sequential scan cost 0 and no index could ever win.
const DEFAULT_RELPAGES: u32 = 10;

/// PostgreSQL's `DEFAULT_EQ_SEL`: the fraction `col = ?` is assumed to keep when
/// nothing is known about the column.
const DEFAULT_EQ_SEL: f64 = 0.005;
/// PostgreSQL's `DEFAULT_INEQ_SEL`: the fraction `col < ?` is assumed to keep
/// when nothing is known — deliberately large, so an unmeasured column does not
/// get an index scan on the strength of a guess.
const DEFAULT_INEQ_SEL: f64 = 0.3333333333333333;
/// PostgreSQL's `DEFAULT_RANGE_INEQ_SEL`: the floor for a two-sided range whose
/// bounds are estimated independently, which can otherwise cancel to zero.
const DEFAULT_RANGE_INEQ_SEL: f64 = 0.005;

/// A relation's size as the cost model sees it.
///
/// Zero pages is a legitimate answer, but only for a relation that was
/// *measured* empty: an unmeasured one is floored at [`DEFAULT_RELPAGES`], so a
/// missing statistic can never make a sequential scan look free.
#[derive(Clone, Copy, Debug)]
pub struct RelSize {
    pub pages: f64,
    pub rows: f64,
}

/// PostgreSQL's `estimate_rel_size`: take the *tuple density* from what was
/// measured and apply it to the relation's size **now**.
///
/// The two are not the same number. `ANALYZE` freezes `relpages`/`reltuples` at
/// the moment it ran, and that pair is what `pg_class` must keep reporting; a
/// plan that used it directly would describe a relation as it was, not as it is.
/// Nothing re-analyzes a relation here on its own, so a table analyzed while
/// small and since grown would otherwise be planned as small forever — and would
/// refuse its own primary key. Scaling the measured density by
/// [`RelStats::curpages`] is how PostgreSQL avoids that, and it degrades
/// gracefully: an engine that reports no live size keeps exactly the measured
/// estimate.
///
/// The 10-page substitution is PG's too, and applies only to a relation that was
/// never measured. Without it a never-measured relation would cost nothing to
/// scan sequentially and no index could ever win.
pub fn estimate_rel_size(stats: &RelStats, schema: &TableSchema) -> RelSize {
    let measured = f64::from(stats.relpages);
    let mut curpages = f64::from(stats.curpages.unwrap_or(stats.relpages));
    if stats.relpages == 0 && curpages < f64::from(DEFAULT_RELPAGES) {
        curpages = f64::from(DEFAULT_RELPAGES);
    }
    // Rows per page: from the measurement where there is one, else from the
    // schema's assumed row width (which is all `from_pages` has to go on). A
    // measured *zero* is a real answer — a relation whose rows were all deleted
    // is empty however many pages it still occupies — so the fallback keys on
    // the page count alone, as PG's does.
    let density = if measured > 0.0 {
        stats.reltuples.max(0.0) / measured
    } else {
        RelStats::from_pages(1, schema).reltuples
    };
    RelSize {
        pages: curpages,
        rows: (density * curpages).max(0.0),
    }
}

/// The cost of reading the whole relation and applying `nquals` qualifications
/// to every row.
pub fn seq_scan_cost(costs: CostSettings, size: RelSize, nquals: usize) -> f64 {
    size.pages * costs.seq_page_cost + size.rows * (CPU_TUPLE_COST + qual_cost(nquals))
}

/// The cost of visiting `selectivity` of the relation through an index:
/// PostgreSQL's `genericcostestimate` for the index descent plus `cost_index`'s
/// heap fetch, whose page count interpolates between the fully correlated and
/// the fully random case by the square of the column's `correlation`.
///
/// `index_pages` is the index's own size; `nquals` counts the qualifications
/// re-checked per fetched row.
pub fn index_scan_cost(
    costs: CostSettings,
    size: RelSize,
    index_pages: f64,
    selectivity: f64,
    correlation: f32,
    nquals: usize,
) -> f64 {
    let selectivity = selectivity.clamp(0.0, 1.0);
    let tuples_fetched = (selectivity * size.rows).clamp(1.0, size.rows.max(1.0));

    // Descent: the fraction of the index the scan reads, never less than the one
    // page every probe touches.
    let index_pages_read = (selectivity * index_pages).max(1.0);
    let index_cost = index_pages_read * costs.random_page_cost
        + tuples_fetched * (CPU_INDEX_TUPLE_COST + qual_cost(nquals));

    // Heap: the same rows may share pages, so the page count is sublinear in the
    // row count (Mackert–Lohman, with this scan run once).
    let random_pages = mackert_lohman(tuples_fetched, size.pages);
    let max_io = random_pages * costs.random_page_cost;
    // Perfectly correlated: the fetches walk the relation in physical order, so
    // only the first page is a random one.
    let ordered_pages = (selectivity * size.pages).ceil();
    let min_io = if ordered_pages > 0.0 {
        costs.random_page_cost + (ordered_pages - 1.0) * costs.seq_page_cost
    } else {
        0.0
    };
    let csquared = f64::from(correlation) * f64::from(correlation);
    let heap_io = max_io + csquared * (min_io - max_io);

    index_cost + heap_io + tuples_fetched * (CPU_TUPLE_COST + qual_cost(nquals))
}

fn qual_cost(nquals: usize) -> f64 {
    nquals as f64 * CPU_OPERATOR_COST
}

/// PostgreSQL's `index_pages_fetched` for a scan run once: fetching `tuples`
/// rows from a `pages`-page relation touches fewer than `tuples` pages once the
/// relation is small enough for them to collide.
fn mackert_lohman(tuples: f64, pages: f64) -> f64 {
    if pages <= 1.0 {
        return pages.max(1.0);
    }
    let fetched = (2.0 * pages * tuples) / (2.0 * pages + tuples);
    if fetched >= pages {
        pages
    } else {
        fetched.ceil()
    }
}

/// The fraction of rows `column = value` keeps.
///
/// `value` is `None` when the comparand is not known at plan time (a parameter,
/// or an expression the planner will not fold): PostgreSQL then spreads the
/// non-null rows evenly over the distinct values instead of consulting the MCV
/// list, since it cannot tell which value it would land on.
pub fn eq_selectivity(
    stats: Option<&ColStats>,
    ty: PgType,
    collation: u32,
    value: Option<&Value>,
    rows: f64,
) -> f64 {
    let Some(stats) = stats else {
        return DEFAULT_EQ_SEL;
    };
    let ndistinct = decode_n_distinct(stats.n_distinct, rows);
    let spread = || match ndistinct {
        Some(nd) if nd >= 1.0 => (1.0 - f64::from(stats.null_frac)) / nd,
        _ => DEFAULT_EQ_SEL,
    };
    let Some(value) = value else {
        return spread();
    };
    if !describes(stats, value) {
        return spread();
    }
    if let Some((_, freq)) = stats
        .mcv
        .iter()
        .find(|(mcv, _)| compare_values_collated(ty, mcv, value, collation) == Ordering::Equal)
    {
        return f64::from(*freq);
    }
    let mcv_mass: f64 = stats.mcv.iter().map(|(_, f)| f64::from(*f)).sum();
    let remaining = (1.0 - mcv_mass - f64::from(stats.null_frac)).max(0.0);
    let Some(ndistinct) = ndistinct else {
        return DEFAULT_EQ_SEL;
    };
    let other_distinct = ndistinct - stats.mcv.len() as f64;
    if other_distinct < 1.0 {
        // The MCV list accounts for every distinct value, and this value is not
        // in it: the column cannot hold it. Zero rows, not "a few" — this is the
        // estimate that lets a lookup of an absent key stay cheap.
        return 0.0;
    }
    remaining / other_distinct
}

/// The fraction of rows a range restriction keeps. Each bound is
/// `(value, inclusive)`; `None` is unbounded on that side.
///
/// The two sides are estimated independently, as fractions of the whole column,
/// and combined the way `clauselist_selectivity` combines a matched range pair:
/// `low + high - 1`. Independent estimates of a narrow range can cancel to zero
/// or below, so the result is floored at [`DEFAULT_RANGE_INEQ_SEL`].
pub fn range_selectivity(
    stats: Option<&ColStats>,
    ty: PgType,
    collation: u32,
    lower: Option<(&Value, bool)>,
    upper: Option<(&Value, bool)>,
) -> f64 {
    let low = lower.map(|(v, inclusive)| {
        bound_selectivity(stats, ty, collation, v, BoundSide::Lower, inclusive)
    });
    let high = upper.map(|(v, inclusive)| {
        bound_selectivity(stats, ty, collation, v, BoundSide::Upper, inclusive)
    });
    match (low, high) {
        (Some(low), Some(high)) => (low + high - 1.0).clamp(DEFAULT_RANGE_INEQ_SEL, 1.0),
        (Some(one), None) | (None, Some(one)) => one.clamp(0.0, 1.0),
        (None, None) => 1.0,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum BoundSide {
    /// `column > value` / `column >= value`.
    Lower,
    /// `column < value` / `column <= value`.
    Upper,
}

/// The fraction of rows one bound keeps: the MCVs that satisfy it, plus the part
/// of the histogram that lies on its side. NULLs satisfy no comparison, so the
/// null fraction is simply never counted.
fn bound_selectivity(
    stats: Option<&ColStats>,
    ty: PgType,
    collation: u32,
    value: &Value,
    side: BoundSide,
    inclusive: bool,
) -> f64 {
    let Some(stats) = stats else {
        return DEFAULT_INEQ_SEL;
    };
    if !describes(stats, value) {
        return DEFAULT_INEQ_SEL;
    }
    let satisfies = |candidate: &Value| {
        let ord = compare_values_collated(ty, candidate, value, collation);
        match (side, inclusive) {
            (BoundSide::Lower, true) => ord != Ordering::Less,
            (BoundSide::Lower, false) => ord == Ordering::Greater,
            (BoundSide::Upper, true) => ord != Ordering::Greater,
            (BoundSide::Upper, false) => ord == Ordering::Less,
        }
    };
    let mcv_mass: f64 = stats.mcv.iter().map(|(_, f)| f64::from(*f)).sum();
    let mcv_hit: f64 = stats
        .mcv
        .iter()
        .filter(|(v, _)| satisfies(v))
        .map(|(_, f)| f64::from(*f))
        .sum();
    let histogram_mass = (1.0 - mcv_mass - f64::from(stats.null_frac)).max(0.0);
    if stats.histogram.len() < 2 {
        // No histogram to divide: either the MCV list is the whole column (mass
        // zero, and the MCV hits are the answer) or nothing is known about the
        // rest, in which case PG's default applies to that part.
        let unknown = if histogram_mass > 0.0 {
            histogram_mass * DEFAULT_INEQ_SEL
        } else {
            0.0
        };
        return (mcv_hit + unknown).clamp(0.0, 1.0);
    }
    let below = histogram_fraction_below(ty, collation, &stats.histogram, value);
    let hist_hit = match side {
        BoundSide::Lower => 1.0 - below,
        BoundSide::Upper => below,
    };
    (mcv_hit + histogram_mass * hist_hit).clamp(0.0, 1.0)
}

/// Where `value` falls in an equi-depth histogram, as a fraction of the column's
/// histogram mass: 0 at or below the first bound, 1 at or above the last.
///
/// Each of the `n - 1` buckets holds the same fraction of the rows, so the
/// bucket index alone gives the answer to within one bucket; the position
/// *within* the bucket is interpolated for types that convert to a number and
/// assumed to be the midpoint for those that do not (PostgreSQL interpolates on
/// string bytes there — a refinement, not a different model).
fn histogram_fraction_below(ty: PgType, collation: u32, histogram: &[Value], value: &Value) -> f64 {
    let buckets = (histogram.len() - 1) as f64;
    if compare_values_collated(ty, value, &histogram[0], collation) != Ordering::Greater {
        return 0.0;
    }
    if compare_values_collated(ty, value, &histogram[histogram.len() - 1], collation)
        != Ordering::Less
    {
        return 1.0;
    }
    // The last bound strictly greater than `value` ends the bucket holding it.
    let upper = histogram
        .iter()
        .position(|b| compare_values_collated(ty, b, value, collation) == Ordering::Greater)
        // Both sentinels above are already handled, so a bound above `value`
        // exists and it is not the first one.
        .expect("a histogram bound above the value exists");
    let within = scalar_fraction(&histogram[upper - 1], value, &histogram[upper]).unwrap_or(0.5);
    ((upper - 1) as f64 + within) / buckets
}

/// Where `value` sits between `low` and `high`, as a fraction, for the types
/// that convert to a number without losing their order. `None` for the types
/// that do not — the caller then assumes the midpoint.
fn scalar_fraction(low: &Value, value: &Value, high: &Value) -> Option<f64> {
    let (low, value, high) = (as_scalar(low)?, as_scalar(value)?, as_scalar(high)?);
    let span = high - low;
    if span <= 0.0 {
        return Some(0.5);
    }
    Some(((value - low) / span).clamp(0.0, 1.0))
}

/// A value's position on a number line, for interpolation only: the mapping has
/// to be monotone in the type's own order, and nothing more. Types with no such
/// mapping (text, uuid, arrays, …) return `None`.
fn as_scalar(value: &Value) -> Option<f64> {
    Some(match value {
        Value::Int2(v) => f64::from(*v),
        Value::Int4(v) => f64::from(*v),
        Value::Int8(v) => *v as f64,
        Value::Oid(v) => f64::from(*v),
        Value::Float4(v) => f64::from(*v),
        Value::Float8(v) => *v,
        Value::Numeric(n) => n.to_f64(),
        Value::Money(c) => *c as f64,
        Value::Date(d) => f64::from(*d),
        Value::Time(t) | Value::Timestamp(t) | Value::TimestampTz(t) => *t as f64,
        Value::Bool(b) => f64::from(u8::from(*b)),
        _ => return None,
    })
}

/// Whether these statistics are about values of `value`'s kind, and so may be
/// compared with it.
///
/// A comparand of another kind is not a defect to reject loudly — the binder may
/// legitimately have promoted a comparison to a wider type than the column, as
/// in `int4_col = 5000000000` — but comparing across kinds would panic in
/// [`compare_values_collated`], which is settled at bind time and has no
/// business being asked here. The caller falls back to a distribution-free
/// estimate instead.
fn describes(stats: &ColStats, value: &Value) -> bool {
    let kind = value.pg_type();
    let known = stats
        .mcv
        .first()
        .map(|(v, _)| v)
        .or_else(|| stats.histogram.first());
    known.is_none_or(|v| v.pg_type() == kind)
}

/// `pg_stats`' signed distinct count as an absolute number of values: a negative
/// entry is the negated fraction of the relation. `None` when nothing was
/// measured (a zero entry), which is not the same as "no distinct values".
fn decode_n_distinct(n_distinct: f32, rows: f64) -> Option<f64> {
    let n = f64::from(n_distinct);
    if n > 0.0 {
        Some(n)
    } else if n < 0.0 {
        Some((-n * rows).max(1.0))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_types::collation::DEFAULT_COLLATION_OID;

    const C: u32 = DEFAULT_COLLATION_OID;

    /// 1000 rows: "a" in half of them, then 100 evenly spaced integers.
    fn skewed() -> ColStats {
        ColStats {
            null_frac: 0.1,
            avg_width: 4,
            n_distinct: 11.0,
            mcv: vec![(Value::Int4(0), 0.5)],
            histogram: (1..=5).map(|i| Value::Int4(i * 100)).collect(),
            correlation: 0.0,
        }
    }

    #[test]
    fn an_mcv_reports_its_own_frequency() {
        let stats = skewed();
        assert_eq!(
            eq_selectivity(Some(&stats), PgType::Int4, C, Some(&Value::Int4(0)), 1000.0),
            0.5
        );
    }

    #[test]
    fn a_non_mcv_shares_what_the_mcvs_and_nulls_leave() {
        let stats = skewed();
        // 1 - 0.5 (mcv) - 0.1 (null) = 0.4, spread over 11 - 1 = 10 values.
        let sel = eq_selectivity(
            Some(&stats),
            PgType::Int4,
            C,
            Some(&Value::Int4(250)),
            1000.0,
        );
        assert!((sel - 0.04).abs() < 1e-9, "sel: {sel}");
    }

    #[test]
    fn a_value_the_mcv_list_rules_out_keeps_no_rows() {
        // Two distinct values, both in the MCV list: anything else is absent.
        let stats = ColStats {
            null_frac: 0.0,
            avg_width: 4,
            n_distinct: 2.0,
            mcv: vec![(Value::Int4(1), 0.7), (Value::Int4(2), 0.3)],
            histogram: Vec::new(),
            correlation: 0.0,
        };
        assert_eq!(
            eq_selectivity(Some(&stats), PgType::Int4, C, Some(&Value::Int4(3)), 1000.0),
            0.0
        );
    }

    /// The binder may compare a column against a wider type than its own
    /// (`int4_col = 5000000000`). Statistics about `int4` values cannot answer
    /// that, and asking would panic in the comparator, so the estimate falls back
    /// to the distinct count.
    #[test]
    fn a_comparand_of_another_type_falls_back_instead_of_comparing() {
        let stats = skewed();
        let sel = eq_selectivity(
            Some(&stats),
            PgType::Int8,
            C,
            Some(&Value::Int8(5_000_000_000)),
            1000.0,
        );
        assert!((sel - 0.9 / 11.0).abs() < 1e-9, "sel: {sel}");
    }

    #[test]
    fn an_unmeasured_column_uses_the_defaults() {
        assert_eq!(
            eq_selectivity(None, PgType::Int4, C, Some(&Value::Int4(1)), 1000.0),
            DEFAULT_EQ_SEL
        );
        assert_eq!(
            range_selectivity(None, PgType::Int4, C, Some((&Value::Int4(1), false)), None),
            DEFAULT_INEQ_SEL
        );
    }

    #[test]
    fn an_unknown_comparand_spreads_over_the_distinct_values() {
        let stats = skewed();
        // Not the MCV frequency: the planner does not know which value it gets.
        let sel = eq_selectivity(Some(&stats), PgType::Int4, C, None, 1000.0);
        assert!((sel - 0.9 / 11.0).abs() < 1e-9, "sel: {sel}");
    }

    #[test]
    fn a_bound_splits_the_histogram_where_it_falls() {
        // No MCVs, no NULLs: bounds 0,100,200,300,400 over four equal buckets.
        let stats = ColStats {
            null_frac: 0.0,
            avg_width: 4,
            n_distinct: -1.0,
            mcv: Vec::new(),
            histogram: (0..=4).map(|i| Value::Int4(i * 100)).collect(),
            correlation: 0.0,
        };
        // Exactly on the second bound: two of four buckets are below it.
        let sel = range_selectivity(
            Some(&stats),
            PgType::Int4,
            C,
            None,
            Some((&Value::Int4(200), false)),
        );
        assert!((sel - 0.5).abs() < 1e-9, "sel: {sel}");
        // Halfway through the first bucket.
        let sel = range_selectivity(
            Some(&stats),
            PgType::Int4,
            C,
            None,
            Some((&Value::Int4(50), false)),
        );
        assert!((sel - 0.125).abs() < 1e-9, "sel: {sel}");
        // Everything is above the lowest bound.
        let sel = range_selectivity(
            Some(&stats),
            PgType::Int4,
            C,
            Some((&Value::Int4(0), true)),
            None,
        );
        assert!((sel - 1.0).abs() < 1e-9, "sel: {sel}");
    }

    #[test]
    fn a_two_sided_range_keeps_the_slice_between_the_bounds() {
        let stats = ColStats {
            null_frac: 0.0,
            avg_width: 4,
            n_distinct: -1.0,
            mcv: Vec::new(),
            histogram: (0..=4).map(|i| Value::Int4(i * 100)).collect(),
            correlation: 0.0,
        };
        let sel = range_selectivity(
            Some(&stats),
            PgType::Int4,
            C,
            Some((&Value::Int4(100), true)),
            Some((&Value::Int4(200), false)),
        );
        // Above 100 keeps 0.75, below 200 keeps 0.5: one bucket of four.
        assert!((sel - 0.25).abs() < 1e-9, "sel: {sel}");
    }

    #[test]
    fn a_range_that_cancels_out_still_keeps_a_floor() {
        let stats = ColStats {
            null_frac: 0.0,
            avg_width: 4,
            n_distinct: -1.0,
            mcv: Vec::new(),
            histogram: (0..=4).map(|i| Value::Int4(i * 100)).collect(),
            correlation: 0.0,
        };
        // An empty range: independent estimates would give a negative fraction.
        let sel = range_selectivity(
            Some(&stats),
            PgType::Int4,
            C,
            Some((&Value::Int4(300), true)),
            Some((&Value::Int4(100), true)),
        );
        assert_eq!(sel, DEFAULT_RANGE_INEQ_SEL);
    }

    /// The estimate follows the relation, not the measurement. A table analyzed
    /// at 10 pages and since grown to 100 is planned as ten times the rows —
    /// otherwise it would keep the plan it earned while it was small, forever,
    /// since nothing re-analyzes it.
    #[test]
    fn a_grown_relation_is_estimated_from_its_current_size() {
        let schema = TableSchema::new(
            "t",
            vec![crabgresql_storage_api::Column::new("id", PgType::Int4)],
        );
        let analyzed = RelStats {
            relpages: 10,
            reltuples: 1_000.0,
            analyzed: true,
            curpages: Some(100),
            columns: std::sync::Arc::from([]),
        };
        let size = estimate_rel_size(&analyzed, &schema);
        assert_eq!(size.pages, 100.0);
        assert_eq!(size.rows, 10_000.0);

        // An engine that cannot report a live size keeps exactly what it
        // measured — no guessing in either direction.
        let size = estimate_rel_size(
            &RelStats {
                curpages: None,
                ..analyzed
            },
            &schema,
        );
        assert_eq!((size.pages, size.rows), (10.0, 1_000.0));
    }

    /// The other direction: a relation measured empty really is free to scan,
    /// and must not be inflated back to the unmeasured default.
    #[test]
    fn a_relation_measured_empty_stays_empty() {
        let schema = TableSchema::new(
            "t",
            vec![crabgresql_storage_api::Column::new("id", PgType::Int4)],
        );
        let size = estimate_rel_size(
            &RelStats {
                relpages: 4,
                reltuples: 0.0,
                analyzed: true,
                curpages: Some(4),
                columns: std::sync::Arc::from([]),
            },
            &schema,
        );
        assert_eq!(size.rows, 0.0);
    }

    #[test]
    fn an_unmeasured_relation_is_never_free_to_scan() {
        let schema = TableSchema::new(
            "t",
            vec![crabgresql_storage_api::Column::new("id", PgType::Int4)],
        );
        let size = estimate_rel_size(&RelStats::unknown(&schema), &schema);
        assert_eq!(size.pages, f64::from(DEFAULT_RELPAGES));
        assert!(size.rows > 0.0);
        assert!(seq_scan_cost(CostSettings::default(), size, 1) > 0.0);
    }

    /// The decision this model exists to make: a selective probe beats a scan of
    /// a large relation, and loses on a tiny one — which is what PostgreSQL does,
    /// because four random pages cost more than reading the whole table.
    #[test]
    fn an_index_wins_on_a_big_relation_and_loses_on_a_small_one() {
        let big = RelSize {
            pages: 5_000.0,
            rows: 500_000.0,
        };
        assert!(
            index_scan_cost(CostSettings::default(), big, 200.0, 1.0 / 500_000.0, 0.0, 1)
                < seq_scan_cost(CostSettings::default(), big, 1)
        );

        let small = RelSize {
            pages: 1.0,
            rows: 5.0,
        };
        assert!(
            index_scan_cost(CostSettings::default(), small, 1.0, 0.2, 0.0, 1)
                > seq_scan_cost(CostSettings::default(), small, 1)
        );
    }

    /// A wide range through an index is worse than reading the relation, because
    /// it reads the same pages in random order — the regression the model is here
    /// to prevent once range scans can be planned.
    #[test]
    fn an_unselective_range_loses_to_a_sequential_scan() {
        let size = RelSize {
            pages: 5_000.0,
            rows: 500_000.0,
        };
        assert!(
            index_scan_cost(CostSettings::default(), size, 200.0, 0.5, 0.0, 1)
                > seq_scan_cost(CostSettings::default(), size, 1)
        );
    }

    /// Physical order is worth real money: the same fetch is cheaper when the
    /// column is stored in index order, because the pages come back sequentially.
    #[test]
    fn correlation_makes_the_same_fetch_cheaper() {
        let size = RelSize {
            pages: 5_000.0,
            rows: 500_000.0,
        };
        let random = index_scan_cost(CostSettings::default(), size, 200.0, 0.05, 0.0, 1);
        let ordered = index_scan_cost(CostSettings::default(), size, 200.0, 0.05, 1.0, 1);
        assert!(ordered < random, "ordered {ordered} vs random {random}");
    }
}
