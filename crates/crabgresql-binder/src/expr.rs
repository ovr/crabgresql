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
use crabgresql_storage_api::{Column, EnumInfo, TableEngine, TableSchema, TypeCatalog, UserCast};
use crabgresql_types::collation::DEFAULT_COLLATION_OID;
use crabgresql_types::numeric::ParseError;
use crabgresql_types::{
    Numeric, PgType, RegKind, Value, cast, date, float, interval, money, parse_bool, time,
    timestamp, timestamptz, timetz,
};

use crate::BindError;
use crate::functions::{AggFn, ScalarFn, TableFn, bind_function, bind_srf_projection};

/// Shared, mutable parameter-inference state for one statement. A `$n`
/// occurrence anywhere in the statement — target list, WHERE, a subquery, a CTE
/// — refers to the same slot here, so a type deduced at one site is visible at
/// every other. Held behind `Rc<RefCell<…>>` because the binder threads a single
/// context through the whole tree (see [`Scope`]) while several sites borrow it.
pub type ParamCtx = std::rc::Rc<std::cell::RefCell<ParamState>>;

/// Per-statement bind-parameter types, indexed by parameter number minus one.
/// `None` in a slot means the type is not yet known; the extended protocol may
/// seed some slots from the client's declared OID list.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamState {
    /// `types[i]` is the type of `$(i+1)`; `None` until inferred/declared.
    types: Vec<Option<PgType>>,
    /// Whether `$n` placeholders are permitted at all. A simple-query bind sets
    /// this false, so any `$n` is PG's `42P02` "there is no parameter $n".
    allow: bool,
    /// The highest valid parameter number, when the caller knows it up front
    /// (a SQL function body has exactly `$1..$max` for its declared arguments).
    /// `None` leaves the wire-protocol bound (`MAX_PARAMS`) as the only cap.
    max: Option<usize>,
}

/// Upper bound on a parameter number `$n`. The Bind message carries the values
/// in an `i16`-counted array, so no more than this many can ever be supplied.
const MAX_PARAMS: usize = 65535;

impl ParamState {
    /// Register a `$n` (1-based) occurrence, growing the slot vector as needed
    /// and returning its 0-based index. When placeholders are not allowed (a
    /// simple query), this is PG's `42P02` "there is no parameter $n".
    fn reference(&mut self, n1: usize) -> Result<usize, BindError> {
        if !self.allow {
            return Err(BindError::new(
                "42P02",
                format!("there is no parameter ${n1}"),
            ));
        }
        // A caller that declared its parameters (a SQL function body) rejects any
        // `$n` past the last argument here, at the reference site, so the error
        // names the actual `n` — matching PG's "there is no parameter $n".
        if self.max.is_some_and(|max| n1 > max) {
            return Err(BindError::new(
                "42P02",
                format!("there is no parameter ${n1}"),
            ));
        }
        // A parameter number is bounded by the wire protocol (Bind delivers at
        // most 65535 parameter values), so a larger `$n` can never be supplied.
        // Reject it up front rather than resizing the slot vector to an
        // attacker-chosen length — `SELECT $2000000000` would otherwise allocate
        // gigabytes.
        if n1 > MAX_PARAMS {
            return Err(BindError::new(
                "54000",
                format!("there is no parameter ${n1}"),
            ));
        }
        let index = n1 - 1;
        if index >= self.types.len() {
            self.types.resize(index + 1, None);
        }
        Ok(index)
    }

    /// Record that parameter `index` was used in a context of type `ty`. A slot
    /// that already carries a *different* concrete type is PG's `42P18`
    /// "inconsistent types deduced for parameter $n".
    fn resolve(&mut self, index: usize, ty: PgType) -> Result<(), BindError> {
        if index >= self.types.len() {
            self.types.resize(index + 1, None);
        }
        match self.types[index] {
            Some(existing) if existing != ty => Err(BindError::new(
                "42P18",
                format!("inconsistent types deduced for parameter ${}", index + 1),
            )),
            _ => {
                self.types[index] = Some(ty);
                Ok(())
            }
        }
    }
}

/// A parameter context for the extended query protocol: placeholders are
/// allowed, and `declared` seeds the initially-known types (a `None` slot is
/// left to be inferred from context, as PG does for an unspecified OID).
pub fn param_ctx_extended(declared: Vec<Option<PgType>>) -> ParamCtx {
    std::rc::Rc::new(std::cell::RefCell::new(ParamState {
        types: declared,
        allow: true,
        max: None,
    }))
}

/// A parameter context for a SQL function body: `$1..$declared.len()` are the
/// declared argument types, and any larger `$n` is PG's `42P02` "there is no
/// parameter $n" reported at the reference site with the real `n`.
pub fn param_ctx_capped(declared: Vec<Option<PgType>>) -> ParamCtx {
    let max = Some(declared.len());
    std::rc::Rc::new(std::cell::RefCell::new(ParamState {
        types: declared,
        allow: true,
        max,
    }))
}

/// A parameter context for the simple query protocol: any `$n` is an error
/// (`42P02`), matching PG, which only accepts parameters via `Parse`/`Bind`.
pub fn param_ctx_none() -> ParamCtx {
    std::rc::Rc::new(std::cell::RefCell::new(ParamState {
        types: Vec::new(),
        allow: false,
        max: None,
    }))
}

/// The current inferred/declared parameter types (index = parameter number − 1).
/// The caller reads this after a successful bind to describe the statement's
/// parameters; a `None` slot is a parameter whose type could not be determined.
pub fn param_types(ctx: &ParamCtx) -> Vec<Option<PgType>> {
    ctx.borrow().types.clone()
}

/// Fail with PG's `42P18` "could not determine data type of parameter $n" for
/// the first parameter whose type is still unknown after binding. The extended
/// protocol calls this before describing a statement.
pub fn require_all_resolved(ctx: &ParamCtx) -> Result<(), BindError> {
    let state = ctx.borrow();
    for (i, ty) in state.types.iter().enumerate() {
        if ty.is_none() {
            return Err(BindError::new(
                "42P18",
                format!("could not determine data type of parameter ${}", i + 1),
            ));
        }
    }
    Ok(())
}

/// A subquery's bound plan, embedded in a [`BoundExpr`]. Wrapped so `BoundExpr`
/// keeps its `Debug`/`PartialEq` derives without imposing them on
/// [`crate::LogicalPlan`], which holds trait objects (`Arc<dyn TableAm>`) that
/// implement neither. Two embedded subplans never compare equal: structural plan
/// equality is needed nowhere, and treating them as distinct keeps optimizations
/// that dedup expressions (e.g. ORDER BY target reuse) conservatively correct.
#[derive(Clone)]
pub struct Subplan(pub Box<crate::plan::LogicalPlan>);

impl std::fmt::Debug for Subplan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Subplan(..)")
    }
}

impl PartialEq for Subplan {
    fn eq(&self, _: &Self) -> bool {
        false
    }
}

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
    /// A bind-parameter placeholder (`$1`, `$2`, …) from the extended query
    /// protocol. `index` is the 0-based parameter number and `ty` the type
    /// inferred from context (or declared by the client). Like a `ColumnRef`,
    /// this is a per-execution runtime value, not a constant — it is supplied by
    /// the Bind message and must never be constant-folded.
    Param {
        index: usize,
        ty: PgType,
    },
    /// A reference to a column of an enclosing query, from within a correlated
    /// subquery — modelled on PostgreSQL's `Var` with `varlevelsup`. `level` is
    /// how many query levels up the referenced relation lives (1 = the immediate
    /// parent), `index` is that ancestor row's combined-row index, and `ty` the
    /// column's type. Like a `ColumnRef` this is a per-execution runtime value:
    /// the executor substitutes it with the outer row's value (via
    /// `crate::plan::substitute_outer`) each time the correlated subplan runs, so
    /// evaluating one directly is an internal invariant break (see
    /// `executor::eval`).
    OuterColumnRef {
        level: usize,
        index: usize,
        ty: PgType,
    },
    /// A collation attached to a string-typed expression: either an explicit
    /// `expr COLLATE "name"` clause, or the declared collation of a column,
    /// which the binder wraps around the `ColumnRef` so the collation travels
    /// with the expression.
    ///
    /// Value-transparent — evaluating it evaluates `expr` unchanged. It exists
    /// only so [`expr_collation`] can derive which collation a comparison or
    /// sort should use, and `explicit` records the two strengths PostgreSQL
    /// distinguishes when combining them (a clause overrides a column's own).
    Collate {
        expr: Box<BoundExpr>,
        collation: u32,
        explicit: bool,
    },
    Unary {
        op: UnaryOp,
        expr: Box<BoundExpr>,
    },
    /// `arg_ty` is the operand type after promotion; comparisons and logic
    /// yield bool, arithmetic yields `arg_ty`. `collation` is the collation
    /// derived from the operands, used when `arg_ty` is a string type — the
    /// only case where it affects the result of `<`/`>` (equality is bytewise
    /// under every supported collation).
    Binary {
        op: BinOp,
        arg_ty: PgType,
        collation: u32,
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
    /// A call to a user-defined routine the binder cannot inline — a
    /// `LANGUAGE plpgsql` body, which is an imperative program rather than an
    /// expression. Unlike [`BoundExpr::FuncCall`] it carries no [`ScalarFn`] to
    /// dispatch on, so it survives to execution as a marker and `eval` hands it
    /// to the routine interpreter.
    ///
    /// Arguments are already coerced to the declared types and are evaluated
    /// exactly once per call, so the inline path's volatile-argument hazard
    /// (see `resolve_user_routine_call`) does not arise here.
    Routine {
        /// Catalog OID of the resolved overload — the runtime handle.
        oid: u32,
        /// Carried so an error raised before the interpreter is entered (the
        /// routine was dropped mid-statement) can still name it.
        name: Arc<str>,
        arg_types: Arc<[PgType]>,
        /// `STRICT`: any NULL argument yields NULL without entering the body.
        strict: bool,
        args: Vec<BoundExpr>,
        ret: PgType,
    },
    /// An array constructor (`ARRAY[a, b, c]` or `ARRAY[]::t[]`). Every element
    /// is already coerced to `elem`, the common element type; `ty` is the
    /// resulting `PgType::Array(elem.oid())`. Evaluates element-wise to a
    /// [`Value::Array`].
    ArrayCtor {
        elem: PgType,
        ty: PgType,
        elems: Vec<BoundExpr>,
    },
    /// Array element access (`a[i]`). `base` is an array expression, `index` is
    /// an int4; the result type `ty` is the element type. A NULL or out-of-range
    /// subscript yields NULL (PG semantics), never an error.
    Subscript {
        base: Box<BoundExpr>,
        index: Box<BoundExpr>,
        ty: PgType,
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
        /// The per-row argument expressions. Empty for `COUNT(*)` (count every
        /// row); one entry for a unary aggregate; two for `string_agg(value,
        /// delimiter)`. The first argument is the value whose NULL skips the row.
        args: Vec<BoundExpr>,
        /// The (first) argument's pre-aggregation type — drives accumulator
        /// dispatch. Unused for `COUNT(*)`.
        input_ty: PgType,
        /// The aggregate's result type (see `agg_return_type`).
        ret: PgType,
    },
    /// A scalar subquery `(SELECT …)`: `subplan` yields exactly one column. A
    /// transient marker for non-correlated subqueries — the executor's
    /// `resolve_subqueries` pass runs the subplan once and folds this node to a
    /// `Const` (0 rows → NULL, 1 row → that value, >1 rows → `21000`) before
    /// evaluation. `ty` is the single output column's type.
    ScalarSubquery {
        subplan: Subplan,
        ty: PgType,
    },
    /// `[NOT] EXISTS (SELECT …)`: folds (in `resolve_subqueries`) to a bool
    /// `Const` — whether `subplan` yields any row, negated when `negated`.
    Exists {
        subplan: Subplan,
        negated: bool,
    },
    /// `left op ANY(SELECT …)` / `left op ALL(SELECT …)`, and equally
    /// `x [NOT] IN (SELECT …)`, which PostgreSQL defines as `= ANY` / `<> ALL`
    /// (see [`bind_in_subquery`]). `cmp` is the bound `left op <hole>`
    /// comparison template — a `Binary { op, arg_ty, left, right }` whose `right`
    /// is a NULL `Const` of the subquery column's type, possibly wrapped in the
    /// coercions the binder resolved. At execution the one-column `subplan`
    /// supplies the candidate values, each substituted into that hole and
    /// compared against the needle (evaluated once): `ANY`/`SOME` is true on the
    /// first match (empty ⇒ false), `ALL` false on the first counterexample
    /// (empty ⇒ true), with three-valued NULL logic throughout.
    QuantifiedSubquery {
        subplan: Subplan,
        /// `true` for `ALL` (AND-chain), `false` for `ANY`/`SOME` (OR-chain).
        all: bool,
        cmp: Box<BoundExpr>,
    },
    /// `left op ANY(array_expr)` / `left op ALL(array_expr)`. `array` evaluates to
    /// a `Value::Array` per row; `cmp` is the `left op <hole>` template over the
    /// element type; `all` selects AND (ALL) vs OR (ANY/SOME) chaining. Unlike
    /// [`BoundExpr::QuantifiedSubquery`] this is an ordinary per-row expression,
    /// not a subquery marker.
    QuantifiedArray {
        array: Box<BoundExpr>,
        /// `true` for `ALL` (AND-chain), `false` for `ANY`/`SOME` (OR-chain).
        all: bool,
        cmp: Box<BoundExpr>,
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
    /// Evaluated per source row; empty = `COUNT(*)`. The first argument is the
    /// value (a NULL there skips the row); `string_agg` carries the delimiter
    /// as a second argument.
    pub args: Vec<BoundExpr>,
    pub input_ty: PgType,
    pub ret: PgType,
    /// The collation `min`/`max` should compare `args[0]` under — the
    /// database default for a non-collatable `input_ty` or an argument with
    /// no collation of its own. Every other aggregate ignores this.
    pub collation: u32,
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
            BoundExpr::Param { ty, .. } => *ty,
            BoundExpr::OuterColumnRef { ty, .. } => *ty,
            // Value-transparent: a collation never changes the operand's type.
            BoundExpr::Collate { expr, .. } => expr.ty(),
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
            BoundExpr::FuncCall { ret, .. } | BoundExpr::Routine { ret, .. } => *ret,
            BoundExpr::ArrayCtor { ty, .. } => *ty,
            BoundExpr::Subscript { ty, .. } => *ty,
            BoundExpr::Case { ty, .. } => *ty,
            BoundExpr::Srf { ret, .. } => *ret,
            BoundExpr::Aggregate { ret, .. } => *ret,
            BoundExpr::ScalarSubquery { ty, .. } => *ty,
            BoundExpr::Exists { .. }
            | BoundExpr::QuantifiedSubquery { .. }
            | BoundExpr::QuantifiedArray { .. } => PgType::Bool,
        }
    }

    /// Whether this is a set-returning function marker (only legal at the top
    /// level of a projection list).
    pub fn is_srf(&self) -> bool {
        matches!(self, BoundExpr::Srf { .. })
    }

    pub fn contains_srf(&self) -> bool {
        match self {
            BoundExpr::Srf { .. } => true,
            BoundExpr::Unary { expr, .. }
            | BoundExpr::IsNull { expr, .. }
            | BoundExpr::Coerce { expr, .. }
            | BoundExpr::Collate { expr, .. }
            | BoundExpr::Reinterpret { expr, .. } => expr.contains_srf(),
            BoundExpr::Binary { left, right, .. } => left.contains_srf() || right.contains_srf(),
            BoundExpr::FuncCall { args, .. } | BoundExpr::Routine { args, .. } => {
                args.iter().any(BoundExpr::contains_srf)
            }
            BoundExpr::ArrayCtor { elems, .. } => elems.iter().any(BoundExpr::contains_srf),
            BoundExpr::Subscript { base, index, .. } => {
                base.contains_srf() || index.contains_srf()
            }
            BoundExpr::Case { whens, else_, .. } => {
                whens
                    .iter()
                    .any(|(condition, result)| condition.contains_srf() || result.contains_srf())
                    || else_.as_ref().is_some_and(|expr| expr.contains_srf())
            }
            // A subquery's own SRFs stay inside its subplan; nothing propagates
            // out to the enclosing projection.
            BoundExpr::QuantifiedSubquery { cmp, .. } => {
                cmp.contains_srf()
            }
            BoundExpr::QuantifiedArray { array, cmp, .. } => {
                array.contains_srf() || cmp.contains_srf()
            }
            BoundExpr::Const { .. }
            | BoundExpr::ColumnRef { .. }
            | BoundExpr::Param { .. }
            | BoundExpr::OuterColumnRef { .. }
            | BoundExpr::Aggregate { .. }
            | BoundExpr::ScalarSubquery { .. }
            | BoundExpr::Exists { .. } => false,
        }
    }

    /// Whether this expression tree calls a user-defined routine anywhere,
    /// including inside a subquery marker — a correlated subplan is executed
    /// per outer row, so a routine in one still runs under this statement.
    pub fn contains_routine(&self) -> bool {
        match self {
            BoundExpr::Routine { .. } => true,
            BoundExpr::Unary { expr, .. }
            | BoundExpr::IsNull { expr, .. }
            | BoundExpr::Coerce { expr, .. }
            | BoundExpr::Collate { expr, .. }
            | BoundExpr::Reinterpret { expr, .. } => expr.contains_routine(),
            BoundExpr::Binary { left, right, .. } => {
                left.contains_routine() || right.contains_routine()
            }
            BoundExpr::FuncCall { args, .. } | BoundExpr::Srf { args, .. } => {
                args.iter().any(BoundExpr::contains_routine)
            }
            BoundExpr::Aggregate { args, .. } => args.iter().any(BoundExpr::contains_routine),
            BoundExpr::ArrayCtor { elems, .. } => elems.iter().any(BoundExpr::contains_routine),
            BoundExpr::Subscript { base, index, .. } => {
                base.contains_routine() || index.contains_routine()
            }
            BoundExpr::Case { whens, else_, .. } => {
                whens.iter().any(|(condition, result)| {
                    condition.contains_routine() || result.contains_routine()
                }) || else_.as_ref().is_some_and(|expr| expr.contains_routine())
            }
            BoundExpr::ScalarSubquery { subplan, .. } | BoundExpr::Exists { subplan, .. } => {
                crate::plan::plan_calls_routine(&subplan.0)
            }
            BoundExpr::QuantifiedSubquery { subplan, cmp, .. } => {
                cmp.contains_routine() || crate::plan::plan_calls_routine(&subplan.0)
            }
            BoundExpr::QuantifiedArray { array, cmp, .. } => {
                array.contains_routine() || cmp.contains_routine()
            }
            BoundExpr::Const { .. }
            | BoundExpr::ColumnRef { .. }
            | BoundExpr::Param { .. }
            | BoundExpr::OuterColumnRef { .. } => false,
        }
    }

    /// Whether this node itself is an aggregate marker.
    pub fn is_aggregate(&self) -> bool {
        matches!(self, BoundExpr::Aggregate { .. })
    }

    /// Whether this expression tree contains an aggregate marker anywhere.
    pub fn contains_aggregate(&self) -> bool {
        match self {
            BoundExpr::Aggregate { .. } => true,
            BoundExpr::Const { .. }
            | BoundExpr::ColumnRef { .. }
            | BoundExpr::Param { .. }
            | BoundExpr::OuterColumnRef { .. } => false,
            BoundExpr::Unary { expr, .. } => expr.contains_aggregate(),
            BoundExpr::Binary { left, right, .. } => {
                left.contains_aggregate() || right.contains_aggregate()
            }
            BoundExpr::IsNull { expr, .. } => expr.contains_aggregate(),
            BoundExpr::Coerce { expr, .. } => expr.contains_aggregate(),
            BoundExpr::Collate { expr, .. } => expr.contains_aggregate(),
            BoundExpr::Reinterpret { expr, .. } => expr.contains_aggregate(),
            BoundExpr::FuncCall { args, .. }
            | BoundExpr::Routine { args, .. }
            | BoundExpr::Srf { args, .. } => args.iter().any(BoundExpr::contains_aggregate),
            BoundExpr::ArrayCtor { elems, .. } => elems.iter().any(BoundExpr::contains_aggregate),
            BoundExpr::Subscript { base, index, .. } => {
                base.contains_aggregate() || index.contains_aggregate()
            }
            BoundExpr::Case { whens, else_, .. } => {
                whens
                    .iter()
                    .any(|(c, r)| c.contains_aggregate() || r.contains_aggregate())
                    || else_.as_ref().is_some_and(|e| e.contains_aggregate())
            }
            // The needle (in `cmp`) is an outer expression, so an aggregate there
            // propagates; a subquery's own body is a separate query and doesn't.
            BoundExpr::QuantifiedSubquery { cmp, .. } => cmp.contains_aggregate(),
            BoundExpr::QuantifiedArray { array, cmp, .. } => {
                array.contains_aggregate() || cmp.contains_aggregate()
            }
            BoundExpr::ScalarSubquery { .. } | BoundExpr::Exists { .. } => false,
        }
    }

    /// Whether this expression contains a volatile function call. Today the only
    /// volatile [`ScalarFn`]s are the sequence functions (`nextval`/`setval` have
    /// side effects, `currval`/`lastval` read mutable session state) — all marked
    /// `VOLATILE` by PostgreSQL. Any future volatile scalar function (e.g.
    /// `random()`) must be added to the match here. Used to refuse duplicating a
    /// volatile argument when inlining a SQL function body.
    pub fn contains_volatile_fn(&self) -> bool {
        match self {
            BoundExpr::FuncCall { func, args, .. } => {
                matches!(
                    func,
                    ScalarFn::Nextval | ScalarFn::Currval | ScalarFn::Setval | ScalarFn::Lastval
                ) || args.iter().any(BoundExpr::contains_volatile_fn)
            }
            // A routine's body is opaque here and PostgreSQL defaults a
            // routine to VOLATILE, so treat every call as volatile.
            BoundExpr::Routine { .. } => true,
            BoundExpr::Srf { args, .. } => args.iter().any(BoundExpr::contains_volatile_fn),
            BoundExpr::ArrayCtor { elems, .. } => elems.iter().any(BoundExpr::contains_volatile_fn),
            BoundExpr::Subscript { base, index, .. } => {
                base.contains_volatile_fn() || index.contains_volatile_fn()
            }
            BoundExpr::Unary { expr, .. }
            | BoundExpr::IsNull { expr, .. }
            | BoundExpr::Coerce { expr, .. }
            | BoundExpr::Collate { expr, .. }
            | BoundExpr::Reinterpret { expr, .. } => expr.contains_volatile_fn(),
            BoundExpr::Binary { left, right, .. } => {
                left.contains_volatile_fn() || right.contains_volatile_fn()
            }
            BoundExpr::Case { whens, else_, .. } => {
                whens
                    .iter()
                    .any(|(c, r)| c.contains_volatile_fn() || r.contains_volatile_fn())
                    || else_.as_ref().is_some_and(|e| e.contains_volatile_fn())
            }
            BoundExpr::Aggregate { args, .. } => {
                args.iter().any(|a| a.contains_volatile_fn())
            }
            // A subquery's own body runs as a separate plan; only the outer needle
            // of an IN-subquery propagates to the enclosing expression.
            BoundExpr::QuantifiedSubquery { cmp, .. } => {
                cmp.contains_volatile_fn()
            }
            BoundExpr::QuantifiedArray { array, cmp, .. } => {
                array.contains_volatile_fn() || cmp.contains_volatile_fn()
            }
            BoundExpr::Const { .. }
            | BoundExpr::ColumnRef { .. }
            | BoundExpr::Param { .. }
            | BoundExpr::OuterColumnRef { .. }
            | BoundExpr::ScalarSubquery { .. }
            | BoundExpr::Exists { .. } => false,
        }
    }

    /// How many times a given `$n` ([`BoundExpr::Param`] with this 0-based
    /// `index`) occurs in this expression tree. Used to decide whether inlining a
    /// SQL function body would duplicate the evaluation of its `index`-th argument.
    pub fn count_param_refs(&self, index: usize) -> usize {
        match self {
            BoundExpr::Param { index: i, .. } => usize::from(*i == index),
            BoundExpr::Const { .. }
            | BoundExpr::ColumnRef { .. }
            | BoundExpr::OuterColumnRef { .. } => 0,
            BoundExpr::Unary { expr, .. }
            | BoundExpr::IsNull { expr, .. }
            | BoundExpr::Coerce { expr, .. }
            | BoundExpr::Collate { expr, .. }
            | BoundExpr::Reinterpret { expr, .. } => expr.count_param_refs(index),
            BoundExpr::Binary { left, right, .. } => {
                left.count_param_refs(index) + right.count_param_refs(index)
            }
            BoundExpr::FuncCall { args, .. }
            | BoundExpr::Routine { args, .. }
            | BoundExpr::Srf { args, .. } => args.iter().map(|a| a.count_param_refs(index)).sum(),
            BoundExpr::ArrayCtor { elems, .. } => {
                elems.iter().map(|a| a.count_param_refs(index)).sum()
            }
            BoundExpr::Subscript {
                base, index: idx, ..
            } => base.count_param_refs(index) + idx.count_param_refs(index),
            BoundExpr::Case { whens, else_, .. } => {
                whens
                    .iter()
                    .map(|(c, r)| c.count_param_refs(index) + r.count_param_refs(index))
                    .sum::<usize>()
                    + else_.as_ref().map_or(0, |e| e.count_param_refs(index))
            }
            BoundExpr::Aggregate { args, .. } => {
                args.iter().map(|a| a.count_param_refs(index)).sum()
            }
            BoundExpr::QuantifiedSubquery { cmp, .. } => {
                cmp.count_param_refs(index)
            }
            BoundExpr::QuantifiedArray { array, cmp, .. } => {
                array.count_param_refs(index) + cmp.count_param_refs(index)
            }
            // A validated scalar body carries no subquery; its params (if any)
            // live in a separate plan and are not this body's arguments.
            BoundExpr::ScalarSubquery { .. } | BoundExpr::Exists { .. } => 0,
        }
    }

    /// The inclusive `(min, max)` range of column indices this expression
    /// references, or `None` if it references no column (a constant/param
    /// expression). Callers that lay out rows as a concatenation — e.g. a join's
    /// `left || right` row — use this to decide which side an expression belongs
    /// to: a range wholly below the boundary is left-only, wholly at/above is
    /// right-only, and one that straddles it spans both.
    pub fn column_ref_bounds(&self) -> Option<(usize, usize)> {
        fn fold(expr: &BoundExpr, acc: &mut Option<(usize, usize)>) {
            match expr {
                BoundExpr::ColumnRef { index, .. } => {
                    *acc = Some(match *acc {
                        None => (*index, *index),
                        Some((lo, hi)) => (lo.min(*index), hi.max(*index)),
                    });
                }
                // An outer reference addresses an enclosing row, not this
                // (join) row's index space, so it contributes no bound here.
                BoundExpr::Const { .. }
                | BoundExpr::Param { .. }
                | BoundExpr::OuterColumnRef { .. } => {}
                BoundExpr::Unary { expr, .. }
                | BoundExpr::IsNull { expr, .. }
                | BoundExpr::Coerce { expr, .. }
                | BoundExpr::Collate { expr, .. }
                | BoundExpr::Reinterpret { expr, .. } => fold(expr, acc),
                BoundExpr::Binary { left, right, .. } => {
                    fold(left, acc);
                    fold(right, acc);
                }
                BoundExpr::FuncCall { args, .. }
                | BoundExpr::Routine { args, .. }
                | BoundExpr::Srf { args, .. } => {
                    args.iter().for_each(|a| fold(a, acc));
                }
                BoundExpr::ArrayCtor { elems, .. } => elems.iter().for_each(|a| fold(a, acc)),
                BoundExpr::Subscript { base, index, .. } => {
                    fold(base, acc);
                    fold(index, acc);
                }
                BoundExpr::Case { whens, else_, .. } => {
                    for (c, r) in whens {
                        fold(c, acc);
                        fold(r, acc);
                    }
                    if let Some(e) = else_ {
                        fold(e, acc);
                    }
                }
                BoundExpr::Aggregate { args, .. } => {
                    args.iter().for_each(|a| fold(a, acc));
                }
                // Non-correlated subplans reference no outer column; only the IN
                // needle (in `cmp`) can. Scalar/EXISTS contribute nothing.
                BoundExpr::QuantifiedSubquery { cmp, .. } => {
                    fold(cmp, acc)
                }
                BoundExpr::QuantifiedArray { array, cmp, .. } => {
                    fold(array, acc);
                    fold(cmp, acc);
                }
                BoundExpr::ScalarSubquery { .. } | BoundExpr::Exists { .. } => {}
            }
        }
        let mut acc = None;
        fold(self, &mut acc);
        acc
    }
}

/// A binding result: typed, or an untyped literal awaiting context (PG's
/// `unknown` pseudo-type). `lit == None` is the `NULL` literal; `span` locates
/// the literal for error positions.
#[derive(Clone, Debug, PartialEq)]
pub enum Binding {
    Typed(BoundExpr),
    /// An untyped literal, `NULL`, or a still-untyped bind parameter awaiting
    /// context. `param` is `Some((index, ctx))` when this unknown is a `$n`
    /// placeholder: resolving it records the deduced type in the shared context
    /// and produces a [`BoundExpr::Param`] instead of a folded `Const`.
    Unknown {
        lit: Option<String>,
        span: Span,
        param: Option<(usize, ParamCtx)>,
    },
}

/// One relation in a name-resolution scope: its qualifier (alias, else table
/// name), its columns, and the base index its columns occupy in the combined
/// row (0 for a single relation; the running total across FROM items in a
/// cross join).
#[derive(Clone)]
pub struct ScopeRel {
    qualifier: String,
    columns: Vec<Column>,
    offset: usize,
}

/// A snapshot of one enclosing query's name-resolution view, kept so a
/// correlated subquery can resolve an outer column: its relations (for qualified
/// `q.c` and the plain unqualified case) plus the merged-join `visible` view, so
/// an unqualified reference to a `USING`/`NATURAL` join column of an outer query
/// resolves to the single merged column (as in PG) rather than being reported
/// ambiguous. Level 1 is the immediate parent; deeper ancestors follow.
#[derive(Clone)]
pub(crate) struct OuterLevel {
    rels: Vec<ScopeRel>,
    visible: Option<Vec<VisibleColumn>>,
}

/// One column of a merged join namespace (`JOIN … USING` / `NATURAL JOIN`): the
/// name it is resolved by and the expression, over the combined row, that
/// produces its value. For inner/left joins that is the left input's column,
/// for right joins the right input's, and for full joins `COALESCE(left,
/// right)` — so the merged column is never the NULL-extended side.
#[derive(Clone)]
pub struct VisibleColumn {
    pub name: String,
    pub expr: BoundExpr,
}

/// The outcome of looking a name up in a merged-column view: exactly one match,
/// none, or several. Callers map each case to their own error wording (a bare
/// column reference vs. a `USING`/`NATURAL` join column).
pub(crate) enum VisibleLookup<'a> {
    Found(&'a BoundExpr),
    Missing,
    Ambiguous,
}

/// Find `name` among a merged view's columns, distinguishing no match from more
/// than one so the caller can raise the right `42703`/`42702`.
pub(crate) fn lookup_visible<'a>(cols: &'a [VisibleColumn], name: &str) -> VisibleLookup<'a> {
    let mut found: Option<&BoundExpr> = None;
    for col in cols {
        if col.name == name {
            if found.is_some() {
                return VisibleLookup::Ambiguous;
            }
            found = Some(&col.expr);
        }
    }
    match found {
        Some(expr) => VisibleLookup::Found(expr),
        None => VisibleLookup::Missing,
    }
}

/// The outcome of resolving a name against one query level's relations.
enum NameLookup {
    Found(BoundExpr),
    Ambiguous,
    Missing,
}

/// A column resolved by name: where it sits in the combined row, its type, and
/// its declared collation (`None` for the type default).
#[derive(Clone, Copy)]
struct ResolvedColumn {
    index: usize,
    ty: PgType,
    collation: Option<u32>,
}

/// Wrap a column reference in its declared collation, so the collation travels
/// with the expression the way an explicit `COLLATE` clause does — but at
/// *implicit* strength, which an explicit clause can still override. A column
/// on the type's default collation needs no wrapper.
pub(crate) fn with_column_collation(expr: BoundExpr, collation: Option<u32>) -> BoundExpr {
    match collation {
        Some(collation) => BoundExpr::Collate {
            expr: Box::new(expr),
            collation,
            explicit: false,
        },
        None => expr,
    }
}

/// Find `name` among `rels`' columns, returning it (`Ok`) — or `Err(())` for
/// more than one match (ambiguous), or `None` for no match. Shared by local and
/// outer (correlated) unqualified resolution.
fn lookup_in_rels(rels: &[ScopeRel], name: &str) -> Option<Result<ResolvedColumn, ()>> {
    let mut found: Option<ResolvedColumn> = None;
    for rel in rels {
        for (local, col) in rel.columns.iter().enumerate() {
            if col.name == name {
                if found.is_some() {
                    return Some(Err(()));
                }
                found = Some(ResolvedColumn {
                    index: rel.offset + local,
                    ty: col.ty,
                    collation: col.collation,
                });
            }
        }
    }
    found.map(Ok)
}

/// Resolve `column` within a single relation `rel` for a qualified
/// `qualifier.column` reference — its *local* index (the caller adds the
/// relation's offset), type, and collation. `42702` if the relation exposes the
/// name more than once (e.g. an alias list `v(x, x)`), or `42703` — spelled with
/// the qualifier, as PG does — if it is absent.
fn column_in_rel(
    rel: &ScopeRel,
    qualifier: &str,
    column: &str,
) -> Result<ResolvedColumn, BindError> {
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
    let local = local.ok_or_else(|| {
        BindError::new(
            sqlstate::UNDEFINED_COLUMN,
            format!("column {qualifier}.{column} does not exist"),
        )
    })?;
    Ok(ResolvedColumn {
        index: local,
        ty: rel.columns[local].ty,
        collation: rel.columns[local].collation,
    })
}

/// Rewrite every `ColumnRef` in `expr` into an `OuterColumnRef` at correlation
/// `level`, cloning everything else. Used to turn a merged-join `visible`
/// column's expression — a `ColumnRef` (inner/left/right join) or a full join's
/// `COALESCE`-as-`CASE` over both sides — into a correlated reference when it is
/// resolved from an enclosing query by an inner subquery.
fn outerize_columns(expr: &BoundExpr, level: usize) -> BoundExpr {
    match expr {
        BoundExpr::ColumnRef { index, ty } => BoundExpr::OuterColumnRef {
            level,
            index: *index,
            ty: *ty,
        },
        BoundExpr::Const { .. } | BoundExpr::Param { .. } | BoundExpr::OuterColumnRef { .. } => {
            expr.clone()
        }
        BoundExpr::Unary { op, expr } => BoundExpr::Unary {
            op: *op,
            expr: Box::new(outerize_columns(expr, level)),
        },
        BoundExpr::Collate {
            expr,
            collation,
            explicit,
        } => BoundExpr::Collate {
            expr: Box::new(outerize_columns(expr, level)),
            collation: *collation,
            explicit: *explicit,
        },
        BoundExpr::Binary {
            op,
            arg_ty,
            collation,
            left,
            right,
        } => BoundExpr::Binary {
            op: *op,
            arg_ty: *arg_ty,
            collation: *collation,
            left: Box::new(outerize_columns(left, level)),
            right: Box::new(outerize_columns(right, level)),
        },
        BoundExpr::IsNull { expr, negated } => BoundExpr::IsNull {
            expr: Box::new(outerize_columns(expr, level)),
            negated: *negated,
        },
        BoundExpr::Coerce { expr, ty } => BoundExpr::Coerce {
            expr: Box::new(outerize_columns(expr, level)),
            ty: *ty,
        },
        BoundExpr::Reinterpret {
            expr,
            reported,
            rep,
        } => BoundExpr::Reinterpret {
            expr: Box::new(outerize_columns(expr, level)),
            reported: *reported,
            rep: *rep,
        },
        BoundExpr::FuncCall { func, ret, args } => BoundExpr::FuncCall {
            func: *func,
            ret: *ret,
            args: args.iter().map(|a| outerize_columns(a, level)).collect(),
        },
        BoundExpr::Routine {
            oid,
            name,
            arg_types,
            strict,
            args,
            ret,
        } => BoundExpr::Routine {
            oid: *oid,
            name: Arc::clone(name),
            arg_types: Arc::clone(arg_types),
            strict: *strict,
            args: args.iter().map(|a| outerize_columns(a, level)).collect(),
            ret: *ret,
        },

        BoundExpr::ArrayCtor { elem, ty, elems } => BoundExpr::ArrayCtor {
            elem: *elem,
            ty: *ty,
            elems: elems.iter().map(|a| outerize_columns(a, level)).collect(),
        },
        BoundExpr::Subscript { base, index, ty } => BoundExpr::Subscript {
            base: Box::new(outerize_columns(base, level)),
            index: Box::new(outerize_columns(index, level)),
            ty: *ty,
        },
        BoundExpr::Case { whens, else_, ty } => BoundExpr::Case {
            whens: whens
                .iter()
                .map(|(c, r)| (outerize_columns(c, level), outerize_columns(r, level)))
                .collect(),
            else_: else_
                .as_ref()
                .map(|e| Box::new(outerize_columns(e, level))),
            ty: *ty,
        },
        // A merged-join visible column expression is only ever a ColumnRef or a
        // COALESCE/CASE over ColumnRefs; these never appear, so clone defensively.
        BoundExpr::Srf { .. }
        | BoundExpr::Aggregate { .. }
        | BoundExpr::ScalarSubquery { .. }
        | BoundExpr::Exists { .. }
        | BoundExpr::QuantifiedSubquery { .. }
        | BoundExpr::QuantifiedArray { .. } => expr.clone(),
    }
}

/// Name-resolution scope: an ordered list of relations (empty for a FROM-less
/// SELECT / INSERT VALUES, one for a single-table SELECT, more for a cross
/// join). A qualified reference addresses one relation by its name or alias;
/// with an alias the bare table name is not a valid qualifier — as in PG.
pub struct Scope {
    rels: Vec<ScopeRel>,
    /// The unqualified-resolution and `*`-expansion view when a `USING`/`NATURAL`
    /// join has merged columns. `None` keeps the plain behavior (every relation's
    /// columns in order); `Some` lists the visible columns exactly, with join
    /// columns merged once. Qualified `q.c` references always use `rels`, so each
    /// side's own copy of a join column stays addressable.
    visible: Option<Vec<VisibleColumn>>,
    /// User-defined type/cast view, so an expression cast to/from a `CREATE TYPE`
    /// name resolves and a `WITHOUT FUNCTION` cast can be applied.
    catalog: Arc<dyn TypeCatalog>,
    /// The statement's shared bind-parameter context. The same handle flows into
    /// every nested scope (subqueries, CTEs, derived tables) so a `$n` unifies
    /// its type across the whole statement.
    params: ParamCtx,
    /// What an expression subquery (`(SELECT …)`, `EXISTS`, `IN (SELECT …)`)
    /// needs to bind its body: the table engine (to resolve scans) and the CTEs
    /// visible at this query level. `None` in contexts where a subquery cannot
    /// appear (column defaults, INSERT VALUES rows), which then reject one.
    subquery: Option<std::rc::Rc<SubqueryContext>>,
    /// The enclosing queries' resolution views, nearest first (index 0 = the
    /// immediate parent = correlation level 1). Empty for a top-level query;
    /// populated by [`Scope::with_outer`] when binding a subquery body so an
    /// unresolved name can fall through to the outer scope as an
    /// [`BoundExpr::OuterColumnRef`].
    outer: Vec<OuterLevel>,
}

/// The handle a [`Scope`] carries so `bind_expr` can bind a nested query. Shared
/// (`Rc`) so cheaply threaded into the transient scopes built per clause.
pub(crate) struct SubqueryContext {
    engine: Arc<dyn TableEngine>,
    ctes: crate::plan::CteEnv,
}

impl Scope {
    /// No tables in scope: FROM-less SELECT, INSERT VALUES.
    pub fn empty(catalog: &Arc<dyn TypeCatalog>, params: &ParamCtx) -> Scope {
        Scope {
            rels: Vec::new(),
            visible: None,
            catalog: catalog.clone(),
            params: params.clone(),
            subquery: None,
            outer: Vec::new(),
        }
    }

    pub fn table(
        schema: &TableSchema,
        qualifier: String,
        catalog: &Arc<dyn TypeCatalog>,
        params: &ParamCtx,
    ) -> Scope {
        Scope {
            rels: vec![ScopeRel {
                qualifier,
                columns: schema.columns.clone(),
                offset: 0,
            }],
            visible: None,
            catalog: catalog.clone(),
            params: params.clone(),
            subquery: None,
            outer: Vec::new(),
        }
    }

    /// A multi-relation scope for a cross join. Each `(qualifier, columns)` pair
    /// becomes a relation; offsets are assigned left-to-right so a column's
    /// index is its position in the concatenated row.
    pub fn relations(
        items: Vec<(String, Vec<Column>)>,
        catalog: &Arc<dyn TypeCatalog>,
        params: &ParamCtx,
    ) -> Scope {
        Self::relations_with_visible(items, None, catalog, params)
    }

    /// Like [`Scope::relations`], but with an explicit merged-column view for a
    /// FROM clause that contains a `USING`/`NATURAL` join. The `rels` are still
    /// built from `items` (so qualified `q.c` resolves each side's own column);
    /// `visible`, when `Some`, drives unqualified resolution and `*` expansion.
    pub fn relations_with_visible(
        items: Vec<(String, Vec<Column>)>,
        visible: Option<Vec<VisibleColumn>>,
        catalog: &Arc<dyn TypeCatalog>,
        params: &ParamCtx,
    ) -> Scope {
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
        Scope {
            rels,
            visible,
            catalog: catalog.clone(),
            params: params.clone(),
            subquery: None,
            outer: Vec::new(),
        }
    }

    /// Attach the context needed to bind expression subqueries against this
    /// scope's query level: the table engine and the visible CTEs. Set by the
    /// clause binders (SELECT projection/WHERE/HAVING, JOIN ON) that have both in
    /// hand; scopes without it reject a subquery as unsupported in that position.
    pub(crate) fn with_subqueries(
        mut self,
        engine: &Arc<dyn TableEngine>,
        ctes: &crate::plan::CteEnv,
    ) -> Scope {
        self.subquery = Some(std::rc::Rc::new(SubqueryContext {
            engine: engine.clone(),
            ctes: ctes.clone(),
        }));
        self
    }

    /// Attach the enclosing queries' resolution views so a correlated reference
    /// inside this (subquery) scope can fall through to an outer column. Set by
    /// the clause binders when binding a subquery body, from
    /// [`Scope::as_outer_levels`] on the enclosing scope.
    pub(crate) fn with_outer(mut self, outer: Vec<OuterLevel>) -> Scope {
        self.outer = outer;
        self
    }

    /// The outer-level views a subquery bound against this scope should see:
    /// this scope's own relations as level 1, then this scope's own outer levels
    /// (the ancestors) shifted one deeper.
    pub(crate) fn as_outer_levels(&self) -> Vec<OuterLevel> {
        let mut levels = Vec::with_capacity(self.outer.len() + 1);
        levels.push(OuterLevel {
            rels: self.rels.clone(),
            visible: self.visible.clone(),
        });
        levels.extend(self.outer.iter().cloned());
        levels
    }

    /// The user-defined type/cast view carried through binding.
    pub fn catalog(&self) -> &Arc<dyn TypeCatalog> {
        &self.catalog
    }

    /// The statement's shared bind-parameter context.
    pub fn params(&self) -> &ParamCtx {
        &self.params
    }

    /// Resolve an unqualified column name. A name that matches exactly one
    /// column binds to its combined-row index; more than one match — whether
    /// across relations or duplicated within one (e.g. an alias list `v(x, x)`)
    /// — is `42702` (ambiguous); no match here nor in any enclosing query is
    /// `42703`. A name that matches no local column but one in an enclosing
    /// query (a correlated reference) resolves to that column via an
    /// [`BoundExpr::OuterColumnRef`], preferring the nearest scope — as in PG.
    fn resolve(&self, name: &str) -> Result<BoundExpr, BindError> {
        match self.resolve_local(name) {
            NameLookup::Found(expr) => Ok(expr),
            NameLookup::Ambiguous => Err(BindError::new(
                sqlstate::AMBIGUOUS_COLUMN,
                format!("column reference \"{name}\" is ambiguous"),
            )),
            NameLookup::Missing => self.resolve_outer(name),
        }
    }

    /// Look a name up in this scope's own relations (or its merged-join `visible`
    /// view), without consulting enclosing queries.
    fn resolve_local(&self, name: &str) -> NameLookup {
        // A merged join namespace resolves unqualified names against its visible
        // columns: the join column appears once (never ambiguous), the merged
        // expression carrying its combined-row value.
        if let Some(visible) = &self.visible {
            return match lookup_visible(visible, name) {
                VisibleLookup::Found(expr) => NameLookup::Found(expr.clone()),
                VisibleLookup::Ambiguous => NameLookup::Ambiguous,
                VisibleLookup::Missing => NameLookup::Missing,
            };
        }
        match lookup_in_rels(&self.rels, name) {
            Some(Ok(col)) => NameLookup::Found(with_column_collation(
                BoundExpr::ColumnRef {
                    index: col.index,
                    ty: col.ty,
                },
                col.collation,
            )),
            Some(Err(())) => NameLookup::Ambiguous,
            None => NameLookup::Missing,
        }
    }

    /// Walk the enclosing queries (nearest first) for a name unresolved locally,
    /// producing a correlated [`BoundExpr::OuterColumnRef`] at the first level
    /// that has it. Outer levels resolve against their relations (a merged-join
    /// `visible` view is not consulted across a correlation boundary); `42703` if
    /// no enclosing query defines the name.
    fn resolve_outer(&self, name: &str) -> Result<BoundExpr, BindError> {
        let ambiguous = || {
            BindError::new(
                sqlstate::AMBIGUOUS_COLUMN,
                format!("column reference \"{name}\" is ambiguous"),
            )
        };
        for (depth, level) in self.outer.iter().enumerate() {
            let level_no = depth + 1;
            // A `USING`/`NATURAL` join in the outer query merges its join column
            // into one `visible` entry; resolve against it first so a correlated
            // unqualified reference to that column is not seen as ambiguous across
            // the two physical sides still present in `rels`.
            if let Some(visible) = &level.visible {
                match lookup_visible(visible, name) {
                    VisibleLookup::Found(expr) => return Ok(outerize_columns(expr, level_no)),
                    VisibleLookup::Ambiguous => return Err(ambiguous()),
                    VisibleLookup::Missing => continue,
                }
            }
            match lookup_in_rels(&level.rels, name) {
                Some(Ok(col)) => {
                    return Ok(with_column_collation(
                        BoundExpr::OuterColumnRef {
                            level: level_no,
                            index: col.index,
                            ty: col.ty,
                        },
                        col.collation,
                    ));
                }
                Some(Err(())) => return Err(ambiguous()),
                None => continue,
            }
        }
        Err(BindError::new(
            sqlstate::UNDEFINED_COLUMN,
            format!("column \"{name}\" does not exist"),
        ))
    }

    /// Resolve a qualified `qualifier.column` reference. A relation matching
    /// `qualifier` in this scope binds to a plain [`BoundExpr::ColumnRef`]; one
    /// found only in an enclosing query (nearest first) is a correlated
    /// [`BoundExpr::OuterColumnRef`]. `42P01` if no relation named `qualifier` is
    /// in scope at any level; `42703` if the relation is found but lacks the
    /// column.
    fn resolve_qualified(&self, qualifier: &str, column: &str) -> Result<BoundExpr, BindError> {
        if let Some(rel) = self.rels.iter().find(|r| r.qualifier == qualifier) {
            let col = column_in_rel(rel, qualifier, column)?;
            return Ok(with_column_collation(
                BoundExpr::ColumnRef {
                    index: rel.offset + col.index,
                    ty: col.ty,
                },
                col.collation,
            ));
        }
        for (depth, level) in self.outer.iter().enumerate() {
            if let Some(rel) = level.rels.iter().find(|r| r.qualifier == qualifier) {
                let col = column_in_rel(rel, qualifier, column)?;
                return Ok(with_column_collation(
                    BoundExpr::OuterColumnRef {
                        level: depth + 1,
                        index: rel.offset + col.index,
                        ty: col.ty,
                    },
                    col.collation,
                ));
            }
        }
        Err(BindError::new(
            sqlstate::UNDEFINED_TABLE,
            format!("missing FROM-clause entry for table \"{qualifier}\""),
        ))
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
        // With a merged join namespace, `*` follows the visible columns: merged
        // join columns first (in clause order), then each input's remaining
        // columns — the join column appearing exactly once, as in PG.
        if let Some(visible) = &self.visible {
            return visible
                .iter()
                .map(|col| {
                    let (collation, strength) = crate::collation::output_collation(&col.expr);
                    (
                        crate::OutputColumn {
                            name: col.name.clone(),
                            ty: col.expr.ty(),
                            collation,
                            strength,
                        },
                        col.expr.clone(),
                    )
                })
                .collect();
        }
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
                collation: col.collation,
                // A resolved column reference is always implicit strength in
                // PG's model, whether or not its collation happens to equal
                // the type default (mirroring `expr_collation`'s `ColumnRef`
                // arm).
                strength: crate::collation::Strength::Implicit,
            },
            with_column_collation(
                BoundExpr::ColumnRef {
                    index: rel.offset + i,
                    ty: col.ty,
                },
                col.collation,
            ),
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
        ast::Expr::Value(v) => bind_value(v, scope),
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
        ast::Expr::BinaryOp {
            left,
            op,
            right,
            op_span,
        } => bind_binary(left, op, right, op_span.0, scope),
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
        ast::Expr::AtTimeZone {
            timestamp,
            time_zone,
        } => bind_at_time_zone(timestamp, time_zone, scope),
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
        ast::Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => bind_substring(
            expr,
            substring_from.as_deref(),
            substring_for.as_deref(),
            scope,
        ),
        ast::Expr::Trim {
            expr,
            trim_where,
            trim_what,
            trim_characters,
        } => bind_trim(
            expr,
            *trim_where,
            trim_what.as_deref(),
            trim_characters.as_deref(),
            scope,
        ),
        ast::Expr::Position { expr, r#in } => {
            // POSITION(sub IN str) == strpos(str, sub).
            let sub = bind_expr(expr, scope)?;
            let str_ = bind_expr(r#in, scope)?;
            crate::functions::resolve_call("strpos", vec![str_, sub], scope.catalog())
        }
        ast::Expr::Overlay {
            expr,
            overlay_what,
            overlay_from,
            overlay_for,
        } => bind_overlay(
            expr,
            overlay_what,
            overlay_from,
            overlay_for.as_deref(),
            scope,
        ),
        ast::Expr::Like {
            negated,
            any,
            expr,
            pattern,
            escape_char,
        } => bind_like_node(
            expr,
            pattern,
            escape_char.as_ref(),
            *any,
            false,
            *negated,
            scope,
        ),
        ast::Expr::ILike {
            negated,
            any,
            expr,
            pattern,
            escape_char,
        } => bind_like_node(
            expr,
            pattern,
            escape_char.as_ref(),
            *any,
            true,
            *negated,
            scope,
        ),
        ast::Expr::SimilarTo {
            negated,
            expr,
            pattern,
            escape_char,
        } => bind_similar_to(expr, pattern, escape_char.as_ref(), *negated, scope),
        ast::Expr::InList {
            expr,
            list,
            negated,
        } => bind_in_list(expr, list, *negated, scope),
        ast::Expr::Subquery(query) => bind_scalar_subquery(query, scope),
        ast::Expr::Exists { subquery, negated } => bind_exists(subquery, *negated, scope),
        ast::Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => bind_in_subquery(expr, subquery, *negated, scope),
        // `left op ANY(…)` / `left op SOME(…)` (SOME ≡ ANY, so `is_some` doesn't
        // affect binding) and `left op ALL(…)`.
        ast::Expr::AnyOp {
            left,
            compare_op,
            right,
            op_span,
            ..
        } => bind_quantified(left, compare_op, right, false, op_span.0, scope),
        ast::Expr::AllOp {
            left,
            compare_op,
            right,
            op_span,
        } => bind_quantified(left, compare_op, right, true, op_span.0, scope),
        ast::Expr::Between {
            expr,
            negated,
            low,
            high,
        } => bind_between(expr, low, high, *negated, scope),
        // `ARRAY[...]` / `[...]` array constructor.
        ast::Expr::Array(arr) => bind_array_ctor(&arr.elem, scope),
        // `a[i]` array element access.
        ast::Expr::CompoundFieldAccess { root, access_chain } => {
            bind_subscript(root, access_chain, scope)
        }
        ast::Expr::Collate { expr, collation } => bind_collate(expr, collation, scope),
        other => Err(unsupported_expr(other)),
    }
}

/// Bind `expr COLLATE "name"`.
///
/// The clause only labels the operand — the value is unchanged — so the result
/// keeps the operand's type and the collation rides along in a
/// [`BoundExpr::Collate`] at *explicit* strength, overriding any collation the
/// operand already carried. An untyped literal (`'x' COLLATE "C"`) settles on
/// `text`, as PG does, since the clause proves it is a string.
fn bind_collate(
    expr: &ast::Expr,
    collation: &ast::ObjectName,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let oid = crate::collation::resolve_collation(collation)?;
    let bound = match bind_expr(expr, scope)? {
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, PgType::Text)?,
        Binding::Typed(e) => e,
    };
    let ty = bound.ty();
    if !ty.is_collatable() {
        return Err(BindError::new(
            sqlstate::WRONG_OBJECT_TYPE,
            format!("collations are not supported by type {}", ty.name()),
        ));
    }
    Ok(Binding::Typed(BoundExpr::Collate {
        expr: Box::new(bound),
        collation: oid,
        explicit: true,
    }))
}

/// Bind an `ARRAY[...]` constructor. Elements are bound and unified to a common
/// element type (untyped literals adapt to it), then coerced; the result type is
/// `PgType::Array(elem)`. An empty `ARRAY[]` settles on `text[]` and typically
/// takes its real type from a surrounding cast (`ARRAY[]::int[]`).
fn bind_array_ctor(elems: &[ast::Expr], scope: &Scope) -> Result<Binding, BindError> {
    // A bare, uncast `ARRAY[]` has no determinable element type. PG requires an
    // explicit cast; `ARRAY[]::t[]` is intercepted in `bind_cast` and never
    // reaches here empty.
    if elems.is_empty() {
        return Err(BindError::new(
            "42P18",
            "cannot determine type of empty array",
        )
        .with_hint(Some(
            "Explicitly cast to the desired type, for example ARRAY[]::integer[]."
                .to_string(),
        )));
    }
    let bindings = elems
        .iter()
        .map(|e| bind_expr(e, scope))
        .collect::<Result<Vec<_>, _>>()?;
    let (elem, exprs) = unify_value_column(bindings, "ARRAY")?;
    // Reject an element type this build has no array type for — this also
    // rejects a multi-dimensional constructor (an array-typed element).
    if crabgresql_types::array::array_oid_for_elem(elem.oid()).is_none() {
        return Err(BindError::feature_not_supported(format!(
            "could not find array type for data type {}",
            elem.name()
        )));
    }
    if elem.is_collatable() {
        crate::collation::check_explicit_conflict(
            exprs.iter().map(crate::collation::expr_collation),
        )?;
    }
    Ok(Binding::Typed(BoundExpr::ArrayCtor {
        elem,
        ty: PgType::Array(elem.oid()),
        elems: exprs,
    }))
}

/// Bind an `a[i]` subscript. Only a single integer index on an array is
/// supported (slices and chained/multi-dim subscripts are `0A000`). The result
/// type is the array's element type.
fn bind_subscript(
    root: &ast::Expr,
    access_chain: &[ast::AccessExpr],
    scope: &Scope,
) -> Result<Binding, BindError> {
    let index_expr = match access_chain {
        [ast::AccessExpr::Subscript(ast::Subscript::Index { index })] => index,
        [ast::AccessExpr::Subscript(ast::Subscript::Slice { .. })] => {
            return Err(BindError::feature_not_supported(
                "array slice access is not supported yet",
            ));
        }
        // Chained/multi-dimensional subscripts and dotted field access.
        _ => {
            return Err(BindError::feature_not_supported(
                "multi-dimensional or field subscripting is not supported yet",
            ));
        }
    };
    let base = bind_scalar(root, scope)?;
    let elem = match base.ty() {
        PgType::Array(elem_oid) => PgType::from_oid(elem_oid).ok_or_else(|| {
            BindError::feature_not_supported("subscripting this array type is not supported yet")
        })?,
        other => {
            return Err(BindError::new(
                sqlstate::DATATYPE_MISMATCH,
                format!(
                    "cannot subscript type {} because it does not support subscripting",
                    other.name()
                ),
            ));
        }
    };
    let index = coerce_expr(bind_scalar(index_expr, scope)?, PgType::Int4)?;
    Ok(Binding::Typed(BoundExpr::Subscript {
        base: Box::new(base),
        index: Box::new(index),
        ty: elem,
    }))
}

/// Bind a nested query into a [`LogicalPlan`] against the enclosing scope's
/// subquery context (table engine + visible CTEs). The subquery body is bound
/// in its own name scope, but with the enclosing scope's relations attached as
/// outer levels ([`Scope::as_outer_levels`]) so a correlated reference resolves
/// outward to a [`BoundExpr::OuterColumnRef`]; a name in neither still errors
/// `42703`.
fn bind_subquery_plan(
    query: &ast::Query,
    scope: &Scope,
) -> Result<(crate::plan::LogicalPlan, Vec<crate::OutputColumn>), BindError> {
    let ctx = scope.subquery.as_ref().ok_or_else(|| {
        BindError::feature_not_supported("subqueries are not supported in this context")
    })?;
    let plan = crate::plan::bind_query_scoped(
        &ctx.engine,
        &scope.catalog,
        &scope.params,
        query,
        &ctx.ctes,
        &scope.as_outer_levels(),
    )?;
    let columns = crate::plan::output_columns_of(&plan)?;
    Ok((plan, columns))
}

/// `(SELECT …)` as a scalar: the subquery must produce exactly one column; its
/// type is the expression's type. Runs once at execution and folds to that
/// value (0 rows → NULL, >1 rows → `21000`).
fn bind_scalar_subquery(query: &ast::Query, scope: &Scope) -> Result<Binding, BindError> {
    let (plan, columns) = bind_subquery_plan(query, scope)?;
    let [col] = columns.as_slice() else {
        return Err(BindError::new(
            sqlstate::SYNTAX_ERROR,
            "subquery must return only one column",
        ));
    };
    Ok(Binding::Typed(BoundExpr::ScalarSubquery {
        subplan: Subplan(Box::new(plan)),
        ty: col.ty,
    }))
}

/// `[NOT] EXISTS (SELECT …)` → a bool test on whether the subquery yields rows.
/// The projected columns are irrelevant (PG ignores them), so the target list is
/// replaced with a constant: the executor then only checks for a first row and
/// never evaluates the original projection (which could error or be expensive).
fn bind_exists(query: &ast::Query, negated: bool, scope: &Scope) -> Result<Binding, BindError> {
    let (plan, _columns) = bind_subquery_plan(query, scope)?;
    Ok(Binding::Typed(BoundExpr::Exists {
        subplan: Subplan(Box::new(crate::plan::strip_to_existence(plan))),
        negated,
    }))
}

/// `x [NOT] IN (SELECT …)`, which PostgreSQL defines as exactly `x = ANY (…)` /
/// `x <> ALL (…)` — so it binds to the same [`BoundExpr::QuantifiedSubquery`]
/// the `ANY`/`ALL` spellings produce. `NOT IN` becomes `<> ALL` rather than a
/// negated `= ANY`: the De Morgan dual keeps three-valued NULL handling right
/// without a wrapping `NOT` (mirroring how `bind_in_list` picks `(NotEq, And)`).
fn bind_in_subquery(
    expr: &ast::Expr,
    query: &ast::Query,
    negated: bool,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let op = if negated { BinOp::NotEq } else { BinOp::Eq };
    // `IN` has no operator token of its own to point a cursor at, so the
    // comparison resolves with an empty span, as it did before.
    bind_quantified_subquery(expr, op, query, negated, Span::empty(), scope)
}

/// `left op ANY(…)` / `left op SOME(…)` / `left op ALL(…)` (`all` selects `ALL`).
/// The right-hand operand is either a subquery (`ANY(SELECT …)`, →
/// [`BoundExpr::QuantifiedSubquery`]) or an array-valued expression
/// (`ANY(ARRAY[…])`, `ANY('{…}')`, → [`BoundExpr::QuantifiedArray`]; a `$n`
/// array parameter binds here too, but only reaches execution over the simple
/// protocol until `types::wire` gains a binary array decoder).
/// In both cases a NULL `Const` "hole" of the element type stands in for a
/// candidate and `bind_binary_op` resolves the operator/coercions exactly as a
/// written `left op v` would (the same trick as [`bind_in_subquery`]).
fn bind_quantified(
    left: &ast::Expr,
    compare_op: &ast::BinaryOperator,
    right: &ast::Expr,
    all: bool,
    op_span: Span,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let Some(op) = binop_from_comparison(compare_op) else {
        // The parser also accepts the LIKE/regex operator spellings after
        // ANY/ALL. Those lower to `ScalarFn` calls, not a `Binary` comparison
        // template, so the quantified path can't build a hole for them yet.
        return Err(BindError::feature_not_supported(format!(
            "{compare_op} {} (…) is not supported yet",
            if all { "ALL" } else { "ANY" }
        ))
        .at(op_span));
    };

    // The parser emits `Expr::Subquery` for the `ANY(SELECT …)` form (possibly
    // wrapped in redundant parentheses); anything else is an array expression.
    let mut rhs = right;
    while let ast::Expr::Nested(inner) = rhs {
        rhs = inner;
    }
    match rhs {
        ast::Expr::Subquery(query) => {
            bind_quantified_subquery(left, op, query, all, op_span, scope)
        }
        _ => bind_quantified_array(left, op, right, all, op_span, scope),
    }
}

/// The comparison subset of the `ast::BinaryOperator` → [`BinOp`] mapping (the
/// only operators a quantified comparison accepts). Shared with `bind_binary` so
/// a new comparison spelling can never bind for `a < b` but not `a < ANY(…)`.
fn binop_from_comparison(op: &ast::BinaryOperator) -> Option<BinOp> {
    Some(match op {
        ast::BinaryOperator::Eq => BinOp::Eq,
        ast::BinaryOperator::NotEq => BinOp::NotEq,
        ast::BinaryOperator::Lt => BinOp::Lt,
        ast::BinaryOperator::LtEq => BinOp::LtEq,
        ast::BinaryOperator::Gt => BinOp::Gt,
        ast::BinaryOperator::GtEq => BinOp::GtEq,
        _ => return None,
    })
}

/// The subquery form of [`bind_quantified`]: the one-column subquery supplies
/// the candidate set. Also serves `x [NOT] IN (SELECT …)` via
/// [`bind_in_subquery`]. A subquery with more than one column errors.
fn bind_quantified_subquery(
    left: &ast::Expr,
    op: BinOp,
    query: &ast::Query,
    all: bool,
    op_span: Span,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let (plan, columns) = bind_subquery_plan(query, scope)?;
    let [col] = columns.as_slice() else {
        return Err(BindError::new(
            sqlstate::SYNTAX_ERROR,
            "subquery has too many columns",
        ));
    };
    let elem_ty = col.ty;
    let needle = bind_expr(left, scope)?;
    let cmp = bind_hole_template(op, needle, elem_ty, col.collation, op_span, scope)?;
    Ok(Binding::Typed(BoundExpr::QuantifiedSubquery {
        subplan: Subplan(Box::new(plan)),
        all,
        cmp: Box::new(cmp),
    }))
}

/// The array form of [`bind_quantified`]. The element type comes from the
/// right-hand array: a typed array contributes its element type; an untyped
/// literal (`'{1,2,3}'`) or bind parameter takes the needle's type (`text` when
/// the needle too is untyped) and is coerced to that array type — mirroring
/// [`bind_in_list`]'s unknown-literal policy. A right side that is not an array
/// (or whose element type has no `PgType`) is PG's `op ANY/ALL (array) requires
/// array on right side` error.
fn bind_quantified_array(
    left: &ast::Expr,
    op: BinOp,
    right: &ast::Expr,
    all: bool,
    op_span: Span,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let needle = bind_expr(left, scope)?;
    let array = bind_expr(right, scope)?;
    let elem_ty = match binding_typed_ty(&array) {
        Some(ty) => ty.array_element().ok_or_else(|| {
            BindError::new(
                sqlstate::WRONG_OBJECT_TYPE,
                "op ANY/ALL (array) requires array on right side",
            )
            .at(op_span)
        })?,
        // Untyped literal / bind parameter: element type follows the needle.
        None => binding_typed_ty(&needle).unwrap_or(PgType::Text),
    };
    // Coerce the right side to `elem_ty[]` (identity for an already-typed array;
    // parses `'{…}'` / types a `$n` param via `resolve_unknown`'s array arm).
    let array_expr = resolve_operand(&array, PgType::Array(elem_ty.oid()))?;
    // `PgType::Array` is never itself collatable (only the element is), and
    // nothing in this build tracks a per-element collation on an array value
    // yet, so the hole falls back to the element type's default collation —
    // unlike the subquery form, which does know its one column's collation.
    let cmp = bind_hole_template(op, needle, elem_ty, None, op_span, scope)?;
    Ok(Binding::Typed(BoundExpr::QuantifiedArray {
        array: Box::new(array_expr),
        all,
        cmp: Box::new(cmp),
    }))
}

/// Build a quantified comparison's `needle op <hole>` template, where `<hole>`
/// is a NULL `Const` of the candidate type. Binding it through
/// [`bind_binary_op`] resolves the operator, operand promotion and every
/// coercion exactly as a written `needle op candidate` would — and raises PG's
/// `operator does not exist` (pointed at `op_span`) when there is none. The
/// executor substitutes each candidate into that hole.
fn bind_hole_template(
    op: BinOp,
    needle: Binding,
    elem_ty: PgType,
    collation: Option<u32>,
    op_span: Span,
    scope: &Scope,
) -> Result<BoundExpr, BindError> {
    // Geometric comparisons exist in PG but lower to `ScalarFn::Geo` calls here,
    // not to a `Binary` with a substitutable RHS hole. `bind_binary_op` would
    // report "operator does not exist", which is untrue — the operator exists,
    // the quantified form just can't build a template for it yet.
    if is_geo_ty(Some(elem_ty)) || is_geo_ty(binding_typed_ty(&needle)) {
        return Err(BindError::feature_not_supported(format!(
            "{} ANY/ALL (…) on geometric types is not supported yet",
            op.sql_symbol()
        ))
        .at(op_span));
    }
    let placeholder = BoundExpr::Const {
        value: Value::Null,
        ty: elem_ty,
    };
    // Wrap the placeholder so `expr_collation` sees the candidate set's real
    // collation rather than a bare NULL's (which asserts none), the same way
    // a column reference of `elem_ty` would if we had a real one to bind.
    let hole = Binding::Typed(match collation {
        Some(collation) if elem_ty.is_collatable() => BoundExpr::Collate {
            expr: Box::new(placeholder),
            collation,
            explicit: false,
        },
        _ => placeholder,
    });
    let cmp = bind_binary_op(op, needle, hole, op_span, scope.catalog().as_ref())?;
    match cmp {
        Binding::Typed(cmp @ BoundExpr::Binary { .. }) => Ok(cmp),
        // Any other comparison that lowers to a `ScalarFn` likewise has no hole
        // to substitute into; fail here rather than leaving the executor to trip
        // over a template shape it cannot destructure.
        _ => Err(BindError::feature_not_supported(format!(
            "{} ANY/ALL (…) on type {} is not supported yet",
            op.sql_symbol(),
            elem_ty.name()
        ))
        .at(op_span)),
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
        None => Binding::Typed(BoundExpr::Const {
            value: Value::Int4(1),
            ty: PgType::Int4,
        }),
    };
    let mut args = vec![subject, start];
    if let Some(e) = for_ {
        args.push(bind_expr(e, scope)?);
    }
    crate::functions::resolve_call("substr", args, scope.catalog())
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
    crate::functions::resolve_call(func, args, scope.catalog())
}

/// `OVERLAY(x PLACING r FROM a [FOR b])` → `overlay(x, r, a[, b])`.
fn bind_overlay(
    expr: &ast::Expr,
    what: &ast::Expr,
    from: &ast::Expr,
    for_: Option<&ast::Expr>,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let mut args = vec![
        bind_expr(expr, scope)?,
        bind_expr(what, scope)?,
        bind_expr(from, scope)?,
    ];
    if let Some(e) = for_ {
        args.push(bind_expr(e, scope)?);
    }
    crate::functions::resolve_call("overlay", args, scope.catalog())
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
        return Err(BindError::feature_not_supported(
            "LIKE ANY is not supported yet",
        ));
    }
    let lb = bind_expr(expr, scope)?;
    let rb = bind_expr(pattern, scope)?;
    let escape = match escape_char {
        Some(v) => match &v.value {
            ast::Value::SingleQuotedString(s) => Some(Binding::Typed(BoundExpr::Const {
                value: Value::Text(s.clone()),
                ty: PgType::Text,
            })),
            other => {
                return Err(BindError::syntax(format!(
                    "invalid ESCAPE literal: {other}"
                )));
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
        // A bare untyped literal defaults to text; but a bind parameter with no
        // surrounding context has no type to take, so PG errors 42P18 rather
        // than silently choosing text.
        Binding::Unknown {
            param: Some((index, _)),
            ..
        } => {
            return Err(BindError::new(
                "42P18",
                format!("could not determine data type of parameter ${}", index + 1),
            ));
        }
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, PgType::Text)?,
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
pub(crate) fn is_orderable(ty: PgType, catalog: &dyn TypeCatalog) -> bool {
    // An array is orderable/comparable iff its element type is (element-wise
    // comparison). Keep in sync with the executor's `compare_values`.
    if let PgType::Array(elem_oid) = ty {
        return PgType::from_oid(elem_oid).is_some_and(|e| is_orderable(e, catalog));
    }
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
            // A reg* value compares by the OID it holds, never by the name it
            // renders as — see `compare_values` in the executor.
            | PgType::Reg(_)
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
            // `jsonb` has a total order (`compareJsonbContainers`); plain `json`
            // has no default equality/ordering, so it is intentionally omitted.
            | PgType::Jsonb
            // Both text-search types have a default btree opclass in PG.
            | PgType::Tsvector
            | PgType::Tsquery
    ) || matches!(ty, PgType::User(oid) if catalog.enum_info(oid).is_some())
}

fn bind_value(value: &ast::ValueWithSpan, scope: &Scope) -> Result<Binding, BindError> {
    match &value.value {
        ast::Value::Placeholder(s) => bind_placeholder(s, value.span, scope),
        ast::Value::Number(n, _) => parse_number(n).map(Binding::Typed),
        ast::Value::SingleQuotedString(s)
        | ast::Value::DollarQuotedString(ast::DollarQuotedString { value: s, .. }) => {
            Ok(Binding::Unknown {
                lit: Some(s.clone()),
                span: value.span,
                param: None,
            })
        }
        ast::Value::Boolean(b) => Ok(Binding::Typed(BoundExpr::Const {
            value: Value::Bool(*b),
            ty: PgType::Bool,
        })),
        ast::Value::Null => Ok(Binding::Unknown {
            lit: None,
            span: value.span,
            param: None,
        }),
        // `B'...'` is a `bit(n)` literal (n binary digits); `X'...'` a `bit(4n)`
        // literal (4 bits per hex digit). PG's `bit_in` rejects a bad digit with
        // a data exception (22P02) naming the offender, at the literal's cursor.
        ast::Value::SingleQuotedByteStringLiteral(s) => {
            bind_bit_literal(crabgresql_types::bit::from_binary(s), value.span)
        }
        ast::Value::HexStringLiteral(s) => {
            bind_bit_literal(crabgresql_types::bit::from_hex(s), value.span)
        }
        other => Err(BindError::feature_not_supported(format!(
            "literal is not supported yet: {other}"
        ))),
    }
}

/// Bind a `$n` placeholder. The trailing number is the 1-based parameter; PG
/// rejects `$0`/non-numeric with a syntax error. A placeholder is registered in
/// the shared context (an error there when the simple protocol forbids
/// parameters). If the parameter's type is already known — declared by the
/// client or inferred at an earlier site — it binds straight to a typed
/// [`BoundExpr::Param`]; otherwise it stays an `Unknown` carrying the param
/// marker, to be typed by the first context that resolves it.
fn bind_placeholder(s: &str, span: Span, scope: &Scope) -> Result<Binding, BindError> {
    let n1: usize = s
        .strip_prefix('$')
        .and_then(|d| d.parse().ok())
        .filter(|&n| n > 0)
        .ok_or_else(|| BindError::syntax(format!("invalid parameter number: {s}")))?;
    let index = scope.params().borrow_mut().reference(n1)?;
    let known = scope.params().borrow().types.get(index).copied().flatten();
    if let Some(ty) = known {
        return Ok(Binding::Typed(BoundExpr::Param { index, ty }));
    }
    Ok(Binding::Unknown {
        lit: None,
        span,
        param: Some((index, scope.params().clone())),
    })
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
        DataType::JSON => PgType::Json,
        DataType::JSONB => PgType::Jsonb,
        // The parser has dedicated keyword variants for these two; the bareword
        // arm below still catches the schema-qualified spellings.
        DataType::TsVector => PgType::Tsvector,
        DataType::TsQuery => PgType::Tsquery,
        DataType::Regclass => PgType::Reg(RegKind::Class),
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
        // `T[]` / `ARRAY[N]` / `ARRAY<T>`: a one-dimensional array of the element
        // type. The `int[5]` length is accepted and ignored (PG does not enforce
        // array length). A bare `ARRAY` (no element type) has no meaning here.
        DataType::Array(elem_def) => {
            let inner = match elem_def {
                ast::ArrayElemTypeDef::SquareBracket(inner, _)
                | ast::ArrayElemTypeDef::AngleBracket(inner)
                | ast::ArrayElemTypeDef::Parenthesis(inner) => inner,
                ast::ArrayElemTypeDef::None => {
                    return Err(BindError::feature_not_supported(
                        "array type without an element type is not supported",
                    ));
                }
            };
            let elem = map_data_type(inner)?;
            // Only element types this build has an array type for are supported;
            // this also rejects multi-dimensional arrays (an array element type),
            // which are not modeled yet.
            if crabgresql_types::array::array_oid_for_elem(elem.oid()).is_none() {
                return Err(BindError::feature_not_supported(format!(
                    "type \"{dt}\" is not supported yet"
                )));
            }
            PgType::Array(elem.oid())
        }
        // Type names the parser has no dedicated `DataType` for arrive here:
        // `bpchar`, `name`, `point`, and every built-in written with a
        // `pg_catalog.` qualifier.
        DataType::Custom(obj, mods) if mods.is_empty() => match builtin_custom_type(obj) {
            Some(t) => t,
            None => {
                return Err(BindError::feature_not_supported(format!(
                    "type \"{dt}\" is not supported yet"
                )));
            }
        },
        other => {
            return Err(BindError::feature_not_supported(format!(
                "type \"{other}\" is not supported yet"
            )));
        }
    })
}

/// The built-in a `DataType::Custom` name denotes, or `None` if it names no
/// built-in — an unknown type, or one qualified with a schema other than
/// `pg_catalog`. Both fall through to the user-type lookup in [`bind_cast`].
///
/// Built-ins live in `pg_catalog`, so a bare `int4` and `pg_catalog.int4` name
/// the same type while `app.int4` names a user type that merely shares the
/// spelling. psql leans on the qualified form throughout `\d` (`::pg_catalog.text`,
/// `pr.prattrs::pg_catalog.int2[]`), which `DataType::Array` picks up by
/// recursing through here.
fn builtin_custom_type(obj: &ast::ObjectName) -> Option<PgType> {
    let parts = obj
        .0
        .iter()
        .map(|p| p.as_ident().map(normalize_ident))
        .collect::<Option<Vec<_>>>()?;
    let name = match parts.as_slice() {
        [name] => name.as_str(),
        [schema, name] if schema == "pg_catalog" => name.as_str(),
        _ => return None,
    };
    PgType::from_name(name)
}

fn precision_of(info: &ast::ExactNumberInfo) -> Option<u64> {
    match info {
        ast::ExactNumberInfo::None => None,
        ast::ExactNumberInfo::Precision(p) => Some(*p),
        ast::ExactNumberInfo::PrecisionAndScale(p, _) => Some(*p),
    }
}

/// Resolve a written type name to a [`PgType`], falling back to the catalog for
/// `CREATE TYPE` names. This is the resolution a cast target goes through, so a
/// type name that works in `expr::t` works everywhere this is used — notably a
/// PL/pgSQL variable declaration, whose type is lifted out of the routine body
/// as text.
pub fn resolve_data_type(
    catalog: &Arc<dyn TypeCatalog>,
    data_type: &ast::DataType,
) -> Result<PgType, BindError> {
    match map_data_type(data_type) {
        Ok(t) => Ok(t),
        // Not a builtin type name — it may be a `CREATE TYPE` name; resolve it
        // against the catalog, else surface the original "not supported" error.
        Err(e) => match custom_type_name(data_type).and_then(|n| catalog.resolve_type(&n)) {
            Some(ut) => Ok(PgType::User(ut.oid)),
            None => Err(e),
        },
    }
}

fn bind_cast(
    inner: &ast::Expr,
    data_type: &ast::DataType,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let target = resolve_data_type(scope.catalog(), data_type)?;
    // `ARRAY[]::t[]`: an empty array constructor is otherwise untypable (see
    // `bind_array_ctor`); the cast target supplies its element type.
    if let ast::Expr::Array(arr) = inner
        && arr.elem.is_empty()
        && let PgType::Array(elem_oid) = target
        && let Some(elem) = PgType::from_oid(elem_oid)
    {
        return Ok(Binding::Typed(BoundExpr::ArrayCtor {
            elem,
            ty: target,
            elems: Vec::new(),
        }));
    }
    // A reg* cast resolves an object name (or an OID's name) against the
    // catalog, which lives in the executor — so it lowers to a function call
    // instead of folding here.
    if let PgType::Reg(kind) = target {
        return Ok(Binding::Typed(bind_reg_cast(inner, kind, scope)?));
    }
    // `ARRAY[…]::reg*[]`: cast each element, for the same reason. A value-level
    // array cast cannot do this — coercing an element needs a catalog lookup,
    // not a pure conversion — so casting an existing `text[]` *expression* to
    // `reg*[]` is still unsupported; only the constructor spelling resolves.
    if let ast::Expr::Array(arr) = inner
        && let PgType::Array(elem_oid) = target
        && let Some(PgType::Reg(kind)) = PgType::from_oid(elem_oid)
    {
        let elems = arr
            .elem
            .iter()
            .map(|e| bind_reg_cast(e, kind, scope))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Binding::Typed(BoundExpr::ArrayCtor {
            elem: PgType::Reg(kind),
            ty: target,
            elems,
        }));
    }
    let expr = match bind_expr(inner, scope)? {
        Binding::Unknown { lit, span, param } => {
            resolve_unknown_ctx(scope.catalog().as_ref(), lit, span, param, target)?
        }
        Binding::Typed(e) => coerce_cast(e, target, scope)?,
    };
    let expr = apply_numeric_typmod_if_any(expr, target, data_type)?;
    Ok(Binding::Typed(apply_length_typmod_if_any(
        expr, target, data_type,
    )?))
}

/// The (normalized) name of a bare `DataType::Custom` type reference — e.g. the
/// `xfloat4` in `x::xfloat4` — used to look a `CREATE TYPE` name up in the
/// catalog. `None` for anything that is not a plain custom name.
/// Lower `expr::regclass` (and the other `reg*` targets) to the catalog-backed
/// function that resolves it at run time.
///
/// Which function depends on what is being cast, matching PG: a *name* is looked
/// up and must exist, whereas an *OID* is taken as-is and only rendered — PG's
/// oid→reg casts are binary-coercible, so `999999::regclass` prints the digits
/// rather than erroring. An unknown literal is the name form.
fn bind_reg_cast(inner: &ast::Expr, kind: RegKind, scope: &Scope) -> Result<BoundExpr, BindError> {
    let expr = match bind_expr(inner, scope)? {
        Binding::Unknown { lit, span, param } => {
            resolve_unknown_ctx(scope.catalog().as_ref(), lit, span, param, PgType::Text)?
        }
        Binding::Typed(e) => e,
    };
    let ty = expr.ty();
    let (func, arg_ty) = match ty {
        PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name => {
            (ScalarFn::RegIn(kind), PgType::Text)
        }
        // reg* -> reg* goes through the OID, as it does in PG: the OID is kept
        // and re-rendered as the new kind of object.
        PgType::Oid | PgType::Int2 | PgType::Int4 | PgType::Int8 | PgType::Reg(_) => {
            (ScalarFn::RegFromOid(kind), PgType::Oid)
        }
        other => {
            return Err(BindError::new(
                sqlstate::CANNOT_COERCE,
                format!("cannot cast type {} to {}", other.name(), kind.typname()),
            ));
        }
    };
    Ok(BoundExpr::FuncCall {
        func,
        ret: PgType::Reg(kind),
        args: vec![coerce_expr(expr, arg_ty)?],
    })
}

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
    let catalog = scope.catalog();

    // Enum → text renders the label (PG's `enum_out`); the other text-family
    // targets do not have this cast, so they must use an explicitly registered
    // conversion just like any other user-type pair.
    if let PgType::User(oid) = source
        && catalog.enum_info(oid).is_some()
        && target == PgType::Text
    {
        return coerce_expr(expr, target);
    }
    // Text family → enum maps the label to its ordinal. Only a constant text
    // value can resolve at bind time; a runtime `textcol::myenum` cast has no
    // catalog to consult in the executor and is not supported yet.
    if is_text_family(source)
        && let PgType::User(oid) = target
        && let Some(info) = catalog.enum_info(oid)
    {
        return match expr {
            BoundExpr::Const { value: Value::Text(s), .. } => {
                enum_const(oid, &info, Some(s), Span::empty())
            }
            BoundExpr::Const { value: Value::Null, .. } => Ok(BoundExpr::Const {
                value: Value::Null,
                ty: target,
            }),
            _ => Err(BindError::feature_not_supported(
                "casting a non-constant text expression to an enum is not supported yet",
            )),
        };
    }

    match catalog.find_cast(source, target) {
        Some(UserCast { without_function: true }) => Ok(BoundExpr::Reinterpret {
            expr: Box::new(expr),
            reported: target,
            rep: scope.catalog().backing_rep(target),
        }),
        // WITH FUNCTION / WITH INOUT are rejected at `CREATE CAST`; guard anyway.
        Some(UserCast {
            without_function: false,
        }) => Err(BindError::feature_not_supported(
            "cast with a conversion function is not supported yet",
        )),
        None => Err(BindError::new(
            sqlstate::CANNOT_COERCE,
            format!(
                "cannot cast type {} to {}",
                type_label(source, catalog.as_ref()),
                type_label(target, catalog.as_ref())
            ),
        )),
    }
}

fn bind_typed_string(ts: &ast::TypedString) -> Result<Binding, BindError> {
    let target = map_data_type(&ts.data_type)?;
    let (lit, span) = match &ts.value.value {
        ast::Value::SingleQuotedString(s) => (Some(s.clone()), ts.value.span),
        other => {
            return Err(BindError::syntax(format!("invalid typed literal: {other}")));
        }
    };
    let expr = resolve_unknown(lit, span, None, target)?;
    let expr = apply_numeric_typmod_if_any(expr, target, &ts.data_type)?;
    Ok(Binding::Typed(apply_length_typmod_if_any(
        expr,
        target,
        &ts.data_type,
    )?))
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
    if let BoundExpr::Const {
        value: Value::Numeric(n),
        ..
    } = &expr
    {
        let applied = n
            .apply_typmod(precision, scale)
            .map_err(|e| BindError::new(e.sqlstate, e.message).with_detail(e.detail))?;
        return Ok(BoundExpr::Const {
            value: Value::Numeric(applied),
            ty: PgType::Numeric,
        });
    }
    Ok(BoundExpr::FuncCall {
        func: ScalarFn::NumApplyTypmod,
        ret: PgType::Numeric,
        args: vec![
            expr,
            BoundExpr::Const {
                value: Value::Int4(precision),
                ty: PgType::Int4,
            },
            BoundExpr::Const {
                value: Value::Int4(scale),
                ty: PgType::Int4,
            },
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

/// The largest `character`/`character varying` length PostgreSQL accepts, and
/// the largest `bit`/`bit varying` one. Probed against 18.4, which rejects both
/// ends of the range (`varchar(0)` and `varchar(10485761)`) with 22023.
const MAX_CHAR_LENGTH: u64 = 10_485_760;
const MAX_BIT_LENGTH: u64 = 83_886_080;

/// [`length_typmod`] with PostgreSQL's declared-length bounds enforced.
///
/// Worth having separately from `length_typmod`, which cannot report an error:
/// an unchecked length is not merely accepted-but-odd, it reaches
/// `pg_attribute` as a stored `atttypmod`, where PostgreSQL's `n + VARHDRSZ`
/// encoding would overflow `i32` and take down every later reader of the
/// catalog. Rejecting the DDL is also what PostgreSQL does, and the error names
/// the type by its `typname` (`char`, not `character`), as PG's does.
pub fn checked_length_typmod(dt: &ast::DataType) -> Result<Option<i32>, BindError> {
    use ast::DataType;
    fn declared(l: &Option<ast::CharacterLength>) -> Option<u64> {
        match l {
            Some(ast::CharacterLength::IntegerLength { length, .. }) => Some(*length),
            _ => None,
        }
    }
    let (length, typname, max) = match dt {
        DataType::Char(l) | DataType::Character(l) => (declared(l), "char", MAX_CHAR_LENGTH),
        DataType::Varchar(l) | DataType::CharacterVarying(l) => {
            (declared(l), "varchar", MAX_CHAR_LENGTH)
        }
        DataType::Bit(n) => (*n, "bit", MAX_BIT_LENGTH),
        DataType::BitVarying(n) | DataType::VarBit(n) => (*n, "varbit", MAX_BIT_LENGTH),
        // No other type carries a length modifier here.
        _ => return Ok(None),
    };
    if let Some(n) = length {
        let invalid = |message: String| BindError::new(sqlstate::INVALID_PARAMETER_VALUE, message);
        if n < 1 {
            return Err(invalid(format!(
                "length for type {typname} must be at least 1"
            )));
        }
        if n > max {
            return Err(invalid(format!(
                "length for type {typname} cannot exceed {max}"
            )));
        }
    }
    Ok(length_typmod(dt))
}

/// Apply a `varchar(n)`/`char(n)` length coercion, or a `name` truncation, when
/// the target is one of those types. Constant inputs fold at bind time.
pub(crate) fn apply_length_typmod_if_any(
    expr: BoundExpr,
    target: PgType,
    data_type: &ast::DataType,
) -> Result<BoundExpr, BindError> {
    let (func, typmod) = match target {
        PgType::Varchar => match checked_length_typmod(data_type)? {
            Some(n) => (ScalarFn::VarcharTypmod, Some(n)),
            None => return Ok(expr),
        },
        PgType::Bpchar => match checked_length_typmod(data_type)? {
            Some(n) => (ScalarFn::BpcharTypmod, Some(n)),
            None => return Ok(expr),
        },
        PgType::Bit => match checked_length_typmod(data_type)? {
            Some(n) => (ScalarFn::BitTypmod, Some(n)),
            None => return Ok(expr),
        },
        PgType::Varbit => match checked_length_typmod(data_type)? {
            Some(n) => (ScalarFn::VarbitTypmod, Some(n)),
            None => return Ok(expr),
        },
        // `name` always truncates to 63 characters, independent of any modifier.
        PgType::Name => (ScalarFn::NameInput, None),
        _ => return Ok(expr),
    };
    // Fold a constant value now (explicit-cast semantics: truncate/pad).
    if let BoundExpr::Const {
        value: Value::Text(s),
        ..
    } = &expr
    {
        let folded = match func {
            ScalarFn::VarcharTypmod => {
                let Some(typmod) = typmod else {
                    return Err(BindError::new("XX000", "varchar typmod is missing"));
                };
                crabgresql_types::text::truncate_chars(s, typmod)
            }
            ScalarFn::BpcharTypmod => {
                let Some(typmod) = typmod else {
                    return Err(BindError::new("XX000", "bpchar typmod is missing"));
                };
                crabgresql_types::text::bpchar_input(s, typmod, true)
                    .map_err(|e| BindError::new(e.sqlstate, e.message))?
            }
            ScalarFn::NameInput => crabgresql_types::text::name_input(s),
            _ => unreachable!(),
        };
        return Ok(BoundExpr::Const {
            value: Value::Text(folded),
            ty: target,
        });
    }
    if let BoundExpr::Const {
        value: Value::Bit { len, data },
        ..
    } = &expr
    {
        let Some(typmod) = typmod else {
            return Err(BindError::new("XX000", "bit typmod is missing"));
        };
        let (len, data) =
            crabgresql_types::bit::coerce(*len, data, typmod, target == PgType::Varbit, true)
                .map_err(|e| BindError::new(e.sqlstate, e.message))?;
        return Ok(BoundExpr::Const {
            value: Value::Bit { len, data },
            ty: target,
        });
    }
    let mut args = vec![expr];
    if let Some(n) = typmod {
        args.push(BoundExpr::Const {
            value: Value::Int4(n),
            ty: PgType::Int4,
        });
    }
    Ok(BoundExpr::FuncCall {
        func,
        ret: target,
        args,
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
                return Err(BindError::syntax(format!(
                    "invalid interval literal: {other}"
                )));
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
        Binding::Unknown { lit, span, param } => (
            ScalarFn::Extract,
            resolve_unknown(lit, span, param, PgType::Timestamp)?,
        ),
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
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, PgType::Text)?,
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
        Binding::Unknown { lit, span, param } => (
            ScalarFn::TimezoneToTz,
            PgType::TimestampTz,
            resolve_unknown(lit, span, param, PgType::Timestamp)?,
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
    let column = normalize_ident(column);
    // Resolves against this scope's relations first, then enclosing queries — a
    // qualified reference to an outer relation yields a correlated
    // `OuterColumnRef`. PG names a missing column with its qualifier, unquoted:
    // `column q.c does not exist` (contrast the unqualified form `column "c"
    // does not exist`).
    scope.resolve_qualified(&qualifier, &column)
}

fn no_op_unary(sym: &str, ty: &str) -> BindError {
    BindError::new(
        sqlstate::UNDEFINED_FUNCTION,
        format!("operator does not exist: {sym} {ty}"),
    )
}

fn ambiguous_unary(sym: &str) -> BindError {
    ambiguous_operator_msg(format!("operator is not unique: {sym} unknown"))
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
                Binding::Typed(e) if e.ty() == PgType::Money && op == ast::UnaryOperator::Minus => {
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
        // `!!` — prefix negation of a `tsquery`.
        ast::UnaryOperator::PGPrefixFactorial => resolve_ts_unary(bind_expr(operand, scope)?),
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
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, PgType::Float8)?,
    };
    Ok(Binding::Typed(BoundExpr::Unary {
        op: uop,
        expr: Box::new(expr),
    }))
}

fn bind_is_null(inner: &ast::Expr, scope: &Scope, negated: bool) -> Result<Binding, BindError> {
    let expr = match bind_expr(inner, scope)? {
        Binding::Typed(e) => e,
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, PgType::Text)?,
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
            Binding::Unknown { lit, span, param } => {
                Binding::Typed(resolve_unknown(lit, span, param, PgType::Text)?)
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
                match bind_binary_op(BinOp::Eq, op.clone(), value, Span::empty(), scope.catalog().as_ref())? {
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
    let whens: Vec<_> = conds.into_iter().zip(results).collect();

    if ty.is_collatable() {
        crate::collation::check_explicit_conflict(
            else_
                .iter()
                .map(|e| crate::collation::expr_collation(e))
                .chain(whens.iter().map(|(_, r)| crate::collation::expr_collation(r))),
        )?;
    }

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
    let items = list
        .iter()
        .map(|item| bind_expr(item, scope))
        .collect::<Result<Vec<_>, _>>()?;
    let (cmp, chain) = if negated {
        (BinOp::NotEq, BinOp::And)
    } else {
        (BinOp::Eq, BinOp::Or)
    };
    // An empty list is a parser syntax error (`IN ()`), so this is unreachable;
    // fold to the constant PG's `= ANY '{}'` yields rather than panic.
    if items.is_empty() {
        return Ok(Binding::Typed(BoundExpr::Const {
            value: Value::Bool(negated),
            ty: PgType::Bool,
        }));
    }

    // PG lowers `x IN (list)` to `x = ANY(ARRAY[list])`: the list is coerced to
    // one common type (its array element type, which excludes the tested
    // expression), then each `x = element` resolves as an operator — so the left
    // keeps its own type and the comparison still promotes (`int`/`real` ->
    // `float8`, temporal/money handling), matching `x = v`. Coercing the elements
    // to the list type first is observable: an int that overflows a float4
    // mantissa rounds when the list type is `real`, exactly as PG's array does.
    let elem_target = match in_list_type(&items) {
        ListType::Uniform(ty) => Some(ty),
        // `x IN (NULL)` settles the untyped elements on the tested expression's
        // type (`text` when it too is untyped), so `1 IN (NULL)` compares in int
        // and `NULL IN (NULL)` in text — never the two-unknown ambiguity error.
        ListType::AllUnknown => Some(binding_typed_ty(&left).unwrap_or(PgType::Text)),
        // An incompatible list leaves each element as-is so the pair resolves on
        // its own — PG's OR fallback and its `operator does not exist` error.
        ListType::Incompatible => None,
    };
    let mut acc: Option<Binding> = None;
    for item in &items {
        let right = match elem_target {
            Some(ty) => Binding::Typed(resolve_operand(item, ty)?),
            None => item.clone(),
        };
        let comparison =
            bind_binary_op(cmp, left.clone(), right, Span::empty(), scope.catalog().as_ref())?;
        acc = Some(match acc {
            None => comparison,
            Some(prev) => {
                bind_binary_op(chain, prev, comparison, Span::empty(), scope.catalog().as_ref())?
            }
        });
    }
    Ok(acc.expect("non-empty list yields at least one comparison"))
}

/// Bind `x BETWEEN low AND high` by desugaring into the pair of comparisons PG
/// itself emits, reusing `bind_binary_op` so each pair resolves with the same
/// type promotion, unknown-literal typing, "operator does not exist" errors, and
/// three-valued NULL handling as a written comparison:
///
/// - `x BETWEEN low AND high`     -> `(x >= low) AND (x <= high)`
/// - `x NOT BETWEEN low AND high` -> `(x < low) OR (x > high)`
///
/// The `NOT` form is the De Morgan dual of the positive one (`<`/`>` chained
/// with `OR`), which keeps it Kleene-correct for NULL bounds — mirroring how
/// `bind_in_list` picks `(NotEq, And)` vs `(Eq, Or)`. The tested expression is
/// bound twice, as `IN (list)` re-binds its left operand per element.
///
/// The low comparison is resolved before the high bound is even bound, so a
/// malformed `BETWEEN` surfaces the low-side error first — matching PG's
/// left-to-right analysis of `(a >= b) AND (a <= c)`, which fully resolves
/// `a >= b` (coercing `b`) before it looks at `c`.
fn bind_between(
    expr: &ast::Expr,
    low: &ast::Expr,
    high: &ast::Expr,
    negated: bool,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let (cmp_lo, cmp_hi, chain) = if negated {
        (BinOp::Lt, BinOp::Gt, BinOp::Or)
    } else {
        (BinOp::GtEq, BinOp::LtEq, BinOp::And)
    };
    let catalog = scope.catalog();
    let left = bind_expr(expr, scope)?;
    let low = bind_expr(low, scope)?;
    let lo = bind_binary_op(cmp_lo, left.clone(), low, Span::empty(), catalog.as_ref())?;
    let high = bind_expr(high, scope)?;
    let hi = bind_binary_op(cmp_hi, left, high, Span::empty(), catalog.as_ref())?;
    bind_binary_op(chain, lo, hi, Span::empty(), catalog.as_ref())
}

/// How an `IN` list resolves to the element type of PG's `= ANY(ARRAY[...])`.
enum ListType {
    /// The typed elements share this common type; coerce every element to it.
    Uniform(PgType),
    /// No typed elements (`x IN (NULL)`); the caller falls back to the tested
    /// expression's type (or `text` when it too is untyped).
    AllUnknown,
    /// The typed elements have no common type; leave each element as-is so the
    /// pair resolves on its own, reproducing PG's `operator does not exist` error.
    Incompatible,
}

/// Fold `merge_types` (PG's `select_common_type`) over the `IN` list's typed
/// elements — the array element type of PG's `= ANY(ARRAY[...])`, which excludes
/// the tested expression (so `x IN (1, 0::float4)` rounds the `1` to `real` as
/// PG's array does).
fn in_list_type(items: &[Binding]) -> ListType {
    let mut common: Option<PgType> = None;
    for b in items {
        if let Some(ty) = binding_typed_ty(b) {
            common = Some(match common {
                None => ty,
                Some(prev) => match merge_types(prev, ty) {
                    Some(m) => m,
                    None => return ListType::Incompatible,
                },
            });
        }
    }
    match common {
        Some(ty) => ListType::Uniform(ty),
        None => ListType::AllUnknown,
    }
}

fn bind_binary(
    left: &ast::Expr,
    op: &ast::BinaryOperator,
    right: &ast::Expr,
    op_span: Span,
    scope: &Scope,
) -> Result<Binding, BindError> {
    // `a OPERATOR(schema.op) b` — PG's explicit-schema operator spelling. Only the
    // bare `op` or a `pg_catalog`-qualified name refers to a built-in operator; map
    // the symbol back to its native `BinaryOperator` and recurse so it reaches the
    // exact same path as the bare spelling (e.g. `~` -> `bind_regex`). A non-empty,
    // non-`pg_catalog` qualifier names no built-in operator, so it is reported as
    // 42883 (PG additionally reports 3F000 when that schema does not exist, but the
    // schema catalog is not reachable here, so both collapse to 42883).
    if let ast::BinaryOperator::PGCustomBinaryOperator(parts) = op {
        let symbol = match parts.as_slice() {
            [sym] => sym.as_str(),
            [schema, sym] if schema.eq_ignore_ascii_case("pg_catalog") => sym.as_str(),
            _ => return Err(custom_op_undefined(left, op, right, scope)),
        };
        let native = match symbol {
            "~" => ast::BinaryOperator::PGRegexMatch,
            "~*" => ast::BinaryOperator::PGRegexIMatch,
            "!~" => ast::BinaryOperator::PGRegexNotMatch,
            "!~*" => ast::BinaryOperator::PGRegexNotIMatch,
            "~~" => ast::BinaryOperator::PGLikeMatch,
            "~~*" => ast::BinaryOperator::PGILikeMatch,
            "!~~" => ast::BinaryOperator::PGNotLikeMatch,
            "!~~*" => ast::BinaryOperator::PGNotILikeMatch,
            "=" => ast::BinaryOperator::Eq,
            "<>" => ast::BinaryOperator::NotEq,
            "<" => ast::BinaryOperator::Lt,
            "<=" => ast::BinaryOperator::LtEq,
            ">" => ast::BinaryOperator::Gt,
            ">=" => ast::BinaryOperator::GtEq,
            "||" => ast::BinaryOperator::StringConcat,
            "+" => ast::BinaryOperator::Plus,
            "-" => ast::BinaryOperator::Minus,
            "*" => ast::BinaryOperator::Multiply,
            "/" => ast::BinaryOperator::Divide,
            "%" => ast::BinaryOperator::Modulo,
            "^" => ast::BinaryOperator::PGExp,
            "@>" => ast::BinaryOperator::AtArrow,
            "<@" => ast::BinaryOperator::ArrowAt,
            "&&" => ast::BinaryOperator::PGOverlap,
            "<<" => ast::BinaryOperator::PGBitwiseShiftLeft,
            ">>" => ast::BinaryOperator::PGBitwiseShiftRight,
            "&" => ast::BinaryOperator::BitwiseAnd,
            "|" => ast::BinaryOperator::BitwiseOr,
            "#" => ast::BinaryOperator::PGBitwiseXor,
            "@@" => ast::BinaryOperator::AtAt,
            "@?" => ast::BinaryOperator::AtQuestion,
            _ => return Err(custom_op_undefined(left, op, right, scope)),
        };
        return bind_binary(left, &native, right, op_span, scope);
    }
    // `||` is not a `BinOp`; PG's `textcat`/`anytextcat` lower to a text concat,
    // and `bitcat` to a bit-string concat when either side is a bit string.
    if matches!(op, ast::BinaryOperator::StringConcat) {
        let lb = bind_expr(left, scope)?;
        let rb = bind_expr(right, scope)?;
        // Array concatenation (`array || array`, `array || element`,
        // `element || array`) when either side is a typed array.
        if binding_typed_ty(&lb).is_some_and(PgType::is_array)
            || binding_typed_ty(&rb).is_some_and(PgType::is_array)
        {
            return bind_array_concat(lb, rb);
        }
        // `tsvector || tsvector` unions the lexemes; `tsquery || tsquery` is an
        // OR. Both need a typed text-search operand, so a plain `text || text`
        // is untouched.
        if let Some(binding) = resolve_ts_concat(&lb, &rb)? {
            return Ok(binding);
        }
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
    // The POSIX regex operators `~` / `~*` / `!~` / `!~*`.
    if let Some((ci, negated)) = match op {
        ast::BinaryOperator::PGRegexMatch => Some((false, false)),
        ast::BinaryOperator::PGRegexIMatch => Some((true, false)),
        ast::BinaryOperator::PGRegexNotMatch => Some((false, true)),
        ast::BinaryOperator::PGRegexNotIMatch => Some((true, true)),
        _ => None,
    } {
        let lb = bind_expr(left, scope)?;
        let rb = bind_expr(right, scope)?;
        return bind_regex(lb, rb, ci, negated);
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

    // `tsvector @@ tsquery` and the `tsquery` combinators. Placed before the
    // jsonpath and array resolvers, which claim `@@` and `&&` respectively. The
    // network and geometric resolvers run earlier and also claim `&&`/`<->`, but
    // each self-guards on its own operand types, and this one only fires on a
    // typed text-search operand — so no resolver shadows another.
    if let Some(binding) = resolve_ts_op(op, &lb, &rb)? {
        return Ok(binding);
    }

    // `jsonb @? jsonpath` / `jsonb @@ jsonpath` lower to the jsonpath query
    // functions (in silent mode). Tried before the generic mapping, which has no
    // arm for `@?`/`@@`.
    if let Some(binding) = resolve_jsonb_op(op, &lb, &rb)? {
        return Ok(binding);
    }

    // Array containment / overlap (`@>` `<@` `&&`) on array operands.
    if let Some(binding) = resolve_array_op(op, &lb, &rb, scope.catalog().as_ref())? {
        return Ok(binding);
    }

    // The comparison spellings are shared with the quantified (`ANY`/`ALL`) path
    // so the two can never drift apart.
    if let Some(op) = binop_from_comparison(op) {
        return bind_binary_op(op, lb, rb, op_span, scope.catalog().as_ref());
    }
    let op = match op {
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
    bind_binary_op(op, lb, rb, op_span, scope.catalog().as_ref())
}

/// Resolve a binary operator over two already-bound operands. Split out from
/// `bind_binary` so a simple `CASE operand WHEN v` can reuse the exact `=`
/// resolution (unknown-literal handling, numeric promotion, "operator does not
/// exist" errors) that a written `operand = v` gets. `op_span` locates the
/// operator token for an error cursor (`Span::empty()` when the caller has no
/// written operator, e.g. `CASE`/chained comparisons).
pub(crate) fn bind_binary_op(
    op: BinOp,
    lb: Binding,
    rb: Binding,
    op_span: Span,
    catalog: &dyn TypeCatalog,
) -> Result<Binding, BindError> {
    if op.is_logic() {
        let left = to_bool_operand(lb, op.sql_symbol())?;
        let right = to_bool_operand(rb, op.sql_symbol())?;
        return Ok(Binding::Typed(BoundExpr::Binary {
            op,
            arg_ty: PgType::Bool,
            collation: DEFAULT_COLLATION_OID,
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
    if let Some(binding) = resolve_temporal(op, &lb, &rb, op_span)? {
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
        (Binding::Typed(l), Binding::Typed(r)) => unify_types(l, r, op, catalog)?,
        (Binding::Typed(l), Binding::Unknown { lit, span, param }) => {
            let ty = l.ty();
            if op.is_arithmetic() && !ty.is_numeric() {
                return Err(no_operator(&type_label(ty, catalog), op, "unknown"));
            }
            let r = resolve_unknown_ctx(catalog, lit, span, param, ty)?;
            (l, r, ty)
        }
        (Binding::Unknown { lit, span, param }, Binding::Typed(r)) => {
            let ty = r.ty();
            if op.is_arithmetic() && !ty.is_numeric() {
                return Err(no_operator("unknown", op, &type_label(ty, catalog)));
            }
            let l = resolve_unknown_ctx(catalog, lit, span, param, ty)?;
            (l, r, ty)
        }
        (
            Binding::Unknown {
                lit: ll,
                span: ls,
                param: lp,
            },
            Binding::Unknown {
                lit: rl,
                span: rs,
                param: rp,
            },
        ) => {
            if op.is_arithmetic() {
                // Every numeric type offers the operator; unknown operands
                // cannot pick one — PG reports ambiguity.
                return Err(
                    ambiguous_operator("unknown", op.sql_symbol(), "unknown").at(op_span)
                );
            }
            // Comparing two untyped literals: PG falls back to text.
            (
                resolve_unknown(ll, ls, lp, PgType::Text)?,
                resolve_unknown(rl, rs, rp, PgType::Text)?,
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
            || matches!(
                arg_ty,
                PgType::Int2 | PgType::Int4 | PgType::Int8 | PgType::Numeric
            );
        numeric_arith && mod_ok
    } else {
        is_orderable(arg_ty, catalog)
    };
    if !supported {
        let name = type_label(arg_ty, catalog);
        return Err(no_operator(&name, op, &name));
    }

    // A string comparison orders by the collation derived from its operands;
    // for every other type the collation is inert, so don't spend the walk.
    let collation = if arg_ty.is_collatable() {
        crate::collation::collation_for_comparison(&left, &right)?
    } else {
        DEFAULT_COLLATION_OID
    };
    Ok(Binding::Typed(BoundExpr::Binary {
        op,
        arg_ty,
        collation,
        left: Box::new(left),
        right: Box::new(right),
    }))
}

/// Resolve mixed-type temporal arithmetic to a function call, or `Ok(None)` to
/// let the generic (same-type / comparison) path handle it — including the
/// `operator does not exist` error for combinations with no operator (e.g.
/// `interval * interval`). An untyped literal opposite a temporal operand takes
/// the partner type: interval for `±`, float8 for the `* /` factor. `op_span`
/// locates the operator for the one ambiguity this owns (`time + time`).
fn resolve_temporal(
    op: BinOp,
    lb: &Binding,
    rb: &Binding,
    op_span: Span,
) -> Result<Option<Binding>, BindError> {
    if !matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div) {
        return Ok(None);
    }
    let lt = binding_typed_ty(lb);
    let rt = binding_typed_ty(rb);
    let is_temporal = |t: Option<PgType>| {
        matches!(
            t,
            Some(
                PgType::Interval | PgType::Timestamp | PgType::Date | PgType::Time | PgType::TimeTz
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
            (Some(T), None) => call(
                ScalarFn::TimestampPlInterval,
                T,
                typed(lb),
                resolve_operand(rb, I)?,
            ),
            (None, Some(T)) => call(
                ScalarFn::TimestampPlInterval,
                T,
                typed(rb),
                resolve_operand(lb, I)?,
            ),
            // date + int -> date; date + interval -> timestamp; date + time -> timestamp.
            (Some(D), _) if is_int(rt) => call(
                ScalarFn::DatePlDays,
                D,
                typed(lb),
                resolve_operand(rb, PgType::Int4)?,
            ),
            (_, Some(D)) if is_int(lt) => call(
                ScalarFn::DatePlDays,
                D,
                typed(rb),
                resolve_operand(lb, PgType::Int4)?,
            ),
            (Some(D), Some(I)) => call(ScalarFn::DatePlInterval, T, typed(lb), typed(rb)),
            (Some(I), Some(D)) => call(ScalarFn::DatePlInterval, T, typed(rb), typed(lb)),
            (Some(D), Some(TI)) => call(ScalarFn::DatePlTime, T, typed(lb), typed(rb)),
            (Some(TI), Some(D)) => call(ScalarFn::DatePlTime, T, typed(rb), typed(lb)),
            // date + timetz -> timestamptz.
            (Some(D), Some(TZ)) => call(
                ScalarFn::DatePlTimeTz,
                PgType::TimestampTz,
                typed(lb),
                typed(rb),
            ),
            (Some(TZ), Some(D)) => call(
                ScalarFn::DatePlTimeTz,
                PgType::TimestampTz,
                typed(rb),
                typed(lb),
            ),
            // time + interval -> time; timetz + interval -> timetz.
            (Some(TI), Some(I)) => call(ScalarFn::TimePlInterval, TI, typed(lb), typed(rb)),
            (Some(I), Some(TI)) => call(ScalarFn::TimePlInterval, TI, typed(rb), typed(lb)),
            (Some(TZ), Some(I)) => call(ScalarFn::TimeTzPlInterval, TZ, typed(lb), typed(rb)),
            (Some(I), Some(TZ)) => call(ScalarFn::TimeTzPlInterval, TZ, typed(rb), typed(lb)),
            // `time + time`: PG reaches several candidate `+` operators via
            // implicit casts and can't pick a best one — ambiguous (42725), not
            // "does not exist". Unique to `time`: `timetz + timetz`, `date +
            // date`, `timestamp[tz] + timestamp[tz]` all stay 42883 (verified
            // against PG), so no other same-type add gets this treatment.
            (Some(TI), Some(TI)) => {
                let name = PgType::Time.name();
                Err(ambiguous_operator(name, "+", name).at(op_span))
            }
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
            (Some(D), _) if is_int(rt) => call(
                ScalarFn::DateMiDays,
                D,
                typed(lb),
                resolve_operand(rb, PgType::Int4)?,
            ),
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
            (Some(D), None) => call(
                ScalarFn::DateMi,
                PgType::Int4,
                typed(lb),
                resolve_operand(rb, D)?,
            ),
            (None, Some(D)) => call(
                ScalarFn::DateMi,
                PgType::Int4,
                resolve_operand(lb, D)?,
                typed(rb),
            ),
            // time - time -> interval; time - interval -> time; timetz - interval -> timetz.
            (Some(TI), Some(TI)) => call(ScalarFn::TimeMi, I, typed(lb), typed(rb)),
            (Some(TI), Some(I)) => call(ScalarFn::TimeMiInterval, TI, typed(lb), typed(rb)),
            (Some(TZ), Some(I)) => call(ScalarFn::TimeTzMiInterval, TZ, typed(lb), typed(rb)),
            _ => Ok(None),
        },
        BinOp::Mul => match (lt, rt) {
            (Some(I), _) if factor_ok(rt) => call(
                ScalarFn::IntervalMul,
                I,
                typed(lb),
                resolve_operand(rb, PgType::Float8)?,
            ),
            (_, Some(I)) if factor_ok(lt) => call(
                ScalarFn::IntervalMul,
                I,
                typed(rb),
                resolve_operand(lb, PgType::Float8)?,
            ),
            _ => Ok(None),
        },
        BinOp::Div => match (lt, rt) {
            (Some(I), _) if factor_ok(rt) => call(
                ScalarFn::IntervalDiv,
                I,
                typed(lb),
                resolve_operand(rb, PgType::Float8)?,
            ),
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
        Ok(Some(Binding::Typed(BoundExpr::FuncCall {
            func,
            ret,
            args: vec![a, b],
        })))
    };
    match op {
        // money ± money; an untyped literal opposite money is parsed as money.
        // money ± int/float has no operator in PG — fall through to the error.
        BinOp::Add | BinOp::Sub => {
            let func = if op == BinOp::Add {
                ScalarFn::CashPl
            } else {
                ScalarFn::CashMi
            };
            match (lt, rt) {
                (Some(M), Some(M)) => call(func, M, typed(lb), typed(rb)),
                (Some(M), None) => call(func, M, typed(lb), resolve_operand(rb, M)?),
                (None, Some(M)) => call(func, M, resolve_operand(lb, M)?, typed(rb)),
                _ => Ok(None),
            }
        }
        BinOp::Mul => match (lt, rt) {
            (Some(M), _) if is_int(rt) => call(
                ScalarFn::CashMulInt,
                M,
                typed(lb),
                resolve_operand(rb, PgType::Int8)?,
            ),
            (_, Some(M)) if is_int(lt) => call(
                ScalarFn::CashMulInt,
                M,
                typed(rb),
                resolve_operand(lb, PgType::Int8)?,
            ),
            (Some(M), _) if is_flt(rt) => call(
                ScalarFn::CashMulFlt,
                M,
                typed(lb),
                resolve_operand(rb, PgType::Float8)?,
            ),
            (_, Some(M)) if is_flt(lt) => call(
                ScalarFn::CashMulFlt,
                M,
                typed(rb),
                resolve_operand(lb, PgType::Float8)?,
            ),
            _ => Ok(None),
        },
        BinOp::Div => match (lt, rt) {
            (Some(M), Some(M)) => call(ScalarFn::CashDivCash, PgType::Float8, typed(lb), typed(rb)),
            (Some(M), _) if is_int(rt) => call(
                ScalarFn::CashDivInt,
                M,
                typed(lb),
                resolve_operand(rb, PgType::Int8)?,
            ),
            (Some(M), _) if is_flt(rt) => call(
                ScalarFn::CashDivFlt,
                M,
                typed(lb),
                resolve_operand(rb, PgType::Float8)?,
            ),
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

/// `operator does not exist: <left> <op> <right>` (42883) for the operator
/// spellings that have no [`BinOp`] — the family resolvers' `@@`, `&&`, `<->`,
/// `>>`, ... Shared so a mis-typed operand reports a missing operator instead of
/// a cast failure from inside `coerce_expr`.
fn undefined_binary_operator(
    lb: &Binding,
    op: &ast::BinaryOperator,
    rb: &Binding,
) -> BindError {
    BindError::new(
        sqlstate::UNDEFINED_FUNCTION,
        format!(
            "operator does not exist: {} {op} {}",
            operand_name(lb),
            operand_name(rb)
        ),
    )
}

/// 42883 for an `OPERATOR(schema.op)` spelling that names no built-in operator
/// (non-`pg_catalog` schema, or an unrecognized symbol). Binds the operands only
/// on this error path — so the normal path is never double-bound — and surfaces
/// an operand error (undefined column, bad cast, …) *first*, as PG does by
/// analyzing the operands before resolving the operator. The operator renders
/// schema-qualified (`pg_catalog.###`) like PG, not wrapped in `OPERATOR(...)`.
fn custom_op_undefined(
    left: &ast::Expr,
    op: &ast::BinaryOperator,
    right: &ast::Expr,
    scope: &Scope,
) -> BindError {
    let lb = match bind_expr(left, scope) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let rb = match bind_expr(right, scope) {
        Ok(b) => b,
        Err(e) => return e,
    };
    // PG names the operator as `schema.op` (or bare `op`), never `OPERATOR(...)`.
    let op_name = match op {
        ast::BinaryOperator::PGCustomBinaryOperator(parts) => parts.join("."),
        _ => op.to_string(),
    };
    BindError::new(
        sqlstate::UNDEFINED_FUNCTION,
        format!(
            "operator does not exist: {} {op_name} {}",
            operand_name(&lb),
            operand_name(&rb)
        ),
    )
    .with_hint(Some(
        "No operator matches the given name and argument types. \
         You might need to add explicit type casts."
            .to_string(),
    ))
}

/// Materialize a network operand: a typed inet/cidr as is (both read through
/// `inet_of`), an untyped literal parsed as `inet`. `None` for a typed non-net
/// operand, so the caller can report the full "operator does not exist" error.
fn net_operand(b: &Binding) -> Option<Result<BoundExpr, BindError>> {
    match b {
        Binding::Typed(e) if is_net_ty(Some(e.ty())) => Some(Ok(e.clone())),
        Binding::Unknown { lit, span, param } => Some(resolve_unknown(
            lit.clone(),
            *span,
            param.clone(),
            PgType::Inet,
        )),
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
            return Err(undefined_binary_operator(lb, op, rb));
        };
        return call(func, ret, a?, b?);
    }

    // Host arithmetic: `inet ± int8` (commutative for `+`), `inet - inet`.
    match op {
        B::Plus if is_net_ty(lt) && !is_net_ty(rt) => {
            let (Some(a), Some(n)) = (net_operand(lb), int_operand(rb)) else {
                return Err(undefined_binary_operator(lb, op, rb));
            };
            call(ScalarFn::InetPlInt8, PgType::Inet, a?, n?)
        }
        B::Plus if is_net_ty(rt) && !is_net_ty(lt) => {
            let (Some(a), Some(n)) = (net_operand(rb), int_operand(lb)) else {
                return Err(undefined_binary_operator(lb, op, rb));
            };
            call(ScalarFn::InetPlInt8, PgType::Inet, a?, n?)
        }
        B::Minus if is_net_ty(lt) && is_net_ty(rt) => {
            let (Some(a), Some(b)) = (net_operand(lb), net_operand(rb)) else {
                return Err(undefined_binary_operator(lb, op, rb));
            };
            call(ScalarFn::InetMi, PgType::Int8, a?, b?)
        }
        B::Minus if is_net_ty(lt) => {
            let (Some(a), Some(n)) = (net_operand(lb), int_operand(rb)) else {
                return Err(undefined_binary_operator(lb, op, rb));
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
    use crate::functions::GeoFn;
    use ast::BinaryOperator as B;
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
        Ok(Some(Binding::Typed(BoundExpr::FuncCall {
            func,
            ret,
            args,
        })))
    };
    let geo = |f: GeoFn| ScalarFn::Geo(f);

    use PgType::{Lseg, Point};
    let (Some(left_ty), Some(right_ty)) = (left_ty, right_ty) else {
        return Ok(None);
    };
    let combo = (left_ty, right_ty);
    match op {
        // Distance — point↔point, point↔lseg, lseg↔lseg.
        B::LtDashGt => match combo {
            (Point, Point) => call(
                geo(GeoFn::PointDist),
                PgType::Float8,
                vec![l(Point)?, r(Point)?],
            ),
            (Point, Lseg) => call(
                geo(GeoFn::DistPointSeg),
                PgType::Float8,
                vec![l(Point)?, r(Lseg)?],
            ),
            (Lseg, Point) => call(
                geo(GeoFn::DistPointSeg),
                PgType::Float8,
                vec![r(Point)?, l(Lseg)?],
            ),
            (Lseg, Lseg) => call(
                geo(GeoFn::DistSegSeg),
                PgType::Float8,
                vec![l(Lseg)?, r(Lseg)?],
            ),
            _ => Ok(None),
        },
        // Point positional / same-as / horizontal / vertical predicates.
        B::PGBitwiseShiftLeft if combo == (Point, Point) => call(
            geo(GeoFn::PointLeft),
            PgType::Bool,
            vec![l(Point)?, r(Point)?],
        ),
        B::PGBitwiseShiftRight if combo == (Point, Point) => call(
            geo(GeoFn::PointRight),
            PgType::Bool,
            vec![l(Point)?, r(Point)?],
        ),
        B::PipeGtGt if combo == (Point, Point) => call(
            geo(GeoFn::PointAbove),
            PgType::Bool,
            vec![l(Point)?, r(Point)?],
        ),
        B::LtLtPipe if combo == (Point, Point) => call(
            geo(GeoFn::PointBelow),
            PgType::Bool,
            vec![l(Point)?, r(Point)?],
        ),
        B::TildeEq if combo == (Point, Point) => call(
            geo(GeoFn::PointEq),
            PgType::Bool,
            vec![l(Point)?, r(Point)?],
        ),
        B::QuestionDash if combo == (Point, Point) => call(
            geo(GeoFn::PointHoriz),
            PgType::Bool,
            vec![l(Point)?, r(Point)?],
        ),
        B::QuestionPipe if combo == (Point, Point) => call(
            geo(GeoFn::PointVert),
            PgType::Bool,
            vec![l(Point)?, r(Point)?],
        ),
        // Point arithmetic (`-> point`).
        B::Plus if combo == (Point, Point) => call(
            geo(GeoFn::PointAdd),
            PgType::Point,
            vec![l(Point)?, r(Point)?],
        ),
        B::Minus if combo == (Point, Point) => call(
            geo(GeoFn::PointSub),
            PgType::Point,
            vec![l(Point)?, r(Point)?],
        ),
        B::Multiply if combo == (Point, Point) => call(
            geo(GeoFn::PointMul),
            PgType::Point,
            vec![l(Point)?, r(Point)?],
        ),
        B::Divide if combo == (Point, Point) => call(
            geo(GeoFn::PointDiv),
            PgType::Point,
            vec![l(Point)?, r(Point)?],
        ),
        // `point <@ lseg` (point lies on the segment).
        B::ArrowAt if combo == (Point, Lseg) => call(
            geo(GeoFn::PointOnSeg),
            PgType::Bool,
            vec![l(Point)?, r(Lseg)?],
        ),
        // `##` closest point: point→lseg or lseg→lseg (result on the 2nd operand).
        B::DoubleHash => match combo {
            (Point, Lseg) => call(
                geo(GeoFn::ClosePointSeg),
                PgType::Point,
                vec![l(Point)?, r(Lseg)?],
            ),
            (Lseg, Lseg) => call(
                geo(GeoFn::CloseSegSeg),
                PgType::Point,
                vec![l(Lseg)?, r(Lseg)?],
            ),
            _ => Ok(None),
        },
        // `#` intersection point of two segments (NULL if none).
        B::PGBitwiseXor if combo == (Lseg, Lseg) => call(
            geo(GeoFn::LsegInterpt),
            PgType::Point,
            vec![l(Lseg)?, r(Lseg)?],
        ),
        // lseg parallel / perpendicular.
        B::QuestionDoublePipe if combo == (Lseg, Lseg) => call(
            geo(GeoFn::LsegParallel),
            PgType::Bool,
            vec![l(Lseg)?, r(Lseg)?],
        ),
        B::QuestionDashPipe if combo == (Lseg, Lseg) => call(
            geo(GeoFn::LsegPerpendicular),
            PgType::Bool,
            vec![l(Lseg)?, r(Lseg)?],
        ),
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
        Binding::Unknown { lit, span, param } => Some(resolve_unknown(
            lit.clone(),
            *span,
            param.clone(),
            PgType::Bit,
        )),
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

/// The `(array type, element type)` of a binding that is a typed array, or
/// `None` for anything else.
fn array_arg_type(b: &Binding) -> Option<(PgType, PgType)> {
    match binding_typed_ty(b) {
        Some(PgType::Array(elem_oid)) => {
            PgType::from_oid(elem_oid).map(|e| (PgType::Array(elem_oid), e))
        }
        _ => None,
    }
}

/// Array containment / overlap operators (`@>`, `<@`, `&&`) → the array
/// `ScalarFn`s. Both operands are arrays (an untyped literal adopts the other's
/// array type); a typed non-array operand yields PG's `operator does not exist`.
/// The element type must have a default equality operator — a non-orderable
/// element (`json`, `point`, ...) reports PG's `could not identify an equality
/// operator` rather than reaching (and panicking in) `compare_values`. Tried
/// after the network/geometric/jsonb resolvers, which own these spellings for
/// their own operand types.
fn resolve_array_op(
    op: &ast::BinaryOperator,
    lb: &Binding,
    rb: &Binding,
    catalog: &dyn TypeCatalog,
) -> Result<Option<Binding>, BindError> {
    use ast::BinaryOperator as B;
    let func = match op {
        B::AtArrow => ScalarFn::ArrayContains,
        B::ArrowAt => ScalarFn::ArrayContainedBy,
        B::PGOverlap => ScalarFn::ArrayOverlap,
        _ => return Ok(None),
    };
    let la = array_arg_type(lb);
    let ra = array_arg_type(rb);
    // The shared array type. Both sides must be arrays: two typed arrays unify on
    // their element type; an untyped literal opposite a typed array adopts it; a
    // typed *non-array* opposite an array is `operator does not exist`.
    let arr_ty = match (la, ra) {
        (Some((_, le)), Some((_, re))) => {
            let elem = merge_types(le, re).ok_or_else(|| undefined_binary_operator(lb, op, rb))?;
            PgType::Array(elem.oid())
        }
        (Some((arr, _)), None) => {
            if binding_typed_ty(rb).is_some() {
                return Err(undefined_binary_operator(lb, op, rb));
            }
            arr
        }
        (None, Some((arr, _))) => {
            if binding_typed_ty(lb).is_some() {
                return Err(undefined_binary_operator(lb, op, rb));
            }
            arr
        }
        (None, None) => return Ok(None),
    };
    // These operators compare elements for equality; a non-orderable element type
    // has no default equality operator (PG's error), and `compare_values` has no
    // arm for it — so gate here to keep it off the panic path.
    let elem = arr_ty.array_element();
    if !elem.is_some_and(|e| is_orderable(e, catalog)) {
        let name = elem.map_or_else(|| arr_ty.name().to_string(), |e| type_label(e, catalog));
        return Err(BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!("could not identify an equality operator for type {name}"),
        ));
    }
    let left = resolve_operand(lb, arr_ty)?;
    let right = resolve_operand(rb, arr_ty)?;
    Ok(Some(Binding::Typed(BoundExpr::FuncCall {
        func,
        ret: PgType::Bool,
        args: vec![left, right],
    })))
}

/// `||` where at least one operand is an array. An **untyped literal or NULL**
/// opposite an array is treated as an array (PG resolves `array || unknown` to
/// `array_cat`), so `ARRAY[1,2] || '{3,4}'` concatenates and `array || NULL`
/// returns the array; a **typed element** opposite an array is append/prepend.
/// Element types are unified (PG promotes `int[] || bigint` to `bigint[]`); a
/// pair with no common type is PG's `operator does not exist: X || Y`.
fn bind_array_concat(lb: Binding, rb: Binding) -> Result<Binding, BindError> {
    let mismatch = |lb: &Binding, rb: &Binding| {
        BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!(
                "operator does not exist: {} || {}",
                binding_type_label(lb),
                binding_type_label(rb)
            ),
        )
    };
    match (array_arg_type(&lb), array_arg_type(&rb)) {
        // array || array: unify element types, then concatenate.
        (Some((_, le)), Some((_, re))) => {
            let arr = PgType::Array(merge_types(le, re).ok_or_else(|| mismatch(&lb, &rb))?.oid());
            let left = resolve_operand(&lb, arr)?;
            let right = resolve_operand(&rb, arr)?;
            Ok(Binding::Typed(BoundExpr::FuncCall {
                func: ScalarFn::ArrayCat,
                ret: arr,
                args: vec![left, right],
            }))
        }
        // array on the left; right is an untyped literal/NULL (→ concat) or a
        // typed element (→ append).
        (Some((arr_ty, elem)), None) => match binding_typed_ty(&rb) {
            None => {
                let left = resolve_operand(&lb, arr_ty)?;
                let right = resolve_operand(&rb, arr_ty)?;
                Ok(Binding::Typed(BoundExpr::FuncCall {
                    func: ScalarFn::ArrayCat,
                    ret: arr_ty,
                    args: vec![left, right],
                }))
            }
            Some(rty) => {
                let arr = PgType::Array(merge_types(elem, rty).ok_or_else(|| mismatch(&lb, &rb))?.oid());
                let elem = arr.array_element().expect("array element resolves");
                let left = resolve_operand(&lb, arr)?;
                let right = resolve_operand(&rb, elem)?;
                Ok(Binding::Typed(BoundExpr::FuncCall {
                    func: ScalarFn::ArrayAppend,
                    ret: arr,
                    args: vec![left, right],
                }))
            }
        },
        // array on the right; symmetric (untyped literal → concat, element → prepend).
        (None, Some((arr_ty, elem))) => match binding_typed_ty(&lb) {
            None => {
                let left = resolve_operand(&lb, arr_ty)?;
                let right = resolve_operand(&rb, arr_ty)?;
                Ok(Binding::Typed(BoundExpr::FuncCall {
                    func: ScalarFn::ArrayCat,
                    ret: arr_ty,
                    args: vec![left, right],
                }))
            }
            Some(lty) => {
                let arr = PgType::Array(merge_types(elem, lty).ok_or_else(|| mismatch(&lb, &rb))?.oid());
                let elem = arr.array_element().expect("array element resolves");
                let left = resolve_operand(&lb, elem)?;
                let right = resolve_operand(&rb, arr)?;
                Ok(Binding::Typed(BoundExpr::FuncCall {
                    func: ScalarFn::ArrayPrepend,
                    ret: arr,
                    args: vec![left, right],
                }))
            }
        },
        // The caller only routes here when a typed array is present; if neither
        // side classifies as an array, report the operator error rather than panic.
        (None, None) => Err(mismatch(&lb, &rb)),
    }
}

/// Bind a polymorphic array function whose overload can't live in the
/// fixed-signature table: `cardinality`, `array_length`, `array_upper`,
/// `array_append`, `array_prepend`, `array_cat`, `array_to_string`. Returns
/// `Ok(None)` if `name` is not one of them, so the caller falls through to
/// ordinary resolution.
pub(crate) fn bind_array_function(
    name: &str,
    bindings: &[Binding],
) -> Result<Option<Binding>, BindError> {
    let undefined = || {
        BindError::new(
            sqlstate::UNDEFINED_FUNCTION,
            format!(
                "function {name}({}) does not exist",
                bindings
                    .iter()
                    .map(binding_type_label)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    };
    let call = |func, ret, args| Ok(Some(Binding::Typed(BoundExpr::FuncCall { func, ret, args })));
    match name {
        "cardinality" => {
            let [b] = bindings else { return Err(undefined()) };
            let (arr_ty, _) = array_arg_type(b).ok_or_else(undefined)?;
            call(ScalarFn::Cardinality, PgType::Int4, vec![resolve_operand(b, arr_ty)?])
        }
        "array_length" => {
            let [a, dim] = bindings else { return Err(undefined()) };
            let (arr_ty, _) = array_arg_type(a).ok_or_else(undefined)?;
            call(
                ScalarFn::ArrayLength,
                PgType::Int4,
                vec![resolve_operand(a, arr_ty)?, resolve_operand(dim, PgType::Int4)?],
            )
        }
        "array_upper" => {
            let [a, dim] = bindings else { return Err(undefined()) };
            let (arr_ty, _) = array_arg_type(a).ok_or_else(undefined)?;
            call(
                ScalarFn::ArrayUpper,
                PgType::Int4,
                vec![resolve_operand(a, arr_ty)?, resolve_operand(dim, PgType::Int4)?],
            )
        }
        // `array_to_string(anyarray, text [, text])` renders the array's elements
        // (NULLs skipped, or replaced by the optional third argument) joined by
        // the delimiter. Always returns text.
        "array_to_string" => {
            let (a, delim, null_str) = match bindings {
                [a, delim] => (a, delim, None),
                [a, delim, null_str] => (a, delim, Some(null_str)),
                _ => return Err(undefined()),
            };
            let (arr_ty, _) = array_arg_type(a).ok_or_else(undefined)?;
            let mut args = vec![
                resolve_operand(a, arr_ty)?,
                resolve_operand(delim, PgType::Text)?,
            ];
            if let Some(null_str) = null_str {
                args.push(resolve_operand(null_str, PgType::Text)?);
            }
            call(ScalarFn::ArrayToString, PgType::Text, args)
        }
        // `array_append(anyarray, elem)` promotes the array/element to their
        // common element type (PG's `anycompatiblearray`/`anycompatible`).
        "array_append" => {
            let [a, e] = bindings else { return Err(undefined()) };
            let (_, elem) = array_arg_type(a).ok_or_else(undefined)?;
            let common = merge_types(elem, binding_typed_ty(e).unwrap_or(elem)).ok_or_else(undefined)?;
            let arr = PgType::Array(common.oid());
            call(
                ScalarFn::ArrayAppend,
                arr,
                vec![resolve_operand(a, arr)?, resolve_operand(e, common)?],
            )
        }
        "array_prepend" => {
            let [e, a] = bindings else { return Err(undefined()) };
            let (_, elem) = array_arg_type(a).ok_or_else(undefined)?;
            let common = merge_types(elem, binding_typed_ty(e).unwrap_or(elem)).ok_or_else(undefined)?;
            let arr = PgType::Array(common.oid());
            call(
                ScalarFn::ArrayPrepend,
                arr,
                vec![resolve_operand(e, common)?, resolve_operand(a, arr)?],
            )
        }
        // `array_cat(anyarray, anyarray)` unifies the two element types.
        "array_cat" => {
            let [a, b] = bindings else { return Err(undefined()) };
            let ae = array_arg_type(a).map(|(_, e)| e);
            let be = array_arg_type(b).map(|(_, e)| e);
            let arr = match (ae, be) {
                (Some(ae), Some(be)) => PgType::Array(merge_types(ae, be).ok_or_else(undefined)?.oid()),
                (Some(ae), None) => PgType::Array(ae.oid()),
                (None, Some(be)) => PgType::Array(be.oid()),
                (None, None) => return Err(undefined()),
            };
            call(
                ScalarFn::ArrayCat,
                arr,
                vec![resolve_operand(a, arr)?, resolve_operand(b, arr)?],
            )
        }
        _ => Ok(None),
    }
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
                return Err(undefined_binary_operator(lb, op, rb));
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
        return Err(undefined_binary_operator(lb, op, rb));
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
/// Lower `jsonb @? jsonpath` / `jsonb @@ jsonpath` to the silent jsonpath query
/// functions. Uses the dedicated `ExistsOp`/`MatchOp` variants (a 2-arg,
/// always-silent form) rather than the STRICT `jsonb_path_exists`/`_match`
/// functions, so the operator never nullifies on a NULL `vars`/`silent`.
/// Returns `Ok(None)` when the operator isn't one of these or the left operand
/// isn't `jsonb`.
fn resolve_jsonb_op(
    op: &ast::BinaryOperator,
    lb: &Binding,
    rb: &Binding,
) -> Result<Option<Binding>, BindError> {
    use crate::functions::JsonPathFn;
    let jf = match op {
        ast::BinaryOperator::AtQuestion => JsonPathFn::ExistsOp,
        ast::BinaryOperator::AtAt => JsonPathFn::MatchOp,
        _ => return Ok(None),
    };
    // Only defined for a `jsonb` left operand (an untyped literal is coerced).
    if matches!(binding_typed_ty(lb), Some(t) if t != PgType::Jsonb) {
        return Ok(None);
    }
    let left = resolve_operand(lb, PgType::Jsonb)?;
    let right = resolve_operand(rb, PgType::Jsonpath)?;
    Ok(Some(Binding::Typed(BoundExpr::FuncCall {
        func: ScalarFn::JsonPath(jf),
        ret: PgType::Bool,
        args: vec![left, right],
    })))
}

/// Text-search operators: `tsvector @@ tsquery` (either operand order), and
/// `tsquery && | <-> tsquery`.
///
/// Both operands must be untyped literals or already the required text-search
/// type. `Ok(None)` when no text-search operand is present, so `@@` still
/// reaches the jsonpath resolver; otherwise an error, so `point <-> tsquery`
/// reports a missing operator rather than a cast failure from inside
/// `coerce_expr`.
fn resolve_ts_op(
    op: &ast::BinaryOperator,
    lb: &Binding,
    rb: &Binding,
) -> Result<Option<Binding>, BindError> {
    use crate::functions::TsFn;
    let (lt, rt) = (binding_typed_ty(lb), binding_typed_ty(rb));
    // An operand is usable as `want` only if it is untyped or already `want`.
    let usable = |t: Option<PgType>, want: PgType| t.is_none_or(|t| t == want);
    match op {
        ast::BinaryOperator::AtAt => {
            // Decide the operand order from whichever side is already typed.
            let swapped = match (lt, rt) {
                (Some(PgType::Tsvector), _) | (_, Some(PgType::Tsquery)) => false,
                (Some(PgType::Tsquery), _) | (_, Some(PgType::Tsvector)) => true,
                _ => return Ok(None),
            };
            let (vec_b, query_b) = if swapped { (rb, lb) } else { (lb, rb) };
            let (vec_t, query_t) = if swapped { (rt, lt) } else { (lt, rt) };
            // The vector side must already *be* a tsvector. PG resolves an
            // untyped literal here to `text`, not `tsvector` -- `'Hello World'
            // @@ 'hello'::tsquery` is `to_tsvector('Hello World') @@ …`, which
            // is true. Parsing the literal as a tsvector instead would answer
            // false, and look like a real answer. Both that and an explicit
            // `text` operand need a text search configuration, a later rung, so
            // report the honest 0A000 rather than a wrong boolean or a 42883
            // that would deny an operator PG really has.
            if vec_t.is_none() || vec_t.is_some_and(is_text_family) {
                return Err(BindError::feature_not_supported(
                    "text @@ tsquery is not supported yet: it requires a text search \
                     configuration (to_tsvector)",
                ));
            }
            if !usable(vec_t, PgType::Tsvector) || !usable(query_t, PgType::Tsquery) {
                return Err(undefined_binary_operator(lb, op, rb));
            }
            Ok(Some(Binding::Typed(BoundExpr::FuncCall {
                func: ScalarFn::Ts(TsFn::Match),
                ret: PgType::Bool,
                args: vec![
                    resolve_operand(vec_b, PgType::Tsvector)?,
                    resolve_operand(query_b, PgType::Tsquery)?,
                ],
            })))
        }
        // `&&` and `<->` combine two queries. Without a typed `tsquery` these
        // belong to arrays/inet and to the geometric distance operator.
        ast::BinaryOperator::PGOverlap | ast::BinaryOperator::LtDashGt => {
            if lt != Some(PgType::Tsquery) && rt != Some(PgType::Tsquery) {
                return Ok(None);
            }
            if !usable(lt, PgType::Tsquery) || !usable(rt, PgType::Tsquery) {
                return Err(undefined_binary_operator(lb, op, rb));
            }
            let f = if matches!(op, ast::BinaryOperator::PGOverlap) {
                TsFn::QueryAnd
            } else {
                TsFn::QueryPhrase
            };
            Ok(Some(Binding::Typed(BoundExpr::FuncCall {
                func: ScalarFn::Ts(f),
                ret: PgType::Tsquery,
                args: vec![
                    resolve_operand(lb, PgType::Tsquery)?,
                    resolve_operand(rb, PgType::Tsquery)?,
                ],
            })))
        }
        _ => Ok(None),
    }
}

fn resolve_ts_concat(lb: &Binding, rb: &Binding) -> Result<Option<Binding>, BindError> {
    use crate::functions::TsFn;
    let (lt, rt) = (binding_typed_ty(lb), binding_typed_ty(rb));
    let ty = if lt == Some(PgType::Tsvector) || rt == Some(PgType::Tsvector) {
        PgType::Tsvector
    } else if lt == Some(PgType::Tsquery) || rt == Some(PgType::Tsquery) {
        PgType::Tsquery
    } else {
        return Ok(None);
    };
    // The other side must be untyped or the same text-search type. Anything else
    // (`text || tsvector`) is PG's `anytextcat`, which renders the tsvector as
    // text — so leave it to `bind_string_concat`.
    if !lt.is_none_or(|t| t == ty) || !rt.is_none_or(|t| t == ty) {
        return Ok(None);
    }
    let f = if ty == PgType::Tsvector {
        TsFn::VectorConcat
    } else {
        TsFn::QueryOr
    };
    let left = resolve_operand(lb, ty)?;
    let right = resolve_operand(rb, ty)?;
    Ok(Some(Binding::Typed(BoundExpr::FuncCall {
        func: ScalarFn::Ts(f),
        ret: ty,
        args: vec![left, right],
    })))
}

/// `!! tsquery` — negation. PG spells prefix `!!` as the "factorial" token.
fn resolve_ts_unary(operand: Binding) -> Result<Binding, BindError> {
    use crate::functions::TsFn;
    let e = match operand {
        Binding::Typed(e) if e.ty() == PgType::Tsquery => e,
        Binding::Typed(e) => return Err(no_op_unary("!!", e.ty().name())),
        Binding::Unknown { lit, span, param } => {
            resolve_unknown(lit, span, param, PgType::Tsquery)?
        }
    };
    Ok(Binding::Typed(BoundExpr::FuncCall {
        func: ScalarFn::Ts(TsFn::QueryNot),
        ret: PgType::Tsquery,
        args: vec![e],
    }))
}

fn resolve_operand(b: &Binding, target: PgType) -> Result<BoundExpr, BindError> {
    match b {
        Binding::Typed(e) if e.ty() == target => Ok(e.clone()),
        Binding::Typed(e) => coerce_expr(e.clone(), target),
        Binding::Unknown { lit, span, param } => {
            resolve_unknown(lit.clone(), *span, param.clone(), target)
        }
    }
}

fn bind_pow(lb: Binding, rb: Binding) -> Result<Binding, BindError> {
    let numeric = |b: &Binding| {
        matches!(b, Binding::Typed(e) if e.ty().is_numeric())
            || matches!(b, Binding::Unknown { .. })
    };
    if !numeric(&lb) || !numeric(&rb) {
        return Err(no_operator(
            &binding_type_label(&lb),
            BinOp::Pow,
            &binding_type_label(&rb),
        ));
    }
    // PG's `^` exists for `float8` and `numeric`. A float operand selects the
    // float8 operator; otherwise a numeric operand selects numeric (returning
    // numeric); with only ints/unknowns it falls back to float8 (as PG does).
    let is_float = |b: &Binding| matches!(b, Binding::Typed(e) if matches!(e.ty(), PgType::Float4 | PgType::Float8));
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
        collation: DEFAULT_COLLATION_OID,
        left: Box::new(left),
        right: Box::new(right),
    }))
}

fn pow_operand(b: Binding, target: PgType) -> Result<BoundExpr, BindError> {
    match b {
        Binding::Typed(e) => coerce_expr(e, target),
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, target),
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
        // A parameter's type is deduced by the *side effect* of `resolve_unknown`
        // (it records the type in the shared context). Overload resolution tries
        // this speculatively for every candidate signature, so resolving a
        // parameter during the exact-only pass would pin it to whichever
        // signature is tried first. Decline an unresolved parameter as an "exact"
        // match and let the typed arguments drive the choice in the fallback
        // pass; a literal (no param) still folds to its exact target as before.
        Binding::Unknown {
            lit,
            span,
            param: Some(param),
        } if exact_only => {
            // A parameter already fixed to `target` by an earlier occurrence is a
            // genuine exact match and must not be dropped. Read the slot into a
            // local so the shared borrow is released before `resolve_unknown`
            // takes it mutably.
            let already = param.1.borrow().types.get(param.0).copied().flatten();
            if already == Some(target) {
                resolve_unknown(lit, span, Some(param), target).ok()
            } else {
                None
            }
        }
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, target).ok(),
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
                // `reg* -> oid` is implicit in PG, which is how `oid =
                // 't'::regclass` resolves (both sides become oid) — the shape
                // psql's `\d` uses to match a relation. The reverse direction is
                // implicit in PG too, but cannot be a pure value cast here: it
                // has to resolve a name through the catalog, so `oid::regclass`
                // stays an explicit cast lowering to `RegFromOid`.
                | (Reg(_), Oid)
        )
}

/// Coerce a binding to `text` for a string function/operator argument. An
/// untyped literal (or NULL) becomes text; a typed value casts to text.
pub(crate) fn to_text_operand(binding: Binding) -> Result<BoundExpr, BindError> {
    match binding {
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, PgType::Text),
        Binding::Typed(e) if e.ty() == PgType::Text => Ok(e),
        Binding::Typed(e) => coerce_expr(e, PgType::Text),
    }
}

/// True for the text-family types that share `text`'s value representation —
/// exactly the collatable types.
pub(crate) fn is_text_family(ty: PgType) -> bool {
    ty.is_collatable()
}

/// Coerce an argument for `concat`/`concat_ws`/`format`, which use each value's
/// *output* representation. Text-family values are kept as-is (so a `bpchar`
/// keeps its blank padding, unlike the trailing-blank-stripping `||`); other
/// types are cast to their text form.
pub(crate) fn to_concat_operand(binding: Binding) -> Result<BoundExpr, BindError> {
    match binding {
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, PgType::Text),
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
            format!(
                "operator does not exist: {} || {}",
                l.ty().name(),
                r.ty().name()
            ),
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
        func: if case_insensitive {
            ScalarFn::ILike
        } else {
            ScalarFn::Like
        },
        ret: PgType::Bool,
        args,
    };
    let expr = if negated {
        BoundExpr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(call),
        }
    } else {
        call
    };
    Ok(Binding::Typed(expr))
}

/// `a ~ b` / `a ~* b` (and their negations): coerce operands to text and build
/// the POSIX regex match call, wrapping a negated form (`!~` / `!~*`) in `NOT`.
fn bind_regex(
    lb: Binding,
    rb: Binding,
    case_insensitive: bool,
    negated: bool,
) -> Result<Binding, BindError> {
    let args = vec![to_text_operand(lb)?, to_text_operand(rb)?];
    let call = BoundExpr::FuncCall {
        func: if case_insensitive {
            ScalarFn::RegexIMatch
        } else {
            ScalarFn::RegexMatch
        },
        ret: PgType::Bool,
        args,
    };
    let expr = if negated {
        BoundExpr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(call),
        }
    } else {
        call
    };
    Ok(Binding::Typed(expr))
}

/// `a SIMILAR TO b [ESCAPE c]`: coerce operands to text and build the match
/// call (the escape string, when present, is a third argument), wrapping a
/// negated form in `NOT`.
fn bind_similar_to(
    expr: &ast::Expr,
    pattern: &ast::Expr,
    escape_char: Option<&ast::ValueWithSpan>,
    negated: bool,
    scope: &Scope,
) -> Result<Binding, BindError> {
    let mut args = vec![
        to_text_operand(bind_expr(expr, scope)?)?,
        to_text_operand(bind_expr(pattern, scope)?)?,
    ];
    if let Some(v) = escape_char {
        match &v.value {
            ast::Value::SingleQuotedString(s) => {
                args.push(BoundExpr::Const {
                    value: Value::Text(s.clone()),
                    ty: PgType::Text,
                });
            }
            other => {
                return Err(BindError::syntax(format!(
                    "invalid ESCAPE literal: {other}"
                )));
            }
        }
    }
    let call = BoundExpr::FuncCall {
        func: ScalarFn::SimilarTo,
        ret: PgType::Bool,
        args,
    };
    let expr = if negated {
        BoundExpr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(call),
        }
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

/// PG reports 42725 (with DETAIL/HINT) when more than one candidate operator
/// matches and none is clearly best — as opposed to `no_operator`'s 42883 when
/// no candidate exists at all. Every 42725 site shares the same DETAIL/HINT.
fn ambiguous_operator_msg(message: String) -> BindError {
    BindError::new(sqlstate::AMBIGUOUS_FUNCTION, message)
        .with_detail(Some("Could not choose a best candidate operator.".to_string()))
        .with_hint(Some(
            "You might need to add explicit type casts.".to_string(),
        ))
}

fn ambiguous_operator(left: &str, sym: &str, right: &str) -> BindError {
    ambiguous_operator_msg(format!("operator is not unique: {left} {sym} {right}"))
}

/// Settle two typed operands on a common type: exact match, or numeric
/// promotion via a `Coerce` on the narrower side.
fn unify_types(
    left: BoundExpr,
    right: BoundExpr,
    op: BinOp,
    catalog: &dyn TypeCatalog,
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
    Err(no_operator(&type_label(lty, catalog), op, &type_label(rty, catalog)))
}

/// Display a user type by its catalog name instead of the generic
/// `user-defined` placeholder used by catalog-free [`PgType::name`].
pub(crate) fn type_label(ty: PgType, catalog: &dyn TypeCatalog) -> String {
    match ty {
        PgType::User(oid) => catalog
            .user_type_name(oid)
            .unwrap_or_else(|| ty.name().to_string()),
        _ => ty.name().to_string(),
    }
}

/// The common type of two column entries (`VALUES` rows / `UNION` arms),
/// approximating PG's `select_common_type`: when exactly one side implicitly
/// casts to the other, the column takes that target (so `real` + `int4` -> `real`,
/// not `float8`). When neither or both cast implicitly, fall back to numeric
/// preferred-type promotion (`float8` dominates). This deliberately differs from
/// `unify_types` (operator resolution), where `real` + `int4` resolves to `float8`.
pub(crate) fn merge_types(a: PgType, b: PgType) -> Option<PgType> {
    if a == b {
        return Some(a);
    }
    // Two arrays unify on their element type (PG promotes `int[]` + `bigint[]`
    // to `bigint[]`); this also drives `array || array` and `array_cat`.
    if let (PgType::Array(la), PgType::Array(rb)) = (a, b) {
        let (le, re) = (PgType::from_oid(la)?, PgType::from_oid(rb)?);
        return merge_types(le, re).map(|e| PgType::Array(e.oid()));
    }
    match (implicit_castable(a, b), implicit_castable(b, a)) {
        (true, false) => Some(b),
        (false, true) => Some(a),
        // Mutually castable: today only `bit` <-> `bit varying`, whose common
        // type is `bit varying` (the preferred type of the bit-string category),
        // as PG's `select_common_type` resolves it.
        (true, true) => Some(PgType::Varbit),
        (false, false) => common_string(a, b).or_else(|| common_numeric(a, b)),
    }
}

/// The common type of two string types. `char(n)` and `varchar(n)` are not
/// castable to each other, so they only meet at `text` — the preferred type of
/// PG's string category, which `select_common_type` picks for them.
fn common_string(a: PgType, b: PgType) -> Option<PgType> {
    let is_string = |ty| {
        matches!(
            ty,
            PgType::Text | PgType::Varchar | PgType::Bpchar | PgType::Name
        )
    };
    (is_string(a) && is_string(b)).then_some(PgType::Text)
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
            Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, ty),
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
    Some(if a == PgType::Float8 || b == PgType::Float8 {
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
    })
}

/// Coerce an expression to `ty`. Constant operands fold (and range-check) at
/// bind time, as PG's planner does; non-constants (and any cast to text, which
/// needs the session `extra_float_digits`) get a runtime `Coerce`.
pub(crate) fn coerce_expr(expr: BoundExpr, ty: PgType) -> Result<BoundExpr, BindError> {
    if expr.ty() == ty {
        return Ok(expr);
    }
    // `bpchar -> text` strips trailing blanks (PG's bpchar->text cast), which is
    // how a padded `char(n)` value loses its padding under `||`, `::text`, and
    // most text functions. It cannot be done in `cast_value` because a padded
    // `bpchar` value is indistinguishable from `text` there.
    if expr.ty() == PgType::Bpchar && ty == PgType::Text {
        if let BoundExpr::Const {
            value: Value::Text(s),
            ..
        } = &expr
        {
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
        Binding::Unknown { lit, span, param } => resolve_unknown(lit, span, param, PgType::Bool),
    }
}

/// Give an untyped literal its type from context, parsing its text the way the
/// type's input function would. A parse failure carries the literal's position
/// (PG's cursor), matching the `LINE n: ... ^` output.
pub(crate) fn resolve_unknown(
    lit: Option<String>,
    span: Span,
    param: Option<(usize, ParamCtx)>,
    ty: PgType,
) -> Result<BoundExpr, BindError> {
    // A bind parameter takes its type from this context: record the deduction
    // (conflicting deductions are 42P18) and emit a runtime `Param`, never a
    // folded constant.
    if let Some((index, ctx)) = param {
        ctx.borrow_mut().resolve(index, ty)?;
        return Ok(BoundExpr::Param { index, ty });
    }
    // A `reg*` literal names an object, and only the catalog can turn a name
    // into an OID — which the binder does not hold. Emit the same runtime
    // resolution an explicit `'t'::regclass` lowers to, so a literal that takes
    // its type from a `reg*` context resolves the way PG's `regclassin` does
    // instead of failing to fold here.
    //
    // Divergence: in a comparison PostgreSQL types the literal from the chosen
    // operator, and `reg* = unknown` picks `oideq`, so PG reads the literal as
    // an OID and rejects `'pg_class'::regclass = 'pg_class'` with "invalid input
    // syntax for type oid". This binder types it from the other side and
    // resolves the name, accepting it. Erring toward resolution keeps the
    // literal usable wherever a `reg*` is expected; matching PG exactly would
    // mean typing unknown operands from operator resolution.
    if let PgType::Reg(kind) = ty {
        let arg = match lit {
            None => BoundExpr::Const {
                value: Value::Null,
                ty: PgType::Text,
            },
            Some(s) => BoundExpr::Const {
                value: Value::Text(s),
                ty: PgType::Text,
            },
        };
        return Ok(BoundExpr::FuncCall {
            func: ScalarFn::RegIn(kind),
            ret: ty,
            args: vec![arg],
        });
    }
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
        // `json` keeps the raw text; `jsonb` parses/canonicalizes. Both carry the
        // JSON DETAIL through so `'{bad'::json` reproduces PG's error report.
        PgType::Json => crabgresql_types::json::json_in(s)
            .map(Value::Json)
            .map_err(|e| BindError::new(e.sqlstate, e.message).with_detail(e.detail)),
        PgType::Jsonb => crabgresql_types::json::jsonb_in(s)
            .map(Value::Jsonb)
            .map_err(|e| BindError::new(e.sqlstate, e.message).with_detail(e.detail)),
        // `jsonpath` parses the SQL/JSON path language into a compiled program.
        PgType::Jsonpath => crabgresql_types::jsonpath::jsonpath_in(s)
            .map(Value::Jsonpath)
            .map_err(|e| BindError::new(e.sqlstate, e.message).with_detail(e.detail)),
        // The text-search types parse their own input languages. Neither carries
        // a DETAIL; the message already names the offending input.
        PgType::Tsvector => crabgresql_types::tsvector::tsvector_in(s)
            .map(Value::Tsvector)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        PgType::Tsquery => crabgresql_types::tsquery::tsquery_in(s)
            .map(Value::Tsquery)
            .map_err(|e| BindError::new(e.sqlstate, e.message)),
        // `array_in`: parse a `{...}` literal, coercing each element to the array
        // element type. Carry PG's DETAIL through (`'{a,,c}'::int[]`).
        PgType::Array(elem_oid) => {
            let elem = PgType::from_oid(elem_oid).ok_or_else(invalid)?;
            crabgresql_types::array::array_in(s, elem)
                .map(|elems| Value::Array { elem, elems })
                .map_err(|e| {
                    BindError::new(e.sqlstate, e.message).with_detail(e.detail.map(String::from))
                })
        }
        // Never reached: `resolve_unknown` intercepts a reg* literal and lowers
        // it to the runtime `RegIn` resolution, because only the catalog can
        // turn an object name into an OID and the binder holds none.
        PgType::Reg(_) => Err(BindError::new(
            sqlstate::INTERNAL_ERROR,
            "reg* literal reached the constant folder",
        )),
        PgType::User(_) => Err(invalid()),
    }
}

/// `resolve_unknown`, but aware of user-defined enum targets: a text literal
/// destined for an enum becomes a [`Value::Enum`] via a catalog label lookup
/// (an unknown label is PG's `invalid input value for enum` error). Every other
/// target defers to the catalog-free [`resolve_unknown`].
pub(crate) fn resolve_unknown_ctx(
    catalog: &dyn TypeCatalog,
    lit: Option<String>,
    span: Span,
    param: Option<(usize, ParamCtx)>,
    ty: PgType,
) -> Result<BoundExpr, BindError> {
    if param.is_some() {
        return resolve_unknown(lit, span, param, ty);
    }
    if let PgType::User(oid) = ty
        && let Some(info) = catalog.enum_info(oid)
    {
        return enum_const(oid, &info, lit, span);
    }
    resolve_unknown(lit, span, None, ty)
}

/// Build an enum constant from a text literal by mapping the label to its
/// definition-order ordinal. A label not in the enum is PG's `enum_in` error
/// (22P02), carrying the literal's cursor position for the `LINE n: ^` caret.
fn enum_const(
    oid: u32,
    info: &EnumInfo,
    lit: Option<String>,
    span: Span,
) -> Result<BoundExpr, BindError> {
    let value = match lit {
        None => Value::Null,
        Some(s) => match info.labels.iter().position(|l| *l == s) {
            Some(ord) => Value::Enum {
                type_oid: oid,
                ordinal: ord as u32,
                label: s,
            },
            None => {
                return Err(BindError::new(
                    sqlstate::INVALID_TEXT_REPRESENTATION,
                    format!("invalid input value for enum {}: \"{s}\"", info.name),
                )
                .at(span));
            }
        },
    };
    Ok(BoundExpr::Const {
        value,
        ty: PgType::User(oid),
    })
}

/// Coerce an expression for assignment into a column (INSERT / UPDATE SET),
/// with PG's column-context error message on a type mismatch.
pub fn coerce_to_column(
    binding: Binding,
    column: &Column,
    scope: &Scope,
) -> Result<BoundExpr, BindError> {
    let base = match binding {
        Binding::Unknown { lit, span, param } => {
            resolve_unknown_ctx(scope.catalog().as_ref(), lit, span, param, column.ty)?
        }
        Binding::Typed(e) => {
            let ty = e.ty();
            if ty == column.ty {
                e
            } else if ty.is_numeric() && column.ty.is_numeric() {
                coerce_expr(e, column.ty)?
            // PG assignment context permits coercion via I/O to a string-category
            // target (the source's output function), so any type assigns to
            // text/varchar/char/name (e.g. INSERT ... VALUES (2) into varchar).
            } else if is_text_family(column.ty) {
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
                        type_label(column.ty, scope.catalog().as_ref()),
                        type_label(ty, scope.catalog().as_ref())
                    ),
                ));
            }
        }
    };
    apply_length_to_column(base, column)
}

/// Bind and assignment-coerce a column default in an empty scope. PostgreSQL
/// defaults cannot reference columns of the row being created.
pub fn bind_column_default(
    expr: &ast::Expr,
    column: &Column,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<BoundExpr, BindError> {
    let params = param_ctx_none();
    let scope = Scope::empty(catalog, &params);
    let bound = coerce_to_column(bind_expr(expr, &scope)?, column, &scope)?;
    if bound.contains_srf() {
        return Err(BindError::feature_not_supported(
            "set-returning functions are not allowed in DEFAULT expressions",
        ));
    }
    if bound.contains_aggregate() {
        return Err(BindError::new(
            sqlstate::GROUPING_ERROR,
            "aggregate functions are not allowed in DEFAULT expressions",
        ));
    }
    Ok(bound)
}

/// Bind the body of a `CREATE FUNCTION ... LANGUAGE SQL` to a typed expression,
/// with `$1..$n` seeded to the declared argument types and the result coerced to
/// the declared return type. `body_sql` is the normalized `SELECT <expr>` the
/// catalog stores; it must be a single FROM-less, single-column `SELECT` — any
/// other shape (FROM, WHERE, GROUP BY, set-op, multiple columns, …) is rejected,
/// since a scalar function is expanded inline and the engine has no per-row query
/// execution for function bodies.
///
/// Used both to validate the body at `CREATE FUNCTION` and to produce the
/// expression a call site inlines: the returned tree still carries `Param` leaves
/// for `$n`, which [`inline_params`] replaces with the argument expressions.
pub fn bind_sql_function_body(
    catalog: &Arc<dyn TypeCatalog>,
    arg_types: &[PgType],
    return_type: PgType,
    body_sql: &str,
) -> Result<BoundExpr, BindError> {
    let statements = crabgresql_parser::parse(body_sql).map_err(|e| {
        BindError::feature_not_supported(format!(
            "SQL function body must be a single SELECT statement: {e}"
        ))
    })?;
    let query = match statements.as_slice() {
        [ast::Statement::Query(query)] => query,
        _ => {
            return Err(BindError::feature_not_supported(
                "SQL function body must be a single SELECT statement",
            ));
        }
    };
    let unsupported: Option<&str> = if query.with.is_some() {
        Some("WITH")
    } else if query.order_by.is_some() {
        Some("ORDER BY")
    } else if query.limit_clause.is_some() {
        Some("LIMIT/OFFSET")
    } else if query.fetch.is_some() || !query.locks.is_empty() {
        Some("this clause")
    } else {
        None
    };
    if let Some(clause) = unsupported {
        return Err(BindError::feature_not_supported(format!(
            "{clause} is not supported in a SQL function body yet"
        )));
    }
    let select = match query.body.as_ref() {
        ast::SetExpr::Select(select) => select,
        _ => {
            return Err(BindError::feature_not_supported(
                "only a simple SELECT is supported in a SQL function body",
            ));
        }
    };
    let group_by_empty = matches!(
        &select.group_by,
        ast::GroupByExpr::Expressions(exprs, mods) if exprs.is_empty() && mods.is_empty()
    );
    let unsupported: Option<&str> = if !select.from.is_empty() {
        Some("FROM")
    } else if select.selection.is_some() {
        Some("WHERE")
    } else if !group_by_empty {
        Some("GROUP BY")
    } else if select.having.is_some() {
        Some("HAVING")
    } else if select.distinct.is_some() {
        Some("DISTINCT")
    } else {
        None
    };
    if let Some(clause) = unsupported {
        return Err(BindError::feature_not_supported(format!(
            "{clause} is not supported in a SQL function body yet"
        )));
    }
    let expr = match select.projection.as_slice() {
        [ast::SelectItem::UnnamedExpr(expr)] | [ast::SelectItem::ExprWithAlias { expr, .. }] => {
            expr
        }
        _ => {
            return Err(BindError::feature_not_supported(
                "a SQL function body must return a single column",
            ));
        }
    };

    // Seed `$1..$argcount` to the declared argument types; the capped context
    // rejects any larger `$n` at its reference site, naming the actual `n`.
    let params = param_ctx_capped(arg_types.iter().copied().map(Some).collect());
    let scope = Scope::empty(catalog, &params);
    let bound = bind_expr(expr, &scope)?;
    let bound = coerce_function_return(bound, return_type, catalog)?;
    if bound.contains_srf() {
        return Err(BindError::feature_not_supported(
            "set-returning functions are not supported in a SQL function body yet",
        ));
    }
    // PG accepts a FROM-less aggregate (e.g. `SELECT sum(1)`) as a function body;
    // this engine cannot yet inline one as a scalar, so it is a limitation, not
    // an illegal construct — report it as unsupported rather than a grouping error.
    if bound.contains_aggregate() {
        return Err(BindError::feature_not_supported(
            "aggregate functions in a SQL function body are not supported yet",
        ));
    }
    Ok(bound)
}

/// Coerce a SQL function body's result to the declared return type, in PG's
/// assignment context. A bare literal/`NULL` body takes the return type
/// directly; otherwise the same numeric-widening / text-assignment / implicit
/// casts as [`coerce_to_column`] apply, and an incompatible pair is PG's `42P13`
/// "return type mismatch in function declared to return …".
fn coerce_function_return(
    binding: Binding,
    return_type: PgType,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<BoundExpr, BindError> {
    let expr = match binding {
        Binding::Unknown { lit, span, param } => {
            return resolve_unknown_ctx(catalog.as_ref(), lit, span, param, return_type);
        }
        Binding::Typed(e) => e,
    };
    let ty = expr.ty();
    if ty == return_type {
        Ok(expr)
    } else if (ty.is_numeric() && return_type.is_numeric())
        || is_text_family(return_type)
        || implicit_castable(ty, return_type)
        || matches!((ty, return_type), (PgType::TimestampTz, PgType::Timestamp))
    {
        coerce_expr(expr, return_type)
    } else {
        Err(BindError::new(
            "42P13",
            format!(
                "return type mismatch in function declared to return {}",
                type_label(return_type, catalog.as_ref())
            ),
        )
        .with_detail(Some(format!(
            "Actual return type is {}.",
            type_label(ty, catalog.as_ref())
        ))))
    }
}

/// Replace each `$n` ([`BoundExpr::Param`]) in a bound SQL-function body with the
/// call's `n`-th argument expression. Mirrors [`crate::plan::subst_expr`], but
/// substitutes a whole expression (not a constant value), since a call argument
/// is an arbitrary expression over the outer row. A validated scalar body never
/// contains a subquery (the body scope forbids one), so those leaves carry no
/// params to replace and are left untouched.
pub fn inline_params(expr: BoundExpr, args: &[BoundExpr]) -> BoundExpr {
    match expr {
        // A validated body never references a `$n` past the argument list, so the
        // index is always in range; a null const is an inert fallback, not panic.
        BoundExpr::Param { index, ty } => args.get(index).cloned().unwrap_or(BoundExpr::Const {
            value: Value::Null,
            ty,
        }),
        BoundExpr::Const { .. }
        | BoundExpr::ColumnRef { .. }
        | BoundExpr::OuterColumnRef { .. } => expr,
        BoundExpr::Unary { op, expr } => BoundExpr::Unary {
            op,
            expr: Box::new(inline_params(*expr, args)),
        },
        BoundExpr::Collate {
            expr,
            collation,
            explicit,
        } => BoundExpr::Collate {
            expr: Box::new(inline_params(*expr, args)),
            collation,
            explicit,
        },
        BoundExpr::Binary {
            op,
            arg_ty,
            collation,
            left,
            right,
        } => BoundExpr::Binary {
            op,
            arg_ty,
            collation,
            left: Box::new(inline_params(*left, args)),
            right: Box::new(inline_params(*right, args)),
        },
        BoundExpr::Routine {
            oid,
            name,
            arg_types,
            strict,
            args: call_args,
            ret,
        } => BoundExpr::Routine {
            oid,
            name,
            arg_types,
            strict,
            args: call_args.into_iter().map(|a| inline_params(a, args)).collect(),
            ret,
        },
        BoundExpr::IsNull { expr, negated } => BoundExpr::IsNull {
            expr: Box::new(inline_params(*expr, args)),
            negated,
        },
        BoundExpr::Coerce { expr, ty } => BoundExpr::Coerce {
            expr: Box::new(inline_params(*expr, args)),
            ty,
        },
        BoundExpr::Reinterpret {
            expr,
            reported,
            rep,
        } => BoundExpr::Reinterpret {
            expr: Box::new(inline_params(*expr, args)),
            reported,
            rep,
        },
        BoundExpr::FuncCall {
            func,
            ret,
            args: call_args,
        } => BoundExpr::FuncCall {
            func,
            ret,
            args: call_args
                .into_iter()
                .map(|a| inline_params(a, args))
                .collect(),
        },
        BoundExpr::Srf {
            func,
            ret,
            args: call_args,
        } => BoundExpr::Srf {
            func,
            ret,
            args: call_args
                .into_iter()
                .map(|a| inline_params(a, args))
                .collect(),
        },
        BoundExpr::Case { whens, else_, ty } => BoundExpr::Case {
            whens: whens
                .into_iter()
                .map(|(cond, result)| (inline_params(cond, args), inline_params(result, args)))
                .collect(),
            else_: else_.map(|e| Box::new(inline_params(*e, args))),
            ty,
        },
        BoundExpr::Aggregate {
            func,
            distinct,
            args: agg_args,
            input_ty,
            ret,
        } => BoundExpr::Aggregate {
            func,
            distinct,
            args: agg_args
                .into_iter()
                .map(|a| inline_params(a, args))
                .collect(),
            input_ty,
            ret,
        },
        BoundExpr::ArrayCtor { elem, ty, elems } => BoundExpr::ArrayCtor {
            elem,
            ty,
            elems: elems.into_iter().map(|a| inline_params(a, args)).collect(),
        },
        BoundExpr::Subscript { base, index, ty } => BoundExpr::Subscript {
            base: Box::new(inline_params(*base, args)),
            index: Box::new(inline_params(*index, args)),
            ty,
        },
        // `x op ANY/ALL(array)` carries no subplan and can appear in a scalar
        // body, so inline params into both the array and the comparison template.
        BoundExpr::QuantifiedArray { array, all, cmp } => BoundExpr::QuantifiedArray {
            array: Box::new(inline_params(*array, args)),
            all,
            cmp: Box::new(inline_params(*cmp, args)),
        },
        // Subqueries cannot appear in a validated scalar body; leave untouched.
        BoundExpr::ScalarSubquery { .. }
        | BoundExpr::Exists { .. }
        | BoundExpr::QuantifiedSubquery { .. } => expr,
    }
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
    if let BoundExpr::Const {
        value: Value::Text(s),
        ..
    } = &expr
    {
        let folded = match func {
            ScalarFn::VarcharTypmod => {
                crabgresql_types::text::varchar_input(s, column.typmod, false)
                    .map_err(|e| BindError::new(e.sqlstate, e.message))?
            }
            ScalarFn::BpcharTypmod => crabgresql_types::text::bpchar_input(s, column.typmod, false)
                .map_err(|e| BindError::new(e.sqlstate, e.message))?,
            ScalarFn::NameInput => crabgresql_types::text::name_input(s),
            _ => unreachable!(),
        };
        return Ok(BoundExpr::Const {
            value: Value::Text(folded),
            ty: column.ty,
        });
    }
    if let BoundExpr::Const {
        value: Value::Bit { len, data },
        ..
    } = &expr
    {
        let (len, data) = crabgresql_types::bit::coerce(
            *len,
            data,
            column.typmod,
            column.ty == PgType::Varbit,
            false,
        )
        .map_err(|e| BindError::new(e.sqlstate, e.message))?;
        return Ok(BoundExpr::Const {
            value: Value::Bit { len, data },
            ty: column.ty,
        });
    }
    let mut args = vec![expr];
    if func != ScalarFn::NameInput {
        args.push(BoundExpr::Const {
            value: Value::Int4(column.typmod),
            ty: PgType::Int4,
        });
        // Third arg 0 = assignment (error on overflow), not a truncating cast.
        args.push(BoundExpr::Const {
            value: Value::Int4(0),
            ty: PgType::Int4,
        });
    }
    Ok(BoundExpr::FuncCall {
        func,
        ret: column.ty,
        args,
    })
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
        // `COLLATE` is value-transparent, like a cast that keeps the type: it
        // takes the wrapped expression's name, not its own.
        ast::Expr::Collate { expr, .. } => output_name(expr),
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
        // An `ARRAY[...]` constructor is named "array" in PG.
        ast::Expr::Array(_) => "array".into(),
        // `a[i]` subscript keeps the base's name, like a bare column through a
        // cast (`a[1]` → "a"); a non-name base falls through to `?column?`.
        ast::Expr::CompoundFieldAccess { root, .. } => output_name(root),
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
        // `EXISTS (…)` is named "exists" in PG.
        ast::Expr::Exists { .. } => "exists".into(),
        // A scalar `(SELECT …)` takes the name of the subquery's single output
        // column (`(SELECT max(x))` → "max", `(SELECT y)` → "y").
        ast::Expr::Subquery(query) => subquery_output_name(query),
        _ => "?column?".into(),
    }
}

/// The output-column name of a scalar `(SELECT …)`: the name of the subquery's
/// first (and only) target-list column — an alias if present, else the item
/// expression's own [`output_name`]. Anything that isn't a plain `SELECT`
/// (e.g. `VALUES`) or whose first item is a wildcard falls back to `?column?`.
fn subquery_output_name(query: &ast::Query) -> String {
    let ast::SetExpr::Select(select) = query.body.as_ref() else {
        return "?column?".into();
    };
    match select.projection.first() {
        Some(ast::SelectItem::UnnamedExpr(expr)) => output_name(expr),
        Some(ast::SelectItem::ExprWithAlias { alias, .. }) => normalize_ident(alias),
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
        .map(|ty| match ty {
            // A cast to an array type is named after the *element* type in PG
            // (`'{1}'::int[]` → column "int4"), not the `_int4` array typname.
            PgType::Array(elem) => PgType::from_oid(elem)
                .map_or_else(|| ty.typname().to_string(), |e| e.typname().to_string()),
            _ => ty.typname().to_string(),
        })
        .unwrap_or_else(|_| {
            // A user-defined type (e.g. an enum) is named after the type itself, as
            // PG does (`'red'::rainbow` → column "rainbow").
            custom_type_name(data_type).unwrap_or_else(|| "?column?".into())
        })
}
