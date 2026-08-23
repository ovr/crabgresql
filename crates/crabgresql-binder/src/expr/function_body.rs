//! `CREATE FUNCTION ... LANGUAGE SQL`: binding a body against its declared
//! arguments, coercing its result, and inlining the call's arguments into the
//! body at the call site.

use std::sync::Arc;

use crabgresql_parser::ast;
use crabgresql_storage_api::TypeCatalog;
use crabgresql_types::{PgType, Value};

use crate::BindError;

use super::bind::bind_expr;
use super::bound::{BoundAggregate, BoundExpr, BoundWindowSpec, ExprSortKey, WindowKind};
use super::coerce::{coerce_expr, implicit_castable, resolve_unknown_ctx, type_label};
use super::operators::is_text_family;
use super::params::param_ctx_capped;
use super::scope::{Binding, Scope};

/// Bind the body of a `CREATE FUNCTION ... LANGUAGE SQL` to a typed expression,
/// with `$1..$n` seeded to the declared argument types and the result coerced to
/// the declared return type. An argument declared with a name is also reachable
/// under that name (`value`, or `func_name.value`), as in PG; `arg_names` is
/// positionally aligned with `arg_types`. `body_sql` is the normalized `SELECT <expr>` the
/// catalog stores; it must be a single FROM-less, single-column `SELECT` — any
/// other shape (FROM, WHERE, GROUP BY, set-op, multiple columns, …) is rejected,
/// since a scalar function is expanded inline into the caller's expression tree.
///
/// TODO: run a SQL function body as its own query, so the shapes PG accepts
/// (FROM, WHERE, GROUP BY, set-ops, multiple columns) work here too.
///
/// Used both to validate the body at `CREATE FUNCTION` and to produce the
/// expression a call site inlines: the returned tree still carries `Param` leaves
/// for `$n`, which [`inline_params`] replaces with the argument expressions.
pub fn bind_sql_function_body(
    catalog: &Arc<dyn TypeCatalog>,
    func_name: &str,
    arg_types: &[PgType],
    arg_names: &[Option<String>],
    return_type: PgType,
    body_sql: &str,
) -> Result<BoundExpr, BindError> {
    let statements = crabgresql_parser::parse(body_sql).map_err(|e| {
        BindError::feature_not_supported(format!(
            "SQL function body must be a single SELECT statement: {e}"
        ))
    })?;
    let query = match statements.as_slice() {
        [ast::Statement::Query(query)] => query,
        _ => {
            return Err(BindError::feature_not_supported(
                "SQL function body must be a single SELECT statement",
            ));
        }
    };
    let unsupported: Option<&str> = if query.with.is_some() {
        Some("WITH")
    } else if query.order_by.is_some() {
        Some("ORDER BY")
    } else if query.limit_clause.is_some() {
        Some("LIMIT/OFFSET")
    } else if query.fetch.is_some() || !query.locks.is_empty() {
        Some("this clause")
    } else {
        None
    };
    if let Some(clause) = unsupported {
        return Err(BindError::feature_not_supported(format!(
            "{clause} is not supported in a SQL function body yet"
        )));
    }
    let select = match query.body.as_ref() {
        ast::SetExpr::Select(select) => select,
        _ => {
            return Err(BindError::feature_not_supported(
                "only a simple SELECT is supported in a SQL function body",
            ));
        }
    };
    let group_by_empty = matches!(
        &select.group_by,
        ast::GroupByExpr::Expressions(exprs, mods) if exprs.is_empty() && mods.is_empty()
    );
    let unsupported: Option<&str> = if !select.from.is_empty() {
        Some("FROM")
    } else if select.selection.is_some() {
        Some("WHERE")
    } else if !group_by_empty {
        Some("GROUP BY")
    } else if select.having.is_some() {
        Some("HAVING")
    } else if select.distinct.is_some() {
        Some("DISTINCT")
    } else {
        None
    };
    if let Some(clause) = unsupported {
        return Err(BindError::feature_not_supported(format!(
            "{clause} is not supported in a SQL function body yet"
        )));
    }
    let expr = match select.projection.as_slice() {
        [ast::SelectItem::UnnamedExpr(expr)] | [ast::SelectItem::ExprWithAlias { expr, .. }] => {
            expr
        }
        _ => {
            return Err(BindError::feature_not_supported(
                "a SQL function body must return a single column",
            ));
        }
    };

    // Seed `$1..$argcount` to the declared argument types; the capped context
    // rejects any larger `$n` at its reference site, naming the actual `n`.
    let params = param_ctx_capped(arg_types.iter().copied().map(Some).collect());
    let scope = Scope::function_body(catalog, &params, func_name, arg_names);
    let bound = bind_expr(expr, &scope)?;
    let bound = coerce_function_return(bound, return_type, catalog)?;
    if bound.contains_srf() {
        return Err(BindError::feature_not_supported(
            "set-returning functions are not supported in a SQL function body yet",
        ));
    }
    // PG runs a function body as its own query, so `SELECT row_number() OVER ()`
    // is a legal body that returns 1 for every call. This engine *inlines* the
    // body into the caller's expression tree, where the marker would join the
    // caller's window chain and number the caller's rows instead — so the body
    // must be refused. A limitation, not an illegal construct, hence 0A000.
    // TODO: allow a window function in a SQL function body, which needs the body
    // to have a query level of its own instead of being inlined.
    if bound.contains_window() {
        return Err(BindError::feature_not_supported(
            "window functions in a SQL function body are not supported yet",
        ));
    }
    // PG accepts a FROM-less aggregate (e.g. `SELECT sum(1)`) as a function body;
    // there is no way to inline one as a scalar — a limitation, not an illegal
    // construct, so report it as unsupported rather than a grouping error.
    // TODO: allow a FROM-less aggregate in a SQL function body.
    if bound.contains_aggregate() {
        return Err(BindError::feature_not_supported(
            "aggregate functions in a SQL function body are not supported yet",
        ));
    }
    Ok(bound)
}

/// Coerce a SQL function body's result to the declared return type, in PG's
/// assignment context. A bare literal/`NULL` body takes the return type
/// directly; otherwise the same numeric-widening / text-assignment / implicit
/// casts as [`coerce_to_column`] apply, and an incompatible pair is PG's `42P13`
/// "return type mismatch in function declared to return …".
fn coerce_function_return(
    binding: Binding,
    return_type: PgType,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<BoundExpr, BindError> {
    let expr = match binding {
        Binding::Unknown { lit, span, param } => {
            return resolve_unknown_ctx(catalog.as_ref(), lit, span, param, return_type);
        }
        Binding::Typed(e) => e,
    };
    let ty = expr.ty();
    if ty == return_type {
        Ok(expr)
    } else if (ty.is_numeric() && return_type.is_numeric())
        || is_text_family(return_type)
        || implicit_castable(ty, return_type)
        || matches!((ty, return_type), (PgType::TimestampTz, PgType::Timestamp))
    {
        coerce_expr(expr, return_type)
    } else {
        Err(BindError::new(
            "42P13",
            format!(
                "return type mismatch in function declared to return {}",
                type_label(return_type, catalog.as_ref())
            ),
        )
        .with_detail(Some(format!(
            "Actual return type is {}.",
            type_label(ty, catalog.as_ref())
        ))))
    }
}

/// Replace each `$n` ([`BoundExpr::Param`]) in a bound SQL-function body with the
/// call's `n`-th argument expression. Mirrors [`crate::plan::subst_expr`], but
/// substitutes a whole expression (not a constant value), since a call argument
/// is an arbitrary expression over the outer row. A validated scalar body never
/// contains a subquery (the body scope forbids one), so those leaves carry no
/// params to replace and are left untouched.
pub fn inline_params(expr: BoundExpr, args: &[BoundExpr]) -> BoundExpr {
    match expr {
        // A validated body never references a `$n` past the argument list, so the
        // index is always in range; a null const is an inert fallback, not panic.
        BoundExpr::Param { index, ty } => args.get(index).cloned().unwrap_or(BoundExpr::Const {
            value: Value::Null,
            ty,
        }),
        BoundExpr::Const { .. }
        | BoundExpr::ColumnRef { .. }
        | BoundExpr::OuterColumnRef { .. } => expr,
        BoundExpr::Unary { op, expr } => BoundExpr::Unary {
            op,
            expr: Box::new(inline_params(*expr, args)),
        },
        BoundExpr::Collate {
            expr,
            collation,
            explicit,
        } => BoundExpr::Collate {
            expr: Box::new(inline_params(*expr, args)),
            collation,
            explicit,
        },
        BoundExpr::Binary {
            op,
            arg_ty,
            collation,
            left,
            right,
        } => BoundExpr::Binary {
            op,
            arg_ty,
            collation,
            left: Box::new(inline_params(*left, args)),
            right: Box::new(inline_params(*right, args)),
        },
        BoundExpr::Routine {
            oid,
            name,
            arg_types,
            strict,
            args: call_args,
            ret,
        } => BoundExpr::Routine {
            oid,
            name,
            arg_types,
            strict,
            args: call_args
                .into_iter()
                .map(|a| inline_params(a, args))
                .collect(),
            ret,
        },
        BoundExpr::IsNull { expr, negated } => BoundExpr::IsNull {
            expr: Box::new(inline_params(*expr, args)),
            negated,
        },
        BoundExpr::BoolTest {
            expr,
            value,
            negated,
        } => BoundExpr::BoolTest {
            expr: Box::new(inline_params(*expr, args)),
            value,
            negated,
        },
        BoundExpr::Coerce { expr, ty } => BoundExpr::Coerce {
            expr: Box::new(inline_params(*expr, args)),
            ty,
        },
        BoundExpr::Reinterpret {
            expr,
            reported,
            rep,
        } => BoundExpr::Reinterpret {
            expr: Box::new(inline_params(*expr, args)),
            reported,
            rep,
        },
        BoundExpr::FuncCall {
            func,
            ret,
            args: call_args,
        } => BoundExpr::FuncCall {
            func,
            ret,
            args: call_args
                .into_iter()
                .map(|a| inline_params(a, args))
                .collect(),
        },
        BoundExpr::Srf {
            func,
            ret,
            args: call_args,
        } => BoundExpr::Srf {
            func,
            ret,
            args: call_args
                .into_iter()
                .map(|a| inline_params(a, args))
                .collect(),
        },
        BoundExpr::Case { whens, else_, ty } => BoundExpr::Case {
            whens: whens
                .into_iter()
                .map(|(cond, result)| (inline_params(cond, args), inline_params(result, args)))
                .collect(),
            else_: else_.map(|e| Box::new(inline_params(*e, args))),
            ty,
        },
        BoundExpr::Coalesce {
            args: coalesce_args,
            ty,
        } => BoundExpr::Coalesce {
            args: coalesce_args
                .into_iter()
                .map(|a| inline_params(a, args))
                .collect(),
            ty,
        },
        BoundExpr::MinMax {
            kind,
            args: min_max_args,
            ty,
            collation,
        } => BoundExpr::MinMax {
            kind,
            args: min_max_args
                .into_iter()
                .map(|a| inline_params(a, args))
                .collect(),
            ty,
            collation,
        },
        BoundExpr::Aggregate {
            func,
            distinct,
            agg_args,
            order_by,
            input_ty,
            ret,
        } => BoundExpr::Aggregate {
            func,
            distinct,
            agg_args: agg_args
                .into_iter()
                .map(|a| inline_params(a, args))
                .collect(),
            order_by: order_by
                .into_iter()
                .map(|key| ExprSortKey {
                    expr: inline_params(key.expr, args),
                    ..key
                })
                .collect(),
            input_ty,
            ret,
        },
        // A SQL function body cannot contain a window call (there is no query
        // level for one to belong to), so this arm is unreachable in practice —
        // it recurses rather than cloning so that if that ever changes, a `$n`
        // in an argument or an OVER clause is still substituted.
        BoundExpr::WindowFunc { kind, spec, ret } => BoundExpr::WindowFunc {
            kind: match kind {
                WindowKind::Builtin { func, args: call } => WindowKind::Builtin {
                    func,
                    args: call.into_iter().map(|a| inline_params(a, args)).collect(),
                },
                WindowKind::Aggregate(agg) => WindowKind::Aggregate(BoundAggregate {
                    args: agg
                        .args
                        .into_iter()
                        .map(|a| inline_params(a, args))
                        .collect(),
                    ..agg
                }),
            },
            spec: Box::new(BoundWindowSpec {
                partition_by: spec
                    .partition_by
                    .into_iter()
                    .map(|a| inline_params(a, args))
                    .collect(),
                order_by: spec
                    .order_by
                    .into_iter()
                    .map(|key| ExprSortKey {
                        expr: inline_params(key.expr, args),
                        ..key
                    })
                    .collect(),
            }),
            ret,
        },
        BoundExpr::ArrayCtor { elem, ty, elems } => BoundExpr::ArrayCtor {
            elem,
            ty,
            elems: elems.into_iter().map(|a| inline_params(a, args)).collect(),
        },
        BoundExpr::Subscript { base, index, ty } => BoundExpr::Subscript {
            base: Box::new(inline_params(*base, args)),
            index: Box::new(inline_params(*index, args)),
            ty,
        },
        // `x op ANY/ALL(array)` carries no subplan and can appear in a scalar
        // body, so inline params into both the array and the comparison template.
        BoundExpr::QuantifiedArray { array, all, cmp } => BoundExpr::QuantifiedArray {
            array: Box::new(inline_params(*array, args)),
            all,
            cmp: Box::new(inline_params(*cmp, args)),
        },
        // Subqueries cannot appear in a validated scalar body; leave untouched.
        BoundExpr::ScalarSubquery { .. }
        | BoundExpr::ArraySubquery { .. }
        | BoundExpr::Exists { .. }
        | BoundExpr::QuantifiedSubquery { .. } => expr,
    }
}
