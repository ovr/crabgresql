//! The row-producing SELECT nodes: a constant row list, a base-table scan, an
//! inheritance/partition union, a wrapped subplan, a table function, and the
//! LIMIT/OFFSET wrapper that sits above any of them.

use std::sync::Arc;

use crabgresql_storage_api::TableAm;

use crate::expr::BoundExpr;
use crate::{OutputColumn, TableFn};

use super::{DistinctKey, LogicalPlan, MappedRelation, SortKey, SystemEmit};

/// [`LogicalPlan::Values`]: one or more constant rows. A predicate (`SELECT 1
/// WHERE false`) contains no column references — it bound in the empty scope.
#[derive(Clone)]
pub struct ValuesPlan {
    pub columns: Vec<OutputColumn>,
    pub rows: Vec<Vec<BoundExpr>>,
    pub predicate: Option<BoundExpr>,
    pub sort: Vec<SortKey>,
    pub distinct: Option<Vec<DistinctKey>>,
}

/// [`LogicalPlan::Query`]: a single-table SELECT with optional predicate.
#[derive(Clone)]
pub struct QueryPlan {
    pub table: Arc<dyn TableAm>,
    /// The system columns this scan appends past the relation's declared ones;
    /// see [`JoinInput::Scan`](super::JoinInput::Scan).
    pub system: Option<SystemEmit>,
    pub columns: Vec<OutputColumn>,
    pub projections: Vec<BoundExpr>,
    pub predicate: Option<BoundExpr>,
    pub sort: Vec<SortKey>,
    pub distinct: Option<Vec<DistinctKey>>,
}

/// [`LogicalPlan::Append`]: the leaf partitions of a partitioned parent, or an
/// inheritance parent together with its descendants. The node emits every arm's
/// rows in arm order, each remapped to the full width of the relation that was
/// named. It carries no projection/predicate/sort of its own — such a FROM item
/// is bound as a [`SubqueryPlan`] wrapping this Append, so the surrounding
/// SELECT's WHERE/projection/ORDER BY/DISTINCT apply on top (and joins /
/// aggregates reuse the same subplan machinery).
#[derive(Clone)]
pub struct AppendPlan {
    pub arms: Vec<MappedRelation>,
    pub columns: Vec<OutputColumn>,
}

/// [`LogicalPlan::Subquery`]: a derived table (`(SELECT ...) s`) or a CTE
/// reference. `source` produces the input rows; the same
/// projection/predicate/sort pipeline as [`QueryPlan`] runs on top.
///
/// Also the binder's general **same-level projection wrapper**: it carries
/// the tail for a window chain (`plan::finish_windowed_select`), a sorted
/// `Limit` (`plan::attach_sort`) and a FROM-less SRF. So this node does *not*
/// imply a query nesting level — see [`substitute_outer`].
///
/// [`substitute_outer`]: crate::substitute_outer
#[derive(Clone)]
pub struct SubqueryPlan {
    pub source: Box<LogicalPlan>,
    pub columns: Vec<OutputColumn>,
    pub projections: Vec<BoundExpr>,
    pub predicate: Option<BoundExpr>,
    pub sort: Vec<SortKey>,
    pub distinct: Option<Vec<DistinctKey>>,
}

/// [`LogicalPlan::TableFunction`]: source rows come from evaluating `func` with
/// `args`; the same projection/predicate/sort pipeline as [`QueryPlan`] runs on
/// top.
#[derive(Clone)]
pub struct TableFunctionPlan {
    pub func: TableFn,
    pub args: Vec<BoundExpr>,
    pub ordinality: bool,
    pub columns: Vec<OutputColumn>,
    pub projections: Vec<BoundExpr>,
    pub predicate: Option<BoundExpr>,
    pub sort: Vec<SortKey>,
    pub distinct: Option<Vec<DistinctKey>>,
}

/// [`LogicalPlan::Limit`]: LIMIT/OFFSET applied above a SELECT body — after its
/// ORDER BY, since PG evaluates the count clauses on the ordered result.
/// `source` produces the (ordered) rows; this node skips `offset` of them and
/// stops after `limit`. A wrapper rather than a field on every SELECT node:
/// LIMIT/OFFSET is a query-level construct that sits above the whole select,
/// mirroring PG's Limit plan node above the sort.
#[derive(Clone)]
pub struct LimitPlan {
    pub source: Box<LogicalPlan>,
    /// `None` = no limit (`LIMIT ALL` or clause absent).
    pub limit: Option<i64>,
    /// `None` = `OFFSET 0` (clause absent).
    pub offset: Option<i64>,
}
