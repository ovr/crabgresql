//! A read-only [`TableAm`] backed by an in-memory row vector — the access
//! method every system-catalog relation is served through.
//!
//! Catalog rows are synthetic: they have no MVCC version history, so a scan
//! yields every row regardless of the caller's snapshot, and the mutating
//! methods are unreachable. The server never routes a write here — INSERT /
//! UPDATE / DELETE resolve relations through `open_table` (user data only),
//! while only `FROM`-position reads consult the system catalog — so the write
//! methods panic as a backstop rather than silently corrupting anything.

use std::sync::{Arc, OnceLock};

use crabgresql_storage_api::{
    ColumnProjection, DeleteResult, RelStats, StorageError, SystemTupleStream, TableAm,
    TableSchema, Tid, Tuple, TupleStream, UpdateResult, txn::TxnContext,
};
use crabgresql_txn::{CommandId, Infomask, TupleHeader, Xid};
use crabgresql_types::Value;

/// The tid of the `i`th catalog row, and its inverse. Offsets start at 1: see
/// [`StaticTable::scan`].
fn row_tid(i: usize) -> Tid {
    Tid::from_packed(i as u64 + 1)
}

fn row_index(tid: Tid) -> Option<usize> {
    (tid.packed() as usize).checked_sub(1)
}

/// Where a [`StaticTable`]'s rows come from: built already, or built on the
/// first read.
enum Rows {
    Ready(Arc<Vec<Tuple>>),
    /// Built when the relation is first *read* rather than when it is opened.
    ///
    /// A catalog relation is normally materialized as the binder resolves its
    /// name, which is fine for rows that describe the database. It is wrong for
    /// one that describes the *statement*: `pg_locks` reports the relations the
    /// statement resolved, and half of them have not been resolved yet at the
    /// moment its own name is. Deferring to the scan puts the build after
    /// binding, which is also when PostgreSQL's `pg_lock_status()` runs.
    Deferred {
        build: Box<dyn Fn() -> Vec<Tuple> + Send + Sync>,
        built: OnceLock<Arc<Vec<Tuple>>>,
    },
}

/// Where a catalog row's `xmin` comes from.
///
/// Catalog rows have no version history of their own: they are derived afresh
/// from live server state on every statement, so there is no per-row xid to
/// report. What callers actually read `xmin` off a catalog relation for is a
/// *state number* — a value that changes exactly when the thing described does —
/// and a DDL generation is that value.
#[derive(Clone)]
enum CatalogXmin {
    /// One generation for every row: the catalog-wide one. What a relation whose
    /// rows describe types, functions or settings reports, since there is no
    /// finer generation to hand out.
    Snapshot(Xid),
    /// Per row, keyed by the OID of the relation the row describes — see
    /// [`crate::registry::CatalogRelDef::xmin_column`]. This is what keeps
    /// `CREATE TABLE b` from looking like it changed `a`.
    PerRelation {
        /// Ordinal of the column holding that OID.
        column: usize,
        by_oid: Arc<std::collections::HashMap<u32, Xid>>,
        /// For a row whose OID names no live relation — a built-in that
        /// describes nothing, or a row built before the map was.
        default: Xid,
    },
}

/// One `pg_catalog` relation: its schema plus its rows.
pub struct StaticTable {
    schema: Arc<TableSchema>,
    rows: Rows,
    /// Where each row's `xmin` comes from — see [`CatalogXmin`].
    xmin: CatalogXmin,
}

impl StaticTable {
    pub fn new(schema: TableSchema, rows: Vec<Tuple>) -> Self {
        Self {
            schema: Arc::new(schema),
            rows: Rows::Ready(Arc::new(rows)),
            xmin: CatalogXmin::Snapshot(Xid::INVALID),
        }
    }

    /// Build behind an `Arc<dyn TableAm>` for handing to the planner/executor.
    pub fn arc(schema: TableSchema, rows: Vec<Tuple>) -> Arc<dyn TableAm> {
        Arc::new(Self::new(schema, rows))
    }

    /// Report `xmin` from the catalog-wide DDL generation — see [`CatalogXmin`].
    #[must_use]
    pub fn with_xmin(mut self, xmin: Xid) -> Self {
        self.xmin = CatalogXmin::Snapshot(xmin);
        self
    }

    /// Report `xmin` per row, from the generation of the relation each row
    /// describes — see [`CatalogXmin::PerRelation`]. `column` is the ordinal of
    /// the column holding that relation's OID.
    #[must_use]
    pub fn with_relation_xmin(
        mut self,
        column: usize,
        by_oid: Arc<std::collections::HashMap<u32, Xid>>,
        default: Xid,
    ) -> Self {
        self.xmin = CatalogXmin::PerRelation {
            column,
            by_oid,
            default,
        };
        self
    }

    /// See [`Rows::Deferred`]. The caller sets the `xmin` afterwards, exactly as
    /// for a ready relation — a deferred one is no less entitled to a
    /// per-relation generation.
    pub fn deferred(
        schema: TableSchema,
        build: impl Fn() -> Vec<Tuple> + Send + Sync + 'static,
    ) -> Self {
        Self {
            schema: Arc::new(schema),
            rows: Rows::Deferred {
                build: Box::new(build),
                built: OnceLock::new(),
            },
            xmin: CatalogXmin::Snapshot(Xid::INVALID),
        }
    }

    /// The header `row` reports; see [`CatalogXmin`]. A catalog row is never
    /// deleted in place, so `xmax` is always invalid and both command ids are
    /// the first one.
    fn header(&self, row: &[Value]) -> TupleHeader {
        let xmin = match &self.xmin {
            CatalogXmin::Snapshot(xmin) => *xmin,
            CatalogXmin::PerRelation {
                column,
                by_oid,
                default,
            } => match row.get(*column) {
                // A row built to a different width, or an OID naming nothing
                // live: neither is worth failing a scan over, and the
                // catalog-wide generation is the honest fallback.
                Some(Value::Oid(oid)) => by_oid.get(oid).copied().unwrap_or(*default),
                _ => *default,
            },
        };
        TupleHeader {
            xmin,
            xmax: Xid::INVALID,
            cmin: CommandId::FIRST,
            cmax: CommandId::FIRST,
            infomask: Infomask::default(),
        }
    }

    fn rows(&self) -> Arc<Vec<Tuple>> {
        match &self.rows {
            Rows::Ready(rows) => Arc::clone(rows),
            Rows::Deferred { build, built } => Arc::clone(built.get_or_init(|| Arc::new(build()))),
        }
    }

    fn read_only(&self) -> ! {
        panic!(
            "system catalog \"{}\" is read-only; a write must never reach it",
            self.schema.name
        )
    }
}

impl TableAm for StaticTable {
    fn schema(&self) -> Arc<TableSchema> {
        Arc::clone(&self.schema)
    }

    /// Exact, not estimated: the rows are already materialized, so counting them
    /// is free and there is nothing for `ANALYZE` to improve. Reported as
    /// analyzed for that reason.
    ///
    /// A deferred relation reports the never-analyzed sentinel instead of
    /// building its rows to count them: the planner asks for this while the
    /// statement is still being bound, and building there would defeat the
    /// deferral. It is the same answer PostgreSQL gives for a relation nothing
    /// has measured.
    fn statistics(&self) -> RelStats {
        match &self.rows {
            Rows::Ready(rows) => RelStats::exact(rows.len(), &self.schema),
            Rows::Deferred { .. } => RelStats::unknown(&self.schema),
        }
    }

    /// Rows are already materialized in RAM, so a projection prunes no read —
    /// but the rows are shared behind an `Arc` and handed out by value, so every
    /// scan clones them, and *that* is what it prunes. A `pg_proc` row is a
    /// `text` body, a `name`, an `oidvector` and a `regproc` per column; psql and
    /// `pg_dump` scan these relations several times per statement, almost always
    /// for a handful of columns.
    ///
    /// Unprojected slots hold `Value::Null` and the row keeps its full width,
    /// exactly as the heap's projected scan leaves them.
    fn scan(&self, _txn: &TxnContext, projection: &ColumnProjection) -> TupleStream {
        // Synthetic tids from the row index; catalog rows are always visible.
        // Numbered from 1, because offset 0 is `InvalidOffsetNumber` upstream —
        // PostgreSQL's line pointers start at 1, so a `ctid` of `(0,0)` is a
        // value no relation ever hands out and a client is entitled to reject.
        let rows = self.rows();
        let ColumnProjection::Some(wanted) = projection else {
            return Box::new((0..rows.len()).map(move |i| Ok((row_tid(i), rows[i].clone()))));
        };
        let wanted = Arc::clone(wanted);
        Box::new((0..rows.len()).map(move |i| {
            let row = &rows[i];
            let mut out = vec![Value::Null; row.len()];
            // `ColumnProjection::of` keeps every ordinal inside the schema, and a
            // catalog row is built to the schema's width — but this is handed a
            // projection built elsewhere, so a stray ordinal drops rather than
            // panicking a session mid-scan.
            for &column in wanted.iter() {
                if let Some(value) = row.get(column) {
                    out[column] = value.clone();
                }
            }
            Ok((row_tid(i), out))
        }))
    }

    fn fetch(&self, tid: Tid, _txn: &TxnContext) -> Result<Option<Tuple>, StorageError> {
        Ok(row_index(tid).and_then(|i| self.rows().get(i).cloned()))
    }

    /// A catalog relation answers all of them. The tids are synthetic but
    /// stable within a statement, which is all `ctid` promises here, and the
    /// header is the snapshot-wide one described on [`StaticTable::with_xmin`].
    fn supports_system_columns(&self) -> bool {
        true
    }

    fn scan_with_system(
        &self,
        txn: &TxnContext,
        projection: &ColumnProjection,
    ) -> Option<SystemTupleStream> {
        // Headers come off the **unprojected** rows: a per-relation `xmin` reads
        // the OID column, which the projection is free to have left as a
        // placeholder. They are computed up front rather than per yielded row so
        // the closure below borrows nothing.
        let headers: Vec<TupleHeader> = self.rows().iter().map(|row| self.header(row)).collect();
        // Both this and `scan` number rows by the same index, and `rows()`
        // memoizes a deferred build, so the two see the same vector and the
        // lookup below is total.
        let rows = self.scan(txn, projection);
        Some(Box::new(rows.map(move |row| {
            row.map(|(tid, tuple)| {
                let i = row_index(tid).expect("a tid this scan minted");
                (tid, headers[i], tuple)
            })
        })))
    }

    fn insert(&self, _tuple: Tuple, _txn: &TxnContext) -> Result<Tid, StorageError> {
        self.read_only()
    }

    fn update(
        &self,
        _tid: Tid,
        _tuple: Tuple,
        _txn: &TxnContext,
    ) -> Result<UpdateResult, StorageError> {
        self.read_only()
    }

    fn delete(&self, _tid: Tid, _txn: &TxnContext) -> Result<DeleteResult, StorageError> {
        self.read_only()
    }

    fn vacuum(&self, _oldest: Xid, _clog: &crabgresql_txn::Clog) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_storage_api::Column;
    use crabgresql_types::{PgType, Value};

    fn table(rows: usize) -> StaticTable {
        StaticTable::new(
            TableSchema::new("t", vec![Column::new("id", PgType::Int4)]),
            (0..rows).map(|i| vec![Value::Int4(i as i32)]).collect(),
        )
    }

    #[test]
    fn statistics_count_the_rows_exactly() {
        let stats = table(7).statistics();
        assert_eq!(stats.reltuples, 7.0);
        assert!(
            stats.analyzed,
            "a materialized row count is exact, not an estimate"
        );
        assert!(stats.relpages > 0);
    }

    /// A projected scan keeps the row's width and its tids, carries the columns
    /// it was asked for, and leaves the rest as the placeholder the contract
    /// allows.
    #[test]
    fn a_projected_scan_keeps_the_row_width() {
        let table = StaticTable::new(
            TableSchema::new(
                "t",
                vec![
                    Column::new("a", PgType::Int4),
                    Column::new("b", PgType::Text),
                    Column::new("c", PgType::Int4),
                ],
            ),
            vec![
                vec![Value::Int4(1), Value::Text("x".into()), Value::Int4(3)],
                vec![Value::Int4(4), Value::Text("y".into()), Value::Int4(6)],
            ],
        );
        let schema = table.schema();
        // A catalog row is visible to every snapshot, so any context serves.
        let tm = crabgresql_txn::TransactionManager::new();
        let txn = tm.context(Xid::INVALID, crabgresql_txn::CommandId::FIRST);
        let rows: Vec<_> = table
            .scan(&txn, &ColumnProjection::of([1], &schema))
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|error| panic!("catalog scan failed: {error}"));
        assert_eq!(
            rows.iter().map(|(_, row)| row.clone()).collect::<Vec<_>>(),
            vec![
                vec![Value::Null, Value::Text("x".into()), Value::Null],
                vec![Value::Null, Value::Text("y".into()), Value::Null],
            ]
        );
        // The tids must be the same synthetic row indices the full scan hands
        // out — `fetch` resolves them back through the same mapping.
        let full: Vec<_> = table
            .scan(&txn, &ColumnProjection::All)
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|error| panic!("catalog scan failed: {error}"));
        assert_eq!(
            rows.iter().map(|(tid, _)| *tid).collect::<Vec<_>>(),
            full.iter().map(|(tid, _)| *tid).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_empty_catalog_relation_reports_no_rows() {
        let stats = table(0).statistics();
        assert_eq!(stats.reltuples, 0.0);
        assert_eq!(stats.relpages, 0);
    }
}
