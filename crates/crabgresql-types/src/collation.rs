//! Collations: the registry of known collation objects and the string
//! comparison they drive.
//!
//! A collation decides how two strings order. PostgreSQL attaches one to every
//! expression of a collatable type (see [`crate::PgType::is_collatable`]) and
//! uses it for `<`/`>`, `ORDER BY`, and B-tree index order.
//!
//! Every collation here is **deterministic**, PostgreSQL's default: when the
//! locale-aware comparison reports equality, the byte comparison breaks the tie,
//! so no two distinct strings ever compare equal. That is what lets equality and
//! hashing stay bytewise everywhere (`agg::hash_key`, `agg::keys_equal`) while
//! only the *ordering* varies by collation. Non-deterministic collations, which
//! would make `'a' = 'A'` true and force collation-aware hashing, are not
//! supported.
//!
//! Two provider families are seeded:
//!
//! * byte-order collations (`default`, `C`, `POSIX`, `ucs_basic`) compare by
//!   raw UTF-8 bytes, which for those locales is exactly PostgreSQL's order;
//! * ICU collations (`unicode` and the `<locale>-x-icu` set) compare by the
//!   Unicode Collation Algorithm tailored to the locale, via ICU4X.
//!
//! `default` is the database collation. This build initializes its cluster with
//! byte ordering (as `initdb --locale=C` does), so `default` compares by bytes.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::LazyLock;

use icu_collator::CollatorBorrowed;
use icu_collator::CollatorPreferences;
use icu_collator::options::CollatorOptions;
use icu_locale_core::Locale;

use crate::PgType;

/// `pg_collation.oid` of the `default` collation — the database's own collation,
/// and the collation of any expression that does not derive a more specific one.
pub const DEFAULT_COLLATION_OID: u32 = 100;
/// `pg_collation.oid` of `C`.
pub const C_COLLATION_OID: u32 = 950;
/// `pg_collation.oid` of `POSIX`.
pub const POSIX_COLLATION_OID: u32 = 951;
/// `pg_collation.oid` of `ucs_basic`.
pub const UCS_BASIC_COLLATION_OID: u32 = 962;
/// `pg_collation.oid` of `unicode` (the ICU root locale).
pub const UNICODE_COLLATION_OID: u32 = 963;

/// Base for the OIDs of the seeded `<locale>-x-icu` collations. A real cluster
/// assigns these at `initdb` time from the regular OID counter; this build hands
/// out stable synthetic OIDs instead, well above every builtin and below the
/// relation/enum bases, so a persisted column collation survives a restart.
const FIRST_ICU_COLLATION_OID: u32 = 0xC000_0000;

/// PostgreSQL's `PG_UTF8` encoding number, for `collencoding`.
const PG_UTF8: i32 = 6;
/// `collencoding` for a collation usable in any database encoding.
const ANY_ENCODING: i32 = -1;

/// The provider backing a collation, mirroring `pg_collation.collprovider`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    /// `d`: the database's default collation.
    Default,
    /// `c`: the C library. Here that means byte ordering.
    Libc,
    /// `b`: PostgreSQL's own built-in provider, whose `C` locale is byte
    /// ordering with no dependency on the platform.
    Builtin,
    /// `i`: ICU, i.e. the Unicode Collation Algorithm tailored to a locale.
    Icu,
}

impl Provider {
    /// The single-character spelling stored in `pg_collation.collprovider`.
    pub fn as_char(self) -> char {
        match self {
            Provider::Default => 'd',
            Provider::Libc => 'c',
            Provider::Builtin => 'b',
            Provider::Icu => 'i',
        }
    }
}

/// One collation object: a `pg_collation` row plus the comparison it selects.
#[derive(Clone, Copy, Debug)]
pub struct CollationDef {
    pub oid: u32,
    /// `collname`, as written in `COLLATE "..."`. Matched case-sensitively, the
    /// way a quoted identifier is.
    pub name: &'static str,
    pub provider: Provider,
    /// Always `true`: only deterministic collations are supported (see the
    /// module docs).
    pub deterministic: bool,
    /// `collencoding`: the database encoding this collation is usable in, or
    /// [`ANY_ENCODING`] for any.
    pub encoding: i32,
    /// `collcollate`/`collctype`: the locale, set only for `Provider::Libc`.
    pub libc_locale: Option<&'static str>,
    /// `colllocale`: the locale, set for the builtin and ICU providers.
    pub locale: Option<&'static str>,
}

impl CollationDef {
    /// Whether this collation orders by ICU rather than by raw bytes.
    pub fn is_icu(&self) -> bool {
        self.provider == Provider::Icu
    }

    /// The ICU locale to build a collator for, or `None` when this collation
    /// orders by bytes.
    fn icu_locale(&self) -> Option<&'static str> {
        self.is_icu().then_some(self.locale).flatten()
    }
}

/// The ICU locales seeded as `<name>-x-icu` collations, spelled the way
/// PostgreSQL's `initdb` names them. Each entry is `(collname, ICU locale)`;
/// OIDs are assigned by position from [`FIRST_ICU_COLLATION_OID`], so entries
/// must only ever be appended — reordering or removing one would change the OID
/// of a collation already persisted in a table's catalog.
const ICU_LOCALES: &[(&str, &str)] = &[
    ("en-x-icu", "en"),
    ("en-US-x-icu", "en-US"),
    ("en-GB-x-icu", "en-GB"),
    ("de-x-icu", "de"),
    ("de-AT-x-icu", "de-AT"),
    ("fr-x-icu", "fr"),
    ("fr-CA-x-icu", "fr-CA"),
    ("es-x-icu", "es"),
    ("it-x-icu", "it"),
    ("pt-x-icu", "pt"),
    ("nl-x-icu", "nl"),
    ("sv-x-icu", "sv"),
    ("da-x-icu", "da"),
    ("nb-x-icu", "nb"),
    ("fi-x-icu", "fi"),
    ("is-x-icu", "is"),
    ("pl-x-icu", "pl"),
    ("cs-x-icu", "cs"),
    ("sk-x-icu", "sk"),
    ("hu-x-icu", "hu"),
    ("ro-x-icu", "ro"),
    ("hr-x-icu", "hr"),
    ("sl-x-icu", "sl"),
    ("et-x-icu", "et"),
    ("lv-x-icu", "lv"),
    ("lt-x-icu", "lt"),
    ("el-x-icu", "el"),
    ("ru-x-icu", "ru"),
    ("uk-x-icu", "uk"),
    ("tr-x-icu", "tr"),
    ("he-x-icu", "he"),
    ("ar-x-icu", "ar"),
    ("fa-x-icu", "fa"),
    ("hi-x-icu", "hi"),
    ("th-x-icu", "th"),
    ("vi-x-icu", "vi"),
    ("ja-x-icu", "ja"),
    ("ko-x-icu", "ko"),
    ("zh-x-icu", "zh"),
    ("zh-Hans-x-icu", "zh-Hans"),
    ("zh-Hant-x-icu", "zh-Hant"),
];

/// Every collation this build knows, in `pg_collation` row order.
pub static COLLATIONS: LazyLock<Vec<CollationDef>> = LazyLock::new(|| {
    let mut all = vec![
        CollationDef {
            oid: DEFAULT_COLLATION_OID,
            name: "default",
            provider: Provider::Default,
            deterministic: true,
            encoding: ANY_ENCODING,
            libc_locale: None,
            locale: None,
        },
        CollationDef {
            oid: C_COLLATION_OID,
            name: "C",
            provider: Provider::Libc,
            deterministic: true,
            encoding: ANY_ENCODING,
            libc_locale: Some("C"),
            locale: None,
        },
        CollationDef {
            oid: POSIX_COLLATION_OID,
            name: "POSIX",
            provider: Provider::Libc,
            deterministic: true,
            encoding: ANY_ENCODING,
            libc_locale: Some("POSIX"),
            locale: None,
        },
        // Defined by the SQL standard as ordering by code point, which for
        // UTF-8 is byte order. PostgreSQL ships it under the builtin provider.
        CollationDef {
            oid: UCS_BASIC_COLLATION_OID,
            name: "ucs_basic",
            provider: Provider::Builtin,
            deterministic: true,
            encoding: PG_UTF8,
            libc_locale: None,
            locale: Some("C"),
        },
        // The ICU root locale: the untailored Unicode Collation Algorithm.
        CollationDef {
            oid: UNICODE_COLLATION_OID,
            name: "unicode",
            provider: Provider::Icu,
            deterministic: true,
            encoding: ANY_ENCODING,
            libc_locale: None,
            locale: Some("und"),
        },
    ];
    all.extend(
        ICU_LOCALES
            .iter()
            .enumerate()
            .map(|(i, (name, locale))| CollationDef {
                oid: FIRST_ICU_COLLATION_OID + i as u32,
                name,
                provider: Provider::Icu,
                deterministic: true,
                encoding: ANY_ENCODING,
                libc_locale: None,
                locale: Some(locale),
            }),
    );
    all
});

/// The collation of values of `ty` — PostgreSQL's `pg_type.typcollation`, and
/// the collation a column of that type carries when it declares no `COLLATE`.
/// `0` for a type that is not collatable.
///
/// `name` is the one type PostgreSQL pins to `C` rather than the database
/// collation, so identifiers sort the same everywhere.
pub fn type_collation(ty: PgType) -> u32 {
    match ty {
        PgType::Name => C_COLLATION_OID,
        _ if ty.is_collatable() => DEFAULT_COLLATION_OID,
        _ => 0,
    }
}

/// The collation named `name`, or `None` if no such collation exists.
/// Collation names are case-sensitive, matching PostgreSQL's quoted-identifier
/// lookup (`COLLATE "C"` and `COLLATE "c"` are different names).
pub fn lookup_by_name(name: &str) -> Option<&'static CollationDef> {
    COLLATIONS.iter().find(|c| c.name == name)
}

/// The collation with OID `oid`, or `None` if it is not a known collation.
pub fn lookup_by_oid(oid: u32) -> Option<&'static CollationDef> {
    COLLATIONS.iter().find(|c| c.oid == oid)
}

/// The ICU collators for every ICU collation, built on first use of one.
///
/// Built eagerly for all ICU locales rather than lazily per locale so that a
/// comparison never has to take a lock: [`compare_str`] touches this map only
/// when the collation actually is an ICU one, so the byte-order collations
/// (`default`, `C`, `POSIX`, `ucs_basic`) never pay for it.
///
/// A locale whose collator fails to build is absent from the map and falls back
/// to byte order. That fallback is per-collation and permanent, so comparison
/// stays a consistent total order either way — which `sort_by` requires.
static ICU_COLLATORS: LazyLock<HashMap<u32, CollatorBorrowed<'static>>> = LazyLock::new(|| {
    COLLATIONS
        .iter()
        .filter_map(|def| {
            let tag = def.icu_locale()?;
            let locale: Locale = tag.parse().ok()?;
            let collator = CollatorBorrowed::try_new(
                CollatorPreferences::from(&locale),
                CollatorOptions::default(),
            )
            .ok()?;
            Some((def.oid, collator))
        })
        .collect()
});

/// Compare two strings under the collation `oid`, as a total order.
///
/// An unknown OID compares by bytes, so a collation dropped from the registry
/// between releases degrades to `C` rather than failing a query.
pub fn compare_str(oid: u32, a: &str, b: &str) -> Ordering {
    let icu = lookup_by_oid(oid)
        .filter(|def| def.is_icu())
        .and_then(|def| ICU_COLLATORS.get(&def.oid));
    match icu {
        // Deterministic: fall back to byte order when ICU sees no difference,
        // so distinct strings never compare equal.
        Some(collator) => collator
            .compare(a, b)
            .then_with(|| a.as_bytes().cmp(b.as_bytes())),
        None => a.as_bytes().cmp(b.as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_order_collations_compare_by_bytes() {
        // 'B' (0x42) sorts before 'a' (0x61) by byte value, unlike in any
        // language-aware order.
        for oid in [
            DEFAULT_COLLATION_OID,
            C_COLLATION_OID,
            POSIX_COLLATION_OID,
            UCS_BASIC_COLLATION_OID,
        ] {
            assert_eq!(compare_str(oid, "B", "a"), Ordering::Less);
        }
    }

    #[test]
    fn icu_collations_order_by_locale() {
        let en = lookup_by_name("en-US-x-icu").expect("en-US-x-icu is seeded");
        // ICU orders case-insensitively at the primary level, so 'a' < 'B'
        // where byte order gives the opposite.
        assert_eq!(compare_str(en.oid, "a", "B"), Ordering::Less);
        assert_eq!(compare_str(C_COLLATION_OID, "a", "B"), Ordering::Greater);
    }

    #[test]
    fn swedish_sorts_a_ring_after_z() {
        // The classic tailoring difference: in Swedish "å" is a distinct letter
        // after "z", while German treats it as a variant of "a".
        let sv = lookup_by_name("sv-x-icu").expect("sv-x-icu is seeded");
        let de = lookup_by_name("de-x-icu").expect("de-x-icu is seeded");
        assert_eq!(compare_str(sv.oid, "å", "z"), Ordering::Greater);
        assert_eq!(compare_str(de.oid, "å", "z"), Ordering::Less);
    }

    #[test]
    fn icu_comparison_is_deterministic() {
        // Strings ICU considers equal at every level still order by bytes, so
        // no two distinct strings compare equal.
        let en = lookup_by_name("en-US-x-icu").expect("en-US-x-icu is seeded");
        // U+0041 U+0301 (A + combining acute) vs U+00C1 (precomposed Á) are
        // canonically equivalent, so ICU alone would call them equal; the byte
        // tiebreak orders them (0x41… < 0xC3…) instead.
        let decomposed = "A\u{0301}";
        let precomposed = "\u{00C1}";
        assert_eq!(
            compare_str(en.oid, decomposed, precomposed),
            decomposed.as_bytes().cmp(precomposed.as_bytes())
        );
        assert_ne!(
            compare_str(en.oid, decomposed, precomposed),
            Ordering::Equal
        );
        // Equal only for genuinely identical strings.
        assert_eq!(compare_str(en.oid, "abc", "abc"), Ordering::Equal);
    }

    #[test]
    fn every_seeded_collation_has_a_unique_oid_and_name() {
        let mut oids: Vec<u32> = COLLATIONS.iter().map(|c| c.oid).collect();
        oids.sort_unstable();
        let count = oids.len();
        oids.dedup();
        assert_eq!(oids.len(), count, "collation OIDs collide");

        let mut names: Vec<&str> = COLLATIONS.iter().map(|c| c.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "collation names collide");
    }

    #[test]
    fn every_icu_collation_builds_a_collator() {
        for def in COLLATIONS.iter().filter(|c| c.is_icu()) {
            assert!(
                ICU_COLLATORS.contains_key(&def.oid),
                "no ICU collator for {}",
                def.name
            );
        }
    }

    #[test]
    fn unknown_collation_falls_back_to_byte_order() {
        assert!(lookup_by_oid(0).is_none());
        assert_eq!(compare_str(0, "B", "a"), Ordering::Less);
    }
}
