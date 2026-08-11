//! One module per `pg_catalog` relation family. Each publishes the pair the
//! registry binds — a `*_schema()` and a `*_rows(&SystemCatalog)` — so adding a
//! relation is a module here plus one line in [`crate::registry`].

pub(crate) mod am;
pub(crate) mod attribute;
pub(crate) mod auth;
pub(crate) mod class;
pub(crate) mod collation;
pub(crate) mod constraint;
pub(crate) mod cursors;
pub(crate) mod database;
pub(crate) mod index;
pub(crate) mod inherits;
pub(crate) mod language;
pub(crate) mod namespace;
pub(crate) mod prepared;
pub(crate) mod proc;
pub(crate) mod sequence;
pub(crate) mod settings;
pub(crate) mod statistic;
pub(crate) mod timezone;
pub(crate) mod types;
