use crabgresql_binder::{BoundExpr, JoinKind};
use crabgresql_storage_api::Tuple;
use crabgresql_types::Value;

use super::join::{JoinPhase, fill_probe, take_joined_row, touched_slots};
use crate::{ExecContext, ExecError, ExecNode, predicate_holds};

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
    /// A full-width joined row reused across candidate pairs, so testing one
    /// costs no allocation. Only the slots in `touched` are meaningful; the rest
    /// hold whatever an earlier pair left there, which nothing reads.
    probe: Vec<Value>,
    /// The slots of the joined row the filter reads, ascending. `None` means
    /// "could not be determined — copy the whole row".
    touched: Option<Vec<usize>>,
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
        debug_assert!(
            kind != JoinKind::Cross || predicate.is_none(),
            "a cross join carries no predicate; the planner flips the kind to Inner \
             when it attaches one"
        );
        let mut right_rows = Vec::new();
        while let Some(row) = right.next()? {
            right_rows.push(row);
        }
        let right_matched = vec![false; right_rows.len()];
        let touched = touched_slots(&predicate);
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
            probe: vec![Value::Null; left_width + right_width],
            touched,
        })
    }

    fn preserves_left(&self) -> bool {
        matches!(self.kind, JoinKind::Left | JoinKind::Full)
    }

    fn preserves_right(&self) -> bool {
        matches!(self.kind, JoinKind::Right | JoinKind::Full)
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
                        if self.current_left.is_none() {
                            continue;
                        }
                        // Test the pair against the reused probe buffer rather
                        // than a freshly built row — see `fill_probe`.
                        //
                        // Field-by-field borrows, so the fill can hold `&mut
                        // probe` while reading the two source rows.
                        let Self {
                            probe,
                            touched,
                            current_left,
                            right_rows,
                            left_width,
                            ..
                        } = self;
                        let left = current_left.as_deref().unwrap_or(&[]);
                        fill_probe(probe, touched, left, &right_rows[right_index], *left_width);
                        // A cross join carries no predicate, and `predicate_holds`
                        // already answers `true` for `None`, so there is no
                        // kind-specific short circuit here: an unconditional check
                        // means a predicate that reaches this node is always applied.
                        let matched = predicate_holds(&self.predicate, &self.probe, &self.ctx)?;
                        if matched {
                            self.current_left_matched = true;
                            self.right_matched[right_index] = true;
                            let Self {
                                probe,
                                touched,
                                current_left,
                                right_rows,
                                ..
                            } = self;
                            let Some(left) = current_left.as_deref() else {
                                continue;
                            };
                            return Ok(Some(take_joined_row(
                                probe,
                                touched,
                                left,
                                &right_rows[right_index],
                            )));
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
