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

/// The 60 bits of a version 7 UUID that order it: the 48-bit millisecond stamp
/// with the 12-bit `rand_a` fraction below it. Held as one integer so the
/// monotonic guard is a single compare-and-swap.
///
/// Zero means "nothing issued yet", which no real clock reading can collide
/// with (it is 1970-01-01T00:00:00.000Z).
static LAST_V7_KEY: AtomicU64 = AtomicU64::new(0);

fn key_of(unix_micros: i64) -> u64 {
    let ms = unix_micros.div_euclid(1000).max(0) as u64;
    (ms << 12) | uuid::sub_ms_fraction(unix_micros) as u64
}

/// Split an ordering key back into the two fields `build_v7` wants.
fn split_key(key: u64) -> (u64, u16) {
    (key >> 12, (key & 0x0fff) as u16)
}

/// The next version 7 ordering key at `unix_micros`, never repeating and never
/// going backwards.
///
/// Within one millisecond the clock offers only 4096 distinct keys, so a burst
/// past that rate exhausts them; incrementing past the last key then carries
/// into the millisecond field, running the stamp slightly ahead of real time.
/// That is the trade PostgreSQL makes too — monotonicity and uniqueness are the
/// observable contract, and the timestamp is documented as approximate.
///
/// The state is process-global rather than per-session: a suspended portal
/// resumes on a different worker thread, so thread-local state could hand a
/// session a *smaller* key than it had already seen. A global counter is
/// strictly stronger than PG's per-backend static — every value this process
/// issues is ordered, not just every value one backend issues — and no client
/// can observe the difference in the failing direction.
fn next_v7_key(unix_micros: i64) -> u64 {
    let clock = key_of(unix_micros);
    let mut result = clock;
    // `fetch_update` retries on contention; the closure must stay pure.
    let _ = LAST_V7_KEY.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
        result = clock.max(last.saturating_add(1));
        Some(result)
    });
    result
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

/// `uuidv7()`: the current instant, with the monotonic guard applied.
pub fn gen_v7(unix_micros: i64) -> [u8; 16] {
    let (ms, rand_a) = split_key(next_v7_key(unix_micros));
    uuid::build_v7(ms, rand_a, random())
}

/// `uuidv7(shift)`: an explicitly chosen instant, *without* the guard.
///
/// The guard exists to order the values one session issues from the clock;
/// feeding a caller-chosen instant into it would let a single
/// `uuidv7('1000 years')` push every later `uuidv7()` a millennium into the
/// future. Observably, PG keeps the two apart the same way: a plain `uuidv7()`
/// after a shifted one still stamps the current millisecond.
pub fn gen_v7_at(unix_micros: i64) -> [u8; 16] {
    let (ms, rand_a) = split_key(key_of(unix_micros));
    uuid::build_v7(ms, rand_a, random())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An arbitrary fixed instant: 2024-01-01T00:00:00.000123Z.
    const FROZEN: i64 = 1_704_067_200_000_123;

    #[test]
    fn v7_keys_increase_and_carry_past_one_millisecond() {
        // Every key from one frozen instant must still be distinct and
        // increasing, which past 4096 values means carrying into the ms field.
        let first = next_v7_key(FROZEN);
        let (start_ms, _) = split_key(first);
        let mut prev = first;
        for _ in 0..8192 {
            let key = next_v7_key(FROZEN);
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
        let mut prev = gen_v7(crabgresql_types::tz::to_unix_micros(
            crabgresql_types::tz::now_micros(),
        ));
        let mut seen = std::collections::HashSet::new();
        seen.insert(prev);
        for _ in 0..10_000 {
            let now = crabgresql_types::tz::to_unix_micros(crabgresql_types::tz::now_micros());
            let next = gen_v7(now);
            assert!(next > prev, "v7 values must sort in generation order");
            assert!(seen.insert(next), "v7 values must be distinct");
            assert_eq!(uuid::extract_version(&next), Some(7));
            prev = next;
        }
    }

    #[test]
    fn a_shifted_value_does_not_move_the_guard() {
        let far_future = FROZEN + 1_000 * 365 * 86_400 * 1_000_000;
        let _ = gen_v7_at(far_future);
        let now = crabgresql_types::tz::to_unix_micros(crabgresql_types::tz::now_micros());
        let after = gen_v7(now);
        let stamped = uuid::extract_timestamp_unix_micros(&after).expect("a v7 carries an instant");
        assert!(
            (stamped - now).abs() < 60 * 1_000_000,
            "a plain uuidv7 must still stamp the current instant"
        );
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
