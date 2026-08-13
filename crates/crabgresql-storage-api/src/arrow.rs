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
    BinaryBuilder, BooleanBuilder, Date32Builder, Decimal32Builder, Decimal64Builder,
    Decimal128Builder, Decimal256Builder, FixedSizeBinaryBuilder, Float32Builder, Float64Builder,
    Int16Builder, Int32Builder, Int64Builder, StringBuilder, StructBuilder,
    Time64MicrosecondBuilder, TimestampMicrosecondBuilder, UInt8Builder,
};
use arrow_array::{
    Array, ArrayRef, BinaryArray, BooleanArray, Date32Array, Decimal32Array, Decimal64Array,
    Decimal128Array, Decimal256Array, FixedSizeBinaryArray, Float32Array, Float64Array, Int16Array,
    Int32Array, Int64Array, RecordBatch, RecordBatchOptions, StringArray, StructArray,
    Time64MicrosecondArray, TimestampMicrosecondArray, UInt8Array, new_null_array,
};
use arrow_buffer::i256;
use arrow_schema::{DataType, Field, Fields, Schema, TimeUnit};
use crabgresql_types::numeric::Numeric;
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

/// `value` as a `numeric` column of `typmod` **stores** it, or `None` when the
/// column's decimal cannot hold it.
///
/// The columnar engines call this on the way in, for two reasons that are
/// really one:
///
/// - a row that cannot be encoded is refused by the `INSERT` that wrote it
///   rather than by the flush that finds it much later — the same contract
///   [`supports_type`] keeps for types, one level down at the value;
/// - a row that *can* be encoded is stored in the form it will come back in.
///   A decimal's scale is fixed by the type, so `1.50` in a column with no
///   typmod reads back as `1.5000000000000000`. A relation has two stores (a
///   RAM buffer and a fragment file) and only the file is decimal-encoded, so
///   without this the same row would print one way before a flush and another
///   after it — and differently again depending on whether the plan happened
///   to be vectorized.
///
/// A `numeric(p, s)` column only ever fails here on NaN, which PostgreSQL
/// accepts and no decimal represents; everything else it holds has already
/// been rounded to `s` by `apply_typmod`, so this is the identity for it. A
/// column *without* a typmod fails on anything needing more than
/// [`NUMERIC_DEFAULT_SCALE`] fractional digits or [`NUMERIC_DEFAULT_PRECISION`]
/// digits in all — a deliberate deviation: PostgreSQL stores those, we refuse
/// them.
pub fn numeric_stored(value: &Numeric, typmod: i32) -> Option<Numeric> {
    let (precision, scale) = numeric_decimal(typmod);
    // The decimal's scale is the only thing storage imposes, and `trunc` is
    // already the operation that imposes one — it truncates nothing here,
    // because `fits_decimal` has just established there is nothing below
    // `-scale` to truncate. Round tripping through the integer (or, for the
    // 256-bit widths, through a rendered string) would arrive at the same
    // value by a longer road.
    value
        .fits_decimal(precision, scale)
        .then(|| value.trunc(scale as i32))
}

/// PostgreSQL's `22003` for a value this column's decimal cannot hold.
///
/// The one constructor for the condition, so the message does not depend on
/// which caller found it: [`build_array`] raises it for a value that reached
/// the encoder unchecked, and the columnar engines raise it from their write
/// gate for the same value one step earlier.
pub fn numeric_overflow(column: &str, value: &Numeric, typmod: i32) -> StorageError {
    let (precision, scale) = numeric_decimal(typmod);
    // Both arms name the column: unlike PostgreSQL's own typmod overflow, this
    // one has no statement position to point at, so the column is the only
    // handle the reader gets. `NaN` and `±Infinity` are quoted outright because
    // "cannot hold NaN" is the whole explanation; an ordinary value is not,
    // since it may run to 38 digits and its magnitude is not the point.
    let detail = if value.is_nan() || value.is_infinite() {
        format!(
            "Column \"{column}\" is stored as numeric({precision},{scale}), \
             which cannot hold {}.",
            value.to_display()
        )
    } else {
        format!(
            "Column \"{column}\" is stored as numeric({precision},{scale}), \
             which cannot hold that value exactly."
        )
    };
    StorageError::NumericFieldOverflow {
        detail: Some(detail),
    }
}

/// Whether a value's Arrow encoding is decided by its type **alone**, or also
/// by the column's type modifier.
///
/// `numeric` is the only type where the modifier is part of the encoding: it
/// fixes the decimal's scale, and a value stored under one scale reads back
/// rendered at that scale. Everywhere a value is encoded *without* a column to
/// take the modifier from — a constant in a vectorized projection, whose
/// `Column::new` leaves the typmod at -1 — that fixed scale would be imposed on
/// a value that has no storage at all, and `1.50` would come back
/// `1.5000000000000000` from a columnar plan and `1.50` from a row plan.
///
/// So such a constant declines to vectorize instead. The row path renders it,
/// exactly as PostgreSQL does.
pub fn encoding_ignores_typmod(ty: PgType) -> bool {
    ty != PgType::Numeric
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

/// The precision a `numeric` column with no typmod is stored at.
pub const NUMERIC_DEFAULT_PRECISION: u8 = 38;

/// The scale a `numeric` column with no typmod is stored at. See
/// [`numeric_decimal`] for why it is 16 and not something rounder.
pub const NUMERIC_DEFAULT_SCALE: i8 = 16;

/// The largest `numeric` precision any Arrow decimal holds; `numeric(p, s)`
/// above it has no columnar representation at all and DDL rejects the column.
pub const NUMERIC_MAX_PRECISION: u8 = 76;

/// The `(precision, scale)` a `numeric` column is stored at.
///
/// A column **with** a typmod uses its own: every value in it has been through
/// [`Numeric::apply_typmod`], which rounds to exactly `scale` fractional digits
/// and rejects anything wider than `precision`, so the fixed-point form is
/// exact and the display scale is recoverable from the type alone.
///
/// A column **without** one has no such rule, and no fixed pair can hold every
/// `numeric` — the type runs to 131072 integer digits and 16383 fractional
/// ones. [`NUMERIC_DEFAULT_SCALE`] is the smallest scale that keeps ordinary
/// division whole: PostgreSQL gives a quotient at least
/// [`crabgresql_types::numeric`]'s 16 significant digits, so `10/3` and
/// `avg(x)` both land on 16 fractional digits. Values needing more are refused
/// rather than rounded — see [`build_array`].
pub fn numeric_decimal(typmod: i32) -> (u8, i8) {
    if typmod < 0 {
        return (NUMERIC_DEFAULT_PRECISION, NUMERIC_DEFAULT_SCALE);
    }
    let (precision, scale) = Numeric::unpack_typmod(typmod);
    (precision as u8, scale as i8)
}

/// Which Arrow decimal a precision is stored in — the ladder ClickHouse uses,
/// and the same digit counts: 9 / 18 / 38 / 76.
///
/// One decision, consulted by all four sites that depend on it (the Arrow type,
/// the builder, the array a cell decodes from, and the choice between the
/// 128-bit and the string conversion). They must agree exactly: a column that
/// encoded at one width and decoded at another would read back garbage, and a
/// fragment already on disk cannot be re-decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecimalWidth {
    Bits32,
    Bits64,
    Bits128,
    Bits256,
}

/// The width `precision` is stored in.
pub fn decimal_width(precision: u8) -> DecimalWidth {
    match precision {
        0..=9 => DecimalWidth::Bits32,
        10..=18 => DecimalWidth::Bits64,
        19..=38 => DecimalWidth::Bits128,
        _ => DecimalWidth::Bits256,
    }
}

/// The components `timetz` is stored as. Named rather than inlined because
/// [`build_array`] needs the same `Fields` to build the struct, and recovering
/// them by asking [`arrow_type`] and re-matching the `DataType` it just built
/// would need a typmod this type does not have.
fn timetz_fields() -> Fields {
    Fields::from(vec![
        Field::new("time_us", DataType::Int64, false),
        Field::new("offset_seconds", DataType::Int32, false),
    ])
}

/// The components `interval` is stored as; see [`timetz_fields`].
fn interval_fields() -> Fields {
    Fields::from(vec![
        Field::new("months", DataType::Int32, false),
        Field::new("days", DataType::Int32, false),
        Field::new("micros", DataType::Int64, false),
    ])
}

/// The Arrow type a column of `ty` with `typmod` is stored as.
///
/// Two entries are worth stating outright, because they decide what a
/// vectorized operator may and may not do with the column:
///
/// - `numeric` is a **`Decimal`**, whose width follows the precision the way
///   ClickHouse's does: `p <= 9` is 32 bits, `<= 18` 64, `<= 38` 128, `<= 76`
///   256. This is why the type modifier reaches this function at all — it is
///   part of the *type*, not just a constraint on the values. See
///   [`numeric_decimal`] for what a column without a typmod gets, and for the
///   one place this mapping is not lossless.
/// - `timetz` and `interval` are `Struct`s of their components, because neither
///   has an Arrow type with matching semantics (Arrow's
///   `IntervalMonthDayNano` orders differently than PostgreSQL's canonical
///   span). Their ordering is likewise not Arrow's to compute.
pub fn arrow_type(ty: PgType, typmod: i32) -> DataType {
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
        PgType::Numeric => {
            let (precision, scale) = numeric_decimal(typmod);
            match decimal_width(precision) {
                DecimalWidth::Bits32 => DataType::Decimal32(precision, scale),
                DecimalWidth::Bits64 => DataType::Decimal64(precision, scale),
                DecimalWidth::Bits128 => DataType::Decimal128(precision, scale),
                DecimalWidth::Bits256 => DataType::Decimal256(precision, scale),
            }
        }
        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => DataType::Utf8,
        PgType::Bytea => DataType::Binary,
        PgType::Uuid => DataType::FixedSizeBinary(16),
        PgType::Date => DataType::Date32,
        PgType::Time => DataType::Time64(TimeUnit::Microsecond),
        PgType::TimeTz => DataType::Struct(timetz_fields()),
        PgType::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
        PgType::TimestampTz => DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
        PgType::Interval => DataType::Struct(interval_fields()),
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
                Field::new(
                    &column.name,
                    arrow_type(column.ty, column.typmod),
                    column.nullable,
                )
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
pub fn null_array(ty: PgType, typmod: i32, len: usize) -> ArrayRef {
    new_null_array(&arrow_type(ty, typmod), len)
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
        // A decimal of the width the precision asks for; the scale comes from
        // the column, not from the value. A value that does not fit is an
        // error rather than a rounding — see [`numeric_stored`], which the
        // columnar engines call on the way *in* so this cannot be the first
        // time a row is found unstorable.
        PgType::Numeric => {
            let (precision, scale) = numeric_decimal(column.typmod);
            macro_rules! decimal {
                ($builder:ty, $convert:expr) => {{
                    let mut builder = <$builder>::with_capacity(tuples.len())
                        .with_precision_and_scale(precision, scale)
                        .map_err(|error| corrupt(format!("numeric column type: {error}")))?;
                    for tuple in tuples {
                        match &tuple[index] {
                            Value::Null => builder.append_null(),
                            Value::Numeric(value) => {
                                builder.append_value($convert(value).ok_or_else(|| {
                                    numeric_overflow(&column.name, value, column.typmod)
                                })?)
                            }
                            _ => return Err(value_mismatch(&column.name, column.ty)),
                        }
                    }
                    Ok(Arc::new(builder.finish()) as ArrayRef)
                }};
            }
            let narrow = |value: &Numeric| value.to_scaled_i128(precision, scale);
            match decimal_width(precision) {
                DecimalWidth::Bits32 => decimal!(Decimal32Builder, |v| narrow(v).map(|n| n as i32)),
                DecimalWidth::Bits64 => decimal!(Decimal64Builder, |v| narrow(v).map(|n| n as i64)),
                DecimalWidth::Bits128 => decimal!(Decimal128Builder, narrow),
                // No Rust integer is this wide, so the value goes through its
                // own decimal rendering; `i256` parses the same digits back.
                DecimalWidth::Bits256 => decimal!(Decimal256Builder, |value: &Numeric| value
                    .to_scaled_string(precision, scale)
                    .and_then(|text| i256::from_string(&text))),
            }
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
            let mut builder = StructBuilder::new(
                timetz_fields(),
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
            let mut builder = StructBuilder::new(
                interval_fields(),
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
        .map(|(array, column)| array.unwrap_or_else(|| null_array(column.ty, column.typmod, rows)))
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
            let (precision, scale) = numeric_decimal(column.typmod);
            // The scale comes from the column, so the stored integer is all the
            // array has to carry — the same trade `build_array` made writing it.
            macro_rules! decimal {
                ($array:ty) => {{
                    let values = required_array::<$array>(array, &column.name)?;
                    Numeric::from_scaled_i128(values.value(row) as i128, scale)
                }};
            }
            let value = match decimal_width(precision) {
                DecimalWidth::Bits32 => decimal!(Decimal32Array),
                DecimalWidth::Bits64 => decimal!(Decimal64Array),
                DecimalWidth::Bits128 => decimal!(Decimal128Array),
                DecimalWidth::Bits256 => {
                    let values = required_array::<Decimal256Array>(array, &column.name)?;
                    Numeric::from_scaled_str(&values.value(row).to_string(), scale).ok_or_else(
                        || corrupt(format!("invalid numeric in column \"{}\"", column.name)),
                    )?
                }
            };
            Ok(Value::Numeric(value))
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

    fn parse(s: &str) -> Result<Numeric, StorageError> {
        Numeric::parse(s).map_err(|_| corrupt("test numeric"))
    }

    /// A decoded cell as PostgreSQL would print it. Used where the display
    /// scale is the point: `Numeric`'s equality is by value and ignores it.
    fn rendered(value: &Value) -> String {
        match value {
            Value::Numeric(n) => n.to_display(),
            other => panic!("not a numeric: {other:?}"),
        }
    }

    /// A `numeric(p, s)` column round trips **exactly**, display scale included:
    /// every value in such a column has been through `apply_typmod`, so its
    /// scale is `s` and the type carries it. One case per decimal width, since
    /// the width is picked from the precision and a wrong branch would decode
    /// against the wrong array type.
    #[test]
    fn a_constrained_numeric_round_trips_exactly() -> Result<(), StorageError> {
        for (precision, scale, text) in [
            (9i32, 2i32, "-1234567.89"),
            (18, 4, "12345678901234.5678"),
            (38, 10, "-1234567890123456789012345678.0123456789"),
            (76, 38, "12345678901234567890123456789012345678.5"),
        ] {
            let mut column = Column::new("c", PgType::Numeric);
            column.typmod = Numeric::pack_typmod(precision, scale);
            let schema = schema_of(vec![column]);
            let value = parse(text)?
                .apply_typmod(precision, scale)
                .map_err(|e| corrupt(e.message))?;
            let tuples = vec![vec![Value::Numeric(value.clone())], vec![Value::Null]];

            let batch = build_batch(&schema, &tuples)?;
            let decoded = decode_row(&schema, &[0], &batch, 0)?;
            // Rendered, not compared: `Numeric`'s equality is by value and
            // ignores the display scale, which is exactly what this asserts is
            // preserved.
            assert_eq!(
                rendered(&decoded[0]),
                value.to_display(),
                "numeric({precision},{scale})"
            );
            assert_eq!(decode_row(&schema, &[0], &batch, 1)?, vec![Value::Null]);
        }
        Ok(())
    }

    /// A column with **no** typmod is stored at one fixed scale, so the display
    /// scale is not preserved — `1.50` comes back as `1.5000000000000000`. The
    /// value is unchanged; only its rendering is. Deliberate: no fixed decimal
    /// can carry a per-value scale, and this is the documented deviation.
    #[test]
    fn an_unconstrained_numeric_keeps_the_value_but_not_its_scale() -> Result<(), StorageError> {
        let schema = schema_of(vec![Column::new("c", PgType::Numeric)]);
        let value = parse("1.50")?;
        let batch = build_batch(&schema, &[vec![Value::Numeric(value.clone())]])?;
        let decoded = decode_row(&schema, &[0], &batch, 0)?;

        assert_eq!(decoded[0], Value::Numeric(value));
        assert_eq!(rendered(&decoded[0]), "1.5000000000000000");
        Ok(())
    }

    /// The default scale is chosen so that ordinary division survives: PG gives
    /// a quotient 16 significant digits, so `10/3` fits and nothing rounds it
    /// away. A value needing more is refused rather than truncated.
    #[test]
    fn an_unconstrained_numeric_holds_a_quotient_and_refuses_a_finer_one()
    -> Result<(), StorageError> {
        let schema = schema_of(vec![Column::new("c", PgType::Numeric)]);
        let quotient = parse("3.3333333333333333")?;
        assert!(numeric_stored(&quotient, -1).is_some());
        let batch = build_batch(&schema, &[vec![Value::Numeric(quotient.clone())]])?;
        assert_eq!(
            rendered(&decode_row(&schema, &[0], &batch, 0)?[0]),
            "3.3333333333333333"
        );

        // 17 fractional digits: one past what the column can hold.
        let finer = parse("0.33333333333333333")?;
        assert!(numeric_stored(&finer, -1).is_none());
        assert!(matches!(
            build_batch(&schema, &[vec![Value::Numeric(finer)]]),
            Err(StorageError::NumericFieldOverflow { .. })
        ));
        Ok(())
    }

    /// NaN and ±Infinity are legal `numeric`s with no decimal image at all.
    #[test]
    fn numeric_specials_have_no_decimal_image() -> Result<(), StorageError> {
        let schema = schema_of(vec![Column::new("c", PgType::Numeric)]);
        for special in [Numeric::nan(), Numeric::pos_inf(), Numeric::neg_inf()] {
            assert!(numeric_stored(&special, -1).is_none());
            assert!(matches!(
                build_batch(&schema, &[vec![Value::Numeric(special)]]),
                Err(StorageError::NumericFieldOverflow { .. })
            ));
        }
        Ok(())
    }

    /// The decimal width follows the precision, the way ClickHouse's does. A
    /// column that changed width would still round trip within one process, but
    /// would silently stop matching the fragments already on disk.
    #[test]
    fn the_decimal_width_follows_the_precision() {
        let typmod = |p, s| Numeric::pack_typmod(p, s);
        assert_eq!(
            arrow_type(PgType::Numeric, typmod(9, 2)),
            DataType::Decimal32(9, 2)
        );
        assert_eq!(
            arrow_type(PgType::Numeric, typmod(18, 2)),
            DataType::Decimal64(18, 2)
        );
        assert_eq!(
            arrow_type(PgType::Numeric, typmod(38, 2)),
            DataType::Decimal128(38, 2)
        );
        assert_eq!(
            arrow_type(PgType::Numeric, typmod(76, 2)),
            DataType::Decimal256(76, 2)
        );
        assert_eq!(
            arrow_type(PgType::Numeric, -1),
            DataType::Decimal128(NUMERIC_DEFAULT_PRECISION, NUMERIC_DEFAULT_SCALE)
        );
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
        let array = null_array(PgType::Int4, -1, 3);
        assert_eq!(array.len(), 3);
        assert_eq!(array.null_count(), 3);
        assert_eq!(array.data_type(), &arrow_type(PgType::Int4, -1));
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
