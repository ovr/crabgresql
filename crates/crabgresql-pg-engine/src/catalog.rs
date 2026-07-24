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
};
use crabgresql_types::PgType;

use crate::smgr::RelFileNode;

/// One relation as reflected at startup: its name, heap relfilenode, schema, and
/// each index paired with the relfilenode of its physical B-tree.
type ReflectedRelation = (String, RelFileNode, TableSchema, Vec<(IndexMetadata, RelFileNode)>);

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

struct PersistCol {
    name: String,
    oid: u32,
    typmod: i32,
    nullable: bool,
    not_null_constraint: Option<String>,
    default: Option<String>,
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
    /// `Some` on a partitioned (parent) table: its partition key.
    partition_scheme: Option<PartitionScheme>,
    /// `Some` on a leaf partition: its parent and bound.
    partition_of: Option<PartitionOf>,
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
                        col
                    })
                    .collect();
                (
                    r.name.clone(),
                    RelFileNode(r.rel),
                    TableSchema {
                        name: r.name.clone(),
                        namespace: r.namespace.clone(),
                        columns,
                        persistence: r.persistence,
                        partition_scheme: r.partition_scheme.clone(),
                        partition_of: r.partition_of.clone(),
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
                })
                .collect(),
            indexes: Vec::new(),
            persistence: schema.persistence,
            partition_scheme: schema.partition_scheme.clone(),
            partition_of: schema.partition_of.clone(),
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
    pub fn current_relfilenode(&self, table: &str) -> Option<RelFileNode> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        state
            .rels
            .iter()
            .find(|r| r.namespace == "public" && r.name == table)
            .map(|r| RelFileNode(r.rel))
    }

    /// Every live relfilenode in the catalog — each table's heap file **and**
    /// each index's physical B-tree file — for the startup orphan-file GC. Index
    /// files must be included or `gc_orphan_relfiles` would delete them on the
    /// next restart.
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
                    .chain(r.indexes.iter().map(|i| i.rel).filter(|&rel| rel != 0))
            })
            .collect()
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
                let indexes = r
                    .indexes
                    .iter()
                    .map(|i| i.rel)
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
            out.extend_from_slice(&(index.keys.len() as u32).to_le_bytes());
            for key in &index.keys {
                out.extend_from_slice(&(key.column as u32).to_le_bytes());
                out.push(u8::from(key.descending));
                out.push(u8::from(key.nulls_first));
            }
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
    out
}

fn put_bound_datums(out: &mut Vec<u8>, datums: &[PartitionBoundDatum]) {
    out.extend_from_slice(&(datums.len() as u32).to_le_bytes());
    for datum in datums {
        match datum {
            // A finite bound is a typed value, encoded with the same
            // self-describing on-page format the heap uses for a datum.
            PartitionBoundDatum::Value(v) => {
                out.push(0);
                crate::datum::encode_datum(v, out);
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
        let v = self.b[self.p];
        self.p += 1;
        v
    }
    fn opt_s(&mut self) -> Option<String> {
        (self.byte() != 0).then(|| self.s())
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
            0 => PartitionBoundDatum::Value(crate::datum::decode_datum(d.b, &mut d.p)),
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
            // Default to unpartitioned; overridden below from the PART1 tail.
            partition_scheme: None,
            partition_of: None,
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
                let nkeys = d.u32();
                let mut keys = Vec::with_capacity(nkeys as usize);
                for _ in 0..nkeys {
                    keys.push(IndexKey {
                        column: d.u32() as usize,
                        descending: d.byte() != 0,
                        nulls_first: d.byte() != 0,
                    });
                }
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
        let (_, _, schema, indexes) = loaded
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
        let (_, _, schema, indexes) = legacy
            .schemas()
            .pop()
            .ok_or_else(|| anyhow::anyhow!("legacy catalog is empty"))?;
        assert!(schema.columns[0].nullable);
        assert!(schema.columns[0].default.is_none());
        assert!(indexes.is_empty());

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
        let (_, _, _, indexes) = loaded
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
        let (_, _, _, indexes) = legacy
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
        let (_, _, parent_schema, _) = &schemas[0];
        assert_eq!(parent_schema.name, "m");
        assert_eq!(
            parent_schema.partition_scheme.as_ref().map(|s| &s.key_columns),
            Some(&vec![1])
        );
        assert!(parent_schema.partition_of.is_none());
        let (_, _, child_schema, _) = &schemas[1];
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
                .all(|(_, _, schema, _)| schema.partition_scheme.is_none()
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
            .map(|(name, _, schema, _)| (schema.namespace, name))
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
                .all(|(_, _, schema, _)| schema.namespace == "public")
        );

        Ok(())
    }
}
