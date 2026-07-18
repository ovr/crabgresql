//! On-page tuple: a fixed 36-byte header carrying the MVCC [`TupleHeader`] and
//! the forward `ctid`, an optional null bitmap, then the self-describing datums.
//!
//! The MVCC fields (`xmin`/`xmax`/`cmin`/`cmax`/`infomask`) live here on the page
//! exactly as the memory engine keeps them beside the tuple, so visibility uses
//! the same shared [`satisfies_mvcc`](crabgresql_txn::satisfies_mvcc) rule. XIDs
//! are stored full-width (64-bit) as `crabgresql-txn` models them; truncation to
//! 32 bits happens only when projecting SQL system columns, never on disk.

use crabgresql_storage_api::{Tid, Tuple};
use crabgresql_txn::{CommandId, Infomask, TupleHeader, Xid};

use crate::datum::{decode_datum, encode_datum};

pub const TUPLE_HEADER_LEN: usize = 36;

const OFF_XMIN: usize = 0;
const OFF_XMAX: usize = 8;
const OFF_CMIN: usize = 16;
const OFF_CMAX: usize = 20;
const OFF_INFOMASK: usize = 24;
const OFF_INFOMASK2: usize = 26;
const OFF_CTID_BLOCK: usize = 28;
const OFF_CTID_OFF: usize = 32;

const HAS_NULL: u16 = 0x8000;
const NATTS_MASK: u16 = 0x07ff;

/// The header portion decoded from an on-page tuple.
pub struct OnPageHeader {
    pub hdr: TupleHeader,
    /// Forward link to the newest version (self when this is the newest). Read
    /// by update-chain following; retained on every decode.
    #[allow(dead_code)]
    pub ctid: Tid,
    pub natts: u16,
    pub has_null: bool,
}

fn wr_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn wr_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn rd_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(buf[off..off + 2].try_into().unwrap())
}
fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}
fn rd_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

fn bitmap_len(natts: usize) -> usize {
    natts.div_ceil(8)
}

/// Serialize a full tuple: header + (optional) null bitmap + non-null datums.
pub fn encode_tuple(vals: &[Value], hdr: &TupleHeader, ctid: Tid) -> Vec<u8> {
    let natts = vals.len();
    let has_null = vals.iter().any(|v| matches!(v, Value::Null));
    let bmlen = if has_null { bitmap_len(natts) } else { 0 };

    let mut buf = vec![0u8; TUPLE_HEADER_LEN + bmlen];
    wr_u64(&mut buf, OFF_XMIN, hdr.xmin.0);
    wr_u64(&mut buf, OFF_XMAX, hdr.xmax.0);
    wr_u32(&mut buf, OFF_CMIN, hdr.cmin.0);
    wr_u32(&mut buf, OFF_CMAX, hdr.cmax.0);
    buf[OFF_INFOMASK..OFF_INFOMASK + 2].copy_from_slice(&hdr.infomask.0.to_le_bytes());
    let mut infomask2 = (natts as u16) & NATTS_MASK;
    if has_null {
        infomask2 |= HAS_NULL;
    }
    buf[OFF_INFOMASK2..OFF_INFOMASK2 + 2].copy_from_slice(&infomask2.to_le_bytes());
    wr_u32(&mut buf, OFF_CTID_BLOCK, ctid.block);
    buf[OFF_CTID_OFF..OFF_CTID_OFF + 2].copy_from_slice(&ctid.offset.to_le_bytes());

    // Null bitmap: a set bit means the column is NON-null (PostgreSQL convention).
    if has_null {
        for (i, v) in vals.iter().enumerate() {
            if !matches!(v, Value::Null) {
                buf[TUPLE_HEADER_LEN + i / 8] |= 1 << (i % 8);
            }
        }
    }
    for v in vals {
        if !matches!(v, Value::Null) {
            encode_datum(v, &mut buf);
        }
    }
    buf
}

/// Decode just the header — cheap enough to run the visibility check before
/// deciding whether to decode the datums.
pub fn decode_header(buf: &[u8]) -> OnPageHeader {
    let hdr = TupleHeader {
        xmin: Xid(rd_u64(buf, OFF_XMIN)),
        xmax: Xid(rd_u64(buf, OFF_XMAX)),
        cmin: CommandId(rd_u32(buf, OFF_CMIN)),
        cmax: CommandId(rd_u32(buf, OFF_CMAX)),
        infomask: Infomask(rd_u16(buf, OFF_INFOMASK)),
    };
    let infomask2 = rd_u16(buf, OFF_INFOMASK2);
    let ctid = Tid { block: rd_u32(buf, OFF_CTID_BLOCK), offset: rd_u16(buf, OFF_CTID_OFF) };
    OnPageHeader {
        hdr,
        ctid,
        natts: infomask2 & NATTS_MASK,
        has_null: infomask2 & HAS_NULL != 0,
    }
}

/// Decode the full tuple: header plus the column values in schema order.
pub fn decode_tuple(buf: &[u8]) -> (OnPageHeader, Tuple) {
    let head = decode_header(buf);
    let natts = head.natts as usize;
    let bmlen = if head.has_null { bitmap_len(natts) } else { 0 };
    let bitmap = &buf[TUPLE_HEADER_LEN..TUPLE_HEADER_LEN + bmlen];
    let mut pos = TUPLE_HEADER_LEN + bmlen;
    let mut vals = Vec::with_capacity(natts);
    for i in 0..natts {
        let is_null = head.has_null && (bitmap[i / 8] & (1 << (i % 8))) == 0;
        if is_null {
            vals.push(Value::Null);
        } else {
            vals.push(decode_datum(buf, &mut pos));
        }
    }
    (head, vals)
}

/// Stamp a delete onto an existing on-page tuple in place (same length): set the
/// deleting transaction and command. Used by delete/update and by redo.
pub fn stamp_xmax(buf: &mut [u8], xmax: Xid, cmax: CommandId) {
    wr_u64(buf, OFF_XMAX, xmax.0);
    wr_u32(buf, OFF_CMAX, cmax.0);
}

/// Rewrite the forward `ctid` (the newest-version link) in place.
pub fn set_ctid(buf: &mut [u8], ctid: Tid) {
    wr_u32(buf, OFF_CTID_BLOCK, ctid.block);
    buf[OFF_CTID_OFF..OFF_CTID_OFF + 2].copy_from_slice(&ctid.offset.to_le_bytes());
}

use crabgresql_types::Value;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_with_and_without_nulls() {
        let hdr = TupleHeader::inserted(Xid(7), CommandId(2));
        let ctid = Tid { block: 3, offset: 9 };
        for vals in [
            vec![Value::Int4(1), Value::Text("x".into()), Value::Bool(true)],
            vec![Value::Null, Value::Text("y".into()), Value::Null],
            vec![Value::Null, Value::Null],
        ] {
            let buf = encode_tuple(&vals, &hdr, ctid);
            let (head, got) = decode_tuple(&buf);
            assert_eq!(head.natts as usize, vals.len());
            assert_eq!(head.hdr.xmin, Xid(7));
            assert_eq!(head.ctid, ctid);
            assert_eq!(got, vals);
        }
    }

    #[test]
    fn stamp_xmax_and_set_ctid_in_place() {
        let hdr = TupleHeader::inserted(Xid(7), CommandId(0));
        let mut buf = encode_tuple(&[Value::Int4(1)], &hdr, Tid { block: 0, offset: 1 });
        stamp_xmax(&mut buf, Xid(8), CommandId(4));
        set_ctid(&mut buf, Tid { block: 2, offset: 5 });
        let head = decode_header(&buf);
        assert_eq!(head.hdr.xmax, Xid(8));
        assert_eq!(head.hdr.cmax, CommandId(4));
        assert_eq!(head.ctid, Tid { block: 2, offset: 5 });
        // The datum still decodes after the in-place edits.
        let (_, vals) = decode_tuple(&buf);
        assert_eq!(vals, vec![Value::Int4(1)]);
    }
}
