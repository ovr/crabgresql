//! INSERT / UPDATE / DELETE nodes, the rows an INSERT writes, and the
//! `RETURNING` target list all three share.

use std::sync::Arc;

use crabgresql_storage_api::{TableAm, Tuple};

use crate::OutputColumn;
use crate::expr::BoundExpr;

use super::{LogicalPlan, MappedRelation};

/// [`LogicalPlan::Insert`]: rows come from a `VALUES` list (full-width, schema
/// order, each cell already coerced), from already-formed values (a COPY load),
/// or from a query source (`INSERT ... SELECT` / `INSERT ... TABLE t`). See
/// [`InsertSource`].
///
/// [`LogicalPlan::Insert`]: super::LogicalPlan::Insert
#[derive(Clone)]
pub struct InsertPlan {
    pub table: Arc<dyn TableAm>,
    pub source: InsertSource,
    /// `RETURNING`: a projection over each inserted row, bound against the
    /// table schema. `None` when the clause is absent.
    pub returning: Option<Returning>,
    /// Tuple routing for a partitioned parent: `Some(leaves)` when `table` is
    /// a partitioned parent, holding its leaf partitions. The executor routes
    /// each row to the leaf whose RANGE bound admits its key (reading the
    /// bound from each leaf's `partition_of`) and writes there instead of to
    /// `table`. `None` for an ordinary table (rows go straight to `table`).
    pub routing: Option<Vec<Arc<dyn TableAm>>>,
    /// `COPY … FREEZE`: stamp the rows visible-to-everyone rather than
    /// visible-once-this-transaction-commits. Carried on the node, not on the
    /// transaction, so it reaches exactly this target's write and nothing else
    /// the statement happens to do — see
    /// [`crabgresql_txn::TxnContext::freeze_inserts`].
    pub freeze: bool,
    /// Whether every row this statement forms carries a trailing `tableoid`
    /// slot, because a WHERE, SET or RETURNING named it. The executor
    /// appends the OID of the target the row actually lives in — the
    /// partition or inheritance child, not the relation the statement
    /// named.
    pub tableoid: bool,
}

/// [`LogicalPlan::Update`].
///
/// [`LogicalPlan::Update`]: super::LogicalPlan::Update
#[derive(Clone)]
pub struct UpdatePlan {
    pub table: Arc<dyn TableAm>,
    pub predicate: Option<BoundExpr>,
    /// (column index, value expression bound against the OLD row).
    pub assignments: Vec<(usize, BoundExpr)>,
    /// `RETURNING`: a projection over each updated (NEW) row.
    pub returning: Option<Returning>,
    /// Tuple routing for a partitioned parent: `Some(leaves)` when `table` is
    /// a partitioned parent, holding its leaf partitions. The executor scans
    /// every leaf, and for each updated row re-routes the NEW tuple to the
    /// leaf whose RANGE bound admits it — moving the row (delete from the old
    /// leaf, insert into the new) when the key change lands it elsewhere.
    /// `None` for an ordinary table.
    pub routing: Option<Vec<Arc<dyn TableAm>>>,
    /// Inheritance fan-out: `table`'s descendants, each with the map that
    /// reads one of its rows as a `table` row. Empty unless `table` has
    /// inheritance children and the statement did not say `ONLY`.
    ///
    /// Deliberately *not* folded into `routing`: routing exists to move a row
    /// between partitions when its key changes, and inheritance has no such
    /// notion — every row is updated where it lies, in its own relation, and
    /// nothing ever moves.
    pub inherited: Vec<MappedRelation>,
    /// Whether every row this statement forms carries a trailing `tableoid`
    /// slot; see the same field on [`InsertPlan`].
    pub tableoid: bool,
}

/// [`LogicalPlan::Delete`].
///
/// [`LogicalPlan::Delete`]: super::LogicalPlan::Delete
#[derive(Clone)]
pub struct DeletePlan {
    pub table: Arc<dyn TableAm>,
    pub predicate: Option<BoundExpr>,
    /// `RETURNING`: a projection over each deleted (OLD) row.
    pub returning: Option<Returning>,
    /// Tuple routing for a partitioned parent: `Some(leaves)` when `table` is
    /// a partitioned parent. The executor scans every leaf and deletes matching
    /// rows from whichever leaf holds them. `None` for an ordinary table.
    pub routing: Option<Vec<Arc<dyn TableAm>>>,
    /// Inheritance fan-out, as on [`UpdatePlan`]: rows are deleted from
    /// whichever descendant holds them.
    pub inherited: Vec<MappedRelation>,
    /// Whether every row this statement forms carries a trailing `tableoid`
    /// slot; see the same field on [`InsertPlan`].
    pub tableoid: bool,
}

/// A bound `RETURNING` target list: the output column shape plus one expression
/// per output column, evaluated against each affected row (the NEW row for
/// INSERT/UPDATE, the deleted row for DELETE). Same shape as a SELECT
/// projection.
#[derive(Clone)]
pub struct Returning {
    pub columns: Vec<OutputColumn>,
    pub projections: Vec<BoundExpr>,
}

/// The rows an INSERT writes.
#[derive(Clone)]
pub enum InsertSource {
    /// `INSERT ... VALUES`: fully-formed rows, full-width in schema order, each
    /// cell already coerced to its column type. Evaluated against the empty row.
    Values(Vec<Vec<BoundExpr>>),
    /// Rows whose cells are already values, with no expression tree to evaluate.
    ///
    /// This is COPY's source (see [`CopyFromPlan::build_insert`]). A load has
    /// already parsed every field through its column's input function, so
    /// wrapping the results in [`BoundExpr::Const`] only to have the executor
    /// clone them back out doubles the work of the whole statement.
    ///
    /// [`CopyFromPlan::build_insert`]: crate::CopyFromPlan::build_insert
    Tuples {
        /// Full-width in schema order.
        ///
        /// A slot named in `defaults` holds `Value::Null` as a placeholder,
        /// which is indistinguishable from a genuine NULL — the `defaults` list
        /// is the only thing that says which is which. So a reader must run that
        /// list before treating a row as complete; `collect_insert_tuples` is
        /// the one place that does, and the only consumer.
        rows: Vec<Tuple>,
        /// Columns whose `DEFAULT` does not fold to a constant — `nextval` on a
        /// `serial`, `now()`, a routine call — paired with the expression that
        /// produces it. Evaluated once per row, in ascending column order, so a
        /// sequence advances exactly as it does on the `Values` path. Empty for
        /// the common load, which is why the fast path stays fast.
        defaults: Vec<(usize, BoundExpr)>,
        /// Columns, ascending, that hold a non-NULL value in **every** row of
        /// `rows` — the builder saw each value as it produced it, so the
        /// executor need not walk them again to enforce `NOT NULL`.
        ///
        /// Purely an optimization, and a subtractive one: the executor still
        /// derives the not-null columns from the live schema and only removes
        /// these, so a stale entry can never turn a violation into an accepted
        /// row. Empty means "nothing proven", which is what every builder other
        /// than COPY passes.
        notnull_verified: Vec<u32>,
    },
    /// `INSERT ... SELECT` / `INSERT ... TABLE t`: rows are pulled from `input`
    /// at execution time. `projections` is full-width in schema order — non-target
    /// columns hold their column default, each target column a `ColumnRef` into
    /// the source row coerced to the column type — evaluated against each source
    /// tuple.
    Query {
        input: Box<LogicalPlan>,
        projections: Vec<BoundExpr>,
    },
}
