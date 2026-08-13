//! Markdown rendering of a [`SuiteReport`], for CI to paste into a pull
//! request comment or a job summary. The runner emits this itself so the
//! workflow never has to parse the human-readable stdout.

use std::fmt::Write as _;
use std::time::Duration;

use crate::runner::SuiteReport;

/// How many of the slowest tests [`Detail::Slowest`] lists.
const SLOWEST: usize = 5;

/// How much of the per-test table a summary carries. A pull request comment
/// wants the few slowest; a job summary page has room for everything.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    Slowest,
    All,
}

/// `4.21s`, the one duration spelling used by both the summary and the
/// per-test stdout lines.
pub fn format_duration(duration: Duration) -> String {
    format!("{:.2}s", duration.as_secs_f64())
}

/// A GitHub-flavored markdown section for one suite: the headline counts, the
/// failures if any, and a per-test table sized by `detail`.
pub fn markdown_summary(suite: &str, report: &SuiteReport, detail: Detail) -> String {
    let (passed, total) = (report.passed(), report.total());
    // `total` can be zero only if a caller ran an empty test list; the binary
    // rejects that earlier, but the percentage must not divide by zero here.
    let percent = if total == 0 { 0 } else { passed * 100 / total };
    let mark = if report.all_passed() { "✅" } else { "❌" };
    let mut out = format!(
        "### {mark} {suite} — {passed}/{total} passed ({percent}%) in {}\n",
        format_duration(report.duration)
    );

    // A crashed server explains every failure under it, so it goes above them.
    if let Some(crash) = &report.crash {
        let _ = write!(out, "\n> ⚠️ {crash}\n");
    }

    // Only tests that actually ran are named: after a crash the tail of the
    // schedule is unproven, not broken, and listing it would bury the one test
    // that matters.
    let failed: Vec<&str> = report
        .failed()
        .filter(|o| o.ran)
        .map(|o| o.name.as_str())
        .collect();
    if !failed.is_empty() {
        let names: Vec<String> = failed.iter().map(|name| format!("`{name}`")).collect();
        let _ = write!(out, "\nFailed: {}\n", names.join(", "));
    }
    let not_run = report.outcomes.iter().filter(|o| !o.ran).count();
    if not_run > 0 {
        let _ = write!(out, "\nNot run: {not_run} test(s) after the crash\n");
    }

    // Slowest first either way: on the full table that is the ordering worth
    // scanning, and it keeps the two shapes comparable.
    let rows = match detail {
        Detail::Slowest => report.slowest(SLOWEST),
        Detail::All => report.slowest(report.total()),
    };
    if !rows.is_empty() {
        let heading = match detail {
            Detail::Slowest => format!("Slowest {}", rows.len()),
            Detail::All => format!("All {} tests, slowest first", rows.len()),
        };
        let _ = write!(
            out,
            "\n{heading}:\n\n| test | result | time |\n| --- | --- | --- |\n"
        );
        for outcome in rows {
            let _ = writeln!(
                out,
                "| `{}` | {} | {} |",
                outcome.name,
                match (outcome.passed, outcome.ran) {
                    (true, _) => "ok",
                    (false, true) => "**FAILED**",
                    (false, false) => "**not run**",
                },
                format_duration(outcome.duration)
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::TestOutcome;

    fn outcome(name: &str, passed: bool, millis: u64) -> TestOutcome {
        TestOutcome {
            name: name.to_string(),
            passed,
            ran: true,
            duration: Duration::from_millis(millis),
        }
    }

    #[test]
    fn summarizes_a_green_run() {
        let report = SuiteReport {
            outcomes: vec![outcome("fast", true, 100), outcome("slow", true, 2500)],
            duration: Duration::from_millis(2600),
            crash: None,
        };
        let md = markdown_summary("smoke", &report, Detail::Slowest);
        assert_eq!(
            md,
            "### ✅ smoke — 2/2 passed (100%) in 2.60s\n\
             \n\
             Slowest 2:\n\
             \n\
             | test | result | time |\n\
             | --- | --- | --- |\n\
             | `slow` | ok | 2.50s |\n\
             | `fast` | ok | 0.10s |\n"
        );
    }

    #[test]
    fn lists_failures() {
        let report = SuiteReport {
            outcomes: vec![outcome("ok", true, 10), outcome("bad", false, 20)],
            duration: Duration::from_millis(30),
            crash: None,
        };
        let md = markdown_summary("upstream", &report, Detail::Slowest);
        assert!(md.starts_with("### ❌ upstream — 1/2 passed (50%) in 0.03s\n"));
        assert!(md.contains("\nFailed: `bad`\n"));
        assert!(md.contains("| `bad` | **FAILED** | 0.02s |\n"));
    }

    /// A crashed server: the reason leads, the test that was running is named
    /// as a failure, and the untouched tail is counted rather than listed.
    #[test]
    fn reports_a_crash_above_the_failures() {
        let not_run = TestOutcome {
            ran: false,
            ..outcome("later", false, 0)
        };
        let report = SuiteReport {
            outcomes: vec![outcome("boom", false, 20), not_run],
            duration: Duration::from_millis(30),
            crash: Some("server exited with signal 6 during test boom".to_string()),
        };
        let md = markdown_summary("upstream", &report, Detail::All);
        assert!(md.contains("\n> ⚠️ server exited with signal 6 during test boom\n"));
        assert!(md.contains("\nFailed: `boom`\n"));
        assert!(md.contains("\nNot run: 1 test(s) after the crash\n"));
        assert!(md.contains("| `later` | **not run** | 0.00s |\n"));
    }

    /// `Detail::All` keeps every test, where `Detail::Slowest` caps the table.
    #[test]
    fn full_detail_lists_every_test() {
        let outcomes = (0..SLOWEST + 3)
            .map(|i| outcome(&format!("t{i}"), true, i as u64))
            .collect();
        let report = SuiteReport {
            outcomes,
            duration: Duration::from_millis(100),
            crash: None,
        };

        let all = markdown_summary("smoke", &report, Detail::All);
        assert!(all.contains(&format!("All {} tests, slowest first:", SLOWEST + 3)));
        for i in 0..SLOWEST + 3 {
            assert!(
                all.contains(&format!("| `t{i}` |")),
                "t{i} missing from {all}"
            );
        }

        let capped = markdown_summary("smoke", &report, Detail::Slowest);
        assert_eq!(capped.matches("| `t").count(), SLOWEST);
    }
}
