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

use crabgresql_parser::{Span, ast};
use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::{Column, TableSchema};
use crabgresql_types::numeric::ParseError;
use crabgresql_types::{
    Numeric, PgType, Value, cast, float, interval, parse_bool, timestamp, timestamptz,
};

use crate::BindError;
use crate::functions::{ScalarFn, TableFn, bind_function, bind_srf_projection};

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
            BoundExpr::FuncCall { ret, .. } => *ret,
            BoundExpr::Case { ty, .. } => *ty,
            BoundExpr::Srf { ret, .. } => *ret,
        }
    }

    /// Whether this is a set-returning function marker (only legal at the top
    /// level of a projection list).
    pub fn is_srf(&self) -> bool {
        matches!(self, BoundExpr::Srf { .. })
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
}

impl Scope {
    /// No tables in scope: FROM-less SELECT, INSERT VALUES.
    pub fn empty() -> Scope {
        Scope { rels: Vec::new() }
    }

    pub fn table(schema: &TableSchema, qualifier: String) -> Scope {
        Scope {
            rels: vec![ScopeRel {
                qualifier,
                columns: schema.columns.clone(),
                offset: 0,
            }],
        }
    }

    /// A multi-relation scope for a cross join. Each `(qualifier, columns)` pair
    /// becomes a relation; offsets are assigned left-to-right so a column's
    /// index is its position in the concatenated row.
    pub fn relations(items: Vec<(String, Vec<Column>)>) -> Scope {
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
        Scope { rels }
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
        other => Err(unsupported_expr(other)),
    }
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
        other => Err(BindError::feature_not_supported(format!(
            "literal is not supported yet: {other}"
        ))),
    }
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
        // `timestamp` / `timestamp with time zone` (the precision modifier is
        // ignored; full microsecond resolution is kept).
        DataType::Timestamp(_, tz) => match tz {
            ast::TimezoneInfo::None | ast::TimezoneInfo::WithoutTimeZone => PgType::Timestamp,
            ast::TimezoneInfo::WithTimeZone | ast::TimezoneInfo::Tz => PgType::TimestampTz,
        },
        // `interval` (any field qualifier / precision is accepted and ignored;
        // full resolution is kept, as with `timestamp(2)`).
        DataType::Interval { .. } => PgType::Interval,
        DataType::Text | DataType::Varchar(None) | DataType::CharacterVarying(None) => PgType::Text,
        DataType::Varchar(Some(_)) | DataType::CharacterVarying(Some(_)) => {
            return Err(BindError::feature_not_supported(
                "varchar length limits are not supported yet",
            ));
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
    let target = map_data_type(data_type)?;
    let expr = match bind_expr(inner, scope)? {
        Binding::Unknown { lit, span } => resolve_unknown(lit, span, target)?,
        Binding::Typed(e) => coerce_expr(e, target)?,
    };
    Ok(Binding::Typed(apply_numeric_typmod_if_any(expr, target, data_type)?))
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
    Ok(Binding::Typed(apply_numeric_typmod_if_any(expr, target, &ts.data_type)?))
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
        other => Err(BindError::feature_not_supported(format!(
            "operator is not supported yet: {other}"
        ))),
    }
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

fn bind_binary(
    left: &ast::Expr,
    op: &ast::BinaryOperator,
    right: &ast::Expr,
    scope: &Scope,
) -> Result<Binding, BindError> {
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

    let lb = bind_expr(left, scope)?;
    let rb = bind_expr(right, scope)?;
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
        matches!(
            arg_ty,
            PgType::Bool
                | PgType::Int2
                | PgType::Int4
                | PgType::Int8
                | PgType::Float4
                | PgType::Float8
                | PgType::Numeric
                | PgType::Text
                | PgType::Bytea
                | PgType::Timestamp
                | PgType::Interval
                | PgType::TimestampTz
        )
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
    let is_temporal = |t: Option<PgType>| matches!(t, Some(PgType::Interval | PgType::Timestamp));
    if !is_temporal(lt) && !is_temporal(rt) {
        return Ok(None);
    }

    use PgType::{Interval as I, Timestamp as T};
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
        )
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
        _ => common_numeric(a, b),
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
        PgType::Text => Ok(Value::Text(s.to_string())),
        // Integer input (trim, base-10, 22003 overflow vs 22P02 malformed) is
        // the same acceptor the executor's text→int cast uses; share it so the
        // two never drift. resolve_unknown attaches the cursor position.
        PgType::Int2 | PgType::Int4 | PgType::Int8 => {
            cast::text_to_int(s, ty).map_err(|e| BindError::new(e.sqlstate, e.message))
        }
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
        PgType::Bit | PgType::User(_) => Err(invalid()),
    }
}

/// Coerce an expression for assignment into a column (INSERT / UPDATE SET),
/// with PG's column-context error message on a type mismatch.
pub(crate) fn coerce_to_column(binding: Binding, column: &Column) -> Result<BoundExpr, BindError> {
    match binding {
        Binding::Unknown { lit, span } => resolve_unknown(lit, span, column.ty),
        Binding::Typed(e) => {
            let ty = e.ty();
            if ty == column.ty {
                return Ok(e);
            }
            if ty.is_numeric() && column.ty.is_numeric() {
                return coerce_expr(e, column.ty);
            }
            // Assignment context also permits the implicit `timestamp ->
            // timestamptz` cast and its assignment-only reverse (both are plain
            // microsecond reinterprets under the UTC session zone), so inserting
            // a `timestamp` expression into a `timestamptz` column works, as in PG.
            if implicit_castable(ty, column.ty)
                || matches!((ty, column.ty), (PgType::TimestampTz, PgType::Timestamp))
            {
                return coerce_expr(e, column.ty);
            }
            Err(BindError::new(
                sqlstate::DATATYPE_MISMATCH,
                format!(
                    "column \"{}\" is of type {} but expression is of type {}",
                    column.name,
                    column.ty.name(),
                    ty.name()
                ),
            ))
        }
    }
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
