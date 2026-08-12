use std::cmp::Ordering;
use std::sync::Arc;

use crabgresql_planner::{IndexBoundExpr, IndexProbeSpec};
use crabgresql_storage_api::{
    ColumnProjection, IndexBound, IndexProbe, IndexProbeKey, StorageError, TableAm, Tuple,
};
use crabgresql_txn::TxnContext;
use crabgresql_types::{PgType, Value};

use crate::{ExecContext, ExecError, ExecNode, compare_values, eval};

/// Index scan: probes the engine's physical index for the key. When the engine
/// cannot serve it (a columnar engine, an index whose key type it cannot
/// physically index, a system catalog) it falls back to a full scan and
/// re-checks the key per row, which is what makes that fallback correct. The
/// physical-index path is already exact (the engine returns only rows the key
/// selects), so it needs no re-check. NULL matches no comparison at all.
pub struct IndexScan {
    iter: Box<dyn Iterator<Item = Result<Tuple, StorageError>> + Send>,
}

impl IndexScan {
    pub fn new(
        table: &Arc<dyn TableAm>,
        index_name: &str,
        key: IndexProbeSpec,
        ctx: &ExecContext,
        txn: &TxnContext,
        projection: &ColumnProjection,
    ) -> Result<Self, ExecError> {
        let rows = index_probe_rows(table, index_name, &key, ctx, txn, projection)?;
        Ok(Self {
            iter: Box::new(rows.map(|row| row.map(|(_, tuple)| tuple))),
        })
    }
}

/// The rows an index probe yields, each with its `Tid`.
///
/// Two paths, both producing exactly the rows the key selects, MVCC-visible:
/// the engine's own `index_lookup`, or — when it declines with `None` — a full
/// scan re-checking the key per row. The `Tid` is kept because DML needs it to
/// write the row back; [`IndexScan`] drops it.
pub(crate) fn index_probe_rows(
    table: &Arc<dyn TableAm>,
    index_name: &str,
    key: &IndexProbeSpec,
    ctx: &ExecContext,
    txn: &TxnContext,
    projection: &ColumnProjection,
) -> Result<IndexProbe, ExecError> {
    // The key value expressions are row-constant (the planner guarantees it), so
    // they evaluate once against an empty row.
    let eq: Vec<Value> = key
        .eq
        .iter()
        .map(|(_, expr)| eval(expr, &[], ctx))
        .collect::<Result<_, _>>()?;
    fn end<'a>(
        bound: &'a Option<Box<IndexBoundExpr>>,
        ctx: &ExecContext,
    ) -> Result<Option<(&'a IndexBoundExpr, Value)>, ExecError> {
        match bound {
            None => Ok(None),
            Some(bound) => eval(&bound.value, &[], ctx).map(|value| Some((&**bound, value))),
        }
    }
    let (lower, upper) = (end(&key.lower, ctx)?, end(&key.upper, ctx)?);
    let probe = IndexProbeKey {
        eq: &eq,
        lower: lower.as_ref().map(|(b, value)| IndexBound {
            value,
            inclusive: b.inclusive,
        }),
        upper: upper.as_ref().map(|(b, value)| IndexBound {
            value,
            inclusive: b.inclusive,
        }),
    };
    if let Some(rows) = table.index_lookup(index_name, &probe, txn) {
        // Exact path: the engine already returned only the selected,
        // MVCC-visible rows.
        return Ok(rows);
    }
    // Fallback path: a full scan, so re-check the whole key per row — the bounds
    // as well as the equalities, or the scan would return every row past the
    // probe's start.
    let column_ty = |column: usize| table.schema().columns[column].ty;
    let eq: Vec<(usize, PgType, Value)> = key
        .eq
        .iter()
        .zip(eq)
        .map(|((column, _), want)| (*column, column_ty(*column), want))
        .collect();
    let bound = |end: Option<(&IndexBoundExpr, Value)>, want: Ordering| {
        end.map(|(b, value)| (b.column, column_ty(b.column), value, want, b.inclusive))
    };
    // A lower bound keeps rows *above* its value, an upper bound rows below it.
    let range: Vec<(usize, PgType, Value, Ordering, bool)> = [
        bound(lower, Ordering::Greater),
        bound(upper, Ordering::Less),
    ]
    .into_iter()
    .flatten()
    .collect();
    // The planner folds every key column into `projection` precisely so this
    // re-check can read them.
    Ok(Box::new(table.scan(txn, projection).filter_map(
        move |row| {
            match row {
                Ok((tid, tuple)) => {
                    // NULL satisfies neither an equality nor a bound, on either
                    // side of it.
                    let compare = |column: usize, ty: PgType, want: &Value| {
                        let cell = &tuple[column];
                        (!matches!(cell, Value::Null) && !matches!(want, Value::Null))
                            .then(|| compare_values(ty, cell, want))
                    };
                    let matches = eq.iter().all(|(column, ty, want)| {
                        compare(*column, *ty, want) == Some(Ordering::Equal)
                    }) && range.iter().all(|(column, ty, want, side, inclusive)| {
                        match compare(*column, *ty, want) {
                            Some(Ordering::Equal) => *inclusive,
                            other => other == Some(*side),
                        }
                    });
                    matches.then_some(Ok((tid, tuple)))
                }
                Err(error) => Some(Err(error)),
            }
        },
    )))
}

impl ExecNode for IndexScan {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        self.iter.next().transpose().map_err(ExecError::from)
    }
}

#[cfg(test)]
mod tests {
    use crabgresql_binder::BoundExpr;
    use crabgresql_planner::{IndexBoundExpr, IndexProbeSpec};
    use crabgresql_storage_api::ColumnProjection;
    use crabgresql_types::Value;

    use super::IndexScan;
    use crate::ExecContext;
    use crate::testutil::{collect, indexed_table, int4, rtxn, test_ok, test_table};

    /// A one-column equality probe.
    fn eq_probe(column: usize, value: BoundExpr) -> IndexProbeSpec {
        IndexProbeSpec {
            eq: vec![(column, value)],
            lower: None,
            upper: None,
        }
    }

    #[test]
    fn index_scan_probes_physical_index() {
        let table = indexed_table();
        let mut node = test_ok(IndexScan::new(
            &table,
            "t_id_key",
            eq_probe(0, int4(2)),
            &ExecContext::default(),
            &rtxn(),
            &ColumnProjection::All,
        ));
        assert_eq!(
            collect(&mut node),
            vec![vec![Value::Int4(2), Value::Text("two".into())]]
        );
    }

    #[test]
    fn index_scan_falls_back_to_scan_without_physical_index() {
        // `test_table` has no physical index: `index_lookup` returns None and the
        // node scans, re-checking the key so the result is still exact.
        let table = test_table();
        let mut node = test_ok(IndexScan::new(
            &table,
            "missing_index",
            eq_probe(0, int4(2)),
            &ExecContext::default(),
            &rtxn(),
            &ColumnProjection::All,
        ));
        assert_eq!(
            collect(&mut node),
            vec![vec![Value::Int4(2), Value::Text("two".into())]]
        );
    }

    /// The fallback has to re-check the *bounds* too, not just the equalities.
    /// It is reached whenever the engine declines a probe the planner already
    /// chose — a `DROP INDEX` landing mid-statement — and a fallback that
    /// checked only equality would return every row instead of the bounded ones.
    #[test]
    fn the_scan_fallback_applies_the_bounds() {
        let table = test_table();
        for (lower, upper, want) in [
            (Some((int4(1), false)), None, vec![2, 3]),
            (Some((int4(2), true)), None, vec![2, 3]),
            (None, Some((int4(2), false)), vec![1]),
            (None, Some((int4(2), true)), vec![1, 2]),
            (Some((int4(2), true)), Some((int4(2), true)), vec![2]),
            (Some((int4(2), false)), Some((int4(2), false)), vec![]),
        ] {
            let bound = |end: Option<(BoundExpr, bool)>| {
                end.map(|(value, inclusive)| {
                    Box::new(IndexBoundExpr {
                        column: 0,
                        value,
                        inclusive,
                    })
                })
            };
            // `test_table` has no physical index, so this is the fallback path.
            let mut node = test_ok(IndexScan::new(
                &table,
                "missing_index",
                IndexProbeSpec {
                    eq: Vec::new(),
                    lower: bound(lower.clone()),
                    upper: bound(upper.clone()),
                },
                &ExecContext::default(),
                &rtxn(),
                &ColumnProjection::All,
            ));
            let ids: Vec<i32> = collect(&mut node)
                .iter()
                .map(|row| match row[0] {
                    Value::Int4(id) => id,
                    ref other => panic!("unexpected id {other:?}"),
                })
                .collect();
            assert_eq!(ids, want, "lower={lower:?} upper={upper:?}");
        }
    }

    #[test]
    fn index_scan_empty_for_missing_key() {
        let table = indexed_table();
        let mut node = test_ok(IndexScan::new(
            &table,
            "t_id_key",
            eq_probe(0, int4(99)),
            &ExecContext::default(),
            &rtxn(),
            &ColumnProjection::All,
        ));
        assert!(collect(&mut node).is_empty());
    }
}
