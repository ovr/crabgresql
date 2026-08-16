use std::cmp::Ordering;
use std::sync::Arc;

use crabgresql_planner::{IndexBoundExpr, IndexProbeSpec};
use crabgresql_storage_api::{
    ColumnProjection, IndexBound, IndexProbe, IndexProbeKey, StorageError, TableAm, Tuple,
};
use crabgresql_txn::TxnContext;
use crabgresql_types::{PgType, Value};

use crate::tally::ScanTally;
use crabgresql_binder::{SysCol, SystemEmit};

use crate::{
    ExecContext, ExecError, ExecNode, compare_values, eval, push_system, resolve_tableoid,
};

/// Index scan: probes the engine's physical index for the key. When the engine
/// cannot serve it (a columnar engine, an index whose key type it cannot
/// physically index, a system catalog) it falls back to a full scan and
/// re-checks the key per row, which is what makes that fallback correct. The
/// physical-index path is already exact (the engine returns only rows the key
/// selects), so it needs no re-check. NULL matches no comparison at all.
pub struct IndexScan {
    iter: Box<dyn Iterator<Item = Result<Tuple, StorageError>> + Send>,
    /// Counted even when the probe fell back to a full scan: `idx_scan` records
    /// the plan the executor ran, not how the access method served it.
    tally: Option<ScanTally>,
}

impl IndexScan {
    /// `system`, when set, names the slots each row carries past the relation's
    /// declared columns — the same widening an `Append` arm applies, done here
    /// so a statement that reads `ctid` keeps its index path.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        table: &Arc<dyn TableAm>,
        index_name: &str,
        key: IndexProbeSpec,
        system: Option<SystemEmit>,
        ctx: &ExecContext,
        txn: &TxnContext,
        projection: &ColumnProjection,
    ) -> Result<Self, ExecError> {
        let tally = ScanTally::index(ctx, table, index_name);
        let Some(emit) = system else {
            let rows = index_probe_rows(table, index_name, &key, ctx, txn, projection)?;
            return Ok(Self {
                iter: Box::new(rows.map(|row| row.map(|(_, tuple)| tuple))),
                tally,
            });
        };
        let cols = Arc::clone(&emit.cols);
        let oid = match cols.contains(&SysCol::TableOid) {
            true => Some(resolve_tableoid(&emit.ident, ctx)?),
            false => None,
        };
        let rows = match cols.iter().any(|c| c.needs_header()) {
            false => Box::new(
                index_probe_rows(table, index_name, &key, ctx, txn, projection)?
                    .map(|row| row.map(|(tid, tuple)| (tid, None, tuple))),
            ) as SystemProbe,
            true => index_probe_system_rows(table, index_name, &key, ctx, txn, projection)?,
        };
        Ok(Self {
            iter: Box::new(rows.map(move |row| {
                row.map(|(tid, hdr, mut tuple)| {
                    push_system(&mut tuple, &cols, oid, tid, hdr.as_ref());
                    tuple
                })
            })),
            tally,
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
    Ok(Box::new(
        probe_rows(table, index_name, key, false, ctx, txn, projection)?
            .map(|row| row.map(|(tid, _, tuple)| (tid, tuple))),
    ))
}

/// [`index_probe_rows`] carrying each version's MVCC header — what a statement
/// reading `xmin`/`xmax`/`cmin`/`cmax` off an index-probed relation needs.
///
/// Both paths widen, not just the engine's: the fallback is a full scan, and a
/// scan that dropped the header would silently answer those columns with a
/// placeholder on exactly the access methods that decline a probe.
pub(crate) fn index_probe_system_rows(
    table: &Arc<dyn TableAm>,
    index_name: &str,
    key: &IndexProbeSpec,
    ctx: &ExecContext,
    txn: &TxnContext,
    projection: &ColumnProjection,
) -> Result<SystemProbe, ExecError> {
    probe_rows(table, index_name, key, true, ctx, txn, projection)
}

/// The rows both of the above yield. `with_header` picks the header-carrying
/// half of each path; the key evaluation, the probe key and the fallback's
/// re-check are shared, so the two can never disagree about which rows match.
#[allow(clippy::too_many_arguments)]
fn probe_rows(
    table: &Arc<dyn TableAm>,
    index_name: &str,
    key: &IndexProbeSpec,
    with_header: bool,
    ctx: &ExecContext,
    txn: &TxnContext,
    projection: &ColumnProjection,
) -> Result<SystemProbe, ExecError> {
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
    // Exact path: the engine already returned only the selected, MVCC-visible
    // rows. An engine that serves `index_lookup` but declines the header
    // variant falls through to the scan below, which is correct but slower —
    // see `TableAm::index_lookup_with_system`.
    match with_header {
        true => {
            if let Some(rows) = table.index_lookup_with_system(index_name, &probe, txn) {
                return Ok(Box::new(
                    rows.map(|row| row.map(|(t, h, v)| (t, Some(h), v))),
                ));
            }
        }
        false => {
            if let Some(rows) = table.index_lookup(index_name, &probe, txn) {
                return Ok(Box::new(rows.map(|row| row.map(|(t, v)| (t, None, v)))));
            }
        }
    }
    // Fallback path: a full scan, so re-check the whole key per row — the bounds
    // as well as the equalities, or the scan would return every row past the
    // probe's start.
    let column_ty = |column: usize| table.schema().columns[column].ty;
    // One list, because an equality is the degenerate bound: the value must
    // compare `Equal`, and `Equal` is accepted. A lower bound keeps rows *above*
    // its value and an upper bound rows below it, so the third field is the
    // ordering each test accepts besides its own `Equal` case.
    let tests: Vec<KeyTest> = key
        .eq
        .iter()
        .zip(eq)
        .map(|((column, _), want)| (*column, want, Ordering::Equal, true))
        .chain(lower.map(|(b, value)| (b.column, value, Ordering::Greater, b.inclusive)))
        .chain(upper.map(|(b, value)| (b.column, value, Ordering::Less, b.inclusive)))
        .map(|(column, want, side, inclusive)| KeyTest {
            column,
            ty: column_ty(column),
            want,
            side,
            inclusive,
        })
        .collect();
    // The planner folds every key column into `projection` precisely so this
    // re-check can read them.
    let rows: SystemProbe = match with_header {
        false => Box::new(
            table
                .scan(txn, projection)
                .map(|row| row.map(|(tid, tuple)| (tid, None, tuple))),
        ),
        true => Box::new(
            table
                .scan_with_system(txn, projection)
                .expect(
                    "the binder rejects a header system column the access method declines, \
                     so a statement reaching here can produce one",
                )
                .map(|row| row.map(|(tid, hdr, tuple)| (tid, Some(hdr), tuple))),
        ),
    };
    Ok(Box::new(rows.filter_map(move |row| {
        match row {
            Ok((tid, hdr, tuple)) => tests
                .iter()
                .all(|test| test.holds(&tuple))
                .then_some(Ok((tid, hdr, tuple))),
            Err(error) => Some(Err(error)),
        }
    })))
}

/// The stream [`probe_rows`] hands back: [`crate::SystemRow`]s, fallible for the
/// same reason a scan's are.
pub(crate) type SystemProbe =
    Box<dyn Iterator<Item = Result<crate::SystemRow, StorageError>> + Send>;

/// One key column's test, as the scan fallback re-checks it per row.
struct KeyTest {
    column: usize,
    ty: PgType,
    want: Value,
    /// The ordering that satisfies this test outright: `Equal` for an equality,
    /// `Greater` for a lower bound, `Less` for an upper one.
    side: Ordering,
    /// Whether comparing `Equal` satisfies it too.
    inclusive: bool,
}

impl KeyTest {
    fn holds(&self, tuple: &[Value]) -> bool {
        let cell = &tuple[self.column];
        // NULL satisfies neither an equality nor a bound, on either side of it.
        if matches!(cell, Value::Null) || matches!(self.want, Value::Null) {
            return false;
        }
        match compare_values(self.ty, cell, &self.want) {
            Ordering::Equal => self.inclusive,
            other => other == self.side,
        }
    }
}

impl ExecNode for IndexScan {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        let row = self.iter.next().transpose().map_err(ExecError::from)?;
        if row.is_some()
            && let Some(tally) = &mut self.tally
        {
            tally.saw(1);
        }
        Ok(row)
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
    use crate::testutil::{collect, ids, indexed_table, int4, rtxn, test_ok, test_table};

    #[test]
    fn index_scan_probes_physical_index() {
        let table = indexed_table();
        let mut node = test_ok(IndexScan::new(
            &table,
            "t_id_key",
            IndexProbeSpec::equality(vec![(0, int4(2))]),
            None,
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
            IndexProbeSpec::equality(vec![(0, int4(2))]),
            None,
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
                None,
                &ExecContext::default(),
                &rtxn(),
                &ColumnProjection::All,
            ));
            assert_eq!(ids(&mut node), want, "lower={lower:?} upper={upper:?}");
        }
    }

    #[test]
    fn index_scan_empty_for_missing_key() {
        let table = indexed_table();
        let mut node = test_ok(IndexScan::new(
            &table,
            "t_id_key",
            IndexProbeSpec::equality(vec![(0, int4(99))]),
            None,
            &ExecContext::default(),
            &rtxn(),
            &ColumnProjection::All,
        ));
        assert!(collect(&mut node).is_empty());
    }
}
