//! CrabgreSQL as a WASI 0.2 component: the whole engine — parser, planner,
//! executor, heap, WAL, recovery — in one wasm module, talking to the host only
//! through `wasi:filesystem`.
//!
//! The component exports [`crabgresql:db/database`](../wit/world.wit): open a
//! data directory, run SQL, get JSON back. In the browser that host filesystem
//! is jco's in-memory shim, so a database lives as long as the instance does.
//!
//! Everything except the WIT glue is plain Rust that builds natively too, which
//! is what lets [`session`]'s tests run on the host instead of inside a
//! browser.

mod json;
mod session;

pub use json::{ExecOutput, StatementResult, error_to_json};
pub use session::{Database, EmbedError};

#[cfg(target_family = "wasm")]
mod bindings;
