//! Volcano (iterator) executor.
//!
//! Nodes: `Values`, `SeqScan`, `Filter`, `Projection`; expression evaluation
//! lives in [`eval`]. DML (INSERT/UPDATE/DELETE) runs as plain functions
//! rather than plan nodes: it yields a row count, and — with `RETURNING` — a
//! row stream projected over the affected tuples the function already owns.

mod agg;
pub mod eval;
mod generate_series;
mod md5;
pub mod scalar_fns;
mod special_fns;

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

pub use crabgresql_binder::OutputColumn;
use crabgresql_binder::{
    BinOp, BoundAggregate, BoundExpr, DistinctKey, JoinKind, LogicalPlan, Returning, SortKey,
    TableFn, UnaryOp,
};
use crabgresql_planner::{
    HashKey, PhysicalAggInput, PhysicalInsertSource, PhysicalJoinExpr, PhysicalJoinInput,
    PhysicalPlan,
};
use crabgresql_storage_api::{IndexMetadata, PartitionBoundDatum, TableAm, TableSchema, Tid, Tuple};
use crabgresql_txn::TxnContext;
use crabgresql_types::{PgType, Value};

use eval::eval;
pub use eval::{coerce_value, compare_values, is_orderable};
use generate_series::Series;

/// Side-effecting sequence operations (`nextval`/`currval`/`setval`/`lastval`),
/// which the otherwise-pure expression evaluator cannot express: they mutate
/// non-transactional engine counters and per-session `currval`/`lastval` state.
/// The server supplies an implementation through [`ExecContext::sequences`]; the
/// executor calls it when it evaluates the corresponding functions.
/// The sequence functions take a possibly schema-qualified name. `namespace` is
/// the schema written by the caller (e.g. `nextval('app.s')`), or `None` when the
/// reference was unqualified — the implementation resolves `None` to its default
/// schema (`public` until a real search_path lands).
pub trait SequenceOps: Send + Sync {
    /// Advance the sequence and return its new value. Errors: 42P01 (no such
    /// sequence), 2200H (reached min/max with `NO CYCLE`).
    fn nextval(&self, namespace: Option<&str>, name: &str) -> Result<i64, ExecError>;
    /// The value `nextval` most recently returned for this sequence in this
    /// session. Errors 55000 if `nextval` has not run for it yet.
    fn currval(&self, namespace: Option<&str>, name: &str) -> Result<i64, ExecError>;
    /// Set the sequence's counter; returns `value`.
    fn setval(
        &self,
        namespace: Option<&str>,
        name: &str,
        value: i64,
        is_called: bool,
    ) -> Result<i64, ExecError>;
    /// The value the last `nextval` in this session returned, for any sequence.
    /// Errors 55000 if no `nextval` has run in the session yet.
    fn lastval(&self) -> Result<i64, ExecError>;
}

/// Session state that runtime evaluation depends on: `extra_float_digits`
/// (float→text output precision) and, when present, the handle the
/// side-effecting sequence functions dispatch through.
#[derive(Clone)]
pub struct ExecContext {
    pub extra_float_digits: i32,
    /// `None` in contexts that never call a sequence function (e.g. `EXPLAIN`'s
    /// `Values` node); a sequence function reaching a `None` context is an
    /// internal wiring error, reported as 5-char `XX000`.
    pub sequences: Option<Arc<dyn SequenceOps>>,
    /// The transaction a correlated subquery re-executes against, per outer row.
    /// Injected by [`execute`] once, at the top of the statement, and cloned into
    /// every node so `eval` can run a correlated subplan when it reaches one.
    /// `None` outside a real `execute` (a subquery marker never survives to a
    /// context without it).
    pub txn: Option<TxnContext>,
}

impl Default for ExecContext {
    fn default() -> Self {
        // PG's default since v12.
        Self {
            extra_float_digits: 1,
            sequences: None,
            txn: None,
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
    /// Optional DETAIL line (e.g. numeric field overflow explains the p/s).
    pub detail: Option<String>,
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExecError {}

impl ExecError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
        }
    }

    /// Attach a DETAIL line.
    pub fn with_detail(mut self, detail: Option<String>) -> Self {
        self.detail = detail;
        self
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
    /// A data-modifying statement with a `RETURNING` clause: the affected rows
    /// projected as a result set, plus the DML verb so the server still emits
    /// the mutation command tag (`INSERT 0 n` / `UPDATE n` / `DELETE n`) rather
    /// than `SELECT n`. RETURNING is scalar one-in/one-out (the binder rejects
    /// aggregates and set-returning functions), so the streamed row count is one
    /// per affected row. It can still exceed the count `update_many`/`delete_many`
    /// actually applied when a matched row is skipped as non-live at write time;
    /// that cross-transaction reconciliation arrives with the isolation work
    /// (see [`execute_update`]).
    ReturningRows {
        columns: Vec<OutputColumn>,
        node: Box<dyn ExecNode>,
        verb: DmlVerb,
    },
}

/// Which data-modifying statement produced a [`Execution::ReturningRows`],
/// selecting the command tag the server reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmlVerb {
    Insert,
    Update,
    Delete,
}

pub fn execute(
    mut plan: PhysicalPlan,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    // Fold every *non-correlated* subquery to a constant/comparison before any
    // node evaluates an expression. Correlated subqueries are left in place and
    // folded per outer row by `eval`, which needs the transaction to re-run their
    // subplans — so thread it through the context every node is built with (and
    // nested `run_subplan` → `execute` re-injects it, so deeper levels see it too).
    resolve_subqueries(&mut plan, ctx, txn)?;
    // Build the enriched context by copying only the fields we keep — not
    // `..ctx.clone()`, which would clone the old `txn` Snapshot (a `Vec<Xid>`)
    // only to overwrite it. One `txn.clone()` per execute, none wasted.
    let ctx = &ExecContext {
        extra_float_digits: ctx.extra_float_digits,
        sequences: ctx.sequences.clone(),
        txn: Some(txn.clone()),
    };
    match plan {
        PhysicalPlan::Values {
            columns,
            rows,
            predicate,
            sort,
            distinct,
        } => {
            // The emitted tuple width, including any hidden ORDER BY / DISTINCT ON
            // columns a FROM-less `SELECT DISTINCT ON (expr)` appended past the
            // visible output — captured before `rows` is moved so a Distinct can
            // keep those columns through the sort.
            let full_width = rows.first().map_or(columns.len(), Vec::len);
            let mut node: Box<dyn ExecNode> = Box::new(Values::new(rows, ctx.clone()));
            if let Some(predicate) = predicate {
                node = Box::new(Filter::new(node, predicate, ctx.clone()));
            }
            node = finish_sort_distinct(node, sort, distinct, full_width, &columns)?;
            Ok(Execution::Rows { columns, node })
        }
        PhysicalPlan::Select {
            table,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        } => project_pipeline(
            Box::new(SeqScan::new(&table, txn)),
            projections,
            predicate,
            sort,
            distinct,
            columns,
            ctx,
        ),
        PhysicalPlan::Append { tables, columns } => {
            // A partitioned parent read: concatenate every leaf's scan. The
            // wrapping Subquery applies this level's projection/predicate/sort.
            Ok(Execution::Rows {
                columns,
                node: Box::new(Append::new(&tables, txn)),
            })
        }
        PhysicalPlan::IndexScan {
            table,
            index_name,
            key,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        } => {
            let source = IndexScan::new(&table, &index_name, key, ctx, txn)?;
            project_pipeline(
                Box::new(source),
                projections,
                predicate,
                sort,
                distinct,
                columns,
                ctx,
            )
        }
        PhysicalPlan::Subquery {
            source,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        } => {
            // Stream the source's rows straight into this level's pipeline. A
            // single FROM reference needs no materialization; buffering waits
            // for multi-reference CTEs and joins.
            let Execution::Rows { node, .. } = execute(*source, ctx, txn)? else {
                return Err(ExecError::new(
                    "XX000",
                    "subquery source did not produce a row set",
                ));
            };
            project_pipeline(node, projections, predicate, sort, distinct, columns, ctx)
        }
        PhysicalPlan::TableFunction {
            func,
            args,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        } => project_pipeline(
            Box::new(TableFunctionSource::new(func, args, ctx.clone())),
            projections,
            predicate,
            sort,
            distinct,
            columns,
            ctx,
        ),
        PhysicalPlan::Join {
            source,
            columns,
            projections,
            predicate,
            sort,
            distinct,
        } => {
            let joined = build_join_expr(source, ctx, txn)?;
            project_pipeline(joined, projections, predicate, sort, distinct, columns, ctx)
        }
        PhysicalPlan::Limit {
            source,
            limit,
            offset,
        } => {
            let Execution::Rows { columns, node } = execute(*source, ctx, txn)? else {
                return Err(ExecError::new(
                    "XX000",
                    "LIMIT source did not produce a row set",
                ));
            };
            Ok(Execution::Rows {
                columns,
                node: Box::new(Limit::new(node, limit, offset)),
            })
        }
        PhysicalPlan::Aggregate {
            input,
            predicate,
            group_exprs,
            aggregates,
            having,
            columns,
            projections,
            sort,
            distinct,
        } => {
            // Source rows: a base table scan or the single virtual row of a
            // FROM-less aggregate.
            let source: Box<dyn ExecNode> = match input {
                PhysicalAggInput::Scan(table) => Box::new(SeqScan::new(&table, txn)),
                PhysicalAggInput::Join(source) => build_join_expr(source, ctx, txn)?,
                PhysicalAggInput::SingleRow => Box::new(Values::new(vec![vec![]], ctx.clone())),
            };
            // WHERE filters rows before aggregation.
            let mut node: Box<dyn ExecNode> = match predicate {
                Some(predicate) => Box::new(Filter::new(source, predicate, ctx.clone())),
                None => source,
            };
            node = Box::new(Aggregate::new(node, group_exprs, aggregates, ctx.clone()));
            // HAVING filters the per-group rows.
            if let Some(having) = having {
                node = Box::new(Filter::new(node, having, ctx.clone()));
            }
            // The projection list and ORDER BY were rewritten to reference the
            // aggregate output row, so the standard tail finishes the job.
            project_pipeline(node, projections, None, sort, distinct, columns, ctx)
        }
        PhysicalPlan::Insert {
            table,
            source,
            returning,
            routing,
        } => execute_insert(&table, source, returning, routing, ctx, txn),
        PhysicalPlan::Update {
            table,
            predicate,
            assignments,
            returning,
        } => execute_update(&table, &predicate, &assignments, returning, ctx, txn),
        PhysicalPlan::Delete {
            table,
            predicate,
            returning,
        } => execute_delete(&table, &predicate, returning, ctx, txn),
    }
}

/// Fold every non-correlated subquery expression in `plan` to a plain
/// [`BoundExpr`] the evaluator handles: a scalar subquery to a `Const`, `EXISTS`
/// to a boolean `Const`, and `IN (SELECT …)` to an OR-chain of equality
/// comparisons (wrapped in `NOT` when negated). Each subplan runs exactly once —
/// a non-correlated subquery does not depend on the outer row — via a recursive
/// `plan` + `execute`. Walks nested source plans so one top-level call covers the
/// whole tree; runs before any node evaluates an expression, so `eval` never sees
/// a subquery marker.
fn resolve_subqueries(
    plan: &mut PhysicalPlan,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<(), ExecError> {
    match plan {
        PhysicalPlan::Values {
            rows, predicate, ..
        } => {
            for row in rows {
                resolve_exprs(row, ctx, txn)?;
            }
            resolve_opt(predicate, ctx, txn)?;
        }
        PhysicalPlan::Select {
            projections,
            predicate,
            ..
        } => {
            resolve_exprs(projections, ctx, txn)?;
            resolve_opt(predicate, ctx, txn)?;
        }
        // An Append holds only leaf table handles — no subquery expressions.
        PhysicalPlan::Append { .. } => {}
        PhysicalPlan::IndexScan {
            key,
            projections,
            predicate,
            ..
        } => {
            for (_, value) in key.iter_mut() {
                resolve_expr(value, ctx, txn)?;
            }
            resolve_exprs(projections, ctx, txn)?;
            resolve_opt(predicate, ctx, txn)?;
        }
        PhysicalPlan::Subquery {
            source,
            projections,
            predicate,
            ..
        } => {
            resolve_subqueries(source, ctx, txn)?;
            resolve_exprs(projections, ctx, txn)?;
            resolve_opt(predicate, ctx, txn)?;
        }
        PhysicalPlan::TableFunction {
            args,
            projections,
            predicate,
            ..
        } => {
            resolve_exprs(args, ctx, txn)?;
            resolve_exprs(projections, ctx, txn)?;
            resolve_opt(predicate, ctx, txn)?;
        }
        PhysicalPlan::Join {
            source,
            projections,
            predicate,
            ..
        } => {
            resolve_join(source, ctx, txn)?;
            resolve_exprs(projections, ctx, txn)?;
            resolve_opt(predicate, ctx, txn)?;
        }
        PhysicalPlan::Aggregate {
            input,
            predicate,
            group_exprs,
            aggregates,
            having,
            projections,
            ..
        } => {
            if let PhysicalAggInput::Join(join) = input {
                resolve_join(join, ctx, txn)?;
            }
            resolve_opt(predicate, ctx, txn)?;
            resolve_exprs(group_exprs, ctx, txn)?;
            for agg in aggregates.iter_mut() {
                if let Some(arg) = &mut agg.arg {
                    resolve_expr(arg, ctx, txn)?;
                }
            }
            resolve_opt(having, ctx, txn)?;
            resolve_exprs(projections, ctx, txn)?;
        }
        PhysicalPlan::Limit { source, .. } => resolve_subqueries(source, ctx, txn)?,
        PhysicalPlan::Insert {
            source, returning, ..
        } => {
            match source {
                PhysicalInsertSource::Values(rows) => {
                    for row in rows {
                        resolve_exprs(row, ctx, txn)?;
                    }
                }
                PhysicalInsertSource::Query { input, projections } => {
                    resolve_subqueries(input, ctx, txn)?;
                    resolve_exprs(projections, ctx, txn)?;
                }
            }
            resolve_returning(returning, ctx, txn)?;
        }
        PhysicalPlan::Update {
            predicate,
            assignments,
            returning,
            ..
        } => {
            resolve_opt(predicate, ctx, txn)?;
            for (_, value) in assignments.iter_mut() {
                resolve_expr(value, ctx, txn)?;
            }
            resolve_returning(returning, ctx, txn)?;
        }
        PhysicalPlan::Delete {
            predicate,
            returning,
            ..
        } => {
            resolve_opt(predicate, ctx, txn)?;
            resolve_returning(returning, ctx, txn)?;
        }
    }
    Ok(())
}

fn resolve_exprs(
    exprs: &mut [BoundExpr],
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<(), ExecError> {
    for e in exprs {
        resolve_expr(e, ctx, txn)?;
    }
    Ok(())
}

fn resolve_opt(
    expr: &mut Option<BoundExpr>,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<(), ExecError> {
    if let Some(e) = expr {
        resolve_expr(e, ctx, txn)?;
    }
    Ok(())
}

fn resolve_returning(
    returning: &mut Option<Returning>,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<(), ExecError> {
    if let Some(r) = returning {
        resolve_exprs(&mut r.projections, ctx, txn)?;
    }
    Ok(())
}

fn resolve_join(
    join: &mut PhysicalJoinExpr,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<(), ExecError> {
    match join {
        PhysicalJoinExpr::Input { input, .. } => match input {
            PhysicalJoinInput::Scan(_) => {}
            PhysicalJoinInput::Subplan(source) => resolve_subqueries(source, ctx, txn)?,
            PhysicalJoinInput::TableFunction { args, .. } => resolve_exprs(args, ctx, txn)?,
        },
        PhysicalJoinExpr::Join {
            left,
            right,
            predicate,
            hash_keys,
            ..
        } => {
            resolve_join(left, ctx, txn)?;
            resolve_join(right, ctx, txn)?;
            resolve_opt(predicate, ctx, txn)?;
            for key in hash_keys.iter_mut() {
                resolve_expr(&mut key.left, ctx, txn)?;
                resolve_expr(&mut key.right, ctx, txn)?;
            }
        }
    }
    Ok(())
}

/// Recurse an expression tree, resolving nested subqueries bottom-up, then fold
/// this node if it is itself a subquery marker.
fn resolve_expr(expr: &mut BoundExpr, ctx: &ExecContext, txn: &TxnContext) -> Result<(), ExecError> {
    match expr {
        BoundExpr::Const { .. }
        | BoundExpr::ColumnRef { .. }
        | BoundExpr::Param { .. }
        | BoundExpr::OuterColumnRef { .. } => {}
        BoundExpr::Unary { expr, .. }
        | BoundExpr::IsNull { expr, .. }
        | BoundExpr::Coerce { expr, .. }
        | BoundExpr::Reinterpret { expr, .. } => resolve_expr(expr, ctx, txn)?,
        BoundExpr::Binary { left, right, .. } => {
            resolve_expr(left, ctx, txn)?;
            resolve_expr(right, ctx, txn)?;
        }
        BoundExpr::FuncCall { args, .. } | BoundExpr::Srf { args, .. } => {
            resolve_exprs(args, ctx, txn)?;
        }
        BoundExpr::Case { whens, else_, .. } => {
            for (cond, result) in whens.iter_mut() {
                resolve_expr(cond, ctx, txn)?;
                resolve_expr(result, ctx, txn)?;
            }
            if let Some(e) = else_ {
                resolve_expr(e, ctx, txn)?;
            }
        }
        BoundExpr::Aggregate { arg, .. } => {
            if let Some(a) = arg {
                resolve_expr(a, ctx, txn)?;
            }
        }
        // The IN needle (in `cmp`) may itself hold a subquery; fold those first.
        BoundExpr::InSubquery { cmp, .. } => resolve_expr(cmp, ctx, txn)?,
        BoundExpr::ScalarSubquery { .. } | BoundExpr::Exists { .. } => {}
    }
    // A correlated subquery cannot fold to a constant here — its value depends on
    // the outer row — so leave the marker for `eval` to fold per row. Only
    // non-correlated markers fold once, up front.
    if is_foldable_subquery(expr) {
        let taken = std::mem::replace(
            expr,
            BoundExpr::Const {
                value: Value::Null,
                ty: PgType::Bool,
            },
        );
        *expr = fold_subquery(taken, ctx, txn)?;
    }
    Ok(())
}

/// Whether `expr` is a subquery marker that can be folded before execution: one
/// whose subplan has no correlated outer reference. A correlated marker is left
/// in place for per-outer-row folding in `eval`.
fn is_foldable_subquery(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::ScalarSubquery { subplan, .. }
        | BoundExpr::Exists { subplan, .. }
        | BoundExpr::InSubquery { subplan, .. } => {
            !crabgresql_binder::plan_has_outer_refs(&subplan.0)
        }
        _ => false,
    }
}

/// Run a subquery marker's subplan once and fold it to a plain expression.
fn fold_subquery(
    expr: BoundExpr,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<BoundExpr, ExecError> {
    match expr {
        BoundExpr::ScalarSubquery { subplan, ty } => {
            let rows = run_subplan(*subplan.0, ctx, txn)?;
            Ok(BoundExpr::Const {
                value: scalar_subquery_value(rows, ty, ctx)?,
                ty,
            })
        }
        BoundExpr::Exists { subplan, negated } => {
            // EXISTS only needs to know whether a row exists, so stop at the first
            // one rather than draining the whole subplan; the binder already
            // stripped the target list to a constant so no per-row projection
            // (or its errors) is evaluated. NOT EXISTS inverts the test.
            let exists = subplan_has_rows(*subplan.0, ctx, txn)?;
            Ok(BoundExpr::Const {
                value: Value::Bool(exists != negated),
                ty: PgType::Bool,
            })
        }
        BoundExpr::InSubquery {
            subplan,
            negated,
            cmp,
        } => {
            let rows = run_subplan(*subplan.0, ctx, txn)?;
            let in_expr = build_in_chain(&cmp, rows, ctx)?;
            Ok(if negated {
                BoundExpr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(in_expr),
                }
            } else {
                in_expr
            })
        }
        // Not a subquery marker (unreachable — the caller matched one).
        other => Ok(other),
    }
}

/// The value a scalar subquery folds to from its materialized `rows`: no row →
/// NULL, one row → its single column coerced to `ty` (the type the outer
/// operator was bound against — a set-op / promoted column can be narrower), and
/// more than one row → the `21000` cardinality violation.
fn scalar_subquery_value(
    rows: Vec<Tuple>,
    ty: PgType,
    ctx: &ExecContext,
) -> Result<Value, ExecError> {
    match rows.len() {
        0 => Ok(Value::Null),
        1 => {
            let value = rows
                .into_iter()
                .next()
                .and_then(|row| row.into_iter().next())
                .unwrap_or(Value::Null);
            coerce_value(value, ty, ctx)
        }
        _ => Err(ExecError::new(
            crabgresql_pg_wire::sqlstate::CARDINALITY_VIOLATION,
            "more than one row returned by a subquery used as an expression",
        )),
    }
}

/// Evaluate a *correlated* subquery marker for one outer `row`: the per-row
/// counterpart of `fold_subquery`. The subplan is cloned, its outer references
/// filled from `row` (via `crabgresql_binder::substitute_outer`), then run and
/// folded to a value — a scalar to its single value, `EXISTS` to a bool, and
/// `IN (…)` to the outer needle's membership (evaluated against `row`). Called
/// from `eval` when it reaches a marker `resolve_subqueries` left in place, which
/// only happens under a real `execute`, so `ctx.txn` is present.
pub(crate) fn eval_correlated_subquery(
    marker: &BoundExpr,
    row: &[Value],
    ctx: &ExecContext,
) -> Result<Value, ExecError> {
    let txn = ctx.txn.as_ref().ok_or_else(|| {
        ExecError::new(
            crabgresql_pg_wire::sqlstate::INTERNAL_ERROR,
            "correlated subquery evaluated without a transaction context",
        )
    })?;
    // Every correlated marker carries a subplan; clone it and fill its outer
    // references from the current `row` once, then interpret per marker kind.
    let subplan = match marker {
        BoundExpr::ScalarSubquery { subplan, .. }
        | BoundExpr::Exists { subplan, .. }
        | BoundExpr::InSubquery { subplan, .. } => subplan,
        // `eval` only calls this for a subquery marker.
        _ => {
            return Err(ExecError::new(
                crabgresql_pg_wire::sqlstate::INTERNAL_ERROR,
                "eval_correlated_subquery called on a non-subquery expression",
            ));
        }
    };
    let mut logical = (*subplan.0).clone();
    crabgresql_binder::substitute_outer(&mut logical, row);
    match marker {
        BoundExpr::ScalarSubquery { ty, .. } => {
            let rows = run_subplan(logical, ctx, txn)?;
            scalar_subquery_value(rows, *ty, ctx)
        }
        BoundExpr::Exists { negated, .. } => {
            let exists = subplan_has_rows(logical, ctx, txn)?;
            Ok(Value::Bool(exists != *negated))
        }
        BoundExpr::InSubquery { negated, cmp, .. } => {
            let rows = run_subplan(logical, ctx, txn)?;
            // The OR-chain reuses the bound `x = <hole>` template, whose needle
            // reads the current row — so evaluate the folded chain against `row`.
            let in_expr = build_in_chain(cmp, rows, ctx)?;
            let membership = eval::eval(&in_expr, row, ctx)?;
            match (negated, membership) {
                (false, m) => Ok(m),
                // NOT IN is `NOT (x IN …)`, but Kleene: NULL stays NULL.
                (true, Value::Bool(b)) => Ok(Value::Bool(!b)),
                (true, other) => Ok(other),
            }
        }
        // Unreachable: `subplan` above already matched these three variants.
        _ => Ok(Value::Null),
    }
}

/// Plan and execute a subplan, draining its result set into materialized rows.
fn run_subplan(
    logical: LogicalPlan,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Vec<Tuple>, ExecError> {
    match execute(crabgresql_planner::plan(logical), ctx, txn)? {
        Execution::Rows { node, .. } => drain(node),
        _ => Err(ExecError::new(
            crabgresql_pg_wire::sqlstate::INTERNAL_ERROR,
            "subquery did not produce a result set",
        )),
    }
}

/// Whether a subplan yields at least one row, stopping at the first — for
/// `EXISTS`, which needs existence only, not the rows themselves.
fn subplan_has_rows(
    logical: LogicalPlan,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<bool, ExecError> {
    match execute(crabgresql_planner::plan(logical), ctx, txn)? {
        Execution::Rows { mut node, .. } => Ok(node.next()?.is_some()),
        _ => Err(ExecError::new(
            crabgresql_pg_wire::sqlstate::INTERNAL_ERROR,
            "subquery did not produce a result set",
        )),
    }
}

fn drain(mut node: Box<dyn ExecNode>) -> Result<Vec<Tuple>, ExecError> {
    let mut out = Vec::new();
    while let Some(tuple) = node.next()? {
        out.push(tuple);
    }
    Ok(out)
}

/// Fold `x IN (values)` to the OR-of-equalities PG lowers `x IN (list)` to: one
/// `x = value` comparison per candidate, combined with OR. `cmp` is the bound
/// `x = <hole>` template; each candidate value is substituted into its RHS hole,
/// preserving the operand coercions the binder resolved. The comparisons are
/// combined into a **balanced** OR tree so `eval` (and `Drop`) recurse in
/// O(log n), not O(n) — a subquery can return far more rows than a hand-written
/// IN list. An empty set folds to `false` (the caller wraps `NOT IN` in `NOT`).
fn build_in_chain(
    cmp: &BoundExpr,
    rows: Vec<Tuple>,
    ctx: &ExecContext,
) -> Result<BoundExpr, ExecError> {
    let BoundExpr::Binary {
        op,
        arg_ty,
        left,
        right,
    } = cmp
    else {
        return Err(ExecError::new(
            crabgresql_pg_wire::sqlstate::INTERNAL_ERROR,
            "IN (SELECT …) comparison template was not a binary comparison",
        ));
    };
    let mut comparisons = Vec::with_capacity(rows.len());
    for mut row in rows {
        let value = if row.is_empty() {
            Value::Null
        } else {
            row.swap_remove(0)
        };
        comparisons.push(BoundExpr::Binary {
            op: *op,
            arg_ty: *arg_ty,
            left: left.clone(),
            right: Box::new(substitute_hole(right, value, ctx)?),
        });
    }
    Ok(balanced_or(comparisons))
}

/// Combine boolean expressions into a balanced `OR` tree (depth ⌈log₂ n⌉). An
/// empty input is `false`. Kept balanced so evaluating / dropping the tree never
/// recurses linearly in the number of candidates.
fn balanced_or(mut nodes: Vec<BoundExpr>) -> BoundExpr {
    if nodes.is_empty() {
        return BoundExpr::Const {
            value: Value::Bool(false),
            ty: PgType::Bool,
        };
    }
    while nodes.len() > 1 {
        let mut merged = Vec::with_capacity(nodes.len().div_ceil(2));
        let mut it = nodes.into_iter();
        while let Some(a) = it.next() {
            match it.next() {
                Some(b) => merged.push(BoundExpr::Binary {
                    op: BinOp::Or,
                    arg_ty: PgType::Bool,
                    left: Box::new(a),
                    right: Box::new(b),
                }),
                None => merged.push(a),
            }
        }
        nodes = merged;
    }
    // Exactly one node remains (input was non-empty).
    nodes.swap_remove(0)
}

/// Substitute a candidate value into the IN comparison template's `<hole>` — the
/// unique NULL `Const` the binder placed on the RHS — and coerce it to that
/// hole's declared type. The hole may sit under a `Coerce`, a `Reinterpret`, or
/// a coercion `FuncCall` (e.g. `bpchar → text` lowers to `FuncCall{BpcharToText}`
/// rather than a `Coerce`), so descend through all of them; only the NULL
/// placeholder is replaced, leaving any constant function arguments (typmods)
/// intact.
fn substitute_hole(
    expr: &BoundExpr,
    value: Value,
    ctx: &ExecContext,
) -> Result<BoundExpr, ExecError> {
    match expr {
        // The placeholder: a NULL `Const` carrying the hole's declared type.
        BoundExpr::Const {
            value: Value::Null,
            ty,
        } => Ok(BoundExpr::Const {
            value: coerce_value(value, *ty, ctx)?,
            ty: *ty,
        }),
        // Any other constant (e.g. a coercion function's typmod argument) stays.
        BoundExpr::Const { .. } => Ok(expr.clone()),
        BoundExpr::Coerce { expr, ty } => Ok(BoundExpr::Coerce {
            expr: Box::new(substitute_hole(expr, value, ctx)?),
            ty: *ty,
        }),
        BoundExpr::Reinterpret {
            expr,
            reported,
            rep,
        } => Ok(BoundExpr::Reinterpret {
            expr: Box::new(substitute_hole(expr, value, ctx)?),
            reported: *reported,
            rep: *rep,
        }),
        BoundExpr::FuncCall { func, ret, args } => {
            let args = args
                .iter()
                .map(|a| substitute_hole(a, value.clone(), ctx))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(BoundExpr::FuncCall {
                func: *func,
                ret: *ret,
                args,
            })
        }
        other => Ok(other.clone()),
    }
}

/// Wrap a source node in the standard `Filter -> Projection -> Sort` tail and
/// package it as a streamable result set. Shared by table scans, subquery
/// sources, and set-returning functions (every SELECT-shaped plan with a
/// projection list).
#[allow(clippy::too_many_arguments)]
fn project_pipeline(
    source: Box<dyn ExecNode>,
    projections: Vec<BoundExpr>,
    predicate: Option<BoundExpr>,
    sort: Vec<SortKey>,
    distinct: Option<Vec<DistinctKey>>,
    columns: Vec<OutputColumn>,
    ctx: &ExecContext,
) -> Result<Execution, ExecError> {
    let mut node = source;
    if let Some(predicate) = predicate {
        node = Box::new(Filter::new(node, predicate, ctx.clone()));
    }
    // The projected tuple width, including any hidden ORDER BY / DISTINCT ON
    // columns appended past the visible output — captured before `projections`
    // is consumed so a Distinct can keep those columns through the sort.
    let full_width = projections.len();
    // A set-returning function in the target list turns one input row into many,
    // so it needs `ProjectSet` rather than the one-in/one-out `Projection`.
    node = if projections.iter().any(BoundExpr::is_srf) {
        Box::new(ProjectSet::new(node, projections, ctx.clone()))
    } else {
        Box::new(Projection::new(node, projections, ctx.clone()))
    };
    node = finish_sort_distinct(node, sort, distinct, full_width, &columns)?;
    Ok(Execution::Rows { columns, node })
}

/// Apply the ORDER BY and DISTINCT tail. Without DISTINCT this is just the sort
/// (which trims hidden columns). With DISTINCT the sort must run first but keep
/// its hidden columns, so it is built with a no-op width and the `Distinct` node
/// performs the final trim to the visible output width.
fn finish_sort_distinct(
    node: Box<dyn ExecNode>,
    sort: Vec<SortKey>,
    distinct: Option<Vec<DistinctKey>>,
    full_width: usize,
    columns: &[OutputColumn],
) -> Result<Box<dyn ExecNode>, ExecError> {
    let Some(keys) = distinct else {
        return maybe_sort(node, sort, columns);
    };
    let mut node = node;
    if !sort.is_empty() {
        node = Box::new(Sort::new(node, sort, full_width)?);
    }
    Ok(Box::new(Distinct::new(node, keys, columns.len())?))
}

/// A source node that replays already-computed output rows. `RETURNING`
/// projects eagerly and streams the finished rows through this — unlike
/// [`Values`], which evaluates `BoundExpr`s on each pull.
struct MaterializedRows {
    rows: std::vec::IntoIter<Tuple>,
}

impl MaterializedRows {
    fn new(rows: Vec<Tuple>) -> Self {
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

/// Project a bound `RETURNING` list over the rows a DML statement affects,
/// eagerly. The projection must run *before* the caller commits: a faulting
/// RETURNING expression (division by zero, a failed cast) then propagates out of
/// `execute` and aborts the statement, rolling the mutation back — matching
/// PostgreSQL, and unlike a lazy node that would fault mid-stream after the
/// write already committed. RETURNING is scalar one-in/one-out (the binder
/// rejects aggregates and set-returning functions), so this is one output row
/// per affected row.
fn project_returning<'a>(
    affected: impl IntoIterator<Item = &'a Tuple>,
    projections: &[BoundExpr],
    ctx: &ExecContext,
) -> Result<Vec<Tuple>, ExecError> {
    affected
        .into_iter()
        .map(|row| projections.iter().map(|expr| eval(expr, row, ctx)).collect())
        .collect()
}

/// Package eagerly-projected `RETURNING` output rows as a streamable result.
fn returning_rows(output: Vec<Tuple>, columns: Vec<OutputColumn>, verb: DmlVerb) -> Execution {
    Execution::ReturningRows {
        columns,
        node: Box::new(MaterializedRows::new(output)),
        verb,
    }
}

/// Statement atomicity: evaluate everything first, mutate only after nothing
/// can fail, so a failure in a later row leaves no earlier rows behind. The
/// writes are stamped with `txn`'s XID and become durable/visible only when the
/// transaction commits.
fn execute_insert(
    table: &Arc<dyn TableAm>,
    source: PhysicalInsertSource,
    returning: Option<Returning>,
    routing: Option<Vec<Arc<dyn TableAm>>>,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    // Materialize every source tuple before any write. Draining a query source
    // to completion first is what makes `INSERT INTO t SELECT ... FROM t` read
    // only pre-insert rows (no Halloween problem), and lets validation/routing
    // see a stable set so the statement stays all-or-nothing.
    let tuples = collect_insert_tuples(source, ctx, txn)?;
    match routing {
        // Partitioned parent: route each row to the leaf whose RANGE bound admits
        // its key and write there.
        Some(leaves) => insert_routed(table, tuples, returning, &leaves, ctx, txn),
        // Ordinary table: rows go straight to `table`.
        None => insert_direct(table, tuples, returning, ctx, txn),
    }
}

/// Evaluate an INSERT's source into fully-formed, schema-order tuples. No
/// validation or writing happens here; the caller does both after the whole
/// source is consumed.
fn collect_insert_tuples(
    source: PhysicalInsertSource,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Vec<Tuple>, ExecError> {
    let mut tuples: Vec<Tuple> = Vec::new();
    match source {
        PhysicalInsertSource::Values(rows) => {
            for row in &rows {
                tuples.push(
                    row.iter()
                        .map(|expr| eval(expr, &[], ctx))
                        .collect::<Result<_, _>>()?,
                );
            }
        }
        PhysicalInsertSource::Query { input, projections } => {
            let Execution::Rows { mut node, .. } = execute(*input, ctx, txn)? else {
                return Err(ExecError::new(
                    "XX000",
                    "insert source did not produce a row set",
                ));
            };
            while let Some(row) = node.next()? {
                tuples.push(
                    projections
                        .iter()
                        .map(|expr| eval(expr, &row, ctx))
                        .collect::<Result<_, _>>()?,
                );
            }
        }
    }
    Ok(tuples)
}

/// Constraint-check and write every tuple to a single table (the non-partitioned
/// path). Each row is validated against the pre-existing rows plus the earlier
/// rows of this statement, so a duplicate within one INSERT is caught.
fn insert_direct(
    table: &Arc<dyn TableAm>,
    tuples: Vec<Tuple>,
    returning: Option<Returning>,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    // Existing rows are only consulted to enforce UNIQUE keys; a table with no
    // unique index never needs the scan (NOT NULL checks only the new row).
    let has_unique = table.indexes().iter().any(|index| index.unique);
    let mut visible: Vec<Tuple> = if has_unique {
        table.scan(txn).map(|(_, tuple)| tuple).collect()
    } else {
        Vec::new()
    };
    for tuple in &tuples {
        validate_constraints(table, tuple, visible.iter(), ctx)?;
        if has_unique {
            visible.push(tuple.clone());
        }
    }
    let inserted = tuples.len() as u64;
    // RETURNING sees the fully-formed row (defaults filled in), in schema order.
    // Project before inserting so a faulting RETURNING expression aborts the
    // statement (nothing written); the tuples then move into `insert` uncloned.
    let output = match &returning {
        Some(returning) => Some(project_returning(&tuples, &returning.projections, ctx)?),
        None => None,
    };
    for tuple in tuples {
        table.insert(tuple, txn);
    }
    finish_insert(returning, output, inserted)
}

/// Route each tuple to the leaf partition of `parent` that admits its key, then
/// validate and write. Each row is processed in order — routed, then validated
/// against its destination leaf — so a routing failure (23514) and a constraint
/// failure (23502/23505) are reported in the same order PostgreSQL would (it
/// routes then checks constraints, row by row), and a NOT NULL / unique violation
/// names the destination partition. A leaf is an ordinary heap and may carry a
/// UNIQUE index, so uniqueness is enforced against the destination leaf's
/// pre-existing rows plus earlier same-statement rows routed to it, exactly as
/// [`insert_direct`] does for a plain table. All checks run before any write, so
/// the statement stays all-or-nothing.
fn insert_routed(
    parent: &Arc<dyn TableAm>,
    tuples: Vec<Tuple>,
    returning: Option<Returning>,
    leaves: &[Arc<dyn TableAm>],
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    let parent_schema = parent.schema();
    // Per-leaf snapshot of the rows a UNIQUE check must see, built lazily the
    // first time a row routes to a leaf that has a unique index (leaves without
    // one, the common case, never get scanned). `None` = not yet scanned.
    let mut visible: Vec<Option<Vec<Tuple>>> = vec![None; leaves.len()];
    let mut routes: Vec<usize> = Vec::with_capacity(tuples.len());
    for tuple in &tuples {
        let leaf = route_tuple(parent_schema, leaves, tuple, ctx)?;
        let has_unique = leaves[leaf].indexes().iter().any(|index| index.unique);
        if has_unique && visible[leaf].is_none() {
            visible[leaf] = Some(leaves[leaf].scan(txn).map(|(_, tuple)| tuple).collect());
        }
        match visible[leaf].as_deref() {
            Some(seen) => validate_constraints(&leaves[leaf], tuple, seen.iter(), ctx)?,
            None => validate_constraints(&leaves[leaf], tuple, std::iter::empty(), ctx)?,
        }
        if let Some(seen) = visible[leaf].as_mut() {
            seen.push(tuple.clone());
        }
        routes.push(leaf);
    }
    let inserted = tuples.len() as u64;
    let output = match &returning {
        Some(returning) => Some(project_returning(&tuples, &returning.projections, ctx)?),
        None => None,
    };
    for (tuple, leaf) in tuples.into_iter().zip(routes) {
        leaves[leaf].insert(tuple, txn);
    }
    finish_insert(returning, output, inserted)
}

/// Shared tail of the INSERT paths: emit RETURNING rows or the inserted count.
fn finish_insert(
    returning: Option<Returning>,
    output: Option<Vec<Tuple>>,
    inserted: u64,
) -> Result<Execution, ExecError> {
    match (returning, output) {
        (Some(returning), Some(output)) => {
            Ok(returning_rows(output, returning.columns, DmlVerb::Insert))
        }
        _ => Ok(Execution::Inserted(inserted)),
    }
}

/// Pick the leaf partition of `parent` whose RANGE bound admits `tuple`'s
/// partition key, returning its index in `leaves`. A NULL key — which no range
/// partition accepts — or a key outside every leaf's bound is rejected with
/// `23514`, matching PostgreSQL's `no partition of relation … found for row`.
fn route_tuple(
    parent: &TableSchema,
    leaves: &[Arc<dyn TableAm>],
    tuple: &Tuple,
    ctx: &ExecContext,
) -> Result<usize, ExecError> {
    // The RANGE-admits rule lives in `leaf_admits` (shared with the leaf-bound
    // check), so a routed row lands in exactly the leaf a direct INSERT would.
    for (idx, leaf) in leaves.iter().enumerate() {
        if leaf_admits(leaf.schema(), tuple) {
            return Ok(idx);
        }
    }
    // No leaf admits the key (or it is NULL, which no range partition accepts):
    // PostgreSQL's tuple-routing failure. The DETAIL clips each field to 64 bytes
    // exactly as `display_tuple` does, so a long key reads byte-identically to PG.
    let scheme = parent
        .partition_scheme
        .as_ref()
        .expect("routing target is a partitioned parent");
    let col = scheme.key_columns[0];
    Err(ExecError::new(
        "23514",
        format!("no partition of relation \"{}\" found for row", parent.name),
    )
    .with_detail(Some(format!(
        "Partition key of the failing row contains ({}) = ({}).",
        parent.columns[col].name,
        clip_failing_row_field(display_value(&tuple[col], ctx)),
    ))))
}

/// The scan sees `txn`'s snapshot, and the new versions it writes carry `txn`'s
/// command id, so the statement never re-visits rows it wrote itself (no
/// Halloween problem). A row that vanished under us (`NotFound`) is skipped, not
/// counted. Cross-transaction write-write conflicts still resolve last-writer-
/// wins here — EvalPlanQual (READ COMMITTED) and the 40001 abort (REPEATABLE
/// READ) that make this correct arrive with the isolation work (P6).
fn execute_update(
    table: &Arc<dyn TableAm>,
    predicate: &Option<BoundExpr>,
    assignments: &[(usize, BoundExpr)],
    returning: Option<Returning>,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    let original: Vec<(Tid, Tuple)> = table.scan(txn).collect();
    // `simulated` mirrors the post-update table so a UNIQUE check sees other
    // rows' new values; it is only needed when a unique index exists.
    let has_unique = table.indexes().iter().any(|index| index.unique);
    let mut simulated = if has_unique {
        original.clone()
    } else {
        Vec::new()
    };
    let mut pending: Vec<(Tid, Tuple)> = Vec::new();
    for (tid, old) in original {
        if !predicate_holds(predicate, &old, ctx)? {
            continue;
        }
        // Every SET expression sees the OLD row: `SET a = b, b = a` swaps.
        let mut new = old.clone();
        for (index, expr) in assignments {
            new[*index] = eval(expr, &old, ctx)?;
        }
        if has_unique {
            let Some(pos) = simulated
                .iter()
                .position(|(candidate, _)| *candidate == tid)
            else {
                continue;
            };
            let (_, removed) = simulated.remove(pos);
            if let Err(error) =
                validate_constraints(table, &new, simulated.iter().map(|(_, t)| t), ctx)
            {
                simulated.insert(pos, (tid, removed));
                return Err(error);
            }
            simulated.insert(pos, (tid, new.clone()));
        } else {
            validate_constraints(table, &new, std::iter::empty(), ctx)?;
        }
        pending.push((tid, new));
    }
    // With RETURNING, project the NEW rows (in schema order) before
    // `update_many` consumes `pending`, so a faulting expression aborts the
    // statement before any row is written.
    match returning {
        Some(returning) => {
            let output =
                project_returning(pending.iter().map(|(_, new)| new), &returning.projections, ctx)?;
            table.update_many(pending, txn);
            Ok(returning_rows(output, returning.columns, DmlVerb::Update))
        }
        None => Ok(Execution::Updated(table.update_many(pending, txn))),
    }
}

fn validate_constraints<'a>(
    table: &Arc<dyn TableAm>,
    tuple: &Tuple,
    existing: impl Iterator<Item = &'a Tuple>,
    ctx: &ExecContext,
) -> Result<(), ExecError> {
    let schema = table.schema();
    for (column, value) in schema.columns.iter().zip(tuple) {
        if !column.nullable && matches!(value, Value::Null) {
            return Err(ExecError::new(
                "23502",
                format!(
                    "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
                    column.name, schema.name
                ),
            )
            .with_detail(Some(format!(
                "Failing row contains ({}).",
                display_tuple(tuple, ctx)
            ))));
        }
    }

    // Order matches PostgreSQL's observable behavior: a not-null violation (above)
    // is reported before a partition-constraint violation, which is reported
    // before a unique-key violation (checked below).
    check_partition_bound(schema, tuple, ctx)?;

    let existing: Vec<&Tuple> = existing.collect();
    for index in table.indexes().iter().filter(|index| index.unique) {
        if unique_key_skipped(index, tuple) {
            continue;
        }
        if existing
            .iter()
            .any(|other| unique_keys_equal(schema, index, tuple, other))
        {
            let names = index
                .keys
                .iter()
                .map(|key| schema.columns[key.column].name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let values = index
                .keys
                .iter()
                .map(|key| display_value(&tuple[key.column], ctx))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ExecError::new(
                "23505",
                format!(
                    "duplicate key value violates unique constraint \"{}\"",
                    index.name
                ),
            )
            .with_detail(Some(format!("Key ({names})=({values}) already exists."))));
        }
    }
    Ok(())
}

/// Enforce a leaf partition's RANGE bound against a fully-formed row. A key
/// value outside `[from, to)` — or a NULL key, which no range partition admits —
/// is rejected with 23514, matching PostgreSQL's observable behavior (error text
/// and SQLSTATE) for a direct INSERT/UPDATE into a partition. A non-partition
/// relation (`None`) passes; the partitioned parent, which would route the row,
/// is rejected in the binder before it ever reaches here.
fn check_partition_bound(
    schema: &TableSchema,
    tuple: &Tuple,
    ctx: &ExecContext,
) -> Result<(), ExecError> {
    // A non-partition relation has no bound to enforce; a leaf whose bound admits
    // the row passes. Only the out-of-range case falls through to the error.
    if schema.partition_of.is_none() || leaf_admits(schema, tuple) {
        return Ok(());
    }
    Err(ExecError::new(
        "23514",
        format!(
            "new row for relation \"{}\" violates partition constraint",
            schema.name
        ),
    )
    .with_detail(Some(format!(
        "Failing row contains ({}).",
        display_tuple(tuple, ctx)
    ))))
}

/// The inclusive lower bound admits `key` when `key >= from` (or the bound is
/// `MINVALUE`). `MAXVALUE` as a lower bound admits nothing.
fn lower_admits(from: &PartitionBoundDatum, ty: PgType, key: &Value) -> bool {
    match from {
        PartitionBoundDatum::MinValue => true,
        PartitionBoundDatum::MaxValue => false,
        PartitionBoundDatum::Value(v) => compare_values(ty, key, v) != Ordering::Less,
    }
}

/// The exclusive upper bound admits `key` when `key < to` (or the bound is
/// `MAXVALUE`). `MINVALUE` as an upper bound admits nothing.
fn upper_admits(to: &PartitionBoundDatum, ty: PgType, key: &Value) -> bool {
    match to {
        PartitionBoundDatum::MaxValue => true,
        PartitionBoundDatum::MinValue => false,
        PartitionBoundDatum::Value(v) => compare_values(ty, key, v) == Ordering::Less,
    }
}

/// Whether the leaf partition described by `leaf`'s schema admits `tuple`'s RANGE
/// key: a non-NULL key inside `[from, to)`. Single-column key (DDL-enforced); a
/// NULL key is admitted by no range partition. This is the one place the RANGE
/// "does this leaf admit this row" rule is composed — shared by leaf-bound
/// enforcement ([`check_partition_bound`]) and parent tuple routing
/// ([`route_tuple`]) so the two never disagree about which leaf a row belongs to.
/// Panics if `leaf` is not a partition leaf; both callers guarantee it.
fn leaf_admits(leaf: &TableSchema, tuple: &Tuple) -> bool {
    let part = leaf
        .partition_of
        .as_ref()
        .expect("leaf_admits called on a partition leaf");
    let col = part.key_columns[0];
    let ty = leaf.columns[col].ty;
    let key = &tuple[col];
    !matches!(key, Value::Null)
        && lower_admits(&part.bound.from[0], ty, key)
        && upper_admits(&part.bound.to[0], ty, key)
}

fn unique_key_skipped(index: &IndexMetadata, tuple: &Tuple) -> bool {
    index.nulls_distinct
        && index
            .keys
            .iter()
            .any(|key| matches!(tuple[key.column], Value::Null))
}

fn unique_keys_equal(
    schema: &crabgresql_storage_api::TableSchema,
    index: &IndexMetadata,
    left: &Tuple,
    right: &Tuple,
) -> bool {
    let tys = index
        .keys
        .iter()
        .map(|key| schema.columns[key.column].ty)
        .collect::<Vec<_>>();
    let left = index
        .keys
        .iter()
        .map(|key| left[key.column].clone())
        .collect::<Vec<_>>();
    let right = index
        .keys
        .iter()
        .map(|key| right[key.column].clone())
        .collect::<Vec<_>>();
    agg::keys_equal(&tys, &left, &right)
}

fn display_value(value: &Value, ctx: &ExecContext) -> String {
    value
        .encode_text_with(ctx.extra_float_digits)
        .unwrap_or_else(|| "null".to_string())
}

/// PostgreSQL renders each column of a "Failing row contains (...)" DETAIL with a
/// 64-byte field limit: a longer value is clipped on a character boundary and
/// `...` appended. Match that so the DETAIL stays byte-identical to PG's.
const FAILING_ROW_FIELD_MAXLEN: usize = 64;

fn clip_failing_row_field(mut s: String) -> String {
    if s.len() <= FAILING_ROW_FIELD_MAXLEN {
        return s;
    }
    let mut end = FAILING_ROW_FIELD_MAXLEN;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push_str("...");
    s
}

fn display_tuple(tuple: &Tuple, ctx: &ExecContext) -> String {
    tuple
        .iter()
        .map(|value| clip_failing_row_field(display_value(value, ctx)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// See the concurrency note on [`execute_update`].
fn execute_delete(
    table: &Arc<dyn TableAm>,
    predicate: &Option<BoundExpr>,
    returning: Option<Returning>,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    let mut pending: Vec<Tid> = Vec::new();
    // RETURNING sees the deleted (OLD) rows; capture them alongside the tids.
    let mut deleted: Vec<Tuple> = Vec::new();
    for (tid, tuple) in table.scan(txn) {
        if predicate_holds(predicate, &tuple, ctx)? {
            pending.push(tid);
            if returning.is_some() {
                deleted.push(tuple);
            }
        }
    }
    // Project the OLD rows before `delete_many` so a faulting RETURNING
    // expression aborts the statement before any row is removed.
    match returning {
        Some(returning) => {
            let output = project_returning(deleted.iter(), &returning.projections, ctx)?;
            table.delete_many(pending, txn);
            Ok(returning_rows(output, returning.columns, DmlVerb::Delete))
        }
        None => Ok(Execution::Deleted(table.delete_many(pending, txn))),
    }
}

/// WHERE keeps a row only when the predicate is exactly true: false and NULL
/// both drop it.
fn predicate_holds(
    predicate: &Option<BoundExpr>,
    row: &[Value],
    ctx: &ExecContext,
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
            .map(|expr| eval(expr, &[], &self.ctx))
            .collect::<Result<_, _>>()?;
        Ok(Some(tuple))
    }
}

/// A set-returning function in FROM position. Evaluates its arguments once (on
/// the first pull) and streams the function's rowset. `pg_input_error_info`
/// yields exactly one row; `generate_series` yields one row per value.
pub struct TableFunctionSource {
    func: TableFn,
    args: Vec<BoundExpr>,
    ctx: ExecContext,
    /// Iteration state, initialized lazily from the evaluated arguments.
    state: Option<TableFnState>,
}

enum TableFnState {
    /// `pg_input_error_info`: a single pending row, then exhausted.
    Single(Option<Tuple>),
    /// `generate_series`: a lazy integer range.
    Series(Series),
}

impl TableFunctionSource {
    pub fn new(func: TableFn, args: Vec<BoundExpr>, ctx: ExecContext) -> Self {
        Self {
            func,
            args,
            ctx,
            state: None,
        }
    }

    /// Evaluate the (constant) arguments once and build the iteration state.
    fn init(&mut self) -> Result<&mut TableFnState, ExecError> {
        if self.state.is_none() {
            let values = self
                .args
                .iter()
                .map(|expr| eval(expr, &[], &self.ctx))
                .collect::<Result<Vec<_>, _>>()?;
            self.state = Some(match self.func {
                TableFn::PgInputErrorInfo => {
                    TableFnState::Single(Some(pg_input_error_info_row(&values)))
                }
                TableFn::GenerateSeries(elem) => {
                    TableFnState::Series(Series::from_args(elem, &values)?)
                }
                TableFn::JsonbPathQuery => {
                    TableFnState::Series(jsonb_path_query_series(&values)?)
                }
            });
        }
        match self.state.as_mut() {
            Some(state) => Ok(state),
            None => panic!("table-function state was not initialized"),
        }
    }
}

impl ExecNode for TableFunctionSource {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        match self.init()? {
            TableFnState::Single(row) => Ok(row.take()),
            TableFnState::Series(series) => Ok(series.next_value()?.map(|v| vec![v])),
        }
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

/// Build the row source for one join input: a table scan, a set-returning
/// function, or a recursively-executed subplan (derived table / CTE / VALUES).
fn build_join_source(
    input: PhysicalJoinInput,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Box<dyn ExecNode>, ExecError> {
    Ok(match input {
        PhysicalJoinInput::Scan(table) => Box::new(SeqScan::new(&table, txn)),
        PhysicalJoinInput::TableFunction { func, args } => {
            Box::new(TableFunctionSource::new(func, args, ctx.clone()))
        }
        PhysicalJoinInput::Subplan(source) => {
            let Execution::Rows { node, .. } = execute(*source, ctx, txn)? else {
                return Err(ExecError::new(
                    "XX000",
                    "join source did not produce a row set",
                ));
            };
            node
        }
    })
}

/// Recursively build a physical join tree. Leaf construction is shared with
/// standalone subquery/table-function sources; each binary node streams its
/// left side and materializes its right side.
fn build_join_expr(
    source: PhysicalJoinExpr,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Box<dyn ExecNode>, ExecError> {
    match source {
        PhysicalJoinExpr::Input { input, .. } => build_join_source(input, ctx, txn),
        PhysicalJoinExpr::Join {
            left,
            right,
            kind,
            predicate,
            hash_keys,
        } => {
            let left_width = left.width();
            let right_width = right.width();
            let left = build_join_expr(*left, ctx, txn)?;
            let right = build_join_expr(*right, ctx, txn)?;
            if hash_keys.is_empty() {
                Ok(Box::new(NestedLoopJoin::new(
                    left,
                    right,
                    left_width,
                    right_width,
                    kind,
                    predicate,
                    ctx.clone(),
                )?))
            } else {
                Ok(Box::new(HashJoin::new(
                    left,
                    right,
                    left_width,
                    right_width,
                    kind,
                    hash_keys,
                    predicate,
                    ctx.clone(),
                )?))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JoinPhase {
    LeftRows,
    UnmatchedRight,
    Done,
}

/// Binary nested-loop join with right-side materialization. For right/full
/// joins, `right_matched` records which materialized rows participated in at
/// least one match so they can be null-extended after the left stream ends.
pub struct NestedLoopJoin {
    left: Box<dyn ExecNode>,
    right_rows: Vec<Tuple>,
    right_matched: Vec<bool>,
    left_width: usize,
    right_width: usize,
    kind: JoinKind,
    predicate: Option<BoundExpr>,
    ctx: ExecContext,
    phase: JoinPhase,
    current_left: Option<Tuple>,
    current_left_matched: bool,
    right_index: usize,
}

impl NestedLoopJoin {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        left: Box<dyn ExecNode>,
        mut right: Box<dyn ExecNode>,
        left_width: usize,
        right_width: usize,
        kind: JoinKind,
        predicate: Option<BoundExpr>,
        ctx: ExecContext,
    ) -> Result<Self, ExecError> {
        let mut right_rows = Vec::new();
        while let Some(row) = right.next()? {
            right_rows.push(row);
        }
        let right_matched = vec![false; right_rows.len()];
        Ok(Self {
            left,
            right_rows,
            right_matched,
            left_width,
            right_width,
            kind,
            predicate,
            ctx,
            phase: JoinPhase::LeftRows,
            current_left: None,
            current_left_matched: false,
            right_index: 0,
        })
    }

    fn preserves_left(&self) -> bool {
        matches!(self.kind, JoinKind::Left | JoinKind::Full)
    }

    fn preserves_right(&self) -> bool {
        matches!(self.kind, JoinKind::Right | JoinKind::Full)
    }

    fn combined_row(&self, left: &[Value], right: &[Value]) -> Tuple {
        let mut row = Vec::with_capacity(self.left_width + self.right_width);
        row.extend_from_slice(left);
        row.extend_from_slice(right);
        row
    }
}

impl ExecNode for NestedLoopJoin {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        loop {
            match self.phase {
                JoinPhase::LeftRows => {
                    if self.current_left.is_none() {
                        self.current_left = self.left.next()?;
                        let Some(_) = self.current_left else {
                            self.phase = if self.preserves_right() {
                                JoinPhase::UnmatchedRight
                            } else {
                                JoinPhase::Done
                            };
                            self.right_index = 0;
                            continue;
                        };
                        self.current_left_matched = false;
                        self.right_index = 0;
                    }

                    while self.right_index < self.right_rows.len() {
                        let right_index = self.right_index;
                        self.right_index += 1;
                        let Some(left) = self.current_left.as_ref() else {
                            continue;
                        };
                        let row = self.combined_row(left, &self.right_rows[right_index]);
                        let matched = self.kind == JoinKind::Cross
                            || predicate_holds(&self.predicate, &row, &self.ctx)?;
                        if matched {
                            self.current_left_matched = true;
                            self.right_matched[right_index] = true;
                            return Ok(Some(row));
                        }
                    }

                    if !self.current_left_matched && self.preserves_left() {
                        let Some(mut row) = self.current_left.take() else {
                            continue;
                        };
                        row.extend(std::iter::repeat_n(Value::Null, self.right_width));
                        return Ok(Some(row));
                    }
                    self.current_left = None;
                }
                JoinPhase::UnmatchedRight => {
                    while self.right_index < self.right_rows.len() {
                        let right_index = self.right_index;
                        self.right_index += 1;
                        if !self.right_matched[right_index] {
                            let mut row = vec![Value::Null; self.left_width];
                            row.extend_from_slice(&self.right_rows[right_index]);
                            return Ok(Some(row));
                        }
                    }
                    self.phase = JoinPhase::Done;
                }
                JoinPhase::Done => return Ok(None),
            }
        }
    }
}

/// Binary hash join over one or more equi-keys, with the hash table built on the
/// materialized right (inner) side and the left side streamed as the probe. It
/// emits rows in the same order a [`NestedLoopJoin`] would — left-driven, right
/// rows in materialization order within each match — so results are identical
/// whether or not the query sorts.
///
/// Outer-join bookkeeping matches `NestedLoopJoin`: `right_matched` tracks which
/// inner rows participated in a match (for RIGHT/FULL), and unmatched left rows
/// are null-extended for LEFT/FULL. NULL keys never match (SQL join equality),
/// so rows with a NULL key are excluded from the hash table and the probe but
/// still surface as null-extended rows on a preserved side.
pub struct HashJoin {
    left: Box<dyn ExecNode>,
    right_rows: Vec<Tuple>,
    right_matched: Vec<bool>,
    /// Key hash → the `(right-row index, key values)` of every inner row carrying
    /// that hash. Only rows with fully non-NULL keys appear here; the stored key
    /// values are the collision guard checked by `keys_equal` at probe time.
    buckets: HashMap<u64, Vec<(usize, Vec<Value>)>>,
    left_width: usize,
    right_width: usize,
    kind: JoinKind,
    /// The left-side operand of each equi-key and its comparison type.
    /// `left_keys[i]` indexes the left (probe) input; `key_tys[i]` drives hashing
    /// and equality. The right-side operands are consumed at build time to fill
    /// `buckets`, so they aren't retained.
    left_keys: Vec<BoundExpr>,
    key_tys: Vec<PgType>,
    /// Non-equi conjuncts of the ON clause, checked per candidate pair.
    residual: Option<BoundExpr>,
    ctx: ExecContext,
    phase: JoinPhase,
    current_left: Option<Tuple>,
    current_left_matched: bool,
    /// Key values of the current left row (valid while its matches are emitted).
    current_left_keys: Vec<Value>,
    /// The bucket the current left row probes (its key hash), or `None` when the
    /// left key was NULL or unmatched. The bucket is re-borrowed per candidate via
    /// this hash — never cloned — and `probe_pos` cursors into it.
    current_probe_hash: Option<u64>,
    probe_pos: usize,
    right_index: usize,
}

impl HashJoin {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        left: Box<dyn ExecNode>,
        mut right: Box<dyn ExecNode>,
        left_width: usize,
        right_width: usize,
        kind: JoinKind,
        hash_keys: Vec<HashKey>,
        residual: Option<BoundExpr>,
        ctx: ExecContext,
    ) -> Result<Self, ExecError> {
        let key_tys: Vec<PgType> = hash_keys.iter().map(|k| k.ty).collect();
        let mut left_keys = Vec::with_capacity(hash_keys.len());
        let mut right_keys = Vec::with_capacity(hash_keys.len());
        for key in hash_keys {
            left_keys.push(key.left);
            right_keys.push(key.right);
        }

        let mut right_rows = Vec::new();
        while let Some(row) = right.next()? {
            right_rows.push(row);
        }

        // Build the hash table over the inner side. A right key expression
        // indexes the concatenated row, so evaluate it against a full-width row
        // whose left half is NULL padding and whose right half is the inner row.
        // One scratch buffer is reused across rows: its left half stays NULL and
        // only the right half is overwritten per row.
        let mut buckets: HashMap<u64, Vec<(usize, Vec<Value>)>> = HashMap::new();
        let mut scratch = vec![Value::Null; left_width + right_width];
        for (index, row) in right_rows.iter().enumerate() {
            scratch.truncate(left_width);
            scratch.extend_from_slice(row);
            if let Some(vals) = eval_join_keys(&right_keys, &scratch, &ctx)? {
                buckets
                    .entry(agg::hash_key(&key_tys, &vals))
                    .or_default()
                    .push((index, vals));
            }
        }

        let right_matched = vec![false; right_rows.len()];
        Ok(Self {
            left,
            right_rows,
            right_matched,
            buckets,
            left_width,
            right_width,
            kind,
            left_keys,
            key_tys,
            residual,
            ctx,
            phase: JoinPhase::LeftRows,
            current_left: None,
            current_left_matched: false,
            current_left_keys: Vec::new(),
            current_probe_hash: None,
            probe_pos: 0,
            right_index: 0,
        })
    }

    fn preserves_left(&self) -> bool {
        matches!(self.kind, JoinKind::Left | JoinKind::Full)
    }

    fn preserves_right(&self) -> bool {
        matches!(self.kind, JoinKind::Right | JoinKind::Full)
    }

    fn combined_row(&self, left: &[Value], right: &[Value]) -> Tuple {
        let mut row = Vec::with_capacity(self.left_width + self.right_width);
        row.extend_from_slice(left);
        row.extend_from_slice(right);
        row
    }

    /// Look up the candidate right rows for a freshly pulled left row: evaluate
    /// its keys (a NULL key yields no candidates) and pull the matching bucket.
    fn load_probe(&mut self) -> Result<(), ExecError> {
        self.probe_pos = 0;
        let Some(left) = self.current_left.as_ref() else {
            self.current_left_keys.clear();
            self.current_probe_hash = None;
            return Ok(());
        };
        match eval_join_keys(&self.left_keys, left, &self.ctx)? {
            Some(vals) => {
                // Record only the bucket's hash; the bucket itself is re-borrowed
                // per candidate during probing, never copied.
                self.current_probe_hash = Some(agg::hash_key(&self.key_tys, &vals));
                self.current_left_keys = vals;
            }
            None => {
                self.current_left_keys.clear();
                self.current_probe_hash = None;
            }
        }
        Ok(())
    }
}

impl ExecNode for HashJoin {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        loop {
            match self.phase {
                JoinPhase::LeftRows => {
                    if self.current_left.is_none() {
                        self.current_left = self.left.next()?;
                        let Some(_) = self.current_left else {
                            self.phase = if self.preserves_right() {
                                JoinPhase::UnmatchedRight
                            } else {
                                JoinPhase::Done
                            };
                            self.right_index = 0;
                            continue;
                        };
                        self.current_left_matched = false;
                        self.load_probe()?;
                    }

                    loop {
                        // Pull the next candidate whose key actually matches (a
                        // bucket hit can be a hash collision), scoping the bucket
                        // borrow so it ends before we build the row and mutate
                        // match state below.
                        let right_index = {
                            let Some(hash) = self.current_probe_hash else {
                                break;
                            };
                            let Some(bucket) = self.buckets.get(&hash) else {
                                break;
                            };
                            let mut found = None;
                            while self.probe_pos < bucket.len() {
                                let (index, right_vals) = &bucket[self.probe_pos];
                                self.probe_pos += 1;
                                if agg::keys_equal(
                                    &self.key_tys,
                                    &self.current_left_keys,
                                    right_vals,
                                ) {
                                    found = Some(*index);
                                    break;
                                }
                            }
                            match found {
                                Some(index) => index,
                                None => break,
                            }
                        };
                        // Then the residual (non-equi) conjuncts of the ON clause.
                        let Some(left) = self.current_left.as_ref() else {
                            continue;
                        };
                        let row = self.combined_row(left, &self.right_rows[right_index]);
                        if !predicate_holds(&self.residual, &row, &self.ctx)? {
                            continue;
                        }
                        self.current_left_matched = true;
                        self.right_matched[right_index] = true;
                        return Ok(Some(row));
                    }

                    if !self.current_left_matched && self.preserves_left() {
                        let Some(mut row) = self.current_left.take() else {
                            continue;
                        };
                        row.extend(std::iter::repeat_n(Value::Null, self.right_width));
                        return Ok(Some(row));
                    }
                    self.current_left = None;
                }
                JoinPhase::UnmatchedRight => {
                    while self.right_index < self.right_rows.len() {
                        let right_index = self.right_index;
                        self.right_index += 1;
                        if !self.right_matched[right_index] {
                            let mut row = vec![Value::Null; self.left_width];
                            row.extend_from_slice(&self.right_rows[right_index]);
                            return Ok(Some(row));
                        }
                    }
                    self.phase = JoinPhase::Done;
                }
                JoinPhase::Done => return Ok(None),
            }
        }
    }
}

/// Evaluate each equi-key expression over `row`, returning `None` as soon as one
/// is NULL (a NULL key can never match in a join, mirroring PG). `row` must be a
/// full-width concatenated row so key column indices stay valid.
fn eval_join_keys(
    keys: &[BoundExpr],
    row: &[Value],
    ctx: &ExecContext,
) -> Result<Option<Vec<Value>>, ExecError> {
    let mut vals = Vec::with_capacity(keys.len());
    for key in keys {
        let v = eval(key, row, ctx)?;
        if matches!(v, Value::Null) {
            return Ok(None);
        }
        vals.push(v);
    }
    Ok(Some(vals))
}

/// Wrap `node` in a `Sort` when there are ORDER BY keys. `columns` is the
/// client-visible output width; sort keys may address hidden ("resjunk")
/// columns past it, which the sort trims before emitting.
fn maybe_sort(
    node: Box<dyn ExecNode>,
    sort: Vec<SortKey>,
    columns: &[OutputColumn],
) -> Result<Box<dyn ExecNode>, ExecError> {
    if sort.is_empty() {
        return Ok(node);
    }
    Ok(Box::new(Sort::new(node, sort, columns.len())?))
}

/// Materializing sort (ORDER BY). NULLs order per `SortKey.nulls_first`;
/// non-null values compare via the key's type total order. Keys may reference
/// hidden columns appended past `visible_width`; those are dropped from each
/// emitted tuple so only the client-visible columns leave the node.
pub struct Sort {
    rows: std::vec::IntoIter<Tuple>,
}

impl Sort {
    pub fn new(
        mut child: Box<dyn ExecNode>,
        keys: Vec<SortKey>,
        visible_width: usize,
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
                        let cmp = compare_values(key.ty, va, vb);
                        if key.asc { cmp } else { cmp.reverse() }
                    }
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });
        // Drop hidden sort-only columns so downstream (the wire layer, an outer
        // subquery) sees exactly the visible output width. Comparison above
        // already read them; they are no longer needed.
        for row in &mut rows {
            row.truncate(visible_width);
        }
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

/// Materializing de-duplication for `SELECT DISTINCT` / `DISTINCT ON`. On the
/// first pull it buffers every child row and keeps the first row seen per
/// distinct-key group (NULL-aware, so two NULL keys collapse — PG's DISTINCT
/// semantics), preserving input order. When the input is already sorted (as
/// `DISTINCT ON` requires), "first per group" is the sort-order winner. Keys may
/// reference hidden columns the child kept past `visible_width`; those are
/// dropped from each surviving row here (the sort below no longer truncates when
/// a Distinct follows).
pub struct Distinct {
    rows: std::vec::IntoIter<Tuple>,
}

impl Distinct {
    pub fn new(
        mut child: Box<dyn ExecNode>,
        keys: Vec<DistinctKey>,
        visible_width: usize,
    ) -> Result<Self, ExecError> {
        let key_tys: Vec<PgType> = keys.iter().map(|k| k.ty).collect();
        let mut out: Vec<Tuple> = Vec::new();
        // Hash of a row's distinct key → indices into `out` of surviving rows
        // sharing that hash; `keys_equal` resolves collisions. Mirrors the
        // grouping in `Aggregate::build`.
        let mut lookup: HashMap<u64, Vec<usize>> = HashMap::new();
        while let Some(row) = child.next()? {
            let key: Vec<Value> = keys.iter().map(|k| row[k.column].clone()).collect();
            let bucket = lookup.entry(agg::hash_key(&key_tys, &key)).or_default();
            let seen = bucket.iter().copied().any(|i| {
                let existing: Vec<Value> =
                    keys.iter().map(|k| out[i][k.column].clone()).collect();
                agg::keys_equal(&key_tys, &existing, &key)
            });
            if !seen {
                bucket.push(out.len());
                out.push(row);
            }
        }
        // Drop hidden distinct/sort-only columns so downstream sees exactly the
        // visible output width, as `Sort` does when no Distinct follows.
        for row in &mut out {
            row.truncate(visible_width);
        }
        Ok(Self {
            rows: out.into_iter(),
        })
    }
}

impl ExecNode for Distinct {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        Ok(self.rows.next())
    }
}

/// Grouped aggregation. On the first pull it buffers every child row, groups
/// them by the NULL-aware equality of the group-key expressions in first-seen
/// order — so per-group accumulation follows scan order, matching PG's hash
/// aggregate — and emits one row per group laid out `[group keys…, aggregates…]`.
/// An empty group-key list is the implicit single group: exactly one output row
/// even over no input (`SELECT count(*)` → 0).
pub struct Aggregate {
    child: Box<dyn ExecNode>,
    group_exprs: Vec<BoundExpr>,
    aggregates: Vec<BoundAggregate>,
    ctx: ExecContext,
    /// Output rows, built lazily on the first `next()`.
    output: Option<std::vec::IntoIter<Tuple>>,
}

/// One accumulating group: its key values and one accumulator per aggregate.
struct AggGroup {
    key: Vec<Value>,
    accumulators: Vec<agg::Accumulator>,
    /// One optional seen-value set per aggregate. Non-DISTINCT aggregates have
    /// no set and therefore retain their streaming accumulation behavior.
    distinct_values: Vec<Option<agg::DistinctValues>>,
}

impl Aggregate {
    pub fn new(
        child: Box<dyn ExecNode>,
        group_exprs: Vec<BoundExpr>,
        aggregates: Vec<BoundAggregate>,
        ctx: ExecContext,
    ) -> Self {
        Self {
            child,
            group_exprs,
            aggregates,
            ctx,
            output: None,
        }
    }

    /// Drain the child, accumulate per group, and materialize the output rows.
    fn build(&mut self) -> Result<std::vec::IntoIter<Tuple>, ExecError> {
        let key_tys: Vec<_> = self.group_exprs.iter().map(BoundExpr::ty).collect();
        let mut groups: Vec<AggGroup> = Vec::new();
        // Hash of each group's key → the indices of groups sharing that hash, so
        // a row finds its group in ~O(1); `keys_equal` resolves hash collisions.
        // Groups stay in first-seen order (accumulation follows scan order).
        let mut lookup: HashMap<u64, Vec<usize>> = HashMap::new();
        // The implicit single group needs one seeded group so an empty input
        // still produces a row.
        if self.group_exprs.is_empty() {
            groups.push(AggGroup {
                key: Vec::new(),
                accumulators: self.aggregates.iter().map(agg::Accumulator::new).collect(),
                distinct_values: self
                    .aggregates
                    .iter()
                    .map(|agg| agg.distinct.then(|| agg::DistinctValues::new(agg.input_ty)))
                    .collect(),
            });
        }
        while let Some(row) = self.child.next()? {
            let idx = if self.group_exprs.is_empty() {
                0
            } else {
                let key = self
                    .group_exprs
                    .iter()
                    .map(|e| eval(e, &row, &self.ctx))
                    .collect::<Result<Vec<_>, _>>()?;
                let bucket = lookup.entry(agg::hash_key(&key_tys, &key)).or_default();
                match bucket
                    .iter()
                    .copied()
                    .find(|&i| agg::keys_equal(&key_tys, &groups[i].key, &key))
                {
                    Some(i) => i,
                    None => {
                        let i = groups.len();
                        groups.push(AggGroup {
                            key,
                            accumulators: self
                                .aggregates
                                .iter()
                                .map(agg::Accumulator::new)
                                .collect(),
                            distinct_values: self
                                .aggregates
                                .iter()
                                .map(|agg| {
                                    agg.distinct.then(|| agg::DistinctValues::new(agg.input_ty))
                                })
                                .collect(),
                        });
                        bucket.push(i);
                        i
                    }
                }
            };
            let group = &mut groups[idx];
            for ((agg, acc), distinct_values) in self
                .aggregates
                .iter()
                .zip(group.accumulators.iter_mut())
                .zip(group.distinct_values.iter_mut())
            {
                match &agg.arg {
                    // COUNT(*) counts every row, skipping no NULLs.
                    None => acc.count_row(),
                    Some(arg) => {
                        let v = eval(arg, &row, &self.ctx)?;
                        // Every aggregate but COUNT(*) ignores NULL inputs.
                        if !matches!(v, Value::Null) {
                            if distinct_values.as_mut().is_none_or(|seen| seen.insert(&v)) {
                                acc.accumulate(v)?;
                            }
                        }
                    }
                }
            }
        }
        let mut out = Vec::with_capacity(groups.len());
        for group in groups {
            let mut tuple = group.key;
            for acc in group.accumulators {
                tuple.push(acc.finalize()?);
            }
            out.push(tuple);
        }
        Ok(out.into_iter())
    }
}

impl ExecNode for Aggregate {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        if self.output.is_none() {
            self.output = Some(self.build()?);
        }
        match self.output.as_mut() {
            Some(output) => Ok(output.next()),
            None => panic!("aggregate output was not initialized"),
        }
    }
}

/// Streaming LIMIT/OFFSET. Discards the first `remaining_offset` child tuples,
/// then passes through up to `remaining_limit` more before stopping. Unlike
/// [`Sort`] it never materializes: rows flow through one at a time, and once the
/// limit is reached it stops pulling from the child entirely.
pub struct Limit {
    child: Box<dyn ExecNode>,
    remaining_offset: u64,
    /// `None` = unbounded (no LIMIT).
    remaining_limit: Option<u64>,
}

impl Limit {
    /// Negative counts are rejected at bind time, so both are non-negative here.
    pub fn new(child: Box<dyn ExecNode>, limit: Option<i64>, offset: Option<i64>) -> Self {
        Self {
            child,
            remaining_offset: offset.unwrap_or(0).max(0) as u64,
            remaining_limit: limit.map(|n| n.max(0) as u64),
        }
    }
}

impl ExecNode for Limit {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        // Skip the offset rows first (they still count as consumed).
        while self.remaining_offset > 0 {
            match self.child.next()? {
                Some(_) => self.remaining_offset -= 1,
                None => return Ok(None),
            }
        }
        match self.remaining_limit {
            Some(0) => Ok(None),
            Some(n) => match self.child.next()? {
                Some(row) => {
                    self.remaining_limit = Some(n - 1);
                    Ok(Some(row))
                }
                None => Ok(None),
            },
            None => self.child.next(),
        }
    }
}

/// Full table scan through the storage API.
pub struct SeqScan {
    iter: Box<dyn Iterator<Item = Tuple> + Send>,
}

impl SeqScan {
    pub fn new(table: &Arc<dyn TableAm>, txn: &TxnContext) -> Self {
        Self {
            iter: Box::new(table.scan(txn).map(|(_, tuple)| tuple)),
        }
    }
}

impl ExecNode for SeqScan {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        Ok(self.iter.next())
    }
}

/// Union scan over a partitioned parent's leaf partitions: concatenates each
/// leaf's snapshot scan into one row stream, in leaf order (see
/// [`PhysicalPlan::Append`](crabgresql_planner::PhysicalPlan::Append)). Each
/// leaf captures its own MVCC snapshot up front, exactly as [`SeqScan`] does.
pub struct Append {
    iter: Box<dyn Iterator<Item = Tuple> + Send>,
}

impl Append {
    pub fn new(tables: &[Arc<dyn TableAm>], txn: &TxnContext) -> Self {
        let scans: Vec<Box<dyn Iterator<Item = Tuple> + Send>> = tables
            .iter()
            .map(|table| {
                Box::new(table.scan(txn).map(|(_, tuple)| tuple))
                    as Box<dyn Iterator<Item = Tuple> + Send>
            })
            .collect();
        Self {
            iter: Box::new(scans.into_iter().flatten()),
        }
    }
}

impl ExecNode for Append {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        Ok(self.iter.next())
    }
}

/// Equality index scan: probes the engine's physical index for the key. When
/// the engine cannot serve it (durable heap engine, an index whose key type it
/// cannot physically index, a system catalog) it falls back to a full scan and
/// re-checks key equality per row, which is what makes that fallback correct.
/// The physical-index path is already exact (the engine returns only rows whose
/// key equals the probe), so it needs no re-check. NULL never matches under `=`.
pub struct IndexScan {
    iter: Box<dyn Iterator<Item = Tuple> + Send>,
}

impl IndexScan {
    pub fn new(
        table: &Arc<dyn TableAm>,
        index_name: &str,
        key: Vec<(usize, BoundExpr)>,
        ctx: &ExecContext,
        txn: &TxnContext,
    ) -> Result<Self, ExecError> {
        // The key value expressions are row-constant (the planner guarantees
        // it), so they evaluate once against an empty row.
        let key_values: Vec<Value> = key
            .iter()
            .map(|(_, expr)| eval(expr, &[], ctx))
            .collect::<Result<_, _>>()?;
        let iter: Box<dyn Iterator<Item = Tuple> + Send> =
            match table.index_lookup(index_name, &key_values, txn) {
                // Exact path: the engine already returned only key-matching,
                // MVCC-visible rows.
                Some(rows) => Box::new(rows.map(|(_, tuple)| tuple)),
                // Fallback path: a full scan, so re-check the key per row.
                None => {
                    let cols: Vec<(usize, PgType)> = key
                        .iter()
                        .map(|(column, _)| (*column, table.schema().columns[*column].ty))
                        .collect();
                    Box::new(table.scan(txn).filter_map(move |(_, tuple)| {
                        cols.iter().zip(&key_values).all(|(&(column, ty), want)| {
                            let cell = &tuple[column];
                            !matches!(cell, Value::Null)
                                && !matches!(want, Value::Null)
                                && compare_values(ty, cell, want) == Ordering::Equal
                        })
                        .then_some(tuple)
                    }))
                }
            };
        Ok(Self { iter })
    }
}

impl ExecNode for IndexScan {
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
            if predicate_holds(&self.predicate, &row, &self.ctx)? {
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
            .map(|expr| eval(expr, &row, &self.ctx))
            .collect::<Result<_, _>>()?;
        Ok(Some(projected))
    }
}

/// Projection with one or more set-returning functions in the target list. Each
/// input row expands to as many output rows as the longest SRF produces; shorter
/// SRFs are NULL-padded once exhausted (PG's `ROWS FROM` semantics since PG 10)
/// and scalar columns repeat. An input row whose SRFs are all empty yields no
/// output rows.
pub struct ProjectSet {
    child: Box<dyn ExecNode>,
    exprs: Vec<BoundExpr>,
    ctx: ExecContext,
    /// Expansion state for the current input row; `None` before the first pull
    /// and between fully-expanded input rows.
    current: Option<RowExpansion>,
}

/// The per-Srf iterators for one input row, parallel to `exprs` (scalar slots
/// are `None`), plus the input row scalar projections evaluate against.
struct RowExpansion {
    input: Tuple,
    series: Vec<Option<Series>>,
}

impl ProjectSet {
    pub fn new(child: Box<dyn ExecNode>, exprs: Vec<BoundExpr>, ctx: ExecContext) -> Self {
        Self {
            child,
            exprs,
            ctx,
            current: None,
        }
    }

    /// Build the per-Srf series for a fresh input row.
    fn expand(&self, input: Tuple) -> Result<RowExpansion, ExecError> {
        let mut series = Vec::with_capacity(self.exprs.len());
        for expr in &self.exprs {
            match expr {
                BoundExpr::Srf { func, args, .. } => {
                    let values = args
                        .iter()
                        .map(|a| eval(a, &input, &self.ctx))
                        .collect::<Result<Vec<_>, _>>()?;
                    series.push(Some(build_series(*func, &values)?));
                }
                _ => series.push(None),
            }
        }
        Ok(RowExpansion { input, series })
    }
}

impl ExecNode for ProjectSet {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        loop {
            if self.current.is_none() {
                let Some(input) = self.child.next()? else {
                    return Ok(None);
                };
                self.current = Some(self.expand(input)?);
            }
            let Some(exp) = self.current.as_mut() else {
                continue;
            };

            // Advance every SRF once; the input row is exhausted when they all are.
            let mut srf_vals: Vec<Option<Value>> = Vec::with_capacity(exp.series.len());
            let mut any = false;
            for slot in exp.series.iter_mut() {
                let value = match slot {
                    Some(series) => series.next_value()?,
                    None => None,
                };
                any |= value.is_some();
                srf_vals.push(value);
            }
            if !any {
                self.current = None;
                continue;
            }

            let input = exp.input.clone();
            let mut out = Vec::with_capacity(self.exprs.len());
            for (expr, srf_val) in self.exprs.iter().zip(srf_vals) {
                match expr {
                    // Exhausted SRFs pad with NULL to match the longest.
                    BoundExpr::Srf { .. } => out.push(srf_val.unwrap_or(Value::Null)),
                    _ => out.push(eval(expr, &input, &self.ctx)?),
                }
            }
            return Ok(Some(out));
        }
    }
}

/// Build the range iterator for a target-list SRF (`generate_series` or
/// `jsonb_path_query`).
fn build_series(func: TableFn, values: &[Value]) -> Result<Series, ExecError> {
    match func {
        TableFn::GenerateSeries(elem) => Series::from_args(elem, values),
        TableFn::JsonbPathQuery => jsonb_path_query_series(values),
        TableFn::PgInputErrorInfo => Err(ExecError::new(
            crabgresql_pg_wire::sqlstate::FEATURE_NOT_SUPPORTED,
            "set-returning function is not supported in this context",
        )),
    }
}

/// Evaluate a `jsonb_path_query(target, path [, vars, silent])` call to a
/// materialized [`Series`] of its result items. `jsonb_path_query` is STRICT, so
/// a NULL in any argument yields no rows; `silent` suppresses structural errors
/// (also no rows). A missing-variable error always raises.
fn jsonb_path_query_series(values: &[Value]) -> Result<Series, ExecError> {
    // STRICT: any NULL argument (target, path, vars, or silent) → no rows.
    if values.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Series::Empty);
    }
    let (Value::Jsonb(target), Value::Jsonpath(path)) = (&values[0], &values[1]) else {
        return Ok(Series::Empty);
    };
    let vars = match values.get(2) {
        Some(Value::Jsonb(v)) => Some(v),
        _ => None,
    };
    let silent = matches!(values.get(3), Some(Value::Bool(true)));
    let items = crabgresql_types::jsonpath::query(path, target, vars, silent)
        .map_err(|e| ExecError::new(e.sqlstate, e.message))?;
    let rows: Vec<Value> = items.into_iter().map(Value::Jsonb).collect();
    Ok(Series::Materialized(rows.into_iter()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crabgresql_binder::{BinOp, UnaryOp};
    use crabgresql_storage_api::{
        Column, IndexConstraint, IndexKey, IndexMethod, TableEngine, TableSchema,
    };
    use crabgresql_txn::{CommandId, TransactionManager, TxnContext, Xid};
    use crabgresql_types::PgType;
    use eval::coerce_value;

    #[track_caller]
    fn test_ok<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("test fixture operation failed: {error:?}"),
        }
    }

    thread_local! {
        /// One transaction manager per test thread (libtest runs each test on its
        /// own thread), so `wtxn`/`rtxn` share a commit log within a test but stay
        /// isolated across tests. These tests exercise executor mechanics, not MVCC
        /// visibility, so a write just commits immediately and a read sees it.
        static TM: TransactionManager = TransactionManager::new();
    }

    /// A committed writer context: seeds/DML whose versions are visible to any
    /// later read. Commits the XID up front, so a row stamped with it (or an old
    /// version it deletes) is immediately committed.
    fn wtxn() -> TxnContext {
        TM.with(|tm| {
            let xid = tm.allocate_xid();
            test_ok(tm.commit(xid));
            tm.context(xid, CommandId::FIRST)
        })
    }

    /// A reader with no XID of its own and a fresh snapshot that sees every
    /// committed version.
    fn rtxn() -> TxnContext {
        TM.with(|tm| tm.context(Xid::INVALID, CommandId::FIRST))
    }

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
        eval(expr, &[], &ExecContext::default())
    }

    /// (op, left, right, expected), with `None` as SQL NULL.
    type TruthTableRow = (BinOp, Option<bool>, Option<bool>, Option<bool>);

    #[test]
    fn and_or_follow_kleene_tables() -> anyhow::Result<()> {
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
            assert_eq!(eval_const(&expr)?, expected, "{l:?} {op:?} {r:?}");
        }

        Ok(())
    }

    #[test]
    fn null_operand_nulls_comparison() -> anyhow::Result<()> {
        let expr = binary(
            BinOp::Eq,
            PgType::Int4,
            int4(1),
            BoundExpr::Const {
                value: Value::Null,
                ty: PgType::Int4,
            },
        );
        assert_eq!(eval_const(&expr)?, Value::Null);

        Ok(())
    }

    #[test]
    fn not_follows_three_valued_logic() -> anyhow::Result<()> {
        let not = |v| BoundExpr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(boolean(v)),
        };
        assert_eq!(eval_const(&not(Some(true)))?, Value::Bool(false));
        assert_eq!(eval_const(&not(None))?, Value::Null);

        Ok(())
    }

    #[test]
    fn is_null_is_never_null() -> anyhow::Result<()> {
        let is_null = |v: Value, negated| BoundExpr::IsNull {
            expr: Box::new(BoundExpr::Const {
                value: v,
                ty: PgType::Int4,
            }),
            negated,
        };
        assert_eq!(eval_const(&is_null(Value::Null, false))?, Value::Bool(true));
        assert_eq!(
            eval_const(&is_null(Value::Int4(1), false))?,
            Value::Bool(false)
        );
        assert_eq!(eval_const(&is_null(Value::Null, true))?, Value::Bool(false));

        Ok(())
    }

    #[test]
    fn case_selects_first_true_branch_lazily() -> anyhow::Result<()> {
        // CASE WHEN <cond1> THEN 10 WHEN <cond2> THEN 20 ELSE 30 END, over
        // constant conditions; false/NULL skip, only the winner is returned.
        let case = |c1: Option<bool>, c2: Option<bool>, else_: Option<BoundExpr>| BoundExpr::Case {
            whens: vec![(boolean(c1), int4(10)), (boolean(c2), int4(20))],
            else_: else_.map(Box::new),
            ty: PgType::Int4,
        };
        let e30 = || Some(int4(30));
        assert_eq!(
            eval_const(&case(Some(true), Some(true), e30()))?,
            Value::Int4(10)
        );
        assert_eq!(
            eval_const(&case(Some(false), Some(true), e30()))?,
            Value::Int4(20)
        );
        // NULL condition behaves like false: falls through to ELSE.
        assert_eq!(
            eval_const(&case(None, Some(false), e30()))?,
            Value::Int4(30)
        );
        // No branch matches and no ELSE: NULL.
        assert_eq!(eval_const(&case(Some(false), None, None))?, Value::Null);

        Ok(())
    }

    #[test]
    fn case_does_not_evaluate_unselected_results() -> anyhow::Result<()> {
        // The losing branch divides by zero; a lazy CASE must never touch it.
        let bomb = binary(BinOp::Div, PgType::Int4, int4(1), int4(0));
        let expr = BoundExpr::Case {
            whens: vec![(boolean(Some(true)), int4(1)), (boolean(Some(true)), bomb)],
            else_: None,
            ty: PgType::Int4,
        };
        assert_eq!(eval_const(&expr)?, Value::Int4(1));

        Ok(())
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
    fn min_over_minus_one_edge_cases() -> anyhow::Result<()> {
        // MIN / -1 overflows ...
        let e =
            eval_const(&binary(BinOp::Div, PgType::Int4, int4(i32::MIN), int4(-1))).unwrap_err();
        assert_eq!(e.code, "22003");
        // ... but MIN % -1 is 0, as in PG.
        assert_eq!(
            eval_const(&binary(BinOp::Mod, PgType::Int4, int4(i32::MIN), int4(-1)))?,
            Value::Int4(0)
        );

        Ok(())
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
    fn text_and_bool_comparisons() -> anyhow::Result<()> {
        let text_const = |s: &str| BoundExpr::Const {
            value: Value::Text(s.into()),
            ty: PgType::Text,
        };
        let expr = binary(BinOp::Lt, PgType::Text, text_const("a"), text_const("b"));
        assert_eq!(eval_const(&expr)?, Value::Bool(true));
        // false < true
        let expr = binary(
            BinOp::Lt,
            PgType::Bool,
            boolean(Some(false)),
            boolean(Some(true)),
        );
        assert_eq!(eval_const(&expr)?, Value::Bool(true));

        Ok(())
    }

    #[test]
    fn coerce_range_checks_int8_to_int4() -> anyhow::Result<()> {
        let ctx = &ExecContext::default();
        assert_eq!(
            coerce_value(Value::Int8(7), PgType::Int4, &ctx)?,
            Value::Int4(7)
        );
        let e = coerce_value(Value::Int8(i64::MAX), PgType::Int4, &ctx).unwrap_err();
        assert_eq!(e.code, "22003");
        assert_eq!(coerce_value(Value::Null, PgType::Int4, &ctx)?, Value::Null);

        Ok(())
    }

    fn test_table() -> Arc<dyn TableAm> {
        let engine = crabgresql_pg_engine::ephemeral_engine();
        let table = test_ok(engine.create_table(TableSchema::in_namespace(
            "t",
            "public",
            vec![
                Column::new("id", PgType::Int4),
                Column::new("label", PgType::Text),
            ],
        )));
        let txn = wtxn();
        table.insert(vec![Value::Int4(1), Value::Text("one".into())], &txn);
        table.insert(vec![Value::Int4(2), Value::Text("two".into())], &txn);
        table.insert(vec![Value::Int4(3), Value::Null], &txn);
        table
    }

    fn collect(node: &mut dyn ExecNode) -> Vec<Tuple> {
        let mut rows = Vec::new();
        while let Some(row) = test_ok(node.next()) {
            rows.push(row);
        }
        rows
    }

    /// `test_table`'s rows plus a physical unique index on `id`.
    fn indexed_table() -> Arc<dyn TableAm> {
        let engine = crabgresql_pg_engine::ephemeral_engine();
        let table = test_ok(engine.create_table(TableSchema::in_namespace(
            "t",
            "public",
            vec![
                Column::new("id", PgType::Int4),
                Column::new("label", PgType::Text),
            ],
        )));
        test_ok(engine.create_index(
            "public",
            "t",
            IndexMetadata {
                name: "t_id_key".into(),
                method: IndexMethod::BTree,
                keys: vec![IndexKey {
                    column: 0,
                    descending: false,
                    nulls_first: false,
                }],
                unique: true,
                nulls_distinct: true,
                constraint: Some(IndexConstraint::Unique),
            },
        ));
        let txn = wtxn();
        table.insert(vec![Value::Int4(1), Value::Text("one".into())], &txn);
        table.insert(vec![Value::Int4(2), Value::Text("two".into())], &txn);
        table.insert(vec![Value::Int4(3), Value::Null], &txn);
        table
    }

    #[test]
    fn index_scan_probes_physical_index() {
        let table = indexed_table();
        let mut node = test_ok(IndexScan::new(
            &table,
            "t_id_key",
            vec![(0, int4(2))],
            &ExecContext::default(),
            &rtxn(),
        ));
        assert_eq!(
            collect(&mut node),
            vec![vec![Value::Int4(2), Value::Text("two".into())]]
        );
    }

    #[test]
    fn index_scan_falls_back_to_scan_without_physical_index() {
        // `test_table` has no physical index: `index_lookup` returns None and the
        // node scans, re-checking the key so the result is still exact.
        let table = test_table();
        let mut node = test_ok(IndexScan::new(
            &table,
            "missing_index",
            vec![(0, int4(2))],
            &ExecContext::default(),
            &rtxn(),
        ));
        assert_eq!(
            collect(&mut node),
            vec![vec![Value::Int4(2), Value::Text("two".into())]]
        );
    }

    #[test]
    fn index_scan_empty_for_missing_key() {
        let table = indexed_table();
        let mut node = test_ok(IndexScan::new(
            &table,
            "t_id_key",
            vec![(0, int4(99))],
            &ExecContext::default(),
            &rtxn(),
        ));
        assert!(collect(&mut node).is_empty());
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
        let mut node = Filter::new(
            Box::new(SeqScan::new(&table, &rtxn())),
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
            Box::new(SeqScan::new(&table, &rtxn())),
            predicate,
            ExecContext::default(),
        );
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
        let mut node = Projection::new(
            Box::new(SeqScan::new(&table, &rtxn())),
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

    #[test]
    fn sort_orders_by_hidden_column_then_trims() -> anyhow::Result<()> {
        // Project only `id` (visible), but sort on a hidden trailing column
        // holding `label` (a resjunk column that ORDER BY references but the
        // client never sees). The sort must order by the hidden value's type
        // and then drop it, emitting a single visible column.
        let table = test_table();
        let exprs = vec![
            BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4,
            },
            BoundExpr::ColumnRef {
                index: 1,
                ty: PgType::Text,
            },
        ];
        let projection = Projection::new(
            Box::new(SeqScan::new(&table, &rtxn())),
            exprs,
            ExecContext::default(),
        );
        // ORDER BY label ASC: NULL (id 3) sorts last (NULLS LAST default), then
        // 'one' (id 1), 'two' (id 2).
        let key = SortKey {
            column: 1,
            ty: PgType::Text,
            asc: true,
            nulls_first: false,
        };
        let mut node = Sort::new(Box::new(projection), vec![key], 1)?;
        assert_eq!(
            collect(&mut node),
            vec![
                vec![Value::Int4(1)],
                vec![Value::Int4(2)],
                vec![Value::Int4(3)],
            ],
            "rows ordered by hidden label, trimmed to the single visible column"
        );

        Ok(())
    }

    /// Scan `t` (ids 1,2,3 in insertion order), keeping just the `id` column.
    fn id_scan(table: &Arc<dyn TableAm>) -> Box<dyn ExecNode> {
        Box::new(Projection::new(
            Box::new(SeqScan::new(table, &rtxn())),
            vec![BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4,
            }],
            ExecContext::default(),
        ))
    }

    fn ids(node: &mut dyn ExecNode) -> Vec<i32> {
        collect(node)
            .into_iter()
            .map(|row| match row[0] {
                Value::Int4(n) => n,
                ref other => panic!("expected int4, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn limit_caps_row_count() {
        let table = test_table();
        let mut node = Limit::new(id_scan(&table), Some(2), None);
        assert_eq!(ids(&mut node), vec![1, 2]);
    }

    #[test]
    fn offset_skips_leading_rows() {
        let table = test_table();
        let mut node = Limit::new(id_scan(&table), None, Some(1));
        assert_eq!(ids(&mut node), vec![2, 3]);
    }

    #[test]
    fn limit_offset_together_slice_the_middle() {
        let table = test_table();
        let mut node = Limit::new(id_scan(&table), Some(1), Some(1));
        assert_eq!(ids(&mut node), vec![2]);
    }

    #[test]
    fn offset_zero_passes_everything_through() {
        // The float4/float8 fence: OFFSET 0 is a no-op over the full input.
        let table = test_table();
        let mut node = Limit::new(id_scan(&table), None, Some(0));
        assert_eq!(ids(&mut node), vec![1, 2, 3]);
    }

    #[test]
    fn offset_past_end_yields_nothing() {
        let table = test_table();
        let mut node = Limit::new(id_scan(&table), None, Some(10));
        assert_eq!(ids(&mut node), Vec::<i32>::new());
    }

    /// A source node that streams pre-built tuples, for exercising nodes that
    /// consume arbitrary rows (Sort, Distinct) without going through storage.
    struct VecSource {
        rows: std::vec::IntoIter<Tuple>,
    }

    impl VecSource {
        fn boxed(rows: Vec<Tuple>) -> Box<dyn ExecNode> {
            Box::new(Self {
                rows: rows.into_iter(),
            })
        }
    }

    impl ExecNode for VecSource {
        fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
            Ok(self.rows.next())
        }
    }

    #[test]
    fn distinct_dedups_keeping_first_seen_order() -> anyhow::Result<()> {
        // Plain SELECT DISTINCT over a single column: keep the first occurrence
        // of each value in input order, collapsing NULLs together.
        let rows = vec![
            vec![Value::Int4(2)],
            vec![Value::Int4(1)],
            vec![Value::Int4(2)],
            vec![Value::Null],
            vec![Value::Int4(1)],
            vec![Value::Null],
        ];
        let keys = vec![DistinctKey {
            column: 0,
            ty: PgType::Int4,
        }];
        let mut node = Distinct::new(VecSource::boxed(rows), keys, 1)?;
        assert_eq!(
            collect(&mut node),
            vec![
                vec![Value::Int4(2)],
                vec![Value::Int4(1)],
                vec![Value::Null],
            ],
            "duplicates removed, first-seen order preserved, NULLs collapsed"
        );
        Ok(())
    }

    #[test]
    fn distinct_on_keys_hidden_column_and_trims() -> anyhow::Result<()> {
        // DISTINCT ON (b) a — b is a hidden trailing column (index 1) the client
        // never sees. Rows arrive already sorted by b (as DISTINCT ON requires);
        // the first row of each b-group survives and the hidden column is
        // trimmed, leaving only the visible `a`.
        let rows = vec![
            vec![Value::Int4(10), Value::Int4(1)],
            vec![Value::Int4(11), Value::Int4(1)],
            vec![Value::Int4(20), Value::Int4(2)],
            vec![Value::Int4(21), Value::Int4(2)],
        ];
        let keys = vec![DistinctKey {
            column: 1,
            ty: PgType::Int4,
        }];
        let mut node = Distinct::new(VecSource::boxed(rows), keys, 1)?;
        assert_eq!(
            collect(&mut node),
            vec![vec![Value::Int4(10)], vec![Value::Int4(20)]],
            "one row per DISTINCT ON group, hidden key column trimmed"
        );
        Ok(())
    }

    #[test]
    fn limit_applies_after_sort() -> anyhow::Result<()> {
        // ORDER BY id DESC LIMIT 1 must return the max, not the first-scanned row.
        let table = test_table();
        let sort = Sort::new(
            id_scan(&table),
            vec![SortKey {
                column: 0,
                ty: PgType::Int4,
                asc: false,
                nulls_first: false,
            }],
            1,
        )?;
        let mut node = Limit::new(Box::new(sort), Some(1), None);
        assert_eq!(ids(&mut node), vec![3]);

        Ok(())
    }

    #[test]
    fn update_evaluates_against_old_row_and_buffers() -> anyhow::Result<()> {
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
        let Execution::Updated(n) =
            execute_update(&table, &None, &assignments, None, &ExecContext::default(), &wtxn())?
        else {
            panic!("expected Updated");
        };
        assert_eq!(n, 3);
        let ids: Vec<Value> = table.scan(&rtxn()).map(|(_, t)| t[0].clone()).collect();
        assert_eq!(ids, vec![Value::Int4(2), Value::Int4(3), Value::Int4(4)]);

        Ok(())
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
        let Err(e) =
            execute_update(&table, &None, &assignments, None, &ExecContext::default(), &wtxn())
        else {
            panic!("expected error");
        };
        assert_eq!(e.code, "22012");
        let ids: Vec<Value> = table.scan(&rtxn()).map(|(_, t)| t[0].clone()).collect();
        assert_eq!(ids, vec![Value::Int4(1), Value::Int4(2), Value::Int4(3)]);
    }

    #[test]
    fn delete_with_predicate_removes_matching_rows() -> anyhow::Result<()> {
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
        let Execution::Deleted(n) =
            execute_delete(&table, &predicate, None, &ExecContext::default(), &wtxn())?
        else {
            panic!("expected Deleted");
        };
        assert_eq!(n, 2);
        assert_eq!(table.scan(&rtxn()).count(), 1);

        Ok(())
    }

    /// A fresh engine with `t(id int4, label text)` seeded with three rows, for
    /// the `RETURNING` tests.
    fn returning_engine() -> Arc<dyn TableEngine> {
        let engine = crabgresql_pg_engine::ephemeral_engine();
        let table = test_ok(engine.create_table(TableSchema::in_namespace(
            "t",
            "public",
            vec![
                Column::new("id", PgType::Int4),
                Column::new("label", PgType::Text),
            ],
        )));
        let txn = wtxn();
        table.insert(vec![Value::Int4(1), Value::Text("one".into())], &txn);
        table.insert(vec![Value::Int4(2), Value::Text("two".into())], &txn);
        table.insert(vec![Value::Int4(3), Value::Text("three".into())], &txn);
        engine
    }

    /// Parse → bind → plan → execute a DML `RETURNING` statement, draining the
    /// projected rows. Panics unless the plan produced [`Execution::ReturningRows`].
    fn run_returning(
        engine: &Arc<dyn TableEngine>,
        sql: &str,
    ) -> (Vec<OutputColumn>, Vec<Tuple>, DmlVerb) {
        use crabgresql_parser::ast;
        let stmts = test_ok(crabgresql_parser::parse(sql));
        let catalog: Arc<dyn crabgresql_storage_api::TypeCatalog> =
            Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
        let logical = match &stmts[0] {
            ast::Statement::Insert(insert) => {
                test_ok(crabgresql_binder::bind_insert(engine, &catalog, insert))
            }
            ast::Statement::Update(update) => {
                test_ok(crabgresql_binder::bind_update(engine, &catalog, update))
            }
            ast::Statement::Delete(delete) => {
                test_ok(crabgresql_binder::bind_delete(engine, &catalog, delete))
            }
            other => panic!("expected a DML statement, got {other:?}"),
        };
        let physical = crabgresql_planner::plan(logical);
        let Execution::ReturningRows {
            columns,
            mut node,
            verb,
        } = test_ok(execute(physical, &ExecContext::default(), &wtxn()))
        else {
            panic!("expected ReturningRows");
        };
        (columns, collect(node.as_mut()), verb)
    }

    #[test]
    fn insert_returning_projects_inserted_rows() {
        let engine = returning_engine();
        let (columns, rows, verb) = run_returning(
            &engine,
            "INSERT INTO t (id, label) VALUES (10, 'ten'), (11, 'eleven') RETURNING id, label",
        );
        assert_eq!(verb, DmlVerb::Insert);
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "label"]);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(10), Value::Text("ten".into())],
                vec![Value::Int4(11), Value::Text("eleven".into())],
            ]
        );
        // The rows were actually persisted, not just projected.
        let table = test_ok(engine.open_table("t"));
        assert_eq!(table.scan(&rtxn()).count(), 5);
    }

    #[test]
    fn insert_returning_star_and_computed_alias() {
        let engine = returning_engine();
        let (columns, rows, _) = run_returning(
            &engine,
            "INSERT INTO t (id, label) VALUES (10, 'ten') RETURNING *, id + 1 AS next",
        );
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "label", "next"]);
        assert_eq!(
            rows,
            vec![vec![
                Value::Int4(10),
                Value::Text("ten".into()),
                Value::Int4(11),
            ]]
        );
    }

    /// Parse → bind → plan → execute a non-RETURNING INSERT, returning the
    /// inserted row count. Panics unless the plan produced [`Execution::Inserted`].
    fn run_insert(engine: &Arc<dyn TableEngine>, sql: &str) -> u64 {
        use crabgresql_parser::ast;
        let stmts = test_ok(crabgresql_parser::parse(sql));
        let catalog: Arc<dyn crabgresql_storage_api::TypeCatalog> =
            Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
        let ast::Statement::Insert(insert) = &stmts[0] else {
            panic!("expected an INSERT statement");
        };
        let logical = test_ok(crabgresql_binder::bind_insert(engine, &catalog, insert));
        let physical = crabgresql_planner::plan(logical);
        match test_ok(execute(physical, &ExecContext::default(), &wtxn())) {
            Execution::Inserted(n) => n,
            _ => panic!("expected Inserted"),
        }
    }

    #[test]
    fn insert_select_copies_rows() {
        let engine = returning_engine();
        // `INSERT ... SELECT` from the same table doubles it (the source is drained
        // under the snapshot before any write, so it never sees its own inserts).
        let inserted = run_insert(&engine, "INSERT INTO t (id, label) SELECT id, label FROM t");
        assert_eq!(inserted, 3);
        let table = test_ok(engine.open_table("t"));
        assert_eq!(table.scan(&rtxn()).count(), 6);
    }

    #[test]
    fn insert_select_honors_order_by_and_limit() {
        let engine = returning_engine();
        let inserted = run_insert(
            &engine,
            "INSERT INTO t (id, label) SELECT id, label FROM t ORDER BY id DESC LIMIT 1",
        );
        assert_eq!(inserted, 1);
        let table = test_ok(engine.open_table("t"));
        assert_eq!(table.scan(&rtxn()).count(), 4);
    }

    #[test]
    fn insert_table_source_copies_rows() {
        let engine = returning_engine();
        let inserted = run_insert(&engine, "INSERT INTO t (id, label) TABLE t");
        assert_eq!(inserted, 3);
        let table = test_ok(engine.open_table("t"));
        assert_eq!(table.scan(&rtxn()).count(), 6);
    }

    #[test]
    fn insert_select_projects_returning() {
        let engine = returning_engine();
        let (columns, rows, verb) = run_returning(
            &engine,
            "INSERT INTO t (id, label) SELECT id, label FROM t WHERE id = 1 RETURNING id, label",
        );
        assert_eq!(verb, DmlVerb::Insert);
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "label"]);
        assert_eq!(rows, vec![vec![Value::Int4(1), Value::Text("one".into())]]);
    }

    #[test]
    fn update_returning_projects_new_rows() {
        let engine = returning_engine();
        let (columns, rows, verb) = run_returning(
            &engine,
            "UPDATE t SET id = id + 100 WHERE id > 1 RETURNING id, label",
        );
        assert_eq!(verb, DmlVerb::Update);
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "label"]);
        // The NEW (post-update) id values.
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(102), Value::Text("two".into())],
                vec![Value::Int4(103), Value::Text("three".into())],
            ]
        );
    }

    #[test]
    fn delete_returning_projects_deleted_rows_reordered() {
        let engine = returning_engine();
        let (columns, rows, verb) =
            run_returning(&engine, "DELETE FROM t WHERE id > 1 RETURNING label, id");
        assert_eq!(verb, DmlVerb::Delete);
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["label", "id"]);
        // The deleted (OLD) rows, columns reordered as requested.
        assert_eq!(
            rows,
            vec![
                vec![Value::Text("two".into()), Value::Int4(2)],
                vec![Value::Text("three".into()), Value::Int4(3)],
            ]
        );
        let table = test_ok(engine.open_table("t"));
        assert_eq!(table.scan(&rtxn()).count(), 1);
    }

    /// Parse → bind → plan → execute a query against a fresh engine.
    fn run_rows(sql: &str) -> (Vec<OutputColumn>, Vec<Tuple>) {
        run_rows_on(
            &(crabgresql_pg_engine::ephemeral_engine() as Arc<dyn TableEngine>),
            sql,
        )
    }

    /// As [`run_rows`], but against a caller-provided engine (for queries over
    /// real tables).
    fn run_rows_on(engine: &Arc<dyn TableEngine>, sql: &str) -> (Vec<OutputColumn>, Vec<Tuple>) {
        let stmts = test_ok(crabgresql_parser::parse(sql));
        let crabgresql_parser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected a query");
        };
        let catalog: Arc<dyn crabgresql_storage_api::TypeCatalog> =
            Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
        let logical = test_ok(crabgresql_binder::bind_query(engine, &catalog, query));
        let physical = crabgresql_planner::plan(logical);
        let Execution::Rows { columns, mut node } =
            test_ok(execute(physical, &ExecContext::default(), &rtxn()))
        else {
            panic!("expected rows");
        };
        let mut rows = Vec::new();
        while let Some(tuple) = test_ok(node.next()) {
            rows.push(tuple);
        }
        (columns, rows)
    }

    /// An engine with `t(a int, b int)` seeded with two groups (a=1,2) plus a
    /// singleton (a=3), and one NULL `b`.
    fn agg_engine() -> Arc<dyn TableEngine> {
        let engine = crabgresql_pg_engine::ephemeral_engine();
        let table = test_ok(engine.create_table(TableSchema::in_namespace(
            "t",
            "public",
            vec![
                Column::new("a", PgType::Int4),
                Column::new("b", PgType::Int4),
            ],
        )));
        let txn = wtxn();
        let seed: [(i32, Option<i32>); 5] = [
            (1, Some(10)),
            (1, Some(20)),
            (2, Some(5)),
            (2, None),
            (3, Some(7)),
        ];
        for (a, b) in seed {
            let b = b.map(Value::Int4).unwrap_or(Value::Null);
            table.insert(vec![Value::Int4(a), b], &txn);
        }
        engine
    }

    #[test]
    fn whole_table_aggregates_ignore_nulls() {
        let engine = agg_engine();
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT count(*), count(b), min(b), max(b), sum(b) FROM t",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            vec![
                Value::Int8(5),  // count(*) — every row
                Value::Int8(4),  // count(b) — NULL skipped
                Value::Int4(5),  // min(b)
                Value::Int4(20), // max(b)
                Value::Int8(42), // sum(b): int4 widens to bigint
            ]
        );
    }

    #[test]
    fn fromless_distinct_on_hidden_expression() {
        // A FROM-less `SELECT DISTINCT ON (expr)` where the ON expression is not
        // in the select list appends a hidden column; the Values pipeline must
        // keep that column through the sort (not trim to the visible width) so
        // Distinct can read it. Regression: previously truncated → out-of-bounds.
        let (columns, rows) = run_rows("SELECT DISTINCT ON (1 + 1) 5 ORDER BY 1 + 1");
        assert_eq!(columns.len(), 1, "the hidden ON column never reaches output");
        assert_eq!(rows, vec![vec![Value::Int4(5)]]);
    }

    #[test]
    fn distinct_aggregates_deduplicate_per_group_and_per_call() -> anyhow::Result<()> {
        let engine = crabgresql_pg_engine::ephemeral_engine();
        let table = engine.create_table(TableSchema::in_namespace(
            "d",
            "public",
            vec![
                Column::new("g", PgType::Int4),
                Column::new("v", PgType::Int4),
            ],
        ))?;
        let txn = wtxn();
        for (g, v) in [
            (1, Some(10)),
            (1, Some(10)),
            (1, None),
            (2, Some(5)),
            (2, Some(5)),
            (2, None),
        ] {
            table.insert(
                vec![Value::Int4(g), v.map(Value::Int4).unwrap_or(Value::Null)],
                &txn,
            );
        }
        let engine: Arc<dyn TableEngine> = engine;
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT g, count(DISTINCT v), sum(DISTINCT v), avg(DISTINCT v), min(DISTINCT v), max(DISTINCT v) FROM d GROUP BY g ORDER BY g",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(
            &rows[0][..3],
            &[Value::Int4(1), Value::Int8(1), Value::Int8(10)]
        );
        assert_eq!(&rows[0][4..], &[Value::Int4(10), Value::Int4(10)]);
        assert_eq!(
            &rows[1][..3],
            &[Value::Int4(2), Value::Int8(1), Value::Int8(5)]
        );
        assert_eq!(&rows[1][4..], &[Value::Int4(5), Value::Int4(5)]);
        let Value::Numeric(avg_one) = &rows[0][3] else {
            panic!("avg(int) should be numeric, got {:?}", rows[0][3]);
        };
        let Value::Numeric(avg_two) = &rows[1][3] else {
            panic!("avg(int) should be numeric, got {:?}", rows[1][3]);
        };
        assert_eq!(avg_one.to_display(), "10.0000000000000000");
        assert_eq!(avg_two.to_display(), "5.0000000000000000");

        // The two calls use independent seen-value sets, even when their
        // inputs have the same type and values.
        let (_c, rows) = run_rows_on(&engine, "SELECT count(DISTINCT g), sum(DISTINCT g) FROM d");
        assert_eq!(rows, vec![vec![Value::Int8(2), Value::Int8(3)]]);

        Ok(())
    }

    #[test]
    fn empty_group_is_zero_count_and_null_sum() {
        let engine = agg_engine();
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT count(*), sum(b), min(b) FROM t WHERE a > 100",
        );
        // The implicit group still yields one row.
        assert_eq!(rows, vec![vec![Value::Int8(0), Value::Null, Value::Null]]);
    }

    #[test]
    fn avg_of_integers_is_numeric() {
        let engine = agg_engine();
        let (_c, rows) = run_rows_on(&engine, "SELECT avg(b) FROM t");
        // 42 / 4 = 10.5, as numeric.
        let Value::Numeric(n) = &rows[0][0] else {
            panic!("avg(int) should be numeric, got {:?}", rows[0][0]);
        };
        assert_eq!(n.to_display(), "10.5000000000000000");
    }

    #[test]
    fn group_by_produces_one_row_per_group() {
        let engine = agg_engine();
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT a, count(*), sum(b) FROM t GROUP BY a ORDER BY a",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int8(2), Value::Int8(30)],
                vec![Value::Int4(2), Value::Int8(2), Value::Int8(5)],
                vec![Value::Int4(3), Value::Int8(1), Value::Int8(7)],
            ]
        );
    }

    #[test]
    fn having_filters_groups() {
        let engine = agg_engine();
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT a FROM t GROUP BY a HAVING count(*) > 1 ORDER BY a",
        );
        assert_eq!(rows, vec![vec![Value::Int4(1)], vec![Value::Int4(2)]]);
    }

    #[test]
    fn from_less_count_star_is_one() {
        let (_c, rows) = run_rows("SELECT count(*)");
        assert_eq!(rows, vec![vec![Value::Int8(1)]]);
    }

    #[test]
    fn max_minus_min_over_aggregate_row() {
        let engine = agg_engine();
        let (_c, rows) = run_rows_on(&engine, "SELECT max(b) - min(b) AS span FROM t");
        assert_eq!(rows, vec![vec![Value::Int4(15)]]);
    }

    #[test]
    fn group_by_null_key_forms_one_group() -> anyhow::Result<()> {
        // Rows with a NULL group key group together (NULL == NULL), distinct from
        // the non-NULL groups. Exercises the hash-grouping NULL path.
        let engine = crabgresql_pg_engine::ephemeral_engine();
        let table = engine.create_table(TableSchema::in_namespace(
            "g",
            "public",
            vec![
                Column::new("k", PgType::Int4),
                Column::new("v", PgType::Int4),
            ],
        ))?;
        let txn = wtxn();
        for (k, v) in [(Some(1), 10), (None, 20), (Some(1), 5), (None, 7)] {
            let k = k.map(Value::Int4).unwrap_or(Value::Null);
            table.insert(vec![k, Value::Int4(v)], &txn);
        }
        let engine: Arc<dyn TableEngine> = engine;
        // ORDER BY k with NULLS LAST-for-ASC (PG default) so the row order is fixed.
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT k, count(*), sum(v) FROM g GROUP BY k ORDER BY k",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int8(2), Value::Int8(15)],
                vec![Value::Null, Value::Int8(2), Value::Int8(27)],
            ]
        );

        Ok(())
    }

    #[test]
    fn group_by_float_treats_neg_zero_and_nan_like_pg() -> anyhow::Result<()> {
        // -0.0 groups with 0.0, and NaN groups with NaN — the hash and keys_equal
        // must agree on both. Two 0.0-family rows, two NaN rows.
        let engine = crabgresql_pg_engine::ephemeral_engine();
        let table = engine.create_table(TableSchema::in_namespace(
            "f",
            "public",
            vec![Column::new("x", PgType::Float8)],
        ))?;
        let txn = wtxn();
        for x in [0.0_f64, -0.0, f64::NAN, f64::NAN] {
            table.insert(vec![Value::Float8(x)], &txn);
        }
        let engine: Arc<dyn TableEngine> = engine;
        let (_c, rows) = run_rows_on(&engine, "SELECT count(*) FROM f GROUP BY x");
        // Exactly two groups (the 0.0 family and the NaN family), each of size 2.
        let mut counts: Vec<Value> = rows.into_iter().map(|r| r[0].clone()).collect();
        counts.sort_by_key(|v| match v {
            Value::Int8(n) => *n,
            _ => -1,
        });
        assert_eq!(counts, vec![Value::Int8(2), Value::Int8(2)]);

        let (_c, rows) = run_rows_on(&engine, "SELECT count(DISTINCT x) FROM f");
        assert_eq!(rows, vec![vec![Value::Int8(2)]]);

        Ok(())
    }

    #[test]
    fn pg_input_error_info_reports_range_error() {
        let (columns, rows) = run_rows("SELECT * FROM pg_input_error_info('1e400', 'float4')");
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
        let (_columns, rows) = run_rows("SELECT * FROM pg_input_error_info('34.5', 'float4')");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], vec![Value::Null; 4]);
    }

    /// Drain a query, returning the first runtime error (SRF errors surface on
    /// the first `next()`, not at plan time).
    fn run_err(sql: &str) -> ExecError {
        let engine: Arc<dyn TableEngine> = crabgresql_pg_engine::ephemeral_engine();
        let stmts = test_ok(crabgresql_parser::parse(sql));
        let crabgresql_parser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected a query");
        };
        let catalog: Arc<dyn crabgresql_storage_api::TypeCatalog> =
            Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
        let logical = test_ok(crabgresql_binder::bind_query(&engine, &catalog, query));
        let physical = crabgresql_planner::plan(logical);
        let Execution::Rows { mut node, .. } =
            test_ok(execute(physical, &ExecContext::default(), &rtxn()))
        else {
            panic!("expected rows");
        };
        loop {
            match node.next() {
                Ok(Some(_)) => continue,
                Ok(None) => panic!("expected a runtime error for: {sql}"),
                Err(e) => return e,
            }
        }
    }

    /// The single `generate_series` column, extracted from result tuples.
    fn series_col(rows: &[Tuple]) -> Vec<Value> {
        rows.iter().map(|r| r[0].clone()).collect()
    }

    #[test]
    fn generate_series_from_yields_int4_range() {
        let (columns, rows) = run_rows("SELECT * FROM generate_series(1, 5)");
        assert_eq!(columns[0].name, "generate_series");
        assert_eq!(columns[0].ty, PgType::Int4);
        assert_eq!(
            series_col(&rows),
            (1..=5).map(Value::Int4).collect::<Vec<_>>()
        );
    }

    #[test]
    fn generate_series_target_list_yields_rows() {
        let (columns, rows) = run_rows("SELECT generate_series(1, 5)");
        assert_eq!(columns[0].name, "generate_series");
        assert_eq!(
            series_col(&rows),
            (1..=5).map(Value::Int4).collect::<Vec<_>>()
        );
    }

    #[test]
    fn generate_series_step_and_direction() {
        let (_c, rows) = run_rows("SELECT generate_series(1, 10, 3)");
        assert_eq!(series_col(&rows), [1, 4, 7, 10].map(Value::Int4));
        // A descending series with a negative step.
        let (_c, rows) = run_rows("SELECT generate_series(5, 1, -2)");
        assert_eq!(series_col(&rows), [5, 3, 1].map(Value::Int4));
    }

    #[test]
    fn generate_series_empty_ranges_yield_no_rows() {
        // Ascending series with start > stop.
        let (_c, rows) = run_rows("SELECT generate_series(5, 1)");
        assert!(rows.is_empty());
        // Positive step in the wrong direction.
        let (_c, rows) = run_rows("SELECT generate_series(5, 1, 1)");
        assert!(rows.is_empty());
    }

    #[test]
    fn generate_series_zero_step_is_22023() {
        let e = run_err("SELECT generate_series(1, 5, 0)");
        assert_eq!(e.code, "22023");
        assert_eq!(e.message, "step size cannot equal zero");
    }

    #[test]
    fn generate_series_int8_range() {
        let (columns, rows) = run_rows("SELECT generate_series(1, 5000000001, 2500000000)");
        assert_eq!(columns[0].ty, PgType::Int8);
        assert_eq!(
            series_col(&rows),
            [1_i64, 2_500_000_001, 5_000_000_001].map(Value::Int8)
        );
    }

    #[test]
    fn generate_series_mixed_with_scalar_over_table() {
        let engine = engine_with_nums(); // nums(n int4) = 1, 2, 3
        let (columns, rows) = run_rows_on(&engine, "SELECT n, generate_series(1, 2) FROM nums");
        assert_eq!(columns.len(), 2);
        // Each of the 3 input rows expands to 2 output rows (scalar repeats).
        assert_eq!(rows.len(), 6);
        let pairs: Vec<(Value, Value)> =
            rows.iter().map(|r| (r[0].clone(), r[1].clone())).collect();
        assert!(pairs.contains(&(Value::Int4(1), Value::Int4(1))));
        assert!(pairs.contains(&(Value::Int4(1), Value::Int4(2))));
        assert!(pairs.contains(&(Value::Int4(3), Value::Int4(2))));
    }

    /// The single generate_series column, rendered as PG-formatted text.
    fn series_text(rows: &[Tuple]) -> Vec<String> {
        rows.iter()
            .map(|r| r[0].encode_text_with(1).unwrap_or_default())
            .collect()
    }

    #[test]
    fn generate_series_numeric_range_keeps_scale() {
        // The start keeps its scale ("1"); adding 0.5 gives scale 1 thereafter.
        let (columns, rows) = run_rows("SELECT generate_series(1, 3, 0.5)");
        assert_eq!(columns[0].ty, PgType::Numeric);
        assert_eq!(series_text(&rows), ["1", "1.5", "2.0", "2.5", "3.0"]);
    }

    #[test]
    fn generate_series_numeric_default_step_and_backward() {
        // 2-arg numeric defaults the step to 1.
        let (_c, rows) = run_rows("SELECT generate_series(1.5, 3)");
        assert_eq!(series_text(&rows), ["1.5", "2.5"]);
        // A negative numeric step counts down.
        let (_c, rows) = run_rows("SELECT generate_series(3.0, 1.0, -0.5)");
        assert_eq!(series_text(&rows), ["3.0", "2.5", "2.0", "1.5", "1.0"]);
    }

    #[test]
    fn generate_series_numeric_nan_bounds_error_22023() {
        for (sql, msg) in [
            (
                "SELECT generate_series('NaN'::numeric, 3)",
                "start value cannot be NaN",
            ),
            (
                "SELECT generate_series(1, 'NaN'::numeric)",
                "stop value cannot be NaN",
            ),
            (
                "SELECT generate_series(1, 3, 'NaN'::numeric)",
                "step size cannot be NaN",
            ),
        ] {
            let e = run_err(sql);
            assert_eq!(e.code, "22023", "{sql}");
            assert_eq!(e.message, msg, "{sql}");
        }
    }

    #[test]
    fn generate_series_numeric_infinite_bounds_error_22023() {
        // Infinite bounds/step are rejected (bounds "cannot be infinity", the
        // step "cannot be infinite") rather than looping forever.
        for (sql, msg) in [
            (
                "SELECT generate_series('infinity'::numeric, 3)",
                "start value cannot be infinity",
            ),
            (
                "SELECT generate_series(1, 'infinity'::numeric)",
                "stop value cannot be infinity",
            ),
            (
                "SELECT generate_series(1, 3, 'infinity'::numeric)",
                "step size cannot be infinity",
            ),
        ] {
            let e = run_err(sql);
            assert_eq!(e.code, "22023", "{sql}");
            assert_eq!(e.message, msg, "{sql}");
        }
    }

    #[test]
    fn generate_series_null_argument_short_circuits_before_validation() {
        // `generate_series` is strict: a NULL argument yields 0 rows before any
        // NaN / infinity / zero-step validation fires.
        for sql in [
            "SELECT generate_series(NULL::int, 5, 0)",
            "SELECT generate_series(NULL::numeric, 'NaN'::numeric)",
            "SELECT generate_series(NULL::numeric, 'infinity'::numeric)",
            "SELECT generate_series(1, 3, NULL::numeric)",
            "SELECT generate_series(NULL::timestamp, timestamp '2020-01-05', interval '0')",
        ] {
            let (_c, rows) = run_rows(sql);
            assert!(rows.is_empty(), "{sql} should yield no rows");
        }
    }

    #[test]
    fn generate_series_timestamp_forward_and_backward() {
        let (columns, rows) = run_rows(
            "SELECT generate_series(timestamp '2020-01-01', timestamp '2020-01-04', \
             interval '1 day')",
        );
        assert_eq!(columns[0].ty, PgType::Timestamp);
        assert_eq!(
            series_text(&rows),
            [
                "2020-01-01 00:00:00",
                "2020-01-02 00:00:00",
                "2020-01-03 00:00:00",
                "2020-01-04 00:00:00",
            ]
        );
        // A negative interval steps backward.
        let (_c, rows) = run_rows(
            "SELECT generate_series(timestamp '2020-01-03', timestamp '2020-01-01', \
             interval '-1 day')",
        );
        assert_eq!(
            series_text(&rows),
            [
                "2020-01-03 00:00:00",
                "2020-01-02 00:00:00",
                "2020-01-01 00:00:00"
            ]
        );
    }

    #[test]
    fn generate_series_timestamp_month_step_clamps_day() {
        // pl_interval clamps the day-of-month, incrementally from cur.
        let (_c, rows) = run_rows(
            "SELECT generate_series(timestamp '2020-01-31', timestamp '2020-04-30', \
             interval '1 month')",
        );
        assert_eq!(
            series_text(&rows),
            [
                "2020-01-31 00:00:00",
                "2020-02-29 00:00:00",
                "2020-03-29 00:00:00",
                "2020-04-29 00:00:00",
            ]
        );
    }

    #[test]
    fn generate_series_timestamp_zero_interval_is_22023() {
        let e = run_err(
            "SELECT generate_series(timestamp '2020-01-01', timestamp '2020-01-05', interval '0')",
        );
        assert_eq!(e.code, "22023");
        assert_eq!(e.message, "step size cannot equal zero");
    }

    #[test]
    fn generate_series_timestamp_overflow_is_22008() {
        // Stepping past the max timestamp raises rather than silently stopping.
        let e = run_err(
            "SELECT generate_series(timestamp '294276-12-30', timestamp '294276-12-31', \
             interval '1 day')",
        );
        assert_eq!(e.code, "22008");
        assert_eq!(e.message, "timestamp out of range");
    }

    #[test]
    fn generate_series_timestamptz_forward_range() {
        let (columns, rows) = run_rows(
            "SELECT generate_series(timestamptz '2020-01-01 00:00+00', \
             timestamptz '2020-01-03 00:00+00', interval '1 day')",
        );
        assert_eq!(columns[0].ty, PgType::TimestampTz);
        assert_eq!(
            series_text(&rows),
            [
                "2020-01-01 00:00:00+00",
                "2020-01-02 00:00:00+00",
                "2020-01-03 00:00:00+00",
            ]
        );
    }

    /// A `nums(n int4)` table seeded with 1, 2, 3.
    fn engine_with_nums() -> Arc<dyn TableEngine> {
        let engine: Arc<dyn TableEngine> = crabgresql_pg_engine::ephemeral_engine();
        let table = test_ok(engine.create_table(TableSchema::in_namespace(
            "nums",
            "public",
            vec![Column::new("n", PgType::Int4)],
        )));
        let txn = wtxn();
        for n in [1, 2, 3] {
            table.insert(vec![Value::Int4(n)], &txn);
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

    #[test]
    fn aggregate_over_derived_table() {
        let engine = engine_with_nums();
        // sum() over a derived table (subquery in FROM): the FROM form that used
        // to error with "aggregates over this FROM form are not supported yet".
        let (columns, rows) = run_rows_on(
            &engine,
            "SELECT sum(n) FROM (SELECT n FROM nums WHERE n <> 2) s",
        );
        assert_eq!(columns.len(), 1);
        assert_eq!(rows, vec![vec![Value::Int8(4)]]);
    }

    #[test]
    fn grouped_aggregate_over_derived_table() {
        let engine = agg_engine();
        let (columns, rows) = run_rows_on(
            &engine,
            "SELECT a, sum(b) FROM (SELECT a, b FROM t) s GROUP BY a ORDER BY a",
        );
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "sum"]);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int8(30)],
                vec![Value::Int4(2), Value::Int8(5)],
                vec![Value::Int4(3), Value::Int8(7)],
            ]
        );
    }

    #[test]
    fn aggregate_over_values_in_from() {
        // count/sum over an inline VALUES relation in FROM.
        let (_c, rows) = run_rows("SELECT count(*), sum(x) FROM (VALUES (1), (2), (3)) v(x)");
        assert_eq!(rows, vec![vec![Value::Int8(3), Value::Int8(6)]]);
    }

    #[test]
    fn aggregate_over_cte_reference() {
        let engine = engine_with_nums();
        let (_c, rows) = run_rows_on(
            &engine,
            "WITH big AS (SELECT n FROM nums WHERE n >= 2) SELECT sum(n) FROM big",
        );
        assert_eq!(rows, vec![vec![Value::Int8(5)]]);
    }

    #[test]
    fn aggregate_over_set_returning_function() {
        // count/sum over generate_series() as a FROM set-returning function.
        let (_c, rows) = run_rows("SELECT count(*), sum(generate_series) FROM generate_series(1, 3)");
        assert_eq!(rows, vec![vec![Value::Int8(3), Value::Int8(6)]]);
    }

    #[test]
    fn cross_join_of_values_is_cartesian_in_pg_order() {
        // First relation outermost (slowest), last relation innermost (fastest).
        let (columns, rows) =
            run_rows("SELECT * FROM (VALUES (1), (2)) a(x), (VALUES (10), (20)) b(y)");
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["x", "y"]);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int4(10)],
                vec![Value::Int4(1), Value::Int4(20)],
                vec![Value::Int4(2), Value::Int4(10)],
                vec![Value::Int4(2), Value::Int4(20)],
            ]
        );
    }

    #[test]
    fn cross_join_over_real_tables_with_join_predicate() {
        let engine = engine_with_nums();
        let (_columns, rows) = run_rows_on(
            &engine,
            "SELECT a.n, b.n FROM nums a, nums b WHERE a.n = b.n ORDER BY 1",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int4(1)],
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(3), Value::Int4(3)],
            ]
        );
    }

    #[test]
    fn explicit_cross_join_matches_comma_semantics() {
        let (_columns, rows) =
            run_rows("SELECT * FROM (VALUES (1)) a(x) CROSS JOIN (VALUES (7), (8)) b(y)");
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int4(7)],
                vec![Value::Int4(1), Value::Int4(8)],
            ]
        );
    }

    #[test]
    fn cross_join_with_an_empty_relation_yields_no_rows() {
        // The inner relation is empty, so the product is empty.
        let (_columns, rows) =
            run_rows("SELECT * FROM (VALUES (1), (2)) a(x), (SELECT 1 WHERE false) b(z)");
        assert!(rows.is_empty());
    }

    #[test]
    fn inner_join_matches_duplicates_and_rejects_null_predicates() {
        let (_columns, rows) = run_rows(
            "SELECT a.x, b.y \
             FROM (VALUES (1), (2), (NULL)) a(x) \
             JOIN (VALUES (2), (2), (3), (NULL)) b(y) ON a.x = b.y",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(2), Value::Int4(2)],
            ]
        );
    }

    #[test]
    fn left_right_and_full_join_null_extend_unmatched_rows() {
        let values = "(VALUES (1), (2), (NULL)) a(x) JOIN_KIND \
             (VALUES (2), (2), (3), (NULL)) b(y) ON a.x = b.y";
        let query =
            |kind: &str| format!("SELECT a.x, b.y FROM {}", values.replace("JOIN_KIND", kind));

        let (_, left) = run_rows(&query("LEFT JOIN"));
        assert_eq!(
            left,
            vec![
                vec![Value::Int4(1), Value::Null],
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Null, Value::Null],
            ]
        );

        let (_, right) = run_rows(&query("RIGHT JOIN"));
        assert_eq!(
            right,
            vec![
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Null, Value::Int4(3)],
                vec![Value::Null, Value::Null],
            ]
        );

        let (_, full) = run_rows(&query("FULL JOIN"));
        assert_eq!(
            full,
            vec![
                vec![Value::Int4(1), Value::Null],
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Null, Value::Null],
                vec![Value::Null, Value::Int4(3)],
                vec![Value::Null, Value::Null],
            ]
        );
    }

    #[test]
    fn outer_join_handles_empty_preserved_side() {
        let (_, right) = run_rows(
            "SELECT a.x, b.y FROM (SELECT 1 WHERE false) a(x) \
             RIGHT JOIN (VALUES (7), (8)) b(y) ON true",
        );
        assert_eq!(
            right,
            vec![
                vec![Value::Null, Value::Int4(7)],
                vec![Value::Null, Value::Int4(8)],
            ]
        );
        let (_, left) = run_rows(
            "SELECT a.x, b.y FROM (VALUES (7), (8)) a(x) \
             LEFT JOIN (SELECT 1 WHERE false) b(y) ON true",
        );
        assert_eq!(
            left,
            vec![
                vec![Value::Int4(7), Value::Null],
                vec![Value::Int4(8), Value::Null],
            ]
        );
    }

    #[test]
    fn chained_outer_join_predicate_sees_null_extended_left_row() {
        let (_, rows) = run_rows(
            "SELECT a.x, b.y, c.z FROM (VALUES (1)) a(x) \
             LEFT JOIN (VALUES (9)) b(y) ON false \
             JOIN (VALUES (2)) c(z) ON b.y IS NULL",
        );
        assert_eq!(
            rows,
            vec![vec![Value::Int4(1), Value::Null, Value::Int4(2)]]
        );
    }

    #[test]
    fn aggregates_and_grouping_consume_outer_join_rows() {
        let (_, rows) = run_rows(
            "SELECT count(*), count(b.y) \
             FROM (VALUES (1), (2), (NULL)) a(x) \
             LEFT JOIN (VALUES (2), (2), (3), (NULL)) b(y) ON a.x = b.y",
        );
        assert_eq!(rows, vec![vec![Value::Int8(4), Value::Int8(2)]]);

        let (_, grouped) = run_rows(
            "SELECT a.x, count(b.y) \
             FROM (VALUES (1), (2), (NULL)) a(x) \
             LEFT JOIN (VALUES (2), (2)) b(y) ON a.x = b.y \
             GROUP BY a.x HAVING count(*) >= 1 ORDER BY a.x",
        );
        assert_eq!(
            grouped,
            vec![
                vec![Value::Int4(1), Value::Int8(0)],
                vec![Value::Int4(2), Value::Int8(2)],
                vec![Value::Null, Value::Int8(0)],
            ]
        );
    }

    #[test]
    fn hash_join_matches_duplicate_keys_on_both_sides() {
        // Two left rows and two right rows share key 2, so the equi-join emits
        // their 2×2 cross product — left-driven, right rows in input order.
        let (_columns, rows) = run_rows(
            "SELECT a.x, b.y \
             FROM (VALUES (1), (2), (2)) a(x) \
             JOIN (VALUES (2), (2), (3)) b(y) ON a.x = b.y",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(2), Value::Int4(2)],
                vec![Value::Int4(2), Value::Int4(2)],
            ]
        );
    }

    #[test]
    fn hash_join_on_composite_key() {
        // Both key columns must match; (1,20) matches only the right (1,20) row.
        let (_columns, rows) = run_rows(
            "SELECT a.x, a.y, b.z \
             FROM (VALUES (1, 10), (1, 20)) a(x, y) \
             JOIN (VALUES (1, 20, 100), (1, 10, 200), (2, 20, 300)) b(x, y, z) \
             ON a.x = b.x AND a.y = b.y \
             ORDER BY a.y",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int4(10), Value::Int4(200)],
                vec![Value::Int4(1), Value::Int4(20), Value::Int4(100)],
            ]
        );
    }

    #[test]
    fn hash_join_applies_residual_predicate() {
        // `a.x = b.x` is the hash key; `a.v < b.v` is a residual conjunct checked
        // per candidate pair. Only the pair (x=1, 5 < 9) survives.
        let (_columns, rows) = run_rows(
            "SELECT a.v, b.v \
             FROM (VALUES (1, 5), (2, 9)) a(x, v) \
             JOIN (VALUES (1, 9), (1, 3), (2, 1)) b(x, v) ON a.x = b.x AND a.v < b.v",
        );
        assert_eq!(rows, vec![vec![Value::Int4(5), Value::Int4(9)]]);
    }

    #[test]
    fn hash_join_left_outer_with_residual_null_extends() {
        // A LEFT join whose ON has a residual: a left row is null-extended when no
        // right row satisfies the *whole* ON (key equality AND the residual).
        let (_columns, rows) = run_rows(
            "SELECT a.x, b.v \
             FROM (VALUES (1, 5), (2, 9)) a(x, v) \
             LEFT JOIN (VALUES (1, 9), (2, 1)) b(x, v) ON a.x = b.x AND a.v < b.v \
             ORDER BY a.x",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int4(9)],
                // x=2 has a key match (b.x=2) but 9 < 1 is false, so null-extended.
                vec![Value::Int4(2), Value::Null],
            ]
        );
    }

    #[test]
    fn equi_join_on_money_matches_correctly() {
        // money hashes distinctly now, so this runs as a hash join.
        let (_columns, rows) = run_rows(
            "SELECT a.x, b.y \
             FROM (VALUES ('$1.00'::money), ('$2.00'::money)) a(x) \
             JOIN (VALUES ('$2.00'::money), ('$3.00'::money)) b(y) ON a.x = b.y",
        );
        assert_eq!(rows, vec![vec![Value::Money(200), Value::Money(200)]]);
    }

    #[test]
    fn equi_join_on_interval_matches_via_nested_loop_fallback() {
        // interval is not hash-distinct, so the planner keeps this as a nested
        // loop; the result must still be correct (and NULL keys still excluded).
        let (_columns, rows) = run_rows(
            "SELECT a.x, b.y \
             FROM (VALUES ('1 day'::interval), ('24 hours'::interval), (NULL)) a(x) \
             JOIN (VALUES ('1 day'::interval), ('2 days'::interval)) b(y) ON a.x = b.y \
             ORDER BY a.x",
        );
        // '1 day' and '24 hours' are equal intervals, so both non-null left rows
        // match the single '1 day' right row; the NULL left key matches nothing.
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| !matches!(r[1], Value::Null)));
    }

    #[test]
    fn hash_join_coerces_mixed_width_keys() {
        // int4 = int8 promotes the int4 side to int8; both sides must hash under
        // the same promoted type for the match to be found.
        let engine = engine_with_nums();
        let (_columns, rows) = run_rows_on(
            &engine,
            "SELECT a.n, b.big FROM nums a \
             JOIN (VALUES (1::int8), (3::int8)) b(big) ON a.n = b.big ORDER BY a.n",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int8(1)],
                vec![Value::Int4(3), Value::Int8(3)],
            ]
        );
    }
}
