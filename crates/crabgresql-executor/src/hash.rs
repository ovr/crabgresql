//! SHA-2 and CRC digests behind PG's core `sha224`…`sha512`, `crc32`, `crc32c`.
//!
//! Thin wrappers over audited third-party crates (`sha2`, `crc32fast`,
//! `crc32c`), the way [`crate::md5`] wraps `md-5`: AGENTS.md permits
//! third-party crates with compatible licenses, and a hash nobody
//! reimplements is a hash nobody gets subtly wrong.

use sha2::Digest;

pub fn sha224(bytes: &[u8]) -> Vec<u8> {
    sha2::Sha224::digest(bytes).to_vec()
}

pub fn sha256(bytes: &[u8]) -> Vec<u8> {
    sha2::Sha256::digest(bytes).to_vec()
}

pub fn sha384(bytes: &[u8]) -> Vec<u8> {
    sha2::Sha384::digest(bytes).to_vec()
}

pub fn sha512(bytes: &[u8]) -> Vec<u8> {
    sha2::Sha512::digest(bytes).to_vec()
}

/// CRC-32 (the IEEE 802.3 / zlib polynomial). PG returns the unsigned 32-bit
/// checksum widened to `int8`, so values above `i32::MAX` stay positive.
pub fn crc32(bytes: &[u8]) -> i64 {
    i64::from(crc32fast::hash(bytes))
}

/// CRC-32C (Castagnoli), widened like [`crc32`].
pub fn crc32c(bytes: &[u8]) -> i64 {
    i64::from(crc32c::crc32c(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vectors PG's own `strings` regression test pins.
    const FOX: &[u8] = b"The quick brown fox jumps over the lazy dog.";

    fn hex(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for &byte in bytes {
            out.push(crabgresql_types::hex::lo(byte >> 4));
            out.push(crabgresql_types::hex::lo(byte & 0x0f));
        }
        out
    }

    #[test]
    fn sha2_matches_pg() {
        assert_eq!(
            hex(&sha224(b"")),
            "d14a028c2a3a2bc9476102bb288234c415a2b01f828ea62ac5b3e42f"
        );
        assert_eq!(
            hex(&sha224(FOX)),
            "619cba8e8e05826e9b8c519c0a5c68f4fb653e8a3d8aa04bb2c8cd4c"
        );
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(FOX)),
            "ef537f25c895bfa782526529a9b63d97aa631564d5d789c2b765448c8635fb6c"
        );
        assert_eq!(
            hex(&sha384(b"")),
            "38b060a751ac96384cd9327eb1b1e36a21fdb71114be07434c0cc7bf63f6e1da\
             274edebfe76f65fbd51ad2f14898b95b"
        );
        assert_eq!(
            hex(&sha384(FOX)),
            "ed892481d8272ca6df370bf706e4d7bc1b5739fa2177aae6c50e946678718fc6\
             7a7af2819a021c2fc34e91bdb63409d7"
        );
        assert_eq!(
            hex(&sha512(b"")),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce\
             47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
        assert_eq!(
            hex(&sha512(FOX)),
            "91ea1245f20d46ae9a037a989f54f1f790f0a47607eeb8a14d12890cea77a1bb\
             c6c7ed9cf205e67b7f2b8fd4c7dfd3a7a8617e45f3c463d481c7e586c39ac1ed"
        );
    }

    #[test]
    fn crc_matches_pg() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(FOX), 1368401385);
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(FOX), 419469235);
    }

    /// Lengths straddling the 128-byte block the accelerated CRC paths consume
    /// at a time, where a mis-sliced tail would go unnoticed on short inputs.
    /// `crc32` and `crc32c` come from different crates with different kernels,
    /// so both need the sweep. The 129-byte crc32c value also pins the
    /// widening: above `i32::MAX`, it would come back negative as `i32`.
    #[test]
    fn crc_block_boundaries() {
        assert_eq!(crc32c(&b"A".repeat(127)), 291820082);
        assert_eq!(crc32c(&b"A".repeat(128)), 816091258);
        assert_eq!(crc32c(&b"A".repeat(129)), 4213642571);
        assert_eq!(crc32c(&b"A".repeat(800)), 3134039419);
        assert_eq!(crc32(&b"A".repeat(127)), 3435576289);
        assert_eq!(crc32(&b"A".repeat(128)), 68717278);
        assert_eq!(crc32(&b"A".repeat(129)), 2998303186);
        assert_eq!(crc32(&b"A".repeat(800)), 213225760);
    }

    /// Every vector above is 44 bytes or shorter, which fits in a single
    /// SHA-512 block — so nothing there exercises multi-block compression or
    /// the length encoding in the final block.
    #[test]
    fn sha2_multi_block() {
        let long = b"A".repeat(1000);
        assert_eq!(
            hex(&sha256(&long)),
            "c2e686823489ced2017f6059b8b239318b6364f6dcd835d0a519105a1eadd6e4"
        );
        assert_eq!(
            hex(&sha512(&long)),
            "329c52ac62d1fe731151f2b895a00475445ef74f50b979c6f7bb7cae349328c1\
             d4cb4f7261a0ab43f936a24b000651d4a824fcdd577f211aef8f806b16afe8af"
        );
    }
}
