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
    TableFunction { func: TableFn, args: Vec<BoundExpr> },
}

/// The SQL join semantics applied by one binary [`JoinExpr::Join`] node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Cross,
    Inner,
    Left,
    Right,
    Full,
}

/// A recursively bound FROM source. Every leaf records its output width so an
/// outer join can synthesize a correctly-sized all-NULL row even when that side
/// has no tuples. Join predicates address the concatenated `left || right` row.
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
    pub fn width(&self) -> usize {
        match self {
            JoinExpr::Input { width, .. } => *width,
            JoinExpr::Join { left, right, .. } => left.width() + right.width(),
        }
    }
}
