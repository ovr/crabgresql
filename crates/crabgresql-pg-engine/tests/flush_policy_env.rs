//! That the flush policy actually reads the environment it documents.
//!
//! Its own test binary on purpose: setting a variable is `unsafe` under the
//! 2024 edition because it races any other thread reading the environment, and
//! a binary with a single test has no other thread to race. The parsing itself
//! is covered by `crabgresql-config`'s unit tests; what is checked here is the
//! wiring — that each field is fed by the variable it claims.

use std::time::Duration;

use crabgresql_pg_engine::BufferFlushPolicy;

#[test]
fn sizes_and_intervals_come_from_the_documented_variables() {
    let defaults = BufferFlushPolicy::default();

    // SAFETY: this binary holds exactly one test, so no other thread is
    // reading the environment while these are set.
    unsafe {
        std::env::set_var(crabgresql_config::BUFFER_TABLE_SOFT_BYTES, "64MB");
        std::env::set_var(crabgresql_config::BUFFER_GLOBAL_HARD_BYTES, "1gb");
        std::env::set_var(crabgresql_config::BUFFER_MAX_AGE_MS, "250");
        std::env::set_var(crabgresql_config::BUFFER_TICK_MS, "50");
    }

    let policy = BufferFlushPolicy::from_env();
    assert_eq!(policy.table_soft_bytes, 64 * 1024 * 1024);
    assert_eq!(policy.global_hard_bytes, 1024 * 1024 * 1024);
    assert_eq!(policy.max_age, Duration::from_millis(250));
    assert_eq!(policy.tick, Duration::from_millis(50));

    // A size we cannot parse leaves the default in place rather than the
    // server refusing to start.
    unsafe {
        std::env::set_var(crabgresql_config::BUFFER_TABLE_SOFT_BYTES, "32 quatloos");
    }
    assert_eq!(
        BufferFlushPolicy::from_env().table_soft_bytes,
        defaults.table_soft_bytes
    );
}
