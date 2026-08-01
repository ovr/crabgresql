//! The session state that value formatting and casting depend on.
//!
//! Two GUCs reach down this far: `extra_float_digits`, which sets float output
//! precision, and `TimeZone`, the display zone `timestamptz` input and output
//! are relative to. They travel together in [`FmtCtx`] rather than as separate
//! parameters so that adding the next one (`DateStyle`, `IntervalStyle`) does
//! not mean touching every call site again.
//!
//! There is deliberately **no `Default` impl**. A missing zone renders as UTC,
//! which is silently wrong rather than loudly wrong, so each context that has no
//! real session behind it must spell out [`FmtCtx::utc`] and thereby stay
//! greppable.

use std::sync::Arc;

use crate::tz::SessionZone;

/// `extra_float_digits` and the display `TimeZone`, as one bag.
///
/// Cheap to clone: the zone is shared behind an `Arc` because the executor's
/// context is cloned into every plan node.
#[derive(Clone)]
pub struct FmtCtx {
    /// `extra_float_digits` — affects float, and therefore geometric, output.
    pub efd: i32,
    /// The session display zone.
    pub zone: Arc<SessionZone>,
}

impl FmtCtx {
    pub fn new(efd: i32, zone: Arc<SessionZone>) -> FmtCtx {
        FmtCtx { efd, zone }
    }

    /// A context with the UTC display zone, for callers with no session behind
    /// them: unit tests, `EXPLAIN` constant rendering, error DETAIL text. Every
    /// use is a place where a real session zone would be more faithful.
    pub fn utc(efd: i32) -> FmtCtx {
        FmtCtx {
            efd,
            zone: Arc::new(SessionZone::utc()),
        }
    }

    /// [`FmtCtx::utc`] at PG's default `extra_float_digits` of 1.
    pub fn utc_default() -> FmtCtx {
        FmtCtx::utc(1)
    }
}
