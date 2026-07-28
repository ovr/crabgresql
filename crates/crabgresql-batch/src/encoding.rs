//! How a PostgreSQL type is laid out inside a batch's Arrow array.
//!
//! A [`PgType`] does not determine an Arrow type, and an Arrow type does not
//! determine a [`PgType`]. `numeric`, `text`, `varchar`, `bpchar` and `name` all
//! arrive as `Utf8`; `interval` and `timetz` are both structs. Naming the
//! encoding separately is what lets kernel dispatch key on the *PostgreSQL* type
//! — which carries the semantics — while the array type stays a decode contract.
//!
//! The dispatch direction matters. `bpchar` equality ignores trailing blanks and
//! `text` equality does not, but both are `Utf8`; a kernel that switched on
//! `DataType` would silently apply `text` rules to a `char(n)` column.

use arrow_schema::{DataType, Field, Fields, TimeUnit};
use crabgresql_types::PgType;

/// How a column's values are physically laid out in its Arrow array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnEncoding {
    /// The PostgreSQL value *is* the Arrow value, with no reinterpretation.
    ///
    /// This includes `date` and `timestamp`, which a batch carries already
    /// rebased into the PostgreSQL epoch (see [`crate::epoch`]) — and therefore
    /// as plain `Int32`/`Int64`, not as `Date32`/`Timestamp`. Dropping the
    /// temporal Arrow type is deliberate: a rebased value is no longer a
    /// Unix-epoch `Date32`, and labelling it one would invite an Arrow temporal
    /// kernel to read it as a date 30 years off.
    Native,
    /// The value's bytes: `Utf8` for the string types, `Binary` for `bytea`,
    /// `FixedSizeBinary(16)` for `uuid`.
    Bytes,
    /// `numeric` rendered as its output text and re-parsed on read — what a V1
    /// Parquet fragment stores.
    ///
    /// Nothing may be computed on this in place. Text order is not numeric order
    /// (`"9" > "10"`), and the display scale that survives the round trip is not
    /// part of the value: `1.0`, `1.00` and `1.000` are one value with three
    /// spellings, so byte equality splits a group PostgreSQL keeps whole.
    NumericText,
    /// `timetz` and `interval` as a struct with positional children. No kernel
    /// accepts this; such a column is read one value at a time or not at all.
    Struct,
}

/// The encoding a batch uses for `ty`, or `None` for a type no batch can carry.
///
/// Exhaustive on purpose: adding a [`PgType`] variant fails this build rather
/// than silently acquiring a default encoding. The existing `matches!`-shaped
/// whitelist in the Parquet engine and its `_ => DataType::Binary` fallback are
/// the shapes this deliberately does not copy — neither breaks when a type is
/// added, so a new type becomes an opaque blob instead of a compile error.
pub fn encoding_of(ty: PgType) -> Option<ColumnEncoding> {
    let encoding = match ty {
        PgType::Bool
        | PgType::Int2
        | PgType::Int4
        | PgType::Int8
        | PgType::Float4
        | PgType::Float8
        | PgType::Date
        | PgType::Time
        | PgType::Timestamp
        | PgType::TimestampTz => ColumnEncoding::Native,

        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => ColumnEncoding::Bytes,
        PgType::Bytea | PgType::Uuid => ColumnEncoding::Bytes,

        PgType::Numeric => ColumnEncoding::NumericText,
        PgType::TimeTz | PgType::Interval => ColumnEncoding::Struct,

        // Types with no batch representation. Each is listed rather than swept
        // into a `_` arm so that adding one later is a decision, not a default.
        PgType::Money
        | PgType::Oid
        | PgType::Reg(_)
        | PgType::Bit
        | PgType::Varbit
        | PgType::Inet
        | PgType::Cidr
        | PgType::Macaddr
        | PgType::Macaddr8
        | PgType::Point
        | PgType::Lseg
        | PgType::Json
        | PgType::Jsonb
        | PgType::Jsonpath
        | PgType::Tsvector
        | PgType::Tsquery
        | PgType::User(_)
        | PgType::Array(_) => return None,
    };
    Some(encoding)
}

/// The Arrow type a batch column holds for `ty`.
///
/// Note where this diverges from the Parquet engine's on-disk mapping: `date`
/// and `timestamp` are `Int32`/`Int64` here because a batch has already rebased
/// them out of the Arrow epoch. Everything else agrees, which is what keeps the
/// Parquet scan a metadata-only handoff for the other 100-odd columns.
pub fn batch_type_of(ty: PgType) -> Option<DataType> {
    let data_type = match ty {
        PgType::Bool => DataType::Boolean,
        PgType::Int2 => DataType::Int16,
        PgType::Int4 => DataType::Int32,
        PgType::Int8 => DataType::Int64,
        PgType::Float4 => DataType::Float32,
        PgType::Float8 => DataType::Float64,

        // PostgreSQL-domain integers, not Arrow temporal types. See
        // [`ColumnEncoding::Native`].
        PgType::Date => DataType::Int32,
        PgType::Time => DataType::Int64,
        PgType::Timestamp | PgType::TimestampTz => DataType::Int64,

        PgType::Numeric | PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => {
            DataType::Utf8
        }
        PgType::Bytea => DataType::Binary,
        PgType::Uuid => DataType::FixedSizeBinary(16),

        PgType::TimeTz => DataType::Struct(Fields::from(vec![
            Field::new("time_us", DataType::Int64, false),
            Field::new("offset_seconds", DataType::Int32, false),
        ])),
        PgType::Interval => DataType::Struct(Fields::from(vec![
            Field::new("months", DataType::Int32, false),
            Field::new("days", DataType::Int32, false),
            Field::new("micros", DataType::Int64, false),
        ])),

        PgType::Money
        | PgType::Oid
        | PgType::Reg(_)
        | PgType::Bit
        | PgType::Varbit
        | PgType::Inet
        | PgType::Cidr
        | PgType::Macaddr
        | PgType::Macaddr8
        | PgType::Point
        | PgType::Lseg
        | PgType::Json
        | PgType::Jsonb
        | PgType::Jsonpath
        | PgType::Tsvector
        | PgType::Tsquery
        | PgType::User(_)
        | PgType::Array(_) => return None,
    };
    Some(data_type)
}

/// The Arrow type a Parquet *fragment* stores for `ty`, which differs from
/// [`batch_type_of`] only in the temporal types' epoch.
///
/// Kept here beside its sibling so the two cannot drift: the whole reason a
/// batch rebases is that these two disagree, and a reader that assumed they
/// agreed would read dates 30 years off.
pub fn storage_type_of(ty: PgType) -> Option<DataType> {
    let data_type = match ty {
        PgType::Date => DataType::Date32,
        PgType::Time => DataType::Time64(TimeUnit::Microsecond),
        PgType::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
        PgType::TimestampTz => DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
        other => return batch_type_of(other),
    };
    Some(data_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_encodable_type_has_a_batch_type_and_vice_versa() {
        // The two functions must admit exactly the same set, or a column could
        // be declared encodable and then have no array to live in.
        for ty in all_types() {
            assert_eq!(
                encoding_of(ty).is_some(),
                batch_type_of(ty).is_some(),
                "{ty:?} is encodable xor representable"
            );
        }
    }

    #[test]
    fn temporal_types_are_integers_in_a_batch_and_temporal_on_disk() {
        assert_eq!(batch_type_of(PgType::Date), Some(DataType::Int32));
        assert_eq!(storage_type_of(PgType::Date), Some(DataType::Date32));
        assert_eq!(batch_type_of(PgType::Timestamp), Some(DataType::Int64));
        assert_eq!(
            storage_type_of(PgType::Timestamp),
            Some(DataType::Timestamp(TimeUnit::Microsecond, None))
        );
    }

    #[test]
    fn non_temporal_types_are_identical_in_both_domains() {
        for ty in [
            PgType::Bool,
            PgType::Int4,
            PgType::Float8,
            PgType::Text,
            PgType::Numeric,
            PgType::Bytea,
            PgType::Uuid,
        ] {
            assert_eq!(batch_type_of(ty), storage_type_of(ty), "{ty:?}");
        }
    }

    /// `numeric` and `text` share `Utf8`, and `bpchar` and `text` share both the
    /// Arrow type *and* the encoding — so the encoding alone can never decide
    /// semantics. This pins the reason kernels dispatch on `PgType`.
    #[test]
    fn arrow_type_does_not_determine_semantics() {
        assert_eq!(batch_type_of(PgType::Numeric), batch_type_of(PgType::Text));
        assert_eq!(batch_type_of(PgType::Bpchar), batch_type_of(PgType::Text));
        assert_eq!(
            encoding_of(PgType::Bpchar),
            encoding_of(PgType::Text),
            "bpchar and text are indistinguishable below the PgType"
        );
        assert_ne!(encoding_of(PgType::Numeric), encoding_of(PgType::Text));
    }

    /// Fails to compile when a `PgType` variant is added, which is the point:
    /// the list below feeds every exhaustiveness test in this crate.
    fn all_types() -> Vec<PgType> {
        // The match is "needless" by design — it exists only so that adding a
        // `PgType` variant breaks this build and forces the list below to be
        // updated along with every whitelist that reads it.
        #[expect(clippy::needless_match, reason = "an exhaustiveness canary")]
        fn _exhaustive(ty: PgType) -> PgType {
            match ty {
                PgType::Bool => PgType::Bool,
                PgType::Int2 => PgType::Int2,
                PgType::Int4 => PgType::Int4,
                PgType::Int8 => PgType::Int8,
                PgType::Float4 => PgType::Float4,
                PgType::Float8 => PgType::Float8,
                PgType::Numeric => PgType::Numeric,
                PgType::Money => PgType::Money,
                PgType::Text => PgType::Text,
                PgType::Varchar => PgType::Varchar,
                PgType::Bpchar => PgType::Bpchar,
                PgType::Name => PgType::Name,
                PgType::Oid => PgType::Oid,
                PgType::Bytea => PgType::Bytea,
                PgType::Bit => PgType::Bit,
                PgType::Varbit => PgType::Varbit,
                PgType::Date => PgType::Date,
                PgType::Time => PgType::Time,
                PgType::TimeTz => PgType::TimeTz,
                PgType::Timestamp => PgType::Timestamp,
                PgType::TimestampTz => PgType::TimestampTz,
                PgType::Interval => PgType::Interval,
                PgType::Uuid => PgType::Uuid,
                PgType::Inet => PgType::Inet,
                PgType::Cidr => PgType::Cidr,
                PgType::Macaddr => PgType::Macaddr,
                PgType::Macaddr8 => PgType::Macaddr8,
                PgType::Point => PgType::Point,
                PgType::Lseg => PgType::Lseg,
                PgType::Json => PgType::Json,
                PgType::Jsonb => PgType::Jsonb,
                PgType::Jsonpath => PgType::Jsonpath,
                PgType::Reg(kind) => PgType::Reg(kind),
                PgType::Tsvector => PgType::Tsvector,
                PgType::Tsquery => PgType::Tsquery,
                PgType::User(oid) => PgType::User(oid),
                PgType::Array(oid) => PgType::Array(oid),
            }
        }
        vec![
            PgType::Bool,
            PgType::Int2,
            PgType::Int4,
            PgType::Int8,
            PgType::Float4,
            PgType::Float8,
            PgType::Numeric,
            PgType::Money,
            PgType::Text,
            PgType::Varchar,
            PgType::Bpchar,
            PgType::Name,
            PgType::Oid,
            PgType::Bytea,
            PgType::Bit,
            PgType::Varbit,
            PgType::Date,
            PgType::Time,
            PgType::TimeTz,
            PgType::Timestamp,
            PgType::TimestampTz,
            PgType::Interval,
            PgType::Uuid,
            PgType::Inet,
            PgType::Cidr,
            PgType::Macaddr,
            PgType::Macaddr8,
            PgType::Point,
            PgType::Lseg,
            PgType::Json,
            PgType::Jsonb,
            PgType::Jsonpath,
            PgType::Reg(crabgresql_types::RegKind::Class),
            PgType::Tsvector,
            PgType::Tsquery,
            PgType::User(0),
            PgType::Array(0),
        ]
    }
}
