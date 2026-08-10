//! Literal binding: values, placeholders, numbers and bit strings, plus the
//! datetime corner where a literal meets the session — intervals, `EXTRACT`,
//! `AT TIME ZONE` and `AT LOCAL`.

use crabgresql_parser::{Span, ast};
use crabgresql_pg_wire::sqlstate;
use crabgresql_types::numeric::ParseError;
use crabgresql_types::{Numeric, PgType, Value, interval};

use crate::BindError;
use crate::functions::ScalarFn;

use super::bind::{bind_expr, unsupported_expr};
use super::bound::BoundExpr;
use super::coerce::{coerce_expr, resolve_unknown};
use super::datatype::apply_interval_typmod;
use super::scope::{Binding, Scope};

pub(super) fn bind_value(value: &ast::ValueWithSpan, scope: &Scope) -> Result<Binding, BindError> {
    // Every character string constant carries plain text by the time it reaches
    // the binder — the tokenizer has already decoded `E'…'`'s backslash escapes
    // and `U&'…'`'s code points. They stay `Unknown` (rather than a typed `text`
    // const) so `E'\t'::name` and context-driven typing keep working.
    if let Some(s) = value.value.as_pg_string() {
        return Ok(Binding::Unknown {
            lit: Some(s.to_string()),
            span: value.span,
            param: None,
        });
    }
    match &value.value {
        ast::Value::Placeholder(s) => bind_placeholder(s, value.span, scope),
        ast::Value::Number(n, _) => parse_number(n).map(Binding::Typed),
        ast::Value::Boolean(b) => Ok(Binding::Typed(BoundExpr::Const {
            value: Value::Bool(*b),
            ty: PgType::Bool,
        })),
        ast::Value::Null => Ok(Binding::Unknown {
            lit: None,
            span: value.span,
            param: None,
        }),
        // `B'...'` is a `bit(n)` literal (n binary digits); `X'...'` a `bit(4n)`
        // literal (4 bits per hex digit). PG's `bit_in` rejects a bad digit with
        // a data exception (22P02) naming the offender, at the literal's cursor.
        ast::Value::SingleQuotedByteStringLiteral(s) => {
            bind_bit_literal(crabgresql_types::bit::from_binary(s), value.span)
        }
        ast::Value::HexStringLiteral(s) => {
            bind_bit_literal(crabgresql_types::bit::from_hex(s), value.span)
        }
        other => Err(BindError::feature_not_supported(format!(
            "literal is not supported yet: {other}"
        ))),
    }
}

/// Bind a `$n` placeholder. The trailing number is the 1-based parameter; PG
/// rejects `$0`/non-numeric with a syntax error. A placeholder is registered in
/// the shared context (an error there when the simple protocol forbids
/// parameters). If the parameter's type is already known — declared by the
/// client or inferred at an earlier site — it binds straight to a typed
/// [`BoundExpr::Param`]; otherwise it stays an `Unknown` carrying the param
/// marker, to be typed by the first context that resolves it.
fn bind_placeholder(s: &str, span: Span, scope: &Scope) -> Result<Binding, BindError> {
    let n1: usize = s
        .strip_prefix('$')
        .and_then(|d| d.parse().ok())
        .filter(|&n| n > 0)
        .ok_or_else(|| BindError::syntax(format!("invalid parameter number: {s}")))?;
    let index = scope.params().borrow_mut().reference(n1)?;
    let known = scope.params().borrow().slot_type(index);
    if let Some(ty) = known {
        return Ok(Binding::Typed(BoundExpr::Param { index, ty }));
    }
    Ok(Binding::Unknown {
        lit: None,
        span,
        param: Some((index, scope.params().clone())),
    })
}

/// Build a `bit` constant from a parsed `B'...'`/`X'...'` literal, attaching the
/// literal's cursor position to a bad-digit error (so it renders `LINE n: ^`).
fn bind_bit_literal(
    parsed: Result<(u32, Vec<u8>), crabgresql_types::bit::BitError>,
    span: Span,
) -> Result<Binding, BindError> {
    let (len, data) = parsed.map_err(|e| BindError::new(e.sqlstate, e.message).at(span))?;
    Ok(Binding::Typed(BoundExpr::Const {
        value: Value::Bit { len, data },
        ty: PgType::Bit,
    }))
}

/// Integer literals become int4 when they fit, int8 otherwise. Literals with a
/// decimal point or exponent bind as `numeric`, as PG does — a numeric constant
/// keeps its exact value and display scale.
///
/// A whole-number literal goes through the same acceptor as the `int4 '…'` input
/// function, so the hex/octal/binary spellings (`0x1F`, `0o17`, `0b11`) and the
/// `_` digit separators mean the same thing written bare as they do quoted.
pub(super) fn parse_number(n: &str) -> Result<BoundExpr, BindError> {
    use crabgresql_types::intlit::{ScanError, scan_int_literal, scan_int_literal_decimal};
    // A leading `-` never reaches here (the parser builds unary minus), but the
    // acceptor reports the sign anyway, so honour it rather than assume.
    let signed = |negative: bool, m: u128| -> Option<i128> {
        i128::try_from(m)
            .ok()
            .map(|m| if negative { -m } else { m })
    };
    match scan_int_literal(n) {
        Ok((negative, magnitude)) => {
            if let Some(v) = signed(negative, magnitude).and_then(|v| i32::try_from(v).ok()) {
                return Ok(BoundExpr::Const {
                    value: Value::Int4(v),
                    ty: PgType::Int4,
                });
            }
            if let Some(v) = signed(negative, magnitude).and_then(|v| i64::try_from(v).ok()) {
                return Ok(BoundExpr::Const {
                    value: Value::Int8(v),
                    ty: PgType::Int8,
                });
            }
            // Past int8, PG keeps the value as `numeric` — `0x8000000000000000`
            // is 9223372036854775808, not an overflow. Render the magnitude in
            // decimal so a non-decimal spelling reaches `Numeric::parse` in a
            // form it reads.
            let decimal = format!("{}{magnitude}", if negative { "-" } else { "" });
            numeric_const(&decimal, n)
        }
        // Well-formed but past `u128`. PostgreSQL keeps widening into `numeric`
        // with no ceiling, so re-fold the same digit run without one; only the
        // non-decimal spellings actually need the conversion, but going through
        // one function keeps the two paths from disagreeing.
        Err(ScanError::Range) => match scan_int_literal_decimal(n) {
            Ok((negative, digits)) => {
                let decimal = format!("{}{digits}", if negative { "-" } else { "" });
                numeric_const(&decimal, n)
            }
            // Unreachable in practice — the two folds share one grammar, so a
            // literal that got as far as `Range` cannot be malformed here. Fall
            // back to the ordinary error rather than assert.
            Err(_) => numeric_const(n, n),
        },
        // Not a whole number: a decimal point or exponent, where the separators
        // (if any) still have to come out before `Numeric::parse` sees the text.
        Err(ScanError::Syntax) => {
            let stripped;
            let text = if n.contains('_') {
                stripped = n.replace('_', "");
                stripped.as_str()
            } else {
                n
            };
            numeric_const(text, n)
        }
    }
}

/// The value of a whole-number literal token, or `None` when the text is not one
/// (a fraction, an exponent, or a magnitude past `i128`).
///
/// The parser keeps a numeric literal as written, so every site that re-reads
/// that text has to decode it — and they must all agree, or `0x2` means one
/// thing in an expression and another as an `ORDER BY` ordinal. This is the one
/// decoder for those sites; the expression path uses [`parse_number`], which is
/// built on the same acceptor.
pub fn literal_int(n: &str) -> Option<i128> {
    let (negative, magnitude) = crabgresql_types::intlit::scan_int_literal(n).ok()?;
    let v = i128::try_from(magnitude).ok()?;
    Some(if negative { -v } else { v })
}

/// Bind a `numeric` constant from `text`, reporting an unreadable one against
/// `original` — the literal as it was written, which may differ once digit
/// separators have been stripped or a non-decimal spelling converted.
fn numeric_const(text: &str, original: &str) -> Result<BoundExpr, BindError> {
    match Numeric::parse(text) {
        Ok(value) => Ok(BoundExpr::Const {
            value: Value::Numeric(value),
            ty: PgType::Numeric,
        }),
        Err(ParseError::Overflow) => Err(BindError::new(
            sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
            "value overflows numeric format",
        )),
        Err(ParseError::Syntax) => Err(BindError::feature_not_supported(format!(
            "numeric literal \"{original}\" is not supported yet"
        ))),
    }
}

/// `interval '...'` (with an optional SQL-standard field qualifier). The literal
/// string is parsed by `interval_in`, then the qualifier is applied as the type
/// modifier it is — the same [`interval_typmod`] encoding a column or a cast
/// carries, so all three masks agree.
///
/// The qualifier's two ends do different jobs:
///
/// * the **leading** field is the default unit for a bare number;
/// * the whole **range** becomes the modifier, and
///   [`crabgresql_types::interval::apply_typmod`] drops everything below its
///   lowest field, so `INTERVAL '1 day 2 hours' DAY` is one day.
///
/// Divergence: in PostgreSQL the qualifier also *steers the parse* of a
/// punctuated literal — `interval '1 2:03' day to second` is `1 day 02:03:00`
/// while `interval '1 2:03' hour to second` reads the same text as `mm:ss`. Here
/// the qualifier only picks the default unit and then masks the result, so those
/// two spellings still agree. That lives inside `interval_in`, not in the type
/// modifier, and is why `interval` is not yet an upstream must-pass test.
pub(super) fn bind_interval(node: &ast::Interval) -> Result<Binding, BindError> {
    let (s, span) = match &*node.value {
        ast::Expr::Value(v) => match v.value.as_pg_string() {
            Some(s) => (s.to_string(), v.span),
            None => {
                return Err(BindError::syntax(format!(
                    "invalid interval literal: {}",
                    v.value
                )));
            }
        },
        other => return Err(unsupported_expr(other)),
    };
    // Which end of a range supplies the default unit depends on the literal's
    // shape, as the SQL forms do. A number standing alone is the range's *fine*
    // end (`INTERVAL '1' YEAR TO MONTH` is one month, `INTERVAL '5' DAY TO HOUR`
    // five hours); once there is a unit word or a time part the form is
    // `D HH:MM:SS`, and the leading integer is the *coarse* end
    // (`INTERVAL '3 4:05:06' DAY TO SECOND` is three days, not three seconds).
    let leading = node.leading_field.as_ref();
    let default = if is_bare_number(&s) {
        node.last_field.as_ref().or(leading)
    } else {
        leading
    };
    let unit = default.map_or(interval::Unit::Second, datetime_field_to_unit);
    // Parse now either way: under `postgres` style the answer *is* the value,
    // and under `sql_standard` it is still the right validity check — the style
    // can only change signs, never whether the literal parses — so the syntax
    // error keeps its position even for a literal whose value must be deferred.
    let iv = interval::parse_with_default(&s, unit)
        .map_err(|e| BindError::new(e.sqlstate, e.message).at(span))?;
    let typmod = interval_literal_typmod(node);

    // A literal whose meaning the session style can change cannot be folded
    // here: the binder has no session, and re-binding on every Execute means a
    // bind-time answer would be wrong for whichever session ran the statement.
    if crabgresql_types::interval::style_sensitive(&s) {
        // Mask now as well, and throw the result away. The style can only flip
        // signs, never magnitudes, so whether the modifier fits is the same
        // question under either reading — and asking it here is what keeps the
        // `LINE n: … ^` cursor on a literal that has to be evaluated later.
        if let Some(typmod) = typmod {
            interval::apply_typmod(iv, typmod)
                .map_err(|e| BindError::new(e.sqlstate, e.message).at(span))?;
        }
        let call = BoundExpr::FuncCall {
            func: ScalarFn::IntervalIn,
            ret: PgType::Interval,
            args: vec![
                BoundExpr::Const {
                    value: Value::Text(s),
                    ty: PgType::Text,
                },
                BoundExpr::Const {
                    value: Value::Int4(unit.as_code()),
                    ty: PgType::Int4,
                },
            ],
        };
        return Ok(Binding::Typed(match typmod {
            Some(typmod) => apply_interval_typmod(call, typmod)?,
            None => call,
        }));
    }

    // The qualifier masks the result, through the same modifier a column or a
    // cast would carry. No qualifier at all leaves the literal's own units to
    // speak for themselves.
    let iv = match typmod {
        Some(typmod) => interval::apply_typmod(iv, typmod)
            .map_err(|e| BindError::new(e.sqlstate, e.message).at(span))?,
        None => iv,
    };
    Ok(Binding::Typed(BoundExpr::Const {
        value: Value::Interval(iv),
        ty: PgType::Interval,
    }))
}

/// The type modifier an `INTERVAL '...' <qualifier>` literal's qualifier
/// declares, in the same encoding [`interval_typmod`] builds for a column or a
/// cast. `None` when the literal carries no qualifier at all.
///
/// The range runs from the leading field down to the trailing one, so a bare
/// `MONTH` admits only months while `DAY TO SECOND` admits four fields. It has to
/// be a combination [`crabgresql_types::interval::range_name`] names, or
/// `apply_typmod` treats it as no modifier and masks nothing.
fn interval_literal_typmod(node: &ast::Interval) -> Option<i32> {
    use crabgresql_types::interval as iv;

    // The ladder of fields a qualifier can name, coarse to fine. `WEEK` is not
    // an SQL qualifier, so it has no bit and falls out below.
    const LADDER: [(interval::Unit, u16); 6] = [
        (interval::Unit::Year, iv::MASK_YEAR),
        (interval::Unit::Month, iv::MASK_MONTH),
        (interval::Unit::Day, iv::MASK_DAY),
        (interval::Unit::Hour, iv::MASK_HOUR),
        (interval::Unit::Minute, iv::MASK_MINUTE),
        (interval::Unit::Second, iv::MASK_SECOND),
    ];
    let rung = |field: &ast::DateTimeField| {
        let unit = datetime_field_to_unit(field);
        LADDER.iter().position(|(u, _)| *u == unit)
    };

    let leading = node.leading_field.as_ref()?;
    let first = rung(leading)?;
    // A bare qualifier admits only its own field; `X TO Y` admits the span.
    let last = match node.last_field.as_ref() {
        Some(field) => rung(field)?,
        None => first,
    };
    if last < first {
        return None;
    }
    let range = LADDER[first..=last]
        .iter()
        .fold(0, |acc, (_, bit)| acc | bit);
    iv::range_name(range)?;
    // A precision only means anything on a range reaching SECOND, and the parser
    // files it in one of two places: `X TO SECOND(n)` fills
    // `fractional_seconds_precision`, while a bare `SECOND(n)` — where SECOND is
    // itself the leading field — goes through the grammar's
    // `SECOND(<leading> [, <fractional>])` form and lands in `leading_precision`.
    let precision = node
        .fractional_seconds_precision
        .or(node.leading_precision)
        .map(|p| (p as i32).min(crabgresql_types::timestamp::MAX_PRECISION) as u8);
    Some(iv::pack_typmod(range, precision))
}

/// Whether an interval literal is nothing but a number — no unit word, no time
/// part — which is what decides which end of a range qualifier types it. Garbage
/// still falls through to `interval_in`, whose error is the one to report.
fn is_bare_number(s: &str) -> bool {
    let body = s.trim();
    let body = body.strip_prefix(['+', '-']).unwrap_or(body);
    !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit() || b == b'.')
}

/// Map a SQL-standard interval leading field to the default unit for a bare
/// number; anything unusual falls back to seconds (PG's default).
fn datetime_field_to_unit(field: &ast::DateTimeField) -> interval::Unit {
    use ast::DateTimeField::*;
    match field {
        Year | Years => interval::Unit::Year,
        Month | Months => interval::Unit::Month,
        Week(_) | Weeks => interval::Unit::Week,
        Day | Days => interval::Unit::Day,
        Hour | Hours => interval::Unit::Hour,
        Minute | Minutes => interval::Unit::Minute,
        _ => interval::Unit::Second,
    }
}

/// `EXTRACT(field FROM ts)`: PG's `date_part`-family sugar that returns
/// `numeric`. We support it on `timestamp`; the field name is carried as a text
/// constant argument and validated at run time (unknown units error there,
/// matching `date_part`).
pub(super) fn bind_extract(
    field: &ast::DateTimeField,
    expr: &ast::Expr,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let unit = datetime_field_unit(field);
    // The operand type selects the overload; interval, timestamp, and
    // timestamptz each have their own extract. An untyped literal defaults to
    // `timestamp`, matching PG.
    let (func, arg) = match bind_expr(expr, scope)? {
        Binding::Typed(e) if e.ty() == PgType::Timestamp => (ScalarFn::Extract, e),
        Binding::Typed(e) if e.ty() == PgType::Interval => (ScalarFn::ExtractInterval, e),
        Binding::Typed(e) if e.ty() == PgType::TimestampTz => (ScalarFn::ExtractTz, e),
        Binding::Typed(e) if e.ty() == PgType::Date => (ScalarFn::ExtractDate, e),
        Binding::Typed(e) if e.ty() == PgType::Time => (ScalarFn::ExtractTime, e),
        Binding::Typed(e) if e.ty() == PgType::TimeTz => (ScalarFn::ExtractTimeTz, e),
        Binding::Typed(e) => {
            return Err(BindError::new(
                sqlstate::UNDEFINED_FUNCTION,
                format!(
                    "function pg_catalog.date_part(unknown, {}) does not exist",
                    e.ty().name()
                ),
            ));
        }
        Binding::Unknown { lit, span, param } => (
            ScalarFn::Extract,
            resolve_unknown(lit, span, param, PgType::Timestamp)?,
        ),
    };
    Ok(Binding::Typed(BoundExpr::FuncCall {
        func,
        ret: PgType::Numeric,
        args: vec![
            BoundExpr::Const {
                value: Value::Text(unit),
                ty: PgType::Text,
            },
            arg,
        ],
    }))
}

/// `<value> AT TIME ZONE <zone>`. The overload is chosen by the value's type: a
/// zone-less `timestamp` wall clock interpreted in `zone` yields a `timestamptz`
/// (UTC) instant; a `timestamptz` instant shown in `zone` yields a zone-less
/// `timestamp`. Lowers to the `timezone(zone_text, value)` function form (PG's
/// implementation of the syntax); the result column is named `timezone`.
pub(super) fn bind_at_time_zone(
    value: &ast::Expr,
    zone: &ast::Expr,
    scope: &Scope,
) -> Result<Binding, BindError> {
    // The zone is either a `text` name or an `interval` displacement; PG has
    // both overloads of every `timezone()` pair.
    let zone_arg = match bind_expr(zone, scope)? {
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, PgType::Text)?,
        Binding::Typed(e) if e.ty() == PgType::Text || e.ty() == PgType::Interval => e,
        Binding::Typed(e) => {
            // PG resolves both operand types before reporting, so name them
            // both. Binding the value here is safe: this is the error path, and
            // a failure in the value is the more specific complaint anyway.
            let value_ty = match bind_expr(value, scope) {
                Ok(Binding::Typed(v)) => v.ty().name().to_string(),
                Ok(Binding::Unknown { .. }) => PgType::Timestamp.name().to_string(),
                Err(inner) => return Err(inner),
            };
            return Err(BindError::new(
                sqlstate::UNDEFINED_FUNCTION,
                format!(
                    "function pg_catalog.timezone({}, {value_ty}) does not exist",
                    e.ty().name()
                ),
            ));
        }
    };
    let by_interval = zone_arg.ty() == PgType::Interval;
    let zone_name = zone_arg.ty().name();
    // An untyped value literal defaults to `timestamp` (→ timestamptz), as PG does.
    let (func, ret, value_arg) = match bind_expr(value, scope)? {
        Binding::Typed(e) if e.ty() == PgType::Timestamp => (
            pick(
                by_interval,
                ScalarFn::TimezoneIntervalToTz,
                ScalarFn::TimezoneToTz,
            ),
            PgType::TimestampTz,
            e,
        ),
        Binding::Typed(e) if e.ty() == PgType::TimestampTz => (
            pick(
                by_interval,
                ScalarFn::TimezoneIntervalToTs,
                ScalarFn::TimezoneToTs,
            ),
            PgType::Timestamp,
            e,
        ),
        // A bare `time` reaches the timetz overload through the implicit cast PG
        // uses here, picking up the session zone on the way.
        Binding::Typed(e) if e.ty() == PgType::Time => (
            pick(
                by_interval,
                ScalarFn::TimezoneIntervalTimeTz,
                ScalarFn::TimezoneTimeTz,
            ),
            PgType::TimeTz,
            coerce_expr(e, PgType::TimeTz)?,
        ),
        // Unlike the timestamp pair, a `timetz` keeps its type: the zone is part
        // of the value, so rotating it yields another `timetz`.
        Binding::Typed(e) if e.ty() == PgType::TimeTz => (
            pick(
                by_interval,
                ScalarFn::TimezoneIntervalTimeTz,
                ScalarFn::TimezoneTimeTz,
            ),
            PgType::TimeTz,
            e,
        ),
        Binding::Typed(e) => {
            return Err(BindError::new(
                sqlstate::UNDEFINED_FUNCTION,
                format!(
                    "function pg_catalog.timezone({zone_name}, {}) does not exist",
                    e.ty().name()
                ),
            ));
        }
        Binding::Unknown { lit, span, param } => (
            pick(
                by_interval,
                ScalarFn::TimezoneIntervalToTz,
                ScalarFn::TimezoneToTz,
            ),
            PgType::TimestampTz,
            resolve_unknown(lit, span, param, PgType::Timestamp)?,
        ),
    };
    Ok(Binding::Typed(BoundExpr::FuncCall {
        func,
        ret,
        args: vec![zone_arg, value_arg],
    }))
}

fn pick(by_interval: bool, interval: ScalarFn, text: ScalarFn) -> ScalarFn {
    if by_interval { interval } else { text }
}

/// `<value> AT LOCAL` — [`bind_at_time_zone`] against the session `TimeZone`,
/// which is only known at execution time, so it lowers to the one-argument
/// `timezone(value)` form rather than to a zone constant. PG names the result
/// column `timezone`, exactly as for `AT TIME ZONE`.
pub(super) fn bind_at_local(value: &ast::Expr, scope: &Scope) -> Result<Binding, BindError> {
    let (func, ret, arg) = match bind_expr(value, scope)? {
        Binding::Typed(e) if e.ty() == PgType::Timestamp => {
            (ScalarFn::TimezoneLocalToTz, PgType::TimestampTz, e)
        }
        Binding::Typed(e) if e.ty() == PgType::TimestampTz => {
            (ScalarFn::TimezoneLocalToTs, PgType::Timestamp, e)
        }
        Binding::Typed(e) if e.ty() == PgType::TimeTz => {
            (ScalarFn::TimezoneLocalTimeTz, PgType::TimeTz, e)
        }
        // As in `bind_at_time_zone`: a `time` casts to `timetz` first.
        Binding::Typed(e) if e.ty() == PgType::Time => (
            ScalarFn::TimezoneLocalTimeTz,
            PgType::TimeTz,
            coerce_expr(e, PgType::TimeTz)?,
        ),
        Binding::Typed(e) => {
            return Err(BindError::new(
                sqlstate::UNDEFINED_FUNCTION,
                format!(
                    "function pg_catalog.timezone({}) does not exist",
                    e.ty().name()
                ),
            ));
        }
        Binding::Unknown { lit, span, param } => (
            ScalarFn::TimezoneLocalToTz,
            PgType::TimestampTz,
            resolve_unknown(lit, span, param, PgType::Timestamp)?,
        ),
    };
    Ok(Binding::Typed(BoundExpr::FuncCall {
        func,
        ret,
        args: vec![arg],
    }))
}

/// The canonical unit string for an EXTRACT field, lowercased. Unknown/unusual
/// spellings fall back to the parser's rendering (also lowercased), leaving the
/// run-time `date_part` to reject truly unrecognized units.
fn datetime_field_unit(field: &ast::DateTimeField) -> String {
    use ast::DateTimeField::*;
    match field {
        Year | Years => "year",
        Month | Months => "month",
        Day | Days => "day",
        Hour | Hours => "hour",
        Minute | Minutes => "minute",
        Second | Seconds => "second",
        Millisecond | Milliseconds => "milliseconds",
        Microsecond | Microseconds => "microseconds",
        Decade => "decade",
        Century => "century",
        Millennium | Millenium => "millennium",
        Quarter => "quarter",
        Week(_) | Weeks => "week",
        Dow => "dow",
        Isodow => "isodow",
        Doy => "doy",
        Epoch => "epoch",
        Isoyear => "isoyear",
        Julian => "julian",
        other => return other.to_string().to_lowercase(),
    }
    .to_string()
}
