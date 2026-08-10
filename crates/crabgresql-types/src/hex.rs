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

/// The lowercase hex digit for a nibble. Values above 15 are not expected;
/// callers mask first.
pub fn lo(nibble: u8) -> char {
    char::from(match nibble {
        0..=9 => b'0' + nibble,
        _ => b'a' + (nibble - 10),
    })
}
