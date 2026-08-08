//! `uuid`: input parsing, canonical output, comparison, and the bit layout of
//! the generated versions.
//!
//! Everything here is pure: the version 4 and version 7 builders take their
//! randomness and their instant as arguments, so the clock and the RNG live one
//! layer up, in the executor.
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

/// The largest instant a version 7 UUID can carry: the timestamp field is
/// 48 bits of milliseconds since the Unix epoch, which runs out in year 10889.
pub const V7_MAX_UNIX_MS: u64 = (1 << 48) - 1;

/// Microseconds from the Gregorian epoch (1582-10-15, where a version 1
/// timestamp counts from) to the Unix epoch.
const GREGORIAN_UNIX_MICROS: i64 = 12_219_292_800_000_000;

/// Stamp the version nibble and the RFC 9562 variant bits into 16 bytes that
/// are otherwise already filled: `version` into the high nibble of byte 6, and
/// `10` into the two high bits of byte 8.
fn stamp(mut b: [u8; 16], version: u8) -> [u8; 16] {
    b[6] = (b[6] & 0x0f) | (version << 4);
    b[8] = (b[8] & 0x3f) | 0x80;
    b
}

/// A version 4 UUID: 122 random bits around the version and variant markers.
pub fn build_v4(random: [u8; 16]) -> [u8; 16] {
    stamp(random, 4)
}

/// A version 7 UUID (RFC 9562 §5.7): a 48-bit Unix millisecond timestamp, then
/// 12 bits of `rand_a`, then 62 random bits.
///
/// PostgreSQL fills `rand_a` with the sub-millisecond part of the clock rather
/// than with randomness — RFC 9562 §6.2 "Replace Leftmost Random Bits with
/// Increased Clock Precision" — so two values from the same millisecond still
/// sort in generation order. See [`sub_ms_fraction`]. Bits of `unix_ms` above
/// the 48th are dropped; callers range-check first (see [`V7_MAX_UNIX_MS`]).
pub fn build_v7(unix_ms: u64, rand_a: u16, rand_b: [u8; 8]) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&unix_ms.to_be_bytes()[2..]);
    b[6] = ((rand_a >> 8) & 0x0f) as u8;
    b[7] = (rand_a & 0xff) as u8;
    b[8..].copy_from_slice(&rand_b);
    stamp(b, 7)
}

/// The 12-bit `rand_a` payload for an instant `sub_ns` nanoseconds from the
/// epoch: the fraction of a millisecond it falls at, scaled to 4096.
///
/// Nanoseconds, not microseconds: the 4096 steps are ~244ns apart, so a
/// microsecond reading reaches only 1000 of them while PostgreSQL's `rand_a`
/// is spread over all 4096. A host whose clock is coarser than that simply
/// never produces the values in between; nothing depends on it for
/// correctness, only for how soon a burst has to borrow from the next
/// millisecond.
pub fn sub_ms_fraction_nanos(sub_ns: i128) -> u16 {
    ((sub_ns.rem_euclid(1_000_000) * 4096) / 1_000_000) as u16
}

/// True when the two high bits of byte 8 are `10`, the RFC 9562 variant. The
/// extract functions answer NULL for anything else, since the version nibble
/// only means what RFC 9562 says it means inside that variant.
fn is_rfc_variant(b: &[u8; 16]) -> bool {
    b[8] & 0xc0 == 0x80
}

/// `uuid_extract_version`: the version nibble, or `None` when the value is not
/// an RFC 9562 variant.
pub fn extract_version(b: &[u8; 16]) -> Option<i16> {
    is_rfc_variant(b).then(|| (b[6] >> 4) as i16)
}

/// `uuid_extract_timestamp`: the instant a version 1 or version 7 UUID carries,
/// as microseconds since the Unix epoch. `None` for every other version, and
/// for a non-RFC-9562 variant.
///
/// Version 7 resolves to whole milliseconds: the sub-millisecond `rand_a`
/// bits are not folded back in, matching what PostgreSQL reports.
pub fn extract_timestamp_unix_micros(b: &[u8; 16]) -> Option<i64> {
    match extract_version(b)? {
        1 => {
            // A 60-bit count of 100ns intervals since 1582-10-15, stored
            // low-half first: time_low, time_mid, then the low 12 bits of the
            // version field.
            let time_low = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64;
            let time_mid = u16::from_be_bytes([b[4], b[5]]) as u64;
            let time_hi = (u16::from_be_bytes([b[6], b[7]]) & 0x0fff) as u64;
            let ticks = (time_hi << 48) | (time_mid << 32) | time_low;
            Some((ticks / 10) as i64 - GREGORIAN_UNIX_MICROS)
        }
        7 => {
            let ms = u64::from_be_bytes([0, 0, b[0], b[1], b[2], b[3], b[4], b[5]]);
            Some(ms as i64 * 1000)
        }
        _ => None,
    }
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
    fn roundtrip_canonical() -> anyhow::Result<()> {
        assert_eq!(parse(CANON)?, BYTES);
        assert_eq!(format(&BYTES), CANON);

        Ok(())
    }

    #[test]
    fn accepts_input_variants() -> anyhow::Result<()> {
        // No hyphens, braces, uppercase — all normalize to the canonical form.
        assert_eq!(parse("a0eebc999c0b4ef8bb6d6bb9bd380a11")?, BYTES);
        assert_eq!(parse("{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}")?, BYTES);
        assert_eq!(parse("A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11")?, BYTES);

        Ok(())
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "",
            "a0eebc99",                              // too short
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11-", // trailing junk
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a1z",  // non-hex
            "{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11", // unmatched brace
            "a0-eebc999c0b4ef8bb6d6bb9bd380a11",     // hyphen after even byte index
        ] {
            let e =
                parse(bad).expect_err("not 32 hex digits with only the punctuation uuid_in allows");
            assert_eq!(e.sqlstate, "22P02", "{bad}");
            assert_eq!(
                e.message,
                format!("invalid input syntax for type uuid: \"{bad}\"")
            );
        }
    }

    /// The instant both RFC 9562 test vectors below encode:
    /// 2022-02-22 14:22:22 GMT-05:00.
    const RFC_VECTOR_UNIX_MICROS: i64 = 1_645_557_742_000_000;

    #[test]
    fn builds_v4() -> anyhow::Result<()> {
        let b = build_v4([0xff; 16]);
        assert_eq!(extract_version(&b), Some(4));
        assert_eq!(
            b[6], 0x4f,
            "version nibble replaces the high half of byte 6"
        );
        assert_eq!(b[8], 0xbf, "variant bits replace the high two of byte 8");
        // Nothing else moved.
        assert_eq!(format(&b), "ffffffff-ffff-4fff-bfff-ffffffffffff");
        // A zero draw still carries the markers.
        assert_eq!(
            format(&build_v4([0; 16])),
            "00000000-0000-4000-8000-000000000000"
        );

        Ok(())
    }

    #[test]
    fn builds_v7() -> anyhow::Result<()> {
        // The RFC 9562 v7 vector, rebuilt from its parts.
        let b = build_v7(
            0x017f_22e2_79b0,
            0x0cc3,
            [0x98, 0xc4, 0xdc, 0x0c, 0x0c, 0x07, 0x39, 0x8f],
        );
        assert_eq!(format(&b), "017f22e2-79b0-7cc3-98c4-dc0c0c07398f");
        assert_eq!(extract_version(&b), Some(7));
        assert_eq!(
            extract_timestamp_unix_micros(&b),
            Some(RFC_VECTOR_UNIX_MICROS)
        );

        // The timestamp field's two ends round-trip.
        for ms in [0, 1, V7_MAX_UNIX_MS] {
            let b = build_v7(ms, 0, [0; 8]);
            assert_eq!(
                extract_timestamp_unix_micros(&b),
                Some(ms as i64 * 1000),
                "{ms}"
            );
        }
        // rand_a occupies exactly 12 bits and does not reach the version.
        let b = build_v7(0, 0xffff, [0; 8]);
        assert_eq!(format(&b), "00000000-0000-7fff-8000-000000000000");

        Ok(())
    }

    #[test]
    fn extracts_v1_vector() -> anyhow::Result<()> {
        let b = parse("C232AB00-9414-11EC-B3C8-9F6BDECED846")?;
        assert_eq!(extract_version(&b), Some(1));
        assert_eq!(
            extract_timestamp_unix_micros(&b),
            Some(RFC_VECTOR_UNIX_MICROS)
        );

        Ok(())
    }

    #[test]
    fn extract_rejects_non_rfc_variant() -> anyhow::Result<()> {
        // Variant bits `0001`: the "1" in the version position is not a
        // version, so both extractors decline.
        let b = parse("11111111-1111-1111-1111-111111111111")?;
        assert_eq!(extract_version(&b), None);
        assert_eq!(extract_timestamp_unix_micros(&b), None);

        // Same bytes with the variant fixed: now the version reads.
        let b = parse("11111111-1111-5111-8111-111111111111")?;
        assert_eq!(extract_version(&b), Some(5));

        Ok(())
    }

    #[test]
    fn only_v1_and_v7_carry_a_timestamp() -> anyhow::Result<()> {
        for version in [0u8, 2, 3, 4, 5, 6, 8, 15] {
            let b = stamp([0x88; 16], version);
            assert_eq!(extract_version(&b), Some(version as i16));
            assert_eq!(extract_timestamp_unix_micros(&b), None, "version {version}");
        }
        for version in [1u8, 7] {
            let b = stamp([0x88; 16], version);
            assert!(extract_timestamp_unix_micros(&b).is_some(), "{version}");
        }

        Ok(())
    }

    #[test]
    fn sub_ms_fraction_tracks_the_clock() {
        assert_eq!(sub_ms_fraction_nanos(0), 0);
        assert_eq!(sub_ms_fraction_nanos(999_999), 4095);
        // Whole milliseconds fall away, so the fraction of an absolute instant
        // is the fraction of its remainder.
        assert_eq!(
            sub_ms_fraction_nanos(1_234_567_890),
            sub_ms_fraction_nanos(567_890)
        );
        // A negative instant (before 1970) still lands in 0..1_000_000.
        assert_eq!(sub_ms_fraction_nanos(-1), sub_ms_fraction_nanos(999_999));

        let mut prev = 0;
        for us in 0..1000 {
            let f = sub_ms_fraction_nanos(us * 1000);
            assert!(f >= prev, "fraction must not go backwards at {us}us");
            assert!(f < 4096, "fraction must fit 12 bits at {us}us");
            prev = f;
        }
    }

    /// Every one of the 4096 `rand_a` values must be reachable. PostgreSQL's
    /// are spread over all of them; a microsecond-resolution clock can only
    /// produce 1000, so this is what pins the nanosecond source.
    #[test]
    fn sub_ms_fraction_covers_all_4096_steps() {
        // The first nanosecond that reaches `step` is the ceiling of the
        // scaled boundary; the floor would land one step short.
        let first_ns_of = |step: i128| (step * 1_000_000 + 4095) / 4096;
        for step in 0..4096i128 {
            assert_eq!(
                sub_ms_fraction_nanos(first_ns_of(step)),
                step as u16,
                "step {step} must be reachable"
            );
        }
    }
}
