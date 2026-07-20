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
    Column, IndexConstraint, IndexKey, IndexMetadata, IndexMethod, TableSchema,
};
use crabgresql_types::{PgType, oid};

use crate::smgr::RelFileNode;

const CATALOG_SUBDIR: &str = "global";
const CATALOG_FILE: &str = "relcatalog";
const FIRST_RELFILENODE: u32 = 1;
const META_MAGIC: &[u8; 4] = b"CRM1";

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

struct State {
    next: u32,
    rels: Vec<PersistRel>,
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
    State { next, rels }
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
}
