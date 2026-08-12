use crabgresql_storage_api::Tuple;

use crate::{ExecError, ExecNode};

/// A source node that replays already-computed output rows. `RETURNING`
/// projects eagerly and streams the finished rows through this — unlike
/// [`Values`](crate::Values), which evaluates `BoundExpr`s on each pull.
pub struct MaterializedRows {
    rows: std::vec::IntoIter<Tuple>,
}

impl MaterializedRows {
    pub fn new(rows: Vec<Tuple>) -> Self {
        Self {
            rows: rows.into_iter(),
        }
    }
}

impl ExecNode for MaterializedRows {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        Ok(self.rows.next())
    }
}
