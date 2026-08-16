use crabgresql_binder::{BoundExpr, TableFn};
use crabgresql_storage_api::Tuple;
use crabgresql_types::Value;

use super::series::{
    generate_subscripts_series, jsonb_path_query_series, pg_available_extensions_rows,
    pg_input_error_info_row, pg_partition_ancestors_series, unnest_series,
};
use crate::generate_series::Series;
use crate::{ExecContext, ExecError, ExecNode, eval};

/// A set-returning function in FROM position. Evaluates its arguments once (on
/// the first pull) and streams the function's rowset. `pg_input_error_info`
/// yields exactly one row; `generate_series` yields one row per value.
pub struct TableFunctionSource {
    func: TableFn,
    args: Vec<BoundExpr>,
    ctx: ExecContext,
    ordinality: bool,
    /// PG numbers the rows the *function* produced, so this is bumped here and
    /// not anywhere a filter above could skip it. It never restarts because
    /// nothing rescans this node: [`ExecNode`] has no rescan entry point.
    /// TODO: restart the ordinal (and rebuild `state`) when a rescan API lands.
    next_ordinal: i64,
    /// Iteration state, initialized lazily from the evaluated arguments.
    state: Option<TableFnState>,
}

enum TableFnState {
    /// `pg_input_error_info`: a single pending row, then exhausted.
    Single(Option<Tuple>),
    /// `pg_available_extensions`: a materialized list of multi-column rows,
    /// which a [`Series`] (one scalar per row) cannot carry.
    Rows(std::vec::IntoIter<Tuple>),
    /// `generate_series`: a lazy integer range.
    Series(Series),
}

impl TableFunctionSource {
    pub fn new(func: TableFn, args: Vec<BoundExpr>, ordinality: bool, ctx: ExecContext) -> Self {
        Self {
            func,
            args,
            ctx,
            ordinality,
            next_ordinal: 1,
            state: None,
        }
    }

    /// Evaluate the (constant) arguments once and build the iteration state.
    fn init(&mut self) -> Result<&mut TableFnState, ExecError> {
        if self.state.is_none() {
            let values = self
                .args
                .iter()
                .map(|expr| eval(expr, &[], &self.ctx))
                .collect::<Result<Vec<_>, _>>()?;
            self.state = Some(match self.func {
                TableFn::PgInputErrorInfo => {
                    TableFnState::Single(Some(pg_input_error_info_row(&values, &self.ctx)?))
                }
                TableFn::GenerateSeries(elem) => {
                    TableFnState::Series(Series::from_args(elem, &values)?)
                }
                TableFn::JsonbPathQuery => TableFnState::Series(jsonb_path_query_series(&values)?),
                TableFn::Unnest(_) => TableFnState::Series(unnest_series(&values)),
                TableFn::GenerateSubscripts => {
                    TableFnState::Series(generate_subscripts_series(&values))
                }
                TableFn::PgPartitionAncestors => {
                    TableFnState::Series(pg_partition_ancestors_series(&values, &self.ctx))
                }
                TableFn::PgAvailableExtensions => {
                    TableFnState::Rows(pg_available_extensions_rows(&self.ctx).into_iter())
                }
            });
        }
        match self.state.as_mut() {
            Some(state) => Ok(state),
            None => panic!("table-function state was not initialized"),
        }
    }
}

impl ExecNode for TableFunctionSource {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        let row = match self.init()? {
            TableFnState::Single(row) => row.take(),
            TableFnState::Rows(rows) => rows.next(),
            TableFnState::Series(series) => series.next_value()?.map(|v| vec![v]),
        };
        match row {
            Some(mut row) if self.ordinality => {
                row.push(Value::Int8(self.next_ordinal));
                self.next_ordinal += 1;
                Ok(Some(row))
            }
            row => Ok(row),
        }
    }
}
