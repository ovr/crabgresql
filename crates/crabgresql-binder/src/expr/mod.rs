//! Expression binding: sqlparser `Expr` → typed [`BoundExpr`] IR.
//!
//! PG resolves operator overloads over concrete types while string literals
//! and `NULL` start out as the pseudo-type `unknown` and take their type from
//! context. [`Binding`] models exactly that: an expression is either typed or
//! still unknown, and every operator/assignment site decides what unknown
//! becomes (or rejects it the way PG does).
//!
//! Clean-room (see AGENTS.md): the resolution rules, coercions, and error text
//! reproduce PG's *observable* behavior, pinned by the regression corpus.

mod assign;
mod bind;
mod bound;
mod coerce;
mod datatype;
mod function_body;
mod literal;
mod operators;
mod params;
mod scope;

pub use assign::{
    ColumnDefault, bind_check_constraint, bind_column_default, bind_generation_expr,
    bind_stored_generation, coerce_to_column, coerce_to_param, const_type_label,
    deparse_literal_default, parse_stored_expr, subquery_in_execute_param,
};
pub use bind::{bind_expr, bind_scalar};
pub use bound::{
    BinOp, BoundAggregate, BoundExpr, BoundWindowFunc, BoundWindowSpec, Subplan, SubplanId,
    UnaryOp, WindowKind, WindowSortKey,
};
pub use datatype::{
    builtin_type_from_syntax, checked_length_typmod, checked_numeric_typmod, datetime_precision,
    declared_typmod, interval_typmod, length_typmod, map_data_type, resolve_data_type,
};
pub use function_body::{bind_sql_function_body, inline_params};
pub use literal::literal_int;
pub use operators::bool_test_clause;
pub use params::{
    ParamCtx, ParamState, param_ctx_capped, param_ctx_extended, param_ctx_none, param_types,
    require_all_resolved,
};
pub use scope::{Binding, Scope, reject_agg_or_window};

pub(crate) use bind::{bind_coalesce, bind_nullif, bind_projection, output_name};
pub(crate) use coerce::{
    ArgFail, binding_type_label, coerce_expr, coerce_for_arg, enum_value, implicit_castable,
    merge_types, parse_unknown, resolve_unknown_ctx, to_bool_operand, type_label,
    unify_value_column,
};
pub(crate) use datatype::{
    apply_column_typmod, apply_datetime_precision, builtin_custom_type, has_equality, is_orderable,
    numeric_typmod,
};
pub(crate) use operators::{
    bind_array_function, bind_binary_op, is_text_family, to_concat_operand,
};
pub(crate) use params::{ViewExpansion, param_ctx_view_body, view_expansion};
pub(crate) use scope::column_value;
pub(crate) use scope::{
    NamedWindows, OuterLevel, ScopeItem, VisibleColumn, VisibleLookup, common_typmod,
    lookup_visible, normalize_ident, projection_typmod, reject_window,
};
