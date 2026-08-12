//! The row-producing SELECT nodes: a constant row list, the two single-table
//! access paths, a wrapped subplan, a table function, an inheritance/partition
//! union, and the LIMIT/OFFSET wrapper that sits above any of them.

use std::sync::Arc;

use crabgresql_binder::{BoundExpr, DistinctKey, MappedRelation, OutputColumn, SortKey, TableFn};
use crabgresql_storage_api::{ColumnProjection, TableAm};

use super::PhysicalPlan;

/// [`PhysicalPlan::Values`]: one or more constant rows.
pub struct PhysicalValues {
    pub columns: Vec<OutputColumn>,
    pub rows: Vec<Vec<BoundExpr>>,
    pub predicate: Option<BoundExpr>,
    pub sort: Vec<SortKey>,
    pub distinct: Option<Vec<DistinctKey>>,
}

/// [`PhysicalPlan::Select`]: a single-table read served by a sequential scan,
/// then the standard Filter → Projection → Sort tail.
pub struct PhysicalSelect {
    pub table: Arc<dyn TableAm>,
    /// The columns this scan's own expressions read, for engines that can
    /// skip the rest (see the `projection` pass). Rows stay full width regardless.
    pub projection: ColumnProjection,
    pub columns: Vec<OutputColumn>,
    pub projections: Vec<BoundExpr>,
    pub predicate: Option<BoundExpr>,
    pub sort: Vec<SortKey>,
    pub distinct: Option<Vec<DistinctKey>>,
}

/// [`PhysicalPlan::IndexScan`]: a single-table read served by an equality probe
/// on `index_name`: the executor evaluates each `key` value once and asks the
/// engine for the matching rows. The planner only emits this when the engine
/// reports [`TableAm::supports_index_scan`], but the executor still
/// scan-fallbacks defensively. `predicate` is the residual WHERE the index did
/// not consume, applied as a `Filter`; the standard Projection → Sort tail
/// follows, exactly as for [`PhysicalSelect`].
pub struct PhysicalIndexScan {
    pub table: Arc<dyn TableAm>,
    /// As for [`PhysicalSelect`], and additionally always covering every
    /// `key` column: the executor's scan fallback re-checks the key per row.
    pub projection: ColumnProjection,
    pub index_name: String,
    /// One `(key column, equality value)` pair per index key column, in key
    /// order. The value expressions are row-constant and evaluated once.
    pub key: Vec<(usize, BoundExpr)>,
    pub columns: Vec<OutputColumn>,
    pub projections: Vec<BoundExpr>,
    pub predicate: Option<BoundExpr>,
    pub sort: Vec<SortKey>,
    pub distinct: Option<Vec<DistinctKey>>,
}

/// [`PhysicalPlan::Subquery`]: `source` produces the input rows; the same
/// projection/predicate/sort pipeline as [`PhysicalSelect`] runs on top.
pub struct PhysicalSubquery {
    pub source: Box<PhysicalPlan>,
    pub columns: Vec<OutputColumn>,
    pub projections: Vec<BoundExpr>,
    pub predicate: Option<BoundExpr>,
    pub sort: Vec<SortKey>,
    pub distinct: Option<Vec<DistinctKey>>,
}

/// [`PhysicalPlan::TableFunction`]: source rows come from evaluating `func` with
/// `args`; the same projection/predicate/sort pipeline as [`PhysicalSelect`]
/// runs on top.
pub struct PhysicalTableFunction {
    pub func: TableFn,
    pub args: Vec<BoundExpr>,
    pub columns: Vec<OutputColumn>,
    pub projections: Vec<BoundExpr>,
    pub predicate: Option<BoundExpr>,
    pub sort: Vec<SortKey>,
    pub distinct: Option<Vec<DistinctKey>>,
}

/// [`PhysicalPlan::Append`]: a union scan over the several relations one FROM
/// item names. Mirrors [`AppendPlan`](crabgresql_binder::AppendPlan): the
/// executor concatenates each arm's scan into one row stream. Such a FROM item
/// is planned as a [`PhysicalSubquery`] wrapping this, so the standard
/// projection/predicate/sort tail runs on top.
pub struct PhysicalAppend {
    pub arms: Vec<PhysicalAppendArm>,
    pub columns: Vec<OutputColumn>,
}

/// One arm of a [`PhysicalAppend`]: a [`MappedRelation`] plus the columns its
/// scan must materialize.
///
/// The relation is embedded rather than flattened so the permutation has one
/// definition — the executor reads a row through [`MappedRelation::view`]
/// instead of open-coding the same indexing a second time.
///
/// The projection is per-arm rather than shared: with a remap in play, an
/// ordinal in one arm's schema names a different column in another's, so a
/// single [`ColumnProjection`] could not be right for both.
pub struct PhysicalAppendArm {
    pub relation: MappedRelation,
    /// Which of this arm's own columns the scan must materialize. Supplied by
    /// the wrapping [`PhysicalSubquery`], which owns the expressions that
    /// read these rows, translated through the map into this arm's ordinals.
    pub projection: ColumnProjection,
}

/// [`PhysicalPlan::Limit`]: LIMIT/OFFSET above a source plan (after its sort).
/// Mirrors [`LimitPlan`](crabgresql_binder::LimitPlan).
pub struct PhysicalLimit {
    pub source: Box<PhysicalPlan>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}
