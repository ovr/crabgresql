//! `generate_series` over an integer, numeric, or timestamp element type.
//!
//! A [`Series`] is a lazy range iterator shared by the FROM-position node
//! ([`crate::TableFunctionSource`]) and the target-list `ProjectSet` node. It
//! covers every PG overload the engine's types support: `int4`/`int8`,
//! `numeric`, and `timestamp`/`timestamptz` (stepped by an `interval`). The
//! binder has already coerced the bounds to the element type and the step to its
//! type (the element type, or `interval` for the temporal overloads).
//!
//! Clean-room (see AGENTS.md): the observable behavior — a non-positive count
//! yielding no rows, a NULL bound yielding no rows, a zero/infinite/NaN step
//! erroring, integer series ending silently at type overflow while a timestamp
//! series raises `22008` — is pinned by real-psql differential tests, not ported
//! from PG's C source.

use std::cmp::Ordering;

use crabgresql_pg_wire::sqlstate;
use crabgresql_types::{Interval, Numeric, PgType, Value, interval, timestamp};

use crate::ExecError;

/// A lazy range, one variant per element type. Bounds and step are held in the
/// element's native representation; `next_value` advances and yields one `Value`
/// per row until the range is exhausted.
pub enum Series {
    /// `int4`/`int8`: an `i64` accumulator (`elem` selects the output width).
    /// `forward` is the step's sign.
    Int {
        cur: i64,
        stop: i64,
        step: i64,
        forward: bool,
        elem: PgType,
        done: bool,
    },
    /// `numeric`: exact decimal arithmetic; `forward` is the step's sign.
    Numeric {
        cur: Numeric,
        stop: Numeric,
        step: Numeric,
        forward: bool,
        done: bool,
    },
    /// `timestamp`/`timestamptz`: `i64` micros since 2000-01-01 stepped by an
    /// `interval`; `tz` selects the output type, `forward` is the step's sign.
    Timestamp {
        cur: i64,
        stop: i64,
        step: Interval,
        forward: bool,
        tz: bool,
        done: bool,
    },
    /// No rows at all: a NULL bound/step, and the empty cases of the other
    /// set-returning functions that reuse [`Series`]. Carries nothing, since
    /// nothing is yielded and the column type comes from the plan.
    Empty,
    /// A pre-computed sequence of values (e.g. `jsonb_path_query`'s matches),
    /// yielded one per row. Lets a set-returning function that isn't a lazy range
    /// reuse the same [`Series`] plumbing.
    Materialized(std::vec::IntoIter<Value>),
}

impl Series {
    /// Build a series from `generate_series` arguments, already coerced by the
    /// binder. `elem` is the element (output) type. A NULL bound or step yields
    /// an empty series; a zero/infinite/NaN step is an error.
    pub fn from_args(elem: PgType, args: &[Value]) -> Result<Series, ExecError> {
        match elem {
            PgType::Int4 | PgType::Int8 => int_series(elem, args),
            PgType::Numeric => numeric_series(args),
            PgType::Timestamp | PgType::TimestampTz => {
                timestamp_series(elem == PgType::TimestampTz, args)
            }
            other => Err(ExecError::new(
                sqlstate::FEATURE_NOT_SUPPORTED,
                format!("generate_series over {} is not supported", other.name()),
            )),
        }
    }

    /// The next value in the range, or `None` when exhausted. Only the timestamp
    /// arm can error (a step that overflows the timestamp range → `22008`).
    pub fn next_value(&mut self) -> Result<Option<Value>, ExecError> {
        match self {
            Series::Empty => Ok(None),
            Series::Materialized(it) => Ok(it.next()),
            Series::Int {
                cur,
                stop,
                step,
                forward,
                elem,
                done,
            } => {
                if *done {
                    return Ok(None);
                }
                let in_range = if *forward {
                    *cur <= *stop
                } else {
                    *cur >= *stop
                };
                if !in_range {
                    *done = true;
                    return Ok(None);
                }
                let value = if *elem == PgType::Int8 {
                    Value::Int8(*cur)
                } else {
                    // int4 values live in an i64 accumulator but always fit i32.
                    Value::Int4(*cur as i32)
                };
                // If the next value would overflow the element type, this was the
                // final row (PG ends the series rather than raising an error).
                let fits = |n: i64| *elem == PgType::Int8 || i32::try_from(n).is_ok();
                match cur.checked_add(*step) {
                    Some(next) if fits(next) => *cur = next,
                    _ => *done = true,
                }
                Ok(Some(value))
            }
            Series::Numeric {
                cur,
                stop,
                step,
                forward,
                done,
            } => {
                if *done {
                    return Ok(None);
                }
                let in_range = if *forward {
                    cur.cmp(stop) != Ordering::Greater
                } else {
                    cur.cmp(stop) != Ordering::Less
                };
                if !in_range {
                    *done = true;
                    return Ok(None);
                }
                let value = Value::Numeric(cur.clone());
                *cur = cur
                    .add(step)
                    .map_err(|e| ExecError::new(e.sqlstate, e.message))?;
                Ok(Some(value))
            }
            Series::Timestamp {
                cur,
                stop,
                step,
                forward,
                tz,
                done,
            } => {
                if *done {
                    return Ok(None);
                }
                // Infinity sentinels are i64 extremes, so a raw compare orders
                // -inf < finite < +inf correctly.
                let in_range = if *forward {
                    *cur <= *stop
                } else {
                    *cur >= *stop
                };
                if !in_range {
                    *done = true;
                    return Ok(None);
                }
                let value = if *tz {
                    Value::TimestampTz(*cur)
                } else {
                    Value::Timestamp(*cur)
                };
                // Advance eagerly: PG computes the successor before yielding, so
                // an overflow aborts this call (the row is not emitted) and the
                // series reports `22008 "timestamp out of range"`.
                match timestamp::pl_interval(*cur, *step) {
                    Ok(next) => {
                        *cur = next;
                        Ok(Some(value))
                    }
                    Err(e) => {
                        *done = true;
                        Err(ExecError::new(e.sqlstate, e.message))
                    }
                }
            }
        }
    }
}

/// The `int4`/`int8` overload: bounds and step share the element type.
fn int_series(elem: PgType, args: &[Value]) -> Result<Series, ExecError> {
    let start = as_i64(&args[0]);
    let stop = as_i64(&args[1]);
    let step = match args.get(2) {
        Some(v) => as_i64(v),
        None => Some(1),
    };
    // Strict function: a NULL argument yields no rows, before validating the step.
    let (Some(cur), Some(stop), Some(step)) = (start, stop, step) else {
        return Ok(Series::Empty);
    };
    if step == 0 {
        return Err(zero_step());
    }
    Ok(Series::Int {
        cur,
        stop,
        forward: step > 0,
        step,
        elem,
        done: false,
    })
}

/// The `numeric` overload; the default 2-arg step is `1`.
fn numeric_series(args: &[Value]) -> Result<Series, ExecError> {
    let start = as_numeric(&args[0]);
    let stop = as_numeric(&args[1]);
    let step = match args.get(2) {
        Some(v) => as_numeric(v),
        None => Some(Numeric::from_i128(1)),
    };
    // Strict function: a NULL argument yields no rows, before rejecting a NaN,
    // infinite, or zero bound/step (all of which PG validates only on non-NULL
    // input).
    let (Some(cur), Some(stop), Some(step)) = (start, stop, step) else {
        return Ok(Series::Empty);
    };
    reject_nonfinite(&cur, "start value")?;
    reject_nonfinite(&stop, "stop value")?;
    reject_nonfinite(&step, "step size")?;
    let forward = match step.cmp(&Numeric::from_i128(0)) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => return Err(zero_step()),
    };
    Ok(Series::Numeric {
        cur,
        stop,
        step,
        forward,
        done: false,
    })
}

/// The `timestamp`/`timestamptz` overload: `(start, stop, interval)`. Rejects a
/// zero or infinite step; direction is the step's sign under PG's interval order.
fn timestamp_series(tz: bool, args: &[Value]) -> Result<Series, ExecError> {
    let start = as_micros(&args[0]);
    let stop = as_micros(&args[1]);
    let step = match &args[2] {
        Value::Interval(iv) => Some(*iv),
        _ => None,
    };
    let (Some(cur), Some(stop), Some(step)) = (start, stop, step) else {
        return Ok(Series::Empty);
    };
    if !step.is_finite() {
        return Err(ExecError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "step size cannot be infinite",
        ));
    }
    const ZERO: Interval = Interval {
        months: 0,
        days: 0,
        usec: 0,
    };
    let forward = match interval::cmp(step, ZERO) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => return Err(zero_step()),
    };
    Ok(Series::Timestamp {
        cur,
        stop,
        step,
        forward,
        tz,
        done: false,
    })
}

fn zero_step() -> ExecError {
    ExecError::new(
        sqlstate::INVALID_PARAMETER_VALUE,
        "step size cannot equal zero",
    )
}

/// Read an integer argument as `i64`; `None` for NULL (an absent bound).
fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int4(x) => Some(*x as i64),
        Value::Int8(x) => Some(*x),
        _ => None,
    }
}

/// Read a numeric argument; `None` for NULL.
fn as_numeric(v: &Value) -> Option<Numeric> {
    match v {
        Value::Numeric(n) => Some(n.clone()),
        _ => None,
    }
}

/// Reject a NaN or infinite numeric bound/step, reproducing PG's per-role
/// messages, e.g. `"start value cannot be NaN"` / `"step size cannot be
/// infinity"`. (The interval-step overload uses `"infinite"` instead and is
/// handled in [`timestamp_series`].)
fn reject_nonfinite(n: &Numeric, role: &str) -> Result<(), ExecError> {
    if n.is_nan() {
        return Err(ExecError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("{role} cannot be NaN"),
        ));
    }
    if n.is_infinite() {
        return Err(ExecError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("{role} cannot be infinity"),
        ));
    }
    Ok(())
}

/// Read a timestamp/timestamptz argument as its raw `i64` micros; `None` for NULL.
fn as_micros(v: &Value) -> Option<i64> {
    match v {
        Value::Timestamp(x) | Value::TimestampTz(x) => Some(*x),
        _ => None,
    }
}
