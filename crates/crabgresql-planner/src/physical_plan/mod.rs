//! The physical plan: [`PhysicalPlan`] and the node types its variants wrap.
//!
//! Pure data — the shape a bound statement takes once an access path, a join
//! algorithm and a column projection have been chosen for it. The lowering that
//! *builds* these nodes lives in [`crate`]; the executor consumes them.

mod aggregate;
mod dml;
mod join;
mod query;
mod setop;
mod window;

pub use aggregate::{PhysicalAggInput, PhysicalAggScan, PhysicalAggregate};
pub use dml::{
    DmlIndexProbe, DmlTarget, PhysicalDelete, PhysicalInsert, PhysicalInsertQuery,
    PhysicalInsertSource, PhysicalInsertTuples, PhysicalUpdate,
};
pub use join::{
    HashKey, PhysicalJoin, PhysicalJoinExpr, PhysicalJoinInput, PhysicalJoinLeaf, PhysicalJoinPair,
    PhysicalJoinScan, PhysicalJoinTableFunction,
};
pub use query::{
    PhysicalAppend, PhysicalAppendArm, PhysicalIndexScan, PhysicalLimit, PhysicalSelect,
    PhysicalSubquery, PhysicalTableFunction, PhysicalValues,
};
pub use setop::{PhysicalSetOp, PhysicalSetOpArm};
pub use window::PhysicalWindow;

/// An executable plan. [`PhysicalSelect`] describes the SeqScan → Filter →
/// Projection → Sort pipeline the executor builds.
///
/// Every variant is a one-field wrapper around a named struct, so a node can be
/// passed, returned and destructured as a value of its own type instead of only
/// through a `match` arm on the enum — as for
/// [`LogicalPlan`](crabgresql_binder::LogicalPlan), which this mirrors.
pub enum PhysicalPlan {
    /// FROM-less SELECT (`SELECT 1`) or a standalone `VALUES` list.
    Values(PhysicalValues),
    /// A single-table read served by a sequential scan.
    Select(PhysicalSelect),
    /// A single-table read served by an equality index probe.
    IndexScan(PhysicalIndexScan),
    /// SELECT over a subplan.
    Subquery(PhysicalSubquery),
    /// SELECT over a set-returning function in FROM position.
    TableFunction(PhysicalTableFunction),
    /// SELECT over a recursive join tree.
    Join(PhysicalJoin),
    /// Grouped aggregation over a single row source.
    Aggregate(PhysicalAggregate),
    /// Union scan over the several relations one FROM item names.
    Append(PhysicalAppend),
    /// A `UNION` / `UNION ALL`.
    SetOp(PhysicalSetOp),
    /// One step of window-function evaluation.
    Window(PhysicalWindow),
    /// LIMIT/OFFSET above a source plan (after its sort).
    Limit(PhysicalLimit),
    /// INSERT from a `VALUES` list, formed values, or a query source.
    Insert(PhysicalInsert),
    /// UPDATE of one relation and, for a partitioned or inheriting target, the
    /// relations it fans out to.
    Update(PhysicalUpdate),
    /// DELETE from one relation and the relations it fans out to.
    Delete(PhysicalDelete),
}
