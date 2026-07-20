//! `crabgresql-pg-engine`: the durable, PostgreSQL-faithful heap engine.
//!
//! Implements the [`TableEngine`]/[`TableAm`] contract over 8 KB slotted pages
//! (`page`), a clock-sweep buffer pool (`bufpool`), on-page tuple headers with
//! genuine `ctid = (block, offset)` (`tuple`), and physiological WAL logging via
//! the core [`crabgresql_wal`] service — with redo-only crash recovery. MVCC is
//! the shared [`satisfies_mvcc`](crabgresql_txn::satisfies_mvcc) rule applied to
//! the on-page header, exactly as in the memory engine; only the storage of the
//! versions differs.
//!
//! Deliberately deferred to keep this first cut tractable (all documented in
//! `docs/ARCHITECTURE.md §3`): TOAST (a tuple must fit one page), a durable SLRU
//! CLOG and checkpoint-bounded recovery (recovery replays the whole WAL),
//! full-page writes / torn-page protection beyond page checksums, WAL segment
//! recycling, and a transactional relation catalog.

mod bufpool;
mod catalog;
mod datum;
mod heap;
mod page;
mod rec;
mod redo;
mod smgr;
mod tuple;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crabgresql_storage_api::{
    IndexMetadata, RelationMetadata, StorageError, TableAm, TableEngine, TableSchema,
};
use crabgresql_wal::{ControlFile, RmgrId, RmgrRegistry, Wal, write_control};

use crate::bufpool::BufferPool;
use crate::catalog::RelCatalog;
use crate::heap::HeapTable;
use crate::redo::HeapRedo;
use crate::smgr::StorageManager;

pub use crate::smgr::RelFileNode;

/// Number of buffer-pool frames (8 MB). Must comfortably exceed the number of
/// pages pinned concurrently; see `bufpool` docs.
const DEFAULT_FRAMES: usize = 1024;

/// Shared engine state that both the table AM and the redo handler reach into.
pub(crate) struct EngineInner {
    pub bufpool: BufferPool,
    pub wal: Arc<Wal>,
}

/// The durable heap engine: a [`TableEngine`] over a data directory.
pub struct PgEngine {
    inner: Arc<EngineInner>,
    data_dir: PathBuf,
    catalog: RelCatalog,
    tables: RwLock<HashMap<String, Arc<HeapTable>>>,
}

impl PgEngine {
    /// Open the engine over `data_dir`, registering its redo handler into
    /// `registry` (which recovery will consult) and loading the relation
    /// catalog. Call [`crabgresql_wal::recover`] afterwards to replay the WAL,
    /// then [`PgEngine::checkpoint`] to make recovered pages durable.
    pub fn new(
        data_dir: &Path,
        wal: Arc<Wal>,
        registry: &mut RmgrRegistry,
    ) -> std::io::Result<PgEngine> {
        let smgr = Arc::new(StorageManager::open(data_dir)?);
        let bufpool = BufferPool::new(DEFAULT_FRAMES, smgr, Arc::clone(&wal));
        let inner = Arc::new(EngineInner { bufpool, wal });
        registry.register(
            RmgrId::HEAP,
            Arc::new(HeapRedo {
                engine: Arc::clone(&inner),
            }),
        );

        let catalog = RelCatalog::load(data_dir)?;
        let mut tables = HashMap::new();
        for (name, rel, schema, indexes) in catalog.schemas() {
            tables.insert(
                name,
                Arc::new(HeapTable::new(Arc::clone(&inner), rel, schema, indexes)),
            );
        }
        Ok(PgEngine {
            inner,
            data_dir: data_dir.to_path_buf(),
            catalog,
            tables: RwLock::new(tables),
        })
    }

    /// Flush all dirty pages to their relation files (obeying the write-ahead
    /// rule) and record a clean control file. Called after recovery and at a
    /// clean shutdown so the data files are current.
    pub fn checkpoint(&self, next_xid: crabgresql_txn::Xid) -> std::io::Result<()> {
        self.inner.bufpool.flush_all()?;
        write_control(
            &self.data_dir,
            &ControlFile {
                next_xid,
                clean_shutdown: true,
            },
        )
        .map_err(std::io::Error::other)?;
        Ok(())
    }
}

impl TableEngine for PgEngine {
    fn create_table(&self, schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError> {
        let mut tables = self
            .tables
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        if tables.contains_key(&schema.name)
            || self.catalog.contains(&schema.name)
            || tables
                .values()
                .any(|t| t.indexes().iter().any(|i| i.name == schema.name))
        {
            return Err(StorageError::TableAlreadyExists(schema.name));
        }
        let rel = self
            .catalog
            .create(&schema)
            .expect("relation catalog write failed");
        let table = Arc::new(HeapTable::new(
            Arc::clone(&self.inner),
            rel,
            schema.clone(),
            Vec::new(),
        ));
        tables.insert(schema.name, Arc::clone(&table));
        Ok(table as Arc<dyn TableAm>)
    }

    fn open_table(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        self.tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .get(name)
            .cloned()
            .map(|t| t as Arc<dyn TableAm>)
            .ok_or_else(|| StorageError::TableNotFound(name.to_string()))
    }

    fn drop_table(&self, name: &str) -> Result<(), StorageError> {
        // Remove the durable catalog entry and the in-memory handle together
        // under the tables lock, so `open_table` never observes a half-dropped
        // relation. The persistent catalog is the source of truth for existence:
        // a missing entry there is the 42P01 case.
        let rel = {
            let mut tables = self
                .tables
                .write()
                .unwrap_or_else(|_| panic!("rwlock poisoned"));
            let rel = self
                .catalog
                .remove(name)
                .expect("relation catalog write failed");
            let Some(rel) = rel else {
                return Err(StorageError::TableNotFound(name.to_string()));
            };
            tables.remove(name);
            rel
        };
        // Physical cleanup runs after the tables lock is released, so an IO error
        // unlinking the file panics only this statement rather than poisoning the
        // lock and disabling every other table operation. Evict the relation's
        // buffered pages first so a later checkpoint can't write them back to the
        // file we are about to unlink.
        self.inner.bufpool.forget_relation(rel);
        self.inner
            .bufpool
            .smgr()
            .unlink(rel)
            .expect("relation file unlink failed");
        Ok(())
    }

    fn create_index(&self, table: &str, index: IndexMetadata) -> Result<(), StorageError> {
        let tables = self
            .tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        if tables.contains_key(&index.name)
            || tables
                .values()
                .any(|t| t.indexes().iter().any(|i| i.name == index.name))
        {
            return Err(StorageError::RelationAlreadyExists(index.name));
        }
        let target = tables
            .get(table)
            .ok_or_else(|| StorageError::IndexTableNotFound(table.to_string()))?;
        self.catalog
            .add_index(table, index.clone())
            .expect("relation catalog write failed");
        target.add_index(index);
        Ok(())
    }

    fn relations(&self) -> Vec<TableSchema> {
        self.tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .values()
            .map(|t| t.schema().clone())
            .collect()
    }

    fn relation_metadata(&self) -> Vec<RelationMetadata> {
        self.tables
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .values()
            .map(|t| RelationMetadata {
                schema: t.schema().clone(),
                indexes: t.indexes(),
            })
            .collect()
    }
}
