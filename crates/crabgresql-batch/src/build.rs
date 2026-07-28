//! Building a [`Batch`] from already-decoded rows.
//!
//! The inverse of [`value_of`](crate::value_of), and the reason a storage engine
//! with no columnar form can still take part in a vectorized pipeline. The
//! Parquet access method needs exactly this: a relation's rows live partly in
//! immutable columnar chunks and partly in a RAM write buffer, and the two are
//! read as one pipeline.
//!
//! Going through [`Value`] rather than through the buffer's internal layout is
//! what makes the two halves agree. A `Value::Date` is already in the
//! PostgreSQL domain, so a buffered row lands on the same integer as a chunk's
//! rebased column — and `GROUP BY d` reports one group per date rather than two.

use std::sync::Arc;

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, FixedSizeBinaryBuilder, Float32Builder, Float64Builder,
    Int16Builder, Int32Builder, Int64Builder, StringBuilder,
};
use arrow_array::{ArrayRef, StructArray};
use arrow_schema::{DataType, Field};
use crabgresql_types::{PgType, Value};

use crate::{Batch, BatchError, BatchSchema, batch_type_of};

/// Assemble `rows` — full-width relation tuples — into a batch holding only the
/// columns `slots` names.
pub fn from_rows(
    schema: &BatchSchema,
    slots: &[usize],
    rows: &[Vec<Value>],
) -> Result<Batch, BatchError> {
    if slots.len() != schema.width() {
        return Err(BatchError::internal(format!(
            "batch schema has {} fields but {} slots were given",
            schema.width(),
            slots.len()
        )));
    }
    let mut columns = Vec::with_capacity(slots.len());
    for (position, &slot) in slots.iter().enumerate() {
        let field = schema
            .field(position)
            .ok_or_else(|| BatchError::internal("batch schema shrank while building"))?;
        columns.push(build_column(field.ty, slot, rows)?);
    }
    Batch::new(schema.clone(), columns, rows.len())
}

/// One column of `ty`, gathered from position `slot` of each row.
fn build_column(ty: PgType, slot: usize, rows: &[Vec<Value>]) -> Result<ArrayRef, BatchError> {
    fn at(row: &[Value], slot: usize) -> Result<&Value, BatchError> {
        row.get(slot)
            .ok_or_else(|| BatchError::internal(format!("row is too narrow for column {slot}")))
    }
    let mismatch = |value: &Value| {
        BatchError::internal(format!(
            "column {slot} holds {value:?} where {} was expected",
            ty.name()
        ))
    };

    /// Fill a builder from one `Value` variant, appending a null for
    /// `Value::Null` and refusing anything else.
    macro_rules! fill {
        ($builder:expr, $pattern:pat => $extract:expr) => {{
            let mut builder = $builder;
            for row in rows {
                match at(row, slot)? {
                    Value::Null => builder.append_null(),
                    $pattern => builder.append_value($extract),
                    other => return Err(mismatch(other)),
                }
            }
            Arc::new(builder.finish()) as ArrayRef
        }};
    }

    let array: ArrayRef = match ty {
        PgType::Bool => fill!(BooleanBuilder::new(), Value::Bool(v) => *v),
        PgType::Int2 => fill!(Int16Builder::new(), Value::Int2(v) => *v),
        PgType::Int4 => fill!(Int32Builder::new(), Value::Int4(v) => *v),
        PgType::Int8 => fill!(Int64Builder::new(), Value::Int8(v) => *v),
        PgType::Float4 => fill!(Float32Builder::new(), Value::Float4(v) => *v),
        PgType::Float8 => fill!(Float64Builder::new(), Value::Float8(v) => *v),
        // Already PostgreSQL-domain: a `Value::Date` counts from 2000-01-01, so
        // there is nothing to rebase. This is the whole reason rows are the
        // meeting point between the two storage halves.
        PgType::Date => fill!(Int32Builder::new(), Value::Date(v) => *v),
        PgType::Time => fill!(Int64Builder::new(), Value::Time(v) => *v),
        PgType::Timestamp => fill!(Int64Builder::new(), Value::Timestamp(v) => *v),
        PgType::TimestampTz => fill!(Int64Builder::new(), Value::TimestampTz(v) => *v),
        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => {
            fill!(StringBuilder::new(), Value::Text(v) => v.as_str())
        }
        PgType::Numeric => {
            fill!(StringBuilder::new(), Value::Numeric(v) => v.to_display())
        }
        PgType::Bytea => fill!(BinaryBuilder::new(), Value::Bytea(v) => v.as_slice()),
        PgType::Uuid => {
            let mut builder = FixedSizeBinaryBuilder::new(16);
            for row in rows {
                match at(row, slot)? {
                    Value::Null => builder.append_null(),
                    Value::Uuid(v) => builder
                        .append_value(v)
                        .map_err(|error| BatchError::internal(format!("build uuid: {error}")))?,
                    other => return Err(mismatch(other)),
                }
            }
            Arc::new(builder.finish())
        }
        PgType::TimeTz => {
            let mut usec = Int64Builder::new();
            let mut zone = Int32Builder::new();
            let mut valid = Vec::with_capacity(rows.len());
            for row in rows {
                match at(row, slot)? {
                    Value::Null => {
                        usec.append_value(0);
                        zone.append_value(0);
                        valid.push(false);
                    }
                    Value::TimeTz(v) => {
                        usec.append_value(v.usec);
                        zone.append_value(v.zone);
                        valid.push(true);
                    }
                    other => return Err(mismatch(other)),
                }
            }
            struct_array(
                &[("time_us", DataType::Int64), ("offset_seconds", DataType::Int32)],
                vec![Arc::new(usec.finish()), Arc::new(zone.finish())],
                &valid,
            )?
        }
        PgType::Interval => {
            let mut months = Int32Builder::new();
            let mut days = Int32Builder::new();
            let mut micros = Int64Builder::new();
            let mut valid = Vec::with_capacity(rows.len());
            for row in rows {
                match at(row, slot)? {
                    Value::Null => {
                        months.append_value(0);
                        days.append_value(0);
                        micros.append_value(0);
                        valid.push(false);
                    }
                    Value::Interval(v) => {
                        months.append_value(v.months);
                        days.append_value(v.days);
                        micros.append_value(v.usec);
                        valid.push(true);
                    }
                    other => return Err(mismatch(other)),
                }
            }
            struct_array(
                &[
                    ("months", DataType::Int32),
                    ("days", DataType::Int32),
                    ("micros", DataType::Int64),
                ],
                vec![
                    Arc::new(months.finish()),
                    Arc::new(days.finish()),
                    Arc::new(micros.finish()),
                ],
                &valid,
            )?
        }
        other => {
            return Err(BatchError::internal(format!(
                "{} has no batch representation",
                other.name()
            )));
        }
    };
    debug_assert_eq!(
        Some(array.data_type().clone()),
        batch_type_of(ty),
        "built column disagrees with the declared batch type"
    );
    Ok(array)
}

fn struct_array(
    fields: &[(&str, DataType)],
    children: Vec<ArrayRef>,
    valid: &[bool],
) -> Result<ArrayRef, BatchError> {
    let fields: Vec<Field> = fields
        .iter()
        .map(|(name, ty)| Field::new(*name, ty.clone(), false))
        .collect();
    let nulls = arrow_buffer::NullBuffer::from(valid.to_vec());
    StructArray::try_new(fields.into(), children, Some(nulls))
        .map(|array| Arc::new(array) as ArrayRef)
        .map_err(|error| BatchError::internal(format!("build struct column: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BatchField;

    fn schema(types: &[PgType]) -> BatchSchema {
        let fields = types
            .iter()
            .enumerate()
            .map(|(i, ty)| {
                BatchField::new(Some(format!("c{i}")), *ty, -1, true).expect("encodable")
            })
            .collect();
        BatchSchema::scan(fields, (0..types.len()).collect()).expect("scan schema")
    }

    /// Every value must survive rows → batch → rows unchanged, or the two
    /// halves of a Parquet relation would disagree about their own data.
    #[test]
    fn rows_round_trip_through_a_batch() {
        let types = [
            PgType::Bool,
            PgType::Int4,
            PgType::Int8,
            PgType::Float8,
            PgType::Text,
            PgType::Date,
            PgType::Timestamp,
            PgType::Numeric,
        ];
        let rows = vec![
            vec![
                Value::Bool(true),
                Value::Int4(-1),
                Value::Int8(1 << 40),
                Value::Float8(-0.0),
                Value::Text("hi".into()),
                Value::Date(4_930),
                Value::Timestamp(-1),
                Value::Numeric(crabgresql_types::numeric::Numeric::parse("1.00").expect("n")),
            ],
            vec![Value::Null; 8],
            vec![
                Value::Bool(false),
                Value::Int4(i32::MAX),
                Value::Int8(i64::MIN),
                Value::Float8(f64::NAN),
                Value::Text(String::new()),
                // The infinity sentinels must survive as themselves.
                Value::Date(i32::MAX),
                Value::Timestamp(i64::MIN),
                Value::Numeric(crabgresql_types::numeric::Numeric::parse("0").expect("n")),
            ],
        ];
        let schema = schema(&types);
        let slots: Vec<usize> = (0..types.len()).collect();
        let batch = from_rows(&schema, &slots, &rows).expect("build");
        assert_eq!(batch.len(), 3);

        for (index, row) in rows.iter().enumerate() {
            let mut out = Vec::new();
            batch.row_into(index, types.len(), &mut out).expect("read back");
            for (column, (expected, actual)) in row.iter().zip(&out).enumerate() {
                match (expected, actual) {
                    // NaN is not equal to itself under `PartialEq`, so compare it
                    // as a bit pattern; the row engine's total order treats every
                    // NaN alike, which is what grouping relies on.
                    (Value::Float8(a), Value::Float8(b)) if a.is_nan() => {
                        assert!(b.is_nan(), "column {column}");
                    }
                    (a, b) => assert_eq!(a, b, "column {column} of row {index}"),
                }
            }
        }
    }

    /// A batch may hold a subset of a row's columns, in the projection's order.
    #[test]
    fn a_narrow_batch_reads_only_the_slots_it_was_given() {
        let fields = vec![
            BatchField::new(Some("c2".into()), PgType::Date, -1, true).expect("encodable"),
        ];
        let schema = BatchSchema::scan(fields, vec![2]).expect("scan schema");
        let rows = vec![vec![
            Value::Int4(1),
            Value::Text("skip".into()),
            Value::Date(7),
        ]];
        let batch = from_rows(&schema, &[2], &rows).expect("build");
        assert_eq!(batch.value_at(0, 0).expect("value"), Value::Date(7));

        let mut out = Vec::new();
        batch.row_into(0, 3, &mut out).expect("read back");
        assert_eq!(out, vec![Value::Null, Value::Null, Value::Date(7)]);
    }

    #[test]
    fn a_value_of_the_wrong_type_is_refused_rather_than_coerced() {
        let schema = schema(&[PgType::Int4]);
        let rows = vec![vec![Value::Text("not an int".into())]];
        assert!(from_rows(&schema, &[0], &rows).is_err());
    }
}
