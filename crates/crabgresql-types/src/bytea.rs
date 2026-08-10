//! The `bytea_output` GUC: which of `byteaout`'s two renderings a `bytea`
//! takes on the way out.
//!
//! Only the *output* side is a choice. `byteain` reads both forms whatever the
//! setting is — see `cast::byteain` — which is what makes `escape` output still
//! round-trip through a `hex` session.

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
    /// backslash, everything else three-digit octal. Produced by
    /// [`crate::text::encode_escape`], which is the same rendering
    /// `encode(bytea, 'escape')` returns.
    Escape,
}

/// The values `SET bytea_output` accepts, in the order and spelling PG's HINT
/// lists them — which is PostgreSQL's declaration order, not alphabetical
/// coincidence.
pub const BYTEA_OUTPUT_VALUES: &str = "escape, hex";

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
