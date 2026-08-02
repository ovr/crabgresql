//! The session state that value formatting, casting and date/time input depend
//! on.
//!
//! Three things reach down this far. Two GUCs: `extra_float_digits`, which sets
//! float output precision, and `TimeZone`, the display zone `timestamptz` input
//! and output are relative to. And the transaction clock, which is what the
//! relative input specials (`'now'`, `'today'`, `'tomorrow'`, `'yesterday'`)
//! read. They travel together in [`FmtCtx`] rather than as separate parameters
//! so that adding the next one (`DateStyle`, `IntervalStyle`) does not mean
//! touching every call site again.
//!
//! There is deliberately **no `Default` impl**. A missing zone renders as UTC,
//! which is silently wrong rather than loudly wrong, so each context that has no
//! real session behind it must spell out [`FmtCtx::utc`] and thereby stay
//! greppable. The clock follows the same principle one step further: it is an
//! `Option`, and asking for it without one is a loud internal error rather than
//! a plausible-looking instant.

use std::sync::Arc;

use crate::tz::SessionZone;

/// `XX000` — see the note in `timestamp.rs` on why the types crate spells its
/// SQLSTATEs locally rather than depending on the protocol crate.
const INTERNAL_ERROR: &str = "XX000";

/// The instants a statement evaluates against, in our microseconds since the
/// 2000 epoch, UTC.
///
/// Both are stamped by the session, never read from the wall clock down here —
/// that is what makes `now()` stable across a transaction and
/// `statement_timestamp()` stable across a protocol message.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    /// When the current transaction started. Backs `now()`,
    /// `transaction_timestamp()` and the `'now'` input special.
    pub xact_start: i64,
    /// When the current protocol message started. Backs
    /// `statement_timestamp()`.
    pub stmt_start: i64,
}

/// The error a clock-dependent value hits when there is no session behind it.
pub struct ClockError {
    pub sqlstate: &'static str,
    pub message: String,
}

/// `extra_float_digits`, the display `TimeZone` and the transaction clock, as
/// one bag.
///
/// Cheap to clone: the zone is shared behind an `Arc` because the executor's
/// context is cloned into every plan node, and [`Clock`] is two words.
#[derive(Clone)]
pub struct FmtCtx {
    /// `extra_float_digits` — affects float, and therefore geometric, output.
    pub efd: i32,
    /// The session display zone.
    pub zone: Arc<SessionZone>,
    /// The statement's instants, absent in contexts with no session.
    clock: Option<Clock>,
}

impl FmtCtx {
    pub fn new(efd: i32, zone: Arc<SessionZone>, clock: Clock) -> FmtCtx {
        FmtCtx {
            efd,
            zone,
            clock: Some(clock),
        }
    }

    /// A context with the UTC display zone and no clock, for callers with no
    /// session behind them: unit tests, `EXPLAIN` constant rendering, error
    /// DETAIL text. Every use is a place where a real session would be more
    /// faithful — and a place where a `'now'` literal cannot appear, which the
    /// missing clock enforces rather than assumes.
    pub fn utc(efd: i32) -> FmtCtx {
        FmtCtx {
            efd,
            zone: Arc::new(SessionZone::utc()),
            clock: None,
        }
    }

    /// [`FmtCtx::utc`] at PG's default `extra_float_digits` of 1.
    pub fn utc_default() -> FmtCtx {
        FmtCtx::utc(1)
    }

    /// A UTC context with the clock frozen at a chosen instant, so a test can
    /// assert an exact value for `'now'`/`'today'`.
    pub fn utc_at(efd: i32, xact_start: i64, stmt_start: i64) -> FmtCtx {
        FmtCtx {
            efd,
            zone: Arc::new(SessionZone::utc()),
            clock: Some(Clock {
                xact_start,
                stmt_start,
            }),
        }
    }

    /// The same context with a different display zone. Used by the tests that
    /// pin a zone-sensitive answer, and by callers rebuilding a context around
    /// an already-resolved zone.
    pub fn with_zone(&self, zone: Arc<SessionZone>) -> FmtCtx {
        FmtCtx {
            efd: self.efd,
            zone,
            clock: self.clock,
        }
    }

    /// The transaction start instant. Errors when there is no session clock:
    /// reaching this without one is a wiring bug, and an invented instant would
    /// hide it behind a plausible-looking answer.
    pub fn xact_start(&self) -> Result<i64, ClockError> {
        self.clock.map(|c| c.xact_start).ok_or_else(no_clock)
    }

    /// The protocol-message start instant. See [`FmtCtx::xact_start`].
    pub fn stmt_start(&self) -> Result<i64, ClockError> {
        self.clock.map(|c| c.stmt_start).ok_or_else(no_clock)
    }

    /// Whether a session clock is present, for the binder's decision to fold a
    /// clock-dependent constant now or defer it to execution.
    pub fn has_clock(&self) -> bool {
        self.clock.is_some()
    }

    /// The zone's offset (seconds east) "today" — what a value with a
    /// time-of-day but no date takes: the `time -> timetz` cast, `timetz_in`
    /// without a zone token, `timetz AT LOCAL`. PostgreSQL resolves these at
    /// the transaction timestamp, which is why they are stable across a block
    /// but can differ between a summer and a winter one.
    ///
    /// With no session clock there is no "today" to ask about, so this falls
    /// back to the zone's standard-time offset — identical for a fixed-offset
    /// zone, and an hour out for a DST one. Unlike a clock-dependent *value*,
    /// this cannot raise: an output-adjacent conversion has nowhere to put an
    /// error, and being an hour off beats failing.
    pub fn zone_offset_today(&self) -> i32 {
        match self.clock {
            Some(c) => self.zone.offset_at(c.xact_start),
            None => self.zone.standard_offset(),
        }
    }
}

fn no_clock() -> ClockError {
    ClockError {
        sqlstate: INTERNAL_ERROR,
        message: "date/time value evaluated without a transaction clock".to_string(),
    }
}
