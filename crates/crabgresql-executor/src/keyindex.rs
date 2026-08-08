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
//! The one entry point is [`GroupIndex::find_or_insert`], which takes the index
//! the caller *would* assign a new group and hashes the key once whether or not
//! it turns out to be new. Its equality closure reads the caller's groups, which
//! borrows cleanly because the index and the groups are separate values.

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

    /// The group `key` belongs to, recording `new_index` as its group if it has
    /// none yet.
    ///
    /// `eq` is asked whether the group at an index has this key, and is called
    /// only in the general case — the specialized variants match on the key
    /// itself, which identifies the group outright.
    ///
    /// A [`Slot::Vacant`] answer means the caller must now create the group at
    /// exactly `new_index`; the index is already recorded, so skipping the
    /// creation leaves a group number pointing at nothing.
    pub fn find_or_insert(
        &mut self,
        key: &[Value],
        new_index: usize,
        eq: impl Fn(usize) -> bool,
    ) -> Slot {
        match &mut self.kind {
            Kind::Scalar { ty, groups, null } => match one(key) {
                Value::Null => Slot::of(*null.get_or_insert(new_index), new_index),
                v => Slot::of(
                    *groups.entry(agg::scalar_code(*ty, v)).or_insert(new_index),
                    new_index,
                ),
            },
            // Deliberately not `entry(key.into())`: that allocates a `Box<str>`
            // on every row, hits included, which costs far more on a
            // low-cardinality `GROUP BY` than the second hash a miss pays here.
            // A miss allocates anyway.
            Kind::Text { ty, groups, null } => match one(key) {
                Value::Null => Slot::of(*null.get_or_insert(new_index), new_index),
                v => {
                    let key = agg::text_key(*ty, v);
                    match groups.get(key) {
                        Some(&i) => Slot::Existing(i),
                        None => {
                            insert_text(groups, key, new_index);
                            Slot::Vacant
                        }
                    }
                }
            },
            Kind::Generic { tys, buckets } => {
                let bucket = buckets.entry(agg::hash_key(tys, key)).or_default();
                match bucket.iter().copied().find(|&i| eq(i)) {
                    Some(i) => Slot::Existing(i),
                    None => {
                        bucket.push(new_index);
                        Slot::Vacant
                    }
                }
            }
        }
    }
}

/// What [`GroupIndex::find_or_insert`] found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    /// The key already belonged to this group.
    Existing(usize),
    /// The key was new and now belongs to the `new_index` that was passed in,
    /// which the caller still has to create.
    Vacant,
}

impl Slot {
    /// Read an `or_insert`-style answer: the stored index, and the one that
    /// would have been stored had the key been new.
    fn of(stored: usize, new_index: usize) -> Slot {
        if stored == new_index {
            Slot::Vacant
        } else {
            Slot::Existing(stored)
        }
    }
}

/// Own a new text key. Kept out of line because it is the only allocating step
/// in `find_or_insert`, and a `GROUP BY` over few distinct strings runs the
/// lookup beside it millions of times without ever reaching it.
#[cold]
#[inline(never)]
fn insert_text(groups: &mut FxHashMap<Box<str>, usize>, key: &str, new_index: usize) {
    groups.insert(key.into(), new_index);
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
    use super::{GroupIndex, Slot};
    use crabgresql_types::{Numeric, PgType, Value};

    /// Drive `keys` through an index the way `Aggregate::build` does, returning
    /// the group each key landed in. The caller's storage is `groups`, so the
    /// general path's equality closure has something to read.
    fn assign(tys: &[PgType], keys: &[Vec<Value>]) -> Vec<usize> {
        let mut index = GroupIndex::new(tys);
        let mut groups: Vec<Vec<Value>> = Vec::new();
        let mut out = Vec::new();
        for key in keys {
            let next = groups.len();
            let i = match index
                .find_or_insert(key, next, |i| crate::agg::keys_equal(tys, &groups[i], key))
            {
                Slot::Existing(i) => i,
                Slot::Vacant => {
                    groups.push(key.clone());
                    next
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

    /// Groups are numbered in first-seen order on every encoding. This is what
    /// `find_or_insert` can silently break, by recording an index on the wrong
    /// side of the caller's push.
    #[test]
    fn groups_are_numbered_in_first_seen_order() {
        let text = |s: &str| vec![Value::Text(s.to_string())];

        // Scalar.
        let keys: Vec<Vec<Value>> = [30i64, 10, 30, 20, 10]
            .into_iter()
            .map(|n| vec![Value::Int8(n)])
            .collect();
        assert_eq!(assign(&[PgType::Int8], &keys), vec![0, 1, 0, 2, 1]);

        // Text: bytewise, so case matters and trailing blanks do not fold.
        let keys = [text("b"), text("a"), text("B"), text("a "), text("a")];
        assert_eq!(assign(&[PgType::Text], &keys), vec![0, 1, 2, 3, 1]);

        // bpchar folds trailing blanks, and only trailing ones.
        let keys = [text("a"), text("a  "), text(" a"), text("A")];
        assert_eq!(assign(&[PgType::Bpchar], &keys), vec![0, 0, 1, 2]);

        // Generic, and multi-column — the arm that consults `eq`.
        let pair = |a: i64, b: &str| vec![Value::Int8(a), Value::Text(b.to_string())];
        let keys = [pair(1, "x"), pair(2, "x"), pair(1, "y"), pair(1, "x")];
        assert_eq!(
            assign(&[PgType::Int8, PgType::Text], &keys),
            vec![0, 1, 2, 0]
        );
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
