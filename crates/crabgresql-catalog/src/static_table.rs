//! A read-only [`TableAm`] backed by an in-memory row vector — the access
//! method every system-catalog relation is served through.
//!
//! Catalog rows are synthetic: they have no MVCC version history, so a scan
//! yields every row regardless of the caller's snapshot, and the mutating
//! methods are unreachable. The server never routes a write here — INSERT /
//! UPDATE / DELETE resolve relations through `open_table` (user data only),
//! while only `FROM`-position reads consult the system catalog — so the write
//! methods panic as a backstop rather than silently corrupting anything.

use std::sync::Arc;

use crabgresql_storage_api::{
    ColumnProjection, DeleteResult, RelStats, StorageError, TableAm, TableSchema, Tid, Tuple,
    TupleStream, UpdateResult, txn::TxnContext,
};
use crabgresql_txn::Xid;

/// One materialized `pg_catalog` relation: its schema plus a fixed set of rows.
pub struct StaticTable {
    schema: TableSchema,
    rows: Arc<Vec<Tuple>>,
}

impl StaticTable {
    pub fn new(schema: TableSchema, rows: Vec<Tuple>) -> Self {
        Self {
            schema,
            rows: Arc::new(rows),
        }
    }

    /// Build behind an `Arc<dyn TableAm>` for handing to the planner/executor.
    pub fn arc(schema: TableSchema, rows: Vec<Tuple>) -> Arc<dyn TableAm> {
        Arc::new(Self::new(schema, rows))
    }

    fn read_only(&self) -> ! {
        panic!(
            "system catalog \"{}\" is read-only; a write must never reach it",
            self.schema.name
        )
    }
}

impl TableAm for StaticTable {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    /// Exact, not estimated: the rows are already materialized, so counting them
    /// is free and there is nothing for `ANALYZE` to improve. Reported as
    /// analyzed for that reason.
    fn statistics(&self) -> RelStats {
        RelStats::exact(self.rows.len(), &self.schema)
    }

    /// Rows are already materialized in RAM, so there is no read to prune: the
    /// projection is ignored, which the scan contract permits.
    fn scan(&self, _txn: &TxnContext, _projection: &ColumnProjection) -> TupleStream {
        // Synthetic tids from the row index; catalog rows are always visible.
        let rows = self.rows.clone();
        Box::new(
            (0..rows.len())
                .map(move |i| Ok((Tid::from_packed(i as u64), rows[i].clone()))),
        )
    }

    fn fetch(&self, tid: Tid, _txn: &TxnContext) -> Result<Option<Tuple>, StorageError> {
        Ok(self.rows.get(tid.packed() as usize).cloned())
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

    fn delete(
        &self,
        _tid: Tid,
        _txn: &TxnContext,
    ) -> Result<DeleteResult, StorageError> {
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

    #[test]
    fn an_empty_catalog_relation_reports_no_rows() {
        let stats = table(0).statistics();
        assert_eq!(stats.reltuples, 0.0);
        assert_eq!(stats.relpages, 0);
    }
}
