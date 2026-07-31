//! Redo handler for the B-tree resource manager. Like the heap's, each record is
//! LSN-gated — applied only when the target page predates the record — so replay
//! is idempotent across repeated crashes. Split and new-page records rebuild
//! whole pages from their logged image, which is trivially idempotent.

use std::sync::Arc;

use crabgresql_wal::{Lsn, RedoContext, RmgrRedo, WalError};

use crate::EngineInner;
use crate::btpage;
use crate::btrec;
use crate::page::{self, Page};
use crate::rec::R;
use crate::smgr::RelFileNode;

pub struct BtreeRedo {
    pub engine: Arc<EngineInner>,
}

impl BtreeRedo {
    /// Pin `(rel, block)` and apply `f` only if the page predates `lsn`, then
    /// stamp the page LSN — the same idempotence gate as `HeapRedo::apply`. Also
    /// keeps the catalog's relfilenode counter above any index file it recreates,
    /// so a freshly issued relfilenode can never alias an index file replayed
    /// from the WAL.
    fn apply(
        &self,
        rel: RelFileNode,
        block: u32,
        lsn: Lsn,
        f: impl FnOnce(&mut Page),
    ) -> Result<(), WalError> {
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

impl RmgrRedo for BtreeRedo {
    fn redo(&self, ctx: &RedoContext) -> Result<(), WalError> {
        let mut r = R::new(ctx.payload);
        match ctx.info {
            btrec::BT_INSERT => {
                let rel = r.rel();
                let block = r.u32();
                let off = r.u16();
                let item = r.bytes().to_vec();
                self.apply(rel, block, ctx.lsn, |pg| {
                    page::insert_item_at(pg, off, &item);
                })?;
            }
            btrec::BT_DELETE => {
                let rel = r.rel();
                let block = r.u32();
                let off = r.u16();
                self.apply(rel, block, ctx.lsn, |pg| page::remove_item_at(pg, off))?;
            }
            btrec::BT_PAGE => {
                let rel = r.rel();
                let block = r.u32();
                let opaque = btrec::read_opaque(&mut r);
                let items = btrec::read_items(&mut r);
                self.apply(rel, block, ctx.lsn, |pg| {
                    btpage::rebuild(pg, &opaque, &items);
                })?;
            }
            btrec::BT_META => {
                let rel = r.rel();
                let root = r.u32();
                let level = r.u32();
                self.apply(rel, btpage::META_BLOCK, ctx.lsn, |pg| {
                    btpage::init_meta(pg);
                    page::put_item_at(pg, 1, &btrec::encode_meta(root, level));
                })?;
            }
            btrec::BT_SPLIT => {
                let rel = r.rel();
                let left_blk = r.u32();
                let right_blk = r.u32();
                let left_opaque = btrec::read_opaque(&mut r);
                let right_opaque = btrec::read_opaque(&mut r);
                let left_items = btrec::read_items(&mut r);
                let right_items = btrec::read_items(&mut r);
                let old_right_sibling = r.u32();
                self.apply(rel, left_blk, ctx.lsn, |pg| {
                    btpage::rebuild(pg, &left_opaque, &left_items);
                })?;
                self.apply(rel, right_blk, ctx.lsn, |pg| {
                    btpage::rebuild(pg, &right_opaque, &right_items);
                })?;
                if old_right_sibling != btpage::INVALID_BLOCK {
                    self.apply(rel, old_right_sibling, ctx.lsn, |pg| {
                        let mut o = btpage::get_opaque(pg);
                        o.prev = right_blk;
                        btpage::set_opaque(pg, &o);
                    })?;
                }
            }
            other => {
                return Err(WalError::Redo(format!(
                    "unknown btree info byte {other:#x}"
                )));
            }
        }
        Ok(())
    }
}
