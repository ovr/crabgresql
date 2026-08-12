use crabgresql_storage_api::Tuple;

use crate::{ExecError, ExecNode};

/// Streaming LIMIT/OFFSET. Discards the first `remaining_offset` child tuples,
/// then passes through up to `remaining_limit` more before stopping. Unlike
/// [`Sort`](crate::Sort) it never materializes: rows flow through one at a time, and once the
/// limit is reached it stops pulling from the child entirely.
pub struct Limit {
    child: Box<dyn ExecNode>,
    remaining_offset: u64,
    /// `None` = unbounded (no LIMIT).
    remaining_limit: Option<u64>,
}

impl Limit {
    /// Negative counts are rejected at bind time, so both are non-negative here.
    pub fn new(child: Box<dyn ExecNode>, limit: Option<i64>, offset: Option<i64>) -> Self {
        Self {
            child,
            remaining_offset: offset.unwrap_or(0).max(0) as u64,
            remaining_limit: limit.map(|n| n.max(0) as u64),
        }
    }
}

impl ExecNode for Limit {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        // Skip the offset rows first (they still count as consumed).
        while self.remaining_offset > 0 {
            match self.child.next()? {
                Some(_) => self.remaining_offset -= 1,
                None => return Ok(None),
            }
        }
        match self.remaining_limit {
            Some(0) => Ok(None),
            Some(n) => match self.child.next()? {
                Some(row) => {
                    self.remaining_limit = Some(n - 1);
                    Ok(Some(row))
                }
                None => Ok(None),
            },
            None => self.child.next(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crabgresql_binder::SortKey;
    use crabgresql_types::PgType;
    use crabgresql_types::collation::DEFAULT_COLLATION_OID;

    use super::Limit;
    use crate::Sort;
    use crate::testutil::{id_scan, ids, test_table};

    #[test]
    fn limit_caps_row_count() {
        let table = test_table();
        let mut node = Limit::new(id_scan(&table), Some(2), None);
        assert_eq!(ids(&mut node), vec![1, 2]);
    }

    #[test]
    fn offset_skips_leading_rows() {
        let table = test_table();
        let mut node = Limit::new(id_scan(&table), None, Some(1));
        assert_eq!(ids(&mut node), vec![2, 3]);
    }

    #[test]
    fn limit_offset_together_slice_the_middle() {
        let table = test_table();
        let mut node = Limit::new(id_scan(&table), Some(1), Some(1));
        assert_eq!(ids(&mut node), vec![2]);
    }

    #[test]
    fn offset_zero_passes_everything_through() {
        // The float4/float8 fence: OFFSET 0 is a no-op over the full input.
        let table = test_table();
        let mut node = Limit::new(id_scan(&table), None, Some(0));
        assert_eq!(ids(&mut node), vec![1, 2, 3]);
    }

    #[test]
    fn offset_past_end_yields_nothing() {
        let table = test_table();
        let mut node = Limit::new(id_scan(&table), None, Some(10));
        assert_eq!(ids(&mut node), Vec::<i32>::new());
    }

    #[test]
    fn limit_applies_after_sort() -> anyhow::Result<()> {
        // ORDER BY id DESC LIMIT 1 must return the max, not the first-scanned row.
        let table = test_table();
        let sort = Sort::new(
            id_scan(&table),
            vec![SortKey {
                column: 0,
                ty: PgType::Int4,
                collation: DEFAULT_COLLATION_OID,
                asc: false,
                nulls_first: false,
            }],
            1,
        )?;
        let mut node = Limit::new(Box::new(sort), Some(1), None);
        assert_eq!(ids(&mut node), vec![3]);

        Ok(())
    }
}
