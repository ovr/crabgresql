//! Casts and coercions: explicit `CAST`, the implicit-castability lattice, the
//! common type two operands unify to, and the rules that turn an `unknown`
//! literal into a typed value.

use crabgresql_parser::{Span, ast};
use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::{EnumInfo, TypeCatalog, UserCast};
use crabgresql_types::numeric::ParseError;
use crabgresql_types::{
    FmtCtx, Numeric, PgType, RegKind, Value, cast, date, float, fmt, interval, money, parse_bool,
    time, timestamp, timestamptz, timetz,
};

use crate::BindError;
use crate::functions::ScalarFn;

use super::bind::bind_expr;
use super::bound::BoundExpr;
use super::datatype::{apply_length_typmod_if_any, apply_numeric_typmod_if_any, resolve_data_type};
use super::operators::is_text_family;
use super::params::ParamCtx;
use super::scope::{Binding, Scope};

pub(super) fn bind_cast(
    inner: &ast::Expr,
    data_type: &ast::DataType,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let target = resolve_data_type(scope.catalog(), data_type)?;
    // `ARRAY[]::t[]`: an empty array constructor is otherwise untypable (see
    // `bind_array_ctor`); the cast target supplies its element type.
    if let ast::Expr::Array(arr) = inner
        && arr.elem.is_empty()
        && let PgType::Array(elem_oid) = target
        && let Some(elem) = PgType::from_oid(elem_oid)
    {
        return Ok(Binding::Typed(BoundExpr::ArrayCtor {
            elem,
            ty: target,
            elems: Vec::new(),
        }));
    }
    // A reg* cast resolves an object name (or an OID's name) against the
    // catalog, which lives in the executor — so it lowers to a function call
    // instead of folding here.
    if let PgType::Reg(kind) = target {
        return Ok(Binding::Typed(bind_reg_cast(inner, kind, scope)?));
    }
    // `ARRAY[…]::reg*[]`: cast each element, for the same reason. A value-level
    // array cast cannot do this — coercing an element needs a catalog lookup,
    // not a pure conversion — so casting an existing `text[]` *expression* to
    // `reg*[]` is still unsupported; only the constructor spelling resolves.
    if let ast::Expr::Array(arr) = inner
        && let PgType::Array(elem_oid) = target
        && let Some(PgType::Reg(kind)) = PgType::from_oid(elem_oid)
    {
        let elems = arr
            .elem
            .iter()
            .map(|e| bind_reg_cast(e, kind, scope))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Binding::Typed(BoundExpr::ArrayCtor {
            elem: PgType::Reg(kind),
            ty: target,
            elems,
        }));
    }
    let expr = match bind_expr(inner, scope)? {
        Binding::Unknown { lit, span, param } => {
            resolve_unknown_ctx(scope.catalog().as_ref(), lit, span, param, target)?
        }
        Binding::Typed(e) => coerce_cast(e, target, scope)?,
    };
    let expr = apply_numeric_typmod_if_any(expr, target, data_type)?;
    Ok(Binding::Typed(apply_length_typmod_if_any(
        expr, target, data_type,
    )?))
}

/// The (normalized) name of a bare `DataType::Custom` type reference — e.g. the
/// `xfloat4` in `x::xfloat4` — used to look a `CREATE TYPE` name up in the
/// catalog. `None` for anything that is not a plain custom name.
/// Lower `expr::regclass` (and the other `reg*` targets) to the catalog-backed
/// function that resolves it at run time.
///
/// Which function depends on what is being cast, matching PG: a *name* is looked
/// up and must exist, whereas an *OID* is taken as-is and only rendered — PG's
/// oid→reg casts are binary-coercible, so `999999::regclass` prints the digits
/// rather than erroring. An unknown literal is the name form.
fn bind_reg_cast(inner: &ast::Expr, kind: RegKind, scope: &Scope) -> Result<BoundExpr, BindError> {
    let expr = match bind_expr(inner, scope)? {
        Binding::Unknown { lit, span, param } => {
            resolve_unknown_ctx(scope.catalog().as_ref(), lit, span, param, PgType::Text)?
        }
        Binding::Typed(e) => e,
    };
    let ty = expr.ty();
    let (func, arg_ty) = match ty {
        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => {
            (ScalarFn::RegIn(kind), PgType::Text)
        }
        // reg* -> reg* goes through the OID, as it does in PG: the OID is kept
        // and re-rendered as the new kind of object.
        PgType::Oid | PgType::Int2 | PgType::Int4 | PgType::Int8 | PgType::Reg(_) => {
            (ScalarFn::RegFromOid(kind), PgType::Oid)
        }
        other => {
            return Err(BindError::new(
                sqlstate::CANNOT_COERCE,
                format!("cannot cast type {} to {}", other.name(), kind.typname()),
            ));
        }
    };
    Ok(BoundExpr::FuncCall {
        func,
        ret: PgType::Reg(kind),
        args: vec![coerce_expr(expr, arg_ty)?],
    })
}

/// Coerce `expr` to an explicit-cast `target`. When a user-defined type is on
/// either side, the catalog decides whether the cast exists and how it runs
/// (a `WITHOUT FUNCTION` cast reinterprets the bit pattern); otherwise this is
/// the ordinary builtin coercion.
fn coerce_cast(expr: BoundExpr, target: PgType, scope: &Scope) -> Result<BoundExpr, BindError> {
    if matches!(expr.ty(), PgType::User(_)) || matches!(target, PgType::User(_)) {
        return coerce_user_cast(expr, target, scope);
    }
    coerce_expr(expr, target)
}

/// Apply a cast where at least one side is a user-defined type. Only casts
/// registered via `CREATE CAST` are allowed; a `WITHOUT FUNCTION` one lowers to
/// a `Reinterpret` over the target's backing builtin.
fn coerce_user_cast(
    expr: BoundExpr,
    target: PgType,
    scope: &Scope,
) -> Result<BoundExpr, BindError> {
    let source = expr.ty();
    if source == target {
        return Ok(expr);
    }
    let catalog = scope.catalog();

    // Enum → text renders the label (PG's `enum_out`); the other text-family
    // targets do not have this cast, so they must use an explicitly registered
    // conversion just like any other user-type pair.
    if let PgType::User(oid) = source
        && catalog.enum_info(oid).is_some()
        && target == PgType::Text
    {
        return coerce_expr(expr, target);
    }
    // Text family → enum maps the label to its ordinal. Only a constant text
    // value can resolve at bind time; a runtime `textcol::myenum` cast has no
    // catalog to consult in the executor and is not supported yet.
    if is_text_family(source)
        && let PgType::User(oid) = target
        && let Some(info) = catalog.enum_info(oid)
    {
        return match expr {
            BoundExpr::Const {
                value: Value::Text(s),
                ..
            } => enum_const(oid, &info, Some(s), Span::empty()),
            BoundExpr::Const {
                value: Value::Null, ..
            } => Ok(BoundExpr::Const {
                value: Value::Null,
                ty: target,
            }),
            _ => Err(BindError::feature_not_supported(
                "casting a non-constant text expression to an enum is not supported yet",
            )),
        };
    }

    match catalog.find_cast(source, target) {
        Some(UserCast {
            without_function: true,
        }) => Ok(BoundExpr::Reinterpret {
            expr: Box::new(expr),
            reported: target,
            rep: scope.catalog().backing_rep(target),
        }),
        // WITH FUNCTION / WITH INOUT are rejected at `CREATE CAST`; guard anyway.
        Some(UserCast {
            without_function: false,
        }) => Err(BindError::feature_not_supported(
            "cast with a conversion function is not supported yet",
        )),
        None => Err(BindError::new(
            sqlstate::CANNOT_COERCE,
            format!(
                "cannot cast type {} to {}",
                type_label(source, catalog.as_ref()),
                type_label(target, catalog.as_ref())
            ),
        )),
    }
}

pub(super) fn bind_typed_string(
    ts: &ast::TypedString,
    scope: &Scope,
) -> Result<Binding, BindError> {
    // Same resolution a cast target goes through, so `CREATE TYPE` names work in
    // the `t 'literal'` spelling exactly as they do in `'literal'::t`.
    let target = resolve_data_type(scope.catalog(), &ts.data_type)?;
    let (lit, span) = match ts.value.value.as_pg_string() {
        Some(s) => (Some(s.to_string()), ts.value.span),
        None => {
            return Err(BindError::syntax(format!(
                "invalid typed literal: {}",
                ts.value.value
            )));
        }
    };
    let expr = resolve_unknown_ctx(scope.catalog().as_ref(), lit, span, None, target)?;
    let expr = apply_numeric_typmod_if_any(expr, target, &ts.data_type)?;
    Ok(Binding::Typed(apply_length_typmod_if_any(
        expr,
        target,
        &ts.data_type,
    )?))
}

pub(crate) fn binding_type_label(b: &Binding) -> String {
    match b {
        Binding::Typed(e) => e.ty().name().to_string(),
        Binding::Unknown { .. } => "unknown".to_string(),
    }
}

/// Coerce a function argument binding to `target`. Unknown literals resolve to
/// `target`; a typed argument matches exactly, or (when `exact_only` is false)
/// is promoted if `target` is its common type with `target` — reproducing PG's
/// implicit numeric widening for function arguments.
pub(crate) fn coerce_for_arg(
    binding: Binding,
    target: PgType,
    exact_only: bool,
) -> Option<BoundExpr> {
    match binding {
        // A parameter's type is deduced by the *side effect* of `resolve_unknown`
        // (it records the type in the shared context). Overload resolution tries
        // this speculatively for every candidate signature, so resolving a
        // parameter during the exact-only pass would pin it to whichever
        // signature is tried first. Decline an unresolved parameter as an "exact"
        // match and let the typed arguments drive the choice in the fallback
        // pass; a literal (no param) still folds to its exact target as before.
        Binding::Unknown {
            lit,
            span,
            param: Some(param),
        } if exact_only => {
            // A parameter already fixed to `target` by an earlier occurrence is a
            // genuine exact match and must not be dropped. Read the slot into a
            // local so the shared borrow is released before `resolve_unknown`
            // takes it mutably.
            let already = param.1.borrow().slot_type(param.0);
            if already == Some(target) {
                resolve_unknown(lit, span, Some(param), target).ok()
            } else {
                None
            }
        }
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, target).ok(),
        Binding::Typed(e) => {
            if e.ty() == target {
                return Some(e);
            }
            if exact_only {
                return None;
            }
            if implicit_castable(e.ty(), target) {
                return coerce_expr(e, target).ok();
            }
            None
        }
    }
}

/// Whether `from` implicitly casts to `to` in a function-argument (or operator)
/// context — the numeric-widening casts PG marks implicit, including int→float4
/// (so e.g. `float4send(1)` resolves).
pub(crate) fn implicit_castable(from: PgType, to: PgType) -> bool {
    use PgType::*;
    from == to
        || matches!(
            (from, to),
            (Int2, Int4)
                | (Int2, Int8)
                | (Int4, Int8)
                | (Int2, Float4)
                | (Int4, Float4)
                | (Int8, Float4)
                | (Int2, Float8)
                | (Int4, Float8)
                | (Int8, Float8)
                | (Float4, Float8)
                | (Int2, Numeric)
                | (Int4, Numeric)
                | (Int8, Numeric)
                | (Numeric, Float4)
                | (Numeric, Float8)
                // `timestamp -> timestamptz` is an implicit cast in PG; the
                // reverse is assignment-only (reached via an explicit cast).
                | (Timestamp, TimestampTz)
                // `date` implicitly widens to `timestamp`/`timestamptz` (PG),
                // so date/timestamp comparisons and `date_trunc(text, date)`
                // resolve without dedicated date overloads.
                | (Date, Timestamp)
                | (Date, TimestampTz)
                // varchar/bpchar/name are implicitly convertible to text (so a
                // text function accepts them; `bpchar -> text` strips blanks).
                | (Varchar, Text)
                | (Bpchar, Text)
                | (Name, Text)
                // `"char" -> text` is implicit in PG (`pg_cast` context 'i'),
                // but the reverse is assignment-only, so it is not listed here.
                | (Char, Text)
                // `cidr -> inet` is an implicit cast in PG, so the inet
                // functions/operators accept a cidr argument.
                | (Cidr, Inet)
                // int -> oid is implicit in PG (`oideq` resolves `oid = 42`);
                // used so catalog predicates/joins compare oid columns to int
                // literals and each other.
                | (Int2, Oid)
                | (Int4, Oid)
                | (Int8, Oid)
                // `bit` and `bit varying` are mutually implicitly convertible in
                // PG (binary-coercible with a length coercion), so a `bit`
                // literal resolves a `varbit` overload and vice versa.
                | (Bit, Varbit)
                | (Varbit, Bit)
                // `reg* -> oid` is implicit in PG, which is how `oid =
                // 't'::regclass` resolves (both sides become oid) — the shape
                // psql's `\d` uses to match a relation. The reverse direction is
                // implicit in PG too, but cannot be a pure value cast here: it
                // has to resolve a name through the catalog, so `oid::regclass`
                // stays an explicit cast lowering to `RegFromOid`.
                | (Reg(_), Oid)
        )
}

/// Display a user type by its catalog name instead of the generic
/// `user-defined` placeholder used by catalog-free [`PgType::name`].
pub(crate) fn type_label(ty: PgType, catalog: &dyn TypeCatalog) -> String {
    match ty {
        PgType::User(oid) => catalog
            .user_type_name(oid)
            .unwrap_or_else(|| ty.name().to_string()),
        _ => ty.name().to_string(),
    }
}

/// The common type of two column entries (`VALUES` rows / `UNION` arms),
/// approximating PG's `select_common_type`: when exactly one side implicitly
/// casts to the other, the column takes that target (so `real` + `int4` -> `real`,
/// not `float8`). When neither or both cast implicitly, fall back to numeric
/// preferred-type promotion (`float8` dominates). This deliberately differs from
/// `unify_types` (operator resolution), where `real` + `int4` resolves to `float8`.
pub(crate) fn merge_types(a: PgType, b: PgType) -> Option<PgType> {
    if a == b {
        return Some(a);
    }
    // Two arrays unify on their element type (PG promotes `int[]` + `bigint[]`
    // to `bigint[]`); this also drives `array || array` and `array_cat`.
    if let (PgType::Array(la), PgType::Array(rb)) = (a, b) {
        let (le, re) = (PgType::from_oid(la)?, PgType::from_oid(rb)?);
        return merge_types(le, re).map(|e| PgType::Array(e.oid()));
    }
    match (implicit_castable(a, b), implicit_castable(b, a)) {
        (true, false) => Some(b),
        (false, true) => Some(a),
        // Mutually castable: today only `bit` <-> `bit varying`, whose common
        // type is `bit varying` (the preferred type of the bit-string category),
        // as PG's `select_common_type` resolves it.
        (true, true) => Some(PgType::Varbit),
        (false, false) => common_string(a, b).or_else(|| common_numeric(a, b)),
    }
}

/// The common type of two string types. `char(n)` and `varchar(n)` are not
/// castable to each other, so they only meet at `text` — the preferred type of
/// PG's string category, which `select_common_type` picks for them.
fn common_string(a: PgType, b: PgType) -> Option<PgType> {
    let is_string = |ty| {
        matches!(
            ty,
            PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name
        )
    };
    (is_string(a) && is_string(b)).then_some(PgType::Text)
}

/// Resolve a set of expressions (one `VALUES`/`UNION` column, or a `CASE`'s
/// result branches) to a common type and coerce every one to it. Untyped
/// literals adapt to the resolved type; an entirely untyped set resolves to
/// `text`, as PG does for unknown `UNION`/`VALUES`/`CASE`. Incompatible concrete
/// types are a `42804` error, prefixed with `label` (`VALUES` / `CASE`) to match
/// PG's wording.
pub(crate) fn unify_value_column(
    bindings: Vec<Binding>,
    label: &str,
) -> Result<(PgType, Vec<BoundExpr>), BindError> {
    let mut common: Option<PgType> = None;
    for binding in &bindings {
        if let Binding::Typed(e) = binding {
            common = Some(match common {
                None => e.ty(),
                Some(prev) => merge_types(prev, e.ty()).ok_or_else(|| {
                    BindError::new(
                        sqlstate::DATATYPE_MISMATCH,
                        format!(
                            "{label} types {} and {} cannot be matched",
                            prev.name(),
                            e.ty().name()
                        ),
                    )
                })?,
            });
        }
    }
    let ty = common.unwrap_or(PgType::Text);
    let exprs = bindings
        .into_iter()
        .map(|binding| match binding {
            Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, ty),
            Binding::Typed(e) => coerce_expr(e, ty),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((ty, exprs))
}

/// The common type of two distinct numeric types, following PG's preferred-type
/// resolution for the cases these tests exercise (float8 dominates; mixed int
/// widens).
pub(super) fn common_numeric(a: PgType, b: PgType) -> Option<PgType> {
    if !a.is_numeric() || !b.is_numeric() {
        return None;
    }
    Some(if a == PgType::Float8 || b == PgType::Float8 {
        PgType::Float8
    } else if a == PgType::Float4 || b == PgType::Float4 {
        // A float mixed with any other numeric type resolves to float8.
        PgType::Float8
    } else if a == PgType::Numeric || b == PgType::Numeric {
        // `numeric` dominates the integer types (int → numeric is exact).
        PgType::Numeric
    } else if a == PgType::Int8 || b == PgType::Int8 {
        PgType::Int8
    } else if a == PgType::Int4 || b == PgType::Int4 {
        PgType::Int4
    } else {
        PgType::Int2
    })
}

/// Coerce an expression to `ty`. Constant operands fold (and range-check) at
/// bind time, as PG's planner does; non-constants — and any conversion whose
/// result depends on session state — get a runtime `Coerce`. See
/// [`fold_needs_session`].
pub(crate) fn coerce_expr(expr: BoundExpr, ty: PgType) -> Result<BoundExpr, BindError> {
    if expr.ty() == ty {
        return Ok(expr);
    }
    // `bpchar -> text` strips trailing blanks (PG's bpchar->text cast), which is
    // how a padded `char(n)` value loses its padding under `||`, `::text`, and
    // most text functions. It cannot be done in `cast_value` because a padded
    // `bpchar` value is indistinguishable from `text` there.
    if expr.ty() == PgType::Bpchar && ty == PgType::Text {
        if let BoundExpr::Const {
            value: Value::Text(s),
            ..
        } = &expr
        {
            return Ok(BoundExpr::Const {
                value: Value::Text(s.trim_end_matches(' ').to_string()),
                ty: PgType::Text,
            });
        }
        return Ok(BoundExpr::FuncCall {
            func: ScalarFn::BpcharToText,
            ret: PgType::Text,
            args: vec![expr],
        });
    }
    match expr {
        BoundExpr::Const {
            value,
            ty: value_ty,
        } if !fold_needs_session(value.pg_type(), ty)
            && !interval_input_needs_style(&value, ty) =>
        {
            // Safe to fold *unless the value reads the clock*: `fold_needs_session`
            // rules out every pair `FmtCtx::utc` could change by GUC, but it is a
            // question about types, and whether a literal is relative is a question
            // about its text — only the input function knows. So try the fold and
            // defer if it turns out to want a session, exactly as `resolve_unknown`
            // does one layer up. Without this, `'now'::text::timestamp` reports the
            // internal "no transaction clock" error to the client.
            match cast::cast_value(value.clone(), ty, &FmtCtx::utc_default()) {
                Ok(value) => Ok(BoundExpr::Const { value, ty }),
                Err(e) if cast_needs_clock(&e) => Ok(BoundExpr::Coerce {
                    expr: Box::new(BoundExpr::Const {
                        value,
                        ty: value_ty,
                    }),
                    ty,
                }),
                Err(e) => Err(BindError::new(e.sqlstate, e.message).with_detail(e.detail)),
            }
        }
        expr => Ok(BoundExpr::Coerce {
            expr: Box::new(expr),
            ty,
        }),
    }
}

/// Whether converting `from` to `to` reads session state, and so must be left
/// to a runtime `Coerce` instead of folded at bind time. The binder holds no
/// session, and folding one of these would freeze the *binding* session's
/// answer into the plan — visibly wrong for a prepared statement re-executed
/// after a `SET`.
///
/// Three GUCs reach this far:
///
/// * `extra_float_digits`, for any conversion to a string type. (The old guard
///   here tested only `Text`, so `1.5::float8::varchar` folded at the default
///   precision and silently ignored the session's setting.)
/// * `bytea_output`, which rides on the same string-type arm: `'\x00'::bytea`
///   renders as `\x00` or `\000` depending on the session, so `::text` on one
///   cannot be folded either.
/// * `TimeZone`, for every `timestamptz` conversion — the zone is what relates
///   an instant to a wall clock, in both directions — and for every conversion
///   to `timetz`, which attaches the zone's offset when the value carries none.
///
/// The transaction clock is the fourth such input, and [`resolve_unknown`]
/// defers on it the same way — but it is detected by probing rather than listed
/// here, since `'today 10:00'` is as relative as `'today'` and only the scanner
/// knows that.
///
/// **Known divergence.** PostgreSQL folds these literals during parse analysis,
/// freezing the instant with the parsing session's zone and clock; we defer and
/// recompute per execution. Visible for a prepared statement re-executed after
/// a `SET TimeZone` (pinned by
/// `prepared_statement_diverges_from_pg_on_a_later_set_timezone`), and for
/// `PREPARE p AS SELECT 'now'::timestamptz`, which PG answers identically on
/// every `EXECUTE`.
///
/// Handing the binder a session `FmtCtx` would *not* close it: the extended
/// protocol re-binds the statement on every `Execute` (`analyze_statement`
/// binds for Describe and throws the plan away; `bind_dml_with_params` runs
/// again from Execute), so a bind-time fold would be re-done with each
/// execution's own session state and land back where it started. Closing it
/// needs a plan cache. What the deferral *does* still cost is nothing at DDL
/// time: a column default resolves through `zoned_literal_default`, where the
/// session is in hand, so `DEFAULT 'now'` freezes exactly as PG's does.
/// Whether folding this string-to-`interval` conversion at bind time would
/// read the literal under the wrong `IntervalStyle`.
///
/// A companion to [`fold_needs_session`] rather than a case inside it:
/// `fold_needs_session` asks about a pair of *types*, and every string is
/// convertible to an interval, so answering there would defer every interval
/// literal to execution and move its syntax errors out of parse analysis. Only
/// the text can say, and for almost every literal the answer is no — see
/// [`interval::style_sensitive`].
fn interval_input_needs_style(value: &Value, to: PgType) -> bool {
    matches!(value, Value::Text(s) if reads_interval_style(s, to))
}

/// Whether reading `text` as `ty` consults `IntervalStyle`, and so cannot be
/// folded by a binder that holds no session.
///
/// For a scalar the answer is per-literal: only one whose leading minus would
/// propagate under `sql_standard` reads differently, which is what
/// [`interval::style_sensitive`] decides.
///
/// For an interval **array** the answer is always yes. `style_sensitive` cannot
/// be pointed at the raw `{…}` text instead: `interval`'s field scanner treats
/// `{`, `}` and `,` as ordinary separators, so `{1 day, -1 year 2 months}` reads
/// as the fields `1 day -1 year 2 months`, whose leading field is positive — it
/// would answer "not sensitive" while the *second element* is. Splitting the
/// array first would need `array_in`'s own scanner exposed. Deferring the whole
/// type costs a bind-time fold for literals that did not need it and never a
/// wrong value, which is the trade `style_sensitive` itself already makes.
fn reads_interval_style(text: &str, ty: PgType) -> bool {
    match ty {
        PgType::Interval => crabgresql_types::interval::style_sensitive(text),
        PgType::Array(elem) => elem == crabgresql_types::oid::INTERVAL,
        _ => false,
    }
}

fn fold_needs_session(from: Option<PgType>, to: PgType) -> bool {
    if matches!(
        to,
        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name
    ) {
        return true;
    }
    depends_on_session_zone(to) || from.is_some_and(depends_on_session_zone)
}

/// Parse a zone-dependent literal for its *diagnostics only*, discarding the
/// value — see the call site in [`resolve_unknown`].
fn validate_zone_dependent_literal(s: &str, ty: PgType) -> Result<(), BindError> {
    match ty {
        PgType::TimestampTz => timestamptz::parse(s, &FmtCtx::utc_default())
            .map(|_| ())
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::TimeTz => timetz::parse(s, &FmtCtx::utc_default())
            .map(|_| ())
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Array(_) => parse_unknown(s, ty, &FmtCtx::utc_default()).map(|_| ()),
        _ => Ok(()),
    }
}

/// Whether values of `ty` are read or rendered relative to the session zone.
///
/// `timestamptz` in both directions, and `timetz` on the way *in*: a `timetz`
/// renders the offset it stores, but a literal that carries no offset takes the
/// session's, so `'03:30'::timetz` is as zone-dependent as `'03:30'::time`
/// widening to one. That is also what makes every conversion *to* `timetz`
/// deferred, which is what a `time -> timetz` cast needs — the fact lives on
/// the type rather than in a list of cast pairs, so a new arm in `cast.rs`
/// cannot silently miss it.
///
/// Arrays follow their element, since `array_in`/`array_out` delegate to the
/// element's own I/O functions.
fn depends_on_session_zone(ty: PgType) -> bool {
    use crabgresql_types::oid;
    match ty {
        PgType::TimestampTz | PgType::TimeTz => true,
        PgType::Array(elem) => elem == oid::TIMESTAMPTZ || elem == oid::TIMETZ,
        _ => false,
    }
}

/// Force a binding to boolean for WHERE / AND / OR / NOT. `context` is the
/// clause or operator name as PG prints it, and `span` locates the operand,
/// which is where PG points the `LINE n: ... ^` cursor for every one of these
/// clauses. Pass `Span::empty()` when the operand was synthesized rather than
/// written (the cursor is then omitted, as it is for a `BETWEEN` desugared into
/// `AND`) — see `bind_binary_op`, which has no operand spans to give.
pub(crate) fn to_bool_operand(
    binding: Binding,
    context: &str,
    span: Span,
) -> Result<BoundExpr, BindError> {
    match binding {
        Binding::Typed(e) if e.ty() == PgType::Bool => Ok(e),
        Binding::Typed(e) => Err(BindError::new(
            sqlstate::DATATYPE_MISMATCH,
            format!(
                "argument of {context} must be type boolean, not type {}",
                e.ty().name()
            ),
        )
        .at(span)),
        // `resolve_unknown` reports the literal's own position, which is finer
        // than the operand span, so leave its cursor alone.
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, PgType::Bool),
    }
}

/// Give an untyped literal its type from context, parsing its text the way the
/// type's input function would. A parse failure carries the literal's position
/// (PG's cursor), matching the `LINE n: ... ^` output.
pub(super) fn resolve_unknown(
    lit: Option<String>,
    span: Span,
    param: Option<(usize, ParamCtx)>,
    ty: PgType,
) -> Result<BoundExpr, BindError> {
    // A bind parameter takes its type from this context: record the deduction
    // (conflicting deductions are 42P18) and emit a runtime `Param`, never a
    // folded constant.
    if let Some((index, ctx)) = param {
        ctx.borrow_mut().resolve(index, ty)?;
        return Ok(BoundExpr::Param { index, ty });
    }
    // A `reg*` literal names an object, and only the catalog can turn a name
    // into an OID — which the binder does not hold. Emit the same runtime
    // resolution an explicit `'t'::regclass` lowers to, so a literal that takes
    // its type from a `reg*` context resolves the way PG's `regclassin` does
    // instead of failing to fold here.
    //
    // Divergence: in a comparison PostgreSQL types the literal from the chosen
    // operator, and `reg* = unknown` picks `oideq`, so PG reads the literal as
    // an OID and rejects `'pg_class'::regclass = 'pg_class'` with "invalid input
    // syntax for type oid". This binder types it from the other side and
    // resolves the name, accepting it. Erring toward resolution keeps the
    // literal usable wherever a `reg*` is expected; matching PG exactly would
    // mean typing unknown operands from operator resolution.
    if let PgType::Reg(kind) = ty {
        let arg = match lit {
            None => BoundExpr::Const {
                value: Value::Null,
                ty: PgType::Text,
            },
            Some(s) => BoundExpr::Const {
                value: Value::Text(s),
                ty: PgType::Text,
            },
        };
        return Ok(BoundExpr::FuncCall {
            func: ScalarFn::RegIn(kind),
            ret: ty,
            args: vec![arg],
        });
    }
    // A `timestamptz` literal cannot be *folded*: with no zone token its text
    // means a different instant in every session zone, and the binder has no
    // session. Emit the same runtime coercion `'…'::timestamptz` lowers to.
    //
    // It is still parsed here, and the result thrown away, purely to keep PG's
    // parse-analysis diagnostics: a bad literal must report the `LINE n: … ^`
    // cursor at its own position, which a runtime error has no span for.
    // Validating against UTC is sound for this because the zone can only change
    // the verdict for a value within a zone's offset of the representable
    // boundary — a syntax error is a syntax error in every zone.
    if depends_on_session_zone(ty) {
        let text = match lit {
            None => Value::Null,
            Some(s) => {
                // A relative literal cannot be validated without the clock
                // either; it is deferred whole, like the value itself.
                match validate_zone_dependent_literal(&s, ty) {
                    Ok(()) => {}
                    Err(e) if is_clock_unavailable(&e) => {}
                    Err(e) => return Err(e.at(span)),
                }
                Value::Text(s)
            }
        };
        return Ok(BoundExpr::Coerce {
            expr: Box::new(BoundExpr::Const {
                value: text,
                ty: PgType::Text,
            }),
            ty,
        });
    }
    // An interval literal whose meaning `sql_standard` would change cannot be
    // folded either, for the same reason and with the same shape — validated
    // here for the diagnostic, evaluated under the executing session's style.
    // Covers `interval[]` as a whole, per `reads_interval_style`.
    if lit.as_deref().is_some_and(|s| reads_interval_style(s, ty)) {
        let s = lit.unwrap_or_else(|| unreachable!("checked as Some above"));
        parse_unknown(&s, ty, &FmtCtx::utc_default()).map_err(|e| e.at(span))?;
        return Ok(BoundExpr::Coerce {
            expr: Box::new(BoundExpr::Const {
                value: Value::Text(s),
                ty: PgType::Text,
            }),
            ty,
        });
    }
    let value = match lit {
        None => Value::Null,
        Some(s) => match parse_unknown(&s, ty, &FmtCtx::utc_default()) {
            Ok(value) => value,
            // The literal reads the transaction clock (`now`, `today`,
            // `tomorrow`, `yesterday`), which the binder does not hold. Defer
            // it to a runtime coercion, exactly as the zone-dependent branch
            // above does — and for the same reason. Any *other* error is a real
            // diagnostic and keeps its cursor position.
            Err(e) if is_clock_unavailable(&e) => {
                return Ok(BoundExpr::Coerce {
                    expr: Box::new(BoundExpr::Const {
                        value: Value::Text(s),
                        ty: PgType::Text,
                    }),
                    ty,
                });
            }
            Err(e) => return Err(e.at(span)),
        },
    };
    Ok(BoundExpr::Const { value, ty })
}

/// Whether an input-function error is "this value needs the transaction clock"
/// rather than a complaint about the input.
///
/// Probing for it, instead of matching the token text, is what keeps the
/// composite forms working: `'today 10:00'` and `'10:00 today'` are relative
/// too, and a token table here would have to re-implement the scanner to know
/// that.
///
/// The SQLSTATE alone would be too coarse a signal. `XX000` means "something
/// unexpected", and `parse_unknown` fans out to every type's input function —
/// several of which raise it for genuine internal faults. Keying on it alone
/// would silently turn any of those into a deferral, losing the diagnostic's
/// cursor position and moving it from bind time to per-row execution. Matching
/// the shared [`fmt::CLOCK_UNAVAILABLE`] marker keeps the two apart.
fn is_clock_unavailable(e: &BindError) -> bool {
    e.code == sqlstate::INTERNAL_ERROR && e.message == fmt::CLOCK_UNAVAILABLE
}

/// [`is_clock_unavailable`] for the cast layer's own error type.
fn cast_needs_clock(e: &crabgresql_types::cast::CastError) -> bool {
    e.sqlstate == sqlstate::INTERNAL_ERROR && e.message == fmt::CLOCK_UNAVAILABLE
}

/// [`parse_unknown`] for a caller that owns the input.
///
/// The text family's value *is* the input string, so a caller holding a `String`
/// can hand it over instead of paying for a copy it then drops. That is every
/// cell of a bulk load: the COPY decoder already builds one owned `String` per
/// field, and passing it by reference made each text cell allocate twice.
///
/// Only the arms whose value is the whole input can take it; everything else
/// parses a fresh representation out of the text and borrows as before. Kept
/// beside [`parse_unknown`] rather than special-cased at the call site, so which
/// types share text's representation stays stated in one place.
pub(crate) fn parse_unknown_owned(s: String, ty: PgType, fmt: &FmtCtx) -> Result<Value, BindError> {
    match ty {
        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => Ok(Value::Text(s)),
        _ => parse_unknown(&s, ty, fmt),
    }
}

pub(crate) fn parse_unknown(s: &str, ty: PgType, fmt: &FmtCtx) -> Result<Value, BindError> {
    let invalid = || {
        BindError::new(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            format!("invalid input syntax for type {}: \"{s}\"", ty.name()),
        )
    };
    let float_error = |e: float::FloatParseError| BindError::new(e.sqlstate, e.message);
    match ty {
        // varchar / bpchar / name share text's value representation; any length
        // limit is applied afterward as a typmod coercion.
        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => {
            Ok(Value::Text(s.to_string()))
        }
        // `"char"` gets its own arm rather than joining the text family: it is
        // one raw byte, and `charin` decodes an octal escape on the way in.
        // Never fails.
        PgType::Char => Ok(Value::Char(crabgresql_types::char::char_in(s))),
        // Integer input (trim, base-10, 22003 overflow vs 22P02 malformed) is
        // the same acceptor the executor's text→int cast uses; share it so the
        // two never drift. resolve_unknown attaches the cursor position.
        PgType::Int2 | PgType::Int4 | PgType::Int8 => {
            cast::text_to_int(s, ty).map_err(|e| BindError::new(e.sqlstate, e.message))
        }
        PgType::Oid => cast::text_to_oid(s).map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Float4 => float::float4in(s).map(Value::Float4).map_err(float_error),
        PgType::Float8 => float::float8in(s).map(Value::Float8).map_err(float_error),
        PgType::Numeric => Numeric::parse(s).map(Value::Numeric).map_err(|e| match e {
            ParseError::Syntax => invalid(),
            ParseError::Overflow => BindError::new(
                sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
                "value overflows numeric format",
            ),
        }),
        PgType::Bool => parse_bool(s).map(Value::Bool).ok_or_else(invalid),
        PgType::Date => date::parse(s, fmt)
            .map(Value::Date)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Time => time::parse(s, fmt)
            .map(Value::Time)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        // Zone-dependent, so unreachable from `resolve_unknown` for the same
        // reason `TimestampTz` below is; UTC serves `pg_input_is_valid`, which
        // asks only whether the syntax is acceptable.
        PgType::TimeTz => timetz::parse(s, fmt)
            .map(Value::TimeTz)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Timestamp => timestamp::parse(s, fmt)
            .map(Value::Timestamp)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Interval => {
            interval::parse_with_style(s, interval::Unit::Second, fmt.interval_style)
                .map(Value::Interval)
                .map_err(|e| BindError::new(e.sqlstate, e.message))
        }
        // Unreachable from `resolve_unknown`, which defers every zone-dependent
        // type to runtime; this serves only `pg_input_is_valid`, which asks
        // whether the *syntax* is acceptable. Validity is zone-independent
        // except within a few hours of the representable range, so UTC answers
        // it correctly for every input a caller can realistically ask about.
        PgType::TimestampTz => timestamptz::parse(s, fmt)
            .map(Value::TimestampTz)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        // bytea input (byteain) is shared with the executor's text→bytea cast.
        PgType::Bytea => cast::byteain(s)
            .map(Value::Bytea)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Uuid => crabgresql_types::uuid::parse(s)
            .map(Value::Uuid)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Tid => crabgresql_types::tid::parse(s)
            .map(|(block, offset)| Value::Tid { block, offset })
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Xid => crabgresql_types::xid::xid_in(s)
            .map(Value::Xid)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Xid8 => crabgresql_types::xid::xid8_in(s)
            .map(Value::Xid8)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::PgLsn => crabgresql_types::pg_lsn::parse(s)
            .map(Value::PgLsn)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Inet => crabgresql_types::net::inet_in(s)
            .map(Value::Inet)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Cidr => crabgresql_types::net::cidr_in(s)
            .map(Value::Cidr)
            .map_err(|e| {
                BindError::new(e.sqlstate, e.message).with_detail(e.detail.map(String::from))
            }),
        PgType::Money => money::parse(s)
            .map(Value::Money)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        // `bit_in`/`varbit_in`: default binary, `x`-prefixed hex. The typmod (the
        // declared length) is applied afterward by the caller's coercion.
        PgType::Bit | PgType::Varbit => crabgresql_types::bit::input(s)
            .map(|(len, data)| Value::Bit { len, data })
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Macaddr => crabgresql_types::macaddr::parse_macaddr(s)
            .map(Value::Macaddr)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Macaddr8 => crabgresql_types::macaddr::parse_macaddr8(s)
            .map(Value::Macaddr8)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Point => crabgresql_types::geo::parse_point(s)
            .map(Value::Point)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Lseg => crabgresql_types::geo::parse_lseg(s)
            .map(Value::Lseg)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Path => crabgresql_types::geo::parse_path(s)
            .map(Value::Path)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Box => crabgresql_types::geo::parse_box(s)
            .map(Value::Box)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Line => crabgresql_types::geo::parse_line(s)
            .map(Value::Line)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Circle => crabgresql_types::geo::parse_circle(s)
            .map(Value::Circle)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Polygon => crabgresql_types::geo::parse_polygon(s)
            .map(Value::Polygon)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        // `json` keeps the raw text; `jsonb` parses/canonicalizes. Both carry the
        // JSON DETAIL through so `'{bad'::json` reproduces PG's error report.
        PgType::Json => crabgresql_types::json::json_in(s)
            .map(Value::Json)
            .map_err(|e| BindError::new(e.sqlstate, e.message).with_detail(e.detail)),
        PgType::Jsonb => crabgresql_types::json::jsonb_in(s)
            .map(Value::Jsonb)
            .map_err(|e| BindError::new(e.sqlstate, e.message).with_detail(e.detail)),
        // `jsonpath` parses the SQL/JSON path language into a compiled program.
        PgType::Jsonpath => crabgresql_types::jsonpath::jsonpath_in(s)
            .map(Value::Jsonpath)
            .map_err(|e| BindError::new(e.sqlstate, e.message).with_detail(e.detail)),
        // The text-search types parse their own input languages. Neither carries
        // a DETAIL; the message already names the offending input.
        PgType::Tsvector => crabgresql_types::tsvector::tsvector_in(s)
            .map(Value::Tsvector)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Tsquery => crabgresql_types::tsquery::tsquery_in(s)
            .map(Value::Tsquery)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        // `array_in`: parse a `{...}` literal, coercing each element to the array
        // element type. Carry PG's DETAIL through (`'{a,,c}'::int[]`).
        PgType::Array(elem_oid) => {
            let elem = PgType::from_oid(elem_oid).ok_or_else(invalid)?;
            // `fmt`, not a fresh UTC one: an element is read by the element
            // type's own input function, so it needs the same session zone and
            // transaction clock a scalar of that type would get.
            crabgresql_types::array::array_in(s, elem, fmt)
                .map(|elems| Value::Array { elem, elems })
                .map_err(|e| BindError::new(e.sqlstate, e.message).with_detail(e.detail))
        }
        // Both vector errors name the *element* type (`oid`/`smallint`), so the
        // message comes from `vector_in` rather than the `invalid` helper above.
        PgType::Vector(kind) => crabgresql_types::vector::vector_in(s, kind)
            .map(|elems| Value::Vector { kind, elems })
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        // Never reached: `resolve_unknown` intercepts a reg* literal and lowers
        // it to the runtime `RegIn` resolution, because only the catalog can
        // turn an object name into an OID and the binder holds none.
        PgType::Reg(_) => Err(BindError::new(
            sqlstate::INTERNAL_ERROR,
            "reg* literal reached the constant folder",
        )),
        PgType::User(_) => Err(invalid()),
    }
}

/// `resolve_unknown`, but aware of user-defined enum targets: a text literal
/// destined for an enum becomes a [`Value::Enum`] via a catalog label lookup
/// (an unknown label is PG's `invalid input value for enum` error). Every other
/// target defers to the catalog-free [`resolve_unknown`].
pub(crate) fn resolve_unknown_ctx(
    catalog: &dyn TypeCatalog,
    lit: Option<String>,
    span: Span,
    param: Option<(usize, ParamCtx)>,
    ty: PgType,
) -> Result<BoundExpr, BindError> {
    if param.is_some() {
        return resolve_unknown(lit, span, param, ty);
    }
    if let PgType::User(oid) = ty
        && let Some(info) = catalog.enum_info(oid)
    {
        return enum_const(oid, &info, lit, span);
    }
    resolve_unknown(lit, span, None, ty)
}

/// Build an enum constant from a text literal by mapping the label to its
/// definition-order ordinal. A label not in the enum is PG's `enum_in` error
/// (22P02), carrying the literal's cursor position for the `LINE n: ^` caret.
fn enum_const(
    oid: u32,
    info: &EnumInfo,
    lit: Option<String>,
    span: Span,
) -> Result<BoundExpr, BindError> {
    let value = match lit {
        None => Value::Null,
        Some(s) => enum_value(oid, info, s).map_err(|e| e.at(span))?,
    };
    Ok(BoundExpr::Const {
        value,
        ty: PgType::User(oid),
    })
}

/// Map an enum label to its [`Value`]. Split out of [`enum_const`] so COPY's
/// direct value path resolves labels through the same lookup and raises the same
/// `enum_in` error, without building an expression to unwrap.
pub(crate) fn enum_value(oid: u32, info: &EnumInfo, label: String) -> Result<Value, BindError> {
    match info.labels.iter().position(|l| *l == label) {
        Some(ord) => Ok(Value::Enum {
            type_oid: oid,
            ordinal: ord as u32,
            label,
        }),
        None => Err(BindError::new(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            format!("invalid input value for enum {}: \"{label}\"", info.name),
        )),
    }
}

#[cfg(test)]
mod interval_style_tests {
    use super::*;

    /// An interval array always reads `IntervalStyle`, because the raw `{…}`
    /// text cannot be asked the per-literal question: the interval field scanner
    /// steps over `{`, `}` and `,`, so an array whose *second* element carries
    /// the propagating minus looks positive from the outside. Folding it at bind
    /// time would read it under the wrong style.
    #[test]
    fn an_interval_array_always_reads_the_session_style() {
        // The scalar answer stays per-literal.
        assert!(reads_interval_style("-1 2:03:04", PgType::Interval));
        assert!(!reads_interval_style("1 day -2 hours", PgType::Interval));

        let interval_array = PgType::Array(crabgresql_types::oid::INTERVAL);
        // Sensitive in the first element, and in the second — where a check
        // against the raw text answers "no".
        assert!(reads_interval_style("{-1 2:03:04}", interval_array));
        assert!(reads_interval_style("{1 day, -1 2:03:04}", interval_array));
        assert!(
            !crabgresql_types::interval::style_sensitive("{1 day, -1 2:03:04}"),
            "the raw-text shortcut this arm exists to avoid must still look positive"
        );
        // Insensitive literals defer too: the cost is a lost fold, not a value.
        assert!(reads_interval_style("{1 day}", interval_array));

        // Every other array is unaffected.
        assert!(!reads_interval_style(
            "{1}",
            PgType::Array(crabgresql_types::oid::INT4)
        ));
        assert!(!reads_interval_style("-1 2:03:04", PgType::Text));
    }
}
