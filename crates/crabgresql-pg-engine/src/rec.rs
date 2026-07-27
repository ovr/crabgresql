//! Payload encode/decode for the heap resource manager's WAL records. Shared by
//! the heap AM (which logs) and the redo handler (which replays). Little-endian.

use crabgresql_txn::{CommandId, Xid};

use crate::smgr::RelFileNode;

// `info` byte opcodes for [`crabgresql_wal::RmgrId::HEAP`] records.
pub const HEAP_INSERT: u8 = 1;
pub const HEAP_DELETE: u8 = 2;
// opcode 3 (HEAP_UPDATE) retired: an update logs its old-version stamp as a
// HEAP_DELETE and its new version as a HEAP_INSERT (see heap::update).
pub const HEAP_VACUUM: u8 = 4;
pub const HEAP_TRUNCATE: u8 = 5;

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
/// `[old:u32][new:u32][ns_len:u32][ns][name_len:u32][name]`.
///
/// The namespace is on the wire because relations are keyed by `(namespace, name)`:
/// assuming `public` would make recovery resolve `app.t` against `public.t` — either
/// a different relation or none at all.
pub fn truncate(namespace: &str, table: &str, old: RelFileNode, new: RelFileNode) -> Vec<u8> {
    let mut w = W(Vec::new());
    w.u32(old.0);
    w.u32(new.0);
    w.bytes(namespace.as_bytes());
    w.bytes(table.as_bytes());
    w.0
}
