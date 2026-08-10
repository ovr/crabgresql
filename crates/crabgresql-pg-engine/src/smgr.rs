//! Storage manager: maps a relation to its on-disk file and does checksummed,
//! block-granular I/O. One file per relation under `<data_dir>/base/<relfilenode>`.
//!
//! TODO: split a relation across 1 GB segment files as PG does; one unbounded
//! file per relation caps a relation at the filesystem's file-size limit.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::page::{self, BLCKSZ, Page};

/// A relation's physical file identity (PostgreSQL's `relfilenode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RelFileNode(pub u32);

pub struct StorageManager {
    base: PathBuf,
    files: Mutex<HashMap<u32, Arc<Mutex<File>>>>,
    /// RAM-backed relations (`Temporary` tables only — `Unlogged` is on-disk). A
    /// relfilenode present here holds its blocks in this map instead of a file
    /// under `base/`; every smgr op routes here first. Its pages never touch disk
    /// and are lost on restart. The membership map is an `RwLock` (mutated only at
    /// register/unlink) so the per-op check takes a shared read lock — permanent
    /// relations never serialize on it — and each relation's pages sit behind their
    /// own `Mutex`, so distinct memory relations don't contend either.
    mem: RwLock<HashMap<u32, Mutex<Vec<Page>>>>,
    /// Relations whose pages have been written but not yet fsynced.
    ///
    /// A page write only reaches the OS page cache. The buffer pool writes pages
    /// back at *eviction* as well as at checkpoint, and an evicted frame keeps no
    /// record of what it held — so a checkpoint that fsynced only "the relations I
    /// just wrote" would miss every relation whose pages were all evicted before
    /// it ran. Whole-stream replay used to repair those lost writes; a replay
    /// bounded at a redo point cannot. PostgreSQL keeps an equivalent queue of
    /// pending fsync requests, drained at checkpoint, for the same reason.
    ///
    /// It lives here rather than in the buffer pool so that memory relations fall
    /// out for free (the callers below register only past the RAM early-return),
    /// so `extend` — which also writes bytes and is invisible to the pool — is
    /// covered, and so clearing sits next to the code that destroys the file.
    pending_fsync: Mutex<std::collections::HashSet<u32>>,
    /// Relations whose file has been created but whose directory entry is not
    /// durable yet. See [`StorageManager::sync_base_dir`].
    created_unsynced: Mutex<std::collections::HashSet<u32>>,
    /// Fast-path hint for "`created_unsynced` is non-empty", so the steady state
    /// costs one atomic load rather than a lock on every page fault.
    dir_unsynced: AtomicBool,
    /// Serializes the `base/` fsync, so N racing creators issue one rather than N.
    dir_sync: Mutex<()>,
    /// Stands in for the `base/` fsync so a test can make it fail or block. The
    /// retry and the no-blocking-under-`files` properties are both invisible from
    /// outside otherwise.
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    dir_sync_hook: Mutex<Option<Arc<dyn Fn() -> std::io::Result<()> + Send + Sync>>>,
    /// Makes the next per-relation fsync fail, so the retry path is testable.
    #[cfg(test)]
    fail_next_file_sync: AtomicBool,
    /// Makes the next block write fail, so the buffer pool's mid-eviction error
    /// path is testable. That path is otherwise reachable only from a full disk.
    #[cfg(test)]
    pub(crate) fail_next_write: AtomicBool,
}

impl StorageManager {
    pub fn open(data_dir: &std::path::Path) -> std::io::Result<StorageManager> {
        let base = data_dir.join("base");
        std::fs::create_dir_all(&base)?;
        Ok(StorageManager {
            base,
            files: Mutex::new(HashMap::new()),
            mem: RwLock::new(HashMap::new()),
            pending_fsync: Mutex::new(std::collections::HashSet::new()),
            created_unsynced: Mutex::new(std::collections::HashSet::new()),
            dir_unsynced: AtomicBool::new(false),
            dir_sync: Mutex::new(()),
            #[cfg(test)]
            dir_sync_hook: Mutex::new(None),
            #[cfg(test)]
            fail_next_file_sync: AtomicBool::new(false),
            #[cfg(test)]
            fail_next_write: AtomicBool::new(false),
        })
    }

    /// Register `rel` as a RAM-backed memory relation. Idempotent — a repeated
    /// call (e.g. a memory table's TRUNCATE staging its new relfilenode) leaves an
    /// existing page vector untouched. Must be called before the relation is first
    /// pinned so every smgr op routes to RAM.
    pub fn register_memory(&self, rel: RelFileNode) {
        self.mem
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .entry(rel.0)
            .or_insert_with(|| Mutex::new(Vec::new()));
    }

    /// Whether `rel` is a RAM-backed memory relation.
    fn is_mem(&self, rel: RelFileNode) -> bool {
        self.mem
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .contains_key(&rel.0)
    }

    /// Run `f` on the RAM page-vector of `rel` if it is a memory relation, or
    /// return `None` (so the caller falls through to the on-disk file path). Holds
    /// only a shared read lock on the membership map plus the relation's own lock.
    fn with_mem<R>(&self, rel: RelFileNode, f: impl FnOnce(&mut Vec<Page>) -> R) -> Option<R> {
        let map = self
            .mem
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let pages = map.get(&rel.0)?;
        let mut pages = pages.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
        Some(f(&mut pages))
    }

    fn file(&self, rel: RelFileNode) -> std::io::Result<Arc<Mutex<File>>> {
        let arc = {
            let mut files = self
                .files
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"));
            match files.get(&rel.0) {
                Some(f) => Arc::clone(f),
                None => {
                    let path = self.base.join(rel.0.to_string());
                    // Decided under the same lock that serializes creation, so a
                    // racing opener cannot make the fsync below be skipped.
                    let created = !path.try_exists()?;
                    // truncate(false): a relation file's existing blocks must be
                    // preserved.
                    let f = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(&path)?;
                    if created {
                        self.created_unsynced
                            .lock()
                            .unwrap_or_else(|_| panic!("mutex poisoned"))
                            .insert(rel.0);
                        self.dir_unsynced.store(true, Ordering::Release);
                    }
                    let arc = Arc::new(Mutex::new(f));
                    files.insert(rel.0, Arc::clone(&arc));
                    arc
                }
            }
        };
        // Outside the `files` lock, and gated on *this* relation rather than on
        // "did I just create it". Outside, because `files` sits under every page
        // read and write. Per-relation, because that is what makes it a retry: a
        // sync that failed leaves this relation queued, so the next open — cache
        // hit or not — tries again, where the old `if created` test would have
        // skipped it forever. And because a relation whose entry is already
        // durable must not wait behind an unrelated relation's fsync.
        if self.needs_dir_sync(rel.0) {
            self.sync_base_dir()?;
        }
        Ok(arc)
    }

    /// The cached handle for `rel`, or `None` if it was never opened or has been
    /// unlinked.
    ///
    /// Deliberately not [`StorageManager::file`], which opens with `create(true)`:
    /// the checkpoint's fsync pass walks relations that may have been dropped
    /// since they were written, and creating a file in order to fsync it would
    /// resurrect a relation the catalog no longer names.
    fn cached_file(&self, rel: u32) -> Option<Arc<Mutex<File>>> {
        self.files
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .get(&rel)
            .map(Arc::clone)
    }

    /// fsync `base/` if any relation file has been created since the last
    /// successful sync, so those directory entries are durable.
    ///
    /// Without it a crash can leave a relation whose pages reached disk but whose
    /// *name* did not, and the relation reads as empty. Whole-stream replay used
    /// to hide that — the file was recreated on the next open and refilled by redo
    /// — but a replay bounded at a redo point starts above those records and has
    /// nothing to refill it with.
    ///
    /// One flag for the whole directory, because one successful fsync makes every
    /// pending entry durable. The flag is cleared *before* the fsync and restored
    /// on failure: a creation whose store lands after the swap simply leaves the
    /// flag set and gets its own sync, so no creation is ever both unclaimed and
    /// unsynced.
    fn needs_dir_sync(&self, rel: u32) -> bool {
        self.dir_unsynced.load(Ordering::Acquire)
            && self
                .created_unsynced
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"))
                .contains(&rel)
    }

    fn sync_base_dir(&self) -> std::io::Result<()> {
        let _one_at_a_time = self
            .dir_sync
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        // Claim every outstanding creation before the fsync, not after: one fsync
        // makes all of them durable, and a creation registered after this drain
        // simply stays queued and gets its own. So no creation is ever both
        // unclaimed and unsynced.
        let claimed: Vec<u32> = std::mem::take(
            &mut *self
                .created_unsynced
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned")),
        )
        .into_iter()
        .collect();
        if claimed.is_empty() {
            return Ok(()); // another thread synced while we waited
        }
        self.dir_unsynced.store(false, Ordering::Release);
        self.do_dir_sync().inspect_err(|_| {
            let mut set = self
                .created_unsynced
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"));
            set.extend(claimed);
            self.dir_unsynced.store(true, Ordering::Release);
        })
    }

    #[cfg(not(test))]
    fn do_dir_sync(&self) -> std::io::Result<()> {
        crabgresql_wal::sync_dir(&self.base)
    }

    #[cfg(test)]
    fn do_dir_sync(&self) -> std::io::Result<()> {
        let hook = self
            .dir_sync_hook
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .clone();
        match hook {
            Some(hook) => hook(),
            None => crabgresql_wal::sync_dir(&self.base),
        }
    }

    /// Number of 8 KB blocks currently in the relation file.
    pub fn nblocks(&self, rel: RelFileNode) -> std::io::Result<u32> {
        if let Some(n) = self.with_mem(rel, |pages| pages.len() as u32) {
            return Ok(n);
        }
        let f = self.file(rel)?;
        let len = f
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .metadata()?
            .len();
        Ok((len / BLCKSZ as u64) as u32)
    }

    /// The relation file's length in bytes. Separate from [`StorageManager::nblocks`]
    /// so a read can tell "past the end" (a fresh page) from "short of the end"
    /// (a truncated file) without rounding to whole blocks.
    fn file_len(&self, rel: RelFileNode) -> std::io::Result<u64> {
        let f = self.file(rel)?;
        let len = f
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .metadata()?
            .len();
        Ok(len)
    }

    /// Read one block. An all-zero block (a fresh or hole page) is returned as-is
    /// without a checksum check — the caller treats it as a new page; any other
    /// page must pass its checksum for `block`.
    pub fn read(&self, rel: RelFileNode, block: u32, buf: &mut Page) -> std::io::Result<()> {
        // A memory relation holds full pages in RAM; a block past its end (a fresh
        // insert target) reads as an all-zero page, exactly as a hole in a file
        // would. No checksum — RAM pages are never torn.
        if self
            .with_mem(rel, |pages| match pages.get(block as usize) {
                Some(page) => buf.copy_from_slice(page),
                None => buf.fill(0),
            })
            .is_some()
        {
            return Ok(());
        }
        // A block at or past the file's end reads as an all-zero page, the same
        // answer the memory path gives above. Redo reaches this case: a relation
        // whose file was created but never extended before the crash has zero
        // blocks, and erroring there would abort recovery rather than hand back
        // the fresh page the handler is about to overwrite.
        //
        // Deliberately gated on the *length*, not on a short read. Treating any
        // `UnexpectedEof` as a fresh page would also swallow a genuinely truncated
        // relation — a file whose tail was lost to an unfsynced extend, say —
        // turning missing rows into silence. Past the end is expected; short of
        // the end is corruption and still an error.
        let block_end = (block as u64 + 1) * BLCKSZ as u64;
        let len = self.file_len(rel)?;
        if len < block_end {
            buf.fill(0);
            return Ok(());
        }
        let f = self.file(rel)?;
        let mut f = f.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
        f.seek(SeekFrom::Start(block as u64 * BLCKSZ as u64))?;
        f.read_exact(buf)?;
        if buf.iter().all(|&b| b == 0) {
            return Ok(());
        }
        if !page::verify_checksum(buf, block) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("page checksum mismatch on relation {} block {block}", rel.0),
            ));
        }
        Ok(())
    }

    /// Write one block, stamping its checksum. The caller's buffer is not
    /// modified (the checksum is stamped on a copy).
    pub fn write(&self, rel: RelFileNode, block: u32, buf: &Page) -> std::io::Result<()> {
        #[cfg(test)]
        if self.fail_next_write.swap(false, Ordering::SeqCst) {
            return Err(std::io::Error::other("injected write failure"));
        }
        // Grow the page vector to cover the block, then store it verbatim (no
        // checksum: RAM is never verified on read).
        if self
            .with_mem(rel, |pages| {
                if pages.len() <= block as usize {
                    pages.resize(block as usize + 1, [0u8; BLCKSZ]);
                }
                pages[block as usize].copy_from_slice(buf);
            })
            .is_some()
        {
            return Ok(());
        }
        self.note_dirty(rel);
        let mut out = *buf;
        page::stamp_checksum(&mut out, block);
        let f = self.file(rel)?;
        let mut f = f.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
        f.seek(SeekFrom::Start(block as u64 * BLCKSZ as u64))?;
        f.write_all(&out)?;
        Ok(())
    }

    /// Append one freshly initialized block and return its block number. The
    /// whole operation runs under the file lock, so concurrent extenders never
    /// collide on the same block number.
    pub fn extend(&self, rel: RelFileNode) -> std::io::Result<u32> {
        if let Some(block) = self.with_mem(rel, |pages| {
            let block = pages.len() as u32;
            let mut fresh = [0u8; BLCKSZ];
            page::init(&mut fresh);
            pages.push(fresh);
            block
        }) {
            return Ok(block);
        }
        self.note_dirty(rel);
        let f = self.file(rel)?;
        let mut g = f.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
        let len = g.metadata()?.len();
        let block = (len / BLCKSZ as u64) as u32;
        let mut fresh = [0u8; BLCKSZ];
        page::init(&mut fresh);
        page::stamp_checksum(&mut fresh, block);
        g.seek(SeekFrom::End(0))?;
        g.write_all(&fresh)?;
        Ok(block)
    }

    /// Ensure a relation's (possibly empty) file exists on disk. Opening through
    /// [`StorageManager::file`] creates it with `create(true)` and caches the
    /// handle. Used to stage a relfilenode-swap TRUNCATE's fresh file and to
    /// materialize it during redo (idempotent: a no-op when the file exists).
    pub fn create_if_missing(&self, rel: RelFileNode) -> std::io::Result<()> {
        if self.is_mem(rel) {
            return Ok(()); // already materialized in RAM by register_memory
        }
        self.file(rel).map(|_| ())
    }

    /// Truncate a relation to zero blocks. A transactional TRUNCATE does not
    /// empty the table's heap file this way — it swaps to a fresh relfilenode —
    /// so the callers are the startup crash-reset of unlogged relations
    /// ([`crate::PgEngine::reset_unlogged_relations`]) and the post-commit
    /// reclaim of a truncated table's chunk store, which keeps its relfilenode.
    pub fn truncate(&self, rel: RelFileNode) -> std::io::Result<()> {
        if self.with_mem(rel, |pages| pages.clear()).is_some() {
            return Ok(());
        }
        // The `set_len` + `sync_data` below is the fsync this relation needed.
        self.forget_pending_fsync(rel);
        let f = self.file(rel)?;
        let g = f.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
        g.set_len(0)?;
        g.sync_data()
    }

    /// Delete a relation's file (DROP TABLE). Drops the cached handle first so the
    /// underlying inode is released, then unlinks. A missing file (the relation was
    /// never extended to disk) is not an error. The caller must evict the
    /// relation's buffered pages before calling this so nothing writes it back.
    pub fn unlink(&self, rel: RelFileNode) -> std::io::Result<()> {
        self.forget_pending_fsync(rel);
        // A memory relation has no file: drop its RAM pages and we are done.
        if self
            .mem
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .remove(&rel.0)
            .is_some()
        {
            return Ok(());
        }
        self.files
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .remove(&rel.0);
        let path = self.base.join(rel.0.to_string());
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Whether `rel` has bytes written but not yet fsynced.
    #[cfg(test)]
    pub(crate) fn fsync_pending(&self, rel: RelFileNode) -> bool {
        self.pending_fsync
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .contains(&rel.0)
    }

    /// Note that `rel` has bytes in the page cache that no fsync has covered yet.
    fn note_dirty(&self, rel: RelFileNode) {
        self.pending_fsync
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .insert(rel.0);
    }

    /// Forget any outstanding fsync for `rel` — its file is being destroyed.
    pub fn forget_pending_fsync(&self, rel: RelFileNode) {
        self.pending_fsync
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .remove(&rel.0);
    }

    /// fsync every relation written since the last successful call. The
    /// checkpoint's durability step: on return, every page write that happened
    /// before it is on stable storage.
    ///
    /// The drain happens under the lock and the fsyncs outside it, so a concurrent
    /// writer never waits behind one. An entry is removed only once its fsync has
    /// succeeded, so a transient failure is retried by the next checkpoint rather
    /// than silently forgotten — and entries added *during* the fsyncs survive,
    /// because they land in the fresh set the drain left behind.
    pub fn sync_pending(&self) -> std::io::Result<()> {
        // A block fsync buys nothing while the name pointing at it is not durable.
        self.sync_base_dir()?;

        let pending: Vec<u32> = std::mem::take(
            &mut *self
                .pending_fsync
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned")),
        )
        .into_iter()
        .collect();

        let mut failed = Vec::new();
        let mut first_error = None;
        for rel in pending {
            // Cache-only: see `cached_file`. A `None` here is a relation that was
            // unlinked since it was written, and it must stay unlinked.
            let Some(handle) = self.cached_file(rel) else {
                continue;
            };
            #[cfg(test)]
            let result = if self.fail_next_file_sync.swap(false, Ordering::SeqCst) {
                Err(std::io::Error::other("injected fsync failure"))
            } else {
                handle
                    .lock()
                    .unwrap_or_else(|_| panic!("mutex poisoned"))
                    .sync_data()
            };
            #[cfg(not(test))]
            let result = handle
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"))
                .sync_data();
            if let Err(error) = result {
                failed.push(rel);
                first_error.get_or_insert(error);
            }
        }
        if !failed.is_empty() {
            let mut set = self
                .pending_fsync
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"));
            set.extend(failed);
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn smgr() -> anyhow::Result<(tempfile::TempDir, StorageManager)> {
        let dir = tempfile::tempdir()?;
        let smgr = StorageManager::open(dir.path())?;
        Ok((dir, smgr))
    }

    fn pending(smgr: &StorageManager) -> usize {
        smgr.pending_fsync
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .len()
    }

    /// Run `f` with a watchdog; `None` means it never finished. Local copy of the
    /// wal crate's helper — a test module cannot import another crate's.
    fn within<T: Send + 'static>(max_ms: u64, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(std::time::Duration::from_millis(max_ms))
            .ok()
    }

    /// A page write is what queues the fsync, not the checkpoint noticing a dirty
    /// frame — which is the whole point, since an evicted frame no longer says
    /// what it held.
    #[test]
    fn writing_a_page_queues_its_relation_for_fsync() -> anyhow::Result<()> {
        let (_dir, smgr) = smgr()?;
        let page = [0u8; BLCKSZ];
        smgr.write(RelFileNode(1), 0, &page)?;
        smgr.write(RelFileNode(1), 1, &page)?;
        smgr.write(RelFileNode(2), 0, &page)?;
        assert_eq!(pending(&smgr), 2, "one entry per relation, not per page");

        smgr.sync_pending()?;
        assert_eq!(pending(&smgr), 0, "a successful drain clears the queue");

        Ok(())
    }

    /// `extend` writes bytes too, and is invisible to the buffer pool — a queue
    /// kept at the pool level would miss it entirely.
    #[test]
    fn extending_a_relation_queues_it_for_fsync() -> anyhow::Result<()> {
        let (_dir, smgr) = smgr()?;
        smgr.extend(RelFileNode(3))?;
        assert_eq!(pending(&smgr), 1);

        Ok(())
    }

    /// Memory relations have no file, so queueing them would make the drain look
    /// for a handle that cannot exist.
    #[test]
    fn a_memory_relation_is_never_queued_for_fsync() -> anyhow::Result<()> {
        let (_dir, smgr) = smgr()?;
        smgr.register_memory(RelFileNode(5));
        smgr.write(RelFileNode(5), 0, &[0u8; BLCKSZ])?;
        smgr.extend(RelFileNode(5))?;
        assert_eq!(pending(&smgr), 0);

        Ok(())
    }

    /// The drain must never reach `file()`, which opens with `create(true)`: a
    /// relation dropped between its last write and the checkpoint would come back
    /// as an empty file the catalog no longer names.
    ///
    /// `unlink` clearing the queue is the first defence, so the queue is re-armed
    /// here on purpose — that is the state a drain reaches when it snapshotted the
    /// relation just before a concurrent `unlink`, and it is what the cache-only
    /// lookup exists for. Without it this test passes whether the lookup creates
    /// or not.
    #[test]
    fn an_unlinked_relation_is_not_resurrected_by_a_drain() -> anyhow::Result<()> {
        let (dir, smgr) = smgr()?;
        let rel = RelFileNode(9);
        smgr.write(rel, 0, &[0u8; BLCKSZ])?;
        let path = dir.path().join("base").join("9");
        assert!(path.exists());

        smgr.unlink(rel)?;
        assert!(!path.exists());
        assert!(!smgr.fsync_pending(rel), "unlink clears the queue");

        smgr.note_dirty(rel);
        smgr.sync_pending()?;
        assert!(
            !path.exists(),
            "the drain recreated a relation file that was unlinked"
        );

        Ok(())
    }

    /// A transient fsync failure must leave the relation queued, or the next
    /// checkpoint publishes a redo point over page writes that never reached disk.
    #[test]
    fn a_failed_relation_fsync_is_retried_by_the_next_drain() -> anyhow::Result<()> {
        let (_dir, smgr) = smgr()?;
        smgr.write(RelFileNode(1), 0, &[0u8; BLCKSZ])?;

        smgr.fail_next_file_sync.store(true, Ordering::SeqCst);
        assert!(
            smgr.sync_pending().is_err(),
            "a failed fsync must surface, not be swallowed"
        );
        assert_eq!(pending(&smgr), 1, "the failed relation stays queued");

        smgr.sync_pending()?;
        assert_eq!(pending(&smgr), 0);

        Ok(())
    }

    /// The `base/` fsync used to be keyed on "did I just create this file", so one
    /// transient failure disabled it for that relation for the process lifetime.
    #[test]
    fn a_failed_base_directory_fsync_is_retried_by_the_next_open() -> anyhow::Result<()> {
        let (_dir, smgr) = smgr()?;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(true));
        {
            let calls = Arc::clone(&calls);
            let fail = Arc::clone(&fail);
            *smgr
                .dir_sync_hook
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned")) = Some(Arc::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                if fail.load(Ordering::SeqCst) {
                    Err(std::io::Error::other("injected directory fsync failure"))
                } else {
                    Ok(())
                }
            }));
        }

        assert!(
            smgr.file(RelFileNode(1)).is_err(),
            "the injected failure must surface"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // The handle is cached now, so this is a map HIT — the path that used to
        // skip the fsync forever.
        fail.store(false, Ordering::SeqCst);
        smgr.file(RelFileNode(1))?;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a cache hit must still complete an outstanding directory fsync"
        );

        // And once it succeeds it stops being retried.
        smgr.file(RelFileNode(1))?;
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        Ok(())
    }

    /// The directory fsync must not run under the `files` mutex, which sits below
    /// every page read and write: one relation being created would otherwise stall
    /// I/O on every other relation for the length of an fsync.
    #[test]
    fn opening_a_cached_relation_does_not_block_behind_a_directory_fsync() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let smgr = Arc::new(StorageManager::open(dir.path())?);
        // Cache a handle for relation 1 before the hook is armed.
        smgr.file(RelFileNode(1))?;

        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        {
            let gate = Arc::clone(&gate);
            *smgr
                .dir_sync_hook
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned")) = Some(Arc::new(move || {
                let (lock, cond) = &*gate;
                let mut open = lock.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
                while !*open {
                    open = cond.wait(open).unwrap_or_else(|_| panic!("mutex poisoned"));
                }
                Ok(())
            }));
        }

        // Creating relation 2 parks inside the directory fsync.
        let parked = {
            let smgr = Arc::clone(&smgr);
            std::thread::spawn(move || smgr.file(RelFileNode(2)))
        };

        let hit = {
            let smgr = Arc::clone(&smgr);
            within(2_000, move || smgr.nblocks(RelFileNode(1)))
        };
        // Open the gate before asserting, so a regression fails instead of leaving
        // the creator parked forever.
        {
            let (lock, cond) = &*gate;
            *lock.lock().unwrap_or_else(|_| panic!("mutex poisoned")) = true;
            cond.notify_all();
        }
        parked
            .join()
            .map_err(|_| anyhow::anyhow!("the parked creator panicked"))??;
        assert!(
            hit.is_some(),
            "a cached relation's I/O blocked behind another relation's directory fsync"
        );

        Ok(())
    }
}
