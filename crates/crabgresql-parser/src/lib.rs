// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! CrabgreSQL SQL parser.
//!
//! This is an in-tree hard fork of Apache DataFusion's `sqlparser-rs`
//! (<https://github.com/apache/datafusion-sqlparser-rs>), pinned at v0.62.0
//! (see `UPSTREAM`). We vendor it so PostgreSQL-specific grammar can be added
//! directly instead of waiting on upstream review; divergences are caught by
//! differential testing (docs/ARCHITECTURE.md §1.1). The upstream `ast`,
//! `parser`, `tokenizer` and `keywords` modules are re-exported; the `dialect`
//! module has been pruned to only [`dialect::GenericDialect`] and
//! [`dialect::PostgreSqlDialect`] (CrabgreSQL only parses PostgreSQL), and the
//! dialect-specific grammar for the other dialects has been removed.
//! [`parse`] is the CrabgreSQL entry point (PostgreSQL dialect).

#![cfg_attr(not(feature = "std"), no_std)]
#![allow(clippy::upper_case_acronyms)]
// Permit large enum variants to keep a unified, expressive AST.
// Splitting complex nodes (expressions, statements, types) into separate types
// would bloat the API and hide intent. Extra memory is a worthwhile tradeoff.
#![allow(clippy::large_enum_variant)]
#![forbid(clippy::unreachable)]
#![forbid(missing_docs)]

// Allow proc-macros to find this crate
extern crate self as sqlparser;

#[cfg(not(feature = "std"))]
extern crate alloc;

#[macro_use]
#[cfg(test)]
extern crate pretty_assertions;

pub mod ast;
#[macro_use]
/// Submodules for SQL dialects.
pub mod dialect;

#[cfg(feature = "derive-dialect")]
pub use dialect::derive_dialect;
mod display_utils;
pub mod keywords;
pub mod numlit;
pub mod parser;
pub mod tokenizer;

#[doc(hidden)]
// Public because upstream shared these helpers with its integration tests
// <https://stackoverflow.com/a/44541071/1026>; this fork vendored no `tests/`
// directory, so only the crate's own unit tests use them. External users are
// not supposed to rely on this module.
pub mod test_utils;

// ---------------------------------------------------------------------------
// CrabgreSQL wrapper: the PostgreSQL-dialect entry point over the vendored parser.
// ---------------------------------------------------------------------------

pub use crate::tokenizer::{Location, Span};

use crate::dialect::PostgreSqlDialect;
use crate::parser::{Parser, ParserError};

/// The SQLSTATE a parse failure carries when it has no more specific one.
pub const SYNTAX_ERROR: &str = tokenizer::DEFAULT_TOKENIZER_SQLSTATE;

/// Error returned when a SQL string cannot be parsed.
///
/// Almost every parse failure is a plain `42601` syntax error whose message
/// already carries its own position text. The exceptions are the failures that
/// reproduce a specific PostgreSQL diagnostic — a bad escape inside `E'…'` or
/// `U&'…'`, a malformed numeric literal, a redundant `COPY` option, a reserved
/// word used as a column name, and others: their message is PG's own wording,
/// to be shown verbatim, and they report [`ParseError::location`] separately so
/// the protocol layer can turn it into a `LINE n:` cursor. Only the
/// escape-string ones ever carry a different [`ParseError::sqlstate`] or a
/// [`ParseError::hint`]; every other exception is still `42601` with no hint.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ParseError {
    /// The text shown to the client, without a SQLSTATE or position prefix.
    pub message: String,
    /// 5-character SQLSTATE; [`SYNTAX_ERROR`] unless the failure reproduces a
    /// more specific PostgreSQL condition.
    pub sqlstate: &'static str,
    /// Optional `HINT:` line.
    pub hint: Option<String>,
    /// 1-based (line, column) of the offending token, when PG reports a cursor
    /// position for this condition.
    pub location: Option<(u64, u64)>,
}

impl From<ParserError> for ParseError {
    fn from(e: ParserError) -> Self {
        match e {
            ParserError::PgDiagnostic(d) => ParseError {
                message: d.message,
                sqlstate: d.sqlstate,
                hint: d.hint,
                // A zero location is how the tokenizer says "no cursor here" —
                // PG omits the position on an encoding error, for instance.
                location: (d.location.line != 0).then_some((d.location.line, d.location.column)),
            },
            other => ParseError {
                message: other.to_string(),
                sqlstate: SYNTAX_ERROR,
                hint: None,
                location: None,
            },
        }
    }
}

/// Parse a query string into a list of statements.
///
/// An empty (or comment-only) string yields an empty list, which the protocol
/// layer must answer with `EmptyQueryResponse`.
pub fn parse(sql: &str) -> Result<Vec<ast::Statement>, ParseError> {
    Parser::parse_sql(&PostgreSqlDialect {}, sql).map_err(ParseError::from)
}

/// Parse a bare SQL type name, e.g. `numeric(10, 2)`, `text[]` or a
/// `CREATE TYPE` name. Trailing input is an error, so a caller cannot silently
/// accept `int; DROP TABLE t`.
///
/// Used for type names that reach us outside a statement — a PL/pgSQL
/// declaration's type, which is lifted out of a routine body as text, and
/// `regtype`/`regprocedure` input.
///
/// A failure is reported the way PostgreSQL reports it: `syntax error at or
/// near "<token>"`, pointing at the token the type grammar stopped on, rather
/// than in the fork's own `Expected: …, found: …` wording. PostgreSQL parses
/// exactly this string in its `RAW_PARSE_TYPE_NAME` mode, so the token it
/// stops on is the same one — verified against 18.4 for `int4 int4`, `4`,
/// `'x'`, `"int4" "int4"`, `numeric(10,2) x`, `.int4`, `-1` and `int4[1,2]`.
pub fn parse_data_type(sql: &str) -> Result<ast::DataType, ParseError> {
    let mut parser = Parser::new(&PostgreSqlDialect {})
        .try_with_sql(sql)
        .map_err(ParseError::from)?;
    let parsed = parser
        .parse_data_type()
        .and_then(|ty| parser.expect_token(&tokenizer::Token::EOF).map(|_| ty));
    match parsed {
        Ok(data_type) => Ok(data_type),
        // A diagnostic the fork already shaped like PostgreSQL's stays as it
        // is; only the generic parser errors need rewording.
        Err(e @ ParserError::PgDiagnostic(_)) => Err(ParseError::from(e)),
        Err(e) => Err(type_name_syntax_error(sql, &e)),
    }
}

/// PostgreSQL's `syntax error` for a type name, naming the token the parse
/// stopped on.
///
/// The token is read back out of the fork's own message rather than off the
/// parser, because where the parser is left standing depends on which check
/// failed — `parse_data_type` consumes the token it rejects while
/// `expect_token` does not — and `found:` names it either way.
fn type_name_syntax_error(sql: &str, e: &ParserError) -> ParseError {
    let text = e.to_string();
    let found = text
        .rsplit_once("found: ")
        .map(|(_, rest)| rest.split(" at Line:").next().unwrap_or(rest).trim());
    // Two ways to run out of input, and PostgreSQL words both the same way: no
    // token at all, and a trailing `.` that promises another name part —
    // `'int4.'` is `syntax error at end of input` where `'.int4'` is `syntax
    // error at or near "."`.
    let at_end = match found {
        None | Some("EOF") => true,
        Some(".") => sql.trim_end().ends_with('.'),
        Some(_) => false,
    };
    let message = match found {
        Some(token) if !at_end => format!("syntax error at or near \"{token}\""),
        _ => "syntax error at end of input".to_string(),
    };
    ParseError {
        message,
        sqlstate: SYNTAX_ERROR,
        hint: None,
        location: None,
    }
}

#[cfg(test)]
mod wrapper_tests {
    use super::*;

    /// A malformed type name reports the token PostgreSQL reports, in
    /// PostgreSQL's wording. Every pair here is `SELECT '<spec>'::regprocedure`
    /// on 18.4, whose argument types go through this same grammar.
    ///
    /// TODO: a spelling *this* parser accepts and PostgreSQL's does not still
    /// gets no error at all — a bare reserved word (`select`, `from`) reads as
    /// a type name here, so a caller reports `type "select" does not exist`
    /// where PG reports a syntax error. Categories do not decide it: PG accepts
    /// `left` and `binary` as type names (`type "left" does not exist`) and
    /// rejects `setof` and `none`, so closing this means following its
    /// `Typename` grammar rather than a keyword list.
    #[test]
    fn a_malformed_type_name_reports_postgresqls_token() {
        let err = |s: &str| parse_data_type(s).expect_err("malformed").message;
        assert_eq!(err("int4 int4"), "syntax error at or near \"int4\"");
        assert_eq!(err("4"), "syntax error at or near \"4\"");
        assert_eq!(err("'x'"), "syntax error at or near \"'x'\"");
        assert_eq!(err("numeric(10,2) x"), "syntax error at or near \"x\"");
        assert_eq!(err(".int4"), "syntax error at or near \".\"");
        assert_eq!(err("-1"), "syntax error at or near \"-\"");
        assert_eq!(err("int4[1,2]"), "syntax error at or near \",\"");
        // A quoted token keeps its quotes, as PG echoes it.
        assert_eq!(
            err("\"int4\" \"int4\""),
            "syntax error at or near \"\"int4\"\""
        );
        // Running out of input, spelled the same way for a trailing dot as for
        // an unfinished modifier.
        assert_eq!(err("int4."), "syntax error at end of input");
        assert_eq!(err("int4("), "syntax error at end of input");
        // Everything the grammar does accept is untouched.
        for ok in [
            "numeric",
            "int4[]",
            "varchar(10)",
            "pg_catalog.int4",
            "\"char\"",
        ] {
            assert!(parse_data_type(ok).is_ok(), "{ok} is a type name");
        }
    }

    #[test]
    fn parses_select_one() -> anyhow::Result<()> {
        let stmts = parse("SELECT 1")?;
        assert_eq!(stmts.len(), 1);
        assert!(matches!(stmts[0], ast::Statement::Query(_)));

        Ok(())
    }

    /// PostgreSQL's inheritance qualifiers on a FROM item. `ONLY t` and
    /// `ONLY (t)` are the same thing; `t*` is the default spelled out, so it
    /// parses to `only = false` and renders back as the bare name.
    #[test]
    fn parses_inheritance_qualifiers_on_a_from_item() -> anyhow::Result<()> {
        let only_of = |sql: &str| -> anyhow::Result<(bool, String)> {
            let stmts = parse(sql)?;
            let ast::Statement::Query(query) = &stmts[0] else {
                anyhow::bail!("expected a query");
            };
            let ast::SetExpr::Select(select) = query.body.as_ref() else {
                anyhow::bail!("expected a SELECT");
            };
            let ast::TableFactor::Table { only, name, .. } = &select.from[0].relation else {
                anyhow::bail!("expected a table factor");
            };
            Ok((*only, name.to_string()))
        };
        assert_eq!(only_of("SELECT * FROM ONLY road")?, (true, "road".into()));
        assert_eq!(only_of("SELECT * FROM ONLY (road)")?, (true, "road".into()));
        assert_eq!(only_of("SELECT * FROM road")?, (false, "road".into()));
        assert_eq!(only_of("SELECT * FROM road*")?, (false, "road".into()));
        assert_eq!(
            only_of("SELECT * FROM ONLY public.road r")?,
            (true, "public.road".into())
        );
        // `ONLY` survives a round trip through Display.
        assert_eq!(
            parse("SELECT * FROM ONLY road")?[0].to_string(),
            "SELECT * FROM ONLY road"
        );
        // UPDATE and DELETE take it on their target too.
        let stmts = parse("DELETE FROM ONLY road WHERE name = 'x'")?;
        let ast::Statement::Delete(delete) = &stmts[0] else {
            anyhow::bail!("expected a DELETE");
        };
        let (ast::FromTable::WithFromKeyword(from) | ast::FromTable::WithoutKeyword(from)) =
            &delete.from;
        assert!(matches!(
            from[0].relation,
            ast::TableFactor::Table { only: true, .. }
        ));

        Ok(())
    }

    #[test]
    fn parses_multiple_statements() -> anyhow::Result<()> {
        let stmts = parse("SELECT 1; SELECT 2;")?;
        assert_eq!(stmts.len(), 2);

        Ok(())
    }

    #[test]
    fn empty_input_yields_no_statements() -> anyhow::Result<()> {
        assert!(parse("")?.is_empty());
        assert!(parse("  -- just a comment")?.is_empty());

        Ok(())
    }

    #[test]
    fn syntax_error_is_reported() {
        assert!(parse("SELEC 1").is_err());
    }

    #[test]
    fn parses_postgresql_table_access_method() -> anyhow::Result<()> {
        let statements = parse("CREATE TABLE events (id int4) USING parquet")?;
        let [ast::Statement::CreateTable(create)] = statements.as_slice() else {
            anyhow::bail!("expected CREATE TABLE");
        };
        let Some(ast::HiveIOFormat::Using { format }) = create
            .hive_formats
            .as_ref()
            .and_then(|formats| formats.storage.as_ref())
        else {
            anyhow::bail!("expected USING table access method");
        };
        assert_eq!(format.value, "parquet");
        Ok(())
    }
}
