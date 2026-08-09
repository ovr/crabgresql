//! `GROUP BY` / `HAVING` and aggregate calls, plus the row source they consume.

use std::sync::Arc;

use crabgresql_storage_api::TableAm;

use crate::expr::BoundExpr;
use crate::{BoundAggregate, OutputColumn};

use super::{DistinctKey, JoinExpr, SortKey};

/// [`LogicalPlan::Aggregate`]: `GROUP BY` / `HAVING` and/or aggregate calls in
/// the target list. The physical pipeline is
/// `input → Filter(predicate) → Aggregate → [Filter(having)] → Projection →
/// Sort`: `predicate` (WHERE) filters the source *before* aggregation, the
/// aggregate node emits one row per group laid out `[group keys…, aggregates…]`,
/// `having` filters those rows, and `projections`/`sort` (whose aggregate and
/// grouped-column references were rewritten to `ColumnRef`s into that row)
/// produce the visible output. An empty `group_exprs` is the implicit single
/// group (`SELECT count(*) …` — always one output row).
///
/// [`LogicalPlan::Aggregate`]: super::LogicalPlan::Aggregate
#[derive(Clone)]
pub struct AggregatePlan {
    pub input: AggInput,
    pub predicate: Option<BoundExpr>,
    pub group_exprs: Vec<BoundExpr>,
    pub aggregates: Vec<BoundAggregate>,
    pub having: Option<BoundExpr>,
    pub columns: Vec<OutputColumn>,
    pub projections: Vec<BoundExpr>,
    pub sort: Vec<SortKey>,
    pub distinct: Option<Vec<DistinctKey>>,
}

/// The row source feeding a [`LogicalPlan::Aggregate`]. `Scan` is a single base
/// table; `Join` is any other FROM source — a recursively joined tree or a
/// single-input node (derived table, CTE reference, `VALUES`, or set-returning
/// function); `SingleRow` is the one virtual row of a FROM-less aggregate
/// (`SELECT count(*)`).
///
/// [`LogicalPlan::Aggregate`]: super::LogicalPlan::Aggregate
#[derive(Clone)]
pub enum AggInput {
    Scan(Arc<dyn TableAm>),
    Join(JoinExpr),
    SingleRow,
}
