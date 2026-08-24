//! Assignment context: coercing an expression into a column or a parameter, the
//! DDL expressions bound against a table (DEFAULT, CHECK), and the deparse that
//! renders a stored default back to SQL.

use std::collections::BTreeSet;
use std::sync::Arc;

use crabgresql_parser::ast::Spanned;
use crabgresql_parser::{Span, ast};
use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::{Column, TableSchema, TypeCatalog};
use crabgresql_types::{PgType, Value};

use crate::BindError;
use crate::functions::ScalarFn;

use super::bind::{NO_SUBQUERY_CONTEXT, bind_expr};
use super::bound::BoundExpr;
use super::coerce::{
    coerce_expr, implicit_castable, resolve_unknown_ctx, to_bool_operand, type_label,
};
use super::datatype::apply_length_to_column;
use super::domain::{domain_of, undomain_binding, wrap_domain};
use super::operators::{is_bit_family, is_text_family};
use super::params::param_ctx_none;
use super::scope::{Binding, Scope, reject_agg_or_window};

/// Coerce an expression for assignment into a column (INSERT / UPDATE SET),
/// with PG's column-context error message on a type mismatch.
pub fn coerce_to_column(
    binding: Binding,
    column: &Column,
    scope: &Scope,
) -> Result<BoundExpr, BindError> {
    let base = coerce_assign(binding, column.ty, scope, |from, to| {
        BindError::new(
            sqlstate::DATATYPE_MISMATCH,
            format!(
                "column \"{}\" is of type {} but expression is of type {}",
                column.name, to, from
            ),
        )
    })?;
    apply_length_to_column(base, column)
}

/// Coerce an `EXECUTE` argument to the type its prepared statement declared for
/// `$n`. The rules are a column assignment's — `EXECUTE p(1.7)` rounds into an
/// `int` parameter — but the rejection is PG's parameter-context message, which
/// names the parameter rather than a column.
pub fn coerce_to_param(
    binding: Binding,
    n1: usize,
    ty: PgType,
    span: Span,
    scope: &Scope,
) -> Result<BoundExpr, BindError> {
    coerce_assign(binding, ty, scope, |from, to| {
        BindError::new(
            sqlstate::DATATYPE_MISMATCH,
            format!("parameter ${n1} of type {from} cannot be coerced to the expected type {to}"),
        )
        .with_hint(Some(
            "You will need to rewrite or cast the expression.".to_string(),
        ))
        .at(span)
    })
}

/// PostgreSQL's assignment-context coercion, shared by column assignment and
/// `EXECUTE` parameter binding. The two differ only in how they word a
/// mismatch, so `mismatch` builds that error from `(source, target)` type
/// labels.
fn coerce_assign(
    binding: Binding,
    target: PgType,
    scope: &Scope,
    mismatch: impl FnOnce(String, String) -> BindError,
) -> Result<BoundExpr, BindError> {
    // A domain on the *source* side is just its base value, so it assigns
    // wherever the base does: `INSERT INTO int_tbl(x) SELECT posint_col` is
    // accepted by PostgreSQL and would otherwise reach the 42804 below, which
    // knows no coercion out of a `PgType::User`. Skipped when the target is
    // that very domain, so the wrap below still sees the two types as equal.
    let binding = match &binding {
        Binding::Typed(e) if e.ty() == target => binding,
        _ => undomain_binding(binding, scope.catalog().as_ref()),
    };
    // A source that is already this very domain skips the wrap: the value is
    // in, and PostgreSQL does not re-check it (`UPDATE t SET a = a` never
    // re-validates).
    if !matches!(&binding, Binding::Typed(e) if e.ty() == target)
        && let Some(info) = domain_of(target, scope.catalog().as_ref())
    {
        let base = scope.catalog().base_type(target);
        let inner = coerce_assign(binding, base, scope, mismatch)?;
        return wrap_domain(inner, &info, scope.catalog(), false);
    }
    let bound = match binding {
        Binding::Unknown { lit, span, param } => {
            resolve_unknown_ctx(scope.catalog(), lit, span, param, target)?
        }
        Binding::Typed(e) => {
            let ty = e.ty();
            if ty == target {
                e
            } else if ty.is_numeric() && target.is_numeric() {
                coerce_expr(e, target)?
            // PG assignment context permits coercion via I/O to a string-category
            // target (the source's output function), so any type assigns to
            // text/varchar/char/name (e.g. INSERT ... VALUES (2) into varchar).
            } else if is_text_family(target) {
                coerce_expr(e, target)?
            // `bit` and `bit varying` assign to each other (shared value); the
            // length rule is applied by `apply_length_to_column`.
            } else if is_bit_family(Some(ty)) && is_bit_family(Some(target)) {
                coerce_expr(e, target)?
            // Assignment context also permits the implicit `timestamp ->
            // timestamptz` cast and its assignment-only reverse (both convert
            // through the session zone, exactly as `AT TIME ZONE` does, so
            // neither folds at bind time), so inserting a `timestamp`
            // expression into a `timestamptz` column works, as in PG.
            // ... and the pairs `pg_cast` marks assignment-only. `"char"` needs
            // them spelled out because it is category `Z`, not `S`: PG's
            // I/O-coercion shortcut for string-category targets does not apply,
            // so `text`/`varchar`/`bpchar` assign into a `"char"` column
            // (truncating to the first byte) while `name`, `int4` and every
            // other source are rejected there too.
            } else if implicit_castable(ty, target)
                || matches!(
                    (ty, target),
                    (PgType::TimestampTz, PgType::Timestamp)
                        | (PgType::Text, PgType::Char)
                        | (PgType::Varchar, PgType::Char)
                        | (PgType::Bpchar, PgType::Char)
                )
            {
                coerce_expr(e, target)?
            } else {
                return Err(mismatch(
                    type_label(ty, scope.catalog().as_ref()),
                    type_label(target, scope.catalog().as_ref()),
                ));
            }
        }
    };
    Ok(bound)
}

/// Bind and assignment-coerce a column default in an empty scope. PostgreSQL
/// defaults cannot reference columns of the row being created.
pub fn bind_column_default(
    expr: &ast::Expr,
    column: &Column,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<BoundExpr, BindError> {
    let params = param_ctx_none();
    let scope = Scope::empty(catalog, &params);
    let bound = coerce_to_column(bind_expr(expr, &scope)?, column, &scope)?;
    if bound.contains_srf() {
        return Err(BindError::feature_not_supported(
            "set-returning functions are not allowed in DEFAULT expressions",
        ));
    }
    reject_agg_or_window(&bound, "DEFAULT expressions")?;
    Ok(bound)
}

/// Bind a `CHECK` constraint against the relation it constrains, returning the
/// predicate and the column positions it reads (`pg_constraint.conkey`).
///
/// Unlike a DEFAULT, a CHECK binds in a scope over `schema`'s columns — it is a
/// statement about the row. The scope is unqualified-plus-`schema.name`, so both
/// `x > 3` and `t.x > 3` resolve, as they do in PostgreSQL.
///
/// The rejections and their SQLSTATEs were probed against PostgreSQL 18.4:
///
/// * a non-boolean predicate is `42804`, via the same [`to_bool_operand`] every
///   other boolean position uses, so it carries the `LINE n: … ^` cursor;
/// * an aggregate is `42803` and a window function `42P20`, which is what
///   [`reject_agg_or_window`] already distinguishes;
/// * a set-returning function is `0A000`;
/// * a subquery is `0A000` — *not* a `42P17` — and is detected by
///   [`BoundExpr::collect_column_refs`] refusing. That refusal is exact here:
///   its only other refusing variant is a window marker, ruled out one line
///   above, so a `false` at this point is a subplan and nothing else.
///
/// A **volatile** function is deliberately allowed. PostgreSQL accepts
/// `CHECK (a < nextval('s'))` and leaves the consequences to whoever wrote it;
/// the upstream `constraints` regress test depends on that.
pub fn bind_check_constraint(
    expr: &ast::Expr,
    schema: &TableSchema,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<(BoundExpr, Vec<usize>), BindError> {
    // PostgreSQL points its cursor at the start of the CHECK expression for
    // every one of these — the unknown column, the subquery, the aggregate.
    // Only `to_bool_operand` sets a finer one of its own, so anything still
    // location-less gets the predicate's own span.
    bind_check_inner(expr, schema, catalog).map_err(|e| e.at_if_unset(expr.span()))
}

fn bind_check_inner(
    expr: &ast::Expr,
    schema: &TableSchema,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<(BoundExpr, Vec<usize>), BindError> {
    let params = param_ctx_none();
    // The scope's missing subquery context is what makes a subquery in the
    // predicate fail — restated below in PostgreSQL's words. PostgreSQL records
    // a system-column reference as a negative `conkey` and evaluates it per row;
    // this scope exposes none, so such a predicate is refused instead.
    //
    // TODO: CHECK over a system column, which needs `conkey` to carry negative
    // attnums and the predicate to be evaluated against the leaf's identity.
    let scope = Scope::stored_row(schema, catalog, &params);
    let bound = to_bool_operand(
        bind_expr(expr, &scope).map_err(subquery_in_check)?,
        "CHECK",
        expr.span(),
    )?;
    if bound.contains_srf() {
        return Err(BindError::feature_not_supported(
            "set-returning functions are not allowed in check constraints",
        ));
    }
    reject_agg_or_window(&bound, "check constraints")?;
    let mut columns = BTreeSet::new();
    if !bound.collect_column_refs(&mut columns) {
        return Err(subquery_in_check_error());
    }
    Ok((bound, columns.into_iter().collect()))
}

/// Reparse a persisted expression (a column default, a generation expression)
/// back into an AST node. Storing canonical SQL keeps the storage API
/// independent of the parser/binder IR; `what` names the kind in the syntax
/// error a corrupt catalog would raise.
pub fn parse_stored_expr(sql: &str, what: &str) -> Result<ast::Expr, BindError> {
    crate::ruleutils::parse_expression(sql)
        .ok_or_else(|| BindError::syntax(format!("invalid stored {what}")))
}

/// Reparse and bind the generation expression `schema`'s column at `index`
/// carries, ready to be evaluated against a row of that shape.
///
/// The DDL path validated the expression once, when the column was declared, so
/// this re-runs the same binding rather than the checks around it: what a
/// statement needs here is the tree, and a rejection at this point would be a
/// catalog that no longer matches its own relation.
pub fn bind_stored_generation(
    schema: &TableSchema,
    index: usize,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<BoundExpr, BindError> {
    let column = &schema.columns[index];
    let generated = column
        .generated
        .as_ref()
        .expect("caller checked the column is generated");
    let expr = parse_stored_expr(&generated.expr, "generation expression")?;
    let params = param_ctx_none();
    let scope = Scope::stored_row(schema, catalog, &params);
    coerce_to_column(bind_expr(&expr, &scope)?, column, &scope)
}

/// Bind a column's generation expression against the relation it belongs to,
/// returning the expression coerced to the column's type and the column
/// positions it reads.
///
/// Like a CHECK — and unlike a DEFAULT — this binds in a scope over `schema`'s
/// columns: it is a statement about the row. `schema` must already carry every
/// column in its final position, and `column` is the generated column's own
/// index in it.
///
/// The rejections and their SQLSTATEs were probed against PostgreSQL 18.4:
///
/// * a reference to any generated column, including the column itself, is
///   `42P17` with a DETAIL naming the rule;
/// * a non-immutable expression is `42P17` — PostgreSQL demands IMMUTABLE here,
///   so `now()` (STABLE) is refused as firmly as `random()` (VOLATILE);
/// * an aggregate is `42803` and a window function `42P20`, which is what
///   [`reject_agg_or_window`] already distinguishes;
/// * a subquery and a set-returning function are both `0A000`.
///
/// A reference to a *system* column is `42P10` upstream (`cannot use system
/// column "xmin" …`). The scope built here exposes none, so such a reference
/// reports `42703` (`column "xmin" does not exist`) instead — the same refusal
/// a CHECK over a system column already gives.
pub fn bind_generation_expr(
    expr: &ast::Expr,
    schema: &TableSchema,
    column: usize,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<(BoundExpr, Vec<usize>), BindError> {
    // PostgreSQL points its cursor at the start of the generation expression for
    // the parse-time refusals, exactly as it does for a CHECK.
    let (bound, columns) = bind_generation_inner(expr, schema, column, catalog)
        .map_err(|e| e.at_if_unset(expr.span()))?;
    // Immutability is checked *after* that stamping, and deliberately: PG
    // reports this one with no cursor at all, having already left the parse
    // analysis that would carry a position.
    if !is_immutable(&bound) {
        return Err(BindError::new(
            "42P17",
            "generation expression is not immutable",
        ));
    }
    Ok((bound, columns))
}

fn bind_generation_inner(
    expr: &ast::Expr,
    schema: &TableSchema,
    column: usize,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<(BoundExpr, Vec<usize>), BindError> {
    let params = param_ctx_none();
    // The scope's missing subquery context is what refuses a subquery below, in
    // PostgreSQL's words.
    let scope = Scope::stored_row(schema, catalog, &params);
    let bound = coerce_to_column(
        bind_expr(expr, &scope).map_err(subquery_in_generation)?,
        &schema.columns[column],
        &scope,
    )?;
    if bound.contains_srf() {
        return Err(BindError::feature_not_supported(
            "set-returning functions are not allowed in column generation expressions",
        ));
    }
    reject_agg_or_window(&bound, "column generation expressions")?;
    let mut columns = BTreeSet::new();
    if !bound.collect_column_refs(&mut columns) {
        return Err(subquery_in_generation_error());
    }
    // A self-reference is just the same rule applied to this column, which is
    // already marked generated in `schema` by the time this runs.
    for &index in &columns {
        if schema.columns[index].generated.is_some() {
            return Err(BindError::new(
                "42P17",
                format!(
                    "cannot use generated column \"{}\" in column generation expression",
                    schema.columns[index].name
                ),
            )
            .with_detail(Some(
                "A generated column cannot reference another generated column.".to_string(),
            )));
        }
    }
    Ok((bound, columns.into_iter().collect()))
}

/// Whether every function call in `expr` is IMMUTABLE in PostgreSQL's sense —
/// the bar a generation expression has to clear.
///
/// [`BoundExpr::contains_volatile_fn`] is not enough on its own: it deliberately
/// treats `now()` and `statement_timestamp()` as the STABLE functions they are,
/// so that a predicate over them can still be pushed down. A generation
/// expression may use neither, so the STABLE calls the binder knows are listed
/// here as well.
///
/// Known gap: a coercion between `timestamp` and `timestamptz` without an
/// explicit zone is STABLE upstream (it reads the session's `TimeZone`), and is
/// accepted here. Nothing else in the tree reads session state without going
/// through one of the calls below.
fn is_immutable(expr: &BoundExpr) -> bool {
    if expr.contains_volatile_fn() {
        return false;
    }
    let mut immutable = true;
    crate::plan::for_each_subexpr(expr, 1, &mut |e, _| {
        if let BoundExpr::FuncCall { func, .. } = e
            && matches!(
                func,
                ScalarFn::TransactionTimestamp
                    | ScalarFn::StatementTimestamp
                    | ScalarFn::PgPostmasterStartTime
                    | ScalarFn::CurrentSetting
                    | ScalarFn::Version
            )
        {
            immutable = false;
        }
    });
    immutable
}

/// PostgreSQL's wording for a subquery inside a generation expression: `0A000`,
/// like the CHECK case. Probed against 18.4.
fn subquery_in_generation_error() -> BindError {
    BindError::feature_not_supported("cannot use subquery in column generation expression")
}

/// Restate the generic "no subquery context here" refusal as the generation-
/// expression one. Any other error passes through untouched.
fn subquery_in_generation(e: BindError) -> BindError {
    match e.message == NO_SUBQUERY_CONTEXT {
        true => subquery_in_generation_error(),
        false => e,
    }
}

/// PostgreSQL's wording for a subquery inside a CHECK: `0A000`, and *not* the
/// `42P17` the SQLSTATE name might suggest. Probed against 18.4.
fn subquery_in_check_error() -> BindError {
    BindError::feature_not_supported("cannot use subquery in check constraint")
}

/// Restate the generic "no subquery context here" refusal as the CHECK-specific
/// one. Any other error passes through untouched.
fn subquery_in_check(e: BindError) -> BindError {
    match e.message == NO_SUBQUERY_CONTEXT {
        true => subquery_in_check_error(),
        false => e,
    }
}

/// [`subquery_in_check`] for an `EXECUTE` argument, which PostgreSQL refuses
/// with its own wording — also `0A000`, probed against 18.4. Any other error
/// passes through untouched.
pub fn subquery_in_execute_param(e: BindError) -> BindError {
    match e.message == NO_SUBQUERY_CONTEXT {
        true => BindError::feature_not_supported("cannot use subquery in EXECUTE parameter"),
        false => e,
    }
}

/// The text PostgreSQL's `pg_get_expr(adbin, adrelid, true)` prints for a column
/// default that is a bare *literal*, or `None` when this expression is not one —
/// in which case the caller keeps the statement's own source text.
///
/// PostgreSQL stores the default as a node tree coerced to the column's type and
/// deparses it on demand, so `b bit(4) DEFAULT '1001'` comes back as
/// `'1001'::"bit"`, not as written. crabgresql stores SQL text (see
/// [`Column::default`]), so the canonical form has to be produced here, once, at
/// DDL time. The rules below were probed against PostgreSQL 18.4:
///
/// * The type label is the **literal's own** type, never the column's: psql
///   passes `pretty`, which hides the implicit coercion to the column type. So
///   `'1001'` — an untyped constant, typed from context — labels itself with the
///   column's type (`"bit"` in a `bit(4)` column, `bit varying` in a `bit
///   varying(5)` one), while `B'0101'` is already `bit` and stays `'0101'::"bit"`
///   in *both*. Same reason `i bigint DEFAULT 42` prints a bare `42` (the
///   literal is `int4`) and `n numeric DEFAULT -1` prints `'-1'::integer`.
/// * The label never carries a modifier, so it is `format_type(oid, -1)` — which
///   is where `bit`'s quoted spelling comes from ([`PgType::format_type`]).
/// * The value is the literal put through the type's input function *without*
///   the modifier: `'007'` on an `integer` prints `7`, and `'x'` on a `char(4)`
///   prints `'x'::bpchar` — unpadded, because the padding is the modifier's work.
/// * `int4` prints bare unless negative (`'-1'::integer`, so a re-parse sees a
///   constant and not a unary minus); `numeric` prints bare only when
///   non-negative and unambiguously fractional (`1.5`, but `'1000'::numeric`,
///   which would otherwise re-parse as `int4`); `boolean` prints the `true`/
///   `false` keywords; everything else is quoted and labelled.
///
/// * **`DEFAULT NULL`** is not recorded at all for a type that needs no length
///   coercion, and recorded as `NULL::<label>` for one that does — see
///   [`ColumnDefault::Omit`].
///
/// Deliberately left alone, keeping the source text it has always stored:
///
/// * **Non-literals**, which go to [`crate::ruleutils::deparse_stored_expr`] instead
///   — it has the precedence rules an operator expression needs. `DEFAULT (1 +
///   2)` must not be folded to `3`, which is why the split is between *literal*
///   and everything else rather than between constant and non-constant. An
///   explicit cast goes there too: PG keeps the modifier on one
///   (`'1001'::bit(4)`), which is the opposite of the rule above.
///
/// A `bytea` value is baked here in `hex` no matter what the DDL session's
/// `bytea_output` was, and a `timestamptz` in that session's zone — the latter
/// by the DDL path, since a zone-dependent literal never folds to a `Const`
/// here and leaves as `Source`. [`crate::ruleutils::stored_expr`] puts both
/// back into the reader's `bytea_output` and zone on the way out.
pub fn deparse_literal_default(
    expr: &ast::Expr,
    column: &Column,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<ColumnDefault, BindError> {
    // PG's grammar folds a sign into a numeric literal, so `-1` is a constant
    // (see `bind_unary`); `+1` is not, and stays an operator expression.
    let literal = match expr {
        ast::Expr::Value(v) => match v.value {
            ast::Value::Null => return Ok(null_default(column)),
            ast::Value::Placeholder(_) => false,
            _ => true,
        },
        ast::Expr::UnaryOp {
            op: ast::UnaryOperator::Minus,
            expr,
        } => {
            matches!(expr.as_ref(), ast::Expr::Value(v) if matches!(v.value, ast::Value::Number(..)))
        }
        _ => false,
    };
    if !literal {
        return Ok(ColumnDefault::Source);
    }
    let params = param_ctx_none();
    let scope = Scope::empty(catalog, &params);
    let bound = match bind_expr(expr, &scope)? {
        // An untyped constant takes the column's type, as PG's unknown-Const
        // resolution does. A `timestamptz`/`timetz` one needs the session zone
        // to resolve, which the binder deliberately does not hold, so it stays
        // unfolded and falls out below as `Source` — the DDL path renders those
        // two itself.
        Binding::Unknown { lit, span, param } => {
            resolve_unknown_ctx(scope.catalog(), lit, span, param, column.ty)?
        }
        Binding::Typed(e) => e,
    };
    let BoundExpr::Const { value, ty } = bound else {
        // A literal that does not fold to a constant — a `reg*` name, say, whose
        // resolution needs the running catalog.
        return Ok(ColumnDefault::Source);
    };
    Ok(deparse_const(&value, ty).map_or(ColumnDefault::Source, ColumnDefault::Deparsed))
}

/// What to record for a column default, once deparsed.
#[derive(Debug, PartialEq, Eq)]
pub enum ColumnDefault {
    /// PostgreSQL's canonical text for it.
    Deparsed(String),
    /// Keep the statement's own expression text.
    Source,
    /// Record no default at all, leaving `atthasdef` false.
    Omit,
}

/// `DEFAULT NULL`: PostgreSQL skips the `pg_attrdef` entry when the transformed
/// default is a bare null `Const`, and keeps it when a length coercion wrapped
/// one — so `text DEFAULT NULL` reports no default at all while `bit(4) DEFAULT
/// NULL` reports `NULL::"bit"`. A coercion is inserted exactly when the column
/// carries a modifier, which is the test used here; verified against PostgreSQL
/// 18.4 across `text`/`varchar`/`varchar(4)`/`bit(4)`/`bit varying(5)`/`char(4)`/
/// `numeric`/`numeric(5,2)`/`int`/`timestamp(3)`/`name`.
///
/// Testing the modifier rather than the shape of the bound expression matters
/// for one type: `name` truncates through a coercion node here but has no
/// modifier and no length coercion in PG, so it must still be omitted.
fn null_default(column: &Column) -> ColumnDefault {
    if column.typmod < 0 {
        return ColumnDefault::Omit;
    }
    let label = column
        .ty
        .format_type(Some(-1))
        .unwrap_or_else(|| column.ty.name().to_string());
    ColumnDefault::Deparsed(format!("NULL::{label}"))
}

/// Render one constant the way PG's deparse does — see
/// [`deparse_literal_default`] for where each rule comes from. `None` for a NULL
/// value, which no caller produces.
fn deparse_const(value: &Value, ty: PgType) -> Option<String> {
    // `true`/`false` are SQL keywords, not the `t`/`f` of the wire encoding,
    // which would not even re-parse as a boolean default.
    if let Value::Bool(b) = value {
        return Some(if *b { "true" } else { "false" }.to_string());
    }
    let text = value.encode_text_utc()?;
    let bare = match ty {
        PgType::Int4 => !text.starts_with('-'),
        PgType::Numeric => {
            !text.starts_with('-')
                && text.contains('.')
                && text.chars().all(|c| c.is_ascii_digit() || c == '.')
        }
        _ => false,
    };
    if bare {
        return Some(text);
    }
    Some(format!(
        "'{}'::{}",
        text.replace('\'', "''"),
        const_type_label(ty)
    ))
}

/// The `::type` suffix a deparsed constant carries.
///
/// `format_type` declines an array or a user type, which have no modifier to
/// render; the bare type name is the right answer for both. Shared with the DDL
/// path's `session_literal_default`, which resolves the value itself but must
/// label it identically — the two used to disagree, and an array default came
/// out as `::text`.
pub fn const_type_label(ty: PgType) -> String {
    ty.format_type(Some(-1))
        .unwrap_or_else(|| ty.name().to_string())
}
