//! That the buffer pool actually reads the variable it documents.
//!
//! Its own test binary on purpose, for the same reason `flush_policy_env` is:
//! setting a variable is `unsafe` under the 2024 edition because it races any
//! other thread reading the environment, and a binary with a single test has no
//! other thread to race. The parsing itself is covered by `crabgresql-config`'s
//! unit tests; what is checked here is the wiring — that the knob reaches the
//! pool, and that the bytes-to-frames conversion happens where it is documented
//! to happen.

use crabgresql_pg_engine::BufferPoolPolicy;

const BLCKSZ: usize = 8192;

#[test]
fn the_pool_is_sized_by_the_documented_variable() {
    assert_eq!(
        BufferPoolPolicy::default().frames,
        128 * 1024 * 1024 / BLCKSZ,
        "the default is PostgreSQL's 128MB, in whole frames"
    );

    // SAFETY: this binary holds exactly one test, so no other thread is reading
    // the environment while these are set.
    unsafe {
        std::env::set_var(crabgresql_config::SHARED_BUFFERS.name, "64MB");
    }
    assert_eq!(
        BufferPoolPolicy::from_env().frames,
        64 * 1024 * 1024 / BLCKSZ,
        "the variable reaches the pool"
    );

    // Not a multiple of the page size: the remainder is dropped rather than
    // rounded up, so a pool never commits more than it was asked for. In range,
    // so nothing complains about it — which is why the README says so out loud.
    unsafe {
        std::env::set_var(crabgresql_config::SHARED_BUFFERS.name, "12289kB");
    }
    assert_eq!(
        BufferPoolPolicy::from_env().frames,
        12289 * 1024 / BLCKSZ,
        "a size that is not a whole number of frames rounds down"
    );

    // Below the floor: clamped up, not rejected, and never to zero frames —
    // the clock sweep divides by the frame count.
    unsafe {
        std::env::set_var(crabgresql_config::SHARED_BUFFERS.name, "1kB");
    }
    assert_eq!(
        BufferPoolPolicy::from_env().frames,
        crabgresql_config::SHARED_BUFFERS.min / BLCKSZ,
        "an undersized request is raised to the documented minimum"
    );

    // Unreadable falls back to the default rather than failing startup.
    unsafe {
        std::env::set_var(crabgresql_config::SHARED_BUFFERS.name, "several");
    }
    assert_eq!(
        BufferPoolPolicy::from_env().frames,
        BufferPoolPolicy::default().frames
    );
}
