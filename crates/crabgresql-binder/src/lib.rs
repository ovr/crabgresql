//! Binder: semantic analysis turning the sqlparser AST into a typed logical
//! plan — name resolution against table schemas, operator type inference with
//! PG semantics (int4/int8 promotion, untyped-literal coercion), and honest
//! rejection (`0A000`) of everything parsed but not yet executable.

mod collation;
mod expr;
mod functions;
mod plan;

pub use collation::{
    Derived, Strength, check_explicit_conflict, collation_name, column_collation, expr_collation,
    output_collation, resolve_collation,
};
pub use expr::{
    BinOp, Binding, BoundAggregate, BoundExpr, ParamCtx, ParamState, Scope, Subplan, UnaryOp,
    bind_column_default, bind_expr, bind_scalar, bind_sql_function_body, checked_length_typmod,
    coerce_to_column, inline_params, length_typmod, map_data_type, param_ctx_extended,
    param_ctx_none, param_types, require_all_resolved,
};
pub use functions::{AggFn, GeoFn, JsonPathFn, ScalarFn, TableFn, lookup_table_fn};
pub use plan::{
    AggInput, CopyFormat, CopyFromPlan, DistinctKey, InsertSource, JoinExpr, JoinInput, JoinKind,
    LogicalPlan, Returning, SortKey,
    bind_copy_from,
    bind_delete,
    bind_delete_with_params, bind_insert, bind_insert_with_params, bind_query,
    bind_query_with_params, bind_update, bind_update_with_params, output_columns_of,
    plan_has_outer_refs, substitute_outer, substitute_params,
};

use crabgresql_parser::Span;
use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::StorageError;
use crabgresql_types::PgType;

/// One column of a plan's result set, as needed for `RowDescription`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputColumn {
    pub name: String,
    pub ty: PgType,
    /// The column's collation when it differs from the type default — carried
    /// so a rowset's collation survives into an enclosing query's scope, the
    /// way PostgreSQL tracks `varcollid` on every column of every rowset.
    /// `None` means the type default (and is always `None` for a
    /// non-collatable type).
    pub collation: Option<u32>,
    /// The derivation strength behind `collation` — `Strength::None` when
    /// `collation` is `None` for want of any collatable input, and otherwise
    /// `Strength::Implicit`/`Strength::Explicit`. Carried alongside
    /// `collation` because a rowset boundary (a `UNION` arm, in particular)
    /// needs to tell an explicit `COLLATE` conflict from an implicit one, and
    /// `collation` alone can't: an implicit collation can equal the type
    /// default and still round-trip as `None`.
    pub strength: Strength,
}

impl OutputColumn {
    /// A column on its type's default collation.
    pub fn new(name: impl Into<String>, ty: PgType) -> Self {
        OutputColumn {
            name: name.into(),
            ty,
            collation: None,
            strength: Strength::None,
        }
    }
}

/// A bind-time error, reported to the client as `ErrorResponse`.
#[derive(Debug)]
pub struct BindError {
    /// 5-character SQLSTATE code.
    pub code: &'static str,
    pub message: String,
    /// Optional DETAIL line (e.g. numeric field overflow explains the p/s).
    pub detail: Option<String>,
    /// Optional HINT line (e.g. "You might need to add explicit type casts.").
    pub hint: Option<String>,
    /// 1-based (line, column) of the offending token, when PG reports a
    /// cursor position (`LINE n: ... ^`). Only set for literal input-function
    /// failures and ambiguous operators, mirroring PG.
    pub location: Option<(u64, u64)>,
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BindError {}

impl BindError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
            hint: None,
            location: None,
        }
    }

    /// Attach a DETAIL line.
    pub fn with_detail(mut self, detail: Option<String>) -> Self {
        self.detail = detail;
        self
    }

    /// Attach a HINT line.
    pub fn with_hint(mut self, hint: Option<String>) -> Self {
        self.hint = hint;
        self
    }

    /// Attach the cursor position from a token span (ignored if the span is
    /// empty, i.e. line 0).
    pub fn at(mut self, span: Span) -> Self {
        if span.start.line != 0 {
            self.location = Some((span.start.line, span.start.column));
        }
        self
    }

    pub fn feature_not_supported(message: impl Into<String>) -> Self {
        Self::new(sqlstate::FEATURE_NOT_SUPPORTED, message)
    }

    pub fn syntax(message: impl Into<String>) -> Self {
        Self::new(sqlstate::SYNTAX_ERROR, message)
    }
}

impl From<StorageError> for BindError {
    fn from(e: StorageError) -> Self {
        let code = match &e {
            StorageError::TableNotFound(_) | StorageError::IndexTableNotFound(_) => {
                sqlstate::UNDEFINED_TABLE
            }
            StorageError::TableAlreadyExists(_) | StorageError::RelationAlreadyExists(_) => {
                sqlstate::DUPLICATE_TABLE
            }
            StorageError::SchemaAlreadyExists(_) => sqlstate::DUPLICATE_SCHEMA,
            StorageError::SchemaNotFound(_) => sqlstate::INVALID_SCHEMA_NAME,
        };
        Self::new(code, e.to_string())
    }
}
