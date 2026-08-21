//! Operator resolution: PG picks an operator by the *types* of its operands, so
//! each type family (temporal, network, geometric, bit, array, json, text
//! search, …) gets its own resolver, and the unary/binary entry points try them
//! in turn before reporting `42883`.

use crabgresql_parser::ast::Spanned;
use crabgresql_parser::{Span, ast};
use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::TypeCatalog;
use crabgresql_types::collation::DEFAULT_COLLATION_OID;
use crabgresql_types::{PgType, Value};

use crate::BindError;
use crate::functions::ScalarFn;

use super::bind::{bind_expr, binop_from_comparison};
use super::bound::{BinOp, BoundExpr, UnaryOp};
use super::coerce::{
    binding_type_label, coerce_expr, common_numeric, implicit_castable, merge_types,
    resolve_unknown, resolve_unknown_ctx, to_bool_operand, type_label,
};
use super::datatype::{has_equality, is_orderable};
use super::literal::parse_number;
use super::scope::{Binding, Scope, normalize_ident};

pub(super) fn bind_compound(parts: &[ast::Ident], scope: &Scope) -> Result<BoundExpr, BindError> {
    let [qualifier, column] = parts else {
        return Err(BindError::feature_not_supported(
            "schema-qualified column references are not supported yet",
        ));
    };
    let qualifier = normalize_ident(qualifier);
    let column = normalize_ident(column);
    // Resolves against this scope's relations first, then enclosing queries — a
    // qualified reference to an outer relation yields a correlated
    // `OuterColumnRef`. PG names a missing column with its qualifier, unquoted:
    // `column q.c does not exist` (contrast the unqualified form `column "c"
    // does not exist`).
    scope.resolve_qualified(&qualifier, &column)
}

fn no_op_unary(sym: &str, ty: &str) -> BindError {
    BindError::new(
        sqlstate::UNDEFINED_FUNCTION,
        format!("operator does not exist: {sym} {ty}"),
    )
}

fn ambiguous_unary(sym: &str) -> BindError {
    ambiguous_operator_msg(format!("operator is not unique: {sym} unknown"))
}

pub(super) fn bind_unary(
    op: ast::UnaryOperator,
    operand: &ast::Expr,
    scope: &Scope,
) -> Result<Binding, BindError> {
    // PG folds `-` into the numeric literal before choosing int4 vs int8:
    // `-2147483648` must be int4, not an overflowing negation of an int8.
    if op == ast::UnaryOperator::Minus
        && let ast::Expr::Value(v) = operand
        && let ast::Value::Number(n, _) = &v.value
    {
        return parse_number(&format!("-{n}")).map(Binding::Typed);
    }
    match op {
        ast::UnaryOperator::Minus | ast::UnaryOperator::Plus => {
            let sym = if op == ast::UnaryOperator::Minus {
                "-"
            } else {
                "+"
            };
            match bind_expr(operand, scope)? {
                Binding::Typed(e) if e.ty().is_numeric() => {
                    Ok(Binding::Typed(if op == ast::UnaryOperator::Minus {
                        BoundExpr::Unary {
                            op: UnaryOp::Neg,
                            expr: Box::new(e),
                        }
                    } else {
                        // Unary + is the identity on numeric types.
                        e
                    }))
                }
                // `- interval` negates every field. PG has no unary `+ interval`
                // operator, so that falls through to the error arm below.
                Binding::Typed(e)
                    if e.ty() == PgType::Interval && op == ast::UnaryOperator::Minus =>
                {
                    Ok(Binding::Typed(BoundExpr::FuncCall {
                        func: ScalarFn::IntervalNeg,
                        ret: PgType::Interval,
                        args: vec![e],
                    }))
                }
                // `- money` (`cash_um`); PG has no unary `+ money`, so that falls
                // through to the error arm below.
                Binding::Typed(e) if e.ty() == PgType::Money && op == ast::UnaryOperator::Minus => {
                    Ok(Binding::Typed(BoundExpr::FuncCall {
                        func: ScalarFn::CashUm,
                        ret: PgType::Money,
                        args: vec![e],
                    }))
                }
                Binding::Typed(e) => Err(no_op_unary(sym, e.ty().name())),
                // Every numeric type has this operator, so an untyped literal
                // cannot pick one — PG reports ambiguity.
                Binding::Unknown { .. } => Err(ambiguous_unary(sym)),
            }
        }
        // `@` absolute value: keeps the operand type.
        ast::UnaryOperator::PGAbs => match bind_expr(operand, scope)? {
            Binding::Typed(e) if e.ty().is_numeric() => Ok(Binding::Typed(BoundExpr::Unary {
                op: UnaryOp::Abs,
                expr: Box::new(e),
            })),
            Binding::Typed(e) => Err(no_op_unary("@", e.ty().name())),
            Binding::Unknown { .. } => Err(ambiguous_unary("@")),
        },
        ast::UnaryOperator::PGSquareRoot => bind_prefix_float8(UnaryOp::Sqrt, "|/", operand, scope),
        ast::UnaryOperator::PGCubeRoot => bind_prefix_float8(UnaryOp::Cbrt, "||/", operand, scope),
        ast::UnaryOperator::Not => {
            let operand = to_bool_operand(bind_expr(operand, scope)?, "NOT", operand.span())?;
            Ok(Binding::Typed(BoundExpr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(operand),
            }))
        }
        // `~inet` — bitwise NOT of the address (masklen preserved); `~bit` — the
        // bitwise complement of a bit string (length preserved).
        ast::UnaryOperator::BitwiseNot => match bind_expr(operand, scope)? {
            Binding::Typed(e) if matches!(e.ty(), PgType::Inet | PgType::Cidr) => {
                Ok(Binding::Typed(BoundExpr::FuncCall {
                    func: ScalarFn::InetNot,
                    ret: PgType::Inet,
                    args: vec![e],
                }))
            }
            Binding::Typed(e) if matches!(e.ty(), PgType::Bit | PgType::Varbit) => {
                // PG's `~` is defined only on `bit` (a varbit operand is cast in),
                // so the result type is `bit`.
                Ok(Binding::Typed(BoundExpr::FuncCall {
                    func: ScalarFn::BitNot,
                    ret: PgType::Bit,
                    args: vec![e],
                }))
            }
            // `~intN` — one's complement, same width back.
            Binding::Typed(e) if matches!(e.ty(), PgType::Int2 | PgType::Int4 | PgType::Int8) => {
                let ret = e.ty();
                Ok(Binding::Typed(BoundExpr::FuncCall {
                    func: ScalarFn::IntNot,
                    ret,
                    args: vec![e],
                }))
            }
            // `~macaddr` / `~macaddr8` — one's complement, same type back.
            Binding::Typed(e) if matches!(e.ty(), PgType::Macaddr | PgType::Macaddr8) => {
                let ret = e.ty();
                Ok(Binding::Typed(BoundExpr::FuncCall {
                    func: ScalarFn::MacaddrNot,
                    ret,
                    args: vec![e],
                }))
            }
            Binding::Typed(e) => Err(no_op_unary("~", e.ty().name())),
            Binding::Unknown { .. } => Err(ambiguous_unary("~")),
        },
        // Unary geometric operators: `@-@` length (lseg / path), `@@` center,
        // `?-` horizontal, `?|` vertical, `#` npoints (path).
        ast::UnaryOperator::AtDashAt
        | ast::UnaryOperator::DoubleAt
        | ast::UnaryOperator::QuestionDash
        | ast::UnaryOperator::QuestionPipe
        | ast::UnaryOperator::Hash => resolve_geometric_unary(op, bind_expr(operand, scope)?),
        // `!!` — prefix negation of a `tsquery`.
        ast::UnaryOperator::PGPrefixFactorial => resolve_ts_unary(bind_expr(operand, scope)?),
        other => Err(BindError::feature_not_supported(format!(
            "operator is not supported yet: {other}"
        ))),
    }
}

/// Unary geometric operators (`@-@`, `@@`, `?-`, `?|`, `#`) over the geometric
/// family. Returns the "operator does not exist" error for an operand whose type
/// has no such operator, and the ambiguity error for an untyped one.
fn resolve_geometric_unary(op: ast::UnaryOperator, operand: Binding) -> Result<Binding, BindError> {
    use crate::functions::GeoFn;
    let sym = op.to_string();
    let e = match operand {
        Binding::Typed(e) if is_geo_ty(Some(e.ty())) => e,
        Binding::Typed(e) => return Err(no_op_unary(&sym, e.ty().name())),
        Binding::Unknown { .. } => return Err(ambiguous_unary(&sym)),
    };
    let (func, ret) = match (op, e.ty()) {
        (ast::UnaryOperator::AtDashAt, PgType::Lseg) => (GeoFn::LsegLength, PgType::Float8),
        (ast::UnaryOperator::DoubleAt, PgType::Lseg) => (GeoFn::LsegCenter, PgType::Point),
        (ast::UnaryOperator::QuestionDash, PgType::Lseg) => (GeoFn::LsegHoriz, PgType::Bool),
        (ast::UnaryOperator::QuestionPipe, PgType::Lseg) => (GeoFn::LsegVert, PgType::Bool),
        (ast::UnaryOperator::AtDashAt, PgType::Path) => (GeoFn::PathLength, PgType::Float8),
        (ast::UnaryOperator::Hash, PgType::Path) => (GeoFn::PathNpoints, PgType::Int4),
        (ast::UnaryOperator::DoubleAt, PgType::Box) => (GeoFn::BoxCenter, PgType::Point),
        (ast::UnaryOperator::DoubleAt, PgType::Circle) => (GeoFn::CircleCenter, PgType::Point),
        (ast::UnaryOperator::DoubleAt, PgType::Polygon) => (GeoFn::PolyCenter, PgType::Point),
        (ast::UnaryOperator::Hash, PgType::Polygon) => (GeoFn::PolyNpoints, PgType::Int4),
        (ast::UnaryOperator::QuestionDash, PgType::Line) => (GeoFn::LineHoriz, PgType::Bool),
        (ast::UnaryOperator::QuestionPipe, PgType::Line) => (GeoFn::LineVert, PgType::Bool),
        (_, ty) => return Err(no_op_unary(&sym, ty.name())),
    };
    Ok(Binding::Typed(BoundExpr::FuncCall {
        func: ScalarFn::Geo(func),
        ret,
        args: vec![e],
    }))
}

/// `|/` / `||/`: coerce the operand to float8 (unknown → float8), producing a
/// float8 result.
fn bind_prefix_float8(
    uop: UnaryOp,
    sym: &str,
    operand: &ast::Expr,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let expr = match bind_expr(operand, scope)? {
        Binding::Typed(e) if e.ty().is_numeric() => coerce_expr(e, PgType::Float8)?,
        Binding::Typed(e) => return Err(no_op_unary(sym, e.ty().name())),
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, PgType::Float8)?,
    };
    Ok(Binding::Typed(BoundExpr::Unary {
        op: uop,
        expr: Box::new(expr),
    }))
}

pub(super) fn bind_is_null(
    inner: &ast::Expr,
    scope: &Scope,
    negated: bool,
) -> Result<Binding, BindError> {
    let expr = match bind_expr(inner, scope)? {
        Binding::Typed(e) => e,
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, PgType::Text)?,
    };
    Ok(Binding::Typed(BoundExpr::IsNull {
        expr: Box::new(expr),
        negated,
    }))
}

/// How a boolean test is spelled in SQL. This is the single source of truth for
/// the six spellings: the binder puts it in the `42804` the way PG names the
/// clause, and `explain_expr` prints it back into a plan.
pub fn bool_test_clause(value: Option<bool>, negated: bool) -> &'static str {
    match (value, negated) {
        (Some(true), false) => "IS TRUE",
        (Some(true), true) => "IS NOT TRUE",
        (Some(false), false) => "IS FALSE",
        (Some(false), true) => "IS NOT FALSE",
        (None, false) => "IS UNKNOWN",
        (None, true) => "IS NOT UNKNOWN",
    }
}

/// `IS [NOT] TRUE` / `IS [NOT] FALSE` / `IS [NOT] UNKNOWN`. Unlike `IS NULL`,
/// which accepts any type, these demand a boolean operand, and an untyped
/// literal takes boolean from here.
pub(super) fn bind_bool_test(
    inner: &ast::Expr,
    scope: &Scope,
    value: Option<bool>,
    negated: bool,
) -> Result<Binding, BindError> {
    let expr = to_bool_operand(
        bind_expr(inner, scope)?,
        bool_test_clause(value, negated),
        inner.span(),
    )?;
    Ok(Binding::Typed(BoundExpr::BoolTest {
        expr: Box::new(expr),
        value,
        negated,
    }))
}

pub(super) fn bind_binary(
    left: &ast::Expr,
    op: &ast::BinaryOperator,
    right: &ast::Expr,
    op_span: Span,
    scope: &Scope,
) -> Result<Binding, BindError> {
    // PG points its caret at the operator token for a 42883. The error is built
    // at dozens of sites deep inside the type-resolution walk, so rather than
    // thread the span through all of them, stamp it here on the way out.
    //
    // Only the operator resolver's *own* 42883 gets the caret, and only this
    // frame's. `blames_operator` is what identifies it — the SQLSTATE cannot,
    // since 42883 is also `function nosuchfn(integer) does not exist` raised
    // while binding an *operand* (PG points at `nosuchfn`) and the resolver's
    // `could not identify an equality operator for type json`, which PG leaves
    // unpositioned. The flag is cleared once stamped, so an error passing
    // outward through an enclosing operator keeps the innermost position.
    bind_binary_inner(left, op, right, op_span, scope).map_err(|mut e| {
        if !e.blames_operator {
            return e;
        }
        e.blames_operator = false;
        e.at(op_span)
    })
}

/// `(a, b) = (c, d)` and its `<>` twin.
///
/// PostgreSQL compares two rows field by field, and for these two operators
/// that is exactly a conjunction (resp. disjunction) of the per-field
/// comparisons — NULL semantics included. Probed against 18.4: `(1,NULL) =
/// (1,2)` is NULL and `(1,NULL) = (2,2)` is false, which `true AND NULL` and
/// `false AND NULL` reproduce; `<>` mirrors both through OR.
///
/// The ordering operators are deliberately absent: `<` on rows is
/// lexicographic, which no AND/OR chain expresses, so they keep falling through
/// to the unsupported-expression path rather than binding to something subtly
/// wrong.
fn bind_row_comparison(
    lhs: &[ast::Expr],
    op: &ast::BinaryOperator,
    rhs: &[ast::Expr],
    op_span: Span,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let combine = match op {
        ast::BinaryOperator::Eq => BinOp::And,
        ast::BinaryOperator::NotEq => BinOp::Or,
        _ => {
            return Err(BindError::feature_not_supported(format!(
                "comparison of row constructors with {} is not supported yet",
                op
            )));
        }
    };
    if lhs.len() != rhs.len() {
        return Err(BindError::new(
            sqlstate::SYNTAX_ERROR,
            "unequal number of entries in row expressions",
        ));
    }
    let mut combined: Option<BoundExpr> = None;
    for (l, r) in lhs.iter().zip(rhs) {
        // Through the ordinary operator path, so a field pair whose types have
        // no equality operator raises the 42883 it would outside a row.
        let field = to_bool_operand(
            bind_binary(l, op, r, op_span, scope)?,
            combine.sql_symbol(),
            op_span,
        )?;
        combined = Some(match combined {
            None => field,
            Some(acc) => BoundExpr::Binary {
                op: combine,
                arg_ty: PgType::Bool,
                collation: DEFAULT_COLLATION_OID,
                left: Box::new(acc),
                right: Box::new(field),
            },
        });
    }
    match combined {
        Some(expr) => Ok(Binding::Typed(expr)),
        // The parser has no production for `()`, so this is unreachable through
        // SQL; refusing beats inventing a truth value for an empty row.
        None => Err(BindError::syntax("row constructor has no entries")),
    }
}

fn bind_binary_inner(
    left: &ast::Expr,
    op: &ast::BinaryOperator,
    right: &ast::Expr,
    op_span: Span,
    scope: &Scope,
) -> Result<Binding, BindError> {
    // `a OPERATOR(schema.op) b` — PG's explicit-schema operator spelling. Only the
    // bare `op` or a `pg_catalog`-qualified name refers to a built-in operator; map
    // the symbol back to its native `BinaryOperator` and recurse so it reaches the
    // exact same path as the bare spelling (e.g. `~` -> `bind_regex`). A non-empty,
    // non-`pg_catalog` qualifier names no built-in operator, so it is reported as
    // 42883.
    // TODO: raise 3F000 `schema "x" does not exist` when the qualifier names no
    // schema, as PG does; the schema catalog is not reachable from the binder
    // scope, so that case collapses into the 42883 above.
    if let ast::BinaryOperator::PGCustomBinaryOperator(parts) = op {
        let symbol = match parts.as_slice() {
            [sym] => sym.as_str(),
            [schema, sym] if schema.eq_ignore_ascii_case("pg_catalog") => sym.as_str(),
            _ => return Err(custom_op_undefined(left, op, right, scope)),
        };
        let native = match symbol {
            "~" => ast::BinaryOperator::PGRegexMatch,
            "~*" => ast::BinaryOperator::PGRegexIMatch,
            "!~" => ast::BinaryOperator::PGRegexNotMatch,
            "!~*" => ast::BinaryOperator::PGRegexNotIMatch,
            "~~" => ast::BinaryOperator::PGLikeMatch,
            "~~*" => ast::BinaryOperator::PGILikeMatch,
            "!~~" => ast::BinaryOperator::PGNotLikeMatch,
            "!~~*" => ast::BinaryOperator::PGNotILikeMatch,
            "=" => ast::BinaryOperator::Eq,
            "<>" => ast::BinaryOperator::NotEq,
            "<" => ast::BinaryOperator::Lt,
            "<=" => ast::BinaryOperator::LtEq,
            ">" => ast::BinaryOperator::Gt,
            ">=" => ast::BinaryOperator::GtEq,
            "||" => ast::BinaryOperator::StringConcat,
            "+" => ast::BinaryOperator::Plus,
            "-" => ast::BinaryOperator::Minus,
            "*" => ast::BinaryOperator::Multiply,
            "/" => ast::BinaryOperator::Divide,
            "%" => ast::BinaryOperator::Modulo,
            "^" => ast::BinaryOperator::PGExp,
            "@>" => ast::BinaryOperator::AtArrow,
            "<@" => ast::BinaryOperator::ArrowAt,
            "&&" => ast::BinaryOperator::PGOverlap,
            "<<" => ast::BinaryOperator::PGBitwiseShiftLeft,
            ">>" => ast::BinaryOperator::PGBitwiseShiftRight,
            "&" => ast::BinaryOperator::BitwiseAnd,
            "|" => ast::BinaryOperator::BitwiseOr,
            "#" => ast::BinaryOperator::PGBitwiseXor,
            "@@" => ast::BinaryOperator::AtAt,
            "@?" => ast::BinaryOperator::AtQuestion,
            "->" => ast::BinaryOperator::Arrow,
            "->>" => ast::BinaryOperator::LongArrow,
            "#>" => ast::BinaryOperator::HashArrow,
            "#>>" => ast::BinaryOperator::HashLongArrow,
            _ => return Err(custom_op_undefined(left, op, right, scope)),
        };
        return bind_binary(left, &native, right, op_span, scope);
    }
    // After the `OPERATOR(...)` rewrite above, so the spelled-out form of a
    // row comparison reaches this too.
    if let (ast::Expr::Tuple(lhs), ast::Expr::Tuple(rhs)) = (left, right) {
        return bind_row_comparison(lhs, op, rhs, op_span, scope);
    }
    // `||` is not a `BinOp`; PG's `textcat`/`anytextcat` lower to a text concat,
    // and `bitcat` to a bit-string concat when either side is a bit string.
    if matches!(op, ast::BinaryOperator::StringConcat) {
        let lb = bind_expr(left, scope)?;
        let rb = bind_expr(right, scope)?;
        // Array concatenation (`array || array`, `array || element`,
        // `element || array`) when either side is a typed array.
        if binding_typed_ty(&lb).is_some_and(PgType::is_array)
            || binding_typed_ty(&rb).is_some_and(PgType::is_array)
        {
            return bind_array_concat(lb, rb);
        }
        // `tsvector || tsvector` unions the lexemes; `tsquery || tsquery` is an
        // OR. Both need a typed text-search operand, so a plain `text || text`
        // is untouched.
        if let Some(binding) = resolve_ts_concat(&lb, &rb)? {
            return Ok(binding);
        }
        // Route to bit concatenation only when neither side is a concrete
        // non-bit type — i.e. both operands are bit strings or untyped literals,
        // and at least one is a bit string. `bit || text` instead falls to the
        // text concat (PG's `anytextcat`), rendering the bit as its 0/1 string.
        let bit_or_unknown = |b: &Binding| {
            is_bit_family(binding_typed_ty(b)) || matches!(b, Binding::Unknown { .. })
        };
        if (is_bit_family(binding_typed_ty(&lb)) || is_bit_family(binding_typed_ty(&rb)))
            && bit_or_unknown(&lb)
            && bit_or_unknown(&rb)
        {
            return bind_bit_concat(lb, rb);
        }
        return bind_string_concat(lb, rb);
    }
    // The `~~`/`~~*`/`!~~`/`!~~*` operator spellings of LIKE / ILIKE.
    if let Some((ci, negated)) = match op {
        ast::BinaryOperator::PGLikeMatch => Some((false, false)),
        ast::BinaryOperator::PGILikeMatch => Some((true, false)),
        ast::BinaryOperator::PGNotLikeMatch => Some((false, true)),
        ast::BinaryOperator::PGNotILikeMatch => Some((true, true)),
        _ => None,
    } {
        let lb = bind_expr(left, scope)?;
        let rb = bind_expr(right, scope)?;
        return bind_like(lb, rb, None, ci, negated);
    }
    // The POSIX regex operators `~` / `~*` / `!~` / `!~*`.
    if let Some((ci, negated)) = match op {
        ast::BinaryOperator::PGRegexMatch => Some((false, false)),
        ast::BinaryOperator::PGRegexIMatch => Some((true, false)),
        ast::BinaryOperator::PGRegexNotMatch => Some((false, true)),
        ast::BinaryOperator::PGRegexNotIMatch => Some((true, true)),
        _ => None,
    } {
        let lb = bind_expr(left, scope)?;
        let rb = bind_expr(right, scope)?;
        return bind_regex(lb, rb, ci, negated);
    }

    let lb = bind_expr(left, scope)?;
    let rb = bind_expr(right, scope)?;

    // inet/cidr operators (containment, overlap, bitwise, host arithmetic) don't
    // fit the single-`arg_ty` `Binary` node; they lower to `ScalarFn` calls.
    // Tried before the generic mapping so `<<`/`>>`/`&`/`|`/`&&` reach here.
    if let Some(binding) = resolve_network_op(op, &lb, &rb)? {
        return Ok(binding);
    }

    // bit/varbit bitwise (`& | #`) and shift (`<< >>`) operators also lower to
    // `ScalarFn` calls. Tried after the network path so an inet operand still
    // wins; without a bit operand it falls through, so integer `&`/`|`/`<<`
    // reach `resolve_int_bitwise_op` below.
    if let Some(binding) = resolve_bit_op(op, &lb, &rb)? {
        return Ok(binding);
    }

    // `macaddr`/`macaddr8` bitwise `&`/`|` — like the inet operators, they don't
    // fit the single-`arg_ty` `Binary` node and lower to `ScalarFn` calls.
    if let Some(binding) = resolve_macaddr_op(op, &lb, &rb)? {
        return Ok(binding);
    }

    // Geometric operators (`point`/`lseg` distance, containment, arithmetic,
    // comparisons) don't fit the single-`arg_ty` `Binary` node either; they
    // lower to `ScalarFn::Geo` calls. Tried before the generic mapping so
    // `<<`/`>>`/`=`/`<` etc. reach here when a geometric operand is present.
    if let Some(binding) = resolve_geometric_op(op, &lb, &rb)? {
        return Ok(binding);
    }

    // `tsvector @@ tsquery` and the `tsquery` combinators. Placed before the
    // jsonpath and array resolvers, which claim `@@` and `&&` respectively. The
    // network and geometric resolvers run earlier and also claim `&&`/`<->`, but
    // each self-guards on its own operand types, and this one only fires on a
    // typed text-search operand — so no resolver shadows another.
    if let Some(binding) = resolve_ts_op(op, &lb, &rb)? {
        return Ok(binding);
    }

    // `jsonb @? jsonpath` / `jsonb @@ jsonpath` lower to the jsonpath query
    // functions (in silent mode). Tried before the generic mapping, which has no
    // arm for `@?`/`@@`.
    if let Some(binding) = resolve_jsonb_op(op, &lb, &rb)? {
        return Ok(binding);
    }

    // The `json`/`jsonb` extraction operators (`-> ->> #> #>>`). Nothing else in
    // the chain claims these spellings, so this resolver owns their errors too.
    if let Some(binding) = resolve_json_op(op, &lb, &rb, op_span)? {
        return Ok(binding);
    }

    // Array containment / overlap (`@>` `<@` `&&`) on array operands.
    if let Some(binding) = resolve_array_op(op, &lb, &rb, scope.catalog().as_ref())? {
        return Ok(binding);
    }

    // Integer bitwise (`& | #`) and shift (`<< >>`) operators. Deliberately the
    // last resolver: every spelling it claims is shared with the network, bit
    // and geometric families above, and this one never claims a typed
    // non-integer left operand, so putting it here means it can never shadow
    // them.
    if let Some(binding) = resolve_int_bitwise_op(op, &lb, &rb)? {
        return Ok(binding);
    }

    // The comparison spellings are shared with the quantified (`ANY`/`ALL`) path
    // so the two can never drift apart.
    if let Some(op) = binop_from_comparison(op) {
        return bind_binary_op(
            op,
            lb,
            rb,
            op_span,
            (left.span(), right.span()),
            scope.catalog().as_ref(),
        );
    }
    let op = match op {
        ast::BinaryOperator::And => BinOp::And,
        ast::BinaryOperator::Or => BinOp::Or,
        ast::BinaryOperator::Plus => BinOp::Add,
        ast::BinaryOperator::Minus => BinOp::Sub,
        ast::BinaryOperator::Multiply => BinOp::Mul,
        ast::BinaryOperator::Divide => BinOp::Div,
        ast::BinaryOperator::Modulo => BinOp::Mod,
        ast::BinaryOperator::PGExp => BinOp::Pow,
        other => {
            return Err(BindError::feature_not_supported(format!(
                "operator is not supported yet: {other}"
            )));
        }
    };
    bind_binary_op(
        op,
        lb,
        rb,
        op_span,
        (left.span(), right.span()),
        scope.catalog().as_ref(),
    )
}

/// Resolve a binary operator over two already-bound operands. Split out from
/// `bind_binary` so a simple `CASE operand WHEN v` can reuse the exact `=`
/// resolution (unknown-literal handling, numeric promotion, "operator does not
/// exist" errors) that a written `operand = v` gets. `op_span` locates the
/// operator token for an error cursor (`Span::empty()` when the caller has no
/// written operator, e.g. `CASE`/chained comparisons).
pub(crate) fn bind_binary_op(
    op: BinOp,
    lb: Binding,
    rb: Binding,
    op_span: Span,
    operand_spans: (Span, Span),
    catalog: &dyn TypeCatalog,
) -> Result<Binding, BindError> {
    if op.is_logic() {
        // `AND`/`OR` are also built by desugaring (BETWEEN, chained
        // comparisons); those callers pass empty spans and so print no cursor,
        // matching PG, which has no source position for them either.
        let left = to_bool_operand(lb, op.sql_symbol(), operand_spans.0)?;
        let right = to_bool_operand(rb, op.sql_symbol(), operand_spans.1)?;
        return Ok(Binding::Typed(BoundExpr::Binary {
            op,
            arg_ty: PgType::Bool,
            collation: DEFAULT_COLLATION_OID,
            left: Box::new(left),
            right: Box::new(right),
        }));
    }

    // `^` has only a float8 operator here: coerce both sides to float8.
    if op == BinOp::Pow {
        return bind_pow(lb, rb);
    }

    // Mixed-type temporal arithmetic (`ts - ts`, `ts ± interval`, `interval ±
    // interval`, `interval * / number`) doesn't fit the single-`arg_ty` `Binary`
    // node, so it lowers to a function call. Comparisons, and combinations with
    // no temporal operator at all, fall through to the generic path below.
    if let Some(binding) = resolve_temporal(op, &lb, &rb, op_span)? {
        return Ok(binding);
    }

    // Money arithmetic (money ± money, money * / int/float, money / money) is
    // not on the generic numeric path (money isn't `is_numeric`), so it lowers
    // to `ScalarFn` calls here. Comparisons fall through to the generic path.
    if let Some(binding) = resolve_money_op(op, &lb, &rb)? {
        return Ok(binding);
    }

    // `pg_lsn` arithmetic is likewise off the generic numeric path. Every
    // combination that *has* an operator is intercepted here; the rest (`lsn *
    // lsn`, `lsn / 2`) falls through to the arithmetic whitelist below, which
    // rejects them with `operator does not exist`.
    if let Some(binding) = resolve_pg_lsn_op(op, &lb, &rb)? {
        return Ok(binding);
    }

    // `xid = int4` / `xid <> int4`, which have no shared type to unify on.
    if let Some(binding) = resolve_xid_op(op, &lb, &rb)? {
        return Ok(binding);
    }

    // Comparison or arithmetic: settle both operands on one type. For
    // arithmetic, the typed side must offer the operator BEFORE the unknown
    // side is parsed as that type — PG reports `operator does not exist:
    // boolean + unknown`, never a coercion failure, when no operator applies.
    let (left, right, arg_ty) = match (lb, rb) {
        (Binding::Typed(l), Binding::Typed(r)) => unify_types(l, r, op, catalog)?,
        (Binding::Typed(l), Binding::Unknown { lit, span, param }) => {
            let ty = l.ty();
            if op.is_arithmetic() && !ty.is_numeric() {
                return Err(no_operator(&type_label(ty, catalog), op, "unknown"));
            }
            let r = resolve_unknown_ctx(catalog, lit, span, param, ty)?;
            (l, r, ty)
        }
        (Binding::Unknown { lit, span, param }, Binding::Typed(r)) => {
            let ty = r.ty();
            if op.is_arithmetic() && !ty.is_numeric() {
                return Err(no_operator("unknown", op, &type_label(ty, catalog)));
            }
            let l = resolve_unknown_ctx(catalog, lit, span, param, ty)?;
            (l, r, ty)
        }
        (
            Binding::Unknown {
                lit: ll,
                span: ls,
                param: lp,
            },
            Binding::Unknown {
                lit: rl,
                span: rs,
                param: rp,
            },
        ) => {
            if op.is_arithmetic() {
                // Every numeric type offers the operator; unknown operands
                // cannot pick one — PG reports ambiguity.
                return Err(ambiguous_operator("unknown", op.sql_symbol(), "unknown").at(op_span));
            }
            // Comparing two untyped literals: PG falls back to text.
            (
                resolve_unknown(ll, ls, lp, PgType::Text)?,
                resolve_unknown(rl, rs, rp, PgType::Text)?,
                PgType::Text,
            )
        }
    };

    // Admit only operators the executor actually implements for `arg_ty`, so a
    // bind never produces a node the evaluator can't handle. PG resolves against
    // a concrete operator catalog; this whitelist is our stand-in. `%` exists
    // for the integer types and `numeric` (PG has `numeric_mod`), but not float.
    let supported = if op.is_arithmetic() {
        let numeric_arith = matches!(
            arg_ty,
            PgType::Int2
                | PgType::Int4
                | PgType::Int8
                | PgType::Float4
                | PgType::Float8
                | PgType::Numeric
        );
        let mod_ok = op != BinOp::Mod
            || matches!(
                arg_ty,
                PgType::Int2 | PgType::Int4 | PgType::Int8 | PgType::Numeric
            );
        numeric_arith && mod_ok
    } else if matches!(op, BinOp::Eq | BinOp::NotEq) {
        // Equality reaches two types ordering does not — `xid` and `cid`, which
        // have a hash opclass but no btree one. Every other comparison stays on
        // `is_orderable`, so `'1'::xid < '2'::xid` still has no operator.
        //
        // `cid` is narrower still: PostgreSQL 18.4's operator catalog gives it
        // `=` and nothing else, not even `<>`. Probed, not assumed — `pg_operator`
        // lists one row for `(cid, cid)` against two for `(xid, xid)`.
        has_equality(arg_ty, catalog) && !(arg_ty == PgType::Cid && op == BinOp::NotEq)
    } else {
        is_orderable(arg_ty, catalog)
    };
    if !supported {
        let name = type_label(arg_ty, catalog);
        return Err(no_operator(&name, op, &name));
    }

    // A string comparison orders by the collation derived from its operands;
    // for every other type the collation is inert, so don't spend the walk.
    let collation = if arg_ty.is_collatable() {
        crate::collation::collation_for_comparison(&left, &right)?
    } else {
        DEFAULT_COLLATION_OID
    };
    Ok(Binding::Typed(BoundExpr::Binary {
        op,
        arg_ty,
        collation,
        left: Box::new(left),
        right: Box::new(right),
    }))
}

/// The DETAIL/HINT pair PG attaches to an `operator does not exist` (42883)
/// raised because the operator *name* is known but no candidate accepts these
/// operand types — which is every site below, since an unrecognized symbol never
/// reaches them. Shared so the constructors cannot drift apart.
///
/// PG words this case as a DETAIL plus a short HINT; the older single-HINT
/// phrasing ("No operator matches the given name and argument types…") belongs
/// to a form PG no longer emits here.
const NO_OPERATOR_DETAIL: &str = "No operator of that name accepts the given argument types.";
const NO_OPERATOR_HINT: &str = "You might need to add explicit type casts.";

/// Build the 42883 with PG's DETAIL/HINT pair. The caret is attached by the
/// caller — [`bind_binary`] stamps the operator token onto any 42883 leaving it
/// that has no position of its own, so the many construction sites do not each
/// have to thread a span.
fn undefined_operator_error(message: String) -> BindError {
    let mut e = BindError::new(sqlstate::UNDEFINED_FUNCTION, message)
        .with_detail(Some(NO_OPERATOR_DETAIL.to_string()))
        .with_hint(Some(NO_OPERATOR_HINT.to_string()));
    // Lets `bind_binary` recognise its own error on the way out; see there.
    e.blames_operator = true;
    e
}

fn no_operator(left: &str, op: BinOp, right: &str) -> BindError {
    undefined_operator_error(format!(
        "operator does not exist: {left} {} {right}",
        op.sql_symbol()
    ))
}

/// PG reports 42725 (with DETAIL/HINT) when more than one candidate operator
/// matches and none is clearly best — as opposed to `no_operator`'s 42883 when
/// no candidate exists at all. Every 42725 site shares the same DETAIL/HINT.
fn ambiguous_operator_msg(message: String) -> BindError {
    BindError::new(sqlstate::AMBIGUOUS_FUNCTION, message)
        .with_detail(Some(
            "Could not choose a best candidate operator.".to_string(),
        ))
        .with_hint(Some(
            "You might need to add explicit type casts.".to_string(),
        ))
}

fn ambiguous_operator(left: &str, sym: &str, right: &str) -> BindError {
    ambiguous_operator_msg(format!("operator is not unique: {left} {sym} {right}"))
}

/// Settle two typed operands on a common type: exact match, or numeric
/// promotion via a `Coerce` on the narrower side.
fn unify_types(
    left: BoundExpr,
    right: BoundExpr,
    op: BinOp,
    catalog: &dyn TypeCatalog,
) -> Result<(BoundExpr, BoundExpr, PgType), BindError> {
    let (lty, rty) = (left.ty(), right.ty());
    if lty == rty {
        return Ok((left, right, lty));
    }
    if let Some(common) = common_numeric(lty, rty) {
        let left = coerce_expr(left, common)?;
        let right = coerce_expr(right, common)?;
        return Ok((left, right, common));
    }
    // Non-numeric implicit cast (e.g. `timestamp` -> `timestamptz`): when one
    // side implicitly casts to the other, compare in that common type, as PG
    // does (`tstz = timestamp`). Numeric pairs are already handled above, so
    // this never changes numeric results.
    if implicit_castable(lty, rty) {
        return Ok((coerce_expr(left, rty)?, right, rty));
    }
    if implicit_castable(rty, lty) {
        return Ok((left, coerce_expr(right, lty)?, lty));
    }
    // Neither side casts to the other, but both may still reach a common third
    // type. PG resolves an operator by picking a candidate and implicitly
    // coercing both operands to its argument type, so `varchar = bpchar` binds
    // via `texteq` even though neither casts to the other directly. This is
    // deliberately *not* `select_common_type` (see `merge_types`): that one
    // requires a shared type category, which is why `"char"` unifies with
    // `varchar` for an operator but a UNION over the two still fails — exactly
    // as in PG, where `"char"` is category `Z` and `varchar` is `S`.
    //
    // `oid` is tried before `text`: it is how PG resolves `oideq` for a `reg*`
    // against an integer literal (`pg_type.typinput = 0`, `relnamespace = 11`).
    // `reg* -> text` is not an implicit cast, so the two can never both apply.
    if implicit_castable(lty, PgType::Oid) && implicit_castable(rty, PgType::Oid) {
        let left = coerce_expr(left, PgType::Oid)?;
        let right = coerce_expr(right, PgType::Oid)?;
        return Ok((left, right, PgType::Oid));
    }
    if implicit_castable(lty, PgType::Text) && implicit_castable(rty, PgType::Text) {
        let left = coerce_expr(left, PgType::Text)?;
        let right = coerce_expr(right, PgType::Text)?;
        return Ok((left, right, PgType::Text));
    }
    Err(no_operator(
        &type_label(lty, catalog),
        op,
        &type_label(rty, catalog),
    ))
}

/// Resolve mixed-type temporal arithmetic to a function call, or `Ok(None)` to
/// let the generic (same-type / comparison) path handle it — including the
/// `operator does not exist` error for combinations with no operator (e.g.
/// `interval * interval`). An untyped literal opposite a temporal operand takes
/// the partner type: interval for `±`, float8 for the `* /` factor. `op_span`
/// locates the operator for the one ambiguity this owns (`time + time`).
fn resolve_temporal(
    op: BinOp,
    lb: &Binding,
    rb: &Binding,
    op_span: Span,
) -> Result<Option<Binding>, BindError> {
    if !matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div) {
        return Ok(None);
    }
    let lt = binding_typed_ty(lb);
    let rt = binding_typed_ty(rb);
    let is_temporal = |t: Option<PgType>| {
        matches!(
            t,
            Some(
                PgType::Interval
                    | PgType::Timestamp
                    | PgType::TimestampTz
                    | PgType::Date
                    | PgType::Time
                    | PgType::TimeTz
            )
        )
    };
    if !is_temporal(lt) && !is_temporal(rt) {
        return Ok(None);
    }

    use PgType::{
        Date as D, Interval as I, Time as TI, TimeTz as TZ, Timestamp as T, TimestampTz as TSZ,
    };
    // Only int2/int4 pair with `date` (PG has `date + int4`; int2 widens to it).
    // int8 has no `date + bigint` operator, so it must fall through to an error.
    let is_int = |t: Option<PgType>| matches!(t, Some(PgType::Int2 | PgType::Int4));
    let typed = |b: &Binding| match b {
        Binding::Typed(e) => e.clone(),
        Binding::Unknown { .. } => unreachable!("typed side is Typed"),
    };
    let call = |func, ret, a: BoundExpr, b: BoundExpr| {
        Ok(Some(Binding::Typed(BoundExpr::FuncCall {
            func,
            ret,
            args: vec![a, b],
        })))
    };
    // A numeric operand (or an untyped literal) can be the `* /` factor.
    let factor_ok = |t: Option<PgType>| matches!(t, Some(ty) if ty.is_numeric()) || t.is_none();

    match op {
        BinOp::Add => match (lt, rt) {
            (Some(I), Some(I)) => call(ScalarFn::IntervalPl, I, typed(lb), typed(rb)),
            (Some(T), Some(I)) => call(ScalarFn::TimestampPlInterval, T, typed(lb), typed(rb)),
            (Some(I), Some(T)) => call(ScalarFn::TimestampPlInterval, T, typed(rb), typed(lb)),
            (Some(I), None) => call(ScalarFn::IntervalPl, I, typed(lb), resolve_operand(rb, I)?),
            (None, Some(I)) => call(ScalarFn::IntervalPl, I, resolve_operand(lb, I)?, typed(rb)),
            (Some(T), None) => call(
                ScalarFn::TimestampPlInterval,
                T,
                typed(lb),
                resolve_operand(rb, I)?,
            ),
            (None, Some(T)) => call(
                ScalarFn::TimestampPlInterval,
                T,
                typed(rb),
                resolve_operand(lb, I)?,
            ),
            // timestamptz + interval -> timestamptz. There is no
            // `timestamptz + timestamptz`, so an untyped literal opposite a
            // timestamptz can only be the interval.
            (Some(TSZ), Some(I)) => {
                call(ScalarFn::TimestampTzPlInterval, TSZ, typed(lb), typed(rb))
            }
            (Some(I), Some(TSZ)) => {
                call(ScalarFn::TimestampTzPlInterval, TSZ, typed(rb), typed(lb))
            }
            (Some(TSZ), None) => call(
                ScalarFn::TimestampTzPlInterval,
                TSZ,
                typed(lb),
                resolve_operand(rb, I)?,
            ),
            (None, Some(TSZ)) => call(
                ScalarFn::TimestampTzPlInterval,
                TSZ,
                typed(rb),
                resolve_operand(lb, I)?,
            ),
            // date + int -> date; date + interval -> timestamp; date + time -> timestamp.
            (Some(D), _) if is_int(rt) => call(
                ScalarFn::DatePlDays,
                D,
                typed(lb),
                resolve_operand(rb, PgType::Int4)?,
            ),
            (_, Some(D)) if is_int(lt) => call(
                ScalarFn::DatePlDays,
                D,
                typed(rb),
                resolve_operand(lb, PgType::Int4)?,
            ),
            (Some(D), Some(I)) => call(ScalarFn::DatePlInterval, T, typed(lb), typed(rb)),
            (Some(I), Some(D)) => call(ScalarFn::DatePlInterval, T, typed(rb), typed(lb)),
            (Some(D), Some(TI)) => call(ScalarFn::DatePlTime, T, typed(lb), typed(rb)),
            (Some(TI), Some(D)) => call(ScalarFn::DatePlTime, T, typed(rb), typed(lb)),
            // date + timetz -> timestamptz.
            (Some(D), Some(TZ)) => call(
                ScalarFn::DatePlTimeTz,
                PgType::TimestampTz,
                typed(lb),
                typed(rb),
            ),
            (Some(TZ), Some(D)) => call(
                ScalarFn::DatePlTimeTz,
                PgType::TimestampTz,
                typed(rb),
                typed(lb),
            ),
            // time + interval -> time; timetz + interval -> timetz.
            (Some(TI), Some(I)) => call(ScalarFn::TimePlInterval, TI, typed(lb), typed(rb)),
            (Some(I), Some(TI)) => call(ScalarFn::TimePlInterval, TI, typed(rb), typed(lb)),
            (Some(TZ), Some(I)) => call(ScalarFn::TimeTzPlInterval, TZ, typed(lb), typed(rb)),
            (Some(I), Some(TZ)) => call(ScalarFn::TimeTzPlInterval, TZ, typed(rb), typed(lb)),
            // `time + time`: PG reaches several candidate `+` operators via
            // implicit casts and can't pick a best one — ambiguous (42725), not
            // "does not exist". Unique to `time`: `timetz + timetz`, `date +
            // date`, `timestamp[tz] + timestamp[tz]` all stay 42883 (verified
            // against PG), so no other same-type add gets this treatment.
            (Some(TI), Some(TI)) => {
                let name = PgType::Time.name();
                Err(ambiguous_operator(name, "+", name).at(op_span))
            }
            _ => Ok(None),
        },
        BinOp::Sub => match (lt, rt) {
            (Some(I), Some(I)) => call(ScalarFn::IntervalMi, I, typed(lb), typed(rb)),
            (Some(T), Some(I)) => call(ScalarFn::TimestampMiInterval, T, typed(lb), typed(rb)),
            (Some(T), Some(T)) => call(ScalarFn::TimestampMi, I, typed(lb), typed(rb)),
            (Some(I), None) => call(ScalarFn::IntervalMi, I, typed(lb), resolve_operand(rb, I)?),
            (None, Some(I)) => call(ScalarFn::IntervalMi, I, resolve_operand(lb, I)?, typed(rb)),
            // For `timestamp - unknown`, PG resolves the literal to `timestamp`
            // (the preferred type), yielding timestamp - timestamp -> interval —
            // so `ts - '1 day'` errors as an invalid timestamp, matching PG,
            // while `ts - '<date>'` and `<date> - ts` produce an interval.
            (Some(T), None) => call(ScalarFn::TimestampMi, I, typed(lb), resolve_operand(rb, T)?),
            (None, Some(T)) => call(ScalarFn::TimestampMi, I, resolve_operand(lb, T)?, typed(rb)),
            (Some(TSZ), Some(I)) => {
                call(ScalarFn::TimestampTzMiInterval, TSZ, typed(lb), typed(rb))
            }
            // `timestamptz - {timestamptz, timestamp, date, unknown}` and the
            // reverses, all `timestamptz_mi -> interval`. `timestamptz` is the
            // preferred type of the datetime category and both
            // `timestamp -> timestamptz` and `date -> timestamptz` are implicit
            // casts, so a mixed pair widens the *other* side rather than
            // narrowing this one — and that widening stays a runtime `Coerce`
            // (`fold_needs_session`), which is what makes it read the executing
            // session's zone. `resolve_operand` is a no-op on the side that is
            // already a timestamptz.
            (Some(TSZ), _) | (_, Some(TSZ))
                if matches!(lt, Some(TSZ | T | D) | None)
                    && matches!(rt, Some(TSZ | T | D) | None) =>
            {
                call(
                    ScalarFn::TimestampTzMi,
                    I,
                    resolve_operand(lb, TSZ)?,
                    resolve_operand(rb, TSZ)?,
                )
            }
            // date - date -> int4; date - int -> date; date - interval -> timestamp.
            (Some(D), Some(D)) => call(ScalarFn::DateMi, PgType::Int4, typed(lb), typed(rb)),
            (Some(D), _) if is_int(rt) => call(
                ScalarFn::DateMiDays,
                D,
                typed(lb),
                resolve_operand(rb, PgType::Int4)?,
            ),
            (Some(D), Some(I)) => call(ScalarFn::DateMiInterval, T, typed(lb), typed(rb)),
            // date - timestamp / timestamp - date -> interval: widen the date to
            // a timestamp (midnight) and take the timestamp difference, as PG's
            // implicit date->timestamp cast does.
            (Some(D), Some(T)) => {
                call(ScalarFn::TimestampMi, I, resolve_operand(lb, T)?, typed(rb))
            }
            (Some(T), Some(D)) => {
                call(ScalarFn::TimestampMi, I, typed(lb), resolve_operand(rb, T)?)
            }
            (Some(D), None) => call(
                ScalarFn::DateMi,
                PgType::Int4,
                typed(lb),
                resolve_operand(rb, D)?,
            ),
            (None, Some(D)) => call(
                ScalarFn::DateMi,
                PgType::Int4,
                resolve_operand(lb, D)?,
                typed(rb),
            ),
            // time - time -> interval; time - interval -> time; timetz - interval -> timetz.
            (Some(TI), Some(TI)) => call(ScalarFn::TimeMi, I, typed(lb), typed(rb)),
            (Some(TI), Some(I)) => call(ScalarFn::TimeMiInterval, TI, typed(lb), typed(rb)),
            (Some(TZ), Some(I)) => call(ScalarFn::TimeTzMiInterval, TZ, typed(lb), typed(rb)),
            _ => Ok(None),
        },
        BinOp::Mul => match (lt, rt) {
            (Some(I), _) if factor_ok(rt) => call(
                ScalarFn::IntervalMul,
                I,
                typed(lb),
                resolve_operand(rb, PgType::Float8)?,
            ),
            (_, Some(I)) if factor_ok(lt) => call(
                ScalarFn::IntervalMul,
                I,
                typed(rb),
                resolve_operand(lb, PgType::Float8)?,
            ),
            _ => Ok(None),
        },
        BinOp::Div => match (lt, rt) {
            (Some(I), _) if factor_ok(rt) => call(
                ScalarFn::IntervalDiv,
                I,
                typed(lb),
                resolve_operand(rb, PgType::Float8)?,
            ),
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

pub(super) fn binding_typed_ty(b: &Binding) -> Option<PgType> {
    match b {
        Binding::Typed(e) => Some(e.ty()),
        Binding::Unknown { .. } => None,
    }
}

/// Resolve `xid = int4` / `xid <> int4` (PG's `xideqint4` / `xidneqint4`), or
/// `Ok(None)` to leave the pair to the generic path.
///
/// This needs its own resolver rather than an `implicit_castable` entry because
/// PG's operator catalog for it is deliberately narrow — all four properties
/// below were probed against PostgreSQL 18.4:
///
/// * only `=` and `<>`; `'1'::xid < 2` is `operator does not exist`;
/// * only with the `xid` on the **left**; `1 = '1'::xid` has no operator, so an
///   `implicit_castable` entry (which unifies symmetrically) would be wrong;
/// * only `int4` — `xid = 1::int8` has no operator, though `int2` reaches it by
///   the usual widening to `int4`;
/// * the int is compared as a raw bit pattern, not by value:
///   `'4294967295'::xid = -1` is **true**. That is why the operand is wrapped in
///   a [`BoundExpr::Reinterpret`] rather than a [`BoundExpr::Coerce`] — the bit
///   reinterpretation must not become reachable as a user-written cast, since
///   PG rejects `1::xid` with `cannot cast type integer to xid`.
///
/// An untyped literal opposite an `xid` already resolves through the ordinary
/// `xid = xid` path, so it is not handled here.
fn resolve_xid_op(op: BinOp, lb: &Binding, rb: &Binding) -> Result<Option<Binding>, BindError> {
    if !matches!(op, BinOp::Eq | BinOp::NotEq) {
        return Ok(None);
    }
    if binding_typed_ty(lb) != Some(PgType::Xid)
        || !matches!(binding_typed_ty(rb), Some(PgType::Int2 | PgType::Int4))
    {
        return Ok(None);
    }
    let (Binding::Typed(left), Binding::Typed(right)) = (lb, rb) else {
        unreachable!("both sides are typed");
    };
    // `int2` reaches the operator by first widening to `int4`, as in PG; the
    // reinterpretation itself is defined on the 32-bit value only.
    let right = coerce_expr(right.clone(), PgType::Int4)?;
    Ok(Some(Binding::Typed(BoundExpr::Binary {
        op,
        arg_ty: PgType::Xid,
        collation: DEFAULT_COLLATION_OID,
        left: Box::new(left.clone()),
        right: Box::new(BoundExpr::Reinterpret {
            expr: Box::new(right),
            reported: PgType::Xid,
            rep: PgType::Xid,
        }),
    })))
}

/// Lower `pg_lsn` arithmetic to a [`ScalarFn`] call, or `Ok(None)` to leave the
/// operands to the generic path (every comparison, and every combination with
/// no operator at all).
///
/// PG defines exactly four: `lsn - lsn -> numeric`, and `lsn ± numeric ->
/// pg_lsn` with `numeric + lsn` commuted. Two consequences that are easy to get
/// wrong, both probed against PostgreSQL 18.4:
///
/// * **An untyped literal resolves differently per operator.** `-` is the only
///   one with a `pg_lsn` on both sides, so a literal opposite a `pg_lsn` takes
///   `pg_lsn` there (`'0/2'::pg_lsn - '0/1'` is 1, not a numeric-input error)
///   but `numeric` under `+`, which has no `lsn + lsn` (`… + 16` works uncast).
/// * **A float operand has no operator at all.** `float8 -> numeric` is an
///   *assignment* cast, not an implicit one, so `pg_lsn + 1.5::float8` is
///   `operator does not exist` in PG. Only the exact numeric types coerce —
///   the same split `resolve_money_op` makes between its int and float cases.
///
/// Every call puts the `pg_lsn` operand first, so the executor's arms can read
/// `args[0]` as the LSN and `args[1]` as the numeric without re-checking.
fn resolve_pg_lsn_op(op: BinOp, lb: &Binding, rb: &Binding) -> Result<Option<Binding>, BindError> {
    use PgType::PgLsn as L;
    let lt = binding_typed_ty(lb);
    let rt = binding_typed_ty(rb);
    if lt != Some(L) && rt != Some(L) {
        return Ok(None);
    }
    // The types with an implicit cast to `numeric`. Deliberately excludes the
    // floats — see the doc comment.
    let exact_numeric = |t: PgType| {
        matches!(
            t,
            PgType::Int2 | PgType::Int4 | PgType::Int8 | PgType::Numeric
        )
    };
    // For `+`, an untyped literal joins them; for `-` it is handled separately.
    let counts_as_numeric = |t: Option<PgType>| t.is_none_or(exact_numeric);
    let typed = |b: &Binding| match b {
        Binding::Typed(e) => e.clone(),
        Binding::Unknown { .. } => unreachable!("typed side is Typed"),
    };
    let call = |func, ret, a: BoundExpr, b: BoundExpr| {
        Ok(Some(Binding::Typed(BoundExpr::FuncCall {
            func,
            ret,
            args: vec![a, b],
        })))
    };
    match op {
        // The `lsn - lsn` forms are matched before `lsn - numeric`, so the
        // result stays the exact signed distance rather than treating one side
        // as a count. An untyped literal on either side lands here.
        BinOp::Sub if lt == Some(L) && rt == Some(L) => {
            call(ScalarFn::PgLsnMi, PgType::Numeric, typed(lb), typed(rb))
        }
        BinOp::Sub if lt == Some(L) && rt.is_none() => call(
            ScalarFn::PgLsnMi,
            PgType::Numeric,
            typed(lb),
            resolve_operand(rb, L)?,
        ),
        BinOp::Sub if rt == Some(L) && lt.is_none() => call(
            ScalarFn::PgLsnMi,
            PgType::Numeric,
            resolve_operand(lb, L)?,
            typed(rb),
        ),
        BinOp::Sub if lt == Some(L) && rt.is_some_and(exact_numeric) => call(
            ScalarFn::PgLsnMii,
            L,
            typed(lb),
            resolve_operand(rb, PgType::Numeric)?,
        ),
        BinOp::Add if lt == Some(L) && counts_as_numeric(rt) => call(
            ScalarFn::PgLsnPli,
            L,
            typed(lb),
            resolve_operand(rb, PgType::Numeric)?,
        ),
        BinOp::Add if rt == Some(L) && counts_as_numeric(lt) => call(
            ScalarFn::PgLsnPli,
            L,
            typed(rb),
            resolve_operand(lb, PgType::Numeric)?,
        ),
        _ => Ok(None),
    }
}

/// Money arithmetic. `money` is deliberately not `is_numeric`, so it never
/// reaches the generic numeric path; its operators lower to `ScalarFn` calls
/// here, as `resolve_temporal`/`resolve_network_op` do for their types:
/// `money ± money -> money`; `money * intN` / `intN * money` / `money * floatN`
/// / `floatN * money -> money`; `money / intN -> money`; `money / floatN ->
/// money`; `money / money -> float8`. Returns `Ok(None)` when neither side is
/// money or the op/operand pair has no money operator, so the generic path (and
/// its comparisons and "operator does not exist" error) still applies. Every
/// call puts the money operand first and the factor/divisor second.

fn resolve_money_op(op: BinOp, lb: &Binding, rb: &Binding) -> Result<Option<Binding>, BindError> {
    use PgType::Money as M;
    let lt = binding_typed_ty(lb);
    let rt = binding_typed_ty(rb);
    if lt != Some(M) && rt != Some(M) {
        return Ok(None);
    }
    let is_int = |t: Option<PgType>| matches!(t, Some(PgType::Int2 | PgType::Int4 | PgType::Int8));
    let is_flt = |t: Option<PgType>| matches!(t, Some(PgType::Float4 | PgType::Float8));
    let typed = |b: &Binding| match b {
        Binding::Typed(e) => e.clone(),
        Binding::Unknown { .. } => unreachable!("typed side is Typed"),
    };
    let call = |func, ret, a: BoundExpr, b: BoundExpr| {
        Ok(Some(Binding::Typed(BoundExpr::FuncCall {
            func,
            ret,
            args: vec![a, b],
        })))
    };
    match op {
        // money ± money; an untyped literal opposite money is parsed as money.
        // money ± int/float has no operator in PG — fall through to the error.
        BinOp::Add | BinOp::Sub => {
            let func = if op == BinOp::Add {
                ScalarFn::CashPl
            } else {
                ScalarFn::CashMi
            };
            match (lt, rt) {
                (Some(M), Some(M)) => call(func, M, typed(lb), typed(rb)),
                (Some(M), None) => call(func, M, typed(lb), resolve_operand(rb, M)?),
                (None, Some(M)) => call(func, M, resolve_operand(lb, M)?, typed(rb)),
                _ => Ok(None),
            }
        }
        BinOp::Mul => match (lt, rt) {
            (Some(M), _) if is_int(rt) => call(
                ScalarFn::CashMulInt,
                M,
                typed(lb),
                resolve_operand(rb, PgType::Int8)?,
            ),
            (_, Some(M)) if is_int(lt) => call(
                ScalarFn::CashMulInt,
                M,
                typed(rb),
                resolve_operand(lb, PgType::Int8)?,
            ),
            (Some(M), _) if is_flt(rt) => call(
                ScalarFn::CashMulFlt,
                M,
                typed(lb),
                resolve_operand(rb, PgType::Float8)?,
            ),
            (_, Some(M)) if is_flt(lt) => call(
                ScalarFn::CashMulFlt,
                M,
                typed(rb),
                resolve_operand(lb, PgType::Float8)?,
            ),
            _ => Ok(None),
        },
        BinOp::Div => match (lt, rt) {
            (Some(M), Some(M)) => call(ScalarFn::CashDivCash, PgType::Float8, typed(lb), typed(rb)),
            (Some(M), _) if is_int(rt) => call(
                ScalarFn::CashDivInt,
                M,
                typed(lb),
                resolve_operand(rb, PgType::Int8)?,
            ),
            (Some(M), _) if is_flt(rt) => call(
                ScalarFn::CashDivFlt,
                M,
                typed(lb),
                resolve_operand(rb, PgType::Float8)?,
            ),
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

fn is_net_ty(t: Option<PgType>) -> bool {
    matches!(t, Some(PgType::Inet | PgType::Cidr))
}

/// The type name PG shows for an operand in an "operator does not exist"
/// message; an untyped literal is `unknown`.
fn operand_name(b: &Binding) -> &'static str {
    binding_typed_ty(b).map_or("unknown", |t| t.name())
}

/// `operator does not exist: <left> <op> <right>` (42883) for the operator
/// spellings that have no [`BinOp`] — the family resolvers' `@@`, `&&`, `<->`,
/// `>>`, ... Shared so a mis-typed operand reports a missing operator instead of
/// a cast failure from inside `coerce_expr`. Carries the DETAIL/HINT pair PG
/// attaches when the operator name exists but no candidate accepts these
/// operands; `custom_op_undefined` owns the other case, where the name itself is
/// unknown.
fn undefined_binary_operator(lb: &Binding, op: &ast::BinaryOperator, rb: &Binding) -> BindError {
    undefined_operator_error(format!(
        "operator does not exist: {} {op} {}",
        operand_name(lb),
        operand_name(rb)
    ))
}

/// 42883 for an `OPERATOR(schema.op)` spelling that names no built-in operator
/// (non-`pg_catalog` schema, or an unrecognized symbol). Binds the operands only
/// on this error path — so the normal path is never double-bound — and surfaces
/// an operand error (undefined column, bad cast, …) *first*, as PG does by
/// analyzing the operands before resolving the operator. The operator renders
/// schema-qualified (`pg_catalog.###`) like PG, not wrapped in `OPERATOR(...)`.
fn custom_op_undefined(
    left: &ast::Expr,
    op: &ast::BinaryOperator,
    right: &ast::Expr,
    scope: &Scope,
) -> BindError {
    let lb = match bind_expr(left, scope) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let rb = match bind_expr(right, scope) {
        Ok(b) => b,
        Err(e) => return e,
    };
    // PG names the operator as `schema.op` (or bare `op`), never `OPERATOR(...)`.
    let op_name = match op {
        ast::BinaryOperator::PGCustomBinaryOperator(parts) => parts.join("."),
        _ => op.to_string(),
    };
    // A different DETAIL from the type-mismatch sites, and no HINT: nothing here
    // names an operator that exists, so there are no casts to suggest.
    // TODO: emit PG's "An operator of that name exists, but it is not in the
    // search_path." DETAIL for a symbol defined in another schema — that needs
    // an operator catalog, so both cases collapse onto the DETAIL below, as the
    // SQLSTATE already does.
    BindError::new(
        sqlstate::UNDEFINED_FUNCTION,
        format!(
            "operator does not exist: {} {op_name} {}",
            operand_name(&lb),
            operand_name(&rb)
        ),
    )
    .with_detail(Some("There is no operator of that name.".to_string()))
}

/// Materialize a network operand: a typed inet/cidr as is (both read through
/// `inet_of`), an untyped literal parsed as `inet`. `None` for a typed non-net
/// operand, so the caller can report the full "operator does not exist" error.
fn net_operand(b: &Binding) -> Option<Result<BoundExpr, BindError>> {
    match b {
        Binding::Typed(e) if is_net_ty(Some(e.ty())) => Some(Ok(e.clone())),
        Binding::Unknown { lit, span, param } => Some(resolve_unknown(
            lit.clone(),
            *span,
            param.clone(),
            PgType::Inet,
        )),
        Binding::Typed(_) => None,
    }
}

/// Materialize the integer side of inet host arithmetic: a typed int2/int4/int8
/// coerced to int8, or an untyped literal parsed as int8. `None` for any other
/// typed operand — PG has only `inet ± bigint` (narrower ints widen), so e.g.
/// `inet + numeric`/`inet + text` must report "operator does not exist" rather
/// than silently coercing/truncating.
fn int_operand(b: &Binding) -> Option<Result<BoundExpr, BindError>> {
    match b {
        Binding::Typed(e) if matches!(e.ty(), PgType::Int2 | PgType::Int4 | PgType::Int8) => {
            Some(resolve_operand(b, PgType::Int8))
        }
        Binding::Unknown { .. } => Some(resolve_operand(b, PgType::Int8)),
        Binding::Typed(_) => None,
    }
}

/// inet/cidr-specific operators lower to `ScalarFn` calls (as `resolve_temporal`
/// does for the temporal operators). Returns `Ok(None)` when the operator and
/// operands are not a network operation, so the generic operator path — and its
/// errors — still applies.
fn resolve_network_op(
    op: &ast::BinaryOperator,
    lb: &Binding,
    rb: &Binding,
) -> Result<Option<Binding>, BindError> {
    use ast::BinaryOperator as B;
    let lt = binding_typed_ty(lb);
    let rt = binding_typed_ty(rb);
    let any_net = is_net_ty(lt) || is_net_ty(rt);
    let call = |func, ret, a, b| {
        Ok(Some(Binding::Typed(BoundExpr::FuncCall {
            func,
            ret,
            args: vec![a, b],
        })))
    };

    // Containment / overlap (`<<` `>>` `&&`) and bitwise (`&` `|`) take two
    // inet-family operands (result bool / inet). Without any net operand, fall
    // through so integer `&`/`|`/`<<` reach `resolve_int_bitwise_op`.
    let net_net = match op {
        B::PGBitwiseShiftLeft => Some((ScalarFn::NetworkContainedBy, PgType::Bool)),
        B::PGBitwiseShiftRight => Some((ScalarFn::NetworkContains, PgType::Bool)),
        B::PGOverlap => Some((ScalarFn::NetworkOverlaps, PgType::Bool)),
        B::BitwiseAnd => Some((ScalarFn::InetAnd, PgType::Inet)),
        B::BitwiseOr => Some((ScalarFn::InetOr, PgType::Inet)),
        _ => None,
    };
    if let Some((func, ret)) = net_net {
        if !any_net {
            return Ok(None);
        }
        let (Some(a), Some(b)) = (net_operand(lb), net_operand(rb)) else {
            return Err(undefined_binary_operator(lb, op, rb));
        };
        return call(func, ret, a?, b?);
    }

    // Host arithmetic: `inet ± int8` (commutative for `+`), `inet - inet`.
    match op {
        B::Plus if is_net_ty(lt) && !is_net_ty(rt) => {
            let (Some(a), Some(n)) = (net_operand(lb), int_operand(rb)) else {
                return Err(undefined_binary_operator(lb, op, rb));
            };
            call(ScalarFn::InetPlInt8, PgType::Inet, a?, n?)
        }
        B::Plus if is_net_ty(rt) && !is_net_ty(lt) => {
            let (Some(a), Some(n)) = (net_operand(rb), int_operand(lb)) else {
                return Err(undefined_binary_operator(lb, op, rb));
            };
            call(ScalarFn::InetPlInt8, PgType::Inet, a?, n?)
        }
        B::Minus if is_net_ty(lt) && is_net_ty(rt) => {
            let (Some(a), Some(b)) = (net_operand(lb), net_operand(rb)) else {
                return Err(undefined_binary_operator(lb, op, rb));
            };
            call(ScalarFn::InetMi, PgType::Int8, a?, b?)
        }
        B::Minus if is_net_ty(lt) => {
            let (Some(a), Some(n)) = (net_operand(lb), int_operand(rb)) else {
                return Err(undefined_binary_operator(lb, op, rb));
            };
            call(ScalarFn::InetMiInt8, PgType::Inet, a?, n?)
        }
        _ => Ok(None),
    }
}

/// Whether `ty` is one of the geometric types.
pub(super) fn is_geo_ty(ty: Option<PgType>) -> bool {
    matches!(
        ty,
        Some(
            PgType::Point
                | PgType::Lseg
                | PgType::Path
                | PgType::Box
                | PgType::Line
                | PgType::Circle
                | PgType::Polygon
        )
    )
}

/// Every geometric type, in the order an untyped operand is tried against them.
/// `Point` leads because most mixed-type overloads pair a shape with a point.
const GEO_TYPES: [PgType; 7] = [
    PgType::Point,
    PgType::Lseg,
    PgType::Line,
    PgType::Box,
    PgType::Path,
    PgType::Polygon,
    PgType::Circle,
];

/// Geometric binary operators lower to `ScalarFn::Geo` calls, as
/// `resolve_network_op` does for the inet operators. Returns `Ok(None)` when
/// no geometric operand is present (so the generic path and its errors apply) or
/// when the operator/operand-type combination has no geometric operator.
fn resolve_geometric_op(
    op: &ast::BinaryOperator,
    lb: &Binding,
    rb: &Binding,
) -> Result<Option<Binding>, BindError> {
    let lt = binding_typed_ty(lb);
    let rt = binding_typed_ty(rb);
    if !is_geo_ty(lt) && !is_geo_ty(rt) {
        return Ok(None);
    }
    // Both sides typed: exactly one combination to try.
    if lt.is_some() && rt.is_some() {
        return resolve_geometric_combo(op, lb, rb, lt, rt);
    }
    // One side is an untyped literal. It first mirrors the other (geometric)
    // side's type, which is what PG's "assume the unknown is the other operand's
    // type" step does: `path <-> '(5,5)'` really does bind `path <-> path`,
    // reading the literal as the one-point path `((5,5))`. If that combination
    // has no operator, try the remaining geometric types — the overload may take
    // any of them on the other side (`path @> point`, `line ## lseg`, ...), so
    // hardcoding `point` as the only fallback would fail to bind operators that
    // are implemented.
    let mirrored = lt.or(rt);
    for candidate in std::iter::once(mirrored).chain(GEO_TYPES.map(Some)) {
        let (left_ty, right_ty) = (lt.or(candidate), rt.or(candidate));
        if let Some(bound) = resolve_geometric_combo(op, lb, rb, left_ty, right_ty)? {
            return Ok(Some(bound));
        }
    }
    Ok(None)
}

/// The geometric operator table for one fully-resolved operand-type pair.
/// `Ok(None)` means this operator has no overload for that combination.
fn resolve_geometric_combo(
    op: &ast::BinaryOperator,
    lb: &Binding,
    rb: &Binding,
    left_ty: Option<PgType>,
    right_ty: Option<PgType>,
) -> Result<Option<Binding>, BindError> {
    use crate::functions::GeoFn;
    use ast::BinaryOperator as B;
    let l = |t: PgType| resolve_operand(lb, t);
    let r = |t: PgType| resolve_operand(rb, t);
    let call = |func, ret, args: Vec<BoundExpr>| {
        Ok(Some(Binding::Typed(BoundExpr::FuncCall {
            func,
            ret,
            args,
        })))
    };
    let geo = |f: GeoFn| ScalarFn::Geo(f);

    use PgType::{Lseg, Path, Point};
    let (Some(left_ty), Some(right_ty)) = (left_ty, right_ty) else {
        return Ok(None);
    };
    let combo = (left_ty, right_ty);
    match op {
        // Distance (`<->`).
        B::LtDashGt => match combo {
            (Point, Point) => call(
                geo(GeoFn::PointDist),
                PgType::Float8,
                vec![l(Point)?, r(Point)?],
            ),
            (Point, Lseg) => call(
                geo(GeoFn::DistPointSeg),
                PgType::Float8,
                vec![l(Point)?, r(Lseg)?],
            ),
            (Lseg, Point) => call(
                geo(GeoFn::DistPointSeg),
                PgType::Float8,
                vec![r(Point)?, l(Lseg)?],
            ),
            (Lseg, Lseg) => call(
                geo(GeoFn::DistSegSeg),
                PgType::Float8,
                vec![l(Lseg)?, r(Lseg)?],
            ),
            (Path, Path) => call(
                geo(GeoFn::PathDist),
                PgType::Float8,
                vec![l(Path)?, r(Path)?],
            ),
            (Path, Point) => call(
                geo(GeoFn::DistPathPoint),
                PgType::Float8,
                vec![l(Path)?, r(Point)?],
            ),
            (Point, Path) => call(
                geo(GeoFn::DistPathPoint),
                PgType::Float8,
                vec![r(Path)?, l(Point)?],
            ),
            (PgType::Box, PgType::Box) => call(
                geo(GeoFn::DistBoxBox),
                PgType::Float8,
                vec![l(PgType::Box)?, r(PgType::Box)?],
            ),
            (Point, PgType::Box) => call(
                geo(GeoFn::DistPointBox),
                PgType::Float8,
                vec![l(Point)?, r(PgType::Box)?],
            ),
            (PgType::Box, Point) => call(
                geo(GeoFn::DistPointBox),
                PgType::Float8,
                vec![r(Point)?, l(PgType::Box)?],
            ),
            (Lseg, PgType::Box) => call(
                geo(GeoFn::DistLsegBox),
                PgType::Float8,
                vec![l(Lseg)?, r(PgType::Box)?],
            ),
            (PgType::Box, Lseg) => call(
                geo(GeoFn::DistLsegBox),
                PgType::Float8,
                vec![r(Lseg)?, l(PgType::Box)?],
            ),
            (PgType::Line, PgType::Line) => call(
                geo(GeoFn::DistLineLine),
                PgType::Float8,
                vec![l(PgType::Line)?, r(PgType::Line)?],
            ),
            (Point, PgType::Line) => call(
                geo(GeoFn::DistPointLine),
                PgType::Float8,
                vec![l(Point)?, r(PgType::Line)?],
            ),
            (PgType::Line, Point) => call(
                geo(GeoFn::DistPointLine),
                PgType::Float8,
                vec![r(Point)?, l(PgType::Line)?],
            ),
            (Lseg, PgType::Line) => call(
                geo(GeoFn::DistLsegLine),
                PgType::Float8,
                vec![l(Lseg)?, r(PgType::Line)?],
            ),
            (PgType::Line, Lseg) => call(
                geo(GeoFn::DistLsegLine),
                PgType::Float8,
                vec![r(Lseg)?, l(PgType::Line)?],
            ),
            (PgType::Circle, PgType::Circle) => call(
                geo(GeoFn::DistCircleCircle),
                PgType::Float8,
                vec![l(PgType::Circle)?, r(PgType::Circle)?],
            ),
            (Point, PgType::Circle) => call(
                geo(GeoFn::DistPointCircle),
                PgType::Float8,
                vec![l(Point)?, r(PgType::Circle)?],
            ),
            (PgType::Circle, Point) => call(
                geo(GeoFn::DistPointCircle),
                PgType::Float8,
                vec![r(Point)?, l(PgType::Circle)?],
            ),
            (PgType::Polygon, PgType::Polygon) => call(
                geo(GeoFn::DistPolyPoly),
                PgType::Float8,
                vec![l(PgType::Polygon)?, r(PgType::Polygon)?],
            ),
            (PgType::Polygon, Point) => call(
                geo(GeoFn::DistPolyPoint),
                PgType::Float8,
                vec![l(PgType::Polygon)?, r(Point)?],
            ),
            (Point, PgType::Polygon) => call(
                geo(GeoFn::DistPolyPoint),
                PgType::Float8,
                vec![r(PgType::Polygon)?, l(Point)?],
            ),
            (PgType::Polygon, PgType::Circle) => call(
                geo(GeoFn::DistPolyCircle),
                PgType::Float8,
                vec![l(PgType::Polygon)?, r(PgType::Circle)?],
            ),
            (PgType::Circle, PgType::Polygon) => call(
                geo(GeoFn::DistPolyCircle),
                PgType::Float8,
                vec![r(PgType::Polygon)?, l(PgType::Circle)?],
            ),
            _ => Ok(None),
        },
        // Point positional / same-as / horizontal / vertical predicates.
        B::PGBitwiseShiftLeft if combo == (Point, Point) => call(
            geo(GeoFn::PointLeft),
            PgType::Bool,
            vec![l(Point)?, r(Point)?],
        ),
        B::PGBitwiseShiftRight if combo == (Point, Point) => call(
            geo(GeoFn::PointRight),
            PgType::Bool,
            vec![l(Point)?, r(Point)?],
        ),
        B::PipeGtGt if combo == (Point, Point) => call(
            geo(GeoFn::PointAbove),
            PgType::Bool,
            vec![l(Point)?, r(Point)?],
        ),
        B::LtLtPipe if combo == (Point, Point) => call(
            geo(GeoFn::PointBelow),
            PgType::Bool,
            vec![l(Point)?, r(Point)?],
        ),
        B::TildeEq if combo == (Point, Point) => call(
            geo(GeoFn::PointEq),
            PgType::Bool,
            vec![l(Point)?, r(Point)?],
        ),
        B::QuestionDash if combo == (Point, Point) => call(
            geo(GeoFn::PointHoriz),
            PgType::Bool,
            vec![l(Point)?, r(Point)?],
        ),
        B::QuestionPipe if combo == (Point, Point) => call(
            geo(GeoFn::PointVert),
            PgType::Bool,
            vec![l(Point)?, r(Point)?],
        ),
        // Point arithmetic (`-> point`).
        B::Plus if combo == (Point, Point) => call(
            geo(GeoFn::PointAdd),
            PgType::Point,
            vec![l(Point)?, r(Point)?],
        ),
        B::Minus if combo == (Point, Point) => call(
            geo(GeoFn::PointSub),
            PgType::Point,
            vec![l(Point)?, r(Point)?],
        ),
        B::Multiply if combo == (Point, Point) => call(
            geo(GeoFn::PointMul),
            PgType::Point,
            vec![l(Point)?, r(Point)?],
        ),
        B::Divide if combo == (Point, Point) => call(
            geo(GeoFn::PointDiv),
            PgType::Point,
            vec![l(Point)?, r(Point)?],
        ),
        // Path arithmetic: `path + path` concatenates, and a point operand
        // translates / rotates / scales every vertex (`-> path`).
        B::Plus if combo == (Path, Path) => {
            call(geo(GeoFn::PathConcat), Path, vec![l(Path)?, r(Path)?])
        }
        B::Plus if combo == (Path, Point) => {
            call(geo(GeoFn::PathAddPt), Path, vec![l(Path)?, r(Point)?])
        }
        B::Minus if combo == (Path, Point) => {
            call(geo(GeoFn::PathSubPt), Path, vec![l(Path)?, r(Point)?])
        }
        B::Multiply if combo == (Path, Point) => {
            call(geo(GeoFn::PathMulPt), Path, vec![l(Path)?, r(Point)?])
        }
        B::Divide if combo == (Path, Point) => {
            call(geo(GeoFn::PathDivPt), Path, vec![l(Path)?, r(Point)?])
        }
        // `point <@ lseg` / `point <@ path`, and `path @> point`.
        B::ArrowAt if combo == (Point, Lseg) => call(
            geo(GeoFn::PointOnSeg),
            PgType::Bool,
            vec![l(Point)?, r(Lseg)?],
        ),
        B::ArrowAt if combo == (Point, Path) => {
            call(geo(GeoFn::OnPpath), PgType::Bool, vec![l(Point)?, r(Path)?])
        }
        B::AtArrow if combo == (Path, Point) => call(
            geo(GeoFn::PathContainPt),
            PgType::Bool,
            vec![l(Path)?, r(Point)?],
        ),
        // `path ?# path`: the two outlines cross.
        B::QuestionHash if combo == (Path, Path) => call(
            geo(GeoFn::PathInter),
            PgType::Bool,
            vec![l(Path)?, r(Path)?],
        ),
        // `##` closest point: the answer sits on the 2nd operand — for
        // `line ## lseg` that is the segment, not the infinite line. The box
        // pairs are the exception: when the operands overlap, `lseg ## box`
        // answers the segment point nearest the box centre and `point ## box`
        // the point itself, as PG does.
        B::DoubleHash => match combo {
            (Point, Lseg) => call(
                geo(GeoFn::ClosePointSeg),
                PgType::Point,
                vec![l(Point)?, r(Lseg)?],
            ),
            (Lseg, Lseg) => call(
                geo(GeoFn::CloseSegSeg),
                PgType::Point,
                vec![l(Lseg)?, r(Lseg)?],
            ),
            (Point, PgType::Box) => call(
                geo(GeoFn::ClosePointBox),
                PgType::Point,
                vec![l(Point)?, r(PgType::Box)?],
            ),
            (Lseg, PgType::Box) => call(
                geo(GeoFn::CloseLsegBox),
                PgType::Point,
                vec![l(Lseg)?, r(PgType::Box)?],
            ),
            (Point, PgType::Line) => call(
                geo(GeoFn::ClosePointLine),
                PgType::Point,
                vec![l(Point)?, r(PgType::Line)?],
            ),
            (PgType::Line, Lseg) => call(
                geo(GeoFn::CloseLineLseg),
                PgType::Point,
                vec![l(PgType::Line)?, r(Lseg)?],
            ),
            _ => Ok(None),
        },
        // `#` intersection point of two segments (NULL if none).
        B::PGBitwiseXor if combo == (Lseg, Lseg) => call(
            geo(GeoFn::LsegInterpt),
            PgType::Point,
            vec![l(Lseg)?, r(Lseg)?],
        ),
        // lseg parallel / perpendicular.
        B::QuestionDoublePipe if combo == (Lseg, Lseg) => call(
            geo(GeoFn::LsegParallel),
            PgType::Bool,
            vec![l(Lseg)?, r(Lseg)?],
        ),
        B::QuestionDashPipe if combo == (Lseg, Lseg) => call(
            geo(GeoFn::LsegPerpendicular),
            PgType::Bool,
            vec![l(Lseg)?, r(Lseg)?],
        ),
        // lseg b-tree comparisons (`=`/`<>` by endpoints, the rest by length).
        B::Eq if combo == (Lseg, Lseg) => {
            call(geo(GeoFn::LsegEq), PgType::Bool, vec![l(Lseg)?, r(Lseg)?])
        }
        B::NotEq if combo == (Lseg, Lseg) => {
            call(geo(GeoFn::LsegNe), PgType::Bool, vec![l(Lseg)?, r(Lseg)?])
        }
        B::Lt if combo == (Lseg, Lseg) => {
            call(geo(GeoFn::LsegLt), PgType::Bool, vec![l(Lseg)?, r(Lseg)?])
        }
        B::LtEq if combo == (Lseg, Lseg) => {
            call(geo(GeoFn::LsegLe), PgType::Bool, vec![l(Lseg)?, r(Lseg)?])
        }
        B::Gt if combo == (Lseg, Lseg) => {
            call(geo(GeoFn::LsegGt), PgType::Bool, vec![l(Lseg)?, r(Lseg)?])
        }
        B::GtEq if combo == (Lseg, Lseg) => {
            call(geo(GeoFn::LsegGe), PgType::Bool, vec![l(Lseg)?, r(Lseg)?])
        }
        // path b-tree comparisons — all six compare the *number of points*, so
        // `'[(0,0),(1,1)]' = '((5,5),(6,6))'` is true.
        B::Eq if combo == (Path, Path) => {
            call(geo(GeoFn::PathEq), PgType::Bool, vec![l(Path)?, r(Path)?])
        }
        B::NotEq if combo == (Path, Path) => {
            call(geo(GeoFn::PathNe), PgType::Bool, vec![l(Path)?, r(Path)?])
        }
        B::Lt if combo == (Path, Path) => {
            call(geo(GeoFn::PathLt), PgType::Bool, vec![l(Path)?, r(Path)?])
        }
        B::LtEq if combo == (Path, Path) => {
            call(geo(GeoFn::PathLe), PgType::Bool, vec![l(Path)?, r(Path)?])
        }
        B::Gt if combo == (Path, Path) => {
            call(geo(GeoFn::PathGt), PgType::Bool, vec![l(Path)?, r(Path)?])
        }
        B::GtEq if combo == (Path, Path) => {
            call(geo(GeoFn::PathGe), PgType::Bool, vec![l(Path)?, r(Path)?])
        }
        // `box` positional / containment / identity predicates (all box × box).
        B::PGOverlap if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxOverlap),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        B::AndLt if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxOverLeft),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        B::AndGt if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxOverRight),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        B::AndLtPipe if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxOverBelow),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        B::PipeAndGt if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxOverAbove),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        B::PGBitwiseShiftLeft if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxLeft),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        B::PGBitwiseShiftRight if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxRight),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        B::LtLtPipe if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxBelow),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        B::PipeGtGt if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxAbove),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        B::LtCaret if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxBelowEq),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        B::GtCaret if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxAboveEq),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        B::ArrowAt if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxContained),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        B::AtArrow if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxContain),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        B::TildeEq if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxSame),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        B::QuestionHash if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxIntersects),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        // `b1 # b2` is the intersection box (NULL when they are disjoint).
        B::PGBitwiseXor if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxIntersect),
            PgType::Box,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        // `box` comparisons are by **area**, so two differently placed boxes of the
        // same size compare equal; identity is `~=` above.
        B::Eq if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxEq),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        B::Lt if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxLt),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        B::LtEq if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxLe),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        B::Gt if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxGt),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        B::GtEq if combo == (PgType::Box, PgType::Box) => call(
            geo(GeoFn::BoxGe),
            PgType::Bool,
            vec![l(PgType::Box)?, r(PgType::Box)?],
        ),
        // `box <op> point`: move / rotate / scale both corners.
        B::Plus if combo == (PgType::Box, Point) => call(
            geo(GeoFn::BoxAddPt),
            PgType::Box,
            vec![l(PgType::Box)?, r(Point)?],
        ),
        B::Minus if combo == (PgType::Box, Point) => call(
            geo(GeoFn::BoxSubPt),
            PgType::Box,
            vec![l(PgType::Box)?, r(Point)?],
        ),
        B::Multiply if combo == (PgType::Box, Point) => call(
            geo(GeoFn::BoxMulPt),
            PgType::Box,
            vec![l(PgType::Box)?, r(Point)?],
        ),
        B::Divide if combo == (PgType::Box, Point) => call(
            geo(GeoFn::BoxDivPt),
            PgType::Box,
            vec![l(PgType::Box)?, r(Point)?],
        ),
        // `box @> point` / `point <@ box`, and the point/segment × box geometry.
        B::AtArrow if combo == (PgType::Box, Point) => call(
            geo(GeoFn::BoxContainPt),
            PgType::Bool,
            vec![l(PgType::Box)?, r(Point)?],
        ),
        B::ArrowAt if combo == (Point, PgType::Box) => call(
            geo(GeoFn::BoxContainPt),
            PgType::Bool,
            vec![r(PgType::Box)?, l(Point)?],
        ),
        B::ArrowAt if combo == (Lseg, PgType::Box) => call(
            geo(GeoFn::LsegInsideBox),
            PgType::Bool,
            vec![l(Lseg)?, r(PgType::Box)?],
        ),
        B::QuestionHash if combo == (Lseg, PgType::Box) => call(
            geo(GeoFn::LsegIntersectsBox),
            PgType::Bool,
            vec![l(Lseg)?, r(PgType::Box)?],
        ),
        // `line`: equality is scale invariant; `?#`/`?-|`/`?||` are the relations.
        B::Eq if combo == (PgType::Line, PgType::Line) => call(
            geo(GeoFn::LineEq),
            PgType::Bool,
            vec![l(PgType::Line)?, r(PgType::Line)?],
        ),
        B::QuestionHash if combo == (PgType::Line, PgType::Line) => call(
            geo(GeoFn::LineIntersects),
            PgType::Bool,
            vec![l(PgType::Line)?, r(PgType::Line)?],
        ),
        B::QuestionDashPipe if combo == (PgType::Line, PgType::Line) => call(
            geo(GeoFn::LinePerpendicular),
            PgType::Bool,
            vec![l(PgType::Line)?, r(PgType::Line)?],
        ),
        B::QuestionDoublePipe if combo == (PgType::Line, PgType::Line) => call(
            geo(GeoFn::LineParallel),
            PgType::Bool,
            vec![l(PgType::Line)?, r(PgType::Line)?],
        ),
        B::PGBitwiseXor if combo == (PgType::Line, PgType::Line) => call(
            geo(GeoFn::LineInterpt),
            PgType::Point,
            vec![l(PgType::Line)?, r(PgType::Line)?],
        ),
        // point / lseg / box against a line.
        B::ArrowAt if combo == (Point, PgType::Line) => call(
            geo(GeoFn::PointOnLine),
            PgType::Bool,
            vec![l(Point)?, r(PgType::Line)?],
        ),
        B::ArrowAt if combo == (Lseg, PgType::Line) => call(
            geo(GeoFn::LsegOnLine),
            PgType::Bool,
            vec![l(Lseg)?, r(PgType::Line)?],
        ),
        B::QuestionHash if combo == (Lseg, PgType::Line) => call(
            geo(GeoFn::LsegIntersectsLine),
            PgType::Bool,
            vec![l(Lseg)?, r(PgType::Line)?],
        ),
        B::QuestionHash if combo == (PgType::Line, PgType::Box) => call(
            geo(GeoFn::LineIntersectsBox),
            PgType::Bool,
            vec![l(PgType::Line)?, r(PgType::Box)?],
        ),
        // `circle` positional / containment / identity predicates.
        B::PGOverlap if combo == (PgType::Circle, PgType::Circle) => call(
            geo(GeoFn::CircleOverlap),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(PgType::Circle)?],
        ),
        B::AndLt if combo == (PgType::Circle, PgType::Circle) => call(
            geo(GeoFn::CircleOverLeft),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(PgType::Circle)?],
        ),
        B::AndGt if combo == (PgType::Circle, PgType::Circle) => call(
            geo(GeoFn::CircleOverRight),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(PgType::Circle)?],
        ),
        B::AndLtPipe if combo == (PgType::Circle, PgType::Circle) => call(
            geo(GeoFn::CircleOverBelow),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(PgType::Circle)?],
        ),
        B::PipeAndGt if combo == (PgType::Circle, PgType::Circle) => call(
            geo(GeoFn::CircleOverAbove),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(PgType::Circle)?],
        ),
        B::PGBitwiseShiftLeft if combo == (PgType::Circle, PgType::Circle) => call(
            geo(GeoFn::CircleLeft),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(PgType::Circle)?],
        ),
        B::PGBitwiseShiftRight if combo == (PgType::Circle, PgType::Circle) => call(
            geo(GeoFn::CircleRight),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(PgType::Circle)?],
        ),
        B::LtLtPipe if combo == (PgType::Circle, PgType::Circle) => call(
            geo(GeoFn::CircleBelow),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(PgType::Circle)?],
        ),
        B::PipeGtGt if combo == (PgType::Circle, PgType::Circle) => call(
            geo(GeoFn::CircleAbove),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(PgType::Circle)?],
        ),
        B::ArrowAt if combo == (PgType::Circle, PgType::Circle) => call(
            geo(GeoFn::CircleContained),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(PgType::Circle)?],
        ),
        B::AtArrow if combo == (PgType::Circle, PgType::Circle) => call(
            geo(GeoFn::CircleContain),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(PgType::Circle)?],
        ),
        B::TildeEq if combo == (PgType::Circle, PgType::Circle) => call(
            geo(GeoFn::CircleSame),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(PgType::Circle)?],
        ),
        // `circle` comparisons are by area, like `box`.
        B::Eq if combo == (PgType::Circle, PgType::Circle) => call(
            geo(GeoFn::CircleEq),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(PgType::Circle)?],
        ),
        B::NotEq if combo == (PgType::Circle, PgType::Circle) => call(
            geo(GeoFn::CircleNe),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(PgType::Circle)?],
        ),
        B::Lt if combo == (PgType::Circle, PgType::Circle) => call(
            geo(GeoFn::CircleLt),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(PgType::Circle)?],
        ),
        B::LtEq if combo == (PgType::Circle, PgType::Circle) => call(
            geo(GeoFn::CircleLe),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(PgType::Circle)?],
        ),
        B::Gt if combo == (PgType::Circle, PgType::Circle) => call(
            geo(GeoFn::CircleGt),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(PgType::Circle)?],
        ),
        B::GtEq if combo == (PgType::Circle, PgType::Circle) => call(
            geo(GeoFn::CircleGe),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(PgType::Circle)?],
        ),
        // `circle <op> point`: move the center; `*` and `/` also scale the radius.
        B::Plus if combo == (PgType::Circle, Point) => call(
            geo(GeoFn::CircleAddPt),
            PgType::Circle,
            vec![l(PgType::Circle)?, r(Point)?],
        ),
        B::Minus if combo == (PgType::Circle, Point) => call(
            geo(GeoFn::CircleSubPt),
            PgType::Circle,
            vec![l(PgType::Circle)?, r(Point)?],
        ),
        B::Multiply if combo == (PgType::Circle, Point) => call(
            geo(GeoFn::CircleMulPt),
            PgType::Circle,
            vec![l(PgType::Circle)?, r(Point)?],
        ),
        B::Divide if combo == (PgType::Circle, Point) => call(
            geo(GeoFn::CircleDivPt),
            PgType::Circle,
            vec![l(PgType::Circle)?, r(Point)?],
        ),
        B::AtArrow if combo == (PgType::Circle, Point) => call(
            geo(GeoFn::CircleContainPt),
            PgType::Bool,
            vec![l(PgType::Circle)?, r(Point)?],
        ),
        B::ArrowAt if combo == (Point, PgType::Circle) => call(
            geo(GeoFn::CircleContainPt),
            PgType::Bool,
            vec![r(PgType::Circle)?, l(Point)?],
        ),
        // `polygon` positional predicates compare bounding boxes; `@>`/`<@`/`&&`/`~=`
        // are real geometry.
        B::PGOverlap if combo == (PgType::Polygon, PgType::Polygon) => call(
            geo(GeoFn::PolyOverlap),
            PgType::Bool,
            vec![l(PgType::Polygon)?, r(PgType::Polygon)?],
        ),
        B::AndLt if combo == (PgType::Polygon, PgType::Polygon) => call(
            geo(GeoFn::PolyOverLeft),
            PgType::Bool,
            vec![l(PgType::Polygon)?, r(PgType::Polygon)?],
        ),
        B::AndGt if combo == (PgType::Polygon, PgType::Polygon) => call(
            geo(GeoFn::PolyOverRight),
            PgType::Bool,
            vec![l(PgType::Polygon)?, r(PgType::Polygon)?],
        ),
        B::AndLtPipe if combo == (PgType::Polygon, PgType::Polygon) => call(
            geo(GeoFn::PolyOverBelow),
            PgType::Bool,
            vec![l(PgType::Polygon)?, r(PgType::Polygon)?],
        ),
        B::PipeAndGt if combo == (PgType::Polygon, PgType::Polygon) => call(
            geo(GeoFn::PolyOverAbove),
            PgType::Bool,
            vec![l(PgType::Polygon)?, r(PgType::Polygon)?],
        ),
        B::PGBitwiseShiftLeft if combo == (PgType::Polygon, PgType::Polygon) => call(
            geo(GeoFn::PolyLeft),
            PgType::Bool,
            vec![l(PgType::Polygon)?, r(PgType::Polygon)?],
        ),
        B::PGBitwiseShiftRight if combo == (PgType::Polygon, PgType::Polygon) => call(
            geo(GeoFn::PolyRight),
            PgType::Bool,
            vec![l(PgType::Polygon)?, r(PgType::Polygon)?],
        ),
        B::LtLtPipe if combo == (PgType::Polygon, PgType::Polygon) => call(
            geo(GeoFn::PolyBelow),
            PgType::Bool,
            vec![l(PgType::Polygon)?, r(PgType::Polygon)?],
        ),
        B::PipeGtGt if combo == (PgType::Polygon, PgType::Polygon) => call(
            geo(GeoFn::PolyAbove),
            PgType::Bool,
            vec![l(PgType::Polygon)?, r(PgType::Polygon)?],
        ),
        B::ArrowAt if combo == (PgType::Polygon, PgType::Polygon) => call(
            geo(GeoFn::PolyContained),
            PgType::Bool,
            vec![l(PgType::Polygon)?, r(PgType::Polygon)?],
        ),
        B::AtArrow if combo == (PgType::Polygon, PgType::Polygon) => call(
            geo(GeoFn::PolyContain),
            PgType::Bool,
            vec![l(PgType::Polygon)?, r(PgType::Polygon)?],
        ),
        B::TildeEq if combo == (PgType::Polygon, PgType::Polygon) => call(
            geo(GeoFn::PolySame),
            PgType::Bool,
            vec![l(PgType::Polygon)?, r(PgType::Polygon)?],
        ),
        B::AtArrow if combo == (PgType::Polygon, Point) => call(
            geo(GeoFn::PolyContainPt),
            PgType::Bool,
            vec![l(PgType::Polygon)?, r(Point)?],
        ),
        B::ArrowAt if combo == (Point, PgType::Polygon) => call(
            geo(GeoFn::PolyContainPt),
            PgType::Bool,
            vec![r(PgType::Polygon)?, l(Point)?],
        ),
        _ => Ok(None),
    }
}

/// Whether `ty` is a bit-string type (`bit` or `bit varying`).
pub(super) fn is_bit_family(ty: Option<PgType>) -> bool {
    matches!(ty, Some(PgType::Bit | PgType::Varbit))
}

/// A bit-string operand: a typed `bit`/`varbit` expression as-is (they share the
/// runtime value), or an untyped literal parsed as `bit`. Anything else is an
/// "operator does not exist" error via the caller.
fn bit_operand(b: &Binding) -> Option<Result<BoundExpr, BindError>> {
    match b {
        Binding::Typed(e) if is_bit_family(Some(e.ty())) => Some(Ok(e.clone())),
        Binding::Typed(_) => None,
        Binding::Unknown { lit, span, param } => Some(resolve_unknown(
            lit.clone(),
            *span,
            param.clone(),
            PgType::Bit,
        )),
    }
}

/// `bit || bit` (or with an untyped literal): a `bit varying` concatenation.
fn bind_bit_concat(lb: Binding, rb: Binding) -> Result<Binding, BindError> {
    let (Some(a), Some(b)) = (bit_operand(&lb), bit_operand(&rb)) else {
        return Err(BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!(
                "operator does not exist: {} || {}",
                binding_type_label(&lb),
                binding_type_label(&rb)
            ),
        ));
    };
    Ok(Binding::Typed(BoundExpr::FuncCall {
        func: ScalarFn::BitConcat,
        ret: PgType::Varbit,
        args: vec![a?, b?],
    }))
}

/// bit/varbit bitwise and shift operators lower to `ScalarFn` calls. Returns
/// `Ok(None)` when the operator/operands are not a bit operation, so the generic
/// path (and its "operator does not exist" error) still applies.
fn resolve_bit_op(
    op: &ast::BinaryOperator,
    lb: &Binding,
    rb: &Binding,
) -> Result<Option<Binding>, BindError> {
    use ast::BinaryOperator as B;
    let lt = binding_typed_ty(lb);
    let rt = binding_typed_ty(rb);
    if !is_bit_family(lt) && !is_bit_family(rt) {
        return Ok(None);
    }
    let no_op = || {
        BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!(
                "operator does not exist: {} {op} {}",
                binding_type_label(lb),
                binding_type_label(rb)
            ),
        )
    };
    // Bitwise `& | #` and the shifts below are defined only on `bit` in PG (a
    // varbit operand is cast in), so the result type is always `bit`.
    let bitwise = match op {
        B::BitwiseAnd => Some(ScalarFn::BitAnd),
        B::BitwiseOr => Some(ScalarFn::BitOr),
        B::PGBitwiseXor => Some(ScalarFn::BitXor),
        _ => None,
    };
    if let Some(func) = bitwise {
        let (Some(a), Some(b)) = (bit_operand(lb), bit_operand(rb)) else {
            return Err(no_op());
        };
        return Ok(Some(Binding::Typed(BoundExpr::FuncCall {
            func,
            ret: PgType::Bit,
            args: vec![a?, b?],
        })));
    }
    // Shifts `<< >>`: `bit << int4`, keeping the bit length; result type `bit`.
    let shift = match op {
        B::PGBitwiseShiftLeft => Some(ScalarFn::BitShl),
        B::PGBitwiseShiftRight => Some(ScalarFn::BitShr),
        _ => None,
    };
    if let Some(func) = shift {
        if !is_bit_family(lt) {
            return Err(no_op());
        }
        let Some(a) = bit_operand(lb) else {
            return Err(no_op());
        };
        let amount = resolve_operand(rb, PgType::Int4)?;
        return Ok(Some(Binding::Typed(BoundExpr::FuncCall {
            func,
            ret: PgType::Bit,
            args: vec![a?, amount],
        })));
    }
    Ok(None)
}

/// `int2`/`int4`/`int8`, or `None` for anything else.
fn int_width(ty: Option<PgType>) -> Option<PgType> {
    ty.filter(|t| matches!(t, PgType::Int2 | PgType::Int4 | PgType::Int8))
}

/// The integer bitwise (`& | #`) and shift (`<< >>`) operators. Like the bit,
/// inet and geometric families these don't fit the single-`arg_ty` `Binary`
/// node — a shift's operands have *different* types — so they lower to
/// `ScalarFn` calls.
///
/// This resolver runs last in the chain, after every type-specific one, so it
/// can never shadow `inet << inet` (containment), `bit << int4`, or the
/// geometric "strictly left of". A typed non-integer operand is left to the
/// generic path; an `unknown` literal is resolved against the other side, the
/// way PG resolves an untyped constant against whichever candidate the typed
/// side selects.
///
/// The two families differ in how much an unknown operand can be pinned by, and
/// every rule below was probed against PostgreSQL 18.4:
///
/// * bitwise `& | #` take two operands of one type in every candidate, so an
///   unknown on either side simply borrows the other's width — `'5' & 1::int8`
///   is 1, not ambiguous. Two typed sides meet at the wider one, since PG's
///   resolution widens implicitly (`int2 & int4` binds the int4 form).
/// * the shifts take an int4 count at *every* width, so a typed count pins
///   nothing about the left operand. `'1' << 2` is 4 because int4 is the exact
///   count type, but `'1' << 2::int2` is 42725 `operator is not unique` — all
///   three candidates accept a widened int2 — and `'1' << 2::int8` is 42883,
///   because int8 → int4 is an assignment cast and matches no candidate at all.
///   That last rule bites a typed left operand too: `1 << 2::int8` is 42883.
///
/// Two unknowns are 42725 either way (`'a' << 'b'`), which PG decides from the
/// signatures alone and never from what the literals contain.
fn resolve_int_bitwise_op(
    op: &ast::BinaryOperator,
    lb: &Binding,
    rb: &Binding,
) -> Result<Option<Binding>, BindError> {
    use ast::BinaryOperator as B;
    let (lt, rt) = (binding_typed_ty(lb), binding_typed_ty(rb));
    // Through `undefined_operator_error` rather than a bare `BindError`, so the
    // 42883 carries PG's HINT and `bind_binary` can hang the caret under the
    // operator, as every other "operator does not exist" here does.
    let no_op = || {
        undefined_operator_error(format!(
            "operator does not exist: {} {op} {}",
            binding_type_label(lb),
            binding_type_label(rb)
        ))
    };
    let ambiguous = || {
        ambiguous_operator(
            &binding_type_label(lb),
            &op.to_string(),
            &binding_type_label(rb),
        )
    };

    let shift = match op {
        B::PGBitwiseShiftLeft => Some(ScalarFn::IntShl),
        B::PGBitwiseShiftRight => Some(ScalarFn::IntShr),
        _ => None,
    };
    if let Some(func) = shift {
        // The width of the value being shifted, which is also the result type —
        // PG applies no overflow check here, so there is no widening.
        let width = match (int_width(lt), lt) {
            (Some(w), _) => w,
            (None, Some(_)) => return Ok(None),
            (None, None) => match rt {
                None => return Err(ambiguous()),
                Some(PgType::Int4) => PgType::Int4,
                Some(PgType::Int2) => return Err(ambiguous()),
                Some(_) => return Err(no_op()),
            },
        };
        // The count is int4, which int2 reaches implicitly and nothing else does.
        if !matches!(rt, None | Some(PgType::Int2 | PgType::Int4)) {
            return Err(no_op());
        }
        let amount = resolve_operand(rb, PgType::Int4)?;
        let left = resolve_operand(lb, width)?;
        return Ok(Some(Binding::Typed(BoundExpr::FuncCall {
            func,
            ret: width,
            args: vec![left, amount],
        })));
    }

    let bitwise = match op {
        B::BitwiseAnd => Some(ScalarFn::IntAnd),
        B::BitwiseOr => Some(ScalarFn::IntOr),
        B::PGBitwiseXor => Some(ScalarFn::IntXor),
        _ => None,
    };
    let Some(func) = bitwise else {
        return Ok(None);
    };
    let common = match (lt, rt) {
        (Some(_), Some(_)) => match (int_width(lt), int_width(rt)) {
            (Some(l), Some(r)) => common_numeric(l, r).unwrap_or(l),
            // An integer beside a typed non-integer has no operator at all; a
            // non-integer on the left is somebody else's business.
            (Some(_), None) => return Err(no_op()),
            _ => return Ok(None),
        },
        (Some(_), None) => match int_width(lt) {
            Some(l) => l,
            None => return Ok(None),
        },
        (None, Some(_)) => match int_width(rt) {
            Some(r) => r,
            None => return Ok(None),
        },
        (None, None) => return Err(ambiguous()),
    };
    Ok(Some(Binding::Typed(BoundExpr::FuncCall {
        func,
        ret: common,
        args: vec![resolve_operand(lb, common)?, resolve_operand(rb, common)?],
    })))
}

/// The `(array type, element type)` of a binding that is a typed array, or
/// `None` for anything else.
fn array_arg_type(b: &Binding) -> Option<(PgType, PgType)> {
    match binding_typed_ty(b) {
        Some(PgType::Array(elem_oid)) => {
            PgType::from_oid(elem_oid).map(|e| (PgType::Array(elem_oid), e))
        }
        _ => None,
    }
}

/// The argument type of the three size-reporting functions — `cardinality`,
/// `array_length`, `array_upper` — which read only how many elements a value has
/// and so accept `oidvector`/`int2vector` as well as a real array.
///
/// Deliberately *not* folded into [`array_arg_type`]. PostgreSQL gives the
/// vectors a `typelem`, so every `anyarray` function accepts them there; here
/// only these three do, and widening the shared helper would make
/// `array_append('1 2'::oidvector, 3)` fail inside coercion (there is no
/// `oidvector` → `oid[]` cast) instead of reporting PG's `42883`.
fn size_arg_type(b: &Binding) -> Option<PgType> {
    match binding_typed_ty(b) {
        Some(PgType::Vector(kind)) => Some(PgType::Vector(kind)),
        _ => array_arg_type(b).map(|(arr_ty, _)| arr_ty),
    }
}

/// Array containment / overlap operators (`@>`, `<@`, `&&`) → the array
/// `ScalarFn`s. Both operands are arrays (an untyped literal adopts the other's
/// array type); a typed non-array operand yields PG's `operator does not exist`.
/// The element type must have a default equality operator — a non-orderable
/// element (`json`, `point`, ...) reports PG's `could not identify an equality
/// operator` rather than reaching (and panicking in) `compare_values`. Tried
/// after the network/geometric/jsonb resolvers, which own these spellings for
/// their own operand types.
fn resolve_array_op(
    op: &ast::BinaryOperator,
    lb: &Binding,
    rb: &Binding,
    catalog: &dyn TypeCatalog,
) -> Result<Option<Binding>, BindError> {
    use ast::BinaryOperator as B;
    let func = match op {
        B::AtArrow => ScalarFn::ArrayContains,
        B::ArrowAt => ScalarFn::ArrayContainedBy,
        B::PGOverlap => ScalarFn::ArrayOverlap,
        _ => return Ok(None),
    };
    let la = array_arg_type(lb);
    let ra = array_arg_type(rb);
    // The shared array type. Both sides must be arrays: two typed arrays unify on
    // their element type; an untyped literal opposite a typed array adopts it; a
    // typed *non-array* opposite an array is `operator does not exist`.
    let arr_ty = match (la, ra) {
        (Some((_, le)), Some((_, re))) => {
            let elem = merge_types(le, re).ok_or_else(|| undefined_binary_operator(lb, op, rb))?;
            PgType::Array(elem.oid())
        }
        (Some((arr, _)), None) => {
            if binding_typed_ty(rb).is_some() {
                return Err(undefined_binary_operator(lb, op, rb));
            }
            arr
        }
        (None, Some((arr, _))) => {
            if binding_typed_ty(lb).is_some() {
                return Err(undefined_binary_operator(lb, op, rb));
            }
            arr
        }
        (None, None) => return Ok(None),
    };
    // These operators compare elements for equality; a non-orderable element type
    // has no default equality operator (PG's error), and `compare_values` has no
    // arm for it — so gate here to keep it off the panic path.
    let elem = arr_ty.array_element();
    if !elem.is_some_and(|e| has_equality(e, catalog)) {
        let name = elem.map_or_else(|| arr_ty.name().to_string(), |e| type_label(e, catalog));
        return Err(BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!("could not identify an equality operator for type {name}"),
        ));
    }
    let left = resolve_operand(lb, arr_ty)?;
    let right = resolve_operand(rb, arr_ty)?;
    Ok(Some(Binding::Typed(BoundExpr::FuncCall {
        func,
        ret: PgType::Bool,
        args: vec![left, right],
    })))
}

/// `||` where at least one operand is an array. An **untyped literal or NULL**
/// opposite an array is treated as an array (PG resolves `array || unknown` to
/// `array_cat`), so `ARRAY[1,2] || '{3,4}'` concatenates and `array || NULL`
/// returns the array; a **typed element** opposite an array is append/prepend.
/// Element types are unified (PG promotes `int[] || bigint` to `bigint[]`); a
/// pair with no common type is PG's `operator does not exist: X || Y`.
fn bind_array_concat(lb: Binding, rb: Binding) -> Result<Binding, BindError> {
    let mismatch = |lb: &Binding, rb: &Binding| {
        BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!(
                "operator does not exist: {} || {}",
                binding_type_label(lb),
                binding_type_label(rb)
            ),
        )
    };
    match (array_arg_type(&lb), array_arg_type(&rb)) {
        // array || array: unify element types, then concatenate.
        (Some((_, le)), Some((_, re))) => {
            let arr = PgType::Array(merge_types(le, re).ok_or_else(|| mismatch(&lb, &rb))?.oid());
            let left = resolve_operand(&lb, arr)?;
            let right = resolve_operand(&rb, arr)?;
            Ok(Binding::Typed(BoundExpr::FuncCall {
                func: ScalarFn::ArrayCat,
                ret: arr,
                args: vec![left, right],
            }))
        }
        // array on the left; right is an untyped literal/NULL (→ concat) or a
        // typed element (→ append).
        (Some((arr_ty, elem)), None) => match binding_typed_ty(&rb) {
            None => {
                let left = resolve_operand(&lb, arr_ty)?;
                let right = resolve_operand(&rb, arr_ty)?;
                Ok(Binding::Typed(BoundExpr::FuncCall {
                    func: ScalarFn::ArrayCat,
                    ret: arr_ty,
                    args: vec![left, right],
                }))
            }
            Some(rty) => {
                let arr = PgType::Array(
                    merge_types(elem, rty)
                        .ok_or_else(|| mismatch(&lb, &rb))?
                        .oid(),
                );
                let elem = arr.array_element().expect("array element resolves");
                let left = resolve_operand(&lb, arr)?;
                let right = resolve_operand(&rb, elem)?;
                Ok(Binding::Typed(BoundExpr::FuncCall {
                    func: ScalarFn::ArrayAppend,
                    ret: arr,
                    args: vec![left, right],
                }))
            }
        },
        // array on the right; symmetric (untyped literal → concat, element → prepend).
        (None, Some((arr_ty, elem))) => match binding_typed_ty(&lb) {
            None => {
                let left = resolve_operand(&lb, arr_ty)?;
                let right = resolve_operand(&rb, arr_ty)?;
                Ok(Binding::Typed(BoundExpr::FuncCall {
                    func: ScalarFn::ArrayCat,
                    ret: arr_ty,
                    args: vec![left, right],
                }))
            }
            Some(lty) => {
                let arr = PgType::Array(
                    merge_types(elem, lty)
                        .ok_or_else(|| mismatch(&lb, &rb))?
                        .oid(),
                );
                let elem = arr.array_element().expect("array element resolves");
                let left = resolve_operand(&lb, elem)?;
                let right = resolve_operand(&rb, arr)?;
                Ok(Binding::Typed(BoundExpr::FuncCall {
                    func: ScalarFn::ArrayPrepend,
                    ret: arr,
                    args: vec![left, right],
                }))
            }
        },
        // The caller only routes here when a typed array is present; if neither
        // side classifies as an array, report the operator error rather than panic.
        (None, None) => Err(mismatch(&lb, &rb)),
    }
}

/// Bind a polymorphic array function whose overload can't live in the
/// fixed-signature table: `cardinality`, `array_length`, `array_upper`,
/// `array_append`, `array_prepend`, `array_cat`, `array_to_string`. Returns
/// `Ok(None)` if `name` is not one of them, so the caller falls through to
/// ordinary resolution.
pub(crate) fn bind_array_function(
    name: &str,
    bindings: &[Binding],
) -> Result<Option<Binding>, BindError> {
    let undefined = || {
        BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!(
                "function {name}({}) does not exist",
                bindings
                    .iter()
                    .map(binding_type_label)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    };
    let call = |func, ret, args| {
        Ok(Some(Binding::Typed(BoundExpr::FuncCall {
            func,
            ret,
            args,
        })))
    };
    match name {
        "cardinality" => {
            let [b] = bindings else {
                return Err(undefined());
            };
            let arg_ty = size_arg_type(b).ok_or_else(undefined)?;
            call(
                ScalarFn::Cardinality,
                PgType::Int4,
                vec![resolve_operand(b, arg_ty)?],
            )
        }
        "array_length" | "array_upper" => {
            let [a, dim] = bindings else {
                return Err(undefined());
            };
            let func = match name {
                "array_length" => ScalarFn::ArrayLength,
                _ => ScalarFn::ArrayUpper,
            };
            let arg_ty = size_arg_type(a).ok_or_else(undefined)?;
            call(
                func,
                PgType::Int4,
                vec![
                    resolve_operand(a, arg_ty)?,
                    resolve_operand(dim, PgType::Int4)?,
                ],
            )
        }
        // `array_to_string(anyarray, text [, text])` renders the array's elements
        // (NULLs skipped, or replaced by the optional third argument) joined by
        // the delimiter. Always returns text.
        "array_to_string" => {
            let (a, delim, null_str) = match bindings {
                [a, delim] => (a, delim, None),
                [a, delim, null_str] => (a, delim, Some(null_str)),
                _ => return Err(undefined()),
            };
            let (arr_ty, _) = array_arg_type(a).ok_or_else(undefined)?;
            let mut args = vec![
                resolve_operand(a, arr_ty)?,
                resolve_operand(delim, PgType::Text)?,
            ];
            if let Some(null_str) = null_str {
                args.push(resolve_operand(null_str, PgType::Text)?);
            }
            call(ScalarFn::ArrayToString, PgType::Text, args)
        }
        // `array_append(anyarray, elem)` promotes the array/element to their
        // common element type (PG's `anycompatiblearray`/`anycompatible`).
        "array_append" => {
            let [a, e] = bindings else {
                return Err(undefined());
            };
            let (_, elem) = array_arg_type(a).ok_or_else(undefined)?;
            let common =
                merge_types(elem, binding_typed_ty(e).unwrap_or(elem)).ok_or_else(undefined)?;
            let arr = PgType::Array(common.oid());
            call(
                ScalarFn::ArrayAppend,
                arr,
                vec![resolve_operand(a, arr)?, resolve_operand(e, common)?],
            )
        }
        "array_prepend" => {
            let [e, a] = bindings else {
                return Err(undefined());
            };
            let (_, elem) = array_arg_type(a).ok_or_else(undefined)?;
            let common =
                merge_types(elem, binding_typed_ty(e).unwrap_or(elem)).ok_or_else(undefined)?;
            let arr = PgType::Array(common.oid());
            call(
                ScalarFn::ArrayPrepend,
                arr,
                vec![resolve_operand(e, common)?, resolve_operand(a, arr)?],
            )
        }
        // `array_cat(anyarray, anyarray)` unifies the two element types.
        "array_cat" => {
            let [a, b] = bindings else {
                return Err(undefined());
            };
            let ae = array_arg_type(a).map(|(_, e)| e);
            let be = array_arg_type(b).map(|(_, e)| e);
            let arr = match (ae, be) {
                (Some(ae), Some(be)) => {
                    PgType::Array(merge_types(ae, be).ok_or_else(undefined)?.oid())
                }
                (Some(ae), None) => PgType::Array(ae.oid()),
                (None, Some(be)) => PgType::Array(be.oid()),
                (None, None) => return Err(undefined()),
            };
            call(
                ScalarFn::ArrayCat,
                arr,
                vec![resolve_operand(a, arr)?, resolve_operand(b, arr)?],
            )
        }
        _ => Ok(None),
    }
}

/// `macaddr`/`macaddr8` `&`/`|`: lower to the width-dispatched `ScalarFn`. PG has
/// only `macaddr & macaddr` and `macaddr8 & macaddr8` — no cross-width operator
/// and no implicit `macaddr`<->`macaddr8` — so both operands must settle on the
/// *same* mac type. The typed side fixes that type; an untyped literal adopts it
/// (EUI-64 expanding for `macaddr8`). Two typed operands of different mac widths,
/// or a mac paired with any other typed value, have no operator: report PG's
/// `operator does not exist` rather than silently coercing one side.
fn resolve_macaddr_op(
    op: &ast::BinaryOperator,
    lb: &Binding,
    rb: &Binding,
) -> Result<Option<Binding>, BindError> {
    use ast::BinaryOperator as B;
    let func = match op {
        B::BitwiseAnd => ScalarFn::MacaddrAnd,
        B::BitwiseOr => ScalarFn::MacaddrOr,
        _ => return Ok(None),
    };
    let lt = binding_typed_ty(lb);
    let rt = binding_typed_ty(rb);
    let is_mac = |t: Option<PgType>| matches!(t, Some(PgType::Macaddr | PgType::Macaddr8));
    // Not our operator unless at least one side is a mac type.
    if !is_mac(lt) && !is_mac(rt) {
        return Ok(None);
    }
    // The mac type both operands must share, taken from a typed mac operand.
    // Two typed mac operands of different widths have no operator.
    let mac_ty = match (lt, rt) {
        (Some(l), Some(r)) if is_mac(lt) && is_mac(rt) => {
            if l != r {
                return Err(undefined_binary_operator(lb, op, rb));
            }
            l
        }
        (Some(l), _) if is_mac(lt) => l,
        (_, Some(r)) if is_mac(rt) => r,
        _ => unreachable!("at least one operand is a mac type"),
    };
    // The partner must be the same-typed mac (handled above) or an untyped
    // literal; a typed non-mac partner (e.g. `macaddr & integer`) has no operator.
    let typed_non_mac = |t: Option<PgType>| t.is_some() && !is_mac(t);
    if typed_non_mac(lt) || typed_non_mac(rt) {
        return Err(undefined_binary_operator(lb, op, rb));
    }
    let a = resolve_operand(lb, mac_ty)?;
    let b = resolve_operand(rb, mac_ty)?;
    Ok(Some(Binding::Typed(BoundExpr::FuncCall {
        func,
        ret: mac_ty,
        args: vec![a, b],
    })))
}

/// Lower `jsonb @? jsonpath` / `jsonb @@ jsonpath` to the silent jsonpath query
/// functions. Uses the dedicated `ExistsOp`/`MatchOp` variants (a 2-arg,
/// always-silent form) rather than the STRICT `jsonb_path_exists`/`_match`
/// functions, so the operator never nullifies on a NULL `vars`/`silent`.
/// Returns `Ok(None)` when the operator isn't one of these or the left operand
/// isn't `jsonb`.
fn resolve_jsonb_op(
    op: &ast::BinaryOperator,
    lb: &Binding,
    rb: &Binding,
) -> Result<Option<Binding>, BindError> {
    use crate::functions::JsonPathFn;
    let jf = match op {
        ast::BinaryOperator::AtQuestion => JsonPathFn::ExistsOp,
        ast::BinaryOperator::AtAt => JsonPathFn::MatchOp,
        _ => return Ok(None),
    };
    // Only defined for a `jsonb` left operand (an untyped literal is coerced).
    if matches!(binding_typed_ty(lb), Some(t) if t != PgType::Jsonb) {
        return Ok(None);
    }
    let left = resolve_operand(lb, PgType::Jsonb)?;
    let right = resolve_operand(rb, PgType::Jsonpath)?;
    Ok(Some(Binding::Typed(BoundExpr::FuncCall {
        func: ScalarFn::JsonPath(jf),
        ret: PgType::Bool,
        args: vec![left, right],
    })))
}

/// The JSON extraction operators `-> ->> #> #>>`, for both `json` and `jsonb`.
///
/// `->`/`->>` are overloaded on the right operand: a text-family key selects the
/// object-field form, an `int2`/`int4` subscript the array-element form. PG's
/// operator is declared on `integer`, and `int8 -> int4` is only an *assignment*
/// cast, so `jsonb -> 1::bigint` is `operator does not exist` — as is
/// `jsonb #> integer[]`, since only `text[]`/`varchar[]` reach `text[]`
/// implicitly.
///
/// Returns `Ok(None)` only when the operator is not one of the four. A bad
/// operand errors here rather than falling through, because no later resolver
/// claims these spellings and the generic mapping would report the wrong
/// "operator is not supported yet" (0A000) instead of PG's 42883.
fn resolve_json_op(
    op: &ast::BinaryOperator,
    lb: &Binding,
    rb: &Binding,
    op_span: Span,
) -> Result<Option<Binding>, BindError> {
    use crate::functions::JsonFn;
    use ast::BinaryOperator as B;
    if !matches!(
        op,
        B::Arrow | B::LongArrow | B::HashArrow | B::HashLongArrow
    ) {
        return Ok(None);
    }
    // Both error paths point at the operator, as PG's `LINE n: ... ^` does.
    // Positioning is per-error, not chain-wide: PG leaves other resolver errors
    // (e.g. array's `could not identify an equality operator`) unpositioned even
    // though they share this SQLSTATE.
    let undefined = || undefined_binary_operator(lb, op, rb).at(op_span);
    // The container type also decides `->`/`#>`'s return type. An untyped left
    // operand leaves all four candidate operators (json/jsonb × text/int4)
    // equally applicable, which is PG's 42725 rather than a missing operator.
    let container = match binding_typed_ty(lb) {
        Some(t @ (PgType::Json | PgType::Jsonb)) => t,
        Some(_) => return Err(undefined()),
        None => {
            return Err(
                ambiguous_operator(operand_name(lb), &op.to_string(), operand_name(rb)).at(op_span),
            );
        }
    };
    let text_out = matches!(op, B::LongArrow | B::HashLongArrow);
    let (func, right) = match op {
        B::Arrow | B::LongArrow => {
            let rt = binding_typed_ty(rb);
            // An untyped literal takes `text`, the string category's preferred
            // type, so `jsonb -> 'a'` is the object-field operator rather than
            // the subscript one.
            if rt.is_none_or(is_text_family) {
                (
                    if text_out {
                        JsonFn::ObjectFieldText
                    } else {
                        JsonFn::ObjectField
                    },
                    resolve_operand(rb, PgType::Text)?,
                )
            } else if matches!(rt, Some(PgType::Int2 | PgType::Int4)) {
                (
                    if text_out {
                        JsonFn::ArrayElementText
                    } else {
                        JsonFn::ArrayElement
                    },
                    resolve_operand(rb, PgType::Int4)?,
                )
            } else {
                return Err(undefined());
            }
        }
        _ => {
            let text_arr = PgType::Array(PgType::Text.oid());
            // An array over any text-family element (or an untyped literal, which
            // parses as `text[]`) is accepted, since each of those elements casts
            // to `text` implicitly — the same rule the scalar arm above applies
            // via `is_text_family`. Anything else must report a missing operator
            // rather than be silently coerced, so `jsonb #> integer[]` is 42883.
            match binding_typed_ty(rb) {
                None => {}
                Some(PgType::Array(elem)) if PgType::from_oid(elem).is_some_and(is_text_family) => {
                }
                Some(_) => return Err(undefined()),
            }
            (
                if text_out {
                    JsonFn::ExtractPathText
                } else {
                    JsonFn::ExtractPath
                },
                resolve_operand(rb, text_arr)?,
            )
        }
    };
    let left = resolve_operand(lb, container)?;
    Ok(Some(Binding::Typed(BoundExpr::FuncCall {
        func: ScalarFn::Json(func),
        ret: if text_out { PgType::Text } else { container },
        args: vec![left, right],
    })))
}

/// Text-search operators: `tsvector @@ tsquery` (either operand order), and
/// `tsquery && | <-> tsquery`.
///
/// Both operands must be untyped literals or already the required text-search
/// type. `Ok(None)` when no text-search operand is present, so `@@` still
/// reaches the jsonpath resolver; otherwise an error, so `point <-> tsquery`
/// reports a missing operator rather than a cast failure from inside
/// `coerce_expr`.
fn resolve_ts_op(
    op: &ast::BinaryOperator,
    lb: &Binding,
    rb: &Binding,
) -> Result<Option<Binding>, BindError> {
    use crate::functions::TsFn;
    let (lt, rt) = (binding_typed_ty(lb), binding_typed_ty(rb));
    // An operand is usable as `want` only if it is untyped or already `want`.
    let usable = |t: Option<PgType>, want: PgType| t.is_none_or(|t| t == want);
    match op {
        ast::BinaryOperator::AtAt => {
            // Decide the operand order from whichever side is already typed.
            let swapped = match (lt, rt) {
                (Some(PgType::Tsvector), _) | (_, Some(PgType::Tsquery)) => false,
                (Some(PgType::Tsquery), _) | (_, Some(PgType::Tsvector)) => true,
                _ => return Ok(None),
            };
            let (vec_b, query_b) = if swapped { (rb, lb) } else { (lb, rb) };
            let (vec_t, query_t) = if swapped { (rt, lt) } else { (lt, rt) };
            // The vector side must already *be* a tsvector. PG resolves an
            // untyped literal here to `text`, not `tsvector` -- `'Hello World'
            // @@ 'hello'::tsquery` is `to_tsvector('Hello World') @@ …`, which
            // is true. Parsing the literal as a tsvector instead would answer
            // false, and look like a real answer. Both that and an explicit
            // `text` operand need a text search configuration, so report the
            // honest 0A000 rather than a wrong boolean or a 42883 that would
            // deny an operator PG really has.
            // TODO: add text search configurations (`to_tsvector`) so a `text`
            // or untyped operand on the vector side of `@@` binds instead of
            // raising 0A000.
            if vec_t.is_none() || vec_t.is_some_and(is_text_family) {
                return Err(BindError::feature_not_supported(
                    "text @@ tsquery is not supported yet: it requires a text search \
                     configuration (to_tsvector)",
                ));
            }
            if !usable(vec_t, PgType::Tsvector) || !usable(query_t, PgType::Tsquery) {
                return Err(undefined_binary_operator(lb, op, rb));
            }
            Ok(Some(Binding::Typed(BoundExpr::FuncCall {
                func: ScalarFn::Ts(TsFn::Match),
                ret: PgType::Bool,
                args: vec![
                    resolve_operand(vec_b, PgType::Tsvector)?,
                    resolve_operand(query_b, PgType::Tsquery)?,
                ],
            })))
        }
        // `&&` and `<->` combine two queries. Without a typed `tsquery` these
        // belong to arrays/inet and to the geometric distance operator.
        ast::BinaryOperator::PGOverlap | ast::BinaryOperator::LtDashGt => {
            if lt != Some(PgType::Tsquery) && rt != Some(PgType::Tsquery) {
                return Ok(None);
            }
            if !usable(lt, PgType::Tsquery) || !usable(rt, PgType::Tsquery) {
                return Err(undefined_binary_operator(lb, op, rb));
            }
            let f = if matches!(op, ast::BinaryOperator::PGOverlap) {
                TsFn::QueryAnd
            } else {
                TsFn::QueryPhrase
            };
            Ok(Some(Binding::Typed(BoundExpr::FuncCall {
                func: ScalarFn::Ts(f),
                ret: PgType::Tsquery,
                args: vec![
                    resolve_operand(lb, PgType::Tsquery)?,
                    resolve_operand(rb, PgType::Tsquery)?,
                ],
            })))
        }
        _ => Ok(None),
    }
}

fn resolve_ts_concat(lb: &Binding, rb: &Binding) -> Result<Option<Binding>, BindError> {
    use crate::functions::TsFn;
    let (lt, rt) = (binding_typed_ty(lb), binding_typed_ty(rb));
    let ty = if lt == Some(PgType::Tsvector) || rt == Some(PgType::Tsvector) {
        PgType::Tsvector
    } else if lt == Some(PgType::Tsquery) || rt == Some(PgType::Tsquery) {
        PgType::Tsquery
    } else {
        return Ok(None);
    };
    // The other side must be untyped or the same text-search type. Anything else
    // (`text || tsvector`) is PG's `anytextcat`, which renders the tsvector as
    // text — so leave it to `bind_string_concat`.
    if !lt.is_none_or(|t| t == ty) || !rt.is_none_or(|t| t == ty) {
        return Ok(None);
    }
    let f = if ty == PgType::Tsvector {
        TsFn::VectorConcat
    } else {
        TsFn::QueryOr
    };
    let left = resolve_operand(lb, ty)?;
    let right = resolve_operand(rb, ty)?;
    Ok(Some(Binding::Typed(BoundExpr::FuncCall {
        func: ScalarFn::Ts(f),
        ret: ty,
        args: vec![left, right],
    })))
}

/// `!! tsquery` — negation. PG spells prefix `!!` as the "factorial" token.
fn resolve_ts_unary(operand: Binding) -> Result<Binding, BindError> {
    use crate::functions::TsFn;
    let e = match operand {
        Binding::Typed(e) if e.ty() == PgType::Tsquery => e,
        Binding::Typed(e) => return Err(no_op_unary("!!", e.ty().name())),
        Binding::Unknown { lit, span, param } => {
            resolve_unknown(lit, span, param, PgType::Tsquery)?
        }
    };
    Ok(Binding::Typed(BoundExpr::FuncCall {
        func: ScalarFn::Ts(TsFn::QueryNot),
        ret: PgType::Tsquery,
        args: vec![e],
    }))
}

pub(super) fn resolve_operand(b: &Binding, target: PgType) -> Result<BoundExpr, BindError> {
    match b {
        Binding::Typed(e) if e.ty() == target => Ok(e.clone()),
        Binding::Typed(e) => coerce_expr(e.clone(), target),
        Binding::Unknown { lit, span, param } => {
            resolve_unknown(lit.clone(), *span, param.clone(), target)
        }
    }
}

fn bind_pow(lb: Binding, rb: Binding) -> Result<Binding, BindError> {
    let numeric = |b: &Binding| {
        matches!(b, Binding::Typed(e) if e.ty().is_numeric())
            || matches!(b, Binding::Unknown { .. })
    };
    if !numeric(&lb) || !numeric(&rb) {
        return Err(no_operator(
            &binding_type_label(&lb),
            BinOp::Pow,
            &binding_type_label(&rb),
        ));
    }
    // PG's `^` exists for `float8` and `numeric`. A float operand selects the
    // float8 operator; otherwise a numeric operand selects numeric (returning
    // numeric); with only ints/unknowns it falls back to float8 (as PG does).
    let is_float = |b: &Binding| matches!(b, Binding::Typed(e) if matches!(e.ty(), PgType::Float4 | PgType::Float8));
    let is_num = |b: &Binding| matches!(b, Binding::Typed(e) if e.ty() == PgType::Numeric);
    if !is_float(&lb) && !is_float(&rb) && (is_num(&lb) || is_num(&rb)) {
        // numeric ^ numeric -> numeric, via the power() function.
        let left = pow_operand(lb, PgType::Numeric)?;
        let right = pow_operand(rb, PgType::Numeric)?;
        return Ok(Binding::Typed(BoundExpr::FuncCall {
            func: ScalarFn::NumPower,
            ret: PgType::Numeric,
            args: vec![left, right],
        }));
    }
    let left = pow_operand(lb, PgType::Float8)?;
    let right = pow_operand(rb, PgType::Float8)?;
    Ok(Binding::Typed(BoundExpr::Binary {
        op: BinOp::Pow,
        arg_ty: PgType::Float8,
        collation: DEFAULT_COLLATION_OID,
        left: Box::new(left),
        right: Box::new(right),
    }))
}

fn pow_operand(b: Binding, target: PgType) -> Result<BoundExpr, BindError> {
    match b {
        Binding::Typed(e) => coerce_expr(e, target),
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, target),
    }
}

/// Coerce a binding to `text` for a string function/operator argument. An
/// untyped literal (or NULL) becomes text; a typed value casts to text.
pub(super) fn to_text_operand(binding: Binding) -> Result<BoundExpr, BindError> {
    match binding {
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, PgType::Text),
        Binding::Typed(e) if e.ty() == PgType::Text => Ok(e),
        Binding::Typed(e) => coerce_expr(e, PgType::Text),
    }
}

/// True for the text-family types that share `text`'s value representation —
/// exactly the collatable types.
pub(crate) fn is_text_family(ty: PgType) -> bool {
    ty.is_collatable()
}

/// Coerce an argument for `concat`/`concat_ws`/`format`, which use each value's
/// *output* representation. Text-family values are kept as-is (so a `bpchar`
/// keeps its blank padding, unlike the trailing-blank-stripping `||`); other
/// types are cast to their text form.
pub(crate) fn to_concat_operand(binding: Binding) -> Result<BoundExpr, BindError> {
    match binding {
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, PgType::Text),
        Binding::Typed(e) if is_text_family(e.ty()) => Ok(e),
        // These functions render each argument with its *output* function, not
        // its cast to text. The two differ for bool (`t` versus `true`) and for
        // inet (`192.168.1.5` versus `192.168.1.5/32`), so those are left for
        // the executor to encode; `||`, which really does cast, goes through
        // `to_text_operand` instead.
        Binding::Typed(e) if matches!(e.ty(), PgType::Bool | PgType::Inet) => Ok(e),
        Binding::Typed(e) => coerce_expr(e, PgType::Text),
    }
}

/// `a || b`: PG accepts `text || text` and `text || anynonarray` (either side),
/// but not two non-text operands. At least one side must be text or an untyped
/// literal; both are then coerced to text.
fn bind_string_concat(lb: Binding, rb: Binding) -> Result<Binding, BindError> {
    let textish = |b: &Binding| {
        matches!(b, Binding::Unknown { .. })
            || matches!(b, Binding::Typed(e) if e.ty() == PgType::Text)
    };
    if !textish(&lb) && !textish(&rb) {
        let (Binding::Typed(l), Binding::Typed(r)) = (&lb, &rb) else {
            unreachable!("a non-textish binding is typed");
        };
        return Err(BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!(
                "operator does not exist: {} || {}",
                l.ty().name(),
                r.ty().name()
            ),
        ));
    }
    let left = to_text_operand(lb)?;
    let right = to_text_operand(rb)?;
    Ok(Binding::Typed(BoundExpr::FuncCall {
        func: ScalarFn::TextConcat,
        ret: PgType::Text,
        args: vec![left, right],
    }))
}

/// `a [I]LIKE b [ESCAPE c]`: coerce operands to text and build the match call
/// (the escape string, when present, is a third argument), wrapping a negated
/// form in `NOT`.
pub(super) fn bind_like(
    lb: Binding,
    rb: Binding,
    escape: Option<Binding>,
    case_insensitive: bool,
    negated: bool,
) -> Result<Binding, BindError> {
    let mut args = vec![to_text_operand(lb)?, to_text_operand(rb)?];
    if let Some(escape) = escape {
        args.push(to_text_operand(escape)?);
    }
    let call = BoundExpr::FuncCall {
        func: if case_insensitive {
            ScalarFn::ILike
        } else {
            ScalarFn::Like
        },
        ret: PgType::Bool,
        args,
    };
    let expr = if negated {
        BoundExpr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(call),
        }
    } else {
        call
    };
    Ok(Binding::Typed(expr))
}

/// `a ~ b` / `a ~* b` (and their negations): coerce operands to text and build
/// the POSIX regex match call, wrapping a negated form (`!~` / `!~*`) in `NOT`.
fn bind_regex(
    lb: Binding,
    rb: Binding,
    case_insensitive: bool,
    negated: bool,
) -> Result<Binding, BindError> {
    let args = vec![to_text_operand(lb)?, to_text_operand(rb)?];
    let call = BoundExpr::FuncCall {
        func: if case_insensitive {
            ScalarFn::RegexIMatch
        } else {
            ScalarFn::RegexMatch
        },
        ret: PgType::Bool,
        args,
    };
    let expr = if negated {
        BoundExpr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(call),
        }
    } else {
        call
    };
    Ok(Binding::Typed(expr))
}

/// `a SIMILAR TO b [ESCAPE c]`: coerce operands to text and build the match
/// call (the escape string, when present, is a third argument), wrapping a
/// negated form in `NOT`.
pub(super) fn bind_similar_to(
    expr: &ast::Expr,
    pattern: &ast::Expr,
    escape_char: Option<&ast::ValueWithSpan>,
    negated: bool,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let mut args = vec![
        to_text_operand(bind_expr(expr, scope)?)?,
        to_text_operand(bind_expr(pattern, scope)?)?,
    ];
    if let Some(v) = escape_char {
        match v.value.as_pg_string() {
            Some(s) => {
                args.push(BoundExpr::Const {
                    value: Value::Text(s.to_string()),
                    ty: PgType::Text,
                });
            }
            None => {
                return Err(BindError::syntax(format!(
                    "invalid ESCAPE literal: {}",
                    v.value
                )));
            }
        }
    }
    let call = BoundExpr::FuncCall {
        func: ScalarFn::SimilarTo,
        ret: PgType::Bool,
        args,
    };
    let expr = if negated {
        BoundExpr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(call),
        }
    } else {
        call
    };
    Ok(Binding::Typed(expr))
}
