//! Fixtures shared by the modules' `#[cfg(test)] mod tests` blocks.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crabgresql_storage_api::arrow::decode_row;
use crabgresql_storage_api::{
    Column, ColumnProjection, IndexKey, RelfilenodeAllocator, StorageError, TableAccessMethod,
    TableAm, TableSchema, Tid, Tuple,
};
use crabgresql_txn::{Clog, CommandId, CommitSink, TransactionManager, TxnContext, Xid};
use crabgresql_types::{Interval, PgType, TimeTz, Value};
use crabgresql_wal::Wal;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use crate::ParquetTable;
use crate::epoch::from_file_epoch;
use crate::error::corrupt;

/// A relfilenode counter for tests. It starts far above the ids the tests
/// assign by hand, so a directory staged by a TRUNCATE can never collide with
/// one of them.
pub(crate) struct Counter(std::sync::atomic::AtomicU32);

impl RelfilenodeAllocator for Counter {
    fn alloc_relfilenode(&self) -> u32 {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

pub(crate) fn open_table(
    dir: &Path,
    rel: u32,
    schema: TableSchema,
    wal: Arc<Wal>,
) -> Result<ParquetTable, StorageError> {
    ParquetTable::open(
        dir,
        rel,
        schema,
        Vec::new(),
        wal,
        Arc::new(Counter(std::sync::atomic::AtomicU32::new(1_000))),
    )
}

/// End a transaction the way the engine's finalize hook does: reconcile, then
/// release the hold a committed TRUNCATE handed back. Returns the swapped-in
/// relfilenode, if any.
pub(crate) fn finish(
    table: &ParquetTable,
    xid: Xid,
    committed: bool,
) -> Result<Option<u32>, StorageError> {
    let swap = table.finish_transaction(xid, committed)?;
    if let Some(swap) = swap {
        table.release_truncate_lock(swap.owner);
    }
    Ok(swap.map(|swap| swap.new_rel))
}

pub(crate) fn manager(wal: &Arc<Wal>) -> TransactionManager {
    let sink: Arc<dyn CommitSink> = Arc::clone(wal) as Arc<dyn CommitSink>;
    TransactionManager::new_recovered(sink, Arc::new(Clog::new()), Xid::FIRST_NORMAL)
}

pub(crate) fn schema(name: &str, types: &[PgType]) -> TableSchema {
    let mut schema = TableSchema::new(
        name,
        types
            .iter()
            .enumerate()
            .map(|(index, ty)| Column::new(format!("c{index}"), *ty))
            .collect(),
    );
    schema.access_method = TableAccessMethod::Parquet;
    schema
}

pub(crate) fn parquet_files(dir: &Path, rel: u32) -> anyhow::Result<Vec<PathBuf>> {
    let table_dir = dir.join("parquet").join(rel.to_string());
    let mut files = Vec::new();
    for entry in std::fs::read_dir(table_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("parquet") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Every `parquet/<n>` directory currently on disk, as relfilenodes.
pub(crate) fn fragment_dirs(dir: &Path) -> anyhow::Result<Vec<u32>> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(dir.join("parquet"))? {
        if let Some(rel) = entry?
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        {
            dirs.push(rel);
        }
    }
    dirs.sort_unstable();
    Ok(dirs)
}

pub(crate) fn scan_values(table: &ParquetTable, txn: &crabgresql_txn::TxnContext) -> Vec<i32> {
    let mut values: Vec<i32> = table
        .scan(txn, &ColumnProjection::All)
        .map(|row| match row.expect("scan row").1.first() {
            Some(Value::Int4(value)) => *value,
            other => panic!("unexpected value {other:?}"),
        })
        .collect();
    values.sort_unstable();
    values
}

/// A relation whose columns span the two shapes that matter to a
/// `ProjectionMask`: plain scalars, and the `Struct`-backed `timetz` /
/// `interval` that own several *leaf* descriptors under one root.
pub(crate) fn struct_mixed_table(
    dir: &Path,
    wal: Arc<Wal>,
) -> Result<(ParquetTable, Vec<Value>), StorageError> {
    let schema = schema(
        "mixed",
        &[
            PgType::Int4,
            PgType::Interval,
            PgType::Text,
            PgType::TimeTz,
            PgType::Bool,
        ],
    );
    let row = vec![
        Value::Int4(7),
        Value::Interval(Interval {
            months: 14,
            days: -3,
            usec: 777,
        }),
        Value::Text("payload".to_string()),
        Value::TimeTz(TimeTz {
            usec: 45_000_000,
            zone: 3_600,
        }),
        Value::Bool(true),
    ];
    let table = open_table(dir, 1, schema, wal)?;
    Ok((table, row))
}

/// Drain a batch scan back into rows, so it can be compared against the row
/// scan value for value.
pub(crate) fn batch_scan_rows(
    table: &ParquetTable,
    txn: &TxnContext,
    projection: &ColumnProjection,
) -> Result<Vec<Tuple>, StorageError> {
    let schema = table.schema().clone();
    let positions: Vec<usize> = (0..schema.columns.len()).collect();
    let mut rows = Vec::new();
    for batch in table
        .scan_batches(txn, projection)
        .ok_or_else(|| corrupt("engine reported batch support but returned none"))?
    {
        let batch = batch?;
        for row in 0..batch.num_rows() {
            rows.push(crabgresql_storage_api::arrow::decode_row(
                &schema, &positions, &batch, row,
            )?);
        }
    }
    Ok(rows)
}

/// [`schema`] plus a layout sort key over `key`, ascending / NULLS LAST —
/// the shape a bare `ORDER BY (columns)` produces.
pub(crate) fn sorted_schema(name: &str, types: &[PgType], key: &[usize]) -> TableSchema {
    let mut schema = schema(name, types);
    schema.sort_key = key
        .iter()
        .map(|column| IndexKey {
            column: *column,
            descending: false,
            nulls_first: false,
        })
        .collect();
    schema
}

/// Every row of `rel`'s fragments, in the order the files store them.
pub(crate) fn stored_rows(
    dir: &Path,
    rel: u32,
    schema: &TableSchema,
) -> anyhow::Result<Vec<Tuple>> {
    let positions: Vec<usize> = (0..schema.columns.len()).collect();
    let mut rows = Vec::new();
    for file in parquet_files(dir, rel)? {
        let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(&file)?)?.build()?;
        for batch in reader {
            let batch = from_file_epoch(&batch?)?;
            for row in 0..batch.num_rows() {
                rows.push(decode_row(schema, &positions, &batch, row)?);
            }
        }
    }
    Ok(rows)
}

/// The sort key a fragment declares, as `(leaf, descending, nulls_first)`,
/// or `None` when it claims no order at all.
pub(crate) fn declared_sort(path: &Path) -> anyhow::Result<Option<Vec<(i32, bool, bool)>>> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?;
    Ok(reader
        .metadata()
        .row_group(0)
        .sorting_columns()
        .map(|columns| {
            columns
                .iter()
                .map(|column| (column.column_idx, column.descending, column.nulls_first))
                .collect()
        }))
}

/// Insert `rows` in one transaction and commit it.
pub(crate) fn insert_committed(
    table: &ParquetTable,
    tm: &TransactionManager,
    rows: Vec<Tuple>,
) -> anyhow::Result<Vec<Tid>> {
    let xid = tm.allocate_xid();
    let tids = table.insert_many(rows, &tm.context(xid, CommandId::FIRST))?;
    tm.commit(xid)?;
    finish(table, xid, true)?;
    Ok(tids)
}
