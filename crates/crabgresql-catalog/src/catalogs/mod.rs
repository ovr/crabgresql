//! One module per `pg_catalog` relation family. Each publishes the pair the
//! registry binds — a `*_schema()` and a `*_rows(&SystemCatalog)` — so adding a
//! relation is a module here plus one line in [`crate::registry`].

pub(crate) mod acl;
pub(crate) mod aggregate;
pub(crate) mod am;
pub(crate) mod amop;
pub(crate) mod attribute;
pub(crate) mod auth;
pub(crate) mod class;
pub(crate) mod collation;
pub(crate) mod constraint;
pub(crate) mod cursors;
pub(crate) mod database;
pub(crate) mod description;
pub(crate) mod extension;
pub(crate) mod foreign;
pub(crate) mod index;
pub(crate) mod inherits;
pub(crate) mod language;
pub(crate) mod locks;
pub(crate) mod misc_empty;
pub(crate) mod namespace;
pub(crate) mod opclass;
pub(crate) mod operator;
pub(crate) mod policy;
pub(crate) mod prepared;
pub(crate) mod proc;
pub(crate) mod progress;
pub(crate) mod publication;
pub(crate) mod relviews;
pub(crate) mod replication;
pub(crate) mod rewrite;
pub(crate) mod sequence;
pub(crate) mod settings;
pub(crate) mod stat_activity;
pub(crate) mod stat_database;
pub(crate) mod stat_indexes;
pub(crate) mod stat_io;
pub(crate) mod stat_tables;
pub(crate) mod statio;
pub(crate) mod statistic;
pub(crate) mod statistic_ext;
pub(crate) mod textsearch;
pub(crate) mod timezone;
pub(crate) mod trigger;
pub(crate) mod types;
