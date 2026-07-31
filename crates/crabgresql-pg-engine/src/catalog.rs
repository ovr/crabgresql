//! A minimal on-disk relation catalog: relation name -> (`RelFileNode`, schema).
//!
//! It exists so the engine can rediscover its tables and their column types
//! after a restart. It is intentionally simple — the whole catalog is rewritten
//! and fsynced on each DDL — and is not itself MVCC/crash-transactional yet; a
//! real `pg_class`/`pg_attribute`-backed catalog is future work.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crabgresql_storage_api::{
    Column, IndexConstraint, IndexKey, IndexMetadata, IndexMethod, PartitionBound,
    PartitionBoundDatum, PartitionOf, PartitionScheme, PartitionStrategy, RelPersistence,
    SequenceAdvance, SequenceDefinition, TableSchema, ViewDefinition,
    TableAccessMethod,
};
use crabgresql_types::PgType;

use crate::smgr::RelFileNode;

/// One relation as reflected at startup: its name, heap relfilenode, schema, and
/// each index paired with the relfilenode of its physical B-tree.
/// A relation as the engine reopens it: its name, heap relfilenode, out-of-line
/// chunk relfilenode (`RelFileNode(0)` for none), schema, and physical indexes.
type ReflectedRelation = (
    String,
    RelFileNode,
    RelFileNode,
    TableSchema,
    Vec<(IndexMetadata, RelFileNode)>,
);

const CATALOG_SUBDIR: &str = "global";
const CATALOG_FILE: &str = "relcatalog";
const FIRST_RELFILENODE: u32 = 1;
/// PostgreSQL's `FIRST_NORMAL_OBJECT_ID`: the first OID handed out for
/// user-created schemas (`CREATE SCHEMA`).
const FIRST_NORMAL_OBJECT_ID: u32 = 16384;
const META_MAGIC: &[u8; 4] = b"CRM1";
/// Marks the view section, appended after the [`META_MAGIC`] block. Like that
/// block it is a backward-compatible tail: a reader that predates views stops
/// after the metadata and ignores it.
const VIEW_MAGIC: &[u8; 4] = b"CVW1";
/// Marks the sequence section, appended after the [`VIEW_MAGIC`] block — a third
/// backward-compatible tail. A pre-sequence reader stops above and never sees it.
const SEQ_MAGIC: &[u8; 4] = b"CSQ1";
/// Marks the namespace section, appended after the [`SEQ_MAGIC`] block — a
/// fourth backward-compatible tail carrying the user schema registry and each
/// object's namespace. A reader that predates schemas stops above and never sees
/// it, so every object decodes as `public` (the sole namespace before schemas).
const NSP_MAGIC: &[u8; 4] = b"NSP1";
/// Marks the partition section, appended after the [`NSP_MAGIC`] block — a fifth
/// backward-compatible tail carrying each relation's declarative-partitioning
/// metadata (partition key for a parent, parent link + bound for a leaf). A
/// reader that predates partitioning stops above and treats every relation as
/// unpartitioned.
const PART_MAGIC: &[u8; 4] = b"PART";
/// Marks the index-relfilenode section, appended after the [`PART_MAGIC`] block —
/// a sixth backward-compatible tail carrying the physical B-tree relfilenode of
/// each index (zipped by position onto the metadata decoded from the `CRM1`
/// block). A reader that predates physical indexes stops above, so every index
/// decodes with `rel = 0` (metadata-only) — exactly today's behavior.
const IDXR_MAGIC: &[u8; 4] = b"IXR1";
/// Marks the persistence section, appended after the [`IDXR_MAGIC`] block — a
/// seventh backward-compatible tail carrying each relation's `relpersistence`
/// (one byte per relation in `rels` order: `'p'` permanent, `'u'` unlogged; only
/// those two persist). A reader that predates this tail stops above and treats
/// every persisted relation as permanent.
const RPRS_MAGIC: &[u8; 4] = b"RPR1";
/// Marks the column-collation section, appended after the [`RPRS_MAGIC`] block —
/// an eighth backward-compatible tail carrying each table column's explicit
/// `COLLATE` OID (`0` = none, i.e. the type default). A reader that predates
/// collations stops above and decodes every column with no collation, which is
/// the pre-collation behavior: byte ordering everywhere.
const COLL_MAGIC: &[u8; 4] = b"COL1";
/// Marks the statistics section, appended after the [`COLL_MAGIC`] block — a
/// ninth backward-compatible tail carrying what `ANALYZE` last measured for each
/// relation. A reader that predates statistics stops above and treats every
/// relation as never analyzed, which is exactly the pre-`ANALYZE` behavior.
///
/// Statistics are the one thing in this file that is legitimately *disposable*:
/// PostgreSQL treats `pg_class.reltuples`/`pg_statistic` as best-effort, so
/// losing this tail costs plan quality and nothing else.
const STAT_MAGIC: &[u8; 4] = b"STA1";
/// Marks the table-access-method section. It is appended after statistics and
/// stores one byte per relation (`0` heap, `1` parquet). A catalog without this
/// tail predates pluggable durable table storage and therefore decodes every
/// relation as heap.
const TAM_MAGIC: &[u8; 4] = b"TAM1";
/// Marks the sort-key section, appended after the [`TAM_MAGIC`] block — an
/// eleventh backward-compatible tail carrying each relation's layout sort key
/// (the columns an engine-managed access method stores rows in order of). A
/// reader that predates it stops above and decodes every relation with an empty
/// key, which is what every relation written before this tail actually had:
/// nothing recorded an order.
const SORT_MAGIC: &[u8; 4] = b"SRT1";
/// Marks the toast-relation section, appended after the [`SORT_MAGIC`] block — a
/// twelfth backward-compatible tail carrying the relfilenode of each relation's
/// out-of-line chunk store (`0` = none). A reader that predates TOAST stops
/// above and decodes every relation with no toast relation, which is exactly
/// what every relation written before this tail had: nothing was ever stored out
/// of line.
const TOAST_MAGIC: &[u8; 4] = b"TOA1";

struct PersistCol {
    name: String,
    oid: u32,
    typmod: i32,
    nullable: bool,
    not_null_constraint: Option<String>,
    default: Option<String>,
    /// The column's explicit `COLLATE` OID, persisted in the [`COLL_MAGIC`] tail.
    /// Only relation columns carry one: a view's column collation is re-derived
    /// each time its stored SELECT text is re-bound, so it is never persisted
    /// and always decodes as `None` here.
    collation: Option<u32>,
}

/// A persisted index: its semantic metadata plus the relfilenode of its physical
/// B-tree file. `rel == 0` means "no physical index" (a pre-B-tree catalog, or a
/// metadata-only index) — the sentinel is safe because `FIRST_RELFILENODE` is 1.
struct PersistIndex {
    meta: IndexMetadata,
    rel: u32,
}

struct PersistRel {
    name: String,
    namespace: String,
    rel: u32,
    cols: Vec<PersistCol>,
    indexes: Vec<PersistIndex>,
    /// How the relation is stored. A memory table (`Unlogged`/`Temporary`) is held
    /// in the in-memory `State` for name resolution during this run but is
    /// **excluded from [`encode`]** — it never persists and so is gone on restart.
    persistence: RelPersistence,
    access_method: TableAccessMethod,
    /// `Some` on a partitioned (parent) table: its partition key.
    partition_scheme: Option<PartitionScheme>,
    /// `Some` on a leaf partition: its parent and bound.
    partition_of: Option<PartitionOf>,
    /// The relation's layout sort key, persisted in the [`SORT_MAGIC`] tail.
    /// Empty for a heap relation and for one written before that tail existed.
    sort_key: Vec<IndexKey>,
    /// What `ANALYZE` last measured, persisted in the [`STAT_MAGIC`] tail.
    /// `None` means never analyzed.
    stats: Option<PersistStats>,
    /// The relfilenode of this relation's out-of-line chunk store, persisted in
    /// the [`TOAST_MAGIC`] tail. `0` means the relation has never needed one —
    /// the same sentinel `PersistIndex::rel` uses, safe because relfilenodes
    /// start at 1.
    toast_rel: u32,
}

/// A persisted `ANALYZE` result. Relation-level only for now; `ncols` is written
/// even when zero so per-column statistics can be added to the same tail without
/// a format break.
#[derive(Clone, Debug, PartialEq)]
struct PersistStats {
    relpages: u32,
    reltuples: f64,
}

/// A persisted view: its SELECT text, derived column list, and the relations it
/// references (for `DROP ... CASCADE`). Views hold no relfilenode and no heap
/// storage — only catalog metadata — so they are persisted separately from
/// [`PersistRel`].
struct PersistView {
    name: String,
    namespace: String,
    sql: String,
    cols: Vec<PersistCol>,
    depends_on: Vec<String>,
}

/// A persisted sequence: its immutable definition plus its live, **non-
/// transactional** counter (`last_value`/`is_called`), which is rewritten to disk
/// on every `nextval`/`setval` — independent of transaction commit, so it
/// survives `ROLLBACK`. Sequences hold no relfilenode and no heap storage.
struct PersistSequence {
    name: String,
    namespace: String,
    type_oid: u32,
    start: i64,
    increment: i64,
    min: i64,
    max: i64,
    cache: i64,
    cycle: bool,
    owned_by: Option<String>,
    last_value: i64,
    is_called: bool,
}

struct State {
    next: u32,
    rels: Vec<PersistRel>,
    views: Vec<PersistView>,
    sequences: Vec<PersistSequence>,
    /// User-created schemas (`CREATE SCHEMA`), name → OID. Built-in namespaces
    /// are not tracked here.
    schemas: Vec<(String, u32)>,
    /// Next OID for a `CREATE SCHEMA`. Monotonic — never reused after a drop.
    next_nsp: u32,
}

pub struct RelCatalog {
    path: PathBuf,
    state: Mutex<State>,
}

impl RelCatalog {
    pub fn load(data_dir: &Path) -> std::io::Result<RelCatalog> {
        let dir = data_dir.join(CATALOG_SUBDIR);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(CATALOG_FILE);
        let state = match std::fs::read(&path) {
            Ok(bytes) => decode(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State {
                next: FIRST_RELFILENODE,
                rels: Vec::new(),
                views: Vec::new(),
                sequences: Vec::new(),
                schemas: Vec::new(),
                next_nsp: FIRST_NORMAL_OBJECT_ID,
            },
            Err(e) => return Err(e),
        };
        Ok(RelCatalog {
            path,
            state: Mutex::new(state),
        })
    }

    /// Every relation's `(name, relfilenode, schema, indexes)` for rebuilding the
    /// table map at startup. Each index carries its physical B-tree relfilenode
    /// (`RelFileNode(0)` when metadata-only).
    pub fn schemas(&self) -> Vec<ReflectedRelation> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        state
            .rels
            .iter()
            .map(|r| {
                let columns = r
                    .cols
                    .iter()
                    .map(|c| {
                        let mut col =
                            Column::with_typmod(c.name.clone(), pgtype_from_oid(c.oid), c.typmod);
                        col.nullable = c.nullable;
                        col.not_null_constraint = c.not_null_constraint.clone();
                        col.default = c.default.clone();
                        col.collation = c.collation;
                        col
                    })
                    .collect();
                (
                    r.name.clone(),
                    RelFileNode(r.rel),
                    RelFileNode(r.toast_rel),
                    TableSchema {
                        name: r.name.clone(),
                        namespace: r.namespace.clone(),
                        columns,
                        persistence: r.persistence,
                        access_method: r.access_method,
                        partition_scheme: r.partition_scheme.clone(),
                        partition_of: r.partition_of.clone(),
                        sort_key: r.sort_key.clone(),
                    },
                    r.indexes
                        .iter()
                        .map(|i| (i.meta.clone(), RelFileNode(i.rel)))
                        .collect(),
                )
            })
            .collect()
    }

    /// Whether a table named `name` exists in `namespace`.
    pub fn contains_in(&self, namespace: &str, name: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .rels
            .iter()
            .any(|r| r.namespace == namespace && r.name == name)
    }

    /// Allocate a fresh relfilenode for `schema`, persist the catalog, and return
    /// the new node. The table's namespace rides on `schema.namespace`.
    pub fn create(&self, schema: &TableSchema) -> std::io::Result<RelFileNode> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let rel = state.next;
        state.next += 1;
        state.rels.push(PersistRel {
            name: schema.name.clone(),
            namespace: schema.namespace.clone(),
            rel,
            cols: schema
                .columns
                .iter()
                .map(|c| PersistCol {
                    name: c.name.clone(),
                    oid: c.ty.oid(),
                    typmod: c.typmod,
                    nullable: c.nullable,
                    not_null_constraint: c.not_null_constraint.clone(),
                    default: c.default.clone(),
                    collation: c.collation,
                })
                .collect(),
            indexes: Vec::new(),
            persistence: schema.persistence,
            access_method: schema.access_method,
            partition_scheme: schema.partition_scheme.clone(),
            partition_of: schema.partition_of.clone(),
            sort_key: schema.sort_key.clone(),
            // No chunk store until a row needs one.
            toast_rel: 0,
            // A brand-new relation has never been analyzed.
            stats: None,
        });
        self.persist(&state)?;
        Ok(RelFileNode(rel))
    }

    /// Remove `name` from the catalog and persist, returning its relfilenode (or
    /// `None` if it was not present). `next` is deliberately left untouched: it
    /// stays monotonic so a freed relfilenode is never reused, which keeps the
    /// durability invariant (see `persist`) intact even after a DROP.
    pub fn remove_in(&self, namespace: &str, name: &str) -> std::io::Result<Option<RelFileNode>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let Some(pos) = state
            .rels
            .iter()
            .position(|r| r.namespace == namespace && r.name == name)
        else {
            return Ok(None);
        };
        let removed = state.rels.remove(pos);
        let rel = removed.rel;
        // A `Temporary` table is excluded from `encode`, so removing it leaves the
        // on-disk catalog byte-identical — skip the whole rewrite + fsync (this
        // fires on every temp table at disconnect). Permanent/Unlogged rows persist.
        if removed.persistence.persists_catalog() {
            self.persist(&state)?;
        }
        Ok(Some(RelFileNode(rel)))
    }

    /// Register a user schema, returning the OID allocated for it, or
    /// `SchemaAlreadyExists`-shaped `None` when it already exists (caller maps
    /// the error). Persisted immediately.
    pub fn create_schema(&self, name: &str) -> std::io::Result<Option<u32>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        if state.schemas.iter().any(|(n, _)| n == name) {
            return Ok(None);
        }
        let oid = state.next_nsp;
        state.next_nsp += 1;
        state.schemas.push((name.to_string(), oid));
        self.persist(&state)?;
        Ok(Some(oid))
    }

    /// Remove a user schema and persist. Returns `false` if it was not present.
    pub fn remove_schema(&self, name: &str) -> std::io::Result<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let Some(pos) = state.schemas.iter().position(|(n, _)| n == name) else {
            return Ok(false);
        };
        state.schemas.remove(pos);
        self.persist(&state)?;
        Ok(true)
    }

    /// Every user schema as `(name, oid)`.
    pub fn schema_list(&self) -> Vec<(String, u32)> {
        self.state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .schemas
            .clone()
    }

    /// Allocate a fresh relfilenode WITHOUT persisting the catalog. Used by a
    /// relfilenode-swap TRUNCATE, which stages a new empty file before its
    /// transaction commits: the catalog entry is only rewritten (via
    /// [`RelCatalog::swap_relfilenode`]) once the swap commits. `next` is bumped
    /// so the id is never reused; a crash that loses this in-memory bump is
    /// repaired at recovery, which calls [`RelCatalog::observe_relfilenode`] for
    /// every relfilenode seen in the WAL before any new id is issued.
    pub fn alloc_relfilenode(&self) -> RelFileNode {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let rel = state.next;
        state.next += 1;
        RelFileNode(rel)
    }

    /// Point `table` at `new`'s file and persist the catalog. Returns the
    /// previous relfilenode, or `None` if the table is absent. Idempotent: if the
    /// table already points at `new` (a re-applied recovery swap) it returns
    /// `Some(new)` without rewriting the file.
    pub fn swap_relfilenode(
        &self,
        namespace: &str,
        table: &str,
        new: RelFileNode,
    ) -> std::io::Result<Option<RelFileNode>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let Some(rel) = state
            .rels
            .iter_mut()
            .find(|r| r.namespace == namespace && r.name == table)
        else {
            return Ok(None);
        };
        let old = rel.rel;
        if old == new.0 {
            return Ok(Some(new));
        }
        rel.rel = new.0;
        // Statistics describe the file being swapped out, so they do not carry
        // over to the empty one. Clearing them here (rather than measuring zero)
        // returns the relation to never-analyzed, which is what PostgreSQL
        // reports after a TRUNCATE. Without this the stale row count would be
        // re-persisted and reloaded on the next open.
        rel.stats = None;
        // A `Temporary` table is excluded from `encode`, so persisting here would
        // rewrite + fsync the whole catalog to produce identical bytes. Update the
        // in-memory relfilenode (so a later DROP unlinks the right rel) but skip the
        // disk write. Permanent/Unlogged persist the swapped relfilenode.
        let persists = rel.persistence.persists_catalog();
        // Keep `next` above the swapped-in id even if it was allocated on a
        // previous boot and the counter was rebuilt from an older catalog file.
        state.next = state.next.max(new.0 + 1);
        if persists {
            self.persist(&state)?;
        }
        Ok(Some(RelFileNode(old)))
    }

    /// Record what `ANALYZE` measured for a relation. Returns `false` if this
    /// catalog has no such relation.
    ///
    /// Deliberately **not** transactional: like PostgreSQL's, an `ANALYZE`
    /// result survives a `ROLLBACK` of the transaction that produced it. That is
    /// safe precisely because nothing depends on statistics for correctness —
    /// the worst a stale or rolled-back number causes is a worse plan.
    pub fn set_stats(
        &self,
        namespace: &str,
        table: &str,
        relpages: u32,
        reltuples: f64,
    ) -> std::io::Result<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let Some(rel) = state
            .rels
            .iter_mut()
            .find(|r| r.namespace == namespace && r.name == table)
        else {
            return Ok(false);
        };
        rel.stats = Some(PersistStats {
            relpages,
            reltuples,
        });
        // A `Temporary` relation is excluded from `encode`, so persisting would
        // rewrite and fsync the whole catalog to produce identical bytes. Keep
        // the in-memory statistics (this session still plans against them) and
        // skip the disk write, exactly as `swap_relfilenode` does.
        if rel.persistence.persists_catalog() {
            self.persist(&state)?;
        }
        Ok(true)
    }

    /// What `ANALYZE` last measured for a relation as `(relpages, reltuples)`, or
    /// `None` if it has never been analyzed (or does not exist here).
    pub fn stats_in(&self, namespace: &str, table: &str) -> Option<(u32, f64)> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let rel = state
            .rels
            .iter()
            .find(|r| r.namespace == namespace && r.name == table)?;
        rel.stats.as_ref().map(|s| (s.relpages, s.reltuples))
    }

    /// Raise `next` above `n` so a freshly allocated relfilenode can never alias a
    /// file that already exists on disk. Called during recovery for every old and
    /// new relfilenode named in a WAL truncate record. No persist: the following
    /// checkpoint (or the next DDL) carries the updated counter durably.
    pub fn observe_relfilenode(&self, n: RelFileNode) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        state.next = state.next.max(n.0 + 1);
    }

    /// The relfilenode `table` currently points at, or `None` if it is absent.
    pub fn current_relfilenode(&self, namespace: &str, table: &str) -> Option<RelFileNode> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        state
            .rels
            .iter()
            .find(|r| r.namespace == namespace && r.name == table)
            .map(|r| RelFileNode(r.rel))
    }

    /// Every live relfilenode in the catalog — each table's heap file, each
    /// index's physical B-tree file, **and** each table's out-of-line chunk store
    /// — for the startup orphan-file GC. Every one of those must be included or
    /// `gc_orphan_relfiles` would delete it on the next restart: for a toast
    /// relation that would leave every out-of-line attribute in the table
    /// pointing at a file that no longer exists.
    pub fn live_relfilenodes(&self) -> Vec<u32> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        state
            .rels
            .iter()
            .flat_map(|r| {
                std::iter::once(r.rel)
                    .chain(std::iter::once(r.toast_rel))
                    .chain(r.indexes.iter().map(|i| i.rel))
                    .filter(|&rel| rel != 0)
            })
            .collect()
    }

    /// Record `rel` as `table`'s out-of-line chunk store and persist the catalog.
    ///
    /// The persist is the durability commit point for the toast relation: the
    /// caller must complete it *before* writing any chunk, or a crash would leave
    /// chunks in a file the next startup's orphan sweep unlinks. Keeps `next`
    /// above `rel` so the relfilenode is never reused.
    pub fn set_toast_rel(
        &self,
        namespace: &str,
        table: &str,
        rel: RelFileNode,
    ) -> std::io::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        state.next = state.next.max(rel.0.saturating_add(1));
        let Some(r) = state
            .rels
            .iter_mut()
            .find(|r| r.namespace == namespace && r.name == table)
        else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("relation \"{namespace}.{table}\" does not exist"),
            ));
        };
        r.toast_rel = rel.0;
        self.persist(&state)
    }

    /// Record that `table` has no chunk store — what a committed TRUNCATE leaves
    /// behind, since the empty file it swaps in points at nothing.
    ///
    /// A no-op when none was recorded, so the common TRUNCATE of a table that
    /// never toasted anything costs no catalog rewrite.
    pub fn clear_toast_rel(&self, namespace: &str, table: &str) -> std::io::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let Some(r) = state
            .rels
            .iter_mut()
            .find(|r| r.namespace == namespace && r.name == table)
        else {
            return Ok(());
        };
        if r.toast_rel == 0 {
            return Ok(());
        }
        r.toast_rel = 0;
        self.persist(&state)
    }

    /// Each `Unlogged` relation's `(heap relfilenode, physical index relfilenodes)`.
    /// The startup crash-reset (`PgEngine::reset_unlogged_relations`) empties these
    /// files, since their WAL-skipped pages were never protected against a torn crash.
    pub fn unlogged_relfilenodes(&self) -> Vec<(RelFileNode, Vec<RelFileNode>)> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        state
            .rels
            .iter()
            .filter(|r| r.persistence.is_unlogged())
            .map(|r| {
                // The chunk store rides along: its pages are WAL-skipped for the
                // same reason the heap's are, so a crash leaves it just as
                // untrustworthy and it must be emptied with the rows that named
                // its chunks.
                let indexes = r
                    .indexes
                    .iter()
                    .map(|i| i.rel)
                    .chain(std::iter::once(r.toast_rel))
                    .filter(|&rel| rel != 0)
                    .map(RelFileNode)
                    .collect();
                (RelFileNode(r.rel), indexes)
            })
            .collect()
    }

    /// Record an index on `table` with the (already allocated) relfilenode `rel`
    /// of its physical B-tree (`RelFileNode(0)` for a metadata-only index), then
    /// persist. Persisting the catalog record is the durability commit point for
    /// CREATE INDEX, so the caller must build and WAL-flush the B-tree *before*
    /// calling this — otherwise a crash could leave a durable index record whose
    /// B-tree was never made durable. Keeps `next` above `rel` so the relfilenode
    /// is never reused.
    pub fn add_index_in(
        &self,
        namespace: &str,
        table: &str,
        index: IndexMetadata,
        rel: RelFileNode,
    ) -> std::io::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        state.next = state.next.max(rel.0.saturating_add(1));
        let target = state
            .rels
            .iter_mut()
            .find(|r| r.namespace == namespace && r.name == table)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, table))?;
        target.indexes.push(PersistIndex {
            meta: index,
            rel: rel.0,
        });
        self.persist(&state)?;
        Ok(())
    }

    /// Remove an index from `table` and persist, returning its physical B-tree
    /// relfilenode (`None` if the index was absent, `Some(RelFileNode(0))` if it
    /// was metadata-only) so the caller can unlink the file.
    pub fn remove_index_in(
        &self,
        namespace: &str,
        table: &str,
        index_name: &str,
    ) -> std::io::Result<Option<RelFileNode>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let rel = state
            .rels
            .iter_mut()
            .find(|r| r.namespace == namespace && r.name == table)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, table))?;
        let Some(pos) = rel.indexes.iter().position(|i| i.meta.name == index_name) else {
            return Ok(None);
        };
        let removed = rel.indexes.remove(pos);
        self.persist(&state)?;
        Ok(Some(RelFileNode(removed.rel)))
    }

    /// Whether a view named `name` exists in `namespace`.
    pub fn contains_view_in(&self, namespace: &str, name: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .views
            .iter()
            .any(|v| v.namespace == namespace && v.name == name)
    }

    /// Register a view and persist the catalog. Returns `false` (without
    /// persisting) if a view of that name already exists in the view's
    /// namespace.
    pub fn create_view(&self, def: &ViewDefinition) -> std::io::Result<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        if state
            .views
            .iter()
            .any(|v| v.namespace == def.namespace && v.name == def.name)
        {
            return Ok(false);
        }
        state.views.push(PersistView {
            name: def.name.clone(),
            namespace: def.namespace.clone(),
            sql: def.sql.clone(),
            cols: def
                .columns
                .iter()
                .map(|c| PersistCol {
                    name: c.name.clone(),
                    oid: c.ty.oid(),
                    typmod: c.typmod,
                    nullable: c.nullable,
                    not_null_constraint: c.not_null_constraint.clone(),
                    default: c.default.clone(),
                    // Never persisted for a view; re-derived from its SELECT.
                    collation: None,
                })
                .collect(),
            depends_on: def.depends_on.clone(),
        });
        self.persist(&state)?;
        Ok(true)
    }

    /// Remove a view in `namespace` and persist. Returns `false` if it was not
    /// present.
    pub fn remove_view_in(&self, namespace: &str, name: &str) -> std::io::Result<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let Some(pos) = state
            .views
            .iter()
            .position(|v| v.namespace == namespace && v.name == name)
        else {
            return Ok(false);
        };
        state.views.remove(pos);
        self.persist(&state)?;
        Ok(true)
    }

    /// Every stored view, for catalog reflection, binder resolution, and drop
    /// dependency checks.
    pub fn views(&self) -> Vec<ViewDefinition> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        state.views.iter().map(persist_view_to_definition).collect()
    }

    /// Look up a single view by name in `namespace`, cloning only the match —
    /// the binder calls this per view reference, so it avoids materializing the
    /// whole view set.
    pub fn view_in(&self, namespace: &str, name: &str) -> Option<ViewDefinition> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        state
            .views
            .iter()
            .find(|v| v.namespace == namespace && v.name == name)
            .map(persist_view_to_definition)
    }

    /// Whether a sequence named `name` exists in `namespace`.
    pub fn contains_sequence_in(&self, namespace: &str, name: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .sequences
            .iter()
            .any(|s| s.namespace == namespace && s.name == name)
    }

    /// Register a sequence and persist. Returns `false` (without persisting) if a
    /// sequence of that name already exists in its namespace. The counter starts
    /// uncalled, so the first `nextval` returns `start`.
    pub fn create_sequence(&self, def: &SequenceDefinition) -> std::io::Result<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        if state
            .sequences
            .iter()
            .any(|s| s.namespace == def.namespace && s.name == def.name)
        {
            return Ok(false);
        }
        state.sequences.push(PersistSequence {
            name: def.name.clone(),
            namespace: def.namespace.clone(),
            type_oid: def.data_type.oid(),
            start: def.start,
            increment: def.increment,
            min: def.min,
            max: def.max,
            cache: def.cache,
            cycle: def.cycle,
            owned_by: def.owned_by.clone(),
            // Seed the counter at `start`; the first `nextval` returns it.
            last_value: def.start,
            is_called: false,
        });
        self.persist(&state)?;
        Ok(true)
    }

    /// Remove a sequence in `namespace` and persist. Returns `false` if it was
    /// not present.
    pub fn remove_sequence_in(&self, namespace: &str, name: &str) -> std::io::Result<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let Some(pos) = state
            .sequences
            .iter()
            .position(|s| s.namespace == namespace && s.name == name)
        else {
            return Ok(false);
        };
        state.sequences.remove(pos);
        self.persist(&state)?;
        Ok(true)
    }

    /// Every stored sequence definition, for catalog reflection and owned-drop.
    pub fn sequences(&self) -> Vec<SequenceDefinition> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        state
            .sequences
            .iter()
            .map(persist_sequence_to_definition)
            .collect()
    }

    /// A single sequence's definition in `namespace`, or `None` if absent.
    pub fn sequence_in(&self, namespace: &str, name: &str) -> Option<SequenceDefinition> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        state
            .sequences
            .iter()
            .find(|s| s.namespace == namespace && s.name == name)
            .map(persist_sequence_to_definition)
    }

    /// Advance a sequence (`nextval`) in `namespace` and persist the new counter
    /// immediately — outside any transaction, so the advance survives `ROLLBACK`.
    /// Returns the new value, or `NotFound`/`Overflow`/`Underflow` without mutating.
    pub fn advance_sequence_in(
        &self,
        namespace: &str,
        name: &str,
    ) -> std::io::Result<SequenceAdvance> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let Some(seq) = state
            .sequences
            .iter()
            .position(|s| s.namespace == namespace && s.name == name)
        else {
            return Ok(SequenceAdvance::NotFound);
        };
        let advance = {
            let def = persist_sequence_to_definition(&state.sequences[seq]);
            def.next_value(state.sequences[seq].last_value, state.sequences[seq].is_called)
        };
        if let SequenceAdvance::Value(v) = advance {
            state.sequences[seq].last_value = v;
            state.sequences[seq].is_called = true;
            self.persist(&state)?;
        }
        Ok(advance)
    }

    /// Set a sequence's counter (`setval`) in `namespace` and persist
    /// immediately. Returns the new value, or `NotFound` without mutating.
    pub fn set_sequence_in(
        &self,
        namespace: &str,
        name: &str,
        value: i64,
        is_called: bool,
    ) -> std::io::Result<SequenceAdvance> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let Some(seq) = state
            .sequences
            .iter_mut()
            .find(|s| s.namespace == namespace && s.name == name)
        else {
            return Ok(SequenceAdvance::NotFound);
        };
        seq.last_value = value;
        seq.is_called = is_called;
        self.persist(&state)?;
        Ok(SequenceAdvance::Value(value))
    }

    fn persist(&self, state: &State) -> std::io::Result<()> {
        let bytes = encode(state);
        let tmp = self.path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_data()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        // fsync the directory so the rename (and the relfilenode counter it
        // carries) is durable; otherwise a crash can lose a committed table's
        // catalog entry and revert `next`, letting a later CREATE reuse the
        // relfilenode and collide with the orphaned data file.
        if let Some(dir) = self.path.parent()
            && let Ok(d) = std::fs::File::open(dir)
        {
            d.sync_all()?;
        }
        Ok(())
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn put_opt_str(out: &mut Vec<u8>, value: &Option<String>) {
    match value {
        Some(value) => {
            out.push(1);
            put_str(out, value);
        }
        None => out.push(0),
    }
}

fn put_i64(out: &mut Vec<u8>, v: i64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn encode(state: &State) -> Vec<u8> {
    let mut out = Vec::new();
    // `Temporary` tables never persist: they live only in the in-memory `State`
    // for this run's name resolution. `Permanent` and `Unlogged` both persist their
    // definition (an Unlogged table's rows are reset on crash, but its catalog row
    // survives). Every rels section below iterates this filtered list so the
    // positional tails (namespaces, partitioning, index relfilenodes, persistence)
    // stay aligned. `next` is written verbatim, so a temp relfilenode is never
    // reused after restart even though its record is dropped.
    let rels: Vec<&PersistRel> = state
        .rels
        .iter()
        .filter(|r| r.persistence.persists_catalog())
        .collect();
    out.extend_from_slice(&state.next.to_le_bytes());
    out.extend_from_slice(&(rels.len() as u32).to_le_bytes());
    for r in &rels {
        out.extend_from_slice(&r.rel.to_le_bytes());
        put_str(&mut out, &r.name);
        out.extend_from_slice(&(r.cols.len() as u32).to_le_bytes());
        for c in &r.cols {
            put_str(&mut out, &c.name);
            out.extend_from_slice(&c.oid.to_le_bytes());
            out.extend_from_slice(&c.typmod.to_le_bytes());
        }
    }
    // Backward-compatible extension: old readers stop after the legacy
    // relation records and ignore this tail; new readers default metadata when
    // the magic is absent.
    out.extend_from_slice(META_MAGIC);
    out.extend_from_slice(&(rels.len() as u32).to_le_bytes());
    for r in &rels {
        out.extend_from_slice(&r.rel.to_le_bytes());
        out.extend_from_slice(&(r.cols.len() as u32).to_le_bytes());
        for c in &r.cols {
            out.push(u8::from(c.nullable));
            put_opt_str(&mut out, &c.not_null_constraint);
            put_opt_str(&mut out, &c.default);
        }
        out.extend_from_slice(&(r.indexes.len() as u32).to_le_bytes());
        for pi in &r.indexes {
            let index = &pi.meta;
            put_str(&mut out, &index.name);
            out.push(match index.method {
                IndexMethod::BTree => 0,
                IndexMethod::Hash => 1,
            });
            out.push(u8::from(index.unique));
            out.push(u8::from(index.nulls_distinct));
            out.push(match index.constraint {
                None => 0,
                Some(IndexConstraint::PrimaryKey) => 1,
                Some(IndexConstraint::Unique) => 2,
            });
            put_index_keys(&mut out, &index.keys);
        }
    }
    // Views: a second backward-compatible tail after the metadata block. A reader
    // that predates views stops above and never sees this.
    out.extend_from_slice(VIEW_MAGIC);
    out.extend_from_slice(&(state.views.len() as u32).to_le_bytes());
    for v in &state.views {
        put_str(&mut out, &v.name);
        put_str(&mut out, &v.sql);
        out.extend_from_slice(&(v.depends_on.len() as u32).to_le_bytes());
        for dep in &v.depends_on {
            put_str(&mut out, dep);
        }
        out.extend_from_slice(&(v.cols.len() as u32).to_le_bytes());
        for c in &v.cols {
            put_str(&mut out, &c.name);
            out.extend_from_slice(&c.oid.to_le_bytes());
            out.extend_from_slice(&c.typmod.to_le_bytes());
            out.push(u8::from(c.nullable));
            put_opt_str(&mut out, &c.not_null_constraint);
            put_opt_str(&mut out, &c.default);
        }
    }
    // Sequences: a third backward-compatible tail after the view block.
    out.extend_from_slice(SEQ_MAGIC);
    out.extend_from_slice(&(state.sequences.len() as u32).to_le_bytes());
    for s in &state.sequences {
        put_str(&mut out, &s.name);
        out.extend_from_slice(&s.type_oid.to_le_bytes());
        put_i64(&mut out, s.start);
        put_i64(&mut out, s.increment);
        put_i64(&mut out, s.min);
        put_i64(&mut out, s.max);
        put_i64(&mut out, s.cache);
        out.push(u8::from(s.cycle));
        put_opt_str(&mut out, &s.owned_by);
        put_i64(&mut out, s.last_value);
        out.push(u8::from(s.is_called));
    }
    // Namespaces: a fourth backward-compatible tail after the sequence block. It
    // carries the user schema registry plus each object's namespace, written in
    // the same order as the sections above so a reader can zip them back on. A
    // reader that predates schemas stops above and treats every object as
    // `public`.
    out.extend_from_slice(NSP_MAGIC);
    out.extend_from_slice(&state.next_nsp.to_le_bytes());
    out.extend_from_slice(&(state.schemas.len() as u32).to_le_bytes());
    for (name, oid) in &state.schemas {
        put_str(&mut out, name);
        out.extend_from_slice(&oid.to_le_bytes());
    }
    out.extend_from_slice(&(rels.len() as u32).to_le_bytes());
    for r in &rels {
        put_str(&mut out, &r.namespace);
    }
    out.extend_from_slice(&(state.views.len() as u32).to_le_bytes());
    for v in &state.views {
        put_str(&mut out, &v.namespace);
    }
    out.extend_from_slice(&(state.sequences.len() as u32).to_le_bytes());
    for s in &state.sequences {
        put_str(&mut out, &s.namespace);
    }
    // Partitioning: a fifth backward-compatible tail after the namespace block,
    // one record per relation in `rels` order (zipped back on by position). A
    // reader that predates partitioning stops above and treats every relation as
    // unpartitioned.
    out.extend_from_slice(PART_MAGIC);
    out.extend_from_slice(&(rels.len() as u32).to_le_bytes());
    for r in &rels {
        match &r.partition_scheme {
            Some(scheme) => {
                out.push(1);
                out.push(match scheme.strategy {
                    PartitionStrategy::Range => 0,
                });
                out.extend_from_slice(&(scheme.key_columns.len() as u32).to_le_bytes());
                for col in &scheme.key_columns {
                    out.extend_from_slice(&(*col as u32).to_le_bytes());
                }
            }
            None => out.push(0),
        }
        match &r.partition_of {
            Some(part) => {
                out.push(1);
                put_str(&mut out, &part.parent_namespace);
                put_str(&mut out, &part.parent_name);
                out.extend_from_slice(&(part.key_columns.len() as u32).to_le_bytes());
                for col in &part.key_columns {
                    out.extend_from_slice(&(*col as u32).to_le_bytes());
                }
                put_bound_datums(&mut out, &part.bound.from);
                put_bound_datums(&mut out, &part.bound.to);
            }
            None => out.push(0),
        }
    }
    // Index relfilenodes: a sixth backward-compatible tail after the partition
    // block. For each relation (in `rels` order) the count of its indexes then
    // each index's physical B-tree relfilenode, zipped back onto the metadata
    // decoded from the CRM1 block. Absent in a pre-B-tree file (all `rel = 0`).
    out.extend_from_slice(IDXR_MAGIC);
    out.extend_from_slice(&(rels.len() as u32).to_le_bytes());
    for r in &rels {
        out.extend_from_slice(&(r.indexes.len() as u32).to_le_bytes());
        for pi in &r.indexes {
            out.extend_from_slice(&pi.rel.to_le_bytes());
        }
    }
    // Persistence: a seventh backward-compatible tail, one `relpersistence` byte per
    // relation in `rels` order. Only Permanent/Unlogged reach here; `decode`
    // defaults to Permanent when this tail is absent (a pre-persistence catalog).
    out.extend_from_slice(RPRS_MAGIC);
    out.extend_from_slice(&(rels.len() as u32).to_le_bytes());
    for r in &rels {
        out.push(r.persistence.as_char() as u8);
    }
    // Column collations: an eighth backward-compatible tail. For each relation
    // (in `rels` order) its column count then each column's explicit `COLLATE`
    // OID, `0` meaning none. `decode` leaves every column at `None` when this
    // tail is absent (a pre-collation catalog).
    out.extend_from_slice(COLL_MAGIC);
    out.extend_from_slice(&(rels.len() as u32).to_le_bytes());
    for r in &rels {
        out.extend_from_slice(&(r.cols.len() as u32).to_le_bytes());
        for c in &r.cols {
            out.extend_from_slice(&c.collation.unwrap_or(0).to_le_bytes());
        }
    }
    // Statistics: a ninth backward-compatible tail. For each relation (in `rels`
    // order) a presence byte, and when set, what ANALYZE measured. `decode`
    // leaves every relation never-analyzed when this tail is absent. The
    // per-relation column count is written even though it is always zero today,
    // so per-column statistics can be appended inside this same tail later
    // without minting a tenth magic.
    out.extend_from_slice(STAT_MAGIC);
    out.extend_from_slice(&(rels.len() as u32).to_le_bytes());
    for r in &rels {
        match &r.stats {
            Some(stats) => {
                out.push(1);
                out.extend_from_slice(&stats.relpages.to_le_bytes());
                // reltuples is a float; persist its exact bit pattern rather
                // than a lossy decimal rendering.
                out.extend_from_slice(&stats.reltuples.to_bits().to_le_bytes());
                out.extend_from_slice(&0u32.to_le_bytes());
            }
            None => out.push(0),
        }
    }
    // Table access method: a tenth backward-compatible tail. Older catalogs
    // omitted it because heap was the only durable table access method.
    out.extend_from_slice(TAM_MAGIC);
    out.extend_from_slice(&(rels.len() as u32).to_le_bytes());
    for r in &rels {
        out.push(match r.access_method {
            TableAccessMethod::Heap => 0,
            TableAccessMethod::Parquet => 1,
            TableAccessMethod::Buffer => 2,
        });
    }
    // Layout sort key: an eleventh backward-compatible tail, in the same
    // `(column, descending, nulls_first)` shape the index keys above use.
    // Older catalogs omitted it because no relation declared an order.
    out.extend_from_slice(SORT_MAGIC);
    out.extend_from_slice(&(rels.len() as u32).to_le_bytes());
    for r in &rels {
        put_index_keys(&mut out, &r.sort_key);
    }
    // Toast relfilenodes: a twelfth backward-compatible tail. Absent in a
    // pre-TOAST file, where every relation decodes with `0` — no chunk store,
    // which is what those relations actually have.
    out.extend_from_slice(TOAST_MAGIC);
    out.extend_from_slice(&(rels.len() as u32).to_le_bytes());
    for r in &rels {
        out.extend_from_slice(&r.toast_rel.to_le_bytes());
    }
    out
}

/// Write a length-prefixed run of [`IndexKey`]s.
///
/// Shared by the index metadata in the `CRM1` block and the layout sort key in
/// the `SRT1` tail: same three fields, same order, one definition — so a field
/// added to `IndexKey` cannot reach one writer and miss the other, which would
/// desynchronize the byte stream for every tail that follows.
fn put_index_keys(out: &mut Vec<u8>, keys: &[IndexKey]) {
    out.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    for key in keys {
        out.extend_from_slice(&(key.column as u32).to_le_bytes());
        out.push(u8::from(key.descending));
        out.push(u8::from(key.nulls_first));
    }
}

fn put_bound_datums(out: &mut Vec<u8>, datums: &[PartitionBoundDatum]) {
    out.extend_from_slice(&(datums.len() as u32).to_le_bytes());
    for datum in datums {
        match datum {
            // A finite bound is a typed value, encoded with the same
            // self-describing on-page format the heap uses for a datum.
            PartitionBoundDatum::Value(v) => {
                out.push(0);
                crabgresql_types::datum::encode_datum(v, out);
            }
            PartitionBoundDatum::MinValue => out.push(1),
            PartitionBoundDatum::MaxValue => out.push(2),
        }
    }
}

struct Dec<'a> {
    b: &'a [u8],
    p: usize,
}
impl<'a> Dec<'a> {
    fn u32(&mut self) -> u32 {
        let v = u32::from_le_bytes(self.array());
        self.p += 4;
        v
    }
    fn i32(&mut self) -> i32 {
        let v = i32::from_le_bytes(self.array());
        self.p += 4;
        v
    }
    fn i64(&mut self) -> i64 {
        let v = i64::from_le_bytes(self.array());
        self.p += 8;
        v
    }
    /// A float persisted as its exact IEEE-754 bit pattern (see [`STAT_MAGIC`]).
    fn f64(&mut self) -> f64 {
        let v = u64::from_le_bytes(self.array());
        self.p += 8;
        f64::from_bits(v)
    }
    fn s(&mut self) -> String {
        let n = self.u32() as usize;
        let bytes = self.b[self.p..self.p + n].to_vec();
        let s = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => panic!("relation catalog contains invalid UTF-8"),
        };
        self.p += n;
        s
    }
    fn byte(&mut self) -> u8 {
        // Same bounds check `array` carries: a short read names the file it came
        // from rather than surfacing as a bare index panic.
        let Some(&v) = self.b.get(self.p) else {
            panic!("relation catalog is truncated");
        };
        self.p += 1;
        v
    }
    fn opt_s(&mut self) -> Option<String> {
        (self.byte() != 0).then(|| self.s())
    }
    /// Read a length-prefixed run of [`IndexKey`]s written by [`put_index_keys`].
    ///
    /// The two flag bytes are read in the order that function writes them, and
    /// this is the only place that order exists on the read side — transposing
    /// them would turn every stored `DESC NULLS LAST` into `ASC NULLS FIRST`.
    ///
    /// Deliberately grows rather than reserving `n` up front: `n` is raw on-disk
    /// data, and a corrupt length would otherwise ask the allocator for
    /// gigabytes and abort the process before a single key byte is checked.
    fn index_keys(&mut self) -> Vec<IndexKey> {
        let n = self.u32();
        let mut keys = Vec::new();
        for _ in 0..n {
            keys.push(IndexKey {
                column: self.u32() as usize,
                descending: self.byte() != 0,
                nulls_first: self.byte() != 0,
            });
        }
        keys
    }

    fn remaining(&self) -> &[u8] {
        &self.b[self.p..]
    }
    fn array<const N: usize>(&self) -> [u8; N] {
        let Some(slice) = self.b.get(self.p..self.p + N) else {
            panic!("relation catalog is truncated");
        };
        let mut out = [0; N];
        out.copy_from_slice(slice);
        out
    }
}

fn get_bound_datums(d: &mut Dec) -> Vec<PartitionBoundDatum> {
    let n = d.u32();
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        out.push(match d.byte() {
            0 => PartitionBoundDatum::Value(crabgresql_types::datum::decode_datum(d.b, &mut d.p)),
            1 => PartitionBoundDatum::MinValue,
            _ => PartitionBoundDatum::MaxValue,
        });
    }
    out
}

fn decode(bytes: &[u8]) -> State {
    let mut d = Dec { b: bytes, p: 0 };
    let next = d.u32();
    let nrels = d.u32();
    let mut rels = Vec::with_capacity(nrels as usize);
    for _ in 0..nrels {
        let rel = d.u32();
        let name = d.s();
        let ncols = d.u32();
        let mut cols = Vec::with_capacity(ncols as usize);
        for _ in 0..ncols {
            let cname = d.s();
            let oid = d.u32();
            let typmod = d.i32();
            cols.push(PersistCol {
                name: cname,
                oid,
                typmod,
                nullable: true,
                not_null_constraint: None,
                default: None,
                // Overridden below from the COL1 tail when present.
                collation: None,
            });
        }
        rels.push(PersistRel {
            name,
            // Default to `public`; overridden below from the NSP1 tail when present.
            namespace: "public".to_string(),
            rel,
            cols,
            indexes: Vec::new(),
            // Anything on disk is a durable heap: memory tables are never persisted.
            persistence: RelPersistence::Permanent,
            access_method: TableAccessMethod::Heap,
            // Default to unpartitioned; overridden below from the PART1 tail.
            partition_scheme: None,
            partition_of: None,
            // Default to no declared order; overridden below from the SRT1 tail.
            sort_key: Vec::new(),
            // Default to never analyzed; overridden below from the STA1 tail.
            stats: None,
            // Default to no chunk store; overridden below from the TOA1 tail.
            toast_rel: 0,
        });
    }
    if d.remaining().starts_with(META_MAGIC) {
        d.p += META_MAGIC.len();
        let nmeta = d.u32();
        for _ in 0..nmeta {
            let relid = d.u32();
            let ncols = d.u32();
            let Some(relpos) = rels.iter().position(|r| r.rel == relid) else {
                break;
            };
            for col in 0..ncols as usize {
                let nullable = d.byte() != 0;
                let not_null_constraint = d.opt_s();
                let default = d.opt_s();
                if let Some(c) = rels[relpos].cols.get_mut(col) {
                    c.nullable = nullable;
                    c.not_null_constraint = not_null_constraint;
                    c.default = default;
                }
            }
            let nindexes = d.u32();
            for _ in 0..nindexes {
                let name = d.s();
                let method = if d.byte() == 1 {
                    IndexMethod::Hash
                } else {
                    IndexMethod::BTree
                };
                let unique = d.byte() != 0;
                let nulls_distinct = d.byte() != 0;
                let constraint = match d.byte() {
                    1 => Some(IndexConstraint::PrimaryKey),
                    2 => Some(IndexConstraint::Unique),
                    _ => None,
                };
                let keys = d.index_keys();
                rels[relpos].indexes.push(PersistIndex {
                    meta: IndexMetadata {
                        name,
                        method,
                        keys,
                        unique,
                        nulls_distinct,
                        constraint,
                    },
                    // Physical relfilenode is filled from the IXR1 tail below;
                    // 0 (metadata-only) if that tail is absent (legacy file).
                    rel: 0,
                });
            }
        }
    }
    let mut views = Vec::new();
    if d.remaining().starts_with(VIEW_MAGIC) {
        d.p += VIEW_MAGIC.len();
        let nviews = d.u32();
        for _ in 0..nviews {
            let name = d.s();
            let sql = d.s();
            let ndeps = d.u32();
            let mut depends_on = Vec::with_capacity(ndeps as usize);
            for _ in 0..ndeps {
                depends_on.push(d.s());
            }
            let ncols = d.u32();
            let mut cols = Vec::with_capacity(ncols as usize);
            for _ in 0..ncols {
                let cname = d.s();
                let oid = d.u32();
                let typmod = d.i32();
                let nullable = d.byte() != 0;
                let not_null_constraint = d.opt_s();
                let default = d.opt_s();
                cols.push(PersistCol {
                    name: cname,
                    oid,
                    typmod,
                    nullable,
                    not_null_constraint,
                    default,
                    collation: None,
                });
            }
            views.push(PersistView {
                name,
                namespace: "public".to_string(),
                sql,
                cols,
                depends_on,
            });
        }
    }
    let mut sequences = Vec::new();
    if d.remaining().starts_with(SEQ_MAGIC) {
        d.p += SEQ_MAGIC.len();
        let nseqs = d.u32();
        for _ in 0..nseqs {
            let name = d.s();
            let type_oid = d.u32();
            let start = d.i64();
            let increment = d.i64();
            let min = d.i64();
            let max = d.i64();
            let cache = d.i64();
            let cycle = d.byte() != 0;
            let owned_by = d.opt_s();
            let last_value = d.i64();
            let is_called = d.byte() != 0;
            sequences.push(PersistSequence {
                name,
                namespace: "public".to_string(),
                type_oid,
                start,
                increment,
                min,
                max,
                cache,
                cycle,
                owned_by,
                last_value,
                is_called,
            });
        }
    }
    // Namespace tail: the user schema registry and each object's namespace,
    // overriding the `public` defaults set above. Absent in a pre-schema file.
    let mut schemas = Vec::new();
    let mut next_nsp = FIRST_NORMAL_OBJECT_ID;
    if d.remaining().starts_with(NSP_MAGIC) {
        d.p += NSP_MAGIC.len();
        next_nsp = d.u32();
        let nschemas = d.u32();
        for _ in 0..nschemas {
            let name = d.s();
            let oid = d.u32();
            schemas.push((name, oid));
        }
        let nrels = d.u32();
        for r in rels.iter_mut().take(nrels as usize) {
            r.namespace = d.s();
        }
        let nviews = d.u32();
        for v in views.iter_mut().take(nviews as usize) {
            v.namespace = d.s();
        }
        let nseqs = d.u32();
        for s in sequences.iter_mut().take(nseqs as usize) {
            s.namespace = d.s();
        }
    }
    // Partition tail: each relation's declarative-partitioning metadata, zipped
    // back on by position. Absent in a pre-partitioning file (all unpartitioned).
    if d.remaining().starts_with(PART_MAGIC) {
        d.p += PART_MAGIC.len();
        let nrels = d.u32();
        for r in rels.iter_mut().take(nrels as usize) {
            if d.byte() != 0 {
                // Only RANGE exists so far; the tag is reserved for LIST/HASH.
                let _strategy_tag = d.byte();
                let strategy = PartitionStrategy::Range;
                let nkeys = d.u32();
                let mut key_columns = Vec::with_capacity(nkeys as usize);
                for _ in 0..nkeys {
                    key_columns.push(d.u32() as usize);
                }
                r.partition_scheme = Some(PartitionScheme {
                    strategy,
                    key_columns,
                });
            }
            if d.byte() != 0 {
                let parent_namespace = d.s();
                let parent_name = d.s();
                let nkeys = d.u32();
                let mut key_columns = Vec::with_capacity(nkeys as usize);
                for _ in 0..nkeys {
                    key_columns.push(d.u32() as usize);
                }
                let from = get_bound_datums(&mut d);
                let to = get_bound_datums(&mut d);
                r.partition_of = Some(PartitionOf {
                    parent_namespace,
                    parent_name,
                    key_columns,
                    bound: PartitionBound { from, to },
                });
            }
        }
    }
    // Index-relfilenode tail: each index's physical B-tree relfilenode, zipped
    // back onto the metadata already decoded from CRM1 (by position within each
    // relation). Absent in a pre-B-tree file — every index keeps `rel = 0`.
    if d.remaining().starts_with(IDXR_MAGIC) {
        d.p += IDXR_MAGIC.len();
        let nrels = d.u32();
        for r in rels.iter_mut().take(nrels as usize) {
            let nindexes = d.u32();
            for i in 0..nindexes as usize {
                let rel = d.u32();
                if let Some(pi) = r.indexes.get_mut(i) {
                    pi.rel = rel;
                }
            }
        }
    }
    // Persistence tail: each relation's relpersistence, zipped by position. Absent
    // in a pre-persistence file — every relation keeps the Permanent default above.
    if d.remaining().starts_with(RPRS_MAGIC) {
        d.p += RPRS_MAGIC.len();
        let nrels = d.u32();
        for r in rels.iter_mut().take(nrels as usize) {
            r.persistence = match d.byte() {
                b'u' => RelPersistence::Unlogged,
                _ => RelPersistence::Permanent,
            };
        }
    }
    // Column-collation tail: each column's explicit COLLATE OID, zipped by
    // position. Absent in a pre-collation file — every column keeps the `None`
    // decoded above, i.e. the type's default collation.
    if d.remaining().starts_with(COLL_MAGIC) {
        d.p += COLL_MAGIC.len();
        let nrels = d.u32();
        for r in rels.iter_mut().take(nrels as usize) {
            let ncols = d.u32();
            for i in 0..ncols as usize {
                let collation = d.u32();
                if let Some(c) = r.cols.get_mut(i) {
                    c.collation = (collation != 0).then_some(collation);
                }
            }
        }
    }
    // Statistics tail: what ANALYZE last measured, zipped by position. Absent in
    // a pre-statistics file — every relation keeps the never-analyzed `None`
    // decoded above. The trailing column count is read and skipped; it is always
    // zero today (see `encode`).
    if d.remaining().starts_with(STAT_MAGIC) {
        d.p += STAT_MAGIC.len();
        let nrels = d.u32();
        for r in rels.iter_mut().take(nrels as usize) {
            if d.byte() == 0 {
                continue;
            }
            let relpages = d.u32();
            let reltuples = d.f64();
            let _ncols = d.u32();
            r.stats = Some(PersistStats {
                relpages,
                reltuples,
            });
        }
    }
    // Table-access-method tail: absent means the legacy heap default.
    if d.remaining().starts_with(TAM_MAGIC) {
        d.p += TAM_MAGIC.len();
        let nrels = d.u32();
        for r in rels.iter_mut().take(nrels as usize) {
            // Adding a method appends a discriminant; an unknown one decodes as
            // heap, which is also how a catalog written before this tail existed
            // is read.
            r.access_method = match d.byte() {
                1 => TableAccessMethod::Parquet,
                2 => TableAccessMethod::Buffer,
                _ => TableAccessMethod::Heap,
            };
        }
    }
    // Sort-key tail: absent means no relation declared an order, which is both
    // the legacy state and the state of every heap relation.
    if d.remaining().starts_with(SORT_MAGIC) {
        d.p += SORT_MAGIC.len();
        let nrels = d.u32();
        for r in rels.iter_mut().take(nrels as usize) {
            r.sort_key = d.index_keys();
        }
    }
    // Toast-relation tail: absent means nothing was ever stored out of line,
    // which is both the legacy state and the state of a table of narrow columns.
    if d.remaining().starts_with(TOAST_MAGIC) {
        d.p += TOAST_MAGIC.len();
        let nrels = d.u32();
        for r in rels.iter_mut().take(nrels as usize) {
            r.toast_rel = d.u32();
        }
    }
    State {
        next,
        rels,
        views,
        sequences,
        schemas,
        next_nsp,
    }
}

/// Reconstruct a [`SequenceDefinition`] from its persisted form.
fn persist_sequence_to_definition(s: &PersistSequence) -> SequenceDefinition {
    SequenceDefinition {
        name: s.name.clone(),
        namespace: s.namespace.clone(),
        data_type: pgtype_from_oid(s.type_oid),
        start: s.start,
        increment: s.increment,
        min: s.min,
        max: s.max,
        cache: s.cache,
        cycle: s.cycle,
        owned_by: s.owned_by.clone(),
    }
}

/// Reconstruct a [`ViewDefinition`] from its persisted form.
fn persist_view_to_definition(v: &PersistView) -> ViewDefinition {
    ViewDefinition {
        name: v.name.clone(),
        namespace: v.namespace.clone(),
        sql: v.sql.clone(),
        columns: v
            .cols
            .iter()
            .map(|c| {
                let mut col =
                    Column::with_typmod(c.name.clone(), pgtype_from_oid(c.oid), c.typmod);
                col.nullable = c.nullable;
                col.not_null_constraint = c.not_null_constraint.clone();
                col.default = c.default.clone();
                col
            })
            .collect(),
        depends_on: v.depends_on.clone(),
    }
}

/// Map a stored `pg_type` OID back to a [`PgType`]. Delegates to
/// [`PgType::from_oid`] (which also resolves array type OIDs like `_int4`); an
/// OID with no built-in type becomes [`PgType::User`], carrying it forward. Kept
/// as a thin wrapper so array/scalar OID handling never drifts from the
/// canonical map.
fn pgtype_from_oid(o: u32) -> PgType {
    PgType::from_oid(o).unwrap_or(PgType::User(o))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_types::Value;

    #[test]
    fn metadata_round_trips_and_legacy_prefix_defaults() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = RelCatalog::load(dir.path())?;
        let mut id = Column::new("id", PgType::Int4);
        id.nullable = false;
        id.default = Some("1 + 2".to_string());
        catalog.create(&TableSchema::new("t", vec![id]))?;
        let index_rel = catalog.alloc_relfilenode();
        catalog.add_index_in(
            "public",
            "t",
            IndexMetadata {
                name: "t_pkey".to_string(),
                method: IndexMethod::BTree,
                keys: vec![IndexKey {
                    column: 0,
                    descending: false,
                    nulls_first: false,
                }],
                unique: true,
                nulls_distinct: true,
                constraint: Some(IndexConstraint::PrimaryKey),
            },
            index_rel,
        )?;
        drop(catalog);

        let loaded = RelCatalog::load(dir.path())?;
        let (_, _, _, schema, indexes) = loaded
            .schemas()
            .pop()
            .ok_or_else(|| anyhow::anyhow!("loaded catalog is empty"))?;
        assert!(!schema.columns[0].nullable);
        assert_eq!(schema.columns[0].default.as_deref(), Some("1 + 2"));
        assert_eq!(indexes[0].0.name, "t_pkey");
        // The index was allocated a physical B-tree relfilenode, distinct from
        // the table's, and it survives the reload.
        assert_ne!(indexes[0].1.0, 0);

        let path = dir.path().join(CATALOG_SUBDIR).join(CATALOG_FILE);
        let bytes = std::fs::read(&path)?;
        let tail = bytes
            .windows(META_MAGIC.len())
            .position(|w| w == META_MAGIC)
            .ok_or_else(|| anyhow::anyhow!("metadata marker is missing"))?;
        std::fs::write(&path, &bytes[..tail])?;
        let legacy = RelCatalog::load(dir.path())?;
        let (_, _, _, schema, indexes) = legacy
            .schemas()
            .pop()
            .ok_or_else(|| anyhow::anyhow!("legacy catalog is empty"))?;
        assert!(schema.columns[0].nullable);
        assert!(schema.columns[0].default.is_none());
        assert!(indexes.is_empty());

        Ok(())
    }

    #[test]
    fn table_access_method_round_trips_and_legacy_defaults_to_heap() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = RelCatalog::load(dir.path())?;
        let mut schema = TableSchema::new("events", vec![Column::new("id", PgType::Int4)]);
        schema.access_method = TableAccessMethod::Parquet;
        catalog.create(&schema)?;
        let mut buffered = TableSchema::new("staging", vec![Column::new("id", PgType::Int4)]);
        buffered.access_method = TableAccessMethod::Buffer;
        catalog.create(&buffered)?;
        drop(catalog);

        let loaded = RelCatalog::load(dir.path())?;
        let method = |name: &str| {
            loaded
                .schemas()
                .into_iter()
                .find(|(rel_name, _, _, _, _)| rel_name == name)
                .map(|(_, _, _, schema, _)| schema.access_method)
        };
        // Every method must survive the round trip, not just the first one added:
        // the tail is one byte per relation, so a missed discriminant would
        // silently reopen the relation on the wrong storage.
        assert_eq!(method("events"), Some(TableAccessMethod::Parquet));
        assert_eq!(method("staging"), Some(TableAccessMethod::Buffer));
        drop(loaded);

        let path = dir.path().join(CATALOG_SUBDIR).join(CATALOG_FILE);
        let bytes = std::fs::read(&path)?;
        let tail = bytes
            .windows(TAM_MAGIC.len())
            .position(|window| window == TAM_MAGIC)
            .ok_or_else(|| anyhow::anyhow!("table access method marker is missing"))?;
        std::fs::write(&path, &bytes[..tail])?;
        let legacy = RelCatalog::load(dir.path())?;
        let schemas = legacy.schemas();
        assert!(!schemas.is_empty(), "legacy catalog is empty");
        for (name, _, _, schema, _) in schemas {
            assert_eq!(
                schema.access_method,
                TableAccessMethod::Heap,
                "a catalog written before the tail existed must decode as heap ({name})"
            );
        }
        Ok(())
    }

    #[test]
    fn sort_key_round_trips_and_legacy_catalog_defaults_to_no_order() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = RelCatalog::load(dir.path())?;
        let mut schema = TableSchema::new(
            "events",
            vec![
                Column::new("id", PgType::Int4),
                Column::new("at", PgType::Timestamp),
            ],
        );
        schema.access_method = TableAccessMethod::Parquet;
        // Deliberately not column order and not all-default flags: a codec that
        // dropped a field or resorted the key would still pass on `[{0, …}]`.
        // The first key's two flags DIFFER, which is what pins their order — the
        // decoder reads them as two adjacent `d.byte()` calls, so a transposition
        // is invisible to any fixture whose `descending` equals its `nulls_first`.
        schema.sort_key = vec![
            IndexKey {
                column: 1,
                descending: true,
                nulls_first: false,
            },
            IndexKey {
                column: 0,
                descending: false,
                nulls_first: true,
            },
        ];
        catalog.create(&schema)?;
        catalog.create(&TableSchema::new(
            "plain",
            vec![Column::new("id", PgType::Int4)],
        ))?;
        drop(catalog);

        let loaded = RelCatalog::load(dir.path())?;
        let key = |name: &str| {
            loaded
                .schemas()
                .into_iter()
                .find(|(rel_name, _, _, _, _)| rel_name == name)
                .map(|(_, _, _, schema, _)| schema.sort_key)
        };
        assert_eq!(key("events"), Some(schema.sort_key.clone()));
        // A heap relation declares no order, and the tail must not borrow its
        // neighbour's key when zipping by position.
        assert_eq!(key("plain"), Some(Vec::new()));
        drop(loaded);

        let path = dir.path().join(CATALOG_SUBDIR).join(CATALOG_FILE);
        let bytes = std::fs::read(&path)?;
        let tail = bytes
            .windows(SORT_MAGIC.len())
            .position(|window| window == SORT_MAGIC)
            .ok_or_else(|| anyhow::anyhow!("sort key marker is missing"))?;
        std::fs::write(&path, &bytes[..tail])?;
        let legacy = RelCatalog::load(dir.path())?;
        let schemas = legacy.schemas();
        assert!(!schemas.is_empty(), "legacy catalog is empty");
        for (name, _, _, schema, _) in schemas {
            assert!(
                schema.sort_key.is_empty(),
                "a catalog written before the tail existed must decode with no order ({name})"
            );
        }
        Ok(())
    }

    #[test]
    fn toast_relfilenode_survives_and_a_pre_toast_catalog_decodes_with_none()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = RelCatalog::load(dir.path())?;
        catalog.create(&TableSchema::new(
            "wide",
            vec![Column::new("body", PgType::Text)],
        ))?;
        catalog.create(&TableSchema::new(
            "narrow",
            vec![Column::new("id", PgType::Int4)],
        ))?;
        catalog.set_toast_rel("public", "wide", RelFileNode(77))?;
        drop(catalog);

        let loaded = RelCatalog::load(dir.path())?;
        let toast = |name: &str| {
            loaded
                .schemas()
                .into_iter()
                .find(|(rel_name, _, _, _, _)| rel_name == name)
                .map(|(_, _, toast, _, _)| toast)
        };
        assert_eq!(toast("wide"), Some(RelFileNode(77)));
        // A table that never toasted anything must not borrow its neighbour's
        // relfilenode when the tail is zipped back on by position.
        assert_eq!(toast("narrow"), Some(RelFileNode(0)));
        // The chunk store must be visible to the startup orphan sweep, or the
        // next restart unlinks it and every pointer into it dangles.
        assert!(loaded.live_relfilenodes().contains(&77));
        drop(loaded);

        // Truncate the file at the tail marker to stand in for a catalog written
        // before TOAST existed: it must still load, with no chunk store anywhere.
        let path = dir.path().join(CATALOG_SUBDIR).join(CATALOG_FILE);
        let bytes = std::fs::read(&path)?;
        let tail = bytes
            .windows(TOAST_MAGIC.len())
            .position(|window| window == TOAST_MAGIC)
            .ok_or_else(|| anyhow::anyhow!("toast marker is missing"))?;
        std::fs::write(&path, &bytes[..tail])?;
        let legacy = RelCatalog::load(dir.path())?;
        let schemas = legacy.schemas();
        assert!(!schemas.is_empty(), "legacy catalog is empty");
        for (name, _, toast, _, _) in schemas {
            assert_eq!(
                toast,
                RelFileNode(0),
                "a catalog written before the tail existed must decode with no chunk store ({name})"
            );
        }
        assert!(!legacy.live_relfilenodes().contains(&0));
        Ok(())
    }

    #[test]
    fn index_relfilenode_survives_and_pre_btree_catalog_defaults_to_metadata_only()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = RelCatalog::load(dir.path())?;
        catalog.create(&TableSchema::new("t", vec![Column::new("id", PgType::Int4)]))?;
        let index_rel = catalog.alloc_relfilenode();
        catalog.add_index_in(
            "public",
            "t",
            IndexMetadata {
                name: "t_id_idx".to_string(),
                method: IndexMethod::BTree,
                keys: vec![IndexKey {
                    column: 0,
                    descending: false,
                    nulls_first: false,
                }],
                unique: false,
                nulls_distinct: true,
                constraint: None,
            },
            index_rel,
        )?;
        assert_ne!(index_rel.0, 0);
        assert!(catalog.live_relfilenodes().contains(&index_rel.0));
        drop(catalog);

        // Reload: the relfilenode round-trips through the IXR1 tail.
        let loaded = RelCatalog::load(dir.path())?;
        let (_, _, _, _, indexes) = loaded
            .schemas()
            .pop()
            .ok_or_else(|| anyhow::anyhow!("loaded catalog is empty"))?;
        assert_eq!(indexes[0].1, index_rel);

        // remove_index_in returns the relfilenode to unlink, then None.
        assert_eq!(
            loaded.remove_index_in("public", "t", "t_id_idx")?,
            Some(index_rel)
        );
        assert_eq!(loaded.remove_index_in("public", "t", "t_id_idx")?, None);

        // A pre-B-tree catalog (truncated at the IXR1 marker) keeps the index
        // metadata but decodes it as metadata-only (rel == 0).
        let catalog = RelCatalog::load(dir.path())?;
        catalog.create(&TableSchema::new("u", vec![Column::new("id", PgType::Int4)]))?;
        let u_index_rel = catalog.alloc_relfilenode();
        catalog.add_index_in(
            "public",
            "u",
            IndexMetadata {
                name: "u_id_idx".to_string(),
                method: IndexMethod::BTree,
                keys: vec![IndexKey {
                    column: 0,
                    descending: false,
                    nulls_first: false,
                }],
                unique: false,
                nulls_distinct: true,
                constraint: None,
            },
            u_index_rel,
        )?;
        drop(catalog);
        let path = dir.path().join(CATALOG_SUBDIR).join(CATALOG_FILE);
        let bytes = std::fs::read(&path)?;
        let tail = bytes
            .windows(IDXR_MAGIC.len())
            .position(|w| w == IDXR_MAGIC)
            .ok_or_else(|| anyhow::anyhow!("index-relfilenode marker is missing"))?;
        std::fs::write(&path, &bytes[..tail])?;
        let legacy = RelCatalog::load(dir.path())?;
        let (_, _, _, _, indexes) = legacy
            .schemas()
            .into_iter()
            .find(|(name, ..)| name == "u")
            .ok_or_else(|| anyhow::anyhow!("relation u missing"))?;
        assert_eq!(indexes[0].0.name, "u_id_idx");
        assert_eq!(indexes[0].1.0, 0, "legacy index decodes as metadata-only");

        Ok(())
    }

    #[test]
    fn views_round_trip_and_legacy_catalog_ignores_them() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = RelCatalog::load(dir.path())?;
        // A table plus two views, one depending on the other.
        catalog.create(&TableSchema::new("t", vec![Column::new("id", PgType::Int4)]))?;
        assert!(catalog.create_view(&ViewDefinition {
            name: "v".to_string(),
            namespace: "public".to_string(),
            sql: "SELECT id FROM t".to_string(),
            columns: vec![Column::new("id", PgType::Int4)],
            depends_on: vec!["t".to_string()],
        })?);
        assert!(catalog.create_view(&ViewDefinition {
            name: "w".to_string(),
            namespace: "public".to_string(),
            sql: "SELECT id FROM v".to_string(),
            columns: vec![Column::with_typmod("id", PgType::Int4, -1)],
            depends_on: vec!["v".to_string()],
        })?);
        // A duplicate is rejected without persisting.
        assert!(!catalog.create_view(&ViewDefinition {
            name: "v".to_string(),
            namespace: "public".to_string(),
            sql: "SELECT 1".to_string(),
            columns: vec![Column::new("x", PgType::Int4)],
            depends_on: Vec::new(),
        })?);
        drop(catalog);

        // Reload: both views survive with their SQL, columns, and dependencies.
        let loaded = RelCatalog::load(dir.path())?;
        let mut views = loaded.views();
        views.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].name, "v");
        assert_eq!(views[0].sql, "SELECT id FROM t");
        assert_eq!(views[0].columns[0].name, "id");
        assert_eq!(views[0].columns[0].ty, PgType::Int4);
        assert_eq!(views[0].depends_on, vec!["t".to_string()]);
        assert_eq!(views[1].name, "w");
        assert_eq!(views[1].depends_on, vec!["v".to_string()]);

        // Removing a view persists.
        assert!(loaded.remove_view_in("public", "v")?);
        assert!(!loaded.remove_view_in("public", "v")?);
        drop(loaded);
        let reloaded = RelCatalog::load(dir.path())?;
        assert_eq!(reloaded.views().len(), 1);

        // A pre-view catalog file (truncated at the view marker) still loads, with
        // no views — the table is unaffected.
        let path = dir.path().join(CATALOG_SUBDIR).join(CATALOG_FILE);
        let bytes = std::fs::read(&path)?;
        let tail = bytes
            .windows(VIEW_MAGIC.len())
            .position(|w| w == VIEW_MAGIC)
            .ok_or_else(|| anyhow::anyhow!("view marker is missing"))?;
        std::fs::write(&path, &bytes[..tail])?;
        let legacy = RelCatalog::load(dir.path())?;
        assert!(legacy.views().is_empty());
        assert_eq!(legacy.schemas().len(), 1);

        Ok(())
    }

    fn seq(name: &str) -> SequenceDefinition {
        SequenceDefinition {
            name: name.to_string(),
            namespace: "public".to_string(),
            data_type: PgType::Int8,
            start: 1,
            increment: 1,
            min: 1,
            max: i64::MAX,
            cache: 1,
            cycle: false,
            owned_by: None,
        }
    }

    #[test]
    fn sequences_round_trip_and_counter_survives_reload() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = RelCatalog::load(dir.path())?;
        assert!(catalog.create_sequence(&seq("s"))?);
        // A duplicate is rejected without persisting.
        assert!(!catalog.create_sequence(&seq("s"))?);
        // Advance a few times: first nextval yields start (1), then +increment.
        assert_eq!(catalog.advance_sequence_in("public", "s")?, SequenceAdvance::Value(1));
        assert_eq!(catalog.advance_sequence_in("public", "s")?, SequenceAdvance::Value(2));
        assert_eq!(
            catalog.advance_sequence_in("public", "missing")?,
            SequenceAdvance::NotFound
        );
        drop(catalog);

        // Reload: the definition and the advanced counter both survive.
        let loaded = RelCatalog::load(dir.path())?;
        let seqs = loaded.sequences();
        assert_eq!(seqs.len(), 1);
        assert_eq!(seqs[0].name, "s");
        assert_eq!(seqs[0].data_type, PgType::Int8);
        // nextval continues past the persisted counter, not from start again.
        assert_eq!(loaded.advance_sequence_in("public", "s")?, SequenceAdvance::Value(3));
        // setval resets it; the following nextval reflects is_called.
        assert_eq!(
            loaded.set_sequence_in("public", "s", 10, true)?,
            SequenceAdvance::Value(10)
        );
        assert_eq!(loaded.advance_sequence_in("public", "s")?, SequenceAdvance::Value(11));
        assert!(loaded.remove_sequence_in("public", "s")?);
        assert!(!loaded.remove_sequence_in("public", "s")?);
        drop(loaded);
        let reloaded = RelCatalog::load(dir.path())?;
        assert!(reloaded.sequences().is_empty());

        // A pre-sequence catalog file (truncated at the sequence marker) still
        // loads, with no sequences.
        let catalog = RelCatalog::load(dir.path())?;
        catalog.create(&TableSchema::new("t", vec![Column::new("id", PgType::Int4)]))?;
        catalog.create_sequence(&seq("s2"))?;
        drop(catalog);
        let path = dir.path().join(CATALOG_SUBDIR).join(CATALOG_FILE);
        let bytes = std::fs::read(&path)?;
        let tail = bytes
            .windows(SEQ_MAGIC.len())
            .position(|w| w == SEQ_MAGIC)
            .ok_or_else(|| anyhow::anyhow!("sequence marker is missing"))?;
        std::fs::write(&path, &bytes[..tail])?;
        let legacy = RelCatalog::load(dir.path())?;
        assert!(legacy.sequences().is_empty());
        assert_eq!(legacy.schemas().len(), 1);

        Ok(())
    }

    #[test]
    fn partitioning_round_trips_and_legacy_catalog_ignores_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = RelCatalog::load(dir.path())?;
        // A partitioned parent plus one leaf partition with a RANGE bound.
        let mut parent = TableSchema::new(
            "m",
            vec![
                Column::new("id", PgType::Int4),
                Column::new("d", PgType::Date),
            ],
        );
        parent.partition_scheme = Some(PartitionScheme {
            strategy: PartitionStrategy::Range,
            key_columns: vec![1],
        });
        catalog.create(&parent)?;
        let mut child = TableSchema::new(
            "m_2024",
            vec![
                Column::new("id", PgType::Int4),
                Column::new("d", PgType::Date),
            ],
        );
        child.partition_of = Some(PartitionOf {
            parent_namespace: "public".to_string(),
            parent_name: "m".to_string(),
            key_columns: vec![1],
            // 2024-01-01 as a Date value (days since 2000-01-01).
            bound: PartitionBound {
                from: vec![PartitionBoundDatum::Value(Value::Date(8766))],
                to: vec![PartitionBoundDatum::MaxValue],
            },
        });
        catalog.create(&child)?;
        drop(catalog);

        // Reload: both the parent's key and the child's parent link + bound survive.
        let loaded = RelCatalog::load(dir.path())?;
        let mut schemas = loaded.schemas();
        schemas.sort_by(|a, b| a.0.cmp(&b.0));
        let (_, _, _, parent_schema, _) = &schemas[0];
        assert_eq!(parent_schema.name, "m");
        assert_eq!(
            parent_schema.partition_scheme.as_ref().map(|s| &s.key_columns),
            Some(&vec![1])
        );
        assert!(parent_schema.partition_of.is_none());
        let (_, _, _, child_schema, _) = &schemas[1];
        assert_eq!(child_schema.name, "m_2024");
        assert!(child_schema.partition_scheme.is_none());
        let part = child_schema
            .partition_of
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("child lost its partition_of"))?;
        assert_eq!(part.parent_name, "m");
        assert_eq!(part.key_columns, vec![1]);
        assert_eq!(
            part.bound.from,
            vec![PartitionBoundDatum::Value(Value::Date(8766))]
        );
        assert_eq!(part.bound.to, vec![PartitionBoundDatum::MaxValue]);
        drop(loaded);

        // A pre-partitioning catalog file (truncated at the partition marker) still
        // loads: every relation decodes as unpartitioned.
        let path = dir.path().join(CATALOG_SUBDIR).join(CATALOG_FILE);
        let bytes = std::fs::read(&path)?;
        let tail = bytes
            .windows(PART_MAGIC.len())
            .position(|w| w == PART_MAGIC)
            .ok_or_else(|| anyhow::anyhow!("partition marker is missing"))?;
        std::fs::write(&path, &bytes[..tail])?;
        let legacy = RelCatalog::load(dir.path())?;
        assert!(
            legacy
                .schemas()
                .iter()
                .all(|(_, _, _, schema, _)| schema.partition_scheme.is_none()
                    && schema.partition_of.is_none())
        );

        Ok(())
    }

    #[test]
    fn schemas_and_namespaces_round_trip_and_legacy_catalog_defaults_public() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let catalog = RelCatalog::load(dir.path())?;
        // A user schema plus a table in it and one in `public`.
        let app_oid = catalog
            .create_schema("app")?
            .ok_or_else(|| anyhow::anyhow!("create_schema returned None"))?;
        // A duplicate schema is rejected without persisting.
        assert!(catalog.create_schema("app")?.is_none());
        catalog.create(&TableSchema::in_namespace(
            "item",
            "app",
            vec![Column::new("id", PgType::Int4)],
        ))?;
        catalog.create(&TableSchema::new("item", vec![Column::new("id", PgType::Int4)]))?;
        drop(catalog);

        // Reload: the schema, its OID, and each table's namespace all survive.
        let loaded = RelCatalog::load(dir.path())?;
        assert_eq!(loaded.schema_list(), vec![("app".to_string(), app_oid)]);
        let mut namespaces: Vec<(String, String)> = loaded
            .schemas()
            .into_iter()
            .map(|(name, _, _, schema, _)| (schema.namespace, name))
            .collect();
        namespaces.sort();
        assert_eq!(
            namespaces,
            vec![
                ("app".to_string(), "item".to_string()),
                ("public".to_string(), "item".to_string()),
            ]
        );

        // Dropping the schema persists; the OID counter stays monotonic so a new
        // schema never reuses the freed OID.
        assert!(loaded.remove_schema("app")?);
        assert!(!loaded.remove_schema("app")?);
        let next_oid = loaded
            .create_schema("app2")?
            .ok_or_else(|| anyhow::anyhow!("create_schema returned None"))?;
        assert!(next_oid > app_oid);
        drop(loaded);

        // A pre-schema catalog file (truncated at the namespace marker) still
        // loads: no user schemas, and every relation decodes as `public`.
        let path = dir.path().join(CATALOG_SUBDIR).join(CATALOG_FILE);
        let bytes = std::fs::read(&path)?;
        let tail = bytes
            .windows(NSP_MAGIC.len())
            .position(|w| w == NSP_MAGIC)
            .ok_or_else(|| anyhow::anyhow!("namespace marker is missing"))?;
        std::fs::write(&path, &bytes[..tail])?;
        let legacy = RelCatalog::load(dir.path())?;
        assert!(legacy.schema_list().is_empty());
        assert!(
            legacy
                .schemas()
                .iter()
                .all(|(_, _, _, schema, _)| schema.namespace == "public")
        );

        Ok(())
    }

    #[test]
    fn statistics_round_trip_and_a_pre_stats_catalog_reports_never_analyzed()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = RelCatalog::load(dir.path())?;
        catalog.create(&TableSchema::new(
            "t",
            vec![Column::new("id", PgType::Int4)],
        ))?;
        catalog.create(&TableSchema::new(
            "untouched",
            vec![Column::new("id", PgType::Int4)],
        ))?;
        // A fresh relation has never been analyzed.
        assert_eq!(catalog.stats_in("public", "t"), None);
        // A fractional row count exercises the float encoding: a sampled
        // reltuples is not an integer, and must survive verbatim.
        assert!(catalog.set_stats("public", "t", 17, 1234.5)?);
        assert!(!catalog.set_stats("public", "nosuch", 1, 1.0)?);
        drop(catalog);

        let loaded = RelCatalog::load(dir.path())?;
        assert_eq!(loaded.stats_in("public", "t"), Some((17, 1234.5)));
        // Statistics are per relation, not global: analyzing one must not make
        // its neighbour look analyzed.
        assert_eq!(loaded.stats_in("public", "untouched"), None);
        drop(loaded);

        // A catalog written before this tail existed stops at the missing magic
        // and decodes every relation as never analyzed — exactly the behavior
        // before ANALYZE existed.
        let path = dir.path().join(CATALOG_SUBDIR).join(CATALOG_FILE);
        let bytes = std::fs::read(&path)?;
        let tail = bytes
            .windows(STAT_MAGIC.len())
            .position(|w| w == STAT_MAGIC)
            .ok_or_else(|| anyhow::anyhow!("statistics marker is missing"))?;
        std::fs::write(&path, &bytes[..tail])?;
        let legacy = RelCatalog::load(dir.path())?;
        assert_eq!(legacy.stats_in("public", "t"), None);
        // Everything the earlier tails carry still decodes.
        assert_eq!(legacy.schemas().len(), 2);

        Ok(())
    }

    /// A committed TRUNCATE swaps the relfilenode; the measurement of the file
    /// it swapped away must not survive, in memory or on the next open.
    #[test]
    fn a_relfilenode_swap_clears_the_persisted_statistics() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = RelCatalog::load(dir.path())?;
        catalog.create(&TableSchema::new(
            "t",
            vec![Column::new("id", PgType::Int4)],
        ))?;
        catalog.create(&TableSchema::new(
            "other",
            vec![Column::new("id", PgType::Int4)],
        ))?;
        assert!(catalog.set_stats("public", "t", 17, 1234.0)?);
        assert!(catalog.set_stats("public", "other", 3, 9.0)?);

        let swapped = catalog.alloc_relfilenode();
        catalog.swap_relfilenode("public", "t", swapped)?;
        assert_eq!(catalog.stats_in("public", "t"), None);
        // Only the truncated relation is affected.
        assert_eq!(catalog.stats_in("public", "other"), Some((3, 9.0)));
        drop(catalog);

        // The clear was persisted, so a restart does not resurrect the old count.
        let reopened = RelCatalog::load(dir.path())?;
        assert_eq!(reopened.stats_in("public", "t"), None);
        assert_eq!(reopened.stats_in("public", "other"), Some((3, 9.0)));

        Ok(())
    }
}
