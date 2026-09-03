//! The view side of the registry: the shapes of the `information_schema` views,
//! and the definition text of every system view of both schemas.
//!
//! Every `information_schema` view is served by **running** its definition —
//! its registry entry carries no row builder, and the binder expands the SQL
//! like any other view — so its answer cannot drift from the catalogs it reads,
//! and a reviewer can diff the text against `pg_get_viewdef` on a running
//! server.
//!
//! TODO: the `pg_catalog` views are still Rust row builders. PostgreSQL defines
//! each of those as SQL over `pg_catalog` too, and switching one over is a
//! `view_sql` entry in the registry plus whatever its definition still needs.
//! What blocks most of them is that `pg_class` does not reflect the system
//! relations, so a view over it would answer only about user objects.

pub(crate) mod definitions;
pub(crate) mod information_schema;
