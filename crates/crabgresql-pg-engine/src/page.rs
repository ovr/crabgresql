//! The 8 KB slotted heap page: header, line-pointer (item id) array growing up
//! from the header, and tuple data growing down from the end. Reproduces the
//! observable structure of a PostgreSQL heap page — a genuine `(block, offset)`
//! `ctid`, `pd_lsn` for the write-ahead rule, and a page checksum — without
//! copying its C layout. All access is little-endian slice reads, so on-page
//! datums need no alignment padding (we deliberately skip PG's `MAXALIGN`; it
//! only affects space accounting, not observable behavior).

pub const BLCKSZ: usize = 8192;
/// Fixed page-header length; the line-pointer array starts here.
pub const PAGE_HEADER_LEN: usize = 24;

// Header field byte offsets.
const OFF_LSN: usize = 0; // u64
const OFF_CHECKSUM: usize = 8; // u16
#[allow(dead_code)] // reserved header field (all-visible/prune hints); kept for layout fidelity
const OFF_FLAGS: usize = 10; // u16
const OFF_LOWER: usize = 12; // u16 — end of line-pointer array
const OFF_UPPER: usize = 14; // u16 — start of tuple area
const OFF_SPECIAL: usize = 16; // u16 — start of special space (= BLCKSZ for heap)
const OFF_PAGESIZE: usize = 18; // u16
#[allow(dead_code)] // reserved pruning hint; kept for layout fidelity
const OFF_PRUNE_XID: usize = 20; // u32

const PAGESIZE_VERSION: u16 = BLCKSZ as u16 | 0x01;

// Line-pointer (ItemId) flags, in the 2-bit field.
pub const LP_UNUSED: u8 = 0;
pub const LP_NORMAL: u8 = 1;
#[allow(dead_code)] // vacuum could mark tuples DEAD before reclaiming; not used in this cut
pub const LP_DEAD: u8 = 3;

pub type Page = [u8; BLCKSZ];

fn bytes<const N: usize>(p: &Page, off: usize) -> [u8; N] {
    let Some(slice) = p.get(off..off + N) else {
        panic!("page field is out of bounds");
    };
    let mut out = [0; N];
    out.copy_from_slice(slice);
    out
}

fn rd_u16(p: &Page, off: usize) -> u16 {
    u16::from_le_bytes(bytes(p, off))
}
fn wr_u16(p: &mut Page, off: usize, v: u16) {
    p[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

pub fn get_lsn(p: &Page) -> u64 {
    u64::from_le_bytes(bytes(p, OFF_LSN))
}
pub fn set_lsn(p: &mut Page, lsn: u64) {
    p[OFF_LSN..OFF_LSN + 8].copy_from_slice(&lsn.to_le_bytes());
}

fn lower(p: &Page) -> usize {
    rd_u16(p, OFF_LOWER) as usize
}
fn upper(p: &Page) -> usize {
    rd_u16(p, OFF_UPPER) as usize
}

/// Whether the page has been initialized (a freshly extended, all-zero page has
/// not, and must be [`init`]ed before use).
pub fn is_initialized(p: &Page) -> bool {
    rd_u16(p, OFF_PAGESIZE) == PAGESIZE_VERSION
}

/// Initialize an empty heap page: no line pointers, all space free.
pub fn init(p: &mut Page) {
    p.fill(0);
    wr_u16(p, OFF_LOWER, PAGE_HEADER_LEN as u16);
    wr_u16(p, OFF_UPPER, BLCKSZ as u16);
    wr_u16(p, OFF_SPECIAL, BLCKSZ as u16);
    wr_u16(p, OFF_PAGESIZE, PAGESIZE_VERSION);
}

/// Free bytes available between the line-pointer array and the tuple area.
pub fn free_space(p: &Page) -> usize {
    upper(p).saturating_sub(lower(p))
}

/// Highest 1-based line-pointer number in use (0 if none).
pub fn max_offset(p: &Page) -> u16 {
    ((lower(p) - PAGE_HEADER_LEN) / 4) as u16
}

/// A line pointer: `(offset, flags, length)` packed into a `u32`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ItemId(u32);

impl ItemId {
    pub fn new(off: u16, flags: u8, len: u16) -> ItemId {
        ItemId((off as u32 & 0x7fff) | ((flags as u32 & 0x3) << 15) | ((len as u32 & 0x7fff) << 17))
    }
    pub fn off(self) -> u16 {
        (self.0 & 0x7fff) as u16
    }
    pub fn flags(self) -> u8 {
        ((self.0 >> 15) & 0x3) as u8
    }
    pub fn len(self) -> u16 {
        ((self.0 >> 17) & 0x7fff) as u16
    }
}

fn lp_pos(off: u16) -> usize {
    // 1-based line-pointer number: offset 0 is invalid (PG OffsetNumber).
    PAGE_HEADER_LEN + (off as usize - 1) * 4
}

pub fn item_id(p: &Page, off: u16) -> ItemId {
    let at = lp_pos(off);
    ItemId(u32::from_le_bytes(bytes(p, at)))
}

fn set_item_id(p: &mut Page, off: u16, id: ItemId) {
    let at = lp_pos(off);
    p[at..at + 4].copy_from_slice(&id.0.to_le_bytes());
}

/// Mark a line pointer's flags (e.g. NORMAL -> DEAD/UNUSED during vacuum).
pub fn set_flags(p: &mut Page, off: u16, flags: u8) {
    let id = item_id(p, off);
    set_item_id(p, off, ItemId::new(id.off(), flags, id.len()));
}

/// Add `bytes` as a new tuple, returning its 1-based line-pointer number, or
/// `None` if the page has no room. Reuses the first UNUSED line pointer if one
/// exists (its old tuple was already vacuumed away), else appends a new one.
pub fn add_item(p: &mut Page, bytes: &[u8]) -> Option<u16> {
    let len = bytes.len();
    let maxoff = max_offset(p);
    let mut reuse = None;
    for off in 1..=maxoff {
        if item_id(p, off).flags() == LP_UNUSED {
            reuse = Some(off);
            break;
        }
    }
    let lp_cost = if reuse.is_some() { 0 } else { 4 };
    if len + lp_cost > free_space(p) {
        return None;
    }
    let new_upper = upper(p) - len;
    p[new_upper..new_upper + len].copy_from_slice(bytes);
    wr_u16(p, OFF_UPPER, new_upper as u16);
    let off = match reuse {
        Some(o) => o,
        None => {
            let o = maxoff + 1;
            wr_u16(p, OFF_LOWER, (lower(p) + 4) as u16);
            o
        }
    };
    set_item_id(p, off, ItemId::new(new_upper as u16, LP_NORMAL, len as u16));
    Some(off)
}

/// Place a tuple at a specific line-pointer number (used by redo, which must
/// reproduce the exact offset the original insert chose). Extends the
/// line-pointer array with UNUSED entries as needed.
pub fn put_item_at(p: &mut Page, off: u16, bytes: &[u8]) {
    // Grow the line-pointer array so `off` is addressable.
    while max_offset(p) < off {
        let next = max_offset(p) + 1;
        wr_u16(p, OFF_LOWER, (lower(p) + 4) as u16);
        set_item_id(p, next, ItemId::new(0, LP_UNUSED, 0));
    }
    let len = bytes.len();
    let new_upper = upper(p) - len;
    p[new_upper..new_upper + len].copy_from_slice(bytes);
    wr_u16(p, OFF_UPPER, new_upper as u16);
    set_item_id(p, off, ItemId::new(new_upper as u16, LP_NORMAL, len as u16));
}

/// Borrow a NORMAL tuple's bytes, or `None` if the slot is not NORMAL.
pub fn get_item(p: &Page, off: u16) -> Option<&[u8]> {
    if off == 0 || off > max_offset(p) {
        return None;
    }
    let id = item_id(p, off);
    if id.flags() != LP_NORMAL {
        return None;
    }
    let start = id.off() as usize;
    Some(&p[start..start + id.len() as usize])
}

/// Mutably borrow a NORMAL tuple's bytes for an in-place, same-length update
/// (stamping xmax / rewriting the forward ctid).
pub fn get_item_mut(p: &mut Page, off: u16) -> Option<&mut [u8]> {
    if off == 0 || off > max_offset(p) {
        return None;
    }
    let id = item_id(p, off);
    if id.flags() != LP_NORMAL {
        return None;
    }
    let start = id.off() as usize;
    let len = id.len() as usize;
    Some(&mut p[start..start + len])
}

/// Reclaim the space of UNUSED/DEAD line pointers by repacking live tuples
/// against the end of the page (PostgreSQL's `PageRepairFragmentation`).
pub fn compact(p: &mut Page) {
    let maxoff = max_offset(p);
    // Collect live tuples (offset -> bytes), preserving line-pointer numbers.
    let mut live: Vec<(u16, Vec<u8>)> = Vec::new();
    for off in 1..=maxoff {
        let id = item_id(p, off);
        if id.flags() == LP_NORMAL {
            let start = id.off() as usize;
            live.push((off, p[start..start + id.len() as usize].to_vec()));
        }
    }
    // Rewrite the tuple area from the end.
    let mut cur_upper = BLCKSZ;
    for (off, bytes) in &live {
        cur_upper -= bytes.len();
        p[cur_upper..cur_upper + bytes.len()].copy_from_slice(bytes);
        set_item_id(
            p,
            *off,
            ItemId::new(cur_upper as u16, LP_NORMAL, bytes.len() as u16),
        );
    }
    wr_u16(p, OFF_UPPER, cur_upper as u16);
}

// -- Page checksum (CRC-32C folded to 16 bits, mixing the block number so a page
// -- landing in the wrong slot is detected). The checksum field is treated as
// -- zero while computing.

fn compute_checksum(p: &Page, blockno: u32) -> u16 {
    let mut c = crc32c::crc32c(&p[..OFF_CHECKSUM]);
    c = crc32c::crc32c_append(c, &[0, 0]);
    c = crc32c::crc32c_append(c, &p[OFF_CHECKSUM + 2..]);
    c ^= blockno;
    let folded = ((c >> 16) as u16) ^ (c as u16);
    // Map into 1..=65535 so an all-zero (never-written) page stays distinguishable,
    // without masking a whole detection bit the way `| 1` would (PostgreSQL's
    // pg_checksum_page uses the same `(x % 65535) + 1`).
    ((folded as u32 % 65535) + 1) as u16
}

/// Stamp the page's checksum for block `blockno` in place (called just before
/// writing to disk).
pub fn stamp_checksum(p: &mut Page, blockno: u32) {
    let ck = compute_checksum(p, blockno);
    wr_u16(p, OFF_CHECKSUM, ck);
}

/// Verify a page just read from block `blockno`.
pub fn verify_checksum(p: &Page, blockno: u32) -> bool {
    rd_u16(p, OFF_CHECKSUM) == compute_checksum(p, blockno)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn required<T>(value: Option<T>, message: &str) -> anyhow::Result<T> {
        value.ok_or_else(|| anyhow::anyhow!(message.to_string()))
    }

    fn new_page() -> Box<Page> {
        let mut p = Box::new([0u8; BLCKSZ]);
        init(&mut p);
        p
    }

    #[test]
    fn init_then_add_and_read() -> anyhow::Result<()> {
        let mut p = new_page();
        assert!(is_initialized(&p));
        assert_eq!(free_space(&p), BLCKSZ - PAGE_HEADER_LEN);
        let a = required(add_item(&mut p, b"hello"), "hello did not fit")?;
        let b = required(add_item(&mut p, b"world!!"), "world did not fit")?;
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(required(get_item(&p, a), "hello is missing")?, b"hello");
        assert_eq!(required(get_item(&p, b), "world is missing")?, b"world!!");
        assert_eq!(max_offset(&p), 2);

        Ok(())
    }

    #[test]
    fn fill_until_no_room() -> anyhow::Result<()> {
        let mut p = new_page();
        let mut n = 0;
        while add_item(&mut p, &[7u8; 100]).is_some() {
            n += 1;
        }
        assert!(n > 0);
        // Every stored item still reads back.
        for off in 1..=n {
            assert_eq!(
                required(get_item(&p, off as u16), "item is missing")?,
                &[7u8; 100]
            );
        }

        Ok(())
    }

    #[test]
    fn compact_reclaims_dead_space_and_reuses_slot() -> anyhow::Result<()> {
        let mut p = new_page();
        let a = required(add_item(&mut p, b"aaaa"), "aaaa did not fit")?;
        let b = required(add_item(&mut p, b"bbbb"), "bbbb did not fit")?;
        let _c = required(add_item(&mut p, b"cccc"), "cccc did not fit")?;
        set_flags(&mut p, b, LP_UNUSED);
        let before = free_space(&p);
        compact(&mut p);
        assert!(
            free_space(&p) > before,
            "compaction frees the dead tuple's bytes"
        );
        assert_eq!(required(get_item(&p, a), "aaaa is missing")?, b"aaaa");
        // The UNUSED slot is reused by the next insert.
        let reused = required(add_item(&mut p, b"dddd"), "dddd did not fit")?;
        assert_eq!(reused, b);

        Ok(())
    }

    #[test]
    fn checksum_is_never_zero() {
        // The zero-avoidance must never yield 0 (which would alias an all-zero
        // page), across varied content and block numbers.
        let mut p = new_page();
        for b in 0..512u32 {
            p[100] = (b & 0xff) as u8;
            p[4000] = (b >> 1) as u8;
            assert_ne!(compute_checksum(&p, b), 0);
        }
    }

    #[test]
    fn checksum_detects_corruption_and_wrong_block() -> anyhow::Result<()> {
        let mut p = new_page();
        required(add_item(&mut p, b"payload"), "payload did not fit")?;
        stamp_checksum(&mut p, 5);
        assert!(verify_checksum(&p, 5));
        assert!(!verify_checksum(&p, 6), "wrong block number fails");
        let mut torn = p.clone();
        torn[BLCKSZ - 1] ^= 0xff;
        assert!(!verify_checksum(&torn, 5), "flipped byte fails");

        Ok(())
    }

    #[test]
    fn put_item_at_reproduces_offset() -> anyhow::Result<()> {
        let mut p = new_page();
        put_item_at(&mut p, 3, b"redo");
        assert_eq!(required(get_item(&p, 3), "redo item is missing")?, b"redo");
        assert_eq!(item_id(&p, 1).flags(), LP_UNUSED);
        assert_eq!(item_id(&p, 2).flags(), LP_UNUSED);

        Ok(())
    }
}
