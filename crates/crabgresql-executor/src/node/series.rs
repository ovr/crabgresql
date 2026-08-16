//! The row sources a set-returning function yields, shared by the two nodes
//! that can host one: [`TableFunctionSource`](super::TableFunctionSource) in
//! FROM position and [`ProjectSet`](super::ProjectSet) in a target list.

use crabgresql_binder::TableFn;
use crabgresql_storage_api::Tuple;
use crabgresql_types::{PgType, RegKind, Value};

use crate::generate_series::Series;
use crate::{ExecContext, ExecError, eval};

/// One row of `pg_input_error_info(value, type_name)`:
/// `(message, detail, hint, sql_error_code)`. A valid input (or a NULL
/// argument) yields all-NULL; an invalid one reports the input function's
/// message, DETAIL, HINT and SQLSTATE. An unusable *type name* is not a row —
/// it raises, as it does in PostgreSQL.
pub(crate) fn pg_input_error_info_row(
    args: &[Value],
    ctx: &ExecContext,
) -> Result<Tuple, ExecError> {
    let all_null = || vec![Value::Null, Value::Null, Value::Null, Value::Null];
    let (Value::Text(value), Value::Text(type_name)) = (&args[0], &args[1]) else {
        return Ok(all_null());
    };
    Ok(match eval::soft_input_in_ctx(type_name, value, ctx)? {
        None => all_null(),
        Some(e) => vec![
            Value::Text(e.message),
            e.detail.map_or(Value::Null, Value::Text),
            e.hint.map_or(Value::Null, Value::Text),
            Value::Text(e.code.into_owned()),
        ],
    })
}

/// Build the [`Series`] a target-list SRF (`generate_series`, `unnest` or
/// `jsonb_path_query`) yields.
pub(crate) fn build_series(
    func: TableFn,
    values: &[Value],
    ctx: &ExecContext,
) -> Result<Series, ExecError> {
    match func {
        TableFn::GenerateSeries(elem) => Series::from_args(elem, values),
        TableFn::JsonbPathQuery => jsonb_path_query_series(values),
        TableFn::Unnest(_) => Ok(unnest_series(values)),
        TableFn::GenerateSubscripts => Ok(generate_subscripts_series(values)),
        TableFn::PgPartitionAncestors => Ok(pg_partition_ancestors_series(values, ctx)),
        // Both return a record, which a target list cannot expand into.
        TableFn::PgInputErrorInfo | TableFn::PgAvailableExtensions => Err(ExecError::new(
            crabgresql_pg_wire::sqlstate::FEATURE_NOT_SUPPORTED,
            "set-returning function is not supported in this context",
        )),
    }
}

/// The rows of `pg_partition_ancestors(regclass)`: the relation itself, then
/// each partitioned parent up to the root, as `regclass` values.
///
/// A relation that is neither a partition nor partitioned yields no rows, and so
/// does a NULL argument or an OID nothing answers to — PostgreSQL returns the
/// empty set in every one of those cases rather than raising.
///
/// The argument arrives as either representation — the binder accepts `regclass`
/// and `oid` both, for the reason `resolve_partition_ancestors` gives.
pub(crate) fn pg_partition_ancestors_series(args: &[Value], ctx: &ExecContext) -> Series {
    let oid = match args.first() {
        Some(Value::Reg(reg)) => reg.oid,
        Some(Value::Oid(oid)) => *oid,
        _ => return Series::Empty,
    };
    let Some(catalog) = ctx.catalog.as_deref() else {
        return Series::Empty;
    };
    let rows: Vec<_> = catalog
        .partition_ancestors(oid)
        .into_iter()
        .map(|oid| Value::Reg(crate::reg::from_oid(RegKind::Class, oid, catalog)))
        .collect();
    Series::Materialized(rows.into_iter())
}

/// The rows of `pg_available_extensions()`, as `(name, default_version,
/// comment)`. Read through the catalog so the function and the view of the same
/// name publish one list rather than two.
pub(crate) fn pg_available_extensions_rows(ctx: &ExecContext) -> Vec<Tuple> {
    let Some(catalog) = ctx.catalog.as_deref() else {
        return Vec::new();
    };
    catalog
        .available_extensions()
        .into_iter()
        .map(|(name, version, comment)| {
            vec![
                Value::Text(name),
                Value::Text(version),
                Value::Text(comment),
            ]
        })
        .collect()
}

/// Materialize `unnest(array)` into a [`Series`] of its elements (NULL elements
/// included). A NULL array argument yields no rows, as PG's `unnest` does.
pub(crate) fn unnest_series(values: &[Value]) -> Series {
    match values.first() {
        // `unnest` expands a vector into its elements too, so
        // `unnest('11 22 33'::oidvector)` yields three `oid` rows.
        Some(Value::Array { elems, .. } | Value::Vector { elems, .. }) => {
            Series::Materialized(elems.clone().into_iter())
        }
        _ => Series::Empty,
    }
}

/// The rows of `generate_subscripts(array, dim [, reverse])`: the valid
/// subscripts of the array's `dim`th dimension, ascending (or descending when
/// `reverse` is true).
///
/// The function is STRICT, so a NULL argument yields no rows, and so does a
/// dimension the value does not have — including every `dim` other than 1 here,
/// since the engine's arrays are one-dimensional. An array's lower bound is 1;
/// `oidvector`/`int2vector` are stored from 0, the same convention subscripting
/// follows (see `eval`'s `Subscript` arm).
pub(crate) fn generate_subscripts_series(values: &[Value]) -> Series {
    let (len, lower) = match values.first() {
        Some(Value::Array { elems, .. }) => (elems.len(), 1i64),
        Some(Value::Vector { elems, .. }) => (elems.len(), 0i64),
        _ => return Series::Empty,
    };
    // Only `dim = 1` exists for a one-dimensional value; 0, a negative
    // dimension, and anything past the first all yield the empty set.
    if !matches!(values.get(1), Some(Value::Int4(1))) {
        return Series::Empty;
    }
    let reverse = match values.get(2) {
        None | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        // A NULL `reverse` (or anything else) is the strict case.
        _ => return Series::Empty,
    };
    // An empty array has no subscripts at all.
    if len == 0 {
        return Series::Empty;
    }
    let last = lower + len as i64 - 1;
    let (cur, stop, step) = if reverse {
        (last, lower, -1)
    } else {
        (lower, last, 1)
    };
    Series::Int {
        cur,
        stop,
        step,
        forward: !reverse,
        elem: PgType::Int4,
        done: false,
    }
}

/// Evaluate a `jsonb_path_query(target, path [, vars, silent])` call to a
/// materialized [`Series`] of its result items. `jsonb_path_query` is STRICT, so
/// a NULL in any argument yields no rows; `silent` suppresses structural errors
/// (also no rows). A missing-variable error always raises.
pub(crate) fn jsonb_path_query_series(values: &[Value]) -> Result<Series, ExecError> {
    // STRICT: any NULL argument (target, path, vars, or silent) → no rows.
    if values.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Series::Empty);
    }
    let (Value::Jsonb(target), Value::Jsonpath(path)) = (&values[0], &values[1]) else {
        return Ok(Series::Empty);
    };
    let vars = match values.get(2) {
        Some(Value::Jsonb(v)) => Some(v),
        _ => None,
    };
    let silent = matches!(values.get(3), Some(Value::Bool(true)));
    let items = crabgresql_types::jsonpath::query(path, target, vars, silent)
        .map_err(|e| ExecError::new(e.sqlstate, e.message))?;
    let rows: Vec<Value> = items.into_iter().map(Value::Jsonb).collect();
    Ok(Series::Materialized(rows.into_iter()))
}
