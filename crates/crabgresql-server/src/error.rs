//! Errors reported to the client as `ErrorResponse` (SQLSTATE + message).

use std::borrow::Cow;

use crabgresql_pg_wire::{ErrorFields, sqlstate};
use crabgresql_storage_api::StorageError;

#[derive(Debug)]
pub struct PgError {
    /// 5-character SQLSTATE code. A `Cow` because a routine body can name its
    /// own SQLSTATE at runtime (`RAISE ... USING ERRCODE`); every built-in
    /// error still passes a `&'static str` from [`sqlstate`].
    pub code: Cow<'static, str>,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    /// 1-based (line, column) of the offending token, when PG reports a cursor
    /// position. Converted to a wire character offset when the error is sent.
    pub location: Option<(u64, u64)>,
    /// The `CONTEXT:` traceback: the call frames this error unwound through,
    /// innermost first. Empty for an error raised at the top level.
    pub context: Vec<String>,
}

impl std::fmt::Display for PgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PgError {}

impl PgError {
    pub fn new(code: impl Into<Cow<'static, str>>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            detail: None,
            hint: None,
            location: None,
            context: Vec::new(),
        }
    }

    pub fn feature_not_supported(message: impl Into<String>) -> Self {
        Self::new(sqlstate::FEATURE_NOT_SUPPORTED, message)
    }

    pub fn syntax(message: impl Into<String>) -> Self {
        Self::new(sqlstate::SYNTAX_ERROR, message)
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// The wire fields for this error. `position` is the cursor offset the
    /// caller resolved from [`PgError::location`] — only the caller holds the
    /// SQL text needed to convert (line, column) into a character offset.
    pub fn to_fields(&self, position: Option<usize>) -> ErrorFields {
        let mut fields = ErrorFields::error(&self.code, &self.message);
        if let Some(detail) = &self.detail {
            fields = fields.with_detail(detail);
        }
        if let Some(hint) = &self.hint {
            fields = fields.with_hint(hint);
        }
        if let Some(position) = position {
            fields = fields.with_position(position);
        }
        if !self.context.is_empty() {
            fields = fields.with_context(&self.context.join("\n"));
        }
        fields
    }
}

impl From<crabgresql_parser::ParseError> for PgError {
    fn from(e: crabgresql_parser::ParseError) -> Self {
        Self {
            code: Cow::Borrowed(e.sqlstate),
            message: e.message,
            detail: None,
            hint: e.hint,
            location: e.location,
            context: Vec::new(),
        }
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
            StorageError::SchemaAlreadyExists(_) => sqlstate::DUPLICATE_SCHEMA,
            StorageError::SchemaNotFound(_) => sqlstate::INVALID_SCHEMA_NAME,
            StorageError::UnsupportedOperation(_) | StorageError::UnsupportedType(_) => {
                sqlstate::FEATURE_NOT_SUPPORTED
            }
            StorageError::Io(_) => sqlstate::IO_ERROR,
            StorageError::CorruptData(_) => "XX001",
            StorageError::RowTooBig { .. }
            | StorageError::ValueTooBig { .. }
            | StorageError::IndexRowTooBig { .. } => sqlstate::PROGRAM_LIMIT_EXCEEDED,
        };
        let (detail, hint) = (e.detail(), e.hint());
        Self {
            detail,
            hint,
            ..Self::new(code, e.to_string())
        }
    }
}

impl From<crabgresql_binder::BindError> for PgError {
    fn from(e: crabgresql_binder::BindError) -> Self {
        Self {
            code: Cow::Borrowed(e.code),
            message: e.message,
            detail: e.detail,
            hint: e.hint,
            location: e.location,
            context: e.context,
        }
    }
}

impl From<crabgresql_executor::ExecError> for PgError {
    fn from(e: crabgresql_executor::ExecError) -> Self {
        Self {
            code: e.code,
            message: e.message,
            detail: e.detail,
            hint: e.hint,
            location: None,
            context: e.context,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_executor::ExecError;

    /// An error raised inside a routine body carries its HINT and its CONTEXT
    /// traceback all the way to the wire — a runtime error is the one path that
    /// used to drop both.
    #[test]
    fn exec_error_hint_and_context_reach_the_wire() {
        let exec = ExecError::new("P0001", "boom")
            .with_hint(Some("try harder".into()))
            .push_context("PL/pgSQL function inner() line 3 at RAISE")
            .push_context("PL/pgSQL function outer() line 2 at PERFORM");
        let fields = PgError::from(exec).to_fields(None);

        assert_eq!(fields.code(), "P0001");
        assert_eq!(fields.get(b'H'), Some("try harder"));
        // Innermost frame first, newline-joined into a single `W` field.
        assert_eq!(
            fields.get(b'W'),
            Some(
                "PL/pgSQL function inner() line 3 at RAISE\n\
                 PL/pgSQL function outer() line 2 at PERFORM"
            )
        );
    }

    /// A top-level error emits no `W` field at all, rather than an empty one.
    #[test]
    fn no_frames_means_no_context_field() {
        let fields = PgError::new(sqlstate::SYNTAX_ERROR, "syntax error").to_fields(None);
        assert_eq!(fields.get(b'W'), None);
    }
}
