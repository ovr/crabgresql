//! What a fragment's schema may contain, and how a relation's schema is
//! projected onto Parquet metadata.

use crabgresql_storage_api::arrow::{NUMERIC_MAX_PRECISION, arrow_schema, numeric_decimal};
use crabgresql_storage_api::{StorageError, TableSchema};
use crabgresql_types::PgType;
use parquet::arrow::ArrowSchemaConverter;
use parquet::file::metadata::SortingColumn;

use crate::error::{corrupt, io_error};

pub(crate) fn schema_identity(schema: &TableSchema) -> String {
    schema
        .columns
        .iter()
        .map(|column| format!("{}:{}:{}", column.name, column.ty.oid(), column.typmod))
        .collect::<Vec<_>>()
        .join("|")
}

/// The layout sort key as Parquet row-group metadata — the *only* record a
/// fragment keeps of how its rows are ordered.
///
/// Written only when the sort actually ran, so its presence means "this file is
/// clustered" rather than "this table declares a key". The claim is made in
/// Parquet's own vocabulary rather than in a private footer key because that is
/// the field an outside reader already looks at.
///
/// TODO: read this metadata back, to skip row groups whose key range cannot
/// match and to drive a compaction pass; nothing in this engine consumes it.
///
/// `SortingColumn::column_idx` is a **leaf** index, not the ordinal of a
/// top-level field. [`crabgresql_storage_api::arrow::arrow_type`] maps `timetz`
/// and `interval` to `Struct`s, so either of those ahead of a key column shifts
/// the leaf numbering away from the column position an `IndexKey` carries —
/// which would publish confidently wrong metadata. Ask the converted schema
/// instead, and take a key's first leaf.
///
/// **Float keys order by PostgreSQL's rules, which are Parquet's and not
/// Arrow's.** The sort canonicalizes `-0.0` to `0.0` and every NaN payload to
/// one NaN before comparing, so a fragment can hold `+0.0` before `-0.0`, or
/// two NaN bit patterns in input order. That is non-decreasing under the IEEE
/// comparison Parquet defines for `FLOAT`/`DOUBLE` (which calls the zeros equal
/// and leaves NaN undefined), so the declaration is honest — but a reader that
/// merges or binary-searches under Arrow's *total* order must canonicalize the
/// same way before trusting it.
pub(crate) fn sorting_columns(schema: &TableSchema) -> Result<Vec<SortingColumn>, StorageError> {
    let descriptor = ArrowSchemaConverter::new()
        .convert(&arrow_schema(schema))
        .map_err(|error| io_error("describe Parquet sort key", error))?;
    schema
        .sort_key
        .iter()
        .map(|key| {
            (0..descriptor.num_columns())
                .find(|leaf| descriptor.get_column_root_idx(*leaf) == key.column)
                .map(|leaf| SortingColumn {
                    column_idx: leaf as i32,
                    descending: key.descending,
                    nulls_first: key.nulls_first,
                })
                .ok_or_else(|| {
                    corrupt(format!(
                        "sort key names column {} of a {}-column relation",
                        key.column,
                        schema.columns.len()
                    ))
                })
        })
        .collect()
}

/// Whether a fragment can represent this type. The whitelist is the shared
/// columnar one — a Parquet file is just the durable form of a batch, so a type
/// this engine accepts is exactly a type [`crabgresql_storage_api::arrow`] can
/// encode.
pub fn supports_type(ty: PgType) -> bool {
    crabgresql_storage_api::arrow::supports_type(ty)
}

/// Reject a schema this format cannot represent.
///
/// The error names `schema.access_method` rather than a hard-coded "parquet"
/// because the buffer table shares this whitelist: a row a buffer accepts must
/// always be convertible to a fragment, or a flush would fail long after the
/// `INSERT` that should have been rejected. One whitelist keeps that true, and
/// naming the relation's own method keeps the message honest.
pub fn validate_schema(schema: &TableSchema) -> Result<(), StorageError> {
    if let Some(column) = schema
        .columns
        .iter()
        .find(|column| !supports_type(column.ty))
    {
        return Err(StorageError::UnsupportedType(format!(
            "data type {} is not supported by table access method \"{}\"",
            column.ty.name(),
            schema.access_method.as_str(),
        )));
    }
    // `numeric` is the one type whose *modifier* can put it outside the format,
    // in two ways. Both belong at DDL: unlike a value that does not fit, this is
    // a property of the declaration, and a relation that passed it would accept
    // rows all day and fail in the *flush*, with no statement to report to.
    for column in &schema.columns {
        if column.ty != PgType::Numeric || column.typmod < 0 {
            continue;
        }
        let method = schema.access_method.as_str();
        let (precision, scale) = numeric_decimal(column.typmod);
        // The widest Arrow decimal is 256 bits, which stops at 76 digits, while
        // PostgreSQL's `numeric(p, s)` allows a thousand.
        if precision > NUMERIC_MAX_PRECISION {
            return Err(StorageError::UnsupportedType(format!(
                "numeric precision {precision} exceeds the maximum {NUMERIC_MAX_PRECISION} \
                 supported by table access method \"{method}\"",
            )));
        }
        // Parquet's DECIMAL is defined only for `0 <= scale <= precision`, but
        // PostgreSQL's runs from -1000 to 1000 — `numeric(4,-2)` rounds to
        // hundreds. Arrow carries a negative scale as far as the batch, so
        // otherwise this surfaces only when the file is written.
        if scale < 0 || scale as u8 > precision {
            return Err(StorageError::UnsupportedType(format!(
                "numeric scale {scale} is outside 0..{precision}, which table access \
                 method \"{method}\" requires",
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crabgresql_storage_api::StorageError;
    use crabgresql_types::PgType;
    use crabgresql_types::numeric::Numeric;

    use crate::test_support::schema;

    /// A `numeric(p, s)` the format cannot represent is a property of the
    /// declaration, so it is DDL that says no — for both ways it can happen.
    /// The scale case is the one that bites: only the Parquet writer refuses a
    /// negative scale, and that refusal lands in a flush.
    #[test]
    fn a_typmod_the_format_cannot_represent_is_rejected_by_ddl() {
        let refused = |precision, scale| {
            let mut schema = schema("t", &[PgType::Numeric]);
            schema.columns[0].typmod = Numeric::pack_typmod(precision, scale);
            matches!(
                super::validate_schema(&schema),
                Err(StorageError::UnsupportedType(_))
            )
        };
        assert!(refused(80, 2), "precision past the widest decimal");
        assert!(
            refused(4, -2),
            "negative scale, which Parquet has no DECIMAL for"
        );
        assert!(refused(2, 5), "scale past the precision");

        assert!(!refused(76, 38), "the widest decimal itself");
        assert!(!refused(9, 0), "scale 0 is the boundary, not an exclusion");
        assert!(!refused(9, 9), "scale == precision is legal");

        // No typmod at all is the common case and is stored at the default
        // precision and scale, not refused.
        let bare = schema("bare", &[PgType::Numeric]);
        assert!(super::validate_schema(&bare).is_ok());
    }
}
