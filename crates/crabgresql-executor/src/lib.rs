//! Volcano (iterator) executor.
//!
//! Nodes: `Values`, `SeqScan`, `Filter`, `Projection`; expression evaluation
//! lives in [`eval`]. DML (INSERT/UPDATE/DELETE) runs as plain functions
//! rather than plan nodes: it yields a row count, not a row stream, and the
//! pull model only becomes the right shape for it once RETURNING exists.

mod agg;
pub mod eval;
mod generate_series;
mod md5;
pub mod scalar_fns;
mod special_fns;

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use crabgresql_binder::{BoundAggregate, BoundExpr, JoinKind, SortKey, TableFn};
pub use crabgresql_binder::OutputColumn;
use crabgresql_planner::{PhysicalAggInput, PhysicalJoinExpr, PhysicalJoinInput, PhysicalPlan};
use crabgresql_storage_api::{IndexMetadata, TableAm, Tid, Tuple};
use crabgresql_txn::TxnContext;
use crabgresql_types::Value;

use eval::eval;
pub use eval::{coerce_value, compare_values};
use generate_series::Series;

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
    /// Optional DETAIL line (e.g. numeric field overflow explains the p/s).
    pub detail: Option<String>,
}

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
}

pub fn execute(
    plan: PhysicalPlan,
    ctx: ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
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
            Box::new(SeqScan::new(&table, txn)),
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
            let Execution::Rows { node, .. } = execute(*source, ctx, txn)? else {
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
        PhysicalPlan::Join {
            source,
            columns,
            projections,
            predicate,
            sort,
        } => {
            let joined = build_join_expr(source, ctx, txn)?;
            project_pipeline(joined, projections, predicate, sort, columns, ctx)
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
        } => {
            // Source rows: a base table scan or the single virtual row of a
            // FROM-less aggregate.
            let source: Box<dyn ExecNode> = match input {
                PhysicalAggInput::Scan(table) => Box::new(SeqScan::new(&table, txn)),
                PhysicalAggInput::Join(source) => build_join_expr(source, ctx, txn)?,
                PhysicalAggInput::SingleRow => {
                    Box::new(Values::new(vec![vec![]], ctx))
                }
            };
            // WHERE filters rows before aggregation.
            let mut node: Box<dyn ExecNode> = match predicate {
                Some(predicate) => Box::new(Filter::new(source, predicate, ctx)),
                None => source,
            };
            node = Box::new(Aggregate::new(node, group_exprs, aggregates, ctx));
            // HAVING filters the per-group rows.
            if let Some(having) = having {
                node = Box::new(Filter::new(node, having, ctx));
            }
            // The projection list and ORDER BY were rewritten to reference the
            // aggregate output row, so the standard tail finishes the job.
            project_pipeline(node, projections, None, sort, columns, ctx)
        }
        PhysicalPlan::Insert { table, rows } => execute_insert(&table, &rows, ctx, txn),
        PhysicalPlan::Update {
            table,
            predicate,
            assignments,
        } => execute_update(&table, &predicate, &assignments, ctx, txn),
        PhysicalPlan::Delete { table, predicate } => execute_delete(&table, &predicate, ctx, txn),
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
    // A set-returning function in the target list turns one input row into many,
    // so it needs `ProjectSet` rather than the one-in/one-out `Projection`.
    node = if projections.iter().any(BoundExpr::is_srf) {
        Box::new(ProjectSet::new(node, projections, ctx))
    } else {
        Box::new(Projection::new(node, projections, ctx))
    };
    node = maybe_sort(node, sort, &columns)?;
    Ok(Execution::Rows { columns, node })
}

/// Statement atomicity: evaluate everything first, mutate only after nothing
/// can fail, so a failure in a later row leaves no earlier rows behind. The
/// writes are stamped with `txn`'s XID and become durable/visible only when the
/// transaction commits.
fn execute_insert(
    table: &Arc<dyn TableAm>,
    rows: &[Vec<BoundExpr>],
    ctx: ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    let mut tuples: Vec<Tuple> = Vec::with_capacity(rows.len());
    let mut visible: Vec<Tuple> = table.scan(txn).map(|(_, tuple)| tuple).collect();
    for row in rows {
        let tuple = row
            .iter()
            .map(|expr| eval(expr, &[], ctx))
            .collect::<Result<_, _>>()?;
        validate_constraints(table, &tuple, visible.iter(), ctx)?;
        visible.push(tuple.clone());
        tuples.push(tuple);
    }
    let inserted = tuples.len() as u64;
    for tuple in tuples {
        table.insert(tuple, txn);
    }
    Ok(Execution::Inserted(inserted))
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
    ctx: ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    let original: Vec<(Tid, Tuple)> = table.scan(txn).collect();
    let mut simulated = original.clone();
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
        let Some(pos) = simulated
            .iter()
            .position(|(candidate, _)| *candidate == tid)
        else {
            continue;
        };
        let (_, removed) = simulated.remove(pos);
        if let Err(error) = validate_constraints(table, &new, simulated.iter().map(|(_, t)| t), ctx)
        {
            simulated.insert(pos, (tid, removed));
            return Err(error);
        }
        simulated.insert(pos, (tid, new.clone()));
        pending.push((tid, new));
    }
    Ok(Execution::Updated(table.update_many(pending, txn)))
}

fn validate_constraints<'a>(
    table: &Arc<dyn TableAm>,
    tuple: &Tuple,
    existing: impl Iterator<Item = &'a Tuple>,
    ctx: ExecContext,
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

fn display_value(value: &Value, ctx: ExecContext) -> String {
    value
        .encode_text_with(ctx.extra_float_digits)
        .unwrap_or_else(|| "null".to_string())
}

fn display_tuple(tuple: &Tuple, ctx: ExecContext) -> String {
    tuple
        .iter()
        .map(|value| display_value(value, ctx))
        .collect::<Vec<_>>()
        .join(", ")
}

/// See the concurrency note on [`execute_update`].
fn execute_delete(
    table: &Arc<dyn TableAm>,
    predicate: &Option<BoundExpr>,
    ctx: ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    let mut pending: Vec<Tid> = Vec::new();
    for (tid, tuple) in table.scan(txn) {
        if predicate_holds(predicate, &tuple, ctx)? {
            pending.push(tid);
        }
    }
    Ok(Execution::Deleted(table.delete_many(pending, txn)))
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
                .map(|expr| eval(expr, &[], self.ctx))
                .collect::<Result<Vec<_>, _>>()?;
            self.state = Some(match self.func {
                TableFn::PgInputErrorInfo => {
                    TableFnState::Single(Some(pg_input_error_info_row(&values)))
                }
                TableFn::GenerateSeries(elem) => {
                    TableFnState::Series(Series::from_args(elem, &values)?)
                }
            });
        }
        Ok(self.state.as_mut().unwrap())
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
    ctx: ExecContext,
    txn: &TxnContext,
) -> Result<Box<dyn ExecNode>, ExecError> {
    Ok(match input {
        PhysicalJoinInput::Scan(table) => Box::new(SeqScan::new(&table, txn)),
        PhysicalJoinInput::TableFunction { func, args } => {
            Box::new(TableFunctionSource::new(func, args, ctx))
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
    ctx: ExecContext,
    txn: &TxnContext,
) -> Result<Box<dyn ExecNode>, ExecError> {
    match source {
        PhysicalJoinExpr::Input { input, .. } => build_join_source(input, ctx, txn),
        PhysicalJoinExpr::Join {
            left,
            right,
            kind,
            predicate,
        } => {
            let left_width = left.width();
            let right_width = right.width();
            let left = build_join_expr(*left, ctx, txn)?;
            let right = build_join_expr(*right, ctx, txn)?;
            Ok(Box::new(NestedLoopJoin::new(
                left,
                right,
                left_width,
                right_width,
                kind,
                predicate,
                ctx,
            )?))
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
                        let row = self.combined_row(
                            self.current_left.as_ref().unwrap(),
                            &self.right_rows[right_index],
                        );
                        let matched = self.kind == JoinKind::Cross
                            || predicate_holds(&self.predicate, &row, self.ctx)?;
                        if matched {
                            self.current_left_matched = true;
                            self.right_matched[right_index] = true;
                            return Ok(Some(row));
                        }
                    }

                    if !self.current_left_matched && self.preserves_left() {
                        let mut row = self.current_left.take().unwrap();
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
                    .map(|e| eval(e, &row, self.ctx))
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
                                .map(|agg| agg.distinct.then(|| agg::DistinctValues::new(agg.input_ty)))
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
                        let v = eval(arg, &row, self.ctx)?;
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
        Ok(self.output.as_mut().unwrap().next())
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
                        .map(|a| eval(a, &input, self.ctx))
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
            let exp = self.current.as_mut().unwrap();

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
                    _ => out.push(eval(expr, &input, self.ctx)?),
                }
            }
            return Ok(Some(out));
        }
    }
}

/// Build the range iterator for a target-list SRF. Only `generate_series` is a
/// set-returning projection today.
fn build_series(func: TableFn, values: &[Value]) -> Result<Series, ExecError> {
    match func {
        TableFn::GenerateSeries(elem) => Series::from_args(elem, values),
        TableFn::PgInputErrorInfo => Err(ExecError::new(
            crabgresql_pg_wire::sqlstate::FEATURE_NOT_SUPPORTED,
            "set-returning function is not supported in this context",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_binder::{BinOp, UnaryOp};
    use crabgresql_memory_storage::MemoryEngine;
    use crabgresql_storage_api::{Column, TableEngine, TableSchema};
    use crabgresql_txn::{CommandId, TransactionManager, TxnContext, Xid};
    use crabgresql_types::PgType;
    use eval::coerce_value;

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
            tm.commit(xid).unwrap();
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
                    Column::new("id", PgType::Int4),
                    Column::new("label", PgType::Text),
                ],
            })
            .unwrap();
        let txn = wtxn();
        table.insert(vec![Value::Int4(1), Value::Text("one".into())], &txn);
        table.insert(vec![Value::Int4(2), Value::Text("two".into())], &txn);
        table.insert(vec![Value::Int4(3), Value::Null], &txn);
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
        let mut node = Filter::new(Box::new(SeqScan::new(&table, &rtxn())), predicate, ExecContext::default());
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
        let mut node = Filter::new(Box::new(SeqScan::new(&table, &rtxn())), predicate, ExecContext::default());
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
        let mut node = Projection::new(Box::new(SeqScan::new(&table, &rtxn())), exprs, ExecContext::default());
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
    fn sort_orders_by_hidden_column_then_trims() {
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
        let projection =
            Projection::new(Box::new(SeqScan::new(&table, &rtxn())), exprs, ExecContext::default());
        // ORDER BY label ASC: NULL (id 3) sorts last (NULLS LAST default), then
        // 'one' (id 1), 'two' (id 2).
        let key = SortKey {
            column: 1,
            ty: PgType::Text,
            asc: true,
            nulls_first: false,
        };
        let mut node = Sort::new(Box::new(projection), vec![key], 1).unwrap();
        assert_eq!(
            collect(&mut node),
            vec![
                vec![Value::Int4(1)],
                vec![Value::Int4(2)],
                vec![Value::Int4(3)],
            ],
            "rows ordered by hidden label, trimmed to the single visible column"
        );
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

    #[test]
    fn limit_applies_after_sort() {
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
        )
        .unwrap();
        let mut node = Limit::new(Box::new(sort), Some(1), None);
        assert_eq!(ids(&mut node), vec![3]);
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
        let Execution::Updated(n) = execute_update(&table, &None, &assignments, ExecContext::default(), &wtxn()).unwrap() else {
            panic!("expected Updated");
        };
        assert_eq!(n, 3);
        let ids: Vec<Value> = table.scan(&rtxn()).map(|(_, t)| t[0].clone()).collect();
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
        let Err(e) = execute_update(&table, &None, &assignments, ExecContext::default(), &wtxn()) else {
            panic!("expected error");
        };
        assert_eq!(e.code, "22012");
        let ids: Vec<Value> = table.scan(&rtxn()).map(|(_, t)| t[0].clone()).collect();
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
        let Execution::Deleted(n) = execute_delete(&table, &predicate, ExecContext::default(), &wtxn()).unwrap() else {
            panic!("expected Deleted");
        };
        assert_eq!(n, 2);
        assert_eq!(table.scan(&rtxn()).count(), 1);
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
        let catalog: Arc<dyn crabgresql_storage_api::TypeCatalog> =
            Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
        let logical = crabgresql_binder::bind_query(engine, &catalog, query).unwrap();
        let physical = crabgresql_planner::plan(logical);
        let Execution::Rows { columns, mut node } =
            execute(physical, ExecContext::default(), &rtxn()).unwrap()
        else {
            panic!("expected rows");
        };
        let mut rows = Vec::new();
        while let Some(tuple) = node.next().unwrap() {
            rows.push(tuple);
        }
        (columns, rows)
    }

    /// An engine with `t(a int, b int)` seeded with two groups (a=1,2) plus a
    /// singleton (a=3), and one NULL `b`.
    fn agg_engine() -> Arc<dyn TableEngine> {
        let engine = MemoryEngine::new();
        let table = engine
            .create_table(TableSchema {
                name: "t".into(),
                columns: vec![Column::new("a", PgType::Int4), Column::new("b", PgType::Int4)],
            })
            .unwrap();
        let txn = wtxn();
        let seed: [(i32, Option<i32>); 5] =
            [(1, Some(10)), (1, Some(20)), (2, Some(5)), (2, None), (3, Some(7))];
        for (a, b) in seed {
            let b = b.map(Value::Int4).unwrap_or(Value::Null);
            table.insert(vec![Value::Int4(a), b], &txn);
        }
        Arc::new(engine)
    }

    #[test]
    fn whole_table_aggregates_ignore_nulls() {
        let engine = agg_engine();
        let (_c, rows) =
            run_rows_on(&engine, "SELECT count(*), count(b), min(b), max(b), sum(b) FROM t");
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
    fn distinct_aggregates_deduplicate_per_group_and_per_call() {
        let engine = MemoryEngine::new();
        let table = engine
            .create_table(TableSchema {
                name: "d".into(),
                columns: vec![Column::new("g", PgType::Int4), Column::new("v", PgType::Int4)],
            })
            .unwrap();
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
        let engine: Arc<dyn TableEngine> = Arc::new(engine);
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
        let (_c, rows) = run_rows_on(
            &engine,
            "SELECT count(DISTINCT g), sum(DISTINCT g) FROM d",
        );
        assert_eq!(rows, vec![vec![Value::Int8(2), Value::Int8(3)]]);
    }

    #[test]
    fn empty_group_is_zero_count_and_null_sum() {
        let engine = agg_engine();
        let (_c, rows) = run_rows_on(&engine, "SELECT count(*), sum(b), min(b) FROM t WHERE a > 100");
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
        let (_c, rows) =
            run_rows_on(&engine, "SELECT a, count(*), sum(b) FROM t GROUP BY a ORDER BY a");
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
    fn group_by_null_key_forms_one_group() {
        // Rows with a NULL group key group together (NULL == NULL), distinct from
        // the non-NULL groups. Exercises the hash-grouping NULL path.
        let engine = MemoryEngine::new();
        let table = engine
            .create_table(TableSchema {
                name: "g".into(),
                columns: vec![Column::new("k", PgType::Int4), Column::new("v", PgType::Int4)],
            })
            .unwrap();
        let txn = wtxn();
        for (k, v) in [(Some(1), 10), (None, 20), (Some(1), 5), (None, 7)] {
            let k = k.map(Value::Int4).unwrap_or(Value::Null);
            table.insert(vec![k, Value::Int4(v)], &txn);
        }
        let engine: Arc<dyn TableEngine> = Arc::new(engine);
        // ORDER BY k with NULLS LAST-for-ASC (PG default) so the row order is fixed.
        let (_c, rows) = run_rows_on(&engine, "SELECT k, count(*), sum(v) FROM g GROUP BY k ORDER BY k");
        assert_eq!(
            rows,
            vec![
                vec![Value::Int4(1), Value::Int8(2), Value::Int8(15)],
                vec![Value::Null, Value::Int8(2), Value::Int8(27)],
            ]
        );
    }

    #[test]
    fn group_by_float_treats_neg_zero_and_nan_like_pg() {
        // -0.0 groups with 0.0, and NaN groups with NaN — the hash and keys_equal
        // must agree on both. Two 0.0-family rows, two NaN rows.
        let engine = MemoryEngine::new();
        let table = engine
            .create_table(TableSchema {
                name: "f".into(),
                columns: vec![Column::new("x", PgType::Float8)],
            })
            .unwrap();
        let txn = wtxn();
        for x in [0.0_f64, -0.0, f64::NAN, f64::NAN] {
            table.insert(vec![Value::Float8(x)], &txn);
        }
        let engine: Arc<dyn TableEngine> = Arc::new(engine);
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

    /// Drain a query, returning the first runtime error (SRF errors surface on
    /// the first `next()`, not at plan time).
    fn run_err(sql: &str) -> ExecError {
        let engine: Arc<dyn TableEngine> = Arc::new(MemoryEngine::new());
        let stmts = crabgresql_parser::parse(sql).unwrap();
        let crabgresql_parser::ast::Statement::Query(query) = &stmts[0] else {
            panic!("expected a query");
        };
        let catalog: Arc<dyn crabgresql_storage_api::TypeCatalog> =
            Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
        let logical = crabgresql_binder::bind_query(&engine, &catalog, query).unwrap();
        let physical = crabgresql_planner::plan(logical);
        let Execution::Rows { mut node, .. } =
            execute(physical, ExecContext::default(), &rtxn()).unwrap()
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
            ("SELECT generate_series('NaN'::numeric, 3)", "start value cannot be NaN"),
            ("SELECT generate_series(1, 'NaN'::numeric)", "stop value cannot be NaN"),
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
            ["2020-01-03 00:00:00", "2020-01-02 00:00:00", "2020-01-01 00:00:00"]
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
        let engine: Arc<dyn TableEngine> = Arc::new(MemoryEngine::new());
        let table = engine
            .create_table(TableSchema {
                name: "nums".into(),
                columns: vec![Column::new("n", PgType::Int4)],
            })
            .unwrap();
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
        let values =
            "(VALUES (1), (2), (NULL)) a(x) JOIN_KIND \
             (VALUES (2), (2), (3), (NULL)) b(y) ON a.x = b.y";
        let query = |kind: &str| {
            format!(
                "SELECT a.x, b.y FROM {}",
                values.replace("JOIN_KIND", kind)
            )
        };

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
}
