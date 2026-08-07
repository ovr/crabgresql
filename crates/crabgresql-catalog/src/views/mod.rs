//! The `information_schema` side of the registry.
//!
//! TODO: these are Rust row builders, not views. PostgreSQL defines each one as
//! SQL over `pg_catalog`, which a reviewer can diff against `pg_get_viewdef` on
//! a running server and which cannot drift from the catalogs it reads — neither
//! is true of a hand-written row builder.

pub(crate) mod information_schema;
