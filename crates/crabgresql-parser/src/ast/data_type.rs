// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

#[cfg(not(feature = "std"))]
use alloc::{boxed::Box, format, string::String, vec::Vec};
use core::fmt;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

#[cfg(feature = "visitor")]
use sqlparser_derive::{Visit, VisitMut};

use crate::ast::{display_comma_separated, ObjectName};

use super::ColumnDef;

/// SQL data types
#[derive(Debug, Clone, PartialEq, PartialOrd, Eq, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "visitor", derive(Visit, VisitMut))]
pub enum DataType {
    /// Table type in [PostgreSQL], e.g. CREATE FUNCTION RETURNS TABLE(...).
    ///
    /// [PostgreSQL]: https://www.postgresql.org/docs/15/sql-createfunction.html
    /// [MsSQL]: https://learn.microsoft.com/en-us/sql/t-sql/statements/create-function-transact-sql?view=sql-server-ver16#c-create-a-multi-statement-table-valued-function
    Table(Option<Vec<ColumnDef>>),
    /// Fixed-length character type, e.g. CHARACTER(10).
    Character(Option<CharacterLength>),
    /// Fixed-length char type, e.g. CHAR(10).
    Char(Option<CharacterLength>),
    /// Character varying type, e.g. CHARACTER VARYING(10).
    CharacterVarying(Option<CharacterLength>),
    /// Char varying type, e.g. CHAR VARYING(10).
    CharVarying(Option<CharacterLength>),
    /// Variable-length character type, e.g. VARCHAR(10).
    Varchar(Option<CharacterLength>),
    /// Uuid type.
    Uuid,
    /// Large character object with optional length,
    /// e.g. CHARACTER LARGE OBJECT, CHARACTER LARGE OBJECT(1000), [SQL Standard].
    ///
    /// [SQL Standard]: https://jakewheat.github.io/sql-overview/sql-2016-foundation-grammar.html#character-large-object-type
    CharacterLargeObject(Option<u64>),
    /// Large character object with optional length,
    /// e.g. CHAR LARGE OBJECT, CHAR LARGE OBJECT(1000), [SQL Standard].
    ///
    /// [SQL Standard]: https://jakewheat.github.io/sql-overview/sql-2016-foundation-grammar.html#character-large-object-type
    CharLargeObject(Option<u64>),
    /// Large character object with optional length,
    /// e.g. CLOB, CLOB(1000), [SQL Standard].
    ///
    /// [SQL Standard]: https://jakewheat.github.io/sql-overview/sql-2016-foundation-grammar.html#character-large-object-type
    /// [Oracle]: https://docs.oracle.com/javadb/10.10.1.2/ref/rrefclob.html
    Clob(Option<u64>),
    /// Fixed-length binary type with optional length,
    /// see [SQL Standard], [MS SQL Server].
    ///
    /// [SQL Standard]: https://jakewheat.github.io/sql-overview/sql-2016-foundation-grammar.html#binary-string-type
    /// [MS SQL Server]: https://learn.microsoft.com/pt-br/sql/t-sql/data-types/binary-and-varbinary-transact-sql?view=sql-server-ver16
    Binary(Option<u64>),
    /// Variable-length binary with optional length type,
    /// see [SQL Standard], [MS SQL Server].
    ///
    /// [SQL Standard]: https://jakewheat.github.io/sql-overview/sql-2016-foundation-grammar.html#binary-string-type
    /// [MS SQL Server]: https://learn.microsoft.com/pt-br/sql/t-sql/data-types/binary-and-varbinary-transact-sql?view=sql-server-ver16
    Varbinary(Option<BinaryLength>),
    /// Large binary object with optional length,
    /// see [SQL Standard], [Oracle].
    ///
    /// [SQL Standard]: https://jakewheat.github.io/sql-overview/sql-2016-foundation-grammar.html#binary-large-object-string-type
    /// [Oracle]: https://docs.oracle.com/javadb/10.8.3.0/ref/rrefblob.html
    Blob(Option<u64>),
    /// Numeric type with optional precision and scale, e.g. NUMERIC(10,2), [SQL Standard][1].
    ///
    /// [1]: https://jakewheat.github.io/sql-overview/sql-2016-foundation-grammar.html#exact-numeric-type
    Numeric(ExactNumberInfo),
    /// Decimal type with optional precision and scale, e.g. DECIMAL(10,2), [SQL Standard][1].
    ///
    /// [1]: https://jakewheat.github.io/sql-overview/sql-2016-foundation-grammar.html#exact-numeric-type
    Decimal(ExactNumberInfo),
    /// Dec type with optional precision and scale, e.g. DEC(10,2), [SQL Standard][1].
    ///
    /// [1]: https://jakewheat.github.io/sql-overview/sql-2016-foundation-grammar.html#exact-numeric-type
    Dec(ExactNumberInfo),
    /// Floating point with optional precision and scale, e.g. FLOAT, FLOAT(8), or FLOAT(8,2).
    Float(ExactNumberInfo),
    /// Int2 is an alias for SmallInt in [PostgreSQL].
    /// Note: Int2 means 2 bytes in PostgreSQL (not 2 bits).
    /// Int2 with optional display width, e.g. INT2 or INT2(5).
    ///
    /// [PostgreSQL]: https://www.postgresql.org/docs/current/datatype.html
    Int2(Option<u64>),
    /// Small integer with optional display width, e.g. SMALLINT or SMALLINT(5).
    SmallInt(Option<u64>),
    /// Int with optional display width, e.g. INT or INT(11).
    Int(Option<u64>),
    /// Int4 is an alias for Integer in [PostgreSQL].
    /// Note: Int4 means 4 bytes in PostgreSQL (not 4 bits).
    /// Int4 with optional display width, e.g. Int4 or Int4(11).
    ///
    /// [PostgreSQL]: https://www.postgresql.org/docs/current/datatype.html
    Int4(Option<u64>),
    /// Int8 is an alias for BigInt in [PostgreSQL] and Integer type in [ClickHouse].
    /// Int8 with optional display width, e.g. INT8 or INT8(11).
    /// Note: Int8 means 8 bytes in [PostgreSQL], but 8 bits in [ClickHouse].
    ///
    /// [PostgreSQL]: https://www.postgresql.org/docs/current/datatype.html
    /// [ClickHouse]: https://clickhouse.com/docs/en/sql-reference/data-types/int-uint
    Int8(Option<u64>),
    /// Integer with optional display width, e.g. INTEGER or INTEGER(11).
    Integer(Option<u64>),
    /// Big integer with optional display width, e.g. BIGINT or BIGINT(20).
    BigInt(Option<u64>),
    /// Float4 is an alias for Real in [PostgreSQL].
    ///
    /// [PostgreSQL]: https://www.postgresql.org/docs/current/datatype.html
    Float4,
    /// Floating point, e.g. REAL.
    Real,
    /// Float8 is an alias for Double in [PostgreSQL].
    ///
    /// [PostgreSQL]: https://www.postgresql.org/docs/current/datatype.html
    Float8,
    /// Double
    Double(ExactNumberInfo),
    /// Double Precision, see [SQL Standard], [PostgreSQL].
    ///
    /// [SQL Standard]: https://jakewheat.github.io/sql-overview/sql-2016-foundation-grammar.html#approximate-numeric-type
    /// [PostgreSQL]: https://www.postgresql.org/docs/current/datatype-numeric.html
    DoublePrecision,
    /// Bool is an alias for Boolean, see [PostgreSQL].
    ///
    /// [PostgreSQL]: https://www.postgresql.org/docs/current/datatype.html
    Bool,
    /// Boolean type.
    Boolean,
    /// Date type.
    Date,
    /// Time with optional time precision and time zone information, see [SQL Standard][1].
    ///
    /// [1]: https://jakewheat.github.io/sql-overview/sql-2016-foundation-grammar.html#datetime-type
    Time(Option<u64>, TimezoneInfo),
    /// Timestamp with optional time precision and time zone information, see [SQL Standard][1].
    ///
    /// [1]: https://jakewheat.github.io/sql-overview/sql-2016-foundation-grammar.html#datetime-type
    Timestamp(Option<u64>, TimezoneInfo),
    /// Interval type.
    Interval {
        /// [PostgreSQL] fields specification like `INTERVAL YEAR TO MONTH`.
        ///
        /// [PostgreSQL]: https://www.postgresql.org/docs/17/datatype-datetime.html
        fields: Option<IntervalFields>,
        /// [PostgreSQL] subsecond precision like `INTERVAL HOUR TO SECOND(3)`
        ///
        /// [PostgreSQL]: https://www.postgresql.org/docs/17/datatype-datetime.html
        precision: Option<u64>,
    },
    /// JSON type.
    JSON,
    /// Binary JSON type.
    JSONB,
    /// Regclass used in [PostgreSQL] serial.
    ///
    /// [PostgreSQL]: https://www.postgresql.org/docs/current/datatype.html
    Regclass,
    /// Text type.
    Text,
    /// Bytea type, see [PostgreSQL].
    ///
    /// [PostgreSQL]: https://www.postgresql.org/docs/current/datatype-bit.html
    Bytea,
    /// Bit string, see [PostgreSQL], [MySQL], or [MSSQL].
    ///
    /// [PostgreSQL]: https://www.postgresql.org/docs/current/datatype-bit.html
    /// [MySQL]: https://dev.mysql.com/doc/refman/9.1/en/bit-type.html
    /// [MSSQL]: https://learn.microsoft.com/en-us/sql/t-sql/data-types/bit-transact-sql?view=sql-server-ver16
    Bit(Option<u64>),
    /// `BIT VARYING(n)`: Variable-length bit string, see [PostgreSQL].
    ///
    /// [PostgreSQL]: https://www.postgresql.org/docs/current/datatype-bit.html
    BitVarying(Option<u64>),
    /// `VARBIT(n)`: Variable-length bit string. [PostgreSQL] alias for `BIT VARYING`.
    ///
    /// [PostgreSQL]: https://www.postgresql.org/docs/current/datatype.html
    VarBit(Option<u64>),
    /// Custom types.
    Custom(ObjectName, Vec<String>),
    /// Arrays.
    Array(ArrayElemTypeDef),
    /// No type specified, from statements such as `CREATE TABLE t1 (a)`.
    Unspecified,
    /// Trigger data type, returned by functions associated with triggers, see [PostgreSQL].
    ///
    /// [PostgreSQL]: https://www.postgresql.org/docs/current/plpgsql-trigger.html
    Trigger,
    /// Geometric type, see [PostgreSQL].
    ///
    /// [PostgreSQL]: https://www.postgresql.org/docs/9.5/functions-geometry.html
    GeometricType(GeometricTypeKind),
    /// PostgreSQL text search vectors, see [PostgreSQL].
    ///
    /// [PostgreSQL]: https://www.postgresql.org/docs/17/datatype-textsearch.html
    TsVector,
    /// PostgreSQL text search query, see [PostgreSQL].
    ///
    /// [PostgreSQL]: https://www.postgresql.org/docs/17/datatype-textsearch.html
    TsQuery,
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            DataType::Character(size) => format_character_string_type(f, "CHARACTER", size),
            DataType::Char(size) => format_character_string_type(f, "CHAR", size),
            DataType::CharacterVarying(size) => {
                format_character_string_type(f, "CHARACTER VARYING", size)
            }
            DataType::CharVarying(size) => format_character_string_type(f, "CHAR VARYING", size),
            DataType::Varchar(size) => format_character_string_type(f, "VARCHAR", size),
            DataType::Uuid => write!(f, "UUID"),
            DataType::CharacterLargeObject(size) => {
                format_type_with_optional_length(f, "CHARACTER LARGE OBJECT", size, false)
            }
            DataType::CharLargeObject(size) => {
                format_type_with_optional_length(f, "CHAR LARGE OBJECT", size, false)
            }
            DataType::Clob(size) => format_type_with_optional_length(f, "CLOB", size, false),
            DataType::Binary(size) => format_type_with_optional_length(f, "BINARY", size, false),
            DataType::Varbinary(size) => format_varbinary_type(f, "VARBINARY", size),
            DataType::Blob(size) => format_type_with_optional_length(f, "BLOB", size, false),
            DataType::Numeric(info) => {
                write!(f, "NUMERIC{info}")
            }
            DataType::Decimal(info) => {
                write!(f, "DECIMAL{info}")
            }
            DataType::Dec(info) => {
                write!(f, "DEC{info}")
            }
            DataType::Float(info) => write!(f, "FLOAT{info}"),
            DataType::Int2(zerofill) => {
                format_type_with_optional_length(f, "INT2", zerofill, false)
            }
            DataType::SmallInt(zerofill) => {
                format_type_with_optional_length(f, "SMALLINT", zerofill, false)
            }
            DataType::Int(zerofill) => format_type_with_optional_length(f, "INT", zerofill, false),
            DataType::Int4(zerofill) => {
                format_type_with_optional_length(f, "INT4", zerofill, false)
            }
            DataType::Int8(zerofill) => {
                format_type_with_optional_length(f, "INT8", zerofill, false)
            }
            DataType::Integer(zerofill) => {
                format_type_with_optional_length(f, "INTEGER", zerofill, false)
            }
            DataType::BigInt(zerofill) => {
                format_type_with_optional_length(f, "BIGINT", zerofill, false)
            }
            DataType::Real => write!(f, "REAL"),
            DataType::Float4 => write!(f, "FLOAT4"),
            DataType::Double(info) => write!(f, "DOUBLE{info}"),
            DataType::Float8 => write!(f, "FLOAT8"),
            DataType::DoublePrecision => write!(f, "DOUBLE PRECISION"),
            DataType::Bool => write!(f, "BOOL"),
            DataType::Boolean => write!(f, "BOOLEAN"),
            DataType::Date => write!(f, "DATE"),
            DataType::Time(precision, timezone_info) => {
                format_datetime_precision_and_tz(f, "TIME", precision, timezone_info)
            }
            DataType::Timestamp(precision, timezone_info) => {
                format_datetime_precision_and_tz(f, "TIMESTAMP", precision, timezone_info)
            }
            DataType::Interval { fields, precision } => {
                write!(f, "INTERVAL")?;
                if let Some(fields) = fields {
                    write!(f, " {fields}")?;
                }
                if let Some(precision) = precision {
                    write!(f, "({precision})")?;
                }
                Ok(())
            }
            DataType::JSON => write!(f, "JSON"),
            DataType::JSONB => write!(f, "JSONB"),
            DataType::Regclass => write!(f, "REGCLASS"),
            DataType::Text => write!(f, "TEXT"),
            DataType::Bytea => write!(f, "BYTEA"),
            DataType::Bit(size) => format_type_with_optional_length(f, "BIT", size, false),
            DataType::BitVarying(size) => {
                format_type_with_optional_length(f, "BIT VARYING", size, false)
            }
            DataType::VarBit(size) => format_type_with_optional_length(f, "VARBIT", size, false),
            DataType::Array(ty) => match ty {
                ArrayElemTypeDef::None => write!(f, "ARRAY"),
                ArrayElemTypeDef::SquareBracket(t, None) => write!(f, "{t}[]"),
                ArrayElemTypeDef::SquareBracket(t, Some(size)) => write!(f, "{t}[{size}]"),
                ArrayElemTypeDef::AngleBracket(t) => write!(f, "ARRAY<{t}>"),
                ArrayElemTypeDef::Parenthesis(t) => write!(f, "Array({t})"),
            },
            DataType::Custom(ty, modifiers) => {
                if modifiers.is_empty() {
                    write!(f, "{ty}")
                } else {
                    write!(f, "{}({})", ty, modifiers.join(", "))
                }
            }
            // ClickHouse
            DataType::Unspecified => Ok(()),
            DataType::Trigger => write!(f, "TRIGGER"),
            DataType::Table(fields) => match fields {
                Some(fields) => {
                    write!(f, "TABLE({})", display_comma_separated(fields))
                }
                None => {
                    write!(f, "TABLE")
                }
            },
            DataType::GeometricType(kind) => write!(f, "{kind}"),
            DataType::TsVector => write!(f, "TSVECTOR"),
            DataType::TsQuery => write!(f, "TSQUERY"),
        }
    }
}

fn format_type_with_optional_length(
    f: &mut fmt::Formatter,
    sql_type: &'static str,
    len: &Option<u64>,
    unsigned: bool,
) -> fmt::Result {
    write!(f, "{sql_type}")?;
    if let Some(len) = len {
        write!(f, "({len})")?;
    }
    if unsigned {
        write!(f, " UNSIGNED")?;
    }
    Ok(())
}

fn format_character_string_type(
    f: &mut fmt::Formatter,
    sql_type: &str,
    size: &Option<CharacterLength>,
) -> fmt::Result {
    write!(f, "{sql_type}")?;
    if let Some(size) = size {
        write!(f, "({size})")?;
    }
    Ok(())
}

fn format_varbinary_type(
    f: &mut fmt::Formatter,
    sql_type: &str,
    size: &Option<BinaryLength>,
) -> fmt::Result {
    write!(f, "{sql_type}")?;
    if let Some(size) = size {
        write!(f, "({size})")?;
    }
    Ok(())
}

fn format_datetime_precision_and_tz(
    f: &mut fmt::Formatter,
    sql_type: &'static str,
    len: &Option<u64>,
    time_zone: &TimezoneInfo,
) -> fmt::Result {
    write!(f, "{sql_type}")?;
    let len_fmt = len.as_ref().map(|l| format!("({l})")).unwrap_or_default();

    match time_zone {
        TimezoneInfo::Tz => {
            write!(f, "{time_zone}{len_fmt}")?;
        }
        _ => {
            write!(f, "{len_fmt}{time_zone}")?;
        }
    }

    Ok(())
}



/// Timestamp and Time data types information about TimeZone formatting.
///
/// This is more related to a display information than real differences between each variant. To
/// guarantee compatibility with the input query we must maintain its exact information.
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Eq, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "visitor", derive(Visit, VisitMut))]
pub enum TimezoneInfo {
    /// No information about time zone, e.g. TIMESTAMP
    None,
    /// Temporal type 'WITH TIME ZONE', e.g. TIMESTAMP WITH TIME ZONE, [SQL Standard], [Oracle]
    ///
    /// [SQL Standard]: https://jakewheat.github.io/sql-overview/sql-2016-foundation-grammar.html#datetime-type
    /// [Oracle]: https://docs.oracle.com/en/database/oracle/oracle-database/12.2/nlspg/datetime-data-types-and-time-zone-support.html#GUID-3F1C388E-C651-43D5-ADBC-1A49E5C2CA05
    WithTimeZone,
    /// Temporal type 'WITHOUT TIME ZONE', e.g. TIME WITHOUT TIME ZONE, [SQL Standard], [Postgresql]
    ///
    /// [SQL Standard]: https://jakewheat.github.io/sql-overview/sql-2016-foundation-grammar.html#datetime-type
    /// [Postgresql]: https://www.postgresql.org/docs/current/datatype-datetime.html
    WithoutTimeZone,
    /// Postgresql specific `WITH TIME ZONE` formatting, for both TIME and TIMESTAMP, e.g. TIMETZ, [Postgresql]
    ///
    /// [Postgresql]: https://www.postgresql.org/docs/current/datatype-datetime.html
    Tz,
}

impl fmt::Display for TimezoneInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TimezoneInfo::None => {
                write!(f, "")
            }
            TimezoneInfo::WithTimeZone => {
                write!(f, " WITH TIME ZONE")
            }
            TimezoneInfo::WithoutTimeZone => {
                write!(f, " WITHOUT TIME ZONE")
            }
            TimezoneInfo::Tz => {
                // TZ is the only one that is displayed BEFORE the precision, so the datatype display
                // must be aware of that. Check <https://www.postgresql.org/docs/14/datatype-datetime.html>
                // for more information
                write!(f, "TZ")
            }
        }
    }
}

/// Fields for [Postgres] `INTERVAL` type.
///
/// [Postgres]: https://www.postgresql.org/docs/17/datatype-datetime.html
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Eq, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "visitor", derive(Visit, VisitMut))]
pub enum IntervalFields {
    /// `YEAR` field
    Year,
    /// `MONTH` field
    Month,
    /// `DAY` field
    Day,
    /// `HOUR` field
    Hour,
    /// `MINUTE` field
    Minute,
    /// `SECOND` field
    Second,
    /// `YEAR TO MONTH` field
    YearToMonth,
    /// `DAY TO HOUR` field
    DayToHour,
    /// `DAY TO MINUTE` field
    DayToMinute,
    /// `DAY TO SECOND` field
    DayToSecond,
    /// `HOUR TO MINUTE` field
    HourToMinute,
    /// `HOUR TO SECOND` field
    HourToSecond,
    /// `MINUTE TO SECOND` field
    MinuteToSecond,
}

impl fmt::Display for IntervalFields {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IntervalFields::Year => write!(f, "YEAR"),
            IntervalFields::Month => write!(f, "MONTH"),
            IntervalFields::Day => write!(f, "DAY"),
            IntervalFields::Hour => write!(f, "HOUR"),
            IntervalFields::Minute => write!(f, "MINUTE"),
            IntervalFields::Second => write!(f, "SECOND"),
            IntervalFields::YearToMonth => write!(f, "YEAR TO MONTH"),
            IntervalFields::DayToHour => write!(f, "DAY TO HOUR"),
            IntervalFields::DayToMinute => write!(f, "DAY TO MINUTE"),
            IntervalFields::DayToSecond => write!(f, "DAY TO SECOND"),
            IntervalFields::HourToMinute => write!(f, "HOUR TO MINUTE"),
            IntervalFields::HourToSecond => write!(f, "HOUR TO SECOND"),
            IntervalFields::MinuteToSecond => write!(f, "MINUTE TO SECOND"),
        }
    }
}

/// Additional information for `NUMERIC`, `DECIMAL`, and `DEC` data types
/// following the 2016 [SQL Standard].
///
/// [SQL Standard]: https://jakewheat.github.io/sql-overview/sql-2016-foundation-grammar.html#exact-numeric-type
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Eq, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "visitor", derive(Visit, VisitMut))]
pub enum ExactNumberInfo {
    /// No additional information, e.g. `DECIMAL`.
    None,
    /// Only precision information, e.g. `DECIMAL(10)`.
    Precision(u64),
    /// Precision and scale information, e.g. `DECIMAL(10,2)`.
    PrecisionAndScale(u64, i64),
}

impl fmt::Display for ExactNumberInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExactNumberInfo::None => {
                write!(f, "")
            }
            ExactNumberInfo::Precision(p) => {
                write!(f, "({p})")
            }
            ExactNumberInfo::PrecisionAndScale(p, s) => {
                write!(f, "({p},{s})")
            }
        }
    }
}

/// Information about [character length][1], including length and possibly unit.
///
/// [1]: https://jakewheat.github.io/sql-overview/sql-2016-foundation-grammar.html#character-length
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Eq, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "visitor", derive(Visit, VisitMut))]
pub enum CharacterLength {
    /// Integer length with optional unit (e.g. `CHAR(10)` or `VARCHAR(10 CHARACTERS)`).
    IntegerLength {
        /// Default (if VARYING) or maximum (if not VARYING) length
        length: u64,
        /// Optional unit. If not informed, the ANSI handles it as CHARACTERS implicitly
        unit: Option<CharLengthUnits>,
    },
    /// VARCHAR(MAX) or NVARCHAR(MAX), used in T-SQL (Microsoft SQL Server).
    Max,
}

impl fmt::Display for CharacterLength {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CharacterLength::IntegerLength { length, unit } => {
                write!(f, "{length}")?;
                if let Some(unit) = unit {
                    write!(f, " {unit}")?;
                }
            }
            CharacterLength::Max => {
                write!(f, "MAX")?;
            }
        }
        Ok(())
    }
}

/// Possible units for characters, initially based on 2016 ANSI [SQL Standard][1].
///
/// [1]: https://jakewheat.github.io/sql-overview/sql-2016-foundation-grammar.html#char-length-units
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Eq, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "visitor", derive(Visit, VisitMut))]
pub enum CharLengthUnits {
    /// CHARACTERS unit
    Characters,
    /// OCTETS unit
    Octets,
}

impl fmt::Display for CharLengthUnits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Characters => {
                write!(f, "CHARACTERS")
            }
            Self::Octets => {
                write!(f, "OCTETS")
            }
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Eq, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "visitor", derive(Visit, VisitMut))]
/// Information about [binary length][1], including length and possibly unit.
///
/// [1]: https://jakewheat.github.io/sql-overview/sql-2016-foundation-grammar.html#binary-length
pub enum BinaryLength {
    /// Integer length for binary types (e.g. `VARBINARY(100)`).
    IntegerLength {
        /// Default (if VARYING)
        length: u64,
    },
    /// VARBINARY(MAX) used in T-SQL (Microsoft SQL Server).
    Max,
}

impl fmt::Display for BinaryLength {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BinaryLength::IntegerLength { length } => {
                write!(f, "{length}")?;
            }
            BinaryLength::Max => {
                write!(f, "MAX")?;
            }
        }
        Ok(())
    }
}

/// Represents the data type of the elements in an array (if any) as well as
/// the syntax used to declare the array.
///
/// For example: Bigquery/Hive use `ARRAY<INT>` whereas snowflake uses ARRAY.
#[derive(Debug, Clone, PartialEq, PartialOrd, Eq, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "visitor", derive(Visit, VisitMut))]
pub enum ArrayElemTypeDef {
    /// Use `ARRAY` style without an explicit element type.
    None,
    /// Angle-bracket style, e.g. `ARRAY<INT>`.
    AngleBracket(Box<DataType>),
    /// Square-bracket style, e.g. `INT[]` or `INT[2]`.
    SquareBracket(Box<DataType>, Option<u64>),
    /// Parenthesis style, e.g. `Array(Int64)`.
    Parenthesis(Box<DataType>),
}

/// Represents different types of geometric shapes which are commonly used in
/// PostgreSQL/Redshift for spatial operations and geometry-related computations.
///
/// [PostgreSQL]: https://www.postgresql.org/docs/9.5/functions-geometry.html
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "visitor", derive(Visit, VisitMut))]
pub enum GeometricTypeKind {
    /// Point geometry
    Point,
    /// Line geometry
    Line,
    /// Line segment geometry
    LineSegment,
    /// Box geometry
    GeometricBox,
    /// Path geometry
    GeometricPath,
    /// Polygon geometry
    Polygon,
    /// Circle geometry
    Circle,
}

impl fmt::Display for GeometricTypeKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            GeometricTypeKind::Point => write!(f, "point"),
            GeometricTypeKind::Line => write!(f, "line"),
            GeometricTypeKind::LineSegment => write!(f, "lseg"),
            GeometricTypeKind::GeometricBox => write!(f, "box"),
            GeometricTypeKind::GeometricPath => write!(f, "path"),
            GeometricTypeKind::Polygon => write!(f, "polygon"),
            GeometricTypeKind::Circle => write!(f, "circle"),
        }
    }
}
