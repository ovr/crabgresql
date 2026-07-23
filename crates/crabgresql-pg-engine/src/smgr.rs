//! Storage manager: maps a relation to its on-disk file and does checksummed,
//! block-granular I/O. One file per relation under `<data_dir>/base/<relfilenode>`
//! (single segment; PG's 1 GB segmentation is a follow-up).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::page::{self, BLCKSZ, Page};

/// A relation's physical file identity (PostgreSQL's `relfilenode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RelFileNode(pub u32);

pub struct StorageManager {
    base: PathBuf,
    files: Mutex<HashMap<u32, Arc<Mutex<File>>>>,
    /// RAM-backed relations (memory tables: `relpersistence` `'u'`/`'t'`). A
    /// relfilenode present here holds its blocks in this vector instead of a file
    /// under `base/`; every smgr op routes here first. Its pages never touch disk
    /// and are lost on restart, matching UNLOGGED/TEMP semantics.
    mem: Mutex<HashMap<u32, Vec<Page>>>,
}

impl StorageManager {
    pub fn open(data_dir: &std::path::Path) -> std::io::Result<StorageManager> {
        let base = data_dir.join("base");
        std::fs::create_dir_all(&base)?;
        Ok(StorageManager {
            base,
            files: Mutex::new(HashMap::new()),
            mem: Mutex::new(HashMap::new()),
        })
    }

    /// Register `rel` as a RAM-backed memory relation. Idempotent — a repeated
    /// call (e.g. a memory table's TRUNCATE staging its new relfilenode) leaves an
    /// existing page vector untouched. Must be called before the relation is first
    /// pinned so every smgr op routes to RAM.
    pub fn register_memory(&self, rel: RelFileNode) {
        self.mem
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .entry(rel.0)
            .or_default();
    }

    /// Whether `rel` is a RAM-backed memory relation.
    fn is_mem(&self, rel: RelFileNode) -> bool {
        self.mem
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .contains_key(&rel.0)
    }

    fn file(&self, rel: RelFileNode) -> std::io::Result<Arc<Mutex<File>>> {
        let mut files = self
            .files
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        if let Some(f) = files.get(&rel.0) {
            return Ok(Arc::clone(f));
        }
        let path = self.base.join(rel.0.to_string());
        // truncate(false): a relation file's existing blocks must be preserved.
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let arc = Arc::new(Mutex::new(f));
        files.insert(rel.0, Arc::clone(&arc));
        Ok(arc)
    }

    /// Number of 8 KB blocks currently in the relation file.
    pub fn nblocks(&self, rel: RelFileNode) -> std::io::Result<u32> {
        if let Some(pages) = self
            .mem
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .get(&rel.0)
        {
            return Ok(pages.len() as u32);
        }
        let f = self.file(rel)?;
        let len = f
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .metadata()?
            .len();
        Ok((len / BLCKSZ as u64) as u32)
    }

    /// Read one block. An all-zero block (a fresh or hole page) is returned as-is
    /// without a checksum check — the caller treats it as a new page; any other
    /// page must pass its checksum for `block`.
    pub fn read(&self, rel: RelFileNode, block: u32, buf: &mut Page) -> std::io::Result<()> {
        if let Some(pages) = self
            .mem
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .get(&rel.0)
        {
            // A memory relation holds full pages in RAM; a block past its end (a
            // fresh insert target) reads as an all-zero page, exactly as a hole in
            // a file would. No checksum — RAM pages are never torn.
            match pages.get(block as usize) {
                Some(page) => buf.copy_from_slice(page),
                None => buf.fill(0),
            }
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
        if let Some(pages) = self
            .mem
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .get_mut(&rel.0)
        {
            // Grow the page vector to cover the block, then store it verbatim (no
            // checksum: RAM is never verified on read).
            if pages.len() <= block as usize {
                pages.resize(block as usize + 1, [0u8; BLCKSZ]);
            }
            pages[block as usize].copy_from_slice(buf);
            return Ok(());
        }
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
        if let Some(pages) = self
            .mem
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .get_mut(&rel.0)
        {
            let block = pages.len() as u32;
            let mut fresh = [0u8; BLCKSZ];
            page::init(&mut fresh);
            pages.push(fresh);
            return Ok(block);
        }
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

    /// Truncate a relation to zero blocks. No longer on the TRUNCATE path (which
    /// now swaps to a fresh relfilenode) but kept as part of the smgr API.
    #[allow(dead_code)]
    pub fn truncate(&self, rel: RelFileNode) -> std::io::Result<()> {
        if let Some(pages) = self
            .mem
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .get_mut(&rel.0)
        {
            pages.clear();
            return Ok(());
        }
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
        // A memory relation has no file: drop its RAM pages and we are done.
        if self
            .mem
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
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

    /// fsync a relation's file (called at checkpoint, after its dirty pages are
    /// written, so the data is durable before the checkpoint record).
    pub fn sync(&self, rel: RelFileNode) -> std::io::Result<()> {
        if self.is_mem(rel) {
            return Ok(()); // memory relations have nothing to fsync
        }
        let f = self.file(rel)?;
        let g = f.lock().unwrap_or_else(|_| panic!("mutex poisoned"));
        g.sync_data()
    }
}
