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
use crabgresql_types::{
    Inet, Interval, Numeric, PgType, TimeTz, Value, bit, cast, collation, date, float, interval,
    json, money, net, time, timetz,
};

use crate::{ExecContext, ExecError};

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
            // The catalog functions read the session's pg_catalog snapshot, which
            // the pure `eval_scalar` has no handle to.
            match eval_catalog_fn(*func, &arg_values, ctx) {
                Some(result) => result,
                None => crate::scalar_fns::eval_scalar(*func, &arg_values),
            }
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
        // `a[i]`: 1-based element access. A NULL array or NULL/out-of-range
        // subscript yields NULL (PG semantics), never an error.
        BoundExpr::Subscript { base, index, .. } => {
            let base = eval(base, row, ctx)?;
            let idx = eval(index, row, ctx)?;
            let elems = match &base {
                Value::Array { elems, .. } => elems,
                // NULL array → NULL element.
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
            // PG arrays are 1-based; a subscript outside `[1, len]` is NULL.
            if i < 1 || (i as usize) > elems.len() {
                Ok(Value::Null)
            } else {
                Ok(elems[(i - 1) as usize].clone())
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
                Value::Array { elems, .. } => {
                    crate::eval_quantified(cmp, &elems, *all, row, ctx)
                }
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
    cast::cast_value(value, ty, ctx.extra_float_digits)
        .map_err(|e| ExecError::new(e.sqlstate, e.message))
}

/// Dispatch the non-strict array constructor functions (`array_cat`,
/// `array_append`, `array_prepend`), which build a [`Value::Array`] of `ret`'s
/// element type. Returns `None` for any other function so the caller falls
/// through to the pure `eval_scalar`.
fn eval_array_ctor_fn(func: ScalarFn, ret: PgType, args: &[Value]) -> Option<Result<Value, ExecError>> {
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
            name => {
                let (ns, seq) = seq_ref(name);
                ops.nextval(ns, seq).map(Value::Int8)
            }
        },
        ScalarFn::Currval => match &args[0] {
            Value::Null => Ok(Value::Null),
            name => {
                let (ns, seq) = seq_ref(name);
                ops.currval(ns, seq).map(Value::Int8)
            }
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
                let (ns, seq) = seq_ref(&args[0]);
                ops.setval(ns, seq, int8(&args[1]), is_called).map(Value::Int8)
            }
        }
        ScalarFn::Lastval => ops.lastval().map(Value::Int8),
        _ => unreachable!("guarded by the matches! above"),
    };
    Some(result)
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
    if !matches!(func, ScalarFn::PgGetUserById | ScalarFn::PgTableIsVisible) {
        return None;
    }
    if matches!(args[0], Value::Null) {
        return Some(Ok(Value::Null));
    }
    let Some(ops) = ctx.catalog.as_deref() else {
        return Some(Err(ExecError::new(
            sqlstate::INTERNAL_ERROR,
            "catalog function evaluated without a catalog context",
        )));
    };
    let oid = match catalog_oid(&args[0]) {
        Ok(oid) => oid,
        Err(e) => return Some(Err(e)),
    };
    let value = match func {
        // PG never returns NULL here: an unresolvable OID prints a placeholder.
        ScalarFn::PgGetUserById => Value::Text(
            ops.role_name(oid)
                .unwrap_or_else(|| format!("unknown (OID={oid})")),
        ),
        // ... whereas an OID no relation has is NULL, not false.
        ScalarFn::PgTableIsVisible => ops.table_is_visible(oid).map_or(Value::Null, Value::Bool),
        _ => unreachable!("guarded by the matches! above"),
    };
    Some(Ok(value))
}

/// The OID a catalog function's argument denotes. The binder declares these
/// arguments as `oid`, and the integer types coerce to it implicitly — but that
/// coercion reinterprets rather than clamps (PG prints `pg_get_userbyid(-1)` as
/// `unknown (OID=4294967295)`), so the integer cases carry it through the same
/// way rather than assuming the value already arrived as `Value::Oid`.
fn catalog_oid(v: &Value) -> Result<u32, ExecError> {
    match v {
        Value::Oid(n) => Ok(*n),
        Value::Int2(n) => Ok(*n as u32),
        Value::Int4(n) => Ok(*n as u32),
        Value::Int8(n) => Ok(*n as u32),
        other => Err(ExecError::new(
            sqlstate::INTERNAL_ERROR,
            format!("catalog function received a non-oid argument: {other:?}"),
        )),
    }
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

/// Whether [`compare_values`] defines an ordering for `ty` — i.e. the type has a
/// default btree operator class. The non-orderable types are exactly those that
/// fall through to the `unreachable!` arm of [`compare_values`]; keep the two in
/// sync. Callers that would otherwise reach `compare_values` on user input (e.g.
/// a RANGE partition key) must gate on this to avoid a panic.
pub fn is_orderable(ty: PgType) -> bool {
    match ty {
        PgType::Json | PgType::Jsonpath | PgType::Point | PgType::Lseg => false,
        // An array is orderable iff its element type is (element-wise btree
        // comparison). Keep in sync with `PgType::has_default_btree_opclass`.
        PgType::Array(elem_oid) => PgType::from_oid(elem_oid).is_some_and(is_orderable),
        _ => true,
    }
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
        // Arrays: element-wise comparison, then the shorter array is less on a
        // common prefix (PG's `array_cmp`). A NULL element sorts after any
        // non-NULL (NULLS-LAST), matching the default btree order.
        PgType::Array(elem_oid) => {
            let elem = PgType::from_oid(elem_oid).expect("orderable array element type resolves");
            let (la, lb) = (array_elems(l), array_elems(r));
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
        // Query-time user-type ordering is currently defined only for enums.
        // Keep this total for defensive callers: malformed/mixed values use
        // their actual non-user representation or type OID, never an unchecked
        // NULL unwrap or recursive redispatch through `PgType::User`.
        PgType::User(_) => match (l, r) {
            (
                Value::Enum { type_oid: a_ty, ordinal: a, .. },
                Value::Enum { type_oid: b_ty, ordinal: b, .. },
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
        assert_eq!(compare_values(ty, &e(0, "red"), &e(3, "green")), Ordering::Less);
        assert_eq!(compare_values(ty, &e(3, "green"), &e(0, "red")), Ordering::Greater);
        assert_eq!(compare_values(ty, &e(2, "yellow"), &e(2, "yellow")), Ordering::Equal);
    }

    #[test]
    fn malformed_user_comparisons_are_total() {
        let ty = PgType::User(16384);
        assert_eq!(compare_values(ty, &Value::Null, &e(0, "red")), Ordering::Less);
        assert_eq!(compare_values(ty, &e(0, "red"), &Value::Int4(1)), Ordering::Greater);
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
