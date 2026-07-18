//! The durable heap access method: `TableAm` over slotted pages + buffer pool +
//! WAL. Visibility is the shared [`satisfies_mvcc`] rule applied to the on-page
//! [`TupleHeader`], so this engine and the in-memory engine agree bit-for-bit on
//! what a snapshot sees; only where the versions live differs.
//!
//! Every mutator follows the same write-ahead sequence inside the page's write
//! lock: change the page, append the WAL record, stamp `pd_lsn` with the record
//! LSN, mark the page dirty — so the page can never reach disk ahead of its log.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crabgresql_storage_api::{
    DeleteResult, TableAm, TableSchema, Tid, Tuple, UpdateResult,
};
use crabgresql_txn::{Clog, TupleHeader, TxnContext, Xid, XactStatus, satisfies_mvcc};
use crabgresql_wal::RmgrId;

use crate::EngineInner;
use crate::page::{self, PAGE_HEADER_LEN};
use crate::rec;
use crate::smgr::RelFileNode;
use crate::tuple::{self, TUPLE_HEADER_LEN};

/// Largest tuple that fits on an otherwise-empty page (no TOAST yet).
const MAX_TUPLE: usize = crate::page::BLCKSZ - PAGE_HEADER_LEN - 4;

pub struct HeapTable {
    schema: TableSchema,
    rel: RelFileNode,
    engine: Arc<EngineInner>,
    /// Last block we inserted into — where the next insert tries first.
    insert_hint: AtomicU32,
}

impl HeapTable {
    pub fn new(engine: Arc<EngineInner>, rel: RelFileNode, schema: TableSchema) -> HeapTable {
        HeapTable { schema, rel, engine, insert_hint: AtomicU32::new(0) }
    }

    #[allow(dead_code)] // used by tooling/tests and future index AM wiring
    pub fn relfilenode(&self) -> RelFileNode {
        self.rel
    }

    fn io<T>(r: std::io::Result<T>) -> T {
        // The storage-api trait is infallible; a disk error here is unrecoverable
        // for this backend, matching PostgreSQL's PANIC-on-I/O behavior.
        r.expect("heap engine I/O error")
    }

    /// Place `tuple_bytes` (a full on-page tuple with a placeholder ctid) onto a
    /// page, patch its self-ctid, log a HEAP_INSERT, and return its tid.
    fn place(&self, xid: Xid, tuple_bytes: &[u8]) -> Tid {
        assert!(
            tuple_bytes.len() <= MAX_TUPLE,
            "tuple of {} bytes exceeds page capacity (TOAST not implemented)",
            tuple_bytes.len()
        );
        let smgr = self.engine.bufpool.smgr();
        loop {
            let nblocks = Self::io(smgr.nblocks(self.rel));
            let target = if nblocks == 0 {
                Self::io(smgr.extend(self.rel))
            } else {
                self.insert_hint.load(Ordering::Relaxed).min(nblocks - 1)
            };
            let page = Self::io(self.engine.bufpool.pin(self.rel, target));
            let placed = page.modify(|pg| {
                let off = page::add_item(pg, tuple_bytes)?;
                let tid = Tid { block: target, offset: off };
                tuple::set_ctid(page::get_item_mut(pg, off).unwrap(), tid);
                let final_bytes = page::get_item(pg, off).unwrap().to_vec();
                let lsn = self.engine.wal.append(
                    RmgrId::HEAP,
                    rec::HEAP_INSERT,
                    xid,
                    &rec::insert(self.rel, target, off, &final_bytes),
                );
                page::set_lsn(pg, lsn.0);
                Some(tid)
            });
            if let Some(tid) = placed {
                self.insert_hint.store(target, Ordering::Relaxed);
                return tid;
            }
            // Page full: extend a fresh block and retry there.
            let fresh = Self::io(smgr.extend(self.rel));
            self.insert_hint.store(fresh, Ordering::Relaxed);
        }
    }
}

/// A version is still updatable/deletable unless a committed transaction deleted
/// it (an aborted or in-flight deleter leaves it live) — the same rule the
/// memory engine uses.
fn is_live(hdr: &TupleHeader, clog: &Clog) -> bool {
    !hdr.xmax.is_valid() || clog.status(hdr.xmax) == XactStatus::Aborted
}

impl TableAm for HeapTable {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    fn scan(&self, txn: &TxnContext) -> Box<dyn Iterator<Item = (Tid, Tuple)> + Send> {
        let nblocks = Self::io(self.engine.bufpool.smgr().nblocks(self.rel));
        Box::new(HeapScan {
            engine: Arc::clone(&self.engine),
            rel: self.rel,
            txn: txn.clone(),
            nblocks,
            cur_block: 0,
            buffer: Vec::new(),
            buf_idx: 0,
        })
    }

    fn fetch(&self, tid: Tid, txn: &TxnContext) -> Option<Tuple> {
        let smgr = self.engine.bufpool.smgr();
        if tid.block >= Self::io(smgr.nblocks(self.rel)) {
            return None;
        }
        let page = Self::io(self.engine.bufpool.pin(self.rel, tid.block));
        page.read(|pg| {
            let bytes = page::get_item(pg, tid.offset)?;
            let head = tuple::decode_header(bytes);
            if satisfies_mvcc(&head.hdr, &txn.snapshot, &txn.clog, txn.xid, txn.cid) {
                Some(tuple::decode_tuple(bytes).1)
            } else {
                None
            }
        })
    }

    fn insert(&self, tuple: Tuple, txn: &TxnContext) -> Tid {
        let hdr = TupleHeader::inserted(txn.xid, txn.cid);
        let bytes = tuple::encode_tuple(&tuple, &hdr, Tid { block: 0, offset: 0 });
        self.place(txn.xid, &bytes)
    }

    fn update(&self, tid: Tid, tuple: Tuple, txn: &TxnContext) -> UpdateResult {
        // Liveness check on the old version before we place anything, so a
        // vanished row never leaves an orphan new version behind.
        let old_live = {
            let page = Self::io(self.engine.bufpool.pin(self.rel, tid.block));
            page.read(|pg| {
                page::get_item(pg, tid.offset)
                    .map(|b| is_live(&tuple::decode_header(b).hdr, &txn.clog))
            })
        };
        match old_live {
            Some(true) => {}
            _ => return UpdateResult::NotFound,
        }

        // Place the new version (its own self-contained HEAP_INSERT).
        let hdr = TupleHeader::inserted(txn.xid, txn.cid);
        let new_bytes = tuple::encode_tuple(&tuple, &hdr, Tid { block: 0, offset: 0 });
        let new_tid = self.place(txn.xid, &new_bytes);

        // Stamp the old version deleted-by-us and forward-link it to the new one.
        let page = Self::io(self.engine.bufpool.pin(self.rel, tid.block));
        page.modify(|pg| {
            let item = page::get_item_mut(pg, tid.offset).unwrap();
            tuple::stamp_xmax(item, txn.xid, txn.cid);
            tuple::set_ctid(item, new_tid);
            let lsn = self.engine.wal.append(
                RmgrId::HEAP,
                rec::HEAP_UPDATE,
                txn.xid,
                &rec::update_old(
                    self.rel,
                    tid.block,
                    tid.offset,
                    txn.xid,
                    txn.cid,
                    new_tid.block,
                    new_tid.offset,
                ),
            );
            page::set_lsn(pg, lsn.0);
        });
        UpdateResult::Updated
    }

    fn delete(&self, tid: Tid, txn: &TxnContext) -> DeleteResult {
        if tid.block >= Self::io(self.engine.bufpool.smgr().nblocks(self.rel)) {
            return DeleteResult::NotFound;
        }
        let page = Self::io(self.engine.bufpool.pin(self.rel, tid.block));
        page.modify(|pg| {
            let Some(item) = page::get_item_mut(pg, tid.offset) else {
                return DeleteResult::NotFound;
            };
            if !is_live(&tuple::decode_header(item).hdr, &txn.clog) {
                return DeleteResult::NotFound;
            }
            tuple::stamp_xmax(item, txn.xid, txn.cid);
            let lsn = self.engine.wal.append(
                RmgrId::HEAP,
                rec::HEAP_DELETE,
                txn.xid,
                &rec::delete(self.rel, tid.block, tid.offset, txn.xid, txn.cid),
            );
            page::set_lsn(pg, lsn.0);
            DeleteResult::Deleted
        })
    }

    fn truncate(&self, txn: &TxnContext) {
        // Durably log the truncate before destroying data. Not transactional in
        // v1 (a rollback will not bring rows back), matching the memory engine's
        // documented limitation.
        let lsn = self.engine.wal.append(RmgrId::HEAP, rec::HEAP_TRUNCATE, txn.xid, &rec::truncate(self.rel));
        Self::io(self.engine.wal.flush(lsn).map_err(std::io::Error::other));
        self.engine.bufpool.forget_relation(self.rel);
        Self::io(self.engine.bufpool.smgr().truncate(self.rel));
        self.insert_hint.store(0, Ordering::Relaxed);
    }

    fn vacuum(&self, oldest: Xid, clog: &Clog) {
        let smgr = self.engine.bufpool.smgr();
        let nblocks = Self::io(smgr.nblocks(self.rel));
        for block in 0..nblocks {
            let page = Self::io(self.engine.bufpool.pin(self.rel, block));
            let freed: Vec<u16> = page.read(|pg| {
                let mut offs = Vec::new();
                for off in 1..=page::max_offset(pg) {
                    if let Some(bytes) = page::get_item(pg, off) {
                        let xmax = tuple::decode_header(bytes).hdr.xmax;
                        if xmax.is_valid() && xmax < oldest && clog.is_committed(xmax) {
                            offs.push(off);
                        }
                    }
                }
                offs
            });
            if freed.is_empty() {
                continue;
            }
            page.modify(|pg| {
                for &off in &freed {
                    page::set_flags(pg, off, page::LP_UNUSED);
                }
                page::compact(pg);
                let lsn = self.engine.wal.append(
                    RmgrId::HEAP,
                    rec::HEAP_VACUUM,
                    Xid::INVALID,
                    &rec::vacuum(self.rel, block, &freed),
                );
                page::set_lsn(pg, lsn.0);
            });
        }
    }
}

/// A snapshot-stable scan: it captures the block count up front and, per block,
/// pins the page, buffers the visible rows, then drops the pin before yielding
/// them — so no frame lock is ever held across an iterator step.
struct HeapScan {
    engine: Arc<EngineInner>,
    rel: RelFileNode,
    txn: TxnContext,
    nblocks: u32,
    cur_block: u32,
    buffer: Vec<(Tid, Tuple)>,
    buf_idx: usize,
}

impl Iterator for HeapScan {
    type Item = (Tid, Tuple);

    fn next(&mut self) -> Option<(Tid, Tuple)> {
        loop {
            if self.buf_idx < self.buffer.len() {
                let row = self.buffer[self.buf_idx].clone();
                self.buf_idx += 1;
                return Some(row);
            }
            if self.cur_block >= self.nblocks {
                return None;
            }
            let block = self.cur_block;
            self.cur_block += 1;
            self.buffer.clear();
            self.buf_idx = 0;
            let page = HeapTable::io(self.engine.bufpool.pin(self.rel, block));
            page.read(|pg| {
                for off in 1..=page::max_offset(pg) {
                    if let Some(bytes) = page::get_item(pg, off) {
                        // A visible tuple must at least be a full header long.
                        debug_assert!(bytes.len() >= TUPLE_HEADER_LEN);
                        let head = tuple::decode_header(bytes);
                        if satisfies_mvcc(
                            &head.hdr,
                            &self.txn.snapshot,
                            &self.txn.clog,
                            self.txn.xid,
                            self.txn.cid,
                        ) {
                            let (_, vals) = tuple::decode_tuple(bytes);
                            self.buffer.push((Tid { block, offset: off }, vals));
                        }
                    }
                }
            });
        }
    }
}
