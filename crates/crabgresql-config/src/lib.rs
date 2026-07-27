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
/// How long a write buffer may hold rows before being flushed anyway.
pub const BUFFER_MAX_AGE_MS: &str = "CRABGRESQL_BUFFER_MAX_AGE_MS";
/// How often the background flush worker looks for eligible buffers.
pub const BUFFER_TICK_MS: &str = "CRABGRESQL_BUFFER_TICK_MS";

const KB: usize = 1024;
const MB: usize = KB * KB;
const GB: usize = KB * KB * KB;

/// Buffered rows live in RAM, so a limit past a couple of gigabytes is a
/// memory problem rather than a tuning choice. Two, not more, so the constant
/// still fits a 32-bit `usize`.
const MAX_BUFFER_BYTES: usize = 2 * GB;
/// One heap page. Below that a buffer would flush on essentially every row,
/// which is the unbuffered behavior plus the bookkeeping.
const MIN_BUFFER_BYTES: usize = 8 * KB;

/// Per-relation buffered bytes that make one write buffer flush-eligible.
pub const BUFFER_TABLE_SOFT_BYTES: SizeVar = SizeVar {
    name: "CRABGRESQL_BUFFER_TABLE_SOFT_BYTES",
    default: 32 * MB,
    min: MIN_BUFFER_BYTES,
    max: MAX_BUFFER_BYTES,
    help: "per-relation buffered bytes that make one write buffer flush-eligible",
};
/// Buffered bytes across all relations that make *every* buffer eligible.
pub const BUFFER_GLOBAL_HARD_BYTES: SizeVar = SizeVar {
    name: "CRABGRESQL_BUFFER_GLOBAL_HARD_BYTES",
    default: 256 * MB,
    min: MIN_BUFFER_BYTES,
    max: MAX_BUFFER_BYTES,
    help: "buffered bytes across all relations that make every buffer eligible",
};

/// One above PostgreSQL's 5432, so a local PostgreSQL can keep running.
pub const DEFAULT_PORT: u16 = 5433;
/// Used when neither `--data-dir` nor [`DATA_DIR`] is given.
pub const DEFAULT_DATA_DIR: &str = "./pgdata";
/// Used when [`LOG_FILTER`] is unset or unparseable.
pub const DEFAULT_LOG_FILTER: &str = "info";
/// Default for [`BUFFER_MAX_AGE_MS`].
pub const DEFAULT_BUFFER_MAX_AGE_MS: u64 = 60_000;
/// Default for [`BUFFER_TICK_MS`].
pub const DEFAULT_BUFFER_TICK_MS: u64 = 1_000;

/// A size knob: its name, its default, and the range it is allowed to take.
///
/// Bounds live here rather than at the call site so a reader cannot be handed
/// another knob's limits, and so the range is documented by the same constant
/// that enforces it.
#[derive(Clone, Copy, Debug)]
pub struct SizeVar {
    pub name: &'static str,
    pub default: usize,
    /// Smallest value that still makes sense; anything under it is raised.
    pub min: usize,
    /// Largest value we are willing to honor; anything over it is lowered.
    pub max: usize,
    pub help: &'static str,
}

impl SizeVar {
    /// The size to use, reading the environment.
    ///
    /// `complain` is called with a human-readable message when the configured
    /// value could not be used as written — the point of validating is that a
    /// mistake is visible, not that it is quietly swallowed.
    pub fn get(&self, complain: impl FnOnce(String)) -> usize {
        let (value, problem) = self.read(var(self.name).as_deref());
        if let Some(problem) = problem {
            complain(problem);
        }
        value
    }

    /// The size `raw` asks for, corrected into range, and what was wrong with
    /// it. Split from [`SizeVar::get`] so it is testable without mutating the
    /// process environment.
    ///
    /// An out-of-range value is clamped rather than discarded: somebody who
    /// asks for 64 GB has said "as much as you can", and the nearest legal
    /// size honors that, where falling back to the default would move the
    /// value the other way. A value we cannot read at all says nothing, so it
    /// gets the default. Neither case fails startup — a typo in a tuning knob
    /// should not keep a server down.
    pub fn read(&self, raw: Option<&str>) -> (usize, Option<String>) {
        let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
            return (self.default, None);
        };
        let Some(size) = parse_bytes(raw) else {
            return (
                self.default,
                Some(format!(
                    "{}: cannot read {raw:?} as a size; using the default of {}",
                    self.name,
                    render_bytes(self.default)
                )),
            );
        };
        let clamped = size.clamp(self.min, self.max);
        let problem = (clamped != size).then(|| {
            format!(
                "{}: {raw:?} is outside the supported range {}..={}; using {}",
                self.name,
                render_bytes(self.min),
                render_bytes(self.max),
                render_bytes(clamped)
            )
        });
        (clamped, problem)
    }
}

/// A documented knob, for anything that wants to render the set rather than
/// read one value out of it (`--help`, the README table, tests).
#[derive(Clone, Copy, Debug)]
pub struct EnvVar {
    pub name: &'static str,
    /// The default rendered the way a user would type it.
    pub default: &'static str,
    pub help: &'static str,
    /// The accepted range, for the knobs that have one.
    pub range: Option<(usize, usize)>,
}

/// Every environment variable in the list above, in the order a reader meets
/// them: server first, storage tuning after.
pub const ALL: &[EnvVar] = &[
    EnvVar {
        name: PORT,
        default: "5433",
        help: "TCP port to listen on",
        range: None,
    },
    EnvVar {
        name: DATA_DIR,
        default: DEFAULT_DATA_DIR,
        help: "data directory the durable heap engine is opened in",
        range: None,
    },
    EnvVar {
        name: LOG_FILTER,
        default: DEFAULT_LOG_FILTER,
        help: "tracing filter directives",
        range: None,
    },
    EnvVar {
        name: BUFFER_TABLE_SOFT_BYTES.name,
        default: "32MB",
        help: BUFFER_TABLE_SOFT_BYTES.help,
        range: Some((BUFFER_TABLE_SOFT_BYTES.min, BUFFER_TABLE_SOFT_BYTES.max)),
    },
    EnvVar {
        name: BUFFER_GLOBAL_HARD_BYTES.name,
        default: "256MB",
        help: BUFFER_GLOBAL_HARD_BYTES.help,
        range: Some((BUFFER_GLOBAL_HARD_BYTES.min, BUFFER_GLOBAL_HARD_BYTES.max)),
    },
    EnvVar {
        name: BUFFER_MAX_AGE_MS,
        default: "60000",
        help: "how long a write buffer may hold rows before being flushed anyway",
        range: None,
    },
    EnvVar {
        name: BUFFER_TICK_MS,
        default: "1000",
        help: "how often the background flush worker looks for eligible buffers",
        range: None,
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

/// `name` read as a count of milliseconds, falling back to `fallback`.
pub fn duration_ms_or(name: &str, fallback: Duration) -> Duration {
    duration_ms_raw(var(name).as_deref(), fallback)
}

/// The size `raw` spells out, or `None` when it spells out no size at all.
///
/// The grammar is a run of decimal digits followed by an optional unit:
/// nothing or `B` for bytes, `kB`, `MB`, `GB`, `TB`. Units are binary
/// (1 kB = 1024 B) and matched case-insensitively, the trailing `B` is
/// optional, and surrounding space is ignored — `32MB`, `32 mb` and `32m` all
/// mean the same thing. A count that overflows `usize` names no size a machine
/// could have, so it fails like any other unreadable value.
fn parse_bytes(raw: &str) -> Option<usize> {
    let raw = raw.trim();
    let digits_end = raw
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(raw.len());
    let (digits, unit) = raw.split_at(digits_end);
    let count = digits.parse::<usize>().ok()?;
    count.checked_mul(byte_unit(unit)?)
}

/// How many bytes `unit` stands for, or `None` when it names no unit we know.
fn byte_unit(unit: &str) -> Option<usize> {
    let unit = unit.trim();
    [
        ("", 1),
        ("b", 1),
        ("k", KB),
        ("kb", KB),
        ("m", MB),
        ("mb", MB),
        ("g", GB),
        ("gb", GB),
        ("t", GB * KB),
        ("tb", GB * KB),
    ]
    .into_iter()
    .find(|(name, _)| unit.eq_ignore_ascii_case(name))
    .map(|(_, multiplier)| multiplier)
}

/// A size written the way [`parse_bytes`] would like to read it back: the
/// largest whole unit that divides it, or plain bytes when none does.
fn render_bytes(size: usize) -> String {
    for (unit, multiplier) in [("GB", GB), ("MB", MB), ("kB", KB)] {
        if size >= multiplier && size.is_multiple_of(multiplier) {
            return format!("{}{unit}", size / multiplier);
        }
    }
    format!("{size}B")
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
            BUFFER_TABLE_SOFT_BYTES.name,
            BUFFER_GLOBAL_HARD_BYTES.name,
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

    /// The size knobs, so a new one is validated by these tests for free.
    const SIZES: &[SizeVar] = &[BUFFER_TABLE_SOFT_BYTES, BUFFER_GLOBAL_HARD_BYTES];

    fn entry(name: &str) -> &'static EnvVar {
        match ALL.iter().find(|entry| entry.name == name) {
            Some(entry) => entry,
            None => panic!("{name} is missing from ALL"),
        }
    }

    #[test]
    fn documented_defaults_match_the_constants() {
        assert_eq!(entry(PORT).default, DEFAULT_PORT.to_string());
        assert_eq!(entry(DATA_DIR).default, DEFAULT_DATA_DIR);
        assert_eq!(entry(LOG_FILTER).default, DEFAULT_LOG_FILTER);
        assert_eq!(
            entry(BUFFER_MAX_AGE_MS).default,
            DEFAULT_BUFFER_MAX_AGE_MS.to_string()
        );
        assert_eq!(
            entry(BUFFER_TICK_MS).default,
            DEFAULT_BUFFER_TICK_MS.to_string()
        );
        // The size knobs are documented the way a user would type them, so the
        // check is that the documented spelling parses back to the constant —
        // and that the range shown is the one the reader enforces.
        for size in SIZES {
            let documented = entry(size.name);
            assert_eq!(parse_bytes(documented.default), Some(size.default));
            assert_eq!(documented.range, Some((size.min, size.max)));
            assert_eq!(documented.help, size.help);
        }
    }

    #[test]
    fn every_size_default_sits_inside_its_own_range() {
        for size in SIZES {
            assert!(
                size.min <= size.default && size.default <= size.max,
                "{}'s default is outside its range",
                size.name
            );
        }
    }

    #[test]
    fn parsing_rejects_anything_unusable() {
        assert_eq!(parse_bytes("12"), Some(12));
        assert_eq!(parse_bytes("nope"), None);
        assert_eq!(parse_bytes("-1"), None);
        // A unit with no count, and a count with a unit we do not know.
        assert_eq!(parse_bytes("MB"), None);
        assert_eq!(parse_bytes("12 quatloos"), None);
        assert_eq!(parse_bytes("1.5MB"), None);
        // Overflowing `usize` is a size we cannot honor, not a saturating one.
        assert_eq!(parse_bytes("99999999999999999999999"), None);
        assert_eq!(parse_bytes("16777216TB"), None);

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
        assert_eq!(parse_bytes("0"), Some(0));
        assert_eq!(parse_bytes("512B"), Some(512));
        assert_eq!(parse_bytes("8kB"), Some(8 * KB));
        assert_eq!(parse_bytes("32MB"), Some(32 * MB));
        assert_eq!(parse_bytes("2GB"), Some(2 * GB));
        assert_eq!(parse_bytes("1TB"), Some(GB * KB));
        // Case, a bare unit letter and space around either half are all fine.
        assert_eq!(parse_bytes("32mb"), Some(32 * MB));
        assert_eq!(parse_bytes("32Mb"), Some(32 * MB));
        assert_eq!(parse_bytes("32m"), Some(32 * MB));
        assert_eq!(parse_bytes("  32 MB  "), Some(32 * MB));
    }

    #[test]
    fn rendering_round_trips_through_the_parser() {
        for size in [0, 1, 512, 8 * KB, 1536, 32 * MB, 2 * GB, GB + MB] {
            assert_eq!(parse_bytes(&render_bytes(size)), Some(size));
        }
        assert_eq!(render_bytes(32 * MB), "32MB");
        assert_eq!(render_bytes(8 * KB), "8kB");
        assert_eq!(render_bytes(512), "512B");
    }

    #[test]
    fn a_value_out_of_range_is_clamped_and_complained_about() {
        let knob = SizeVar {
            name: "CRABGRESQL_TEST_SIZE",
            default: 32 * MB,
            min: 8 * KB,
            max: GB,
            help: "a knob that exists only in this test",
        };

        assert_eq!(knob.read(Some("64MB")), (64 * MB, None));
        assert_eq!(knob.read(Some("8kB")).0, 8 * KB, "the minimum is allowed");
        assert_eq!(knob.read(Some("1GB")).0, GB, "the maximum is allowed");

        let (value, problem) = knob.read(Some("4GB"));
        assert_eq!(value, GB);
        let problem = problem.unwrap_or_default();
        assert!(
            problem.contains("CRABGRESQL_TEST_SIZE")
                && problem.contains("\"4GB\"")
                && problem.contains("8kB..=1GB"),
            "unhelpful complaint: {problem}"
        );

        let (value, problem) = knob.read(Some("1"));
        assert_eq!(value, 8 * KB, "a size under the minimum is raised to it");
        assert!(problem.is_some());
    }

    #[test]
    fn an_unreadable_or_absent_value_takes_the_default() {
        let knob = BUFFER_TABLE_SOFT_BYTES;
        assert_eq!(knob.read(None), (knob.default, None));
        // An empty value counts as unset — a shell that exports `FOO=` has
        // said nothing, and there is nothing to complain about.
        assert_eq!(knob.read(Some("   ")), (knob.default, None));

        let (value, problem) = knob.read(Some("32 quatloos"));
        assert_eq!(value, knob.default);
        assert!(
            problem.unwrap_or_default().contains("32MB"),
            "the complaint should name the default it fell back to"
        );
    }
}
