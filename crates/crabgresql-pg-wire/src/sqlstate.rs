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
/// A configuration parameter exists but cannot be assigned.
pub const CANT_CHANGE_RUNTIME_PARAM: &str = "55P02";
pub const UNDEFINED_FUNCTION: &str = "42883";
pub const AMBIGUOUS_FUNCTION: &str = "42725";
pub const WRONG_OBJECT_TYPE: &str = "42809";
pub const DUPLICATE_COLUMN: &str = "42701";
pub const DUPLICATE_OBJECT: &str = "42710";
pub const DUPLICATE_FUNCTION: &str = "42723";
pub const CANNOT_COERCE: &str = "42846";
pub const DATATYPE_MISMATCH: &str = "42804";
pub const GROUPING_ERROR: &str = "42803";
/// `42P20` — a window construct is used where one is not allowed (a window
/// function in WHERE/GROUP BY/HAVING, a nested window call), or a window
/// definition is itself invalid (a frame whose bounds are ordered backwards, a
/// duplicate `WINDOW` name). The window analogue of [`GROUPING_ERROR`].
pub const WINDOWING_ERROR: &str = "42P20";
/// `42P22` — no collation could be derived for an expression, because two
/// explicit `COLLATE` clauses conflict or two differently-collated inputs meet
/// with equal precedence.
pub const INDETERMINATE_COLLATION: &str = "42P22";
pub const UNDEFINED_TABLE: &str = "42P01";
pub const DUPLICATE_TABLE: &str = "42P07";
/// `42P06` — `CREATE SCHEMA` names a schema that already exists (without
/// `IF NOT EXISTS`).
pub const DUPLICATE_SCHEMA: &str = "42P06";
/// `3F000` — a reference to (or `DROP` of) a schema that does not exist.
pub const INVALID_SCHEMA_NAME: &str = "3F000";
/// `42939` — a name reserved for the system was used, e.g. a `pg_`-prefixed
/// schema name in `CREATE SCHEMA`.
pub const RESERVED_NAME: &str = "42939";
/// `42P16` — a relation definition is invalid (e.g. a `CREATE OR REPLACE VIEW`
/// that renames, drops, or retypes an existing view column).
pub const INVALID_TABLE_DEFINITION: &str = "42P16";
pub const INVALID_OBJECT_DEFINITION: &str = "42P17";
/// `54000` — an implementation limit was reached, e.g. a row too big to store
/// on one page.
pub const PROGRAM_LIMIT_EXCEEDED: &str = "54000";
/// `54001` — a statement nests too deeply to bind or execute safely (PG reports
/// this as "stack depth limit exceeded").
pub const STATEMENT_TOO_COMPLEX: &str = "54001";
pub const DEPENDENT_OBJECTS_STILL_EXIST: &str = "2BP01";
pub const PROTOCOL_VIOLATION: &str = "08P01";
/// `26000` — a Bind/Describe, or a SQL `EXECUTE`/`DEALLOCATE`, names a prepared
/// statement that does not exist.
pub const INVALID_SQL_STATEMENT_NAME: &str = "26000";
/// `42P05` — Parse or a SQL `PREPARE` names a prepared statement that already
/// exists.
pub const DUPLICATE_PREPARED_STATEMENT: &str = "42P05";
/// `34000` — an Execute/Describe, or a SQL `FETCH`/`MOVE`/`CLOSE`, names a
/// portal (cursor) that does not exist.
pub const INVALID_CURSOR_NAME: &str = "34000";
/// `42P03` — `DECLARE` names a cursor that is already open in this session.
pub const DUPLICATE_CURSOR: &str = "42P03";
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
/// `22004` — a NULL where the operation forbids one, e.g. a NULL element in
/// `ts_filter`'s weight array.
pub const NULL_VALUE_NOT_ALLOWED: &str = "22004";
/// `2200H` — a sequence hit its `MINVALUE`/`MAXVALUE` bound with `NO CYCLE`.
pub const SEQUENCE_GENERATOR_LIMIT_EXCEEDED: &str = "2200H";
/// Class 22 — data exception: a COPY data stream that does not match the
/// expected format (extra/missing columns, unterminated CSV quoting).
pub const BAD_COPY_FILE_FORMAT: &str = "22P04";
/// Class 57 — operator intervention: a client-issued CopyFail during COPY FROM.
pub const QUERY_CANCELED: &str = "57014";
/// Class 58 — system error: a WAL/data-file I/O failure (e.g. commit fsync).
pub const IO_ERROR: &str = "58030";
/// `58P01` — a file named by a statement could not be opened or read
/// (`COPY … FROM '<file>'`).
pub const UNDEFINED_FILE: &str = "58P01";
