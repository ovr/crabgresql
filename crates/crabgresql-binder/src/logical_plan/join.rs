//! The join tree: a recursively bound FROM source and the SELECT node over it.

use std::sync::Arc;

use crabgresql_storage_api::TableAm;

use crate::expr::BoundExpr;
use crate::{OutputColumn, TableFn};

use super::{DistinctKey, LogicalPlan, SortKey};

/// [`LogicalPlan::Join`]: leaf rows are laid out left-to-right in the combined
/// row; the same projection/predicate/sort pipeline as [`QueryPlan`] runs on
/// top, with `ColumnRef`s indexing that combined row.
///
/// [`QueryPlan`]: super::QueryPlan
#[derive(Clone)]
pub struct JoinPlan {
    pub source: JoinExpr,
    pub columns: Vec<OutputColumn>,
    pub projections: Vec<BoundExpr>,
    pub predicate: Option<BoundExpr>,
    pub sort: Vec<SortKey>,
    pub distinct: Option<Vec<DistinctKey>>,
}

/// One row source feeding a [`LogicalPlan::Join`]: a base table scan, a
/// subplan (derived table, CTE reference, or `VALUES` in FROM), or a
/// set-returning function.
#[derive(Clone)]
pub enum JoinInput {
    Scan(Arc<dyn TableAm>),
    Subplan(Box<LogicalPlan>),
    TableFunction {
        func: TableFn,
        args: Vec<BoundExpr>,
        /// `WITH ORDINALITY`: append a trailing `bigint` column numbering the
        /// function's rows from 1. It is part of the rowset, so the item's
        /// column list (and thus its width) already counts it.
        ordinality: bool,
    },
}

/// The SQL join semantics applied by one binary [`JoinExpr::Join`] node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Cross,
    Inner,
    Left,
    Right,
    Full,
    /// Emit each left row **once** if at least one right row matches. Unlike
    /// every other kind the output is the left row alone — see
    /// [`JoinExpr::width`] — because the right side of a semi join is a
    /// membership test, not a source of columns. The node's own predicate and
    /// hash keys still address the concatenated `left || right` row.
    ///
    /// Built by the logical optimizer's `DecorrelateSubqueries` rule, out of an
    /// `EXISTS` or `x op ANY (SELECT …)` in a `WHERE`. No SQL syntax binds to it
    /// directly.
    Semi,
    /// The complement of [`JoinKind::Semi`]: emit each left row that matches no
    /// right row, again as the left row alone.
    ///
    /// This is `NOT EXISTS` semantics, **not** `NOT IN`. A left row whose join
    /// key is NULL matches nothing and is therefore emitted; `NOT IN` would have
    /// to answer NULL (and so drop the row) instead. Anything rewriting `NOT IN`
    /// into this kind has to rule out NULLs on both sides first.
    Anti,
}

impl JoinKind {
    /// Whether a node of this kind emits the concatenated `left || right` row.
    pub fn emits_pairs(self) -> bool {
        !matches!(self, JoinKind::Semi | JoinKind::Anti)
    }
}

/// A recursively bound FROM source. Every leaf records its output width so an
/// outer join can synthesize a correctly-sized all-NULL row even when that side
/// has no tuples. Join predicates address the concatenated `left || right` row —
/// including under a semi/anti join, whose *output* is narrower than the row its
/// own predicate reads.
#[derive(Clone)]
pub enum JoinExpr {
    Input {
        input: JoinInput,
        width: usize,
    },
    Join {
        left: Box<JoinExpr>,
        right: Box<JoinExpr>,
        kind: JoinKind,
        /// `None` for `CROSS JOIN` / comma joins, and for a `NATURAL` join
        /// whose sides share no column name: no equality is derived, so the
        /// node behaves like `ON TRUE` while keeping the kind it was written
        /// with — a bare `NATURAL JOIN` stays `Inner`, it does not become
        /// `Cross`.
        predicate: Option<BoundExpr>,
    },
}

impl JoinExpr {
    /// The width of the row this subtree *emits* — under a semi/anti join, less
    /// than the row that node's own predicate reads.
    pub fn width(&self) -> usize {
        match self {
            JoinExpr::Input { width, .. } => *width,
            JoinExpr::Join {
                left, right, kind, ..
            } if kind.emits_pairs() => left.width() + right.width(),
            JoinExpr::Join { left, .. } => left.width(),
        }
    }
}
