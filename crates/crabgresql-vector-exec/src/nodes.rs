//! Batch-producing nodes, and the aggregate that consumes them.

use std::collections::HashMap;
use std::sync::Arc;

use crabgresql_batch::{Batch, BatchSchema, VectorExpr, eval_batch, value_of};
use crabgresql_executor::agg::{Accumulator, hash_key, keys_equal};
use crabgresql_executor::{ExecError, ExecNode};
use crabgresql_storage_api::{BatchStream, ScanRequest, TableAm, Tuple, TupleStream};
use crabgresql_txn::TxnContext;
use crabgresql_types::{PgType, Value};

use crate::error::to_exec_error;
use crate::plan::AggregateSpec;

/// A vectorized execution node: `next_batch()` pulls one [`Batch`] at a time.
///
/// The columnar twin of [`ExecNode`] and, like it, pull-based — so back-pressure
/// is the caller's stack. A returned batch is never empty; `None` ends the
/// stream, so no consumer needs to distinguish "no rows yet" from "no more rows".
pub trait BatchNode: Send {
    fn next_batch(&mut self) -> Result<Option<Batch>, ExecError>;
}

/// One storage leaf's columnar scan.
pub struct BatchScan {
    streams: std::vec::IntoIter<BatchStream>,
    current: Option<BatchStream>,
}

impl BatchScan {
    /// Open `table`'s columnar scan, or `None` if the engine has none.
    pub fn open(table: &Arc<dyn TableAm>, txn: &TxnContext, req: &ScanRequest) -> Option<Self> {
        let streams = table.scan_batches(txn, req)?;
        Some(BatchScan {
            streams: streams.into_iter(),
            current: None,
        })
    }
}

impl BatchNode for BatchScan {
    fn next_batch(&mut self) -> Result<Option<Batch>, ExecError> {
        loop {
            if let Some(stream) = &mut self.current {
                match stream.next() {
                    Some(Ok(batch)) if batch.is_empty() => continue,
                    Some(Ok(batch)) => return Ok(Some(batch)),
                    Some(Err(error)) => return Err(error.into()),
                    None => self.current = None,
                }
            }
            // Streams are consumed in order, because their concatenation in that
            // order is what equals the row scan's row order.
            let Some(stream) = self.streams.next() else {
                return Ok(None);
            };
            self.current = Some(stream);
        }
    }
}

/// A row-only storage leaf, fed into a batch pipeline by building arrays from
/// its tuples.
///
/// This is what lets a Parquet relation be scanned as one pipeline even though
/// half of it — the RAM write buffer in front of the chunk store — has no
/// columnar form. Without it, vectorization would switch itself off whenever a
/// `COPY` left rows unflushed, which is to say whenever it mattered most, and
/// the benchmark would depend on flush timing rather than on the engine.
///
/// The conversion goes through [`Value`], so the buffer's rows arrive already in
/// the PostgreSQL domain and agree with the chunk store's rebased columns by
/// construction. A `GROUP BY` spanning both leaves therefore reports one group
/// per key rather than two.
pub struct Rebatch {
    rows: TupleStream,
    schema: BatchSchema,
    slots: Vec<usize>,
    capacity: usize,
    done: bool,
}

impl Rebatch {
    pub fn open(
        table: &Arc<dyn TableAm>,
        txn: &TxnContext,
        req: &ScanRequest,
        schema: BatchSchema,
    ) -> Self {
        let slots = req.slots(table.schema().columns.len());
        Rebatch {
            rows: table.scan(txn, &req.columns),
            schema,
            slots,
            capacity: req.batch_size,
            done: false,
        }
    }
}

impl BatchNode for Rebatch {
    fn next_batch(&mut self) -> Result<Option<Batch>, ExecError> {
        if self.done {
            return Ok(None);
        }
        let mut rows: Vec<Tuple> = Vec::new();
        while rows.len() < self.capacity {
            match self.rows.next() {
                Some(Ok((_, tuple))) => rows.push(tuple),
                Some(Err(error)) => return Err(error.into()),
                None => {
                    self.done = true;
                    break;
                }
            }
        }
        if rows.is_empty() {
            return Ok(None);
        }
        crabgresql_batch::build::from_rows(&self.schema, &self.slots, &rows)
            .map(Some)
            .map_err(to_exec_error)
    }
}

/// The leaves of one relation, in order.
pub struct BatchConcat {
    children: std::vec::IntoIter<Box<dyn BatchNode>>,
    current: Option<Box<dyn BatchNode>>,
}

impl BatchConcat {
    pub fn new(children: Vec<Box<dyn BatchNode>>) -> Self {
        BatchConcat {
            children: children.into_iter(),
            current: None,
        }
    }
}

impl BatchNode for BatchConcat {
    fn next_batch(&mut self) -> Result<Option<Batch>, ExecError> {
        loop {
            if let Some(child) = &mut self.current {
                match child.next_batch()? {
                    Some(batch) => return Ok(Some(batch)),
                    None => self.current = None,
                }
            }
            let Some(child) = self.children.next() else {
                return Ok(None);
            };
            self.current = Some(child);
        }
    }
}

/// `WHERE`, applied to whole batches.
pub struct BatchFilter {
    child: Box<dyn BatchNode>,
    predicate: VectorExpr,
}

impl BatchFilter {
    pub fn new(child: Box<dyn BatchNode>, predicate: VectorExpr) -> Self {
        BatchFilter { child, predicate }
    }
}

impl BatchNode for BatchFilter {
    fn next_batch(&mut self) -> Result<Option<Batch>, ExecError> {
        // Loops rather than returning an empty batch, so a batch that the
        // predicate rejects entirely is invisible to the consumer.
        while let Some(batch) = self.child.next_batch()? {
            let mask = eval_batch(&self.predicate, &batch).map_err(to_exec_error)?;
            let kept = batch.filter_by(&mask).map_err(to_exec_error)?;
            if !kept.is_empty() {
                return Ok(Some(kept));
            }
        }
        Ok(None)
    }
}

/// Grouping and aggregation over a batch pipeline.
///
/// An [`ExecNode`], not a [`BatchNode`]: its output is one row per group, which
/// is small, so everything above it — `HAVING`, the projection list, `ORDER BY`,
/// `LIMIT` — keeps running through the row engine's proven nodes. The rows that
/// matter for performance are the input rows, and those never become tuples.
///
/// # What is and is not vectorized here
///
/// The scan, the decode and the `WHERE` are columnar. Accumulation still steps
/// one value at a time, through the row engine's own [`Accumulator`], and
/// grouping through its own [`hash_key`] and [`keys_equal`]. That is deliberate:
/// it makes `sum(int8) -> numeric`, `avg(int)`'s 16-digit scale, `min`/`max`
/// collation, float summation order, NaN and signed-zero folding, and
/// `bpchar` blank-trimming identical to the row engine by construction rather
/// than by a second implementation that has to be kept in step.
///
/// Groups are numbered in **first-seen order** and keep their **first-seen key
/// value**, both of which are observable: a `GROUP BY` with no `ORDER BY`
/// reports rows in that order, and `GROUP BY x` over `(-0.0, 0.0)` must emit
/// `-0` rather than the canonical form the hash uses.
pub struct VectorAggregate {
    child: Box<dyn BatchNode>,
    keys: Vec<VectorExpr>,
    key_types: Vec<PgType>,
    aggregates: Vec<AggregateSpec>,
    output: Option<std::vec::IntoIter<Tuple>>,
}

struct Group {
    key: Vec<Value>,
    accumulators: Vec<Accumulator>,
}

impl VectorAggregate {
    pub fn new(
        child: Box<dyn BatchNode>,
        keys: Vec<VectorExpr>,
        key_types: Vec<PgType>,
        aggregates: Vec<AggregateSpec>,
    ) -> Self {
        VectorAggregate {
            child,
            keys,
            key_types,
            aggregates,
            output: None,
        }
    }

    fn build(&mut self) -> Result<std::vec::IntoIter<Tuple>, ExecError> {
        let mut groups: Vec<Group> = Vec::new();
        let mut lookup: HashMap<u64, Vec<usize>> = HashMap::new();

        // A grouping-free aggregate has exactly one group even over no rows, so
        // `SELECT count(*) FROM empty` returns 0 rather than nothing.
        if self.keys.is_empty() {
            groups.push(Group {
                key: Vec::new(),
                accumulators: self.aggregates.iter().map(|a| Accumulator::new(&a.agg)).collect(),
            });
            lookup.insert(hash_key(&[], &[]), vec![0]);
        }

        while let Some(batch) = self.child.next_batch()? {
            // Each key and argument is evaluated once per batch, over the whole
            // batch, into a narrow array — so a hundred-column relation costs
            // only the columns this query names.
            let key_arrays = self
                .keys
                .iter()
                .map(|key| eval_batch(key, &batch).map_err(to_exec_error))
                .collect::<Result<Vec<_>, _>>()?;
            let arg_arrays = self
                .aggregates
                .iter()
                .map(|spec| {
                    spec.args
                        .iter()
                        .map(|arg| eval_batch(arg, &batch).map_err(to_exec_error))
                        .collect::<Result<Vec<_>, _>>()
                })
                .collect::<Result<Vec<_>, _>>()?;

            let mut key = vec![Value::Null; self.keys.len()];
            for row in 0..batch.len() {
                let index = if self.keys.is_empty() {
                    0
                } else {
                    for ((cell, array), ty) in
                        key.iter_mut().zip(&key_arrays).zip(&self.key_types)
                    {
                        *cell = value_of(array.as_ref(), *ty, row).map_err(to_exec_error)?;
                    }
                    self.intern(&mut groups, &mut lookup, &key)
                };

                for (slot, (spec, arrays)) in
                    self.aggregates.iter().zip(&arg_arrays).enumerate()
                {
                    let accumulator = &mut groups[index].accumulators[slot];
                    if spec.args.is_empty() {
                        accumulator.count_row();
                        continue;
                    }
                    // Sized to the largest aggregate arity, matching the row
                    // engine, so a single-argument aggregate pays no allocation.
                    let mut buf = [Value::Null, Value::Null];
                    for ((cell, array), arg) in buf.iter_mut().zip(arrays).zip(&spec.args) {
                        *cell = value_of(array.as_ref(), arg.result_type(), row)
                            .map_err(to_exec_error)?;
                    }
                    let values = &buf[..spec.args.len()];
                    // Every aggregate but COUNT(*) ignores a NULL first argument.
                    if !matches!(values[0], Value::Null) {
                        accumulator.accumulate(values)?;
                    }
                }
            }
        }

        let mut out = Vec::with_capacity(groups.len());
        for group in groups {
            let mut tuple = group.key;
            for accumulator in group.accumulators {
                tuple.push(accumulator.finalize()?);
            }
            out.push(tuple);
        }
        Ok(out.into_iter())
    }

    /// The ordinal of `key`'s group, creating it if this is its first row.
    fn intern(
        &self,
        groups: &mut Vec<Group>,
        lookup: &mut HashMap<u64, Vec<usize>>,
        key: &[Value],
    ) -> usize {
        let hash = hash_key(&self.key_types, key);
        let bucket = lookup.entry(hash).or_default();
        for &candidate in bucket.iter() {
            if keys_equal(&self.key_types, &groups[candidate].key, key) {
                return candidate;
            }
        }
        let index = groups.len();
        groups.push(Group {
            key: key.to_vec(),
            accumulators: self
                .aggregates
                .iter()
                .map(|a| Accumulator::new(&a.agg))
                .collect(),
        });
        bucket.push(index);
        index
    }
}

impl ExecNode for VectorAggregate {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        if self.output.is_none() {
            self.output = Some(self.build()?);
        }
        Ok(self
            .output
            .as_mut()
            .and_then(std::iter::Iterator::next))
    }
}
