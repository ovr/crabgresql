//! SQLSTATE error codes (PostgreSQL Appendix A). Only the codes we emit.

pub const FEATURE_NOT_SUPPORTED: &str = "0A000";
pub const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
pub const DIVISION_BY_ZERO: &str = "22012";
pub const INVALID_TEXT_REPRESENTATION: &str = "22P02";
pub const SYNTAX_ERROR: &str = "42601";
pub const UNDEFINED_COLUMN: &str = "42703";
pub const UNDEFINED_FUNCTION: &str = "42883";
pub const AMBIGUOUS_FUNCTION: &str = "42725";
pub const DUPLICATE_COLUMN: &str = "42701";
pub const DATATYPE_MISMATCH: &str = "42804";
pub const UNDEFINED_TABLE: &str = "42P01";
pub const DUPLICATE_TABLE: &str = "42P07";
pub const PROTOCOL_VIOLATION: &str = "08P01";
