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
        //
        // Bounded, because the alternative is not a slow pin but a hung engine:
        // this loop runs with `map` held, so a sweep that never finds a victim
        // blocks every other pin in the process, including the ones holding the
        // pins it is waiting for. Two revolutions suffice when a victim exists —
        // the first clears every `ref_bit`, so the second can only be blocked by
        // pins — and a third leaves no doubt. Past that every frame is pinned,
        // which is resource exhaustion and belongs to the caller as an error,
        // the way PostgreSQL raises "no unpinned buffers available".
        let mut swept = 0usize;
        let sweep_limit = self.frames.len().saturating_mul(3);
        let idx = loop {
            swept += 1;
            if swept > sweep_limit {
                return Err(std::io::Error::other(format!(
                    "no unpinned buffer available: all {} buffer pool frames are pinned",
                    self.frames.len()
                )));
            }
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
            //
            // The tag is surrendered only once the write-back has succeeded, and
            // the mapping is updated in the same breath. Taking it first would be
            // enough for the happy path, but the two `?` below can return while
            // the frame is mid-eviction — a full disk, an EIO, or a `wal.flush`
            // for a cached page whose `pd_lsn` predates a recovery `reset_to`.
            // That would leave the mapping naming a frame with no tag: the next
            // sweep reuses the frame without removing the stale key (the removal
            // lives inside this arm), the mapping ends up with two keys for one
            // frame, and a later pin of the old tag is served another relation's
            // page as a hit. So an error here has to leave the frame exactly as
            // it found it.
            if let Some(old) = fr.tag {
                if fr.dirty {
                    let lsn = page::get_lsn(&fr.data);
                    self.wal
                        .flush(Lsn(lsn))
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                    self.smgr.write(old.rel, old.block, &fr.data)?;
                }
                fr.tag = None;
                map.remove(&old);
            }
            // Unconditionally, not just when something was evicted: a frame can
            // reach here untagged *and* dirty, because `forget_relation` clears
            // the tag of a frame a live `PinnedPage` may still `modify`. Leaving
            // the flag set would carry it onto the page loaded next, so a page
            // read clean from disk would be written straight back at every
            // checkpoint and `dirty` would stop meaning "differs from disk".
            fr.dirty = false;
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

    /// Write every dirty page to disk (obeying the write-ahead rule), then fsync
    /// everything written since the last checkpoint. Used by checkpoint / clean
    /// shutdown.
    ///
    /// The fsync pass covers what was *written*, not what this pass wrote. Those
    /// differ: the clock sweep writes pages back at eviction too, and an evicted
    /// frame keeps no record of what it held, so a relation whose pages were all
    /// evicted before the checkpoint has nothing dirty here and would never be
    /// fsynced. `StorageManager` tracks it instead — see its `pending_fsync`.
    ///
    /// Why an eviction racing this pass is still safe: fix a frame, and every
    /// writer and eviction section on it is totally ordered against the moment
    /// this loop holds its lock. A section before that moment registered its
    /// fsync before the drain below, so it is covered. A section after it must
    /// have re-dirtied a frame this loop had just cleaned, which takes a writer
    /// whose WAL append is also after the checkpoint's redo sample — so its
    /// record is replayed instead. (The argument runs on the *frame* lock: the
    /// eviction path holds `map` across its write but this loop never takes it.)
    pub fn flush_all(&self) -> std::io::Result<()> {
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
            }
        }
        self.smgr.sync_pending()
    }

    /// `pd_lsn` of every frame that is currently dirty, skipping never-stamped
    /// pages (`pd_lsn == 0`). White-box hook for the checkpoint-ordering test:
    /// after `flush_all` returns, a dirty frame whose LSN is at or below a
    /// previously sampled redo point is a change that is neither on disk nor in
    /// the replayed suffix.
    #[cfg(test)]
    pub(crate) fn dirty_page_lsns(&self) -> Vec<u64> {
        self.frames
            .iter()
            .filter_map(|frame| {
                let fr = frame.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
                let lsn = page::get_lsn(&fr.data);
                (fr.dirty && fr.tag.is_some() && lsn != 0).then_some(lsn)
            })
            .collect()
    }

    /// Drop every cached page for a relation (used by TRUNCATE). Unmaps and
    /// cleans the frames but never touches `pins`: a frame a concurrent
    /// `PinnedPage` still holds keeps `pins > 0`, so it is not chosen as an
    /// eviction victim (no cross-relation aliasing) and its holder's `Drop`
    /// decrements normally (no underflow). Once the holder drops, the frame is
    /// clean and unmapped, hence reusable. (TRUNCATE remains non-transactional,
    /// as documented — this only removes the crash/aliasing.)
    pub fn forget_relation(&self, rel: RelFileNode) {
        // "Do not write these pages back" and "do not fsync on their behalf" are
        // the same intent; every caller here goes on to unlink or truncate the
        // file. Defence in depth — `unlink`/`truncate` clear it too.
        self.smgr.forget_pending_fsync(rel);
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
        // Poison-tolerant, unlike every other lock site here, because this one
        // runs during unwinding. `read`/`modify` hold the frame guard across the
        // caller's closure, so a panic in that closure poisons this very mutex;
        // panicking again here would be a panic inside a destructor during
        // cleanup, which Rust turns into an `abort()` — not even catchable by an
        // enclosing `catch_unwind`. Releasing a pin must never be what escalates
        // somebody else's failed statement into a dead server.
        //
        // Only the pin count is touched under that tolerance, and it is exactly
        // the field a panic cannot have corrupted. The *page* may well be torn —
        // `modify` stamps `dirty` after the closure returns, so a panic halfway
        // through leaves half-written bytes carrying the old flag — which is why
        // every other site here still treats a poisoned frame as fatal rather
        // than reading it. This releases the pin and lets that judgement stand.
        let mut fr = self.pool.frames[self.idx]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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

    /// The failure this pass exists to prevent: a relation whose pages were all
    /// written back by *eviction* has nothing dirty left for the checkpoint to
    /// notice, so a checkpoint that fsynced only what it wrote would never force
    /// it to disk. Under bounded replay those page-cache writes have no second
    /// chance — the records that would rebuild them sit below the redo point.
    #[test]
    fn a_relation_evicted_before_the_checkpoint_is_still_fsynced() -> anyhow::Result<()> {
        let (_d, bp) = pool(2)?;
        let victim = RelFileNode(1);
        // Dirty both frames with `victim`'s pages.
        for block in 0..2 {
            let page = bp.pin(victim, block)?;
            page.modify(|p| page::add_item(p, b"row"))
                .ok_or_else(|| anyhow::anyhow!("row did not fit"))?;
        }
        // Churn a second relation through both frames, evicting every trace of
        // `victim`: its pages are written but only into the page cache.
        for block in 0..4 {
            let page = bp.pin(RelFileNode(2), block)?;
            page.modify(|p| page::add_item(p, b"other"))
                .ok_or_else(|| anyhow::anyhow!("row did not fit"))?;
        }
        assert!(
            !bp.frames.iter().any(|f| {
                let fr = f.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
                fr.dirty && fr.tag.map(|t| t.rel) == Some(victim)
            }),
            "the victim must have no dirty frame left, or the test proves nothing"
        );

        bp.flush_all()?;
        assert!(
            !bp.smgr.fsync_pending(victim),
            "the evicted relation was never fsynced by the checkpoint"
        );

        Ok(())
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

    /// Run `f` with a watchdog; `None` means it never finished. Local copy of
    /// the one in `smgr`'s tests — a test module cannot import another's.
    fn within<T: Send + 'static>(max_ms: u64, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(std::time::Duration::from_millis(max_ms))
            .ok()
    }

    /// An eviction that fails partway must leave the frame exactly as it found
    /// it. The dangerous shape is the opposite: a frame stripped of its tag
    /// while the mapping still names it, because nothing removes that key
    /// afterwards — the removal lives inside the very branch the error skipped
    /// next time round — so the mapping ends up with two keys for one frame and
    /// serves one relation's page under the other's tag.
    #[test]
    fn a_failed_eviction_leaves_the_mapping_and_the_tag_agreeing() -> anyhow::Result<()> {
        let (_d, bp) = pool(1)?;
        let victim = BufferTag {
            rel: RelFileNode(1),
            block: 0,
        };
        // Fill the single frame with a dirty page, then let go of it.
        {
            let page = bp.pin(victim.rel, victim.block)?;
            page.modify(|p| page::add_item(p, b"keep"))
                .ok_or_else(|| anyhow::anyhow!("row did not fit"))?;
        }
        // The next pin must evict it; make the write-back fail mid-eviction.
        bp.smgr.fail_next_write.store(true, Ordering::SeqCst);
        assert!(
            bp.pin(RelFileNode(2), 0).is_err(),
            "the injected write failure must surface"
        );

        let map = bp.map.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
        let fr = bp.frames[0]
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        assert_eq!(
            fr.tag,
            Some(victim),
            "a failed eviction must not surrender the tag"
        );
        assert_eq!(
            map.get(&victim),
            Some(&0),
            "and must leave the mapping naming the frame it still describes"
        );
        assert!(fr.dirty, "the page it could not write is still unwritten");
        assert_eq!(map.len(), 1, "no key was leaked for a frame it never took");

        Ok(())
    }

    /// A pool whose every frame is pinned has no victim to find. The sweep used
    /// to spin on that forever while holding the mapping lock, which blocks
    /// every other pin in the process — including the ones holding the pins it
    /// is waiting for. It has to be an error the caller can see instead.
    #[test]
    fn a_fully_pinned_pool_reports_exhaustion_instead_of_spinning() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let smgr = Arc::new(StorageManager::open(dir.path())?);
        let wal = Arc::new(Wal::open(dir.path())?);
        let bp = Arc::new(BufferPool::new(4, smgr, wal));

        let held: Vec<_> = (0..4)
            .map(|b| bp.pin(RelFileNode(1), b))
            .collect::<std::io::Result<_>>()?;

        let probe = {
            let bp = Arc::clone(&bp);
            within(5_000, move || bp.pin(RelFileNode(1), 99).is_err())
        };
        drop(held);
        assert_eq!(
            probe,
            Some(true),
            "pinning against a fully pinned pool must return an error, not hang"
        );

        Ok(())
    }

    /// `read`/`modify` hold the frame guard across the caller's closure, so a
    /// panic in that closure poisons this frame's mutex. Releasing the pin must
    /// not then panic on the poison: that would be a panic inside a destructor
    /// during unwinding, which aborts the process outright — not catchable by
    /// any enclosing `catch_unwind`, not loggable, and fatal to every other
    /// session. If this regresses, the whole test binary dies with SIGABRT
    /// rather than failing this one case, so a plain FAILED here is already the
    /// good outcome.
    ///
    /// What deliberately is *not* asserted: that the frame stays usable. A
    /// panic inside `modify` can leave the page half-written, so every other
    /// site treats a poisoned frame as fatal; this only keeps the failure
    /// unwinding to the one statement instead of taking the process with it.
    #[test]
    fn a_panic_under_the_frame_guard_does_not_abort_the_process() -> anyhow::Result<()> {
        let (_d, bp) = pool(2)?;
        let page = bp.pin(RelFileNode(1), 0)?;

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            page.read(|_| panic!("a statement failed while holding the page"));
        }));
        assert!(panicked.is_err(), "the closure's panic must propagate");
        // Dropping the pin is the step that used to abort. Reaching the next
        // line at all is the assertion.
        drop(page);

        Ok(())
    }
}
