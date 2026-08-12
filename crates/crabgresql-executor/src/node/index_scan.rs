use std::cmp::Ordering;
use std::sync::Arc;

use crabgresql_binder::BoundExpr;
use crabgresql_storage_api::{ColumnProjection, IndexProbe, StorageError, TableAm, Tuple};
use crabgresql_txn::TxnContext;
use crabgresql_types::{PgType, Value};

use crate::{ExecContext, ExecError, ExecNode, compare_values, eval};

/// Equality index scan: probes the engine's physical index for the key. When
/// the engine cannot serve it (a columnar engine, an index whose key type it
/// cannot physically index, a system catalog) it falls back to a full scan and
/// re-checks key equality per row, which is what makes that fallback correct.
/// The physical-index path is already exact (the engine returns only rows whose
/// key equals the probe), so it needs no re-check. NULL never matches under `=`.
pub struct IndexScan {
    iter: Box<dyn Iterator<Item = Result<Tuple, StorageError>> + Send>,
}

impl IndexScan {
    pub fn new(
        table: &Arc<dyn TableAm>,
        index_name: &str,
        key: Vec<(usize, BoundExpr)>,
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

/// The rows an equality index probe yields, each with its `Tid`.
///
/// Two paths, both producing exactly the key-matching, MVCC-visible rows:
/// the engine's own `index_lookup`, or — when it declines with `None` — a full
/// scan re-checking the key per row. The `Tid` is kept because DML needs it to
/// write the row back; [`IndexScan`] drops it.
pub(crate) fn index_probe_rows(
    table: &Arc<dyn TableAm>,
    index_name: &str,
    key: &[(usize, BoundExpr)],
    ctx: &ExecContext,
    txn: &TxnContext,
    projection: &ColumnProjection,
) -> Result<IndexProbe, ExecError> {
    // The key value expressions are row-constant (the planner guarantees it), so
    // they evaluate once against an empty row.
    let key_values: Vec<Value> = key
        .iter()
        .map(|(_, expr)| eval(expr, &[], ctx))
        .collect::<Result<_, _>>()?;
    Ok(match table.index_lookup(index_name, &key_values, txn) {
        // Exact path: the engine already returned only key-matching, MVCC-visible
        // rows.
        Some(rows) => rows,
        // Fallback path: a full scan, so re-check the key per row.
        None => {
            let cols: Vec<(usize, PgType)> = key
                .iter()
                .map(|(column, _)| (*column, table.schema().columns[*column].ty))
                .collect();
            // The planner folds every key column into `projection` precisely so
            // this re-check can read them.
            Box::new(table.scan(txn, projection).filter_map(move |row| {
                match row {
                    Ok((tid, tuple)) => cols
                        .iter()
                        .zip(&key_values)
                        .all(|(&(column, ty), want)| {
                            let cell = &tuple[column];
                            !matches!(cell, Value::Null)
                                && !matches!(want, Value::Null)
                                && compare_values(ty, cell, want) == Ordering::Equal
                        })
                        .then_some(Ok((tid, tuple))),
                    Err(error) => Some(Err(error)),
                }
            }))
        }
    })
}

impl ExecNode for IndexScan {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        self.iter.next().transpose().map_err(ExecError::from)
    }
}

#[cfg(test)]
mod tests {
    use crabgresql_storage_api::ColumnProjection;
    use crabgresql_types::Value;

    use super::IndexScan;
    use crate::ExecContext;
    use crate::testutil::{collect, indexed_table, int4, rtxn, test_ok, test_table};

    #[test]
    fn index_scan_probes_physical_index() {
        let table = indexed_table();
        let mut node = test_ok(IndexScan::new(
            &table,
            "t_id_key",
            vec![(0, int4(2))],
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
            vec![(0, int4(2))],
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
    fn index_scan_empty_for_missing_key() {
        let table = indexed_table();
        let mut node = test_ok(IndexScan::new(
            &table,
            "t_id_key",
            vec![(0, int4(99))],
            &ExecContext::default(),
            &rtxn(),
            &ColumnProjection::All,
        ));
        assert!(collect(&mut node).is_empty());
    }
}
