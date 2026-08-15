//! The cumulative statistics counters behind `pg_stat_database`,
//! `pg_stat_all_tables` and `pg_stat_all_indexes`.
//!
//! In this crate rather than in the server because both ends write here: it is
//! the one crate the executor, the engines, the catalog and the server all
//! already depend on.
//!
//! **Nothing is persisted.** A restart starts from zero, which is what
//! PostgreSQL reports after a crash — it discards a statistics file it cannot
//! prove. A clean shutdown there would keep the counters; `stats_reset` is
//! where a client sees the difference.
//!
//! Every counter is `Relaxed`: they are read as totals long after the fact, and
//! ordering them against the work they describe would cost more than the number
//! is worth.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

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
    /// Which transaction ended how. The rule for what counts as one is the
    /// protocol's, not this crate's — see the server's `Session`.
    xact_commit: AtomicU64,
    xact_rollback: AtomicU64,
    /// PostgreSQL's split: `tup_returned` is what a scan read, `tup_fetched`
    /// what an index pointed at.
    tup_returned: AtomicU64,
    tup_fetched: AtomicU64,
    tup_inserted: AtomicU64,
    tup_updated: AtomicU64,
    tup_deleted: AtomicU64,
    numbackends: AtomicI32,
    sessions: AtomicU64,
    /// `timestamptz` micros.
    stats_reset: i64,
    /// Keyed by name, not by OID: an OID here is assigned by the *reading*
    /// snapshot, so a writer has none to use.
    ///
    /// A map of `Arc`s rather than a lock per counter, so a scan takes the read
    /// lock once and never again.
    relations: RwLock<HashMap<(String, String), Arc<RelCounters>>>,
}

/// One relation's counters, as `pg_stat_all_tables` reports them.
#[derive(Debug, Default)]
pub struct RelCounters {
    seq_scan: AtomicU64,
    seq_tup_read: AtomicU64,
    /// `timestamptz` micros, or `0` for never; see [`stamp`].
    last_seq_scan: AtomicI64,
    idx_scan: AtomicU64,
    idx_tup_fetch: AtomicU64,
    last_idx_scan: AtomicI64,
    n_tup_ins: AtomicU64,
    n_tup_upd: AtomicU64,
    n_tup_del: AtomicU64,
    /// Unlike the three above, these two are *zeroed* by the command they count
    /// up to — that is what makes them a backlog rather than a total.
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

/// The database counters at one instant. Plain numbers, because a catalog row
/// is built column by column and re-reading an atomic per column would let one
/// row disagree with itself.
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
    /// Filled by the server from the engine's buffer pool: this build serves
    /// one database, so the pool's totals *are* that database's totals.
    pub blks_read: u64,
    pub blks_hit: u64,
}

/// One relation's counters at one instant.
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

/// One index's counters at one instant. Carries its relation, which the catalog
/// needs to resolve both OIDs.
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

/// `0` is "never", which PostgreSQL reports as NULL. Nothing real collides with
/// it: as a `timestamptz` it is the 2000-01-01 epoch.
fn stamp(value: i64) -> Option<i64> {
    (value != 0).then_some(value)
}

impl PgStatCounters {
    /// `now` is taken as an argument rather than read from the clock so a test
    /// can pin `stats_reset`.
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

    /// The counters for `(namespace, name)`, created on first mention. A scan
    /// node holds the `Arc` for its life, so the map is touched once per scan.
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

    /// Drop a relation's counters with the relation, as PostgreSQL does: one
    /// recreated under the same name starts from zero.
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

    pub fn tup_returned(&self, rows: u64) {
        bump(&self.tup_returned, rows);
    }

    pub fn tup_fetched(&self, rows: u64) {
        bump(&self.tup_fetched, rows);
    }

    /// Counted at both levels at once: the relation's `n_tup_*` and the
    /// database's `tup_*`.
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

    pub fn vacuumed(&self, namespace: &str, name: &str, now: i64) {
        let counters = self.relation(namespace, name);
        counters.last_vacuum.store(now, Ordering::Relaxed);
        counters.n_ins_since_vacuum.store(0, Ordering::Relaxed);
        bump(&counters.vacuum_count, 1);
    }

    pub fn analyzed(&self, namespace: &str, name: &str, now: i64) {
        let counters = self.relation(namespace, name);
        counters.last_analyze.store(now, Ordering::Relaxed);
        counters.n_mod_since_analyze.store(0, Ordering::Relaxed);
        bump(&counters.analyze_count, 1);
    }

    pub fn backend_started(&self) {
        self.numbackends.fetch_add(1, Ordering::Relaxed);
        bump(&self.sessions, 1);
    }

    /// Floored at zero: this is published as `pg_stat_database.numbackends`,
    /// where `-1` would be worse than a lost decrement.
    pub fn backend_ended(&self) {
        let _ = self
            .numbackends
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some((n - 1).max(0))
            });
    }

    /// `blks_read`/`blks_hit` are left at zero for the caller to fill: this
    /// crate defines the counters but owns no buffer pool.
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

    /// Sorted, so `SELECT * FROM pg_stat_user_tables` is stable across runs.
    /// PostgreSQL's own order is a hash order, so nothing can depend on it.
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

    /// Sorted, for the reason [`Self::relation_snapshots`] gives.
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
    pub fn seq_scan_finished(&self, rows: u64, at: i64) {
        bump(&self.seq_scan, 1);
        bump(&self.seq_tup_read, rows);
        self.last_seq_scan.store(at, Ordering::Relaxed);
    }

    pub fn idx_scan_finished(&self, rows: u64, at: i64) {
        bump(&self.idx_scan, 1);
        bump(&self.idx_tup_fetch, rows);
        self.last_idx_scan.store(at, Ordering::Relaxed);
    }

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
    /// `idx_tup_read` and `idx_tup_fetch` take the same number, where PostgreSQL
    /// can report fewer fetches: its `fetch` counts heap visits, which an
    /// index-only scan skips. There is no index-only scan here, so the two
    /// really are equal.
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

    /// A `last_*` stamp nothing has set must read NULL, not the 2000-01-01
    /// epoch a bare `0` renders as.
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
