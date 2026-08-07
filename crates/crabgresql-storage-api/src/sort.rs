//! Columnar sorting, shared by the vectorized executor and by the columnar
//! engines' write path.
//!
//! It lives here rather than in the executor because the two callers sit on
//! opposite sides of the crate graph — `crabgresql-parquet-engine` cannot see
//! `crabgresql-executor`, and both see this crate. What must not be duplicated
//! is the *semantics*: [`canonicalize`]'s `-0.0`/NaN rewrite and the stability
//! tiebreak are what make Arrow's total order coincide with PostgreSQL's, and a
//! second copy of either would be a second place for the two to drift apart.
//!
//! A batch handed to these functions is in `Value` semantics (see
//! [`crate::arrow`]'s epoch invariant), which is exactly the order a sort has
//! to reproduce.

use std::sync::Arc;

use arrow_array::{Array, ArrayRef, Float32Array, Float64Array, RecordBatch, UInt32Array};
use arrow_ord::sort::{SortColumn, SortOptions, lexsort_to_indices};
use arrow_select::take::take;
use crabgresql_types::{PgType, collation};

use crate::{Column, IndexKey, StorageError, TableSchema};

/// Whether Arrow's total order over a column of `ty` under `collation` is
/// PostgreSQL's order.
///
/// The float types are included although Arrow's raw order differs from
/// PostgreSQL's: [`sort_permutation`] canonicalizes the key column first, which
/// makes the two coincide. `"char"` is included because [`crate::arrow`] stores
/// it as `UInt8` precisely so that Arrow's order is the type's own unsigned
/// one. `numeric` is excluded and cannot be included — it is stored as `Utf8`,
/// so an Arrow comparison on it is a string comparison; `bpchar` ignores
/// trailing blanks, which byte order does not; `timetz` and `interval` are
/// `Struct`s, which no ordering kernel accepts.
pub fn sortable(ty: PgType, collation: u32) -> bool {
    match ty {
        PgType::Bool
        | PgType::Char
        | PgType::Int2
        | PgType::Int4
        | PgType::Int8
        | PgType::Float4
        | PgType::Float8
        | PgType::Date
        | PgType::Time
        | PgType::Timestamp
        | PgType::TimestampTz
        | PgType::Bytea
        | PgType::Uuid => true,
        PgType::Text | PgType::Varchar | PgType::Name => collation::is_byte_order(collation),
        _ => false,
    }
}

/// The first column `keys` names that this module cannot order, or `None` when
/// every key column is fine.
///
/// The one place that walks a key deciding this, so the DDL gate that reports
/// the offending column and the write path that falls back to insertion order
/// can never disagree about which keys are honored. It takes the columns and
/// the keys separately because DDL asks before the key is on the schema.
///
/// A key index out of range yields `None` — that is the range check's error to
/// report, not this function's, and [`sortable_layout`] still calls such a
/// layout unsortable.
pub fn unsortable_column<'a>(columns: &'a [Column], keys: &[IndexKey]) -> Option<&'a Column> {
    keys.iter().find_map(|key| {
        let column = columns.get(key.column)?;
        let collation = column.collation.unwrap_or(collation::DEFAULT_COLLATION_OID);
        (!sortable(column.ty, collation)).then_some(column)
    })
}

/// Whether `schema`'s layout sort key names only columns this module can sort.
///
/// A key that fails here is not an error at write time: the relation is stored
/// in insertion order instead. DDL rejects such a key going forward, but a
/// relation created before that check still has to accept writes.
pub fn sortable_layout(schema: &TableSchema) -> bool {
    schema
        .sort_key
        .iter()
        .all(|key| key.column < schema.columns.len())
        && unsortable_column(&schema.columns, &schema.sort_key).is_none()
}

/// The permutation that puts `batch` in `keys` order: entry `p` is the index of
/// the input row that belongs at output position `p`.
///
/// **Stability.** `lexsort_to_indices` is not stable, so a final position key is
/// appended: rows every real key calls equal then resolve by input position,
/// which *is* stability, expressed as a comparison. PostgreSQL's sort is stable
/// for keys with no tiebreak too.
///
/// Callers must have checked [`sortable`] for every key; a key naming a column
/// Arrow cannot order fails here rather than silently ordering it wrong.
pub fn sort_permutation(
    batch: &RecordBatch,
    keys: &[IndexKey],
) -> Result<UInt32Array, StorageError> {
    let mut columns: Vec<SortColumn> = keys
        .iter()
        .map(|key| {
            let values = batch
                .columns()
                .get(key.column)
                .ok_or_else(|| {
                    StorageError::CorruptData(format!(
                        "sort key names column {} of a {}-column batch",
                        key.column,
                        batch.num_columns()
                    ))
                })
                .map(canonicalize)?;
            Ok(SortColumn {
                values,
                options: Some(SortOptions {
                    descending: key.descending,
                    // PostgreSQL's NULLS FIRST/LAST is independent of ASC/DESC;
                    // Arrow's `nulls_first` is too, so it maps straight across.
                    nulls_first: key.nulls_first,
                }),
            })
        })
        .collect::<Result<_, StorageError>>()?;
    // The stability tiebreak. Ascending with no nulls, so it only ever decides
    // between rows every real key called equal.
    let positions: UInt32Array = (0..batch.num_rows() as u32).collect::<Vec<_>>().into();
    columns.push(SortColumn {
        values: Arc::new(positions),
        options: Some(SortOptions {
            descending: false,
            nulls_first: false,
        }),
    });
    lexsort_to_indices(&columns, None)
        .map_err(|error| StorageError::Io(format!("sort failed: {error}")))
}

/// `take` the first `width` columns of `batch` through `indices`. The caller
/// rebuilds the batch, because the schema it wants may be narrower than
/// `batch`'s — an executor sort drops the hidden ORDER BY columns here.
pub fn take_columns(
    batch: &RecordBatch,
    indices: &UInt32Array,
    width: usize,
) -> Result<Vec<ArrayRef>, StorageError> {
    batch
        .columns()
        .iter()
        .take(width)
        .map(|column| take(column.as_ref(), indices, None))
        .collect::<Result<_, _>>()
        .map_err(|error| StorageError::Io(format!("sort take failed: {error}")))
}

/// `take` every column of `batch` through `indices`, keeping its schema.
pub fn take_batch(batch: &RecordBatch, indices: &UInt32Array) -> Result<RecordBatch, StorageError> {
    let columns = take_columns(batch, indices, batch.num_columns())?;
    RecordBatch::try_new(batch.schema(), columns)
        .map_err(|error| StorageError::Io(format!("sort rebuild failed: {error}")))
}

/// Make a float column sort the way PostgreSQL does.
///
/// Two divergences, both real and both silent:
///
/// - PostgreSQL treats `-0.0` and `0.0` as **equal** (`float8_cmp`), while
///   Arrow's total order ranks `-0.0` below `0.0`.
/// - PostgreSQL treats all NaNs as one value, greater than everything. Arrow's
///   total order also puts NaN last, but orders distinct NaN *bit patterns*
///   against each other, so two NaNs that PostgreSQL calls equal would get a
///   defined relative order — and a stable sort would report it.
///
/// Mapping `-0.0` to `0.0` and every NaN to one canonical NaN makes Arrow's
/// total order coincide with PostgreSQL's exactly. Only the sort *key* is
/// rewritten; the values that get taken come from the untouched columns.
fn canonicalize(array: &ArrayRef) -> ArrayRef {
    if let Some(values) = array.as_any().downcast_ref::<Float64Array>() {
        let fixed: Float64Array = values.unary(|v: f64| {
            if v.is_nan() {
                f64::NAN
            } else if v == 0.0 {
                0.0
            } else {
                v
            }
        });
        return Arc::new(fixed);
    }
    if let Some(values) = array.as_any().downcast_ref::<Float32Array>() {
        let fixed: Float32Array = values.unary(|v: f32| {
            if v.is_nan() {
                f32::NAN
            } else if v == 0.0 {
                0.0
            } else {
                v
            }
        });
        return Arc::new(fixed);
    }
    Arc::clone(array)
}

#[cfg(test)]
mod tests {
    use crabgresql_types::Value;

    use super::*;

    fn schema(columns: Vec<Column>, sort_key: Vec<IndexKey>) -> TableSchema {
        let mut schema = TableSchema::new("t", columns);
        schema.sort_key = sort_key;
        schema
    }

    fn key(column: usize) -> IndexKey {
        IndexKey {
            column,
            descending: false,
            nulls_first: false,
        }
    }

    fn batch(schema: &TableSchema, rows: &[Vec<Value>]) -> RecordBatch {
        crate::arrow::build_batch(schema, rows).expect("batch")
    }

    fn order(indices: &UInt32Array) -> Vec<u32> {
        indices.values().to_vec()
    }

    #[test]
    fn every_arrow_ordered_type_is_sortable_and_the_rest_is_not() {
        for ty in [
            PgType::Bool,
            // `UInt8` in the Arrow mapping, chosen for exactly this reason.
            PgType::Char,
            PgType::Int2,
            PgType::Int4,
            PgType::Int8,
            PgType::Float4,
            PgType::Float8,
            PgType::Date,
            PgType::Time,
            PgType::Timestamp,
            PgType::TimestampTz,
            PgType::Bytea,
            PgType::Uuid,
        ] {
            assert!(sortable(ty, collation::DEFAULT_COLLATION_OID), "{ty:?}");
        }
        // Stored as `Utf8`, as a `Struct`, or blank-padded — none of the three
        // orders like PostgreSQL does.
        for ty in [
            PgType::Numeric,
            PgType::TimeTz,
            PgType::Interval,
            PgType::Bpchar,
        ] {
            assert!(!sortable(ty, collation::DEFAULT_COLLATION_OID), "{ty:?}");
        }
    }

    #[test]
    fn a_collated_text_column_is_sortable_only_under_byte_order() {
        assert!(sortable(PgType::Text, collation::DEFAULT_COLLATION_OID));
        assert!(sortable(PgType::Text, collation::C_COLLATION_OID));
        let icu = collation::lookup_by_name("unicode")
            .expect("ICU collation")
            .oid;
        assert!(!sortable(PgType::Text, icu));
    }

    #[test]
    fn a_layout_is_sortable_only_when_every_key_column_is() {
        let sortable_schema = schema(
            vec![
                Column::new("a", PgType::Int4),
                Column::new("b", PgType::Numeric),
            ],
            vec![key(0)],
        );
        assert!(sortable_layout(&sortable_schema));
        assert!(unsortable_column(&sortable_schema.columns, &sortable_schema.sort_key).is_none());

        let numeric_key = schema(
            vec![
                Column::new("a", PgType::Int4),
                Column::new("b", PgType::Numeric),
            ],
            vec![key(0), key(1)],
        );
        assert!(!sortable_layout(&numeric_key));
        // The offending column is named, so the DDL gate reports the same one
        // the write path would have refused to order.
        assert_eq!(
            unsortable_column(&numeric_key.columns, &numeric_key.sort_key)
                .map(|column| column.name.as_str()),
            Some("b")
        );

        // An out-of-range key is as unusable as an unsortable type, and takes
        // the same insertion-order path rather than panicking on a write — but
        // it names no column, because reporting it is the range check's job.
        let out_of_range = schema(vec![Column::new("a", PgType::Int4)], vec![key(7)]);
        assert!(!sortable_layout(&out_of_range));
        assert!(unsortable_column(&out_of_range.columns, &out_of_range.sort_key).is_none());
    }

    #[test]
    fn a_permutation_names_every_input_row_exactly_once() {
        let table = schema(vec![Column::new("a", PgType::Int4)], vec![key(0)]);
        let rows: Vec<Vec<Value>> = [5, 1, 4, 1, 3]
            .into_iter()
            .map(|v| vec![Value::Int4(v)])
            .collect();
        let indices = sort_permutation(&batch(&table, &rows), &table.sort_key).expect("sort");
        let mut seen = order(&indices);
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn rows_with_equal_keys_keep_their_input_order() {
        let table = schema(vec![Column::new("a", PgType::Int4)], vec![key(0)]);
        let rows: Vec<Vec<Value>> = [2, 1, 2, 1, 2]
            .into_iter()
            .map(|v| vec![Value::Int4(v)])
            .collect();
        let indices = sort_permutation(&batch(&table, &rows), &table.sort_key).expect("sort");
        assert_eq!(order(&indices), vec![1, 3, 0, 2, 4]);
    }

    #[test]
    fn signed_zero_and_nan_order_as_postgresql_does() {
        let table = schema(vec![Column::new("a", PgType::Float8)], vec![key(0)]);
        // `-0.0` must tie with `0.0` (so the tiebreak decides), and the two NaN
        // bit patterns must tie with each other and sort last.
        let nan = f64::from_bits(f64::NAN.to_bits() | 1);
        let rows: Vec<Vec<Value>> = [nan, 0.0, f64::NAN, -0.0, -1.0]
            .into_iter()
            .map(|v| vec![Value::Float8(v)])
            .collect();
        let indices = sort_permutation(&batch(&table, &rows), &table.sort_key).expect("sort");
        assert_eq!(order(&indices), vec![4, 1, 3, 0, 2]);
    }

    #[test]
    fn a_descending_nulls_first_key_is_honored() {
        let table = schema(
            vec![Column::new("a", PgType::Int4)],
            vec![IndexKey {
                column: 0,
                descending: true,
                nulls_first: true,
            }],
        );
        let rows = vec![
            vec![Value::Int4(1)],
            vec![Value::Null],
            vec![Value::Int4(3)],
        ];
        let indices = sort_permutation(&batch(&table, &rows), &table.sort_key).expect("sort");
        assert_eq!(order(&indices), vec![1, 2, 0]);
    }

    #[test]
    fn a_key_naming_a_missing_column_is_an_error() {
        let table = schema(vec![Column::new("a", PgType::Int4)], vec![key(0)]);
        let rows = vec![vec![Value::Int4(1)]];
        assert!(matches!(
            sort_permutation(&batch(&table, &rows), &[key(4)]),
            Err(StorageError::CorruptData(_))
        ));
    }

    #[test]
    fn taking_a_batch_preserves_its_schema_and_reorders_every_column() {
        let table = schema(
            vec![
                Column::new("a", PgType::Int4),
                Column::new("b", PgType::Text),
            ],
            vec![key(0)],
        );
        let rows = vec![
            vec![Value::Int4(2), Value::Text("two".into())],
            vec![Value::Int4(1), Value::Text("one".into())],
        ];
        let source = batch(&table, &rows);
        let indices = sort_permutation(&source, &table.sort_key).expect("sort");
        let sorted = take_batch(&source, &indices).expect("take");
        assert_eq!(sorted.schema(), source.schema());
        assert_eq!(
            crate::arrow::decode_row(&table, &[0, 1], &sorted, 0).expect("decode"),
            rows[1]
        );
        assert_eq!(
            crate::arrow::decode_row(&table, &[0, 1], &sorted, 1).expect("decode"),
            rows[0]
        );
    }

    #[test]
    fn taking_columns_narrows_to_the_requested_width() {
        let table = schema(
            vec![
                Column::new("a", PgType::Int4),
                Column::new("b", PgType::Int4),
            ],
            vec![key(1)],
        );
        let rows = vec![
            vec![Value::Int4(1), Value::Int4(9)],
            vec![Value::Int4(2), Value::Int4(8)],
        ];
        let source = batch(&table, &rows);
        let indices = sort_permutation(&source, &table.sort_key).expect("sort");
        let columns = take_columns(&source, &indices, 1).expect("take");
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].len(), 2);
    }
}
