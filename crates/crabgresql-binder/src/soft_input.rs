//! The *soft* input path behind `pg_input_is_valid(value, type)` and
//! `pg_input_error_info(value, type)`: run a type's input function and report
//! the failure as a value instead of raising it.
//!
//! Both take the target type as a *written type name*, so the work is the same
//! a cast does — parse the type spec, resolve it, run the input function, apply
//! the typmod — and every step already exists for `expr::CAST`.
//!
//! Two boundaries have to hold, and both are observable:
//!
//! - **Typmod semantics.** An explicit cast truncates an over-long string while
//!   the *input* function errors (`22001`), so `'abcde'::varchar(4)` is `abcd`
//!   but `pg_input_is_valid('abcde', 'varchar(4)')` is false.
//! - **Hard vs soft.** Only a bad *value* is an answer. Anything describing the
//!   *type spec* still raises, as PostgreSQL does: an unparsable name (`42601`),
//!   one that denotes nothing (`42704`), and a modifier `typmodin` rejects
//!   (`22023` — `varchar(0)`, `numeric(1001)`). [`TypeSpec::resolve`] settles all
//!   of those before [`TypeSpec::check`] ever looks at the value.
//!
//! The binder holds no catalog, so a user enum is indistinguishable from a name
//! that denotes nothing and both raise `42704`; a type this build parses but
//! does not model keeps its own `0A000` rather than being called nonexistent. A
//! `reg*` name needs a catalog outright, so [`TypeSpec`] is public for the
//! executor to finish that one itself.

use std::rc::Rc;

use crabgresql_parser::ast;
use crabgresql_pg_wire::sqlstate;
use crabgresql_types::{PgType, Value};

use crate::BindError;
use crate::expr::{
    builtin_custom_type, checked_length_typmod, checked_numeric_typmod, length_typmod,
    map_data_type, normalize_ident, numeric_typmod, parse_unknown,
};

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
/// - `Err(_)` — the type spec is unparsable (`42601`), names nothing (`42704`),
///   names a type this build does not model yet (`0A000`), or carries a
///   modifier `typmodin` would reject (`22023`).
pub fn soft_input(type_spec: &str, value: &str) -> Result<Result<(), SoftError>, BindError> {
    let spec = TypeSpec::resolve(type_spec)?;
    // A `reg*` input function is a catalog lookup, which this entry point has
    // no handle for. Callers that do (the executor) go through `TypeSpec`.
    if let PgType::Reg(_) = spec.ty {
        return Err(BindError::feature_not_supported(
            "soft input for a reg* type needs a catalog",
        ));
    }
    Ok(spec.check(value))
}

/// A written type name, resolved. Separate from [`soft_input`] so a caller that
/// holds a catalog can inspect [`TypeSpec::ty`] and take over the types the
/// binder cannot resolve alone — a `reg*` name, whose input function is a
/// catalog lookup.
pub struct TypeSpec {
    pub ty: PgType,
    /// The written form, kept because the modifier lives here rather than in
    /// `ty` (`PgType::Varchar` alone cannot say `varchar(4)`).
    data_type: ast::DataType,
}

/// How many resolved type specs to keep per thread. `pg_input_is_valid` is a
/// per-row scalar whose type argument is a constant at every realistic call
/// site, so without this every row re-runs the SQL type-name parser — measured
/// at ~250ns, an order of magnitude more than the rest of the call. The bound
/// caps per-thread memory; the exact depth is not observable.
const SPEC_CACHE_MAX: usize = 16;

thread_local! {
    /// Most-recently-used first. Only successful resolutions are cached: a
    /// failure is a raise, so it happens once per statement, not once per row.
    static SPEC_CACHE: std::cell::RefCell<Vec<(String, Rc<TypeSpec>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

impl TypeSpec {
    /// Resolve a written type name the way PostgreSQL does before it ever looks
    /// at the value: the name, then `typmodin`. Every failure is a hard error —
    /// it describes the *type spec*, so it cannot be reported as a bad value.
    ///
    /// Memoized per thread on the written spelling — see [`SPEC_CACHE_MAX`].
    pub fn resolve(type_spec: &str) -> Result<Rc<Self>, BindError> {
        SPEC_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            if let Some(idx) = cache.iter().position(|(s, _)| s == type_spec) {
                // Promote to most-recently-used; an already-hot spec (the
                // per-row case) needs no shuffling at all.
                if idx != 0 {
                    cache[..=idx].rotate_right(1);
                }
            } else {
                let spec = Rc::new(Self::resolve_uncached(type_spec)?);
                cache.insert(0, (type_spec.to_string(), spec));
                cache.truncate(SPEC_CACHE_MAX);
            }
            Ok(Rc::clone(&cache[0].1))
        })
    }

    fn resolve_uncached(type_spec: &str) -> Result<Self, BindError> {
        let data_type = crabgresql_parser::parse_data_type(type_spec)
            .map_err(|e| BindError::new(sqlstate::SYNTAX_ERROR, e.to_string()))?;
        let ty = resolve_name(&data_type)?;
        validate_typmod(&data_type, ty)?;
        Ok(TypeSpec { ty, data_type })
    }

    /// Run the type's input function over `value` without raising.
    pub fn check(&self, value: &str) -> Result<(), SoftError> {
        soft(&self.data_type, self.ty, value)
    }
}

/// The `PgType` a written name denotes, keeping PostgreSQL's two distinct
/// failures apart: a bareword that names no built-in "does not exist" (42704),
/// while a name this build parses but does not model keeps `map_data_type`'s
/// own "is not supported yet" (0A000) — so a real PostgreSQL type is never
/// reported as nonexistent, and the feature-gap signal survives.
fn resolve_name(data_type: &ast::DataType) -> Result<PgType, BindError> {
    map_data_type(data_type).map_err(|unsupported| {
        let ast::DataType::Custom(obj, _) = data_type else {
            return unsupported;
        };
        if builtin_custom_type(obj).is_some() {
            return unsupported;
        }
        // PostgreSQL reports the *folded* name: an unquoted identifier
        // downcases, a quoted one keeps its spelling.
        let name = obj
            .0
            .iter()
            .filter_map(|p| p.as_ident().map(normalize_ident))
            .collect::<Vec<_>>()
            .join(".");
        BindError::new(
            sqlstate::UNDEFINED_OBJECT,
            format!("type \"{name}\" does not exist"),
        )
    })
}

/// Reject a modifier PostgreSQL's `typmodin` would reject (`varchar(0)`,
/// `numeric(1001)`), recursing into an array's element type. This runs in the
/// hard channel; the soft path below reads the modifier back with the
/// *unchecked* readers, so a 22023 can never reach a `SoftError`.
fn validate_typmod(data_type: &ast::DataType, ty: PgType) -> Result<(), BindError> {
    checked_length_typmod(data_type)?;
    checked_numeric_typmod(data_type)?;
    if let Some((elem_ty, elem_dt)) = array_element(data_type, ty) {
        validate_typmod(elem_dt, elem_ty)?;
    }
    Ok(())
}

/// The fallible half: input function, then typmod. Split out so every early
/// `?` here lands in the *soft* result rather than the hard one.
fn soft(data_type: &ast::DataType, ty: PgType, value: &str) -> Result<(), SoftError> {
    let parsed = parse_unknown(value, ty)?;
    apply_input_typmod(&parsed, ty, data_type)?;
    Ok(())
}

/// An array type's `(element type, element type name)`, or `None` when
/// `data_type` is not a written array spec. Both halves are needed to carry a
/// modifier like `varchar(4)[]` down to the elements.
fn array_element(data_type: &ast::DataType, ty: PgType) -> Option<(PgType, &ast::DataType)> {
    let PgType::Array(elem_oid) = ty else {
        return None;
    };
    let ast::DataType::Array(def) = data_type else {
        return None;
    };
    let inner = match def {
        ast::ArrayElemTypeDef::SquareBracket(inner, _)
        | ast::ArrayElemTypeDef::AngleBracket(inner)
        | ast::ArrayElemTypeDef::Parenthesis(inner) => inner,
        ast::ArrayElemTypeDef::None => return None,
    };
    Some((PgType::from_oid(elem_oid)?, inner))
}

/// Apply the type modifier the way an input function does — in *assignment*
/// terms, `explicit = false`, so an over-long value errors instead of being
/// truncated. Trailing blanks are still absorbed, which is why
/// `pg_input_is_valid('abcd  ', 'char(4)')` is true while `'abcde'` is not.
///
/// Reads the modifier with the *unchecked* [`length_typmod`]/[`numeric_typmod`]
/// — [`validate_typmod`] has already rejected an unusable one in the hard
/// channel, so nothing here can report a bad type spec as a bad value.
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
            // `length_typmod` already supplies.
            let Some(n) = length_typmod(data_type) else {
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
            let Some(n) = length_typmod(data_type) else {
                return Ok(());
            };
            crabgresql_types::bit::coerce(*len, data, n, ty == PgType::Varbit, false)
                .map_err(|e| BindError::new(e.sqlstate, e.message))?;
        }
        // `array_in` builds the elements without a modifier, so `varchar(4)[]`
        // enforces its element length here — as PostgreSQL's `array_in` does,
        // which passes the element typmod down to each element's input call.
        PgType::Array(_) => {
            let (Value::Array { elems, .. }, Some((elem_ty, elem_dt))) =
                (value, array_element(data_type, ty))
            else {
                return Ok(());
            };
            for elem in elems {
                apply_input_typmod(elem, elem_ty, elem_dt)?;
            }
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

    /// `"SQLSTATE: message"` for a *hard* error — an unusable type spec.
    fn hard(type_spec: &str) -> String {
        let e = soft_input(type_spec, "irrelevant").expect_err("expected a hard error");
        format!("{}: {}", e.code, e.message)
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
        assert_eq!(
            hard("nosuchtype"),
            "42704: type \"nosuchtype\" does not exist"
        );
    }

    /// PostgreSQL reports the *folded* identifier: an unquoted name downcases,
    /// a quoted one keeps its spelling.
    #[test]
    fn unknown_type_name_is_reported_folded() {
        assert_eq!(
            hard("NoSuchType"),
            "42704: type \"nosuchtype\" does not exist"
        );
        assert_eq!(
            hard("\"NoSuchType\""),
            "42704: type \"NoSuchType\" does not exist"
        );
    }

    /// A spec this build recognizes but does not model keeps `map_data_type`'s
    /// own 0A000, rather than being relabelled as a type that does not exist —
    /// so the feature-gap signal survives.
    ///
    /// A bare name the parser hands over as `Custom` (`box`, `xml`) cannot be
    /// told apart from a typo without a catalog, and stays 42704.
    #[test]
    fn unmodelled_spec_keeps_its_not_supported_error() {
        assert_eq!(
            hard("int4[][]"),
            "0A000: type \"INT4[][]\" is not supported yet"
        );
        assert_eq!(hard("xml[]"), "0A000: type \"xml\" is not supported yet");
    }

    #[test]
    fn unparsable_type_spec_is_a_hard_error() {
        assert!(hard("int4(").starts_with("42601:"));
    }

    /// A modifier `typmodin` would reject describes the type spec, not the
    /// value, so it raises rather than answering false.
    #[test]
    fn unusable_type_modifier_is_a_hard_error() {
        assert_eq!(
            hard("varchar(0)"),
            "22023: length for type varchar must be at least 1"
        );
        assert_eq!(
            hard("char(0)"),
            "22023: length for type char must be at least 1"
        );
        assert_eq!(
            hard("bit(0)"),
            "22023: length for type bit must be at least 1"
        );
        assert_eq!(
            hard("numeric(0)"),
            "22023: NUMERIC precision 0 must be between 1 and 1000"
        );
        assert_eq!(
            hard("numeric(1001)"),
            "22023: NUMERIC precision 1001 must be between 1 and 1000"
        );
        assert_eq!(
            hard("numeric(5,1001)"),
            "22023: NUMERIC scale 1001 must be between -1000 and 1000"
        );
    }

    #[test]
    fn qualified_builtin_resolves() {
        assert!(bad("pg_catalog.int4", "42").is_none());
        assert!(report("pg_catalog.int4", "x").starts_with("22P02:"));
    }

    /// `bpchar(4)` and `pg_catalog.varchar(4)` name the same types as
    /// `char(4)` / `varchar(4)`; the parser hands their modifier over as raw
    /// token text rather than in a typed field.
    #[test]
    fn aliased_and_qualified_names_carry_their_modifier() {
        assert!(bad("bpchar(4)", "abcd  ").is_none());
        assert_eq!(
            report("bpchar(4)", "abcde"),
            "22001: value too long for type character(4)"
        );
        assert_eq!(
            report("pg_catalog.varchar(4)", "abcde"),
            "22001: value too long for type character varying(4)"
        );
        assert!(report("pg_catalog.numeric(5,2)", "123456").starts_with("22003:"));
        // A modifier-less `bpchar` is unlimited, unlike a bare `char`.
        assert!(bad("bpchar", "abcde").is_none());
    }

    /// `array_in` builds elements without a modifier, so the element typmod is
    /// applied per element afterwards.
    #[test]
    fn array_element_modifier_is_enforced() {
        assert!(bad("varchar(4)[]", "{abcd,ab}").is_none());
        assert_eq!(
            report("varchar(4)[]", "{abcde}"),
            "22001: value too long for type character varying(4)"
        );
        assert!(report("numeric(5,2)[]", "{123456}").starts_with("22003:"));
        // The element's modifier is validated in the hard channel too.
        assert_eq!(
            hard("varchar(0)[]"),
            "22023: length for type varchar must be at least 1"
        );
    }

    /// A `reg*` input function is a catalog lookup, so this catalog-free entry
    /// point declines rather than guessing; the executor drives it instead.
    #[test]
    fn reg_types_need_a_catalog() {
        assert_eq!(
            hard("regclass"),
            "0A000: soft input for a reg* type needs a catalog"
        );
    }
}
