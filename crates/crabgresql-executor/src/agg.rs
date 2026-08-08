//! Aggregate accumulators: the running state of one aggregate over one group.
//!
//! Type and value rules reproduce PG 14's observable behavior (see AGENTS.md):
//! `count` is `bigint` and never NULL (0 over an empty group); `min`/`max` keep
//! the argument type; `sum` widens small integers to `bigint` and `bigint` to
//! `numeric`; `avg` of any exact type is `numeric` and of floats is `float8`.
//! Every aggregate but `count` ignores NULL inputs and yields NULL over an empty
//! (or all-NULL) group.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

use rustc_hash::{FxHashMap, FxHashSet, FxHasher};

use crabgresql_binder::{AggFn, BoundAggregate};
use crabgresql_types::{Numeric, PgType, Value, float};

use crate::ExecError;
use crate::eval::{compare_values, compare_values_for_aggregate};

/// The non-NULL input values already accepted by one `DISTINCT` aggregate in
/// one group. It uses the same type-aware equality and compatible hash as
/// grouping, so duplicate elimination matches PostgreSQL's value semantics.
pub struct DistinctValues {
    ty: PgType,
    kind: Kind,
}

/// How one `DISTINCT` aggregate remembers what it has seen. The variant is
/// picked once from the input type, so the per-row path holds no type dispatch
/// beyond one `match` over three arms.
enum Kind {
    /// Types whose [`keys_equal`] is the comparison of a single raw field that
    /// fits in a `u64` losslessly (see [`scalar_code`]). Storing the code
    /// instead of the value costs no `Value` clone and no per-bucket `Vec`.
    Scalar {
        seen: FxHashSet<u64>,
        odd: Vec<Value>,
    },
    /// `text`/`varchar`/`name`/`bpchar`: equality is bytewise, since every
    /// collation this engine supports is deterministic (see
    /// `crabgresql_types::collation`). A hit allocates nothing — the lookup
    /// borrows `&str` — and a miss allocates one `Box<str>` per distinct value.
    /// `bpchar` is stored with trailing blanks trimmed, as its equality ignores
    /// them.
    Text {
        seen: FxHashSet<Box<str>>,
        odd: Vec<Value>,
    },
    /// Everything else — `numeric` (equal across display scales), `uuid`,
    /// `bytea`, `jsonb`, enums, and the types that hash to nothing at all.
    /// Buckets of values narrowed by [`hash_key`] and decided by [`keys_equal`].
    Generic(FxHashMap<u64, Vec<Value>>),
}

impl DistinctValues {
    pub fn new(ty: PgType) -> Self {
        let kind = match ty {
            PgType::Text | PgType::Varchar | PgType::Name | PgType::Bpchar => Kind::Text {
                seen: FxHashSet::default(),
                odd: Vec::new(),
            },
            _ if scalar_coded(ty) => Kind::Scalar {
                seen: FxHashSet::default(),
                odd: Vec::new(),
            },
            _ => Kind::Generic(FxHashMap::default()),
        };
        Self { ty, kind }
    }

    /// Record `value` if it has not appeared before, returning whether the
    /// caller should pass it to the aggregate accumulator.
    ///
    /// `value` is never NULL: [`feed`] drops NULL inputs before any aggregate
    /// sees them, so no variant here has to encode one.
    pub fn insert(&mut self, value: &Value) -> bool {
        debug_assert!(!matches!(value, Value::Null), "DISTINCT saw a NULL input");
        match &mut self.kind {
            Kind::Scalar { seen, odd } => match scalar_code(self.ty, value) {
                Some(code) => seen.insert(code),
                None => insert_odd(self.ty, odd, value),
            },
            Kind::Text { seen, odd } => match text_key(self.ty, value) {
                Some(key) => {
                    if seen.contains(key) {
                        false
                    } else {
                        seen.insert(key.into());
                        true
                    }
                }
                None => insert_odd(self.ty, odd, value),
            },
            Kind::Generic(buckets) => {
                let tys = [self.ty];
                let values = std::slice::from_ref(value);
                let bucket = buckets.entry(hash_key(&tys, values)).or_default();
                if bucket
                    .iter()
                    .any(|seen| keys_equal(&tys, std::slice::from_ref(seen), values))
                {
                    false
                } else {
                    bucket.push(value.clone());
                    true
                }
            }
        }
    }
}

/// The escape hatch for a value whose variant does not match the type the
/// binder promised. It cannot be dropped (that would undercount) and it cannot
/// be admitted blindly (that would overcount), so it goes to a list compared
/// with the general equality. In practice the list stays empty.
fn insert_odd(ty: PgType, odd: &mut Vec<Value>, value: &Value) -> bool {
    debug_assert!(false, "DISTINCT over {ty:?} got {value:?}");
    if odd.iter().any(|seen| value_eq(ty, seen, value)) {
        false
    } else {
        odd.push(value.clone());
        true
    }
}

/// Whether [`scalar_code`] has an encoding for `ty`. Kept beside it so the two
/// cannot drift: a type listed here without an arm there sends every value to
/// the `odd` list.
///
/// Enums are deliberately absent even though their equality *is* a raw-field
/// compare. A type here promises an encoding for every value that can arrive
/// under it, and a `PgType::User(oid)` cannot make that promise: nothing in the
/// type itself says the value is the `Value::Enum` its equality expects.
pub(crate) fn scalar_coded(ty: PgType) -> bool {
    matches!(
        ty,
        PgType::Bool
            | PgType::Char
            | PgType::Int2
            | PgType::Int4
            | PgType::Int8
            | PgType::Float4
            | PgType::Float8
            | PgType::Oid
            | PgType::Xid
            | PgType::Xid8
            | PgType::PgLsn
            | PgType::Money
            | PgType::Date
            | PgType::Time
            | PgType::Timestamp
            | PgType::TimestampTz
            | PgType::Tid
            | PgType::Reg(_)
    )
}

/// Pack a value into a `u64` that is equal for exactly the values [`keys_equal`]
/// calls equal — an *injective* encoding, unlike [`hash_key`], which may collide.
/// That is why only types whose equality is one raw-field comparison appear
/// here: `numeric` (`1.0` = `1.00`) and `interval` do not, and never can.
///
/// `None` means the value is not of the variant `ty` promises; the caller falls
/// back to the general equality rather than guessing.
pub(crate) fn scalar_code(ty: PgType, v: &Value) -> Option<u64> {
    Some(match (ty, v) {
        (PgType::Bool, Value::Bool(b)) => *b as u64,
        // `"char"` orders unsigned, and equality is the same either way.
        (PgType::Char, Value::Char(c)) => *c as u64,
        (PgType::Int2, Value::Int2(n)) => *n as i64 as u64,
        (PgType::Int4, Value::Int4(n)) => *n as i64 as u64,
        (PgType::Int8, Value::Int8(n)) => *n as u64,
        // Canonicalize in the value's own width before taking the bits, so
        // every NaN and both zeros land on one code (`-0.0f32 as f64` is
        // still `-0.0`, so widening first would not do it).
        (PgType::Float4, Value::Float4(x)) => canonical_f32(*x).to_bits() as u64,
        (PgType::Float8, Value::Float8(x)) => canonical_f64(*x).to_bits(),
        (PgType::Oid, Value::Oid(o)) => *o as u64,
        (PgType::Xid, Value::Xid(x)) => *x as u64,
        (PgType::Xid8, Value::Xid8(x)) => *x,
        (PgType::PgLsn, Value::PgLsn(x)) => *x,
        (PgType::Money, Value::Money(m)) => *m as u64,
        // ±infinity are the plain `i32`/`i64` sentinels, so they encode as
        // themselves.
        (PgType::Date, Value::Date(d)) => *d as i64 as u64,
        (PgType::Time, Value::Time(t)) => *t as u64,
        (PgType::Timestamp, Value::Timestamp(t)) => *t as u64,
        (PgType::TimestampTz, Value::TimestampTz(t)) => *t as u64,
        (PgType::Tid, Value::Tid { block, offset }) => (*block as u64) << 16 | *offset as u64,
        // A `reg*` compares by OID alone: the rendered name is display only.
        (PgType::Reg(_), Value::Reg(r)) => r.oid as u64,
        _ => return None,
    })
}

/// The bytes that decide a string value's identity: the string itself, minus
/// the trailing blanks `bpchar` equality ignores.
pub(crate) fn text_key(ty: PgType, v: &Value) -> Option<&str> {
    let Value::Text(s) = v else { return None };
    Some(match ty {
        PgType::Bpchar => s.trim_end_matches(' '),
        _ => s,
    })
}

/// The running state of one aggregate over one group.
pub struct Accumulator {
    state: AggState,
}

enum AggState {
    /// `count(*)` and `count(expr)`: a running row/non-null count.
    Count(i64),
    /// `min`/`max`: the running extreme (`None` until the first non-null input),
    /// and the type/collation/direction used to compare candidates.
    Extreme {
        ty: PgType,
        collation: u32,
        want_max: bool,
        cur: Option<Value>,
    },
    /// `sum(int2|int4)` → `bigint`.
    SumI64(Option<i64>),
    /// `sum(float4)` → `float4`.
    SumF4(Option<f32>),
    /// `sum(float8)` → `float8`.
    SumF8(Option<f64>),
    /// `sum(int8)` → `numeric`, accumulated in a register rather than a
    /// `Numeric`, whose coefficient is a heap `Vec`: one group-by group per
    /// distinct key means one live allocation per group and two malloc/free
    /// per row. Promotes to `SumNumeric` if it ever overflows.
    SumI128(Option<i128>),
    /// `sum(numeric)` → `numeric`. Also where an overflowing
    /// [`AggState::SumI128`] lands, which is why it is still unbounded.
    SumNumeric(Option<Numeric>),
    /// `avg(int2|int4|int8)` → `numeric`: running sum and count, in registers.
    /// See [`AggState::SumI128`] for why this is not a `Numeric`.
    AvgI128 { sum: i128, count: i64 },
    /// `avg(numeric)` → `numeric`: running sum and count.
    AvgNumeric { sum: Numeric, count: i64 },
    /// `avg(float4|float8)` → `float8`: running sum and count.
    AvgFloat { sum: f64, count: i64 },
    /// `string_agg(value, delimiter)` → `text`: the running concatenation, or
    /// `None` until the first non-null value (an empty group finalizes to NULL).
    StringAgg { cur: Option<String> },
}

// A group-by group holds one accumulator per aggregate, and a query like
// ClickBench's `GROUP BY WatchID, ClientIP` has as many groups as rows — so this
// size is multiplied by tens of millions. `Extreme`'s `Option<Value>` is what
// sets it; the `i128` states only raise the alignment.
const _: () = assert!(std::mem::size_of::<AggState>() <= 80);

impl Accumulator {
    /// A fresh accumulator for `agg`, seeded to its empty-group state.
    pub fn new(agg: &BoundAggregate) -> Accumulator {
        let state = match agg.func {
            AggFn::Count => AggState::Count(0),
            AggFn::Min => AggState::Extreme {
                ty: agg.input_ty,
                collation: agg.collation,
                want_max: false,
                cur: None,
            },
            AggFn::Max => AggState::Extreme {
                ty: agg.input_ty,
                collation: agg.collation,
                want_max: true,
                cur: None,
            },
            AggFn::Sum => match agg.input_ty {
                PgType::Int2 | PgType::Int4 => AggState::SumI64(None),
                PgType::Int8 => AggState::SumI128(None),
                PgType::Numeric => AggState::SumNumeric(None),
                PgType::Float4 => AggState::SumF4(None),
                PgType::Float8 => AggState::SumF8(None),
                other => unreachable!("binder rejected sum({other:?})"),
            },
            AggFn::Avg => match agg.input_ty {
                PgType::Float4 | PgType::Float8 => AggState::AvgFloat { sum: 0.0, count: 0 },
                PgType::Int2 | PgType::Int4 | PgType::Int8 => {
                    AggState::AvgI128 { sum: 0, count: 0 }
                }
                _ => AggState::AvgNumeric {
                    sum: Numeric::from_i128(0),
                    count: 0,
                },
            },
            AggFn::StringAgg => AggState::StringAgg { cur: None },
        };
        Accumulator { state }
    }

    /// Count one row for `count(*)` (which takes no argument and skips no rows).
    /// A no-op for every other aggregate.
    pub fn count_row(&mut self) {
        if let AggState::Count(n) = &mut self.state {
            *n += 1;
        }
    }

    /// Fold one row's argument values into the running state. `values[0]` is the
    /// value (already known non-null); `string_agg` also reads `values[1]` as the
    /// per-row delimiter.
    pub fn accumulate(&mut self, values: &[Value]) -> Result<(), ExecError> {
        // Set by the register states when their `i128` runs out of room. The
        // switch happens after the borrow below ends.
        let mut promote = None;
        match &mut self.state {
            AggState::Count(n) => *n += 1,
            AggState::Extreme {
                ty,
                collation,
                want_max,
                cur,
            } => {
                let v = values[0].clone();
                let replace = match cur {
                    None => true,
                    Some(c) => {
                        // Not `compare_values_collated`: `min`/`max` on `oidvector`
                        // compare element-wise while ORDER BY compares the element
                        // count first, as in PG.
                        let ord = compare_values_for_aggregate(*ty, &v, c, *collation);
                        if *want_max {
                            ord == Ordering::Greater
                        } else {
                            ord == Ordering::Less
                        }
                    }
                };
                if replace {
                    *cur = Some(v);
                }
            }
            AggState::SumI64(acc) => {
                let x = as_i64(&values[0]);
                let next = match acc {
                    None => x,
                    Some(a) => a.checked_add(x).ok_or_else(bigint_out_of_range)?,
                };
                *acc = Some(next);
            }
            AggState::SumF4(acc) => {
                let x = as_f32(&values[0]);
                let next = match *acc {
                    None => x,
                    Some(a) => float::f4_add(a, x).map_err(float_error)?,
                };
                *acc = Some(next);
            }
            AggState::SumF8(acc) => {
                let x = as_f64(&values[0]);
                let next = match *acc {
                    None => x,
                    Some(a) => float::f8_add(a, x).map_err(float_error)?,
                };
                *acc = Some(next);
            }
            AggState::SumI128(acc) => {
                let x = as_i128(&values[0]);
                match acc {
                    None => *acc = Some(x),
                    // Unlike `AvgI128` there is no count to cap the row number
                    // here, so the overflow is only *physically* unreachable
                    // (2^64 bigints is ~147 exabytes). Promote rather than raise:
                    // PostgreSQL's `sum(bigint)` is numeric and never overflows.
                    Some(a) => match a.checked_add(x) {
                        Some(next) => *a = next,
                        None => promote = Some((*a, x)),
                    },
                }
            }
            AggState::SumNumeric(acc) => {
                let x = as_numeric(&values[0]);
                *acc = Some(match acc {
                    None => x,
                    Some(a) => a.add(&x),
                });
            }
            AggState::AvgI128 { sum, count } => {
                // No `checked_add`: `count` counts the same values `sum` adds, so
                // it caps them at `i64::MAX`, and (2^63-1)·2^63 < i128::MAX means
                // `count` overflows first. `sum` cannot be the one to run out.
                *sum += as_i128(&values[0]);
                *count += 1;
            }
            AggState::AvgNumeric { sum, count } => {
                *sum = sum.add(&as_numeric(&values[0]));
                *count += 1;
            }
            AggState::AvgFloat { sum, count } => {
                *sum = float::f8_add(*sum, as_f64(&values[0])).map_err(float_error)?;
                *count += 1;
            }
            AggState::StringAgg { cur } => {
                let value = match &values[0] {
                    Value::Text(s) => s.as_str(),
                    other => unreachable!("binder rejected string_agg({other:?}, _)"),
                };
                // A NULL (or absent) delimiter contributes nothing between values.
                let delim = match values.get(1) {
                    None | Some(Value::Null) => "",
                    Some(Value::Text(s)) => s.as_str(),
                    other => unreachable!("binder rejected string_agg(_, {other:?})"),
                };
                match cur {
                    None => *cur = Some(value.to_string()),
                    Some(s) => {
                        s.push_str(delim);
                        s.push_str(value);
                    }
                }
            }
        }
        // A register sum is exactly `Numeric::from_i128` of itself — `from_i128`
        // fixes `dscale` at 0 and `normalize` leaves one canonical form per value
        // — so resuming in `Numeric` from here is the same running sum
        // `SumNumeric` would be holding, and finalize is unchanged.
        if let Some((sum, x)) = promote {
            self.state =
                AggState::SumNumeric(Some(Numeric::from_i128(sum).add(&Numeric::from_i128(x))));
        }
        Ok(())
    }

    /// The aggregate value for everything accumulated so far. A typed NULL for
    /// an empty group, except `count`, which is `0`.
    ///
    /// Takes `&self` rather than consuming: a *window* aggregate over the default
    /// frame is a running total, so its accumulator is read once per row and must
    /// survive. The three states that own a heap value clone it here; the rest
    /// are copies or build a fresh value anyway.
    pub fn finalize(&self) -> Result<Value, ExecError> {
        Ok(match &self.state {
            AggState::Count(n) => Value::Int8(*n),
            AggState::Extreme { cur, .. } => cur.clone().unwrap_or(Value::Null),
            AggState::SumI64(acc) => acc.map(Value::Int8).unwrap_or(Value::Null),
            AggState::SumF4(acc) => acc.map(Value::Float4).unwrap_or(Value::Null),
            AggState::SumF8(acc) => acc.map(Value::Float8).unwrap_or(Value::Null),
            AggState::SumI128(acc) => acc
                .map(|n| Value::Numeric(Numeric::from_i128(n)))
                .unwrap_or(Value::Null),
            AggState::SumNumeric(acc) => acc.clone().map(Value::Numeric).unwrap_or(Value::Null),
            AggState::AvgI128 { sum, count } => {
                if *count == 0 {
                    Value::Null
                } else {
                    avg_quotient(&Numeric::from_i128(*sum), *count)?
                }
            }
            AggState::AvgNumeric { sum, count } => {
                if *count == 0 {
                    Value::Null
                } else {
                    avg_quotient(sum, *count)?
                }
            }
            AggState::AvgFloat { sum, count } => {
                if *count == 0 {
                    Value::Null
                } else {
                    Value::Float8(float::f8_div(*sum, *count as f64).map_err(float_error)?)
                }
            }
            AggState::StringAgg { cur } => cur.clone().map(Value::Text).unwrap_or(Value::Null),
        })
    }
}

/// Fold one input row into `acc`, applying the rules every aggregate shares:
/// `count(*)` (no argument expression) counts the row unconditionally, every
/// other aggregate skips a row whose first argument is NULL, and a `DISTINCT`
/// aggregate skips a value `seen` already holds.
///
/// `values` must already hold `agg.args.len()` evaluated arguments. Shared by the
/// grouped and windowed drivers so the two cannot drift on NULL handling; the
/// windowed one passes `seen: None`, since PG does not implement `DISTINCT` for
/// window functions.
pub fn feed(
    acc: &mut Accumulator,
    agg: &BoundAggregate,
    values: &[Value],
    seen: Option<&mut DistinctValues>,
) -> Result<(), ExecError> {
    if agg.args.is_empty() {
        acc.count_row();
        return Ok(());
    }
    if matches!(values[0], Value::Null) {
        return Ok(());
    }
    if !seen.is_none_or(|seen| seen.insert(&values[0])) {
        return Ok(());
    }
    acc.accumulate(values)
}

/// A hash of a group key that is *consistent with [`keys_equal`]*: two keys that
/// group together always hash equal (collisions between distinct keys are fine —
/// the caller resolves them with `keys_equal`). This lets the aggregate node find
/// a row's group in O(1) instead of scanning every group.
///
/// Floats canonicalize `-0.0`→`0.0` and every `NaN` to one bit pattern (matching
/// PG's grouping equality); numeric hashes via its `f64` value (equal numerics of
/// different scale share it). Types whose equality is not a raw-field comparison
/// (`timetz`, `interval`, `inet`, `cidr`) contribute nothing and land in a shared
/// bucket that `keys_equal` then disambiguates.
///
/// The consistency in the first paragraph is a **correctness** requirement, not
/// a performance one, because `crate::unique` enforces `UNIQUE` through these
/// buckets. Splitting one arm so that two `keys_equal`-equal values hash apart
/// puts them in different buckets, and a duplicate key is then admitted with no
/// error at all — the failure the build-time check avoids by sorting instead
/// (see `find_duplicate` in the server crate, which argues a hash would be a
/// second definition of key equality). It is not one here only because the
/// buckets never decide: they narrow the candidates, and `keys_equal` answers.
/// Widening this function is safe; making it finer than `keys_equal` is not.
///
/// The hash value itself is never persisted — it only keys in-process maps, and
/// no two runs have to agree on it — so the *function* is free to change (a
/// faster hasher, a new arm for a type that used to contribute nothing). What
/// cannot change is the consistency above.
pub fn hash_key<V: Borrow<Value>>(tys: &[PgType], values: &[V]) -> u64 {
    let mut h = FxHasher::default();
    for (ty, v) in tys.iter().zip(values) {
        let v = v.borrow();
        if matches!(v, Value::Null) {
            0u8.hash(&mut h);
            continue;
        }
        1u8.hash(&mut h);
        match ty {
            PgType::Bool => {
                if let Value::Bool(b) = v {
                    b.hash(&mut h);
                }
            }
            PgType::Char => {
                if let Value::Char(c) = v {
                    c.hash(&mut h);
                }
            }
            PgType::Int2 | PgType::Int4 | PgType::Int8 => as_i64(v).hash(&mut h),
            PgType::Float4 | PgType::Float8 => canonical_f64(as_f64(v)).to_bits().hash(&mut h),
            PgType::Numeric => canonical_f64(as_numeric(v).to_f64()).to_bits().hash(&mut h),
            PgType::Text | PgType::Varchar | PgType::Name => text_of(v).hash(&mut h),
            // bpchar ignores trailing blanks, as `compare_values` does.
            PgType::Bpchar => text_of(v).trim_end_matches(' ').hash(&mut h),
            PgType::Bytea => {
                if let Value::Bytea(b) = v {
                    b.hash(&mut h);
                }
            }
            PgType::Date => {
                if let Value::Date(d) = v {
                    d.hash(&mut h);
                }
            }
            PgType::Time => {
                if let Value::Time(t) = v {
                    t.hash(&mut h);
                }
            }
            PgType::Timestamp => {
                if let Value::Timestamp(t) = v {
                    t.hash(&mut h);
                }
            }
            PgType::TimestampTz => {
                if let Value::TimestampTz(t) = v {
                    t.hash(&mut h);
                }
            }
            PgType::Uuid => {
                if let Value::Uuid(u) = v {
                    u.hash(&mut h);
                }
            }
            // money/oid/macaddr compare by their raw i64/u32/byte representation,
            // so hashing that representation agrees with `keys_equal`.
            PgType::Money => {
                if let Value::Money(m) = v {
                    m.hash(&mut h);
                }
            }
            PgType::Oid => {
                if let Value::Oid(o) = v {
                    o.hash(&mut h);
                }
            }
            PgType::Tid => {
                if let Value::Tid { block, offset } = v {
                    (block, offset).hash(&mut h);
                }
            }
            PgType::Xid => {
                if let Value::Xid(x) = v {
                    x.hash(&mut h);
                }
            }
            PgType::Xid8 => {
                if let Value::Xid8(x) = v {
                    x.hash(&mut h);
                }
            }
            PgType::PgLsn => {
                if let Value::PgLsn(x) = v {
                    x.hash(&mut h);
                }
            }
            // Only the OID: equality ignores the rendered name, so hashing it
            // would put two equal values in different buckets.
            PgType::Reg(_) => {
                if let Value::Reg(r) = v {
                    r.oid.hash(&mut h);
                }
            }
            PgType::Macaddr => {
                if let Value::Macaddr(b) = v {
                    b.hash(&mut h);
                }
            }
            PgType::Macaddr8 => {
                if let Value::Macaddr8(b) = v {
                    b.hash(&mut h);
                }
            }
            // jsonb equality is structural on its canonical tree, so hashing that
            // tree agrees with `keys_equal` (which uses `compare_values`). Equal
            // numbers of different display scale hash equal (numeric `Hash`).
            PgType::Jsonb => {
                if let Value::Jsonb(j) = v {
                    j.hash(&mut h);
                }
            }
            // `tsvector` is canonicalized on input, so hashing the parsed
            // structure agrees with `keys_equal`. `tsquery` contributes nothing
            // (see `PgType::hashes_distinctly`): its equality ignores a leaf's
            // prefix flag and weight mask, which a structural hash cannot.
            PgType::Tsvector => {
                if let Value::Tsvector(t) = v {
                    t.hash(&mut h);
                }
            }
            // A vector's equality is element-wise over `oid`/`int2`, both of
            // which hash by their raw representation. The length is folded in
            // so `'1 2'` and `'12'` cannot collide by concatenation.
            PgType::Vector(_) => {
                if let Value::Vector { elems, .. } = v {
                    elems.len().hash(&mut h);
                    for e in elems {
                        match e {
                            Value::Oid(o) => o.hash(&mut h),
                            Value::Int2(i) => i.hash(&mut h),
                            _ => {}
                        }
                    }
                }
            }
            PgType::User(type_oid) => {
                if let Value::Enum {
                    type_oid: value_oid,
                    ordinal,
                    ..
                } = v
                    && value_oid == type_oid
                {
                    type_oid.hash(&mut h);
                    ordinal.hash(&mut h);
                }
            }
            // timetz/interval/inet/cidr/bit/varbit (and anything else): equality
            // is not a raw-field compare, so contribute nothing and rely on
            // `keys_equal`. See `PgType::hashes_distinctly`, which the join
            // planner uses to avoid a hash join on these one-bucket types.
            _ => {}
        }
    }
    h.finish()
}

/// Canonicalize a float for hashing so grouping-equal values hash equal:
/// `-0.0`→`0.0` and every `NaN`→one representative.
fn canonical_f64(x: f64) -> f64 {
    if x.is_nan() {
        f64::NAN
    } else if x == 0.0 {
        0.0
    } else {
        x
    }
}

/// [`canonical_f64`] at `float4` width, for the encoding that takes the bits of
/// the value as stored.
fn canonical_f32(x: f32) -> f32 {
    if x.is_nan() {
        f32::NAN
    } else if x == 0.0 {
        0.0
    } else {
        x
    }
}

fn text_of(v: &Value) -> &str {
    match v {
        Value::Text(s) => s,
        _ => "",
    }
}

/// Whether two group keys are equal for grouping. NULLs group together
/// (`NULL == NULL`), and non-null values compare with the type's total order —
/// the same equality PG's `GROUP BY` uses (so `0.0` and `-0.0`, and two `NaN`s,
/// each group together).
pub fn keys_equal<A: Borrow<Value>, B: Borrow<Value>>(tys: &[PgType], a: &[A], b: &[B]) -> bool {
    a.iter()
        .zip(b)
        .zip(tys)
        .all(|((x, y), ty)| value_eq(*ty, x.borrow(), y.borrow()))
}

/// Whether two values of type `ty` are the same key: the one definition of key
/// equality behind [`keys_equal`], exposed for callers that compare a key held
/// column by column and so cannot hand over a slice.
pub fn value_eq(ty: PgType, a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        (a, b) => compare_values(ty, a, b) == Ordering::Equal,
    }
}

/// `avg`'s one division, shared so the two exact states cannot drift on the
/// display scale: `select_div_scale` reads the magnitude *and* `dscale` of both
/// operands, so the divisor has to stay `Numeric::from_i128(count)`.
fn avg_quotient(sum: &Numeric, count: i64) -> Result<Value, ExecError> {
    // count > 0, so the division never divides by zero.
    let q = sum
        .div(&Numeric::from_i128(count as i128))
        .map_err(|e| ExecError::new(e.sqlstate, e.message).with_detail(e.detail))?;
    Ok(Value::Numeric(q))
}

fn as_i128(v: &Value) -> i128 {
    match v {
        Value::Int2(n) => *n as i128,
        Value::Int4(n) => *n as i128,
        Value::Int8(n) => *n as i128,
        other => unreachable!("integer accumulator got {other:?}"),
    }
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int2(n) => *n as i64,
        Value::Int4(n) => *n as i64,
        Value::Int8(n) => *n,
        other => unreachable!("sum accumulator got {other:?}"),
    }
}

fn as_f32(v: &Value) -> f32 {
    match v {
        Value::Float4(n) => *n,
        other => unreachable!("float4 sum accumulator got {other:?}"),
    }
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Float4(n) => *n as f64,
        Value::Float8(n) => *n,
        other => unreachable!("float accumulator got {other:?}"),
    }
}

fn as_numeric(v: &Value) -> Numeric {
    match v {
        Value::Int2(n) => Numeric::from_i128(*n as i128),
        Value::Int4(n) => Numeric::from_i128(*n as i128),
        Value::Int8(n) => Numeric::from_i128(*n as i128),
        Value::Numeric(n) => n.clone(),
        other => unreachable!("numeric accumulator got {other:?}"),
    }
}

fn bigint_out_of_range() -> ExecError {
    ExecError::new("22003", "bigint out of range")
}

#[cfg(test)]
mod tests {
    use super::{Accumulator, DistinctValues, hash_key, value_eq};
    use crabgresql_binder::{AggFn, BoundAggregate};
    use crabgresql_types::{Numeric, PgType, Value};

    /// Fold `values` through a real accumulator for `func` over `input_ty`, and
    /// render what it finalizes to — so the dispatch in `Accumulator::new` is
    /// under test alongside the arithmetic.
    fn run(func: AggFn, input_ty: PgType, values: &[Value]) -> String {
        let agg = BoundAggregate {
            func,
            distinct: false,
            args: Vec::new(),
            input_ty,
            ret: PgType::Numeric,
            collation: 0,
        };
        let mut acc = Accumulator::new(&agg);
        for v in values {
            acc.accumulate(std::slice::from_ref(v)).expect("accumulate");
        }
        match acc.finalize().expect("finalize") {
            Value::Numeric(n) => n.to_display(),
            Value::Null => "NULL".to_string(),
            other => panic!("expected numeric, got {other:?}"),
        }
    }

    /// A register sum that stopped short of what a `Numeric` held would shift the
    /// quotient's display scale, so it has to carry a total wider than `i64`
    /// exactly. These are PostgreSQL 18.4's answers.
    #[test]
    fn an_integer_sum_wider_than_i64_keeps_its_scale() {
        let max = Value::Int8(i64::MAX);
        let rows = [max.clone(), max.clone(), max];

        assert_eq!(run(AggFn::Sum, PgType::Int8, &rows), "27670116110564327421");
        // That total over 3 divides exactly, and its magnitude drives
        // `select_div_scale` to rscale 0 — no decimal point at all.
        assert_eq!(run(AggFn::Avg, PgType::Int8, &rows), "9223372036854775807");
    }

    /// A sum cancelling to zero must canonicalize the way `Numeric` does, or the
    /// quotient's trailing zeros move.
    #[test]
    fn a_sum_that_cancels_to_zero_averages_at_full_scale() {
        let rows = [Value::Int4(-5), Value::Int4(5)];
        assert_eq!(
            run(AggFn::Avg, PgType::Int4, &rows),
            "0.00000000000000000000"
        );
        assert_eq!(
            run(AggFn::Sum, PgType::Int8, &rows[..1]),
            "-5",
            "a lone negative keeps its sign"
        );
    }

    /// An empty group is NULL, not zero — `count == 0` must short-circuit before
    /// the division.
    #[test]
    fn an_empty_integer_aggregate_is_null() {
        assert_eq!(run(AggFn::Avg, PgType::Int2, &[]), "NULL");
        assert_eq!(run(AggFn::Sum, PgType::Int8, &[]), "NULL");
    }

    /// `avg` of the narrow integer types still divides at full scale, which is
    /// what `select_div_scale` gives for a small quotient.
    #[test]
    fn narrow_integer_averages_divide_at_full_scale() {
        assert_eq!(
            run(AggFn::Avg, PgType::Int2, &[Value::Int2(5), Value::Int2(20)]),
            "12.5000000000000000"
        );
        assert_eq!(
            run(AggFn::Avg, PgType::Int4, &[Value::Int4(1), Value::Int4(2)]),
            "1.5000000000000000"
        );
    }

    #[test]
    fn enum_hash_uses_definition_ordinal() {
        let ty = [PgType::User(16384)];
        let value = |ordinal: u32, label: &str| {
            [Value::Enum {
                type_oid: 16384,
                ordinal,
                label: label.to_string(),
            }]
        };

        assert_ne!(
            hash_key(&ty, &value(0, "red")),
            hash_key(&ty, &value(1, "green"))
        );
        assert_eq!(
            hash_key(&ty, &value(0, "red")),
            hash_key(&ty, &value(0, "red"))
        );
    }

    /// How many of `values` a `DISTINCT` aggregate would accept, and how many
    /// `keys_equal` says are distinct when compared pairwise. The specialized
    /// storage in [`DistinctValues`] is an optimization only if these agree for
    /// every type, so every case below asserts on both.
    fn distinct_counts(ty: PgType, values: &[Value]) -> (usize, usize) {
        let mut seen = DistinctValues::new(ty);
        let accepted = values.iter().filter(|v| seen.insert(v)).count();
        let mut pairwise: Vec<&Value> = Vec::new();
        for v in values {
            if !pairwise.iter().any(|seen| value_eq(ty, seen, v)) {
                pairwise.push(v);
            }
        }
        (accepted, pairwise.len())
    }

    #[track_caller]
    fn assert_distinct_agrees(ty: PgType, values: &[Value], expected: usize) {
        let (accepted, pairwise) = distinct_counts(ty, values);
        assert_eq!(pairwise, expected, "pairwise equality over {ty:?}");
        assert_eq!(accepted, expected, "DISTINCT storage over {ty:?}");
    }

    /// The encoded types have to be *injective*, not merely collision-free like
    /// `hash_key`: two values sharing a code are silently the same value.
    #[test]
    fn distinct_over_encoded_types_matches_pairwise_equality() {
        let text = |s: &str| Value::Text(s.to_string());

        assert_distinct_agrees(
            PgType::Int8,
            &[
                Value::Int8(0),
                Value::Int8(0),
                Value::Int8(-1),
                Value::Int8(i64::MIN),
                Value::Int8(i64::MAX),
            ],
            4,
        );
        // A `date`'s ±infinity are the plain sentinels, and so are a
        // timestamp's — nothing about them needs a code of its own.
        assert_distinct_agrees(
            PgType::Date,
            &[
                Value::Date(0),
                Value::Date(i32::MIN),
                Value::Date(i32::MAX),
                Value::Date(i32::MIN),
            ],
            3,
        );
        // `"char"` compares unsigned, but equality is blind to the sign either
        // way; what matters is that all 256 bytes stay apart.
        assert_distinct_agrees(
            PgType::Char,
            &[Value::Char(b'a'), Value::Char(0xff), Value::Char(b'a')],
            2,
        );
        // Both zeros are one value and all NaNs are one value, at either width.
        assert_distinct_agrees(
            PgType::Float8,
            &[
                Value::Float8(0.0),
                Value::Float8(-0.0),
                Value::Float8(f64::NAN),
                Value::Float8(-f64::NAN),
                Value::Float8(1.0),
            ],
            3,
        );
        assert_distinct_agrees(
            PgType::Float4,
            &[
                Value::Float4(0.0),
                Value::Float4(-0.0),
                Value::Float4(f32::NAN),
                Value::Float4(-f32::NAN),
                Value::Float4(1.0),
            ],
            3,
        );
        assert_distinct_agrees(
            PgType::Tid,
            &[
                Value::Tid {
                    block: 1,
                    offset: 2,
                },
                Value::Tid {
                    block: 1,
                    offset: 2,
                },
                // Not the same address, and a shift-and-or that dropped a bit
                // would say it was.
                Value::Tid {
                    block: 0,
                    offset: 0x0001,
                },
                Value::Tid {
                    block: 0x0001,
                    offset: 0,
                },
            ],
            3,
        );
        // Text is bytewise: case and trailing blanks each make a new value.
        assert_distinct_agrees(
            PgType::Text,
            &[text("a"), text("A"), text("a "), text("a")],
            3,
        );
        // bpchar equality ignores trailing blanks — but only trailing ones.
        assert_distinct_agrees(
            PgType::Bpchar,
            &[text("a"), text("a  "), text("A"), text(" a")],
            3,
        );
    }

    /// The types deliberately left on the general path. If one of them ever
    /// grows an encoding, this is what catches the loss.
    #[test]
    fn distinct_over_unencoded_types_matches_pairwise_equality() {
        let numeric = |s: &str| Value::Numeric(Numeric::parse(s).expect("numeric literal"));

        // Display scale is not identity: `1.0` and `1.00` are one value.
        assert_distinct_agrees(
            PgType::Numeric,
            &[
                numeric("1"),
                numeric("1.0"),
                numeric("1.00"),
                numeric("NaN"),
                numeric("NaN"),
                numeric("2"),
            ],
            3,
        );
        assert_distinct_agrees(
            PgType::Uuid,
            &[
                Value::Uuid([0; 16]),
                Value::Uuid([1; 16]),
                Value::Uuid([0; 16]),
            ],
            2,
        );
        // `interval` contributes nothing to the hash, so every value shares one
        // bucket and `keys_equal` alone separates them — including the
        // canonical-span equality that makes 24 hours and 1 day the same.
        let interval =
            |months, days, usec| Value::Interval(crabgresql_types::Interval { months, days, usec });
        assert_distinct_agrees(
            PgType::Interval,
            &[
                interval(0, 1, 0),
                interval(0, 0, 24 * 3_600_000_000),
                interval(1, 0, 0),
            ],
            2,
        );
    }

    /// An enum is its `(type, ordinal)`: the label is the spelling, and a value
    /// of a different enum type is not this type's value at all.
    #[test]
    fn distinct_over_enums_ignores_the_label() {
        let value = |type_oid, ordinal, label: &str| Value::Enum {
            type_oid,
            ordinal,
            label: label.to_string(),
        };
        assert_distinct_agrees(
            PgType::User(16384),
            &[
                value(16384, 0, "red"),
                value(16384, 0, "red"),
                value(16384, 1, "green"),
            ],
            2,
        );
    }

    /// A user type promises nothing about the *variant* a value arrives in, so
    /// an enum key stays on the general path where `compare_values` decides.
    /// This is the shape a binary-coercible cast used to produce before the
    /// catalog started rejecting one (see `enum_binary_coercible_cast_is_rejected`
    /// in the server's e2e suite); the storage must not assume it away.
    #[test]
    fn distinct_over_a_user_type_tolerates_a_foreign_variant() {
        let enum_value = |ordinal| Value::Enum {
            type_oid: 16384,
            ordinal,
            label: "red".to_string(),
        };
        assert_distinct_agrees(
            PgType::User(16384),
            &[
                Value::Int4(1),
                Value::Int4(1),
                Value::Int4(2),
                enum_value(0),
                enum_value(0),
            ],
            3,
        );
    }
}

fn float_error(e: float::FloatError) -> ExecError {
    ExecError::new(e.sqlstate, e.message)
}
