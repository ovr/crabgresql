//! Fixtures and helpers shared by every binder-plan test module.

pub(super) use std::sync::Arc;

pub(super) use crabgresql_parser::ast;
pub(super) use crabgresql_pg_wire::sqlstate;
pub(super) use crabgresql_storage_api::{Column, TableAm, TableEngine, TableSchema, TypeCatalog};
pub(super) use crabgresql_types::collation::DEFAULT_COLLATION_OID;
pub(super) use crabgresql_types::{FmtCtx, PgType, Value};

pub(super) use crate::expr::{
    BinOp, BoundExpr, ParamCtx, apply_column_typmod, param_ctx_extended, param_types,
};
pub(super) use crate::plan::*;
pub(super) use crate::{BindError, OutputColumn};

pub(super) fn engine_with_table() -> Arc<dyn TableEngine> {
    let engine = crabgresql_pg_engine::ephemeral_engine();
    if let Err(error) = engine.create_table(TableSchema::new(
        "t",
        vec![
            Column::new("id", PgType::Int4),
            Column::new("big", PgType::Int8),
            Column::new("name", PgType::Text),
            Column::new("flag", PgType::Bool),
        ],
    )) {
        panic!("failed to create binder test table: {error}");
    }
    engine
}

pub(super) fn bind_one(sql: &str) -> Result<LogicalPlan, BindError> {
    let engine = engine_with_table();
    let catalog: Arc<dyn TypeCatalog> = Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
    let stmts = match crabgresql_parser::parse(sql) {
        Ok(stmts) => stmts,
        Err(error) => panic!("invalid SQL test fixture `{sql}`: {error}"),
    };
    match &stmts[0] {
        ast::Statement::Query(q) => bind_query(&engine, &catalog, q),
        ast::Statement::Insert(i) => bind_insert(&engine, &catalog, i),
        ast::Statement::Update(u) => bind_update(&engine, &catalog, u),
        ast::Statement::Delete(d) => bind_delete(&engine, &catalog, d),
        other => panic!("unexpected statement: {other}"),
    }
}

pub(super) fn bound(sql: &str) -> LogicalPlan {
    match bind_one(sql) {
        Ok(plan) => plan,
        Err(error) => panic!("failed to bind SQL test fixture `{sql}`: {error}"),
    }
}

pub(super) fn bind_err(sql: &str) -> BindError {
    match bind_one(sql) {
        Err(e) => e,
        Ok(_) => panic!("expected bind error for: {sql}"),
    }
}

/// The pieces of a bound `Aggregate` plan.
pub(super) fn agg_of(
    sql: &str,
) -> (
    Vec<BoundExpr>,
    Vec<crate::BoundAggregate>,
    Vec<BoundExpr>,
    Option<BoundExpr>,
) {
    match bound(sql) {
        LogicalPlan::Aggregate(AggregatePlan {
            group_exprs,
            aggregates,
            projections,
            having,
            ..
        }) => (group_exprs, aggregates, projections, having),
        other => panic!(
            "expected Aggregate for `{sql}`, got another plan variant: {}",
            plan_name(&other)
        ),
    }
}

/// Every [`LogicalPlan`] variant, as `method => Variant -> Payload`.
///
/// A test that has just bound a statement knows which variant it built, so the
/// only thing a mismatch can mean is that the expectation is wrong. The
/// generated extractors say that outright: they consume the plan, hand back the
/// payload, and panic naming the variant that actually turned up — which the
/// `let LogicalPlan::Query(QueryPlan { .. }) = … else { panic!("expected
/// Query") }` blocks they replace could not do.
macro_rules! plan_variants {
    ($($method:ident => $variant:ident -> $payload:ident),+ $(,)?) => {
        /// The variant of `plan`, for assertions and panic messages.
        pub(super) fn plan_name(plan: &LogicalPlan) -> &'static str {
            match plan {
                $(LogicalPlan::$variant(_) => stringify!($variant),)+
            }
        }

        /// Consuming, panicking extractors: one per [`LogicalPlan`] variant.
        ///
        /// The set is complete on purpose — a test for a variant nothing
        /// covers yet should find its extractor already here — so some of
        /// them have no caller at any given time.
        #[allow(dead_code)]
        pub(super) trait PlanExt {
            $(
                #[doc = concat!(
                    "The [`", stringify!($payload), "`] of a [`LogicalPlan::",
                    stringify!($variant), "`]. Panics on any other variant.",
                )]
                fn $method(self) -> $payload;
            )+
        }

        impl PlanExt for LogicalPlan {
            $(
                fn $method(self) -> $payload {
                    match self {
                        LogicalPlan::$variant(plan) => plan,
                        other => panic!(
                            "expected a {} plan, got {}",
                            stringify!($variant),
                            plan_name(&other),
                        ),
                    }
                }
            )+
        }
    };
}

plan_variants! {
    expect_values => Values -> ValuesPlan,
    expect_query => Query -> QueryPlan,
    expect_append => Append -> AppendPlan,
    expect_set_op => SetOp -> SetOpPlan,
    expect_subquery => Subquery -> SubqueryPlan,
    expect_table_function => TableFunction -> TableFunctionPlan,
    expect_join => Join -> JoinPlan,
    expect_aggregate => Aggregate -> AggregatePlan,
    expect_window => Window -> WindowPlan,
    expect_limit => Limit -> LimitPlan,
    expect_insert => Insert -> InsertPlan,
    expect_update => Update -> UpdatePlan,
    expect_delete => Delete -> DeletePlan,
}

/// The extractors are the only report a test gets when a plan is not the shape
/// it was written for, so the message has to name both sides of the mismatch.
#[test]
#[should_panic(expected = "expected a Query plan, got Values")]
fn a_mismatched_extractor_names_the_variant_it_got() {
    bound("SELECT 1").expect_query();
}

/// The first projected expression of a bound FROM-less `SELECT`.
pub(super) fn one_projection(sql: &str) -> BoundExpr {
    let ValuesPlan { mut rows, .. } = bound(sql).expect_values();
    rows.remove(0).remove(0)
}
