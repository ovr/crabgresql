use std::cmp::Ordering;

use crabgresql_binder::SortKey;
use crabgresql_storage_api::Tuple;
use crabgresql_types::Value;

use crate::{ExecError, ExecNode, compare_values_collated};

/// Materializing sort (ORDER BY). NULLs order per `SortKey.nulls_first`;
/// non-null values compare via the key's type total order. Keys may reference
/// hidden columns appended past `visible_width`; those are dropped from each
/// emitted tuple so only the client-visible columns leave the node.
pub struct Sort {
    rows: std::vec::IntoIter<Tuple>,
}

impl Sort {
    pub fn new(
        mut child: Box<dyn ExecNode>,
        keys: Vec<SortKey>,
        visible_width: usize,
    ) -> Result<Self, ExecError> {
        let mut rows: Vec<Tuple> = Vec::new();
        while let Some(row) = child.next()? {
            rows.push(row);
        }
        // Stable sort preserves input order for equal keys, as PG does for a
        // sort with no tiebreak.
        rows.sort_by(|a, b| compare_rows(a, b, &keys));
        // Drop hidden sort-only columns so downstream (the wire layer, an outer
        // subquery) sees exactly the visible output width. Comparison above
        // already read them; they are no longer needed.
        for row in &mut rows {
            row.truncate(visible_width);
        }
        Ok(Self {
            rows: rows.into_iter(),
        })
    }
}

impl ExecNode for Sort {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        Ok(self.rows.next())
    }
}

/// Order two rows by `keys`, the comparison behind every `ORDER BY` in the
/// engine. Shared by [`Sort`] and [`WindowAgg`] so the window sort and the
/// query's own can never disagree about where NULLs go.
pub(crate) fn compare_rows(a: &Tuple, b: &Tuple, keys: &[SortKey]) -> Ordering {
    for key in keys {
        let (va, vb) = (&a[key.column], &b[key.column]);
        // NULL placement follows nulls_first directly; only the value
        // comparison is reversed for DESC. (Reversing the null branch
        // too would flip NULLS FIRST/LAST for descending sorts.)
        let ord = match (matches!(va, Value::Null), matches!(vb, Value::Null)) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if key.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if key.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, false) => {
                let cmp = compare_values_collated(key.ty, va, vb, key.collation);
                if key.asc { cmp } else { cmp.reverse() }
            }
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use crabgresql_binder::{BoundExpr, SortKey};
    use crabgresql_storage_api::ColumnProjection;
    use crabgresql_types::collation::DEFAULT_COLLATION_OID;
    use crabgresql_types::{PgType, Value};

    use super::Sort;
    use crate::testutil::{collect, rtxn, test_table};
    use crate::{ExecContext, Projection, SeqScan};

    #[test]
    fn sort_orders_by_hidden_column_then_trims() -> anyhow::Result<()> {
        // Project only `id` (visible), but sort on a hidden trailing column
        // holding `label` (a resjunk column that ORDER BY references but the
        // client never sees). The sort must order by the hidden value's type
        // and then drop it, emitting a single visible column.
        let table = test_table();
        let exprs = vec![
            BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4,
            },
            BoundExpr::ColumnRef {
                index: 1,
                ty: PgType::Text,
            },
        ];
        let projection = Projection::new(
            Box::new(SeqScan::new(&table, &rtxn(), &ColumnProjection::All)),
            exprs,
            ExecContext::default(),
        );
        // ORDER BY label ASC: NULL (id 3) sorts last (NULLS LAST default), then
        // 'one' (id 1), 'two' (id 2).
        let key = SortKey {
            column: 1,
            ty: PgType::Text,
            collation: DEFAULT_COLLATION_OID,
            asc: true,
            nulls_first: false,
        };
        let mut node = Sort::new(Box::new(projection), vec![key], 1)?;
        assert_eq!(
            collect(&mut node),
            vec![
                vec![Value::Int4(1)],
                vec![Value::Int4(2)],
                vec![Value::Int4(3)],
            ],
            "rows ordered by hidden label, trimmed to the single visible column"
        );

        Ok(())
    }
}
