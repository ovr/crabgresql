use std::cmp::Ordering;

use crabgresql_binder::{
    BoundAggregate, BoundWindowFunc, BoundWindowSpec, SortKey, WindowFn, WindowKind,
};
use crabgresql_storage_api::Tuple;
use crabgresql_types::Value;

use super::sort::compare_rows;
use crate::{ExecContext, ExecError, ExecNode, agg, eval};

/// One step of window-function evaluation: partition the input, order each
/// partition, and fill this step's results into the slots the binder assigned.
///
/// Eagerly materializing, like [`Sort`](crate::Sort) rather than
/// [`Aggregate`](crate::Aggregate) — the node *is* a sort plus a forward pass,
/// and draining the child in `new()` lets it (and its buffer) drop before the
/// next link of a chain builds, so peak memory stays at roughly two buffers
/// instead of one per link.
///
/// **Rows come out in window-sort order, not input order.** That is PG's
/// behavior: a window query with no `ORDER BY` of its own returns rows in the
/// order of the last spec evaluated. The query's own `ORDER BY`, when it has one,
/// runs above this node and re-sorts.
///
/// The default frame (`RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW`)
/// needs just two evaluation strategies, both O(n) per partition: with no
/// `ORDER BY` every row is a peer of every other, so an aggregate is the
/// whole-partition total; with one, the frame runs through the current row's
/// last *peer*, so it is a running total that only advances at a peer-group
/// boundary.
///
/// TODO: evaluate explicit window frames (ROWS/RANGE/GROUPS bounds and
/// EXCLUDE); anything but the default frame is rejected at bind time.
pub struct WindowAgg {
    rows: std::vec::IntoIter<Tuple>,
}

impl WindowAgg {
    pub fn new(
        mut child: Box<dyn ExecNode>,
        spec: BoundWindowSpec,
        funcs: Vec<BoundWindowFunc>,
        output_width: usize,
        ctx: &ExecContext,
    ) -> Result<Self, ExecError> {
        let mut rows: Vec<Tuple> = Vec::new();
        while let Some(row) = child.next()? {
            rows.push(row);
        }

        // Widen every row to the chain's full width. The bottom node of a chain
        // finds rows `input_width` wide and pads them; the ones above find the
        // padding already there and this is a no-op.
        // `Vec::resize` shrinks as well as grows, so a row wider than the chain
        // would be silently truncated — every `func.slot` write would then land
        // on the wrong column. Check instead of asserting, so a release build
        // reports the broken plan rather than computing a wrong answer.
        for row in &mut rows {
            if row.len() > output_width {
                return Err(ExecError::new(
                    crabgresql_pg_wire::sqlstate::INTERNAL_ERROR,
                    "window input row is wider than the window chain's output row",
                ));
            }
            row.resize(output_width, Value::Null);
        }

        // Evaluate the spec's keys into hidden columns past the chain's slots, so
        // the sort can address them by index and reuse `compare_rows` — the same
        // resjunk trick the binder uses for an ORDER BY expression that is not in
        // the select list. They are truncated away before the rows are handed on.
        let mut keys = Vec::with_capacity(spec.partition_by.len() + spec.order_by.len());
        for (offset, expr) in spec.partition_by.iter().enumerate() {
            keys.push(SortKey {
                column: output_width + offset,
                ty: expr.ty(),
                // Membership is bytewise whatever the collation (every supported
                // one is deterministic), but the *order* partitions come out in
                // is not — `PARTITION BY s COLLATE "en-US-x-icu"` must group in
                // that locale's order. Derived from the expression exactly as
                // the binder derives an ORDER BY key's collation.
                collation: crabgresql_binder::expr_collation(expr).collation,
                // Partitioning is grouping, not ordering: any consistent
                // direction works, and ascending with NULLs last matches the
                // order PG's partitions come out in.
                asc: true,
                nulls_first: false,
            });
        }
        for (offset, key) in spec.order_by.iter().enumerate() {
            keys.push(SortKey {
                column: output_width + spec.partition_by.len() + offset,
                ty: key.ty,
                collation: key.collation,
                asc: key.asc,
                nulls_first: key.nulls_first,
            });
        }
        for row in &mut rows {
            for expr in spec
                .partition_by
                .iter()
                .chain(spec.order_by.iter().map(|k| &k.expr))
            {
                let value = eval(expr, row, ctx)?;
                row.push(value);
            }
        }
        // Stable, so rows that tie on every key keep input order.
        rows.sort_by(|a, b| compare_rows(a, b, &keys));

        let partition_keys = &keys[..spec.partition_by.len()];
        let order_keys = &keys[spec.partition_by.len()..];
        let mut start = 0;
        while start < rows.len() {
            // The rows are sorted on the partition keys, so a partition is a run
            // of adjacent equal rows and one comparison per row finds its end —
            // no hashing, unlike `Aggregate`, whose input is unordered.
            let mut end = start + 1;
            while end < rows.len() && rows_match(&rows[end - 1], &rows[end], partition_keys) {
                end += 1;
            }
            Self::fill_partition(&mut rows[start..end], order_keys, &funcs, ctx)?;
            start = end;
        }

        for row in &mut rows {
            row.truncate(output_width);
        }
        Ok(Self {
            rows: rows.into_iter(),
        })
    }

    /// Compute every window call over one partition, already sorted.
    fn fill_partition(
        rows: &mut [Tuple],
        order_keys: &[SortKey],
        funcs: &[BoundWindowFunc],
        ctx: &ExecContext,
    ) -> Result<(), ExecError> {
        // Peer groups: maximal runs equal on the window's ORDER BY keys. With no
        // ORDER BY the key list is empty and `rows_match` is vacuously true, so
        // the whole partition is one peer group — which is exactly why `rank()
        // OVER ()` is 1 everywhere and `sum(x) OVER ()` is the partition total.
        // `peer_start[i]` is the index of the first row of `i`'s group, and
        // `peer_group[i]` that group's 0-based ordinal.
        let mut peer_start = Vec::with_capacity(rows.len());
        let mut peer_group = Vec::with_capacity(rows.len());
        for i in 0..rows.len() {
            if i > 0 && rows_match(&rows[i - 1], &rows[i], order_keys) {
                peer_start.push(peer_start[i - 1]);
                peer_group.push(peer_group[i - 1]);
            } else {
                peer_start.push(i);
                peer_group.push(if i == 0 { 0 } else { peer_group[i - 1] + 1 });
            }
        }

        for func in funcs {
            match &func.kind {
                WindowKind::Builtin { func: builtin, .. } => {
                    for i in 0..rows.len() {
                        let value = match builtin {
                            WindowFn::RowNumber => i as i64 + 1,
                            WindowFn::Rank => peer_start[i] as i64 + 1,
                            WindowFn::DenseRank => peer_group[i] as i64 + 1,
                        };
                        rows[i][func.slot] = Value::Int8(value);
                    }
                }
                WindowKind::Aggregate(agg) => {
                    Self::fill_aggregate(rows, &peer_start, agg, func.slot, ctx)?;
                }
            }
        }
        Ok(())
    }

    /// Accumulate one window aggregate across a sorted partition.
    ///
    /// The default frame ends at the current row's *last peer*, so every row of a
    /// peer group sees the same frame and therefore the same value. Feeding rows
    /// in order and only reading the running total once a group is complete gives
    /// that in a single pass.
    fn fill_aggregate(
        rows: &mut [Tuple],
        peer_start: &[usize],
        agg: &BoundAggregate,
        slot: usize,
        ctx: &ExecContext,
    ) -> Result<(), ExecError> {
        debug_assert!(agg.args.len() <= 2, "widen ARG_BUF for a >2-arg aggregate");
        let mut acc = agg::Accumulator::new(agg);
        let mut group_start = 0;
        for i in 0..=rows.len() {
            // At a peer-group boundary (and at the end), the accumulator holds
            // every row through the group that just closed: publish it to all of
            // that group's rows.
            if i == rows.len() || peer_start[i] != group_start {
                let value = acc.finalize()?;
                for row in &mut rows[group_start..i] {
                    row[slot] = value.clone();
                }
                if i == rows.len() {
                    break;
                }
                group_start = peer_start[i];
            }
            let mut buf = [Value::Null, Value::Null];
            for (dest, arg) in buf.iter_mut().zip(agg.args.iter()) {
                *dest = eval(arg, &rows[i], ctx)?;
            }
            agg::feed(&mut acc, agg, &buf[..agg.args.len()], None)?;
        }
        Ok(())
    }
}

impl ExecNode for WindowAgg {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        Ok(self.rows.next())
    }
}

/// Whether two rows are equal on every one of `keys` — the boundary test for
/// both partitions and peer groups.
///
/// Defined as "the sort sees no difference" rather than as its own comparison
/// ladder, so a boundary can never disagree with the order that produced it:
/// NULLs group together because the sort places them together, and `asc` /
/// `nulls_first` cannot matter because they only ever flip a non-`Equal`
/// result. An empty `keys` makes every row match.
fn rows_match(a: &Tuple, b: &Tuple, keys: &[SortKey]) -> bool {
    compare_rows(a, b, keys) == Ordering::Equal
}
