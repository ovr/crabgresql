//! Simple-query execution: AST → bind → plan → Volcano executor.
//!
//! DQL/DML statements run through the binder/planner pipeline. DDL
//! (CREATE TABLE) and session commands (SET) execute directly here until the
//! catalog and GUC store exist.

use std::sync::Arc;

use crabgresql_binder::{bind_delete, bind_insert, bind_query, bind_update};
use crabgresql_executor::{ExecNode, Execution, OutputColumn, execute};
use crabgresql_parser::ast;
use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::{Column, StorageError, TableEngine, TableSchema};
use crabgresql_types::PgType;

use crate::error::PgError;
use crate::session::Session;

pub enum QueryResult {
    /// A result set, streamed: the caller pulls tuples from the node and
    /// derives the `SELECT n` tag from the row count.
    Rows {
        columns: Vec<OutputColumn>,
        node: Box<dyn ExecNode>,
    },
    Command {
        tag: String,
    },
}

pub fn execute_statement(
    engine: &Arc<dyn TableEngine>,
    stmt: &ast::Statement,
    session: &mut Session,
) -> Result<QueryResult, PgError> {
    let logical = match stmt {
        ast::Statement::Query(query) => bind_query(engine, query)?,
        ast::Statement::Insert(insert) => bind_insert(engine, insert)?,
        ast::Statement::Update(update) => bind_update(engine, update)?,
        ast::Statement::Delete(delete) => bind_delete(engine, delete)?,
        ast::Statement::CreateTable(create) => return execute_create_table(engine, create),
        ast::Statement::Set(set) => return apply_set(set, session),
        ast::Statement::Reset(reset) => return apply_reset(reset, session),
        other => {
            return Err(PgError::feature_not_supported(format!(
                "statement is not supported yet: {}",
                statement_kind(other)
            )));
        }
    };
    let result = match execute(crabgresql_planner::plan(logical), session.exec_context())? {
        Execution::Rows { columns, node } => QueryResult::Rows { columns, node },
        Execution::Inserted(n) => QueryResult::Command {
            tag: format!("INSERT 0 {n}"),
        },
        Execution::Updated(n) => QueryResult::Command {
            tag: format!("UPDATE {n}"),
        },
        Execution::Deleted(n) => QueryResult::Command {
            tag: format!("DELETE {n}"),
        },
    };
    Ok(result)
}

/// `SET`: only `extra_float_digits` is honored; other GUCs are accepted and
/// ignored (driver compatibility), as before.
fn apply_set(set: &ast::Set, session: &mut Session) -> Result<QueryResult, PgError> {
    if let ast::Set::SingleAssignment {
        variable, values, ..
    } = set
        && single_ident_lower(variable).as_deref() == Some("extra_float_digits")
    {
        let v = set_value_to_i32(values)?;
        if !(-15..=3).contains(&v) {
            return Err(PgError::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                format!(
                    "{v} is outside the valid range for parameter \"extra_float_digits\" (-15 .. 3)"
                ),
            ));
        }
        session.extra_float_digits = v;
    }
    Ok(QueryResult::Command { tag: "SET".into() })
}

/// `RESET extra_float_digits` / `RESET ALL` restore the default (1).
fn apply_reset(reset: &ast::ResetStatement, session: &mut Session) -> Result<QueryResult, PgError> {
    let reset_efd = match &reset.reset {
        ast::Reset::ALL => true,
        ast::Reset::ConfigurationParameter(name) => {
            single_ident_lower(name).as_deref() == Some("extra_float_digits")
        }
    };
    if reset_efd {
        session.extra_float_digits = 1;
    }
    Ok(QueryResult::Command {
        tag: "RESET".into(),
    })
}

/// A single-part object name, lowercased (GUC names are case-insensitive).
fn single_ident_lower(name: &ast::ObjectName) -> Option<String> {
    if name.0.len() != 1 {
        return None;
    }
    name.0[0].as_ident().map(normalize_ident)
}

fn set_value_to_i32(exprs: &[ast::Expr]) -> Result<i32, PgError> {
    let [expr] = exprs else {
        return Err(PgError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "parameter \"extra_float_digits\" requires an integer value",
        ));
    };
    parse_i32_expr(expr).ok_or_else(|| {
        PgError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "parameter \"extra_float_digits\" requires an integer value",
        )
    })
}

fn parse_i32_expr(expr: &ast::Expr) -> Option<i32> {
    match expr {
        ast::Expr::Value(v) => match &v.value {
            ast::Value::Number(n, _) => n.parse().ok(),
            ast::Value::SingleQuotedString(s) => s.trim().parse().ok(),
            _ => None,
        },
        ast::Expr::UnaryOp {
            op: ast::UnaryOperator::Minus,
            expr,
        } => parse_i32_expr(expr).map(|v| -v),
        ast::Expr::UnaryOp {
            op: ast::UnaryOperator::Plus,
            expr,
        } => parse_i32_expr(expr),
        _ => None,
    }
}

fn statement_kind(stmt: &ast::Statement) -> String {
    // First word of the SQL rendering, e.g. "DROP" or "TRUNCATE".
    stmt.to_string()
        .split_whitespace()
        .next()
        .unwrap_or("?")
        .to_string()
}

/// Unquoted identifiers fold to lowercase, as in PG.
fn normalize_ident(ident: &ast::Ident) -> String {
    match ident.quote_style {
        Some(_) => ident.value.clone(),
        None => ident.value.to_lowercase(),
    }
}

fn object_name_to_table_name(name: &ast::ObjectName) -> Result<String, PgError> {
    if name.0.len() != 1 {
        return Err(PgError::feature_not_supported(format!(
            "qualified relation names are not supported yet: {name}"
        )));
    }
    match name.0[0].as_ident() {
        Some(ident) => Ok(normalize_ident(ident)),
        None => Err(PgError::syntax(format!("invalid relation name: {name}"))),
    }
}

fn execute_create_table(
    engine: &Arc<dyn TableEngine>,
    create: &ast::CreateTable,
) -> Result<QueryResult, PgError> {
    let name = object_name_to_table_name(&create.name)?;
    if let Some(constraint) = create.constraints.first() {
        return Err(PgError::feature_not_supported(format!(
            "table constraints are not supported yet: {constraint}"
        )));
    }
    let mut columns = Vec::new();
    for col in &create.columns {
        // Constraints we can't enforce must not be accepted: a silently
        // dropped NOT NULL / PRIMARY KEY would let invalid data in.
        if let Some(option) = col.options.first() {
            return Err(PgError::feature_not_supported(format!(
                "column constraints are not supported yet: {}",
                option.option
            )));
        }
        columns.push(Column {
            name: normalize_ident(&col.name),
            ty: map_data_type(&col.data_type)?,
        });
    }
    match engine.create_table(TableSchema { name, columns }) {
        Ok(_) => {}
        // PG succeeds with a notice; NoticeResponse itself is still todo.
        Err(StorageError::TableAlreadyExists(_)) if create.if_not_exists => {}
        Err(e) => return Err(e.into()),
    }
    Ok(QueryResult::Command {
        tag: "CREATE TABLE".into(),
    })
}

/// Shared with cast/typed-literal binding, so CREATE TABLE and `::` casts agree
/// on the type name mapping.
fn map_data_type(dt: &ast::DataType) -> Result<PgType, PgError> {
    crabgresql_binder::map_data_type(dt).map_err(PgError::from)
}
