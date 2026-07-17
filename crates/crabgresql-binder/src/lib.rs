//! Binder: semantic analysis turning the sqlparser AST into a typed logical
//! plan — name resolution against table schemas, operator type inference with
//! PG semantics (int4/int8 promotion, untyped-literal coercion), and honest
//! rejection (`0A000`) of everything parsed but not yet executable.

mod expr;
mod functions;
mod plan;

pub use expr::{BinOp, Binding, BoundExpr, Scope, UnaryOp, bind_expr, bind_scalar, map_data_type};
pub use functions::{ScalarFn, TableFn, lookup_table_fn};
pub use plan::{LogicalPlan, SortKey, bind_delete, bind_insert, bind_query, bind_update};

use crabgresql_parser::Span;
use crabgresql_protocol::sqlstate;
use crabgresql_storage_api::StorageError;
use crabgresql_types::PgType;

/// One column of a plan's result set, as needed for `RowDescription`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputColumn {
    pub name: String,
    pub ty: PgType,
}

/// A bind-time error, reported to the client as `ErrorResponse`.
#[derive(Debug)]
pub struct BindError {
    /// 5-character SQLSTATE code.
    pub code: &'static str,
    pub message: String,
    /// 1-based (line, column) of the offending token, when PG reports a
    /// cursor position (`LINE n: ... ^`). Only set for literal input-function
    /// failures, mirroring PG.
    pub location: Option<(u64, u64)>,
}

impl BindError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            location: None,
        }
    }

    /// Attach the cursor position from a token span (ignored if the span is
    /// empty, i.e. line 0).
    pub fn at(mut self, span: Span) -> Self {
        if span.start.line != 0 {
            self.location = Some((span.start.line, span.start.column));
        }
        self
    }

    pub fn feature_not_supported(message: impl Into<String>) -> Self {
        Self::new(sqlstate::FEATURE_NOT_SUPPORTED, message)
    }

    pub fn syntax(message: impl Into<String>) -> Self {
        Self::new(sqlstate::SYNTAX_ERROR, message)
    }
}

impl From<StorageError> for BindError {
    fn from(e: StorageError) -> Self {
        let code = match &e {
            StorageError::TableNotFound(_) => sqlstate::UNDEFINED_TABLE,
            StorageError::TableAlreadyExists(_) => sqlstate::DUPLICATE_TABLE,
        };
        Self::new(code, e.to_string())
    }
}
