//! Columnar batches and the kernels that evaluate expressions over them.
//!
//! This crate is the currency of the vectorized executor: a [`Batch`] is a run
//! of rows held column-at-a-time in Arrow arrays, and a [`VectorExpr`] is a
//! compiled expression that consumes one. It is a leaf — it knows PostgreSQL's
//! type system and Arrow, and nothing about plans, binders, transactions or
//! storage. That is what lets both the executor (above) and a storage engine
//! (below, for predicate pushdown) evaluate the same compiled expression.
//!
//! # Batches carry PostgreSQL values, not Arrow values
//!
//! A [`PgType`] does not determine an Arrow type and an Arrow type does not
//! determine a [`PgType`], so every column carries both: the type it *means* and
//! the [`ColumnEncoding`] it is *stored in*. Kernels dispatch on the former.
//! `date` and `timestamp` are rebased out of the Arrow epoch when a batch is
//! built (see [`epoch`]), so nothing above a scan has to know where its rows
//! came from.
//!
//! [`VectorExpr`]: crate::expr::VectorExpr

use std::borrow::Cow;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Float32Type, Float64Type, Int16Type, Int32Type, Int64Type, UInt64Type,
};
use arrow_array::{Array, ArrayRef, BooleanArray, UInt32Array};
use arrow_schema::DataType;
use crabgresql_types::{Interval, PgType, TimeTz, Value};

pub mod build;
pub mod encoding;
pub mod epoch;
pub mod expr;
pub mod kernels;

pub use encoding::{ColumnEncoding, batch_type_of, encoding_of, storage_type_of};
pub use expr::{Selection, VectorExpr, eval_batch};
pub use kernels::{ArithOp, CmpOp};

/// An error raised while building or evaluating a batch.
///
/// Mirrors the executor's `ExecError` field for field so a fault inside a kernel
/// reaches the client with the same SQLSTATE, message, DETAIL and HINT the row
/// engine would have produced. A vectorized plan must never be observable
/// through its error text.
#[derive(Clone, Debug)]
pub struct BatchError {
    pub code: Cow<'static, str>,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
}

impl BatchError {
    pub fn new(code: impl Into<Cow<'static, str>>, message: impl Into<String>) -> Self {
        BatchError {
            code: code.into(),
            message: message.into(),
            detail: None,
            hint: None,
        }
    }

    /// `XX000 internal_error`: a batch whose shape contradicts its schema, which
    /// is a bug in this crate or its caller rather than anything a user did.
    pub fn internal(message: impl Into<String>) -> Self {
        BatchError::new("XX000", message)
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl std::fmt::Display for BatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BatchError {}

/// One column's identity within a batch.
#[derive(Clone, Debug)]
pub struct BatchField {
    /// The relation column this came from, when it came from one. `None` for a
    /// computed column, which has no name to report.
    pub name: Option<String>,
    /// The PostgreSQL type this column means. Kernels dispatch on this.
    pub ty: PgType,
    pub typmod: i32,
    pub nullable: bool,
    pub encoding: ColumnEncoding,
}

impl BatchField {
    /// A field for `ty`, or `None` if no batch can carry it.
    pub fn new(name: Option<String>, ty: PgType, typmod: i32, nullable: bool) -> Option<Self> {
        Some(BatchField {
            name,
            ty,
            typmod,
            nullable,
            encoding: encoding_of(ty)?,
        })
    }
}

/// The shape of every batch a node produces: fixed for that node's whole life,
/// because expression compilation resolved column positions against it before
/// execution began.
#[derive(Clone, Debug)]
pub struct BatchSchema {
    fields: Arc<[BatchField]>,
    /// For a batch read from a relation, the schema ordinal each column holds —
    /// ascending, matching the scan's `ColumnProjection`. `None` for a computed
    /// batch, whose columns correspond to nothing in a relation.
    ///
    /// This is what lets a *narrow* batch coexist with a row engine that
    /// addresses columns by relation position: the compiler rewrites indices
    /// once, rather than every row carrying padding to keep them valid.
    slots: Option<Arc<[usize]>>,
}

impl BatchSchema {
    pub fn new(fields: Vec<BatchField>) -> Self {
        BatchSchema {
            fields: fields.into(),
            slots: None,
        }
    }

    /// A schema for a scan: `slots[i]` is the relation ordinal of column `i`.
    pub fn scan(fields: Vec<BatchField>, slots: Vec<usize>) -> Result<Self, BatchError> {
        if fields.len() != slots.len() {
            return Err(BatchError::internal(format!(
                "batch schema has {} fields but {} slots",
                fields.len(),
                slots.len()
            )));
        }
        Ok(BatchSchema {
            fields: fields.into(),
            slots: Some(slots.into()),
        })
    }

    pub fn fields(&self) -> &[BatchField] {
        &self.fields
    }

    pub fn width(&self) -> usize {
        self.fields.len()
    }

    pub fn field(&self, index: usize) -> Option<&BatchField> {
        self.fields.get(index)
    }

    pub fn slots(&self) -> Option<&[usize]> {
        self.slots.as_deref()
    }

    /// The batch column holding relation ordinal `slot`, if this schema came
    /// from a scan that read it.
    pub fn position_of(&self, slot: usize) -> Option<usize> {
        self.slots.as_ref()?.iter().position(|&s| s == slot)
    }
}

/// A columnar run of rows.
///
/// Dense: there is no selection vector here. Filters compact, and the selection
/// machinery that expression evaluation needs (to keep a faulting kernel off
/// rows the row engine would never have evaluated) lives inside
/// [`expr`](crate::expr) and never crosses a node boundary. Admitting "may be
/// selected" at the boundary would put a second path into every kernel forever,
/// and removing it later would not.
#[derive(Clone, Debug)]
pub struct Batch {
    schema: BatchSchema,
    columns: Vec<ArrayRef>,
    len: usize,
    /// Row identity, only when a scan was asked for it. `None` on every path
    /// today — the vectorized engine is read-only — so the field exists to keep
    /// delta-chunk UPDATE/DELETE an implementation rather than a redesign.
    row_ids: Option<ArrayRef>,
}

impl Batch {
    /// Build a batch, checking every column against the type its field claims.
    ///
    /// The check is not paranoia: `numeric` and `text` are both `Utf8`, so a
    /// mismatched *field* would not be caught by Arrow and would hand a kernel
    /// the wrong semantics for bytes it can happily read.
    pub fn new(
        schema: BatchSchema,
        columns: Vec<ArrayRef>,
        len: usize,
    ) -> Result<Self, BatchError> {
        if columns.len() != schema.width() {
            return Err(BatchError::internal(format!(
                "batch has {} columns but its schema has {}",
                columns.len(),
                schema.width()
            )));
        }
        for (index, column) in columns.iter().enumerate() {
            let field = schema
                .field(index)
                .ok_or_else(|| BatchError::internal("batch schema shrank while building"))?;
            let expected = batch_type_of(field.ty).ok_or_else(|| {
                BatchError::internal(format!("{} has no batch representation", field.ty.name()))
            })?;
            if column.data_type() != &expected {
                return Err(BatchError::internal(format!(
                    "batch column {index} holds {} but {} requires {expected}",
                    column.data_type(),
                    field.ty.name(),
                )));
            }
            if column.len() != len {
                return Err(BatchError::internal(format!(
                    "batch column {index} has {} rows but the batch has {len}",
                    column.len()
                )));
            }
        }
        Ok(Batch {
            schema,
            columns,
            len,
            row_ids: None,
        })
    }

    pub fn with_row_ids(mut self, row_ids: ArrayRef) -> Result<Self, BatchError> {
        if row_ids.len() != self.len {
            return Err(BatchError::internal("row id column has the wrong length"));
        }
        self.row_ids = Some(row_ids);
        Ok(self)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn schema(&self) -> &BatchSchema {
        &self.schema
    }

    pub fn columns(&self) -> &[ArrayRef] {
        &self.columns
    }

    pub fn column(&self, index: usize) -> Option<&ArrayRef> {
        self.columns.get(index)
    }

    /// The tid of row `i`, if this batch was asked to carry identity.
    pub fn row_id(&self, i: usize) -> Option<u64> {
        let ids = self.row_ids.as_ref()?;
        let ids = ids.as_primitive_opt::<UInt64Type>()?;
        (i < ids.len() && !ids.is_null(i)).then(|| ids.value(i))
    }

    /// A zero-copy window onto `len` rows starting at `offset`. Arrow arrays
    /// share buffers, so this is bookkeeping rather than data movement.
    pub fn slice(&self, offset: usize, len: usize) -> Result<Batch, BatchError> {
        if offset + len > self.len {
            return Err(BatchError::internal("batch slice out of bounds"));
        }
        Ok(Batch {
            schema: self.schema.clone(),
            columns: self.columns.iter().map(|c| c.slice(offset, len)).collect(),
            len,
            row_ids: self.row_ids.as_ref().map(|ids| ids.slice(offset, len)),
        })
    }

    /// Keep the rows where `mask` is true. Null mask entries drop their row,
    /// matching the row engine's `predicate_holds`, where only `Bool(true)`
    /// passes and an unknown is not a pass.
    pub fn filter(&self, mask: &BooleanArray) -> Result<Batch, BatchError> {
        if mask.len() != self.len {
            return Err(BatchError::internal(
                "filter mask length does not match the batch",
            ));
        }
        let columns = self
            .columns
            .iter()
            .map(|column| {
                arrow_select::filter::filter(column.as_ref(), mask)
                    .map_err(|error| BatchError::internal(format!("filter batch: {error}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let len = columns.first().map_or(0, |c| c.len());
        let row_ids = match &self.row_ids {
            Some(ids) => Some(
                arrow_select::filter::filter(ids.as_ref(), mask)
                    .map_err(|error| BatchError::internal(format!("filter row ids: {error}")))?,
            ),
            None => None,
        };
        Ok(Batch {
            schema: self.schema.clone(),
            columns,
            len,
            row_ids,
        })
    }

    /// Gather the rows named by `indices`, in that order.
    pub fn take(&self, indices: &UInt32Array) -> Result<Batch, BatchError> {
        let columns = self
            .columns
            .iter()
            .map(|column| {
                arrow_select::take::take(column.as_ref(), indices, None)
                    .map_err(|error| BatchError::internal(format!("take from batch: {error}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let row_ids = match &self.row_ids {
            Some(ids) => Some(
                arrow_select::take::take(ids.as_ref(), indices, None)
                    .map_err(|error| BatchError::internal(format!("take row ids: {error}")))?,
            ),
            None => None,
        };
        Ok(Batch {
            schema: self.schema.clone(),
            columns,
            len: indices.len(),
            row_ids,
        })
    }

    /// One cell as the row engine would see it.
    ///
    /// The bridge back to row-at-a-time execution: aggregation reads through
    /// this so it can reuse the row engine's own accumulators, and
    /// devectorization reads through it to rebuild tuples for the wire.
    pub fn value_at(&self, column: usize, row: usize) -> Result<Value, BatchError> {
        let field = self
            .schema
            .field(column)
            .ok_or_else(|| BatchError::internal(format!("no batch column {column}")))?;
        let array = self
            .columns
            .get(column)
            .ok_or_else(|| BatchError::internal(format!("no batch column {column}")))?;
        value_of(array.as_ref(), field.ty, row)
    }

    /// Keep the rows where `mask` — which must be a boolean array — is true.
    ///
    /// The typed twin of [`Batch::filter`], so a caller holding the untyped
    /// result of an expression does not need to name an Arrow type to use it.
    pub fn filter_by(&self, mask: &ArrayRef) -> Result<Batch, BatchError> {
        let mask = mask.as_boolean_opt().ok_or_else(|| {
            BatchError::internal(format!(
                "expected a boolean filter mask, found {}",
                mask.data_type()
            ))
        })?;
        self.filter(mask)
    }

    /// Rebuild row `i` as a full-width relation tuple, `width` wide.
    ///
    /// Slots this batch did not read keep the `Null` they were initialized with
    /// — exactly the contract a projected row scan already has, where unread
    /// positions hold unspecified values.
    pub fn row_into(
        &self,
        row: usize,
        width: usize,
        out: &mut Vec<Value>,
    ) -> Result<(), BatchError> {
        out.clear();
        out.resize(width, Value::Null);
        let slots = self.schema.slots().ok_or_else(|| {
            BatchError::internal("cannot rebuild a relation tuple from a computed batch")
        })?;
        for (column, &slot) in slots.iter().enumerate() {
            let slot = out
                .get_mut(slot)
                .ok_or_else(|| BatchError::internal("batch slot is outside the relation"))?;
            *slot = self.value_at(column, row)?;
        }
        Ok(())
    }
}

/// One cell of `array`, read as `ty`.
///
/// Dispatches on the PostgreSQL type, never on the Arrow type — `numeric`,
/// `text` and `bpchar` are all `Utf8` and mean three different things, so the
/// array cannot say which rules apply to its own bytes.
pub fn value_of(array: &dyn Array, ty: PgType, row: usize) -> Result<Value, BatchError> {
    if row >= array.len() {
        return Err(BatchError::internal("batch row index out of bounds"));
    }
    if array.is_null(row) {
        return Ok(Value::Null);
    }
    let mismatch = || {
        BatchError::internal(format!(
            "batch column holds {} where {} was expected",
            array.data_type(),
            ty.name()
        ))
    };
    let value = match ty {
        PgType::Bool => Value::Bool(array.as_boolean_opt().ok_or_else(mismatch)?.value(row)),
        PgType::Int2 => Value::Int2(
            array
                .as_primitive_opt::<Int16Type>()
                .ok_or_else(mismatch)?
                .value(row),
        ),
        PgType::Int4 => Value::Int4(
            array
                .as_primitive_opt::<Int32Type>()
                .ok_or_else(mismatch)?
                .value(row),
        ),
        PgType::Int8 => Value::Int8(
            array
                .as_primitive_opt::<Int64Type>()
                .ok_or_else(mismatch)?
                .value(row),
        ),
        PgType::Float4 => Value::Float4(
            array
                .as_primitive_opt::<Float32Type>()
                .ok_or_else(mismatch)?
                .value(row),
        ),
        PgType::Float8 => Value::Float8(
            array
                .as_primitive_opt::<Float64Type>()
                .ok_or_else(mismatch)?
                .value(row),
        ),
        // Already in the PostgreSQL epoch: a batch rebases once, at the scan.
        PgType::Date => Value::Date(
            array
                .as_primitive_opt::<Int32Type>()
                .ok_or_else(mismatch)?
                .value(row),
        ),
        PgType::Time => Value::Time(
            array
                .as_primitive_opt::<Int64Type>()
                .ok_or_else(mismatch)?
                .value(row),
        ),
        PgType::Timestamp => Value::Timestamp(
            array
                .as_primitive_opt::<Int64Type>()
                .ok_or_else(mismatch)?
                .value(row),
        ),
        PgType::TimestampTz => Value::TimestampTz(
            array
                .as_primitive_opt::<Int64Type>()
                .ok_or_else(mismatch)?
                .value(row),
        ),
        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => Value::Text(
            array
                .as_string_opt::<i32>()
                .ok_or_else(mismatch)?
                .value(row)
                .to_string(),
        ),
        PgType::Numeric => {
            let text = array
                .as_string_opt::<i32>()
                .ok_or_else(mismatch)?
                .value(row);
            crabgresql_types::numeric::Numeric::parse(text)
                .map(Value::Numeric)
                .map_err(|_| {
                    BatchError::internal(format!("batch column holds an invalid numeric {text:?}"))
                })?
        }
        PgType::Bytea => Value::Bytea(
            array
                .as_binary_opt::<i32>()
                .ok_or_else(mismatch)?
                .value(row)
                .to_vec(),
        ),
        PgType::Uuid => {
            let bytes = array
                .as_fixed_size_binary_opt()
                .ok_or_else(mismatch)?
                .value(row);
            Value::Uuid(
                bytes
                    .try_into()
                    .map_err(|_| BatchError::internal("batch column holds an invalid uuid"))?,
            )
        }
        PgType::TimeTz => {
            let fields = array.as_struct_opt().ok_or_else(mismatch)?;
            Value::TimeTz(TimeTz {
                usec: struct_i64(fields, 0, row).ok_or_else(mismatch)?,
                zone: struct_i32(fields, 1, row).ok_or_else(mismatch)?,
            })
        }
        PgType::Interval => {
            let fields = array.as_struct_opt().ok_or_else(mismatch)?;
            Value::Interval(Interval {
                months: struct_i32(fields, 0, row).ok_or_else(mismatch)?,
                days: struct_i32(fields, 1, row).ok_or_else(mismatch)?,
                usec: struct_i64(fields, 2, row).ok_or_else(mismatch)?,
            })
        }
        other => {
            return Err(BatchError::internal(format!(
                "{} has no batch representation",
                other.name()
            )));
        }
    };
    Ok(value)
}

fn struct_i32(fields: &arrow_array::StructArray, child: usize, row: usize) -> Option<i32> {
    Some(
        fields
            .column(child)
            .as_primitive_opt::<Int32Type>()?
            .value(row),
    )
}

fn struct_i64(fields: &arrow_array::StructArray, child: usize, row: usize) -> Option<i64> {
    Some(
        fields
            .column(child)
            .as_primitive_opt::<Int64Type>()?
            .value(row),
    )
}

/// The Arrow type a batch column must hold for `field`.
pub fn required_type(field: &BatchField) -> Option<DataType> {
    batch_type_of(field.ty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{Int32Array, StringArray};

    fn field(ty: PgType) -> BatchField {
        BatchField::new(Some("c".into()), ty, -1, true).expect("encodable")
    }

    fn batch(fields: Vec<BatchField>, columns: Vec<ArrayRef>, len: usize) -> Batch {
        Batch::new(BatchSchema::new(fields), columns, len).expect("valid batch")
    }

    #[test]
    fn a_column_whose_arrow_type_contradicts_its_pg_type_is_refused() {
        let columns: Vec<ArrayRef> = vec![Arc::new(StringArray::from(vec!["1"]))];
        let error = Batch::new(BatchSchema::new(vec![field(PgType::Int4)]), columns, 1)
            .expect_err("int4 cannot be Utf8");
        assert!(error.message.contains("Utf8"), "{}", error.message);
    }

    /// `numeric` and `text` share `Utf8`, so Arrow cannot catch a swapped field
    /// and only the declared `PgType` decides how the bytes are read.
    #[test]
    fn same_arrow_type_yields_different_values_per_pg_type() {
        let array: ArrayRef = Arc::new(StringArray::from(vec!["1.50"]));
        let as_text = batch(vec![field(PgType::Text)], vec![Arc::clone(&array)], 1);
        let as_numeric = batch(vec![field(PgType::Numeric)], vec![array], 1);

        assert_eq!(as_text.value_at(0, 0).expect("text"), Value::Text("1.50".into()));
        let Value::Numeric(n) = as_numeric.value_at(0, 0).expect("numeric") else {
            panic!("expected a numeric");
        };
        assert_eq!(n.to_display(), "1.50");
    }

    #[test]
    fn dates_are_read_straight_out_of_the_batch_without_a_second_rebase() {
        // The batch is already in the PostgreSQL domain, so 4930 must come back
        // as 4930 — rebasing here as well would shift it a second time.
        let columns: Vec<ArrayRef> = vec![Arc::new(Int32Array::from(vec![4_930]))];
        let batch = batch(vec![field(PgType::Date)], columns, 1);
        assert_eq!(batch.value_at(0, 0).expect("date"), Value::Date(4_930));
    }

    #[test]
    fn nulls_read_back_as_null_whatever_the_type() {
        for ty in [PgType::Int4, PgType::Text, PgType::Date, PgType::Float8] {
            let array = arrow_array::new_null_array(&batch_type_of(ty).expect("type"), 1);
            let batch = batch(vec![field(ty)], vec![array], 1);
            assert_eq!(batch.value_at(0, 0).expect("null"), Value::Null, "{ty:?}");
        }
    }

    #[test]
    fn filtering_drops_null_mask_entries_like_the_row_engine_does() {
        let columns: Vec<ArrayRef> = vec![Arc::new(Int32Array::from(vec![1, 2, 3]))];
        let batch = batch(vec![field(PgType::Int4)], columns, 3);
        let mask = BooleanArray::from(vec![Some(true), None, Some(false)]);
        let kept = batch.filter(&mask).expect("filter");
        assert_eq!(kept.len(), 1);
        assert_eq!(kept.value_at(0, 0).expect("kept"), Value::Int4(1));
    }

    #[test]
    fn rebuilding_a_tuple_leaves_unread_slots_null() {
        let columns: Vec<ArrayRef> = vec![Arc::new(Int32Array::from(vec![7]))];
        let schema = BatchSchema::scan(vec![field(PgType::Int4)], vec![2]).expect("scan schema");
        let batch = Batch::new(schema, columns, 1).expect("valid batch");
        let mut row = Vec::new();
        batch.row_into(0, 4, &mut row).expect("rebuild");
        assert_eq!(row, vec![Value::Null, Value::Null, Value::Int4(7), Value::Null]);
    }

    #[test]
    fn a_computed_batch_cannot_be_rebuilt_as_a_relation_tuple() {
        let columns: Vec<ArrayRef> = vec![Arc::new(Int32Array::from(vec![1]))];
        let batch = batch(vec![field(PgType::Int4)], columns, 1);
        let mut row = Vec::new();
        assert!(batch.row_into(0, 1, &mut row).is_err());
    }

    #[test]
    fn slicing_is_a_window_not_a_copy() {
        let columns: Vec<ArrayRef> = vec![Arc::new(Int32Array::from(vec![1, 2, 3, 4]))];
        let batch = batch(vec![field(PgType::Int4)], columns, 4);
        let window = batch.slice(1, 2).expect("slice");
        assert_eq!(window.len(), 2);
        assert_eq!(window.value_at(0, 0).expect("first"), Value::Int4(2));
        assert!(batch.slice(3, 2).is_err());
    }
}
