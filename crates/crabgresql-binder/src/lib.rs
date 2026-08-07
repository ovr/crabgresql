//! Binder: semantic analysis turning the sqlparser AST into a typed logical
//! plan — name resolution against table schemas, operator type inference with
//! PG semantics (int4/int8 promotion, untyped-literal coercion), and honest
//! rejection (`0A000`) of everything parsed but not yet executable.

mod collation;
mod expr;
mod functions;
mod plan;
pub mod ruleutils;
mod soft_input;

pub use collation::{
    Derived, Strength, check_explicit_conflict, collation_name, column_collation, expr_collation,
    output_collation, resolve_collation,
};
pub use expr::{
    BinOp, Binding, BoundAggregate, BoundExpr, BoundWindowFunc, BoundWindowSpec, ColumnDefault,
    ParamCtx, ParamState, Scope, Subplan, UnaryOp, WindowKind, WindowSortKey,
    bind_check_constraint, bind_column_default, bind_expr, bind_scalar, bind_sql_function_body,
    bool_test_clause, builtin_type_from_syntax, checked_length_typmod, checked_numeric_typmod,
    coerce_to_column, const_type_label, datetime_precision, declared_typmod,
    deparse_literal_default, inline_params, interval_typmod, length_typmod, literal_int,
    map_data_type, param_ctx_capped, param_ctx_extended, param_ctx_none, param_types,
    require_all_resolved, resolve_data_type,
};
pub use functions::{
    AggFn, GeoFn, JsonFn, JsonPathFn, ScalarFn, TableFn, TsFn, WindowFn, lookup_table_fn,
};
pub use plan::{
    AggInput, CopyFormat, CopyFromPlan, CopyFromSource, CopyHeader, DistinctKey, InsertSource,
    JoinExpr, JoinInput, JoinKind, LogicalPlan, MappedRelation, Returning, SortKey, bind_copy_from,
    bind_delete, bind_delete_with_params, bind_insert, bind_insert_with_params, bind_query,
    bind_query_with_params, bind_update, bind_update_with_params, inheritance_descendants,
    output_columns_of, plan_calls_routine, plan_has_outer_refs, substitute_outer,
    substitute_params,
};
pub use soft_input::{SoftError, TypeSpec, soft_input};

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
    /// The type modifier this column carries, in the same *raw* encoding as
    /// [`crabgresql_storage_api::Column::typmod`] (`-1` for none). Only a
    /// reference to a modifier-bearing column, or an explicit coercion to one,
    /// produces a non-`-1` value; everything computed loses it, exactly as
    /// PostgreSQL's `exprTypmod` does. `CREATE VIEW` reads it so a view's
    /// columns describe themselves as `character varying(20)` rather than as a
    /// bare `character varying`.
    pub typmod: i32,
}

impl OutputColumn {
    /// A column on its type's default collation, with no type modifier.
    pub fn new(name: impl Into<String>, ty: PgType) -> Self {
        OutputColumn {
            name: name.into(),
            ty,
            collation: None,
            strength: Strength::None,
            typmod: -1,
        }
    }

    /// The same column carrying a type modifier.
    pub fn with_typmod(mut self, typmod: i32) -> Self {
        self.typmod = typmod;
        self
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
    /// The `CONTEXT:` traceback: the call frames this error unwound through,
    /// innermost first. Non-empty only when binding happened inside a routine
    /// body (a `LANGUAGE SQL` inline expansion, or a statement in a PL/pgSQL
    /// body bound at call time).
    pub context: Vec<String>,
    /// Set when this error is the operator resolver's own "operator does not
    /// exist" — as opposed to one an *operand* raised on its way out through
    /// the same call. `bind_binary` uses it to decide whether the caret belongs
    /// under the operator token, which is a question the SQLSTATE cannot answer:
    /// `42883` is also `function nosuchfn(integer) does not exist` raised while
    /// binding an operand, where PG points at the function instead.
    pub blames_operator: bool,
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
            context: Vec::new(),
            blames_operator: false,
        }
    }

    /// Attach a DETAIL line.
    pub fn with_detail(mut self, detail: Option<String>) -> Self {
        self.detail = detail;
        self
    }

    /// Record the call frame this error is propagating out of. Called while
    /// unwinding, so frames land innermost-first without a frame stack.
    pub fn push_context(mut self, frame: impl Into<String>) -> Self {
        self.context.push(frame.into());
        self
    }

    /// The `CONTEXT` wire field: frames newline-joined, innermost first, or
    /// `None` when no frame contributed.
    pub fn context(&self) -> Option<String> {
        (!self.context.is_empty()).then(|| self.context.join("\n"))
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
            StorageError::UnsupportedOperation(_) | StorageError::UnsupportedType(_) => {
                sqlstate::FEATURE_NOT_SUPPORTED
            }
            StorageError::Io(_) => sqlstate::IO_ERROR,
            StorageError::CorruptData(_) => "XX001",
            StorageError::RowTooBig { .. }
            | StorageError::ValueTooBig { .. }
            | StorageError::IndexRowTooBig { .. } => sqlstate::PROGRAM_LIMIT_EXCEEDED,
        };
        Self::new(code, e.to_string())
    }
}
