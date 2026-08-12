//! The `UNION` / `UNION ALL` node and its arms.

use crabgresql_binder::{BoundExpr, DistinctKey, OutputColumn, SortKey};

use super::PhysicalPlan;

/// [`PhysicalPlan::SetOp`]: a `UNION` / `UNION ALL`. Mirrors
/// [`SetOpPlan`](crabgresql_binder::SetOpPlan): the executor drains each arm
/// into one row stream, coercing arms that need it, then applies this node's own
/// deduplication and sort.
pub struct PhysicalSetOp {
    pub arms: Vec<PhysicalSetOpArm>,
    pub columns: Vec<OutputColumn>,
    pub sort: Vec<SortKey>,
    pub distinct: Option<Vec<DistinctKey>>,
}

/// One arm of a [`PhysicalSetOp`], mirroring
/// [`SetOpArm`](crabgresql_binder::SetOpArm).
pub struct PhysicalSetOpArm {
    pub plan: PhysicalPlan,
    /// Projections mapping this arm onto the set operation's output layout;
    /// `None` when it already emits that layout.
    pub coercion: Option<Vec<BoundExpr>>,
}
