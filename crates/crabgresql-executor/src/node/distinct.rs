use crabgresql_binder::DistinctKey;
use crabgresql_storage_api::Tuple;
use crabgresql_types::{PgType, Value};

use crate::{ExecError, ExecNode, agg, keyindex};

/// Materializing de-duplication for `SELECT DISTINCT` / `DISTINCT ON`. On the
/// first pull it buffers every child row and keeps the first row seen per
/// distinct-key group (NULL-aware, so two NULL keys collapse — PG's DISTINCT
/// semantics), preserving input order. When the input is already sorted (as
/// `DISTINCT ON` requires), "first per group" is the sort-order winner. Keys may
/// reference hidden columns the child kept past `visible_width`; those are
/// dropped from each surviving row here (the sort below no longer truncates when
/// a Distinct follows).
pub struct Distinct {
    rows: std::vec::IntoIter<Tuple>,
}

impl Distinct {
    pub fn new(
        mut child: Box<dyn ExecNode>,
        keys: Vec<DistinctKey>,
        visible_width: usize,
    ) -> Result<Self, ExecError> {
        let key_tys: Vec<PgType> = keys.iter().map(|k| k.ty).collect();
        let mut out: Vec<Tuple> = Vec::new();
        // A row's distinct key → the index into `out` of the row that already
        // represents it. Mirrors the grouping in `Aggregate::build`.
        let mut lookup = keyindex::GroupIndex::new(&key_tys);
        while let Some(row) = child.next()? {
            let key: Vec<Value> = keys.iter().map(|k| row[k.column].clone()).collect();
            // A surviving row's key is read column by column out of `out[i]`
            // rather than gathered into a `Vec`: a probe compares against every
            // candidate, and materializing each one would allocate per
            // comparison (a `String` per text key column at that).
            let slot = lookup.find_or_insert(&key, out.len(), |i| {
                keys.iter()
                    .zip(&key)
                    .all(|(k, probe)| agg::value_eq(k.ty, &out[i][k.column], probe))
            });
            if slot == keyindex::Slot::Vacant {
                out.push(row);
            }
        }
        // Drop hidden distinct/sort-only columns so downstream sees exactly the
        // visible output width, as `Sort` does when no Distinct follows.
        for row in &mut out {
            row.truncate(visible_width);
        }
        Ok(Self {
            rows: out.into_iter(),
        })
    }
}

impl ExecNode for Distinct {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        Ok(self.rows.next())
    }
}

#[cfg(test)]
mod tests {
    use crabgresql_binder::DistinctKey;
    use crabgresql_types::{PgType, Value};

    use super::Distinct;
    use crate::testutil::{VecSource, collect};

    #[test]
    fn distinct_dedups_keeping_first_seen_order() -> anyhow::Result<()> {
        // Plain SELECT DISTINCT over a single column: keep the first occurrence
        // of each value in input order, collapsing NULLs together.
        let rows = vec![
            vec![Value::Int4(2)],
            vec![Value::Int4(1)],
            vec![Value::Int4(2)],
            vec![Value::Null],
            vec![Value::Int4(1)],
            vec![Value::Null],
        ];
        let keys = vec![DistinctKey {
            column: 0,
            ty: PgType::Int4,
        }];
        let mut node = Distinct::new(VecSource::boxed(rows), keys, 1)?;
        assert_eq!(
            collect(&mut node),
            vec![
                vec![Value::Int4(2)],
                vec![Value::Int4(1)],
                vec![Value::Null],
            ],
            "duplicates removed, first-seen order preserved, NULLs collapsed"
        );
        Ok(())
    }

    #[test]
    fn distinct_on_keys_hidden_column_and_trims() -> anyhow::Result<()> {
        // DISTINCT ON (b) a — b is a hidden trailing column (index 1) the client
        // never sees. Rows arrive already sorted by b (as DISTINCT ON requires);
        // the first row of each b-group survives and the hidden column is
        // trimmed, leaving only the visible `a`.
        let rows = vec![
            vec![Value::Int4(10), Value::Int4(1)],
            vec![Value::Int4(11), Value::Int4(1)],
            vec![Value::Int4(20), Value::Int4(2)],
            vec![Value::Int4(21), Value::Int4(2)],
        ];
        let keys = vec![DistinctKey {
            column: 1,
            ty: PgType::Int4,
        }];
        let mut node = Distinct::new(VecSource::boxed(rows), keys, 1)?;
        assert_eq!(
            collect(&mut node),
            vec![vec![Value::Int4(10)], vec![Value::Int4(20)]],
            "one row per DISTINCT ON group, hidden key column trimmed"
        );
        Ok(())
    }
}
