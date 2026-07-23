//! The B-tree page: a slotted [`page`] page that reserves a 16-byte *special*
//! region for per-page metadata — the level, the left/right sibling links, and
//! flag bits. The line-pointer array stays in logical (sorted) key order, so a
//! binary search walks it directly.
//!
//! Slot convention (PostgreSQL's `P_HIKEY`/`P_FIRSTDATAKEY`):
//! * a non-rightmost page keeps a **high key** at offset 1 (an upper bound: every
//!   data key on the page is `<=` it); data items start at offset 2;
//! * a rightmost page has no high key (its bound is +infinity); data starts at 1;
//! * an internal page's leftmost downlink is a zero-length *minus-infinity* key
//!   (`tid = 0`) that sorts below every real entry, so descent always has a
//!   leftmost target.

use crate::page::{self, Page};

/// Special-region length reserved by [`page::init_special`] on every B-tree page.
pub const BT_SPECIAL_LEN: usize = 16;

/// Sentinel for "no sibling / no such block". Block 0 is always the meta page and
/// is never a sibling, so it is safe as the invalid marker.
pub const INVALID_BLOCK: u32 = u32::MAX;
/// The meta page lives at block 0 and holds the root pointer.
pub const META_BLOCK: u32 = 0;

pub const BTP_LEAF: u16 = 1 << 0;
pub const BTP_ROOT: u16 = 1 << 1;
pub const BTP_META: u16 = 1 << 2;
#[allow(dead_code)] // page deletion/merge is deferred; the flag is reserved
pub const BTP_DELETED: u16 = 1 << 3;
/// Set on the left half of a split until its parent downlink is inserted. Equality
/// search does not depend on it (right-links cover an incomplete split); it is
/// reserved for a future `_bt_finish_split`.
#[allow(dead_code)]
pub const BTP_INCOMPLETE_SPLIT: u16 = 1 << 4;

/// The decoded special region of a B-tree page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BtOpaque {
    pub prev: u32,
    pub next: u32,
    pub level: u32,
    pub flags: u16,
}

fn rd_u32(b: &[u8], off: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[off..off + 4]);
    u32::from_le_bytes(a)
}
fn rd_u16(b: &[u8], off: usize) -> u16 {
    let mut a = [0u8; 2];
    a.copy_from_slice(&b[off..off + 2]);
    u16::from_le_bytes(a)
}

pub fn get_opaque(p: &Page) -> BtOpaque {
    let s = page::special(p);
    BtOpaque {
        prev: rd_u32(s, 0),
        next: rd_u32(s, 4),
        level: rd_u32(s, 8),
        flags: rd_u16(s, 12),
    }
}

pub fn set_opaque(p: &mut Page, o: &BtOpaque) {
    let s = page::special_mut(p);
    s[0..4].copy_from_slice(&o.prev.to_le_bytes());
    s[4..8].copy_from_slice(&o.next.to_le_bytes());
    s[8..12].copy_from_slice(&o.level.to_le_bytes());
    s[12..14].copy_from_slice(&o.flags.to_le_bytes());
    s[14..16].copy_from_slice(&0u16.to_le_bytes());
}

/// Initialize the meta page (block 0). Its single item, written by the caller,
/// carries the root pointer.
pub fn init_meta(p: &mut Page) {
    page::init_special(p, BT_SPECIAL_LEN);
    set_opaque(
        p,
        &BtOpaque {
            prev: INVALID_BLOCK,
            next: INVALID_BLOCK,
            level: 0,
            flags: BTP_META,
        },
    );
}

/// Rebuild a page from scratch to exactly `opaque` + `items` (in order). The
/// single primitive behind a split's or a new page's redo, and used by the live
/// path too so the in-memory result and the replayed result are byte-identical.
pub fn rebuild(p: &mut Page, opaque: &BtOpaque, items: &[Vec<u8>]) {
    page::init_special(p, BT_SPECIAL_LEN);
    set_opaque(p, opaque);
    for (i, it) in items.iter().enumerate() {
        page::put_item_at(p, (i + 1) as u16, it);
    }
}

pub fn is_leaf(p: &Page) -> bool {
    get_opaque(p).flags & BTP_LEAF != 0
}

pub fn is_rightmost(p: &Page) -> bool {
    get_opaque(p).next == INVALID_BLOCK
}

/// The 1-based offset of the first data item: 2 when a high key occupies offset
/// 1 (a non-rightmost page), else 1.
pub fn first_data_off(p: &Page) -> u16 {
    if is_rightmost(p) { 1 } else { 2 }
}

/// The high-key item bytes (offset 1) on a non-rightmost page, or `None` on a
/// rightmost page (implicit +infinity bound).
pub fn high_key(p: &Page) -> Option<&[u8]> {
    if is_rightmost(p) {
        None
    } else {
        page::get_item(p, 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::BLCKSZ;

    fn blank() -> Box<Page> {
        Box::new([0u8; BLCKSZ])
    }

    #[test]
    fn opaque_round_trips_and_survives_item_writes() {
        let mut p = blank();
        rebuild(
            &mut p,
            &BtOpaque {
                prev: INVALID_BLOCK,
                next: INVALID_BLOCK,
                level: 0,
                flags: BTP_LEAF | BTP_ROOT,
            },
            &[],
        );
        let mut o = get_opaque(&p);
        assert!(o.flags & BTP_LEAF != 0);
        assert!(o.flags & BTP_ROOT != 0);
        assert_eq!(o.next, INVALID_BLOCK);
        assert!(is_rightmost(&p));
        // Add items; the special region must be untouched by tuple writes.
        assert!(page::insert_item_at(&mut p, 1, b"aaa"));
        assert!(page::insert_item_at(&mut p, 2, b"bbb"));
        o.next = 7;
        o.prev = 3;
        set_opaque(&mut p, &o);
        assert_eq!(get_opaque(&p).next, 7);
        assert_eq!(get_opaque(&p).prev, 3);
        assert!(!is_rightmost(&p));
        assert_eq!(first_data_off(&p), 2);
        assert_eq!(high_key(&p), Some(&b"aaa"[..]));
        // Items still read back intact next to the special region.
        assert_eq!(page::get_item(&p, 1), Some(&b"aaa"[..]));
        assert_eq!(page::get_item(&p, 2), Some(&b"bbb"[..]));
    }

    #[test]
    fn is_initialized_true_so_bufpool_leaves_it_alone() {
        let mut p = blank();
        rebuild(
            &mut p,
            &BtOpaque {
                prev: INVALID_BLOCK,
                next: INVALID_BLOCK,
                level: 2,
                flags: 0,
            },
            &[],
        );
        assert!(page::is_initialized(&p));
        assert!(!is_leaf(&p));
        assert_eq!(get_opaque(&p).level, 2);
    }
}
