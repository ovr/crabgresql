use crabgresql_binder::BoundExpr;
use crabgresql_storage_api::Tuple;
use crabgresql_types::Value;

use super::series::build_series;
use crate::generate_series::Series;
use crate::{ExecContext, ExecError, ExecNode, eval};

/// Projection with one or more set-returning functions in the target list. Each
/// input row expands to as many output rows as the longest SRF produces; shorter
/// SRFs are NULL-padded once exhausted (PG's `ROWS FROM` semantics since PG 10)
/// and scalar columns repeat. An input row whose SRFs are all empty yields no
/// output rows.
pub struct ProjectSet {
    child: Box<dyn ExecNode>,
    exprs: Vec<BoundExpr>,
    ctx: ExecContext,
    /// Expansion state for the current input row; `None` before the first pull
    /// and between fully-expanded input rows.
    current: Option<RowExpansion>,
}

/// The per-Srf iterators for one input row, parallel to `exprs` (scalar slots
/// are `None`), plus the input row scalar projections evaluate against.
struct RowExpansion {
    input: Tuple,
    series: Vec<Option<Series>>,
}

impl ProjectSet {
    pub fn new(child: Box<dyn ExecNode>, exprs: Vec<BoundExpr>, ctx: ExecContext) -> Self {
        Self {
            child,
            exprs,
            ctx,
            current: None,
        }
    }

    /// Build the per-Srf series for a fresh input row.
    fn expand(&self, input: Tuple) -> Result<RowExpansion, ExecError> {
        let mut series = Vec::with_capacity(self.exprs.len());
        for expr in &self.exprs {
            match expr {
                BoundExpr::Srf { func, args, .. } => {
                    let values = args
                        .iter()
                        .map(|a| eval(a, &input, &self.ctx))
                        .collect::<Result<Vec<_>, _>>()?;
                    series.push(Some(build_series(*func, &values, &self.ctx)?));
                }
                _ => series.push(None),
            }
        }
        Ok(RowExpansion { input, series })
    }
}

impl ExecNode for ProjectSet {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        loop {
            if self.current.is_none() {
                let Some(input) = self.child.next()? else {
                    return Ok(None);
                };
                self.current = Some(self.expand(input)?);
            }
            let Some(exp) = self.current.as_mut() else {
                continue;
            };

            // Advance every SRF once; the input row is exhausted when they all are.
            let mut srf_vals: Vec<Option<Value>> = Vec::with_capacity(exp.series.len());
            let mut any = false;
            for slot in exp.series.iter_mut() {
                let value = match slot {
                    Some(series) => series.next_value()?,
                    None => None,
                };
                any |= value.is_some();
                srf_vals.push(value);
            }
            if !any {
                self.current = None;
                continue;
            }

            let input = exp.input.clone();
            let mut out = Vec::with_capacity(self.exprs.len());
            for (expr, srf_val) in self.exprs.iter().zip(srf_vals) {
                match expr {
                    // Exhausted SRFs pad with NULL to match the longest.
                    BoundExpr::Srf { .. } => out.push(srf_val.unwrap_or(Value::Null)),
                    _ => out.push(eval(expr, &input, &self.ctx)?),
                }
            }
            return Ok(Some(out));
        }
    }
}
