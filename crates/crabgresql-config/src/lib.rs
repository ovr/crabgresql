//! Every environment variable crabgresql reads, and every default that goes
//! with it.
//!
//! The names used to live as string literals at the point of use, which made
//! the set of knobs impossible to enumerate, document, or test: adding one was
//! a local edit nobody else could see. Here a name, its default and the range
//! it may take sit in one constant, and the README's Configuration table is
//! their prose copy.
//!
//! Only the knobs this crate reads itself go through [`RangedVar`]. The
//! server's listen address, port, data directory and `COPY … FROM` path list
//! are clap arguments that happen to take an environment fallback, and
//! `RUST_LOG` belongs to `tracing_subscriber`; those are here as names and
//! defaults only, and they keep their own handling of a value they cannot
//! use — clap exits, and `RUST_LOG` falls back to the default filter.
//!
//! Cargo's own build-time variables (`OUT_DIR`, `CARGO_MANIFEST_DIR`,
//! `CARGO_TARGET_TMPDIR`) are deliberately absent — they are an interface with
//! the build system, not something a user configures.
//!
//! A value we cannot use is corrected rather than fatal: out of range is
//! clamped, unreadable falls back to the default, and either way the caller is
//! handed a complaint to log. A typo in a tuning knob should not keep a server
//! from coming up, but it should not pass unnoticed either.

use std::time::Duration;

/// TCP port the server listens on.
pub const PORT: &str = "CRABGRESQL_PORT";
/// Address the server accepts connections on. Loopback by default: the only
/// authentication method is trust and there is no TLS.
pub const LISTEN_ADDRESS: &str = "CRABGRESQL_LISTEN_ADDRESS";
/// Data directory the durable heap engine is opened in. Spelled `PGDATA` to
/// match PostgreSQL, since the same directory serves the same purpose.
pub const DATA_DIR: &str = "PGDATA";
/// Directories a server-side `COPY … FROM '<file>'` may read, beyond the data
/// directory. Colon-separated, like `PATH`. Empty means the data directory and
/// nothing else — the read runs with the server's privileges, so it is confined
/// by default.
pub const COPY_ALLOW_PATHS: &str = "CRABGRESQL_COPY_ALLOW_PATHS";
/// `tracing` filter directives. Read by `tracing_subscriber`'s `EnvFilter`
/// rather than by this crate, so the name is here only to be documented.
pub const LOG_FILTER: &str = "RUST_LOG";

/// One above PostgreSQL's 5432, so a local PostgreSQL can keep running.
pub const DEFAULT_PORT: u16 = 5433;
/// Used when neither `--listen-address` nor [`LISTEN_ADDRESS`] is given.
pub const DEFAULT_LISTEN_ADDRESS: &str = "127.0.0.1";
/// Used when neither `--data-dir` nor [`DATA_DIR`] is given.
pub const DEFAULT_DATA_DIR: &str = "./pgdata";
/// Used when [`LOG_FILTER`] is unset or unparseable.
pub const DEFAULT_LOG_FILTER: &str = "info";

const KB: usize = 1024;
const MB: usize = KB * KB;
const GB: usize = KB * KB * KB;

/// One relation's buffered rows sit in RAM until they become a chunk, so past
/// a couple of gigabytes for a single table this is a memory problem rather
/// than a tuning choice. And the steady state is not the peak: flushing copies
/// the visible rows, encodes them into Arrow, and holds the originals until a
/// snapshot releases them, so a buffer this size costs several times this much
/// while it drains.
const MAX_TABLE_BUFFER_BYTES: usize = 2 * GB;
/// A backstop against a typo, not a supported setting: 16 GB of resident rows
/// plus what flushing them transiently costs is more than most machines have.
/// `saturating_mul` because 16 GB is more than a 32-bit `usize` can hold, where
/// the address space is the ceiling anyway.
const MAX_GLOBAL_BUFFER_BYTES: usize = GB.saturating_mul(16);
/// Below a megabyte a buffer flushes every few rows, which is the unbuffered
/// behavior plus the bookkeeping. These bytes are a RAM footprint, not an
/// encoded size, and one row of a wide analytics table already costs several
/// kilobytes of it — a `Vec<Value>` pays `size_of::<Value>()` per column
/// whatever that column holds.
const MIN_BUFFER_BYTES: usize = MB;
/// A page pool has to hold every page pinned at once, or the clock sweep finds
/// no victim and the pin fails. How many that is depends on the workload — a
/// btree split holds four at once, and an index build holds a heap page across
/// the whole insert — and nothing caps the number of sessions, so no floor here
/// can make exhaustion impossible; the pool reports it instead. What this floor
/// does is keep the smallest configurable pool no smaller than the 8 MB this
/// engine shipped with before the size was configurable at all.
const MIN_SHARED_BUFFERS: usize = 8 * MB;
/// Unlike the write buffers above, these bytes are reserved for the process
/// lifetime rather than transiently occupied, so the ceiling is about the
/// machine rather than about a flush. Still a backstop against a typo.
const MAX_SHARED_BUFFERS: usize = GB.saturating_mul(16);
/// Anything shorter is a background thread that spends its life waking up.
const MIN_INTERVAL: Duration = Duration::from_millis(10);
/// One WAL segment (`crabgresql_wal::SEGMENT_SIZE`, restated because this crate
/// deliberately has no dependencies; a const-assert in `crabgresql-pg-engine`
/// keeps the two together). Below a segment, a checkpoint could not retire even
/// one file — pure write amplification.
const MIN_MAX_WAL_SIZE: usize = 32 * MB;
/// A backstop against a typo, not a supported setting: 16 GB of log to replay
/// is a recovery measured in minutes, which is the opposite of what bounding
/// replay is for.
const MAX_MAX_WAL_SIZE: usize = GB.saturating_mul(16);
/// A backstop against a typo in the other direction: the retained log is dead
/// weight by construction — recovery never reads it — so a value large enough to
/// fill a disk is far likelier to be a mistake than a wish.
const MAX_WAL_KEEP_SIZE: usize = GB.saturating_mul(16);

/// Per-relation buffered bytes that make one write buffer flush-eligible.
///
/// Resident bytes, not encoded ones: what the rows occupy in RAM, which for a
/// wide table is several times what they would serialize to.
pub const BUFFER_TABLE_SOFT_BYTES: SizeVar = SizeVar {
    name: "CRABGRESQL_BUFFER_TABLE_SOFT_BYTES",
    default: 32 * MB,
    min: MIN_BUFFER_BYTES,
    max: MAX_TABLE_BUFFER_BYTES,
    help: "per-relation buffered bytes that make one write buffer flush-eligible",
};
/// Buffered bytes across all relations that make *every* buffer eligible, and
/// past which a writer waits for the flush to catch up.
pub const BUFFER_GLOBAL_HARD_BYTES: SizeVar = SizeVar {
    name: "CRABGRESQL_BUFFER_GLOBAL_HARD_BYTES",
    default: 256 * MB,
    min: MIN_BUFFER_BYTES,
    max: MAX_GLOBAL_BUFFER_BYTES,
    help: "buffered bytes across all relations that make every buffer eligible",
};
/// How long a write buffer may hold rows before being flushed anyway.
pub const BUFFER_MAX_AGE: TimeVar = TimeVar {
    name: "CRABGRESQL_BUFFER_MAX_AGE",
    default: Duration::from_secs(60),
    min: MIN_INTERVAL,
    max: Duration::from_secs(24 * 60 * 60),
    help: "how long a write buffer may hold rows before being flushed anyway",
};
/// How often the background flush worker looks for eligible buffers.
pub const BUFFER_TICK: TimeVar = TimeVar {
    name: "CRABGRESQL_BUFFER_TICK",
    default: Duration::from_secs(1),
    min: MIN_INTERVAL,
    max: Duration::from_secs(60 * 60),
    help: "how often the background flush worker looks for eligible buffers",
};
/// RAM the buffer pool holds relation pages in, rounded down to whole 8 KiB
/// frames.
///
/// PostgreSQL's default, and for the same reason: a pool far below the working
/// set turns every page read into a file read, and each of those is serialized
/// against every other one. The old value here was 8 MiB, against which
/// `pgbench -s 10` puts a 178 MiB working set — so essentially nothing hit.
pub const SHARED_BUFFERS: SizeVar = SizeVar {
    name: "CRABGRESQL_SHARED_BUFFERS",
    default: 128 * MB,
    min: MIN_SHARED_BUFFERS,
    max: MAX_SHARED_BUFFERS,
    help: "RAM the buffer pool holds relation pages in, rounded down to whole 8 KiB frames",
};
/// WAL written past the published redo point that makes a checkpoint due, and
/// so roughly how much log a restart has to replay.
///
/// A level, not a cap: nothing refuses a write for being past it, and one
/// transaction logging more than this overshoots until it can be checkpointed.
/// A gigabyte, matching PostgreSQL's `max_wal_size` — the point where the
/// recovery time this buys stops paying for the checkpoint I/O it costs.
pub const MAX_WAL_SIZE: SizeVar = SizeVar {
    name: "CRABGRESQL_MAX_WAL_SIZE",
    default: GB,
    min: MIN_MAX_WAL_SIZE,
    max: MAX_MAX_WAL_SIZE,
    help: "WAL bytes written past the published redo point that trigger a checkpoint",
};
/// WAL kept below the published redo point, on top of what recovery needs.
///
/// A checkpoint retires every segment lying wholly below the redo point it
/// published; this holds that many bytes of them back. Zero, like PostgreSQL's
/// `wal_keep_size`, because nothing in this server reads the retained log —
/// there is no replication and no archiver yet — so the only thing it buys is a
/// forensic window after a crash, which costs disk to keep.
pub const WAL_KEEP_SIZE: SizeVar = SizeVar {
    name: "CRABGRESQL_WAL_KEEP_SIZE",
    default: 0,
    min: 0,
    max: MAX_WAL_KEEP_SIZE,
    help: "WAL bytes kept below the published redo point, beyond what recovery needs",
};

// Bounds that are transposed, or a default outside them, would be a startup
// panic waiting for the first operator to set that variable: `Ord::clamp`
// asserts `min <= max`. Checked here rather than in a test, because a test
// only covers the knobs somebody remembered to list in it.
const _: () = assert!(BUFFER_TABLE_SOFT_BYTES.is_sane());
const _: () = assert!(BUFFER_GLOBAL_HARD_BYTES.is_sane());
const _: () = assert!(BUFFER_MAX_AGE.is_sane());
const _: () = assert!(BUFFER_TICK.is_sane());
const _: () = assert!(SHARED_BUFFERS.is_sane());
const _: () = assert!(MAX_WAL_SIZE.is_sane());
const _: () = assert!(WAL_KEEP_SIZE.is_sane());

/// A knob whose value is a quantity: its name, its default, and the range it
/// is allowed to take.
///
/// Bounds live here rather than at the call site so a reader cannot be handed
/// another knob's limits, and so the range that is documented is the range
/// that is enforced.
#[derive(Clone, Copy, Debug)]
pub struct RangedVar<T> {
    pub name: &'static str,
    pub default: T,
    /// Smallest value that still makes sense; anything under it is raised.
    pub min: T,
    /// Largest value we are willing to honor; anything over it is lowered.
    pub max: T,
    pub help: &'static str,
}

/// A knob measured in bytes, written `32MB` or `33554432`.
pub type SizeVar = RangedVar<usize>;
/// A knob measured in time, written `60s` or `60000`.
pub type TimeVar = RangedVar<Duration>;

impl SizeVar {
    /// Whether the bounds are ordered and hold the default, checkable while
    /// compiling. `Ord` is not usable in a `const fn`, hence one of these per
    /// quantity rather than one on [`RangedVar`].
    const fn is_sane(&self) -> bool {
        self.min <= self.default && self.default <= self.max
    }
}

impl TimeVar {
    /// See [`SizeVar::is_sane`]; compares milliseconds because `Duration`'s
    /// comparisons are not `const` either.
    const fn is_sane(&self) -> bool {
        self.min.as_millis() <= self.default.as_millis()
            && self.default.as_millis() <= self.max.as_millis()
    }
}

impl<T: Quantity> RangedVar<T> {
    /// The value to use, reading the environment.
    ///
    /// `complain` is called with a human-readable message when the configured
    /// value could not be used as written — the point of validating is that a
    /// mistake is visible, not that it is quietly swallowed.
    pub fn get(&self, complain: impl FnOnce(String)) -> T {
        let (value, problem) = self.read(var(self.name).as_deref());
        if let Some(problem) = problem {
            complain(problem);
        }
        value
    }

    /// The value `raw` asks for, corrected into range, and what was wrong with
    /// it. Split from [`RangedVar::get`] so it is testable without mutating the
    /// process environment.
    ///
    /// An out-of-range value is clamped rather than discarded: somebody who
    /// asks for 64 GB has said "as much as you can", and the nearest legal
    /// value honors that, where falling back to the default would move it the
    /// other way. A value we cannot read at all says nothing, so it gets the
    /// default.
    fn read(&self, raw: Option<&str>) -> (T, Option<String>) {
        let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
            return (self.default, None);
        };
        let Some(asked) = T::parse(raw) else {
            return (
                self.default,
                Some(format!(
                    "{}: cannot read {raw:?} as a {}; using the default of {}",
                    self.name,
                    T::NOUN,
                    self.default.render()
                )),
            );
        };
        let clamped = asked.clamp(self.min, self.max);
        let problem = (clamped != asked).then(|| {
            format!(
                "{}: {raw:?} is outside the supported range {}..={}; using {}",
                self.name,
                self.min.render(),
                self.max.render(),
                clamped.render()
            )
        });
        (clamped, problem)
    }
}

/// A quantity a knob can be set to: how it is spelled and how it reads back.
///
/// Implemented for `usize` (bytes) and `Duration`, which is what lets one
/// [`RangedVar`] serve both instead of two near-identical copies.
pub trait Quantity: Copy + Ord + Sized {
    /// What to call this kind of value when complaining about one.
    const NOUN: &'static str;
    /// The quantity `raw` spells out, or `None` when it spells out none.
    ///
    /// The grammar is a run of decimal digits followed by an optional unit,
    /// matched case-insensitively and ignoring space on either side. A count
    /// that overflows names no quantity a machine could have, so it fails like
    /// any other unreadable value.
    fn parse(raw: &str) -> Option<Self>;
    /// The value written the way [`Quantity::parse`] would like to read it
    /// back: the largest whole unit that divides it.
    fn render(self) -> String;
}

impl Quantity for usize {
    const NOUN: &'static str = "size";

    /// Units are binary and the trailing `B` is optional, so `32MB`, `32 mb`
    /// and `32m` all mean the same thing; a bare count is bytes.
    fn parse(raw: &str) -> Option<usize> {
        let (count, unit) = split_count(raw)?;
        let multiplier = match_unit(
            unit,
            &[
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
            ],
        )?;
        usize::try_from(count).ok()?.checked_mul(multiplier)
    }

    fn render(self) -> String {
        for (unit, multiplier) in [("GB", GB), ("MB", MB), ("kB", KB)] {
            if self >= multiplier && self.is_multiple_of(multiplier) {
                return format!("{}{unit}", self / multiplier);
            }
        }
        format!("{self}B")
    }
}

impl Quantity for Duration {
    const NOUN: &'static str = "duration";

    /// `ms`, `s` (also `sec`), `m` (also `min`) and `h`; a bare count is
    /// milliseconds, which is what these knobs took before they had units at
    /// all.
    fn parse(raw: &str) -> Option<Duration> {
        const SECOND: u64 = 1_000;
        const MINUTE: u64 = 60 * SECOND;
        let (count, unit) = split_count(raw)?;
        let multiplier = match_unit(
            unit,
            &[
                ("", 1),
                ("ms", 1),
                ("s", SECOND),
                ("sec", SECOND),
                ("m", MINUTE),
                ("min", MINUTE),
                ("h", 60 * MINUTE),
            ],
        )?;
        count.checked_mul(multiplier).map(Duration::from_millis)
    }

    fn render(self) -> String {
        let millis = self.as_millis();
        for (unit, multiplier) in [("h", 3_600_000), ("m", 60_000), ("s", 1_000)] {
            if millis >= multiplier && millis.is_multiple_of(multiplier) {
                return format!("{}{unit}", millis / multiplier);
            }
        }
        format!("{millis}ms")
    }
}

/// The leading digits of `raw` and whatever unit follows them.
fn split_count(raw: &str) -> Option<(u64, &str)> {
    let raw = raw.trim();
    let digits_end = raw
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(raw.len());
    let (digits, unit) = raw.split_at(digits_end);
    Some((digits.parse().ok()?, unit))
}

/// How much `unit` multiplies a count by, or `None` when it names no unit in
/// `table`.
fn match_unit<T: Copy>(unit: &str, table: &[(&str, T)]) -> Option<T> {
    let unit = unit.trim();
    table
        .iter()
        .find(|(name, _)| unit.eq_ignore_ascii_case(name))
        .map(|(_, multiplier)| *multiplier)
}

/// The raw value of `name`, or `None` when it is unset, empty, or not UTF-8.
///
/// An empty value counts as unset: a shell that exports `FOO=` has said
/// nothing, and treating it as a parse failure would mean the same thing by a
/// longer route.
fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|raw| !raw.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spellings the README documents, checked against what the knobs
    /// actually enforce. The table there is the only place these ranges are
    /// written for a human, so it is the copy worth pinning.
    #[test]
    fn the_readme_table_says_what_the_knobs_enforce() {
        assert_eq!(BUFFER_TABLE_SOFT_BYTES.default.render(), "32MB");
        assert_eq!(BUFFER_TABLE_SOFT_BYTES.min.render(), "1MB");
        assert_eq!(BUFFER_TABLE_SOFT_BYTES.max.render(), "2GB");

        assert_eq!(BUFFER_GLOBAL_HARD_BYTES.default.render(), "256MB");
        assert_eq!(BUFFER_GLOBAL_HARD_BYTES.min.render(), "1MB");
        assert_eq!(BUFFER_GLOBAL_HARD_BYTES.max.render(), "16GB");

        assert_eq!(BUFFER_MAX_AGE.default.render(), "1m");
        assert_eq!(BUFFER_MAX_AGE.min.render(), "10ms");
        assert_eq!(BUFFER_MAX_AGE.max.render(), "24h");

        assert_eq!(BUFFER_TICK.default.render(), "1s");
        assert_eq!(BUFFER_TICK.min.render(), "10ms");
        assert_eq!(BUFFER_TICK.max.render(), "1h");

        assert_eq!(SHARED_BUFFERS.default.render(), "128MB");
        assert_eq!(SHARED_BUFFERS.min.render(), "8MB");
        assert_eq!(SHARED_BUFFERS.max.render(), "16GB");

        assert_eq!(MAX_WAL_SIZE.default.render(), "1GB");
        assert_eq!(MAX_WAL_SIZE.min.render(), "32MB");
        assert_eq!(MAX_WAL_SIZE.max.render(), "16GB");

        assert_eq!(WAL_KEEP_SIZE.default.render(), "0B");
        assert_eq!(WAL_KEEP_SIZE.min.render(), "0B");
        assert_eq!(WAL_KEEP_SIZE.max.render(), "16GB");
    }

    /// The same table, read rather than transcribed.
    ///
    /// The test above pins the constants to spellings a human typed here, which
    /// catches a knob changing under the documentation but not the documentation
    /// drifting from the knob: raising a minimum and forgetting the README
    /// leaves it green. This one opens the file, so the row has to agree about
    /// the default, the range, and the description a knob gives of itself.
    #[test]
    fn the_readme_table_agrees_with_the_knobs_it_describes() {
        let readme = include_str!("../../../README.md");
        let mut rows = vec![
            (
                SHARED_BUFFERS.name,
                SHARED_BUFFERS.help,
                Row::from(SHARED_BUFFERS),
            ),
            (
                BUFFER_TABLE_SOFT_BYTES.name,
                BUFFER_TABLE_SOFT_BYTES.help,
                Row::from(BUFFER_TABLE_SOFT_BYTES),
            ),
            (
                BUFFER_GLOBAL_HARD_BYTES.name,
                BUFFER_GLOBAL_HARD_BYTES.help,
                Row::from(BUFFER_GLOBAL_HARD_BYTES),
            ),
        ];
        rows.push((
            BUFFER_MAX_AGE.name,
            BUFFER_MAX_AGE.help,
            Row::from(BUFFER_MAX_AGE),
        ));
        rows.push((BUFFER_TICK.name, BUFFER_TICK.help, Row::from(BUFFER_TICK)));
        rows.push((
            MAX_WAL_SIZE.name,
            MAX_WAL_SIZE.help,
            Row::from(MAX_WAL_SIZE),
        ));
        rows.push((
            WAL_KEEP_SIZE.name,
            WAL_KEEP_SIZE.help,
            Row::from(WAL_KEEP_SIZE),
        ));

        for (name, help, row) in rows {
            let line = readme
                .lines()
                .find(|line| line.starts_with(&format!("| `{name}` |")))
                .unwrap_or_else(|| panic!("{name} has no row in the README Configuration table"));
            for (what, expected) in [
                ("default", format!("`{}`", row.default)),
                ("range", format!("`{}`–`{}`", row.min, row.max)),
                ("description", help.to_string()),
            ] {
                assert!(
                    line.contains(&expected),
                    "the README row for {name} disagrees about its {what}\n  row:      {line}\n  expected: {expected}"
                );
            }
        }
    }

    /// One knob's three spellings, so both quantity kinds go through one check.
    struct Row {
        default: String,
        min: String,
        max: String,
    }

    impl<T: Quantity> From<RangedVar<T>> for Row {
        fn from(var: RangedVar<T>) -> Row {
            Row {
                default: var.default.render(),
                min: var.min.render(),
                max: var.max.render(),
            }
        }
    }

    #[test]
    fn parsing_rejects_anything_unusable() {
        assert_eq!(usize::parse("nope"), None);
        assert_eq!(usize::parse("-1"), None);
        assert_eq!(Duration::parse("half past four"), None);
        // A unit with no count, and a count with a unit we do not know.
        assert_eq!(usize::parse("MB"), None);
        assert_eq!(usize::parse("12 quatloos"), None);
        assert_eq!(usize::parse("1.5MB"), None);
        assert_eq!(Duration::parse("5 fortnights"), None);
        assert_eq!(Duration::parse("1.5s"), None);
        // Overflow is a quantity we cannot honor, not a saturating one.
        assert_eq!(usize::parse("99999999999999999999999"), None);
        assert_eq!(usize::parse("16777216TB"), None);
        assert_eq!(Duration::parse("99999999999999999999999"), None);
        assert_eq!(Duration::parse("9999999999999999h"), None);
    }

    #[test]
    fn sizes_may_be_spelled_with_a_unit() {
        assert_eq!(usize::parse("0"), Some(0));
        assert_eq!(usize::parse("12"), Some(12));
        assert_eq!(usize::parse("512B"), Some(512));
        assert_eq!(usize::parse("8kB"), Some(8 * KB));
        assert_eq!(usize::parse("32MB"), Some(32 * MB));
        assert_eq!(usize::parse("2GB"), Some(2 * GB));
        assert_eq!(usize::parse("1TB"), Some(GB * KB));
        // Case, a bare unit letter and space around either half are all fine.
        assert_eq!(usize::parse("32mb"), Some(32 * MB));
        assert_eq!(usize::parse("32Mb"), Some(32 * MB));
        assert_eq!(usize::parse("32m"), Some(32 * MB));
        assert_eq!(usize::parse("  32 MB  "), Some(32 * MB));
    }

    #[test]
    fn durations_may_be_spelled_with_a_unit() {
        // A bare count stays milliseconds, which is what these knobs took
        // before they had units.
        assert_eq!(Duration::parse("250"), Some(Duration::from_millis(250)));
        assert_eq!(Duration::parse("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(Duration::parse("30s"), Some(Duration::from_secs(30)));
        assert_eq!(Duration::parse("5m"), Some(Duration::from_secs(5 * 60)));
        assert_eq!(Duration::parse("5min"), Some(Duration::from_secs(5 * 60)));
        assert_eq!(Duration::parse("2h"), Some(Duration::from_secs(2 * 3600)));
        assert_eq!(Duration::parse("2H"), Some(Duration::from_secs(2 * 3600)));
        assert_eq!(Duration::parse("  2 h "), Some(Duration::from_secs(7200)));
        // `m` is minutes and `ms` is milliseconds — the one place where a
        // reader could be caught out, so it is pinned down here.
        assert_ne!(Duration::parse("5m"), Duration::parse("5ms"));
    }

    #[test]
    fn rendering_round_trips_through_the_parser() {
        for size in [0, 1, 512, 8 * KB, 1536, 32 * MB, 2 * GB, GB + MB] {
            assert_eq!(usize::parse(&size.render()), Some(size));
        }
        assert_eq!((32 * MB).render(), "32MB");
        assert_eq!((8 * KB).render(), "8kB");
        assert_eq!(512.render(), "512B");

        for millis in [0, 1, 10, 999, 1_000, 90_000, 3_600_000, 86_400_000] {
            let span = Duration::from_millis(millis);
            assert_eq!(Duration::parse(&span.render()), Some(span));
        }
        assert_eq!(Duration::from_secs(60).render(), "1m");
        assert_eq!(Duration::from_secs(90).render(), "90s");
        assert_eq!(Duration::from_millis(10).render(), "10ms");
        assert_eq!(Duration::from_secs(24 * 3600).render(), "24h");
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
    fn a_duration_out_of_range_is_clamped_too() {
        // A worker that wakes every zero milliseconds is a busy loop, which is
        // the case the minimum exists to rule out.
        let (value, problem) = BUFFER_TICK.read(Some("0"));
        assert_eq!(value, BUFFER_TICK.min);
        assert!(problem.unwrap_or_default().contains("10ms..=1h"));

        let (value, problem) = BUFFER_MAX_AGE.read(Some("48h"));
        assert_eq!(value, BUFFER_MAX_AGE.max);
        assert!(problem.unwrap_or_default().contains("using 24h"));
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

        let (value, problem) = BUFFER_MAX_AGE.read(Some("soon"));
        assert_eq!(value, BUFFER_MAX_AGE.default);
        assert!(problem.unwrap_or_default().contains("as a duration"));
    }
}
