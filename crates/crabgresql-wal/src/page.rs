//! WAL pages: the fixed-size unit the log is written in, and the LSN arithmetic
//! that steps over their headers.
//!
//! The stream is a sequence of [`XLOG_BLCKSZ`]-byte pages, each opening with a
//! [`XLP_PAGE_HEADER_SIZE`]-byte header. Records are packed into the space
//! between headers and split across pages whenever they do not fit, so a page
//! boundary is invisible to a record's own encoding.
//!
//! Two properties are what the rest of the crate is built on:
//!
//! * **Writes are whole pages at page-aligned offsets.** A partial write into
//!   the middle of a block is a read-modify-write at the filesystem and device
//!   level, and it is what makes `O_DIRECT` (macOS: `F_NOCACHE`) unavailable.
//! * **A page names its own address.** [`PageHeader::pageaddr`] is the LSN of the
//!   page's first byte, so a page that was never written (a preallocated segment
//!   is zeros) and a page still holding a *recycled* segment's previous life are
//!   both distinguishable from live log without consulting the file's length.
//!   That second case is the one nothing else catches: every record in a
//!   segment renamed forward still passes its own CRC.

use crate::record::Lsn;

/// The WAL page size. Deliberately equal to the data-page `BLCKSZ`, and a
/// multiple of the 4096-byte alignment every device this runs on wants.
pub const XLOG_BLCKSZ: u64 = 8192;

/// Bytes of [`PageHeader`] at the start of every page.
pub const XLP_PAGE_HEADER_SIZE: u64 = 24;

/// Record bytes one page can hold.
pub const XLP_USABLE: u64 = XLOG_BLCKSZ - XLP_PAGE_HEADER_SIZE;

/// Identifies a page as ours, and at which layout version. Bumped whenever the
/// page or record encoding changes.
///
/// Its unique job is *versioning*, not corruption detection — the header CRC and
/// the address check below are both far stronger filters. A data directory
/// written by the pre-paging build holds, at the first byte of the log, a record
/// whose leading `u32` is a plausible `total_len`; without a magic to disagree
/// with, that would be read as a page header, and a log that fails to parse reads
/// as an *empty* log, which is silently discarded.
pub const XLP_MAGIC: u16 = 0xC6A1;

/// The first byte of this page continues a record that began earlier;
/// [`PageHeader::rem_len`] says how many of its bytes land here.
pub const XLP_FIRST_IS_CONTRECORD: u16 = 0x0001;

/// The header at the top of every WAL page.
///
/// On-disk layout (little-endian):
///
/// | Off | Size | Field |
/// |----|----|----|
/// | 0  | 2 | `magic` ([`XLP_MAGIC`]) |
/// | 2  | 2 | `info` (flags; [`XLP_FIRST_IS_CONTRECORD`]) |
/// | 4  | 4 | `rem_len` |
/// | 8  | 8 | `pageaddr` |
/// | 16 | 4 | reserved (`0`) |
/// | 20 | 4 | `crc` (CRC-32C over bytes `[0, 20)`) |
///
/// The header CRC is a deliberate addition over PostgreSQL, whose page header is
/// unchecksummed. `rem_len` steers how many bytes a reader skips before looking
/// for the next record, so a torn `rem_len` on an otherwise plausible page would
/// silently misalign the whole record chain rather than fail. Sixteen bytes of
/// coverage removes that class of failure for the cost of one CRC per page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageHeader {
    pub info: u16,
    /// Bytes at the head of this page belonging to a record that started on an
    /// earlier page. `0` unless [`XLP_FIRST_IS_CONTRECORD`] is set.
    pub rem_len: u32,
    /// The LSN of this page's own first byte.
    pub pageaddr: u64,
}

impl PageHeader {
    /// Bytes the CRC covers, which is also the offset it is stored at.
    const CRC_OFFSET: usize = 20;

    pub fn is_contrecord(&self) -> bool {
        self.info & XLP_FIRST_IS_CONTRECORD != 0
    }

    /// Serialize into the first [`XLP_PAGE_HEADER_SIZE`] bytes of `out`.
    ///
    /// # Panics
    ///
    /// If `out` is shorter than a header. Every caller writes into a buffer it
    /// just sized to a whole page, so a short slice is a programming error rather
    /// than a runtime condition worth a `Result`.
    pub fn encode(&self, out: &mut [u8]) {
        let out = &mut out[..XLP_PAGE_HEADER_SIZE as usize];
        out[0..2].copy_from_slice(&XLP_MAGIC.to_le_bytes());
        out[2..4].copy_from_slice(&self.info.to_le_bytes());
        out[4..8].copy_from_slice(&self.rem_len.to_le_bytes());
        out[8..16].copy_from_slice(&self.pageaddr.to_le_bytes());
        out[16..20].copy_from_slice(&0u32.to_le_bytes());
        let crc = crc32c::crc32c(&out[..Self::CRC_OFFSET]);
        out[Self::CRC_OFFSET..24].copy_from_slice(&crc.to_le_bytes());
    }

    /// Parse a header, or `None` for a wrong magic, a short slice, or a CRC
    /// mismatch — all of which mean "this is not one of our pages".
    ///
    /// Deliberately does **not** check [`PageHeader::pageaddr`]: only the caller
    /// knows which address it expected to find, and that comparison is the whole
    /// point of the field. Doing it here would let a caller that forgot to make
    /// it look like it had.
    pub fn decode(bytes: &[u8]) -> Option<PageHeader> {
        let bytes = bytes.get(..XLP_PAGE_HEADER_SIZE as usize)?;
        if u16::from_le_bytes(bytes[0..2].try_into().ok()?) != XLP_MAGIC {
            return None;
        }
        let stored = u32::from_le_bytes(bytes[Self::CRC_OFFSET..24].try_into().ok()?);
        if crc32c::crc32c(&bytes[..Self::CRC_OFFSET]) != stored {
            return None;
        }
        Some(PageHeader {
            info: u16::from_le_bytes(bytes[2..4].try_into().ok()?),
            rem_len: u32::from_le_bytes(bytes[4..8].try_into().ok()?),
            pageaddr: u64::from_le_bytes(bytes[8..16].try_into().ok()?),
        })
    }
}

/// The LSN of the first byte of the page `lsn` falls in.
pub fn page_start(lsn: u64) -> u64 {
    lsn - lsn % XLOG_BLCKSZ
}

/// How far into its page `lsn` sits.
pub fn page_offset(lsn: u64) -> u64 {
    lsn % XLOG_BLCKSZ
}

/// The first byte of `page` that can hold record data. `page` must be a page
/// start.
pub fn first_usable(page: u64) -> u64 {
    debug_assert_eq!(page_offset(page), 0);
    page + XLP_PAGE_HEADER_SIZE
}

/// Whether `lsn` names a byte a record can occupy — i.e. it is not inside a page
/// header. Every record boundary satisfies this, which is what makes it a usable
/// check on a redo point read back from `pg_control`.
pub fn is_record_position(lsn: u64) -> bool {
    page_offset(lsn) >= XLP_PAGE_HEADER_SIZE
}

/// Advance `lsn` past `n` bytes of record data, stepping over the header of every
/// page the run crosses.
///
/// The "record ends exactly at the page edge" case falls out rather than being
/// special-cased: consuming the last byte of a page leaves `lsn` on the boundary,
/// the header of the next page is stepped over, and the result is
/// [`first_usable`] of that page — precisely where the next record starts. That
/// keeps [`crate::LsnRange`]'s contiguity contract (`range.start ==
/// previous_end`) true across page boundaries, which the buffer pool's
/// write-ahead check depends on.
pub fn advance(mut lsn: u64, mut n: u64) -> u64 {
    debug_assert!(is_record_position(lsn), "advance from inside a page header");
    loop {
        let room = XLOG_BLCKSZ - page_offset(lsn);
        if n < room {
            return lsn + n;
        }
        lsn += room;
        n -= room;
        lsn += XLP_PAGE_HEADER_SIZE;
        if n == 0 {
            return lsn;
        }
    }
}

/// Same as [`advance`], on [`Lsn`]s.
pub fn advance_lsn(lsn: Lsn, n: u64) -> Lsn {
    Lsn(advance(lsn.0, n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() -> anyhow::Result<()> {
        for header in [
            PageHeader {
                info: 0,
                rem_len: 0,
                pageaddr: 0,
            },
            PageHeader {
                info: XLP_FIRST_IS_CONTRECORD,
                rem_len: 37,
                pageaddr: 8192,
            },
            PageHeader {
                info: XLP_FIRST_IS_CONTRECORD,
                rem_len: XLP_USABLE as u32,
                pageaddr: u64::MAX - (u64::MAX % XLOG_BLCKSZ),
            },
        ] {
            let mut page = vec![0u8; XLOG_BLCKSZ as usize];
            header.encode(&mut page);
            let got = PageHeader::decode(&page)
                .ok_or_else(|| anyhow::anyhow!("header did not decode"))?;
            assert_eq!(got, header);
            assert_eq!(got.is_contrecord(), header.info & XLP_FIRST_IS_CONTRECORD != 0);
        }

        Ok(())
    }

    /// A zeroed page is what a preallocated segment is full of, and it must read
    /// as "not a page" rather than as a header claiming address 0.
    #[test]
    fn a_zeroed_page_has_no_header() {
        assert!(PageHeader::decode(&[0u8; XLOG_BLCKSZ as usize]).is_none());
    }

    #[test]
    fn a_flipped_bit_anywhere_in_the_header_fails_the_crc() {
        let header = PageHeader {
            info: XLP_FIRST_IS_CONTRECORD,
            rem_len: 1234,
            pageaddr: 16384,
        };
        let mut page = vec![0u8; XLOG_BLCKSZ as usize];
        header.encode(&mut page);
        for byte in 0..XLP_PAGE_HEADER_SIZE as usize {
            let mut torn = page.clone();
            torn[byte] ^= 0x01;
            assert!(
                PageHeader::decode(&torn).is_none(),
                "flipping byte {byte} of the header went undetected"
            );
        }
    }

    #[test]
    fn a_short_slice_has_no_header() {
        let header = PageHeader {
            info: 0,
            rem_len: 0,
            pageaddr: 8192,
        };
        let mut page = vec![0u8; XLP_PAGE_HEADER_SIZE as usize];
        header.encode(&mut page);
        assert!(PageHeader::decode(&page[..23]).is_none());
    }

    /// Step `n` bytes one at a time, skipping headers, and compare against the
    /// closed form. The two implementations are independent, which is the point:
    /// `append` and the reader each rely on this arithmetic and must agree.
    fn advance_naively(mut lsn: u64, n: u64) -> u64 {
        for _ in 0..n {
            lsn += 1;
            if page_offset(lsn) == 0 {
                lsn += XLP_PAGE_HEADER_SIZE;
            }
        }
        lsn
    }

    #[test]
    fn advance_matches_a_byte_at_a_time_walk() {
        let start = first_usable(0);
        for from in [
            start,
            start + 1,
            XLOG_BLCKSZ - 1,
            first_usable(XLOG_BLCKSZ),
            first_usable(3 * XLOG_BLCKSZ) + 100,
        ] {
            for n in [
                0,
                1,
                XLP_USABLE - 1,
                XLP_USABLE,
                XLP_USABLE + 1,
                3 * XLP_USABLE + 17,
                10 * XLOG_BLCKSZ,
            ] {
                assert_eq!(
                    advance(from, n),
                    advance_naively(from, n),
                    "advance({from}, {n})"
                );
            }
        }
    }

    #[test]
    fn advance_never_lands_inside_a_header() {
        let mut lsn = first_usable(0);
        for n in 0..2_000u64 {
            lsn = advance(lsn, n % 9_000);
            assert!(is_record_position(lsn), "landed inside a header at {lsn}");
        }
    }

    /// The case most easily got wrong: a record that ends flush with the page
    /// edge must leave the cursor at the *next* page's first usable byte, not on
    /// the boundary itself.
    #[test]
    fn a_record_ending_at_the_page_edge_lands_on_the_next_page() {
        let start = first_usable(0);
        assert_eq!(advance(start, XLP_USABLE), first_usable(XLOG_BLCKSZ));
        // And one byte more spills a single byte onto that page.
        assert_eq!(advance(start, XLP_USABLE + 1), first_usable(XLOG_BLCKSZ) + 1);
    }

    #[test]
    fn page_arithmetic_agrees_at_the_boundaries() {
        assert_eq!(page_start(0), 0);
        assert_eq!(page_start(XLOG_BLCKSZ - 1), 0);
        assert_eq!(page_start(XLOG_BLCKSZ), XLOG_BLCKSZ);
        assert_eq!(page_offset(XLOG_BLCKSZ + 5), 5);
        assert_eq!(first_usable(XLOG_BLCKSZ), XLOG_BLCKSZ + XLP_PAGE_HEADER_SIZE);
        assert!(!is_record_position(XLOG_BLCKSZ));
        assert!(!is_record_position(XLOG_BLCKSZ + XLP_PAGE_HEADER_SIZE - 1));
        assert!(is_record_position(XLOG_BLCKSZ + XLP_PAGE_HEADER_SIZE));
    }
}
