//! The two iterators a scan of a fragment set hands up: rows with their tids,
//! and the batches those rows were shredded from.

use std::sync::Arc;

use arrow_array::RecordBatch;
use crabgresql_storage_api::arrow::decode_row;
use crabgresql_storage_api::{ColumnProjection, StorageError, TableSchema, Tid, Tuple};
use crabgresql_txn::SharedGuard;
use parquet::arrow::arrow_reader::ParquetRecordBatchReader;

use crate::epoch::from_file_epoch;
use crate::error::corrupt;
use crate::fragment::{Fragment, open_reader};

/// The columnar twin of [`ParquetScan`]: the same fragments, in the same order,
/// handed up as batches instead of shredded into rows.
///
/// This is the whole point of the batch path — [`ParquetScan`] holds exactly
/// this `RecordBatch` and takes it apart one cell at a time, so a vectorized
/// consumer would otherwise pay to rebuild what the reader already had.
///
/// No [`Tid`] accompanies a batch, which is why this cannot replace the row
/// scan: `fetch` addresses rows by their ordinal *within a scan*, and DML needs
/// that identity.
pub(crate) struct ParquetBatchScan {
    pub(crate) schema: Arc<TableSchema>,
    /// The relation's `scan_schema`, built once. Every batch is stamped with it,
    /// and deriving it per batch would rebuild a `Field` and a metadata map per
    /// column — on a hundred-column relation that dwarfs the widening.
    pub(crate) stamp: Arc<arrow_schema::Schema>,
    pub(crate) rel: u32,
    pub(crate) projection: ColumnProjection,
    pub(crate) fragments: Vec<Fragment>,
    pub(crate) fragment_index: usize,
    pub(crate) reader: Option<ParquetRecordBatchReader>,
    /// Batch column → schema ordinal for the fragment currently open, rebuilt
    /// each time `reader` is replaced.
    pub(crate) positions: Arc<[usize]>,
    /// As in [`ParquetScan`]: held for the whole iterator life so a concurrent
    /// TRUNCATE cannot remove the directory being read.
    pub(crate) _guard: SharedGuard,
}

impl Iterator for ParquetBatchScan {
    type Item = Result<RecordBatch, StorageError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(reader) = &mut self.reader {
                match reader.next() {
                    Some(Ok(batch)) => {
                        // Two conversions, both once per batch rather than once
                        // per row: out of the file's epoch, and back to the
                        // table's full width.
                        return Some(from_file_epoch(&batch).and_then(|batch| {
                            crabgresql_storage_api::arrow::widen(
                                &self.schema,
                                &self.stamp,
                                &self.positions,
                                &batch,
                            )
                        }));
                    }
                    Some(Err(error)) => {
                        self.reader = None;
                        return Some(Err(corrupt(format!("decode Parquet row group: {error}"))));
                    }
                    None => self.reader = None,
                }
            }
            let fragment = self.fragments.get(self.fragment_index)?;
            self.fragment_index += 1;
            match open_reader(&self.schema, self.rel, fragment, &self.projection) {
                Ok((reader, positions)) => {
                    self.reader = Some(reader);
                    self.positions = positions;
                }
                Err(error) => return Some(Err(error)),
            }
        }
    }
}

pub(crate) struct ParquetScan {
    pub(crate) schema: Arc<TableSchema>,
    pub(crate) rel: u32,
    /// The columns to read off disk. Prunes work only: a mask never changes how
    /// many rows a fragment yields or the order they arrive in, which is what
    /// keeps the `Tid` ordinals below stable and `fetch` able to find them.
    pub(crate) projection: ColumnProjection,
    pub(crate) fragments: Vec<Fragment>,
    pub(crate) fragment_index: usize,
    pub(crate) reader: Option<ParquetRecordBatchReader>,
    /// Batch column → schema ordinal for the fragment currently open, rebuilt
    /// each time `reader` is replaced.
    pub(crate) positions: Arc<[usize]>,
    pub(crate) batch: Option<RecordBatch>,
    pub(crate) batch_row: usize,
    pub(crate) file_row: u32,
    pub(crate) current_block: u32,
    /// Keeps the shared hold for the whole iterator life, so a concurrent
    /// TRUNCATE cannot remove the directory this scan is still reading.
    pub(crate) _guard: SharedGuard,
}

impl Iterator for ParquetScan {
    type Item = Result<(Tid, Tuple), StorageError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(batch) = &self.batch
                && self.batch_row < batch.num_rows()
            {
                let row = self.batch_row;
                self.batch_row += 1;
                let offset = self.file_row + 1;
                self.file_row += 1;
                if offset > u16::MAX as u32 {
                    return Some(Err(corrupt("Parquet fragment exceeds the TID row limit")));
                }
                return Some(
                    decode_row(&self.schema, &self.positions, batch, row).map(|tuple| {
                        (
                            Tid {
                                block: self.current_block,
                                offset: offset as u16,
                            },
                            tuple,
                        )
                    }),
                );
            }
            self.batch = None;
            if let Some(reader) = &mut self.reader {
                match reader.next() {
                    // One vectorized rebase per batch, at the file boundary and
                    // nowhere else — see `crate::epoch::rebase_epoch`.
                    Some(Ok(batch)) => match from_file_epoch(&batch) {
                        Ok(batch) => {
                            self.batch = Some(batch);
                            self.batch_row = 0;
                            continue;
                        }
                        Err(error) => {
                            self.reader = None;
                            return Some(Err(error));
                        }
                    },
                    Some(Err(error)) => {
                        self.reader = None;
                        return Some(Err(corrupt(format!("decode Parquet row group: {error}"))));
                    }
                    None => self.reader = None,
                }
            }
            let fragment = self.fragments.get(self.fragment_index)?.clone();
            self.fragment_index += 1;
            self.current_block = fragment.block;
            self.file_row = 0;
            match open_reader(&self.schema, self.rel, &fragment, &self.projection) {
                Ok((reader, positions)) => {
                    self.reader = Some(reader);
                    self.positions = positions;
                }
                Err(error) => return Some(Err(error)),
            }
        }
    }
}
