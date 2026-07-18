//! A minimal on-disk relation catalog: relation name -> (`RelFileNode`, schema).
//!
//! It exists so the engine can rediscover its tables and their column types
//! after a restart. It is intentionally simple — the whole catalog is rewritten
//! and fsynced on each DDL — and is not itself MVCC/crash-transactional yet; a
//! real `pg_class`/`pg_attribute`-backed catalog is future work.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crabgresql_storage_api::{Column, TableSchema};
use crabgresql_types::{PgType, oid};

use crate::smgr::RelFileNode;

const CATALOG_SUBDIR: &str = "global";
const CATALOG_FILE: &str = "relcatalog";
const FIRST_RELFILENODE: u32 = 1;

struct PersistCol {
    name: String,
    oid: u32,
    typmod: i32,
}

struct PersistRel {
    name: String,
    rel: u32,
    cols: Vec<PersistCol>,
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                State { next: FIRST_RELFILENODE, rels: Vec::new() }
            }
            Err(e) => return Err(e),
        };
        Ok(RelCatalog { path, state: Mutex::new(state) })
    }

    /// Every relation's `(name, relfilenode, schema)` for rebuilding the table
    /// map at startup.
    pub fn schemas(&self) -> Vec<(String, RelFileNode, TableSchema)> {
        let state = self.state.lock().unwrap();
        state
            .rels
            .iter()
            .map(|r| {
                let columns = r
                    .cols
                    .iter()
                    .map(|c| Column::with_typmod(c.name.clone(), pgtype_from_oid(c.oid), c.typmod))
                    .collect();
                (r.name.clone(), RelFileNode(r.rel), TableSchema { name: r.name.clone(), columns })
            })
            .collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.state.lock().unwrap().rels.iter().any(|r| r.name == name)
    }

    /// Allocate a fresh relfilenode for `schema`, persist the catalog, and return
    /// the new node.
    pub fn create(&self, schema: &TableSchema) -> std::io::Result<RelFileNode> {
        let mut state = self.state.lock().unwrap();
        let rel = state.next;
        state.next += 1;
        state.rels.push(PersistRel {
            name: schema.name.clone(),
            rel,
            cols: schema
                .columns
                .iter()
                .map(|c| PersistCol { name: c.name.clone(), oid: c.ty.oid(), typmod: c.typmod })
                .collect(),
        });
        self.persist(&state)?;
        Ok(RelFileNode(rel))
    }

    /// Remove `name` from the catalog and persist, returning its relfilenode (or
    /// `None` if it was not present). `next` is deliberately left untouched: it
    /// stays monotonic so a freed relfilenode is never reused, which keeps the
    /// durability invariant (see `persist`) intact even after a DROP.
    pub fn remove(&self, name: &str) -> std::io::Result<Option<RelFileNode>> {
        let mut state = self.state.lock().unwrap();
        let Some(pos) = state.rels.iter().position(|r| r.name == name) else {
            return Ok(None);
        };
        let rel = state.rels.remove(pos).rel;
        self.persist(&state)?;
        Ok(Some(RelFileNode(rel)))
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
    out
}

struct Dec<'a> {
    b: &'a [u8],
    p: usize,
}
impl<'a> Dec<'a> {
    fn u32(&mut self) -> u32 {
        let v = u32::from_le_bytes(self.b[self.p..self.p + 4].try_into().unwrap());
        self.p += 4;
        v
    }
    fn i32(&mut self) -> i32 {
        let v = i32::from_le_bytes(self.b[self.p..self.p + 4].try_into().unwrap());
        self.p += 4;
        v
    }
    fn s(&mut self) -> String {
        let n = self.u32() as usize;
        let s = String::from_utf8(self.b[self.p..self.p + n].to_vec()).unwrap();
        self.p += n;
        s
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
            cols.push(PersistCol { name: cname, oid, typmod });
        }
        rels.push(PersistRel { name, rel, cols });
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
        other => PgType::User(other),
    }
}
