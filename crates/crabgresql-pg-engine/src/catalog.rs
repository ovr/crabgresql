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
    Column, IndexConstraint, IndexKey, IndexMetadata, IndexMethod, SequenceAdvance,
    SequenceDefinition, TableSchema, ViewDefinition,
};
use crabgresql_types::{PgType, oid};

use crate::smgr::RelFileNode;

const CATALOG_SUBDIR: &str = "global";
const CATALOG_FILE: &str = "relcatalog";
const FIRST_RELFILENODE: u32 = 1;
const META_MAGIC: &[u8; 4] = b"CRM1";
/// Marks the view section, appended after the [`META_MAGIC`] block. Like that
/// block it is a backward-compatible tail: a reader that predates views stops
/// after the metadata and ignores it.
const VIEW_MAGIC: &[u8; 4] = b"CVW1";
/// Marks the sequence section, appended after the [`VIEW_MAGIC`] block — a third
/// backward-compatible tail. A pre-sequence reader stops above and never sees it.
const SEQ_MAGIC: &[u8; 4] = b"CSQ1";

struct PersistCol {
    name: String,
    oid: u32,
    typmod: i32,
    nullable: bool,
    not_null_constraint: Option<String>,
    default: Option<String>,
}

struct PersistRel {
    name: String,
    rel: u32,
    cols: Vec<PersistCol>,
    indexes: Vec<IndexMetadata>,
}

/// A persisted view: its SELECT text, derived column list, and the relations it
/// references (for `DROP ... CASCADE`). Views hold no relfilenode and no heap
/// storage — only catalog metadata — so they are persisted separately from
/// [`PersistRel`].
struct PersistView {
    name: String,
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
            },
            Err(e) => return Err(e),
        };
        Ok(RelCatalog {
            path,
            state: Mutex::new(state),
        })
    }

    /// Every relation's `(name, relfilenode, schema)` for rebuilding the table
    /// map at startup.
    pub fn schemas(&self) -> Vec<(String, RelFileNode, TableSchema, Vec<IndexMetadata>)> {
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
                        columns,
                    },
                    r.indexes.clone(),
                )
            })
            .collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .rels
            .iter()
            .any(|r| r.name == name)
    }

    /// Allocate a fresh relfilenode for `schema`, persist the catalog, and return
    /// the new node.
    pub fn create(&self, schema: &TableSchema) -> std::io::Result<RelFileNode> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let rel = state.next;
        state.next += 1;
        state.rels.push(PersistRel {
            name: schema.name.clone(),
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
        });
        self.persist(&state)?;
        Ok(RelFileNode(rel))
    }

    /// Remove `name` from the catalog and persist, returning its relfilenode (or
    /// `None` if it was not present). `next` is deliberately left untouched: it
    /// stays monotonic so a freed relfilenode is never reused, which keeps the
    /// durability invariant (see `persist`) intact even after a DROP.
    pub fn remove(&self, name: &str) -> std::io::Result<Option<RelFileNode>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let Some(pos) = state.rels.iter().position(|r| r.name == name) else {
            return Ok(None);
        };
        let rel = state.rels.remove(pos).rel;
        self.persist(&state)?;
        Ok(Some(RelFileNode(rel)))
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
        table: &str,
        new: RelFileNode,
    ) -> std::io::Result<Option<RelFileNode>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let Some(rel) = state.rels.iter_mut().find(|r| r.name == table) else {
            return Ok(None);
        };
        let old = rel.rel;
        if old == new.0 {
            return Ok(Some(new));
        }
        rel.rel = new.0;
        // Keep `next` above the swapped-in id even if it was allocated on a
        // previous boot and the counter was rebuilt from an older catalog file.
        state.next = state.next.max(new.0 + 1);
        self.persist(&state)?;
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
            .find(|r| r.name == table)
            .map(|r| RelFileNode(r.rel))
    }

    /// Every live relfilenode in the catalog, for the startup orphan-file GC.
    pub fn live_relfilenodes(&self) -> Vec<u32> {
        self.state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .rels
            .iter()
            .map(|r| r.rel)
            .collect()
    }

    pub fn add_index(&self, table: &str, index: IndexMetadata) -> std::io::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let rel = state
            .rels
            .iter_mut()
            .find(|r| r.name == table)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, table))?;
        rel.indexes.push(index);
        self.persist(&state)
    }

    /// Whether a view named `name` exists.
    pub fn contains_view(&self, name: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .views
            .iter()
            .any(|v| v.name == name)
    }

    /// Register a view and persist the catalog. Returns `false` (without
    /// persisting) if a view of that name already exists.
    pub fn create_view(&self, def: &ViewDefinition) -> std::io::Result<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        if state.views.iter().any(|v| v.name == def.name) {
            return Ok(false);
        }
        state.views.push(PersistView {
            name: def.name.clone(),
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

    /// Remove a view and persist. Returns `false` if it was not present.
    pub fn remove_view(&self, name: &str) -> std::io::Result<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let Some(pos) = state.views.iter().position(|v| v.name == name) else {
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

    /// Look up a single view by name, cloning only the match — the binder calls
    /// this per view reference, so it avoids materializing the whole view set.
    pub fn view(&self, name: &str) -> Option<ViewDefinition> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        state
            .views
            .iter()
            .find(|v| v.name == name)
            .map(persist_view_to_definition)
    }

    /// Whether a sequence named `name` exists.
    pub fn contains_sequence(&self, name: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .sequences
            .iter()
            .any(|s| s.name == name)
    }

    /// Register a sequence and persist. Returns `false` (without persisting) if a
    /// sequence of that name already exists. The counter starts uncalled, so the
    /// first `nextval` returns `start`.
    pub fn create_sequence(&self, def: &SequenceDefinition) -> std::io::Result<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        if state.sequences.iter().any(|s| s.name == def.name) {
            return Ok(false);
        }
        state.sequences.push(PersistSequence {
            name: def.name.clone(),
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

    /// Remove a sequence and persist. Returns `false` if it was not present.
    pub fn remove_sequence(&self, name: &str) -> std::io::Result<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let Some(pos) = state.sequences.iter().position(|s| s.name == name) else {
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

    /// A single sequence's definition, or `None` if absent.
    pub fn sequence(&self, name: &str) -> Option<SequenceDefinition> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        state
            .sequences
            .iter()
            .find(|s| s.name == name)
            .map(persist_sequence_to_definition)
    }

    /// Advance a sequence (`nextval`) and persist the new counter immediately —
    /// outside any transaction, so the advance survives `ROLLBACK`. Returns the
    /// new value, or `NotFound`/`Overflow`/`Underflow` without mutating.
    pub fn advance_sequence(&self, name: &str) -> std::io::Result<SequenceAdvance> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let Some(seq) = state.sequences.iter().position(|s| s.name == name) else {
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

    /// Set a sequence's counter (`setval`) and persist immediately. Returns the
    /// new value, or `NotFound` without mutating.
    pub fn set_sequence(
        &self,
        name: &str,
        value: i64,
        is_called: bool,
    ) -> std::io::Result<SequenceAdvance> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let Some(seq) = state.sequences.iter_mut().find(|s| s.name == name) else {
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
    out.extend_from_slice(&state.next.to_le_bytes());
    out.extend_from_slice(&(state.rels.len() as u32).to_le_bytes());
    for r in &state.rels {
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
    out.extend_from_slice(&(state.rels.len() as u32).to_le_bytes());
    for r in &state.rels {
        out.extend_from_slice(&r.rel.to_le_bytes());
        out.extend_from_slice(&(r.cols.len() as u32).to_le_bytes());
        for c in &r.cols {
            out.push(u8::from(c.nullable));
            put_opt_str(&mut out, &c.not_null_constraint);
            put_opt_str(&mut out, &c.default);
        }
        out.extend_from_slice(&(r.indexes.len() as u32).to_le_bytes());
        for index in &r.indexes {
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
    out
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
            rel,
            cols,
            indexes: Vec::new(),
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
                rels[relpos].indexes.push(IndexMetadata {
                    name,
                    method,
                    keys,
                    unique,
                    nulls_distinct,
                    constraint,
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
    State {
        next,
        rels,
        views,
        sequences,
    }
}

/// Reconstruct a [`SequenceDefinition`] from its persisted form.
fn persist_sequence_to_definition(s: &PersistSequence) -> SequenceDefinition {
    SequenceDefinition {
        name: s.name.clone(),
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

/// Map a stored `pg_type` OID back to a [`PgType`]. Unknown OIDs become
/// [`PgType::User`], carrying the OID forward.
fn pgtype_from_oid(o: u32) -> PgType {
    match o {
        oid::BOOL => PgType::Bool,
        oid::INT2 => PgType::Int2,
        oid::INT4 => PgType::Int4,
        oid::INT8 => PgType::Int8,
        oid::FLOAT4 => PgType::Float4,
        oid::FLOAT8 => PgType::Float8,
        oid::NUMERIC => PgType::Numeric,
        oid::TEXT => PgType::Text,
        oid::VARCHAR => PgType::Varchar,
        oid::BPCHAR => PgType::Bpchar,
        oid::NAME => PgType::Name,
        oid::OID => PgType::Oid,
        oid::BYTEA => PgType::Bytea,
        oid::BIT => PgType::Bit,
        oid::VARBIT => PgType::Varbit,
        oid::DATE => PgType::Date,
        oid::TIME => PgType::Time,
        oid::TIMETZ => PgType::TimeTz,
        oid::TIMESTAMP => PgType::Timestamp,
        oid::TIMESTAMPTZ => PgType::TimestampTz,
        oid::INTERVAL => PgType::Interval,
        oid::UUID => PgType::Uuid,
        oid::INET => PgType::Inet,
        oid::CIDR => PgType::Cidr,
        oid::MONEY => PgType::Money,
        oid::MACADDR => PgType::Macaddr,
        oid::MACADDR8 => PgType::Macaddr8,
        oid::POINT => PgType::Point,
        oid::LSEG => PgType::Lseg,
        oid::JSON => PgType::Json,
        oid::JSONB => PgType::Jsonb,
        oid::JSONPATH => PgType::Jsonpath,
        other => PgType::User(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_round_trips_and_legacy_prefix_defaults() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = RelCatalog::load(dir.path())?;
        let mut id = Column::new("id", PgType::Int4);
        id.nullable = false;
        id.default = Some("1 + 2".to_string());
        catalog.create(&TableSchema {
            name: "t".to_string(),
            columns: vec![id],
        })?;
        catalog.add_index(
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
        )?;
        drop(catalog);

        let loaded = RelCatalog::load(dir.path())?;
        let (_, _, schema, indexes) = loaded
            .schemas()
            .pop()
            .ok_or_else(|| anyhow::anyhow!("loaded catalog is empty"))?;
        assert!(!schema.columns[0].nullable);
        assert_eq!(schema.columns[0].default.as_deref(), Some("1 + 2"));
        assert_eq!(indexes[0].name, "t_pkey");

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
    fn views_round_trip_and_legacy_catalog_ignores_them() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let catalog = RelCatalog::load(dir.path())?;
        // A table plus two views, one depending on the other.
        catalog.create(&TableSchema {
            name: "t".to_string(),
            columns: vec![Column::new("id", PgType::Int4)],
        })?;
        assert!(catalog.create_view(&ViewDefinition {
            name: "v".to_string(),
            sql: "SELECT id FROM t".to_string(),
            columns: vec![Column::new("id", PgType::Int4)],
            depends_on: vec!["t".to_string()],
        })?);
        assert!(catalog.create_view(&ViewDefinition {
            name: "w".to_string(),
            sql: "SELECT id FROM v".to_string(),
            columns: vec![Column::with_typmod("id", PgType::Int4, -1)],
            depends_on: vec!["v".to_string()],
        })?);
        // A duplicate is rejected without persisting.
        assert!(!catalog.create_view(&ViewDefinition {
            name: "v".to_string(),
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
        assert!(loaded.remove_view("v")?);
        assert!(!loaded.remove_view("v")?);
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
        assert_eq!(catalog.advance_sequence("s")?, SequenceAdvance::Value(1));
        assert_eq!(catalog.advance_sequence("s")?, SequenceAdvance::Value(2));
        assert_eq!(catalog.advance_sequence("missing")?, SequenceAdvance::NotFound);
        drop(catalog);

        // Reload: the definition and the advanced counter both survive.
        let loaded = RelCatalog::load(dir.path())?;
        let seqs = loaded.sequences();
        assert_eq!(seqs.len(), 1);
        assert_eq!(seqs[0].name, "s");
        assert_eq!(seqs[0].data_type, PgType::Int8);
        // nextval continues past the persisted counter, not from start again.
        assert_eq!(loaded.advance_sequence("s")?, SequenceAdvance::Value(3));
        // setval resets it; the following nextval reflects is_called.
        assert_eq!(loaded.set_sequence("s", 10, true)?, SequenceAdvance::Value(10));
        assert_eq!(loaded.advance_sequence("s")?, SequenceAdvance::Value(11));
        assert!(loaded.remove_sequence("s")?);
        assert!(!loaded.remove_sequence("s")?);
        drop(loaded);
        let reloaded = RelCatalog::load(dir.path())?;
        assert!(reloaded.sequences().is_empty());

        // A pre-sequence catalog file (truncated at the sequence marker) still
        // loads, with no sequences.
        let catalog = RelCatalog::load(dir.path())?;
        catalog.create(&TableSchema {
            name: "t".to_string(),
            columns: vec![Column::new("id", PgType::Int4)],
        })?;
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
}
