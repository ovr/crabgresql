//! `uuid`: input parsing, canonical output, and comparison.
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior — the canonical lowercase output, the accepted input spellings, and
//! the SQLSTATE/message of a syntax error — implemented independently.
//!
//! Representation: the 16 raw bytes in network order (`Value::Uuid([u8; 16])`).
//! The natural byte order already gives PG's `uuid_cmp`, so ordering is a plain
//! `[u8; 16]` comparison and needs no helper here.

// SQLSTATE (kept as a literal; the types crate does not depend on the protocol
// crate — the binder/executor map these to `sqlstate::*`).
const INVALID_TEXT_REPRESENTATION: &str = "22P02";

/// A parse error, carrying the SQLSTATE and message PG reports.
#[derive(Clone, Debug, PartialEq)]
pub struct UuidError {
    pub sqlstate: &'static str,
    pub message: String,
}

fn invalid_syntax(input: &str) -> UuidError {
    UuidError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("invalid input syntax for type uuid: \"{input}\""),
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn hex_lo(nibble: u8) -> char {
    char::from(match nibble {
        0..=9 => b'0' + nibble,
        _ => b'a' + (nibble - 10),
    })
}

/// `uuid_in`: accept the 16 bytes as 32 hex digits, optionally wrapped in
/// `{ }` and optionally punctuated with `-` after any even count of hex digits
/// (so the canonical `8-4-4-4-12` form, an unpunctuated run, and PG's lenient
/// intermediate forms all parse). Anything else is `22P02`, echoing the input.
///
/// This mirrors PG's `string_to_uuid`, which consumes an optional `-` after
/// byte `i` when `i` is odd and not the last byte.
pub fn parse(input: &str) -> Result<[u8; 16], UuidError> {
    let s = input.as_bytes();
    let mut pos = 0usize;
    let braces = s.first() == Some(&b'{');
    if braces {
        pos += 1;
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        let hi = s.get(pos).copied().and_then(hex_val);
        let lo = s.get(pos + 1).copied().and_then(hex_val);
        let (Some(hi), Some(lo)) = (hi, lo) else {
            return Err(invalid_syntax(input));
        };
        out[i] = (hi << 4) | lo;
        pos += 2;
        if i % 2 == 1 && i < 15 && s.get(pos) == Some(&b'-') {
            pos += 1;
        }
    }
    if braces {
        if s.get(pos) != Some(&b'}') {
            return Err(invalid_syntax(input));
        }
        pos += 1;
    }
    if pos != s.len() {
        return Err(invalid_syntax(input));
    }
    Ok(out)
}

/// `uuid_out`: the canonical lowercase `8-4-4-4-12` hyphenated form.
pub fn format(b: &[u8; 16]) -> String {
    let mut out = String::with_capacity(36);
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push(hex_lo(byte >> 4));
        out.push(hex_lo(byte & 0x0f));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANON: &str = "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11";
    const BYTES: [u8; 16] = [
        0xa0, 0xee, 0xbc, 0x99, 0x9c, 0x0b, 0x4e, 0xf8, 0xbb, 0x6d, 0x6b, 0xb9, 0xbd, 0x38, 0x0a,
        0x11,
    ];

    #[test]
    fn roundtrip_canonical() {
        assert_eq!(parse(CANON).unwrap(), BYTES);
        assert_eq!(format(&BYTES), CANON);
    }

    #[test]
    fn accepts_input_variants() {
        // No hyphens, braces, uppercase — all normalize to the canonical form.
        assert_eq!(parse("a0eebc999c0b4ef8bb6d6bb9bd380a11").unwrap(), BYTES);
        assert_eq!(parse("{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}").unwrap(), BYTES);
        assert_eq!(parse("A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11").unwrap(), BYTES);
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "",
            "a0eebc99",                                // too short
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11-",   // trailing junk
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a1z",    // non-hex
            "{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",   // unmatched brace
            "a0-eebc999c0b4ef8bb6d6bb9bd380a11",        // hyphen after even byte index
        ] {
            let e = parse(bad).unwrap_err();
            assert_eq!(e.sqlstate, "22P02", "{bad}");
            assert_eq!(e.message, format!("invalid input syntax for type uuid: \"{bad}\""));
        }
    }
}
