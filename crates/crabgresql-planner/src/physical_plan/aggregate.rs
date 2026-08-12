//! The aggregation node and the row sources it can draw from.

use std::sync::Arc;

use crabgresql_binder::{BoundAggregate, BoundExpr, DistinctKey, OutputColumn, SortKey};
use crabgresql_storage_api::{ColumnProjection, TableAm};

use super::PhysicalJoinExpr;

/// [`PhysicalPlan::Aggregate`](super::PhysicalPlan::Aggregate): grouped
/// aggregation. Mirrors [`AggregatePlan`](crabgresql_binder::AggregatePlan): the
/// executor filters `input` by `predicate`, groups by `group_exprs`, accumulates
/// the `aggregates`, filters groups by `having`, then runs the standard
/// projection/sort tail.
pub struct PhysicalAggregate {
    pub input: PhysicalAggInput,
    pub predicate: Option<BoundExpr>,
    pub group_exprs: Vec<BoundExpr>,
    pub aggregates: Vec<BoundAggregate>,
    pub having: Option<BoundExpr>,
    pub columns: Vec<OutputColumn>,
    pub projections: Vec<BoundExpr>,
    pub sort: Vec<SortKey>,
    pub distinct: Option<Vec<DistinctKey>>,
}

/// The row source of a [`PhysicalAggregate`], mirroring
/// [`AggInput`](crabgresql_binder::AggInput).
pub enum PhysicalAggInput {
    Scan(PhysicalAggScan),
    Join(PhysicalJoinExpr),
    SingleRow,
}

/// [`PhysicalAggInput::Scan`]: a base-table row source.
pub struct PhysicalAggScan {
    pub table: Arc<dyn TableAm>,
    /// The columns the grouping keys, aggregate arguments and WHERE read.
    pub projection: ColumnProjection,
}
