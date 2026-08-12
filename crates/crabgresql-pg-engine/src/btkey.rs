//! Order-preserving key encoding and index-tuple codecs for the durable B-tree
//! (`nbtree`). The encoding's one job is to make on-page byte order (`memcmp`)
//! reproduce `crabgresql_executor::compare_values` for the supported key types,
//! so the tree can order and search entirely by raw bytes with no type
//! dispatch on the hot path.
//!
//! A `DESC` key column is encoded ascending and then **bit-inverted**. Each
//! column's encoding is prefix-free (see [`encode_text`]; every other type is
//! fixed width), and over a prefix-free code the bitwise complement reverses
//! lexicographic order exactly — so an index declared `(a DESC)` is stored in
//! the order a descending scan reads forward. Equality never noticed the
//! direction; a range does, which is why this exists. The inversion is applied
//! to the *finished* column encoding, after its escaping and terminator, so the
//! prefix-freeness that makes it valid is the property being inverted.
//!
//! The servable key types are exactly the arms of [`encode_one`]: `bool`,
//! `"char"`, the signed integers, `oid`, the text family
//! (`text`/`varchar`/`name`), `uuid`, `date`, `tid`, `xid8` and `pg_lsn`.
//!
//! TODO: encode key types whose SQL `=` normalizes a value's representation —
//! `bpchar` (blank padding), `numeric`/`float` (canonicalization). An index over
//! one of those is registered metadata-only and the planner falls back to a
//! scan.
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

/// Encode the key columns of `tuple`, in key order, into order-preserving
/// bytes, or `None` when any key column is NULL (a NULL never satisfies
/// equality, so such a row is simply not indexed) or holds a value of an
/// un-indexable form.
pub fn encode_row(schema: &TableSchema, keys: &[IndexKey], tuple: &Tuple) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for key in keys {
        let ty = schema.columns.get(key.column)?.ty;
        append_key_column(ty, key.descending, tuple.get(key.column)?, &mut out)?;
    }
    Some(out)
}

/// Why a probe's bytes could not be produced — a distinction the caller must
/// keep, because the two answers are opposite.
///
/// [`Encoded::Null`] is a value the tree provably indexed no row under
/// ([`encode_row`] declines a NULL), so the probe *was* served and matched
/// nothing. [`Encoded::Unsupported`] is a value this encoding cannot represent
/// at all — a type that does not match its key column — about which the tree
/// says nothing; treating it as "no match" would silently answer a query with
/// too few rows, so the caller declines the probe and falls back to a scan.
#[derive(Debug, PartialEq, Eq)]
pub enum Encoded {
    Bytes(Vec<u8>),
    Null,
    Unsupported,
}

/// Encode the leading `vals.len()` key columns of a probe, so the result is a
/// byte **prefix** of every stored key whose leading columns hold those values.
///
/// `vals.len() == keys.len()` is the equality probe; a shorter list is a
/// prefix probe (an index on `(a, b)` searched by `a` alone), and an empty one
/// encodes to empty bytes — a prefix every key starts with, which is what a
/// pure range probe on the first key column wants.
pub fn encode_prefix(schema: &TableSchema, keys: &[IndexKey], vals: &[Value]) -> Encoded {
    if vals.len() > keys.len() {
        return Encoded::Unsupported;
    }
    let mut out = Vec::new();
    for (key, v) in keys.iter().zip(vals) {
        match encode_column(schema, key, v, &mut out) {
            ColumnEncode::Ok => {}
            ColumnEncode::Null => return Encoded::Null,
            ColumnEncode::Unsupported => return Encoded::Unsupported,
        }
    }
    Encoded::Bytes(out)
}

/// Encode one range bound's value for key column `key`, producing the bytes a
/// stored key holding that value would carry at that position.
pub fn encode_bound(schema: &TableSchema, key: &IndexKey, val: &Value) -> Encoded {
    let mut out = Vec::new();
    match encode_column(schema, key, val, &mut out) {
        ColumnEncode::Ok => Encoded::Bytes(out),
        ColumnEncode::Null => Encoded::Null,
        ColumnEncode::Unsupported => Encoded::Unsupported,
    }
}

/// [`Encoded`] without the bytes: what appending one column to a caller-owned
/// buffer can report.
enum ColumnEncode {
    Ok,
    Null,
    Unsupported,
}

fn encode_column(
    schema: &TableSchema,
    key: &IndexKey,
    v: &Value,
    out: &mut Vec<u8>,
) -> ColumnEncode {
    if matches!(v, Value::Null) {
        return ColumnEncode::Null;
    }
    let Some(ty) = schema.columns.get(key.column).map(|c| c.ty) else {
        return ColumnEncode::Unsupported;
    };
    match append_key_column(ty, key.descending, v, out) {
        Some(()) => ColumnEncode::Ok,
        None => ColumnEncode::Unsupported,
    }
}

/// Append one value's key encoding, inverted when the key column is `DESC` (see
/// the module docs for why complementing is the right way to descend).
fn append_key_column(ty: PgType, descending: bool, v: &Value, out: &mut Vec<u8>) -> Option<()> {
    let start = out.len();
    encode_one(ty, v, out)?;
    if descending {
        for b in &mut out[start..] {
            *b = !*b;
        }
    }
    Some(())
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

    /// The load-bearing invariant: for every supported type, the byte order of
    /// the encoding reproduces `compare_values`. If this ever fails a probe could
    /// miss a live row, so it is checked exhaustively over a per-type sample.
    ///
    /// Checked in both directions: a `DESC` key column is the same encoding
    /// complemented, and the whole point of complementing is that it reverses
    /// the order exactly — an approximation there would order a descending
    /// index almost right, which a range scan reads as missing rows.
    fn assert_order_preserved(ty: PgType, samples: &[Value]) {
        for a in samples {
            for b in samples {
                for descending in [false, true] {
                    let mut ea = Vec::new();
                    let mut eb = Vec::new();
                    append_key_column(ty, descending, a, &mut ea).expect("a encodes");
                    append_key_column(ty, descending, b, &mut eb).expect("b encodes");
                    let want = match descending {
                        false => compare_values(ty, a, b),
                        true => compare_values(ty, a, b).reverse(),
                    };
                    assert_eq!(
                        ea.cmp(&eb),
                        want,
                        "encoding order disagrees with compare_values for {ty:?} \
                         (descending={descending}): {a:?} vs {b:?}"
                    );
                }
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

    /// A two-column `(text, int4)` schema and its key, `n`'s direction chosen
    /// by the caller.
    fn text_int_schema() -> TableSchema {
        TableSchema::new(
            "t",
            vec![
                crabgresql_storage_api::Column::new("s", PgType::Text),
                crabgresql_storage_api::Column::new("n", PgType::Int4),
            ],
        )
    }

    fn keys(directions: [bool; 2]) -> Vec<IndexKey> {
        directions
            .iter()
            .enumerate()
            .map(|(column, &descending)| IndexKey {
                column,
                descending,
                nulls_first: false,
            })
            .collect()
    }

    #[test]
    fn composite_key_is_order_preserving_across_a_text_boundary() {
        // Two-column (text, int4) key: the escape/terminator must stop a longer
        // first column from bleeding into the second's bytes. Compare by the
        // logical (text, int4) order and assert the encoding agrees.
        //
        // Run with `n` ascending and descending: the second column's direction
        // must reverse *only* the tiebreak, leaving the first column's order
        // untouched — an inversion applied to the wrong span shows up here and
        // nowhere else.
        let schema = text_int_schema();
        let rows: Vec<(String, i32)> = vec![
            ("a".into(), 1),
            ("a".into(), 2),
            ("a".into(), i32::MIN),
            ("ab".into(), -5),
            ("a\0".into(), 9),
            ("b".into(), i32::MIN),
        ];
        for descending in [false, true] {
            let key = keys([false, descending]);
            for (s1, n1) in &rows {
                for (s2, n2) in &rows {
                    let e1 = encode_row(
                        &schema,
                        &key,
                        &vec![Value::Text(s1.clone()), Value::Int4(*n1)],
                    )
                    .expect("row 1 encodes");
                    let e2 = encode_row(
                        &schema,
                        &key,
                        &vec![Value::Text(s2.clone()), Value::Int4(*n2)],
                    )
                    .expect("row 2 encodes");
                    let tiebreak = match descending {
                        false => n1.cmp(n2),
                        true => n1.cmp(n2).reverse(),
                    };
                    let want = s1.as_bytes().cmp(s2.as_bytes()).then(tiebreak);
                    assert_eq!(
                        e1.cmp(&e2),
                        want,
                        "composite order (n descending={descending}): \
                         {s1:?},{n1} vs {s2:?},{n2}"
                    );
                }
            }
        }
    }

    /// What makes a prefix probe legal: the encoding of the leading key columns
    /// is a byte prefix of exactly the keys whose leading columns hold those
    /// values, and of no others. The `("a", _)` / `("ab", _)` pair is the case
    /// that fails without the text terminator.
    #[test]
    fn a_prefix_encodes_to_a_byte_prefix_of_the_matching_rows_only() {
        let schema = text_int_schema();
        let key = keys([false, false]);
        let Encoded::Bytes(prefix) = encode_prefix(&schema, &key, &[Value::Text("a".into())])
        else {
            panic!("a text prefix encodes");
        };
        for (s, n, want) in [
            ("a", 1, true),
            ("a", i32::MIN, true),
            ("ab", 1, false),
            ("a\0", 1, false),
            ("b", 1, false),
        ] {
            let row = encode_row(&schema, &key, &vec![Value::Text(s.into()), Value::Int4(n)])
                .expect("row encodes");
            assert_eq!(
                row.starts_with(&prefix),
                want,
                "prefix \"a\" against row ({s:?}, {n})"
            );
        }
    }

    /// The full key is itself a prefix — that is what lets one scan routine
    /// serve equality and range alike (`nbtree::search_equal`).
    #[test]
    fn a_full_length_prefix_equals_the_row_encoding() {
        let schema = text_int_schema();
        let key = keys([false, true]);
        let vals = vec![Value::Text("a".into()), Value::Int4(7)];
        let Encoded::Bytes(prefix) = encode_prefix(&schema, &key, &vals) else {
            panic!("a full key encodes");
        };
        assert_eq!(
            Some(prefix),
            encode_row(&schema, &key, &vals),
            "a full-length prefix must be the row's key, byte for byte"
        );
    }

    /// The distinction the probe contract rests on: a NULL is "served, no
    /// match", a type mismatch is "cannot answer". Collapsing them would turn a
    /// mismatch into silently missing rows.
    #[test]
    fn null_and_type_mismatch_are_different_answers() {
        let schema = text_int_schema();
        let key = keys([false, false]);
        assert_eq!(encode_prefix(&schema, &key, &[Value::Null]), Encoded::Null);
        assert_eq!(
            encode_prefix(&schema, &key, &[Value::Int4(1)]),
            Encoded::Unsupported,
            "an int4 against a text key column is a mismatch, not a NULL"
        );
        assert_eq!(encode_bound(&schema, &key[1], &Value::Null), Encoded::Null);
        // More values than key columns: nothing could match, and it is not a
        // NULL, so the probe is declined rather than answered empty.
        assert_eq!(
            encode_prefix(
                &schema,
                &key,
                &[Value::Text("a".into()), Value::Int4(1), Value::Int4(2)]
            ),
            Encoded::Unsupported
        );
    }

    /// A bound's bytes are the bytes that value occupies inside a stored key,
    /// so `prefix ++ bound` is directly comparable to a stored key.
    #[test]
    fn a_bound_encodes_as_the_column_occupies_a_stored_key() {
        let schema = text_int_schema();
        let key = keys([false, true]);
        let Encoded::Bytes(prefix) = encode_prefix(&schema, &key, &[Value::Text("a".into())])
        else {
            panic!("prefix encodes");
        };
        let Encoded::Bytes(bound) = encode_bound(&schema, &key[1], &Value::Int4(7)) else {
            panic!("bound encodes");
        };
        let row = encode_row(
            &schema,
            &key,
            &vec![Value::Text("a".into()), Value::Int4(7)],
        )
        .expect("row encodes");
        assert_eq!([prefix, bound].concat(), row);
    }
}
