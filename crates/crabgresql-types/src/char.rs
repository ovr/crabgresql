//! `"char"`: PostgreSQL's ad-hoc one-*byte* type (OID 18).
//!
//! Not to be confused with `bpchar` (OID 1042), the blank-padded `CHAR(n)` that
//! SQL's unquoted `char`/`character` keyword names. `"char"` holds a single raw
//! byte, takes no typmod, and is what most `pg_catalog` flag columns really are
//! (`relkind`, `typtype`, `provolatile`, `castcontext`, …).
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior. Every rule below was derived by probing PostgreSQL 18.4 directly,
//! not read off its source; the boundary cases are pinned by the tests at the
//! bottom of this file.
//!
//! Representation: `Value::Char(u8)`, the raw byte. Two asymmetries in the
//! surrounding semantics look like bugs but are what PG does, so they are
//! called out here rather than at each use site:
//!
//! * Ordering and hashing are **unsigned** (`'\377' > 'a'` is true), but the
//!   `int4` conversion is **signed** (`'\377'::"char"::int4` is `-1`).
//! * [`char_out`] escapes a high-bit byte into four ASCII characters, but
//!   `"char" -> bpchar` does *not* — PG hands back the raw byte there.

/// The text input function, `charin`; also `text_char`, the `text -> "char"`
/// cast, which PG observably routes through the same rule.
///
/// Never fails: anything that is not an octal escape contributes its first
/// *byte*, and an empty input is the zero byte. So `'é'::"char"` is `0xC3`,
/// the first byte of the UTF-8 encoding, not the character.
///
/// The escape must be *exactly* a backslash and three octal digits — `'\77'`
/// and `'\1234'` both fall back to the first byte and yield `\` (`0x5C`). The
/// leading digit is not restricted to `0..=3`: PG accumulates and truncates to
/// a byte, so `'\401'` (0o401 = 257) is `0x01`.
pub fn char_in(s: &str) -> u8 {
    let b = s.as_bytes();
    if b.len() == 4 && b[0] == b'\\' && b[1..].iter().all(|c| (b'0'..=b'7').contains(c)) {
        let v =
            (u32::from(b[1] - b'0') << 6) | (u32::from(b[2] - b'0') << 3) | u32::from(b[3] - b'0');
        return v as u8;
    }
    b.first().copied().unwrap_or(0)
}

/// The text output function, `charout`; also `char_text`, the `"char" -> text`
/// cast, which PG observably produces the same string for.
///
/// The zero byte renders as the empty string — which makes `char_out` lossy in
/// the same way PG's is, since `''::"char"` reads back as the zero byte. A
/// high-bit byte is re-escaped as `\ooo` so the result stays valid UTF-8; every
/// other byte, control characters included, is emitted raw.
pub fn char_out(c: u8) -> String {
    if c & 0x80 != 0 {
        format!("\\{}{}{}", (c >> 6) & 3, (c >> 3) & 7, c & 7)
    } else if c == 0 {
        String::new()
    } else {
        // `c < 0x80`, so this is ASCII and the cast is lossless.
        char::from(c).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The nine statements of `vendor/postgres/regress/sql/char.sql` that
    /// exercise `"char"`, in file order.
    #[test]
    fn upstream_regress_cases() {
        assert_eq!(char_out(char_in("a")), "a");
        assert_eq!(char_out(char_in("\\101")), "A");
        assert_eq!(char_out(char_in("\\377")), "\\377");
        assert_eq!(char_out(char_in("\\000")), "");
        assert_eq!(char_out(char_in("")), "");
    }

    #[test]
    fn octal_escape_must_be_exactly_four_characters() {
        // Anything shorter or longer falls back to the first byte, a backslash.
        assert_eq!(char_in("\\77"), b'\\');
        assert_eq!(char_in("\\1234"), b'\\');
        assert_eq!(char_in("\\"), b'\\');
        assert_eq!(char_in("\\37"), b'\\');
        // A non-octal digit disqualifies it too.
        assert_eq!(char_in("\\38a"), b'\\');
        assert_eq!(char_in("\\008"), b'\\');
    }

    #[test]
    fn octal_escape_truncates_to_a_byte() {
        // 0o401 == 257 == 0x101, truncated to 0x01.
        assert_eq!(char_in("\\401"), 0x01);
        assert_eq!(char_in("\\777"), 0xFF);
        assert_eq!(char_in("\\200"), 0x80);
        assert_eq!(char_in("\\000"), 0x00);
    }

    #[test]
    fn input_takes_the_first_byte_not_the_first_char() {
        assert_eq!(char_in("ab"), b'a');
        assert_eq!(char_in("é"), 0xC3);
        assert_eq!(char_in("日本"), 0xE6);
    }

    #[test]
    fn output_escapes_only_the_high_bit_and_swallows_nul() {
        assert_eq!(char_out(0x00), "");
        assert_eq!(char_out(0x01), "\u{1}");
        assert_eq!(char_out(0x7F), "\u{7f}");
        assert_eq!(char_out(0x80), "\\200");
        assert_eq!(char_out(0xFF), "\\377");
        assert_eq!(char_out(b'\\'), "\\");
    }

    /// A lone backslash survives a round trip precisely because it is not a
    /// four-character escape, so the asymmetry in [`char_in`] is load-bearing.
    #[test]
    fn round_trips_every_byte_except_nul() {
        for c in 1..=u8::MAX {
            assert_eq!(char_in(&char_out(c)), c, "byte {c:#04x}");
        }
        // Zero is the one lossy value: it prints empty, and empty reads as zero.
        assert_eq!(char_in(&char_out(0)), 0);
    }
}
