use crabgresql_binder::{BoundExpr, JoinKind};
use crabgresql_planner::HashKey;
use crabgresql_storage_api::Tuple;
use crabgresql_types::{PgType, Value};
use rustc_hash::FxHashMap;

use super::join::{JoinPhase, eval_join_keys, fill_probe, take_joined_row, touched_slots};
use crate::{ExecContext, ExecError, ExecNode, agg, predicate_holds};

/// Binary hash join over one or more equi-keys, with the hash table built on the
/// materialized right (inner) side and the left side streamed as the probe. It
/// emits rows in the same order a [`NestedLoopJoin`](crate::NestedLoopJoin)
/// would — left-driven, right rows in materialization order within each match —
/// so results are identical whether or not the query sorts.
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
    buckets: FxHashMap<u64, Vec<(usize, Vec<Value>)>>,
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
    /// A full-width joined row reused across candidate pairs, so testing the
    /// residual costs no allocation. Only the slots in `touched` are meaningful;
    /// the rest hold whatever an earlier pair left there, which nothing reads.
    probe: Vec<Value>,
    /// The slots of the joined row the residual reads, ascending. `None` means
    /// "could not be determined — copy the whole row".
    touched: Option<Vec<usize>>,
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
        debug_assert!(
            kind != JoinKind::Cross,
            "a cross join has no equi-keys; the planner flips the kind to Inner when \
             it attaches a predicate"
        );
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
        let mut buckets: FxHashMap<u64, Vec<(usize, Vec<Value>)>> = FxHashMap::default();
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
        let touched = touched_slots(&residual);
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
                        // Then the residual (non-equi) conjuncts of the ON
                        // clause, tested against the reused probe buffer so a
                        // pair it rejects never builds a row — see `fill_probe`.
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
                        let Some(left) = current_left.as_deref() else {
                            continue;
                        };
                        fill_probe(probe, touched, left, &right_rows[right_index], *left_width);
                        if !predicate_holds(&self.residual, &self.probe, &self.ctx)? {
                            continue;
                        }
                        // Only now, for a surviving pair, is the output row
                        // built and the match bookkeeping updated — moving
                        // either above the residual test would null-extend the
                        // wrong rows on a preserved side.
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
