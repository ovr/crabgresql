//! Reading back the text of a `Token::Number`.
//!
//! The tokenizer keeps a numeric literal as written — prefix, digit separators
//! and all — so the places that need its *value* while still parsing (a type
//! modifier, a `FETCH` count, `MODULUS`/`REMAINDER`) have to decode it. A plain
//! `str::parse` cannot: it rejects `0x5`, `0o17`, `0b11` and `1_0`, all of which
//! PostgreSQL accepts wherever an integer constant is allowed.
//!
//! This deliberately duplicates `crabgresql_types::intlit`, which is the same
//! grammar and the authority on it. This crate is a fork of sqlparser-rs and
//! carries no dependency on the rest of the workspace; that autonomy is worth
//! more than sharing 40 lines, so the rule is: change `intlit` first, then
//! mirror it here. The two are kept honest by the
//! `mirrors_the_parser_crate_grammar` test in
//! `crates/crabgresql-types/src/intlit.rs`, which asserts the same table of
//! spellings both must accept.

/// The value of an integer literal token, or `None` if the text is not one.
///
/// Accepts what PostgreSQL's integer constants accept: an optional sign, an
/// optional `0x`/`0o`/`0b` prefix, and `_` separators between digits — where the
/// radix prefix counts as the digit on a separator's left, so `0b_10_0101` is a
/// number and a bare `_100` is not.
pub fn literal_int(s: &str) -> Option<i128> {
    let t = s.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'));
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

    let radix: u32 = match (bytes.get(i), bytes.get(i + 1)) {
        (Some(b'0'), Some(b'x' | b'X')) => 16,
        (Some(b'0'), Some(b'o' | b'O')) => 8,
        (Some(b'0'), Some(b'b' | b'B')) => 2,
        _ => 10,
    };
    if radix != 10 {
        i += 2;
    }

    let digit = |c: u8| (c as char).to_digit(radix);
    // True after the prefix as well as after a digit: PostgreSQL lets a
    // separator lead the digits of a prefixed literal.
    let mut prev_is_digit = radix != 10;
    let mut digits = 0usize;
    let mut acc: i128 = 0;

    while i < bytes.len() {
        let c = bytes[i];
        if c == b'_' {
            if !prev_is_digit
                || !bytes
                    .get(i + 1)
                    .copied()
                    .is_some_and(|n| digit(n).is_some())
            {
                return None;
            }
            prev_is_digit = false;
            i += 1;
            continue;
        }
        let d = digit(c)?;
        acc = acc.checked_mul(i128::from(radix))?.checked_add(d.into())?;
        prev_is_digit = true;
        digits += 1;
        i += 1;
    }

    (digits > 0).then(|| if negative { -acc } else { acc })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_what_postgresql_accepts() {
        assert_eq!(literal_int("5"), Some(5));
        assert_eq!(literal_int(" -42 "), Some(-42));
        assert_eq!(literal_int("+7"), Some(7));
        assert_eq!(literal_int("0x5"), Some(5));
        assert_eq!(literal_int("0X42f"), Some(1071));
        assert_eq!(literal_int("0o17"), Some(15));
        assert_eq!(literal_int("0b11"), Some(3));
        assert_eq!(literal_int("1_000"), Some(1000));
        assert_eq!(literal_int("0xE_FF"), Some(3839));
        assert_eq!(literal_int("0b_10_0101"), Some(37));
        assert_eq!(literal_int("-0x8000"), Some(-32768));
    }

    #[test]
    fn rejects_what_postgresql_rejects() {
        for bad in [
            "", "  ", "1.5", "1e5", "abc", "0x", "0o", "0b", "_100", "100_", "10__000", "0b12",
            "12abc", "1 2",
        ] {
            assert_eq!(literal_int(bad), None, "for {bad:?}");
        }
    }

    /// The value is only useful if it does not silently wrap.
    #[test]
    fn a_magnitude_past_i128_is_not_a_value() {
        assert_eq!(literal_int(&"9".repeat(40)), None);
        assert_eq!(literal_int(&format!("0x{}", "F".repeat(33))), None);
    }
}
