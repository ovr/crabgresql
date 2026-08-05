//! Markdown rendering of a [`SuiteReport`], for CI to paste into a pull
//! request comment or a job summary. The runner emits this itself so the
//! workflow never has to parse the human-readable stdout.

use std::fmt::Write as _;
use std::time::Duration;

use crate::runner::SuiteReport;

/// How many of the slowest tests the summary lists.
const SLOWEST: usize = 5;

/// `4.21s`, the one duration spelling used by both the summary and the
/// per-test stdout lines.
pub fn format_duration(duration: Duration) -> String {
    format!("{:.2}s", duration.as_secs_f64())
}

/// A GitHub-flavored markdown section for one suite: the headline counts, the
/// failures if any, and the slowest tests.
pub fn markdown_summary(suite: &str, report: &SuiteReport) -> String {
    let (passed, total) = (report.passed(), report.total());
    // `total` can be zero only if a caller ran an empty test list; the binary
    // rejects that earlier, but the percentage must not divide by zero here.
    let percent = if total == 0 { 0 } else { passed * 100 / total };
    let mark = if report.all_passed() { "✅" } else { "❌" };
    let mut out = format!(
        "### {mark} {suite} — {passed}/{total} passed ({percent}%) in {}\n",
        format_duration(report.duration)
    );

    let failed: Vec<&str> = report.failed().map(|o| o.name.as_str()).collect();
    if !failed.is_empty() {
        let names: Vec<String> = failed.iter().map(|name| format!("`{name}`")).collect();
        let _ = write!(out, "\nFailed: {}\n", names.join(", "));
    }

    let slowest = report.slowest(SLOWEST);
    if !slowest.is_empty() {
        let _ = write!(
            out,
            "\nSlowest {}:\n\n| test | time |\n| --- | --- |\n",
            slowest.len()
        );
        for outcome in slowest {
            let _ = writeln!(
                out,
                "| `{}` | {} |",
                outcome.name,
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
            duration: Duration::from_millis(millis),
        }
    }

    #[test]
    fn summarizes_a_green_run() {
        let report = SuiteReport {
            outcomes: vec![outcome("fast", true, 100), outcome("slow", true, 2500)],
            duration: Duration::from_millis(2600),
        };
        let md = markdown_summary("smoke", &report);
        assert_eq!(
            md,
            "### ✅ smoke — 2/2 passed (100%) in 2.60s\n\
             \n\
             Slowest 2:\n\
             \n\
             | test | time |\n\
             | --- | --- |\n\
             | `slow` | 2.50s |\n\
             | `fast` | 0.10s |\n"
        );
    }

    #[test]
    fn lists_failures() {
        let report = SuiteReport {
            outcomes: vec![outcome("ok", true, 10), outcome("bad", false, 20)],
            duration: Duration::from_millis(30),
        };
        let md = markdown_summary("upstream", &report);
        assert!(md.starts_with("### ❌ upstream — 1/2 passed (50%) in 0.03s\n"));
        assert!(md.contains("\nFailed: `bad`\n"));
    }
}
