//! The type-shape questions `information_schema` asks about a column: how long
//! it may be, how precise it is, which interval fields it admits.
//!
//! PostgreSQL answers them with seven `information_schema._pg_*` SQL functions,
//! and `information_schema.columns`/`.domains` are defined in terms of those
//! functions — so one implementation has to serve both. These are that
//! implementation: the executor's `_pg_*` functions call them with a value read
//! straight out of `pg_attribute.atttypmod`, and the catalog's view row builders
//! call them after encoding their declared modifier the same way (see
//! `crabgresql_storage_api::pg_typmod`).
//!
//! Clean-room (see AGENTS.md): the rules below were derived by probing a stock
//! PostgreSQL 18.4 over a matrix of types and modifiers — the function bodies
//! are not visible in `pg_proc.prosrc` on that server, and nothing here was read
//! off upstream's SQL.
//!
//! Two conventions in this module are easy to get wrong and are load-bearing:
//!
//! * The **sentinel is `typmod = -1` exactly**, not `typmod < 0`, for every
//!   length and `numeric` question. PostgreSQL does the arithmetic on any other
//!   negative modifier and reports the nonsense that falls out —
//!   `_pg_char_max_length(1043, -5)` is `-9`, not NULL. The datetime family is
//!   the exception: it does test `< 0`.
//! * `numeric`'s precision and scale are cut out of the raw 16-bit halves of
//!   the modifier, *not* through [`Numeric::unpack_typmod`](crate::Numeric::unpack_typmod),
//!   which sign-extends the 11-bit scale field. The two disagree on a negative
//!   scale: `numeric(4,-2)` stores `atttypmod` 264194, and PostgreSQL reports a
//!   scale of **2046** there. That is the answer to reproduce.

use crate::PgType;
use crate::arith::{ArithError, out_of_range};

/// PostgreSQL's "no modifier was declared" value, as `pg_attribute` stores it.
const NO_TYPMOD: i32 = -1;

/// The 4-byte varlena length header the character and `numeric` modifiers are
/// stored with — the same constant `pg_typmod` adds on the way in.
const VARHDRSZ: i32 = 4;

/// Bytes per character at the widest, which is what PostgreSQL multiplies a
/// declared character length by to get an octet length. This build is UTF-8
/// only, so the encoding is never in question.
const MAX_BYTES_PER_CHAR: i32 = 4;

/// What an unbounded character column reports as its octet length: 2^30, the
/// largest value that fits the standard's `cardinal_number` here.
const UNBOUNDED_OCTET_LENGTH: i32 = 1 << 30;

/// The fractional-second precision every datetime type keeps when none was
/// declared.
const DEFAULT_DATETIME_PRECISION: i32 = 6;

/// `_pg_char_max_length(typid, typmod)`: the declared length in characters, or
/// in bits for the two bit-string types. NULL for a type that has no length and
/// for an undeclared modifier.
///
/// `Err` where PostgreSQL raises `22003`: the subtraction below overflows for
/// `i32::MIN`. Unreachable from a catalog row, but it is the observable answer.
pub fn char_max_length(ty: PgType, typmod: i32) -> Result<Option<i32>, ArithError> {
    if typmod == NO_TYPMOD {
        return Ok(None);
    }
    Ok(match ty {
        PgType::Bpchar | PgType::Varchar => Some(
            typmod
                .checked_sub(VARHDRSZ)
                .ok_or_else(|| out_of_range(PgType::Int4))?,
        ),
        // A bit string's modifier is the bit count itself — no header is
        // reserved, so it needs no adjustment.
        PgType::Bit | PgType::Varbit => Some(typmod),
        _ => None,
    })
}

/// `_pg_char_octet_length(typid, typmod)`: the declared length in bytes. Only
/// the three character types answer — `bit` has a length but not one measured
/// in octets — and an undeclared modifier means "as long as the type allows",
/// which PostgreSQL reports as 2^30 rather than NULL.
///
/// `Err` on `22003` for the same reason as [`char_max_length`], plus the
/// multiplication, which overflows well before `i32::MAX`.
pub fn char_octet_length(ty: PgType, typmod: i32) -> Result<Option<i32>, ArithError> {
    if !matches!(ty, PgType::Text | PgType::Bpchar | PgType::Varchar) {
        return Ok(None);
    }
    if typmod == NO_TYPMOD {
        return Ok(Some(UNBOUNDED_OCTET_LENGTH));
    }
    // `text` takes no modifier, so it has no length to scale up: it is NULL here
    // for every modifier but the undeclared one handled above.
    let Some(chars) = char_max_length(ty, typmod)? else {
        return Ok(None);
    };
    Ok(Some(
        chars
            .checked_mul(MAX_BYTES_PER_CHAR)
            .ok_or_else(|| out_of_range(PgType::Int4))?,
    ))
}

/// `_pg_numeric_precision(typid, typmod)`: how many digits the type holds — in
/// *bits* for the binary types (an `integer` reports 32), in decimal digits for
/// `numeric`, which is the one type whose answer comes from the modifier.
pub fn numeric_precision(ty: PgType, typmod: i32) -> Option<i32> {
    match ty {
        PgType::Int2 => Some(16),
        PgType::Int4 => Some(32),
        PgType::Int8 => Some(64),
        PgType::Float4 => Some(24),
        PgType::Float8 => Some(53),
        PgType::Numeric if typmod != NO_TYPMOD => Some((typmod - VARHDRSZ) >> 16 & 0xffff),
        _ => None,
    }
}

/// `_pg_numeric_precision_radix(typid, typmod)`: the base the precision above is
/// counted in — 2 for every binary type, 10 for `numeric`. `numeric` answers 10
/// even with no modifier at all, where its precision is NULL.
pub fn numeric_precision_radix(ty: PgType, _typmod: i32) -> Option<i32> {
    match ty {
        PgType::Int2 | PgType::Int4 | PgType::Int8 | PgType::Float4 | PgType::Float8 => Some(2),
        PgType::Numeric => Some(10),
        _ => None,
    }
}

/// `_pg_numeric_scale(typid, typmod)`: the digits after the point. Zero for the
/// integer types, which hold none; the modifier's low half for `numeric`. The
/// float types are NULL — a binary float has no decimal scale.
pub fn numeric_scale(ty: PgType, typmod: i32) -> Option<i32> {
    match ty {
        PgType::Int2 | PgType::Int4 | PgType::Int8 => Some(0),
        // Raw and unsigned, deliberately: see the module comment on
        // `numeric(4,-2)`.
        PgType::Numeric if typmod != NO_TYPMOD => Some((typmod - VARHDRSZ) & 0xffff),
        _ => None,
    }
}

/// `_pg_datetime_precision(typid, typmod)`: the declared fractional-second
/// precision. `date` has none and reports 0; the rest default to 6.
///
/// This is the one family that treats *every* negative modifier as "undeclared"
/// rather than only `-1`.
pub fn datetime_precision(ty: PgType, typmod: i32) -> Option<i32> {
    match ty {
        PgType::Date => Some(0),
        PgType::Time | PgType::TimeTz | PgType::Timestamp | PgType::TimestampTz => {
            Some(if typmod < 0 {
                DEFAULT_DATETIME_PRECISION
            } else {
                typmod
            })
        }
        // An interval's modifier packs the field range above its precision, and
        // the all-ones precision half is how "none was declared" is spelled
        // inside a modifier that still names a range.
        PgType::Interval => {
            let precision = typmod & 0xffff;
            Some(if typmod < 0 || precision == 0xffff {
                DEFAULT_DATETIME_PRECISION
            } else {
                precision
            })
        }
        _ => None,
    }
}

/// `_pg_interval_type(typid, typmod)`: the interval fields the modifier admits,
/// spelled as the standard does — `YEAR`, `DAY TO SECOND`, `DAY TO SECOND(4)`.
/// NULL for every other type, and for an interval over the full range: an
/// `interval` and an `interval(2)` name no fields, and carry their precision
/// through [`datetime_precision`] alone.
///
/// One divergence, on an input no catalog holds: PostgreSQL renders this through
/// `format_type`, which raises `XX000 invalid INTERVAL typmod` for a range mask
/// it cannot name (`_pg_interval_type(1186, 24)`). This build's `format_type`
/// prints a bare `interval` for such a mask rather than raising, and this
/// follows it by answering NULL.
pub fn interval_type(ty: PgType, typmod: i32) -> Option<String> {
    if ty != PgType::Interval {
        return None;
    }
    let (range, precision) = crate::interval::unpack_typmod(typmod);
    let fields = crate::interval::range_name(range)?;
    let mut spelling = fields.to_ascii_uppercase();
    if let Some(p) = precision {
        spelling.push_str(&format!("({p})"));
    }
    Some(spelling)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every expectation below was read off a PostgreSQL 18.4 server, either by
    /// calling the function directly or by declaring a column and reading its
    /// `atttypmod` back.
    fn max_len(ty: PgType, typmod: i32) -> Option<i32> {
        char_max_length(ty, typmod).expect("no overflow for a modifier a catalog can hold")
    }

    fn octets(ty: PgType, typmod: i32) -> Option<i32> {
        char_octet_length(ty, typmod).expect("no overflow for a modifier a catalog can hold")
    }

    #[test]
    fn character_lengths_strip_the_varlena_header() {
        // `varchar(10)` stores 14.
        assert_eq!(max_len(PgType::Varchar, 14), Some(10));
        assert_eq!(octets(PgType::Varchar, 14), Some(40));
        assert_eq!(max_len(PgType::Bpchar, 9), Some(5));
        assert_eq!(octets(PgType::Bpchar, 9), Some(20));
        // Undeclared: no length, but an octet length all the same.
        assert_eq!(max_len(PgType::Varchar, -1), None);
        assert_eq!(octets(PgType::Varchar, -1), Some(1_073_741_824));
        assert_eq!(max_len(PgType::Text, -1), None);
        assert_eq!(octets(PgType::Text, -1), Some(1_073_741_824));
        // `text` takes no modifier, so any other value leaves both NULL.
        assert_eq!(octets(PgType::Text, 0), None);
    }

    #[test]
    fn bit_strings_report_their_bit_count_and_no_octets() {
        assert_eq!(max_len(PgType::Bit, 3), Some(3));
        assert_eq!(max_len(PgType::Varbit, 8), Some(8));
        assert_eq!(octets(PgType::Bit, 3), None);
        assert_eq!(max_len(PgType::Bit, -1), None);
    }

    #[test]
    fn types_without_a_length_report_none() {
        for ty in [
            PgType::Name,
            PgType::Char,
            PgType::Bytea,
            PgType::Int4,
            PgType::Uuid,
        ] {
            assert_eq!(max_len(ty, 14), None, "{ty:?}");
            assert_eq!(octets(ty, 14), None, "{ty:?}");
        }
    }

    /// The sentinel is `-1` and nothing else: PostgreSQL does the arithmetic on
    /// any other negative modifier rather than calling it undeclared.
    #[test]
    fn only_minus_one_means_undeclared() {
        assert_eq!(max_len(PgType::Varchar, -2), Some(-6));
        assert_eq!(octets(PgType::Varchar, -2), Some(-24));
        assert_eq!(max_len(PgType::Varchar, -5), Some(-9));
        assert_eq!(max_len(PgType::Bit, -5), Some(-5));
        assert_eq!(max_len(PgType::Bpchar, 0), Some(-4));
        assert_eq!(octets(PgType::Bpchar, 0), Some(-16));
        assert_eq!(numeric_precision(PgType::Numeric, -2), Some(65535));
        assert_eq!(numeric_scale(PgType::Numeric, -2), Some(65530));
        assert_eq!(numeric_precision(PgType::Numeric, -100_000), Some(65534));
        assert_eq!(numeric_scale(PgType::Numeric, -100_000), Some(31068));
    }

    #[test]
    fn overflowing_lengths_raise_the_integer_range_error() {
        let e = char_max_length(PgType::Varchar, i32::MIN).expect_err("i32::MIN - 4 overflows");
        assert_eq!(e.message, "integer out of range");
        let e = char_octet_length(PgType::Varchar, i32::MAX).expect_err("i32::MAX * 4 overflows");
        assert_eq!(e.message, "integer out of range");
    }

    #[test]
    fn binary_types_report_precision_in_bits() {
        for (ty, precision) in [
            (PgType::Int2, 16),
            (PgType::Int4, 32),
            (PgType::Int8, 64),
            (PgType::Float4, 24),
            (PgType::Float8, 53),
        ] {
            assert_eq!(numeric_precision(ty, -1), Some(precision), "{ty:?}");
            // The modifier is irrelevant to a type that takes none.
            assert_eq!(numeric_precision(ty, 24), Some(precision), "{ty:?}");
            assert_eq!(numeric_precision_radix(ty, -1), Some(2), "{ty:?}");
        }
        for ty in [PgType::Int2, PgType::Int4, PgType::Int8] {
            assert_eq!(numeric_scale(ty, -1), Some(0), "{ty:?}");
        }
        // A binary float has a precision but no decimal scale.
        assert_eq!(numeric_scale(PgType::Float4, -1), None);
        assert_eq!(numeric_scale(PgType::Float8, -1), None);
    }

    #[test]
    fn numeric_reads_precision_and_scale_out_of_the_modifier() {
        // `numeric(5,2)`, `numeric(3)` and `numeric` as 18.4 stores them.
        assert_eq!(numeric_precision(PgType::Numeric, 327_686), Some(5));
        assert_eq!(numeric_scale(PgType::Numeric, 327_686), Some(2));
        assert_eq!(numeric_precision(PgType::Numeric, 196_612), Some(3));
        assert_eq!(numeric_scale(PgType::Numeric, 196_612), Some(0));
        assert_eq!(numeric_precision(PgType::Numeric, -1), None);
        assert_eq!(numeric_scale(PgType::Numeric, -1), None);
        // The radix is known even when the precision is not.
        assert_eq!(numeric_precision_radix(PgType::Numeric, -1), Some(10));
    }

    /// `numeric(4,-2)`: the scale field is read unsigned, so PostgreSQL reports
    /// 2046 rather than -2. Reproducing that is the point of not going through
    /// `Numeric::unpack_typmod`.
    #[test]
    fn negative_numeric_scale_reads_back_unsigned() {
        assert_eq!(numeric_precision(PgType::Numeric, 264_194), Some(4));
        assert_eq!(numeric_scale(PgType::Numeric, 264_194), Some(2046));
    }

    #[test]
    fn datetime_precision_defaults_to_six() {
        for ty in [
            PgType::Time,
            PgType::TimeTz,
            PgType::Timestamp,
            PgType::TimestampTz,
        ] {
            assert_eq!(datetime_precision(ty, -1), Some(6), "{ty:?}");
            assert_eq!(datetime_precision(ty, 3), Some(3), "{ty:?}");
            // Unlike the length family, every negative value is undeclared
            // here — and a positive one is passed through unclamped.
            assert_eq!(datetime_precision(ty, -5), Some(6), "{ty:?}");
            assert_eq!(datetime_precision(ty, 196_612), Some(196_612), "{ty:?}");
        }
        // A date has no fractional seconds at all, and says so with a 0.
        assert_eq!(datetime_precision(PgType::Date, -1), Some(0));
        assert_eq!(datetime_precision(PgType::Date, 3), Some(0));
        assert_eq!(datetime_precision(PgType::Int4, -1), None);
        assert_eq!(datetime_precision(PgType::Interval, -1), Some(6));
    }

    /// The `atttypmod` 18.4 stores for each spelling of an interval column,
    /// and what the two interval questions answer for it.
    #[test]
    fn interval_reports_its_fields_and_precision() {
        for (typmod, fields, precision) in [
            (-1, None, 6),
            (327_679, Some("YEAR"), 6),
            (196_607, Some("MONTH"), 6),
            (589_823, Some("DAY"), 6),
            (67_174_399, Some("HOUR"), 6),
            (134_283_263, Some("MINUTE"), 6),
            (268_500_991, Some("SECOND"), 6),
            (458_751, Some("YEAR TO MONTH"), 6),
            (67_698_687, Some("DAY TO HOUR"), 6),
            (201_916_415, Some("DAY TO MINUTE"), 6),
            (470_351_871, Some("DAY TO SECOND"), 6),
            (201_392_127, Some("HOUR TO MINUTE"), 6),
            (469_827_583, Some("HOUR TO SECOND"), 6),
            (402_718_719, Some("MINUTE TO SECOND"), 6),
            (268_435_459, Some("SECOND(3)"), 3),
            (470_286_340, Some("DAY TO SECOND(4)"), 4),
            // `interval(2)`: a precision over the full range, which names no
            // fields.
            (2_147_418_114, None, 2),
        ] {
            assert_eq!(
                interval_type(PgType::Interval, typmod).as_deref(),
                fields,
                "typmod {typmod}"
            );
            assert_eq!(
                datetime_precision(PgType::Interval, typmod),
                Some(precision),
                "typmod {typmod}"
            );
        }
        assert_eq!(interval_type(PgType::Timestamp, 470_351_871), None);
        assert_eq!(interval_type(PgType::Text, -1), None);
    }
}
