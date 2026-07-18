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

// `RelFileNode` names the type of `apply`'s `rel` parameter below.

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
                let rel = r.rel();
                self.engine.bufpool.forget_relation(rel);
                self.engine.bufpool.smgr().truncate(rel)?;
            }
            other => return Err(WalError::Redo(format!("unknown heap info byte {other:#x}"))),
        }
        Ok(())
    }
}
