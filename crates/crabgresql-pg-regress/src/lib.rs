//! crabgresql-pg-regress: a pg_regress-style regression runner.
//!
//! Runs the PostgreSQL regression corpus (vendored under `vendor/postgres/`)
//! against an in-process CrabgreSQL server and diffs the output against the
//! upstream `expected/*.out` files, emulating what `psql -X -a -q` would
//! print. See the `regress` binary for the CLI and `tests/must_pass.rs` for
//! the curated suites that gate `cargo test`.

pub mod client;
pub mod format;
pub mod psql_var;
pub mod runner;
pub mod schedule;
pub mod script;
