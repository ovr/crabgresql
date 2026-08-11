//! Exception condition names.
//!
//! `RAISE division_by_zero` and `RAISE ... USING ERRCODE = 'unique_violation'`
//! both name an error by condition rather than by SQLSTATE. This table holds
//! the conditions a routine body is plausibly going to raise deliberately, plus
//! the message PostgreSQL uses when a condition name is given with no message
//! of its own.
//!
//! An unlisted name is an error rather than a silent fallback: quietly
//! inventing a SQLSTATE would make a typo look like a working `RAISE`.
//!
//! TODO: carry every condition name of PostgreSQL's error-code appendix — a
//! real but unlisted name such as `lock_not_available` raises `unrecognized
//! exception condition` here and works in PostgreSQL.

/// `(sqlstate, default message)` for a condition name, or `None` if unknown.
/// Comparison is case-insensitive, as PostgreSQL's is.
pub fn lookup(name: &str) -> Option<(&'static str, &'static str)> {
    CONDITIONS
        .iter()
        .find(|(n, _, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, code, message)| (*code, *message))
}

/// `(condition name, SQLSTATE, default message)`.
const CONDITIONS: &[(&str, &str, &str)] = &[
    ("raise_exception", "P0001", "raise_exception"),
    ("no_data_found", "P0002", "no_data_found"),
    ("too_many_rows", "P0003", "too_many_rows"),
    ("assert_failure", "P0004", "assert_failure"),
    // Class 22 — data exception
    ("data_exception", "22000", "data_exception"),
    ("array_subscript_error", "2202E", "array_subscript_error"),
    ("division_by_zero", "22012", "division_by_zero"),
    (
        "invalid_parameter_value",
        "22023",
        "invalid_parameter_value",
    ),
    (
        "invalid_text_representation",
        "22P02",
        "invalid_text_representation",
    ),
    ("null_value_not_allowed", "22004", "null_value_not_allowed"),
    (
        "numeric_value_out_of_range",
        "22003",
        "numeric_value_out_of_range",
    ),
    (
        "string_data_right_truncation",
        "22001",
        "string_data_right_truncation",
    ),
    ("substring_error", "22011", "substring_error"),
    // Class 23 — integrity constraint violation
    (
        "integrity_constraint_violation",
        "23000",
        "integrity_constraint_violation",
    ),
    ("check_violation", "23514", "check_violation"),
    ("foreign_key_violation", "23503", "foreign_key_violation"),
    ("not_null_violation", "23502", "not_null_violation"),
    ("unique_violation", "23505", "unique_violation"),
    // Class 25 — invalid transaction state
    (
        "read_only_sql_transaction",
        "25006",
        "read_only_sql_transaction",
    ),
    // Class 42 — syntax error or access rule violation
    (
        "syntax_error_or_access_rule_violation",
        "42000",
        "syntax_error_or_access_rule_violation",
    ),
    ("duplicate_column", "42701", "duplicate_column"),
    ("duplicate_function", "42723", "duplicate_function"),
    ("duplicate_object", "42710", "duplicate_object"),
    ("duplicate_table", "42P07", "duplicate_table"),
    ("insufficient_privilege", "42501", "insufficient_privilege"),
    ("syntax_error", "42601", "syntax_error"),
    ("undefined_column", "42703", "undefined_column"),
    ("undefined_function", "42883", "undefined_function"),
    ("undefined_object", "42704", "undefined_object"),
    ("undefined_table", "42P01", "undefined_table"),
    // Class 0A / 21 / 54 / XX
    ("feature_not_supported", "0A000", "feature_not_supported"),
    ("cardinality_violation", "21000", "cardinality_violation"),
    ("program_limit_exceeded", "54000", "program_limit_exceeded"),
    ("internal_error", "XX000", "internal_error"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condition_names_are_matched_case_insensitively() {
        assert_eq!(lookup("division_by_zero").map(|c| c.0), Some("22012"));
        assert_eq!(lookup("DIVISION_BY_ZERO").map(|c| c.0), Some("22012"));
    }

    /// A typo must not quietly become a working RAISE.
    #[test]
    fn an_unknown_condition_is_not_invented() {
        assert_eq!(lookup("divison_by_zero"), None);
    }

    /// The table is the compatibility surface, so every entry must be a real
    /// 5-character SQLSTATE.
    #[test]
    fn every_sqlstate_is_five_characters() {
        for (name, code, _) in CONDITIONS {
            assert_eq!(code.len(), 5, "{name} has SQLSTATE {code:?}");
            assert!(
                code.chars().all(|c| c.is_ascii_alphanumeric()),
                "{name} has SQLSTATE {code:?}"
            );
        }
    }
}
