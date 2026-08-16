//! Relations a statement reaches through the name of another: the tuple
//! remapping an inheritance or partition fan-out needs, and the identity a scan
//! answers `tableoid` with.

use std::borrow::Cow;
use std::sync::Arc;

use crabgresql_storage_api::{TableAm, TableSchema};
use crabgresql_types::{PgType, Value};

/// One relation a statement reaches through the name of another, with the
/// permutation that puts its tuples into the *named* relation's layout — an arm
/// of a [`LogicalPlan::Append`], or one relation an inheriting `UPDATE`/`DELETE`
/// fans out to.
///
/// `map` is indexed by the named relation's ordinal and holds this relation's
/// own ordinal for that column. `None` is the identity map, meaning this
/// relation already carries exactly the named layout — true of every leaf
/// partition (a partition is created as a verbatim clone of its parent) and of
/// every storage leaf of a single relation, so those paths pay nothing.
///
/// Only an inheritance descendant needs a map. Its columns are a *superset* of
/// its parent's, in an order the child chose, and the order need not even keep
/// the parent's columns contiguous: with `emp(name, age, location, salary,
/// manager)` and `student(name, age, location, gpa)` both inheriting `person`,
/// `stud_emp INHERITS (emp, student)` lays out `name, age, location, salary,
/// manager, gpa, percent` — so read as a `student`, its `gpa` is at ordinal 5,
/// not 3.
///
/// Nothing above ever re-indexes an expression: a bound predicate, SET target or
/// RETURNING projection stays in the named relation's index space, and it is the
/// *tuple* that is viewed through `map` on the way in and scattered back through
/// it on the way out.
///
/// [`LogicalPlan::Append`]: super::LogicalPlan::Append
#[derive(Clone)]
pub struct MappedRelation {
    pub table: Arc<dyn TableAm>,
    pub map: Option<Arc<[usize]>>,
    /// Set when this relation's rows must carry system-column slots, appended
    /// after `view` so `map` stays a pure gather and the write-back paths
    /// (`scatter`, `rebuild`) never see a column with nowhere to write.
    ///
    /// Each arm names *itself*, which is what makes `tableoid` report the
    /// partition or inheritance child a row actually came from rather than the
    /// relation the query named.
    pub system: Option<SystemEmit>,
}

/// The system columns one scan appends to every row it emits.
///
/// `cols` is in row order — the order the slots occupy past the relation's
/// declared columns — and matches the order the binder appended the matching
/// [`OutputColumn`](crate::OutputColumn)s in, which is what lets a reference
/// resolve as an ordinary column of the row.
///
/// `ident` rides along even when `tableoid` is not among `cols`: it is two
/// strings cloned once per arm at bind time, and carrying it unconditionally
/// keeps the arm constructors from needing two shapes.
#[derive(Clone, Debug)]
pub struct SystemEmit {
    pub cols: Arc<[SysCol]>,
    pub ident: RelationIdent,
}

/// One of PostgreSQL's per-row system columns.
///
/// `oid` is absent because PostgreSQL 12 removed it. The order of the variants
/// is the order slots are appended to a row in, so it is load-bearing: the
/// binder pushes [`OutputColumn`](crate::OutputColumn)s and the executor pushes
/// [`Value`]s by walking the same sorted list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SysCol {
    /// The OID of the relation the row lives in. Answerable by every access
    /// method: it is a fact about the relation, not about the row's storage.
    TableOid,
    /// The row version's physical address, PG's `ctid`.
    Ctid,
    /// The inserting transaction.
    Xmin,
    /// The command within [`SysCol::Xmin`] that inserted the row.
    Cmin,
    /// The deleting transaction, or `0` while the version is live.
    Xmax,
    /// The command within [`SysCol::Xmax`] that deleted the row.
    Cmax,
}

impl SysCol {
    /// Every system column, in row order.
    pub const ALL: [SysCol; 6] = [
        SysCol::TableOid,
        SysCol::Ctid,
        SysCol::Xmin,
        SysCol::Cmin,
        SysCol::Xmax,
        SysCol::Cmax,
    ];

    /// The name a query addresses this column by.
    pub const fn name(self) -> &'static str {
        match self {
            SysCol::TableOid => "tableoid",
            SysCol::Ctid => "ctid",
            SysCol::Xmin => "xmin",
            SysCol::Cmin => "cmin",
            SysCol::Xmax => "xmax",
            SysCol::Cmax => "cmax",
        }
    }

    /// The type the slot carries.
    pub const fn ty(self) -> PgType {
        match self {
            SysCol::TableOid => PgType::Oid,
            SysCol::Ctid => PgType::Tid,
            SysCol::Xmin | SysCol::Xmax => PgType::Xid,
            SysCol::Cmin | SysCol::Cmax => PgType::Cid,
        }
    }

    /// Whether answering this needs the row version's MVCC header rather than
    /// just its tid — the difference between
    /// [`TableAm::scan`](crabgresql_storage_api::TableAm::scan) and
    /// [`TableAm::scan_with_system`](crabgresql_storage_api::TableAm::scan_with_system).
    pub const fn needs_header(self) -> bool {
        matches!(
            self,
            SysCol::Xmin | SysCol::Cmin | SysCol::Xmax | SysCol::Cmax
        )
    }

    /// Whether the access method has to be able to produce this per row. Only
    /// `tableoid` does not: every relation has an identity, whatever it stores.
    pub const fn needs_storage_support(self) -> bool {
        !matches!(self, SysCol::TableOid)
    }
}

/// The relation a scan answers `tableoid` with.
///
/// A name rather than an OID: relation OIDs are positional over the catalog
/// snapshot (`SystemCatalog::relation_oids` sorts by `(namespace, name)`), so a
/// prepared statement holding a folded one would go stale the moment another
/// relation is created ahead of it. The scan resolves it once when it opens —
/// per statement, not per row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelationIdent {
    pub namespace: String,
    pub name: String,
}

impl RelationIdent {
    /// The identity of the relation `schema` describes.
    pub fn of(schema: &TableSchema) -> Self {
        Self {
            namespace: schema.namespace.clone(),
            name: schema.name.clone(),
        }
    }
}

impl MappedRelation {
    /// This relation's tuple read as a tuple of the named relation.
    ///
    /// Borrowed for an identity relation, which is what the parent of every
    /// inheritance fan-out is: it is called once per *scanned* row, before the
    /// predicate runs, so cloning there would make `DELETE FROM parent WHERE id
    /// = 1` deep-copy every row of the parent to hand the predicate a tuple it
    /// was already holding.
    pub fn view<'a>(&self, tuple: &'a [Value]) -> Cow<'a, [Value]> {
        match &self.map {
            None => Cow::Borrowed(tuple),
            Some(map) => Cow::Owned(map.iter().map(|&i| tuple[i].clone()).collect()),
        }
    }

    /// Write a named-relation tuple back over the columns of `tuple` it was read
    /// from, leaving this relation's own extra columns untouched.
    pub fn scatter(&self, tuple: &mut [Value], view: &[Value]) {
        match &self.map {
            None => tuple.clone_from_slice(view),
            Some(map) => {
                for (value, &i) in view.iter().zip(map.iter()) {
                    tuple[i] = value.clone();
                }
            }
        }
    }

    /// The full tuple to store, given the row it was read from and the
    /// named-relation tuple an UPDATE produced from it.
    ///
    /// For an identity relation the new view *is* the new tuple, so it moves
    /// through untouched — no clone of `old`, no copy back over it. That is the
    /// common case twice over: the parent of every inheritance fan-out is an
    /// identity relation, and so is every child declared `() INHERITS (p)`.
    pub fn rebuild(&self, old: &[Value], view: Vec<Value>) -> Vec<Value> {
        match &self.map {
            None => view,
            Some(_) => {
                let mut tuple = old.to_vec();
                self.scatter(&mut tuple, &view);
                tuple
            }
        }
    }
}
