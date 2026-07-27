//! Every environment variable crabgresql reads, and every default that goes
//! with it.
//!
//! The names used to live as string literals at the point of use, which made
//! the set of knobs impossible to enumerate, document, or test: adding one was
//! a local edit nobody else could see. Here a name and its default sit next to
//! each other, [`ALL`] lists the whole set, and [`var`] is the only place in
//! the workspace that calls `std::env::var` for configuration.
//!
//! Cargo's own build-time variables (`OUT_DIR`, `CARGO_MANIFEST_DIR`,
//! `CARGO_TARGET_TMPDIR`) are deliberately absent — they are an interface with
//! the build system, not something a user configures.
//!
//! Unparseable values fall back to the default rather than failing startup: a
//! typo in a tuning knob should not keep a server from coming up.

use std::time::Duration;

/// TCP port the server listens on.
pub const PORT: &str = "CRABGRESQL_PORT";
/// Data directory the durable heap engine is opened in. Spelled `PGDATA` to
/// match PostgreSQL, since the same directory serves the same purpose.
pub const DATA_DIR: &str = "PGDATA";
/// `tracing` filter directives. Read by `tracing_subscriber`'s `EnvFilter`
/// rather than by this crate, so the name is here only to be documented.
pub const LOG_FILTER: &str = "RUST_LOG";
/// Per-relation buffered bytes that make one write buffer flush-eligible.
/// Accepts a `kB`/`MB`/`GB`/`TB` suffix — see [`bytes_or`].
pub const BUFFER_TABLE_SOFT_BYTES: &str = "CRABGRESQL_BUFFER_TABLE_SOFT_BYTES";
/// Buffered bytes across all relations that make *every* buffer eligible.
/// Accepts a `kB`/`MB`/`GB`/`TB` suffix — see [`bytes_or`].
pub const BUFFER_GLOBAL_HARD_BYTES: &str = "CRABGRESQL_BUFFER_GLOBAL_HARD_BYTES";
/// How long a write buffer may hold rows before being flushed anyway.
pub const BUFFER_MAX_AGE_MS: &str = "CRABGRESQL_BUFFER_MAX_AGE_MS";
/// How often the background flush worker looks for eligible buffers.
pub const BUFFER_TICK_MS: &str = "CRABGRESQL_BUFFER_TICK_MS";

/// One above PostgreSQL's 5432, so a local PostgreSQL can keep running.
pub const DEFAULT_PORT: u16 = 5433;
/// Used when neither `--data-dir` nor [`DATA_DIR`] is given.
pub const DEFAULT_DATA_DIR: &str = "./pgdata";
/// Used when [`LOG_FILTER`] is unset or unparseable.
pub const DEFAULT_LOG_FILTER: &str = "info";
/// Default for [`BUFFER_TABLE_SOFT_BYTES`].
pub const DEFAULT_BUFFER_TABLE_SOFT_BYTES: usize = 32 * 1024 * 1024;
/// Default for [`BUFFER_GLOBAL_HARD_BYTES`].
pub const DEFAULT_BUFFER_GLOBAL_HARD_BYTES: usize = 256 * 1024 * 1024;
/// Default for [`BUFFER_MAX_AGE_MS`].
pub const DEFAULT_BUFFER_MAX_AGE_MS: u64 = 60_000;
/// Default for [`BUFFER_TICK_MS`].
pub const DEFAULT_BUFFER_TICK_MS: u64 = 1_000;

/// A documented knob, for anything that wants to render the set rather than
/// read one value out of it (`--help`, the README table, tests).
#[derive(Clone, Copy, Debug)]
pub struct EnvVar {
    pub name: &'static str,
    /// The default rendered the way a user would type it.
    pub default: &'static str,
    pub help: &'static str,
}

/// Every environment variable in the list above, in the order a reader meets
/// them: server first, storage tuning after.
pub const ALL: &[EnvVar] = &[
    EnvVar {
        name: PORT,
        default: "5433",
        help: "TCP port to listen on",
    },
    EnvVar {
        name: DATA_DIR,
        default: DEFAULT_DATA_DIR,
        help: "data directory the durable heap engine is opened in",
    },
    EnvVar {
        name: LOG_FILTER,
        default: DEFAULT_LOG_FILTER,
        help: "tracing filter directives",
    },
    EnvVar {
        name: BUFFER_TABLE_SOFT_BYTES,
        default: "32MB",
        help: "per-relation buffered bytes that make one write buffer flush-eligible",
    },
    EnvVar {
        name: BUFFER_GLOBAL_HARD_BYTES,
        default: "256MB",
        help: "buffered bytes across all relations that make every buffer eligible",
    },
    EnvVar {
        name: BUFFER_MAX_AGE_MS,
        default: "60000",
        help: "how long a write buffer may hold rows before being flushed anyway",
    },
    EnvVar {
        name: BUFFER_TICK_MS,
        default: "1000",
        help: "how often the background flush worker looks for eligible buffers",
    },
];

/// The raw value of `name`, or `None` when it is unset, empty, or not UTF-8.
///
/// An empty value counts as unset: a shell that exports `FOO=` has said
/// nothing, and treating it as a parse failure would mean the same thing by a
/// longer route.
pub fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|raw| !raw.is_empty())
}

/// `name` read as a byte count, falling back to `fallback` when it is unset or
/// unusable.
///
/// The grammar is a run of decimal digits followed by an optional unit:
/// nothing or `B` for bytes, `kB`, `MB`, `GB`, `TB`. Units are binary
/// (1 kB = 1024 B) and matched case-insensitively, the trailing `B` is
/// optional, and surrounding space is ignored — `32MB`, `32 mb` and `32m` all
/// mean the same thing. Anything else, including a size that overflows
/// `usize`, is a value we cannot honor, so it yields `fallback`.
pub fn bytes_or(name: &str, fallback: usize) -> usize {
    bytes_raw(var(name).as_deref(), fallback)
}

/// `name` read as a count of milliseconds, falling back to `fallback`.
pub fn duration_ms_or(name: &str, fallback: Duration) -> Duration {
    duration_ms_raw(var(name).as_deref(), fallback)
}

/// The parsing half of [`bytes_or`], split out so it can be tested without
/// mutating the process environment.
fn bytes_raw(raw: Option<&str>, fallback: usize) -> usize {
    let Some(raw) = raw else {
        return fallback;
    };
    let raw = raw.trim();
    let digits_end = raw
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(raw.len());
    let (digits, unit) = raw.split_at(digits_end);
    match (digits.parse::<usize>(), byte_unit(unit)) {
        (Ok(count), Some(multiplier)) => count.checked_mul(multiplier).unwrap_or(fallback),
        _ => fallback,
    }
}

/// How many bytes `unit` stands for, or `None` when it names no unit we know.
fn byte_unit(unit: &str) -> Option<usize> {
    const KB: usize = 1024;
    let unit = unit.trim();
    [
        ("", 1),
        ("b", 1),
        ("k", KB),
        ("kb", KB),
        ("m", KB * KB),
        ("mb", KB * KB),
        ("g", KB * KB * KB),
        ("gb", KB * KB * KB),
        ("t", KB * KB * KB * KB),
        ("tb", KB * KB * KB * KB),
    ]
    .into_iter()
    .find(|(name, _)| unit.eq_ignore_ascii_case(name))
    .map(|(_, multiplier)| multiplier)
}

/// The parsing half of [`duration_ms_or`].
fn duration_ms_raw(raw: Option<&str>, fallback: Duration) -> Duration {
    match raw.and_then(|raw| raw.trim().parse::<u64>().ok()) {
        Some(millis) => Duration::from_millis(millis),
        None => fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_lists_every_name_once() {
        let declared = [
            PORT,
            DATA_DIR,
            LOG_FILTER,
            BUFFER_TABLE_SOFT_BYTES,
            BUFFER_GLOBAL_HARD_BYTES,
            BUFFER_MAX_AGE_MS,
            BUFFER_TICK_MS,
        ];
        let listed: Vec<&str> = ALL.iter().map(|entry| entry.name).collect();
        assert_eq!(listed, declared);
        for (index, entry) in ALL.iter().enumerate() {
            assert!(
                !ALL[..index].iter().any(|other| other.name == entry.name),
                "{} is listed twice",
                entry.name
            );
        }
    }

    #[test]
    fn only_the_postgres_compatible_names_go_unprefixed() {
        for entry in ALL {
            let ours = entry.name.starts_with("CRABGRESQL_");
            let borrowed = entry.name == DATA_DIR || entry.name == LOG_FILTER;
            assert!(
                ours ^ borrowed,
                "{} is neither ours nor a name we borrow on purpose",
                entry.name
            );
        }
    }

    #[test]
    fn documented_defaults_match_the_constants() {
        let default_of = |name: &str| {
            ALL.iter()
                .find(|entry| entry.name == name)
                .map(|entry| entry.default)
        };
        assert_eq!(default_of(PORT), Some(DEFAULT_PORT.to_string().as_str()));
        assert_eq!(default_of(DATA_DIR), Some(DEFAULT_DATA_DIR));
        assert_eq!(default_of(LOG_FILTER), Some(DEFAULT_LOG_FILTER));
        // The size knobs are documented the way a user would type them, so the
        // check is that the documented spelling *parses back* to the constant.
        assert_eq!(
            bytes_raw(default_of(BUFFER_TABLE_SOFT_BYTES), 0),
            DEFAULT_BUFFER_TABLE_SOFT_BYTES
        );
        assert_eq!(
            bytes_raw(default_of(BUFFER_GLOBAL_HARD_BYTES), 0),
            DEFAULT_BUFFER_GLOBAL_HARD_BYTES
        );
        assert_eq!(
            default_of(BUFFER_MAX_AGE_MS),
            Some(DEFAULT_BUFFER_MAX_AGE_MS.to_string().as_str())
        );
        assert_eq!(
            default_of(BUFFER_TICK_MS),
            Some(DEFAULT_BUFFER_TICK_MS.to_string().as_str())
        );
    }

    #[test]
    fn parsing_falls_back_on_anything_unusable() {
        assert_eq!(bytes_raw(None, 7), 7);
        assert_eq!(bytes_raw(Some("nope"), 7), 7);
        assert_eq!(bytes_raw(Some("-1"), 7), 7);
        assert_eq!(bytes_raw(Some("12"), 7), 12);
        // A unit with no count, and a count with a unit we do not know.
        assert_eq!(bytes_raw(Some("MB"), 7), 7);
        assert_eq!(bytes_raw(Some("12 quatloos"), 7), 7);
        assert_eq!(bytes_raw(Some("1.5MB"), 7), 7);
        // Overflowing `usize` is a size we cannot honor, not a saturating one.
        assert_eq!(bytes_raw(Some("99999999999999999999999"), 7), 7);
        assert_eq!(bytes_raw(Some("16777216TB"), 7), 7);

        let fallback = Duration::from_secs(60);
        assert_eq!(duration_ms_raw(None, fallback), fallback);
        assert_eq!(duration_ms_raw(Some("1.5"), fallback), fallback);
        assert_eq!(
            duration_ms_raw(Some("250"), fallback),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn sizes_may_be_spelled_with_a_unit() {
        const KB: usize = 1024;
        assert_eq!(bytes_raw(Some("0"), 7), 0);
        assert_eq!(bytes_raw(Some("512B"), 7), 512);
        assert_eq!(bytes_raw(Some("8kB"), 7), 8 * KB);
        assert_eq!(bytes_raw(Some("32MB"), 7), 32 * KB * KB);
        assert_eq!(bytes_raw(Some("2GB"), 7), 2 * KB * KB * KB);
        assert_eq!(bytes_raw(Some("1TB"), 7), KB * KB * KB * KB);
        // Case, a bare unit letter and space around either half are all fine.
        assert_eq!(bytes_raw(Some("32mb"), 7), 32 * KB * KB);
        assert_eq!(bytes_raw(Some("32Mb"), 7), 32 * KB * KB);
        assert_eq!(bytes_raw(Some("32m"), 7), 32 * KB * KB);
        assert_eq!(bytes_raw(Some("  32 MB  "), 7), 32 * KB * KB);
    }

    #[test]
    fn an_empty_value_reads_as_unset() {
        // Not `var` itself: setting a variable is `unsafe` under the 2024
        // edition and would race the other tests in this binary. The filter it
        // applies is what matters, and an empty string reaching the parsers
        // falls back all the same.
        assert_eq!(bytes_raw(Some(""), 7), 7);
        let fallback = Duration::from_secs(60);
        assert_eq!(duration_ms_raw(Some(""), fallback), fallback);
    }
}
