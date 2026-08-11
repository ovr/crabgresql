//! Self-describing durable encoding of a single [`Value`].
//!
//! Each datum begins with a one-byte type tag, so decoding never needs the
//! column's declared type — the null bitmap in the tuple header handles NULLs,
//! and everything stored is exactly the value that was inserted. Fixed-width
//! kinds write their raw little-endian bytes; variable-width kinds write a
//! `u32` length prefix followed by the payload. Floats are stored via `to_bits`
//! so NaN payloads survive a round trip.
//!
//! This lives beside [`Value`] rather than in a storage crate because every
//! durable representation of a value shares it: heap pages, the relation
//! catalog's partition bounds, and the Parquet buffer table's WAL records. One
//! codec means one place to get a type's byte layout right, and it means an
//! access method outside `crabgresql-pg-engine` can persist a `Value` without
//! reimplementing every value kind's encoding.
//!
//! The tag bytes below are an on-disk format shared by all of those. Adding a
//! kind appends a tag; renumbering one silently misreads every existing file.

use crate::{Inet, Interval, Numeric, PgType, Reg, TimeTz, Value, VectorKind, json};

// Type tags. Never reordered — they are an on-disk format.
const T_BOOL: u8 = 1;
const T_INT2: u8 = 2;
const T_INT4: u8 = 3;
const T_INT8: u8 = 4;
const T_FLOAT4: u8 = 5;
const T_FLOAT8: u8 = 6;
const T_NUMERIC: u8 = 7;
const T_TEXT: u8 = 8;
const T_BYTEA: u8 = 9;
const T_BIT: u8 = 10;
const T_DATE: u8 = 11;
const T_TIME: u8 = 12;
const T_TIMETZ: u8 = 13;
const T_TIMESTAMP: u8 = 14;
const T_TIMESTAMPTZ: u8 = 15;
const T_INTERVAL: u8 = 16;
const T_UUID: u8 = 17;
const T_INET: u8 = 18;
const T_CIDR: u8 = 19;
const T_MONEY: u8 = 20;
const T_OID: u8 = 21;
const T_MACADDR: u8 = 22;
const T_MACADDR8: u8 = 23;
const T_POINT: u8 = 24;
const T_LSEG: u8 = 25;
const T_ENUM: u8 = 26;
const T_JSON: u8 = 27;
const T_JSONB: u8 = 28;
const T_JSONPATH: u8 = 29;
const T_ARRAY: u8 = 30;
const T_REG: u8 = 31;
const T_TSVECTOR: u8 = 32;
const T_TSQUERY: u8 = 33;
/// `jsonpath` as a serialized tree. Supersedes [`T_JSONPATH`], whose canonical
/// text had to be re-parsed on read; that tag is still decoded so pages written
/// before the change keep working.
const T_JSONPATH_TREE: u8 = 34;
const T_PATH: u8 = 35;
const T_TID: u8 = 36;
const T_XID: u8 = 37;
const T_XID8: u8 = 38;
const T_PG_LSN: u8 = 39;
const T_BOX: u8 = 40;
const T_LINE: u8 = 41;
const T_CIRCLE: u8 = 42;
const T_POLYGON: u8 = 43;
const T_VECTOR: u8 = 44;
const T_CHAR: u8 = 45;

// Tags at 200 and above are NOT value kinds. They are storage-layer markers that
// an access method resolves before this codec is reached, and they live in their
// own range so the sequential block above stays free for real types — a marker
// sitting in that block collides with the next type someone adds.
/// Reserved for the heap access method's out-of-line ("toasted") attribute
/// pointer. Deliberately **never produced by [`encode_datum`] and never accepted
/// by [`decode_datum`]**: reassembling one needs a buffer pool this module has
/// no access to, and the two other consumers of this codec — the relation
/// catalog's partition bounds and the buffer engine's WAL rows — have no way to
/// resolve it. The heap's tuple codec recognises the tag itself, before handing
/// the remaining bytes here.
///
/// Reserving it here rather than in the heap keeps this tag registry the single
/// place a new on-disk kind is claimed, so nothing can reuse 200 by accident.
pub const T_EXTERNAL: u8 = 200;

fn put_var(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// Encode one non-null value (tag + payload) onto `out`.
pub fn encode_datum(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => unreachable!("nulls are recorded in the tuple bitmap, never encoded"),
        Value::Bool(b) => {
            out.push(T_BOOL);
            out.push(*b as u8);
        }
        // Fixed one-byte payload, not `put_var` — a length prefix would
        // quadruple the on-page size of a `typlen = 1` type.
        Value::Char(c) => {
            out.push(T_CHAR);
            out.push(*c);
        }
        Value::Int2(x) => {
            out.push(T_INT2);
            out.extend_from_slice(&x.to_le_bytes());
        }
        Value::Int4(x) => {
            out.push(T_INT4);
            out.extend_from_slice(&x.to_le_bytes());
        }
        Value::Oid(x) => {
            out.push(T_OID);
            out.extend_from_slice(&x.to_le_bytes());
        }
        Value::Int8(x) => {
            out.push(T_INT8);
            out.extend_from_slice(&x.to_le_bytes());
        }
        Value::Float4(x) => {
            out.push(T_FLOAT4);
            out.extend_from_slice(&x.to_bits().to_le_bytes());
        }
        Value::Float8(x) => {
            out.push(T_FLOAT8);
            out.extend_from_slice(&x.to_bits().to_le_bytes());
        }
        Value::Numeric(n) => {
            out.push(T_NUMERIC);
            put_var(out, n.to_display().as_bytes());
        }
        Value::Text(s) => {
            out.push(T_TEXT);
            put_var(out, s.as_bytes());
        }
        Value::Bytea(b) => {
            out.push(T_BYTEA);
            put_var(out, b);
        }
        Value::Bit { len, data } => {
            out.push(T_BIT);
            out.extend_from_slice(&len.to_le_bytes());
            put_var(out, data);
        }
        Value::Date(d) => {
            out.push(T_DATE);
            out.extend_from_slice(&d.to_le_bytes());
        }
        Value::Time(u) => {
            out.push(T_TIME);
            out.extend_from_slice(&u.to_le_bytes());
        }
        Value::TimeTz(t) => {
            out.push(T_TIMETZ);
            out.extend_from_slice(&t.usec.to_le_bytes());
            out.extend_from_slice(&t.zone.to_le_bytes());
        }
        Value::Timestamp(u) => {
            out.push(T_TIMESTAMP);
            out.extend_from_slice(&u.to_le_bytes());
        }
        Value::TimestampTz(u) => {
            out.push(T_TIMESTAMPTZ);
            out.extend_from_slice(&u.to_le_bytes());
        }
        Value::Interval(iv) => {
            out.push(T_INTERVAL);
            out.extend_from_slice(&iv.months.to_le_bytes());
            out.extend_from_slice(&iv.days.to_le_bytes());
            out.extend_from_slice(&iv.usec.to_le_bytes());
        }
        Value::Uuid(b) => {
            out.push(T_UUID);
            out.extend_from_slice(b);
        }
        Value::Inet(a) => {
            out.push(T_INET);
            put_net(out, a);
        }
        Value::Cidr(a) => {
            out.push(T_CIDR);
            put_net(out, a);
        }
        Value::Money(c) => {
            out.push(T_MONEY);
            out.extend_from_slice(&c.to_le_bytes());
        }
        Value::Tid { block, offset } => {
            out.push(T_TID);
            out.extend_from_slice(&block.to_le_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
        }
        Value::Xid(v) => {
            out.push(T_XID);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Xid8(v) => {
            out.push(T_XID8);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::PgLsn(v) => {
            out.push(T_PG_LSN);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Macaddr(b) => {
            out.push(T_MACADDR);
            out.extend_from_slice(b);
        }
        Value::Macaddr8(b) => {
            out.push(T_MACADDR8);
            out.extend_from_slice(b);
        }
        Value::Point(p) => {
            out.push(T_POINT);
            for c in p {
                out.extend_from_slice(&c.to_bits().to_le_bytes());
            }
        }
        Value::Lseg(l) => {
            out.push(T_LSEG);
            for c in l {
                out.extend_from_slice(&c.to_bits().to_le_bytes());
            }
        }
        // `path` is variable length: the open/closed flag, then the vertex count,
        // then the coordinate pairs.
        Value::Path(p) => {
            out.push(T_PATH);
            out.push(p.closed as u8);
            out.extend_from_slice(&(p.pts.len() as u32).to_le_bytes());
            for pt in &p.pts {
                for c in pt {
                    out.extend_from_slice(&c.to_bits().to_le_bytes());
                }
            }
        }
        Value::Box(b) => {
            out.push(T_BOX);
            for c in b {
                out.extend_from_slice(&c.to_bits().to_le_bytes());
            }
        }
        Value::Line(l) => {
            out.push(T_LINE);
            for c in l {
                out.extend_from_slice(&c.to_bits().to_le_bytes());
            }
        }
        Value::Circle(c) => {
            out.push(T_CIRCLE);
            for v in c {
                out.extend_from_slice(&v.to_bits().to_le_bytes());
            }
        }
        // `polygon` is variable length: the vertex count, then the coordinates.
        Value::Polygon(p) => {
            out.push(T_POLYGON);
            out.extend_from_slice(&(p.pts.len() as u32).to_le_bytes());
            for pt in &p.pts {
                for c in pt {
                    out.extend_from_slice(&c.to_bits().to_le_bytes());
                }
            }
        }
        // A reg* value stores the OID *and* the name it renders as, because the
        // name is resolved when the value is built, not at output time (see
        // `crabgresql_types::Reg`). A row read back therefore shows the name the
        // object had when it was written, which is a documented divergence from
        // PG — PG re-resolves the OID on every output.
        Value::Reg(reg) => {
            out.push(T_REG);
            out.extend_from_slice(&reg.kind.oid().to_le_bytes());
            out.extend_from_slice(&reg.oid.to_le_bytes());
            put_var(out, reg.name.as_bytes());
        }
        Value::Enum {
            type_oid,
            ordinal,
            label,
        } => {
            out.push(T_ENUM);
            out.extend_from_slice(&type_oid.to_le_bytes());
            out.extend_from_slice(&ordinal.to_le_bytes());
            put_var(out, label.as_bytes());
        }
        // `json` stores its raw text; `jsonb` stores its canonical serialization
        // (already deterministic), re-parsed on decode.
        Value::Json(s) => {
            out.push(T_JSON);
            put_var(out, s.as_bytes());
        }
        Value::Jsonb(j) => {
            out.push(T_JSONB);
            put_var(out, json::format(j).as_bytes());
        }
        // `jsonpath` stores its tree, not its canonical text: `jsonpath_out`
        // parenthesizes equal-priority sub-expressions, so re-parsing could
        // exceed the depth limit the original passed, and any tightening of the
        // parser would retroactively make stored values unreadable.
        Value::Jsonpath(p) => {
            out.push(T_JSONPATH_TREE);
            put_var(out, &crate::jsonpath::encode(p));
        }
        // `tsvector` stores its canonical text: the output form round-trips
        // exactly through the input parser.
        Value::Tsvector(v) => {
            out.push(T_TSVECTOR);
            put_var(out, crate::tsvector::format(v).as_bytes());
        }
        // `tsquery` cannot store its text form: `tsquery_out` is lossy for
        // `&`/`|` associativity, so `'1|(2|4)'` would come back as `'1|2|4'`.
        Value::Tsquery(q) => {
            out.push(T_TSQUERY);
            put_var(out, &crate::tsquery::encode(q));
        }
        // A 1-D array: element type OID, element count, then per element a
        // presence byte (0 = NULL, 1 = a self-describing datum). Elements recurse
        // through `encode_datum`, so nested value types work automatically.
        Value::Array { elem, elems } => {
            out.push(T_ARRAY);
            out.extend_from_slice(&elem.oid().to_le_bytes());
            out.extend_from_slice(&(elems.len() as u32).to_le_bytes());
            for e in elems {
                if matches!(e, Value::Null) {
                    out.push(0);
                } else {
                    out.push(1);
                    encode_datum(e, out);
                }
            }
        }
        // An `oidvector`/`int2vector`: kind byte, element count, then the raw
        // fixed-width elements. Unlike an array these need no per-element tag or
        // presence byte — every element is of the kind's own type and none can
        // be NULL, which `vector_in` and the catalog both guarantee.
        Value::Vector { kind, elems } => {
            out.push(T_VECTOR);
            out.push(match kind {
                VectorKind::Oid => 0,
                VectorKind::Int2 => 1,
            });
            out.extend_from_slice(&(elems.len() as u32).to_le_bytes());
            for e in elems {
                match e {
                    Value::Oid(x) => out.extend_from_slice(&x.to_le_bytes()),
                    Value::Int2(x) => out.extend_from_slice(&x.to_le_bytes()),
                    other => unreachable!("vector element is not oid/int2: {other:?}"),
                }
            }
        }
    }
}

fn put_net(out: &mut Vec<u8>, a: &Inet) {
    out.push(a.is_ipv6 as u8);
    out.push(a.bits);
    out.extend_from_slice(&a.addr);
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> &'a [u8] {
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        s
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.array())
    }
    fn i32(&mut self) -> i32 {
        i32::from_le_bytes(self.array())
    }
    fn i64(&mut self) -> i64 {
        i64::from_le_bytes(self.array())
    }
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.array())
    }
    fn var(&mut self) -> &'a [u8] {
        let n = self.u32() as usize;
        self.take(n)
    }
    fn net(&mut self, cidr: bool) -> Value {
        let is_ipv6 = self.take(1)[0] != 0;
        let bits = self.take(1)[0];
        let mut addr = [0u8; 16];
        addr.copy_from_slice(self.take(16));
        let a = Inet {
            is_ipv6,
            addr,
            bits,
        };
        if cidr { Value::Cidr(a) } else { Value::Inet(a) }
    }
    fn array<const N: usize>(&mut self) -> [u8; N] {
        let slice = self.take(N);
        let mut out = [0; N];
        out.copy_from_slice(slice);
        out
    }
}

/// Decode one datum starting at `*pos`, advancing `*pos` past it.
pub fn decode_datum(buf: &[u8], pos: &mut usize) -> Value {
    let mut r = Reader { buf, pos: *pos };
    let tag = r.take(1)[0];
    let v = match tag {
        T_BOOL => Value::Bool(r.take(1)[0] != 0),
        T_CHAR => Value::Char(r.take(1)[0]),
        T_INT2 => Value::Int2(i16::from_le_bytes(r.array())),
        T_INT4 => Value::Int4(r.i32()),
        T_OID => Value::Oid(r.u32()),
        T_INT8 => Value::Int8(r.i64()),
        T_FLOAT4 => Value::Float4(f32::from_bits(r.u32())),
        T_FLOAT8 => Value::Float8(f64::from_bits(r.u64())),
        T_NUMERIC => {
            let s = std::str::from_utf8(r.var()).expect("numeric text is valid utf-8");
            Value::Numeric(Numeric::parse(s).expect("stored numeric re-parses"))
        }
        T_TEXT => Value::Text(String::from_utf8(r.var().to_vec()).expect("text is valid utf-8")),
        T_BYTEA => Value::Bytea(r.var().to_vec()),
        T_BIT => {
            let len = r.u32();
            let data = r.var().to_vec();
            Value::Bit { len, data }
        }
        T_DATE => Value::Date(r.i32()),
        T_TIME => Value::Time(r.i64()),
        T_TIMETZ => {
            let usec = r.i64();
            let zone = r.i32();
            Value::TimeTz(TimeTz { usec, zone })
        }
        T_TIMESTAMP => Value::Timestamp(r.i64()),
        T_TIMESTAMPTZ => Value::TimestampTz(r.i64()),
        T_INTERVAL => {
            let months = r.i32();
            let days = r.i32();
            let usec = r.i64();
            Value::Interval(Interval { months, days, usec })
        }
        T_UUID => {
            let mut b = [0u8; 16];
            b.copy_from_slice(r.take(16));
            Value::Uuid(b)
        }
        T_INET => r.net(false),
        T_CIDR => r.net(true),
        T_MONEY => Value::Money(r.i64()),
        T_TID => {
            let block = r.u32();
            let mut b = [0u8; 2];
            b.copy_from_slice(r.take(2));
            Value::Tid {
                block,
                offset: u16::from_le_bytes(b),
            }
        }
        T_XID => Value::Xid(r.u32()),
        T_XID8 => Value::Xid8(r.u64()),
        T_PG_LSN => Value::PgLsn(r.u64()),
        T_MACADDR => {
            let mut b = [0u8; 6];
            b.copy_from_slice(r.take(6));
            Value::Macaddr(b)
        }
        T_MACADDR8 => {
            let mut b = [0u8; 8];
            b.copy_from_slice(r.take(8));
            Value::Macaddr8(b)
        }
        T_POINT => Value::Point([f64::from_bits(r.u64()), f64::from_bits(r.u64())]),
        T_LSEG => Value::Lseg([
            f64::from_bits(r.u64()),
            f64::from_bits(r.u64()),
            f64::from_bits(r.u64()),
            f64::from_bits(r.u64()),
        ]),
        T_PATH => {
            let closed = r.take(1)[0] != 0;
            let npts = r.u32() as usize;
            // The count comes off the page, so bound it by the bytes actually
            // left before reserving. `(0..npts)` is `TrustedLen`, so `collect`
            // reserves `npts * 16` up front; on a corrupt count that would be a
            // huge allocation (and an abort on failure) instead of the ordinary
            // bounds panic every other varlena decode gets from `take`.
            let need = npts.saturating_mul(16);
            if need > r.buf.len().saturating_sub(r.pos) {
                r.take(need); // diverges: same bounds panic as `var()`
            }
            let pts = (0..npts)
                .map(|_| [f64::from_bits(r.u64()), f64::from_bits(r.u64())])
                .collect();
            Value::Path(crate::geo::PathVal { closed, pts })
        }
        T_BOX => Value::Box([
            f64::from_bits(r.u64()),
            f64::from_bits(r.u64()),
            f64::from_bits(r.u64()),
            f64::from_bits(r.u64()),
        ]),
        T_LINE => Value::Line([
            f64::from_bits(r.u64()),
            f64::from_bits(r.u64()),
            f64::from_bits(r.u64()),
        ]),
        T_CIRCLE => Value::Circle([
            f64::from_bits(r.u64()),
            f64::from_bits(r.u64()),
            f64::from_bits(r.u64()),
        ]),
        T_POLYGON => {
            // Same bound-the-count-first guard as `T_PATH` above.
            let npts = r.u32() as usize;
            let need = npts.saturating_mul(16);
            if need > r.buf.len().saturating_sub(r.pos) {
                r.take(need); // diverges: same bounds panic as `var()`
            }
            let pts = (0..npts)
                .map(|_| [f64::from_bits(r.u64()), f64::from_bits(r.u64())])
                .collect();
            Value::Polygon(crate::geo::PolygonVal { pts })
        }
        T_REG => {
            let kind_oid = r.u32();
            let PgType::Reg(kind) =
                PgType::from_oid(kind_oid).expect("stored reg* type re-resolves")
            else {
                panic!("reg datum tagged with a non-reg type oid {kind_oid}")
            };
            let oid = r.u32();
            let name = String::from_utf8(r.var().to_vec()).expect("reg name is valid utf-8");
            Value::Reg(Reg { kind, oid, name })
        }
        T_ENUM => {
            let type_oid = r.u32();
            let ordinal = r.u32();
            let label = String::from_utf8(r.var().to_vec()).expect("enum label is valid utf-8");
            Value::Enum {
                type_oid,
                ordinal,
                label,
            }
        }
        T_JSON => {
            Value::Json(String::from_utf8(r.var().to_vec()).expect("json text is valid utf-8"))
        }
        T_JSONB => {
            let s = std::str::from_utf8(r.var()).expect("jsonb text is valid utf-8");
            Value::Jsonb(json::jsonb_in(s).expect("stored jsonb re-parses"))
        }
        T_JSONPATH_TREE => {
            Value::Jsonpath(crate::jsonpath::decode(r.var()).expect("stored jsonpath decodes"))
        }
        // Written by builds before the tree encoding existed.
        T_JSONPATH => {
            let s = std::str::from_utf8(r.var()).expect("jsonpath text is valid utf-8");
            Value::Jsonpath(crate::jsonpath::jsonpath_in(s).expect("stored jsonpath re-parses"))
        }
        T_TSVECTOR => {
            let s = std::str::from_utf8(r.var()).expect("tsvector text is valid utf-8");
            Value::Tsvector(crate::tsvector::tsvector_in(s).expect("stored tsvector re-parses"))
        }
        T_TSQUERY => {
            Value::Tsquery(crate::tsquery::decode(r.var()).expect("stored tsquery decodes"))
        }
        T_ARRAY => {
            let elem_oid = r.u32();
            let elem = PgType::from_oid(elem_oid).expect("stored array element type re-resolves");
            let count = r.u32() as usize;
            // Each element occupies at least one byte (its presence flag), so a
            // count larger than the bytes remaining is corrupt; cap the up-front
            // reservation to avoid a huge allocation on a bad page (the loop still
            // fails loudly via `take` if the data is actually short).
            let remaining = buf.len().saturating_sub(r.pos);
            let mut elems = Vec::with_capacity(count.min(remaining));
            for _ in 0..count {
                if r.take(1)[0] == 0 {
                    elems.push(Value::Null);
                } else {
                    elems.push(decode_datum(buf, &mut r.pos));
                }
            }
            Value::Array { elem, elems }
        }
        T_VECTOR => {
            let kind = match r.take(1)[0] {
                0 => VectorKind::Oid,
                1 => VectorKind::Int2,
                other => panic!("corrupt vector kind {other}"),
            };
            let count = r.u32() as usize;
            let width = kind.element().typlen() as usize;
            // Cap the up-front reservation at what the remaining bytes could
            // possibly hold, so a corrupt count cannot request a huge
            // allocation; `take` still fails loudly if the data is short.
            let remaining = (buf.len().saturating_sub(r.pos)) / width;
            let mut elems = Vec::with_capacity(count.min(remaining));
            for _ in 0..count {
                elems.push(match kind {
                    VectorKind::Oid => Value::Oid(r.u32()),
                    VectorKind::Int2 => Value::Int2(i16::from_le_bytes(r.array())),
                });
            }
            Value::Vector { kind, elems }
        }
        other => panic!("corrupt datum tag {other}"),
    };
    *pos = r.pos;
    v
}

/// Advance `*pos` past one datum without building its [`Value`].
///
/// The point is what it does *not* do: no `String`, no `Vec`, no re-parse of a
/// `numeric`/`jsonb`/`tsvector` payload. A scan that was told it needs two of
/// forty columns walks the other thirty-eight through here instead of decoding
/// them (see `TableAm::scan`'s column-projection contract).
///
/// Reads through the same `Reader` as [`decode_datum`], so a truncated or
/// corrupt datum fails identically: the same slice bounds panic, the same
/// `corrupt datum tag` message. That matters because both run under a heap
/// page's frame lock, where the two must not diverge in *how* they fail.
pub fn skip_datum(buf: &[u8], pos: &mut usize) {
    let start = *pos;
    let mut r = Reader { buf, pos: start };
    let tag = r.take(1)[0];
    match tag {
        // Fixed-width payloads, grouped by their byte count. This table is the
        // one place that knows a kind's width without decoding it, so a kind
        // added to `encode_datum` and forgotten here would walk the tuple off
        // by its own length — which is what the debug cross-check at the bottom
        // exists to catch, on every kind the test suite ever stores.
        T_BOOL | T_CHAR => {
            r.take(1);
        }
        T_INT2 => {
            r.take(2);
        }
        T_INT4 | T_OID | T_FLOAT4 | T_DATE | T_XID => {
            r.take(4);
        }
        T_TID | T_MACADDR => {
            r.take(6);
        }
        T_INT8 | T_FLOAT8 | T_TIME | T_TIMESTAMP | T_TIMESTAMPTZ | T_MONEY | T_XID8 | T_PG_LSN
        | T_MACADDR8 => {
            r.take(8);
        }
        T_TIMETZ => {
            r.take(12);
        }
        T_UUID | T_INTERVAL | T_POINT => {
            r.take(16);
        }
        T_INET | T_CIDR => {
            r.take(18);
        }
        T_LINE | T_CIRCLE => {
            r.take(24);
        }
        T_LSEG | T_BOX => {
            r.take(32);
        }
        // A `u32` length prefix and its payload: `var` reads the length and
        // slices past the bytes without copying them.
        T_NUMERIC | T_TEXT | T_BYTEA | T_JSON | T_JSONB | T_JSONPATH_TREE | T_JSONPATH
        | T_TSVECTOR | T_TSQUERY => {
            r.var();
        }
        // A bit length, then the packed bytes.
        T_BIT => {
            r.take(4);
            r.var();
        }
        // Two OIDs (kind + value, or type + ordinal), then the rendered name.
        T_REG | T_ENUM => {
            r.take(8);
            r.var();
        }
        // The open/closed flag, then the vertex count and its coordinate pairs.
        T_PATH => {
            r.take(1);
            skip_points(&mut r);
        }
        T_POLYGON => skip_points(&mut r),
        T_VECTOR => {
            let width = match r.take(1)[0] {
                0 => VectorKind::Oid,
                1 => VectorKind::Int2,
                other => panic!("corrupt vector kind {other}"),
            }
            .element()
            .typlen() as usize;
            let count = r.u32() as usize;
            r.take(count.saturating_mul(width));
        }
        // Elements are self-describing, so skipping one is this same walk a
        // level down — the recursion mirrors `encode_datum`'s.
        T_ARRAY => {
            r.take(4);
            let count = r.u32();
            for _ in 0..count {
                if r.take(1)[0] != 0 {
                    skip_datum(buf, &mut r.pos);
                }
            }
        }
        other => panic!("corrupt datum tag {other}"),
    }

    // Debug builds pay for a full decode and check the walk against it. Every
    // kind the test suite and the regression corpus ever store is therefore
    // checked against the codec itself, rather than against a hand-written list
    // of expected widths that would need its own maintenance.
    #[cfg(debug_assertions)]
    {
        let mut decoded = start;
        decode_datum(buf, &mut decoded);
        debug_assert_eq!(
            decoded, r.pos,
            "skip_datum disagrees with decode_datum on tag {tag}"
        );
    }

    *pos = r.pos;
}

/// Skip a vertex count and the `count * 16` bytes of coordinates behind it.
///
/// `saturating_mul` and a single `take` keep a corrupt count to the ordinary
/// bounds panic, exactly as the decoding side's explicit guard does.
fn skip_points(r: &mut Reader<'_>) {
    let npts = r.u32() as usize;
    r.take(npts.saturating_mul(16));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RegKind;

    /// A corrupt vertex count must fail the ordinary bounds check rather than
    /// trying to reserve `npts * 16` bytes (which for a bogus count is a huge
    /// allocation, and an abort rather than a panic if it fails).
    #[test]
    #[should_panic(expected = "range end index")]
    fn path_decode_rejects_a_bogus_vertex_count() {
        let mut buf = Vec::new();
        encode_datum(
            &Value::Path(crate::geo::PathVal {
                closed: false,
                pts: vec![[1.0, 2.0], [3.0, 4.0]],
            }),
            &mut buf,
        );
        // Overwrite the count (tag byte + closed flag) with 0xFFFFFFFF.
        buf[2..6].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut pos = 0;
        decode_datum(&buf, &mut pos);
    }

    /// The same corrupt-count guard as `path`, for the other varlena geometric
    /// type. `polygon` has no open/closed flag, so the count starts one byte
    /// earlier.
    #[test]
    #[should_panic(expected = "range end index")]
    fn polygon_decode_rejects_a_bogus_vertex_count() {
        let mut buf = Vec::new();
        encode_datum(
            &Value::Polygon(crate::geo::PolygonVal {
                pts: vec![[1.0, 2.0], [3.0, 4.0]],
            }),
            &mut buf,
        );
        // Overwrite the count (which follows the tag byte) with 0xFFFFFFFF.
        buf[1..5].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut pos = 0;
        decode_datum(&buf, &mut pos);
    }

    fn roundtrip(v: Value) {
        let mut buf = Vec::new();
        encode_datum(&v, &mut buf);
        let mut pos = 0;
        let got = decode_datum(&buf, &mut pos);
        assert_eq!(pos, buf.len(), "consumed exactly the datum");
        assert_eq!(got, v);

        // Skipping must land where decoding did. Asserted here rather than in a
        // test of its own so that every kind listed below — and every kind
        // anyone adds to that list later — checks its own width for free.
        let mut skipped = 0;
        skip_datum(&buf, &mut skipped);
        assert_eq!(skipped, buf.len(), "skipped exactly the datum: {v:?}");
    }

    /// A skipped datum must leave the position the *next* one starts at. Landing
    /// one byte off is invisible to a single-datum check — the length still
    /// matches the buffer — and corrupts every attribute after it in a tuple.
    #[test]
    fn skipping_lands_on_the_next_datum() {
        let values = [
            Value::Text("skip me".into()),
            Value::Int4(-7),
            Value::Bit {
                len: 12,
                data: vec![0xAB, 0xC0],
            },
            Value::Array {
                elem: PgType::Text,
                elems: vec![Value::Text("x".into()), Value::Null],
            },
            Value::Path(crate::geo::PathVal {
                closed: true,
                pts: vec![[1.0, 2.0], [3.0, 4.0]],
            }),
            Value::Vector {
                kind: VectorKind::Int2,
                elems: vec![Value::Int2(3), Value::Int2(4)],
            },
            Value::Numeric(Numeric::parse("1.2300").expect("valid numeric")),
        ];
        let mut buf = Vec::new();
        for v in &values {
            encode_datum(v, &mut buf);
        }
        // Skip the first `n`, then decode the rest and require them to be the
        // values that were written.
        for n in 0..values.len() {
            let mut pos = 0;
            for _ in 0..n {
                skip_datum(&buf, &mut pos);
            }
            for want in &values[n..] {
                assert_eq!(&decode_datum(&buf, &mut pos), want, "after skipping {n}");
            }
            assert_eq!(pos, buf.len());
        }
    }

    /// `jsonpath` written before the tree encoding existed is stored under its
    /// own tag, which `encode_datum` no longer produces — so `roundtrip` cannot
    /// reach it and the skip table would go unchecked for that tag.
    #[test]
    fn a_legacy_jsonpath_datum_skips() {
        let mut buf = vec![T_JSONPATH];
        put_var(&mut buf, b"$.a[*]");
        let mut pos = 0;
        skip_datum(&buf, &mut pos);
        assert_eq!(pos, buf.len());
    }

    /// A corrupt vertex count is a bounds panic when skipping too, not a
    /// `npts * 16` allocation — the guard the decoding side spells out.
    #[test]
    #[should_panic(expected = "range end index")]
    fn skipping_a_polygon_rejects_a_bogus_vertex_count() {
        let mut buf = Vec::new();
        encode_datum(
            &Value::Polygon(crate::geo::PolygonVal {
                pts: vec![[1.0, 2.0]],
            }),
            &mut buf,
        );
        buf[1..5].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut pos = 0;
        skip_datum(&buf, &mut pos);
    }

    /// `Reg`'s `PartialEq` compares only `(kind, oid)`, so `roundtrip`'s
    /// assertion cannot see a lost or corrupted name. Check the rendered text
    /// explicitly — losing it would make a stored `regclass` column read back as
    /// bare digits.
    #[test]
    fn reg_datum_keeps_its_rendered_name() {
        let mut buf = Vec::new();
        encode_datum(
            &Value::Reg(Reg {
                kind: RegKind::Class,
                oid: 1259,
                name: "rs.\"Mixed Case\"".into(),
            }),
            &mut buf,
        );
        let mut pos = 0;
        let Value::Reg(got) = decode_datum(&buf, &mut pos) else {
            panic!("decoded a non-reg value");
        };
        assert_eq!(got.kind, RegKind::Class);
        assert_eq!(got.oid, 1259);
        assert_eq!(got.name, "rs.\"Mixed Case\"");
    }

    #[test]
    fn all_variants_roundtrip() {
        // A reg* value carries its rendered name through the page, including the
        // `-`/digits renderings an unresolved OID gets.
        roundtrip(Value::Reg(Reg {
            kind: RegKind::Class,
            oid: 1259,
            name: "pg_class".into(),
        }));
        roundtrip(Value::Reg(Reg {
            kind: RegKind::Type,
            oid: 23,
            name: "integer".into(),
        }));
        roundtrip(Value::Reg(Reg::unresolved(RegKind::Namespace, 0)));
        roundtrip(Value::Reg(Reg::unresolved(RegKind::Class, 999_999)));
        roundtrip(Value::Bool(true));
        roundtrip(Value::Bool(false));
        roundtrip(Value::Char(b'a'));
        // The two values a text-backed representation could not carry, and so
        // the ones that prove `T_CHAR` is not quietly a `T_TEXT`.
        roundtrip(Value::Char(0));
        roundtrip(Value::Char(0xFF));
        roundtrip(Value::Int2(i16::MIN));
        roundtrip(Value::Int4(i32::MAX));
        roundtrip(Value::Int8(i64::MIN));
        roundtrip(Value::Float4(0.0));
        roundtrip(Value::Float8(-1.5));
        roundtrip(Value::Text(String::new()));
        roundtrip(Value::Text("héllo world".into()));
        roundtrip(Value::Bytea(vec![0, 1, 2, 255]));
        roundtrip(Value::Bit {
            len: 8,
            data: vec![0b1010_1010],
        });
        roundtrip(Value::Bit {
            len: 0,
            data: vec![],
        });
        roundtrip(Value::Bit {
            len: 1000,
            data: vec![0xA5; 125],
        });
        roundtrip(Value::Date(-5));
        roundtrip(Value::Time(86_399_000_000));
        roundtrip(Value::TimeTz(TimeTz {
            usec: 1,
            zone: -3600,
        }));
        roundtrip(Value::Timestamp(0));
        roundtrip(Value::TimestampTz(i64::MAX));
        roundtrip(Value::Interval(Interval {
            months: -13,
            days: 2,
            usec: -999,
        }));
        roundtrip(Value::Uuid([9u8; 16]));
        roundtrip(Value::Money(i64::MIN));
        roundtrip(Value::Money(12345));
        roundtrip(Value::Tid {
            block: 0,
            offset: 0,
        });
        roundtrip(Value::Tid {
            block: u32::MAX,
            offset: u16::MAX,
        });
        roundtrip(Value::Xid(0));
        roundtrip(Value::Xid(u32::MAX));
        roundtrip(Value::Xid8(u64::MAX));
        roundtrip(Value::PgLsn(0));
        roundtrip(Value::PgLsn(u64::MAX));
        roundtrip(Value::Macaddr([0x08, 0x00, 0x2b, 0x01, 0x02, 0x03]));
        roundtrip(Value::Macaddr8([
            0x08, 0x00, 0x2b, 0xff, 0xfe, 0x01, 0x02, 0x03,
        ]));
        roundtrip(Value::Point([5.1, 34.5]));
        roundtrip(Value::Point([f64::INFINITY, -1e300]));
        roundtrip(Value::Lseg([1.0, 2.0, 3.0, 4.0]));
        roundtrip(Value::Lseg([-1e6, 200.0, 3e5, -40.0]));
        roundtrip(Value::Path(crate::geo::PathVal {
            closed: false,
            pts: vec![[1.0, 2.0], [3.0, 4.0]],
        }));
        roundtrip(Value::Path(crate::geo::PathVal {
            closed: true,
            pts: vec![[0.0, 0.0], [3.0, 0.0], [4.0, 5.0], [1.0, 6.0]],
        }));
        roundtrip(Value::Path(crate::geo::PathVal {
            closed: true,
            pts: vec![[10.0, 20.0]],
        }));
        roundtrip(Value::Box([2.0, 2.0, 0.0, 0.0]));
        roundtrip(Value::Box([f64::INFINITY, 1.0, -1e300, f64::NEG_INFINITY]));
        roundtrip(Value::Line([1.0, -1.0, 0.0]));
        // A NaN coefficient (which `line` input accepts) cannot go through
        // `roundtrip`: `Value`'s `PartialEq` is IEEE, so `NaN != NaN`.
        roundtrip(Value::Line([-0.4, -1.0, -6.0]));
        roundtrip(Value::Circle([5.0, 1.0, 3.0]));
        roundtrip(Value::Circle([-1e300, 0.0, 0.0]));
        roundtrip(Value::Polygon(crate::geo::PolygonVal {
            pts: vec![[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0]],
        }));
        roundtrip(Value::Polygon(crate::geo::PolygonVal {
            pts: vec![[7.0, 8.0]],
        }));
        roundtrip(Value::Enum {
            type_oid: 16384,
            ordinal: 0,
            label: "red".into(),
        });
        roundtrip(Value::Enum {
            type_oid: 99999,
            ordinal: 42,
            label: String::new(),
        });
        // `json` keeps its raw text; `jsonb` its canonical tree.
        roundtrip(Value::Json("{\"b\": 1,  \"a\": 2}".into()));
        roundtrip(Value::Json("null".into()));
        roundtrip(
            json::jsonb_in("{\"b\":1,\"a\":[1,2,3],\"k\":\"v\"}")
                .map(Value::Jsonb)
                .expect("valid jsonb"),
        );
        roundtrip(json::jsonb_in("[]").map(Value::Jsonb).expect("valid jsonb"));
        roundtrip(
            json::jsonb_in("1.50")
                .map(Value::Jsonb)
                .expect("valid jsonb"),
        );
        // `jsonpath` stores its parsed tree, not its canonical text form.
        roundtrip(
            crate::jsonpath::jsonpath_in("$.a[*] ? (@ > 3)")
                .map(Value::Jsonpath)
                .expect("valid jsonpath"),
        );
        // `tsvector` stores its canonical text form. The escaped lexeme and the
        // weighted positions exercise the parts of that form most likely to
        // lose information on a round trip.
        for tv in ["'a':1A,3B 'b' 'c':16383", r"'ab\\c' 'x''y'", ""] {
            roundtrip(
                crate::tsvector::tsvector_in(tv)
                    .map(Value::Tsvector)
                    .expect("valid tsvector"),
            );
        }
        // The last two print identically but are distinct values, so they pin
        // that storage keeps the tree shape rather than the canonical text.
        for tq in [
            "'a':*AB <2> ( 'b' | !'c' )",
            "!!'x'",
            "",
            "1|2|4",
            "1|(2|4)",
        ] {
            roundtrip(
                crate::tsquery::tsquery_in(tq)
                    .map(Value::Tsquery)
                    .expect("valid tsquery"),
            );
        }
        // Arrays: 1-D, empty, NULL elements, and a text element type.
        roundtrip(Value::Array {
            elem: PgType::Int4,
            elems: vec![Value::Int4(1), Value::Int4(2), Value::Int4(3)],
        });
        roundtrip(Value::Array {
            elem: PgType::Int4,
            elems: vec![],
        });
        roundtrip(Value::Array {
            elem: PgType::Int8,
            elems: vec![Value::Int8(10), Value::Null, Value::Int8(-30)],
        });
        roundtrip(Value::Array {
            elem: PgType::Text,
            elems: vec![Value::Text("a".into()), Value::Text("b,c".into())],
        });
        roundtrip(Value::Vector {
            kind: VectorKind::Oid,
            elems: vec![Value::Oid(23), Value::Oid(u32::MAX)],
        });
        roundtrip(Value::Vector {
            kind: VectorKind::Oid,
            elems: vec![],
        });
        roundtrip(Value::Vector {
            kind: VectorKind::Int2,
            elems: vec![Value::Int2(1), Value::Int2(-32768), Value::Int2(32767)],
        });
    }

    #[test]
    fn float_nan_bits_survive() {
        let mut buf = Vec::new();
        encode_datum(&Value::Float8(f64::NAN), &mut buf);
        let mut pos = 0;
        match decode_datum(&buf, &mut pos) {
            Value::Float8(x) => assert!(x.is_nan()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn numeric_edge_cases_roundtrip() -> anyhow::Result<()> {
        for s in [
            "0",
            "-0.00",
            "123.4500",
            "NaN",
            "Infinity",
            "-Infinity",
            "1e-40",
        ] {
            roundtrip(Value::Numeric(Numeric::parse(s)?));
        }

        Ok(())
    }

    #[test]
    fn inet_and_cidr_v4_v6() {
        let v4 = Inet {
            is_ipv6: false,
            addr: {
                let mut a = [0u8; 16];
                a[0] = 192;
                a[1] = 168;
                a
            },
            bits: 24,
        };
        let v6 = Inet {
            is_ipv6: true,
            addr: [0xab; 16],
            bits: 64,
        };
        roundtrip(Value::Inet(v4.clone()));
        roundtrip(Value::Cidr(v6.clone()));
    }
}
