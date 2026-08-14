//! `macaddr` / `macaddr8`: input parsing, canonical output, comparison, the
//! bitwise operators (`~` `&` `|`), `trunc`, `macaddr8_set7bit`, and the
//! `macaddr` <-> `macaddr8` conversions.
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior — the canonical lowercase colon output, the accepted input
//! spellings (which differ between the two types), the byte-order comparison,
//! and the SQLSTATE/message of a syntax error — implemented independently.
//!
//! Representation: the 6 / 8 raw bytes (`Value::Macaddr([u8; 6])`,
//! `Value::Macaddr8([u8; 8])`). The natural byte order already gives PG's
//! `macaddr_cmp`, so ordering is a plain slice comparison and needs no helper.

use crate::hex;

// SQLSTATEs (kept as literals; the types crate does not depend on the protocol
// crate — the binder/executor map these to `sqlstate::*`).
const INVALID_TEXT_REPRESENTATION: &str = "22P02";
const INVALID_PARAMETER_VALUE: &str = "22023";

/// A parse/conversion error, carrying the SQLSTATE and message PG reports.
#[derive(Clone, Debug, PartialEq)]
pub struct MacaddrError {
    pub sqlstate: &'static str,
    pub message: String,
}

fn invalid6(input: &str) -> MacaddrError {
    MacaddrError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("invalid input syntax for type macaddr: \"{input}\""),
    }
}

fn invalid8(input: &str) -> MacaddrError {
    MacaddrError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("invalid input syntax for type macaddr8: \"{input}\""),
    }
}

fn is_delim(c: u8) -> bool {
    matches!(c, b':' | b'-' | b'.')
}

/// `macaddr_in`: accept exactly PG's fixed set of six-byte spellings — the
/// `sscanf` pattern list. With a single delimiter kind the allowed groupings
/// are: `:`/`-` as six 2-digit groups, `:`/`-` as two 6-digit groups,
/// `.`/`-` as three 4-digit groups; with no delimiter, a bare run of 12 hex
/// digits. Anything else (notably `0800:2b01:0203`) is `22P02`.
pub fn parse_macaddr(input: &str) -> Result<[u8; 6], MacaddrError> {
    let trimmed = input.trim_ascii();
    let s = trimmed.as_bytes();
    let err = || invalid6(input);

    // Determine the (single) delimiter kind used, if any.
    let delim = s.iter().copied().find(|&c| is_delim(c));
    if let Some(d) = delim {
        // Reject mixed delimiter kinds.
        if s.iter().any(|&c| is_delim(c) && c != d) {
            return Err(err());
        }
        let groups: Vec<&[u8]> = s.split(|&c| c == d).collect();
        let lens: Vec<usize> = groups.iter().map(|g| g.len()).collect();
        let ok = match d {
            b':' | b'-' if lens == [2, 2, 2, 2, 2, 2] => true,
            b':' | b'-' if lens == [6, 6] => true,
            b'-' | b'.' if lens == [4, 4, 4] => true,
            _ => false,
        };
        if !ok {
            return Err(err());
        }
        let mut out = [0u8; 6];
        let mut i = 0usize;
        for g in groups {
            for pair in g.chunks(2) {
                let (Some(hi), Some(lo)) = (hex::val(pair[0]), hex::val(pair[1])) else {
                    return Err(err());
                };
                out[i] = (hi << 4) | lo;
                i += 1;
            }
        }
        Ok(out)
    } else {
        // No delimiter: exactly 12 hex digits.
        if s.len() != 12 {
            return Err(err());
        }
        let mut out = [0u8; 6];
        for i in 0..6 {
            let (Some(hi), Some(lo)) = (hex::val(s[2 * i]), hex::val(s[2 * i + 1])) else {
                return Err(err());
            };
            out[i] = (hi << 4) | lo;
        }
        Ok(out)
    }
}

/// `macaddr8_in`: a lenient walker. Leading/trailing whitespace is trimmed; hex
/// digits are grouped by a single, consistent delimiter kind (`:`, `-`, or `.`)
/// or none, with each delimited group an even number of digits; the total must
/// be exactly 12 hex digits (a six-byte MAC, expanded to EUI-64 by inserting
/// `ff:fe` in the middle) or 16 (an eight-byte value taken as-is). Mixed
/// delimiters, non-hex characters, and any other length are `22P02`.
pub fn parse_macaddr8(input: &str) -> Result<[u8; 8], MacaddrError> {
    let trimmed = input.trim_ascii();
    let err = || invalid8(input);

    let mut nibbles: Vec<u8> = Vec::with_capacity(16);
    let mut delim: Option<u8> = None;
    let mut group_len = 0usize;
    for &c in trimmed.as_bytes() {
        if let Some(n) = hex::val(c) {
            nibbles.push(n);
            group_len += 1;
        } else if is_delim(c) {
            // A delimiter must sit between two even-length groups of digits.
            if group_len == 0 || group_len % 2 != 0 {
                return Err(err());
            }
            match delim {
                None => delim = Some(c),
                Some(d) if d == c => {}
                Some(_) => return Err(err()),
            }
            group_len = 0;
        } else {
            return Err(err());
        }
    }
    // Reject a trailing delimiter / empty trailing group.
    if delim.is_some() && group_len == 0 {
        return Err(err());
    }

    match nibbles.len() {
        12 => {
            let mut six = [0u8; 6];
            for i in 0..6 {
                six[i] = (nibbles[2 * i] << 4) | nibbles[2 * i + 1];
            }
            Ok(expand6to8(&six))
        }
        16 => {
            let mut out = [0u8; 8];
            for i in 0..8 {
                out[i] = (nibbles[2 * i] << 4) | nibbles[2 * i + 1];
            }
            Ok(out)
        }
        _ => Err(err()),
    }
}

/// `macaddr_out`: canonical lowercase `xx:xx:xx:xx:xx:xx`.
pub fn format6(b: &[u8; 6]) -> String {
    format_bytes(b)
}

/// `macaddr8_out`: canonical lowercase `xx:xx:xx:xx:xx:xx:xx:xx`.
pub fn format8(b: &[u8; 8]) -> String {
    format_bytes(b)
}

fn format_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (i, byte) in bytes.iter().enumerate() {
        if i != 0 {
            out.push(':');
        }
        out.push(hex::lo(byte >> 4));
        out.push(hex::lo(byte & 0x0f));
    }
    out
}

/// EUI-64 expansion: a six-byte MAC becomes eight bytes by inserting `ff:fe`
/// after the third byte.
pub fn expand6to8(b: &[u8; 6]) -> [u8; 8] {
    [b[0], b[1], b[2], 0xff, 0xfe, b[3], b[4], b[5]]
}

/// `macaddr8`->`macaddr`: only addresses with `ff:fe` in the 4th/5th bytes are
/// convertible; those bytes are dropped.
pub fn narrow8to6(b: &[u8; 8]) -> Result<[u8; 6], MacaddrError> {
    if b[3] != 0xff || b[4] != 0xfe {
        return Err(MacaddrError {
            sqlstate: INVALID_PARAMETER_VALUE,
            message: "macaddr8 data out of range to convert to macaddr".to_string(),
        });
    }
    Ok([b[0], b[1], b[2], b[5], b[6], b[7]])
}

/// `~` (one's complement), bytewise.
pub fn not6(b: &[u8; 6]) -> [u8; 6] {
    let mut out = *b;
    for x in &mut out {
        *x = !*x;
    }
    out
}

pub fn not8(b: &[u8; 8]) -> [u8; 8] {
    let mut out = *b;
    for x in &mut out {
        *x = !*x;
    }
    out
}

pub fn and6(a: &[u8; 6], b: &[u8; 6]) -> [u8; 6] {
    let mut out = [0u8; 6];
    for i in 0..6 {
        out[i] = a[i] & b[i];
    }
    out
}

pub fn or6(a: &[u8; 6], b: &[u8; 6]) -> [u8; 6] {
    let mut out = [0u8; 6];
    for i in 0..6 {
        out[i] = a[i] | b[i];
    }
    out
}

pub fn and8(a: &[u8; 8], b: &[u8; 8]) -> [u8; 8] {
    let mut out = [0u8; 8];
    for i in 0..8 {
        out[i] = a[i] & b[i];
    }
    out
}

pub fn or8(a: &[u8; 8], b: &[u8; 8]) -> [u8; 8] {
    let mut out = [0u8; 8];
    for i in 0..8 {
        out[i] = a[i] | b[i];
    }
    out
}

/// `trunc(macaddr)`: zero the low three bytes (keep the OUI).
pub fn trunc6(b: &[u8; 6]) -> [u8; 6] {
    [b[0], b[1], b[2], 0, 0, 0]
}

/// `trunc(macaddr8)`: zero the low five bytes (keep the OUI).
pub fn trunc8(b: &[u8; 8]) -> [u8; 8] {
    [b[0], b[1], b[2], 0, 0, 0, 0, 0]
}

/// `macaddr8_set7bit`: set the 7th bit (`0x02`) of the first octet, forming a
/// modified EUI-64 address.
pub fn set7bit(b: &[u8; 8]) -> [u8; 8] {
    let mut out = *b;
    out[0] |= 0x02;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macaddr_roundtrip_and_spellings() -> anyhow::Result<()> {
        let canon = [0x08, 0x00, 0x2b, 0x01, 0x02, 0x03];
        for good in [
            "08:00:2b:01:02:03",
            "08-00-2b-01-02-03",
            "08002b:010203",
            "08002b-010203",
            "0800.2b01.0203",
            "0800-2b01-0203",
            "08002b010203",
        ] {
            assert_eq!(parse_macaddr(good)?, canon, "{good}");
        }
        assert_eq!(format6(&canon), "08:00:2b:01:02:03");

        Ok(())
    }

    #[test]
    fn macaddr_rejects() {
        for bad in ["0800:2b01:0203", "not even close", "08:00:2b:01:02", ""] {
            let e = parse_macaddr(bad)
                .expect_err("not one of macaddr's six-byte groupings of 12 hex digits");
            assert_eq!(e.sqlstate, "22P02", "{bad}");
            assert_eq!(
                e.message,
                format!("invalid input syntax for type macaddr: \"{bad}\"")
            );
        }
    }

    #[test]
    fn macaddr8_six_byte_expands_eui64() -> anyhow::Result<()> {
        let want = [0x08, 0x00, 0x2b, 0xff, 0xfe, 0x01, 0x02, 0x03];
        for good in [
            "08:00:2b:01:02:03",
            "08-00-2b-01-02-03",
            "08002b:010203",
            "08002b-010203",
            "0800.2b01.0203",
            "0800-2b01-0203",
            "08002b010203",
            "0800:2b01:0203",
            "08:00:2b:01:02:03     ",
            "    08:00:2b:01:02:03",
        ] {
            assert_eq!(parse_macaddr8(good)?, want, "{good}");
        }
        assert_eq!(format8(&want), "08:00:2b:ff:fe:01:02:03");

        Ok(())
    }

    #[test]
    fn macaddr8_eight_byte_forms() -> anyhow::Result<()> {
        let want = [0x08, 0x00, 0x2b, 0x01, 0x02, 0x03, 0x04, 0x05];
        for good in [
            "08:00:2b:01:02:03:04:05",
            "08-00-2b-01-02-03-04-05",
            "08002b:0102030405",
            "08002b-0102030405",
            "0800.2b01.0203.0405",
            "08002b01:02030405",
            "08002b0102030405",
        ] {
            assert_eq!(parse_macaddr8(good)?, want, "{good}");
        }

        Ok(())
    }

    #[test]
    fn macaddr8_rejects() {
        for bad in [
            "123    08:00:2b:01:02:03",
            "08:00:2b:01:02:03  123",
            "08:00:2b:01:02:03:04:05:06:07",
            "08-00-2b-01-02-03-04-05-06-07",
            "08002b:01020304050607",
            "08002b01020304050607",
            "0z002b0102030405",
            "08002b010203xyza",
            "08:00-2b:01:02:03:04:05",
            "08:00:2b:01.02:03:04:05",
        ] {
            let e = parse_macaddr8(bad).expect_err(
                "not 12 or 16 hex digits under a single delimiter kind, as macaddr8 requires",
            );
            assert_eq!(e.sqlstate, "22P02", "{bad}");
            assert_eq!(
                e.message,
                format!("invalid input syntax for type macaddr8: \"{bad}\"")
            );
        }
    }

    #[test]
    fn ops_and_conversions() -> anyhow::Result<()> {
        let m = [0x08, 0x00, 0x2b, 0x01, 0x02, 0x03];
        assert_eq!(not6(&m), [0xf7, 0xff, 0xd4, 0xfe, 0xfd, 0xfc]);
        assert_eq!(
            and6(&m, &[0, 0, 0, 0xff, 0xff, 0xff]),
            [0, 0, 0, 0x01, 0x02, 0x03]
        );
        assert_eq!(
            or6(&m, &[1, 2, 3, 4, 5, 6]),
            [0x09, 0x02, 0x2b, 0x05, 0x07, 0x07]
        );
        assert_eq!(trunc6(&m), [0x08, 0x00, 0x2b, 0, 0, 0]);

        let m8 = expand6to8(&m);
        assert_eq!(trunc8(&m8), [0x08, 0x00, 0x2b, 0, 0, 0, 0, 0]);
        assert_eq!(narrow8to6(&m8)?, m);

        let set = set7bit(&expand6to8(&[0x00, 0x08, 0x2b, 0x01, 0x02, 0x03]));
        assert_eq!(format8(&set), "02:08:2b:ff:fe:01:02:03");

        Ok(())
    }
}
