//! Hashed subplans: run a correlated `EXISTS` once for the whole statement
//! instead of once per outer row.
//!
//! The default path for a correlated subquery is a nested loop —
//! `eval_correlated_subquery` clones the subplan, substitutes the outer row into
//! it, plans it and runs it, for every row the marker is applied to. For
//!
//! ```sql
//! select max(unique1) from tenk1 a
//! where exists (select 1 from tenk1 b where b.thousand = a.unique2)
//! ```
//!
//! that is 10 000 full scans of a 10 000-row table: 43 s, against roughly 10 ms
//! in PostgreSQL, which turns the whole thing into a hash semi join.
//!
//! Rather than teach the planner semi joins, this module does what PostgreSQL's
//! *hashed subplan* does when it cannot decorrelate. The correlation is almost
//! always a plain equality between an inner expression and an outer column:
//!
//! ```text
//! select 1 from tenk1 b where b.thousand = <outer.unique2> and <rest>
//!                             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^     ^^^^^^
//!                             the correlation                  the residual
//! ```
//!
//! Strip the correlation conjuncts, run what is left **once**, project the inner
//! side of each stripped equality, and hash the results. Every outer row then
//! costs one hash probe. `EXISTS` becomes membership, `NOT EXISTS` its negation.
//!
//! # What disqualifies a subplan
//!
//! Everything the analysis cannot prove, because the fallback is merely slow
//! while a wrong answer is not recoverable:
//!
//! * a shape other than a single-relation `Query` with no `ORDER BY`/`DISTINCT`;
//! * a correlated conjunct that is not `inner = outer-column` — the outer side
//!   must be a bare [`BoundExpr::OuterColumnRef`] at level 1, so the probe value
//!   is simply a slot of the outer row and needs no evaluation;
//! * a key type that does not hash distinctly (interval, inet, …), for the same
//!   reason the hash join refuses one;
//! * a residual or key expression calling a volatile function or a routine —
//!   running it once instead of per row would change how many times it fires;
//! * a subplan whose id was cleared by [`Subplan::mark_rebound`], i.e. one an
//!   enclosing substitution rewrote.
//!
//! # One difference that is visible
//!
//! The residual is evaluated for every inner row rather than only for the inner
//! rows whose correlation equality already held, so a residual that raises an
//! error on a row the equality used to filter out now raises it. PostgreSQL's
//! hashed subplans have exactly the same property, and it is the same trade the
//! planner already makes when it reorders quals.

use rustc_hash::FxHashMap;
use std::sync::Arc;
use std::sync::Mutex;

use crabgresql_binder::{
    BinOp, BoundExpr, LogicalPlan, OutputColumn, QueryPlan, Subplan, SubplanId, ValuesPlan,
};
use crabgresql_txn::TxnContext;
use crabgresql_types::{PgType, Value};

use crate::{ExecContext, ExecError, agg, run_subplan};

/// The outer row slots a memoizable subplan reads, with the type each is read
/// as — the shape [`crabgresql_binder::plan_outer_ref_slots`] returns.
type Slots = Arc<[(usize, PgType)]>;

/// One memo bucket: the key values that hashed here, each with its answer.
type MemoBucket = Vec<(Vec<Value>, Value)>;

/// Everything the executor has learned about the subplans of one statement.
///
/// Created by the top-level [`execute`](crate::execute) and shared, through
/// [`ExecContext`], with every node and every nested execution — including the
/// per-row ones, which is the whole point.
#[derive(Default)]
pub struct SubplanCache {
    /// `None` records a subplan the analysis rejected, so the analysis runs once
    /// per subplan rather than once per outer row.
    exists: Mutex<FxHashMap<SubplanId, Option<Arc<HashedExists>>>>,
    /// The outer row slots each memoizable subplan reads, with their types;
    /// `None` records a subplan that may not be memoized at all.
    memo_slots: Mutex<FxHashMap<SubplanId, Option<Slots>>>,
    /// Answers already computed. `Value` is neither `Eq` nor `Hash` — its
    /// `PartialEq` is IEEE, so `NaN != NaN` — so the key values are hashed the
    /// way the aggregates and the hash join hash a group key, and a bucket hit
    /// is confirmed with `keys_equal` rather than `==`.
    memo: Mutex<FxHashMap<(SubplanId, u64), MemoBucket>>,
    /// Entries in `memo`, for the [`MEMO_CAPACITY`] check (which a `HashMap` of
    /// buckets cannot answer with `len`).
    memo_len: Mutex<usize>,
}

/// One outer row's identity for the memo: the subplan, the hash of the values it
/// reads from that row, and those values.
pub(crate) struct MemoKey {
    id: SubplanId,
    hash: u64,
    tys: Slots,
    values: Vec<Value>,
}

/// How many memoized answers one statement may accumulate.
///
/// The memo pays for itself only when outer rows repeat their key, and a query
/// whose keys are all distinct would otherwise grow one entry per row for no
/// hits at all. Past the cap the cache stops growing and keeps serving what it
/// has, so the worst case is bounded memory and the existing per-row work.
const MEMO_CAPACITY: usize = 1 << 16;

/// How many *distinct subplans* one statement may cache anything for.
///
/// A statement has finitely many subplans, so ordinarily this never binds. It
/// binds when something mints `SubplanId`s per row — re-binding a statement in a
/// loop is the way to do that — and there each entry is dead on arrival, since
/// the id is never seen again. Unlike the memo, an `exists` entry holds a
/// materialized hash of a whole inner relation, so leaving that unbounded is the
/// expensive mistake. Past the cap both caches decline and the per-row path
/// takes over.
const SUBPLAN_CAPACITY: usize = 1 << 10;

/// The hashed inner side of one correlated `EXISTS`.
pub(crate) struct HashedExists {
    /// The outer row slot feeding each key, in key order.
    outer_slots: Vec<usize>,
    key_tys: Vec<PgType>,
    /// Hash of the key tuple → the key tuples themselves, so a bucket hit that
    /// is only a hash collision is rejected on the values.
    buckets: FxHashMap<u64, Vec<Vec<Value>>>,
}

impl HashedExists {
    /// Whether the inner side holds a row matching this outer row's key.
    ///
    /// A NULL in the outer key can never satisfy `=`, so it matches nothing —
    /// the same rule that keeps NULL keys out of the table at build time, and the
    /// same one the hash join follows.
    pub(crate) fn probe(&self, row: &[Value]) -> bool {
        let mut keys = Vec::with_capacity(self.outer_slots.len());
        for slot in &self.outer_slots {
            match row.get(*slot) {
                Some(Value::Null) | None => return false,
                Some(value) => keys.push(value),
            }
        }
        let Some(bucket) = self.buckets.get(&agg::hash_key(&self.key_tys, &keys)) else {
            return false;
        };
        bucket
            .iter()
            .any(|candidate| agg::keys_equal(&self.key_tys, &keys, candidate))
    }
}

/// The hashed inner side for `subplan`, building it on first use, or `None` when
/// this subplan is not one the analysis accepts.
pub(crate) fn hashed_exists(
    subplan: &Subplan,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Option<Arc<HashedExists>>, ExecError> {
    let (Some(cache), Some(id)) = (ctx.subplans.as_ref(), subplan.id()) else {
        return Ok(None);
    };
    {
        let exists = cache.exists.lock().expect("subplan cache mutex");
        if let Some(hit) = exists.get(&id) {
            return Ok(hit.clone());
        }
        if exists.len() >= SUBPLAN_CAPACITY {
            return Ok(None);
        }
    }

    // Deliberately built with the lock released: the build runs a plan, which
    // may reach further correlated subqueries that consult this same cache.
    let built = match analyze(&subplan.plan) {
        Some(spec) => Some(Arc::new(build(spec, ctx, txn)?)),
        None => None,
    };
    cache
        .exists
        .lock()
        .expect("subplan cache mutex")
        .insert(id, built.clone());
    Ok(built)
}

/// The memo key for `subplan` under this outer `row`, or `None` when this
/// subplan may not be memoized.
///
/// Two outer rows agreeing on the slots the subplan reads run *the same plan*,
/// so they must produce the same answer — that is the whole argument for the
/// memo, and it is why the key is the values in those slots rather than the row.
///
/// A key type must hash distinctly, for a harder reason than the hash join's:
/// `agg::hash_key` contributes nothing for the types outside that set, so every
/// key would share one bucket and be resolved by `agg::keys_equal` — and
/// `compare_values` has no arm at all for the geometric and json types, so the
/// second outer row would reach its `unreachable!` rather than merely scan a
/// long bucket.
pub(crate) fn memo_key(subplan: &Subplan, row: &[Value], ctx: &ExecContext) -> Option<MemoKey> {
    let (cache, id) = (ctx.subplans.as_ref()?, subplan.id()?);
    let mut memo_slots = cache.memo_slots.lock().expect("subplan cache mutex");
    if memo_slots.len() >= SUBPLAN_CAPACITY && !memo_slots.contains_key(&id) {
        return None;
    }
    let tys = match memo_slots.entry(id) {
        std::collections::hash_map::Entry::Occupied(hit) => hit.get().clone(),
        std::collections::hash_map::Entry::Vacant(slot) => {
            // A volatile body must run as many times as it would have; a plan
            // reading past the immediately enclosing row has no key at all; and
            // a key type the hash cannot separate has no usable key either.
            let slots = (!crabgresql_binder::plan_contains_volatile_fn(&subplan.plan))
                .then(|| crabgresql_binder::plan_outer_ref_slots(&subplan.plan))
                .flatten()
                .filter(|slots| slots.iter().all(|(_, ty)| ty.hashes_distinctly()))
                .map(Arc::<[(usize, PgType)]>::from);
            slot.insert(slots).clone()
        }
    }?;
    let values: Vec<Value> = tys
        .iter()
        .map(|(slot, _)| row.get(*slot).cloned().unwrap_or(Value::Null))
        .collect();
    let hash = agg::hash_key(&key_tys(&tys), &values);
    Some(MemoKey {
        id,
        hash,
        tys,
        values,
    })
}

/// The answer already computed for this key, if any.
pub(crate) fn memo_get(key: &MemoKey, ctx: &ExecContext) -> Option<Value> {
    let tys = key_tys(&key.tys);
    ctx.subplans
        .as_ref()?
        .memo
        .lock()
        .expect("subplan cache mutex")
        .get(&(key.id, key.hash))?
        .iter()
        .find(|(values, _)| agg::keys_equal(&tys, values, &key.values))
        .map(|(_, answer)| answer.clone())
}

/// Record an answer, unless the memo has already reached [`MEMO_CAPACITY`].
pub(crate) fn memo_put(key: MemoKey, value: &Value, ctx: &ExecContext) {
    let Some(cache) = ctx.subplans.as_ref() else {
        return;
    };
    let mut len = cache.memo_len.lock().expect("subplan cache mutex");
    if *len >= MEMO_CAPACITY {
        return;
    }
    *len += 1;
    cache
        .memo
        .lock()
        .expect("subplan cache mutex")
        .entry((key.id, key.hash))
        .or_default()
        .push((key.values, value.clone()));
}

/// Just the types out of a slot list, for the hashing helpers.
fn key_tys(slots: &[(usize, PgType)]) -> Vec<PgType> {
    slots.iter().map(|(_, ty)| *ty).collect()
}

/// A subplan rewritten for one hashed build: the plan to run, and the outer row
/// slot each of its output columns is to be probed against.
struct Spec {
    plan: LogicalPlan,
    outer_slots: Vec<usize>,
    key_tys: Vec<PgType>,
}

/// Split `plan` into "the correlation" and "everything else", or give up.
fn analyze(plan: &LogicalPlan) -> Option<Spec> {
    let LogicalPlan::Query(QueryPlan {
        table,
        predicate: Some(predicate),
        sort,
        distinct,
        ..
    }) = plan
    else {
        return None;
    };
    if !sort.is_empty() || distinct.is_some() {
        return None;
    }

    let mut conjuncts = Vec::new();
    flatten_and(predicate, &mut conjuncts);

    let mut inner_keys = Vec::new();
    let mut outer_slots = Vec::new();
    let mut key_tys = Vec::new();
    let mut residual = Vec::new();
    for conjunct in conjuncts {
        match as_correlation_key(conjunct) {
            Some((inner, slot, ty)) => {
                inner_keys.push(inner.clone());
                outer_slots.push(slot);
                key_tys.push(ty);
            }
            // A conjunct still naming the outer row that is not a usable key
            // leaves the subplan correlated, so nothing can be built once.
            None if conjunct_is_correlated(conjunct) => return None,
            None => residual.push(conjunct.clone()),
        }
    }
    if inner_keys.is_empty() {
        return None;
    }
    // Running the residual once for the whole statement instead of once per
    // outer row is only invisible if it has no side effects to count. The deep
    // test, not `BoundExpr::contains_volatile_fn`: a residual conjunct may hold
    // a subquery of its own, and that body runs under this build too — the same
    // question `memo_key` asks with `plan_contains_volatile_fn`.
    if inner_keys
        .iter()
        .chain(&residual)
        .any(crabgresql_binder::expr_contains_volatile_fn)
    {
        return None;
    }

    let columns = inner_keys
        .iter()
        .enumerate()
        .map(|(i, key)| OutputColumn::new(format!("k{i}"), key.ty()))
        .collect();
    Some(Spec {
        plan: LogicalPlan::Query(QueryPlan {
            table: Arc::clone(table),
            columns,
            projections: inner_keys,
            predicate: rebuild_and(residual),
            sort: Vec::new(),
            distinct: None,
        }),
        outer_slots,
        key_tys,
    })
}

/// If `conjunct` is `inner-expression = outer-column`, its inner side, the outer
/// row slot to probe with, and the comparison type.
fn as_correlation_key(conjunct: &BoundExpr) -> Option<(&BoundExpr, usize, PgType)> {
    let BoundExpr::Binary {
        op: BinOp::Eq,
        arg_ty,
        left,
        right,
        ..
    } = conjunct
    else {
        return None;
    };
    // A type the hash cannot separate would collapse the whole inner side into
    // one bucket and then compare it value by value — slower than the nested
    // loop it replaces, for the types where `compare_values` and hashing
    // disagree.
    if !arg_ty.hashes_distinctly() {
        return None;
    }
    // Level 1 is the immediately enclosing query — the row being probed. A bare
    // reference, not an expression over one, so the probe reads a slot instead
    // of evaluating anything.
    let (inner, outer) = match (left.as_ref(), right.as_ref()) {
        (
            inner,
            BoundExpr::OuterColumnRef {
                level: 1, index, ..
            },
        ) => (inner, *index),
        (
            BoundExpr::OuterColumnRef {
                level: 1, index, ..
            },
            inner,
        ) => (inner, *index),
        _ => return None,
    };
    // The inner side must be evaluable against the inner row alone.
    if conjunct_is_correlated(inner) {
        return None;
    }
    Some((inner, outer, *arg_ty))
}

/// Whether an expression names the enclosing row (or one further out) anywhere.
fn conjunct_is_correlated(expr: &BoundExpr) -> bool {
    // `plan_has_outer_refs` answers this for a plan; wrapping the expression in
    // the smallest plan that carries one reuses the binder's depth arithmetic
    // rather than duplicating it here.
    crabgresql_binder::plan_has_outer_refs(&LogicalPlan::Values(ValuesPlan {
        columns: Vec::new(),
        rows: vec![vec![expr.clone()]],
        predicate: None,
        sort: Vec::new(),
        distinct: None,
    }))
}

/// Run the stripped subplan once and hash its *distinct* key tuples.
///
/// Only membership is ever asked of this table, so a key repeated by a thousand
/// inner rows is one entry's worth of information stored a thousand times — and
/// the table lives in the statement's cache until the statement ends. The
/// folded-`IN` path deduplicates for the same reason (`dedup_candidates`).
fn build(spec: Spec, ctx: &ExecContext, txn: &TxnContext) -> Result<HashedExists, ExecError> {
    let Spec {
        plan,
        outer_slots,
        key_tys,
    } = spec;
    let mut buckets: FxHashMap<u64, Vec<Vec<Value>>> = FxHashMap::default();
    for row in run_subplan(plan, ctx, txn)? {
        // A NULL key satisfies no equality, so it is not in the table at all.
        if row.iter().any(|v| matches!(v, Value::Null)) {
            continue;
        }
        let bucket = buckets.entry(agg::hash_key(&key_tys, &row)).or_default();
        // A bucket hit can be a hash collision, so confirm on the values.
        if bucket
            .iter()
            .any(|seen| agg::keys_equal(&key_tys, seen, &row))
        {
            continue;
        }
        bucket.push(row);
    }
    Ok(HashedExists {
        outer_slots,
        key_tys,
        buckets,
    })
}

/// Split a top-level `AND` tree into its conjuncts, by reference.
fn flatten_and<'a>(expr: &'a BoundExpr, out: &mut Vec<&'a BoundExpr>) {
    match expr {
        BoundExpr::Binary {
            op: BinOp::And,
            left,
            right,
            ..
        } => {
            flatten_and(left, out);
            flatten_and(right, out);
        }
        other => out.push(other),
    }
}

/// Re-combine conjuncts with `AND`, yielding `None` for an empty list.
fn rebuild_and(mut conjuncts: Vec<BoundExpr>) -> Option<BoundExpr> {
    let mut acc = conjuncts.pop()?;
    while let Some(next) = conjuncts.pop() {
        acc = BoundExpr::Binary {
            op: BinOp::And,
            arg_ty: PgType::Bool,
            collation: crabgresql_types::collation::DEFAULT_COLLATION_OID,
            left: Box::new(next),
            right: Box::new(acc),
        };
    }
    Some(acc)
}
