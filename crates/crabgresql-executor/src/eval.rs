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
    Inet, Interval, Numeric, PgType, TimeTz, Value, bit, cast, collation, date, float, interval,
    json, money, net, time, timetz, tsquery, tsvector,
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
            // The catalog functions read the session's pg_catalog snapshot, which
            // the pure `eval_scalar` has no handle to.
            match eval_catalog_fn(*func, &arg_values, ctx) {
                Some(result) => result,
                None => crate::scalar_fns::eval_scalar(*func, &arg_values),
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
    ) {
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
        // `pg_get_expr` echoes the SQL text crabgresql stores in place of a
        // `pg_node_tree`: a partition's `relpartbound` is deparsed when the row
        // is built (see `crabgresql_catalog`'s `deparse_partbound`). A column
        // default's `adbin` is the statement's own expression text rather than a
        // canonical deparse, so it prints as written — `nextval('s')` where
        // PostgreSQL prints `nextval('s'::regclass)`, and `'x'` where PostgreSQL
        // prints `'x'::text`. A NULL node yields NULL, as in PG.
        ScalarFn::PgGetExpr => Some(Ok(args[0].clone())),
        _ => None,
    }
}

/// `format_type(oid, typmod)`. A NULL oid yields NULL; oid `0` is `-`; an oid
/// nothing in the catalog claims is `???`. The modifier is decoded in
/// PostgreSQL's `atttypmod` encoding (see `crabgresql_catalog`'s
/// `atttypmod_of`), so this is the inverse that reproduces PG's `\d` strings.
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
/// `typmod` applied. `typmod` is `None` when no modifier was given at all and
/// `Some(m)` for one that was — including `Some(-1)`, the "no modifier" value
/// `pg_attribute` stores, which PostgreSQL still distinguishes from `None`.
///
/// Each type prints its modifier only above its own threshold, matching the
/// `typmodout` functions (probed against PostgreSQL 18.4): the character types
/// need more than the four-byte varlena header they reserve, `numeric` needs at
/// least that header, and the rest need only a non-negative value. Below the
/// threshold PostgreSQL prints the bare type name rather than a nonsensical
/// `character varying(-2)`.
///
/// Two deliberate gaps, neither reachable from a crabgresql catalog row (both
/// need a modifier this build never stores): `interval`'s modifier packs range
/// bits and is printed bare rather than decoded, and PostgreSQL's generic
/// fallback for a type with no `typmodout` (`format_type(25, 5)` → `text(5)`)
/// is not reproduced.
fn format_type_text(oid: u32, typmod: Option<i32>, catalog: Option<&dyn CatalogOps>) -> String {
    // VARHDRSZ: character types encode `length + 4` (see `atttypmod_of`).
    const VARHDRSZ: i32 = 4;
    if oid == 0 {
        return "-".to_string();
    }
    let Some(ty) = PgType::from_oid(oid) else {
        // Not a built-in: a `CREATE TYPE` type resolves through the catalog, the
        // same lookup `regtype` renders through, so the two agree on a name.
        // Anything else is `???`, as in PG.
        return catalog
            .and_then(|ops| ops.user_type_name(oid))
            .map_or_else(|| "???".to_string(), |(_, name)| quote_ident(&name));
    };
    // An array formats its element type (carrying the modifier) with `[]`.
    if let PgType::Array(elem) = ty {
        return format!("{}[]", format_type_text(elem, typmod, catalog));
    }
    let name = ty.name();
    let Some(m) = typmod else {
        return name.to_string();
    };
    match ty {
        PgType::Numeric if m >= VARHDRSZ => {
            let m = m - VARHDRSZ;
            // The scale is an 11-bit *signed* field, so `numeric(4,-2)` round
            // trips; the precision is masked to the 16 bits above it.
            let precision = (m >> 16) & 0xffff;
            let scale = (((m & 0x7ff) ^ 1024) - 1024) as i16;
            format!("numeric({precision},{scale})")
        }
        PgType::Varchar if m > VARHDRSZ => format!("character varying({})", m - VARHDRSZ),
        PgType::Bpchar if m > VARHDRSZ => format!("character({})", m - VARHDRSZ),
        // `bpchar` is the one type that reports which spelling it was asked
        // about: given a modifier it cannot print, it is `bpchar`; given none at
        // all it is `character`. An unmodified `bpchar` column stores -1, so
        // this is the arm `\d` takes for one.
        PgType::Bpchar => "bpchar".to_string(),
        PgType::Bit if m >= 0 => format!("bit({m})"),
        PgType::Varbit if m >= 0 => format!("bit varying({m})"),
        // The precision goes *before* the "with[out] time zone" suffix.
        PgType::Time if m >= 0 => format!("time({m}) without time zone"),
        PgType::TimeTz if m >= 0 => format!("time({m}) with time zone"),
        PgType::Timestamp if m >= 0 => format!("timestamp({m}) without time zone"),
        PgType::TimestampTz if m >= 0 => format!("timestamp({m}) with time zone"),
        // Below its type's threshold a modifier prints nothing at all.
        _ => name.to_string(),
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
