//! Type system: values and their wire encodings.
//!
//! Scope: bool / int2 / int4 / int8 / float4 / float8 / text with text-format
//! encoding, plus the minimal `numeric` / `bytea` / `bit` and user-type support
//! the float regression tests need. `float` and `cast` hold the PG-exact I/O
//! and cast machinery.

pub mod bit;
pub mod cast;
pub mod date;
pub mod float;
pub mod geo;
pub mod interval;
pub mod macaddr;
pub mod money;
pub mod net;
pub mod numeric;
pub mod text;
pub mod time;
pub mod timestamp;
pub mod timestamptz;
pub mod timetz;
pub mod to_char;
pub mod tz;
pub mod uuid;

pub use interval::Interval;
pub use net::Inet;
pub use timetz::TimeTz;

/// OIDs of built-in types. Must match PostgreSQL's `pg_type.dat` — drivers
/// hardcode these.
pub mod oid {
    pub const BOOL: u32 = 16;
    pub const BYTEA: u32 = 17;
    pub const NAME: u32 = 19;
    pub const INT8: u32 = 20;
    /// `oid`: PostgreSQL's object-identifier type (unsigned 32-bit). Pervasive
    /// across `pg_catalog` (every `oid`/`reg*` column), so worth a real type.
    pub const OID: u32 = 26;
    pub const INT2: u32 = 21;
    pub const INT4: u32 = 23;
    pub const TEXT: u32 = 25;
    pub const BPCHAR: u32 = 1042;
    pub const VARCHAR: u32 = 1043;
    pub const FLOAT4: u32 = 700;
    pub const FLOAT8: u32 = 701;
    pub const MONEY: u32 = 790;
    pub const DATE: u32 = 1082;
    pub const TIME: u32 = 1083;
    pub const TIMESTAMP: u32 = 1114;
    pub const TIMESTAMPTZ: u32 = 1184;
    pub const INTERVAL: u32 = 1186;
    pub const TIMETZ: u32 = 1266;
    pub const BIT: u32 = 1560;
    pub const VARBIT: u32 = 1562;
    pub const NUMERIC: u32 = 1700;
    pub const CIDR: u32 = 650;
    pub const INET: u32 = 869;
    pub const MACADDR: u32 = 829;
    pub const MACADDR8: u32 = 774;
    pub const UUID: u32 = 2950;
    /// `point`: a geometric `(x, y)` pair of float8. See [`crate::geo`].
    pub const POINT: u32 = 600;
    /// `lseg`: a geometric line segment (two points). See [`crate::geo`].
    pub const LSEG: u32 = 601;
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
    /// `money` (a.k.a. `cash`): an `i64` count of hundredths. See [`crate::money`].
    Money,
    Text,
    /// `character varying` / `varchar`. Shares the `text` value representation;
    /// a length limit is applied as a coercion, never stored on the type.
    Varchar,
    /// `character` / `char` / `bpchar`. Values are blank-padded to their
    /// declared length at coercion time.
    Bpchar,
    /// `name`: a 63-character identifier type backed by `text`.
    Name,
    /// `oid`: an unsigned 32-bit object identifier. Fixed 4-byte type; values
    /// print as unsigned decimals. Backs `pg_catalog` OID/`reg*` columns.
    Oid,
    Bytea,
    /// `bit(n)`: a fixed-length bit string. The declared length is enforced as a
    /// coercion; the runtime value carries its own length.
    Bit,
    /// `bit varying(n)` / `varbit`: a variable-length bit string. Shares
    /// [`Value::Bit`] with `bit`; the static type distinguishes the length rules
    /// (max vs exact) and the error/`\d` spelling.
    Varbit,
    /// `date`.
    Date,
    /// `time without time zone`.
    Time,
    /// `time with time zone`.
    TimeTz,
    /// `timestamp without time zone`.
    Timestamp,
    /// `timestamp with time zone`.
    TimestampTz,
    /// `interval`.
    Interval,
    /// `uuid`: 16 raw bytes. See [`crate::uuid`].
    Uuid,
    /// `inet`: an IPv4/IPv6 host or network address. See [`crate::net`].
    Inet,
    /// `cidr`: an IPv4/IPv6 network specification. See [`crate::net`].
    Cidr,
    /// `macaddr`: a 6-byte MAC address. See [`crate::macaddr`].
    Macaddr,
    /// `macaddr8`: an 8-byte MAC address (EUI-64). See [`crate::macaddr`].
    Macaddr8,
    /// `point`: a geometric `(x, y)` pair. Fixed 16-byte type. See [`crate::geo`].
    Point,
    /// `lseg`: a geometric line segment (two points). Fixed 32-byte type.
    /// See [`crate::geo`].
    Lseg,
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
            PgType::Money => oid::MONEY,
            PgType::Text => oid::TEXT,
            PgType::Varchar => oid::VARCHAR,
            PgType::Bpchar => oid::BPCHAR,
            PgType::Name => oid::NAME,
            PgType::Oid => oid::OID,
            PgType::Bytea => oid::BYTEA,
            PgType::Bit => oid::BIT,
            PgType::Varbit => oid::VARBIT,
            PgType::Date => oid::DATE,
            PgType::Time => oid::TIME,
            PgType::TimeTz => oid::TIMETZ,
            PgType::Timestamp => oid::TIMESTAMP,
            PgType::TimestampTz => oid::TIMESTAMPTZ,
            PgType::Interval => oid::INTERVAL,
            PgType::Uuid => oid::UUID,
            PgType::Inet => oid::INET,
            PgType::Cidr => oid::CIDR,
            PgType::Macaddr => oid::MACADDR,
            PgType::Macaddr8 => oid::MACADDR8,
            PgType::Point => oid::POINT,
            PgType::Lseg => oid::LSEG,
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
            PgType::Date => 4,
            PgType::Time => 8,
            PgType::TimeTz => 12,
            PgType::Timestamp => 8,
            PgType::TimestampTz => 8,
            PgType::Money => 8,
            PgType::Interval => 16,
            PgType::Uuid => 16,
            PgType::Oid => 4,
            PgType::Macaddr => 6,
            PgType::Macaddr8 => 8,
            PgType::Point => 16,
            PgType::Lseg => 32,
            // `name` is a fixed 64-byte type; the rest are varlena.
            PgType::Name => 64,
            PgType::Numeric
            | PgType::Text
            | PgType::Varchar
            | PgType::Bpchar
            | PgType::Bytea
            | PgType::Bit
            | PgType::Varbit
            | PgType::Inet
            | PgType::Cidr => -1,
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
            PgType::Money => "money",
            PgType::Text => "text",
            PgType::Varchar => "character varying",
            PgType::Bpchar => "character",
            PgType::Name => "name",
            PgType::Oid => "oid",
            PgType::Bytea => "bytea",
            PgType::Bit => "bit",
            PgType::Varbit => "bit varying",
            PgType::Date => "date",
            PgType::Time => "time without time zone",
            PgType::TimeTz => "time with time zone",
            PgType::Timestamp => "timestamp without time zone",
            PgType::TimestampTz => "timestamp with time zone",
            PgType::Interval => "interval",
            PgType::Uuid => "uuid",
            PgType::Inet => "inet",
            PgType::Cidr => "cidr",
            PgType::Macaddr => "macaddr",
            PgType::Macaddr8 => "macaddr8",
            PgType::Point => "point",
            PgType::Lseg => "lseg",
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
            PgType::Money => "money",
            PgType::Text => "text",
            PgType::Varchar => "varchar",
            PgType::Bpchar => "bpchar",
            PgType::Name => "name",
            PgType::Oid => "oid",
            PgType::Bytea => "bytea",
            PgType::Bit => "bit",
            PgType::Varbit => "varbit",
            PgType::Date => "date",
            PgType::Time => "time",
            PgType::TimeTz => "timetz",
            PgType::Timestamp => "timestamp",
            PgType::TimestampTz => "timestamptz",
            PgType::Interval => "interval",
            PgType::Uuid => "uuid",
            PgType::Inet => "inet",
            PgType::Cidr => "cidr",
            PgType::Macaddr => "macaddr",
            PgType::Macaddr8 => "macaddr8",
            PgType::Point => "point",
            PgType::Lseg => "lseg",
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
    /// `money`: a signed count of hundredths (cents). See [`crate::money`].
    Money(i64),
    /// `oid`: an unsigned 32-bit object identifier. Prints as an unsigned
    /// decimal; carries the referenced OID for `reg*`-style catalog columns.
    Oid(u32),
    Text(String),
    Bytea(Vec<u8>),
    /// A `bit`/`bit varying` value: `len` bits packed most-significant-bit-first
    /// in `data` (`ceil(len/8)` bytes, trailing pad bits zero). See [`crate::bit`].
    Bit { len: u32, data: Vec<u8> },
    /// `date`: signed days since 2000-01-01, with `i32::MIN`/`i32::MAX` as the
    /// `-infinity`/`infinity` sentinels. See [`crate::date`].
    Date(i32),
    /// `time without time zone`: microseconds since midnight. See [`crate::time`].
    Time(i64),
    /// `time with time zone`: local time-of-day plus a UTC offset. See
    /// [`crate::timetz`].
    TimeTz(TimeTz),
    /// `timestamp without time zone`: microseconds since 2000-01-01, with
    /// `i64::MIN`/`i64::MAX` as the `-infinity`/`infinity` sentinels. See
    /// [`crate::timestamp`].
    Timestamp(i64),
    /// `timestamp with time zone`: microseconds since 2000-01-01 in UTC, with
    /// the same `i64::MIN`/`i64::MAX` sentinels. See [`crate::timestamptz`].
    TimestampTz(i64),
    /// `interval`: PG's `{ months, days, usec }` split. See [`crate::interval`].
    Interval(Interval),
    /// `uuid`: 16 raw bytes in network order. See [`crate::uuid`].
    Uuid([u8; 16]),
    /// `inet`: an IPv4/IPv6 host or network address. See [`crate::net`].
    Inet(Inet),
    /// `cidr`: an IPv4/IPv6 network specification. See [`crate::net`].
    Cidr(Inet),
    /// `macaddr`: 6 raw bytes. See [`crate::macaddr`].
    Macaddr([u8; 6]),
    /// `macaddr8`: 8 raw bytes (EUI-64). See [`crate::macaddr`].
    Macaddr8([u8; 8]),
    /// `point`: an `(x, y)` pair of float8. See [`crate::geo`].
    Point([f64; 2]),
    /// `lseg`: a line segment `[(x1,y1),(x2,y2)]` of float8. See [`crate::geo`].
    Lseg([f64; 4]),
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
            Value::Money(_) => Some(PgType::Money),
            Value::Oid(_) => Some(PgType::Oid),
            Value::Text(_) => Some(PgType::Text),
            Value::Bytea(_) => Some(PgType::Bytea),
            Value::Bit { .. } => Some(PgType::Bit),
            Value::Date(_) => Some(PgType::Date),
            Value::Time(_) => Some(PgType::Time),
            Value::TimeTz(_) => Some(PgType::TimeTz),
            Value::Timestamp(_) => Some(PgType::Timestamp),
            Value::TimestampTz(_) => Some(PgType::TimestampTz),
            Value::Interval(_) => Some(PgType::Interval),
            Value::Uuid(_) => Some(PgType::Uuid),
            Value::Inet(_) => Some(PgType::Inet),
            Value::Cidr(_) => Some(PgType::Cidr),
            Value::Macaddr(_) => Some(PgType::Macaddr),
            Value::Macaddr8(_) => Some(PgType::Macaddr8),
            Value::Point(_) => Some(PgType::Point),
            Value::Lseg(_) => Some(PgType::Lseg),
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
            Value::Money(c) => Some(money::format(*c)),
            Value::Oid(v) => Some(v.to_string()),
            Value::Text(s) => Some(s.clone()),
            Value::Bytea(bytes) => {
                let mut out = String::with_capacity(2 + bytes.len() * 2);
                out.push_str("\\x");
                for b in bytes {
                    out.push_str(&format!("{b:02x}"));
                }
                Some(out)
            }
            Value::Bit { len, data } => Some(bit::format(*len, data)),
            Value::Date(d) => Some(date::format(*d)),
            Value::Time(usec) => Some(time::format(*usec)),
            Value::TimeTz(v) => Some(timetz::format(*v)),
            Value::Timestamp(micros) => Some(timestamp::format(*micros)),
            Value::TimestampTz(micros) => Some(timestamptz::format(*micros)),
            Value::Interval(iv) => Some(interval::format(*iv)),
            Value::Uuid(b) => Some(uuid::format(b)),
            Value::Inet(v) => Some(net::inet_out(v)),
            Value::Cidr(v) => Some(net::cidr_out(v)),
            Value::Macaddr(b) => Some(macaddr::format6(b)),
            Value::Macaddr8(b) => Some(macaddr::format8(b)),
            Value::Point(p) => Some(geo::format_point(p, efd)),
            Value::Lseg(l) => Some(geo::format_lseg(l, efd)),
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
        assert_eq!(PgType::Oid.oid(), 26);
    }

    #[test]
    fn oid_encodes_as_unsigned_decimal() {
        assert_eq!(Value::Oid(2200).encode_text().as_deref(), Some("2200"));
        // Past i32::MAX: an oid is unsigned, so it must not print negative.
        assert_eq!(
            Value::Oid(u32::MAX).encode_text().as_deref(),
            Some("4294967295")
        );
        assert_eq!(PgType::Oid.typname(), "oid");
        assert_eq!(PgType::Oid.typlen(), 4);
    }

    #[test]
    fn bytea_hex_encoding() {
        assert_eq!(
            Value::Bytea(vec![0x00, 0x10, 0x00]).encode_text().as_deref(),
            Some("\\x001000")
        );
    }
}
