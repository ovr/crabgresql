//! Which parts of a plan can run on Arrow batches.
//!
//! These rules live here, in the planner, for the reason
//! [`PhysicalJoinExpr::uses_hash_join`](crate::PhysicalJoinExpr::uses_hash_join)
//! does: **the executor and `EXPLAIN` must make the same decision.** `EXPLAIN`
//! annotates a node as columnar, and an annotation the executor disagrees with
//! is worse than none — it reports work that never happened.
//!
//! So this module is the single source of truth for *whether*, and
//! `crabgresql_executor::vector` only decides *how*. The executor gates its
//! compilers on these functions and may decline further (an operand shape it has
//! not implemented yet), but it must never vectorize something rejected here.
//!
//! Everything is a pure function of the plan, so no Arrow dependency is needed
//! at this layer.

use crabgresql_binder::{BinOp, BoundExpr, DistinctKey, ScalarFn, SortKey, UnaryOp};
use crabgresql_types::text::LikeMatcher;
use crabgresql_types::{PgType, Value, collation};

use crate::{PhysicalAppendArm, PhysicalPlan};

/// Which parts of one plan node run columnar. Rendered by
/// [`explain`](crate::explain) and consulted by the executor as it builds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vectorization {
    /// The scan hands up Arrow batches instead of tuples.
    pub scan: bool,
    /// The `WHERE` runs as an Arrow filter, below the row boundary.
    pub filter: bool,
    /// The `ORDER BY` (with its projection) runs as an Arrow sort.
    pub sort: bool,
}

impl Vectorization {
    pub fn any(self) -> bool {
        self.scan || self.filter || self.sort
    }

    /// The `EXPLAIN` suffix for a node, or `None` when nothing vectorized.
    ///
    /// A divergence from PostgreSQL's output, taken deliberately: a plan that
    /// runs on a different engine than it appears to is not worth the
    /// compatibility. Only plans that actually vectorize are affected, so every
    /// row-path plan still renders exactly as before.
    pub fn suffix(self) -> Option<String> {
        if !self.any() {
            return None;
        }
        let parts = [
            (self.scan, "scan"),
            (self.filter, "filter"),
            (self.sort, "sort"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect::<Vec<_>>()
        .join(", ");
        Some(format!(" (columnar: {parts})"))
    }
}

/// Whether values of `ty` compare in Arrow exactly as PostgreSQL compares them,
/// under `op` and `collation`.
///
/// Each exclusion is a case where Arrow's answer would be *wrong*, not merely
/// different:
///
/// - `numeric` has no Arrow type (arbitrary precision), so it is stored as text
///   and an Arrow comparison would compare text: `'9' > '10'`.
/// - `float4`/`float8` — Arrow's comparison kernels are not IEEE `==` but
///   bitwise, i.e. IEEE's totalOrder predicate. So `-0.0 = 0.0` is false where
///   PostgreSQL says true, and two NaNs of different bit patterns compare
///   unequal where PostgreSQL calls every NaN one value. (Floats *can* be sort
///   keys — see [`sortable_key`].)
/// - `bpchar` compares with trailing blanks trimmed.
/// - `timetz`/`interval` are structs whose PostgreSQL order is not
///   lexicographic over their fields.
/// - text *ordering* follows the collation, and an ICU collation is not byte
///   order. Equality is allowed under any collation, because every supported
///   collation is deterministic and so equality is bytewise regardless.
pub fn comparable(ty: PgType, op: BinOp, collation: u32) -> bool {
    match ty {
        PgType::Bool
        | PgType::Int2
        | PgType::Int4
        | PgType::Int8
        | PgType::Date
        | PgType::Time
        | PgType::Timestamp
        | PgType::TimestampTz
        | PgType::Bytea
        | PgType::Uuid => true,
        PgType::Text | PgType::Varchar | PgType::Name => {
            matches!(op, BinOp::Eq | BinOp::NotEq) || collation::is_byte_order(collation)
        }
        _ => false,
    }
}

/// A `LIKE`/`ILIKE` call that can run as a columnar filter: the subject to
/// evaluate per row, and the pattern already compiled.
pub struct LikeCall<'a> {
    /// The operand the matcher runs against, with value-transparent wrappers
    /// already peeled — a column or a constant, as [`vectorizable_operand`]
    /// requires.
    pub subject: &'a BoundExpr,
    pub matcher: LikeMatcher,
}

/// Whether this `LIKE`/`ILIKE` call can run as a columnar filter, and with what
/// compiled pattern.
///
/// Unlike every other rule here this one returns the *artifact* rather than a
/// verdict, and deliberately: the executor must not be able to decide
/// differently from `EXPLAIN`, and handing back the compiled matcher makes that
/// structural rather than a convention two functions have to keep.
///
/// Both callers really do call this — the gate
/// ([`vectorizable_predicate`]) throws the matcher away and keeps the `is_some`,
/// then the executor's compiler calls it again for the matcher it holds. That
/// is the price of the gate, not an oversight, and it is not a second compile:
/// [`LikeMatcher::compile`] goes through the thread's pattern cache, so the
/// pattern is compiled once per thread and every later call is a lookup.
///
/// There is no Arrow `like` kernel involved. Arrow's has no user `ESCAPE` and
/// its own idea of case folding; PostgreSQL's semantics live in
/// [`crabgresql_types::text`], and running exactly that matcher over the batch
/// is what makes the columnar answer provably the row answer.
///
/// The pattern and the `ESCAPE` clause must be non-NULL constants:
///
/// - a computed pattern would have to compile per row, which is the row path
///   with extra steps;
/// - a NULL pattern or escape makes the predicate NULL for every row. Correct
///   to vectorize, but a distinct shape with no distinct payoff.
///
/// A pattern that fails to compile — the one error `LIKE` raises, a pattern
/// ending in a bare escape character — is refused here so the row evaluator
/// raises it, per row, exactly as it always has.
pub fn like_call(func: ScalarFn, args: &[BoundExpr], width: usize) -> Option<LikeCall<'_>> {
    let case_insensitive = match func {
        ScalarFn::Like => false,
        ScalarFn::ILike => true,
        _ => return None,
    };
    let ([subject, pattern], escape) = match args {
        [subject, pattern] => ([subject, pattern], None),
        [subject, pattern, escape] => ([subject, pattern], Some(escape)),
        _ => return None,
    };
    let subject = like_subject(subject, width)?;
    let pattern = text_const(pattern)?;
    // The row path's rule (`scalar_fns::escape_char`): an absent clause is `\`,
    // an empty one disables escaping, and more than one character is an error —
    // which, like a bad pattern, is left for the row evaluator to raise.
    let escape = match escape {
        None => Some('\\'),
        Some(escape) => {
            let mut chars = text_const(escape)?.chars();
            let first = chars.next();
            chars.next().is_none().then_some(first)?
        }
    };
    let matcher = LikeMatcher::compile(pattern, escape, case_insensitive).ok()?;
    Some(LikeCall { subject, matcher })
}

/// The operand a `LIKE` matcher runs against, or `None` if this subject cannot
/// be one.
///
/// `bpchar` is absent for the same reason [`comparable`] excludes it: PostgreSQL
/// matches a `char(n)` with its trailing blanks stripped. It cannot reach here
/// anyway — the binder coerces `bpchar` to `text` through a
/// `ScalarFn::BpcharToText` call, which is computed and so refused — but naming
/// it keeps the exclusion from looking accidental.
fn like_subject(expr: &BoundExpr, width: usize) -> Option<&BoundExpr> {
    let expr = strip_relabel(expr);
    let text = matches!(expr.ty(), PgType::Text | PgType::Varchar | PgType::Name);
    (text && vectorizable_operand(expr, width)).then_some(expr)
}

/// Peel the wrappers that change how a value is *labelled* without changing the
/// value, so an operand is judged by the column or constant underneath.
///
/// Two of them:
///
/// - `Collate` — a collation decides how a *comparison* orders, and a
///   comparison reads that from its own node, never from its operand.
/// - `Coerce` to `text` over `text`/`varchar`/`name` — all three are backed by
///   `Value::Text`, whose `pg_type()` is `text`, so `cast_value` returns on
///   `from == to` and the node evaluates to its input unchanged.
///
/// The second one is load-bearing rather than cosmetic: the binder wraps a
/// `varchar`/`name` operand the moment the other side is `text`
/// (`to_text_operand`, `unify_types`), so without this peel `vc = t` and
/// `vc = 'x'::text` decline while the same column under `vc = 'x'` does not.
///
/// `bpchar` is deliberately not here. It reaches `text` through a
/// `ScalarFn::BpcharToText` call — a computed node, refused anyway — which is
/// the binder's way of saying the conversion is *not* a relabel: it strips
/// trailing blanks.
pub fn strip_relabel(expr: &BoundExpr) -> &BoundExpr {
    match expr {
        BoundExpr::Collate { expr, .. } => strip_relabel(expr),
        BoundExpr::Coerce {
            expr,
            ty: PgType::Text,
        } if matches!(expr.ty(), PgType::Text | PgType::Varchar | PgType::Name) => {
            strip_relabel(expr)
        }
        _ => expr,
    }
}

/// A non-NULL text constant, whatever text type it is labelled with.
fn text_const(expr: &BoundExpr) -> Option<&str> {
    match strip_relabel(expr) {
        BoundExpr::Const {
            value: Value::Text(s),
            ..
        } => Some(s),
        _ => None,
    }
}

/// Whether Arrow's total order over a sort key is PostgreSQL's order.
///
/// The float types qualify although [`comparable`] excludes them: the sort
/// canonicalizes `-0.0` to `0.0` and every NaN to one NaN before ordering,
/// which makes the two orders coincide. The same rewrite would repair equality
/// too, but a sort owns its key column and can rewrite it once, whereas a
/// filter would have to rewrite both operands of every comparison it evaluates.
///
/// The set itself lives with the sort that relies on it
/// ([`crabgresql_storage_api::sort::sortable`]), because the columnar engines'
/// write path asks the same question of a stored column and the two answers
/// must be one answer.
pub fn sortable_key(key: &SortKey) -> bool {
    crabgresql_storage_api::sort::sortable(key.ty, key.collation)
}

/// Whether `predicate` can run as an Arrow filter over a batch `width` columns
/// wide.
///
/// Operands are columns and constants only. Anything computed — arithmetic, a
/// cast, a bind parameter, a correlated reference — stays on the row path,
/// which is where PostgreSQL's evaluation semantics and side effects live.
///
/// The one function call admitted is `LIKE`/`ILIKE` against a constant pattern
/// ([`like_call`]), because its pattern compiles once for the whole scan rather
/// than once per row, and the compiled matcher is the same one the row path
/// runs.
pub fn vectorizable_predicate(predicate: &BoundExpr, width: usize) -> bool {
    match predicate {
        // Value-transparent; a comparison reads its collation from its own node.
        BoundExpr::Collate { expr, .. } => vectorizable_predicate(expr, width),
        BoundExpr::Binary {
            op: BinOp::And | BinOp::Or,
            left,
            right,
            ..
        } => vectorizable_predicate(left, width) && vectorizable_predicate(right, width),
        BoundExpr::Unary {
            op: UnaryOp::Not,
            expr,
        } => vectorizable_predicate(expr, width),
        BoundExpr::IsNull { expr, .. } => vectorizable_operand(expr, width),
        BoundExpr::Binary {
            op,
            arg_ty,
            collation,
            left,
            right,
        } if is_comparison(*op) => {
            comparable(*arg_ty, *op, *collation)
                && vectorizable_operand(left, width)
                && vectorizable_operand(right, width)
        }
        // `NOT LIKE` is `NOT` over this call, so the arm above covers it.
        BoundExpr::FuncCall {
            func: func @ (ScalarFn::Like | ScalarFn::ILike),
            args,
            ..
        } => like_call(*func, args, width).is_some(),
        // A bare boolean column or constant is a legal WHERE on its own.
        BoundExpr::ColumnRef {
            ty: PgType::Bool, ..
        }
        | BoundExpr::Const {
            ty: PgType::Bool, ..
        } => vectorizable_operand(predicate, width),
        _ => false,
    }
}

fn is_comparison(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq
    )
}

fn vectorizable_operand(expr: &BoundExpr, width: usize) -> bool {
    match strip_relabel(expr) {
        // Batches are full width in schema order, so a schema ordinal is a batch
        // ordinal — but only if it is actually in range.
        BoundExpr::ColumnRef { index, .. } => *index < width,
        // A constant is only usable if Arrow can hold its type. Roughly half of
        // `PgType` cannot be encoded (json, inet, arrays, …), and such a
        // constant is perfectly legal in a target list or a `WHERE` even on a
        // relation that could never store one — `SELECT id, '{}'::json FROM p
        // ORDER BY id`. Without this check the planner advertises a columnar
        // plan the executor cannot build.
        BoundExpr::Const { ty, .. } => crabgresql_storage_api::arrow::supports_type(*ty),
        _ => false,
    }
}

/// Whether a projection is a pure column take — a reorder or a constant, never
/// a computation. Only such a projection keeps the columnar segment alive as far
/// as a sort, because a [`SortKey`] indexes the *projected* tuple.
pub fn vectorizable_projection(projections: &[BoundExpr], width: usize) -> bool {
    projections
        .iter()
        .all(|expr| vectorizable_operand(expr, width))
}

/// The tail a scan-bearing plan node carries, in the order the executor applies
/// it. Grouped so [`tail_vectorization`] states the rules once for every node
/// shape that has one.
struct Tail<'a> {
    predicate: Option<&'a BoundExpr>,
    projections: &'a [BoundExpr],
    sort: &'a [SortKey],
    distinct: Option<&'a Vec<DistinctKey>>,
    /// The client-visible output width; the sort drops anything past it.
    visible: usize,
}

/// The decision for one node, given whether its scan can produce batches and how
/// wide its input rows are.
///
/// The conditions mirror the executor's pipeline exactly, and in the same order,
/// because they describe the same pipeline:
///
/// - the filter needs a batch source;
/// - the sort needs the filter to have *consumed* the predicate, since a
///   predicate left for the row `Filter` must run before the projection;
/// - the sort needs no `DISTINCT`, which is a row node that wants the hidden
///   ORDER BY columns the sort would drop;
/// - the sort needs its projection to be a pure take, since a [`SortKey`]
///   indexes the projected tuple.
fn tail_vectorization(scan: bool, width: usize, tail: Tail<'_>) -> Vectorization {
    let filter = scan
        && tail
            .predicate
            .is_some_and(|predicate| vectorizable_predicate(predicate, width));
    let predicate_gone = tail.predicate.is_none() || filter;
    let sort = scan
        && predicate_gone
        && tail.distinct.is_none()
        && !tail.sort.is_empty()
        && tail.visible <= tail.projections.len()
        && vectorizable_projection(tail.projections, width)
        && tail
            .sort
            .iter()
            .all(|key| key.column < tail.projections.len() && sortable_key(key));
    Vectorization { scan, filter, sort }
}

/// Whether every arm of an `Append` can hand up batches. All or none: their
/// outputs are concatenated, so they must share one representation.
///
/// A remapped arm disqualifies the whole node — including the arms that could
/// have handed up batches, since the outputs are concatenated and must share one
/// representation. Batches carry the arm's own column order and the batch path
/// has nowhere to apply a permutation, so it would concatenate mis-ordered
/// columns rather than fail.
///
/// Nothing pays for this today because DDL refuses an engine-managed relation on
/// *either* side of an inheritance link, so no hierarchy can contain a
/// batch-capable relation at all. That is what makes the all-or-none rule free
/// rather than a silent de-optimization of the parent.
fn arms_batch(arms: &[PhysicalAppendArm]) -> bool {
    !arms.is_empty()
        && arms
            .iter()
            .all(|arm| arm.relation.map.is_none() && arm.relation.table.supports_batch_scan())
}

impl PhysicalPlan {
    /// What this node runs on Arrow batches.
    ///
    /// Consulted by [`explain`](crate::explain) so the plan it prints is the
    /// plan that runs.
    pub fn vectorization(&self) -> Vectorization {
        match self {
            PhysicalPlan::Select {
                table,
                columns,
                projections,
                predicate,
                sort,
                distinct,
                ..
            } => tail_vectorization(
                table.supports_batch_scan(),
                table.schema().columns.len(),
                Tail {
                    predicate: predicate.as_ref(),
                    projections,
                    sort,
                    distinct: distinct.as_ref(),
                    visible: columns.len(),
                },
            ),
            // An engine-managed relation reads as an `Append` over its storage
            // leaves wrapped in a `Subquery`, and the tail lives on the wrapper.
            // Recognising that shape here is what lets such a relation report a
            // columnar filter at all.
            PhysicalPlan::Subquery {
                source,
                columns,
                projections,
                predicate,
                sort,
                distinct,
            } => {
                let PhysicalPlan::Append {
                    arms,
                    columns: append_columns,
                } = source.as_ref()
                else {
                    return Vectorization::default();
                };
                tail_vectorization(
                    arms_batch(arms),
                    // The Append's output width is the *named* relation's, which
                    // an arm's own width equals only when it does not remap.
                    append_columns.len(),
                    Tail {
                        predicate: predicate.as_ref(),
                        projections,
                        sort,
                        distinct: distinct.as_ref(),
                        visible: columns.len(),
                    },
                )
            }
            PhysicalPlan::Append { arms, .. } => Vectorization {
                scan: arms_batch(arms),
                ..Vectorization::default()
            },
            _ => Vectorization::default(),
        }
    }
}
