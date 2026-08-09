//! One step of window-function evaluation.

use crate::expr::{BoundWindowFunc, BoundWindowSpec};

use super::LogicalPlan;

/// [`LogicalPlan::Window`]: partition `source` by `spec`, order each partition,
/// and fill this step's `funcs` into the row.
///
/// Windows are evaluated after WHERE and after GROUP BY/HAVING, but before
/// the query's own projection, ORDER BY and DISTINCT. So this carries no
/// projection tail of its own — it is a bare row source, and the surrounding
/// SELECT is bound as a [`SubqueryPlan`] wrapping the chain. That wrapper
/// is *not* a query nesting level (unlike a derived table, which shares the
/// node); see [`substitute_outer`].
///
/// `source` is the query block the binder would have built anyway, but with
/// identity projections and no sort/distinct, so this node's `spec` and
/// `funcs` read the raw pre-window row and every `ColumnRef` the target list
/// already held stays valid — unlike [`AggregatePlan`], which collapses the
/// row.
///
/// Every node in a chain emits `output_width` columns: the input row, then
/// one slot per window call in the *whole* chain. A node fills only the slots
/// its own `funcs` name (see [`BoundWindowFunc::slot`]) and leaves the rest
/// as they were, so the chain order and the slot order are independent. The
/// bottom node widens the row; the others find it already wide.
///
/// PG evaluates the spec with the most keys first and the fewest last, and
/// the last one's sort is what the query returns when it has no ORDER BY of
/// its own — so the chain order is observable, not just a cost choice.
///
/// [`LogicalPlan::Window`]: super::LogicalPlan::Window
/// [`SubqueryPlan`]: super::SubqueryPlan
/// [`AggregatePlan`]: super::AggregatePlan
/// [`substitute_outer`]: crate::substitute_outer
#[derive(Clone)]
pub struct WindowPlan {
    pub source: Box<LogicalPlan>,
    pub spec: BoundWindowSpec,
    pub funcs: Vec<BoundWindowFunc>,
    /// Width of the pre-window row: where the window slots begin.
    pub input_width: usize,
    /// `input_width` + the number of window calls in the whole chain.
    pub output_width: usize,
}
