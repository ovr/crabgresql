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
use crate::parser::Parser;

/// Error returned when a SQL string cannot be parsed.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ParseError(String);

/// Parse a query string into a list of statements.
///
/// An empty (or comment-only) string yields an empty list, which the protocol
/// layer must answer with `EmptyQueryResponse`.
pub fn parse(sql: &str) -> Result<Vec<ast::Statement>, ParseError> {
    Parser::parse_sql(&PostgreSqlDialect {}, sql).map_err(|e| ParseError(e.to_string()))
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
        .map_err(|e| ParseError(e.to_string()))?;
    let data_type = parser
        .parse_data_type()
        .map_err(|e| ParseError(e.to_string()))?;
    parser
        .expect_token(&tokenizer::Token::EOF)
        .map_err(|e| ParseError(e.to_string()))?;
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
}
