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
//!
//! # Known divergences from PostgreSQL
//!
//! - **Variable/column conflicts resolve in the variable's favour.** When a
//!   routine variable and a column of a table in the query share a name, the
//!   variable wins. PostgreSQL makes this configurable through
//!   `plpgsql.variable_conflict` and defaults to raising an ambiguity error;
//!   deciding that needs the binder's view of which columns are in scope, which
//!   compilation deliberately does not have. Variable-wins is PostgreSQL's
//!   historical behavior and what almost all real code assumes.
//! - **Assignment uses an explicit cast, not an assignment cast.** The two
//!   differ only at the edges — an explicit cast to `varchar(5)` truncates
//!   where an assignment cast would raise.
//! - **`EXCEPTION` handlers are rejected.** A handler needs a subtransaction
//!   per block entry, and this engine has no savepoints yet. Without handlers,
//!   an error in a body aborts the whole transaction — which is exactly what
//!   PostgreSQL does for a body that has none.
//! - `SETOF` / `RETURN NEXT` / `RETURN QUERY`, cursors, `EXECUTE`,
//!   `FOR ... IN <query>`, `FOREACH`, `GET DIAGNOSTICS` and record / `%TYPE`
//!   variables are not implemented; each is reported as `0A000` by name rather
//!   than as a syntax error.

pub mod ast;
mod condition;
mod exec;
mod frame;
mod lexer;
mod parse;

pub use ast::{CompileError, Routine};
pub use exec::{Interpreter, RoutineCache, RoutineDef, RoutineSource};
pub use parse::{compile, compile_inline_block};
