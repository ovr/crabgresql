//! The vectorized plan, and the reasons a physical plan is not one.

use std::sync::Arc;

use crabgresql_batch::{BatchSchema, VectorExpr};
use crabgresql_binder::{BinOp, BoundAggregate, BoundExpr};
use crabgresql_storage_api::{ScanRequest, TableAm};
use crabgresql_types::PgType;

/// A batch-producing pipeline.
///
/// One node per operator, unlike [`PhysicalPlan`], whose filter, projection,
/// sort and de-duplication are a tail repeated on every row-source variant. The
/// uniform shape is not a matter of taste: predicate pushdown absorbs a `Filter`
/// into the `Scan` beneath it, and parallelism inserts an exchange *between*
/// arbitrary operators. Neither is expressible against an inlined tail.
///
/// [`PhysicalPlan`]: crabgresql_planner::PhysicalPlan
pub enum BatchPipeline {
    Scan {
        table: Arc<dyn TableAm>,
        req: ScanRequest,
        schema: BatchSchema,
        /// Whether this leaf's engine serves batches natively. A leaf that does
        /// not is read a row at a time and re-batched — correct, and the only
        /// way a relation whose rows are split between a columnar store and a
        /// RAM write buffer can be scanned as one pipeline.
        columnar: bool,
    },
    /// The leaves of one relation, in order. Their concatenation is exactly what
    /// a row `Append` over the same leaves produces.
    Concat(Vec<BatchPipeline>),
    Filter {
        input: Box<BatchPipeline>,
        predicate: VectorExpr,
    },
}

impl BatchPipeline {
    /// The batch schema this pipeline produces. Every leaf of a `Concat` shares
    /// it — they are leaves of one relation, read under one projection.
    pub fn schema(&self) -> &BatchSchema {
        match self {
            BatchPipeline::Scan { schema, .. } => schema,
            BatchPipeline::Concat(leaves) => leaves
                .first()
                .map(BatchPipeline::schema)
                .expect("a concat always has at least one leaf"),
            BatchPipeline::Filter { input, .. } => input.schema(),
        }
    }

    /// Whether any leaf is served columnar. A pipeline with none would be pure
    /// overhead — every row converted to a batch and straight back again — so
    /// the gate refuses one.
    pub fn any_columnar(&self) -> bool {
        match self {
            BatchPipeline::Scan { columnar, .. } => *columnar,
            BatchPipeline::Concat(leaves) => leaves.iter().any(BatchPipeline::any_columnar),
            BatchPipeline::Filter { input, .. } => input.any_columnar(),
        }
    }
}

/// A vectorized plan: a batch pipeline, plus how its rows re-enter the row world.
pub enum VectorPlan {
    /// Grouping and aggregation over the pipeline.
    ///
    /// The accumulators are the row engine's own, fed one value at a time — what
    /// is vectorized is the scan, the decode and the predicate, which is where
    /// the rows are. The group keys and aggregate arguments are read out of
    /// narrow arrays rather than out of a rebuilt full-width tuple, so a
    /// hundred-column relation costs only the columns the query names.
    Aggregate {
        source: BatchPipeline,
        keys: Vec<VectorExpr>,
        key_types: Vec<PgType>,
        aggregates: Vec<AggregateSpec>,
    },
    /// Rows straight out of the pipeline, full width, for the row-side tail.
    Rows { source: BatchPipeline },
}

/// One aggregate, with its arguments compiled against the batch.
pub struct AggregateSpec {
    pub agg: BoundAggregate,
    pub args: Vec<VectorExpr>,
}

/// Why a plan will not vectorize.
///
/// Carried rather than discarded so that `crabgresql.vectorize = force` can name
/// the reason. Under the default setting nothing reads it: a plan is declined
/// for reasons a user cannot act on, so surfacing them would be noise.
#[derive(Clone, Debug)]
pub enum NotVectorizable {
    /// No leaf of this plan has a columnar scan, so batching would be pure cost.
    NoColumnarScan,
    /// A plan variant with no vectorized form yet.
    PlanShape(&'static str),
    /// An expression construct with no vectorized form.
    Expression(&'static str),
    /// A correlated subquery, `EXISTS`, or a quantified comparison: each needs
    /// the transaction to re-run a subplan per row.
    CorrelatedSubquery,
    /// A set-returning function: one input row is not one output row.
    SetReturning,
    /// A `LANGUAGE plpgsql` body — opaque, and PostgreSQL defaults it VOLATILE.
    Routine,
    /// A function whose invocation count or order is observable. Vectorizing
    /// changes both, which is exactly what `docs/ARCHITECTURE.md` §1.2 warns a
    /// vectorized fast path must not do.
    Volatile,
    /// An operator with no kernel.
    Operator(BinOp),
    /// A type no kernel handles under this operator.
    UnsupportedType(PgType),
    /// An ordering comparison under a collation that is not byte order.
    Collation(u32),
    /// An aggregate with no vectorized form.
    Aggregate(&'static str),
    /// An expression reads a column the scan was not asked for — a projection
    /// pushdown bug rather than a missing feature, so it declines rather than
    /// reading the wrong column.
    ColumnNotScanned { index: usize },
    /// A plan whose rows are pulled lazily with nothing blocking in between, so
    /// a batch would evaluate rows the row engine would never have reached.
    EagerUnderLimit,
    /// This node does not vectorize, but the subplan beneath it does, and the
    /// row engine will consult the gate again when it descends.
    ///
    /// Distinct from a real refusal so that `crabgresql.vectorize = force` does
    /// not report a query as un-vectorized when the expensive half of it is
    /// about to be vectorized. Reporting those alike would make the coverage
    /// assertion lie in the direction that matters.
    HandledBelow,
}

impl std::fmt::Display for NotVectorizable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotVectorizable::NoColumnarScan => f.write_str("no leaf has a columnar scan"),
            NotVectorizable::PlanShape(what) => write!(f, "{what} has no vectorized form"),
            NotVectorizable::Expression(what) => write!(f, "{what} has no vectorized form"),
            NotVectorizable::CorrelatedSubquery => f.write_str("a correlated subquery"),
            NotVectorizable::SetReturning => f.write_str("a set-returning function"),
            NotVectorizable::Routine => f.write_str("a routine call"),
            NotVectorizable::Volatile => f.write_str("a volatile function"),
            NotVectorizable::Operator(op) => write!(f, "the {op:?} operator has no kernel"),
            NotVectorizable::UnsupportedType(ty) => {
                write!(f, "no kernel for type {}", ty.name())
            }
            NotVectorizable::Collation(oid) => {
                write!(f, "ordering under collation {oid} is not byte order")
            }
            NotVectorizable::Aggregate(what) => write!(f, "{what} has no vectorized form"),
            NotVectorizable::ColumnNotScanned { index } => {
                write!(f, "column {index} is not in the scan's projection")
            }
            NotVectorizable::EagerUnderLimit => {
                f.write_str("rows are pulled lazily and the expressions can raise")
            }
            NotVectorizable::HandledBelow => {
                f.write_str("this node runs on rows, over a vectorized subplan")
            }
        }
    }
}

/// Reject any expression whose invocation count or order is observable.
///
/// Reuses the binder's own predicate rather than reimplementing it, so a
/// function that becomes volatile later is excluded here automatically.
pub fn reject_volatile(expr: &BoundExpr) -> Result<(), NotVectorizable> {
    if expr.contains_volatile_fn() {
        return Err(NotVectorizable::Volatile);
    }
    Ok(())
}
