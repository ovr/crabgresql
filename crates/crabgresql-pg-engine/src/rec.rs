//! Payload encode/decode for the heap resource manager's WAL records. Shared by
//! the heap AM (which logs) and the redo handler (which replays). Little-endian.

use crabgresql_txn::{CommandId, Xid};

use crate::page::BLCKSZ;
use crate::smgr::RelFileNode;

// `info` byte opcodes for [`crabgresql_wal::RmgrId::HEAP`] records.
pub const HEAP_INSERT: u8 = 1;
pub const HEAP_DELETE: u8 = 2;
// opcode 3 (HEAP_UPDATE) retired: an update logs its old-version stamp as a
// HEAP_DELETE and its new version as a HEAP_INSERT (see heap::update).
pub const HEAP_VACUUM: u8 = 4;
pub const HEAP_TRUNCATE: u8 = 5;
pub const HEAP_MULTI_INSERT: u8 = 6;

struct W(Vec<u8>);
impl W {
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
}

pub struct R<'a> {
    b: &'a [u8],
    p: usize,
}
impl<'a> R<'a> {
    pub fn new(b: &'a [u8]) -> R<'a> {
        R { b, p: 0 }
    }
    pub fn u16(&mut self) -> u16 {
        let v = u16::from_le_bytes(self.array());
        self.p += 2;
        v
    }
    pub fn u32(&mut self) -> u32 {
        let v = u32::from_le_bytes(self.array());
        self.p += 4;
        v
    }
    pub fn u64(&mut self) -> u64 {
        let v = u64::from_le_bytes(self.array());
        self.p += 8;
        v
    }
    pub fn bytes(&mut self) -> &'a [u8] {
        let n = self.u32() as usize;
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        s
    }
    pub fn rel(&mut self) -> RelFileNode {
        RelFileNode(self.u32())
    }
    pub fn xid(&mut self) -> Xid {
        Xid(self.u64())
    }
    pub fn cid(&mut self) -> CommandId {
        CommandId(self.u32())
    }

    fn array<const N: usize>(&self) -> [u8; N] {
        let Some(slice) = self.b.get(self.p..self.p + N) else {
            panic!("heap WAL record is truncated");
        };
        let mut out = [0; N];
        out.copy_from_slice(slice);
        out
    }
}

pub fn insert(rel: RelFileNode, block: u32, off: u16, tuple: &[u8]) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u32(rel.0);
    w.u32(block);
    w.u16(off);
    w.bytes(tuple);
    w.0
}

/// Builder for a `HEAP_MULTI_INSERT`: every tuple a batch placed on a single
/// page, in one record. Layout:
/// `[rel:u32][block:u32][n:u32] { [off:u16][len:u32][bytes] }*`.
///
/// Grown item by item rather than encoded from a collected list, so the batch
/// insert can append each tuple straight from the page it just wrote it to —
/// there is no copy of the tuple bytes anywhere between the page and the log.
/// `n` is not known until the page fills, so it is written as a placeholder and
/// patched by [`MultiInsert::finish`].
pub struct MultiInsert {
    w: W,
    n: u32,
    /// Captured as the header is written rather than spelled as a constant, which
    /// would silently rot the day a header field is added.
    count_at: usize,
}

impl MultiInsert {
    pub fn new(rel: RelFileNode, block: u32) -> MultiInsert {
        // The record describes one page, so it can never outgrow one.
        let mut w = W(Vec::with_capacity(BLCKSZ));
        w.u32(rel.0);
        w.u32(block);
        let count_at = w.0.len();
        w.u32(0);
        MultiInsert { w, n: 0, count_at }
    }

    pub fn push(&mut self, off: u16, tuple: &[u8]) {
        self.w.u16(off);
        self.w.bytes(tuple);
        self.n += 1;
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    pub fn finish(mut self) -> Vec<u8> {
        let at = self.count_at;
        self.w.0[at..at + 4].copy_from_slice(&self.n.to_le_bytes());
        self.w.0
    }
}

pub fn delete(rel: RelFileNode, block: u32, off: u16, xmax: Xid, cmax: CommandId) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u32(rel.0);
    w.u32(block);
    w.u16(off);
    w.u64(xmax.0);
    w.u32(cmax.0);
    w.0
}

pub fn vacuum(rel: RelFileNode, block: u32, offs: &[u16]) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u32(rel.0);
    w.u32(block);
    w.u32(offs.len() as u32);
    for &o in offs {
        w.u16(o);
    }
    w.0
}

/// A relfilenode-swap TRUNCATE: the relation's old (still-live) file and the new
/// empty file staged for it, plus the relation's schema-qualified name so recovery
/// can rebind the catalog once it knows the transaction's fate. Layout:
/// `[old:u32][new:u32][ns_len:u32][ns][name_len:u32][name]`
/// `[nindexes:u32] { [name_len:u32][name][old:u32][new:u32] }*`.
///
/// The namespace is on the wire because relations are keyed by `(namespace, name)`:
/// assuming `public` would make recovery resolve `app.t` against `public.t` — either
/// a different relation or none at all.
///
/// The index swaps ride in this record rather than one of their own so that a
/// crash cannot land *between* them: recovery would then apply a committed heap
/// swap over unswapped indexes, which is exactly the corruption the swap exists
/// to prevent. One record, one verdict, one set of files. Only physical indexes
/// appear — a metadata-only one (relfilenode 0) has no file to swap.
pub fn truncate(
    namespace: &str,
    table: &str,
    old: RelFileNode,
    new: RelFileNode,
    indexes: &[(String, RelFileNode, RelFileNode)],
) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u32(old.0);
    w.u32(new.0);
    w.bytes(namespace.as_bytes());
    w.bytes(table.as_bytes());
    w.u32(indexes.len() as u32);
    for (name, old, new) in indexes {
        w.bytes(name.as_bytes());
        w.u32(old.0);
        w.u32(new.0);
    }
    w.0
}
