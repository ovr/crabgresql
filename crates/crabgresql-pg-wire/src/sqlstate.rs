//! SQLSTATE error codes (PostgreSQL Appendix A). Only the codes we emit.

pub const FEATURE_NOT_SUPPORTED: &str = "0A000";
/// `21000` — a scalar subquery used as an expression returned more than one row.
pub const CARDINALITY_VIOLATION: &str = "21000";
pub const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
pub const INVALID_DATETIME_FORMAT: &str = "22007";
pub const DATETIME_FIELD_OVERFLOW: &str = "22008";
pub const INVALID_TIME_ZONE_DISPLACEMENT_VALUE: &str = "22009";
pub const DIVISION_BY_ZERO: &str = "22012";
pub const INVALID_TEXT_REPRESENTATION: &str = "22P02";
/// Class 22 — a byte sequence that is not valid in the server encoding (UTF-8),
/// e.g. an invalid byte or embedded NUL in COPY data.
pub const CHARACTER_NOT_IN_REPERTOIRE: &str = "22021";
pub const INVALID_PARAMETER_VALUE: &str = "22023";
pub const INVALID_ESCAPE_SEQUENCE: &str = "22025";
pub const INVALID_ARGUMENT_FOR_LOG: &str = "2201E";
pub const INVALID_ARGUMENT_FOR_POWER_FUNCTION: &str = "2201F";
pub const INVALID_ROW_COUNT_IN_LIMIT_CLAUSE: &str = "2201W";
pub const INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE: &str = "2201X";
pub const INSUFFICIENT_PRIVILEGE: &str = "42501";
pub const SYNTAX_ERROR: &str = "42601";
pub const DUPLICATE_ALIAS: &str = "42712";
pub const UNDEFINED_COLUMN: &str = "42703";
pub const AMBIGUOUS_COLUMN: &str = "42702";
pub const INVALID_COLUMN_REFERENCE: &str = "42P10";
pub const UNDEFINED_OBJECT: &str = "42704";
pub const UNDEFINED_FUNCTION: &str = "42883";
pub const AMBIGUOUS_FUNCTION: &str = "42725";
pub const WRONG_OBJECT_TYPE: &str = "42809";
pub const DUPLICATE_COLUMN: &str = "42701";
pub const DUPLICATE_OBJECT: &str = "42710";
pub const DUPLICATE_FUNCTION: &str = "42723";
pub const CANNOT_COERCE: &str = "42846";
pub const DATATYPE_MISMATCH: &str = "42804";
pub const GROUPING_ERROR: &str = "42803";
pub const UNDEFINED_TABLE: &str = "42P01";
pub const DUPLICATE_TABLE: &str = "42P07";
/// `42P16` — a relation definition is invalid (e.g. a `CREATE OR REPLACE VIEW`
/// that renames, drops, or retypes an existing view column).
pub const INVALID_TABLE_DEFINITION: &str = "42P16";
pub const INVALID_OBJECT_DEFINITION: &str = "42P17";
pub const DEPENDENT_OBJECTS_STILL_EXIST: &str = "2BP01";
pub const PROTOCOL_VIOLATION: &str = "08P01";
/// `26000` — a Bind/Describe names a prepared statement that does not exist.
pub const INVALID_SQL_STATEMENT_NAME: &str = "26000";
/// `42P05` — Parse names a prepared statement that already exists.
pub const DUPLICATE_PREPARED_STATEMENT: &str = "42P05";
/// `34000` — an Execute/Describe names a portal (cursor) that does not exist.
pub const INVALID_CURSOR_NAME: &str = "34000";
/// `42P02` — a `$n` placeholder with no such parameter (e.g. in a simple query).
pub const UNDEFINED_PARAMETER: &str = "42P02";
/// `42P18` — a parameter's type could not be determined / is inconsistent.
pub const INDETERMINATE_DATATYPE: &str = "42P18";
/// `22P03` — a binary parameter value the type's receive function rejects.
pub const INVALID_BINARY_REPRESENTATION: &str = "22P03";
/// `XX000` — an internal invariant was violated (should be unreachable).
pub const INTERNAL_ERROR: &str = "XX000";
pub const ACTIVE_SQL_TRANSACTION: &str = "25001";
pub const NO_ACTIVE_SQL_TRANSACTION: &str = "25P01";
pub const IN_FAILED_SQL_TRANSACTION: &str = "25P02";
pub const READ_ONLY_SQL_TRANSACTION: &str = "25006";
/// `55000` — the object is not in a state the operation requires, e.g. `currval`
/// / `lastval` before any `nextval` has run in the session.
pub const OBJECT_NOT_IN_PREREQUISITE_STATE: &str = "55000";
/// `2200H` — a sequence hit its `MINVALUE`/`MAXVALUE` bound with `NO CYCLE`.
pub const SEQUENCE_GENERATOR_LIMIT_EXCEEDED: &str = "2200H";
/// Class 22 — data exception: a COPY data stream that does not match the
/// expected format (extra/missing columns, unterminated CSV quoting).
pub const BAD_COPY_FILE_FORMAT: &str = "22P04";
/// Class 57 — operator intervention: a client-issued CopyFail during COPY FROM.
pub const QUERY_CANCELED: &str = "57014";
/// Class 58 — system error: a WAL/data-file I/O failure (e.g. commit fsync).
pub const IO_ERROR: &str = "58030";
