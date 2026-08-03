//! The row set a runtime UNIQUE check consults, as a keyed multiset.
//!
//! Every DML path that can create a duplicate builds one of these: the rows a
//! new tuple must not collide with, which is the relation's live rows adjusted
//! for what the statement itself has done so far. The predecessor kept that as a
//! row vector and compared the candidate against every element, so a statement
//! touching `n` rows of an `N`-row relation cost `O(n·(N+n))`.
//!
//! Two sources feed it, chosen per index:
//!
//! * the engine's own equality probe, for an index it can physically serve — the
//!   relation is then never read as a whole, which is what takes a batched
//!   `COPY` off one full scan per batch;
//! * a scan, for everything else (a metadata-only index, a `NULLS NOT DISTINCT`
//!   index, or an engine with no physical index at all).
//!
//! Either way the rows the statement itself contributes live in the buckets
//! here: they are not written until every row has been checked, so no engine can
//! answer for them.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crabgresql_storage_api::{
    ColumnProjection, IndexMetadata, StorageError, TableAm, TableSchema, Tid, Tuple,
};
use crabgresql_txn::TxnContext;
use crabgresql_types::{PgType, Value};

use crate::agg;

/// The unique-key values a candidate row must not collide with, held as one
/// bucketed multiset per unique index.
///
/// Bucketed by [`agg::hash_key`], which is documented to agree with
/// [`agg::keys_equal`]: equal keys always hash equal, so the bucket a key lands
/// in holds every row it could collide with, and `keys_equal` still decides.
/// Key equality therefore keeps a single definition — the hash only narrows the
/// search. Types whose equality is not a raw-field compare (`timetz`,
/// `interval`, `inet`, `bit`, ...) contribute nothing to the hash and share one
/// bucket, which degrades to the linear compare this replaced rather than to a
/// wrong answer.
pub(crate) struct UniqueKeySet<'a> {
    /// One entry per unique index, in the order the caller's `indexes` slice
    /// lists them, so the index reported for a collision is the one the linear
    /// predecessor would have reported.
    indexes: Vec<KeySet>,
    /// The relation to probe, for the indexes held as [`Source::Probe`].
    probe: Option<(&'a Arc<dyn TableAm>, &'a TxnContext)>,
    /// The rows recorded with a tid. This is what the row-vector predecessor
    /// answered with `position()` — whether the simulation still holds a given
    /// row — and it is not derivable from the key buckets, which a row with a
    /// skipped key never enters.
    tids: HashSet<Tid>,
}

/// One unique index's keys.
struct KeySet {
    /// Position of this index in the caller's `indexes` slice.
    slot: usize,
    name: String,
    /// Key columns as schema ordinals, and their types, resolved once here
    /// rather than per comparison.
    columns: Vec<usize>,
    types: Vec<PgType>,
    nulls_distinct: bool,
    source: Source,
    /// Keys of the rows this set answers for itself: the statement's own rows
    /// always, plus the relation's rows under [`Source::Scan`].
    buckets: HashMap<u64, Vec<Entry>>,
}

/// Where the *pre-existing* rows' keys for one index come from.
#[derive(PartialEq, Eq)]
enum Source {
    /// Asked of the engine one key at a time.
    Probe,
    /// Read into `buckets` up front.
    Scan,
}

struct Entry {
    /// The row this key came from, when it has an identity the statement can
    /// later retract. A row inserted by the statement has none.
    tid: Option<Tid>,
    key: Vec<Value>,
}

impl<'a> UniqueKeySet<'a> {
    /// The set an INSERT checks against: every unique index of `table`, with the
    /// relation's existing rows already accounted for — probed per key where the
    /// engine can, read in with one scan where it cannot.
    pub(crate) fn for_insert(
        table: &'a Arc<dyn TableAm>,
        txn: &'a TxnContext,
        schema: &TableSchema,
        indexes: &[IndexMetadata],
    ) -> Result<Self, StorageError> {
        let mut set = UniqueKeySet {
            indexes: key_sets(schema, indexes, |index| {
                // A probe is only equivalent to the scan it replaces when a NULL
                // key can be left out of the question entirely. Under `NULLS NOT
                // DISTINCT` it cannot: two NULLs collide, while the engine's
                // equality probe (which is `btkey`-encoded) has no NULL to
                // encode and answers "no such key".
                if index.nulls_distinct && table.supports_index_scan(&index.name) {
                    Source::Probe
                } else {
                    Source::Scan
                }
            }),
            probe: Some((table, txn)),
            tids: HashSet::new(),
        };
        set.seed(table, txn, schema)?;
        Ok(set)
    }

    /// The set an UPDATE simulates against. Every index reads from the buckets:
    /// the caller has the relation's rows in hand already (it is updating them)
    /// and seeds them itself, because it also has to retract the rows the
    /// statement supersedes.
    pub(crate) fn simulation(schema: &TableSchema, indexes: &[IndexMetadata]) -> Self {
        UniqueKeySet {
            indexes: key_sets(schema, indexes, |_| Source::Scan),
            probe: None,
            tids: HashSet::new(),
        }
    }

    /// A set that tracks nothing, for the paths whose statement cannot create a
    /// duplicate (no unique index, or none whose key it writes). Every check
    /// against it passes, which is what passing an empty row iterator did.
    pub(crate) fn none() -> Self {
        UniqueKeySet {
            indexes: Vec::new(),
            probe: None,
            tids: HashSet::new(),
        }
    }

    /// Read the relation once for the indexes that are not probed, if any.
    ///
    /// The scan is projected onto their key columns: the set compares those and
    /// nothing else, and a violation's DETAIL renders the *candidate* row rather
    /// than the row it collided with.
    fn seed(
        &mut self,
        table: &Arc<dyn TableAm>,
        txn: &TxnContext,
        schema: &TableSchema,
    ) -> Result<(), StorageError> {
        let scanned: Vec<usize> = self
            .indexes
            .iter()
            .filter(|set| set.source == Source::Scan)
            .flat_map(|set| set.columns.iter().copied())
            .collect();
        if scanned.is_empty() {
            return Ok(());
        }
        let projection = ColumnProjection::of(scanned, schema);
        for row in table.scan(txn, &projection) {
            let (_, tuple) = row?;
            for set in &mut self.indexes {
                if set.source == Source::Scan {
                    set.record(&tuple, None);
                }
            }
        }
        Ok(())
    }

    /// Add `tuple`'s keys. `tid` is the row's identity when the statement may
    /// later retract it ([`UniqueKeySet::forget`]), `None` for a row the
    /// statement is inserting.
    pub(crate) fn record(&mut self, tuple: &Tuple, tid: Option<Tid>) {
        if let Some(tid) = tid {
            self.tids.insert(tid);
        }
        for set in &mut self.indexes {
            set.record(tuple, tid);
        }
    }

    /// Drop the row `tid` holds, whose stored version is `tuple`. Returns
    /// whether the set held it — a row that is not there vanished under the
    /// statement, and the caller skips it rather than updating it.
    pub(crate) fn forget(&mut self, tuple: &Tuple, tid: Tid) -> bool {
        for set in &mut self.indexes {
            let Some(key) = set.key_of(tuple) else {
                continue;
            };
            // By tid alone: the entry was recorded from this very row, so it is
            // in this bucket, and one row contributes one entry per index.
            // Comparing the key values instead would ask `Value`'s `PartialEq`,
            // which is IEEE (a NaN key would never match itself).
            if let Some(bucket) = set.buckets.get_mut(&agg::hash_key(&set.types, &key))
                && let Some(pos) = bucket.iter().position(|entry| entry.tid == Some(tid))
            {
                bucket.swap_remove(pos);
            }
        }
        self.tids.remove(&tid)
    }

    /// The position in the caller's `indexes` slice of the first unique index
    /// `tuple` collides on, or `None` when it collides on none.
    pub(crate) fn conflict(&self, tuple: &Tuple) -> Result<Option<usize>, StorageError> {
        for set in &self.indexes {
            let Some(key) = set.key_of(tuple) else {
                continue;
            };
            if set.holds(&key) || (set.source == Source::Probe && self.probed(set, &key)?) {
                return Ok(Some(set.slot));
            }
        }
        Ok(None)
    }

    /// Whether the engine holds a visible row keyed `key` under `set`'s index.
    ///
    /// The probe's rows are re-checked with [`agg::keys_equal`] rather than
    /// trusted: the tree orders by `btkey`'s encoding, and only a comparison in
    /// `compare_values` terms can decide the equality this check is about.
    fn probed(&self, set: &KeySet, key: &[Value]) -> Result<bool, StorageError> {
        let Some((table, txn)) = self.probe else {
            return Ok(false);
        };
        // An engine may decline a probe it advertised (`supports_index_scan` is
        // a promise about the index, not about this key). Declining must not
        // skip the check, so it falls back to the scan the probe replaced —
        // slow, and unreachable on today's engines, but never silent.
        match table.index_lookup(&set.name, key, txn) {
            Some(rows) => {
                for row in rows {
                    let (_, tuple) = row?;
                    if let Some(found) = set.key_of(&tuple)
                        && agg::keys_equal(&set.types, &found, key)
                    {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            None => {
                let schema = table.schema();
                let projection = ColumnProjection::of(set.columns.iter().copied(), &schema);
                for row in table.scan(txn, &projection) {
                    let (_, tuple) = row?;
                    if let Some(found) = set.key_of(&tuple)
                        && agg::keys_equal(&set.types, &found, key)
                    {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }
}

/// One [`KeySet`] per unique index of `indexes`, each reading from the source
/// `source` picks for it.
fn key_sets(
    schema: &TableSchema,
    indexes: &[IndexMetadata],
    source: impl Fn(&IndexMetadata) -> Source,
) -> Vec<KeySet> {
    indexes
        .iter()
        .enumerate()
        .filter(|(_, index)| index.unique)
        .map(|(slot, index)| KeySet {
            slot,
            name: index.name.clone(),
            columns: index.keys.iter().map(|key| key.column).collect(),
            types: index
                .keys
                .iter()
                .map(|key| schema.columns[key.column].ty)
                .collect(),
            nulls_distinct: index.nulls_distinct,
            source: source(index),
            buckets: HashMap::new(),
        })
        .collect()
}

impl KeySet {
    /// `tuple`'s key for this index, or `None` when the row cannot take part in
    /// this index's uniqueness at all: under `NULLS DISTINCT` (PostgreSQL's
    /// default) a key containing NULL conflicts with nothing, so such a row is
    /// neither stored nor looked up.
    fn key_of(&self, tuple: &Tuple) -> Option<Vec<Value>> {
        let key: Vec<Value> = self.columns.iter().map(|c| tuple[*c].clone()).collect();
        let skipped = self.nulls_distinct && key.iter().any(|v| matches!(v, Value::Null));
        (!skipped).then_some(key)
    }

    fn record(&mut self, tuple: &Tuple, tid: Option<Tid>) {
        let Some(key) = self.key_of(tuple) else {
            return;
        };
        self.buckets
            .entry(agg::hash_key(&self.types, &key))
            .or_default()
            .push(Entry { tid, key });
    }

    /// Whether this set's own buckets already hold `key`.
    fn holds(&self, key: &[Value]) -> bool {
        self.buckets
            .get(&agg::hash_key(&self.types, key))
            .is_some_and(|bucket| {
                bucket
                    .iter()
                    .any(|entry| agg::keys_equal(&self.types, &entry.key, key))
            })
    }
}
