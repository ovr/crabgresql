use crabgresql_binder::BoundExpr;
use crabgresql_storage_api::Tuple;

use crate::{ExecContext, ExecError, ExecNode, eval};

/// Evaluates one expression per output column on top of a child node.
pub struct Projection {
    child: Box<dyn ExecNode>,
    exprs: Vec<BoundExpr>,
    ctx: ExecContext,
}

impl Projection {
    pub fn new(child: Box<dyn ExecNode>, exprs: Vec<BoundExpr>, ctx: ExecContext) -> Self {
        Self { child, exprs, ctx }
    }
}

impl ExecNode for Projection {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        let Some(row) = self.child.next()? else {
            return Ok(None);
        };
        let projected = self
            .exprs
            .iter()
            .map(|expr| eval(expr, &row, &self.ctx))
            .collect::<Result<_, _>>()?;
        Ok(Some(projected))
    }
}

#[cfg(test)]
mod tests {
    use crabgresql_binder::{BinOp, BoundExpr};
    use crabgresql_storage_api::ColumnProjection;
    use crabgresql_types::{PgType, Value};

    use super::Projection;
    use crate::testutil::{binary, collect, int4, rtxn, test_table};
    use crate::{ExecContext, SeqScan};

    #[test]
    fn projection_evaluates_expressions() {
        let table = test_table();
        let exprs = vec![binary(
            BinOp::Add,
            PgType::Int4,
            BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4,
            },
            int4(10),
        )];
        let mut node = Projection::new(
            Box::new(SeqScan::new(&table, &rtxn(), &ColumnProjection::All)),
            exprs,
            ExecContext::default(),
        );
        assert_eq!(
            collect(&mut node),
            vec![
                vec![Value::Int4(11)],
                vec![Value::Int4(12)],
                vec![Value::Int4(13)],
            ]
        );
    }
}
