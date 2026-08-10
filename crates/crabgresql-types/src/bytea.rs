//! The `bytea_output` GUC: which of `byteaout`'s two renderings a `bytea`
//! takes on the way out.
//!
//! Only the *output* side is a choice. `byteain` reads both forms whatever the
//! setting is — see `cast::byteain` — which is what makes `escape` output still
//! round-trip through a `hex` session.

use crate::hex;

/// The `bytea_output` GUC.
///
/// Input is unaffected: this picks between `\x4142` and `AB` on the way out,
/// and both read back as the same bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ByteaOutput {
    /// `\x0061` — PostgreSQL's default since 9.0.
    #[default]
    Hex,
    /// `\000a` — the pre-9.0 form: printable ASCII verbatim, a doubled
    /// backslash, everything else three-digit octal. Note that
    /// `encode(bytea, 'escape')` is a *different* rule despite the shared name
    /// — see `escape_out` in this module for the transcript separating them.
    Escape,
}

impl ByteaOutput {
    /// Parse a `SET bytea_output` value. Names are case-insensitive in PG, and
    /// nothing more: `SET bytea_output TO ' hex '` is
    /// `invalid value for parameter` there, so the padding is not trimmed away.
    pub fn from_name(name: &str) -> Option<ByteaOutput> {
        match name.to_ascii_lowercase().as_str() {
            "hex" => Some(ByteaOutput::Hex),
            "escape" => Some(ByteaOutput::Escape),
            _ => None,
        }
    }

    /// The canonical lower-case spelling `SHOW bytea_output` prints.
    pub fn name(self) -> &'static str {
        match self {
            ByteaOutput::Hex => "hex",
            ByteaOutput::Escape => "escape",
        }
    }
}

/// `byteaout` under `bytea_output = hex`: `\x` and two lower-case hex digits
/// per byte. PostgreSQL's default since 9.0.
pub(crate) fn hex_out(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("\\x");
    for &b in bytes {
        hex::push(&mut out, b);
    }
    out
}

/// `byteaout` under `bytea_output = escape`: a doubled backslash, printable
/// ASCII (`0x20..=0x7e`) verbatim, and every other byte as `\ooo`.
///
/// `0x7e` is the last byte that prints as itself and `0x7f` the first that does
/// not, so an `is_ascii_graphic`-style test would be wrong at both ends.
///
/// Not to be confused with `encode(bytea, 'escape')`, which passes the C0
/// controls and `0x7f` through untouched — see [`crate::text::encode_escape`]
/// for the transcript that separates them.
pub(crate) fn escape_out(bytes: &[u8]) -> String {
    // Four bytes per input byte is the worst case (`\ooo`); one allocation up
    // front beats the log2(n) reallocation passes a growing buffer would pay on
    // a large value.
    let mut out = String::with_capacity(bytes.len() * 4);
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(b as char),
            _ => crate::text::push_octal_escape(&mut out, b),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for style in [ByteaOutput::Hex, ByteaOutput::Escape] {
            assert_eq!(ByteaOutput::from_name(style.name()), Some(style));
        }
    }

    #[test]
    fn parsing_is_case_insensitive_but_does_not_trim() {
        assert_eq!(ByteaOutput::from_name("HEX"), Some(ByteaOutput::Hex));
        assert_eq!(ByteaOutput::from_name("EsCaPe"), Some(ByteaOutput::Escape));
        // PG rejects the padded spellings; so must we.
        assert_eq!(ByteaOutput::from_name(" hex "), None);
        assert_eq!(ByteaOutput::from_name("bogus"), None);
    }

    /// PG boots at `hex`.
    #[test]
    fn default_is_hex() {
        assert_eq!(ByteaOutput::default(), ByteaOutput::Hex);
    }
}
