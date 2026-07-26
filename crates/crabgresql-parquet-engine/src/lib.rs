//! Managed append-only Parquet table access method.
//!
//! A statement batch is written as one or more immutable fragments. Files remain
//! `.pending` until the transaction commits; the engine's finalize hook promotes
//! them to `.parquet` or removes them on abort. MVCC identity lives in file
//! metadata, leaving the physical Parquet schema composed solely of user columns.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, FixedSizeBinaryBuilder, Float32Builder, Float64Builder,
    Int16Builder, Int32Builder, Int64Builder, StringBuilder, StructBuilder,
    Time64MicrosecondBuilder, TimestampMicrosecondBuilder,
};
use arrow_array::{
    Array, ArrayRef, BinaryArray, BooleanArray, Date32Array, FixedSizeBinaryArray, Float32Array,
    Float64Array, Int16Array, Int32Array, Int64Array, RecordBatch, RecordBatchOptions, StringArray,
    StructArray, Time64MicrosecondArray, TimestampMicrosecondArray,
};
use arrow_schema::{DataType, Field, Fields, Schema, TimeUnit};
use crabgresql_storage_api::{
    DeleteResult, IndexMetadata, RelStats, StorageError, TableAm, TableCapabilities, TableSchema,
    Tid, Tuple, TupleStream, UpdateResult,
};
use crabgresql_txn::{
    CommandId, Infomask, TupleHeader, TxnContext, Xid, satisfies_mvcc,
};
use crabgresql_types::{Interval, PgType, TimeTz, Value};
use crabgresql_wal::{RedoContext, RmgrId, RmgrRedo, Wal, WalError};
use parquet::arrow::arrow_reader::{
    ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder,
};
use parquet::arrow::arrow_writer::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

pub const RMGR_PARQUET: RmgrId = RmgrId(12);
pub const PARQUET_XID_OBSERVED: u8 = 1;

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

pub fn validate_schema(schema: &TableSchema) -> Result<(), StorageError> {
    if let Some(column) = schema.columns.iter().find(|column| !supports_type(column.ty)) {
        return Err(StorageError::UnsupportedType(format!(
            "data type {} is not supported by table access method \"parquet\"",
            column.ty.name()
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

fn decode_row(
    schema: &TableSchema,
    batch: &RecordBatch,
    row: usize,
) -> Result<Tuple, StorageError> {
    schema
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| decode_value(column, batch.column(index).as_ref(), row))
        .collect()
}

#[derive(Clone, Debug)]
struct Fragment {
    path: PathBuf,
    block: u32,
    xid: Xid,
    cid: CommandId,
    pending: bool,
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
    let block = parts
        .next()
        .and_then(|value| u32::from_str_radix(value, 16).ok())
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

fn fragments(dir: &Path) -> Result<Vec<Fragment>, StorageError> {
    let entries = std::fs::read_dir(dir).map_err(|error| io_error("read Parquet table", error))?;
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

fn open_reader(
    schema: &TableSchema,
    rel: u32,
    fragment: &Fragment,
) -> Result<ParquetRecordBatchReader, StorageError> {
    let file = File::open(&fragment.path).map_err(|error| io_error("open Parquet fragment", error))?;
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
    builder
        .with_batch_size(8_192)
        .build()
        .map_err(|error| corrupt(format!("open Parquet row reader: {error}")))
}

struct ParquetScan {
    schema: TableSchema,
    rel: u32,
    fragments: Vec<Fragment>,
    fragment_index: usize,
    reader: Option<ParquetRecordBatchReader>,
    batch: Option<RecordBatch>,
    batch_row: usize,
    file_row: u32,
    current_block: u32,
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
                    decode_row(&self.schema, batch, row).map(|tuple| {
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
            match open_reader(&self.schema, self.rel, &fragment) {
                Ok(reader) => self.reader = Some(reader),
                Err(error) => return Some(Err(error)),
            }
        }
    }
}

pub struct ParquetTable {
    schema: TableSchema,
    rel: u32,
    dir: PathBuf,
    wal: Arc<Wal>,
    indexes: RwLock<Vec<IndexMetadata>>,
    analyzed: RwLock<Option<(u32, f64)>>,
    next_block: Mutex<u32>,
}

impl ParquetTable {
    pub fn open(
        root: &Path,
        rel: u32,
        schema: TableSchema,
        indexes: Vec<IndexMetadata>,
        wal: Arc<Wal>,
    ) -> Result<Self, StorageError> {
        validate_schema(&schema)?;
        let dir = root.join("parquet").join(rel.to_string());
        std::fs::create_dir_all(&dir)
            .map_err(|error| io_error("create Parquet table directory", error))?;
        let next_block = fragments(&dir)?
            .into_iter()
            .map(|fragment| fragment.block)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        Ok(Self {
            schema,
            rel,
            dir,
            wal,
            indexes: RwLock::new(indexes),
            analyzed: RwLock::new(None),
            next_block: Mutex::new(next_block),
        })
    }

    pub fn set_analyzed(&self, relpages: u32, reltuples: f64) {
        *self
            .analyzed
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned")) = Some((relpages, reltuples));
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

    pub fn finish_transaction(&self, xid: Xid, committed: bool) -> Result<(), StorageError> {
        for fragment in fragments(&self.dir)?
            .into_iter()
            .filter(|fragment| fragment.pending && fragment.xid == xid)
        {
            if committed {
                let name = fragment
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| corrupt("pending Parquet filename is invalid"))?;
                let final_name = name
                    .strip_suffix(".pending")
                    .ok_or_else(|| corrupt("pending Parquet suffix is invalid"))?;
                std::fs::rename(&fragment.path, self.dir.join(final_name))
                    .map_err(|error| io_error("promote Parquet fragment", error))?;
            } else {
                std::fs::remove_file(&fragment.path)
                    .map_err(|error| io_error("remove aborted Parquet fragment", error))?;
            }
        }
        sync_dir(&self.dir)
    }

    pub fn recover(&self, clog: &crabgresql_txn::Clog) -> Result<(), StorageError> {
        let entries =
            std::fs::read_dir(&self.dir).map_err(|error| io_error("recover Parquet table", error))?;
        for entry in entries {
            let path = entry
                .map_err(|error| io_error("recover Parquet entry", error))?
                .path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("tmp") {
                std::fs::remove_file(path)
                    .map_err(|error| io_error("remove temporary Parquet fragment", error))?;
                continue;
            }
            if let Some(fragment) = parse_fragment(path)?
                && fragment.pending
            {
                self.finish_transaction(fragment.xid, clog.is_committed(fragment.xid))?;
            }
        }
        Ok(())
    }

    pub fn drop_storage(&self) -> Result<(), StorageError> {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("drop Parquet table storage", error)),
        }
    }

    fn visible_fragments(&self, txn: &TxnContext) -> Result<Vec<Fragment>, StorageError> {
        Ok(fragments(&self.dir)?
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

    fn write_fragment(
        &self,
        block: u32,
        tuples: &[Tuple],
        txn: &TxnContext,
    ) -> Result<(PathBuf, PathBuf), StorageError> {
        let base = format!("{block:08x}-{}-{}", txn.xid.0, txn.cid.0);
        let temp = self.dir.join(format!("{base}.tmp"));
        let pending = self.dir.join(format!("{base}.parquet.pending"));
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
            KeyValue::new(META_REL.to_string(), Some(self.rel.to_string())),
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

impl TableAm for ParquetTable {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    fn capabilities(&self) -> TableCapabilities {
        TableCapabilities::APPEND_ONLY
    }

    fn indexes(&self) -> Vec<IndexMetadata> {
        self.indexes
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .clone()
    }

    fn statistics(&self) -> RelStats {
        if let Some((relpages, reltuples)) = *self
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
        let Ok(fragments) = fragments(&self.dir) else {
            return RelStats::unknown(&self.schema);
        };
        let bytes: u64 = fragments
            .iter()
            .filter(|fragment| !fragment.pending)
            .filter_map(|fragment| std::fs::metadata(&fragment.path).ok())
            .map(|metadata| metadata.len())
            .sum();
        RelStats::from_pages(bytes.div_ceil(8_192).min(u32::MAX as u64) as u32, &self.schema)
    }

    fn scan(&self, txn: &TxnContext) -> TupleStream {
        match self.visible_fragments(txn) {
            Ok(fragments) => Box::new(ParquetScan {
                schema: self.schema.clone(),
                rel: self.rel,
                fragments,
                fragment_index: 0,
                reader: None,
                batch: None,
                batch_row: 0,
                file_row: 0,
                current_block: 0,
            }),
            Err(error) => Box::new(std::iter::once(Err(error))),
        }
    }

    fn fetch(&self, tid: Tid, txn: &TxnContext) -> Result<Option<Tuple>, StorageError> {
        let Some(fragment) = self
            .visible_fragments(txn)?
            .into_iter()
            .find(|fragment| fragment.block == tid.block)
        else {
            return Ok(None);
        };
        let mut reader = open_reader(&self.schema, self.rel, &fragment)?;
        let mut ordinal = 1u32;
        for batch in &mut reader {
            let batch = batch.map_err(|error| corrupt(format!("decode Parquet row group: {error}")))?;
            for row in 0..batch.num_rows() {
                if ordinal == tid.offset as u32 {
                    return decode_row(&self.schema, &batch, row).map(Some);
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
        let mut next = self
            .next_block
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let mut staged = Vec::new();
        let mut tids = Vec::with_capacity(tuples.len());
        for chunk in tuples.chunks(MAX_FRAGMENT_ROWS) {
            let block = *next;
            *next = next
                .checked_add(1)
                .ok_or_else(|| io_error("allocate Parquet fragment", "fragment id exhausted"))?;
            let (temp, pending) = match self.write_fragment(block, chunk, txn) {
                Ok(paths) => paths,
                Err(error) => {
                    let base = format!("{block:08x}-{}-{}", txn.xid.0, txn.cid.0);
                    let _ = std::fs::remove_file(self.dir.join(format!("{base}.tmp")));
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
                let _ = sync_dir(&self.dir);
                return Err(io_error("publish pending Parquet fragment", error));
            }
        }
        if let Err(error) = sync_dir(&self.dir) {
            for (temp, pending) in &staged {
                let _ = std::fs::remove_file(temp);
                let _ = std::fs::remove_file(pending);
            }
            let _ = sync_dir(&self.dir);
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
            let _ = sync_dir(&self.dir);
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

    fn truncate(&self, _txn: &TxnContext) -> Result<(), StorageError> {
        Err(unsupported(
            "table access method \"parquet\" does not support TRUNCATE",
        ))
    }
}

/// Recovery only needs the record to make the XID allocator observe the
/// transaction. Fragment bytes were fsynced before commit and pending-file
/// promotion is reconciled separately.
pub struct ParquetRedo;

impl RmgrRedo for ParquetRedo {
    fn redo(&self, ctx: &RedoContext) -> Result<(), WalError> {
        if ctx.info == PARQUET_XID_OBSERVED && ctx.payload.is_empty() {
            Ok(())
        } else {
            Err(WalError::Redo(format!(
                "unknown parquet WAL record info byte {:#x}",
                ctx.info
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{File, OpenOptions};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use crabgresql_storage_api::{
        Column, StorageError, TableAccessMethod, TableAm, TableSchema, Tid, Tuple,
    };
    use crabgresql_txn::{
        Clog, CommandId, CommitSink, TransactionManager, Xid,
    };
    use crabgresql_types::numeric::Numeric;
    use crabgresql_types::{Interval, PgType, TimeTz, Value};
    use crabgresql_wal::{RmgrRegistry, Wal, recover};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use parquet::basic::Compression;

    use super::ParquetTable;

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
        let table = ParquetTable::open(
            dir.path(),
            1,
            schema,
            Vec::new(),
            Arc::clone(&wal),
        )?;
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
                .scan(&tm.context(xid, CommandId::FIRST))
                .count(),
            0,
            "a statement cannot see its own inserts before the command counter advances"
        );
        let own_rows: Vec<Tuple> = table
            .scan(&tm.context(xid, CommandId(1)))
            .map(|result| result.map(|(_, tuple)| tuple))
            .collect::<Result<_, _>>()?;
        assert_eq!(own_rows, vec![row.clone(), nulls.clone()]);
        assert_eq!(
            table
                .scan(&tm.context(Xid::INVALID, CommandId::FIRST))
                .count(),
            0
        );

        tm.commit(xid)?;
        table.finish_transaction(xid, true)?;
        let rows: Vec<Tuple> = table
            .scan(&tm.context(Xid::INVALID, CommandId::FIRST))
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

    #[test]
    fn inserts_split_at_fragment_limit_and_tids_fetch_stably() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = ParquetTable::open(
            dir.path(),
            1,
            schema("many", &[PgType::Int4]),
            Vec::new(),
            Arc::clone(&wal),
        )?;
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
        table.finish_transaction(xid, true)?;
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
        let table = ParquetTable::open(
            dir.path(),
            1,
            schema("aborted", &[PgType::Int4]),
            Vec::new(),
            Arc::clone(&wal),
        )?;
        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(xid, CommandId::FIRST))?;
        tm.abort(xid);
        table.finish_transaction(xid, false)?;
        assert!(parquet_files(dir.path(), 1)?.is_empty());
        assert_eq!(
            table
                .scan(&tm.context(Xid::INVALID, CommandId::FIRST))
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
        let committed = ParquetTable::open(
            dir.path(),
            1,
            schema("committed", &[PgType::Int4]),
            Vec::new(),
            Arc::clone(&wal),
        )?;
        let committed_xid = tm.allocate_xid();
        committed.insert(
            vec![Value::Int4(1)],
            &tm.context(committed_xid, CommandId::FIRST),
        )?;
        tm.commit(committed_xid)?;

        let interrupted = ParquetTable::open(
            dir.path(),
            2,
            schema("interrupted", &[PgType::Int4]),
            Vec::new(),
            Arc::clone(&wal),
        )?;
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
        registry.register(super::RMGR_PARQUET, Arc::new(super::ParquetRedo));
        let clog = Arc::new(Clog::new());
        let result = recover(dir.path(), &registry, &clog)?;
        assert!(result.next_xid > interrupted_xid);

        let committed = ParquetTable::open(
            dir.path(),
            1,
            schema("committed", &[PgType::Int4]),
            Vec::new(),
            Arc::clone(&recovered_wal),
        )?;
        committed.recover(&clog)?;
        let interrupted = ParquetTable::open(
            dir.path(),
            2,
            schema("interrupted", &[PgType::Int4]),
            Vec::new(),
            recovered_wal,
        )?;
        interrupted.recover(&clog)?;
        assert_eq!(parquet_files(dir.path(), 1)?.len(), 1);
        assert!(parquet_files(dir.path(), 2)?.is_empty());
        Ok(())
    }

    #[test]
    fn truncated_fragment_is_reported_as_corrupt_storage() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = ParquetTable::open(
            dir.path(),
            1,
            schema("corrupt", &[PgType::Int4]),
            Vec::new(),
            wal,
        )?;
        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        table.finish_transaction(xid, true)?;
        let file = parquet_files(dir.path(), 1)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing committed fragment"))?;
        OpenOptions::new().write(true).open(file)?.set_len(10)?;

        let error = table
            .scan(&tm.context(Xid::INVALID, CommandId::FIRST))
            .next()
            .ok_or_else(|| anyhow::anyhow!("corrupt scan returned no item"))?
            .expect_err("truncated fragment must return an error");
        assert!(matches!(error, StorageError::CorruptData(_)));
        Ok(())
    }
}
