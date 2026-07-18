//! Storage engine API: the `TableEngine` / `TableAm` extension point.
//!
//! M1 scope: create/open/scan/insert/update/delete addressed by `Tid`, no
//! snapshots. When crabgresql-txn lands (M2), the methods grow snapshot and
//! transaction parameters and `update`/`delete` report conflict info for
//! EvalPlanQual — see docs/ARCHITECTURE.md §1.3.

use std::sync::Arc;

use crabgresql_types::{PgType, Value};

/// A materialized row. Column order matches the table schema.
pub type Tuple = Vec<Value>;

/// Row identity: stable for the lifetime of a row, never reused within a
/// table. An opaque scalar until pg-engine gives it (page, slot) structure.
pub type Tid = u64;

#[derive(Clone, Debug)]
pub struct Column {
    pub name: String,
    pub ty: PgType,
}

#[derive(Clone, Debug)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<Column>,
}

impl TableSchema {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("relation \"{0}\" already exists")]
    TableAlreadyExists(String),
    #[error("relation \"{0}\" does not exist")]
    TableNotFound(String),
}

/// Outcome of `TableAm::update`. `NotFound` (row vanished under us) is the
/// M2 seam where EvalPlanQual conflict info will live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateResult {
    Updated,
    NotFound,
}

/// Outcome of `TableAm::delete`, mirroring [`UpdateResult`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteResult {
    Deleted,
    NotFound,
}

/// Table access: scans and modifications on one table.
pub trait TableAm: Send + Sync {
    fn schema(&self) -> &TableSchema;

    /// Full scan. The iterator sees a stable snapshot of the table as of the
    /// call (statement-level consistency until real MVCC snapshots exist), so
    /// a DML statement never re-visits rows it modified itself.
    fn scan(&self) -> Box<dyn Iterator<Item = (Tid, Tuple)> + Send>;

    /// The tuple must have exactly `schema().columns.len()` values in schema
    /// order — executors index tuples by schema position and rely on this.
    fn insert(&self, tuple: Tuple) -> Tid;

    /// Replace the row identified by `tid`. The tuple contract matches
    /// [`TableAm::insert`].
    fn update(&self, tid: Tid, tuple: Tuple) -> UpdateResult;

    fn delete(&self, tid: Tid) -> DeleteResult;

    /// Apply a batch of replacements, returning how many rows were found and
    /// updated (vanished tids are skipped, not counted). Engines should
    /// override this to apply the whole batch under one lock — per-row calls
    /// make a large UPDATE quadratic.
    fn update_many(&self, updates: Vec<(Tid, Tuple)>) -> u64 {
        let mut applied = 0;
        for (tid, tuple) in updates {
            if self.update(tid, tuple) == UpdateResult::Updated {
                applied += 1;
            }
        }
        applied
    }

    /// Batch counterpart of [`TableAm::delete`], mirroring
    /// [`TableAm::update_many`].
    fn delete_many(&self, tids: Vec<Tid>) -> u64 {
        let mut applied = 0;
        for tid in tids {
            if self.delete(tid) == DeleteResult::Deleted {
                applied += 1;
            }
        }
        applied
    }

    /// Remove every row (TRUNCATE). Row identity is not preserved: engines need
    /// not keep tids reusable after a truncate. The default scans and deletes;
    /// engines should override with a whole-table reset.
    fn truncate(&self) {
        let tids: Vec<Tid> = self.scan().map(|(tid, _)| tid).collect();
        self.delete_many(tids);
    }
}

/// Engine factory: `CREATE TABLE ... USING <engine>`.
pub trait TableEngine: Send + Sync {
    fn create_table(&self, schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError>;

    fn open_table(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError>;
}
