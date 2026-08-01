//! `xid` / `xid8`: transaction identifiers, 32- and 64-bit.
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior — the accepted input spellings, the unsigned decimal output, and the
//! SQLSTATE/message of each rejection. Every rule below was derived by probing
//! PostgreSQL 18.4 directly, not read off its source; the boundary cases are
//! pinned by the tests at the bottom of this file.
//!
//! Representation: `Value::Xid(u32)` and `Value::Xid8(u64)`.
//!
//! The two types differ in more than width. PostgreSQL gives `xid` a hash
//! operator class but deliberately *no* btree one, because transaction ids
//! compare with modular arithmetic — so `xid` has `=` and `<>` but no `<`, no
//! ORDER BY, and no `min`/`max`. `xid8` is a plain unsigned 64-bit counter and
//! gets the full set. That split is enforced in the binder (`is_orderable` vs
//! `has_equality`), not here.

// SQLSTATEs (kept as literals; the types crate does not depend on the protocol
// crate — the binder/executor map these to `sqlstate::*`).
const INVALID_TEXT_REPRESENTATION: &str = "22P02";
const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";

/// The lowest value `xid` accepts once a negative input has wrapped:
/// `(-2147483648i64) as u64`. See [`xid_in`] for why the accepted set has this
/// second, disjoint band at the top of the `u64` range.
const MIN_WRAPPED_XID: u64 = (i32::MIN as i64) as u64;

/// A parse error, carrying the SQLSTATE and message PG reports.
#[derive(Clone, Debug, PartialEq)]
pub struct XidError {
    pub sqlstate: &'static str,
    pub message: String,
}

fn invalid_syntax(input: &str, type_name: &str) -> XidError {
    XidError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("invalid input syntax for type {type_name}: \"{input}\""),
    }
}

fn out_of_range(input: &str, type_name: &str) -> XidError {
    XidError {
        sqlstate: NUMERIC_VALUE_OUT_OF_RANGE,
        message: format!("value \"{input}\" is out of range for type {type_name}"),
    }
}

/// Which way a scan failed, so the caller can attach the right type name.
pub(crate) enum ScanError {
    Syntax,
    Range,
}

/// Read the longest prefix of `s` that C's `strtoul(s, &end, 0)` would convert,
/// returning the value alongside the byte offset C would leave in `end`. This is
/// what PG's `xidin`, `xid8in` and `oidin` are all observed to accept:
///
/// * leading whitespace is skipped, and an optional `+`/`-` sign follows;
/// * `0x`/`0X` introduces hex, a bare leading `0` introduces octal, and
///   anything else is decimal — so `'010'` is 8, `'0x1f'` is 31, and `'08'`
///   converts just the `0`, stopping at the `8`;
/// * the magnitude must fit `u64`, and a negative one is negated *within*
///   `u64`. That wrap is observable: `'-1'::xid8` prints as
///   `18446744073709551615`.
///
/// When no conversion happens at all (`""`, `"-"`, `"abc"`) the offset is `0`,
/// as C leaves `end == nptr`. On overflow the digit run is still consumed in
/// full, so the offset points past it — PG reports those as out of range, not
/// as a syntax error.
///
/// [`crate::vector::vector_in`] needs the offset to reproduce `oidvectorin`'s
/// error text, which quotes the input from wherever the scan stopped.
pub(crate) fn scan_prefix(s: &str) -> (Result<u64, ScanError>, usize) {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let negative = match bytes.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };

    // `0x` is a hex prefix only when a hex digit actually follows; otherwise the
    // `0` converts on its own and the scan stops at the `x`.
    let (radix, mut consumed) = match (bytes.get(i), bytes.get(i + 1), bytes.get(i + 2)) {
        (Some(b'0'), Some(b'x' | b'X'), Some(c)) if c.is_ascii_hexdigit() => {
            i += 2;
            (16u32, 0usize)
        }
        // The leading `0` is itself the first octal digit consumed, which is why
        // a bare `0` is legal while a bare sign is not.
        (Some(b'0'), _, _) => {
            i += 1;
            (8u32, 1usize)
        }
        _ => (10u32, 0usize),
    };

    let mut magnitude: u64 = 0;
    let mut overflowed = false;
    while let Some(digit) = bytes.get(i).and_then(|b| char::from(*b).to_digit(radix)) {
        i += 1;
        consumed += 1;
        match magnitude
            .checked_mul(u64::from(radix))
            .and_then(|m| m.checked_add(u64::from(digit)))
        {
            Some(m) => magnitude = m,
            // Keep eating digits so `end` lands past the whole run, as C does.
            None => overflowed = true,
        }
    }

    if consumed == 0 {
        return (Err(ScanError::Syntax), 0);
    }
    if overflowed {
        return (Err(ScanError::Range), i);
    }
    let value = if negative {
        magnitude.wrapping_neg()
    } else {
        magnitude
    };
    (Ok(value), i)
}

/// [`scan_prefix`] over a whole string: the trimmed input must convert in full,
/// so a trailing character is a syntax error (`'1abc'`, `'08'`, `'0b11'`).
fn scan(input: &str) -> Result<u64, ScanError> {
    let text = input.trim_matches(|c: char| c.is_ascii_whitespace());
    let (result, stop) = scan_prefix(text);
    let value = result?;
    if stop == text.len() {
        Ok(value)
    } else {
        Err(ScanError::Syntax)
    }
}

/// `xidin`: a 32-bit transaction id.
///
/// The scanned `u64` is accepted when it either fits `u32` or sign-extends from
/// `i32` — exactly the values C's `strtoul`-into-`TransactionId` round-trips.
/// So `'-1'` is `4294967295` and `'18446744073709551614'` is `4294967294`
/// (the same wrapped value spelled the other way), while `'4294967296'` and
/// `'-2147483649'` fall in the gap between the two bands and are rejected.
pub fn xid_in(input: &str) -> Result<u32, XidError> {
    match scan(input) {
        Ok(v) if v <= u64::from(u32::MAX) || v >= MIN_WRAPPED_XID => Ok(v as u32),
        Ok(_) | Err(ScanError::Range) => Err(out_of_range(input, "xid")),
        Err(ScanError::Syntax) => Err(invalid_syntax(input, "xid")),
    }
}

/// `xid8in`: a 64-bit transaction id. Every scanned `u64` is in range, so the
/// only rejections are malformed input and a magnitude wider than `u64`.
pub fn xid8_in(input: &str) -> Result<u64, XidError> {
    match scan(input) {
        Ok(v) => Ok(v),
        Err(ScanError::Range) => Err(out_of_range(input, "xid8")),
        Err(ScanError::Syntax) => Err(invalid_syntax(input, "xid8")),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn accepts_strtoul_base_zero_spellings() -> anyhow::Result<()> {
        // Octal, hex (either case), decimal, both signs, surrounding space.
        assert_eq!(xid_in("010")?, 8);
        assert_eq!(xid_in("0")?, 0);
        assert_eq!(xid_in("0x1f")?, 31);
        assert_eq!(xid_in("0X1F")?, 31);
        assert_eq!(xid_in("42")?, 42);
        assert_eq!(xid_in("+42")?, 42);
        assert_eq!(xid_in(" 42 ")?, 42);
        assert_eq!(xid_in("-0")?, 0);
        assert_eq!(xid8_in("010")?, 8);
        assert_eq!(xid8_in("0x1f")?, 31);

        Ok(())
    }

    /// Each of these stops the digit run early, leaving a trailing character —
    /// which PG treats as a syntax error rather than a partial parse.
    #[test]
    fn rejects_malformed() {
        for bad in ["", "asdf", "1abc", "08", "0b11", "0o17", "0x", "-", "+"] {
            for (name, e) in [
                ("xid", xid_in(bad).unwrap_err()),
                ("xid8", xid8_in(bad).unwrap_err()),
            ] {
                assert_eq!(e.sqlstate, "22P02", "input {bad:?} as {name}");
                assert_eq!(
                    e.message,
                    format!("invalid input syntax for type {name}: \"{bad}\"")
                );
            }
        }
    }

    /// `xid` takes anything that fits `int4` or `uint4`, negatives wrapping.
    /// The upper band is the unsigned-64 spelling of those same negatives, so
    /// the two spellings must agree.
    #[test]
    fn xid_accepts_i32_or_u32_and_wraps_negatives() -> anyhow::Result<()> {
        assert_eq!(xid_in("-1")?, u32::MAX);
        assert_eq!(xid_in("0xffffffff")?, u32::MAX);
        assert_eq!(xid_in("18446744073709551615")?, u32::MAX);
        assert_eq!(xid_in("-2")?, 4294967294);
        assert_eq!(xid_in("18446744073709551614")?, 4294967294);
        assert_eq!(xid_in("-2147483648")?, 2147483648);

        // The gap between the two bands, and past `u64` entirely.
        for bad in [
            "4294967296",
            "-2147483649",
            "-4294967295",
            "0xffffffffff",
            "18446744073709551616",
            "99999999999999999999999",
        ] {
            let e = xid_in(bad).unwrap_err();
            assert_eq!(e.sqlstate, "22003", "input {bad:?}");
            assert_eq!(
                e.message,
                format!("value \"{bad}\" is out of range for type xid")
            );
        }

        Ok(())
    }

    /// `xid8` spans the whole `u64`, so a negative simply wraps and nothing
    /// short of a magnitude wider than `u64` is out of range.
    #[test]
    fn xid8_wraps_across_the_whole_u64() -> anyhow::Result<()> {
        assert_eq!(xid8_in("-1")?, u64::MAX);
        assert_eq!(xid8_in("0xffffffffffffffff")?, u64::MAX);
        assert_eq!(xid8_in("18446744073709551615")?, u64::MAX);
        assert_eq!(xid8_in("-9223372036854775808")?, 9223372036854775808);
        assert_eq!(xid8_in("-9223372036854775809")?, 9223372036854775807);
        // A value `xid` rejects for width is perfectly ordinary here.
        assert_eq!(xid8_in("4294967296")?, 4294967296);

        for bad in ["18446744073709551616", "0xffffffffffffffffffff"] {
            let e = xid8_in(bad).unwrap_err();
            assert_eq!(e.sqlstate, "22003", "input {bad:?}");
            assert_eq!(
                e.message,
                format!("value \"{bad}\" is out of range for type xid8")
            );
        }

        Ok(())
    }
}
