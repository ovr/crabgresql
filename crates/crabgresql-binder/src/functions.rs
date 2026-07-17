//! Scalar function resolution.
//!
//! Clean-room (see AGENTS.md): the function set, argument coercions, and error
//! text reproduce PG's *observable* behavior for the functions the float
//! regression tests call, pinned by the corpus. A minimal name+arity+coercion
//! resolver stands in for PG's full overload machinery — enough for these
//! tests, where arguments are floats, unknown literals, or ints promoted to
//! float8.

use crabgresql_parser::ast;
use crabgresql_protocol::sqlstate;
use crabgresql_types::PgType;

use crate::BindError;
use crate::expr::{Binding, BoundExpr, Scope, bind_expr, coerce_for_arg};

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
}

struct Signature {
    func: ScalarFn,
    args: &'static [PgType],
    ret: PgType,
}

const F8: PgType = PgType::Float8;

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
    if !list.duplicate_treatment.is_none() || !list.clauses.is_empty() {
        return Err(BindError::feature_not_supported(
            "this function argument form is not supported yet",
        ));
    }
    let mut out = Vec::with_capacity(list.args.len());
    for arg in &list.args {
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

trait DuplicateTreatmentExt {
    fn is_none(&self) -> bool;
}

impl DuplicateTreatmentExt for Option<ast::DuplicateTreatment> {
    fn is_none(&self) -> bool {
        Option::is_none(self)
    }
}
