//! Resource-manager identifiers and the redo-handler registry.

use std::collections::HashMap;
use std::sync::Arc;

use crabgresql_txn::Xid;

use crate::record::{Lsn, WalError};

/// A resource-manager id — the dispatch key that routes a record to its redo
/// handler. Ids `0..10` are reserved for core services; engines pick `>= 10`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RmgrId(pub u8);

impl RmgrId {
    /// Transaction commit/abort records (owned by this crate).
    pub const XACT: RmgrId = RmgrId(0);
    /// Checkpoint records (owned by this crate).
    pub const CHECKPOINT: RmgrId = RmgrId(1);
    /// Records about the log itself rather than about any relation (owned by
    /// this crate). Currently only segment padding.
    pub const XLOG: RmgrId = RmgrId(2);
    /// The heap access method (`crabgresql-pg-engine`).
    pub const HEAP: RmgrId = RmgrId(10);
}

/// `info` byte values for [`RmgrId::XACT`] records.
pub const XACT_COMMIT: u8 = 0x01;
pub const XACT_ABORT: u8 = 0x02;

/// The `info` byte of a [`RmgrId::XLOG`] padding record: filler that carries the
/// stream from where a record would no longer fit to the end of its segment. Its
/// payload is zeros and replay ignores it.
pub const XLOG_PAD: u8 = 0x01;

/// Everything a redo handler sees for one record during recovery. A handler must
/// be **idempotent**: apply the change only when the target page's LSN is below
/// `lsn`, then stamp the page with `lsn`. That gate is what lets recovery replay
/// the same record any number of times across repeated crashes.
pub struct RedoContext<'a> {
    /// The record's end-LSN — the value to stamp on any page it touches.
    pub lsn: Lsn,
    pub xid: Xid,
    pub info: u8,
    pub payload: &'a [u8],
}

/// A resource manager's redo entry point.
pub trait RmgrRedo: Send + Sync {
    fn redo(&self, ctx: &RedoContext) -> Result<(), WalError>;
}

/// Maps a resource-manager id to its redo handler. Assembled at startup, before
/// recovery, so every record type that might appear in the log has a handler.
#[derive(Default)]
pub struct RmgrRegistry {
    handlers: HashMap<u8, Arc<dyn RmgrRedo>>,
}

impl RmgrRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, id: RmgrId, redo: Arc<dyn RmgrRedo>) {
        self.handlers.insert(id.0, redo);
    }

    pub fn get(&self, id: u8) -> Option<&Arc<dyn RmgrRedo>> {
        self.handlers.get(&id)
    }
}
