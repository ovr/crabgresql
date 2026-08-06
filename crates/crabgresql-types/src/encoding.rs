//! PostgreSQL's character-set encoding numbering, as `pg_encoding_to_char` and
//! `pg_char_to_encoding` expose it.
//!
//! Clean-room (see AGENTS.md): the number → name mapping and the alias list are
//! *observable* through those two functions, and both were transcribed from a
//! stock PostgreSQL 18.4 (`SELECT i, pg_encoding_to_char(i) FROM
//! generate_series(0, 45) i`, then one probe per alias), not from upstream
//! source.
//!
//! This server speaks only UTF8 — [`UTF8`] is the one encoding a connection can
//! actually use. The rest of the table exists because the numbering is part of
//! the wire-visible catalog: `pg_database.encoding` is a number, and a client
//! turns it into a name through here.

/// The encoding number of `UTF8`, the only encoding this server serves.
pub const UTF8: i32 = 6;

/// PostgreSQL's `pg_enc` numbering, 0..=41 as of 18.
///
/// The order *is* the data: index is the encoding number, and a client that
/// reads `pg_database.encoding` resolves it positionally.
const ENCODING_NAMES: [&str; 42] = [
    "SQL_ASCII",
    "EUC_JP",
    "EUC_CN",
    "EUC_KR",
    "EUC_TW",
    "EUC_JIS_2004",
    "UTF8",
    "MULE_INTERNAL",
    "LATIN1",
    "LATIN2",
    "LATIN3",
    "LATIN4",
    "LATIN5",
    "LATIN6",
    "LATIN7",
    "LATIN8",
    "LATIN9",
    "LATIN10",
    "WIN1256",
    "WIN1258",
    "WIN866",
    "WIN874",
    "KOI8R",
    "WIN1251",
    "WIN1252",
    "ISO_8859_5",
    "ISO_8859_6",
    "ISO_8859_7",
    "ISO_8859_8",
    "WIN1250",
    "WIN1253",
    "WIN1254",
    "WIN1255",
    "WIN1257",
    "KOI8U",
    "SJIS",
    "BIG5",
    "GBK",
    "UHC",
    "GB18030",
    "JOHAB",
    "SHIFT_JIS_2004",
];

/// Names that are not in [`ENCODING_NAMES`] but that
/// `pg_char_to_encoding` still answers for, already [`clean`]ed.
///
/// Only the ones cleaning does not already produce: `win1252` reaches WIN1252
/// on its own, so it is not listed, while `windows1252` is a genuine alias.
const ALIASES: &[(&str, i32)] = &[
    ("abc", 19),
    ("alt", 20),
    // The `LATIN*` encodings answer to their ISO-8859 numbers too. Note the
    // numbering is not the LATIN one: ISO-8859-5..8 are the Cyrillic/Arabic/
    // Greek/Hebrew sets, which have their own `ISO_8859_*` table entries, while
    // ISO-8859-9 is LATIN5.
    ("iso88591", 8),
    ("iso885910", 13),
    ("iso885913", 14),
    ("iso885914", 15),
    ("iso885915", 16),
    ("iso885916", 17),
    ("iso88592", 9),
    ("iso88593", 10),
    ("iso88594", 11),
    ("iso88599", 12),
    ("koi8", 22),
    ("mskanji", 35),
    ("shiftjis", 35),
    ("tcvn", 19),
    ("tcvn5712", 19),
    ("unicode", 6),
    ("vscii", 19),
    ("win", 23),
    ("windows1250", 29),
    ("windows1251", 23),
    ("windows1252", 24),
    ("windows1253", 30),
    ("windows1254", 31),
    ("windows1255", 32),
    ("windows1256", 18),
    ("windows1257", 33),
    ("windows1258", 19),
    ("windows866", 20),
    ("windows874", 21),
    ("windows936", 37),
    ("windows949", 38),
    ("windows950", 36),
];

/// `pg_encoding_to_char`: the name of encoding number `n`.
///
/// An out-of-range number is the **empty string**, not NULL — verified against
/// PostgreSQL 18.4 for 42, 999 and −1. The function is STRICT, so a NULL
/// argument is short-circuited by the caller and never reaches here.
pub fn encoding_to_char(n: i32) -> &'static str {
    usize::try_from(n)
        .ok()
        .and_then(|i| ENCODING_NAMES.get(i))
        .copied()
        .unwrap_or("")
}

/// `pg_char_to_encoding`: the number of the encoding `name` denotes, or `-1`
/// for a name no encoding answers to (including the empty string).
pub fn char_to_encoding(name: &str) -> i32 {
    let key = clean(name);
    if let Some(i) = ENCODING_NAMES.iter().position(|n| clean(n) == key) {
        // Every name in the table cleans to something non-empty, so an empty
        // key cannot match one — `pg_char_to_encoding('')` is -1.
        return i as i32;
    }
    ALIASES
        .iter()
        .find(|(alias, _)| *alias == key)
        .map_or(-1, |(_, n)| *n)
}

/// PostgreSQL's normalization before an encoding-name lookup: lower-case, then
/// drop every non-alphanumeric character. `'UTF-8'`, `'utf8'` and `'  UTF8 '`
/// all reach `utf8`, and `'latin-1'` reaches `latin1`.
fn clean(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pair is a bijection over the whole table — the property a client
    /// relies on when it round-trips `pg_database.encoding` through a name.
    #[test]
    fn the_two_directions_are_inverses() {
        for (i, name) in ENCODING_NAMES.iter().enumerate() {
            let n = i as i32;
            assert_eq!(encoding_to_char(n), *name);
            assert_eq!(char_to_encoding(name), n, "{name}");
        }
    }

    /// Out-of-range is the empty string and an unknown name is −1, neither of
    /// them NULL. Values probed on PostgreSQL 18.4.
    #[test]
    fn misses_report_sentinels_not_null() {
        for n in [-1, 42, 999, i32::MIN, i32::MAX] {
            assert_eq!(encoding_to_char(n), "", "{n}");
        }
        for name in ["nosuch", "", "  ", "koi"] {
            assert_eq!(char_to_encoding(name), -1, "{name:?}");
        }
    }

    /// Punctuation and case are ignored, and PostgreSQL's alias table holds.
    /// Every expectation here was probed, including that `koi` is *not* an
    /// alias for KOI8R though `koi8` is.
    #[test]
    fn names_are_matched_loosely() {
        for (name, want) in [
            ("UTF8", 6),
            ("utf8", 6),
            ("UTF-8", 6),
            ("  UTF8 ", 6),
            ("unicode", 6),
            ("latin-1", 8),
            ("iso8859_1", 8),
            ("ISO-8859-5", 25),
            ("windows1252", 24),
            ("win1252", 24),
            ("mskanji", 35),
            ("sjis", 35),
            ("alt", 20),
            ("koi8", 22),
            ("euc_cn", 2),
        ] {
            assert_eq!(char_to_encoding(name), want, "{name}");
        }
    }

    #[test]
    fn utf8_is_six() {
        assert_eq!(encoding_to_char(UTF8), "UTF8");
    }
}
