//! Splicing a decorrelated subquery into the plan as a new join arm.
//!
//! # Why the arm goes on the right
//!
//! A `ColumnRef` is an offset into the row its node reads, counted left to
//! right, so appending a relation on the **right** leaves every existing index
//! valid: the projections, the `WHERE`, an aggregate's grouping keys and
//! arguments all keep addressing the same values. Nothing is renumbered, and
//! there is no rewrite here that could get the renumbering wrong.
//!
//! The condition of the new node is written in the same index space: a join
//! node's predicate is base-0 relative to its own subtree, and this node is the
//! root of the tree, so its subtree *is* the whole row (see the "Index spaces"
//! section of `crabgresql_planner::pushdown`).

use std::sync::Arc;

use crabgresql_binder::{
    AggInput, AggregatePlan, BoundExpr, JoinExpr, JoinInput, JoinKind, JoinPlan, LogicalPlan,
    QueryPlan, SystemEmit, ValuesPlan,
};
use crabgresql_storage_api::TableAm;

/// One decorrelated subquery, ready to be joined in.
pub(super) struct Arm {
    /// The uncorrelated plan producing the arm's rows.
    pub plan: LogicalPlan,
    /// How many columns it emits — the width the join tree records for the leaf.
    pub width: usize,
    pub kind: JoinKind,
    /// The join condition, addressing the concatenated `left || arm` row.
    pub on: BoundExpr,
}

/// The width of the row `node` reads — where the arm's first column will land.
///
/// `None` for a node this module cannot splice into, which is the same set
/// [`attach_arm`] accepts.
pub(super) fn source_width(node: &LogicalPlan) -> Option<usize> {
    match node {
        LogicalPlan::Query(QueryPlan { table, system, .. }) => {
            Some(scan_width(table, system.as_ref()))
        }
        LogicalPlan::Join(JoinPlan { source, .. }) => Some(source.width()),
        LogicalPlan::Aggregate(AggregatePlan { input, .. }) => match input {
            AggInput::Scan(table) => Some(scan_width(table, None)),
            AggInput::Join(source) => Some(source.width()),
            // A FROM-less aggregate has no row source to join against.
            AggInput::SingleRow => None,
        },
        _ => None,
    }
}

/// The width of a base-relation scan's row: the relation's stored columns plus
/// whatever system slots the scan appends past them.
///
/// The slots have to be counted. A `JoinExpr::Input`'s width is what every index
/// above it is rebased against, so a width short by one silently shifts every
/// column of the right-hand side of the join this attaches.
fn scan_width(table: &Arc<dyn TableAm>, system: Option<&SystemEmit>) -> usize {
    table.schema().columns.len() + system.map_or(0, |emit| emit.cols.len())
}

/// Join `arm` onto the right of `node`'s row source. Returns whether the node
/// was one this could be done to.
///
/// The caller must not have modified `node` yet: a refusal here has to leave the
/// plan exactly as it found it.
pub(super) fn attach_arm(node: &mut LogicalPlan, arm: Arm) -> bool {
    let Arm {
        plan,
        width,
        kind,
        on,
    } = arm;
    let leaf = JoinExpr::Input {
        input: JoinInput::Subplan(Box::new(plan)),
        width,
        // The arm is the subquery's own body with its correlation lifted into
        // the join condition, so it reads no row but the one this node feeds it.
        lateral: false,
    };
    let join = |left: JoinExpr| JoinExpr::Join {
        left: Box::new(left),
        right: Box::new(leaf),
        kind,
        predicate: Some(on),
    };
    match node {
        // A single-relation SELECT has no join tree to extend, so it grows one.
        // Everything else about the node — its projections, filter, ORDER BY,
        // DISTINCT — is carried over untouched, because a `JoinPlan` runs the
        // same pipeline over a row whose prefix is the row a `QueryPlan` read.
        LogicalPlan::Query(_) => {
            let LogicalPlan::Query(QueryPlan {
                table,
                system,
                columns,
                projections,
                predicate,
                sort,
                distinct,
            }) = std::mem::replace(node, placeholder())
            else {
                unreachable!("matched as a Query above");
            };
            let width = scan_width(&table, system.as_ref());
            *node = LogicalPlan::Join(JoinPlan {
                source: join(JoinExpr::Input {
                    input: JoinInput::Scan { table, system },
                    width,
                    lateral: false,
                }),
                columns,
                projections,
                predicate,
                sort,
                distinct,
            });
            true
        }
        LogicalPlan::Join(JoinPlan { source, .. }) => {
            let left = std::mem::replace(source, placeholder_join());
            *source = join(left);
            true
        }
        LogicalPlan::Aggregate(AggregatePlan { input, .. }) => {
            let left = match std::mem::replace(input, AggInput::SingleRow) {
                // `AggInput::Scan` is only built for a scan with no system
                // slots (see `finish_single_select`), so there is none to carry.
                AggInput::Scan(table) => {
                    let width = scan_width(&table, None);
                    JoinExpr::Input {
                        input: JoinInput::Scan {
                            table,
                            system: None,
                        },
                        width,
                        lateral: false,
                    }
                }
                AggInput::Join(source) => source,
                AggInput::SingleRow => {
                    *input = AggInput::SingleRow;
                    return false;
                }
            };
            *input = AggInput::Join(join(left));
            true
        }
        _ => false,
    }
}

/// A plan value to hand `std::mem::replace` while the real one is being
/// rebuilt. Never observed: it is overwritten before the function returns.
fn placeholder() -> LogicalPlan {
    LogicalPlan::Values(ValuesPlan {
        columns: Vec::new(),
        rows: Vec::new(),
        predicate: None,
        sort: Vec::new(),
        distinct: None,
    })
}

/// [`placeholder`] for a join tree.
fn placeholder_join() -> JoinExpr {
    JoinExpr::Input {
        input: JoinInput::Subplan(Box::new(placeholder())),
        width: 0,
        lateral: false,
    }
}
