//! A fixed-frame buffer pool with clock-sweep eviction.
//!
//! Enforces the write-ahead rule: a dirty page is never written back to its
//! relation file until the WAL is flushed up to that page's `pd_lsn`
//! ([`crabgresql_wal`] module docs). The pool is fully synchronous — the
//! executor calls the heap AM on its own threads — and every field is behind a
//! lock, so it is `Send + Sync`.
//!
//! Eviction I/O runs while the mapping lock is held, which serializes page
//! faults. That is correct, and tolerable while the pool stays far larger than
//! the number of concurrently pinned pages.
//!
//! TODO(perf): run the eviction write-back and read outside the mapping lock
//! (or partition the mapping), so one page fault stops serializing every other
//! pin.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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

/// How many 8 KiB frames a pool commits.
///
/// A value object with a separate `from_env`, in the shape `BufferFlushPolicy`
/// already established in this crate, and for the same reason: the frame count
/// is a property of the engine being opened, not of the process environment.
/// Without the seam, every engine a test builds would commit the production
/// default, and the only way to ask for less would be to mutate the environment
/// — which is `unsafe` under the 2024 edition precisely because it races every
/// other thread reading it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferPoolPolicy {
    pub frames: usize,
}

impl Default for BufferPoolPolicy {
    fn default() -> Self {
        BufferPoolPolicy {
            frames: crabgresql_config::SHARED_BUFFERS.default / BLCKSZ,
        }
    }
}

impl BufferPoolPolicy {
    /// The pool size [`crabgresql_config::SHARED_BUFFERS`] asks for, rounded
    /// down to whole frames. A value that cannot be used as written is corrected
    /// and the correction logged, as with every other knob.
    pub fn from_env() -> Self {
        let complain = |message: String| tracing::warn!("{message}");
        BufferPoolPolicy {
            frames: crabgresql_config::SHARED_BUFFERS.get(complain) / BLCKSZ,
        }
    }

    /// The smallest pool this engine has ever shipped with, for tests and tools
    /// that open an engine to exercise it rather than to serve with it. A test
    /// binary opens many engines, and at the production default each would
    /// commit its full `shared_buffers` up front.
    pub fn minimal() -> Self {
        BufferPoolPolicy {
            frames: crabgresql_config::SHARED_BUFFERS.min / BLCKSZ,
        }
    }
}

// The pool divides by its frame count on every miss, so a floor below one whole
// frame would be a division by zero. The bound belongs to the config crate,
// which has no notion of a page size; the reconciliation belongs here.
const _: () = assert!(crabgresql_config::SHARED_BUFFERS.min >= BLCKSZ);

struct Frame {
    tag: Option<BufferTag>,
    /// The frame's 8 KiB page, committed when the pool is built.
    ///
    /// Up front, the way PostgreSQL commits `shared_buffers`, so a pool sized
    /// past what the machine has fails at startup where an operator can see it
    /// rather than hours later mid-query. Allocating on first use instead would
    /// also put the allocation inside `pin`'s critical section, which is the one
    /// stretch of code the whole pool serializes on, to zero bytes that
    /// `smgr::read` overwrites in full a line later.
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
    /// How each served pin was answered.
    ///
    /// A throughput number on its own cannot tell "the pool got faster" from "the
    /// working set started fitting", and those call for opposite fixes. Reported
    /// next to every measurement so the difference stays visible.
    ///
    /// Three counters rather than two because creating a page is not a cache
    /// miss: a pin past end-of-file extends the relation, which is what every
    /// insert into a full page and every recovered block does. Folding those in
    /// would report a bulk `COPY` at a hit rate near zero and send the reader off
    /// to enlarge a pool that was never the problem. They are counted once the
    /// outcome is known, so a pin that fails partway counts as none of the three
    /// and the sum stays "pins actually served".
    ///
    /// Relaxed: they are a ratio over millions of pins, and ordering them against
    /// the pin they describe would cost more than the counter is worth.
    hits: AtomicU64,
    misses: AtomicU64,
    extends: AtomicU64,
}

/// How a pool answered the pins it served. See [`BufferPool::hit_stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Served from a resident frame.
    pub hits: u64,
    /// The block existed and had to be read in.
    pub misses: u64,
    /// The block was past end-of-file and the relation was extended for it.
    pub extends: u64,
}

impl PoolStats {
    /// Share of pins that found their page resident, over the pins that could
    /// have — a page this pin created was never a candidate for residency, so
    /// counting it would understate the pool at no one's benefit. `None` when
    /// nothing has been read or hit yet, since 0/0 is not a rate.
    pub fn hit_rate(&self) -> Option<f64> {
        let looked_up = self.hits + self.misses;
        (looked_up > 0).then(|| self.hits as f64 / looked_up as f64)
    }
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
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            extends: AtomicU64::new(0),
        }
    }

    /// How the pins served since this pool was opened were answered.
    ///
    /// Deliberately readable outside tests: the whole argument for keeping these
    /// counters is that a hit rate has to sit next to a throughput number for
    /// either to mean anything, and a hit rate only a unit test can reach does
    /// not do that for the workloads anyone actually runs.
    pub fn hit_stats(&self) -> PoolStats {
        PoolStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            extends: self.extends.load(Ordering::Relaxed),
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
            self.hits.fetch_add(1, Ordering::Relaxed);
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
            let mut created = false;
            while self.smgr.nblocks(rel)? <= block {
                self.smgr.extend(rel)?;
                created = true;
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
            // Counted here rather than on the way in, so the outcome is known:
            // a page this pin brought into existence was never absent from the
            // pool, and a pin that failed above counts as nothing at all.
            if created {
                self.extends.fetch_add(1, Ordering::Relaxed);
            } else {
                self.misses.fetch_add(1, Ordering::Relaxed);
            }
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
                // An untagged frame is skipped rather than reported: the only
                // way one is dirty is a `PinnedPage` that outlived
                // `forget_relation` and wrote through it (see `pin`), and those
                // bytes belong to no relation — they are discarded, not owed to
                // disk.
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
    /// clean and unmapped, hence reusable.
    pub fn forget_relation(&self, rel: RelFileNode) {
        // "Do not write these pages back" and "do not fsync on their behalf" are
        // the same intent; every caller here goes on to unlink or truncate the
        // file. Defence in depth — `unlink`/`truncate` clear it too.
        self.smgr.forget_pending_fsync(rel);
        let mut map = self.map.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
        // Driven off the mapping rather than by sweeping every frame: a frame
        // carries a tag exactly while the mapping names it (`pin` sets both under
        // this lock, and the removal below clears both), so the mapping already
        // lists every frame this could touch. Sweeping instead would lock all
        // `shared_buffers` worth of frames to find the few that match, which at a
        // realistic pool size is thousands of acquisitions for a handful of hits.
        //
        // The mapping decides *which* frames to visit, but the frame decides what
        // it holds. Checking the tag before clearing it is what keeps a mapping
        // that has somehow drifted from costing another relation its dirty page:
        // the entry goes either way, since an entry naming a frame that does not
        // hold it is wrong by definition, but only a frame that agrees is
        // emptied. `retain` rather than a collected `Vec` so the removal cannot
        // be forgotten by a later edit, and so a TRUNCATE of a pool-sized
        // relation does not allocate its way through the mapping lock.
        map.retain(|tag, &mut idx| {
            if tag.rel != rel {
                return true;
            }
            let mut fr = self.frames[idx]
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"));
            if fr.tag == Some(*tag) {
                fr.tag = None;
                fr.dirty = false;
                fr.ref_bit = false;
            }
            false
        });
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

/// Throughput of [`BufferPool::pin`] at 1, 2, 4, 8 and 16 threads, across three
/// working-set sizes.
///
/// Every page every query touches arrives through `pin`, and `pin` takes one
/// process-wide `map` lock to do it — so how it behaves under concurrency is a
/// design constraint, not a detail. This measures the pool in isolation: no
/// parse, no plan, no executor.
///
/// The three working sets exist because a single column cannot distinguish the
/// two failure modes, and they call for opposite fixes:
///
/// * `resident` — half the pool. Every pin is a hit, so this is the cost of the
///   `map` lock and the three frame-lock acquisitions per pin, and nothing else.
/// * `tight` — an eighth over the pool. Just past the boundary, so the clock
///   sweep runs continuously; *at* the pool it would not run at all, because a
///   working set that fits is an absorbing state and every pin would hit.
/// * `thrash` — 18x the pool. Almost every pin is a miss, and a miss holds `map`
///   across `nblocks`, `extend` and `read`. The ratio is the one `pgbench -s 10`
///   put on the 1024 frames this engine shipped before `shared_buffers` became a
///   knob: 178 MiB of heap and index against 8 MiB, which is 22:1 — rounded down
///   here because the point is the order of magnitude, not the decimal.
///
/// A pool sized past the working set makes `thrash` disappear and flatters every
/// number in the other two columns. That is why the hit rate is printed next to
/// every measurement: a change that only moved the working set is supposed to be
/// visible as such, not bankable as a speedup.
///
/// The relation is written immediately before the run, so its blocks are in the
/// OS page cache and a miss measures the pool's serialization rather than the
/// SSD. That is deliberate — the lock structure is what is under test — but it
/// does mean these numbers are an upper bound on a cold-cache server.
///
/// `#[ignore]`d because it is a *measurement*, not an assertion. It asserts
/// nothing about timing — a timing assertion is the classic CI flake — so it
/// never runs in CI, and its numbers are machine-dependent. Run it explicitly:
///
/// ```text
/// cargo test --release -p crabgresql-pg-engine --lib bufpool_contention_bench \
///     -- --ignored --nocapture --test-threads=1
/// ```
///
/// `--release` is mandatory (a debug build measures lock bookkeeping and
/// unoptimised atomics, not the design) and `--test-threads=1` stops the harness
/// from running two thread counts concurrently and poisoning both.
///
/// # Baseline
///
/// Recorded on a 10-core machine against a 1024-frame pool, Kops/s aggregate
/// with the hit rate beside it. Absolute numbers are machine-dependent; the
/// *shape* is the point:
///
/// ```text
///  threads  resident (512 blk)    tight (1152 blk)  thrash (18432 blk)
///        1      18638.1 100.0%          317.6 0.2%          280.1 0.0%
///        2      10431.3 100.0%         501.3 53.1%          247.5 0.0%
///        4       5002.8 100.0%         569.0 68.2%          182.7 0.0%
///        8       5825.6 100.0%         496.6 73.3%          163.2 0.0%
///       16       5373.1 100.0%         980.2 91.2%          149.3 0.6%
/// ```
///
/// Two things to notice, because they decide what is worth fixing first.
///
/// The `resident` column *falls* from one thread to four — aggregate throughput
/// gets worse as threads are added, which is a lock convoy rather than a mere
/// failure to scale. Whatever else is true, `map` is a real bottleneck.
///
/// But `thrash` at one thread is already 66x slower than `resident` at one
/// thread, while the whole span of the contention collapse is 3.7x. So on this
/// pool a pin that misses costs more than every lock on the hit path put
/// together, by more than an order of magnitude — which is why sizing the pool
/// comes before partitioning the lock, not after.
///
/// `tight` is the one column whose hit rate moves along the row, and that is
/// the finding rather than a defect in it: a lone cyclic scanner over a working
/// set an eighth larger than the pool hits essentially never (0.2%), because a
/// clock sweep evicts precisely the block the walk is about to want next.
/// Spread enough scanners around the same orbit and the pool stays warm
/// (91.2%), since between them they hold most of it resident. Read that column
/// as the cost of a scan slightly too big to cache, not as a contention curve —
/// `thrash`, whose hit rate stays flat at zero, is the contention curve.
///
/// # This bench does not have the server's shape
///
/// `pgbench -S -s 10` against the server on the same machine, same day:
///
/// ```text
///  clients     1       2       4       8      16
///      tps  15316   22646   33567   29655   26319
/// ```
///
/// The server peaks at four clients; this bench peaks at one. Both are honest.
/// A `pin` here is the whole iteration, so the convoy shows up as soon as a
/// second thread exists, whereas a pgbench transaction spends most of its time
/// in parse, plan and execute, which spaces the pins out and delays the
/// collapse. This bench is the more sensitive instrument and the one to iterate
/// against; it is not the one that decides whether the user-visible curve got
/// fixed. Check both.
#[cfg(test)]
mod bufpool_contention_bench {
    use super::*;
    use std::hint::black_box;
    use std::sync::Barrier;
    use std::time::{Duration, Instant};

    /// Deliberately fixed, and deliberately not tied to `SHARED_BUFFERS`: the
    /// three working sets below are defined as ratios against it, so following
    /// the shipping default would silently redefine what `thrash` means the next
    /// time somebody tunes the server and make every recorded number
    /// incomparable. 1024 frames is what this engine shipped before that knob
    /// existed, which is what the recorded baseline was taken against.
    const POOL_FRAMES: usize = 1024;
    /// Prime, so it is coprime with every span below and each thread's walk is
    /// one cycle over the whole working set rather than a few frames.
    const STRIDE: u64 = 7_919;

    struct Workload {
        name: &'static str,
        blocks: u32,
        /// Chosen per workload rather than shared: a resident pin is ~100 ns and
        /// needs millions of iterations to time reliably, while a thrashing pin
        /// is a serialized syscall and would run for minutes at that count.
        /// Throughput is normalised, so the columns stay comparable.
        iters: u64,
    }

    const WORKLOADS: [Workload; 3] = [
        Workload {
            name: "resident",
            blocks: POOL_FRAMES as u32 / 2,
            iters: 2_000_000,
        },
        // Above the pool, not equal to it. At exactly `POOL_FRAMES` the working
        // set fits and the state is absorbing: every timed pin hits, the sweep
        // never runs, and the column is a second copy of `resident` wearing a
        // different name. An eighth over is enough to keep eviction going
        // continuously while the working set is still nearly resident, which is
        // the boundary this column is supposed to describe.
        Workload {
            name: "tight",
            blocks: POOL_FRAMES as u32 + POOL_FRAMES as u32 / 8,
            iters: 200_000,
        },
        Workload {
            name: "thrash",
            blocks: POOL_FRAMES as u32 * 18,
            iters: 20_000,
        },
    ];

    const REL: RelFileNode = RelFileNode(1);

    fn work(pool: &BufferPool, blocks: u32, iters: u64, seed: u64) -> u64 {
        let span = u64::from(blocks);
        let mut idx = seed % span;
        let mut acc = 0u64;
        for _ in 0..iters {
            idx = (idx + STRIDE) % span;
            let page = pool
                .pin(REL, idx as u32)
                .unwrap_or_else(|e| panic!("bench pin failed: {e}"));
            // Touch the bytes under the frame lock: a pin nobody reads would let
            // the optimiser skip the part of the sequence that costs the most.
            acc += page.read(|pg| u64::from(pg[0]));
        }
        acc
    }

    /// Elapsed time and the pins served inside it, for `threads` workers.
    fn run(threads: usize, workload: &Workload) -> (Duration, PoolStats) {
        let dir = tempfile::tempdir().expect("tempdir");
        let smgr = Arc::new(StorageManager::open(dir.path()).expect("smgr"));
        // Materialise every block before the pool exists, so the run starts with
        // a cold pool over a warm file and no worker pays an `extend`.
        for _ in 0..workload.blocks {
            smgr.extend(REL).expect("extend");
        }
        let wal = Arc::new(Wal::open(dir.path()).expect("wal"));
        let pool = Arc::new(BufferPool::new(POOL_FRAMES, smgr, wal));

        let barrier = Arc::new(Barrier::new(threads + 1));
        let mut handles = Vec::with_capacity(threads);
        for t in 0..threads {
            let pool = Arc::clone(&pool);
            let barrier = Arc::clone(&barrier);
            let (blocks, iters) = (workload.blocks, workload.iters);
            // Spread the threads evenly around the orbit instead of nudging each
            // one a fixed step along it. Every thread walks the same cycle, so a
            // small offset makes the trailing threads read what the leader just
            // faulted in: the effective working set then shrinks as threads are
            // added, and a row meant to isolate contention picks up a rising hit
            // rate instead. Even spacing keeps the formation as wide as the orbit
            // allows.
            let seed = t as u64 * u64::from(blocks) / threads as u64;
            handles.push(std::thread::spawn(move || {
                // Warm up before the barrier, so filling the pool and
                // first-touch effects sit outside the timed window. Caught,
                // because a worker that dies here never reaches the barrier and
                // `Barrier` has no poisoning — the run would hang with no
                // diagnostic instead of failing.
                let warmed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    black_box(work(&pool, blocks, iters / 10, seed));
                }));
                barrier.wait();
                warmed.expect("bench worker panicked during warmup");
                let acc = work(&pool, blocks, iters, seed);
                // Without this the optimiser is free to delete the whole loop.
                black_box(acc);
            }));
        }
        // Sampled before the barrier is released, not after. Every worker has
        // finished its warmup and is parked here, and none can start timed work
        // until this thread arrives — so this is the one instant at which the
        // warmup is complete and the measurement has not begun. Reading it after
        // the release would subtract whatever the workers managed in the gap,
        // which is timed work, from the hit rate but not from the throughput.
        let warm = pool.hit_stats();
        barrier.wait();
        let started = Instant::now();
        for handle in handles {
            assert!(handle.join().is_ok(), "a bench worker panicked");
        }
        let elapsed = started.elapsed();
        let end = pool.hit_stats();
        let served = PoolStats {
            hits: end.hits - warm.hits,
            misses: end.misses - warm.misses,
            extends: end.extends - warm.extends,
        };
        (elapsed, served)
    }

    #[test]
    #[ignore = "measurement, not an assertion: prints throughput, asserts nothing about timing"]
    fn bufpool_contention() {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0);
        eprintln!(
            "bufpool contention: {POOL_FRAMES}-frame pool ({} KiB), available_parallelism={cores}",
            POOL_FRAMES * BLCKSZ / 1024
        );
        eprintln!("each cell: Kops/s (aggregate), hit rate");
        // One width for the header and the data, so the columns cannot drift
        // apart the way two hand-matched format strings did.
        const COL: usize = 18;
        eprint!("{:>8}", "threads");
        for w in &WORKLOADS {
            eprint!("  {:>COL$}", format!("{} ({} blk)", w.name, w.blocks));
        }
        eprintln!();
        for threads in [1usize, 2, 4, 8, 16] {
            eprint!("{threads:>8}");
            for workload in &WORKLOADS {
                let (elapsed, served) = run(threads, workload);
                let kops = (workload.iters * threads as u64) as f64 / elapsed.as_secs_f64() / 1e3;
                let rate = match served.hit_rate() {
                    Some(rate) => format!("{:.1}%", rate * 100.0),
                    None => "--".to_string(),
                };
                eprint!("  {:>COL$}", format!("{kops:.1} {rate}"));
            }
            eprintln!();
        }
    }
}
