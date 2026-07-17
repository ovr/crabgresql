//! Per-connection session state (GUCs). Grows a temp-table catalog in a later
//! milestone; for now it carries the settings runtime evaluation depends on.

use crabgresql_executor::ExecContext;

pub struct Session {
    /// `extra_float_digits` GUC — controls float→text output precision.
    pub extra_float_digits: i32,
}

impl Session {
    pub fn new() -> Self {
        // PG's default since v12.
        Self {
            extra_float_digits: 1,
        }
    }

    pub fn exec_context(&self) -> ExecContext {
        ExecContext {
            extra_float_digits: self.extra_float_digits,
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}
