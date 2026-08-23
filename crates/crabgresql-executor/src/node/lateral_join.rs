//! The `LATERAL` join: a nested loop whose right side is *built* per left row.
//!
//! Every other join node can materialize or hash its right input once, because
//! that input is the same rowset for every left row. A lateral item is not: its
//! body reads the left row through `OuterColumnRef { level: 1 }` slots, so the
//! rows it produces change with each one. This node therefore keeps the right
//! side as the binder left it — a logical [`JoinInput`] — and, per left row,
//! fills those slots from the row and builds a source out of the result.
//!
//! The mechanism is the one a correlated subquery already uses
//! (`eval_correlated_subquery`), and so is the cost: a subquery body is planned
//! once per left row. A table function needs no planning at all, only its
//! arguments substituted, which is the common `FROM t, unnest(t.arr)` case.
//!
//! TODO: memoize the built source on the left slots the item actually reads
//! ([`crabgresql_binder::lateral_input_slots`] already names them), the way
//! `subplan::memo_get` does for a correlated subquery.

use crabgresql_binder::{BoundExpr, JoinInput, JoinKind};
use crabgresql_planner::PhysicalJoinInput;
use crabgresql_storage_api::Tuple;
use crabgresql_types::Value;

use super::join::{fill_probe, take_joined_row, touched_slots};
use crate::{ExecContext, ExecError, ExecNode, build_join_source, predicate_holds};

pub struct LateralJoin {
    left: Box<dyn ExecNode>,
    /// The logical right side, cloned and filled from each left row.
    template: JoinInput,
    left_width: usize,
    right_width: usize,
    kind: JoinKind,
    predicate: Option<BoundExpr>,
    ctx: ExecContext,
    /// The left row being expanded, and the source built for it.
    current: Option<(Tuple, Box<dyn ExecNode>)>,
    /// Whether any right row of the current left row passed the predicate —
    /// what a `LEFT JOIN LATERAL` null-extends on.
    matched: bool,
    probe: Vec<Value>,
    touched: Option<Vec<usize>>,
}

impl LateralJoin {
    pub fn new(
        left: Box<dyn ExecNode>,
        template: JoinInput,
        left_width: usize,
        right_width: usize,
        kind: JoinKind,
        predicate: Option<BoundExpr>,
        ctx: ExecContext,
    ) -> Result<Self, ExecError> {
        // The binder refuses a lateral reference across a RIGHT/FULL join (as
        // PostgreSQL does), and no rewrite produces a lateral semi/anti join, so
        // the preserved-right machinery the other join nodes carry has no case
        // here.
        if !matches!(kind, JoinKind::Cross | JoinKind::Inner | JoinKind::Left) {
            return Err(ExecError::new(
                crabgresql_pg_wire::sqlstate::INTERNAL_ERROR,
                format!("a lateral join cannot be {kind:?}"),
            ));
        }
        let touched = touched_slots(&predicate);
        Ok(Self {
            left,
            template,
            left_width,
            right_width,
            kind,
            predicate,
            ctx,
            current: None,
            matched: false,
            probe: vec![Value::Null; left_width + right_width],
            touched,
        })
    }

    /// Build the right side for one left row.
    fn source_for(&self, left: &[Value]) -> Result<Box<dyn ExecNode>, ExecError> {
        let txn = self.ctx.txn.clone().ok_or_else(|| {
            ExecError::new(
                crabgresql_pg_wire::sqlstate::INTERNAL_ERROR,
                "lateral join executed without a transaction context",
            )
        })?;
        let input = match self.template.clone() {
            JoinInput::TableFunction {
                func,
                mut args,
                ordinality,
            } => {
                crabgresql_binder::substitute_outer_exprs(&mut args, left);
                // The arguments skipped the statement-wide fold (their slots did
                // not exist then), so any uncorrelated subquery marker in them is
                // resolved here instead.
                crate::resolve_exprs(&mut args, &self.ctx, &txn)?;
                PhysicalJoinInput::TableFunction {
                    func,
                    args,
                    ordinality,
                }
            }
            JoinInput::Subplan(mut plan) => {
                crabgresql_binder::substitute_outer(&mut plan, left);
                // Planned, not optimized, for the reason `run_subplan` gives:
                // the body was rewritten once with the statement, and the only
                // thing a second pass would fold is the constants just
                // substituted in — once per left row.
                PhysicalJoinInput::Subplan(Box::new(crabgresql_planner::plan(
                    *plan,
                    self.ctx.costs,
                )))
            }
            // A base relation reads no row but its own, so it is never lateral.
            JoinInput::Scan { .. } => {
                return Err(ExecError::new(
                    crabgresql_pg_wire::sqlstate::INTERNAL_ERROR,
                    "a relation scan cannot be a lateral join input",
                ));
            }
        };
        build_join_source(input, &self.ctx, &txn)
    }
}

impl ExecNode for LateralJoin {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        loop {
            let Some((left, right)) = &mut self.current else {
                let Some(left) = self.left.next()? else {
                    return Ok(None);
                };
                let right = self.source_for(&left)?;
                self.current = Some((left, right));
                self.matched = false;
                continue;
            };

            let Some(right_row) = right.next()? else {
                let (left, _) = self.current.take().expect("matched above");
                if !self.matched && self.kind == JoinKind::Left {
                    let mut row = left;
                    row.extend(std::iter::repeat_n(Value::Null, self.right_width));
                    return Ok(Some(row));
                }
                continue;
            };

            // Tested in the reused probe buffer, as the two non-lateral join
            // nodes do — see `fill_probe`.
            fill_probe(
                &mut self.probe,
                &self.touched,
                left,
                &right_row,
                self.left_width,
            );
            if !predicate_holds(&self.predicate, &self.probe, &self.ctx)? {
                continue;
            }
            self.matched = true;
            return Ok(Some(take_joined_row(
                &mut self.probe,
                &self.touched,
                left,
                &right_row,
            )));
        }
    }
}
