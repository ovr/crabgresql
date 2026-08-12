use crabgresql_storage_api::Tuple;

use crate::{ExecError, ExecNode};

/// Concatenation of child pipelines — the executor side of a `UNION ALL` body
/// (see [`PhysicalPlan::SetOp`](crabgresql_planner::PhysicalPlan::SetOp)). Drains
/// each child fully, in order, before advancing to the next. Unlike [`Append`](crate::Append),
/// the children are already-projected `ExecNode` arms, so rows are pulled through
/// the Volcano `next()` rather than by flattening iterators. `UNION`
/// deduplication and ORDER BY belong to the `SetOp` node itself and are layered
/// on top of this one.
pub struct Concat {
    children: Vec<Box<dyn ExecNode>>,
    cursor: usize,
}

impl Concat {
    pub fn new(children: Vec<Box<dyn ExecNode>>) -> Self {
        Self {
            children,
            cursor: 0,
        }
    }
}

impl ExecNode for Concat {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        while self.cursor < self.children.len() {
            if let Some(tuple) = self.children[self.cursor].next()? {
                return Ok(Some(tuple));
            }
            self.cursor += 1;
        }
        Ok(None)
    }
}
