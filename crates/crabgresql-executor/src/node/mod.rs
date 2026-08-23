//! The row executor's plan nodes: one [`ExecNode`](crate::ExecNode) per file.
//!
//! Two modules here carry no node of their own. [`join`] holds the scratch-row
//! machinery the two join nodes share, so a change to how a candidate pair is
//! tested cannot reach one of them and miss the other; [`series`] holds the
//! set-returning row sources that a FROM-position table function and a
//! target-list SRF both build.

mod aggregate;
mod append;
mod concat;
mod distinct;
mod filter;
mod hash_join;
mod index_scan;
mod join;
mod lateral_join;
mod limit;
mod materialized_rows;
mod nested_loop_join;
mod project_set;
mod projection;
mod seq_scan;
mod series;
mod sort;
mod table_function;
mod values;
mod window_agg;

pub use aggregate::Aggregate;
pub use append::Append;
pub use concat::Concat;
pub use distinct::Distinct;
pub use filter::Filter;
pub use hash_join::HashJoin;
pub use index_scan::IndexScan;
pub use lateral_join::LateralJoin;
pub use limit::Limit;
pub use materialized_rows::MaterializedRows;
pub use nested_loop_join::NestedLoopJoin;
pub use project_set::ProjectSet;
pub use projection::Projection;
pub use seq_scan::SeqScan;
pub use sort::Sort;
pub use table_function::TableFunctionSource;
pub use values::Values;
pub use window_agg::WindowAgg;

/// The DML paths build their row source from the same probe [`IndexScan`] does.
pub(crate) use index_scan::{index_probe_rows, index_probe_system_rows};
