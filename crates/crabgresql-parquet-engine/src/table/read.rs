//! Reading a transaction's visible fragments, as rows or as batches.

use std::sync::Arc;

use crabgresql_storage_api::arrow::decode_row;
use crabgresql_storage_api::{ColumnProjection, StorageError, Tid, Tuple};
use crabgresql_txn::{SharedGuard, TxnContext, satisfies_mvcc};

use crate::epoch::from_file_epoch;
use crate::error::corrupt;
use crate::fragment::{Fragment, fragments, header, open_reader};
use crate::scan::{ParquetBatchScan, ParquetScan};
use crate::table::ParquetTable;

impl ParquetTable {
    /// Build a scan holding a shared lock for the whole iterator life, so a
    /// concurrent TRUNCATE cannot remove the directory it is still reading.
    ///
    /// The relfilenode is resolved **after** the guard is granted, together with
    /// the fragment listing, and then carried in the iterator. Reading it before
    /// the guard would break invariant P1: `acquire_shared` can block for the
    /// lifetime of a concurrent TRUNCATE's transaction, and the swap that
    /// transaction commits while we wait would leave a pre-lock id describing a
    /// directory that no longer exists — reporting the new directory's perfectly
    /// good fragments as corrupt.
    pub(crate) fn scan_in(
        &self,
        txn: &TxnContext,
        projection: &ColumnProjection,
    ) -> Result<ParquetScan, StorageError> {
        let guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        let fragments = self.visible_fragments(rel, txn)?;
        Ok(self.scan_over(rel, fragments, guard, projection))
    }

    /// The batch-shaped twin of [`ParquetTable::scan_in`], listing the same
    /// fragments under the same shared hold.
    pub(crate) fn batch_scan_in(
        &self,
        txn: &TxnContext,
        projection: &ColumnProjection,
    ) -> Result<ParquetBatchScan, StorageError> {
        let guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        Ok(ParquetBatchScan {
            stamp: crabgresql_storage_api::arrow::scan_schema(&self.schema),
            schema: Arc::clone(&self.schema),
            rel,
            projection: projection.clone(),
            fragments: self.visible_fragments(rel, txn)?,
            fragment_index: 0,
            reader: None,
            positions: Arc::from(Vec::new()),
            _guard: guard,
        })
    }

    /// Scan an already-listed fragment set, taking over the caller's shared hold.
    /// Lets a caller that has both measured and listed (see [`ParquetTable::measure`])
    /// read exactly what it measured.
    pub(crate) fn scan_over(
        &self,
        rel: u32,
        fragments: Vec<Fragment>,
        guard: SharedGuard,
        projection: &ColumnProjection,
    ) -> ParquetScan {
        ParquetScan {
            schema: Arc::clone(&self.schema),
            rel,
            projection: projection.clone(),
            fragments,
            fragment_index: 0,
            reader: None,
            positions: Arc::from(Vec::new()),
            batch: None,
            batch_row: 0,
            file_row: 0,
            current_block: 0,
            _guard: guard,
        }
    }

    /// The fragments of `rel`'s directory visible to `txn`. `rel` is passed in
    /// rather than re-derived so the caller's id and the listed directory are
    /// guaranteed to be the same generation (invariant P1).
    pub(crate) fn visible_fragments(
        &self,
        rel: u32,
        txn: &TxnContext,
    ) -> Result<Vec<Fragment>, StorageError> {
        Ok(fragments(&self.dir_of(rel))?
            .into_iter()
            .filter(|fragment| {
                satisfies_mvcc(
                    &header(fragment),
                    &txn.snapshot,
                    &txn.clog,
                    txn.xid,
                    txn.cid,
                )
            })
            .collect())
    }

    pub(super) fn fetch_in(
        &self,
        tid: Tid,
        txn: &TxnContext,
    ) -> Result<Option<Tuple>, StorageError> {
        let _guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        let Some(fragment) = self
            .visible_fragments(rel, txn)?
            .into_iter()
            .find(|fragment| fragment.block == tid.block)
        else {
            return Ok(None);
        };
        // Always unprojected: `fetch` serves EvalPlanQual re-reads and index
        // point lookups, both of which need the whole row.
        let (mut reader, positions) =
            open_reader(&self.schema, rel, &fragment, &ColumnProjection::All)?;
        let mut ordinal = 1u32;
        for batch in &mut reader {
            let batch =
                batch.map_err(|error| corrupt(format!("decode Parquet row group: {error}")))?;
            for row in 0..batch.num_rows() {
                if ordinal == tid.offset as u32 {
                    // Sliced first: this is a point lookup, so rebasing the
                    // whole row group to return one tuple would scale the cost
                    // of a `fetch` with the fragment rather than with the row.
                    let one = from_file_epoch(&batch.slice(row, 1))?;
                    return decode_row(&self.schema, &positions, &one, 0).map(Some);
                }
                ordinal += 1;
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crabgresql_storage_api::{StorageError, TableAm};
    use crabgresql_txn::{CommandId, Xid};
    use crabgresql_types::numeric::Numeric;
    use crabgresql_types::{Interval, PgType, TimeTz, Value};
    use crabgresql_wal::Wal;

    use crate::test_support::{finish, manager, open_table, parquet_files, schema};
    use std::fs::OpenOptions;

    use crabgresql_storage_api::{ColumnProjection, Tid, Tuple};

    use crate::test_support::{batch_scan_rows, struct_mixed_table};

    #[test]
    fn truncated_fragment_is_reported_as_corrupt_storage() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("corrupt", &[PgType::Int4]), wal)?;
        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        finish(&table, xid, true)?;
        let file = parquet_files(dir.path(), 1)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing committed fragment"))?;
        OpenOptions::new().write(true).open(file)?.set_len(10)?;

        let error = table
            .scan(
                &tm.context(Xid::INVALID, CommandId::FIRST),
                &ColumnProjection::All,
            )
            .next()
            .ok_or_else(|| anyhow::anyhow!("corrupt scan returned no item"))?
            .expect_err("truncated fragment must return an error");
        assert!(matches!(error, StorageError::CorruptData(_)));
        Ok(())
    }

    /// Every column outside the projection reads back as `Null`, every one
    /// inside keeps its real value, and the tuple stays as wide as the schema.
    ///
    /// Projecting *around* a `Struct` column (index 1 here) is the part that
    /// would break under a naive positional decode: the batch is dense over the
    /// selected columns, so batch position 1 is schema position 2.
    #[test]
    fn a_projected_scan_fills_only_the_selected_columns() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let (table, row) = struct_mixed_table(dir.path(), Arc::clone(&wal))?;

        let xid = tm.allocate_xid();
        table.insert(row.clone(), &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        finish(&table, xid, true)?;

        let reader = tm.context(Xid::INVALID, CommandId::FIRST);
        let projected: Vec<Tuple> = table
            .scan(&reader, &ColumnProjection::of([0, 2], &table.schema()))
            .map(|result| result.map(|(_, tuple)| tuple))
            .collect::<Result<_, _>>()?;

        assert_eq!(
            projected,
            vec![vec![
                row[0].clone(),
                Value::Null,
                row[2].clone(),
                Value::Null,
                Value::Null,
            ]]
        );
        Ok(())
    }

    /// `timetz` and `interval` map to an arrow `Struct`, so they occupy several
    /// leaf descriptors under a single root. Building the mask from *leaf*
    /// indices would select the wrong columns; this pins `roots`.
    #[test]
    fn a_projection_of_only_struct_columns_decodes_them() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let (table, row) = struct_mixed_table(dir.path(), Arc::clone(&wal))?;

        let xid = tm.allocate_xid();
        table.insert(row.clone(), &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        finish(&table, xid, true)?;

        let reader = tm.context(Xid::INVALID, CommandId::FIRST);
        for column in [1usize, 3] {
            let rows: Vec<Tuple> = table
                .scan(&reader, &ColumnProjection::of([column], &table.schema()))
                .map(|result| result.map(|(_, tuple)| tuple))
                .collect::<Result<_, _>>()?;
            let mut want = vec![Value::Null; row.len()];
            want[column] = row[column].clone();
            assert_eq!(rows, vec![want], "projecting only column {column}");
        }
        Ok(())
    }

    /// The batch scan and the row scan are the same scan. Every supported type
    /// is present, so this is also where a temporal column that forgot to leave
    /// the file's epoch shows up — a `Date` would come back shifted by
    /// `PG_UNIX_EPOCH_DAYS` and the comparison against the row scan would fail.
    #[test]
    fn a_batch_scan_yields_exactly_what_the_row_scan_does() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = schema(
            "types",
            &[
                PgType::Bool,
                PgType::Int4,
                PgType::Numeric,
                PgType::Text,
                PgType::Uuid,
                PgType::Date,
                PgType::Time,
                PgType::TimeTz,
                PgType::Timestamp,
                PgType::TimestampTz,
                PgType::Interval,
            ],
        );
        let table = open_table(dir.path(), 1, schema, Arc::clone(&wal))?;
        let row = |n: i32| {
            Ok::<Tuple, anyhow::Error>(vec![
                Value::Bool(n % 2 == 0),
                Value::Int4(n),
                Value::Numeric(Numeric::parse("1234567890.012300")?),
                Value::Text(format!("row {n}")),
                Value::Uuid([n as u8; 16]),
                // Either side of both epochs, plus the ±infinity sentinels.
                Value::Date(n * 4_000 - 8_000),
                Value::Time(12_345_678),
                Value::TimeTz(TimeTz {
                    usec: 45_000_000,
                    zone: 3_600,
                }),
                Value::Timestamp(i64::from(n) * 1_000_000_000 - 2_000_000_000),
                Value::TimestampTz(-987_654_321),
                Value::Interval(Interval {
                    months: 14,
                    days: -3,
                    usec: 777,
                }),
            ])
        };
        let mut expected: Vec<Tuple> = (0..4).map(row).collect::<Result<_, _>>()?;
        // The sentinels are ordinary bit patterns to Arrow; a rebase that did
        // not exempt them would overflow or turn infinity into a date.
        let mut infinities = row(0)?;
        infinities[5] = Value::Date(i32::MAX);
        infinities[8] = Value::Timestamp(i64::MIN);
        expected.push(infinities);
        expected.push(vec![Value::Null; 11]);

        // Two fragments, so the batch scan has to cross a fragment boundary.
        for chunk in expected.chunks(3) {
            let xid = tm.allocate_xid();
            table.insert_many(chunk.to_vec(), &tm.context(xid, CommandId::FIRST))?;
            tm.commit(xid)?;
            finish(&table, xid, true)?;
        }

        let reader = tm.context(Xid::INVALID, CommandId::FIRST);
        let row_scan: Vec<Tuple> = table
            .scan(&reader, &ColumnProjection::All)
            .map(|result| result.map(|(_, tuple)| tuple))
            .collect::<Result<_, _>>()?;
        assert_eq!(row_scan, expected);
        assert_eq!(
            batch_scan_rows(&table, &reader, &ColumnProjection::All)?,
            expected
        );
        Ok(())
    }

    /// A projected batch comes back at full width, with the skipped columns
    /// NULL — the same contract the row scan has, so the two still agree.
    #[test]
    fn a_projected_batch_scan_stays_full_width() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let (table, row) = struct_mixed_table(dir.path(), Arc::clone(&wal))?;

        let xid = tm.allocate_xid();
        table.insert_many(vec![row.clone()], &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        finish(&table, xid, true)?;

        let reader = tm.context(Xid::INVALID, CommandId::FIRST);
        // Project around the `Interval` struct column, the case a naive
        // positional decode gets wrong.
        let projection = ColumnProjection::of([2], &table.schema());
        let batched = batch_scan_rows(&table, &reader, &projection)?;
        let scanned: Vec<Tuple> = table
            .scan(&reader, &projection)
            .map(|result| result.map(|(_, tuple)| tuple))
            .collect::<Result<_, _>>()?;

        assert_eq!(batched.len(), 1);
        assert_eq!(
            batched[0].len(),
            row.len(),
            "width is the schema's, not the projection's"
        );
        assert_eq!(batched[0][2], row[2]);
        assert_eq!(batched, scanned);
        Ok(())
    }

    /// A mask prunes columns, never rows — so the tid sequence, and `fetch`'s
    /// ability to find a row by it, must be identical to an unprojected scan.
    /// Spans several fragments, since the ordinal restarts within each.
    #[test]
    fn a_projected_scan_yields_the_same_tids_as_a_full_one() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let (table, row) = struct_mixed_table(dir.path(), Arc::clone(&wal))?;

        // Three fragments, two rows each: one `insert_many` per transaction.
        for _ in 0..3 {
            let xid = tm.allocate_xid();
            table.insert_many(
                vec![row.clone(), row.clone()],
                &tm.context(xid, CommandId::FIRST),
            )?;
            tm.commit(xid)?;
            finish(&table, xid, true)?;
        }

        let reader = tm.context(Xid::INVALID, CommandId::FIRST);
        let tids = |projection: &ColumnProjection| -> Result<Vec<Tid>, StorageError> {
            table
                .scan(&reader, projection)
                .map(|result| result.map(|(tid, _)| tid))
                .collect()
        };
        let full = tids(&ColumnProjection::All)?;
        assert_eq!(full.len(), 6);
        assert_eq!(tids(&ColumnProjection::of([2], &table.schema()))?, full);
        // The empty set is the `count(*)` shape, normalized to one column.
        assert_eq!(tids(&ColumnProjection::of([], &table.schema()))?, full);

        // `fetch` still resolves each tid to the whole row.
        for tid in full {
            assert_eq!(table.fetch(tid, &reader)?, Some(row.clone()));
        }
        Ok(())
    }
}
