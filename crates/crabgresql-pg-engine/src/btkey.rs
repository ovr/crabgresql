//! Order-preserving key encoding and index-tuple codecs for the durable B-tree
//! (`nbtree`). The encoding's one job is to make on-page byte order (`memcmp`)
//! reproduce `crabgresql_executor::compare_values` for the supported key types,
//! so the tree can order and search entirely by raw bytes with no type
//! dispatch on the hot path.
//!
//! Supported key types match the in-memory reference engine's servable set
//! (`crabgresql_memory_storage`'s `key_type_indexable`): `bool`, the signed
//! integers, `oid`, the text family (`text`/`varchar`/`name`), `uuid`, and
//! `date`. Types whose SQL `=` normalizes a value's representation — `bpchar`
//! (blank padding), `numeric`/`float` (canonicalization) — are deferred: an
//! index over one is metadata-only and the planner falls back to a scan.
//!
//! Every leaf/internal item carries the heap `Tid` as a low-order tiebreak, so
//! the ordering `(key, tid)` is total even when a key has many duplicates. That
//! tiebreak is what lets descent route past a run of equal keys that a split
//! spread across several leaves (see `nbtree`).

use crabgresql_storage_api::{IndexKey, TableSchema, Tid, Tuple};
use crabgresql_types::{PgType, Value};

/// Whether `ty` is a key type this B-tree can physically index. Kept in sync
/// with [`encode_one`]: exactly the types that arm handles.
pub fn type_indexable(ty: PgType) -> bool {
    matches!(
        ty,
        PgType::Bool
            | PgType::Char
            | PgType::Int2
            | PgType::Int4
            | PgType::Int8
            | PgType::Oid
            | PgType::Text
            | PgType::Varchar
            | PgType::Name
            | PgType::Uuid
            | PgType::Date
            | PgType::Tid
            | PgType::Xid8
            | PgType::PgLsn
    )
}

/// Whether every one of `keys`' columns is present in `schema` and has an
/// indexable type — i.e. this engine can serve a physical scan for the index.
pub fn keys_indexable(schema: &TableSchema, keys: &[IndexKey]) -> bool {
    !keys.is_empty()
        && keys.iter().all(|k| {
            schema
                .columns
                .get(k.column)
                .is_some_and(|c| type_indexable(c.ty))
        })
}

/// The column positions of `keys`, in key order.
pub fn key_columns(keys: &[IndexKey]) -> Vec<usize> {
    keys.iter().map(|k| k.column).collect()
}

/// Encode the key columns (`cols`, in key order) of `tuple` into
/// order-preserving bytes, or `None` when any key column is NULL (a NULL never
/// satisfies equality, so such a row is simply not indexed — matching the
/// in-memory engine) or holds a value of an un-indexable form.
pub fn encode_row(schema: &TableSchema, cols: &[usize], tuple: &Tuple) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for &c in cols {
        let ty = schema.columns.get(c)?.ty;
        encode_one(ty, tuple.get(c)?, &mut out)?;
    }
    Some(out)
}

/// Encode a probe key (`vals`, one value per key column in key order) exactly as
/// [`encode_row`] would, so a probe's bytes equal a matching row's key bytes.
/// `None` (fall back / no match) on a length mismatch, a NULL, or an
/// un-indexable value.
pub fn encode_values(schema: &TableSchema, cols: &[usize], vals: &[Value]) -> Option<Vec<u8>> {
    if vals.len() != cols.len() {
        return None;
    }
    let mut out = Vec::new();
    for (&c, v) in cols.iter().zip(vals) {
        let ty = schema.columns.get(c)?.ty;
        encode_one(ty, v, &mut out)?;
    }
    Some(out)
}

/// Append one value's order-preserving encoding to `out`. Signed integers and
/// `date` are big-endian with the sign bit flipped, so two's-complement
/// negatives sort before positives; `oid` is unsigned big-endian; text is
/// escaped (see [`encode_text`]) so it is composite-safe; `uuid` is its raw
/// bytes. `None` on NULL or a type/value mismatch.
fn encode_one(ty: PgType, v: &Value, out: &mut Vec<u8>) -> Option<()> {
    match (ty, v) {
        (_, Value::Null) => return None,
        (PgType::Bool, Value::Bool(b)) => out.push(u8::from(*b)),
        // `"char"` orders unsigned, so the raw byte is already order-preserving.
        // Fixed width means it needs no escaping or terminator to stay
        // composite-safe, unlike the text family below.
        (PgType::Char, Value::Char(c)) => out.push(*c),
        (PgType::Int2, Value::Int2(x)) => {
            out.extend_from_slice(&(*x as u16 ^ 0x8000).to_be_bytes())
        }
        (PgType::Int4, Value::Int4(x)) => {
            out.extend_from_slice(&(*x as u32 ^ 0x8000_0000).to_be_bytes())
        }
        (PgType::Int8, Value::Int8(x)) => {
            out.extend_from_slice(&(*x as u64 ^ 0x8000_0000_0000_0000).to_be_bytes())
        }
        // date is signed days since 2000-01-01 with i32::MIN/MAX as ±infinity;
        // sign-flipped big-endian orders those sentinels naturally, matching
        // `compare_values`' `date::cmp`.
        (PgType::Date, Value::Date(x)) => {
            out.extend_from_slice(&(*x as u32 ^ 0x8000_0000).to_be_bytes())
        }
        (PgType::Oid, Value::Oid(x)) => out.extend_from_slice(&x.to_be_bytes()),
        // Unsigned counters: plain big-endian already matches `compare_values`.
        // `xid` is deliberately absent — it has no btree opclass at all, so a
        // key of that type is rejected before it reaches any index.
        (PgType::Xid8, Value::Xid8(x)) => out.extend_from_slice(&x.to_be_bytes()),
        (PgType::PgLsn, Value::PgLsn(x)) => out.extend_from_slice(&x.to_be_bytes()),
        // tid orders by block then offset, so concatenating the two big-endian
        // fields reproduces that lexicographic order bytewise.
        (PgType::Tid, Value::Tid { block, offset }) => {
            out.extend_from_slice(&block.to_be_bytes());
            out.extend_from_slice(&offset.to_be_bytes());
        }
        (PgType::Text | PgType::Varchar | PgType::Name, Value::Text(s)) => {
            encode_text(s.as_bytes(), out)
        }
        (PgType::Uuid, Value::Uuid(b)) => out.extend_from_slice(b),
        // Type/value mismatch or an un-indexable type: not encodable.
        _ => return None,
    }
    Some(())
}

/// Escape a text segment so its `memcmp` order equals byte order even when
/// another column follows: a data `0x00` becomes `0x00 0xFF` and the segment is
/// terminated by `0x00 0x00`. A data `0xFF` never occurs (a Rust `String` is
/// valid UTF-8, which has no `0xFF` byte), so the escape is unambiguous. Since
/// the terminator (`0x00 0x00`) sorts below any escaped byte, a shorter string
/// (a prefix) sorts before its extensions, as byte comparison requires.
fn encode_text(bytes: &[u8], out: &mut Vec<u8>) {
    for &b in bytes {
        out.push(b);
        if b == 0x00 {
            out.push(0xFF);
        }
    }
    out.push(0x00);
    out.push(0x00);
}

// -- Index-tuple (item) codecs. A leaf item points a key at a heap tuple; an
// -- internal item points a separator key at a child block. Both carry the tid
// -- tiebreak. Little-endian, matching the page primitives.

/// A leaf item: `[tid: u64][key bytes]`. The key is the item tail.
pub fn make_leaf_item(tid: Tid, key: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + key.len());
    v.extend_from_slice(&tid.packed().to_le_bytes());
    v.extend_from_slice(key);
    v
}

pub fn leaf_tid(item: &[u8]) -> Tid {
    let mut b = [0u8; 8];
    b.copy_from_slice(&item[..8]);
    Tid::from_packed(u64::from_le_bytes(b))
}

pub fn leaf_key(item: &[u8]) -> &[u8] {
    &item[8..]
}

/// An internal item: `[child: u32][tid: u64][key bytes]`. The routing key is
/// `(key, tid)` — the first entry of the child subtree. The leftmost downlink on
/// a page uses an empty key and `tid = 0` (a minus-infinity separator that sorts
/// below every real entry).
pub fn make_internal_item(child: u32, tid_packed: u64, key: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(12 + key.len());
    v.extend_from_slice(&child.to_le_bytes());
    v.extend_from_slice(&tid_packed.to_le_bytes());
    v.extend_from_slice(key);
    v
}

pub fn internal_child(item: &[u8]) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&item[..4]);
    u32::from_le_bytes(b)
}

pub fn internal_tid(item: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&item[4..12]);
    u64::from_le_bytes(b)
}

pub fn internal_key(item: &[u8]) -> &[u8] {
    &item[12..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_executor::compare_values;
    use std::cmp::Ordering;

    /// The load-bearing invariant: for every supported type, the byte order of
    /// the encoding reproduces `compare_values`. If this ever fails a probe could
    /// miss a live row, so it is checked exhaustively over a per-type sample.
    fn assert_order_preserved(ty: PgType, samples: &[Value]) {
        for a in samples {
            for b in samples {
                let mut ea = Vec::new();
                let mut eb = Vec::new();
                encode_one(ty, a, &mut ea).expect("a encodes");
                encode_one(ty, b, &mut eb).expect("b encodes");
                assert_eq!(
                    ea.cmp(&eb),
                    compare_values(ty, a, b),
                    "encoding order disagrees with compare_values for {ty:?}: {a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn integers_are_order_preserving() {
        assert_order_preserved(
            PgType::Int2,
            &[i16::MIN, -257, -1, 0, 1, 256, i16::MAX]
                .map(Value::Int2)
                .to_vec(),
        );
        assert_order_preserved(
            PgType::Int4,
            &[i32::MIN, -70000, -1, 0, 1, 70000, i32::MAX]
                .map(Value::Int4)
                .to_vec(),
        );
        assert_order_preserved(
            PgType::Int8,
            &[i64::MIN, -1, 0, 1, i64::MAX].map(Value::Int8).to_vec(),
        );
    }

    #[test]
    fn oid_bool_date_uuid_are_order_preserving() {
        assert_order_preserved(
            PgType::Oid,
            &[0u32, 1, 16384, u32::MAX].map(Value::Oid).to_vec(),
        );
        assert_order_preserved(PgType::Bool, &[Value::Bool(false), Value::Bool(true)]);
        // `"char"` orders unsigned: 0x7F/0x80/0xFF are the samples that fail if
        // either this encoding or `compare_values` ever reads the byte signed.
        assert_order_preserved(
            PgType::Char,
            &[0u8, 1, b'A', b'a', 0x7F, 0x80, 0xFF]
                .map(Value::Char)
                .to_vec(),
        );
        assert_order_preserved(
            PgType::Date,
            &[i32::MIN, -1, 0, 1, 8766, i32::MAX]
                .map(Value::Date)
                .to_vec(),
        );
        assert_order_preserved(
            PgType::Uuid,
            &[
                [0u8; 16],
                [0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                [0xff; 16],
            ]
            .map(Value::Uuid)
            .to_vec(),
        );
    }

    #[test]
    fn tid_xid8_and_pg_lsn_are_order_preserving() {
        assert_order_preserved(
            PgType::Xid8,
            &[0u64, 1, 42, i64::MAX as u64, u64::MAX]
                .map(Value::Xid8)
                .to_vec(),
        );
        assert_order_preserved(
            PgType::PgLsn,
            &[0u64, 1, 0x1_0000_0000, 0xFFFF_FFFF_FFFF_FFFF]
                .map(Value::PgLsn)
                .to_vec(),
        );
        // The block field must dominate the offset: `(1,0)` sorts after
        // `(0,65535)` even though its offset is smaller.
        assert_order_preserved(
            PgType::Tid,
            &[
                (0u32, 0u16),
                (0, 1),
                (0, u16::MAX),
                (1, 0),
                (u32::MAX, u16::MAX),
            ]
            .map(|(block, offset)| Value::Tid { block, offset })
            .to_vec(),
        );
    }

    #[test]
    fn text_is_order_preserving_including_prefixes_and_embedded_nul() {
        let samples: Vec<Value> = ["", "a", "aa", "ab", "a\0", "a\0b", "b", "\u{7f}", "é"]
            .iter()
            .map(|s| Value::Text((*s).to_string()))
            .collect();
        for ty in [PgType::Text, PgType::Varchar, PgType::Name] {
            assert_order_preserved(ty, &samples);
        }
    }

    #[test]
    fn null_and_type_mismatch_do_not_encode() {
        let mut out = Vec::new();
        assert!(encode_one(PgType::Int4, &Value::Null, &mut out).is_none());
        assert!(encode_one(PgType::Int4, &Value::Text("x".into()), &mut out).is_none());
    }

    #[test]
    fn leaf_and_internal_items_round_trip() {
        let tid = Tid::new(3, 9);
        let item = make_leaf_item(tid, b"key");
        assert_eq!(leaf_tid(&item), tid);
        assert_eq!(leaf_key(&item), b"key");

        let internal = make_internal_item(42, tid.packed(), b"sep");
        assert_eq!(internal_child(&internal), 42);
        assert_eq!(internal_tid(&internal), tid.packed());
        assert_eq!(internal_key(&internal), b"sep");
    }

    #[test]
    fn composite_key_is_order_preserving_across_a_text_boundary() {
        // Two-column (text, int4) key: the escape/terminator must stop a longer
        // first column from bleeding into the second's bytes. Compare by the
        // logical (text, int4) order and assert the encoding agrees.
        let schema = TableSchema::new(
            "t",
            vec![
                crabgresql_storage_api::Column::new("s", PgType::Text),
                crabgresql_storage_api::Column::new("n", PgType::Int4),
            ],
        );
        let rows: Vec<(String, i32)> = vec![
            ("a".into(), 1),
            ("a".into(), 2),
            ("a".into(), i32::MIN),
            ("ab".into(), -5),
            ("a\0".into(), 9),
            ("b".into(), i32::MIN),
        ];
        for (s1, n1) in &rows {
            for (s2, n2) in &rows {
                let e1 = encode_row(
                    &schema,
                    &[0, 1],
                    &vec![Value::Text(s1.clone()), Value::Int4(*n1)],
                )
                .expect("row 1 encodes");
                let e2 = encode_row(
                    &schema,
                    &[0, 1],
                    &vec![Value::Text(s2.clone()), Value::Int4(*n2)],
                )
                .expect("row 2 encodes");
                let want = s1.as_bytes().cmp(s2.as_bytes()).then(n1.cmp(n2));
                let got = e1.cmp(&e2);
                // Map to the same 3-way outcome, treating equal keys as Equal.
                let want = match want {
                    Ordering::Equal => Ordering::Equal,
                    o => o,
                };
                assert_eq!(got, want, "composite order: {s1:?},{n1} vs {s2:?},{n2}");
            }
        }
    }
}
