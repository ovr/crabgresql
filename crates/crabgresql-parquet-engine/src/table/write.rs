//! Turning a statement's rows into fragments.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use arrow_array::RecordBatch;
use crabgresql_storage_api::arrow::build_batch;
use crabgresql_storage_api::sort::{sort_permutation, sortable_layout, take_batch};
use crabgresql_storage_api::{MAX_PHYSICAL_BLOCK, StorageError, Tid, Tuple};
use crabgresql_txn::TxnContext;
use parquet::arrow::arrow_writer::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::metadata::{KeyValue, SortingColumn};
use parquet::file::properties::WriterProperties;

use crate::epoch::to_file_epoch;
use crate::error::io_error;
use crate::fragment::{
    FORMAT_VERSION, MAX_FRAGMENT_ROWS, META_CMIN, META_REL, META_SCHEMA, META_VERSION, META_XMIN,
    fragment_base, sync_dir,
};
use crate::schema::{schema_identity, sorting_columns};
use crate::table::ParquetTable;
use crate::wal::{PARQUET_XID_OBSERVED, RMGR_PARQUET};

impl ParquetTable {
    /// Write one fragment into `dir` and fsync it, returning its `.tmp` path and
    /// the `.pending` name the caller renames it to.
    ///
    /// `rel` must be the relfilenode that names `dir` (invariant P1) — it is
    /// stamped into the footer and re-checked on every later read, so a
    /// post-TRUNCATE insert has to carry the *staged* directory's id.
    ///
    /// `sorting` is `Some` exactly when `batch`'s rows are in the relation's
    /// layout sort key order, and is what puts that on the record, in Parquet's
    /// own row-group metadata — see [`sorting_columns`].
    pub(crate) fn write_fragment(
        &self,
        rel: u32,
        dir: &Path,
        block: u32,
        batch: &RecordBatch,
        sorting: Option<&[SortingColumn]>,
        txn: &TxnContext,
    ) -> Result<(PathBuf, PathBuf), StorageError> {
        let base = fragment_base(block, txn);
        let temp = dir.join(format!("{base}.tmp"));
        let pending = dir.join(format!("{base}.parquet.pending"));
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .map_err(|error| io_error("create Parquet fragment", error))?;
        let writer_file = file
            .try_clone()
            .map_err(|error| io_error("clone Parquet fragment handle", error))?;
        let metadata = vec![
            KeyValue::new(META_VERSION.to_string(), Some(FORMAT_VERSION.to_string())),
            KeyValue::new(META_REL.to_string(), Some(rel.to_string())),
            KeyValue::new(META_XMIN.to_string(), Some(txn.xid.0.to_string())),
            KeyValue::new(META_CMIN.to_string(), Some(txn.cid.0.to_string())),
            KeyValue::new(META_SCHEMA.to_string(), Some(schema_identity(&self.schema))),
        ];
        let properties = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .set_max_row_group_row_count(Some(MAX_FRAGMENT_ROWS))
            .set_key_value_metadata(Some(metadata))
            .set_sorting_columns(sorting.map(<[SortingColumn]>::to_vec))
            .build();
        // Arrives in PG semantics, and is shifted here — once, per fragment —
        // into the epoch the file format is defined in (see `crate::epoch::rebase_epoch`).
        // The caller may already have sorted it: `rebase_epoch` adds a constant
        // to every non-sentinel and leaves the ±infinity sentinels at the
        // extremes of Arrow's order, so the shift preserves the order and the
        // sort can happen on either side of it.
        let batch = to_file_epoch(batch)?;
        let mut writer = ArrowWriter::try_new(writer_file, batch.schema(), Some(properties))
            .map_err(|error| io_error("create Parquet writer", error))?;
        writer
            .write(&batch)
            .map_err(|error| io_error("write Parquet fragment", error))?;
        writer
            .close()
            .map_err(|error| io_error("close Parquet fragment", error))?;
        file.sync_all()
            .map_err(|error| io_error("fsync Parquet fragment", error))?;
        Ok((temp, pending))
    }

    pub(super) fn insert_rows(
        &self,
        tuples: Vec<Tuple>,
        txn: &TxnContext,
    ) -> Result<Vec<Tid>, StorageError> {
        if tuples.is_empty() {
            return Ok(Vec::new());
        }
        // A frozen fragment is visible the instant it is fsynced: `header` reports
        // `Xid::FROZEN` for it and `visible_fragments` never looks at the `.pending`
        // suffix. What keeps that from being a dirty read is that the fragment
        // lands in a staged TRUNCATE directory no other session lists — which is
        // the same precondition the server checked before authorizing the freeze.
        // Asserting it here too, where it is actually relied upon, so a caller that
        // widens the freeze fails loudly instead of publishing uncommitted rows
        // into the live directory.
        if txn.freeze_inserts && self.staged_truncate(txn.xid).is_none() {
            return Err(StorageError::UnsupportedOperation(format!(
                "cannot write frozen rows into \"{}\": \
                 this transaction has not truncated it",
                self.schema.name
            )));
        }
        // Shared hold for the whole write: a concurrent TRUNCATE must not swap the
        // directory out from under fragments that are already being written into it.
        let _guard = self.lock.acquire_shared(txn.lock_owner);
        // Record the writer before any file appears on disk, so the finalize
        // hook is guaranteed to reconcile this transaction's fragments even if
        // the write fails partway. A stale entry only costs one directory scan.
        self.staged_xids
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .insert(txn.xid);
        // Resolve the target directory ONCE, before taking `next_block`, and use it
        // for every path below: the block counter, the footer's relfilenode and the
        // fsync all have to describe the same directory (invariant P1).
        let rel = self.effective_rel(txn.xid);
        let dir = self.dir_of(rel);
        let rows = tuples.len();
        let batch = build_batch(&self.schema, &tuples)?;
        // Load-bearing, not tidiness: a `Tuple` is a `Vec<Value>` per row, which
        // outweighs the Arrow image on every schema this engine stores, so
        // releasing it here keeps the sorted path's peak at or under the
        // unsorted one's — the same argument the executor's `SortBatch` makes.
        drop(tuples);
        // Sorting is best-effort by design. A key naming a column Arrow cannot
        // order the way PostgreSQL does (`timetz` and `interval` are structs)
        // leaves the rows in insertion order instead
        // of failing: DDL rejects such a key going forward, but a relation
        // created before that check still has to accept writes, and a flush
        // that failed forever would grow the buffer without bound and surface
        // as backpressure on unrelated inserts. Nothing is lost silently — the
        // row-group sort metadata is written only when the sort actually ran.
        //
        // The permutation and the metadata are decided together, in one `if`:
        // a fragment claiming an order it was not written in is the failure
        // this whole change exists to avoid, and two conditions could drift.
        // Note also that the *whole* insert is permuted, not each fragment —
        // only that makes one write's fragments cover disjoint key ranges.
        let (order, sorting) = if !self.schema.sort_key.is_empty() && sortable_layout(&self.schema)
        {
            (
                Some(sort_permutation(&batch, &self.schema.sort_key)?),
                Some(sorting_columns(&self.schema)?),
            )
        } else {
            (None, None)
        };
        let mut next = self
            .next_block
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let mut staged = Vec::new();
        let mut tids = vec![
            Tid {
                block: 0,
                offset: 0
            };
            rows
        ];
        for start in (0..rows).step_by(MAX_FRAGMENT_ROWS) {
            let len = MAX_FRAGMENT_ROWS.min(rows - start);
            let block = *next;
            // A fragment block is a physical address, so it must stay below the
            // logical-tid flag (see `TID_LOGICAL_FLAG`) — past it, a fragment tid
            // would read as a logical row id and `fetch` would route it wrong.
            *next = next
                .checked_add(1)
                .filter(|next| *next <= MAX_PHYSICAL_BLOCK)
                .ok_or_else(|| io_error("allocate Parquet fragment", "fragment id exhausted"))?;
            // Gathered one fragment at a time rather than taking the whole
            // permutation up front: the sorted copy then never exceeds a
            // fragment, where a whole-batch `take` would hold a second full
            // image of the insert across every compression and fsync below.
            // Same elements, same order — `order` holds global input positions,
            // and `take` is elementwise. The unsorted path slices instead,
            // which is free: an offset and a length over the same buffers.
            //
            // Chained rather than `?`-ed so a failed gather unwinds through the
            // same cleanup as a failed write: either way this transaction's
            // half-written fragments must not survive the error.
            let written = match &order {
                Some(indices) => take_batch(&batch, &indices.slice(start, len)),
                None => Ok(batch.slice(start, len)),
            }
            .and_then(|fragment| {
                self.write_fragment(rel, &dir, block, &fragment, sorting.as_deref(), txn)
            });
            let (temp, pending) = match written {
                Ok(paths) => paths,
                Err(error) => {
                    let base = fragment_base(block, txn);
                    let _ = std::fs::remove_file(dir.join(format!("{base}.tmp")));
                    for (temp, pending) in &staged {
                        let _ = std::fs::remove_file(temp);
                        let _ = std::fs::remove_file(pending);
                    }
                    return Err(error);
                }
            };
            staged.push((temp, pending));
            // A tid is a physical address, so it is assigned in the order rows
            // were written — but the caller indexes the result by *input*
            // position, so the permutation has to be undone here. `order` is a
            // bijection, so every slot is filled exactly once.
            for row in 0..len {
                let input = match &order {
                    Some(indices) => indices.value(start + row) as usize,
                    None => start + row,
                };
                tids[input] = Tid {
                    block,
                    offset: (row + 1) as u16,
                };
            }
        }
        for (temp, pending) in &staged {
            if let Err(error) = std::fs::rename(temp, pending) {
                for (staged_temp, staged_pending) in &staged {
                    let _ = std::fs::remove_file(staged_temp);
                    let _ = std::fs::remove_file(staged_pending);
                }
                let _ = sync_dir(&dir);
                return Err(io_error("publish pending Parquet fragment", error));
            }
        }
        if let Err(error) = sync_dir(&dir) {
            for (temp, pending) in &staged {
                let _ = std::fs::remove_file(temp);
                let _ = std::fs::remove_file(pending);
            }
            let _ = sync_dir(&dir);
            return Err(error);
        }
        let lsn = self
            .wal
            .append(RMGR_PARQUET, PARQUET_XID_OBSERVED, txn.xid, &[])
            .end;
        if let Err(error) = self.wal.flush(lsn) {
            for (temp, pending) in &staged {
                let _ = std::fs::remove_file(temp);
                let _ = std::fs::remove_file(pending);
            }
            let _ = sync_dir(&dir);
            return Err(io_error("flush Parquet XID WAL record", error));
        }
        Ok(tids)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crabgresql_storage_api::{StorageError, TableAm};
    use crabgresql_txn::{CommandId, Xid};
    use crabgresql_types::{PgType, Value};
    use crabgresql_wal::Wal;

    use crate::test_support::{manager, open_table, parquet_files, schema};
    use std::fs::File;

    use crabgresql_storage_api::{IndexKey, Tuple};
    use crabgresql_types::TimeTz;
    use crabgresql_types::numeric::Numeric;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    use crate::test_support::{declared_sort, insert_committed, sorted_schema, stored_rows};

    /// A `numeric` column lands in the Parquet physical type its precision asks
    /// for, which is where the space is won: `INT32` up to 9 digits, `INT64` to
    /// 18, then a fixed-length byte array sized by the precision.
    #[test]
    fn a_numeric_column_lands_in_the_physical_type_its_precision_asks_for() -> anyhow::Result<()> {
        use parquet::basic::Type as Physical;

        for (precision, scale, expected, length) in [
            (9i32, 2i32, Physical::INT32, 0),
            (18, 2, Physical::INT64, 0),
            (38, 2, Physical::FIXED_LEN_BYTE_ARRAY, 16),
            (76, 2, Physical::FIXED_LEN_BYTE_ARRAY, 32),
        ] {
            let dir = tempfile::tempdir()?;
            let wal = Arc::new(Wal::open(dir.path())?);
            let tm = manager(&wal);
            let mut schema = schema("phys", &[PgType::Numeric]);
            schema.columns[0].typmod = Numeric::pack_typmod(precision, scale);
            let table = open_table(dir.path(), 1, schema, Arc::clone(&wal))?;
            insert_committed(
                &table,
                &tm,
                vec![vec![Value::Numeric(
                    Numeric::parse("1.25")?.apply_typmod(precision, scale)?,
                )]],
            )?;

            let file = parquet_files(dir.path(), 1)?
                .pop()
                .ok_or_else(|| anyhow::anyhow!("missing fragment"))?;
            let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(&file)?)?;
            let column = reader.metadata().file_metadata().schema_descr().column(0);
            assert_eq!(
                column.physical_type(),
                expected,
                "numeric({precision},{scale})"
            );
            assert_eq!(
                column.type_length().max(0),
                length,
                "numeric({precision},{scale}) byte length"
            );
        }
        Ok(())
    }

    #[test]
    fn a_fragment_stores_its_rows_in_the_layout_sort_key_order() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = sorted_schema("sorted", &[PgType::Int4, PgType::Text], &[0]);
        let table = open_table(dir.path(), 1, schema.clone(), Arc::clone(&wal))?;

        let rows: Vec<Tuple> = [5, 1, 4, 2, 3]
            .into_iter()
            .map(|n| vec![Value::Int4(n), Value::Text(format!("row{n}"))])
            .collect();
        insert_committed(&table, &tm, rows)?;

        let stored = stored_rows(dir.path(), 1, &schema)?;
        let keys: Vec<Value> = stored.into_iter().map(|row| row[0].clone()).collect();
        assert_eq!(
            keys,
            (1..=5).map(Value::Int4).collect::<Vec<_>>(),
            "the file must hold the rows in key order, not insertion order"
        );

        let file = parquet_files(dir.path(), 1)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing fragment"))?;
        assert_eq!(declared_sort(&file)?, Some(vec![(0, false, false)]));
        Ok(())
    }

    #[test]
    fn a_sorted_insert_returns_tids_that_still_name_their_own_rows() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = sorted_schema("sorted", &[PgType::Int4, PgType::Text], &[0]);
        let table = open_table(dir.path(), 1, schema, Arc::clone(&wal))?;

        // The caller indexes the returned tids by *input* position, so a
        // permutation applied in the wrong direction shows up here and nowhere
        // else: the rows would all be present and all be reachable, just under
        // each other's addresses.
        let rows: Vec<Tuple> = [7, 3, 9, 1, 5, 2]
            .into_iter()
            .map(|n| vec![Value::Int4(n), Value::Text(format!("row{n}"))])
            .collect();
        let tids = insert_committed(&table, &tm, rows.clone())?;

        let reader = tm.context(Xid::INVALID, CommandId::FIRST);
        for (tid, row) in tids.iter().zip(&rows) {
            assert_eq!(table.fetch(*tid, &reader)?.as_ref(), Some(row));
        }
        Ok(())
    }

    #[test]
    fn a_sorted_insert_spanning_fragments_does_not_interleave_them() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = sorted_schema("wide", &[PgType::Int4], &[0]);
        let table = open_table(dir.path(), 1, schema.clone(), Arc::clone(&wal))?;

        // Descending input across the fragment boundary: sorting each chunk on
        // its own would produce two sorted files whose ranges cover everything,
        // which prunes exactly as badly as no sort at all.
        let rows: Vec<Tuple> = (0..super::MAX_FRAGMENT_ROWS as i32 + 100)
            .rev()
            .map(|n| vec![Value::Int4(n)])
            .collect();
        let tids = insert_committed(&table, &tm, rows.clone())?;

        let files = parquet_files(dir.path(), 1)?;
        assert_eq!(files.len(), 2);
        let mut previous_max: Option<i32> = None;
        for file in &files {
            let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(file)?)?.build()?;
            let mut min = i32::MAX;
            let mut max = i32::MIN;
            for batch in reader {
                let batch = batch?;
                let values = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<arrow_array::Int32Array>()
                    .ok_or_else(|| anyhow::anyhow!("column 0 is not an Int32"))?;
                min = min.min(values.iter().flatten().min().unwrap_or(i32::MAX));
                max = max.max(values.iter().flatten().max().unwrap_or(i32::MIN));
            }
            if let Some(previous) = previous_max {
                assert!(previous <= min, "fragment key ranges overlap");
            }
            previous_max = Some(max);
        }

        // The tid permutation has to survive the fragment split too.
        let reader = tm.context(Xid::INVALID, CommandId::FIRST);
        for index in [0, 1, super::MAX_FRAGMENT_ROWS - 1, rows.len() - 1] {
            assert_eq!(
                table.fetch(tids[index], &reader)?.as_ref(),
                Some(&rows[index])
            );
        }
        Ok(())
    }

    #[test]
    fn a_relation_with_no_sort_key_keeps_insertion_order() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = schema("unsorted", &[PgType::Int4]);
        let table = open_table(dir.path(), 1, schema.clone(), Arc::clone(&wal))?;

        let rows: Vec<Tuple> = [5, 1, 4]
            .into_iter()
            .map(|n| vec![Value::Int4(n)])
            .collect();
        insert_committed(&table, &tm, rows.clone())?;

        assert_eq!(stored_rows(dir.path(), 1, &schema)?, rows);
        let file = parquet_files(dir.path(), 1)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing fragment"))?;
        assert_eq!(declared_sort(&file)?, None);
        Ok(())
    }

    #[test]
    fn an_unsortable_sort_key_writes_unsorted_rather_than_failing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        // `interval` maps to a `Struct`, which no ordering kernel accepts, and
        // its PostgreSQL order is by canonical span rather than field by field
        // anyway. DDL rejects such a key today, but a relation created before
        // that check still has to accept writes — unsorted, and saying so.
        let schema = sorted_schema("legacy", &[PgType::Interval], &[0]);
        let table = open_table(dir.path(), 1, schema.clone(), Arc::clone(&wal))?;

        let rows: Vec<Tuple> = [2, 1, 3]
            .into_iter()
            .map(|days| {
                vec![Value::Interval(crabgresql_types::Interval {
                    months: 0,
                    days,
                    usec: 0,
                })]
            })
            .collect();
        insert_committed(&table, &tm, rows.clone())?;

        assert_eq!(stored_rows(dir.path(), 1, &schema)?, rows);
        let file = parquet_files(dir.path(), 1)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing fragment"))?;
        assert_eq!(
            declared_sort(&file)?,
            None,
            "an unsorted fragment must not claim to be clustered"
        );
        Ok(())
    }

    /// A `numeric` sort key orders **numerically** — the order below is the one
    /// a text encoding gets wrong, since `"10"` and `"100"` sort below `"9"`.
    #[test]
    fn a_numeric_sort_key_orders_numerically() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let mut schema = sorted_schema("nums", &[PgType::Numeric], &[0]);
        schema.columns[0].typmod = Numeric::pack_typmod(10, 2);
        let table = open_table(dir.path(), 1, schema.clone(), Arc::clone(&wal))?;

        let numeric = |n: &str| -> anyhow::Result<Tuple> {
            Ok(vec![Value::Numeric(
                Numeric::parse(n)?.apply_typmod(10, 2)?,
            )])
        };
        insert_committed(
            &table,
            &tm,
            vec![numeric("10")?, numeric("9")?, numeric("100")?],
        )?;

        assert_eq!(
            stored_rows(dir.path(), 1, &schema)?,
            vec![numeric("9")?, numeric("10")?, numeric("100")?],
        );
        let file = parquet_files(dir.path(), 1)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing fragment"))?;
        assert_eq!(declared_sort(&file)?, Some(vec![(0, false, false)]));
        Ok(())
    }

    /// NaN is a legal `numeric` in PostgreSQL and has no decimal image, so the
    /// INSERT that wrote it is refused rather than the flush that would have
    /// found it later.
    #[test]
    fn a_nan_is_refused_by_the_insert_not_the_flush() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = schema("nan", &[PgType::Numeric]);
        let table = open_table(dir.path(), 1, schema, Arc::clone(&wal))?;

        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        let error = table
            .insert(vec![Value::Numeric(Numeric::nan())], &txn)
            .expect_err("NaN has no decimal representation");
        assert!(
            matches!(error, StorageError::NumericFieldOverflow { .. }),
            "unexpected error: {error:?}"
        );
        // Nothing was staged: the refusal happened before any file appeared.
        assert!(parquet_files(dir.path(), 1)?.is_empty());
        Ok(())
    }

    #[test]
    fn a_sorted_fragment_records_its_sorting_columns() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        // `timetz` maps to a two-field `Struct`, so it owns two *leaf*
        // descriptors: the key at column 1 is leaf 2, not leaf 1. A schema of
        // scalars alone would pass with the naive `column_idx = key.column`.
        let schema = sorted_schema("leaves", &[PgType::TimeTz, PgType::Int4], &[1]);
        let table = open_table(dir.path(), 1, schema, Arc::clone(&wal))?;

        let rows: Vec<Tuple> = [2, 1]
            .into_iter()
            .map(|n| {
                vec![
                    Value::TimeTz(TimeTz {
                        usec: 1,
                        zone: 3_600,
                    }),
                    Value::Int4(n),
                ]
            })
            .collect();
        insert_committed(&table, &tm, rows)?;

        let file = parquet_files(dir.path(), 1)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing fragment"))?;
        assert_eq!(declared_sort(&file)?, Some(vec![(2, false, false)]));
        Ok(())
    }

    #[test]
    fn a_descending_nulls_first_key_is_honored() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        // Built by hand: the layout `ORDER BY` clause parses bare expressions,
        // so DDL cannot spell a direction, but the key is persisted with both
        // flags and the write path has to honor whatever it finds.
        // TODO: accept ASC/DESC and NULLS FIRST/LAST in the layout ORDER BY
        // clause, so a descending key is reachable through DDL.
        let mut schema = schema("descending", &[PgType::Int4]);
        schema.sort_key = vec![IndexKey {
            column: 0,
            descending: true,
            nulls_first: true,
        }];
        let table = open_table(dir.path(), 1, schema.clone(), Arc::clone(&wal))?;

        let rows: Vec<Tuple> = vec![
            vec![Value::Int4(1)],
            vec![Value::Null],
            vec![Value::Int4(3)],
        ];
        insert_committed(&table, &tm, rows)?;

        assert_eq!(
            stored_rows(dir.path(), 1, &schema)?,
            vec![
                vec![Value::Null],
                vec![Value::Int4(3)],
                vec![Value::Int4(1)],
            ]
        );
        let file = parquet_files(dir.path(), 1)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing fragment"))?;
        assert_eq!(declared_sort(&file)?, Some(vec![(0, true, true)]));
        Ok(())
    }

    #[test]
    fn a_sorted_fragment_orders_floats_as_postgresql_does() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = sorted_schema("floats", &[PgType::Float8, PgType::Int4], &[0]);
        let table = open_table(dir.path(), 1, schema.clone(), Arc::clone(&wal))?;

        // `-0.0` ties with `0.0` and the two NaN bit patterns tie with each
        // other, so the stability tiebreak decides both — that the *write* path
        // reaches the shared canonicalization is what this pins down.
        let other_nan = f64::from_bits(f64::NAN.to_bits() | 1);
        let rows: Vec<Tuple> = [other_nan, 0.0, f64::NAN, -0.0, -1.0]
            .into_iter()
            .enumerate()
            .map(|(index, value)| vec![Value::Float8(value), Value::Int4(index as i32)])
            .collect();
        insert_committed(&table, &tm, rows)?;

        let tags: Vec<Value> = stored_rows(dir.path(), 1, &schema)?
            .into_iter()
            .map(|row| row[1].clone())
            .collect();
        assert_eq!(
            tags,
            [4, 1, 3, 0, 2].map(Value::Int4).to_vec(),
            "-1.0 < (0.0, -0.0 in input order) < (NaN, NaN in input order)"
        );

        // The fragment still declares itself sorted, and that declaration is
        // honest under the IEEE comparison Parquet defines for DOUBLE: the two
        // zeros compare equal and NaN's placement is left undefined. Only a
        // reader using Arrow's *total* order would call `+0.0, -0.0` a descent,
        // which is why `sorting_columns`' doc spells the caveat out.
        let file = parquet_files(dir.path(), 1)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing fragment"))?;
        assert_eq!(declared_sort(&file)?, Some(vec![(0, false, false)]));
        Ok(())
    }

    #[test]
    fn a_char_key_sorts_by_its_unsigned_byte() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        // `"char"` is stored as `UInt8` for exactly this reason: a high-bit byte
        // must sort *above* an ASCII one, as PostgreSQL's unsigned comparison
        // says, and would sort below it under a signed encoding.
        let schema = sorted_schema("chars", &[PgType::Char], &[0]);
        let table = open_table(dir.path(), 1, schema.clone(), Arc::clone(&wal))?;

        let rows: Vec<Tuple> = [0xFF, 0x41, 0x00, 0x80]
            .into_iter()
            .map(|byte| vec![Value::Char(byte)])
            .collect();
        insert_committed(&table, &tm, rows)?;

        assert_eq!(
            stored_rows(dir.path(), 1, &schema)?,
            [0x00, 0x41, 0x80, 0xFF]
                .into_iter()
                .map(|byte| vec![Value::Char(byte)])
                .collect::<Vec<_>>()
        );
        Ok(())
    }
}
