//! Simple-query execution: AST → bind → plan → Volcano executor.
//!
//! DQL/DML statements run through the binder/planner pipeline. DDL
//! (CREATE TABLE) and session commands (SET) execute directly here until the
//! catalog and GUC store exist.

use std::sync::Arc;

use crabgresql_binder::{bind_delete, bind_insert, bind_query, bind_update};
use crabgresql_executor::{ExecNode, Execution, OutputColumn, execute};
use crabgresql_parser::ast;
use crabgresql_storage_api::{Column, StorageError, TableEngine, TableSchema};
use crabgresql_types::PgType;

use crate::error::PgError;

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
) -> Result<QueryResult, PgError> {
    let logical = match stmt {
        ast::Statement::Query(query) => bind_query(engine, query)?,
        ast::Statement::Insert(insert) => bind_insert(engine, insert)?,
        ast::Statement::Update(update) => bind_update(engine, update)?,
        ast::Statement::Delete(delete) => bind_delete(engine, delete)?,
        ast::Statement::CreateTable(create) => return execute_create_table(engine, create),
        // Accepted and ignored for driver compatibility (no GUC store yet).
        ast::Statement::Set(_) => return Ok(QueryResult::Command { tag: "SET".into() }),
        other => {
            return Err(PgError::feature_not_supported(format!(
                "statement is not supported yet: {}",
                statement_kind(other)
            )));
        }
    };
    let result = match execute(crabgresql_planner::plan(logical))? {
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

fn map_data_type(dt: &ast::DataType) -> Result<PgType, PgError> {
    use ast::DataType;
    Ok(match dt {
        DataType::Bool | DataType::Boolean => PgType::Bool,
        DataType::Int(_) | DataType::Integer(_) | DataType::Int4(_) => PgType::Int4,
        DataType::BigInt(_) | DataType::Int8(_) => PgType::Int8,
        DataType::Text | DataType::Varchar(None) | DataType::CharacterVarying(None) => PgType::Text,
        // Accepting varchar(n) without enforcing the length (22001) would
        // silently store over-long values; reject until typmod exists.
        DataType::Varchar(Some(_)) | DataType::CharacterVarying(Some(_)) => {
            return Err(PgError::feature_not_supported(
                "varchar length limits are not supported yet",
            ));
        }
        other => {
            return Err(PgError::feature_not_supported(format!(
                "type \"{other}\" is not supported yet"
            )));
        }
    })
}
