//! What an allocation actually costs, as opposed to what was asked for.
//!
//! A caller that wants to know how much memory it is holding cannot just add up
//! the lengths it requested: an allocator rounds every request up, and it
//! refuses to hand out anything below a floor. Ignoring both is a one-sided
//! error — it can only ever under-report — and a memory budget built on a
//! systematic under-report admits several times the rows its operator asked
//! for. So the rounding is modelled here, once, rather than approximated with a
//! fudge factor at each call site.
//!
//! The model is a 16-byte granule and a 32-byte floor. That is a floor, not a
//! guess, on the allocators we can actually run under:
//!
//! - **glibc malloc** rounds a request to `max(32, align16(n + 8))` — an 8-byte
//!   size field ahead of the block, 16-byte alignment, and a 32-byte minimum
//!   chunk. [`alloc_bytes`] is exactly that without the header, so it
//!   under-reports by at most 8 bytes per allocation there.
//! - **macOS libmalloc** quantizes its tiny region to 16 bytes and keeps
//!   metadata out of band, so a request of `n` occupies `align16(n)`.
//!   [`alloc_bytes`] is exact above the floor and over-reports the very
//!   smallest allocations by one granule.
//! - **jemalloc and mimalloc** space their small size classes 16 bytes apart up
//!   to 128 bytes and more coarsely above, with waste bounded by the class
//!   width. [`alloc_bytes`] is exact in the small range and under-reports by at
//!   most a class width above it.
//!
//! Nothing beyond the granule and the floor is modelled. Allocators also carry
//! per-region metadata and fragment over time, but no defensible constant
//! describes that, and an undefensible one is worse than leaving it out: it
//! would move every number without anybody being able to say by how much it
//! should have.

/// Alignment every allocation is rounded up to. Sixteen bytes on every 64-bit
/// target we build for, because that is the alignment `max_align_t` requires.
pub const ALLOC_GRANULE: usize = 16;

/// Smallest block an allocator will hand out at all. A one-byte `String` and a
/// sixteen-byte one cost the same.
pub const MIN_ALLOC_BYTES: usize = 32;

/// What a heap allocation of `request` bytes occupies.
///
/// Zero is the case worth stating: a `Vec` or `String` that has never grown
/// holds a dangling pointer and never calls the allocator, so it must not be
/// charged the floor. Without that, a wide row of NULLs or empty strings would
/// be billed 32 bytes a column for allocations that do not exist.
pub const fn alloc_bytes(request: usize) -> usize {
    if request == 0 {
        return 0;
    }
    let rounded = request.next_multiple_of(ALLOC_GRANULE);
    if rounded < MIN_ALLOC_BYTES {
        MIN_ALLOC_BYTES
    } else {
        rounded
    }
}

/// What the buffer behind a `Vec<T>`/`Box<[T]>` of `capacity` elements
/// occupies. Capacity rather than length, because capacity is what the
/// allocator was asked for; length is a property of the data, not of the
/// allocation.
pub fn slice_bytes<T>(capacity: usize) -> usize {
    alloc_bytes(capacity * size_of::<T>())
}

/// What a `String`'s buffer occupies. Named separately from [`slice_bytes`]
/// only so call sites read as prose; a `String` is a `Vec<u8>` underneath.
pub fn string_bytes(text: &str) -> usize {
    alloc_bytes(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_container_is_not_charged_for_an_allocation() {
        assert_eq!(alloc_bytes(0), 0);
        assert_eq!(slice_bytes::<u64>(0), 0);
        assert_eq!(string_bytes(""), 0);
    }

    /// The property the whole model rests on: the charge is never *below* what
    /// was asked for, and it always lands on a granule. A rounding that could
    /// come out low would reintroduce the under-report this module exists to
    /// remove.
    #[test]
    fn a_request_is_charged_at_least_what_it_asked_for() {
        for request in 1..=4096usize {
            let charged = alloc_bytes(request);
            assert!(
                charged >= request,
                "{request} charged {charged}, which is less than it asked for"
            );
            assert_eq!(charged % ALLOC_GRANULE, 0, "{request} charged {charged}");
            assert!(charged >= MIN_ALLOC_BYTES);
        }
    }

    #[test]
    fn a_request_rounds_up_to_the_granule_above_the_floor() {
        assert_eq!(alloc_bytes(1), 32, "the floor, not one granule");
        assert_eq!(alloc_bytes(32), 32, "the floor exactly");
        assert_eq!(alloc_bytes(33), 48);
        assert_eq!(alloc_bytes(48), 48, "already on a granule");
        assert_eq!(alloc_bytes(49), 64);
    }

    #[test]
    fn a_slice_is_charged_for_its_elements() {
        // Eight `u64`s are 64 bytes, which is already a whole number of
        // granules; seven are 56 and round up to the same block.
        assert_eq!(slice_bytes::<u64>(8), 64);
        assert_eq!(slice_bytes::<u64>(7), 64);
        assert_eq!(slice_bytes::<u8>(1), MIN_ALLOC_BYTES);
    }

    #[test]
    fn a_string_is_charged_for_its_bytes_not_its_characters() {
        // `len` is bytes in Rust, which is also what was allocated; a
        // multi-byte character must not be charged as one.
        assert_eq!(string_bytes("hello"), 32);
        assert_eq!(string_bytes(&"x".repeat(100)), 112);
        assert_eq!(string_bytes("é"), 32, "two bytes, one character");
    }
}
