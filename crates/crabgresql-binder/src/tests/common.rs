//! Fixtures and helpers shared by every binder-plan test module.
//!
//! Nothing here panics: a fixture that will not parse, a statement the test
//! harness does not bind, a plan that came back the wrong shape — each is an
//! `anyhow::Error` the test propagates with `?`, so the failure arrives as one
//! chain ("binding `SELECT 1`: expected a Query plan, got Values") instead of
//! a bare panic message.

pub(super) use std::sync::Arc;

pub(super) use anyhow::{Context, anyhow, bail};
pub(super) use crabgresql_parser::ast;
pub(super) use crabgresql_pg_wire::sqlstate;
pub(super) use crabgresql_storage_api::{Column, TableAm, TableEngine, TableSchema, TypeCatalog};
pub(super) use crabgresql_types::collation::DEFAULT_COLLATION_OID;
pub(super) use crabgresql_types::{FmtCtx, PgType, Value};

pub(super) use crate::expr::{
    BinOp, BoundExpr, ParamCtx, apply_column_typmod, param_ctx_extended, param_types,
};
pub(super) use crate::logical_plan::*;
pub(super) use crate::plan::*;
pub(super) use crate::{BindError, OutputColumn};

pub(super) fn engine_with_table() -> anyhow::Result<Arc<dyn TableEngine>> {
    let engine = crabgresql_pg_engine::ephemeral_engine();
    engine
        .create_table(TableSchema::new(
            "t",
            vec![
                Column::new("id", PgType::Int4),
                Column::new("big", PgType::Int8),
                Column::new("name", PgType::Text),
                Column::new("flag", PgType::Bool),
            ],
        ))
        .context("creating the binder test table")?;
    Ok(engine)
}

/// Bind `sql` against the four-column test table.
///
/// The binder's own [`BindError`] is folded into the chain rather than wrapped,
/// so [`bind_err`] can still recover it by downcast.
pub(super) fn bound(sql: &str) -> anyhow::Result<LogicalPlan> {
    let engine = engine_with_table()?;
    let catalog: Arc<dyn TypeCatalog> = Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
    let stmts = crabgresql_parser::parse(sql)
        .map_err(|error| anyhow!("invalid SQL test fixture `{sql}`: {error}"))?;
    let plan = match &stmts[0] {
        ast::Statement::Query(q) => bind_query(&engine, &catalog, q),
        ast::Statement::Insert(i) => bind_insert(&engine, &catalog, i),
        ast::Statement::Update(u) => bind_update(&engine, &catalog, u),
        ast::Statement::Delete(d) => bind_delete(&engine, &catalog, d),
        other => bail!("unexpected statement in test fixture: {other}"),
    };
    plan.map_err(anyhow::Error::new)
        .with_context(|| format!("binding `{sql}`"))
}

/// The [`BindError`] `sql` is expected to raise.
pub(super) fn bind_err(sql: &str) -> anyhow::Result<BindError> {
    match bound(sql) {
        Ok(_) => bail!("expected a bind error for `{sql}`"),
        Err(error) => error
            .downcast::<BindError>()
            .with_context(|| format!("`{sql}` failed before the binder ran")),
    }
}

/// The pieces of a bound `Aggregate` plan.
pub(super) fn agg_of(
    sql: &str,
) -> anyhow::Result<(
    Vec<BoundExpr>,
    Vec<crate::BoundAggregate>,
    Vec<BoundExpr>,
    Option<BoundExpr>,
)> {
    let AggregatePlan {
        group_exprs,
        aggregates,
        projections,
        having,
        ..
    } = bound_aggregate(sql)?;
    Ok((group_exprs, aggregates, projections, having))
}

/// Every [`LogicalPlan`] variant, as `Variant -> Payload { conversion, binder }`.
///
/// A test that has just bound a statement knows which variant it built, so the
/// only thing a mismatch can mean is that the expectation is wrong. Both
/// generated forms report that as an error naming the variant that actually
/// turned up — which the `let LogicalPlan::Query(QueryPlan { .. }) = … else {
/// panic!("expected Query") }` blocks they replace could not do.
///
/// The conversion takes a plan already in hand (a window chain's source, a
/// subquery's body); the binder is the whole trip from SQL, which is what a
/// test opens with.
macro_rules! plan_variants {
    ($($variant:ident -> $payload:ident { $into:ident, $bind:ident })+) => {
        /// The variant of `plan`, for assertions and error messages.
        pub(super) fn plan_name(plan: &LogicalPlan) -> &'static str {
            match plan {
                $(LogicalPlan::$variant(_) => stringify!($variant),)+
            }
        }

        /// Consuming conversions: one per [`LogicalPlan`] variant.
        ///
        /// The set is complete on purpose — a test for a variant nothing
        /// covers yet should find its conversion already here — so some of
        /// them have no caller at any given time.
        #[allow(dead_code)]
        pub(super) trait PlanExt {
            $(
                #[doc = concat!(
                    "The [`", stringify!($payload), "`] of a [`LogicalPlan::",
                    stringify!($variant), "`], or an error naming the variant it is.",
                )]
                fn $into(self) -> anyhow::Result<$payload>;
            )+
        }

        impl PlanExt for LogicalPlan {
            $(
                fn $into(self) -> anyhow::Result<$payload> {
                    match self {
                        LogicalPlan::$variant(plan) => Ok(plan),
                        other => Err(anyhow!(
                            "expected a {} plan, got {}",
                            stringify!($variant),
                            plan_name(&other),
                        )),
                    }
                }
            )+
        }

        $(
            #[doc = concat!(
                "Bind `sql` to the [`", stringify!($payload), "`] it is expected ",
                "to produce.",
            )]
            #[allow(dead_code)]
            pub(super) fn $bind(sql: &str) -> anyhow::Result<$payload> {
                bound(sql)?
                    .$into()
                    .with_context(|| format!("binding `{sql}`"))
            }
        )+
    };
}

plan_variants! {
    Values -> ValuesPlan { into_values, bound_values }
    Query -> QueryPlan { into_query, bound_query }
    Append -> AppendPlan { into_append, bound_append }
    SetOp -> SetOpPlan { into_set_op, bound_set_op }
    Subquery -> SubqueryPlan { into_subquery, bound_subquery }
    TableFunction -> TableFunctionPlan { into_table_function, bound_table_function }
    Join -> JoinPlan { into_join, bound_join }
    Aggregate -> AggregatePlan { into_aggregate, bound_aggregate }
    Window -> WindowPlan { into_window, bound_window }
    Limit -> LimitPlan { into_limit, bound_limit }
    Insert -> InsertPlan { into_insert, bound_insert }
    Update -> UpdatePlan { into_update, bound_update }
    Delete -> DeletePlan { into_delete, bound_delete }
}

/// The conversions are the only report a test gets when a plan is not the shape
/// it was written for, so the chain has to name both sides of the mismatch.
#[test]
fn a_mismatched_conversion_names_the_variant_it_got() -> anyhow::Result<()> {
    // A FROM-less SELECT binds to Values, never to Query.
    let Err(error) = bound_query("SELECT 1") else {
        bail!("`SELECT 1` must not bind to a Query plan");
    };
    assert_eq!(
        format!("{error:#}"),
        "binding `SELECT 1`: expected a Query plan, got Values"
    );
    Ok(())
}

/// The first projected expression of a bound FROM-less `SELECT`.
pub(super) fn one_projection(sql: &str) -> anyhow::Result<BoundExpr> {
    let ValuesPlan { mut rows, .. } = bound_values(sql)?;
    Ok(rows.remove(0).remove(0))
}
