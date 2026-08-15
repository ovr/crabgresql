//! The scratch-row machinery both join nodes run on.
//!
//! Stated once so the two cannot drift: a candidate pair is filled into a
//! reused buffer, tested there, and only turned into an output row if it
//! survives.

use crabgresql_binder::{BoundExpr, JoinKind};
use crabgresql_storage_api::Tuple;
use crabgresql_types::Value;

use crate::{ExecContext, ExecError, eval};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JoinPhase {
    LeftRows,
    UnmatchedRight,
    Done,
}

/// What a node does with the left row once its candidates have been classified.
/// Both join nodes read the kind through this, so the two cannot disagree about
/// which kinds emit pairs and which emit the left row alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MatchMode {
    /// Emit every surviving pair as a concatenated row (`Cross`/`Inner` and the
    /// three outer kinds, whose null extension is handled separately).
    Pairs,
    /// Emit the left row alone on the first surviving match, then move on.
    Semi,
    /// Emit the left row alone when no candidate survives.
    Anti,
}

impl MatchMode {
    pub(crate) fn of(kind: JoinKind) -> Self {
        match kind {
            JoinKind::Semi => MatchMode::Semi,
            JoinKind::Anti => MatchMode::Anti,
            _ => MatchMode::Pairs,
        }
    }
}

/// Concatenate a left and a right row into a joined output row.
pub(crate) fn combined_row(left: &[Value], right: &[Value]) -> Tuple {
    let mut row = Vec::with_capacity(left.len() + right.len());
    row.extend_from_slice(left);
    row.extend_from_slice(right);
    row
}

/// Which slots of the joined row `predicate` reads, ascending. `None` — an
/// expression whose dependencies cannot be pinned down — means "all of them",
/// the same fail-safe the projection pass uses. `Some(vec![])` means there is
/// nothing to test.
pub(crate) fn touched_slots(predicate: &Option<BoundExpr>) -> Option<Vec<usize>> {
    match predicate {
        // No predicate reads nothing — not "reads everything". Returning `None`
        // here would make every candidate pair copy a full row for a test that
        // never runs.
        None => Some(Vec::new()),
        Some(predicate) => {
            let mut refs = std::collections::BTreeSet::new();
            predicate
                .collect_column_refs(&mut refs)
                .then(|| refs.into_iter().collect())
        }
    }
}

/// Fill the reused probe buffer with the candidate pair, ready for a predicate
/// test. Only the slots the predicate reads are copied in: for a wide relation
/// a full joined row is an allocation plus a deep clone of every column
/// (`Value::Text` is an owned `String`), paid for every candidate pair and
/// thrown away on a mismatch. Slots outside `touched` keep whatever an earlier
/// pair left there, which nothing reads.
pub(crate) fn fill_probe(
    probe: &mut Vec<Value>,
    touched: &Option<Vec<usize>>,
    left: &[Value],
    right: &[Value],
    left_width: usize,
) {
    match touched {
        Some(touched) => {
            debug_assert!(
                touched.last().is_none_or(|slot| *slot < probe.len()),
                "refreshing only some slots needs a full-width buffer: the buffer is \
                 handed out as an output row in `None` mode only, where the next fill \
                 rebuilds it whole"
            );
            for slot in touched {
                let source = if *slot < left_width {
                    left.get(*slot)
                } else {
                    right.get(*slot - left_width)
                };
                probe[*slot] = source.cloned().unwrap_or(Value::Null);
            }
        }
        None => {
            // One reservation: `take_joined_row` hands this buffer out, so it
            // comes back empty and two bare `extend_from_slice` calls would
            // grow it twice.
            probe.clear();
            probe.reserve(left.len() + right.len());
            probe.extend_from_slice(left);
            probe.extend_from_slice(right);
        }
    }
}

/// Build the output row for a pair that passed the predicate.
///
/// When `touched` is `None` the probe buffer already *is* the joined row, so
/// hand it out rather than deep-cloning every column a second time. Emptying it
/// is safe because the next `fill_probe` in that mode rebuilds it from scratch,
/// so an emitted row costs exactly one allocation — what the code paid before
/// the buffer existed.
pub(crate) fn take_joined_row(
    probe: &mut Vec<Value>,
    touched: &Option<Vec<usize>>,
    left: &[Value],
    right: &[Value],
) -> Tuple {
    match touched {
        None => std::mem::take(probe),
        // A partial buffer: only the predicate's slots are meaningful, so the
        // row has to be built from the source rows.
        Some(_) => combined_row(left, right),
    }
}

/// Evaluate each equi-key expression over `row`, returning `None` as soon as one
/// is NULL (a NULL key can never match in a join, mirroring PG). `row` must be a
/// full-width concatenated row so key column indices stay valid.
pub(crate) fn eval_join_keys(
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
