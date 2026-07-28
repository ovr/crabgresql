//! The vectorized executor: batch-at-a-time execution for analytical plans.
//!
//! `docs/ARCHITECTURE.md` §1.2 frames the constraint this crate works under:
//!
//! > PostgreSQL semantics are tied to row-at-a-time execution: the invocation
//! > order of volatile functions, side effects, cursors (`FETCH`), `LIMIT` with
//! > early termination, per-row triggers. Therefore: **v1**: a classic
//! > Volcano/iterator executor — semantically identical to PG. **v2**:
//! > vectorized fast paths for read-only plans without volatile functions
//! > (morally like JIT in PG: enabled only when it is safe).
//!
//! This is that v2 path. It is **opt-in per plan and never authoritative**: the
//! row engine is untouched, runs everything this crate declines, and is the
//! oracle every test here compares against.
//!
//! # What is vectorized
//!
//! The scan, the decode and the `WHERE` — which is where the rows are. A
//! hundred-million-row aggregate reads only the columns it names, evaluates its
//! predicate a batch at a time, and never materializes a `Vec<Value>` per row.
//!
//! Accumulation is **not** vectorized, deliberately. It reuses
//! [`crabgresql_executor::agg`] verbatim, so `sum(int8) → numeric`, `avg(int)`'s
//! 16-digit scale, `min`/`max` collation, float summation order, and the
//! first-seen group order and representative are inherited rather than
//! reimplemented. There is one implementation of those rules, not two that must
//! be kept in step.
//!
//! # Divergences
//!
//! **None currently known.** That is a measured claim, not an aspiration —
//! `crabgresql-batch/tests/arrow_assumptions.rs` pins every arrow behaviour this
//! path depends on, and the differential harness compares the two engines' wire
//! output directly.
//!
//! Two divergences were expected during design and turned out not to exist:
//!
//! * **Float comparison.** arrow-ord orders floats by IEEE 754 *totalOrder*, not
//!   by `==`, so it already agrees with PostgreSQL that `NaN = NaN` and that NaN
//!   sorts above infinity. It differs only in separating `-0.0` from `0.0`,
//!   which [`crabgresql_batch::kernels`] repairs by folding signed zero on
//!   comparison operands.
//! * **Integer overflow.** arrow's integer kernels are checked, so they raise
//!   where PostgreSQL raises. Only the message differs, and it is remapped.
//!
//! Where an exact reproduction was not available, the construct is refused
//! rather than approximated — float *arithmetic* (PostgreSQL raises on overflow
//! *and* on underflow-to-zero; arrow produces infinity and zero), `numeric`
//! (text on disk, with a display scale that is not part of the value), `bpchar`
//! (trailing blanks are insignificant to it and significant to arrow), and
//! ordering comparisons under a non-byte-order collation.
//!
//! # The gate
//!
//! [`gate::try_vectorize`] *is* the lowering: there is no separate "can this be
//! vectorized" predicate that could drift from what the builder supports. Its
//! matches over `PhysicalPlan` and `BoundExpr` have no `_` arm, so adding a
//! variant to either fails this build rather than silently acquiring a default.

use crabgresql_executor::{BatchEngine, ExecContext, ExecError, Execution, ExecNode};
use crabgresql_planner::PhysicalPlan;
use crabgresql_txn::TxnContext;

pub mod compile;
pub mod error;
pub mod gate;
pub mod nodes;
pub mod plan;

use nodes::{BatchConcat, BatchFilter, BatchNode, BatchScan, Rebatch, VectorAggregate};
use plan::{BatchPipeline, NotVectorizable, VectorPlan};

/// The vectorized executor, installed on an [`ExecContext`] by the server.
pub struct VectorEngine {
    /// Report why a plan was declined instead of falling back to the row engine.
    ///
    /// Test-only, reached through `crabgresql.vectorize = force`. It exists
    /// because a fast path nobody can observe is a fast path that can silently
    /// stop firing and still pass every correctness test: under `force`, a query
    /// the gate no longer accepts becomes a loud failure rather than an
    /// unnoticed regression.
    forcing: bool,
}

impl VectorEngine {
    pub fn new() -> Self {
        VectorEngine { forcing: false }
    }

    pub fn forcing() -> Self {
        VectorEngine { forcing: true }
    }
}

impl Default for VectorEngine {
    fn default() -> Self {
        VectorEngine::new()
    }
}

impl BatchEngine for VectorEngine {
    fn try_execute(
        &self,
        plan: &PhysicalPlan,
        ctx: &ExecContext,
        txn: &TxnContext,
    ) -> Result<Option<Execution>, ExecError> {
        let vector_plan = match gate::try_vectorize(plan) {
            Ok(vector_plan) => vector_plan,
            // Not a refusal: the row engine is about to descend into a subplan
            // this gate will accept, so insisting here would report a query as
            // un-vectorized while its expensive half is being vectorized.
            Err(NotVectorizable::HandledBelow) => return Ok(None),
            Err(reason) if self.forcing => {
                return Err(ExecError::new(
                    "0A000",
                    format!("cannot vectorize this plan: {reason}"),
                ));
            }
            // The ordinary path. A plan is declined for reasons a user cannot
            // act on, so it falls back silently and runs on rows.
            Err(_) => return Ok(None),
        };
        build(vector_plan, plan, ctx, txn).map(Some)
    }
}

/// Turn a vectorized plan into a running execution.
fn build(
    vector_plan: VectorPlan,
    plan: &PhysicalPlan,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Execution, ExecError> {
    match vector_plan {
        VectorPlan::Aggregate {
            source,
            keys,
            key_types,
            aggregates,
        } => {
            let source = build_pipeline(source, txn)?;
            let node: Box<dyn ExecNode> = Box::new(VectorAggregate::new(
                source,
                keys,
                key_types,
                aggregates,
            ));
            finish_aggregate(node, plan, ctx)
        }
        VectorPlan::Rows { .. } => Err(ExecError::new(
            "XX000",
            "the vectorized row path is not built yet",
        )),
    }
}

/// Attach the row-side tail an aggregate plan carries: `HAVING`, then the
/// projection list, `ORDER BY` and `DISTINCT`.
///
/// Calls the row engine's own `project_pipeline` rather than reproducing it. The
/// order those are applied in is semantically load-bearing, and one
/// implementation cannot drift from itself.
fn finish_aggregate(
    node: Box<dyn ExecNode>,
    plan: &PhysicalPlan,
    ctx: &ExecContext,
) -> Result<Execution, ExecError> {
    let PhysicalPlan::Aggregate {
        having,
        columns,
        projections,
        sort,
        distinct,
        ..
    } = plan
    else {
        return Err(ExecError::new(
            "XX000",
            "the vectorized aggregate was built from a plan that is not one",
        ));
    };
    let node: Box<dyn ExecNode> = match having {
        Some(having) => Box::new(crabgresql_executor::Filter::new(
            node,
            having.clone(),
            ctx.clone(),
        )),
        None => node,
    };
    crabgresql_executor::project_pipeline(
        node,
        projections.clone(),
        None,
        sort.clone(),
        distinct.clone(),
        columns.clone(),
        ctx,
    )
}

fn build_pipeline(
    pipeline: BatchPipeline,
    txn: &TxnContext,
) -> Result<Box<dyn BatchNode>, ExecError> {
    let node: Box<dyn BatchNode> = match pipeline {
        BatchPipeline::Scan {
            table,
            req,
            schema,
            columnar,
        } => {
            // `columnar` was read at plan time; re-checking it here through the
            // scan's own `Option` means a race that turned a columnar engine
            // row-only between the two degrades to a re-batched row scan rather
            // than to a wrong answer.
            match columnar
                .then(|| BatchScan::open(&table, txn, &req))
                .flatten()
            {
                Some(scan) => Box::new(scan),
                None => Box::new(Rebatch::open(&table, txn, &req, schema)),
            }
        }
        BatchPipeline::Concat(leaves) => {
            let children = leaves
                .into_iter()
                .map(|leaf| build_pipeline(leaf, txn))
                .collect::<Result<Vec<_>, _>>()?;
            Box::new(BatchConcat::new(children))
        }
        BatchPipeline::Filter { input, predicate } => {
            Box::new(BatchFilter::new(build_pipeline(*input, txn)?, predicate))
        }
    };
    Ok(node)
}

/// Why `plan` will not vectorize, or `None` if it will.
///
/// Exposed for tests: because the gate is a pure function of the physical plan,
/// a suite can assert the whole allow-list's verdict with no data and no server.
pub fn explain_refusal(plan: &PhysicalPlan) -> Option<NotVectorizable> {
    gate::try_vectorize(plan).err()
}
