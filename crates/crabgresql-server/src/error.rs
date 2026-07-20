//! Errors reported to the client as `ErrorResponse` (SQLSTATE + message).

use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::StorageError;

#[derive(Debug)]
pub struct PgError {
    /// 5-character SQLSTATE code.
    pub code: &'static str,
    pub message: String,
    /// Optional DETAIL line.
    pub detail: Option<String>,
    /// Optional HINT line.
    pub hint: Option<String>,
    /// 1-based (line, column) of the offending token, when PG reports a cursor
    /// position. Converted to a wire character offset when the error is sent.
    pub location: Option<(u64, u64)>,
}

impl std::fmt::Display for PgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PgError {}

impl PgError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
            hint: None,
            location: None,
        }
    }

    pub fn feature_not_supported(message: impl Into<String>) -> Self {
        Self::new(sqlstate::FEATURE_NOT_SUPPORTED, message)
    }

    pub fn syntax(message: impl Into<String>) -> Self {
        Self::new(sqlstate::SYNTAX_ERROR, message)
    }

    /// Attach a DETAIL line.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Attach a HINT line.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl From<StorageError> for PgError {
    fn from(e: StorageError) -> Self {
        let code = match &e {
            StorageError::TableNotFound(_) | StorageError::IndexTableNotFound(_) => {
                sqlstate::UNDEFINED_TABLE
            }
            StorageError::TableAlreadyExists(_) | StorageError::RelationAlreadyExists(_) => {
                sqlstate::DUPLICATE_TABLE
            }
        };
        Self::new(code, e.to_string())
    }
}

impl From<crabgresql_binder::BindError> for PgError {
    fn from(e: crabgresql_binder::BindError) -> Self {
        Self {
            code: e.code,
            message: e.message,
            detail: e.detail,
            hint: e.hint,
            location: e.location,
        }
    }
}

impl From<crabgresql_executor::ExecError> for PgError {
    fn from(e: crabgresql_executor::ExecError) -> Self {
        Self {
            code: e.code,
            message: e.message,
            detail: e.detail,
            hint: None,
            location: None,
        }
    }
}
