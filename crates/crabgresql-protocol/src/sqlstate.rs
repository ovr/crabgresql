//! SQLSTATE error codes (PostgreSQL Appendix A). Only the codes we emit.

pub const FEATURE_NOT_SUPPORTED: &str = "0A000";
pub const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
pub const INVALID_DATETIME_FORMAT: &str = "22007";
pub const DATETIME_FIELD_OVERFLOW: &str = "22008";
pub const DIVISION_BY_ZERO: &str = "22012";
pub const INVALID_TEXT_REPRESENTATION: &str = "22P02";
pub const INVALID_PARAMETER_VALUE: &str = "22023";
pub const INVALID_ARGUMENT_FOR_LOG: &str = "2201E";
pub const INVALID_ARGUMENT_FOR_POWER_FUNCTION: &str = "2201F";
pub const SYNTAX_ERROR: &str = "42601";
pub const DUPLICATE_ALIAS: &str = "42712";
pub const UNDEFINED_COLUMN: &str = "42703";
pub const INVALID_COLUMN_REFERENCE: &str = "42P10";
pub const UNDEFINED_OBJECT: &str = "42704";
pub const UNDEFINED_FUNCTION: &str = "42883";
pub const AMBIGUOUS_FUNCTION: &str = "42725";
pub const DUPLICATE_COLUMN: &str = "42701";
pub const DUPLICATE_OBJECT: &str = "42710";
pub const CANNOT_COERCE: &str = "42846";
pub const DATATYPE_MISMATCH: &str = "42804";
pub const GROUPING_ERROR: &str = "42803";
pub const UNDEFINED_TABLE: &str = "42P01";
pub const DUPLICATE_TABLE: &str = "42P07";
pub const PROTOCOL_VIOLATION: &str = "08P01";
