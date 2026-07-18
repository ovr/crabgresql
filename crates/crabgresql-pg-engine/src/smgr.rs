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
}

impl StorageManager {
    pub fn open(data_dir: &std::path::Path) -> std::io::Result<StorageManager> {
        let base = data_dir.join("base");
        std::fs::create_dir_all(&base)?;
        Ok(StorageManager { base, files: Mutex::new(HashMap::new()) })
    }

    fn file(&self, rel: RelFileNode) -> std::io::Result<Arc<Mutex<File>>> {
        let mut files = self.files.lock().unwrap();
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
        let f = self.file(rel)?;
        let len = f.lock().unwrap().metadata()?.len();
        Ok((len / BLCKSZ as u64) as u32)
    }

    /// Read one block. An all-zero block (a fresh or hole page) is returned as-is
    /// without a checksum check — the caller treats it as a new page; any other
    /// page must pass its checksum for `block`.
    pub fn read(&self, rel: RelFileNode, block: u32, buf: &mut Page) -> std::io::Result<()> {
        let f = self.file(rel)?;
        let mut f = f.lock().unwrap();
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
        let mut out = *buf;
        page::stamp_checksum(&mut out, block);
        let f = self.file(rel)?;
        let mut f = f.lock().unwrap();
        f.seek(SeekFrom::Start(block as u64 * BLCKSZ as u64))?;
        f.write_all(&out)?;
        Ok(())
    }

    /// Append one freshly initialized block and return its block number. The
    /// whole operation runs under the file lock, so concurrent extenders never
    /// collide on the same block number.
    pub fn extend(&self, rel: RelFileNode) -> std::io::Result<u32> {
        let f = self.file(rel)?;
        let mut g = f.lock().unwrap();
        let len = g.metadata()?.len();
        let block = (len / BLCKSZ as u64) as u32;
        let mut fresh = [0u8; BLCKSZ];
        page::init(&mut fresh);
        page::stamp_checksum(&mut fresh, block);
        g.seek(SeekFrom::End(0))?;
        g.write_all(&fresh)?;
        Ok(block)
    }

    /// Truncate a relation to zero blocks (TRUNCATE and its redo).
    pub fn truncate(&self, rel: RelFileNode) -> std::io::Result<()> {
        let f = self.file(rel)?;
        let g = f.lock().unwrap();
        g.set_len(0)?;
        g.sync_data()
    }

    /// fsync a relation's file (called at checkpoint, after its dirty pages are
    /// written, so the data is durable before the checkpoint record).
    pub fn sync(&self, rel: RelFileNode) -> std::io::Result<()> {
        let f = self.file(rel)?;
        let g = f.lock().unwrap();
        g.sync_data()
    }
}
