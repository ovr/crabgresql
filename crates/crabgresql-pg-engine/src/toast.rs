//! Out-of-line storage for oversized attributes — PostgreSQL's TOAST.
//!
//! An attribute too wide to keep inline is written to a **toast relation**, a
//! second relfilenode owned by the table, and replaced in the heap tuple by a
//! fixed-width [`ToastPointer`]. The pointer is stored under a datum tag the
//! ordinary value codec never emits ([`crabgresql_types::datum::T_EXTERNAL`]),
//! so a toasted attribute is unambiguous on the page.
//!
//! # What is stored out of line
//!
//! Exactly the bytes [`encode_datum`](crabgresql_types::datum::encode_datum)
//! would have written inline — tag and all. Detoasting is therefore
//! `concat(chunks)` followed by an ordinary `decode_datum`, with no per-type
//! logic: `text`, `bytea`, `json`, `jsonb`, arrays and `tsvector` all travel the
//! same path.
//!
//! # How chunks are found
//!
//! PostgreSQL gives each toasted value a `chunk_id` and indexes the toast
//! relation on `(chunk_id, chunk_seq)`. We reproduce the observable behavior —
//! wide values are stored and read back — with a chain instead: the pointer
//! names the first chunk's [`Tid`], and each chunk's on-page `ctid` field names
//! its successor (a chunk pointing at itself is the last). That needs no
//! sequence, no index, and no index maintenance, and keeps the pointer O(1) at
//! [`POINTER_LEN`] bytes however large the value is.
//!
//! Two honest costs of chaining, both acceptable today:
//!
//! - Reads are strictly sequential: there is no way to fetch chunk *n* without
//!   walking chunks `0..n`, so a slice read (`substr` over a toasted value)
//!   cannot skip ahead. Nothing pushes slicing down to storage yet.
//! - The toast relation cannot be vacuumed on its own. A chunk carries no
//!   back-pointer, so nothing can decide from the chunk alone whether it is
//!   live; reclamation is driven from the heap side by
//!   [`HeapTable::vacuum`](crate::heap::HeapTable), which is where the
//!   visibility information already is.
//!
//! Both are contained by the pointer's `format` byte: a future `chunk_id`
//! layout is a new format value, and old pointers keep decoding.
//!
//! # The relfilenode never changes
//!
//! A table's chunk store keeps its relfilenode for the table's whole life. It is
//! created lazily, on the first row that needs it, and from then on the durable
//! catalog always names it — which is what makes the startup orphan sweep safe.
//!
//! That is why TRUNCATE *empties* the store rather than swapping it the way it
//! swaps the heap file: a second relfilenode would be named by no WAL record and
//! would reach the catalog only at commit, so a crash in that window would leave
//! a committed row pointing into a file the sweep unlinks. Creation is serialized
//! for the same reason — two writers each publishing a store would leave one of
//! them unnamed, and unnamed means swept.
//!
//! # A chunk is an ordinary on-page tuple
//!
//! Chunks reuse [`crate::tuple`]'s 36-byte header verbatim, with `natts = 0` and
//! the `ctid` field repurposed as the chain link. That is what keeps this module
//! small: chunk writes go through the heap's own placement path and log
//! `HEAP_INSERT`, chunk reclamation logs `HEAP_VACUUM`, and **crash recovery
//! needs no new code at all** — the heap's redo handler applies those records to
//! a page without caring which relfilenode it belongs to.

use crabgresql_storage_api::{StorageError, Tid};
use crabgresql_types::datum::T_EXTERNAL;

use crate::page::{BLCKSZ, PAGE_HEADER_LEN};
use crate::smgr::RelFileNode;
use crate::tuple::TUPLE_HEADER_LEN;

/// Payload bytes carried by one chunk. Sized so four chunk items fill an 8 KB
/// page — the same density rule PostgreSQL's ~2 KB chunk size expresses, which
/// keeps a toasted value's chunks from wasting most of every page they touch.
///
/// `four_chunks_fit_one_page` asserts the arithmetic, so a change to the page or
/// tuple header cannot silently break the density this constant encodes.
pub const TOAST_MAX_CHUNK: usize = (BLCKSZ - PAGE_HEADER_LEN) / 4 - TUPLE_HEADER_LEN - 4;

/// `format` value for a chain of uncompressed chunks.
const FORMAT_PLAIN: u8 = 0;

/// Encoded width of a [`ToastPointer`]:
/// `[tag][format][rel: u32][block: u32][offset: u16][rawsize: u32]`.
pub const POINTER_LEN: usize = 1 + 1 + 4 + 4 + 2 + 4;

/// Locates one attribute's bytes in a toast relation.
///
/// The relfilenode rides on the pointer rather than being inferred from the
/// table so a tuple is self-describing: during a TRUNCATE the table names a new
/// toast relation while tuples in the old file still point at the old one, and
/// inferring would read the wrong generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToastPointer {
    pub rel: RelFileNode,
    /// The chain's first chunk.
    pub first: Tid,
    /// Total bytes stored across the chain. Checking the walk against it turns a
    /// truncated or corrupt chain into an error instead of a silently short
    /// value.
    pub rawsize: u32,
}

/// Append the on-page encoding of `p`, including its datum tag.
pub fn encode_pointer(p: &ToastPointer, out: &mut Vec<u8>) {
    out.push(T_EXTERNAL);
    out.push(FORMAT_PLAIN);
    out.extend_from_slice(&p.rel.0.to_le_bytes());
    out.extend_from_slice(&p.first.block.to_le_bytes());
    out.extend_from_slice(&p.first.offset.to_le_bytes());
    out.extend_from_slice(&p.rawsize.to_le_bytes());
}

/// Decode a pointer from `buf[pos..]`, which must start at its tag byte, and
/// advance `pos` past it. `None` if the bytes are truncated or carry a `format`
/// this build does not know — both mean the caller must not guess at a value.
pub fn decode_pointer(buf: &[u8], pos: &mut usize) -> Option<ToastPointer> {
    let bytes = buf.get(*pos..*pos + POINTER_LEN)?;
    if bytes[0] != T_EXTERNAL || bytes[1] != FORMAT_PLAIN {
        return None;
    }
    let u32_at = |off: usize| {
        u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
    };
    let p = ToastPointer {
        rel: RelFileNode(u32_at(2)),
        first: Tid {
            block: u32_at(6),
            offset: u16::from_le_bytes([bytes[10], bytes[11]]),
        },
        rawsize: u32_at(12),
    };
    *pos += POINTER_LEN;
    Some(p)
}

/// Split `bytes` into chunk payloads, last first.
///
/// Reversed on purpose: chunks are written in this order so each one knows the
/// [`Tid`] of its successor before it is placed. Writing forwards would need a
/// second pass to patch every chain link.
pub fn chunks_last_first(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.chunks(TOAST_MAX_CHUNK).rev()
}

/// A chain whose walk disagreed with the pointer that named it.
pub fn corrupt_chain(p: &ToastPointer, got: usize) -> StorageError {
    StorageError::CorruptData(format!(
        "toast chunk chain for relation {} at ({},{}) yielded {got} bytes, expected {}",
        p.rel.0, p.first.block, p.first.offset, p.rawsize
    ))
}

/// Toast a row down to this size when it exceeds it, so that several rows still
/// fit one page rather than one row monopolising it. PostgreSQL calls the
/// equivalent knob `TOAST_TUPLE_TARGET`.
pub const TOAST_TUPLE_TARGET: usize = 2000;

/// Below this width, moving an attribute out of line does not pay for itself: it
/// trades `width` inline bytes for [`POINTER_LEN`] inline bytes plus a whole
/// chunk item in another relation.
///
/// A *preference*, not a limit. If honouring it would leave the row too big to
/// store, [`plan`] drops to [`ABSOLUTE_MIN_TOAST_WIDTH`] and keeps going —
/// otherwise a row of many medium-width attributes would be refused where
/// PostgreSQL, which has no such floor, stores it.
const MIN_TOAST_WIDTH: usize = POINTER_LEN + 128;

/// The floor [`plan`] falls back to when the preferred one cannot make the row
/// fit. Anything at or below the pointer's own width frees no space at all, so
/// this is the point past which externalizing is genuinely pointless.
const ABSOLUTE_MIN_TOAST_WIDTH: usize = POINTER_LEN + 1;

/// Choose which attributes to store out of line.
///
/// `widths[i]` is attribute `i`'s encoded width (0 for NULL, which occupies no
/// datum), `toastable[i]` whether its type is variable-length, and `base` the
/// tuple header plus null bitmap. Returns the chosen attribute numbers, widest
/// first.
///
/// Two thresholds, matching PostgreSQL's split between the size it *aims* for
/// and the size it *fails* at: we keep moving attributes out while the tuple
/// exceeds `target`, but only raise [`StorageError::RowTooBig`] if it still
/// exceeds `max` once nothing is left to move.
pub fn plan(
    widths: &[usize],
    toastable: &[bool],
    base: usize,
    target: usize,
    max: usize,
) -> Result<Vec<usize>, StorageError> {
    let full: usize = base + widths.iter().sum::<usize>();
    if full <= target {
        return Ok(Vec::new());
    }
    // First pass at the preferred floor. If the row still does not fit, retry
    // taking anything wider than the pointer that replaces it: a row PostgreSQL
    // can store must not be refused just because our heuristic would rather not
    // externalize something small.
    let chosen = choose(widths, toastable, full, target, MIN_TOAST_WIDTH);
    if base + remaining(widths, &chosen) <= max {
        return Ok(chosen);
    }
    let chosen = choose(widths, toastable, full, target, ABSOLUTE_MIN_TOAST_WIDTH);
    let total = base + remaining(widths, &chosen);
    if total > max {
        return Err(StorageError::RowTooBig { size: full, max });
    }
    Ok(chosen)
}

/// Attributes to externalize, widest first, taking only those at least `floor`
/// bytes wide, until the tuple would fit `target`.
fn choose(
    widths: &[usize],
    toastable: &[bool],
    full: usize,
    target: usize,
    floor: usize,
) -> Vec<usize> {
    let mut chosen: Vec<usize> = Vec::new();
    let mut total = full;
    while total > target {
        // Widest eligible attribute; ties break on the lowest attribute number so
        // the choice is deterministic.
        let next = widths
            .iter()
            .enumerate()
            .filter(|&(i, &w)| toastable[i] && w >= floor && !chosen.contains(&i))
            .max_by_key(|&(i, &w)| (w, std::cmp::Reverse(i)));
        let Some((i, &w)) = next else { break };
        chosen.push(i);
        total = total - w + POINTER_LEN;
    }
    chosen
}

/// The inline bytes the attributes occupy once `chosen` are replaced by pointers.
fn remaining(widths: &[usize], chosen: &[usize]) -> usize {
    widths
        .iter()
        .enumerate()
        .map(|(i, &w)| if chosen.contains(&i) { POINTER_LEN } else { w })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_chunks_fit_one_page() {
        // The density this constant exists for: four chunk items — each a line
        // pointer, a tuple header and a payload — must fit an empty page.
        let item = 4 + TUPLE_HEADER_LEN + TOAST_MAX_CHUNK;
        assert!(4 * item <= BLCKSZ - PAGE_HEADER_LEN, "four chunks must fit");
        assert!(
            5 * item > BLCKSZ - PAGE_HEADER_LEN,
            "the chunk is sized for four per page, so five must not fit"
        );
    }

    #[test]
    fn pointer_encoding_is_fixed_width_and_round_trips() {
        let p = ToastPointer {
            rel: RelFileNode(0x0102_0304),
            first: Tid {
                block: 7,
                offset: 9,
            },
            rawsize: 100_000,
        };
        let mut buf = Vec::new();
        encode_pointer(&p, &mut buf);
        // The width is an on-disk format: assert the number, not `buf.len()`
        // against itself.
        assert_eq!(buf.len(), 16);
        assert_eq!(POINTER_LEN, 16);
        assert_eq!(buf[0], T_EXTERNAL);
        let mut pos = 0;
        assert_eq!(decode_pointer(&buf, &mut pos), Some(p));
        assert_eq!(pos, POINTER_LEN);
    }

    #[test]
    fn a_truncated_or_unknown_pointer_decodes_as_none() {
        let p = ToastPointer {
            rel: RelFileNode(1),
            first: Tid {
                block: 0,
                offset: 1,
            },
            rawsize: 5,
        };
        let mut buf = Vec::new();
        encode_pointer(&p, &mut buf);
        for short in 0..POINTER_LEN {
            let mut pos = 0;
            assert_eq!(decode_pointer(&buf[..short], &mut pos), None);
        }
        // A format this build does not know must not be guessed at.
        let mut future = buf.clone();
        future[1] = 1;
        let mut pos = 0;
        assert_eq!(decode_pointer(&future, &mut pos), None);
    }

    #[test]
    fn chunking_splits_at_the_chunk_size_last_first() {
        for len in [
            1,
            TOAST_MAX_CHUNK - 1,
            TOAST_MAX_CHUNK,
            TOAST_MAX_CHUNK + 1,
            2 * TOAST_MAX_CHUNK,
            2 * TOAST_MAX_CHUNK + 1,
        ] {
            let bytes: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let got: Vec<&[u8]> = chunks_last_first(&bytes).collect();
            assert_eq!(
                got.len(),
                len.div_ceil(TOAST_MAX_CHUNK),
                "chunk count for {len}"
            );
            assert!(
                got.iter()
                    .all(|c| !c.is_empty() && c.len() <= TOAST_MAX_CHUNK)
            );
            // Reversing the walk reassembles the original.
            let rejoined: Vec<u8> = got.iter().rev().flat_map(|c| c.iter().copied()).collect();
            assert_eq!(rejoined, bytes, "chunks must reassemble in reverse order");
        }
    }

    /// `plan` with the production thresholds.
    fn plan_at(
        widths: &[usize],
        toastable: &[bool],
        base: usize,
    ) -> Result<Vec<usize>, StorageError> {
        plan(widths, toastable, base, TOAST_TUPLE_TARGET, 8160)
    }

    #[test]
    fn a_tuple_within_the_target_is_left_alone() {
        assert_eq!(
            plan_at(&[500, 500], &[true, true], 36).expect("fits"),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn the_widest_attribute_goes_first_and_the_walk_stops_once_it_fits() {
        // Only the 5000-byte attribute needs to move: 36 + 16 + 900 + 300 < 2000.
        let chosen = plan_at(&[900, 5000, 300], &[true, true, true], 36).expect("fits");
        assert_eq!(chosen, vec![1]);
    }

    #[test]
    fn ties_break_on_the_lowest_attribute_number() {
        let chosen = plan_at(&[3000, 3000], &[true, true], 36).expect("fits");
        assert_eq!(chosen, vec![0, 1]);
    }

    #[test]
    fn a_row_of_untoastable_columns_is_rejected() {
        // 500 fixed-width attributes: nothing is eligible, so no amount of
        // out-of-line storage helps and the row must be refused.
        let widths = vec![17usize; 500];
        let toastable = vec![false; 500];
        let error =
            plan_at(&widths, &toastable, 36).expect_err("a row of untoastable columns is rejected");
        match error {
            StorageError::RowTooBig { size, max } => {
                assert_eq!(size, 36 + 500 * 17);
                assert_eq!(max, 8160);
            }
            other => panic!("expected RowTooBig, got {other}"),
        }
    }

    #[test]
    fn a_narrow_attribute_is_left_alone_when_the_row_already_fits() {
        // Just over the target but under `max`: the preferred floor declines to
        // shred the row into 60 chunks for no benefit, and nothing is moved.
        let widths = vec![20usize; 120];
        let toastable = vec![true; 120];
        assert_eq!(
            plan_at(&widths, &toastable, 36).expect("fits"),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn medium_attributes_are_externalized_rather_than_refusing_the_row() {
        // 100 attributes of 135 bytes each is 13536 bytes — over `max`, and every
        // attribute is under the preferred 144-byte floor. PostgreSQL has no such
        // floor and stores this row, so the fallback pass must too.
        let widths = vec![135usize; 100];
        let toastable = vec![true; 100];
        let chosen = plan_at(&widths, &toastable, 36).expect("must not refuse a storable row");
        assert!(!chosen.is_empty());
        let inline: usize = 36
            + widths
                .iter()
                .enumerate()
                .map(|(i, &w)| if chosen.contains(&i) { POINTER_LEN } else { w })
                .sum::<usize>();
        assert!(inline <= 8160, "the row must end up storable, got {inline}");
    }
}
