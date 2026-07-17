//! Volcano (iterator) executor.
//!
//! Nodes: `Values`, `SeqScan`, `Filter`, `Projection`; expression evaluation
//! lives in [`eval`]. DML (INSERT/UPDATE/DELETE) runs as plain functions
//! rather than plan nodes: it yields a row count, not a row stream, and the
//! pull model only becomes the right shape for it once RETURNING exists.

pub mod eval;

use std::sync::Arc;

use crabgresql_binder::BoundExpr;
pub use crabgresql_binder::OutputColumn;
use crabgresql_planner::PhysicalPlan;
use crabgresql_storage_api::{DeleteResult, TableAm, Tid, Tuple, UpdateResult};
use crabgresql_types::Value;

use eval::eval;

/// A runtime execution error, reported to the client as `ErrorResponse`.
/// Distinct from a bind error: it can surface mid-stream, after rows of the
/// result set have already been sent.
#[derive(Debug)]
pub struct ExecError {
    /// 5-character SQLSTATE code.
    pub code: &'static str,
    pub message: String,
}

impl ExecError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// A Volcano execution node: `next()` pulls one tuple at a time.
pub trait ExecNode: Send {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError>;
}

/// The outcome of a statement: a streamable result set, or a mutation count.
pub enum Execution {
    Rows {
        columns: Vec<OutputColumn>,
        node: Box<dyn ExecNode>,
    },
    Inserted(u64),
    Updated(u64),
    Deleted(u64),
}

pub fn execute(plan: PhysicalPlan) -> Result<Execution, ExecError> {
    match plan {
        PhysicalPlan::Values {
            columns,
            rows,
            predicate,
        } => {
            let mut node: Box<dyn ExecNode> = Box::new(Values::new(rows));
            if let Some(predicate) = predicate {
                node = Box::new(Filter::new(node, predicate));
            }
            Ok(Execution::Rows { columns, node })
        }
        PhysicalPlan::Select {
            table,
            columns,
            projections,
            predicate,
        } => {
            let mut node: Box<dyn ExecNode> = Box::new(SeqScan::new(&table));
            if let Some(predicate) = predicate {
                node = Box::new(Filter::new(node, predicate));
            }
            node = Box::new(Projection::new(node, projections));
            Ok(Execution::Rows { columns, node })
        }
        PhysicalPlan::Insert { table, rows } => execute_insert(&table, &rows),
        PhysicalPlan::Update {
            table,
            predicate,
            assignments,
        } => execute_update(&table, &predicate, &assignments),
        PhysicalPlan::Delete { table, predicate } => execute_delete(&table, &predicate),
    }
}

/// Statement atomicity without a transaction engine: evaluate everything
/// first, mutate only after nothing can fail. A failure in a later row must
/// not leave earlier rows behind.
fn execute_insert(
    table: &Arc<dyn TableAm>,
    rows: &[Vec<BoundExpr>],
) -> Result<Execution, ExecError> {
    let mut tuples: Vec<Tuple> = Vec::with_capacity(rows.len());
    for row in rows {
        tuples.push(
            row.iter()
                .map(|expr| eval(expr, &[]))
                .collect::<Result<_, _>>()?,
        );
    }
    let inserted = tuples.len() as u64;
    for tuple in tuples {
        table.insert(tuple);
    }
    Ok(Execution::Inserted(inserted))
}

fn execute_update(
    table: &Arc<dyn TableAm>,
    predicate: &Option<BoundExpr>,
    assignments: &[(usize, BoundExpr)],
) -> Result<Execution, ExecError> {
    // The scan snapshot is stable, so the statement never re-visits rows it
    // wrote itself (no Halloween problem).
    let mut pending: Vec<(Tid, Tuple)> = Vec::new();
    for (tid, old) in table.scan() {
        if !predicate_holds(predicate, &old)? {
            continue;
        }
        // Every SET expression sees the OLD row: `SET a = b, b = a` swaps.
        let mut new = old.clone();
        for (index, expr) in assignments {
            new[*index] = eval(expr, &old)?;
        }
        pending.push((tid, new));
    }
    let mut updated = 0u64;
    for (tid, tuple) in pending {
        // A row deleted since the snapshot (NotFound) is simply not counted;
        // this is where M2's EvalPlanQual recheck slots in.
        if table.update(tid, tuple) == UpdateResult::Updated {
            updated += 1;
        }
    }
    Ok(Execution::Updated(updated))
}

fn execute_delete(
    table: &Arc<dyn TableAm>,
    predicate: &Option<BoundExpr>,
) -> Result<Execution, ExecError> {
    let mut pending: Vec<Tid> = Vec::new();
    for (tid, tuple) in table.scan() {
        if predicate_holds(predicate, &tuple)? {
            pending.push(tid);
        }
    }
    let mut deleted = 0u64;
    for tid in pending {
        if table.delete(tid) == DeleteResult::Deleted {
            deleted += 1;
        }
    }
    Ok(Execution::Deleted(deleted))
}

/// WHERE keeps a row only when the predicate is exactly true: false and NULL
/// both drop it.
fn predicate_holds(predicate: &Option<BoundExpr>, row: &[Value]) -> Result<bool, ExecError> {
    match predicate {
        None => Ok(true),
        Some(p) => Ok(matches!(eval(p, row)?, Value::Bool(true))),
    }
}

/// Constant rows evaluated lazily: `SELECT 1`, a FROM-less SELECT.
pub struct Values {
    rows: std::vec::IntoIter<Vec<BoundExpr>>,
}

impl Values {
    pub fn new(rows: Vec<Vec<BoundExpr>>) -> Self {
        Self {
            rows: rows.into_iter(),
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
            .map(|expr| eval(expr, &[]))
            .collect::<Result<_, _>>()?;
        Ok(Some(tuple))
    }
}

/// Full table scan through the storage API.
pub struct SeqScan {
    iter: Box<dyn Iterator<Item = Tuple> + Send>,
}

impl SeqScan {
    pub fn new(table: &Arc<dyn TableAm>) -> Self {
        Self {
            iter: Box::new(table.scan().map(|(_, tuple)| tuple)),
        }
    }
}

impl ExecNode for SeqScan {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        Ok(self.iter.next())
    }
}

/// Filters child rows by a boolean predicate (WHERE).
pub struct Filter {
    child: Box<dyn ExecNode>,
    predicate: Option<BoundExpr>,
}

impl Filter {
    pub fn new(child: Box<dyn ExecNode>, predicate: BoundExpr) -> Self {
        Self {
            child,
            predicate: Some(predicate),
        }
    }
}

impl ExecNode for Filter {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        while let Some(row) = self.child.next()? {
            if predicate_holds(&self.predicate, &row)? {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }
}

/// Evaluates one expression per output column on top of a child node.
pub struct Projection {
    child: Box<dyn ExecNode>,
    exprs: Vec<BoundExpr>,
}

impl Projection {
    pub fn new(child: Box<dyn ExecNode>, exprs: Vec<BoundExpr>) -> Self {
        Self { child, exprs }
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
            .map(|expr| eval(expr, &row))
            .collect::<Result<_, _>>()?;
        Ok(Some(projected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_binder::{BinOp, UnaryOp};
    use crabgresql_memory_storage::MemoryEngine;
    use crabgresql_storage_api::{Column, TableEngine, TableSchema};
    use crabgresql_types::PgType;
    use eval::coerce_value;

    fn int4(v: i32) -> BoundExpr {
        BoundExpr::Const {
            value: Value::Int4(v),
            ty: PgType::Int4,
        }
    }

    fn boolean(v: Option<bool>) -> BoundExpr {
        BoundExpr::Const {
            value: v.map(Value::Bool).unwrap_or(Value::Null),
            ty: PgType::Bool,
        }
    }

    fn binary(op: BinOp, arg_ty: PgType, left: BoundExpr, right: BoundExpr) -> BoundExpr {
        BoundExpr::Binary {
            op,
            arg_ty,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn eval_const(expr: &BoundExpr) -> Result<Value, ExecError> {
        eval(expr, &[])
    }

    /// (op, left, right, expected), with `None` as SQL NULL.
    type TruthTableRow = (BinOp, Option<bool>, Option<bool>, Option<bool>);

    #[test]
    fn and_or_follow_kleene_tables() {
        let cases: &[TruthTableRow] = &[
            (BinOp::And, Some(true), Some(true), Some(true)),
            (BinOp::And, Some(true), Some(false), Some(false)),
            (BinOp::And, Some(false), None, Some(false)),
            (BinOp::And, None, Some(false), Some(false)),
            (BinOp::And, None, Some(true), None),
            (BinOp::And, None, None, None),
            (BinOp::Or, Some(false), Some(false), Some(false)),
            (BinOp::Or, Some(false), Some(true), Some(true)),
            (BinOp::Or, Some(true), None, Some(true)),
            (BinOp::Or, None, Some(true), Some(true)),
            (BinOp::Or, None, Some(false), None),
            (BinOp::Or, None, None, None),
        ];
        for (op, l, r, expected) in cases {
            let expr = binary(*op, PgType::Bool, boolean(*l), boolean(*r));
            let expected = expected.map(Value::Bool).unwrap_or(Value::Null);
            assert_eq!(eval_const(&expr).unwrap(), expected, "{l:?} {op:?} {r:?}");
        }
    }

    #[test]
    fn null_operand_nulls_comparison() {
        let expr = binary(
            BinOp::Eq,
            PgType::Int4,
            int4(1),
            BoundExpr::Const {
                value: Value::Null,
                ty: PgType::Int4,
            },
        );
        assert_eq!(eval_const(&expr).unwrap(), Value::Null);
    }

    #[test]
    fn not_follows_three_valued_logic() {
        let not = |v| BoundExpr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(boolean(v)),
        };
        assert_eq!(eval_const(&not(Some(true))).unwrap(), Value::Bool(false));
        assert_eq!(eval_const(&not(None)).unwrap(), Value::Null);
    }

    #[test]
    fn is_null_is_never_null() {
        let is_null = |v: Value, negated| BoundExpr::IsNull {
            expr: Box::new(BoundExpr::Const {
                value: v,
                ty: PgType::Int4,
            }),
            negated,
        };
        assert_eq!(
            eval_const(&is_null(Value::Null, false)).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            eval_const(&is_null(Value::Int4(1), false)).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            eval_const(&is_null(Value::Null, true)).unwrap(),
            Value::Bool(false)
        );
    }

    #[test]
    fn arithmetic_overflow_is_22003() {
        let expr = binary(BinOp::Add, PgType::Int4, int4(i32::MAX), int4(1));
        let e = eval_const(&expr).unwrap_err();
        assert_eq!(e.code, "22003");
        assert_eq!(e.message, "integer out of range");

        let expr = binary(
            BinOp::Mul,
            PgType::Int8,
            BoundExpr::Const {
                value: Value::Int8(i64::MAX),
                ty: PgType::Int8,
            },
            BoundExpr::Const {
                value: Value::Int8(2),
                ty: PgType::Int8,
            },
        );
        assert_eq!(
            eval_const(&expr).unwrap_err().message,
            "bigint out of range"
        );
    }

    #[test]
    fn division_and_modulo_by_zero_are_22012() {
        for op in [BinOp::Div, BinOp::Mod] {
            let e = eval_const(&binary(op, PgType::Int4, int4(1), int4(0))).unwrap_err();
            assert_eq!(e.code, "22012");
            assert_eq!(e.message, "division by zero");
        }
    }

    #[test]
    fn min_over_minus_one_edge_cases() {
        // MIN / -1 overflows ...
        let e =
            eval_const(&binary(BinOp::Div, PgType::Int4, int4(i32::MIN), int4(-1))).unwrap_err();
        assert_eq!(e.code, "22003");
        // ... but MIN % -1 is 0, as in PG.
        assert_eq!(
            eval_const(&binary(BinOp::Mod, PgType::Int4, int4(i32::MIN), int4(-1))).unwrap(),
            Value::Int4(0)
        );
    }

    #[test]
    fn negating_min_is_22003() {
        let expr = BoundExpr::Unary {
            op: UnaryOp::Neg,
            expr: Box::new(int4(i32::MIN)),
        };
        assert_eq!(eval_const(&expr).unwrap_err().code, "22003");
    }

    #[test]
    fn text_and_bool_comparisons() {
        let text_const = |s: &str| BoundExpr::Const {
            value: Value::Text(s.into()),
            ty: PgType::Text,
        };
        let expr = binary(BinOp::Lt, PgType::Text, text_const("a"), text_const("b"));
        assert_eq!(eval_const(&expr).unwrap(), Value::Bool(true));
        // false < true
        let expr = binary(
            BinOp::Lt,
            PgType::Bool,
            boolean(Some(false)),
            boolean(Some(true)),
        );
        assert_eq!(eval_const(&expr).unwrap(), Value::Bool(true));
    }

    #[test]
    fn coerce_range_checks_int8_to_int4() {
        assert_eq!(
            coerce_value(Value::Int8(7), PgType::Int4).unwrap(),
            Value::Int4(7)
        );
        let e = coerce_value(Value::Int8(i64::MAX), PgType::Int4).unwrap_err();
        assert_eq!(e.code, "22003");
        assert_eq!(
            coerce_value(Value::Null, PgType::Int4).unwrap(),
            Value::Null
        );
    }

    fn test_table() -> Arc<dyn TableAm> {
        let engine = MemoryEngine::new();
        let table = engine
            .create_table(TableSchema {
                name: "t".into(),
                columns: vec![
                    Column {
                        name: "id".into(),
                        ty: PgType::Int4,
                    },
                    Column {
                        name: "label".into(),
                        ty: PgType::Text,
                    },
                ],
            })
            .unwrap();
        table.insert(vec![Value::Int4(1), Value::Text("one".into())]);
        table.insert(vec![Value::Int4(2), Value::Text("two".into())]);
        table.insert(vec![Value::Int4(3), Value::Null]);
        table
    }

    fn collect(node: &mut dyn ExecNode) -> Vec<Tuple> {
        let mut rows = Vec::new();
        while let Some(row) = node.next().unwrap() {
            rows.push(row);
        }
        rows
    }

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
        let mut node = Filter::new(Box::new(SeqScan::new(&table)), predicate);
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
        let mut node = Filter::new(Box::new(SeqScan::new(&table)), predicate);
        assert_eq!(collect(&mut node).len(), 2);
    }

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
        let mut node = Projection::new(Box::new(SeqScan::new(&table)), exprs);
        assert_eq!(
            collect(&mut node),
            vec![
                vec![Value::Int4(11)],
                vec![Value::Int4(12)],
                vec![Value::Int4(13)],
            ]
        );
    }

    #[test]
    fn update_evaluates_against_old_row_and_buffers() {
        let table = test_table();
        // SET id = id + 1 for every row.
        let assignments = vec![(
            0usize,
            binary(
                BinOp::Add,
                PgType::Int4,
                BoundExpr::ColumnRef {
                    index: 0,
                    ty: PgType::Int4,
                },
                int4(1),
            ),
        )];
        let Execution::Updated(n) = execute_update(&table, &None, &assignments).unwrap() else {
            panic!("expected Updated");
        };
        assert_eq!(n, 3);
        let ids: Vec<Value> = table.scan().map(|(_, t)| t[0].clone()).collect();
        assert_eq!(ids, vec![Value::Int4(2), Value::Int4(3), Value::Int4(4)]);
    }

    #[test]
    fn failing_update_mutates_nothing() {
        let table = test_table();
        // id / (id - 2) fails on the id=2 row after the id=1 row succeeded.
        let assignments = vec![(
            0usize,
            binary(
                BinOp::Div,
                PgType::Int4,
                BoundExpr::ColumnRef {
                    index: 0,
                    ty: PgType::Int4,
                },
                binary(
                    BinOp::Sub,
                    PgType::Int4,
                    BoundExpr::ColumnRef {
                        index: 0,
                        ty: PgType::Int4,
                    },
                    int4(2),
                ),
            ),
        )];
        let Err(e) = execute_update(&table, &None, &assignments) else {
            panic!("expected error");
        };
        assert_eq!(e.code, "22012");
        let ids: Vec<Value> = table.scan().map(|(_, t)| t[0].clone()).collect();
        assert_eq!(ids, vec![Value::Int4(1), Value::Int4(2), Value::Int4(3)]);
    }

    #[test]
    fn delete_with_predicate_removes_matching_rows() {
        let table = test_table();
        let predicate = Some(binary(
            BinOp::Gt,
            PgType::Int4,
            BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4,
            },
            int4(1),
        ));
        let Execution::Deleted(n) = execute_delete(&table, &predicate).unwrap() else {
            panic!("expected Deleted");
        };
        assert_eq!(n, 2);
        assert_eq!(table.scan().count(), 1);
    }
}
