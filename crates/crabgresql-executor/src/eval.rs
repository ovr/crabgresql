//! Expression evaluation over one row.
//!
//! Types were settled at bind time: every `Binary` node carries its operand
//! type and `Coerce` nodes mark the only runtime casts, so evaluation
//! dispatches on recorded types and never re-infers. SQL three-valued logic
//! applies throughout: a NULL operand nulls out comparisons and arithmetic,
//! and AND/OR follow the Kleene truth tables.

use std::cmp::Ordering;

use crabgresql_binder::{BinOp, BoundExpr, ScalarFn, UnaryOp};
use crabgresql_pg_wire::sqlstate;
use crabgresql_types::collation::DEFAULT_COLLATION_OID;
use crabgresql_types::text::quote_ident;
use crabgresql_types::{
    Inet, Interval, Numeric, PgType, TimeTz, Value, VectorKind, bit, cast, collation, date, float,
    interval, json, money, net, time, timetz, tsquery, tsvector,
};

use crate::{CatalogOps, ExecContext, ExecError};

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
                .map_err(|e| ExecError::new(e.sqlstate, e.message))
        }
        BoundExpr::FuncCall { func, ret, args } => {
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
        // An array constructor: evaluate each element and collect into a
        // `Value::Array` of the declared element type.
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
    cast::cast_value(value, ty, &ctx.fmt).map_err(|e| ExecError::new(e.sqlstate, e.message))
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
    cast::cast_value_assign(value, ty, &ctx.fmt).map_err(|e| ExecError::new(e.sqlstate, e.message))
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
/// `age(xid)` is how many transactions have started since `xid`. It answers
/// from the *live* counter, not from the statement's snapshot: inside one
/// repeatable-read transaction PG's answer grows as other sessions allocate
/// XIDs, while the snapshot stands still. `Clog::next_xid_floor` is that
/// counter — it is bumped at allocation — and unlike `TxnContext::xid` it is
/// meaningful in a read-only transaction, which never allocates an XID at all.
fn eval_txn_fn(
    func: ScalarFn,
    args: &[Value],
    ctx: &ExecContext,
) -> Option<Result<Value, ExecError>> {
    if func != ScalarFn::AgeXid {
        return None;
    }
    let xid = match &args[0] {
        Value::Null => return Some(Ok(Value::Null)),
        Value::Xid(x) => *x,
        other => unreachable!("expected an xid arg, got {other:?}"),
    };
    // XIDs below the first normal one are permanent, and PG reports them as
    // infinitely old rather than as a difference: `age('0'::xid)`,
    // `age('1'::xid)` and `age('2'::xid)` are all `2147483647`.
    if u64::from(xid) < crabgresql_txn::Xid::FIRST_NORMAL.0 {
        return Some(Ok(Value::Int4(i32::MAX)));
    }
    let Some(txn) = ctx.txn.as_ref() else {
        return Some(Err(ExecError::new(
            sqlstate::INTERNAL_ERROR,
            "age(xid) evaluated without a transaction context",
        )));
    };
    // Our XIDs are 64-bit and never wrap; the SQL `xid` type is 32-bit and PG's
    // answer is a 32-bit wrapping difference reinterpreted as a signed integer.
    // That is what makes `age('4294967295'::xid)` one *more* than the counter,
    // and an xid ahead of it negative.
    let next = txn.clog.next_xid_floor().0 as u32;
    Some(Ok(Value::Int4(next.wrapping_sub(xid) as i32)))
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
///
/// Both functions are STRICT, but this path runs ahead of `eval_scalar`'s NULL
/// short-circuit, so a NULL argument is handled here.
fn eval_catalog_fn(
    func: ScalarFn,
    args: &[Value],
    ctx: &ExecContext,
) -> Option<Result<Value, ExecError>> {
    if !matches!(
        func,
        ScalarFn::PgGetUserById
            | ScalarFn::PgTableIsVisible
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
    // `unknown (OID=…4295)`), which `cast` owns. `RegIn` alone takes text.
    let oid = match args[0] {
        Value::Text(_) => 0,
        _ => oid_of(&args[0]),
    };
    let value = match func {
        // PG never returns NULL here: an unresolvable OID prints a placeholder.
        ScalarFn::PgGetUserById => Value::Text(
            ops.role_name(oid)
                .unwrap_or_else(|| format!("unknown (OID={oid})")),
        ),
        // ... whereas an OID no relation has is NULL, not false.
        ScalarFn::PgTableIsVisible => ops.table_is_visible(oid).map_or(Value::Null, Value::Bool),
        // "Other" means a temp namespace that is not this session's. An OID
        // naming nothing is `false`, not NULL — verified against PG 18.4 for 0,
        // 2200 and 999999 — and so is this session's own.
        ScalarFn::PgIsOtherTempSchema => Value::Bool(
            ops.namespace_name(oid)
                .is_some_and(|name| name.starts_with("pg_temp_"))
                && ops.my_temp_schema() != Some(oid),
        ),
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
        _ => None,
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
/// **Known divergence, in `ruleutils` rather than here:** PostgreSQL's pretty
/// mode keeps the parentheses around an operator nested in another operator, so
/// `CHECK (x + y < 100)` renders as `CHECK ((x + y) < 100)`, while
/// [`crabgresql_binder::ruleutils`] drops them by precedence and renders
/// `CHECK (x + y < 100)`. The non-pretty form — what `pg_constraint.conbin`
/// stores and what `information_schema` reads — agrees exactly. This predates
/// CHECK support (the same deparser renders column defaults) and is left alone
/// here rather than changed underneath every DEFAULT in the tree.
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
    // The same identifier rules every other name-taking catalog function uses:
    // an unquoted part folds to lower case, a `"quoted"` one keeps its spelling.
    let Some((namespace, relation)) = crate::reg::split_qualified_name(name) else {
        return Err(ExecError::new(
            sqlstate::UNDEFINED_TABLE,
            format!("relation \"{name}\" does not exist"),
        ));
    };
    let (namespace, relation) = (namespace.as_deref(), relation.as_str());
    if catalog.rel_oid(namespace, relation).is_none() {
        return Err(ExecError::new(
            sqlstate::UNDEFINED_TABLE,
            format!("relation \"{name}\" does not exist"),
        ));
    }
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
/// (`app.s` → `(Some("app"), "s")`, `s` → `(None, "s")`). Full `regclass` name
/// normalization (quoting, case-folding, search_path) is a v1 gap.
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

fn eval_unary(op: UnaryOp, operand: Value) -> Result<Value, ExecError> {
    match (op, operand) {
        (_, Value::Null) => Ok(Value::Null),
        (UnaryOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (UnaryOp::Neg, Value::Int4(v)) => v
            .checked_neg()
            .map(Value::Int4)
            .ok_or_else(|| out_of_range(PgType::Int4)),
        (UnaryOp::Neg, Value::Int8(v)) => v
            .checked_neg()
            .map(Value::Int8)
            .ok_or_else(|| out_of_range(PgType::Int8)),
        (UnaryOp::Neg, Value::Int2(v)) => v
            .checked_neg()
            .map(Value::Int2)
            .ok_or_else(|| out_of_range(PgType::Int2)),
        (UnaryOp::Neg, Value::Float4(v)) => Ok(Value::Float4(-v)),
        (UnaryOp::Neg, Value::Float8(v)) => Ok(Value::Float8(-v)),
        (UnaryOp::Neg, Value::Numeric(v)) => Ok(Value::Numeric(v.neg())),
        (UnaryOp::Abs, Value::Int2(v)) => v
            .checked_abs()
            .map(Value::Int2)
            .ok_or_else(|| out_of_range(PgType::Int2)),
        (UnaryOp::Abs, Value::Int4(v)) => v
            .checked_abs()
            .map(Value::Int4)
            .ok_or_else(|| out_of_range(PgType::Int4)),
        (UnaryOp::Abs, Value::Int8(v)) => v
            .checked_abs()
            .map(Value::Int8)
            .ok_or_else(|| out_of_range(PgType::Int8)),
        (UnaryOp::Abs, Value::Float4(v)) => Ok(Value::Float4(v.abs())),
        (UnaryOp::Abs, Value::Float8(v)) => Ok(Value::Float8(v.abs())),
        (UnaryOp::Abs, Value::Numeric(v)) => Ok(Value::Numeric(v.abs())),
        (UnaryOp::Sqrt, Value::Float8(v)) => {
            float::f8_sqrt(v).map(Value::Float8).map_err(float_error)
        }
        (UnaryOp::Cbrt, Value::Float8(v)) => Ok(Value::Float8(float::f8_cbrt(v))),
        (op, operand) => unreachable!("binder let through {op:?} on {operand:?}"),
    }
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
    let l = eval(left, row, ctx)?;
    let r = eval(right, row, ctx)?;
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    if op.is_arithmetic() {
        return match arg_ty {
            PgType::Int2 => eval_arith_int2(op, int2(&l), int2(&r)),
            PgType::Int4 => eval_arith_int4(op, int4(&l), int4(&r)),
            PgType::Int8 => eval_arith_int8(op, int8(&l), int8(&r)),
            PgType::Float4 => eval_arith_f4(op, float4(&l), float4(&r)),
            PgType::Float8 => eval_arith_f8(op, float8(&l), float8(&r)),
            PgType::Numeric => eval_arith_numeric(op, numeric(&l), numeric(&r)),
            other => unreachable!("binder let arithmetic through on {other:?}"),
        };
    }
    Ok(Value::Bool(apply_comparison(op, arg_ty, collation, &l, &r)))
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

/// Total-order comparison of two non-null values of type `ty` under the
/// database's default collation. Floats use PG's total order (NaN sorts
/// greatest, `NaN = NaN`), so this also drives ORDER BY.
///
/// String comparison here is byte order. Use [`compare_values_collated`] where a
/// collation has been derived — comparison operators and ORDER BY — and this one
/// where the collation is provably irrelevant: equality and hashing (every
/// supported collation is deterministic, so equal bytes and equal values
/// coincide), and ordering of non-string types.
pub fn compare_values(ty: PgType, l: &Value, r: &Value) -> Ordering {
    compare_values_collated(ty, l, r, DEFAULT_COLLATION_OID)
}

/// Total-order comparison of two non-null values of type `ty`, ordering strings
/// under `collation`. Identical to [`compare_values`] for every other type.
pub fn compare_values_collated(ty: PgType, l: &Value, r: &Value, collation: u32) -> Ordering {
    match ty {
        PgType::Int2 => int2(l).cmp(&int2(r)),
        PgType::Int4 => int4(l).cmp(&int4(r)),
        PgType::Int8 => int8(l).cmp(&int8(r)),
        PgType::Float4 => float::f4_cmp(float4(l), float4(r)),
        PgType::Float8 => float::f8_cmp(float8(l), float8(r)),
        // Collation-driven comparison — byte order for `C`/`POSIX`/the database
        // default, the locale's order for an ICU collation. varchar and name
        // compare like text; bpchar ignores trailing blanks.
        PgType::Text | PgType::Varchar | PgType::Name => {
            collation::compare_str(collation, text(l), text(r))
        }
        PgType::Bpchar => collation::compare_str(
            collation,
            text(l).trim_end_matches(' '),
            text(r).trim_end_matches(' '),
        ),
        // `"char"` is a byte, not a string: no collation, and deliberately
        // **unsigned**, so `'\377' > 'a'`. PG's `btcharcmp` casts to `uint8` for
        // exactly this reason. Note the asymmetry with the `int4` conversion,
        // which reads the same byte as signed — see `crabgresql_types::char`.
        PgType::Char => char_of(l).cmp(&char_of(r)),
        PgType::Bytea => bytea(l).cmp(bytea(r)),
        // false < true, as in PG.
        PgType::Bool => bool_of(l).cmp(&bool_of(r)),
        // Microsecond order; the ±infinity sentinels sort naturally.
        PgType::Timestamp => timestamp_of(l).cmp(&timestamp_of(r)),
        PgType::TimestampTz => timestamptz_of(l).cmp(&timestamptz_of(r)),
        // Canonical-span order (30-day months, 24-hour days), infinities first/last.
        PgType::Interval => interval::cmp(interval_of(l), interval_of(r)),
        // Arbitrary-precision total order; NaN sorts greatest (== itself).
        PgType::Numeric => numeric(l).cmp(numeric(r)),
        // Day order (the ±infinity sentinels sort naturally); microsecond order;
        // UTC-instant-then-zone order.
        PgType::Date => date::cmp(date_of(l), date_of(r)),
        PgType::Time => time::cmp(time_of(l), time_of(r)),
        PgType::TimeTz => timetz::cmp(timetz_of(l), timetz_of(r)),
        // uuid: raw byte order (PG's `uuid_cmp`).
        PgType::Uuid => uuid_of(l).cmp(uuid_of(r)),
        // inet/cidr: family, common-prefix bits, masklen, address (`network_cmp`).
        PgType::Inet | PgType::Cidr => net::network_cmp(inet_of(l), inet_of(r)),
        // money: the natural i64 (cents) order.
        PgType::Money => money::cmp(money_of(l), money_of(r)),
        // oid: unsigned 32-bit order (PG's `oidcmp`).
        PgType::Oid => oid_of(l).cmp(&oid_of(r)),
        // tid: block first, then offset — PG's `tidcmp`, and the order the
        // heap itself lays rows out in.
        PgType::Tid => tid_of(l).cmp(&tid_of(r)),
        // Both transaction id types order as plain unsigned integers. `xid` is
        // reachable here only through equality and hashing — `is_orderable`
        // above keeps it out of every sort — but the arm must exist, because
        // `keys_equal` routes grouping equality through `compare_values`.
        PgType::Xid => xid_of(l).cmp(&xid_of(r)),
        PgType::Xid8 => xid8_of(l).cmp(&xid8_of(r)),
        // pg_lsn: the natural unsigned order of the 64-bit counter.
        PgType::PgLsn => lsn_of(l).cmp(&lsn_of(r)),
        // A reg* value orders by OID, never by the name it renders as — the
        // same rule its `PartialEq` and `hash_key` use.
        PgType::Reg(_) => reg_oid(l).cmp(&reg_oid(r)),
        // bit/varbit: common-prefix bit order, then shorter first (`bit_cmp`).
        PgType::Bit | PgType::Varbit => {
            let (la, da) = bit_of(l);
            let (lb, db) = bit_of(r);
            bit::cmp(la, da, lb, db)
        }
        // macaddr/macaddr8: raw byte order (PG's `macaddr_cmp`).
        PgType::Macaddr | PgType::Macaddr8 => macaddr_bytes(l).cmp(macaddr_bytes(r)),
        // jsonb: PG's `compareJsonbContainers` total order. (`json` has no
        // default ordering and never reaches here.)
        PgType::Jsonb => json::cmp(jsonb_of(l), jsonb_of(r)),
        // The text-search types carry their own total orders.
        PgType::Tsvector => tsvector::cmp(tsvector_of(l), tsvector_of(r)),
        PgType::Tsquery => tsquery::cmp(tsquery_of(l), tsquery_of(r)),
        // Arrays: element-wise comparison, then the shorter array is less on a
        // common prefix (PG's `array_cmp`). A NULL element sorts after any
        // non-NULL (NULLS-LAST), matching the default btree order.
        PgType::Array(elem_oid) => {
            let elem = PgType::from_oid(elem_oid).expect("orderable array element type resolves");
            compare_elementwise(elem, array_elems(l), array_elems(r))
        }
        // `oidvector` is the one type whose *sort* order is not its element-wise
        // order: PG gives it its own operator class (`btoidvectorcmp`), which
        // compares the element **count** first, so `'2' < '1 1'` is true.
        // `int2vector` has no opclass of its own and falls back to the
        // polymorphic array ordering, so for it `'2' > '1 1'`.
        //
        // This is the *btree* order — what ORDER BY, `<` and indexes use.
        // `min`/`max` deliberately do NOT use it; see
        // [`compare_values_for_aggregate`].
        PgType::Vector(kind) => {
            let (la, lb) = (vector_elems(l), vector_elems(r));
            if matches!(kind, VectorKind::Oid) && la.len() != lb.len() {
                return la.len().cmp(&lb.len());
            }
            compare_elementwise(kind.element(), la, lb)
        }
        // Query-time user-type ordering is currently defined only for enums.
        // Keep this total for defensive callers: malformed/mixed values use
        // their actual non-user representation or type OID, never an unchecked
        // NULL unwrap or recursive redispatch through `PgType::User`.
        PgType::User(_) => match (l, r) {
            (
                Value::Enum {
                    type_oid: a_ty,
                    ordinal: a,
                    ..
                },
                Value::Enum {
                    type_oid: b_ty,
                    ordinal: b,
                    ..
                },
            ) => a_ty.cmp(b_ty).then_with(|| a.cmp(b)),
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Less,
            (_, Value::Null) => Ordering::Greater,
            _ => match (l.pg_type(), r.pg_type()) {
                (Some(a), Some(b)) if a == b && !matches!(a, PgType::User(_)) => {
                    compare_values(a, l, r)
                }
                (Some(a), Some(b)) => a.oid().cmp(&b.oid()),
                _ => Ordering::Equal,
            },
        },
        other => unreachable!("comparison not supported for {other:?}"),
    }
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

fn int2(v: &Value) -> i16 {
    match v {
        Value::Int2(v) => *v,
        other => unreachable!("expected int2, got {other:?}"),
    }
}

pub(crate) fn array_elems(v: &Value) -> &[Value] {
    match v {
        Value::Array { elems, .. } => elems,
        other => unreachable!("expected array, got {other:?}"),
    }
}

/// Compare two element sequences the way PG's `array_cmp` does: element-wise,
/// then the shorter one first on a common prefix. A NULL element sorts after
/// any non-NULL (NULLS-LAST), matching the default btree order; vectors never
/// contain NULLs, so that arm is only reachable from arrays.
fn compare_elementwise(elem: PgType, la: &[Value], lb: &[Value]) -> Ordering {
    for (x, y) in la.iter().zip(lb.iter()) {
        let ord = match (x, y) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Greater,
            (_, Value::Null) => Ordering::Less,
            _ => compare_values(elem, x, y),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    la.len().cmp(&lb.len())
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

pub(crate) fn vector_elems(v: &Value) -> &[Value] {
    match v {
        Value::Vector { elems, .. } => elems,
        other => unreachable!("expected vector, got {other:?}"),
    }
}

fn int4(v: &Value) -> i32 {
    match v {
        Value::Int4(v) => *v,
        other => unreachable!("expected int4, got {other:?}"),
    }
}

fn oid_of(v: &Value) -> u32 {
    match v {
        Value::Oid(v) => *v,
        other => unreachable!("expected oid, got {other:?}"),
    }
}

fn tid_of(v: &Value) -> (u32, u16) {
    match v {
        Value::Tid { block, offset } => (*block, *offset),
        other => unreachable!("expected tid, got {other:?}"),
    }
}

fn xid_of(v: &Value) -> u32 {
    match v {
        Value::Xid(x) => *x,
        other => unreachable!("expected xid, got {other:?}"),
    }
}

fn xid8_of(v: &Value) -> u64 {
    match v {
        Value::Xid8(x) => *x,
        other => unreachable!("expected xid8, got {other:?}"),
    }
}

fn lsn_of(v: &Value) -> u64 {
    match v {
        Value::PgLsn(x) => *x,
        other => unreachable!("expected pg_lsn, got {other:?}"),
    }
}

fn reg_oid(v: &Value) -> u32 {
    match v {
        Value::Reg(r) => r.oid,
        other => unreachable!("expected a reg* value, got {other:?}"),
    }
}

fn int8(v: &Value) -> i64 {
    match v {
        Value::Int8(v) => *v,
        other => unreachable!("expected int8, got {other:?}"),
    }
}

fn float4(v: &Value) -> f32 {
    match v {
        Value::Float4(v) => *v,
        other => unreachable!("expected float4, got {other:?}"),
    }
}

fn float8(v: &Value) -> f64 {
    match v {
        Value::Float8(v) => *v,
        other => unreachable!("expected float8, got {other:?}"),
    }
}

fn numeric(v: &Value) -> &Numeric {
    match v {
        Value::Numeric(n) => n,
        other => unreachable!("expected numeric, got {other:?}"),
    }
}

fn money_of(v: &Value) -> i64 {
    match v {
        Value::Money(c) => *c,
        other => unreachable!("expected money, got {other:?}"),
    }
}

fn text(v: &Value) -> &str {
    match v {
        Value::Text(s) => s,
        other => unreachable!("expected text, got {other:?}"),
    }
}

fn bytea(v: &Value) -> &[u8] {
    match v {
        Value::Bytea(b) => b,
        other => unreachable!("expected bytea, got {other:?}"),
    }
}

fn bool_of(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        other => unreachable!("expected bool, got {other:?}"),
    }
}

fn char_of(v: &Value) -> u8 {
    match v {
        Value::Char(c) => *c,
        other => unreachable!("expected \"char\", got {other:?}"),
    }
}

fn uuid_of(v: &Value) -> &[u8; 16] {
    match v {
        Value::Uuid(b) => b,
        other => unreachable!("expected uuid, got {other:?}"),
    }
}

fn inet_of(v: &Value) -> &Inet {
    match v {
        Value::Inet(i) | Value::Cidr(i) => i,
        other => unreachable!("expected inet/cidr, got {other:?}"),
    }
}

fn bit_of(v: &Value) -> (u32, &[u8]) {
    match v {
        Value::Bit { len, data } => (*len, data),
        other => unreachable!("expected bit, got {other:?}"),
    }
}

fn macaddr_bytes(v: &Value) -> &[u8] {
    match v {
        Value::Macaddr(b) => b,
        Value::Macaddr8(b) => b,
        other => unreachable!("expected macaddr/macaddr8, got {other:?}"),
    }
}

fn jsonb_of(v: &Value) -> &json::Jsonb {
    match v {
        Value::Jsonb(j) => j,
        other => unreachable!("expected jsonb, got {other:?}"),
    }
}

fn tsvector_of(v: &Value) -> &tsvector::TsVector {
    match v {
        Value::Tsvector(t) => t,
        other => unreachable!("expected tsvector, got {other:?}"),
    }
}

fn tsquery_of(v: &Value) -> &tsquery::TsQuery {
    match v {
        Value::Tsquery(q) => q,
        other => unreachable!("expected tsquery, got {other:?}"),
    }
}

fn timestamp_of(v: &Value) -> i64 {
    match v {
        Value::Timestamp(t) => *t,
        other => unreachable!("expected timestamp, got {other:?}"),
    }
}

fn interval_of(v: &Value) -> Interval {
    match v {
        Value::Interval(iv) => *iv,
        other => unreachable!("expected interval, got {other:?}"),
    }
}

fn timestamptz_of(v: &Value) -> i64 {
    match v {
        Value::TimestampTz(t) => *t,
        other => unreachable!("expected timestamptz, got {other:?}"),
    }
}

fn date_of(v: &Value) -> i32 {
    match v {
        Value::Date(d) => *d,
        other => unreachable!("expected date, got {other:?}"),
    }
}

fn time_of(v: &Value) -> i64 {
    match v {
        Value::Time(t) => *t,
        other => unreachable!("expected time, got {other:?}"),
    }
}

fn timetz_of(v: &Value) -> TimeTz {
    match v {
        Value::TimeTz(t) => *t,
        other => unreachable!("expected timetz, got {other:?}"),
    }
}

fn out_of_range(ty: PgType) -> ExecError {
    let message = match ty {
        PgType::Int2 => "smallint out of range",
        PgType::Int4 => "integer out of range",
        PgType::Int8 => "bigint out of range",
        _ => unreachable!(),
    };
    ExecError::new(sqlstate::NUMERIC_VALUE_OUT_OF_RANGE, message)
}

fn float_error(e: float::FloatError) -> ExecError {
    ExecError::new(e.sqlstate, e.message)
}

fn division_by_zero() -> ExecError {
    ExecError::new(sqlstate::DIVISION_BY_ZERO, "division by zero")
}

fn eval_arith_int2(op: BinOp, a: i16, b: i16) -> Result<Value, ExecError> {
    let result = match op {
        BinOp::Add => a.checked_add(b),
        BinOp::Sub => a.checked_sub(b),
        BinOp::Mul => a.checked_mul(b),
        BinOp::Div => {
            if b == 0 {
                return Err(division_by_zero());
            }
            a.checked_div(b)
        }
        BinOp::Mod => {
            if b == 0 {
                return Err(division_by_zero());
            }
            return Ok(Value::Int2(a.checked_rem(b).unwrap_or(0)));
        }
        _ => unreachable!(),
    };
    result
        .map(Value::Int2)
        .ok_or_else(|| out_of_range(PgType::Int2))
}

fn eval_arith_int4(op: BinOp, a: i32, b: i32) -> Result<Value, ExecError> {
    let result = match op {
        BinOp::Add => a.checked_add(b),
        BinOp::Sub => a.checked_sub(b),
        BinOp::Mul => a.checked_mul(b),
        BinOp::Div => {
            if b == 0 {
                return Err(division_by_zero());
            }
            // MIN / -1 overflows; MIN % -1 is 0 in PG, but checked_rem
            // refuses it, so special-case below.
            a.checked_div(b)
        }
        BinOp::Mod => {
            if b == 0 {
                return Err(division_by_zero());
            }
            return Ok(Value::Int4(a.checked_rem(b).unwrap_or(0)));
        }
        _ => unreachable!(),
    };
    result
        .map(Value::Int4)
        .ok_or_else(|| out_of_range(PgType::Int4))
}

fn eval_arith_int8(op: BinOp, a: i64, b: i64) -> Result<Value, ExecError> {
    let result = match op {
        BinOp::Add => a.checked_add(b),
        BinOp::Sub => a.checked_sub(b),
        BinOp::Mul => a.checked_mul(b),
        BinOp::Div => {
            if b == 0 {
                return Err(division_by_zero());
            }
            a.checked_div(b)
        }
        BinOp::Mod => {
            if b == 0 {
                return Err(division_by_zero());
            }
            return Ok(Value::Int8(a.checked_rem(b).unwrap_or(0)));
        }
        _ => unreachable!(),
    };
    result
        .map(Value::Int8)
        .ok_or_else(|| out_of_range(PgType::Int8))
}

fn eval_arith_f4(op: BinOp, a: f32, b: f32) -> Result<Value, ExecError> {
    let r = match op {
        BinOp::Add => float::f4_add(a, b),
        BinOp::Sub => float::f4_sub(a, b),
        BinOp::Mul => float::f4_mul(a, b),
        BinOp::Div => float::f4_div(a, b),
        other => unreachable!("float4 arithmetic {other:?}"),
    };
    r.map(Value::Float4).map_err(float_error)
}

fn eval_arith_f8(op: BinOp, a: f64, b: f64) -> Result<Value, ExecError> {
    let r = match op {
        BinOp::Add => float::f8_add(a, b),
        BinOp::Sub => float::f8_sub(a, b),
        BinOp::Mul => float::f8_mul(a, b),
        BinOp::Div => float::f8_div(a, b),
        BinOp::Pow => float::f8_pow(a, b),
        other => unreachable!("float8 arithmetic {other:?}"),
    };
    r.map(Value::Float8).map_err(float_error)
}

fn eval_arith_numeric(op: BinOp, a: &Numeric, b: &Numeric) -> Result<Value, ExecError> {
    let r = match op {
        BinOp::Add => a.add(b),
        BinOp::Sub => a.sub(b),
        BinOp::Mul => a.mul(b),
        BinOp::Div => a.div(b).map_err(numeric_error)?,
        BinOp::Mod => a.modulo(b).map_err(numeric_error)?,
        other => unreachable!("numeric arithmetic {other:?}"),
    };
    Ok(Value::Numeric(r))
}

fn numeric_error(e: crabgresql_types::numeric::NumErr) -> ExecError {
    ExecError::new(e.sqlstate, e.message).with_detail(e.detail)
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
            fn rel_name(&self, _oid: u32) -> Option<(String, String)> {
                None
            }
            fn rel_oid(&self, _namespace: Option<&str>, _name: &str) -> Option<u32> {
                None
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
mod vector_cmp_tests {
    use super::compare_values;
    use crabgresql_types::{PgType, Value, VectorKind};
    use std::cmp::Ordering;

    fn v(kind: VectorKind, elems: &[i64]) -> Value {
        Value::Vector {
            kind,
            elems: elems
                .iter()
                .map(|n| match kind {
                    VectorKind::Oid => Value::Oid(*n as u32),
                    VectorKind::Int2 => Value::Int2(*n as i16),
                })
                .collect(),
        }
    }

    /// `oidvector` has its own operator class and compares the element count
    /// before any element; `int2vector` has none and compares element-wise.
    /// Both probed against PostgreSQL 18.4 — `'2' < '1 1'` is true for
    /// `oidvector` and false for `int2vector`.
    #[test]
    fn the_two_kinds_order_differently_on_unequal_lengths() {
        let (oid, int2) = (VectorKind::Oid, VectorKind::Int2);
        let ov = PgType::Vector(oid);
        let iv = PgType::Vector(int2);

        assert_eq!(
            compare_values(ov, &v(oid, &[2]), &v(oid, &[1, 1])),
            Ordering::Less
        );
        assert_eq!(
            compare_values(iv, &v(int2, &[2]), &v(int2, &[1, 1])),
            Ordering::Greater
        );
        assert_eq!(
            compare_values(ov, &v(oid, &[1, 5]), &v(oid, &[1, 1, 1])),
            Ordering::Less
        );
        assert_eq!(
            compare_values(iv, &v(int2, &[9, 8]), &v(int2, &[1, 1, 1])),
            Ordering::Greater
        );
    }

    /// At equal length both kinds compare element-wise, and a shorter prefix
    /// still sorts first — `'1' < '1 2'` either way.
    #[test]
    fn equal_lengths_and_common_prefixes_agree() {
        for kind in [VectorKind::Oid, VectorKind::Int2] {
            let ty = PgType::Vector(kind);
            assert_eq!(
                compare_values(ty, &v(kind, &[2, 0]), &v(kind, &[1, 9])),
                Ordering::Greater,
                "{kind:?}"
            );
            assert_eq!(
                compare_values(ty, &v(kind, &[1]), &v(kind, &[1, 2])),
                Ordering::Less,
                "{kind:?}"
            );
            assert_eq!(
                compare_values(ty, &v(kind, &[1, 2]), &v(kind, &[1, 2])),
                Ordering::Equal,
                "{kind:?}"
            );
        }
    }
}

#[cfg(test)]
mod enum_cmp_tests {
    use super::compare_values;
    use crabgresql_types::{PgType, Value};
    use std::cmp::Ordering;

    fn e(ordinal: u32, label: &str) -> Value {
        Value::Enum {
            type_oid: 16384,
            ordinal,
            label: label.into(),
        }
    }

    #[test]
    fn enum_orders_by_definition_ordinal_not_label() {
        let ty = PgType::User(16384);
        // 'red'(0) < 'green'(3), even though "green" < "red" alphabetically.
        assert_eq!(
            compare_values(ty, &e(0, "red"), &e(3, "green")),
            Ordering::Less
        );
        assert_eq!(
            compare_values(ty, &e(3, "green"), &e(0, "red")),
            Ordering::Greater
        );
        assert_eq!(
            compare_values(ty, &e(2, "yellow"), &e(2, "yellow")),
            Ordering::Equal
        );
    }

    #[test]
    fn malformed_user_comparisons_are_total() {
        let ty = PgType::User(16384);
        assert_eq!(
            compare_values(ty, &Value::Null, &e(0, "red")),
            Ordering::Less
        );
        assert_eq!(
            compare_values(ty, &e(0, "red"), &Value::Int4(1)),
            Ordering::Greater
        );
        assert_eq!(
            compare_values(
                ty,
                &e(0, "red"),
                &Value::Enum {
                    type_oid: 16385,
                    ordinal: 0,
                    label: "other".into(),
                },
            ),
            Ordering::Less
        );
    }
}
