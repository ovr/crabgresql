//! Mapping between crabgresql's `TableSchema`/`Value` model and Apache Arrow, and
//! the on-disk Parquet read/write of a table's rows.
//!
//! Each table is one Parquet file. The exact `TableSchema` (namespace, name, and
//! per-column PgType oid / typmod / nullability) is embedded in the file's
//! key-value metadata under [`SCHEMA_META_KEY`], so the engine reconstructs the
//! catalog faithfully on restart without a separate sidecar or a serde
//! dependency. Only the type set below is supported; other column types are
//! rejected at `CREATE TABLE` time.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BinaryArray, BinaryBuilder, BooleanArray, BooleanBuilder, Date32Array,
    Date32Builder, Float32Array, Float32Builder, Float64Array, Float64Builder, Int16Array,
    Int16Builder, Int32Array, Int32Builder, Int64Array, Int64Builder, StringArray, StringBuilder,
    TimestampMicrosecondArray, TimestampMicrosecondBuilder,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::properties::WriterProperties;
use parquet::format::KeyValue;

use crabgresql_storage_api::{Column, StorageError, TableSchema, Tuple};
use crabgresql_types::{PgType, Value};

/// Key under which the encoded [`TableSchema`] lives in the Parquet file's
/// key-value metadata.
pub const SCHEMA_META_KEY: &str = "crabgresql:schema";

/// Days between the Unix epoch (1970-01-01, Arrow's `Date32` origin) and
/// PostgreSQL's date origin (2000-01-01).
const DATE_PG_EPOCH_UNIX_DAYS: i32 = 10_957;
/// Microseconds between the Unix epoch and PostgreSQL's timestamp origin
/// (2000-01-01 00:00:00 UTC).
const TS_PG_EPOCH_UNIX_MICROS: i64 = 946_684_800_000_000;

fn io<E: std::fmt::Display>(context: &str, e: E) -> StorageError {
    StorageError::Io(format!("parquet: {context}: {e}"))
}

/// The Arrow `DataType` backing a PgType, or `None` if the type is not supported
/// by the Parquet access method (the caller rejects such columns at DDL time).
pub fn pg_to_arrow_type(ty: PgType) -> Option<DataType> {
    Some(match ty {
        PgType::Bool => DataType::Boolean,
        PgType::Int2 => DataType::Int16,
        PgType::Int4 => DataType::Int32,
        PgType::Int8 => DataType::Int64,
        PgType::Float4 => DataType::Float32,
        PgType::Float8 => DataType::Float64,
        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => DataType::Utf8,
        PgType::Bytea => DataType::Binary,
        PgType::Date => DataType::Date32,
        PgType::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
        PgType::TimestampTz => DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
        _ => return None,
    })
}

/// The first column of `schema` whose type the Parquet engine cannot represent,
/// or `None` if every column is supported.
pub fn first_unsupported_column(schema: &TableSchema) -> Option<&Column> {
    schema
        .columns
        .iter()
        .find(|c| pg_to_arrow_type(c.ty).is_none())
}

fn arrow_schema(schema: &TableSchema) -> Result<SchemaRef, StorageError> {
    let mut fields = Vec::with_capacity(schema.columns.len());
    for c in &schema.columns {
        let dt = pg_to_arrow_type(c.ty).ok_or_else(|| {
            StorageError::Io(format!(
                "parquet: column \"{}\" has unsupported type",
                c.name
            ))
        })?;
        // All columns are written nullable; SQL NULL maps to an Arrow null.
        fields.push(Field::new(&c.name, dt, true));
    }
    Ok(Arc::new(Schema::new(fields)))
}

/// A `'static` NULL to return for absent cells without borrowing a temporary.
const NULL_VALUE: Value = Value::Null;

/// The value at column `idx` of `row`, or NULL if the row is short. A free
/// function (not a closure) so lifetime elision ties the result to `row`.
fn cell(row: &Tuple, idx: usize) -> &Value {
    row.get(idx).unwrap_or(&NULL_VALUE)
}

/// Build one Arrow column array from every row's value at `idx`.
fn build_column(ty: PgType, rows: &[Tuple], idx: usize) -> Result<ArrayRef, StorageError> {
    // Simple scalar columns: pull the inner value, mapping SQL NULL (and any
    // unexpected variant, which should not occur after coercion) to an Arrow null.
    macro_rules! prim {
        ($Builder:ty, $pat:pat => $val:expr) => {{
            let mut b = <$Builder>::new();
            for r in rows {
                match cell(r, idx) {
                    $pat => b.append_value($val),
                    Value::Null => b.append_null(),
                    other => {
                        tracing::warn!(?other, column = idx, "parquet: unexpected value, writing NULL");
                        b.append_null();
                    }
                }
            }
            Arc::new(b.finish()) as ArrayRef
        }};
    }

    let array = match ty {
        PgType::Bool => prim!(BooleanBuilder, Value::Bool(v) => *v),
        PgType::Int2 => prim!(Int16Builder, Value::Int2(v) => *v),
        PgType::Int4 => prim!(Int32Builder, Value::Int4(v) => *v),
        PgType::Int8 => prim!(Int64Builder, Value::Int8(v) => *v),
        PgType::Float4 => prim!(Float32Builder, Value::Float4(v) => *v),
        PgType::Float8 => prim!(Float64Builder, Value::Float8(v) => *v),
        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => {
            prim!(StringBuilder, Value::Text(v) => v.as_str())
        }
        PgType::Bytea => prim!(BinaryBuilder, Value::Bytea(v) => v.as_slice()),
        PgType::Date => {
            prim!(Date32Builder, Value::Date(v) => v.saturating_add(DATE_PG_EPOCH_UNIX_DAYS))
        }
        PgType::Timestamp => {
            prim!(TimestampMicrosecondBuilder, Value::Timestamp(v) => v.saturating_add(TS_PG_EPOCH_UNIX_MICROS))
        }
        PgType::TimestampTz => {
            let mut b = TimestampMicrosecondBuilder::new();
            for r in rows {
                match cell(r, idx) {
                    Value::TimestampTz(v) => b.append_value(v.saturating_add(TS_PG_EPOCH_UNIX_MICROS)),
                    Value::Null => b.append_null(),
                    other => {
                        tracing::warn!(?other, column = idx, "parquet: unexpected value, writing NULL");
                        b.append_null();
                    }
                }
            }
            Arc::new(b.finish().with_timezone("UTC")) as ArrayRef
        }
        _ => {
            return Err(StorageError::Io(
                "parquet: unsupported column type reached the writer".to_string(),
            ));
        }
    };
    Ok(array)
}

/// Read the value at `row` of `array`, interpreting it as `ty` and reversing the
/// Arrow epoch shifts for temporal types.
fn arrow_value(array: &dyn Array, row: usize, ty: PgType) -> Value {
    if array.is_null(row) {
        return Value::Null;
    }
    // Downcasts cannot fail: the array was built from this column's type.
    macro_rules! col {
        ($T:ty) => {
            array
                .as_any()
                .downcast_ref::<$T>()
                .expect("parquet: column array type mismatch")
        };
    }
    match ty {
        PgType::Bool => Value::Bool(col!(BooleanArray).value(row)),
        PgType::Int2 => Value::Int2(col!(Int16Array).value(row)),
        PgType::Int4 => Value::Int4(col!(Int32Array).value(row)),
        PgType::Int8 => Value::Int8(col!(Int64Array).value(row)),
        PgType::Float4 => Value::Float4(col!(Float32Array).value(row)),
        PgType::Float8 => Value::Float8(col!(Float64Array).value(row)),
        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => {
            Value::Text(col!(StringArray).value(row).to_string())
        }
        PgType::Bytea => Value::Bytea(col!(BinaryArray).value(row).to_vec()),
        PgType::Date => {
            Value::Date(col!(Date32Array).value(row).saturating_sub(DATE_PG_EPOCH_UNIX_DAYS))
        }
        PgType::Timestamp => Value::Timestamp(
            col!(TimestampMicrosecondArray)
                .value(row)
                .saturating_sub(TS_PG_EPOCH_UNIX_MICROS),
        ),
        PgType::TimestampTz => Value::TimestampTz(
            col!(TimestampMicrosecondArray)
                .value(row)
                .saturating_sub(TS_PG_EPOCH_UNIX_MICROS),
        ),
        // Unsupported types never reach a file, so a stored value of one is a bug.
        _ => Value::Null,
    }
}

/// Serialize `TableSchema` into the compact tab/newline form stored in the
/// Parquet file's key-value metadata. Column defaults and NOT NULL constraint
/// names are not persisted (not needed for append+read).
fn encode_schema(schema: &TableSchema) -> String {
    let mut out = format!("{}\n{}", schema.namespace, schema.name);
    for c in &schema.columns {
        out.push('\n');
        out.push_str(&format!(
            "{}\t{}\t{}\t{}",
            c.name,
            c.ty.oid(),
            c.typmod,
            u8::from(c.nullable)
        ));
    }
    out
}

/// Inverse of [`encode_schema`]. Returns `None` if the payload is malformed.
fn decode_schema(payload: &str) -> Option<TableSchema> {
    let mut lines = payload.split('\n');
    let namespace = lines.next()?.to_string();
    let name = lines.next()?.to_string();
    let mut columns = Vec::new();
    for line in lines {
        let mut parts = line.split('\t');
        let cname = parts.next()?.to_string();
        let oid: u32 = parts.next()?.parse().ok()?;
        let typmod: i32 = parts.next()?.parse().ok()?;
        let nullable = parts.next()? == "1";
        let ty = PgType::from_oid(oid)?;
        let mut col = Column::with_typmod(cname, ty, typmod);
        col.nullable = nullable;
        columns.push(col);
    }
    Some(TableSchema {
        name,
        namespace,
        columns,
        access_method: Some("parquet".to_string()),
    })
}

/// Write `rows` to the Parquet file at `path` (overwriting), embedding `schema`
/// in the file metadata. Written via a temporary sibling file and renamed into
/// place so a crash mid-write cannot leave a truncated table file.
pub fn write_parquet(path: &Path, schema: &TableSchema, rows: &[Tuple]) -> Result<(), StorageError> {
    let arrow_schema = arrow_schema(schema)?;
    let mut columns = Vec::with_capacity(schema.columns.len());
    for (idx, col) in schema.columns.iter().enumerate() {
        columns.push(build_column(col.ty, rows, idx)?);
    }
    let batch = RecordBatch::try_new(arrow_schema.clone(), columns)
        .map_err(|e| io("building record batch", e))?;

    let props = WriterProperties::builder()
        .set_key_value_metadata(Some(vec![KeyValue {
            key: SCHEMA_META_KEY.to_string(),
            value: Some(encode_schema(schema)),
        }]))
        .build();

    let tmp_path = path.with_extension("parquet.tmp");
    let file = File::create(&tmp_path).map_err(|e| io("creating file", e))?;
    let mut writer = ArrowWriter::try_new(file, arrow_schema, Some(props))
        .map_err(|e| io("opening writer", e))?;
    // A table with zero rows still writes a valid, self-describing file.
    if batch.num_rows() > 0 {
        writer.write(&batch).map_err(|e| io("writing batch", e))?;
    }
    writer.close().map_err(|e| io("closing writer", e))?;
    std::fs::rename(&tmp_path, path).map_err(|e| io("renaming file", e))?;
    Ok(())
}

/// Read a Parquet table file back into its `TableSchema` and rows.
pub fn read_parquet(path: &Path) -> Result<(TableSchema, Vec<Tuple>), StorageError> {
    let file = File::open(path).map_err(|e| io("opening file", e))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| io("reading metadata", e))?;

    let payload = builder
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .and_then(|kvs| kvs.iter().find(|kv| kv.key == SCHEMA_META_KEY))
        .and_then(|kv| kv.value.clone())
        .ok_or_else(|| io("missing schema metadata", path.display()))?;
    let schema = decode_schema(&payload)
        .ok_or_else(|| io("malformed schema metadata", path.display()))?;

    let reader = builder.build().map_err(|e| io("building reader", e))?;
    let mut rows: Vec<Tuple> = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|e| io("reading batch", e))?;
        for row in 0..batch.num_rows() {
            let mut tuple = Vec::with_capacity(schema.columns.len());
            for (idx, col) in schema.columns.iter().enumerate() {
                tuple.push(arrow_value(batch.column(idx).as_ref(), row, col.ty));
            }
            rows.push(tuple);
        }
    }
    Ok((schema, rows))
}
