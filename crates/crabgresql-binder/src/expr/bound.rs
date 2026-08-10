//! The expression IR: [`BoundExpr`] and the node types its variants wrap, plus
//! the tree walks (aggregate/window/SRF detection, column-reference collection)
//! every consumer of a bound expression asks it for.

use std::collections::BTreeSet;
use std::sync::Arc;

use crabgresql_types::{PgType, Value};

use crate::functions::{AggFn, ScalarFn, TableFn, WindowFn};

/// A subquery's bound plan, embedded in a [`BoundExpr`]. Wrapped so `BoundExpr`
/// keeps its `Debug`/`PartialEq` derives without imposing them on
/// [`crate::LogicalPlan`], which holds trait objects (`Arc<dyn TableAm>`) that
/// implement neither. Two embedded subplans never compare equal: structural plan
/// equality is needed nowhere, and treating them as distinct keeps optimizations
/// that dedup expressions (e.g. ORDER BY target reuse) conservatively correct.
#[derive(Clone)]
pub struct Subplan(pub Box<crate::logical_plan::LogicalPlan>);

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
    /// `IS [NOT] TRUE` / `IS [NOT] FALSE` / `IS [NOT] UNKNOWN`. Total, unlike a
    /// comparison: the operand equals exactly one of the three boolean values,
    /// so the test is never itself NULL.
    BoolTest {
        expr: Box<BoundExpr>,
        /// The boolean value the operand is tested against, in SQL's own
        /// three-valued domain — `None` is `UNKNOWN`, i.e. NULL.
        value: Option<bool>,
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
    /// A window-function call (`rank() OVER (…)`, `sum(x) OVER w`). A transient
    /// marker, exactly like [`BoundExpr::Aggregate`]: it may appear anywhere in
    /// a target-list / ORDER BY / DISTINCT ON expression, but the binder
    /// extracts every one into a [`crate::LogicalPlan::Window`] and rewrites the
    /// marker to a `ColumnRef` into that node's output row before planning.
    /// Evaluating it as a scalar is a bug (see `executor::eval`).
    ///
    /// `spec` decides which `Window` node the call lands on: calls sharing a
    /// spec are computed by one node, over one sort of the input.
    WindowFunc {
        kind: WindowKind,
        spec: Box<BoundWindowSpec>,
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
    /// (see `bind_in_subquery`). `cmp` is the bound `left op <hole>`
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

/// What a window call actually computes, once its `OVER` clause is stripped off.
#[derive(Clone, Debug, PartialEq)]
pub enum WindowKind {
    /// A dedicated window function, which reads only the row's position within
    /// its partition.
    Builtin {
        func: WindowFn,
        args: Vec<BoundExpr>,
    },
    /// An ordinary aggregate used as a window function (`sum(x) OVER (…)`).
    /// Deliberately carries a [`BoundAggregate`] so the executor drives it with
    /// the same accumulators as a grouped aggregate — that is what makes
    /// `sum(x) OVER (…)` inherit `sum(x)`'s exact numeric, overflow and NULL
    /// behavior rather than reimplementing it.
    Aggregate(BoundAggregate),
}

impl WindowKind {
    /// The call's per-row argument expressions, evaluated against the window
    /// node's input row. Empty for `row_number()` and for `count(*) OVER (…)`.
    pub fn args(&self) -> &[BoundExpr] {
        match self {
            WindowKind::Builtin { args, .. } => args,
            WindowKind::Aggregate(agg) => &agg.args,
        }
    }

    /// [`Self::args`], mutably.
    pub fn args_mut(&mut self) -> &mut Vec<BoundExpr> {
        match self {
            WindowKind::Builtin { args, .. } => args,
            WindowKind::Aggregate(agg) => &mut agg.args,
        }
    }
}

impl BoundWindowSpec {
    /// Every expression the spec evaluates against the window node's input row,
    /// partition keys first. The order matches [`Self::exprs_mut`], so the two
    /// can be zipped.
    pub fn exprs(&self) -> impl Iterator<Item = &BoundExpr> {
        self.partition_by
            .iter()
            .chain(self.order_by.iter().map(|key| &key.expr))
    }

    /// [`Self::exprs`], mutably.
    pub fn exprs_mut(&mut self) -> impl Iterator<Item = &mut BoundExpr> {
        self.partition_by
            .iter_mut()
            .chain(self.order_by.iter_mut().map(|key| &mut key.expr))
    }
}

/// One window call extracted from a query's expressions, occupying one slot of
/// its [`crate::LogicalPlan::Window`] node's output row (after the input row).
/// Produced by the binder's window-extraction pass from a
/// [`BoundExpr::WindowFunc`] marker, which is where the spec is left behind.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundWindowFunc {
    pub kind: WindowKind,
    pub ret: PgType,
    /// This call's absolute index in the window node's output row.
    ///
    /// Slots are numbered in the order the calls appear in the query, while the
    /// chain evaluates specs in a different order (fewest keys last, so that the
    /// output row order matches PG's). One node's slots are therefore not
    /// contiguous, and each call has to carry its own.
    pub slot: usize,
}

/// A bound `OVER (…)` clause: how the input is divided and ordered before the
/// window calls under it are evaluated.
///
/// Rung 1 supports only the default frame (`RANGE BETWEEN UNBOUNDED PRECEDING
/// AND CURRENT ROW`), so no frame is carried: with an `ORDER BY` the frame runs
/// from the partition start through the current row's last peer, and without
/// one it is the whole partition. An explicit non-default frame is refused at
/// bind time.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundWindowSpec {
    /// Evaluated against the pre-window row. Rows sharing these values form one
    /// partition; NULLs compare equal, so they group together.
    pub partition_by: Vec<BoundExpr>,
    /// The window's own `ORDER BY`. Rows equal on these keys are *peers*, which
    /// is what `rank`/`dense_rank` and the default frame are defined in terms of.
    pub order_by: Vec<WindowSortKey>,
}

/// One `ORDER BY` key of a window spec.
///
/// [`crate::SortKey`] cannot be reused: its `column` indexes an already-projected
/// tuple, whereas these are expressions evaluated against the window node's
/// input row.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowSortKey {
    pub expr: BoundExpr,
    /// `expr.ty()`, carried so the executor never re-derives it.
    pub ty: PgType,
    /// The collation ordering this key; only meaningful for a string `ty`.
    pub collation: u32,
    pub asc: bool,
    pub nulls_first: bool,
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

    pub(super) fn is_logic(self) -> bool {
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
            BoundExpr::IsNull { .. } | BoundExpr::BoolTest { .. } => PgType::Bool,
            BoundExpr::Coerce { ty, .. } => *ty,
            BoundExpr::Reinterpret { reported, .. } => *reported,
            BoundExpr::FuncCall { ret, .. } | BoundExpr::Routine { ret, .. } => *ret,
            BoundExpr::ArrayCtor { ty, .. } => *ty,
            BoundExpr::Subscript { ty, .. } => *ty,
            BoundExpr::Case { ty, .. } => *ty,
            BoundExpr::Srf { ret, .. } => *ret,
            BoundExpr::Aggregate { ret, .. } => *ret,
            BoundExpr::WindowFunc { ret, .. } => *ret,
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
            | BoundExpr::BoolTest { expr, .. }
            | BoundExpr::Coerce { expr, .. }
            | BoundExpr::Collate { expr, .. }
            | BoundExpr::Reinterpret { expr, .. } => expr.contains_srf(),
            BoundExpr::Binary { left, right, .. } => left.contains_srf() || right.contains_srf(),
            BoundExpr::FuncCall { args, .. }
            | BoundExpr::Routine { args, .. }
            | BoundExpr::Aggregate { args, .. } => args.iter().any(BoundExpr::contains_srf),
            BoundExpr::WindowFunc { kind, spec, .. } => {
                kind.args().iter().any(BoundExpr::contains_srf)
                    || spec.exprs().any(BoundExpr::contains_srf)
            }
            BoundExpr::ArrayCtor { elems, .. } => elems.iter().any(BoundExpr::contains_srf),
            BoundExpr::Subscript { base, index, .. } => base.contains_srf() || index.contains_srf(),
            BoundExpr::Case { whens, else_, .. } => {
                whens
                    .iter()
                    .any(|(condition, result)| condition.contains_srf() || result.contains_srf())
                    || else_.as_ref().is_some_and(|expr| expr.contains_srf())
            }
            // A subquery's own SRFs stay inside its subplan; nothing propagates
            // out to the enclosing projection.
            BoundExpr::QuantifiedSubquery { cmp, .. } => cmp.contains_srf(),
            BoundExpr::QuantifiedArray { array, cmp, .. } => {
                array.contains_srf() || cmp.contains_srf()
            }
            BoundExpr::Const { .. }
            | BoundExpr::ColumnRef { .. }
            | BoundExpr::Param { .. }
            | BoundExpr::OuterColumnRef { .. }
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
            | BoundExpr::BoolTest { expr, .. }
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
            BoundExpr::WindowFunc { kind, spec, .. } => {
                kind.args().iter().any(BoundExpr::contains_routine)
                    || spec.exprs().any(BoundExpr::contains_routine)
            }
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
            BoundExpr::IsNull { expr, .. } | BoundExpr::BoolTest { expr, .. } => {
                expr.contains_aggregate()
            }
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
            // A window call is not itself an aggregate — `sum(x) OVER ()` does
            // not make the query an aggregate query. But its arguments and its
            // OVER clause are ordinary expressions of *this* query level, so an
            // aggregate in one does: `sum(sum(x)) OVER ()` is a window sum over
            // a grouped sum, and PG accepts it.
            BoundExpr::WindowFunc { kind, spec, .. } => {
                kind.args().iter().any(BoundExpr::contains_aggregate)
                    || spec.exprs().any(BoundExpr::contains_aggregate)
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

    /// Whether this node itself is a window-call marker.
    pub fn is_window(&self) -> bool {
        matches!(self, BoundExpr::WindowFunc { .. })
    }

    /// Whether this expression tree contains a window-call marker anywhere,
    /// including inside another window call's arguments or `OVER` clause (which
    /// is how nesting is detected).
    ///
    /// A subquery's body is a separate query level and does not propagate: a
    /// window inside `(SELECT rank() OVER () …)` belongs to that subquery.
    pub fn contains_window(&self) -> bool {
        match self {
            BoundExpr::WindowFunc { .. } => true,
            BoundExpr::Const { .. }
            | BoundExpr::ColumnRef { .. }
            | BoundExpr::Param { .. }
            | BoundExpr::OuterColumnRef { .. }
            | BoundExpr::ScalarSubquery { .. }
            | BoundExpr::Exists { .. } => false,
            BoundExpr::Unary { expr, .. }
            | BoundExpr::IsNull { expr, .. }
            | BoundExpr::BoolTest { expr, .. }
            | BoundExpr::Coerce { expr, .. }
            | BoundExpr::Collate { expr, .. }
            | BoundExpr::Reinterpret { expr, .. } => expr.contains_window(),
            BoundExpr::Binary { left, right, .. } => {
                left.contains_window() || right.contains_window()
            }
            BoundExpr::FuncCall { args, .. }
            | BoundExpr::Routine { args, .. }
            | BoundExpr::Srf { args, .. }
            | BoundExpr::Aggregate { args, .. } => args.iter().any(BoundExpr::contains_window),
            BoundExpr::ArrayCtor { elems, .. } => elems.iter().any(BoundExpr::contains_window),
            BoundExpr::Subscript { base, index, .. } => {
                base.contains_window() || index.contains_window()
            }
            BoundExpr::Case { whens, else_, .. } => {
                whens
                    .iter()
                    .any(|(c, r)| c.contains_window() || r.contains_window())
                    || else_.as_ref().is_some_and(|e| e.contains_window())
            }
            BoundExpr::QuantifiedSubquery { cmp, .. } => cmp.contains_window(),
            BoundExpr::QuantifiedArray { array, cmp, .. } => {
                array.contains_window() || cmp.contains_window()
            }
        }
    }

    /// The first aggregate or window marker in this tree in **source order**, or
    /// `None` if it holds neither.
    ///
    /// PostgreSQL analyzes an expression tree in source order and blames the
    /// first misplaced node it meets, so `count(*) + rank() OVER ()` in a WHERE
    /// is an *aggregate* error while `rank() OVER () + count(*)` is a *window*
    /// error. Reproducing that needs the first offender, not just "does it
    /// contain one".
    ///
    /// A marker is returned without descending into it: `sum(x) OVER ()` is a
    /// window call, not an aggregate one. Subquery bodies are a separate query
    /// level and never propagate, exactly as in [`Self::contains_window`].
    pub(super) fn first_agg_or_window(&self) -> Option<&BoundExpr> {
        fn first(exprs: &[BoundExpr]) -> Option<&BoundExpr> {
            exprs.iter().find_map(BoundExpr::first_agg_or_window)
        }
        match self {
            BoundExpr::Aggregate { .. } | BoundExpr::WindowFunc { .. } => Some(self),
            BoundExpr::Const { .. }
            | BoundExpr::ColumnRef { .. }
            | BoundExpr::Param { .. }
            | BoundExpr::OuterColumnRef { .. }
            | BoundExpr::ScalarSubquery { .. }
            | BoundExpr::Exists { .. } => None,
            BoundExpr::Unary { expr, .. }
            | BoundExpr::IsNull { expr, .. }
            | BoundExpr::BoolTest { expr, .. }
            | BoundExpr::Coerce { expr, .. }
            | BoundExpr::Collate { expr, .. }
            | BoundExpr::Reinterpret { expr, .. } => expr.first_agg_or_window(),
            BoundExpr::Binary { left, right, .. } => left
                .first_agg_or_window()
                .or_else(|| right.first_agg_or_window()),
            BoundExpr::FuncCall { args, .. }
            | BoundExpr::Routine { args, .. }
            | BoundExpr::Srf { args, .. } => first(args),
            BoundExpr::ArrayCtor { elems, .. } => first(elems),
            BoundExpr::Subscript { base, index, .. } => base
                .first_agg_or_window()
                .or_else(|| index.first_agg_or_window()),
            BoundExpr::Case { whens, else_, .. } => {
                for (cond, result) in whens {
                    if let Some(found) = cond
                        .first_agg_or_window()
                        .or_else(|| result.first_agg_or_window())
                    {
                        return Some(found);
                    }
                }
                else_.as_ref().and_then(|e| e.first_agg_or_window())
            }
            BoundExpr::QuantifiedSubquery { cmp, .. } => cmp.first_agg_or_window(),
            BoundExpr::QuantifiedArray { array, cmp, .. } => array
                .first_agg_or_window()
                .or_else(|| cmp.first_agg_or_window()),
        }
    }

    /// Whether **this node alone** is a volatile call, ignoring its arguments.
    ///
    /// The list of volatile scalar functions lives here and only here, so a walk
    /// that visits every node (see [`crate::plan_contains_volatile_fn`]) and the
    /// subtree predicate below cannot drift apart. A routine's body is opaque
    /// and PostgreSQL defaults a routine to VOLATILE, so every call counts.
    pub fn is_volatile_call(&self) -> bool {
        match self {
            BoundExpr::FuncCall { func, .. } => matches!(
                func,
                ScalarFn::Nextval
                    | ScalarFn::Currval
                    | ScalarFn::Setval
                    | ScalarFn::Lastval
                    | ScalarFn::ClockTimestamp
                    | ScalarFn::GenRandomUuid
                    | ScalarFn::UuidV7
                    | ScalarFn::UuidV7Shift
            ),
            BoundExpr::Routine { .. } => true,
            _ => false,
        }
    }

    /// Whether this expression contains a volatile function call. The volatile
    /// [`ScalarFn`]s today are the sequence functions (`nextval`/`setval` have
    /// side effects, `currval`/`lastval` read mutable session state),
    /// `clock_timestamp`, which reads the wall clock afresh at every call, and
    /// the UUID generators, which draw fresh randomness — all marked `VOLATILE`
    /// by PostgreSQL. Any future volatile scalar function (e.g. `random()`)
    /// must be added to the match here. Used to refuse duplicating a volatile
    /// argument when inlining a SQL function body, and to keep such a call from
    /// being pushed down into a scan.
    ///
    /// `uuid_extract_version`/`uuid_extract_timestamp` are deliberately absent:
    /// they read only their argument, and PG marks them `IMMUTABLE`.
    ///
    /// `now()` and `statement_timestamp()` are deliberately *not* here: PG
    /// marks them `STABLE`, and they are. Calling them volatile would cost a
    /// real optimization — `WHERE ts > now() - interval '1 day'` could no
    /// longer be pushed to a leaf — for no change in the answer.
    ///
    /// Note this stops at a subquery marker: a subquery's body is a plan of its
    /// own. A caller that needs to see inside one wants
    /// [`crate::expr_contains_volatile_fn`].
    pub fn contains_volatile_fn(&self) -> bool {
        match self {
            BoundExpr::FuncCall { args, .. } => {
                self.is_volatile_call() || args.iter().any(BoundExpr::contains_volatile_fn)
            }
            BoundExpr::Routine { .. } => true,
            BoundExpr::Srf { args, .. } => args.iter().any(BoundExpr::contains_volatile_fn),
            BoundExpr::WindowFunc { kind, spec, .. } => {
                kind.args().iter().any(BoundExpr::contains_volatile_fn)
                    || spec.exprs().any(BoundExpr::contains_volatile_fn)
            }
            BoundExpr::ArrayCtor { elems, .. } => elems.iter().any(BoundExpr::contains_volatile_fn),
            BoundExpr::Subscript { base, index, .. } => {
                base.contains_volatile_fn() || index.contains_volatile_fn()
            }
            BoundExpr::Unary { expr, .. }
            | BoundExpr::IsNull { expr, .. }
            | BoundExpr::BoolTest { expr, .. }
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
            BoundExpr::Aggregate { args, .. } => args.iter().any(|a| a.contains_volatile_fn()),
            // A subquery's own body runs as a separate plan; only the outer needle
            // of an IN-subquery propagates to the enclosing expression.
            BoundExpr::QuantifiedSubquery { cmp, .. } => cmp.contains_volatile_fn(),
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
            | BoundExpr::BoolTest { expr, .. }
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
            BoundExpr::WindowFunc { kind, spec, .. } => kind
                .args()
                .iter()
                .chain(spec.exprs())
                .map(|a| a.count_param_refs(index))
                .sum(),
            BoundExpr::QuantifiedSubquery { cmp, .. } => cmp.count_param_refs(index),
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
                | BoundExpr::BoolTest { expr, .. }
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
                // The arguments and the OVER clause both read this row, so both
                // widen the hull. A marker that survives to a caller of this is
                // a binder bug; reporting the true hull keeps that bug from
                // becoming a *wrong* relocation decision on top of a loud one.
                BoundExpr::WindowFunc { kind, spec, .. } => {
                    kind.args()
                        .iter()
                        .chain(spec.exprs())
                        .for_each(|a| fold(a, acc));
                }
                // Non-correlated subplans reference no outer column; only the IN
                // needle (in `cmp`) can. Scalar/EXISTS contribute nothing.
                BoundExpr::QuantifiedSubquery { cmp, .. } => fold(cmp, acc),
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

    /// Collect the exact set of `ColumnRef` indices this expression reads into
    /// `out`, returning whether the set is *complete*.
    ///
    /// `false` means the dependency could not be determined and the caller must
    /// assume every column; `out` may hold a partial set and must be discarded.
    /// This is the third member of the family with
    /// [`Self::column_ref_bounds`] and [`Self::shift_column_refs`], and differs
    /// from the first in a way that matters: bounds are an over-approximating
    /// *hull*, safe for a containment test, whereas this is used to *discard*
    /// columns, so it has to be exact or refuse.
    ///
    /// The subplan-carrying variants therefore refuse. A **correlated** subquery
    /// records its dependency on this row inside its body, as an
    /// `OuterColumnRef` filled in at execution — and the body is a `LogicalPlan`
    /// no `BoundExpr` walk reaches. Pruning a column read only by a correlated
    /// `EXISTS` would silently substitute NULL and return wrong rows. The same
    /// reasoning is spelled out in `pushdown::is_relocatable`.
    ///
    /// The match is exhaustive on purpose: a new variant must fail to compile
    /// here rather than silently prune a column it reads.
    pub fn collect_column_refs(&self, out: &mut BTreeSet<usize>) -> bool {
        match self {
            BoundExpr::ColumnRef { index, .. } => {
                out.insert(*index);
                true
            }
            // An outer reference addresses an enclosing row, not this row's
            // index space, so it contributes nothing — exactly as in
            // `column_ref_bounds`. It is safe here only because every construct
            // that can *carry* one (the subplan variants below) refuses.
            BoundExpr::Const { .. }
            | BoundExpr::Param { .. }
            | BoundExpr::OuterColumnRef { .. } => true,
            BoundExpr::Unary { expr, .. }
            | BoundExpr::IsNull { expr, .. }
            | BoundExpr::BoolTest { expr, .. }
            | BoundExpr::Coerce { expr, .. }
            | BoundExpr::Collate { expr, .. }
            | BoundExpr::Reinterpret { expr, .. } => expr.collect_column_refs(out),
            BoundExpr::Binary { left, right, .. } => {
                left.collect_column_refs(out) && right.collect_column_refs(out)
            }
            // A routine body has its own frame and can only see this row through
            // its arguments.
            BoundExpr::FuncCall { args, .. }
            | BoundExpr::Routine { args, .. }
            | BoundExpr::Srf { args, .. }
            | BoundExpr::Aggregate { args, .. } => Self::collect_all(args, out),
            BoundExpr::ArrayCtor { elems, .. } => Self::collect_all(elems, out),
            BoundExpr::Subscript { base, index, .. } => {
                base.collect_column_refs(out) && index.collect_column_refs(out)
            }
            BoundExpr::Case { whens, else_, .. } => {
                let mut complete = true;
                for (cond, result) in whens {
                    complete &= cond.collect_column_refs(out);
                    complete &= result.collect_column_refs(out);
                }
                if let Some(else_) = else_ {
                    complete &= else_.collect_column_refs(out);
                }
                complete
            }
            // No subplan: both operands are ordinary expressions over this row.
            BoundExpr::QuantifiedArray { array, cmp, .. } => {
                array.collect_column_refs(out) && cmp.collect_column_refs(out)
            }
            // Carries a subplan whose body may be correlated — see above.
            //
            // A window marker refuses for a different reason: it is transient,
            // so reaching here at all means the extraction pass missed it. Its
            // results also live in slots this row does not have yet, so any
            // answer would be a lie about a row shape that no longer applies.
            BoundExpr::ScalarSubquery { .. }
            | BoundExpr::Exists { .. }
            | BoundExpr::QuantifiedSubquery { .. }
            | BoundExpr::WindowFunc { .. } => false,
        }
    }

    /// [`Self::collect_column_refs`] over a slice, with the same contract: a
    /// `false` result leaves `out` in an unspecified state that the caller must
    /// discard.
    ///
    /// How much of `out` is populated on refusal is deliberately not promised —
    /// the sibling arms above short-circuit with `&&`, so a partial set is not
    /// a usable lower bound and must never be read as one.
    fn collect_all(exprs: &[BoundExpr], out: &mut BTreeSet<usize>) -> bool {
        exprs.iter().all(|expr| expr.collect_column_refs(out))
    }

    /// Add `delta` to every `ColumnRef` index, relocating this expression from
    /// one row layout to another. `delta` is signed: combining a comma group's
    /// merged-column view with the groups laid out before it shifts *up* by the
    /// group's base, while pushing a predicate from a join's combined row down
    /// into a subtree shifts *down* by that subtree's base offset.
    ///
    /// Mirrors [`Self::column_ref_bounds`] exactly — an expression this moves is
    /// an expression whose bounds that reports, and vice versa. In particular an
    /// `OuterColumnRef` addresses an enclosing row and is left untouched, as is a
    /// subplan's own body: only the `IN` needle and array operand of a
    /// quantified comparison index this row.
    pub fn shift_column_refs(&mut self, delta: isize) {
        match self {
            BoundExpr::ColumnRef { index, .. } => {
                let Some(shifted) = index.checked_add_signed(delta) else {
                    panic!("column index {index} shifted out of range by {delta}");
                };
                *index = shifted;
            }
            BoundExpr::Const { .. }
            | BoundExpr::Param { .. }
            | BoundExpr::OuterColumnRef { .. }
            | BoundExpr::ScalarSubquery { .. }
            | BoundExpr::Exists { .. } => {}
            BoundExpr::Unary { expr, .. }
            | BoundExpr::IsNull { expr, .. }
            | BoundExpr::BoolTest { expr, .. }
            | BoundExpr::Coerce { expr, .. }
            | BoundExpr::Collate { expr, .. }
            | BoundExpr::Reinterpret { expr, .. } => expr.shift_column_refs(delta),
            BoundExpr::Binary { left, right, .. } => {
                left.shift_column_refs(delta);
                right.shift_column_refs(delta);
            }
            // A routine's body has its own frame; only its arguments index this row.
            BoundExpr::FuncCall { args, .. }
            | BoundExpr::Routine { args, .. }
            | BoundExpr::Srf { args, .. }
            | BoundExpr::Aggregate { args, .. } => {
                args.iter_mut().for_each(|a| a.shift_column_refs(delta));
            }
            // Mirrors `column_ref_bounds`: the arguments and the OVER clause
            // both index this row, so both move with it.
            BoundExpr::WindowFunc { kind, spec, .. } => {
                kind.args_mut()
                    .iter_mut()
                    .chain(spec.exprs_mut())
                    .for_each(|a| a.shift_column_refs(delta));
            }
            BoundExpr::ArrayCtor { elems, .. } => {
                elems.iter_mut().for_each(|e| e.shift_column_refs(delta));
            }
            BoundExpr::Subscript { base, index, .. } => {
                base.shift_column_refs(delta);
                index.shift_column_refs(delta);
            }
            BoundExpr::Case { whens, else_, .. } => {
                for (c, r) in whens {
                    c.shift_column_refs(delta);
                    r.shift_column_refs(delta);
                }
                if let Some(e) = else_ {
                    e.shift_column_refs(delta);
                }
            }
            BoundExpr::QuantifiedSubquery { cmp, .. } => cmp.shift_column_refs(delta),
            BoundExpr::QuantifiedArray { array, cmp, .. } => {
                array.shift_column_refs(delta);
                cmp.shift_column_refs(delta);
            }
        }
    }
}

#[cfg(test)]
mod collect_column_refs_tests {
    use crabgresql_types::collation::DEFAULT_COLLATION_OID;

    use super::*;
    use crate::functions::ScalarFn;

    fn col(index: usize) -> BoundExpr {
        BoundExpr::ColumnRef {
            index,
            ty: PgType::Int4,
        }
    }

    fn refs(expr: &BoundExpr) -> Option<Vec<usize>> {
        let mut out = BTreeSet::new();
        expr.collect_column_refs(&mut out)
            .then(|| out.into_iter().collect())
    }

    /// A trivially empty subplan, enough to build the marker variants.
    fn subplan() -> Subplan {
        Subplan(Box::new(crate::logical_plan::LogicalPlan::Values(
            crate::logical_plan::ValuesPlan {
                columns: Vec::new(),
                rows: Vec::new(),
                predicate: None,
                sort: Vec::new(),
                distinct: None,
            },
        )))
    }

    #[test]
    fn only_the_deep_volatility_test_sees_inside_a_subquery_body() {
        // `EXISTS (SELECT nextval('s'))`. The shallow predicate stops at the
        // marker because a subquery body is a plan of its own; the deep one has
        // to cross it, or a caller reasoning about the *marker* — how many rows
        // reach it, how many times it is built — draws the wrong conclusion.
        let nextval = BoundExpr::FuncCall {
            func: ScalarFn::Nextval,
            ret: PgType::Int8,
            args: vec![BoundExpr::Const {
                value: Value::Text("s".into()),
                ty: PgType::Text,
            }],
        };
        let marker = BoundExpr::Exists {
            subplan: Subplan(Box::new(crate::logical_plan::LogicalPlan::Values(
                crate::logical_plan::ValuesPlan {
                    columns: Vec::new(),
                    rows: vec![vec![nextval.clone()]],
                    predicate: None,
                    sort: Vec::new(),
                    distinct: None,
                },
            ))),
            negated: false,
        };
        assert!(nextval.is_volatile_call());
        assert!(
            !marker.is_volatile_call(),
            "the marker itself is not a call"
        );
        assert!(
            !marker.contains_volatile_fn(),
            "shallow stops at the marker"
        );
        assert!(crate::plan::expr_contains_volatile_fn(&marker));
    }

    #[test]
    fn nested_expressions_report_every_column_they_read() {
        // CASE WHEN c3 THEN f(c1, c0) ELSE c0 + 1 END
        let expr = BoundExpr::Case {
            whens: vec![(
                col(3),
                BoundExpr::FuncCall {
                    func: ScalarFn::Power,
                    ret: PgType::Int4,
                    args: vec![col(1), col(0)],
                },
            )],
            else_: Some(Box::new(BoundExpr::Binary {
                op: BinOp::Add,
                arg_ty: PgType::Int4,
                collation: DEFAULT_COLLATION_OID,
                left: Box::new(col(0)),
                right: Box::new(BoundExpr::Const {
                    value: Value::Int4(1),
                    ty: PgType::Int4,
                }),
            })),
            ty: PgType::Int4,
        };
        assert_eq!(refs(&expr), Some(vec![0, 1, 3]));
    }

    /// An outer reference addresses an *enclosing* row, so it contributes
    /// nothing to this row's set — the same rule `column_ref_bounds` follows.
    #[test]
    fn an_outer_column_reference_contributes_nothing() {
        let expr = BoundExpr::IsNull {
            expr: Box::new(BoundExpr::OuterColumnRef {
                level: 1,
                index: 9,
                ty: PgType::Int4,
            }),
            negated: false,
        };
        assert_eq!(refs(&expr), Some(Vec::new()));
    }

    /// The load-bearing refusal: a correlated subplan body records its
    /// dependency on this row as an `OuterColumnRef` that no `BoundExpr` walk
    /// reaches, so pruning on a partial set would read NULL. All three
    /// subplan-carrying variants must decline.
    #[test]
    fn subplan_variants_refuse_to_report_a_set() {
        for expr in [
            BoundExpr::ScalarSubquery {
                subplan: subplan(),
                ty: PgType::Int4,
            },
            BoundExpr::Exists {
                subplan: subplan(),
                negated: false,
            },
            BoundExpr::QuantifiedSubquery {
                subplan: subplan(),
                all: false,
                cmp: Box::new(col(2)),
            },
        ] {
            assert_eq!(refs(&expr), None, "{expr:?} must refuse");
        }
    }

    /// One refusal anywhere in a tree poisons the whole result.
    #[test]
    fn a_refusal_propagates_through_enclosing_expressions() {
        let expr = BoundExpr::Binary {
            op: BinOp::And,
            arg_ty: PgType::Bool,
            collation: DEFAULT_COLLATION_OID,
            left: Box::new(col(4)),
            right: Box::new(BoundExpr::Exists {
                subplan: subplan(),
                negated: false,
            }),
        };
        assert_eq!(refs(&expr), None);
    }

    /// A quantified comparison over an *array* carries no subplan, so both of
    /// its operands are ordinary expressions over this row.
    #[test]
    fn a_quantified_array_reports_both_operands() {
        let expr = BoundExpr::QuantifiedArray {
            array: Box::new(col(5)),
            all: true,
            cmp: Box::new(col(1)),
        };
        assert_eq!(refs(&expr), Some(vec![1, 5]));
    }
}
