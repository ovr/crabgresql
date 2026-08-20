use std::cmp::Ordering;

use crabgresql_binder::{BoundAggregate, BoundExpr, SortKey};
use crabgresql_storage_api::Tuple;
use crabgresql_types::Value;

use crate::node::sort::compare_rows;
use crate::{ExecContext, ExecError, ExecNode, agg, eval, keyindex};

/// Grouped aggregation. On the first pull it buffers every child row, groups
/// them by the NULL-aware equality of the group-key expressions in first-seen
/// order — so per-group accumulation follows scan order, matching PG's hash
/// aggregate — and emits one row per group laid out `[group keys…, aggregates…]`.
/// An empty group-key list is the implicit single group: exactly one output row
/// even over no input (`SELECT count(*)` → 0).
///
/// An aggregate carrying its own `ORDER BY` (`array_agg(x ORDER BY y)`) cannot
/// accumulate as the rows arrive — its order is only known once the group is
/// complete — so its inputs are buffered per group and folded in at finalize.
/// Everything else keeps the streaming path.
pub struct Aggregate {
    child: Box<dyn ExecNode>,
    group_exprs: Vec<BoundExpr>,
    aggregates: Vec<BoundAggregate>,
    /// The specification each accumulator is built from. Identical to
    /// `aggregates` except that an *ordered* aggregate has `distinct` cleared:
    /// it dedups in the buffered path, right after the sort, and leaving the
    /// flag set would make `array_agg`'s finalize re-sort the elements the
    /// `ORDER BY` had just placed.
    acc_specs: Vec<BoundAggregate>,
    /// The sort keys of each ordered aggregate, addressing its own buffered key
    /// tuple by position, or `None` for an aggregate with no `ORDER BY`. Built
    /// once so the per-group sort reuses [`compare_rows`] — the comparison
    /// behind every other `ORDER BY` in the engine.
    order_keys: Vec<Option<Vec<SortKey>>>,
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
    ///
    /// Empty when *no* aggregate is `DISTINCT`, which is the overwhelmingly
    /// common case — a query grouping to millions of groups would otherwise
    /// allocate a vector of `None` per group. Readers index it rather than
    /// zipping it, so the short and full-length forms behave alike.
    distinct_values: Vec<Option<agg::DistinctValues>>,
    /// One buffer per aggregate, holding `(sort keys, arguments)` for the rows
    /// of this group. Only an *ordered* aggregate ever pushes; the rest keep
    /// their empty slot. Empty altogether when no aggregate is ordered, and
    /// indexed rather than zipped for the reason `distinct_values` is.
    buffered: Vec<Vec<(Tuple, Vec<Value>)>>,
}

impl Aggregate {
    pub fn new(
        child: Box<dyn ExecNode>,
        group_exprs: Vec<BoundExpr>,
        aggregates: Vec<BoundAggregate>,
        ctx: ExecContext,
    ) -> Self {
        let acc_specs = aggregates
            .iter()
            .map(|agg| {
                if agg.order_by.is_empty() {
                    agg.clone()
                } else {
                    BoundAggregate {
                        distinct: false,
                        ..agg.clone()
                    }
                }
            })
            .collect();
        let order_keys = aggregates
            .iter()
            .map(|agg| {
                (!agg.order_by.is_empty()).then(|| {
                    agg.order_by
                        .iter()
                        .enumerate()
                        .map(|(column, key)| SortKey {
                            column,
                            ty: key.ty,
                            collation: key.collation,
                            asc: key.asc,
                            nulls_first: key.nulls_first,
                        })
                        .collect()
                })
            })
            .collect();
        Self {
            child,
            group_exprs,
            aggregates,
            acc_specs,
            order_keys,
            ctx,
            output: None,
        }
    }

    /// Whether this aggregate wants a streaming [`agg::DistinctValues`] set. An
    /// ordered one never does: it sees no row until finalize, where adjacent
    /// duplicates are dropped from the sorted buffer instead.
    fn wants_distinct_set(agg: &BoundAggregate) -> bool {
        agg.order_by.is_empty() && agg::wants_distinct_set(agg)
    }

    /// A fresh group for `key`, with one accumulator per aggregate. Both the
    /// implicit single group and every keyed group come through here, so the
    /// two cannot disagree on what a new group holds.
    fn new_group(&self, key: Vec<Value>, any_distinct: bool, any_ordered: bool) -> AggGroup {
        AggGroup {
            key,
            accumulators: self.acc_specs.iter().map(agg::Accumulator::new).collect(),
            distinct_values: if any_distinct {
                self.aggregates
                    .iter()
                    .map(|agg| {
                        Self::wants_distinct_set(agg)
                            .then(|| agg::DistinctValues::new(agg.input_ty))
                    })
                    .collect()
            } else {
                Vec::new()
            },
            buffered: if any_ordered {
                vec![Vec::new(); self.aggregates.len()]
            } else {
                Vec::new()
            },
        }
    }

    /// Fold each ordered aggregate's buffer into its accumulator, in the order
    /// its own `ORDER BY` asks for.
    ///
    /// `DISTINCT` is applied here rather than through a hashed seen-set: the
    /// buffer is already sorted on the keys, and PostgreSQL only accepts
    /// `DISTINCT` together with an `ORDER BY` whose expressions *are* the
    /// arguments (the binder enforces that), so dropping adjacent duplicates
    /// eliminates exactly the values PostgreSQL's own sort-based dedup does —
    /// NULLs included, which a `DistinctValues` cannot even encode.
    fn drain_buffers(&self, group: &mut AggGroup) -> Result<(), ExecError> {
        for (i, keys) in self.order_keys.iter().enumerate() {
            let Some(keys) = keys else { continue };
            let rows = &mut group.buffered[i];
            // Stable, so rows equal on every key keep arrival order — the same
            // tiebreak `Sort` gives a query's own ORDER BY.
            rows.sort_by(|a, b| compare_rows(&a.0, &b.0, keys));
            if self.aggregates[i].distinct {
                rows.dedup_by(|a, b| compare_rows(&a.0, &b.0, keys) == Ordering::Equal);
            }
            for (_, args) in rows.iter() {
                agg::feed(&mut group.accumulators[i], &self.acc_specs[i], args, None)?;
            }
        }
        Ok(())
    }

    /// Drain the child, accumulate per group, and materialize the output rows.
    fn build(&mut self) -> Result<std::vec::IntoIter<Tuple>, ExecError> {
        let key_tys: Vec<_> = self.group_exprs.iter().map(BoundExpr::ty).collect();
        let any_distinct = self.aggregates.iter().any(Self::wants_distinct_set);
        let any_ordered = self.order_keys.iter().any(Option::is_some);
        let mut groups: Vec<AggGroup> = Vec::new();
        // Each group's key → its index in `groups`, so a row finds its group in
        // ~O(1). Groups stay in first-seen order (accumulation follows scan
        // order).
        let mut lookup = keyindex::GroupIndex::new(&key_tys);
        // The implicit single group needs one seeded group so an empty input
        // still produces a row.
        if self.group_exprs.is_empty() {
            groups.push(self.new_group(Vec::new(), any_distinct, any_ordered));
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
                let next = groups.len();
                match lookup.find_or_insert(&key, next, |i| {
                    agg::keys_equal(&key_tys, &groups[i].key, &key)
                }) {
                    keyindex::Slot::Existing(i) => i,
                    keyindex::Slot::Vacant => {
                        groups.push(self.new_group(key, any_distinct, any_ordered));
                        next
                    }
                }
            };
            let AggGroup {
                accumulators,
                distinct_values,
                buffered,
                ..
            } = &mut groups[idx];
            // `distinct_values` is indexed rather than zipped: it is empty when
            // no aggregate is DISTINCT, and a third `zip` would then yield
            // nothing at all — feeding no aggregate and counting no row.
            // `buffered` is indexed for the same reason.
            for (i, (agg, acc)) in self
                .aggregates
                .iter()
                .zip(accumulators.iter_mut())
                .enumerate()
            {
                // Evaluated into a small stack buffer, sized to the largest
                // aggregate arity (2, for `string_agg`), so the common
                // single-argument aggregates (sum/avg/min/max/count(expr))
                // don't pay a per-row Vec allocation.
                debug_assert!(agg.args.len() <= 2, "widen ARG_BUF for a >2-arg aggregate");
                let mut buf = [Value::Null, Value::Null];
                for (slot, arg) in buf.iter_mut().zip(agg.args.iter()) {
                    *slot = eval(arg, &row, &self.ctx)?;
                }
                // An ordered aggregate defers: its accumulator must not see a
                // row before the group's rows have been put in order.
                if self.order_keys[i].is_some() {
                    let keys = agg
                        .order_by
                        .iter()
                        .map(|key| eval(&key.expr, &row, &self.ctx))
                        .collect::<Result<Vec<_>, _>>()?;
                    buffered[i].push((keys, buf[..agg.args.len()].to_vec()));
                    continue;
                }
                let seen = distinct_values.get_mut(i).and_then(Option::as_mut);
                agg::feed(acc, agg, &buf[..agg.args.len()], seen)?;
            }
        }
        for group in &mut groups {
            self.drain_buffers(group)?;
        }
        let mut out = Vec::with_capacity(groups.len());
        for group in groups {
            let mut tuple = group.key;
            for acc in &group.accumulators {
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
