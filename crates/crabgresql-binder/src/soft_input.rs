//! PostgreSQL's *soft* input path — `InputFunctionCallSafe` — behind
//! `pg_input_is_valid(value, type)` and `pg_input_error_info(value, type)`.
//!
//! Both take the target type as a *written type name*, so the work is the same
//! a cast does: parse the type spec, resolve it, run the type's input function,
//! then apply the typmod. Every one of those steps already exists for
//! `expr::CAST`; this module reuses them and reports the failure as a value
//! instead of raising it.
//!
//! The one place it must not reuse the cast path is the typmod: an explicit
//! cast truncates an over-long string, while the *input* function errors
//! (`22001`). That is the difference between `'abcde'::varchar(4)` yielding
//! `abcd` and `pg_input_is_valid('abcde', 'varchar(4)')` yielding false.
//!
//! Only built-in types resolve here — the binder holds no catalog, so a user
//! enum is indistinguishable from a name that denotes nothing. Both raise
//! `42704`, which is PostgreSQL's answer for the latter.

use crabgresql_parser::ast;
use crabgresql_pg_wire::sqlstate;
use crabgresql_types::{PgType, Value};

use crate::BindError;
use crate::expr::{checked_length_typmod, map_data_type, numeric_typmod, parse_unknown};

/// Why a soft input failed: the fields `pg_input_error_info` reports. This is
/// a [`BindError`] minus the cursor position, which has no meaning for a value
/// that never appeared in the query text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoftError {
    pub code: &'static str,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
}

impl From<BindError> for SoftError {
    fn from(e: BindError) -> Self {
        SoftError {
            code: e.code,
            message: e.message,
            detail: e.detail,
            hint: e.hint,
        }
    }
}

/// Run `type_spec`'s input function over `value` without raising.
///
/// The nesting distinguishes the two failure kinds PostgreSQL itself keeps
/// apart: a bad *value* is what these functions exist to report, while a bad
/// *type name* is an error the call still raises.
///
/// - `Ok(Ok(()))` — the value is valid input for the type.
/// - `Ok(Err(e))` — the value is not; `e` is what `pg_input_error_info` returns.
/// - `Err(_)` — the type spec is unparsable (`42601`), names nothing this build
///   knows (`42704`), or is a `reg*` type whose input needs the runtime catalog
///   (`0A000`).
pub fn soft_input(type_spec: &str, value: &str) -> Result<Result<(), SoftError>, BindError> {
    let data_type = crabgresql_parser::parse_data_type(type_spec)
        .map_err(|e| BindError::new(sqlstate::SYNTAX_ERROR, e.to_string()))?;
    let ty = map_data_type(&data_type).map_err(|_| {
        BindError::new(
            sqlstate::UNDEFINED_OBJECT,
            format!("type \"{}\" does not exist", type_spec.trim()),
        )
    })?;
    // A `reg*` name resolves against the catalog, which the binder does not
    // hold; `parse_unknown` would answer `XX000`. Say so honestly instead.
    if let PgType::Reg(_) = ty {
        return Err(BindError::feature_not_supported(format!(
            "pg_input_is_valid on type \"{}\" is not supported yet",
            type_spec.trim()
        )));
    }
    Ok(soft(&data_type, ty, value))
}

/// The fallible half: input function, then typmod. Split out so every early
/// `?` here lands in the *soft* result rather than the hard one.
fn soft(data_type: &ast::DataType, ty: PgType, value: &str) -> Result<(), SoftError> {
    let parsed = parse_unknown(value, ty)?;
    apply_input_typmod(&parsed, ty, data_type)?;
    Ok(())
}

/// Apply the type modifier the way an input function does — in *assignment*
/// terms, `explicit = false`, so an over-long value errors instead of being
/// truncated. Trailing blanks are still absorbed, which is why
/// `pg_input_is_valid('abcd  ', 'char(4)')` is true while `'abcde'` is not.
fn apply_input_typmod(
    value: &Value,
    ty: PgType,
    data_type: &ast::DataType,
) -> Result<(), BindError> {
    let text_err = |e: crabgresql_types::text::TextError| BindError::new(e.sqlstate, e.message);
    match ty {
        PgType::Numeric => {
            let (Some((precision, scale)), Value::Numeric(n)) = (numeric_typmod(data_type), value)
            else {
                return Ok(());
            };
            n.apply_typmod(precision, scale)
                .map_err(|e| BindError::new(e.sqlstate, e.message).with_detail(e.detail))?;
        }
        PgType::Varchar | PgType::Bpchar => {
            let Value::Text(s) = value else {
                return Ok(());
            };
            // A bare `varchar` is unlimited; a bare `char` is `char(1)`, which
            // `checked_length_typmod` already supplies.
            let Some(n) = checked_length_typmod(data_type)? else {
                return Ok(());
            };
            if ty == PgType::Varchar {
                crabgresql_types::text::varchar_input(s, n, false).map_err(text_err)?;
            } else {
                crabgresql_types::text::bpchar_input(s, n, false).map_err(text_err)?;
            }
        }
        PgType::Bit | PgType::Varbit => {
            let Value::Bit { len, data } = value else {
                return Ok(());
            };
            let Some(n) = checked_length_typmod(data_type)? else {
                return Ok(());
            };
            crabgresql_types::bit::coerce(*len, data, n, ty == PgType::Varbit, false)
                .map_err(|e| BindError::new(e.sqlstate, e.message))?;
        }
        // `name` truncates to 63 characters and never fails; no other type
        // carries a modifier that can reject a value.
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The soft error for `value` as `type_spec`, or `None` if it is valid.
    fn bad(type_spec: &str, value: &str) -> Option<SoftError> {
        soft_input(type_spec, value)
            .expect("type spec resolves")
            .err()
    }

    /// `"SQLSTATE: message"` — one string, so the expectations below read as
    /// the single line `pg_input_error_info` would report.
    fn report(type_spec: &str, value: &str) -> String {
        let e = bad(type_spec, value).expect("expected a soft failure");
        format!("{}: {}", e.code, e.message)
    }

    #[test]
    fn boolean_rejects_junk() {
        assert!(bad("bool", "true").is_none());
        assert_eq!(
            report("bool", "junk"),
            "22P02: invalid input syntax for type boolean: \"junk\""
        );
    }

    #[test]
    fn char_length_absorbs_trailing_blanks_but_not_characters() {
        assert!(bad("char(4)", "abcd  ").is_none());
        assert_eq!(
            report("char(4)", "abcde"),
            "22001: value too long for type character(4)"
        );
    }

    #[test]
    fn varchar_length_absorbs_trailing_blanks_but_not_characters() {
        assert!(bad("varchar(4)", "abcd  ").is_none());
        assert_eq!(
            report("varchar(4)", "abcde"),
            "22001: value too long for type character varying(4)"
        );
    }

    #[test]
    fn int2_separates_malformed_from_out_of_range() {
        assert!(bad("int2", "34").is_none());
        assert_eq!(
            report("int2", "asdf"),
            "22P02: invalid input syntax for type smallint: \"asdf\""
        );
        assert_eq!(
            report("int2", "50000"),
            "22003: value \"50000\" is out of range for type smallint"
        );
    }

    #[test]
    fn oid_separates_malformed_from_out_of_range() {
        assert!(bad("oid", "1234").is_none());
        assert_eq!(
            report("oid", "01XYZ"),
            "22P02: invalid input syntax for type oid: \"01XYZ\""
        );
        assert_eq!(
            report("oid", "9999999999"),
            "22003: value \"9999999999\" is out of range for type oid"
        );
    }

    #[test]
    fn text_and_name_accept_anything() {
        assert!(bad("text", "anything at all").is_none());
        assert!(bad("name", &"x".repeat(200)).is_none());
    }

    #[test]
    fn numeric_typmod_overflow_is_soft() {
        assert!(bad("numeric(5,2)", "123.45").is_none());
        assert!(report("numeric(5,2)", "123456").starts_with("22003:"));
    }

    #[test]
    fn json_failure_carries_its_detail() {
        let e = bad("json", "{bad").expect("expected a soft failure");
        assert_eq!(e.code, "22P02");
        assert!(e.detail.is_some(), "json errors carry a DETAIL line");
    }

    #[test]
    fn unknown_type_name_is_a_hard_error() {
        let e = soft_input("nosuchtype", "x").expect_err("expected a hard error");
        assert_eq!(e.code, "42704");
        assert_eq!(e.message, "type \"nosuchtype\" does not exist");
    }

    #[test]
    fn unparsable_type_spec_is_a_hard_error() {
        let e = soft_input("int4(", "x").expect_err("expected a hard error");
        assert_eq!(e.code, "42601");
    }

    #[test]
    fn qualified_builtin_resolves() {
        assert!(bad("pg_catalog.int4", "42").is_none());
        assert!(report("pg_catalog.int4", "x").starts_with("22P02:"));
    }
}
