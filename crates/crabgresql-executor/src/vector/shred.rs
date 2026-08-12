use arrow_array::RecordBatch;
use crabgresql_storage_api::TableSchema;
use crabgresql_storage_api::arrow::decode_columns;

use super::{BatchLayout, BatchNode};
use crate::{ExecError, ExecNode, Tuple};

/// Turns a batch stream back into a tuple stream — the boundary where a
/// columnar segment ends and the row executor resumes.
///
/// Every vectorized plan has exactly one of these per segment. It is pure cost
/// (the work the columnar nodes below it saved has to be paid back for the rows
/// that survive), which is the whole argument for pushing selective operators
/// like a filter *below* it: fewer surviving rows, less to shred.
///
/// A batch with no rows is skipped rather than ending the stream — an empty
/// batch means "nothing here", not "nothing left", and a filter that rejects
/// everything in one batch produces exactly that.
pub struct Shred {
    child: Box<dyn BatchNode>,
    /// The batch's column types, in the shape [`decode_columns`] takes. Only
    /// its `columns` are read; the relation name is never used.
    schema: TableSchema,
    /// Which batch columns actually carry values.
    ///
    /// A scan's batch is full width, but the columns outside its
    /// [`ColumnProjection`] are all-NULL padding that only exists so a schema
    /// ordinal is a batch ordinal. Decoding those would make the per-row cost
    /// scale with the table's width instead of with the query's — on a
    /// hundred-column relation read for two columns, fifty times the work the
    /// row scan does. `decode_columns` leaves the slots it is not given as `Null`,
    /// which is exactly the row scan's contract for an unprojected column.
    positions: Vec<usize>,
    batch: Option<RecordBatch>,
    row: usize,
}

impl Shred {
    pub fn new(child: Box<dyn BatchNode>, layout: BatchLayout, positions: Vec<usize>) -> Self {
        Shred {
            child,
            schema: TableSchema::new("", layout.to_vec()),
            positions,
            batch: None,
            row: 0,
        }
    }

    /// Every column of the batch carries a value — the shape an operator that
    /// builds its own output columns (a projection, a sort) hands up.
    pub fn dense(child: Box<dyn BatchNode>, layout: BatchLayout) -> Self {
        let positions = (0..layout.len()).collect();
        Shred::new(child, layout, positions)
    }
}

impl ExecNode for Shred {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        loop {
            if let Some(batch) = &self.batch
                && self.row < batch.num_rows()
            {
                let row = self.row;
                self.row += 1;
                return decode_columns(&self.schema, &self.positions, batch, row)
                    .map(Some)
                    .map_err(ExecError::from);
            }
            match self.child.next_batch()? {
                Some(batch) => {
                    self.batch = Some(batch);
                    self.row = 0;
                }
                None => return Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crabgresql_binder::BinOp;
    use crabgresql_storage_api::Tuple;
    use crabgresql_storage_api::arrow::build_scan_batch;
    use crabgresql_types::{PgType, Value};

    use super::Shred;
    use crate::ExecNode;
    use crate::vector::layout_of;
    use crate::vector::testutil::{Batches, column, columnar_filter, compare, constant, schema_of};

    /// `Shred` decodes only the columns a scan filled. A batch is full width so a
    /// schema ordinal is a batch ordinal, but the columns outside the projection
    /// are all-NULL padding — decoding those makes the per-row cost scale with the
    /// table rather than the query, and the row scan never did.
    #[test]
    fn shred_decodes_only_the_projected_columns() {
        let schema = schema_of(&[PgType::Int4, PgType::Text, PgType::Int8]);
        let rows = vec![vec![
            Value::Int4(1),
            Value::Text("x".into()),
            Value::Int8(9),
        ]];
        let batch = build_scan_batch(&schema, &rows).expect("batch");

        // Only column 2 was "projected"; the rest read back as Null, which is what
        // the row scan promises for an unselected column.
        let mut node = Shred::new(
            Box::new(Batches(vec![batch].into_iter())),
            layout_of(&schema),
            vec![2],
        );
        let row = node.next().expect("shred").expect("a row");
        assert_eq!(row, vec![Value::Null, Value::Null, Value::Int8(9)]);
    }

    /// A filter that rejects everything in a batch yields an empty batch, and the
    /// shred above must read that as "nothing here", not "end of stream" — the
    /// rows in the *next* batch still have to come out.
    #[test]
    fn an_entirely_rejected_batch_does_not_end_the_stream() {
        let schema = schema_of(&[PgType::Int4]);
        // First batch all rejected, second batch all kept.
        let rows: Vec<Tuple> = vec![
            vec![Value::Int4(0)],
            vec![Value::Int4(0)],
            vec![Value::Int4(5)],
            vec![Value::Int4(5)],
        ];
        let kept = columnar_filter(
            &schema,
            &rows,
            &compare(
                BinOp::Eq,
                PgType::Int4,
                column(0, PgType::Int4),
                constant(Value::Int4(5), PgType::Int4),
            ),
        )
        .expect("filter");
        assert_eq!(kept, vec![vec![Value::Int4(5)], vec![Value::Int4(5)]]);
    }
}
