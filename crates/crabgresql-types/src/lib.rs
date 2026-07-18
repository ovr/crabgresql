//! Type system: values and their wire encodings.
//!
//! Scope: bool / int2 / int4 / int8 / float4 / float8 / text with text-format
//! encoding, plus the minimal `numeric` / `bytea` / `bit` and user-type support
//! the float regression tests need. `float` and `cast` hold the PG-exact I/O
//! and cast machinery.

pub mod cast;
pub mod float;
pub mod interval;
pub mod numeric;
pub mod timestamp;
pub mod timestamptz;
pub mod to_char;
pub mod tz;

pub use interval::Interval;

/// OIDs of built-in types. Must match PostgreSQL's `pg_type.dat` — drivers
/// hardcode these.
pub mod oid {
    pub const BOOL: u32 = 16;
    pub const BYTEA: u32 = 17;
    pub const INT8: u32 = 20;
    pub const INT2: u32 = 21;
    pub const INT4: u32 = 23;
    pub const TEXT: u32 = 25;
    pub const FLOAT4: u32 = 700;
    pub const FLOAT8: u32 = 701;
    pub const TIMESTAMP: u32 = 1114;
    pub const TIMESTAMPTZ: u32 = 1184;
    pub const INTERVAL: u32 = 1186;
    pub const BIT: u32 = 1560;
    pub const NUMERIC: u32 = 1700;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PgType {
    Bool,
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    Numeric,
    Text,
    Bytea,
    Bit,
    /// `timestamp without time zone`.
    Timestamp,
    /// `timestamp with time zone`.
    TimestampTz,
    /// `interval`.
    Interval,
    /// A user-defined type (`CREATE TYPE`); values are stored using the
    /// backing built-in representation, so this only carries the assigned OID.
    User(u32),
}

impl PgType {
    pub fn oid(self) -> u32 {
        match self {
            PgType::Bool => oid::BOOL,
            PgType::Int2 => oid::INT2,
            PgType::Int4 => oid::INT4,
            PgType::Int8 => oid::INT8,
            PgType::Float4 => oid::FLOAT4,
            PgType::Float8 => oid::FLOAT8,
            PgType::Numeric => oid::NUMERIC,
            PgType::Text => oid::TEXT,
            PgType::Bytea => oid::BYTEA,
            PgType::Bit => oid::BIT,
            PgType::Timestamp => oid::TIMESTAMP,
            PgType::TimestampTz => oid::TIMESTAMPTZ,
            PgType::Interval => oid::INTERVAL,
            PgType::User(oid) => oid,
        }
    }

    /// `pg_type.typlen`: byte width for fixed-size types, -1 for varlena.
    pub fn typlen(self) -> i16 {
        match self {
            PgType::Bool => 1,
            PgType::Int2 => 2,
            PgType::Int4 => 4,
            PgType::Int8 => 8,
            PgType::Float4 => 4,
            PgType::Float8 => 8,
            PgType::Timestamp => 8,
            PgType::TimestampTz => 8,
            PgType::Interval => 16,
            PgType::Numeric | PgType::Text | PgType::Bytea | PgType::Bit => -1,
            PgType::User(_) => -1,
        }
    }

    /// Display name as it appears in error messages (`double precision`, ...).
    pub fn name(self) -> &'static str {
        match self {
            PgType::Bool => "boolean",
            PgType::Int2 => "smallint",
            PgType::Int4 => "integer",
            PgType::Int8 => "bigint",
            PgType::Float4 => "real",
            PgType::Float8 => "double precision",
            PgType::Numeric => "numeric",
            PgType::Text => "text",
            PgType::Bytea => "bytea",
            PgType::Bit => "bit",
            PgType::Timestamp => "timestamp without time zone",
            PgType::TimestampTz => "timestamp with time zone",
            PgType::Interval => "interval",
            PgType::User(_) => "user-defined",
        }
    }

    /// Catalog `typname` (used for cast-derived column headers): `float4`, not
    /// `real`. `'NaN'::float4` yields a column named `float4`.
    pub fn typname(self) -> &'static str {
        match self {
            PgType::Bool => "bool",
            PgType::Int2 => "int2",
            PgType::Int4 => "int4",
            PgType::Int8 => "int8",
            PgType::Float4 => "float4",
            PgType::Float8 => "float8",
            PgType::Numeric => "numeric",
            PgType::Text => "text",
            PgType::Bytea => "bytea",
            PgType::Bit => "bit",
            PgType::Timestamp => "timestamp",
            PgType::TimestampTz => "timestamptz",
            PgType::Interval => "interval",
            PgType::User(_) => "user-defined",
        }
    }

    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            PgType::Int2
                | PgType::Int4
                | PgType::Int8
                | PgType::Float4
                | PgType::Float8
                | PgType::Numeric
        )
    }
}

pub use numeric::Numeric;

/// `boolin`: the spellings PG's boolean input accepts — any unambiguous
/// case-insensitive prefix of true/false/yes/no/off, exact "on", and "1"/"0"
/// (a bare "o" is ambiguous between on and off) — trimmed. Shared by the
/// binder (unknown-literal resolution) and `cast::cast_value` (text→bool).
pub fn parse_bool(s: &str) -> Option<bool> {
    let s = s.trim().to_ascii_lowercase();
    match s.as_str() {
        "" => None,
        "1" | "on" => Some(true),
        "0" => Some(false),
        _ if "true".starts_with(&s) || "yes".starts_with(&s) => Some(true),
        _ if "false".starts_with(&s) || "no".starts_with(&s) => Some(false),
        _ if s.len() >= 2 && "off".starts_with(&s) => Some(false),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int2(i16),
    Int4(i32),
    Int8(i64),
    Float4(f32),
    Float8(f64),
    Numeric(Numeric),
    Text(String),
    Bytea(Vec<u8>),
    /// A bit-string literal (`x'...'`): `len` bits packed right-aligned in
    /// `bits`. Only produced by hex literals and consumed by bit→int casts.
    Bit { len: u16, bits: u64 },
    /// `timestamp without time zone`: microseconds since 2000-01-01, with
    /// `i64::MIN`/`i64::MAX` as the `-infinity`/`infinity` sentinels. See
    /// [`crate::timestamp`].
    Timestamp(i64),
    /// `timestamp with time zone`: microseconds since 2000-01-01 in UTC, with
    /// the same `i64::MIN`/`i64::MAX` sentinels. See [`crate::timestamptz`].
    TimestampTz(i64),
    /// `interval`: PG's `{ months, days, usec }` split. See [`crate::interval`].
    Interval(Interval),
}

impl Value {
    pub fn pg_type(&self) -> Option<PgType> {
        match self {
            Value::Null => None,
            Value::Bool(_) => Some(PgType::Bool),
            Value::Int2(_) => Some(PgType::Int2),
            Value::Int4(_) => Some(PgType::Int4),
            Value::Int8(_) => Some(PgType::Int8),
            Value::Float4(_) => Some(PgType::Float4),
            Value::Float8(_) => Some(PgType::Float8),
            Value::Numeric(_) => Some(PgType::Numeric),
            Value::Text(_) => Some(PgType::Text),
            Value::Bytea(_) => Some(PgType::Bytea),
            Value::Bit { .. } => Some(PgType::Bit),
            Value::Timestamp(_) => Some(PgType::Timestamp),
            Value::TimestampTz(_) => Some(PgType::TimestampTz),
            Value::Interval(_) => Some(PgType::Interval),
        }
    }

    /// Text-format encoding at the default `extra_float_digits` (1).
    pub fn encode_text(&self) -> Option<String> {
        self.encode_text_with(1)
    }

    /// Text-format encoding as sent in `DataRow`; `None` encodes SQL NULL.
    /// `efd` is `extra_float_digits`, affecting only float output.
    pub fn encode_text_with(&self, efd: i32) -> Option<String> {
        match self {
            Value::Null => None,
            Value::Bool(b) => Some(if *b { "t" } else { "f" }.to_string()),
            Value::Int2(v) => Some(v.to_string()),
            Value::Int4(v) => Some(v.to_string()),
            Value::Int8(v) => Some(v.to_string()),
            Value::Float4(v) => Some(float::fmt_f32(*v, efd)),
            Value::Float8(v) => Some(float::fmt_f64(*v, efd)),
            Value::Numeric(n) => Some(n.to_display()),
            Value::Text(s) => Some(s.clone()),
            Value::Bytea(bytes) => {
                let mut out = String::with_capacity(2 + bytes.len() * 2);
                out.push_str("\\x");
                for b in bytes {
                    out.push_str(&format!("{b:02x}"));
                }
                Some(out)
            }
            Value::Bit { len, bits } => {
                Some(format!("{:0width$b}", bits, width = *len as usize))
            }
            Value::Timestamp(micros) => Some(timestamp::format(*micros)),
            Value::TimestampTz(micros) => Some(timestamptz::format(*micros)),
            Value::Interval(iv) => Some(interval::format(*iv)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bool_encodes_as_t_f() {
        assert_eq!(Value::Bool(true).encode_text().as_deref(), Some("t"));
        assert_eq!(Value::Bool(false).encode_text().as_deref(), Some("f"));
    }

    #[test]
    fn null_encodes_as_none() {
        assert_eq!(Value::Null.encode_text(), None);
    }

    #[test]
    fn oids_match_pg_catalog() {
        assert_eq!(PgType::Bool.oid(), 16);
        assert_eq!(PgType::Int8.oid(), 20);
        assert_eq!(PgType::Int4.oid(), 23);
        assert_eq!(PgType::Text.oid(), 25);
        assert_eq!(PgType::Float4.oid(), 700);
        assert_eq!(PgType::Float8.oid(), 701);
        assert_eq!(PgType::Int2.oid(), 21);
        assert_eq!(PgType::Bytea.oid(), 17);
    }

    #[test]
    fn bytea_hex_encoding() {
        assert_eq!(
            Value::Bytea(vec![0x00, 0x10, 0x00]).encode_text().as_deref(),
            Some("\\x001000")
        );
    }
}
