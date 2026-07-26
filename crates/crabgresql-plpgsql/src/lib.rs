//! PL/pgSQL: a parser and interpreter for PostgreSQL's procedural language.
//!
//! The SQL parser this project vendors has no PL/pgSQL support and no reason to
//! grow any: PostgreSQL stores a routine body as an opaque string and hands it
//! to the language's own handler. This crate is that handler.
//!
//! # Shape
//!
//! PL/pgSQL is a thin imperative shell around SQL. Everything that is not
//! control flow is SQL text, which this crate never interprets — it lifts each
//! construct out of the body ([`ast::SqlFragment`]), rewrites references to the
//! routine's variables as `$n` placeholders, and at run time binds, plans and
//! executes the fragment through the ordinary pipeline with the frame's values
//! substituted for the placeholders.
//!
//! That split is why compilation needs no catalog: a fragment is text, and the
//! types in it are resolved when it is bound, per call. A body can therefore be
//! stored before the tables and types it mentions exist — which is also
//! PostgreSQL's behavior, and what makes a recursive routine definable at all.
//!
//! # Layering
//!
//! This crate sits *above* the executor and depends on the binder and planner,
//! because interpreting a statement means re-entering the whole pipeline. The
//! executor therefore cannot depend on it; expression evaluation reaches a
//! routine through [`crabgresql_executor::RoutineOps`], a trait object the
//! server installs on the execution context — the same escape hatch the
//! sequence and catalog functions already use.

pub mod ast;
mod lexer;
mod parse;

pub use ast::{CompileError, Routine};
pub use parse::{compile, compile_inline_block};
