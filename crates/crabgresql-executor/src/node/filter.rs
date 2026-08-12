use crabgresql_binder::BoundExpr;
use crabgresql_storage_api::Tuple;

use crate::{ExecContext, ExecError, ExecNode, predicate_holds};

/// Filters child rows by a boolean predicate (WHERE).
pub struct Filter {
    child: Box<dyn ExecNode>,
    predicate: Option<BoundExpr>,
    ctx: ExecContext,
}

impl Filter {
    pub fn new(child: Box<dyn ExecNode>, predicate: BoundExpr, ctx: ExecContext) -> Self {
        Self {
            child,
            predicate: Some(predicate),
            ctx,
        }
    }
}

impl ExecNode for Filter {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        while let Some(row) = self.child.next()? {
            if predicate_holds(&self.predicate, &row, &self.ctx)? {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use crabgresql_binder::{BinOp, BoundExpr};
    use crabgresql_storage_api::ColumnProjection;
    use crabgresql_types::{PgType, Value};

    use super::Filter;
    use crate::testutil::{binary, collect, int4, rtxn, test_table};
    use crate::{ExecContext, SeqScan};

    #[test]
    fn filter_drops_false_and_null_rows() {
        let table = test_table();
        // WHERE id <> 2 — the NULL-label row still passes (predicate is on id).
        let predicate = binary(
            BinOp::NotEq,
            PgType::Int4,
            BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4,
            },
            int4(2),
        );
        let mut node = Filter::new(
            Box::new(SeqScan::new(&table, &rtxn(), &ColumnProjection::All)),
            predicate,
            ExecContext::default(),
        );
        assert_eq!(collect(&mut node).len(), 2);

        // WHERE label < 'zzz' — NULL label makes the predicate NULL: dropped.
        let predicate = binary(
            BinOp::Lt,
            PgType::Text,
            BoundExpr::ColumnRef {
                index: 1,
                ty: PgType::Text,
            },
            BoundExpr::Const {
                value: Value::Text("zzz".into()),
                ty: PgType::Text,
            },
        );
        let mut node = Filter::new(
            Box::new(SeqScan::new(&table, &rtxn(), &ColumnProjection::All)),
            predicate,
            ExecContext::default(),
        );
        assert_eq!(collect(&mut node).len(), 2);
    }
}
