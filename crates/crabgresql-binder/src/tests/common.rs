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

pub(super) fn plan_name(p: &LogicalPlan) -> &'static str {
    match p {
        LogicalPlan::Values(ValuesPlan { .. }) => "Values",
        LogicalPlan::Query(QueryPlan { .. }) => "Query",
        LogicalPlan::Append(AppendPlan { .. }) => "Append",
        LogicalPlan::SetOp(SetOpPlan { .. }) => "SetOp",
        LogicalPlan::Subquery(SubqueryPlan { .. }) => "Subquery",
        LogicalPlan::TableFunction(TableFunctionPlan { .. }) => "TableFunction",
        LogicalPlan::Join(JoinPlan { .. }) => "Join",
        LogicalPlan::Aggregate(AggregatePlan { .. }) => "Aggregate",
        LogicalPlan::Window(WindowPlan { .. }) => "Window",
        LogicalPlan::Limit(LimitPlan { .. }) => "Limit",
        LogicalPlan::Insert(InsertPlan { .. }) => "Insert",
        LogicalPlan::Update(UpdatePlan { .. }) => "Update",
        LogicalPlan::Delete(DeletePlan { .. }) => "Delete",
    }
}

/// The first projected expression of a bound FROM-less `SELECT`.
pub(super) fn one_projection(sql: &str) -> BoundExpr {
    let LogicalPlan::Values(ValuesPlan { mut rows, .. }) = bound(sql) else {
        panic!("expected Values");
    };
    rows.remove(0).remove(0)
}
