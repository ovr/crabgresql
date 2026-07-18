//! `generate_series` over an integer element type.
//!
//! A [`Series`] is a lazy range iterator shared by the FROM-position node
//! ([`crate::TableFunctionSource`]) and the target-list `ProjectSet` node. It
//! covers the `int4` and `int8` overloads; the binder has already coerced every
//! argument to the element type.
//!
//! Clean-room (see AGENTS.md): the observable behavior — a non-positive count
//! yielding no rows, a NULL bound yielding no rows, a zero step erroring
//! `22023`, and the series ending when the next step would overflow the element
//! type — is pinned by the regression corpus, not ported from PG's C source.

use crabgresql_pg_wire::sqlstate;
use crabgresql_types::{PgType, Value};

use crate::ExecError;

/// A half-open integer range `start, start+step, ...` bounded by `stop`
/// (inclusive), yielding `Value::Int4`/`Value::Int8` per the element type.
pub struct Series {
    cur: i64,
    stop: i64,
    step: i64,
    elem: PgType,
    done: bool,
}

impl Series {
    /// Build a series from `generate_series(start, stop [, step])` arguments,
    /// already coerced to `elem` (`Int4` or `Int8`). A NULL bound or step yields
    /// an empty series (PG returns no rows); a zero step is a `22023` error.
    pub fn from_args(elem: PgType, args: &[Value]) -> Result<Series, ExecError> {
        let start = as_i64(&args[0]);
        let stop = as_i64(&args[1]);
        let step = match args.get(2) {
            Some(v) => as_i64(v),
            None => Some(1),
        };
        if step == Some(0) {
            return Err(ExecError::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                "step size cannot equal zero",
            ));
        }
        let (Some(start), Some(stop), Some(step)) = (start, stop, step) else {
            return Ok(Series::empty(elem));
        };
        Ok(Series {
            cur: start,
            stop,
            step,
            elem,
            done: false,
        })
    }

    fn empty(elem: PgType) -> Series {
        Series {
            cur: 0,
            stop: 0,
            step: 1,
            elem,
            done: true,
        }
    }

    /// The next value in the range, or `None` when the range is exhausted.
    pub fn next_value(&mut self) -> Option<Value> {
        if self.done {
            return None;
        }
        let in_range = if self.step > 0 {
            self.cur <= self.stop
        } else {
            self.cur >= self.stop
        };
        if !in_range {
            self.done = true;
            return None;
        }
        let value = self.value(self.cur);
        // Advance; if the next value would overflow the element type, this was
        // the final row (PG ends the series rather than raising an error).
        match self.cur.checked_add(self.step) {
            Some(next) if self.fits(next) => self.cur = next,
            _ => self.done = true,
        }
        Some(value)
    }

    fn value(&self, n: i64) -> Value {
        match self.elem {
            PgType::Int8 => Value::Int8(n),
            // int4 values are held in an i64 accumulator but always fit i32.
            _ => Value::Int4(n as i32),
        }
    }

    fn fits(&self, n: i64) -> bool {
        match self.elem {
            PgType::Int8 => true,
            _ => i32::try_from(n).is_ok(),
        }
    }
}

/// Read an integer argument as `i64`; `None` for NULL (an absent bound).
fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int4(x) => Some(*x as i64),
        Value::Int8(x) => Some(*x),
        _ => None,
    }
}
