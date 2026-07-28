//! The gate: which physical plans run vectorized.
//!
//! The gate **is** the lowering. There is no separate "can this be vectorized"
//! predicate that could drift from what the builder actually supports — a plan
//! is vectorizable exactly when [`try_vectorize`] returns a [`VectorPlan`] for
//! it.
//!
//! It runs in the executor rather than the planner because `execute` folds
//! non-correlated subqueries to constants before anything else; a decision taken
//! earlier would judge a plan that is not the one about to run.
//!
//! # Keeping the allow-list honest
//!
//! Every match over a plan or expression enum here has **no `_` arm**. Adding a
//! variant to `PhysicalPlan` or `BoundExpr` therefore fails this build, rather
//! than silently acquiring a default that might be wrong. That is affordable
//! precisely because these enums are coarse — 13 and 22 variants — and it is the
//! difference between a missing feature (a plan runs on rows) and a wrong answer
//! (a plan runs vectorized under rules that do not apply to it).
//!
//! The pattern deliberately avoids the two shapes already in this codebase that
//! do *not* have it: a `matches!` whitelist has an implicit `_ => false`, and a
//! `match` ending in `_ => DataType::Binary` turns a new type into an opaque
//! blob. Neither breaks when a type is added.

use std::sync::Arc;

use crabgresql_batch::{BatchField, BatchSchema};
use crabgresql_binder::{BoundAggregate, BoundExpr};
use crabgresql_binder::AggFn;
use crabgresql_planner::{
    PhysicalAggInput, PhysicalJoinExpr, PhysicalJoinInput, PhysicalPlan,
};
use crabgresql_storage_api::{ColumnProjection, ScanRequest, TableAm, TableSchema};
use crabgresql_types::PgType;
use crabgresql_types::collation::DEFAULT_COLLATION_OID;

use crate::compile::compile;
use crate::plan::{AggregateSpec, BatchPipeline, NotVectorizable, VectorPlan, reject_volatile};

/// Lower `plan` to a vectorized plan, or say why not.
pub fn try_vectorize(plan: &PhysicalPlan) -> Result<VectorPlan, NotVectorizable> {
    match plan {
        PhysicalPlan::Aggregate {
            input,
            predicate,
            group_exprs,
            aggregates,
            having,
            ..
        } => {
            // HAVING and the projection/sort tail run on the row side, above the
            // aggregate, over one row per group rather than one per input row —
            // so leaving them there costs nothing and keeps the collation-aware
            // sort and `DISTINCT ON` semantics exactly where they already work.
            //
            // HAVING is the one exception: it is a filter over group rows, and
            // the row `Filter` node handles it, so it needs no check here beyond
            // volatility.
            if let Some(having) = having {
                reject_volatile(having)?;
            }
            let source = pipeline_for_agg_input(input)?;
            let schema = source.schema().clone();

            let source = match predicate {
                Some(predicate) => {
                    reject_volatile(predicate)?;
                    BatchPipeline::Filter {
                        predicate: compile(predicate, &schema)?,
                        input: Box::new(source),
                    }
                }
                None => source,
            };

            let mut keys = Vec::with_capacity(group_exprs.len());
            let mut key_types = Vec::with_capacity(group_exprs.len());
            for group in group_exprs {
                reject_volatile(group)?;
                admit_group_key(group.ty())?;
                keys.push(compile(group, &schema)?);
                key_types.push(group.ty());
            }

            let aggregates = aggregates
                .iter()
                .map(|agg| compile_aggregate(agg, &schema))
                .collect::<Result<Vec<_>, _>>()?;

            if !source.any_columnar() {
                return Err(NotVectorizable::NoColumnarScan);
            }
            Ok(VectorPlan::Aggregate {
                source,
                keys,
                key_types,
                aggregates,
            })
        }

        // --- refused, one arm each ---------------------------------------
        //
        // `Select` is a natural next step and is deliberately not here yet: its
        // rows leave the batch world one full-width tuple at a time, so the only
        // saving is the filter, while an aggregate's rows never become tuples at
        // all. Landing the shape with the larger win first keeps the first
        // measurement honest.
        PhysicalPlan::Select { .. } => Err(NotVectorizable::PlanShape("a bare scan")),
        PhysicalPlan::Values { .. } => Err(NotVectorizable::PlanShape("a VALUES list")),
        PhysicalPlan::IndexScan { .. } => Err(NotVectorizable::PlanShape("an index scan")),
        PhysicalPlan::Subquery { .. } => Err(NotVectorizable::PlanShape("a subquery")),
        PhysicalPlan::TableFunction { .. } => Err(NotVectorizable::SetReturning),
        PhysicalPlan::Join { .. } => Err(NotVectorizable::PlanShape("a join")),
        PhysicalPlan::Append { .. } => Err(NotVectorizable::PlanShape("a bare append")),
        PhysicalPlan::SetOp { .. } => Err(NotVectorizable::PlanShape("a set operation")),
        // A `LIMIT` stops the row engine pulling, so rows past it are never
        // evaluated. A batch cannot stop mid-batch, so vectorizing *here* would
        // risk raising on a row PostgreSQL never reaches.
        //
        // The row `Limit` node re-enters `execute` for its source, though, which
        // consults this gate again — and a blocking source such as an aggregate
        // drains its whole input regardless of the limit, so it is safe there and
        // gets vectorized on the way down. Distinguishing the two cases keeps
        // `force` honest: `… GROUP BY … LIMIT 10` really is vectorized, and
        // reporting it as refused would understate coverage.
        PhysicalPlan::Limit { source, .. } => match try_vectorize(source) {
            Ok(_) => Err(NotVectorizable::HandledBelow),
            Err(_) => Err(NotVectorizable::EagerUnderLimit),
        },
        PhysicalPlan::Insert { .. }
        | PhysicalPlan::Update { .. }
        | PhysicalPlan::Delete { .. } => {
            Err(NotVectorizable::PlanShape("a data-modifying statement"))
        }
    }
}

/// The batch pipeline under an aggregate.
fn pipeline_for_agg_input(input: &PhysicalAggInput) -> Result<BatchPipeline, NotVectorizable> {
    match input {
        PhysicalAggInput::Scan { table, projection } => leaf(table, projection),
        // A relation whose storage is split into leaves — a Parquet chunk store
        // and its RAM write buffer — reaches the planner as a one-input join over
        // an `Append`. That is the *only* shape a `USING parquet` relation takes,
        // so it is the shape that matters rather than an edge case.
        PhysicalAggInput::Join(PhysicalJoinExpr::Input {
            input: join_input,
            predicate: None,
            ..
        }) => match join_input {
            PhysicalJoinInput::Scan { table, projection } => leaf(table, projection),
            PhysicalJoinInput::Subplan(plan) => match plan.as_ref() {
                PhysicalPlan::Append {
                    tables, projection, ..
                } => {
                    let leaves = tables
                        .iter()
                        .map(|table| leaf(table, projection))
                        .collect::<Result<Vec<_>, _>>()?;
                    if leaves.is_empty() {
                        return Err(NotVectorizable::PlanShape("an empty append"));
                    }
                    Ok(BatchPipeline::Concat(leaves))
                }
                _ => Err(NotVectorizable::PlanShape("a subplan under an aggregate")),
            },
            PhysicalJoinInput::TableFunction { .. } => Err(NotVectorizable::SetReturning),
        },
        PhysicalAggInput::Join(_) => Err(NotVectorizable::PlanShape("a join under an aggregate")),
        PhysicalAggInput::SingleRow => Err(NotVectorizable::PlanShape("a FROM-less aggregate")),
    }
}

/// One storage leaf, batched natively if its engine can and re-batched if not.
fn leaf(
    table: &Arc<dyn TableAm>,
    projection: &ColumnProjection,
) -> Result<BatchPipeline, NotVectorizable> {
    let schema = table.schema();
    let req = ScanRequest::new(projection.clone());
    let slots = req.slots(schema.columns.len());
    Ok(BatchPipeline::Scan {
        schema: batch_schema(schema, &slots)?,
        columnar: table.supports_batch_scan(),
        table: Arc::clone(table),
        req,
    })
}

fn batch_schema(schema: &TableSchema, slots: &[usize]) -> Result<BatchSchema, NotVectorizable> {
    let mut fields = Vec::with_capacity(slots.len());
    for &slot in slots {
        let column = schema
            .columns
            .get(slot)
            .ok_or(NotVectorizable::ColumnNotScanned { index: slot })?;
        fields.push(
            BatchField::new(
                Some(column.name.clone()),
                column.ty,
                column.typmod,
                column.nullable,
            )
            .ok_or(NotVectorizable::UnsupportedType(column.ty))?,
        );
    }
    BatchSchema::scan(fields, slots.to_vec())
        .map_err(|_| NotVectorizable::PlanShape("a malformed projection"))
}

/// Whether a type may be a grouping key.
///
/// Stricter than being representable, and for a reason that is easy to miss:
/// grouping equality is not byte equality. `hash_key` folds `-0.0` into `0.0`,
/// treats every NaN alike, ignores a `bpchar`'s trailing blanks, and hashes a
/// `numeric` through its `f64` so that `1.0` and `1.00` land in one group. A key
/// type is admitted here only when the row engine's hash and equality are the
/// ones being reused — which they are, since this path calls `hash_key` and
/// `keys_equal` directly rather than hashing arrays.
///
/// What is refused is the set whose *values* a batch cannot carry losslessly, so
/// that no group is ever split by a representation the row engine does not see.
fn admit_group_key(ty: PgType) -> Result<(), NotVectorizable> {
    match ty {
        PgType::Bool
        | PgType::Int2
        | PgType::Int4
        | PgType::Int8
        | PgType::Float4
        | PgType::Float8
        | PgType::Numeric
        | PgType::Text
        | PgType::Varchar
        | PgType::Bpchar
        | PgType::Name
        | PgType::Bytea
        | PgType::Uuid
        | PgType::Date
        | PgType::Time
        | PgType::Timestamp
        | PgType::TimestampTz => Ok(()),

        // `PgType::hashes_distinctly` reports these as sharing one hash bucket,
        // which the row engine resolves with `keys_equal`. That still works
        // here — but `interval '1 day'` and `interval '24 hours'` are equal
        // under `Interval::cmp` while their struct fields differ, so any future
        // array-level hashing would split them. Refused now so that a later
        // optimization cannot quietly change an answer.
        PgType::Interval | PgType::TimeTz => Err(NotVectorizable::UnsupportedType(ty)),

        PgType::Money
        | PgType::Oid
        | PgType::Reg(_)
        | PgType::Bit
        | PgType::Varbit
        | PgType::Inet
        | PgType::Cidr
        | PgType::Macaddr
        | PgType::Macaddr8
        | PgType::Point
        | PgType::Lseg
        | PgType::Json
        | PgType::Jsonb
        | PgType::Jsonpath
        | PgType::Tsvector
        | PgType::Tsquery
        | PgType::User(_)
        | PgType::Array(_) => Err(NotVectorizable::UnsupportedType(ty)),
    }
}

/// Compile one aggregate's arguments, refusing the forms with no vectorized
/// equivalent.
fn compile_aggregate(
    agg: &BoundAggregate,
    schema: &BatchSchema,
) -> Result<AggregateSpec, NotVectorizable> {
    if agg.distinct {
        // Needs a per-group set keyed by `hash_key`/`keys_equal`. Buildable, but
        // it is a second data structure rather than a kernel.
        return Err(NotVectorizable::Aggregate("a DISTINCT aggregate"));
    }
    match agg.func {
        // Every one of these delegates to the row engine's own accumulator, so
        // `sum(int8) -> numeric`, `avg(int)`'s scale and float summation order
        // are inherited rather than reimplemented.
        AggFn::Count | AggFn::Sum | AggFn::Avg => {}
        AggFn::Min | AggFn::Max => {
            // `min`/`max` compare under a collation. The accumulator honours it,
            // but only because it sees `Value`s — which it does here too.
            let collatable = matches!(
                agg.input_ty,
                PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name
            );
            if collatable && agg.collation != DEFAULT_COLLATION_OID {
                return Err(NotVectorizable::Collation(agg.collation));
            }
        }
        // Concatenation order is scan order, which is preserved — but the
        // delimiter argument makes it the only two-argument aggregate, and it
        // has no ORDER BY support to reason about yet.
        AggFn::StringAgg => return Err(NotVectorizable::Aggregate("string_agg")),
    }
    let args = agg
        .args
        .iter()
        .map(|arg| {
            reject_volatile(arg)?;
            compile(arg, schema)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(AggregateSpec {
        agg: agg.clone(),
        args,
    })
}

/// Every expression under an aggregate node, for the volatility sweep.
///
/// Kept beside the gate so a new expression-bearing field on the plan is noticed
/// here rather than in a wrong answer.
pub fn agg_expressions(plan: &PhysicalPlan) -> Vec<&BoundExpr> {
    let mut out = Vec::new();
    if let PhysicalPlan::Aggregate {
        predicate,
        group_exprs,
        aggregates,
        having,
        projections,
        ..
    } = plan
    {
        out.extend(predicate.iter());
        out.extend(group_exprs.iter());
        out.extend(having.iter());
        out.extend(projections.iter());
        for agg in aggregates {
            out.extend(agg.args.iter());
        }
    }
    out
}
