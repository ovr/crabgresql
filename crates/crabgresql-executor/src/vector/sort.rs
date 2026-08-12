//! Columnar sort.

use std::sync::Arc;

use arrow_array::{RecordBatch, RecordBatchOptions};
use arrow_schema::{Field, Schema};
use arrow_select::concat::concat_batches;
use crabgresql_binder::SortKey;
use crabgresql_planner::vectorize;
use crabgresql_storage_api::arrow::scan_schema;
use crabgresql_storage_api::{IndexKey, sort};

use super::{BatchLayout, BatchNode, internal};
use crate::ExecError;

/// Materializing sort — the columnar [`crate::Sort`].
///
/// Same memory model as the row node: everything is buffered before the first
/// row comes out, so this changes the representation, not the contract.
///
/// The ordering itself belongs to [`crabgresql_storage_api::sort`], which the
/// columnar write path shares: PostgreSQL's `-0.0`/NaN key rewrite and the
/// stability tiebreak are stated once, there.
pub struct SortBatch {
    rows: Option<RecordBatch>,
    emitted: bool,
}

impl SortBatch {
    /// Whether every key can be sorted by Arrow with PostgreSQL's ordering.
    pub fn compilable(keys: &[SortKey], layout: &BatchLayout) -> bool {
        !keys.is_empty()
            && keys
                .iter()
                .all(|key| key.column < layout.len() && vectorize::sortable_key(key))
    }

    /// Drain `child`, sort it, and keep the result for [`BatchNode::next_batch`].
    ///
    /// `visible_width` drops the hidden ORDER BY columns the planner appended
    /// past the output, exactly as the row node's truncation does.
    pub fn new(
        mut child: Box<dyn BatchNode>,
        keys: &[SortKey],
        layout: &BatchLayout,
        visible_width: usize,
    ) -> Result<Self, ExecError> {
        let schema = scan_schema(&crabgresql_storage_api::TableSchema::new(
            "",
            layout.to_vec(),
        ));
        let mut batches = Vec::new();
        while let Some(batch) = child.next_batch()? {
            batches.push(batch);
        }
        let all = concat_batches(&schema, &batches)
            .map_err(|error| internal(&format!("sort concat failed: {error}")))?;
        // `concat_batches` copies, so the inputs are now a second full copy of
        // the relation. Release them before the take allocates a third — this
        // node already holds everything in memory, and holding it three times
        // over turns a sort that fit into one that does not.
        drop(batches);

        // A `SortKey` is an `IndexKey` with the direction spelled the other way
        // round; everything else about the two is the same column-and-flags
        // triple the sort kernel wants.
        let index_keys: Vec<IndexKey> = keys
            .iter()
            .map(|key| IndexKey {
                column: key.column,
                descending: !key.asc,
                nulls_first: key.nulls_first,
            })
            .collect();
        let indices = sort::sort_permutation(&all, &index_keys)?;
        let sorted = sort::take_columns(&all, &indices, visible_width)?;

        let fields: Vec<Field> = schema
            .fields()
            .iter()
            .take(visible_width)
            .map(|field| field.as_ref().clone())
            .collect();
        let height = all.num_rows();
        // The sorted copy is complete; the unsorted one is dead weight now.
        drop(all);
        let options = RecordBatchOptions::new().with_row_count(Some(height));
        let rows =
            RecordBatch::try_new_with_options(Arc::new(Schema::new(fields)), sorted, &options)
                .map_err(|error| internal(&format!("sort rebuild failed: {error}")))?;
        Ok(SortBatch {
            rows: Some(rows),
            emitted: false,
        })
    }
}

impl BatchNode for SortBatch {
    fn next_batch(&mut self) -> Result<Option<RecordBatch>, ExecError> {
        if self.emitted {
            return Ok(None);
        }
        self.emitted = true;
        Ok(self.rows.take())
    }
}

#[cfg(test)]
mod tests {
    use crabgresql_storage_api::Tuple;
    use crabgresql_types::{PgType, Value};

    use super::SortBatch;
    use crate::vector::layout_of;
    use crate::vector::testutil::{
        assert_same_order, columnar_sort, int_rows, schema_of, sort_key,
    };

    /// All four ASC/DESC × NULLS FIRST/LAST combinations. PostgreSQL keeps NULL
    /// placement independent of the direction, so a DESC sort does not flip it —
    /// assumed nowhere, checked here against the row node.
    #[test]
    fn every_direction_and_null_placement_agrees() {
        let schema = schema_of(&[PgType::Int4]);
        let rows = int_rows();
        for asc in [true, false] {
            for nulls_first in [true, false] {
                assert_same_order(
                    &schema,
                    &rows,
                    &[sort_key(0, PgType::Int4, asc, nulls_first)],
                    1,
                );
            }
        }
    }

    /// Equal keys must come out in input order. `lexsort_to_indices` is not a
    /// stable sort, so this passes only because of the appended position key.
    #[test]
    fn ties_keep_input_order() {
        let schema = schema_of(&[PgType::Int4, PgType::Text]);
        // One key value, many payloads: every row is a tie.
        let rows: Vec<Tuple> = ["a", "b", "c", "d", "e", "f"]
            .into_iter()
            .map(|s| vec![Value::Int4(1), Value::Text(s.into())])
            .collect();
        let sorted = columnar_sort(&schema, &rows, &[sort_key(0, PgType::Int4, true, false)], 2)
            .expect("columnar sort");
        assert_eq!(sorted, rows, "a tie must preserve input order");
        assert_same_order(&schema, &rows, &[sort_key(0, PgType::Int4, true, false)], 2);
    }

    /// The float repair. PostgreSQL calls `-0.0` and `0.0` equal and every NaN
    /// equal to every other; Arrow's total order does neither. Without
    /// canonicalization the two paths would order these rows differently.
    #[test]
    fn float_zero_and_nan_sort_as_postgresql_does() {
        let schema = schema_of(&[PgType::Float8, PgType::Int4]);
        let rows: Vec<Tuple> = [
            (0.0_f64, 1),
            (-0.0, 2),
            (f64::NAN, 3),
            (1.5, 4),
            (-f64::NAN, 5),
            (-1.5, 6),
            (0.0, 7),
        ]
        .into_iter()
        .map(|(f, i)| vec![Value::Float8(f), Value::Int4(i)])
        .collect();
        for asc in [true, false] {
            assert_same_order(
                &schema,
                &rows,
                &[sort_key(0, PgType::Float8, asc, false)],
                2,
            );
        }
    }

    /// A multi-key sort, where the second key only decides rows the first ties.
    #[test]
    fn multi_key_sorts_agree() {
        let schema = schema_of(&[PgType::Int4, PgType::Text]);
        let rows: Vec<Tuple> = [
            (Some(2), Some("b")),
            (Some(1), Some("z")),
            (Some(2), Some("a")),
            (None, Some("m")),
            (Some(1), None),
            (Some(1), Some("a")),
        ]
        .into_iter()
        .map(|(i, s)| {
            vec![
                i.map_or(Value::Null, Value::Int4),
                s.map_or(Value::Null, |s| Value::Text(s.into())),
            ]
        })
        .collect();
        assert_same_order(
            &schema,
            &rows,
            &[
                sort_key(0, PgType::Int4, true, false),
                sort_key(1, PgType::Text, false, true),
            ],
            2,
        );
    }

    /// A hidden ORDER BY column — one the planner appended past the visible output —
    /// orders the rows and is then dropped, leaving the client width.
    #[test]
    fn hidden_sort_columns_are_dropped_after_ordering() {
        let schema = schema_of(&[PgType::Text, PgType::Int4]);
        let rows: Vec<Tuple> = [("a", 3), ("b", 1), ("c", 2)]
            .into_iter()
            .map(|(s, i)| vec![Value::Text(s.into()), Value::Int4(i)])
            .collect();
        // Order by column 1, emit only column 0.
        let keys = [sort_key(1, PgType::Int4, true, false)];
        let sorted = columnar_sort(&schema, &rows, &keys, 1).expect("columnar sort");
        assert_eq!(
            sorted,
            vec![
                vec![Value::Text("b".into())],
                vec![Value::Text("c".into())],
                vec![Value::Text("a".into())],
            ]
        );
        assert_same_order(&schema, &rows, &keys, 1);
    }

    /// `"char"` sorts columnar, and by its *unsigned* byte. It is stored as
    /// `UInt8` for exactly that reason — under a signed encoding `0xFF` would sort
    /// below `0x00` and contradict the type — so the columnar node must agree with
    /// the row node on a high-bit byte.
    #[test]
    fn a_char_column_sorts_columnar_by_its_unsigned_byte() {
        let schema = schema_of(&[PgType::Char]);
        let rows: Vec<Tuple> = [0xFF, 0x41, 0x00, 0x80]
            .into_iter()
            .map(|byte| vec![Value::Char(byte)])
            .collect();
        let keys = [sort_key(0, PgType::Char, true, false)];
        assert!(SortBatch::compilable(&keys, &layout_of(&schema)));
        let sorted = columnar_sort(&schema, &rows, &keys, 1).expect("columnar sort");
        assert_eq!(
            sorted,
            [0x00, 0x41, 0x80, 0xFF]
                .into_iter()
                .map(|byte| vec![Value::Char(byte)])
                .collect::<Vec<_>>()
        );
        assert_same_order(&schema, &rows, &keys, 1);
    }

    /// Sort keys whose Arrow order is not PostgreSQL's are refused, so the row
    /// `Sort` keeps them. `numeric` is the dangerous one: stored as text, it would
    /// sort `'10'` before `'9'` without any error.
    #[test]
    fn unsortable_key_types_are_refused() {
        for ty in [
            PgType::Numeric,
            PgType::Bpchar,
            PgType::Interval,
            PgType::TimeTz,
        ] {
            let schema = schema_of(&[ty]);
            assert!(
                !SortBatch::compilable(&[sort_key(0, ty, true, false)], &layout_of(&schema)),
                "{ty:?} must not sort columnar"
            );
        }
        // An ICU collation orders text by locale rules, not bytes.
        let schema = schema_of(&[PgType::Text]);
        let mut key = sort_key(0, PgType::Text, true, false);
        key.collation = 0xC000_0000;
        assert!(!SortBatch::compilable(&[key], &layout_of(&schema)));
    }

    /// An empty input sorts to an empty result rather than failing on the
    /// zero-batch concat.
    #[test]
    fn an_empty_input_sorts_to_nothing() {
        let schema = schema_of(&[PgType::Int4]);
        let sorted = columnar_sort(&schema, &[], &[sort_key(0, PgType::Int4, true, false)], 1)
            .expect("columnar sort");
        assert!(sorted.is_empty());
    }
}
