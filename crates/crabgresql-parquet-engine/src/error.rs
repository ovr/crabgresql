//! The `StorageError` constructors this engine builds its messages with.

use crabgresql_storage_api::StorageError;

pub(crate) fn io_error(context: &str, error: impl std::fmt::Display) -> StorageError {
    StorageError::Io(format!("{context}: {error}"))
}

pub(crate) fn corrupt(context: impl Into<String>) -> StorageError {
    StorageError::CorruptData(context.into())
}

pub(crate) fn unsupported(message: impl Into<String>) -> StorageError {
    StorageError::UnsupportedOperation(message.into())
}
