//! Tests for the plan binder ([`crate::plan`]), split by the SQL surface
//! each one covers. Shared fixtures live in [`common`].

mod aggregates;
mod case_bool;
mod common;
mod copy;
mod distinct_order_limit;
mod dml;
mod expressions;
mod inheritance;
mod insert_select;
mod joins;
mod literals;
mod outer_refs;
mod params;
mod setops;
mod subqueries;
mod table_functions;
mod tableoid;
mod values_cte;
mod window;
