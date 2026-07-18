//! Scalar function resolution.
//!
//! Clean-room (see AGENTS.md): the function set, argument coercions, and error
//! text reproduce PG's *observable* behavior for the functions the float
//! regression tests call, pinned by the corpus. A minimal name+arity+coercion
//! resolver stands in for PG's full overload machinery — enough for these
//! tests, where arguments are floats, unknown literals, or ints promoted to
//! float8.

use crabgresql_parser::ast;
use crabgresql_pg_wire::sqlstate;
use crabgresql_types::PgType;

use crate::expr::{Binding, BoundExpr, Scope, bind_expr, coerce_for_arg};
use crate::{BindError, OutputColumn};

/// A scalar function the executor can evaluate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalarFn {
    Trunc,
    Round,
    Ceil,
    Floor,
    Sign,
    Sqrt,
    Cbrt,
    Exp,
    Ln,
    Power,
    Sinh,
    Cosh,
    Tanh,
    Asinh,
    Acosh,
    Atanh,
    Erf,
    Erfc,
    Gamma,
    Lgamma,
    Sind,
    Cosd,
    Tand,
    Cotd,
    Asind,
    Acosd,
    Atand,
    Atan2d,
    Float4Send,
    Float8Send,
    PgInputIsValid,
    Md5,
    /// `date_part(text, timestamp) -> float8`.
    DatePart,
    /// `EXTRACT(field FROM timestamp) -> numeric`; the field is a text arg.
    Extract,
    /// `date_trunc(text, timestamp) -> timestamp`.
    DateTrunc,
    /// `isfinite(timestamp) -> bool`.
    Isfinite,
    /// `make_timestamp(int, int, int, int, int, float8) -> timestamp`.
    MakeTimestamp,

    // --- interval operators (built directly by the binder, not via `lookup`) ---
    /// unary `- interval`.
    IntervalNeg,
    /// `interval + interval`.
    IntervalPl,
    /// `interval - interval`.
    IntervalMi,
    /// `interval * float8` (factor is arg 1).
    IntervalMul,
    /// `interval / float8`.
    IntervalDiv,
    /// `timestamp + interval -> timestamp`.
    TimestampPlInterval,
    /// `timestamp - interval -> timestamp`.
    TimestampMiInterval,
    /// `timestamp - timestamp -> interval`.
    TimestampMi,

    // --- interval functions ---
    /// `date_part(text, interval) -> float8`.
    DatePartInterval,
    /// `EXTRACT(field FROM interval) -> numeric`.
    ExtractInterval,
    /// `date_trunc(text, interval) -> interval`.
    DateTruncInterval,
    /// `isfinite(interval) -> bool`.
    IsfiniteInterval,
    /// `make_interval(int, int, int, int, int, int, float8) -> interval`.
    MakeInterval,
    /// `justify_days(interval) -> interval`.
    JustifyDays,
    /// `justify_hours(interval) -> interval`.
    JustifyHours,
    /// `justify_interval(interval) -> interval`.
    JustifyInterval,
    /// `age(timestamp, timestamp) -> interval`.
    Age,
    /// `to_char(interval, text) -> text`.
    ToCharInterval,
}

struct Signature {
    func: ScalarFn,
    args: &'static [PgType],
    ret: PgType,
}

/// A set-returning function callable in `FROM` position. Unlike [`ScalarFn`],
/// these produce a rowset (a fixed set of named output columns), so they are
/// bound as a relation rather than an expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableFn {
    /// `pg_input_error_info(value text, type_name text)` — the non-throwing
    /// sibling of `pg_input_is_valid`, reporting why an input would fail.
    PgInputErrorInfo,
}

impl TableFn {
    /// The function's declared parameter types (for arity/coercion checks).
    fn arg_types(self) -> &'static [PgType] {
        match self {
            TableFn::PgInputErrorInfo => &[PgType::Text, PgType::Text],
        }
    }

    /// The fixed output columns of the rowset, in order.
    pub fn columns(self) -> Vec<OutputColumn> {
        let text = |name: &str| OutputColumn {
            name: name.to_string(),
            ty: PgType::Text,
        };
        match self {
            TableFn::PgInputErrorInfo => vec![
                text("message"),
                text("detail"),
                text("hint"),
                text("sql_error_code"),
            ],
        }
    }
}

/// Resolve a set-returning function by (already lowercased) name.
pub fn lookup_table_fn(name: &str) -> Option<TableFn> {
    match name {
        "pg_input_error_info" => Some(TableFn::PgInputErrorInfo),
        _ => None,
    }
}

/// Bind a table function's call arguments to typed expressions, resolving the
/// function and enforcing arity/coercion. `arg_exprs` are the raw call
/// arguments (bound in the empty scope, as SRF arguments are constants here).
pub(crate) fn bind_table_fn_call(
    name: &str,
    arg_exprs: &[ast::Expr],
    scope: &Scope,
) -> Result<(TableFn, Vec<BoundExpr>), BindError> {
    let bindings = arg_exprs
        .iter()
        .map(|e| bind_expr(e, scope))
        .collect::<Result<Vec<_>, _>>()?;
    let Some(func) = lookup_table_fn(name) else {
        return Err(undefined_function(name, &bindings));
    };
    let params = func.arg_types();
    if params.len() != bindings.len() {
        return Err(undefined_function(name, &bindings));
    }
    // Exact-type first, then a coercing pass — same policy as scalar overloads.
    for exact_only in [true, false] {
        if let Some(args) = try_coerce_args(&bindings, params, exact_only) {
            return Ok((func, args));
        }
    }
    Err(undefined_function(name, &bindings))
}

const F8: PgType = PgType::Float8;
const TS: PgType = PgType::Timestamp;
const TEXT: PgType = PgType::Text;
const I4: PgType = PgType::Int4;
const IV: PgType = PgType::Interval;

/// The overloads for `name` (already lowercased). Most math functions take one
/// float8 and return float8.
fn lookup(name: &str) -> &'static [Signature] {
    macro_rules! unary_f8 {
        ($f:expr) => {
            &[Signature {
                func: $f,
                args: &[F8],
                ret: F8,
            }]
        };
    }
    match name {
        "trunc" => unary_f8!(ScalarFn::Trunc),
        "round" => unary_f8!(ScalarFn::Round),
        "ceil" | "ceiling" => unary_f8!(ScalarFn::Ceil),
        "floor" => unary_f8!(ScalarFn::Floor),
        "sign" => unary_f8!(ScalarFn::Sign),
        "sqrt" => unary_f8!(ScalarFn::Sqrt),
        "cbrt" => unary_f8!(ScalarFn::Cbrt),
        "exp" => unary_f8!(ScalarFn::Exp),
        "ln" => unary_f8!(ScalarFn::Ln),
        "sinh" => unary_f8!(ScalarFn::Sinh),
        "cosh" => unary_f8!(ScalarFn::Cosh),
        "tanh" => unary_f8!(ScalarFn::Tanh),
        "asinh" => unary_f8!(ScalarFn::Asinh),
        "acosh" => unary_f8!(ScalarFn::Acosh),
        "atanh" => unary_f8!(ScalarFn::Atanh),
        "erf" => unary_f8!(ScalarFn::Erf),
        "erfc" => unary_f8!(ScalarFn::Erfc),
        "gamma" => unary_f8!(ScalarFn::Gamma),
        "lgamma" => unary_f8!(ScalarFn::Lgamma),
        "sind" => unary_f8!(ScalarFn::Sind),
        "cosd" => unary_f8!(ScalarFn::Cosd),
        "tand" => unary_f8!(ScalarFn::Tand),
        "cotd" => unary_f8!(ScalarFn::Cotd),
        "asind" => unary_f8!(ScalarFn::Asind),
        "acosd" => unary_f8!(ScalarFn::Acosd),
        "atand" => unary_f8!(ScalarFn::Atand),
        "power" | "pow" => &[Signature {
            func: ScalarFn::Power,
            args: &[F8, F8],
            ret: F8,
        }],
        "atan2d" => &[Signature {
            func: ScalarFn::Atan2d,
            args: &[F8, F8],
            ret: F8,
        }],
        "float4send" => &[Signature {
            func: ScalarFn::Float4Send,
            args: &[PgType::Float4],
            ret: PgType::Bytea,
        }],
        "float8send" => &[Signature {
            func: ScalarFn::Float8Send,
            args: &[F8],
            ret: PgType::Bytea,
        }],
        "pg_input_is_valid" => &[Signature {
            func: ScalarFn::PgInputIsValid,
            args: &[PgType::Text, PgType::Text],
            ret: PgType::Bool,
        }],
        "date_part" => &[
            Signature { func: ScalarFn::DatePart, args: &[TEXT, TS], ret: F8 },
            Signature { func: ScalarFn::DatePartInterval, args: &[TEXT, IV], ret: F8 },
        ],
        "date_trunc" => &[
            Signature { func: ScalarFn::DateTrunc, args: &[TEXT, TS], ret: TS },
            Signature { func: ScalarFn::DateTruncInterval, args: &[TEXT, IV], ret: IV },
        ],
        "isfinite" => &[
            Signature { func: ScalarFn::Isfinite, args: &[TS], ret: PgType::Bool },
            Signature { func: ScalarFn::IsfiniteInterval, args: &[IV], ret: PgType::Bool },
        ],
        "make_timestamp" => &[Signature {
            func: ScalarFn::MakeTimestamp,
            args: &[I4, I4, I4, I4, I4, F8],
            ret: TS,
        }],
        "make_interval" => &[Signature {
            func: ScalarFn::MakeInterval,
            args: &[I4, I4, I4, I4, I4, I4, F8],
            ret: IV,
        }],
        "justify_days" => &[Signature { func: ScalarFn::JustifyDays, args: &[IV], ret: IV }],
        "justify_hours" => &[Signature { func: ScalarFn::JustifyHours, args: &[IV], ret: IV }],
        "justify_interval" => &[Signature { func: ScalarFn::JustifyInterval, args: &[IV], ret: IV }],
        "age" => &[Signature { func: ScalarFn::Age, args: &[TS, TS], ret: IV }],
        "to_char" => &[Signature { func: ScalarFn::ToCharInterval, args: &[IV, TEXT], ret: TEXT }],
        // Two overloads. Text is listed first so a bare `md5('abc')` unknown
        // literal resolves to text; a typed `bytea` argument never coerces to
        // text (see `implicit_castable`), so `md5(x::bytea)` binds the bytea one.
        "md5" => &[
            Signature {
                func: ScalarFn::Md5,
                args: &[PgType::Text],
                ret: PgType::Text,
            },
            Signature {
                func: ScalarFn::Md5,
                args: &[PgType::Bytea],
                ret: PgType::Text,
            },
        ],
        _ => &[],
    }
}

/// The last part of a function name, lowercased (`pg_catalog.abs` → `abs`).
fn function_name(name: &ast::ObjectName) -> Option<String> {
    name.0
        .last()
        .and_then(|p| p.as_ident())
        .map(crate::expr::normalize_ident)
}

pub(crate) fn bind_function(func: &ast::Function, scope: &Scope) -> Result<Binding, BindError> {
    if func.over.is_some()
        || func.filter.is_some()
        || !func.within_group.is_empty()
        || func.null_treatment.is_some()
    {
        return Err(BindError::feature_not_supported(
            "this function form is not supported yet",
        ));
    }
    let Some(name) = function_name(&func.name) else {
        return Err(BindError::feature_not_supported(format!(
            "function is not supported yet: {func}"
        )));
    };
    let arg_exprs = positional_args(&func.args)?;
    let bindings = arg_exprs
        .iter()
        .map(|e| bind_expr(e, scope))
        .collect::<Result<Vec<_>, _>>()?;

    let sigs = lookup(&name);
    if sigs.is_empty() {
        return Err(undefined_function(&name, &bindings));
    }
    // Prefer an exact-arity signature whose args all coerce; try exact-type
    // matches before ones needing a coercion.
    for pass in [true, false] {
        for sig in sigs {
            if sig.args.len() != bindings.len() {
                continue;
            }
            if let Some(args) = try_coerce_args(&bindings, sig.args, pass) {
                return Ok(Binding::Typed(BoundExpr::FuncCall {
                    func: sig.func,
                    ret: sig.ret,
                    args,
                }));
            }
        }
    }
    Err(undefined_function(&name, &bindings))
}

/// Try to coerce every binding to the signature's parameter types. When
/// `exact_only`, reject anything that would need a numeric promotion.
fn try_coerce_args(
    bindings: &[Binding],
    params: &[PgType],
    exact_only: bool,
) -> Option<Vec<BoundExpr>> {
    let mut out = Vec::with_capacity(params.len());
    for (binding, &target) in bindings.iter().zip(params) {
        out.push(coerce_for_arg(binding.clone(), target, exact_only)?);
    }
    Some(out)
}

fn undefined_function(name: &str, bindings: &[Binding]) -> BindError {
    let types = bindings
        .iter()
        .map(crate::expr::binding_type_label)
        .collect::<Vec<_>>()
        .join(", ");
    BindError::new(
        sqlstate::UNDEFINED_FUNCTION,
        format!("function {name}({types}) does not exist"),
    )
}

fn positional_args(args: &ast::FunctionArguments) -> Result<Vec<ast::Expr>, BindError> {
    let list = match args {
        ast::FunctionArguments::None => return Ok(Vec::new()),
        ast::FunctionArguments::List(list) => list,
        ast::FunctionArguments::Subquery(_) => {
            return Err(BindError::feature_not_supported(
                "subquery function arguments are not supported yet",
            ));
        }
    };
    if list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
        return Err(BindError::feature_not_supported(
            "this function argument form is not supported yet",
        ));
    }
    positional_arg_exprs(&list.args)
}

/// Extract plain positional argument expressions, rejecting named/wildcard
/// forms. Shared by scalar calls and table-function (FROM-position) calls.
pub(crate) fn positional_arg_exprs(
    args: &[ast::FunctionArg],
) -> Result<Vec<ast::Expr>, BindError> {
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(e)) => out.push(e.clone()),
            _ => {
                return Err(BindError::feature_not_supported(
                    "named or wildcard function arguments are not supported yet",
                ));
            }
        }
    }
    Ok(out)
}
