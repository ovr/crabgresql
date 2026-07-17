//! Expression binding: sqlparser `Expr` → typed [`BoundExpr`] IR.
//!
//! PG resolves operator overloads over concrete types while string literals
//! and `NULL` start out as the pseudo-type `unknown` and take their type from
//! context. [`Binding`] models exactly that: an expression is either typed or
//! still unknown, and every operator/assignment site decides what unknown
//! becomes (or rejects it the way PG does).

use crabgresql_parser::ast;
use crabgresql_protocol::sqlstate;
use crabgresql_storage_api::{Column, TableSchema};
use crabgresql_types::{PgType, Value};

use crate::BindError;

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
    /// Runtime cast between the integer types: int4→int8 widens, int8→int4
    /// range-checks (SQLSTATE 22003).
    Coerce {
        expr: Box<BoundExpr>,
        ty: PgType,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Neg,
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
}

impl BinOp {
    pub fn is_arithmetic(self) -> bool {
        matches!(
            self,
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod
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
            BoundExpr::Unary {
                op: UnaryOp::Neg,
                expr,
            } => expr.ty(),
            BoundExpr::Binary { op, arg_ty, .. } => {
                if op.is_arithmetic() {
                    *arg_ty
                } else {
                    PgType::Bool
                }
            }
            BoundExpr::IsNull { .. } => PgType::Bool,
            BoundExpr::Coerce { ty, .. } => *ty,
        }
    }
}

/// A binding result: typed, or an untyped literal awaiting context (PG's
/// `unknown` pseudo-type). `None` is the `NULL` literal.
#[derive(Clone, Debug, PartialEq)]
pub enum Binding {
    Typed(BoundExpr),
    Unknown(Option<String>),
}

/// Name-resolution scope: at most one table (M1), addressed by its name or
/// alias. With an alias the bare table name is not a valid qualifier — as in
/// PG.
pub struct Scope<'a> {
    schema: Option<&'a TableSchema>,
    qualifier: Option<String>,
}

impl<'a> Scope<'a> {
    /// No tables in scope: FROM-less SELECT, INSERT VALUES.
    pub fn empty() -> Scope<'static> {
        Scope {
            schema: None,
            qualifier: None,
        }
    }

    pub fn table(schema: &'a TableSchema, qualifier: String) -> Scope<'a> {
        Scope {
            schema: Some(schema),
            qualifier: Some(qualifier),
        }
    }

    fn resolve(&self, name: &str) -> Result<BoundExpr, BindError> {
        let index = self
            .schema
            .and_then(|schema| schema.column_index(name))
            .ok_or_else(|| {
                BindError::new(
                    sqlstate::UNDEFINED_COLUMN,
                    format!("column \"{name}\" does not exist"),
                )
            })?;
        Ok(BoundExpr::ColumnRef {
            index,
            ty: self.schema.unwrap().columns[index].ty,
        })
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
        ast::Expr::Value(v) => bind_value(&v.value),
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
        other => Err(unsupported_expr(other)),
    }
}

/// Bind at a spot with no surrounding type context (a SELECT-list item):
/// a leftover unknown resolves to text, as PG does in a bare SELECT.
pub fn bind_scalar(expr: &ast::Expr, scope: &Scope) -> Result<BoundExpr, BindError> {
    Ok(match bind_expr(expr, scope)? {
        Binding::Typed(e) => e,
        Binding::Unknown(lit) => resolve_unknown(lit, PgType::Text)?,
    })
}

fn unsupported_expr(expr: &ast::Expr) -> BindError {
    BindError::feature_not_supported(format!("expression is not supported yet: {expr}"))
}

fn bind_value(value: &ast::Value) -> Result<Binding, BindError> {
    match value {
        ast::Value::Number(n, _) => parse_number(n).map(Binding::Typed),
        ast::Value::SingleQuotedString(s)
        | ast::Value::DollarQuotedString(ast::DollarQuotedString { value: s, .. }) => {
            Ok(Binding::Unknown(Some(s.clone())))
        }
        ast::Value::Boolean(b) => Ok(Binding::Typed(BoundExpr::Const {
            value: Value::Bool(*b),
            ty: PgType::Bool,
        })),
        ast::Value::Null => Ok(Binding::Unknown(None)),
        other => Err(BindError::feature_not_supported(format!(
            "literal is not supported yet: {other}"
        ))),
    }
}

/// Integer literals become int4 when they fit, int8 otherwise — PG semantics.
/// Decimals need `numeric`, which is a later M1 type.
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
    Err(BindError::feature_not_supported(format!(
        "numeric literal \"{n}\" is not supported yet (numeric lands later in M1)"
    )))
}

fn bind_compound(parts: &[ast::Ident], scope: &Scope) -> Result<BoundExpr, BindError> {
    let [qualifier, column] = parts else {
        return Err(BindError::feature_not_supported(
            "schema-qualified column references are not supported yet",
        ));
    };
    let qualifier = normalize_ident(qualifier);
    if scope.qualifier.as_deref() != Some(qualifier.as_str()) {
        return Err(BindError::new(
            sqlstate::UNDEFINED_TABLE,
            format!("missing FROM-clause entry for table \"{qualifier}\""),
        ));
    }
    scope.resolve(&normalize_ident(column))
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
            let symbol = if op == ast::UnaryOperator::Minus {
                "-"
            } else {
                "+"
            };
            match bind_expr(operand, scope)? {
                Binding::Typed(e) if matches!(e.ty(), PgType::Int4 | PgType::Int8) => {
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
                Binding::Typed(e) => Err(BindError::new(
                    sqlstate::UNDEFINED_FUNCTION,
                    format!("operator does not exist: {symbol} {}", e.ty().name()),
                )),
                // Every numeric type has this operator, so an untyped literal
                // cannot pick one — PG reports ambiguity.
                Binding::Unknown(_) => Err(BindError::new(
                    sqlstate::AMBIGUOUS_FUNCTION,
                    format!("operator is not unique: {symbol} unknown"),
                )),
            }
        }
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

fn bind_is_null(inner: &ast::Expr, scope: &Scope, negated: bool) -> Result<Binding, BindError> {
    let expr = match bind_expr(inner, scope)? {
        Binding::Typed(e) => e,
        Binding::Unknown(lit) => resolve_unknown(lit, PgType::Text)?,
    };
    Ok(Binding::Typed(BoundExpr::IsNull {
        expr: Box::new(expr),
        negated,
    }))
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
        other => {
            return Err(BindError::feature_not_supported(format!(
                "operator is not supported yet: {other}"
            )));
        }
    };

    let lb = bind_expr(left, scope)?;
    let rb = bind_expr(right, scope)?;

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

    // Comparison or arithmetic: settle both operands on one type. For
    // arithmetic, the typed side must offer the operator BEFORE the unknown
    // side is parsed as that type — PG reports `operator does not exist:
    // boolean + unknown`, never a coercion failure, when no operator applies.
    let (left, right, arg_ty) = match (lb, rb) {
        (Binding::Typed(l), Binding::Typed(r)) => unify_types(l, r, op)?,
        (Binding::Typed(l), Binding::Unknown(lit)) => {
            let ty = l.ty();
            if op.is_arithmetic() && !matches!(ty, PgType::Int4 | PgType::Int8) {
                return Err(no_operator(ty.name(), op, "unknown"));
            }
            (l, resolve_unknown(lit, ty)?, ty)
        }
        (Binding::Unknown(lit), Binding::Typed(r)) => {
            let ty = r.ty();
            if op.is_arithmetic() && !matches!(ty, PgType::Int4 | PgType::Int8) {
                return Err(no_operator("unknown", op, ty.name()));
            }
            (resolve_unknown(lit, ty)?, r, ty)
        }
        (Binding::Unknown(l), Binding::Unknown(r)) => {
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
                resolve_unknown(l, PgType::Text)?,
                resolve_unknown(r, PgType::Text)?,
                PgType::Text,
            )
        }
    };

    if op.is_arithmetic() && !matches!(arg_ty, PgType::Int4 | PgType::Int8) {
        return Err(no_operator(arg_ty.name(), op, arg_ty.name()));
    }

    Ok(Binding::Typed(BoundExpr::Binary {
        op,
        arg_ty,
        left: Box::new(left),
        right: Box::new(right),
    }))
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

/// Settle two typed operands on a common type: exact match, or int4/int8
/// promotion via a `Coerce` on the int4 side. Anything else has no operator.
fn unify_types(
    left: BoundExpr,
    right: BoundExpr,
    op: BinOp,
) -> Result<(BoundExpr, BoundExpr, PgType), BindError> {
    let (lty, rty) = (left.ty(), right.ty());
    match (lty, rty) {
        _ if lty == rty => Ok((left, right, lty)),
        (PgType::Int4, PgType::Int8) => Ok((coerce_expr(left, PgType::Int8)?, right, PgType::Int8)),
        (PgType::Int8, PgType::Int4) => Ok((left, coerce_expr(right, PgType::Int8)?, PgType::Int8)),
        _ => Err(no_operator(lty.name(), op, rty.name())),
    }
}

/// Constants coerce (and range-check) at bind time, as PG's planner does when
/// it const-folds a cast — `UPDATE t SET id = 2147483648` errors even when no
/// row matches. Anything else gets a runtime `Coerce`.
///
/// The int4/int8 semantics mirror the runtime side
/// (`crabgresql_executor::eval::coerce_value`); they cannot share code because
/// the executor depends on this crate.
fn coerce_expr(expr: BoundExpr, ty: PgType) -> Result<BoundExpr, BindError> {
    match expr {
        BoundExpr::Const { value, .. } => {
            let value = match (value, ty) {
                (Value::Int4(v), PgType::Int8) => Value::Int8(v as i64),
                (Value::Int8(v), PgType::Int4) => match i32::try_from(v) {
                    Ok(v) => Value::Int4(v),
                    Err(_) => {
                        return Err(BindError::new(
                            sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
                            "integer out of range",
                        ));
                    }
                },
                (value, _) => value,
            };
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
        Binding::Unknown(lit) => resolve_unknown(lit, PgType::Bool),
    }
}

/// Give an untyped literal its type from context, parsing its text the way
/// the type's input function would.
pub(crate) fn resolve_unknown(lit: Option<String>, ty: PgType) -> Result<BoundExpr, BindError> {
    let value = match lit {
        None => Value::Null,
        Some(s) => parse_unknown(&s, ty)?,
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
    // PG's integer input functions distinguish a well-formed number that does
    // not fit (22003) from malformed input (22P02).
    let out_of_range = || {
        BindError::new(
            sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("value \"{s}\" is out of range for type {}", ty.name()),
        )
    };
    let int_error = |e: &std::num::ParseIntError| {
        use std::num::IntErrorKind;
        match e.kind() {
            IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => out_of_range(),
            _ => invalid(),
        }
    };
    match ty {
        PgType::Text => Ok(Value::Text(s.to_string())),
        PgType::Int4 => s
            .trim()
            .parse::<i32>()
            .map(Value::Int4)
            .map_err(|e| int_error(&e)),
        PgType::Int8 => s
            .trim()
            .parse::<i64>()
            .map(Value::Int8)
            .map_err(|e| int_error(&e)),
        PgType::Bool => parse_bool_text(s).map(Value::Bool).ok_or_else(invalid),
    }
}

/// The spellings `boolin` accepts: any unambiguous case-insensitive prefix of
/// true/false/yes/no/off, exact "on", and "1"/"0" (a bare "o" is ambiguous
/// between on and off) — trimmed, as in PG.
fn parse_bool_text(s: &str) -> Option<bool> {
    let s = s.trim().to_ascii_lowercase();
    match s.as_str() {
        "" => None,
        "1" | "on" => Some(true),
        "0" => Some(false),
        _ if "true".starts_with(&s) || "yes".starts_with(&s) => Some(true),
        _ if "false".starts_with(&s) || "no".starts_with(&s) => Some(false),
        _ if s.len() >= 2 && "off".starts_with(&s) => Some(false),
        _ => None,
    }
}

/// Coerce an expression for assignment into a column (INSERT / UPDATE SET),
/// with PG's column-context error message on a type mismatch.
pub(crate) fn coerce_to_column(binding: Binding, column: &Column) -> Result<BoundExpr, BindError> {
    match binding {
        Binding::Unknown(lit) => resolve_unknown(lit, column.ty),
        Binding::Typed(e) => {
            let ty = e.ty();
            match (ty, column.ty) {
                _ if ty == column.ty => Ok(e),
                (PgType::Int4, PgType::Int8) | (PgType::Int8, PgType::Int4) => {
                    coerce_expr(e, column.ty)
                }
                _ => Err(BindError::new(
                    sqlstate::DATATYPE_MISMATCH,
                    format!(
                        "column \"{}\" is of type {} but expression is of type {}",
                        column.name,
                        column.ty.name(),
                        ty.name()
                    ),
                )),
            }
        }
    }
}

/// The result-column name PG derives from an expression's syntax: column
/// references keep their name (through parens), boolean literals are named
/// after the type, everything else is `?column?`.
pub(crate) fn output_name(expr: &ast::Expr) -> String {
    match expr {
        ast::Expr::Identifier(ident) => normalize_ident(ident),
        ast::Expr::CompoundIdentifier(parts) => parts
            .last()
            .map(normalize_ident)
            .unwrap_or_else(|| "?column?".into()),
        ast::Expr::Nested(inner) => output_name(inner),
        ast::Expr::Value(v) if matches!(v.value, ast::Value::Boolean(_)) => "bool".into(),
        _ => "?column?".into(),
    }
}
