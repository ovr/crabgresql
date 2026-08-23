//! The view side of the registry: the `information_schema` row builders, and
//! the definition text of every system view of both schemas.
//!
//! TODO: these are Rust row builders, not views. PostgreSQL defines each one as
//! SQL over `pg_catalog`, which a reviewer can diff against `pg_get_viewdef` on
//! a running server and which cannot drift from the catalogs it reads — neither
//! is true of a hand-written row builder. [`definitions`] is the first half of
//! closing that: the SQL is in the tree and checked against what the row
//! builders publish, but nothing runs it yet.

pub(crate) mod definitions;
pub(crate) mod information_schema;
