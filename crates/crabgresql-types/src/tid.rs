//! `tid`: a tuple identifier — the `(block, offset)` address of a row version.
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior — the accepted input spellings, the `(block,offset)` output, and the
//! SQLSTATE/message of a syntax error. Every rule below was derived by probing
//! PostgreSQL 18.4 directly, not read off its source; the boundary cases are
//! pinned by the tests at the bottom of this file.
//!
//! Representation: `Value::Tid { block: u32, offset: u16 }`, PG's `BlockNumber`
//! and `OffsetNumber`. Ordering is `(block, offset)` lexicographic, which is
//! PG's `tid` btree order, so no comparison helper is needed here.

// SQLSTATE (kept as a literal; the types crate does not depend on the protocol
// crate — the binder/executor map these to `sqlstate::*`).
const INVALID_TEXT_REPRESENTATION: &str = "22P02";

/// The lowest value the block field accepts once a negative input has wrapped:
/// `(-2147483648i64) as u64`. See [`parse`] for why the accepted set has this
/// second, disjoint band at the top of the `u64` range.
const MIN_WRAPPED_BLOCK: u64 = (i32::MIN as i64) as u64;

/// A parse error, carrying the SQLSTATE and message PG reports.
#[derive(Clone, Debug, PartialEq)]
pub struct TidError {
    pub sqlstate: &'static str,
    pub message: String,
}

fn invalid_syntax(input: &str) -> TidError {
    TidError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("invalid input syntax for type tid: \"{input}\""),
    }
}

/// One field of the pair, decoded the way PG's input function is observed to
/// decode it: leading whitespace and an optional sign are skipped, the digits
/// are read in base 10, and the field must then be fully consumed — so
/// `"(0, 1)"` parses but `"(0 ,1)"` does not.
///
/// The magnitude is accumulated in `u64` and *negated within `u64`* rather than
/// being kept signed. That wrap is observable: PG renders `'(-2,0)'::tid` and
/// `'(18446744073709551614,0)'::tid` identically, as `(4294967294,0)`. A
/// magnitude too wide for `u64` is rejected (PG's `ERANGE`), which is why
/// `'(99999999999999999999999,0)'` is an error rather than a wrap.
fn field(text: &str) -> Option<u64> {
    let text = text.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // `u128` first so a magnitude just past `u64::MAX` is a clean rejection
    // rather than a parse failure indistinguishable from malformed input.
    let magnitude = u64::try_from(digits.parse::<u128>().ok()?).ok()?;
    Some(if negative {
        magnitude.wrapping_neg()
    } else {
        magnitude
    })
}

/// `tidin`: read the `(block,offset)` pair.
///
/// Text before the first `(` and after the first `)` is ignored, matching PG —
/// `'x(0,1)y'::tid` is `(0,1)`. Within the parens each field follows [`field`].
///
/// The two fields have different range rules, which is what makes `'(-1,0)'`
/// legal (it is `(4294967295,0)`) while `'(0,-1)'` is not:
///
/// * **block** — accepted when the wrapped `u64` is either at most `u32::MAX`
///   or at least [`MIN_WRAPPED_BLOCK`], i.e. exactly the values that fit `i32`
///   or `u32`. So `-2147483648` is accepted (as `2147483648`) but
///   `-2147483649` is not, and `4294967296` is rejected rather than truncated.
/// * **offset** — accepted only in `0..=65535`. A negative offset wraps to a
///   huge `u64` and so always falls outside, which is the whole of why the two
///   fields behave asymmetrically.
pub fn parse(input: &str) -> Result<(u32, u16), TidError> {
    let invalid = || invalid_syntax(input);

    let (_, after_open) = input.split_once('(').ok_or_else(invalid)?;
    let (block, after_block) = after_open.split_once(',').ok_or_else(invalid)?;
    // Taking the *first* `)` is what rejects a third field: `'(0,1,2)'` leaves
    // `"1,2"` here, which is not a decimal run.
    let (offset, _tail) = after_block.split_once(')').ok_or_else(invalid)?;

    let block = field(block).ok_or_else(invalid)?;
    let offset = field(offset).ok_or_else(invalid)?;

    if block > u64::from(u32::MAX) && block < MIN_WRAPPED_BLOCK {
        return Err(invalid());
    }
    if offset > u64::from(u16::MAX) {
        return Err(invalid());
    }

    Ok((block as u32, offset as u16))
}

/// `tidout`: `(block,offset)`, no spaces.
pub fn format(block: u32, offset: u16) -> String {
    format!("({block},{offset})")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(input: &str) {
        let e = parse(input)
            .expect_err("not a parenthesized (block,offset) pair within tid's field ranges");
        assert_eq!(e.sqlstate, "22P02", "input {input:?}");
        assert_eq!(
            e.message,
            format!("invalid input syntax for type tid: \"{input}\"")
        );
    }

    #[test]
    fn roundtrips_the_canonical_form() -> anyhow::Result<()> {
        for (text, block, offset) in [
            ("(0,0)", 0u32, 0u16),
            ("(0,1)", 0, 1),
            ("(1,42)", 1, 42),
            ("(4294967295,65535)", u32::MAX, u16::MAX),
        ] {
            assert_eq!(parse(text)?, (block, offset), "input {text}");
            assert_eq!(format(block, offset), text);
        }

        Ok(())
    }

    /// PG ignores everything outside the parens and skips whitespace *leading*
    /// each field — but not trailing it, which is why `"(0 ,1)"` is an error
    /// while `"(0, 1)"` is not. All six probed against PostgreSQL 18.4.
    #[test]
    fn accepts_pgs_lenient_spellings() -> anyhow::Result<()> {
        assert_eq!(parse("(0,1)x")?, (0, 1));
        assert_eq!(parse("x(0,1)")?, (0, 1));
        assert_eq!(parse(" (0,1)")?, (0, 1));
        assert_eq!(parse("( 0,1)")?, (0, 1));
        assert_eq!(parse("(0,  1)")?, (0, 1));
        assert_eq!(parse("(+1,+2)")?, (1, 2));

        err("(0 ,1)");
        err(" ( 0 , 1 ) ");

        Ok(())
    }

    /// The block field accepts exactly the values that fit `i32` or `u32`,
    /// negatives wrapping. The upper band is `strtoul`'s unsigned-long
    /// representation of those negatives showing through, so the two spellings
    /// of the same wrapped value must agree.
    #[test]
    fn block_accepts_i32_or_u32_and_wraps_negatives() -> anyhow::Result<()> {
        assert_eq!(parse("(-1,0)")?, (u32::MAX, 0));
        assert_eq!(parse("(-2,0)")?, (4294967294, 0));
        assert_eq!(parse("(18446744073709551614,0)")?, (4294967294, 0));
        assert_eq!(parse("(18446744073709551615,0)")?, (u32::MAX, 0));
        assert_eq!(parse("(-2147483648,0)")?, (2147483648, 0));

        // Just past the negative band, and anywhere in the gap above `u32`.
        err("(-2147483649,0)");
        err("(4294967296,0)");
        err("(8589934592,0)");
        err("(-4294967295,0)");
        // Past `u64` entirely: PG's ERANGE, not a wrap.
        err("(18446744073709551616,0)");
        err("(99999999999999999999999,0)");

        Ok(())
    }

    /// The offset field has no negative band at all — a negative wraps to a
    /// huge `u64` and lands outside `0..=65535`.
    #[test]
    fn offset_is_unsigned_16_bit() -> anyhow::Result<()> {
        assert_eq!(parse("(1,-0)")?, (1, 0));

        for bad in ["(1,-1)", "(1,65536)", "(1,-32768)", "(1,4294967295)"] {
            err(bad);
        }

        Ok(())
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "", "(0)",     // missing offset
            "(,1)",    // empty block
            "(0,)",    // empty offset
            "0,1",     // unparenthesized
            "(0,1",    // unterminated
            "((0,1)",  // the block field is then "(0"
            "(0,1,2)", // the offset field is then "1,2"
            "(0x1,1)", // not decimal
        ] {
            err(bad);
        }
    }
}
