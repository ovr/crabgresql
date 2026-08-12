//! The logical plan: [`LogicalPlan`] and the node types its variants wrap.
//!
//! Pure data — the shape a bound statement takes once names are resolved and
//! expressions are typed. The binding logic that *builds* these nodes lives in
//! [`crate::plan`]; the executor consumes them.

mod aggregate;
mod dml;
mod join;
mod keys;
mod query;
mod relation;
mod setop;
mod visit;
mod window;

pub use aggregate::{AggInput, AggregatePlan};
pub use dml::{DeletePlan, InsertPlan, InsertSource, Returning, UpdatePlan};
pub use join::{JoinExpr, JoinInput, JoinKind, JoinPlan};
pub use keys::{DistinctKey, SortKey};
pub use query::{AppendPlan, LimitPlan, QueryPlan, SubqueryPlan, TableFunctionPlan, ValuesPlan};
pub use relation::{MappedRelation, RelationIdent};
pub use setop::{SetOpArm, SetOpPlan};
pub use visit::{ExprVisitor, walk_exprs_mut};
pub use window::WindowPlan;

/// A bound query or DML statement.
///
/// Every variant is a one-field wrapper around a named struct, so a node can be
/// passed, returned and destructured as a value of its own type instead of only
/// through a `match` arm on the enum.
#[derive(Clone)]
pub enum LogicalPlan {
    /// FROM-less SELECT (`SELECT 1`) or a standalone `VALUES` list.
    Values(ValuesPlan),
    /// Single-table SELECT with optional predicate.
    Query(QueryPlan),
    /// Union scan over the several relations one FROM item names.
    Append(AppendPlan),
    /// A `UNION` / `UNION ALL`.
    ///
    /// TODO: also represent `INTERSECT` and `EXCEPT`, which the binder rejects
    /// as unsupported.
    SetOp(SetOpPlan),
    /// SELECT over a subquery source in FROM, and the binder's general
    /// same-level projection wrapper.
    Subquery(SubqueryPlan),
    /// SELECT over a set-returning function in FROM position.
    TableFunction(TableFunctionPlan),
    /// SELECT over a recursive join tree.
    Join(JoinPlan),
    /// LIMIT/OFFSET applied above a SELECT body.
    Limit(LimitPlan),
    /// Aggregation over a single row source.
    Aggregate(AggregatePlan),
    /// One step of window-function evaluation.
    Window(WindowPlan),
    /// INSERT from a `VALUES` list, formed values, or a query source.
    Insert(InsertPlan),
    /// UPDATE of one relation and, for a partitioned or inheriting target, the
    /// relations it fans out to.
    Update(UpdatePlan),
    /// DELETE from one relation and the relations it fans out to.
    Delete(DeletePlan),
}
