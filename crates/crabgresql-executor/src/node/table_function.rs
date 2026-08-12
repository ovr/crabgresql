use crabgresql_binder::{BoundExpr, TableFn};
use crabgresql_storage_api::Tuple;

use super::series::{jsonb_path_query_series, pg_input_error_info_row, unnest_series};
use crate::generate_series::Series;
use crate::{ExecContext, ExecError, ExecNode, eval};

/// A set-returning function in FROM position. Evaluates its arguments once (on
/// the first pull) and streams the function's rowset. `pg_input_error_info`
/// yields exactly one row; `generate_series` yields one row per value.
pub struct TableFunctionSource {
    func: TableFn,
    args: Vec<BoundExpr>,
    ctx: ExecContext,
    /// Iteration state, initialized lazily from the evaluated arguments.
    state: Option<TableFnState>,
}

enum TableFnState {
    /// `pg_input_error_info`: a single pending row, then exhausted.
    Single(Option<Tuple>),
    /// `generate_series`: a lazy integer range.
    Series(Series),
}

impl TableFunctionSource {
    pub fn new(func: TableFn, args: Vec<BoundExpr>, ctx: ExecContext) -> Self {
        Self {
            func,
            args,
            ctx,
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
        match self.init()? {
            TableFnState::Single(row) => Ok(row.take()),
            TableFnState::Series(series) => Ok(series.next_value()?.map(|v| vec![v])),
        }
    }
}
