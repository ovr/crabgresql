use crabgresql_binder::{BoundAggregate, BoundExpr};
use crabgresql_storage_api::Tuple;
use crabgresql_types::Value;

use crate::{ExecContext, ExecError, ExecNode, agg, eval, keyindex};

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
    ///
    /// Empty when *no* aggregate is `DISTINCT`, which is the overwhelmingly
    /// common case — a query grouping to millions of groups would otherwise
    /// allocate a vector of `None` per group. Readers index it rather than
    /// zipping it, so the short and full-length forms behave alike.
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

    /// A fresh group for `key`, with one accumulator per aggregate. Both the
    /// implicit single group and every keyed group come through here, so the
    /// two cannot disagree on what a new group holds.
    fn new_group(&self, key: Vec<Value>, any_distinct: bool) -> AggGroup {
        AggGroup {
            key,
            accumulators: self.aggregates.iter().map(agg::Accumulator::new).collect(),
            distinct_values: if any_distinct {
                self.aggregates
                    .iter()
                    .map(|agg| {
                        agg::wants_distinct_set(agg).then(|| agg::DistinctValues::new(agg.input_ty))
                    })
                    .collect()
            } else {
                Vec::new()
            },
        }
    }

    /// Drain the child, accumulate per group, and materialize the output rows.
    fn build(&mut self) -> Result<std::vec::IntoIter<Tuple>, ExecError> {
        let key_tys: Vec<_> = self.group_exprs.iter().map(BoundExpr::ty).collect();
        let any_distinct = self.aggregates.iter().any(agg::wants_distinct_set);
        let mut groups: Vec<AggGroup> = Vec::new();
        // Each group's key → its index in `groups`, so a row finds its group in
        // ~O(1). Groups stay in first-seen order (accumulation follows scan
        // order).
        let mut lookup = keyindex::GroupIndex::new(&key_tys);
        // The implicit single group needs one seeded group so an empty input
        // still produces a row.
        if self.group_exprs.is_empty() {
            groups.push(self.new_group(Vec::new(), any_distinct));
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
                        groups.push(self.new_group(key, any_distinct));
                        next
                    }
                }
            };
            let AggGroup {
                accumulators,
                distinct_values,
                ..
            } = &mut groups[idx];
            // `distinct_values` is indexed rather than zipped: it is empty when
            // no aggregate is DISTINCT, and a third `zip` would then yield
            // nothing at all — feeding no aggregate and counting no row.
            for (i, (agg, acc)) in self
                .aggregates
                .iter()
                .zip(accumulators.iter_mut())
                .enumerate()
            {
                let seen = distinct_values.get_mut(i).and_then(Option::as_mut);
                // Evaluated into a small stack buffer, sized to the largest
                // aggregate arity (2, for `string_agg`), so the common
                // single-argument aggregates (sum/avg/min/max/count(expr))
                // don't pay a per-row Vec allocation.
                debug_assert!(agg.args.len() <= 2, "widen ARG_BUF for a >2-arg aggregate");
                let mut buf = [Value::Null, Value::Null];
                for (slot, arg) in buf.iter_mut().zip(agg.args.iter()) {
                    *slot = eval(arg, &row, &self.ctx)?;
                }
                agg::feed(acc, agg, &buf[..agg.args.len()], seen)?;
            }
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
