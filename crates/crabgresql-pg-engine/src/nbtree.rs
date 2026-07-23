//! The durable B-tree access method: a page-backed, WAL-logged index over an
//! index relfilenode. This milestone serves **equality** probes; the on-disk
//! structure is fully ordered (right-links, high keys, order-preserving keys via
//! [`crate::btkey`]) so range scans can be added later with no format change.
//!
//! Concurrency is a single coarse per-index latch: writers take it exclusively,
//! readers share it, so the tree is quiescent during any one operation. The
//! right-links are still maintained — they are what keeps the tree navigable
//! after a crash interrupts a split (a page that migrated right stays reachable
//! from its left sibling even if the parent downlink record never reached durable
//! WAL), and they let a later milestone drop the coarse latch for Lehman-Yao
//! latch-coupling without touching the format.
//!
//! Visibility is *not* the index's concern: it maps `key -> Tid`, and the caller
//! ([`crate::heap`]) re-fetches each heap tuple and applies the shared MVCC rule,
//! exactly like a PostgreSQL secondary index.

use std::cmp::Ordering;
use std::sync::{Arc, RwLock};

use crabgresql_storage_api::Tid;
use crabgresql_txn::Xid;
use crabgresql_wal::RmgrId;

use crate::EngineInner;
use crate::btkey;
use crate::btpage::{self, BTP_LEAF, BTP_ROOT, BtOpaque, INVALID_BLOCK, META_BLOCK};
use crate::btrec;
use crate::page::{self, BLCKSZ, PAGE_HEADER_LEN};
use crate::smgr::RelFileNode;

/// Usable bytes on a B-tree page (between the header and the special region).
const USABLE: usize = BLCKSZ - PAGE_HEADER_LEN - btpage::BT_SPECIAL_LEN;
/// Per-item line-pointer overhead (an `ItemId` in the slot array).
const LP_OVERHEAD: usize = 4;
/// Largest index item (pointer + key) accepted, matching PostgreSQL's "index row
/// size exceeds btree maximum" cap. Kept at `<= (USABLE - 16) / 4` so that after
/// folding one max-size item into a full page, [`choose_split`] always finds a
/// split point whose feasibility window (width `>= USABLE - 3*maxsize`) is wider
/// than a single item — guaranteeing a balanced split exists and neither half
/// (data + its high key) can exceed [`USABLE`]. See the sizing proof in
/// `choose_split`.
const BT_MAX_ITEM: usize = 2000;

/// A durable B-tree rooted in one index relfilenode. Cheap to construct per
/// operation; all persistent state lives in the buffer pool / WAL keyed by `rel`.
/// The `latch` is shared across every handle for the same index (it lives on the
/// table's index entry), so it actually serializes operations.
pub struct BTree {
    engine: Arc<EngineInner>,
    rel: RelFileNode,
    latch: Arc<RwLock<()>>,
}

/// The outcome of splitting one page: the separator to install in the parent and
/// the new right block, plus whether the split page was the root (so the caller
/// grows a new root instead of touching a parent).
struct SplitResult {
    left_blk: u32,
    right_blk: u32,
    sep_key: Vec<u8>,
    sep_tid: u64,
    was_root: bool,
    level: u32,
}

fn io<T>(r: std::io::Result<T>) -> T {
    r.expect("btree engine I/O error")
}

/// The `(key, packed tid)` routing tuple of an item — `(leaf_key, leaf_tid)` on a
/// leaf, `(internal_key, internal_tid)` on an internal page.
fn route(leaf: bool, item: &[u8]) -> (&[u8], u64) {
    if leaf {
        (btkey::leaf_key(item), btkey::leaf_tid(item).packed())
    } else {
        (btkey::internal_key(item), btkey::internal_tid(item))
    }
}

/// Total order over routing tuples: key bytes first, then packed tid.
fn cmp_route(a: (&[u8], u64), b: (&[u8], u64)) -> Ordering {
    a.0.cmp(b.0).then(a.1.cmp(&b.1))
}

impl BTree {
    pub fn open(engine: Arc<EngineInner>, rel: RelFileNode, latch: Arc<RwLock<()>>) -> BTree {
        BTree {
            engine,
            rel,
            latch,
        }
    }

    fn wal_append(&self, info: u8, payload: &[u8]) -> crabgresql_wal::Lsn {
        self.engine
            .wal
            .append(RmgrId(btrec::RMGR_BTREE.0), info, Xid::INVALID, payload)
    }

    /// Build an empty tree: meta page (block 0) pointing at an empty root leaf
    /// (block 1). Both pages are WAL-logged, so recovery reconstructs them.
    pub fn create(&self) {
        let _w = self.latch.write().unwrap_or_else(|_| panic!("btree latch poisoned"));
        // Root leaf first (block 1), then the meta pointing at it.
        self.write_page_image(
            1,
            &BtOpaque {
                prev: INVALID_BLOCK,
                next: INVALID_BLOCK,
                level: 0,
                flags: BTP_LEAF | BTP_ROOT,
            },
            &[],
        );
        self.write_meta(1, 0);
    }

    /// The `(root_block, level)` recorded on the meta page.
    fn read_meta(&self) -> (u32, u32) {
        let page = io(self.engine.bufpool.pin(self.rel, META_BLOCK));
        page.read(|pg| match page::get_item(pg, 1).and_then(btrec::decode_meta) {
            Some(rl) => rl,
            None => panic!("btree meta page is not initialized"),
        })
    }

    fn write_meta(&self, root: u32, level: u32) {
        let page = io(self.engine.bufpool.pin(self.rel, META_BLOCK));
        page.modify(|pg| {
            btpage::init_meta(pg);
            page::put_item_at(pg, 1, &btrec::encode_meta(root, level));
            let lsn = self.wal_append(btrec::BT_META, &btrec::meta(self.rel, root, level));
            page::set_lsn(pg, lsn.0);
        });
    }

    /// Write a full page image (used to create the root leaf and a grown root).
    fn write_page_image(&self, blk: u32, opaque: &BtOpaque, items: &[Vec<u8>]) {
        let page = io(self.engine.bufpool.pin(self.rel, blk));
        page.modify(|pg| {
            btpage::rebuild(pg, opaque, items);
            let lsn = self.wal_append(
                btrec::BT_PAGE,
                &btrec::page_image(self.rel, blk, opaque, items),
            );
            page::set_lsn(pg, lsn.0);
        });
    }

    // -- Descent -----------------------------------------------------------

    /// Follow right-links from `blk` while the search tuple `s` is `>=` the page's
    /// high key (so the target migrated right, e.g. across an incomplete split).
    /// Returns the block that owns `s`'s key range.
    fn move_right(&self, mut blk: u32, s: (&[u8], u64)) -> u32 {
        loop {
            let page = io(self.engine.bufpool.pin(self.rel, blk));
            let next = page.read(|pg| {
                let Some(hk) = btpage::high_key(pg) else {
                    return None; // rightmost page: unbounded, never step past it
                };
                if cmp_route(s, route(btpage::is_leaf(pg), hk)) != Ordering::Less {
                    Some(btpage::get_opaque(pg).next)
                } else {
                    None
                }
            });
            match next {
                Some(n) => blk = n,
                None => return blk,
            }
        }
    }

    /// The child block on an internal page `pg` for search tuple `s`: the last
    /// downlink whose routing tuple is `<= s` (the leftmost is minus-infinity, so
    /// there is always a match).
    fn internal_pick(pg: &page::Page, s: (&[u8], u64)) -> u32 {
        let start = btpage::first_data_off(pg);
        let maxoff = page::max_offset(pg);
        let mut chosen = start;
        for off in start..=maxoff {
            let item = page::get_item(pg, off).expect("internal downlink slot is normal");
            if cmp_route(route(false, item), s) != Ordering::Greater {
                chosen = off;
            } else {
                break;
            }
        }
        btkey::internal_child(page::get_item(pg, chosen).expect("chosen downlink is normal"))
    }

    /// Descend from the root to the leaf owning search tuple `s`, pushing every
    /// internal block visited onto `stack` (nearest-ancestor last) so a splitting
    /// insert can install downlinks without re-descending.
    fn descend(&self, s: (&[u8], u64), stack: &mut Vec<u32>) -> u32 {
        let (root, _level) = self.read_meta();
        let mut blk = root;
        loop {
            blk = self.move_right(blk, s);
            let page = io(self.engine.bufpool.pin(self.rel, blk));
            let child = page.read(|pg| {
                if btpage::is_leaf(pg) {
                    None
                } else {
                    Some(BTree::internal_pick(pg, s))
                }
            });
            match child {
                None => return blk,
                Some(c) => {
                    stack.push(blk);
                    blk = c;
                }
            }
        }
    }

    // -- Search ------------------------------------------------------------

    /// Every heap `Tid` whose key equals `key`, in `(key, tid)` order, spanning
    /// leaf pages via right-links when a run of duplicates was split apart.
    pub fn search_equal(&self, key: &[u8]) -> Vec<Tid> {
        let _r = self.latch.read().unwrap_or_else(|_| panic!("btree latch poisoned"));
        let s = (key, 0u64); // tid 0 sorts below every real entry (offsets are 1-based)
        let mut blk = {
            let mut stack = Vec::new();
            self.descend(s, &mut stack)
        };
        let mut out = Vec::new();
        loop {
            let page = io(self.engine.bufpool.pin(self.rel, blk));
            let follow = page.read(|pg| {
                let start = btpage::first_data_off(pg);
                let maxoff = page::max_offset(pg);
                let mut saw_greater = false;
                for off in start..=maxoff {
                    let item = page::get_item(pg, off).expect("leaf slot is normal");
                    match btkey::leaf_key(item).cmp(key) {
                        Ordering::Less => continue,
                        Ordering::Equal => out.push(btkey::leaf_tid(item)),
                        Ordering::Greater => {
                            saw_greater = true;
                            break;
                        }
                    }
                }
                // The run may continue on the next leaf unless we already passed
                // it (saw a key > `key`) or this is the rightmost page.
                if !saw_greater && !btpage::is_rightmost(pg) {
                    Some(btpage::get_opaque(pg).next)
                } else {
                    None
                }
            });
            match follow {
                Some(n) => blk = n,
                None => return out,
            }
        }
    }

    // -- Insert ------------------------------------------------------------

    /// Insert `key -> tid`. Duplicates are allowed (the `(key, tid)` order is
    /// total). Panics if the encoded item exceeds the page item limit, matching
    /// PostgreSQL's "index row size exceeds btree maximum".
    pub fn insert(&self, key: &[u8], tid: Tid) {
        let _w = self.latch.write().unwrap_or_else(|_| panic!("btree latch poisoned"));
        let item = btkey::make_leaf_item(tid, key);
        assert!(
            item.len() <= BT_MAX_ITEM,
            "index row size {} exceeds btree maximum {BT_MAX_ITEM}",
            item.len()
        );
        let s = (key, tid.packed());
        let mut stack = Vec::new();
        let leaf = self.descend(s, &mut stack);
        if self.try_plain_insert(leaf, &item, true) {
            return;
        }
        // The leaf is full: split it (folding the new item in), then propagate
        // the resulting downlink up the recorded ancestor stack.
        let mut result = self.split_page(leaf, item);
        loop {
            if result.was_root {
                self.grow_root(&result);
                return;
            }
            let parent = stack
                .pop()
                .expect("a non-root split must have a parent on the descent stack");
            let downlink =
                btkey::make_internal_item(result.right_blk, result.sep_tid, &result.sep_key);
            if self.try_plain_insert(parent, &downlink, false) {
                return;
            }
            result = self.split_page(parent, downlink);
        }
    }

    /// Try to insert `item` into `blk` in sorted position without splitting.
    /// Returns `false` (page full) without modifying the page.
    fn try_plain_insert(&self, blk: u32, item: &[u8], leaf: bool) -> bool {
        let page = io(self.engine.bufpool.pin(self.rel, blk));
        let s = route(leaf, item);
        let (off, fits) = page.read(|pg| {
            let start = btpage::first_data_off(pg);
            let maxoff = page::max_offset(pg);
            let mut off = maxoff + 1;
            for o in start..=maxoff {
                let existing = page::get_item(pg, o).expect("slot is normal");
                if cmp_route(route(leaf, existing), s) == Ordering::Greater {
                    off = o;
                    break;
                }
            }
            (off, item.len() + 4 <= page::free_space(pg))
        });
        if !fits {
            return false;
        }
        page.modify(|pg| {
            page::insert_item_at(pg, off, item);
            let lsn = self.wal_append(btrec::BT_INSERT, &btrec::insert(self.rel, blk, off, item));
            page::set_lsn(pg, lsn.0);
        });
        true
    }

    /// Split `blk`, folding `new_item` into it, and log the split as full page
    /// images. Returns the separator + new right block for the parent.
    fn split_page(&self, blk: u32, new_item: Vec<u8>) -> SplitResult {
        let page = io(self.engine.bufpool.pin(self.rel, blk));
        let (opaque, orig_high, data) = page.read(read_page_items);
        let leaf = opaque.flags & BTP_LEAF != 0;
        let was_root = opaque.flags & BTP_ROOT != 0;

        // Fold the new item into the sorted data list.
        let mut merged = data;
        let s = route(leaf, &new_item);
        let pos = merged
            .iter()
            .position(|it| cmp_route(route(leaf, it), s) == Ordering::Greater)
            .unwrap_or(merged.len());
        merged.insert(pos, new_item);

        let sizes: Vec<usize> = merged.iter().map(|it| it.len() + 4).collect();
        let split_at = choose_split(&sizes);

        // Partition into left/right data and derive the separator pushed up.
        let (left_data, right_data, sep_key, sep_tid) = if leaf {
            // Leaf split: the boundary key stays as the right page's first entry;
            // the left high key copies it.
            let left = merged[..split_at].to_vec();
            let right = merged[split_at..].to_vec();
            let (rk, rt) = route(true, &right[0]);
            let sep_key = rk.to_vec();
            (left, right, sep_key, rt)
        } else {
            // Internal split: the pivot's key moves up; its child becomes the
            // right page's leftmost (minus-infinity) downlink.
            let pivot = merged[split_at].clone();
            let pivot_child = btkey::internal_child(&pivot);
            let pk = btkey::internal_key(&pivot).to_vec();
            let pt = btkey::internal_tid(&pivot);
            let left = merged[..split_at].to_vec();
            let mut right = vec![btkey::make_internal_item(pivot_child, 0, &[])];
            right.extend_from_slice(&merged[split_at + 1..]);
            (left, right, pk, pt)
        };

        let right_blk = io(self.engine.bufpool.smgr().extend(self.rel));

        // Left keeps its old block, gains a high key equal to the separator, and
        // points at the new right sibling. The right sibling inherits the old
        // high key (or none, when the split page was rightmost).
        let left_high = if leaf {
            right_data[0].clone()
        } else {
            btkey::make_internal_item(0, sep_tid, &sep_key)
        };
        let mut left_items = vec![left_high];
        left_items.extend_from_slice(&left_data);
        let mut right_items = Vec::new();
        if let Some(h) = &orig_high {
            right_items.push(h.clone());
        }
        right_items.extend_from_slice(&right_data);

        // Both halves (data + high key) must fit the page; the choose_split sizing
        // proof guarantees this, so a failure here is a sizing-invariant regression
        // (which would otherwise silently overflow the page in `put_item_at`).
        debug_assert!(page_bytes(&left_items) <= USABLE, "btree split: left half overflows");
        debug_assert!(page_bytes(&right_items) <= USABLE, "btree split: right half overflows");

        let leaf_bit = if leaf { BTP_LEAF } else { 0 };
        let left_opaque = BtOpaque {
            prev: opaque.prev,
            next: right_blk,
            level: opaque.level,
            flags: leaf_bit,
        };
        let right_opaque = BtOpaque {
            prev: blk,
            next: opaque.next,
            level: opaque.level,
            flags: leaf_bit,
        };
        let old_right_sibling = opaque.next;

        let payload = btrec::split(
            self.rel,
            blk,
            right_blk,
            &left_opaque,
            &right_opaque,
            &left_items,
            &right_items,
            old_right_sibling,
        );
        let lsn = self.wal_append(btrec::BT_SPLIT, &payload);

        let left_page = io(self.engine.bufpool.pin(self.rel, blk));
        left_page.modify(|pg| {
            btpage::rebuild(pg, &left_opaque, &left_items);
            page::set_lsn(pg, lsn.0);
        });
        let right_page = io(self.engine.bufpool.pin(self.rel, right_blk));
        right_page.modify(|pg| {
            btpage::rebuild(pg, &right_opaque, &right_items);
            page::set_lsn(pg, lsn.0);
        });
        if old_right_sibling != INVALID_BLOCK {
            let sib = io(self.engine.bufpool.pin(self.rel, old_right_sibling));
            sib.modify(|pg| {
                let mut o = btpage::get_opaque(pg);
                o.prev = right_blk;
                btpage::set_opaque(pg, &o);
                page::set_lsn(pg, lsn.0);
            });
        }

        SplitResult {
            left_blk: blk,
            right_blk,
            sep_key,
            sep_tid,
            was_root,
            level: opaque.level,
        }
    }

    /// Grow a new root above a just-split old root: a new internal page with two
    /// downlinks (minus-infinity -> old block, separator -> right block), then
    /// repoint the meta page at it.
    fn grow_root(&self, split: &SplitResult) {
        let new_root = io(self.engine.bufpool.smgr().extend(self.rel));
        let downlinks = vec![
            btkey::make_internal_item(split.left_blk, 0, &[]),
            btkey::make_internal_item(split.right_blk, split.sep_tid, &split.sep_key),
        ];
        self.write_page_image(
            new_root,
            &BtOpaque {
                prev: INVALID_BLOCK,
                next: INVALID_BLOCK,
                level: split.level + 1,
                flags: BTP_ROOT,
            },
            &downlinks,
        );
        self.write_meta(new_root, split.level + 1);
    }

    // -- Delete ------------------------------------------------------------

    /// Remove the exact leaf entry `(key, tid)` if present (called by vacuum when
    /// a heap version is reclaimed, so a reused heap slot is never reachable by a
    /// stale key). No page merging — the slot is removed and the array stays
    /// contiguous; empty pages linger until a future milestone reclaims them.
    pub fn delete(&self, key: &[u8], tid: Tid) {
        let _w = self.latch.write().unwrap_or_else(|_| panic!("btree latch poisoned"));
        let s = (key, tid.packed());
        let mut stack = Vec::new();
        let mut blk = self.descend(s, &mut stack);
        loop {
            let page = io(self.engine.bufpool.pin(self.rel, blk));
            let outcome = page.read(|pg| {
                let start = btpage::first_data_off(pg);
                let maxoff = page::max_offset(pg);
                for off in start..=maxoff {
                    let item = page::get_item(pg, off).expect("leaf slot is normal");
                    match cmp_route(route(true, item), s) {
                        Ordering::Less => continue,
                        Ordering::Equal => return Found(off),
                        Ordering::Greater => return Done,
                    }
                }
                // Fell off the end without passing `s`; the entry may be on the
                // next leaf (a split could have moved it right).
                if !btpage::is_rightmost(pg) {
                    Continue(btpage::get_opaque(pg).next)
                } else {
                    Done
                }
            });
            match outcome {
                Found(off) => {
                    page.modify(|pg| {
                        page::remove_item_at(pg, off);
                        let lsn =
                            self.wal_append(btrec::BT_DELETE, &btrec::delete(self.rel, blk, off));
                        page::set_lsn(pg, lsn.0);
                    });
                    return;
                }
                Continue(n) => blk = n,
                Done => return,
            }
        }
    }
}

/// Local enum for [`BTree::delete`]'s per-page scan outcome.
enum DeleteScan {
    Found(u16),
    Continue(u32),
    Done,
}
use DeleteScan::{Continue, Done, Found};

/// A page decomposed for a split: its opaque, its high key (if non-rightmost),
/// and its data items (excluding the high key).
type PageItems = (BtOpaque, Option<Vec<u8>>, Vec<Vec<u8>>);

/// Total on-page bytes a set of items occupies: each item's bytes plus its line
/// pointer. Used only by the split fit-check debug assertions.
fn page_bytes(items: &[Vec<u8>]) -> usize {
    items.iter().map(|it| it.len() + LP_OVERHEAD).sum()
}

/// Read a page's opaque, its high key (if non-rightmost), and its data items
/// (excluding the high key), each cloned out so the caller can rebuild the page.
fn read_page_items(pg: &page::Page) -> PageItems {
    let opaque = btpage::get_opaque(pg);
    let high = if btpage::is_rightmost(pg) {
        None
    } else {
        Some(page::get_item(pg, 1).expect("non-rightmost page has a high key").to_vec())
    };
    let start = btpage::first_data_off(pg);
    let maxoff = page::max_offset(pg);
    let data = (start..=maxoff)
        .map(|o| page::get_item(pg, o).expect("data slot is normal").to_vec())
        .collect();
    (opaque, high, data)
}

/// Choose a split index `p` in `[1, n-1]` that most evenly balances bytes while
/// leaving each side room for its high key.
///
/// Sizing proof (why this never fails): each `sizes[i] <= BT_MAX_ITEM + LP_OVERHEAD`.
/// `side_cap` reserves a whole max-size item (key + its line pointer) for the high
/// key each side carries, so any `p` with both prefix sums `<= side_cap` yields
/// halves that fit `USABLE`. Such a `p` exists because the feasible window
/// `[total - side_cap, side_cap]` has width `2*side_cap - total >= USABLE - 3*(BT_MAX_ITEM+LP_OVERHEAD)`,
/// which — with `BT_MAX_ITEM <= (USABLE-16)/4` — exceeds one item's size, so a
/// prefix boundary (steps of `<= one item`) always lands inside it.
fn choose_split(sizes: &[usize]) -> usize {
    let n = sizes.len();
    debug_assert!(n >= 2, "cannot split a page with fewer than two items");
    // Reserve room for the high key (an item plus its line pointer) each side carries.
    let side_cap = USABLE - (BT_MAX_ITEM + LP_OVERHEAD);
    let prefix: Vec<usize> = std::iter::once(0)
        .chain(sizes.iter().scan(0usize, |acc, &s| {
            *acc += s;
            Some(*acc)
        }))
        .collect();
    let total = prefix[n];
    let mut best = 0usize;
    let mut best_diff = usize::MAX;
    // Consider each split point p in [1, n-1]: left = prefix[p], right = the rest.
    for (p, &left) in prefix.iter().enumerate().skip(1).take(n - 1) {
        let right = total - left;
        if left <= side_cap && right <= side_cap {
            let diff = left.abs_diff(right);
            if diff < best_diff {
                best_diff = diff;
                best = p;
            }
        }
    }
    assert!(best != 0, "btree split found no feasible split point");
    best
}
