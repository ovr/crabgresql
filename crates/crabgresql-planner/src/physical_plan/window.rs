//! The window-function node.

use crabgresql_binder::{BoundWindowFunc, BoundWindowSpec};

use super::PhysicalPlan;

/// [`PhysicalPlan::Window`]: one step of window-function evaluation. Mirrors
/// [`WindowPlan`](crabgresql_binder::WindowPlan): the executor materializes
/// `source`, sorts it by `spec`'s partition keys then its ORDER BY keys, and
/// fills each of `funcs` into the slot it names. A window query is planned as a
/// [`PhysicalSubquery`](super::PhysicalSubquery) wrapping the chain, so the
/// standard projection/sort tail runs on top.
pub struct PhysicalWindow {
    pub source: Box<PhysicalPlan>,
    pub spec: BoundWindowSpec,
    pub funcs: Vec<BoundWindowFunc>,
    pub input_width: usize,
    pub output_width: usize,
}
