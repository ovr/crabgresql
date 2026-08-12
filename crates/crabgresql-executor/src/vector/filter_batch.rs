use arrow_array::RecordBatch;
use arrow_select::filter::filter_record_batch;

use super::{BatchNode, expr, internal};
use crate::ExecError;

/// Drops the rows of each batch that fail a predicate — the columnar
/// [`crate::Filter`].
///
/// This is the operator that makes vectorizing pay. [`Shred`](super::Shred) costs one tuple
/// build per surviving row, so filtering *below* the shred is what turns a
/// selective `WHERE` into work the row executor never does at all.
///
/// A batch that loses every row is passed on empty rather than skipped; `Shred`
/// treats an empty batch as "nothing here", not "nothing left".
pub struct FilterBatch {
    child: Box<dyn BatchNode>,
    predicate: expr::VectorPredicate,
}

impl FilterBatch {
    pub fn new(child: Box<dyn BatchNode>, predicate: expr::VectorPredicate) -> Self {
        FilterBatch { child, predicate }
    }
}

impl BatchNode for FilterBatch {
    fn next_batch(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        let Some(batch) = self.child.next_batch()? else {
            return Ok(None);
        };
        let mask = self.predicate.evaluate(&batch)?;
        // `filter_record_batch` keeps only `true`; `false` and NULL both drop,
        // which is SQL's rule for a `WHERE` and matches `predicate_holds`.
        filter_record_batch(&batch, &mask)
            .map(Some)
            .map_err(|error| internal(&format!("vectorized filter failed: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use crabgresql_binder::{BinOp, BoundExpr, UnaryOp};
    use crabgresql_storage_api::Tuple;
    use crabgresql_types::{PgType, Value};

    use crate::vector::testutil::{
        assert_same, column, columnar_filter, compare, constant, int_rows, logic, schema_of,
    };

    #[test]
    fn every_comparison_operator_agrees_with_the_row_filter() {
        let schema = schema_of(&[PgType::Int4]);
        let rows = int_rows();
        for op in [
            BinOp::Eq,
            BinOp::NotEq,
            BinOp::Lt,
            BinOp::LtEq,
            BinOp::Gt,
            BinOp::GtEq,
        ] {
            assert_same(
                &schema,
                &rows,
                &compare(
                    op,
                    PgType::Int4,
                    column(0, PgType::Int4),
                    constant(Value::Int4(1), PgType::Int4),
                ),
            );
        }
    }

    /// The Kleene case. `false AND NULL` is `false` (drop) and `true OR NULL` is
    /// `true` (keep); Arrow's plain `and`/`or` would return NULL for both, so a
    /// row that should survive an `OR` would be silently dropped.
    #[test]
    fn and_or_follow_three_valued_logic() {
        let schema = schema_of(&[PgType::Int4, PgType::Bool]);
        let rows: Vec<Tuple> = [
            (Some(1), Some(true)),
            (Some(1), Some(false)),
            (Some(1), None),
            (Some(9), Some(true)),
            (Some(9), Some(false)),
            (Some(9), None),
            (None, Some(true)),
            (None, None),
        ]
        .into_iter()
        .map(|(i, b)| {
            vec![
                i.map_or(Value::Null, Value::Int4),
                b.map_or(Value::Null, Value::Bool),
            ]
        })
        .collect();

        // `c0 = 1` is false for 9 and NULL for a NULL c0, so this covers
        // false AND NULL, NULL AND true, and every other combination.
        let eq_one = compare(
            BinOp::Eq,
            PgType::Int4,
            column(0, PgType::Int4),
            constant(Value::Int4(1), PgType::Int4),
        );
        for op in [BinOp::And, BinOp::Or] {
            assert_same(
                &schema,
                &rows,
                &logic(op, eq_one.clone(), column(1, PgType::Bool)),
            );
        }
        // NOT NULL is NULL, which drops — not "keep everything that was not true".
        assert_same(
            &schema,
            &rows,
            &BoundExpr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(column(1, PgType::Bool)),
            },
        );
    }

    #[test]
    fn is_null_and_is_not_null_agree() {
        let schema = schema_of(&[PgType::Int4]);
        let rows = int_rows();
        for negated in [false, true] {
            assert_same(
                &schema,
                &rows,
                &BoundExpr::IsNull {
                    expr: Box::new(column(0, PgType::Int4)),
                    negated,
                },
            );
        }
    }

    /// A bare boolean column is a legal `WHERE`, and its NULLs must drop.
    #[test]
    fn a_bare_boolean_column_filters() {
        let schema = schema_of(&[PgType::Bool]);
        let rows: Vec<Tuple> = [Some(true), Some(false), None, Some(true)]
            .into_iter()
            .map(|b| vec![b.map_or(Value::Null, Value::Bool)])
            .collect();
        assert_same(&schema, &rows, &column(0, PgType::Bool));
    }

    /// Comparing against a NULL constant yields NULL for every row, so everything
    /// drops — `x = NULL` is never true in SQL.
    #[test]
    fn a_null_constant_drops_every_row() {
        let schema = schema_of(&[PgType::Int4]);
        let rows = int_rows();
        let predicate = compare(
            BinOp::Eq,
            PgType::Int4,
            column(0, PgType::Int4),
            constant(Value::Null, PgType::Int4),
        );
        assert_same(&schema, &rows, &predicate);
        assert!(
            columnar_filter(&schema, &rows, &predicate)
                .expect("filter")
                .is_empty()
        );
    }

    /// A predicate with no column reference evaluates against scalars, so it
    /// produces a mask describing ONE value. Every consumer assumes a mask is as
    /// tall as the batch, and both fail quietly if it is not: `filter_record_batch`
    /// truncates the batch to the mask's length, so `WHERE 1=1` returned one row
    /// per batch instead of every row.
    ///
    /// `WHERE 1=1` is what ORMs and query builders emit for a dynamically-empty
    /// predicate, so this was reachable from ordinary SQL.
    #[test]
    fn a_constant_only_predicate_keeps_every_row() {
        let schema = schema_of(&[PgType::Int4]);
        let rows = int_rows();
        let one = || constant(Value::Int4(1), PgType::Int4);

        // `1 = 1` — true for every row.
        let always = compare(BinOp::Eq, PgType::Int4, one(), one());
        assert_same(&schema, &rows, &always);
        assert_eq!(
            columnar_filter(&schema, &rows, &always)
                .expect("filter")
                .len(),
            rows.len(),
            "a constant-true predicate must keep every row, not one per batch"
        );

        // `1 = 2` — false for every row.
        let never = compare(
            BinOp::Eq,
            PgType::Int4,
            one(),
            constant(Value::Int4(2), PgType::Int4),
        );
        assert_same(&schema, &rows, &never);
        assert!(
            columnar_filter(&schema, &rows, &never)
                .expect("filter")
                .is_empty()
        );

        // A bare `true`, and `NULL IS NULL` — the other two shapes with no column.
        assert_same(&schema, &rows, &constant(Value::Bool(true), PgType::Bool));
        assert_same(
            &schema,
            &rows,
            &BoundExpr::IsNull {
                expr: Box::new(constant(Value::Null, PgType::Int4)),
                negated: false,
            },
        );
    }

    /// The other half of the same defect: a constant beside a column gives
    /// `and_kleene` a length-N and a length-1 operand, which it rejects outright —
    /// so `WHERE id = 1 AND true` failed the query rather than answering it.
    #[test]
    fn a_constant_beside_a_column_still_compares() {
        let schema = schema_of(&[PgType::Int4]);
        let rows = int_rows();
        let eq_one = compare(
            BinOp::Eq,
            PgType::Int4,
            column(0, PgType::Int4),
            constant(Value::Int4(1), PgType::Int4),
        );
        for op in [BinOp::And, BinOp::Or] {
            for literal in [Value::Bool(true), Value::Bool(false), Value::Null] {
                assert_same(
                    &schema,
                    &rows,
                    &logic(op, eq_one.clone(), constant(literal.clone(), PgType::Bool)),
                );
            }
        }
    }

    /// A zero-row batch is where a length-1 mask is *longer* than the data, which
    /// `filter_record_batch` does reject — so the empty relation errored where the
    /// row path returned nothing.
    #[test]
    fn a_constant_predicate_over_no_rows_yields_no_rows() {
        let schema = schema_of(&[PgType::Int4]);
        let always = compare(
            BinOp::Eq,
            PgType::Int4,
            constant(Value::Int4(1), PgType::Int4),
            constant(Value::Int4(1), PgType::Int4),
        );
        assert!(
            columnar_filter(&schema, &[], &always)
                .expect("filter")
                .is_empty()
        );
    }
}
