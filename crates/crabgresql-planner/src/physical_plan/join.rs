//! The join nodes: the plan node itself, the lowered join tree and its leaves.

use std::sync::Arc;

use crabgresql_binder::{BoundExpr, DistinctKey, JoinKind, OutputColumn, SortKey, TableFn};
use crabgresql_storage_api::{ColumnProjection, TableAm};
use crabgresql_types::PgType;

use super::PhysicalPlan;

/// [`PhysicalPlan::Join`]: a recursive joined row source, then the standard
/// Filter → Projection → Sort tail. Mirrors
/// [`JoinPlan`](crabgresql_binder::JoinPlan).
pub struct PhysicalJoin {
    pub source: PhysicalJoinExpr,
    pub columns: Vec<OutputColumn>,
    pub projections: Vec<BoundExpr>,
    pub predicate: Option<BoundExpr>,
    pub sort: Vec<SortKey>,
    pub distinct: Option<Vec<DistinctKey>>,
}

/// A join input, mirroring [`JoinInput`](crabgresql_binder::JoinInput) but with
/// the subplan already lowered to a [`PhysicalPlan`].
pub enum PhysicalJoinInput {
    Scan(PhysicalJoinScan),
    Subplan(Box<PhysicalPlan>),
    TableFunction(PhysicalJoinTableFunction),
}

/// [`PhysicalJoinInput::Scan`]: a base-table leaf of the join tree.
pub struct PhysicalJoinScan {
    pub table: Arc<dyn TableAm>,
    /// The columns this leaf's own row supplies to the join tree above it,
    /// in the leaf's base-0 space (see the `projection` pass).
    pub projection: ColumnProjection,
}

/// [`PhysicalJoinInput::TableFunction`]: a set-returning function as a leaf of
/// the join tree.
pub struct PhysicalJoinTableFunction {
    pub func: TableFn,
    pub args: Vec<BoundExpr>,
}

/// One equi-join key of a hash join: a pair of expressions, one addressing the
/// left input and one the right, that must compare equal. Both evaluate to `ty`
/// (the operands' promoted comparison type), so the hash/equality of the two
/// sides is computed under a single type. Extracted from `col = col` conjuncts
/// of the ON predicate (see `extract_hash_keys`).
///
/// The operand expressions still index into the *concatenated* `left || right`
/// row (the form the binder produced): `left` references `[0, left_width)` and
/// `right` references `[left_width, ..)`. The executor evaluates each against a
/// padded row so the indices stay valid.
#[derive(Clone, Debug, PartialEq)]
pub struct HashKey {
    pub left: BoundExpr,
    pub right: BoundExpr,
    pub ty: PgType,
}

/// A [`JoinExpr`](crabgresql_binder::JoinExpr) whose leaf subplans have already
/// been physically planned.
pub enum PhysicalJoinExpr {
    Input(PhysicalJoinLeaf),
    /// A binary join. When `hash_keys` is non-empty the executor runs a hash
    /// join keyed on them and `predicate` holds only the residual (non-equi)
    /// conjuncts of the ON clause; when empty it runs a nested-loop join and
    /// `predicate` is the whole ON condition (`None` for CROSS/comma joins).
    Join(PhysicalJoinPair),
}

/// [`PhysicalJoinExpr::Input`]: one row source of the join tree.
pub struct PhysicalJoinLeaf {
    pub input: PhysicalJoinInput,
    pub width: usize,
    /// Single-relation conjuncts the planner sank to this leaf, applied
    /// above the source before any join sees the row. Indexes the leaf's own
    /// row, so a right child's conjuncts are rebased on the way down.
    pub predicate: Option<BoundExpr>,
}

/// [`PhysicalJoinExpr::Join`]: a binary join of two join expressions.
pub struct PhysicalJoinPair {
    pub left: Box<PhysicalJoinExpr>,
    pub right: Box<PhysicalJoinExpr>,
    pub kind: JoinKind,
    pub predicate: Option<BoundExpr>,
    pub hash_keys: Vec<HashKey>,
}

impl PhysicalJoinExpr {
    pub fn width(&self) -> usize {
        match self {
            PhysicalJoinExpr::Input(leaf) => leaf.width,
            PhysicalJoinExpr::Join(join) => join.left.width() + join.right.width(),
        }
    }

    /// The executor and EXPLAIN must use the same physical-algorithm decision.
    /// Keeping it here prevents the renderer from drifting away if the
    /// selection rule grows beyond the presence of extracted hash keys.
    pub fn uses_hash_join(&self) -> bool {
        matches!(self, PhysicalJoinExpr::Join(join) if !join.hash_keys.is_empty())
    }
}
