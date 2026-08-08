//! The lookup from a grouping key to the caller's group, shared by
//! `Aggregate::build` and `Distinct::new`.
//!
//! Both nodes hold their groups themselves — one as accumulators, the other as
//! surviving rows — and want the same thing from a key: "which group is this,
//! if any". The index answers that, specializing its storage on the key type
//! through `agg::key_encoding`, the same classifier `agg::DistinctValues` asks,
//! so a single `int8` or `text` key costs one hash of the key itself instead of
//! a hash, a bucket vector, and a type-dispatched comparison per candidate.
//!
//! That shared classifier is as far as the sharing goes: this type and
//! `DistinctValues` stay separate because they differ on *who owns the values
//! equality is decided against*. `DistinctValues` owns them and answers for
//! itself; this index owns only group numbers and delegates to a closure over
//! storage it cannot see. Merging the two would force one of them to change
//! sides — either a set that keeps a side arena and an indirection on the
//! DISTINCT hot path, or an index that clones every group key, which is the
//! per-key `String` clone `Distinct::new` exists to avoid.
//!
//! The API is two-phase — [`GroupIndex::find`] then [`GroupIndex::record`] —
//! because the caller assigns the group's index only after deciding it is new,
//! and because `find` taking `&self` lets its equality closure borrow the
//! caller's groups freely.

use rustc_hash::FxHashMap;

use crabgresql_types::{PgType, Value};

use crate::agg;

pub struct GroupIndex {
    kind: Kind,
}

/// How a key maps to a group index. The variant is chosen once from the key
/// types; only a single-column key of a type with an injective encoding can
/// skip the general equality.
enum Kind {
    /// A one-column key whose values pack into a `u64` losslessly
    /// (`agg::scalar_code`).
    Scalar {
        ty: PgType,
        groups: FxHashMap<u64, usize>,
        /// The NULL key's group. It is held apart rather than encoded, since
        /// no code can be reserved: every `u64` is some value's. Two NULL keys
        /// group together, as PG's `GROUP BY` and `DISTINCT` have them.
        null: Option<usize>,
    },
    /// A one-column `text`/`varchar`/`name`/`bpchar` key, compared bytewise
    /// (`agg::text_key`).
    Text {
        ty: PgType,
        groups: FxHashMap<Box<str>, usize>,
        null: Option<usize>,
    },
    /// Any multi-column key, and any type without an injective encoding
    /// (`numeric`, `uuid`, enums, and the types that hash to nothing at all):
    /// the hash narrows the candidates and the caller's equality decides.
    Generic {
        tys: Vec<PgType>,
        buckets: FxHashMap<u64, Vec<usize>>,
    },
}

impl GroupIndex {
    pub fn new(tys: &[PgType]) -> Self {
        // Only a single-column key can be identified by an encoding of itself;
        // a wide key needs the hash and the caller's equality either way.
        let kind = match tys {
            [ty] => match agg::key_encoding(*ty) {
                agg::KeyEncoding::Scalar => Kind::Scalar {
                    ty: *ty,
                    groups: FxHashMap::default(),
                    null: None,
                },
                agg::KeyEncoding::Text => Kind::Text {
                    ty: *ty,
                    groups: FxHashMap::default(),
                    null: None,
                },
                agg::KeyEncoding::Generic => Kind::Generic {
                    tys: tys.to_vec(),
                    buckets: FxHashMap::default(),
                },
            },
            _ => Kind::Generic {
                tys: tys.to_vec(),
                buckets: FxHashMap::default(),
            },
        };
        Self { kind }
    }

    /// The group `key` belongs to, if it has one already.
    ///
    /// `eq` is asked whether the group at an index has this key, and is called
    /// only in the general case — the specialized variants match on the key
    /// itself, which identifies the group outright.
    pub fn find(&self, key: &[Value], eq: impl Fn(usize) -> bool) -> Option<usize> {
        match &self.kind {
            Kind::Scalar { ty, groups, null } => match one(key) {
                Value::Null => *null,
                v => groups.get(&agg::scalar_code(*ty, v)).copied(),
            },
            Kind::Text { ty, groups, null } => match one(key) {
                Value::Null => *null,
                v => groups.get(agg::text_key(*ty, v)).copied(),
            },
            Kind::Generic { tys, buckets } => buckets
                .get(&agg::hash_key(tys, key))
                .and_then(|bucket| bucket.iter().copied().find(|&i| eq(i))),
        }
    }

    /// Record that `key` now belongs to the group at `index`. Call it only
    /// after [`GroupIndex::find`] returned `None` for the same key.
    pub fn record(&mut self, key: &[Value], index: usize) {
        match &mut self.kind {
            Kind::Scalar { ty, groups, null } => match one(key) {
                Value::Null => *null = Some(index),
                v => {
                    groups.insert(agg::scalar_code(*ty, v), index);
                }
            },
            Kind::Text { ty, groups, null } => match one(key) {
                Value::Null => *null = Some(index),
                v => {
                    groups.insert(agg::text_key(*ty, v).into(), index);
                }
            },
            Kind::Generic { tys, buckets } => {
                buckets
                    .entry(agg::hash_key(tys, key))
                    .or_default()
                    .push(index);
            }
        }
    }
}

/// The single key column a specialized variant indexes. A key of any other
/// width means the index was built from types the caller then didn't use, which
/// no lookup could answer correctly.
fn one(key: &[Value]) -> &Value {
    match key {
        [v] => v,
        _ => unreachable!("specialized group index got a {}-column key", key.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::GroupIndex;
    use crabgresql_types::{Numeric, PgType, Value};

    /// Drive `keys` through an index the way `Aggregate::build` does, returning
    /// the group each key landed in. The caller's storage is `groups`, so the
    /// general path's equality closure has something to read.
    fn assign(tys: &[PgType], keys: &[Vec<Value>]) -> Vec<usize> {
        let mut index = GroupIndex::new(tys);
        let mut groups: Vec<Vec<Value>> = Vec::new();
        let mut out = Vec::new();
        for key in keys {
            let i = match index.find(key, |i| crate::agg::keys_equal(tys, &groups[i], key)) {
                Some(i) => i,
                None => {
                    let i = groups.len();
                    index.record(key, i);
                    groups.push(key.clone());
                    i
                }
            };
            out.push(i);
        }
        out
    }

    /// A user type promises nothing about the *variant* a value arrives in, so
    /// an enum key stays on the general path. Mirrors
    /// `agg::tests::distinct_over_a_user_type_tolerates_a_foreign_variant` for
    /// the grouping side.
    #[test]
    fn a_user_type_key_tolerates_a_foreign_variant() {
        let ty = PgType::User(16384);
        let red = Value::Enum {
            type_oid: 16384,
            ordinal: 0,
            label: "red".to_string(),
        };
        let keys: Vec<Vec<Value>> = [Value::Int4(1), Value::Int4(1), red.clone(), red]
            .into_iter()
            .map(|v| vec![v])
            .collect();
        assert_eq!(assign(&[ty], &keys), vec![0, 0, 1, 1]);
    }

    /// Two NULL keys group together and share a group with nothing else, on
    /// both specialized paths — the slot NULL is held in rather than encoded.
    #[test]
    fn null_keys_group_together_on_every_path() {
        for (ty, present) in [
            (PgType::Int8, Value::Int8(7)),
            (PgType::Text, Value::Text("a".to_string())),
            (
                PgType::Numeric,
                Value::Numeric(Numeric::parse("7").expect("numeric literal")),
            ),
        ] {
            let keys: Vec<Vec<Value>> = [Value::Null, present.clone(), Value::Null, present]
                .into_iter()
                .map(|v| vec![v])
                .collect();
            assert_eq!(assign(&[ty], &keys), vec![0, 1, 0, 1], "{ty:?}");
        }
    }
}
