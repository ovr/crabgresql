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

/// A literal that has passed the grammar: its sign, its base, and the digit run
/// as written (separators still in it). Borrowing the run rather than collecting
/// it keeps the common path allocation-free — `text_to_int` sits under COPY.
struct Literal<'a> {
    negative: bool,
    radix: u32,
    body: &'a str,
}

impl Literal<'_> {
    /// The digit values, left to right. `scan` has already proved every byte is
    /// either a separator or a digit of `radix`, so nothing here can fail.
    fn digits(&self) -> impl Iterator<Item = u32> + '_ {
        self.body
            .bytes()
            .filter_map(move |c| digit_val(c, self.radix))
    }
}

/// The grammar, in one place: everything below folds the same digit run
/// differently. Only ever reports [`ScanError::Syntax`] — a magnitude that does
/// not fit is the *fold's* business, not the grammar's.
fn scan(s: &str) -> Result<Literal<'_>, ScanError> {
    scan_trimmed(trim_pg_space(s))
}

/// [`scan`] over an already-trimmed literal, so a caller that trimmed to look
/// for its own fast path does not pay for a second pass.
fn scan_trimmed(t: &str) -> Result<Literal<'_>, ScanError> {
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
    let body = &t[i..];

    // `prev_is_digit` starts true for a prefixed literal so `0b_10_0101` passes:
    // PG treats the prefix itself as the left-hand digit of the separator rule.
    let mut prev_is_digit = radix != 10;
    let mut digits = 0usize;

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
        if digit_val(c, radix).is_none() {
            return Err(ScanError::Syntax);
        }
        prev_is_digit = true;
        digits += 1;
        i += 1;
    }

    if digits == 0 {
        return Err(ScanError::Syntax);
    }
    Ok(Literal {
        negative,
        radix,
        body,
    })
}

/// Scan an integer literal, returning `(negative, magnitude)`.
///
/// The magnitude is unsigned so the most negative value of each width still
/// round-trips: `int2 '-0x8000'` is 32768 with `negative` set, which no `i16`
/// could carry. Callers apply their own range check against that pair — see
/// [`crate::cast::text_to_int`].
///
/// A magnitude past `u128` is [`ScanError::Range`]. That is the honest answer
/// for the integer types (it is out of range for all three widths anyway); a
/// caller that can hold more — a `numeric` constant — wants
/// [`scan_int_literal_decimal`] instead.
pub fn scan_int_literal(s: &str) -> Result<(bool, u128), ScanError> {
    // A plain decimal run is what a bulk load carries in essentially every
    // cell, and it needs neither the general scanner's separator state machine
    // nor 128-bit arithmetic: at most 19 digits cannot overflow a `u64`. What
    // makes this safe to short-circuit is that the shape it accepts — optional
    // sign, then nothing but ASCII digits — is a strict subset of the grammar
    // in `scan`, folded the same way. Anything else (a separator, a radix
    // prefix, a longer run, junk) falls through untouched.
    let t = trim_pg_space(s);
    let bytes = t.as_bytes();
    let (negative, digits) = match bytes.first() {
        Some(b'-') => (true, &bytes[1..]),
        Some(b'+') => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    if !digits.is_empty() && digits.len() <= MAX_U64_DIGITS {
        let mut acc: u64 = 0;
        let mut plain = true;
        for &c in digits {
            let d = c.wrapping_sub(b'0');
            if d > 9 {
                plain = false;
                break;
            }
            acc = acc * 10 + d as u64;
        }
        if plain {
            return Ok((negative, acc as u128));
        }
    }

    let lit = scan_trimmed(t)?;
    // Folding under a *constant* radix, so the digit decode below compiles to a
    // compare against a literal rather than one against a runtime value.
    let acc = match lit.radix {
        16 => fold::<16>(lit.body),
        8 => fold::<8>(lit.body),
        2 => fold::<2>(lit.body),
        _ => fold::<10>(lit.body),
    }?;
    Ok((lit.negative, acc))
}

/// The longest decimal run that cannot overflow a `u64`: `u64::MAX` has 20
/// digits, so any 19 of them fit.
const MAX_U64_DIGITS: usize = 19;

/// `body * R + digit`, over a run [`scan`] has already validated.
///
/// The `else continue` arm is reached only for a `_` separator: every other
/// byte was proved a digit of this radix by the grammar, which is also why the
/// separators can be skipped here rather than re-checked.
fn fold<const R: u32>(body: &str) -> Result<u128, ScanError> {
    let mut acc: u128 = 0;
    for &c in body.as_bytes() {
        let Some(d) = digit_val(c, R) else { continue };
        acc = acc
            .checked_mul(R as u128)
            .and_then(|v| v.checked_add(d as u128))
            .ok_or(ScanError::Range)?;
    }
    Ok(acc)
}

/// Scan an integer literal into its decimal digits, with no ceiling on the
/// magnitude. PostgreSQL widens an integer constant past `bigint` into
/// `numeric`, and keeps doing so without limit, so `0x` followed by forty hex
/// digits is a number and not an error.
///
/// Never returns [`ScanError::Range`]; the only failure is a malformed literal.
pub fn scan_int_literal_decimal(s: &str) -> Result<(bool, String), ScanError> {
    let lit = scan(s)?;
    // Base 10 is already the answer — take the digits as written. This is not
    // just an optimization: the long multiplication below is quadratic, and a
    // `numeric` may carry thousands of digits.
    if lit.radix == 10 {
        let text: String = lit.digits().map(|d| char::from(b'0' + d as u8)).collect();
        return Ok((lit.negative, text));
    }
    // Long multiplication into little-endian decimal digits: `out = out * radix + d`.
    let mut out: Vec<u8> = vec![0];
    for d in lit.digits() {
        let mut carry = d;
        for slot in out.iter_mut() {
            let v = u32::from(*slot) * lit.radix + carry;
            *slot = (v % 10) as u8;
            carry = v / 10;
        }
        while carry > 0 {
            out.push((carry % 10) as u8);
            carry /= 10;
        }
    }
    while out.len() > 1 && out.last() == Some(&0) {
        out.pop();
    }
    let text: String = out.iter().rev().map(|d| char::from(b'0' + d)).collect();
    Ok((lit.negative, text))
}

/// The whitespace PostgreSQL's integer input functions skip, which is C's
/// `isspace`. Rust has no predicate for exactly this set: `is_ascii_whitespace`
/// omits vertical tab, and `char::is_whitespace` is Unicode White_Space, which
/// wrongly accepts NBSP and U+3000 (`' 1'::int4` with a NBSP is `22P02` in PG).
/// The same six characters separate `oidvector` elements — see the separator
/// test in [`crate::vector`].
///
/// Tested per *byte* rather than per `char`: every member is ASCII, so a
/// multi-byte character can never match a continuation byte, and this skips the
/// UTF-8 decode `str::trim_matches` would run over the whole literal.
const fn is_pg_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// [`str::trim_matches`] over [`is_pg_space`], without the decode.
///
/// Slicing at the computed indices cannot split a character: both ends stopped
/// at a byte that is not one of the six ASCII spaces, and a UTF-8 continuation
/// byte is never ASCII.
fn trim_pg_space(s: &str) -> &str {
    let b = s.as_bytes();
    let mut start = 0;
    while start < b.len() && is_pg_space(b[start]) {
        start += 1;
    }
    let mut end = b.len();
    while end > start && is_pg_space(b[end - 1]) {
        end -= 1;
    }
    &s[start..end]
}

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

    /// The surrounding whitespace is C's `isspace`, not Rust's idea of either
    /// ASCII or Unicode space — vertical tab counts, NBSP does not.
    #[test]
    fn pg_whitespace_is_trimmed_and_nothing_more() {
        for sep in [" ", "\t", "\n", "\x0b", "\x0c", "\r"] {
            assert_eq!(ok(&format!("{sep}42{sep}")), 42, "sep {sep:?}");
        }
        for sep in ["\u{a0}", "\u{3000}"] {
            assert_eq!(
                scan_int_literal(&format!("{sep}42")),
                Err(ScanError::Syntax),
                "sep {sep:?}"
            );
        }
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

    /// `crabgresql-parser` carries a copy of this grammar (`numlit.rs`) because
    /// that crate is a fork of sqlparser-rs and depends on nothing else in the
    /// workspace. This is the table both must agree on; the parser asserts the
    /// same one. Change this file first, then mirror it there.
    #[test]
    fn mirrors_the_parser_crate_grammar() {
        let accepted = [
            ("5", 5),
            ("-42", -42),
            ("+7", 7),
            ("0x5", 5),
            ("0X42f", 1071),
            ("0o17", 15),
            ("0b11", 3),
            ("1_000", 1000),
            ("0xE_FF", 3839),
            ("0b_10_0101", 37),
            ("-0x8000", -32768),
        ];
        for (text, want) in accepted {
            assert_eq!(ok(text), want, "for {text:?}");
        }
        for bad in [
            "", "  ", "1.5", "1e5", "abc", "0x", "0o", "0b", "_100", "100_", "10__000", "0b12",
            "12abc", "1 2",
        ] {
            assert_eq!(scan_int_literal(bad), Err(ScanError::Syntax), "for {bad:?}");
        }
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

    /// [`scan_int_literal`] answers a plain decimal run without consulting
    /// [`scan`] at all, so the two have to agree on every input — the value it
    /// returns *and* which error it declines with.
    ///
    /// The corpus is every spelling the tests above pin, plus the boundaries a
    /// register-sized fast path can get wrong: 19 vs 20 digits, `u64::MAX`
    /// either side, and a run long enough to need the checked 128-bit fold.
    #[test]
    fn fast_path_agrees_with_the_general_scanner() {
        let general = |s: &str| -> Result<(bool, u128), ScanError> {
            let lit = scan(s)?;
            let radix = lit.radix as u128;
            let mut acc: u128 = 0;
            for d in lit.digits() {
                acc = acc
                    .checked_mul(radix)
                    .and_then(|v| v.checked_add(d as u128))
                    .ok_or(ScanError::Range)?;
            }
            Ok((lit.negative, acc))
        };

        let mut corpus: Vec<String> = [
            "0",
            "007",
            "1234",
            "+1234",
            "-1234",
            "  1234 ",
            "\t-32768\n",
            "\x0b42\x0c",
            "0b100101",
            "0o273",
            "0x42F",
            "0X42f",
            "-0x8000",
            "1_000",
            "1_2_3",
            "0xE_FF",
            "0b_10_0101",
            "",
            "  ",
            "1.5",
            "1e5",
            "abc",
            "0x",
            "0o",
            "0b",
            "_100",
            "100_",
            "10__000",
            "0b12",
            "12abc",
            "1 2",
            "- 1234",
            "+",
            "-",
            "\u{a0}42",
            "\u{3000}42",
            "1\u{a0}",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        // The register boundary, from both sides and with both signs.
        for digits in [1usize, 18, 19, 20, 21, 39, 40] {
            corpus.push("9".repeat(digits));
            corpus.push(format!("-{}", "9".repeat(digits)));
            corpus.push(format!("1{}", "0".repeat(digits)));
        }
        for around in [u64::MAX - 1, u64::MAX] {
            corpus.push(around.to_string());
            corpus.push(format!("-{around}"));
        }
        corpus.push((u64::MAX as u128 + 1).to_string());
        corpus.push(u128::MAX.to_string());
        corpus.push((u128::MAX as i128 as u128).to_string() + "0");

        for text in corpus {
            assert_eq!(
                scan_int_literal(&text),
                general(&text),
                "fast and general paths disagree on {text:?}"
            );
        }
    }

    /// The decimal fold has no ceiling, so the magnitudes that are `Range` above
    /// still convert — that is what lets a wide non-decimal constant widen into
    /// `numeric` instead of failing to bind.
    #[test]
    fn decimal_fold_has_no_ceiling() -> anyhow::Result<()> {
        let dec = |s: &str| -> anyhow::Result<String> {
            let (neg, text) = scan_int_literal_decimal(s)?;
            Ok(if neg { format!("-{text}") } else { text })
        };
        assert_eq!(dec("0b100101")?, "37");
        assert_eq!(dec("0o273")?, "187");
        assert_eq!(dec("0x42F")?, "1071");
        assert_eq!(dec("0x0")?, "0");
        assert_eq!(dec("0b_10_0101")?, "37");
        assert_eq!(dec("-0x8000")?, "-32768");
        assert_eq!(dec("0x8000000000000000")?, "9223372036854775808");
        // Past u128, where `scan_int_literal` gives up.
        assert_eq!(
            dec(&format!("0x{}", "F".repeat(33)))?,
            "5444517870735015415413993718908291383295"
        );
        // Base 10 takes the digits as written: separators drop out, but the
        // run is not renormalized, so leading zeros survive.
        assert_eq!(dec("1_000")?, "1000");
        assert_eq!(dec("007")?, "007");
        assert_eq!(scan_int_literal_decimal("0x"), Err(ScanError::Syntax));

        Ok(())
    }
}
