//! crabgresql-bench: a harness for running published analytical benchmarks
//! against CrabgreSQL (or, for comparison, any PostgreSQL-compatible server).
//!
//! A benchmark is a [`suite::Suite`]: a schema, a dataset to load, and a list
//! of queries. [`runner::run`] starts the `crabgresql` binary as a child
//! process (or connects to an external server), loads the dataset once, times every query a few times, and
//! returns a [`report::SuiteRun`] the `bench` binary prints as a table or as
//! JSON.
//!
//! The suites themselves live in `suites/<name>/` as plain `.sql` files, kept
//! byte-identical to their upstream sources so results stay comparable; see
//! `NOTICE` for their provenance.

pub mod client;
pub mod report;
pub mod runner;
pub mod suite;
pub mod suites;
