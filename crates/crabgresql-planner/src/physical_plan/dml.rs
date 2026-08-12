//! The DML nodes: INSERT, UPDATE and DELETE, plus the row sources and per-target
//! access paths they fan out to.

use std::sync::Arc;

use crabgresql_binder::{BoundExpr, MappedRelation, Returning};
use crabgresql_storage_api::{TableAm, Tuple};

use super::PhysicalPlan;

/// [`PhysicalPlan::Insert`]: a write into `table`, or into the leaf partition
/// each row routes to.
pub struct PhysicalInsert {
    pub table: Arc<dyn TableAm>,
    pub source: PhysicalInsertSource,
    pub returning: Option<Returning>,
    /// Leaf partitions for tuple routing when `table` is a partitioned parent
    /// (see [`InsertPlan`](crabgresql_binder::InsertPlan)); `None` for an
    /// ordinary table.
    pub routing: Option<Vec<Arc<dyn TableAm>>>,
    /// `COPY … FREEZE` (see [`InsertPlan`](crabgresql_binder::InsertPlan)): the
    /// executor freezes this target's write and nothing else.
    pub freeze: bool,
    /// Whether each inserted row carries a trailing `tableoid` slot for
    /// RETURNING to read (see [`InsertPlan`](crabgresql_binder::InsertPlan)).
    /// The executor fills it with the leaf the row was routed to.
    pub tableoid: bool,
}

/// [`PhysicalPlan::Update`]: an update of `table` and the relations it fans out
/// to.
pub struct PhysicalUpdate {
    /// Whether each row carries a trailing `tableoid` slot for WHERE, SET or
    /// RETURNING to read (see [`InsertPlan`](crabgresql_binder::InsertPlan)).
    pub tableoid: bool,
    pub table: Arc<dyn TableAm>,
    pub predicate: Option<BoundExpr>,
    pub assignments: Vec<(usize, BoundExpr)>,
    pub returning: Option<Returning>,
    /// Leaf partitions for tuple routing when `table` is a partitioned parent
    /// (see [`UpdatePlan`](crabgresql_binder::UpdatePlan)); `None` for an
    /// ordinary table.
    pub routing: Option<Vec<DmlTarget>>,
    /// `table` and its inheritance descendants, each with its column map
    /// (see [`UpdatePlan`](crabgresql_binder::UpdatePlan)); empty for a table
    /// with no children.
    pub inherited: Vec<DmlTarget>,
    /// The row source for `table` itself, used when it is neither partitioned
    /// nor inherited (the other two arms carry their own).
    pub probe: Option<DmlIndexProbe>,
}

/// [`PhysicalPlan::Delete`]: a delete from `table` and the relations it fans out
/// to.
pub struct PhysicalDelete {
    /// Whether each row carries a trailing `tableoid` slot for WHERE or
    /// RETURNING to read (see [`InsertPlan`](crabgresql_binder::InsertPlan)).
    pub tableoid: bool,
    pub table: Arc<dyn TableAm>,
    pub predicate: Option<BoundExpr>,
    pub returning: Option<Returning>,
    /// Leaf partitions for tuple routing when `table` is a partitioned parent
    /// (see [`DeletePlan`](crabgresql_binder::DeletePlan)); `None` for an
    /// ordinary table.
    pub routing: Option<Vec<DmlTarget>>,
    /// `table` and its inheritance descendants, each with its column map
    /// (see [`DeletePlan`](crabgresql_binder::DeletePlan)); empty for a table
    /// with no children.
    pub inherited: Vec<DmlTarget>,
    /// The row source for `table` itself, used when it is neither partitioned
    /// nor inherited (the other two arms carry their own).
    pub probe: Option<DmlIndexProbe>,
}

/// One relation an `UPDATE`/`DELETE` reads rows from, with the row source chosen
/// for it.
///
/// The probe travels *with* its relation rather than in a vector alongside one,
/// so the two cannot fall out of step — the same reason
/// [`PhysicalAppendArm`](super::PhysicalAppendArm) embeds its [`MappedRelation`]
/// instead of running parallel to it. A positional pairing would survive
/// partition pruning skipping a leaf, and read leaf B through leaf A's index.
pub struct DmlTarget {
    pub relation: MappedRelation,
    /// `None` scans the whole relation.
    pub probe: Option<DmlIndexProbe>,
}

/// An equality index probe standing in for one DML target's sequential scan.
///
/// Unlike [`PhysicalIndexScan`](super::PhysicalIndexScan), a probe here does
/// *not* consume the conjuncts it matched: the plan's `predicate` stays the whole
/// `WHERE` and is re-checked per row. The probe only narrows the row source, so a
/// target that cannot serve one falls back to a scan without any predicate
/// rewriting — which is what lets each inheritance descendant and each leaf
/// partition decide independently, in its own column space.
pub struct DmlIndexProbe {
    pub index_name: String,
    /// One `(key column, equality value)` pair per index key column, in key
    /// order. Columns are ordinals in the *target's* own schema, already
    /// translated through [`MappedRelation::map`] where one applies.
    pub key: Vec<(usize, BoundExpr)>,
    /// The conjuncts the key did *not* cover, for `EXPLAIN` only.
    ///
    /// The executor re-checks the whole `WHERE`, so this never drives execution;
    /// it exists so a plan can show the same `Index Cond` / `Filter` split PG
    /// shows. Re-checking a conjunct the index already satisfied is not
    /// observable, which is what lets display and execution differ here.
    pub residual: Option<BoundExpr>,
}

/// The rows an INSERT writes, mirroring
/// [`InsertSource`](crabgresql_binder::InsertSource) with the query source's
/// subplan already lowered to a [`PhysicalPlan`].
pub enum PhysicalInsertSource {
    /// Fully-formed rows, full-width in schema order, evaluated against the empty
    /// row.
    Values(Vec<Vec<BoundExpr>>),
    /// Rows whose cells are already values, mirroring
    /// [`InsertSource::Tuples`](crabgresql_binder::InsertSource::Tuples).
    Tuples(PhysicalInsertTuples),
    /// Rows pulled from a subplan.
    Query(PhysicalInsertQuery),
}

/// [`PhysicalInsertSource::Tuples`]: rows whose cells are already values.
/// `defaults` names the columns whose `DEFAULT` still needs evaluating once per
/// row.
pub struct PhysicalInsertTuples {
    pub rows: Vec<Tuple>,
    pub defaults: Vec<(usize, BoundExpr)>,
}

/// [`PhysicalInsertSource::Query`]: rows pulled from `input`, each mapped through
/// `projections` (full-width, schema order) evaluated against the source tuple.
pub struct PhysicalInsertQuery {
    pub input: Box<PhysicalPlan>,
    pub projections: Vec<BoundExpr>,
}
