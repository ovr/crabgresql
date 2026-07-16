//! Errors reported to the client as `ErrorResponse` (SQLSTATE + message).

use crabgresql_protocol::sqlstate;
use crabgresql_storage_api::StorageError;

#[derive(Debug)]
pub struct PgError {
    /// 5-character SQLSTATE code.
    pub code: &'static str,
    pub message: String,
}

impl PgError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
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
