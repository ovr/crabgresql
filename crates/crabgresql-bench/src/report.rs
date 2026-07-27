//! Results of a run, and the two ways to print them: an aligned table for a
//! human, and JSON shaped like the upstream benchmarks' result files
//! (one array of per-run seconds per query, `null` where the query failed).

use std::fmt::Write as _;
use std::time::Duration;

pub struct SuiteRun {
    pub suite: String,
    /// What was benchmarked, for the report header.
    pub target: String,
    /// Rows loaded, when this run loaded the dataset.
    pub loaded_rows: Option<u64>,
    pub load_time: Option<Duration>,
    pub queries: Vec<QueryRun>,
}

pub struct QueryRun {
    pub number: usize,
    pub sql: String,
    pub runs: Vec<Outcome>,
}

pub enum Outcome {
    Ok { elapsed: Duration, rows: usize },
    Failed(String),
    TimedOut,
}

impl Outcome {
    fn seconds(&self) -> Option<f64> {
        match self {
            Outcome::Ok { elapsed, .. } => Some(elapsed.as_secs_f64()),
            _ => None,
        }
    }
}

impl QueryRun {
    /// Fastest successful run, the number the published results compare.
    pub fn best(&self) -> Option<f64> {
        self.runs
            .iter()
            .filter_map(Outcome::seconds)
            .min_by(f64::total_cmp)
    }

    /// The first thing that went wrong, if anything did.
    pub fn failure(&self) -> Option<String> {
        self.runs.iter().find_map(|outcome| match outcome {
            Outcome::Ok { .. } => None,
            Outcome::Failed(message) => Some(message.clone()),
            Outcome::TimedOut => Some("timed out".to_string()),
        })
    }
}

impl SuiteRun {
    pub fn succeeded(&self) -> usize {
        self.queries.iter().filter(|q| q.best().is_some()).count()
    }

    pub fn table(&self) -> String {
        let runs = self.queries.first().map_or(0, |q| q.runs.len());
        let mut out = String::new();
        let _ = writeln!(out, "\n{} on {}", self.suite, self.target);
        if let (Some(rows), Some(time)) = (self.loaded_rows, self.load_time) {
            let _ = writeln!(
                out,
                "loaded {rows} rows in {:.1}s ({:.0} rows/s)",
                time.as_secs_f64(),
                rows as f64 / time.as_secs_f64().max(f64::EPSILON),
            );
        }

        let _ = write!(out, "\n{:>3}", "#");
        for i in 1..=runs {
            let _ = write!(out, " {:>9}", format!("run {i}"));
        }
        let _ = writeln!(out, " {:>9}  status", "best");

        for query in &self.queries {
            let _ = write!(out, "{:>3}", query.number);
            for outcome in &query.runs {
                match outcome.seconds() {
                    Some(secs) => {
                        let _ = write!(out, " {secs:>9.3}");
                    }
                    None => {
                        let _ = write!(out, " {:>9}", "-");
                    }
                }
            }
            match (query.best(), query.failure()) {
                (Some(best), None) => {
                    let _ = writeln!(out, " {best:>9.3}  ok");
                }
                (best, Some(failure)) => {
                    match best {
                        Some(best) => {
                            let _ = write!(out, " {best:>9.3}");
                        }
                        None => {
                            let _ = write!(out, " {:>9}", "-");
                        }
                    }
                    let _ = writeln!(out, "  {}", one_line(&failure));
                }
                (None, None) => {
                    let _ = writeln!(out, " {:>9}  not run", "-");
                }
            }
        }

        let total: f64 = self.queries.iter().filter_map(QueryRun::best).sum();
        let _ = writeln!(
            out,
            "\n{} of {} queries succeeded, {total:.3}s total (best runs)",
            self.succeeded(),
            self.queries.len(),
        );
        out
    }

    /// `{"system": …, "result": [[…], …]}` — the shape ClickBench's own
    /// `results/*.json` files use, so a run can be pasted straight in.
    pub fn json(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "{{");
        let _ = writeln!(out, "  \"system\": \"CrabgreSQL\",");
        let _ = writeln!(out, "  \"suite\": \"{}\",", self.suite);
        let _ = writeln!(out, "  \"target\": \"{}\",", escape(&self.target));
        if let Some(rows) = self.loaded_rows {
            let _ = writeln!(out, "  \"rows\": {rows},");
        }
        if let Some(time) = self.load_time {
            let _ = writeln!(out, "  \"load_time\": {:.3},", time.as_secs_f64());
        }
        let _ = writeln!(out, "  \"result\": [");
        for (i, query) in self.queries.iter().enumerate() {
            let runs: Vec<String> = query
                .runs
                .iter()
                .map(|outcome| match outcome.seconds() {
                    Some(secs) => format!("{secs:.3}"),
                    None => "null".to_string(),
                })
                .collect();
            let comma = if i + 1 == self.queries.len() { "" } else { "," };
            let _ = writeln!(out, "    [{}]{comma}", runs.join(", "));
        }
        let _ = writeln!(out, "  ]");
        let _ = writeln!(out, "}}");
        out
    }
}

/// Server errors are multi-line; a results table has room for the first line.
fn one_line(message: &str) -> String {
    let line = message.lines().next().unwrap_or(message).trim();
    if line.chars().count() > 100 {
        format!("{}…", line.chars().take(99).collect::<String>())
    } else {
        line.to_string()
    }
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> SuiteRun {
        SuiteRun {
            suite: "clickbench".to_string(),
            target: "in-process".to_string(),
            loaded_rows: Some(10),
            load_time: Some(Duration::from_secs(1)),
            queries: vec![
                QueryRun {
                    number: 1,
                    sql: "SELECT 1".to_string(),
                    runs: vec![
                        Outcome::Ok {
                            elapsed: Duration::from_millis(500),
                            rows: 1,
                        },
                        Outcome::Ok {
                            elapsed: Duration::from_millis(250),
                            rows: 1,
                        },
                    ],
                },
                QueryRun {
                    number: 2,
                    sql: "SELECT nope()".to_string(),
                    runs: vec![Outcome::Failed(
                        "ERROR: no function nope\nline 2".to_string(),
                    )],
                },
            ],
        }
    }

    #[test]
    fn best_is_the_fastest_successful_run() {
        assert_eq!(run().queries[0].best(), Some(0.25));
        assert_eq!(run().queries[1].best(), None);
    }

    #[test]
    fn table_reports_the_failure_on_one_line() {
        let table = run().table();
        assert!(table.contains("ERROR: no function nope"), "{table}");
        assert!(!table.contains("line 2"), "{table}");
        assert!(table.contains("1 of 2 queries succeeded"), "{table}");
    }

    #[test]
    fn json_uses_null_for_failed_runs() {
        let json = run().json();
        assert!(json.contains("[0.500, 0.250],"), "{json}");
        assert!(json.contains("[null]"), "{json}");
    }
}
