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

use std::sync::Arc;

use crabgresql_parser::{Span, ast};
use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::{Column, TableSchema, TypeCatalog, UserCast};
use crabgresql_types::numeric::ParseError;
use crabgresql_types::{
    Numeric, PgType, Value, cast, date, float, interval, money, parse_bool, time, timestamp,
    timestamptz, timetz,
};

use crate::BindError;
use crate::functions::{AggFn, ScalarFn, TableFn, bind_function, bind_srf_projection};

/// Typed expression IR. Every node knows its result type; the evaluator
/// dispatches on the recorded types and never re-infers them.
#[derive(Clone, Debug, PartialEq)]
pub enum BoundExpr {
    Const {
        value: Value,
        ty: PgType,
    },
    ColumnRef {
        index: usize,
        ty: PgType,
    },
    Unary {
        op: UnaryOp,
        expr: Box<BoundExpr>,
    },
    /// `arg_ty` is the operand type after promotion; comparisons and logic
    /// yield bool, arithmetic yields `arg_ty`.
    Binary {
        op: BinOp,
        arg_ty: PgType,
        left: Box<BoundExpr>,
        right: Box<BoundExpr>,
    },
    IsNull {
        expr: Box<BoundExpr>,
        negated: bool,
    },
    /// Runtime cast, evaluated by `executor::eval::coerce_value` via the shared
    /// `crabgresql_types::cast::cast_value`.
    Coerce {
        expr: Box<BoundExpr>,
        ty: PgType,
    },
    /// A binary-coercible (`CREATE CAST ... WITHOUT FUNCTION`) cast: reinterpret
    /// the operand's bit pattern as `rep` (a builtin) at runtime via
    /// `crabgresql_types::cast::reinterpret_value`. `reported` is the cast's
    /// declared result type — possibly a `PgType::User(oid)` — while `rep` is the
    /// concrete builtin the value is physically stored as.
    Reinterpret {
        expr: Box<BoundExpr>,
        reported: PgType,
        rep: PgType,
    },
    /// A scalar function call; `ret` is the result type.
    FuncCall {
        func: ScalarFn,
        ret: PgType,
        args: Vec<BoundExpr>,
    },
    /// `CASE`: WHEN conditions are boolean and evaluated top-to-bottom; the
    /// first one that is *true* selects its result. `else_` is present only for
    /// an explicit ELSE (a missing ELSE yields NULL). Every result is already
    /// coerced to `ty`, the unified result type.
    Case {
        whens: Vec<(BoundExpr, BoundExpr)>,
        else_: Option<Box<BoundExpr>>,
        ty: PgType,
    },
    /// A set-returning function in the SELECT target list; `ret` is the element
    /// (per-row output) type. This is a marker that is only legal at the top
    /// level of a projection: the `ProjectSet` executor node expands it into
    /// rows. Evaluating it as a scalar is an error (see `executor::eval`).
    Srf {
        func: TableFn,
        ret: PgType,
        args: Vec<BoundExpr>,
    },
    /// An aggregate call (`min(x)`, `count(*)`, …). A transient marker: it may
    /// appear anywhere in a target-list / HAVING / ORDER BY expression, but the
    /// binder extracts every aggregate into a [`crate::LogicalPlan::Aggregate`]
    /// and rewrites the marker to a `ColumnRef` into the aggregate's output row
    /// before planning. Evaluating it as a scalar is a bug (see `executor::eval`).
    Aggregate {
        func: AggFn,
        /// Whether duplicate non-NULL input values are eliminated before this
        /// aggregate accumulates them.
        distinct: bool,
        /// `None` for `COUNT(*)` (count every row); the per-row argument
        /// expression otherwise.
        arg: Option<Box<BoundExpr>>,
        /// The argument's (pre-aggregation) type — drives accumulator dispatch.
        /// Unused for `COUNT(*)`.
        input_ty: PgType,
        /// The aggregate's result type (see `agg_return_type`).
        ret: PgType,
    },
}

/// One aggregate call extracted from a query's expressions, occupying one slot
/// of the aggregate node's output row (after the group keys). Produced by the
/// binder's aggregate-extraction pass from a [`BoundExpr::Aggregate`] marker.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundAggregate {
    pub func: AggFn,
    /// Whether duplicate non-NULL input values are eliminated per group before
    /// accumulation.
    pub distinct: bool,
    /// Evaluated per source row; `None` = `COUNT(*)`.
    pub arg: Option<BoundExpr>,
    pub input_ty: PgType,
    pub ret: PgType,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Neg,
    /// `@` — absolute value; result type is the operand type.
    Abs,
    /// `|/` — square root (float8).
    Sqrt,
    /// `||/` — cube root (float8).
    Cbrt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    /// `^` — exponentiation (float8).
    Pow,
}

impl BinOp {
    pub fn is_arithmetic(self) -> bool {
        matches!(
            self,
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Pow
        )
    }

    fn is_logic(self) -> bool {
        matches!(self, BinOp::And | BinOp::Or)
    }

    /// The operator's SQL spelling, as it appears in PG error messages.
    pub fn sql_symbol(self) -> &'static str {
        match self {
            BinOp::Eq => "=",
            BinOp::NotEq => "<>",
            BinOp::Lt => "<",
            BinOp::LtEq => "<=",
            BinOp::Gt => ">",
            BinOp::GtEq => ">=",
            BinOp::And => "AND",
            BinOp::Or => "OR",
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::Pow => "^",
        }
    }
}

impl BoundExpr {
    pub fn ty(&self) -> PgType {
        match self {
            BoundExpr::Const { ty, .. } => *ty,
            BoundExpr::ColumnRef { ty, .. } => *ty,
            BoundExpr::Unary {
                op: UnaryOp::Not, ..
            } => PgType::Bool,
            // Neg/Abs keep the operand type; Sqrt/Cbrt operands were coerced to
            // float8, so the operand type is already the result type.
            BoundExpr::Unary { expr, .. } => expr.ty(),
            BoundExpr::Binary { op, arg_ty, .. } => {
                if op.is_arithmetic() {
                    *arg_ty
                } else {
                    PgType::Bool
                }
            }
            BoundExpr::IsNull { .. } => PgType::Bool,
            BoundExpr::Coerce { ty, .. } => *ty,
            BoundExpr::Reinterpret { reported, .. } => *reported,
            BoundExpr::FuncCall { ret, .. } => *ret,
            BoundExpr::Case { ty, .. } => *ty,
            BoundExpr::Srf { ret, .. } => *ret,
            BoundExpr::Aggregate { ret, .. } => *ret,
        }
    }

    /// Whether this is a set-returning function marker (only legal at the top
    /// level of a projection list).
    pub fn is_srf(&self) -> bool {
        matches!(self, BoundExpr::Srf { .. })
    }

    /// Whether this node itself is an aggregate marker.
    pub fn is_aggregate(&self) -> bool {
        matches!(self, BoundExpr::Aggregate { .. })
    }

    /// Whether this expression tree contains an aggregate marker anywhere.
    pub fn contains_aggregate(&self) -> bool {
        match self {
            BoundExpr::Aggregate { .. } => true,
            BoundExpr::Const { .. } | BoundExpr::ColumnRef { .. } => false,
            BoundExpr::Unary { expr, .. } => expr.contains_aggregate(),
            BoundExpr::Binary { left, right, .. } => {
                left.contains_aggregate() || right.contains_aggregate()
            }
            BoundExpr::IsNull { expr, .. } => expr.contains_aggregate(),
            BoundExpr::Coerce { expr, .. } => expr.contains_aggregate(),
            BoundExpr::Reinterpret { expr, .. } => expr.contains_aggregate(),
            BoundExpr::FuncCall { args, .. } | BoundExpr::Srf { args, .. } => {
                args.iter().any(BoundExpr::contains_aggregate)
            }
            BoundExpr::Case { whens, else_, .. } => {
                whens
                    .iter()
                    .any(|(c, r)| c.contains_aggregate() || r.contains_aggregate())
                    || else_.as_ref().is_some_and(|e| e.contains_aggregate())
            }
        }
    }
}

/// A binding result: typed, or an untyped literal awaiting context (PG's
/// `unknown` pseudo-type). `lit == None` is the `NULL` literal; `span` locates
/// the literal for error positions.
#[derive(Clone, Debug, PartialEq)]
pub enum Binding {
    Typed(BoundExpr),
    Unknown { lit: Option<String>, span: Span },
}

/// One relation in a name-resolution scope: its qualifier (alias, else table
/// name), its columns, and the base index its columns occupy in the combined
/// row (0 for a single relation; the running total across FROM items in a
/// cross join).
pub struct ScopeRel {
    qualifier: String,
    columns: Vec<Column>,
    offset: usize,
}

/// Name-resolution scope: an ordered list of relations (empty for a FROM-less
/// SELECT / INSERT VALUES, one for a single-table SELECT, more for a cross
/// join). A qualified reference addresses one relation by its name or alias;
/// with an alias the bare table name is not a valid qualifier — as in PG.
pub struct Scope {
    rels: Vec<ScopeRel>,
    /// User-defined type/cast view, so an expression cast to/from a `CREATE TYPE`
    /// name resolves and a `WITHOUT FUNCTION` cast can be applied.
    catalog: Arc<dyn TypeCatalog>,
}

impl Scope {
    /// No tables in scope: FROM-less SELECT, INSERT VALUES.
    pub fn empty(catalog: &Arc<dyn TypeCatalog>) -> Scope {
        Scope { rels: Vec::new(), catalog: catalog.clone() }
    }

    pub fn table(schema: &TableSchema, qualifier: String, catalog: &Arc<dyn TypeCatalog>) -> Scope {
        Scope {
            rels: vec![ScopeRel {
                qualifier,
                columns: schema.columns.clone(),
                offset: 0,
            }],
            catalog: catalog.clone(),
        }
    }

    /// A multi-relation scope for a cross join. Each `(qualifier, columns)` pair
    /// becomes a relation; offsets are assigned left-to-right so a column's
    /// index is its position in the concatenated row.
    pub fn relations(items: Vec<(String, Vec<Column>)>, catalog: &Arc<dyn TypeCatalog>) -> Scope {
        let mut offset = 0;
        let mut rels = Vec::with_capacity(items.len());
        for (qualifier, columns) in items {
            let width = columns.len();
            rels.push(ScopeRel {
                qualifier,
                columns,
                offset,
            });
            offset += width;
        }
        Scope { rels, catalog: catalog.clone() }
    }

    /// The user-defined type/cast view carried through binding.
    pub fn catalog(&self) -> &Arc<dyn TypeCatalog> {
        &self.catalog
    }

    /// Resolve an unqualified column name. A name that matches exactly one
    /// column binds to its combined-row index; more than one match — whether
    /// across relations or duplicated within one (e.g. an alias list `v(x, x)`)
    /// — is `42702` (ambiguous); no match is `42703`.
    fn resolve(&self, name: &str) -> Result<BoundExpr, BindError> {
        let mut found: Option<(usize, PgType)> = None;
        for rel in &self.rels {
            for (local, col) in rel.columns.iter().enumerate() {
                if col.name == name {
                    if found.is_some() {
                        return Err(BindError::new(
                            sqlstate::AMBIGUOUS_COLUMN,
                            format!("column reference \"{name}\" is ambiguous"),
                        ));
                    }
                    found = Some((rel.offset + local, col.ty));
                }
            }
        }
        let (index, ty) = found.ok_or_else(|| {
            BindError::new(
                sqlstate::UNDEFINED_COLUMN,
                format!("column \"{name}\" does not exist"),
            )
        })?;
        Ok(BoundExpr::ColumnRef { index, ty })
    }

    /// Find the relation addressed by `qualifier` (alias or table name), or the
    /// `42P01` "missing FROM-clause entry" error PG reports when no such
    /// relation is in scope.
    fn relation(&self, qualifier: &str) -> Result<&ScopeRel, BindError> {
        self.rels
            .iter()
            .find(|r| r.qualifier == qualifier)
            .ok_or_else(|| {
                BindError::new(
                    sqlstate::UNDEFINED_TABLE,
                    format!("missing FROM-clause entry for table \"{qualifier}\""),
                )
            })
    }

    /// Expand `*`: every relation's columns in FROM order, each as an output
    /// column paired with a `ColumnRef` at its combined-row index. Duplicate
    /// output names are allowed, as in PG.
    pub fn expand_wildcard(&self) -> Vec<(crate::OutputColumn, BoundExpr)> {
        let mut out = Vec::new();
        for rel in &self.rels {
            expand_rel(rel, &mut out);
        }
        out
    }

    /// The `qualifier.name` label of the column at a combined-row `index`, as PG
    /// spells it in the "must appear in the GROUP BY clause" error. Falls back to
    /// `?column?` if the index is past every relation (should not happen).
    pub fn column_label(&self, index: usize) -> String {
        for rel in &self.rels {
            if index >= rel.offset && index < rel.offset + rel.columns.len() {
                return format!("{}.{}", rel.qualifier, rel.columns[index - rel.offset].name);
            }
        }
        "?column?".to_string()
    }

    /// Expand `q.*`: the columns of the relation addressed by `q`, or `42P01`
    /// if `q` is not in scope.
    pub fn expand_qualified(
        &self,
        qualifier: &str,
    ) -> Result<Vec<(crate::OutputColumn, BoundExpr)>, BindError> {
        let rel = self.relation(qualifier)?;
        let mut out = Vec::new();
        expand_rel(rel, &mut out);
        Ok(out)
    }
}

/// Append every column of `rel` as an `(output column, ColumnRef)` pair at its
/// combined-row index.
fn expand_rel(rel: &ScopeRel, out: &mut Vec<(crate::OutputColumn, BoundExpr)>) {
    for (i, col) in rel.columns.iter().enumerate() {
        out.push((
            crate::OutputColumn {
                name: col.name.clone(),
                ty: col.ty,
            },
            BoundExpr::ColumnRef {
                index: rel.offset + i,
                ty: col.ty,
            },
        ));
    }
}

/// Unquoted identifiers fold to lowercase, as in PG.
pub(crate) fn normalize_ident(ident: &ast::Ident) -> String {
    match ident.quote_style {
        Some(_) => ident.value.clone(),
        None => ident.value.to_lowercase(),
    }
}

pub fn bind_expr(expr: &ast::Expr, scope: &Scope) -> Result<Binding, BindError> {
    match expr {
        ast::Expr::Value(v) => bind_value(v),
        // The DEFAULT keyword (INSERT VALUES / UPDATE SET) parses as a plain
        // identifier; without this check it would bind as a column reference
        // and mislead with `column "default" does not exist`. A real column
        // named "default" must be quoted, which keeps quote_style set.
        ast::Expr::Identifier(ident)
            if ident.quote_style.is_none() && ident.value.eq_ignore_ascii_case("default") =>
        {
            Err(BindError::feature_not_supported(
                "DEFAULT is not supported yet",
            ))
        }
        ast::Expr::Identifier(ident) => scope.resolve(&normalize_ident(ident)).map(Binding::Typed),
        ast::Expr::CompoundIdentifier(parts) => bind_compound(parts, scope).map(Binding::Typed),
        ast::Expr::Nested(inner) => bind_expr(inner, scope),
        ast::Expr::UnaryOp { op, expr } => bind_unary(*op, expr, scope),
        ast::Expr::BinaryOp { left, op, right } => bind_binary(left, op, right, scope),
        ast::Expr::IsNull(inner) => bind_is_null(inner, scope, false),
        ast::Expr::IsNotNull(inner) => bind_is_null(inner, scope, true),
        ast::Expr::Cast {
            expr, data_type, ..
        } => bind_cast(expr, data_type, scope),
        ast::Expr::TypedString(ts) => bind_typed_string(ts),
        ast::Expr::Function(func) => bind_function(func, scope),
        ast::Expr::Ceil { expr, field } => {
            crate::functions::bind_ceil_floor("ceil", expr, field, scope)
        }
        ast::Expr::Floor { expr, field } => {
            crate::functions::bind_ceil_floor("floor", expr, field, scope)
        }
        ast::Expr::Extract { field, expr, .. } => bind_extract(field, expr, scope),
        ast::Expr::Interval(iv) => bind_interval(iv),
        ast::Expr::AtTimeZone { timestamp, time_zone } => {
            bind_at_time_zone(timestamp, time_zone, scope)
        }
        ast::Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => bind_case(
            operand.as_deref(),
            conditions,
            else_result.as_deref(),
            scope,
        ),
        // String special-syntax expressions desugar to the equivalent function.
        ast::Expr::Substring { expr, substring_from, substring_for, .. } => {
            bind_substring(expr, substring_from.as_deref(), substring_for.as_deref(), scope)
        }
        ast::Expr::Trim { expr, trim_where, trim_what, trim_characters } => {
            bind_trim(expr, *trim_where, trim_what.as_deref(), trim_characters.as_deref(), scope)
        }
        ast::Expr::Position { expr, r#in } => {
            // POSITION(sub IN str) == strpos(str, sub).
            let sub = bind_expr(expr, scope)?;
            let str_ = bind_expr(r#in, scope)?;
            crate::functions::resolve_call("strpos", vec![str_, sub])
        }
        ast::Expr::Overlay { expr, overlay_what, overlay_from, overlay_for } => {
            bind_overlay(expr, overlay_what, overlay_from, overlay_for.as_deref(), scope)
        }
        ast::Expr::Like { negated, any, expr, pattern, escape_char } => {
            bind_like_node(expr, pattern, escape_char.as_ref(), *any, false, *negated, scope)
        }
        ast::Expr::ILike { negated, any, expr, pattern, escape_char } => {
            bind_like_node(expr, pattern, escape_char.as_ref(), *any, true, *negated, scope)
        }
        ast::Expr::InList { expr, list, negated } => bind_in_list(expr, list, *negated, scope),
        other => Err(unsupported_expr(other)),
    }
}

/// `SUBSTRING(x [FROM a] [FOR b])` → `substr(x, a[, b])`. With no `FROM`, PG
/// defaults the start to 1.
fn bind_substring(
    expr: &ast::Expr,
    from: Option<&ast::Expr>,
    for_: Option<&ast::Expr>,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let subject = bind_expr(expr, scope)?;
    let start = match from {
        Some(e) => bind_expr(e, scope)?,
        None => Binding::Typed(BoundExpr::Const { value: Value::Int4(1), ty: PgType::Int4 }),
    };
    let mut args = vec![subject, start];
    if let Some(e) = for_ {
        args.push(bind_expr(e, scope)?);
    }
    crate::functions::resolve_call("substr", args)
}

/// `TRIM([LEADING|TRAILING|BOTH] [chars FROM] x)` → `ltrim`/`rtrim`/`btrim`.
fn bind_trim(
    expr: &ast::Expr,
    side: Option<ast::TrimWhereField>,
    trim_what: Option<&ast::Expr>,
    trim_characters: Option<&[ast::Expr]>,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let func = match side {
        Some(ast::TrimWhereField::Leading) => "ltrim",
        Some(ast::TrimWhereField::Trailing) => "rtrim",
        Some(ast::TrimWhereField::Both) | None => "btrim",
    };
    let subject = bind_expr(expr, scope)?;
    let mut args = vec![subject];
    // `TRIM(chars FROM x)` and the `TRIM(x, chars)` comma form both give a
    // characters argument.
    if let Some(chars) = trim_what {
        args.push(bind_expr(chars, scope)?);
    } else if let Some([chars]) = trim_characters {
        args.push(bind_expr(chars, scope)?);
    } else if trim_characters.is_some_and(|c| !c.is_empty()) {
        return Err(BindError::feature_not_supported(
            "TRIM with multiple characters is not supported yet",
        ));
    }
    crate::functions::resolve_call(func, args)
}

/// `OVERLAY(x PLACING r FROM a [FOR b])` → `overlay(x, r, a[, b])`.
fn bind_overlay(
    expr: &ast::Expr,
    what: &ast::Expr,
    from: &ast::Expr,
    for_: Option<&ast::Expr>,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let mut args =
        vec![bind_expr(expr, scope)?, bind_expr(what, scope)?, bind_expr(from, scope)?];
    if let Some(e) = for_ {
        args.push(bind_expr(e, scope)?);
    }
    crate::functions::resolve_call("overlay", args)
}

/// Bind a `LIKE`/`ILIKE` expression node (as opposed to the operator form).
fn bind_like_node(
    expr: &ast::Expr,
    pattern: &ast::Expr,
    escape_char: Option<&ast::ValueWithSpan>,
    any: bool,
    case_insensitive: bool,
    negated: bool,
    scope: &Scope,
) -> Result<Binding, BindError> {
    if any {
        return Err(BindError::feature_not_supported("LIKE ANY is not supported yet"));
    }
    let lb = bind_expr(expr, scope)?;
    let rb = bind_expr(pattern, scope)?;
    let escape = match escape_char {
        Some(v) => match &v.value {
            ast::Value::SingleQuotedString(s) => {
                Some(Binding::Typed(BoundExpr::Const {
                    value: Value::Text(s.clone()),
                    ty: PgType::Text,
                }))
            }
            other => {
                return Err(BindError::syntax(format!("invalid ESCAPE literal: {other}")));
            }
        },
        None => None,
    };
    bind_like(lb, rb, escape, case_insensitive, negated)
}

/// Bind at a spot with no surrounding type context (a SELECT-list item):
/// a leftover unknown resolves to text, as PG does in a bare SELECT.
pub fn bind_scalar(expr: &ast::Expr, scope: &Scope) -> Result<BoundExpr, BindError> {
    Ok(match bind_expr(expr, scope)? {
        Binding::Typed(e) => e,
        Binding::Unknown { lit, span } => resolve_unknown(lit, span, PgType::Text)?,
    })
}

/// Bind a SELECT-list item. A top-level call to a set-returning function
/// (currently `generate_series`) binds to a [`BoundExpr::Srf`] marker that the
/// executor's `ProjectSet` node expands into rows; everything else binds as an
/// ordinary scalar via [`bind_scalar`].
pub fn bind_projection(expr: &ast::Expr, scope: &Scope) -> Result<BoundExpr, BindError> {
    if let ast::Expr::Function(func) = expr
        && let Some(srf) = bind_srf_projection(func, scope)?
    {
        return Ok(srf);
    }
    bind_scalar(expr, scope)
}

fn unsupported_expr(expr: &ast::Expr) -> BindError {
    BindError::feature_not_supported(format!("expression is not supported yet: {expr}"))
}

/// Types the executor's `compare_values` can order. Both comparison operators
/// (`=`, `<`, …) and ORDER BY require this — binding a sort or comparison on any
/// other type would produce a node the evaluator can't handle.
pub(crate) fn is_orderable(ty: PgType) -> bool {
    matches!(
        ty,
        PgType::Bool
            | PgType::Bit
            | PgType::Varbit
            | PgType::Int2
            | PgType::Int4
            | PgType::Int8
            | PgType::Float4
            | PgType::Float8
            | PgType::Numeric
            | PgType::Text
            | PgType::Varchar
            | PgType::Bpchar
            | PgType::Name
            | PgType::Oid
            | PgType::Bytea
            | PgType::Date
            | PgType::Time
            | PgType::TimeTz
            | PgType::Timestamp
            | PgType::Interval
            | PgType::TimestampTz
            | PgType::Uuid
            | PgType::Inet
            | PgType::Cidr
            | PgType::Money
            | PgType::Macaddr
            | PgType::Macaddr8
    )
}

fn bind_value(value: &ast::ValueWithSpan) -> Result<Binding, BindError> {
    match &value.value {
        ast::Value::Number(n, _) => parse_number(n).map(Binding::Typed),
        ast::Value::SingleQuotedString(s)
        | ast::Value::DollarQuotedString(ast::DollarQuotedString { value: s, .. }) => {
            Ok(Binding::Unknown {
                lit: Some(s.clone()),
                span: value.span,
            })
        }
        ast::Value::Boolean(b) => Ok(Binding::Typed(BoundExpr::Const {
            value: Value::Bool(*b),
            ty: PgType::Bool,
        })),
        ast::Value::Null => Ok(Binding::Unknown {
            lit: None,
            span: value.span,
        }),
        // `B'...'` is a `bit(n)` literal (n binary digits); `X'...'` a `bit(4n)`
        // literal (4 bits per hex digit). PG's `bit_in` rejects a bad digit with
        // a data exception (22P02) naming the offender, at the literal's cursor.
        ast::Value::SingleQuotedByteStringLiteral(s) => bind_bit_literal(
            crabgresql_types::bit::from_binary(s),
            value.span,
        ),
        ast::Value::HexStringLiteral(s) => {
            bind_bit_literal(crabgresql_types::bit::from_hex(s), value.span)
        }
        other => Err(BindError::feature_not_supported(format!(
            "literal is not supported yet: {other}"
        ))),
    }
}

/// Build a `bit` constant from a parsed `B'...'`/`X'...'` literal, attaching the
/// literal's cursor position to a bad-digit error (so it renders `LINE n: ^`).
fn bind_bit_literal(
    parsed: Result<(u32, Vec<u8>), crabgresql_types::bit::BitError>,
    span: Span,
) -> Result<Binding, BindError> {
    let (len, data) = parsed.map_err(|e| BindError::new(e.sqlstate, e.message).at(span))?;
    Ok(Binding::Typed(BoundExpr::Const {
        value: Value::Bit { len, data },
        ty: PgType::Bit,
    }))
}

/// Integer literals become int4 when they fit, int8 otherwise. Literals with a
/// decimal point or exponent bind as `numeric`, as PG does — a numeric constant
/// keeps its exact value and display scale.
fn parse_number(n: &str) -> Result<BoundExpr, BindError> {
    if let Ok(v) = n.parse::<i32>() {
        return Ok(BoundExpr::Const {
            value: Value::Int4(v),
            ty: PgType::Int4,
        });
    }
    if let Ok(v) = n.parse::<i64>() {
        return Ok(BoundExpr::Const {
            value: Value::Int8(v),
            ty: PgType::Int8,
        });
    }
    // A whole-number literal too large for int8, or one with a decimal point or
    // exponent, is `numeric` (PG's `numeric` type for any unsuffixed decimal).
    match Numeric::parse(n) {
        Ok(value) => Ok(BoundExpr::Const {
            value: Value::Numeric(value),
            ty: PgType::Numeric,
        }),
        Err(ParseError::Overflow) => Err(BindError::new(
            sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
            "value overflows numeric format",
        )),
        Err(ParseError::Syntax) => Err(BindError::feature_not_supported(format!(
            "numeric literal \"{n}\" is not supported yet"
        ))),
    }
}

/// Map a SQL type name to a `PgType`. Shared by cast/typed-string binding and
/// server-side CREATE TABLE.
pub fn map_data_type(dt: &ast::DataType) -> Result<PgType, BindError> {
    use ast::DataType;
    Ok(match dt {
        DataType::Bool | DataType::Boolean => PgType::Bool,
        DataType::SmallInt(_) | DataType::Int2(_) => PgType::Int2,
        DataType::Int(_) | DataType::Integer(_) | DataType::Int4(_) => PgType::Int4,
        DataType::BigInt(_) | DataType::Int8(_) => PgType::Int8,
        DataType::Real | DataType::Float4 => PgType::Float4,
        DataType::DoublePrecision | DataType::Float8 => PgType::Float8,
        DataType::Double(_) => PgType::Float8,
        // float(p): p <= 24 is single precision, else double (PG semantics).
        DataType::Float(info) => match precision_of(info) {
            Some(p) if p <= 24 => PgType::Float4,
            _ => PgType::Float8,
        },
        DataType::Numeric(_) | DataType::Decimal(_) => PgType::Numeric,
        DataType::Bytea => PgType::Bytea,
        // `date`.
        DataType::Date => PgType::Date,
        // `time` / `time with time zone` (the precision modifier is accepted and
        // ignored; full microsecond resolution is kept).
        DataType::Time(_, tz) => match tz {
            ast::TimezoneInfo::None | ast::TimezoneInfo::WithoutTimeZone => PgType::Time,
            ast::TimezoneInfo::WithTimeZone | ast::TimezoneInfo::Tz => PgType::TimeTz,
        },
        // `timestamp` / `timestamp with time zone` (the precision modifier is
        // ignored; full microsecond resolution is kept).
        DataType::Timestamp(_, tz) => match tz {
            ast::TimezoneInfo::None | ast::TimezoneInfo::WithoutTimeZone => PgType::Timestamp,
            ast::TimezoneInfo::WithTimeZone | ast::TimezoneInfo::Tz => PgType::TimestampTz,
        },
        // `interval` (any field qualifier / precision is accepted and ignored;
        // full resolution is kept, as with `timestamp(2)`).
        DataType::Interval { .. } => PgType::Interval,
        DataType::Uuid => PgType::Uuid,
        DataType::Inet => PgType::Inet,
        DataType::Cidr => PgType::Cidr,
        DataType::Text => PgType::Text,
        // `varchar`/`character varying` (with or without a length limit).
        DataType::Varchar(_) | DataType::CharacterVarying(_) => PgType::Varchar,
        // `char`/`character` (blank-padded; a bare `char` is `char(1)`).
        DataType::Char(_) | DataType::Character(_) => PgType::Bpchar,
        // `bit(n)` (fixed) and `bit varying(n)` / `varbit` (variable); the length
        // is enforced separately as a typmod coercion.
        DataType::Bit(_) => PgType::Bit,
        DataType::BitVarying(_) | DataType::VarBit(_) => PgType::Varbit,
        // Geometric types. `point`/`lseg` are modeled; the rest are not yet.
        DataType::GeometricType(kind) => match kind {
            ast::GeometricTypeKind::Point => PgType::Point,
            ast::GeometricTypeKind::LineSegment => PgType::Lseg,
            other => {
                return Err(BindError::feature_not_supported(format!(
                    "type \"{other}\" is not supported yet"
                )));
            }
        },
        // `bpchar` (no length = unlimited, like text) and `name` arrive as
        // custom type names.
        DataType::Custom(obj, mods) if mods.is_empty() => {
            match obj.0.last().and_then(|p| p.as_ident()).map(normalize_ident).as_deref() {
                Some("bpchar") => PgType::Bpchar,
                Some("varchar") => PgType::Varchar,
                Some("name") => PgType::Name,
                Some("money") => PgType::Money,
                Some("oid") => PgType::Oid,
                Some("macaddr") => PgType::Macaddr,
                Some("macaddr8") => PgType::Macaddr8,
                // `point`/`lseg` reach here as bareword type names (`::point`,
                // `f1 point`); the `point '...'`/`GeometricType` path is separate.
                Some("point") => PgType::Point,
                Some("lseg") => PgType::Lseg,
                _ => {
                    return Err(BindError::feature_not_supported(format!(
                        "type \"{dt}\" is not supported yet"
                    )));
                }
            }
        }
        other => {
            return Err(BindError::feature_not_supported(format!(
                "type \"{other}\" is not supported yet"
            )));
        }
    })
}

fn precision_of(info: &ast::ExactNumberInfo) -> Option<u64> {
    match info {
        ast::ExactNumberInfo::None => None,
        ast::ExactNumberInfo::Precision(p) => Some(*p),
        ast::ExactNumberInfo::PrecisionAndScale(p, _) => Some(*p),
    }
}

fn bind_cast(
    inner: &ast::Expr,
    data_type: &ast::DataType,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let target = match map_data_type(data_type) {
        Ok(t) => t,
        // Not a builtin type name — it may be a `CREATE TYPE` name; resolve it
        // against the catalog, else surface the original "not supported" error.
        Err(e) => match custom_type_name(data_type).and_then(|n| scope.catalog().resolve_type(&n)) {
            Some(ut) => PgType::User(ut.oid),
            None => return Err(e),
        },
    };
    let expr = match bind_expr(inner, scope)? {
        Binding::Unknown { lit, span } => resolve_unknown(lit, span, target)?,
        Binding::Typed(e) => coerce_cast(e, target, scope)?,
    };
    let expr = apply_numeric_typmod_if_any(expr, target, data_type)?;
    Ok(Binding::Typed(apply_length_typmod_if_any(expr, target, data_type)?))
}

/// The (normalized) name of a bare `DataType::Custom` type reference — e.g. the
/// `xfloat4` in `x::xfloat4` — used to look a `CREATE TYPE` name up in the
/// catalog. `None` for anything that is not a plain custom name.
fn custom_type_name(dt: &ast::DataType) -> Option<String> {
    match dt {
        ast::DataType::Custom(obj, mods) if mods.is_empty() => {
            obj.0.last().and_then(|p| p.as_ident()).map(normalize_ident)
        }
        _ => None,
    }
}

/// Coerce `expr` to an explicit-cast `target`. When a user-defined type is on
/// either side, the catalog decides whether the cast exists and how it runs
/// (a `WITHOUT FUNCTION` cast reinterprets the bit pattern); otherwise this is
/// the ordinary builtin coercion.
fn coerce_cast(expr: BoundExpr, target: PgType, scope: &Scope) -> Result<BoundExpr, BindError> {
    if matches!(expr.ty(), PgType::User(_)) || matches!(target, PgType::User(_)) {
        return coerce_user_cast(expr, target, scope);
    }
    coerce_expr(expr, target)
}

/// Apply a cast where at least one side is a user-defined type. Only casts
/// registered via `CREATE CAST` are allowed; a `WITHOUT FUNCTION` one lowers to
/// a `Reinterpret` over the target's backing builtin.
fn coerce_user_cast(
    expr: BoundExpr,
    target: PgType,
    scope: &Scope,
) -> Result<BoundExpr, BindError> {
    let source = expr.ty();
    if source == target {
        return Ok(expr);
    }
    match scope.catalog().find_cast(source, target) {
        Some(UserCast { without_function: true }) => Ok(BoundExpr::Reinterpret {
            expr: Box::new(expr),
            reported: target,
            rep: scope.catalog().backing_rep(target),
        }),
        // WITH FUNCTION / WITH INOUT are rejected at `CREATE CAST`; guard anyway.
        Some(UserCast { without_function: false }) => Err(BindError::feature_not_supported(
            "cast with a conversion function is not supported yet",
        )),
        None => Err(BindError::new(
            sqlstate::CANNOT_COERCE,
            format!("cannot cast type {} to {}", source.name(), target.name()),
        )),
    }
}

fn bind_typed_string(ts: &ast::TypedString) -> Result<Binding, BindError> {
    let target = map_data_type(&ts.data_type)?;
    let (lit, span) = match &ts.value.value {
        ast::Value::SingleQuotedString(s) => (Some(s.clone()), ts.value.span),
        other => {
            return Err(BindError::syntax(format!(
                "invalid typed literal: {other}"
            )));
        }
    };
    let expr = resolve_unknown(lit, span, target)?;
    let expr = apply_numeric_typmod_if_any(expr, target, &ts.data_type)?;
    Ok(Binding::Typed(apply_length_typmod_if_any(expr, target, &ts.data_type)?))
}

/// The `(precision, scale)` of a `numeric(p[,s])` / `decimal(...)` type name,
/// or `None` for an unconstrained `numeric`. A bare `numeric(p)` has scale 0.
fn numeric_typmod(dt: &ast::DataType) -> Option<(i32, i32)> {
    use ast::{DataType, ExactNumberInfo};
    let info = match dt {
        DataType::Numeric(i) | DataType::Decimal(i) => i,
        _ => return None,
    };
    match info {
        ExactNumberInfo::None => None,
        ExactNumberInfo::Precision(p) => Some((*p as i32, 0)),
        ExactNumberInfo::PrecisionAndScale(p, s) => Some((*p as i32, *s as i32)),
    }
}

/// When `target` is `numeric` and `data_type` carries a `(p,s)` modifier, apply
/// it — folding constants at bind time (so overflow errors here, with PG's
/// DETAIL) and inserting a runtime length-coercion for non-constants.
fn apply_numeric_typmod_if_any(
    expr: BoundExpr,
    target: PgType,
    data_type: &ast::DataType,
) -> Result<BoundExpr, BindError> {
    if target != PgType::Numeric {
        return Ok(expr);
    }
    let Some((precision, scale)) = numeric_typmod(data_type) else {
        return Ok(expr);
    };
    if let BoundExpr::Const { value: Value::Numeric(n), .. } = &expr {
        let applied = n
            .apply_typmod(precision, scale)
            .map_err(|e| BindError::new(e.sqlstate, e.message).with_detail(e.detail))?;
        return Ok(BoundExpr::Const { value: Value::Numeric(applied), ty: PgType::Numeric });
    }
    Ok(BoundExpr::FuncCall {
        func: ScalarFn::NumApplyTypmod,
        ret: PgType::Numeric,
        args: vec![
            expr,
            BoundExpr::Const { value: Value::Int4(precision), ty: PgType::Int4 },
            BoundExpr::Const { value: Value::Int4(scale), ty: PgType::Int4 },
        ],
    })
}

/// The declared character length of a `char(n)`/`varchar(n)` type name. A bare
/// `char`/`character` defaults to length 1; a bare `varchar` has no limit.
pub fn length_typmod(dt: &ast::DataType) -> Option<i32> {
    use ast::DataType;
    fn len(l: &Option<ast::CharacterLength>) -> Option<i32> {
        match l {
            Some(ast::CharacterLength::IntegerLength { length, .. }) => Some(*length as i32),
            _ => None,
        }
    }
    match dt {
        DataType::Char(l) | DataType::Character(l) => Some(len(l).unwrap_or(1)),
        DataType::Varchar(l) | DataType::CharacterVarying(l) => len(l),
        // `bit(n)` defaults to `bit(1)`; `bit varying` with no length is unlimited.
        DataType::Bit(n) => Some(n.map(|n| n as i32).unwrap_or(1)),
        DataType::BitVarying(n) | DataType::VarBit(n) => n.map(|n| n as i32),
        _ => None,
    }
}

/// Apply a `varchar(n)`/`char(n)` length coercion, or a `name` truncation, when
/// the target is one of those types. Constant inputs fold at bind time.
pub(crate) fn apply_length_typmod_if_any(
    expr: BoundExpr,
    target: PgType,
    data_type: &ast::DataType,
) -> Result<BoundExpr, BindError> {
    let (func, typmod) = match target {
        PgType::Varchar => match length_typmod(data_type) {
            Some(n) => (ScalarFn::VarcharTypmod, Some(n)),
            None => return Ok(expr),
        },
        PgType::Bpchar => match length_typmod(data_type) {
            Some(n) => (ScalarFn::BpcharTypmod, Some(n)),
            None => return Ok(expr),
        },
        PgType::Bit => match length_typmod(data_type) {
            Some(n) => (ScalarFn::BitTypmod, Some(n)),
            None => return Ok(expr),
        },
        PgType::Varbit => match length_typmod(data_type) {
            Some(n) => (ScalarFn::VarbitTypmod, Some(n)),
            None => return Ok(expr),
        },
        // `name` always truncates to 63 characters, independent of any modifier.
        PgType::Name => (ScalarFn::NameInput, None),
        _ => return Ok(expr),
    };
    // Fold a constant value now (explicit-cast semantics: truncate/pad).
    if let BoundExpr::Const { value: Value::Text(s), .. } = &expr {
        let folded = match func {
            ScalarFn::VarcharTypmod => crabgresql_types::text::truncate_chars(s, typmod.unwrap()),
            ScalarFn::BpcharTypmod => crabgresql_types::text::bpchar_input(s, typmod.unwrap(), true)
                .map_err(|e| BindError::new(e.sqlstate, e.message))?,
            ScalarFn::NameInput => crabgresql_types::text::name_input(s),
            _ => unreachable!(),
        };
        return Ok(BoundExpr::Const { value: Value::Text(folded), ty: target });
    }
    if let BoundExpr::Const { value: Value::Bit { len, data }, .. } = &expr {
        let (len, data) =
            crabgresql_types::bit::coerce(*len, data, typmod.unwrap(), target == PgType::Varbit, true)
                .map_err(|e| BindError::new(e.sqlstate, e.message))?;
        return Ok(BoundExpr::Const { value: Value::Bit { len, data }, ty: target });
    }
    let mut args = vec![expr];
    if let Some(n) = typmod {
        args.push(BoundExpr::Const { value: Value::Int4(n), ty: PgType::Int4 });
    }
    Ok(BoundExpr::FuncCall { func, ret: target, args })
}

/// `interval '...'` (with an optional SQL-standard field qualifier). The
/// literal string is parsed by `interval_in`; a leading field (`INTERVAL '1'
/// DAY`) sets the default unit for a bare number, and any precision is ignored.
fn bind_interval(node: &ast::Interval) -> Result<Binding, BindError> {
    let (s, span) = match &*node.value {
        ast::Expr::Value(v) => match &v.value {
            ast::Value::SingleQuotedString(s) => (s.clone(), v.span),
            other => {
                return Err(BindError::syntax(format!("invalid interval literal: {other}")));
            }
        },
        other => return Err(unsupported_expr(other)),
    };
    let default = node
        .leading_field
        .as_ref()
        .map(datetime_field_to_unit)
        .unwrap_or(interval::Unit::Second);
    let iv = interval::parse_with_default(&s, default)
        .map_err(|e| BindError::new(e.sqlstate, e.message).at(span))?;
    Ok(Binding::Typed(BoundExpr::Const {
        value: Value::Interval(iv),
        ty: PgType::Interval,
    }))
}

/// Map a SQL-standard interval leading field to the default unit for a bare
/// number; anything unusual falls back to seconds (PG's default).
fn datetime_field_to_unit(field: &ast::DateTimeField) -> interval::Unit {
    use ast::DateTimeField::*;
    match field {
        Year | Years => interval::Unit::Year,
        Month | Months => interval::Unit::Month,
        Week(_) | Weeks => interval::Unit::Week,
        Day | Days => interval::Unit::Day,
        Hour | Hours => interval::Unit::Hour,
        Minute | Minutes => interval::Unit::Minute,
        _ => interval::Unit::Second,
    }
}

/// `EXTRACT(field FROM ts)`: PG's `date_part`-family sugar that returns
/// `numeric`. We support it on `timestamp`; the field name is carried as a text
/// constant argument and validated at run time (unknown units error there,
/// matching `date_part`).
fn bind_extract(
    field: &ast::DateTimeField,
    expr: &ast::Expr,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let unit = datetime_field_unit(field);
    // The operand type selects the overload; interval, timestamp, and
    // timestamptz each have their own extract. An untyped literal defaults to
    // `timestamp`, matching PG.
    let (func, arg) = match bind_expr(expr, scope)? {
        Binding::Typed(e) if e.ty() == PgType::Timestamp => (ScalarFn::Extract, e),
        Binding::Typed(e) if e.ty() == PgType::Interval => (ScalarFn::ExtractInterval, e),
        Binding::Typed(e) if e.ty() == PgType::TimestampTz => (ScalarFn::ExtractTz, e),
        Binding::Typed(e) if e.ty() == PgType::Date => (ScalarFn::ExtractDate, e),
        Binding::Typed(e) if e.ty() == PgType::Time => (ScalarFn::ExtractTime, e),
        Binding::Typed(e) if e.ty() == PgType::TimeTz => (ScalarFn::ExtractTimeTz, e),
        Binding::Typed(e) => {
            return Err(BindError::new(
                sqlstate::UNDEFINED_FUNCTION,
                format!(
                    "function pg_catalog.date_part(unknown, {}) does not exist",
                    e.ty().name()
                ),
            ));
        }
        Binding::Unknown { lit, span } => {
            (ScalarFn::Extract, resolve_unknown(lit, span, PgType::Timestamp)?)
        }
    };
    Ok(Binding::Typed(BoundExpr::FuncCall {
        func,
        ret: PgType::Numeric,
        args: vec![
            BoundExpr::Const {
                value: Value::Text(unit),
                ty: PgType::Text,
            },
            arg,
        ],
    }))
}

/// `<value> AT TIME ZONE <zone>`. The overload is chosen by the value's type: a
/// zone-less `timestamp` wall clock interpreted in `zone` yields a `timestamptz`
/// (UTC) instant; a `timestamptz` instant shown in `zone` yields a zone-less
/// `timestamp`. Lowers to the `timezone(zone_text, value)` function form (PG's
/// implementation of the syntax); the result column is named `timezone`.
fn bind_at_time_zone(
    value: &ast::Expr,
    zone: &ast::Expr,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let zone_arg = match bind_expr(zone, scope)? {
        Binding::Unknown { lit, span } => resolve_unknown(lit, span, PgType::Text)?,
        Binding::Typed(e) if e.ty() == PgType::Text => e,
        Binding::Typed(e) => {
            return Err(BindError::new(
                sqlstate::UNDEFINED_FUNCTION,
                format!(
                    "function pg_catalog.timezone({}, ...) does not exist",
                    e.ty().name()
                ),
            ));
        }
    };
    // An untyped value literal defaults to `timestamp` (→ timestamptz), as PG does.
    let (func, ret, value_arg) = match bind_expr(value, scope)? {
        Binding::Typed(e) if e.ty() == PgType::Timestamp => {
            (ScalarFn::TimezoneToTz, PgType::TimestampTz, e)
        }
        Binding::Typed(e) if e.ty() == PgType::TimestampTz => {
            (ScalarFn::TimezoneToTs, PgType::Timestamp, e)
        }
        Binding::Typed(e) => {
            return Err(BindError::new(
                sqlstate::UNDEFINED_FUNCTION,
                format!(
                    "function pg_catalog.timezone(text, {}) does not exist",
                    e.ty().name()
                ),
            ));
        }
        Binding::Unknown { lit, span } => (
            ScalarFn::TimezoneToTz,
            PgType::TimestampTz,
            resolve_unknown(lit, span, PgType::Timestamp)?,
        ),
    };
    Ok(Binding::Typed(BoundExpr::FuncCall {
        func,
        ret,
        args: vec![zone_arg, value_arg],
    }))
}

/// The canonical unit string for an EXTRACT field, lowercased. Unknown/unusual
/// spellings fall back to the parser's rendering (also lowercased), leaving the
/// run-time `date_part` to reject truly unrecognized units.
fn datetime_field_unit(field: &ast::DateTimeField) -> String {
    use ast::DateTimeField::*;
    match field {
        Year | Years => "year",
        Month | Months => "month",
        Day | Days => "day",
        Hour | Hours => "hour",
        Minute | Minutes => "minute",
        Second | Seconds => "second",
        Millisecond | Milliseconds => "milliseconds",
        Microsecond | Microseconds => "microseconds",
        Decade => "decade",
        Century => "century",
        Millennium | Millenium => "millennium",
        Quarter => "quarter",
        Week(_) | Weeks => "week",
        Dow => "dow",
        Isodow => "isodow",
        Doy => "doy",
        Epoch => "epoch",
        Isoyear => "isoyear",
        Julian => "julian",
        other => return other.to_string().to_lowercase(),
    }
    .to_string()
}

fn bind_compound(parts: &[ast::Ident], scope: &Scope) -> Result<BoundExpr, BindError> {
    let [qualifier, column] = parts else {
        return Err(BindError::feature_not_supported(
            "schema-qualified column references are not supported yet",
        ));
    };
    let qualifier = normalize_ident(qualifier);
    let rel = scope.relation(&qualifier)?;
    let column = normalize_ident(column);
    // A qualified reference is still ambiguous if the relation exposes the name
    // more than once (e.g. an alias list `v(x, x)`), matching PG's 42702.
    let mut local: Option<usize> = None;
    for (i, col) in rel.columns.iter().enumerate() {
        if col.name == column {
            if local.is_some() {
                return Err(BindError::new(
                    sqlstate::AMBIGUOUS_COLUMN,
                    format!("column reference \"{column}\" is ambiguous"),
                ));
            }
            local = Some(i);
        }
    }
    // PG names the missing column with its qualifier, unquoted: `column q.c does
    // not exist` (contrast the unqualified form `column "c" does not exist`).
    let local = local.ok_or_else(|| {
        BindError::new(
            sqlstate::UNDEFINED_COLUMN,
            format!("column {qualifier}.{column} does not exist"),
        )
    })?;
    Ok(BoundExpr::ColumnRef {
        index: rel.offset + local,
        ty: rel.columns[local].ty,
    })
}

fn no_op_unary(sym: &str, ty: &str) -> BindError {
    BindError::new(
        sqlstate::UNDEFINED_FUNCTION,
        format!("operator does not exist: {sym} {ty}"),
    )
}

fn ambiguous_unary(sym: &str) -> BindError {
    BindError::new(
        sqlstate::AMBIGUOUS_FUNCTION,
        format!("operator is not unique: {sym} unknown"),
    )
}

fn bind_unary(
    op: ast::UnaryOperator,
    operand: &ast::Expr,
    scope: &Scope,
) -> Result<Binding, BindError> {
    // PG folds `-` into the numeric literal before choosing int4 vs int8:
    // `-2147483648` must be int4, not an overflowing negation of an int8.
    if op == ast::UnaryOperator::Minus
        && let ast::Expr::Value(v) = operand
        && let ast::Value::Number(n, _) = &v.value
    {
        return parse_number(&format!("-{n}")).map(Binding::Typed);
    }
    match op {
        ast::UnaryOperator::Minus | ast::UnaryOperator::Plus => {
            let sym = if op == ast::UnaryOperator::Minus {
                "-"
            } else {
                "+"
            };
            match bind_expr(operand, scope)? {
                Binding::Typed(e) if e.ty().is_numeric() => {
                    Ok(Binding::Typed(if op == ast::UnaryOperator::Minus {
                        BoundExpr::Unary {
                            op: UnaryOp::Neg,
                            expr: Box::new(e),
                        }
                    } else {
                        // Unary + is the identity on numeric types.
                        e
                    }))
                }
                // `- interval` negates every field. PG has no unary `+ interval`
                // operator, so that falls through to the error arm below.
                Binding::Typed(e)
                    if e.ty() == PgType::Interval && op == ast::UnaryOperator::Minus =>
                {
                    Ok(Binding::Typed(BoundExpr::FuncCall {
                        func: ScalarFn::IntervalNeg,
                        ret: PgType::Interval,
                        args: vec![e],
                    }))
                }
                // `- money` (`cash_um`); PG has no unary `+ money`, so that falls
                // through to the error arm below.
                Binding::Typed(e)
                    if e.ty() == PgType::Money && op == ast::UnaryOperator::Minus =>
                {
                    Ok(Binding::Typed(BoundExpr::FuncCall {
                        func: ScalarFn::CashUm,
                        ret: PgType::Money,
                        args: vec![e],
                    }))
                }
                Binding::Typed(e) => Err(no_op_unary(sym, e.ty().name())),
                // Every numeric type has this operator, so an untyped literal
                // cannot pick one — PG reports ambiguity.
                Binding::Unknown { .. } => Err(ambiguous_unary(sym)),
            }
        }
        // `@` absolute value: keeps the operand type.
        ast::UnaryOperator::PGAbs => match bind_expr(operand, scope)? {
            Binding::Typed(e) if e.ty().is_numeric() => Ok(Binding::Typed(BoundExpr::Unary {
                op: UnaryOp::Abs,
                expr: Box::new(e),
            })),
            Binding::Typed(e) => Err(no_op_unary("@", e.ty().name())),
            Binding::Unknown { .. } => Err(ambiguous_unary("@")),
        },
        ast::UnaryOperator::PGSquareRoot => bind_prefix_float8(UnaryOp::Sqrt, "|/", operand, scope),
        ast::UnaryOperator::PGCubeRoot => bind_prefix_float8(UnaryOp::Cbrt, "||/", operand, scope),
        ast::UnaryOperator::Not => {
            let operand = to_bool_operand(bind_expr(operand, scope)?, "NOT")?;
            Ok(Binding::Typed(BoundExpr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(operand),
            }))
        }
        // `~inet` — bitwise NOT of the address (masklen preserved); `~bit` — the
        // bitwise complement of a bit string (length preserved).
        ast::UnaryOperator::BitwiseNot => match bind_expr(operand, scope)? {
            Binding::Typed(e) if matches!(e.ty(), PgType::Inet | PgType::Cidr) => {
                Ok(Binding::Typed(BoundExpr::FuncCall {
                    func: ScalarFn::InetNot,
                    ret: PgType::Inet,
                    args: vec![e],
                }))
            }
            Binding::Typed(e) if matches!(e.ty(), PgType::Bit | PgType::Varbit) => {
                // PG's `~` is defined only on `bit` (a varbit operand is cast in),
                // so the result type is `bit`.
                Ok(Binding::Typed(BoundExpr::FuncCall {
                    func: ScalarFn::BitNot,
                    ret: PgType::Bit,
                    args: vec![e],
                }))
            }
            // `~macaddr` / `~macaddr8` — one's complement, same type back.
            Binding::Typed(e) if matches!(e.ty(), PgType::Macaddr | PgType::Macaddr8) => {
                let ret = e.ty();
                Ok(Binding::Typed(BoundExpr::FuncCall {
                    func: ScalarFn::MacaddrNot,
                    ret,
                    args: vec![e],
                }))
            }
            Binding::Typed(e) => Err(no_op_unary("~", e.ty().name())),
            Binding::Unknown { .. } => Err(ambiguous_unary("~")),
        },
        // Unary geometric operators over `lseg`: `@-@` length, `@@` center,
        // `?-` horizontal, `?|` vertical. (Other geometric operands are added
        // with their types.)
        ast::UnaryOperator::AtDashAt
        | ast::UnaryOperator::DoubleAt
        | ast::UnaryOperator::QuestionDash
        | ast::UnaryOperator::QuestionPipe => {
            resolve_geometric_unary(op, bind_expr(operand, scope)?)
        }
        other => Err(BindError::feature_not_supported(format!(
            "operator is not supported yet: {other}"
        ))),
    }
}

/// Unary geometric operators (`@-@`, `@@`, `?-`, `?|`) over `lseg`. Returns the
/// "operator does not exist" error for a non-geometric or untyped operand.
fn resolve_geometric_unary(op: ast::UnaryOperator, operand: Binding) -> Result<Binding, BindError> {
    use crate::functions::GeoFn;
    let sym = op.to_string();
    let e = match operand {
        Binding::Typed(e) if e.ty() == PgType::Lseg => e,
        Binding::Typed(e) => return Err(no_op_unary(&sym, e.ty().name())),
        Binding::Unknown { .. } => return Err(ambiguous_unary(&sym)),
    };
    let (func, ret) = match op {
        ast::UnaryOperator::AtDashAt => (GeoFn::LsegLength, PgType::Float8),
        ast::UnaryOperator::DoubleAt => (GeoFn::LsegCenter, PgType::Point),
        ast::UnaryOperator::QuestionDash => (GeoFn::LsegHoriz, PgType::Bool),
        ast::UnaryOperator::QuestionPipe => (GeoFn::LsegVert, PgType::Bool),
        _ => unreachable!("resolve_geometric_unary only handles the geometric unary operators"),
    };
    Ok(Binding::Typed(BoundExpr::FuncCall {
        func: ScalarFn::Geo(func),
        ret,
        args: vec![e],
    }))
}

/// `|/` / `||/`: coerce the operand to float8 (unknown → float8), producing a
/// float8 result.
fn bind_prefix_float8(
    uop: UnaryOp,
    sym: &str,
    operand: &ast::Expr,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let expr = match bind_expr(operand, scope)? {
        Binding::Typed(e) if e.ty().is_numeric() => coerce_expr(e, PgType::Float8)?,
        Binding::Typed(e) => return Err(no_op_unary(sym, e.ty().name())),
        Binding::Unknown { lit, span } => resolve_unknown(lit, span, PgType::Float8)?,
    };
    Ok(Binding::Typed(BoundExpr::Unary {
        op: uop,
        expr: Box::new(expr),
    }))
}

fn bind_is_null(inner: &ast::Expr, scope: &Scope, negated: bool) -> Result<Binding, BindError> {
    let expr = match bind_expr(inner, scope)? {
        Binding::Typed(e) => e,
        Binding::Unknown { lit, span } => resolve_unknown(lit, span, PgType::Text)?,
    };
    Ok(Binding::Typed(BoundExpr::IsNull {
        expr: Box::new(expr),
        negated,
    }))
}

/// `CASE`: both the searched form (`CASE WHEN cond THEN r ...`) and the simple
/// form (`CASE operand WHEN v THEN r ...`, sugar for `CASE WHEN operand = v`).
/// Conditions are forced to boolean; all `THEN`/`ELSE` results resolve to one
/// common type the same way a `VALUES`/`UNION` column does.
fn bind_case(
    operand: Option<&ast::Expr>,
    conditions: &[ast::CaseWhen],
    else_result: Option<&ast::Expr>,
    scope: &Scope,
) -> Result<Binding, BindError> {
    // Simple CASE evaluates the operand once at run time; we re-bind a clone of
    // it per WHEN to build `operand = value`. That is equivalent because our
    // scalar expressions are pure (no volatile functions reach here yet).
    //
    // PG gives an untyped-literal operand its own type before comparing — an
    // unknown resolves to text (its default), independent of the WHEN values —
    // so `CASE NULL WHEN 1` is `text = integer` (operator does not exist), not
    // an attempt to read the operand as integer. Resolve it here to reproduce
    // that; a typed operand is left as-is.
    let operand = match operand {
        None => None,
        Some(e) => Some(match bind_expr(e, scope)? {
            Binding::Unknown { lit, span } => {
                Binding::Typed(resolve_unknown(lit, span, PgType::Text)?)
            }
            typed => typed,
        }),
    };

    // Bind everything in source order (operand, then each WHEN's condition and
    // result, then ELSE) so bind-time errors surface where PG's do.
    let mut conds = Vec::with_capacity(conditions.len());
    let mut then_bindings = Vec::with_capacity(conditions.len());
    for when in conditions {
        let cond = match &operand {
            None => to_bool_operand(bind_expr(&when.condition, scope)?, "CASE/WHEN")?,
            Some(op) => {
                let value = bind_expr(&when.condition, scope)?;
                match bind_binary_op(BinOp::Eq, op.clone(), value)? {
                    Binding::Typed(e) => e,
                    // `=` always resolves to a typed boolean expression.
                    Binding::Unknown { .. } => unreachable!("= yields a typed bool"),
                }
            }
        };
        conds.push(cond);
        then_bindings.push(bind_expr(&when.result, scope)?);
    }
    // A missing ELSE is NULL, which is compatible with any type and needs no
    // coercion node.
    let else_binding = else_result.map(|e| bind_expr(e, scope)).transpose()?;
    let has_else = else_binding.is_some();

    // Result-type unification lists the ELSE result first, then the WHEN
    // results, matching the operand order PG uses for its "CASE types A and B
    // cannot be matched" message.
    let mut result_bindings = Vec::with_capacity(then_bindings.len() + 1);
    result_bindings.extend(else_binding);
    result_bindings.extend(then_bindings);
    let (ty, mut results) = unify_value_column(result_bindings, "CASE")?;

    let else_ = if has_else {
        Some(Box::new(results.remove(0)))
    } else {
        None
    };
    let whens = conds.into_iter().zip(results).collect();

    Ok(Binding::Typed(BoundExpr::Case { whens, else_, ty }))
}

/// `x IN (a, b, c)` desugars to `x = a OR x = b OR x = c`; `x NOT IN (...)` to
/// `x <> a AND x <> b AND x <> c`. Both reproduce PG's three-valued logic (a
/// NULL element yields NULL, not false — the executor's Kleene `OR`/`AND`) and
/// per-element type resolution: each comparison is bound through the shared
/// `bind_binary_op`, so the left operand's unknown-literal typing, numeric
/// promotion, and `operator does not exist` / `invalid input syntax` errors all
/// match a written `x = a`. The left `Binding` is left unresolved and cloned per
/// element (like a simple `CASE operand`), so `'5' IN (5, 6)` types `'5'` from
/// the list as int4 rather than defaulting it to text.
fn bind_in_list(
    expr: &ast::Expr,
    list: &[ast::Expr],
    negated: bool,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let left = bind_expr(expr, scope)?;
    let (cmp, chain) = if negated {
        (BinOp::NotEq, BinOp::And)
    } else {
        (BinOp::Eq, BinOp::Or)
    };
    let mut acc: Option<Binding> = None;
    for item in list {
        let rb = bind_expr(item, scope)?;
        let comparison = bind_binary_op(cmp, left.clone(), rb)?;
        acc = Some(match acc {
            None => comparison,
            Some(prev) => bind_binary_op(chain, prev, comparison)?,
        });
    }
    // An empty list is a parser syntax error (`IN ()`), so this is unreachable;
    // fold to the constant PG's `= ANY '{}'` yields rather than panic.
    Ok(acc.unwrap_or_else(|| {
        Binding::Typed(BoundExpr::Const {
            value: Value::Bool(negated),
            ty: PgType::Bool,
        })
    }))
}

fn bind_binary(
    left: &ast::Expr,
    op: &ast::BinaryOperator,
    right: &ast::Expr,
    scope: &Scope,
) -> Result<Binding, BindError> {
    // `||` is not a `BinOp`; PG's `textcat`/`anytextcat` lower to a text concat,
    // and `bitcat` to a bit-string concat when either side is a bit string.
    if matches!(op, ast::BinaryOperator::StringConcat) {
        let lb = bind_expr(left, scope)?;
        let rb = bind_expr(right, scope)?;
        // Route to bit concatenation only when neither side is a concrete
        // non-bit type — i.e. both operands are bit strings or untyped literals,
        // and at least one is a bit string. `bit || text` instead falls to the
        // text concat (PG's `anytextcat`), rendering the bit as its 0/1 string.
        let bit_or_unknown = |b: &Binding| {
            is_bit_family(binding_typed_ty(b)) || matches!(b, Binding::Unknown { .. })
        };
        if (is_bit_family(binding_typed_ty(&lb)) || is_bit_family(binding_typed_ty(&rb)))
            && bit_or_unknown(&lb)
            && bit_or_unknown(&rb)
        {
            return bind_bit_concat(lb, rb);
        }
        return bind_string_concat(lb, rb);
    }
    // The `~~`/`~~*`/`!~~`/`!~~*` operator spellings of LIKE / ILIKE.
    if let Some((ci, negated)) = match op {
        ast::BinaryOperator::PGLikeMatch => Some((false, false)),
        ast::BinaryOperator::PGILikeMatch => Some((true, false)),
        ast::BinaryOperator::PGNotLikeMatch => Some((false, true)),
        ast::BinaryOperator::PGNotILikeMatch => Some((true, true)),
        _ => None,
    } {
        let lb = bind_expr(left, scope)?;
        let rb = bind_expr(right, scope)?;
        return bind_like(lb, rb, None, ci, negated);
    }

    let lb = bind_expr(left, scope)?;
    let rb = bind_expr(right, scope)?;

    // inet/cidr operators (containment, overlap, bitwise, host arithmetic) don't
    // fit the single-`arg_ty` `Binary` node; they lower to `ScalarFn` calls.
    // Tried before the generic mapping so `<<`/`>>`/`&`/`|`/`&&` reach here.
    if let Some(binding) = resolve_network_op(op, &lb, &rb)? {
        return Ok(binding);
    }

    // bit/varbit bitwise (`& | #`) and shift (`<< >>`) operators also lower to
    // `ScalarFn` calls. Tried after the network path so an inet operand still
    // wins; falls through so integer `&`/`|`/`<<` keep their error.
    if let Some(binding) = resolve_bit_op(op, &lb, &rb)? {
        return Ok(binding);
    }

    // `macaddr`/`macaddr8` bitwise `&`/`|` — like the inet operators, they don't
    // fit the single-`arg_ty` `Binary` node and lower to `ScalarFn` calls.
    if let Some(binding) = resolve_macaddr_op(op, &lb, &rb)? {
        return Ok(binding);
    }

    // Geometric operators (`point`/`lseg` distance, containment, arithmetic,
    // comparisons) don't fit the single-`arg_ty` `Binary` node either; they
    // lower to `ScalarFn::Geo` calls. Tried before the generic mapping so
    // `<<`/`>>`/`=`/`<` etc. reach here when a geometric operand is present.
    if let Some(binding) = resolve_geometric_op(op, &lb, &rb)? {
        return Ok(binding);
    }

    let op = match op {
        ast::BinaryOperator::Eq => BinOp::Eq,
        ast::BinaryOperator::NotEq => BinOp::NotEq,
        ast::BinaryOperator::Lt => BinOp::Lt,
        ast::BinaryOperator::LtEq => BinOp::LtEq,
        ast::BinaryOperator::Gt => BinOp::Gt,
        ast::BinaryOperator::GtEq => BinOp::GtEq,
        ast::BinaryOperator::And => BinOp::And,
        ast::BinaryOperator::Or => BinOp::Or,
        ast::BinaryOperator::Plus => BinOp::Add,
        ast::BinaryOperator::Minus => BinOp::Sub,
        ast::BinaryOperator::Multiply => BinOp::Mul,
        ast::BinaryOperator::Divide => BinOp::Div,
        ast::BinaryOperator::Modulo => BinOp::Mod,
        ast::BinaryOperator::PGExp => BinOp::Pow,
        other => {
            return Err(BindError::feature_not_supported(format!(
                "operator is not supported yet: {other}"
            )));
        }
    };
    bind_binary_op(op, lb, rb)
}

/// Resolve a binary operator over two already-bound operands. Split out from
/// `bind_binary` so a simple `CASE operand WHEN v` can reuse the exact `=`
/// resolution (unknown-literal handling, numeric promotion, "operator does not
/// exist" errors) that a written `operand = v` gets.
fn bind_binary_op(op: BinOp, lb: Binding, rb: Binding) -> Result<Binding, BindError> {
    if op.is_logic() {
        let left = to_bool_operand(lb, op.sql_symbol())?;
        let right = to_bool_operand(rb, op.sql_symbol())?;
        return Ok(Binding::Typed(BoundExpr::Binary {
            op,
            arg_ty: PgType::Bool,
            left: Box::new(left),
            right: Box::new(right),
        }));
    }

    // `^` has only a float8 operator here: coerce both sides to float8.
    if op == BinOp::Pow {
        return bind_pow(lb, rb);
    }

    // Mixed-type temporal arithmetic (`ts - ts`, `ts ± interval`, `interval ±
    // interval`, `interval * / number`) doesn't fit the single-`arg_ty` `Binary`
    // node, so it lowers to a function call. Comparisons and same-type cases fall
    // through to the generic path below.
    if let Some(binding) = resolve_temporal(op, &lb, &rb)? {
        return Ok(binding);
    }

    // Money arithmetic (money ± money, money * / int/float, money / money) is
    // not on the generic numeric path (money isn't `is_numeric`), so it lowers
    // to `ScalarFn` calls here. Comparisons fall through to the generic path.
    if let Some(binding) = resolve_money_op(op, &lb, &rb)? {
        return Ok(binding);
    }

    // Comparison or arithmetic: settle both operands on one type. For
    // arithmetic, the typed side must offer the operator BEFORE the unknown
    // side is parsed as that type — PG reports `operator does not exist:
    // boolean + unknown`, never a coercion failure, when no operator applies.
    let (left, right, arg_ty) = match (lb, rb) {
        (Binding::Typed(l), Binding::Typed(r)) => unify_types(l, r, op)?,
        (Binding::Typed(l), Binding::Unknown { lit, span }) => {
            let ty = l.ty();
            if op.is_arithmetic() && !ty.is_numeric() {
                return Err(no_operator(ty.name(), op, "unknown"));
            }
            (l, resolve_unknown(lit, span, ty)?, ty)
        }
        (Binding::Unknown { lit, span }, Binding::Typed(r)) => {
            let ty = r.ty();
            if op.is_arithmetic() && !ty.is_numeric() {
                return Err(no_operator("unknown", op, ty.name()));
            }
            (resolve_unknown(lit, span, ty)?, r, ty)
        }
        (
            Binding::Unknown {
                lit: ll,
                span: ls,
            },
            Binding::Unknown {
                lit: rl,
                span: rs,
            },
        ) => {
            if op.is_arithmetic() {
                // Every numeric type offers the operator; unknown operands
                // cannot pick one — PG reports ambiguity.
                return Err(BindError::new(
                    sqlstate::AMBIGUOUS_FUNCTION,
                    format!(
                        "operator is not unique: unknown {} unknown",
                        op.sql_symbol()
                    ),
                ));
            }
            // Comparing two untyped literals: PG falls back to text.
            (
                resolve_unknown(ll, ls, PgType::Text)?,
                resolve_unknown(rl, rs, PgType::Text)?,
                PgType::Text,
            )
        }
    };

    // Admit only operators the executor actually implements for `arg_ty`, so a
    // bind never produces a node the evaluator can't handle. PG resolves against
    // a concrete operator catalog; this whitelist is our stand-in. `%` exists
    // for the integer types and `numeric` (PG has `numeric_mod`), but not float.
    let supported = if op.is_arithmetic() {
        let numeric_arith = matches!(
            arg_ty,
            PgType::Int2
                | PgType::Int4
                | PgType::Int8
                | PgType::Float4
                | PgType::Float8
                | PgType::Numeric
        );
        let mod_ok = op != BinOp::Mod
            || matches!(arg_ty, PgType::Int2 | PgType::Int4 | PgType::Int8 | PgType::Numeric);
        numeric_arith && mod_ok
    } else {
        is_orderable(arg_ty)
    };
    if !supported {
        return Err(no_operator(arg_ty.name(), op, arg_ty.name()));
    }

    Ok(Binding::Typed(BoundExpr::Binary {
        op,
        arg_ty,
        left: Box::new(left),
        right: Box::new(right),
    }))
}

/// Resolve mixed-type temporal arithmetic to a function call, or `Ok(None)` to
/// let the generic (same-type / comparison) path handle it — including the
/// `operator does not exist` error for combinations with no operator (e.g.
/// `interval * interval`). An untyped literal opposite a temporal operand takes
/// the partner type: interval for `±`, float8 for the `* /` factor.
fn resolve_temporal(op: BinOp, lb: &Binding, rb: &Binding) -> Result<Option<Binding>, BindError> {
    if !matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div) {
        return Ok(None);
    }
    let lt = binding_typed_ty(lb);
    let rt = binding_typed_ty(rb);
    let is_temporal = |t: Option<PgType>| {
        matches!(
            t,
            Some(
                PgType::Interval
                    | PgType::Timestamp
                    | PgType::Date
                    | PgType::Time
                    | PgType::TimeTz
            )
        )
    };
    if !is_temporal(lt) && !is_temporal(rt) {
        return Ok(None);
    }

    use PgType::{Date as D, Interval as I, Time as TI, TimeTz as TZ, Timestamp as T};
    // Only int2/int4 pair with `date` (PG has `date + int4`; int2 widens to it).
    // int8 has no `date + bigint` operator, so it must fall through to an error.
    let is_int = |t: Option<PgType>| matches!(t, Some(PgType::Int2 | PgType::Int4));
    let typed = |b: &Binding| match b {
        Binding::Typed(e) => e.clone(),
        Binding::Unknown { .. } => unreachable!("typed side is Typed"),
    };
    let call = |func, ret, a: BoundExpr, b: BoundExpr| {
        Ok(Some(Binding::Typed(BoundExpr::FuncCall {
            func,
            ret,
            args: vec![a, b],
        })))
    };
    // A numeric operand (or an untyped literal) can be the `* /` factor.
    let factor_ok = |t: Option<PgType>| matches!(t, Some(ty) if ty.is_numeric()) || t.is_none();

    match op {
        BinOp::Add => match (lt, rt) {
            (Some(I), Some(I)) => call(ScalarFn::IntervalPl, I, typed(lb), typed(rb)),
            (Some(T), Some(I)) => call(ScalarFn::TimestampPlInterval, T, typed(lb), typed(rb)),
            (Some(I), Some(T)) => call(ScalarFn::TimestampPlInterval, T, typed(rb), typed(lb)),
            (Some(I), None) => call(ScalarFn::IntervalPl, I, typed(lb), resolve_operand(rb, I)?),
            (None, Some(I)) => call(ScalarFn::IntervalPl, I, resolve_operand(lb, I)?, typed(rb)),
            (Some(T), None) => call(ScalarFn::TimestampPlInterval, T, typed(lb), resolve_operand(rb, I)?),
            (None, Some(T)) => call(ScalarFn::TimestampPlInterval, T, typed(rb), resolve_operand(lb, I)?),
            // date + int -> date; date + interval -> timestamp; date + time -> timestamp.
            (Some(D), _) if is_int(rt) => {
                call(ScalarFn::DatePlDays, D, typed(lb), resolve_operand(rb, PgType::Int4)?)
            }
            (_, Some(D)) if is_int(lt) => {
                call(ScalarFn::DatePlDays, D, typed(rb), resolve_operand(lb, PgType::Int4)?)
            }
            (Some(D), Some(I)) => call(ScalarFn::DatePlInterval, T, typed(lb), typed(rb)),
            (Some(I), Some(D)) => call(ScalarFn::DatePlInterval, T, typed(rb), typed(lb)),
            (Some(D), Some(TI)) => call(ScalarFn::DatePlTime, T, typed(lb), typed(rb)),
            (Some(TI), Some(D)) => call(ScalarFn::DatePlTime, T, typed(rb), typed(lb)),
            // date + timetz -> timestamptz.
            (Some(D), Some(TZ)) => {
                call(ScalarFn::DatePlTimeTz, PgType::TimestampTz, typed(lb), typed(rb))
            }
            (Some(TZ), Some(D)) => {
                call(ScalarFn::DatePlTimeTz, PgType::TimestampTz, typed(rb), typed(lb))
            }
            // time + interval -> time; timetz + interval -> timetz.
            (Some(TI), Some(I)) => call(ScalarFn::TimePlInterval, TI, typed(lb), typed(rb)),
            (Some(I), Some(TI)) => call(ScalarFn::TimePlInterval, TI, typed(rb), typed(lb)),
            (Some(TZ), Some(I)) => call(ScalarFn::TimeTzPlInterval, TZ, typed(lb), typed(rb)),
            (Some(I), Some(TZ)) => call(ScalarFn::TimeTzPlInterval, TZ, typed(rb), typed(lb)),
            _ => Ok(None),
        },
        BinOp::Sub => match (lt, rt) {
            (Some(I), Some(I)) => call(ScalarFn::IntervalMi, I, typed(lb), typed(rb)),
            (Some(T), Some(I)) => call(ScalarFn::TimestampMiInterval, T, typed(lb), typed(rb)),
            (Some(T), Some(T)) => call(ScalarFn::TimestampMi, I, typed(lb), typed(rb)),
            (Some(I), None) => call(ScalarFn::IntervalMi, I, typed(lb), resolve_operand(rb, I)?),
            (None, Some(I)) => call(ScalarFn::IntervalMi, I, resolve_operand(lb, I)?, typed(rb)),
            // For `timestamp - unknown`, PG resolves the literal to `timestamp`
            // (the preferred type), yielding timestamp - timestamp -> interval —
            // so `ts - '1 day'` errors as an invalid timestamp, matching PG,
            // while `ts - '<date>'` and `<date> - ts` produce an interval.
            (Some(T), None) => call(ScalarFn::TimestampMi, I, typed(lb), resolve_operand(rb, T)?),
            (None, Some(T)) => call(ScalarFn::TimestampMi, I, resolve_operand(lb, T)?, typed(rb)),
            // date - date -> int4; date - int -> date; date - interval -> timestamp.
            (Some(D), Some(D)) => call(ScalarFn::DateMi, PgType::Int4, typed(lb), typed(rb)),
            (Some(D), _) if is_int(rt) => {
                call(ScalarFn::DateMiDays, D, typed(lb), resolve_operand(rb, PgType::Int4)?)
            }
            (Some(D), Some(I)) => call(ScalarFn::DateMiInterval, T, typed(lb), typed(rb)),
            // date - timestamp / timestamp - date -> interval: widen the date to
            // a timestamp (midnight) and take the timestamp difference, as PG's
            // implicit date->timestamp cast does.
            (Some(D), Some(T)) => {
                call(ScalarFn::TimestampMi, I, resolve_operand(lb, T)?, typed(rb))
            }
            (Some(T), Some(D)) => {
                call(ScalarFn::TimestampMi, I, typed(lb), resolve_operand(rb, T)?)
            }
            (Some(D), None) => call(ScalarFn::DateMi, PgType::Int4, typed(lb), resolve_operand(rb, D)?),
            (None, Some(D)) => call(ScalarFn::DateMi, PgType::Int4, resolve_operand(lb, D)?, typed(rb)),
            // time - time -> interval; time - interval -> time; timetz - interval -> timetz.
            (Some(TI), Some(TI)) => call(ScalarFn::TimeMi, I, typed(lb), typed(rb)),
            (Some(TI), Some(I)) => call(ScalarFn::TimeMiInterval, TI, typed(lb), typed(rb)),
            (Some(TZ), Some(I)) => call(ScalarFn::TimeTzMiInterval, TZ, typed(lb), typed(rb)),
            _ => Ok(None),
        },
        BinOp::Mul => match (lt, rt) {
            (Some(I), _) if factor_ok(rt) => {
                call(ScalarFn::IntervalMul, I, typed(lb), resolve_operand(rb, PgType::Float8)?)
            }
            (_, Some(I)) if factor_ok(lt) => {
                call(ScalarFn::IntervalMul, I, typed(rb), resolve_operand(lb, PgType::Float8)?)
            }
            _ => Ok(None),
        },
        BinOp::Div => match (lt, rt) {
            (Some(I), _) if factor_ok(rt) => {
                call(ScalarFn::IntervalDiv, I, typed(lb), resolve_operand(rb, PgType::Float8)?)
            }
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

fn binding_typed_ty(b: &Binding) -> Option<PgType> {
    match b {
        Binding::Typed(e) => Some(e.ty()),
        Binding::Unknown { .. } => None,
    }
}

/// Money arithmetic. `money` is deliberately not `is_numeric`, so it never
/// reaches the generic numeric path; its operators lower to `ScalarFn` calls
/// here, as `resolve_temporal`/`resolve_network_op` do for their types:
/// `money ± money -> money`; `money * intN` / `intN * money` / `money * floatN`
/// / `floatN * money -> money`; `money / intN -> money`; `money / floatN ->
/// money`; `money / money -> float8`. Returns `Ok(None)` when neither side is
/// money or the op/operand pair has no money operator, so the generic path (and
/// its comparisons and "operator does not exist" error) still applies. Every
/// call puts the money operand first and the factor/divisor second.
fn resolve_money_op(op: BinOp, lb: &Binding, rb: &Binding) -> Result<Option<Binding>, BindError> {
    use PgType::Money as M;
    let lt = binding_typed_ty(lb);
    let rt = binding_typed_ty(rb);
    if lt != Some(M) && rt != Some(M) {
        return Ok(None);
    }
    let is_int = |t: Option<PgType>| matches!(t, Some(PgType::Int2 | PgType::Int4 | PgType::Int8));
    let is_flt = |t: Option<PgType>| matches!(t, Some(PgType::Float4 | PgType::Float8));
    let typed = |b: &Binding| match b {
        Binding::Typed(e) => e.clone(),
        Binding::Unknown { .. } => unreachable!("typed side is Typed"),
    };
    let call = |func, ret, a: BoundExpr, b: BoundExpr| {
        Ok(Some(Binding::Typed(BoundExpr::FuncCall { func, ret, args: vec![a, b] })))
    };
    match op {
        // money ± money; an untyped literal opposite money is parsed as money.
        // money ± int/float has no operator in PG — fall through to the error.
        BinOp::Add | BinOp::Sub => {
            let func = if op == BinOp::Add { ScalarFn::CashPl } else { ScalarFn::CashMi };
            match (lt, rt) {
                (Some(M), Some(M)) => call(func, M, typed(lb), typed(rb)),
                (Some(M), None) => call(func, M, typed(lb), resolve_operand(rb, M)?),
                (None, Some(M)) => call(func, M, resolve_operand(lb, M)?, typed(rb)),
                _ => Ok(None),
            }
        }
        BinOp::Mul => match (lt, rt) {
            (Some(M), _) if is_int(rt) => {
                call(ScalarFn::CashMulInt, M, typed(lb), resolve_operand(rb, PgType::Int8)?)
            }
            (_, Some(M)) if is_int(lt) => {
                call(ScalarFn::CashMulInt, M, typed(rb), resolve_operand(lb, PgType::Int8)?)
            }
            (Some(M), _) if is_flt(rt) => {
                call(ScalarFn::CashMulFlt, M, typed(lb), resolve_operand(rb, PgType::Float8)?)
            }
            (_, Some(M)) if is_flt(lt) => {
                call(ScalarFn::CashMulFlt, M, typed(rb), resolve_operand(lb, PgType::Float8)?)
            }
            _ => Ok(None),
        },
        BinOp::Div => match (lt, rt) {
            (Some(M), Some(M)) => call(ScalarFn::CashDivCash, PgType::Float8, typed(lb), typed(rb)),
            (Some(M), _) if is_int(rt) => {
                call(ScalarFn::CashDivInt, M, typed(lb), resolve_operand(rb, PgType::Int8)?)
            }
            (Some(M), _) if is_flt(rt) => {
                call(ScalarFn::CashDivFlt, M, typed(lb), resolve_operand(rb, PgType::Float8)?)
            }
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

fn is_net_ty(t: Option<PgType>) -> bool {
    matches!(t, Some(PgType::Inet | PgType::Cidr))
}

/// The type name PG shows for an operand in an "operator does not exist"
/// message; an untyped literal is `unknown`.
fn operand_name(b: &Binding) -> &'static str {
    binding_typed_ty(b).map_or("unknown", |t| t.name())
}

/// `operator does not exist: <lname> <op> <rname>`, with the real operand names
/// in their actual order.
fn net_no_operator(lb: &Binding, op: &ast::BinaryOperator, rb: &Binding) -> BindError {
    BindError::new(
        sqlstate::UNDEFINED_FUNCTION,
        format!("operator does not exist: {} {op} {}", operand_name(lb), operand_name(rb)),
    )
}

/// Materialize a network operand: a typed inet/cidr as is (both read through
/// `inet_of`), an untyped literal parsed as `inet`. `None` for a typed non-net
/// operand, so the caller can report the full "operator does not exist" error.
fn net_operand(b: &Binding) -> Option<Result<BoundExpr, BindError>> {
    match b {
        Binding::Typed(e) if is_net_ty(Some(e.ty())) => Some(Ok(e.clone())),
        Binding::Unknown { lit, span } => Some(resolve_unknown(lit.clone(), *span, PgType::Inet)),
        Binding::Typed(_) => None,
    }
}

/// Materialize the integer side of inet host arithmetic: a typed int2/int4/int8
/// coerced to int8, or an untyped literal parsed as int8. `None` for any other
/// typed operand — PG has only `inet ± bigint` (narrower ints widen), so e.g.
/// `inet + numeric`/`inet + text` must report "operator does not exist" rather
/// than silently coercing/truncating.
fn int_operand(b: &Binding) -> Option<Result<BoundExpr, BindError>> {
    match b {
        Binding::Typed(e) if matches!(e.ty(), PgType::Int2 | PgType::Int4 | PgType::Int8) => {
            Some(resolve_operand(b, PgType::Int8))
        }
        Binding::Unknown { .. } => Some(resolve_operand(b, PgType::Int8)),
        Binding::Typed(_) => None,
    }
}

/// inet/cidr-specific operators lower to `ScalarFn` calls (as `resolve_temporal`
/// does for the temporal operators). Returns `Ok(None)` when the operator and
/// operands are not a network operation, so the generic operator path — and its
/// errors — still applies.
fn resolve_network_op(
    op: &ast::BinaryOperator,
    lb: &Binding,
    rb: &Binding,
) -> Result<Option<Binding>, BindError> {
    use ast::BinaryOperator as B;
    let lt = binding_typed_ty(lb);
    let rt = binding_typed_ty(rb);
    let any_net = is_net_ty(lt) || is_net_ty(rt);
    let call = |func, ret, a, b| {
        Ok(Some(Binding::Typed(BoundExpr::FuncCall {
            func,
            ret,
            args: vec![a, b],
        })))
    };

    // Containment / overlap (`<<` `>>` `&&`) and bitwise (`&` `|`) take two
    // inet-family operands (result bool / inet). Without any net operand, fall
    // through so integer `&`/`|`/`<<` still error as before.
    let net_net = match op {
        B::PGBitwiseShiftLeft => Some((ScalarFn::NetworkContainedBy, PgType::Bool)),
        B::PGBitwiseShiftRight => Some((ScalarFn::NetworkContains, PgType::Bool)),
        B::PGOverlap => Some((ScalarFn::NetworkOverlaps, PgType::Bool)),
        B::BitwiseAnd => Some((ScalarFn::InetAnd, PgType::Inet)),
        B::BitwiseOr => Some((ScalarFn::InetOr, PgType::Inet)),
        _ => None,
    };
    if let Some((func, ret)) = net_net {
        if !any_net {
            return Ok(None);
        }
        let (Some(a), Some(b)) = (net_operand(lb), net_operand(rb)) else {
            return Err(net_no_operator(lb, op, rb));
        };
        return call(func, ret, a?, b?);
    }

    // Host arithmetic: `inet ± int8` (commutative for `+`), `inet - inet`.
    match op {
        B::Plus if is_net_ty(lt) && !is_net_ty(rt) => {
            let (Some(a), Some(n)) = (net_operand(lb), int_operand(rb)) else {
                return Err(net_no_operator(lb, op, rb));
            };
            call(ScalarFn::InetPlInt8, PgType::Inet, a?, n?)
        }
        B::Plus if is_net_ty(rt) && !is_net_ty(lt) => {
            let (Some(a), Some(n)) = (net_operand(rb), int_operand(lb)) else {
                return Err(net_no_operator(lb, op, rb));
            };
            call(ScalarFn::InetPlInt8, PgType::Inet, a?, n?)
        }
        B::Minus if is_net_ty(lt) && is_net_ty(rt) => {
            let (Some(a), Some(b)) = (net_operand(lb), net_operand(rb)) else {
                return Err(net_no_operator(lb, op, rb));
            };
            call(ScalarFn::InetMi, PgType::Int8, a?, b?)
        }
        B::Minus if is_net_ty(lt) => {
            let (Some(a), Some(n)) = (net_operand(lb), int_operand(rb)) else {
                return Err(net_no_operator(lb, op, rb));
            };
            call(ScalarFn::InetMiInt8, PgType::Inet, a?, n?)
        }
        _ => Ok(None),
    }
}

/// Whether `ty` is a geometric type modeled here (`point` or `lseg`).
fn is_geo_ty(ty: Option<PgType>) -> bool {
    matches!(ty, Some(PgType::Point | PgType::Lseg))
}

/// Geometric (`point`/`lseg`) binary operators lower to `ScalarFn::Geo` calls,
/// as `resolve_network_op` does for the inet operators. Returns `Ok(None)` when
/// no geometric operand is present (so the generic path and its errors apply) or
/// when the operator/operand-type combination has no geometric operator.
fn resolve_geometric_op(
    op: &ast::BinaryOperator,
    lb: &Binding,
    rb: &Binding,
) -> Result<Option<Binding>, BindError> {
    use ast::BinaryOperator as B;
    use crate::functions::GeoFn;
    let lt = binding_typed_ty(lb);
    let rt = binding_typed_ty(rb);
    if !is_geo_ty(lt) && !is_geo_ty(rt) {
        return Ok(None);
    }
    // An untyped literal mirrors the other (geometric) side's type.
    let left_ty = lt.or(rt);
    let right_ty = rt.or(lt);
    // Both operands must land on a geometric type for any of these operators.
    if !is_geo_ty(left_ty) || !is_geo_ty(right_ty) {
        return Ok(None);
    }
    let l = |t: PgType| resolve_operand(lb, t);
    let r = |t: PgType| resolve_operand(rb, t);
    let call = |func, ret, args: Vec<BoundExpr>| {
        Ok(Some(Binding::Typed(BoundExpr::FuncCall { func, ret, args })))
    };
    let geo = |f: GeoFn| ScalarFn::Geo(f);

    use PgType::{Lseg, Point};
    let combo = (left_ty.unwrap(), right_ty.unwrap());
    match op {
        // Distance — point↔point, point↔lseg, lseg↔lseg.
        B::LtDashGt => match combo {
            (Point, Point) => call(geo(GeoFn::PointDist), PgType::Float8, vec![l(Point)?, r(Point)?]),
            (Point, Lseg) => {
                call(geo(GeoFn::DistPointSeg), PgType::Float8, vec![l(Point)?, r(Lseg)?])
            }
            (Lseg, Point) => {
                call(geo(GeoFn::DistPointSeg), PgType::Float8, vec![r(Point)?, l(Lseg)?])
            }
            (Lseg, Lseg) => call(geo(GeoFn::DistSegSeg), PgType::Float8, vec![l(Lseg)?, r(Lseg)?]),
            _ => Ok(None),
        },
        // Point positional / same-as / horizontal / vertical predicates.
        B::PGBitwiseShiftLeft if combo == (Point, Point) => {
            call(geo(GeoFn::PointLeft), PgType::Bool, vec![l(Point)?, r(Point)?])
        }
        B::PGBitwiseShiftRight if combo == (Point, Point) => {
            call(geo(GeoFn::PointRight), PgType::Bool, vec![l(Point)?, r(Point)?])
        }
        B::PipeGtGt if combo == (Point, Point) => {
            call(geo(GeoFn::PointAbove), PgType::Bool, vec![l(Point)?, r(Point)?])
        }
        B::LtLtPipe if combo == (Point, Point) => {
            call(geo(GeoFn::PointBelow), PgType::Bool, vec![l(Point)?, r(Point)?])
        }
        B::TildeEq if combo == (Point, Point) => {
            call(geo(GeoFn::PointEq), PgType::Bool, vec![l(Point)?, r(Point)?])
        }
        B::QuestionDash if combo == (Point, Point) => {
            call(geo(GeoFn::PointHoriz), PgType::Bool, vec![l(Point)?, r(Point)?])
        }
        B::QuestionPipe if combo == (Point, Point) => {
            call(geo(GeoFn::PointVert), PgType::Bool, vec![l(Point)?, r(Point)?])
        }
        // Point arithmetic (`-> point`).
        B::Plus if combo == (Point, Point) => {
            call(geo(GeoFn::PointAdd), PgType::Point, vec![l(Point)?, r(Point)?])
        }
        B::Minus if combo == (Point, Point) => {
            call(geo(GeoFn::PointSub), PgType::Point, vec![l(Point)?, r(Point)?])
        }
        B::Multiply if combo == (Point, Point) => {
            call(geo(GeoFn::PointMul), PgType::Point, vec![l(Point)?, r(Point)?])
        }
        B::Divide if combo == (Point, Point) => {
            call(geo(GeoFn::PointDiv), PgType::Point, vec![l(Point)?, r(Point)?])
        }
        // `point <@ lseg` (point lies on the segment).
        B::ArrowAt if combo == (Point, Lseg) => {
            call(geo(GeoFn::PointOnSeg), PgType::Bool, vec![l(Point)?, r(Lseg)?])
        }
        // `##` closest point: point→lseg or lseg→lseg (result on the 2nd operand).
        B::DoubleHash => match combo {
            (Point, Lseg) => {
                call(geo(GeoFn::ClosePointSeg), PgType::Point, vec![l(Point)?, r(Lseg)?])
            }
            (Lseg, Lseg) => call(geo(GeoFn::CloseSegSeg), PgType::Point, vec![l(Lseg)?, r(Lseg)?]),
            _ => Ok(None),
        },
        // `#` intersection point of two segments (NULL if none).
        B::PGBitwiseXor if combo == (Lseg, Lseg) => {
            call(geo(GeoFn::LsegInterpt), PgType::Point, vec![l(Lseg)?, r(Lseg)?])
        }
        // lseg parallel / perpendicular.
        B::QuestionDoublePipe if combo == (Lseg, Lseg) => {
            call(geo(GeoFn::LsegParallel), PgType::Bool, vec![l(Lseg)?, r(Lseg)?])
        }
        B::QuestionDashPipe if combo == (Lseg, Lseg) => {
            call(geo(GeoFn::LsegPerpendicular), PgType::Bool, vec![l(Lseg)?, r(Lseg)?])
        }
        // lseg b-tree comparisons (`=`/`<>` by endpoints, the rest by length).
        B::Eq if combo == (Lseg, Lseg) => {
            call(geo(GeoFn::LsegEq), PgType::Bool, vec![l(Lseg)?, r(Lseg)?])
        }
        B::NotEq if combo == (Lseg, Lseg) => {
            call(geo(GeoFn::LsegNe), PgType::Bool, vec![l(Lseg)?, r(Lseg)?])
        }
        B::Lt if combo == (Lseg, Lseg) => {
            call(geo(GeoFn::LsegLt), PgType::Bool, vec![l(Lseg)?, r(Lseg)?])
        }
        B::LtEq if combo == (Lseg, Lseg) => {
            call(geo(GeoFn::LsegLe), PgType::Bool, vec![l(Lseg)?, r(Lseg)?])
        }
        B::Gt if combo == (Lseg, Lseg) => {
            call(geo(GeoFn::LsegGt), PgType::Bool, vec![l(Lseg)?, r(Lseg)?])
        }
        B::GtEq if combo == (Lseg, Lseg) => {
            call(geo(GeoFn::LsegGe), PgType::Bool, vec![l(Lseg)?, r(Lseg)?])
        }
        _ => Ok(None),
    }
}

/// Whether `ty` is a bit-string type (`bit` or `bit varying`).
fn is_bit_family(ty: Option<PgType>) -> bool {
    matches!(ty, Some(PgType::Bit | PgType::Varbit))
}

/// A bit-string operand: a typed `bit`/`varbit` expression as-is (they share the
/// runtime value), or an untyped literal parsed as `bit`. Anything else is an
/// "operator does not exist" error via the caller.
fn bit_operand(b: &Binding) -> Option<Result<BoundExpr, BindError>> {
    match b {
        Binding::Typed(e) if is_bit_family(Some(e.ty())) => Some(Ok(e.clone())),
        Binding::Typed(_) => None,
        Binding::Unknown { lit, span } => Some(resolve_unknown(lit.clone(), *span, PgType::Bit)),
    }
}

/// `bit || bit` (or with an untyped literal): a `bit varying` concatenation.
fn bind_bit_concat(lb: Binding, rb: Binding) -> Result<Binding, BindError> {
    let (Some(a), Some(b)) = (bit_operand(&lb), bit_operand(&rb)) else {
        return Err(BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!(
                "operator does not exist: {} || {}",
                binding_type_label(&lb),
                binding_type_label(&rb)
            ),
        ));
    };
    Ok(Binding::Typed(BoundExpr::FuncCall {
        func: ScalarFn::BitConcat,
        ret: PgType::Varbit,
        args: vec![a?, b?],
    }))
}

/// bit/varbit bitwise and shift operators lower to `ScalarFn` calls. Returns
/// `Ok(None)` when the operator/operands are not a bit operation, so the generic
/// path (and its "operator does not exist" error) still applies.
fn resolve_bit_op(
    op: &ast::BinaryOperator,
    lb: &Binding,
    rb: &Binding,
) -> Result<Option<Binding>, BindError> {
    use ast::BinaryOperator as B;
    let lt = binding_typed_ty(lb);
    let rt = binding_typed_ty(rb);
    if !is_bit_family(lt) && !is_bit_family(rt) {
        return Ok(None);
    }
    let no_op = || {
        BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!(
                "operator does not exist: {} {op} {}",
                binding_type_label(lb),
                binding_type_label(rb)
            ),
        )
    };
    // Bitwise `& | #` and the shifts below are defined only on `bit` in PG (a
    // varbit operand is cast in), so the result type is always `bit`.
    let bitwise = match op {
        B::BitwiseAnd => Some(ScalarFn::BitAnd),
        B::BitwiseOr => Some(ScalarFn::BitOr),
        B::PGBitwiseXor => Some(ScalarFn::BitXor),
        _ => None,
    };
    if let Some(func) = bitwise {
        let (Some(a), Some(b)) = (bit_operand(lb), bit_operand(rb)) else {
            return Err(no_op());
        };
        return Ok(Some(Binding::Typed(BoundExpr::FuncCall {
            func,
            ret: PgType::Bit,
            args: vec![a?, b?],
        })));
    }
    // Shifts `<< >>`: `bit << int4`, keeping the bit length; result type `bit`.
    let shift = match op {
        B::PGBitwiseShiftLeft => Some(ScalarFn::BitShl),
        B::PGBitwiseShiftRight => Some(ScalarFn::BitShr),
        _ => None,
    };
    if let Some(func) = shift {
        if !is_bit_family(lt) {
            return Err(no_op());
        }
        let Some(a) = bit_operand(lb) else {
            return Err(no_op());
        };
        let amount = resolve_operand(rb, PgType::Int4)?;
        return Ok(Some(Binding::Typed(BoundExpr::FuncCall {
            func,
            ret: PgType::Bit,
            args: vec![a?, amount],
        })));
    }
    Ok(None)
}

/// `macaddr`/`macaddr8` `&`/`|`: lower to the width-dispatched `ScalarFn`. PG has
/// only `macaddr & macaddr` and `macaddr8 & macaddr8` — no cross-width operator
/// and no implicit `macaddr`<->`macaddr8` — so both operands must settle on the
/// *same* mac type. The typed side fixes that type; an untyped literal adopts it
/// (EUI-64 expanding for `macaddr8`). Two typed operands of different mac widths,
/// or a mac paired with any other typed value, have no operator: report PG's
/// `operator does not exist` rather than silently coercing one side.
fn resolve_macaddr_op(
    op: &ast::BinaryOperator,
    lb: &Binding,
    rb: &Binding,
) -> Result<Option<Binding>, BindError> {
    use ast::BinaryOperator as B;
    let func = match op {
        B::BitwiseAnd => ScalarFn::MacaddrAnd,
        B::BitwiseOr => ScalarFn::MacaddrOr,
        _ => return Ok(None),
    };
    let lt = binding_typed_ty(lb);
    let rt = binding_typed_ty(rb);
    let is_mac = |t: Option<PgType>| matches!(t, Some(PgType::Macaddr | PgType::Macaddr8));
    // Not our operator unless at least one side is a mac type.
    if !is_mac(lt) && !is_mac(rt) {
        return Ok(None);
    }
    // The mac type both operands must share, taken from a typed mac operand.
    // Two typed mac operands of different widths have no operator.
    let mac_ty = match (lt, rt) {
        (Some(l), Some(r)) if is_mac(lt) && is_mac(rt) => {
            if l != r {
                return Err(net_no_operator(lb, op, rb));
            }
            l
        }
        (Some(l), _) if is_mac(lt) => l,
        (_, Some(r)) if is_mac(rt) => r,
        _ => unreachable!("at least one operand is a mac type"),
    };
    // The partner must be the same-typed mac (handled above) or an untyped
    // literal; a typed non-mac partner (e.g. `macaddr & integer`) has no operator.
    let typed_non_mac = |t: Option<PgType>| t.is_some() && !is_mac(t);
    if typed_non_mac(lt) || typed_non_mac(rt) {
        return Err(net_no_operator(lb, op, rb));
    }
    let a = resolve_operand(lb, mac_ty)?;
    let b = resolve_operand(rb, mac_ty)?;
    Ok(Some(Binding::Typed(BoundExpr::FuncCall {
        func,
        ret: mac_ty,
        args: vec![a, b],
    })))
}

/// Materialize an operand at `target`: an untyped literal is parsed as `target`,
/// a typed operand is coerced (used for the numeric `* /` factor → float8).
fn resolve_operand(b: &Binding, target: PgType) -> Result<BoundExpr, BindError> {
    match b {
        Binding::Typed(e) if e.ty() == target => Ok(e.clone()),
        Binding::Typed(e) => coerce_expr(e.clone(), target),
        Binding::Unknown { lit, span } => resolve_unknown(lit.clone(), *span, target),
    }
}

fn bind_pow(lb: Binding, rb: Binding) -> Result<Binding, BindError> {
    let numeric = |b: &Binding| {
        matches!(b, Binding::Typed(e) if e.ty().is_numeric())
            || matches!(b, Binding::Unknown { .. })
    };
    if !numeric(&lb) || !numeric(&rb) {
        return Err(no_operator(&binding_type_label(&lb), BinOp::Pow, &binding_type_label(&rb)));
    }
    // PG's `^` exists for `float8` and `numeric`. A float operand selects the
    // float8 operator; otherwise a numeric operand selects numeric (returning
    // numeric); with only ints/unknowns it falls back to float8 (as PG does).
    let is_float = |b: &Binding| {
        matches!(b, Binding::Typed(e) if matches!(e.ty(), PgType::Float4 | PgType::Float8))
    };
    let is_num = |b: &Binding| matches!(b, Binding::Typed(e) if e.ty() == PgType::Numeric);
    if !is_float(&lb) && !is_float(&rb) && (is_num(&lb) || is_num(&rb)) {
        // numeric ^ numeric -> numeric, via the power() function.
        let left = pow_operand(lb, PgType::Numeric)?;
        let right = pow_operand(rb, PgType::Numeric)?;
        return Ok(Binding::Typed(BoundExpr::FuncCall {
            func: ScalarFn::NumPower,
            ret: PgType::Numeric,
            args: vec![left, right],
        }));
    }
    let left = pow_operand(lb, PgType::Float8)?;
    let right = pow_operand(rb, PgType::Float8)?;
    Ok(Binding::Typed(BoundExpr::Binary {
        op: BinOp::Pow,
        arg_ty: PgType::Float8,
        left: Box::new(left),
        right: Box::new(right),
    }))
}

fn pow_operand(b: Binding, target: PgType) -> Result<BoundExpr, BindError> {
    match b {
        Binding::Typed(e) => coerce_expr(e, target),
        Binding::Unknown { lit, span } => resolve_unknown(lit, span, target),
    }
}

pub(crate) fn binding_type_label(b: &Binding) -> String {
    match b {
        Binding::Typed(e) => e.ty().name().to_string(),
        Binding::Unknown { .. } => "unknown".to_string(),
    }
}

/// Coerce a function argument binding to `target`. Unknown literals resolve to
/// `target`; a typed argument matches exactly, or (when `exact_only` is false)
/// is promoted if `target` is its common type with `target` — reproducing PG's
/// implicit numeric widening for function arguments.
pub(crate) fn coerce_for_arg(
    binding: Binding,
    target: PgType,
    exact_only: bool,
) -> Option<BoundExpr> {
    match binding {
        Binding::Unknown { lit, span } => resolve_unknown(lit, span, target).ok(),
        Binding::Typed(e) => {
            if e.ty() == target {
                return Some(e);
            }
            if exact_only {
                return None;
            }
            if implicit_castable(e.ty(), target) {
                return coerce_expr(e, target).ok();
            }
            None
        }
    }
}

/// Whether `from` implicitly casts to `to` in a function-argument (or operator)
/// context — the numeric-widening casts PG marks implicit, including int→float4
/// (so e.g. `float4send(1)` resolves).
fn implicit_castable(from: PgType, to: PgType) -> bool {
    use PgType::*;
    from == to
        || matches!(
            (from, to),
            (Int2, Int4)
                | (Int2, Int8)
                | (Int4, Int8)
                | (Int2, Float4)
                | (Int4, Float4)
                | (Int8, Float4)
                | (Int2, Float8)
                | (Int4, Float8)
                | (Int8, Float8)
                | (Float4, Float8)
                | (Int2, Numeric)
                | (Int4, Numeric)
                | (Int8, Numeric)
                | (Numeric, Float4)
                | (Numeric, Float8)
                // `timestamp -> timestamptz` is an implicit cast in PG; the
                // reverse is assignment-only (reached via an explicit cast).
                | (Timestamp, TimestampTz)
                // `date` implicitly widens to `timestamp`/`timestamptz` (PG),
                // so date/timestamp comparisons and `date_trunc(text, date)`
                // resolve without dedicated date overloads.
                | (Date, Timestamp)
                | (Date, TimestampTz)
                // varchar/bpchar/name are implicitly convertible to text (so a
                // text function accepts them; `bpchar -> text` strips blanks).
                | (Varchar, Text)
                | (Bpchar, Text)
                | (Name, Text)
                // `cidr -> inet` is an implicit cast in PG, so the inet
                // functions/operators accept a cidr argument.
                | (Cidr, Inet)
                // int -> oid is implicit in PG (`oideq` resolves `oid = 42`);
                // used so catalog predicates/joins compare oid columns to int
                // literals and each other.
                | (Int2, Oid)
                | (Int4, Oid)
                | (Int8, Oid)
                // `bit` and `bit varying` are mutually implicitly convertible in
                // PG (binary-coercible with a length coercion), so a `bit`
                // literal resolves a `varbit` overload and vice versa.
                | (Bit, Varbit)
                | (Varbit, Bit)
        )
}

/// Coerce a binding to `text` for a string function/operator argument. An
/// untyped literal (or NULL) becomes text; a typed value casts to text.
pub(crate) fn to_text_operand(binding: Binding) -> Result<BoundExpr, BindError> {
    match binding {
        Binding::Unknown { lit, span } => resolve_unknown(lit, span, PgType::Text),
        Binding::Typed(e) if e.ty() == PgType::Text => Ok(e),
        Binding::Typed(e) => coerce_expr(e, PgType::Text),
    }
}

/// True for the text-family types that share `text`'s value representation.
pub(crate) fn is_text_family(ty: PgType) -> bool {
    matches!(ty, PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name)
}

/// Coerce an argument for `concat`/`concat_ws`/`format`, which use each value's
/// *output* representation. Text-family values are kept as-is (so a `bpchar`
/// keeps its blank padding, unlike the trailing-blank-stripping `||`); other
/// types are cast to their text form.
pub(crate) fn to_concat_operand(binding: Binding) -> Result<BoundExpr, BindError> {
    match binding {
        Binding::Unknown { lit, span } => resolve_unknown(lit, span, PgType::Text),
        Binding::Typed(e) if is_text_family(e.ty()) => Ok(e),
        Binding::Typed(e) => coerce_expr(e, PgType::Text),
    }
}

/// `a || b`: PG accepts `text || text` and `text || anynonarray` (either side),
/// but not two non-text operands. At least one side must be text or an untyped
/// literal; both are then coerced to text.
fn bind_string_concat(lb: Binding, rb: Binding) -> Result<Binding, BindError> {
    let textish = |b: &Binding| {
        matches!(b, Binding::Unknown { .. })
            || matches!(b, Binding::Typed(e) if e.ty() == PgType::Text)
    };
    if !textish(&lb) && !textish(&rb) {
        let (Binding::Typed(l), Binding::Typed(r)) = (&lb, &rb) else {
            unreachable!("a non-textish binding is typed");
        };
        return Err(BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!("operator does not exist: {} || {}", l.ty().name(), r.ty().name()),
        ));
    }
    let left = to_text_operand(lb)?;
    let right = to_text_operand(rb)?;
    Ok(Binding::Typed(BoundExpr::FuncCall {
        func: ScalarFn::TextConcat,
        ret: PgType::Text,
        args: vec![left, right],
    }))
}

/// `a [I]LIKE b [ESCAPE c]`: coerce operands to text and build the match call
/// (the escape string, when present, is a third argument), wrapping a negated
/// form in `NOT`.
fn bind_like(
    lb: Binding,
    rb: Binding,
    escape: Option<Binding>,
    case_insensitive: bool,
    negated: bool,
) -> Result<Binding, BindError> {
    let mut args = vec![to_text_operand(lb)?, to_text_operand(rb)?];
    if let Some(escape) = escape {
        args.push(to_text_operand(escape)?);
    }
    let call = BoundExpr::FuncCall {
        func: if case_insensitive { ScalarFn::ILike } else { ScalarFn::Like },
        ret: PgType::Bool,
        args,
    };
    let expr = if negated {
        BoundExpr::Unary { op: UnaryOp::Not, expr: Box::new(call) }
    } else {
        call
    };
    Ok(Binding::Typed(expr))
}

fn no_operator(left: &str, op: BinOp, right: &str) -> BindError {
    BindError::new(
        sqlstate::UNDEFINED_FUNCTION,
        format!(
            "operator does not exist: {left} {} {right}",
            op.sql_symbol()
        ),
    )
}

/// Settle two typed operands on a common type: exact match, or numeric
/// promotion via a `Coerce` on the narrower side.
fn unify_types(
    left: BoundExpr,
    right: BoundExpr,
    op: BinOp,
) -> Result<(BoundExpr, BoundExpr, PgType), BindError> {
    let (lty, rty) = (left.ty(), right.ty());
    if lty == rty {
        return Ok((left, right, lty));
    }
    if let Some(common) = common_numeric(lty, rty) {
        let left = coerce_expr(left, common)?;
        let right = coerce_expr(right, common)?;
        return Ok((left, right, common));
    }
    // Non-numeric implicit cast (e.g. `timestamp` -> `timestamptz`): when one
    // side implicitly casts to the other, compare in that common type, as PG
    // does (`tstz = timestamp`). Numeric pairs are already handled above, so
    // this only fires for the datetime cast and never changes numeric results.
    if implicit_castable(lty, rty) {
        return Ok((coerce_expr(left, rty)?, right, rty));
    }
    if implicit_castable(rty, lty) {
        return Ok((left, coerce_expr(right, lty)?, lty));
    }
    Err(no_operator(lty.name(), op, rty.name()))
}

/// The common type of two column entries (`VALUES` rows / `UNION` arms),
/// approximating PG's `select_common_type`: when exactly one side implicitly
/// casts to the other, the column takes that target (so `real` + `int4` -> `real`,
/// not `float8`). When neither or both cast implicitly, fall back to numeric
/// preferred-type promotion (`float8` dominates). This deliberately differs from
/// `unify_types` (operator resolution), where `real` + `int4` resolves to `float8`.
fn merge_types(a: PgType, b: PgType) -> Option<PgType> {
    if a == b {
        return Some(a);
    }
    match (implicit_castable(a, b), implicit_castable(b, a)) {
        (true, false) => Some(b),
        (false, true) => Some(a),
        // Mutually castable: today only `bit` <-> `bit varying`, whose common
        // type is `bit varying` (the preferred type of the bit-string category),
        // as PG's `select_common_type` resolves it.
        (true, true) => Some(PgType::Varbit),
        (false, false) => common_numeric(a, b),
    }
}

/// Resolve a set of expressions (one `VALUES`/`UNION` column, or a `CASE`'s
/// result branches) to a common type and coerce every one to it. Untyped
/// literals adapt to the resolved type; an entirely untyped set resolves to
/// `text`, as PG does for unknown `UNION`/`VALUES`/`CASE`. Incompatible concrete
/// types are a `42804` error, prefixed with `label` (`VALUES` / `CASE`) to match
/// PG's wording.
pub(crate) fn unify_value_column(
    bindings: Vec<Binding>,
    label: &str,
) -> Result<(PgType, Vec<BoundExpr>), BindError> {
    let mut common: Option<PgType> = None;
    for binding in &bindings {
        if let Binding::Typed(e) = binding {
            common = Some(match common {
                None => e.ty(),
                Some(prev) => merge_types(prev, e.ty()).ok_or_else(|| {
                    BindError::new(
                        sqlstate::DATATYPE_MISMATCH,
                        format!(
                            "{label} types {} and {} cannot be matched",
                            prev.name(),
                            e.ty().name()
                        ),
                    )
                })?,
            });
        }
    }
    let ty = common.unwrap_or(PgType::Text);
    let exprs = bindings
        .into_iter()
        .map(|binding| match binding {
            Binding::Unknown { lit, span } => resolve_unknown(lit, span, ty),
            Binding::Typed(e) => coerce_expr(e, ty),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((ty, exprs))
}

/// The common type of two distinct numeric types, following PG's preferred-type
/// resolution for the cases these tests exercise (float8 dominates; mixed int
/// widens).
fn common_numeric(a: PgType, b: PgType) -> Option<PgType> {
    if !a.is_numeric() || !b.is_numeric() {
        return None;
    }
    Some(
        if a == PgType::Float8 || b == PgType::Float8 {
            PgType::Float8
        } else if a == PgType::Float4 || b == PgType::Float4 {
            // A float mixed with any other numeric type resolves to float8.
            PgType::Float8
        } else if a == PgType::Numeric || b == PgType::Numeric {
            // `numeric` dominates the integer types (int → numeric is exact).
            PgType::Numeric
        } else if a == PgType::Int8 || b == PgType::Int8 {
            PgType::Int8
        } else if a == PgType::Int4 || b == PgType::Int4 {
            PgType::Int4
        } else {
            PgType::Int2
        },
    )
}

/// Coerce an expression to `ty`. Constant operands fold (and range-check) at
/// bind time, as PG's planner does; non-constants (and any cast to text, which
/// needs the session `extra_float_digits`) get a runtime `Coerce`.
fn coerce_expr(expr: BoundExpr, ty: PgType) -> Result<BoundExpr, BindError> {
    if expr.ty() == ty {
        return Ok(expr);
    }
    // `bpchar -> text` strips trailing blanks (PG's bpchar->text cast), which is
    // how a padded `char(n)` value loses its padding under `||`, `::text`, and
    // most text functions. It cannot be done in `cast_value` because a padded
    // `bpchar` value is indistinguishable from `text` there.
    if expr.ty() == PgType::Bpchar && ty == PgType::Text {
        if let BoundExpr::Const { value: Value::Text(s), .. } = &expr {
            return Ok(BoundExpr::Const {
                value: Value::Text(s.trim_end_matches(' ').to_string()),
                ty: PgType::Text,
            });
        }
        return Ok(BoundExpr::FuncCall {
            func: ScalarFn::BpcharToText,
            ret: PgType::Text,
            args: vec![expr],
        });
    }
    match expr {
        BoundExpr::Const { value, .. } if ty != PgType::Text => {
            let value = cast::cast_value(value, ty, 1)
                .map_err(|e| BindError::new(e.sqlstate, e.message))?;
            Ok(BoundExpr::Const { value, ty })
        }
        expr => Ok(BoundExpr::Coerce {
            expr: Box::new(expr),
            ty,
        }),
    }
}

/// Force a binding to boolean for WHERE / AND / OR / NOT. `context` is the
/// clause or operator name as PG prints it.
pub(crate) fn to_bool_operand(binding: Binding, context: &str) -> Result<BoundExpr, BindError> {
    match binding {
        Binding::Typed(e) if e.ty() == PgType::Bool => Ok(e),
        Binding::Typed(e) => Err(BindError::new(
            sqlstate::DATATYPE_MISMATCH,
            format!(
                "argument of {context} must be type boolean, not type {}",
                e.ty().name()
            ),
        )),
        Binding::Unknown { lit, span } => resolve_unknown(lit, span, PgType::Bool),
    }
}

/// Give an untyped literal its type from context, parsing its text the way the
/// type's input function would. A parse failure carries the literal's position
/// (PG's cursor), matching the `LINE n: ... ^` output.
pub(crate) fn resolve_unknown(
    lit: Option<String>,
    span: Span,
    ty: PgType,
) -> Result<BoundExpr, BindError> {
    let value = match lit {
        None => Value::Null,
        Some(s) => parse_unknown(&s, ty).map_err(|e| e.at(span))?,
    };
    Ok(BoundExpr::Const { value, ty })
}

fn parse_unknown(s: &str, ty: PgType) -> Result<Value, BindError> {
    let invalid = || {
        BindError::new(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            format!("invalid input syntax for type {}: \"{s}\"", ty.name()),
        )
    };
    let float_error = |e: float::FloatParseError| BindError::new(e.sqlstate, e.message);
    match ty {
        // varchar / bpchar / name share text's value representation; any length
        // limit is applied afterward as a typmod coercion.
        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => {
            Ok(Value::Text(s.to_string()))
        }
        // Integer input (trim, base-10, 22003 overflow vs 22P02 malformed) is
        // the same acceptor the executor's text→int cast uses; share it so the
        // two never drift. resolve_unknown attaches the cursor position.
        PgType::Int2 | PgType::Int4 | PgType::Int8 => {
            cast::text_to_int(s, ty).map_err(|e| BindError::new(e.sqlstate, e.message))
        }
        PgType::Oid => cast::text_to_oid(s).map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Float4 => float::float4in(s).map(Value::Float4).map_err(float_error),
        PgType::Float8 => float::float8in(s).map(Value::Float8).map_err(float_error),
        PgType::Numeric => Numeric::parse(s).map(Value::Numeric).map_err(|e| match e {
            ParseError::Syntax => invalid(),
            ParseError::Overflow => BindError::new(
                sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
                "value overflows numeric format",
            ),
        }),
        PgType::Bool => parse_bool(s).map(Value::Bool).ok_or_else(invalid),
        PgType::Date => date::parse(s)
            .map(Value::Date)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Time => time::parse(s)
            .map(Value::Time)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::TimeTz => timetz::parse(s)
            .map(Value::TimeTz)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Timestamp => timestamp::parse(s)
            .map(Value::Timestamp)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Interval => interval::parse(s)
            .map(Value::Interval)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::TimestampTz => timestamptz::parse(s)
            .map(Value::TimestampTz)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        // bytea input (byteain) is shared with the executor's text→bytea cast.
        PgType::Bytea => cast::byteain(s)
            .map(Value::Bytea)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Uuid => crabgresql_types::uuid::parse(s)
            .map(Value::Uuid)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Inet => crabgresql_types::net::inet_in(s)
            .map(Value::Inet)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Cidr => crabgresql_types::net::cidr_in(s)
            .map(Value::Cidr)
            .map_err(|e| {
                BindError::new(e.sqlstate, e.message).with_detail(e.detail.map(String::from))
            }),
        PgType::Money => money::parse(s)
            .map(Value::Money)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        // `bit_in`/`varbit_in`: default binary, `x`-prefixed hex. The typmod (the
        // declared length) is applied afterward by the caller's coercion.
        PgType::Bit | PgType::Varbit => crabgresql_types::bit::input(s)
            .map(|(len, data)| Value::Bit { len, data })
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Macaddr => crabgresql_types::macaddr::parse_macaddr(s)
            .map(Value::Macaddr)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Macaddr8 => crabgresql_types::macaddr::parse_macaddr8(s)
            .map(Value::Macaddr8)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Point => crabgresql_types::geo::parse_point(s)
            .map(Value::Point)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Lseg => crabgresql_types::geo::parse_lseg(s)
            .map(Value::Lseg)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::User(_) => Err(invalid()),
    }
}

/// Coerce an expression for assignment into a column (INSERT / UPDATE SET),
/// with PG's column-context error message on a type mismatch.
pub(crate) fn coerce_to_column(binding: Binding, column: &Column) -> Result<BoundExpr, BindError> {
    let base = match binding {
        Binding::Unknown { lit, span } => resolve_unknown(lit, span, column.ty)?,
        Binding::Typed(e) => {
            let ty = e.ty();
            if ty == column.ty {
                e
            } else if ty.is_numeric() && column.ty.is_numeric() {
                coerce_expr(e, column.ty)?
            // Any text-family type assigns to any other (text/varchar/char/name).
            } else if is_text_family(ty) && is_text_family(column.ty) {
                coerce_expr(e, column.ty)?
            // `bit` and `bit varying` assign to each other (shared value); the
            // length rule is applied by `apply_length_to_column`.
            } else if is_bit_family(Some(ty)) && is_bit_family(Some(column.ty)) {
                coerce_expr(e, column.ty)?
            // Assignment context also permits the implicit `timestamp ->
            // timestamptz` cast and its assignment-only reverse (both are plain
            // microsecond reinterprets under the UTC session zone), so inserting
            // a `timestamp` expression into a `timestamptz` column works, as in PG.
            } else if implicit_castable(ty, column.ty)
                || matches!((ty, column.ty), (PgType::TimestampTz, PgType::Timestamp))
            {
                coerce_expr(e, column.ty)?
            } else {
                return Err(BindError::new(
                    sqlstate::DATATYPE_MISMATCH,
                    format!(
                        "column \"{}\" is of type {} but expression is of type {}",
                        column.name,
                        column.ty.name(),
                        ty.name()
                    ),
                ));
            }
        }
    };
    apply_length_to_column(base, column)
}

/// Apply a column's `varchar(n)`/`char(n)`/`name` length coercion in assignment
/// context (an over-long varchar/char errors unless the excess is blank).
fn apply_length_to_column(expr: BoundExpr, column: &Column) -> Result<BoundExpr, BindError> {
    let func = match column.ty {
        PgType::Varchar if column.typmod >= 0 => ScalarFn::VarcharTypmod,
        PgType::Bpchar if column.typmod >= 0 => ScalarFn::BpcharTypmod,
        PgType::Bit if column.typmod >= 0 => ScalarFn::BitTypmod,
        PgType::Varbit if column.typmod >= 0 => ScalarFn::VarbitTypmod,
        PgType::Name => ScalarFn::NameInput,
        _ => return Ok(expr),
    };
    // Fold a constant now (assignment semantics: error on non-blank overflow).
    if let BoundExpr::Const { value: Value::Text(s), .. } = &expr {
        let folded = match func {
            ScalarFn::VarcharTypmod => crabgresql_types::text::varchar_input(s, column.typmod, false)
                .map_err(|e| BindError::new(e.sqlstate, e.message))?,
            ScalarFn::BpcharTypmod => crabgresql_types::text::bpchar_input(s, column.typmod, false)
                .map_err(|e| BindError::new(e.sqlstate, e.message))?,
            ScalarFn::NameInput => crabgresql_types::text::name_input(s),
            _ => unreachable!(),
        };
        return Ok(BoundExpr::Const { value: Value::Text(folded), ty: column.ty });
    }
    if let BoundExpr::Const { value: Value::Bit { len, data }, .. } = &expr {
        let (len, data) = crabgresql_types::bit::coerce(
            *len,
            data,
            column.typmod,
            column.ty == PgType::Varbit,
            false,
        )
        .map_err(|e| BindError::new(e.sqlstate, e.message))?;
        return Ok(BoundExpr::Const { value: Value::Bit { len, data }, ty: column.ty });
    }
    let mut args = vec![expr];
    if func != ScalarFn::NameInput {
        args.push(BoundExpr::Const { value: Value::Int4(column.typmod), ty: PgType::Int4 });
        // Third arg 0 = assignment (error on overflow), not a truncating cast.
        args.push(BoundExpr::Const { value: Value::Int4(0), ty: PgType::Int4 });
    }
    Ok(BoundExpr::FuncCall { func, ret: column.ty, args })
}

/// The result-column name PG derives from an expression's syntax: column
/// references keep their name (through parens), casts take the target type's
/// name, boolean literals are named after the type, everything else is
/// `?column?`.
pub(crate) fn output_name(expr: &ast::Expr) -> String {
    match expr {
        ast::Expr::Identifier(ident) => normalize_ident(ident),
        ast::Expr::CompoundIdentifier(parts) => parts
            .last()
            .map(normalize_ident)
            .unwrap_or_else(|| "?column?".into()),
        ast::Expr::Nested(inner) => output_name(inner),
        ast::Expr::Value(v) if matches!(v.value, ast::Value::Boolean(_)) => "bool".into(),
        // PG keeps a bare column's name through a cast (`id::int8` → "id"), but
        // uses the target type name when the argument has no inherent name
        // (`(1+1)::int8`, `'nan'::numeric::float4` → the type). Only a direct
        // column reference (strength 2) is preserved; a nested cast is not.
        ast::Expr::Cast {
            expr, data_type, ..
        } => column_name(expr).unwrap_or_else(|| type_output_name(data_type)),
        ast::Expr::TypedString(ts) => type_output_name(&ts.data_type),
        // `interval '...'` is named after the type, like a typed literal.
        ast::Expr::Interval(_) => "interval".into(),
        // EXTRACT(... ) is named "extract" in PG, regardless of the field.
        ast::Expr::Extract { .. } => "extract".into(),
        // `x AT TIME ZONE y` lowers to timezone(); PG names the column "timezone".
        ast::Expr::AtTimeZone { .. } => "timezone".into(),
        // A bare CASE expression is named "case" in PG.
        ast::Expr::Case { .. } => "case".into(),
        // CEIL/FLOOR special syntax is named after the function.
        ast::Expr::Ceil { .. } => "ceil".into(),
        ast::Expr::Floor { .. } => "floor".into(),
        // String special-syntax expressions are named after the function they
        // desugar to (`TRIM` → its ltrim/rtrim/btrim variant).
        ast::Expr::Substring { .. } => "substring".into(),
        ast::Expr::Position { .. } => "position".into(),
        ast::Expr::Overlay { .. } => "overlay".into(),
        ast::Expr::Trim { trim_where, .. } => match trim_where {
            Some(ast::TrimWhereField::Leading) => "ltrim".into(),
            Some(ast::TrimWhereField::Trailing) => "rtrim".into(),
            Some(ast::TrimWhereField::Both) | None => "btrim".into(),
        },
        // A function's output column is named after the function.
        ast::Expr::Function(func) => func
            .name
            .0
            .last()
            .and_then(|p| p.as_ident())
            .map(normalize_ident)
            .unwrap_or_else(|| "?column?".into()),
        _ => "?column?".into(),
    }
}

/// The name of a bare column reference (through parens), if any — PG's
/// strength-2 name that survives an enclosing cast. A cast, value, or function
/// argument has no such name.
fn column_name(expr: &ast::Expr) -> Option<String> {
    match expr {
        ast::Expr::Identifier(ident) => Some(normalize_ident(ident)),
        ast::Expr::CompoundIdentifier(parts) => parts.last().map(normalize_ident),
        ast::Expr::Nested(inner) => column_name(inner),
        _ => None,
    }
}

fn type_output_name(data_type: &ast::DataType) -> String {
    map_data_type(data_type)
        .map(|ty| ty.typname().to_string())
        .unwrap_or_else(|_| "?column?".into())
}
