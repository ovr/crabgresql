//! The `bind_expr` dispatcher and the per-node binders it fans out to —
//! subqueries, CASE/IN/BETWEEN, array constructors, and the output-name rules
//! a projection takes its column label from.

use crabgresql_parser::ast::Spanned;
use crabgresql_parser::{Span, ast};
use crabgresql_pg_wire::sqlstate;
use crabgresql_types::{PgType, Value};

use crate::BindError;
use crate::functions::{bind_function, bind_srf_projection};

use super::bound::{BinOp, BoundExpr, Subplan};
use super::coerce::{
    bind_cast, bind_typed_string, coerce_expr, custom_type_name, merge_types, resolve_unknown,
    to_bool_operand, unify_value_column,
};
use super::datatype::map_data_type;
use super::literal::{bind_at_local, bind_at_time_zone, bind_extract, bind_interval, bind_value};
use super::operators::{
    bind_binary, bind_binary_op, bind_bool_test, bind_compound, bind_is_null, bind_like,
    bind_similar_to, bind_unary, binding_typed_ty, is_geo_ty, resolve_operand,
};
use super::scope::{Binding, Scope, normalize_ident};

pub fn bind_expr(expr: &ast::Expr, scope: &Scope) -> Result<Binding, BindError> {
    match expr {
        ast::Expr::Value(v) => bind_value(v, scope),
        // The DEFAULT keyword (INSERT VALUES / UPDATE SET) parses as a plain
        // identifier; without this check it would bind as a column reference
        // and mislead with `column "default" does not exist`. A real column
        // named "default" must be quoted, which keeps quote_style set.
        ast::Expr::Identifier(ident)
            if ident.quote_style.is_none() && ident.value.eq_ignore_ascii_case("default") =>
        {
            Err(BindError::feature_not_supported(
                "DEFAULT is not supported yet",
            ))
        }
        ast::Expr::Identifier(ident) => scope.resolve(&normalize_ident(ident)).map(Binding::Typed),
        ast::Expr::CompoundIdentifier(parts) => bind_compound(parts, scope).map(Binding::Typed),
        ast::Expr::Nested(inner) => bind_expr(inner, scope),
        ast::Expr::UnaryOp { op, expr } => bind_unary(*op, expr, scope),
        ast::Expr::BinaryOp {
            left,
            op,
            right,
            op_span,
        } => bind_binary(left, op, right, op_span.0, scope),
        ast::Expr::IsNull(inner) => bind_is_null(inner, scope, false),
        ast::Expr::IsNotNull(inner) => bind_is_null(inner, scope, true),
        ast::Expr::IsTrue(i) => bind_bool_test(i, scope, Some(true), false),
        ast::Expr::IsNotTrue(i) => bind_bool_test(i, scope, Some(true), true),
        ast::Expr::IsFalse(i) => bind_bool_test(i, scope, Some(false), false),
        ast::Expr::IsNotFalse(i) => bind_bool_test(i, scope, Some(false), true),
        ast::Expr::IsUnknown(i) => bind_bool_test(i, scope, None, false),
        ast::Expr::IsNotUnknown(i) => bind_bool_test(i, scope, None, true),
        ast::Expr::Cast {
            expr, data_type, ..
        } => bind_cast(expr, data_type, scope),
        ast::Expr::TypedString(ts) => bind_typed_string(ts, scope),
        ast::Expr::Function(func) => bind_function(func, scope),
        ast::Expr::Ceil { expr, field } => {
            crate::functions::bind_ceil_floor("ceil", expr, field, scope)
        }
        ast::Expr::Floor { expr, field } => {
            crate::functions::bind_ceil_floor("floor", expr, field, scope)
        }
        ast::Expr::Extract { field, expr, .. } => bind_extract(field, expr, scope),
        ast::Expr::Interval(iv) => bind_interval(iv),
        ast::Expr::AtTimeZone {
            timestamp,
            time_zone,
        } => bind_at_time_zone(timestamp, time_zone, scope),
        ast::Expr::AtLocal { timestamp } => bind_at_local(timestamp, scope),
        ast::Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => bind_case(
            operand.as_deref(),
            conditions,
            else_result.as_deref(),
            scope,
        ),
        // String special-syntax expressions desugar to the equivalent function.
        ast::Expr::Substring {
            expr,
            substring_from,
            substring_for,
            shorthand,
            ..
        } => bind_substring(
            expr,
            substring_from.as_deref(),
            substring_for.as_deref(),
            *shorthand,
            scope,
        ),
        ast::Expr::Trim {
            expr,
            trim_where,
            trim_what,
            trim_characters,
        } => bind_trim(
            expr,
            *trim_where,
            trim_what.as_deref(),
            trim_characters.as_deref(),
            scope,
        ),
        ast::Expr::Position { expr, r#in } => {
            // POSITION(sub IN str) == strpos(str, sub).
            let sub = bind_expr(expr, scope)?;
            let str_ = bind_expr(r#in, scope)?;
            crate::functions::resolve_call("strpos", vec![str_, sub], scope.catalog())
        }
        ast::Expr::Overlay {
            expr,
            overlay_what,
            overlay_from,
            overlay_for,
        } => bind_overlay(
            expr,
            overlay_what,
            overlay_from,
            overlay_for.as_deref(),
            scope,
        ),
        ast::Expr::Like {
            negated,
            any,
            expr,
            pattern,
            escape_char,
        } => bind_like_node(
            expr,
            pattern,
            escape_char.as_ref(),
            *any,
            false,
            *negated,
            scope,
        ),
        ast::Expr::ILike {
            negated,
            any,
            expr,
            pattern,
            escape_char,
        } => bind_like_node(
            expr,
            pattern,
            escape_char.as_ref(),
            *any,
            true,
            *negated,
            scope,
        ),
        ast::Expr::SimilarTo {
            negated,
            expr,
            pattern,
            escape_char,
        } => bind_similar_to(expr, pattern, escape_char.as_ref(), *negated, scope),
        ast::Expr::InList {
            expr,
            list,
            negated,
        } => bind_in_list(expr, list, *negated, scope),
        ast::Expr::Subquery(query) => bind_scalar_subquery(query, scope),
        ast::Expr::Exists { subquery, negated } => bind_exists(subquery, *negated, scope),
        ast::Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => bind_in_subquery(expr, subquery, *negated, scope),
        // `left op ANY(…)` / `left op SOME(…)` (SOME ≡ ANY, so `is_some` doesn't
        // affect binding) and `left op ALL(…)`.
        ast::Expr::AnyOp {
            left,
            compare_op,
            right,
            op_span,
            ..
        } => bind_quantified(left, compare_op, right, false, op_span.0, scope),
        ast::Expr::AllOp {
            left,
            compare_op,
            right,
            op_span,
        } => bind_quantified(left, compare_op, right, true, op_span.0, scope),
        ast::Expr::Between {
            expr,
            negated,
            low,
            high,
        } => bind_between(expr, low, high, *negated, scope),
        // `ARRAY[...]` / `[...]` array constructor.
        ast::Expr::Array(arr) => bind_array_ctor(&arr.elem, scope),
        // `a[i]` array element access.
        ast::Expr::CompoundFieldAccess { root, access_chain } => {
            bind_subscript(root, access_chain, scope)
        }
        ast::Expr::Collate { expr, collation } => bind_collate(expr, collation, scope),
        other => Err(unsupported_expr(other)),
    }
}

/// Bind `expr COLLATE "name"`.
///
/// The clause only labels the operand — the value is unchanged — so the result
/// keeps the operand's type and the collation rides along in a
/// [`BoundExpr::Collate`] at *explicit* strength, overriding any collation the
/// operand already carried. An untyped literal (`'x' COLLATE "C"`) settles on
/// `text`, as PG does, since the clause proves it is a string.
fn bind_collate(
    expr: &ast::Expr,
    collation: &ast::ObjectName,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let oid = crate::collation::resolve_collation(collation)?;
    let bound = match bind_expr(expr, scope)? {
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, PgType::Text)?,
        Binding::Typed(e) => e,
    };
    let ty = bound.ty();
    if !ty.is_collatable() {
        return Err(BindError::new(
            sqlstate::WRONG_OBJECT_TYPE,
            format!("collations are not supported by type {}", ty.name()),
        ));
    }
    Ok(Binding::Typed(BoundExpr::Collate {
        expr: Box::new(bound),
        collation: oid,
        explicit: true,
    }))
}

/// Bind an `ARRAY[...]` constructor. Elements are bound and unified to a common
/// element type (untyped literals adapt to it), then coerced; the result type is
/// `PgType::Array(elem)`. An empty `ARRAY[]` settles on `text[]` and typically
/// takes its real type from a surrounding cast (`ARRAY[]::int[]`).
fn bind_array_ctor(elems: &[ast::Expr], scope: &Scope) -> Result<Binding, BindError> {
    // A bare, uncast `ARRAY[]` has no determinable element type. PG requires an
    // explicit cast; `ARRAY[]::t[]` is intercepted in `bind_cast` and never
    // reaches here empty.
    if elems.is_empty() {
        return Err(
            BindError::new("42P18", "cannot determine type of empty array").with_hint(Some(
                "Explicitly cast to the desired type, for example ARRAY[]::integer[].".to_string(),
            )),
        );
    }
    let bindings = elems
        .iter()
        .map(|e| bind_expr(e, scope))
        .collect::<Result<Vec<_>, _>>()?;
    let (elem, exprs) = unify_value_column(bindings, "ARRAY")?;
    // Reject an element type this build has no array type for — this also
    // rejects a multi-dimensional constructor (an array-typed element).
    if crabgresql_types::array::array_oid_for_elem(elem.oid()).is_none() {
        return Err(BindError::feature_not_supported(format!(
            "could not find array type for data type {}",
            elem.name()
        )));
    }
    if elem.is_collatable() {
        crate::collation::check_explicit_conflict(
            exprs.iter().map(crate::collation::expr_collation),
        )?;
    }
    Ok(Binding::Typed(BoundExpr::ArrayCtor {
        elem,
        ty: PgType::Array(elem.oid()),
        elems: exprs,
    }))
}

/// Bind an `a[i]` subscript. Only a single integer index on an array is
/// supported (slices and chained/multi-dim subscripts are `0A000`). The result
/// type is the array's element type.
fn bind_subscript(
    root: &ast::Expr,
    access_chain: &[ast::AccessExpr],
    scope: &Scope,
) -> Result<Binding, BindError> {
    let index_expr = match access_chain {
        [ast::AccessExpr::Subscript(ast::Subscript::Index { index })] => index,
        [ast::AccessExpr::Subscript(ast::Subscript::Slice { .. })] => {
            return Err(BindError::feature_not_supported(
                "array slice access is not supported yet",
            ));
        }
        // Chained/multi-dimensional subscripts and dotted field access.
        _ => {
            return Err(BindError::feature_not_supported(
                "multi-dimensional or field subscripting is not supported yet",
            ));
        }
    };
    let base = bind_scalar(root, scope)?;
    let elem = match base.ty() {
        PgType::Array(elem_oid) => PgType::from_oid(elem_oid).ok_or_else(|| {
            BindError::feature_not_supported("subscripting this array type is not supported yet")
        })?,
        // `oidvector`/`int2vector` subscript to their element type. Their lower
        // bound is 0 rather than 1, which the executor's `Subscript` evaluation
        // handles — nothing here depends on it.
        PgType::Vector(kind) => kind.element(),
        other => {
            return Err(BindError::new(
                sqlstate::DATATYPE_MISMATCH,
                format!(
                    "cannot subscript type {} because it does not support subscripting",
                    other.name()
                ),
            ));
        }
    };
    let index = coerce_expr(bind_scalar(index_expr, scope)?, PgType::Int4)?;
    Ok(Binding::Typed(BoundExpr::Subscript {
        base: Box::new(base),
        index: Box::new(index),
        ty: elem,
    }))
}

/// What a scope with no subquery context says when a subquery reaches it.
/// Named so a caller that *deliberately* builds such a scope — a CHECK
/// constraint, which PostgreSQL forbids subqueries in — can recognize its own
/// refusal and restate it in PostgreSQL's words.
pub(super) const NO_SUBQUERY_CONTEXT: &str = "subqueries are not supported in this context";

/// Bind a nested query into a [`LogicalPlan`] against the enclosing scope's
/// subquery context (table engine + visible CTEs). The subquery body is bound
/// in its own name scope, but with the enclosing scope's relations attached as
/// outer levels ([`Scope::as_outer_levels`]) so a correlated reference resolves
/// outward to a [`BoundExpr::OuterColumnRef`]; a name in neither still errors
/// `42703`.
fn bind_subquery_plan(
    query: &ast::Query,
    scope: &Scope,
) -> Result<(crate::logical_plan::LogicalPlan, Vec<crate::OutputColumn>), BindError> {
    let ctx = scope
        .subquery
        .as_ref()
        .ok_or_else(|| BindError::feature_not_supported(NO_SUBQUERY_CONTEXT))?;
    let plan = crate::plan::bind_query_scoped(
        &ctx.engine,
        scope.catalog(),
        scope.params(),
        query,
        &ctx.ctes,
        &scope.as_outer_levels(),
    )?;
    let columns = crate::plan::output_columns_of(&plan)?;
    Ok((plan, columns))
}

/// `(SELECT …)` as a scalar: the subquery must produce exactly one column; its
/// type is the expression's type. Runs once at execution and folds to that
/// value (0 rows → NULL, >1 rows → `21000`).
fn bind_scalar_subquery(query: &ast::Query, scope: &Scope) -> Result<Binding, BindError> {
    let (plan, columns) = bind_subquery_plan(query, scope)?;
    let [col] = columns.as_slice() else {
        return Err(BindError::new(
            sqlstate::SYNTAX_ERROR,
            "subquery must return only one column",
        ));
    };
    Ok(Binding::Typed(BoundExpr::ScalarSubquery {
        subplan: Subplan(Box::new(plan)),
        ty: col.ty,
    }))
}

/// `[NOT] EXISTS (SELECT …)` → a bool test on whether the subquery yields rows.
/// The projected columns are irrelevant (PG ignores them), so the target list is
/// replaced with a constant: the executor then only checks for a first row and
/// never evaluates the original projection (which could error or be expensive).
fn bind_exists(query: &ast::Query, negated: bool, scope: &Scope) -> Result<Binding, BindError> {
    let (plan, _columns) = bind_subquery_plan(query, scope)?;
    Ok(Binding::Typed(BoundExpr::Exists {
        subplan: Subplan(Box::new(crate::plan::strip_to_existence(plan))),
        negated,
    }))
}

/// `x [NOT] IN (SELECT …)`, which PostgreSQL defines as exactly `x = ANY (…)` /
/// `x <> ALL (…)` — so it binds to the same [`BoundExpr::QuantifiedSubquery`]
/// the `ANY`/`ALL` spellings produce. `NOT IN` becomes `<> ALL` rather than a
/// negated `= ANY`: the De Morgan dual keeps three-valued NULL handling right
/// without a wrapping `NOT` (mirroring how `bind_in_list` picks `(NotEq, And)`).
fn bind_in_subquery(
    expr: &ast::Expr,
    query: &ast::Query,
    negated: bool,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let op = if negated { BinOp::NotEq } else { BinOp::Eq };
    // `IN` has no operator token of its own to point a cursor at, so the
    // comparison resolves with an empty span, as it did before.
    bind_quantified_subquery(expr, op, query, negated, Span::empty(), scope)
}

/// `left op ANY(…)` / `left op SOME(…)` / `left op ALL(…)` (`all` selects `ALL`).
/// The right-hand operand is either a subquery (`ANY(SELECT …)`, →
/// [`BoundExpr::QuantifiedSubquery`]) or an array-valued expression
/// (`ANY(ARRAY[…])`, `ANY('{…}')`, → [`BoundExpr::QuantifiedArray`]; a `$n`
/// array parameter binds here too, but only reaches execution over the simple
/// protocol until `types::wire` gains a binary array decoder).
/// In both cases a NULL `Const` "hole" of the element type stands in for a
/// candidate and `bind_binary_op` resolves the operator/coercions exactly as a
/// written `left op v` would (the same trick as [`bind_in_subquery`]).
fn bind_quantified(
    left: &ast::Expr,
    compare_op: &ast::BinaryOperator,
    right: &ast::Expr,
    all: bool,
    op_span: Span,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let Some(op) = binop_from_comparison(compare_op) else {
        // The parser also accepts the LIKE/regex operator spellings after
        // ANY/ALL. Those lower to `ScalarFn` calls, not a `Binary` comparison
        // template, so the quantified path can't build a hole for them yet.
        return Err(BindError::feature_not_supported(format!(
            "{compare_op} {} (…) is not supported yet",
            if all { "ALL" } else { "ANY" }
        ))
        .at(op_span));
    };

    // The parser emits `Expr::Subquery` for the `ANY(SELECT …)` form (possibly
    // wrapped in redundant parentheses); anything else is an array expression.
    let mut rhs = right;
    while let ast::Expr::Nested(inner) = rhs {
        rhs = inner;
    }
    match rhs {
        ast::Expr::Subquery(query) => {
            bind_quantified_subquery(left, op, query, all, op_span, scope)
        }
        _ => bind_quantified_array(left, op, right, all, op_span, scope),
    }
}

/// The comparison subset of the `ast::BinaryOperator` → [`BinOp`] mapping (the
/// only operators a quantified comparison accepts). Shared with `bind_binary` so
/// a new comparison spelling can never bind for `a < b` but not `a < ANY(…)`.
pub(super) fn binop_from_comparison(op: &ast::BinaryOperator) -> Option<BinOp> {
    Some(match op {
        ast::BinaryOperator::Eq => BinOp::Eq,
        ast::BinaryOperator::NotEq => BinOp::NotEq,
        ast::BinaryOperator::Lt => BinOp::Lt,
        ast::BinaryOperator::LtEq => BinOp::LtEq,
        ast::BinaryOperator::Gt => BinOp::Gt,
        ast::BinaryOperator::GtEq => BinOp::GtEq,
        _ => return None,
    })
}

/// The subquery form of [`bind_quantified`]: the one-column subquery supplies
/// the candidate set. Also serves `x [NOT] IN (SELECT …)` via
/// [`bind_in_subquery`]. A subquery with more than one column errors.
fn bind_quantified_subquery(
    left: &ast::Expr,
    op: BinOp,
    query: &ast::Query,
    all: bool,
    op_span: Span,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let (plan, columns) = bind_subquery_plan(query, scope)?;
    let [col] = columns.as_slice() else {
        return Err(BindError::new(
            sqlstate::SYNTAX_ERROR,
            "subquery has too many columns",
        ));
    };
    let elem_ty = col.ty;
    let needle = bind_expr(left, scope)?;
    let cmp = bind_hole_template(op, needle, elem_ty, col.collation, op_span, scope)?;
    Ok(Binding::Typed(BoundExpr::QuantifiedSubquery {
        subplan: Subplan(Box::new(plan)),
        all,
        cmp: Box::new(cmp),
    }))
}

/// The array form of [`bind_quantified`]. The element type comes from the
/// right-hand array: a typed array contributes its element type; an untyped
/// literal (`'{1,2,3}'`) or bind parameter takes the needle's type (`text` when
/// the needle too is untyped) and is coerced to that array type — mirroring
/// [`bind_in_list`]'s unknown-literal policy. A right side that is not an array
/// (or whose element type has no `PgType`) is PG's `op ANY/ALL (array) requires
/// array on right side` error.
fn bind_quantified_array(
    left: &ast::Expr,
    op: BinOp,
    right: &ast::Expr,
    all: bool,
    op_span: Span,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let needle = bind_expr(left, scope)?;
    let array = bind_expr(right, scope)?;
    let elem_ty = match binding_typed_ty(&array) {
        Some(ty) => ty.array_element().ok_or_else(|| {
            BindError::new(
                sqlstate::WRONG_OBJECT_TYPE,
                "op ANY/ALL (array) requires array on right side",
            )
            .at(op_span)
        })?,
        // Untyped literal / bind parameter: element type follows the needle.
        None => binding_typed_ty(&needle).unwrap_or(PgType::Text),
    };
    // Coerce the right side to `elem_ty[]` (identity for an already-typed array;
    // parses `'{…}'` / types a `$n` param via `resolve_unknown`'s array arm).
    let array_expr = resolve_operand(&array, PgType::Array(elem_ty.oid()))?;
    // `PgType::Array` is never itself collatable (only the element is), and
    // nothing in this build tracks a per-element collation on an array value
    // yet, so the hole falls back to the element type's default collation —
    // unlike the subquery form, which does know its one column's collation.
    let cmp = bind_hole_template(op, needle, elem_ty, None, op_span, scope)?;
    Ok(Binding::Typed(BoundExpr::QuantifiedArray {
        array: Box::new(array_expr),
        all,
        cmp: Box::new(cmp),
    }))
}

/// Build a quantified comparison's `needle op <hole>` template, where `<hole>`
/// is a NULL `Const` of the candidate type. Binding it through
/// [`bind_binary_op`] resolves the operator, operand promotion and every
/// coercion exactly as a written `needle op candidate` would — and raises PG's
/// `operator does not exist` (pointed at `op_span`) when there is none. The
/// executor substitutes each candidate into that hole.
fn bind_hole_template(
    op: BinOp,
    needle: Binding,
    elem_ty: PgType,
    collation: Option<u32>,
    op_span: Span,
    scope: &Scope,
) -> Result<BoundExpr, BindError> {
    // Geometric comparisons exist in PG but lower to `ScalarFn::Geo` calls here,
    // not to a `Binary` with a substitutable RHS hole. `bind_binary_op` would
    // report "operator does not exist", which is untrue — the operator exists,
    // the quantified form just can't build a template for it yet.
    if is_geo_ty(Some(elem_ty)) || is_geo_ty(binding_typed_ty(&needle)) {
        return Err(BindError::feature_not_supported(format!(
            "{} ANY/ALL (…) on geometric types is not supported yet",
            op.sql_symbol()
        ))
        .at(op_span));
    }
    let placeholder = BoundExpr::Const {
        value: Value::Null,
        ty: elem_ty,
    };
    // Wrap the placeholder so `expr_collation` sees the candidate set's real
    // collation rather than a bare NULL's (which asserts none), the same way
    // a column reference of `elem_ty` would if we had a real one to bind.
    let hole = Binding::Typed(match collation {
        Some(collation) if elem_ty.is_collatable() => BoundExpr::Collate {
            expr: Box::new(placeholder),
            collation,
            explicit: false,
        },
        _ => placeholder,
    });
    let cmp = bind_binary_op(
        op,
        needle,
        hole,
        op_span,
        (Span::empty(), Span::empty()),
        scope.catalog().as_ref(),
    )?;
    match cmp {
        Binding::Typed(cmp @ BoundExpr::Binary { .. }) => Ok(cmp),
        // Any other comparison that lowers to a `ScalarFn` likewise has no hole
        // to substitute into; fail here rather than leaving the executor to trip
        // over a template shape it cannot destructure.
        _ => Err(BindError::feature_not_supported(format!(
            "{} ANY/ALL (…) on type {} is not supported yet",
            op.sql_symbol(),
            elem_ty.name()
        ))
        .at(op_span)),
    }
}

/// `SUBSTRING(x [FROM a] [FOR b])` → `substring(x, a[, b])`. With no `FROM`, PG
/// defaults the start to 1.
///
/// The name matters: overload resolution is what separates the positional form
/// from the two regex forms, and only `substring` has the latter. `SUBSTR` is
/// therefore resolved under its own name so `substr(x, '2')` keeps treating the
/// literal as an offset.
fn bind_substring(
    expr: &ast::Expr,
    from: Option<&ast::Expr>,
    for_: Option<&ast::Expr>,
    shorthand: bool,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let subject = bind_expr(expr, scope)?;
    let start = match from {
        Some(e) => bind_expr(e, scope)?,
        None => Binding::Typed(BoundExpr::Const {
            value: Value::Int4(1),
            ty: PgType::Int4,
        }),
    };
    let mut args = vec![subject, start];
    if let Some(e) = for_ {
        args.push(bind_expr(e, scope)?);
    }
    let name = if shorthand { "substr" } else { "substring" };
    crate::functions::resolve_call(name, args, scope.catalog())
}

/// `TRIM([LEADING|TRAILING|BOTH] [chars FROM] x)` → `ltrim`/`rtrim`/`btrim`.
fn bind_trim(
    expr: &ast::Expr,
    side: Option<ast::TrimWhereField>,
    trim_what: Option<&ast::Expr>,
    trim_characters: Option<&[ast::Expr]>,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let func = match side {
        Some(ast::TrimWhereField::Leading) => "ltrim",
        Some(ast::TrimWhereField::Trailing) => "rtrim",
        Some(ast::TrimWhereField::Both) | None => "btrim",
    };
    let subject = bind_expr(expr, scope)?;
    let mut args = vec![subject];
    // `TRIM(chars FROM x)` and the `TRIM(x, chars)` comma form both give a
    // characters argument.
    if let Some(chars) = trim_what {
        args.push(bind_expr(chars, scope)?);
    } else if let Some([chars]) = trim_characters {
        args.push(bind_expr(chars, scope)?);
    } else if trim_characters.is_some_and(|c| !c.is_empty()) {
        return Err(BindError::feature_not_supported(
            "TRIM with multiple characters is not supported yet",
        ));
    }
    crate::functions::resolve_call(func, args, scope.catalog())
}

/// `OVERLAY(x PLACING r FROM a [FOR b])` → `overlay(x, r, a[, b])`.
fn bind_overlay(
    expr: &ast::Expr,
    what: &ast::Expr,
    from: &ast::Expr,
    for_: Option<&ast::Expr>,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let mut args = vec![
        bind_expr(expr, scope)?,
        bind_expr(what, scope)?,
        bind_expr(from, scope)?,
    ];
    if let Some(e) = for_ {
        args.push(bind_expr(e, scope)?);
    }
    crate::functions::resolve_call("overlay", args, scope.catalog())
}

/// Bind a `LIKE`/`ILIKE` expression node (as opposed to the operator form).
fn bind_like_node(
    expr: &ast::Expr,
    pattern: &ast::Expr,
    escape_char: Option<&ast::ValueWithSpan>,
    any: bool,
    case_insensitive: bool,
    negated: bool,
    scope: &Scope,
) -> Result<Binding, BindError> {
    if any {
        return Err(BindError::feature_not_supported(
            "LIKE ANY is not supported yet",
        ));
    }
    let lb = bind_expr(expr, scope)?;
    let rb = bind_expr(pattern, scope)?;
    let escape = match escape_char {
        Some(v) => match v.value.as_pg_string() {
            Some(s) => Some(Binding::Typed(BoundExpr::Const {
                value: Value::Text(s.to_string()),
                ty: PgType::Text,
            })),
            None => {
                return Err(BindError::syntax(format!(
                    "invalid ESCAPE literal: {}",
                    v.value
                )));
            }
        },
        None => None,
    };
    bind_like(lb, rb, escape, case_insensitive, negated)
}

/// Bind at a spot with no surrounding type context (a SELECT-list item):
/// a leftover unknown resolves to text, as PG does in a bare SELECT.
pub fn bind_scalar(expr: &ast::Expr, scope: &Scope) -> Result<BoundExpr, BindError> {
    Ok(match bind_expr(expr, scope)? {
        Binding::Typed(e) => e,
        // A bare untyped literal defaults to text; but a bind parameter with no
        // surrounding context has no type to take, so PG errors 42P18 rather
        // than silently choosing text.
        Binding::Unknown {
            param: Some((index, _)),
            ..
        } => {
            return Err(BindError::new(
                "42P18",
                format!("could not determine data type of parameter ${}", index + 1),
            ));
        }
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, PgType::Text)?,
    })
}

/// Bind a SELECT-list item. A top-level call to a set-returning function
/// (currently `generate_series`) binds to a [`BoundExpr::Srf`] marker that the
/// executor's `ProjectSet` node expands into rows; everything else binds as an
/// ordinary scalar via [`bind_scalar`].
pub fn bind_projection(expr: &ast::Expr, scope: &Scope) -> Result<BoundExpr, BindError> {
    if let ast::Expr::Function(func) = expr
        && let Some(srf) = bind_srf_projection(func, scope)?
    {
        return Ok(srf);
    }
    bind_scalar(expr, scope)
}

pub(super) fn unsupported_expr(expr: &ast::Expr) -> BindError {
    BindError::feature_not_supported(format!("expression is not supported yet: {expr}"))
}

/// `CASE`: both the searched form (`CASE WHEN cond THEN r ...`) and the simple
/// form (`CASE operand WHEN v THEN r ...`, sugar for `CASE WHEN operand = v`).
/// Conditions are forced to boolean; all `THEN`/`ELSE` results resolve to one
/// common type the same way a `VALUES`/`UNION` column does.
fn bind_case(
    operand: Option<&ast::Expr>,
    conditions: &[ast::CaseWhen],
    else_result: Option<&ast::Expr>,
    scope: &Scope,
) -> Result<Binding, BindError> {
    // Simple CASE evaluates the operand once at run time; we re-bind a clone of
    // it per WHEN to build `operand = value`. That is equivalent because our
    // scalar expressions are pure (no volatile functions reach here yet).
    //
    // PG gives an untyped-literal operand its own type before comparing — an
    // unknown resolves to text (its default), independent of the WHEN values —
    // so `CASE NULL WHEN 1` is `text = integer` (operator does not exist), not
    // an attempt to read the operand as integer. Resolve it here to reproduce
    // that; a typed operand is left as-is.
    let operand = match operand {
        None => None,
        Some(e) => Some(match bind_expr(e, scope)? {
            Binding::Unknown { lit, span, param } => {
                Binding::Typed(resolve_unknown(lit, span, param, PgType::Text)?)
            }
            typed => typed,
        }),
    };

    // Bind everything in source order (operand, then each WHEN's condition and
    // result, then ELSE) so bind-time errors surface where PG's do.
    let mut conds = Vec::with_capacity(conditions.len());
    let mut then_bindings = Vec::with_capacity(conditions.len());
    for when in conditions {
        let cond = match &operand {
            None => to_bool_operand(
                bind_expr(&when.condition, scope)?,
                "CASE/WHEN",
                when.condition.span(),
            )?,
            Some(op) => {
                let value = bind_expr(&when.condition, scope)?;
                match bind_binary_op(
                    BinOp::Eq,
                    op.clone(),
                    value,
                    Span::empty(),
                    (Span::empty(), Span::empty()),
                    scope.catalog().as_ref(),
                )? {
                    Binding::Typed(e) => e,
                    // `=` always resolves to a typed boolean expression.
                    Binding::Unknown { .. } => unreachable!("= yields a typed bool"),
                }
            }
        };
        conds.push(cond);
        then_bindings.push(bind_expr(&when.result, scope)?);
    }
    // A missing ELSE is NULL, which is compatible with any type and needs no
    // coercion node.
    let else_binding = else_result.map(|e| bind_expr(e, scope)).transpose()?;
    let has_else = else_binding.is_some();

    // Result-type unification lists the ELSE result first, then the WHEN
    // results, matching the operand order PG uses for its "CASE types A and B
    // cannot be matched" message.
    let mut result_bindings = Vec::with_capacity(then_bindings.len() + 1);
    result_bindings.extend(else_binding);
    result_bindings.extend(then_bindings);
    let (ty, mut results) = unify_value_column(result_bindings, "CASE")?;

    let else_ = if has_else {
        Some(Box::new(results.remove(0)))
    } else {
        None
    };
    let whens: Vec<_> = conds.into_iter().zip(results).collect();

    if ty.is_collatable() {
        crate::collation::check_explicit_conflict(
            else_
                .iter()
                .map(|e| crate::collation::expr_collation(e))
                .chain(
                    whens
                        .iter()
                        .map(|(_, r)| crate::collation::expr_collation(r)),
                ),
        )?;
    }

    Ok(Binding::Typed(BoundExpr::Case { whens, else_, ty }))
}

/// `x IN (a, b, c)` desugars to `x = a OR x = b OR x = c`; `x NOT IN (...)` to
/// `x <> a AND x <> b AND x <> c`. Both reproduce PG's three-valued logic (a
/// NULL element yields NULL, not false — the executor's Kleene `OR`/`AND`) and
/// per-element type resolution: each comparison is bound through the shared
/// `bind_binary_op`, so the left operand's unknown-literal typing, numeric
/// promotion, and `operator does not exist` / `invalid input syntax` errors all
/// match a written `x = a`. The left `Binding` is left unresolved and cloned per
/// element (like a simple `CASE operand`), so `'5' IN (5, 6)` types `'5'` from
/// the list as int4 rather than defaulting it to text.
fn bind_in_list(
    expr: &ast::Expr,
    list: &[ast::Expr],
    negated: bool,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let left = bind_expr(expr, scope)?;
    let items = list
        .iter()
        .map(|item| bind_expr(item, scope))
        .collect::<Result<Vec<_>, _>>()?;
    let (cmp, chain) = if negated {
        (BinOp::NotEq, BinOp::And)
    } else {
        (BinOp::Eq, BinOp::Or)
    };
    // An empty list is a parser syntax error (`IN ()`), so this is unreachable;
    // fold to the constant PG's `= ANY '{}'` yields rather than panic.
    if items.is_empty() {
        return Ok(Binding::Typed(BoundExpr::Const {
            value: Value::Bool(negated),
            ty: PgType::Bool,
        }));
    }

    // PG lowers `x IN (list)` to `x = ANY(ARRAY[list])`: the list is coerced to
    // one common type (its array element type, which excludes the tested
    // expression), then each `x = element` resolves as an operator — so the left
    // keeps its own type and the comparison still promotes (`int`/`real` ->
    // `float8`, temporal/money handling), matching `x = v`. Coercing the elements
    // to the list type first is observable: an int that overflows a float4
    // mantissa rounds when the list type is `real`, exactly as PG's array does.
    let elem_target = match in_list_type(&items) {
        ListType::Uniform(ty) => Some(ty),
        // `x IN (NULL)` settles the untyped elements on the tested expression's
        // type (`text` when it too is untyped), so `1 IN (NULL)` compares in int
        // and `NULL IN (NULL)` in text — never the two-unknown ambiguity error.
        ListType::AllUnknown => Some(binding_typed_ty(&left).unwrap_or(PgType::Text)),
        // An incompatible list leaves each element as-is so the pair resolves on
        // its own — PG's OR fallback and its `operator does not exist` error.
        ListType::Incompatible => None,
    };
    let mut acc: Option<Binding> = None;
    for item in &items {
        let right = match elem_target {
            Some(ty) => Binding::Typed(resolve_operand(item, ty)?),
            None => item.clone(),
        };
        let comparison = bind_binary_op(
            cmp,
            left.clone(),
            right,
            Span::empty(),
            (Span::empty(), Span::empty()),
            scope.catalog().as_ref(),
        )?;
        acc = Some(match acc {
            None => comparison,
            Some(prev) => bind_binary_op(
                chain,
                prev,
                comparison,
                Span::empty(),
                (Span::empty(), Span::empty()),
                scope.catalog().as_ref(),
            )?,
        });
    }
    Ok(acc.expect("non-empty list yields at least one comparison"))
}

/// Bind `x BETWEEN low AND high` by desugaring into the pair of comparisons PG
/// itself emits, reusing `bind_binary_op` so each pair resolves with the same
/// type promotion, unknown-literal typing, "operator does not exist" errors, and
/// three-valued NULL handling as a written comparison:
///
/// - `x BETWEEN low AND high`     -> `(x >= low) AND (x <= high)`
/// - `x NOT BETWEEN low AND high` -> `(x < low) OR (x > high)`
///
/// The `NOT` form is the De Morgan dual of the positive one (`<`/`>` chained
/// with `OR`), which keeps it Kleene-correct for NULL bounds — mirroring how
/// `bind_in_list` picks `(NotEq, And)` vs `(Eq, Or)`. The tested expression is
/// bound twice, as `IN (list)` re-binds its left operand per element.
///
/// The low comparison is resolved before the high bound is even bound, so a
/// malformed `BETWEEN` surfaces the low-side error first — matching PG's
/// left-to-right analysis of `(a >= b) AND (a <= c)`, which fully resolves
/// `a >= b` (coercing `b`) before it looks at `c`.
fn bind_between(
    expr: &ast::Expr,
    low: &ast::Expr,
    high: &ast::Expr,
    negated: bool,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let (cmp_lo, cmp_hi, chain) = if negated {
        (BinOp::Lt, BinOp::Gt, BinOp::Or)
    } else {
        (BinOp::GtEq, BinOp::LtEq, BinOp::And)
    };
    let catalog = scope.catalog();
    let left = bind_expr(expr, scope)?;
    let low = bind_expr(low, scope)?;
    let lo = bind_binary_op(
        cmp_lo,
        left.clone(),
        low,
        Span::empty(),
        (Span::empty(), Span::empty()),
        catalog.as_ref(),
    )?;
    let high = bind_expr(high, scope)?;
    let hi = bind_binary_op(
        cmp_hi,
        left,
        high,
        Span::empty(),
        (Span::empty(), Span::empty()),
        catalog.as_ref(),
    )?;
    bind_binary_op(
        chain,
        lo,
        hi,
        Span::empty(),
        (Span::empty(), Span::empty()),
        catalog.as_ref(),
    )
}

/// How an `IN` list resolves to the element type of PG's `= ANY(ARRAY[...])`.
enum ListType {
    /// The typed elements share this common type; coerce every element to it.
    Uniform(PgType),
    /// No typed elements (`x IN (NULL)`); the caller falls back to the tested
    /// expression's type (or `text` when it too is untyped).
    AllUnknown,
    /// The typed elements have no common type; leave each element as-is so the
    /// pair resolves on its own, reproducing PG's `operator does not exist` error.
    Incompatible,
}

/// Fold `merge_types` (PG's `select_common_type`) over the `IN` list's typed
/// elements — the array element type of PG's `= ANY(ARRAY[...])`, which excludes
/// the tested expression (so `x IN (1, 0::float4)` rounds the `1` to `real` as
/// PG's array does).
fn in_list_type(items: &[Binding]) -> ListType {
    let mut common: Option<PgType> = None;
    for b in items {
        if let Some(ty) = binding_typed_ty(b) {
            common = Some(match common {
                None => ty,
                Some(prev) => match merge_types(prev, ty) {
                    Some(m) => m,
                    None => return ListType::Incompatible,
                },
            });
        }
    }
    match common {
        Some(ty) => ListType::Uniform(ty),
        None => ListType::AllUnknown,
    }
}

/// The result-column name PG derives from an expression's syntax: column
/// references keep their name (through parens), casts take the target type's
/// name, boolean literals are named after the type, everything else is
/// `?column?`.
pub(crate) fn output_name(expr: &ast::Expr) -> String {
    match expr {
        ast::Expr::Identifier(ident) => normalize_ident(ident),
        ast::Expr::CompoundIdentifier(parts) => parts
            .last()
            .map(normalize_ident)
            .unwrap_or_else(|| "?column?".into()),
        ast::Expr::Nested(inner) => output_name(inner),
        // `COLLATE` is value-transparent, like a cast that keeps the type: it
        // takes the wrapped expression's name, not its own.
        ast::Expr::Collate { expr, .. } => output_name(expr),
        ast::Expr::Value(v) if matches!(v.value, ast::Value::Boolean(_)) => "bool".into(),
        // PG keeps a bare column's name through a cast (`id::int8` → "id"), but
        // uses the target type name when the argument has no inherent name
        // (`(1+1)::int8`, `'nan'::numeric::float4` → the type). Only a direct
        // column reference (strength 2) is preserved; a nested cast is not.
        ast::Expr::Cast {
            expr, data_type, ..
        } => column_name(expr).unwrap_or_else(|| type_output_name(data_type)),
        ast::Expr::TypedString(ts) => type_output_name(&ts.data_type),
        // `interval '...'` is named after the type, like a typed literal.
        ast::Expr::Interval(_) => "interval".into(),
        // EXTRACT(... ) is named "extract" in PG, regardless of the field.
        ast::Expr::Extract { .. } => "extract".into(),
        // `x AT TIME ZONE y` lowers to timezone(); PG names the column "timezone".
        ast::Expr::AtTimeZone { .. } | ast::Expr::AtLocal { .. } => "timezone".into(),
        // An `ARRAY[...]` constructor is named "array" in PG.
        ast::Expr::Array(_) => "array".into(),
        // `a[i]` subscript keeps the base's name, like a bare column through a
        // cast (`a[1]` → "a"); a non-name base falls through to `?column?`.
        ast::Expr::CompoundFieldAccess { root, .. } => output_name(root),
        // A bare CASE expression is named "case" in PG.
        ast::Expr::Case { .. } => "case".into(),
        // CEIL/FLOOR special syntax is named after the function.
        ast::Expr::Ceil { .. } => "ceil".into(),
        ast::Expr::Floor { .. } => "floor".into(),
        // String special-syntax expressions are named after the function they
        // desugar to (`TRIM` → its ltrim/rtrim/btrim variant).
        ast::Expr::Substring { shorthand, .. } => {
            if *shorthand {
                "substr".into()
            } else {
                "substring".into()
            }
        }
        ast::Expr::Position { .. } => "position".into(),
        ast::Expr::Overlay { .. } => "overlay".into(),
        ast::Expr::Trim { trim_where, .. } => match trim_where {
            Some(ast::TrimWhereField::Leading) => "ltrim".into(),
            Some(ast::TrimWhereField::Trailing) => "rtrim".into(),
            Some(ast::TrimWhereField::Both) | None => "btrim".into(),
        },
        // A function's output column is named after the function.
        ast::Expr::Function(func) => func
            .name
            .0
            .last()
            .and_then(|p| p.as_ident())
            .map(normalize_ident)
            .unwrap_or_else(|| "?column?".into()),
        // `EXISTS (…)` is named "exists" in PG.
        ast::Expr::Exists { .. } => "exists".into(),
        // A scalar `(SELECT …)` takes the name of the subquery's single output
        // column (`(SELECT max(x))` → "max", `(SELECT y)` → "y").
        ast::Expr::Subquery(query) => subquery_output_name(query),
        _ => "?column?".into(),
    }
}

/// The output-column name of a scalar `(SELECT …)`: the name of the subquery's
/// first (and only) target-list column — an alias if present, else the item
/// expression's own [`output_name`]. Anything that isn't a plain `SELECT`
/// (e.g. `VALUES`) or whose first item is a wildcard falls back to `?column?`.
fn subquery_output_name(query: &ast::Query) -> String {
    let ast::SetExpr::Select(select) = query.body.as_ref() else {
        return "?column?".into();
    };
    match select.projection.first() {
        Some(ast::SelectItem::UnnamedExpr(expr)) => output_name(expr),
        Some(ast::SelectItem::ExprWithAlias { alias, .. }) => normalize_ident(alias),
        _ => "?column?".into(),
    }
}

/// The name of a bare column reference (through parens), if any — PG's
/// strength-2 name that survives an enclosing cast. A cast, value, or function
/// argument has no such name.
fn column_name(expr: &ast::Expr) -> Option<String> {
    match expr {
        ast::Expr::Identifier(ident) => Some(normalize_ident(ident)),
        ast::Expr::CompoundIdentifier(parts) => parts.last().map(normalize_ident),
        ast::Expr::Nested(inner) => column_name(inner),
        _ => None,
    }
}

fn type_output_name(data_type: &ast::DataType) -> String {
    map_data_type(data_type)
        .map(|ty| match ty {
            // A cast to an array type is named after the *element* type in PG
            // (`'{1}'::int[]` → column "int4"), not the `_int4` array typname.
            PgType::Array(elem) => PgType::from_oid(elem)
                .map_or_else(|| ty.typname().to_string(), |e| e.typname().to_string()),
            _ => ty.typname().to_string(),
        })
        .unwrap_or_else(|_| {
            // A user-defined type (e.g. an enum) is named after the type itself, as
            // PG does (`'red'::rainbow` → column "rainbow").
            custom_type_name(data_type).unwrap_or_else(|| "?column?".into())
        })
}
