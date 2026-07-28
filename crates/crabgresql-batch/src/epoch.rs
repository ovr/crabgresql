//! Converting Arrow's date and timestamp domain to PostgreSQL's.
//!
//! Arrow counts from 1970-01-01, PostgreSQL from 2000-01-01. A [`Batch`] always
//! carries PostgreSQL-domain values, so this conversion happens exactly once,
//! where a batch is built, and nothing above the scan has to know which epoch a
//! column arrived in.
//!
//! Doing it the other way — leaving arrays in Arrow's domain and shifting
//! constants instead — is the tempting optimization and is a trap twice over:
//!
//! * The shift preserves ordering, so a forgotten rebase does not crash or
//!   return garbage. It returns *plausible* rows for a window 30 years away.
//!   `WHERE d >= DATE '2013-07-01'` silently matches nothing.
//! * A `USING parquet` relation reads from two storage leaves — durable Parquet
//!   chunks and a RAM buffer of already-decoded [`Value`](crabgresql_types::Value)s.
//!   The buffer leaf has no Arrow epoch to shift, so the two leaves would
//!   disagree and `GROUP BY d` would report two groups per date.
//!
//! [`Batch`]: crate::Batch

use arrow_array::{Array, Int32Array, Int64Array};

/// Days between the Unix epoch (1970-01-01) and the PostgreSQL epoch
/// (2000-01-01).
pub const PG_UNIX_EPOCH_DAYS: i32 = 10_957;

/// Microseconds between the Unix epoch and the PostgreSQL epoch.
pub const PG_UNIX_EPOCH_MICROS: i64 = 946_684_800_000_000;

/// PostgreSQL's `date` infinity sentinels, which are *not* days and must never
/// be shifted. They are the extremes of the domain, so ordering and equality
/// survive the raw representation — but arithmetic does not, which is why a
/// batch admits `date` as a comparison operand and never as an arithmetic one.
pub const DATE_NEG_INFINITY: i32 = i32::MIN;
pub const DATE_POS_INFINITY: i32 = i32::MAX;

/// PostgreSQL's `timestamp`/`timestamptz` infinity sentinels. Same rules as
/// [`DATE_NEG_INFINITY`].
pub const TIMESTAMP_NEG_INFINITY: i64 = i64::MIN;
pub const TIMESTAMP_POS_INFINITY: i64 = i64::MAX;

/// One Arrow `Date32` value as PostgreSQL epoch days, or `None` if the shift
/// would overflow. Infinities pass through untouched.
#[inline]
pub fn date_to_pg(arrow_days: i32) -> Option<i32> {
    if arrow_days == DATE_NEG_INFINITY || arrow_days == DATE_POS_INFINITY {
        return Some(arrow_days);
    }
    arrow_days.checked_sub(PG_UNIX_EPOCH_DAYS)
}

/// Inverse of [`date_to_pg`].
#[inline]
pub fn date_from_pg(pg_days: i32) -> Option<i32> {
    if pg_days == DATE_NEG_INFINITY || pg_days == DATE_POS_INFINITY {
        return Some(pg_days);
    }
    pg_days.checked_add(PG_UNIX_EPOCH_DAYS)
}

/// One Arrow microsecond timestamp as PostgreSQL epoch microseconds, or `None`
/// on overflow. Infinities pass through untouched.
#[inline]
pub fn timestamp_to_pg(arrow_micros: i64) -> Option<i64> {
    if arrow_micros == TIMESTAMP_NEG_INFINITY || arrow_micros == TIMESTAMP_POS_INFINITY {
        return Some(arrow_micros);
    }
    arrow_micros.checked_sub(PG_UNIX_EPOCH_MICROS)
}

/// Inverse of [`timestamp_to_pg`].
#[inline]
pub fn timestamp_from_pg(pg_micros: i64) -> Option<i64> {
    if pg_micros == TIMESTAMP_NEG_INFINITY || pg_micros == TIMESTAMP_POS_INFINITY {
        return Some(pg_micros);
    }
    pg_micros.checked_add(PG_UNIX_EPOCH_MICROS)
}

/// Rebase a whole `Date32`-valued array into PostgreSQL epoch days.
///
/// Nulls stay null and infinities stay infinite. Returns `None` if any value
/// would overflow the shift, which the caller reports as corrupt data — the same
/// verdict the row decoder reaches for the same value.
pub fn rebase_dates(values: &Int32Array) -> Option<Int32Array> {
    // `unary_opt` visits only valid slots and folds a `None` into a null, so an
    // overflow would otherwise masquerade as a missing date. Comparing null
    // counts is what tells the two apart: the shift is total on every value it
    // accepts, so any new null is an overflow.
    let rebased: Int32Array = values.unary_opt(date_to_pg);
    (rebased.null_count() == values.null_count()).then_some(rebased)
}

/// Rebase a whole microsecond-timestamp array into PostgreSQL epoch
/// microseconds. Same contract as [`rebase_dates`].
pub fn rebase_timestamps(values: &Int64Array) -> Option<Int64Array> {
    let rebased: Int64Array = values.unary_opt(timestamp_to_pg);
    (rebased.null_count() == values.null_count()).then_some(rebased)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pg_epoch_day_count_is_the_gregorian_gap() {
        // 30 years, 1970..2000, with leap days in 1972, 1976, 1980, 1984, 1988,
        // 1992 and 1996 — seven of them, since 2000-01-01 precedes its own.
        assert_eq!(PG_UNIX_EPOCH_DAYS, 30 * 365 + 7);
    }

    #[test]
    fn round_trip_preserves_ordinary_values() {
        for days in [-100_000, -1, 0, 1, 10_957, 15_887, 2_000_000] {
            assert_eq!(date_from_pg(date_to_pg(days).expect("shift")), Some(days));
        }
        for micros in [-1, 0, PG_UNIX_EPOCH_MICROS, 1_700_000_000_000_000] {
            assert_eq!(
                timestamp_from_pg(timestamp_to_pg(micros).expect("shift")),
                Some(micros)
            );
        }
    }

    #[test]
    fn infinities_are_not_shifted() {
        assert_eq!(date_to_pg(DATE_POS_INFINITY), Some(DATE_POS_INFINITY));
        assert_eq!(date_to_pg(DATE_NEG_INFINITY), Some(DATE_NEG_INFINITY));
        assert_eq!(date_from_pg(DATE_POS_INFINITY), Some(DATE_POS_INFINITY));
        assert_eq!(
            timestamp_to_pg(TIMESTAMP_POS_INFINITY),
            Some(TIMESTAMP_POS_INFINITY)
        );
        assert_eq!(
            timestamp_to_pg(TIMESTAMP_NEG_INFINITY),
            Some(TIMESTAMP_NEG_INFINITY)
        );
    }

    /// The sentinels are the extremes of the domain, which is what lets a
    /// comparison kernel run on the raw representation: `-infinity` sorts below
    /// every finite date and `infinity` above every one, in both epochs.
    #[test]
    fn infinities_bound_the_finite_range() {
        let finite = [i32::MIN + 1, -1, 0, 1, i32::MAX - 1];
        for day in finite {
            assert!(DATE_NEG_INFINITY < day);
            assert!(DATE_POS_INFINITY > day);
        }
    }

    #[test]
    fn near_sentinel_values_that_would_overflow_are_refused() {
        // One below `+infinity` is an ordinary (if absurd) date, and shifting it
        // down is fine; one above `-infinity` would underflow.
        assert_eq!(
            date_to_pg(i32::MAX - 1),
            Some(i32::MAX - 1 - PG_UNIX_EPOCH_DAYS)
        );
        assert_eq!(date_to_pg(i32::MIN + 1), None);
        assert_eq!(timestamp_to_pg(i64::MIN + 1), None);
    }

    #[test]
    fn rebasing_an_array_keeps_nulls_and_infinities() {
        let input = Int32Array::from(vec![
            Some(10_957),
            None,
            Some(DATE_POS_INFINITY),
            Some(DATE_NEG_INFINITY),
            Some(0),
        ]);
        let out = rebase_dates(&input).expect("no overflow");
        assert_eq!(out.value(0), 0);
        assert!(out.is_null(1));
        assert_eq!(out.value(2), DATE_POS_INFINITY);
        assert_eq!(out.value(3), DATE_NEG_INFINITY);
        assert_eq!(out.value(4), -PG_UNIX_EPOCH_DAYS);
    }

    #[test]
    fn rebasing_refuses_an_array_that_would_overflow() {
        let input = Int32Array::from(vec![Some(i32::MIN + 1)]);
        assert!(rebase_dates(&input).is_none());
    }

    /// The canary for the whole module: a ClickBench-shaped date predicate.
    /// Comparing a PostgreSQL-domain constant against an un-rebased array
    /// matches nothing, and matches nothing *quietly*.
    #[test]
    fn forgetting_the_rebase_shifts_the_window_by_thirty_years() {
        let arrow_days = 15_887; // 2013-07-01 counted from 1970.
        let pg_days = date_to_pg(arrow_days).expect("shift");
        assert_eq!(pg_days, 4_930);
        assert_ne!(arrow_days, pg_days);
        assert_eq!(arrow_days - pg_days, PG_UNIX_EPOCH_DAYS);
    }
}
