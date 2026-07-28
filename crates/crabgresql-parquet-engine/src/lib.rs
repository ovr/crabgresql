//! Managed append-only Parquet table access method.
//!
//! A statement batch is written as one or more immutable fragments. Files remain
//! `.pending` until the transaction commits; the engine's finalize hook promotes
//! them to `.parquet` or removes them on abort. MVCC identity lives in file
//! metadata, leaving the physical Parquet schema composed solely of user columns.
//!
//! Fragments are immutable, so the only supported mutation besides INSERT is
//! TRUNCATE, implemented as the directory-level twin of the heap's
//! relfilenode-swap: the truncating transaction stages a fresh, empty
//! `parquet/<new>/` and reads and writes there, while the old directory stays
//! untouched until commit (it is removed on commit, and the staged one on abort).
//! A `.pending`-style rename cannot express "all rows are gone", and a tombstone
//! that merely hides fragments would neither free the space nor reset `relpages`
//! until a vacuum, which is not what TRUNCATE promises.
//!
//! Known divergence, shared with the heap and inherited from
//! [`crabgresql_txn::TableLock`]'s scope: a reader's/writer's shared hold covers one
//! *operation*, not its whole transaction (PostgreSQL holds `RowExclusiveLock` to
//! end-of-transaction). So a TRUNCATE can commit between two statements of another
//! open transaction and discard fragments that transaction had already staged. The
//! fix is transaction-scoped table locks in the engine, not per-AM bookkeeping.

mod buffered;

pub use buffered::BufferedParquetTable;

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, FixedSizeBinaryBuilder, Float32Builder, Float64Builder,
    Int16Builder, Int32Builder, Int64Builder, StringBuilder, StructBuilder,
    Time64MicrosecondBuilder, TimestampMicrosecondBuilder,
};
use arrow_array::{
    Array, ArrayRef, BinaryArray, BooleanArray, Date32Array, FixedSizeBinaryArray, Float32Array,
    Float64Array, Int16Array, Int32Array, Int64Array, RecordBatch, RecordBatchOptions, StringArray,
    RecordBatchReader, StructArray, Time64MicrosecondArray, TimestampMicrosecondArray,
};
use arrow_schema::{DataType, Field, Fields, Schema, TimeUnit};
use crabgresql_batch::{Batch, BatchField, BatchSchema, epoch};
use crabgresql_storage_api::{
    BatchStream, ColumnProjection, DeleteResult, IndexMetadata, MAX_PHYSICAL_BLOCK, RelStats,
    RelfilenodeAllocator, ScanRequest, StorageError, TableAm, TableCapabilities, TableSchema, Tid,
    Tuple, TupleStream, UpdateResult,
};
use crabgresql_txn::{
    CommandId, Infomask, LockOwner, SharedGuard, TableLock, TupleHeader, TxnContext, Xid,
    satisfies_mvcc,
};
use crabgresql_types::{Interval, PgType, TimeTz, Value};
use crabgresql_wal::{RedoContext, RmgrId, RmgrRedo, Wal, WalError};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{
    ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder,
};
use parquet::arrow::arrow_writer::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

pub const RMGR_PARQUET: RmgrId = RmgrId(12);
pub const PARQUET_XID_OBSERVED: u8 = 1;
/// A directory-swap TRUNCATE: see [`encode_truncate`] for the payload.
pub const PARQUET_TRUNCATE: u8 = 2;

const FORMAT_VERSION: &str = "1";
const MAX_FRAGMENT_ROWS: usize = u16::MAX as usize;
const PG_UNIX_EPOCH_DAYS: i32 = 10_957;
const PG_UNIX_EPOCH_MICROS: i64 = 946_684_800_000_000;

const META_VERSION: &str = "crabgresql.format_version";
const META_REL: &str = "crabgresql.relfilenode";
const META_XMIN: &str = "crabgresql.xmin";
const META_CMIN: &str = "crabgresql.cmin";
const META_SCHEMA: &str = "crabgresql.schema";

fn io_error(context: &str, error: impl std::fmt::Display) -> StorageError {
    StorageError::Io(format!("{context}: {error}"))
}

fn corrupt(context: impl Into<String>) -> StorageError {
    StorageError::CorruptData(context.into())
}

fn unsupported(message: impl Into<String>) -> StorageError {
    StorageError::UnsupportedOperation(message.into())
}

fn schema_identity(schema: &TableSchema) -> String {
    schema
        .columns
        .iter()
        .map(|column| format!("{}:{}:{}", column.name, column.ty.oid(), column.typmod))
        .collect::<Vec<_>>()
        .join("|")
}

pub fn supports_type(ty: PgType) -> bool {
    matches!(
        ty,
        PgType::Bool
            | PgType::Int2
            | PgType::Int4
            | PgType::Int8
            | PgType::Float4
            | PgType::Float8
            | PgType::Numeric
            | PgType::Text
            | PgType::Varchar
            | PgType::Bpchar
            | PgType::Name
            | PgType::Bytea
            | PgType::Uuid
            | PgType::Date
            | PgType::Time
            | PgType::TimeTz
            | PgType::Timestamp
            | PgType::TimestampTz
            | PgType::Interval
    )
}

/// Reject a schema this format cannot represent.
///
/// The error names `schema.access_method` rather than a hard-coded "parquet"
/// because the buffer table shares this whitelist: a row a buffer accepts must
/// always be convertible to a fragment, or a flush would fail long after the
/// `INSERT` that should have been rejected. One whitelist keeps that true, and
/// naming the relation's own method keeps the message honest.
pub fn validate_schema(schema: &TableSchema) -> Result<(), StorageError> {
    if let Some(column) = schema.columns.iter().find(|column| !supports_type(column.ty)) {
        return Err(StorageError::UnsupportedType(format!(
            "data type {} is not supported by table access method \"{}\"",
            column.ty.name(),
            schema.access_method.as_str(),
        )));
    }
    Ok(())
}

fn arrow_type(ty: PgType) -> DataType {
    match ty {
        PgType::Bool => DataType::Boolean,
        PgType::Int2 => DataType::Int16,
        PgType::Int4 => DataType::Int32,
        PgType::Int8 => DataType::Int64,
        PgType::Float4 => DataType::Float32,
        PgType::Float8 => DataType::Float64,
        PgType::Numeric
        | PgType::Text
        | PgType::Varchar
        | PgType::Bpchar
        | PgType::Name => DataType::Utf8,
        PgType::Bytea => DataType::Binary,
        PgType::Uuid => DataType::FixedSizeBinary(16),
        PgType::Date => DataType::Date32,
        PgType::Time => DataType::Time64(TimeUnit::Microsecond),
        PgType::TimeTz => DataType::Struct(Fields::from(vec![
            Field::new("time_us", DataType::Int64, false),
            Field::new("offset_seconds", DataType::Int32, false),
        ])),
        PgType::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
        PgType::TimestampTz => {
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        }
        PgType::Interval => DataType::Struct(Fields::from(vec![
            Field::new("months", DataType::Int32, false),
            Field::new("days", DataType::Int32, false),
            Field::new("micros", DataType::Int64, false),
        ])),
        _ => DataType::Binary,
    }
}

fn arrow_schema(schema: &TableSchema) -> Arc<Schema> {
    Arc::new(Schema::new(
        schema
            .columns
            .iter()
            .map(|column| {
                let metadata = HashMap::from([
                    ("crabgresql.pg_type_oid".to_string(), column.ty.oid().to_string()),
                    ("crabgresql.typmod".to_string(), column.typmod.to_string()),
                ]);
                Field::new(&column.name, arrow_type(column.ty), column.nullable)
                    .with_metadata(metadata)
            })
            .collect::<Vec<_>>(),
    ))
}

fn value_mismatch(column: &str, ty: PgType) -> StorageError {
    corrupt(format!(
        "Parquet conversion for column \"{column}\" expected {}",
        ty.name()
    ))
}

fn build_array(
    column: &crabgresql_storage_api::Column,
    tuples: &[Tuple],
    index: usize,
) -> Result<ArrayRef, StorageError> {
    macro_rules! primitive {
        ($builder:ty, $variant:path) => {{
            let mut builder = <$builder>::with_capacity(tuples.len());
            for tuple in tuples {
                match &tuple[index] {
                    Value::Null => builder.append_null(),
                    $variant(value) => builder.append_value(*value),
                    _ => return Err(value_mismatch(&column.name, column.ty)),
                }
            }
            Ok(Arc::new(builder.finish()) as ArrayRef)
        }};
    }

    match column.ty {
        PgType::Bool => primitive!(BooleanBuilder, Value::Bool),
        PgType::Int2 => primitive!(Int16Builder, Value::Int2),
        PgType::Int4 => primitive!(Int32Builder, Value::Int4),
        PgType::Int8 => primitive!(Int64Builder, Value::Int8),
        PgType::Float4 => primitive!(Float32Builder, Value::Float4),
        PgType::Float8 => primitive!(Float64Builder, Value::Float8),
        PgType::Numeric => {
            let mut builder = StringBuilder::new();
            for tuple in tuples {
                match &tuple[index] {
                    Value::Null => builder.append_null(),
                    Value::Numeric(value) => builder.append_value(value.to_display()),
                    _ => return Err(value_mismatch(&column.name, column.ty)),
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => {
            let mut builder = StringBuilder::new();
            for tuple in tuples {
                match &tuple[index] {
                    Value::Null => builder.append_null(),
                    Value::Text(value) => builder.append_value(value),
                    _ => return Err(value_mismatch(&column.name, column.ty)),
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        PgType::Bytea => {
            let mut builder = BinaryBuilder::new();
            for tuple in tuples {
                match &tuple[index] {
                    Value::Null => builder.append_null(),
                    Value::Bytea(value) => builder.append_value(value),
                    _ => return Err(value_mismatch(&column.name, column.ty)),
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        PgType::Uuid => {
            let mut builder = FixedSizeBinaryBuilder::with_capacity(tuples.len(), 16);
            for tuple in tuples {
                match &tuple[index] {
                    Value::Null => builder.append_null(),
                    Value::Uuid(value) => builder
                        .append_value(value)
                        .map_err(|error| io_error("encode UUID", error))?,
                    _ => return Err(value_mismatch(&column.name, column.ty)),
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        PgType::Date => {
            let mut builder = arrow_array::builder::Date32Builder::with_capacity(tuples.len());
            for tuple in tuples {
                match &tuple[index] {
                    Value::Null => builder.append_null(),
                    Value::Date(value) if *value == i32::MIN || *value == i32::MAX => {
                        builder.append_value(*value)
                    }
                    Value::Date(value) => builder.append_value(
                        value
                            .checked_add(PG_UNIX_EPOCH_DAYS)
                            .ok_or_else(|| corrupt("date epoch conversion overflow"))?,
                    ),
                    _ => return Err(value_mismatch(&column.name, column.ty)),
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        PgType::Time => {
            let mut builder = Time64MicrosecondBuilder::with_capacity(tuples.len());
            for tuple in tuples {
                match &tuple[index] {
                    Value::Null => builder.append_null(),
                    Value::Time(value) => builder.append_value(*value),
                    _ => return Err(value_mismatch(&column.name, column.ty)),
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        PgType::TimeTz => {
            let fields = match arrow_type(PgType::TimeTz) {
                DataType::Struct(fields) => fields,
                _ => return Err(corrupt("invalid timetz Arrow schema")),
            };
            let mut builder = StructBuilder::new(
                fields,
                vec![Box::new(Int64Builder::new()), Box::new(Int32Builder::new())],
            );
            for tuple in tuples {
                match &tuple[index] {
                    Value::Null => {
                        builder
                            .field_builder::<Int64Builder>(0)
                            .ok_or_else(|| corrupt("timetz time builder is missing"))?
                            .append_null();
                        builder
                            .field_builder::<Int32Builder>(1)
                            .ok_or_else(|| corrupt("timetz zone builder is missing"))?
                            .append_null();
                        builder.append(false);
                    }
                    Value::TimeTz(value) => {
                        builder
                            .field_builder::<Int64Builder>(0)
                            .ok_or_else(|| corrupt("timetz time builder is missing"))?
                            .append_value(value.usec);
                        builder
                            .field_builder::<Int32Builder>(1)
                            .ok_or_else(|| corrupt("timetz zone builder is missing"))?
                            .append_value(value.zone);
                        builder.append(true);
                    }
                    _ => return Err(value_mismatch(&column.name, column.ty)),
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        PgType::Timestamp | PgType::TimestampTz => {
            let mut builder = TimestampMicrosecondBuilder::with_capacity(tuples.len());
            for tuple in tuples {
                let value = match (&tuple[index], column.ty) {
                    (Value::Null, _) => {
                        builder.append_null();
                        continue;
                    }
                    (Value::Timestamp(value), PgType::Timestamp)
                    | (Value::TimestampTz(value), PgType::TimestampTz) => *value,
                    _ => return Err(value_mismatch(&column.name, column.ty)),
                };
                let unix = if value == i64::MIN || value == i64::MAX {
                    value
                } else {
                    value
                        .checked_add(PG_UNIX_EPOCH_MICROS)
                        .ok_or_else(|| corrupt("timestamp epoch conversion overflow"))?
                };
                builder.append_value(unix);
            }
            let array = builder.finish();
            if column.ty == PgType::TimestampTz {
                Ok(Arc::new(array.with_timezone("UTC")))
            } else {
                Ok(Arc::new(array))
            }
        }
        PgType::Interval => {
            let fields = match arrow_type(PgType::Interval) {
                DataType::Struct(fields) => fields,
                _ => return Err(corrupt("invalid interval Arrow schema")),
            };
            let mut builder = StructBuilder::new(
                fields,
                vec![
                    Box::new(Int32Builder::new()),
                    Box::new(Int32Builder::new()),
                    Box::new(Int64Builder::new()),
                ],
            );
            for tuple in tuples {
                match &tuple[index] {
                    Value::Null => {
                        builder
                            .field_builder::<Int32Builder>(0)
                            .ok_or_else(|| corrupt("interval month builder is missing"))?
                            .append_null();
                        builder
                            .field_builder::<Int32Builder>(1)
                            .ok_or_else(|| corrupt("interval day builder is missing"))?
                            .append_null();
                        builder
                            .field_builder::<Int64Builder>(2)
                            .ok_or_else(|| corrupt("interval time builder is missing"))?
                            .append_null();
                        builder.append(false);
                    }
                    Value::Interval(value) => {
                        builder
                            .field_builder::<Int32Builder>(0)
                            .ok_or_else(|| corrupt("interval month builder is missing"))?
                            .append_value(value.months);
                        builder
                            .field_builder::<Int32Builder>(1)
                            .ok_or_else(|| corrupt("interval day builder is missing"))?
                            .append_value(value.days);
                        builder
                            .field_builder::<Int64Builder>(2)
                            .ok_or_else(|| corrupt("interval time builder is missing"))?
                            .append_value(value.usec);
                        builder.append(true);
                    }
                    _ => return Err(value_mismatch(&column.name, column.ty)),
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        _ => Err(StorageError::UnsupportedType(format!(
            "data type {} is not supported by table access method \"parquet\"",
            column.ty.name()
        ))),
    }
}

fn build_batch(schema: &TableSchema, tuples: &[Tuple]) -> Result<RecordBatch, StorageError> {
    for tuple in tuples {
        if tuple.len() != schema.columns.len() {
            return Err(corrupt("tuple width does not match Parquet table schema"));
        }
    }
    let arrays = schema
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| build_array(column, tuples, index))
        .collect::<Result<Vec<_>, _>>()?;
    let options = RecordBatchOptions::new().with_row_count(Some(tuples.len()));
    RecordBatch::try_new_with_options(arrow_schema(schema), arrays, &options)
        .map_err(|error| io_error("build Arrow record batch", error))
}

fn required_array<'a, T: 'static>(
    array: &'a dyn Array,
    column: &str,
) -> Result<&'a T, StorageError> {
    array
        .as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| corrupt(format!("Parquet column \"{column}\" has an unexpected type")))
}

fn decode_value(
    column: &crabgresql_storage_api::Column,
    array: &dyn Array,
    row: usize,
) -> Result<Value, StorageError> {
    if array.is_null(row) {
        return Ok(Value::Null);
    }
    macro_rules! primitive {
        ($array:ty, $variant:path) => {{
            let values = required_array::<$array>(array, &column.name)?;
            Ok($variant(values.value(row)))
        }};
    }
    match column.ty {
        PgType::Bool => primitive!(BooleanArray, Value::Bool),
        PgType::Int2 => primitive!(Int16Array, Value::Int2),
        PgType::Int4 => primitive!(Int32Array, Value::Int4),
        PgType::Int8 => primitive!(Int64Array, Value::Int8),
        PgType::Float4 => primitive!(Float32Array, Value::Float4),
        PgType::Float8 => primitive!(Float64Array, Value::Float8),
        PgType::Numeric => {
            let values = required_array::<StringArray>(array, &column.name)?;
            crabgresql_types::numeric::Numeric::parse(values.value(row))
                .map(Value::Numeric)
                .map_err(|_| corrupt(format!("invalid numeric in column \"{}\"", column.name)))
        }
        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => {
            let values = required_array::<StringArray>(array, &column.name)?;
            Ok(Value::Text(values.value(row).to_string()))
        }
        PgType::Bytea => {
            let values = required_array::<BinaryArray>(array, &column.name)?;
            Ok(Value::Bytea(values.value(row).to_vec()))
        }
        PgType::Uuid => {
            let values = required_array::<FixedSizeBinaryArray>(array, &column.name)?;
            let bytes: [u8; 16] = values
                .value(row)
                .try_into()
                .map_err(|_| corrupt(format!("invalid UUID in column \"{}\"", column.name)))?;
            Ok(Value::Uuid(bytes))
        }
        PgType::Date => {
            let values = required_array::<Date32Array>(array, &column.name)?;
            let value = values.value(row);
            Ok(Value::Date(if value == i32::MIN || value == i32::MAX {
                value
            } else {
                value
                    .checked_sub(PG_UNIX_EPOCH_DAYS)
                    .ok_or_else(|| corrupt("date epoch conversion overflow"))?
            }))
        }
        PgType::Time => primitive!(Time64MicrosecondArray, Value::Time),
        PgType::TimeTz => {
            let values = required_array::<StructArray>(array, &column.name)?;
            let time = required_array::<Int64Array>(values.column(0).as_ref(), &column.name)?;
            let zone = required_array::<Int32Array>(values.column(1).as_ref(), &column.name)?;
            Ok(Value::TimeTz(TimeTz {
                usec: time.value(row),
                zone: zone.value(row),
            }))
        }
        PgType::Timestamp | PgType::TimestampTz => {
            let values = required_array::<TimestampMicrosecondArray>(array, &column.name)?;
            let value = values.value(row);
            let pg = if value == i64::MIN || value == i64::MAX {
                value
            } else {
                value
                    .checked_sub(PG_UNIX_EPOCH_MICROS)
                    .ok_or_else(|| corrupt("timestamp epoch conversion overflow"))?
            };
            Ok(if column.ty == PgType::Timestamp {
                Value::Timestamp(pg)
            } else {
                Value::TimestampTz(pg)
            })
        }
        PgType::Interval => {
            let values = required_array::<StructArray>(array, &column.name)?;
            let months = required_array::<Int32Array>(values.column(0).as_ref(), &column.name)?;
            let days = required_array::<Int32Array>(values.column(1).as_ref(), &column.name)?;
            let micros = required_array::<Int64Array>(values.column(2).as_ref(), &column.name)?;
            Ok(Value::Interval(Interval {
                months: months.value(row),
                days: days.value(row),
                usec: micros.value(row),
            }))
        }
        _ => Err(corrupt(format!(
            "unsupported type {} in Parquet catalog",
            column.ty.name()
        ))),
    }
}

/// Decode one row of `batch` into a full-width tuple.
///
/// `positions` maps batch column → schema ordinal (see [`open_reader`]). Under a
/// projection the batch is dense over the *selected* columns while the tuple
/// stays as wide as the schema, so unselected slots keep the `Null` they were
/// initialized with — values the scan contract leaves unspecified.
fn decode_row(
    schema: &TableSchema,
    positions: &[usize],
    batch: &RecordBatch,
    row: usize,
) -> Result<Tuple, StorageError> {
    let mut tuple = vec![Value::Null; schema.columns.len()];
    for (batch_index, &schema_index) in positions.iter().enumerate() {
        tuple[schema_index] = decode_value(
            &schema.columns[schema_index],
            batch.column(batch_index).as_ref(),
            row,
        )?;
    }
    Ok(tuple)
}

#[derive(Clone, Debug)]
struct Fragment {
    path: PathBuf,
    block: u32,
    xid: Xid,
    cid: CommandId,
    pending: bool,
}

impl Fragment {
    /// The name this fragment takes once its transaction commits: the same file
    /// with the `.pending` suffix stripped. `None` for an already-promoted one.
    fn promoted_path(&self) -> Option<PathBuf> {
        if !self.pending {
            return None;
        }
        let name = self.path.file_name()?.to_str()?.strip_suffix(".pending")?;
        Some(self.path.with_file_name(name))
    }
}

/// Open a fragment by path, tolerating a concurrent commit's promotion rename.
///
/// A reader lists fragments up front but opens them lazily, and a fragment is
/// visible under MVCC as soon as its transaction is marked committed in the
/// clog — which happens *before* the finalize hook renames `.pending` away. A
/// scan can therefore hold a `.pending` path that has since been promoted; the
/// bytes are unchanged, so retry under the committed name rather than failing
/// the query with a spurious ENOENT.
fn open_fragment_file(fragment: &Fragment) -> Result<File, StorageError> {
    match File::open(&fragment.path) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let promoted = fragment
                .promoted_path()
                .ok_or_else(|| io_error("open Parquet fragment", error))?;
            File::open(&promoted).map_err(|error| io_error("open Parquet fragment", error))
        }
        Err(error) => Err(io_error("open Parquet fragment", error)),
    }
}

fn parse_fragment(path: PathBuf) -> Result<Option<Fragment>, StorageError> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(corrupt("Parquet fragment has a non-UTF-8 filename"));
    };
    if name.ends_with(".tmp") {
        return Ok(None);
    }
    let (base, pending) = match name.strip_suffix(".parquet.pending") {
        Some(base) => (base, true),
        None => match name.strip_suffix(".parquet") {
            Some(base) => (base, false),
            None => return Ok(None),
        },
    };
    let mut parts = base.split('-');
    // A fragment block is a physical tid address, so the logical-row-id flag must
    // be clear (see `TID_LOGICAL_FLAG`). Rejecting it here, at the one place a
    // block number is read off disk, keeps a hand-edited or future-format name
    // from producing tids that `fetch` would route to the wrong storage.
    let block = parts
        .next()
        .and_then(|value| u32::from_str_radix(value, 16).ok())
        .filter(|block| *block <= MAX_PHYSICAL_BLOCK)
        .ok_or_else(|| corrupt(format!("invalid Parquet fragment filename \"{name}\"")))?;
    let xid = parts
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Xid)
        .ok_or_else(|| corrupt(format!("invalid Parquet fragment filename \"{name}\"")))?;
    let cid = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .map(CommandId)
        .ok_or_else(|| corrupt(format!("invalid Parquet fragment filename \"{name}\"")))?;
    if parts.next().is_some() {
        return Err(corrupt(format!("invalid Parquet fragment filename \"{name}\"")));
    }
    Ok(Some(Fragment {
        path,
        block,
        xid,
        cid,
        pending,
    }))
}

/// List `dir`'s fragments, ordered by block. A missing directory is an error, not
/// an empty table: the relation's storage having vanished must surface, never read
/// back as "no rows". Paths that legitimately race with a directory being reclaimed
/// use [`remove_dir_all_ok`] or create the directory first.
fn fragments(dir: &Path) -> Result<Vec<Fragment>, StorageError> {
    let entries =
        std::fs::read_dir(dir).map_err(|error| io_error("read Parquet table", error))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| io_error("read Parquet table entry", error))?;
        if let Some(fragment) = parse_fragment(entry.path())? {
            out.push(fragment);
        }
    }
    out.sort_by_key(|fragment| fragment.block);
    Ok(out)
}

fn header(fragment: &Fragment) -> TupleHeader {
    TupleHeader {
        xmin: fragment.xid,
        xmax: Xid::INVALID,
        cmin: fragment.cid,
        cmax: CommandId::FIRST,
        infomask: Infomask::default(),
    }
}

fn metadata_map(
    metadata: Option<&Vec<KeyValue>>,
) -> HashMap<&str, &str> {
    metadata
        .into_iter()
        .flatten()
        .filter_map(|item| item.value.as_deref().map(|value| (item.key.as_str(), value)))
        .collect()
}

/// Open a row reader over one fragment, re-checking the footer identity first.
///
/// **Invariant P1.** Every fragment under `parquet/<r>/` carries `META_REL = r`,
/// so `rel` must be the relfilenode that *named the directory the fragment was
/// listed from* — [`ParquetTable::effective_rel`] for a reader inside a
/// transaction with a staged TRUNCATE, not the live one. Directories are only ever
/// created and removed, never renamed, so no footer has to be rewritten; the price
/// is that passing the wrong generation's id reports perfectly good bytes as
/// [`StorageError::CorruptData`].
/// Opens `fragment` for reading, restricted to `projection`.
///
/// Returns the reader together with its **position map**: entry `i` is the
/// schema ordinal that batch column `i` decodes into. For an unprojected read
/// that is the identity; for a projected one it is the selected ordinals. The
/// map is derived from the reader's own schema by field *name* rather than by
/// assuming the mask preserves the requested order.
fn open_reader(
    schema: &TableSchema,
    rel: u32,
    fragment: &Fragment,
    projection: &ColumnProjection,
) -> Result<(ParquetRecordBatchReader, Arc<[usize]>), StorageError> {
    let file = open_fragment_file(fragment)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|error| corrupt(format!("read Parquet footer: {error}")))?;
    let metadata = metadata_map(builder.metadata().file_metadata().key_value_metadata());
    let expected = [
        (META_VERSION, FORMAT_VERSION.to_string()),
        (META_REL, rel.to_string()),
        (META_XMIN, fragment.xid.0.to_string()),
        (META_CMIN, fragment.cid.0.to_string()),
        (META_SCHEMA, schema_identity(schema)),
    ];
    for (key, value) in expected {
        if metadata.get(key).copied() != Some(value.as_str()) {
            return Err(corrupt(format!(
                "Parquet fragment {} has invalid {key} metadata",
                fragment.path.display()
            )));
        }
    }
    if builder.schema().fields().len() != schema.columns.len() {
        return Err(corrupt(format!(
            "Parquet fragment {} does not match the table schema",
            fragment.path.display()
        )));
    }
    // Built *after* the checks above, which read the file schema from the
    // builder — `with_projection` narrows only the reader's output, so both
    // the field-count check and the `META_SCHEMA` identity stay meaningful.
    //
    // `roots`, not `leaves`: `arrow_type` maps `timetz` and `interval` to a
    // `Struct`, so those columns own several *leaf* descriptors but one root.
    // Root indices are the top-level fields, which the writer lays out 1:1 with
    // `schema.columns` — the invariant the field-count check just enforced.
    let mask = match projection {
        ColumnProjection::All => ProjectionMask::all(),
        ColumnProjection::Some(cols) => {
            ProjectionMask::roots(builder.parquet_schema(), cols.iter().copied())
        }
    };
    let reader = builder
        .with_batch_size(8_192)
        .with_projection(mask)
        .build()
        .map_err(|error| corrupt(format!("open Parquet row reader: {error}")))?;

    let positions: Arc<[usize]> = reader
        .schema()
        .fields()
        .iter()
        .map(|field| {
            schema
                .columns
                .iter()
                .position(|column| column.name == *field.name())
                .ok_or_else(|| {
                    corrupt(format!(
                        "Parquet fragment {} has column \"{}\", which the table schema does not",
                        fragment.path.display(),
                        field.name()
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?
        .into();

    Ok((reader, positions))
}

struct ParquetScan {
    schema: TableSchema,
    rel: u32,
    /// The columns to read off disk. Prunes work only: a mask never changes how
    /// many rows a fragment yields or the order they arrive in, which is what
    /// keeps the `Tid` ordinals below stable and `fetch` able to find them.
    projection: ColumnProjection,
    fragments: Vec<Fragment>,
    fragment_index: usize,
    reader: Option<ParquetRecordBatchReader>,
    /// Batch column → schema ordinal for the fragment currently open, rebuilt
    /// each time `reader` is replaced.
    positions: Arc<[usize]>,
    batch: Option<RecordBatch>,
    batch_row: usize,
    file_row: u32,
    current_block: u32,
    /// Keeps the shared hold for the whole iterator life, so a concurrent
    /// TRUNCATE cannot remove the directory this scan is still reading.
    _guard: SharedGuard,
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
                    Some(Ok(batch)) => {
                        self.batch = Some(batch);
                        self.batch_row = 0;
                        continue;
                    }
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

/// The batch form of [`ParquetScan`]: the same fragments, the same order, the
/// same shared hold — handing the reader's `RecordBatch`es up instead of
/// shredding each one into rows.
///
/// Everything that decides *which* rows a scan returns is shared with the row
/// scan: `visible_fragments` filters whole files by MVCC before either iterator
/// exists, and `open_reader` applies the same projection mask and re-checks the
/// same footer identity. This iterator only changes the shape of the handoff.
struct ParquetBatchScan {
    schema: TableSchema,
    /// The batch schema every produced batch carries. Fixed for the scan's life
    /// — expression compilation resolved column positions against it — so a
    /// fragment whose reader disagrees is an error rather than a reshaping.
    batch_schema: BatchSchema,
    /// The schema ordinals, ascending, that every fragment must decode into.
    slots: Arc<[usize]>,
    rel: u32,
    projection: ColumnProjection,
    fragments: Vec<Fragment>,
    fragment_index: usize,
    reader: Option<ParquetRecordBatchReader>,
    /// Keeps the shared hold for the whole iterator life, so a concurrent
    /// TRUNCATE cannot remove the directory this scan is still reading.
    _guard: SharedGuard,
}

impl Iterator for ParquetBatchScan {
    type Item = Result<Batch, StorageError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(reader) = &mut self.reader {
                match reader.next() {
                    Some(Ok(batch)) => {
                        // An empty batch carries no rows and no meaning; skipping
                        // it here keeps `None` the only end-of-stream signal.
                        if batch.num_rows() == 0 {
                            continue;
                        }
                        return Some(to_batch(&self.schema, &self.batch_schema, &self.slots, &batch));
                    }
                    Some(Err(error)) => {
                        self.reader = None;
                        return Some(Err(corrupt(format!("decode Parquet row group: {error}"))));
                    }
                    None => self.reader = None,
                }
            }
            let fragment = self.fragments.get(self.fragment_index)?.clone();
            self.fragment_index += 1;
            match open_reader(&self.schema, self.rel, &fragment, &self.projection) {
                Ok((reader, positions)) => {
                    if positions.as_ref() != self.slots.as_ref() {
                        return Some(Err(corrupt(format!(
                            "Parquet fragment {} decodes columns {:?} where {:?} was expected",
                            fragment.path.display(),
                            positions,
                            self.slots
                        ))));
                    }
                    self.reader = Some(reader);
                }
                Err(error) => return Some(Err(error)),
            }
        }
    }
}

/// Convert one reader `RecordBatch` into a [`Batch`].
///
/// Mostly free: an Arrow array whose storage layout already matches the batch
/// representation is handed over by `Arc` clone with no copy at all, which is
/// every type except `date` and `timestamp`. Those two are rebased out of the
/// Arrow epoch — one pass over one column — because a batch carries
/// PostgreSQL-domain values (see [`crabgresql_batch::epoch`] for why this is not
/// pushed into the constants instead).
fn to_batch(
    schema: &TableSchema,
    batch_schema: &BatchSchema,
    slots: &[usize],
    batch: &RecordBatch,
) -> Result<Batch, StorageError> {
    if batch.num_columns() != slots.len() {
        return Err(corrupt(format!(
            "Parquet batch has {} columns where {} were expected",
            batch.num_columns(),
            slots.len()
        )));
    }
    let mut columns = Vec::with_capacity(slots.len());
    for (index, &slot) in slots.iter().enumerate() {
        let column = schema
            .columns
            .get(slot)
            .ok_or_else(|| corrupt(format!("Parquet batch names missing column {slot}")))?;
        columns.push(to_batch_array(column, batch.column(index))?);
    }
    Batch::new(batch_schema.clone(), columns, batch.num_rows())
        .map_err(|error| corrupt(format!("build batch: {error}")))
}

/// One stored Arrow array in the batch representation of `column`'s type.
fn to_batch_array(
    column: &crabgresql_storage_api::Column,
    array: &ArrayRef,
) -> Result<ArrayRef, StorageError> {
    let mismatch = || corrupt(format!("Parquet column \"{}\" has an unexpected type", column.name));
    let converted: ArrayRef = match column.ty {
        // `Date32Array` and `Int32Array` share a layout, so reinterpreting the
        // buffers is free; only the epoch shift touches values.
        PgType::Date => {
            let stored = array
                .as_any()
                .downcast_ref::<Date32Array>()
                .ok_or_else(mismatch)?;
            let raw = Int32Array::new(stored.values().clone(), stored.nulls().cloned());
            Arc::new(epoch::rebase_dates(&raw).ok_or_else(|| {
                corrupt(format!(
                    "date epoch conversion overflow in column \"{}\"",
                    column.name
                ))
            })?)
        }
        PgType::Timestamp | PgType::TimestampTz => {
            let stored = array
                .as_any()
                .downcast_ref::<TimestampMicrosecondArray>()
                .ok_or_else(mismatch)?;
            let raw = Int64Array::new(stored.values().clone(), stored.nulls().cloned());
            Arc::new(epoch::rebase_timestamps(&raw).ok_or_else(|| {
                corrupt(format!(
                    "timestamp epoch conversion overflow in column \"{}\"",
                    column.name
                ))
            })?)
        }
        // PostgreSQL's `time` is already microseconds since midnight, so this is
        // a pure relabelling with no shift.
        PgType::Time => {
            let stored = array
                .as_any()
                .downcast_ref::<Time64MicrosecondArray>()
                .ok_or_else(mismatch)?;
            Arc::new(Int64Array::new(
                stored.values().clone(),
                stored.nulls().cloned(),
            ))
        }
        // Every other type stores exactly what a batch wants. This is the whole
        // point of the exercise: 100-odd of ClickBench's 105 columns arrive here
        // and cost one `Arc` clone.
        _ => Arc::clone(array),
    };
    Ok(converted)
}

/// The batch schema a scan of `schema` restricted to `slots` produces.
fn batch_schema_for(schema: &TableSchema, slots: &[usize]) -> Result<BatchSchema, StorageError> {
    let mut fields = Vec::with_capacity(slots.len());
    for &slot in slots {
        let column = schema
            .columns
            .get(slot)
            .ok_or_else(|| corrupt(format!("projection names missing column {slot}")))?;
        let field = BatchField::new(
            Some(column.name.clone()),
            column.ty,
            column.typmod,
            column.nullable,
        )
        .ok_or_else(|| {
            StorageError::UnsupportedType(format!(
                "data type {} has no columnar batch representation",
                column.ty.name()
            ))
        })?;
        fields.push(field);
    }
    BatchSchema::scan(fields, slots.to_vec())
        .map_err(|error| corrupt(format!("build batch schema: {error}")))
}

/// An uncommitted directory-swap TRUNCATE staged by one transaction. Because a
/// TRUNCATE holds the table exclusively until it commits, at most one can exist on
/// a table at a time — hence a single `Option`, not a map.
struct PendingTruncate {
    xid: Xid,
    new_rel: u32,
    /// The lock owner holding the table exclusively — needed to release the hold
    /// from the commit/abort path, which only receives the XID.
    owner: LockOwner,
    /// `next_block` as of the *first* TRUNCATE in this transaction, restored on
    /// abort so a later insert into the surviving directory cannot re-issue a
    /// block number an existing fragment already owns (invariant P2).
    saved_next_block: u32,
}

pub struct ParquetTable {
    schema: TableSchema,
    /// The data directory. The table's fragments live in `root/parquet/<rel>`,
    /// which TRUNCATE swaps — so the path is derived, never cached.
    root: PathBuf,
    /// The committed relfilenode: the directory every transaction reads, except
    /// the one holding a pending TRUNCATE (which reads `pending.new_rel`).
    live_rel: AtomicU32,
    /// A staged, not-yet-committed TRUNCATE, if any. `pending` and `has_pending`
    /// are the single source of truth for an in-flight swap and are mutated ONLY
    /// together, through this type's methods, so they never drift.
    pending: RwLock<Option<PendingTruncate>>,
    /// Cheap gate letting the read/write hot path skip the `pending` RwLock read
    /// entirely while no TRUNCATE is in flight — kept in sync with `pending`.
    has_pending: AtomicBool,
    /// Serializes TRUNCATE (exclusive) against readers/writers (shared).
    lock: Arc<TableLock>,
    wal: Arc<Wal>,
    /// The engine's relfilenode counter, used to name the directory a TRUNCATE
    /// stages. Shared with every other relation so an id can never alias one.
    relfilenodes: Arc<dyn RelfilenodeAllocator>,
    indexes: RwLock<Vec<IndexMetadata>>,
    /// The cached ANALYZE result, tagged with the transaction that measured it —
    /// [`Xid::INVALID`] for a result seeded from the catalog at startup. The tag is
    /// what lets an abort tell a measurement of storage it just destroyed (its own)
    /// from one taken before it, which stays valid.
    analyzed: RwLock<Option<(Xid, u32, f64)>>,
    next_block: Mutex<u32>,
    /// Transactions that have staged `.pending` fragments or a TRUNCATE in this
    /// table and not yet been finalized. The engine's commit/abort hook runs over
    /// every open table, so this lets [`ParquetTable::finish_transaction`] answer
    /// "nothing of mine" from memory instead of paying a directory scan and an
    /// fsync on every transaction end. Empty after a restart, which is correct:
    /// [`ParquetTable::recover`] reconciles leftover pending files directly and
    /// the WAL carries the swap.
    staged_xids: Mutex<HashSet<Xid>>,
}

impl ParquetTable {
    pub fn open(
        root: &Path,
        rel: u32,
        schema: TableSchema,
        indexes: Vec<IndexMetadata>,
        wal: Arc<Wal>,
        relfilenodes: Arc<dyn RelfilenodeAllocator>,
    ) -> Result<Self, StorageError> {
        validate_schema(&schema)?;
        let dir = root.join("parquet").join(rel.to_string());
        std::fs::create_dir_all(&dir)
            .map_err(|error| io_error("create Parquet table directory", error))?;
        Ok(Self {
            schema,
            root: root.to_path_buf(),
            live_rel: AtomicU32::new(rel),
            pending: RwLock::new(None),
            has_pending: AtomicBool::new(false),
            lock: Arc::new(TableLock::new()),
            wal,
            relfilenodes,
            indexes: RwLock::new(indexes),
            analyzed: RwLock::new(None),
            next_block: Mutex::new(next_block_in(&dir)?),
            staged_xids: Mutex::new(HashSet::new()),
        })
    }

    fn dir_of(&self, rel: u32) -> PathBuf {
        self.root.join("parquet").join(rel.to_string())
    }

    /// The committed relfilenode — the one the catalog names.
    pub fn relfilenode(&self) -> u32 {
        self.live_rel.load(Ordering::Relaxed)
    }

    fn live_dir(&self) -> PathBuf {
        self.dir_of(self.relfilenode())
    }

    /// The relfilenode `xid` should read and write: the directory staged by its own
    /// TRUNCATE, else the committed one.
    pub fn effective_rel(&self, xid: Xid) -> u32 {
        if self.has_pending.load(Ordering::Acquire)
            && let Some(p) = self
                .pending
                .read()
                .unwrap_or_else(|_| panic!("rwlock poisoned"))
                .as_ref()
            && p.xid == xid
        {
            return p.new_rel;
        }
        self.relfilenode()
    }

    /// The `(new_rel, owner)` of a TRUNCATE staged by `xid`, if any.
    fn staged_truncate(&self, xid: Xid) -> Option<(u32, LockOwner)> {
        self.pending
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .as_ref()
            .filter(|p| p.xid == xid)
            .map(|p| (p.new_rel, p.owner))
    }

    /// Measure the relation's current on-disk size in 8 KB pages, ignoring any
    /// cached ANALYZE result. `statistics()` deliberately prefers the cached
    /// value; ANALYZE itself must re-measure, or `relpages` would freeze at
    /// whatever the first ANALYZE recorded and never track the table's growth.
    ///
    /// Deliberately lock-free: `TableAm::statistics` has no `TxnContext` (so no
    /// lock owner) and is called while planning, where blocking behind a TRUNCATE's
    /// transaction would stall unrelated queries. The cost is a race with a
    /// committing TRUNCATE, which publishes the new relfilenode before removing the
    /// old directory — so a listing that lost the race is simply retried against the
    /// directory that is live by then.
    pub fn measure_relpages(&self) -> Result<u32, StorageError> {
        let rel = self.relfilenode();
        match relpages_in(&self.dir_of(rel)) {
            Ok(relpages) => Ok(relpages),
            Err(error) => {
                let now = self.relfilenode();
                if now == rel {
                    return Err(error);
                }
                relpages_in(&self.dir_of(now))
            }
        }
    }

    /// Size and row count for ANALYZE, taken under one shared hold from ONE listing
    /// of the fragment directory — the scan below reuses the very fragments that
    /// were measured, rather than re-listing. Two listings would let a fragment
    /// promoted (`.pending` renamed away) in between be stat'd at a path that no
    /// longer exists — silently contributing 0 bytes while its rows still count —
    /// and would let an ANALYZE inside an uncommitted TRUNCATE pair one directory's
    /// rows with another's pages.
    pub fn measure(&self, txn: &TxnContext) -> Result<(u32, f64), StorageError> {
        let guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        let visible = self.visible_fragments(rel, txn)?;
        let relpages = relpages_of(&visible);
        let mut rows = 0u64;
        // Only the rows are being counted, so read the narrowest column and skip
        // the rest. The count is unchanged — a mask prunes columns, never rows.
        let projection = ColumnProjection::of([], &self.schema);
        for row in self.scan_over(rel, visible, guard, &projection) {
            row?;
            rows += 1;
        }
        Ok((relpages, rows as f64))
    }

    /// Drop the cached ANALYZE result, returning the relation to never-analyzed —
    /// which is what PostgreSQL reports after a TRUNCATE (`relpages = 0`,
    /// `reltuples = -1`), not a measured zero. Called when the fragments the
    /// measurement described stop existing.
    fn forget_analyzed(&self) {
        *self
            .analyzed
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned")) = None;
    }

    /// Drop the cached ANALYZE result only if `xid` is the transaction that took
    /// it. Used when a rollback unlinks storage that transaction had staged: its
    /// own measurement covered those fragments, while a measurement taken before
    /// the transaction started still describes what survives.
    fn forget_analyzed_by(&self, xid: Xid) {
        let mut analyzed = self
            .analyzed
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        if analyzed.is_some_and(|(measured, _, _)| measured == xid) {
            *analyzed = None;
        }
    }

    /// Record a result measured outside any transaction — the catalog's persisted
    /// statistics, replayed into the handle at startup.
    pub fn set_analyzed(&self, relpages: u32, reltuples: f64) {
        self.set_analyzed_by(Xid::INVALID, relpages, reltuples);
    }

    /// Record the result of an ANALYZE run by `xid` (see the `analyzed` field).
    pub fn set_analyzed_by(&self, xid: Xid, relpages: u32, reltuples: f64) {
        *self
            .analyzed
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned")) = Some((xid, relpages, reltuples));
    }

    pub fn add_index(&self, index: IndexMetadata) {
        self.indexes
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .push(index);
    }

    pub fn remove_index(&self, name: &str) {
        self.indexes
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .retain(|index| index.name != name);
    }

    /// Promote (on commit) or unlink (on abort) an already-listed set of pending
    /// fragments. Does not scan the directory or fsync it — the caller owns both,
    /// so a batch of transactions costs one listing and one fsync rather than one
    /// of each per transaction.
    fn reconcile(&self, pending: &[Fragment], committed: bool) -> Result<(), StorageError> {
        for fragment in pending {
            if committed {
                let promoted = fragment
                    .promoted_path()
                    .ok_or_else(|| corrupt("pending Parquet filename is invalid"))?;
                std::fs::rename(&fragment.path, &promoted)
                    .map_err(|error| io_error("promote Parquet fragment", error))?;
            } else {
                std::fs::remove_file(&fragment.path)
                    .map_err(|error| io_error("remove aborted Parquet fragment", error))?;
            }
        }
        Ok(())
    }

    /// Promote or unlink the `.pending` fragments `xid` staged in `dir`, scanning
    /// and fsyncing that one directory. A transaction that staged nothing there
    /// costs the listing and no writes.
    fn reconcile_pending_in(
        &self,
        dir: &Path,
        xid: Xid,
        committed: bool,
    ) -> Result<(), StorageError> {
        let pending: Vec<Fragment> = fragments(dir)?
            .into_iter()
            .filter(|fragment| fragment.pending && fragment.xid == xid)
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        self.reconcile(&pending, committed)?;
        sync_dir(dir)
    }

    /// Reconcile everything `xid` staged in this table: its `.pending` fragments
    /// and, if it ran a TRUNCATE, the directory swap.
    ///
    /// Returns the applied swap when a committed TRUNCATE replaced the directory.
    /// The caller must then persist it with `swap_relfilenode` and release the hold
    /// with [`ParquetTable::release_truncate_lock`] — in that order, and on every
    /// path including a failed persist. Handing the hold back rather than dropping
    /// it here is what keeps a stale catalog write from clobbering a newer
    /// TRUNCATE's: a second TRUNCATE cannot even stage until the hold is released.
    ///
    /// The in-memory state transition happens before any error is returned, so an
    /// error here means "the swap took effect in memory but some file work did
    /// not" — the caller logs it, and the WAL record repairs the catalog at the
    /// next recovery. On every path that returns `Ok(None)`/`Err` the hold is
    /// already released, so no caller can leak it.
    pub fn finish_transaction(
        &self,
        xid: Xid,
        committed: bool,
    ) -> Result<Option<ParquetSwap>, StorageError> {
        // The engine's finalize hook calls this for every open Parquet table on
        // every transaction end. Tables the transaction never wrote to answer
        // from memory here, without touching the filesystem at all.
        let staged = self
            .staged_xids
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .remove(&xid);
        if !staged {
            return Ok(None);
        }
        let Some((new_rel, owner)) = self.staged_truncate(xid) else {
            let outcome = self.reconcile_pending_in(&self.live_dir(), xid, committed);
            if !committed {
                // The fragments just unlinked may have been measured: an ANALYZE run
                // inside this transaction sized its own not-yet-committed fragments
                // (the same rows it counted). Those bytes are gone now, so its cached
                // measurement describes a relation that never existed. A measurement
                // taken before this transaction is untouched — it still describes the
                // fragments that survive.
                self.forget_analyzed_by(xid);
            }
            return outcome.map(|()| None);
        };
        if !committed {
            // The TRUNCATE never happened. Its staged directory goes wholesale, so
            // the fragments inside it need no per-file pass — but fragments this
            // transaction staged *before* the TRUNCATE live in the surviving
            // directory and must be unlinked there. Nothing to persist, so the hold
            // is released here.
            self.abort_truncate(xid);
            let cleaned = self.reconcile_pending_in(&self.live_dir(), xid, false);
            let reclaimed = remove_dir_all_ok(&self.dir_of(new_rel));
            // As above: an ANALYZE run by this transaction measured the staged
            // directory, which is now gone. An older measurement described the
            // surviving directory and stays.
            self.forget_analyzed_by(xid);
            self.lock.release_exclusive(owner);
            return cleaned.and(reclaimed).map(|()| None);
        }
        // Swap first, so `live_dir()` already names the new directory and a failure
        // past this point cannot leave the table reading the directory this commit
        // is about to remove.
        let old = self.commit_truncate(xid);
        let promoted = self.reconcile_pending_in(&self.dir_of(new_rel), xid, true);
        // The old directory's rows are gone as of this commit. Failing to remove it
        // only leaks disk: it is no longer named by the catalog, so
        // `gc_orphan_parquet_dirs` reclaims it at the next boot.
        let reclaimed = old.map_or(Ok(()), |old| remove_dir_all_ok(&self.dir_of(old)));
        match promoted.and(reclaimed) {
            Ok(()) => Ok(Some(ParquetSwap { new_rel, owner })),
            Err(error) => {
                // The caller gets no swap to persist, so it cannot release for us.
                self.lock.release_exclusive(owner);
                Err(error)
            }
        }
    }

    /// Release the exclusive hold a committed TRUNCATE kept (keyed by its lock
    /// owner). Call it after persisting the swap [`ParquetTable::finish_transaction`]
    /// returned, whether or not that persist succeeded.
    pub fn release_truncate_lock(&self, owner: LockOwner) {
        self.lock.release_exclusive(owner);
    }

    /// Apply a committed TRUNCATE: the staged directory becomes the live one.
    /// Returns the superseded relfilenode, or `None` if nothing was staged by `xid`.
    ///
    /// Deliberately leaves `next_block` alone (invariant P2): the truncating
    /// transaction may already have filled blocks in the new directory, and
    /// restarting the counter would hand out a block number a fragment there
    /// already owns — duplicate TIDs in a scan and a wrong row from `fetch`.
    fn commit_truncate(&self, xid: Xid) -> Option<u32> {
        let mut pending = self
            .pending
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let p = pending.take_if(|p| p.xid == xid)?;
        let old = self.live_rel.swap(p.new_rel, Ordering::Relaxed);
        self.has_pending.store(false, Ordering::Release);
        // The measurement described the directory that just went away.
        self.forget_analyzed();
        Some(old)
    }

    /// Discard a staged TRUNCATE on abort: the live directory keeps its rows, and
    /// the block counter returns to where the first TRUNCATE found it.
    fn abort_truncate(&self, xid: Xid) {
        let mut pending = self
            .pending
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let Some(p) = pending.take_if(|p| p.xid == xid) else {
            return;
        };
        self.has_pending.store(false, Ordering::Release);
        *self
            .next_block
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned")) = p.saved_next_block;
    }

    /// Point the table at `new` after recovery applied a committed swap (the
    /// on-disk catalog lagged the WAL). Clears any stale pending state.
    ///
    /// Re-derives `next_block` from the new directory (invariant P4): the handle
    /// was opened against the *old* one, so a carried-over counter can sit below
    /// the highest block the new directory already holds and collide with it.
    pub fn rebind(&self, new: u32) -> Result<(), StorageError> {
        // Both fallible steps run BEFORE `live_rel` is published: a caller that
        // logs the error and carries on (startup must not abort over one relation)
        // would otherwise leave the table pointing at the new directory while the
        // counter still describes the old one — the block collision P4 exists to
        // prevent.
        let dir = self.dir_of(new);
        std::fs::create_dir_all(&dir)
            .map_err(|error| io_error("create Parquet table directory", error))?;
        let next_block = next_block_in(&dir)?;
        {
            // `pending` and `has_pending` are cleared under one write guard, so no
            // reader can observe the gate set with nothing behind it.
            let mut pending = self
                .pending
                .write()
                .unwrap_or_else(|_| panic!("rwlock poisoned"));
            *pending = None;
            self.has_pending.store(false, Ordering::Release);
        }
        *self
            .next_block
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned")) = next_block;
        self.live_rel.store(new, Ordering::Relaxed);
        // The seeded ANALYZE result came from the catalog, which described the
        // pre-swap directory; as on the commit path, go back to never-analyzed
        // rather than let it describe the relation we just swapped in.
        self.forget_analyzed();
        Ok(())
    }

    pub fn recover(&self, clog: &crabgresql_txn::Clog) -> Result<(), StorageError> {
        let dir = self.live_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|error| io_error("create Parquet table directory", error))?;
        // Collect the whole directory before touching it: the reconciliation
        // below renames and unlinks entries, and mutating a directory while a
        // `read_dir` stream over it is still open can silently skip entries,
        // stranding another transaction's fragments as `.pending` forever.
        let entries = std::fs::read_dir(&dir)
            .map_err(|error| io_error("recover Parquet table", error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| io_error("recover Parquet entry", error))?;
        let mut pending: HashMap<Xid, Vec<Fragment>> = HashMap::new();
        let mut dirty = false;
        for entry in entries {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("tmp") {
                std::fs::remove_file(path)
                    .map_err(|error| io_error("remove temporary Parquet fragment", error))?;
                dirty = true;
                continue;
            }
            if let Some(fragment) = parse_fragment(path)?
                && fragment.pending
            {
                pending.entry(fragment.xid).or_default().push(fragment);
            }
        }
        // One reconcile pass per distinct transaction, not per file — and a
        // single directory fsync covering all of them.
        for (xid, fragments) in &pending {
            self.reconcile(fragments, clog.is_committed(*xid))?;
            dirty = true;
        }
        if dirty {
            sync_dir(&dir)?;
        }
        // A carried-over counter would be wrong if recovery promoted fragments
        // this handle never saw (it is seeded at `open`, before replay).
        *self
            .next_block
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned")) = next_block_in(&dir)?;
        Ok(())
    }

    /// Remove the table's storage: the live directory and, if a TRUNCATE is still
    /// staged, the directory it staged (the catalog never named it, so nothing else
    /// would ever reclaim it in this process).
    ///
    /// Deliberately does NOT take the table lock, matching the heap's DROP path: an
    /// exclusive acquire here would wait for an *uncommitted* TRUNCATE's hold, which
    /// is kept to that transaction's end, so a DROP could block for an unbounded
    /// time on a reactor thread with no timeout. The consequence is the same as the
    /// heap's — a concurrent scan can have its storage removed mid-iteration and
    /// report an I/O error — and closing it needs transactional DDL, not a lock here.
    pub fn drop_storage(&self) -> Result<(), StorageError> {
        let staged = self
            .pending
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .as_ref()
            .map(|p| p.new_rel);
        remove_dir_all_ok(&self.live_dir())?;
        match staged {
            Some(staged) => remove_dir_all_ok(&self.dir_of(staged)),
            None => Ok(()),
        }
    }

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
    fn scan_in(
        &self,
        txn: &TxnContext,
        projection: &ColumnProjection,
    ) -> Result<ParquetScan, StorageError> {
        let guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        let fragments = self.visible_fragments(rel, txn)?;
        Ok(self.scan_over(rel, fragments, guard, projection))
    }

    /// The batch twin of [`ParquetTable::scan_in`].
    ///
    /// Takes the shared hold and resolves the relfilenode in the same order and
    /// for the same reason (invariant P1), then lists the same fragments through
    /// the same `visible_fragments` — so a batch scan and a row scan started
    /// under one snapshot provably see the same rows.
    fn batch_scan_in(
        &self,
        txn: &TxnContext,
        req: &ScanRequest,
    ) -> Result<ParquetBatchScan, StorageError> {
        let slots: Arc<[usize]> = req.slots(self.schema.columns.len()).into();
        let batch_schema = batch_schema_for(&self.schema, &slots)?;
        let guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        let fragments = self.visible_fragments(rel, txn)?;
        Ok(ParquetBatchScan {
            schema: self.schema.clone(),
            batch_schema,
            slots,
            rel,
            projection: req.columns.clone(),
            fragments,
            fragment_index: 0,
            reader: None,
            _guard: guard,
        })
    }

    /// Scan an already-listed fragment set, taking over the caller's shared hold.
    /// Lets a caller that has both measured and listed (see [`ParquetTable::measure`])
    /// read exactly what it measured.
    fn scan_over(
        &self,
        rel: u32,
        fragments: Vec<Fragment>,
        guard: SharedGuard,
        projection: &ColumnProjection,
    ) -> ParquetScan {
        ParquetScan {
            schema: self.schema.clone(),
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
    fn visible_fragments(
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

    /// Write one fragment into `dir` and fsync it, returning its `.tmp` path and
    /// the `.pending` name the caller renames it to.
    ///
    /// `rel` must be the relfilenode that names `dir` (invariant P1) — it is
    /// stamped into the footer and re-checked on every later read, so a
    /// post-TRUNCATE insert has to carry the *staged* directory's id.
    fn write_fragment(
        &self,
        rel: u32,
        dir: &Path,
        block: u32,
        tuples: &[Tuple],
        txn: &TxnContext,
    ) -> Result<(PathBuf, PathBuf), StorageError> {
        let base = format!("{block:08x}-{}-{}", txn.xid.0, txn.cid.0);
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
            .build();
        let batch = build_batch(&self.schema, tuples)?;
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
}

fn sync_dir(dir: &Path) -> Result<(), StorageError> {
    File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(|error| io_error("fsync Parquet table directory", error))
}

/// The first block number no fragment in `dir` owns.
fn next_block_in(dir: &Path) -> Result<u32, StorageError> {
    Ok(fragments(dir)?
        .into_iter()
        .map(|fragment| fragment.block)
        .max()
        .unwrap_or(0)
        .saturating_add(1))
}

/// `fragments` measured in 8 KB pages, so `relpages` is comparable to a heap
/// relation's.
fn relpages_of(fragments: &[Fragment]) -> u32 {
    let bytes: u64 = fragments
        .iter()
        .filter_map(|fragment| std::fs::metadata(&fragment.path).ok())
        .map(|metadata| metadata.len())
        .sum();
    bytes.div_ceil(8_192).min(u32::MAX as u64) as u32
}

/// `dir`'s committed fragments measured in 8 KB pages. Pending fragments belong to
/// no committed transaction yet and are excluded — a caller measuring what a
/// specific transaction can see wants [`relpages_of`] over its visible set instead.
fn relpages_in(dir: &Path) -> Result<u32, StorageError> {
    let committed: Vec<Fragment> = fragments(dir)?
        .into_iter()
        .filter(|fragment| !fragment.pending)
        .collect();
    Ok(relpages_of(&committed))
}

/// Remove a fragment directory; an already-absent one is success. Every caller is
/// reclaiming storage that may have been reclaimed by a previous attempt, by a
/// crash-time sweep, or never created at all.
fn remove_dir_all_ok(dir: &Path) -> Result<(), StorageError> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("remove Parquet fragment directory", error)),
    }
}

/// A committed TRUNCATE's applied directory swap, handed to the engine so it can
/// persist the new relfilenode and then release the table's exclusive hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParquetSwap {
    pub new_rel: u32,
    pub owner: LockOwner,
}

/// A directory-swap TRUNCATE replayed from the WAL, awaiting the CLOG's verdict on
/// its transaction. Collected by [`ParquetRedo`] and drained by the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredParquetTruncate {
    pub xid: Xid,
    pub namespace: String,
    pub name: String,
    pub old: u32,
    pub new: u32,
}

/// Encode a [`PARQUET_TRUNCATE`] payload: the relation's old (still-live) and new
/// (staged, empty) fragment directories, plus the relation's schema-qualified name
/// so recovery can rebind the catalog once it knows the transaction's fate. Layout
/// `[old:u32][new:u32][ns_len:u32][ns][name_len:u32][name]`, little-endian.
fn encode_truncate(namespace: &str, name: &str, old: u32, new: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&old.to_le_bytes());
    out.extend_from_slice(&new.to_le_bytes());
    for text in [namespace, name] {
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
    }
    out
}

fn decode_truncate(xid: Xid, payload: &[u8]) -> Result<RecoveredParquetTruncate, WalError> {
    let bad = || WalError::Redo("parquet truncate record: truncated payload".to_string());
    let u32_at = |offset: usize| -> Result<u32, WalError> {
        payload
            .get(offset..offset + 4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().expect("4 bytes")))
            .ok_or_else(bad)
    };
    let old = u32_at(0)?;
    let new = u32_at(4)?;
    let mut at = 8;
    let mut text = || -> Result<String, WalError> {
        let len = u32_at(at)? as usize;
        at += 4;
        let bytes = payload.get(at..at + len).ok_or_else(bad)?;
        at += len;
        String::from_utf8(bytes.to_vec())
            .map_err(|e| WalError::Redo(format!("parquet truncate record: bad name: {e}")))
    };
    let namespace = text()?;
    let name = text()?;
    Ok(RecoveredParquetTruncate {
        xid,
        namespace,
        name,
        old,
        new,
    })
}

impl TableAm for ParquetTable {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    fn capabilities(&self) -> TableCapabilities {
        // Append-only per row — fragments are immutable, so there is no UPDATE and
        // no DELETE — but the whole relation can still be replaced wholesale, which
        // is what TRUNCATE does (a fresh fragment directory swapped in on commit).
        TableCapabilities {
            truncate: true,
            ..TableCapabilities::APPEND_ONLY
        }
    }

    fn indexes(&self) -> Vec<IndexMetadata> {
        self.indexes
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .clone()
    }

    fn statistics(&self) -> RelStats {
        if let Some((_, relpages, reltuples)) = *self
            .analyzed
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
        {
            return RelStats {
                relpages,
                reltuples,
                analyzed: true,
                columns: Vec::new(),
            };
        }
        let Ok(relpages) = self.measure_relpages() else {
            return RelStats::unknown(&self.schema);
        };
        RelStats::from_pages(relpages, &self.schema)
    }

    fn scan(&self, txn: &TxnContext, projection: &ColumnProjection) -> TupleStream {
        match self.scan_in(txn, projection) {
            Ok(scan) => Box::new(scan),
            Err(error) => Box::new(std::iter::once(Err(error))),
        }
    }

    fn supports_batch_scan(&self) -> bool {
        // Every type this format stores has a batch representation except the
        // two it stores as Arrow structs, which no kernel reads. Answering from
        // the schema alone keeps this free of I/O, as the contract requires.
        self.schema
            .columns
            .iter()
            .all(|column| crabgresql_batch::encoding_of(column.ty).is_some())
    }

    fn scan_batches(&self, txn: &TxnContext, req: &ScanRequest) -> Option<Vec<BatchStream>> {
        if !self.supports_batch_scan() {
            return None;
        }
        // A failure to open surfaces as the stream's first item rather than as a
        // `None`, matching `scan`: `None` means "this engine has no columnar
        // path", and reporting an I/O error that way would silently downgrade
        // to a row scan that is about to hit the same error.
        let stream: BatchStream = match self.batch_scan_in(txn, req) {
            Ok(scan) => Box::new(scan),
            Err(error) => Box::new(std::iter::once(Err(error))),
        };
        Some(vec![stream])
    }

    fn fetch(&self, tid: Tid, txn: &TxnContext) -> Result<Option<Tuple>, StorageError> {
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
            let batch = batch.map_err(|error| corrupt(format!("decode Parquet row group: {error}")))?;
            for row in 0..batch.num_rows() {
                if ordinal == tid.offset as u32 {
                    return decode_row(&self.schema, &positions, &batch, row).map(Some);
                }
                ordinal += 1;
            }
        }
        Ok(None)
    }

    fn insert(&self, tuple: Tuple, txn: &TxnContext) -> Result<Tid, StorageError> {
        let mut tids = self.insert_many(vec![tuple], txn)?;
        tids.pop()
            .ok_or_else(|| corrupt("Parquet insert produced no tuple identifier"))
    }

    fn insert_many(
        &self,
        tuples: Vec<Tuple>,
        txn: &TxnContext,
    ) -> Result<Vec<Tid>, StorageError> {
        if tuples.is_empty() {
            return Ok(Vec::new());
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
        let mut next = self
            .next_block
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let mut staged = Vec::new();
        let mut tids = Vec::with_capacity(tuples.len());
        for chunk in tuples.chunks(MAX_FRAGMENT_ROWS) {
            let block = *next;
            // A fragment block is a physical address, so it must stay below the
            // logical-tid flag (see `TID_LOGICAL_FLAG`) — past it, a fragment tid
            // would read as a logical row id and `fetch` would route it wrong.
            *next = next
                .checked_add(1)
                .filter(|next| *next <= MAX_PHYSICAL_BLOCK)
                .ok_or_else(|| io_error("allocate Parquet fragment", "fragment id exhausted"))?;
            let (temp, pending) = match self.write_fragment(rel, &dir, block, chunk, txn) {
                Ok(paths) => paths,
                Err(error) => {
                    let base = format!("{block:08x}-{}-{}", txn.xid.0, txn.cid.0);
                    let _ = std::fs::remove_file(dir.join(format!("{base}.tmp")));
                    for (temp, pending) in &staged {
                        let _ = std::fs::remove_file(temp);
                        let _ = std::fs::remove_file(pending);
                    }
                    return Err(error);
                }
            };
            staged.push((temp, pending));
            tids.extend((1..=chunk.len()).map(|offset| Tid {
                block,
                offset: offset as u16,
            }));
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
            .append(RMGR_PARQUET, PARQUET_XID_OBSERVED, txn.xid, &[]);
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

    fn update(
        &self,
        _tid: Tid,
        _tuple: Tuple,
        _txn: &TxnContext,
    ) -> Result<UpdateResult, StorageError> {
        Err(unsupported(
            "table access method \"parquet\" does not support UPDATE",
        ))
    }

    fn delete(
        &self,
        _tid: Tid,
        _txn: &TxnContext,
    ) -> Result<DeleteResult, StorageError> {
        Err(unsupported(
            "table access method \"parquet\" does not support DELETE",
        ))
    }

    /// Transactional TRUNCATE via a fragment-directory swap — the Parquet twin of
    /// the heap's relfilenode swap. Stages a fresh, empty `parquet/<new>/` and holds
    /// the table exclusively until the transaction ends; the swap is applied on
    /// commit and discarded on abort by [`ParquetTable::finish_transaction`]. The
    /// old directory stays intact until commit, so a rollback or a
    /// crash-before-commit restores every row.
    fn truncate(&self, txn: &TxnContext) -> Result<(), StorageError> {
        // AccessExclusiveLock: block concurrent readers/writers of this table until
        // we commit, so no one reads the directory we are about to remove or writes
        // fragments the swap would drop. Held until txn end.
        self.lock.acquire_exclusive(txn.lock_owner);
        let old = self.effective_rel(txn.xid);
        // A fresh, never-reused relfilenode for the empty post-truncate directory.
        let new = self.relfilenodes.alloc_relfilenode();
        let new_dir = self.dir_of(new);
        match self.stage_truncate(old, new, &new_dir, txn) {
            Ok(()) => Ok(()),
            Err(error) => {
                // Nothing was staged, so nothing will ever release this hold on our
                // behalf — unless a previous TRUNCATE in the same transaction owns
                // it, in which case its commit/abort still does. The empty directory
                // is not named by the catalog: `gc_orphan_parquet_dirs` reclaims it.
                let _ = remove_dir_all_ok(&new_dir);
                if self.staged_truncate(txn.xid).is_none() {
                    self.lock.release_exclusive(txn.lock_owner);
                }
                Err(error)
            }
        }
    }
}

impl ParquetTable {
    /// Create the staged directory, WAL-log the swap and record it in memory.
    /// Split out of [`TableAm::truncate`] so every failure before the state
    /// transition takes the same cleanup path.
    fn stage_truncate(
        &self,
        old: u32,
        new: u32,
        new_dir: &Path,
        txn: &TxnContext,
    ) -> Result<(), StorageError> {
        std::fs::create_dir_all(new_dir)
            .map_err(|error| io_error("create Parquet table directory", error))?;
        sync_dir(&self.root.join("parquet"))?;
        // WAL-log the swap intent {old, new, relation} and flush it. Recovery
        // applies the swap only for a committed XID, so the record is safe to write
        // now; and because it carries the XID, a transaction that only TRUNCATEs is
        // still observed by recovery's XID allocator without a separate
        // `PARQUET_XID_OBSERVED` record.
        let lsn = self.wal.append(
            RMGR_PARQUET,
            PARQUET_TRUNCATE,
            txn.xid,
            &encode_truncate(&self.schema.namespace, &self.schema.name, old, new),
        );
        self.wal
            .flush(lsn)
            .map_err(|error| io_error("flush Parquet TRUNCATE WAL record", error))?;
        self.staged_xids
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .insert(txn.xid);
        let mut pending = self
            .pending
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let mut next = self
            .next_block
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        // A second TRUNCATE in one transaction keeps the FIRST saved counter: abort
        // must restore what the transaction found, not what its own first TRUNCATE
        // left behind (invariant P2).
        let saved = pending
            .as_ref()
            .filter(|p| p.xid == txn.xid)
            .map_or(*next, |p| p.saved_next_block);
        let superseded = pending.replace(PendingTruncate {
            xid: txn.xid,
            new_rel: new,
            owner: txn.lock_owner,
            saved_next_block: saved,
        });
        self.has_pending.store(true, Ordering::Release);
        // The staged directory is empty, so its fragments start from block 1 again.
        *next = 1;
        drop(next);
        drop(pending);
        if let Some(superseded) = superseded {
            // Used only by this uncommitted transaction; reclaim it now.
            let _ = remove_dir_all_ok(&self.dir_of(superseded.new_rel));
        }
        Ok(())
    }
}

/// Replays Parquet WAL records. A [`PARQUET_XID_OBSERVED`] record exists only so
/// the XID allocator observes the transaction — fragment bytes were fsynced before
/// commit and pending-file promotion is reconciled separately. A
/// [`PARQUET_TRUNCATE`] record additionally materializes the staged directory (so
/// the same transaction's later inserts have somewhere to have landed) and records
/// the swap for the engine to resolve once the CLOG is rebuilt.
pub struct ParquetRedo {
    root: PathBuf,
    recovered: Mutex<Vec<RecoveredParquetTruncate>>,
}

impl ParquetRedo {
    pub fn new(data_dir: &Path) -> ParquetRedo {
        ParquetRedo {
            root: data_dir.to_path_buf(),
            recovered: Mutex::new(Vec::new()),
        }
    }

    /// Drain the swaps seen during replay, in WAL order.
    pub fn take_recovered(&self) -> Vec<RecoveredParquetTruncate> {
        std::mem::take(
            &mut *self
                .recovered
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned")),
        )
    }
}

impl RmgrRedo for ParquetRedo {
    fn redo(&self, ctx: &RedoContext) -> Result<(), WalError> {
        match ctx.info {
            PARQUET_XID_OBSERVED if ctx.payload.is_empty() => Ok(()),
            PARQUET_TRUNCATE => {
                let record = decode_truncate(ctx.xid, ctx.payload)?;
                let dir = self.root.join("parquet").join(record.new.to_string());
                std::fs::create_dir_all(&dir).map_err(|error| {
                    WalError::Redo(format!(
                        "create Parquet directory {}: {error}",
                        dir.display()
                    ))
                })?;
                self.recovered
                    .lock()
                    .unwrap_or_else(|_| panic!("mutex poisoned"))
                    .push(record);
                Ok(())
            }
            other => Err(WalError::Redo(format!(
                "unknown parquet WAL record info byte {other:#x}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{File, OpenOptions};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use crabgresql_storage_api::{
        Column, ColumnProjection, StorageError, TableAccessMethod, TableAm, TableSchema, Tid, Tuple,
    };
    use crabgresql_txn::{
        Clog, CommandId, CommitSink, TransactionManager, Xid,
    };
    use crabgresql_types::numeric::Numeric;
    use crabgresql_types::{Interval, PgType, TimeTz, Value};
    use crabgresql_wal::{RmgrRegistry, Wal, recover};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use parquet::basic::Compression;

    use super::{ParquetTable, RelfilenodeAllocator};

    /// A relfilenode counter for tests. It starts far above the ids the tests
    /// assign by hand, so a directory staged by a TRUNCATE can never collide with
    /// one of them.
    struct Counter(std::sync::atomic::AtomicU32);

    impl RelfilenodeAllocator for Counter {
        fn alloc_relfilenode(&self) -> u32 {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        }
    }

    fn open_table(
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
    fn finish(
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

    fn manager(wal: &Arc<Wal>) -> TransactionManager {
        let sink: Arc<dyn CommitSink> = Arc::clone(wal) as Arc<dyn CommitSink>;
        TransactionManager::new_recovered(
            sink,
            Arc::new(Clog::new()),
            Xid::FIRST_NORMAL,
        )
    }

    fn schema(name: &str, types: &[PgType]) -> TableSchema {
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

    fn parquet_files(dir: &Path, rel: u32) -> anyhow::Result<Vec<PathBuf>> {
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

    #[test]
    fn supported_values_round_trip_and_file_has_only_user_columns() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = schema(
            "types",
            &[
                PgType::Bool,
                PgType::Int2,
                PgType::Int4,
                PgType::Int8,
                PgType::Float4,
                PgType::Float8,
                PgType::Numeric,
                PgType::Text,
                PgType::Varchar,
                PgType::Bpchar,
                PgType::Name,
                PgType::Bytea,
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
        let row = vec![
            Value::Bool(true),
            Value::Int2(-2),
            Value::Int4(42),
            Value::Int8(9_000_000_000),
            Value::Float4(1.25),
            Value::Float8(-2.5),
            Value::Numeric(Numeric::parse("1234567890.012300")?),
            Value::Text("hello".to_string()),
            Value::Text("varchar".to_string()),
            Value::Text("bpchar".to_string()),
            Value::Text("name".to_string()),
            Value::Bytea(vec![0, 1, 255]),
            Value::Uuid([0x42; 16]),
            Value::Date(9_000),
            Value::Time(12_345_678),
            Value::TimeTz(TimeTz {
                usec: 45_000_000,
                zone: 3_600,
            }),
            Value::Timestamp(123_456_789),
            Value::TimestampTz(-987_654_321),
            Value::Interval(Interval {
                months: 14,
                days: -3,
                usec: 777,
            }),
        ];
        let expected_column_count = row.len();
        let nulls = vec![Value::Null; row.len()];
        let xid = tm.allocate_xid();
        table.insert_many(
            vec![row.clone(), nulls.clone()],
            &tm.context(xid, CommandId::FIRST),
        )?;

        assert_eq!(
            table
                .scan(&tm.context(xid, CommandId::FIRST), &ColumnProjection::All)
                .count(),
            0,
            "a statement cannot see its own inserts before the command counter advances"
        );
        let own_rows: Vec<Tuple> = table
            .scan(&tm.context(xid, CommandId(1)), &ColumnProjection::All)
            .map(|result| result.map(|(_, tuple)| tuple))
            .collect::<Result<_, _>>()?;
        assert_eq!(own_rows, vec![row.clone(), nulls.clone()]);
        assert_eq!(
            table
                .scan(&tm.context(Xid::INVALID, CommandId::FIRST), &ColumnProjection::All)
                .count(),
            0
        );

        tm.commit(xid)?;
        finish(&table, xid, true)?;
        let rows: Vec<Tuple> = table
            .scan(&tm.context(Xid::INVALID, CommandId::FIRST), &ColumnProjection::All)
            .map(|result| result.map(|(_, tuple)| tuple))
            .collect::<Result<_, _>>()?;
        assert_eq!(rows, vec![row, nulls]);

        let files = parquet_files(dir.path(), 1)?;
        assert_eq!(files.len(), 1);
        let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(&files[0])?)?;
        assert_eq!(builder.schema().fields().len(), expected_column_count);
        assert!(
            builder
                .schema()
                .fields()
                .iter()
                .enumerate()
                .all(|(index, field)| field.name() == &format!("c{index}"))
        );
        assert_eq!(
            builder.metadata().row_group(0).column(0).compression(),
            Compression::SNAPPY
        );
        let metadata = builder
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .ok_or_else(|| anyhow::anyhow!("missing Parquet footer metadata"))?;
        assert!(
            metadata
                .iter()
                .any(|item| item.key == super::META_XMIN && item.value.as_deref() == Some("3"))
        );
        Ok(())
    }

    /// Every row a batch scan produces, rebuilt as a full-width tuple in scan
    /// order — the shape a row scan returns, so the two can be compared directly.
    fn batch_scan_rows(
        table: &ParquetTable,
        txn: &crabgresql_txn::TxnContext,
        projection: ColumnProjection,
    ) -> anyhow::Result<Vec<Tuple>> {
        let width = table.schema().columns.len();
        let streams = table
            .scan_batches(txn, &crabgresql_storage_api::ScanRequest::new(projection))
            .ok_or_else(|| anyhow::anyhow!("engine declined a batch scan"))?;
        let mut rows = Vec::new();
        for stream in streams {
            for batch in stream {
                let batch = batch?;
                for row in 0..batch.len() {
                    let mut tuple = Vec::new();
                    batch.row_into(row, width, &mut tuple)?;
                    rows.push(tuple);
                }
            }
        }
        Ok(rows)
    }

    /// The load-bearing equivalence: a batch scan and a row scan of the same
    /// relation under the same snapshot must agree value for value.
    ///
    /// Every type the format stores is present, including the three whose batch
    /// representation differs from their stored one. `date` and `timestamp` are
    /// the reason this test exists — a missing epoch rebase shifts them by
    /// exactly 30 years while preserving order, so nothing else would notice.
    #[test]
    fn a_batch_scan_returns_exactly_what_a_row_scan_returns() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = schema(
            "types",
            &[
                PgType::Bool,
                PgType::Int2,
                PgType::Int4,
                PgType::Int8,
                PgType::Float4,
                PgType::Float8,
                PgType::Numeric,
                PgType::Text,
                PgType::Bytea,
                PgType::Uuid,
                PgType::Date,
                PgType::Time,
                PgType::Timestamp,
                PgType::TimestampTz,
            ],
        );
        let table = open_table(dir.path(), 1, schema, Arc::clone(&wal))?;
        let row = |date: i32, timestamp: i64| {
            vec![
                Value::Bool(true),
                Value::Int2(-2),
                Value::Int4(42),
                Value::Int8(9_000_000_000),
                Value::Float4(1.25),
                // Signed zero and NaN survive the round trip untouched; the
                // comparison kernels, not the scan, reconcile them.
                Value::Float8(-0.0),
                Value::Numeric(Numeric::parse("1.00").expect("numeric")),
                Value::Text("hello".to_string()),
                Value::Bytea(vec![0, 1, 255]),
                Value::Uuid([0x42; 16]),
                Value::Date(date),
                Value::Time(12_345_678),
                Value::Timestamp(timestamp),
                Value::TimestampTz(-987_654_321),
            ]
        };
        let rows = vec![
            // An ordinary date, the PostgreSQL epoch itself, and both infinity
            // sentinels — which must pass through unshifted.
            row(4_930, 123_456_789),
            row(0, 0),
            row(i32::MAX, i64::MAX),
            row(i32::MIN, i64::MIN),
            vec![Value::Null; 14],
        ];
        let xid = tm.allocate_xid();
        table.insert_many(rows.clone(), &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        finish(&table, xid, true)?;

        let txn = tm.context(Xid::INVALID, CommandId::FIRST);
        let by_row: Vec<Tuple> = table
            .scan(&txn, &ColumnProjection::All)
            .map(|result| result.map(|(_, tuple)| tuple))
            .collect::<Result<_, _>>()?;
        assert_eq!(by_row, rows, "the row scan itself must round-trip");

        let by_batch = batch_scan_rows(&table, &txn, ColumnProjection::All)?;
        assert_eq!(by_batch, by_row, "batch and row scans must agree");
        Ok(())
    }

    /// The same equivalence under a projection, where the batch is narrow and
    /// the tuple stays full width with unread slots left null.
    #[test]
    fn a_projected_batch_scan_agrees_with_a_projected_row_scan() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = schema(
            "t",
            &[PgType::Int4, PgType::Text, PgType::Date, PgType::Int8],
        );
        let table = open_table(dir.path(), 1, schema.clone(), Arc::clone(&wal))?;
        let rows: Vec<Tuple> = (0..3)
            .map(|i| {
                vec![
                    Value::Int4(i),
                    Value::Text(format!("row{i}")),
                    Value::Date(4_930 + i),
                    Value::Int8(i64::from(i) * 100),
                ]
            })
            .collect();
        let xid = tm.allocate_xid();
        table.insert_many(rows, &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        finish(&table, xid, true)?;

        let txn = tm.context(Xid::INVALID, CommandId::FIRST);
        // Deliberately not adjacent and not starting at zero, so a scan that
        // returned columns in reader order rather than schema order would show.
        let projection = ColumnProjection::of([2, 0], &schema);
        let by_row: Vec<Tuple> = table
            .scan(&txn, &projection)
            .map(|result| result.map(|(_, tuple)| tuple))
            .collect::<Result<_, _>>()?;
        let by_batch = batch_scan_rows(&table, &txn, projection)?;
        assert_eq!(by_batch, by_row);
        // And the projection really did narrow: the unread columns are null.
        assert!(by_batch.iter().all(|row| row[1] == Value::Null && row[3] == Value::Null));
        Ok(())
    }

    /// A batch scan must not see rows a row scan cannot, so both go through the
    /// same `visible_fragments`. Checked at the two boundaries that matter: a
    /// statement's own uncommitted insert, and another transaction's.
    #[test]
    fn a_batch_scan_obeys_the_same_snapshot_as_a_row_scan() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("t", &[PgType::Int4]), Arc::clone(&wal))?;
        let xid = tm.allocate_xid();
        table.insert_many(vec![vec![Value::Int4(1)]], &tm.context(xid, CommandId::FIRST))?;

        // Assert against the row scan rather than against a literal, so the
        // property under test is "the two agree" rather than a restatement of
        // MVCC that could drift from it.
        let mut agree = |txn: &crabgresql_txn::TxnContext, expected: usize| -> anyhow::Result<()> {
            let by_row = table.scan(txn, &ColumnProjection::All).count();
            let by_batch = batch_scan_rows(&table, txn, ColumnProjection::All)?.len();
            assert_eq!((by_row, by_batch), (expected, expected));
            Ok(())
        };

        // Its own insert, before and after the command counter advances.
        agree(&tm.context(xid, CommandId::FIRST), 0)?;
        agree(&tm.context(xid, CommandId(1)), 1)?;
        // Another transaction, while the insert is still uncommitted.
        let before_commit = tm.context(Xid::INVALID, CommandId::FIRST);
        agree(&before_commit, 0)?;

        tm.commit(xid)?;
        finish(&table, xid, true)?;
        // A snapshot taken before the commit still cannot see it...
        agree(&before_commit, 0)?;
        // ...but one taken after can.
        agree(&tm.context(Xid::INVALID, CommandId::FIRST), 1)?;
        Ok(())
    }

    /// Rows must arrive in the same order across fragments, since a `GROUP BY`
    /// with no `ORDER BY` reports groups in first-seen order.
    #[test]
    fn a_batch_scan_preserves_row_order_across_fragments() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("t", &[PgType::Int4]), Arc::clone(&wal))?;
        for value in 0..3 {
            let xid = tm.allocate_xid();
            table.insert_many(
                vec![vec![Value::Int4(value)]],
                &tm.context(xid, CommandId::FIRST),
            )?;
            tm.commit(xid)?;
            finish(&table, xid, true)?;
        }
        let txn = tm.context(Xid::INVALID, CommandId::FIRST);
        let by_row: Vec<Tuple> = table
            .scan(&txn, &ColumnProjection::All)
            .map(|result| result.map(|(_, tuple)| tuple))
            .collect::<Result<_, _>>()?;
        assert_eq!(by_row.len(), 3, "expected one fragment per transaction");
        assert_eq!(batch_scan_rows(&table, &txn, ColumnProjection::All)?, by_row);
        Ok(())
    }

    /// Every type this format can store also has a batch representation, so no
    /// Parquet relation is ever denied the columnar path by its schema.
    ///
    /// The two whitelists are maintained separately — `supports_type` here and
    /// `encoding_of` in `crabgresql-batch` — so this pins the relationship
    /// between them. If a type is ever added to one and not the other, a scan
    /// would decline for a reason nobody chose; this fails first.
    ///
    /// Note the whitelists agree without being identical: `interval` and
    /// `timetz` are *representable* in a batch (as Arrow structs) but no kernel
    /// computes on them. Refusing them is the gate's job, not the scan's.
    #[test]
    fn every_storable_type_can_also_be_scanned_as_batches() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let storable = [
            PgType::Bool,
            PgType::Int2,
            PgType::Int4,
            PgType::Int8,
            PgType::Float4,
            PgType::Float8,
            PgType::Numeric,
            PgType::Text,
            PgType::Varchar,
            PgType::Bpchar,
            PgType::Name,
            PgType::Bytea,
            PgType::Uuid,
            PgType::Date,
            PgType::Time,
            PgType::TimeTz,
            PgType::Timestamp,
            PgType::TimestampTz,
            PgType::Interval,
        ];
        assert!(
            storable.iter().all(|ty| super::supports_type(*ty)),
            "the storable list has drifted from `supports_type`"
        );
        let table = open_table(dir.path(), 1, schema("t", &storable), Arc::clone(&wal))?;
        assert!(table.supports_batch_scan());
        let txn = tm.context(Xid::INVALID, CommandId::FIRST);
        assert!(
            table
                .scan_batches(
                    &txn,
                    &crabgresql_storage_api::ScanRequest::new(ColumnProjection::All)
                )
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn inserts_split_at_fragment_limit_and_tids_fetch_stably() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("many", &[PgType::Int4]), Arc::clone(&wal))?;
        let xid = tm.allocate_xid();
        let tuples = (0..=u16::MAX as i32)
            .map(|value| vec![Value::Int4(value)])
            .collect();
        let tids = table.insert_many(tuples, &tm.context(xid, CommandId::FIRST))?;
        assert_eq!(tids.len(), u16::MAX as usize + 1);
        assert_eq!(tids[0], Tid::new(1, 1));
        assert_eq!(tids[u16::MAX as usize - 1], Tid::new(1, u16::MAX));
        assert_eq!(tids[u16::MAX as usize], Tid::new(2, 1));
        tm.commit(xid)?;
        finish(&table, xid, true)?;
        assert_eq!(parquet_files(dir.path(), 1)?.len(), 2);
        assert_eq!(
            table.fetch(
                Tid::new(2, 1),
                &tm.context(Xid::INVALID, CommandId::FIRST)
            )?,
            Some(vec![Value::Int4(u16::MAX as i32)])
        );
        Ok(())
    }

    #[test]
    fn aborted_pending_fragments_are_removed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("aborted", &[PgType::Int4]), Arc::clone(&wal))?;
        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(xid, CommandId::FIRST))?;
        tm.abort(xid);
        finish(&table, xid, false)?;
        assert!(parquet_files(dir.path(), 1)?.is_empty());
        assert_eq!(
            table
                .scan(&tm.context(Xid::INVALID, CommandId::FIRST), &ColumnProjection::All)
                .count(),
            0
        );
        Ok(())
    }

    #[test]
    fn recovery_reconciles_pending_fragments_and_observes_xids() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let committed = open_table(dir.path(), 1, schema("committed", &[PgType::Int4]), Arc::clone(&wal))?;
        let committed_xid = tm.allocate_xid();
        committed.insert(
            vec![Value::Int4(1)],
            &tm.context(committed_xid, CommandId::FIRST),
        )?;
        tm.commit(committed_xid)?;

        let interrupted = open_table(dir.path(), 2, schema("interrupted", &[PgType::Int4]), Arc::clone(&wal))?;
        let interrupted_xid = tm.allocate_xid();
        interrupted.insert(
            vec![Value::Int4(2)],
            &tm.context(interrupted_xid, CommandId::FIRST),
        )?;
        drop(committed);
        drop(interrupted);
        drop(tm);
        drop(wal);

        let recovered_wal = Arc::new(Wal::open(dir.path())?);
        let mut registry = RmgrRegistry::new();
        registry.register(
            super::RMGR_PARQUET,
            Arc::new(super::ParquetRedo::new(dir.path())),
        );
        let clog = Arc::new(Clog::new());
        let result = recover(dir.path(), &registry, &clog)?;
        assert!(result.next_xid > interrupted_xid);

        let committed = open_table(dir.path(), 1, schema("committed", &[PgType::Int4]), Arc::clone(&recovered_wal))?;
        committed.recover(&clog)?;
        let interrupted = open_table(dir.path(), 2, schema("interrupted", &[PgType::Int4]), recovered_wal)?;
        interrupted.recover(&clog)?;
        assert_eq!(parquet_files(dir.path(), 1)?.len(), 1);
        assert!(parquet_files(dir.path(), 2)?.is_empty());
        Ok(())
    }

    /// A scan lists fragments up front but opens them lazily, and a fragment
    /// becomes MVCC-visible the moment its transaction is marked committed —
    /// before the finalize hook renames `.pending` away. The reader must follow
    /// the promotion rather than failing the query with a spurious ENOENT.
    #[test]
    fn scan_follows_a_fragment_promoted_after_it_was_listed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("promoted", &[PgType::Int4]), Arc::clone(&wal))?;
        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(7)], &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;

        // Snapshot the fragment list (still `.pending`) before the rename lands,
        // exactly as a concurrent session's scan would.
        let scan = table.scan(&tm.context(Xid::INVALID, CommandId::FIRST), &ColumnProjection::All);
        finish(&table, xid, true)?;
        let rows = scan.collect::<Result<Vec<_>, _>>()?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, vec![Value::Int4(7)]);
        Ok(())
    }

    /// `recover` must reconcile *every* pending transaction it finds, including
    /// when several interleave in the same directory. Reconciling from a live
    /// `read_dir` stream while renaming/unlinking entries could skip some.
    #[test]
    fn recover_reconciles_every_pending_transaction() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("interleaved", &[PgType::Int4]), Arc::clone(&wal))?;
        // Interleave several fragments from two transactions, leaving all of
        // them `.pending` as an interrupted run would.
        let (first, second) = (tm.allocate_xid(), tm.allocate_xid());
        for value in 0..8 {
            let xid = if value % 2 == 0 { first } else { second };
            table.insert(
                vec![Value::Int4(value)],
                &tm.context(xid, CommandId::FIRST),
            )?;
        }
        assert!(parquet_files(dir.path(), 1)?.is_empty());

        let clog = Clog::new();
        clog.set_committed(first);
        clog.set_aborted(second);
        table.recover(&clog)?;

        // The committed transaction's four fragments were promoted; the aborted
        // transaction's four were unlinked. Neither was left half-reconciled.
        assert_eq!(parquet_files(dir.path(), 1)?.len(), 4);
        let table_dir = dir.path().join("parquet").join("1");
        let pending = std::fs::read_dir(&table_dir)?
            .filter(|entry| {
                entry.as_ref().is_ok_and(|entry| {
                    entry.file_name().to_string_lossy().ends_with(".pending")
                })
            })
            .count();
        assert_eq!(pending, 0);
        Ok(())
    }

    /// The finalize hook runs over every open Parquet table on every transaction
    /// end, so a table the transaction never wrote to must answer from memory —
    /// no directory scan, no fsync. Deleting the directory makes any filesystem
    /// access observable as an error.
    #[test]
    fn finish_transaction_skips_tables_the_xid_never_wrote() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("untouched", &[PgType::Int4]), Arc::clone(&wal))?;
        std::fs::remove_dir_all(dir.path().join("parquet").join("1"))?;
        let xid = tm.allocate_xid();
        finish(&table, xid, true)?;
        finish(&table, xid, false)?;
        Ok(())
    }

    /// `statistics()` intentionally returns the last ANALYZE's cached numbers,
    /// so ANALYZE itself must re-measure — otherwise `relpages` freezes at the
    /// first value recorded and never tracks the table's growth.
    #[test]
    fn measure_relpages_ignores_the_cached_analyze_result() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("stats", &[PgType::Int4]), Arc::clone(&wal))?;
        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        finish(&table, xid, true)?;

        let measured = table.measure_relpages()?;
        table.set_analyzed(9_999, 1.0);
        assert_eq!(table.statistics().relpages, 9_999, "cache serves statistics");
        assert_eq!(
            table.measure_relpages()?,
            measured,
            "ANALYZE re-measures instead of reading its own cached value back"
        );
        Ok(())
    }

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
            .scan(&tm.context(Xid::INVALID, CommandId::FIRST), &ColumnProjection::All)
            .next()
            .ok_or_else(|| anyhow::anyhow!("corrupt scan returned no item"))?
            .expect_err("truncated fragment must return an error");
        assert!(matches!(error, StorageError::CorruptData(_)));
        Ok(())
    }

    /// Every `parquet/<n>` directory currently on disk, as relfilenodes.
    fn fragment_dirs(dir: &Path) -> anyhow::Result<Vec<u32>> {
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

    fn scan_values(table: &ParquetTable, txn: &crabgresql_txn::TxnContext) -> Vec<i32> {
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

    #[test]
    fn truncate_commit_empties_the_table_and_swaps_the_directory() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("t", &[PgType::Int4]), Arc::clone(&wal))?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        let truncater = tm.allocate_xid();
        table.truncate(&tm.context(truncater, CommandId::FIRST))?;
        // Read-your-own-truncate: the truncating transaction sees an empty table.
        // (Another session cannot look at all while the TRUNCATE is staged — its
        // AccessShare hold waits for the AccessExclusive one, as in PostgreSQL.)
        assert!(scan_values(&table, &tm.context(truncater, CommandId::FIRST)).is_empty());
        tm.commit(truncater)?;
        let swapped = finish(&table, truncater, true)?
            .ok_or_else(|| anyhow::anyhow!("a committed TRUNCATE must report its new relfilenode"))?;

        assert_eq!(table.relfilenode(), swapped);
        assert_eq!(fragment_dirs(dir.path())?, vec![swapped], "old directory gone");
        assert!(scan_values(&table, &tm.context(Xid::INVALID, CommandId::FIRST)).is_empty());
        Ok(())
    }

    #[test]
    fn truncate_abort_restores_every_row_and_removes_the_staged_directory() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("t", &[PgType::Int4]), Arc::clone(&wal))?;
        let loader = tm.allocate_xid();
        table.insert_many(
            vec![vec![Value::Int4(1)], vec![Value::Int4(2)]],
            &tm.context(loader, CommandId::FIRST),
        )?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        let truncater = tm.allocate_xid();
        table.truncate(&tm.context(truncater, CommandId::FIRST))?;
        tm.abort(truncater);
        assert_eq!(finish(&table, truncater, false)?, None);

        assert_eq!(table.relfilenode(), 1);
        assert_eq!(fragment_dirs(dir.path())?, vec![1], "staged directory gone");
        assert_eq!(
            scan_values(&table, &tm.context(Xid::INVALID, CommandId::FIRST)),
            vec![1, 2]
        );
        Ok(())
    }

    /// Invariant P1: a post-TRUNCATE insert must stamp the *staged* directory's
    /// relfilenode into its footer, or reading it back reports valid bytes as
    /// corrupt — including through a freshly opened handle after a restart.
    #[test]
    fn post_truncate_fragments_carry_the_staged_relfilenode() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("t", &[PgType::Int4]), Arc::clone(&wal))?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        let xid = tm.allocate_xid();
        table.truncate(&tm.context(xid, CommandId::FIRST))?;
        table.insert(vec![Value::Int4(7)], &tm.context(xid, CommandId(1)))?;
        // Visible to its own transaction from the staged directory, before commit.
        assert_eq!(scan_values(&table, &tm.context(xid, CommandId(2))), vec![7]);
        tm.commit(xid)?;
        let swapped = finish(&table, xid, true)?
            .ok_or_else(|| anyhow::anyhow!("missing swap"))?;

        let file = parquet_files(dir.path(), swapped)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing promoted fragment"))?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(&file)?)?;
        let stamped = reader
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .and_then(|kv| {
                kv.iter()
                    .find(|item| item.key == super::META_REL)
                    .and_then(|item| item.value.clone())
            })
            .ok_or_else(|| anyhow::anyhow!("fragment has no relfilenode metadata"))?;
        assert_eq!(stamped, swapped.to_string());

        // A fresh handle over the swapped directory reads the same row back.
        let reopened = open_table(dir.path(), swapped, schema("t", &[PgType::Int4]), wal)?;
        assert_eq!(
            scan_values(&reopened, &tm.context(Xid::INVALID, CommandId::FIRST)),
            vec![7]
        );
        Ok(())
    }

    /// Invariant P2, abort half: the block counter must return to what the
    /// transaction found, or a later insert re-issues a block an existing fragment
    /// in the surviving directory already owns — duplicate TIDs.
    #[test]
    fn aborted_truncate_restores_the_block_counter() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("t", &[PgType::Int4]), Arc::clone(&wal))?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        table.insert(vec![Value::Int4(2)], &tm.context(loader, CommandId(1)))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        let truncater = tm.allocate_xid();
        table.truncate(&tm.context(truncater, CommandId::FIRST))?;
        // Two TRUNCATEs in one transaction must still restore the FIRST counter.
        table.truncate(&tm.context(truncater, CommandId(1)))?;
        tm.abort(truncater);
        finish(&table, truncater, false)?;

        let writer = tm.allocate_xid();
        let tids = table.insert(vec![Value::Int4(3)], &tm.context(writer, CommandId::FIRST))?;
        assert_eq!(tids, Tid::new(3, 1), "block 1 and 2 are still occupied");
        tm.commit(writer)?;
        finish(&table, writer, true)?;
        assert_eq!(
            scan_values(&table, &tm.context(Xid::INVALID, CommandId::FIRST)),
            vec![1, 2, 3]
        );
        assert_eq!(fragment_dirs(dir.path())?, vec![1]);
        Ok(())
    }

    /// Invariant P2, commit half: the counter must NOT restart, or the next insert
    /// collides with the fragments the truncating transaction itself wrote.
    #[test]
    fn committed_truncate_keeps_the_counter_its_own_inserts_advanced() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("t", &[PgType::Int4]), Arc::clone(&wal))?;
        let xid = tm.allocate_xid();
        table.truncate(&tm.context(xid, CommandId::FIRST))?;
        assert_eq!(
            table.insert(vec![Value::Int4(1)], &tm.context(xid, CommandId(1)))?,
            Tid::new(1, 1)
        );
        tm.commit(xid)?;
        finish(&table, xid, true)?;

        let writer = tm.allocate_xid();
        assert_eq!(
            table.insert(vec![Value::Int4(2)], &tm.context(writer, CommandId::FIRST))?,
            Tid::new(2, 1),
            "the post-truncate insert already owns block 1"
        );
        tm.commit(writer)?;
        finish(&table, writer, true)?;
        assert_eq!(
            scan_values(&table, &tm.context(Xid::INVALID, CommandId::FIRST)),
            vec![1, 2]
        );
        Ok(())
    }

    /// `INSERT; TRUNCATE; ROLLBACK`: the pending fragments the transaction staged
    /// *before* its TRUNCATE live in the surviving directory and must be unlinked
    /// there, not promoted.
    #[test]
    fn aborted_insert_then_truncate_unlinks_the_pre_truncate_fragments() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("t", &[PgType::Int4]), Arc::clone(&wal))?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(2)], &tm.context(xid, CommandId::FIRST))?;
        table.truncate(&tm.context(xid, CommandId(1)))?;
        tm.abort(xid);
        finish(&table, xid, false)?;

        assert_eq!(fragment_dirs(dir.path())?, vec![1]);
        assert_eq!(
            parquet_files(dir.path(), 1)?.len(),
            1,
            "only the committed loader's fragment remains"
        );
        assert_eq!(
            std::fs::read_dir(dir.path().join("parquet").join("1"))?.count(),
            1,
            "the aborted transaction's .pending fragment was unlinked"
        );
        Ok(())
    }

    /// A superseded staged directory is reclaimed as soon as the next TRUNCATE in
    /// the same transaction replaces it.
    #[test]
    fn double_truncate_in_one_transaction_reclaims_the_superseded_directory()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("t", &[PgType::Int4]), Arc::clone(&wal))?;
        let xid = tm.allocate_xid();
        table.truncate(&tm.context(xid, CommandId::FIRST))?;
        let first_staged = fragment_dirs(dir.path())?;
        assert_eq!(first_staged.len(), 2, "live plus staged");
        table.truncate(&tm.context(xid, CommandId(1)))?;
        let second_staged = fragment_dirs(dir.path())?;
        assert_eq!(second_staged.len(), 2, "the superseded directory is gone");
        assert_ne!(first_staged, second_staged);
        tm.commit(xid)?;
        let swapped = finish(&table, xid, true)?
            .ok_or_else(|| anyhow::anyhow!("missing swap"))?;
        assert_eq!(fragment_dirs(dir.path())?, vec![swapped]);
        Ok(())
    }

    #[test]
    fn truncate_resets_the_analyze_cache_on_commit_only() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("t", &[PgType::Int4]), Arc::clone(&wal))?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;
        table.set_analyzed(7, 1.0);

        let rolled_back = tm.allocate_xid();
        table.truncate(&tm.context(rolled_back, CommandId::FIRST))?;
        assert_eq!(table.statistics().relpages, 7, "still cached while staged");
        tm.abort(rolled_back);
        finish(&table, rolled_back, false)?;
        assert_eq!(table.statistics().relpages, 7, "an abort changes nothing");

        let committed = tm.allocate_xid();
        table.truncate(&tm.context(committed, CommandId::FIRST))?;
        tm.commit(committed)?;
        finish(&table, committed, true)?;
        let stats = table.statistics();
        assert!(!stats.analyzed, "back to never-analyzed, as PostgreSQL reports");
        assert_eq!(stats.relpages, 0);
        Ok(())
    }

    /// A measurement taken inside a transaction covers that transaction's own
    /// not-yet-committed fragments (the same rows it counts). If the transaction
    /// rolls back, those fragments are unlinked, so the cached result describes
    /// bytes that no longer exist and must be dropped.
    #[test]
    fn a_rollback_discards_statistics_measured_over_its_own_fragments() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("t", &[PgType::Int4]), Arc::clone(&wal))?;

        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(xid, CommandId::FIRST))?;
        let (relpages, reltuples) = table.measure(&tm.context(xid, CommandId(1)))?;
        assert!(relpages > 0, "its own pending fragment is measured");
        assert_eq!(reltuples, 1.0);
        table.set_analyzed_by(xid, relpages, reltuples);

        tm.abort(xid);
        finish(&table, xid, false)?;
        let stats = table.statistics();
        assert!(
            !stats.analyzed,
            "the rolled-back measurement must not survive: {stats:?}"
        );
        assert_eq!(stats.relpages, 0);
        Ok(())
    }

    /// The same rule for a rolled-back TRUNCATE: a measurement the transaction took
    /// of its staged (empty) directory goes with it, while one taken before the
    /// TRUNCATE still describes the directory that survives.
    #[test]
    fn a_rolled_back_truncate_discards_only_its_own_measurement() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("t", &[PgType::Int4]), Arc::clone(&wal))?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        // Measured before any TRUNCATE: still valid after one rolls back.
        let (relpages, reltuples) = table.measure(&tm.context(Xid::INVALID, CommandId::FIRST))?;
        table.set_analyzed(relpages, reltuples);
        let rolled_back = tm.allocate_xid();
        table.truncate(&tm.context(rolled_back, CommandId::FIRST))?;
        tm.abort(rolled_back);
        finish(&table, rolled_back, false)?;
        let stats = table.statistics();
        assert!(stats.analyzed, "an older measurement survives an abort");
        assert_eq!((stats.relpages, stats.reltuples), (relpages, reltuples));

        // Measured by the truncating transaction, against the staged empty
        // directory: discarded when that directory is.
        let second = tm.allocate_xid();
        table.truncate(&tm.context(second, CommandId::FIRST))?;
        let (staged_pages, staged_rows) = table.measure(&tm.context(second, CommandId(1)))?;
        assert_eq!((staged_pages, staged_rows), (0, 0.0));
        table.set_analyzed_by(second, staged_pages, staged_rows);
        tm.abort(second);
        finish(&table, second, false)?;
        assert!(
            !table.statistics().analyzed,
            "a measurement of the discarded directory must not outlive it"
        );
        Ok(())
    }

    /// ANALYZE inside an uncommitted TRUNCATE must measure ONE directory: pairing
    /// the staged directory's row count with the old one's page count would persist
    /// statistics describing a relation that never existed.
    #[test]
    fn measure_inside_an_uncommitted_truncate_sees_only_the_staged_directory()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("t", &[PgType::Int4]), Arc::clone(&wal))?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;
        let (loaded_pages, loaded_rows) = table.measure(&tm.context(Xid::INVALID, CommandId::FIRST))?;
        assert!(loaded_pages > 0);
        assert_eq!(loaded_rows, 1.0);

        let xid = tm.allocate_xid();
        table.truncate(&tm.context(xid, CommandId::FIRST))?;
        assert_eq!(table.measure(&tm.context(xid, CommandId(1)))?, (0, 0.0));
        // And once the TRUNCATE rolls back, the old measurement is what everyone
        // (including another session, which may now take the lock) sees again.
        tm.abort(xid);
        finish(&table, xid, false)?;
        assert_eq!(
            table.measure(&tm.context(Xid::INVALID, CommandId::FIRST))?,
            (loaded_pages, loaded_rows)
        );
        Ok(())
    }

    #[test]
    fn drop_storage_removes_the_staged_directory_too() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("t", &[PgType::Int4]), Arc::clone(&wal))?;
        let xid = tm.allocate_xid();
        table.truncate(&tm.context(xid, CommandId::FIRST))?;
        assert_eq!(fragment_dirs(dir.path())?.len(), 2);
        table.drop_storage()?;
        assert!(fragment_dirs(dir.path())?.is_empty());
        Ok(())
    }

    /// The same owner may TRUNCATE a table it is already scanning (lock upgrade),
    /// while another owner's in-flight scan blocks the TRUNCATE until it finishes.
    #[test]
    fn truncate_upgrades_over_its_own_scan_and_waits_for_a_foreign_one()
    -> anyhow::Result<()> {
        use crabgresql_txn::LockOwner;
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = Arc::new(open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?);
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        // Same owner: an open scan does not self-deadlock the TRUNCATE.
        let own = tm.allocate_xid();
        let mut own_ctx = tm.context(own, CommandId::FIRST);
        own_ctx.lock_owner = LockOwner(42);
        let cursor = table.scan(&own_ctx, &ColumnProjection::All);
        table.truncate(&own_ctx)?;
        drop(cursor);
        tm.abort(own);
        finish(&table, own, false)?;

        // Foreign owner: the TRUNCATE must wait for the scan to be dropped.
        let mut reader_ctx = tm.context(Xid::INVALID, CommandId::FIRST);
        reader_ctx.lock_owner = LockOwner(7);
        let cursor = table.scan(&reader_ctx, &ColumnProjection::All);
        let truncater = tm.allocate_xid();
        let mut truncate_ctx = tm.context(truncater, CommandId::FIRST);
        truncate_ctx.lock_owner = LockOwner(8);
        let (tx, rx) = mpsc::channel();
        let worker = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || {
                let outcome = table.truncate(&truncate_ctx);
                tx.send(()).expect("send");
                outcome
            })
        };
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "TRUNCATE must wait for a foreign owner's open scan"
        );
        drop(cursor);
        rx.recv_timeout(Duration::from_secs(10))
            .expect("TRUNCATE must proceed once the scan is dropped");
        worker.join().expect("worker panicked")?;
        Ok(())
    }

    /// Invariant P1 under contention: a reader that parks in `acquire_shared` while
    /// a TRUNCATE holds the table must read the directory that exists when it is
    /// finally granted the hold — not the one it saw before waiting. Resolving the
    /// relfilenode before the lock made a plain scan of healthy data fail with
    /// `CorruptData`, because the footer stamp belongs to the new generation.
    #[test]
    fn a_scan_that_waits_for_a_truncate_reads_the_swapped_in_directory()
    -> anyhow::Result<()> {
        use crabgresql_txn::LockOwner;
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = Arc::new(open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?);
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        // The truncater commits (so the CLOG already says committed and its rows are
        // MVCC-visible) but has not been finalized yet, so it still holds the table.
        let truncater = tm.allocate_xid();
        let mut truncate_ctx = tm.context(truncater, CommandId::FIRST);
        truncate_ctx.lock_owner = LockOwner(8);
        table.truncate(&truncate_ctx)?;
        table.insert(vec![Value::Int4(7)], &{
            let mut ctx = tm.context(truncater, CommandId(1));
            ctx.lock_owner = LockOwner(8);
            ctx
        })?;
        tm.commit(truncater)?;

        // A foreign reader whose snapshot sees the committed truncater, parked on the
        // exclusive hold.
        let mut reader_ctx = tm.context(Xid::INVALID, CommandId::FIRST);
        reader_ctx.lock_owner = LockOwner(7);
        let (tx, rx) = mpsc::channel();
        let reader = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || {
                let rows: Vec<Result<(Tid, Tuple), StorageError>> =
                    table.scan(&reader_ctx, &ColumnProjection::All).collect();
                tx.send(()).expect("send");
                rows
            })
        };
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "the reader must wait for the TRUNCATE's exclusive hold"
        );
        // Finalize: the directory the reader looked at before waiting is now gone.
        finish(&table, truncater, true)?;
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the reader must proceed once the hold is released");
        let rows = reader.join().expect("reader panicked");
        let values: Vec<i32> = rows
            .into_iter()
            .map(|row| match row.expect("a waiting scan must not report corruption").1[0] {
                Value::Int4(value) => value,
                ref other => panic!("unexpected value {other:?}"),
            })
            .collect();
        assert_eq!(values, vec![7]);
        Ok(())
    }

    /// A TRUNCATE that fails before staging anything must not keep the exclusive
    /// hold: nothing would ever release it (the transaction has no staged swap for
    /// the finalize path to find), and the table would be unusable for the process
    /// lifetime.
    #[test]
    fn a_truncate_that_fails_to_stage_releases_the_exclusive_hold() -> anyhow::Result<()> {
        use crabgresql_txn::LockOwner;

        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("t", &[PgType::Int4]), Arc::clone(&wal))?;
        // Occupy the path the allocator will hand out with a regular file, so
        // `create_dir_all` for the staged directory fails.
        std::fs::write(dir.path().join("parquet").join("1000"), b"not a directory")?;

        let truncater = tm.allocate_xid();
        let mut truncate_ctx = tm.context(truncater, CommandId::FIRST);
        truncate_ctx.lock_owner = LockOwner(8);
        let error = table
            .truncate(&truncate_ctx)
            .expect_err("staging must fail when the directory cannot be created");
        assert!(matches!(error, StorageError::Io(_)), "{error:?}");
        tm.abort(truncater);
        finish(&table, truncater, false)?;

        // Another owner can still read and write: the hold was released.
        let mut reader_ctx = tm.context(Xid::INVALID, CommandId::FIRST);
        reader_ctx.lock_owner = LockOwner(7);
        assert!(scan_values(&table, &reader_ctx).is_empty());
        let writer = tm.allocate_xid();
        let mut write_ctx = tm.context(writer, CommandId::FIRST);
        write_ctx.lock_owner = LockOwner(7);
        table.insert(vec![Value::Int4(3)], &write_ctx)?;
        tm.commit(writer)?;
        finish(&table, writer, true)?;
        let mut reader_ctx = tm.context(Xid::INVALID, CommandId::FIRST);
        reader_ctx.lock_owner = LockOwner(7);
        assert_eq!(scan_values(&table, &reader_ctx), vec![3]);
        Ok(())
    }

    /// `rebind`'s fallible steps run before it publishes the new relfilenode: the
    /// caller logs a failure and keeps serving the table, so a half-applied rebind
    /// (new directory, old block counter) would hand out colliding block numbers.
    #[test]
    fn a_failed_rebind_leaves_the_table_on_its_old_directory() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("t", &[PgType::Int4]), Arc::clone(&wal))?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        // A regular file where the rebind target directory should be.
        std::fs::write(dir.path().join("parquet").join("77"), b"not a directory")?;
        table
            .rebind(77)
            .expect_err("rebind must fail when the directory cannot be created");
        assert_eq!(table.relfilenode(), 1, "the table stays on its old directory");

        // And the block counter still describes that directory: the next insert does
        // not collide with the existing fragment.
        let writer = tm.allocate_xid();
        let tid = table.insert(vec![Value::Int4(2)], &tm.context(writer, CommandId::FIRST))?;
        assert_eq!(tid, Tid::new(2, 1));
        tm.commit(writer)?;
        finish(&table, writer, true)?;
        assert_eq!(
            scan_values(&table, &tm.context(Xid::INVALID, CommandId::FIRST)),
            vec![1, 2]
        );
        Ok(())
    }

    /// A relation whose columns span the two shapes that matter to a
    /// `ProjectionMask`: plain scalars, and the `Struct`-backed `timetz` /
    /// `interval` that own several *leaf* descriptors under one root.
    fn struct_mixed_table(
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
            .scan(&reader, &ColumnProjection::of([0, 2], table.schema()))
            .map(|result| result.map(|(_, tuple)| tuple))
            .collect::<Result<_, _>>()?;

        assert_eq!(projected, vec![vec![
            row[0].clone(),
            Value::Null,
            row[2].clone(),
            Value::Null,
            Value::Null,
        ]]);
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
                .scan(&reader, &ColumnProjection::of([column], table.schema()))
                .map(|result| result.map(|(_, tuple)| tuple))
                .collect::<Result<_, _>>()?;
            let mut want = vec![Value::Null; row.len()];
            want[column] = row[column].clone();
            assert_eq!(rows, vec![want], "projecting only column {column}");
        }
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
        assert_eq!(tids(&ColumnProjection::of([2], table.schema()))?, full);
        // The empty set is the `count(*)` shape, normalized to one column.
        assert_eq!(tids(&ColumnProjection::of([], table.schema()))?, full);

        // `fetch` still resolves each tid to the whole row.
        for tid in full {
            assert_eq!(table.fetch(tid, &reader)?, Some(row.clone()));
        }
        Ok(())
    }
}
