//! Carrying a batch error into the row engine's error type.

use crabgresql_batch::BatchError;
use crabgresql_executor::ExecError;

/// Lift a [`BatchError`] into an [`ExecError`], field for field.
///
/// Lossless on purpose. A kernel that raises `22003 integer out of range` must
/// reach the client as exactly that, because a user who can tell which engine
/// ran their query from the error text has found a divergence. The two types
/// exist separately only because `crabgresql-batch` sits below the executor and
/// cannot name its error; a plain function rather than a `From` impl because
/// both types are foreign to this crate.
pub fn to_exec_error(error: BatchError) -> ExecError {
    ExecError {
        code: error.code,
        message: error.message,
        detail: error.detail,
        hint: error.hint,
        context: Vec::new(),
    }
}
