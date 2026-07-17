//! Errors reported to the client as `ErrorResponse` (SQLSTATE + message).

use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::StorageError;

#[derive(Debug)]
pub struct PgError {
    /// 5-character SQLSTATE code.
    pub code: &'static str,
    pub message: String,
    /// 1-based (line, column) of the offending token, when PG reports a cursor
    /// position. Converted to a wire character offset when the error is sent.
    pub location: Option<(u64, u64)>,
}

impl PgError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            location: None,
        }
    }

    pub fn feature_not_supported(message: impl Into<String>) -> Self {
        Self::new(sqlstate::FEATURE_NOT_SUPPORTED, message)
    }

    pub fn syntax(message: impl Into<String>) -> Self {
        Self::new(sqlstate::SYNTAX_ERROR, message)
    }
}

impl From<StorageError> for PgError {
    fn from(e: StorageError) -> Self {
        let code = match &e {
            StorageError::TableNotFound(_) => sqlstate::UNDEFINED_TABLE,
            StorageError::TableAlreadyExists(_) => sqlstate::DUPLICATE_TABLE,
        };
        Self::new(code, e.to_string())
    }
}

impl From<crabgresql_binder::BindError> for PgError {
    fn from(e: crabgresql_binder::BindError) -> Self {
        Self {
            code: e.code,
            message: e.message,
            location: e.location,
        }
    }
}

impl From<crabgresql_executor::ExecError> for PgError {
    fn from(e: crabgresql_executor::ExecError) -> Self {
        Self::new(e.code, e.message)
    }
}
