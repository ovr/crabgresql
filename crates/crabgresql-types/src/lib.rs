//! Type system: values and their wire encodings.
//!
//! M0 scope: bool / int4 / int8 / text with text-format encoding only.
//! Binary encodings, numeric, datetime and the cast machinery arrive with M1+.

/// OIDs of built-in types. Must match PostgreSQL's `pg_type.dat` — drivers
/// hardcode these.
pub mod oid {
    pub const BOOL: u32 = 16;
    pub const INT8: u32 = 20;
    pub const INT4: u32 = 23;
    pub const TEXT: u32 = 25;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PgType {
    Bool,
    Int4,
    Int8,
    Text,
}

impl PgType {
    pub fn oid(self) -> u32 {
        match self {
            PgType::Bool => oid::BOOL,
            PgType::Int4 => oid::INT4,
            PgType::Int8 => oid::INT8,
            PgType::Text => oid::TEXT,
        }
    }

    /// `pg_type.typlen`: byte width for fixed-size types, -1 for varlena.
    pub fn typlen(self) -> i16 {
        match self {
            PgType::Bool => 1,
            PgType::Int4 => 4,
            PgType::Int8 => 8,
            PgType::Text => -1,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            PgType::Bool => "boolean",
            PgType::Int4 => "integer",
            PgType::Int8 => "bigint",
            PgType::Text => "text",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int4(i32),
    Int8(i64),
    Text(String),
}

impl Value {
    pub fn pg_type(&self) -> Option<PgType> {
        match self {
            Value::Null => None,
            Value::Bool(_) => Some(PgType::Bool),
            Value::Int4(_) => Some(PgType::Int4),
            Value::Int8(_) => Some(PgType::Int8),
            Value::Text(_) => Some(PgType::Text),
        }
    }

    /// Text-format encoding as sent in `DataRow`; `None` encodes SQL NULL.
    pub fn encode_text(&self) -> Option<String> {
        match self {
            Value::Null => None,
            Value::Bool(b) => Some(if *b { "t" } else { "f" }.to_string()),
            Value::Int4(v) => Some(v.to_string()),
            Value::Int8(v) => Some(v.to_string()),
            Value::Text(s) => Some(s.clone()),
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
    }
}
