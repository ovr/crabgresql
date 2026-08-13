//! The PostgreSQL/Unix epoch boundary, crossed only when a batch enters or
//! leaves a fragment.

use std::sync::Arc;

use arrow_array::{
    Array, ArrayRef, Date32Array, RecordBatch, RecordBatchOptions, TimestampMicrosecondArray,
};
use arrow_schema::{DataType, TimeUnit};
use crabgresql_storage_api::StorageError;

use crate::error::{corrupt, io_error};

const PG_UNIX_EPOCH_DAYS: i32 = 10_957;
const PG_UNIX_EPOCH_MICROS: i64 = 946_684_800_000_000;

/// Rebase the temporal columns of `batch` between PostgreSQL's epoch and the
/// Unix epoch Parquet's `Date32`/`Timestamp` logical types are defined in.
///
/// This is the **only** place the two epochs meet. Everywhere above it —
/// including every [`RecordBatch`] handed to the executor — a date is PG days
/// and a timestamp is PG microseconds, per the invariant documented on
/// [`crabgresql_storage_api::arrow`]. Keeping the shift at the file boundary is
/// what stops a relation's two storage leaves from disagreeing: the RAM buffer
/// never sees a file, so if the shift lived anywhere else, half a table's rows
/// would come back displaced by `PG_UNIX_EPOCH_DAYS` (about thirty years) with
/// no error to notice.
///
/// `delta` is added to every non-sentinel value; pass it negated to invert.
/// `i32::MIN`/`i32::MAX` and `i64::MIN`/`i64::MAX` are the ±infinity sentinels
/// and are ordinary bit patterns rather than instants, so they pass through
/// untouched — shifting them would both overflow and turn infinity into a date.
fn rebase_epoch(batch: &RecordBatch, days: i32, micros: i64) -> Result<RecordBatch, StorageError> {
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(batch.num_columns());
    let mut changed = false;
    for column in batch.columns() {
        match column.data_type() {
            DataType::Date32 => {
                let values = required_array::<Date32Array>(column.as_ref(), "date")?;
                let shifted: Date32Array = values.try_unary(|value| {
                    if value == i32::MIN || value == i32::MAX {
                        Ok(value)
                    } else {
                        value
                            .checked_add(days)
                            .ok_or_else(|| corrupt("date epoch conversion overflow"))
                    }
                })?;
                columns.push(Arc::new(shifted));
                changed = true;
            }
            DataType::Timestamp(TimeUnit::Microsecond, zone) => {
                let values =
                    required_array::<TimestampMicrosecondArray>(column.as_ref(), "timestamp")?;
                let shifted: TimestampMicrosecondArray = values.try_unary(|value| {
                    if value == i64::MIN || value == i64::MAX {
                        Ok(value)
                    } else {
                        value
                            .checked_add(micros)
                            .ok_or_else(|| corrupt("timestamp epoch conversion overflow"))
                    }
                })?;
                // `try_unary` drops the zone, and it is what distinguishes
                // `timestamptz` from `timestamp` in the file schema.
                columns.push(match zone {
                    Some(zone) => Arc::new(shifted.with_timezone(zone.clone())) as ArrayRef,
                    None => Arc::new(shifted) as ArrayRef,
                });
                changed = true;
            }
            _ => columns.push(Arc::clone(column)),
        }
    }
    if !changed {
        return Ok(batch.clone());
    }
    let options = RecordBatchOptions::new().with_row_count(Some(batch.num_rows()));
    RecordBatch::try_new_with_options(batch.schema(), columns, &options)
        .map_err(|error| io_error("rebase Arrow record batch", error))
}

/// PG epoch -> Unix epoch, on the way into a fragment.
pub(crate) fn to_file_epoch(batch: &RecordBatch) -> Result<RecordBatch, StorageError> {
    rebase_epoch(batch, PG_UNIX_EPOCH_DAYS, PG_UNIX_EPOCH_MICROS)
}

/// Unix epoch -> PG epoch, on the way out of a fragment.
pub(crate) fn from_file_epoch(batch: &RecordBatch) -> Result<RecordBatch, StorageError> {
    rebase_epoch(batch, -PG_UNIX_EPOCH_DAYS, -PG_UNIX_EPOCH_MICROS)
}

fn required_array<'a, T: 'static>(
    array: &'a dyn Array,
    column: &str,
) -> Result<&'a T, StorageError> {
    array.as_any().downcast_ref::<T>().ok_or_else(|| {
        corrupt(format!(
            "Parquet column \"{column}\" has an unexpected type"
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::sync::Arc;

    use crabgresql_storage_api::TableAm;
    use crabgresql_txn::CommandId;
    use crabgresql_types::{PgType, Value};
    use crabgresql_wal::Wal;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    use crate::test_support::{finish, manager, open_table, parquet_files, schema};

    /// The bytes in a fragment are in **Arrow's** epoch, not PostgreSQL's.
    ///
    /// A fragment is a persisted format, so this is a compatibility boundary
    /// rather than an internal detail: fragments written before the conversion
    /// moved out of the per-row decode must still read back correctly. Every
    /// other temporal test round-trips through both directions at once and would
    /// pass just as happily if the shift were dropped or inverted on both sides,
    /// so this is the only place the actual on-disk value is pinned.
    #[test]
    fn a_fragment_stores_temporal_columns_in_the_unix_epoch() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("temporal", &[PgType::Date, PgType::Timestamp]),
            Arc::clone(&wal),
        )?;

        // 2000-01-01, PostgreSQL's epoch: day 0 and microsecond 0 to us, and
        // exactly PG_UNIX_EPOCH_DAYS / _MICROS to Arrow.
        let xid = tm.allocate_xid();
        table.insert_many(
            vec![
                vec![Value::Date(0), Value::Timestamp(0)],
                vec![Value::Date(i32::MAX), Value::Timestamp(i64::MIN)],
            ],
            &tm.context(xid, CommandId::FIRST),
        )?;
        tm.commit(xid)?;
        finish(&table, xid, true)?;

        let files = parquet_files(dir.path(), 1)?;
        let mut reader =
            ParquetRecordBatchReaderBuilder::try_new(File::open(&files[0])?)?.build()?;
        let batch = reader
            .next()
            .ok_or_else(|| anyhow::anyhow!("fragment has no batch"))??;

        let dates = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow_array::Date32Array>()
            .ok_or_else(|| anyhow::anyhow!("column 0 is not a Date32"))?;
        let stamps = batch
            .column(1)
            .as_any()
            .downcast_ref::<arrow_array::TimestampMicrosecondArray>()
            .ok_or_else(|| anyhow::anyhow!("column 1 is not a TimestampMicrosecond"))?;

        assert_eq!(dates.value(0), super::PG_UNIX_EPOCH_DAYS);
        assert_eq!(stamps.value(0), super::PG_UNIX_EPOCH_MICROS);
        // The ±infinity sentinels are stored verbatim: shifting them would both
        // overflow and turn infinity into an ordinary instant.
        assert_eq!(dates.value(1), i32::MAX);
        assert_eq!(stamps.value(1), i64::MIN);
        Ok(())
    }
}
