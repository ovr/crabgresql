//! Type system: values and their wire encodings.
//!
//! Scope: bool / int2 / int4 / int8 / float4 / float8 / text with text-format
//! encoding, plus the minimal `numeric` / `bytea` / `bit` and user-type support
//! the float regression tests need. `float` and `cast` hold the PG-exact I/O
//! and cast machinery.

pub mod array;
pub mod bit;
pub mod cast;
pub mod char;
pub mod collation;
pub mod date;
pub mod datum;
pub mod float;
pub mod fmt;
pub mod formatting;
pub mod formatting_num;
pub mod geo;
pub mod interval;
pub mod json;
pub mod jsonpath;
pub mod macaddr;
pub mod money;
pub mod net;
pub mod numeric;
pub mod pg_lsn;
pub mod text;
pub mod tid;
pub mod time;
pub mod timestamp;
pub mod timestamptz;
pub mod timetz;
pub mod tsquery;
pub mod tsvector;
pub mod tz;
pub mod uuid;
pub mod vector;
pub mod wire;
pub mod xid;

pub use fmt::FmtCtx;
pub use interval::Interval;
pub use net::Inet;
pub use timetz::TimeTz;
pub use vector::VectorKind;

/// OIDs of built-in types. Must match PostgreSQL's `pg_type.dat` — drivers
/// hardcode these.
pub mod oid {
    pub const BOOL: u32 = 16;
    pub const BYTEA: u32 = 17;
    /// `"char"`: PG's ad-hoc one-byte type, *not* `bpchar`. See [`crate::char`].
    pub const CHAR: u32 = 18;
    pub const NAME: u32 = 19;
    pub const INT8: u32 = 20;
    /// `oid`: PostgreSQL's object-identifier type (unsigned 32-bit). Pervasive
    /// across `pg_catalog` (every `oid`/`reg*` column), so worth a real type.
    pub const OID: u32 = 26;
    /// `tid`: a tuple identifier, the `(block, offset)` address of a row
    /// version — the type of the `ctid` system column. See [`crate::tid`].
    pub const TID: u32 = 27;
    /// `xid`: a 32-bit transaction id. See [`crate::xid`].
    pub const XID: u32 = 28;
    /// `xid8`: a 64-bit transaction id. See [`crate::xid`].
    pub const XID8: u32 = 5069;
    /// `pg_lsn`: a WAL log sequence number. See [`crate::pg_lsn`].
    pub const PG_LSN: u32 = 3220;
    pub const INT2: u32 = 21;
    /// `int2vector`: a vector of `int2`, used by `pg_index.indkey` and friends.
    /// See [`crate::vector`].
    pub const INT2VECTOR: u32 = 22;
    pub const INT4: u32 = 23;
    /// `oidvector`: a vector of `oid`, used by `pg_proc.proargtypes`.
    /// See [`crate::vector`].
    pub const OIDVECTOR: u32 = 30;
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
    /// `path`: a geometric open or closed list of points. See [`crate::geo`].
    pub const PATH: u32 = 602;
    /// `box`: a geometric rectangle given by two opposite corners.
    /// See [`crate::geo`].
    pub const BOX: u32 = 603;
    /// `polygon`: a geometric closed vertex list with an interior.
    /// See [`crate::geo`].
    pub const POLYGON: u32 = 604;
    /// `line`: an infinite geometric line `Ax + By + C = 0`. See [`crate::geo`].
    pub const LINE: u32 = 628;
    /// `circle`: a geometric center point plus a radius. See [`crate::geo`].
    pub const CIRCLE: u32 = 718;
    /// `json`: JSON stored as validated text. See [`crate::json`].
    pub const JSON: u32 = 114;
    /// `jsonb`: JSON stored as a canonical binary tree. See [`crate::json`].
    pub const JSONB: u32 = 3802;
    /// `jsonpath`: an SQL/JSON path expression. See [`crate::jsonpath`].
    pub const JSONPATH: u32 = 4072;
    /// `unknown`: the type of a literal that has not yet been resolved. It has
    /// no [`crate::PgType`] of its own — the binder models it as
    /// `Binding::Unknown` — but `pg_typeof` and `705::regtype` have to name it.
    pub const UNKNOWN: u32 = 705;
    /// `regclass`: an OID that renders as a relation name. See [`crate::Reg`].
    pub const REGCLASS: u32 = 2205;
    /// `regtype`: an OID that renders as a type name. See [`crate::Reg`].
    pub const REGTYPE: u32 = 2206;
    /// `regnamespace`: an OID that renders as a schema name. See [`crate::Reg`].
    pub const REGNAMESPACE: u32 = 4089;
    /// `tsvector`: a sorted list of weighted lexemes. See [`crate::tsvector`].
    pub const TSVECTOR: u32 = 3614;
    /// `tsquery`: a text-search query expression. See [`crate::tsquery`].
    pub const TSQUERY: u32 = 3615;

    // Array type OIDs (`pg_type.typarray` of the element type). Element↔array
    // mapping lives in [`crate::array`].
    pub const BOOL_ARRAY: u32 = 1000;
    pub const BYTEA_ARRAY: u32 = 1001;
    /// `"char"[]`. Note it is 1002, *not* adjacent to `BPCHAR_ARRAY` (1014).
    pub const CHAR_ARRAY: u32 = 1002;
    pub const NAME_ARRAY: u32 = 1003;
    pub const INT2_ARRAY: u32 = 1005;
    pub const INT4_ARRAY: u32 = 1007;
    pub const TEXT_ARRAY: u32 = 1009;
    pub const BPCHAR_ARRAY: u32 = 1014;
    pub const VARCHAR_ARRAY: u32 = 1015;
    pub const INT8_ARRAY: u32 = 1016;
    pub const POINT_ARRAY: u32 = 1017;
    pub const LSEG_ARRAY: u32 = 1018;
    pub const PATH_ARRAY: u32 = 1019;
    pub const BOX_ARRAY: u32 = 1020;
    pub const POLYGON_ARRAY: u32 = 1027;
    pub const LINE_ARRAY: u32 = 629;
    pub const CIRCLE_ARRAY: u32 = 719;
    pub const FLOAT4_ARRAY: u32 = 1021;
    pub const FLOAT8_ARRAY: u32 = 1022;
    pub const OID_ARRAY: u32 = 1028;
    pub const TID_ARRAY: u32 = 1010;
    pub const XID_ARRAY: u32 = 1011;
    pub const XID8_ARRAY: u32 = 271;
    pub const PG_LSN_ARRAY: u32 = 3221;
    pub const MACADDR_ARRAY: u32 = 1040;
    pub const MACADDR8_ARRAY: u32 = 775;
    pub const INET_ARRAY: u32 = 1041;
    pub const CIDR_ARRAY: u32 = 651;
    pub const NUMERIC_ARRAY: u32 = 1231;
    pub const MONEY_ARRAY: u32 = 791;
    pub const UUID_ARRAY: u32 = 2951;
    pub const JSON_ARRAY: u32 = 199;
    pub const JSONB_ARRAY: u32 = 3807;
    pub const JSONPATH_ARRAY: u32 = 4073;
    pub const TSVECTOR_ARRAY: u32 = 3643;
    pub const TSQUERY_ARRAY: u32 = 3645;
    pub const DATE_ARRAY: u32 = 1182;
    pub const TIME_ARRAY: u32 = 1183;
    pub const TIMETZ_ARRAY: u32 = 1270;
    pub const TIMESTAMP_ARRAY: u32 = 1115;
    pub const TIMESTAMPTZ_ARRAY: u32 = 1185;
    pub const INTERVAL_ARRAY: u32 = 1187;
    pub const BIT_ARRAY: u32 = 1561;
    pub const VARBIT_ARRAY: u32 = 1563;
    pub const REGCLASS_ARRAY: u32 = 2210;
    pub const REGTYPE_ARRAY: u32 = 2211;
    pub const REGNAMESPACE_ARRAY: u32 = 4090;
    pub const INT2VECTOR_ARRAY: u32 = 1006;
    pub const OIDVECTOR_ARRAY: u32 = 1013;
}

#[derive(deepsize::DeepSizeOf, Clone, Copy, Debug, PartialEq, Eq)]
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
    /// `"char"`: PG's ad-hoc one-*byte* type, distinct from [`PgType::Bpchar`]
    /// and reachable only through the quoted spelling. Holds a raw byte that
    /// need not be valid UTF-8, so it does not share the `text` representation.
    /// See [`crate::char`].
    Char,
    /// `name`: a 63-character identifier type backed by `text`.
    Name,
    /// `oid`: an unsigned 32-bit object identifier. Fixed 4-byte type; values
    /// print as unsigned decimals. Backs `pg_catalog` OID/`reg*` columns.
    Oid,
    /// `tid`: the `(block, offset)` address of a row version, and the type of
    /// the `ctid` system column. Fixed 6-byte type. See [`crate::tid`].
    Tid,
    /// `xid`: a 32-bit transaction id. Fixed 4-byte type. Has equality but no
    /// ordering — see [`crate::xid`]. See also [`PgType::Xid8`].
    Xid,
    /// `xid8`: a 64-bit transaction id. Fixed 8-byte type, fully ordered.
    /// See [`crate::xid`].
    Xid8,
    /// `pg_lsn`: a WAL log sequence number. Fixed 8-byte type, fully ordered,
    /// with arithmetic against `numeric`. See [`crate::pg_lsn`].
    PgLsn,
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
    /// `path`: a geometric list of points, open (`[..]`) or closed (`(..)`).
    /// Varlena; no default b-tree opclass. See [`crate::geo`].
    Path,
    /// `box`: a geometric rectangle, stored with its corners normalized. Fixed
    /// 32-byte type; `=` compares area, not identity. See [`crate::geo`].
    Box,
    /// `polygon`: a geometric closed vertex list. Varlena. See [`crate::geo`].
    Polygon,
    /// `line`: an infinite geometric line `Ax + By + C = 0`. Fixed 24-byte type.
    /// See [`crate::geo`].
    Line,
    /// `circle`: a geometric center plus radius. Fixed 24-byte type; `=`
    /// compares area, not identity. See [`crate::geo`].
    Circle,
    /// `json`: JSON kept as validated text (whitespace/key order/duplicate keys
    /// preserved). Varlena; no default equality. See [`crate::json`].
    Json,
    /// `jsonb`: JSON stored as a canonical tree ([`Value::Jsonb`]). Varlena;
    /// fully ordered/hashable. See [`crate::json`].
    Jsonb,
    /// `jsonpath`: a compiled SQL/JSON path program ([`Value::Jsonpath`]).
    /// Varlena; no default equality/ordering. See [`crate::jsonpath`].
    Jsonpath,
    /// A `reg*` type: an OID that renders as the name of the object it
    /// identifies ([`Value::Reg`]). Fixed 4-byte type. The three PG types
    /// share one variant because they differ only in *what* the OID names —
    /// see [`RegKind`].
    Reg(RegKind),
    /// `tsvector`: a sorted list of distinct lexemes with weighted positions
    /// ([`Value::Tsvector`]). Varlena; ordered and hashable.
    /// See [`crate::tsvector`].
    Tsvector,
    /// `tsquery`: a text-search query tree ([`Value::Tsquery`]). Varlena;
    /// ordered and hashable. See [`crate::tsquery`].
    Tsquery,
    /// `oidvector` or `int2vector` ([`Value::Vector`]): a vector of a fixed
    /// element type, used by PG's own catalogs. Varlena; ordered and hashable.
    /// The two PG types share one variant because they differ only in the
    /// element type — see [`VectorKind`].
    ///
    /// Deliberately **not** an array type: `is_array` is false and
    /// [`PgType::array_element`] returns `None`, so `ARRAY[v]` builds an
    /// `oidvector[]` rather than flattening, matching PG. Subscripting and
    /// `unnest` reach the element type through [`VectorKind::element`].
    /// See [`crate::vector`].
    Vector(VectorKind),
    /// A user-defined type (`CREATE TYPE`); values are stored using the
    /// backing built-in representation, so this only carries the assigned OID.
    User(u32),
    /// A one-dimensional array (`T[]`). Carries the **element** type's OID
    /// (e.g. `Array(oid::INT4)` is `integer[]` / `_int4`); dimensionality is a
    /// property of the value, not the type, matching PG (`int[]` and `int[][]`
    /// are the same type). Recover the element [`PgType`] with
    /// [`PgType::from_oid`]; the array's own OID via [`crate::array::array_oid_for_elem`].
    Array(u32),
}

/// Which kind of object a `reg*` OID names. Each variant is a distinct
/// PostgreSQL type that stores an OID and renders as that object's name, so
/// `'pg_class'::regclass` and `1259::regclass` are the same value.
///
/// PG has more of these (`regproc`, `regoper`, `regconfig`, `regrole`, …);
/// only the three crabgresql can actually resolve are modeled. `regproc` in
/// particular needs a `pg_proc` this build does not have, so `pg_type.typinput`
/// and friends stay `text` in the catalog.
#[derive(deepsize::DeepSizeOf, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegKind {
    /// `regclass`: names a relation (table, view, sequence, index).
    Class,
    /// `regtype`: names a type.
    Type,
    /// `regnamespace`: names a schema.
    Namespace,
}

impl RegKind {
    pub fn oid(self) -> u32 {
        match self {
            RegKind::Class => oid::REGCLASS,
            RegKind::Type => oid::REGTYPE,
            RegKind::Namespace => oid::REGNAMESPACE,
        }
    }

    /// Catalog `typname`, which for these is also the SQL spelling.
    pub fn typname(self) -> &'static str {
        match self {
            RegKind::Class => "regclass",
            RegKind::Type => "regtype",
            RegKind::Namespace => "regnamespace",
        }
    }

    /// What the object is called in the "does not exist" error a failed lookup
    /// raises — PG reports `relation "x" does not exist` for `regclass` but
    /// `type "x" does not exist` for `regtype`.
    pub fn object_noun(self) -> &'static str {
        match self {
            RegKind::Class => "relation",
            RegKind::Type => "type",
            RegKind::Namespace => "schema",
        }
    }
}

/// A `reg*` value: the OID, plus the text it renders as.
///
/// PostgreSQL stores only the OID and resolves the name in the type's output
/// function, at output time. `encode_text` here is pure — it has no catalog
/// handle, and giving every type one would be a far larger change — so the name
/// is resolved when the value is *produced* (the cast) and carried alongside.
/// The difference is observable only if the object is renamed between the cast
/// and output, or if a `reg*` value is stored in a table and the object is
/// renamed afterwards.
///
/// Equality is by `(kind, oid)` only, never the name: the same OID reached
/// through different paths (resolved by a cast, or read back from disk) must
/// still compare equal. Ordering and hashing agree — see `compare_values` and
/// `hash_key` in the executor.
#[derive(deepsize::DeepSizeOf, Clone, Debug)]
pub struct Reg {
    pub kind: RegKind,
    pub oid: u32,
    /// The rendered name: `pg_class`, a schema-qualified `rs.t`, a quoted
    /// `"Mixed Case"`, `-` for OID 0, or the bare digits when the OID resolves
    /// to nothing (all probed against PG 18.4).
    pub name: String,
}

impl Reg {
    /// The rendering PG gives an OID that names nothing: `-` for `0`
    /// (`InvalidOid`), the bare digits otherwise.
    pub fn unresolved(kind: RegKind, oid: u32) -> Self {
        let name = if oid == 0 {
            "-".to_string()
        } else {
            oid.to_string()
        };
        Self { kind, oid, name }
    }
}

impl PartialEq for Reg {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.oid == other.oid
    }
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
            PgType::Char => oid::CHAR,
            PgType::Name => oid::NAME,
            PgType::Oid => oid::OID,
            PgType::Tid => oid::TID,
            PgType::Xid => oid::XID,
            PgType::Xid8 => oid::XID8,
            PgType::PgLsn => oid::PG_LSN,
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
            PgType::Path => oid::PATH,
            PgType::Box => oid::BOX,
            PgType::Polygon => oid::POLYGON,
            PgType::Line => oid::LINE,
            PgType::Circle => oid::CIRCLE,
            PgType::Json => oid::JSON,
            PgType::Jsonb => oid::JSONB,
            PgType::Jsonpath => oid::JSONPATH,
            PgType::Reg(kind) => kind.oid(),
            PgType::Tsvector => oid::TSVECTOR,
            PgType::Tsquery => oid::TSQUERY,
            PgType::Vector(kind) => kind.oid(),
            PgType::User(oid) => oid,
            PgType::Array(elem) => array::array_oid_for_elem(elem).unwrap_or(0),
        }
    }

    /// The element type of an array type, or `None` for a non-array type.
    pub fn array_element(self) -> Option<PgType> {
        match self {
            PgType::Array(elem) => PgType::from_oid(elem),
            _ => None,
        }
    }

    /// Whether the executor's `agg::hash_key` gives distinct values of this type
    /// distinct-enough hashes — i.e. equality is a raw-representation compare, so
    /// hashing that representation agrees with `keys_equal`. Types that fail this
    /// (`interval`, `timetz`, `inet`, `cidr`, `bit`, `varbit`, `point`, `lseg`,
    /// and user types) all hash into one shared bucket, so a hash join keyed on
    /// them would collapse to a full scan; the join planner keeps such equalities
    /// as nested-loop predicates instead. Must stay in sync with `hash_key`.
    pub fn hashes_distinctly(self) -> bool {
        matches!(
            self,
            PgType::Bool
                | PgType::Int2
                | PgType::Int4
                | PgType::Int8
                | PgType::Float4
                | PgType::Float8
                | PgType::Numeric
                | PgType::Money
                | PgType::Text
                | PgType::Varchar
                | PgType::Bpchar
                | PgType::Char
                | PgType::Name
                | PgType::Oid
                | PgType::Tid
                | PgType::Xid
                | PgType::Xid8
                | PgType::PgLsn
                | PgType::Bytea
                | PgType::Date
                | PgType::Time
                | PgType::Timestamp
                | PgType::TimestampTz
                | PgType::Uuid
                | PgType::Macaddr
                | PgType::Macaddr8
                // jsonb equality is structural on its canonical tree, so hashing
                // that tree agrees with `keys_equal`. `json` has no equality and
                // never reaches `hash_key`.
                | PgType::Jsonb
                // A reg* value compares by OID alone, and `hash_key` hashes the
                // OID alone — the carried name is never part of either.
                | PgType::Reg(_)
                // `tsvector` is canonicalized on input, so its structural hash
                // agrees with `keys_equal`. `tsquery` is deliberately absent:
                // its equality ignores a leaf's prefix flag and weight mask (as
                // PG's does), which the derived `Hash` cannot, so hashing it
                // would split groups `keys_equal` calls equal.
                | PgType::Tsvector
                // A vector's elements are `oid`/`int2`, both of which hash
                // distinctly, and its equality is element-wise — so hashing the
                // element sequence agrees with `keys_equal`.
                | PgType::Vector(_)
        )
    }

    /// Whether this is a (one-dimensional) array type.
    pub fn is_array(self) -> bool {
        matches!(self, PgType::Array(_))
    }

    /// Resolve a built-in type OID back to its [`PgType`], the reverse of
    /// [`PgType::oid`]. Used to map the parameter type OIDs a `Parse` message
    /// declares. Returns `None` for `0` ("unspecified", to be inferred) and any
    /// OID this build has no built-in type for. User-type OIDs are not resolved
    /// here — a `Parse` cannot name a `CREATE TYPE` OID as a parameter type.
    pub fn from_oid(oid: u32) -> Option<PgType> {
        Some(match oid {
            oid::BOOL => PgType::Bool,
            oid::INT2 => PgType::Int2,
            oid::INT4 => PgType::Int4,
            oid::INT8 => PgType::Int8,
            oid::FLOAT4 => PgType::Float4,
            oid::FLOAT8 => PgType::Float8,
            oid::NUMERIC => PgType::Numeric,
            oid::MONEY => PgType::Money,
            oid::TEXT => PgType::Text,
            oid::VARCHAR => PgType::Varchar,
            oid::BPCHAR => PgType::Bpchar,
            oid::CHAR => PgType::Char,
            oid::NAME => PgType::Name,
            oid::OID => PgType::Oid,
            oid::TID => PgType::Tid,
            oid::XID => PgType::Xid,
            oid::XID8 => PgType::Xid8,
            oid::PG_LSN => PgType::PgLsn,
            oid::BYTEA => PgType::Bytea,
            oid::BIT => PgType::Bit,
            oid::VARBIT => PgType::Varbit,
            oid::DATE => PgType::Date,
            oid::TIME => PgType::Time,
            oid::TIMETZ => PgType::TimeTz,
            oid::TIMESTAMP => PgType::Timestamp,
            oid::TIMESTAMPTZ => PgType::TimestampTz,
            oid::INTERVAL => PgType::Interval,
            oid::UUID => PgType::Uuid,
            oid::INET => PgType::Inet,
            oid::CIDR => PgType::Cidr,
            oid::MACADDR => PgType::Macaddr,
            oid::MACADDR8 => PgType::Macaddr8,
            oid::POINT => PgType::Point,
            oid::LSEG => PgType::Lseg,
            oid::PATH => PgType::Path,
            oid::BOX => PgType::Box,
            oid::POLYGON => PgType::Polygon,
            oid::LINE => PgType::Line,
            oid::CIRCLE => PgType::Circle,
            oid::JSON => PgType::Json,
            oid::JSONB => PgType::Jsonb,
            oid::JSONPATH => PgType::Jsonpath,
            oid::REGCLASS => PgType::Reg(RegKind::Class),
            oid::REGTYPE => PgType::Reg(RegKind::Type),
            oid::REGNAMESPACE => PgType::Reg(RegKind::Namespace),
            oid::TSVECTOR => PgType::Tsvector,
            oid::TSQUERY => PgType::Tsquery,
            oid::OIDVECTOR => PgType::Vector(VectorKind::Oid),
            oid::INT2VECTOR => PgType::Vector(VectorKind::Int2),
            // Array type OIDs (`_int4`, `_text`, ...) decode to `Array(elem)`.
            other => match array::elem_oid_for_array(other) {
                Some(elem) => PgType::Array(elem),
                None => return None,
            },
        })
    }

    /// Resolve a built-in type *name* to its [`PgType`], the reverse of
    /// [`PgType::typname`] and [`PgType::name`]. Every built-in answers to both
    /// its catalog spelling (`int4`, `float8`, `timestamptz`) and its SQL
    /// spelling (`integer`, `double precision`, `timestamp with time zone`),
    /// plus the aliases SQL allows (`int`, `decimal`, `char`). `None` for a name
    /// this build has no built-in for — a user type, or an unsupported one.
    ///
    /// Built-in type names live in `pg_catalog`, so this is what a bare or
    /// `pg_catalog.`-qualified name resolves against before the user-type
    /// catalog is consulted. Multi-word spellings only reach here from a catalog
    /// name (`CREATE TYPE ... LIKE`); the parser gives those their own
    /// `DataType` variants.
    ///
    /// `pg_type_rows_agree_with_pgtype_for_modeled_types` in `crabgresql-catalog`
    /// checks every modeled type's vendored `typname` against this, so a new
    /// type cannot land here spelled differently than the catalog spells it.
    pub fn from_name(name: &str) -> Option<PgType> {
        Some(match name {
            "bool" | "boolean" => PgType::Bool,
            "int2" | "smallint" => PgType::Int2,
            "int4" | "integer" | "int" => PgType::Int4,
            "int8" | "bigint" => PgType::Int8,
            "float4" | "real" => PgType::Float4,
            "float8" | "double precision" => PgType::Float8,
            "numeric" | "decimal" => PgType::Numeric,
            "money" => PgType::Money,
            "text" => PgType::Text,
            "varchar" | "character varying" => PgType::Varchar,
            "bpchar" | "character" => PgType::Bpchar,
            // This table maps a *catalog typname*, and `pg_type.typname` for oid
            // 18 is `char` — so that is what the bare string resolves to here.
            //
            // It is NOT the SQL type-name grammar, where an unquoted `char` is
            // the `char(1)` keyword (`bpchar`) and only a quoted `"char"` is oid
            // 18. Callers holding user-written type *syntax* must apply that
            // grammar themselves before falling back here, because quoting is
            // already lost by the time a plain `&str` arrives: see
            // `builtin_type_oid_from_syntax` in the executor's `reg` module and
            // the `LIKE` arm of the server's `type_shape_from_options`.
            "char" => PgType::Char,
            "name" => PgType::Name,
            "oid" => PgType::Oid,
            "tid" => PgType::Tid,
            "xid" => PgType::Xid,
            "xid8" => PgType::Xid8,
            "pg_lsn" => PgType::PgLsn,
            "bytea" => PgType::Bytea,
            "bit" => PgType::Bit,
            "varbit" | "bit varying" => PgType::Varbit,
            "date" => PgType::Date,
            "time" | "time without time zone" => PgType::Time,
            "timetz" | "time with time zone" => PgType::TimeTz,
            "timestamp" | "timestamp without time zone" => PgType::Timestamp,
            "timestamptz" | "timestamp with time zone" => PgType::TimestampTz,
            "interval" => PgType::Interval,
            "uuid" => PgType::Uuid,
            "inet" => PgType::Inet,
            "cidr" => PgType::Cidr,
            "macaddr" => PgType::Macaddr,
            "macaddr8" => PgType::Macaddr8,
            "point" => PgType::Point,
            "lseg" => PgType::Lseg,
            "path" => PgType::Path,
            "box" => PgType::Box,
            "polygon" => PgType::Polygon,
            "line" => PgType::Line,
            "circle" => PgType::Circle,
            "json" => PgType::Json,
            "jsonb" => PgType::Jsonb,
            "jsonpath" => PgType::Jsonpath,
            "tsvector" => PgType::Tsvector,
            "tsquery" => PgType::Tsquery,
            "regclass" => PgType::Reg(RegKind::Class),
            "regtype" => PgType::Reg(RegKind::Type),
            "regnamespace" => PgType::Reg(RegKind::Namespace),
            "oidvector" => PgType::Vector(VectorKind::Oid),
            "int2vector" => PgType::Vector(VectorKind::Int2),
            _ => return None,
        })
    }

    /// `pg_type.typlen`: byte width for fixed-size types, -1 for varlena.
    pub fn typlen(self) -> i16 {
        match self {
            PgType::Bool => 1,
            PgType::Char => 1,
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
            // Every reg* type is a 4-byte OID under the name it renders as.
            PgType::Reg(_) => 4,
            PgType::Macaddr => 6,
            PgType::Macaddr8 => 8,
            // A `BlockNumber` (4) plus an `OffsetNumber` (2).
            PgType::Tid => 6,
            PgType::Xid => 4,
            PgType::Xid8 => 8,
            PgType::PgLsn => 8,
            PgType::Point => 16,
            PgType::Lseg => 32,
            PgType::Box => 32,
            PgType::Line => 24,
            PgType::Circle => 24,
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
            | PgType::Cidr
            | PgType::Json
            | PgType::Jsonb
            | PgType::Jsonpath
            | PgType::Path
            | PgType::Polygon
            | PgType::Tsvector
            | PgType::Tsquery
            // Both vectors are varlena: PG stores them in the array layout,
            // whose length is the element count.
            | PgType::Vector(_) => -1,
            PgType::User(_) => -1,
            // Arrays are varlena.
            PgType::Array(_) => -1,
        }
    }

    /// Display name carrying a type modifier, as `format_type` renders it
    /// (`character varying(10)`, `numeric(4,2)`, `timestamp(3) with time zone`).
    ///
    /// Each type prints its modifier only above its own threshold, matching the
    /// `typmodout` functions (probed against PostgreSQL 18.4): the character
    /// types need more than the four-byte varlena header they reserve, `numeric`
    /// needs at least that header, and the rest need only a non-negative value.
    /// Below the threshold PostgreSQL prints the bare type name rather than a
    /// nonsensical `character varying(-2)`.
    ///
    /// `interval` is a deliberate gap: its modifier packs range bits and is
    /// printed bare rather than decoded. Arrays and non-built-in types are the
    /// caller's problem — they need a catalog this crate does not have.
    pub fn name_with_typmod(self, typmod: i32) -> String {
        // VARHDRSZ: character types encode `length + 4`; the storage layer's
        // `Column::atttypmod` is what produces that encoding.
        const VARHDRSZ: i32 = 4;
        let m = typmod;
        match self {
            PgType::Numeric if m >= VARHDRSZ => {
                let m = m - VARHDRSZ;
                // The scale is an 11-bit *signed* field, so `numeric(4,-2)`
                // round trips; the precision is masked to the 16 bits above it.
                let precision = (m >> 16) & 0xffff;
                let scale = (((m & 0x7ff) ^ 1024) - 1024) as i16;
                format!("numeric({precision},{scale})")
            }
            PgType::Varchar if m > VARHDRSZ => format!("character varying({})", m - VARHDRSZ),
            PgType::Bpchar if m > VARHDRSZ => format!("character({})", m - VARHDRSZ),
            // `bpchar` is the one type that reports which spelling it was asked
            // about. This arm is the "asked with a modifier I cannot print" case
            // — which includes `-1`, the modifier `pg_attribute` stores for an
            // unmodified column, so it is the arm `\d` takes for one. The other
            // case, "asked with no modifier at all", cannot arrive here: it is an
            // absent `Option` that callers resolve to [`Self::name`] before
            // reaching this function.
            PgType::Bpchar => "bpchar".to_string(),
            PgType::Bit if m >= 0 => format!("bit({m})"),
            PgType::Varbit if m >= 0 => format!("bit varying({m})"),
            // The precision goes *before* the "with[out] time zone" suffix.
            PgType::Time if m >= 0 => format!("time({m}) without time zone"),
            PgType::TimeTz if m >= 0 => format!("time({m}) with time zone"),
            PgType::Timestamp if m >= 0 => format!("timestamp({m}) without time zone"),
            PgType::TimestampTz if m >= 0 => format!("timestamp({m}) with time zone"),
            // Below its type's threshold a modifier prints nothing at all.
            _ => self.name().to_string(),
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
            // PG quotes it everywhere it renders a type name, to keep it
            // distinct from the `char` keyword that means `bpchar`:
            // `format_type(18, null)` is `"char"`.
            PgType::Char => "\"char\"",
            PgType::Varchar => "character varying",
            PgType::Bpchar => "character",
            PgType::Name => "name",
            PgType::Oid => "oid",
            PgType::Tid => "tid",
            PgType::Xid => "xid",
            PgType::Xid8 => "xid8",
            PgType::PgLsn => "pg_lsn",
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
            PgType::Path => "path",
            PgType::Box => "box",
            PgType::Polygon => "polygon",
            PgType::Line => "line",
            PgType::Circle => "circle",
            PgType::Json => "json",
            PgType::Jsonb => "jsonb",
            PgType::Jsonpath => "jsonpath",
            PgType::Reg(kind) => kind.typname(),
            PgType::Tsvector => "tsvector",
            PgType::Tsquery => "tsquery",
            // Neither vector has a separate SQL spelling.
            PgType::Vector(kind) => kind.typname(),
            PgType::User(_) => "user-defined",
            PgType::Array(elem) => array_display_name(elem),
        }
    }

    /// PostgreSQL's SQL spelling of this type with `typmod` applied — the body
    /// of the `format_type(oid, typmod)` function, and the type label a deparsed
    /// constant carries. `None` for [`PgType::Array`] and [`PgType::User`],
    /// whose names need a catalog the type layer has no access to.
    ///
    /// `typmod` is `None` when no modifier was given at all and `Some(m)` for
    /// one that was — including `Some(-1)`, the "no modifier" value
    /// `pg_attribute` stores, which PostgreSQL still distinguishes from `None`
    /// (`bpchar` and `bit` both report themselves differently for each).
    ///
    /// Each type prints its modifier only above its own threshold, matching the
    /// `typmodout` functions (probed against PostgreSQL 18.4): the character
    /// types need more than the four-byte varlena header they reserve, `numeric`
    /// needs at least that header, and the rest need only a non-negative value.
    /// Below the threshold PostgreSQL prints the bare type name rather than a
    /// nonsensical `character varying(-2)`.
    ///
    /// One deliberate gap, not reachable from a crabgresql catalog row (it needs
    /// a modifier this build never stores): PostgreSQL's generic fallback for a
    /// type with no `typmodout` (`format_type(25, 5)` → `text(5)`) is not
    /// reproduced.
    pub fn format_type(self, typmod: Option<i32>) -> Option<String> {
        // VARHDRSZ: character types encode `length + 4` (see the catalog's
        // `atttypmod_of`, which is the encoder this decodes).
        const VARHDRSZ: i32 = 4;
        if matches!(self, PgType::Array(_) | PgType::User(_)) {
            return None;
        }
        let name = self.name();
        let Some(m) = typmod else {
            return Some(name.to_string());
        };
        Some(match self {
            PgType::Numeric if m >= VARHDRSZ => {
                let (precision, scale) = Numeric::unpack_typmod(m - VARHDRSZ);
                format!("numeric({precision},{scale})")
            }
            PgType::Varchar if m > VARHDRSZ => format!("character varying({})", m - VARHDRSZ),
            PgType::Bpchar if m > VARHDRSZ => format!("character({})", m - VARHDRSZ),
            // `bpchar` and `bit` are the two types that report which spelling
            // they were asked about. Given a modifier they cannot print,
            // `bpchar` is `bpchar` (not `character`) and `bit` is quoted, to
            // keep it distinct from the `bit` keyword that a re-parse would read
            // as `bit(1)`. An unmodified column of either stores -1, so these
            // are the arms `\d` and a deparsed default take.
            PgType::Bpchar => "bpchar".to_string(),
            PgType::Bit if m >= 0 => format!("bit({m})"),
            PgType::Bit => "\"bit\"".to_string(),
            PgType::Varbit if m >= 0 => format!("bit varying({m})"),
            // The precision goes *before* the "with[out] time zone" suffix.
            PgType::Time if m >= 0 => format!("time({m}) without time zone"),
            PgType::TimeTz if m >= 0 => format!("time({m}) with time zone"),
            PgType::Timestamp if m >= 0 => format!("timestamp({m}) without time zone"),
            PgType::TimestampTz if m >= 0 => format!("timestamp({m}) with time zone"),
            // `interval` is the one type whose modifier is two things at once:
            // the fields it admits, spelled out, then the precision.
            PgType::Interval if m >= 0 => {
                let (range, precision) = interval::unpack_typmod(m);
                let mut s = name.to_string();
                if let Some(fields) = interval::range_name(range) {
                    s.push(' ');
                    s.push_str(fields);
                }
                if let Some(p) = precision {
                    s.push_str(&format!("({p})"));
                }
                s
            }
            // Below its type's threshold a modifier prints nothing at all.
            _ => name.to_string(),
        })
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
            PgType::Char => "char",
            PgType::Varchar => "varchar",
            PgType::Bpchar => "bpchar",
            PgType::Name => "name",
            PgType::Oid => "oid",
            PgType::Tid => "tid",
            PgType::Xid => "xid",
            PgType::Xid8 => "xid8",
            PgType::PgLsn => "pg_lsn",
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
            PgType::Path => "path",
            PgType::Box => "box",
            PgType::Polygon => "polygon",
            PgType::Line => "line",
            PgType::Circle => "circle",
            PgType::Json => "json",
            PgType::Jsonb => "jsonb",
            PgType::Jsonpath => "jsonpath",
            PgType::Reg(kind) => kind.typname(),
            PgType::Tsvector => "tsvector",
            PgType::Tsquery => "tsquery",
            PgType::Vector(kind) => kind.typname(),
            PgType::User(_) => "user-defined",
            PgType::Array(elem) => array_typname(elem),
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

    /// `pg_type.typdelim`: the character separating elements of an array of
    /// this type. Every built-in uses `,` except `box`, whose own output text is
    /// full of commas (`(1,1),(0,0)`), so PG gives it `;`.
    pub const fn typdelim(self) -> char {
        match self {
            PgType::Box => ';',
            _ => ',',
        }
    }

    /// Whether this type has a default B-tree operator class — i.e. it can be
    /// ordered, so it may key a B-tree / UNIQUE index or a PRIMARY KEY. `json`
    /// and the geometric types have no default ordering in PostgreSQL (and the
    /// executor's `compare_values` has no arm for them); everything else,
    /// including user types (enums order by ordinal), does. Must stay in sync
    /// with `crabgresql_executor::is_orderable`.
    ///
    /// `xid` is the one type whose `compare_values` arm exists even though this
    /// returns `false`: the executor needs an ordering internally to make
    /// `GROUP BY`/`DISTINCT` work through `keys_equal`, but no SQL surface
    /// exposes it.
    pub fn has_default_btree_opclass(self) -> bool {
        match self {
            PgType::Json
            | PgType::Jsonpath
            | PgType::Point
            | PgType::Lseg
            | PgType::Path
            | PgType::Box
            | PgType::Polygon
            | PgType::Line
            | PgType::Circle => false,
            // `xid` is the one type here with equality but no ordering: PG gives
            // it a hash opclass only, because transaction ids compare with
            // modular arithmetic. `xid8` is an ordinary counter and does have a
            // btree opclass. See `crate::xid`.
            PgType::Xid => false,
            // An array is orderable iff its element type is (element-wise btree
            // comparison). An unknown element type (no `from_oid`) is treated as
            // non-orderable.
            PgType::Array(elem) => {
                PgType::from_oid(elem).is_some_and(|e| e.has_default_btree_opclass())
            }
            _ => true,
        }
    }

    /// Whether values of this type carry a collation — the types PostgreSQL
    /// marks with a non-zero `pg_type.typcollation`. Only the string types are
    /// collatable here; `COLLATE` on anything else is a bind-time error, and a
    /// column of a non-collatable type records no collation.
    pub fn is_collatable(self) -> bool {
        matches!(
            self,
            PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name
        )
    }
}

/// PG's display spelling of an array type (`integer[]`) for error messages,
/// keyed on the element OID. Falls back to a generic `"array"` for an element
/// type this build does not special-case.
fn array_display_name(elem: u32) -> &'static str {
    match PgType::from_oid(elem) {
        Some(PgType::Bool) => "boolean[]",
        Some(PgType::Int2) => "smallint[]",
        Some(PgType::Int4) => "integer[]",
        Some(PgType::Int8) => "bigint[]",
        Some(PgType::Float4) => "real[]",
        Some(PgType::Float8) => "double precision[]",
        Some(PgType::Numeric) => "numeric[]",
        Some(PgType::Money) => "money[]",
        Some(PgType::Text) => "text[]",
        Some(PgType::Varchar) => "character varying[]",
        Some(PgType::Bpchar) => "character[]",
        Some(PgType::Char) => "\"char\"[]",
        Some(PgType::Name) => "name[]",
        Some(PgType::Oid) => "oid[]",
        Some(PgType::Tid) => "tid[]",
        Some(PgType::Xid) => "xid[]",
        Some(PgType::Xid8) => "xid8[]",
        Some(PgType::PgLsn) => "pg_lsn[]",
        Some(PgType::Bytea) => "bytea[]",
        Some(PgType::Bit) => "bit[]",
        Some(PgType::Varbit) => "bit varying[]",
        Some(PgType::Date) => "date[]",
        Some(PgType::Time) => "time without time zone[]",
        Some(PgType::TimeTz) => "time with time zone[]",
        Some(PgType::Timestamp) => "timestamp without time zone[]",
        Some(PgType::TimestampTz) => "timestamp with time zone[]",
        Some(PgType::Interval) => "interval[]",
        Some(PgType::Uuid) => "uuid[]",
        Some(PgType::Inet) => "inet[]",
        Some(PgType::Cidr) => "cidr[]",
        Some(PgType::Macaddr) => "macaddr[]",
        Some(PgType::Macaddr8) => "macaddr8[]",
        Some(PgType::Point) => "point[]",
        Some(PgType::Lseg) => "lseg[]",
        Some(PgType::Path) => "path[]",
        Some(PgType::Box) => "box[]",
        Some(PgType::Polygon) => "polygon[]",
        Some(PgType::Line) => "line[]",
        Some(PgType::Circle) => "circle[]",
        Some(PgType::Json) => "json[]",
        Some(PgType::Jsonb) => "jsonb[]",
        Some(PgType::Jsonpath) => "jsonpath[]",
        Some(PgType::Tsvector) => "tsvector[]",
        Some(PgType::Tsquery) => "tsquery[]",
        Some(PgType::Vector(VectorKind::Oid)) => "oidvector[]",
        Some(PgType::Vector(VectorKind::Int2)) => "int2vector[]",
        _ => "array",
    }
}

/// PG's catalog `typname` of an array type (`_int4`) — an underscore followed by
/// the element's `typname`, keyed on the element OID.
fn array_typname(elem: u32) -> &'static str {
    match PgType::from_oid(elem) {
        Some(PgType::Bool) => "_bool",
        Some(PgType::Int2) => "_int2",
        Some(PgType::Int4) => "_int4",
        Some(PgType::Int8) => "_int8",
        Some(PgType::Float4) => "_float4",
        Some(PgType::Float8) => "_float8",
        Some(PgType::Numeric) => "_numeric",
        Some(PgType::Money) => "_money",
        Some(PgType::Text) => "_text",
        Some(PgType::Varchar) => "_varchar",
        Some(PgType::Bpchar) => "_bpchar",
        Some(PgType::Char) => "_char",
        Some(PgType::Name) => "_name",
        Some(PgType::Oid) => "_oid",
        Some(PgType::Tid) => "_tid",
        Some(PgType::Xid) => "_xid",
        Some(PgType::Xid8) => "_xid8",
        Some(PgType::PgLsn) => "_pg_lsn",
        Some(PgType::Bytea) => "_bytea",
        Some(PgType::Bit) => "_bit",
        Some(PgType::Varbit) => "_varbit",
        Some(PgType::Date) => "_date",
        Some(PgType::Time) => "_time",
        Some(PgType::TimeTz) => "_timetz",
        Some(PgType::Timestamp) => "_timestamp",
        Some(PgType::TimestampTz) => "_timestamptz",
        Some(PgType::Interval) => "_interval",
        Some(PgType::Uuid) => "_uuid",
        Some(PgType::Inet) => "_inet",
        Some(PgType::Cidr) => "_cidr",
        Some(PgType::Macaddr) => "_macaddr",
        Some(PgType::Macaddr8) => "_macaddr8",
        Some(PgType::Point) => "_point",
        Some(PgType::Lseg) => "_lseg",
        Some(PgType::Path) => "_path",
        Some(PgType::Box) => "_box",
        Some(PgType::Polygon) => "_polygon",
        Some(PgType::Line) => "_line",
        Some(PgType::Circle) => "_circle",
        Some(PgType::Json) => "_json",
        Some(PgType::Jsonb) => "_jsonb",
        Some(PgType::Jsonpath) => "_jsonpath",
        Some(PgType::Tsvector) => "_tsvector",
        Some(PgType::Tsquery) => "_tsquery",
        Some(PgType::Vector(VectorKind::Oid)) => "_oidvector",
        Some(PgType::Vector(VectorKind::Int2)) => "_int2vector",
        _ => "array",
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

#[derive(deepsize::DeepSizeOf, Clone, Debug, PartialEq)]
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
    /// `tid`: the `(block, offset)` address of a row version. See [`crate::tid`].
    Tid {
        block: u32,
        offset: u16,
    },
    /// `xid`: a 32-bit transaction id. See [`crate::xid`].
    Xid(u32),
    /// `xid8`: a 64-bit transaction id. See [`crate::xid`].
    Xid8(u64),
    /// `pg_lsn`: a WAL log sequence number. See [`crate::pg_lsn`].
    PgLsn(u64),
    /// A `reg*` value: an OID that prints as the name of what it identifies.
    /// See [`Reg`] for why the name is carried rather than resolved at output.
    Reg(Reg),
    Text(String),
    /// A `"char"` value: one raw byte, which need not be valid UTF-8 — hence a
    /// `u8` rather than a one-character [`Value::Text`]. Ordering and hashing
    /// treat it as unsigned; only the `int4` conversion reads it as signed.
    /// See [`crate::char`].
    Char(u8),
    Bytea(Vec<u8>),
    /// A `bit`/`bit varying` value: `len` bits packed most-significant-bit-first
    /// in `data` (`ceil(len/8)` bytes, trailing pad bits zero). See [`crate::bit`].
    Bit {
        len: u32,
        data: Vec<u8>,
    },
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
    /// `path`: an open or closed list of points. See [`crate::geo`].
    Path(geo::PathVal),
    /// `box`: `[high.x, high.y, low.x, low.y]`, kept in PG's normal form where
    /// `high` is componentwise >= `low`. See [`crate::geo`].
    Box([f64; 4]),
    /// `polygon`: a closed vertex list with an interior. See [`crate::geo`].
    Polygon(geo::PolygonVal),
    /// `line`: the coefficients `[A, B, C]` of `Ax + By + C = 0`.
    /// See [`crate::geo`].
    Line([f64; 3]),
    /// `circle`: `[center.x, center.y, radius]`. See [`crate::geo`].
    Circle([f64; 3]),
    /// `json`: validated JSON text, kept verbatim. See [`crate::json`].
    Json(String),
    /// `jsonb`: a canonical parsed JSON tree. See [`crate::json`].
    Jsonb(json::Jsonb),
    /// `jsonpath`: a compiled SQL/JSON path program. See [`crate::jsonpath`].
    Jsonpath(jsonpath::JsonPath),
    /// `tsvector`: sorted lexemes with weighted positions. See [`crate::tsvector`].
    Tsvector(tsvector::TsVector),
    /// `tsquery`: a text-search query tree. See [`crate::tsquery`].
    Tsquery(tsquery::TsQuery),
    /// A `CREATE TYPE ... AS ENUM` value. `type_oid` is the enum's type OID (so
    /// [`Value::pg_type`] reports `PgType::User(type_oid)`); `ordinal` is the
    /// 0-based position of the label in the enum's definition, which is also its
    /// sort order (enums order by definition, not alphabetically); `label` is the
    /// text spelling, used for output and `enum → text`. Carrying the ordinal in
    /// the value lets the executor order enums without any catalog access.
    Enum {
        type_oid: u32,
        ordinal: u32,
        label: String,
    },
    /// A one-dimensional array. `elem` is the element type (so
    /// [`Value::pg_type`] can report `PgType::Array(elem.oid())` even for an
    /// empty array); `elems` are the element values, which may be
    /// [`Value::Null`]. See [`crate::array`].
    Array {
        elem: PgType,
        elems: Vec<Value>,
    },
    /// An `oidvector`/`int2vector`. `elems` are [`Value::Oid`]/[`Value::Int2`]
    /// per `kind` and are never [`Value::Null`] — neither input function has a
    /// spelling for one. See [`crate::vector`].
    Vector {
        kind: VectorKind,
        elems: Vec<Value>,
    },
}

impl Value {
    pub fn pg_type(&self) -> Option<PgType> {
        match self {
            Value::Null => None,
            Value::Bool(_) => Some(PgType::Bool),
            Value::Char(_) => Some(PgType::Char),
            Value::Int2(_) => Some(PgType::Int2),
            Value::Int4(_) => Some(PgType::Int4),
            Value::Int8(_) => Some(PgType::Int8),
            Value::Float4(_) => Some(PgType::Float4),
            Value::Float8(_) => Some(PgType::Float8),
            Value::Numeric(_) => Some(PgType::Numeric),
            Value::Money(_) => Some(PgType::Money),
            Value::Oid(_) => Some(PgType::Oid),
            Value::Tid { .. } => Some(PgType::Tid),
            Value::Xid(_) => Some(PgType::Xid),
            Value::Xid8(_) => Some(PgType::Xid8),
            Value::PgLsn(_) => Some(PgType::PgLsn),
            Value::Reg(r) => Some(PgType::Reg(r.kind)),
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
            Value::Path(_) => Some(PgType::Path),
            Value::Box(_) => Some(PgType::Box),
            Value::Polygon(_) => Some(PgType::Polygon),
            Value::Line(_) => Some(PgType::Line),
            Value::Circle(_) => Some(PgType::Circle),
            Value::Json(_) => Some(PgType::Json),
            Value::Jsonb(_) => Some(PgType::Jsonb),
            Value::Jsonpath(_) => Some(PgType::Jsonpath),
            Value::Tsvector(_) => Some(PgType::Tsvector),
            Value::Tsquery(_) => Some(PgType::Tsquery),
            Value::Enum { type_oid, .. } => Some(PgType::User(*type_oid)),
            Value::Array { elem, .. } => Some(PgType::Array(elem.oid())),
            Value::Vector { kind, .. } => Some(PgType::Vector(*kind)),
        }
    }

    /// Text-format encoding in the UTC display zone at the default
    /// `extra_float_digits` (1) — for callers with no session behind them.
    pub fn encode_text_utc(&self) -> Option<String> {
        self.encode_text_with(&FmtCtx::utc_default())
    }

    /// Text-format encoding as sent in `DataRow`; `None` encodes SQL NULL.
    /// `fmt` carries `extra_float_digits` (float output) and the session
    /// display zone (`timestamptz` output).
    pub fn encode_text_with(&self, fmt: &FmtCtx) -> Option<String> {
        let efd = fmt.efd;
        match self {
            Value::Null => None,
            Value::Bool(b) => Some(if *b { "t" } else { "f" }.to_string()),
            // This is also the `"char" -> text` cast: PG's `char_text` and
            // `charout` produce the same string, so the generic
            // `(_, PgType::Text)` arm in `cast.rs` needs no `Char` case.
            Value::Char(c) => Some(crate::char::char_out(*c)),
            Value::Int2(v) => Some(v.to_string()),
            Value::Int4(v) => Some(v.to_string()),
            Value::Int8(v) => Some(v.to_string()),
            Value::Float4(v) => Some(float::fmt_f32(*v, efd)),
            Value::Float8(v) => Some(float::fmt_f64(*v, efd)),
            Value::Numeric(n) => Some(n.to_display()),
            Value::Money(c) => Some(money::format(*c)),
            Value::Oid(v) => Some(v.to_string()),
            Value::Tid { block, offset } => Some(tid::format(*block, *offset)),
            // Both transaction id types print as unsigned decimals.
            Value::Xid(v) => Some(v.to_string()),
            Value::Xid8(v) => Some(v.to_string()),
            Value::PgLsn(v) => Some(pg_lsn::format(*v)),
            // The name was resolved when the value was built; this is the whole
            // of the reg* output function.
            Value::Reg(r) => Some(r.name.clone()),
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
            Value::TimestampTz(micros) => Some(timestamptz::format(*micros, &fmt.zone)),
            Value::Interval(iv) => Some(interval::format(*iv)),
            Value::Uuid(b) => Some(uuid::format(b)),
            Value::Inet(v) => Some(net::inet_out(v)),
            Value::Cidr(v) => Some(net::cidr_out(v)),
            Value::Macaddr(b) => Some(macaddr::format6(b)),
            Value::Macaddr8(b) => Some(macaddr::format8(b)),
            Value::Point(p) => Some(geo::format_point(p, efd)),
            Value::Lseg(l) => Some(geo::format_lseg(l, efd)),
            Value::Path(p) => Some(geo::format_path(p, efd)),
            Value::Box(b) => Some(geo::format_box(b, efd)),
            Value::Polygon(p) => Some(geo::format_polygon(p, efd)),
            Value::Line(l) => Some(geo::format_line(l, efd)),
            Value::Circle(c) => Some(geo::format_circle(c, efd)),
            // `json` prints its stored text verbatim; `jsonb` re-serializes its
            // canonical tree (`jsonb_out`).
            Value::Json(s) => Some(s.clone()),
            Value::Jsonb(j) => Some(json::format(j)),
            // `jsonpath` prints its canonical form (`jsonpath_out`).
            Value::Jsonpath(p) => Some(jsonpath::format(p)),
            // Both text-search types print their canonical form.
            Value::Tsvector(v) => Some(tsvector::format(v)),
            Value::Tsquery(q) => Some(tsquery::format(q)),
            // An enum prints as its label (PG's `enum_out`).
            Value::Enum { label, .. } => Some(label.clone()),
            // An array prints in PG's `{...}` form (`array_out`).
            Value::Array { elem, elems } => Some(array::format(*elem, elems, fmt)),
            // A vector prints space-separated and unbraced (`oidvectorout`).
            // Zone-independent: its elements are `oid`/`int2`.
            Value::Vector { elems, .. } => Some(vector::format(elems)),
        }
    }
}

macro_rules! impl_message_error {
    ($($ty:path),+ $(,)?) => {
        $(
            impl std::fmt::Display for $ty {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str(&self.message)
                }
            }

            impl std::error::Error for $ty {}
        )+
    };
}

impl_message_error!(
    array::ArrayError,
    bit::BitError,
    cast::CastError,
    date::DateError,
    float::FloatParseError,
    float::FloatError,
    geo::GeoError,
    interval::IntervalError,
    json::JsonError,
    macaddr::MacaddrError,
    money::MoneyError,
    net::NetError,
    numeric::NumErr,
    text::TextError,
    tid::TidError,
    time::TimeError,
    timestamp::TimestampError,
    timetz::TimeTzError,
    tsvector::TsError,
    pg_lsn::PgLsnError,
    uuid::UuidError,
    xid::XidError,
);

impl std::fmt::Display for numeric::ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            numeric::ParseError::Syntax => "invalid numeric syntax",
            numeric::ParseError::Overflow => "numeric value out of range",
        })
    }
}

impl std::error::Error for numeric::ParseError {}

impl std::fmt::Display for tz::ZoneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            tz::ZoneError::NotRecognized(name) => write!(f, "time zone {name:?} not recognized"),
            tz::ZoneError::DisplacementOutOfRange(name) => {
                write!(f, "time zone displacement out of range: {name:?}")
            }
        }
    }
}

impl std::error::Error for tz::ZoneError {}

#[cfg(test)]
mod tests {
    use super::*;
    use deepsize::DeepSizeOf;

    /// `bpchar` and `bit` are the two types whose spelling depends on *whether*
    /// a modifier was given, not just on its value. Probed against PostgreSQL
    /// 18.4: `format_type(1560, -1)` is `"bit"` but `format_type(1560, NULL)` is
    /// `bit`, and the quoted form is what a deparsed `bit` constant's type label
    /// uses (`'1001'::"bit"`).
    #[test]
    fn a_modifier_that_was_given_but_cannot_print_changes_the_spelling() {
        let ft = |ty: PgType, m: Option<i32>| ty.format_type(m);
        assert_eq!(ft(PgType::Bit, None).as_deref(), Some("bit"));
        assert_eq!(ft(PgType::Bit, Some(-1)).as_deref(), Some("\"bit\""));
        assert_eq!(ft(PgType::Bit, Some(4)).as_deref(), Some("bit(4)"));
        assert_eq!(ft(PgType::Bpchar, None).as_deref(), Some("character"));
        assert_eq!(ft(PgType::Bpchar, Some(-1)).as_deref(), Some("bpchar"));
        assert_eq!(ft(PgType::Bpchar, Some(8)).as_deref(), Some("character(4)"));
        // `bit varying` has one spelling either way.
        assert_eq!(ft(PgType::Varbit, None).as_deref(), Some("bit varying"));
        assert_eq!(ft(PgType::Varbit, Some(-1)).as_deref(), Some("bit varying"));
        assert_eq!(
            ft(PgType::Varbit, Some(5)).as_deref(),
            Some("bit varying(5)")
        );
        // The two catalog-dependent types decline, leaving their name to the
        // caller that holds the catalog.
        assert_eq!(PgType::Array(23).format_type(Some(-1)), None);
        assert_eq!(PgType::User(90000).format_type(Some(-1)), None);
    }

    #[test]
    fn bool_encodes_as_t_f() {
        assert_eq!(Value::Bool(true).encode_text_utc().as_deref(), Some("t"));
        assert_eq!(Value::Bool(false).encode_text_utc().as_deref(), Some("f"));
    }

    /// Pins the derived [`DeepSizeOf`]: a variant with no allocation of its
    /// own must report exactly the bytes it occupies in place. This is what
    /// fails if a future variant smuggles in a `Vec`/`String`/`Box` without the
    /// derive being able to see it, or if the derive is dropped from a payload
    /// type and its heap silently stops being counted.
    #[test]
    fn an_inline_value_reports_only_itself() {
        let inline = [
            Value::Null,
            Value::Bool(true),
            Value::Int2(-1),
            Value::Int4(0),
            Value::Int8(i64::MAX),
            Value::Float4(1.5),
            Value::Float8(-0.0),
            Value::Money(12345),
            Value::Oid(1259),
            Value::Tid {
                block: 4294967295,
                offset: 65535,
            },
            Value::Xid(4294967295),
            Value::Xid8(u64::MAX),
            Value::PgLsn(u64::MAX),
            Value::Date(-5),
            Value::Time(1),
            Value::TimeTz(TimeTz {
                usec: 1,
                zone: -3600,
            }),
            Value::Timestamp(0),
            Value::TimestampTz(i64::MIN),
            Value::Interval(Interval {
                months: -13,
                days: 2,
                usec: -999,
            }),
            Value::Uuid([9u8; 16]),
            Value::Macaddr([0x08, 0x00, 0x2b, 0x01, 0x02, 0x03]),
            Value::Macaddr8([0u8; 8]),
            Value::Point([5.1, 34.5]),
            Value::Lseg([1.0, 2.0, 3.0, 4.0]),
        ];
        for value in inline {
            assert_eq!(
                value.deep_size_of(),
                size_of::<Value>(),
                "{value:?} owns nothing beyond itself"
            );
        }
        // A heap-*capable* variant that has not allocated is in the same
        // position: an empty `Vec`/`String` never called the allocator.
        assert_eq!(
            Value::Text(String::new()).deep_size_of(),
            size_of::<Value>()
        );
        assert_eq!(Value::Bytea(Vec::new()).deep_size_of(), size_of::<Value>());
        assert_eq!(
            Value::Array {
                elem: PgType::Int4,
                elems: Vec::new()
            }
            .deep_size_of(),
            size_of::<Value>()
        );
    }

    #[test]
    fn every_heap_owning_variant_charges_its_payload() {
        let cases: Vec<(Value, usize)> = vec![
            (Value::Text("héllo world".into()), 12),
            (Value::Json("{\"b\": 1,  \"a\": 2}".into()), 17),
            (Value::Bytea(vec![0, 1, 2, 255]), 4),
            (
                Value::Bit {
                    len: 1000,
                    data: vec![0xA5; 125],
                },
                125,
            ),
            (
                Value::Reg(Reg {
                    kind: RegKind::Class,
                    oid: 1259,
                    name: "pg_class".into(),
                }),
                8,
            ),
            (
                Value::Enum {
                    type_oid: 16384,
                    ordinal: 0,
                    label: "red".into(),
                },
                3,
            ),
            (
                Value::Numeric(Numeric::parse("123.456").expect("valid numeric")),
                6,
            ),
            (
                json::jsonb_in("{\"b\":1,\"a\":[1,2,3],\"k\":\"v\"}")
                    .map(Value::Jsonb)
                    .expect("valid jsonb"),
                3,
            ),
            (
                jsonpath::jsonpath_in("$.a[*] ? (@ > 3)")
                    .map(Value::Jsonpath)
                    .expect("valid jsonpath"),
                1,
            ),
            (
                tsvector::tsvector_in("'a':1A,3B 'b' 'c':16383")
                    .map(Value::Tsvector)
                    .expect("valid tsvector"),
                3,
            ),
            (
                tsquery::tsquery_in("'a':*AB <2> ( 'b' | !'c' )")
                    .map(Value::Tsquery)
                    .expect("valid tsquery"),
                3,
            ),
            (
                Value::Array {
                    elem: PgType::Text,
                    elems: vec![Value::Text("a".into()), Value::Text("b,c".into())],
                },
                2,
            ),
        ];
        for (value, least) in cases {
            let charged = value.deep_size_of();
            let floor = size_of::<Value>() + least;
            assert!(
                charged >= floor,
                "{value:?} charged {charged}, below the {floor} bytes it visibly holds"
            );
        }
    }

    /// The property a type missing its `DeepSizeOf` derive cannot fake. A
    /// payload whose heap is not walked reports a constant, and a constant
    /// cannot grow with the data.
    #[test]
    fn a_bigger_payload_is_charged_more() -> anyhow::Result<()> {
        let small = Value::Text("x".repeat(10));
        let big = Value::Text("x".repeat(1000));
        assert!(big.deep_size_of() > small.deep_size_of());

        let scalar = Value::Jsonb(json::jsonb_in("1")?);
        let nested = Value::Jsonb(json::jsonb_in("{\"a\":[1,2,3],\"b\":{\"c\":\"d\"}}")?);
        assert!(nested.deep_size_of() > scalar.deep_size_of());

        let one_lexeme = Value::Tsvector(tsvector::tsvector_in("'a'")?);
        let many = Value::Tsvector(tsvector::tsvector_in("'a':1,2,3 'b' 'c' 'd' 'e'")?);
        assert!(many.deep_size_of() > one_lexeme.deep_size_of());

        let leaf = Value::Tsquery(tsquery::tsquery_in("'a'")?);
        let tree = Value::Tsquery(tsquery::tsquery_in("'a' & 'b' & ('c' | !'d')")?);
        assert!(tree.deep_size_of() > leaf.deep_size_of());

        let short_path = Value::Jsonpath(jsonpath::jsonpath_in("$")?);
        let long_path =
            Value::Jsonpath(jsonpath::jsonpath_in("$.a.b.c[*] ? (@.x > 3 && @.y < 4)")?);
        assert!(long_path.deep_size_of() > short_path.deep_size_of());

        let one = Value::Array {
            elem: PgType::Int4,
            elems: vec![Value::Int4(1)],
        };
        let hundred = Value::Array {
            elem: PgType::Int4,
            elems: (0..100).map(Value::Int4).collect(),
        };
        assert!(hundred.deep_size_of() > one.deep_size_of());
        Ok(())
    }

    #[test]
    fn null_encodes_as_none() {
        assert_eq!(Value::Null.encode_text_utc(), None);
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
        assert_eq!(Value::Oid(2200).encode_text_utc().as_deref(), Some("2200"));
        // Past i32::MAX: an oid is unsigned, so it must not print negative.
        assert_eq!(
            Value::Oid(u32::MAX).encode_text_utc().as_deref(),
            Some("4294967295")
        );
        assert_eq!(PgType::Oid.typname(), "oid");
        assert_eq!(PgType::Oid.typlen(), 4);
    }

    #[test]
    fn bytea_hex_encoding() {
        assert_eq!(
            Value::Bytea(vec![0x00, 0x10, 0x00])
                .encode_text_utc()
                .as_deref(),
            Some("\\x001000")
        );
    }
}
