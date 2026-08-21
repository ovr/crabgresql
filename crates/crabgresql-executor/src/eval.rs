//! Expression evaluation over one row.
//!
//! Types were settled at bind time: every `Binary` node carries its operand
//! type and `Coerce` nodes mark the only runtime casts, so evaluation
//! dispatches on recorded types and never re-infers. SQL three-valued logic
//! applies throughout: a NULL operand nulls out comparisons and arithmetic,
//! and AND/OR follow the Kleene truth tables.

use std::cmp::Ordering;

use crabgresql_binder::{BinOp, BoundExpr, MinMaxKind, ScalarFn, UnaryOp};
use crabgresql_pg_wire::sqlstate;
use crabgresql_txn::{TxnContext, XactStatus};
use crabgresql_types::text::quote_ident;
use crabgresql_types::{Interval, PgType, Value, arith, cast};

// The comparator and the `Value` accessors it is built from live in
// `crabgresql-types`, where the engine (column statistics) and the planner
// (selectivity) can reach them too. Re-exported rather than moved-and-renamed so
// every caller that knows them as `executor::compare_values` still compiles.
pub(crate) use crabgresql_types::compare::{array_elems, uuid_of, vector_elems};
use crabgresql_types::compare::{compare_elementwise, int8, oid_of};
pub use crabgresql_types::compare::{compare_values, compare_values_collated};

use crate::{CatalogOps, ExecContext, ExecError, SerialSequence};

/// The value an argument already *is*, when evaluating it would only copy it.
/// A parameter placeholder is substituted into a `Const` before the portal
/// runs, so `$1` takes this path too.
///
/// Two wrappers are transparent here and have to be, because the binder inserts
/// them around a bare column: `Collate` never touches the value, and a `Coerce`
/// into `text` from another type that is *already* `Value::Text` reaches
/// `cast_value`'s `from == to` early return. Without them the borrow path would
/// miss every `varchar` column, which is most real schemas.
///
/// The gate is on the source type, not the target. `char` (OID 18) also coerces
/// to text but holds a `Value::Char`, and unwrapping it would hand `eval_like` a
/// non-text value; `bpchar` reaches text through a `BpcharToText` call that
/// trims, and `Reinterpret` rewrites bits. All three must stay on the slow path.
pub(crate) fn arg_ref<'a>(expr: &'a BoundExpr, row: &'a [Value]) -> Option<&'a Value> {
    match expr {
        BoundExpr::Const { value, .. } => Some(value),
        BoundExpr::ColumnRef { index, .. } => row.get(*index),
        BoundExpr::Collate { expr, .. } => arg_ref(expr, row),
        BoundExpr::Coerce {
            expr,
            ty: PgType::Text,
        } if matches!(expr.ty(), PgType::Text | PgType::Varchar | PgType::Name) => {
            arg_ref(expr, row)
        }
        _ => None,
    }
}

/// Bind `&Value` for an operand that is only ever *read*: the value it already
/// is when [`arg_ref`] can point at it, and a freshly evaluated one otherwise.
/// The fast path costs a pointer rather than a deep clone of a
/// `text`/`numeric`/`bytea` column.
///
/// `$slot` is a bare `let slot;` in the caller's frame: the slow path's value
/// has to outlive the expression, and only the caller's frame does. Two tidier
/// spellings both cost more than the clone they remove, because `Value` is a
/// wide enum and each adds a discriminant to it — a returned `Cow<Value>` lost
/// 4% on TPC-H Q19's comparison loop, and a `fn` taking `&mut Option<Value>`
/// lost the same (worse with `#[inline]`). Hence a macro: it is the only form
/// that leaves the slow-path value a plain `Value` in the caller's frame.
///
/// [`arg_ref`] neither evaluates anything nor fails, so operands are still
/// evaluated in the order, and with the errors, a plain [`eval`] pair gives.
macro_rules! eval_ref {
    ($slot:ident, $expr:expr, $row:expr, $ctx:expr) => {
        match $crate::eval::arg_ref($expr, $row) {
            Some(value) => value,
            None => {
                $slot = $crate::eval::eval($expr, $row, $ctx)?;
                &$slot
            }
        }
    };
}
pub(crate) use eval_ref;

pub fn eval(expr: &BoundExpr, row: &[Value], ctx: &ExecContext) -> Result<Value, ExecError> {
    match expr {
        BoundExpr::Const { value, .. } => Ok(value.clone()),
        BoundExpr::ColumnRef { index, .. } => Ok(row[*index].clone()),
        // A `$n` placeholder is replaced with its bound `Const` value by
        // `substitute_params` before a portal executes, so evaluation never sees
        // one. Reaching here means the extended-protocol driver skipped that step
        // — an internal invariant violation, not a user error.
        BoundExpr::Param { index, .. } => Err(ExecError::new(
            sqlstate::INTERNAL_ERROR,
            format!("parameter ${} was not bound before execution", index + 1),
        )),
        // A correlated outer reference is replaced with the enclosing row's value
        // by `crabgresql_binder::substitute_outer` before its subplan runs (see
        // `crate::eval_correlated_subquery`). Reaching evaluation means that step
        // was skipped — an internal invariant break, like an unbound `Param`.
        BoundExpr::OuterColumnRef { level, index, .. } => Err(ExecError::new(
            sqlstate::INTERNAL_ERROR,
            format!(
                "outer reference (level {level}, column {index}) was not substituted before execution"
            ),
        )),
        BoundExpr::Unary { op, expr } => eval_unary(*op, eval(expr, row, ctx)?),
        // A collation labels the operand for comparison; the value is unchanged.
        BoundExpr::Collate { expr, .. } => eval(expr, row, ctx),
        BoundExpr::Binary {
            op,
            arg_ty,
            collation,
            left,
            right,
        } => eval_binary(*op, *arg_ty, *collation, left, right, row, ctx),
        BoundExpr::IsNull { expr, negated } => {
            let is_null = matches!(eval(expr, row, ctx)?, Value::Null);
            Ok(Value::Bool(is_null != *negated))
        }
        // Also total: the operand is exactly one of true/false/unknown, so
        // every test against it answers yes or no. That relies on the binder's
        // `to_bool_operand` gate — a non-boolean here would make `IS TRUE`,
        // `IS FALSE` and `IS UNKNOWN` all answer false at once, so say so
        // loudly rather than silently returning a fourth truth value.
        BoundExpr::BoolTest {
            expr,
            value,
            negated,
        } => {
            let hit = match (eval(expr, row, ctx)?, value) {
                (Value::Bool(b), Some(want)) => b == *want,
                (Value::Null, None) => true,
                (Value::Bool(_), None) | (Value::Null, Some(_)) => false,
                (other, _) => {
                    return Err(ExecError::new(
                        sqlstate::INTERNAL_ERROR,
                        format!(
                            "boolean test operand evaluated to {}, which is not boolean",
                            other.pg_type().map_or("unknown", PgType::name)
                        ),
                    ));
                }
            };
            Ok(Value::Bool(hit != *negated))
        }
        BoundExpr::Coerce { expr, ty } => coerce_value(eval(expr, row, ctx)?, *ty, ctx),
        BoundExpr::Reinterpret { expr, rep, .. } => {
            cast::reinterpret_value(eval(expr, row, ctx)?, *rep)
                .map_err(|e| ExecError::new(e.sqlstate, e.message).with_detail(e.detail))
        }
        BoundExpr::FuncCall { func, ret, args } => {
            // `LIKE` is the one scalar hot enough for the generic path's cost
            // to matter: it allocates a `Vec<Value>` and deep-clones both
            // operands — for `col LIKE 'lit'` that is two `String` copies per
            // row, on top of the match itself. When both operands are already
            // sitting somewhere borrowable, hand them over by reference.
            // Anything else (a cast, a concat, a subquery) falls through to the
            // generic path untouched. Safe to place ahead of the dispatchers
            // below because none of them handles `Like`/`ILike`.
            if matches!(func, ScalarFn::Like | ScalarFn::ILike)
                && let Some(subject) = arg_ref(&args[0], row)
                && let Some(pattern) = arg_ref(&args[1], row)
                && let Some(escape) = args
                    .get(2)
                    .map_or(Some(None), |a| arg_ref(a, row).map(Some))
            {
                return crate::scalar_fns::eval_like(
                    matches!(func, ScalarFn::ILike),
                    subject,
                    pattern,
                    escape,
                );
            }
            let arg_values = args
                .iter()
                .map(|a| eval(a, row, ctx))
                .collect::<Result<Vec<_>, _>>()?;
            // `array_cat`/`array_append`/`array_prepend` are non-strict (a NULL
            // element or side is meaningful) and need the result element type, so
            // they are dispatched here rather than through the pure `eval_scalar`.
            if let Some(result) = eval_array_ctor_fn(*func, *ret, &arg_values) {
                return result;
            }
            // The sequence functions are side-effecting and need the session's
            // sequence handle, so they are dispatched here rather than through the
            // pure `eval_scalar`.
            if let Some(result) = eval_sequence_fn(*func, &arg_values, ctx) {
                return result;
            }
            // `format_type` / `pg_get_expr` are non-strict in a way the pure
            // `eval_scalar` cannot express (`format_type` returns a name for a
            // NULL modifier), so they are dispatched here.
            if let Some(result) = eval_deparse_fn(*func, &arg_values, ctx) {
                return result;
            }
            // `pg_input_is_valid` may need the catalog too (a `reg*` target),
            // so it is dispatched here rather than in `eval_scalar`.
            if let Some(result) = eval_soft_input_fn(*func, &arg_values, ctx) {
                return result;
            }
            // `current_setting` reads the session GUC table, which the pure
            // `eval_scalar` has no handle to.
            if let Some(result) = eval_guc_fn(*func, &arg_values, ctx) {
                return result;
            }
            // The clock functions answer from the session's stamped instants
            // (or, for `clock_timestamp`, from real time) rather than from
            // their arguments — there are none.
            if let Some(result) = eval_clock_fn(*func, ctx) {
                return result;
            }
            // The uuid generators read the wall clock and fresh randomness, so
            // they sit beside the clock family rather than in `eval_scalar`.
            if let Some(result) = eval_uuid_gen_fn(*func, &arg_values, ctx) {
                return result;
            }
            // `age(xid)` reads the live transaction counter, which lives on the
            // transaction context rather than in `FmtCtx`.
            if let Some(result) = eval_txn_fn(*func, &arg_values, ctx) {
                return result;
            }
            // The catalog functions read the session's pg_catalog snapshot, which
            // the pure `eval_scalar` has no handle to.
            match eval_catalog_fn(*func, &arg_values, ctx) {
                Some(result) => result,
                None => crate::scalar_fns::eval_scalar(*func, &arg_values, &ctx.fmt),
            }
        }
        // A call to a user-defined routine the binder could not inline. The
        // interpreter lives above this crate, so the call goes out through the
        // handle the server installed on the context.
        BoundExpr::Routine {
            oid,
            name,
            strict,
            args,
            ret,
            ..
        } => {
            let arg_values = args
                .iter()
                .map(|a| eval(a, row, ctx))
                .collect::<Result<Vec<_>, _>>()?;
            // STRICT short-circuits before the body is entered, as in PG.
            if *strict && arg_values.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let routines = ctx.routines.as_ref().ok_or_else(|| {
                ExecError::new(
                    sqlstate::INTERNAL_ERROR,
                    format!("routine \"{name}\" called without a routine handle"),
                )
            })?;
            let txn = ctx.txn.as_ref().ok_or_else(|| {
                ExecError::new(
                    sqlstate::INTERNAL_ERROR,
                    format!("routine \"{name}\" called without a transaction context"),
                )
            })?;
            let value = routines.call(*oid, arg_values, ctx, txn)?;
            coerce_value(value, *ret, ctx)
        }
        BoundExpr::ArrayCtor { elem, elems, .. } => {
            let values = elems
                .iter()
                .map(|e| eval(e, row, ctx))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Array {
                elem: *elem,
                elems: values,
            })
        }
        // `a[i]`: element access. A NULL base or NULL/out-of-range subscript
        // yields NULL (PG semantics), never an error.
        BoundExpr::Subscript { base, index, .. } => {
            let base = eval(base, row, ctx)?;
            // Evaluated before the base is inspected, so a NULL base still
            // raises whatever the subscript expression raises:
            // `(NULL::int[])[1/0]` is a division-by-zero error, not NULL.
            let idx = eval(index, row, ctx)?;
            // An array's lower bound is 1, but `oidvector`/`int2vector` are
            // stored with a lower bound of 0, so `('11 22 33'::oidvector)[0]` is
            // `11` where `(array[11,22,33])[0]` is NULL. See `types::vector`.
            let (elems, lower) = match &base {
                Value::Array { elems, .. } => (elems, 1i32),
                Value::Vector { elems, .. } => (elems, 0i32),
                // NULL base → NULL element.
                Value::Null => return Ok(Value::Null),
                other => {
                    return Err(ExecError::new(
                        sqlstate::INTERNAL_ERROR,
                        format!("subscript base is not an array: {other:?}"),
                    ));
                }
            };
            let i = match idx {
                Value::Int4(i) => i,
                Value::Null => return Ok(Value::Null),
                other => {
                    return Err(ExecError::new(
                        sqlstate::INTERNAL_ERROR,
                        format!("array subscript is not an int4: {other:?}"),
                    ));
                }
            };
            // Outside `[lower, lower + len)` is NULL. The offset is computed in
            // i64 so a subscript near `i32::MIN`/`MAX` cannot wrap into range.
            let offset = i64::from(i) - i64::from(lower);
            if offset < 0 || offset >= elems.len() as i64 {
                Ok(Value::Null)
            } else {
                Ok(elems[offset as usize].clone())
            }
        }
        // CASE tests conditions top-to-bottom and evaluates only the winning
        // branch's result (false and NULL conditions both skip); a missing ELSE
        // yields NULL.
        BoundExpr::Case { whens, else_, .. } => {
            for (cond, result) in whens {
                if matches!(eval(cond, row, ctx)?, Value::Bool(true)) {
                    return eval(result, row, ctx);
                }
            }
            match else_ {
                Some(e) => eval(e, row, ctx),
                None => Ok(Value::Null),
            }
        }
        // COALESCE evaluates its arguments left to right and stops at the first
        // one that is not NULL — the rest are never evaluated, so
        // `coalesce(1, 1/0)` is 1 rather than a division-by-zero error, as in PG.
        BoundExpr::Coalesce { args, .. } => {
            for arg in args {
                let value = eval(arg, row, ctx)?;
                if !matches!(value, Value::Null) {
                    return Ok(value);
                }
            }
            Ok(Value::Null)
        }
        // The btree ordering, deliberately not `compare_values_for_aggregate`:
        // PG resolves `GREATEST` through the type's own operator class, which for
        // `oidvector` is not what `max()` compares by.
        BoundExpr::MinMax {
            kind,
            args,
            ty,
            collation,
        } => {
            let want = match kind {
                MinMaxKind::Greatest => Ordering::Greater,
                MinMaxKind::Least => Ordering::Less,
            };
            let mut best = Value::Null;
            for arg in args {
                let value = eval(arg, row, ctx)?;
                if matches!(value, Value::Null) {
                    continue;
                }
                if matches!(best, Value::Null)
                    || compare_values_collated(*ty, &value, &best, *collation) == want
                {
                    best = value;
                }
            }
            Ok(best)
        }
        // An SRF marker only expands via the `ProjectSet` node; reaching scalar
        // evaluation means it appeared where a set is not allowed (WHERE, an
        // operator argument, ORDER BY, ...). PG reports this as 0A000.
        BoundExpr::Srf { .. } => Err(ExecError::new(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "set-valued function called in context that cannot accept a set",
        )),
        // Aggregate markers are always rewritten to `ColumnRef`s (into the
        // aggregate node's output row) before planning; one reaching scalar
        // evaluation is a binder bug.
        BoundExpr::Aggregate { .. } => Err(ExecError::new(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "aggregate function called in a context that cannot accept one",
        )),
        // Likewise for window markers: the binder rewrites every one to a
        // `ColumnRef` into its `Window` node's output row, so one reaching
        // scalar evaluation is a binder bug.
        BoundExpr::WindowFunc { .. } => Err(ExecError::new(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "window function called in a context that cannot accept one",
        )),
        // A *non-correlated* subquery marker is folded to a constant/comparison by
        // `resolve_subqueries` before any node evaluates an expression, so it never
        // reaches here. A *correlated* one is left in place — its value depends on
        // the outer row — and folded now, against this row.
        BoundExpr::ScalarSubquery { .. }
        | BoundExpr::ArraySubquery { .. }
        | BoundExpr::Exists { .. }
        | BoundExpr::QuantifiedSubquery { .. } => crate::eval_correlated_subquery(expr, row, ctx),
        // `left op ANY/ALL(array)`: compare the needle (evaluated once) against
        // each element. A NULL array yields NULL. A constant array — including
        // the one a folded `op ANY/ALL (SELECT …)` becomes — is borrowed rather
        // than cloned, so a large candidate set costs nothing per row.
        BoundExpr::QuantifiedArray { array, all, cmp } => match array.as_ref() {
            BoundExpr::Const {
                value: Value::Array { elems, .. },
                ..
            } => crate::eval_quantified(cmp, elems, *all, row, ctx),
            BoundExpr::Const {
                value: Value::Null, ..
            } => Ok(Value::Null),
            _ => match eval(array, row, ctx)? {
                Value::Null => Ok(Value::Null),
                Value::Array { elems, .. } => crate::eval_quantified(cmp, &elems, *all, row, ctx),
                other => Err(ExecError::new(
                    sqlstate::INTERNAL_ERROR,
                    format!("ANY/ALL right operand is not an array: {other:?}"),
                )),
            },
        },
    }
}

/// Runtime side of a bind-time `Coerce` node, via the shared cast machinery.
/// NULL passes through any cast.
pub fn coerce_value(value: Value, ty: PgType, ctx: &ExecContext) -> Result<Value, ExecError> {
    cast::cast_value(value, ty, &ctx.fmt)
        .map_err(|e| ExecError::new(e.sqlstate, e.message).with_detail(e.detail))
}

/// Assignment-context sibling of [`coerce_value`], for PL/pgSQL's `:=`,
/// `SELECT … INTO` and `RETURN`. Unlike a bind-time `Coerce`, these are not
/// gated by the binder's explicit-cast rules, so they must not reach a cast
/// PostgreSQL only offers explicitly. See [`cast::cast_value_assign`].
pub fn coerce_value_assign(
    value: Value,
    ty: PgType,
    ctx: &ExecContext,
) -> Result<Value, ExecError> {
    cast::cast_value_assign(value, ty, &ctx.fmt)
        .map_err(|e| ExecError::new(e.sqlstate, e.message).with_detail(e.detail))
}

/// Dispatch the non-strict array constructor functions (`array_cat`,
/// `array_append`, `array_prepend`), which build a [`Value::Array`] of `ret`'s
/// element type. Returns `None` for any other function so the caller falls
/// through to the pure `eval_scalar`.
fn eval_array_ctor_fn(
    func: ScalarFn,
    ret: PgType,
    args: &[Value],
) -> Option<Result<Value, ExecError>> {
    let elem = match func {
        ScalarFn::ArrayCat | ScalarFn::ArrayAppend | ScalarFn::ArrayPrepend => match ret {
            PgType::Array(elem_oid) => PgType::from_oid(elem_oid),
            _ => None,
        },
        _ => return None,
    };
    let Some(elem) = elem else {
        return Some(Err(ExecError::new(
            sqlstate::INTERNAL_ERROR,
            "array constructor result type is not a known array type",
        )));
    };
    // Elements of an array-typed argument, or `None` when that argument is NULL.
    let elems_of = |v: &Value| -> Option<Vec<Value>> {
        match v {
            Value::Array { elems, .. } => Some(elems.clone()),
            _ => None,
        }
    };
    let result = match func {
        // `array_cat(a, b)`: a NULL side is treated as empty; both NULL → NULL.
        ScalarFn::ArrayCat => match (elems_of(&args[0]), elems_of(&args[1])) {
            (None, None) => return Some(Ok(Value::Null)),
            (a, b) => {
                let mut elems = a.unwrap_or_default();
                elems.extend(b.unwrap_or_default());
                Value::Array { elem, elems }
            }
        },
        // `array_append(arr, e)`: a NULL array is treated as empty; `e` (possibly
        // NULL) is appended.
        ScalarFn::ArrayAppend => {
            let mut elems = elems_of(&args[0]).unwrap_or_default();
            elems.push(args[1].clone());
            Value::Array { elem, elems }
        }
        // `array_prepend(e, arr)`: `e` (possibly NULL) is prepended.
        ScalarFn::ArrayPrepend => {
            let mut elems = vec![args[0].clone()];
            elems.extend(elems_of(&args[1]).unwrap_or_default());
            Value::Array { elem, elems }
        }
        _ => unreachable!(),
    };
    Some(Ok(result))
}

/// Dispatch the transaction-state functions. Returns `None` for any other
/// function (the caller falls back to the pure `eval_scalar`).
///
/// They read the transaction machinery — this transaction's id, or the CLOG
/// behind it — which the pure `eval_scalar` has no handle to. `pg_is_in_recovery`
/// needs nothing at all, and sits here because it too answers with server state
/// rather than with its arguments.
fn eval_txn_fn(
    func: ScalarFn,
    args: &[Value],
    ctx: &ExecContext,
) -> Option<Result<Value, ExecError>> {
    Some(match func {
        ScalarFn::AgeXid => eval_age_xid(args, ctx),
        ScalarFn::CurrentXactId { xid8, if_assigned } => {
            eval_current_xact_id(xid8, if_assigned, ctx)
        }
        ScalarFn::PgXactStatus => eval_xact_status(args, ctx),
        // crabgresql has no standby mode, so there is no state in which this
        // could answer otherwise.
        ScalarFn::PgIsInRecovery => Ok(Value::Bool(false)),
        _ => return None,
    })
}

/// The transaction the statement runs under, or the wiring error that says a
/// transaction-state function reached a context without one.
fn txn_of<'a>(func: &str, ctx: &'a ExecContext) -> Result<&'a TxnContext, ExecError> {
    ctx.txn.as_ref().ok_or_else(|| {
        ExecError::new(
            sqlstate::INTERNAL_ERROR,
            format!("{func} evaluated without a transaction context"),
        )
    })
}

/// `age(xid)`: how many transactions have started since `xid`. It answers from
/// the *live* counter, not from the statement's snapshot: inside one
/// repeatable-read transaction PG's answer grows as other sessions allocate
/// XIDs, while the snapshot stands still. `Clog::next_xid_floor` is that
/// counter — it is bumped at allocation — and unlike `TxnContext::xid` it is
/// meaningful in a read-only transaction, which never allocates an XID at all.
fn eval_age_xid(args: &[Value], ctx: &ExecContext) -> Result<Value, ExecError> {
    let xid = match &args[0] {
        Value::Null => return Ok(Value::Null),
        Value::Xid(x) => *x,
        other => unreachable!("expected an xid arg, got {other:?}"),
    };
    // XIDs below the first normal one are permanent, and PG reports them as
    // infinitely old rather than as a difference: `age('0'::xid)`,
    // `age('1'::xid)` and `age('2'::xid)` are all `2147483647`.
    if u64::from(xid) < crabgresql_txn::Xid::FIRST_NORMAL.0 {
        return Ok(Value::Int4(i32::MAX));
    }
    let txn = txn_of("age(xid)", ctx)?;
    // Our XIDs are 64-bit and never wrap; the SQL `xid` type is 32-bit and PG's
    // answer is a 32-bit wrapping difference reinterpreted as a signed integer.
    // That is what makes `age('4294967295'::xid)` one *more* than the counter,
    // and an xid ahead of it negative.
    let next = txn.clog.next_xid_floor().0 as u32;
    Ok(Value::Int4(next.wrapping_sub(xid) as i32))
}

/// `txid_current()` / `pg_current_xact_id()` and their `_if_assigned` forms.
///
/// The XID is not allocated here: the server allocates it with the rest of the
/// transaction, for any statement `crabgresql_binder::plan_needs_xid` reports.
/// That is what makes the id stable across an explicit block, and what commits
/// the one an autocommit statement consumed.
fn eval_current_xact_id(
    xid8: bool,
    if_assigned: bool,
    ctx: &ExecContext,
) -> Result<Value, ExecError> {
    let name = match (xid8, if_assigned) {
        (false, false) => "txid_current()",
        (false, true) => "txid_current_if_assigned()",
        (true, false) => "pg_current_xact_id()",
        (true, true) => "pg_current_xact_id_if_assigned()",
    };
    let xid = txn_of(name, ctx)?.xid;
    if !xid.is_valid() {
        if if_assigned {
            return Ok(Value::Null);
        }
        // Unreachable while `plan_needs_xid` and the allocation above it agree.
        // An error rather than a `0` so that a drift between them is a bug and
        // not a plausible-looking id.
        return Err(ExecError::new(
            sqlstate::INTERNAL_ERROR,
            format!("{name} evaluated with no transaction id assigned"),
        ));
    }
    Ok(match xid8 {
        true => Value::Xid8(xid.0),
        // Our XIDs never reach the top bit, so the narrowing is exact.
        false => Value::Int8(xid.0 as i64),
    })
}

/// `pg_xact_status(xid8)`: whether that transaction committed, aborted, or is
/// still running.
fn eval_xact_status(args: &[Value], ctx: &ExecContext) -> Result<Value, ExecError> {
    let xid = match &args[0] {
        Value::Null => return Ok(Value::Null),
        Value::Xid8(x) => crabgresql_txn::Xid(*x),
        other => unreachable!("expected an xid8 arg, got {other:?}"),
    };
    // The invalid XID names no transaction; PG answers NULL rather than raising,
    // as it does for an XID too old to have a status left.
    if !xid.is_valid() {
        return Ok(Value::Null);
    }
    let txn = txn_of("pg_xact_status", ctx)?;
    // The reserved XIDs below the first normal one are permanently committed and
    // carry no CLOG entry, which would read back as never recorded — in progress.
    if xid < crabgresql_txn::Xid::FIRST_NORMAL {
        return Ok(Value::Text("committed".into()));
    }
    // An XID at or above the next one to hand out has not started, and PG
    // refuses to guess for it rather than calling it in progress.
    if xid >= txn.clog.next_xid_floor() {
        return Err(ExecError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("transaction ID {} is in the future", xid.0),
        ));
    }
    Ok(Value::Text(
        match txn.clog.status(xid) {
            XactStatus::Committed => "committed",
            XactStatus::Aborted => "aborted",
            // A subtransaction that committed to a parent which has not is still
            // in progress as far as anyone outside it is concerned.
            XactStatus::InProgress | XactStatus::SubCommitted => "in progress",
        }
        .to_string(),
    ))
}

/// Dispatch the side-effecting sequence functions. Returns `None` for any other
/// function (the caller falls back to the pure `eval_scalar`), `Some(result)`
/// for a sequence function — including a wiring error if the context supplied no
/// [`SequenceOps`] handle. A NULL sequence-name or value argument yields NULL.
fn eval_sequence_fn(
    func: ScalarFn,
    args: &[Value],
    ctx: &ExecContext,
) -> Option<Result<Value, ExecError>> {
    if !matches!(
        func,
        ScalarFn::Nextval | ScalarFn::Currval | ScalarFn::Setval | ScalarFn::Lastval
    ) {
        return None;
    }
    let Some(ops) = ctx.sequences.as_deref() else {
        return Some(Err(ExecError::new(
            sqlstate::INTERNAL_ERROR,
            "sequence function evaluated without a sequence context",
        )));
    };
    let result = match func {
        ScalarFn::Nextval => match &args[0] {
            Value::Null => Ok(Value::Null),
            name => match seq_ref_owned(name, ctx) {
                Some((ns, seq)) => ops.nextval(ns.as_deref(), &seq).map(Value::Int8),
                None => Err(missing_catalog()),
            },
        },
        ScalarFn::Currval => match &args[0] {
            Value::Null => Ok(Value::Null),
            name => match seq_ref_owned(name, ctx) {
                Some((ns, seq)) => ops.currval(ns.as_deref(), &seq).map(Value::Int8),
                None => Err(missing_catalog()),
            },
        },
        ScalarFn::Setval => {
            // setval is STRICT: a NULL in any argument (including the optional
            // `is_called`) yields NULL with no side effect.
            let is_called = match args.get(2) {
                None => true,
                Some(Value::Bool(b)) => *b,
                _ => return Some(Ok(Value::Null)),
            };
            if matches!(args[0], Value::Null) || matches!(args[1], Value::Null) {
                Ok(Value::Null)
            } else {
                match seq_ref_owned(&args[0], ctx) {
                    Some((ns, seq)) => ops
                        .setval(ns.as_deref(), &seq, int8(&args[1]), is_called)
                        .map(Value::Int8),
                    None => Err(missing_catalog()),
                }
            }
        }
        ScalarFn::Lastval => ops.lastval().map(Value::Int8),
        _ => unreachable!("guarded by the matches! above"),
    };
    Some(result)
}

/// Dispatch the clock functions. Returns `None` for any other function.
///
/// They take no arguments and read no row, so a pure `eval_scalar` could hold
/// two of them — but not `clock_timestamp`, which reads real time. Keeping the
/// family together here is what makes "which instant does this answer with" a
/// single readable decision instead of one split across two files.
fn eval_clock_fn(func: ScalarFn, ctx: &ExecContext) -> Option<Result<Value, ExecError>> {
    let micros = match func {
        ScalarFn::TransactionTimestamp => ctx.fmt.xact_start(),
        ScalarFn::StatementTimestamp => ctx.fmt.stmt_start(),
        ScalarFn::ClockTimestamp => Ok(crabgresql_types::tz::now_micros()),
        // Not one of the session's instants: the process's, stamped once at
        // startup. It lands here because it answers with a bare instant and no
        // arguments, exactly as the three above do.
        ScalarFn::PgPostmasterStartTime => Ok(crabgresql_types::tz::postmaster_start_micros()),
        _ => return None,
    };
    Some(
        micros
            .map(Value::TimestampTz)
            .map_err(|e| ExecError::new(e.sqlstate, e.message)),
    )
}

/// Dispatch the UUID generators. Returns `None` for any other function.
///
/// They read the wall clock and fresh randomness, so — like `clock_timestamp`
/// above — a pure `eval_scalar` could not hold them. The extract functions are
/// immutable and do live there.
fn eval_uuid_gen_fn(
    func: ScalarFn,
    args: &[Value],
    ctx: &ExecContext,
) -> Option<Result<Value, ExecError>> {
    let bytes = match func {
        ScalarFn::GenRandomUuid => crate::uuid_gen::gen_v4(),
        ScalarFn::UuidV7 => crate::uuid_gen::gen_v7(crabgresql_types::tz::now_unix_nanos()),
        ScalarFn::UuidV7Shift => {
            let Value::Interval(span) = args[0] else {
                return Some(Ok(Value::Null));
            };
            // One clock reading, used for both the displacement and the stamp,
            // so the two cannot name different instants.
            let now = crabgresql_types::tz::now_unix_nanos();
            let shift_ms = match v7_shift_millis(span, now, ctx) {
                Ok(ms) => ms,
                Err(e) => return Some(Err(e)),
            };
            match crate::uuid_gen::gen_v7_shifted(now, shift_ms) {
                Some(bytes) => bytes,
                None => return Some(Err(v7_timestamp_out_of_range())),
            }
        }
        _ => return None,
    };
    Some(Ok(Value::Uuid(bytes)))
}

/// How far `uuidv7(shift)` moves the stamp, in whole milliseconds.
///
/// The displacement is *measured*, not applied: `now + span` goes through the
/// same `timestamptz + interval` helper an explicit `now() + shift` would, so a
/// month or year component moves the calendar rather than a fixed count of
/// microseconds — but only the distance is kept, and the generator adds it to
/// the latched key. That is what keeps a shifted value ordered against the
/// others while leaving the guard's state a function of the clock alone.
///
/// Whole milliseconds because `rand_a` is a precision field, not a time field:
/// folding a sub-millisecond remainder into it could carry into the stamp
/// depending on what the latch happened to hold, so `uuidv7(interval '1.0005
/// seconds')` would land on either of two milliseconds at random. PostgreSQL's
/// shifted values sit at exactly the named offset, so the remainder is dropped
/// — by `div_euclid`, which floors, keeping the mapping monotone across zero.
fn v7_shift_millis(
    span: Interval,
    now_unix_nanos: i128,
    ctx: &ExecContext,
) -> Result<i64, ExecError> {
    if !span.is_finite() {
        return Err(ExecError::new(
            sqlstate::DATETIME_FIELD_OVERFLOW,
            "interval out of range for UUID version 7",
        )
        .with_detail(Some(
            "UUID version 7 does not support infinite intervals.".into(),
        )));
    }
    let base = crabgresql_types::tz::from_unix_micros(now_unix_nanos.div_euclid(1_000) as i64);
    // Any failure inside the shift is a range failure, and reporting it as a
    // bare "timestamp out of range" would name a type the caller never wrote.
    let shifted = crabgresql_types::timestamptz::pl_interval(base, span, ctx.fmt.zone.zone())
        .map_err(|_| v7_timestamp_out_of_range())?;
    Ok(shifted.saturating_sub(base).div_euclid(1_000))
}

fn v7_timestamp_out_of_range() -> ExecError {
    ExecError::new(
        sqlstate::DATETIME_FIELD_OVERFLOW,
        "timestamp out of range for UUID version 7",
    )
    .with_detail(Some(
        "UUID version 7 supports timestamps from 1970-01-01 to approximately year 10889.".into(),
    ))
}

/// Dispatch `current_setting`, which reads the session GUC table through the
/// [`GucOps`] handle. Returns `None` for any other function.
///
/// A NULL name is NULL. An unknown parameter is `42704`, unless the optional
/// `missing_ok` argument is true, in which case the answer is NULL — PG's rule.
fn eval_guc_fn(
    func: ScalarFn,
    args: &[Value],
    ctx: &ExecContext,
) -> Option<Result<Value, ExecError>> {
    if func != ScalarFn::CurrentSetting {
        return None;
    }
    let Value::Text(name) = &args[0] else {
        return Some(Ok(Value::Null));
    };
    let missing_ok = matches!(args.get(1), Some(Value::Bool(true)));
    let Some(ops) = ctx.gucs.as_deref() else {
        return Some(Err(ExecError::new(
            sqlstate::INTERNAL_ERROR,
            "current_setting evaluated without a GUC context",
        )));
    };
    Some(match ops.show(name) {
        Some(v) => Ok(Value::Text(v)),
        None if missing_ok => Ok(Value::Null),
        None => Err(ExecError::new(
            sqlstate::UNDEFINED_OBJECT,
            format!("unrecognized configuration parameter \"{name}\""),
        )),
    })
}

/// Dispatch the catalog-reading functions. Returns `None` for any other function
/// (the caller falls back to the pure `eval_scalar`), `Some(result)` for a
/// catalog function — including a wiring error if the context supplied no
/// [`CatalogOps`] handle.
fn eval_catalog_fn(
    func: ScalarFn,
    args: &[Value],
    ctx: &ExecContext,
) -> Option<Result<Value, ExecError>> {
    if !matches!(
        func,
        ScalarFn::PgGetUserById
            | ScalarFn::PgTableIsVisible
            | ScalarFn::PgRelationSize
            | ScalarFn::PgTableSize
            | ScalarFn::PgIndexesSize
            | ScalarFn::PgTotalRelationSize
            | ScalarFn::PgIndexHasProperty
            | ScalarFn::PgIndexColumnHasProperty
            | ScalarFn::ObjDescription
            | ScalarFn::ColDescription
            | ScalarFn::TableOid
            | ScalarFn::RegIn(_)
            | ScalarFn::RegFromOid(_)
            | ScalarFn::PgTypeof(_)
            | ScalarFn::CurrentDatabase
            | ScalarFn::CurrentSchema
            | ScalarFn::CurrentSchemas
            | ScalarFn::CurrentUser
            | ScalarFn::SessionUser
            | ScalarFn::PgMyTempSchema
            | ScalarFn::PgIsOtherTempSchema
            | ScalarFn::PgBackendPid
    ) {
        return None;
    }
    // Every function here but `pg_typeof` is STRICT, and this path runs ahead of
    // `eval_scalar`'s NULL short-circuit, so the check is hand-rolled.
    // `pg_typeof(NULL)` reports the argument's declared type, not NULL. Kept
    // ahead of the handle check so a NULL argument answers NULL whether or not
    // the statement was given a catalog context.
    //
    // `args.first()` rather than a hand-listed set of the zero-argument
    // functions: several of the session-identity ones take none, and a list
    // would be a `matches!` — not exhaustive, so the next zero-arity function
    // added past it would index an empty slice and panic.
    if !matches!(func, ScalarFn::PgTypeof(_)) && matches!(args.first(), Some(Value::Null)) {
        return Some(Ok(Value::Null));
    }
    // These are the functions here that are STRICT in an argument past the first,
    // which `args.first()` above cannot see: `col_description(oid, NULL)` is
    // NULL, not the whole-object comment that a NULL read as column 0 would find,
    // a NULL property name is NULL rather than an unknown property, and a NULL
    // fork name is NULL rather than the `22023` an unrecognized one raises.
    if matches!(
        func,
        ScalarFn::ObjDescription
            | ScalarFn::ColDescription
            | ScalarFn::PgIndexHasProperty
            | ScalarFn::PgIndexColumnHasProperty
            | ScalarFn::PgRelationSize
    ) && args.iter().any(|arg| matches!(arg, Value::Null))
    {
        return Some(Ok(Value::Null));
    }
    let Some(ops) = ctx.catalog.as_deref() else {
        return Some(Err(ExecError::new(
            sqlstate::INTERNAL_ERROR,
            "catalog function evaluated without a catalog context",
        )));
    };
    match func {
        ScalarFn::CurrentDatabase => return Some(Ok(Value::Text(ops.current_database()))),
        ScalarFn::CurrentUser => return Some(Ok(Value::Text(ops.current_user()))),
        ScalarFn::SessionUser => return Some(Ok(Value::Text(ops.session_user()))),
        // The first schema an unqualified CREATE would land in. PG returns NULL
        // when the path names none that exist, which cannot happen here:
        // `public` is always on it.
        ScalarFn::CurrentSchema => {
            return Some(Ok(ops
                .search_path(false)
                .into_iter()
                .next()
                .map_or(Value::Null, Value::Text)));
        }
        ScalarFn::PgBackendPid => return Some(Ok(Value::Int4(ops.backend_pid()))),
        // PG reports 0, not NULL, before the temp namespace is instantiated.
        ScalarFn::PgMyTempSchema => {
            return Some(Ok(Value::Oid(ops.my_temp_schema().unwrap_or(0))));
        }
        // `current_schemas` takes a bool rather than an OID, so it too returns
        // before the `oid_of` below.
        ScalarFn::CurrentSchemas => {
            let include_implicit = matches!(args[0], Value::Bool(true));
            return Some(Ok(Value::Array {
                elem: crabgresql_types::PgType::Name,
                elems: ops
                    .search_path(include_implicit)
                    .into_iter()
                    .map(Value::Text)
                    .collect(),
            }));
        }
        _ => {}
    }
    // `pg_typeof` names the type the binder recorded on the call. It returns
    // before `oid_of` below, which accepts only `Value::Oid` — the argument here
    // is the user's own expression, of any type, already evaluated for its errors
    // and side effects and of no further use.
    if let ScalarFn::PgTypeof(type_oid) = func {
        return Some(Ok(Value::Reg(crate::reg::from_oid(
            crabgresql_types::RegKind::Type,
            type_oid,
            ops,
        ))));
    }
    // The binder declares the OID-taking arguments as `oid` and inserts the
    // coercion, so the value has already arrived as one — including the
    // reinterpret-not-clamp of a negative (PG prints `pg_get_userbyid(-1)` as
    // `unknown (OID=…4295)`), which `cast` owns. `RegIn` alone takes text, and
    // the functions with a `regclass` overload arrive holding the resolved
    // reference (`oid_of` panics on one).
    let oid = match &args[0] {
        Value::Text(_) => 0,
        Value::Reg(reg) => reg.oid,
        other => oid_of(other),
    };
    let value = match func {
        // PG never returns NULL here: an unresolvable OID prints a placeholder.
        ScalarFn::PgGetUserById => Value::Text(
            ops.role_name(oid)
                .unwrap_or_else(|| format!("unknown (OID={oid})")),
        ),
        // ... whereas an OID no relation has is NULL, not false.
        ScalarFn::PgTableIsVisible => ops.table_is_visible(oid).map_or(Value::Null, Value::Bool),
        // The four size functions. Each sums a different part of the same
        // measurement, so they share one catalog call; an OID naming no relation
        // is NULL for all of them, which is distinct from the zero a view (no
        // storage of its own) reports.
        ScalarFn::PgRelationSize
        | ScalarFn::PgTableSize
        | ScalarFn::PgIndexesSize
        | ScalarFn::PgTotalRelationSize => {
            // The relation is looked up **before** the fork name is read, as
            // PostgreSQL does: it opens the relation first, so
            // `pg_relation_size(999999, 'bogus')` is NULL rather than the
            // `22023` the same fork name raises for a relation that exists.
            let Some(size) = ops.relation_size(oid) else {
                return Some(Ok(Value::Null));
            };
            if let Some(fork) = args.get(1) {
                let Value::Text(fork) = fork else {
                    unreachable!("pg_relation_size fork argument was {fork:?}");
                };
                match fork.as_str() {
                    "main" => {}
                    // crabgresql keeps no free-space or visibility map and has no
                    // `init` fork, so the three non-main forks are legitimately
                    // empty rather than unimplemented.
                    "fsm" | "vm" | "init" => return Some(Ok(Value::Int8(0))),
                    // Case-sensitive, as PostgreSQL is: `'MAIN'` raises too.
                    _ => {
                        return Some(Err(ExecError::new(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            "invalid fork name",
                        )
                        .with_hint(Some(
                            "Valid fork names are \"main\", \"fsm\", \"vm\", and \"init\"."
                                .to_string(),
                        ))));
                    }
                }
            }
            let bytes = match func {
                ScalarFn::PgRelationSize => size.main,
                ScalarFn::PgTableSize => size.main + size.toast,
                ScalarFn::PgIndexesSize => size.indexes,
                _ => size.main + size.toast + size.indexes,
            };
            // No relation can approach `i64::MAX` bytes, so the cast cannot
            // wrap; `try_into` keeps that a compile-time-checked claim rather
            // than an `as` that would silently go negative if it ever did.
            Value::Int8(bytes.try_into().unwrap_or(i64::MAX))
        }
        // The index property functions. Nothing here raises: an OID that is no
        // index, a property the level does not own and a column outside the key
        // list are all NULL, verified against PostgreSQL 18.4.
        ScalarFn::PgIndexHasProperty => {
            let Value::Text(prop) = &args[1] else {
                unreachable!("pg_index_has_property property was {:?}", args[1]);
            };
            ops.index_def(oid)
                .and_then(|def| crate::index_props::index_property(def.index.method, prop))
                .map_or(Value::Null, Value::Bool)
        }
        ScalarFn::PgIndexColumnHasProperty => {
            let (Value::Int4(column), Value::Text(prop)) = (&args[1], &args[2]) else {
                unreachable!(
                    "pg_index_column_has_property arguments were {:?}",
                    &args[1..]
                );
            };
            ops.index_def(oid)
                .and_then(|def| {
                    // PostgreSQL numbers an index's columns from 1.
                    let key = usize::try_from(*column)
                        .ok()
                        .and_then(|n| n.checked_sub(1))
                        .and_then(|n| def.index.keys.get(n))?;
                    crate::index_props::index_column_property(def.index.method, key, prop)
                })
                .map_or(Value::Null, Value::Bool)
        }
        // "Other" means a temp namespace that is not this session's. An OID
        // naming nothing is `false`, not NULL — verified against PG 18.4 for 0,
        // 2200 and 999999 — and so is this session's own.
        ScalarFn::PgIsOtherTempSchema => Value::Bool(
            ops.namespace_name(oid)
                .is_some_and(|name| name.starts_with("pg_temp_"))
                && ops.my_temp_schema() != Some(oid),
        ),
        // `tableoid`: the namespace and name the binder recorded, resolved here
        // so the answer tracks the current catalog rather than the one binding
        // saw. A relation that has since been dropped reports 0 rather than
        // raising — the same choice `reg::from_oid` makes for an OID that names
        // nothing, and the row it would have described is gone anyway.
        ScalarFn::TableOid => {
            let (Value::Text(namespace), Value::Text(name)) = (&args[0], &args[1]) else {
                unreachable!("tableoid arguments were {args:?}");
            };
            Value::Oid(ops.rel_oid(Some(namespace), name).unwrap_or(0))
        }
        // The one-argument form searches every catalog at once and so can match
        // twice, where upstream's sub-select raises `21000` — so this raises it
        // too rather than picking one of the two comments.
        ScalarFn::ObjDescription => {
            let catalog = match args.get(1) {
                Some(Value::Text(name)) => Some(name.as_str()),
                None => None,
                other => unreachable!("obj_description catalog argument was {other:?}"),
            };
            let mut found = ops.object_description(oid, 0, catalog);
            if found.len() > 1 {
                return Some(Err(ExecError::new(
                    sqlstate::CARDINALITY_VIOLATION,
                    "more than one row returned by a subquery used as an expression",
                )));
            }
            found.pop().map_or(Value::Null, Value::Text)
        }
        // A column comment hangs off `pg_class`: the relation's OID in
        // `objoid`, the attribute number in `objsubid`.
        ScalarFn::ColDescription => {
            let Value::Int4(objsubid) = args[1] else {
                unreachable!("col_description column argument was {:?}", args[1]);
            };
            ops.object_description(oid, objsubid, Some("pg_class"))
                .pop()
                .map_or(Value::Null, Value::Text)
        }
        // `'name'::reg*` must find the object; `oid::reg*` takes the OID as
        // given and only resolves how it renders, so it cannot fail.
        ScalarFn::RegIn(kind) => match &args[0] {
            Value::Text(s) => match crate::reg::from_text(kind, s, ops) {
                Ok(reg) => Value::Reg(reg),
                Err(e) => return Some(Err(e)),
            },
            other => unreachable!("reg* input was {other:?}"),
        },
        ScalarFn::RegFromOid(kind) => Value::Reg(crate::reg::from_oid(kind, oid, ops)),
        _ => unreachable!("guarded by the matches! above"),
    };
    Some(Ok(value))
}

/// `pg_input_is_valid`: a type's input function run without raising.
/// Dispatched here rather than in the pure `eval_scalar` because a `reg*`
/// input function *is* a catalog lookup, and only this layer holds the handle.
fn eval_soft_input_fn(
    func: ScalarFn,
    args: &[Value],
    ctx: &ExecContext,
) -> Option<Result<Value, ExecError>> {
    if !matches!(func, ScalarFn::PgInputIsValid) {
        return None;
    }
    // STRICT, so a NULL type name is NULL rather than a resolution failure —
    // `pg_input_is_valid(NULL, 'nosuchtype')` is NULL in PG, not an error.
    let (Value::Text(value), Value::Text(type_name)) = (&args[0], &args[1]) else {
        return Some(Ok(Value::Null));
    };
    Some(soft_input_in_ctx(type_name, value, ctx).map(|bad| Value::Bool(bad.is_none())))
}

/// Run `type_name`'s input function over `value` without raising, resolving a
/// `reg*` name through the session's catalog. `Ok(None)` means valid; the
/// `Ok(Some(_))` failure is carried as an [`ExecError`] purely for its shape
/// (SQLSTATE + message + DETAIL/HINT) — it is a value to report, not a raise.
/// Only an unusable *type spec* comes back as `Err`.
pub(crate) fn soft_input_in_ctx(
    type_name: &str,
    value: &str,
    ctx: &ExecContext,
) -> Result<Option<ExecError>, ExecError> {
    let spec = crabgresql_binder::TypeSpec::resolve(type_name)?;
    // `regclassin` and friends fail softly when the object is missing, which is
    // exactly what these functions are asked to report.
    if let PgType::Reg(kind) = spec.ty {
        let ops = ctx.catalog.as_deref().ok_or_else(|| {
            ExecError::new(
                sqlstate::INTERNAL_ERROR,
                "soft input for a reg* type evaluated without a catalog context",
            )
        })?;
        return Ok(crate::reg::from_text(kind, value, ops).err());
    }
    Ok(spec.check(value, &ctx.fmt).err().map(ExecError::from))
}

/// Dispatch the type-formatting / node-tree deparse functions. Returns `None`
/// for any other function (the caller falls back to the pure `eval_scalar`).
/// These run ahead of `eval_scalar` because they are not uniformly STRICT:
/// `format_type` must return a type name when only its modifier is NULL.
fn eval_deparse_fn(
    func: ScalarFn,
    args: &[Value],
    ctx: &ExecContext,
) -> Option<Result<Value, ExecError>> {
    match func {
        ScalarFn::FormatType => Some(Ok(eval_format_type(args, ctx))),
        // crabgresql stores a `pg_node_tree` as the SQL text it deparses to,
        // written when the row is: a partition's `relpartbound` by
        // `crabgresql_catalog`'s `deparse_partbound`, a column default by the
        // binder (`deparse_literal_default` and `ruleutils::deparse_stored_expr`).
        //
        // Two things about a default belong to the *reader* rather than the
        // writer — the `pretty` parenthesisation and the session's time zone —
        // so `pg_get_expr` re-renders for them; see `ruleutils::stored_expr`.
        // Anything that is not one expression is echoed. A NULL node yields
        // NULL, as in PG.
        ScalarFn::PgGetExpr => Some(Ok(eval_pg_get_expr(args, ctx))),
        ScalarFn::PgGetViewdef => Some(eval_pg_get_viewdef(args, ctx)),
        ScalarFn::PgGetConstraintdef => Some(eval_pg_get_constraintdef(args, ctx)),
        ScalarFn::PgGetIndexdef => Some(eval_pg_get_indexdef(args, ctx)),
        ScalarFn::PgGetSerialSequence => Some(eval_pg_get_serial_sequence(args, ctx)),
        _ => None,
    }
}

/// `pg_get_serial_sequence(relation, column)`: the sequence that column owns,
/// schema-qualified and quoted the way PostgreSQL prints it
/// (`dp2."MixT_id_seq"`).
///
/// The relation argument is a *name*, resolved by
/// [`crate::reg::resolve_relation`] — which is also where the `42P01`/`3F000`
/// wording lives, and why `pg_get_serial_sequence('123', …)` reports a missing
/// relation rather than reading 123 as an OID.
///
/// The **column** argument is matched literally: `'ColX'` finds a column named
/// `"ColX"` and `'colx'` raises `42703`, as observed on PostgreSQL 18.4. A
/// column that owns no sequence is NULL, not an error.
///
/// Strict, stated here rather than inherited for the reason
/// [`eval_pg_get_indexdef`] gives.
fn eval_pg_get_serial_sequence(args: &[Value], ctx: &ExecContext) -> Result<Value, ExecError> {
    if args.iter().any(|arg| matches!(arg, Value::Null)) {
        return Ok(Value::Null);
    }
    let (Value::Text(relation), Value::Text(column)) = (&args[0], &args[1]) else {
        return Ok(Value::Null);
    };
    let Some(ops) = ctx.catalog.as_deref() else {
        return Err(ExecError::new(
            sqlstate::INTERNAL_ERROR,
            "pg_get_serial_sequence evaluated without a catalog context",
        ));
    };
    let (relation, ..) = crate::reg::resolve_relation(relation, ops)?;
    match ops.serial_sequence(relation, column) {
        SerialSequence::Owned { namespace, name } => Ok(Value::Text(format!(
            "{}.{}",
            quote_ident(&namespace),
            quote_ident(&name)
        ))),
        SerialSequence::Unowned | SerialSequence::NoRelation => Ok(Value::Null),
        SerialSequence::NoColumn { relation } => Err(ExecError::new(
            sqlstate::UNDEFINED_COLUMN,
            format!("column \"{column}\" of relation \"{relation}\" does not exist"),
        )),
    }
}

/// `pg_get_constraintdef(oid[, pretty])`: the constraint's DDL text.
///
/// An OID no constraint answers to yields **NULL** — verified against
/// PostgreSQL 18.4, which does not raise for it here.
///
/// A check constraint is doubly parenthesised in the non-pretty form:
/// `CHECK ((x > 3))`, the outer pair from the `CHECK` syntax and the inner from
/// `pg_get_expr`. `pretty` drops the inner one, giving psql's `CHECK (x > 3)`.
///
/// TODO: parenthesise a comparison's operator operands the way PostgreSQL's
/// pretty mode does — there `CHECK (x + y < 100)` renders as
/// `CHECK ((x + y) < 100)`, while [`crabgresql_binder::ruleutils`] drops the
/// pair by precedence and renders `CHECK (x + y < 100)`. Only a comparison's
/// operands take the extra pair: under `AND`/`OR`/`NOT`, and between arithmetic
/// operators, PostgreSQL goes by precedence as well — verified against
/// PostgreSQL 18.4, where `CHECK (x > 1 AND y > 2 OR z)` agrees exactly and
/// `CHECK ((x / 2 + 1) > 0)` differs only in that outer pair. The fix belongs
/// in `ruleutils` rather than here, and moves more than CHECK output: the same
/// deparser renders every column default in the tree. The non-pretty form —
/// what `pg_constraint.conbin` stores and what `information_schema` reads —
/// already agrees exactly.
fn eval_pg_get_constraintdef(args: &[Value], ctx: &ExecContext) -> Result<Value, ExecError> {
    let Value::Oid(oid) = &args[0] else {
        return Ok(Value::Null);
    };
    let pretty = matches!(args.get(1), Some(Value::Bool(true)));
    let Some(catalog) = ctx.catalog.as_deref() else {
        return Err(ExecError::new(
            sqlstate::INTERNAL_ERROR,
            "pg_get_constraintdef evaluated without a catalog context",
        ));
    };
    let Some(def) = catalog.constraint_def(*oid) else {
        return Ok(Value::Null);
    };
    let columns = || def.columns.join(", ");
    let text = match def.contype.as_str() {
        "c" => {
            let expr = def.expr.as_deref().unwrap_or_default();
            // Re-rendered rather than echoed, so the reader's `pretty` flag and
            // session time zone apply — the same round trip `pg_get_expr` makes.
            let rendered = crabgresql_binder::ruleutils::stored_expr(expr, pretty, &ctx.fmt)
                .unwrap_or_else(|| expr.to_string());
            format!("CHECK ({rendered})")
        }
        "p" => format!("PRIMARY KEY ({})", columns()),
        "u" => format!("UNIQUE ({})", columns()),
        "n" => format!("NOT NULL {}", columns()),
        // A contype this build does not render is better reported as absent
        // than as a definition that omits half of itself.
        _ => return Ok(Value::Null),
    };
    Ok(Value::Text(text))
}

/// `pg_get_indexdef(oid[, column, pretty])`. Three outcomes, all observed on
/// PostgreSQL 18.4: an OID no index answers to is NULL; a `column` of `0` (or
/// the one-argument form) is the whole `CREATE INDEX` statement; a non-zero
/// `column` is that key alone, bare — no `DESC`, no null placement — and a
/// `column` past the end of the key list is the **empty string**, not NULL.
///
/// The statement itself is rendered by
/// [`crabgresql_storage_api::index_definition`], shared with
/// `pg_indexes.indexdef`; `pretty` changes only line breaking, which neither
/// form produces.
///
/// Strict in every argument (`pg_proc.proisstrict` is true for both overloads on
/// 18.4), which has to be said here rather than inherited: this is dispatched
/// from [`eval`] alongside `format_type`, not from the STRICT `eval_scalar` path
/// that short-circuits a NULL for its callers. Without it a NULL `column` would
/// read as `0` and return the whole statement where PostgreSQL returns NULL.
fn eval_pg_get_indexdef(args: &[Value], ctx: &ExecContext) -> Result<Value, ExecError> {
    if args.iter().any(|arg| matches!(arg, Value::Null)) {
        return Ok(Value::Null);
    }
    let Value::Oid(oid) = &args[0] else {
        return Ok(Value::Null);
    };
    let Some(catalog) = ctx.catalog.as_deref() else {
        return Err(ExecError::new(
            sqlstate::INTERNAL_ERROR,
            "pg_get_indexdef evaluated without a catalog context",
        ));
    };
    let Some(def) = catalog.index_def(*oid) else {
        return Ok(Value::Null);
    };
    let column = match args.get(1) {
        Some(Value::Int4(n)) => *n,
        _ => 0,
    };
    if column == 0 {
        return Ok(Value::Text(crabgresql_storage_api::index_definition(
            &def.index, &def.table,
        )));
    }
    // PostgreSQL numbers the key list from 1.
    let key = usize::try_from(column)
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|n| def.index.keys.get(n));
    let Some(key) = key else {
        return Ok(Value::Text(String::new()));
    };
    let name = match def.table.columns.get(key.column) {
        Some(column) => crabgresql_types::text::quote_ident(&column.name),
        None => "?column?".to_string(),
    };
    Ok(Value::Text(name))
}

/// The stored deparse of a `pg_node_tree` column, re-rendered for this reader
/// (see [`eval_deparse_fn`]). The third argument is PG's `pretty` flag, which
/// defaults to false — `information_schema` leaves it off and gets the fully
/// parenthesised form, psql passes it and gets `\d`'s.
fn eval_pg_get_expr(args: &[Value], ctx: &ExecContext) -> Value {
    let Value::Text(sql) = &args[0] else {
        return args[0].clone();
    };
    let pretty = matches!(args.get(2), Some(Value::Bool(true)));
    match crabgresql_binder::ruleutils::stored_expr(sql, pretty, &ctx.fmt) {
        Some(text) => Value::Text(text),
        None => args[0].clone(),
    }
}

/// `pg_get_viewdef(name[, pretty])`. Three outcomes, all PostgreSQL's: a name no
/// relation answers to is `42P01`; a relation that is not a view is the empty
/// string; a view is its `SELECT`, re-rendered by [`crabgresql_binder::ruleutils`].
fn eval_pg_get_viewdef(args: &[Value], ctx: &ExecContext) -> Result<Value, ExecError> {
    let Value::Text(name) = &args[0] else {
        return Ok(Value::Null);
    };
    // PG's `pretty` is `PRETTYFLAG_PAREN`: it drops the parentheses that only
    // restate precedence. Absent or NULL means false, as in PG.
    let pretty = matches!(args.get(1), Some(Value::Bool(true)));
    let Some(catalog) = ctx.catalog.as_deref() else {
        return Err(ExecError::new(
            sqlstate::INTERNAL_ERROR,
            "pg_get_viewdef evaluated without a catalog context",
        ));
    };
    // Resolved through the one relation-name resolver, so this and
    // `pg_get_serial_sequence` report a missing schema and a missing relation
    // in the same words.
    let (_, namespace, relation) = crate::reg::resolve_relation(name, catalog)?;
    let (namespace, relation) = (namespace.as_deref(), relation.as_str());
    let Some((sql, columns)) = catalog.view_sql(namespace, relation) else {
        // The relation exists but is not a view — PG answers with the empty
        // string rather than an error.
        return Ok(Value::Text(String::new()));
    };
    // A view whose body the deparser cannot render must *not* also answer with
    // the empty string: that is the "not a view" answer, and returning it here
    // would let a dump silently drop the view body. Say so instead.
    crabgresql_binder::ruleutils::view_definition(&sql, pretty, &columns)
        .map(Value::Text)
        .ok_or_else(|| {
            ExecError::new(
                sqlstate::FEATURE_NOT_SUPPORTED,
                format!("pg_get_viewdef cannot deparse the definition of view \"{name}\""),
            )
        })
}

/// `format_type(oid, typmod)`. A NULL oid yields NULL; oid `0` is `-`; an oid
/// nothing in the catalog claims is `???`. The modifier is decoded in
/// PostgreSQL's `atttypmod` encoding (see
/// [`Column::atttypmod`](crabgresql_storage_api::Column::atttypmod)), so this is
/// the inverse that reproduces PG's `\d` strings.
///
/// A NULL modifier and the `-1` modifier are *not* the same input: PostgreSQL
/// tracks whether one was given at all, and `bpchar` reports itself differently
/// for each (see [`format_type_text`]).
fn eval_format_type(args: &[Value], ctx: &ExecContext) -> Value {
    let oid = match &args[0] {
        Value::Null => return Value::Null,
        v => oid_of(v),
    };
    let typmod = match args.get(1) {
        Some(Value::Int4(m)) => Some(*m),
        // Absent or SQL NULL: no modifier was given.
        _ => None,
    };
    Value::Text(format_type_text(oid, typmod, ctx.catalog.as_deref()))
}

/// The body of `format_type`: PostgreSQL's SQL spelling of type `oid` with
/// `typmod` applied. Resolves the oid, then defers the per-type spelling to
/// [`PgType::format_type`] (which the binder shares, so a deparsed constant's
/// type label and `\d`'s Type column cannot drift apart).
fn format_type_text(oid: u32, typmod: Option<i32>, catalog: Option<&dyn CatalogOps>) -> String {
    if oid == 0 {
        return "-".to_string();
    }
    let Some(ty) = PgType::from_oid(oid) else {
        // Not a built-in: a pseudo-type names itself from the shared table, and a
        // `CREATE TYPE` type resolves through the catalog. Both are the lookups
        // `regtype` renders through, so the two agree on a name. Anything else is
        // `???`, as in PG.
        return crabgresql_types::pseudo_type_name(oid)
            .map(str::to_string)
            .or_else(|| {
                catalog
                    .and_then(|ops| ops.user_type_name(oid))
                    .map(|(_, name)| quote_ident(&name))
            })
            .unwrap_or_else(|| "???".to_string());
    };
    // An array formats its element type (carrying the modifier) with `[]`; a
    // user-defined type needs the catalog. Both are the cases `format_type`
    // declines, and both recurse through the oid path above.
    if let PgType::Array(elem) = ty {
        return format!("{}[]", format_type_text(elem, typmod, catalog));
    }
    ty.format_type(typmod)
        .unwrap_or_else(|| ty.name().to_string())
}

/// Split a `nextval`/`currval`/`setval` text argument into `(namespace, name)`:
/// the last `.` separates an optional schema qualifier from the sequence name
/// (`app.s` → `(Some("app"), "s")`, `s` → `(None, "s")`).
///
/// TODO: normalize this name the way `regclass` input does — quoting,
/// case-folding and `search_path` resolution — instead of splitting on the
/// last `.`.
fn seq_ref(v: &Value) -> (Option<&str>, &str) {
    match v {
        Value::Text(s) => match s.rsplit_once('.') {
            Some((schema, name)) => (Some(schema), name),
            None => (None, s),
        },
        other => unreachable!("sequence name argument was {other:?}"),
    }
}

/// A `regclass` sequence argument reached a context with no catalog handle, or
/// names a relation that has since gone: the same internal wiring error the
/// other catalog-less paths report.
fn missing_catalog() -> ExecError {
    ExecError::new(
        sqlstate::INTERNAL_ERROR,
        "sequence function could not resolve a regclass argument",
    )
}

/// The `(namespace, name)` a sequence-function argument denotes. A `regclass`
/// argument already resolved its OID at cast time, so the pair comes from the
/// catalog rather than from re-parsing the rendered name — which would be
/// ambiguous for a quoted name containing a `.`.
fn seq_ref_owned(v: &Value, ctx: &ExecContext) -> Option<(Option<String>, String)> {
    match v {
        Value::Reg(r) => {
            let ops = ctx.catalog.as_deref()?;
            let (ns, name) = ops.rel_name(r.oid)?;
            // Qualify only what an unqualified name would not reach, so an error
            // about a visible relation names it the way the caller wrote it —
            // `"s" is not a sequence`, matching both the `text` spelling of
            // these functions and PG.
            let ns = (ops.table_is_visible(r.oid) != Some(true)).then_some(ns);
            Some((ns, name))
        }
        other => {
            let (ns, name) = seq_ref(other);
            Some((ns.map(str::to_string), name.to_string()))
        }
    }
}

/// Unary operators, delegated to [`crabgresql_types::arith`] so the optimizer's
/// constant folding and this per-row path cannot disagree on overflow.
fn eval_unary(op: UnaryOp, operand: Value) -> Result<Value, ExecError> {
    arith::eval_unary(unary_arith_op(op), operand).map_err(arith_error)
}

/// The binder's `UnaryOp` in the types crate's spelling. A plain rename: the
/// binder sits above `crabgresql-types`, so the shared implementation cannot
/// name its enum.
fn unary_arith_op(op: UnaryOp) -> arith::UnaryArithOp {
    match op {
        UnaryOp::Not => arith::UnaryArithOp::Not,
        UnaryOp::Neg => arith::UnaryArithOp::Neg,
        UnaryOp::Abs => arith::UnaryArithOp::Abs,
        UnaryOp::Sqrt => arith::UnaryArithOp::Sqrt,
        UnaryOp::Cbrt => arith::UnaryArithOp::Cbrt,
    }
}

/// The arithmetic subset of `BinOp`, likewise. Only reached under
/// [`BinOp::is_arithmetic`].
fn arith_op(op: BinOp) -> arith::ArithOp {
    match op {
        BinOp::Add => arith::ArithOp::Add,
        BinOp::Sub => arith::ArithOp::Sub,
        BinOp::Mul => arith::ArithOp::Mul,
        BinOp::Div => arith::ArithOp::Div,
        BinOp::Mod => arith::ArithOp::Mod,
        BinOp::Pow => arith::ArithOp::Pow,
        other => unreachable!("{other:?} is not arithmetic"),
    }
}

pub(crate) fn arith_error(e: arith::ArithError) -> ExecError {
    ExecError::new(e.sqlstate, e.message).with_detail(e.detail)
}

fn eval_binary(
    op: BinOp,
    arg_ty: PgType,
    collation: u32,
    left: &BoundExpr,
    right: &BoundExpr,
    row: &[Value],
    ctx: &ExecContext,
) -> Result<Value, ExecError> {
    // AND/OR evaluate lazily left-to-right, as PG does at runtime.
    if let BinOp::And | BinOp::Or = op {
        return eval_logic(op, left, right, row, ctx);
    }
    // Both the arithmetic accessors below and `apply_comparison` read through a
    // `&Value`, so neither side has to be owned: for `text_col = 'lit'` that
    // saves two `String` copies per row, for `numeric` two heap allocations.
    let (l_slot, r_slot);
    let l = eval_ref!(l_slot, left, row, ctx);
    let r = eval_ref!(r_slot, right, row, ctx);
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    if op.is_arithmetic() {
        return arith::eval_arith(arith_op(op), arg_ty, l, r).map_err(arith_error);
    }
    Ok(Value::Bool(apply_comparison(op, arg_ty, collation, l, r)))
}

/// Apply a comparison operator to two already-evaluated, non-NULL operands of
/// `arg_ty`, ordering strings under `collation`. Split out of [`eval_binary`] so
/// the quantified comparisons (`op ANY/ALL`) resolve each candidate exactly as a
/// written comparison does, without rebuilding an expression node per candidate.
pub(crate) fn apply_comparison(
    op: BinOp,
    arg_ty: PgType,
    collation: u32,
    l: &Value,
    r: &Value,
) -> bool {
    // Every supported collation is deterministic, so equal bytes and equal
    // values coincide (see `compare_values`'s doc comment) — equality never
    // needs the collation-aware path, so skip straight past the ICU collator.
    if matches!(op, BinOp::Eq | BinOp::NotEq) {
        let eq = compare_values(arg_ty, l, r).is_eq();
        return if op == BinOp::Eq { eq } else { !eq };
    }
    let ordering = compare_values_collated(arg_ty, l, r, collation);
    match op {
        BinOp::Lt => ordering.is_lt(),
        BinOp::LtEq => ordering.is_le(),
        BinOp::Gt => ordering.is_gt(),
        BinOp::GtEq => ordering.is_ge(),
        _ => unreachable!(),
    }
}

/// Whether `ty` has a default btree operator class, and so may be sorted or key
/// an index. Callers that would otherwise reach [`compare_values`] on user input
/// (e.g. a RANGE partition key) must gate on this.
///
/// This is [`PgType::has_default_btree_opclass`] — kept as a re-export here
/// because that is the name the executor's callers know it by, but delegating
/// rather than restating the list, so the two cannot drift.
///
/// Note it is *narrower* than the set [`compare_values`] handles: `xid` has a
/// `compare_values` arm (needed so `keys_equal` can settle grouping equality)
/// yet returns `false` here, because PG gives it a hash opclass and no btree
/// one. So `false` no longer implies "`compare_values` would panic" — it means
/// "nothing may sort by this".
pub fn is_orderable(ty: PgType) -> bool {
    ty.has_default_btree_opclass()
}

/// Kleene three-valued AND/OR with left-to-right lazy evaluation: the right
/// side only runs when the left side has not decided the result.
fn eval_logic(
    op: BinOp,
    left: &BoundExpr,
    right: &BoundExpr,
    row: &[Value],
    ctx: &ExecContext,
) -> Result<Value, ExecError> {
    // The operand value that decides the result on its own: false for AND,
    // true for OR.
    let decisive = op == BinOp::Or;
    let l = eval(left, row, ctx)?;
    if let Value::Bool(b) = l
        && b == decisive
    {
        return Ok(Value::Bool(decisive));
    }
    let r = eval(right, row, ctx)?;
    Ok(match (l, r) {
        (_, Value::Bool(b)) if b == decisive => Value::Bool(decisive),
        (Value::Null, _) | (_, Value::Null) => Value::Null,
        _ => Value::Bool(!decisive),
    })
}

/// The ordering `min`/`max` use, which is **not** always the btree ordering.
///
/// PostgreSQL resolves `min`/`max` on `oidvector` through the polymorphic
/// `min(anyarray)`/`max(anyarray)` aggregates rather than through the type's own
/// operator class, so the aggregates compare element-wise even though `ORDER BY`
/// compares the element count first. The two deliberately disagree in PG, and
/// reproducing that is the only way `max()` returns the row PG returns:
/// `max(VALUES '9 8'::oidvector, '1 1 1')` is `9 8`, not `1 1 1`.
///
/// Every other type — including `int2vector`, which has no opclass to diverge
/// from — orders identically here and in [`compare_values_collated`].
pub fn compare_values_for_aggregate(ty: PgType, l: &Value, r: &Value, collation: u32) -> Ordering {
    match ty {
        PgType::Vector(kind) => {
            compare_elementwise(kind.element(), vector_elems(l), vector_elems(r))
        }
        _ => compare_values_collated(ty, l, r, collation),
    }
}

pub(crate) fn out_of_range(ty: PgType) -> ExecError {
    arith_error(arith::out_of_range(ty))
}

#[cfg(test)]
mod format_type_tests {
    use super::{eval_format_type, format_type_text};
    use crate::{CatalogOps, ExecContext};
    use crabgresql_types::{Value, oid};
    use std::sync::Arc;

    /// `format_type` with no catalog behind it — the built-in-only path.
    fn ft(oid: u32, typmod: Option<i32>) -> String {
        format_type_text(oid, typmod, None)
    }

    /// Every expectation here was probed against PostgreSQL 18.4
    /// (`SELECT format_type(oid, typmod)`). The modifier is in PG's `atttypmod`
    /// encoding — the one `crabgresql_catalog`'s `pg_attribute` emits — so this
    /// is the decode side of that contract.
    #[test]
    fn format_type_matches_postgres() {
        // No modifier: the plain SQL spelling, which is not the `typname`
        // (`integer`, not `int4`).
        assert_eq!(ft(oid::INT4, None), "integer");
        assert_eq!(ft(oid::NUMERIC, None), "numeric");
        assert_eq!(ft(oid::BPCHAR, None), "character");
        assert_eq!(ft(oid::TEXT, None), "text");
        // numeric packs (precision, scale) into the two halves above the
        // varlena header: 262150 = ((4 << 16) | 2) + 4.
        assert_eq!(ft(oid::NUMERIC, Some(262150)), "numeric(4,2)");
        // The character types reserve four bytes for that header, so the
        // declared length is the modifier minus 4.
        assert_eq!(ft(oid::VARCHAR, Some(24)), "character varying(20)");
        assert_eq!(ft(oid::BPCHAR, Some(14)), "character(10)");
        // Bit lengths are stored directly, with no header allowance.
        assert_eq!(ft(oid::BIT, Some(5)), "bit(5)");
        assert_eq!(ft(oid::VARBIT, Some(5)), "bit varying(5)");
        // The precision goes *before* the time-zone suffix, not at the end.
        assert_eq!(
            ft(oid::TIMESTAMP, Some(3)),
            "timestamp(3) without time zone"
        );
        assert_eq!(ft(oid::TIMESTAMPTZ, Some(3)), "timestamp(3) with time zone");
        assert_eq!(ft(oid::TIME, Some(3)), "time(3) without time zone");
        assert_eq!(ft(oid::TIMETZ, Some(3)), "time(3) with time zone");
        // `interval` is the one modifier that carries two things: the fields it
        // admits and the precision. Every value here is the `atttypmod`
        // PostgreSQL 18.4 stores for a column declared that way.
        assert_eq!(ft(oid::INTERVAL, Some(-1)), "interval");
        assert_eq!(ft(oid::INTERVAL, Some(2147418115)), "interval(3)");
        assert_eq!(ft(oid::INTERVAL, Some(327679)), "interval year");
        assert_eq!(ft(oid::INTERVAL, Some(458751)), "interval year to month");
        assert_eq!(ft(oid::INTERVAL, Some(268435458)), "interval second(2)");
        assert_eq!(
            ft(oid::INTERVAL, Some(470286340)),
            "interval day to second(4)"
        );
        assert_eq!(
            ft(oid::INTERVAL, Some(402653184)),
            "interval minute to second(0)"
        );
        // A `reg*` type spells as its own name.
        assert_eq!(ft(oid::REGCLASS, None), "regclass");
        // An array formats its element type, carrying the modifier, plus `[]`.
        assert_eq!(ft(oid::INT4_ARRAY, None), "integer[]");
        assert_eq!(ft(oid::VARCHAR_ARRAY, Some(24)), "character varying(20)[]");
        // The two sentinels: OID 0 is `-`, an OID no type has is `???`.
        assert_eq!(ft(0, None), "-");
        assert_eq!(ft(0, Some(5)), "-");
        assert_eq!(ft(999_999, None), "???");
    }

    /// A modifier below its type's threshold prints nothing, rather than a
    /// negative or zero length that is not valid SQL. The thresholds differ per
    /// type and were probed: the character types need more than the four-byte
    /// header they reserve, `numeric` needs at least it, the rest need only a
    /// non-negative value.
    #[test]
    fn modifier_below_its_threshold_prints_nothing() {
        assert_eq!(ft(oid::VARCHAR, Some(2)), "character varying");
        assert_eq!(ft(oid::VARCHAR, Some(4)), "character varying");
        assert_eq!(ft(oid::VARCHAR, Some(5)), "character varying(1)");
        assert_eq!(ft(oid::BPCHAR, Some(4)), "bpchar");
        assert_eq!(ft(oid::BPCHAR, Some(5)), "character(1)");
        assert_eq!(ft(oid::NUMERIC, Some(3)), "numeric");
        assert_eq!(ft(oid::NUMERIC, Some(4)), "numeric(0,0)");
        assert_eq!(ft(oid::NUMERIC, Some(5)), "numeric(0,1)");
        assert_eq!(ft(oid::TIMESTAMP, Some(-1)), "timestamp without time zone");
        assert_eq!(ft(oid::VARBIT, Some(-1)), "bit varying");
    }

    /// `numeric`'s scale is an 11-bit *signed* field and its precision is masked
    /// to the 16 bits above it, so a negative-scale numeric round trips.
    #[test]
    fn numeric_scale_is_signed() {
        // PostgreSQL stores numeric(4,-2) as atttypmod 264194.
        assert_eq!(ft(oid::NUMERIC, Some(264_194)), "numeric(4,-2)");
        assert_eq!(ft(oid::NUMERIC, Some(i32::MAX)), "numeric(32767,-5)");
    }

    /// `bpchar` is the one type that distinguishes "a modifier was given, but it
    /// is the no-modifier value" from "no modifier at all": the former is
    /// `bpchar`, the latter `character`. An unmodified `bpchar` column stores
    /// `-1`, so this is what `\d` prints for one.
    #[test]
    fn bpchar_reports_which_spelling_it_was_asked_about() {
        assert_eq!(ft(oid::BPCHAR, None), "character");
        assert_eq!(ft(oid::BPCHAR, Some(-1)), "bpchar");
        // Only bpchar does this; varchar reads the same either way.
        assert_eq!(ft(oid::VARCHAR, None), "character varying");
        assert_eq!(ft(oid::VARCHAR, Some(-1)), "character varying");
    }

    /// A `CREATE TYPE` type resolves through the catalog rather than falling to
    /// `???`, so `format_type` and `regtype` agree on what to call it.
    #[test]
    fn user_type_resolves_through_the_catalog() {
        struct OneEnum;
        impl CatalogOps for OneEnum {
            fn role_name(&self, _oid: u32) -> Option<String> {
                None
            }
            fn table_is_visible(&self, _oid: u32) -> Option<bool> {
                None
            }
            fn relation_size(&self, _oid: u32) -> Option<crate::RelationSize> {
                None
            }
            fn rel_name(&self, _oid: u32) -> Option<(String, String)> {
                None
            }
            fn rel_oid(&self, _namespace: Option<&str>, _name: &str) -> Option<u32> {
                None
            }
            fn proc_name(&self, _oid: u32) -> Option<String> {
                None
            }
            fn proc_oid(&self, _namespace: Option<&str>, _name: &str) -> Option<u32> {
                None
            }
            fn oper_signature(&self, _oid: u32) -> Option<crate::CatalogOperator> {
                None
            }
            fn oper_oids(&self, _namespace: Option<&str>, _name: &str) -> Vec<u32> {
                Vec::new()
            }
            fn oper_oid(
                &self,
                _namespace: Option<&str>,
                _name: &str,
                _left: u32,
                _right: u32,
            ) -> Option<u32> {
                None
            }
            fn object_description(
                &self,
                _objoid: u32,
                _objsubid: i32,
                _catalog: Option<&str>,
            ) -> Vec<String> {
                Vec::new()
            }
            fn namespace_name(&self, _oid: u32) -> Option<String> {
                None
            }
            fn namespace_oid(&self, _name: &str) -> Option<u32> {
                None
            }
            fn user_type_name(&self, oid: u32) -> Option<(String, String)> {
                (oid == 16_384).then(|| ("public".to_string(), "mood".to_string()))
            }
            fn user_type_oid(&self, _namespace: Option<&str>, _name: &str) -> Option<u32> {
                None
            }
            fn view_sql(
                &self,
                _namespace: Option<&str>,
                _name: &str,
            ) -> Option<(String, Vec<String>)> {
                None
            }
            fn constraint_def(&self, _oid: u32) -> Option<crate::ConstraintDef> {
                None
            }
            fn index_def(&self, _oid: u32) -> Option<crate::IndexDef> {
                None
            }
            fn partition_ancestors(&self, _oid: u32) -> Vec<u32> {
                Vec::new()
            }
            fn serial_sequence(&self, _oid: u32, _column: &str) -> crate::SerialSequence {
                crate::SerialSequence::NoRelation
            }
            fn available_extensions(&self) -> Vec<crate::ExtensionVersion> {
                Vec::new()
            }
            fn current_database(&self) -> String {
                "postgres".to_string()
            }
            fn current_user(&self) -> String {
                "postgres".to_string()
            }
            fn session_user(&self) -> String {
                "postgres".to_string()
            }
            fn search_path(&self, _include_implicit: bool) -> Vec<String> {
                vec!["public".to_string()]
            }
            fn my_temp_schema(&self) -> Option<u32> {
                None
            }
            fn backend_pid(&self) -> i32 {
                1
            }
        }

        let ctx = ExecContext {
            catalog: Some(Arc::new(OneEnum)),
            ..ExecContext::default()
        };
        assert_eq!(
            eval_format_type(&[Value::Oid(16_384), Value::Int4(-1)], &ctx),
            Value::Text("mood".to_string())
        );
        // An OID the catalog does not claim is still `???`.
        assert_eq!(
            eval_format_type(&[Value::Oid(999_999), Value::Int4(-1)], &ctx),
            Value::Text("???".to_string())
        );
    }

    /// The argument-level contract: `format_type` is strict in its OID but *not*
    /// in its modifier — a NULL modifier means "no modifier", which is why this
    /// function bypasses `eval_scalar`'s STRICT short-circuit. psql's sequence
    /// query relies on it (`format_type(seqtypid, NULL)`).
    #[test]
    fn null_oid_is_null_but_null_typmod_is_no_modifier() {
        let ctx = ExecContext::default();
        assert_eq!(
            eval_format_type(&[Value::Null, Value::Int4(24)], &ctx),
            Value::Null
        );
        assert_eq!(
            eval_format_type(&[Value::Oid(oid::VARCHAR), Value::Null], &ctx),
            Value::Text("character varying".to_string())
        );
        assert_eq!(
            eval_format_type(&[Value::Oid(oid::VARCHAR), Value::Int4(24)], &ctx),
            Value::Text("character varying(20)".to_string())
        );
    }
}

#[cfg(test)]
mod arg_ref_tests {
    use super::arg_ref;
    use crabgresql_binder::BoundExpr;
    use crabgresql_types::{PgType, Value};

    fn col(ty: PgType) -> BoundExpr {
        BoundExpr::ColumnRef { index: 0, ty }
    }

    fn coerce_to_text(expr: BoundExpr) -> BoundExpr {
        BoundExpr::Coerce {
            expr: Box::new(expr),
            ty: PgType::Text,
        }
    }

    /// The borrow path exists to skip a `String` clone per row, so which
    /// wrappers it sees through decides whether it fires at all — a `varchar`
    /// column reaches `LIKE` as `Coerce{ColumnRef, Text}`, never bare.
    #[test]
    fn borrows_through_runtime_identity_wrappers() {
        let row = [Value::Text("abc".into())];
        let collate = |e| BoundExpr::Collate {
            expr: Box::new(e),
            collation: 950,
            explicit: true,
        };
        for (label, expr) in [
            ("text", col(PgType::Text)),
            ("varchar", coerce_to_text(col(PgType::Varchar))),
            ("name", coerce_to_text(col(PgType::Name))),
            ("collated text", collate(col(PgType::Text))),
            (
                "collated varchar",
                coerce_to_text(collate(col(PgType::Varchar))),
            ),
        ] {
            assert_eq!(arg_ref(&expr, &row), Some(&row[0]), "{label} should borrow");
        }
    }

    /// These reach text through a conversion that changes the value, so
    /// borrowing the operand would hand `eval_like` something that is not text.
    #[test]
    fn refuses_wrappers_that_change_the_value() {
        let row = [Value::Char(b'a')];
        for (label, expr) in [
            ("char (OID 18)", coerce_to_text(col(PgType::Char))),
            ("int4", coerce_to_text(col(PgType::Int4))),
            (
                "binary-coercible",
                BoundExpr::Reinterpret {
                    expr: Box::new(col(PgType::Text)),
                    reported: PgType::Text,
                    rep: PgType::Text,
                },
            ),
        ] {
            assert_eq!(arg_ref(&expr, &row), None, "{label} must not borrow");
        }
    }
}

#[cfg(test)]
mod min_max_tests {
    use crabgresql_binder::{BoundExpr, MinMaxKind};
    use crabgresql_types::collation::DEFAULT_COLLATION_OID;
    use crabgresql_types::{PgType, Value, VectorKind};

    use crate::testutil::eval_const;

    fn min_max(kind: MinMaxKind, ty: PgType, values: &[Value], collation: u32) -> BoundExpr {
        BoundExpr::MinMax {
            kind,
            args: values
                .iter()
                .map(|value| BoundExpr::Const {
                    value: value.clone(),
                    ty,
                })
                .collect(),
            ty,
            collation,
        }
    }

    fn pick(kind: MinMaxKind, ty: PgType, values: &[Value]) -> Value {
        crate::testutil::test_ok(eval_const(&min_max(
            kind,
            ty,
            values,
            DEFAULT_COLLATION_OID,
        )))
    }

    #[test]
    fn nulls_are_skipped_and_an_all_null_list_is_null() {
        let ints = [Value::Null, Value::Int4(3), Value::Null, Value::Int4(1)];
        assert_eq!(
            pick(MinMaxKind::Greatest, PgType::Int4, &ints),
            Value::Int4(3)
        );
        assert_eq!(pick(MinMaxKind::Least, PgType::Int4, &ints), Value::Int4(1));
        assert_eq!(
            pick(
                MinMaxKind::Greatest,
                PgType::Int4,
                &[Value::Null, Value::Null]
            ),
            Value::Null
        );
    }

    /// PG's total order puts NaN above every number.
    #[test]
    fn nan_is_the_greatest_float() {
        let floats = [Value::Float8(f64::NAN), Value::Float8(1.0)];
        assert!(matches!(
            pick(MinMaxKind::Greatest, PgType::Float8, &floats),
            Value::Float8(v) if v.is_nan()
        ));
        assert_eq!(
            pick(MinMaxKind::Least, PgType::Float8, &floats),
            Value::Float8(1.0)
        );
    }

    /// `"char"` orders unsigned (PG's `btcharcmp`), so `'\377' > 'Z'` — and
    /// unlike `min`/`max`, which resolve through `text`, the value stays a byte.
    #[test]
    fn char_orders_unsigned() {
        let bytes = [Value::Char(b'Z'), Value::Char(0o377)];
        assert_eq!(
            pick(MinMaxKind::Greatest, PgType::Char, &bytes),
            Value::Char(0o377)
        );
    }

    /// `oidvector`'s own operator class compares the element *count* first, which
    /// is why PG's `GREATEST` and `max()` disagree on it.
    #[test]
    fn oidvector_compares_by_the_btree_order() {
        let vectors = [
            Value::Vector {
                kind: VectorKind::Oid,
                elems: vec![Value::Oid(9), Value::Oid(8)],
            },
            Value::Vector {
                kind: VectorKind::Oid,
                elems: vec![Value::Oid(1), Value::Oid(1), Value::Oid(1)],
            },
        ];
        let ty = PgType::Vector(VectorKind::Oid);
        assert_eq!(pick(MinMaxKind::Greatest, ty, &vectors), vectors[1]);
        assert_eq!(pick(MinMaxKind::Least, ty, &vectors), vectors[0]);
    }

    /// Both cases are needed: byte order puts `'a'` (0x61) above `'B'` (0x42),
    /// while a linguistic collation puts `'B'` on top.
    #[test]
    fn strings_compare_under_the_nodes_collation() {
        let strings = [Value::Text("B".into()), Value::Text("a".into())];
        let by_name = |name: &str| {
            crabgresql_types::collation::lookup_by_name(name)
                .unwrap_or_else(|| panic!("{name} is a built-in collation"))
                .oid
        };
        for (collation, winner) in [(by_name("C"), "a"), (by_name("en-US-x-icu"), "B")] {
            let expr = min_max(MinMaxKind::Greatest, PgType::Text, &strings, collation);
            assert_eq!(
                crate::testutil::test_ok(eval_const(&expr)),
                Value::Text(winner.into()),
                "collation {collation}"
            );
        }
    }

    #[test]
    fn every_argument_is_evaluated() {
        let divide_by_zero = BoundExpr::Binary {
            op: crabgresql_binder::BinOp::Div,
            arg_ty: PgType::Int4,
            collation: DEFAULT_COLLATION_OID,
            left: Box::new(BoundExpr::Const {
                value: Value::Int4(1),
                ty: PgType::Int4,
            }),
            right: Box::new(BoundExpr::Const {
                value: Value::Int4(0),
                ty: PgType::Int4,
            }),
        };
        let expr = BoundExpr::MinMax {
            kind: MinMaxKind::Greatest,
            args: vec![
                BoundExpr::Const {
                    value: Value::Int4(1),
                    ty: PgType::Int4,
                },
                divide_by_zero,
            ],
            ty: PgType::Int4,
            collation: DEFAULT_COLLATION_OID,
        };
        let e = eval_const(&expr).expect_err("the second argument must be evaluated");
        assert_eq!(e.code, crabgresql_pg_wire::sqlstate::DIVISION_BY_ZERO);
    }
}
