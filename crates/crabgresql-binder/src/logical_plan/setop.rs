//! `UNION` and friends: the N-ary set-operation node and its arms.

use crate::OutputColumn;
use crate::expr::BoundExpr;

use super::{DistinctKey, LogicalPlan, SortKey};

/// One arm of a [`LogicalPlan::SetOp`].
#[derive(Clone)]
pub struct SetOpArm {
    pub plan: LogicalPlan,
    /// Projections mapping this arm's own columns onto the set operation's
    /// unified output layout; `None` when the arm already emits that layout.
    pub coercion: Option<Vec<BoundExpr>>,
}

/// [`LogicalPlan::SetOp`]: concatenate every arm, then optionally deduplicate
/// and sort. `columns` is the unified output layout (per-position common types,
/// named from the left arm).
///
/// This node owns its whole tail rather than delegating to a wrapping
/// [`SubqueryPlan`], so that an arm's projection and its coercion onto
/// `columns` stay in the arm's own index space — a wrapper would have to
/// re-derive both.
///
/// The node is N-ary, and `plan::bind_set_operation` flattens a chain of
/// equivalent operations into one node (`a UNION b UNION c` is three arms,
/// not nested pairs), matching PG's single Append over N children. Besides
/// keeping the plan shallow, that collapses the redundant per-level
/// deduplication a nested encoding would produce.
///
/// [`SubqueryPlan`]: super::SubqueryPlan
#[derive(Clone)]
pub struct SetOpPlan {
    /// Two or more arms, in query order.
    pub arms: Vec<SetOpArm>,
    pub columns: Vec<OutputColumn>,
    /// A query-level `ORDER BY` over the combined result.
    pub sort: Vec<SortKey>,
    /// `Some(all output columns)` for `UNION`; `None` for `UNION ALL`.
    pub distinct: Option<Vec<DistinctKey>>,
}
