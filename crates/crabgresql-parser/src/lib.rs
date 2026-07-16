//! Parser: a thin wrapper over sqlparser-rs with the PostgreSQL dialect.
//!
//! Grammar gaps relative to PG are closed by upstream PRs; divergences are
//! caught by differential testing (docs/ARCHITECTURE.md §1.1).

pub use sqlparser::ast;

use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_select_one() {
        let stmts = parse("SELECT 1").unwrap();
        assert_eq!(stmts.len(), 1);
        assert!(matches!(stmts[0], ast::Statement::Query(_)));
    }

    #[test]
    fn parses_multiple_statements() {
        let stmts = parse("SELECT 1; SELECT 2;").unwrap();
        assert_eq!(stmts.len(), 2);
    }

    #[test]
    fn empty_input_yields_no_statements() {
        assert!(parse("").unwrap().is_empty());
        assert!(parse("  -- just a comment").unwrap().is_empty());
    }

    #[test]
    fn syntax_error_is_reported() {
        assert!(parse("SELEC 1").is_err());
    }
}
