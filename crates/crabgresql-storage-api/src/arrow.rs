//! The canonical `Value` ⇄ Arrow mapping, shared by every engine that speaks
//! columnar and by the vectorized executor.
//!
//! # The one invariant
//!
//! **An Arrow array built here carries values in `Value` semantics, not in
//! Arrow's.** Concretely: a `Date32` holds days since PostgreSQL's 2000-01-01
//! epoch (not Arrow's Unix epoch), a `Timestamp` holds microseconds since the
//! same, and both keep `i32::MIN`/`i32::MAX` (`i64::MIN`/`i64::MAX`) as the
//! ±infinity sentinels rather than as ordinary instants.
//!
//! That is a deliberate lie about Arrow's own definition of `Date32`, and it is
//! the lesser evil. A batch produced here flows straight into comparison and
//! sort kernels that must agree with [`crabgresql_types::Value`] ordering and
//! with predicate constants the binder produced. Keeping the batch in Arrow's
//! epoch would mean rebasing every constant, every sort key and every shred
//! back to rows — many sites, each of which silently shifts results by ~30
//! years when missed. Keeping it in `Value` semantics means the conversion
//! happens **once, at the storage boundary that owns an on-disk format**.
//!
//! So: a format whose file layout is defined in Arrow's epoch (Parquet) rebases
//! on the way in and out of the file, and nowhere else. These arrays never
//! reach Arrow's own display, cast, or temporal kernels, where the difference
//! would be observable.
//!
//! # Width
//!
//! [`build_batch`] produces a batch as wide as the table schema, matching the
//! [`Tuple`](crate::Tuple) contract. A batch narrowed by a
//! [`ColumnProjection`](crate::ColumnProjection) is widened back with
//! [`null_array`] so a column ordinal means the same thing in a batch as it
//! does in a row — the whole executor addresses columns by schema position.

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Date32Builder, FixedSizeBinaryBuilder, Float32Builder,
    Float64Builder, Int16Builder, Int32Builder, Int64Builder, StringBuilder, StructBuilder,
    Time64MicrosecondBuilder, TimestampMicrosecondBuilder, UInt8Builder,
};
use arrow_array::{
    Array, ArrayRef, BinaryArray, BooleanArray, Date32Array, FixedSizeBinaryArray, Float32Array,
    Float64Array, Int16Array, Int32Array, Int64Array, RecordBatch, RecordBatchOptions, StringArray,
    StructArray, Time64MicrosecondArray, TimestampMicrosecondArray, UInt8Array, new_null_array,
};
use arrow_schema::{DataType, Field, Fields, Schema, TimeUnit};
use crabgresql_types::{Interval, PgType, TimeTz, Value};

use crate::{Column, StorageError, TableSchema, Tuple};

fn corrupt(context: impl Into<String>) -> StorageError {
    StorageError::CorruptData(context.into())
}

fn value_mismatch(column: &str, ty: PgType) -> StorageError {
    corrupt(format!(
        "Arrow conversion for column \"{column}\" expected {}",
        ty.name()
    ))
}

/// Whether a type has an Arrow representation in this mapping.
///
/// The set is the columnar storage whitelist: a value an engine accepts must
/// always be convertible to a batch, or a flush would fail long after the
/// `INSERT` that should have been rejected.
pub fn supports_type(ty: PgType) -> bool {
    matches!(
        ty,
        PgType::Bool
            | PgType::Char
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

/// The Arrow type a column of `ty` is stored as.
///
/// Two entries are worth stating outright, because they decide what a
/// vectorized operator may and may not do with the column:
///
/// - `numeric` is **`Utf8`**, not a `Decimal`. `Numeric` is arbitrary-precision
///   and Arrow's decimals are not, so the text form is the only lossless one
///   available. It follows that an Arrow comparison on this column is a *string*
///   comparison and would be wrong; `numeric` is excluded from every vectorized
///   comparison and sort.
/// - `timetz` and `interval` are `Struct`s of their components, because neither
///   has an Arrow type with matching semantics (Arrow's
///   `IntervalMonthDayNano` orders differently than PostgreSQL's canonical
///   span). Their ordering is likewise not Arrow's to compute.
pub fn arrow_type(ty: PgType) -> DataType {
    match ty {
        PgType::Bool => DataType::Boolean,
        // `"char"` is `UInt8`, not `Utf8` or `Int8`. `Utf8` cannot hold a
        // high-bit byte at all, and `Int8` would sort 0xFF *below* 0x00, which
        // contradicts the type's unsigned order and would quietly give a
        // vectorized sort the wrong answer.
        PgType::Char => DataType::UInt8,
        PgType::Int2 => DataType::Int16,
        PgType::Int4 => DataType::Int32,
        PgType::Int8 => DataType::Int64,
        PgType::Float4 => DataType::Float32,
        PgType::Float8 => DataType::Float64,
        PgType::Numeric | PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => {
            DataType::Utf8
        }
        PgType::Bytea => DataType::Binary,
        PgType::Uuid => DataType::FixedSizeBinary(16),
        PgType::Date => DataType::Date32,
        PgType::Time => DataType::Time64(TimeUnit::Microsecond),
        PgType::TimeTz => DataType::Struct(Fields::from(vec![
            Field::new("time_us", DataType::Int64, false),
            Field::new("offset_seconds", DataType::Int32, false),
        ])),
        PgType::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
        PgType::TimestampTz => DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
        PgType::Interval => DataType::Struct(Fields::from(vec![
            Field::new("months", DataType::Int32, false),
            Field::new("days", DataType::Int32, false),
            Field::new("micros", DataType::Int64, false),
        ])),
        _ => DataType::Binary,
    }
}

/// The Arrow schema for a table, one field per column in schema order.
///
/// Each field records the PostgreSQL type OID and typmod it came from. Nothing
/// reads them back — relation identity is checked by the engine's own schema
/// string — but they make a fragment self-describing to an outside reader,
/// which is the point of storing a table as Parquet at all.
pub fn arrow_schema(schema: &TableSchema) -> Arc<Schema> {
    Arc::new(Schema::new(
        schema
            .columns
            .iter()
            .map(|column| {
                let metadata = HashMap::from([
                    (
                        "crabgresql.pg_type_oid".to_string(),
                        column.ty.oid().to_string(),
                    ),
                    ("crabgresql.typmod".to_string(), column.typmod.to_string()),
                ]);
                Field::new(&column.name, arrow_type(column.ty), column.nullable)
                    .with_metadata(metadata)
            })
            .collect::<Vec<_>>(),
    ))
}

/// The Arrow schema every [`BatchStream`](crate::BatchStream) carries: as
/// [`arrow_schema`], but with **every field nullable**.
///
/// Two reasons, and either alone would be enough:
///
/// - A batch narrowed by a [`ColumnProjection`](crate::ColumnProjection) is
///   widened back with all-NULL placeholder columns, and one of those may stand
///   in for a `NOT NULL` column. Arrow validates declared nullability, so a
///   faithful schema would reject the very padding the full-width contract
///   requires.
/// - Batches from different storage leaves of the same relation get
///   concatenated (a Parquet relation is its chunk store plus its RAM buffer),
///   and Arrow requires concatenated batches to share one schema. Deriving it
///   from the table rather than from whichever leaf produced the batch is what
///   makes that true by construction.
///
/// Nullability is not lost — it lives in the [`TableSchema`], which is where
/// every constraint check already reads it.
pub fn scan_schema(schema: &TableSchema) -> Arc<Schema> {
    Arc::new(Schema::new(
        arrow_schema(schema)
            .fields()
            .iter()
            .map(|field| field.as_ref().clone().with_nullable(true))
            .collect::<Vec<_>>(),
    ))
}

/// An all-NULL array of `len` rows, used to widen a projected batch back to the
/// table's full width.
pub fn null_array(ty: PgType, len: usize) -> ArrayRef {
    new_null_array(&arrow_type(ty), len)
}

/// Build one column's array from the `index`th field of each tuple.
pub fn build_array(
    column: &Column,
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
        PgType::Char => primitive!(UInt8Builder, Value::Char),
        PgType::Int2 => primitive!(Int16Builder, Value::Int2),
        PgType::Int4 => primitive!(Int32Builder, Value::Int4),
        PgType::Int8 => primitive!(Int64Builder, Value::Int8),
        PgType::Float4 => primitive!(Float32Builder, Value::Float4),
        PgType::Float8 => primitive!(Float64Builder, Value::Float8),
        // Text, not a decimal: see [`arrow_type`].
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
        // The four string types share one array; which one a column is stays in
        // the table schema, where the typmod and padding rules live too.
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
                        .map_err(|error| StorageError::Io(format!("encode UUID: {error}")))?,
                    _ => return Err(value_mismatch(&column.name, column.ty)),
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        // PG epoch days, sentinels included — see the module invariant.
        PgType::Date => {
            let mut builder = Date32Builder::with_capacity(tuples.len());
            for tuple in tuples {
                match &tuple[index] {
                    Value::Null => builder.append_null(),
                    Value::Date(value) => builder.append_value(*value),
                    _ => return Err(value_mismatch(&column.name, column.ty)),
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        PgType::Time => primitive!(Time64MicrosecondBuilder, Value::Time),
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
        // PG epoch microseconds, sentinels included. Both timestamp types share
        // the builder and are told apart by the column's own type; only
        // `timestamptz` carries the UTC zone in its Arrow type.
        PgType::Timestamp | PgType::TimestampTz => {
            let mut builder = TimestampMicrosecondBuilder::with_capacity(tuples.len());
            for tuple in tuples {
                match (&tuple[index], column.ty) {
                    (Value::Null, _) => builder.append_null(),
                    (Value::Timestamp(value), PgType::Timestamp)
                    | (Value::TimestampTz(value), PgType::TimestampTz) => {
                        builder.append_value(*value)
                    }
                    _ => return Err(value_mismatch(&column.name, column.ty)),
                }
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
            "data type {} has no columnar representation",
            column.ty.name()
        ))),
    }
}

/// Build a full-width batch from full-width tuples, stamped with `stamp`.
fn build_with(
    schema: &TableSchema,
    tuples: &[Tuple],
    stamp: Arc<Schema>,
) -> Result<RecordBatch, StorageError> {
    for tuple in tuples {
        if tuple.len() != schema.columns.len() {
            return Err(corrupt("tuple width does not match the table schema"));
        }
    }
    let arrays = schema
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| build_array(column, tuples, index))
        .collect::<Result<Vec<_>, _>>()?;
    // An explicit row count, so a batch of zero columns still knows its length.
    let options = RecordBatchOptions::new().with_row_count(Some(tuples.len()));
    RecordBatch::try_new_with_options(stamp, arrays, &options)
        .map_err(|error| StorageError::Io(format!("build Arrow record batch: {error}")))
}

/// Build a batch for **storage**, declaring nullability as the table does. Use
/// this where the batch becomes a file whose schema outlives the process.
pub fn build_batch(schema: &TableSchema, tuples: &[Tuple]) -> Result<RecordBatch, StorageError> {
    build_with(schema, tuples, arrow_schema(schema))
}

/// Build a batch for a [`BatchStream`](crate::BatchStream), under
/// [`scan_schema`] so it can be concatenated with any other leaf's batches.
pub fn build_scan_batch(
    schema: &TableSchema,
    tuples: &[Tuple],
) -> Result<RecordBatch, StorageError> {
    build_with(schema, tuples, scan_schema(schema))
}

/// Widen a batch that a projection narrowed back to the table's full width,
/// padding the columns the scan skipped with all-NULL arrays.
///
/// `positions[i]` is the schema ordinal of the batch's `i`th column. `stamp` is
/// the relation's [`scan_schema`], passed in rather than derived: it is a pure
/// function of the table and this runs once per batch, so rebuilding every
/// `Field` and its metadata map here would cost more than the widening itself
/// on a wide relation.
///
/// Padding is built **only** for the ordinals `positions` does not name.
/// `new_null_array` is O(rows), so allocating one per schema column and then
/// overwriting the projected ones would scale with the table's width rather
/// than with what the scan skipped — the opposite of what a projection is for.
pub fn widen(
    schema: &TableSchema,
    stamp: &Arc<Schema>,
    positions: &[usize],
    batch: &RecordBatch,
) -> Result<RecordBatch, StorageError> {
    let rows = batch.num_rows();
    let mut columns: Vec<Option<ArrayRef>> = vec![None; schema.columns.len()];
    for (batch_index, &schema_index) in positions.iter().enumerate() {
        let slot = columns
            .get_mut(schema_index)
            .ok_or_else(|| corrupt("projection names a column outside the table schema"))?;
        *slot = Some(Arc::clone(batch.column(batch_index)));
    }
    let columns: Vec<ArrayRef> = columns
        .into_iter()
        .zip(&schema.columns)
        .map(|(array, column)| array.unwrap_or_else(|| null_array(column.ty, rows)))
        .collect();
    let options = RecordBatchOptions::new().with_row_count(Some(rows));
    RecordBatch::try_new_with_options(Arc::clone(stamp), columns, &options)
        .map_err(|error| StorageError::Io(format!("widen Arrow record batch: {error}")))
}

fn required_array<'a, T: 'static>(
    array: &'a dyn Array,
    column: &str,
) -> Result<&'a T, StorageError> {
    array
        .as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| corrupt(format!("column \"{column}\" has an unexpected Arrow type")))
}

/// Decode one cell back into a [`Value`].
pub fn decode_value(column: &Column, array: &dyn Array, row: usize) -> Result<Value, StorageError> {
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
        PgType::Char => primitive!(UInt8Array, Value::Char),
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
        PgType::Date => primitive!(Date32Array, Value::Date),
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
            Ok(if column.ty == PgType::Timestamp {
                Value::Timestamp(value)
            } else {
                Value::TimestampTz(value)
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
            "data type {} has no columnar representation",
            column.ty.name()
        ))),
    }
}

/// Decode a chosen subset of a **full-width** batch into a full-width tuple.
///
/// `indices` are ordinals in the batch *and* in the tuple, because the two have
/// the same width — that is the difference from [`decode_row`], whose
/// `positions` map a narrowed batch's `i`th column onto a schema ordinal. Use
/// this when the batch has already been widened and the caller only wants to
/// pay for the columns the query reads; the rest keep `Value::Null`, which is
/// what the scan contract says an unprojected slot holds.
pub fn decode_columns(
    schema: &TableSchema,
    indices: &[usize],
    batch: &RecordBatch,
    row: usize,
) -> Result<Tuple, StorageError> {
    let mut tuple = vec![Value::Null; schema.columns.len()];
    for &index in indices {
        let column = schema
            .columns
            .get(index)
            .ok_or_else(|| corrupt("decode names a column outside the table schema"))?;
        let array = batch
            .columns()
            .get(index)
            .ok_or_else(|| corrupt("decode names a column outside the batch"))?;
        tuple[index] = decode_value(column, array.as_ref(), row)?;
    }
    Ok(tuple)
}

/// Decode one row of `batch` into a full-width tuple.
///
/// `positions[i]` is the schema ordinal of the batch's `i`th column, so a batch
/// narrowed by a projection still lands in the right slots; every other slot
/// keeps `Value::Null`, matching the [`Tuple`](crate::Tuple) contract that
/// unselected positions are unspecified.
pub fn decode_row(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TableAccessMethod;

    fn schema_of(columns: Vec<Column>) -> TableSchema {
        let mut schema = TableSchema::new("t", columns);
        schema.access_method = TableAccessMethod::Parquet;
        schema
    }

    /// Every supported type survives `Value -> array -> Value` unchanged,
    /// alongside a NULL in the same column so the null buffer is exercised.
    fn round_trip(ty: PgType, values: Vec<Value>) -> Result<(), StorageError> {
        let schema = schema_of(vec![Column::new("c", ty)]);
        let mut tuples: Vec<Tuple> = values.iter().map(|v| vec![v.clone()]).collect();
        tuples.push(vec![Value::Null]);

        let batch = build_batch(&schema, &tuples)?;
        assert_eq!(batch.num_rows(), tuples.len(), "{ty:?} row count");

        for (row, tuple) in tuples.iter().enumerate() {
            let decoded = decode_row(&schema, &[0], &batch, row)?;
            assert_eq!(decoded, *tuple, "{ty:?} row {row}");
        }
        Ok(())
    }

    #[test]
    fn scalars_round_trip() -> Result<(), StorageError> {
        round_trip(PgType::Bool, vec![Value::Bool(true), Value::Bool(false)])?;
        round_trip(
            PgType::Int2,
            vec![Value::Int2(0), Value::Int2(i16::MIN), Value::Int2(i16::MAX)],
        )?;
        round_trip(
            PgType::Int4,
            vec![Value::Int4(0), Value::Int4(i32::MIN), Value::Int4(i32::MAX)],
        )?;
        round_trip(
            PgType::Int8,
            vec![Value::Int8(0), Value::Int8(i64::MIN), Value::Int8(i64::MAX)],
        )?;
        round_trip(
            PgType::Float4,
            vec![Value::Float4(0.0), Value::Float4(-0.0), Value::Float4(1.5)],
        )?;
        round_trip(
            PgType::Float8,
            vec![Value::Float8(0.0), Value::Float8(-0.0), Value::Float8(1.5)],
        )?;
        Ok(())
    }

    /// An empty string is not a NULL, and the two must stay distinguishable
    /// through the null buffer rather than through the value.
    #[test]
    fn strings_round_trip() -> Result<(), StorageError> {
        for ty in [PgType::Text, PgType::Varchar, PgType::Bpchar, PgType::Name] {
            round_trip(
                ty,
                vec![
                    Value::Text(String::new()),
                    Value::Text("hello".into()),
                    Value::Text("ünïcødé".into()),
                ],
            )?;
        }
        Ok(())
    }

    #[test]
    fn binary_round_trips() -> Result<(), StorageError> {
        // `"char"` belongs here rather than with the strings: it is a raw byte,
        // and 0x00/0xFF are exactly the values a `Utf8` mapping could not hold.
        round_trip(
            PgType::Char,
            vec![
                Value::Char(0),
                Value::Char(b'a'),
                Value::Char(0x7F),
                Value::Char(0xFF),
            ],
        )?;
        round_trip(
            PgType::Bytea,
            vec![Value::Bytea(vec![]), Value::Bytea(vec![0, 255, 128])],
        )?;
        round_trip(
            PgType::Uuid,
            vec![Value::Uuid([7; 16]), Value::Uuid([0; 16])],
        )?;
        Ok(())
    }

    /// The ±infinity sentinels are ordinary bit patterns to Arrow, so they only
    /// survive if nothing here rebases them. This is the test that fails if a
    /// Unix-epoch shift ever leaks back into this module.
    #[test]
    fn temporal_sentinels_and_epoch_survive() -> Result<(), StorageError> {
        round_trip(
            PgType::Date,
            vec![
                Value::Date(0),
                Value::Date(-1),
                Value::Date(i32::MIN),
                Value::Date(i32::MAX),
            ],
        )?;
        round_trip(
            PgType::Timestamp,
            vec![
                Value::Timestamp(0),
                Value::Timestamp(-1),
                Value::Timestamp(i64::MIN),
                Value::Timestamp(i64::MAX),
            ],
        )?;
        round_trip(
            PgType::TimestampTz,
            vec![
                Value::TimestampTz(0),
                Value::TimestampTz(i64::MIN),
                Value::TimestampTz(i64::MAX),
            ],
        )?;
        round_trip(
            PgType::Time,
            vec![Value::Time(0), Value::Time(86_399_999_999)],
        )?;
        Ok(())
    }

    #[test]
    fn struct_backed_types_round_trip() -> Result<(), StorageError> {
        round_trip(
            PgType::TimeTz,
            vec![
                Value::TimeTz(TimeTz { usec: 0, zone: 0 }),
                Value::TimeTz(TimeTz {
                    usec: 3_600_000_000,
                    zone: -18_000,
                }),
            ],
        )?;
        round_trip(
            PgType::Interval,
            vec![
                Value::Interval(Interval {
                    months: 0,
                    days: 0,
                    usec: 0,
                }),
                Value::Interval(Interval {
                    months: -13,
                    days: 40,
                    usec: -1,
                }),
            ],
        )?;
        Ok(())
    }

    #[test]
    fn numeric_round_trips_through_text() -> Result<(), StorageError> {
        let parse = |s: &str| {
            crabgresql_types::numeric::Numeric::parse(s)
                .map(Value::Numeric)
                .map_err(|_| corrupt("test numeric"))
        };
        round_trip(
            PgType::Numeric,
            vec![
                parse("0")?,
                // 1.0 and 1.00 are equal but not identical: the scale is part of
                // the value, so a text round trip has to preserve it.
                parse("1.0")?,
                parse("1.00")?,
                parse("-12345678901234567890.123456789")?,
            ],
        )
    }

    /// A projected batch decodes into the schema slots the projection named,
    /// leaving every other slot NULL — the row contract, unchanged.
    #[test]
    fn a_projected_batch_lands_in_schema_slots() -> Result<(), StorageError> {
        let schema = schema_of(vec![
            Column::new("a", PgType::Int4),
            Column::new("b", PgType::Text),
            Column::new("c", PgType::Int8),
        ]);
        let tuples = vec![vec![
            Value::Int4(1),
            Value::Text("x".into()),
            Value::Int8(9),
        ]];
        let full = build_batch(&schema, &tuples)?;
        // Keep only column `c`, as a projected scan would.
        let narrowed = full.project(&[2]).map_err(|e| corrupt(e.to_string()))?;

        let decoded = decode_row(&schema, &[2], &narrowed, 0)?;
        assert_eq!(decoded, vec![Value::Null, Value::Null, Value::Int8(9)]);
        Ok(())
    }

    /// Widening restores the schema's column count and leaves the skipped
    /// columns NULL — the batch twin of the row contract.
    #[test]
    fn widening_restores_full_width() -> Result<(), StorageError> {
        let schema = schema_of(vec![
            Column::new("a", PgType::Int4),
            Column::new("b", PgType::Text),
            Column::new("c", PgType::Int8),
        ]);
        let tuples = vec![vec![
            Value::Int4(1),
            Value::Text("x".into()),
            Value::Int8(9),
        ]];
        let narrowed = build_scan_batch(&schema, &tuples)?
            .project(&[2])
            .map_err(|e| corrupt(e.to_string()))?;

        let wide = widen(&schema, &scan_schema(&schema), &[2], &narrowed)?;
        assert_eq!(wide.num_columns(), 3);
        assert_eq!(wide.num_rows(), 1);
        assert_eq!(
            decode_row(&schema, &[0, 1, 2], &wide, 0)?,
            vec![Value::Null, Value::Null, Value::Int8(9)]
        );
        Ok(())
    }

    /// A `NOT NULL` column that a projection skipped comes back as an all-NULL
    /// padding column. Arrow validates declared nullability, so this only works
    /// because [`scan_schema`] relaxes it — the reason that function exists.
    #[test]
    fn widening_can_pad_a_not_null_column() -> Result<(), StorageError> {
        let mut required = Column::new("a", PgType::Int4);
        required.nullable = false;
        let schema = schema_of(vec![required, Column::new("b", PgType::Int8)]);

        let narrowed = build_scan_batch(&schema, &[vec![Value::Int4(1), Value::Int8(2)]])?
            .project(&[1])
            .map_err(|e| corrupt(e.to_string()))?;

        let wide = widen(&schema, &scan_schema(&schema), &[1], &narrowed)?;
        assert_eq!(
            decode_row(&schema, &[0, 1], &wide, 0)?,
            vec![Value::Null, Value::Int8(2)]
        );
        Ok(())
    }

    /// Two leaves of one relation produce batches a concat can accept. This is
    /// what a Parquet relation's chunk store and RAM buffer must satisfy.
    #[test]
    fn scan_batches_from_different_widths_share_one_schema() -> Result<(), StorageError> {
        let schema = schema_of(vec![
            Column::new("a", PgType::Int4),
            Column::new("b", PgType::Text),
        ]);
        let full = build_scan_batch(&schema, &[vec![Value::Int4(1), Value::Text("x".into())]])?;
        let widened = widen(
            &schema,
            &scan_schema(&schema),
            &[1],
            &full.project(&[1]).map_err(|e| corrupt(e.to_string()))?,
        )?;
        assert_eq!(full.schema(), widened.schema());
        Ok(())
    }

    #[test]
    fn a_null_array_is_full_width_padding() {
        let array = null_array(PgType::Int4, 3);
        assert_eq!(array.len(), 3);
        assert_eq!(array.null_count(), 3);
        assert_eq!(array.data_type(), &arrow_type(PgType::Int4));
    }

    #[test]
    fn an_unsupported_type_is_rejected_not_silently_encoded() {
        let schema = schema_of(vec![Column::new("j", PgType::Jsonb)]);
        assert!(!supports_type(PgType::Jsonb));
        assert!(matches!(
            build_batch(&schema, &[vec![Value::Null]]),
            Err(StorageError::UnsupportedType(_))
        ));
    }

    #[test]
    fn a_value_of_the_wrong_type_is_rejected() {
        let schema = schema_of(vec![Column::new("a", PgType::Int4)]);
        assert!(matches!(
            build_batch(&schema, &[vec![Value::Text("nope".into())]]),
            Err(StorageError::CorruptData(_))
        ));
    }

    #[test]
    fn a_tuple_of_the_wrong_width_is_rejected() {
        let schema = schema_of(vec![
            Column::new("a", PgType::Int4),
            Column::new("b", PgType::Int4),
        ]);
        assert!(matches!(
            build_batch(&schema, &[vec![Value::Int4(1)]]),
            Err(StorageError::CorruptData(_))
        ));
    }
}
