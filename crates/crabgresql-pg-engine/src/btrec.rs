//! Payload encode/decode for the B-tree resource manager's WAL records, shared
//! by `nbtree` (which logs) and `btredo` (which replays). Little-endian, mirrors
//! the heap's `rec` module.
//!
//! Split and new-root records carry *full page images* (every item + the opaque
//! fields) rather than PostgreSQL's incremental right-half logging, which makes
//! replay a plain page rebuild, unmistakably idempotent under the LSN gate.
//!
//! TODO(perf): log only the items that moved to the right half instead of both
//! full page images — the record carries every item of the page that split,
//! about twice what replay needs.

use crabgresql_wal::RmgrId;

use crate::btpage::BtOpaque;
use crate::rec::R;
use crate::smgr::RelFileNode;

/// The B-tree access method's resource-manager id (`0..10` reserved; HEAP is 10).
pub const RMGR_BTREE: RmgrId = RmgrId(11);

pub const BT_INSERT: u8 = 1;
pub const BT_SPLIT: u8 = 2;
/// A full single-page image (opaque + every item). Used to create the initial
/// root leaf and to write a freshly grown root — any page built from scratch.
pub const BT_PAGE: u8 = 3;
pub const BT_META: u8 = 4;
pub const BT_DELETE: u8 = 5;

/// Magic + version guarding the meta page's record, so a corrupt/legacy meta
/// page is caught rather than silently misread.
const META_MAGIC: u32 = 0x0053_5442; // "BTS\0" little-endian-ish tag
const META_VERSION: u32 = 1;

struct W(Vec<u8>);
impl W {
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
    fn opaque(&mut self, o: &BtOpaque) {
        self.u32(o.prev);
        self.u32(o.next);
        self.u32(o.level);
        self.u16(o.flags);
    }
    fn items(&mut self, items: &[Vec<u8>]) {
        self.u32(items.len() as u32);
        for it in items {
            self.bytes(it);
        }
    }
}

/// Read an opaque written by [`W::opaque`].
pub fn read_opaque(r: &mut R) -> BtOpaque {
    BtOpaque {
        prev: r.u32(),
        next: r.u32(),
        level: r.u32(),
        flags: r.u16(),
    }
}

/// Read an item vector written by [`W::items`].
pub fn read_items(r: &mut R) -> Vec<Vec<u8>> {
    let n = r.u32();
    (0..n).map(|_| r.bytes().to_vec()).collect()
}

// -- Meta page record: the single item on block 0 carrying the root pointer.

pub fn encode_meta(root: u32, level: u32) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u32(META_MAGIC);
    w.u32(META_VERSION);
    w.u32(root);
    w.u32(level);
    w.0
}

/// The `(root, level)` a meta item encodes, or `None` if the magic/version does
/// not match (a not-yet-initialized or foreign page).
pub fn decode_meta(item: &[u8]) -> Option<(u32, u32)> {
    if item.len() < 16 {
        return None;
    }
    let mut r = R::new(item);
    if r.u32() != META_MAGIC || r.u32() != META_VERSION {
        return None;
    }
    Some((r.u32(), r.u32()))
}

// -- WAL record builders. `off` fields are 1-based line-pointer numbers.

pub fn insert(rel: RelFileNode, block: u32, off: u16, item: &[u8]) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u32(rel.0);
    w.u32(block);
    w.u16(off);
    w.bytes(item);
    w.0
}

pub fn delete(rel: RelFileNode, block: u32, off: u16) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u32(rel.0);
    w.u32(block);
    w.u16(off);
    w.0
}

#[allow(clippy::too_many_arguments)]
pub fn split(
    rel: RelFileNode,
    left_blk: u32,
    right_blk: u32,
    left_opaque: &BtOpaque,
    right_opaque: &BtOpaque,
    left_items: &[Vec<u8>],
    right_items: &[Vec<u8>],
    old_right_sibling: u32,
) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u32(rel.0);
    w.u32(left_blk);
    w.u32(right_blk);
    w.opaque(left_opaque);
    w.opaque(right_opaque);
    w.items(left_items);
    w.items(right_items);
    w.u32(old_right_sibling);
    w.0
}

/// A full single-page image: the block, its opaque, and every item in order.
pub fn page_image(rel: RelFileNode, block: u32, opaque: &BtOpaque, items: &[Vec<u8>]) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u32(rel.0);
    w.u32(block);
    w.opaque(opaque);
    w.items(items);
    w.0
}

pub fn meta(rel: RelFileNode, root: u32, level: u32) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u32(rel.0);
    w.u32(root);
    w.u32(level);
    w.0
}
