//! The row sources a set-returning function yields, shared by the two nodes
//! that can host one: [`TableFunctionSource`](super::TableFunctionSource) in
//! FROM position and [`ProjectSet`](super::ProjectSet) in a target list.

use crabgresql_binder::TableFn;
use crabgresql_storage_api::Tuple;
use crabgresql_types::Value;

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
pub(crate) fn build_series(func: TableFn, values: &[Value]) -> Result<Series, ExecError> {
    match func {
        TableFn::GenerateSeries(elem) => Series::from_args(elem, values),
        TableFn::JsonbPathQuery => jsonb_path_query_series(values),
        TableFn::Unnest(_) => Ok(unnest_series(values)),
        TableFn::PgInputErrorInfo => Err(ExecError::new(
            crabgresql_pg_wire::sqlstate::FEATURE_NOT_SUPPORTED,
            "set-returning function is not supported in this context",
        )),
    }
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
