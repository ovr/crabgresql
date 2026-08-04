//! A page-granular, memory-aligned staging buffer.
//!
//! The WAL writer stages records here and hands whole pages of it to a
//! positioned write. Two invariants make that safe, and both are maintained by
//! this type rather than by its caller:
//!
//! * the data pointer is [`ALIGN`]-aligned and the capacity is a whole number of
//!   [`XLOG_BLCKSZ`] pages;
//! * **every byte past `len` is zero.** [`AlignedBuf::whole_pages`] rounds up to
//!   a page boundary, so those bytes become the on-disk tail of a partially
//!   filled page. They are what a reader walking forward stops at, which is the
//!   whole reason the log no longer needs its file's length to know where it
//!   ends.

// Lands ahead of its consumer: the writer that stages into this is rewritten a
// few commits later, and splitting the `unsafe` out to be reviewed and tested on
// its own is worth carrying the attribute until then.
#![allow(dead_code)]

use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::ptr::NonNull;

use crate::page::XLOG_BLCKSZ;

/// Alignment every WAL buffer is allocated at.
///
/// Not for speed: it is the precondition an `O_DIRECT` write has on Linux — the
/// buffer address, the file offset and the length must all be aligned to the
/// device's logical block size — and the same for `F_NOCACHE` on macOS. Direct
/// I/O is *not* enabled here; the point is that turning it on later is a flag,
/// not a rewrite of the write path. 4096 covers every device this runs on: 512e
/// drives report 512, 4Kn drives report 4096.
pub const ALIGN: usize = 4096;

/// A heap buffer that is [`ALIGN`]-aligned, sized in whole [`XLOG_BLCKSZ`] pages,
/// and zero-filled past its fill mark.
pub struct AlignedBuf {
    ptr: NonNull<u8>,
    /// Always a non-zero multiple of [`XLOG_BLCKSZ`].
    cap: usize,
    len: usize,
}

// Just bytes: no interior pointers and no shared state, so the raw pointer is
// the only reason the auto-derivation does not apply.
unsafe impl Send for AlignedBuf {}
unsafe impl Sync for AlignedBuf {}

fn layout_for(cap: usize) -> Layout {
    // `cap` is a multiple of XLOG_BLCKSZ, which is a multiple of ALIGN, so this
    // can only fail on an absurd capacity — and then aborting is right.
    match Layout::from_size_align(cap, ALIGN) {
        Ok(layout) => layout,
        Err(_) => panic!("WAL buffer capacity {cap} is not a valid allocation"),
    }
}

fn alloc_pages(pages: usize) -> (NonNull<u8>, usize) {
    let cap = pages.max(1) * XLOG_BLCKSZ as usize;
    let layout = layout_for(cap);
    // SAFETY: `layout` has a non-zero size.
    let ptr = unsafe { alloc_zeroed(layout) };
    match NonNull::new(ptr) {
        Some(ptr) => (ptr, cap),
        None => handle_alloc_error(layout),
    }
}

impl AlignedBuf {
    /// A buffer with room for `pages` pages and nothing in it. `pages == 0` is
    /// rounded up to one: a zero-sized allocation is not a valid `Layout`, and a
    /// caller asking for nothing still wants a usable buffer.
    pub fn with_pages(pages: usize) -> AlignedBuf {
        let (ptr, cap) = alloc_pages(pages);
        AlignedBuf { ptr, cap, len: 0 }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `[0, len)` is initialized and within the allocation.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// The filled region, mutably. Cannot reach past `len`, so the zero-fill
    /// invariant is out of a caller's reach.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` rules out aliasing.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    /// The whole pages covering `[0, len)`, zero-padded past `len`.
    ///
    /// The only thing ever handed to a write. The padding is not slack: it is
    /// what erases whatever the tail of that page held before, which is what
    /// stops a reader from walking off the end of a rewound log into records
    /// that were supposed to be discarded.
    pub fn whole_pages(&self) -> &[u8] {
        let pages = self.len.div_ceil(XLOG_BLCKSZ as usize) * XLOG_BLCKSZ as usize;
        debug_assert!(pages <= self.cap);
        // SAFETY: `[0, cap)` is initialized — the allocation is zeroed and every
        // path below restores zeros past `len` — and `pages <= cap`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), pages) }
    }

    /// Grow so that `additional` more bytes fit, preserving the contents and the
    /// zero fill.
    ///
    /// A fresh zeroed allocation plus a copy rather than a `realloc`: the grown
    /// tail has to be zero, and `realloc` would hand back uninitialized bytes
    /// that a separate memset would then have to chase.
    fn reserve(&mut self, additional: usize) {
        let needed = self.len + additional;
        if needed <= self.cap {
            return;
        }
        let pages = needed.div_ceil(XLOG_BLCKSZ as usize);
        let grown = (self.cap / XLOG_BLCKSZ as usize) * 2;
        let (ptr, cap) = alloc_pages(pages.max(grown));
        // SAFETY: both allocations are at least `len` long and do not overlap.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr.as_ptr(), ptr.as_ptr(), self.len) };
        // SAFETY: `self.ptr` came from `alloc_zeroed` with exactly this layout.
        unsafe { dealloc(self.ptr.as_ptr(), layout_for(self.cap)) };
        self.ptr = ptr;
        self.cap = cap;
    }

    pub fn extend_from_slice(&mut self, bytes: &[u8]) {
        self.reserve(bytes.len());
        // SAFETY: `reserve` guarantees `len + bytes.len() <= cap`, and the source
        // cannot overlap an allocation this type owns exclusively.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.as_ptr().add(self.len), bytes.len());
        }
        self.len += bytes.len();
    }

    /// Drop the first `n` bytes, shifting the remainder down to offset 0.
    ///
    /// `n` must be a multiple of [`XLOG_BLCKSZ`], because the caller's `buf_base`
    /// has to stay page-aligned — a buffer whose first byte was mid-page could
    /// not be written back as whole pages.
    ///
    /// # Panics
    ///
    /// If `n` is not a page multiple or is past the fill mark.
    pub fn drain_front(&mut self, n: usize) {
        assert_eq!(n % XLOG_BLCKSZ as usize, 0, "drain_front must be page-granular");
        assert!(n <= self.len, "drain_front past the fill mark");
        let remaining = self.len - n;
        // SAFETY: source and destination are both inside `[0, len)`.
        unsafe { std::ptr::copy(self.ptr.as_ptr().add(n), self.ptr.as_ptr(), remaining) };
        // Restore the zero fill over the region the shift left holding a stale
        // copy of the tail. Without this, `whole_pages` would emit those bytes
        // into the padding of a later page.
        // SAFETY: `[remaining, len)` is inside the allocation.
        unsafe { std::ptr::write_bytes(self.ptr.as_ptr().add(remaining), 0, n) };
        self.len = remaining;
    }

    pub fn clear(&mut self) {
        // SAFETY: `[0, len)` is inside the allocation.
        unsafe { std::ptr::write_bytes(self.ptr.as_ptr(), 0, self.len) };
        self.len = 0;
    }

    /// Give back capacity above `pages`, so one oversized record does not pin its
    /// buffer for the life of the process. A no-op when the contents would not
    /// fit.
    pub fn shrink_to_pages(&mut self, pages: usize) {
        let target = pages.max(1) * XLOG_BLCKSZ as usize;
        if target >= self.cap || self.len > target {
            return;
        }
        let (ptr, cap) = alloc_pages(pages.max(1));
        // SAFETY: `len <= target == cap`, and the two allocations do not overlap.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr.as_ptr(), ptr.as_ptr(), self.len) };
        // SAFETY: `self.ptr` came from `alloc_zeroed` with exactly this layout.
        unsafe { dealloc(self.ptr.as_ptr(), layout_for(self.cap)) };
        self.ptr = ptr;
        self.cap = cap;
    }

    /// Whether the data pointer meets the direct-I/O alignment requirement. Only
    /// interesting to the tests that assert it, but that assertion is the reason
    /// the type exists.
    pub fn is_aligned(&self) -> bool {
        self.ptr.as_ptr() as usize % ALIGN == 0
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from `alloc_zeroed` with exactly this layout and is
        // never handed out.
        unsafe { dealloc(self.ptr.as_ptr(), layout_for(self.cap)) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: usize = XLOG_BLCKSZ as usize;

    /// The invariant every other test leans on: past the fill mark, zeros.
    fn assert_zero_padded(buf: &AlignedBuf) {
        let pages = buf.whole_pages();
        assert!(
            pages[buf.len()..].iter().all(|&b| b == 0),
            "bytes past len({}) are not zero",
            buf.len()
        );
    }

    #[test]
    fn stays_aligned_across_a_growth_sequence() {
        let mut buf = AlignedBuf::with_pages(1);
        assert!(buf.is_aligned());
        for i in 0..40 {
            buf.extend_from_slice(&vec![i as u8; 977]);
            assert!(buf.is_aligned(), "lost alignment after {i} appends");
            assert_zero_padded(&buf);
        }
        // The contents survived every reallocation.
        for i in 0..40usize {
            assert_eq!(&buf.as_slice()[i * 977..(i + 1) * 977], &vec![i as u8; 977][..]);
        }
    }

    #[test]
    fn whole_pages_rounds_up_and_pads() {
        let mut buf = AlignedBuf::with_pages(1);
        assert_eq!(buf.whole_pages().len(), 0);
        buf.extend_from_slice(&[7u8; 10]);
        assert_eq!(buf.whole_pages().len(), PAGE);
        assert_eq!(&buf.whole_pages()[..10], &[7u8; 10]);
        assert_zero_padded(&buf);
        buf.extend_from_slice(&[9u8; PAGE]);
        assert_eq!(buf.whole_pages().len(), 2 * PAGE);
        assert_zero_padded(&buf);
    }

    /// A record exactly filling the buffer must not round up to a page of pure
    /// padding: writing that page would zero bytes the next flush is about to
    /// fill.
    #[test]
    fn an_exactly_full_buffer_does_not_round_up() {
        let mut buf = AlignedBuf::with_pages(2);
        buf.extend_from_slice(&[1u8; 2 * PAGE]);
        assert_eq!(buf.whole_pages().len(), 2 * PAGE);
    }

    #[test]
    fn drain_front_keeps_the_remainder_and_re_zeroes() {
        let mut buf = AlignedBuf::with_pages(4);
        buf.extend_from_slice(&[1u8; PAGE]);
        buf.extend_from_slice(&[2u8; PAGE]);
        buf.extend_from_slice(&[3u8; 100]);
        buf.drain_front(2 * PAGE);
        assert_eq!(buf.len(), 100);
        assert_eq!(buf.as_slice(), &[3u8; 100]);
        assert_zero_padded(&buf);
        assert!(buf.is_aligned());
    }

    #[test]
    fn drain_front_of_everything_empties_the_buffer() {
        let mut buf = AlignedBuf::with_pages(1);
        buf.extend_from_slice(&[5u8; PAGE]);
        buf.drain_front(PAGE);
        assert!(buf.is_empty());
        assert_eq!(buf.whole_pages().len(), 0);
    }

    #[test]
    #[should_panic(expected = "page-granular")]
    fn drain_front_refuses_a_partial_page() {
        let mut buf = AlignedBuf::with_pages(1);
        buf.extend_from_slice(&[1u8; 100]);
        buf.drain_front(10);
    }

    #[test]
    #[should_panic(expected = "past the fill mark")]
    fn drain_front_refuses_to_run_past_the_contents() {
        let mut buf = AlignedBuf::with_pages(4);
        buf.extend_from_slice(&[1u8; 100]);
        buf.drain_front(PAGE);
    }

    #[test]
    fn clear_restores_the_zero_fill() {
        let mut buf = AlignedBuf::with_pages(1);
        buf.extend_from_slice(&[0xABu8; 300]);
        buf.clear();
        assert!(buf.is_empty());
        buf.extend_from_slice(&[1u8; 4]);
        assert_zero_padded(&buf);
        assert!(buf.whole_pages()[4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn shrink_gives_capacity_back_and_keeps_the_contents() {
        let mut buf = AlignedBuf::with_pages(1);
        buf.extend_from_slice(&[6u8; 20 * PAGE]);
        buf.drain_front(20 * PAGE);
        buf.extend_from_slice(&[7u8; 50]);
        buf.shrink_to_pages(1);
        assert_eq!(buf.as_slice(), &[7u8; 50]);
        assert_eq!(buf.whole_pages().len(), PAGE);
        assert!(buf.is_aligned());
        assert_zero_padded(&buf);
    }

    /// Shrinking below what is held must do nothing rather than truncate.
    #[test]
    fn shrink_declines_when_the_contents_would_not_fit() {
        let mut buf = AlignedBuf::with_pages(4);
        buf.extend_from_slice(&[8u8; 3 * PAGE]);
        buf.shrink_to_pages(1);
        assert_eq!(buf.len(), 3 * PAGE);
        assert_eq!(buf.as_slice(), &[8u8; 3 * PAGE]);
    }

    #[test]
    fn a_zero_page_request_still_yields_a_usable_buffer() {
        let mut buf = AlignedBuf::with_pages(0);
        assert!(buf.is_aligned());
        buf.extend_from_slice(&[1u8; 10]);
        assert_eq!(buf.len(), 10);
    }
}
