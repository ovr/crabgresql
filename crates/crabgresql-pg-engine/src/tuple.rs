//! On-page tuple: a fixed 36-byte header carrying the MVCC [`TupleHeader`] and
//! the forward `ctid`, an optional null bitmap, then the self-describing datums.
//!
//! The MVCC fields (`xmin`/`xmax`/`cmin`/`cmax`/`infomask`) live here on the page
//! exactly as the memory engine keeps them beside the tuple, so visibility uses
//! the same shared [`satisfies_mvcc`](crabgresql_txn::satisfies_mvcc) rule. XIDs
//! are stored full-width (64-bit) as `crabgresql-txn` models them; truncation to
//! 32 bits happens only when projecting SQL system columns, never on disk.

use crabgresql_storage_api::{StorageError, Tid, Tuple};
use crabgresql_txn::{CommandId, Infomask, TupleHeader, Xid};

use crabgresql_types::datum::{T_EXTERNAL, decode_datum, encode_datum};

use crate::toast::{self, ToastPointer};

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
/// At least one attribute is stored out of line (see [`crate::toast`]).
///
/// A cheap gate, not a locator: it lets the scan hot path and VACUUM skip
/// per-attribute work entirely for the tuples — almost all of them — that own no
/// chunks. What actually locates a pointer is its datum tag, which is
/// unambiguous because the value codec never emits that tag itself.
const HAS_EXTERNAL: u16 = 0x4000;

fn bytes<const N: usize>(buf: &[u8], off: usize) -> [u8; N] {
    let Some(slice) = buf.get(off..off + N) else {
        panic!("tuple field is out of bounds");
    };
    let mut out = [0; N];
    out.copy_from_slice(slice);
    out
}

/// The header portion decoded from an on-page tuple.
pub struct OnPageHeader {
    pub hdr: TupleHeader,
    /// Forward link to the newest version. Every heap version is placed
    /// self-linked and an UPDATE leaves the old one that way, so no reader
    /// consumes this link. On a toast chunk the field is the chain link
    /// instead — see [`crate::toast`].
    ///
    /// TODO: point this at the successor version on UPDATE and follow the chain
    /// for the READ COMMITTED re-check (EvalPlanQual).
    #[allow(dead_code)]
    pub ctid: Tid,
    pub natts: u16,
    pub has_null: bool,
    /// At least one attribute is stored out of line.
    pub has_external: bool,
}

fn wr_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn wr_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn rd_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(bytes(buf, off))
}
fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(bytes(buf, off))
}
fn rd_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(bytes(buf, off))
}

fn bitmap_len(natts: usize) -> usize {
    natts.div_ceil(8)
}

/// One attribute as it will be written to the page: either the value itself or a
/// pointer to the chunks holding it.
///
/// Making "this attribute lives elsewhere" part of the type is what stops a
/// caller from silently writing a value that does not fit: there is no way to
/// encode a tuple without having decided, per attribute, which it is.
#[derive(Clone, Copy, Debug)]
pub enum Attr<'a> {
    Inline(&'a Value),
    External(ToastPointer),
}

impl Attr<'_> {
    fn is_null(&self) -> bool {
        matches!(self, Attr::Inline(Value::Null))
    }
}

/// Serialize a full tuple: header + (optional) null bitmap + non-null datums.
pub fn encode_tuple(vals: &[Attr<'_>], hdr: &TupleHeader, ctid: Tid) -> Vec<u8> {
    let natts = vals.len();
    let has_null = vals.iter().any(Attr::is_null);
    let has_external = vals.iter().any(|a| matches!(a, Attr::External(_)));
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
    if has_external {
        infomask2 |= HAS_EXTERNAL;
    }
    buf[OFF_INFOMASK2..OFF_INFOMASK2 + 2].copy_from_slice(&infomask2.to_le_bytes());
    wr_u32(&mut buf, OFF_CTID_BLOCK, ctid.block);
    buf[OFF_CTID_OFF..OFF_CTID_OFF + 2].copy_from_slice(&ctid.offset.to_le_bytes());

    // Null bitmap: a set bit means the column is NON-null (PostgreSQL convention).
    // An out-of-line attribute is never null, so it always sets its bit.
    if has_null {
        for (i, v) in vals.iter().enumerate() {
            if !v.is_null() {
                buf[TUPLE_HEADER_LEN + i / 8] |= 1 << (i % 8);
            }
        }
    }
    for v in vals {
        match v {
            Attr::Inline(Value::Null) => {}
            Attr::Inline(v) => encode_datum(v, &mut buf),
            Attr::External(p) => toast::encode_pointer(p, &mut buf),
        }
    }
    buf
}

/// Encode a tuple none of whose attributes are stored out of line. The common
/// case, and the one every test that predates TOAST wants.
pub fn encode_inline(vals: &[Value], hdr: &TupleHeader, ctid: Tid) -> Vec<u8> {
    let attrs: Vec<Attr<'_>> = vals.iter().map(Attr::Inline).collect();
    encode_tuple(&attrs, hdr, ctid)
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
    let ctid = Tid {
        block: rd_u32(buf, OFF_CTID_BLOCK),
        offset: rd_u16(buf, OFF_CTID_OFF),
    };
    OnPageHeader {
        hdr,
        ctid,
        natts: infomask2 & NATTS_MASK,
        has_null: infomask2 & HAS_NULL != 0,
        has_external: infomask2 & HAS_EXTERNAL != 0,
    }
}

/// A tuple decoded from a page but not yet detoasted.
///
/// `vals` is private, and [`RawTuple::resolve`] is the only way to reach the
/// column values, so "decoded but still holding pointers" is not a state a
/// caller can consume by accident.
pub struct RawTuple {
    vals: Tuple,
    /// `(attribute number, pointer)` for each out-of-line attribute, in
    /// attribute order. Empty for the overwhelming majority of tuples.
    external: Vec<(usize, ToastPointer)>,
}

/// Decode the header and every inline datum, leaving out-of-line attributes as
/// pointers.
///
/// Safe to call while the page's frame lock is held: it reads only `buf` and
/// never touches another relation. Reassembling the chunks — which does — is
/// [`RawTuple::resolve`]'s job, deliberately split out so the lock is released
/// first.
///
/// Returns [`StorageError::CorruptData`] rather than panicking on a malformed
/// pointer. Panicking here would fire *inside* the buffer pool's frame guard,
/// poisoning it and taking every later page fault down with it — so one bad page
/// would stop the process rather than one query.
pub fn decode_raw(buf: &[u8]) -> Result<RawTuple, StorageError> {
    let head = decode_header(buf);
    // The header is decoded again here rather than passed in: callers run the
    // visibility check on `decode_header` alone, so an invisible tuple never
    // reaches this function and never pays for its datums.
    let natts = head.natts as usize;
    let bmlen = if head.has_null { bitmap_len(natts) } else { 0 };
    let bitmap = &buf[TUPLE_HEADER_LEN..TUPLE_HEADER_LEN + bmlen];
    let mut pos = TUPLE_HEADER_LEN + bmlen;
    let mut vals = Vec::with_capacity(natts);
    let mut external = Vec::new();
    for i in 0..natts {
        let is_null = head.has_null && (bitmap[i / 8] & (1 << (i % 8))) == 0;
        if is_null {
            vals.push(Value::Null);
        } else if head.has_external && buf.get(pos) == Some(&T_EXTERNAL) {
            let Some(p) = toast::decode_pointer(buf, &mut pos) else {
                return Err(StorageError::CorruptData(format!(
                    "unreadable out-of-line pointer in attribute {i}"
                )));
            };
            external.push((i, p));
            // A placeholder until `resolve` fills it in. Never observable: the
            // only way out of this type replaces every entry listed in
            // `external`.
            vals.push(Value::Null);
        } else {
            vals.push(decode_datum(buf, &mut pos));
        }
    }
    Ok(RawTuple { vals, external })
}

impl RawTuple {
    /// The out-of-line attributes this tuple owns — what VACUUM reclaims once the
    /// tuple itself is dead.
    pub fn external(&self) -> &[(usize, ToastPointer)] {
        &self.external
    }

    /// The tuple's column values, with every out-of-line attribute reassembled.
    ///
    /// `detoast` is handed a pointer and returns the bytes its chain holds; it is
    /// the caller that owns a buffer pool. Nothing is read when the tuple has no
    /// out-of-line attribute, which is the common case.
    pub fn resolve(
        mut self,
        detoast: impl Fn(&ToastPointer) -> Result<Vec<u8>, StorageError>,
    ) -> Result<Tuple, StorageError> {
        for (i, p) in &self.external {
            let bytes = detoast(p)?;
            // The chunks hold exactly what `encode_datum` would have written
            // inline, so reassembly is an ordinary decode and needs no knowledge
            // of the attribute's type.
            let mut pos = 0;
            self.vals[*i] = decode_datum(&bytes, &mut pos);
        }
        Ok(self.vals)
    }
}

/// Build one toast chunk: the ordinary tuple header with `natts = 0` and its
/// `ctid` naming the next chunk in the chain (itself, on the last), followed by
/// the raw payload.
///
/// Reusing the tuple header verbatim is what lets chunks travel the heap's own
/// placement, WAL and redo paths — `natts = 0` means a chunk that ever reaches
/// [`decode_raw`] by mistake yields an empty tuple rather than garbage.
pub fn encode_chunk(payload: &[u8], hdr: &TupleHeader, next: Tid) -> Vec<u8> {
    let mut buf = vec![0u8; TUPLE_HEADER_LEN];
    wr_u64(&mut buf, OFF_XMIN, hdr.xmin.0);
    wr_u64(&mut buf, OFF_XMAX, hdr.xmax.0);
    wr_u32(&mut buf, OFF_CMIN, hdr.cmin.0);
    wr_u32(&mut buf, OFF_CMAX, hdr.cmax.0);
    buf[OFF_INFOMASK..OFF_INFOMASK + 2].copy_from_slice(&hdr.infomask.0.to_le_bytes());
    // infomask2 stays 0: natts = 0, no null bitmap, nothing external.
    wr_u32(&mut buf, OFF_CTID_BLOCK, next.block);
    buf[OFF_CTID_OFF..OFF_CTID_OFF + 2].copy_from_slice(&next.offset.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Read a toast chunk: its chain link and its payload. `None` if the item is too
/// short to be a chunk — corruption, which the caller reports rather than
/// indexing past the end (this runs inside a page guard, where a panic would
/// poison the frame).
pub fn decode_chunk(buf: &[u8]) -> Option<(Tid, &[u8])> {
    let payload = buf.get(TUPLE_HEADER_LEN..)?;
    let next = Tid {
        block: rd_u32(buf, OFF_CTID_BLOCK),
        offset: rd_u16(buf, OFF_CTID_OFF),
    };
    Some((next, payload))
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

    use crate::smgr::RelFileNode;

    /// Decode a tuple that has nothing stored out of line.
    fn decode_inline(buf: &[u8]) -> (OnPageHeader, Tuple) {
        let head = decode_header(buf);
        let raw = decode_raw(buf).unwrap_or_else(|e| panic!("decode failed: {e}"));
        assert!(raw.external().is_empty(), "expected no external attributes");
        let vals = raw
            .resolve(|_| unreachable!("no external attributes to detoast"))
            .unwrap_or_else(|e| panic!("resolve failed: {e}"));
        (head, vals)
    }

    #[test]
    fn roundtrip_with_and_without_nulls() {
        let hdr = TupleHeader::inserted(Xid(7), CommandId(2));
        let ctid = Tid {
            block: 3,
            offset: 9,
        };
        for vals in [
            vec![Value::Int4(1), Value::Text("x".into()), Value::Bool(true)],
            vec![Value::Null, Value::Text("y".into()), Value::Null],
            vec![Value::Null, Value::Null],
        ] {
            let buf = encode_inline(&vals, &hdr, ctid);
            let (head, got) = decode_inline(&buf);
            assert_eq!(head.natts as usize, vals.len());
            assert_eq!(head.hdr.xmin, Xid(7));
            assert_eq!(head.ctid, ctid);
            assert!(!head.has_external);
            assert_eq!(got, vals);
        }
    }

    fn pointer(rawsize: u32) -> ToastPointer {
        ToastPointer {
            rel: RelFileNode(42),
            first: Tid {
                block: 1,
                offset: 2,
            },
            rawsize,
        }
    }

    #[test]
    fn an_external_attribute_round_trips_as_a_pointer() {
        let hdr = TupleHeader::inserted(Xid(7), CommandId(2));
        let ctid = Tid {
            block: 0,
            offset: 1,
        };
        let p = pointer(9);
        let big = Value::Text("detoasted".into());
        let buf = encode_tuple(
            &[
                Attr::Inline(&Value::Int4(1)),
                Attr::External(p),
                Attr::Inline(&Value::Bool(true)),
            ],
            &hdr,
            ctid,
        );
        let head = decode_header(&buf);
        let raw = decode_raw(&buf).unwrap_or_else(|e| panic!("decode failed: {e}"));
        assert!(head.has_external);
        assert_eq!(head.natts, 3);
        assert_eq!(raw.external(), &[(1, p)]);

        // The inline attributes on either side of the pointer are untouched, and
        // the resolved value lands in the right slot.
        let mut encoded = Vec::new();
        encode_datum(&big, &mut encoded);
        let vals = raw
            .resolve(|got| {
                assert_eq!(*got, p);
                Ok(encoded.clone())
            })
            .unwrap_or_else(|e| panic!("resolve failed: {e}"));
        assert_eq!(vals, vec![Value::Int4(1), big, Value::Bool(true)]);
    }

    #[test]
    fn an_external_attribute_does_not_disturb_the_null_bitmap() {
        // Nulls and an out-of-line attribute in one tuple: the pointer occupies a
        // datum slot and so must set its bitmap bit like any non-null value.
        let hdr = TupleHeader::inserted(Xid(1), CommandId(0));
        let ctid = Tid {
            block: 0,
            offset: 1,
        };
        let p = pointer(4);
        let buf = encode_tuple(
            &[
                Attr::Inline(&Value::Null),
                Attr::External(p),
                Attr::Inline(&Value::Null),
                Attr::Inline(&Value::Int4(5)),
            ],
            &hdr,
            ctid,
        );
        let head = decode_header(&buf);
        let raw = decode_raw(&buf).unwrap_or_else(|e| panic!("decode failed: {e}"));
        assert!(head.has_null && head.has_external);
        assert_eq!(raw.external(), &[(1, p)]);
        let vals = raw
            .resolve(|_| {
                let mut out = Vec::new();
                encode_datum(&Value::Text("v".into()), &mut out);
                Ok(out)
            })
            .unwrap_or_else(|e| panic!("resolve failed: {e}"));
        assert_eq!(
            vals,
            vec![
                Value::Null,
                Value::Text("v".into()),
                Value::Null,
                Value::Int4(5)
            ]
        );
    }

    #[test]
    fn in_place_edits_survive_an_external_attribute() {
        // `stamp_xmax` and `set_ctid` write fixed header offsets, so they must not
        // care that a pointer sits in the datum area.
        let hdr = TupleHeader::inserted(Xid(1), CommandId(0));
        let p = pointer(3);
        let mut buf = encode_tuple(
            &[Attr::External(p), Attr::Inline(&Value::Int4(9))],
            &hdr,
            Tid {
                block: 0,
                offset: 1,
            },
        );
        stamp_xmax(&mut buf, Xid(8), CommandId(4));
        set_ctid(
            &mut buf,
            Tid {
                block: 2,
                offset: 5,
            },
        );
        let head = decode_header(&buf);
        let raw = decode_raw(&buf).unwrap_or_else(|e| panic!("decode failed: {e}"));
        assert_eq!(head.hdr.xmax, Xid(8));
        assert!(head.has_external);
        assert_eq!(raw.external(), &[(0, p)]);
    }

    #[test]
    fn stamp_xmax_and_set_ctid_in_place() {
        let hdr = TupleHeader::inserted(Xid(7), CommandId(0));
        let mut buf = encode_inline(
            &[Value::Int4(1)],
            &hdr,
            Tid {
                block: 0,
                offset: 1,
            },
        );
        stamp_xmax(&mut buf, Xid(8), CommandId(4));
        set_ctid(
            &mut buf,
            Tid {
                block: 2,
                offset: 5,
            },
        );
        let head = decode_header(&buf);
        assert_eq!(head.hdr.xmax, Xid(8));
        assert_eq!(head.hdr.cmax, CommandId(4));
        assert_eq!(
            head.ctid,
            Tid {
                block: 2,
                offset: 5
            }
        );
        // The datum still decodes after the in-place edits.
        let (_, vals) = decode_inline(&buf);
        assert_eq!(vals, vec![Value::Int4(1)]);
    }
}
