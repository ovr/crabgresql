//! Volcano (iterator) executor.
//!
//! Nodes: `Values`, `SeqScan`, `Filter`, `Projection`; expression evaluation
//! lives in [`eval`]. DML (INSERT/UPDATE/DELETE) runs as plain functions
//! rather than plan nodes: it yields a row count, not a row stream, and the
//! pull model only becomes the right shape for it once RETURNING exists.

pub mod eval;
mod md5;
pub mod scalar_fns;
mod special_fns;

use std::cmp::Ordering;
use std::sync::Arc;

use crabgresql_binder::{BoundExpr, SortKey, TableFn};
pub use crabgresql_binder::OutputColumn;
use crabgresql_planner::PhysicalPlan;
use crabgresql_storage_api::{TableAm, Tid, Tuple};
use crabgresql_types::{PgType, Value};

use eval::eval;
pub use eval::{coerce_value, compare_values};

/// Session state that runtime evaluation depends on. Currently just
/// `extra_float_digits`, which controls float→text output precision.
#[derive(Clone, Copy, Debug)]
pub struct ExecContext {
    pub extra_float_digits: i32,
}

impl Default for ExecContext {
    fn default() -> Self {
        // PG's default since v12.
        Self {
            extra_float_digits: 1,
        }
    }
}

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

pub fn execute(plan: PhysicalPlan, ctx: ExecContext) -> Result<Execution, ExecError> {
    match plan {
        PhysicalPlan::Values {
            columns,
            rows,
            predicate,
            sort,
        } => {
            let mut node: Box<dyn ExecNode> = Box::new(Values::new(rows, ctx));
            if let Some(predicate) = predicate {
                node = Box::new(Filter::new(node, predicate, ctx));
            }
            node = maybe_sort(node, sort, &columns)?;
            Ok(Execution::Rows { columns, node })
        }
        PhysicalPlan::Select {
            table,
            columns,
            projections,
            predicate,
            sort,
        } => project_pipeline(
            Box::new(SeqScan::new(&table)),
            projections,
            predicate,
            sort,
            columns,
            ctx,
        ),
        PhysicalPlan::Subquery {
            source,
            columns,
            projections,
            predicate,
            sort,
        } => {
            // Stream the source's rows straight into this level's pipeline. A
            // single FROM reference needs no materialization; buffering waits
            // for multi-reference CTEs and joins.
            let Execution::Rows { node, .. } = execute(*source, ctx)? else {
                return Err(ExecError::new(
                    "XX000",
                    "subquery source did not produce a row set",
                ));
            };
            project_pipeline(node, projections, predicate, sort, columns, ctx)
        }
        PhysicalPlan::TableFunction {
            func,
            args,
            columns,
            projections,
            predicate,
            sort,
        } => project_pipeline(
            Box::new(TableFunctionSource::new(func, args, ctx)),
            projections,
            predicate,
            sort,
            columns,
            ctx,
        ),
        PhysicalPlan::Insert { table, rows } => execute_insert(&table, &rows, ctx),
        PhysicalPlan::Update {
            table,
            predicate,
            assignments,
        } => execute_update(&table, &predicate, &assignments, ctx),
        PhysicalPlan::Delete { table, predicate } => execute_delete(&table, &predicate, ctx),
    }
}

/// Wrap a source node in the standard `Filter -> Projection -> Sort` tail and
/// package it as a streamable result set. Shared by table scans, subquery
/// sources, and set-returning functions (every SELECT-shaped plan with a
/// projection list).
fn project_pipeline(
    source: Box<dyn ExecNode>,
    projections: Vec<BoundExpr>,
    predicate: Option<BoundExpr>,
    sort: Vec<SortKey>,
    columns: Vec<OutputColumn>,
    ctx: ExecContext,
) -> Result<Execution, ExecError> {
    let mut node = source;
    if let Some(predicate) = predicate {
        node = Box::new(Filter::new(node, predicate, ctx));
    }
    node = Box::new(Projection::new(node, projections, ctx));
    node = maybe_sort(node, sort, &columns)?;
    Ok(Execution::Rows { columns, node })
}

/// Statement atomicity without a transaction engine: evaluate everything
/// first, mutate only after nothing can fail. A failure in a later row must
/// not leave earlier rows behind.
fn execute_insert(
    table: &Arc<dyn TableAm>,
    rows: &[Vec<BoundExpr>],
    ctx: ExecContext,
) -> Result<Execution, ExecError> {
    let mut tuples: Vec<Tuple> = Vec::with_capacity(rows.len());
    for row in rows {
        tuples.push(
            row.iter()
                .map(|expr| eval(expr, &[], ctx))
                .collect::<Result<_, _>>()?,
        );
    }
    let inserted = tuples.len() as u64;
    for tuple in tuples {
        table.insert(tuple);
    }
    Ok(Execution::Inserted(inserted))
}

/// KNOWN M2 GAP — DML statements are not isolated from concurrent writers:
/// predicates evaluate against the scan snapshot, whole tuples are written
/// back, and a vanished row (NotFound) is skipped, not re-evaluated. Two
/// concurrent UPDATEs of one row are last-writer-wins. Row locks, MVCC and
/// the EvalPlanQual recheck that fix this arrive with crabgresql-txn in M2.
fn execute_update(
    table: &Arc<dyn TableAm>,
    predicate: &Option<BoundExpr>,
    assignments: &[(usize, BoundExpr)],
    ctx: ExecContext,
) -> Result<Execution, ExecError> {
    // The scan snapshot is stable, so the statement never re-visits rows it
    // wrote itself (no Halloween problem).
    let mut pending: Vec<(Tid, Tuple)> = Vec::new();
    for (tid, old) in table.scan() {
        if !predicate_holds(predicate, &old, ctx)? {
            continue;
        }
        // Every SET expression sees the OLD row: `SET a = b, b = a` swaps.
        let mut new = old.clone();
        for (index, expr) in assignments {
            new[*index] = eval(expr, &old, ctx)?;
        }
        pending.push((tid, new));
    }
    Ok(Execution::Updated(table.update_many(pending)))
}

/// See the concurrency note on [`execute_update`].
fn execute_delete(
    table: &Arc<dyn TableAm>,
    predicate: &Option<BoundExpr>,
    ctx: ExecContext,
) -> Result<Execution, ExecError> {
    let mut pending: Vec<Tid> = Vec::new();
    for (tid, tuple) in table.scan() {
        if predicate_holds(predicate, &tuple, ctx)? {
            pending.push(tid);
        }
    }
    Ok(Execution::Deleted(table.delete_many(pending)))
}

/// WHERE keeps a row only when the predicate is exactly true: false and NULL
/// both drop it.
fn predicate_holds(
    predicate: &Option<BoundExpr>,
    row: &[Value],
    ctx: ExecContext,
) -> Result<bool, ExecError> {
    match predicate {
        None => Ok(true),
        Some(p) => Ok(matches!(eval(p, row, ctx)?, Value::Bool(true))),
    }
}

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
            .map(|expr| eval(expr, &[], self.ctx))
            .collect::<Result<_, _>>()?;
        Ok(Some(tuple))
    }
}

/// A set-returning function in FROM position. Evaluates its arguments once and
/// emits the function's rowset. `pg_input_error_info` yields exactly one row.
pub struct TableFunctionSource {
    func: TableFn,
    args: Vec<BoundExpr>,
    ctx: ExecContext,
    done: bool,
}

impl TableFunctionSource {
    pub fn new(func: TableFn, args: Vec<BoundExpr>, ctx: ExecContext) -> Self {
        Self {
            func,
            args,
            ctx,
            done: false,
        }
    }
}

impl ExecNode for TableFunctionSource {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        let values = self
            .args
            .iter()
            .map(|expr| eval(expr, &[], self.ctx))
            .collect::<Result<Vec<_>, _>>()?;
        let tuple = match self.func {
            TableFn::PgInputErrorInfo => pg_input_error_info_row(&values),
        };
        Ok(Some(tuple))
    }
}

/// One row of `pg_input_error_info(value, type_name)`:
/// `(message, detail, hint, sql_error_code)`. A valid input (or a NULL
/// argument) yields all-NULL; an invalid one reports the message and SQLSTATE
/// (detail/hint stay NULL for the types the corpus exercises).
fn pg_input_error_info_row(args: &[Value]) -> Tuple {
    let all_null = || vec![Value::Null, Value::Null, Value::Null, Value::Null];
    let (Value::Text(value), Value::Text(type_name)) = (&args[0], &args[1]) else {
        return all_null();
    };
    match scalar_fns::soft_input(type_name, value) {
        Ok(()) => all_null(),
        Err((sqlstate, message)) => vec![
            Value::Text(message),
            Value::Null,
            Value::Null,
            Value::Text(sqlstate.to_string()),
        ],
    }
}

/// Wrap `node` in a `Sort` when there are ORDER BY keys.
fn maybe_sort(
    node: Box<dyn ExecNode>,
    sort: Vec<SortKey>,
    columns: &[OutputColumn],
) -> Result<Box<dyn ExecNode>, ExecError> {
    if sort.is_empty() {
        return Ok(node);
    }
    let types: Vec<PgType> = columns.iter().map(|c| c.ty).collect();
    Ok(Box::new(Sort::new(node, sort, types)?))
}

/// Materializing sort (ORDER BY). NULLs order per `SortKey.nulls_first`;
/// non-null values compare via the type's total order.
pub struct Sort {
    rows: std::vec::IntoIter<Tuple>,
}

impl Sort {
    pub fn new(
        mut child: Box<dyn ExecNode>,
        keys: Vec<SortKey>,
        types: Vec<PgType>,
    ) -> Result<Self, ExecError> {
        let mut rows: Vec<Tuple> = Vec::new();
        while let Some(row) = child.next()? {
            rows.push(row);
        }
        // Stable sort preserves input order for equal keys, as PG does for a
        // sort with no tiebreak.
        rows.sort_by(|a, b| {
            for key in &keys {
                let (va, vb) = (&a[key.column], &b[key.column]);
                // NULL placement follows nulls_first directly; only the value
                // comparison is reversed for DESC. (Reversing the null branch
                // too would flip NULLS FIRST/LAST for descending sorts.)
                let ord = match (matches!(va, Value::Null), matches!(vb, Value::Null)) {
                    (true, true) => Ordering::Equal,
                    (true, false) => {
                        if key.nulls_first {
                            Ordering::Less
                        } else {
                            Ordering::Greater
                        }
                    }
                    (false, true) => {
                        if key.nulls_first {
                            Ordering::Greater
                        } else {
                            Ordering::Less
                        }
                    }
                    (false, false) => {
                        let cmp = compare_values(types[key.column], va, vb);
                        if key.asc { cmp } else { cmp.reverse() }
                    }
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });
        Ok(Self {
            rows: rows.into_iter(),
        })
    }
}

impl ExecNode for Sort {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        Ok(self.rows.next())
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
            if predicate_holds(&self.predicate, &row, self.ctx)? {
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
            .map(|expr| eval(expr, &row, self.ctx))
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
        eval(expr, &[], ExecContext::default())
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
    fn case_selects_first_true_branch_lazily() {
        // CASE WHEN <cond1> THEN 10 WHEN <cond2> THEN 20 ELSE 30 END, over
        // constant conditions; false/NULL skip, only the winner is returned.
        let case = |c1: Option<bool>, c2: Option<bool>, else_: Option<BoundExpr>| BoundExpr::Case {
            whens: vec![(boolean(c1), int4(10)), (boolean(c2), int4(20))],
            else_: else_.map(Box::new),
            ty: PgType::Int4,
        };
        let e30 = || Some(int4(30));
        assert_eq!(
            eval_const(&case(Some(true), Some(true), e30())).unwrap(),
            Value::Int4(10)
        );
        assert_eq!(
            eval_const(&case(Some(false), Some(true), e30())).unwrap(),
            Value::Int4(20)
        );
        // NULL condition behaves like false: falls through to ELSE.
        assert_eq!(
            eval_const(&case(None, Some(false), e30())).unwrap(),
            Value::Int4(30)
        );
        // No branch matches and no ELSE: NULL.
        assert_eq!(
            eval_const(&case(Some(false), None, None)).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn case_does_not_evaluate_unselected_results() {
        // The losing branch divides by zero; a lazy CASE must never touch it.
        let bomb = binary(BinOp::Div, PgType::Int4, int4(1), int4(0));
        let expr = BoundExpr::Case {
            whens: vec![(boolean(Some(true)), int4(1)), (boolean(Some(true)), bomb)],
            else_: None,
            ty: PgType::Int4,
        };
        assert_eq!(eval_const(&expr).unwrap(), Value::Int4(1));
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
        let ctx = ExecContext::default();
        assert_eq!(
            coerce_value(Value::Int8(7), PgType::Int4, ctx).unwrap(),
            Value::Int4(7)
        );
        let e = coerce_value(Value::Int8(i64::MAX), PgType::Int4, ctx).unwrap_err();
        assert_eq!(e.code, "22003");
        assert_eq!(
            coerce_value(Value::Null, PgType::Int4, ctx).unwrap(),
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
        let mut node = Filter::new(Box::new(SeqScan::new(&table)), predicate, ExecContext::default());
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
        let mut node = Filter::new(Box::new(SeqScan::new(&table)), predicate, ExecContext::default());
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
        let mut node = Projection::new(Box::new(SeqScan::new(&table)), exprs, ExecContext::default());
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
        let Execution::Updated(n) = execute_update(&table, &None, &assignments, ExecContext::default()).unwrap() else {
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
        let Err(e) = execute_update(&table, &None, &assignments, ExecContext::default()) else {
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
        let Execution::Deleted(n) = execute_delete(&table, &predicate, ExecContext::default()).unwrap() else {
            panic!("expected Deleted");
        };
        assert_eq!(n, 2);
        assert_eq!(table.scan().count(), 1);
    }

    /// Parse → bind → plan → execute a query against a fresh engine.
    fn run_rows(sql: &str) -> (Vec<OutputColumn>, Vec<Tuple>) {
        run_rows_on(&(Arc::new(MemoryEngine::new()) as Arc<dyn TableEngine>), sql)
    }

    /// As [`run_rows`], but against a caller-provided engine (for queries over
    /// real tables).
    fn run_rows_on(engine: &Arc<dyn TableEngine>, sql: &str) -> (Vec<OutputColumn>, Vec<Tuple>) {
        let stmts = crabgresql_parser::parse(sql).unwrap();
        let crabgresql_parser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected a query");
        };
        let logical = crabgresql_binder::bind_query(engine, query).unwrap();
        let physical = crabgresql_planner::plan(logical);
        let Execution::Rows { columns, mut node } =
            execute(physical, ExecContext::default()).unwrap()
        else {
            panic!("expected rows");
        };
        let mut rows = Vec::new();
        while let Some(tuple) = node.next().unwrap() {
            rows.push(tuple);
        }
        (columns, rows)
    }

    #[test]
    fn pg_input_error_info_reports_range_error() {
        let (columns, rows) =
            run_rows("SELECT * FROM pg_input_error_info('1e400', 'float4')");
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["message", "detail", "hint", "sql_error_code"]);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            vec![
                Value::Text("\"1e400\" is out of range for type real".into()),
                Value::Null,
                Value::Null,
                Value::Text("22003".into()),
            ]
        );
    }

    #[test]
    fn pg_input_error_info_is_all_null_for_valid_input() {
        let (_columns, rows) =
            run_rows("SELECT * FROM pg_input_error_info('34.5', 'float4')");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], vec![Value::Null; 4]);
    }

    /// A `nums(n int4)` table seeded with 1, 2, 3.
    fn engine_with_nums() -> Arc<dyn TableEngine> {
        let engine: Arc<dyn TableEngine> = Arc::new(MemoryEngine::new());
        let table = engine
            .create_table(TableSchema {
                name: "nums".into(),
                columns: vec![Column {
                    name: "n".into(),
                    ty: PgType::Int4,
                }],
            })
            .unwrap();
        for n in [1, 2, 3] {
            table.insert(vec![Value::Int4(n)]);
        }
        engine
    }

    #[test]
    fn standalone_values_names_columns_and_keeps_rows() {
        let (columns, rows) = run_rows("VALUES (1), (2), (3)");
        assert_eq!(columns[0].name, "column1");
        assert_eq!(columns[0].ty, PgType::Int4);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1)],
                vec![Value::Int4(2)],
                vec![Value::Int4(3)],
            ]
        );
    }

    #[test]
    fn values_column_unifies_to_common_type() {
        // Mixed int4/int8 widens the whole column to int8.
        let (columns, rows) = run_rows("VALUES (1), (9000000000)");
        assert_eq!(columns[0].ty, PgType::Int8);
        assert_eq!(rows[0], vec![Value::Int8(1)]);
        assert_eq!(rows[1], vec![Value::Int8(9_000_000_000)]);
    }

    #[test]
    fn derived_table_projects_and_filters() {
        let (columns, rows) =
            run_rows("SELECT y FROM (VALUES (1, 'a'), (2, 'b')) v(x, y) WHERE x > 1");
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "y");
        assert_eq!(rows, vec![vec![Value::Text("b".into())]]);
    }

    #[test]
    fn cte_of_values_is_scannable_by_name() {
        let (columns, rows) =
            run_rows("WITH t(x) AS (VALUES (1), (2)) SELECT x FROM t WHERE x = 2");
        assert_eq!(columns[0].name, "x");
        assert_eq!(rows, vec![vec![Value::Int4(2)]]);
    }

    #[test]
    fn cte_over_table_and_ordering() {
        let engine = engine_with_nums();
        let (columns, rows) = run_rows_on(
            &engine,
            "WITH big AS (SELECT n FROM nums WHERE n >= 2) SELECT n FROM big ORDER BY 1 DESC",
        );
        assert_eq!(columns[0].name, "n");
        assert_eq!(rows, vec![vec![Value::Int4(3)], vec![Value::Int4(2)]]);
    }

    #[test]
    fn derived_table_over_real_table() {
        let engine = engine_with_nums();
        let (_columns, rows) = run_rows_on(
            &engine,
            "SELECT n FROM (SELECT n FROM nums WHERE n <> 2) s ORDER BY 1",
        );
        assert_eq!(rows, vec![vec![Value::Int4(1)], vec![Value::Int4(3)]]);
    }
}
