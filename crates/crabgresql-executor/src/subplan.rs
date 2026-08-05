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
}

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
    if let Some(hit) = cache.exists.lock().expect("subplan cache mutex").get(&id) {
        return Ok(hit.clone());
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
    // outer row is only invisible if it has no side effects to count.
    if inner_keys
        .iter()
        .chain(&residual)
        .any(|e| e.contains_volatile_fn())
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

/// Run the stripped subplan once and hash its key tuples.
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
        buckets
            .entry(agg::hash_key(&key_tys, &row))
            .or_default()
            .push(row);
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
