use crabgresql_binder::BoundExpr;
use crabgresql_storage_api::Tuple;

use crate::{ExecContext, ExecError, ExecNode, eval};

/// Constant rows evaluated lazily: `SELECT 1`, a FROM-less SELECT.
pub struct Values {
    rows: std::vec::IntoIter<Vec<BoundExpr>>,
    ctx: ExecContext,
}

impl Values {
    pub fn new(rows: Vec<Vec<BoundExpr>>, ctx: ExecContext) -> Self {
        Self {
            rows: rows.into_iter(),
            ctx,
        }
    }
}

impl ExecNode for Values {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        let Some(row) = self.rows.next() else {
            return Ok(None);
        };
        let tuple = row
            .iter()
            .map(|expr| eval(expr, &[], &self.ctx))
            .collect::<Result<_, _>>()?;
        Ok(Some(tuple))
    }
}
