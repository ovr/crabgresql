//! Name resolution: [`Binding`], the visible relations a [`Scope`] holds, and
//! the column/qualifier lookups the binder resolves an identifier through.

use std::sync::Arc;

use crabgresql_parser::{Span, ast};
use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::{Column, TableEngine, TableSchema, TypeCatalog};
use crabgresql_types::{Numeric, Value};

use crate::BindError;
use crate::functions::ScalarFn;
use crate::logical_plan::SysCol;

use super::bound::BoundExpr;
use super::datatype::declared_typmod;
use super::params::ParamCtx;

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

/// One FROM item as a name-resolution scope is built from it: the qualifier it
/// is addressed by, the columns it exposes, and where its system column sits.
#[derive(Clone)]
pub struct ScopeItem {
    pub qualifier: String,
    pub columns: Vec<Column>,
    /// Local index within `columns` where this item's system columns start,
    /// when the query asked for any. They are real columns of the row — that is
    /// what lets an outer join null-extend them and an `Append` arm answer for
    /// itself — but they sit past every declared column, so `*` never reaches
    /// them. `None` for a FROM item that is not a relation scan.
    pub system_base: Option<usize>,
    /// Set when this relation's access method cannot produce system columns, so
    /// none were appended and a reference to one must be refused rather than
    /// reported missing. See [`DeclinedSystem`].
    pub declined_system: Option<DeclinedSystem>,
}

/// Why a relation carries no system-column slots: its access method stores
/// column chunks rather than row versions, so there is no tid that addresses a
/// row and no MVCC header to read.
///
/// Carried through name resolution rather than acted on when the FROM item is
/// bound, because what the binder knows there is the *demanded* set — an
/// over-approximation over rendered SQL, so a string literal spelling `xmin`
/// lands in it. Only a reference that actually resolves deserves the error.
#[derive(Clone, Debug)]
pub struct DeclinedSystem {
    pub access_method: &'static str,
    pub relation: String,
}

impl DeclinedSystem {
    /// The `0A000` a resolved reference to `column` raises.
    fn refuse(&self, column: &str) -> BindError {
        BindError::feature_not_supported(format!(
            "access method \"{}\" does not support system column \"{column}\" on relation \"{}\"",
            self.access_method, self.relation,
        ))
    }
}

impl ScopeItem {
    /// The columns `*` expands to: everything before the system slots.
    pub fn declared(&self) -> &[Column] {
        match self.system_base {
            Some(base) => &self.columns[..base],
            None => &self.columns,
        }
    }
}

/// One relation in a name-resolution scope: its qualifier (alias, else table
/// name), its columns, the base index its columns occupy in the combined
/// row (0 for a single relation; the running total across FROM items in a
/// cross join), and where its system column sits among them.
#[derive(Clone)]
pub struct ScopeRel {
    qualifier: String,
    columns: Vec<Column>,
    offset: usize,
    system_base: Option<usize>,
    declined_system: Option<DeclinedSystem>,
}

impl ScopeRel {
    /// This relation's declared columns — everything `*` expands to, which is
    /// every column before the system slots.
    fn declared(&self) -> &[Column] {
        match self.system_base {
            Some(base) => &self.columns[..base],
            None => &self.columns,
        }
    }
}

/// A snapshot of one enclosing query's name-resolution view, kept so a
/// correlated subquery can resolve an outer column: its relations (for qualified
/// `q.c` and the plain unqualified case) plus the merged-join `visible` view, so
/// an unqualified reference to a `USING`/`NATURAL` join column of an outer query
/// resolves to the single merged column (as in PG) rather than being reported
/// ambiguous. Level 1 is the immediate parent; deeper ancestors follow.
///
/// A level carrying a [`LateralBarrier`] is not an enclosing query at all — see
/// that type — and so does not consume a level number.
#[derive(Clone)]
pub(crate) struct OuterLevel {
    rels: Vec<ScopeRel>,
    visible: Option<Vec<VisibleColumn>>,
    barrier: Option<LateralBarrier>,
}

/// A pseudo outer level holding FROM items that sit *beside* the one being
/// bound rather than above it: they precede it in the same FROM clause, so a
/// reference to one is neither a local column nor a correlated reference to an
/// enclosing query — it is the specific mistake PostgreSQL names.
///
/// Resolution consults these before the real enclosing queries, which is what
/// makes a same-level FROM item shadow a like-named outer one, and raises
/// instead of binding.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LateralBarrier {
    /// A plain subquery in FROM: legal to write, but only `LATERAL` lets it see
    /// its siblings.
    NeedsLateral,
    /// The same, on the right of a `RIGHT`/`FULL` join — where writing `LATERAL`
    /// would not help either, so PostgreSQL states the fact and offers no hint.
    NeverReachable,
    /// A `LATERAL` item on the right of a `RIGHT`/`FULL` join, where the left
    /// row it would reference may not exist at all.
    WrongJoinType,
    /// A `LATERAL` item *anywhere in an explicit join chain* — leading it or
    /// inside it — referencing a FROM item from an earlier comma-separated
    /// group. PostgreSQL answers these queries; we do not, because the join tree
    /// cross-joins the groups *above* the chain, so that item's columns are not
    /// in the row any node of the chain is fed.
    ///
    /// A barrier rather than a silent fall-through to the enclosing queries,
    /// which would bind a like-named outer relation and answer a different
    /// question — and rather than binding it anyway, which would leave a leaf
    /// holding `level: 1` references no join node above it can substitute.
    OtherGroup,
}

impl LateralBarrier {
    /// The `42P01` a qualified reference (`t.c`) through this barrier raises.
    /// The qualifier alone decides — PostgreSQL reports this even when the
    /// relation has no column by that name.
    fn refuse_relation(self, qualifier: &str) -> BindError {
        let err = BindError::new(
            sqlstate::UNDEFINED_TABLE,
            format!("invalid reference to FROM-clause entry for table \"{qualifier}\""),
        );
        match self {
            LateralBarrier::NeedsLateral | LateralBarrier::NeverReachable => {
                let err = err.with_detail(Some(format!(
                    "There is an entry for table \"{qualifier}\", but it cannot be referenced \
                     from this part of the query."
                )));
                err.with_hint(self.lateral_would_help().then(|| {
                    "To reference that table, you must mark this subquery with LATERAL.".to_string()
                }))
            }
            LateralBarrier::WrongJoinType => err.with_detail(Some(
                "The combining JOIN type must be INNER or LEFT for a LATERAL reference."
                    .to_string(),
            )),
            LateralBarrier::OtherGroup => BindError::feature_not_supported(format!(
                "LATERAL reference to \"{qualifier}\" from another comma-separated FROM item \
                 is not supported yet"
            )),
        }
    }

    /// The error an *unqualified* column name found behind this barrier raises.
    /// A plain FROM subquery gets a column-shaped wording; a `LATERAL` item
    /// across the wrong join type is blamed on the relation either way.
    fn refuse_column(self, column: &str, qualifier: &str) -> BindError {
        match self {
            LateralBarrier::NeedsLateral | LateralBarrier::NeverReachable => {
                let err = BindError::new(
                    sqlstate::UNDEFINED_COLUMN,
                    format!("column \"{column}\" does not exist"),
                )
                .with_detail(Some(format!(
                    "There is a column named \"{column}\" in table \"{qualifier}\", but it \
                     cannot be referenced from this part of the query."
                )));
                err.with_hint(self.lateral_would_help().then(|| {
                    "To reference that column, you must mark this subquery with LATERAL."
                        .to_string()
                }))
            }
            LateralBarrier::WrongJoinType | LateralBarrier::OtherGroup => {
                self.refuse_relation(qualifier)
            }
        }
    }

    /// Whether writing `LATERAL` would actually reach the item — the one thing
    /// the HINT claims.
    fn lateral_would_help(self) -> bool {
        self == LateralBarrier::NeedsLateral
    }
}

/// Lay `items` out as scope relations, each column's index its position in the
/// concatenated row.
fn scope_rels(items: impl IntoIterator<Item = ScopeItem>) -> Vec<ScopeRel> {
    let mut offset = 0;
    items
        .into_iter()
        .map(|item| {
            let rel = ScopeRel {
                qualifier: item.qualifier,
                columns: item.columns,
                offset,
                system_base: item.system_base,
                declined_system: item.declined_system,
            };
            offset += rel.columns.len();
            rel
        })
        .collect()
}

/// The outer-level chain a FROM item binds against when the items to its left
/// are visible but off limits: a barrier level naming them, then the real
/// enclosing queries unchanged (the barrier consumes no level number).
///
/// The offsets `scope_rels` assigns are never read here — every match through a
/// barrier is an error, not an expression.
pub(crate) fn barrier_levels(
    items: impl IntoIterator<Item = ScopeItem>,
    barrier: LateralBarrier,
    outer: &[OuterLevel],
) -> Vec<OuterLevel> {
    let mut levels = Vec::with_capacity(outer.len() + 1);
    levels.push(OuterLevel {
        rels: scope_rels(items),
        visible: None,
        barrier: Some(barrier),
    });
    levels.extend(outer.iter().cloned());
    levels
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

/// A column resolved by name: which relation of the scope holds it, and where
/// within that relation.
///
/// Deliberately *not* the combined-row index: every caller turns this into an
/// expression through [`Scope::column_expr`], which needs the relation itself —
/// a virtual generated column's substitution binds against that relation's own
/// columns before it is rebased.
#[derive(Clone, Copy)]
struct ResolvedColumn {
    rel: usize,
    local: usize,
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

/// A column read straight out of the row: its `ColumnRef` at `index`, wrapped
/// in the column's declared collation.
pub(crate) fn plain_column_ref(column: &Column, index: usize) -> BoundExpr {
    with_column_collation(
        BoundExpr::ColumnRef {
            index,
            ty: column.ty,
        },
        column.collation,
    )
}

/// The expression a reference to `columns[local]` binds to, for a relation
/// whose columns start at `offset` in the combined row: the plain `ColumnRef`,
/// or — for a **virtual** generated column — its generation expression rebased
/// onto that row.
///
/// The one substitution site. Both name resolution ([`Scope::column_expr`]) and
/// the merged-join `visible` view ([`crate::plan::default_visible`]) go through
/// it, because a virtual column that keeps its slot in either place reads back
/// as the NULL nothing was ever stored in.
///
/// The expression is re-parsed and re-bound per reference. That is the same
/// per-statement cost a stored CHECK or a column default already pays, and it
/// keeps the substitution honest: the expression binds against the relation's
/// *current* shape, not one cached when the scope was built.
pub(crate) fn column_value(
    columns: &[Column],
    local: usize,
    offset: usize,
    qualifier: &str,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<BoundExpr, BindError> {
    let column = &columns[local];
    if !column.is_virtual_generated() {
        return Ok(plain_column_ref(column, offset + local));
    }
    // The stored text is unqualified (the DDL deparse drops the `t.`), so it
    // resolves against the relation's own columns whatever the caller calls the
    // relation here.
    let schema = TableSchema::new(qualifier.to_string(), columns.to_vec());
    let mut bound = crate::bind_stored_generation(&schema, local, catalog)?;
    bound.shift_column_refs(offset as isize);
    Ok(with_column_collation(bound, column.collation))
}

/// Find `name` among `rels`' columns, returning it (`Ok`) — or `Err(())` for
/// more than one match (ambiguous), or `None` for no match. Shared by local and
/// outer (correlated) unqualified resolution.
fn lookup_in_rels(rels: &[ScopeRel], name: &str) -> Option<Result<ResolvedColumn, ()>> {
    let mut found: Option<ResolvedColumn> = None;
    for (r, rel) in rels.iter().enumerate() {
        for (local, col) in rel.columns.iter().enumerate() {
            if col.name == name {
                if found.is_some() {
                    return Some(Err(()));
                }
                found = Some(ResolvedColumn { rel: r, local });
            }
        }
    }
    found.map(Ok)
}

/// Resolve `column` within a single relation `rel` for a qualified
/// `qualifier.column` reference, returning its *local* index within that
/// relation. `42702` if the relation exposes the name more than once (e.g. an
/// alias list `v(x, x)`), or `42703` — spelled with the qualifier, as PG does —
/// if it is absent.
fn column_in_rel(rel: &ScopeRel, qualifier: &str, column: &str) -> Result<usize, BindError> {
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
    local.ok_or_else(|| {
        declined_or_missing(rel, column, || {
            BindError::new(
                sqlstate::UNDEFINED_COLUMN,
                format!("column {qualifier}.{column} does not exist"),
            )
        })
    })
}

/// The error for a name `rel` does not expose: `0A000` when the name is a system
/// column this relation's access method declines, `missing()` otherwise.
///
/// The two are worth telling apart. A relation that simply lacks the column is a
/// typo; one whose storage cannot produce it is a capability the query has to be
/// written around, and saying so names both the column and the access method.
fn declined_or_missing(
    rel: &ScopeRel,
    column: &str,
    missing: impl FnOnce() -> BindError,
) -> BindError {
    match &rel.declined_system {
        Some(declined) if SysCol::ALL.iter().any(|c| c.name() == column) => declined.refuse(column),
        _ => missing(),
    }
}

/// A SELECT's `WINDOW w AS (…)` definitions, by normalized name.
///
/// Each stored spec is already **expanded**: a definition that names an earlier
/// one (`WINDOW w2 AS (w1 ORDER BY x)`) has the base merged in at build time, so
/// nothing downstream has to resolve a base recursively. Shared (`Rc`) because
/// the same map is threaded into every clause scope of one SELECT.
pub(crate) type NamedWindows = std::rc::Rc<std::collections::HashMap<String, ast::WindowSpec>>;

/// Reject a window call in a clause that is evaluated before windows are, naming
/// the clause the way PostgreSQL does.
///
/// Use this where aggregates are *legal* but windows are not — HAVING (which
/// filters the grouped rows windows are computed over) and a window definition's
/// own `PARTITION BY`/`ORDER BY`. Everywhere else, prefer
/// [`reject_agg_or_window`], which also reports a misplaced aggregate.
pub(crate) fn reject_window(expr: &BoundExpr, clause: &str) -> Result<(), BindError> {
    if expr.contains_window() {
        return Err(window_not_allowed(clause));
    }
    Ok(())
}

/// Reject an aggregate *or* a window call in a clause that allows neither,
/// blaming whichever comes first in source order — as PostgreSQL does.
pub fn reject_agg_or_window(expr: &BoundExpr, clause: &str) -> Result<(), BindError> {
    match expr.first_agg_or_window() {
        Some(BoundExpr::WindowFunc { .. }) => Err(window_not_allowed(clause)),
        Some(_) => Err(BindError::new(
            sqlstate::GROUPING_ERROR,
            format!("aggregate functions are not allowed in {clause}"),
        )),
        None => Ok(()),
    }
}

fn window_not_allowed(clause: &str) -> BindError {
    BindError::new(
        sqlstate::WINDOWING_ERROR,
        format!("window functions are not allowed in {clause}"),
    )
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
        BoundExpr::BoolTest {
            expr,
            value,
            negated,
        } => BoundExpr::BoolTest {
            expr: Box::new(outerize_columns(expr, level)),
            value: *value,
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
        // Only the operand: a domain's predicates live in their own one-column
        // world, where `VALUE` is index 0 and no outer level exists.
        BoundExpr::CoerceToDomain { expr, domain } => BoundExpr::CoerceToDomain {
            expr: Box::new(outerize_columns(expr, level)),
            domain: domain.clone(),
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
            else_: else_.as_ref().map(|e| Box::new(outerize_columns(e, level))),
            ty: *ty,
        },
        BoundExpr::Coalesce { args, ty } => BoundExpr::Coalesce {
            args: args.iter().map(|a| outerize_columns(a, level)).collect(),
            ty: *ty,
        },
        BoundExpr::MinMax {
            kind,
            args,
            ty,
            collation,
        } => BoundExpr::MinMax {
            kind: *kind,
            args: args.iter().map(|a| outerize_columns(a, level)).collect(),
            ty: *ty,
            collation: *collation,
        },
        // A merged-join visible column expression is only ever a ColumnRef or a
        // COALESCE/CASE over ColumnRefs; these never appear, so clone defensively.
        BoundExpr::Srf { .. }
        | BoundExpr::Aggregate { .. }
        | BoundExpr::WindowFunc { .. }
        | BoundExpr::ScalarSubquery { .. }
        | BoundExpr::ArraySubquery { .. }
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
    /// appear (column defaults, CHECK constraints, EXECUTE parameters), which
    /// then reject one.
    pub(super) subquery: Option<std::rc::Rc<SubqueryContext>>,
    /// The enclosing queries' resolution views, nearest first (index 0 = the
    /// immediate parent = correlation level 1). Empty for a top-level query;
    /// populated by [`Scope::with_outer`] when binding a subquery body so an
    /// unresolved name can fall through to the outer scope as an
    /// [`BoundExpr::OuterColumnRef`].
    outer: Vec<OuterLevel>,
    /// The named-parameter namespace of the `LANGUAGE SQL` function body being
    /// bound, if any. `None` for an ordinary statement, where a bare identifier
    /// can only be a column.
    func_params: Option<std::rc::Rc<FuncParams>>,
    /// The `WINDOW w AS (…)` definitions of the SELECT being bound, so `OVER w`
    /// resolves. `None` where a `WINDOW` clause cannot appear.
    named_windows: Option<NamedWindows>,
    /// Whether a reference to a **virtual** generated column resolves to its
    /// generation expression rather than to the (empty) slot the column occupies
    /// in the row.
    ///
    /// True everywhere a row is *read*, which is what makes a virtual column
    /// work at all: nothing is stored for it, so the value has to be recomputed
    /// from the row's other columns. The two DDL binders turn it off, and each
    /// needs the raw reference for its own reason — a stored CHECK records the
    /// column positions it reads, and a generation expression must *refuse* a
    /// reference to another generated column rather than quietly inline it.
    expand_generated: bool,
}

/// The handle a [`Scope`] carries so `bind_expr` can bind a nested query. Shared
/// (`Rc`) so cheaply threaded into the transient scopes built per clause.
pub(super) struct SubqueryContext {
    pub(super) engine: Arc<dyn TableEngine>,
    pub(super) ctes: crate::plan::CteEnv,
}

/// The parameter namespace of a `LANGUAGE SQL` function body: the routine's own
/// name (which qualifies a parameter, as in `f.value`) and each *named*
/// parameter's 0-based `$n` slot. Arguments declared without a name are absent
/// and stay reachable only as `$n`.
pub(super) struct FuncParams {
    func_name: String,
    params: Vec<(String, usize)>,
}

impl FuncParams {
    fn slot(&self, name: &str) -> Option<usize> {
        self.params
            .iter()
            .find(|(p, _)| p == name)
            .map(|(_, index)| *index)
    }
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
            func_params: None,
            named_windows: None,
            expand_generated: true,
        }
    }

    /// The scope a `LANGUAGE SQL` function body binds against: no tables, but
    /// the declared parameter names resolve to their `$n` slots, so a body may
    /// say `value` (or `f.value`) where it could also say `$1`. `arg_names` is
    /// positionally aligned with the seeded parameter types.
    pub fn function_body(
        catalog: &Arc<dyn TypeCatalog>,
        params: &ParamCtx,
        func_name: &str,
        arg_names: &[Option<String>],
    ) -> Scope {
        let named = arg_names
            .iter()
            .enumerate()
            .filter_map(|(index, name)| name.clone().map(|n| (n, index)))
            .collect();
        Scope {
            func_params: Some(std::rc::Rc::new(FuncParams {
                func_name: func_name.to_string(),
                params: named,
            })),
            ..Scope::empty(catalog, params)
        }
    }

    /// A one-relation scope over `schema`.
    ///
    /// `system` lists the system columns the row this scope describes carries
    /// past its declared ones, in row order. The write paths fill it from what
    /// the statement names, because there the values are appended per *target*
    /// — the partition or child a row actually lives in, not the relation the
    /// statement named.
    pub fn table(
        schema: &TableSchema,
        qualifier: String,
        catalog: &Arc<dyn TypeCatalog>,
        params: &ParamCtx,
        system: &[SysCol],
        declined_system: Option<DeclinedSystem>,
    ) -> Scope {
        let mut columns = schema.columns.clone();
        let system_base = (!system.is_empty()).then(|| {
            let base = columns.len();
            columns.extend(system.iter().map(|c| Column::new(c.name(), c.ty())));
            base
        });
        Scope {
            rels: vec![ScopeRel {
                qualifier,
                columns,
                offset: 0,
                system_base,
                declined_system,
            }],
            visible: None,
            catalog: catalog.clone(),
            params: params.clone(),
            subquery: None,
            outer: Vec::new(),
            func_params: None,
            named_windows: None,
            expand_generated: true,
        }
    }

    /// A multi-relation scope for a cross join. Each item becomes a relation;
    /// offsets are assigned left-to-right so a column's index is its position in
    /// the concatenated row.
    ///
    /// `visible`, when `Some`, is the merged-column view of a FROM clause
    /// containing a `USING`/`NATURAL` join: the `rels` are still built from
    /// `items` (so qualified `q.c` resolves each side's own column), but
    /// unqualified resolution and `*` expansion follow the merged view.
    pub fn relations_with_visible(
        items: Vec<ScopeItem>,
        visible: Option<Vec<VisibleColumn>>,
        catalog: &Arc<dyn TypeCatalog>,
        params: &ParamCtx,
    ) -> Scope {
        Scope {
            rels: scope_rels(items),
            visible,
            catalog: catalog.clone(),
            params: params.clone(),
            subquery: None,
            outer: Vec::new(),
            func_params: None,
            named_windows: None,
            expand_generated: true,
        }
    }

    /// The scope a **stored per-row expression** binds against: a CHECK
    /// predicate, a generation expression, or either of them re-bound from the
    /// catalog. It is [`Scope::table`] over the relation's raw storage layout —
    /// no system slot, no subquery context, and no generated-column expansion.
    ///
    /// All three properties are the same decision seen from different sides: the
    /// expression is evaluated against a tuple, once per row, by whoever holds
    /// it. A system column would mean a different relation — and a different
    /// row version — per leaf it is re-bound for; a subquery has no plan to run
    /// there; and a reference to a generated
    /// column must stay a reference, so that a CHECK can record its ordinal and
    /// a generation expression can *refuse* it.
    pub fn stored_row(
        schema: &TableSchema,
        catalog: &Arc<dyn TypeCatalog>,
        params: &ParamCtx,
    ) -> Scope {
        Scope {
            expand_generated: false,
            ..Scope::table(schema, schema.name.clone(), catalog, params, &[], None)
        }
    }

    /// The expression a resolved column binds to, within one relation of this
    /// scope: its `ColumnRef` in the combined row, or — for a virtual generated
    /// column this scope expands — the generation expression, rebased onto the
    /// combined row.
    fn column_expr(&self, rel: &ScopeRel, local: usize) -> Result<BoundExpr, BindError> {
        if !self.expand_generated {
            return Ok(plain_column_ref(&rel.columns[local], rel.offset + local));
        }
        column_value(
            &rel.columns,
            local,
            rel.offset,
            &rel.qualifier,
            &self.catalog,
        )
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

    /// Attach the SELECT's `WINDOW w AS (…)` definitions so `OVER w` resolves.
    /// Set by the clause binders that bind a target list / ORDER BY, alongside
    /// [`Scope::with_subqueries`].
    pub(crate) fn with_named_windows(mut self, windows: &NamedWindows) -> Scope {
        self.named_windows = Some(windows.clone());
        self
    }

    /// The `WINDOW` definition named `name`, if this scope has one.
    pub(crate) fn named_window(&self, name: &str) -> Option<&ast::WindowSpec> {
        self.named_windows.as_ref()?.get(name)
    }

    /// The width of the row this scope's `ColumnRef` indices address — the
    /// concatenation of every relation in FROM order. `0` for a FROM-less
    /// SELECT.
    ///
    /// Equals `JoinExpr::width()` for the same FROM clause. A `USING`/`NATURAL`
    /// join's merged `visible` view does not change it: merged columns are
    /// expressions over both sides' `ColumnRef`s in this same index space, not
    /// extra columns.
    pub fn width(&self) -> usize {
        self.rels
            .last()
            .map_or(0, |rel| rel.offset + rel.columns.len())
    }

    /// The projection list that reproduces this scope's row unchanged:
    /// `ColumnRef(0), …, ColumnRef(width-1)`.
    ///
    /// Built from `rels` rather than [`Scope::expand_wildcard`] because that
    /// honors the merged `visible` view and so would drop a `USING` join's
    /// duplicate column — this must be the *raw* row, since the expressions
    /// layered above it index that row positionally.
    pub fn identity_projection(&self) -> Vec<BoundExpr> {
        self.rels
            .iter()
            .flat_map(|rel| {
                rel.columns
                    .iter()
                    .enumerate()
                    .map(|(i, col)| BoundExpr::ColumnRef {
                        index: rel.offset + i,
                        ty: col.ty,
                    })
            })
            .collect()
    }

    /// The outer-level views a subquery bound against this scope should see:
    /// this scope's own relations as level 1, then this scope's own outer levels
    /// (the ancestors) shifted one deeper.
    pub(crate) fn as_outer_levels(&self) -> Vec<OuterLevel> {
        let mut levels = Vec::with_capacity(self.outer.len() + 1);
        levels.push(OuterLevel {
            rels: self.rels.clone(),
            visible: self.visible.clone(),
            barrier: None,
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
    pub(super) fn resolve(&self, name: &str) -> Result<BoundExpr, BindError> {
        match self.resolve_local(name)? {
            NameLookup::Found(expr) => Ok(expr),
            NameLookup::Ambiguous => Err(BindError::new(
                sqlstate::AMBIGUOUS_COLUMN,
                format!("column reference \"{name}\" is ambiguous"),
            )),
            // A SQL function body's parameter names are consulted only after its
            // relations, as in PG, where a column shadows a same-named parameter.
            // Moot today: a function body is FROM-less, so `rels` is empty.
            NameLookup::Missing => match self.func_param(name)? {
                Some(expr) => Ok(expr),
                None => self.resolve_outer(name),
            },
        }
    }

    /// Bind `name` as a parameter of the SQL function body being bound, if it is
    /// one. Registering the reference mirrors [`bind_placeholder`] so the
    /// parameter context's bookkeeping is the same whichever spelling was used;
    /// the type is always known, since the body's context is seeded with every
    /// declared argument type.
    fn func_param(&self, name: &str) -> Result<Option<BoundExpr>, BindError> {
        let Some(index) = self.func_params.as_ref().and_then(|f| f.slot(name)) else {
            return Ok(None);
        };
        let index = self.params.borrow_mut().reference(index + 1)?;
        let ty = self
            .params
            .borrow()
            .slot_type(index)
            .expect("SQL function body parameter types are seeded at bind");
        Ok(Some(BoundExpr::Param { index, ty }))
    }

    /// Look a name up in this scope's own relations (or its merged-join `visible`
    /// view), without consulting enclosing queries.
    fn resolve_local(&self, name: &str) -> Result<NameLookup, BindError> {
        // A merged join namespace resolves unqualified names against its visible
        // columns: the join column appears once (never ambiguous), the merged
        // expression carrying its combined-row value.
        if let Some(visible) = &self.visible {
            // A merged join namespace hides the inputs' own columns, but a
            // system column never takes part in `USING`, so a name it does not
            // claim still falls through to the relations below.
            match lookup_visible(visible, name) {
                VisibleLookup::Found(expr) => return Ok(NameLookup::Found(expr.clone())),
                VisibleLookup::Ambiguous => return Ok(NameLookup::Ambiguous),
                VisibleLookup::Missing => {}
            }
        }
        match lookup_in_rels(&self.rels, name) {
            Some(Ok(col)) => Ok(NameLookup::Found(
                self.column_expr(&self.rels[col.rel], col.local)?,
            )),
            Some(Err(())) => Ok(NameLookup::Ambiguous),
            None => Ok(NameLookup::Missing),
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
        let mut level_no = 0;
        for level in self.outer.iter() {
            // A barrier level is not an enclosing query, so it leaves the
            // level numbering of the real ancestors alone.
            if let Some(barrier) = level.barrier {
                if let Some(rel) = level
                    .rels
                    .iter()
                    .find(|rel| rel.columns.iter().any(|c| c.name == name))
                {
                    return Err(barrier.refuse_column(name, &rel.qualifier));
                }
                continue;
            }
            level_no += 1;
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
                    // A virtual generated column of an enclosing query resolves
                    // to its expression over *that* query's row, so the whole
                    // substituted tree becomes correlated at this level.
                    let expr = self.column_expr(&level.rels[col.rel], col.local)?;
                    return Ok(outerize_columns(&expr, level_no));
                }
                Some(Err(())) => return Err(ambiguous()),
                None => continue,
            }
        }
        // A relation in scope whose access method declines system columns
        // explains the miss better than "does not exist" does, and it is the
        // only relation that could have exposed the name.
        if let Some(rel) = self
            .rels
            .iter()
            .find(|rel| rel.declined_system.is_some())
            .filter(|_| SysCol::ALL.iter().any(|c| c.name() == name))
        {
            return Err(declined_or_missing(rel, name, || unreachable!("filtered")));
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
    pub(super) fn resolve_qualified(
        &self,
        qualifier: &str,
        column: &str,
    ) -> Result<BoundExpr, BindError> {
        // Inside a SQL function body, the routine's own name qualifies its
        // parameters (`f.value`). A member that is not a parameter falls through
        // to the `42P01` below — what PG reports for `f.nosuchparam` too.
        if self
            .func_params
            .as_ref()
            .is_some_and(|f| f.func_name == qualifier)
            && let Some(expr) = self.func_param(column)?
        {
            return Ok(expr);
        }
        if let Some(rel) = self.rels.iter().find(|r| r.qualifier == qualifier) {
            let local = column_in_rel(rel, qualifier, column)?;
            return self.column_expr(rel, local);
        }
        let mut level_no = 0;
        for level in self.outer.iter() {
            // The qualifier alone decides — PG reports this for `t.nosuch`
            // too, without ever asking whether `t` has such a column.
            if let Some(barrier) = level.barrier {
                if level.rels.iter().any(|r| r.qualifier == qualifier) {
                    return Err(barrier.refuse_relation(qualifier));
                }
                continue;
            }
            level_no += 1;
            if let Some(rel) = level.rels.iter().find(|r| r.qualifier == qualifier) {
                let local = column_in_rel(rel, qualifier, column)?;
                let expr = self.column_expr(rel, local)?;
                return Ok(outerize_columns(&expr, level_no));
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
    pub fn expand_wildcard(&self) -> Result<Vec<(crate::OutputColumn, BoundExpr)>, BindError> {
        // With a merged join namespace, `*` follows the visible columns: merged
        // join columns first (in clause order), then each input's remaining
        // columns — the join column appearing exactly once, as in PG.
        if let Some(visible) = &self.visible {
            return Ok(visible
                .iter()
                .map(|col| {
                    let (collation, strength) = crate::collation::output_collation(&col.expr);
                    (
                        crate::OutputColumn {
                            name: col.name.clone(),
                            ty: col.expr.ty(),
                            collation,
                            strength,
                            typmod: expr_typmod(&col.expr, self),
                            // A merged join column is an expression over both
                            // sides, not a relation's column.
                            generated: None,
                        },
                        col.expr.clone(),
                    )
                })
                .collect());
        }
        let mut out = Vec::new();
        for rel in &self.rels {
            self.expand_rel(rel, &mut out)?;
        }
        Ok(out)
    }

    /// The type modifier of the column at a combined-row `index`, or `-1` when
    /// it has none (or the index is past every relation).
    pub fn column_typmod(&self, index: usize) -> i32 {
        typmod_at(&self.rels, index)
    }

    /// The same, for a correlated reference `level` query levels up (1 = the
    /// immediate parent).
    pub fn outer_column_typmod(&self, level: usize, index: usize) -> i32 {
        match self
            .outer
            .iter()
            .filter(|l| l.barrier.is_none())
            .nth(level.wrapping_sub(1))
        {
            Some(outer) => typmod_at(&outer.rels, index),
            None => -1,
        }
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
        self.expand_rel(rel, &mut out)?;
        Ok(out)
    }

    /// Append every *declared* column of `rel` as an `(output column,
    /// expression)` pair — a `ColumnRef` at its combined-row index, or a virtual
    /// generated column's substituted expression. A system column is reachable
    /// by name and by name alone, so `*` and `q.*` skip the slot, as they do
    /// upstream.
    fn expand_rel(
        &self,
        rel: &ScopeRel,
        out: &mut Vec<(crate::OutputColumn, BoundExpr)>,
    ) -> Result<(), BindError> {
        for (i, col) in rel.declared().iter().enumerate() {
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
                    typmod: col.typmod,
                    // What `*` exposes is this column's *value*: a virtual one
                    // was already substituted by `column_expr` just below.
                    generated: None,
                },
                self.column_expr(rel, i)?,
            ));
        }
        Ok(())
    }
}

/// The modifier of the column at a combined-row `index` within `rels`, or `-1`
/// when the index is past every relation.
fn typmod_at(rels: &[ScopeRel], index: usize) -> i32 {
    for rel in rels {
        if index >= rel.offset && index < rel.offset + rel.columns.len() {
            return rel.columns[index - rel.offset].typmod;
        }
    }
    -1
}

/// The type modifier a projected expression carries, as PostgreSQL derives it:
/// a modifier survives a reference and an explicit coercion, and nothing else.
/// `CREATE VIEW` stores the result on the view's column, so `\d v` can print
/// `character varying(20)`.
///
/// The bound tree already records every coercion — `x::varchar(9)` is a
/// `FuncCall` whose second argument is the modifier — so this reads them back
/// rather than needing a slot on [`BoundExpr`]. The exception is a *folded*
/// constant, where the wrapper is gone by the time we get here; the projection
/// sites handle that by consulting the AST first (see
/// [`crate::declared_typmod`]).
pub(super) fn expr_typmod(expr: &BoundExpr, scope: &Scope) -> i32 {
    match expr {
        BoundExpr::ColumnRef { index, .. } => scope.column_typmod(*index),
        BoundExpr::OuterColumnRef { level, index, .. } => scope.outer_column_typmod(*level, *index),
        // Value-transparent wrappers.
        BoundExpr::Collate { expr, .. } => expr_typmod(expr, scope),
        BoundExpr::FuncCall { func, args, .. } => {
            let arg = |i: usize| match args.get(i) {
                Some(BoundExpr::Const {
                    value: Value::Int4(n),
                    ..
                }) => Some(*n),
                _ => None,
            };
            match func {
                ScalarFn::VarcharTypmod
                | ScalarFn::BpcharTypmod
                | ScalarFn::BitTypmod
                | ScalarFn::VarbitTypmod
                | ScalarFn::TimeApplyTypmod
                | ScalarFn::IntervalTypmod => arg(1).unwrap_or(-1),
                // `numeric` is the one modifier that packs two numbers, and it
                // travels as two separate arguments at run time.
                ScalarFn::NumApplyTypmod => match (arg(1), arg(2)) {
                    (Some(p), Some(s)) => Numeric::pack_typmod(p, s),
                    _ => -1,
                },
                _ => -1,
            }
        }
        // `CASE` keeps a modifier only when every arm agrees on it, the same
        // rule PostgreSQL applies when it resolves a common modifier over a set
        // of alternative results.
        BoundExpr::Case { whens, else_, .. } => {
            let arms = whens
                .iter()
                .map(|(_, result)| result)
                .chain(else_.as_deref());
            common_typmod(arms.map(|arm| expr_typmod(arm, scope)))
        }
        // `COALESCE` and `GREATEST`/`LEAST` resolve their modifier by the same
        // rule over their arguments: `coalesce(varchar(3), varchar(3))` is
        // `varchar(3)`, while `coalesce(varchar(3), varchar(5))` is bare `varchar`.
        BoundExpr::Coalesce { args, .. } | BoundExpr::MinMax { args, .. } => {
            common_typmod(args.iter().map(|arg| expr_typmod(arg, scope)))
        }
        // A scalar subquery reports its single output column's modifier.
        BoundExpr::ScalarSubquery { subplan, .. } => crate::plan::output_columns_of(&subplan.plan)
            .ok()
            .and_then(|columns| columns.first().map(|c| c.typmod))
            .unwrap_or(-1),
        _ => -1,
    }
}

/// The type modifier a projected select-list item carries.
///
/// A top-level cast is read off the *written* type name rather than the bound
/// tree, because a cast over a constant folds away at bind time and takes the
/// modifier with it — PostgreSQL keeps it (`select 'abc'::varchar(9)` in a view
/// is a `character varying(9)` column), and reading the AST is how we do too.
/// Everything else comes from [`expr_typmod`].
pub(crate) fn projection_typmod(expr: &ast::Expr, bound: &BoundExpr, scope: &Scope) -> i32 {
    if let ast::Expr::Cast { data_type, .. } = expr {
        // A modifier that failed its range check already errored while binding
        // the cast itself, so an error here cannot happen; treat it as "none"
        // rather than duplicating the diagnostic.
        if let Ok(Some(m)) = declared_typmod(bound.ty(), data_type) {
            return m;
        }
        return -1;
    }
    expr_typmod(bound, scope)
}

/// The modifier a set of alternatives share, or `-1` if they disagree (or there
/// are none).
pub(crate) fn common_typmod(mut typmods: impl Iterator<Item = i32>) -> i32 {
    let Some(first) = typmods.next() else {
        return -1;
    };
    if first >= 0 && typmods.all(|m| m == first) {
        first
    } else {
        -1
    }
}

/// Unquoted identifiers fold to lowercase, as in PG.
pub(crate) fn normalize_ident(ident: &ast::Ident) -> String {
    match ident.quote_style {
        Some(_) => ident.value.clone(),
        None => ident.value.to_lowercase(),
    }
}
