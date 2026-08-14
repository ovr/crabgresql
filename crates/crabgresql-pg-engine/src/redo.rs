//! Redo handlers for the heap resource manager. Each is LSN-gated — it applies a
//! record only when the target page's `pd_lsn` is below the record's LSN, then
//! stamps the page — so replay is idempotent across repeated crashes.

use std::sync::Arc;

use crabgresql_wal::{Lsn, RedoContext, RmgrRedo, WalError};

use crate::EngineInner;
use crate::page::{self, Page};
use crate::rec;
use crate::smgr::RelFileNode;
use crate::tuple;

pub struct HeapRedo {
    pub engine: Arc<EngineInner>,
}

impl HeapRedo {
    /// Pin `(rel, block)` and apply `f` only if the page predates `lsn`, then
    /// stamp the page LSN. The gate is what makes replay safe to repeat.
    fn apply(
        &self,
        rel: RelFileNode,
        block: u32,
        lsn: Lsn,
        f: impl FnOnce(&mut Page),
    ) -> Result<(), WalError> {
        // Keep the catalog's relfilenode counter above any file this replay
        // recreates, so a relfilenode issued after recovery can never alias one.
        // It matters most for a toast relation: created lazily mid-statement, its
        // id may be above anything the durable catalog names if the crash landed
        // between the two. The B-tree handler does the same, for the same reason.
        self.engine.catalog.observe_relfilenode(rel);
        let page = self.engine.bufpool.pin(rel, block)?;
        page.modify(|pg| {
            if page::get_lsn(pg) < lsn.0 {
                f(pg);
                page::set_lsn(pg, lsn.0);
            }
        });
        Ok(())
    }
}

impl RmgrRedo for HeapRedo {
    fn redo(&self, ctx: &RedoContext) -> Result<(), WalError> {
        let mut r = rec::R::new(ctx.payload);
        match ctx.info {
            rec::HEAP_INSERT => {
                let rel = r.rel();
                let block = r.u32();
                let off = r.u16();
                let tuple = r.bytes().to_vec();
                self.apply(rel, block, ctx.lsn, |pg| page::put_item_at(pg, off, &tuple))?;
            }
            rec::HEAP_MULTI_INSERT => {
                // Every item of this record landed on one page in one critical
                // section, so it replays as one gated apply: the page either
                // predates the record and takes the whole batch, or it already
                // holds all of it. Replaying item by item would need a gate per
                // item, and the page carries only one LSN.
                let rel = r.rel();
                let block = r.u32();
                let n = r.u32();
                let items: Vec<(u16, Vec<u8>)> = (0..n)
                    .map(|_| {
                        let off = r.u16();
                        (off, r.bytes().to_vec())
                    })
                    .collect();
                self.apply(rel, block, ctx.lsn, |pg| {
                    for (off, tuple) in &items {
                        page::put_item_at(pg, *off, tuple);
                    }
                })?;
            }
            rec::HEAP_DELETE => {
                let rel = r.rel();
                let block = r.u32();
                let off = r.u16();
                let xmax = r.xid();
                let cmax = r.cid();
                self.apply(rel, block, ctx.lsn, |pg| {
                    if let Some(item) = page::get_item_mut(pg, off) {
                        tuple::stamp_xmax(item, xmax, cmax);
                    }
                })?;
            }
            rec::HEAP_VACUUM => {
                let rel = r.rel();
                let block = r.u32();
                let n = r.u32();
                let offs: Vec<u16> = (0..n).map(|_| r.u16()).collect();
                self.apply(rel, block, ctx.lsn, |pg| {
                    for &off in &offs {
                        page::set_flags(pg, off, page::LP_UNUSED);
                    }
                    page::compact(pg);
                })?;
            }
            rec::HEAP_TRUNCATE => {
                // A relfilenode-swap TRUNCATE. Materialize the new (empty) file so
                // the same transaction's later inserts can redo into it, and
                // record the pending swap keyed by XID — but do NOT touch the
                // catalog or the old file here: the swap is applied after recovery
                // only if the transaction committed (see
                // `PgEngine::apply_recovered_truncates`). Never `set_len(0)` the
                // new file: it is a fresh, never-reused relfilenode whose only
                // contents are LSN-gated inserts that must survive replay.
                let old = r.rel();
                let new = r.rel();
                let namespace = String::from_utf8(r.bytes().to_vec())
                    .map_err(|e| WalError::Redo(format!("truncate record: bad namespace: {e}")))?;
                let table = String::from_utf8(r.bytes().to_vec())
                    .map_err(|e| WalError::Redo(format!("truncate record: bad table name: {e}")))?;
                self.engine.bufpool.smgr().create_if_missing(new)?;
                // The index swaps ride in this record; materialize their new
                // files for the same reason as the heap's above.
                let nindexes = r.u32();
                let mut indexes = Vec::with_capacity(nindexes as usize);
                for _ in 0..nindexes {
                    let name = String::from_utf8(r.bytes().to_vec()).map_err(|e| {
                        WalError::Redo(format!("truncate record: bad index name: {e}"))
                    })?;
                    let iold = r.rel();
                    let inew = r.rel();
                    self.engine.bufpool.smgr().create_if_missing(inew)?;
                    indexes.push((name, iold, inew));
                }
                self.engine
                    .recovered_truncates
                    .lock()
                    .unwrap_or_else(|_| panic!("mutex poisoned"))
                    .push(crate::RecoveredTruncate {
                        xid: ctx.xid,
                        namespace,
                        table,
                        old,
                        new,
                        indexes,
                        parquet: false,
                    });
            }
            other => return Err(WalError::Redo(format!("unknown heap info byte {other:#x}"))),
        }
        Ok(())
    }
}
