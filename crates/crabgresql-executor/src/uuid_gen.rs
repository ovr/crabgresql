//! The impure half of UUID generation: the RNG, the clock, and the guard that
//! keeps version 7 values increasing.
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior — the bit layout RFC 9562 fixes, and the fact that successive
//! `uuidv7()` calls in one session sort in generation order even when they land
//! in the same millisecond. The layout itself lives in
//! [`crabgresql_types::uuid`], which stays pure so it can be unit-tested
//! without a clock.
//!
//! Randomness comes from `rand`'s thread generator, a buffered ChaCha12
//! reseeded from the OS. PG draws UUID bytes from `pg_strong_random`, so a
//! cryptographic generator is the behavior to match, not an upgrade over it.

use std::sync::atomic::{AtomicU64, Ordering};

use rand::Rng;

use crabgresql_types::uuid;

/// The largest representable ordering key: the 48-bit millisecond field full,
/// with `rand_a` full below it. Beyond this a version 7 value has nowhere to
/// put the stamp (it is somewhere in year 10889).
const MAX_V7_KEY: i128 = ((uuid::V7_MAX_UNIX_MS as i128) << 12) | 0x0fff;

/// The monotonic guard's state.
///
/// Factored out of the process-global below so a test can drive one from a
/// frozen clock: the bug this guards against is invisible when the wall clock
/// advances between calls, which on a debug build it always does.
struct Latch(AtomicU64);

impl Latch {
    const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// `max(clock_key, last + 1)` — the only writer of the guard.
    ///
    /// Within one millisecond the clock offers only 4096 distinct keys, so a
    /// burst past that rate exhausts them; incrementing past the last key then
    /// carries into the millisecond field, running the stamp slightly ahead of
    /// real time. PostgreSQL makes the same trade — its stamps were measured
    /// running up to 43ms ahead of the wall clock and never stepping back, and
    /// monotonicity and uniqueness are the observable contract.
    ///
    /// The clock reading is clamped into the representable range here and only
    /// here, so the state is always well-formed; whether the value can actually
    /// be *stamped* is decided afterwards by [`in_v7_range`], once the caller's
    /// shift has been applied.
    fn next(&self, clock_key: i128) -> u64 {
        let clock = clock_key.clamp(0, MAX_V7_KEY) as u64;
        let mut result = clock;
        // `fetch_update` retries on contention; the closure must stay pure.
        let _ = self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
                result = clock.max(last.saturating_add(1));
                Some(result)
            });
        result
    }
}

/// The process-wide guard.
///
/// Process-global rather than per-session: a suspended portal resumes on a
/// different worker thread, so thread-local state could hand a session a
/// *smaller* key than it had already seen. A global counter is strictly
/// stronger than PG's per-backend static — every value this process issues is
/// ordered, not just every value one backend issues — and no client can
/// observe the difference in the failing direction.
static LAST_V7_KEY: Latch = Latch::new();

/// The ordering key of an instant: the 48-bit millisecond stamp with the
/// 12-bit `rand_a` fraction below it.
///
/// Signed, so an instant outside the field stays *representable* and can be
/// rejected rather than silently folded into range.
fn key_of(unix_nanos: i128) -> i128 {
    (unix_nanos.div_euclid(1_000_000) << 12) | uuid::sub_ms_fraction_nanos(unix_nanos) as i128
}

/// The key as a value the layout can hold, or `None` when it names an instant
/// before 1970 or past year 10889 — the two ends `uuidv7(shift)` reports as
/// "timestamp out of range for UUID version 7".
fn in_v7_range(key: i128) -> Option<u64> {
    (0..=MAX_V7_KEY).contains(&key).then_some(key as u64)
}

/// Split an ordering key back into the two fields `build_v7` wants.
fn split_key(key: u64) -> (u64, u16) {
    (key >> 12, (key & 0x0fff) as u16)
}

/// Fill `n` bytes from the thread's generator.
fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

/// `gen_random_uuid()` / `uuidv4()`.
pub fn gen_v4() -> [u8; 16] {
    uuid::build_v4(random())
}

/// A version 7 value from `latch`, stamped `shift_ms` milliseconds from the
/// instant `unix_nanos` names. `None` when that lands outside the 48-bit field.
///
/// The shift is added to the key the latch *returns*, never to the key it is
/// *given*: the guard's state stays a function of the clock alone, so a
/// `uuidv7('1000 years')` cannot drag later plain calls into the future, while
/// every value sharing one shift still inherits the latch's strict increase
/// (adding a constant preserves order). That is the arrangement PostgreSQL's
/// output is consistent with: interleaved plain and shifted calls each come out
/// strictly increasing, and de-shifting merges them into one strictly
/// increasing run with no ties.
fn gen_v7_with(latch: &Latch, unix_nanos: i128, shift_ms: i64) -> Option<[u8; 16]> {
    let key = latch.next(key_of(unix_nanos)) as i128 + ((shift_ms as i128) << 12);
    let (ms, rand_a) = split_key(in_v7_range(key)?);
    Some(uuid::build_v7(ms, rand_a, random()))
}

/// `uuidv7()`: the current instant, with the monotonic guard applied.
pub fn gen_v7(unix_nanos: i128) -> [u8; 16] {
    // Unreachable short of a system clock set past year 10889: the plain form
    // takes no shift, so nothing but the clock can push the key off the end of
    // the field. Saturating there keeps the sequence non-decreasing, where
    // dropping the high bits would throw the value back to 1970.
    gen_v7_with(&LAST_V7_KEY, unix_nanos, 0)
        .unwrap_or_else(|| uuid::build_v7(uuid::V7_MAX_UNIX_MS, 0x0fff, random()))
}

/// `uuidv7(shift)`: as above, moved `shift_ms` whole milliseconds. `None` when
/// the result is out of range, which the caller reports as `22008`.
pub fn gen_v7_shifted(unix_nanos: i128, shift_ms: i64) -> Option<[u8; 16]> {
    gen_v7_with(&LAST_V7_KEY, unix_nanos, shift_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An arbitrary fixed instant: 2024-01-01T00:00:00.000123456Z.
    ///
    /// Every test below drives a fresh [`Latch`] from this one reading rather
    /// than from the wall clock. That is what makes them profile-independent:
    /// with a real clock a debug build advances the key between calls all by
    /// itself, which is exactly how an unguarded generator passed for a while.
    const FROZEN_NS: i128 = 1_704_067_200_000_123_456;

    const ONE_DAY_MS: i64 = 86_400_000;

    /// The 60-bit ordering prefix of a value, which is what sorts it.
    fn prefix(v: &[u8; 16]) -> u64 {
        let ms = u64::from_be_bytes([0, 0, v[0], v[1], v[2], v[3], v[4], v[5]]);
        (ms << 12) | ((v[6] as u64 & 0x0f) << 8) | v[7] as u64
    }

    #[test]
    fn v7_keys_increase_and_carry_past_one_millisecond() {
        // Every key from one frozen instant must still be distinct and
        // increasing, which past 4096 values means carrying into the ms field.
        let latch = Latch::new();
        let first = latch.next(key_of(FROZEN_NS));
        let (start_ms, _) = split_key(first);
        let mut prev = first;
        for _ in 0..8192 {
            let key = latch.next(key_of(FROZEN_NS));
            assert!(key > prev, "keys must strictly increase");
            prev = key;
        }
        let (end_ms, _) = split_key(prev);
        assert!(
            end_ms > start_ms,
            "8192 keys in one millisecond must carry into the stamp"
        );
    }

    #[test]
    fn v7_values_are_ordered_and_distinct() {
        let latch = Latch::new();
        let mut prev = gen_v7_with(&latch, FROZEN_NS, 0).expect("in range");
        let mut seen = std::collections::HashSet::new();
        seen.insert(prev);
        for _ in 0..10_000 {
            let next = gen_v7_with(&latch, FROZEN_NS, 0).expect("in range");
            assert!(next > prev, "v7 values must sort in generation order");
            assert!(seen.insert(next), "v7 values must be distinct");
            assert_eq!(uuid::extract_version(&next), Some(7));
            prev = next;
        }
    }

    /// The regression test for the shifted form: 10 000 values from one frozen
    /// instant must still sort in generation order. Deriving `rand_a` from the
    /// clock alone — as the unguarded shifted path did — leaves only the 62
    /// random bits to separate them, so roughly half the pairs come out
    /// reversed on any build profile.
    #[test]
    fn shifted_values_from_one_instant_are_ordered() {
        let latch = Latch::new();
        let mut prev = gen_v7_with(&latch, FROZEN_NS, ONE_DAY_MS).expect("in range");
        let mut seen = std::collections::HashSet::new();
        seen.insert(prev);
        for _ in 0..10_000 {
            let next = gen_v7_with(&latch, FROZEN_NS, ONE_DAY_MS).expect("in range");
            assert!(next > prev, "shifted v7 values must sort in issue order");
            assert!(seen.insert(next), "shifted v7 values must be distinct");
            prev = next;
        }
    }

    /// Both forms draw from the *same* guard: de-shifting the shifted values
    /// must merge the two runs into one strictly increasing sequence stepping
    /// by exactly 1. Two independent latches would pass the test above and
    /// fail this one.
    #[test]
    fn plain_and_shifted_interleave_without_ties() {
        let latch = Latch::new();
        let shift_in_key_units = (ONE_DAY_MS as u64) << 12;
        let mut prev: Option<u64> = None;
        for i in 0..2000 {
            let shift = if i % 2 == 0 { 0 } else { ONE_DAY_MS };
            let v = gen_v7_with(&latch, FROZEN_NS, shift).expect("in range");
            let mut key = prefix(&v);
            if shift != 0 {
                key -= shift_in_key_units;
            }
            if let Some(prev) = prev {
                assert_eq!(key, prev + 1, "the merged run must step by exactly 1");
            }
            prev = Some(key);
        }
    }

    /// A caller-chosen instant must not drag later plain values with it: the
    /// shift is applied to what the latch returns, never to what it is given.
    #[test]
    fn a_shift_does_not_drag_later_plain_values() {
        let latch = Latch::new();
        let millennium_ms = 1_000 * 365 * ONE_DAY_MS;
        let _ = gen_v7_with(&latch, FROZEN_NS, millennium_ms).expect("in range");
        let after = gen_v7_with(&latch, FROZEN_NS, 0).expect("in range");
        let stamped = uuid::extract_timestamp_unix_micros(&after).expect("a v7 carries an instant");
        // Exactly the frozen millisecond — no tolerance window needed.
        assert_eq!(stamped, FROZEN_NS.div_euclid(1_000_000) as i64 * 1000);
    }

    #[test]
    fn an_out_of_field_shift_is_rejected() {
        let latch = Latch::new();
        let frozen_ms = FROZEN_NS.div_euclid(1_000_000) as i64;
        let to_the_end = uuid::V7_MAX_UNIX_MS as i64 - frozen_ms;
        assert!(
            gen_v7_with(&latch, FROZEN_NS, to_the_end).is_some(),
            "the last representable millisecond is in range"
        );
        assert!(
            gen_v7_with(&latch, FROZEN_NS, to_the_end + 1).is_none(),
            "one millisecond past the field is out of range"
        );
        // The epoch itself is in range; anything before it is not.
        assert!(gen_v7_with(&latch, FROZEN_NS, -frozen_ms).is_some());
        assert!(gen_v7_with(&latch, FROZEN_NS, -frozen_ms - 1).is_none());
    }

    /// The guard's own carry is range-checked too, and the plain form saturates
    /// rather than wrapping when it runs off the end of the field.
    #[test]
    fn the_latch_carry_is_range_checked() {
        let latch = Latch::new();
        let top_ns = uuid::V7_MAX_UNIX_MS as i128 * 1_000_000;
        // Park the latch at the very top of the field, then keep drawing.
        let mut issued = 0;
        while gen_v7_with(&latch, top_ns, 0).is_some() {
            issued += 1;
            assert!(issued <= 4096, "the field cannot hold more than 4096 more");
        }
        // The public plain form saturates instead of failing or wrapping.
        let saturated = gen_v7(top_ns);
        assert_eq!(
            uuid::extract_timestamp_unix_micros(&saturated),
            Some(uuid::V7_MAX_UNIX_MS as i64 * 1000)
        );
    }

    /// The key reads nanoseconds: one `rand_a` step is ~244ns, which a
    /// microsecond clock cannot resolve.
    #[test]
    fn the_key_reads_nanoseconds() {
        let step = key_of(FROZEN_NS + 244) - key_of(FROZEN_NS);
        assert_eq!(step, 1, "244ns is exactly one rand_a step");
        assert_eq!(key_of(FROZEN_NS + 1), key_of(FROZEN_NS), "1ns is below it");
    }

    /// Concurrent draws share one guard, so uniqueness must survive contention.
    #[test]
    fn concurrent_generation_is_distinct() {
        let latch = Latch::new();
        let values = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    s.spawn(|| {
                        (0..2000)
                            .map(|_| gen_v7_with(&latch, FROZEN_NS, 0).expect("in range"))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().expect("thread panicked"))
                .collect::<std::collections::HashSet<_>>()
        });
        assert_eq!(values.len(), 8000, "no two threads may share a value");
    }

    #[test]
    fn v4_is_marked_and_distinct() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let v = gen_v4();
            assert_eq!(uuid::extract_version(&v), Some(4));
            assert_eq!(v[8] & 0xc0, 0x80, "RFC 9562 variant");
            assert!(seen.insert(v), "1000 draws must not repeat");
        }
    }
}
