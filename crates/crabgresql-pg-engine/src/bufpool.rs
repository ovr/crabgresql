//! A fixed-frame buffer pool with clock-sweep eviction.
//!
//! Enforces the write-ahead rule: a dirty page is never written back to its
//! relation file until the WAL is flushed up to that page's `pd_lsn`
//! ([`crabgresql_wal`] module docs). The pool is fully synchronous — the
//! executor calls the heap AM on its own threads — and every field is behind a
//! lock, so it is `Send + Sync`.
//!
//! Simplicity note: eviction I/O runs while the mapping lock is held, which
//! serializes page faults. That is correct and fine for a first cut given the
//! pool is far larger than the number of concurrently pinned pages; a
//! finer-grained scheme is a later optimization.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crabgresql_wal::{Lsn, Wal};

use crate::page::{self, BLCKSZ, Page};
use crate::smgr::{RelFileNode, StorageManager};

/// Identifies a page: which relation, which block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BufferTag {
    pub rel: RelFileNode,
    pub block: u32,
}

struct Frame {
    tag: Option<BufferTag>,
    data: Box<Page>,
    dirty: bool,
    pins: u32,
    ref_bit: bool,
}

pub struct BufferPool {
    frames: Vec<Mutex<Frame>>,
    map: Mutex<HashMap<BufferTag, usize>>,
    hand: AtomicUsize,
    smgr: Arc<StorageManager>,
    wal: Arc<Wal>,
}

impl BufferPool {
    pub fn new(nframes: usize, smgr: Arc<StorageManager>, wal: Arc<Wal>) -> BufferPool {
        let frames = (0..nframes)
            .map(|_| {
                Mutex::new(Frame {
                    tag: None,
                    data: Box::new([0u8; BLCKSZ]),
                    dirty: false,
                    pins: 0,
                    ref_bit: false,
                })
            })
            .collect();
        BufferPool {
            frames,
            map: Mutex::new(HashMap::new()),
            hand: AtomicUsize::new(0),
            smgr,
            wal,
        }
    }

    pub fn smgr(&self) -> &Arc<StorageManager> {
        &self.smgr
    }

    /// Pin the page for `(rel, block)`, reading it in (or initializing a fresh
    /// block) on a miss. The returned guard keeps the page resident until
    /// dropped.
    pub fn pin(&self, rel: RelFileNode, block: u32) -> std::io::Result<PinnedPage<'_>> {
        let tag = BufferTag { rel, block };
        let mut map = self.map.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
        if let Some(&idx) = map.get(&tag) {
            let mut fr = self.frames[idx]
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"));
            fr.pins += 1;
            fr.ref_bit = true;
            drop(fr);
            return Ok(PinnedPage { pool: self, idx });
        }
        // Miss: find an unpinned victim via clock sweep.
        let idx = loop {
            let i = self.hand.fetch_add(1, Ordering::Relaxed) % self.frames.len();
            let mut fr = self.frames[i]
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"));
            if fr.pins > 0 {
                continue;
            }
            if fr.ref_bit {
                fr.ref_bit = false;
                continue;
            }
            // Evict whatever this frame held.
            if let Some(old) = fr.tag.take() {
                if fr.dirty {
                    let lsn = page::get_lsn(&fr.data);
                    self.wal
                        .flush(Lsn(lsn))
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    self.smgr.write(old.rel, old.block, &fr.data)?;
                    fr.dirty = false;
                }
                map.remove(&old);
            }
            // Load the requested block. A block past end-of-file (a fresh insert
            // target, or a block a redo record recreates) extends the relation so
            // `nblocks` always reflects every pinned block — scans and recovery
            // then see it without waiting for a checkpoint.
            while self.smgr.nblocks(rel)? <= block {
                self.smgr.extend(rel)?;
            }
            self.smgr.read(rel, block, &mut fr.data)?;
            if !page::is_initialized(&fr.data) {
                page::init(&mut fr.data);
                fr.dirty = true; // the initialization must reach disk
            }
            fr.tag = Some(tag);
            fr.pins = 1;
            fr.ref_bit = true;
            map.insert(tag, i);
            break i;
        };
        Ok(PinnedPage { pool: self, idx })
    }

    /// Write every dirty page to disk (obeying the write-ahead rule) and fsync
    /// each touched relation. Used by checkpoint / clean shutdown.
    pub fn flush_all(&self) -> std::io::Result<()> {
        let mut synced: Vec<RelFileNode> = Vec::new();
        for frame in &self.frames {
            let mut fr = frame.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
            if fr.dirty
                && let Some(tag) = fr.tag
            {
                let lsn = page::get_lsn(&fr.data);
                self.wal
                    .flush(Lsn(lsn))
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                self.smgr.write(tag.rel, tag.block, &fr.data)?;
                fr.dirty = false;
                if !synced.contains(&tag.rel) {
                    synced.push(tag.rel);
                }
            }
        }
        for rel in synced {
            self.smgr.sync(rel)?;
        }
        Ok(())
    }

    /// Drop every cached page for a relation (used by TRUNCATE). Unmaps and
    /// cleans the frames but never touches `pins`: a frame a concurrent
    /// `PinnedPage` still holds keeps `pins > 0`, so it is not chosen as an
    /// eviction victim (no cross-relation aliasing) and its holder's `Drop`
    /// decrements normally (no underflow). Once the holder drops, the frame is
    /// clean and unmapped, hence reusable. (TRUNCATE remains non-transactional,
    /// as documented — this only removes the crash/aliasing.)
    pub fn forget_relation(&self, rel: RelFileNode) {
        let mut map = self.map.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
        for frame in &self.frames {
            let mut fr = frame.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
            if fr.tag.map(|t| t.rel == rel).unwrap_or(false) {
                if let Some(tag) = fr.tag.take() {
                    map.remove(&tag);
                }
                fr.dirty = false;
                fr.ref_bit = false;
            }
        }
    }
}

/// A pinned page. Access the bytes through [`PinnedPage::read`] /
/// [`PinnedPage::modify`]; the pin is released on drop.
pub struct PinnedPage<'a> {
    pool: &'a BufferPool,
    idx: usize,
}

impl<'a> PinnedPage<'a> {
    fn frame(&self) -> MutexGuard<'a, Frame> {
        self.pool.frames[self.idx]
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
    }

    /// Read the page under the frame lock.
    pub fn read<R>(&self, f: impl FnOnce(&Page) -> R) -> R {
        let fr = self.frame();
        f(&fr.data)
    }

    /// Mutate the page under the frame lock and mark it dirty. The closure is the
    /// place to run the WAL-before-data sequence: change the page, append the WAL
    /// record, then stamp `pd_lsn` with the returned LSN — all before this
    /// returns and the frame becomes eligible for write-back.
    pub fn modify<R>(&self, f: impl FnOnce(&mut Page) -> R) -> R {
        let mut fr = self.frame();
        let r = f(&mut fr.data);
        fr.dirty = true;
        r
    }
}

impl Drop for PinnedPage<'_> {
    fn drop(&mut self) {
        let mut fr = self.pool.frames[self.idx]
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        // saturating: a concurrent forget_relation must never drive this to
        // underflow even though it no longer resets pins.
        fr.pins = fr.pins.saturating_sub(1);
        fr.ref_bit = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(nframes: usize) -> anyhow::Result<(tempfile::TempDir, BufferPool)> {
        let dir = tempfile::tempdir()?;
        let smgr = Arc::new(StorageManager::open(dir.path())?);
        let wal = Arc::new(Wal::open(dir.path())?);
        Ok((dir, BufferPool::new(nframes, smgr, wal)))
    }

    #[test]
    fn fresh_pages_are_initialized_and_persist_across_eviction() -> anyhow::Result<()> {
        let (_d, bp) = pool(2)?;
        let rel = RelFileNode(1);
        // Write a marker into block 0.
        {
            let p = bp.pin(rel, 0)?;
            p.modify(|page| page::add_item(page, b"row0"))
                .ok_or_else(|| anyhow::anyhow!("row0 did not fit"))?;
        }
        // Touch two more blocks to force block 0 out of the 2-frame pool.
        for b in 1..=2 {
            let p = bp.pin(rel, b)?;
            p.modify(|page| page::add_item(page, format!("row{b}").as_bytes()))
                .ok_or_else(|| anyhow::anyhow!("row did not fit"))?;
        }
        bp.flush_all()?;
        // Re-pin block 0: it must read back its marker from disk.
        let p = bp.pin(rel, 0)?;
        p.read(|page| assert_eq!(page::get_item(page, 1), Some(&b"row0"[..])));

        Ok(())
    }

    #[test]
    fn pinned_pages_are_never_evicted() -> anyhow::Result<()> {
        let (_d, bp) = pool(2)?;
        let rel = RelFileNode(1);
        let held = bp.pin(rel, 0)?;
        held.modify(|page| page::add_item(page, b"keep"))
            .ok_or_else(|| anyhow::anyhow!("keep row did not fit"))?;
        // Churn other blocks through the remaining frame; the pinned one survives.
        for b in 1..10 {
            let p = bp.pin(rel, b)?;
            p.modify(|page| page::add_item(page, b"x"))
                .ok_or_else(|| anyhow::anyhow!("row did not fit"))?;
        }
        held.read(|page| assert_eq!(page::get_item(page, 1), Some(&b"keep"[..])));

        Ok(())
    }

    #[test]
    fn forget_relation_while_pinned_is_safe() -> anyhow::Result<()> {
        let (_d, bp) = pool(4)?;
        let rel = RelFileNode(1);
        // Hold a pin on block 0 while the relation is forgotten (TRUNCATE).
        let held = bp.pin(rel, 0)?;
        held.modify(|page| page::add_item(page, b"stale"))
            .ok_or_else(|| anyhow::anyhow!("stale row did not fit"))?;
        bp.forget_relation(rel);
        // Churn other pins: the still-pinned frame must not be reused underneath
        // the live PinnedPage (no aliasing).
        for b in 1..8 {
            let p = bp.pin(RelFileNode(2), b)?;
            p.modify(|page| page::add_item(page, b"other"))
                .ok_or_else(|| anyhow::anyhow!("row did not fit"))?;
        }
        held.read(|page| assert_eq!(page::get_item(page, 1), Some(&b"stale"[..])));
        // Dropping the pin after forget must not underflow the pin count.
        drop(held);
        // A fresh pin of the forgotten block reads an initialized (empty) page,
        // not the stale contents.
        let fresh = bp.pin(rel, 0)?;
        fresh.read(|page| assert!(page::get_item(page, 1).is_none()));

        Ok(())
    }
}
