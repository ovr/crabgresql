//! Aggregate accumulators: the running state of one aggregate over one group.
//!
//! Type and value rules reproduce PG 14's observable behavior (see AGENTS.md):
//! `count` is `bigint` and never NULL (0 over an empty group); `min`/`max` keep
//! the argument type; `sum` widens small integers to `bigint` and `bigint` to
//! `numeric`; `avg` of any exact type is `numeric` and of floats is `float8`.
//! Every aggregate but `count` ignores NULL inputs and yields NULL over an empty
//! (or all-NULL) group.

use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crabgresql_binder::{AggFn, BoundAggregate};
use crabgresql_types::{Numeric, PgType, Value, float};

use crate::ExecError;
use crate::eval::compare_values;

/// The running state of one aggregate over one group.
pub struct Accumulator {
    state: AggState,
}

enum AggState {
    /// `count(*)` and `count(expr)`: a running row/non-null count.
    Count(i64),
    /// `min`/`max`: the running extreme (`None` until the first non-null input),
    /// and the type/direction used to compare candidates.
    Extreme {
        ty: PgType,
        want_max: bool,
        cur: Option<Value>,
    },
    /// `sum(int2|int4)` → `bigint`.
    SumI64(Option<i64>),
    /// `sum(float4)` → `float4`.
    SumF4(Option<f32>),
    /// `sum(float8)` → `float8`.
    SumF8(Option<f64>),
    /// `sum(int8|numeric)` → `numeric`.
    SumNumeric(Option<Numeric>),
    /// `avg(int2|int4|int8|numeric)` → `numeric`: running sum and count.
    AvgNumeric { sum: Numeric, count: i64 },
    /// `avg(float4|float8)` → `float8`: running sum and count.
    AvgFloat { sum: f64, count: i64 },
}

impl Accumulator {
    /// A fresh accumulator for `agg`, seeded to its empty-group state.
    pub fn new(agg: &BoundAggregate) -> Accumulator {
        let state = match agg.func {
            AggFn::Count => AggState::Count(0),
            AggFn::Min => AggState::Extreme {
                ty: agg.input_ty,
                want_max: false,
                cur: None,
            },
            AggFn::Max => AggState::Extreme {
                ty: agg.input_ty,
                want_max: true,
                cur: None,
            },
            AggFn::Sum => match agg.input_ty {
                PgType::Int2 | PgType::Int4 => AggState::SumI64(None),
                PgType::Int8 | PgType::Numeric => AggState::SumNumeric(None),
                PgType::Float4 => AggState::SumF4(None),
                PgType::Float8 => AggState::SumF8(None),
                other => unreachable!("binder rejected sum({other:?})"),
            },
            AggFn::Avg => match agg.input_ty {
                PgType::Float4 | PgType::Float8 => AggState::AvgFloat { sum: 0.0, count: 0 },
                _ => AggState::AvgNumeric {
                    sum: Numeric::from_i128(0),
                    count: 0,
                },
            },
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

    /// Fold one non-null argument value into the running state.
    pub fn accumulate(&mut self, v: Value) -> Result<(), ExecError> {
        match &mut self.state {
            AggState::Count(n) => *n += 1,
            AggState::Extreme { ty, want_max, cur } => {
                let replace = match cur {
                    None => true,
                    Some(c) => {
                        let ord = compare_values(*ty, &v, c);
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
                let x = as_i64(&v);
                let next = match acc {
                    None => x,
                    Some(a) => a.checked_add(x).ok_or_else(bigint_out_of_range)?,
                };
                *acc = Some(next);
            }
            AggState::SumF4(acc) => {
                let x = as_f32(&v);
                let next = match *acc {
                    None => x,
                    Some(a) => float::f4_add(a, x).map_err(float_error)?,
                };
                *acc = Some(next);
            }
            AggState::SumF8(acc) => {
                let x = as_f64(&v);
                let next = match *acc {
                    None => x,
                    Some(a) => float::f8_add(a, x).map_err(float_error)?,
                };
                *acc = Some(next);
            }
            AggState::SumNumeric(acc) => {
                let x = as_numeric(&v);
                *acc = Some(match acc {
                    None => x,
                    Some(a) => a.add(&x),
                });
            }
            AggState::AvgNumeric { sum, count } => {
                *sum = sum.add(&as_numeric(&v));
                *count += 1;
            }
            AggState::AvgFloat { sum, count } => {
                *sum = float::f8_add(*sum, as_f64(&v)).map_err(float_error)?;
                *count += 1;
            }
        }
        Ok(())
    }

    /// The final aggregate value for the group. A typed NULL for an empty group,
    /// except `count`, which is `0`.
    pub fn finalize(self) -> Result<Value, ExecError> {
        Ok(match self.state {
            AggState::Count(n) => Value::Int8(n),
            AggState::Extreme { cur, .. } => cur.unwrap_or(Value::Null),
            AggState::SumI64(acc) => acc.map(Value::Int8).unwrap_or(Value::Null),
            AggState::SumF4(acc) => acc.map(Value::Float4).unwrap_or(Value::Null),
            AggState::SumF8(acc) => acc.map(Value::Float8).unwrap_or(Value::Null),
            AggState::SumNumeric(acc) => acc.map(Value::Numeric).unwrap_or(Value::Null),
            AggState::AvgNumeric { sum, count } => {
                if count == 0 {
                    Value::Null
                } else {
                    // count > 0, so the division never divides by zero.
                    Value::Numeric(sum.div(&Numeric::from_i128(count as i128)).map_err(
                        |e| ExecError::new(e.sqlstate, e.message).with_detail(e.detail),
                    )?)
                }
            }
            AggState::AvgFloat { sum, count } => {
                if count == 0 {
                    Value::Null
                } else {
                    Value::Float8(float::f8_div(sum, count as f64).map_err(float_error)?)
                }
            }
        })
    }
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
pub fn hash_key(tys: &[PgType], values: &[Value]) -> u64 {
    let mut h = DefaultHasher::new();
    for (ty, v) in tys.iter().zip(values) {
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
            // timetz/interval/inet/cidr (and anything else): equality is not a
            // raw-field compare, so contribute nothing and rely on `keys_equal`.
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
pub fn keys_equal(tys: &[PgType], a: &[Value], b: &[Value]) -> bool {
    a.iter().zip(b).zip(tys).all(|((x, y), ty)| match (x, y) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        _ => compare_values(*ty, x, y) == Ordering::Equal,
    })
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

fn float_error(e: float::FloatError) -> ExecError {
    ExecError::new(e.sqlstate, e.message)
}
