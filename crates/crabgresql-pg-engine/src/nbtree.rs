//! The durable B-tree access method: a page-backed, WAL-logged index over an
//! index relfilenode. Equality and range probes are served by one routine,
//! [`BTree::search_range`]: the on-disk structure is fully ordered (right-links,
//! high keys, order-preserving keys via [`crate::btkey`]), so neither needed a
//! format change.
//!
//! TODO: serve *ordered* scans — a scan that hands its rows to the caller in
//! index order, letting an `ORDER BY` on the key columns drop its sort.
//!
//! TODO(perf): a search materializes every matching `Tid` into a `Vec` before
//! returning. That is what lets the coarse latch and every page pin be released
//! at the end of the call, which the current locking design relies on; a
//! streaming cursor has to hold them across the caller's iteration instead.
//! Until then a range wide enough to select most of the relation buffers one
//! tid per matching row.
//!
//! Concurrency is a single coarse per-index latch: writers take it exclusively,
//! readers share it, so the tree is quiescent during any one operation. The
//! right-links are still maintained — they are what keeps the tree navigable
//! after a crash interrupts a split (a page that migrated right stays reachable
//! from its left sibling even if the parent downlink record never reached durable
//! WAL), and they are what a Lehman-Yao latch-coupling descent moves right
//! through, so dropping the coarse latch needs no format change.
//!
//! TODO(perf): drop the coarse latch for Lehman-Yao latch-coupling, so readers
//! and writers no longer serialize on one lock per index.
//!
//! Visibility is *not* the index's concern: it maps `key -> Tid`, and the caller
//! ([`crate::heap`]) re-fetches each heap tuple and applies the shared MVCC rule,
//! exactly like a PostgreSQL secondary index.

use std::cmp::Ordering;
use std::ops::Bound;
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

/// The size a `key -> tid` entry would occupy, and the largest one this tree
/// accepts. A caller checks the pair *before* [`BTree::insert`] because the
/// error PostgreSQL raises names the index, the heap tuple and the relation —
/// none of which a tree knows. Cheap: the item's size is its key plus the tid,
/// so nothing is encoded to ask.
pub fn item_size(key: &[u8]) -> (usize, usize) {
    (8 + key.len(), BT_MAX_ITEM)
}

/// A durable B-tree rooted in one index relfilenode. Cheap to construct per
/// operation; all persistent state lives in the buffer pool / WAL keyed by `rel`.
/// The `latch` is shared across every handle for the same index (it lives on the
/// table's index entry), so it actually serializes operations.
pub struct BTree {
    engine: Arc<EngineInner>,
    rel: RelFileNode,
    latch: Arc<RwLock<()>>,
    /// Whether this index's ops skip the WAL (an `Unlogged` table's index —
    /// file-backed but WAL-silent, rebuilt on crash). `wal_append` then returns
    /// `Lsn(0)`, exactly like `HeapTable::log`, so eviction is a no-op flush.
    wal_skipped: bool,
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

/// What the page a descent is standing on says to do next.
///
/// The three answers are decided together, under one pin, because they are one
/// question about one page: the high key says whether this page still owns the
/// search tuple, and only if it does are its downlinks meaningful.
enum Step {
    /// The search tuple is `>=` this page's high key, so the target migrated
    /// right (e.g. across an incomplete split) and this page does not own it.
    ///
    /// Not reached by any test in this crate, and not reachable under the coarse
    /// latch at all: a writer holds it exclusively, so no reader ever observes a
    /// split in progress. It is here for the states the module header describes
    /// — a crash between a split and its parent downlink, and latch-coupling
    /// once the coarse latch is dropped — which means it is verified by reading,
    /// not by running.
    Right(u32),
    /// Follow this downlink.
    Down(u32),
    /// This page is the leaf that owns the range.
    Leaf,
}

impl BTree {
    pub fn open(
        engine: Arc<EngineInner>,
        rel: RelFileNode,
        latch: Arc<RwLock<()>>,
        wal_skipped: bool,
    ) -> BTree {
        BTree {
            engine,
            rel,
            latch,
            wal_skipped,
        }
    }

    fn wal_append(&self, info: u8, payload: &[u8]) -> crabgresql_wal::Lsn {
        if self.wal_skipped {
            return crabgresql_wal::Lsn(0);
        }
        self.engine
            .wal
            .append(RmgrId(btrec::RMGR_BTREE.0), info, Xid::INVALID, payload)
            .end
    }

    /// Hold the checkpointer off across a multi-page update. `None` for a
    /// WAL-skipped (`UNLOGGED`/`TEMPORARY`) tree, which writes no record and so
    /// has nothing a redo point could strand — that keeps bulk unlogged index
    /// builds off the barrier entirely.
    fn checkpoint_delay(&self) -> Option<crabgresql_wal::CheckpointDelay<'_>> {
        (!self.wal_skipped).then(|| self.engine.wal.delay_checkpoint())
    }

    /// Build an empty tree: meta page (block 0) pointing at an empty root leaf
    /// (block 1). Both pages are WAL-logged, so recovery reconstructs them.
    pub fn create(&self) {
        let _w = self
            .latch
            .write()
            .unwrap_or_else(|_| panic!("btree latch poisoned"));
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
        page.read(
            |pg| match page::get_item(pg, 1).and_then(btrec::decode_meta) {
                Some(rl) => rl,
                None => panic!("btree meta page is not initialized"),
            },
        )
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
            let page = io(self.engine.bufpool.pin(self.rel, blk));
            let step = page.read(|pg| {
                // The high key is asked first and answered before anything else
                // on this page is consulted. A page whose range no longer covers
                // `s` has downlinks that route some *other* range, and picking
                // one of those lands the descent in a subtree that cannot
                // contain the key — a search that quietly returns too few rows
                // rather than an error. Right-stepping is the whole reason this
                // check exists; letting it run second would defeat it.
                if let Some(hk) = btpage::high_key(pg)
                    && cmp_route(s, route(btpage::is_leaf(pg), hk)) != Ordering::Less
                {
                    // A rightmost page has no high key at all, so this can never
                    // step past the end of the level.
                    return Step::Right(btpage::get_opaque(pg).next);
                }
                if btpage::is_leaf(pg) {
                    Step::Leaf
                } else {
                    Step::Down(BTree::internal_pick(pg, s))
                }
            });
            match step {
                Step::Right(next) => blk = next,
                Step::Down(child) => {
                    stack.push(blk);
                    blk = child;
                }
                Step::Leaf => return blk,
            }
        }
    }

    // -- Search ------------------------------------------------------------

    /// Every heap `Tid` whose key lies in `range`, in `(key, tid)` order,
    /// spanning leaf pages via right-links.
    pub fn search_range(&self, range: &KeyRange) -> Vec<Tid> {
        let _r = self
            .latch
            .read()
            .unwrap_or_else(|_| panic!("btree latch poisoned"));
        // Descend to the first key the scan could accept. A lower bound always
        // starts with the prefix (it is the prefix plus one column), so it is
        // never to the left of it; without one the prefix itself is the start.
        // `tid = 0` sorts below every real entry (offsets are 1-based), so the
        // descent lands at the start of a run rather than inside it.
        let start_key = match &range.lower {
            Bound::Included(b) | Bound::Excluded(b) => &b[..],
            Bound::Unbounded => &range.prefix[..],
        };
        let mut blk = {
            let mut stack = Vec::new();
            self.descend((start_key, 0u64), &mut stack)
        };
        let mut out = Vec::new();
        loop {
            let page = io(self.engine.bufpool.pin(self.rel, blk));
            let follow = page.read(|pg| {
                let start = btpage::first_data_off(pg);
                let maxoff = page::max_offset(pg);
                for off in start..=maxoff {
                    let item = page::get_item(pg, off).expect("leaf slot is normal");
                    let key = btkey::leaf_key(item);
                    // Keys only grow from here, so once one is past the range's
                    // end no later entry on this or any later page can match.
                    if range.past_end(key) {
                        return None;
                    }
                    // Entries *below* the range are skipped rather than stopped
                    // on: the descent lands on the page owning the start key,
                    // which normally also holds smaller keys before it.
                    if range.contains(key) {
                        out.push(btkey::leaf_tid(item));
                    }
                }
                // Fell off the end of the page still inside the range: the run
                // continues on the next leaf, unless this is the rightmost one.
                (!btpage::is_rightmost(pg)).then(|| btpage::get_opaque(pg).next)
            });
            match follow {
                Some(n) => blk = n,
                None => return out,
            }
        }
    }

    // -- Insert ------------------------------------------------------------

    /// Insert `key -> tid`. Duplicates are allowed (the `(key, tid)` order is
    /// total).
    ///
    /// Panics if the encoded item exceeds the page item limit. That is a
    /// backstop, not the error path: an oversized key is an ordinary user
    /// mistake, so callers ask [`item_size`] first and raise `54000` with the
    /// index and heap tuple named. Reaching this assert means one did not.
    pub fn insert(&self, key: &[u8], tid: Tid) {
        let _w = self
            .latch
            .write()
            .unwrap_or_else(|_| panic!("btree latch poisoned"));
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
        debug_assert!(
            page_bytes(&left_items) <= USABLE,
            "btree split: left half overflows"
        );
        debug_assert!(
            page_bytes(&right_items) <= USABLE,
            "btree split: right half overflows"
        );

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
        // A split is one record spanning three pages, so it is the one writer
        // with a window between "record appended" and "every page it describes
        // is dirty" — `flush_all` takes frame locks one at a time and can
        // interleave between the `modify` calls below. Hold the checkpointer off
        // across the whole window: a redo point sampled inside it would sit above
        // the record while two of the three pages are still clean, leaving the
        // split neither on disk nor in the replayed suffix, while the parent
        // downlink (a later `BT_INSERT`) *is* replayed — descent into an empty
        // page.
        // Pin all three pages BEFORE taking the barrier. A `pin` that misses runs
        // clock-sweep eviction, which fsyncs the WAL and writes the victim out —
        // holding the barrier across that would stall the checkpointer, and with
        // it (via the barrier's writer queue) every other index writer, for the
        // length of an fsync chain. Pinning first leaves the barrier spanning
        // only in-memory work.
        let left_page = io(self.engine.bufpool.pin(self.rel, blk));
        let right_page = io(self.engine.bufpool.pin(self.rel, right_blk));
        let sibling = (old_right_sibling != INVALID_BLOCK)
            .then(|| io(self.engine.bufpool.pin(self.rel, old_right_sibling)));

        let _delay = self.checkpoint_delay();

        // Append inside the left page's `modify`, matching every other page
        // writer here and in the heap: the record and the first dirty page then
        // become visible under one frame lock.
        let lsn = left_page.modify(|pg| {
            btpage::rebuild(pg, &left_opaque, &left_items);
            let lsn = self.wal_append(btrec::BT_SPLIT, &payload);
            page::set_lsn(pg, lsn.0);
            lsn
        });
        right_page.modify(|pg| {
            btpage::rebuild(pg, &right_opaque, &right_items);
            page::set_lsn(pg, lsn.0);
        });
        if let Some(sib) = sibling {
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
    /// contiguous.
    ///
    /// TODO: reclaim pages emptied by deletes; an empty page stays linked into
    /// its level, so every search over its range still steps through it.
    pub fn delete(&self, key: &[u8], tid: Tid) {
        let _w = self
            .latch
            .write()
            .unwrap_or_else(|_| panic!("btree latch poisoned"));
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

/// The stretch of key space one search covers: an equality-pinned `prefix` (the
/// leading key columns, empty for none) and optional bounds on the column right
/// after it, each stated as the whole key it would appear in — `prefix`
/// followed by that column's bytes.
///
/// **The bounds are prefix-wise, not bytewise.** On a key `(a, b, c)`, the
/// predicate `a = 1 AND b > 5` has an exclusive lower bound of
/// `enc(1) ++ enc(5)`, and every row with `b = 5` encodes to something bytewise
/// *greater* than that (its `c` follows) while satisfying neither `b > 5` nor
/// the query. So an exclusive bound excludes the whole run of keys extending
/// its bytes, and an inclusive upper bound includes it:
///
/// * lower `Included(b)`: `key >= b`
/// * lower `Excluded(b)`: `key > b` **and** `!key.starts_with(b)`
/// * upper `Included(b)`: `key < b` **or** `key.starts_with(b)`
/// * upper `Excluded(b)`: `key < b`
///
/// Direction is not this type's business: a `DESC` key column is stored
/// inverted (see [`crate::btkey`]), so its builder has already swapped the two
/// bounds and everything here reads forward.
///
/// [`KeyRange::contains`] is the whole rule in one place, which is what lets a
/// caller re-check a fetched row against exactly what the scan selected.
pub struct KeyRange {
    pub prefix: Vec<u8>,
    pub lower: Bound<Vec<u8>>,
    pub upper: Bound<Vec<u8>>,
}

impl KeyRange {
    /// A range covering exactly the keys starting with `prefix` — a prefix probe
    /// when `prefix` is shorter than a whole key, and **equality** when it is a
    /// whole one: [`crate::btkey`] encodes each column prefix-free, so no stored
    /// key of the same column count can start with another one's bytes and still
    /// differ from it. That is what lets one traversal serve both, rather than
    /// two that could drift apart on the boundary rules.
    pub fn prefix(prefix: Vec<u8>) -> KeyRange {
        KeyRange {
            prefix,
            lower: Bound::Unbounded,
            upper: Bound::Unbounded,
        }
    }

    /// Whether `key` is in the range.
    pub fn contains(&self, key: &[u8]) -> bool {
        key.starts_with(&self.prefix) && self.within_lower(key) && self.within_upper(key)
    }

    fn within_lower(&self, key: &[u8]) -> bool {
        match &self.lower {
            Bound::Unbounded => true,
            Bound::Included(b) => key >= &b[..],
            // A key that starts with `b` carries `b`'s value in the bounded
            // column and a further column after it — the value the bound
            // excludes.
            Bound::Excluded(b) => key > &b[..] && !key.starts_with(b),
        }
    }

    fn within_upper(&self, key: &[u8]) -> bool {
        match &self.upper {
            Bound::Unbounded => true,
            // Mirror of the lower `Excluded` case: those extensions of `b` are
            // the rows holding exactly the bound's value, which an inclusive
            // bound keeps even though their bytes run past it.
            Bound::Included(b) => key < &b[..] || key.starts_with(b),
            Bound::Excluded(b) => key < &b[..],
        }
    }

    /// Whether `key` is past everything the range can still accept — so a scan
    /// reading keys in order can stop rather than skip. True once the key leaves
    /// the prefix's stretch *upward* or fails the upper bound; a key below the
    /// range is neither.
    fn past_end(&self, key: &[u8]) -> bool {
        (!key.starts_with(&self.prefix) && key > &self.prefix[..]) || !self.within_upper(key)
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
        Some(
            page::get_item(pg, 1)
                .expect("non-rightmost page has a high key")
                .to_vec(),
        )
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    use crabgresql_storage_api::{
        Column, IndexConstraint, IndexKey, IndexMetadata, IndexMethod, TableEngine, TableSchema,
    };
    use crabgresql_txn::{CommandId, CommitSink, TransactionManager, TxnFinalize};
    use crabgresql_types::{PgType, Value};
    use crabgresql_wal::Wal;

    use super::*;
    use crate::PgEngine;

    /// The ordering a checkpoint must establish: **after `flush_all` completes,
    /// no frame is still dirty at or below the redo point sampled before it.**
    ///
    /// A dirty frame with `pd_lsn <= redo` is a change that a bounded replay
    /// would skip (its record starts below `redo`) and that was never written
    /// back — silently lost. `pd_lsn > redo` is fine: the record was appended
    /// after the sample, so replay reapplies it.
    ///
    /// Every other page writer gets this for free by appending *inside* the
    /// `modify` closure, under the frame lock. A B-tree split cannot: it is one
    /// record over three pages, dirtied in three separate critical sections, and
    /// `flush_all` takes frame locks one at a time. `CheckpointDelay` is what
    /// closes that window.
    ///
    /// Checking the invariant in-process catches *every* violation under load,
    /// rather than only one that happens to survive to the final checkpoint of a
    /// crash-and-replay test.
    ///
    /// Honest scope: this is a load test against a real interleaving, not a
    /// proof. The window it hunts is roughly a microsecond wide against a much
    /// longer `flush_all` scan, so removing the delay does **not** reliably make
    /// it red. What actually establishes the fix is that `redo_point` provably
    /// cannot return while a delay is held (`crabgresql-wal`'s
    /// `redo_point_blocks_until_every_delay_is_released`) plus this function
    /// holding one across the whole three-page window. This test is the
    /// regression guard for a change that widens the window *systematically*.
    #[test]
    fn no_page_stays_dirty_at_or_below_a_sampled_redo_point() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let (engine, clog, next_xid) = PgEngine::open_recovered_with_pool(
            dir.path(),
            Arc::clone(&wal),
            crate::BufferPoolPolicy::minimal(),
        )?;
        let sink: Arc<dyn CommitSink> = Arc::clone(&wal) as Arc<dyn CommitSink>;
        let mut tm = TransactionManager::new_recovered(sink, clog, next_xid);
        tm.set_finalize(Arc::clone(&engine) as Arc<dyn TxnFinalize>);

        let table =
            engine.create_table(TableSchema::new("t", vec![Column::new("id", PgType::Int4)]))?;
        engine.create_index(
            "public",
            "t",
            IndexMetadata {
                name: "t_id_idx".into(),
                method: IndexMethod::BTree,
                keys: vec![IndexKey {
                    column: 0,
                    descending: false,
                    nulls_first: false,
                }],
                unique: false,
                nulls_distinct: true,
                constraint: Some(IndexConstraint::Unique),
            },
        )?;

        const ROWS: i32 = 6_000;
        let done = Arc::new(AtomicBool::new(false));
        let x = tm.allocate_xid();

        std::thread::scope(|s| -> anyhow::Result<()> {
            let checkpointer = {
                let (engine, wal, done) =
                    (Arc::clone(&engine), Arc::clone(&wal), Arc::clone(&done));
                s.spawn(move || -> anyhow::Result<()> {
                    let mut rounds = 0u64;
                    while !done.load(AtomicOrdering::SeqCst) {
                        // Sample redo BEFORE flushing — the only order that makes
                        // "flush_all visited the frame before the append implies
                        // LSN > redo" a complete argument.
                        let redo = wal.redo_point()?;
                        engine.bufpool().flush_all()?;
                        let stranded: Vec<u64> = engine
                            .bufpool()
                            .dirty_page_lsns()
                            .into_iter()
                            .filter(|&lsn| lsn <= redo.0)
                            .collect();
                        assert!(
                            stranded.is_empty(),
                            "pages {stranded:?} are dirty at or below redo {redo} \
                             after flush_all: neither on disk nor replayed"
                        );
                        rounds += 1;
                    }
                    assert!(rounds > 0, "the checkpointer never ran a round");
                    Ok(())
                })
            };

            // Shuffled order so splits happen mid-tree, not only at the right edge.
            let ctx = tm.context(x, CommandId::FIRST);
            let mut id = 1i32;
            for _ in 0..ROWS {
                table.insert(vec![Value::Int4(id)], &ctx)?;
                id = (id + 1237) % ROWS;
            }
            done.store(true, AtomicOrdering::SeqCst);
            checkpointer
                .join()
                .map_err(|_| anyhow::anyhow!("checkpointer thread panicked"))?
        })?;
        tm.commit(x)?;

        Ok(())
    }
}
