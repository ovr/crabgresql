//! The acceptor shared by every integer *literal* spelling: the `int2`/`int4`/
//! `int8` input functions ([`crate::cast::text_to_int`]) and the numeric
//! constants the parser hands the binder.
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! acceptance — which spellings convert, which raise `22P02`, and which raise
//! `22003` — derived by probing PostgreSQL 18.4 and pinned by the boundary cases
//! at the bottom of this file. It is deliberately *not* C's `strtol`: that one
//! backs `oid`/`xid` (see [`crate::xid::scan_prefix`]) and differs on almost
//! every rule — it takes a bare leading `0` as octal, has no `0b`/`0o`, allows
//! no underscores, and stops at the first bad character instead of rejecting.
//!
//! The grammar, in one place because three call sites depend on it agreeing:
//!
//! ```text
//! literal := ws* sign? body ws*
//! sign    := '+' | '-'
//! body    := ('0x'|'0X') hexdigits | ('0o'|'0O') octdigits
//!          | ('0b'|'0B') bindigits | decdigits
//! ```
//!
//! where a digit run may carry `_` separators. An underscore is accepted only
//! *between* digits — with one wrinkle: the radix prefix counts as a digit for
//! that rule, so `0b_10_0101` converts while a bare `_100` does not. `100_` and
//! `10__000` are rejected because the underscore is not followed by a digit.

/// Which way a scan failed. The caller owns the message, because the type name
/// (`smallint` / `integer` / `bigint`) and the quoted spelling differ per site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanError {
    /// The text is not an integer literal at all — `22P02`.
    Syntax,
    /// Well-formed, but the magnitude does not fit a `u128` — `22003`.
    Range,
}

/// Scan an integer literal, returning `(negative, magnitude)`.
///
/// The magnitude is unsigned so the most negative value of each width still
/// round-trips: `int2 '-0x8000'` is 32768 with `negative` set, which no `i16`
/// could carry. Callers apply their own range check against that pair — see
/// [`crate::cast::text_to_int`].
pub fn scan_int_literal(s: &str) -> Result<(bool, u128), ScanError> {
    let t = s.trim_matches(|c: char| c.is_ascii_whitespace());
    let bytes = t.as_bytes();
    let mut i = 0;

    let negative = match bytes.first() {
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

    // A radix prefix is only a prefix when it introduces a body; `0` alone is
    // the decimal zero, and `0x` with no digits after it falls out of the digit
    // loop below as a syntax error either way.
    let radix = match (bytes.get(i), bytes.get(i + 1)) {
        (Some(b'0'), Some(b'x' | b'X')) => 16,
        (Some(b'0'), Some(b'o' | b'O')) => 8,
        (Some(b'0'), Some(b'b' | b'B')) => 2,
        _ => 10,
    };
    if radix != 10 {
        i += 2;
    }

    // `prev_is_digit` starts true for a prefixed literal so `0b_10_0101` passes:
    // PG treats the prefix itself as the left-hand digit of the separator rule.
    let mut prev_is_digit = radix != 10;
    let mut digits = 0usize;
    let mut acc: u128 = 0;
    let mut overflow = false;

    while i < bytes.len() {
        let c = bytes[i];
        if c == b'_' {
            // Both neighbours must be digits. The next-char half is what rejects
            // `100_` (nothing follows) and `10__000` (an underscore follows).
            if !prev_is_digit
                || !bytes
                    .get(i + 1)
                    .is_some_and(|&n| digit_val(n, radix).is_some())
            {
                return Err(ScanError::Syntax);
            }
            prev_is_digit = false;
            i += 1;
            continue;
        }
        let Some(d) = digit_val(c, radix) else {
            return Err(ScanError::Syntax);
        };
        // Keep scanning past the overflow so a trailing bad character is still
        // reported as a syntax error, as PG does: `0x` + junk is `22P02` however
        // long the run is.
        acc = match acc
            .checked_mul(radix as u128)
            .and_then(|v| v.checked_add(d as u128))
        {
            Some(v) => v,
            None => {
                overflow = true;
                0
            }
        };
        prev_is_digit = true;
        digits += 1;
        i += 1;
    }

    if digits == 0 {
        return Err(ScanError::Syntax);
    }
    if overflow {
        return Err(ScanError::Range);
    }
    Ok((negative, acc))
}

/// The value of `c` as a digit in `radix`, or `None` if it is not one.
fn digit_val(c: u8, radix: u32) -> Option<u32> {
    let v = match c {
        b'0'..=b'9' => (c - b'0') as u32,
        b'a'..=b'f' => (c - b'a') as u32 + 10,
        b'A'..=b'F' => (c - b'A') as u32 + 10,
        _ => return None,
    };
    (v < radix).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> i128 {
        let (neg, mag) = scan_int_literal(s).expect("accepted");
        if neg { -(mag as i128) } else { mag as i128 }
    }

    #[test]
    fn decimal_spellings() {
        assert_eq!(ok("0"), 0);
        assert_eq!(ok("1234"), 1234);
        assert_eq!(ok("+1234"), 1234);
        assert_eq!(ok("-1234"), -1234);
        assert_eq!(ok("  1234 "), 1234);
        assert_eq!(ok("\t-32768\n"), -32768);
    }

    #[test]
    fn non_decimal_spellings() {
        assert_eq!(ok("0b100101"), 37);
        assert_eq!(ok("0o273"), 187);
        assert_eq!(ok("0x42F"), 1071);
        // The prefix letter is case-insensitive, as is a hex digit.
        assert_eq!(ok("0X42f"), 1071);
        assert_eq!(ok("0B100101"), 37);
        assert_eq!(ok("0O273"), 187);
        // A sign binds outside the prefix.
        assert_eq!(ok("-0x8000"), -32768);
        assert_eq!(ok("-0b1000000000000000"), -32768);
        assert_eq!(ok("-0o100000"), -32768);
    }

    #[test]
    fn underscore_separators() {
        assert_eq!(ok("1_000"), 1000);
        assert_eq!(ok("1_2_3"), 123);
        assert_eq!(ok("0xE_FF"), 3839);
        assert_eq!(ok("0o2_73"), 187);
        // The one asymmetry: a separator may lead the digits of a *prefixed*
        // literal, because the prefix stands in for the digit on its left.
        assert_eq!(ok("0b_10_0101"), 37);
    }

    #[test]
    fn syntax_errors() {
        for bad in [
            "", "   ", "asdf", "34.5", "- 1234", "4 444", "123 dt", "0b", "0o", "0x", "0b2", "0o8",
            "0xg", "_100", "100_", "10__000", "0x_", "1x", "123abc", "+", "-",
        ] {
            assert_eq!(
                scan_int_literal(bad),
                Err(ScanError::Syntax),
                "expected a syntax error for {bad:?}"
            );
        }
    }

    #[test]
    fn magnitude_past_u128_is_range() {
        let wide = format!("0x{}", "F".repeat(33));
        assert_eq!(scan_int_literal(&wide), Err(ScanError::Range));
        // …but junk after an overflowing run is still a syntax error.
        assert_eq!(
            scan_int_literal(&format!("{wide}z")),
            Err(ScanError::Syntax)
        );
    }
}
