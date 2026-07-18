//! Self-describing on-page encoding of a single [`Value`].
//!
//! Each datum begins with a one-byte type tag, so decoding never needs the
//! column's declared type — the null bitmap in the tuple header handles NULLs,
//! and everything stored is exactly the value that was inserted. Fixed-width
//! kinds write their raw little-endian bytes; variable-width kinds write a
//! `u32` length prefix followed by the payload. Floats are stored via `to_bits`
//! so NaN payloads survive a round trip.

use crabgresql_types::{Inet, Interval, Numeric, TimeTz, Value};

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
        Value::Bit { len, bits } => {
            out.push(T_BIT);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&bits.to_le_bytes());
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
    fn u16(&mut self) -> u16 {
        u16::from_le_bytes(self.take(2).try_into().unwrap())
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take(4).try_into().unwrap())
    }
    fn i32(&mut self) -> i32 {
        i32::from_le_bytes(self.take(4).try_into().unwrap())
    }
    fn i64(&mut self) -> i64 {
        i64::from_le_bytes(self.take(8).try_into().unwrap())
    }
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take(8).try_into().unwrap())
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
        let a = Inet { is_ipv6, addr, bits };
        if cidr { Value::Cidr(a) } else { Value::Inet(a) }
    }
}

/// Decode one datum starting at `*pos`, advancing `*pos` past it.
pub fn decode_datum(buf: &[u8], pos: &mut usize) -> Value {
    let mut r = Reader { buf, pos: *pos };
    let tag = r.take(1)[0];
    let v = match tag {
        T_BOOL => Value::Bool(r.take(1)[0] != 0),
        T_INT2 => Value::Int2(i16::from_le_bytes(r.take(2).try_into().unwrap())),
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
            let len = r.u16();
            let bits = r.u64();
            Value::Bit { len, bits }
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
        other => panic!("corrupt datum tag {other}"),
    };
    *pos = r.pos;
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: Value) {
        let mut buf = Vec::new();
        encode_datum(&v, &mut buf);
        let mut pos = 0;
        let got = decode_datum(&buf, &mut pos);
        assert_eq!(pos, buf.len(), "consumed exactly the datum");
        assert_eq!(got, v);
    }

    #[test]
    fn all_variants_roundtrip() {
        roundtrip(Value::Bool(true));
        roundtrip(Value::Bool(false));
        roundtrip(Value::Int2(i16::MIN));
        roundtrip(Value::Int4(i32::MAX));
        roundtrip(Value::Int8(i64::MIN));
        roundtrip(Value::Float4(0.0));
        roundtrip(Value::Float8(-1.5));
        roundtrip(Value::Text(String::new()));
        roundtrip(Value::Text("héllo world".into()));
        roundtrip(Value::Bytea(vec![0, 1, 2, 255]));
        roundtrip(Value::Bit { len: 8, bits: 0b1010_1010 });
        roundtrip(Value::Date(-5));
        roundtrip(Value::Time(86_399_000_000));
        roundtrip(Value::TimeTz(TimeTz { usec: 1, zone: -3600 }));
        roundtrip(Value::Timestamp(0));
        roundtrip(Value::TimestampTz(i64::MAX));
        roundtrip(Value::Interval(Interval { months: -13, days: 2, usec: -999 }));
        roundtrip(Value::Uuid([9u8; 16]));
        roundtrip(Value::Money(i64::MIN));
        roundtrip(Value::Money(12345));
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
    fn numeric_edge_cases_roundtrip() {
        for s in ["0", "-0.00", "123.4500", "NaN", "Infinity", "-Infinity", "1e-40"] {
            roundtrip(Value::Numeric(Numeric::parse(s).unwrap()));
        }
    }

    #[test]
    fn inet_and_cidr_v4_v6() {
        let v4 = Inet { is_ipv6: false, addr: { let mut a = [0u8; 16]; a[0] = 192; a[1] = 168; a }, bits: 24 };
        let v6 = Inet { is_ipv6: true, addr: [0xab; 16], bits: 64 };
        roundtrip(Value::Inet(v4.clone()));
        roundtrip(Value::Cidr(v6.clone()));
    }
}
