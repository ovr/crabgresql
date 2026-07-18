//! A read-only [`TableAm`] backed by an in-memory row vector — the access
//! method every `pg_catalog` relation is served through.
//!
//! Catalog rows are synthetic: they have no MVCC version history, so a scan
//! yields every row regardless of the caller's snapshot, and the mutating
//! methods are unreachable. The server never routes a write here — INSERT /
//! UPDATE / DELETE resolve relations through `open_table` (user data only),
//! while only `FROM`-position reads consult the system catalog — so the write
//! methods panic as a backstop rather than silently corrupting anything.

use std::sync::Arc;

use crabgresql_storage_api::{
    DeleteResult, TableAm, TableSchema, Tid, Tuple, UpdateResult, txn::TxnContext,
};
use crabgresql_txn::Xid;

/// One materialized `pg_catalog` relation: its schema plus a fixed set of rows.
pub struct StaticTable {
    schema: TableSchema,
    rows: Arc<Vec<Tuple>>,
}

impl StaticTable {
    pub fn new(schema: TableSchema, rows: Vec<Tuple>) -> Self {
        Self { schema, rows: Arc::new(rows) }
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

    fn scan(&self, _txn: &TxnContext) -> Box<dyn Iterator<Item = (Tid, Tuple)> + Send> {
        // Synthetic tids from the row index; catalog rows are always visible.
        let rows = self.rows.clone();
        Box::new((0..rows.len()).map(move |i| (Tid::from_packed(i as u64), rows[i].clone())))
    }

    fn fetch(&self, tid: Tid, _txn: &TxnContext) -> Option<Tuple> {
        self.rows.get(tid.packed() as usize).cloned()
    }

    fn insert(&self, _tuple: Tuple, _txn: &TxnContext) -> Tid {
        self.read_only()
    }

    fn update(&self, _tid: Tid, _tuple: Tuple, _txn: &TxnContext) -> UpdateResult {
        self.read_only()
    }

    fn delete(&self, _tid: Tid, _txn: &TxnContext) -> DeleteResult {
        self.read_only()
    }

    fn vacuum(&self, _oldest: Xid, _clog: &crabgresql_txn::Clog) {}
}
