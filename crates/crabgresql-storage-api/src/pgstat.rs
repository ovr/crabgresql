//! The cumulative statistics counters behind `pg_stat_database`,
//! `pg_stat_all_tables` and `pg_stat_all_indexes`.
//!
//! PostgreSQL keeps these in shared memory, written by every backend and read
//! through a snapshot the statistics collector hands out. Here they are one
//! [`PgStatCounters`] the server owns for its whole life, shared by `Arc`: the
//! executor's scan nodes and the server's statement paths add to it, and a
//! catalog snapshot reads it.
//!
//! It lives in this crate rather than in the server because both ends need it —
//! `crabgresql-storage-api` is the one crate the executor, the engines, the
//! catalog and the server all already depend on, so nothing new has to point at
//! anything else.
//!
//! **Nothing here is persisted.** A restart starts from zero with
//! [`PgStatCounters::stats_reset`] stamped at startup, which is what PostgreSQL
//! itself reports after a crash — it discards the statistics file rather than
//! trusting counters it cannot prove. A clean shutdown there would keep them;
//! that difference is real and `pg_stat_database.stats_reset` is where a client
//! sees it.
//!
//! Every counter is `Relaxed`. They are read as totals long after the fact, and
//! ordering them against the work they describe would cost more than the number
//! is worth — the same argument the buffer pool's hit counters make.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// Relaxed add, the only way anything here is written.
fn bump(counter: &AtomicU64, by: u64) {
    if by > 0 {
        counter.fetch_add(by, Ordering::Relaxed);
    }
}

/// The database-wide counters, plus the per-relation table underneath.
///
/// One per server. Sessions hold `Arc`s of it; scan nodes hold `Arc`s of the
/// [`RelCounters`] inside it, which is what keeps the hot path off the map.
#[derive(Debug)]
pub struct PgStatCounters {
    /// Transactions that ended in a commit, and in a rollback. Counted at the
    /// point the session finalizes, so an autocommit statement counts one and a
    /// `BEGIN … COMMIT` block counts one, as in PostgreSQL.
    xact_commit: AtomicU64,
    xact_rollback: AtomicU64,
    /// Rows a sequential scan handed up, and rows an index probe fetched from
    /// the heap. PostgreSQL's split is exactly this one: `tup_returned` is what
    /// the scan read, `tup_fetched` what an index pointed at.
    tup_returned: AtomicU64,
    tup_fetched: AtomicU64,
    tup_inserted: AtomicU64,
    tup_updated: AtomicU64,
    tup_deleted: AtomicU64,
    /// Connections open right now, and connections ever opened.
    numbackends: AtomicI32,
    sessions: AtomicU64,
    /// When these counters started counting, in `timestamptz` micros.
    stats_reset: i64,
    /// Per-relation counters, keyed as `(namespace, name)` — the pair
    /// `TableSchema` carries, which is also how the catalog resolves a relation
    /// back to its `pg_class` OID. Not keyed by OID because an OID here is
    /// assigned by the *reading* snapshot, so a writer has none to use.
    ///
    /// An `RwLock` around a map of `Arc`s rather than a lock per counter: a scan
    /// takes the read lock once to fetch its `Arc` and never again, so the only
    /// writer is the first statement to touch a relation.
    relations: RwLock<HashMap<(String, String), Arc<RelCounters>>>,
}

/// One relation's counters, as `pg_stat_all_tables` reports them.
#[derive(Debug, Default)]
pub struct RelCounters {
    seq_scan: AtomicU64,
    seq_tup_read: AtomicU64,
    /// When the last sequential scan started, in `timestamptz` micros, or `0`
    /// for a relation nothing has scanned — which the catalog renders as the
    /// NULL PostgreSQL reports for the same state.
    last_seq_scan: AtomicI64,
    idx_scan: AtomicU64,
    idx_tup_fetch: AtomicU64,
    last_idx_scan: AtomicI64,
    n_tup_ins: AtomicU64,
    n_tup_upd: AtomicU64,
    n_tup_del: AtomicU64,
    /// Rows written since the last `ANALYZE` / inserted since the last
    /// `VACUUM`. Unlike the three above these run *backwards*: each is zeroed
    /// by the command it counts up to, which is how PostgreSQL's autovacuum
    /// launcher decides a relation needs attention.
    n_mod_since_analyze: AtomicU64,
    n_ins_since_vacuum: AtomicU64,
    last_vacuum: AtomicI64,
    last_analyze: AtomicI64,
    vacuum_count: AtomicU64,
    analyze_count: AtomicU64,
    /// Per-index counters, keyed by index name (unique within a relation).
    indexes: RwLock<HashMap<String, Arc<IndexCounters>>>,
}

/// One index's counters, as `pg_stat_all_indexes` reports them.
#[derive(Debug, Default)]
pub struct IndexCounters {
    idx_scan: AtomicU64,
    idx_tup_read: AtomicU64,
    idx_tup_fetch: AtomicU64,
    last_idx_scan: AtomicI64,
}

/// The database counters at one instant. Plain numbers: a snapshot is read
/// column by column while a catalog row is built, and re-reading an atomic per
/// column would let a row disagree with itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DbStatSnapshot {
    pub numbackends: i32,
    pub xact_commit: u64,
    pub xact_rollback: u64,
    pub tup_returned: u64,
    pub tup_fetched: u64,
    pub tup_inserted: u64,
    pub tup_updated: u64,
    pub tup_deleted: u64,
    pub sessions: u64,
    pub stats_reset: i64,
    /// Blocks the buffer pool had to read in, and blocks it found resident.
    /// Filled by the server from the engine's pool: this build serves one
    /// database, so the pool's totals *are* that database's totals.
    pub blks_read: u64,
    pub blks_hit: u64,
}

/// One relation's counters at one instant, named the way the catalog needs
/// them: it resolves `(schema, relation)` back to a `pg_class` OID.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelStatSnapshot {
    pub namespace: String,
    pub name: String,
    pub seq_scan: u64,
    pub seq_tup_read: u64,
    pub last_seq_scan: Option<i64>,
    pub idx_scan: u64,
    pub idx_tup_fetch: u64,
    pub last_idx_scan: Option<i64>,
    pub n_tup_ins: u64,
    pub n_tup_upd: u64,
    pub n_tup_del: u64,
    pub n_mod_since_analyze: u64,
    pub n_ins_since_vacuum: u64,
    pub last_vacuum: Option<i64>,
    pub last_analyze: Option<i64>,
    pub vacuum_count: u64,
    pub analyze_count: u64,
}

/// One index's counters at one instant, carrying the relation it belongs to so
/// the catalog can resolve both OIDs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexStatSnapshot {
    pub namespace: String,
    pub relation: String,
    pub index: String,
    pub idx_scan: u64,
    pub idx_tup_read: u64,
    pub idx_tup_fetch: u64,
    pub last_idx_scan: Option<i64>,
}

/// `0` is "never", which PostgreSQL reports as NULL. No real timestamp can
/// collide with it: it is the 2000-01-01 epoch, and a server whose clock reads
/// exactly that micro has bigger problems than one missing `last_vacuum`.
fn stamp(value: i64) -> Option<i64> {
    (value != 0).then_some(value)
}

impl PgStatCounters {
    /// Start counting, stamping [`DbStatSnapshot::stats_reset`] with `now` (in
    /// `timestamptz` micros). Taken as an argument rather than read from the
    /// clock so a test can pin it.
    pub fn new(now: i64) -> Self {
        Self {
            xact_commit: AtomicU64::new(0),
            xact_rollback: AtomicU64::new(0),
            tup_returned: AtomicU64::new(0),
            tup_fetched: AtomicU64::new(0),
            tup_inserted: AtomicU64::new(0),
            tup_updated: AtomicU64::new(0),
            tup_deleted: AtomicU64::new(0),
            numbackends: AtomicI32::new(0),
            sessions: AtomicU64::new(0),
            stats_reset: now,
            relations: RwLock::new(HashMap::new()),
        }
    }

    /// The counters for `(namespace, name)`, creating them if this is the first
    /// mention. Held by a scan node for its life, so the map is touched once per
    /// scan rather than once per row.
    pub fn relation(&self, namespace: &str, name: &str) -> Arc<RelCounters> {
        let key = (namespace.to_string(), name.to_string());
        if let Ok(relations) = self.relations.read()
            && let Some(counters) = relations.get(&key)
        {
            return Arc::clone(counters);
        }
        let Ok(mut relations) = self.relations.write() else {
            // A poisoned map costs statistics, never a query: hand back counters
            // nothing will ever read rather than propagate the panic.
            return Arc::new(RelCounters::default());
        };
        Arc::clone(relations.entry(key).or_default())
    }

    /// Forget a relation's counters, as `DROP TABLE` does in PostgreSQL — its
    /// statistics go with it, and a table later recreated under the same name
    /// starts from zero rather than inheriting the dead one's totals.
    pub fn forget_relation(&self, namespace: &str, name: &str) {
        if let Ok(mut relations) = self.relations.write() {
            relations.remove(&(namespace.to_string(), name.to_string()));
        }
    }

    pub fn xact_commit(&self) {
        bump(&self.xact_commit, 1);
    }

    pub fn xact_rollback(&self) {
        bump(&self.xact_rollback, 1);
    }

    /// Rows a sequential scan handed up, at the database level. The relation's
    /// own `seq_tup_read` is counted separately, by the scan that read them.
    pub fn tup_returned(&self, rows: u64) {
        bump(&self.tup_returned, rows);
    }

    pub fn tup_fetched(&self, rows: u64) {
        bump(&self.tup_fetched, rows);
    }

    /// A write of `rows` rows to `(namespace, name)`, counted once at both
    /// levels: the relation's `n_tup_ins`/`n_tup_upd`/`n_tup_del` and the
    /// database's `tup_inserted`/`tup_updated`/`tup_deleted`.
    pub fn tuples_written(&self, namespace: &str, name: &str, kind: WriteKind, rows: u64) {
        if rows == 0 {
            return;
        }
        let counters = self.relation(namespace, name);
        let (relation, database) = match kind {
            WriteKind::Insert => (&counters.n_tup_ins, &self.tup_inserted),
            WriteKind::Update => (&counters.n_tup_upd, &self.tup_updated),
            WriteKind::Delete => (&counters.n_tup_del, &self.tup_deleted),
        };
        bump(relation, rows);
        bump(database, rows);
        bump(&counters.n_mod_since_analyze, rows);
        if kind == WriteKind::Insert {
            bump(&counters.n_ins_since_vacuum, rows);
        }
    }

    /// A completed `VACUUM` of `(namespace, name)` at `now`.
    pub fn vacuumed(&self, namespace: &str, name: &str, now: i64) {
        let counters = self.relation(namespace, name);
        counters.last_vacuum.store(now, Ordering::Relaxed);
        counters.n_ins_since_vacuum.store(0, Ordering::Relaxed);
        bump(&counters.vacuum_count, 1);
    }

    /// A completed `ANALYZE` of `(namespace, name)` at `now`.
    pub fn analyzed(&self, namespace: &str, name: &str, now: i64) {
        let counters = self.relation(namespace, name);
        counters.last_analyze.store(now, Ordering::Relaxed);
        counters.n_mod_since_analyze.store(0, Ordering::Relaxed);
        bump(&counters.analyze_count, 1);
    }

    /// A connection opened: one more backend, and one more session ever.
    pub fn backend_started(&self) {
        self.numbackends.fetch_add(1, Ordering::Relaxed);
        bump(&self.sessions, 1);
    }

    /// A connection closed. Floored at zero rather than left to wrap: the count
    /// is published as `pg_stat_database.numbackends`, and `-1` there is worse
    /// than a lost decrement.
    pub fn backend_ended(&self) {
        let _ = self
            .numbackends
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some((n - 1).max(0))
            });
    }

    /// The database counters right now. `blks_read`/`blks_hit` are left at zero
    /// for the caller to fill from the engine's buffer pool — this crate defines
    /// the counters but does not own a pool.
    pub fn database_snapshot(&self) -> DbStatSnapshot {
        DbStatSnapshot {
            numbackends: self.numbackends.load(Ordering::Relaxed),
            xact_commit: self.xact_commit.load(Ordering::Relaxed),
            xact_rollback: self.xact_rollback.load(Ordering::Relaxed),
            tup_returned: self.tup_returned.load(Ordering::Relaxed),
            tup_fetched: self.tup_fetched.load(Ordering::Relaxed),
            tup_inserted: self.tup_inserted.load(Ordering::Relaxed),
            tup_updated: self.tup_updated.load(Ordering::Relaxed),
            tup_deleted: self.tup_deleted.load(Ordering::Relaxed),
            sessions: self.sessions.load(Ordering::Relaxed),
            stats_reset: self.stats_reset,
            blks_read: 0,
            blks_hit: 0,
        }
    }

    /// Every relation with counters, sorted by `(namespace, name)` so a
    /// `SELECT * FROM pg_stat_user_tables` is stable across runs — PostgreSQL's
    /// own order is a hash order, so nothing depends on it being different.
    pub fn relation_snapshots(&self) -> Vec<RelStatSnapshot> {
        let Ok(relations) = self.relations.read() else {
            return Vec::new();
        };
        let mut out: Vec<_> = relations
            .iter()
            .map(|((namespace, name), counters)| RelStatSnapshot {
                namespace: namespace.clone(),
                name: name.clone(),
                seq_scan: counters.seq_scan.load(Ordering::Relaxed),
                seq_tup_read: counters.seq_tup_read.load(Ordering::Relaxed),
                last_seq_scan: stamp(counters.last_seq_scan.load(Ordering::Relaxed)),
                idx_scan: counters.idx_scan.load(Ordering::Relaxed),
                idx_tup_fetch: counters.idx_tup_fetch.load(Ordering::Relaxed),
                last_idx_scan: stamp(counters.last_idx_scan.load(Ordering::Relaxed)),
                n_tup_ins: counters.n_tup_ins.load(Ordering::Relaxed),
                n_tup_upd: counters.n_tup_upd.load(Ordering::Relaxed),
                n_tup_del: counters.n_tup_del.load(Ordering::Relaxed),
                n_mod_since_analyze: counters.n_mod_since_analyze.load(Ordering::Relaxed),
                n_ins_since_vacuum: counters.n_ins_since_vacuum.load(Ordering::Relaxed),
                last_vacuum: stamp(counters.last_vacuum.load(Ordering::Relaxed)),
                last_analyze: stamp(counters.last_analyze.load(Ordering::Relaxed)),
                vacuum_count: counters.vacuum_count.load(Ordering::Relaxed),
                analyze_count: counters.analyze_count.load(Ordering::Relaxed),
            })
            .collect();
        out.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
        out
    }

    /// Every index with counters, sorted like [`Self::relation_snapshots`].
    pub fn index_snapshots(&self) -> Vec<IndexStatSnapshot> {
        let Ok(relations) = self.relations.read() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for ((namespace, name), counters) in relations.iter() {
            let Ok(indexes) = counters.indexes.read() else {
                continue;
            };
            out.extend(indexes.iter().map(|(index, counters)| IndexStatSnapshot {
                namespace: namespace.clone(),
                relation: name.clone(),
                index: index.clone(),
                idx_scan: counters.idx_scan.load(Ordering::Relaxed),
                idx_tup_read: counters.idx_tup_read.load(Ordering::Relaxed),
                idx_tup_fetch: counters.idx_tup_fetch.load(Ordering::Relaxed),
                last_idx_scan: stamp(counters.last_idx_scan.load(Ordering::Relaxed)),
            }));
        }
        out.sort_by(|a, b| {
            (&a.namespace, &a.relation, &a.index).cmp(&(&b.namespace, &b.relation, &b.index))
        });
        out
    }
}

/// Which of a relation's three write counters a statement moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteKind {
    Insert,
    Update,
    Delete,
}

impl RelCounters {
    /// One finished sequential scan that read `rows` rows, started at `at`.
    pub fn seq_scan_finished(&self, rows: u64, at: i64) {
        bump(&self.seq_scan, 1);
        bump(&self.seq_tup_read, rows);
        self.last_seq_scan.store(at, Ordering::Relaxed);
    }

    /// One finished index scan over this relation that fetched `rows` heap rows.
    pub fn idx_scan_finished(&self, rows: u64, at: i64) {
        bump(&self.idx_scan, 1);
        bump(&self.idx_tup_fetch, rows);
        self.last_idx_scan.store(at, Ordering::Relaxed);
    }

    /// The counters for one index of this relation, creating them on first use.
    pub fn index(&self, name: &str) -> Arc<IndexCounters> {
        if let Ok(indexes) = self.indexes.read()
            && let Some(counters) = indexes.get(name)
        {
            return Arc::clone(counters);
        }
        let Ok(mut indexes) = self.indexes.write() else {
            return Arc::new(IndexCounters::default());
        };
        Arc::clone(indexes.entry(name.to_string()).or_default())
    }
}

impl IndexCounters {
    /// One finished scan of this index that returned `rows` entries.
    ///
    /// `idx_tup_read` and `idx_tup_fetch` take the same number here, where
    /// PostgreSQL can report fewer fetches than reads: its `fetch` counts only
    /// the heap visits a scan actually made, and an index-only scan skips them.
    /// There is no index-only scan in this build — every match is read back from
    /// the heap — so the two really are equal, and reporting one of them lower
    /// would be the invention.
    pub fn scan_finished(&self, rows: u64, at: i64) {
        bump(&self.idx_scan, 1);
        bump(&self.idx_tup_read, rows);
        bump(&self.idx_tup_fetch, rows);
        self.last_idx_scan.store(at, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relation_counters_are_shared_by_key() {
        let stats = PgStatCounters::new(1_000);
        let first = stats.relation("public", "t");
        let second = stats.relation("public", "t");
        first.seq_scan_finished(3, 2_000);
        second.seq_scan_finished(4, 3_000);

        let snapshot = stats.relation_snapshots();
        assert_eq!(snapshot.len(), 1, "one relation, one row");
        assert_eq!(snapshot[0].seq_scan, 2);
        assert_eq!(snapshot[0].seq_tup_read, 7);
        assert_eq!(snapshot[0].last_seq_scan, Some(3_000));
    }

    /// A relation nothing has scanned reports NULL for its `last_*` stamps, not
    /// the 2000-01-01 epoch a bare `0` would render as.
    #[test]
    fn untouched_stamps_are_null() {
        let stats = PgStatCounters::new(1_000);
        stats.tuples_written("public", "t", WriteKind::Insert, 5);

        let snapshot = stats.relation_snapshots();
        assert_eq!(snapshot[0].n_tup_ins, 5);
        assert_eq!(snapshot[0].last_seq_scan, None);
        assert_eq!(snapshot[0].last_vacuum, None);
        assert_eq!(stats.database_snapshot().tup_inserted, 5);
    }

    /// Dropping a relation drops its statistics with it: a table recreated under
    /// the same name must not inherit the dead one's counters.
    #[test]
    fn forgetting_a_relation_clears_its_counters() {
        let stats = PgStatCounters::new(1_000);
        stats.tuples_written("public", "t", WriteKind::Insert, 5);
        stats.forget_relation("public", "t");

        assert!(stats.relation_snapshots().is_empty());
        // The database-level totals stay: PostgreSQL does not un-count a write
        // because the table it landed in was later dropped.
        assert_eq!(stats.database_snapshot().tup_inserted, 5);
    }

    #[test]
    fn backends_come_and_go_without_going_negative() {
        let stats = PgStatCounters::new(1_000);
        stats.backend_started();
        stats.backend_started();
        stats.backend_ended();
        stats.backend_ended();
        // One decrement more than there were backends: still zero, never -1.
        stats.backend_ended();

        let snapshot = stats.database_snapshot();
        assert_eq!(snapshot.numbackends, 0);
        assert_eq!(snapshot.sessions, 2);
        assert_eq!(snapshot.stats_reset, 1_000);
    }

    #[test]
    fn index_counters_hang_off_their_relation() {
        let stats = PgStatCounters::new(1_000);
        stats
            .relation("public", "t")
            .index("t_pkey")
            .scan_finished(2, 5_000);

        let indexes = stats.index_snapshots();
        assert_eq!(indexes.len(), 1);
        assert_eq!(indexes[0].relation, "t");
        assert_eq!(indexes[0].index, "t_pkey");
        assert_eq!(indexes[0].idx_scan, 1);
        assert_eq!(indexes[0].idx_tup_read, 2);
        assert_eq!(indexes[0].idx_tup_fetch, 2);
    }
}
