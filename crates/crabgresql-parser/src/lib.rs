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
pub mod parser;
pub mod tokenizer;

#[doc(hidden)]
// This is required to make utilities accessible by both the crate-internal
// unit-tests and by the integration tests <https://stackoverflow.com/a/44541071/1026>
// External users are not supposed to rely on this module.
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
/// already carries its own position text. The exceptions are the escape
/// sequences inside `E'…'` and `U&'…'`, which reproduce a specific PostgreSQL
/// diagnostic: those set a different [`ParseError::sqlstate`], may add a
/// [`ParseError::hint`], and report [`ParseError::location`] separately so the
/// protocol layer can turn it into a `LINE n:` cursor.
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
/// declaration's type, which is lifted out of a routine body as text.
pub fn parse_data_type(sql: &str) -> Result<ast::DataType, ParseError> {
    let mut parser = Parser::new(&PostgreSqlDialect {})
        .try_with_sql(sql)
        .map_err(ParseError::from)?;
    let data_type = parser.parse_data_type().map_err(ParseError::from)?;
    parser
        .expect_token(&tokenizer::Token::EOF)
        .map_err(ParseError::from)?;
    Ok(data_type)
}

#[cfg(test)]
mod wrapper_tests {
    use super::*;

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
