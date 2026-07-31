//! `pg_lsn`: a WAL log sequence number, printed as two hex halves.
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior — the accepted input syntax, the `X/YYYYYYYY` output, the arithmetic
//! against `numeric`, and the SQLSTATE/message of every rejection. The rules and
//! messages were derived by probing PostgreSQL 18.4 and reading the vendored
//! 19devel corpus (`vendor/postgres/regress/expected/pg_lsn.out`), not by
//! reading PostgreSQL's source.
//!
//! Representation: `Value::PgLsn(u64)` — the same flat 64-bit counter PG stores.
//!
//! **Output format is version-dependent.** PostgreSQL 18 prints each half with
//! no padding (`0/16AE7F8`); 19devel zero-pads the low half to eight hex digits
//! (`0/016AE7F8`). CrabgreSQL advertises `server_version 19.0` and its vendored
//! regress corpus is 19devel, so [`format`] pads. A `pg_lsn` rendered here will
//! therefore not match a PostgreSQL 18 server byte-for-byte.

use crate::Numeric;

// SQLSTATEs (kept as literals; the types crate does not depend on the protocol
// crate — the binder/executor map these to `sqlstate::*`).
const INVALID_TEXT_REPRESENTATION: &str = "22P02";
/// PG reports an out-of-range LSN as `invalid_parameter_value`, not as the
/// `numeric_value_out_of_range` (22003) most types use — verified against
/// PostgreSQL 18.4 with `VERBOSITY = verbose`.
const INVALID_PARAMETER_VALUE: &str = "22023";
const FEATURE_NOT_SUPPORTED: &str = "0A000";

/// Each half is at most eight hex digits, so `'FFFFFFFF/FFFFFFFF'` is the
/// largest input and a ninth digit in either half is a syntax error.
const MAX_HALF_DIGITS: usize = 8;

/// An input or arithmetic error, carrying the SQLSTATE and message PG reports.
#[derive(Clone, Debug, PartialEq)]
pub struct PgLsnError {
    pub sqlstate: &'static str,
    pub message: String,
}

fn invalid_syntax(input: &str) -> PgLsnError {
    PgLsnError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("invalid input syntax for type pg_lsn: \"{input}\""),
    }
}

fn out_of_range() -> PgLsnError {
    PgLsnError {
        sqlstate: INVALID_PARAMETER_VALUE,
        message: "pg_lsn out of range".to_string(),
    }
}

fn cannot_convert(what: &str) -> PgLsnError {
    PgLsnError {
        sqlstate: FEATURE_NOT_SUPPORTED,
        message: format!("cannot convert {what} to pg_lsn"),
    }
}

/// One `1..=8`-digit hex half, case-insensitive. Rejects an empty run, a ninth
/// digit, a sign, and any surrounding whitespace — `pg_lsn` input is exact.
fn half(text: &str) -> Option<u64> {
    if text.is_empty() || text.len() > MAX_HALF_DIGITS {
        return None;
    }
    u64::from_str_radix(text, 16).ok().filter(|_| {
        // `from_str_radix` accepts a leading `+`, which PG does not.
        text.bytes().all(|b| b.is_ascii_hexdigit())
    })
}

/// `pg_lsn_in`: exactly `<hi>/<lo>`, each side 1-8 hex digits and nothing else.
///
/// Deliberately strict, matching PG: a leading space (`' 0/12345678'`), a sign
/// (`'-1/0'`), an empty half (`'ABCD/'`, `'/ABCD'`), a missing slash
/// (`'16AE7F7'`), a second slash (`'0/1/2'`) and a nine-digit half
/// (`'000000000/1'`) are all `22P02`.
pub fn parse(input: &str) -> Result<u64, PgLsnError> {
    let (hi, lo) = input.split_once('/').ok_or_else(|| invalid_syntax(input))?;
    let hi = half(hi).ok_or_else(|| invalid_syntax(input))?;
    let lo = half(lo).ok_or_else(|| invalid_syntax(input))?;
    Ok((hi << 32) | lo)
}

/// `pg_lsn_out`: uppercase hex, the high half unpadded and the low half padded
/// to eight digits. See the module docs on the version dependence here.
pub fn format(v: u64) -> String {
    format!("{:X}/{:08X}", v >> 32, v as u32)
}

/// `pg_lsn - pg_lsn -> numeric`: the exact signed byte distance, which may be
/// negative and does not fit `i64` at the extremes — hence `numeric`.
pub fn sub(a: u64, b: u64) -> Numeric {
    Numeric::from_i128(i128::from(a) - i128::from(b))
}

/// Round `n` to an integer the way PG's `numeric` → LSN arithmetic does (half
/// away from zero), rejecting the specials with the message PG uses for each.
fn integral(n: &Numeric, nan_message: &str) -> Result<i128, PgLsnError> {
    if n.is_nan() {
        return Err(PgLsnError {
            sqlstate: FEATURE_NOT_SUPPORTED,
            message: nan_message.to_string(),
        });
    }
    if n.is_infinite() {
        return Err(cannot_convert("infinity"));
    }
    // `None` here means the magnitude does not fit `i128`, which is far outside
    // the `u64` an LSN can hold — the same rejection either way.
    n.to_i128().ok_or_else(out_of_range)
}

fn from_i128(v: i128) -> Result<u64, PgLsnError> {
    u64::try_from(v).map_err(|_| out_of_range())
}

/// `pg_lsn + numeric -> pg_lsn` (and the commuted `numeric + pg_lsn`).
pub fn add_numeric(lsn: u64, n: &Numeric) -> Result<u64, PgLsnError> {
    from_i128(i128::from(lsn) + integral(n, "cannot add NaN to pg_lsn")?)
}

/// `pg_lsn - numeric -> pg_lsn`.
pub fn sub_numeric(lsn: u64, n: &Numeric) -> Result<u64, PgLsnError> {
    from_i128(i128::from(lsn) - integral(n, "cannot subtract NaN from pg_lsn")?)
}

/// `pg_lsn(numeric) -> pg_lsn`: the explicit conversion function.
pub fn from_numeric(n: &Numeric) -> Result<u64, PgLsnError> {
    from_i128(integral(n, "cannot convert NaN to pg_lsn")?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn num(s: &str) -> Numeric {
        Numeric::parse(s).expect("valid numeric")
    }

    #[test]
    fn roundtrips_both_extremes() -> anyhow::Result<()> {
        assert_eq!(parse("0/0")?, 0);
        assert_eq!(format(0), "0/00000000");
        assert_eq!(parse("FFFFFFFF/FFFFFFFF")?, u64::MAX);
        assert_eq!(format(u64::MAX), "FFFFFFFF/FFFFFFFF");
        // Input is case-insensitive; output is always uppercase, low half padded.
        assert_eq!(parse("abcd/ef")?, parse("ABCD/EF")?);
        assert_eq!(format(parse("abcd/ef")?), "ABCD/000000EF");

        Ok(())
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "",
            "G/0",           // not hex
            "-1/0",          // no sign
            " 0/12345678",   // no surrounding whitespace
            "0/12345678 ",   //
            "ABCD/",         // empty low half
            "/ABCD",         // empty high half
            "16AE7F7",       // no slash
            "0//1",          // empty half between two slashes
            "0/1/2",         // a second slash lands in the low half
            "000000000/1",   // nine digits
            "0/000000001",   //
            "+1/0",          // `from_str_radix` would take this; PG does not
        ] {
            let e = parse(bad).unwrap_err();
            assert_eq!(e.sqlstate, "22P02", "input {bad:?}");
            assert_eq!(
                e.message,
                format!("invalid input syntax for type pg_lsn: \"{bad}\"")
            );
        }
    }

    #[test]
    fn subtracting_two_lsns_is_an_exact_signed_distance() -> anyhow::Result<()> {
        assert_eq!(sub(parse("0/16AE7F7")?, parse("0/16AE7F8")?).to_display(), "-1");
        assert_eq!(sub(parse("0/16AE7F8")?, parse("0/16AE7F7")?).to_display(), "1");
        // The full span does not fit `i64`, which is why the result is numeric.
        assert_eq!(sub(u64::MAX, 0).to_display(), "18446744073709551615");

        Ok(())
    }

    #[test]
    fn numeric_arithmetic_rounds_half_away_from_zero() -> anyhow::Result<()> {
        let base = parse("0/16AE7F7")?;
        assert_eq!(format(add_numeric(base, &num("16"))?), "0/016AE807");
        assert_eq!(format(sub_numeric(base, &num("16"))?), "0/016AE7E7");
        // 1.5 rounds to 2, not to the nearest even.
        assert_eq!(add_numeric(base, &num("1.5"))?, base + 2);
        assert_eq!(add_numeric(base, &num("1.7"))?, base + 2);

        // Round-tripping the whole range is exact.
        assert_eq!(add_numeric(0, &sub(u64::MAX, 0))?, u64::MAX);
        assert_eq!(sub_numeric(u64::MAX, &sub(u64::MAX, 0))?, 0);

        Ok(())
    }

    #[test]
    fn reports_pgs_message_for_each_failure_mode() -> anyhow::Result<()> {
        // Past either end of the u64.
        for e in [
            add_numeric(u64::MAX - 1, &num("2")).unwrap_err(),
            sub_numeric(1, &num("2")).unwrap_err(),
            from_numeric(&num("-1")).unwrap_err(),
            from_numeric(&num("18446744073709551616")).unwrap_err(),
        ] {
            assert_eq!(e.sqlstate, "22023");
            assert_eq!(e.message, "pg_lsn out of range");
        }

        // Each of the three specials names its own operation.
        for (e, message) in [
            (add_numeric(1, &num("NaN")), "cannot add NaN to pg_lsn"),
            (sub_numeric(1, &num("NaN")), "cannot subtract NaN from pg_lsn"),
            (from_numeric(&num("NaN")), "cannot convert NaN to pg_lsn"),
            (add_numeric(1, &num("Infinity")), "cannot convert infinity to pg_lsn"),
        ] {
            let e = e.unwrap_err();
            assert_eq!(e.sqlstate, "0A000", "{message}");
            assert_eq!(e.message, message);
        }

        // The boundary values on the good side of each check.
        assert_eq!(add_numeric(u64::MAX - 1, &num("1"))?, u64::MAX);
        assert_eq!(sub_numeric(1, &num("1"))?, 0);
        assert_eq!(from_numeric(&num("23783416"))?, 23783416);

        Ok(())
    }
}
