//! ASCII hex digits, shared by every type whose text form is hex — uuid,
//! macaddr, bytea and the `\x` escapes in string literals.

/// The value of a single hex digit, either case. `None` if `c` is not one.
pub fn val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// The lower-case hex digits, indexed by nibble. A table rather than
/// `format!("{b:02x}")`: these run per byte of every `bytea` on the default
/// output path, where the per-byte `String` that `format!` allocates measured
/// 33x slower than two `push`es (37.2 vs 1.1 ms per MB).
const DIGITS: &[u8; 16] = b"0123456789abcdef";

/// The lowercase hex digit for a nibble. Values above 15 are not expected;
/// callers mask first.
pub fn lo(nibble: u8) -> char {
    DIGITS[(nibble & 0x0f) as usize] as char
}

/// Append a byte as two lower-case hex digits.
pub fn push(out: &mut String, byte: u8) {
    out.push(lo(byte >> 4));
    out.push(lo(byte));
}

/// A byte buffer as lower-case hex, no prefix or separator. Callers that
/// interleave separators (uuid's dashes, macaddr's colons) push per byte
/// instead.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        push(&mut out, byte);
    }
    out
}

/// SQLSTATE + message for malformed hex input. The same shape as `TextError`
/// and `CastError`, so each caller copies the two fields into its own type
/// without a translation layer. Spelled out rather than taken from either,
/// which would make this module depend on one of its own consumers.
pub struct HexError {
    pub sqlstate: &'static str,
    pub message: String,
}

/// Decode a run of hex digit pairs into bytes.
///
/// Shared by `decode(…, 'hex')` and `byteain`'s `\x` form because PostgreSQL
/// runs both through one decoder, so their messages and their whitespace rule
/// have to agree. Whitespace separates pairs and is skipped; *inside* a pair it
/// is an invalid digit like any other byte, which falls out of upstream reading
/// both nibbles in a single step. Only the four characters upstream skips count
/// as whitespace — `\x0B` and `\x0C` are digits to reject, unlike what
/// `is_ascii_whitespace` would say.
pub fn decode(s: &str) -> Result<Vec<u8>, HexError> {
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut hi: Option<u8> = None;
    for ch in s.chars() {
        if matches!(ch, ' ' | '\n' | '\t' | '\r') && hi.is_none() {
            continue;
        }
        // A non-ASCII character cannot be a digit, and is echoed whole: PG
        // measures the character, not the byte, so `é` must not come back as
        // the first half of its UTF-8 encoding.
        let Some(v) = u8::try_from(ch).ok().and_then(val) else {
            return Err(HexError {
                sqlstate: INVALID_PARAMETER_VALUE,
                message: format!("invalid hexadecimal digit: \"{ch}\""),
            });
        };
        match hi.take() {
            None => hi = Some(v),
            Some(h) => out.push((h << 4) | v),
        }
    }
    if hi.is_some() {
        return Err(HexError {
            sqlstate: INVALID_PARAMETER_VALUE,
            message: "invalid hexadecimal data: odd number of digits".into(),
        });
    }
    Ok(out)
}

/// `22023` — the code PG's hex decoder raises, for both the bad-digit and the
/// odd-count case.
const INVALID_PARAMETER_VALUE: &str = "22023";
