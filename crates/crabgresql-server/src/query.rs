//! Simple-query execution: AST → bind → plan → Volcano executor.
//!
//! DQL/DML statements run through the binder/planner pipeline. DDL
//! (CREATE TABLE) and session commands (SET) execute directly here until the
//! catalog and GUC store exist.

use std::sync::Arc;

use crabgresql_binder::{bind_delete, bind_insert, bind_query, bind_update};
use crabgresql_executor::{ExecNode, Execution, OutputColumn, execute};
use crabgresql_parser::ast;
use crabgresql_pg_wire::{TransactionStatus, sqlstate};
use crabgresql_storage_api::{Column, StorageError, TableAm, TableEngine, TableSchema};
use crabgresql_types::PgType;

use crate::catalog::SessionCatalog;
use crate::error::PgError;
use crate::global_catalog::{CatalogNotice, GlobalCatalog, TypeRef};
use crate::session::Session;

/// Severity of a non-error message sent before a command's CommandComplete.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NoticeSeverity {
    /// A true `NOTICE` (e.g. "drop cascades to N other objects").
    Notice,
    /// A `WARNING` (e.g. "there is no transaction in progress").
    Warning,
}

/// A non-error message sent before a command's CommandComplete.
pub struct Notice {
    pub severity: NoticeSeverity,
    pub code: &'static str,
    pub message: String,
    pub detail: Option<String>,
}

impl Notice {
    /// A `WARNING`-severity message (no DETAIL), as used by transaction control.
    fn warning(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: NoticeSeverity::Warning,
            code,
            message: message.into(),
            detail: None,
        }
    }

    /// A `NOTICE`-severity message with an optional DETAIL line.
    #[allow(clippy::self_named_constructors)]
    fn notice(message: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            severity: NoticeSeverity::Notice,
            // psql does not print the SQLSTATE of a NOTICE; PG uses
            // successful_completion (00000) for these.
            code: "00000",
            message: message.into(),
            detail,
        }
    }
}

pub enum QueryResult {
    /// A result set, streamed: the caller pulls tuples from the node and
    /// derives the `SELECT n` tag from the row count.
    Rows {
        columns: Vec<OutputColumn>,
        node: Box<dyn ExecNode>,
    },
    Command {
        tag: String,
        /// Warnings to emit before the CommandComplete, in order.
        notices: Vec<Notice>,
    },
}

impl QueryResult {
    /// A command result with no accompanying warnings.
    fn command(tag: impl Into<String>) -> Self {
        QueryResult::Command {
            tag: tag.into(),
            notices: Vec::new(),
        }
    }
}

pub fn execute_statement(
    engine: &Arc<dyn TableEngine>,
    global_catalog: &Arc<GlobalCatalog>,
    stmt: &ast::Statement,
    session: &mut Session,
) -> Result<QueryResult, PgError> {
    // In an aborted transaction block, PG rejects everything but COMMIT/ROLLBACK
    // until the block ends.
    if session.tx_status == TransactionStatus::Failed
        && !matches!(
            stmt,
            ast::Statement::Commit { .. } | ast::Statement::Rollback { .. }
        )
    {
        return Err(PgError::new(
            sqlstate::IN_FAILED_SQL_TRANSACTION,
            "current transaction is aborted, commands ignored until end of transaction block",
        ));
    }
    // Resolution overlay: the session's temp catalog shadows the shared global
    // engine (PG's `pg_temp`-first search). CREATE routes temp vs global itself,
    // so it keeps the raw engine + session below.
    let catalog: Arc<dyn TableEngine> =
        Arc::new(SessionCatalog::new(session.temp.clone(), engine.clone()));
    let logical = match stmt {
        ast::Statement::Query(query) => bind_query(&catalog, query)?,
        ast::Statement::Insert(insert) => bind_insert(&catalog, insert)?,
        ast::Statement::Update(update) => bind_update(&catalog, update)?,
        ast::Statement::Delete(delete) => bind_delete(&catalog, delete)?,
        ast::Statement::CreateTable(create) => {
            return execute_create_table(engine, create, session);
        }
        ast::Statement::CreateType {
            name,
            representation,
        } => return execute_create_type(global_catalog, name, representation),
        ast::Statement::CreateFunction(create) => {
            return execute_create_function(global_catalog, create);
        }
        ast::Statement::CreateCast {
            source,
            target,
            method,
            ..
        } => return execute_create_cast(global_catalog, source, target, method),
        ast::Statement::Drop {
            object_type: ast::ObjectType::Type,
            names,
            cascade,
            if_exists,
            ..
        } => return execute_drop_type(global_catalog, names, *cascade, *if_exists),
        ast::Statement::DropCast {
            if_exists,
            source,
            target,
            ..
        } => return execute_drop_cast(global_catalog, source, target, *if_exists),
        ast::Statement::Set(set) => return apply_set(set, session),
        ast::Statement::Reset(reset) => return apply_reset(reset, session),
        ast::Statement::StartTransaction {
            modes,
            begin,
            modifier,
            statements,
            exception,
            has_end_keyword,
            ..
        } => {
            return begin_transaction(
                session,
                modes,
                *begin,
                modifier,
                statements,
                exception,
                *has_end_keyword,
            );
        }
        ast::Statement::Commit {
            chain, modifier, ..
        } => return commit_transaction(session, *chain, modifier),
        ast::Statement::Rollback { chain, savepoint } => {
            return rollback_transaction(session, *chain, savepoint);
        }
        ast::Statement::Truncate(truncate) => return execute_truncate(&catalog, truncate),
        other => {
            return Err(PgError::feature_not_supported(format!(
                "statement is not supported yet: {}",
                statement_kind(other)
            )));
        }
    };
    let result = match execute(crabgresql_planner::plan(logical), session.exec_context())? {
        Execution::Rows { columns, node } => QueryResult::Rows { columns, node },
        Execution::Inserted(n) => QueryResult::command(format!("INSERT 0 {n}")),
        Execution::Updated(n) => QueryResult::command(format!("UPDATE {n}")),
        Execution::Deleted(n) => QueryResult::command(format!("DELETE {n}")),
    };
    Ok(result)
}

/// `AND CHAIN` (commit/rollback then immediately open an identical block) is not
/// implemented yet; shared by the BEGIN/COMMIT/ROLLBACK handlers.
fn and_chain_unsupported() -> PgError {
    PgError::feature_not_supported("AND CHAIN is not supported yet")
}

/// `BEGIN` / `START TRANSACTION` (bare forms only). Enters the transaction
/// block; a redundant BEGIN warns but stays in the block. Real data rollback is
/// M2 — this only tracks the control-flow state.
fn begin_transaction(
    session: &mut Session,
    modes: &[ast::TransactionMode],
    begin: bool,
    modifier: &Option<ast::TransactionModifier>,
    statements: &[ast::Statement],
    exception: &Option<Vec<ast::ExceptionWhen>>,
    has_end_keyword: bool,
) -> Result<QueryResult, PgError> {
    // `BEGIN ... END` is a procedural (atomic) block, not transaction control —
    // reject it regardless of the current state.
    if !statements.is_empty() || exception.is_some() || has_end_keyword {
        return Err(PgError::feature_not_supported(
            "BEGIN ... END atomic blocks are not supported yet",
        ));
    }
    // PG completes `BEGIN` and `START TRANSACTION` with distinct tags.
    let tag = if begin { "BEGIN" } else { "START TRANSACTION" };
    // Inside a block PG ignores the command's arguments and only warns, so an
    // unsupported mode must not abort an already-open transaction.
    if session.tx_status == TransactionStatus::InTransaction {
        return Ok(QueryResult::Command {
            tag: tag.into(),
            notices: vec![Notice::warning(
                sqlstate::ACTIVE_SQL_TRANSACTION,
                "there is already a transaction in progress",
            )],
        });
    }
    // Opening a new block: unsupported modes/modifiers are an honest 0A000.
    if !modes.is_empty() {
        return Err(PgError::feature_not_supported(
            "transaction modes (isolation level / read write) are not supported yet",
        ));
    }
    if modifier.is_some() {
        return Err(and_chain_unsupported());
    }
    session.tx_status = TransactionStatus::InTransaction;
    Ok(QueryResult::command(tag))
}

/// `COMMIT` / `END`. Ends the block. A COMMIT of a failed block is reported as
/// ROLLBACK, and a COMMIT with no block open warns — both as in PG.
fn commit_transaction(
    session: &mut Session,
    chain: bool,
    modifier: &Option<ast::TransactionModifier>,
) -> Result<QueryResult, PgError> {
    if chain {
        return Err(and_chain_unsupported());
    }
    if modifier.is_some() {
        return Err(PgError::feature_not_supported(
            "transaction modifiers are not supported yet",
        ));
    }
    let mut notices = Vec::new();
    let tag = match session.tx_status {
        TransactionStatus::Failed => "ROLLBACK",
        TransactionStatus::Idle => {
            notices.push(Notice::warning(
                sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                "there is no transaction in progress",
            ));
            "COMMIT"
        }
        TransactionStatus::InTransaction => "COMMIT",
    };
    session.tx_status = TransactionStatus::Idle;
    Ok(QueryResult::Command {
        tag: tag.into(),
        notices,
    })
}

/// `ROLLBACK`. Ends the block (in any state); with no block open it warns.
/// Note: committed in-memory data is not undone yet — that is M2.
fn rollback_transaction(
    session: &mut Session,
    chain: bool,
    savepoint: &Option<ast::Ident>,
) -> Result<QueryResult, PgError> {
    if chain {
        return Err(and_chain_unsupported());
    }
    if savepoint.is_some() {
        return Err(PgError::feature_not_supported(
            "ROLLBACK TO SAVEPOINT is not supported yet",
        ));
    }
    let mut notices = Vec::new();
    if session.tx_status == TransactionStatus::Idle {
        notices.push(Notice::warning(
            sqlstate::NO_ACTIVE_SQL_TRANSACTION,
            "there is no transaction in progress",
        ));
    }
    session.tx_status = TransactionStatus::Idle;
    Ok(QueryResult::Command {
        tag: "ROLLBACK".into(),
        notices,
    })
}

/// `TRUNCATE [TABLE] name [, ...]` (bare form only). All named tables are
/// resolved before any is emptied, so a missing table fails the whole statement.
fn execute_truncate(
    engine: &Arc<dyn TableEngine>,
    truncate: &ast::Truncate,
) -> Result<QueryResult, PgError> {
    if truncate.cascade.is_some() {
        return Err(PgError::feature_not_supported(
            "TRUNCATE ... CASCADE/RESTRICT is not supported yet",
        ));
    }
    if truncate.identity.is_some() {
        return Err(PgError::feature_not_supported(
            "TRUNCATE ... RESTART/CONTINUE IDENTITY is not supported yet",
        ));
    }
    if truncate.if_exists {
        return Err(PgError::feature_not_supported(
            "TRUNCATE ... IF EXISTS is not supported yet",
        ));
    }
    if truncate.partitions.is_some() {
        return Err(PgError::feature_not_supported(
            "TRUNCATE of a partition list is not supported yet",
        ));
    }
    if truncate.on_cluster.is_some() {
        return Err(PgError::feature_not_supported(
            "TRUNCATE ... ON CLUSTER is not supported yet",
        ));
    }
    let mut tables: Vec<Arc<dyn TableAm>> = Vec::with_capacity(truncate.table_names.len());
    for target in &truncate.table_names {
        if target.only || target.has_asterisk {
            return Err(PgError::feature_not_supported(
                "TRUNCATE ONLY / descendant selection is not supported yet",
            ));
        }
        let name = object_name_to_table_name(&target.name)?;
        tables.push(engine.open_table(&name)?);
    }
    for table in &tables {
        table.truncate();
    }
    Ok(QueryResult::command("TRUNCATE TABLE"))
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
    Ok(QueryResult::command("SET"))
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
    Ok(QueryResult::command("RESET"))
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
    session: &Session,
) -> Result<QueryResult, PgError> {
    let name = object_name_to_table_name(&create.name)?;
    // Clauses we can't honor must be rejected, not silently dropped: CREATE
    // TABLE AS would otherwise create an empty table (the SELECT is discarded),
    // and ON COMMIT DROP/DELETE ROWS needs the M2 transaction engine — accepting
    // it would leave a plain session-lifetime table that diverges from PG.
    if create.query.is_some() {
        return Err(PgError::feature_not_supported(
            "CREATE TABLE ... AS is not supported yet",
        ));
    }
    if create.on_commit.is_some() {
        return Err(PgError::feature_not_supported(
            "CREATE TABLE ... ON COMMIT is not supported yet",
        ));
    }
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
        let ty = map_data_type(&col.data_type)?;
        // Carry a varchar(n)/char(n) length so INSERT/UPDATE can pad/validate.
        let typmod = crabgresql_binder::length_typmod(&col.data_type).unwrap_or(-1);
        columns.push(Column::with_typmod(normalize_ident(&col.name), ty, typmod));
    }
    // TEMP tables go in the session-local catalog, which shadows a same-named
    // permanent table; its separate keyspace means shadowing never raises 42P07.
    let target = if create.temporary {
        &session.temp
    } else {
        engine
    };
    match target.create_table(TableSchema { name, columns }) {
        Ok(_) => {}
        // PG succeeds with a notice; NoticeResponse itself is still todo.
        Err(StorageError::TableAlreadyExists(_)) if create.if_not_exists => {}
        Err(e) => return Err(e.into()),
    }
    Ok(QueryResult::command("CREATE TABLE"))
}

/// Shared with cast/typed-literal binding, so CREATE TABLE and `::` casts agree
/// on the type name mapping.
fn map_data_type(dt: &ast::DataType) -> Result<PgType, PgError> {
    crabgresql_binder::map_data_type(dt).map_err(PgError::from)
}

/// Wrap catalog-produced notices as NOTICE-severity messages for the wire.
fn to_notices(notices: Vec<CatalogNotice>) -> Vec<Notice> {
    notices
        .into_iter()
        .map(|n| Notice::notice(n.message, n.detail))
        .collect()
}

/// A single-part object name (type/function), lowercased. Qualified names are
/// not supported yet.
fn single_ident_name(name: &ast::ObjectName) -> Result<String, PgError> {
    if name.0.len() != 1 {
        return Err(PgError::feature_not_supported(format!(
            "qualified names are not supported yet: {name}"
        )));
    }
    match name.0[0].as_ident() {
        Some(ident) => Ok(normalize_ident(ident)),
        None => Err(PgError::syntax(format!("invalid name: {name}"))),
    }
}

/// The single lowercased name of a bare custom type (e.g. `xfloat8`, `cstring`),
/// or `None` for a built-in `DataType` variant.
fn datatype_simple_name(dt: &ast::DataType) -> Option<String> {
    match dt {
        ast::DataType::Custom(obj, mods) if mods.is_empty() && obj.0.len() == 1 => {
            obj.0[0].as_ident().map(normalize_ident)
        }
        _ => None,
    }
}

/// Resolve a SQL type name to a catalog [`TypeRef`]: the `cstring` pseudo-type, a
/// user-defined type (consulting the catalog), or a built-in type.
fn resolve_type_ref(catalog: &GlobalCatalog, dt: &ast::DataType) -> Result<TypeRef, PgError> {
    if let Some(name) = datatype_simple_name(dt) {
        if name == "cstring" {
            return Ok(TypeRef::Cstring);
        }
        if catalog.is_user_type(&name) {
            return Ok(TypeRef::User(name));
        }
    }
    Ok(TypeRef::Builtin(map_data_type(dt)?))
}

/// Physical width of a built-in type named in a `LIKE` clause, by catalog name.
fn builtin_typlen_by_name(name: &str) -> Option<i32> {
    Some(match name {
        "int8" | "bigint" => 8,
        "int4" | "integer" | "int" => 4,
        "int2" | "smallint" => 2,
        "float8" | "double precision" => 8,
        "float4" | "real" => 4,
        "bool" | "boolean" => 1,
        _ => return None,
    })
}

/// Derive a base type's physical width (`pg_type.typlen`) from its CREATE TYPE
/// options: `INTERNALLENGTH` or `LIKE`. Defaults to variable (-1).
fn typlen_from_options(
    catalog: &GlobalCatalog,
    options: &[ast::UserDefinedTypeSqlDefinitionOption],
) -> i32 {
    use ast::UserDefinedTypeSqlDefinitionOption as Opt;
    let mut typlen = -1;
    for opt in options {
        match opt {
            Opt::InternalLength(ast::UserDefinedTypeInternalLength::Fixed(n)) => typlen = *n as i32,
            Opt::InternalLength(ast::UserDefinedTypeInternalLength::Variable) => typlen = -1,
            Opt::Like(name) => {
                if let Some(n) = name.0.last().and_then(|p| p.as_ident()).map(normalize_ident)
                    && let Some(len) =
                        catalog.user_type_typlen(&n).or_else(|| builtin_typlen_by_name(&n))
                {
                    typlen = len;
                }
            }
            _ => {}
        }
    }
    typlen
}

/// `CREATE TYPE name;` (shell) or `CREATE TYPE name (INPUT=..., OUTPUT=..., ...)`.
fn execute_create_type(
    catalog: &GlobalCatalog,
    name: &ast::ObjectName,
    representation: &Option<ast::UserDefinedTypeRepresentation>,
) -> Result<QueryResult, PgError> {
    let tname = single_ident_name(name)?;
    let notices = match representation {
        None => catalog.create_shell_type(&tname)?,
        Some(ast::UserDefinedTypeRepresentation::SqlDefinition { options }) => {
            let typlen = typlen_from_options(catalog, options);
            catalog.define_type(&tname, typlen)?
        }
        Some(_) => {
            return Err(PgError::feature_not_supported(
                "CREATE TYPE AS (composite / enum / range) is not supported yet",
            ));
        }
    };
    Ok(QueryResult::Command {
        tag: "CREATE TYPE".into(),
        notices: to_notices(notices),
    })
}

/// The `AS '<builtin>'` internal function name of a `LANGUAGE internal` function.
fn function_internal_name(create: &ast::CreateFunction) -> Result<String, PgError> {
    match &create.function_body {
        Some(ast::CreateFunctionBody::AsBeforeOptions { body, .. })
        | Some(ast::CreateFunctionBody::AsAfterOptions(body)) => string_literal(body),
        _ => Err(PgError::feature_not_supported(
            "CREATE FUNCTION LANGUAGE internal requires AS '<builtin>'",
        )),
    }
}

/// Extract a single-quoted (or dollar-quoted) string literal expression.
fn string_literal(expr: &ast::Expr) -> Result<String, PgError> {
    match expr {
        ast::Expr::Value(v) => match &v.value {
            ast::Value::SingleQuotedString(s) => Ok(s.clone()),
            ast::Value::DollarQuotedString(d) => Ok(d.value.clone()),
            other => Err(PgError::syntax(format!(
                "expected a string literal, found: {other}"
            ))),
        },
        other => Err(PgError::syntax(format!(
            "expected a string literal, found: {other}"
        ))),
    }
}

/// `CREATE FUNCTION ... LANGUAGE internal AS '<builtin>'`.
fn execute_create_function(
    catalog: &GlobalCatalog,
    create: &ast::CreateFunction,
) -> Result<QueryResult, PgError> {
    let lang = create.language.as_ref().map(|i| i.value.to_ascii_lowercase());
    if lang.as_deref() != Some("internal") {
        return Err(PgError::feature_not_supported(
            "CREATE FUNCTION is only supported for LANGUAGE internal",
        ));
    }
    let internal_name = function_internal_name(create)?;
    let name = single_ident_name(&create.name)?;
    let ret = match &create.return_type {
        Some(ast::FunctionReturnType::DataType(dt)) => resolve_type_ref(catalog, dt)?,
        Some(ast::FunctionReturnType::SetOf(_)) => {
            return Err(PgError::feature_not_supported(
                "SETOF return types are not supported yet",
            ));
        }
        None => {
            return Err(PgError::feature_not_supported(
                "CREATE FUNCTION without RETURNS is not supported yet",
            ));
        }
    };
    let mut args = Vec::new();
    for arg in create.args.iter().flatten() {
        args.push(resolve_type_ref(catalog, &arg.data_type)?);
    }
    let notices = catalog.create_function(&name, args, ret, &internal_name)?;
    Ok(QueryResult::Command {
        tag: "CREATE FUNCTION".into(),
        notices: to_notices(notices),
    })
}

/// `CREATE CAST (source AS target) ...`.
fn execute_create_cast(
    catalog: &GlobalCatalog,
    source: &ast::DataType,
    target: &ast::DataType,
    method: &ast::CastMethod,
) -> Result<QueryResult, PgError> {
    let source_ref = resolve_type_ref(catalog, source)?;
    let target_ref = resolve_type_ref(catalog, target)?;
    let without_function = matches!(method, ast::CastMethod::WithoutFunction);
    let notices = catalog.create_cast(source_ref, target_ref, without_function)?;
    Ok(QueryResult::Command {
        tag: "CREATE CAST".into(),
        notices: to_notices(notices),
    })
}

/// `DROP TYPE name [, ...] [CASCADE|RESTRICT]`.
fn execute_drop_type(
    catalog: &GlobalCatalog,
    names: &[ast::ObjectName],
    cascade: bool,
    if_exists: bool,
) -> Result<QueryResult, PgError> {
    let mut notices = Vec::new();
    for name in names {
        let tname = single_ident_name(name)?;
        notices.extend(to_notices(catalog.drop_type(&tname, cascade, if_exists)?));
    }
    Ok(QueryResult::Command {
        tag: "DROP TYPE".into(),
        notices,
    })
}

/// `DROP CAST (source AS target) [CASCADE|RESTRICT]`.
fn execute_drop_cast(
    catalog: &GlobalCatalog,
    source: &ast::DataType,
    target: &ast::DataType,
    if_exists: bool,
) -> Result<QueryResult, PgError> {
    let source_ref = resolve_type_ref(catalog, source)?;
    let target_ref = resolve_type_ref(catalog, target)?;
    let notices = catalog.drop_cast(source_ref, target_ref, if_exists)?;
    Ok(QueryResult::Command {
        tag: "DROP CAST".into(),
        notices: to_notices(notices),
    })
}
