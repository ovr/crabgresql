//! MD5 message-digest (RFC 1321).
//!
//! The digest itself comes from the `md-5` crate (RustCrypto); what is left
//! here is the hex rendering that reproduces PG's observable `md5()` output —
//! the 32-character lowercase digest — as pinned by the `md5` regression test.
//! No PG C source is consulted.

use crabgresql_types::hex;
use md5::{Digest, Md5};

/// The lowercase 32-character hex MD5 digest of `data`.
pub fn md5_hex(data: &[u8]) -> String {
    // `digest` 0.11 returns a `hybrid_array::Array`, which — unlike the
    // `generic-array` of 0.10 — has no `LowerHex`, so the nibbles are spelled
    // out by hand.
    let mut out = String::with_capacity(32);
    for byte in Md5::digest(data) {
        out.push(hex::lo(byte >> 4));
        out.push(hex::lo(byte & 0x0f));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::md5_hex;

    #[test]
    fn rfc1321_vectors() {
        // The seven RFC 1321 / md5.sql test vectors.
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"a"), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"message digest"),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
        assert_eq!(
            md5_hex(b"abcdefghijklmnopqrstuvwxyz"),
            "c3fcd3d76192e4007dfb496cca67e13b"
        );
        assert_eq!(
            md5_hex(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"),
            "d174ab98d277d9f5a5611c2c9f419d9f"
        );
        assert_eq!(
            md5_hex(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            ),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }
}
